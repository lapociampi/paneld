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
        ExprKind::Method(inner, name, args) => call_method(eval(inner, ctx)?, name, args, ctx),
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
        // Schema 16: display helpers.
        "format" if argc == 2 => {
            let x = number_arg(&eval(&args[0], ctx)?, "format")?;
            let Value::String(unit) = eval(&args[1], ctx)? else {
                return Err(EvalError::new("format(x, unit) needs a text unit"));
            };
            Ok(Value::String(format_unit(x, &unit)))
        }
        "duration" if argc == 1 => {
            let secs = number_arg(&eval(&args[0], ctx)?, "duration")?;
            Ok(Value::String(duration_text(secs)))
        }
        "clamp" if argc == 3 => {
            let x = eval(&args[0], ctx)?;
            let (lo, hi) = (eval(&args[1], ctx)?, eval(&args[2], ctx)?);
            match (&x, &lo, &hi) {
                (Value::Int(x), Value::Int(lo), Value::Int(hi)) if lo <= hi => Ok(Value::Int(*x.max(lo).min(hi))),
                _ => {
                    let (x, lo, hi) = (number_arg(&x, "clamp")?, number_arg(&lo, "clamp")?, number_arg(&hi, "clamp")?);
                    if lo > hi || lo.is_nan() || hi.is_nan() {
                        return Err(EvalError::new("clamp(x, a, b) needs a <= b"));
                    }
                    Ok(Value::Double(x.clamp(lo, hi)))
                }
            }
        }
        "color" if argc == 1 => {
            let (rgb, alpha) = color_arg(&eval(&args[0], ctx)?, ctx)?;
            Ok(Value::String(hex_text(rgb, alpha)))
        }
        "mix" if argc == 3 => {
            let (a, _) = color_arg(&eval(&args[0], ctx)?, ctx)?;
            let (b, _) = color_arg(&eval(&args[1], ctx)?, ctx)?;
            let t = number_arg(&eval(&args[2], ctx)?, "mix")?;
            let t = if t.is_nan() { 0.0 } else { t.clamp(0.0, 1.0) };
            let blend = |shift: u32| {
                let (x, y) = (((a >> shift) & 0xff) as f64, ((b >> shift) & 0xff) as f64);
                ((x + (y - x) * t).round() as u32) << shift
            };
            Ok(Value::String(hex_text(blend(16) | blend(8) | blend(0), None)))
        }
        "double" if argc == 1 => call_builtin("number", args, ctx),
        "size" if argc == 1 => call_builtin("len", args, ctx),
        // There is no timestamp type: Unix seconds as the RFC 3339
        // text CEL prints for one, in UTC.
        "timestamp" if argc == 1 => match eval(&args[0], ctx)? {
            Value::Int(n) => Ok(Value::String(rfc3339(n))),
            Value::Double(d) if d.is_finite() => Ok(Value::String(rfc3339(d as i64))),
            Value::String(s) => Ok(Value::String(s)),
            other => Err(EvalError::new(format!("timestamp({}) is not supported", type_name(&other)))),
        },
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

pub use paneld_config::cel::FUNCTIONS;

