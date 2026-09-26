//! A small, safe expression language (§2.5).
//!
//! ```text
//! event.tier >= 2 && mode == 'live'
//! scene in ['duo', 'wide'] and not queue.playing
//! clamp(music.bass * 2, 0, 1) > 0.5 ? 'hot' : 'cold'
//! ```
//!
//! No loops, no assignment, no I/O: evaluation is bounded by the size of the AST.
//! Identifiers are dotted paths resolved through a [`Scope`].

use se_proto::Value;
use std::fmt;

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("{msg} at {pos}")]
pub struct ExprError {
    pub msg: String,
    pub pos: usize,
}

/// Resolves identifiers. Unknown paths should return `Value::Null`.
pub trait Scope {
    fn lookup(&self, path: &str) -> Value;
}

impl<F: Fn(&str) -> Value> Scope for F {
    fn lookup(&self, path: &str) -> Value {
        self(path)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    Lit(Value),
    Path(String),
    List(Vec<Node>),
    Unary(UnOp, Box<Node>),
    Binary(BinOp, Box<Node>, Box<Node>),
    Cond(Box<Node>, Box<Node>, Box<Node>),
    Call(String, Vec<Node>),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum UnOp {
    Not,
    Neg,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BinOp {
    Or,
    And,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    In,
    NotIn,
    Add,
    Sub,
    Mul,
    Div,
    Rem,
}

/// A parsed expression; parse once, evaluate many times.
#[derive(Debug, Clone, PartialEq)]
pub struct Expr {
    src: String,
    root: Node,
}

impl fmt::Display for Expr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.src)
    }
}

const MAX_DEPTH: usize = 64;
const MAX_LEN: usize = 4096;

impl Expr {
    pub fn parse(src: &str) -> Result<Expr, ExprError> {
        if src.len() > MAX_LEN {
            return Err(ExprError { msg: "expression too long".into(), pos: 0 });
        }
        let toks = lex(src)?;
        let mut p = Parser { toks, i: 0, depth: 0 };
        let root = p.expr()?;
        if p.i < p.toks.len() {
            return Err(ExprError { msg: format!("unexpected `{}`", p.toks[p.i].0), pos: p.toks[p.i].1 });
        }
        Ok(Expr { src: src.to_string(), root })
    }

    pub fn source(&self) -> &str {
        &self.src
    }

    pub fn root(&self) -> &Node {
        &self.root
    }

    pub fn eval(&self, scope: &dyn Scope) -> Value {
        eval(&self.root, scope)
    }

    pub fn eval_bool(&self, scope: &dyn Scope) -> bool {
        self.eval(scope).truthy()
    }

    /// Every identifier path the expression reads (for dependency tracking and autocomplete).
    pub fn paths(&self) -> Vec<String> {
        fn walk(n: &Node, out: &mut Vec<String>) {
            match n {
                Node::Path(p) => out.push(p.clone()),
                Node::List(l) | Node::Call(_, l) => l.iter().for_each(|x| walk(x, out)),
                Node::Unary(_, a) => walk(a, out),
                Node::Binary(_, a, b) => {
                    walk(a, out);
                    walk(b, out)
                }
                Node::Cond(a, b, c) => {
                    walk(a, out);
                    walk(b, out);
                    walk(c, out)
                }
                Node::Lit(_) => {}
            }
        }
        let mut v = Vec::new();
        walk(&self.root, &mut v);
        v
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Num(f64, bool),
    Str(String),
    Ident(String),
    Sym(&'static str),
}

impl fmt::Display for Tok {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Tok::Num(n, _) => write!(f, "{n}"),
            Tok::Str(s) => write!(f, "'{s}'"),
            Tok::Ident(s) => write!(f, "{s}"),
            Tok::Sym(s) => write!(f, "{s}"),
        }
    }
}

const SYMS: &[&str] = &["&&", "||", "==", "!=", "<=", ">=", "<", ">", "!", "+", "-", "*", "/", "%", "(", ")", "[", "]", ",", "?", ":"];

