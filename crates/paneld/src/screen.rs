//! Screen power and night hours (schema 12): sleep after `idle`
//! without a touch, wake on a touch or on a new alert of a `wake_on`
//! severity, `on` / `off` / `dim` backlight commands, the frame rate
//! (`fps`), and `night { }` hours that switch theme, idle, brightness
//! and frame rate.
//!
//! [`Screen::step`] is pure (time and inputs in, [`Effect`]s out) so
//! the rules are tested without LVGL; `main.rs` applies the effects.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use paneld_config::parse::{AssignTarget, Block, Document, Item};

use crate::tz::{parse_clock, Zone};

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Night {
    pub from: u32,
    pub to: u32,
    pub theme: Option<String>,
    pub idle: Option<Duration>,
    pub brightness: Option<u8>,
    pub fps: Option<u32>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub theme: Option<String>,
    pub idle: Option<Duration>,
    pub brightness: u8,
    /// Frames a second, 1..=60 (default 30).
    pub fps: u32,
    pub on: Option<String>,
    pub off: Option<String>,
    pub dim: Option<String>,
    pub wake_on: Vec<String>,
    /// `first_tap = wake` (default): the waking tap does nothing else.
    pub first_tap_wakes: bool,
    pub night: Option<Night>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            theme: None,
            idle: None,
            brightness: 100,
            fps: 30,
            on: None,
            off: None,
            dim: None,
            wake_on: vec!["critical".into()],
            first_tap_wakes: true,
            night: None,
        }
    }
}

impl Config {
    /// The `screen { }` block (after `apply_overrides` there is one).
    pub fn from_doc(doc: &Document) -> Self {
        let mut c = Self::default();
        let Some(b) = doc.items.iter().find_map(|it| match it {
            Item::Block(b) if b.kind.text == "screen" && b.name.is_none() => Some(b),
            _ => None,
        }) else {
            return c;
        };
        for (key, raw) in assigns(b) {
            match key {
                "theme" => c.theme = Some(raw.to_string()),
                "idle" => c.idle = paneld_config::value::parse_duration(raw),
                "brightness" => c.brightness = parse_level(raw).unwrap_or(c.brightness),
                "fps" => c.fps = parse_fps(raw).unwrap_or(c.fps),
                "on" => c.on = Some(raw.to_string()),
                "off" => c.off = Some(raw.to_string()),
                "dim" => c.dim = Some(raw.to_string()),
                "wake_on" => {
                    c.wake_on = paneld_config::value::split_list(raw)
                        .unwrap_or_else(|| vec![raw.to_string()])
                        .into_iter()
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect()
                }
                "first_tap" => c.first_tap_wakes = raw != "act",
                _ => {}
            }
        }
        c.night = b.items.iter().find_map(|it| match it {
            Item::Block(n) if n.kind.text == "night" => {
                let mut night = Night::default();
                let (mut from, mut to) = (None, None);
                for (key, raw) in assigns(n) {
                    match key {
                        "from" => from = parse_clock(raw),
                        "to" => to = parse_clock(raw),
                        "theme" => night.theme = Some(raw.to_string()),
                        "idle" => night.idle = paneld_config::value::parse_duration(raw),
                        "brightness" => night.brightness = parse_level(raw),
                        "fps" => night.fps = parse_fps(raw),
                        _ => {}
                    }
                }
                night.from = from?;
                night.to = to?;
                Some(night)
            }
            _ => None,
        });
        c
    }
}

fn assigns(b: &Block) -> impl Iterator<Item = (&str, &str)> {
    b.items.iter().filter_map(|it| match it {
        Item::Assign(a) => match &a.target {
            AssignTarget::Simple { key } => Some((key.text.as_str(), a.value.text.trim())),
            _ => None,
        },
        _ => None,
    })
}

fn parse_level(raw: &str) -> Option<u8> {
    raw.trim().parse::<u32>().ok().map(|v| v.min(100) as u8)
}

/// `fps`: 1..=60; anything else (the check refuses it) is ignored.
fn parse_fps(raw: &str) -> Option<u32> {
    raw.trim().parse::<u32>().ok().filter(|v| (1..=60).contains(v))
}