/// `receiver.name(args)`: the CEL macros (`all`, `exists`,
/// `exists_one`, `map`, `filter`) over a list or a map's keys, and the
/// string and list methods.
fn call_method(receiver: Value, name: &str, args: &[Expr], ctx: &Context) -> Result<Value, EvalError> {
    if matches!(name, "all" | "exists" | "exists_one" | "map" | "filter") {
        let items = match receiver {
            Value::List(l) => l,
            Value::Map(m) => m.into_keys().map(Value::String).collect(),
            other => return Err(EvalError::new(format!(".{name}() on a {} is not supported", type_name(&other)))),
        };
        let (var, rest) = match args.split_first() {
            Some((Expr { kind: ExprKind::Ident(v), .. }, rest)) if !rest.is_empty() && rest.len() <= 2 => (v, rest),
            _ => return Err(EvalError::new(format!(".{name}() takes a name and an expression"))),
        };
        let mut scope = ctx.clone();
        let mut with = |item: &Value, e: &Expr| {
            scope.insert(var.clone(), item.clone());
            eval(e, &scope)
        };
        let pred = |v: Value| to_bool(&v);
        return match (name, rest) {
            ("all", [p]) => {
                for i in &items {
                    if !pred(with(i, p)?)? {
                        return Ok(Value::Bool(false));
                    }
                }
                Ok(Value::Bool(true))
            }
            ("exists", [p]) => {
                for i in &items {
                    if pred(with(i, p)?)? {
                        return Ok(Value::Bool(true));
                    }
                }
                Ok(Value::Bool(false))
            }
            ("exists_one", [p]) => {
                let mut n = 0;
                for i in &items {
                    if pred(with(i, p)?)? {
                        n += 1;
                    }
                }
                Ok(Value::Bool(n == 1))
            }
            ("filter", [p]) => {
                let mut out = Vec::new();
                for i in items {
                    if pred(with(&i, p)?)? {
                        out.push(i);
                    }
                }
                Ok(Value::List(out))
            }
            ("map", [e]) => items.iter().map(|i| with(i, e)).collect::<Result<_, _>>().map(Value::List),
            ("map", [p, e]) => {
                let mut out = Vec::new();
                for i in &items {
                    if pred(with(i, p)?)? {
                        out.push(with(i, e)?);
                    }
                }
                Ok(Value::List(out))
            }
            _ => Err(EvalError::new(format!(".{name}() takes a name and one expression"))),
        };
    }
    let args = args.iter().map(|a| eval(a, ctx)).collect::<Result<Vec<_>, _>>()?;
    let text = |v: &Value| match v {
        Value::String(s) => Ok(s.clone()),
        other => Err(EvalError::new(format!(".{name}() needs text, got {}", type_name(other)))),
    };
    match (name, &receiver, args.as_slice()) {
        ("size", Value::String(s), []) => Ok(Value::Int(s.chars().count() as i64)),
        ("size", Value::List(l), []) => Ok(Value::Int(l.len() as i64)),
        ("size", Value::Map(m), []) => Ok(Value::Int(m.len() as i64)),
        ("contains", Value::String(s), [x]) => Ok(Value::Bool(s.contains(&text(x)?))),
        ("startsWith", Value::String(s), [x]) => Ok(Value::Bool(s.starts_with(&text(x)?))),
        ("endsWith", Value::String(s), [x]) => Ok(Value::Bool(s.ends_with(&text(x)?))),
        ("lowerAscii", Value::String(s), []) => Ok(Value::String(s.to_ascii_lowercase())),
        ("upperAscii", Value::String(s), []) => Ok(Value::String(s.to_ascii_uppercase())),
        ("trim", Value::String(s), []) => Ok(Value::String(s.trim().to_string())),
        ("split", Value::String(s), [sep]) => {
            Ok(Value::List(s.split(text(sep)?.as_str()).map(|p| Value::String(p.to_string())).collect()))
        }
        ("join", Value::List(l), []) | ("join", Value::List(l), [_]) => {
            let sep = match args.first() {
                Some(x) => text(x)?,
                None => String::new(),
            };
            let parts = l.iter().map(text).collect::<Result<Vec<_>, _>>()?;
            Ok(Value::String(parts.join(&sep)))
        }
        _ => Err(EvalError::new(format!(
            "`.{name}()` with {} argument(s) is not supported on a {}",
            args.len(),
            type_name(&receiver)
        ))),
    }
}

