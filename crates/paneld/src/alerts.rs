//! Collect top-level `alert NAME { ... }` blocks and evaluate the
//! condition-mode ones every tick. An alert fires when its `when`
//! expression has stayed true for `for` (default: immediately), and
//! resolves on the next tick when `when` goes false. The engine
//! exposes an `alerts` map (`active`, `count`, `worst`) to the eval
//! context so pages can read it.
//!
//! Scope today: condition-mode only (`when` + optional `for`). Event
//! mode (`from = <source>`), ack/snooze, the state file (ADR 0014)
//! and the schema's `source_down` watcher come in later phases.
//! `title`, `text`, `link` and `severity` are re-evaluated on every
//! transition so interpolated values (`{{ round(sources.cpu_temp
//! .value, 1) }} C`) reflect the triggering sample.

use std::collections::BTreeMap;
use std::time::{Duration, Instant, SystemTime};

use paneld_config::parse::{AssignTarget, Document, Item};
use paneld_ui::{Context, Value};

/// One parsed `alert NAME { ... }` block, held raw so each tick can
/// re-interpolate `title`/`text`/`link` against the current ctx.
#[derive(Clone, Debug)]
pub struct AlertRule {
    pub name: String,
    /// Event-mode source. Condition-mode rules leave this `None`.
    pub from: Option<String>,
    /// Raw `when` text (with `{{ }}` still in place).
    pub when_raw: Option<String>,
    pub when_span: paneld_config::source::Span,
    /// `for` holding period; condition-mode only. 0 means "fire on
    /// the first tick where `when` is true".
    pub for_: Duration,
    pub id_raw: Option<String>,
    pub id_span: paneld_config::source::Span,
    pub severity_raw: String,
    pub severity_span: paneld_config::source::Span,
    pub title_raw: String,
    pub title_span: paneld_config::source::Span,
    pub text_raw: String,
    pub text_span: paneld_config::source::Span,
    pub link_raw: String,
    pub link_span: paneld_config::source::Span,
    pub action: Option<String>,
}

/// A firing alert, as held by the engine and exposed through the
/// `alerts.active` list to the rest of the UI.
#[derive(Clone, Debug, PartialEq)]
pub struct Alert {
    /// The rule that produced this alert.
    pub rule: String,
    /// Instance id: the evaluated `id` expression, or the rule name
    /// when none is set. One rule can track many alerts by setting
    /// `id` to a distinguishing expression.
    pub instance: String,
    pub severity: String,
    pub title: String,
    pub text: String,
    pub link: String,
    pub since: SystemTime,
    pub acked: bool,
    pub snoozed_until: Option<SystemTime>,
}

impl Alert {
    fn to_value(&self) -> Value {
        let mut m = BTreeMap::new();
        m.insert("id".into(), Value::String(self.full_id()));
        m.insert("rule".into(), Value::String(self.rule.clone()));
        m.insert("instance".into(), Value::String(self.instance.clone()));
        m.insert("severity".into(), Value::String(self.severity.clone()));
        m.insert("title".into(), Value::String(self.title.clone()));
        m.insert("text".into(), Value::String(self.text.clone()));
        m.insert("link".into(), Value::String(self.link.clone()));
        m.insert("acked".into(), Value::Bool(self.acked));
        m.insert(
            "snoozed".into(),
            Value::Bool(self.snoozed_until.is_some()),
        );
        Value::Map(m)
    }

    /// Full id used as the key in `Engine::active`: `<rule>:<instance>`,
    /// following the schema's section 9 (see `docs/schema.md`).
    pub fn full_id(&self) -> String {
        format!("{}:{}", self.rule, self.instance)
    }
}

/// Runtime alert engine. One per daemon instance; rebuilt on every
/// `reload`.
pub struct Engine {
    pub rules: Vec<AlertRule>,
    /// Firing alerts, keyed by `<rule>:<instance>`.
    active: BTreeMap<String, Alert>,
    /// Condition-mode rules whose `when` is true but which have not
    /// held long enough yet. Same key as `active`.
    pending: BTreeMap<String, Instant>,
}

impl Engine {
    pub fn new(doc: &Document) -> Self {
        Self {
            rules: collect(doc),
            active: BTreeMap::new(),
            pending: BTreeMap::new(),
        }
    }

