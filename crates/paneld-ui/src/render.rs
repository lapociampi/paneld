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
use crate::ui::{
    ActionArg, Anchor, FlexAlign, ImageFit, InlineSetItem, Obj, ScrollSnap, SelfAlign, Size, TextOverflow, Track, Tracks,
    Trigger, UiEvent, ValueWidget,
};

/// Children of a component call plus the outer context they should
/// resolve against. Threaded through [`render_node`] so a `slot { }`
/// inside the component's body expands to the caller's children.
struct SlotCtx<'a> {
    children: &'a [Item],
    outer_ctx: &'a Context,
}

/// Show `text` as a toast on `layer` (the top layer), replacing any
/// earlier one: a pill near the bottom in `background` (a color: a hex
/// or a token of `theme`, e.g. `ok`) with white text. The caller
/// removes it with `layer.clear_children()` when it expires.
pub fn render_toast(layer: &Obj<'_>, text: &str, background: &str, theme: &std::collections::BTreeMap<String, Value>) {
    layer.clear_children();
    let pill = layer.new_container();
    // Taps go through to the page (or a modal) under it.
    pill.set_enabled(false);
    pill.set_size(Size::Content, Size::Content);
    pill.set_padding(12);
    pill.set_radius(8);
    pill.set_border(0, None);
    if let Some((rgb, opa)) = theme_color(background, Some(theme)) {
        pill.set_background_hex(rgb, opa);
    }
    let label = pill.new_label(text);
    label.set_text_color_hex(0xffffff, 255);
    // Above the shell's nav bar.
    pill.align_bottom_mid(-72);
}

