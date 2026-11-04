//! Evaluate a parsed CEL expression against a context.
//!
//! The CEL AST (`paneld_config::cel::Expr`) is produced at config load
//! time; this module turns it into a [`Value`] by walking the tree.
//!
//! Scope today: enough to make `text { value = {{ ... }} }` render in
//! real pixels. Supports:
//!
//! - Literals: int, double, string, bool, null.
//! - Identifiers from the context (`sources`, params, `item`, ...).
//! - Field access (`sources.cpu_temp.value`), index (`list[0]`).
//! - Binary arithmetic and comparison, logical `&&`/`||`, `in`.
//! - Unary `-` and `!`.
//! - Ternary `a ? b : c`.
//! - A handful of builtin functions: `string`, `int`, `number`,
//!   `round`, `len`, `has`, `ago`.
//!
//! - Map literals with string keys.
//!
//! What is deferred: method calls on complex types, overload
//! resolution, UTF-8 aware string functions, duration / timestamp
//! arithmetic.

use std::collections::BTreeMap;

use paneld_config::cel::{BinaryOp, Expr, ExprKind, UnaryOp};

/// A runtime value produced by evaluating an [`Expr`].
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Double(f64),
    String(String),
    List(Vec<Value>),
    Map(BTreeMap<String, Value>),
}

impl Value {
    /// Render as the display string that would appear in a `text`
    /// node's `value`. Mirrors CEL's string coercion for the simple
    /// cases.
    pub fn to_display_string(&self) -> String {
        match self {
            Value::Null => "null".to_string(),
            Value::Bool(b) => b.to_string(),
            Value::Int(n) => n.to_string(),
            Value::Double(d) => {
                if d.fract() == 0.0 && d.is_finite() && d.abs() < 1e15 {
                    format!("{}", *d as i64)
                } else {
                    format!("{d}")
                }
            }
            Value::String(s) => s.clone(),
            Value::List(items) => {
                let inner = items
                    .iter()
                    .map(Value::to_display_string)
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("[{inner}]")
            }
            Value::Map(_) => "<map>".to_string(),
        }
    }
}

/// Context handed to [`eval`]: a flat map of bindings reachable as
/// bare identifiers (`sources`, `theme`, `now`, component params,
/// `item`, ...). Field access climbs through [`Value::Map`].
pub type Context = BTreeMap<String, Value>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvalError {
    pub message: String,
}

impl EvalError {
    fn new(msg: impl Into<String>) -> Self {
        Self { message: msg.into() }
    }
}

impl std::fmt::Display for EvalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

