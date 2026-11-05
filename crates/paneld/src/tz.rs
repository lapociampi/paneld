//! Local time without libc or a dependency, for `screen:night` hours.
//!
//! Reads `/etc/localtime` (TZif, RFC 8536): the offset of the last
//! transition at or before `now`; past the last transition (or in a
//! "slim" file with none) the POSIX TZ rule in the footer, e.g.
//! `CET-1CEST,M3.5.0,M10.5.0/3`. Only the `Mm.w.d` rule form is
//! handled; anything else falls back to the standard offset. No file
//! means UTC.

use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Zone {
    /// (transition time, UTC offset in seconds after it).
    transitions: Vec<(i64, i32)>,
    /// Offset before the first transition.
    initial: i32,
    footer: Option<Rule>,
}

#[derive(Debug, Clone, PartialEq)]
struct Rule {
    std: i32,
    dst: Option<(i32, When, When)>,
}

/// `Mm.w.d/time`: month 1-12, week 1-5 (5 = last), weekday 0 = Sunday,
/// local time of day in seconds.
#[derive(Debug, Clone, Copy, PartialEq)]
struct When {
    month: u32,
    week: u32,
    weekday: u32,
    secs: i64,
}

impl Zone {
    /// The local zone as libc finds it: `TZ` when set, else
    /// `/etc/localtime`, else UTC.
    pub fn local() -> Self {
        Self::from_tz(std::env::var("TZ").ok().as_deref(), std::path::Path::new("/usr/share/zoneinfo"))
    }

    /// The zone `TZ` names (libc's rules): unset, `/etc/localtime`;
    /// empty, UTC; a POSIX rule (`UTC0`, `CET-1CEST,M3.5.0,M10.5.0/3`);
    /// else a TZif file, `:` in front optional: an absolute path, or a
    /// name under `zoneinfo` (`Europe/Rome`, `UTC`). Anything that does
    /// not read is UTC.
    pub fn from_tz(tz: Option<&str>, zoneinfo: &std::path::Path) -> Self {
        let read = |p: &std::path::Path| std::fs::read(p).ok().and_then(|b| Self::parse(&b));
        let Some(tz) = tz else {
            return read(std::path::Path::new("/etc/localtime")).unwrap_or_default();
        };
        let name = tz.strip_prefix(':').unwrap_or(tz);
        if name.is_empty() {
            return Zone::default();
        }
        if !tz.starts_with(':') {
            if let Some(rule) = parse_rule(tz) {
                return Zone { footer: Some(rule), ..Zone::default() };
            }
        }
        let path = std::path::Path::new(name);
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else if path.components().all(|c| matches!(c, std::path::Component::Normal(_))) {
            zoneinfo.join(path)
        } else {
            return Zone::default();
        };
        read(&path).unwrap_or_default()
    }