    /// Evaluate every condition-mode rule against `ctx` and update
    /// the firing set. `now` is `Instant::now()` (for the `for`
    /// timer); `wall` is `SystemTime::now()` and gets stamped onto
    /// a newly firing alert's `since`.
    ///
    /// Returns `true` if the active set changed, so the main loop
    /// can trigger a re-render.
    pub fn tick(&mut self, ctx: &Context, now: Instant, wall: SystemTime) -> bool {
        let mut changed = false;
        let mut seen: BTreeMap<String, ()> = BTreeMap::new();

        for rule in &self.rules {
            // Event-mode rules do not fire from `tick`; they will be
            // driven by source updates in a later phase.
            if rule.from.is_some() {
                continue;
            }
            let Some(when_raw) = rule.when_raw.as_deref() else {
                continue;
            };
            let truthy = match paneld_ui::resolve_value(when_raw, rule.when_span, ctx) {
                Some(Value::Bool(b)) => b,
                Some(_) | None => continue,
            };
            let instance = resolve_string(ctx, rule.id_raw.as_deref(), rule.id_span)
                .unwrap_or_else(|| rule.name.clone());
            let key = format!("{}:{instance}", rule.name);
            seen.insert(key.clone(), ());

            if truthy {
                if self.active.contains_key(&key) {
                    continue;
                }
                let ready = match self.pending.get(&key) {
                    Some(start) => now.duration_since(*start) >= rule.for_,
                    None if rule.for_.is_zero() => true,
                    None => {
                        self.pending.insert(key.clone(), now);
                        false
                    }
                };
                if ready {
                    self.pending.remove(&key);
                    let alert = evaluate(rule, &instance, ctx, wall);
                    self.active.insert(key, alert);
                    changed = true;
                }
            } else {
                if self.pending.remove(&key).is_some() {
                    // was pending, never fired: no visible change.
                }
                if self.active.remove(&key).is_some() {
                    changed = true;
                }
            }
        }

        // A rule removed from the config on reload is also dropped
        // from `active`; the engine itself is rebuilt on reload so
        // this is defensive against a future in-place rule swap.
        self.active.retain(|k, _| seen.contains_key(k));
        self.pending.retain(|k, _| seen.contains_key(k));

        changed
    }

    /// Build the `alerts` value for the render context: `active`
    /// (list of alert maps, worst severity first), `count` (int) and
    /// `worst` (highest severity, or `"none"` when the list is empty).
    pub fn context_value(&self) -> Value {
        let mut sorted: Vec<&Alert> = self.active.values().collect();
        sorted.sort_by(|a, b| {
            severity_rank(&b.severity)
                .cmp(&severity_rank(&a.severity))
                .then_with(|| a.since.cmp(&b.since))
        });
        let active: Vec<Value> = sorted.iter().map(|a| a.to_value()).collect();
        let worst = sorted
            .first()
            .map(|a| a.severity.clone())
            .unwrap_or_else(|| "none".into());

        let mut m = BTreeMap::new();
        m.insert("count".into(), Value::Int(active.len() as i64));
        m.insert("worst".into(), Value::String(worst));
        m.insert("active".into(), Value::List(active));
        Value::Map(m)
    }

    #[cfg(test)]
    pub fn active_count(&self) -> usize {
        self.active.len()
    }
}

fn evaluate(
    rule: &AlertRule,
    instance: &str,
    ctx: &Context,
    wall: SystemTime,
) -> Alert {
    let severity =
        resolve_string(ctx, Some(&rule.severity_raw), rule.severity_span)
            .unwrap_or_else(|| "info".into());
    let title =
        resolve_string(ctx, Some(&rule.title_raw), rule.title_span).unwrap_or_default();
    let text =
        resolve_string(ctx, Some(&rule.text_raw), rule.text_span).unwrap_or_default();
    let link =
        resolve_string(ctx, Some(&rule.link_raw), rule.link_span).unwrap_or_default();
    Alert {
        rule: rule.name.clone(),
        instance: instance.to_string(),
        severity: severity.trim().to_string(),
        title,
        text,
        link,
        since: wall,
        acked: false,
        snoozed_until: None,
    }
}

/// Interpolate a `title`/`text`/`link`/`id`/`severity`-style field.
/// Returns `None` only for an empty or missing raw value, so callers
/// fall back to a default.
fn resolve_string(
    ctx: &Context,
    raw: Option<&str>,
    span: paneld_config::source::Span,
) -> Option<String> {
    let raw = raw?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(paneld_ui::interpolate(raw, span.file, span.start, ctx))
}

/// Rank for sorting: higher is worse. `"none"` is only used when the
/// list is empty.
fn severity_rank(s: &str) -> u8 {
    match s {
        "critical" => 3,
        "warning" => 2,
        "info" => 1,
        _ => 0,
    }
}

