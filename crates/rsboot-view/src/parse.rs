//! 解析：文本/标签切分 → 表达式 → 语句树。
//!
//! 分层结构照 minijinja（Rust 生态里公认最干净的模板引擎）：**不做 token 流**，
//! 边读边解析。模板里文本占九成以上，先切 token 再消费等于每个字符过两遍。
//!
//! 语法层换成 Smarty 的：`{$var}`、`{if}`、`{foreach}`、`|escape`。

use serde_json::Value;

/// 模板语法错误。`line` 从 1 起。
#[derive(Debug, Clone, PartialEq)]
pub struct SyntaxError {
    pub line: usize,
    pub msg: String,
}

impl SyntaxError {
    fn new(line: usize, msg: impl Into<String>) -> Self {
        Self {
            line,
            msg: msg.into(),
        }
    }
}

/// 变量访问的一段：`$user.name` → `[Key("user"), Key("name")]`。
#[derive(Debug, Clone, PartialEq)]
pub enum Access {
    Key(String),
    Index(Box<Expr>),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Eq,
    Ne,
    Gt,
    Ge,
    Lt,
    Le,
    And,
    Or,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Lit(Value),
    Var(Vec<Access>),
    Neg(Box<Expr>),
    Not(Box<Expr>),
    Bin(BinOp, Box<Expr>, Box<Expr>),
    /// `$x|upper` / `$x|truncate:20`。修饰符可以链式出现。
    Mod {
        base: Box<Expr>,
        name: String,
        args: Vec<Expr>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Stmt {
    Text(String),
    Print(Expr),
    If {
        arms: Vec<(Expr, Vec<Stmt>)>,
        else_: Vec<Stmt>,
    },
    ForEach {
        iter: Expr,
        key: Option<String>,
        val: String,
        body: Vec<Stmt>,
        else_: Vec<Stmt>,
    },
    Include(Expr),
    /// 注册进来的自定义函数：`{url path="/a" id=$id}`。
    Call {
        name: String,
        args: Vec<(String, Expr)>,
    },
    /// `{literal}` 里原样输出。
    Literal(String),
}


/// 相邻文本合成一个节点：注释、`{literal}` 前后很容易切出碎片，
/// 合并后渲染期少一次 `push_str` 分派，也少几百字节的 `Vec` 容量。
fn push_text(out: &mut Vec<Stmt>, t: String) {
    if t.is_empty() {
        return;
    }
    if let Some(Stmt::Text(prev)) = out.last_mut() {
        prev.push_str(&t);
        return;
    }
    out.push(Stmt::Text(t));
}

/// 解析整份模板。
pub fn parse(src: &str) -> Result<Vec<Stmt>, SyntaxError> {
    let mut p = P {
        s: src.as_bytes(),
        src,
        i: 0,
        line: 1,
    };
    let (nodes, term) = p.nodes(&[])?;
    if let Some((name, _)) = term {
        return Err(SyntaxError::new(p.line, format!("意外的标签 {{{name}}}")));
    }
    Ok(nodes)
}

struct P<'a> {
    s: &'a [u8],
    src: &'a str,
    i: usize,
    line: usize,
}

/// 结束一个块时遇到的标签：`{elseif $x}` 或 `{/if}`。
type Term = Option<(String, String)>;

impl<'a> P<'a> {
    fn at_end(&self) -> bool {
        self.i >= self.s.len()
    }

    fn bump(&mut self, n: usize) {
        for k in 0..n {
            if self.s[self.i + k] == b'\n' {
                self.line += 1;
            }
        }
        self.i += n;
    }