    pub fn parse(b: &[u8]) -> Option<Self> {
        let header = |at: usize| -> Option<(u8, [usize; 6])> {
            if b.get(at..at + 4)? != b"TZif" {
                return None;
            }
            let version = *b.get(at + 4)?;
            let mut n = [0usize; 6];
            for (i, slot) in n.iter_mut().enumerate() {
                let o = at + 20 + i * 4;
                *slot = u32::from_be_bytes(b.get(o..o + 4)?.try_into().ok()?) as usize;
            }
            Some((version, n))
        };
        // Counts: isutcnt, isstdcnt, leapcnt, timecnt, typecnt, charcnt.
        let (version, n1) = header(0)?;
        let v1_len = n1[3] * 4 + n1[3] + n1[4] * 6 + n1[5] + n1[2] * 8 + n1[1] + n1[0];
        let (start, n, tsize) = if version >= b'2' {
            let at = 44 + v1_len;
            let (_, n2) = header(at)?;
            (at + 44, n2, 8)
        } else {
            (44, n1, 4)
        };
        let [isut, isstd, leap, timecnt, typecnt, charcnt] = n;
        let times_at = start;
        let idx_at = times_at + timecnt * tsize;
        let types_at = idx_at + timecnt;
        let types: Vec<i32> = (0..typecnt)
            .map(|i| {
                let o = types_at + i * 6;
                b.get(o..o + 4).map(|x| i32::from_be_bytes(x.try_into().unwrap()))
            })
            .collect::<Option<_>>()?;
        if types.is_empty() {
            return None;
        }
        let mut transitions = Vec::with_capacity(timecnt);
        for i in 0..timecnt {
            let o = times_at + i * tsize;
            let t = if tsize == 8 {
                i64::from_be_bytes(b.get(o..o + 8)?.try_into().ok()?)
            } else {
                i32::from_be_bytes(b.get(o..o + 4)?.try_into().ok()?) as i64
            };
            let ty = *b.get(idx_at + i)? as usize;
            transitions.push((t, *types.get(ty)?));
        }
        let footer = if version >= b'2' {
            let end = types_at + typecnt * 6 + charcnt + leap * (tsize + 4) + isstd + isut;
            b.get(end..)
                .and_then(|f| std::str::from_utf8(f).ok())
                .and_then(|f| f.trim_matches('\n').lines().next().map(str::to_string))
                .and_then(|s| parse_rule(&s))
        } else {
            None
        };
        Some(Zone { transitions, initial: types[0], footer })
    }

    /// Offset from UTC, in seconds, at Unix time `t`.
    pub fn offset_at(&self, t: i64) -> i32 {
        match self.transitions.iter().rposition(|&(at, _)| at <= t) {
            Some(i) if i + 1 < self.transitions.len() || self.footer.is_none() => {
                self.transitions[i].1
            }
            Some(i) => self.footer.as_ref().map_or(self.transitions[i].1, |r| r.offset_at(t)),
            None if self.transitions.is_empty() => {
                self.footer.as_ref().map_or(self.initial, |r| r.offset_at(t))
            }
            None => self.initial,
        }
    }

    /// Seconds since local midnight at `now`.
    pub fn local_seconds_of_day(&self, now: SystemTime) -> u32 {
        let t = now
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        (t + self.offset_at(t) as i64).rem_euclid(86_400) as u32
    }
}

impl Rule {
    fn offset_at(&self, t: i64) -> i32 {
        let Some((dst, start, end)) = self.dst else {
            return self.std;
        };
        let year = civil_from_days((t + self.std as i64).div_euclid(86_400)).0;
        // Start is given in standard local time, end in DST local time.
        let s = when_local(year, start) - self.std as i64;
        let e = when_local(year, end) - dst as i64;
        let in_dst = if s < e { s <= t && t < e } else { !(e <= t && t < s) };
        if in_dst { dst } else { self.std }
    }
}

/// Local Unix-style seconds of rule `w` in `year`.
fn when_local(year: i64, w: When) -> i64 {
    let first = days_from_civil(year, w.month, 1);
    let first_wd = (first + 4).rem_euclid(7) as u32; // 1970-01-01 was a Thursday
    let mut day = 1 + (w.weekday + 7 - first_wd) % 7 + 7 * (w.week - 1);
    let len = month_len(year, w.month);
    while day > len {
        day -= 7;
    }
    (first + (day - 1) as i64) * 86_400 + w.secs
}

fn month_len(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 => 29,
        _ => 28,
    }
}

