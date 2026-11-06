//! What an expression reads: each root name it looks up and the
//! static path after it (`sources.cpu.value`, `now['time']`). One
//! analysis for `paneld check` (every name must be in scope where it
//! is written), for the sources a page shows (`active = visible`) and
//! for the dep keys of in-place updates (ADR 0024).

use crate::cel::{self, Expr, ExprKind};
use crate::parse::RawValue;
use crate::source::Span;
use crate::value::{classify, ValueShape};

/// The macros (`l.map(x, ...)`): their first argument is a name bound
/// in the others.
pub const MACROS: &[&str] = &["all", "exists", "exists_one", "map", "filter"];

/// One step after the root: `.name` or `['name']`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step {
    pub name: String,
    /// Written as `['name']` rather than `.name`.
    pub indexed: bool,
    /// The access up to and including this step (`sources.x` in
    /// `sources.x.value`).
    pub span: Span,
}

/// A name an expression reads, with what it reads of it.
#[derive(Clone, Debug)]
pub struct Read {
    pub root: String,
    pub root_span: Span,
    /// The static steps after the root, up to the first computed index
    /// or the end of the access.
    pub path: Vec<Step>,
    /// The access goes on with a computed index (`sources[x]`,
    /// `l[0]`): it reads more than `path` says.
    pub computed: bool,
}

impl Read {
    /// The access as written up to the step `n` (`sources.x`,
    /// `now["time"]`), for messages.
    pub fn written(&self, n: usize) -> String {
        let mut s = self.root.clone();
        for st in self.path.iter().take(n + 1) {
            if st.indexed {
                s.push_str(&format!("[\"{}\"]", st.name));
            } else {
                s.push('.');
                s.push_str(&st.name);
            }
        }
        s
    }
}

/// Every name `e` reads, in the order written. A macro's own name
/// (`x` in `l.exists(x, x > 1)`) is not a read inside that macro;
/// function names are not reads.
pub fn reads(e: &Expr) -> Vec<Read> {
    let mut out = Vec::new();
    walk(e, &mut Vec::new(), &mut out);
    out
}

/// The parsed `{{ }}` expressions of a raw value (none for a literal);
/// parse errors are the schema check's to report.
pub fn value_exprs(v: &RawValue) -> Vec<Expr> {
    match classify(&v.text) {
        ValueShape::Literal(_) => Vec::new(),
        ValueShape::Expression(body) => {
            let open = v.text.find("{{").unwrap_or(0) as u32;
            cel::parse_fragment(body, v.span.file, v.span.start + open + 2).0.into_iter().collect()
        }
        ValueShape::Interp(_) => cel::parse_interp(&v.text, v.span.file, v.span.start).0,
    }
}

/// What every expression of a raw value reads.
pub fn value_reads(v: &RawValue) -> Vec<Read> {
    value_exprs(v).iter().flat_map(reads).collect()
}

/// `a.b['c'][d]`: the root, its static steps, whether a computed
/// index follows them, and the index expressions (to walk on their
/// own). `None` when the access does not start at a name.
fn chain<'a>(e: &'a Expr, steps: &mut Vec<Step>, computed: &mut bool, indexes: &mut Vec<&'a Expr>) -> Option<(&'a str, Span)> {
    match &e.kind {
        ExprKind::Ident(n) => Some((n.as_str(), e.span)),
        ExprKind::Field(inner, f) => {
            let root = chain(inner, steps, computed, indexes)?;
            if !*computed {
                steps.push(Step { name: f.clone(), indexed: false, span: e.span });
            }
            Some(root)
        }
        ExprKind::Index(inner, idx) => {
            let root = chain(inner, steps, computed, indexes)?;
            match &idx.kind {
                ExprKind::StringLit(s) if !*computed => steps.push(Step { name: s.clone(), indexed: true, span: e.span }),
                _ => *computed = true,
            }
            indexes.push(idx);
            Some(root)
        }
        _ => None,
    }
}