    /// 读到 `stop` 里的任一标签（或文件末尾）。
    ///
    /// 返回节点列表与**那个标签**。把"遇到谁"作为返回值而不是塞一个哨兵
    /// 语句进列表，是因为哨兵要先入列表再弹出，中间任何一层忘了弹都是 bug。
    fn nodes(&mut self, stop: &[&str]) -> Result<(Vec<Stmt>, Term), SyntaxError> {
        let mut out = Vec::<Stmt>::new();
        let mut text_start = self.i;
        loop {
            if self.at_end() {
                if text_start < self.i {
                    push_text(&mut out, self.src[text_start..self.i].to_string());
                }
                // 没遇到结束标签就到底了：把"缺了什么"交给上层报，
                // 上层才知道这是 `{if}` 还是 `{foreach}`，也才能给出开工那行的行号。
                return Ok((out, None));
            }
            let Some(tag_i) = self.s[self.i..].iter().position(|&b| b == b'{') else {
                self.bump(self.s.len() - self.i);
                push_text(&mut out, self.src[text_start..self.i].to_string());
                return Ok((out, None));
            };
            let tag_start = self.i + tag_i;
            // `{` 后面不是标签起步字符就当文本：CSS/JS 里 `{` 满天飞，
            // 要求每个 `{` 都是标签会让模板根本没法写。
            let next = self.s.get(tag_start + 1).copied();
            let is_tag = matches!(next, Some(b'$') | Some(b'*') | Some(b'/') | Some(b'!'))
                || next.map(|b| b.is_ascii_alphabetic()).unwrap_or(false);
            if !is_tag {
                self.bump(tag_i + 1);
                continue;
            }
            if tag_start > text_start {
                push_text(&mut out, self.src[text_start..tag_start].to_string());
            }
            // 跳过 `{` 本身：`tag()` 从标签内容第一个字符开始读。
            self.bump(tag_i + 1);
            let line = self.line;
            match self.tag()? {
                Tag::Comment => {}
                Tag::Print(body) => out.push(Stmt::Print(parse_expr(&body, line)?)),
                Tag::Close(name) => {
                    // 结束标签统一按 `{/if}` 这种带斜杠的形式交给上层比对，
                    // 免得 `if` 与 `/if` 两种拼法在 `stop` 里各写一遍。
                    let key = format!("/{name}");
                    if stop.contains(&key.as_str()) {
                        return Ok((out, Some((key, String::new()))));
                    }
                    return Err(SyntaxError::new(
                        line,
                        format!("意外的结束标签 {{/{name}}}"),
                    ));
                }
                Tag::Mid(name, rest) => {
                    if stop.contains(&name.as_str()) {
                        return Ok((out, Some((name, rest))));
                    }
                    return Err(SyntaxError::new(line, format!("意外的标签 {{{name}}}")));
                }
                Tag::Open(name, rest) => out.push(self.block(&name, &rest, line)?),
            }
            text_start = self.i;
        }
    }

    fn block(&mut self, name: &str, rest: &str, line: usize) -> Result<Stmt, SyntaxError> {
        match name {
            "if" => {
                let cond = parse_expr(rest, line)?;
                let (body, mut term) = self.nodes(&["/if", "elseif", "else"])?;
                let mut arms = vec![(cond, body)];
                let mut else_ = Vec::new();
                loop {
                    let Some((kind, args)) = term else {
                        return Err(SyntaxError::new(line, "if 缺少 `{/if}`"));
                    };
                    match kind.as_str() {
                        "elseif" => {
                            let c = parse_expr(&args, line)?;
                            let (b, t) = self.nodes(&["/if", "elseif", "else"])?;
                            arms.push((c, b));
                            term = t;
                        }
                        "else" => {
                            let (b, t) = self.nodes(&["/if"])?;
                            else_ = b;
                            term = t;
                        }
                        "/if" => break,
                        _ => return Err(SyntaxError::new(line, "if 块结构异常")),
                    }
                }
                Ok(Stmt::If { arms, else_ })
            }
            "foreach" => self.foreach(rest, line),
            "include" => {
                let e = if rest.trim_start().starts_with("file") {
                    let args = parse_args(rest, line)?;
                    args.into_iter()
                        .find(|(k, _)| k == "file")
                        .map(|(_, e)| e)
                        .ok_or_else(|| SyntaxError::new(line, "include 缺少 `file=`"))?
                } else {
                    parse_expr(rest, line)?
                };
                Ok(Stmt::Include(e))
            }
            "literal" => {
                let end = self
                    .src
                    .get(self.i..)
                    .and_then(|s| s.find("{/literal}"))
                    .ok_or_else(|| SyntaxError::new(line, "`{literal}` 没有结束"))?;
                let body = self.src[self.i..self.i + end].to_string();
                self.bump(end + "{/literal}".len());
                Ok(Stmt::Literal(body))
            }
            other => {
                let args = parse_args(rest, line)?;
                Ok(Stmt::Call {
                    name: other.to_string(),
                    args,
                })
            }
        }
    }