// Howard Hinnant's civil-date algorithms.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = m as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// POSIX TZ string: `std offset [dst [offset] [,start[/time],end[/time]]]`.
fn parse_rule(s: &str) -> Option<Rule> {
    let mut p = s;
    let take_name = |p: &mut &str| -> Option<()> {
        if let Some(rest) = p.strip_prefix('<') {
            let end = rest.find('>')?;
            *p = &rest[end + 1..];
        } else {
            let end = p.find(|c: char| !c.is_ascii_alphabetic()).unwrap_or(p.len());
            if end < 3 {
                return None;
            }
            *p = &p[end..];
        }
        Some(())
    };
    let take_offset = |p: &mut &str| -> Option<i64> {
        let end = p.find(|c: char| !(c.is_ascii_digit() || c == ':' || c == '+' || c == '-')).unwrap_or(p.len());
        let text = &p[..end];
        if text.is_empty() {
            return None;
        }
        *p = &p[end..];
        parse_hms(text)
    };
    take_name(&mut p)?;
    // POSIX offsets are west-positive: `CET-1` is UTC+1.
    let std = -take_offset(&mut p)? as i32;
    if p.is_empty() {
        return Some(Rule { std, dst: None });
    }
    take_name(&mut p)?;
    let dst = if p.starts_with(',') { std + 3600 } else { -take_offset(&mut p)? as i32 };
    let p = p.strip_prefix(',')?;
    let (a, b) = p.split_once(',')?;
    Some(Rule { std, dst: Some((dst, parse_when(a)?, parse_when(b)?)) })
}

fn parse_when(s: &str) -> Option<When> {
    let (date, time) = match s.split_once('/') {
        Some((d, t)) => (d, parse_hms(t)?),
        None => (s, 7200),
    };
    let mut it = date.strip_prefix('M')?.split('.');
    let month = it.next()?.parse().ok()?;
    let week = it.next()?.parse().ok()?;
    let weekday = it.next()?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=5).contains(&week) || weekday > 6 {
        return None;
    }
    Some(When { month, week, weekday, secs: time })
}

/// `[+-]h[:mm[:ss]]` in seconds.
fn parse_hms(s: &str) -> Option<i64> {
    let (sign, s) = match s.as_bytes().first()? {
        b'-' => (-1, &s[1..]),
        b'+' => (1, &s[1..]),
        _ => (1, s),
    };
    let mut parts = s.split(':');
    let h: i64 = parts.next()?.parse().ok()?;
    let m: i64 = parts.next().map_or(Some(0), |x| x.parse().ok())?;
    let sec: i64 = parts.next().map_or(Some(0), |x| x.parse().ok())?;
    Some(sign * (h * 3600 + m * 60 + sec))
}

impl Zone {
    /// Unix time of local midnight starting `date` (`YYYY-MM-DD`).
    pub fn local_midnight(&self, date: (i64, u32, u32)) -> i64 {
        let utc_midnight = days_from_civil(date.0, date.1, date.2) * 86_400;
        // The offset at that moment, refined once around a DST change.
        let guess = utc_midnight - self.offset_at(utc_midnight) as i64;
        utc_midnight - self.offset_at(guess) as i64
    }
}

impl Zone {
    /// A zone that follows POSIX TZ rule `rule` alone
    /// (`CET-1CEST,M3.5.0,M10.5.0/3`), for tests.
    #[cfg(test)]
    pub fn from_rule(rule: &str) -> Option<Self> {
        Some(Zone { footer: Some(parse_rule(rule)?), ..Zone::default() })
    }

    /// The local date, weekday (0 = Sunday) and seconds of day at
    /// Unix time `t`.
    pub fn local_parts(&self, t: i64) -> ((i64, u32, u32), u32, u32) {
        let local = t + self.offset_at(t) as i64;
        let days = local.div_euclid(86_400);
        // 1970-01-01 was a Thursday.
        (civil_from_days(days), (days + 4).rem_euclid(7) as u32, local.rem_euclid(86_400) as u32)
    }

    /// The local calendar date at Unix time `t`.
    pub fn local_date(&self, t: i64) -> (i64, u32, u32) {
        civil_from_days((t + self.offset_at(t) as i64).div_euclid(86_400))
    }
}

/// `date` moved by `days` calendar days.
pub fn add_days(date: (i64, u32, u32), days: i64) -> (i64, u32, u32) {
    civil_from_days(days_from_civil(date.0, date.1, date.2) + days)
}

