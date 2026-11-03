//! Reminders (schema 10): dated chores that show up as alerts.
//!
//! A due reminder is an alert under the rule `reminder` with the
//! reminder's name as instance, so the banner, the alerts page, ack
//! and snooze all work as for any alert. The built-in action
//! `reminder_done` records the day and resolves it until the next one
//! is due. Due dates are local midnights; `every` in months or years
//! steps calendar months, anything else whole days (at least one).
//! When each reminder was first seen and last done lives in the state
//! file (ADR 0014).

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use paneld_config::parse::{AssignTarget, Document, Item};
use paneld_config::source::Span;
use paneld_ui::Context;

use crate::alerts;
use crate::state::SavedReminder;
use crate::tz::{self, Zone};

/// Rule name of reminder alerts.
pub const RULE: &str = "reminder";

type Date = (i64, u32, u32);

#[derive(Clone, Copy, Debug, PartialEq)]
enum Step {
    Days(i64),
    Months(i64),
}

impl Step {
    fn after(self, d: Date) -> Date {
        match self {
            Step::Days(n) => tz::add_days(d, n),
            Step::Months(n) => tz::add_months(d, n),
        }
    }

    /// `every` text: months and years step the calendar, the rest is
    /// rounded up to whole days.
    fn parse(raw: &str) -> Option<Self> {
        let t = raw.trim();
        let split = t.find(|c: char| !c.is_ascii_digit() && c != '.').unwrap_or(t.len());
        let n: f64 = t[..split].parse().ok()?;
        match t[split..].trim().to_ascii_lowercase().as_str() {
            "month" | "months" => Some(Step::Months((n.round() as i64).max(1))),
            "y" | "year" | "years" => Some(Step::Months((n.round() as i64).max(1) * 12)),
            _ => {
                let secs = paneld_config::value::parse_duration(t)?.as_secs();
                Some(Step::Days((secs.div_ceil(86_400) as i64).max(1)))
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Schedule {
    Every { step: Step, start: Option<Date> },
    Once(Date),
}

#[derive(Clone, Debug)]
struct Reminder {
    name: String,
    schedule: Schedule,
    severity: String,
    title: (String, Span),
    text: (String, Span),
    link: (String, Span),
}

/// The configured reminders and their dates.
pub struct Book {
    list: Vec<Reminder>,
    zone: Zone,
    dates: BTreeMap<String, SavedReminder>,
}

fn secs(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

impl Book {
    /// Reminders from `doc`, with dates from `saved` (the state file
    /// or the previous config). One seen for the first time counts
    /// from `now`; dates of reminders no longer configured are
    /// dropped.
    pub fn new(doc: &Document, zone: Zone, saved: &BTreeMap<String, SavedReminder>, now: SystemTime) -> Self {
        let list = collect(doc);
        let dates = list
            .iter()
            .map(|r| {
                let d = saved
                    .get(&r.name)
                    .cloned()
                    .unwrap_or(SavedReminder { first_seen: secs(now), done: None });
                (r.name.clone(), d)
            })
            .collect();
        Self { list, zone, dates }
    }

    /// First seen and last done, for the state file.
    pub fn dates(&self) -> &BTreeMap<String, SavedReminder> {
        &self.dates
    }

    /// When `r` is next due (Unix seconds), or `None` for a one-time
    /// reminder already done.
    fn due_at(&self, r: &Reminder) -> Option<i64> {
        let saved = self.dates.get(&r.name)?;
        let date = match r.schedule {
            Schedule::Once(_) if saved.done.is_some() => return None,
            Schedule::Once(d) => d,
            Schedule::Every { step, start } => {
                let next = saved.done.map(|t| step.after(self.zone.local_date(t as i64)));
                match (next, start) {
                    (Some(n), Some(s)) => n.max(s),
                    (Some(n), None) => n,
                    (None, Some(s)) => s,
                    (None, None) => step.after(self.zone.local_date(saved.first_seen as i64)),
                }
            }
        };
        Some(self.zone.local_midnight(date))
    }

    /// Raise the due reminders as alerts and resolve the others.
    /// Returns true if the alert set changed.
    pub fn tick(&self, engine: &mut alerts::Engine, ctx: &Context, now: SystemTime) -> bool {
        let now_s = secs(now) as i64;
        let mut changed = false;
        for r in &self.list {
            match self.due_at(r) {
                Some(due) if due <= now_s => {
                    let text = |(raw, span): &(String, Span)| paneld_ui::interpolate(raw, span.file, span.start, ctx);
                    let since = UNIX_EPOCH + Duration::from_secs(due.max(0) as u64);
                    changed |= engine.fire_rule(
                        RULE,
                        &r.name,
                        &r.severity,
                        &text(&r.title),
                        &text(&r.text),
                        &text(&r.link),
                        since,
                    );
                }
                _ => changed |= engine.resolve_rule(RULE, &r.name),
            }
        }
        changed
    }

    /// Built-in `reminder_done` (ADR 0016): record today as done.
    /// `id` is the reminder's name or its alert id (`reminder:<name>`).
    /// Returns false if no such reminder is due.
    pub fn done(&mut self, id: &str, now: SystemTime) -> bool {
        let name = id.strip_prefix("reminder:").unwrap_or(id);
        let Some(r) = self.list.iter().find(|r| r.name == name) else {
            return false;
        };
        if !self.due_at(r).is_some_and(|due| due <= secs(now) as i64) {
            return false;
        }
        match self.dates.get_mut(name) {
            Some(d) => {
                d.done = Some(secs(now));
                true
            }
            None => false,
        }
    }
}

fn collect(doc: &Document) -> Vec<Reminder> {
    let mut out = Vec::new();
    for item in &doc.items {
        let Item::Block(b) = item else { continue };
        if b.kind.text != "reminder" {
            continue;
        }
        let Some(name) = &b.name else { continue };
        let zero = name.span;
        let (mut every, mut start, mut once) = (None, None, None);
        let mut r = Reminder {
            name: name.text.clone(),
            schedule: Schedule::Once((1970, 1, 1)),
            severity: "info".into(),
            title: (name.text.clone(), zero),
            text: (String::new(), zero),
            link: (String::new(), zero),
        };
        for inner in &b.items {
            let Item::Assign(a) = inner else { continue };
            let AssignTarget::Simple { key } = &a.target else { continue };
            let v = (a.value.text.clone(), a.value.span);
            match key.text.as_str() {
                "every" => every = Step::parse(&v.0),
                "start" => start = tz::parse_date(&v.0),
                "once" => once = tz::parse_date(&v.0),
                "severity" => r.severity = v.0.trim().to_string(),
                "title" => r.title = v,
                "text" => r.text = v,
                "link" => r.link = v,
                _ => {}
            }
        }
        // `paneld check` rejects the rest; skip them at run time.
        r.schedule = match (every, once) {
            (Some(step), None) => Schedule::Every { step, start },
            (None, Some(d)) => Schedule::Once(d),
            _ => continue,
        };
        out.push(r);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(src: &str) -> Document {
        use paneld_config::lex::tokenize;
        use paneld_config::source::SourceMap;
        let mut map = SourceMap::new();
        let id = map.add(std::path::PathBuf::from("t.conf"), src.to_string());
        let (toks, _) = tokenize(id, src);
        let (d, diags) = paneld_config::parse(&toks, &map);
        assert!(diags.is_empty(), "{diags:?}");
        d
    }

    fn at(date: Date) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(Zone::default().local_midnight(date) as u64)
    }

    fn book(src: &str, saved: &BTreeMap<String, SavedReminder>, now: Date) -> Book {
        Book::new(&doc(src), Zone::default(), saved, at(now))
    }

    fn due(b: &Book, name: &str) -> Option<Date> {
        let r = b.list.iter().find(|r| r.name == name)?;
        b.due_at(r).map(|t| Zone::default().local_date(t))
    }

    #[test]
    fn every_counts_from_first_seen_then_from_done() {
        let mut b = book("reminder r { every = 1 month }\n", &BTreeMap::new(), (2026, 1, 31));
        assert_eq!(due(&b, "r"), Some((2026, 2, 28)));
        assert!(!b.done("r", at((2026, 2, 1))), "not due yet");
        assert!(b.done("reminder:r", at((2026, 3, 5)) + Duration::from_secs(3600)));
        assert_eq!(due(&b, "r"), Some((2026, 4, 5)));
    }

    #[test]
    fn start_and_once() {
        let src = "reminder a { every = 1w\n start = 2026-12-01 }\nreminder b { once = 2026-11-10 }\n";
        let mut b = book(src, &BTreeMap::new(), (2026, 11, 3));
        assert_eq!(due(&b, "a"), Some((2026, 12, 1)));
        assert_eq!(due(&b, "b"), Some((2026, 11, 10)));
        assert!(b.done("b", at((2026, 11, 10))));
        assert_eq!(due(&b, "b"), None);
        assert!(!b.done("b", at((2026, 11, 11))), "done once is done");
        assert!(b.done("a", at((2026, 12, 2))));
        assert_eq!(due(&b, "a"), Some((2026, 12, 9)));
    }

    #[test]
    fn sub_day_steps_round_up_to_a_day() {
        assert_eq!(Step::parse("12h"), Some(Step::Days(1)));
        assert_eq!(Step::parse("3d"), Some(Step::Days(3)));
        assert_eq!(Step::parse("2 weeks"), Some(Step::Days(14)));
        assert_eq!(Step::parse("1y"), Some(Step::Months(12)));
        assert_eq!(Step::parse("soon"), None);
    }

    #[test]
    fn saved_dates_survive_and_unknown_ones_go() {
        let mut saved = BTreeMap::new();
        saved.insert("r".to_string(), SavedReminder { first_seen: secs(at((2026, 1, 1))), done: None });
        saved.insert("gone".to_string(), SavedReminder::default());
        let b = book("reminder r { every = 10d }\n", &saved, (2026, 6, 1));
        assert_eq!(due(&b, "r"), Some((2026, 1, 11)));
        assert!(!b.dates().contains_key("gone"));
    }

    #[test]
    fn due_reminders_become_alerts_and_done_resolves_them() {
        let d = doc("reminder r { once = 2026-11-01\n title = Test a restore\n severity = warning }\n");
        let mut engine = alerts::Engine::new(&d);
        let mut b = Book::new(&d, Zone::default(), &BTreeMap::new(), at((2026, 10, 1)));
        let ctx = Context::new();
        assert!(!b.tick(&mut engine, &ctx, at((2026, 10, 31))));
        assert_eq!(engine.active_count(), 0);
        assert!(b.tick(&mut engine, &ctx, at((2026, 11, 2))));
        let a = &engine.active_alerts()["reminder:r"];
        assert_eq!((a.title.as_str(), a.severity.as_str()), ("Test a restore", "warning"));
        assert_eq!(a.since, at((2026, 11, 1)));
        assert!(b.done("reminder:r", at((2026, 11, 2))));
        assert!(b.tick(&mut engine, &ctx, at((2026, 11, 2))));
        assert_eq!(engine.active_count(), 0);
    }
}
