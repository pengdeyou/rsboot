//! 求值：语句 → 输出字符串。
//!
//! 作用域是**一个栈**（`Vec<HashMap>`），查找从内层往外走。`foreach` 压一层、
//! `include` 压一层——include 因此天然看得见父模板的变量（Smarty 的语义），
//! 又不会把子模板里的东西漏回父层（子层弹出即丢）。

use std::collections::HashMap;

use serde_json::Value;

use crate::parse::{Access, BinOp, Expr, Stmt};
use crate::{Engine, Error};

/// 单次渲染的上下文。
pub(crate) struct Rt<'a> {
    engine: &'a Engine,
    scopes: Vec<HashMap<String, Value>>,
    /// 当前模板名，出错时报给使用者。
    current: String,
    depth: usize,
}

/// include 的最大嵌套层数。环形 include 在运行期会无限递归，
/// 与其栈溢出崩掉整个 worker，不如在第 32 层报一个明确的错。
const MAX_DEPTH: usize = 32;

impl<'a> Rt<'a> {
    pub(crate) fn new(engine: &'a Engine, name: &str, data: &Value) -> Self {
        let mut root = HashMap::new();
        if let Value::Object(map) = data {
            for (k, v) in map {
                root.insert(k.clone(), v.clone());
            }
        }
        // 允许 `{"data": {...}, "user": ...}` 之外再包一层：很多人习惯
        // 把所有东西塞进 `data`。两种取法都支持，不做两套 API。
        Self {
            engine,
            scopes: vec![root],
            current: name.to_string(),
            depth: 0,
        }
    }

    pub(crate) fn run(&mut self, nodes: &[Stmt]) -> Result<String, Error> {
        let mut out = String::with_capacity(1024);
        self.exec(nodes, &mut out)?;
        Ok(out)
    }

    fn exec(&mut self, nodes: &[Stmt], out: &mut String) -> Result<(), Error> {
        for n in nodes {
            match n {
                Stmt::Text(t) => out.push_str(t),
                Stmt::Literal(t) => out.push_str(t),
                Stmt::Print(e) => {
                    let (v, escaped) = self.eval_with_mods(e)?;
                    let mut s = self.display(&v);
                    if self.engine.is_auto_escape() && !escaped {
                        s = escape_html(&s);
                    }
                    out.push_str(&s);
                }
                Stmt::If { arms, else_ } => {
                    let mut hit = false;
                    for (cond, body) in arms {
                        let cv = self.eval(cond)?;
                        if self.truthy(&cv) {
                            self.exec(body, out)?;
                            hit = true;
                            break;
                        }
                    }
                    if !hit {
                        self.exec(else_, out)?;
                    }
                }
                Stmt::ForEach {
                    iter,
                    key,
                    val,
                    body,
                    else_,
                } => {
                    let v = self.eval(iter)?;
                    let items = self.iter_items(v);
                    if items.is_empty() {
                        self.exec(else_, out)?;
                    } else {
                        // 迭代前把所有元素拍平成一个 Vec：渲染期间不再访问
                        // 父层数据，也就不可能"边迭代边被修改"。
                        for (k, item) in items {
                            let mut scope = HashMap::new();
                            if let Some(kn) = key {
                                scope.insert(kn.clone(), k);
                            }
                            scope.insert(val.clone(), item);
                            self.scopes.push(scope);
                            let r = self.exec(body, out);
                            self.scopes.pop();
                            r?;
                        }
                    }
                }
                Stmt::Include(e) => {
                    let name = match self.eval(e)? {
                        Value::String(s) => s,
                        other => {
                            return Err(self.err(format!(
                                "include 需要一个字符串，拿到 {}",
                                self.display(&other)
                            )))
                        }
                    };
                    let Some(tpl) = self.engine.nodes(&name) else {
                        return Err(self.err(format!("找不到模板 `{name}`")));
                    };
                    if self.depth >= MAX_DEPTH {
                        return Err(self.err("include 嵌套太深（可能成环）"));
                    }
                    self.depth += 1;
                    let prev = std::mem::replace(&mut self.current, name);
                    self.scopes.push(HashMap::new());
                    let r = self.exec(tpl, out);
                    self.scopes.pop();
                    self.current = prev;
                    self.depth -= 1;
                    r?;
                }
                // `{block}` 是个透明容器：继承已经在编译期展开完了，
                // 渲染期它只是"把 body 原样跑一遍"。
                Stmt::Block { body, .. } => self.exec(body, out)?,
                // 写当前最内层作用域：`{foreach}` 里 assign 只影响这一轮迭代，
                // 弹出就没了，这跟 PHP 里 for 循环内赋值不一样，但更不容易串味。
                Stmt::Assign { name, expr } => {
                    let v = self.eval(expr)?;
                    if let Some(scope) = self.scopes.last_mut() {
                        scope.insert(name.clone(), v);
                    }
                }
                Stmt::Extends(_) => {
                    return Err(self.err("内部错误：`{extends}` 没有在编译期展开"))
                }
                Stmt::Call { name, args } => {
                    let Some(f) = self.engine.lookup_func(name) else {
                        return Err(self.err(format!("没有注册函数 `{{{name}}}`")));
                    };
                    let mut av = Vec::with_capacity(args.len());
                    for (k, e) in args {
                        av.push((k.as_str(), self.eval(e)?));
                    }
                    let v = f(&av).map_err(|m| self.err(format!("函数 `{name}` 失败：{m}")))?;
                    let mut s = self.display(&v);
                    if self.engine.is_auto_escape() {
                        s = escape_html(&s);
                    }
                    out.push_str(&s);
                }
            }
        }
        Ok(())
    }

