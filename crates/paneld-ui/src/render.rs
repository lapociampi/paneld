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

use crate::expr::{eval_in, Context, Scope, Value};
use crate::nodes::{self, Dep, Kind, Mode, Visit};
use crate::ui::{
    ActionArg, Anchor, FlexAlign, ImageFit, InlineSetItem, Obj, ScrollSnap, SelfAlign, Size, TextOverflow, Track, Tracks,
    Trigger, UiEvent, ValueWidget,
};

/// Children of a component call plus the outer context they should
/// resolve against. Threaded through [`render_node`] so a `slot { }`
/// inside the component's body expands to the caller's children.
struct SlotCtx<'a> {
    children: &'a [Item],
    outer_ctx: &'a Scope<'a>,
    /// The caller's names, as dep keys (ADR 0024).
    frame: Option<usize>,
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
/// diagnostic. The page is recorded for in-place updates
/// ([`update_page`], ADR 0024).
pub fn render_page(
    doc: &Document,
    page_name: &str,
    ctx: &Context,
    sink: &Sender<UiEvent>,
    screen: &Obj<'_>,
) {
    nodes::begin_build(page_name);
    nodes::with_mode(Mode::Build, || draw_page(doc, page_name, ctx, sink, screen));
}

fn draw_page(doc: &Document, page_name: &str, ctx: &Context, sink: &Sender<UiEvent>, screen: &Obj<'_>) {
    let ctx = &Scope::root(ctx);
    let critical = color_of("critical", ctx);
    FAILED_BORDER.with(|c| c.set(critical.unwrap_or((0xf85149, 255))));
    // The inputs and the dialogs over this page in the theme's colors.
    crate::ui::set_input_colors(input_colors(ctx));
    let scratch = nodes::mode() == Mode::Scratch;
    match find_page(doc, page_name) {
        Some(b) => {
            if !scratch {
                crate::motion::begin(page_name, theme_of(ctx));
            }
            render_page_body(b, doc, ctx, sink, screen);
            if !scratch {
                crate::motion::end();
            }
        }
        None => {
            nodes::mark_broken("no such page");
            screen.new_label(&format!("page `{page_name}` not found"));
        }
    }
}

fn find_page<'a>(doc: &'a Document, page_name: &str) -> Option<&'a Block> {
    doc.items.iter().find_map(|item| match item {
        Item::Block(b) if b.kind.text == "page" && b.name.as_ref().is_some_and(|n| n.text == page_name) => Some(b),
        _ => None,
    })
}

/// What an update in place did (ADR 0024).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Updated {
    /// Nothing on the page reads what changed.
    Nothing,
    /// Updated: records walked into, nodes set up again, objects
    /// restyled.
    Done { visited: usize, reapplied: usize, redrawn: usize },
    /// It cannot be done in place: build the page again.
    Rebuild(&'static str),
}

/// Update the page drawn last (`page_name`, by [`render_page`]) in
/// place for `changes`, from `ctx` as it is now: only the nodes that
/// read what changed are set up again, each as a fresh render would
/// make it. [`Updated::Rebuild`] when that cannot be done (the caller
/// renders the page again).
pub fn update_page(
    doc: &Document,
    page_name: &str,
    ctx: &Context,
    sink: &Sender<UiEvent>,
    changes: &nodes::Changes,
) -> Updated {
    let start = match nodes::begin_update(page_name, changes) {
        Err(why) => return Updated::Rebuild(why),
        Ok(None) => return Updated::Nothing,
        Ok(Some(s)) => s,
    };
    let Some(page) = find_page(doc, page_name) else { return Updated::Rebuild("no such page") };
    let scope = &Scope::root(ctx);
    crate::motion::begin_update(theme_of(scope));
    crate::ui::begin_update();
    nodes::with_mode(Mode::Update, || {
        let locals = page_locals(doc, page, scope);
        let pctx = scope.with(&locals);
        let page_root = Obj::from_raw(start.page_root);
        page_items(page, doc, &pctx, sink, &page_root);
    });
    let redrawn = crate::ui::end_update();
    crate::motion::end_update();
    crate::ui::settle_backgrounds();
    match nodes::end_update() {
        Ok((visited, reapplied)) => Updated::Done { visited, reapplied, redrawn },
        Err(why) => Updated::Rebuild(why),
    }
}

/// When the soonest `ago()` text on the page drawn reads otherwise
/// (Unix seconds).
pub fn page_ago_due() -> Option<i64> {
    nodes::page_ago()
}

/// The shell's or the page's items into the page root.
fn page_items(page: &Block, doc: &Document, pctx: &Scope<'_>, sink: &Sender<UiEvent>, page_root: &Obj<'_>) {
    match find_component(doc, "shell") {
        // Schema 8: the `shell` component wraps every page; the page's
        // nodes fill its `slot`.
        Some(shell) => {
            let slot = SlotCtx { children: &page.items, outer_ctx: pctx, frame: nodes::current_frame() };
            render_item_list(&shell.items, doc, pctx, Some(&slot), sink, page_root);
        }
        None => render_item_list(&page.items, doc, pctx, None, sink, page_root),
    }
}

/// Build page `page_name` afresh on a scratch screen, as
/// [`render_page`] would now, and compare it with the page on screen
/// (ADR 0024, "Correctness guard"): `Err` names the first node that
/// differs. Nothing it draws is remembered; the scratch screen is
/// deleted before it returns.
pub fn verify_page(doc: &Document, page_name: &str, ctx: &Context, sink: &Sender<UiEvent>, screen: &Obj<'_>) -> Result<(), String> {
    let Some(live) = nodes::holder() else { return Ok(()) };
    let (scratch, due) = scratch_build(doc, page_name, ctx, sink);
    screen.update_layout();
    scratch.update_layout();
    let fresh = scratch.child(0);
    let result = match fresh {
        Some(fresh) => crate::ui::compare_trees(&Obj::from_raw(live), &fresh),
        None => Err("the fresh render drew nothing".to_string()),
    };
    scratch.delete();
    result?;
    // And the page is drawn again when an `ago()` on it reads otherwise,
    // as a fresh page would be.
    let ours = nodes::page_ago();
    if ours != due {
        return Err(format!("`ago()` texts timed for {ours:?}, a fresh render times them for {due:?}"));
    }
    Ok(())
}

/// A fresh render of `page_name` on a new screen that is not shown,
/// with nothing of it remembered (no motion, no node table).
pub fn scratch_page<'u>(doc: &Document, page_name: &str, ctx: &Context, sink: &Sender<UiEvent>) -> Obj<'u> {
    scratch_build(doc, page_name, ctx, sink).0
}

/// [`scratch_page`], and when its soonest `ago()` text reads otherwise.
fn scratch_build<'u>(doc: &Document, page_name: &str, ctx: &Context, sink: &Sender<UiEvent>) -> (Obj<'u>, Option<i64>) {
    let scratch = Obj::new_scratch_screen();
    let ago = crate::expr::take_ago_next();
    crate::motion::scratch(true);
    nodes::with_mode(Mode::Scratch, || draw_page(doc, page_name, ctx, sink, &scratch));
    crate::motion::scratch(false);
    let due = crate::expr::take_ago_next();
    crate::expr::restore_ago_next(ago);
    (scratch, due)
}

/// Show `screen` on the display (a scratch page for `paneld shot
/// --verify`, then the real one again).
pub fn show_screen(screen: &Obj<'_>) {
    screen.load_screen();
}

/// Delete a scratch screen made by [`scratch_page`].
pub fn delete_screen(screen: Obj<'_>) {
    screen.delete();
}

fn render_page_body(
    page: &Block,
    doc: &Document,
    ctx: &Scope<'_>,
    sink: &Sender<UiEvent>,
    screen: &Obj<'_>,
) {
    // `page` and `nav` are visible to the shell and to the page body.
    let locals = page_locals(doc, page, ctx);
    let pctx = ctx.with(&locals);
    let _frame = (nodes::mode() == Mode::Build).then(|| nodes::push_frame(page_frame(doc, page)));

    // The theme's `font` is the text font of everything on the page,
    // the dialogs on the top layer included; a node's `font` wins.
    apply_font(screen, "font", theme_of(ctx));
    // (A scratch copy leaves the real top layer alone: setting it, even
    // to the same font, redraws the whole screen.)
    if nodes::mode() != Mode::Scratch {
        apply_font(&screen.top_layer(), "font", theme_of(ctx));
    }
    // And `text` its text color (a dark theme's light text); widgets
    // with a box of their own keep theirs.
    if let Some((rgb, opa)) = color_of("text", ctx) {
        screen.set_text_color_hex(rgb, opa);
    }

    // The whole page in one holder, which a page transition moves.
    let holder = screen.new_page_holder();
    // Behind every page, two layers: the theme's `background` (a
    // color, gradient or image) on a backdrop of the screen's size, and
    // its `background_image` over it on the page root, so each has its
    // own background slot.
    let backdrop = holder.new_container();
    backdrop.set_size(Size::Percent(100), Size::Percent(100));
    backdrop.set_enabled(false);
    backdrop.set_border(0, None);
    backdrop.set_radius(0);
    let page_root = holder.new_container();
    page_root.set_size(Size::Percent(100), Size::Percent(100));
    page_root.set_flex_column();
    page_root.set_border(0, None);
    page_root.set_radius(0);
    page_root.set_background_hex(0, 0);
    if let Some(theme) = theme_of(ctx) {
        if let Some(bg) = theme.get("background") {
            apply_background(&bg.to_display_string(), ctx, &backdrop);
        }
        let image = theme.get("background_image").map(|v| v.to_display_string()).unwrap_or_default();
        if !image.trim().is_empty() {
            page_root.set_background_image(image.trim(), paneld_config::paint::Fit::Cover);
        }
    }
    if let Some((rgb, opa)) = color_of("text", ctx) {
        page_root.set_text_color_hex(rgb, opa);
    }

    if find_component(doc, "shell").is_some() {
        page_root.set_padding(0);
        page_root.set_gap(0);
    } else {
        page_root.set_padding(16);
        page_root.set_gap(8);
    }
    nodes::set_roots(&holder, &page_root);
    page_items(page, doc, &pctx, sink, &page_root);
    // Image backgrounds are fitted to their node's size, known now.
    crate::ui::settle_backgrounds();
}

