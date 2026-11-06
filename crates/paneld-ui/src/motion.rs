//! What the theme's motion allows (themes.md "Motion", ADR 0023), and
//! what each animated widget showed last, so a page drawn again whole
//! (every value change rebuilds it) can animate from there.
//!
//! A widget is known by the page, the node's place in the config, and
//! the `repeat` items and component calls it was drawn under. Only the
//! page on screen is remembered, at most [`MAX_ENTRIES`] widgets and
//! [`MAX_CHART_POINTS`] chart points.

use std::collections::{BTreeMap, HashMap};
use std::hash::{Hash, Hasher};

use paneld_config::motion::{parse_ms, parse_transition, Effect, Level, Transition, TAP_LEVEL, VALUE_LEVEL};
use paneld_config::parse::Block;

use crate::expr::Value;
use crate::ui::{ease, tick_ms, SeriesTween};

pub const MAX_ENTRIES: usize = 4096;
pub const MAX_CHART_POINTS: usize = 65536;

/// The theme's motion tokens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Settings {
    pub level: Level,
    pub page: Transition,
    pub value_ms: u32,
    pub tap_ms: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            level: Level::Reduced,
            page: Transition { effect: Effect::Fade, ms: 120 },
            value_ms: 300,
            tap_ms: 50,
        }
    }
}

impl Settings {
    /// From a theme's tokens (`motion`, `page_transition`,
    /// `value_transition`, `tap_transition`); a missing or bad one
    /// keeps its default (the check reports bad literals).
    pub fn from_theme(theme: Option<&BTreeMap<String, Value>>) -> Settings {
        let mut s = Settings::default();
        let Some(t) = theme else { return s };
        let tok = |k: &str| t.get(k).map(|v| v.to_display_string());
        if let Some(l) = tok("motion").and_then(|v| Level::parse(&v)) {
            s.level = l;
        }
        if let Some(p) = tok("page_transition").and_then(|v| parse_transition(&v).ok()) {
            s.page = p;
        }
        if let Some(ms) = tok("value_transition").and_then(|v| parse_ms(&v).ok()) {
            s.value_ms = ms;
        }
        if let Some(ms) = tok("tap_transition").and_then(|v| parse_ms(&v).ok()) {
            s.tap_ms = ms;
        }
        s
    }

    /// The page transition, if it runs at this level: (slide, ms).
    pub fn page_transition(&self) -> Option<(bool, u32)> {
        self.page.runs_at(self.level).then_some((self.page.effect == Effect::Slide, self.page.ms))
    }

    /// How long value changes take (0: at once).
    pub fn value_ms(&self) -> u32 {
        if self.level >= VALUE_LEVEL { self.value_ms } else { 0 }
    }

    /// How long a press takes to show (0: at once).
    pub fn tap_ms(&self) -> u32 {
        if self.level >= TAP_LEVEL { self.tap_ms } else { 0 }
    }
}

/// What a widget showed, or is moving to.
#[derive(Clone, Debug)]
enum Shown {
    Gauge { from: i32, to: i32, start: u32, ms: u32 },
    Chart { from: Vec<Vec<Option<i32>>>, to: Vec<Vec<Option<i32>>>, start: u32, ms: u32 },
    /// A node with `enter`, shown: its entrance went from progress
    /// `from` (0 hidden, 1 in) at `start` over `ms`.
    Entered { from: f64, start: u32, ms: u32 },
}

fn progress(start: u32, ms: u32, now: u32) -> f64 {
    if ms == 0 {
        return 1.0;
    }
    ease(now.wrapping_sub(start) as f64 / ms as f64)
}

fn lerp(from: i32, to: i32, e: f64) -> i32 {
    (from as f64 + (to - from) as f64 * e).round() as i32
}

/// Time left of a tween begun at `start`.
fn left(start: u32, ms: u32, now: u32) -> u32 {
    ms.saturating_sub(now.wrapping_sub(start))
}

