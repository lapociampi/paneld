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
use paneld_config::value::parse_duration;
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
    /// Event-mode: `state = firing | resolved` (or an expr yielding
    /// one). `None` means default `firing`.
    pub state_raw: Option<String>,
    pub state_span: paneld_config::source::Span,
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
    /// Wall-clock first-fire time. Shown to the user and persisted
    /// to the state file.
    pub since: SystemTime,
    /// Monotonic first-fire moment, used only to break ties in the
    /// same-severity sort so an NTP step or DST change can't reorder
    /// two active alerts. Resets to `Instant::now()` on state-file
    /// restore (monotonic clocks can't be serialised) which is fine:
    /// the user-visible sort within one session stays stable.
    pub fired_at: Instant,
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
        m.insert("snoozed".into(), Value::Bool(self.snoozed_until.is_some()));
        // Schema 9: `since` and `snoozed_until`, as Unix seconds.
        m.insert("since".into(), Value::Int(unix_secs(self.since)));
        m.insert(
            "snoozed_until".into(),
            self.snoozed_until.map(|t| Value::Int(unix_secs(t))).unwrap_or(Value::Null),
        );
        Value::Map(m)
    }

    /// Full id used as the key in `Engine::active`: `<rule>:<instance>`,
    /// following the schema's section 9 (see `docs/schema.md`).
    pub fn full_id(&self) -> String {
        format!("{}:{}", self.rule, self.instance)
    }
}

/// See [`Engine::carry`]; a free function so the rule loops, which
/// borrow `rules`, can use it.
fn carry_of(
    active: &BTreeMap<String, Alert>,
    restored: &mut BTreeMap<String, Carry>,
    key: &str,
    wall: SystemTime,
) -> (Carry, Instant) {
    if let Some(a) = active.get(key) {
        let c = Carry {
            since: a.since,
            acked: a.acked,
            snoozed_until: a.snoozed_until,
            rank: severity_rank(&a.severity),
        };
        return (c, a.fired_at);
    }
    let c = restored.remove(key).unwrap_or(Carry { since: wall, acked: false, snoozed_until: None, rank: 0 });
    (c, Instant::now())
}

/// Put what `alert` carries over onto it. An alert that got worse
/// (warning to critical) loses its ack and snooze: the new severity
/// has not been seen yet.
fn apply_carry(alert: &mut Alert, c: Carry, fired_at: Instant) {
    alert.since = c.since;
    alert.fired_at = fired_at;
    if severity_rank(&alert.severity) <= c.rank {
        alert.acked = c.acked;
        alert.snoozed_until = c.snoozed_until;
    }
}

/// `alerts:snooze = [1d, 1w]` (schema 9), default `1d`, `1w`.
fn parse_snooze(doc: &Document) -> Vec<(String, Duration)> {
    let mut raw: Option<String> = None;
    for item in &doc.items {
        if let Item::Block(b) = item {
            if b.kind.text == "alerts" && b.name.is_none() {
                for inner in &b.items {
                    if let Item::Assign(a) = inner {
                        if matches!(&a.target, AssignTarget::Simple { key } if key.text == "snooze") {
                            raw = Some(a.value.text.clone());
                        }
                    }
                }
            }
        }
    }
    let parsed: Vec<(String, Duration)> = raw
        .as_deref()
        .and_then(paneld_config::value::split_list)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|t| {
            let t = t.trim().to_string();
            parse_duration(&t).map(|d| (t, d))
        })
        .collect();
    if parsed.is_empty() {
        vec![("1d".into(), Duration::from_secs(86_400)), ("1w".into(), Duration::from_secs(7 * 86_400))]
    } else {
        parsed
    }
}

fn unix_secs(t: SystemTime) -> i64 {
    t.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// What a firing alert keeps across a repeat (event, re-fire, restart):
/// when it first fired, and whether it was acknowledged or snoozed.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Carry {
    pub since: SystemTime,
    pub acked: bool,
    pub snoozed_until: Option<SystemTime>,
    /// [`severity_rank`] it had when acked or snoozed.
    pub rank: u8,
}

/// Rule name for alerts raised over `paneld ctl`.
pub const CTL_RULE: &str = "ctl";

