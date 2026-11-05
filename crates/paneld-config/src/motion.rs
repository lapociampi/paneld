//! Theme motion (themes.md, "Motion"; ADR 0023): the `motion` level
//! and the transition values of the motion tokens, shared by the
//! schema check and the renderer.

use crate::value::parse_duration;

/// How much a theme animates. Each animation runs at its own level
/// and above; below it, the change happens at once.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    None,
    Reduced,
    Full,
}

impl Level {
    /// `full`, `reduced` or `none`.
    pub fn parse(s: &str) -> Option<Level> {
        match s.trim() {
            "full" => Some(Level::Full),
            "reduced" => Some(Level::Reduced),
            "none" => Some(Level::None),
            _ => None,
        }
    }
}

/// What a transition does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effect {
    None,
    /// Opacity from 0: needs `reduced`.
    Fade,
    /// Moved in from a side (a page) or from above (a node): needs
    /// `full`, since it redraws everything it crosses.
    Slide,
}

impl Effect {
    /// The lowest `motion` level it runs at.
    pub fn level(self) -> Level {
        match self {
            Effect::None => Level::None,
            Effect::Fade => Level::Reduced,
            Effect::Slide => Level::Full,
        }
    }
}

/// A transition value: `<fade|slide|none> [<duration>]`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Transition {
    pub effect: Effect,
    pub ms: u32,
}

impl Transition {
    pub const NONE: Transition = Transition { effect: Effect::None, ms: 0 };

    /// Whether it runs under `level`.
    pub fn runs_at(self, level: Level) -> bool {
        self.effect != Effect::None && self.ms > 0 && level >= self.effect.level()
    }
}

/// The longest any theme animation may take.
pub const MAX_MS: u32 = 2000;

/// Duration of a transition written without one.
pub const DEFAULT_MS: u32 = 200;

/// The minimum level of value changes (`arc`, `bar`, `chart`) and of
/// tap feedback.
pub const VALUE_LEVEL: Level = Level::Reduced;
pub const TAP_LEVEL: Level = Level::Reduced;

/// Parse a transition: `fade`, `slide 300ms`, `none`.
pub fn parse_transition(text: &str) -> Result<Transition, String> {
    let mut parts = text.split_whitespace();
    let effect = match parts.next() {
        Some("fade") => Effect::Fade,
        Some("slide") => Effect::Slide,
        Some("none") => Effect::None,
        _ => return Err(format!("expected a transition (`fade`, `slide` or `none`, then a duration), got `{text}`")),
    };
    let rest: Vec<&str> = parts.collect();
    let ms = if rest.is_empty() {
        if effect == Effect::None { 0 } else { DEFAULT_MS }
    } else {
        parse_ms(&rest.join(" "))?
    };
    Ok(Transition { effect, ms })
}

/// Parse a motion duration: `none` (0), or a duration of at most
/// [`MAX_MS`] (`150ms`, `0.3s`).
pub fn parse_ms(text: &str) -> Result<u32, String> {
    let t = text.trim();
    if t == "none" {
        return Ok(0);
    }
    let d = parse_duration(t).ok_or_else(|| format!("expected a duration (`200ms`, `0.3s`) or `none`, got `{t}`"))?;
    let ms = d.as_millis();
    if ms > MAX_MS as u128 {
        return Err(format!("`{t}` is too long for an animation: at most {MAX_MS}ms"));
    }
    Ok(ms as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transitions_parse_with_and_without_a_duration() {
        assert_eq!(parse_transition("fade"), Ok(Transition { effect: Effect::Fade, ms: DEFAULT_MS }));
        assert_eq!(parse_transition("slide 300ms"), Ok(Transition { effect: Effect::Slide, ms: 300 }));
        assert_eq!(parse_transition("fade 0.5s"), Ok(Transition { effect: Effect::Fade, ms: 500 }));
        assert_eq!(parse_transition("none"), Ok(Transition::NONE));
        assert!(parse_transition("wipe 100ms").is_err());
        assert!(parse_transition("fade 3s").is_err());
        assert!(parse_transition("fade soon").is_err());
        assert_eq!(parse_ms("none"), Ok(0));
        assert_eq!(parse_ms("80ms"), Ok(80));
    }

    #[test]
    fn each_effect_runs_from_its_level() {
        let fade = Transition { effect: Effect::Fade, ms: 200 };
        let slide = Transition { effect: Effect::Slide, ms: 200 };
        assert!(fade.runs_at(Level::Reduced) && fade.runs_at(Level::Full) && !fade.runs_at(Level::None));
        assert!(slide.runs_at(Level::Full) && !slide.runs_at(Level::Reduced));
        assert!(!Transition { effect: Effect::Fade, ms: 0 }.runs_at(Level::Full));
        assert_eq!(Level::parse("reduced"), Some(Level::Reduced));
        assert_eq!(Level::parse("some"), None);
    }
}