#[derive(Default)]
struct State {
    page: String,
    /// The page's first render: nothing was on screen to move from.
    first: bool,
    /// Set the final values (the screen just woke, or motion is off).
    instant: bool,
    settings: Settings,
    prev: HashMap<u64, Shown>,
    next: HashMap<u64, Shown>,
    chart_points: usize,
    scope: Vec<u64>,
    /// The next render is instant.
    next_instant: bool,
    /// The next render starts afresh (a reload, a theme switch).
    fresh: bool,
    /// An update in place (ADR 0024): what is drawn now is recorded
    /// over what was shown, since the rest stays as it was.
    updating: bool,
    /// A fresh render to compare with: every value at its target,
    /// nothing remembered.
    scratch: bool,
}

thread_local! {
    static STATE: std::cell::RefCell<State> = std::cell::RefCell::new(State::default());
}

/// The next page render sets every value at once (it follows a sleep).
pub fn next_render_instant() {
    STATE.with(|s| s.borrow_mut().next_instant = true);
}

/// The config or the theme changed (a reload, a theme switch): the
/// next render draws every value and node as it is, no entrance or
/// value move, since a remembered place may now be another node.
pub fn forget() {
    STATE.with(|s| s.borrow_mut().fresh = true);
}

/// What a page kept off screen showed (the page cache, ADR 0024):
/// every value at its target, as its tweens were ended there.
pub(crate) struct Saved {
    page: String,
    shown: HashMap<u64, Shown>,
}

/// The page drawn is left for the cache: what it shows, at its
/// targets; nothing is remembered for the next page.
pub(crate) fn park() -> Saved {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        let shown = std::mem::take(&mut s.prev)
            .into_iter()
            .map(|(k, v)| {
                let v = match v {
                    Shown::Gauge { to, start, .. } => Shown::Gauge { from: to, to, start, ms: 0 },
                    Shown::Chart { to, start, .. } => Shown::Chart { from: to.clone(), to, start, ms: 0 },
                    Shown::Entered { start, .. } => Shown::Entered { from: 1.0, start, ms: 0 },
                };
                (k, v)
            })
            .collect();
        Saved { page: std::mem::take(&mut s.page), shown }
    })
}

/// A page from the cache is the page drawn again: its next update
/// moves from what it showed.
pub(crate) fn unpark(saved: Saved) {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        s.page = saved.page;
        s.prev = saved.shown;
    });
}

/// Whether the next render is instant ([`next_render_instant`]).
pub(crate) fn instant_pending() -> bool {
    STATE.with(|s| s.borrow().next_instant)
}

/// Drop a [`next_render_instant`] that no render took.
pub fn cancel_instant() {
    STATE.with(|s| s.borrow_mut().next_instant = false);
}

/// Start drawing `page` under `theme`.
pub(crate) fn begin(page: &str, theme: Option<&BTreeMap<String, Value>>) {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        s.settings = Settings::from_theme(theme);
        s.instant = std::mem::take(&mut s.next_instant);
        let fresh = std::mem::take(&mut s.fresh);
        if s.page != page || fresh {
            s.page = page.to_string();
            s.prev.clear();
            s.first = true;
        } else {
            s.first = false;
        }
        s.next.clear();
        s.scope.clear();
        s.chart_points = 0;
    });
}

/// Start updating `page` in place (it is the page drawn last).
pub(crate) fn begin_update(theme: Option<&BTreeMap<String, Value>>) {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        s.settings = Settings::from_theme(theme);
        s.instant = std::mem::take(&mut s.next_instant);
        s.first = false;
        s.updating = true;
        s.scope.clear();
        s.chart_points = 0;
    });
}

pub(crate) fn end_update() {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        s.updating = false;
        s.scope.clear();
    });
}

/// Draw a scratch copy (verify) from `on` until `off`: no motion, and
/// nothing of it remembered.
pub(crate) fn scratch(on: bool) {
    STATE.with(|s| s.borrow_mut().scratch = on);
}

/// Done drawing: what was drawn is what the next render moves from.
pub(crate) fn end() {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        s.prev = std::mem::take(&mut s.next);
        s.scope.clear();
    });
}

/// The settings of the render under way.
pub(crate) fn settings() -> Settings {
    STATE.with(|s| s.borrow().settings)
}

/// Enter a `repeat` item (`key`: its `id` or `name`, else its
/// position) or a component call (its place in the config).
pub(crate) fn push_scope(key: impl Hash) {
    if STATE.with(|s| s.borrow().scratch) {
        return;
    }
    let mut h = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut h);
    STATE.with(|s| s.borrow_mut().scope.push(h.finish()));
}