fn collect(doc: &Document) -> Vec<AlertRule> {
    let mut out = Vec::new();
    for item in &doc.items {
        let Item::Block(b) = item else { continue };
        if b.kind.text != "alert" {
            continue;
        }
        let Some(name) = &b.name else { continue };
        let mut rule = AlertRule {
            name: name.text.clone(),
            from: None,
            when_raw: None,
            when_span: zero_span(),
            for_: Duration::ZERO,
            id_raw: None,
            id_span: zero_span(),
            severity_raw: "info".into(),
            severity_span: zero_span(),
            title_raw: name.text.clone(),
            title_span: zero_span(),
            text_raw: String::new(),
            text_span: zero_span(),
            link_raw: String::new(),
            link_span: zero_span(),
            action: None,
        };
        for inner in &b.items {
            let Item::Assign(a) = inner else { continue };
            let AssignTarget::Simple { key } = &a.target else {
                continue;
            };
            let span = a.value.span;
            let raw = a.value.text.clone();
            match key.text.as_str() {
                "from" => rule.from = Some(raw.trim().to_string()),
                "when" => {
                    rule.when_raw = Some(raw);
                    rule.when_span = span;
                }
                "for" => {
                    rule.for_ = paneld_config::value::parse_duration(&raw)
                        .unwrap_or(Duration::ZERO);
                }
                "id" => {
                    rule.id_raw = Some(raw);
                    rule.id_span = span;
                }
                "severity" => {
                    rule.severity_raw = raw;
                    rule.severity_span = span;
                }
                "title" => {
                    rule.title_raw = raw;
                    rule.title_span = span;
                }
                "text" => {
                    rule.text_raw = raw;
                    rule.text_span = span;
                }
                "link" => {
                    rule.link_raw = raw;
                    rule.link_span = span;
                }
                "action" => rule.action = Some(raw.trim().to_string()),
                _ => {}
            }
        }
        out.push(rule);
    }
    out
}