/// What the main loop must do after a [`Screen::step`].
#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    /// Run `off` and black the screen out (`absorb`: the cover takes
    /// the waking touch).
    Sleep { absorb: bool },
    /// Remove the cover, run `on`, reset the idle clock, redraw.
    Wake,
    /// Run `dim` with this level.
    Dim(u8),
    /// Switch to this theme (`None` = the config's normal one).
    Theme(Option<String>),
    /// Draw at this many frames a second.
    Fps(u32),
}

/// Inputs to one [`Screen::step`].
pub struct Inputs<'a> {
    pub now: SystemTime,
    /// Time since the last touch.
    pub inactive: Duration,
    /// The sleep cover reported a touch.
    pub touched: bool,
    /// Active alerts not acknowledged or snoozed: (id, severity).
    pub shown: &'a [(String, String)],
}

pub struct Screen {
    pub cfg: Config,
    zone: Zone,
    asleep: bool,
    /// Night state, level and frame rate last applied; `None` before
    /// the first step.
    night: Option<bool>,
    level: Option<u8>,
    fps: Option<u32>,
    /// Shown alert ids when the screen went to sleep.
    seen: BTreeSet<String>,
    /// Where `on` / `off` / `dim` run (the config folder).
    pub cwd: PathBuf,
}

impl Screen {
    pub fn new(cfg: Config, zone: Zone, cwd: PathBuf) -> Self {
        Self { cfg, zone, asleep: false, night: None, level: None, fps: None, seen: BTreeSet::new(), cwd }
    }

    pub fn asleep(&self) -> bool {
        self.asleep
    }

    /// Keep the run-time state (asleep, levels applied) of the screen
    /// this one replaces on reload.
    pub fn carry_from(&mut self, old: &Screen) {
        self.asleep = old.asleep;
        self.seen = old.seen.clone();
        self.level = old.level;
        self.fps = old.fps;
        // `night` stays `None` so the new config's theme and level are
        // applied on the first step.
    }

    fn is_night(&self, now: SystemTime) -> bool {
        let Some(n) = &self.cfg.night else { return false };
        let t = self.zone.local_seconds_of_day(now);
        if n.from <= n.to {
            n.from <= t && t < n.to
        } else {
            t >= n.from || t < n.to
        }
    }

    fn night_cfg(&self, night: bool) -> Option<&Night> {
        if night { self.cfg.night.as_ref() } else { None }
    }