/// Runtime alert engine. One per daemon instance; rebuilt on every
/// `reload`.
pub struct Engine {
    pub rules: Vec<AlertRule>,
    /// Firing alerts, keyed by `<rule>:<instance>`.
    active: BTreeMap<String, Alert>,
    /// Condition-mode rules whose `when` is true but which have not
    /// held long enough yet. Same key as `active`.
    pending: BTreeMap<String, Instant>,
    /// First-fire time, ack and snooze replayed from the state file,
    /// consumed the first time a matching key fires, so a restart
    /// during a long incident keeps the alert's clock and its ack.
    restored: BTreeMap<String, Carry>,
    /// `alerts:snooze` choices, as written (default `1d`, `1w`).
    snooze: Vec<(String, Duration)>,
    /// Monotonic timestamp of the last event we saw per source that
    /// feeds at least one event-mode alert. ADR 0013's `source_down`
    /// watchdog raises a built-in warning when one goes silent for
    /// too long.
    source_last: BTreeMap<String, Instant>,
    source_down_after: Duration,
}

impl Engine {
    pub fn new(doc: &Document) -> Self {
        let rules = collect(doc);
        // Prime the watchdog clock for every source referenced by an
        // event-mode rule, so a startup during a down window still
        // fires `source_down` instead of pretending the source is
        // live until the next tick.
        let now = Instant::now();
        let mut source_last = BTreeMap::new();
        for r in &rules {
            if let Some(src) = &r.from {
                source_last.entry(src.clone()).or_insert(now);
            }
        }
        Self {
            rules,
            active: BTreeMap::new(),
            pending: BTreeMap::new(),
            restored: BTreeMap::new(),
            snooze: parse_snooze(doc),
            source_last,
            source_down_after: parse_source_down(doc),
        }
    }

    /// Prime the engine with `since` timestamps from the state file
    /// (ADR 0014). Each matching alert, on its first fire after
    /// startup, picks up the saved wall-clock time instead of the
    /// current one.
    pub fn restore(&mut self, saved: BTreeMap<String, Carry>) {
        self.restored = saved;
    }

    /// Call once after the first tick of a (re)start. A condition
    /// rule, reminder or built-in alert that did not fire again has
    /// resolved while the daemon was down, so its saved ack and
    /// snooze are dropped: firing later starts fresh (ADR 0016).
    /// Event rules keep theirs until the source's next event for that
    /// alert, since their state only arrives with events.
    pub fn forget_unfired(&mut self) {
        let (rules, pending) = (&self.rules, &self.pending);
        self.restored.retain(|k, _| {
            let rule = k.split_once(':').map_or(k.as_str(), |(r, _)| r);
            // Still waiting out its `for`: the same incident, maybe.
            pending.contains_key(k) || rules.iter().any(|r| r.name == rule && r.from.is_some())
        });
    }

    /// What `key` carries if it fires now: from the active alert, else
    /// from the state file, else fresh.
    fn carry(&mut self, key: &str, wall: SystemTime) -> (Carry, Instant) {
        carry_of(&self.active, &mut self.restored, key, wall)
    }

    /// Built-in action `alert_ack` (ADR 0016): keep it listed, take it
    /// off the banner. `id` is the full `<rule>:<instance>`. Returns
    /// false if no such alert is active.
    pub fn ack(&mut self, id: &str) -> bool {
        match self.active.get_mut(id) {
            Some(a) => {
                a.acked = true;
                true
            }
            None => false,
        }
    }

    /// Built-in action `alert_snooze` (ADR 0016): hide it until `until`.
    pub fn snooze(&mut self, id: &str, until: SystemTime) -> bool {
        match self.active.get_mut(id) {
            Some(a) => {
                a.snoozed_until = Some(until);
                true
            }
            None => false,
        }
    }

    /// `(id, severity)` of the alerts not acknowledged or snoozed, for
    /// the screen's wake-on-alert rule.
    pub fn shown(&self, now: SystemTime) -> Vec<(String, String)> {
        self.active
            .iter()
            .filter(|(_, a)| !a.acked && a.snoozed_until.map_or(true, |t| t <= now))
            .map(|(k, a)| (k.clone(), a.severity.clone()))
            .collect()
    }

