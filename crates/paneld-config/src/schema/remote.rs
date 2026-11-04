//! Schema 17: `remote_kind` and `remote` (ADR 0018).
//!
//! A kind is checked once, its members against the source and action
//! keys of its `via`; an instance against its kind's params. Members
//! are named `<instance>.<member>` for references, so `on_tap =
//! device1.reboot` and `refresh = device1.temp` check like any other
//! action or source.

use super::*;

/// What `via` can be.
pub(crate) const VIAS: &[&str] = &["local", "ssh", "http", "socket"];

/// The request keys of an `http` member.
pub const METHODS: &[&str] = &["get", "post", "put", "patch", "delete"];

/// Member names that `device1.<name>` uses for the remote itself.
const RESERVED_MEMBERS: &[&str] = &["ok", "error", "status"];

/// Expression roots an instance name would shadow (schema 16).
const RESERVED_ROOTS: &[&str] = &[
    "sources", "vars", "alerts", "theme", "now", "secrets", "page", "nav", "item", "out", "event",
    "value", "result", "level", "output", "self", "true", "false", "null",
];

const fn k(name: &'static str, ty: ExpectedType, required: bool, expr_ok: bool) -> KeyRule {
    KeyRule { name, ty, required, expr_ok }
}

const HEADERS: NestedBlock = NestedBlock { kind: "headers", name_mode: NameMode::Forbidden, schema: ENV_BLOCK };
const ENV: NestedBlock = NestedBlock { kind: "env", name_mode: NameMode::Forbidden, schema: ENV_BLOCK };
const SET: NestedBlock = NestedBlock { kind: "set", name_mode: NameMode::Forbidden, schema: SET_BLOCK };
const THEN: NestedBlock = NestedBlock { kind: "then", name_mode: NameMode::Optional, schema: THEN_ELSE_BLOCK };
const ELSE: NestedBlock = NestedBlock { kind: "else", name_mode: NameMode::Optional, schema: THEN_ELSE_BLOCK };

use ExpectedType as E;

/// Source keys every `value` takes, whatever its `via`.
macro_rules! value_keys {
    ($($extra:expr),* $(,)?) => {
        &[
            k("poll", E::Dur, false, false),
            k("stream", E::Bool, false, false),
            k("parse", E::OneOf(&["text", "number", "json", "lines"]), false, false),
            k("value", E::Any, false, true),
            k("timeout", E::Dur, false, false),
            k("history", E::Dur, false, false),
            k("active", E::OneOf(&["always", "visible"]), false, false),
            $($extra),*
        ]
    };
}

/// Action keys every `action` member takes, whatever its `via`.
macro_rules! action_keys {
    ($($extra:expr),* $(,)?) => {
        &[
            k("pin", E::Bool, false, false),
            k("confirm", E::Str, false, true),
            k("title", E::Str, false, true),
            k("timeout", E::Dur, false, false),
            k("goto", E::Ident, false, false),
            k("refresh", E::IdentOrList, false, false),
            k("then", E::Ident, false, false),
            k("else", E::Ident, false, false),
            k("busy", E::Str, false, true),
            k("done", E::Str, false, true),
            k("failed", E::Str, false, true),
            $($extra),*
        ]
    };
}

const HTTP_REQUEST: [KeyRule; 6] = [
    k("get", E::Str, false, true),
    k("post", E::Str, false, true),
    k("put", E::Str, false, true),
    k("patch", E::Str, false, true),
    k("delete", E::Str, false, true),
    k("body", E::Str, false, true),
];

const VALUE_LOCAL: &BlockSchema = &BlockSchema {
    keys: value_keys![k("run", E::Command, true, false), k("cwd", E::Path, false, false)],
    nested: &[ENV],
    body_mode: BodyMode::Keyed,
};
// `env` is accepted here to be refused with a reason in `check_kind`.
const VALUE_SSH: &BlockSchema = &BlockSchema {
    keys: value_keys![k("run", E::Command, true, false)],
    nested: &[ENV],
    body_mode: BodyMode::Keyed,
};
const VALUE_HTTP: &BlockSchema = &BlockSchema {
    keys: value_keys![
        HTTP_REQUEST[0], HTTP_REQUEST[1], HTTP_REQUEST[2], HTTP_REQUEST[3], HTTP_REQUEST[4], HTTP_REQUEST[5]
    ],
    nested: &[HEADERS],
    body_mode: BodyMode::Keyed,
};
const VALUE_SOCKET: &BlockSchema = &BlockSchema {
    keys: value_keys![k("send", E::Str, true, true)],
    nested: &[],
    body_mode: BodyMode::Keyed,
};