fn lex(src: &str) -> Result<Vec<(Tok, usize)>, ExprError> {
    let b = src.as_bytes();
    let mut i = 0;
    let mut out = Vec::new();
    while i < b.len() {
        let c = b[i] as char;
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        let start = i;
        if c.is_ascii_digit() || (c == '.' && b.get(i + 1).is_some_and(|d| d.is_ascii_digit())) {
            let mut is_float = false;
            while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'.' || b[i] == b'_') {
                if b[i] == b'.' {
                    // stop at `.` followed by a non-digit (not part of the number)
                    if !b.get(i + 1).is_some_and(|d| d.is_ascii_digit()) {
                        break;
                    }
                    is_float = true;
                }
                i += 1;
            }
            let s: String = src[start..i].chars().filter(|c| *c != '_').collect();
            let n: f64 = s.parse().map_err(|_| ExprError { msg: format!("bad number `{s}`"), pos: start })?;
            out.push((Tok::Num(n, is_float), start));
            continue;
        }
        if c == '\'' || c == '"' {
            i += 1;
            let mut s = String::new();
            loop {
                let Some(&ch) = b.get(i) else {
                    return Err(ExprError { msg: "unterminated string".into(), pos: start });
                };
                if ch as char == c {
                    i += 1;
                    break;
                }
                if ch == b'\\' && i + 1 < b.len() {
                    i += 1;
                }
                // handle UTF-8 by slicing chars
                let ch_str = &src[i..];
                let chr = ch_str.chars().next().unwrap();
                s.push(chr);
                i += chr.len_utf8();
            }
            out.push((Tok::Str(s), start));
            continue;
        }
        if c.is_ascii_alphabetic() || c == '_' || c == '$' {
            i += 1;
            while i < b.len() {
                let ch = b[i];
                let part_of_path = ch.is_ascii_alphanumeric()
                    || ch == b'_'
                    || (ch == b'.' && b.get(i + 1).is_some_and(|d| d.is_ascii_alphanumeric() || *d == b'_' || *d == b'*'))
                    || (ch == b'*' && b[i - 1] == b'.');
                if !part_of_path {
                    break;
                }
                i += 1;
            }
            out.push((Tok::Ident(src[start..i].to_string()), start));
            continue;
        }
        if let Some(sym) = SYMS.iter().find(|s| src[i..].starts_with(**s)) {
            i += sym.len();
            out.push((Tok::Sym(sym), start));
            continue;
        }
        return Err(ExprError { msg: format!("unexpected character `{c}`"), pos: start });
    }
    Ok(out)
}

