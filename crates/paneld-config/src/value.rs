//! Parsers for the primitive value types the schema uses.
//!
//! Each function takes the raw text of a value (as it comes out of the
//! parser, after `$var` substitution) and returns a typed value or
//! `None` if the text does not parse as that type. Nothing here reads
//! the schema; dispatching to the right parser is the schema check's
//! job (Phase 5+).
//!
//! Expressions (`{{ ... }}`) and mixed-interpolation values short-circuit
//! these parsers: a key whose value is an expression can only be
//! resolved at runtime by CEL, so the schema check accepts
//! [`ValueShape::Expression`] / [`ValueShape::Interp`] for any
//! expr-allowing type.

use std::time::Duration;

/// Classify a raw value's top-level shape.
///
/// - [`Literal`]: no `{{ }}` in the raw text; the string stands on
///   its own and the type-specific parsers can look at it.
/// - [`Expression`]: the raw text is exactly `{{ expr }}` with no
///   literal content around it. The expression's own CEL type will
///   be checked later.
/// - [`Interp`]: a mix of literal text and one or more `{{ }}`. The
///   result at runtime is always a string.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ValueShape<'a> {
    Literal(&'a str),
    Expression(&'a str),
    Interp(&'a str),
}

/// Decide the top-level shape of a raw value's text. The `text` here
/// is `RawValue::text` from the parser, trailing spaces already trimmed.
pub fn classify(text: &str) -> ValueShape<'_> {
    let trimmed = text.trim();
    if trimmed.starts_with("{{") && trimmed.ends_with("}}") {
        // One expression spanning the whole value, with no other content.
        // Verify by scanning: the first `}}` at depth 0 must be the final
        // one. If something sits between, this is Interp.
        if is_single_expression(trimmed) {
            return ValueShape::Expression(&trimmed[2..trimmed.len() - 2]);
        }
    }
    if contains_expression(trimmed) {
        return ValueShape::Interp(trimmed);
    }
    ValueShape::Literal(trimmed)
}

fn is_single_expression(s: &str) -> bool {
    // s begins with "{{"; find the matching "}}" and check nothing
    // non-ws remains after.
    let bytes = s.as_bytes();
    let mut i = 2;
    let mut depth: u32 = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'{' => {
                depth = depth.saturating_add(1);
                i += 1;
            }
            b'}' => {
                if depth > 0 {
                    depth -= 1;
                    i += 1;
                } else if i + 1 < bytes.len() && bytes[i + 1] == b'}' {
                    // End of expression.
                    return i + 2 == bytes.len();
                } else {
                    i += 1;
                }
            }
            b'"' | b'\'' => {
                let q = bytes[i];
                i += 1;
                while i < bytes.len() {
                    match bytes[i] {
                        b'\\' => i = (i + 2).min(bytes.len()),
                        c if c == q => {
                            i += 1;
                            break;
                        }
                        _ => i += 1,
                    }
                }
            }
            _ => i += 1,
        }
    }
    false
}

fn contains_expression(s: &str) -> bool {
    // Scan outside of quotes for `{{`, honoring the `\{{` escape.
    // `\\` is a literal backslash pair, so `\\{{` is NOT an escape --
    // the `\\` consumes both bytes and the following `{{` opens an
    // expression. Only an unpaired `\` immediately before `{{` elides
    // the opener.
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if bytes.get(i + 1) == Some(&b'\\') => {
                i += 2;
            }
            b'\\' if bytes.get(i + 1) == Some(&b'{')
                && bytes.get(i + 2) == Some(&b'{') =>
            {
                i += 3;
            }
            b'{' if bytes.get(i + 1) == Some(&b'{') => return true,
            b'"' => {
                i += 1;
                while i < bytes.len() {
                    match bytes[i] {
                        b'\\' => i = (i + 2).min(bytes.len()),
                        b'"' => {
                            i += 1;
                            break;
                        }
                        _ => i += 1,
                    }
                }
            }
            _ => i += 1,
        }
    }
    false
}

