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

use std::sync::mpsc::Sender;

use paneld_config::cel;
use paneld_config::parse::{AssignTarget, Block, Document, Item};
use paneld_config::value::{classify, ValueShape};

use crate::expr::{eval, Context, Value};
use crate::ui::{ActionArg, FlexAlign, InlineSetItem, Obj, Size, Trigger, UiEvent};

/// Children of a component call plus the outer context they should
/// resolve against. Threaded through [`render_node`] so a `slot { }`
/// inside the component's body expands to the caller's children.
struct SlotCtx<'a> {
    children: &'a [Item],
    outer_ctx: &'a Context,
}

/// Render the page named `page_name` from `doc` into `screen`. If no
/// such page exists, draws a plain "page not found" label as a
/// diagnostic.
/// Show `text` as a toast on `layer` (the top layer), replacing any
/// earlier one: a pill near the bottom in `background` (a hex color,
/// e.g. a theme token) with white text. The caller removes it with
/// `layer.clear_children()` when it expires.
pub fn render_toast(layer: &Obj<'_>, text: &str, background: &str) {
    layer.clear_children();
    let pill = layer.new_container();
    pill.set_size(Size::Content, Size::Content);
    pill.set_padding(12);
    pill.set_radius(8);
    pill.set_border(0, None);
    if let Some(rgb) = parse_hex_color(background) {
        pill.set_background_hex(rgb);
    }
    let label = pill.new_label(text);
    label.set_text_color_hex(0xffffff);
    // Above the shell's nav bar.
    pill.align_bottom_mid(-72);
}

pub fn render_page(
    doc: &Document,
    page_name: &str,
    ctx: &Context,
    sink: &Sender<UiEvent>,
    screen: &Obj<'_>,
) {
    for item in &doc.items {
        if let Item::Block(b) = item {
            if b.kind.text == "page"
                && b.name.as_ref().is_some_and(|n| n.text == page_name)
            {
                render_page_body(b, doc, ctx, sink, screen);
                return;
            }
        }
    }
    screen.new_label(&format!("page `{page_name}` not found"));
}

fn render_page_body(
    page: &Block,
    doc: &Document,
    ctx: &Context,
    sink: &Sender<UiEvent>,
    screen: &Obj<'_>,
) {
    // `page` and `nav` are visible to the shell and to the page body.
    let mut pctx = ctx.clone();
    pctx.insert("page".into(), page_info(page, ctx));
    pctx.insert("nav".into(), nav_entries(doc, page, ctx));

    let page_root = screen.new_container();
    page_root.set_size(Size::Percent(100), Size::Percent(100));
    page_root.set_flex_column();
    page_root.set_border(0, None);
    page_root.set_radius(0);

    match find_component(doc, "shell") {
        // Schema 8: the `shell` component wraps every page; the page's
        // nodes fill its `slot`.
        Some(shell) => {
            page_root.set_padding(0);
            page_root.set_gap(0);
            let slot = SlotCtx { children: &page.items, outer_ctx: &pctx };
            render_item_list(&shell.items, doc, &pctx, Some(&slot), sink, &page_root);
        }
        None => {
            page_root.set_padding(16);
            page_root.set_gap(8);
            render_item_list(&page.items, doc, &pctx, None, sink, &page_root);
        }
    }
}

/// `{ name, title, icon }` of a page; `title` defaults to the name.
fn page_info(page: &Block, ctx: &Context) -> Value {
    let name = page.name.as_ref().map(|n| n.text.clone()).unwrap_or_default();
    let title = find_raw(page, "title")
        .map(|(raw, span)| resolve_scalar(raw, span, ctx))
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| name.clone());
    let icon = find_simple(page, "icon").unwrap_or("").trim().to_string();
    let mut m = std::collections::BTreeMap::new();
    m.insert("name".to_string(), Value::String(name));
    m.insert("title".to_string(), Value::String(title));
    m.insert("icon".to_string(), Value::String(icon));
    Value::Map(m)
}

/// Pages that have `nav = <n>`, ordered by `n` (schema 8), each as
/// `page_info` plus `current` (the page being shown).
fn nav_entries(doc: &Document, current: &Block, ctx: &Context) -> Value {
    let mut entries: Vec<(i64, Value)> = Vec::new();
    for item in &doc.items {
        let Item::Block(b) = item else { continue };
        if b.kind.text != "page" {
            continue;
        }
        let Some(pos) = find_simple(b, "nav").and_then(|v| v.trim().parse::<i64>().ok()) else {
            continue;
        };
        let mut info = page_info(b, ctx);
        if let Value::Map(m) = &mut info {
            let is_current = b.name.as_ref().map(|n| &n.text) == current.name.as_ref().map(|n| &n.text);
            m.insert("current".to_string(), Value::Bool(is_current));
        }
        entries.push((pos, info));
    }
    entries.sort_by_key(|(pos, _)| *pos);
    Value::List(entries.into_iter().map(|(_, v)| v).collect())
}