/// `page` and `nav`; an `ago()` in a title is timed with the page.
fn page_locals(doc: &Document, page: &Block, ctx: &Scope<'_>) -> Context {
    let saved = crate::expr::take_ago_next();
    let locals = Context::from([("page".into(), page_info(page, ctx)), ("nav".into(), nav_entries(doc, page, ctx))]);
    let due = crate::expr::take_ago_next();
    nodes::set_page_ago(due);
    crate::expr::restore_ago_next(min_opt(saved, due));
    locals
}

/// `{ name, title, icon }` of a page; `title` defaults to the name.
fn page_info(page: &Block, ctx: &Scope<'_>) -> Value {
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
fn nav_entries(doc: &Document, current: &Block, ctx: &Scope<'_>) -> Value {
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
    ctx: &Scope<'_>,
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
/// Updating in place, the items are walked again only when their keys
/// are the ones drawn (else the page is built again: ADR 0024 phase 1).
fn expand_repeat(
    r: &paneld_config::parse::Repeat,
    doc: &Document,
    ctx: &Scope<'_>,
    slot: Option<&SlotCtx<'_>>,
    sink: &Sender<UiEvent>,
    parent: &Obj<'_>,
) {
    let mode = nodes::mode();
    let block = r as *const paneld_config::parse::Repeat as usize;
    let body_offset = r.source.span.start + 2;
    let parsed = parse_fragment_cached(&r.source.body, r.source.span.file, body_offset);
    let rec = match mode {
        Mode::Update => match nodes::visit(Kind::Repeat, block, false) {
            None | Some(Visit::Skip) => return,
            Some(Visit::Look(i)) => Some(i),
        },
        Mode::Build => {
            let mut own = Vec::new();
            if let Some(e) = parsed.0.as_ref() {
                nodes::expr_deps(parse_key(&parsed), e, &mut own);
            }
            own.sort();
            own.dedup();
            Some(nodes::open(Kind::Repeat, block, own, "repeat", r.source.span))
        }
        Mode::Off | Mode::Scratch => None,
    };
    let finish = |rec: Option<usize>| match (mode, rec) {
        (Mode::Build, Some(i)) => nodes::close(i),
        (Mode::Update, Some(i)) => nodes::leave(i),
        _ => {}
    };
    let (expr, diags) = (&parsed.0, &parsed.1);
    let failed = |text: String| {
        nodes::mark_broken("a repeat that does not draw");
        if mode != Mode::Update {
            parent.new_label(&text);
        }
    };
    if !diags.is_empty() {
        failed(format!("[repeat parse: {}]", diags[0].message));
        return finish(rec);
    }
    let Some(expr) = expr.as_ref() else {
        return finish(rec);
    };
    let items = match eval_in(expr, ctx) {
        Ok(Value::List(xs)) => xs,
        Ok(other) => {
            failed(format!("[repeat: expected a list, got {}]", display_type(&other)));
            return finish(rec);
        }
        Err(e) => {
            failed(format!("[repeat eval: {}]", e.message));
            return finish(rec);
        }
    };
    let keys: Vec<String> = items.iter().enumerate().map(|(i, item)| crate::motion::item_key(item, i)).collect();
    match (mode, rec) {
        (Mode::Build, Some(i)) => nodes::set_keys(i, keys.clone()),
        (Mode::Update, Some(i)) if nodes::keys(i) != keys => {
            // Phase 2 diffs the items by key; until then, the page.
            nodes::mark_broken("a repeat's items changed");
            return finish(rec);
        }
        _ => {}
    }
    // Every item's binding reads what the list does.
    let _frame = match (mode, rec) {
        (Mode::Build, Some(i)) => Some(nodes::push_frame(vec![(r.binding.text.clone(), nodes::own_of(i))])),
        _ => None,
    };
    for (i, item) in items.into_iter().enumerate() {
        crate::motion::push_scope(&keys[i]);
        let inner = ctx.with_one(&r.binding.text, &item);
        render_item_list(&r.items, doc, &inner, slot, sink, parent);
        crate::motion::pop_scope();
    }
    drop(_frame);
    finish(rec);
}

/// The key a parse is known by in the node table's caches: its place
/// in the parse cache.
fn parse_key<T>(p: &std::rc::Rc<T>) -> usize {
    std::rc::Rc::as_ptr(p) as *const () as usize
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

thread_local! {
    /// The next widget drawn is a component call's root: set by the
    /// call, taken by that widget (or a nested call).
    static ROOT_PENDING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// The root of the call under way was built or set up again (its
    /// call's own keys go on it then), and its object.
    static ROOT_REDONE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static ROOT_OBJ: std::cell::Cell<*mut lvgl_sys::lv_obj_t> = const { std::cell::Cell::new(std::ptr::null_mut()) };
    /// Update: the next widget (a call's root) is set up again whatever
    /// it reads (its call's own keys changed).
    static FORCE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Update: a node under the one being walked was set up again (its
    /// parent lays out its children again).
    static REDONE_UNDER: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// The widget being built is a component call's root.
    static WIDGET_IS_ROOT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// The widget kinds (what is not a `slot` or a component call).
const WIDGETS: &[&str] = &[
    "column", "row", "stack", "grid", "scroll", "text", "button", "icon", "rect", "bar", "arc", "line", "image", "chart", "spinner",
    "qr", "toggle", "slider", "keypad", "select",
];

fn render_node(
    node: &Block,
    doc: &Document,
    ctx: &Scope<'_>,
    slot: Option<&SlotCtx<'_>>,
    sink: &Sender<UiEvent>,
    parent: &Obj<'_>,
) {
    let kind = node.kind.text.as_str();
    if kind == "slot" {
        // Expand the caller's child-items here, resolving them
        // against the OUTER context (not the component's inner
        // scope). If there's no enclosing component call, the
        // slot renders nothing.
        if let Some(s) = slot {
            let _frame = nodes::switch_frame(s.frame);
            render_item_list(s.children, doc, s.outer_ctx, None, sink, parent);
        }
        return;
    }
    if !WIDGETS.contains(&kind) {
        // A user-defined component: its body in place, each arg bound
        // in a scope of its own.
        if let Some(comp) = find_component(doc, kind) {
            expand_component(node, comp, doc, ctx, sink, parent);
        } else {
            build_widget(node, doc, ctx, slot, sink, parent);
        }
        return;
    }
    let is_root = ROOT_PENDING.with(|r| r.replace(false));
    let forced = FORCE.with(|f| f.replace(false));
    NODE_TAP.with(|t| t.set(ROOT_TAP.with(|r| r.replace(false))));
    let block = node as *const Block as usize;
    let mut redone = true;
    match nodes::mode() {
        Mode::Build => {
            let mut own = Vec::new();
            node_deps(node, ctx, &mut own);
            if kind == "icon" {
                // A theme PNG takes the text color of what it is in.
                nodes::union(&mut own, &nodes::ancestor_deps());
            }
            let idx = nodes::open(Kind::Node, block, own, kind, node.kind.span);
            let ago = crate::expr::take_ago_next();
            build_widget(node, doc, ctx, slot, sink, parent);
            let mine = crate::expr::take_ago_next();
            nodes::set_ago(idx, mine.filter(|_| nodes::reads(idx, &Dep::Ago)));
            crate::expr::restore_ago_next(min_opt(ago, mine));
            if is_root {
                ROOT_OBJ.with(|r| r.set(nodes::objs(idx).first().copied().unwrap_or(std::ptr::null_mut())));
            }
            nodes::close(idx);
        }
        Mode::Update => {
            let Some(visit) = nodes::visit(Kind::Node, block, forced) else { return };
            let Visit::Look(idx) = visit else {
                if is_root {
                    ROOT_REDONE.with(|r| r.set(false));
                }
                return;
            };
            if is_root {
                ROOT_OBJ.with(|r| r.set(nodes::objs(idx).first().copied().unwrap_or(std::ptr::null_mut())));
            }
            let ago = crate::expr::take_ago_next();
            // The fingerprint is taken whenever the node is set up again
            // (forced too), so it is always that of what is shown.
            let own = nodes::own_hit(idx);
            let changed = (own || forced) && nodes::fp_changed(idx, node_fp(node, ctx, parent));
            let reapply = forced || (own && (changed || nodes::call_hit(idx)));
            if reapply {
                nodes::begin_reuse(idx);
                WIDGET_IS_ROOT.with(|r| r.set(is_root));
                build_widget(node, doc, ctx, slot, sink, parent);
                nodes::end_reuse(idx);
                REDONE_UNDER.with(|r| r.set(true));
            } else {
                redone = false;
                walk_children(node, idx, doc, ctx, slot, sink);
            }
            let mine = crate::expr::take_ago_next();
            if nodes::reads(idx, &Dep::Ago) && nodes::own_hit(idx) {
                nodes::set_ago(idx, mine);
            }
            crate::expr::restore_ago_next(min_opt(ago, mine));
            nodes::leave(idx);
        }
        Mode::Off | Mode::Scratch => build_widget(node, doc, ctx, slot, sink, parent),
    }
    if is_root {
        ROOT_REDONE.with(|r| r.set(redone));
    }
}

fn min_opt(a: Option<i64>, b: Option<i64>) -> Option<i64> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.min(y)),
        (x, y) => x.or(y),
    }
}

/// A hash of `v`, for what an object was made with.
fn sig_of(v: &impl std::hash::Hash) -> u64 {
    use std::hash::Hasher;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    v.hash(&mut h);
    h.finish()
}

/// A layout block's object.
fn layout<'u>(parent: &Obj<'u>) -> Obj<'u> {
    nodes::made(parent, "layout", 0, |p| p.new_layout(), |_| {})
}