fn zero_span() -> paneld_config::source::Span {
    paneld_config::source::Span {
        file: paneld_config::source::FileId::default(),
        start: 0,
        end: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_doc(src: &str) -> Document {
        use paneld_config::lex::tokenize;
        use paneld_config::source::SourceMap;
        use std::path::PathBuf;
        let mut map = SourceMap::new();
        let id = map.add(PathBuf::from("t.conf"), src.to_string());
        let (toks, lex_errs) = tokenize(id, src);
        assert!(lex_errs.is_empty(), "lex errors: {lex_errs:?}");
        let (doc, diags) = paneld_config::parse(&toks, &map);
        assert!(diags.is_empty(), "parse errors: {diags:?}");
        doc
    }

    fn ctx_with_temp(t: f64) -> Context {
        let mut sources = BTreeMap::new();
        let mut cpu = BTreeMap::new();
        cpu.insert("value".into(), Value::Double(t));
        sources.insert("cpu_temp".into(), Value::Map(cpu));
        let mut ctx = Context::new();
        ctx.insert("sources".into(), Value::Map(sources));
        ctx
    }

    #[test]
    fn collects_condition_mode_rule() {
        let doc = parse_doc(
            "alert pi_hot { when = {{ sources.cpu_temp.value > 70 }}; \
             severity = warning; title = Pi hot }\n",
        );
        let eng = Engine::new(&doc);
        assert_eq!(eng.rules.len(), 1);
        let r = &eng.rules[0];
        assert_eq!(r.name, "pi_hot");
        assert!(r.from.is_none());
        assert!(r.when_raw.as_ref().unwrap().contains("cpu_temp"));
        assert_eq!(r.for_, Duration::ZERO);
    }

    #[test]
    fn fires_when_true_without_for() {
        let doc = parse_doc(
            "alert pi_hot { when = {{ sources.cpu_temp.value > 70 }}; \
             severity = warning; title = Pi hot }\n",
        );
        let mut eng = Engine::new(&doc);
        let now = Instant::now();
        let wall = SystemTime::now();

        assert!(!eng.tick(&ctx_with_temp(60.0), now, wall));
        assert_eq!(eng.active_count(), 0);

        assert!(eng.tick(&ctx_with_temp(80.0), now, wall));
        assert_eq!(eng.active_count(), 1);

        assert!(eng.tick(&ctx_with_temp(50.0), now, wall));
        assert_eq!(eng.active_count(), 0);
    }

    #[test]
    fn holds_for_duration() {
        let doc = parse_doc(
            "alert pi_hot { when = {{ sources.cpu_temp.value > 70 }}; \
             for = 2s; severity = warning; title = Pi hot }\n",
        );
        let mut eng = Engine::new(&doc);
        assert_eq!(eng.rules[0].for_, Duration::from_secs(2));
        let t0 = Instant::now();
        let wall = SystemTime::now();

        // True but not held long enough.
        assert!(!eng.tick(&ctx_with_temp(80.0), t0, wall));
        assert_eq!(eng.active_count(), 0);

        // One second in, still not firing.
        let t1 = t0 + Duration::from_secs(1);
        assert!(!eng.tick(&ctx_with_temp(80.0), t1, wall));
        assert_eq!(eng.active_count(), 0);

        // Three seconds in, past `for`, now firing.
        let t2 = t0 + Duration::from_secs(3);
        assert!(eng.tick(&ctx_with_temp(80.0), t2, wall));
        assert_eq!(eng.active_count(), 1);
    }

    #[test]
    fn flapping_resets_pending() {
        let doc = parse_doc(
            "alert pi_hot { when = {{ sources.cpu_temp.value > 70 }}; \
             for = 2s; severity = warning; title = Pi hot }\n",
        );
        let mut eng = Engine::new(&doc);
        let t0 = Instant::now();
        let wall = SystemTime::now();

        assert!(!eng.tick(&ctx_with_temp(80.0), t0, wall));
        assert!(!eng.tick(&ctx_with_temp(60.0), t0 + Duration::from_secs(1), wall));

        // Back true, but the pending clock restarted.
        let t_back = t0 + Duration::from_secs(2);
        assert!(!eng.tick(&ctx_with_temp(80.0), t_back, wall));
        assert_eq!(eng.active_count(), 0);

        let t_later = t_back + Duration::from_secs(3);
        assert!(eng.tick(&ctx_with_temp(80.0), t_later, wall));
        assert_eq!(eng.active_count(), 1);
    }

    #[test]
    fn context_value_sorts_by_severity_then_age() {
        let doc = parse_doc(
            "alert a { when = {{ true }}; severity = info; title = A }\n\
             alert b { when = {{ true }}; severity = critical; title = B }\n\
             alert c { when = {{ true }}; severity = warning; title = C }\n",
        );
        let mut eng = Engine::new(&doc);
        eng.tick(&Context::new(), Instant::now(), SystemTime::now());
        let v = eng.context_value();
        let Value::Map(m) = v else { panic!("expected map") };
        assert_eq!(m.get("count"), Some(&Value::Int(3)));
        assert_eq!(m.get("worst"), Some(&Value::String("critical".into())));
        let Some(Value::List(active)) = m.get("active") else {
            panic!("expected list");
        };
        let titles: Vec<&str> = active
            .iter()
            .filter_map(|v| match v {
                Value::Map(m) => match m.get("title") {
                    Some(Value::String(s)) => Some(s.as_str()),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        assert_eq!(titles, vec!["B", "C", "A"]);
    }

    #[test]
    fn interpolates_text_at_fire_time() {
        let doc = parse_doc(
            "alert pi_hot { when = {{ sources.cpu_temp.value > 70 }}; \
             severity = warning; title = Pi hot; \
             text = {{ round(sources.cpu_temp.value, 1) }} C }\n",
        );
        let mut eng = Engine::new(&doc);
        eng.tick(&ctx_with_temp(72.345), Instant::now(), SystemTime::now());
        let v = eng.context_value();
        let Value::Map(m) = v else { panic!() };
        let Some(Value::List(list)) = m.get("active") else { panic!() };
        let Value::Map(a) = &list[0] else { panic!() };
        assert_eq!(a.get("text"), Some(&Value::String("72.3 C".into())));
    }

    #[test]
    fn event_mode_rules_are_skipped_in_tick() {
        let doc = parse_doc(
            "source ntfy { run = /bin/true }\n\
             alert incoming { from = ntfy; severity = info; title = {{ event.title }} }\n",
        );
        let mut eng = Engine::new(&doc);
        assert_eq!(eng.rules.len(), 1);
        assert_eq!(eng.rules[0].from.as_deref(), Some("ntfy"));
        // Even if some ctx somehow had `event`, condition-mode tick
        // must leave event-mode alerts alone.
        assert!(!eng.tick(&Context::new(), Instant::now(), SystemTime::now()));
        assert_eq!(eng.active_count(), 0);
    }

    #[test]
    fn for_accepts_fractional_and_unit_variants() {
        // Uses the shared `paneld_config::value::parse_duration`, so
        // the alert `for` key accepts everything the rest of the
        // schema does. The old local parser silently gave 0 for
        // `1.5s`, meaning the hold never kicked in.
        let doc = parse_doc(
            "alert a { when = {{ true }}; for = 1.5s; severity = info; title = A }\n",
        );
        let eng = Engine::new(&doc);
        assert_eq!(eng.rules[0].for_, Duration::from_millis(1_500));

        let doc = parse_doc(
            "alert a { when = {{ true }}; for = 2m; severity = info; title = A }\n",
        );
        let eng = Engine::new(&doc);
        assert_eq!(eng.rules[0].for_, Duration::from_secs(120));
    }
}