struct Parser {
    toks: Vec<(Tok, usize)>,
    i: usize,
    depth: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.i).map(|t| &t.0)
    }
    fn pos(&self) -> usize {
        self.toks.get(self.i).map(|t| t.1).unwrap_or(usize::MAX)
    }
    fn err<T>(&self, msg: &str) -> Result<T, ExprError> {
        Err(ExprError { msg: msg.to_string(), pos: self.pos() })
    }
    fn eat_sym(&mut self, s: &str) -> bool {
        if matches!(self.peek(), Some(Tok::Sym(x)) if *x == s) {
            self.i += 1;
            true
        } else {
            false
        }
    }
    fn eat_kw(&mut self, k: &str) -> bool {
        if matches!(self.peek(), Some(Tok::Ident(x)) if x == k) {
            self.i += 1;
            true
        } else {
            false
        }
    }
    fn enter(&mut self) -> Result<(), ExprError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return self.err("expression nested too deeply");
        }
        Ok(())
    }

    fn expr(&mut self) -> Result<Node, ExprError> {
        self.enter()?;
        let c = self.or()?;
        let r = if self.eat_sym("?") {
            let a = self.expr()?;
            if !self.eat_sym(":") {
                return self.err("expected `:`");
            }
            let b = self.expr()?;
            Node::Cond(Box::new(c), Box::new(a), Box::new(b))
        } else {
            c
        };
        self.depth -= 1;
        Ok(r)
    }

    fn or(&mut self) -> Result<Node, ExprError> {
        let mut l = self.and()?;
        while self.eat_sym("||") || self.eat_kw("or") {
            let r = self.and()?;
            l = Node::Binary(BinOp::Or, Box::new(l), Box::new(r));
        }
        Ok(l)
    }

    fn and(&mut self) -> Result<Node, ExprError> {
        let mut l = self.not()?;
        while self.eat_sym("&&") || self.eat_kw("and") {
            let r = self.not()?;
            l = Node::Binary(BinOp::And, Box::new(l), Box::new(r));
        }
        Ok(l)
    }

    fn not(&mut self) -> Result<Node, ExprError> {
        if self.eat_sym("!") || self.eat_kw("not") {
            self.enter()?;
            let n = self.not()?;
            self.depth -= 1;
            return Ok(Node::Unary(UnOp::Not, Box::new(n)));
        }
        self.cmp()
    }

    fn cmp(&mut self) -> Result<Node, ExprError> {
        let l = self.add()?;
        let op = match self.peek() {
            Some(Tok::Sym("==")) => BinOp::Eq,
            Some(Tok::Sym("!=")) => BinOp::Ne,
            Some(Tok::Sym("<")) => BinOp::Lt,
            Some(Tok::Sym("<=")) => BinOp::Le,
            Some(Tok::Sym(">")) => BinOp::Gt,
            Some(Tok::Sym(">=")) => BinOp::Ge,
            Some(Tok::Ident(k)) if k == "in" => BinOp::In,
            Some(Tok::Ident(k)) if k == "not" && matches!(self.toks.get(self.i + 1), Some((Tok::Ident(n), _)) if n == "in") => {
                self.i += 1;
                BinOp::NotIn
            }
            _ => return Ok(l),
        };
        self.i += 1;
        let r = self.add()?;
        Ok(Node::Binary(op, Box::new(l), Box::new(r)))
    }

    fn add(&mut self) -> Result<Node, ExprError> {
        let mut l = self.mul()?;
        loop {
            let op = if self.eat_sym("+") {
                BinOp::Add
            } else if self.eat_sym("-") {
                BinOp::Sub
            } else {
                return Ok(l);
            };
            let r = self.mul()?;
            l = Node::Binary(op, Box::new(l), Box::new(r));
        }
    }

    fn mul(&mut self) -> Result<Node, ExprError> {
        let mut l = self.unary()?;
        loop {
            let op = if self.eat_sym("*") {
                BinOp::Mul
            } else if self.eat_sym("/") {
                BinOp::Div
            } else if self.eat_sym("%") {
                BinOp::Rem
            } else {
                return Ok(l);
            };
            let r = self.unary()?;
            l = Node::Binary(op, Box::new(l), Box::new(r));
        }
    }

    fn unary(&mut self) -> Result<Node, ExprError> {
        if self.eat_sym("-") {
            self.enter()?;
            let n = self.unary()?;
            self.depth -= 1;
            return Ok(Node::Unary(UnOp::Neg, Box::new(n)));
        }
        self.atom()
    }

    fn atom(&mut self) -> Result<Node, ExprError> {
        let Some((tok, _)) = self.toks.get(self.i).cloned() else {
            return self.err("unexpected end of expression");
        };
        self.i += 1;
        match tok {
            Tok::Num(n, is_float) => Ok(Node::Lit(if !is_float && n.fract() == 0.0 && n.abs() < 9e15 { Value::Int(n as i64) } else { Value::Float(n) })),
            Tok::Str(s) => Ok(Node::Lit(Value::Str(s))),
            Tok::Ident(id) => match id.as_str() {
                "true" => Ok(Node::Lit(Value::Bool(true))),
                "false" => Ok(Node::Lit(Value::Bool(false))),
                "null" => Ok(Node::Lit(Value::Null)),
                _ => {
                    if self.eat_sym("(") {
                        let args = self.list_items(")")?;
                        if !FUNCS.contains(&id.as_str()) {
                            return Err(ExprError { msg: format!("unknown function `{id}`"), pos: self.pos() });
                        }
                        Ok(Node::Call(id, args))
                    } else {
                        Ok(Node::Path(id))
                    }
                }
            },
            Tok::Sym("(") => {
                let e = self.expr()?;
                if !self.eat_sym(")") {
                    return self.err("expected `)`");
                }
                Ok(e)
            }
            Tok::Sym("[") => Ok(Node::List(self.list_items("]")?)),
            t => {
                self.i -= 1;
                self.err(&format!("unexpected `{t}`"))
            }
        }
    }

    fn list_items(&mut self, close: &str) -> Result<Vec<Node>, ExprError> {
        let mut items = Vec::new();
        if self.eat_sym(close) {
            return Ok(items);
        }
        loop {
            items.push(self.expr()?);
            if self.eat_sym(close) {
                return Ok(items);
            }
            if !self.eat_sym(",") {
                return self.err(&format!("expected `,` or `{close}`"));
            }
        }
    }
}