/// What a container does with its children once they are drawn: a
/// grid places them, `align = stretch` sizes them.
fn after_children(node: &Block, ctx: &Scope<'_>, obj: &Obj<'_>) {
    match node.kind.text.as_str() {
        "grid" => {
            let cols = resolve_assign_value(node, "columns", ctx).and_then(|v| parse_tracks(&v)).unwrap_or(Tracks::Count(2));
            let rows = resolve_assign_value(node, "rows", ctx).and_then(|v| parse_tracks(&v));
            obj.layout_grid(&cols, rows.as_ref());
        }
        "row" | "column" | "scroll" if find_simple(node, "align").map(str::trim) == Some("stretch") => {
            let column = match node.kind.text.as_str() {
                "scroll" => find_simple(node, "direction").map(str::trim) != Some("horizontal"),
                k => k == "column",
            };
            obj.stretch_children(column);
        }
        _ => {}
    }
}

/// Update: widget `node` (record `idx`) reads the same as it did:
/// only what is under it is walked, and if any of it was set up
/// again, the node does again what it does with its children.
fn walk_children(node: &Block, idx: usize, doc: &Document, ctx: &Scope<'_>, slot: Option<&SlotCtx<'_>>, sink: &Sender<UiEvent>) {
    let has_children = node.items.iter().any(|it| matches!(it, Item::Block(b) if !is_event_handler_block(&b.kind.text)) || matches!(it, Item::Repeat(_)));
    if !has_children {
        return;
    }
    let Some(&raw) = nodes::objs(idx).first() else { return };
    let obj = Obj::from_raw(raw);
    let before = REDONE_UNDER.with(|r| r.replace(false));
    render_item_list(&node.items, doc, ctx, slot, sink, &obj);
    let under = REDONE_UNDER.with(|r| r.get());
    REDONE_UNDER.with(|r| r.set(before || under));
    if under {
        after_children(node, ctx, &obj);
        if node.kind.text == "button" {
            // A child shown again by its own `visible` stays hidden
            // while the button's call runs.
            obj.reassert_busy();
        }
    }
}

/// Build widget `node` (a fresh render), or set it up again on the
/// objects it made (an update: [`nodes::made`] hands them back).
fn build_widget(
    node: &Block,
    doc: &Document,
    ctx: &Scope<'_>,
    slot: Option<&SlotCtx<'_>>,
    sink: &Sender<UiEvent>,
    parent: &Obj<'_>,
) {
    match node.kind.text.as_str() {
        "column" | "row" => {
            let obj = layout(parent);
            // Taps go to the enclosing node (a button) unless it has
            // an `on_tap` / `on_hold` of its own.
            obj.set_enabled(false);
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
            after_children(node, ctx, &obj);
        }
        "stack" => {
            let obj = layout(parent);
            obj.set_enabled(false);
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
            let obj = layout(parent);
            obj.set_enabled(false);
            obj.set_size(Size::Percent(100), Size::Content);
            apply_props(node, ctx, sink, &obj);
            render_item_list(&node.items, doc, ctx, slot, sink, &obj);
            // Lay out after rendering so `repeat` output counts.
            after_children(node, ctx, &obj);
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
            let obj = layout(parent);
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
            after_children(node, ctx, &obj);
        }
        "text" => {
            let text = resolve_text(node, "value", ctx).unwrap_or_default();
            let obj = nodes::made(parent, "label", 0, |p| p.new_label(&text), |o| o.set_label_text(&text));
            apply_props(node, ctx, sink, &obj);
            obj.fit_dots();
        }
        "button" => {
            let obj = nodes::made(parent, "button", 0, |p| p.new_button(), |_| {});
            // A component's root whose call has an `on_tap`: the call
            // says where its call stands.
            let call_taps = NODE_TAP.with(|t| t.get());
            // A failed call's default look, under the theme's
            // `button:failed`: a border in the theme's critical color.
            let critical = FAILED_BORDER.with(|c| c.get());
            crate::ui::with_state("failed", || obj.set_border(3, Some(critical)));
            let tap_call = apply_props(node, ctx, sink, &obj);
            {
                // Shortcut: `button { label = "Save" }` renders a label
                // child so the simplest button only takes one line. If
                // the button also has child nodes, those render after
                // the shortcut label.
                let label = resolve_assign_value(node, "label", ctx).map(|v| v.to_display_string());
                let icon = resolve_assign_value(node, "icon", ctx).map(|v| v.to_display_string()).unwrap_or_default();
                let icon = icon.trim();
                // The shortcut's content: centered on the button (alone
                // on it), the label fitted to the button's width.
                let content = match label {
                    // `icon` and `label`: side by side, icon first.
                    Some(label) if !icon.is_empty() && !label.is_empty() => {
                        let row = nodes::made(&obj, "row", 0, |p| p.new_plain_row(6), |_| {});
                        if let Some(icon) = new_icon(&row, icon, None, ctx) {
                            tint_icon(&icon);
                        }
                        let text = nodes::made(&row, "label", 0, |p| p.new_label(&label), |o| o.set_label_text(&label));
                        obj.fit_shortcut_label(&text);
                        Some(row)
                    }
                    Some(label) if icon.is_empty() => {
                        let text = nodes::made(&obj, "label", 0, |p| p.new_label(&label), |o| o.set_label_text(&label));
                        obj.fit_shortcut_label(&text);
                        Some(text)
                    }
                    _ => new_icon(&obj, icon, None, ctx).map(|icon| {
                        tint_icon(&icon);
                        icon.0
                    }),
                };
                let has_children = node.items.iter().any(|it| matches!(it, Item::Block(b) if !is_event_handler_block(&b.kind.text)) || matches!(it, Item::Repeat(_)));
                if let Some(content) = content.filter(|_| !has_children) {
                    content.center();
                }
                render_item_list(&node.items, doc, ctx, slot, sink, &obj);
            }
            // Where its call stands, over the content (ADR 0024).
            show_call_state(&obj, tap_call.as_ref(), !call_taps);
            // Its label fitted again (a component's root: after its
            // call's keys, which may make it busy).
            if nodes::mode() == Mode::Update && !WIDGET_IS_ROOT.with(|r| r.get()) {
                obj.refit_shortcut();
            }
        }
        "icon" => {
            let name = resolve_text(node, "name", ctx).unwrap_or_default();
            let size = resolve_num(node, "size", ctx)
                .filter(|n| n.is_finite())
                .map(|n| n.clamp(1.0, f64::from(paneld_config::icons::MAX_SIZE)) as u16);
            let Some(icon) = new_icon(parent, name.trim(), size, ctx) else { return };
            apply_props(node, ctx, sink, &icon.0);
            tint_icon(&icon);
        }
        "rect" => {
            let obj = nodes::made(parent, "container", 0, |p| p.new_container(), |_| {});
            obj.set_enabled(false);
            obj.set_size(Size::Content, Size::Content);
            // A shape: no inner padding, which on a small one would
            // leave no room and show scrollbars.
            obj.set_padding(0);
            apply_props(node, ctx, sink, &obj);
        }
        "bar" => {
            let obj = nodes::made(parent, "bar", 0, |p| p.new_bar(), |_| {});
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
            match resolve_num(node, "value", ctx) {
                Some(v) => {
                    let to = (v as i32).clamp(min.min(max), max.max(min));
                    match crate::motion::gauge(node, to) {
                        Some((from, ms)) => obj.animate_bar(from.clamp(min.min(max), max.max(min)), to, ms),
                        None => obj.set_bar_value(to),
                    }
                }
                // No value: as a new bar is (0, in its range).
                None if nodes::mode() == Mode::Update => obj.set_bar_value(0.clamp(min.min(max), max.max(min))),
                None => {}
            }
            apply_props(node, ctx, sink, &obj);
        }
        "arc" => {
            let obj = nodes::made(parent, "arc", 0, |p| p.new_arc(), |_| {});
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
            match resolve_num(node, "value", ctx) {
                Some(v) => {
                    let to = (v as i32).clamp(min.min(max), max.max(min));
                    match crate::motion::gauge(node, to) {
                        Some((from, ms)) => obj.animate_arc(from.clamp(min.min(max), max.max(min)), to, ms),
                        None => obj.set_arc_value(to),
                    }
                }
                None if nodes::mode() == Mode::Update => obj.unset_arc_value(),
                None => {}
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
            let obj = nodes::made(parent, "line", 0, |p| p.new_line(&points), |o| o.set_line_points(&points));
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
            let fit_sig = fit as u64;
            let Some(obj) = nodes::made_opt(parent, "image", fit_sig, |p| p.new_image(src, fit), |o| {
                o.set_image_path(src);
            }) else {
                return;
            };
            if let Some((rgb, opa)) = resolve_text(node, "recolor", ctx).and_then(|c| color_of(&c, ctx)) {
                obj.set_image_recolor(rgb, opa);
            }
            apply_props(node, ctx, sink, &obj);
        }
        "chart" => {
            let obj = nodes::made(parent, "chart", 0, |p| p.new_chart(), |o| o.chart_clear_series());
            // Schema 8.3: no white box; division lines in a faint
            // `muted`. `background` / `border` (applied below) win.
            let muted = color_of("muted", ctx).unwrap_or((0x757575, 255)).0;
            obj.set_chart_look((muted, FAINT));
            render_chart(node, ctx, &obj);
            apply_props(node, ctx, sink, &obj);
        }
        "spinner" => {
            let obj = nodes::made(parent, "spinner", 0, |p| p.new_spinner(), |_| {});
            apply_props(node, ctx, sink, &obj);
        }
        "qr" => {
            let size = find_simple(node, "size")
                .and_then(|v| v.trim().parse::<i32>().ok())
                .unwrap_or(128);
            let obj = nodes::made(parent, "qr", size as u64, |p| p.new_qrcode(size), |_| {});
            if let Some(data) = resolve_assign_value(node, "data", ctx) {
                obj.set_qrcode_data(&data.to_display_string());
            }
            apply_props(node, ctx, sink, &obj);
        }
        "toggle" => {
            let obj = nodes::made(parent, "toggle", 0, |p| p.new_toggle(), |_| {});
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
            let obj = nodes::made(parent, "slider", 0, |p| p.new_slider(), |_| {});
            let min = resolve_num(node, "min", ctx).unwrap_or(0.0) as i32;
            let max = resolve_num(node, "max", ctx).unwrap_or(100.0) as i32;
            obj.set_slider_range(min, max);
            // No value: where a new slider is, unless the user moved it.
            let updating = nodes::mode() == Mode::Update && !obj.user_changed();
            match resolve_num(node, "value", ctx) {
                Some(v) => obj.set_slider_value(v as i32),
                None if updating => obj.set_slider_value(0),
                None => {}
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
            let ta = nodes::made(parent, "textarea", sig_of(&(masked, length)), |p| p.new_textarea(masked, length), |_| {});
            ta.register_input();
            let kb = nodes::made(parent, "keyboard", 0, |p| p.new_number_keyboard(), |_| {});
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
                    let opts = options.join("\n");
                    let obj = nodes::made(parent, "roller", 0, |p| p.new_roller(&opts, 3), |o| o.set_roller_options(&opts));
                    match selected {
                        Some(i) => obj.set_roller_selected(i as u32),
                        // As a new one, unless the user turned it.
                        None if nodes::mode() == Mode::Update && !obj.user_changed() => obj.set_roller_selected(0),
                        None => {}
                    }
                    (obj, ValueWidget::Roller)
                }
                Some("buttons") => {
                    // Other options or choice: built again (phase 2).
                    let sig = sig_of(&(&options, selected));
                    (nodes::made(parent, "buttons", sig, |p| p.new_button_row(&options, selected), |_| {}), ValueWidget::Buttons)
                }
                _ => {
                    let obj = nodes::made(parent, "dropdown", 0, |p| p.new_dropdown(), |_| {});
                    let updating = nodes::mode() == Mode::Update;
                    if !options.is_empty() {
                        obj.set_dropdown_options(&options.join("\n"));
                    } else if updating {
                        // No options now: what a new dropdown lists.
                        obj.set_dropdown_options(crate::ui::DROPDOWN_DEFAULT);
                    }
                    match selected {
                        Some(i) => obj.set_dropdown_selected(i as u16),
                        None if updating && !obj.user_changed() => obj.set_dropdown_selected(0),
                        None => {}
                    }
                    (obj, ValueWidget::Dropdown)
                }
            };
            apply_props(node, ctx, sink, &obj);
            if let Some(assigns) = bind_change_action(node, "on_change", widget, ctx, sink, &obj) {
                obj.on_change_select_inline_set(widget, sink.clone(), assigns);
            }
        }
        other => {
            nodes::mark_broken("an unknown node");
            parent.new_label(&format!("[{other}: not implemented yet]"));
        }
    }
}