/// Unix seconds as `2026-11-04T18:30:00Z`.
fn rfc3339(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Civil date from days since 1970-01-01 (Howard Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z", rem / 3_600, rem % 3_600 / 60, rem % 60)
}

fn number_arg(v: &Value, f: &str) -> Result<f64, EvalError> {
    match v {
        Value::Int(n) => Ok(*n as f64),
        Value::Double(d) => Ok(*d),
        other => Err(EvalError::new(format!("{f}() needs a number, got {}", type_name(other)))),
    }
}

/// A number with at most one decimal, none when it is whole.
fn short_number(x: f64) -> String {
    let r = (x * 10.0).round() / 10.0;
    if r.fract() == 0.0 && r.abs() < 1e15 {
        format!("{}", r as i64)
    } else {
        format!("{r:.1}")
    }
}

/// `format(x, unit)`: `bytes` in steps of 1024 (`1.5 KiB`), `si` in
/// steps of 1000 (`2.3 M`), `%` as a percentage, any other unit put
/// after the number (`21.5 C`).
fn format_unit(x: f64, unit: &str) -> String {
    let scaled = |step: f64, names: &[&str]| {
        let mut v = x;
        let mut i = 0;
        while v.abs() >= step && i + 1 < names.len() {
            v /= step;
            i += 1;
        }
        let n = short_number(v);
        if names[i].is_empty() { n } else { format!("{n} {}", names[i]) }
    };
    match unit {
        "bytes" => scaled(1024.0, &["B", "KiB", "MiB", "GiB", "TiB", "PiB"]),
        "si" => scaled(1000.0, &["", "k", "M", "G", "T", "P"]),
        "%" => format!("{}%", short_number(x)),
        "" => short_number(x),
        u => format!("{} {u}", short_number(x)),
    }
}

/// Seconds as the two largest units: `45s`, `5m 3s`, `1h 5m`, `3d 4h`.
fn duration_text(secs: f64) -> String {
    let sign = if secs < 0.0 { "-" } else { "" };
    let s = secs.abs().round() as u64;
    let units = [("d", 86_400), ("h", 3_600), ("m", 60), ("s", 1)];
    // The largest unit that fits, then the next one if it is not 0.
    let i = units.iter().position(|(_, size)| s >= *size).unwrap_or(3);
    let (name, size) = units[i];
    let mut out = format!("{sign}{}{name}", s / size);
    if let Some((next, next_size)) = units.get(i + 1) {
        let rest = (s % size) / next_size;
        if rest > 0 {
            out.push_str(&format!(" {rest}{next}"));
        }
    }
    out
}

/// A color value: `#rgb`, `#rrggbb`, `#rrggbbaa`, or the name of a
/// theme color token. Its RGB and alpha.
fn color_arg(v: &Value, ctx: &Context) -> Result<(u32, Option<u8>), EvalError> {
    let Value::String(s) = v else {
        return Err(EvalError::new(format!("a color is text, got {}", type_name(v))));
    };
    let text = s.trim();
    let hex = if text.starts_with('#') {
        text.to_string()
    } else {
        match ctx.get("theme") {
            Some(Value::Map(t)) => match t.get(text) {
                Some(Value::String(h)) if h.trim().starts_with('#') => h.trim().to_string(),
                _ => return Err(EvalError::new(format!("`{text}` is not a theme color"))),
            },
            _ => return Err(EvalError::new(format!("`{text}` is not a color"))),
        }
    };
    let digits = &hex[1..];
    let byte = |i: usize| u8::from_str_radix(&digits[i..i + 2], 16).ok();
    let bad = || EvalError::new(format!("`{hex}` is not a color"));
    if !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(bad());
    }
    match digits.len() {
        3 => {
            let d: Vec<u32> = digits.chars().map(|c| c.to_digit(16).unwrap() * 17).collect();
            Ok(((d[0] << 16) | (d[1] << 8) | d[2], None))
        }
        6 | 8 => {
            let rgb = ((byte(0).ok_or_else(bad)? as u32) << 16) | ((byte(2).ok_or_else(bad)? as u32) << 8) | byte(4).ok_or_else(bad)? as u32;
            Ok((rgb, if digits.len() == 8 { byte(6) } else { None }))
        }
        _ => Err(bad()),
    }
}