    fn eval_with_mods(&mut self, e: &Expr) -> Result<(Value, bool), Error> {
        match e {
            Expr::Mod { base, name, args } => {
                let (v, mut escaped) = self.eval_with_mods(base)?;
                let mut a = Vec::with_capacity(args.len());
                for x in args {
                    a.push(self.eval(x)?);
                }
                let (v2, esc2) = apply_mod(name, v, &a)
                    .map_err(|m| self.err(format!("修饰符 `{name}` 失败：{m}")))?;
                escaped |= esc2;
                Ok((v2, escaped))
            }
            other => Ok((self.eval(other)?, false)),
        }
    }

    fn eval(&mut self, e: &Expr) -> Result<Value, Error> {
        Ok(match e {
            Expr::Lit(v) => v.clone(),
            Expr::Var(path) => self.lookup(path)?,
            Expr::Neg(x) => {
                let v = self.eval(x)?;
                Value::Number(num(-self.num(&v)?))
            }
            Expr::Not(x) => {
                let v = self.eval(x)?;
                Value::Bool(!self.truthy(&v))
            }
            Expr::Bin(op, l, r) => {
                if matches!(op, BinOp::And | BinOp::Or) {
                    // 短路：`$a and $b` 里 `$b` 可能读到一个不存在的字段
                    let lv = self.eval(l)?;
                    let lb = self.truthy(&lv);
                    return match (op, lb) {
                        (BinOp::And, false) => Ok(Value::Bool(false)),
                        (BinOp::Or, true) => Ok(Value::Bool(true)),
                        _ => {
                            let rv = self.eval(r)?;
                            Ok(Value::Bool(self.truthy(&rv)))
                        }
                    };
                }
                let lv = self.eval(l)?;
                let rv = self.eval(r)?;
                self.binop(*op, lv, rv)?
            }
            // `Expr::Mod` 走 `eval_with_mods`，走到这里说明是嵌套在运算里，
            // 忽略转义标记即可。
            Expr::Mod { .. } => self.eval_with_mods(e)?.0,
        })
    }