/// `date` moved by `months` calendar months; the day is clamped to the
/// month's length (Jan 31 + 1 month is Feb 28 or 29).
pub fn add_months(date: (i64, u32, u32), months: i64) -> (i64, u32, u32) {
    let total = date.0 * 12 + (date.1 as i64 - 1) + months;
    let (y, m) = (total.div_euclid(12), total.rem_euclid(12) as u32 + 1);
    (y, m, date.2.min(month_len(y, m)))
}

/// `YYYY-MM-DD` (schema `date`).
pub use paneld_config::value::parse_date;

/// `HH:MM` as seconds of day.
pub fn parse_clock(s: &str) -> Option<u32> {
    let (h, m) = s.trim().split_once(':')?;
    let (h, m): (u32, u32) = (h.parse().ok()?, m.parse().ok()?);
    (h < 24 && m < 60).then_some(h * 3600 + m * 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    const EU: &str = "CET-1CEST,M3.5.0,M10.5.0/3";

    fn utc(y: i64, mo: u32, d: u32, h: i64) -> i64 {
        days_from_civil(y, mo, d) * 86_400 + h * 3600
    }

    #[test]
    fn civil_date_round_trip() {
        for days in [-719_468i64, -1, 0, 19_000, 20_000, 60_000] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days);
        }
        assert_eq!(civil_from_days(0), (1970, 1, 1));
    }

    #[test]
    fn posix_rule_switches_on_the_last_sundays() {
        let r = parse_rule(EU).unwrap();
        // 2026: DST from 29 March 01:00 UTC to 25 October 01:00 UTC.
        assert_eq!(r.offset_at(utc(2026, 3, 29, 0)), 3600);
        assert_eq!(r.offset_at(utc(2026, 3, 29, 1)), 7200);
        assert_eq!(r.offset_at(utc(2026, 7, 1, 12)), 7200);
        assert_eq!(r.offset_at(utc(2026, 10, 25, 0)), 7200);
        assert_eq!(r.offset_at(utc(2026, 10, 25, 1)), 3600);
        assert_eq!(parse_rule("UTC0").unwrap().offset_at(0), 0);
        assert_eq!(parse_rule("<+0530>-5:30").unwrap().offset_at(0), 19_800);
        // Southern hemisphere: DST spans the new year.
        let nz = parse_rule("NZST-12NZDT,M9.5.0,M4.1.0/3").unwrap();
        assert_eq!(nz.offset_at(utc(2026, 1, 15, 0)), 13 * 3600);
        assert_eq!(nz.offset_at(utc(2026, 6, 15, 0)), 12 * 3600);
    }

    /// A TZif v2 file with one transition and a footer.
    fn tzif(transition: i64, footer: &str) -> Vec<u8> {
        let header = |n: [u32; 6], version: u8| {
            let mut h = b"TZif".to_vec();
            h.push(version);
            h.extend([0u8; 15]);
            for c in n {
                h.extend(c.to_be_bytes());
            }
            h
        };
        // isut, isstd, leap, time, type, char
        let mut b = header([0, 0, 0, 0, 1, 4], b'2');
        b.extend(3600i32.to_be_bytes());
        b.extend([0, 0]);
        b.extend(b"CET\0");
        b.extend(header([0, 0, 0, 1, 2, 9], b'2'));
        b.extend(transition.to_be_bytes());
        b.push(1);
        b.extend(3600i32.to_be_bytes());
        b.extend([0, 0]);
        b.extend(7200i32.to_be_bytes());
        b.extend([1, 4]);
        b.extend(b"CET\0CEST\0");
        b.extend(format!("\n{footer}\n").bytes());
        b
    }

    #[test]
    fn tz_is_read_as_libc_reads_it() {
        let dir = std::env::temp_dir().join(format!("paneld-tz-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("Europe")).unwrap();
        let summer = utc(2026, 7, 1, 12);
        std::fs::write(dir.join("Europe/Rome"), tzif(utc(2026, 3, 29, 1), EU)).unwrap();
        let at = |tz: Option<&str>| Zone::from_tz(tz, &dir).offset_at(summer);
        assert_eq!(at(Some("Europe/Rome")), 7200);
        assert_eq!(at(Some(":Europe/Rome")), 7200);
        let abs = dir.join("Europe/Rome").display().to_string();
        assert_eq!(at(Some(&format!(":{abs}"))), 7200);
        assert_eq!(at(Some(EU)), 7200, "a POSIX rule");
        assert_eq!(at(Some("<+0530>-5:30")), 19_800);
        assert_eq!(at(Some("UTC0")), 0);
        // UTC without a zoneinfo file, empty, unknown and `..` are UTC.
        assert_eq!(at(Some("UTC")), 0);
        assert_eq!(at(Some("")), 0);
        assert_eq!(at(Some("Mars/Base")), 0);
        assert_eq!(at(Some("../Europe/Rome")), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tzif_uses_transitions_then_the_footer() {
        let t = utc(2026, 3, 29, 1);
        let z = Zone::parse(&tzif(t, EU)).unwrap();
        assert_eq!(z.offset_at(t - 1), 3600);
        // At the last transition and later, the footer rule decides.
        assert_eq!(z.offset_at(t), 7200);
        assert_eq!(z.offset_at(utc(2026, 12, 1, 0)), 3600);
        assert!(Zone::parse(b"nope").is_none());
    }

    #[test]
    fn real_zoneinfo_when_present() {
        // Skipped quietly on systems without the file.
        let Ok(b) = std::fs::read("/usr/share/zoneinfo/Europe/Rome") else { return };
        let z = Zone::parse(&b).unwrap();
        assert_eq!(z.offset_at(utc(2026, 1, 15, 12)), 3600);
        assert_eq!(z.offset_at(utc(2026, 7, 15, 12)), 7200);
    }

    #[test]
    fn dates_and_local_midnight() {
        assert_eq!(parse_date("2026-11-01"), Some((2026, 11, 1)));
        assert_eq!(parse_date("2026-02-29"), None);
        assert_eq!(parse_date("2028-02-29"), Some((2028, 2, 29)));
        assert_eq!(parse_date("2026-13-01"), None);
        let utc = Zone::default();
        assert_eq!(utc.local_midnight((1970, 1, 2)), 86_400);
        let cet = Zone { footer: parse_rule(EU), ..Zone::default() };
        // Midnight in Rome on 1 July is 22:00 UTC the day before.
        assert_eq!(cet.local_midnight((2026, 7, 1)), utc_(2026, 6, 30, 22));
    }

    fn utc_(y: i64, mo: u32, d: u32, h: i64) -> i64 {
        days_from_civil(y, mo, d) * 86_400 + h * 3600
    }

    #[test]
    fn calendar_steps() {
        assert_eq!(add_days((2026, 12, 31), 1), (2027, 1, 1));
        assert_eq!(add_days((2024, 3, 1), -1), (2024, 2, 29));
        assert_eq!(add_months((2026, 1, 31), 1), (2026, 2, 28));
        assert_eq!(add_months((2026, 11, 15), 3), (2027, 2, 15));
        assert_eq!(add_months((2026, 5, 10), 12), (2027, 5, 10));
        let z = Zone { footer: parse_rule(EU), ..Zone::default() };
        // 23:30 UTC on Nov 2 is already Nov 3 in CET.
        assert_eq!(z.local_date(utc_(2026, 11, 2, 23) + 1800), (2026, 11, 3));
        assert_eq!(z.local_date(utc_(2026, 11, 2, 22)), (2026, 11, 2));
    }

    #[test]
    fn clock_parses_hh_mm() {
        assert_eq!(parse_clock("22:00"), Some(79_200));
        assert_eq!(parse_clock("07:30"), Some(27_000));
        assert_eq!(parse_clock("24:00"), None);
    }
}