const FUNCS: &[&str] = &[
    "min",
    "max",
    "abs",
    "clamp",
    "floor",
    "ceil",
    "round",
    "len",
    "lower",
    "upper",
    "contains",
    "starts_with",
    "ends_with",
    "exists",
    "default",
    "int",
    "float",
    "str",
    "sin",
    "cos",
    "sqrt",
    "lerp",
];

fn num(v: &Value) -> Option<f64> {
    v.as_f64()
}

fn arith(op: BinOp, a: Value, b: Value) -> Value {
    if op == BinOp::Add {
        match (&a, &b) {
            (Value::Str(x), y) => return Value::Str(format!("{x}{y}")),
            (y, Value::Str(x)) => return Value::Str(format!("{y}{x}")),
            _ => {}
        }
    }
    if let (Value::Int(x), Value::Int(y)) = (&a, &b) {
        let (x, y) = (*x, *y);
        return match op {
            BinOp::Add => Value::Int(x.saturating_add(y)),
            BinOp::Sub => Value::Int(x.saturating_sub(y)),
            BinOp::Mul => Value::Int(x.saturating_mul(y)),
            BinOp::Div => {
                if y == 0 {
                    Value::Null
                } else if x % y == 0 {
                    Value::Int(x / y)
                } else {
                    Value::Float(x as f64 / y as f64)
                }
            }
            BinOp::Rem => {
                if y == 0 {
                    Value::Null
                } else {
                    Value::Int(x.rem_euclid(y))
                }
            }
            _ => Value::Null,
        };
    }
    let (Some(x), Some(y)) = (num(&a), num(&b)) else { return Value::Null };
    Value::Float(match op {
        BinOp::Add => x + y,
        BinOp::Sub => x - y,
        BinOp::Mul => x * y,
        BinOp::Div => {
            if y == 0.0 {
                return Value::Null;
            }
            x / y
        }
        BinOp::Rem => {
            if y == 0.0 {
                return Value::Null;
            }
            x.rem_euclid(y)
        }
        _ => return Value::Null,
    })
}

/// Loose equality: numbers compare numerically across int/float.
pub fn loose_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Int(_) | Value::Float(_), Value::Int(_) | Value::Float(_)) => num(a) == num(b),
        _ => a == b,
    }
}

fn cmp(a: &Value, b: &Value) -> Option<std::cmp::Ordering> {
    match (a, b) {
        (Value::Str(x), Value::Str(y)) => Some(x.cmp(y)),
        _ => num(a)?.partial_cmp(&num(b)?),
    }
}

fn contains(hay: &Value, needle: &Value) -> bool {
    match hay {
        Value::List(l) => l.iter().any(|x| loose_eq(x, needle)),
        Value::Str(s) => needle.as_str().is_some_and(|n| s.contains(n)),
        Value::Map(m) => needle.as_str().is_some_and(|n| m.contains_key(n)),
        _ => false,
    }
}