/// Render the page named `page_name` from `doc` into `screen`. If no
/// such page exists, draws a plain "page not found" label as a
/// diagnostic.
pub fn render_page(
    doc: &Document,
    page_name: &str,
    ctx: &Context,
    sink: &Sender<UiEvent>,
    screen: &Obj<'_>,
) {
    let critical = color_of("critical", ctx);
    FAILED_BORDER.with(|c| c.set(critical.unwrap_or((0xf85149, 255))));
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
            let obj = parent.new_layout();
            obj.set_size(Size::Content, Size::Content);
            if node.kind.text == "row" {
                obj.set_flex_row();
            } else {
                obj.set_flex_column();
            }
            // Schema 8.2: `wrap` onto more lines (needs a fixed size
            // along the flow).
            if resolve_assign_value(node, "wrap", ctx).is_some_and(|v| value_truthy(&v)) {
                obj.set_flex_wrap(node.kind.text == "column");
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
            if find_simple(node, "align").map(str::trim) == Some("stretch") {
                obj.stretch_children(node.kind.text == "column");
            }
        }
        "stack" => {
            let obj = parent.new_layout();
            obj.set_size(Size::Content, Size::Content);
            // Children are placed by `x` / `y` from its corner.
            obj.set_padding(0);
            apply_props(node, ctx, sink, &obj);
            render_item_list(&node.items, doc, ctx, slot, sink, &obj);
        }
        "grid" => {
            // A grid fills its parent's width (its columns are
            // fractions of it) and grows with its rows; `width` /
            // `height` props override.
            let obj = parent.new_layout();
            obj.set_size(Size::Percent(100), Size::Content);
            apply_props(node, ctx, sink, &obj);
            render_item_list(&node.items, doc, ctx, slot, sink, &obj);
            // Lay out after rendering so `repeat` output counts.
            let cols = resolve_assign_value(node, "columns", ctx).and_then(|v| parse_tracks(&v)).unwrap_or(Tracks::Count(2));
            let rows = resolve_assign_value(node, "rows", ctx).and_then(|v| parse_tracks(&v));
            obj.layout_grid(&cols, rows.as_ref());
        }
        "scroll" => {
            // Schema 8.2: children as a column (or a row with
            // `direction = horizontal`) that scrolls along that axis.
            // By default it fills the parent's width and the space
            // left in a column; `width` / `height` / `grow` override.
            let horizontal = find_simple(node, "direction").map(str::trim) == Some("horizontal");
            let snap = match find_simple(node, "snap").map(str::trim) {
                Some("start") => ScrollSnap::Start,
                Some("center") => ScrollSnap::Center,
                _ => ScrollSnap::None,
            };
            let obj = parent.new_layout();
            obj.set_size(Size::Percent(100), Size::Content);
            // `grow` would override a `height` along a column.
            if find_simple(node, "height").is_none() && find_simple(node, "grow").is_none() {
                obj.set_flex_grow(1);
            }
            if horizontal {
                obj.set_flex_row();
            } else {
                obj.set_flex_column();
            }
            obj.set_scroll(horizontal, snap);
            apply_props(node, ctx, sink, &obj);
            if let Some(align) = find_simple(node, "align").and_then(FlexAlign::parse) {
                obj.set_flex_align(FlexAlign::Start, align);
            }
            render_item_list(&node.items, doc, ctx, slot, sink, &obj);
            if find_simple(node, "align").map(str::trim) == Some("stretch") {
                obj.stretch_children(!horizontal);
            }
        }
        "text" => {
            let text = resolve_text(node, "value", ctx).unwrap_or_default();
            let obj = parent.new_label(&text);
            apply_props(node, ctx, sink, &obj);
            obj.fit_dots();
        }
        "button" => {
            let obj = parent.new_button();
            // A failed call's default look, under the theme's
            // `button:failed`: a border in the theme's critical color.
            let critical = FAILED_BORDER.with(|c| c.get());
            crate::ui::with_state("failed", || obj.set_border(3, Some(critical)));
            let tap_call = apply_props(node, ctx, sink, &obj);
            if !show_call_state(&obj, tap_call.as_ref()) {
                // Shortcut: `button { label = "Save" }` renders a label
                // child so the simplest button only takes one line. If
                // the button also has child nodes, those render after
                // the shortcut label.
                if let Some(label) = resolve_assign_value(node, "label", ctx) {
                    obj.new_label(&label.to_display_string());
                }
                render_item_list(&node.items, doc, ctx, slot, sink, &obj);
            }
        }
        "rect" => {
            let obj = parent.new_container();
            obj.set_size(Size::Content, Size::Content);
            // A shape: no inner padding, which on a small one would
            // leave no room and show scrollbars.
            obj.set_padding(0);
            apply_props(node, ctx, sink, &obj);
        }
        "bar" => {
            let obj = parent.new_bar();
            // Schema 8.3: in the theme's `accent` on a faint `muted`
            // track; `color` / `track` (applied below) override.
            let (indicator, track) = gauge_colors(ctx);
            obj.set_bar_indicator(indicator.0, indicator.1);
            obj.set_bar_track(track.0, track.1);
            if find_simple(node, "direction").map(str::trim) == Some("vertical") {
                obj.set_bar_vertical();
            }
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
            let (indicator, track) = gauge_colors(ctx);
            obj.set_arc_indicator(indicator.0, indicator.1);
            obj.set_arc_track(track.0, track.1);
            obj.set_arc_rounded(false);
            // Degrees, 0 at 3 o'clock, clockwise: 135 to 45 leaves the
            // gap at the bottom.
            let start = resolve_num(node, "start", ctx).unwrap_or(135.0);
            let end = resolve_num(node, "end", ctx).unwrap_or(45.0);
            obj.set_arc_angles(start as i32, end as i32);
            let min = resolve_num(node, "min", ctx).unwrap_or(0.0) as i32;
            let max = resolve_num(node, "max", ctx).unwrap_or(100.0) as i32;
            obj.set_arc_range(min, max);
            if let Some(v) = resolve_num(node, "value", ctx) {
                obj.set_arc_value(v as i32);
            }
            apply_props(node, ctx, sink, &obj);
        }
        "line" => {
            // `points`: a list of `[x, y]` pairs, in px.
            let num = |v: &Value| match v {
                Value::Int(n) => Some(*n as i32),
                Value::Double(d) if d.is_finite() => Some(*d as i32),
                _ => None,
            };
            let points: Vec<(i32, i32)> = match resolve_assign_value(node, "points", ctx) {
                Some(Value::List(l)) => l
                    .iter()
                    .filter_map(|p| match p {
                        Value::List(xy) if xy.len() == 2 => Some((num(&xy[0])?, num(&xy[1])?)),
                        _ => None,
                    })
                    .take(1000)
                    .collect(),
                _ => Vec::new(),
            };
            let obj = parent.new_line(&points);
            let width = resolve_num(node, "thickness", ctx).unwrap_or(2.0) as i32;
            let rounded = resolve_assign_value(node, "rounded", ctx).is_some_and(|v| value_truthy(&v));
            let color = resolve_text(node, "color", ctx)
                .and_then(|c| color_of(&c, ctx))
                .or_else(|| color_of("text", ctx));
            obj.set_line_style(width.clamp(1, 64), rounded, color);
            apply_props(node, ctx, sink, &obj);
        }
        "image" => {
            let fit = match find_simple(node, "fit").map(str::trim) {
                Some("cover") => ImageFit::Cover,
                Some("fill") => ImageFit::Fill,
                Some("none") => ImageFit::None,
                _ => ImageFit::Contain,
            };
            let src = resolve_text(node, "src", ctx).unwrap_or_default();
            let src = src.trim();
            if src.is_empty() {
                return;
            }
            let Some(obj) = parent.new_image(src, fit) else { return };
            if let Some((rgb, opa)) = resolve_text(node, "recolor", ctx).and_then(|c| color_of(&c, ctx)) {
                obj.set_image_recolor(rgb, opa);
            }
            apply_props(node, ctx, sink, &obj);
        }
        "chart" => {
            let obj = parent.new_chart();
            // Schema 8.3: no white box; division lines in a faint
            // `muted`. `background` / `border` (applied below) win.
            let muted = color_of("muted", ctx).unwrap_or((0x757575, 255)).0;
            obj.set_chart_look((muted, FAINT));
            render_chart(node, ctx, &obj);
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
            if let Some(assigns) = bind_change_action(node, "on_change", ValueWidget::Toggle, ctx, sink, &obj) {
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
            if let Some(assigns) = bind_change_action(node, "on_change", ValueWidget::Slider, ctx, sink, &obj) {
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
            // The digits arrive as `value` / `event.value`.
            if let Some(assigns) = bind_change_action(node, "on_submit", ValueWidget::Keypad, ctx, sink, &kb) {
                kb.on_submit_keyboard_inline_set(sink.clone(), assigns);
            }
        }
        "select" => {
            // Schema 8.4: `options` (a list), `value` (the selected
            // option, or its position), `style` = `dropdown` (default),
            // `roller` or `buttons`; `on_change` gets the option.
            let options = resolve_assign_value(node, "options", ctx)
                .map(dropdown_options)
                .unwrap_or_default();
            // The option itself first (`value = 5` with options [1, 5]),
            // else a position.
            let selected = resolve_assign_value(node, "value", ctx).map(|v| {
                let text = v.to_display_string();
                options.iter().position(|o| *o == text).unwrap_or(match v {
                    Value::Int(n) if n >= 0 && (n as usize) < options.len() => n as usize,
                    Value::Double(d) if d >= 0.0 && (d as usize) < options.len() => d as usize,
                    _ => 0,
                })
            });
            let (obj, widget) = match find_simple(node, "style").map(str::trim) {
                Some("roller") => {
                    let obj = parent.new_roller(&options.join("\n"), 3);
                    if let Some(i) = selected {
                        obj.set_roller_selected(i as u32);
                    }
                    (obj, ValueWidget::Roller)
                }
                Some("buttons") => (parent.new_button_row(&options, selected), ValueWidget::Buttons),
                _ => {
                    let obj = parent.new_dropdown();
                    if !options.is_empty() {
                        obj.set_dropdown_options(&options.join("\n"));
                    }
                    if let Some(i) = selected {
                        obj.set_dropdown_selected(i as u16);
                    }
                    (obj, ValueWidget::Dropdown)
                }
            };
            apply_props(node, ctx, sink, &obj);
            if let Some(assigns) = bind_change_action(node, "on_change", widget, ctx, sink, &obj) {
                obj.on_change_select_inline_set(widget, sink.clone(), assigns);
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
    // The call's `on_tap` wins over its root button's `action`.
    ROOT_TAP.with(|t| t.set(has_handler(call, "on_tap")));
    render_item_list(&comp.items, doc, &inner_ctx, Some(&slot), sink, parent);
    ROOT_TAP.with(|t| t.set(false));
    // The call's own `on_tap` / `on_hold` act on the component's root
    // object, with args evaluated in the caller's scope (`item`, ...).
    // So do its common properties (schema 7), over the component's
    // own: everything on the call that is not one of its params.
    if let Some(root) = parent.child(before) {
        let tap_call = bind_handlers(call, outer_ctx, sink, &root);
        let root_kind = comp.items.iter().find_map(|it| match it {
            Item::Block(b) => Some(b.kind.text.as_str()),
            _ => None,
        });
        if root_kind == Some("button") {
            show_call_state(&root, tap_call.as_ref());
        }
        let params: Vec<&str> = comp
            .items
            .iter()
            .filter_map(|it| match it {
                Item::Param(p) => Some(p.name.text.as_str()),
                _ => None,
            })
            .collect();
        let props: Vec<Item> = call
            .items
            .iter()
            .filter(|it| match it {
                Item::Assign(a) => match &a.target {
                    AssignTarget::Simple { key } => !params.contains(&key.text.as_str()),
                    _ => false,
                },
                _ => false,
            })
            .cloned()
            .collect();
        let root_kind = comp.items.iter().find_map(|it| match it {
            Item::Block(b) => Some(b.kind.text.as_str()),
            _ => None,
        });
        let root_kind = root_kind.unwrap_or("");
        // The root already has its `*` and kind styles; the call adds
        // its classes only, so they do not undo the root's own props.
        apply_styles(root_kind, &props, false, outer_ctx, &root);
        apply_plain_props(root_kind, &props, outer_ctx, &root);
        if root_kind == "text" {
            root.fit_dots();
        }
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
        // `"..."` keeps spaces and `;` (config-format, "Lines"); the
        // quotes themselves are not part of the value.
        ValueShape::Literal(text) => Some(Value::String(paneld_config::value::parse_string(text))),
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
            Some(Value::String(splice(raw, &fragments, ctx, false)))
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
        ValueShape::Literal(text) => paneld_config::value::parse_string(text),
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
    splice(raw, fragments, ctx, true)
}

/// Put the values of `fragments` (parsed from `raw` by
/// [`cel::parse_interp`]) in place of their `{{ ... }}` in `raw`.
/// Mirrors the lexer's `\\` and `\{{` escapes. A failed fragment
/// shows as `[message]` with `show_errors`, else as nothing.
fn splice(raw: &str, fragments: &[cel::Expr], ctx: &Context, show_errors: bool) -> String {
    let mut out = String::with_capacity(raw.len());
    let bytes = raw.as_bytes();
    let (mut i, mut literal_from, mut frag_idx) = (0, 0, 0);
    while i < bytes.len() {
        let rest = &bytes[i..];
        if rest.starts_with(b"\\\\") {
            i += 2;
            continue;
        }
        let escaped_open = rest.starts_with(b"\\{{");
        let open = rest.starts_with(b"{{");
        if !escaped_open && !open {
            i += 1;
            continue;
        }
        // Text before here is literal; the special bytes are ASCII,
        // so the slice ends on a char boundary. `##` in it is `#`.
        out.push_str(&paneld_config::value::unescape_hash(&raw[literal_from..i]));
        if escaped_open {
            out.push_str("{{");
            i += 3;
        } else {
            let end = cel::expr_end(raw, i).unwrap_or(bytes.len());
            if let Some(expr) = fragments.get(frag_idx) {
                match eval(expr, ctx) {
                    Ok(v) => out.push_str(&v.to_display_string()),
                    Err(e) if show_errors => out.push_str(&format!("[{}]", e.message)),
                    Err(_) => {}
                }
            }
            frag_idx += 1;
            i = end;
        }
        literal_from = i;
    }
    out.push_str(&paneld_config::value::unescape_hash(&raw[literal_from..]));
    out
}

/// Returns the action call a tap on the node makes, if any.
fn apply_props(node: &Block, ctx: &Context, sink: &Sender<UiEvent>, obj: &Obj<'_>) -> Option<(String, Vec<ActionArg>)> {
    // Inline-action block form, with or without a name:
    //   `on_tap { goto = settings }`
    //   `on_tap { set { k = v; ... } }`
    //   `on_tap vm_stop { vm = {{ item.id }} }`
    // Only one gets wired per tap: the first form found wins,
    // matching schema semantics (one action per handler).
    let tap_call = bind_handlers(node, ctx, sink, obj);
    apply_styles(&node.kind.text, &node.items, true, ctx, obj);
    apply_plain_props(&node.kind.text, &node.items, ctx, obj);
    tap_call
}

thread_local! {
    /// The active theme's `style <selector> { }` bodies, by selector
    /// (see [`set_styles`]). Read on the UI thread while drawing.
    static STYLES: std::cell::RefCell<std::collections::BTreeMap<String, Vec<Item>>> =
        Default::default();
}

/// Where an action call stands, for the buttons that make it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunState {
    Running,
    /// Its last run failed (until the action runs again).
    Failed,
}

/// An action call the main loop has running, or whose last run
/// failed: what [`set_call_states`] takes.
#[derive(Clone, Debug)]
pub struct CallState {
    pub name: String,
    pub args: Vec<ActionArg>,
    pub state: RunState,
    /// The action's `busy` text for this call, evaluated with its args.
    pub busy: Option<String>,
}

thread_local! {
    /// The calls running or failed, as of the page being drawn.
    static CALL_STATES: std::cell::RefCell<Vec<CallState>> = Default::default();
    /// The action calls the buttons on screen make (see [`shows_call`]).
    static DRAWN_CALLS: std::cell::RefCell<Vec<(String, Vec<ActionArg>)>> = Default::default();
    /// A `:failed` button's default border color: the theme's
    /// `critical`, resolved once per page.
    static FAILED_BORDER: std::cell::Cell<(u32, u8)> = const { std::cell::Cell::new((0xf85149, 255)) };
    /// The component call being expanded has its own `on_tap`, which
    /// wins over its root button's `action`: taken by the root.
    static ROOT_TAP: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Install the action calls running or failed now, before drawing a
/// page: its buttons show them (`:running`, `:failed`).
pub fn set_call_states(states: Vec<CallState>) {
    CALL_STATES.with(|s| *s.borrow_mut() = states);
    DRAWN_CALLS.with(|d| d.borrow_mut().clear());
}

/// Whether a button on the page drawn last makes call `name(args)`,
/// so a change in that call's state needs a redraw.
pub fn shows_call(name: &str, args: &[ActionArg]) -> bool {
    DRAWN_CALLS.with(|d| d.borrow().iter().any(|(n, a)| n == name && covers(args, a)))
}

/// `call` carries every arg in `button` with the same value.
fn covers(call: &[ActionArg], button: &[ActionArg]) -> bool {
    button.iter().all(|a| call.iter().any(|b| b.name == a.name && same_value(&b.value, &a.value)))
}

/// Equal values, scalars compared as shown: a call's args were
/// converted to their param types (`1` in the config is `"1"` until
/// then). Maps and lists compare by their items.
fn same_value(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Map(x), Value::Map(y)) => x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| same_value(v, w))),
        (Value::List(x), Value::List(y)) => x.len() == y.len() && x.iter().zip(y).all(|(v, w)| same_value(v, w)),
        (Value::Map(_) | Value::List(_), _) | (_, Value::Map(_) | Value::List(_)) => false,
        _ => a.to_display_string() == b.to_display_string(),
    }
}

/// The state of a button's call `name(args)`: a known call of the
/// same action with every arg the button passes (a dialog may have
/// added more). Running wins over failed.
fn call_state(name: &str, args: &[ActionArg]) -> Option<CallState> {
    CALL_STATES.with(|s| {
        let s = s.borrow();
        let matching = s.iter().filter(|c| c.name == name && covers(&c.args, args));
        matching.min_by_key(|c| c.state != RunState::Running).cloned()
    })
}

/// Show on button `obj` where the call its tap makes stands: the
/// `:running` / `:failed` state, and while it runs the call's `busy`
/// text or a spinner in place of its content. Returns true when the
/// content was replaced.
fn show_call_state(obj: &Obj<'_>, tap_call: Option<&(String, Vec<ActionArg>)>) -> bool {
    let Some((name, args)) = tap_call else {
        return false;
    };
    DRAWN_CALLS.with(|d| d.borrow_mut().push((name.clone(), args.clone())));
    let call = call_state(name, args);
    let state = call.as_ref().map(|c| c.state);
    obj.set_run_state(state == Some(RunState::Running), state == Some(RunState::Failed));
    let Some(CallState { state: RunState::Running, busy, .. }) = call else {
        return false;
    };
    obj.clear_children();
    match busy.filter(|t| !t.trim().is_empty()) {
        Some(text) => {
            obj.new_label(&text);
        }
        None => {
            obj.new_busy_spinner();
        }
    }
    true
}

/// Install the active theme's styles: selector (`*`, `button`,
/// `.card`) to its `key = value` items, parents merged in already.
/// Applies from the next page drawn.
pub fn set_styles(styles: std::collections::BTreeMap<String, Vec<Item>>) {
    STYLES.with(|s| *s.borrow_mut() = styles);
}

/// The class names in `class = ...` (`card`, `card wide`, `[card,
/// wide]`).
fn class_names(items: &[Item]) -> Vec<String> {
    let Some(raw) = items.iter().find_map(|it| match it {
        Item::Assign(a) if matches!(&a.target, AssignTarget::Simple { key } if key.text == "class") => {
            Some(a.value.text.as_str())
        }
        _ => None,
    }) else {
        return Vec::new();
    };
    raw.trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|c| !c.is_empty())
        .map(str::to_string)
        .collect()
}