/// Walk a slice of `Item`s inside a UI container, dispatching each
/// one to the right path: `Block` -> `render_node`, `Repeat` ->
/// iterate the source list and render per-item.
fn render_item_list(
    items: &[Item],
    doc: &Document,
    ctx: &Context,
    slot: Option<&SlotCtx<'_>>,
    sink: &Sender<UiEvent>,
    parent: &Obj<'_>,
) {
    for item in items {
        match item {
            Item::Block(child) => {
                if is_event_handler_block(&child.kind.text) {
                    // Handled by apply_props on the parent; don't
                    // render as a child UI node.
                    continue;
                }
                render_node(child, doc, ctx, slot, sink, parent);
            }
            Item::Repeat(r) => expand_repeat(r, doc, ctx, slot, sink, parent),
            _ => {}
        }
    }
}

fn is_event_handler_block(kind: &str) -> bool {
    matches!(kind, "on_tap" | "on_hold" | "on_change" | "on_submit")
}

/// Expand a `repeat BIND in {{ EXPR }} { ... }`: evaluate the
/// source expression, iterate the resulting list, and render the
/// contained items for each element with `BIND` bound to the item.
fn expand_repeat(
    r: &paneld_config::parse::Repeat,
    doc: &Document,
    ctx: &Context,
    slot: Option<&SlotCtx<'_>>,
    sink: &Sender<UiEvent>,
    parent: &Obj<'_>,
) {
    let body_offset = r.source.span.start + 2;
    let (expr, diags) =
        cel::parse_fragment(&r.source.body, r.source.span.file, body_offset);
    if !diags.is_empty() {
        parent.new_label(&format!("[repeat parse: {}]", diags[0].message));
        return;
    }
    let Some(expr) = expr else {
        return;
    };
    let items = match eval(&expr, ctx) {
        Ok(Value::List(xs)) => xs,
        Ok(other) => {
            parent.new_label(&format!(
                "[repeat: expected a list, got {}]",
                display_type(&other)
            ));
            return;
        }
        Err(e) => {
            parent.new_label(&format!("[repeat eval: {}]", e.message));
            return;
        }
    };
    for item in items {
        let mut inner = ctx.clone();
        inner.insert(r.binding.text.clone(), item);
        render_item_list(&r.items, doc, &inner, slot, sink, parent);
    }
}

fn display_type(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Int(_) => "int",
        Value::Double(_) => "number",
        Value::String(_) => "string",
        Value::List(_) => "list",
        Value::Map(_) => "map",
    }
}