    fn foreach(&mut self, rest: &str, line: usize) -> Result<Stmt, SyntaxError> {
        // `$items as $it` / `$items as $k => $v`
        let r = rest.trim();
        let as_i = find_word(r, "as").ok_or_else(|| SyntaxError::new(line, "foreach 缺少 `as`"))?;
        let iter = parse_expr(&r[..as_i], line)?;
        let target = r[as_i + 2..].trim();
        let (key, val) = match target.find("=>") {
            Some(i) => (
                Some(var_name(&target[..i], line)?),
                var_name(&target[i + 2..], line)?,
            ),
            None => (None, var_name(target, line)?),
        };
        let (body, mut term) = self.nodes(&["/foreach", "foreachelse"])?;
        let mut else_ = Vec::new();
        loop {
            let Some((kind, _)) = term else {
                return Err(SyntaxError::new(line, "foreach 缺少 `{/foreach}`"));
            };
            match kind.as_str() {
                "foreachelse" => {
                    let (b, t) = self.nodes(&["/foreach"])?;
                    else_ = b;
                    term = t;
                }
                "/foreach" => break,
                _ => return Err(SyntaxError::new(line, "foreach 块结构异常")),
            }
        }
        Ok(Stmt::ForEach {
            iter,
            key,
            val,
            body,
            else_,
        })
    }