thread_local! {
    /// Icon names already reported as unknown: logged once each, up to
    /// [`UNKNOWN_ICONS_MAX`] (a name from an expression may change on
    /// every render).
    static UNKNOWN_ICONS: std::cell::RefCell<std::collections::HashSet<String>> =
        std::cell::RefCell::new(std::collections::HashSet::new());
    /// Font values already reported as unusable (same bound).
    static BAD_FONTS: std::cell::RefCell<std::collections::HashSet<String>> =
        std::cell::RefCell::new(std::collections::HashSet::new());
}

const UNKNOWN_ICONS_MAX: usize = 32;

/// Draw icon `name` into `parent` at `size` px (default: the theme's
/// `font` size): the theme's `icons/<name>.png` when its `icons`
/// token is a folder (relative to the config folder; a theme folder's
/// is `themes/<name>/...`, ADR 0021) holding that file, else the
/// built-in glyph. An unknown name draws nothing and is
/// logged once; an empty one draws nothing.
/// Returns the object and whether it is an image.
fn new_icon<'ui>(parent: &Obj<'ui>, name: &str, size: Option<u16>, ctx: &Scope<'_>) -> Option<(Obj<'ui>, bool)> {
    if name.is_empty() {
        return None;
    }
    let theme = match ctx.get("theme") {
        Some(Value::Map(t)) => Some(t),
        _ => None,
    };
    let size = size.unwrap_or_else(|| theme_font_size(theme).unwrap_or(16));
    let folder = theme.and_then(|t| t.get("icons")).map(|v| v.to_display_string()).unwrap_or_default();
    let folder = folder.trim().trim_end_matches('/');
    if !folder.is_empty() && folder != "builtin" && paneld_config::icons::is_file_name(name) {
        let path = format!("{folder}/{name}.png");
        if crate::ui::asset_exists(&path) {
            let size = size.min(paneld_config::icons::MAX_SIZE);
            let obj = nodes::made_opt(parent, "icon-image", sig_of(&(&path, size)), |p| p.new_icon_image(&path, size), |_| {})?;
            return Some((obj, true));
        }
    }
    if let Some(glyph) = paneld_config::icons::glyph(name) {
        // Another glyph of the same size is its text.
        let obj = nodes::made(parent, "icon-glyph", u64::from(size), |p| p.new_icon_glyph(glyph, size), |o| {
            o.set_label_text(&glyph.to_string())
        });
        return Some((obj, false));
    }
    UNKNOWN_ICONS.with(|u| {
        for line in note_unknown_icon(&mut u.borrow_mut(), name) {
            eprintln!("paneld: {line}");
        }
    });
    None
}

/// The log lines for an unknown icon `name`: one the first time, none
/// after; past [`UNKNOWN_ICONS_MAX`] names none at all (said once), so
/// the set stays small.
fn note_unknown_icon(seen: &mut std::collections::HashSet<String>, name: &str) -> Vec<String> {
    if seen.len() >= UNKNOWN_ICONS_MAX || !seen.insert(name.to_string()) {
        return Vec::new();
    }
    let mut lines = vec![format!("icon `{name}`: not a built-in icon and not in the theme's icons folder; not shown")];
    if seen.len() == UNKNOWN_ICONS_MAX {
        lines.push(format!("{UNKNOWN_ICONS_MAX} unknown icon names reported; further ones are not"));
    }
    lines
}