pub fn eval(expr: &Expr, ctx: &Context) -> Result<Value, EvalError> {
    match &expr.kind {
        ExprKind::IntLit(n) => Ok(Value::Int(*n)),
        ExprKind::DoubleLit(d) => Ok(Value::Double(*d)),
        ExprKind::StringLit(s) => Ok(Value::String(s.clone())),
        ExprKind::BoolLit(b) => Ok(Value::Bool(*b)),
        ExprKind::Null => Ok(Value::Null),
        ExprKind::Ident(name) => ctx
            .get(name)
            .cloned()
            .ok_or_else(|| EvalError::new(format!("unknown identifier `{name}`"))),
        ExprKind::Field(inner, name) => {
            let base = eval(inner, ctx)?;
            match base {
                Value::Map(m) => m.get(name).cloned().ok_or_else(|| {
                    EvalError::new(format!("field `{name}` not set on map"))
                }),
                other => Err(EvalError::new(format!(
                    "`.{name}` on a {} is not supported",
                    type_name(&other)
                ))),
            }
        }
        ExprKind::Index(inner, idx) => {
            let base = eval(inner, ctx)?;
            let i = eval(idx, ctx)?;
            match (&base, &i) {
                (Value::List(items), Value::Int(n)) => {
                    let n = *n;
                    if n < 0 || (n as usize) >= items.len() {
                        Err(EvalError::new(format!(
                            "list index {n} out of range (len {})",
                            items.len()
                        )))
                    } else {
                        Ok(items[n as usize].clone())
                    }
                }
                (Value::Map(m), Value::String(k)) => m
                    .get(k)
                    .cloned()
                    .ok_or_else(|| EvalError::new(format!("map has no key `{k}`"))),
                _ => Err(EvalError::new(format!(
                    "cannot index {} with {}",
                    type_name(&base),
                    type_name(&i)
                ))),
            }
        }
        ExprKind::Call(name, args) => call_builtin(name, args, ctx),
        ExprKind::Method(inner, name, args) => {
            // No method lookup yet; evaluate the receiver so a user
            // error (e.g. a missing field in the receiver) still
            // surfaces at this call site.
            let _ = eval(inner, ctx)?;
            for a in args {
                let _ = eval(a, ctx)?;
            }
            Err(EvalError::new(format!(
                "method `.{name}(...)` is not supported yet"
            )))
        }
        ExprKind::List(items) => {
            let mut vals = Vec::with_capacity(items.len());
            for i in items {
                vals.push(eval(i, ctx)?);
            }
            Ok(Value::List(vals))
        }
        ExprKind::Map(pairs) => {
            let mut m = BTreeMap::new();
            for (k, v) in pairs {
                let Value::String(key) = eval(k, ctx)? else {
                    return Err(EvalError::new("map keys must be strings"));
                };
                m.insert(key, eval(v, ctx)?);
            }
            Ok(Value::Map(m))
        }
        ExprKind::Unary(op, inner) => {
            let v = eval(inner, ctx)?;
            match (op, v) {
                (UnaryOp::Neg, Value::Int(n)) => Ok(Value::Int(-n)),
                (UnaryOp::Neg, Value::Double(d)) => Ok(Value::Double(-d)),
                (UnaryOp::Not, Value::Bool(b)) => Ok(Value::Bool(!b)),
                (op, v) => Err(EvalError::new(format!(
                    "cannot apply {:?} to {}",
                    op,
                    type_name(&v)
                ))),
            }
        }
        ExprKind::Binary(op, l, r) => {
            // Short-circuit `&&` / `||` so a bad right-hand side
            // doesn't error when the left already decides the result.
            if matches!(op, BinaryOp::And | BinaryOp::Or) {
                let lv = eval(l, ctx)?;
                let lb = to_bool(&lv)?;
                match op {
                    BinaryOp::And => {
                        if !lb {
                            return Ok(Value::Bool(false));
                        }
                    }
                    BinaryOp::Or => {
                        if lb {
                            return Ok(Value::Bool(true));
                        }
                    }
                    _ => unreachable!(),
                }
                let rv = eval(r, ctx)?;
                let rb = to_bool(&rv)?;
                return Ok(Value::Bool(rb));
            }
            let lv = eval(l, ctx)?;
            let rv = eval(r, ctx)?;
            apply_binary(*op, lv, rv)
        }
        ExprKind::Conditional(cond, then, els) => {
            let c = eval(cond, ctx)?;
            if to_bool(&c)? {
                eval(then, ctx)
            } else {
                eval(els, ctx)
            }
        }
    }
}