/// Theme styles for a node of `kind` with `items` (themes.md,
/// "Precedence": under what the node itself sets): `*`, then the
/// kind, then each `class` in order, later winning. State selectors
/// (`button:pressed`, `.card:checked`) follow, for that state.
/// Without `by_kind`, only the classes.
fn apply_styles(kind: &str, items: &[Item], by_kind: bool, ctx: &Context, obj: &Obj<'_>) {
    let classes = class_names(items);
    STYLES.with(|styles| {
        let styles = styles.borrow();
        if styles.is_empty() {
            return;
        }
        let selectors = ["*".to_string(), kind.to_string()]
            .into_iter()
            .filter(|_| by_kind)
            .chain(classes.iter().map(|c| format!(".{c}")));
        let selectors: Vec<String> = selectors.collect();
        for sel in &selectors {
            if let Some(body) = styles.get(sel) {
                // `*`'s `color` is the text color, not a gauge's.
                let kind = if sel == "*" && matches!(kind, "arc" | "bar") { "" } else { kind };
                apply_plain_props(kind, body, ctx, obj);
            }
        }
        // Then the states (`button:pressed`, `.card:checked`): the
        // same properties, for when the node is in that state.
        for sel in &selectors {
            for state in ["pressed", "checked", "disabled", "focused", "hover", "running", "failed"] {
                if let Some(body) = styles.get(&format!("{sel}:{state}")) {
                    crate::ui::with_state(state, || apply_plain_props(kind, body, ctx, obj));
                }
            }
        }
    });
}