    fn binop(&self, op: BinOp, l: Value, r: Value) -> Result<Value, Error> {
        Ok(match op {
            BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod => {
                let a = self.num(&l)?;
                let b = self.num(&r)?;
                let v = match op {
                    BinOp::Add => a + b,
                    BinOp::Sub => a - b,
                    BinOp::Mul => a * b,
                    BinOp::Div => {
                        if b == 0.0 {
                            return Err(self.err("除以零"));
                        }
                        a / b
                    }
                    _ => {
                        if b == 0.0 {
                            return Err(self.err("对零取模"));
                        }
                        a % b
                    }
                };
                Value::Number(num(v))
            }
            BinOp::Eq | BinOp::Ne => {
                let eq = match (&l, &r) {
                    (Value::Number(a), Value::Number(b)) => {
                        a.as_f64().unwrap_or(0.0) == b.as_f64().unwrap_or(0.0)
                    }
                    _ => l == r,
                };
                Value::Bool(if matches!(op, BinOp::Eq) { eq } else { !eq })
            }
            BinOp::Gt | BinOp::Ge | BinOp::Lt | BinOp::Le => {
                let ord = match (&l, &r) {
                    (Value::Number(a), Value::Number(b)) => {
                        a.as_f64().unwrap_or(0.0).partial_cmp(&b.as_f64().unwrap_or(0.0))
                    }
                    (Value::String(a), Value::String(b)) => Some(a.cmp(b)),
                    _ => None,
                };
                let res = match (op, ord) {
                    (BinOp::Gt, Some(o)) => o.is_gt(),
                    (BinOp::Ge, Some(o)) => o.is_gt() || o.is_eq(),
                    (BinOp::Lt, Some(o)) => o.is_lt(),
                    (BinOp::Le, Some(o)) => o.is_lt() || o.is_eq(),
                    _ => false,
                };
                Value::Bool(res)
            }
            BinOp::And | BinOp::Or => unreachable!("逻辑运算在 eval 里短路处理"),
        })
    }

    fn lookup(&mut self, path: &[Access]) -> Result<Value, Error> {
        let Some(first) = path.first() else {
            return Ok(Value::Null);
        };
        let name = match first {
            Access::Key(k) => k,
            // 不可能以 `[` 开头，解析器只允许 `$var[...]`
            Access::Index(_) => return Ok(Value::Null),
        };
        let mut cur = Value::Null;
        let mut found = false;
        for scope in self.scopes.iter().rev() {
            if let Some(v) = scope.get(name) {
                cur = v.clone();
                found = true;
                break;
            }
        }
        if !found {
            // 缺字段不当错误：模板里 `{$maybe}` 很常见，渲染成空串更好用。
            return Ok(Value::Null);
        }
        for acc in &path[1..] {
            cur = match (acc, &cur) {
                (Access::Key(k), Value::Object(m)) => m.get(k).cloned().unwrap_or(Value::Null),
                (Access::Index(i), v) => {
                    let idx = match self.eval(i)? {
                        Value::Number(n) => n.as_u64().unwrap_or(u64::MAX) as usize,
                        Value::String(s) => {
                            // `$obj[$key]` 也支持
                            if let Value::Object(m) = v {
                                return Ok(m.get(&s).cloned().unwrap_or(Value::Null));
                            }
                            match s.parse::<usize>() {
                                Ok(n) => n,
                                Err(_) => usize::MAX,
                            }
                        }
                        _ => usize::MAX,
                    };
                    match v {
                        Value::Array(a) => a.get(idx).cloned().unwrap_or(Value::Null),
                        _ => Value::Null,
                    }
                }
                (Access::Key(_), _) => Value::Null,
            };
        }
        Ok(cur)
    }

    fn num(&self, v: &Value) -> Result<f64, Error> {
        Ok(match v {
            Value::Number(n) => n.as_f64().unwrap_or(0.0),
            Value::String(s) => s.trim().parse().unwrap_or(0.0),
            Value::Bool(b) => {
                if *b {
                    1.0
                } else {
                    0.0
                }
            }
            Value::Null => 0.0,
            _ => return Err(self.err("不能把数组/对象当数字用")),
        })
    }

    fn truthy(&self, v: &Value) -> bool {
        match v {
            Value::Null => false,
            Value::Bool(b) => *b,
            Value::Number(n) => n.as_f64().unwrap_or(0.0) != 0.0,
            Value::String(s) => !s.is_empty(),
            Value::Array(a) => !a.is_empty(),
            Value::Object(m) => !m.is_empty(),
        }
    }

    /// `foreach` 展开成 `Vec<(键, 值)>`：数组给下标，对象给字段名。
    fn iter_items(&self, v: Value) -> Vec<(Value, Value)> {
        match v {
            Value::Array(a) => a
                .into_iter()
                .enumerate()
                .map(|(i, x)| (Value::Number(i.into()), x))
                .collect(),
            Value::Object(m) => m
                .into_iter()
                .map(|(k, x)| (Value::String(k), x))
                .collect(),
            _ => Vec::new(),
        }
    }