const ACTION_LOCAL: &BlockSchema = &BlockSchema {
    keys: action_keys![k("run", E::Command, false, false), k("cwd", E::Path, false, false)],
    nested: &[ENV, SET, THEN, ELSE],
    body_mode: BodyMode::Keyed,
};
const ACTION_SSH: &BlockSchema = &BlockSchema {
    keys: action_keys![k("run", E::Command, false, false)],
    nested: &[ENV, SET, THEN, ELSE],
    body_mode: BodyMode::Keyed,
};
const ACTION_HTTP: &BlockSchema = &BlockSchema {
    keys: action_keys![
        HTTP_REQUEST[0], HTTP_REQUEST[1], HTTP_REQUEST[2], HTTP_REQUEST[3], HTTP_REQUEST[4], HTTP_REQUEST[5]
    ],
    nested: &[HEADERS, SET, THEN, ELSE],
    body_mode: BodyMode::Keyed,
};
const ACTION_SOCKET: &BlockSchema = &BlockSchema {
    keys: action_keys![k("send", E::Str, false, true)],
    nested: &[SET, THEN, ELSE],
    body_mode: BodyMode::Keyed,
};

macro_rules! kind_schema {
    ($value:expr, $action:expr, [$($key:expr),* $(,)?], [$($nested:expr),*]) => {
        &BlockSchema {
            keys: &[
                k("via", E::OneOf(VIAS), true, false),
                k("timeout", E::Dur, false, false),
                $($key),*
            ],
            nested: &[
                NestedBlock { kind: "value", name_mode: NameMode::Required, schema: $value },
                NestedBlock { kind: "action", name_mode: NameMode::Required, schema: $action },
                // One per instance, named `<instance>.<name>`.
                NestedBlock { kind: "alert", name_mode: NameMode::Required, schema: ALERT_SCHEMA },
                $($nested),*
            ],
            body_mode: BodyMode::Keyed,
        }
    };
}

const KIND_LOCAL: &BlockSchema = kind_schema!(VALUE_LOCAL, ACTION_LOCAL, [], []);
const KIND_SSH: &BlockSchema = kind_schema!(
    VALUE_SSH,
    ACTION_SSH,
    [
        k("host", E::Str, true, true),
        k("user", E::Str, false, true),
        k("port", E::Int, false, true),
        k("key", E::Path, true, true),
        k("known_hosts", E::Path, false, true),
        k("host_key", E::OneOf(&["check", "accept_new"]), false, false),
    ],
    []
);
const KIND_HTTP: &BlockSchema = kind_schema!(
    VALUE_HTTP,
    ACTION_HTTP,
    [
        k("url", E::Str, true, true),
        k("user", E::Str, false, true),
        k("ca", E::Path, false, true),
        k("insecure", E::Bool, false, false)
    ],
    [HEADERS]
);
const KIND_SOCKET: &BlockSchema = kind_schema!(
    VALUE_SOCKET,
    ACTION_SOCKET,
    [k("path", E::Path, false, true), k("host", E::Str, false, true), k("port", E::Int, false, true)],
    []
);

/// The `via` of a kind, if it is one of [`VIAS`].
pub(crate) fn via_of(kind: &Block) -> Option<&'static str> {
    let v = kind.items.iter().find_map(|it| match it {
        Item::Assign(a) if matches!(&a.target, AssignTarget::Simple { key } if key.text == "via") => {
            Some(a.value.text.trim())
        }
        _ => None,
    })?;
    VIAS.iter().copied().find(|x| *x == v)
}

pub(crate) fn find_kind<'a>(doc: &'a Document, name: &str) -> Option<&'a Block> {
    doc.items.iter().find_map(|it| match it {
        Item::Block(b) if b.kind.text == "remote_kind" && b.name.as_ref().is_some_and(|n| n.text == name) => Some(b),
        _ => None,
    })
}

/// The `value` and `action` members of a kind, with their names.
pub fn members(kind: &Block) -> impl Iterator<Item = (&str, &Block)> {
    kind.items.iter().filter_map(|it| match it {
        Item::Block(b) if matches!(b.kind.text.as_str(), "value" | "action") => {
            b.name.as_ref().map(|n| (n.text.as_str(), b))
        }
        _ => None,
    })
}