fn eval(n: &Node, s: &dyn Scope) -> Value {
    match n {
        Node::Lit(v) => v.clone(),
        Node::Path(p) => s.lookup(p),
        Node::List(items) => Value::List(items.iter().map(|x| eval(x, s)).collect()),
        Node::Unary(UnOp::Not, a) => Value::Bool(!eval(a, s).truthy()),
        Node::Unary(UnOp::Neg, a) => match eval(a, s) {
            Value::Int(i) => Value::Int(-i),
            v => num(&v).map(|f| Value::Float(-f)).unwrap_or(Value::Null),
        },
        Node::Binary(BinOp::And, a, b) => Value::Bool(eval(a, s).truthy() && eval(b, s).truthy()),
        Node::Binary(BinOp::Or, a, b) => Value::Bool(eval(a, s).truthy() || eval(b, s).truthy()),
        Node::Binary(op, a, b) => {
            let (x, y) = (eval(a, s), eval(b, s));
            match op {
                BinOp::Eq => Value::Bool(loose_eq(&x, &y)),
                BinOp::Ne => Value::Bool(!loose_eq(&x, &y)),
                BinOp::Lt => Value::Bool(cmp(&x, &y) == Some(std::cmp::Ordering::Less)),
                BinOp::Le => Value::Bool(matches!(cmp(&x, &y), Some(std::cmp::Ordering::Less | std::cmp::Ordering::Equal))),
                BinOp::Gt => Value::Bool(cmp(&x, &y) == Some(std::cmp::Ordering::Greater)),
                BinOp::Ge => Value::Bool(matches!(cmp(&x, &y), Some(std::cmp::Ordering::Greater | std::cmp::Ordering::Equal))),
                BinOp::In => Value::Bool(contains(&y, &x)),
                BinOp::NotIn => Value::Bool(!contains(&y, &x)),
                _ => arith(*op, x, y),
            }
        }
        Node::Cond(c, a, b) => {
            if eval(c, s).truthy() {
                eval(a, s)
            } else {
                eval(b, s)
            }
        }
        Node::Call(f, args) => call(f, args, s),
    }
}