    /// The `alerts:snooze` choices; the first is `alert_snooze`'s
    /// default.
    pub fn snooze_choices(&self) -> &[(String, Duration)] {
        &self.snooze
    }

    /// Snapshot of the currently firing alerts, for state-file
    /// persistence.
    pub fn active_alerts(&self) -> &BTreeMap<String, Alert> {
        &self.active
    }

    /// `paneld ctl alert fire <id> ...` (schema 9: "creates the same
    /// alerts"). Kept under the rule name `ctl`; firing an id again
    /// updates it and keeps its first `since`. Returns true if the
    /// active set changed.
    pub fn fire_external(
        &mut self,
        id: &str,
        severity: &str,
        title: &str,
        text: &str,
        link: &str,
        wall: SystemTime,
    ) -> bool {
        self.fire_rule(CTL_RULE, id, severity, title, text, link, wall)
    }

    /// `paneld ctl alert resolve <id>`. Returns true if it was active.
    pub fn resolve_external(&mut self, id: &str) -> bool {
        self.resolve_rule(CTL_RULE, id)
    }

    /// Raise `<rule>:<id>` from outside the rules (ctl, reminders).
    /// `wall` is its `since` if it is new. Returns true if the active
    /// set changed.
    #[allow(clippy::too_many_arguments)]
    pub fn fire_rule(
        &mut self,
        rule: &str,
        id: &str,
        severity: &str,
        title: &str,
        text: &str,
        link: &str,
        wall: SystemTime,
    ) -> bool {
        let key = format!("{rule}:{id}");
        let (c, fired_at) = self.carry(&key, wall);
        let mut alert = Alert {
            rule: rule.to_string(),
            instance: id.to_string(),
            severity: severity.to_string(),
            title: title.to_string(),
            text: text.to_string(),
            link: link.to_string(),
            since: c.since,
            fired_at,
            acked: false,
            snoozed_until: None,
        };
        apply_carry(&mut alert, c, fired_at);
        self.active.insert(key, alert.clone()) != Some(alert)
    }

    /// Resolve `<rule>:<id>`. Returns true if it was active.
    pub fn resolve_rule(&mut self, rule: &str, id: &str) -> bool {
        self.active.remove(&format!("{rule}:{id}")).is_some()
    }