fn hex_text(rgb: u32, alpha: Option<u8>) -> String {
    match alpha {
        Some(a) => format!("#{rgb:06x}{a:02x}"),
        None => format!("#{rgb:06x}"),
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

/// Wider coverage of the evaluator: every operator pairing, every
/// built-in, and the error each misuse gives (a page shows that text).
#[cfg(test)]
mod more_tests {
    use super::*;
    use paneld_config::cel;
    use paneld_config::source::FileId;

    fn try_ev(s: &str, ctx: &Context) -> Result<Value, EvalError> {
        let (e, d) = cel::parse_fragment(s, FileId::default(), 0);
        assert!(d.is_empty(), "parse errors in `{s}`: {d:?}");
        eval(&e.expect("parsed"), ctx)
    }

    fn ev(s: &str) -> Value {
        try_ev(s, &Context::new()).unwrap_or_else(|e| panic!("`{s}`: {e}"))
    }

    fn err(s: &str) -> String {
        match try_ev(s, &sample_ctx()) {
            Ok(v) => panic!("`{s}` gave {v:?}, expected an error"),
            Err(e) => e.message,
        }
    }

    fn sample_ctx() -> Context {
        let mut ctx = Context::new();
        ctx.insert("m".into(), Value::Map(BTreeMap::from([("a".to_string(), Value::Int(1))])));
        ctx.insert("l".into(), Value::List(vec![Value::Int(10), Value::Int(20)]));
        ctx.insert("s".into(), Value::String("text".into()));
        ctx
    }

    use Value::{Bool as B, Double as D, Int as I, String as S};

    #[test]
    fn integer_arithmetic_wraps_and_refuses_zero_divisors() {
        assert_eq!(ev("7 - 10"), I(-3));
        assert_eq!(ev("6 * 7"), I(42));
        assert_eq!(ev("7 % 3"), I(1));
        assert_eq!(ev("-7 / 2"), I(-3));
        assert_eq!(ev("9223372036854775807 + 1"), I(i64::MIN));
        assert_eq!(err("1 / 0"), "integer division by zero");
        assert_eq!(err("1 % 0"), "integer modulo by zero");
    }

    #[test]
    fn double_and_mixed_arithmetic_promote_to_double() {
        assert_eq!(ev("1.5 + 1.5"), D(3.0));
        assert_eq!(ev("5.0 - 1.5"), D(3.5));
        assert_eq!(ev("2.0 * 1.5"), D(3.0));
        assert_eq!(ev("7.5 % 2.0"), D(1.5));
        assert_eq!(ev("2 - 0.5"), D(1.5));
        assert_eq!(ev("2.5 - 1"), D(1.5));
        assert_eq!(ev("2 * 0.5"), D(1.0));
        assert_eq!(ev("0.5 * 4"), D(2.0));
        assert_eq!(ev("1 / 4.0"), D(0.25));
        assert_eq!(ev("1.0 / 4"), D(0.25));
        assert_eq!(ev("2.5 + 1"), D(3.5));
        // Division by zero in doubles is IEEE, not an error.
        assert_eq!(ev("1.0 / 0.0"), D(f64::INFINITY));
    }

    #[test]
    fn mismatched_operands_say_which_types() {
        assert_eq!(err("'a' - 1"), "operator Sub does not accept string and int");
        assert_eq!(err("true + 1"), "operator Add does not accept bool and int");
        assert_eq!(err("'a' + 1"), "operator Add does not accept string and int");
        assert_eq!(err("1 in 2"), "operator In does not accept int and int");
    }

    #[test]
    fn comparisons_across_types() {
        assert_eq!(ev("1 < 2.5"), B(true));
        assert_eq!(ev("2.5 <= 2"), B(false));
        assert_eq!(ev("3 >= 3.0"), B(true));
        assert_eq!(ev("2.0 > 1"), B(true));
        assert_eq!(ev("'a' < 'b'"), B(true));
        assert_eq!(ev("false < true"), B(true));
        assert_eq!(ev("1 == 1.0"), B(true));
        assert_eq!(ev("1.0 == 1"), B(true));
        assert_eq!(ev("1 != 2"), B(true));
        assert_eq!(ev("null == null"), B(true));
        assert_eq!(ev("[1, 2] == [1, 2.0]"), B(true));
        assert_eq!(ev("[1, 2] == [1]"), B(false));
        assert_eq!(ev("'1' == 1"), B(false));
        assert_eq!(ev("{'a': 1} == {'a': 1}"), B(false), "maps never compare equal");
        assert_eq!(err("'a' < 1"), "cannot compare string with int");
        assert_eq!(err("[1] < [2]"), "cannot compare list with list");
    }

    #[test]
    fn nan_comparisons_are_errors() {
        let mut ctx = Context::new();
        ctx.insert("nan".into(), D(f64::NAN));
        for e in ["nan < 1.0", "nan < 1", "1 < nan"] {
            assert_eq!(try_ev(e, &ctx).unwrap_err().message, "NaN in comparison", "{e}");
        }
    }

    #[test]
    fn in_on_strings_and_lists() {
        assert_eq!(ev("'ex' in 'text'"), B(true));
        assert_eq!(ev("'z' in 'text'"), B(false));
        assert_eq!(ev("3 in [1, 2]"), B(false));
        assert_eq!(ev("'a' in ['a']"), B(true));
    }

    #[test]
    fn unary_operators() {
        assert_eq!(ev("-(2)"), I(-2));
        assert_eq!(ev("-(2.5)"), D(-2.5));
        assert_eq!(ev("!false"), B(true));
        assert!(err("!1").starts_with("cannot apply Not to int"));
        assert!(err("-'a'").starts_with("cannot apply Neg to string"));
    }

    #[test]
    fn logic_needs_bools_unless_short_circuited() {
        assert_eq!(err("1 && true"), "expected a bool, got int");
        assert_eq!(err("true && 1"), "expected a bool, got int");
        assert_eq!(err("false || 'x'"), "expected a bool, got string");
        assert_eq!(err("'x' ? 1 : 2"), "expected a bool, got string");
        // The right side is not evaluated when the left decides.
        assert_eq!(ev("false && nope.x"), B(false));
        assert_eq!(ev("true || nope.x"), B(true));
        assert_eq!(ev("true && false"), B(false));
        assert_eq!(ev("false || true"), B(true));
    }

    #[test]
    fn names_fields_and_indexes() {
        let ctx = sample_ctx();
        assert_eq!(try_ev("m.a", &ctx).unwrap(), I(1));
        assert_eq!(try_ev("m['a']", &ctx).unwrap(), I(1));
        assert_eq!(try_ev("l[1]", &ctx).unwrap(), I(20));
        assert_eq!(err("nope"), "unknown identifier `nope`");
        assert_eq!(err("m.b"), "field `b` not set on map");
        assert_eq!(err("s.x"), "`.x` on a string is not supported");
        assert_eq!(err("l[2]"), "list index 2 out of range (len 2)");
        assert_eq!(err("l[-1]"), "list index -1 out of range (len 2)");
        assert_eq!(err("m['z']"), "map has no key `z`");
        assert_eq!(err("l['a']"), "cannot index list with string");
        assert_eq!(err("s[0]"), "cannot index string with int");
        // An error inside an index or a list item comes through.
        assert_eq!(err("l[nope]"), "unknown identifier `nope`");
        assert_eq!(err("[1, nope]"), "unknown identifier `nope`");
    }

    #[test]
    fn map_literals_need_string_keys() {
        assert_eq!(ev("{'a': 1 + 1}"), Value::Map(BTreeMap::from([("a".into(), I(2))])));
        assert_eq!(err("{1: 2}"), "map keys must be strings");
        assert_eq!(err("{'a': nope}"), "unknown identifier `nope`");
    }

    #[test]
    fn macros_over_lists_and_map_keys() {
        let ctx = sample_ctx();
        let e = |s: &str| try_ev(s, &ctx).unwrap_or_else(|e| panic!("`{s}`: {e}"));
        assert_eq!(e("l.exists(x, x > 15)"), B(true));
        assert_eq!(e("l.all(x, x > 15)"), B(false));
        assert_eq!(e("l.exists_one(x, x > 5)"), B(false));
        assert_eq!(e("l.filter(x, x > 15)"), Value::List(vec![I(20)]));
        assert_eq!(e("l.map(x, x * 2)"), Value::List(vec![I(20), I(40)]));
        assert_eq!(e("l.map(x, x > 15, x + 1)"), Value::List(vec![I(21)]));
        assert_eq!(e("m.exists(k, k == 'a')"), B(true));
        assert_eq!(e("[].all(x, x)"), B(true));
        // The macro's name does not leak, and a name from outside is kept.
        assert_eq!(e("l.map(s, s)[0] == 10 && s == 'text'"), B(true));
        assert_eq!(err("l.map(1, 2)"), ".map() takes a name and an expression");
        assert_eq!(err("l.exists(x, 1)"), "expected a bool, got int");
        assert_eq!(err("s.exists(x, true)"), ".exists() on a string is not supported");
        assert_eq!(err("nope.size()"), "unknown identifier `nope`");
    }

    #[test]
    fn string_and_list_methods() {
        let e = ev;
        assert_eq!(e("'Hello'.size()"), I(5));
        assert_eq!(e("[1, 2].size()"), I(2));
        assert_eq!(e("{'a': 1}.size()"), I(1));
        assert_eq!(e("'abc'.contains('b')"), B(true));
        assert_eq!(e("'abc'.startsWith('ab') && 'abc'.endsWith('bc')"), B(true));
        assert_eq!(e("'Ab'.lowerAscii() + 'Ab'.upperAscii()"), S("abAB".into()));
        assert_eq!(e("'  x '.trim()"), S("x".into()));
        assert_eq!(e("'a,b'.split(',')"), Value::List(vec![S("a".into()), S("b".into())]));
        assert_eq!(e("['a', 'b'].join(', ')"), S("a, b".into()));
        assert_eq!(e("['a', 'b'].join()"), S("ab".into()));
        assert_eq!(err("[1].join(',')"), ".join() needs text, got int");
        assert_eq!(err("'a'.contains(1)"), ".contains() needs text, got int");
        assert_eq!(err("l.nope()"), "`.nope()` with 0 argument(s) is not supported on a list");
    }

    #[test]
    fn double_size_and_timestamp() {
        assert_eq!(ev("double(2)"), D(2.0));
        assert_eq!(ev("size('abc')"), I(3));
        assert_eq!(ev("timestamp(0)"), S("1970-01-01T00:00:00Z".into()));
        assert_eq!(ev("timestamp(1762281000)"), S("2025-11-04T18:30:00Z".into()));
        assert_eq!(ev("timestamp(951782400.5)"), S("2000-02-29T00:00:00Z".into()));
        assert_eq!(ev("timestamp(-86400)"), S("1969-12-31T00:00:00Z".into()));
        assert_eq!(ev("timestamp('2026-01-01T00:00:00Z')"), S("2026-01-01T00:00:00Z".into()));
        assert_eq!(err("timestamp(true)"), "timestamp(bool) is not supported");
    }

    #[test]
    fn conversions() {
        assert_eq!(ev("int(2.9)"), I(2));
        assert_eq!(ev("int(5)"), I(5));
        assert_eq!(ev("int(' 12 ')"), I(12));
        assert_eq!(ev("int(true)"), I(1));
        assert_eq!(ev("int(false)"), I(0));
        assert_eq!(err("int('x')"), "cannot parse `x` as int");
        assert_eq!(err("int([1])"), "int(list) is not supported");
        assert_eq!(ev("number(3)"), D(3.0));
        assert_eq!(ev("number(1.5)"), D(1.5));
        assert_eq!(ev("number(' 2.5 ')"), D(2.5));
        assert_eq!(err("number('x')"), "cannot parse `x` as number");
        assert_eq!(err("number(true)"), "number(bool) is not supported");
        assert_eq!(ev("string(1.5)"), S("1.5".into()));
        assert_eq!(ev("string(2.0)"), S("2".into()));
        assert_eq!(ev("string(null)"), S("null".into()));
        assert_eq!(ev("string([1, 'a', [true]])"), S("[1, a, [true]]".into()));
        assert_eq!(ev("string({'a': 1})"), S("<map>".into()));
    }

    #[test]
    fn rounding_and_length() {
        assert_eq!(ev("round(2)"), I(2));
        assert_eq!(ev("round(2.5)"), D(3.0));
        assert_eq!(ev("round(7, 2)"), I(7));
        assert_eq!(ev("round(1.005, 1)"), D(1.0));
        assert_eq!(err("round('a')"), "round(string) is not supported");
        assert_eq!(err("round(1.5, 'a')"), "round(number, string) is not supported");
        assert_eq!(ev("len({'a': 1, 'b': 2})"), I(2));
        assert_eq!(ev("len('héllo')"), I(5), "characters, not bytes");
        assert_eq!(err("len(3)"), "len(int) is not supported");
    }

    #[test]
    fn ago_takes_seconds_and_refuses_the_rest() {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
        let mut ctx = Context::new();
        ctx.insert("t".into(), I(now - 7_200));
        ctx.insert("d".into(), D((now - 120) as f64));
        assert_eq!(try_ev("ago(t)", &ctx).unwrap(), S("2h ago".into()));
        assert_eq!(try_ev("ago(d)", &ctx).unwrap(), S("2m ago".into()));
        assert_eq!(err("ago('x')"), "ago(string) is not supported");
    }

    #[test]
    fn unknown_functions_and_wrong_argument_counts() {
        assert_eq!(err("nope(1)"), "unknown function `nope` with 1 args");
        assert_eq!(err("len(1, 2)"), "unknown function `len` with 2 args");
        assert_eq!(err("has()"), "unknown function `has` with 0 args");
    }

    #[test]
    fn display_strings() {
        assert_eq!(D(1e16).to_display_string(), "10000000000000000");
        assert_eq!(D(f64::INFINITY).to_display_string(), "inf");
        assert_eq!(D(-0.5).to_display_string(), "-0.5");
        assert_eq!(Value::Null.to_display_string(), "null");
        assert_eq!(B(false).to_display_string(), "false");
        assert_eq!(EvalError::new("boom").to_string(), "boom");
    }

    #[test]
    fn format_scales_and_labels() {
        assert_eq!(ev("format(1536, 'bytes')"), S("1.5 KiB".into()));
        assert_eq!(ev("format(512, 'bytes')"), S("512 B".into()));
        assert_eq!(ev("format(3.5 * 1024.0 * 1024.0 * 1024.0, 'bytes')"), S("3.5 GiB".into()));
        assert_eq!(ev("format(2345678, 'si')"), S("2.3 M".into()));
        assert_eq!(ev("format(999, 'si')"), S("999".into()));
        assert_eq!(ev("format(41.96, '%')"), S("42%".into()));
        assert_eq!(ev("format(21.54, 'C')"), S("21.5 C".into()));
        assert_eq!(ev("format(-0.04, '')"), S("0".into()));
        assert_eq!(err("format('a', 'C')"), "format() needs a number, got string");
        assert_eq!(err("format(1, 2)"), "format(x, unit) needs a text unit");
    }

    #[test]
    fn duration_takes_the_two_largest_units() {
        assert_eq!(ev("duration(0)"), S("0s".into()));
        assert_eq!(ev("duration(45)"), S("45s".into()));
        assert_eq!(ev("duration(303)"), S("5m 3s".into()));
        assert_eq!(ev("duration(3600)"), S("1h".into()));
        assert_eq!(ev("duration(3900.4)"), S("1h 5m".into()));
        assert_eq!(ev("duration(273600)"), S("3d 4h".into()));
        assert_eq!(ev("duration(86400 + 59)"), S("1d".into()), "seconds under a day's minutes drop");
        assert_eq!(ev("duration(-90)"), S("-1m 30s".into()));
        assert_eq!(err("duration('x')"), "duration() needs a number, got string");
    }

    #[test]
    fn clamp_keeps_within_bounds() {
        assert_eq!(ev("clamp(5, 0, 3)"), I(3));
        assert_eq!(ev("clamp(-1, 0, 3)"), I(0));
        assert_eq!(ev("clamp(2, 0, 3)"), I(2));
        assert_eq!(ev("clamp(1.5, 0, 1)"), D(1.0));
        assert_eq!(ev("clamp(5, 0.5, 2.5)"), D(2.5));
        assert_eq!(err("clamp(1, 3, 0)"), "clamp(x, a, b) needs a <= b");
        assert_eq!(err("clamp('a', 0, 1)"), "clamp() needs a number, got string");
    }

    #[test]
    fn colors_and_mixing() {
        let mut ctx = Context::new();
        ctx.insert("theme".into(), Value::Map(BTreeMap::from([
            ("accent".to_string(), S("#2196F3".into())),
            ("font".to_string(), S("roboto 16".into())),
        ])));
        let c = |e: &str| try_ev(e, &ctx);
        assert_eq!(c("color('accent')").unwrap(), S("#2196f3".into()));
        assert_eq!(c("color('#abc')").unwrap(), S("#aabbcc".into()));
        assert_eq!(c("color('#11223344')").unwrap(), S("#11223344".into()));
        assert_eq!(c("mix('#000000', '#ffffff', 0.5)").unwrap(), S("#808080".into()));
        assert_eq!(c("mix('accent', '#000', 1)").unwrap(), S("#000000".into()));
        assert_eq!(c("mix('#ff0000', '#0000ff', 2)").unwrap(), S("#0000ff".into()), "t is clamped");
        assert_eq!(c("color('font')").unwrap_err().message, "`font` is not a theme color");
        assert_eq!(c("color('nope')").unwrap_err().message, "`nope` is not a theme color");
        assert_eq!(c("color('#12')").unwrap_err().message, "`#12` is not a color");
        assert_eq!(c("color('#zzzzzz')").unwrap_err().message, "`#zzzzzz` is not a color");
        assert_eq!(c("color(1)").unwrap_err().message, "a color is text, got int");
        assert_eq!(err("color('accent')"), "`accent` is not a color", "no theme in scope");
    }

    #[test]
    fn every_listed_function_exists_at_its_arities() {
        for (name, arities) in FUNCTIONS {
            for n in *arities {
                let args = vec!["1"; *n].join(", ");
                let e = try_ev(&format!("{name}({args})"), &sample_ctx());
                if let Err(e) = e {
                    assert!(!e.message.starts_with("unknown function"), "{name}/{n}: {}", e.message);
                }
            }
        }
    }

    #[test]
    fn a_list_of_pools_filtered_mapped_and_joined() {
        let pool = |name: &str, health: &str, alloc: i64, size: i64| {
            Value::Map(BTreeMap::from([
                ("name".to_string(), S(name.into())),
                ("health".to_string(), S(health.into())),
                ("alloc".to_string(), I(alloc)),
                ("size".to_string(), I(size)),
            ]))
        };
        let mut ctx = Context::new();
        ctx.insert("pools".into(), Value::List(vec![pool("tank", "ONLINE", 90, 100), pool("fast", "DEGRADED", 10, 100)]));
        let e = |s: &str| try_ev(s, &ctx).unwrap_or_else(|e| panic!("`{s}`: {e}"));
        assert_eq!(e("pools.exists(p, p.health != 'ONLINE')"), B(true));
        assert_eq!(e("pools.filter(p, p.health != 'ONLINE').map(p, p.name + ' ' + p.health).join(', ')"), S("fast DEGRADED".into()));
        assert_eq!(e("pools.filter(p, double(p.alloc) > 0.8 * double(p.size)).map(p, p.name).join(', ')"), S("tank".into()));
        assert_eq!(e("string(int(100.0 * double(pools[0].alloc) / double(pools[0].size))) + '%'"), S("90%".into()));
    }
}
