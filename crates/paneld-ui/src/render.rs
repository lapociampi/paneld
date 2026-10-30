//! Walk a parsed [`paneld_config::Document`] and build an LVGL tree
//! on a target screen.
//!
//! Scope for the first panel milestone: a single page's static UI
//! only. Components, `repeat`, themes, event handlers, and
//! `{{ ... }}` expressions are deferred to their respective later
//! phases.
//!
//! Supported node kinds today:
//!
//! | Kind     | LVGL mapping                                   |
//! |----------|------------------------------------------------|
//! | `column` | plain `obj` with flex column flow              |
//! | `row`    | plain `obj` with flex row flow                 |
//! | `stack`  | plain `obj` with no layout (absolute children) |
//! | `text`   | `lv_label` with `value =` text                 |
//! | `button` | `lv_button` (children drawn inside it)         |
//! | `rect`   | plain `obj` with a background color            |
//!
//! Properties supported today: `width`, `height`, `padding`, `gap`,
//! `background` (literal hex), `color` (literal hex, on text).

use paneld_config::cel;
use paneld_config::parse::{AssignTarget, Block, Document, Item};
use paneld_config::value::{classify, ValueShape};

use crate::expr::{eval, Context};
use crate::ui::{Obj, Size};

/// Render the page named `page_name` from `doc` into `screen`. If no
/// such page exists, draws a plain "page not found" label as a
/// diagnostic.
pub fn render_page(doc: &Document, page_name: &str, ctx: &Context, screen: &Obj<'_>) {
    for item in &doc.items {
        if let Item::Block(b) = item {
            if b.kind.text == "page"
                && b.name.as_ref().is_some_and(|n| n.text == page_name)
            {
                render_page_body(b, ctx, screen);
                return;
            }
        }
    }
    screen.new_label(&format!("page `{page_name}` not found"));
}

fn render_page_body(page: &Block, ctx: &Context, screen: &Obj<'_>) {
    let page_root = screen.new_container();
    page_root.set_size(Size::Percent(100), Size::Percent(100));
    page_root.set_flex_column();
    page_root.set_padding(16);
    page_root.set_gap(8);

    for item in &page.items {
        match item {
            Item::Block(child) => render_node(child, ctx, &page_root),
            Item::Assign(_a) => {
                // Page-level keys (`title`, `icon`, `nav`) don't
                // affect the current single-page view. Skip them.
            }
            _ => {}
        }
    }
}

fn render_node(node: &Block, ctx: &Context, parent: &Obj<'_>) {
    match node.kind.text.as_str() {
        "column" => {
            let obj = parent.new_container();
            obj.set_size(Size::Content, Size::Content);
            obj.set_flex_column();
            apply_props(node, ctx, &obj);
            for item in &node.items {
                if let Item::Block(child) = item {
                    render_node(child, ctx, &obj);
                }
            }
        }
        "row" => {
            let obj = parent.new_container();
            obj.set_size(Size::Content, Size::Content);
            obj.set_flex_row();
            apply_props(node, ctx, &obj);
            for item in &node.items {
                if let Item::Block(child) = item {
                    render_node(child, ctx, &obj);
                }
            }
        }
        "stack" => {
            let obj = parent.new_container();
            obj.set_size(Size::Content, Size::Content);
            apply_props(node, ctx, &obj);
            for item in &node.items {
                if let Item::Block(child) = item {
                    render_node(child, ctx, &obj);
                }
            }
        }
        "text" => {
            let text = resolve_text(node, "value", ctx).unwrap_or_default();
            let obj = parent.new_label(&text);
            apply_props(node, ctx, &obj);
        }
        "button" => {
            let obj = parent.new_button();
            apply_props(node, ctx, &obj);
            for item in &node.items {
                if let Item::Block(child) = item {
                    render_node(child, ctx, &obj);
                }
            }
        }
        "rect" => {
            let obj = parent.new_container();
            obj.set_size(Size::Content, Size::Content);
            apply_props(node, ctx, &obj);
        }
        other => {
            // Unknown or not-yet-supported widget: draw a stub so
            // the user sees something and knows which node is
            // missing instead of silently ignoring it.
            parent.new_label(&format!("[{other}: not implemented yet]"));
        }
    }
}

/// Evaluate a `key = ...` on `node` and return it as a display
/// string. Three shapes are honored:
///
/// - `Literal`:  returned verbatim (`value = Hello`).
/// - `Expression`: parsed as CEL, evaluated, [`Value::to_display_string`].
/// - `Interp`: each `{{ ... }}` fragment is parsed+evaluated and
///   spliced back into the surrounding literal (`value = Temp
///   is {{ x }} C`).
///
/// On eval error the node renders the error message so the user sees
/// the problem instead of a blank panel.
fn resolve_text(node: &Block, key: &str, ctx: &Context) -> Option<String> {
    let (raw, span) = find_raw(node, key)?;
    Some(match classify(raw) {
        ValueShape::Literal(text) => text.to_string(),
        ValueShape::Expression(body) => {
            let (expr, diags) = cel::parse_fragment(body, span.file, span.start + 2);
            if !diags.is_empty() {
                return Some(format!("[parse error: {}]", diags[0].message));
            }
            match expr.and_then(|e| eval(&e, ctx).ok()) {
                Some(v) => v.to_display_string(),
                None => "[eval error]".to_string(),
            }
        }
        ValueShape::Interp(_) => {
            let (fragments, diags) = cel::parse_interp(raw, span.file, span.start);
            if !diags.is_empty() {
                return Some(format!("[parse error: {}]", diags[0].message));
            }
            interpolate_text(raw, &fragments, ctx)
        }
    })
}

