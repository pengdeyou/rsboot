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
    /// 执行到的行号（`Stmt::At` 更新），出错时一起报。
    line: usize,
    depth: usize,
    /// `{cycle}` 的游标。键是 `{cycle}` 的 `name=`，默认空串。
    counters: HashMap<String, usize>,
}

/// include 的最大嵌套层数。环形 include 在运行期会无限递归，
/// 与其栈溢出崩掉整个 worker，不如在第 32 层报一个明确的错。
const MAX_DEPTH: usize = 32;

/// 循环上限。`{for $i=1 to 0}` 这类"方向写反"的模板本该走 `{forelse}`，
/// 但 `step` 是运行期算出来的，只能在跑够次数后拦住，别让一个模板把 worker 卡死。
const MAX_ITER: usize = 1_000_000;

impl<'a> Rt<'a> {
    pub(crate) fn new(engine: &'a Engine, name: &str, data: &Value) -> Self {
        let mut root = HashMap::new();
        // 全局变量先铺一层，业务数据后面覆盖：`{$site.name}` 这类站点级常量
        // 不必每个 handler 都往 data 里塞一遍。
        for (k, v) in engine.global_values() {
            root.insert(k.clone(), v.clone());
        }
        if let Value::Object(map) = data {
            for (k, v) in map {
                root.insert(k.clone(), v.clone());
            }
        }
        Self {
            engine,
            scopes: vec![root],
            current: name.to_string(),
            line: 0,
            depth: 0,
            counters: HashMap::new(),
        }
    }

    pub(crate) fn run_into(&mut self, nodes: &[Stmt], out: &mut String) -> Result<(), Error> {
        self.exec(nodes, out)
    }

