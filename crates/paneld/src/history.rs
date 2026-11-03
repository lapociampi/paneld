//! Source history (schema 4): `history = 1h` keeps a source's past
//! values in memory, as `sources.<name>.history` (a list of
//! `{at, value}`, oldest first) for charts. Nothing is written to
//! disk; longer series come from a source that returns one
//! (requirements, section 6).

use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use paneld_config::parse::{AssignTarget, Document, Item};
use paneld_ui::Value;

/// Most samples kept per source, whatever `history` says: a fast
/// poll over a long window must not eat the Pi's memory.
const MAX_SAMPLES: usize = 2000;

/// Only small values are kept (what a chart draws): numbers, bools
/// and strings up to this many bytes. A source returning a big map
/// would otherwise keep thousands of copies of it.
const MAX_SAMPLE_TEXT: usize = 64;

fn small(v: &Value) -> bool {
    match v {
        Value::Int(_) | Value::Double(_) | Value::Bool(_) | Value::Null => true,
        Value::String(s) => s.len() <= MAX_SAMPLE_TEXT,
        _ => false,
    }
}

#[derive(Default)]
pub struct Histories {
    /// How long each source with `history` keeps its samples.
    keep: BTreeMap<String, Duration>,
    samples: BTreeMap<String, VecDeque<(i64, Value)>>,
    /// Sources already warned about values too big to keep.
    warned: std::collections::BTreeSet<String>,
}

impl Histories {
    pub fn from_doc(doc: &Document) -> Self {
        let mut keep = BTreeMap::new();
        for item in &doc.items {
            let Item::Block(b) = item else { continue };
            if b.kind.text != "source" {
                continue;
            }
            let Some(name) = &b.name else { continue };
            for inner in &b.items {
                if let Item::Assign(a) = inner {
                    if matches!(&a.target, AssignTarget::Simple { key } if key.text == "history") {
                        if let Some(d) = paneld_config::value::parse_duration(&a.value.text) {
                            keep.insert(name.text.clone(), d);
                        }
                    }
                }
            }
        }
        Self { keep, samples: BTreeMap::new(), warned: Default::default() }
    }

    /// After a reload: keep the samples of sources that still have
    /// `history`.
    pub fn carry_from(&mut self, old: &mut Histories) {
        for name in self.keep.keys() {
            if let Some(s) = old.samples.remove(name) {
                self.samples.insert(name.clone(), s);
            }
        }
    }

    /// Note a new `value` map of source `name` (from a run): stamp
    /// it with `at` and, for a source with `history`, add its `value`
    /// to the samples and put the list in as `history`.
    pub fn record(&mut self, name: &str, value: &mut Value, now: SystemTime) {
        let Value::Map(m) = value else { return };
        let at = now.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
        m.insert("at".into(), Value::Int(at));
        let Some(keep) = self.keep.get(name) else { return };
        let samples = self.samples.entry(name.to_string()).or_default();
        match m.get("value") {
            Some(v) if small(v) => samples.push_back((at, v.clone())),
            Some(_) if self.warned.insert(name.to_string()) => {
                eprintln!("paneld: source `{name}`: `history` keeps numbers and short text only; use `value` to pick one");
            }
            _ => {}
        }
        let oldest = at.saturating_sub(keep.as_secs() as i64);
        while samples.front().is_some_and(|(t, _)| *t < oldest) || samples.len() > MAX_SAMPLES {
            samples.pop_front();
        }
        let list = samples
            .iter()
            .map(|(t, v)| {
                let mut s = BTreeMap::new();
                s.insert("at".to_string(), Value::Int(*t));
                s.insert("value".to_string(), v.clone());
                Value::Map(s)
            })
            .collect();
        m.insert("history".into(), Value::List(list));
    }
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

    fn sample(v: i64) -> Value {
        Value::Map(BTreeMap::from([("value".to_string(), Value::Int(v))]))
    }

    fn history_of(v: &Value) -> Vec<(i64, i64)> {
        let Value::Map(m) = v else { panic!() };
        let Some(Value::List(l)) = m.get("history") else { return Vec::new() };
        l.iter()
            .map(|s| match s {
                Value::Map(s) => match (&s["at"], &s["value"]) {
                    (Value::Int(a), Value::Int(v)) => (*a, *v),
                    _ => panic!(),
                },
                _ => panic!(),
            })
            .collect()
    }

    #[test]
    fn keeps_samples_for_the_window_only() {
        let mut h = Histories::from_doc(&doc("source t { run = x\n history = 1m }\nsource u { run = y }\n"));
        let t0 = UNIX_EPOCH + Duration::from_secs(1_000);
        let mut last = sample(0);
        for (i, dt) in [0u64, 30, 60, 90].iter().enumerate() {
            last = sample(i as i64);
            h.record("t", &mut last, t0 + Duration::from_secs(*dt));
        }
        // At 1090 the window starts at 1030.
        assert_eq!(history_of(&last), [(1030, 1), (1060, 2), (1090, 3)]);
        // No `history`: only `at`.
        let mut u = sample(5);
        h.record("u", &mut u, t0);
        assert!(history_of(&u).is_empty());
        let Value::Map(m) = &u else { panic!() };
        assert_eq!(m["at"], Value::Int(1_000));

        // Big values are not kept.
        let mut big = Value::Map(BTreeMap::from([(
            "value".to_string(),
            Value::Map(BTreeMap::from([("k".to_string(), Value::Int(1))])),
        )]));
        h.record("t", &mut big, t0 + Duration::from_secs(95));
        // (1030 has left the window by 1095.)
        assert_eq!(history_of(&big), [(1060, 2), (1090, 3)]);

        // A reload keeps the samples of sources that still keep them.
        let mut fresh = Histories::from_doc(&doc("source t { run = x\n history = 1h }\n"));
        fresh.carry_from(&mut h);
        let mut next = sample(4);
        fresh.record("t", &mut next, t0 + Duration::from_secs(120));
        assert_eq!(history_of(&next).len(), 3);
    }
}