/// An icon image (a theme's PNG) takes the text color, as a glyph
/// does: its own `color`, else the inherited one. A glyph is left as is.
fn tint_icon((obj, image): &(Obj<'_>, bool)) {
    if *image {
        let (rgb, opa) = obj.text_color();
        obj.set_image_recolor(rgb, 255);
        obj.set_image_opa(opa);
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
/// omitted args (each reading the params before it), bind them as top-level identifiers in a fresh
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
    outer_ctx: &Scope<'_>,
    sink: &Sender<UiEvent>,
    parent: &Obj<'_>,
) {
    let mode = nodes::mode();
    let is_root = ROOT_PENDING.with(|r| r.replace(false));
    let forced = FORCE.with(|f| f.replace(false));
    let block = call as *const Block as usize;
    // The call's record, and whether its own keys (which act on the
    // component's root) read something else now.
    let (rec, force_root) = match mode {
        Mode::Update => match nodes::visit(Kind::Call, block, forced) {
            None => return,
            Some(Visit::Skip) => {
                if is_root {
                    ROOT_REDONE.with(|r| r.set(false));
                }
                return;
            }
            Some(Visit::Look(i)) => {
                let own = nodes::own_hit(i);
                let changed = (own || forced) && nodes::fp_changed(i, call_fp(call, comp, outer_ctx));
                (Some(i), forced || (own && (changed || nodes::call_hit(i))))
            }
        },
        Mode::Build => {
            let mut own = Vec::new();
            call_deps(call, comp, &mut own);
            (Some(nodes::open(Kind::Call, block, own, &call.kind.text, call.kind.span)), false)
        }
        Mode::Off | Mode::Scratch => (None, false),
    };
    let outer_frame = nodes::current_frame();
    let mut locals = Context::new();
    // An `ago()` in an arg is timed with the call: its readers only
    // see the text.
    let ago_saved = crate::expr::take_ago_next();
    // Build: each param's deps, bound in turn (a default reads the
    // params before it).
    let mut frames = Vec::new();
    for item in &comp.items {
        let Item::Param(p) = item else { continue };
        let mut deps = Vec::new();
        let bound = if let Some((raw, span)) = find_raw(call, &p.name.text) {
            if mode == Mode::Build {
                let _outer = nodes::switch_frame(outer_frame);
                raw_deps(raw, span, &mut deps);
            }
            resolve_expr_value(raw, span, outer_ctx).unwrap_or(Value::Null)
        } else if let Some(default) = &p.default {
            if mode == Mode::Build {
                raw_deps(&default.text, default.span, &mut deps);
            }
            // Schema 6: a default reads the params declared before it,
            // bound so far; `paneld check` refuses a later one, itself
            // and the caller's names.
            resolve_expr_value(&default.text, default.span, &outer_ctx.with(&locals)).unwrap_or(Value::Null)
        } else {
            Value::Null
        };
        if mode == Mode::Build {
            deps.sort();
            deps.dedup();
            frames.push(nodes::push_frame(vec![(p.name.text.clone(), deps)]));
        }
        locals.insert(p.name.text.clone(), typed_param(&p.ty.text, bound));
    }
    let ago_due = crate::expr::take_ago_next();
    if let (Mode::Build | Mode::Update, Some(i)) = (mode, rec) {
        nodes::set_ago(i, ago_due);
    }
    crate::expr::restore_ago_next(min_opt(ago_saved, ago_due));
    let inner_ctx = outer_ctx.with(&locals);
    // Children of the call that aren't args become the slot's
    // content. The schema check already enforced "exactly one slot"
    // and "no children unless a slot exists". `render_item_list` takes
    // only the UI nodes and `repeat`s of them: the args and handler
    // blocks (`on_hold vm_stop { ... }`, which belong to the call) are
    // skipped there.
    let slot = SlotCtx { children: &call.items, outer_ctx, frame: outer_frame };
    let before = parent.child_count();
    // The call's `on_tap` wins over its root button's `action`.
    ROOT_TAP.with(|t| t.set(has_handler(call, "on_tap")));
    ROOT_PENDING.with(|r| r.set(true));
    ROOT_REDONE.with(|r| r.set(false));
    FORCE.with(|f| f.set(force_root));
    crate::motion::push_scope((call.kind.span.file, call.kind.span.start));
    render_item_list(&comp.items, doc, &inner_ctx, Some(&slot), sink, parent);
    crate::motion::pop_scope();
    ROOT_TAP.with(|t| t.set(false));
    ROOT_PENDING.with(|r| r.set(false));
    FORCE.with(|f| f.set(false));
    while let Some(f) = frames.pop() {
        drop(f);
    }
    let redone = ROOT_REDONE.with(|r| r.replace(false));
    // The call's own `on_tap` / `on_hold` act on the component's root
    // object, with args evaluated in the caller's scope (`item`, ...).
    // So do its common properties (schema 7), over the component's
    // own: everything on the call that is not one of its params. When
    // updating, only on a root set up again (it lost them).
    let root = match mode {
        Mode::Update => redone.then(|| ROOT_OBJ.with(|r| r.get())).filter(|r| !r.is_null()).map(Obj::from_raw),
        _ => parent.child(before),
    };
    if let Some(root) = root {
        let tap_call = bind_handlers(call, outer_ctx, sink, &root, false);
        let root_kind = comp.items.iter().find_map(|it| match it {
            Item::Block(b) => Some(b.kind.text.as_str()),
            _ => None,
        });
        if root_kind == Some("button") {
            show_call_state(&root, tap_call.as_ref(), false);
        }
        let params: Vec<&str> = comp
            .items
            .iter()
            .filter_map(|it| match it {
                Item::Param(p) => Some(p.name.text.as_str()),
                _ => None,
            })
            .collect();
        let props: Vec<Item> = crate::perf::time(crate::perf::Slot::Clone, || {
            call.items
                .iter()
                .filter(|it| match it {
                    Item::Assign(a) => match &a.target {
                        AssignTarget::Simple { key } => !params.contains(&key.text.as_str()),
                        _ => false,
                    },
                    _ => false,
                })
                .cloned()
                .collect()
        });
        let root_kind = root_kind.unwrap_or("");
        // The root already has its `*` and kind styles; the call adds
        // its classes only, so they do not undo the root's own props.
        apply_styles(root_kind, &props, false, outer_ctx, &root);
        apply_plain_props(root_kind, &props, outer_ctx, &root);
        if root_kind == "text" {
            root.fit_dots();
        }
        if root_kind == "button" && mode == Mode::Update {
            root.refit_shortcut();
        }
    }
    if is_root {
        ROOT_REDONE.with(|r| r.set(redone || mode != Mode::Update));
    }
    match (mode, rec) {
        (Mode::Build, Some(i)) => nodes::close(i),
        (Mode::Update, Some(i)) => nodes::leave(i),
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// What a node reads (ADR 0024): its dep keys, recorded when it is
// built, and a hash of what its inputs evaluate to now, so an update
// sets up again only a node whose inputs give something else.
// ---------------------------------------------------------------------------

/// The deps of raw value `raw` at `span` where the walk is.
fn raw_deps(raw: &str, span: paneld_config::source::Span, out: &mut Vec<Dep>) {
    match classify(raw) {
        ValueShape::Literal(_) => {}
        ValueShape::Expression(body) => {
            let p = parse_fragment_cached(body, span.file, span.start + 2);
            if let Some(e) = p.0.as_ref() {
                nodes::expr_deps(parse_key(&p), e, out);
            }
        }
        ValueShape::Interp(_) => {
            let p = parse_interp_cached(raw, span.file, span.start);
            for (i, e) in p.0.iter().enumerate() {
                nodes::expr_deps(parse_key(&p).wrapping_add(i), e, out);
            }
        }
    }
}

/// An action name as written (`on_tap = x`, `action = x`): whose call
/// state a button shows.
fn name_dep(raw: &str) -> Dep {
    match classify(raw) {
        ValueShape::Literal(t) => Dep::Call(Some(t.trim().to_string())),
        _ => Dep::Call(None),
    }
}

/// The deps of `items` (a node's, a style body's, a call's own keys):
/// every value, and in handler blocks (and a chart's `series`) theirs.
fn items_deps(items: &[Item], skip: &[&str], out: &mut Vec<Dep>) {
    let mut handlers = false;
    for item in items {
        match item {
            Item::Assign(a) => {
                let AssignTarget::Simple { key } = &a.target else { continue };
                if skip.contains(&key.text.as_str()) {
                    continue;
                }
                raw_deps(&a.value.text, a.value.span, out);
                if matches!(key.text.as_str(), "on_tap" | "on_hold" | "on_change" | "on_submit" | "action") {
                    out.push(name_dep(&a.value.text));
                    handlers = true;
                }
            }
            Item::Block(b) if is_event_handler_block(&b.kind.text) => {
                handlers = true;
                match &b.name {
                    Some(n) => out.push(Dep::Call(Some(n.text.clone()))),
                    None if b.is_inline_action() => out.push(Dep::Call(Some(b.inline_action_name()))),
                    None => {}
                }
                items_deps(&b.items, &[], out);
            }
            Item::Block(b) if matches!(b.kind.text.as_str(), "series" | "set") => items_deps(&b.items, &[], out),
            _ => {}
        }
    }
    // A handler's binding carries the names where it is drawn.
    if handlers {
        nodes::union(out, &nodes::all_local_deps());
    }
}

/// The theme style bodies that apply to a node of `kind` with `items`
/// (as [`apply_styles`] picks them), each with the kind it is applied as.
fn style_bodies(kind: &str, items: &[Item], by_kind: bool, f: &mut dyn FnMut(&[Item])) {
    let classes = class_names(items);
    STYLES.with(|styles| {
        let styles = styles.borrow();
        if styles.is_empty() {
            return;
        }
        let selectors: Vec<String> = ["*".to_string(), kind.to_string()]
            .into_iter()
            .filter(|_| by_kind)
            .chain(classes.iter().map(|c| format!(".{c}")))
            .collect();
        for sel in &selectors {
            if let Some(body) = styles.get(sel) {
                f(body);
            }
            for state in ["pressed", "checked", "disabled", "focused", "hover", "running", "failed"] {
                if let Some(body) = styles.get(&format!("{sel}:{state}")) {
                    f(body);
                }
            }
        }
    });
}

/// What widget `node` itself reads (not its children): its keys, its
/// handlers, its theme styles.
fn node_deps(node: &Block, _ctx: &Scope<'_>, out: &mut Vec<Dep>) {
    items_deps(&node.items, &[], out);
    style_bodies(&node.kind.text, &node.items, true, &mut |body| items_deps(body, &[], out));
    out.sort();
    out.dedup();
}

/// The params of component `comp`.
fn param_names(comp: &Block) -> Vec<&str> {
    comp.items
        .iter()
        .filter_map(|it| match it {
            Item::Param(p) => Some(p.name.text.as_str()),
            _ => None,
        })
        .collect()
}

/// What a component call's own keys read (they act on its root):
/// everything on it that is not a param, its handlers, its classes'
/// styles.
fn call_deps(call: &Block, comp: &Block, out: &mut Vec<Dep>) {
    let params = param_names(comp);
    items_deps(&call.items, &params, out);
    style_bodies("", &call.items, false, &mut |body| items_deps(body, &[], out));
    out.sort();
    out.dedup();
}

/// `page` reads the page's `title`, `nav` every nav page's.
fn page_frame(doc: &Document, page: &Block) -> Vec<(String, Vec<Dep>)> {
    let title = |b: &Block, out: &mut Vec<Dep>| {
        if let Some((raw, span)) = find_raw(b, "title") {
            raw_deps(raw, span, out);
        }
    };
    let mut page_deps = Vec::new();
    title(page, &mut page_deps);
    let mut nav = Vec::new();
    for item in &doc.items {
        if let Item::Block(b) = item {
            if b.kind.text == "page" && find_simple(b, "nav").is_some() {
                title(b, &mut nav);
            }
        }
    }
    for d in [&mut page_deps, &mut nav] {
        d.sort();
        d.dedup();
    }
    vec![("page".into(), page_deps), ("nav".into(), nav)]
}

/// Hash `v` (numbers by their bits, maps in key order).
fn hash_value(v: &Value, h: &mut impl std::hash::Hasher) {
    use std::hash::Hash;
    match v {
        Value::Null => 0u8.hash(h),
        Value::Bool(b) => (1u8, b).hash(h),
        Value::Int(n) => (2u8, n).hash(h),
        Value::Double(d) => (3u8, d.to_bits()).hash(h),
        Value::String(s) => (4u8, s).hash(h),
        Value::List(l) => {
            (5u8, l.len()).hash(h);
            l.iter().for_each(|x| hash_value(x, h));
        }
        Value::Map(m) => {
            (6u8, m.len()).hash(h);
            for (k, x) in m {
                k.hash(h);
                hash_value(x, h);
            }
        }
    }
}

/// What every value of `items` (and of their handler, `series` and
/// `set` blocks) evaluates to in `ctx`, into `h`.
fn hash_items(items: &[Item], skip: &[&str], ctx: &Scope<'_>, h: &mut impl std::hash::Hasher) {
    use std::hash::Hash;
    let mut handlers = false;
    for item in items {
        match item {
            Item::Assign(a) => {
                let AssignTarget::Simple { key } = &a.target else { continue };
                if skip.contains(&key.text.as_str()) || matches!(classify(&a.value.text), ValueShape::Literal(_)) {
                    continue;
                }
                key.text.hash(h);
                match resolve_expr_value(&a.value.text, a.value.span, ctx) {
                    Some(v) => hash_value(&v, h),
                    None => 0xffu8.hash(h),
                }
                handlers |= matches!(key.text.as_str(), "on_tap" | "on_hold" | "on_change" | "on_submit" | "action");
            }
            Item::Block(b) if is_event_handler_block(&b.kind.text) => {
                handlers = true;
                hash_items(&b.items, &[], ctx, h);
            }
            Item::Block(b) if matches!(b.kind.text.as_str(), "series" | "set") => hash_items(&b.items, &[], ctx, h),
            _ => {}
        }
    }
    if handlers {
        for (k, v) in ctx.visible() {
            if !matches!(k, "sources" | "vars" | "alerts" | "theme" | "now") {
                k.hash(h);
                hash_value(v, h);
            }
        }
    }
}

/// A hash of what widget `node`'s own inputs evaluate to now (an
/// icon's: and the text color it takes from `parent`, which a theme
/// PNG is tinted with when drawn).
fn node_fp(node: &Block, ctx: &Scope<'_>, parent: &Obj<'_>) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    if node.kind.text == "icon" {
        parent.text_color().hash(&mut h);
    }
    hash_items(&node.items, &[], ctx, &mut h);
    style_bodies(&node.kind.text, &node.items, true, &mut |body| hash_items(body, &[], ctx, &mut h));
    h.finish()
}

/// The same for a component call's own keys, in the caller's scope.
fn call_fp(call: &Block, comp: &Block, outer: &Scope<'_>) -> u64 {
    use std::hash::Hasher;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    let params = param_names(comp);
    hash_items(&call.items, &params, outer, &mut h);
    style_bodies("", &call.items, false, &mut |body| hash_items(body, &[], outer, &mut h));
    h.finish()
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
    let ctx = &Scope::root(ctx);
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
    resolve_expr_value(raw, span, &Scope::root(ctx))
}

/// [`resolve_value`] in a [`Scope`]: names bound over the context
/// without copying it (an action's params before the one defaulted).
pub fn resolve_value_in(raw: &str, span: paneld_config::source::Span, ctx: &Scope<'_>) -> Option<Value> {
    resolve_expr_value(raw, span, ctx)
}

/// Evaluate a `key = <raw>` as either a literal string (for a
/// non-expression value) or a CEL expression resolved against `ctx`,
/// returning a [`Value`]. Interpolated-string values become a
/// [`Value::String`].
fn resolve_expr_value(
    raw: &str,
    span: paneld_config::source::Span,
    ctx: &Scope<'_>,
) -> Option<Value> {
    match classify(raw) {
        // `"..."` keeps spaces and `;` (config-format, "Lines"); the
        // quotes themselves are not part of the value.
        ValueShape::Literal(text) => Some(Value::String(paneld_config::value::parse_string(text))),
        ValueShape::Expression(body) => {
            let parsed = parse_fragment_cached(body, span.file, span.start + 2);
            let (expr, diags) = (&parsed.0, &parsed.1);
            if !diags.is_empty() {
                return None;
            }
            expr.as_ref().and_then(|e| eval_in(e, ctx).ok())
        }
        ValueShape::Interp(_) => {
            let parsed = parse_interp_cached(raw, span.file, span.start);
            let (fragments, diags) = (&parsed.0, &parsed.1);
            if !diags.is_empty() {
                return None;
            }
            Some(Value::String(splice(raw, fragments, ctx, false)))
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
fn resolve_text(node: &Block, key: &str, ctx: &Scope<'_>) -> Option<String> {
    let (raw, span) = find_raw(node, key)?;
    Some(match classify(raw) {
        ValueShape::Literal(text) => paneld_config::value::parse_string(text),
        ValueShape::Expression(body) => {
            let parsed = parse_fragment_cached(body, span.file, span.start + 2);
            let (expr, diags) = (&parsed.0, &parsed.1);
            if !diags.is_empty() {
                return Some(format!("[parse error: {}]", diags[0].message));
            }
            match expr.as_ref().and_then(|e| eval_in(e, ctx).ok()) {
                Some(v) => v.to_display_string(),
                None => "[eval error]".to_string(),
            }
        }
        ValueShape::Interp(_) => {
            let parsed = parse_interp_cached(raw, span.file, span.start);
            let (fragments, diags) = (&parsed.0, &parsed.1);
            if !diags.is_empty() {
                return Some(format!("[parse error: {}]", diags[0].message));
            }
            interpolate_text(raw, fragments, ctx)
        }
    })
}

/// Walk `raw` left to right, replacing each `{{ ... }}` with the
/// evaluated fragment's display string and leaving literal text
/// (including `\{{` escapes handled by the lexer) intact.
///
/// `fragments` is the ordered list of parsed CEL ASTs; we match them
/// up to the openers encountered in `raw`.
fn interpolate_text(raw: &str, fragments: &[cel::Expr], ctx: &Scope<'_>) -> String {
    splice(raw, fragments, ctx, true)
}

/// Put the values of `fragments` (parsed from `raw` by
/// [`cel::parse_interp`]) in place of their `{{ ... }}` in `raw`.
/// Mirrors the lexer's `\\` and `\{{` escapes. A failed fragment
/// shows as `[message]` with `show_errors`, else as nothing.
fn splice(raw: &str, fragments: &[cel::Expr], ctx: &Scope<'_>, show_errors: bool) -> String {
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
                match eval_in(expr, ctx) {
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
fn apply_props(node: &Block, ctx: &Scope<'_>, sink: &Sender<UiEvent>, obj: &Obj<'_>) -> Option<(String, Vec<ActionArg>)> {
    // Inline-action block form, with or without a name:
    //   `on_tap { goto = settings }`
    //   `on_tap { set { k = v; ... } }`
    //   `on_tap vm_stop { vm = {{ item.id }} }`
    // Only one gets wired per tap: the first form found wins,
    // matching schema semantics (one action per handler).
    crate::perf::time(crate::perf::Slot::Props, || {
        let call_taps = NODE_TAP.with(|t| t.replace(false));
        let tap_call = bind_handlers(node, ctx, sink, obj, call_taps);
        apply_styles(&node.kind.text, &node.items, true, ctx, obj);
        apply_plain_props(&node.kind.text, &node.items, ctx, obj);
        apply_motion(node, ctx, obj);
        tap_call
    })
}

/// Theme motion on a node (ADR 0023): a press shown over
/// `tap_transition` on what takes taps, and `enter` when the node
/// appears on the page shown.
fn apply_motion(node: &Block, ctx: &Scope<'_>, obj: &Obj<'_>) {
    let settings = crate::motion::settings();
    let tappable = node.kind.text == "button" || has_handler(node, "on_tap") || has_handler(node, "on_hold");
    if tappable && settings.tap_ms() > 0 {
        obj.set_press_transition(settings.tap_ms());
    }
    if let Some((raw, span)) = find_raw(node, "enter") {
        let text = resolve_scalar(raw, span, ctx);
        match paneld_config::motion::parse_transition(text.trim()) {
            Ok(t) => {
                if let Some((slide, from, ms)) = crate::motion::enter(node, t, obj.is_hidden()) {
                    obj.animate_enter(slide, from, ms);
                }
            }
            Err(e) => note_bad_value(format!("enter: {e}")),
        }
    }
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
    /// A `:failed` button's default border color: the theme's
    /// `critical`, resolved once per page.
    static FAILED_BORDER: std::cell::Cell<(u32, u8)> = const { std::cell::Cell::new((0xf85149, 255)) };
    /// The component call being expanded has its own `on_tap`, which
    /// wins over its root button's `action`: taken by the root.
    static ROOT_TAP: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// That, for the widget being drawn (its `apply_props`).
    static NODE_TAP: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Install the action calls running or failed now, before drawing a
/// page: its buttons show them (`:running`, `:failed`).
pub fn set_call_states(states: Vec<CallState>) {
    CALL_STATES.with(|s| *s.borrow_mut() = states);
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
/// text or a spinner over its content (ADR 0024: the content stays,
/// hidden). `own`: the button's own tap (else its component call's,
/// which only ever adds one).
fn show_call_state(obj: &Obj<'_>, tap_call: Option<&(String, Vec<ActionArg>)>, own: bool) {
    let Some((name, args)) = tap_call else {
        if own {
            obj.show_busy(None);
        }
        return;
    };
    let call = call_state(name, args);
    let state = call.as_ref().map(|c| c.state);
    obj.set_run_state(state == Some(RunState::Running), state == Some(RunState::Failed));
    match call {
        Some(CallState { state: RunState::Running, busy, .. }) => obj.show_busy(Some(busy.filter(|t| !t.trim().is_empty()))),
        _ => obj.show_busy(None),
    }
}

/// Install the active theme's styles: selector (`*`, `button`,
/// `.card`) to its `key = value` items, parents merged in already.
/// Applies from the next page drawn.
pub fn set_styles(styles: std::collections::BTreeMap<String, Vec<Item>>) {
    STYLES.with(|s| *s.borrow_mut() = styles);
    // A (re)loaded config: its expressions are parsed afresh.
    PARSED.with(|p| p.borrow_mut().clear());
    PARSED_INTERP.with(|p| p.borrow_mut().clear());
    nodes::clear_reads();
    // Other styles: every page is built afresh.
    nodes::forget();
}

type ParseKey = (String, paneld_config::source::FileId, u32);
type Parsed = std::rc::Rc<(Option<cel::Expr>, Vec<paneld_config::parse::Diagnostic>)>;
type ParsedInterp = std::rc::Rc<(Vec<cel::Expr>, Vec<paneld_config::parse::Diagnostic>)>;

thread_local! {
    /// Parsed `{{ }}` bodies by text and place: a page rebuild
    /// evaluates the same expressions again, it need not parse them.
    static PARSED: std::cell::RefCell<std::collections::HashMap<ParseKey, Parsed>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
    static PARSED_INTERP: std::cell::RefCell<std::collections::HashMap<ParseKey, ParsedInterp>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

fn parse_fragment_cached(
    body: &str,
    file: paneld_config::source::FileId,
    base: u32,
) -> Parsed {
    PARSED.with(|p| {
        let mut p = p.borrow_mut();
        let key = (body.to_string(), file, base);
        if let Some(hit) = p.get(&key) {
            return hit.clone();
        }
        use crate::perf::{time, Slot};
        let parsed = time(Slot::Parse, || cel::parse_fragment(body, file, base));
        let parsed = std::rc::Rc::new(parsed);
        p.insert(key, parsed.clone());
        parsed
    })
}

fn parse_interp_cached(
    raw: &str,
    file: paneld_config::source::FileId,
    base: u32,
) -> ParsedInterp {
    PARSED_INTERP.with(|p| {
        let mut p = p.borrow_mut();
        let key = (raw.to_string(), file, base);
        if let Some(hit) = p.get(&key) {
            return hit.clone();
        }
        use crate::perf::{time, Slot};
        let parsed = time(Slot::Parse, || cel::parse_interp(raw, file, base));
        let parsed = std::rc::Rc::new(parsed);
        p.insert(key, parsed.clone());
        parsed
    })
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
fn apply_styles(kind: &str, items: &[Item], by_kind: bool, ctx: &Scope<'_>, obj: &Obj<'_>) {
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
                let kind = if sel == "*" && matches!(kind, "arc" | "bar" | "toggle" | "slider") { "" } else { kind };
                apply_plain_props(kind, body, ctx, obj);
            }
        }
        // Then the states (`button:pressed`, `.card:checked`): the
        // same properties, for when the node is in that state.
        for sel in &selectors {
            for state in ["pressed", "checked", "disabled", "focused", "hover", "running", "failed"] {
                if let Some(body) = styles.get(&format!("{sel}:{state}")) {
                    // On a select, the part that shows the state.
                    if let Some((target, selector)) = obj.state_target(state) {
                        crate::ui::with_selector(selector, || apply_plain_props(kind, body, ctx, &target));
                    }
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
/// `call_taps`: `node` is a component's root whose call has an `on_tap`.
fn bind_handlers(node: &Block, ctx: &Scope<'_>, sink: &Sender<UiEvent>, obj: &Obj<'_>, call_taps: bool) -> Option<(String, Vec<ActionArg>)> {
    let mut tap_call = None;
    // A node with an `on_hold` gets short taps only for `on_tap`, so a
    // long press does not fire both.
    let has_hold = has_handler(node, "on_hold");
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
fn action_name(raw: &str, span: paneld_config::source::Span, ctx: &Scope<'_>) -> Option<String> {
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

fn apply_plain_props(kind: &str, items: &[Item], ctx: &Scope<'_>, obj: &Obj<'_>) {
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
            "track" if matches!(kind, "arc" | "bar" | "toggle" | "slider") => {
                if let Some((rgb, opa)) = resolve_color(value, a.value.span, ctx) {
                    if kind == "arc" {
                        obj.set_arc_track(rgb, opa);
                    } else {
                        // A bar's, toggle's and slider's track is its box.
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
                let text = resolve_scalar(value, a.value.span, ctx);
                apply_background(&text, ctx, obj);
                // A button's text reads on the color it was given, unless
                // the same body sets `color` too.
                let sets_color = items.iter().any(|i| matches!(i, Item::Assign(b) if matches!(&b.target, AssignTarget::Simple { key } if key.text == "color")));
                if kind == "button" && !sets_color {
                    if let Some((rgb, _)) = color_of(text.trim(), ctx).filter(|c| c.1 > 0x80) {
                        obj.set_text_color_hex(crate::ui::on_color(rgb), 255);
                    }
                }
            }
            "shadow" => apply_shadow(&resolve_scalar(value, a.value.span, ctx), ctx, obj),
            "color" => {
                if let Some((rgb, opa)) = resolve_color(value, a.value.span, ctx) {
                    // A gauge's foreground is its indicator.
                    match kind {
                        "arc" => obj.set_arc_indicator(rgb, opa),
                        "bar" => obj.set_bar_indicator(rgb, opa),
                        // A toggle's on color, a slider's value.
                        "toggle" => obj.set_toggle_on_color(rgb, opa),
                        "slider" => obj.set_slider_color(rgb, opa),
                        _ => obj.set_text_color_hex(rgb, opa),
                    }
                }
            }
            "font" => {
                let resolved = resolve_expr_value(value, a.value.span, ctx)
                    .map(|v| v.to_display_string())
                    .unwrap_or_else(|| value.to_string());
                apply_font(obj, &resolved, theme_of(ctx));
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

/// Draw a `background` value (schema 8.1, resolved text) on `obj`: a
/// color, `none`, a gradient or an image; a bare name is a theme
/// token holding one; an image replaces the color. A value that
/// does not parse (an expression's result: a literal is checked) is
/// logged once and leaves the node as it was.
fn apply_background(text: &str, ctx: &Scope<'_>, obj: &Obj<'_>) {
    use paneld_config::paint::{parse_background, Background};
    let mut text = text.trim().to_string();
    for _ in 0..8 {
        let parsed = match parse_background(&text) {
            Ok(b) => b,
            Err(e) => {
                note_bad_value(format!("background: {e}"));
                return;
            }
        };
        match parsed {
            Background::None => {
                obj.clear_background_paint();
                obj.set_background_hex(0, 0);
            }
            Background::Color(c) => {
                if let Some((rgb, opa)) = color_of(c, ctx) {
                    obj.clear_background_paint();
                    obj.set_background_hex(rgb, opa);
                } else if let Some(v) = theme_of(ctx).and_then(|t| t.get(c)) {
                    // A token holding a gradient or an image.
                    text = v.to_display_string().trim().to_string();
                    continue;
                } else {
                    note_bad_value(format!("background: `{c}` is not a color or a theme token"));
                }
            }
            Background::Gradient(g) => {
                let stops: Option<Vec<(u32, u8)>> = g.stops.iter().map(|c| color_of(c, ctx)).collect();
                match stops {
                    Some(stops) => obj.set_background_gradient(&g.kind, &stops),
                    None => note_bad_value(format!("background: a stop of `{text}` is not a color")),
                }
            }
            Background::Image { path, fit } => {
                if obj.set_background_image(path, fit) {
                    obj.set_background_hex(0, 0);
                }
            }
        }
        return;
    }
}

/// Draw a `shadow` value (schema 8.1): `none`, `<size> [<color>] [<x>
/// <y>]`, or a theme token's name holding one.
fn apply_shadow(text: &str, ctx: &Scope<'_>, obj: &Obj<'_>) {
    use paneld_config::paint::{parse_shadow, ShadowValue};
    let mut text = text.trim().to_string();
    for _ in 0..8 {
        match parse_shadow(&text) {
            Ok(ShadowValue::None) => obj.set_shadow(0, 0, 0, 0, 0),
            Ok(ShadowValue::Shadow(sh)) => match color_of(sh.color, ctx) {
                Some((rgb, opa)) => obj.set_shadow(sh.size, rgb, opa, sh.x, sh.y),
                None => note_bad_value(format!("shadow: `{}` is not a color", sh.color)),
            },
            Ok(ShadowValue::Token(name)) => match theme_of(ctx).and_then(|t| t.get(name)) {
                Some(v) => {
                    text = v.to_display_string().trim().to_string();
                    continue;
                }
                None => note_bad_value(format!("shadow: `{name}` is not a theme token")),
            },
            Err(e) => note_bad_value(format!("shadow: {e}")),
        }
        return;
    }
}

/// Log a value that cannot be drawn, once (bounded like icon names).
fn note_bad_value(msg: String) {
    BAD_FONTS.with(|b| {
        let mut b = b.borrow_mut();
        if b.len() < UNKNOWN_ICONS_MAX && b.insert(msg.clone()) {
            eprintln!("paneld: {msg}");
        }
    });
}

/// Resolve a `key = ...` on `node` as an f64. Handles literal
/// numbers and `{{ expr }}` evaluating to Int/Double. Returns None
/// if the key is absent or doesn't produce a number.
fn resolve_num(node: &Block, key: &str, ctx: &Scope<'_>) -> Option<f64> {
    let (raw, span) = find_raw(node, key)?;
    let v = resolve_expr_value(raw, span, ctx)?;
    match v {
        Value::Int(n) => Some(n as f64),
        Value::Double(d) => Some(d),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

type Theme = std::collections::BTreeMap<String, Value>;

fn theme_of<'a>(ctx: &Scope<'a>) -> Option<&'a Theme> {
    match ctx.get("theme") {
        Some(Value::Map(t)) => Some(t),
        _ => None,
    }
}

/// The font a `font` value means (ADR 0015): `<family> <size> [bold]`,
/// or a theme font token named bare (`font_big`).
fn resolve_font(text: &str, theme: Option<&Theme>) -> Result<paneld_config::fonts::Font, String> {
    let text = text.trim();
    if paneld_config::fonts::TOKENS.contains(&text) {
        let token = theme.and_then(|t| t.get(text)).map(|v| v.to_display_string()).unwrap_or_default();
        return paneld_config::fonts::parse(&token).map_err(|e| format!("theme token `{text}`: {e}"));
    }
    paneld_config::fonts::parse(text)
}

/// Set `obj`'s text font from a `font` value. One that does not parse
/// (an expression's result: a literal is checked) draws in the bitmap
/// Montserrat at the size it names, if any, and is logged once.
fn apply_font(obj: &Obj<'_>, text: &str, theme: Option<&Theme>) {
    match resolve_font(text, theme) {
        Ok(font) => crate::perf::time(crate::perf::Slot::Font, || obj.set_font(&font)),
        Err(e) => {
            BAD_FONTS.with(|b| {
                let mut b = b.borrow_mut();
                if b.len() < UNKNOWN_ICONS_MAX && b.insert(e.clone()) {
                    eprintln!("paneld: font: {e}; drawn in Montserrat");
                }
            });
            if let Some(n) = text.split_whitespace().find_map(|t| t.parse::<u16>().ok()) {
                obj.set_font_size(n);
            }
        }
    }
}

/// The size of the theme's `font` token, the default for an icon.
fn theme_font_size(theme: Option<&Theme>) -> Option<u16> {
    resolve_font("font", theme).ok().map(|f| f.size)
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
fn render_chart(node: &Block, ctx: &Scope<'_>, obj: &Obj<'_>) {
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
    let padded: Vec<Vec<Option<i32>>> = series
        .iter()
        .map(|(values, _)| {
            let tail = shown(values);
            let mut padded: Vec<Option<i32>> = vec![None; points - tail.len()];
            padded.extend(tail.iter().map(|v| v.map(scaled)));
            padded
        })
        .collect();
    // Value changes move (schema 8.3) from what the chart showed.
    let moving = crate::motion::chart(node, &padded);
    for (i, (_, color)) in series.iter().enumerate() {
        match &moving {
            Some((tween, _)) => obj.chart_add_series(color.0, color.1, &tween[i].0),
            None => obj.chart_add_series(color.0, color.1, &padded[i]),
        }
    }
    if let Some((tween, ms)) = moving {
        obj.animate_chart(tween, ms);
    }
}

fn resolve_assign_value(node: &Block, key: &str, ctx: &Scope<'_>) -> Option<Value> {
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
    ctx: &Scope<'_>,
    sink: &Sender<UiEvent>,
    obj: &Obj<'_>,
) -> Option<Vec<InlineSetItem>> {
    if let Some((raw, span)) = find_raw(node, handler) {
        if let Some(action) = action_name(raw, span, ctx) {
            obj.on_change_action(widget, sink.clone(), action, local_args(ctx), Vec::new());
            return None;
        }
    }
    for item in &node.items {
        let Item::Block(child) = item else { continue };
        if child.kind.text != handler {
            continue;
        }
        if let Some(name) = &child.name {
            // Args that read the new value wait for it (schema 8.4).
            let (args, late) = change_call_args(child, ctx);
            obj.on_change_action(widget, sink.clone(), name.text.clone(), args, late);
            return None;
        }
        if child.is_inline_action() {
            obj.on_change_action(widget, sink.clone(), child.inline_action_name(), local_args(ctx), Vec::new());
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
fn local_args(ctx: &Scope<'_>) -> Vec<ActionArg> {
    const GLOBAL: &[&str] = &["sources", "vars", "alerts", "theme", "now", "page", "nav"];
    ctx.visible()
        .into_iter()
        .filter(|(k, _)| !GLOBAL.contains(k))
        .map(|(k, v)| ActionArg { name: k.to_string(), value: v.clone() })
        .collect()
}

/// Evaluate every `arg = <raw>` in an action-call shortcut body
/// against `ctx`, returning the already-resolved [`ActionArg`]s. An
/// arg whose expression fails to evaluate is dropped so a bad arg
/// does not make the whole tap no-op.
fn collect_action_args(call: &Block, ctx: &Scope<'_>) -> Vec<ActionArg> {
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

/// The args of `on_change name { ... }`: those that read `value` or
/// `event` (the new value) left as text, with the names where the node
/// is drawn, for the main loop to evaluate when it fires; the rest
/// evaluated now, as for a tap.
fn change_call_args(call: &Block, ctx: &Scope<'_>) -> (Vec<ActionArg>, Vec<InlineSetItem>) {
    let mut now = Vec::new();
    let mut late = Vec::new();
    for item in &call.items {
        let Item::Assign(a) = item else { continue };
        let AssignTarget::Simple { key } = &a.target else { continue };
        let reads_event = paneld_config::reads::value_reads(&a.value).iter().any(|r| matches!(r.root.as_str(), "value" | "event"));
        if reads_event {
            late.push(InlineSetItem { key: key.text.clone(), value_raw: a.value.text.clone(), locals: drawn_locals(ctx) });
        } else if let Some(v) = resolve_expr_value(&a.value.text, a.value.span, ctx) {
            now.push(ActionArg { name: key.text.clone(), value: v });
        }
    }
    (now, late)
}

/// [`local_args`] plus `page` and `nav`: what an expression evaluated
/// later (an inline `set`, a late `on_change` arg) read where drawn.
/// Not action args themselves, which would change what counts as
/// the same call.
fn drawn_locals(ctx: &Scope<'_>) -> Vec<ActionArg> {
    let mut locals = local_args(ctx);
    for k in ["page", "nav"] {
        if let Some(v) = ctx.get(k) {
            locals.push(ActionArg { name: k.into(), value: v.clone() });
        }
    }
    locals
}

fn collect_inline_set(set_block: &Block, ctx: &Scope<'_>) -> Vec<InlineSetItem> {
    let locals = drawn_locals(ctx);
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
fn resolve_scalar(value: &str, span: paneld_config::source::Span, ctx: &Scope<'_>) -> String {
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
/// `track` when it has one, else its `muted`, faint).
fn gauge_colors(ctx: &Scope<'_>) -> ((u32, u8), (u32, u8)) {
    let accent = color_of("accent", ctx).unwrap_or((0x2196f3, 255));
    (accent, track_color(ctx))
}

/// The theme's `track` color, else its `muted`, faint.
fn track_color(ctx: &Scope<'_>) -> (u32, u8) {
    let theme = theme_of(ctx);
    theme
        .and_then(|t| t.get("track"))
        .and_then(|v| theme_color(&v.to_display_string(), theme))
        .unwrap_or_else(|| (color_of("muted", ctx).unwrap_or((0x757575, 255)).0, FAINT))
}

/// The inputs' and dialogs' colors (themes.md): `accent`, the track,
/// `text`, `surface`, and the text on the accent.
fn input_colors(ctx: &Scope<'_>) -> crate::ui::InputColors {
    let d = crate::ui::InputColors::default();
    let accent = color_of("accent", ctx).unwrap_or(d.accent);
    // `on_accent` when the theme sets it, else what reads best.
    let theme = theme_of(ctx);
    let on_accent = theme
        .and_then(|t| t.get("on_accent"))
        .and_then(|v| theme_color(&v.to_display_string(), theme))
        .unwrap_or((crate::ui::on_color(accent.0), 255));
    crate::ui::InputColors {
        accent,
        track: track_color(ctx),
        text: color_of("text", ctx).unwrap_or(d.text),
        surface: color_of("surface", ctx).unwrap_or(d.surface),
        on_accent,
    }
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
    ctx: &Scope<'_>,
) -> Option<(u32, u8)> {
    match classify(value) {
        ValueShape::Literal(text) => color_of(text, ctx),
        ValueShape::Expression(body) => {
            let parsed = parse_fragment_cached(body, span.file, span.start + 2);
            let (expr, diags) = (&parsed.0, &parsed.1);
            if !diags.is_empty() {
                return None;
            }
            let v = expr.as_ref().and_then(|e| eval_in(e, ctx).ok())?;
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
fn color_of(s: &str, ctx: &Scope<'_>) -> Option<(u32, u8)> {
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
    fn parsed_expressions_are_kept_until_a_reload() {
        let f = paneld_config::source::FileId::default();
        let globals = Context::new();
        let ctx = Scope::root(&globals);
        set_styles(Default::default());
        let a = parse_fragment_cached("1 + 1", f, 10);
        assert!(std::rc::Rc::ptr_eq(&a, &parse_fragment_cached("1 + 1", f, 10)), "a hit");
        // Same place, other text (an edited config): parsed afresh.
        let b = parse_fragment_cached("2 + 2", f, 10);
        assert_eq!(eval_in(b.0.as_ref().unwrap(), &ctx), Ok(Value::Int(4)));
        // A reload empties the cache.
        set_styles(Default::default());
        let again = parse_fragment_cached("1 + 1", f, 10);
        assert!(!std::rc::Rc::ptr_eq(&a, &again), "a miss after a reload");
        // A parse error is kept as it was found.
        assert!(!parse_fragment_cached("1 +", f, 0).1.is_empty());
        assert!(!parse_fragment_cached("1 +", f, 0).1.is_empty());
        let i = parse_interp_cached("t {{ 1 + 2 }} C", f, 0);
        assert!(std::rc::Rc::ptr_eq(&i, &parse_interp_cached("t {{ 1 + 2 }} C", f, 0)));
        assert_eq!(interpolate_text("t {{ 1 + 2 }} C", &i.0, &ctx), "t 3 C");
        let j = parse_interp_cached("t {{ 5 }} C", f, 0);
        assert_eq!(interpolate_text("t {{ 5 }} C", &j.0, &ctx), "t 5 C");
    }

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
    fn unknown_icon_names_are_logged_once_and_bounded() {
        let mut seen = std::collections::HashSet::new();
        assert_eq!(note_unknown_icon(&mut seen, "a").len(), 1);
        assert!(note_unknown_icon(&mut seen, "a").is_empty());
        let mut lines = 1;
        for i in 0..1000 {
            lines += note_unknown_icon(&mut seen, &format!("x{i}")).len();
        }
        assert_eq!(seen.len(), UNKNOWN_ICONS_MAX);
        // One line per name, plus the one saying no more are logged.
        assert_eq!(lines, UNKNOWN_ICONS_MAX + 1);
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