/// Every `remote <name>: <kind>` of `doc` with its kind block, when
/// that kind exists.
pub(crate) fn instances(doc: &Document) -> impl Iterator<Item = (&str, &Block, &Block)> {
    doc.items.iter().filter_map(move |it| match it {
        Item::Block(b) if b.kind.text == "remote" => {
            let name = b.name.as_ref()?;
            let kind = find_kind(doc, &b.of.as_ref()?.text)?;
            Some((name.text.as_str(), b, kind))
        }
        _ => None,
    })
}

/// Instance members as sources and actions, `device1.temp`.
pub(super) fn add_names(doc: &Document, names: &mut Names) {
    for (inst, _, kind) in instances(doc) {
        for (member, b) in members(kind) {
            let table = if b.kind.text == "value" { &mut names.sources } else { &mut names.actions };
            table.insert(format!("{inst}.{member}"));
        }
    }
}

/// The params of each instance's actions, for calls with arguments
/// (`on_tap device1.restart { unit = nginx }`). Problems in them are
/// reported once, by the kind's own check.
pub(super) fn add_params(doc: &Document, source: &SourceMap, params: &mut Params) {
    for (inst, _, kind) in instances(doc) {
        for (member, b) in members(kind) {
            if b.kind.text == "action" {
                let specs = collect_params_in_body(b, "action", source, &mut Vec::new());
                params.actions.insert(format!("{inst}.{member}"), specs);
            }
        }
    }
}