/// `node` has a `handler` (`on_tap`, `on_hold`), block or `= name`.
fn has_handler(node: &Block, handler: &str) -> bool {
    node.items.iter().any(|it| match it {
        Item::Block(b) => b.kind.text == handler,
        Item::Assign(a) => matches!(&a.target, AssignTarget::Simple { key } if key.text == handler),
        _ => false,
    })
}

/// Wire `node`'s `on_tap` / `on_hold` handlers (block and `= name`
/// forms) to `obj`. Action-call args are evaluated against `ctx` now.
/// Returns the action call a tap makes, if any (name and args).
fn bind_handlers(node: &Block, ctx: &Context, sink: &Sender<UiEvent>, obj: &Obj<'_>) -> Option<(String, Vec<ActionArg>)> {
    let mut tap_call = None;
    // A node with an `on_hold` gets short taps only for `on_tap`, so a
    // long press does not fire both.
    let has_hold = has_handler(node, "on_hold");
    // Set for a component's root: its call has an `on_tap`.
    let call_taps = ROOT_TAP.with(|t| t.replace(false));
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
                if handler == "on_tap" {
                    tap_call = Some((name.text.clone(), args.clone()));
                }
                obj.bind_action(trigger, sink.clone(), &name.text, args);
                break;
            }
            // An inline handler with action keys (`run`, `pin`,
            // `confirm`, ...) is an anonymous action: call it like a
            // named one so its gates apply. The local bindings
            // (`item`, component params) go along as args.
            if child.is_inline_action() {
                obj.set_clickable();
                let (name, args) = (child.inline_action_name(), local_args(ctx));
                if handler == "on_tap" {
                    tap_call = Some((name.clone(), args.clone()));
                }
                obj.bind_action(trigger, sink.clone(), &name, args);
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
                let assigns = collect_inline_set(set_block, ctx);
                if !assigns.is_empty() {
                    obj.set_clickable();
                    obj.bind_inline_set(trigger, sink.clone(), assigns);
                    break;
                }
            }
        }
    }
    // `on_tap = <action>` / `on_hold = <action>`, and a button's
    // `action`, its tap unless it has an `on_tap` too.
    let has_tap = call_taps || has_handler(node, "on_tap");
    for item in &node.items {
        let Item::Assign(a) = item else { continue };
        let AssignTarget::Simple { key } = &a.target else {
            continue;
        };
        let trigger = match key.text.as_str() {
            "on_tap" => tap,
            "on_hold" => Trigger::Hold,
            "action" if node.kind.text == "button" && !has_tap => tap,
            _ => continue,
        };
        if let Some(name) = action_name(&a.value.text, a.value.span, ctx) {
            obj.set_clickable();
            if key.text != "on_hold" {
                tap_call = Some((name.clone(), Vec::new()));
            }
            obj.bind_action(trigger, sink.clone(), &name, Vec::new());
        }
    }
    tap_call
}

