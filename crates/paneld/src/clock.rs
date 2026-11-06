//! `now` in expressions (schema 16): the wall clock in local time, as
//! a map. Pages that read it are redrawn when what they read changes
//! (see `visible::clock_on_page`): each minute, or each second for
//! `now.second` / `now.unix`.

use std::collections::BTreeMap;
use std::sync::RwLock;
use std::time::{SystemTime, UNIX_EPOCH};

use paneld_ui::{Context, Value};

use crate::tz::Zone;

/// The local time zone, read at load and on reload.
static ZONE: RwLock<Option<Zone>> = RwLock::new(None);

const WEEKDAYS: [&str; 7] = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
const MONTHS: [&str; 12] = [
    "January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November",
    "December",
];

/// Use `zone` for `now` from here on.
pub fn set_zone(zone: Zone) {
    *ZONE.write().unwrap_or_else(|e| e.into_inner()) = Some(zone);
}

/// Set by [`freeze`]: the time `now` reads (`i64::MIN`: the clock's).
static FROZEN: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(i64::MIN);

/// Unix seconds now (as [`freeze`] holds it, if it does).
pub fn unix_now() -> i64 {
    match FROZEN.load(std::sync::atomic::Ordering::Relaxed) {
        i64::MIN => SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64),
        t => t,
    }
}

/// Now, as [`unix_now`] reads it: held still with it (`shot --verify`),
/// else the wall clock.
pub fn system_now() -> SystemTime {
    match FROZEN.load(std::sync::atomic::Ordering::Relaxed) {
        i64::MIN => SystemTime::now(),
        t => UNIX_EPOCH + std::time::Duration::from_secs(t.max(0) as u64),
    }
}

/// `paneld shot --verify`: `now` reads `t` until `freeze(None)`, so a
/// page and the fresh render it is compared with see the same second.
pub fn freeze(t: Option<i64>) {
    FROZEN.store(t.unwrap_or(i64::MIN), std::sync::atomic::Ordering::Relaxed);
}

/// `now` at Unix time `t` in `zone`.
pub fn now_value(zone: &Zone, t: i64) -> Value {
    let ((year, month, day), weekday, secs) = zone.local_parts(t);
    let (hour, minute, second) = (secs / 3600, secs / 60 % 60, secs % 60);
    let (wd, mo) = (WEEKDAYS[weekday as usize], MONTHS[month as usize - 1]);
    let h12 = match hour % 12 {
        0 => 12,
        h => h,
    };
    let mut m = BTreeMap::new();
    let mut put = |k: &str, v: Value| {
        m.insert(k.to_string(), v);
    };
    put("unix", Value::Int(t));
    put("year", Value::Int(year));
    put("month", Value::Int(month.into()));
    put("day", Value::Int(day.into()));
    put("hour", Value::Int(hour.into()));
    put("minute", Value::Int(minute.into()));
    put("second", Value::Int(second.into()));
    put("weekday", Value::String(wd.into()));
    put("weekday_short", Value::String(wd[..3].into()));
    put("month_name", Value::String(mo.into()));
    put("month_short", Value::String(mo[..3].into()));
    put("date", Value::String(format!("{day} {mo}")));
    put("iso_date", Value::String(format!("{year:04}-{month:02}-{day:02}")));
    put("time", Value::String(format!("{hour:02}:{minute:02}")));
    put("time12", Value::String(format!("{h12}:{minute:02} {}", if hour < 12 { "AM" } else { "PM" })));
    Value::Map(m)
}

/// `ctx` gets `now`, read from the clock at this moment.
pub fn insert(ctx: &mut Context) {
    let t = unix_now();
    let zone = ZONE.read().unwrap_or_else(|e| e.into_inner());
    let v = match zone.as_ref() {
        Some(z) => now_value(z, t),
        None => now_value(&Zone::default(), t),
    };
    ctx.insert("now".into(), v);
}

/// How often what a page reads of `now` changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tick {
    Minute,
    Second,
}

impl Tick {
    /// Which minute or second Unix time `t` is in: a page drawn in one
    /// is drawn again when this changes. (Local minutes start on UTC
    /// minutes: zone offsets are whole minutes.)
    pub fn key(self, t: i64) -> i64 {
        match self {
            Tick::Minute => t.div_euclid(60),
            Tick::Second => t,
        }
    }

    /// Milliseconds from `now` (since the epoch) to the next change.
    pub fn ms_to_next(self, now: std::time::Duration) -> u64 {
        let period = match self {
            Tick::Minute => 60_000,
            Tick::Second => 1_000,
        };
        let ms = now.as_millis() as u64;
        period - ms % period
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field<'a>(v: &'a Value, k: &str) -> &'a Value {
        let Value::Map(m) = v else { panic!("not a map") };
        &m[k]
    }

    fn text(v: &Value, k: &str) -> String {
        field(v, k).to_display_string()
    }

    #[test]
    fn now_is_local_time_with_every_documented_field() {
        let cet = Zone::from_rule("CET-1CEST,M3.5.0,M10.5.0/3").unwrap();
        // 2026-11-05 13:05:09 UTC: CET (no DST) is 14:05:09, a Thursday.
        let t = 1_793_883_909;
        let v = now_value(&cet, t);
        let Value::Map(m) = &v else { panic!() };
        let keys: Vec<&str> = m.keys().map(String::as_str).collect();
        let mut documented = paneld_config::cel::NOW_FIELDS.to_vec();
        documented.sort();
        assert_eq!(keys, documented);
        assert_eq!(field(&v, "unix"), &Value::Int(t));
        assert_eq!(
            ["year", "month", "day", "hour", "minute", "second"].map(|k| field(&v, k).clone()),
            [2026, 11, 5, 14, 5, 9].map(Value::Int)
        );
        assert_eq!(text(&v, "weekday"), "Thursday");
        assert_eq!(text(&v, "weekday_short"), "Thu");
        assert_eq!(text(&v, "month_name"), "November");
        assert_eq!(text(&v, "month_short"), "Nov");
        assert_eq!(text(&v, "date"), "5 November");
        assert_eq!(text(&v, "iso_date"), "2026-11-05");
        assert_eq!(text(&v, "time"), "14:05");
        assert_eq!(text(&v, "time12"), "2:05 PM");
        // Summer time, and midnight on a 12-hour clock.
        let v = now_value(&cet, 1_783_634_400); // 2026-07-09 22:00 UTC
        assert_eq!((text(&v, "time"), text(&v, "time12"), text(&v, "weekday")), ("00:00".into(), "12:00 AM".into(), "Friday".into()));
        assert_eq!(text(&v, "iso_date"), "2026-07-10");
        // No zone: UTC.
        let v = now_value(&Zone::default(), t);
        assert_eq!(text(&v, "time"), "13:05");
        assert_eq!(text(&v, "time12"), "1:05 PM");
    }

    #[test]
    fn ticks_change_on_the_minute_or_the_second() {
        assert_eq!(Tick::Minute.key(119), Tick::Minute.key(60));
        assert_ne!(Tick::Minute.key(120), Tick::Minute.key(119));
        assert_ne!(Tick::Second.key(120), Tick::Second.key(119));
        let at = |ms: u64| std::time::Duration::from_millis(ms);
        assert_eq!(Tick::Minute.ms_to_next(at(60_000 * 7 + 59_250)), 750);
        assert_eq!(Tick::Second.ms_to_next(at(1_000 * 7 + 250)), 750);
        assert_eq!(Tick::Second.ms_to_next(at(5_000)), 1_000);
    }
}