fn render_node(
    node: &Block,
    doc: &Document,
    ctx: &Context,
    slot: Option<&SlotCtx<'_>>,
    sink: &Sender<UiEvent>,
    parent: &Obj<'_>,
) {
    match node.kind.text.as_str() {
        "column" | "row" => {
            let obj = parent.new_container();
            obj.set_size(Size::Content, Size::Content);
            if node.kind.text == "row" {
                obj.set_flex_row();
            } else {
                obj.set_flex_column();
            }
            apply_props(node, ctx, sink, &obj);
            // Schema 8.2: `justify` on the main axis, `align` across.
            let justify = find_simple(node, "justify").and_then(FlexAlign::parse);
            let align = find_simple(node, "align").and_then(FlexAlign::parse);
            if justify.is_some() || align.is_some() {
                obj.set_flex_align(
                    justify.unwrap_or(FlexAlign::Start),
                    align.unwrap_or(FlexAlign::Start),
                );
            }
            render_item_list(&node.items, doc, ctx, slot, sink, &obj);
        }
        "stack" => {
            let obj = parent.new_container();
            obj.set_size(Size::Content, Size::Content);
            apply_props(node, ctx, sink, &obj);
            render_item_list(&node.items, doc, ctx, slot, sink, &obj);
        }
        "grid" => {
            // A grid fills its parent's width (its columns are
            // fractions of it) and grows with its rows; `width` /
            // `height` props override.
            let obj = parent.new_container();
            obj.set_size(Size::Percent(100), Size::Content);
            apply_props(node, ctx, sink, &obj);
            render_item_list(&node.items, doc, ctx, slot, sink, &obj);
            // Lay out after rendering so `repeat` output counts.
            let cols = resolve_num(node, "columns", ctx).unwrap_or(2.0) as i32;
            let rows = resolve_num(node, "rows", ctx).map(|r| r as i32);
            obj.layout_grid(cols, rows);
        }
        "text" => {
            let text = resolve_text(node, "value", ctx).unwrap_or_default();
            let obj = parent.new_label(&text);
            apply_props(node, ctx, sink, &obj);
        }
        "button" => {
            let obj = parent.new_button();
            apply_props(node, ctx, sink, &obj);
            // Shortcut: `button { label = "Save" }` renders a label
            // child so the simplest button only takes one line. If
            // the button also has child nodes, those render after
            // the shortcut label.
            if let Some(label) = resolve_assign_value(node, "label", ctx) {
                obj.new_label(&label.to_display_string());
            }
            render_item_list(&node.items, doc, ctx, slot, sink, &obj);
        }
        "rect" => {
            let obj = parent.new_container();
            obj.set_size(Size::Content, Size::Content);
            apply_props(node, ctx, sink, &obj);
        }
        "bar" => {
            let obj = parent.new_bar();
            let min = resolve_num(node, "min", ctx).unwrap_or(0.0) as i32;
            let max = resolve_num(node, "max", ctx).unwrap_or(100.0) as i32;
            obj.set_bar_range(min, max);
            if let Some(v) = resolve_num(node, "value", ctx) {
                obj.set_bar_value(v as i32);
            }
            apply_props(node, ctx, sink, &obj);
        }
        "arc" => {
            let obj = parent.new_arc();
            let min = resolve_num(node, "min", ctx).unwrap_or(0.0) as i32;
            let max = resolve_num(node, "max", ctx).unwrap_or(100.0) as i32;
            obj.set_arc_range(min, max);
            if let Some(v) = resolve_num(node, "value", ctx) {
                obj.set_arc_value(v as i32);
            }
            apply_props(node, ctx, sink, &obj);
        }
        "spinner" => {
            let obj = parent.new_spinner();
            apply_props(node, ctx, sink, &obj);
        }
        "qr" => {
            let size = find_simple(node, "size")
                .and_then(|v| v.trim().parse::<i32>().ok())
                .unwrap_or(128);
            let obj = parent.new_qrcode(size);
            if let Some(data) = resolve_assign_value(node, "data", ctx) {
                obj.set_qrcode_data(&data.to_display_string());
            }
            apply_props(node, ctx, sink, &obj);
        }
        "toggle" => {
            let obj = parent.new_toggle();
            let value_on = resolve_assign_value(node, "value", ctx)
                .map(|v| value_truthy(&v))
                .unwrap_or(false);
            obj.set_toggle_state(value_on);
            apply_props(node, ctx, sink, &obj);
            if let Some(assigns) = inline_on_change_set(node) {
                obj.on_change_toggle_inline_set(sink.clone(), assigns);
            }
        }
        "slider" => {
            let obj = parent.new_slider();
            let min = resolve_num(node, "min", ctx).unwrap_or(0.0) as i32;
            let max = resolve_num(node, "max", ctx).unwrap_or(100.0) as i32;
            obj.set_slider_range(min, max);
            if let Some(v) = resolve_num(node, "value", ctx) {
                obj.set_slider_value(v as i32);
            }
            apply_props(node, ctx, sink, &obj);
            if let Some(assigns) = inline_on_change_set(node) {
                obj.on_change_slider_inline_set(sink.clone(), assigns);
            }
        }
        "keypad" => {
            // Schema v1: `length` (max digits, default 0 = any),
            // `masked` (bool, default false), `on_submit`. Rendered
            // as an LVGL textarea stacked above a numeric keyboard;
            // the keyboard's OK fires `on_submit` with
            // `event.value` = the typed string.
            let masked = resolve_assign_value(node, "masked", ctx)
                .map(|v| value_truthy(&v))
                .unwrap_or(false);
            let length = resolve_num(node, "length", ctx).unwrap_or(0.0) as u32;
            let ta = parent.new_textarea(masked, length);
            ta.register_input();
            let kb = parent.new_number_keyboard();
            kb.attach_keyboard_to(&ta);
            apply_props(node, ctx, sink, &ta);
            // Two forms: `on_submit = <action>` (the digits arrive as
            // `value`) or `on_submit { set { ... } }` (`event.value`).
            match find_simple(node, "on_submit").map(str::trim) {
                Some(action) if is_bare_ident(action) => {
                    kb.on_submit_action(sink.clone(), action.to_string());
                }
                _ => {
                    if let Some(assigns) = inline_on_submit_set(node) {
                        kb.on_submit_keyboard_inline_set(sink.clone(), assigns);
                    }
                }
            }
        }
        "select" => {
            // Schema v1: `options` is a list expression, `value`
            // picks the initial index (or the matching option when
            // it's a value from the same list). Only the default
            // `style = dropdown` ships today; `roller` and `buttons`
            // are deferred.
            let obj = parent.new_dropdown();
            let options = resolve_assign_value(node, "options", ctx)
                .map(dropdown_options)
                .unwrap_or_default();
            if !options.is_empty() {
                obj.set_dropdown_options(&options.join("\n"));
            }
            if let Some(v) = resolve_assign_value(node, "value", ctx) {
                let idx = match v {
                    Value::Int(n) if n >= 0 => n as u16,
                    Value::Double(d) if d >= 0.0 => d as u16,
                    other => {
                        let s = other.to_display_string();
                        options
                            .iter()
                            .position(|o| o == &s)
                            .map(|p| p as u16)
                            .unwrap_or(0)
                    }
                };
                obj.set_dropdown_selected(idx);
            }
            apply_props(node, ctx, sink, &obj);
            if let Some(assigns) = inline_on_change_set(node) {
                obj.on_change_dropdown_inline_set(sink.clone(), assigns);
            }
        }
        "slot" => {
            // Expand the caller's child-items here, resolving them
            // against the OUTER context (not the component's inner
            // scope). If there's no enclosing component call, the
            // slot renders nothing.
            if let Some(s) = slot {
                render_item_list(s.children, doc, s.outer_ctx, None, sink, parent);
            }
        }
        other => {
            // Try to resolve as a user-defined component. If found,
            // expand its body in place with each arg bound in a
            // fresh scope.
            if let Some(comp) = find_component(doc, other) {
                expand_component(node, comp, doc, ctx, sink, parent);
                return;
            }
            // Not a built-in widget and not a known component.
            parent.new_label(&format!("[{other}: not implemented yet]"));
        }
    }
}