fn apply_binary(op: BinaryOp, l: Value, r: Value) -> Result<Value, EvalError> {
    use BinaryOp::*;
    use Value::*;
    match (op, l, r) {
        // --- arithmetic ---
        (Add, Int(a), Int(b)) => Ok(Int(a.wrapping_add(b))),
        (Sub, Int(a), Int(b)) => Ok(Int(a.wrapping_sub(b))),
        (Mul, Int(a), Int(b)) => Ok(Int(a.wrapping_mul(b))),
        (Div, Int(_), Int(0)) => Err(EvalError::new("integer division by zero")),
        (Div, Int(a), Int(b)) => Ok(Int(a / b)),
        (Mod, Int(_), Int(0)) => Err(EvalError::new("integer modulo by zero")),
        (Mod, Int(a), Int(b)) => Ok(Int(a % b)),

        (Add, Double(a), Double(b)) => Ok(Double(a + b)),
        (Sub, Double(a), Double(b)) => Ok(Double(a - b)),
        (Mul, Double(a), Double(b)) => Ok(Double(a * b)),
        (Div, Double(a), Double(b)) => Ok(Double(a / b)),
        (Mod, Double(a), Double(b)) => Ok(Double(a % b)),

        // Mixed int/double: promote to double.
        (Add, Int(a), Double(b)) => Ok(Double(a as f64 + b)),
        (Add, Double(a), Int(b)) => Ok(Double(a + b as f64)),
        (Sub, Int(a), Double(b)) => Ok(Double(a as f64 - b)),
        (Sub, Double(a), Int(b)) => Ok(Double(a - b as f64)),
        (Mul, Int(a), Double(b)) => Ok(Double(a as f64 * b)),
        (Mul, Double(a), Int(b)) => Ok(Double(a * b as f64)),
        (Div, Int(a), Double(b)) => Ok(Double(a as f64 / b)),
        (Div, Double(a), Int(b)) => Ok(Double(a / b as f64)),

        // --- string concat ---
        (Add, String(a), String(b)) => Ok(String(a + &b)),

        // --- comparisons ---
        (Eq, a, b) => Ok(Bool(value_eq(&a, &b))),
        (Ne, a, b) => Ok(Bool(!value_eq(&a, &b))),
        (Lt, a, b) => cmp(&a, &b).map(|o| Bool(o == std::cmp::Ordering::Less)),
        (Le, a, b) => cmp(&a, &b).map(|o| Bool(o != std::cmp::Ordering::Greater)),
        (Gt, a, b) => cmp(&a, &b).map(|o| Bool(o == std::cmp::Ordering::Greater)),
        (Ge, a, b) => cmp(&a, &b).map(|o| Bool(o != std::cmp::Ordering::Less)),

        (In, needle, List(items)) => Ok(Bool(items.iter().any(|v| value_eq(v, &needle)))),
        (In, String(s), String(hay)) => Ok(Bool(hay.contains(&s))),
        // CEL: `k in map` tests the keys.
        (In, String(k), Map(m)) => Ok(Bool(m.contains_key(&k))),
        (In, _, Map(_)) => Ok(Bool(false)),

        (op, l, r) => Err(EvalError::new(format!(
            "operator {op:?} does not accept {} and {}",
            type_name(&l),
            type_name(&r)
        ))),
    }
}

fn value_eq(a: &Value, b: &Value) -> bool {
    use Value::*;
    match (a, b) {
        (Int(x), Int(y)) => x == y,
        (Double(x), Double(y)) => x == y,
        (Int(x), Double(y)) | (Double(y), Int(x)) => (*x as f64) == *y,
        (Bool(x), Bool(y)) => x == y,
        (String(x), String(y)) => x == y,
        (Null, Null) => true,
        (List(xs), List(ys)) => {
            xs.len() == ys.len() && xs.iter().zip(ys).all(|(x, y)| value_eq(x, y))
        }
        _ => false,
    }
}

fn cmp(a: &Value, b: &Value) -> Result<std::cmp::Ordering, EvalError> {
    use Value::*;
    match (a, b) {
        (Int(x), Int(y)) => Ok(x.cmp(y)),
        (Double(x), Double(y)) => {
            x.partial_cmp(y).ok_or_else(|| EvalError::new("NaN in comparison"))
        }
        (Int(x), Double(y)) => (*x as f64)
            .partial_cmp(y)
            .ok_or_else(|| EvalError::new("NaN in comparison")),
        (Double(x), Int(y)) => x
            .partial_cmp(&(*y as f64))
            .ok_or_else(|| EvalError::new("NaN in comparison")),
        (String(x), String(y)) => Ok(x.cmp(y)),
        (Bool(x), Bool(y)) => Ok(x.cmp(y)),
        _ => Err(EvalError::new(format!(
            "cannot compare {} with {}",
            type_name(a),
            type_name(b)
        ))),
    }
}

fn to_bool(v: &Value) -> Result<bool, EvalError> {
    match v {
        Value::Bool(b) => Ok(*b),
        other => Err(EvalError::new(format!(
            "expected a bool, got {}",
            type_name(other)
        ))),
    }
}