/// The action an `on_tap = ...` names: a bare name, or an expression
/// that gives one (`on_tap = {{ tap }}` with a component's `action`
/// param). `none`, or anything that is not a name, binds nothing.
fn action_name(raw: &str, span: paneld_config::source::Span, ctx: &Context) -> Option<String> {
    let name = match classify(raw) {
        ValueShape::Literal(t) => t.trim().to_string(),
        _ => match resolve_expr_value(raw, span, ctx) {
            Some(Value::String(s)) => s.trim().to_string(),
            _ => return None,
        },
    };
    (name != "none" && is_bare_ident(&name)).then_some(name)
}

/// Apply `node`'s plain properties (size, spacing, colors, font,
/// visibility, ...) to `obj`.
/// Properties that are not styles LVGL keeps per state: in a theme's
/// `:pressed` (etc.) body they would apply in every state.
pub const STATELESS_KEYS: &[&str] = &["width", "height", "grow", "visible", "enabled"];

fn apply_plain_props(kind: &str, items: &[Item], ctx: &Context, obj: &Obj<'_>) {
    for item in items {
        let Item::Assign(a) = item else { continue };
        let AssignTarget::Simple { key } = &a.target else {
            continue;
        };
        // In a `:state` style only what LVGL keeps per state applies
        // (the check refuses the rest there).
        if crate::ui::in_state() && STATELESS_KEYS.contains(&key.text.as_str()) {
            continue;
        }
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
            "min_width" | "max_width" | "min_height" | "max_height" => {
                if let Some(s) = parse_size(&resolve_scalar(value, a.value.span, ctx)) {
                    match key.text.as_str() {
                        "min_width" => obj.set_min_width(s),
                        "max_width" => obj.set_max_width(s),
                        "min_height" => obj.set_min_height(s),
                        _ => obj.set_max_height(s),
                    }
                }
            }
            "padding" => {
                // 1, 2 (vertical horizontal) or 4 (top right bottom
                // left) sizes in px.
                let text = resolve_scalar(value, a.value.span, ctx);
                let px: Option<Vec<i32>> = text.split_whitespace().map(|p| p.parse::<i32>().ok()).collect();
                match px.as_deref() {
                    Some(&[all]) => obj.set_padding(all),
                    Some(&[v, h]) => obj.set_padding_sides(v, h, v, h),
                    Some(&[t, r, b, l]) => obj.set_padding_sides(t, r, b, l),
                    _ => {}
                }
            }
            "gap" | "row_gap" | "column_gap" => {
                if let Ok(px) = resolve_scalar(value, a.value.span, ctx).trim().parse::<i32>() {
                    match key.text.as_str() {
                        "row_gap" => obj.set_row_gap(px),
                        "column_gap" => obj.set_column_gap(px),
                        _ => obj.set_gap(px),
                    }
                }
            }
            // In a `stack`: an offset from the `anchor` point.
            "x" | "y" => {
                if let Some(s) = parse_offset(&resolve_scalar(value, a.value.span, ctx)) {
                    if key.text == "x" {
                        obj.set_x(s);
                    } else {
                        obj.set_y(s);
                    }
                }
            }
            "anchor" => {
                if let Some(anchor) = Anchor::parse(&resolve_scalar(value, a.value.span, ctx)) {
                    obj.set_anchor(anchor);
                }
            }
            "align_self" => {
                if let Some(align) = SelfAlign::parse(&resolve_scalar(value, a.value.span, ctx)) {
                    obj.set_align_self(align);
                }
            }
            // In a `grid`: `col row [colspan rowspan]`, from 0.
            "cell" => {
                if let Some((c, r, cs, rs)) = parse_cell(&resolve_scalar(value, a.value.span, ctx)) {
                    obj.set_grid_cell(c, r, cs, rs);
                }
            }
            "overflow" if kind == "text" => {
                if let Some(mode) = TextOverflow::parse(&resolve_scalar(value, a.value.span, ctx)) {
                    obj.set_text_overflow(mode);
                }
            }
            "track" if matches!(kind, "arc" | "bar") => {
                if let Some((rgb, opa)) = resolve_color(value, a.value.span, ctx) {
                    if kind == "arc" {
                        obj.set_arc_track(rgb, opa);
                    } else {
                        obj.set_bar_track(rgb, opa);
                    }
                }
            }
            "thickness" if kind == "arc" => {
                if let Some(Size::Px(px)) = parse_size(&resolve_scalar(value, a.value.span, ctx)) {
                    obj.set_arc_thickness(px.clamp(1, 256));
                }
            }
            "rounded" if kind == "arc" => {
                let flag = resolve_expr_value(value, a.value.span, ctx).is_some_and(|v| value_truthy(&v));
                obj.set_arc_rounded(flag);
            }
            "background" => {
                if let Some((rgb, opa)) = resolve_color(value, a.value.span, ctx) {
                    obj.set_background_hex(rgb, opa);
                }
            }
            "color" => {
                if let Some((rgb, opa)) = resolve_color(value, a.value.span, ctx) {
                    // A gauge's foreground is its indicator.
                    match kind {
                        "arc" => obj.set_arc_indicator(rgb, opa),
                        "bar" => obj.set_bar_indicator(rgb, opa),
                        _ => obj.set_text_color_hex(rgb, opa),
                    }
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
            "align" if !matches!(kind, "row" | "column" | "scroll") => {
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
                    obj.set_border(w, parts.next().and_then(|c| color_of(c, ctx)));
                }
            }
            "scale" => {
                if let Ok(x) = resolve_scalar(value, a.value.span, ctx).trim().parse::<f64>() {
                    obj.set_scale(x);
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
                obj.set_disabled(!flag);
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
/// Chart values are integers in LVGL: keep two decimals.
const CHART_SCALE: f64 = 100.0;

/// Schema 7 `chart`: `type`, `min`, `max`, `points`, `divisions`, and
/// one `series { values; color }` per line or bar set. `values` is a
/// list of numbers, or of `{value}` maps such as a source's
/// `history`. With fewer values than `points` the series is drawn
/// against the right edge, newest last; with more, the newest
/// `points` are shown. `min` / `max` default to the data's range.
fn render_chart(node: &Block, ctx: &Context, obj: &Obj<'_>) {
    let number = |v: &Value| match v {
        Value::Int(n) => Some(*n as f64),
        Value::Double(d) if d.is_finite() => Some(*d),
        Value::Map(m) => match m.get("value") {
            Some(Value::Int(n)) => Some(*n as f64),
            Some(Value::Double(d)) if d.is_finite() => Some(*d),
            _ => None,
        },
        _ => None,
    };
    let accent = color_of("accent", ctx).unwrap_or((0x2196f3, 255));
    type Series = (Vec<Option<f64>>, (u32, u8));
    let series: Vec<Series> = node
        .items
        .iter()
        .filter_map(|it| match it {
            Item::Block(b) if b.kind.text == "series" => Some(b),
            _ => None,
        })
        .map(|b| {
            let values = match resolve_assign_value(b, "values", ctx) {
                Some(Value::List(l)) => l.iter().map(number).collect(),
                _ => Vec::new(),
            };
            let color = resolve_text(b, "color", ctx).and_then(|c| color_of(&c, ctx)).unwrap_or(accent);
            (values, color)
        })
        .collect();
    let longest = series.iter().map(|(v, _)| v.len()).max().unwrap_or(0);
    let points = resolve_num(node, "points", ctx)
        .map(|p| p.max(1.0) as usize)
        .unwrap_or(longest.max(2))
        .min(1000);
    let shown = |v: &[Option<f64>]| v[v.len().saturating_sub(points)..].to_vec();
    let all: Vec<f64> = series.iter().flat_map(|(v, _)| shown(v)).flatten().collect();
    let lo = all.iter().copied().fold(f64::INFINITY, f64::min);
    let hi = all.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let (mut min, mut max) = if lo.is_finite() { (lo, hi) } else { (0.0, 100.0) };
    if (max - min).abs() < f64::EPSILON {
        // A flat line in the middle rather than on an edge.
        min -= 1.0;
        max += 1.0;
    }
    let min = resolve_num(node, "min", ctx).unwrap_or(min);
    let max = resolve_num(node, "max", ctx).unwrap_or(max);
    let (hdiv, vdiv) = find_simple(node, "divisions")
        .and_then(|d| {
            let mut it = d.split_whitespace().map(|n| n.parse::<u32>().ok());
            Some((it.next()??, it.next()??))
        })
        .unwrap_or((3, 0));
    let scaled = |x: f64| (x * CHART_SCALE).round().clamp(i32::MIN as f64 + 1.0, i32::MAX as f64 - 1.0) as i32;
    let ty = find_simple(node, "type").map(str::trim);
    let bars = ty == Some("bar");
    obj.chart_setup(bars, points as u32, scaled(min), scaled(max), hdiv.min(20), vdiv.min(20));
    if ty == Some("area") {
        obj.chart_fill_area();
    }
    for (values, color) in &series {
        let tail = shown(values);
        let mut padded: Vec<Option<i32>> = vec![None; points - tail.len()];
        padded.extend(tail.iter().map(|v| v.map(scaled)));
        obj.chart_add_series(color.0, color.1, &padded);
    }
}

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

/// Wire a value widget's `on_change` / `on_submit` when it calls an
/// action: `= name`, `name { args }`, or an inline block with action
/// keys (`run`, `pin`, `confirm`, ...), which is an anonymous action
/// so its gates apply as for `on_tap`. Otherwise returns the payload
/// of a plain `{ set { ... } }` for the caller to bind.
fn bind_change_action(
    node: &Block,
    handler: &str,
    widget: ValueWidget,
    ctx: &Context,
    sink: &Sender<UiEvent>,
    obj: &Obj<'_>,
) -> Option<Vec<InlineSetItem>> {
    if let Some((raw, span)) = find_raw(node, handler) {
        if let Some(action) = action_name(raw, span, ctx) {
            obj.on_change_action(widget, sink.clone(), action, local_args(ctx));
            return None;
        }
    }
    for item in &node.items {
        let Item::Block(child) = item else { continue };
        if child.kind.text != handler {
            continue;
        }
        if let Some(name) = &child.name {
            obj.on_change_action(widget, sink.clone(), name.text.clone(), collect_action_args(child, ctx));
            return None;
        }
        if child.is_inline_action() {
            obj.on_change_action(widget, sink.clone(), child.inline_action_name(), local_args(ctx));
            return None;
        }
        if let Some(set_block) = find_block(child, "set") {
            let assigns = collect_inline_set(set_block, ctx);
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

fn collect_inline_set(set_block: &Block, ctx: &Context) -> Vec<InlineSetItem> {
    // `page` and `nav` too: only a drawn page has them (they are not
    // action args, which would change what counts as the same call).
    let mut locals = local_args(ctx);
    for k in ["page", "nav"] {
        if let Some(v) = ctx.get(k) {
            locals.push(ActionArg { name: k.into(), value: v.clone() });
        }
    }
    let mut out = Vec::new();
    for item in &set_block.items {
        let Item::Assign(a) = item else { continue };
        let AssignTarget::Simple { key } = &a.target else {
            continue;
        };
        out.push(InlineSetItem {
            key: key.text.clone(),
            value_raw: a.value.text.clone(),
            locals: locals.clone(),
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
        // A literal `visible = false` arrives as text.
        Value::String(s) => paneld_config::value::parse_bool(s.trim()).unwrap_or(!s.is_empty()),
        Value::List(items) => !items.is_empty(),
        Value::Map(m) => !m.is_empty(),
    }
}

/// A name, or a remote member's `device1.reboot` (schema 17).
fn is_bare_ident(s: &str) -> bool {
    s.split('.').all(|seg| {
        let b = seg.as_bytes();
        !b.is_empty()
            && (b[0].is_ascii_alphabetic() || b[0] == b'_')
            && b.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'_')
    })
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

/// The alpha of a faint line or track: a gauge's default track, a
/// chart's division lines.
const FAINT: u8 = 0x40;

/// A gauge's default indicator (the theme's `accent`) and track (its
/// `muted`, faint).
fn gauge_colors(ctx: &Context) -> ((u32, u8), (u32, u8)) {
    let accent = color_of("accent", ctx).unwrap_or((0x2196f3, 255));
    let muted = color_of("muted", ctx).unwrap_or((0x757575, 255)).0;
    (accent, (muted, FAINT))
}

/// A grid's `columns` / `rows`: a count, or a list of tracks (`[120,
/// 1fr, content]`, also without the brackets or commas).
fn parse_tracks(v: &Value) -> Option<Tracks> {
    let items: Vec<String> = match v {
        Value::Int(n) => return Some(Tracks::Count((*n).clamp(1, 1024) as i32)),
        Value::Double(d) if d.is_finite() => return Some(Tracks::Count(d.clamp(1.0, 1024.0) as i32)),
        Value::List(l) => l.iter().map(|x| x.to_display_string()).collect(),
        Value::String(s) => {
            let t = s.trim();
            if let Ok(n) = t.parse::<i32>() {
                return Some(Tracks::Count(n.clamp(1, 1024)));
            }
            t.trim_start_matches('[')
                .trim_end_matches(']')
                .split(|c: char| c == ',' || c.is_whitespace())
                .filter(|p| !p.is_empty())
                .map(str::to_string)
                .collect()
        }
        _ => return None,
    };
    let tracks: Option<Vec<Track>> = items.iter().map(|t| Track::parse(t)).collect();
    tracks.filter(|t| !t.is_empty()).map(Tracks::List)
}

/// `col row [colspan rowspan]`, from 0.
fn parse_cell(s: &str) -> Option<(u8, u8, u8, u8)> {
    let n: Option<Vec<u8>> = s.split_whitespace().map(|p| p.parse::<u8>().ok()).collect();
    match *n?.as_slice() {
        [c, r] => Some((c, r, 1, 1)),
        [c, r, cs, rs] if cs > 0 && rs > 0 => Some((c, r, cs, rs)),
        _ => None,
    }
}

/// A `stack` child's `x` / `y`: px or a percentage, either may be
/// negative.
fn parse_offset(s: &str) -> Option<Size> {
    let s = s.trim();
    match s.strip_suffix('%') {
        Some(p) => p.trim().parse::<i16>().ok().filter(|p| p.abs() <= 1000).map(Size::Percent),
        None => s.parse::<i32>().ok().map(Size::Px),
    }
}

fn parse_size(s: &str) -> Option<Size> {
    let s = s.trim();
    if s == "content" {
        return Some(Size::Content);
    }
    if let Some(p) = s.strip_suffix('%') {
        return p.trim().parse::<u16>().ok().map(|p| Size::Percent(p.min(1000) as i16));
    }
    s.parse::<i32>().ok().map(Size::Px)
}

/// Resolve a color value's raw text. Accepts a literal color (a hex
/// `#rgb`, `#rrggbb`, `#rrggbbaa`, or a theme token's name), a
/// `{{ expr }}` whose evaluated string is one, or an interpolated
/// value whose rendered string is one. Returns (rgb, alpha).
fn resolve_color(
    value: &str,
    span: paneld_config::source::Span,
    ctx: &Context,
) -> Option<(u32, u8)> {
    match classify(value) {
        ValueShape::Literal(text) => color_of(text, ctx),
        ValueShape::Expression(body) => {
            let (expr, diags) = cel::parse_fragment(body, span.file, span.start + 2);
            if !diags.is_empty() {
                return None;
            }
            let v = expr.and_then(|e| eval(&e, ctx).ok())?;
            color_of(&v.to_display_string(), ctx)
        }
        ValueShape::Interp(_) => {
            // Rarely useful for colors; evaluate the whole thing
            // and hope the result is a color.
            resolve_expr_value(value, span, ctx)
                .and_then(|v| color_of(&v.to_display_string(), ctx))
        }
    }
}

/// A hex color, or the name of a token in the context's `theme` map
/// whose value is one (a token may name another, a few levels deep).
fn color_of(s: &str, ctx: &Context) -> Option<(u32, u8)> {
    let theme = match ctx.get("theme") {
        Some(Value::Map(t)) => Some(t),
        _ => None,
    };
    theme_color(s, theme)
}

/// A hex color, or a token of `theme` naming one (through other
/// names, a few levels deep, so a cycle ends).
fn theme_color(s: &str, theme: Option<&std::collections::BTreeMap<String, Value>>) -> Option<(u32, u8)> {
    let mut text = s.trim().to_string();
    for _ in 0..8 {
        if let Some(c) = parse_hex_color(&text) {
            return Some(c);
        }
        text = theme?.get(text.as_str())?.to_display_string().trim().to_string();
    }
    None
}

/// `#rgb`, `#rrggbb`, `#rrggbbaa` as (rgb, alpha); alpha is 255
/// without the `aa`.
fn parse_hex_color(s: &str) -> Option<(u32, u8)> {
    let c = paneld_config::value::parse_color(s)?;
    Some((((c.r as u32) << 16) | ((c.g as u32) << 8) | c.b as u32, c.a))
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

    #[test]
    fn theme_colors_follow_names_and_end_on_a_cycle() {
        let s = |t: &str| Value::String(t.into());
        let theme: std::collections::BTreeMap<String, Value> = [
            ("warning".to_string(), s("#ef6c0080")),
            ("critical".to_string(), s("warning")),
            ("a".to_string(), s("b")),
            ("b".to_string(), s("a")),
        ]
        .into();
        // The toast's `critical` names another token (review round 1).
        assert_eq!(theme_color("critical", Some(&theme)), Some((0xef6c00, 0x80)));
        assert_eq!(theme_color("#fff", None), Some((0xffffff, 255)));
        assert_eq!(theme_color("a", Some(&theme)), None);
    }

    #[test]
    fn a_quoted_literal_loses_its_quotes() {
        let span = paneld_config::source::Span::new(paneld_config::source::FileId::default(), 0, 0);
        let v = resolve_value("\" a; b \"", span, &Context::new());
        assert_eq!(v, Some(Value::String(" a; b ".into())));
        assert!(is_bare_ident("device1.reboot") && !is_bare_ident("a..b") && !is_bare_ident("a."));
    }

    #[test]
    fn interpolation_keeps_text_and_finds_the_real_end() {
        let mut ctx = Context::new();
        ctx.insert("x".into(), Value::String("42".into()));
        let f = paneld_config::source::SourceMap::new().add("t".into(), String::new());
        let i = |raw: &str| interpolate(raw, f, 0, &ctx);
        // Review round 6: non-ASCII text next to `{{ }}` was mangled.
        assert_eq!(i("Caf\u{e9} {{ x }} \u{65e5}"), "Caf\u{e9} 42 \u{65e5}");
        assert_eq!(i("a {{ x == \"}}\" ? 1 : 2 }} b"), "a 2 b");
        assert_eq!(i("{{ {'a': {'b': x}}['a']['b'] }}!"), "42!");
        assert_eq!(i("\\{{ x }} {{ x }}"), "{{ x }} 42");
        // `##` is `#` in the text, not inside `{{ }}`.
        assert_eq!(i("Item ##{{ x }}"), "Item #42");
        assert_eq!(i("{{ '##' + x }}"), "##42");
        assert_eq!(i("a ## {{ x }} ##"), "a # 42 #");
    }
}