/// Find a top-level `component NAME { ... }` block by name.
fn find_component<'a>(doc: &'a Document, name: &str) -> Option<&'a Block> {
    for item in &doc.items {
        if let Item::Block(b) = item {
            if b.kind.text == "component"
                && b.name.as_ref().is_some_and(|n| n.text == name)
            {
                return Some(b);
            }
        }
    }
    None
}

/// Expand a user-component call: evaluate each `arg = value` from
/// the call site against the outer context, fill in defaults for
/// omitted args, bind them as top-level identifiers in a fresh
/// Context, and render the component's single UI root into
/// `parent`.
///
/// Scope today: no `slot`, no `repeat`. Nested calls to other user
/// components work (they re-enter this expander with their own
/// scope).
fn expand_component(
    call: &Block,
    comp: &Block,
    doc: &Document,
    outer_ctx: &Context,
    sink: &Sender<UiEvent>,
    parent: &Obj<'_>,
) {
    let mut inner_ctx = outer_ctx.clone();
    for item in &comp.items {
        let Item::Param(p) = item else { continue };
        let bound = if let Some((raw, span)) = find_raw(call, &p.name.text) {
            resolve_expr_value(raw, span, outer_ctx).unwrap_or(Value::Null)
        } else if let Some(default) = &p.default {
            resolve_expr_value(&default.text, default.span, outer_ctx).unwrap_or(Value::Null)
        } else {
            Value::Null
        };
        inner_ctx.insert(p.name.text.clone(), typed_param(&p.ty.text, bound));
    }
    // Children of the call that aren't args become the slot's
    // content. The schema check already enforced "exactly one slot"
    // and "no children unless a slot exists".
    // Handler blocks (`on_hold vm_stop { ... }`) belong to the call,
    // not to the slot.
    let child_items: Vec<Item> = call
        .items
        .iter()
        .filter(|it| match it {
            Item::Block(b) => !is_event_handler_block(&b.kind.text),
            Item::Repeat(_) => true,
            _ => false,
        })
        .cloned()
        .collect();
    let slot = SlotCtx { children: &child_items, outer_ctx };
    let before = parent.child_count();
    render_item_list(&comp.items, doc, &inner_ctx, Some(&slot), sink, parent);
    // The call's own `on_tap` / `on_hold` act on the component's root
    // object, with args evaluated in the caller's scope (`item`, ...).
    if let Some(root) = parent.child(before) {
        bind_handlers(call, outer_ctx, sink, &root);
    }
}