pub(crate) fn pop_scope() {
    STATE.with(|s| {
        if s.borrow().scratch {
            return;
        }
        s.borrow_mut().scope.pop();
    });
}

/// The key of a repeat item: its `id` or `name` when it has one, so a
/// row keeps its widgets when rows above it come or go.
pub(crate) fn item_key(item: &Value, index: usize) -> String {
    if let Value::Map(m) = item {
        for k in ["id", "name"] {
            if let Some(v) = m.get(k) {
                return format!("{k}:{}", v.to_display_string());
            }
        }
    }
    format!("#{index}")
}

/// The key `node` is known by where the render is now.
pub(crate) fn node_key(node: &Block) -> u64 {
    STATE.with(|s| key_of(&s.borrow(), node))
}

fn key_of(s: &State, node: &Block) -> u64 {
    key_tagged(s, node, 0)
}

/// `tag` keeps a node's entrance apart from its value (a `bar` with
/// `enter`).
fn key_tagged(s: &State, node: &Block, tag: u8) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    tag.hash(&mut h);
    s.scope.hash(&mut h);
    node.kind.span.file.hash(&mut h);
    node.kind.span.start.hash(&mut h);
    h.finish()
}

fn record(s: &mut State, key: u64, shown: Shown) {
    let map = if s.updating { &mut s.prev } else { &mut s.next };
    if map.len() < MAX_ENTRIES || map.contains_key(&key) {
        map.insert(key, shown);
        // Its node keeps the key, to forget it when it goes.
        crate::nodes::note_motion(key);
    }
}

/// The nodes that showed `keys` went (`repeat` items no longer listed):
/// what they showed is forgotten, and one coming back enters anew.
pub(crate) fn forget_keys(keys: &[u64]) {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        for k in keys {
            s.prev.remove(k);
            s.next.remove(k);
        }
    });
}

/// An `arc` or `bar` of `node` shows `to`: where to start it from and
/// over how long, if it moves.
pub(crate) fn gauge(node: &Block, to: i32) -> Option<(i32, u32)> {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        if s.scratch {
            return None;
        }
        let key = key_of(&s, node);
        let now = tick_ms();
        let ms = if s.instant { 0 } else { s.settings.value_ms() };
        let moving = match s.prev.get(&key) {
            Some(Shown::Gauge { from, to: was, start, ms: dur }) if ms > 0 => {
                let shown = lerp(*from, *was, progress(*start, *dur, now));
                // Still on its way to the same value: the rest of the way.
                let ms = if *was == to { left(*start, *dur, now) } else { ms };
                (shown != to && ms > 0).then_some((shown, ms))
            }
            _ => None,
        };
        let (from, ms) = moving.unwrap_or((to, 0));
        record(&mut s, key, Shown::Gauge { from, to, start: now, ms });
        moving
    })
}

/// A `chart` of `node` shows `to` (points of each series): the
/// series from and to, and over how long, if it moves. A series whose
/// points moved left by k (a history with new points) keeps its line:
/// the new points come out of the last one.
pub(crate) fn chart(node: &Block, to: &[Vec<Option<i32>>]) -> Option<(Vec<SeriesTween>, u32)> {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        if s.scratch {
            return None;
        }
        let key = key_of(&s, node);
        let now = tick_ms();
        let points: usize = to.iter().map(Vec::len).sum();
        if s.chart_points + points > MAX_CHART_POINTS {
            return None;
        }
        s.chart_points += points;
        let ms = if s.instant { 0 } else { s.settings.value_ms() };
        let moving = match s.prev.get(&key) {
            Some(Shown::Chart { from, to: was, start, ms: dur })
                if ms > 0 && was.len() == to.len() && was.iter().zip(to).all(|(a, b)| a.len() == b.len()) =>
            {
                let e = progress(*start, *dur, now);
                let same = was.as_slice() == to;
                let series: Vec<SeriesTween> = from
                    .iter()
                    .zip(was)
                    .zip(to)
                    .map(|((f, w), t)| {
                        let shown: Vec<Option<i32>> = f
                            .iter()
                            .zip(w)
                            .map(|(f, w)| match (f, w) {
                                (Some(f), Some(w)) => Some(lerp(*f, *w, e)),
                                (_, w) => *w,
                            })
                            .collect();
                        (shifted(&shown, w, t), t.clone())
                    })
                    .collect();
                let ms = if same { left(*start, *dur, now) } else { ms };
                let moves = series.iter().any(|(f, t)| f != t);
                (moves && ms > 0).then_some((series, ms))
            }
            _ => None,
        };
        let shown = match &moving {
            Some((series, ms)) => Shown::Chart {
                from: series.iter().map(|(f, _)| f.clone()).collect(),
                to: to.to_vec(),
                start: now,
                ms: *ms,
            },
            None => Shown::Chart { from: to.to_vec(), to: to.to_vec(), start: now, ms: 0 },
        };
        record(&mut s, key, shown);
        moving
    })
}