/// `remote_kind <name> { }`.
pub(super) fn check_kind(b: &Block, source: &SourceMap, diags: &mut Vec<Diagnostic>) {
    let schema = match via_of(b) {
        Some("local") => KIND_LOCAL,
        Some("ssh") => KIND_SSH,
        Some("http") => KIND_HTTP,
        Some("socket") => KIND_SOCKET,
        _ => {
            // Report a bad or missing `via` and the rest against the
            // keys every kind takes.
            const BARE: &BlockSchema = kind_schema!(VALUE_LOCAL, ACTION_LOCAL, [], []);
            if !b.items.iter().any(|it| matches!(it, Item::Assign(a) if matches!(&a.target, AssignTarget::Simple { key } if key.text == "via"))) {
                diags.push(Diagnostic::new(b.kind.span, "`remote_kind` needs `via` (`local`, `ssh`, `http` or `socket`)"));
            } else {
                validate_block_body(&without_params(b), BARE, source, diags);
            }
            return;
        }
    };
    validate_block_body(&without_params(b), schema, source, diags);

    // Kind params: plain settings, read at load.
    let kind_params = collect_params_in_body(b, "remote_kind", source, diags);
    for item in &b.items {
        let Item::Param(p) = item else { continue };
        if matches!(p.ty.text.as_str(), "action" | "source" | "list" | "any") {
            diags.push(Diagnostic::new(
                p.ty.span,
                "a remote kind param is a string, int, number, bool, duration, color, size or secret",
            ));
        }
        if !is_plain_name(&p.name.text) {
            diags.push(Diagnostic::new(p.name.span, "a param name is `[a-z][a-z0-9_]*` and not a CEL keyword"));
        }
        // Bound into every member's expressions: it would hide
        // `secrets` (and so their masking), `vars`, `out`, ...
        if RESERVED_ROOTS.contains(&p.name.text.as_str()) {
            diags.push(Diagnostic::new(
                p.name.span,
                format!("`{}` is a name expressions already use; pick another param name", p.name.text),
            ));
        }
        if let Some(opts) = &p.options {
            validate_param_options(opts, diags);
            if options_has_ask(opts) {
                diags.push(Diagnostic::new(opts.span, "`ask` is for action params only"));
            }
        }
        if let Some(d) = &p.default {
            check_param_default(p, d, diags);
        }
    }

    // `ssh`: the remote is reached on its host; `socket`: a path, or a
    // host and a port.
    let has = |key: &str| {
        b.items.iter().any(|it| matches!(it, Item::Assign(a) if matches!(&a.target, AssignTarget::Simple { key: k } if k.text == key)))
    };
    if via_of(b) == Some("socket") && has("path") == (has("host") || has("port")) {
        diags.push(Diagnostic::new(b.kind.span, "a `socket` kind sets either `path`, or `host` and `port`"));
    }
    if via_of(b) == Some("socket") && has("host") != has("port") {
        diags.push(Diagnostic::new(b.kind.span, "a TCP `socket` kind needs both `host` and `port`"));
    }

    if let Some(via) = via_of(b) {
        check_secret_use(b, via, diags);
    }
    // Where to connect comes from the instance, not from the screen:
    // connection keys read the kind's params (and `secrets`) only.
    let param_names: Vec<&str> = b
        .items
        .iter()
        .filter_map(|it| match it {
            Item::Param(p) => Some(p.name.text.as_str()),
            _ => None,
        })
        .collect();
    let connection = |items: &[Item], diags: &mut Vec<Diagnostic>| {
        for it in items {
            let Item::Assign(a) = it else { continue };
            for e in exprs_of(&a.value) {
                for (name, span) in crate::cel::idents(&e) {
                    if name != "secrets" && !param_names.contains(&name.as_str()) {
                        diags.push(Diagnostic::new(
                            span,
                            format!("a connection key reads the kind's params only; `{name}` is not one"),
                        ));
                    }
                }
            }
        }
    };
    connection(&b.items, diags);
    for it in &b.items {
        if let Item::Block(h) = it {
            if h.kind.text == "headers" {
                connection(&h.items, diags);
            }
        }
    }
    if via_of(b) == Some("socket") {
        check_socket_address(b, diags);
    }

    let mut seen: HashMap<&str, Span> = HashMap::new();
    for (name, m) in members(b) {
        let span = m.name.as_ref().map_or(m.kind.span, |n| n.span);
        if !is_plain_name(name) {
            diags.push(Diagnostic::new(span, "a member name is `[a-z][a-z0-9_]*` and not a CEL keyword"));
        }
        if RESERVED_MEMBERS.contains(&name) {
            diags.push(Diagnostic::new(
                span,
                format!("`{name}` is reserved: `<remote>.{name}` is about the remote itself"),
            ));
        }
        if let Some(prev) = seen.insert(name, span) {
            diags.push(Diagnostic::new(
                span,
                format!("member `{name}` is already defined (first at {})", source.location(prev)),
            ));
        }
        if via_of(b) == Some("http") {
            let methods: Vec<&Assign> = m
                .items
                .iter()
                .filter_map(|it| match it {
                    Item::Assign(a) if matches!(&a.target, AssignTarget::Simple { key } if METHODS.contains(&key.text.as_str())) => Some(a),
                    _ => None,
                })
                .collect();
            if methods.len() > 1 {
                diags.push(Diagnostic::new(methods[1].span, "a member makes one request: one of `get`, `post`, `put`, `patch`, `delete`"));
            } else if methods.is_empty() && m.kind.text == "value" {
                diags.push(Diagnostic::new(m.kind.span, "an `http` value needs its request: `get = <path>` (or `post`, ...)"));
            }
            let has_body = m.items.iter().any(|it| matches!(it, Item::Assign(a) if matches!(&a.target, AssignTarget::Simple { key } if key.text == "body")));
            if has_body && methods.is_empty() {
                diags.push(Diagnostic::new(m.kind.span, "`body` needs a request (`post = <path>`, ...)"));
            }
        }
        if via_of(b) == Some("ssh") {
            for item in &m.items {
                if let Item::Block(e) = item {
                    if e.kind.text == "env" {
                        diags.push(Diagnostic::new(
                            e.kind.span,
                            "`env` does not reach the remote over ssh; pass values as arguments in `run`",
                        ));
                    }
                }
            }
        }
        if m.kind.text == "action" {
            for item in &m.items {
                let Item::Param(p) = item else { continue };
                if kind_params.iter().any(|k| k.name == p.name.text) {
                    diags.push(Diagnostic::new(
                        p.name.span,
                        format!("`{}` is already a param of the remote kind", p.name.text),
                    ));
                }
            }
            // The same param checks a top-level action gets.
            collect_params_in_body(m, "action", source, diags);
        }
    }
}

/// `[a-z][a-z0-9_]*` and not a CEL keyword: usable as `a.b` in an
/// expression.
fn is_plain_name(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b[0].is_ascii_lowercase()
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_')
        && !matches!(s, "in" | "true" | "false" | "null")
}

/// The parsed `{{ }}` expressions of a raw value.
fn exprs_of(v: &crate::parse::RawValue) -> Vec<crate::cel::Expr> {
    match classify(&v.text) {
        ValueShape::Literal(_) => Vec::new(),
        ValueShape::Expression(body) => {
            let open = v.text.find("{{").unwrap_or(0) as u32;
            crate::cel::parse_fragment(body, v.span.file, v.span.start + open + 2).0.into_iter().collect()
        }
        ValueShape::Interp(_) => crate::cel::parse_interp(&v.text, v.span.file, v.span.start).0,
    }
}