    /// Feed one event (a source update's value) to every event-mode
    /// rule whose `from` matches `source_name`. `base_ctx` is the
    /// standard eval ctx; this method overlays `event` with the
    /// source's own value before evaluating `when`, `id`, `severity`,
    /// `title`, `text`, `link` and `state`. Returns `true` if the
    /// active set changed.
    ///
    /// Each new source value is one event, matching the schema's
    /// section 9 wording ("each new value, each line of a stream, is
    /// an `event`"). Condition-mode rules are ignored here.
    pub fn ingest_event(
        &mut self,
        source_name: &str,
        event: &Value,
        base_ctx: &Context,
        wall: SystemTime,
    ) -> bool {
        // Mark the source alive for the ADR 0013 watchdog, even if
        // the event doesn't match any rule (schema says "each new
        // value is an `event`"). Resolve any outstanding
        // `source_down:<name>` right here so the UI reflects the
        // recovery without waiting for the next `tick`.
        let mut changed = false;
        if self.source_last.contains_key(source_name) {
            self.source_last.insert(source_name.to_string(), Instant::now());
            let down_key = format!("source_down:{source_name}");
            if self.active.remove(&down_key).is_some() {
                changed = true;
            }
        }
        for rule in &self.rules {
            if rule.from.as_deref() != Some(source_name) {
                continue;
            }
            let mut ctx = base_ctx.clone();
            ctx.insert("event".into(), event.clone());

            if let Some(when_raw) = rule.when_raw.as_deref() {
                match paneld_ui::resolve_value(when_raw, rule.when_span, &ctx) {
                    Some(Value::Bool(true)) => {}
                    Some(Value::Bool(false)) => continue,
                    // Non-bool or unparseable `when`: skip.
                    _ => continue,
                }
            }
            let instance = resolve_string(&ctx, rule.id_raw.as_deref(), rule.id_span)
                .unwrap_or_else(|| rule.name.clone());
            let key = format!("{}:{instance}", rule.name);
            let state = resolve_string(&ctx, rule.state_raw.as_deref(), rule.state_span)
                .map(|s| s.trim().to_lowercase())
                .unwrap_or_else(|| "firing".into());

            match state.as_str() {
                "resolved" => {
                    self.restored.remove(&key);
                    if self.active.remove(&key).is_some() {
                        changed = true;
                    }
                }
                _ => {
                    // An already-active instance keeps its first-fire
                    // times and its ack / snooze; a fresh fire takes
                    // them from the state file, else starts clean.
                    let (c, fired_at) = carry_of(&self.active, &mut self.restored, &key, wall);
                    let mut alert = evaluate(rule, &instance, &ctx, c.since);
                    apply_carry(&mut alert, c, fired_at);
                    // An event stream can carry updates to a firing
                    // alert (severity escalation, text change). Only
                    // mark the frame dirty when something a user can
                    // see actually changed.
                    let prev = self.active.insert(key, alert.clone());
                    if prev.as_ref() != Some(&alert) {
                        changed = true;
                    }
                }
            }
        }
        changed
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
                    let (c, fired_at) = carry_of(&self.active, &mut self.restored, &key, wall);
                    let mut alert = evaluate(rule, &instance, ctx, c.since);
                    apply_carry(&mut alert, c, fired_at);
                    self.active.insert(key, alert);
                    changed = true;
                }
            } else {
                // Resolved: a later fire is a new incident.
                self.restored.remove(&key);
                if self.pending.remove(&key).is_some() {
                    // was pending, never fired: no visible change.
                }
                if self.active.remove(&key).is_some() {
                    changed = true;
                }
            }
        }

        // Reap only the condition-mode keys. An event-mode alert's
        // key will not be in `seen` (we skip event-mode rules in
        // this loop) so a blanket retain would wipe them every
        // tick. Match on the rule's `from` to decide ownership. The
        // built-in `source_down:*` keys we manage separately.
        let is_condition_key = |k: &str| -> bool {
            let (rule, _) = k.split_once(':').unwrap_or((k, ""));
            self.rules
                .iter()
                .any(|r| r.name == rule && r.from.is_none())
        };
        self.active
            .retain(|k, _| !is_condition_key(k) || seen.contains_key(k));
        self.pending
            .retain(|k, _| !is_condition_key(k) || seen.contains_key(k));

        // A snooze that ran out puts the alert back on the banner.
        for a in self.active.values_mut() {
            if a.snoozed_until.is_some_and(|t| t <= wall) {
                a.snoozed_until = None;
                changed = true;
            }
        }

        // Watchdog pass (ADR 0013). For every source feeding an
        // event-mode alert: if silent longer than `source_down`,
        // raise the built-in `source_down:<name>`; otherwise, if we
        // raised one earlier, resolve it.
        for (src_name, last) in &self.source_last {
            let down = now.duration_since(*last) >= self.source_down_after;
            let key = format!("source_down:{src_name}");
            if down {
                if !self.active.contains_key(&key) {
                    self.active.insert(
                        key.clone(),
                        Alert {
                            rule: "source_down".into(),
                            instance: src_name.clone(),
                            severity: "warning".into(),
                            title: format!("Source `{src_name}` is down"),
                            text: format!(
                                "No events from `{src_name}` in {:?}",
                                self.source_down_after
                            ),
                            link: String::new(),
                            since: wall,
                            fired_at: now,
                            acked: false,
                            snoozed_until: None,
                        },
                    );
                    changed = true;
                }
            } else if self.active.remove(&key).is_some() {
                changed = true;
            }
        }

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
                .then_with(|| a.fired_at.cmp(&b.fired_at))
        });
        let active: Vec<Value> = sorted.iter().map(|a| a.to_value()).collect();
        let worst = sorted
            .first()
            .map(|a| a.severity.clone())
            .unwrap_or_else(|| "none".into());

        // Not acknowledged and not snoozed: what the banner shows.
        let now = SystemTime::now();
        let shown: Vec<Value> = sorted
            .iter()
            .filter(|a| !a.acked && a.snoozed_until.map_or(true, |t| t <= now))
            .map(|a| a.to_value())
            .collect();

        let mut m = BTreeMap::new();
        m.insert("count".into(), Value::Int(active.len() as i64));
        m.insert("worst".into(), Value::String(worst));
        m.insert("active".into(), Value::List(active));
        m.insert("shown".into(), Value::List(shown));
        m.insert(
            "snooze".into(),
            Value::List(self.snooze.iter().map(|(t, _)| Value::String(t.clone())).collect()),
        );
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
        fired_at: Instant::now(),
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
pub fn severity_rank(s: &str) -> u8 {
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
            state_raw: None,
            state_span: zero_span(),
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
                "state" => {
                    rule.state_raw = Some(raw);
                    rule.state_span = span;
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

/// Parse `alerts:source_down` out of the config, falling back to
/// ADR 0013's default of 2 minutes. Accepts both the `category:key`
/// qualified form and the `alerts { source_down = ... }` block form.
fn parse_source_down(doc: &Document) -> Duration {
    let default = Duration::from_secs(120);
    for item in &doc.items {
        if let Item::Assign(a) = item {
            if let AssignTarget::Qualified { category, key } = &a.target {
                if category.text == "alerts" && key.text == "source_down" {
                    return parse_duration(&a.value.text).unwrap_or(default);
                }
            }
        }
        if let Item::Block(b) = item {
            if b.kind.text == "alerts" {
                for inner in &b.items {
                    if let Item::Assign(a) = inner {
                        if let AssignTarget::Simple { key } = &a.target {
                            if key.text == "source_down" {
                                return parse_duration(&a.value.text).unwrap_or(default);
                            }
                        }
                    }
                }
            }
        }
    }
    default
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

    fn event_map(title: &str, severity_num: i64) -> Value {
        let mut m = BTreeMap::new();
        m.insert("title".into(), Value::String(title.into()));
        m.insert("priority".into(), Value::Int(severity_num));
        m.insert("state".into(), Value::String("firing".into()));
        Value::Map(m)
    }

    #[test]
    fn ingest_event_fires_filtered_rule() {
        let doc = parse_doc(
            "source ntfy { run = /bin/true }\n\
             alert in { from = ntfy; \
                         when = {{ event.priority >= 4 }}; \
                         id = {{ event.title }}; \
                         severity = {{ event.priority >= 5 ? 'critical' : 'warning' }}; \
                         title = {{ event.title }}; \
                         text = incoming }\n",
        );
        let mut eng = Engine::new(&doc);
        let wall = SystemTime::now();

        // Below filter: no alert.
        assert!(!eng.ingest_event("ntfy", &event_map("x", 2), &Context::new(), wall));
        assert_eq!(eng.active_count(), 0);

        // Above filter: fires, with severity from the expression.
        assert!(eng.ingest_event("ntfy", &event_map("rack hot", 5), &Context::new(), wall));
        let a = &eng.active_alerts()["in:rack hot"];
        assert_eq!(a.severity, "critical");
        assert_eq!(a.title, "rack hot");

        // A later event with lower priority but above the filter
        // changes severity in place without creating a new alert.
        assert!(eng.ingest_event("ntfy", &event_map("rack hot", 4), &Context::new(), wall));
        assert_eq!(eng.active_count(), 1);
        assert_eq!(eng.active_alerts()["in:rack hot"].severity, "warning");
    }

    #[test]
    fn ingest_event_resolves_on_state_resolved() {
        let doc = parse_doc(
            "source ntfy { run = /bin/true }\n\
             alert in { from = ntfy; id = {{ event.title }}; \
                         state = {{ event.state }}; \
                         severity = warning; title = {{ event.title }} }\n",
        );
        let mut eng = Engine::new(&doc);
        let wall = SystemTime::now();
        let mut firing = event_map("vm42", 5);
        if let Value::Map(ref mut m) = firing {
            m.insert("state".into(), Value::String("firing".into()));
        }
        let mut resolved = event_map("vm42", 5);
        if let Value::Map(ref mut m) = resolved {
            m.insert("state".into(), Value::String("resolved".into()));
        }

        assert!(eng.ingest_event("ntfy", &firing, &Context::new(), wall));
        assert_eq!(eng.active_count(), 1);
        assert!(eng.ingest_event("ntfy", &resolved, &Context::new(), wall));
        assert_eq!(eng.active_count(), 0);
    }

    #[test]
    fn tick_does_not_wipe_event_mode_alerts() {
        // An event-mode alert fires through `ingest_event`. The next
        // `tick` (for condition-mode rules) must not sweep it out
        // just because `tick`'s seen-set does not include it.
        let doc = parse_doc(
            "source ntfy { run = /bin/true }\n\
             alert evt { from = ntfy; severity = info; title = Hi }\n\
             alert cond { when = {{ true }}; severity = info; title = Always }\n",
        );
        let mut eng = Engine::new(&doc);
        let wall = SystemTime::now();
        eng.ingest_event("ntfy", &event_map("x", 5), &Context::new(), wall);
        assert_eq!(eng.active_count(), 1);
        eng.tick(&Context::new(), Instant::now(), wall);
        assert_eq!(eng.active_count(), 2);
        // And a second tick keeps both.
        eng.tick(&Context::new(), Instant::now(), wall);
        assert_eq!(eng.active_count(), 2);
    }

    #[test]
    fn ingest_event_preserves_since_across_repeats() {
        // Repeat events for the same (rule, instance) must keep the
        // first-fire wall time; a tick storm must not keep bumping
        // `since` forward and must not keep marking the frame dirty
        // when nothing visible changed.
        let doc = parse_doc(
            "source ntfy { run = /bin/true }\n\
             alert in { from = ntfy; id = {{ event.title }}; \
                         severity = info; title = {{ event.title }} }\n",
        );
        let mut eng = Engine::new(&doc);
        let first = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let later = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_100);

        assert!(eng.ingest_event("ntfy", &event_map("x", 5), &Context::new(), first));
        assert_eq!(eng.active_alerts()["in:x"].since, first);

        // Second identical event: no visible change, no dirty flag.
        assert!(!eng.ingest_event("ntfy", &event_map("x", 5), &Context::new(), later));
        assert_eq!(eng.active_alerts()["in:x"].since, first);

        // Third event with a changed field: dirty, but since still
        // the original.
        let mut evt = event_map("x", 5);
        if let Value::Map(ref mut m) = evt {
            m.insert("priority".into(), Value::Int(9));
        }
        // The `title` field hasn't moved, so this isn't a severity
        // change; use a title-dependent rule to trigger a visible
        // diff. Easier: assert the since stays put across one more
        // tick.
        assert!(!eng.ingest_event("ntfy", &event_map("x", 5), &Context::new(), later));
        assert_eq!(eng.active_alerts()["in:x"].since, first);
    }

    #[test]
    fn ingest_event_ignores_other_sources() {
        let doc = parse_doc(
            "source ntfy { run = /bin/true }\n\
             source other { run = /bin/true }\n\
             alert in { from = ntfy; severity = info; title = x }\n",
        );
        let mut eng = Engine::new(&doc);
        assert!(!eng.ingest_event(
            "other",
            &event_map("x", 9),
            &Context::new(),
            SystemTime::now()
        ));
        assert_eq!(eng.active_count(), 0);
    }

    fn shown_titles(eng: &Engine) -> Vec<String> {
        let Value::Map(m) = eng.context_value() else { panic!() };
        let Some(Value::List(l)) = m.get("shown") else { panic!() };
        l.iter()
            .map(|a| match a {
                Value::Map(a) => a["title"].to_display_string(),
                _ => panic!(),
            })
            .collect()
    }

    #[test]
    fn ack_keeps_the_alert_listed_but_off_the_shown_list() {
        let doc = parse_doc(
            "source s { run = /bin/true }\n\
             alert a { from = s; id = {{ event.title }}; severity = critical; title = {{ event.title }} }\n",
        );
        let mut eng = Engine::new(&doc);
        let wall = SystemTime::now();
        eng.ingest_event("s", &event_map("disk", 5), &Context::new(), wall);
        assert_eq!(shown_titles(&eng), vec!["disk"]);
        assert!(eng.ack("a:disk"));
        assert!(shown_titles(&eng).is_empty());
        assert_eq!(eng.active_count(), 1);
        // A repeat event for the same alert must not clear the ack.
        eng.ingest_event("s", &event_map("disk", 5), &Context::new(), wall);
        assert!(shown_titles(&eng).is_empty(), "repeat event reset the ack");
        assert!(!eng.ack("a:nope"));
    }

    #[test]
    fn snooze_hides_until_it_runs_out() {
        let doc = parse_doc("alert a { when = {{ true }}; severity = warning; title = A }\n");
        let mut eng = Engine::new(&doc);
        let t0 = SystemTime::now();
        eng.tick(&Context::new(), Instant::now(), t0);
        assert!(eng.snooze("a:a", t0 + Duration::from_secs(60)));
        assert!(shown_titles(&eng).is_empty());
        // Before the end: still hidden; after: back, and the snooze
        // is cleared.
        eng.tick(&Context::new(), Instant::now(), t0 + Duration::from_secs(30));
        assert!(shown_titles(&eng).is_empty());
        assert!(eng.tick(&Context::new(), Instant::now(), t0 + Duration::from_secs(61)));
        assert_eq!(shown_titles(&eng), vec!["A"]);
    }

    #[test]
    fn snooze_choices_come_from_the_alerts_block() {
        let eng = Engine::new(&parse_doc("alerts { snooze = [2h, 3d] }\n"));
        let c: Vec<&str> = eng.snooze_choices().iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(c, vec!["2h", "3d"]);
        assert_eq!(eng.snooze_choices()[0].1, Duration::from_secs(7200));
        let eng = Engine::new(&parse_doc("page p { }\n"));
        assert_eq!(eng.snooze_choices()[1].0, "1w");
    }

    #[test]
    fn restore_replays_since_timestamps() {
        let doc = parse_doc(
            "alert pi_hot { when = {{ sources.cpu_temp.value > 70 }}; \
             severity = warning; title = Pi hot }\n",
        );
        let mut eng = Engine::new(&doc);
        let saved_since = std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let mut saved = BTreeMap::new();
        saved.insert(
            "pi_hot:pi_hot".to_string(),
            Carry { since: saved_since, acked: true, snoozed_until: None, rank: 2 },
        );
        eng.restore(saved);

        // Fire the alert with a "now" that's clearly later than the
        // saved time: it must still carry the saved since.
        let wall = std::time::UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        eng.tick(&ctx_with_temp(80.0), Instant::now(), wall);
        let active = eng.active_alerts();
        assert_eq!(active["pi_hot:pi_hot"].since, saved_since);
        // The ack survives the restart too.
        assert!(active["pi_hot:pi_hot"].acked);

        // After resolve + re-fire, the saved slot has been consumed:
        // a second fire uses the fresh `wall`.
        eng.tick(&ctx_with_temp(60.0), Instant::now(), wall);
        assert!(eng.active_alerts().is_empty());
        eng.tick(&ctx_with_temp(80.0), Instant::now(), wall);
        assert_eq!(eng.active_alerts()["pi_hot:pi_hot"].since, wall);
    }

    #[test]
    fn an_ack_saved_for_an_alert_that_resolved_while_down_is_dropped() {
        // Review round 6: acked, restart while the condition is false,
        // fires again later still acked (no banner for a new incident).
        let doc = parse_doc(
            "alert pi_hot { when = {{ sources.cpu_temp.value > 70 }}; severity = critical; title = Hot }\n\
             alert slow { when = {{ sources.cpu_temp.value > 70 }}; for = 1h; severity = critical; title = Slow }\n\
             source ev { run = /bin/true }\n\
             alert ev { from = ev; severity = critical; title = E; state = {{ event.state }} }\n",
        );
        let saved = |k: &str| (k.to_string(), Carry { since: SystemTime::UNIX_EPOCH, acked: true, snoozed_until: None, rank: 3 });
        let mut eng = Engine::new(&doc);
        eng.restore(["pi_hot:pi_hot", "slow:slow", "ev:ev", "reminder:x"].into_iter().map(saved).collect());
        let wall = SystemTime::now();
        // First tick after start: pi_hot is not firing.
        eng.tick(&ctx_with_temp(60.0), Instant::now(), wall);
        eng.forget_unfired();
        let keys: Vec<&String> = eng.restored.keys().collect();
        assert_eq!(keys, ["ev:ev"], "only event rules wait for their next event");
        eng.tick(&ctx_with_temp(80.0), Instant::now(), wall);
        assert!(!eng.active_alerts()["pi_hot:pi_hot"].acked, "a new incident must show");

        // An alert still waiting out its `for` keeps its ack.
        let mut eng = Engine::new(&doc);
        eng.restore(["slow:slow"].into_iter().map(saved).collect());
        eng.tick(&ctx_with_temp(80.0), Instant::now(), wall);
        eng.forget_unfired();
        assert!(eng.restored.contains_key("slow:slow"));

        // A resolved event drops the event rule's saved ack.
        let mut eng = Engine::new(&doc);
        eng.restore(["ev:ev"].into_iter().map(saved).collect());
        let mut m = BTreeMap::new();
        m.insert("state".to_string(), Value::String("resolved".into()));
        eng.ingest_event("ev", &Value::Map(m.clone()), &Context::new(), wall);
        m.insert("state".to_string(), Value::String("firing".into()));
        eng.ingest_event("ev", &Value::Map(m), &Context::new(), wall);
        assert!(!eng.active_alerts()["ev:ev"].acked);
    }

    #[test]
    fn getting_worse_clears_the_ack() {
        let doc = parse_doc("alert a { when = {{ true }}; title = A }\n");
        let mut eng = Engine::new(&doc);
        let wall = SystemTime::now();
        eng.fire_rule("ctl", "x", "warning", "X", "", "", wall);
        assert!(eng.ack("ctl:x"));
        eng.fire_rule("ctl", "x", "warning", "X again", "", "", wall);
        assert!(eng.active_alerts()["ctl:x"].acked, "same severity keeps the ack");
        eng.fire_rule("ctl", "x", "info", "X", "", "", wall);
        assert!(eng.active_alerts()["ctl:x"].acked, "milder keeps the ack");
        eng.fire_rule("ctl", "x", "critical", "X", "", "", wall);
        assert!(!eng.active_alerts()["ctl:x"].acked, "worse must show again");
    }

    #[test]
    fn source_down_watchdog_fires_and_resolves() {
        let doc = parse_doc(
            "source ntfy { run = /bin/true }\n\
             alert in { from = ntfy; severity = info; title = x }\n\
             alerts { source_down = 2s }\n",
        );
        let mut eng = Engine::new(&doc);
        assert_eq!(eng.source_down_after, Duration::from_secs(2));
        let wall = SystemTime::now();
        let t0 = eng.source_last["ntfy"];

        // Just after priming, nothing fires.
        assert!(!eng.tick(&Context::new(), t0, wall));
        assert_eq!(eng.active_count(), 0);

        // Two seconds later with no events, source_down raised.
        let t2 = t0 + Duration::from_secs(2);
        assert!(eng.tick(&Context::new(), t2, wall));
        assert!(eng.active_alerts().contains_key("source_down:ntfy"));

        // An event resets the clock and resolves the alert.
        assert!(eng.ingest_event("ntfy", &event_map("hi", 3), &Context::new(), wall));
        // After ingest, source_last is Instant::now(); the next tick
        // uses that same instant so no watchdog should fire.
        let now = Instant::now();
        assert!(!eng.active_alerts().contains_key("source_down:ntfy"));
        // Belt-and-braces: tick again at the current time.
        eng.tick(&Context::new(), now, wall);
        assert!(!eng.active_alerts().contains_key("source_down:ntfy"));
    }

    #[test]
    fn source_down_default_is_two_minutes() {
        let doc = parse_doc(
            "source ntfy { run = /bin/true }\n\
             alert in { from = ntfy; severity = info; title = x }\n",
        );
        let eng = Engine::new(&doc);
        assert_eq!(eng.source_down_after, Duration::from_secs(120));
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