    fn exec(&mut self, nodes: &[Stmt], out: &mut String) -> Result<(), Error> {
        for n in nodes {
            match n {
                Stmt::At(line) => self.line = *line,
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
                Stmt::For {
                    var,
                    start,
                    end,
                    step,
                    body,
                    else_,
                } => {
                    let start = self.num_of(start)?;
                    let end = self.num_of(end)?;
                    // 默认步长固定 1（与 Smarty 一致）：要倒着数必须显式写
                    // `step -1`，否则 `{for $i=3 to 1}` 直接走 `{forelse}`——
                    // 让方向随参数"猜"看着聪明，实际是让人不敢改数据。
                    let step = match step {
                        Some(e) => self.num_of(e)?,
                        None => 1.0,
                    };
                    if step == 0.0 {
                        return Err(self.err("for 的 `step` 不能为 0"));
                    }
                    let mut i = start;
                    let mut n = 0usize;
                    while (step > 0.0 && i <= end) || (step < 0.0 && i >= end) {
                        if n >= MAX_ITER {
                            return Err(self.err("for 循环超过 100 万次，检查 `to`/`step`"));
                        }
                        n += 1;
                        let mut scope = HashMap::new();
                        scope.insert(var.clone(), Value::Number(num(i)));
                        self.scopes.push(scope);
                        let r = self.exec(body, out);
                        self.scopes.pop();
                        r?;
                        i += step;
                    }
                    if n == 0 {
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
                        let total = items.len();
                        // 迭代前把所有元素拍平成一个 Vec：渲染期间不再访问
                        // 父层数据，也就不可能"边迭代边被修改"。
                        for (idx, (k, item)) in items.into_iter().enumerate() {
                            let mut scope = HashMap::with_capacity(8);
                            if let Some(kn) = key {
                                scope.insert(kn.clone(), k.clone());
                            }
                            // `$it@index` 这类循环属性实现成同作用域里的普通键：
                            // 查找时拼 `it@index` 即可，不必在 Rt 里维护一层"循环帧"。
                            let p = |s: &str| format!("{val}@{s}");
                            scope.insert(p("index"), Value::Number(idx.into()));
                            scope.insert(p("iteration"), Value::Number((idx + 1).into()));
                            scope.insert(p("first"), Value::Bool(idx == 0));
                            scope.insert(p("last"), Value::Bool(idx + 1 == total));
                            scope.insert(p("total"), Value::Number(total.into()));
                            scope.insert(p("key"), k.clone());
                            scope.insert(val.clone(), item);
                            self.scopes.push(scope);
                            let r = self.exec(body, out);
                            self.scopes.pop();
                            r?;
                        }
                    }
                }
                Stmt::Capture { name, append, body } => {
                    let mut buf = String::new();
                    self.exec(body, &mut buf)?;
                    match (append, self.scopes.last_mut()) {
                        (true, Some(scope)) => {
                            let slot = scope
                                .entry(name.clone())
                                .or_insert_with(|| Value::String(String::new()));
                            if let Value::String(s) = slot {
                                s.push_str(&buf);
                            } else {
                                *slot = Value::String(buf);
                            }
                        }
                        (false, Some(scope)) => {
                            scope.insert(name.clone(), Value::String(buf));
                        }
                        (_, None) => unreachable!("根作用域永远在"),
                    }
                }
                Stmt::Include { tpl, params } => {
                    let name = match self.eval(tpl)? {
                        Value::String(s) => s,
                        other => {
                            return Err(self.err(format!(
                                "include 需要一个字符串，拿到 {}",
                                self.display(&other)
                            )))
                        }
                    };
                    let Some(nodes) = self.engine.nodes(&name) else {
                        return Err(self.err(format!("找不到模板 `{name}`")));
                    };
                    if self.depth >= MAX_DEPTH {
                        return Err(self.err("include 嵌套太深（可能成环）"));
                    }
                    let mut scope = HashMap::with_capacity(params.len() + 1);
                    for (k, e) in params {
                        let v = self.eval(e)?;
                        scope.insert(k.clone(), v);
                    }
                    self.depth += 1;
                    let prev = std::mem::replace(&mut self.current, name);
                    let prev_line = std::mem::replace(&mut self.line, 0);
                    self.scopes.push(scope);
                    let r = self.exec(nodes, out);
                    self.scopes.pop();
                    self.current = prev;
                    self.line = prev_line;
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
                Stmt::Cycle {
                    name,
                    values,
                    reset,
                } => {
                    let key = match name {
                        Some(e) => self.display_of(e)?,
                        None => String::new(),
                    };
                    let vals = match self.eval(values)? {
                        Value::Array(a) => a
                            .iter()
                            .map(|x| self.display(x))
                            .collect::<Vec<_>>(),
                        Value::String(s) => s.split(',').map(|x| x.trim().to_string()).collect(),
                        other => vec![self.display(&other)],
                    };
                    if vals.is_empty() {
                        return Err(self.err("cycle 的 `values` 为空"));
                    }
                    let idx = if *reset {
                        self.counters.insert(key.clone(), 0);
                        0
                    } else {
                        self.counters.get(&key).copied().unwrap_or_default()
                    };
                    self.counters.insert(key, idx + 1);
                    let mut s = vals[idx % vals.len()].clone();
                    if self.engine.is_auto_escape() {
                        s = escape_html(&s);
                    }
                    out.push_str(&s);
                }
                Stmt::HtmlSelect {
                    name,
                    options,
                    selected,
                    empty,
                } => {
                    let name = self.display_of(name)?;
                    let options = self.eval(options)?;
                    let selected = match selected {
                        Some(e) => self.display_of(e)?,
                        None => String::new(),
                    };
                    let empty = match empty {
                        Some(e) => Some(self.display_of(e)?),
                        None => None,
                    };
                    let sel = if selected.is_empty() {
                        None
                    } else {
                        Some(selected.as_str())
                    };
                    out.push_str("<select name=\"");
                    out.push_str(&escape_html(&name));
                    out.push_str("\">");
                    if let Some(label) = &empty {
                        out.push_str("<option value=\"\"");
                        if sel.is_none() {
                            out.push_str(" selected");
                        }
                        out.push('>');
                        out.push_str(&escape_html(label));
                        out.push_str("</option>");
                    }
                    let items: Vec<Value> = match options {
                        Value::Array(a) => a,
                        Value::Object(m) => m.into_values().collect(),
                        other => vec![other],
                    };
                    for it in items {
                        let (val, label) = match &it {
                            Value::Object(m) => {
                                let v = m.get("value").cloned().unwrap_or(Value::Null);
                                let l = m.get("label").cloned().unwrap_or_else(|| v.clone());
                                (self.display(&v), self.display(&l))
                            }
                            other => (self.display(other), self.display(other)),
                        };
                        out.push_str("<option value=\"");
                        out.push_str(&escape_html(&val));
                        out.push('"');
                        if sel == Some(val.as_str()) {
                            out.push_str(" selected");
                        }
                        out.push('>');
                        out.push_str(&escape_html(&label));
                        out.push_str("</option>");
                    }
                    out.push_str("</select>");
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

    /// 求值后再 `display`。分成两步是借用检查的要求，
    /// 顺便让"求值 + 格式化"在调用处只占一行。
    fn display_of(&mut self, e: &Expr) -> Result<String, Error> {
        let v = self.eval(e)?;
        Ok(self.display(&v))
    }

    fn num_of(&mut self, e: &Expr) -> Result<f64, Error> {
        let v = self.eval(e)?;
        self.num(&v)
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
                    // `"2" == 2` 在模板里几乎总是想要的结果（查询参数都是字符串），
                    // 只在两侧都能当数字时才宽松比较，其余仍按类型严格比。
                    (Value::String(a), Value::Number(b)) | (Value::Number(b), Value::String(a)) => {
                        match (a.trim().parse::<f64>(), b.as_f64()) {
                            (Ok(x), Some(y)) => x == y,
                            _ => false,
                        }
                    }
                    _ => l == r,
                };
                Value::Bool(if matches!(op, BinOp::Eq) { eq } else { !eq })
            }
            BinOp::Gt | BinOp::Ge | BinOp::Lt | BinOp::Le => {
                let ord = match (&l, &r) {
                    (Value::Number(a), Value::Number(b)) => a
                        .as_f64()
                        .unwrap_or(0.0)
                        .partial_cmp(&b.as_f64().unwrap_or(0.0)),
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
            Access::Prop(_) => return Ok(Value::Null),
        };
        // `$it@index`：属性不是数据里的字段，而是迭代时插进作用域的 `it@index`。
        if let [Access::Key(_), Access::Prop(prop)] = path {
            let key = format!("{name}@{prop}");
            for scope in self.scopes.iter().rev() {
                if let Some(v) = scope.get(&key) {
                    return Ok(v.clone());
                }
            }
            return Ok(Value::Null);
        }
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
                // 属性只支持 `$var@prop` 这种一级形式，混在深处就是笔误
                (Access::Prop(_), _) => Value::Null,
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

    pub(crate) fn display(&self, v: &Value) -> String {
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
            line: self.line,
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
        "capitalize" => (Value::String(capitalize(&as_str(&v))), false),
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
        "length" | "count" => {
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
        "substr" => {
            let s = as_str(&v);
            let chars: Vec<char> = s.chars().collect();
            let start = arg_str(0, "0").parse::<i64>().unwrap_or(0);
            let len = args.get(1).and_then(|x| as_str(x).parse::<i64>().ok());
            let begin = if start < 0 {
                (chars.len() as i64 + start).max(0) as usize
            } else {
                (start as usize).min(chars.len())
            };
            let take = match len {
                Some(n) if n < 0 => {
                    // 负数长度 = 去掉末尾 n 个字符
                    (chars.len() as i64 + n).max(begin as i64) as usize - begin
                }
                Some(n) => (n as usize).min(chars.len() - begin),
                None => chars.len() - begin,
            };
            (Value::String(chars[begin..begin + take].iter().collect()), false)
        }
        "replace" => {
            let from = arg_str(0, "");
            let to = arg_str(1, "");
            if from.is_empty() {
                (v, false)
            } else {
                (Value::String(as_str(&v).replace(&from, &to)), false)
            }
        }
        "split" => {
            let sep = arg_str(0, ",");
            let s = as_str(&v);
            let parts: Vec<Value> = if s.is_empty() {
                Vec::new()
            } else {
                s.split(&sep).map(|x| Value::String(x.to_string())).collect()
            };
            (Value::Array(parts), true)
        }
        "join" => {
            let sep = arg_str(0, ",");
            match v {
                Value::Array(a) => {
                    let parts: Vec<String> = a.iter().map(as_str).collect();
                    (Value::String(parts.join(&sep)), false)
                }
                other => (Value::String(as_str(&other)), false),
            }
        }
        "pad" => {
            let want: usize = arg_str(0, "8").parse().unwrap_or(8);
            let fill = arg_str(1, "0");
            let right = arg_str(2, "left").eq_ignore_ascii_case("right");
            let s = as_str(&v);
            let have = s.chars().count();
            if have >= want || fill.is_empty() {
                (Value::String(s), false)
            } else {
                let mut pad = String::new();
                for c in fill.chars().cycle().take(want - have) {
                    pad.push(c);
                }
                let out = if right {
                    format!("{s}{pad}")
                } else {
                    format!("{pad}{s}")
                };
                (Value::String(out), false)
            }
        }
        "repeat" => {
            let n: usize = arg_str(0, "1").parse().unwrap_or(1);
            let s = as_str(&v);
            (Value::String(s.repeat(n.min(1_000_000))), false)
        }
        "number_format" => {
            let dec: usize = arg_str(0, "0").parse().unwrap_or(0);
            let dec_point = arg_str(1, ".");
            let thousand = arg_str(2, ",");
            let f = match &v {
                Value::Number(n) => n.as_f64().unwrap_or(0.0),
                Value::String(s) => s.trim().parse().unwrap_or(0.0),
                _ => 0.0,
            };
            (
                Value::String(number_format(f, dec, &dec_point, &thousand)),
                false,
            )
        }
        "date" => {
            let fmt = arg_str(0, "%Y-%m-%d %H:%M:%S");
            let ts = match &v {
                Value::Number(n) => n.as_i64().unwrap_or(0),
                Value::String(s) => s.trim().parse().unwrap_or(0),
                _ => 0,
            };
            (Value::String(format_date(ts, &fmt)), false)
        }
        "urlencode" => (Value::String(urlencode(&as_str(&v))), false),
        "strip_tags" => (Value::String(strip_tags(&as_str(&v))), false),
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
        other => return Err(format!("不认识的修饰符 `{other}`")),
    })
}

/// `{* 只把每段的首字母大写 *}`，按空白分段。
fn capitalize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut start = true;
    for c in s.chars() {
        if c.is_whitespace() {
            start = true;
            out.push(c);
        } else if start {
            out.extend(c.to_uppercase());
            start = false;
        } else {
            out.push(c);
        }
    }
    out
}

/// `1234567.891` → `1,234,567.89`（负数、零、超大数都按普通十进制处理）。
fn number_format(v: f64, dec: usize, dec_point: &str, thousand: &str) -> String {
    if !v.is_finite() {
        return "0".to_string();
    }
    let neg = v < 0.0;
    let s = format!("{:.*}", dec, v.abs());
    let (ip, fp) = match s.split_once('.') {
        Some((a, b)) => (a.to_string(), Some(b.to_string())),
        None => (s, None),
    };
    let mut out = String::with_capacity(ip.len() + ip.len() / 3 + dec + 1);
    for (i, c) in ip.chars().enumerate() {
        if i > 0 && (ip.len() - i) % 3 == 0 {
            out.push_str(thousand);
        }
        out.push(c);
    }
    if let Some(f) = fp {
        out.push_str(dec_point);
        out.push_str(&f);
    }
    if neg {
        out.insert(0, '-');
    }
    out
}

const WEEKDAY_SHORT: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const WEEKDAY_LONG: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];
const MONTH_SHORT: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];
const MONTH_LONG: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

/// 时间戳按 **UTC** 格式化。为什么不做本地时区：模板引擎不该读进程环境
/// （`TZ` 一变输出就变，缓存和测试都没法稳定）。要本地时间就在数据里传
/// 已经算好的字符串，或者传本地偏移后的时间戳。
fn format_date(ts: i64, fmt: &str) -> String {
    let days = ts.div_euclid(86_400);
    let secs = ts.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let wd = ((days.rem_euclid(7) + 4) % 7) as usize;
    let h = secs / 3600;
    let mi = (secs % 3600) / 60;
    let s = secs % 60;
    let doy = day_of_year(y, m, d);
    let mut out = String::with_capacity(fmt.len() + 8);
    let mut it = fmt.chars().peekable();
    while let Some(c) = it.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('Y') => out.push_str(&y.to_string()),
            Some('y') => out.push_str(&format!("{:02}", y % 100)),
            Some('m') => out.push_str(&format!("{:02}", m)),
            Some('n') => out.push_str(&m.to_string()),
            Some('d') => out.push_str(&format!("{:02}", d)),
            Some('j') => out.push_str(&format!("{:03}", doy)),
            Some('H') => out.push_str(&format!("{:02}", h)),
            Some('M') => out.push_str(&format!("{:02}", mi)),
            Some('S') => out.push_str(&format!("{:02}", s)),
            Some('A') => out.push_str(WEEKDAY_LONG[wd]),
            Some('a') => out.push_str(WEEKDAY_SHORT[wd]),
            Some('B') => out.push_str(MONTH_LONG[(m - 1) as usize]),
            Some('b') => out.push_str(MONTH_SHORT[(m - 1) as usize]),
            Some('%') => out.push('%'),
            Some(other) => {
                // 不认识的占位符原样留着：写错格式时能一眼看出是哪个
                out.push('%');
                out.push(other);
            }
            None => out.push('%'),
        }
    }
    out
}