/// Give a component arg its declared type. A literal arg (`warn = 70`)
/// or default (`= 1e9`) arrives as text; without this `value > warn`
/// compares a number with a string and the expression yields nothing.
/// A value that does not convert is passed through unchanged.
pub(crate) fn typed_param(ty: &str, v: Value) -> Value {
    use paneld_config::value::{parse_bool, parse_int, parse_number};
    let Value::String(s) = &v else {
        return match (ty, v) {
            ("number", Value::Int(n)) => Value::Double(n as f64),
            (_, v) => v,
        };
    };
    let t = s.trim();
    match ty {
        "number" => parse_number(t).map(Value::Double).unwrap_or(v),
        "int" => parse_int(t).map(Value::Int).unwrap_or(v),
        "bool" => parse_bool(t).map(Value::Bool).unwrap_or(v),
        _ => v,
    }
}

/// Interpolate every `{{ ... }}` fragment in `raw` against `ctx`,
/// returning the rendered string. Literal text is kept verbatim.
/// Used by sources / actions to splice secrets, sources and theme
/// tokens into their shell commands before `runner::exec`.
///
/// `file` and `base` identify where `raw` lives in the enclosing
/// source so CEL parse errors can point back at it; a caller
/// without good coordinates can pass a dummy FileId and 0 base.
pub fn interpolate(
    raw: &str,
    file: paneld_config::source::FileId,
    base: u32,
    ctx: &Context,
) -> String {
    let span = paneld_config::source::Span { file, start: base, end: base };
    match resolve_expr_value(raw, span, ctx) {
        Some(Value::String(s)) => s,
        Some(v) => v.to_display_string(),
        None => raw.to_string(),
    }
}

/// Public re-export of [`resolve_expr_value`] for callers that have a
/// raw value text and need its evaluated [`Value`] against `ctx`.
/// Used by the action worker to resolve each `set { k = <raw> }` RHS
/// through the same path as a UI node's `value =`.
pub fn resolve_value(
    raw: &str,
    span: paneld_config::source::Span,
    ctx: &Context,
) -> Option<Value> {
    resolve_expr_value(raw, span, ctx)
}