// --- bool ---

pub fn parse_bool(text: &str) -> Option<bool> {
    match text.trim() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

// --- integer and number ---

pub fn parse_int(text: &str) -> Option<i64> {
    let t = text.trim();
    t.parse::<i64>().ok()
}

/// Parse a decimal number: integer, float, or scientific (`1e9`).
pub fn parse_number(text: &str) -> Option<f64> {
    let t = text.trim();
    t.parse::<f64>().ok()
}

// --- duration ---

/// Parse a duration literal: `500ms`, `30s`, `2m`, `1h`, `1d`,
/// `1 month`. A space before the unit is tolerated.
/// `YYYY-MM-DD` as `(year, month, day)`, checked against the month's
/// length.
pub fn parse_date(text: &str) -> Option<(i64, u32, u32)> {
    let mut it = text.trim().split('-');
    let y: i64 = it.next()?.parse().ok()?;
    let m: u32 = it.next()?.parse().ok()?;
    let d: u32 = it.next()?.parse().ok()?;
    if it.next().is_some() || !(1..=12).contains(&m) || d == 0 {
        return None;
    }
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let len = match m {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    (d <= len).then_some((y, m, d))
}

const MAX_DURATION_SECS: f64 = 100.0 * 365.0 * 86_400.0;

pub fn parse_duration(text: &str) -> Option<Duration> {
    let t = text.trim();
    // Split on first non-numeric (and non-`.`, non-`-`) byte.
    let bytes = t.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b.is_ascii_digit() || b == b'.' {
            i += 1;
        } else {
            break;
        }
    }
    if i == 0 {
        return None;
    }
    let num: f64 = t[..i].parse().ok()?;
    let unit = t[i..].trim().to_ascii_lowercase();
    let seconds = match unit.as_str() {
        "ms" => num / 1_000.0,
        "s" => num,
        "m" => num * 60.0,
        "h" => num * 3_600.0,
        "d" | "day" | "days" => num * 86_400.0,
        "w" | "week" | "weeks" => num * 7.0 * 86_400.0,
        "month" | "months" => num * 30.0 * 86_400.0,
        "y" | "year" | "years" => num * 365.0 * 86_400.0,
        _ => return None,
    };
    // Over 100 years is a typo, and adding it to a clock would
    // overflow (a panic in `Instant + Duration`).
    if seconds.is_nan() || !(0.0..=MAX_DURATION_SECS).contains(&seconds) {
        return None;
    }
    let whole = seconds.trunc() as u64;
    let nanos = ((seconds - seconds.trunc()) * 1e9).round() as u32;
    Some(Duration::new(whole, nanos))
}

// --- size ---

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Size {
    /// Pixels.
    Px(f64),
    /// Percentage of the parent's cross-axis extent (0.0 .. 100.0).
    Percent(f64),
    /// Share of the row/column's remaining space.
    Grow,
    /// Size to the content.
    Content,
}

pub fn parse_size(text: &str) -> Option<Size> {
    let t = text.trim();
    match t {
        "grow" => return Some(Size::Grow),
        "content" => return Some(Size::Content),
        _ => {}
    }
    if let Some(stripped) = t.strip_suffix('%') {
        return stripped.trim().parse::<f64>().ok().map(Size::Percent);
    }
    t.parse::<f64>().ok().map(Size::Px)
}

// --- bytes ---

/// Parse a byte count like `512`, `64KiB`, `1MiB`. IEC binary units;
/// decimal units are not accepted (the spec uses `KiB`/`MiB`).
pub fn parse_bytes(text: &str) -> Option<u64> {
    let t = text.trim();
    let bytes = t.as_bytes();
    let mut i = 0;
    while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
        i += 1;
    }
    if i == 0 {
        return None;
    }
    let num: f64 = t[..i].parse().ok()?;
    let unit = t[i..].trim();
    let mult: f64 = match unit {
        "" | "B" => 1.0,
        "KiB" => 1024.0,
        "MiB" => 1024.0 * 1024.0,
        "GiB" => 1024.0 * 1024.0 * 1024.0,
        "TiB" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    let total = num * mult;
    if total.is_nan() || total.is_infinite() || total < 0.0 {
        return None;
    }
    // Reject counts that overflow a `u64`. Casting an f64 larger than
    // `u64::MAX` otherwise saturates to `u64::MAX` silently, which
    // would turn `1000TiB` into a valid-looking max value.
    if total >= (u64::MAX as f64) {
        return None;
    }
    Some(total.round() as u64)
}

// --- color ---

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

/// Parse a hex color literal: `#rgb`, `#rrggbb`, `#rrggbbaa`. `#rgb`
/// expands each nibble (so `#f80` is `#ff8800`).
pub fn parse_color(text: &str) -> Option<Color> {
    let t = text.trim();
    let hex = t.strip_prefix('#')?;
    if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    fn h(b: u8) -> u8 {
        match b {
            b'0'..=b'9' => b - b'0',
            b'a'..=b'f' => b - b'a' + 10,
            b'A'..=b'F' => b - b'A' + 10,
            _ => 0,
        }
    }
    let bytes = hex.as_bytes();
    match bytes.len() {
        3 => Some(Color {
            r: h(bytes[0]) * 17,
            g: h(bytes[1]) * 17,
            b: h(bytes[2]) * 17,
            a: 255,
        }),
        6 => Some(Color {
            r: h(bytes[0]) * 16 + h(bytes[1]),
            g: h(bytes[2]) * 16 + h(bytes[3]),
            b: h(bytes[4]) * 16 + h(bytes[5]),
            a: 255,
        }),
        8 => Some(Color {
            r: h(bytes[0]) * 16 + h(bytes[1]),
            g: h(bytes[2]) * 16 + h(bytes[3]),
            b: h(bytes[4]) * 16 + h(bytes[5]),
            a: h(bytes[6]) * 16 + h(bytes[7]),
        }),
        _ => None,
    }
}

// --- string (unquoting) ---

/// Return the content of a quoted string with escapes resolved, or the
/// original text if it is not quoted. Only `\"`, `\\`, `\n`, `\t`, `\r`
/// are honored; unknown escapes keep the backslash.
pub fn parse_string(text: &str) -> String {
    let t = text.trim();
    let inner = match t.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        Some(inner) => inner,
        None => return t.to_string(),
    };
    let mut out = String::with_capacity(inner.len());
    let mut it = inner.chars();
    while let Some(c) = it.next() {
        if c == '\\' {
            match it.next() {
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

// --- list literal: `[a, b, c]` ---

/// Return the inner items of a bracketed list literal, split on
/// top-level commas (not inside quotes, braces, or expressions).
///
/// This is a lightweight tokenizer: each item is returned as a raw
/// slice. The caller still has to parse each item as its own type.
pub fn split_list(text: &str) -> Option<Vec<String>> {
    let t = text.trim();
    let inner = t.strip_prefix('[').and_then(|s| s.strip_suffix(']'))?;
    let bytes = inner.as_bytes();
    let mut items = Vec::new();
    let mut depth_brace: u32 = 0;
    let mut depth_bracket: u32 = 0;
    let mut i = 0;
    let mut start = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'"' | b'\'' => {
                let q = bytes[i];
                i += 1;
                while i < bytes.len() {
                    match bytes[i] {
                        b'\\' => i = (i + 2).min(bytes.len()),
                        c if c == q => {
                            i += 1;
                            break;
                        }
                        _ => i += 1,
                    }
                }
            }
            b'{' if bytes.get(i + 1) == Some(&b'{') => {
                // Treat `{{ ... }}` as opaque.
                i += 2;
                let mut d: u32 = 0;
                while i < bytes.len() {
                    match bytes[i] {
                        b'{' => {
                            d = d.saturating_add(1);
                            i += 1;
                        }
                        b'}' => {
                            if d > 0 {
                                d -= 1;
                                i += 1;
                            } else if bytes.get(i + 1) == Some(&b'}') {
                                i += 2;
                                break;
                            } else {
                                i += 1;
                            }
                        }
                        _ => i += 1,
                    }
                }
            }
            b'{' => {
                depth_brace += 1;
                i += 1;
            }
            b'}' => {
                depth_brace = depth_brace.saturating_sub(1);
                i += 1;
            }
            b'[' => {
                depth_bracket += 1;
                i += 1;
            }
            b']' => {
                depth_bracket = depth_bracket.saturating_sub(1);
                i += 1;
            }
            b',' if depth_brace == 0 && depth_bracket == 0 => {
                items.push(inner[start..i].trim().to_string());
                i += 1;
                start = i;
            }
            _ => i += 1,
        }
    }
    let last = inner[start..].trim();
    if !last.is_empty() {
        items.push(last.to_string());
    } else if items.is_empty() && !inner.trim().is_empty() {
        // The input had content but no commas (e.g. a single item that
        // trimmed to empty was already handled above). Fall through.
    }
    // A trailing comma (`[a, b, ]`) leaves `last` empty while `items`
    // is non-empty; dropping the empty final item makes the trailing
    // comma a benign style choice rather than a phantom entry.
    Some(items)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_shapes() {
        assert!(matches!(classify("hello"), ValueShape::Literal("hello")));
        assert!(matches!(classify("{{ x }}"), ValueShape::Expression(_)));
        assert!(matches!(classify("CPU {{ x }} C"), ValueShape::Interp(_)));
        assert!(matches!(classify("{{ a }}{{ b }}"), ValueShape::Interp(_)));
        assert!(matches!(classify("{{ a }} "), ValueShape::Expression(_)));
    }

    #[test]
    fn classify_expression_ignores_escaped_opener() {
        // `\{{` is literal text, so this is not an expression at all.
        assert!(matches!(classify(r"\{{ x }}"), ValueShape::Literal(_)));
    }

    #[test]
    fn bool_cases() {
        assert_eq!(parse_bool("true"), Some(true));
        assert_eq!(parse_bool(" false "), Some(false));
        assert_eq!(parse_bool("TRUE"), None);
        assert_eq!(parse_bool("yes"), None);
    }

    #[test]
    fn number_cases() {
        assert_eq!(parse_number("12"), Some(12.0));
        assert_eq!(parse_number("1.5"), Some(1.5));
        assert_eq!(parse_number("1e9"), Some(1_000_000_000.0));
        assert_eq!(parse_number("-3"), Some(-3.0));
        assert_eq!(parse_number("abc"), None);
    }

    #[test]
    fn int_cases() {
        assert_eq!(parse_int("42"), Some(42));
        assert_eq!(parse_int("-7"), Some(-7));
        assert_eq!(parse_int("1.5"), None);
    }

    #[test]
    fn duration_cases() {
        assert_eq!(parse_duration("500ms"), Some(Duration::from_millis(500)));
        assert_eq!(parse_duration("30s"), Some(Duration::from_secs(30)));
        assert_eq!(parse_duration("2m"), Some(Duration::from_secs(120)));
        assert_eq!(parse_duration("1h"), Some(Duration::from_secs(3_600)));
        assert_eq!(parse_duration("1d"), Some(Duration::from_secs(86_400)));
        assert_eq!(
            parse_duration("1 month"),
            Some(Duration::from_secs(30 * 86_400))
        );
        assert_eq!(parse_duration("1.5s"), Some(Duration::from_millis(1_500)));
        assert_eq!(parse_duration("1w"), Some(Duration::from_secs(7 * 86_400)));
        assert_eq!(parse_duration("2 weeks"), Some(Duration::from_secs(14 * 86_400)));
        assert_eq!(parse_duration("3 days"), Some(Duration::from_secs(3 * 86_400)));
        assert_eq!(parse_duration("100y"), Some(Duration::from_secs(100 * 365 * 86_400)));
        assert_eq!(parse_duration("101y"), None);
        assert_eq!(parse_duration("300000000000y"), None);
        assert_eq!(parse_duration("1e30s"), None);
        assert_eq!(parse_duration("foo"), None);
        assert_eq!(parse_duration("5"), None); // missing unit
    }

    #[test]
    fn date_cases() {
        assert_eq!(parse_date("2026-11-01"), Some((2026, 11, 1)));
        assert_eq!(parse_date("2028-02-29"), Some((2028, 2, 29)));
        assert_eq!(parse_date("2026-02-29"), None);
        assert_eq!(parse_date("2026-11"), None);
        assert_eq!(parse_date("tomorrow"), None);
    }

    #[test]
    fn size_cases() {
        assert_eq!(parse_size("120"), Some(Size::Px(120.0)));
        assert_eq!(parse_size("50%"), Some(Size::Percent(50.0)));
        assert_eq!(parse_size("grow"), Some(Size::Grow));
        assert_eq!(parse_size("content"), Some(Size::Content));
        assert_eq!(parse_size("foo"), None);
    }

    #[test]
    fn bytes_cases() {
        assert_eq!(parse_bytes("512"), Some(512));
        assert_eq!(parse_bytes("64KiB"), Some(64 * 1024));
        assert_eq!(parse_bytes("1MiB"), Some(1024 * 1024));
        assert_eq!(parse_bytes("512B"), Some(512));
        assert_eq!(parse_bytes("foo"), None);
        assert_eq!(parse_bytes("1KB"), None); // decimal not supported
        // Values that overflow u64 must be refused rather than
        // saturating silently. `u64::MAX` is ~1.8e19 bytes = ~1.6e7
        // TiB; 1e8 TiB is just past that threshold.
        assert_eq!(parse_bytes("100000000TiB"), None);
        assert_eq!(parse_bytes("16384PiB"), None); // unknown unit, still None
    }

    #[test]
    fn color_cases() {
        assert_eq!(
            parse_color("#f80"),
            Some(Color { r: 0xff, g: 0x88, b: 0x00, a: 0xff })
        );
        assert_eq!(
            parse_color("#44aaff"),
            Some(Color { r: 0x44, g: 0xaa, b: 0xff, a: 0xff })
        );
        assert_eq!(
            parse_color("#44aaff80"),
            Some(Color { r: 0x44, g: 0xaa, b: 0xff, a: 0x80 })
        );
        assert_eq!(parse_color("44aaff"), None); // missing #
        assert_eq!(parse_color("#gggggg"), None);
        assert_eq!(parse_color("#1234"), None); // 4 digits not valid
    }

    #[test]
    fn string_cases() {
        assert_eq!(parse_string("hello"), "hello");
        assert_eq!(parse_string("\"hello\""), "hello");
        assert_eq!(parse_string("\" leading and trailing \""), " leading and trailing ");
        assert_eq!(parse_string(r#""a\"b""#), "a\"b");
        assert_eq!(parse_string(r#""line\nbreak""#), "line\nbreak");
    }

    #[test]
    fn list_split_cases() {
        assert_eq!(split_list("[]"), Some(vec![]));
        assert_eq!(split_list("[1]"), Some(vec!["1".into()]));
        assert_eq!(split_list("[1, 2, 3]"), Some(vec!["1".into(), "2".into(), "3".into()]));
        assert_eq!(
            split_list("[\"a,b\", c]"),
            Some(vec!["\"a,b\"".into(), "c".into()])
        );
        assert_eq!(
            split_list("[{{ a, b }}, c]"),
            Some(vec!["{{ a, b }}".into(), "c".into()])
        );
        assert_eq!(split_list("not a list"), None);
        // Trailing comma: the final empty slot is dropped so `[a, b, ]`
        // is read as `[a, b]`.
        assert_eq!(
            split_list("[a, b, ]"),
            Some(vec!["a".into(), "b".into()])
        );
        assert_eq!(split_list("[a,]"), Some(vec!["a".into()]));
    }
}