fn walk(e: &Expr, bound: &mut Vec<String>, out: &mut Vec<Read>) {
    match &e.kind {
        ExprKind::Ident(_) | ExprKind::Field(..) | ExprKind::Index(..) => {
            let (mut steps, mut computed, mut indexes) = (Vec::new(), false, Vec::new());
            match chain(e, &mut steps, &mut computed, &mut indexes) {
                Some((root, root_span)) => {
                    if !bound.iter().any(|b| b == root) {
                        out.push(Read { root: root.to_string(), root_span, path: steps, computed });
                    }
                    for i in indexes {
                        walk(i, bound, out);
                    }
                }
                // `f(x).a`, `[1, 2][i]`: walk the parts.
                None => match &e.kind {
                    ExprKind::Field(inner, _) => walk(inner, bound, out),
                    ExprKind::Index(a, b) => {
                        walk(a, bound, out);
                        walk(b, bound, out);
                    }
                    _ => {}
                },
            }
        }
        ExprKind::IntLit(_) | ExprKind::DoubleLit(_) | ExprKind::StringLit(_) | ExprKind::BoolLit(_) | ExprKind::Null => {}
        ExprKind::Unary(_, a) => walk(a, bound, out),
        ExprKind::Binary(_, a, b) => {
            walk(a, bound, out);
            walk(b, bound, out);
        }
        ExprKind::Conditional(a, b, c) => {
            walk(a, bound, out);
            walk(b, bound, out);
            walk(c, bound, out);
        }
        ExprKind::Call(_, args) | ExprKind::List(args) => args.iter().for_each(|a| walk(a, bound, out)),
        ExprKind::Method(inner, name, args) => {
            walk(inner, bound, out);
            match (args.first().map(|a| &a.kind), MACROS.contains(&name.as_str()) && args.len() >= 2) {
                (Some(ExprKind::Ident(v)), true) => {
                    bound.push(v.clone());
                    args[1..].iter().for_each(|a| walk(a, bound, out));
                    bound.pop();
                }
                _ => args.iter().for_each(|a| walk(a, bound, out)),
            }
        }
        ExprKind::Map(pairs) => pairs.iter().for_each(|(k, v)| {
            walk(k, bound, out);
            walk(v, bound, out);
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(src: &str) -> Vec<(String, Vec<String>, bool)> {
        let (e, d) = cel::parse_fragment(src, crate::source::FileId::default(), 0);
        assert!(d.is_empty(), "{d:?}");
        reads(&e.unwrap()).into_iter().map(|r| (r.root, r.path.into_iter().map(|s| s.name).collect(), r.computed)).collect()
    }

    fn t(root: &str, path: &[&str], computed: bool) -> (String, Vec<String>, bool) {
        (root.to_string(), path.iter().map(|s| s.to_string()).collect(), computed)
    }

    #[test]
    fn roots_and_their_static_paths() {
        assert_eq!(r("sources.cpu.value > 1"), vec![t("sources", &["cpu", "value"], false)]);
        assert_eq!(r("now['time']"), vec![t("now", &["time"], false)]);
        assert_eq!(r("sources[vars.name].value"), vec![t("sources", &[], true), t("vars", &["name"], false)]);
        assert_eq!(r("a.b[0].c"), vec![t("a", &["b"], true)]);
        assert_eq!(r("size(now) + x"), vec![t("now", &[], false), t("x", &[], false)]);
        assert_eq!(r("has(d1.status.temp)"), vec![t("d1", &["status", "temp"], false)]);
        assert_eq!(r("string(1).size()"), vec![]);
        assert_eq!(r("{'a': y}[z]"), vec![t("y", &[], false), t("z", &[], false)]);
    }

    #[test]
    fn a_macro_name_is_bound_in_its_macro_only() {
        assert_eq!(
            r("l.exists(x, x > y) && x == 1"),
            vec![t("l", &[], false), t("y", &[], false), t("x", &[], false)]
        );
        assert_eq!(r("l.map(x, l.filter(z, z > x))"), vec![t("l", &[], false), t("l", &[], false)]);
        assert_eq!(r("l.contains(x)"), vec![t("l", &[], false), t("x", &[], false)]);
    }
}