/// Where a kind may use `secrets` and its `secret` params: what goes
/// to the command's stdin or environment, never its argv over ssh, a
/// request path, or text that reaches the screen.
fn check_secret_use(b: &Block, via: &str, diags: &mut Vec<Diagnostic>) {
    let secret_params: Vec<String> = b
        .items
        .iter()
        .filter_map(|it| match it {
            Item::Param(p) if p.ty.text == "secret" => Some(p.name.text.clone()),
            _ => None,
        })
        .collect();
    // `allowed`: secrets may appear here. `secrets` itself is already
    // checked by the generic pass except where the remote is stricter
    // (an ssh `run`); `strict` flags it here too.
    fn walk(items: &[Item], via: &str, in_secret_block: bool, depth: u8, secret_params: &[String], diags: &mut Vec<Diagnostic>) {
        for it in items {
            match it {
                Item::Assign(a) => {
                    let AssignTarget::Simple { key } = &a.target else { continue };
                    let key = key.text.as_str();
                    let allowed = in_secret_block
                        || match via {
                            "local" => key == "run",
                            "http" => matches!(key, "url" | "user" | "body"),
                            "socket" => key == "send",
                            _ => false,
                        };
                    let strict = via == "ssh" && key == "run" && depth > 0;
                    for e in exprs_of(&a.value) {
                        if !allowed {
                            for name in secret_params {
                                if let Some(span) = crate::cel::find_ident(&e, name) {
                                    diags.push(Diagnostic::new(
                                        span,
                                        format!("`{name}` is a secret: only in `run` and `env` (local), `url`, `headers` and `body` (http), `send` (socket)"),
                                    ));
                                }
                            }
                        }
                        if strict {
                            if let Some(span) = crate::cel::find_ident(&e, "secrets") {
                                diags.push(Diagnostic::new(
                                    span,
                                    "no `secrets` in an ssh `run`: they would be on ssh's command line and in the remote process list",
                                ));
                            }
                        }
                    }
                }
                Item::Block(child) => {
                    let secret_block = in_secret_block
                        || child.kind.text == "headers"
                        || (via == "local" && child.kind.text == "env");
                    walk(&child.items, via, secret_block, depth + 1, secret_params, diags);
                }
                _ => {}
            }
        }
    }
    walk(&b.items, via, false, 0, &secret_params, diags);
}

/// A literal socket `path` or `host` that socat could read as address
/// options. An expression is checked when it runs.
fn check_socket_address(b: &Block, diags: &mut Vec<Diagnostic>) {
    for it in &b.items {
        let Item::Assign(a) = it else { continue };
        let AssignTarget::Simple { key } = &a.target else { continue };
        let v = a.value.text.trim();
        if v.contains("{{") {
            continue;
        }
        let bad = match key.text.as_str() {
            "path" => !crate::remote::socket_path_ok(v),
            "host" => !crate::remote::socket_host_ok(v),
            _ => false,
        };
        if bad {
            diags.push(Diagnostic::new(
                a.value.span,
                format!("`{v}` cannot be a socket {}: socat would read part of it as options", key.text),
            ));
        }
    }
}

/// A param default must fit its type, as for actions.
fn check_param_default(p: &Param, d: &crate::parse::RawValue, diags: &mut Vec<Diagnostic>) {
    if let Some(ParamType::Expected(ty)) = param_type_from_ident(&p.ty.text) {
        check_value_type(&d.text, d.span, ty, false, diags);
    }
}

/// `b` without its `param` lines, which the block-body validator
/// would refuse outside actions and components.
fn without_params(b: &Block) -> Block {
    Block {
        items: b.items.iter().filter(|it| !matches!(it, Item::Param(_))).cloned().collect(),
        ..b.clone()
    }
}