fn call_builtin(name: &str, args: &[Expr], ctx: &Context) -> Result<Value, EvalError> {
    let argc = args.len();
    match name {
        "string" if argc == 1 => Ok(Value::String(eval(&args[0], ctx)?.to_display_string())),
        "int" if argc == 1 => match eval(&args[0], ctx)? {
            Value::Int(n) => Ok(Value::Int(n)),
            Value::Double(d) => Ok(Value::Int(d as i64)),
            Value::String(s) => s
                .trim()
                .parse::<i64>()
                .map(Value::Int)
                .map_err(|_| EvalError::new(format!("cannot parse `{s}` as int"))),
            Value::Bool(b) => Ok(Value::Int(if b { 1 } else { 0 })),
            other => Err(EvalError::new(format!(
                "int({}) is not supported",
                type_name(&other)
            ))),
        },
        "number" if argc == 1 => match eval(&args[0], ctx)? {
            Value::Int(n) => Ok(Value::Double(n as f64)),
            Value::Double(d) => Ok(Value::Double(d)),
            Value::String(s) => s
                .trim()
                .parse::<f64>()
                .map(Value::Double)
                .map_err(|_| EvalError::new(format!("cannot parse `{s}` as number"))),
            other => Err(EvalError::new(format!(
                "number({}) is not supported",
                type_name(&other)
            ))),
        },
        "round" if argc == 1 => match eval(&args[0], ctx)? {
            Value::Double(d) => Ok(Value::Double(d.round())),
            Value::Int(n) => Ok(Value::Int(n)),
            other => Err(EvalError::new(format!(
                "round({}) is not supported",
                type_name(&other)
            ))),
        },
        "round" if argc == 2 => match (eval(&args[0], ctx)?, eval(&args[1], ctx)?) {
            (Value::Double(d), Value::Int(p)) => {
                let scale = 10f64.powi(p as i32);
                Ok(Value::Double((d * scale).round() / scale))
            }
            (Value::Int(n), Value::Int(_)) => Ok(Value::Int(n)),
            (a, b) => Err(EvalError::new(format!(
                "round({}, {}) is not supported",
                type_name(&a),
                type_name(&b)
            ))),
        },
        "len" if argc == 1 => match eval(&args[0], ctx)? {
            Value::String(s) => Ok(Value::Int(s.chars().count() as i64)),
            Value::List(items) => Ok(Value::Int(items.len() as i64)),
            Value::Map(m) => Ok(Value::Int(m.len() as i64)),
            other => Err(EvalError::new(format!(
                "len({}) is not supported",
                type_name(&other)
            ))),
        },
        // Schema 16: `ago(t)`, Unix seconds as "5m ago" for display.
        "ago" if argc == 1 => {
            let t = match eval(&args[0], ctx)? {
                Value::Int(n) => n,
                Value::Double(d) => d as i64,
                other => return Err(EvalError::new(format!("ago({}) is not supported", type_name(&other)))),
            };
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs() as i64);
            Ok(Value::String(ago_text(now.saturating_sub(t))))
        }
        "has" if argc == 1 => {
            // `has(a.b)` in CEL tests field presence without erroring
            // on absence. Our Field eval returns Err on missing, so
            // we trap that here.
            match eval(&args[0], ctx) {
                Ok(_) => Ok(Value::Bool(true)),
                Err(_) => Ok(Value::Bool(false)),
            }
        }
        _ => Err(EvalError::new(format!(
            "unknown function `{name}` with {argc} args"
        ))),
    }
}