/// Evaluate a `key = <raw>` as either a literal string (for a
/// non-expression value) or a CEL expression resolved against `ctx`,
/// returning a [`Value`]. Interpolated-string values become a
/// [`Value::String`].
fn resolve_expr_value(
    raw: &str,
    span: paneld_config::source::Span,
    ctx: &Context,
) -> Option<Value> {
    match classify(raw) {
        ValueShape::Literal(text) => Some(Value::String(text.to_string())),
        ValueShape::Expression(body) => {
            let (expr, diags) = cel::parse_fragment(body, span.file, span.start + 2);
            if !diags.is_empty() {
                return None;
            }
            expr.and_then(|e| eval(&e, ctx).ok())
        }
        ValueShape::Interp(_) => {
            let (fragments, diags) = cel::parse_interp(raw, span.file, span.start);
            if !diags.is_empty() {
                return None;
            }
            let mut out = String::new();
            let bytes = raw.as_bytes();
            let mut i = 0;
            let mut frag_idx = 0;
            while i < bytes.len() {
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
                    if let Some(frag) = fragments.get(frag_idx) {
                        if let Ok(v) = eval(frag, ctx) {
                            out.push_str(&v.to_display_string());
                        }
                    }
                    frag_idx += 1;
                    i = j;
                    continue;
                }
                out.push(bytes[i] as char);
                i += 1;
            }
            Some(Value::String(out))
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

fn apply_props(node: &Block, ctx: &Context, sink: &Sender<UiEvent>, obj: &Obj<'_>) {
    // Inline-action block form, with or without a name:
    //   `on_tap { goto = settings }`
    //   `on_tap { set { k = v; ... } }`
    //   `on_tap vm_stop { vm = {{ item.id }} }`
    // Only one gets wired per tap: the first form found wins,
    // matching schema semantics (one action per handler).
    bind_handlers(node, ctx, sink, obj);
    apply_plain_props(node, ctx, obj);
}

/// Wire `node`'s `on_tap` / `on_hold` handlers (block and `= name`
/// forms) to `obj`. Action-call args are evaluated against `ctx` now.
fn bind_handlers(node: &Block, ctx: &Context, sink: &Sender<UiEvent>, obj: &Obj<'_>) {
    // A node with an `on_hold` gets short taps only for `on_tap`, so a
    // long press does not fire both.
    let has_hold = node.items.iter().any(|it| match it {
        Item::Block(b) => b.kind.text == "on_hold",
        Item::Assign(a) => {
            matches!(&a.target, AssignTarget::Simple { key } if key.text == "on_hold")
        }
        _ => false,
    });
    let tap = if has_hold { Trigger::ShortTap } else { Trigger::Tap };
    for (handler, trigger) in [("on_tap", tap), ("on_hold", Trigger::Hold)] {
        for item in &node.items {
            let Item::Block(child) = item else { continue };
            if child.kind.text != handler {
                continue;
            }
            // Named action-call shortcut: eval args NOW against the
            // current render ctx so the stored binding captures the
            // row (`item`, `vars.*`, ...) the user is touching.
            if let Some(name) = &child.name {
                let args = collect_action_args(child, ctx);
                obj.set_clickable();
                obj.bind_action(trigger, sink.clone(), &name.text, args);
                break;
            }
            // An inline handler with action keys (`run`, `pin`,
            // `confirm`, ...) is an anonymous action: call it like a
            // named one so its gates apply. The local bindings
            // (`item`, component params) go along as args.
            if child.is_inline_action() {
                obj.set_clickable();
                obj.bind_action(trigger, sink.clone(), &child.inline_action_name(), local_args(ctx));
                break;
            }
            // `goto = page` or `goto = {{ expr }}` (e.g. a nav entry
            // in a `repeat`), resolved now against the render ctx.
            if let Some((raw, span)) = find_raw(child, "goto") {
                let page = resolve_scalar(raw, span, ctx);
                let page = page.trim();
                if is_bare_ident(page) {
                    obj.set_clickable();
                    obj.bind_goto(trigger, sink.clone(), page);
                    break;
                }
            }
            if let Some(set_block) = find_block(child, "set") {
                let assigns = collect_inline_set(set_block);
                if !assigns.is_empty() {
                    obj.set_clickable();
                    obj.bind_inline_set(trigger, sink.clone(), assigns);
                    break;
                }
            }
        }
    }
    // `on_tap = <action>` / `on_hold = <action>`.
    for item in &node.items {
        let Item::Assign(a) = item else { continue };
        let AssignTarget::Simple { key } = &a.target else {
            continue;
        };
        let trigger = match key.text.as_str() {
            "on_tap" => tap,
            "on_hold" => Trigger::Hold,
            _ => continue,
        };
        let name = a.value.text.trim();
        if name != "none" && is_bare_ident(name) {
            obj.set_clickable();
            obj.bind_action(trigger, sink.clone(), name, Vec::new());
        }
    }
}

/// Apply `node`'s plain properties (size, spacing, colors, font,
/// visibility, ...) to `obj`.
fn apply_plain_props(node: &Block, ctx: &Context, obj: &Obj<'_>) {
    for item in &node.items {
        let Item::Assign(a) = item else { continue };
        let AssignTarget::Simple { key } = &a.target else {
            continue;
        };
        let value = a.value.text.trim();
        match key.text.as_str() {
            "width" => {
                if let Some(s) = parse_size(&resolve_scalar(value, a.value.span, ctx)) {
                    obj.set_width(s);
                }
            }
            "height" => {
                if let Some(s) = parse_size(&resolve_scalar(value, a.value.span, ctx)) {
                    obj.set_height(s);
                }
            }
            "padding" => {
                if let Ok(px) = resolve_scalar(value, a.value.span, ctx).trim().parse::<i32>() {
                    obj.set_padding(px);
                }
            }
            "gap" => {
                if let Ok(px) = resolve_scalar(value, a.value.span, ctx).trim().parse::<i32>() {
                    obj.set_gap(px);
                }
            }
            "background" => {
                if let Some(rgb) = resolve_color(value, a.value.span, ctx) {
                    obj.set_background_hex(rgb);
                }
            }
            "color" => {
                if let Some(rgb) = resolve_color(value, a.value.span, ctx) {
                    obj.set_text_color_hex(rgb);
                }
            }
            "font" => {
                // Accept `<family> <size> [bold]` and pick the
                // compiled Montserrat at that size, ignoring the
                // family token for now (TinyTTF-loaded fonts come
                // in a later phase). Falls back silently if the
                // size isn't compiled in.
                let resolved = resolve_expr_value(value, a.value.span, ctx)
                    .map(|v| v.to_display_string())
                    .unwrap_or_else(|| value.to_string());
                if let Some(size) = extract_font_size(&resolved) {
                    obj.set_font_size(size);
                }
            }
            // On `row` / `column` (and `scroll`) `align` places the
            // children; it is handled where those are built.
            "align" if !matches!(node.kind.text.as_str(), "row" | "column" | "scroll") => {
                match value {
                    "left" => obj.set_text_align_left(),
                    "center" => obj.set_text_align_center(),
                    "right" => obj.set_text_align_right(),
                    _ => {}
                }
            }
            "radius" => {
                if let Some(Size::Px(px)) = parse_size(&resolve_scalar(value, a.value.span, ctx)) {
                    obj.set_radius(px);
                }
            }
            "border" => {
                // `<width> [<color>]`, e.g. `border = 1 #cccccc`, `border = 0`.
                let text = resolve_scalar(value, a.value.span, ctx);
                let mut parts = text.split_whitespace();
                if let Some(Ok(w)) = parts.next().map(|w| w.trim_end_matches("px").parse::<i32>()) {
                    obj.set_border(w, parts.next().and_then(parse_hex_color));
                }
            }
            "opacity" => {
                if let Ok(o) = resolve_scalar(value, a.value.span, ctx).trim().parse::<f64>() {
                    obj.set_opacity(o);
                }
            }
            "grow" => {
                if let Ok(g) = resolve_scalar(value, a.value.span, ctx).trim().parse::<f64>() {
                    obj.set_flex_grow(g.clamp(0.0, 255.0) as u8);
                }
            }
            "visible" => {
                let flag = resolve_expr_value(value, a.value.span, ctx)
                    .map(|v| value_truthy(&v))
                    .unwrap_or(true);
                obj.set_hidden(!flag);
            }
            "enabled" => {
                let flag = resolve_expr_value(value, a.value.span, ctx)
                    .map(|v| value_truthy(&v))
                    .unwrap_or(true);
                obj.set_enabled(flag);
            }
            _ => {
                // Handlers are bound by `bind_handlers`; other keys
                // are legal schema-wise but not wired yet.
            }
        }
    }
}

/// Resolve a `key = ...` on `node` as an f64. Handles literal
/// numbers and `{{ expr }}` evaluating to Int/Double. Returns None
/// if the key is absent or doesn't produce a number.
fn resolve_num(node: &Block, key: &str, ctx: &Context) -> Option<f64> {
    let (raw, span) = find_raw(node, key)?;
    let v = resolve_expr_value(raw, span, ctx)?;
    match v {
        Value::Int(n) => Some(n as f64),
        Value::Double(d) => Some(d),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

/// Pull the size integer out of a `font` value such as
/// `roboto 20`, `montserrat 16 bold`, `fonts/Inter.ttf 24`.
fn extract_font_size(s: &str) -> Option<u16> {
    for tok in s.split_whitespace() {
        if let Ok(n) = tok.parse::<u16>() {
            return Some(n);
        }
    }
    None
}

fn find_simple<'a>(node: &'a Block, key: &str) -> Option<&'a str> {
    for item in &node.items {
        let Item::Assign(a) = item else { continue };
        let AssignTarget::Simple { key: k } = &a.target else {
            continue;
        };
        if k.text == key {
            return Some(a.value.text.as_str());
        }
    }
    None
}

/// Resolve `key = <raw>` in `node` through [`resolve_expr_value`],
/// using the real value span so a CEL error points at the right place.
fn resolve_assign_value(node: &Block, key: &str, ctx: &Context) -> Option<Value> {
    for item in &node.items {
        let Item::Assign(a) = item else { continue };
        let AssignTarget::Simple { key: k } = &a.target else {
            continue;
        };
        if k.text == key {
            return resolve_expr_value(&a.value.text, a.value.span, ctx);
        }
    }
    None
}

fn find_block<'a>(node: &'a Block, kind: &str) -> Option<&'a Block> {
    for item in &node.items {
        if let Item::Block(b) = item {
            if b.kind.text == kind {
                return Some(b);
            }
        }
    }
    None
}

