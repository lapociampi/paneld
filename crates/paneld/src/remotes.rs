//! Remotes at run time (schema 17, ADR 0018). The config side lowers
//! each instance into sources and actions named `<instance>.<member>`
//! that carry `remote = <instance>`; here a member's command is built
//! around its `run` or request for its kind's `via`, the kind's params
//! are bound for its expressions, and member values are grouped under
//! the instance name for pages (`device1.temp`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use paneld_config::parse::{AssignTarget, Block, Item};
use paneld_config::remote::{self as cr, Remote};
use paneld_config::source::Span;
use paneld_ui::{Context, Value};

use crate::runner;

/// The instances of the running config.
#[derive(Default)]
pub struct Remotes {
    by_name: BTreeMap<String, Remote>,
    /// Where `known_hosts` lives unless a kind says: the folder of
    /// `app:state`, which the service can write.
    state_dir: PathBuf,
}

static CURRENT: RwLock<Option<Arc<Remotes>>> = RwLock::new(None);

/// Make `r` the instances that sources and actions run against (on
/// load and reload).
pub fn install(r: Remotes) {
    *CURRENT.write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(r));
}

/// The installed instances (none before the first load).
pub fn current() -> Arc<Remotes> {
    CURRENT.read().unwrap_or_else(|e| e.into_inner()).clone().unwrap_or_default()
}

/// A command ready to run: its argv and what goes to its stdin.
#[derive(Debug, PartialEq)]
pub struct Request {
    pub argv: Vec<String>,
    pub stdin: Option<Vec<u8>>,
}

/// The command of a source or action `block` (local or a remote
/// member) with its expressions filled from `ctx`, which must already
/// hold the params ([`Remotes::bind`]) and, for commands, `secrets`.
/// `None` when it has nothing to run (an action that only sets vars).
/// `stream`: a long-running source, with no time limit on the request.
pub fn request(block: &Block, ctx: &Context, timeout: Duration, stream: bool) -> Result<Option<Request>, String> {
    let remotes = current();
    match cr::remote_of(block) {
        // A member of an instance a reload has removed must not fall
        // back to running its `run` here, on the panel.
        Some(n) => match remotes.by_name.get(n) {
            Some(remote) => remotes.build(remote, block, ctx, timeout, stream),
            None => Err(format!("remote `{n}` no longer exists")),
        },
        None => match find(block, "run") {
            Some((raw, span)) => local_argv(raw, span, ctx).map(|argv| Some(Request { argv, stdin: None })),
            None => Ok(None),
        },
    }
}

/// A remote member (not a plain source or action).
pub fn is_remote(block: &Block) -> bool {
    cr::remote_of(block).is_some()
}

/// Put `sources` into `ctx` for expressions: plain sources under
/// `sources`, each remote instance under its own name (schema 17).
pub fn expose(ctx: &mut Context, sources: BTreeMap<String, Value>) {
    let remotes = current();
    for (name, root) in remotes.roots(&sources) {
        ctx.insert(name, root);
    }
    let plain = sources.into_iter().filter(|(k, _)| !k.contains('.')).collect();
    ctx.insert("sources".into(), Value::Map(plain));
}

/// `block`'s params bound into `ctx` (see [`Remotes::bind`]).
pub fn bind(block: &Block, ctx: &mut Context) {
    if let Some(name) = cr::remote_of(block) {
        current().bind(name, ctx);
    }
}

/// A clearer `error` for a failed remote run, when the transport
/// itself failed rather than the command.
pub fn explain(block: &Block, exit: Option<i32>, stderr: &str) -> Option<String> {
    let remotes = current();
    let remote = remotes.by_name.get(cr::remote_of(block)?)?;
    let detail = stderr.trim();
    let what = match (remote.via, exit) {
        ("ssh", Some(255)) => "ssh could not run it on the remote",
        ("http", Some(22)) => "the server answered with an error status",
        ("http", Some(6)) => "the host name did not resolve",
        ("http", Some(7)) => "could not connect",
        ("http", Some(28)) => "the request timed out",
        ("http", Some(35 | 51 | 58 | 60)) => "TLS failed",
        ("socket", Some(n)) if n != 0 => "socat failed",
        _ => return None,
    };
    Some(if detail.is_empty() { what.to_string() } else { format!("{what}: {detail}") })
}