/// `remote <name>: <kind> { <param> = <value> }`.
pub(super) fn check_instance(b: &Block, doc: &Document, source: &SourceMap, diags: &mut Vec<Diagnostic>) {
    let Some(name) = &b.name else { return };
    if !is_plain_name(&name.text) {
        diags.push(Diagnostic::new(name.span, "a remote name is `[a-z][a-z0-9_]*` and not a CEL keyword"));
    }
    // The shipped library's own `repeat a in ...` reads no remote:
    // only the config's names count.
    if let Some(span) = shadowing(doc, &name.text).into_iter().find(|s| source.location(*s).path != std::path::Path::new("<library>")) {
        diags.push(Diagnostic::new(
            span,
            format!("`{}` is a remote; a param or `repeat` of that name would hide it in expressions", name.text),
        ));
    }
    if RESERVED_ROOTS.contains(&name.text.as_str()) {
        diags.push(Diagnostic::new(
            name.span,
            format!("`{}` is a name expressions already use; pick another remote name", name.text),
        ));
    }
    let Some(of) = &b.of else {
        diags.push(Diagnostic::new(name.span, format!("`remote {}` needs its kind: `remote {0}: <kind> {{ }}`", name.text)));
        return;
    };
    let Some(kind) = find_kind(doc, &of.text) else {
        diags.push(Diagnostic::new(of.span, format!("unknown remote kind `{}`", of.text)));
        return;
    };
    let specs = collect_params_in_body(kind, "remote_kind", source, &mut Vec::new());
    let mut seen: HashMap<&str, Span> = HashMap::new();
    for item in &b.items {
        let Item::Assign(a) = item else {
            diags.push(Diagnostic::new(item.span(), "a remote only sets its kind's params: `<param> = <value>`"));
            continue;
        };
        let AssignTarget::Simple { key } = &a.target else {
            diags.push(Diagnostic::new(a.span, "a remote only sets its kind's params: `<param> = <value>`"));
            continue;
        };
        let Some(spec) = specs.iter().find(|s| s.name == key.text) else {
            diags.push(Diagnostic::new(
                key.span,
                format!("unknown param `{}` for remote kind `{}`; valid: {}", key.text, of.text, format_param_list(&specs)),
            ));
            continue;
        };
        if let Some(prev) = seen.insert(key.text.as_str(), key.span) {
            diags.push(Diagnostic::new(key.span, format!("`{}` is already set (first at {})", key.text, source.location(prev))));
        }
        if a.value.text.contains("{{") {
            diags.push(Diagnostic::new(a.value.span, "a remote param is read once at load: a literal or a `$variable`, not `{{ }}`"));
            continue;
        }
        if let ParamType::Expected(ty) = spec.ty {
            check_value_type(&a.value.text, a.value.span, ty, false, diags);
        }
        let is_secret = kind.items.iter().any(|it| matches!(it, Item::Param(p) if p.name.text == key.text && p.ty.text == "secret"));
        if is_secret && !has_secret(doc, a.value.text.trim()) {
            diags.push(Diagnostic::new(a.value.span, format!("unknown secret `{}`", a.value.text.trim())));
        }
    }
    for spec in &specs {
        if !spec.has_default && !seen.contains_key(spec.name.as_str()) {
            diags.push(Diagnostic::new(
                name.span,
                format!("remote `{}` is missing `{}` ({})", name.text, spec.name, param_type_name(spec.ty)),
            ));
        }
    }
}

fn has_secret(doc: &Document, name: &str) -> bool {
    doc.items.iter().any(|it| matches!(it, Item::Block(b) if b.kind.text == "secret" && b.name.as_ref().is_some_and(|n| n.text == name)))
}

/// The first `param` or `repeat` binding named `name` anywhere in
/// `doc`: inside it, `name.x` would read the param, not the remote.
fn shadowing(doc: &Document, name: &str) -> Vec<Span> {
    fn walk(items: &[Item], name: &str, out: &mut Vec<Span>) {
        for it in items {
            match it {
                Item::Param(p) if p.name.text == name => out.push(p.name.span),
                Item::Repeat(r) => {
                    if r.binding.text == name {
                        out.push(r.binding.span);
                    }
                    walk(&r.items, name, out);
                }
                Item::Block(b) => walk(&b.items, name, out),
                _ => {}
            }
        }
    }
    let mut out = Vec::new();
    walk(&doc.items, name, &mut out);
    out
}

/// `self` (the instance) exists only in a remote kind's alerts: refuse
/// it everywhere else, where it would be an unknown name at run time.
pub(super) fn check_self_use(doc: &Document, diags: &mut Vec<Diagnostic>) {
    fn walk(items: &[Item], in_kind: bool, diags: &mut Vec<Diagnostic>) {
        for it in items {
            match it {
                Item::Assign(a) => {
                    for e in exprs_of(&a.value) {
                        if let Some(span) = crate::cel::find_ident(&e, "self") {
                            diags.push(Diagnostic::new(span, "`self` is the instance in a remote kind's alerts only; elsewhere use its name"));
                        }
                    }
                }
                Item::Block(b) if in_kind && b.kind.text == "alert" => {}
                Item::Block(b) => walk(&b.items, in_kind || b.kind.text == "remote_kind", diags),
                Item::Repeat(r) => walk(&r.items, in_kind, diags),
                _ => {}
            }
        }
    }
    walk(&doc.items, false, diags);
}