/// Scan a widget node for `on_change { set { ... } }` and return the
/// inline set payload. Returns `None` if no such handler is present.
fn inline_on_change_set(node: &Block) -> Option<Vec<InlineSetItem>> {
    inline_set_for_handler(node, "on_change")
}

/// Same shape, for a `keypad`'s `on_submit { set { ... } }`.
fn inline_on_submit_set(node: &Block) -> Option<Vec<InlineSetItem>> {
    inline_set_for_handler(node, "on_submit")
}

fn inline_set_for_handler(node: &Block, handler: &str) -> Option<Vec<InlineSetItem>> {
    for item in &node.items {
        let Item::Block(child) = item else { continue };
        if child.kind.text != handler || child.name.is_some() {
            continue;
        }
        if let Some(set_block) = find_block(child, "set") {
            let assigns = collect_inline_set(set_block);
            if !assigns.is_empty() {
                return Some(assigns);
            }
        }
    }
    None
}

/// The bindings in `ctx` that are local to where a node is rendered
/// (`repeat` items, component params), as action args.
fn local_args(ctx: &Context) -> Vec<ActionArg> {
    const GLOBAL: &[&str] = &["sources", "vars", "alerts", "theme", "now", "page", "nav"];
    ctx.iter()
        .filter(|(k, _)| !GLOBAL.contains(&k.as_str()))
        .map(|(k, v)| ActionArg { name: k.clone(), value: v.clone() })
        .collect()
}