fn call(f: &str, args: &[Node], s: &dyn Scope) -> Value {
    let a: Vec<Value> = args.iter().map(|x| eval(x, s)).collect();
    let n = |i: usize| a.get(i).and_then(num);
    let st = |i: usize| a.get(i).map(|v| v.to_string()).unwrap_or_default();
    match f {
        "min" => a.iter().filter_map(num).reduce(f64::min).map(Value::Float).unwrap_or(Value::Null),
        "max" => a.iter().filter_map(num).reduce(f64::max).map(Value::Float).unwrap_or(Value::Null),
        "abs" => n(0).map(|x| Value::Float(x.abs())).unwrap_or(Value::Null),
        "floor" => n(0).map(|x| Value::Int(x.floor() as i64)).unwrap_or(Value::Null),
        "ceil" => n(0).map(|x| Value::Int(x.ceil() as i64)).unwrap_or(Value::Null),
        "round" => n(0).map(|x| Value::Int(x.round() as i64)).unwrap_or(Value::Null),
        "sqrt" => n(0).map(|x| Value::Float(x.max(0.0).sqrt())).unwrap_or(Value::Null),
        "sin" => n(0).map(|x| Value::Float(x.sin())).unwrap_or(Value::Null),
        "cos" => n(0).map(|x| Value::Float(x.cos())).unwrap_or(Value::Null),
        "clamp" => match (n(0), n(1), n(2)) {
            (Some(x), Some(lo), Some(hi)) if lo <= hi => Value::Float(x.clamp(lo, hi)),
            _ => Value::Null,
        },
        "lerp" => match (n(0), n(1), n(2)) {
            (Some(x), Some(y), Some(t)) => Value::Float(x + (y - x) * t),
            _ => Value::Null,
        },
        "len" => match a.first() {
            Some(Value::Str(x)) => Value::Int(x.chars().count() as i64),
            Some(Value::List(l)) => Value::Int(l.len() as i64),
            Some(Value::Map(m)) => Value::Int(m.len() as i64),
            _ => Value::Int(0),
        },
        "lower" => Value::Str(st(0).to_lowercase()),
        "upper" => Value::Str(st(0).to_uppercase()),
        "contains" => Value::Bool(a.len() == 2 && contains(&a[0], &a[1])),
        "starts_with" => Value::Bool(st(0).starts_with(&st(1))),
        "ends_with" => Value::Bool(st(0).ends_with(&st(1))),
        "exists" => Value::Bool(a.first().is_some_and(|v| !v.is_null())),
        "default" => match a.first() {
            Some(v) if !v.is_null() => v.clone(),
            _ => a.get(1).cloned().unwrap_or(Value::Null),
        },
        "int" => n(0).map(|x| Value::Int(x as i64)).unwrap_or(Value::Null),
        "float" => n(0).map(Value::Float).unwrap_or(Value::Null),
        "str" => Value::Str(st(0)),
        _ => Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(p: &str) -> Value {
        match p {
            "event.tier" => Value::Int(2),
            "event.bits" => Value::Int(1000),
            "event.message" => Value::Str("hello World".into()),
            "mode" => Value::Str("live".into()),
            "scene" => Value::Str("duo".into()),
            "music.bass" => Value::Float(0.4),
            "mixer.16r.ch.3.fader" => Value::Float(0.75),
            _ => Value::Null,
        }
    }

    fn ev(s: &str) -> Value {
        Expr::parse(s).unwrap_or_else(|e| panic!("{s}: {e}")).eval(&scope)
    }

    #[test]
    fn plan_examples() {
        assert_eq!(ev("event.tier >= 2 && mode == 'live'"), Value::Bool(true));
        assert_eq!(ev("event.tier >= 3 && mode == 'live'"), Value::Bool(false));
        assert_eq!(ev("scene in [duo, wide]"), Value::Bool(false)); // bare idents resolve as paths → null
        assert_eq!(ev("scene in ['duo', 'wide']"), Value::Bool(true));
        assert_eq!(ev("mode == 'chill'"), Value::Bool(false));
        assert_eq!(ev("not queue.playing"), Value::Bool(true));
        assert_eq!(ev("scene not in ['wide']"), Value::Bool(true));
    }

    #[test]
    fn arithmetic_and_calls() {
        assert_eq!(ev("event.bits / 100"), Value::Int(10));
        assert_eq!(ev("event.bits / 3 > 333"), Value::Bool(true));
        assert_eq!(ev("clamp(music.bass * 4, 0, 1)"), Value::Float(1.0));
        assert_eq!(ev("mixer.16r.ch.3.fader > 0.5"), Value::Bool(true));
        assert_eq!(ev("-2 + 5 * 2"), Value::Int(8));
        assert_eq!(ev("contains(lower(event.message), 'world')"), Value::Bool(true));
        assert_eq!(ev("event.tier == 2 ? 'big' : 'small'"), Value::Str("big".into()));
        assert_eq!(ev("1.5 == 1.5"), Value::Bool(true));
        assert_eq!(ev("2 == 2.0"), Value::Bool(true));
        assert_eq!(ev("10 / 0"), Value::Null);
        assert_eq!(ev("'a' + 1"), Value::Str("a1".into()));
        assert_eq!(ev("default(event.nope, 5)"), Value::Int(5));
    }

    #[test]
    fn errors_and_limits() {
        assert!(Expr::parse("a ==").is_err());
        assert!(Expr::parse("system('rm')").is_err());
        assert!(Expr::parse("'open").is_err());
        let deep = "(".repeat(200) + "1" + &")".repeat(200);
        assert!(Expr::parse(&deep).is_err());
        let e = Expr::parse("event.tier >= 2 && music.bass > 0").unwrap();
        assert_eq!(e.paths(), vec!["event.tier".to_string(), "music.bass".to_string()]);
    }
}