/// References inside a kind's members: a sibling member by its bare
/// name (`refresh = temp`, `then = reboot`), then anything global.
pub(super) fn resolve_kind_refs(kind: &Block, names: &Names, diags: &mut Vec<Diagnostic>) {
    let mut local = names.clone();
    for (name, m) in members(kind) {
        let table = if m.kind.text == "value" { &mut local.sources } else { &mut local.actions };
        table.insert(name.to_string());
    }
    for (_, m) in members(kind) {
        if m.kind.text == "action" {
            resolve_refs_in_block(m, "action", &local, diags);
        }
    }
    // A kind's alert: `from` a sibling value, `action` a sibling action.
    for it in &kind.items {
        if let Item::Block(b) = it {
            if b.kind.text == "alert" {
                resolve_refs_in_block(b, "alert", &local, diags);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::load;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static N: AtomicUsize = AtomicUsize::new(0);

    fn check_text(text: &str) -> Vec<String> {
        let dir = std::env::temp_dir().join(format!("paneld-remote-{}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("p.conf");
        std::fs::write(&p, text).unwrap();
        let (map, doc, mut diags) = load(&p).unwrap();
        diags.extend(crate::check(&doc, &map));
        let _ = std::fs::remove_dir_all(&dir);
        diags.iter().map(|d| format!("{}: {}", map.location(d.span), d.message)).collect()
    }

    fn has(diags: &[String], needle: &str) -> bool {
        diags.iter().any(|d| d.contains(needle))
    }

    const LINUX: &str = "secret tok { env = TOK }\n\
        remote_kind linux {\n\
            param host: string\n\
            param user: string = admin\n\
            via  = ssh\n\
            host = {{ host }}\n\
            user = {{ user }}\n\
            key  = /etc/paneld/id\n\
            value temp {\n\
                run   = cat /sys/class/thermal/thermal_zone0/temp\n\
                parse = number\n\
                poll  = 30s\n\
            }\n\
            action reboot { confirm = Reboot {{ host }}?; run = sudo systemctl reboot; refresh = temp }\n\
            action restart {\n\
                param unit: string { ask = select; options = [nginx, ssh] }\n\
                run = sudo systemctl restart -- {{ unit }}\n\
            }\n\
        }\n";

    #[test]
    fn a_kind_and_its_instances_check_and_are_referenced() {
        let d = check_text(&format!(
            "{LINUX}remote device1: linux {{ host = 10.0.0.5 }}\n\
             remote device2: linux {{ host = 10.0.0.6; user = pi }}\n\
             remote[device2]:host = 10.0.0.7\n\
             page home {{\n\
                 text {{ value = {{{{ device1.temp }}}} }}\n\
                 button {{ label = Reboot; on_tap = device1.reboot }}\n\
                 button {{ label = Restart; on_tap device2.restart {{ unit = nginx }} }}\n\
             }}\n\
             action both {{ refresh = [device1.temp, device2.temp] }}\n"
        ));
        assert!(d.is_empty(), "{d:?}");
    }

    #[test]
    fn instance_errors() {
        let d = check_text(&format!(
            "{LINUX}remote device1: linux {{ }}\n\
             remote device2: nope {{ host = x }}\n\
             remote device3: linux {{ host = {{{{ vars.h }}}}; port = 2 }}\n\
             remote vars: linux {{ host = x }}\n\
             remote device4 {{ host = x }}\n\
             source s: linux {{ run = x }}\n\
             page p {{ button {{ on_tap = device1.nope }} }}\n\
             page q {{ repeat device1 in {{{{ [1] }}}} {{ text {{ value = x }} }} }}\n"
        ));
        for needle in [
            "remote `device1` is missing `host`",
            "unknown remote kind `nope`",
            "read once at load",
            "unknown param `port`",
            "a name expressions already use",
            "needs its kind",
            "is for `remote` only",
            "unknown action `device1.nope`",
            "would hide it in expressions",
        ] {
            assert!(has(&d, needle), "missing `{needle}` in {d:?}");
        }
    }

    #[test]
    fn kind_errors() {
        let d = check_text(
            "secret tok { env = TOK }\n\
             remote_kind a {\n\
                 param token: secret\n\
                 param fn: action\n\
                 via  = ssh\n\
                 host = h\n\
                 value ok { run = x }\n\
                 value t { run = x; poll = 1s; env { A = b } }\n\
                 value t { run = y }\n\
                 value u { run = echo {{ secrets.tok }} }\n\
                 action x { param token: string\n run = y }\n\
             }\n\
             remote_kind b {\n\
                 param token: secret\n\
                 via = http\n\
                 url = https://nas/api\n\
                 headers { Authorization = Bearer {{ token }}; }\n\
                 value s { get = /a/{{ token }} }\n\
                 value n { }\n\
                 action two { get = /a; post = /b }\n\
                 action t { confirm = {{ token }}?; post = /x }\n\
             }\n\
             remote_kind c { via = socket; path = /run/x,fork; value v { send = hi } }\n\
             remote_kind d { via = carrier }\n\
             remote_kind e { value v { run = x } }\n\
             remote nas: b { token = nope }\n\
             action z { param p: secret }\n",
        );
        for needle in [
            "missing required key `key`",
            "`ok` is reserved",
            "`env` does not reach the remote over ssh",
            "member `t` is already defined",
            "no `secrets` in an ssh `run`",
            "already a param of the remote kind",
            "a remote kind param is a string",
            "`token` is a secret",
            "needs its request",
            "one request",
            "socat would read part of it",
            "expected one of",
            "needs `via`",
            "unknown secret `nope`",
            "`secret` params are for remote kinds only",
        ] {
            assert!(has(&d, needle), "missing `{needle}` in {d:?}");
        }
        // Connection keys read params only.
        let d2 = check_text("remote_kind k {\n param host: string\n via = ssh\n host = {{ vars.h }}\n user = {{ host }}\n key = /k\n }\nvar a.b: int = 1\nremote output: k { host = x }\n");
        assert!(has(&d2, "`vars` is not one"), "{d2:?}");
        assert!(!has(&d2, "`host` is not one"), "{d2:?}");
        assert!(has(&d2, "a `var` name cannot contain `.`"), "{d2:?}");
        assert!(has(&d2, "a name expressions already use"), "{d2:?}");
        // Reserved param names, overrides checked like block keys, and
        // macro variables are not params.
        let d3 = check_text("remote_kind k {\n param secrets: string = x\n param host: string\n via = http\n url = http://{{ [host].exists(x, x == '') ? 'a' : host }}\n }\n\
                             remote_kind[k]:url = http://{{ vars.h }}/api\nremote d1: k { host = h }\nremote[d1]:nope = 1\nremote[d1]:host = {{ vars.x }}\n");
        assert!(has(&d3, "pick another param name"), "{d3:?}");
        assert!(!has(&d3, "`x` is not one"), "{d3:?}");
        assert!(has(&d3, "`vars` is not one"), "{d3:?}");
        assert!(has(&d3, "unknown param `nope`"), "{d3:?}");
        assert!(has(&d3, "read once at load"), "{d3:?}");
        // The header itself may hold the secret param.
        assert!(!d.iter().any(|m| m.contains(":18:") && m.contains("is a secret")), "{d:?}");
    }

    #[test]
    fn a_kind_may_declare_alerts_reading_its_members() {
        let src = "remote_kind box {\n param label: string\n via = local\n value temp { run = echo 1; parse = number }\n\
                   value log { run = cat; stream = true }\n action fix { run = true }\n\
                   alert hot { when = {{ self.temp > 80 }}; title = {{ label }}; action = fix }\n alert ev { from = log }\n}\n\
                   remote a: box { label = x }\n";
        let d = check_text(src);
        assert!(d.is_empty(), "{d:?}");
        let d = check_text("remote_kind box { via = local\n alert bad { from = nope } }\nremote self: box { }\n");
        assert!(has(&d, "unknown source `nope`"), "{d:?}");
        assert!(has(&d, "a name expressions already use"), "{d:?}");
        // The library's own `repeat a in ...` does not keep a remote from
        // being called `a`.
        assert!(check_text("remote_kind k { via = local }\nremote a: k { }\n").is_empty());
    }

    #[test]
    fn self_is_only_for_a_kinds_alerts() {
        let d = check_text(
            "remote_kind k { via = local\n value v { run = echo {{ self.x }} }\n alert a { when = {{ self.v > 1 }} } }\n\
             page p { text { value = {{ self.v }} } }\n",
        );
        assert_eq!(d.iter().filter(|m| m.contains("`self` is the instance")).count(), 2, "{d:?}");
    }
}