/// Evaluate every `arg = <raw>` in an action-call shortcut body
/// against `ctx`, returning the already-resolved [`ActionArg`]s. An
/// arg whose expression fails to evaluate is dropped so a bad arg
/// does not make the whole tap no-op.
fn collect_action_args(call: &Block, ctx: &Context) -> Vec<ActionArg> {
    let mut out = Vec::new();
    for item in &call.items {
        let Item::Assign(a) = item else { continue };
        let AssignTarget::Simple { key } = &a.target else {
            continue;
        };
        if let Some(v) = resolve_expr_value(&a.value.text, a.value.span, ctx) {
            out.push(ActionArg { name: key.text.clone(), value: v });
        }
    }
    out
}

fn collect_inline_set(set_block: &Block) -> Vec<InlineSetItem> {
    let mut out = Vec::new();
    for item in &set_block.items {
        let Item::Assign(a) = item else { continue };
        let AssignTarget::Simple { key } = &a.target else {
            continue;
        };
        out.push(InlineSetItem {
            key: key.text.clone(),
            value_raw: a.value.text.clone(),
        });
    }
    out
}

/// Flatten a `select { options = ... }` value into the per-item
/// display strings LVGL expects, joined later with newlines. A
/// scalar is wrapped so `options = "only"` produces a one-item
/// dropdown; a list spreads out item by item. Any `\n` inside an
/// option would collide with LVGL's own separator, so each entry's
/// newlines are stripped.
fn dropdown_options(v: Value) -> Vec<String> {
    let items: Vec<Value> = match v {
        Value::List(xs) => xs,
        other => vec![other],
    };
    items
        .into_iter()
        .map(|item| item.to_display_string().replace('\n', " "))
        .collect()
}

fn value_truthy(v: &Value) -> bool {
    match v {
        Value::Bool(b) => *b,
        Value::Null => false,
        Value::Int(n) => *n != 0,
        Value::Double(d) => *d != 0.0,
        Value::String(s) => !s.is_empty(),
        Value::List(items) => !items.is_empty(),
        Value::Map(m) => !m.is_empty(),
    }
}

fn is_bare_ident(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && (b[0].is_ascii_alphabetic() || b[0] == b'_')
        && b.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'_')
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

/// A property value as text: a literal as written, or the result of a
/// `{{ ... }}` expression (so `padding = {{ theme.padding }}` works).
fn resolve_scalar(value: &str, span: paneld_config::source::Span, ctx: &Context) -> String {
    match classify(value) {
        ValueShape::Literal(t) => t.to_string(),
        _ => resolve_expr_value(value, span, ctx)
            .map(|v| v.to_display_string())
            .unwrap_or_default(),
    }
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

/// Resolve a color value's raw text. Accepts a literal hex (`#rgb`,
/// `#rrggbb`, `#rrggbbaa`), a `{{ expr }}` whose evaluated string
/// is a hex, or an interpolated value whose rendered string is a
/// hex. Returns the 24-bit RGB (alpha dropped for now).
fn resolve_color(
    value: &str,
    span: paneld_config::source::Span,
    ctx: &Context,
) -> Option<u32> {
    match classify(value) {
        ValueShape::Literal(text) => parse_hex_color(text),
        ValueShape::Expression(body) => {
            let (expr, diags) = cel::parse_fragment(body, span.file, span.start + 2);
            if !diags.is_empty() {
                return None;
            }
            let v = expr.and_then(|e| eval(&e, ctx).ok())?;
            parse_hex_color(&v.to_display_string())
        }
        ValueShape::Interp(_) => {
            // Rarely useful for colors; evaluate the whole thing
            // and hope the result is a hex.
            resolve_expr_value(value, span, ctx)
                .and_then(|v| parse_hex_color(&v.to_display_string()))
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn component_args_take_their_declared_type() {
        let s = |t: &str| Value::String(t.into());
        assert_eq!(typed_param("number", s("70")), Value::Double(70.0));
        assert_eq!(typed_param("number", s("1e9")), Value::Double(1e9));
        assert_eq!(typed_param("number", Value::Int(3)), Value::Double(3.0));
        assert_eq!(typed_param("int", s(" 4 ")), Value::Int(4));
        assert_eq!(typed_param("bool", s("true")), Value::Bool(true));
        assert_eq!(typed_param("string", s("70")), s("70"));
        // Not convertible: passed through unchanged.
        assert_eq!(typed_param("number", s("abc")), s("abc"));
        assert_eq!(typed_param("number", Value::Null), Value::Null);
    }
}