impl Remotes {
    pub fn from_doc(doc: &paneld_config::Document, state_file: &Path) -> Self {
        Remotes {
            by_name: cr::remotes(doc),
            state_dir: state_file.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from(".")),
        }
    }

    /// Bind instance `name`'s params into `ctx` by name, typed. A
    /// `secret` param is bound to the secret's value only when `ctx`
    /// holds `secrets` (a command's context).
    pub fn bind(&self, name: &str, ctx: &mut Context) {
        let Some(remote) = self.by_name.get(name) else { return };
        for (param, ty, raw) in &remote.params {
            let Some(raw) = raw else { continue };
            let value = match ty.as_str() {
                "secret" => match ctx.get("secrets") {
                    Some(Value::Map(s)) => match s.get(raw) {
                        Some(v) => v.clone(),
                        None => continue,
                    },
                    _ => continue,
                },
                "int" => paneld_config::value::parse_int(raw).map_or_else(|| Value::String(raw.clone()), Value::Int),
                "number" => {
                    paneld_config::value::parse_number(raw).map_or_else(|| Value::String(raw.clone()), Value::Double)
                }
                "bool" => paneld_config::value::parse_bool(raw).map_or_else(|| Value::String(raw.clone()), Value::Bool),
                _ => Value::String(paneld_config::value::parse_string(raw)),
            };
            ctx.insert(param.clone(), value);
        }
    }

    fn build(&self, remote: &Remote, member: &Block, ctx: &Context, timeout: Duration, stream: bool) -> Result<Option<Request>, String> {
        let kind = &remote.kind;
        // Where to connect comes from the instance alone: kind keys are
        // filled from its params (and `secrets`), never from a page's
        // names (a `repeat` binding called `host`).
        let mut kctx = Context::new();
        if let Some(secrets) = ctx.get("secrets") {
            kctx.insert("secrets".into(), secrets.clone());
        }
        self.bind(&remote.name, &mut kctx);
        let kctx = &kctx;
        // A kind key, with its expressions filled in.
        let key = |k: &str| find(kind, k).map(|(raw, span)| fill(raw, span, kctx)).transpose();
        match remote.via {
            "local" => match find(member, "run") {
                Some((raw, span)) => local_argv(raw, span, ctx).map(|argv| Some(Request { argv, stdin: None })),
                None => Ok(None),
            },
            "ssh" => {
                let Some((raw, span)) = find(member, "run") else { return Ok(None) };
                let run = local_argv(raw, span, ctx)?;
                let host = key("host")?.unwrap_or_default();
                if !plain_word(&host) {
                    return Err(format!("`{host}` is not a host name"));
                }
                let user = key("user")?;
                if let Some(u) = &user {
                    if !plain_word(u) || u.contains('@') {
                        return Err(format!("`{u}` is not a user name"));
                    }
                }
                let port = match key("port")? {
                    Some(p) => p.trim().parse::<u16>().map_err(|_| format!("`{p}` is not a port"))?,
                    None => 22,
                };
                let ssh_path = |p: String, what: &str| -> Result<String, String> {
                    if p.is_empty() || p.chars().any(|c| c.is_whitespace() || c.is_control()) {
                        return Err(format!("`{p}` cannot be the {what} path (no spaces)"));
                    }
                    // ssh expands `%` tokens in these.
                    Ok(p.replace('%', "%%"))
                };
                let key_file = ssh_path(key("key")?.unwrap_or_default(), "key")?;
                let known_hosts = ssh_path(
                    key("known_hosts")?.unwrap_or_else(|| self.state_dir.join("known_hosts").to_string_lossy().into_owned()),
                    "known_hosts",
                )?;
                let checking = match find(kind, "host_key").map(|(v, _)| v.trim()) {
                    Some("accept_new") => "accept-new",
                    _ => "yes",
                };
                let connect = timeout.min(Duration::from_secs(10)).as_secs().max(1);
                let mut argv: Vec<String> = [
                    "ssh", "-T", "-F", "none", "-o", "BatchMode=yes", "-o", "IdentitiesOnly=yes", "-i",
                ]
                .iter()
                .map(|s| s.to_string())
                .collect();
                argv.push(key_file);
                for opt in [
                    format!("StrictHostKeyChecking={checking}"),
                    format!("UserKnownHostsFile={known_hosts}"),
                    "GlobalKnownHostsFile=/dev/null".into(),
                    "UpdateHostKeys=no".into(),
                    format!("ConnectTimeout={connect}"),
                    "ServerAliveInterval=15".into(),
                    "ServerAliveCountMax=3".into(),
                    "LogLevel=ERROR".into(),
                ] {
                    argv.push("-o".into());
                    argv.push(opt);
                }
                argv.push("-p".into());
                argv.push(port.to_string());
                if let Some(u) = user {
                    argv.push("-l".into());
                    argv.push(u);
                }
                argv.push("--".into());
                argv.push(host);
                argv.push(cr::shell_quote(&run));
                Ok(Some(Request { argv, stdin: None }))
            }
            "http" => {
                let Some((method, path, path_span)) = cr::METHODS.iter().find_map(|m| find(member, m).map(|(raw, span)| (*m, raw, span)))
                else {
                    return Ok(None);
                };
                let base = key("url")?.unwrap_or_default();
                let lower = base.to_ascii_lowercase();
                if !(lower.starts_with("http://") || lower.starts_with("https://")) || base.chars().any(|c| c.is_whitespace() || c.is_control()) {
                    return Err(format!("`{base}` is not an http(s) URL"));
                }
                let path = fill_encoded(path, path_span, ctx)?;
                let url = match (base.ends_with('/'), path.starts_with('/')) {
                    (true, true) => format!("{base}{}", &path[1..]),
                    _ => format!("{base}{path}"),
                };
                if !cr::single_line(&url) {
                    return Err("the URL holds a line break".into());
                }
                let mut cfg = vec![
                    "silent".to_string(),
                    "show-error".into(),
                    "fail-with-body".into(),
                    "path-as-is".into(),
                    // `{a,b}` and `[1-3]` in a URL are not curl ranges.
                    "globoff".into(),
                    "proto = \"=http,https\"".into(),
                    format!("connect-timeout = {}", timeout.min(Duration::from_secs(10)).as_secs().max(1)),
                ];
                if !stream {
                    cfg.push(format!("max-time = {}", timeout.as_secs().max(1)));
                }
                let q = |s: &str| cr::curl_quote(s).ok_or_else(|| "a request value holds a NUL byte".to_string());
                cfg.push(format!("url = {}", q(&url)?));
                cfg.push(format!("request = {}", q(&method.to_ascii_uppercase())?));
                // The kind's headers, then the member's; a member header
                // replaces the kind's of the same name.
                let mut all: Vec<(String, String)> = Vec::new();
                for (b, c) in [(kind, kctx), (member, ctx)] {
                    for (name, raw, span) in headers(b) {
                        let value = fill(raw, span, c)?;
                        if !cr::single_line(&value) {
                            return Err(format!("header `{name}` holds a line break"));
                        }
                        all.retain(|(n, _)| !n.eq_ignore_ascii_case(&name));
                        all.push((name, value));
                    }
                }
                for (name, value) in all {
                    cfg.push(format!("header = {}", q(&format!("{name}: {value}"))?));
                }
                if let Some((raw, span)) = find(member, "body") {
                    cfg.push(format!("data-raw = {}", q(&fill(raw, span, ctx)?)?));
                }
                if let Some(ca) = key("ca")? {
                    cfg.push(format!("cacert = {}", q(&ca)?));
                }
                if find(kind, "insecure").and_then(|(v, _)| paneld_config::value::parse_bool(v.trim())) == Some(true) {
                    cfg.push("insecure".into());
                }
                let mut stdin = cfg.join("\n");
                stdin.push('\n');
                let argv = ["curl", "-q", "-K", "-"].iter().map(|s| s.to_string()).collect();
                Ok(Some(Request { argv, stdin: Some(stdin.into_bytes()) }))
            }
            "socket" => {
                let Some((raw, span)) = find(member, "send") else { return Ok(None) };
                let send = fill(raw, span, ctx)?;
                let address = match key("path")? {
                    Some(p) if cr::socket_path_ok(&p) => format!("UNIX-CONNECT:{p}"),
                    Some(p) => return Err(format!("`{p}` cannot be a socket path")),
                    None => {
                        let host = key("host")?.unwrap_or_default();
                        if !cr::socket_host_ok(&host) {
                            return Err(format!("`{host}` cannot be a socket host"));
                        }
                        let port = key("port")?.unwrap_or_default();
                        let port = port.trim().parse::<u16>().map_err(|_| format!("`{port}` is not a port"))?;
                        format!("TCP:{host}:{port}")
                    }
                };
                // socat gives up a second before the runner would kill
                // it, so a peer that keeps the connection open still
                // yields what it sent.
                let secs = {
                    let t = timeout.as_secs_f64();
                    let t = if t > 2.0 { t - 1.0 } else { t / 2.0 };
                    format!("{t:.3}").trim_end_matches('0').trim_end_matches('.').to_string()
                };
                let mut argv: Vec<String> = vec!["socat".into()];
                if stream {
                    // Keep reading after the request is sent.
                    argv.extend(["-t".into(), "2147483647".into()]);
                } else {
                    argv.extend(["-T".into(), secs.clone(), "-t".into(), secs]);
                }
                argv.extend(["-".into(), address]);
                Ok(Some(Request { argv, stdin: Some(send.into_bytes()) }))
            }
            _ => Err(format!("unknown `via` `{}`", remote.via)),
        }
    }

    /// Each instance as an expression root: its members' values by
    /// name, `ok`, `error` and `status` (schema 17). `sources` holds
    /// the lowered values under their dotted names.
    pub fn roots(&self, sources: &BTreeMap<String, Value>) -> Vec<(String, Value)> {
        self.by_name
            .iter()
            .map(|(name, remote)| {
                let mut m = BTreeMap::new();
                let mut status = BTreeMap::new();
                let (mut any_result, mut error) = (false, None);
                for (member, b) in cr_members(&remote.kind) {
                    if b.kind.text != "value" {
                        continue;
                    }
                    let full = sources.get(&format!("{name}.{member}")).cloned().unwrap_or(Value::Null);
                    let field = |k: &str| match &full {
                        Value::Map(f) => f.get(k).cloned(),
                        _ => None,
                    };
                    let ok = matches!(field("ok"), Some(Value::Bool(true)));
                    // Ran and failed, as opposed to not run yet.
                    let ran = field("at").is_some()
                        || matches!(field("error"), Some(Value::String(e)) if e != "not run yet");
                    any_result |= ok;
                    if ran && !ok && error.is_none() {
                        error = Some(match field("error") {
                            Some(Value::String(e)) if !e.is_empty() => format!("{member}: {e}"),
                            _ => format!("{member} failed"),
                        });
                    }
                    m.insert(member.to_string(), field("value").unwrap_or(Value::Null));
                    status.insert(member.to_string(), full);
                }
                m.insert("ok".into(), Value::Bool(any_result && error.is_none()));
                m.insert("error".into(), Value::String(error.unwrap_or_default()));
                m.insert("status".into(), Value::Map(status));
                (name.clone(), Value::Map(m))
            })
            .collect()
    }
}