    pub fn step(&mut self, i: &Inputs<'_>) -> Vec<Effect> {
        let mut out = Vec::new();
        let night = self.is_night(i.now);
        let n = self.night_cfg(night);
        let idle = n.and_then(|n| n.idle).or(self.cfg.idle);
        let level = n.and_then(|n| n.brightness).unwrap_or(self.cfg.brightness);
        let theme = n.and_then(|n| n.theme.clone());
        let fps = n.and_then(|n| n.fps).unwrap_or(self.cfg.fps);

        if self.night != Some(night) {
            let first = self.night.is_none();
            self.night = Some(night);
            // The first step keeps the config's own theme unless it is
            // night already.
            if !first || night {
                out.push(Effect::Theme(theme));
            }
        }

        // Asleep too: a paused refresh costs nothing at any rate, and
        // the wake draws at the right one.
        if self.fps != Some(fps) {
            self.fps = Some(fps);
            out.push(Effect::Fps(fps));
        }

        if self.asleep {
            let new_alert = i.shown.iter().any(|(id, sev)| {
                !self.seen.contains(id) && self.cfg.wake_on.iter().any(|w| w == sev)
            });
            // `first_tap = act` has no absorbing cover: any touch
            // shows up as a fresh inactivity clock.
            let touched = i.touched
                || (!self.cfg.first_tap_wakes && i.inactive < Duration::from_millis(500));
            if touched || new_alert {
                self.asleep = false;
                out.push(Effect::Wake);
                self.level = None;
            }
        } else if idle.is_some_and(|d| i.inactive >= d) {
            self.asleep = true;
            self.seen = i.shown.iter().map(|(id, _)| id.clone()).collect();
            out.push(Effect::Sleep { absorb: self.cfg.first_tap_wakes });
            return out;
        }

        if !self.asleep && self.level != Some(level) {
            self.level = Some(level);
            out.push(Effect::Dim(level));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(src: &str) -> Document {
        use paneld_config::lex::tokenize;
        use paneld_config::source::SourceMap;
        let mut map = SourceMap::new();
        let id = map.add(PathBuf::from("t.conf"), src.to_string());
        let (toks, _) = tokenize(id, src);
        let (d, diags) = paneld_config::parse(&toks, &map);
        assert!(diags.is_empty(), "{diags:?}");
        d
    }

    fn screen(src: &str) -> Screen {
        Screen::new(Config::from_doc(&doc(src)), Zone::default(), PathBuf::from("."))
    }

    fn at(h: u64, m: u64) -> SystemTime {
        // A UTC day (the default zone is UTC).
        SystemTime::UNIX_EPOCH + Duration::from_secs(20_000 * 86_400 + h * 3600 + m * 60)
    }

    fn step(s: &mut Screen, now: SystemTime, inactive: u64, touched: bool, shown: &[(&str, &str)]) -> Vec<Effect> {
        let shown: Vec<(String, String)> =
            shown.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect();
        s.step(&Inputs { now, inactive: Duration::from_secs(inactive), touched, shown: &shown })
    }

    #[test]
    fn parses_the_schema_example() {
        let c = Config::from_doc(&doc(
            "screen {\n theme = dark\n idle = 2m\n brightness = 80\n off = /bin/true\n\
             wake_on = [critical, warning]\n first_tap = act\n\
             night { from = 22:00; to = 07:00; theme = dim; idle = 30s; brightness = 20 }\n}\n",
        ));
        assert_eq!(c.idle, Some(Duration::from_secs(120)));
        assert_eq!(c.brightness, 80);
        assert_eq!(c.wake_on, vec!["critical", "warning"]);
        assert!(!c.first_tap_wakes);
        let n = c.night.unwrap();
        assert_eq!((n.from, n.to), (22 * 3600, 7 * 3600));
        assert_eq!(n.brightness, Some(20));
        // No `fps`: 30, and the night keeps it.
        assert_eq!((c.fps, n.fps), (30, None));
    }

    #[test]
    fn fps_and_a_night_fps_are_read() {
        let c = Config::from_doc(&doc("screen { fps = 12; night { from = 22:00; to = 07:00; fps = 4 } }\n"));
        assert_eq!((c.fps, c.night.unwrap().fps), (12, Some(4)));
        // Options: last one wins, as everywhere.
        let mut d = doc("screen { fps = 12 }\nscreen:fps = 20\n");
        paneld_config::apply_overrides(&mut d);
        assert_eq!(Config::from_doc(&d).fps, 20);
    }

    #[test]
    fn the_frame_rate_is_set_once_and_switched_by_the_night_hours() {
        let mut s = screen(
            "screen { idle = 10m; fps = 20\n\
             night { from = 22:00; to = 07:00; fps = 5 } }\n",
        );
        assert_eq!(step(&mut s, at(21, 0), 0, false, &[]), vec![Effect::Fps(20), Effect::Dim(100)]);
        assert_eq!(step(&mut s, at(21, 30), 0, false, &[]), vec![]);
        assert_eq!(
            step(&mut s, at(22, 0), 0, false, &[]),
            vec![Effect::Theme(None), Effect::Fps(5)]
        );
        // Asleep at night, the morning still switches it back.
        assert_eq!(step(&mut s, at(23, 0), 600, false, &[]), vec![Effect::Sleep { absorb: true }]);
        assert_eq!(step(&mut s, at(7, 0), 30_000, false, &[]), vec![Effect::Theme(None), Effect::Fps(20)]);
        // A night without `fps` keeps the day's.
        let mut s = screen("screen { fps = 15; night { from = 22:00; to = 07:00; brightness = 5 } }\n");
        assert!(step(&mut s, at(21, 0), 0, false, &[]).contains(&Effect::Fps(15)));
        assert!(!step(&mut s, at(22, 0), 0, false, &[]).iter().any(|e| matches!(e, Effect::Fps(_))));
    }

    #[test]
    fn a_reload_sets_the_new_frame_rate_only_when_it_changed() {
        let mut old = screen("screen { fps = 20 }\n");
        step(&mut old, at(12, 0), 0, false, &[]);
        let mut same = screen("screen { fps = 20; brightness = 100 }\n");
        same.carry_from(&old);
        assert!(!step(&mut same, at(12, 0), 0, false, &[]).iter().any(|e| matches!(e, Effect::Fps(_))));
        let mut new = screen("screen { fps = 10 }\n");
        new.carry_from(&old);
        assert!(step(&mut new, at(12, 0), 0, false, &[]).contains(&Effect::Fps(10)));
        // Removed: back to 30.
        let mut gone = screen("page p { }\n");
        gone.carry_from(&new);
        assert!(step(&mut gone, at(12, 0), 0, false, &[]).contains(&Effect::Fps(30)));
    }

    #[test]
    fn sleeps_after_idle_and_wakes_on_touch() {
        let mut s = screen("screen { idle = 60s; brightness = 70 }\n");
        assert_eq!(step(&mut s, at(12, 0), 0, false, &[]), vec![Effect::Fps(30), Effect::Dim(70)]);
        assert_eq!(step(&mut s, at(12, 0), 59, false, &[]), vec![]);
        assert_eq!(step(&mut s, at(12, 1), 60, false, &[]), vec![Effect::Sleep { absorb: true }]);
        assert!(s.asleep());
        assert_eq!(step(&mut s, at(12, 5), 300, false, &[]), vec![]);
        assert_eq!(
            step(&mut s, at(12, 6), 0, true, &[]),
            vec![Effect::Wake, Effect::Dim(70)]
        );
    }

    #[test]
    fn a_new_critical_alert_wakes_but_an_old_one_does_not() {
        let mut s = screen("screen { idle = 10s }\n");
        step(&mut s, at(12, 0), 0, false, &[]);
        step(&mut s, at(12, 0), 10, false, &[("a:old", "critical")]);
        assert!(s.asleep());
        assert_eq!(step(&mut s, at(12, 1), 70, false, &[("a:old", "critical")]), vec![]);
        assert_eq!(step(&mut s, at(12, 1), 70, false, &[("a:w", "warning")]), vec![]);
        assert!(step(&mut s, at(12, 2), 130, false, &[("a:new", "critical")]).contains(&Effect::Wake));
    }

    #[test]
    fn first_tap_act_wakes_on_activity_without_absorbing() {
        let mut s = screen("screen { idle = 10s; first_tap = act }\n");
        step(&mut s, at(12, 0), 0, false, &[]);
        assert_eq!(step(&mut s, at(12, 0), 10, false, &[]), vec![Effect::Sleep { absorb: false }]);
        assert!(step(&mut s, at(12, 1), 0, false, &[]).contains(&Effect::Wake));
    }

    #[test]
    fn night_hours_switch_theme_idle_and_brightness() {
        let mut s = screen(
            "screen { idle = 10m; brightness = 90\n\
             night { from = 22:00; to = 07:00; theme = dim; idle = 30s; brightness = 20 } }\n",
        );
        assert_eq!(step(&mut s, at(21, 59), 0, false, &[]), vec![Effect::Fps(30), Effect::Dim(90)]);
        assert_eq!(
            step(&mut s, at(22, 0), 0, false, &[]),
            vec![Effect::Theme(Some("dim".into())), Effect::Dim(20)]
        );
        // Night idle is 30s.
        assert_eq!(step(&mut s, at(23, 0), 30, false, &[]), vec![Effect::Sleep { absorb: true }]);
        step(&mut s, at(6, 0), 0, true, &[]);
        assert!(step(&mut s, at(7, 0), 0, false, &[]).contains(&Effect::Theme(None)));
    }

    #[test]
    fn no_idle_means_never_sleeping() {
        let mut s = screen("page p { }\n");
        step(&mut s, at(12, 0), 0, false, &[]);
        assert!(step(&mut s, at(12, 0), 1_000_000, false, &[]).is_empty());
    }
}