/// `shown` (where a series is on screen, moving to `was`) as the start
/// of a move to `to`: when `to` is `was` moved left by k points, the
/// shown points move left too and the k new ones start at the last.
fn shifted(shown: &[Option<i32>], was: &[Option<i32>], to: &[Option<i32>]) -> Vec<Option<i32>> {
    let n = to.len();
    for k in 1..=n / 2 {
        if was[k..] == to[..n - k] && was[k..].iter().any(Option::is_some) {
            let last = shown.last().copied().flatten();
            let mut v: Vec<Option<i32>> = shown[k..].to_vec();
            v.extend(std::iter::repeat_n(last, k));
            return v;
        }
    }
    shown.to_vec()
}

/// A node with `enter = <t>` is drawn, shown or `hidden`: whether it
/// comes in now (it was not shown on the last render of this page, or
/// its entrance was under way), as (slide, progress to start from,
/// ms).
pub(crate) fn enter(node: &Block, t: Transition, hidden: bool) -> Option<(bool, f64, u32)> {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        if s.scratch {
            return None;
        }
        let key = key_tagged(&s, node, 1);
        if hidden {
            // Hidden now: it comes in when it shows again, as after a
            // render that did not show it.
            if s.updating {
                s.prev.remove(&key);
            }
            return None;
        }
        let now = tick_ms();
        let can = !s.first && !s.instant && t.runs_at(s.settings.level);
        let (from, ms) = match s.prev.get(&key) {
            // Under way: the rest of it, from where it is.
            Some(Shown::Entered { from, start, ms }) if can && left(*start, *ms, now) > 0 => {
                (from + (1.0 - from) * progress(*start, *ms, now), left(*start, *ms, now))
            }
            Some(_) => (1.0, 0),
            None if can => (0.0, t.ms),
            None => (1.0, 0),
        };
        record(&mut s, key, Shown::Entered { from, start: now, ms });
        (ms > 0).then_some((t.effect == Effect::Slide, from, ms))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_history_moved_left_keeps_its_points() {
        let was = vec![Some(1), Some(2), Some(3), Some(4)];
        let to = vec![Some(2), Some(3), Some(4), Some(9)];
        assert_eq!(shifted(&was, &was, &to), vec![Some(2), Some(3), Some(4), Some(4)]);
        // Not a shift: point by point.
        let to = vec![Some(5), Some(5), Some(5), Some(5)];
        assert_eq!(shifted(&was, &was, &to), was);
    }

    #[test]
    fn settings_follow_the_level() {
        let mut t = BTreeMap::new();
        t.insert("motion".to_string(), Value::String("none".into()));
        let s = Settings::from_theme(Some(&t));
        assert_eq!((s.value_ms(), s.tap_ms(), s.page_transition()), (0, 0, None));
        t.insert("motion".to_string(), Value::String("reduced".into()));
        t.insert("page_transition".to_string(), Value::String("slide 300ms".into()));
        let s = Settings::from_theme(Some(&t));
        assert_eq!((s.value_ms(), s.tap_ms(), s.page_transition()), (300, 50, None));
        t.insert("motion".to_string(), Value::String("full".into()));
        assert_eq!(Settings::from_theme(Some(&t)).page_transition(), Some((true, 300)));
        t.insert("page_transition".to_string(), Value::String("fade 120ms".into()));
        t.insert("motion".to_string(), Value::String("reduced".into()));
        assert_eq!(Settings::from_theme(Some(&t)).page_transition(), Some((false, 120)));
    }
}