    /// 读到匹配的 `}`（跳过字符串里的 `}`）。
    fn tag(&mut self) -> Result<Tag, SyntaxError> {
        if self.s.get(self.i) == Some(&b'*') {
            let end = self
                .src
                .get(self.i..)
                .and_then(|s| s.find("*}"))
                .ok_or_else(|| SyntaxError::new(self.line, "注释没有 `*}` 收尾"))?;
            self.bump(end + 2);
            return Ok(Tag::Comment);
        }
        let start = self.i;
        let end = find_tag_end(self.s, start)
            .ok_or_else(|| SyntaxError::new(self.line, "标签缺少 `}`"))?;
        let body = self.src[start..end].trim().to_string();
        self.bump(end - start + 1);
        Ok(match body.as_bytes().first() {
            Some(b'/') => Tag::Close(body[1..].trim().to_string()),
            Some(b'$') | Some(b'!') | Some(b'(') => Tag::Print(body),
            _ => {
                let (name, rest) = match body.find(char::is_whitespace) {
                    Some(i) => (&body[..i], body[i..].trim()),
                    None => (body.as_str(), ""),
                };
                match name {
                    "else" | "elseif" | "foreachelse" => {
                        Tag::Mid(name.to_string(), rest.to_string())
                    }
                    _ => Tag::Open(name.to_string(), rest.to_string()),
                }
            }
        })
    }
}

enum Tag {
    Comment,
    Print(String),
    Close(String),
    Mid(String, String),
    Open(String, String),
}

fn find_tag_end(s: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    let mut quote = 0u8;
    while i < s.len() {
        let c = s[i];
        if quote != 0 {
            if c == quote {
                quote = 0;
            }
        } else if c == b'"' || c == b'\'' {
            quote = c;
        } else if c == b'}' {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// 找独立出现的单词（`as` / `file`），不做正则。
fn find_word(s: &str, w: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i + w.len() <= bytes.len() {
        if bytes[i..i + w.len()].eq_ignore_ascii_case(w.as_bytes()) {
            let before = i == 0 || !bytes[i - 1].is_ascii_alphanumeric();
            let after = bytes.get(i + w.len()).copied().unwrap_or(b' ');
            if before && !after.is_ascii_alphanumeric() {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

fn var_name(s: &str, line: usize) -> Result<String, SyntaxError> {
    s.trim()
        .strip_prefix('$')
        .map(|x| x.trim().to_string())
        .filter(|x| !x.is_empty() && x.chars().all(|c| c.is_alphanumeric() || c == '_'))
        .ok_or_else(|| SyntaxError::new(line, "这里需要一个变量名（`$name`）"))
}

fn parse_args(s: &str, line: usize) -> Result<Vec<(String, Expr)>, SyntaxError> {
    let mut out = Vec::new();
    let mut p = Ex {
        s: s.as_bytes(),
        src: s,
        i: 0,
        line,
    };
    loop {
        p.ws();
        if p.at_end() {
            break;
        }
        let name = p.ident().ok_or_else(|| SyntaxError::new(line, "参数名不合法"))?;
        if !p.eat(b'=') {
            return Err(SyntaxError::new(line, format!("参数 `{name}` 缺少 `=`")));
        }
        out.push((name, p.expr()?));
        let _ = p.eat(b',');
    }
    Ok(out)
}

/// 解析一个表达式片段。
pub fn parse_expr(s: &str, line: usize) -> Result<Expr, SyntaxError> {
    let mut p = Ex {
        s: s.as_bytes(),
        src: s,
        i: 0,
        line,
    };
    let e = p.expr()?;
    p.ws();
    if !p.at_end() {
        return Err(SyntaxError::new(
            line,
            format!("表达式多余内容：`{}`", &s[p.i..]),
        ));
    }
    Ok(e)
}

struct Ex<'a> {
    s: &'a [u8],
    src: &'a str,
    i: usize,
    line: usize,
}

impl<'a> Ex<'a> {
    fn at_end(&self) -> bool {
        self.i >= self.s.len()
    }

    fn ws(&mut self) {
        while !self.at_end() && self.s[self.i].is_ascii_whitespace() {
            self.i += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    fn eat(&mut self, c: u8) -> bool {
        self.ws();
        if self.peek() == Some(c) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn eat_sym(&mut self, sym: &str) -> bool {
        self.ws();
        if self.src[self.i..].starts_with(sym) {
            self.i += sym.len();
            true
        } else {
            false
        }
    }

    fn eat_word(&mut self, w: &str) -> bool {
        self.ws();
        let rest = &self.src[self.i..];
        if rest.len() >= w.len() && rest[..w.len()].eq_ignore_ascii_case(w) {
            let after = rest.as_bytes().get(w.len()).copied().unwrap_or(b' ');
            if !after.is_ascii_alphanumeric() && after != b'_' {
                self.i += w.len();
                return true;
            }
        }
        false
    }

    fn ident(&mut self) -> Option<String> {
        self.ws();
        let start = self.i;
        while let Some(c) = self.peek() {
            if c.is_ascii_alphanumeric() || c == b'_' {
                self.i += 1;
            } else {
                break;
            }
        }
        if self.i == start {
            None
        } else {
            Some(self.src[start..self.i].to_string())
        }
    }

    fn expr(&mut self) -> Result<Expr, SyntaxError> {
        self.or()
    }

    fn or(&mut self) -> Result<Expr, SyntaxError> {
        let mut l = self.and()?;
        loop {
            if self.eat_sym("||") || self.eat_word("or") {
                let r = self.and()?;
                l = Expr::Bin(BinOp::Or, Box::new(l), Box::new(r));
            } else {
                return Ok(l);
            }
        }
    }

    fn and(&mut self) -> Result<Expr, SyntaxError> {
        let mut l = self.equality()?;
        loop {
            if self.eat_sym("&&") || self.eat_word("and") {
                let r = self.equality()?;
                l = Expr::Bin(BinOp::And, Box::new(l), Box::new(r));
            } else {
                return Ok(l);
            }
        }
    }

    fn equality(&mut self) -> Result<Expr, SyntaxError> {
        let mut l = self.comparison()?;
        loop {
            let op = if self.eat_sym("==") {
                BinOp::Eq
            } else if self.eat_sym("!=") {
                BinOp::Ne
            } else {
                return Ok(l);
            };
            let r = self.comparison()?;
            l = Expr::Bin(op, Box::new(l), Box::new(r));
        }
    }

    fn comparison(&mut self) -> Result<Expr, SyntaxError> {
        let mut l = self.additive()?;
        loop {
            let op = if self.eat_sym(">=") {
                BinOp::Ge
            } else if self.eat_sym("<=") {
                BinOp::Le
            } else if self.eat_sym(">") {
                BinOp::Gt
            } else if self.eat_sym("<") {
                BinOp::Lt
            } else {
                return Ok(l);
            };
            let r = self.additive()?;
            l = Expr::Bin(op, Box::new(l), Box::new(r));
        }
    }

    fn additive(&mut self) -> Result<Expr, SyntaxError> {
        let mut l = self.multiplicative()?;
        loop {
            let op = if self.eat_sym("+") {
                BinOp::Add
            } else if self.eat_sym("-") {
                BinOp::Sub
            } else {
                return Ok(l);
            };
            let r = self.multiplicative()?;
            l = Expr::Bin(op, Box::new(l), Box::new(r));
        }
    }

    fn multiplicative(&mut self) -> Result<Expr, SyntaxError> {
        let mut l = self.unary()?;
        loop {
            let op = if self.eat_sym("*") {
                BinOp::Mul
            } else if self.eat_sym("/") {
                BinOp::Div
            } else if self.eat_sym("%") {
                BinOp::Mod
            } else {
                return Ok(l);
            };
            let r = self.unary()?;
            l = Expr::Bin(op, Box::new(l), Box::new(r));
        }
    }

    fn unary(&mut self) -> Result<Expr, SyntaxError> {
        if self.eat_sym("!") || self.eat_word("not") {
            return Ok(Expr::Not(Box::new(self.unary()?)));
        }
        if self.eat_sym("-") {
            return Ok(Expr::Neg(Box::new(self.unary()?)));
        }
        self.postfix()
    }

    fn postfix(&mut self) -> Result<Expr, SyntaxError> {
        let mut e = self.primary()?;
        loop {
            self.ws();
            match self.peek() {
                Some(b'.') => {
                    self.i += 1;
                    let key = self
                        .ident()
                        .ok_or_else(|| SyntaxError::new(self.line, "`.` 后面需要字段名"))?;
                    e = match e {
                        Expr::Var(mut acc) => {
                            acc.push(Access::Key(key));
                            Expr::Var(acc)
                        }
                        _other => {
                            return Err(SyntaxError::new(self.line, "只有变量能取字段"));
                        }
                    };
                }
                Some(b'[') => {
                    self.i += 1;
                    let idx = self.expr()?;
                    if !self.eat(b']') {
                        return Err(SyntaxError::new(self.line, "缺少 `]`"));
                    }
                    e = match e {
                        Expr::Var(mut acc) => {
                            acc.push(Access::Index(Box::new(idx)));
                            Expr::Var(acc)
                        }
                        _ => return Err(SyntaxError::new(self.line, "只有变量能下标访问")),
                    };
                }
                Some(b'|') if self.s.get(self.i + 1) != Some(&b'|') => {
                    self.i += 1;
                    let name = self
                        .ident()
                        .ok_or_else(|| SyntaxError::new(self.line, "`|` 后面需要修饰符名"))?;
                    let mut args = Vec::new();
                    while self.eat(b':') {
                        args.push(self.expr()?);
                    }
                    e = Expr::Mod {
                        base: Box::new(e),
                        name,
                        args,
                    };
                }
                _ => return Ok(e),
            }
        }
    }

    fn primary(&mut self) -> Result<Expr, SyntaxError> {
        self.ws();
        match self.peek() {
            Some(b'$') => {
                self.i += 1;
                let name = self
                    .ident()
                    .ok_or_else(|| SyntaxError::new(self.line, "`$` 后面需要变量名"))?;
                Ok(Expr::Var(vec![Access::Key(name)]))
            }
            Some(b'"') | Some(b'\'') => self.string(),
            Some(b'(') => {
                self.i += 1;
                let e = self.expr()?;
                if !self.eat(b')') {
                    return Err(SyntaxError::new(self.line, "缺少 `)`"));
                }
                Ok(e)
            }
            Some(c) if c.is_ascii_digit() || c == b'.' => self.number(),
            _ => {
                if self.eat_word("true") {
                    return Ok(Expr::Lit(Value::Bool(true)));
                }
                if self.eat_word("false") {
                    return Ok(Expr::Lit(Value::Bool(false)));
                }
                if self.eat_word("null") || self.eat_word("nil") {
                    return Ok(Expr::Lit(Value::Null));
                }
                Err(SyntaxError::new(
                    self.line,
                    format!("看不懂的表达式：`{}`", &self.src[self.i..]),
                ))
            }
        }
    }

    fn string(&mut self) -> Result<Expr, SyntaxError> {
        let q = self.s[self.i];
        self.i += 1;
        let mut out = String::new();
        while let Some(c) = self.peek() {
            if c == q {
                self.i += 1;
                return Ok(Expr::Lit(Value::String(out)));
            }
            if c == b'\\' {
                self.i += 1;
                let esc = self.peek().unwrap_or(b'\\');
                self.i += 1;
                out.push(match esc {
                    b'n' => '\n',
                    b't' => '\t',
                    b'r' => '\r',
                    other => other as char,
                });
            } else {
                let ch = self.src[self.i..].chars().next().unwrap();
                out.push(ch);
                self.i += ch.len_utf8();
            }
        }
        Err(SyntaxError::new(self.line, "字符串缺少结束引号"))
    }

    fn number(&mut self) -> Result<Expr, SyntaxError> {
        let start = self.i;
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() || c == b'.' || c == b'e' || c == b'E' || c == b'+' || c == b'-'
            {
                // `1-2` 的 `-` 是运算符，不是数字的一部分；`1e-3` 的才是
                if (c == b'+' || c == b'-')
                    && !matches!(self.s.get(self.i.wrapping_sub(1)), Some(b'e') | Some(b'E'))
                {
                    break;
                }
                self.i += 1;
            } else {
                break;
            }
        }
        let raw = &self.src[start..self.i];
        if let Ok(v) = raw.parse::<i64>() {
            return Ok(Expr::Lit(Value::Number(v.into())));
        }
        raw.parse::<f64>()
            .ok()
            .and_then(|f| serde_json::Number::from_f64(f).map(Value::Number))
            .map(Expr::Lit)
            .ok_or_else(|| SyntaxError::new(self.line, format!("不是合法数字：`{raw}`")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(src: &str) -> Vec<Stmt> {
        parse(src).unwrap()
    }

    #[test]
    fn plain_text_is_one_node() {
        assert_eq!(p("hello"), vec![Stmt::Text("hello".into())]);
    }

    #[test]
    fn braces_that_are_not_tags_stay_text() {
        assert_eq!(
            p("body { color: red }"),
            vec![Stmt::Text("body { color: red }".into())]
        );
    }

    #[test]
    fn comment_disappears() {
        assert_eq!(p("a{* x *}b"), vec![Stmt::Text("ab".into())]);
    }

    #[test]
    fn print_a_variable_path() {
        let n = p("{$user.name}");
        assert_eq!(
            n,
            vec![Stmt::Print(Expr::Var(vec![
                Access::Key("user".into()),
                Access::Key("name".into())
            ]))]
        );
    }

    #[test]
    fn modifier_chain() {
        let n = p("{$x|upper|truncate:10}");
        match &n[0] {
            Stmt::Print(Expr::Mod { base, name, args }) => {
                assert_eq!(name, "truncate");
                assert_eq!(args.len(), 1);
                match base.as_ref() {
                    Expr::Mod { name, .. } => assert_eq!(name, "upper"),
                    other => panic!("{other:?}"),
                }
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn if_elseif_else() {
        let n = p("{if $a > 1}a{elseif $a == 1}b{else}c{/if}");
        match &n[0] {
            Stmt::If { arms, else_ } => {
                assert_eq!(arms.len(), 2);
                assert_eq!(else_, &vec![Stmt::Text("c".into())]);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn foreach_with_key_and_else() {
        let n = p("{foreach $list as $k => $v}{$v}{foreachelse}empty{/foreach}");
        match &n[0] {
            Stmt::ForEach {
                key,
                val,
                body,
                else_,
                ..
            } => {
                assert_eq!(key.as_deref(), Some("k"));
                assert_eq!(val, "v");
                assert_eq!(body.len(), 1);
                assert_eq!(else_, &vec![Stmt::Text("empty".into())]);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn literal_keeps_braces() {
        let n = p("{literal}{$not_a_var}{/literal}");
        assert_eq!(n, vec![Stmt::Literal("{$not_a_var}".into())]);
    }

    #[test]
    fn include_accepts_both_forms() {
        for src in [r#"{include "x.tpl"}"#, r#"{include file="x.tpl"}"#] {
            match &p(src)[0] {
                Stmt::Include(Expr::Lit(Value::String(s))) => assert_eq!(s, "x.tpl"),
                other => panic!("{other:?}"),
            }
        }
    }

    #[test]
    fn missing_close_is_an_error() {
        assert!(parse("{if $a}b").is_err());
        assert!(parse("{foreach $a as $b}c{/if}").is_err());
    }

    #[test]
    fn strings_with_braces_do_not_break_the_tag() {
        let n = p(r#"{$x|default:"a}b"}"#);
        assert_eq!(n.len(), 1);
    }
}