fn cr_members(kind: &Block) -> impl Iterator<Item = (&str, &Block)> {
    kind.items.iter().filter_map(|it| match it {
        Item::Block(b) if matches!(b.kind.text.as_str(), "value" | "action") => b.name.as_ref().map(|n| (n.text.as_str(), b)),
        _ => None,
    })
}

/// `key = ...` in `b` (the last one, as overrides land last).
fn find<'a>(b: &'a Block, key: &str) -> Option<(&'a str, Span)> {
    b.items.iter().rev().find_map(|it| match it {
        Item::Assign(a) if matches!(&a.target, AssignTarget::Simple { key: k } if k.text == key) => {
            Some((a.value.text.as_str(), a.value.span))
        }
        _ => None,
    })
}

/// The `headers { }` of `b`: name (`_` sent as `-`), raw value, span.
fn headers(b: &Block) -> Vec<(String, &str, Span)> {
    b.items
        .iter()
        .filter_map(|it| match it {
            Item::Block(h) if h.kind.text == "headers" => Some(h),
            _ => None,
        })
        .flat_map(|h| &h.items)
        .filter_map(|it| match it {
            Item::Assign(a) => match &a.target {
                AssignTarget::Simple { key } => Some((key.text.replace('_', "-"), a.value.text.as_str(), a.value.span)),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

fn local_argv(raw: &str, span: Span, ctx: &Context) -> Result<Vec<String>, String> {
    runner::command_argv(raw, |t| paneld_ui::interpolate(t, span.file, span.start, ctx)).map_err(|e| format!("run: {e}"))
}

/// `raw` with each `{{ }}` filled in; an expression that does not
/// evaluate is an error, not text sent along.
fn fill(raw: &str, span: Span, ctx: &Context) -> Result<String, String> {
    fill_with(raw, span, ctx, |s| s)
}

/// [`fill`] for a request path: each filled-in value percent-encoded.
fn fill_encoded(raw: &str, span: Span, ctx: &Context) -> Result<String, String> {
    fill_with(raw, span, ctx, |s| cr::percent_encode(&s))
}

fn fill_with(raw: &str, span: Span, ctx: &Context, map: impl Fn(String) -> String) -> Result<String, String> {
    // `"..."` with its escapes resolved (config-format, "Lines").
    let unquoted = paneld_config::value::parse_string(raw);
    let raw = unquoted.as_str();
    let mut out = String::new();
    let mut i = 0;
    while let Some(off) = raw[i..].find("{{") {
        let open = i + off;
        out.push_str(&raw[i..open]);
        let Some(end) = paneld_config::cel::expr_end(raw, open) else {
            return Err("an unclosed `{{`".into());
        };
        let frag = &raw[open..end];
        let value = paneld_ui::interpolate(frag, span.file, span.start, ctx);
        if value == frag {
            return Err(format!("`{}` did not evaluate", frag.trim()));
        }
        out.push_str(&map(value));
        i = end;
    }
    out.push_str(&raw[i..]);
    Ok(out)
}

/// A host or user name: no leading `-` (ssh would read an option), no
/// spaces or control characters.
fn plain_word(s: &str) -> bool {
    !s.is_empty() && !s.starts_with('-') && !s.chars().any(|c| c.is_whitespace() || c.is_control())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(src: &str) -> paneld_config::Document {
        let mut map = paneld_config::SourceMap::new();
        let id = map.add(PathBuf::from("t.conf"), src.to_string());
        let (toks, _) = paneld_config::lex::tokenize(id, src);
        let (mut d, diags) = paneld_config::parse(&toks, &map);
        assert!(diags.is_empty(), "{diags:?}");
        paneld_config::apply_overrides(&mut d);
        cr::lower(&mut d);
        d
    }

    fn block<'a>(d: &'a paneld_config::Document, kind: &str, name: &str) -> &'a Block {
        d.items
            .iter()
            .find_map(|it| match it {
                Item::Block(b) if b.kind.text == kind && b.name.as_ref().is_some_and(|n| n.text == name) => Some(b),
                _ => None,
            })
            .unwrap()
    }

    fn ctx_for(r: &Remotes, b: &Block, secrets: &[(&str, &str)]) -> Context {
        let mut ctx = Context::new();
        ctx.insert(
            "secrets".into(),
            Value::Map(secrets.iter().map(|(k, v)| (k.to_string(), Value::String(v.to_string()))).collect()),
        );
        r.bind(cr::remote_of(b).unwrap(), &mut ctx);
        ctx
    }

    #[test]
    fn ssh_quotes_the_run_for_the_remote_shell() {
        let d = doc("remote_kind k {\n param host: string\n param user: string = admin\n via = ssh\n host = {{ host }}\n user = {{ user }}\n key = /etc/paneld/id\n\
                     action restart { param unit: string\n run = sudo systemctl restart -- {{ unit }} }\n}\n\
                     remote d1: k { host = 10.0.0.5 }\n");
        let r = Remotes::from_doc(&d, Path::new("/var/lib/paneld/state"));
        let b = block(&d, "action", "d1.restart");
        let mut ctx = ctx_for(&r, b, &[]);
        ctx.insert("unit".into(), Value::String("x'; reboot; echo '".into()));
        let req = r.build(&r.by_name["d1"], b, &ctx, Duration::from_secs(30), false).unwrap().unwrap();
        assert_eq!(req.stdin, None);
        let n = req.argv.len();
        assert_eq!(&req.argv[n - 5..n - 1], ["-l", "admin", "--", "10.0.0.5"]);
        assert_eq!(req.argv[n - 1], r#"'sudo' 'systemctl' 'restart' '--' 'x'\''; reboot; echo '\'''"#);
        assert!(req.argv.contains(&"UserKnownHostsFile=/var/lib/paneld/known_hosts".to_string()));
        assert!(req.argv.contains(&"ConnectTimeout=10".to_string()));
        // A host that would be read as an option is refused.
        let d2 = doc("remote_kind k {\n param host: string\n via = ssh\n host = {{ host }}\n key = /k\n value v { run = true }\n}\nremote d: k { host = -oProxyCommand=x }\n");
        let r2 = Remotes::from_doc(&d2, Path::new("/s/state"));
        let b2 = block(&d2, "source", "d.v");
        assert!(r2.build(&r2.by_name["d"], b2, &ctx_for(&r2, b2, &[]), Duration::from_secs(5), false).is_err());
    }

    #[test]
    fn http_puts_the_request_on_stdin_and_encodes_the_path() {
        let d = doc("secret tok { env = T }\n\
                     remote_kind nas {\n param host: string\n param token: secret\n via = http\n url = https://{{ host }}/api\n\
                     headers { Authorization = Bearer {{ token }}; X_Trace = 1 }\n\
                     action stop { param vm: string\n post = /vms/{{ vm }}/stop\n body = {\"why\": \"{{ vm }}\"} }\n}\n\
                     remote n1: nas { host = nas.lan; token = tok }\n");
        let r = Remotes::from_doc(&d, Path::new("/s/state"));
        let b = block(&d, "action", "n1.stop");
        let mut ctx = ctx_for(&r, b, &[("tok", "s3cret")]);
        ctx.insert("vm".into(), Value::String("../x?y\nz".into()));
        let req = r.build(&r.by_name["n1"], b, &ctx, Duration::from_secs(10), false).unwrap().unwrap();
        assert_eq!(req.argv, ["curl", "-q", "-K", "-"]);
        assert!(!req.argv.iter().any(|a| a.contains("s3cret")));
        let cfg = String::from_utf8(req.stdin.unwrap()).unwrap();
        assert!(cfg.contains("url = \"https://nas.lan/api/vms/..%2Fx%3Fy%0Az/stop\"\n"), "{cfg}");
        assert!(cfg.contains("request = \"POST\"\n"));
        assert!(cfg.contains("header = \"Authorization: Bearer s3cret\"\n"));
        assert!(cfg.contains("header = \"X-Trace: 1\"\n"));
        assert!(cfg.contains("data-raw = \"{\\\"why\\\": \\\"../x?y\\nz\\\"}\"\n"), "{cfg}");
        assert!(cfg.contains("fail-with-body\n") && cfg.contains("path-as-is\n") && cfg.contains("max-time = 10\n"));
        // A header value with a line break is refused, not sent.
        let d2 = doc("remote_kind k {\n via = http\n url = http://x\n action v { param h: string\n get = /\n headers { A = {{ h }} } }\n}\nremote a: k { }\n");
        let r2 = Remotes::from_doc(&d2, Path::new("/s/state"));
        let b2 = block(&d2, "action", "a.v");
        let mut ctx2 = ctx_for(&r2, b2, &[]);
        ctx2.insert("h".into(), Value::String("v\r\nX-Evil: 1".into()));
        assert!(r2.build(&r2.by_name["a"], b2, &ctx2, Duration::from_secs(5), false).is_err());
    }

    #[test]
    fn a_page_name_cannot_redirect_the_connection() {
        // Review: a `repeat host in ...` around the button put its own
        // `host` into the call's args, and the token went there.
        let d = doc("secret tok { env = T }\n\
                     remote_kind k {\n param host: string\n param token: secret\n via = http\n url = http://{{ host }}/api\n\
                     headers { Authorization = Bearer {{ token }} }\n action stop { post = /stop } }\n\
                     remote h1: k { host = 127.0.0.1; token = tok }\n");
        let r = Remotes::from_doc(&d, Path::new("/s/state"));
        let b = block(&d, "action", "h1.stop");
        let mut ctx = Context::new();
        ctx.insert("secrets".into(), Value::Map(BTreeMap::from([("tok".into(), Value::String("s".into()))])));
        ctx.insert("host".into(), Value::String("evil.example".into()));
        ctx.insert("token".into(), Value::String("x".into()));
        let req = r.build(&r.by_name["h1"], b, &ctx, Duration::from_secs(5), false).unwrap().unwrap();
        let cfg = String::from_utf8(req.stdin.unwrap()).unwrap();
        assert!(cfg.contains("url = \"http://127.0.0.1/api/stop\""), "{cfg}");
        assert!(cfg.contains("Authorization: Bearer s\""), "{cfg}");
        assert!(cfg.contains("globoff\n"));
    }

    #[test]
    fn quoted_values_lose_quotes_and_escapes_and_member_headers_replace() {
        let d = doc("remote_kind k {\n via = http\n url = http://x\n headers { Accept = a; X_A = 1 }\n\
                     action a { post = /p\n body = \"{\\\"why\\\": \\\"a;b\\\"}\"\n headers { accept = b } } }\n\
                     remote r: k { }\n");
        let r = Remotes::from_doc(&d, Path::new("/s/state"));
        let b = block(&d, "action", "r.a");
        let req = r.build(&r.by_name["r"], b, &Context::new(), Duration::from_secs(5), false).unwrap().unwrap();
        let cfg = String::from_utf8(req.stdin.unwrap()).unwrap();
        assert!(cfg.contains("data-raw = \"{\\\"why\\\": \\\"a;b\\\"}\"\n"), "{cfg}");
        assert!(cfg.contains("header = \"accept: b\"") && !cfg.contains("Accept: a"), "{cfg}");
        assert!(cfg.contains("header = \"X-A: 1\""), "{cfg}");
    }

    /// Tests that install a remote set hold this: the set is global.
    static INSTALLED: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn a_member_of_a_removed_remote_does_not_run_locally() {
        let _g = INSTALLED.lock().unwrap_or_else(|e| e.into_inner());
        let d = doc("remote_kind k { via = ssh\n host = h\n key = /k\n action reboot { run = reboot } }\nremote gone: k { }\n");
        let b = block(&d, "action", "gone.reboot").clone();
        install(Remotes::default());
        let e = request(&b, &Context::new(), Duration::from_secs(1), false).unwrap_err();
        assert!(e.contains("no longer exists"), "{e}");
    }

    #[test]
    fn socket_sends_on_stdin() {
        let d = doc("remote_kind k { via = socket; path = /run/x.sock\n value v { send = status } }\nremote s: k { }\n");
        let r = Remotes::from_doc(&d, Path::new("/s/state"));
        let b = block(&d, "source", "s.v");
        let req = r.build(&r.by_name["s"], b, &ctx_for(&r, b, &[]), Duration::from_secs(3), false).unwrap().unwrap();
        assert_eq!(req.argv, ["socat", "-T", "2", "-t", "2", "-", "UNIX-CONNECT:/run/x.sock"]);
        assert_eq!(req.stdin.as_deref(), Some(&b"status"[..]));
    }

    #[test]
    fn roots_group_member_values() {
        let d = doc("remote_kind k { via = local\n value a { run = true }\n value b { run = true }\n action x { run = true } }\nremote r1: k { }\n");
        let r = Remotes::from_doc(&d, Path::new("/s/state"));
        let ok = |v: i64| Value::Map(BTreeMap::from([("value".into(), Value::Int(v)), ("ok".into(), Value::Bool(true)), ("at".into(), Value::Int(1))]));
        let mut sources = BTreeMap::from([("r1.a".to_string(), ok(5))]);
        let root = |s: &BTreeMap<String, Value>| match r.roots(s).remove(0).1 {
            Value::Map(m) => m,
            _ => panic!(),
        };
        let m = root(&sources);
        assert_eq!(m["a"], Value::Int(5));
        assert_eq!(m["b"], Value::Null);
        assert_eq!(m["ok"], Value::Bool(true));
        assert!(!m.contains_key("x"));
        sources.insert(
            "r1.b".into(),
            Value::Map(BTreeMap::from([("ok".into(), Value::Bool(false)), ("error".into(), Value::String("boom".into())), ("at".into(), Value::Int(2))])),
        );
        let m = root(&sources);
        assert_eq!(m["ok"], Value::Bool(false));
        assert_eq!(m["error"], Value::String("b: boom".into()));
        // Nothing has a result yet: not ok, and no error.
        assert_eq!(root(&BTreeMap::new())["ok"], Value::Bool(false));
        assert_eq!(root(&BTreeMap::new())["error"], Value::String(String::new()));
        // A value that never succeeded and then failed has an error.
        let failed = BTreeMap::from([(
            "r1.a".to_string(),
            Value::Map(BTreeMap::from([("ok".into(), Value::Bool(false)), ("error".into(), Value::String("timed out".into()))])),
        )]);
        assert_eq!(root(&failed)["error"], Value::String("a: timed out".into()));
    }

    fn built(src: &str, kind: &str, name: &str, ctx_extra: &[(&str, Value)]) -> Result<Option<Request>, String> {
        let d = doc(src);
        let r = Remotes::from_doc(&d, Path::new("/s/state"));
        let b = block(&d, kind, name);
        let mut ctx = ctx_for(&r, b, &[]);
        for (k, v) in ctx_extra {
            ctx.insert(k.to_string(), v.clone());
        }
        let inst = cr::remote_of(b).unwrap();
        r.build(&r.by_name[inst], b, &ctx, Duration::from_secs(5), false)
    }

    #[test]
    fn ssh_refuses_bad_users_ports_and_paths() {
        let kind = |extra: &str| {
            format!("remote_kind k {{\n param v: string\n via = ssh\n host = h\n key = /k\n {extra}\n value x {{ run = true }}\n}}\nremote r: k {{ v = a }}\n")
        };
        let err = |extra: &str| built(&kind(extra), "source", "r.x", &[]).unwrap_err();
        assert!(err("user = -oX").contains("not a user name"));
        assert!(err("user = a@b").contains("not a user name"));
        assert!(err("port = 99999").contains("not a port"));
        assert!(err("known_hosts = \"/a b\"").contains("no spaces"));
        assert!(err("key = \"\"").contains("no spaces"));
        // `%` in a path is escaped for ssh, and accept_new maps.
        let req = built(&kind("key = /k%h\n host_key = accept_new\n port = 2222"), "source", "r.x", &[]).unwrap().unwrap();
        assert!(req.argv.contains(&"/k%%h".to_string()), "{:?}", req.argv);
        assert!(req.argv.contains(&"StrictHostKeyChecking=accept-new".to_string()));
        assert!(req.argv.windows(2).any(|w| w == ["-p", "2222"]));
        // No `-l` without a user.
        assert!(!req.argv.contains(&"-l".to_string()));
    }

    #[test]
    fn http_options_and_refusals() {
        let kind = |url: &str, extra: &str| {
            format!("remote_kind k {{\n via = http\n url = {url}\n {extra}\n value v {{ get = /s }}\n action a {{ }}\n}}\nremote r: k {{ }}\n")
        };
        let cfg = |r: Request| String::from_utf8(r.stdin.unwrap()).unwrap();
        let c = cfg(built(&kind("https://x/api/", "ca = /etc/ca.pem\n insecure = true"), "source", "r.v", &[]).unwrap().unwrap());
        assert!(c.contains("url = \"https://x/api/s\"\n"), "one slash between: {c}");
        assert!(c.contains("cacert = \"/etc/ca.pem\"\n") && c.contains("insecure\n"), "{c}");
        assert!(built(&kind("ftp://x", ""), "source", "r.v", &[]).unwrap_err().contains("not an http(s) URL"));
        assert!(built(&kind("\"http://a b\"", ""), "source", "r.v", &[]).unwrap_err().contains("not an http(s) URL"));
        // An action with no request has nothing to run.
        assert_eq!(built(&kind("http://x", ""), "action", "r.a", &[]).unwrap(), None);
        // A stream has no overall time limit.
        let d = doc(&kind("http://x", ""));
        let r = Remotes::from_doc(&d, Path::new("/s/state"));
        let b = block(&d, "source", "r.v");
        let req = r.build(&r.by_name["r"], b, &ctx_for(&r, b, &[]), Duration::from_secs(5), true).unwrap().unwrap();
        assert!(!cfg(req).contains("max-time"));
    }

    #[test]
    fn socket_over_tcp_and_its_refusals() {
        let kind = |addr: &str| format!("remote_kind k {{\n via = socket\n {addr}\n value v {{ send = ping }}\n}}\nremote r: k {{ }}\n");
        let req = built(&kind("host = [::1]\n port = 7000"), "source", "r.v", &[]).unwrap().unwrap();
        assert_eq!(req.argv.last().unwrap(), "TCP:[::1]:7000");
        assert!(built(&kind("host = h\n port = x"), "source", "r.v", &[]).unwrap_err().contains("not a port"));
        // A stream keeps reading after its request.
        let d = doc(&kind("path = /run/s"));
        let r = Remotes::from_doc(&d, Path::new("/s/state"));
        let b = block(&d, "source", "r.v");
        let req = r.build(&r.by_name["r"], b, &ctx_for(&r, b, &[]), Duration::from_secs(5), true).unwrap().unwrap();
        assert_eq!(&req.argv[1..3], ["-t", "2147483647"]);
        // Short timeouts: socat gives up first, in fractions of a second.
        let req = r.build(&r.by_name["r"], b, &ctx_for(&r, b, &[]), Duration::from_millis(500), false).unwrap().unwrap();
        assert_eq!(&req.argv[1..5], ["-T", "0.25", "-t", "0.25"]);
    }

    #[test]
    fn expressions_that_do_not_evaluate_are_errors_not_text() {
        let src = "remote_kind k {\n via = http\n url = http://x\n action a { param p: string\n post = /{{ p }}\n } }\nremote r: k { }\n";
        let e = built(src, "action", "r.a", &[]).unwrap_err();
        assert!(e.contains("did not evaluate"), "{e}");
        let ok = built(src, "action", "r.a", &[("p", Value::String("x".into()))]).unwrap().unwrap();
        assert!(String::from_utf8(ok.stdin.unwrap()).unwrap().contains("http://x/x"));
    }

    #[test]
    fn transport_failures_are_explained() {
        let _g = INSTALLED.lock().unwrap_or_else(|e| e.into_inner());
        let d = doc("remote_kind s { via = ssh\n host = h\n key = /k\n value v { run = x } }\nremote a: s { }\n\
                     remote_kind h { via = http\n url = http://x\n value v { get = / } }\nremote b: h { }\n\
                     remote_kind o { via = socket\n path = /p\n value v { send = x } }\nremote c: o { }\n\
                     remote_kind l { via = local\n value v { run = x } }\nremote e: l { }\n");
        install(Remotes::from_doc(&d, Path::new("/s/state")));
        let ex = |name: &str, code: i32, err: &str| explain(block(&d, "source", name), Some(code), err);
        assert_eq!(ex("a.v", 255, "Connection refused").as_deref(), Some("ssh could not run it on the remote: Connection refused"));
        assert_eq!(ex("a.v", 1, ""), None, "a command's own failure is its own");
        assert_eq!(ex("b.v", 22, "").as_deref(), Some("the server answered with an error status"));
        assert_eq!(ex("b.v", 6, "").as_deref(), Some("the host name did not resolve"));
        assert_eq!(ex("b.v", 7, "").as_deref(), Some("could not connect"));
        assert_eq!(ex("b.v", 28, "").as_deref(), Some("the request timed out"));
        assert_eq!(ex("b.v", 60, "cert").as_deref(), Some("TLS failed: cert"));
        assert_eq!(ex("c.v", 1, "").as_deref(), Some("socat failed"));
        assert_eq!(ex("e.v", 1, ""), None);
        install(Remotes::default());
    }
}