/// `secs` ago in the largest whole unit: `just now`, `5m ago`,
/// `3h ago`, `2d ago`.
fn ago_text(secs: i64) -> String {
    match secs {
        s if s < 60 => "just now".to_string(),
        s if s < 3_600 => format!("{}m ago", s / 60),
        s if s < 86_400 => format!("{}h ago", s / 3_600),
        s => format!("{}d ago", s / 86_400),
    }
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Int(_) => "int",
        Value::Double(_) => "number",
        Value::String(_) => "string",
        Value::List(_) => "list",
        Value::Map(_) => "map",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use paneld_config::cel;
    use paneld_config::source::{FileId, SourceMap};
    use std::path::PathBuf;

    #[test]
    fn ago_picks_the_largest_unit() {
        assert_eq!(ago_text(-5), "just now");
        assert_eq!(ago_text(59), "just now");
        assert_eq!(ago_text(61), "1m ago");
        assert_eq!(ago_text(3 * 3_600 + 5), "3h ago");
        assert_eq!(ago_text(2 * 86_400), "2d ago");
    }

    fn fid() -> FileId {
        let mut map = SourceMap::new();
        map.add(PathBuf::from("t"), String::new())
    }

    fn parse(s: &str) -> Expr {
        let (e, d) = cel::parse_fragment(s, fid(), 0);
        assert!(d.is_empty(), "parse errors: {d:?}");
        e.expect("parsed")
    }

    fn ev(s: &str) -> Value {
        let ctx = Context::new();
        eval(&parse(s), &ctx).expect("eval")
    }

    fn ev_ctx(s: &str, ctx: &Context) -> Value {
        eval(&parse(s), ctx).expect("eval")
    }

    #[test]
    fn arith() {
        assert_eq!(ev("1 + 2 * 3"), Value::Int(7));
        assert_eq!(ev("10 / 3"), Value::Int(3));
        assert_eq!(ev("10.0 / 4.0"), Value::Double(2.5));
        assert_eq!(ev("1 + 2.5"), Value::Double(3.5));
    }

    #[test]
    fn in_tests_list_items_and_map_keys() {
        assert_eq!(ev("2 in [1, 2]"), Value::Bool(true));
        assert_eq!(ev("'x' in {'x': 1}"), Value::Bool(true));
        assert_eq!(ev("'y' in {'x': 1}"), Value::Bool(false));
        assert_eq!(ev("1 in {'x': 1}"), Value::Bool(false));
    }

    #[test]
    fn string_concat() {
        assert_eq!(ev(r#""hello " + "world""#), Value::String("hello world".into()));
    }

    #[test]
    fn ternary() {
        assert_eq!(ev("1 < 2 ? \"a\" : \"b\""), Value::String("a".into()));
        assert_eq!(ev("1 > 2 ? \"a\" : \"b\""), Value::String("b".into()));
    }

    #[test]
    fn builtins() {
        assert_eq!(ev("string(42)"), Value::String("42".into()));
        assert_eq!(ev("int(\"7\")"), Value::Int(7));
        assert_eq!(ev("number(\"2.5\")"), Value::Double(2.5));
        assert_eq!(ev("round(1.6)"), Value::Double(2.0));
        assert_eq!(ev("round(1.2345, 2)"), Value::Double(1.23));
        assert_eq!(ev("len(\"abc\")"), Value::Int(3));
        assert_eq!(ev("len([1, 2, 3])"), Value::Int(3));
    }

    #[test]
    fn field_access_on_map() {
        let mut ctx = Context::new();
        let mut inner: BTreeMap<String, Value> = BTreeMap::new();
        inner.insert("value".into(), Value::Double(42.5));
        let mut sources: BTreeMap<String, Value> = BTreeMap::new();
        sources.insert("cpu_temp".into(), Value::Map(inner));
        ctx.insert("sources".into(), Value::Map(sources));
        assert_eq!(
            ev_ctx("sources.cpu_temp.value", &ctx),
            Value::Double(42.5)
        );
        assert_eq!(
            ev_ctx("sources.cpu_temp.value > 40", &ctx),
            Value::Bool(true)
        );
    }

    #[test]
    fn short_circuit_and_or() {
        // Right side would error (unknown ident), but short-circuit
        // keeps the eval green.
        let ctx = Context::new();
        assert_eq!(eval(&parse("true || missing"), &ctx), Ok(Value::Bool(true)));
        assert_eq!(
            eval(&parse("false && missing"), &ctx),
            Ok(Value::Bool(false))
        );
    }

    #[test]
    fn has_traps_missing() {
        let ctx = Context::new();
        assert_eq!(eval(&parse("has(missing)"), &ctx), Ok(Value::Bool(false)));
    }

    #[test]
    fn index_list_and_map() {
        let mut ctx = Context::new();
        ctx.insert(
            "xs".into(),
            Value::List(vec![Value::Int(10), Value::Int(20), Value::Int(30)]),
        );
        assert_eq!(ev_ctx("xs[1]", &ctx), Value::Int(20));

        let mut m: BTreeMap<String, Value> = BTreeMap::new();
        m.insert("a".into(), Value::Int(1));
        ctx.insert("m".into(), Value::Map(m));
        assert_eq!(ev_ctx(r#"m["a"]"#, &ctx), Value::Int(1));
    }
}