/// Walk `raw` left to right, replacing each `{{ ... }}` with the
/// evaluated fragment's display string and leaving literal text
/// (including `\{{` escapes handled by the lexer) intact.
///
/// `fragments` is the ordered list of parsed CEL ASTs; we match them
/// up to the openers encountered in `raw`.
fn interpolate_text(raw: &str, fragments: &[cel::Expr], ctx: &Context) -> String {
    let mut out = String::with_capacity(raw.len());
    let bytes = raw.as_bytes();
    let mut i = 0;
    let mut frag_idx = 0;
    while i < bytes.len() {
        // Mirror the lexer's `\\{{` and `\{{` handling.
        if bytes[i] == b'\\' && bytes.get(i + 1) == Some(&b'\\') {
            out.push_str("\\\\");
            i += 2;
            continue;
        }
        if bytes[i] == b'\\'
            && bytes.get(i + 1) == Some(&b'{')
            && bytes.get(i + 2) == Some(&b'{')
        {
            out.push_str("{{");
            i += 3;
            continue;
        }
        if bytes[i] == b'{' && bytes.get(i + 1) == Some(&b'{') {
            // Advance past the matching `}}` using the same scan the
            // CEL parser used.
            let mut j = i + 2;
            let mut depth: u32 = 0;
            while j < bytes.len() {
                match bytes[j] {
                    b'"' | b'\'' => {
                        let q = bytes[j];
                        j += 1;
                        while j < bytes.len() {
                            if bytes[j] == b'\\' {
                                j += 2;
                                continue;
                            }
                            if bytes[j] == q {
                                j += 1;
                                break;
                            }
                            j += 1;
                        }
                    }
                    b'{' => {
                        depth = depth.saturating_add(1);
                        j += 1;
                    }
                    b'}' => {
                        if depth > 0 {
                            depth -= 1;
                            j += 1;
                        } else if bytes.get(j + 1) == Some(&b'}') {
                            j += 2;
                            break;
                        } else {
                            j += 1;
                        }
                    }
                    _ => j += 1,
                }
            }
            if let Some(expr) = fragments.get(frag_idx) {
                match eval(expr, ctx) {
                    Ok(v) => out.push_str(&v.to_display_string()),
                    Err(e) => out.push_str(&format!("[{}]", e.message)),
                }
            }
            frag_idx += 1;
            i = j;
            continue;
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

fn apply_props(node: &Block, _ctx: &Context, obj: &Obj<'_>) {
    for item in &node.items {
        let Item::Assign(a) = item else { continue };
        let AssignTarget::Simple { key } = &a.target else {
            continue;
        };
        let value = a.value.text.trim();
        match key.text.as_str() {
            "width" => {
                if let Some(s) = parse_size(value) {
                    obj.set_size(s, Size::Content);
                }
            }
            "height" => {
                if let Some(s) = parse_size(value) {
                    obj.set_size(Size::Content, s);
                }
            }
            "padding" => {
                if let Ok(px) = value.parse::<i32>() {
                    obj.set_padding(px);
                }
            }
            "gap" => {
                if let Ok(px) = value.parse::<i32>() {
                    obj.set_gap(px);
                }
            }
            "background" => {
                if let Some(rgb) = parse_hex_color(value) {
                    obj.set_background_hex(rgb);
                }
            }
            "color" => {
                if let Some(rgb) = parse_hex_color(value) {
                    obj.set_text_color_hex(rgb);
                }
            }
            _ => {
                // Not yet wired; silently ignore so the daemon
                // doesn't crash on properties that are legal
                // schema-wise but need engine work (theme resolution,
                // event bindings).
            }
        }
    }
}

/// Find a simple `key = value` on `node`, returning the raw text and
/// its span. The span lets a caller translate inner `{{ ... }}`
/// offsets back to the enclosing source file.
fn find_raw<'a>(
    node: &'a Block,
    key: &str,
) -> Option<(&'a str, paneld_config::source::Span)> {
    for item in &node.items {
        let Item::Assign(a) = item else { continue };
        let AssignTarget::Simple { key: k } = &a.target else {
            continue;
        };
        if k.text == key {
            return Some((a.value.text.as_str(), a.value.span));
        }
    }
    None
}

fn parse_size(s: &str) -> Option<Size> {
    let s = s.trim();
    if s == "content" {
        return Some(Size::Content);
    }
    if let Some(p) = s.strip_suffix('%') {
        return p.trim().parse::<u16>().ok().map(Size::Percent);
    }
    s.parse::<i32>().ok().map(Size::Px)
}

/// Accept `#rgb`, `#rrggbb`, `#rrggbbaa` (alpha ignored for now).
fn parse_hex_color(s: &str) -> Option<u32> {
    let hex = s.trim().strip_prefix('#')?;
    let (r, g, b) = match hex.len() {
        3 => {
            let r = u8::from_str_radix(&hex[0..1], 16).ok()?;
            let g = u8::from_str_radix(&hex[1..2], 16).ok()?;
            let b = u8::from_str_radix(&hex[2..3], 16).ok()?;
            (r * 17, g * 17, b * 17)
        }
        6 | 8 => {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            (r, g, b)
        }
        _ => return None,
    };
    Some(((r as u32) << 16) | ((g as u32) << 8) | (b as u32))
}