/// 天数 → (年, 月, 日)。Howard Hinnant 的 civil_from_days，1970-01-01 为 0。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn day_of_year(y: i64, m: u32, d: u32) -> u32 {
    const CUM: [u32; 12] = [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    CUM[(m - 1) as usize] + d + u32::from(leap && m > 2)
}

/// 百分号编码，只保留 RFC 3986 的非保留字符。空格编成 `%20`（不是 `+`）。
fn urlencode(s: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.as_bytes() {
        match *b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            other => {
                out.push('%');
                out.push(HEX[(other >> 4) as usize] as char);
                out.push(HEX[(other & 0x0F) as usize] as char);
            }
        }
    }
    out
}

/// 去掉 `<...>` 标签，保留里面的文字。
fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match (in_tag, c) {
            (false, '<') => in_tag = true,
            (true, '>') => in_tag = false,
            (false, other) => out.push(other),
            (true, _) => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn number_format_groups_thousands() {
        assert_eq!(number_format(1234567.891, 2, ".", ","), "1,234,567.89");
        assert_eq!(number_format(-1234.0, 0, ".", ","), "-1,234");
        assert_eq!(number_format(0.0, 0, ".", ","), "0");
        assert_eq!(number_format(999.0, 2, ".", ","), "999.00");
    }

    #[test]
    fn date_uses_utc_and_handles_leap_day() {
        // 1704067200 = 2024-01-01 00:00:00 UTC
        assert_eq!(
            format_date(1_704_067_200, "%Y-%m-%d %H:%M:%S %a"),
            "2024-01-01 00:00:00 Mon"
        );
        // 1709164800 = 2024-02-29 00:00:00 UTC，闰日
        assert_eq!(format_date(1_709_164_800, "%Y-%m-%d %j"), "2024-02-29 060");
        // 负数时间戳（1970 以前）也要对
        assert_eq!(format_date(-86_400, "%Y-%m-%d"), "1969-12-31");
    }

    #[test]
    fn urlencode_keeps_unreserved_only() {
        assert_eq!(urlencode("a b/c?d=1&e"), "a%20b%2Fc%3Fd%3D1%26e");
        assert_eq!(urlencode("中文"), "%E4%B8%AD%E6%96%87");
    }

    #[test]
    fn strip_tags_drops_markup() {
        assert_eq!(strip_tags("<b>粗</b>体"), "粗体");
        assert_eq!(strip_tags("a < b"), "a ");
    }
}