    fn display(&self, v: &Value) -> String {
        match v {
            Value::Null => String::new(),
            Value::Bool(b) => if *b { "true" } else { "false" }.to_string(),
            Value::Number(n) => n.to_string(),
            Value::String(s) => s.clone(),
            // 数组/对象：宁可给出 JSON，也不要打印一个看不出结构的 `Array`
            other => serde_json::to_string(other).unwrap_or_default(),
        }
    }

    fn err(&self, msg: impl Into<String>) -> Error {
        Error {
            template: self.current.clone(),
            line: 0,
            msg: msg.into(),
        }
    }
}

fn num(v: f64) -> serde_json::Number {
    if v.fract() == 0.0 && v.abs() < 9.2e18 {
        serde_json::Number::from(v as i64)
    } else {
        serde_json::Number::from_f64(v).unwrap_or_else(|| serde_json::Number::from(0))
    }
}

/// HTML 转义。`& < > " '` 五个字符，覆盖属性值与文本两种位置。
pub fn escape_html(s: &str) -> String {
    if !s
        .as_bytes()
        .iter()
        .any(|b| matches!(b, b'&' | b'<' | b'>' | b'"' | b'\''))
    {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

/// 修饰符。返回 `(新值, 是否已经安全)`——`escape` / `raw` / `nl2br` 之后
/// 不能再转一次，否则 `&amp;` 会变成 `&amp;amp;`。
fn apply_mod(name: &str, v: Value, args: &[Value]) -> Result<(Value, bool), String> {
    let as_str = |v: &Value| -> String {
        match v {
            Value::String(s) => s.clone(),
            Value::Null => String::new(),
            Value::Bool(b) => if *b { "true" } else { "false" }.to_string(),
            Value::Number(n) => n.to_string(),
            other => serde_json::to_string(other).unwrap_or_default(),
        }
    };
    let arg_str = |i: usize, dflt: &str| -> String {
        args.get(i).map(as_str).unwrap_or_else(|| dflt.to_string())
    };
    Ok(match name {
        "escape" | "e" | "html" => (Value::String(escape_html(&as_str(&v))), true),
        // 明确要求原样输出（内容由后端自己保证安全）
        "raw" => (v, true),
        "upper" => (Value::String(as_str(&v).to_uppercase()), false),
        "lower" => (Value::String(as_str(&v).to_lowercase()), false),
        "trim" => (Value::String(as_str(&v).trim().to_string()), false),
        "nl2br" => (Value::String(as_str(&v).replace('\n', "<br>")), true),
        "default" => {
            let empty = match &v {
                Value::Null => true,
                Value::String(s) => s.is_empty(),
                Value::Array(a) => a.is_empty(),
                Value::Object(m) => m.is_empty(),
                _ => false,
            };
            if empty {
                (args.first().cloned().unwrap_or(Value::Null), false)
            } else {
                (v, false)
            }
        }
        "length" => {
            let n = match &v {
                Value::String(s) => s.chars().count(),
                Value::Array(a) => a.len(),
                Value::Object(m) => m.len(),
                _ => 0,
            };
            (Value::Number(n.into()), false)
        }
        "truncate" => {
            let n: usize = arg_str(0, "30").parse().unwrap_or(30);
            let tail = arg_str(1, "...");
            let s = as_str(&v);
            if s.chars().count() <= n {
                (Value::String(s), false)
            } else {
                let head: String = s.chars().take(n).collect();
                (Value::String(format!("{head}{tail}")), false)
            }
        }
        "json" => {
            // JSON 常被塞进 `<script>` 或 `data-*`：直接输出会把 `</script>`
            // 提前结束掉。Jinja 的 `tojson` 也是这个做法——把能改变 HTML
            // 词法的四个字符转成 `\uXXXX`，JSON 语义不变，插哪儿都安全。
            let s = serde_json::to_string(&v).map_err(|e| e.to_string())?;
            let s = s
                .replace('<', "\\u003c")
                .replace('>', "\\u003e")
                .replace('&', "\\u0026")
                .replace('\'', "\\u0027");
            (Value::String(s), true)
        }
        "join" => {
            let sep = arg_str(0, ",");
            match v {
                Value::Array(a) => {
                    let parts: Vec<String> = a.iter().map(|x| as_str(x)).collect();
                    (Value::String(parts.join(&sep)), false)
                }
                other => (Value::String(as_str(&other)), false),
            }
        }
        other => return Err(format!("不认识的修饰符 `{other}`")),
    })
}
