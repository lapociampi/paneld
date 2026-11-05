//! Safe wrapper over `lvgl-sys`. All `unsafe` for the UI module lives
//! here. Only this crate links to `lvgl_sys`.
//!
//! LVGL is single-threaded by design (ADR 0010): one [`Ui`] on the
//! main thread owns the global LVGL state. Dropping it calls
//! `lv_deinit`.

use std::ffi::CString;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use lvgl_sys as sys;

/// A tap on a UI node bound to either a named top-level action or
/// a `goto = <page>` inline navigation. Sent to the consumer
/// (`main.rs`) by the LVGL click callback registered in
/// [`Obj::on_tap`] / [`Obj::on_tap_goto`]. The action worker also
/// sends [`UiEvent::VarsChanged`] after it applies a `set` block,
/// so the main loop knows to rebuild the context and re-render.
#[derive(Debug, Clone)]
pub enum UiEvent {
    /// `on_tap = action_name` or `on_tap action_name { arg = val }` --
    /// run that action's `run` command, with the already-evaluated
    /// `args` overlaid on the eval context as named bindings.
    Action { name: String, args: Vec<ActionArg> },
    /// `on_tap { goto = page_name }` -- switch to that page.
    Goto { page: String },
    /// `on_tap { set { k = v; ... } }` -- apply these writes to the
    /// shared vars map. Each item carries the key and the raw value
    /// text; the main loop evaluates each RHS against the current
    /// context, like a named action's `set` block.
    InlineSet { assigns: Vec<InlineSetItem> },
    /// `on_change { set { k = v; ... } }` on an input widget whose
    /// new value should be visible in the set-RHS expressions as the
    /// `event.value` identifier. Carries the live value snapshot
    /// (bool for a toggle, int for a slider, ...).
    ChangeInlineSet {
        assigns: Vec<InlineSetItem>,
        event_value: EventValue,
    },
    /// Some `var` was written (by a named action's `set` or by an
    /// [`UiEvent::InlineSet`] on tap). The main loop treats this like
    /// a source update: rebuild ctx, re-render.
    VarsChanged,
    /// A confirm modal's OK or Cancel was tapped; the main loop
    /// uses this to resolve the `pending_confirm` state and either
    /// forward the pending action or drop it.
    ConfirmReply { accept: bool },
    /// OK or Cancel on a dialog asking for an action param, with the
    /// value the dialog's widget holds (schema 6 `ask`).
    AskReply { accept: bool, value: Option<EventValue> },
    /// The PIN modal's keyboard OK was pressed. Carries the typed
    /// digits; the main loop verifies them and must never log them.
    PinEntered { digits: String },
    /// A `then` / `else` call from the worker. Unlike a tap it is
    /// kept, and run after any modal that is up closes.
    Chained { name: String, args: Vec<ActionArg> },
    /// The sleeping screen was touched (schema 12, `first_tap = wake`).
    Wake,
    /// The worker finished a call of action `name` (`key` tells it
    /// from calls with other args); `result` is the schema's
    /// `{ok, code, stdout, stderr}`.
    ActionDone { name: String, key: String, ok: bool, result: crate::expr::Value },
}

/// The scalar an input widget's `on_change` handler sees as
/// `event.value`. Kept primitive so the UI thread can snapshot it
/// without allocating; the main loop lifts it into a
/// [`crate::Value`] when building the eval context.
#[derive(Clone, Debug)]
pub enum EventValue {
    Bool(bool),
    Int(i64),
    Double(f64),
    /// Full typed string from a keypad on submit, or any widget that
    /// carries a non-numeric payload.
    String(String),
}

/// One `k = v` entry inside an inline `on_tap { set { ... } }`.
#[derive(Debug, Clone)]
pub struct InlineSetItem {
    pub key: String,
    pub value_raw: String,
    /// The bindings local to where the node was drawn (`repeat`
    /// items, component params), so `{{ row.id }}` evaluates when the
    /// node is tapped.
    pub locals: Vec<ActionArg>,
}

/// A single already-evaluated `name = value` arg from an action-call
/// shortcut (`on_tap vm_stop { vm = {{ item.id }} }`). Captured at
/// render time so the value reflects the scope the user tapped in --
/// after a repeat has advanced or a var has changed, the stored
/// value still matches what the row showed.
#[derive(Debug, Clone)]
pub struct ActionArg {
    pub name: String,
    pub value: crate::expr::Value,
}

/// Back-compat alias for the pre-nav payload.
#[allow(dead_code)]
pub type ActionTrigger = UiEvent;

#[derive(Clone)]
enum Binding {
    Action {
        name: String,
        args: Vec<ActionArg>,
        sink: Sender<UiEvent>,
    },
    Goto { page: String, sink: Sender<UiEvent> },
    InlineSet { assigns: Vec<InlineSetItem>, sink: Sender<UiEvent> },
    /// On an LVGL VALUE_CHANGED / RELEASED event, the trampoline
    /// reads the widget's current state, builds an
    /// [`EventValue`] of the right flavor, and sends an
    /// [`UiEvent::ChangeInlineSet`].
    Change {
        kind: ChangeKind,
        assigns: Vec<InlineSetItem>,
        sink: Sender<UiEvent>,
    },
    /// Modal OK / Cancel button; sends [`UiEvent::ConfirmReply`].
    Confirm { accept: bool, sink: Sender<UiEvent> },
    /// `on_change` / `on_submit` calling an action (named, or an
    /// inline handler with action keys); sends [`UiEvent::Action`]
    /// with `args` plus the widget's value as `value` and
    /// `event.value`.
    /// The widget of an ask dialog: keeps [`ASK_VALUE`] current.
    AskValue { kind: ChangeKind },
    /// An ask dialog's OK / Cancel: sends [`UiEvent::AskReply`] with
    /// [`ASK_VALUE`]. A keypad's own OK sends it too.
    AskButton { accept: bool, sink: Sender<UiEvent> },
    ChangeAction {
        kind: ChangeKind,
        action: String,
        args: Vec<ActionArg>,
        sink: Sender<UiEvent>,
    },
}

/// A widget whose `on_change` / `on_submit` carries a value.
#[derive(Clone, Copy, Debug)]
pub enum ValueWidget {
    Toggle,
    Slider,
    Dropdown,
    /// `select { style = roller }`.
    Roller,
    /// `select { style = buttons }`: a row of buttons, one checked.
    Buttons,
    /// A keypad's keyboard (`on_submit`).
    Keypad,
}

#[derive(Clone, Copy, Debug)]
enum ChangeKind {
    Toggle,
    Slider,
    /// The ask dialog's dropdown: the index, mapped back to the
    /// options it showed.
    Dropdown,
    /// A `select` on a page: the selected option's text (schema 8.4,
    /// `value` is the new value, not its position).
    Select,
    /// The same for `style = roller`.
    Roller,
    /// The same for `style = buttons`.
    Buttons,
    /// Keypad: READY on the keyboard. The trampoline reads the
    /// attached textarea's typed content and emits it as a
    /// [`EventValue::String`].
    Keypad,
    /// Same keyboard READY, but for the PIN modal: emits
    /// [`UiEvent::PinEntered`] instead of an inline `set`.
    PinEntry,
}

/// Global registry of per-widget bindings. [`Obj::on_tap`] /
/// [`Obj::on_tap_goto`] append a binding and use the returned index
/// as the LVGL event user_data; the static trampoline reads that
/// index when the event fires. The main loop calls
/// [`reset_action_bindings`] before each re-render so freshly-
/// cleared LVGL objects cannot fire against stale indices.
fn bindings() -> &'static Mutex<Vec<Binding>> {
    static B: OnceLock<Mutex<Vec<Binding>>> = OnceLock::new();
    B.get_or_init(|| Mutex::new(Vec::new()))
}

/// Drop every registered action / goto binding. Call this on the UI
/// thread AFTER [`Obj::clear_children`] on the screen.
pub fn reset_action_bindings() {
    bindings().lock().unwrap().clear();
    INPUTS.with(|v| v.borrow_mut().clear());
}

/// Milliseconds since the last touch on the default display.
pub fn inactive_ms() -> u32 {
    unsafe { sys::lv_display_get_inactive_time(std::ptr::null()) }
}

/// Count as a touch now (restarts the idle clock), e.g. when an alert
/// wakes the screen.
pub fn trigger_activity() {
    unsafe { sys::lv_display_trigger_activity(sys::lv_display_get_default()) };
}

thread_local! {
    /// The sleep cover on the system layer, while the screen sleeps.
    static SLEEP_COVER: std::cell::Cell<*mut sys::lv_obj_t> =
        const { std::cell::Cell::new(std::ptr::null_mut()) };
}

/// Where the sleep cover sends [`UiEvent::Wake`]. A static, not the
/// binding table: page re-renders reset that table, not this.
fn wake_sink() -> &'static Mutex<Option<Sender<UiEvent>>> {
    static S: OnceLock<Mutex<Option<Sender<UiEvent>>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(None))
}

unsafe extern "C" fn on_cover_pressed(_e: *mut sys::lv_event_t) {
    if let Some(tx) = wake_sink().lock().unwrap().as_ref() {
        let _ = tx.send(UiEvent::Wake);
    }
}

/// Point a sleep cover that is up at `sink`: after a reload the old
/// queue is no longer read, and a touch must still wake the screen.
pub fn set_wake_sink(sink: Sender<UiEvent>) {
    let mut g = wake_sink().lock().unwrap();
    if g.is_some() {
        *g = Some(sink);
    }
}

/// Black out the screen while it sleeps: a full-screen black object on
/// LVGL's system layer, above pages, modals and toasts. With `absorb`
/// it takes the touch that wakes the screen (`first_tap = wake`) and
/// reports it as [`UiEvent::Wake`]; without, touches pass through to
/// the page (`first_tap = act`). Idempotent.
pub fn show_sleep_cover(absorb: bool, sink: Sender<UiEvent>) {
    if !SLEEP_COVER.with(|c| c.get()).is_null() {
        return;
    }
    unsafe {
        let layer = sys::lv_layer_sys();
        let cover = sys::lv_obj_create(layer);
        sys::lv_obj_remove_style_all(cover);
        sys::lv_obj_set_size(cover, Size::Percent(100).to_lvgl(), Size::Percent(100).to_lvgl());
        sys::lv_obj_set_style_bg_color(cover, sys::lv_color_hex(0x000000), 0);
        sys::lv_obj_set_style_bg_opa(cover, 255, 0);
        sys::lv_obj_set_clickable(cover, absorb);
        if absorb {
            *wake_sink().lock().unwrap() = Some(sink);
            sys::lv_obj_add_event_cb(
                cover,
                Some(on_cover_pressed),
                sys::lv_event_code_t_LV_EVENT_PRESSED,
                std::ptr::null_mut(),
            );
        }
        SLEEP_COVER.with(|c| c.set(cover));
        // Nothing animates under the cover (a running button's
        // spinner): the wake redraws the page with fresh ones.
        sys::lv_anim_delete_all();
    }
}

/// Remove the sleep cover, if any. Called from the main loop, never
/// from inside the cover's own event callback.
pub fn hide_sleep_cover() {
    let cover = SLEEP_COVER.with(|c| c.replace(std::ptr::null_mut()));
    if !cover.is_null() {
        unsafe { sys::lv_obj_delete(cover) };
        *wake_sink().lock().unwrap() = None;
    }
}

thread_local! {
    /// Page inputs (keypad textareas) of the current render, in
    /// render order. LVGL objects are UI-thread only, hence a
    /// thread-local; cleared with the bindings.
    static INPUTS: std::cell::RefCell<Vec<*mut sys::lv_obj_t>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Text typed into each page input, in render order. Call before
/// clearing the screen for a re-render.
pub fn snapshot_inputs() -> Vec<String> {
    INPUTS.with(|v| {
        v.borrow()
            .iter()
            .map(|&ta| {
                let c = unsafe { sys::lv_textarea_get_text(ta) };
                if c.is_null() {
                    String::new()
                } else {
                    unsafe { std::ffi::CStr::from_ptr(c) }
                        .to_string_lossy()
                        .into_owned()
                }
            })
            .collect()
    })
}

/// Put text taken by [`snapshot_inputs`] back into the freshly
/// rendered inputs, by position. Only meaningful when the same page
/// was re-rendered.
pub fn restore_inputs(texts: &[String]) {
    INPUTS.with(|v| {
        // Position is the only key, so only restore onto the same set
        // of inputs; otherwise digits could land in another keypad.
        if v.borrow().len() != texts.len() {
            return;
        }
        for (&ta, text) in v.borrow().iter().zip(texts) {
            if text.is_empty() {
                continue;
            }
            if let Ok(c) = CString::new(text.as_str()) {
                // lv_textarea_set_text copies the string.
                unsafe { sys::lv_textarea_set_text(ta, c.as_ptr()) };
            }
        }
    });
}

unsafe extern "C" fn on_tap_trampoline(e: *mut sys::lv_event_t) {
    let idx = unsafe { sys::lv_event_get_user_data(e) } as usize;
    let bind = {
        let guard = bindings().lock().unwrap();
        guard.get(idx).cloned()
    };
    match bind {
        Some(Binding::Action { name, args, sink }) => {
            let _ = sink.send(UiEvent::Action { name, args });
        }
        Some(Binding::Goto { page, sink }) => {
            let _ = sink.send(UiEvent::Goto { page });
        }
        Some(Binding::InlineSet { assigns, sink }) => {
            let _ = sink.send(UiEvent::InlineSet { assigns });
        }
        Some(Binding::Confirm { accept, sink }) => {
            let _ = sink.send(UiEvent::ConfirmReply { accept });
        }
        Some(Binding::AskButton { accept, sink }) => {
            let value = ASK_VALUE.lock().unwrap_or_else(|e| e.into_inner()).take();
            let _ = sink.send(UiEvent::AskReply { accept, value });
        }
        _ => {}
    }
}

/// What the open ask dialog's widget holds; see [`set_ask_value`].
static ASK_VALUE: Mutex<Option<EventValue>> = Mutex::new(None);

/// Seed the ask dialog's value (what OK sends if the widget is left
/// as it is).
pub fn set_ask_value(v: Option<EventValue>) {
    *ASK_VALUE.lock().unwrap_or_else(|e| e.into_inner()) = v;
}

unsafe extern "C" fn on_change_trampoline(e: *mut sys::lv_event_t) {
    let idx = unsafe { sys::lv_event_get_user_data(e) } as usize;
    let target = unsafe { sys::lv_event_get_target(e) } as *mut sys::lv_obj_t;
    let bind = {
        let guard = bindings().lock().unwrap();
        guard.get(idx).cloned()
    };
    match bind {
        Some(Binding::ChangeAction { kind, action, mut args, sink }) => {
            use crate::expr::Value;
            let value = match widget_value(kind, target) {
                Some(EventValue::Bool(b)) => Value::Bool(b),
                Some(EventValue::Int(n)) => Value::Int(n),
                Some(EventValue::Double(d)) => Value::Double(d),
                Some(EventValue::String(s)) => Value::String(s),
                None => return,
            };
            let event = std::collections::BTreeMap::from([("value".to_string(), value.clone())]);
            args.push(ActionArg { name: "value".into(), value });
            args.push(ActionArg { name: "event".into(), value: Value::Map(event) });
            let _ = sink.send(UiEvent::Action { name: action, args });
        }
        Some(Binding::AskValue { kind }) => {
            let v = widget_value(kind, target);
            *ASK_VALUE.lock().unwrap_or_else(|e| e.into_inner()) = v;
        }
        Some(Binding::AskButton { sink, .. }) => {
            // A keypad's keyboard OK: submit what was typed; nothing
            // typed is no answer (the dialog asks again).
            let value = widget_value(ChangeKind::Keypad, target)
                .filter(|v| !matches!(v, EventValue::String(t) if t.is_empty()));
            let _ = sink.send(UiEvent::AskReply { accept: true, value });
        }
        Some(Binding::Change { kind: ChangeKind::PinEntry, sink, .. }) => {
            let digits = keyboard_text(target);
            let _ = sink.send(UiEvent::PinEntered { digits });
        }
        Some(Binding::Change { kind, assigns, sink }) => {
            if let Some(event_value) = widget_value(kind, target) {
                let _ = sink.send(UiEvent::ChangeInlineSet { assigns, event_value });
            }
        }
        _ => {}
    }
}

/// The value a toggle, slider, dropdown or keypad reports on change.
fn widget_value(kind: ChangeKind, target: *mut sys::lv_obj_t) -> Option<EventValue> {
    Some(match kind {
        ChangeKind::Toggle => {
            let on = unsafe { sys::lv_obj_has_state(target, sys::lv_state_t_LV_STATE_CHECKED) };
            EventValue::Bool(on)
        }
        ChangeKind::Slider => EventValue::Int(unsafe { sys::lv_slider_get_value(target) } as i64),
        ChangeKind::Dropdown => EventValue::Int(unsafe { sys::lv_dropdown_get_selected(target) } as i64),
        ChangeKind::Select => {
            // The selected line of the options text, whatever its length.
            let idx = unsafe { sys::lv_dropdown_get_selected(target) } as usize;
            let all = unsafe { sys::lv_dropdown_get_options(target) };
            let text = if all.is_null() { Default::default() } else { unsafe { std::ffi::CStr::from_ptr(all) }.to_string_lossy() };
            EventValue::String(text.split('\n').nth(idx).unwrap_or_default().to_string())
        }
        ChangeKind::Roller => {
            let idx = unsafe { sys::lv_roller_get_selected(target) } as usize;
            let all = unsafe { sys::lv_roller_get_options(target) };
            let text = if all.is_null() { Default::default() } else { unsafe { std::ffi::CStr::from_ptr(all) }.to_string_lossy() };
            EventValue::String(text.split('\n').nth(idx).unwrap_or_default().to_string())
        }
        ChangeKind::Buttons => {
            let idx = unsafe { sys::lv_buttonmatrix_get_selected_button(target) };
            // Tapping the option already chosen changes nothing.
            if ROW_CHOICE.with(|c| c.borrow_mut().insert(target as usize, Some(idx))) == Some(Some(idx)) {
                return None;
            }
            let text = unsafe { sys::lv_buttonmatrix_get_button_text(target, idx) };
            if text.is_null() {
                return None;
            }
            EventValue::String(unsafe { std::ffi::CStr::from_ptr(text) }.to_string_lossy().into_owned())
        }
        ChangeKind::Keypad => EventValue::String(keyboard_text(target)),
        ChangeKind::PinEntry => return None,
    })
}

/// Take the text of the textarea attached to keyboard `kb` and clear
/// the field: a submitted entry is consumed (and a masked one should
/// not linger on screen). Empty when no textarea is linked.
fn keyboard_text(kb: *mut sys::lv_obj_t) -> String {
    let ta = unsafe { sys::lv_keyboard_get_textarea(kb) };
    if ta.is_null() {
        return String::new();
    }
    let cstr = unsafe { sys::lv_textarea_get_text(ta) };
    let text = if cstr.is_null() {
        String::new()
    } else {
        unsafe { std::ffi::CStr::from_ptr(cstr) }
            .to_string_lossy()
            .into_owned()
    };
    unsafe { sys::lv_textarea_set_text(ta, c"".as_ptr()) };
    text
}

// LVGL's `LV_SIZE_CONTENT`, `LV_PCT(x)` and the grid sentinels are C
// macros that bindgen doesn't translate; mirror the exact bit layout
// from `vendor/lvgl/include/lvgl/core/lv_area.h` and
// `include/lvgl/layouts/lv_grid.h` here.
const LV_COORD_TYPE_SPEC: i32 = 1 << 29;
const LV_COORD_MAX_LOCAL: i32 = (1 << 29) - 1;
const LV_SIZE_CONTENT_LOCAL: i32 = LV_COORD_MAX_LOCAL | LV_COORD_TYPE_SPEC;
/// Mirrors `#define LV_GRID_FR(x) (LV_COORD_MAX - 100 + x)`.
pub(crate) fn grid_fr(x: i32) -> i32 {
    LV_COORD_MAX_LOCAL - 100 + x
}
/// Mirrors `#define LV_GRID_CONTENT (LV_COORD_MAX - 101)`: a track
/// sized to its content.
pub(crate) const GRID_CONTENT: i32 = LV_COORD_MAX_LOCAL - 101;

thread_local! {
    /// Grid track arrays of the current render. LVGL stores the
    /// pointers, so they must outlive their grid objects; freed by
    /// [`Obj::clear_children`] on the screen.
    static LAYOUT_MEM: std::cell::RefCell<Vec<Box<[i32]>>> =
        const { std::cell::RefCell::new(Vec::new()) };
    /// Chart series alphas: (chart, alpha of each series in the order
    /// added, which is LVGL's series index). Cleared with the screen,
    /// and per chart when one is created.
    static CHART_ALPHA: std::cell::RefCell<Vec<(*mut sys::lv_obj_t, Vec<u8>)>> =
        const { std::cell::RefCell::new(Vec::new()) };
    /// Point arrays of the current render's `line`s; same lifetime.
    static LINE_MEM: std::cell::RefCell<Vec<Box<[sys::lv_point_precise_t]>>> =
        const { std::cell::RefCell::new(Vec::new()) };
    /// Folder relative `image` paths are read from (the config's).
    static ASSET_DIR: std::cell::RefCell<std::path::PathBuf> =
        std::cell::RefCell::new(std::path::PathBuf::from("."));
}

/// Folder that a relative `image { src }` is read from: the config
/// file's, like commands' working directory (ADR 0013).
pub fn set_asset_dir(dir: &std::path::Path) {
    ASSET_DIR.with(|d| *d.borrow_mut() = dir.to_path_buf());
}

/// Theme icons decoded and scaled to their drawn size, by (file, size,
/// mtime). Entries are never freed (an image on screen, a dialog's
/// included, may point at one), so the cache is bounded by
/// [`ICON_CACHE_BYTES`]: past it, icons are read as plain images.
#[derive(Default)]
struct IconCache {
    entries: Vec<((std::path::PathBuf, u16, std::time::SystemTime), *mut sys::lv_draw_buf_t)>,
    bytes: usize,
}

/// 1 MiB: 64 icons of 64 px, or 16 of the largest (128 px).
const ICON_CACHE_BYTES: usize = 1024 * 1024;

thread_local! {
    static ICON_CACHE: std::cell::RefCell<IconCache> = std::cell::RefCell::new(IconCache::default());
}

/// A file `path` (relative to the asset folder) exists: a theme's
/// `icons/<name>.png` replaces the built-in only then.
pub fn asset_exists(path: &str) -> bool {
    ASSET_DIR.with(|d| d.borrow().join(path).is_file())
}

/// The compiled bitmap Montserrat nearest to `size` px.
fn bitmap_font(size: u16) -> *const sys::lv_font_t {
    let sizes = paneld_config::icons::FONT_SIZES;
    let px = *sizes.iter().min_by_key(|s| (i32::from(**s) - i32::from(size)).abs()).unwrap_or(&16);
    // SAFETY: taking the address of LVGL's static font descriptors.
    unsafe {
        match px {
            12 => &sys::lv_font_montserrat_12,
            14 => &sys::lv_font_montserrat_14,
            20 => &sys::lv_font_montserrat_20,
            24 => &sys::lv_font_montserrat_24,
            32 => &sys::lv_font_montserrat_32,
            _ => &sys::lv_font_montserrat_16,
        }
    }
}

/// The built-in families as TTF, regular and bold (`fonts/README.md`).
fn builtin_ttf(family: paneld_config::fonts::Builtin, bold: bool) -> &'static [u8] {
    use paneld_config::fonts::Builtin as B;
    match (family, bold) {
        (B::Roboto, false) => include_bytes!("../fonts/roboto-regular.ttf"),
        (B::Roboto, true) => include_bytes!("../fonts/roboto-bold.ttf"),
        (B::OpenSans, false) => include_bytes!("../fonts/opensans-regular.ttf"),
        (B::OpenSans, true) => include_bytes!("../fonts/opensans-bold.ttf"),
        (B::NotoSans, false) => include_bytes!("../fonts/notosans-regular.ttf"),
        (B::NotoSans, true) => include_bytes!("../fonts/notosans-bold.ttf"),
        (B::Montserrat, false) => include_bytes!("../fonts/montserrat-regular.ttf"),
        (B::Montserrat, true) => include_bytes!("../fonts/montserrat-bold.ttf"),
        (B::Inter, false) => include_bytes!("../fonts/inter-regular.ttf"),
        (B::Inter, true) => include_bytes!("../fonts/inter-bold.ttf"),
    }
}

/// At most this many TTF fonts (family, weight, size) are live. A font
/// is kept while any object may point at it, so fonts are only freed
/// all at once, by [`Obj::release_fonts`] when a config is (re)loaded
/// and the old screen is gone; until then, past the bound a new one
/// draws as bitmap Montserrat. A theme uses four; 32 leaves room for
/// pages that set their own.
const MAX_FONTS: usize = 32;

/// Glyph bitmaps a font keeps rendered: about 256 KB worth at its size,
/// from 4 glyphs (at 200 px) to 128 (LVGL's default; Latin-1 text
/// rarely uses more distinct glyphs on one screen).
fn glyph_cache_size(size: u16) -> usize {
    (256 * 1024 / (usize::from(size) * usize::from(size))).clamp(4, 128)
}

/// A font in the cache: what it was made from (a file font by its
/// canonical path, so `a.ttf` and `./a.ttf` are one font).
#[derive(PartialEq, Eq)]
struct FontKey {
    family: paneld_config::fonts::Family,
    bold: bool,
    size: u16,
}

struct FontEntry {
    key: FontKey,
    font: *mut sys::lv_font_t,
    /// A file font's bytes: TinyTTF reads glyphs from them, so they are
    /// freed only after the font.
    _data: Option<Box<[u8]>>,
}

#[derive(Default)]
struct FontCache {
    entries: Vec<FontEntry>,
    /// Problems already logged (each once, at most [`MAX_FONTS`]).
    logged: Vec<String>,
}

thread_local! {
    /// TinyTTF fonts by (family, weight, size), until the next
    /// [`Obj::release_fonts`]; UI thread only, like every LVGL object.
    static FONTS: std::cell::RefCell<FontCache> = std::cell::RefCell::new(FontCache::default());
}

fn log_font_once(msg: String) {
    FONTS.with(|c| {
        let logged = &mut c.borrow_mut().logged;
        if logged.len() < MAX_FONTS && !logged.contains(&msg) {
            eprintln!("paneld: {msg}");
            logged.push(msg);
        }
    });
}

/// What a built-in family falls back to for glyphs it lacks: Roboto,
/// Open Sans and Noto Sans have no arrows, Montserrat of the same size
/// and weight has. Everything ends in the bitmap Montserrat, which
/// holds LVGL's symbols (icons in text, the keypad's backspace and OK).
fn font_fallback(font: &paneld_config::fonts::Font) -> *const sys::lv_font_t {
    use paneld_config::fonts::{Builtin, Family, Font};
    match font.family {
        Family::Builtin(Builtin::Roboto | Builtin::OpenSans | Builtin::NotoSans) => {
            let m = Font { family: Family::Builtin(Builtin::Montserrat), ..font.clone() };
            ttf_font(&m).unwrap_or_else(|| bitmap_font(font.size))
        }
        _ => bitmap_font(font.size),
    }
}

/// The TinyTTF font for `font`, created on first use; `None` (logged
/// once) when it cannot be made.
fn ttf_font(font: &paneld_config::fonts::Font) -> Option<*const sys::lv_font_t> {
    use paneld_config::fonts::Family;
    // A file font: read whole from inside the config folder.
    let (family, data) = match &font.family {
        Family::Builtin(_) => (font.family.clone(), None),
        Family::File(p) => {
            let dir = ASSET_DIR.with(|d| d.borrow().clone());
            let full = match paneld_config::fonts::resolve_file(&dir, p) {
                Ok(full) => full,
                Err(e) => {
                    log_font_once(format!("{e}; drawn in Montserrat"));
                    return None;
                }
            };
            (Family::File(full.to_string_lossy().into_owned()), Some(full))
        }
    };
    let key = FontKey { family, bold: font.bold, size: font.size };
    let found = FONTS.with(|c| {
        let c = c.borrow();
        let hit = c.entries.iter().find(|e| e.key == key).map(|e| e.font as *const sys::lv_font_t);
        (hit, c.entries.len())
    });
    match found {
        (Some(f), _) => return Some(f),
        (None, n) if n >= MAX_FONTS => {
            log_font_once(format!("more than {MAX_FONTS} fonts in use; more are drawn in Montserrat"));
            return None;
        }
        _ => {}
    }
    // Made before this font, so it is in the cache already.
    let fallback = font_fallback(font);
    let bytes: Option<Box<[u8]>> = match &data {
        None => None,
        Some(full) => match paneld_config::fonts::read_file(full) {
            Ok(b) => Some(b.into_boxed_slice()),
            Err(e) => {
                log_font_once(format!("{e}; drawn in Montserrat"));
                return None;
            }
        },
    };
    let ttf: &[u8] = match (&font.family, &bytes) {
        (Family::Builtin(b), _) => builtin_ttf(*b, font.bold),
        (_, Some(b)) => b,
        (Family::File(_), None) => return None,
    };
    // SAFETY: `ttf` is static or kept in the entry with the font, and
    // freed after it. TinyTTF returns NULL for a file without the
    // tables it needs; past that stb_truetype trusts the file, which is
    // why a font file must be inside the config folder.
    let f = unsafe {
        sys::lv_tiny_ttf_create_data_ex(
            ttf.as_ptr().cast(),
            ttf.len(),
            i32::from(font.size),
            sys::lv_font_kerning_t_LV_FONT_KERNING_NONE,
            glyph_cache_size(font.size),
        )
    };
    if f.is_null() {
        log_font_once(format!("font {:?} could not be read as TTF; drawn in Montserrat", key.family));
        return None;
    }
    unsafe { (*f).fallback = fallback };
    FONTS.with(|c| c.borrow_mut().entries.push(FontEntry { key, font: f, _data: bytes }));
    Some(f as *const _)
}

/// Width and height from a PNG's header; `None` (with a log line) for
/// a file that is missing or not a PNG.
fn png_size(path: &std::path::Path) -> Option<(u32, u32)> {
    use std::io::Read;
    let mut head = [0u8; 24];
    let read = std::fs::File::open(path).and_then(|mut f| f.read_exact(&mut head));
    if read.is_err() || &head[..8] != b"\x89PNG\r\n\x1a\n" || &head[12..16] != b"IHDR" {
        eprintln!("paneld: image {}: not a readable PNG; not shown", path.display());
        return None;
    }
    let be = |b: &[u8]| u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
    Some((be(&head[16..20]), be(&head[20..24])))
}

/// How an image fills its box (schema 7 `fit`).
#[derive(Clone, Copy, Debug)]
pub enum ImageFit {
    Contain,
    Cover,
    Fill,
    None,
}

/// Sentinel ending a grid dsc array; matches `LV_GRID_TEMPLATE_LAST`.
pub(crate) const GRID_TEMPLATE_LAST: i32 = sys::LV_GRID_TEMPLATE_LAST as i32;

/// Guards the global `lv_init` / `lv_deinit` so a second `Ui` can't
/// be built in the same process. Dropping [`Ui`] releases the guard.
static LVGL_INITIALIZED: AtomicBool = AtomicBool::new(false);

/// Owns the global LVGL state for the lifetime of the daemon.
///
/// Build with [`Ui::init`], attach a window with [`Ui::open_window`],
/// then call [`Ui::run`] to drive the LVGL tick loop until the window
/// is closed.
pub struct Ui {
    _not_send: PhantomData<*const ()>,
}

impl Ui {
    pub fn init() -> Result<Self, &'static str> {
        if LVGL_INITIALIZED.swap(true, Ordering::SeqCst) {
            return Err("LVGL is already initialized in this process");
        }
        // SAFETY: ran at most once per process thanks to the guard.
        unsafe { sys::lv_init() };
        Ok(Self { _not_send: PhantomData })
    }

    pub fn open_window(&self, width: i32, height: i32, title: &str) -> Window {
        #[cfg(feature = "sdl")]
        {
            self.open_sdl_window(width, height, title)
        }
        #[cfg(not(feature = "sdl"))]
        {
            let _ = (width, height, title);
            panic!("paneld-ui built without the `sdl` feature; use open_drm_display or open_fbdev_display")
        }
    }

    /// Open an SDL2 window with a mouse input device attached (PC
    /// build). WSLg surfaces this as an X11 window.
    #[cfg(feature = "sdl")]
    pub fn open_sdl_window(&self, width: i32, height: i32, title: &str) -> Window {
        let display = unsafe { sys::lv_sdl_window_create(width, height) };
        assert!(!display.is_null(), "lv_sdl_window_create returned null");
        let c_title = CString::new(title).expect("window title contains a NUL byte");
        unsafe { sys::lv_sdl_window_set_title(display, c_title.as_ptr()) };
        let mouse = unsafe { sys::lv_sdl_mouse_create() };
        assert!(!mouse.is_null(), "lv_sdl_mouse_create returned null");
        Window { display, _input: Some(mouse) }
    }

    /// Open a DRM/KMS display (device build; ADR 0012). `device` is
    /// the DRM card path (`/dev/dri/card1` on the Pi 4). Returns
    /// `None` if the device can't be opened.
    #[cfg(feature = "drm")]
    pub fn open_drm_display(&self, device: &str) -> Option<Window> {
        let display = unsafe { sys::lv_linux_drm_create() };
        if display.is_null() {
            return None;
        }
        let c_dev = CString::new(device).ok()?;
        let r = unsafe { sys::lv_linux_drm_set_file(display, c_dev.as_ptr(), -1) };
        if r != sys::lv_result_t_LV_RESULT_OK {
            return None;
        }
        Some(Window { display, _input: None })
    }

    /// Open a Linux fbdev display (fallback for boards without a
    /// usable DRM/KMS path). `device` is e.g. `/dev/fb0`.
    pub fn open_fbdev_display(&self, device: &str) -> Option<Window> {
        let display = unsafe { sys::lv_linux_fbdev_create() };
        if display.is_null() {
            return None;
        }
        let c_dev = CString::new(device).ok()?;
        let r = unsafe { sys::lv_linux_fbdev_set_file(display, c_dev.as_ptr()) };
        if r != sys::lv_result_t_LV_RESULT_OK {
            return None;
        }
        Some(Window { display, _input: None })
    }

    /// Attach an evdev touch / mouse pointer at `device` (e.g.
    /// `/dev/input/event0`). Returns `false` if the device can't be
    /// opened.
    pub fn add_evdev_pointer(&self, device: &str) -> bool {
        use std::os::unix::io::IntoRawFd;
        match std::fs::File::open(device) {
            Ok(f) => add_pointer_fd(f.into_raw_fd()),
            Err(_) => false,
        }
    }

    /// A display that renders into memory instead of a screen, with a
    /// scriptable pointer. For `paneld shot`: render a page (and the
    /// result of taps on it) to an image without any hardware.
    pub fn open_memory_display(&self, width: i32, height: i32) -> MemDisplay {
        let display = unsafe { sys::lv_display_create(width, height) };
        assert!(!display.is_null(), "lv_display_create returned null");
        // One full-size buffer in DIRECT mode: after a refresh it holds
        // the whole frame. u32 elements keep it 4-byte aligned and big
        // enough for any color format up to 32 bits.
        // Leaked on purpose: LVGL keeps this pointer for the display's
        // whole life and displays are never deleted, so the buffer must
        // never be freed (one per `paneld shot` run).
        let buf: &'static mut [u32] =
            Box::leak(vec![0u32; (width * height) as usize].into_boxed_slice());
        unsafe {
            sys::lv_display_set_flush_cb(display, Some(mem_flush));
            sys::lv_display_set_buffers(
                display,
                buf.as_mut_ptr().cast(),
                std::ptr::null_mut(),
                (buf.len() * 4) as u32,
                sys::lv_display_render_mode_t_LV_DISPLAY_RENDER_MODE_DIRECT,
            );
            sys::lv_display_set_default(display);
        }
        let indev = unsafe { sys::lv_indev_create() };
        assert!(!indev.is_null());
        unsafe {
            sys::lv_indev_set_type(indev, sys::lv_indev_type_t_LV_INDEV_TYPE_POINTER);
            sys::lv_indev_set_read_cb(indev, Some(mem_pointer_read));
            sys::lv_indev_set_display(indev, display);
        }
        MemDisplay { display, buf, width, height }
    }

    /// Advance LVGL's clock by `ms` and run its timers, without
    /// sleeping. Drives a [`MemDisplay`].
    pub fn advance(&self, ms: u32) {
        RETIRED_LABELS.with(|r| r.borrow_mut().clear());
        unsafe {
            sys::lv_tick_inc(ms);
            sys::lv_timer_handler();
        }
    }

    /// Return a handle to the active screen's root `lv_obj`. Children
    /// added to it are drawn on the next frame.
    pub fn screen(&self) -> Obj<'_> {
        let raw = unsafe { sys::lv_screen_active() };
        assert!(!raw.is_null(), "lv_screen_active returned null");
        Obj { raw, _ui: PhantomData }
    }

    /// Pump the LVGL tick loop until `done` returns `true`. The LVGL
    /// SDL driver watches for its own window-close event and sets
    /// LV_SDL_DIRECT_EXIT; we also hand out `done` so callers can
    /// stop on `paneld ctl exit` later.
    pub fn run<F: FnMut() -> bool>(&self, mut done: F) {
        let mut pump = Pump::new();
        loop {
            if done() {
                break;
            }
            pump.step();
        }
    }

    /// Return a [`Pump`] a caller can drive manually. The caller is
    /// responsible for calling `pump.step()` on every iteration of
    /// its own loop; between steps it can drain inter-thread channels
    /// and tweak the UI tree.
    pub fn pump(&self) -> Pump<'_> {
        Pump::new()
    }
}

thread_local! {
    /// Touch devices read on demand: their fd and LVGL input device.
    static TOUCH: std::cell::RefCell<Vec<(std::os::raw::c_int, *mut sys::lv_indev_t)>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Read pointer events from `fd` (an evdev device), in LVGL's event
/// mode: no read timer runs while nobody touches the screen; the pump
/// reads when the fd has data ([`Pump::step`]). LVGL owns `fd` after.
fn add_pointer_fd(fd: std::os::raw::c_int) -> bool {
    let indev = unsafe { sys::lv_evdev_create_fd(sys::lv_indev_type_t_LV_INDEV_TYPE_POINTER, fd) };
    if indev.is_null() {
        return false;
    }
    unsafe extern "C" fn forget(e: *mut sys::lv_event_t) {
        // SAFETY: the target is the input device being deleted (LVGL
        // does that on a read error, and closes its fd).
        let indev = unsafe { sys::lv_event_get_target(e) } as *mut sys::lv_indev_t;
        TOUCH.with(|t| t.borrow_mut().retain(|(_, i)| *i != indev));
    }
    unsafe {
        sys::lv_indev_set_mode(indev, sys::lv_indev_mode_t_LV_INDEV_MODE_EVENT);
        sys::lv_indev_add_event_cb(indev, Some(forget), sys::lv_event_code_t_LV_EVENT_DELETE, std::ptr::null_mut());
    }
    TOUCH.with(|i| i.borrow_mut().push((fd, indev)));
    true
}

/// The read and write ends of the pipe that wakes the pump.
static WAKE_PIPE: std::sync::OnceLock<(std::os::raw::c_int, std::os::raw::c_int)> = std::sync::OnceLock::new();

fn wake_pipe() -> (std::os::raw::c_int, std::os::raw::c_int) {
    *WAKE_PIPE.get_or_init(|| {
        let mut fds = [-1 as std::os::raw::c_int; 2];
        // O_NONBLOCK | O_CLOEXEC: a full pipe already says "wake up".
        let ok = unsafe { pipe2(fds.as_mut_ptr(), 0o4000 | 0o2000000) } == 0;
        if ok { (fds[0], fds[1]) } else { (-1, -1) }
    })
}

/// Wake the UI thread's pump now (from any thread): something for the
/// main loop arrived (a source value, a ctl request, an action's
/// result).
pub fn wake() {
    let (_, w) = wake_pipe();
    if w >= 0 {
        let b = 1u8;
        unsafe { write(w, (&b as *const u8).cast(), 1) };
    }
}

/// A channel sender that wakes the pump after each send, for threads
/// that report to the main loop.
pub struct WakingSender<T>(std::sync::mpsc::Sender<T>);

impl<T> WakingSender<T> {
    pub fn send(&self, v: T) -> Result<(), std::sync::mpsc::SendError<T>> {
        let r = self.0.send(v);
        wake();
        r
    }
}

impl<T> Clone for WakingSender<T> {
    fn clone(&self) -> Self {
        WakingSender(self.0.clone())
    }
}

impl<T> From<std::sync::mpsc::Sender<T>> for WakingSender<T> {
    fn from(s: std::sync::mpsc::Sender<T>) -> Self {
        WakingSender(s)
    }
}

#[repr(C)]
struct PollFd {
    fd: std::os::raw::c_int,
    events: std::os::raw::c_short,
    revents: std::os::raw::c_short,
}

const POLLIN: std::os::raw::c_short = 1;

const POLLERR: std::os::raw::c_short = 8;
const POLLHUP: std::os::raw::c_short = 16;
const POLLNVAL: std::os::raw::c_short = 32;

unsafe extern "C" {
    fn poll(fds: *mut PollFd, nfds: std::os::raw::c_ulong, timeout: std::os::raw::c_int) -> std::os::raw::c_int;
    fn pipe2(fds: *mut std::os::raw::c_int, flags: std::os::raw::c_int) -> std::os::raw::c_int;
    fn read(fd: std::os::raw::c_int, buf: *mut std::ffi::c_void, n: usize) -> isize;
    fn write(fd: std::os::raw::c_int, buf: *const std::ffi::c_void, n: usize) -> isize;
}

/// The longest the pump sleeps when LVGL has nothing scheduled. News
/// on the channels (sources, ctl, actions) wakes it through the pipe,
/// so this only paces the clocks the main loop keeps (screen idle and
/// dim, toasts, alert `for`, reminders).
const IDLE_STEP_MS: u32 = 1000;

/// The longest step while a finger is down. A resting finger sends no
/// touch events, so the pump reads the device each step itself: LVGL
/// times a long press in its input read.
const PRESSED_STEP_MS: u32 = 50;

/// One iteration of the LVGL tick loop. See [`Ui::pump`].
pub struct Pump<'ui> {
    last: Instant,
    _ui: PhantomData<&'ui Ui>,
}

impl<'ui> Pump<'ui> {
    fn new() -> Self {
        Self { last: Instant::now(), _ui: PhantomData }
    }

    /// Advance LVGL's internal clock, run its timers, and block for
    /// the hint it returned (capped at 100ms). Returns the hint so
    /// the caller can use it as its own idle timeout.
    pub fn step(&mut self) -> Duration {
        RETIRED_LABELS.with(|r| r.borrow_mut().clear());
        self.tick();
        let inputs = TOUCH.with(|i| i.borrow().clone());
        let pressed = inputs
            .iter()
            .any(|(_, indev)| unsafe { sys::lv_indev_get_state(*indev) } == sys::lv_indev_state_t_LV_INDEV_STATE_PRESSED);
        let cap = if pressed { PRESSED_STEP_MS } else { IDLE_STEP_MS };
        let sleep_ms = unsafe { sys::lv_timer_handler() }.min(cap);
        let sleep = Duration::from_millis(sleep_ms as u64);
        // Sleep in `poll` on the touch devices and the wake pipe: a touch
        // is read at once, news for the main loop wakes it at once, and
        // an idle screen costs no wakeups beyond the idle step.
        let (wake_fd, _) = wake_pipe();
        let mut fds: Vec<PollFd> = inputs.iter().map(|(fd, _)| PollFd { fd: *fd, events: POLLIN, revents: 0 }).collect();
        fds.push(PollFd { fd: wake_fd, events: POLLIN, revents: 0 });
        let ready = unsafe { poll(fds.as_mut_ptr(), fds.len() as std::os::raw::c_ulong, sleep_ms as std::os::raw::c_int) };
        if ready <= 0 {
            if pressed {
                self.tick();
                for (_, indev) in &inputs {
                    unsafe { sys::lv_indev_read(*indev) };
                }
            }
            return sleep;
        }
        // Read with the clock at the touch's time, not the step's start:
        // a long press is timed from the real press.
        self.tick();
        for (pfd, (_, indev)) in fds.iter().zip(&inputs) {
            if pfd.revents & (POLLHUP | POLLERR | POLLNVAL) != 0 {
                // Unplugged or failing: stop reading it (deleting the
                // input device closes its fd and drops it from TOUCH).
                eprintln!("paneld: touch device lost (poll {:#x}); no more touch input from it", pfd.revents);
                unsafe { sys::lv_indev_delete(*indev) };
            } else if pfd.revents & POLLIN != 0 {
                unsafe { sys::lv_indev_read(*indev) };
            }
        }
        if fds.last().is_some_and(|w| w.revents & POLLIN != 0) {
            let mut buf = [0u8; 64];
            while unsafe { read(wake_fd, buf.as_mut_ptr().cast(), buf.len()) } > 0 {}
        }
        sleep
    }

    /// Advance LVGL's clock by the whole milliseconds since the last
    /// tick (the rest carries over, so the clock does not drift).
    fn tick(&mut self) {
        let ms = self.last.elapsed().as_millis() as u32;
        if ms > 0 {
            unsafe { sys::lv_tick_inc(ms) };
            self.last += Duration::from_millis(ms as u64);
        }
    }
}

impl Drop for Ui {
    fn drop(&mut self) {
        // SAFETY: paired with `lv_init` in `Ui::init`.
        unsafe { sys::lv_deinit() };
        LVGL_INITIALIZED.store(false, Ordering::SeqCst);
    }
}

/// Keyboard map for digit entry. LVGL keeps the pointers (it does
/// not copy the map), so it lives in a static. The keyboard reacts
/// to the button texts: LV_SYMBOL_BACKSPACE deletes, LV_SYMBOL_OK
/// sends READY.
struct KbMap([*const std::ffi::c_char; 16]);
// SAFETY: the pointers refer to 'static C string literals and are
// only ever read.
unsafe impl Sync for KbMap {}

static DIGIT_MAP: KbMap = KbMap([
    c"1".as_ptr(), c"2".as_ptr(), c"3".as_ptr(), c"\n".as_ptr(),
    c"4".as_ptr(), c"5".as_ptr(), c"6".as_ptr(), c"\n".as_ptr(),
    c"7".as_ptr(), c"8".as_ptr(), c"9".as_ptr(), c"\n".as_ptr(),
    c"\xEF\x95\x9A".as_ptr(), // LV_SYMBOL_BACKSPACE
    c"0".as_ptr(),
    c"\xEF\x80\x8C".as_ptr(), // LV_SYMBOL_OK
    c"".as_ptr(),
]);

/// One entry per button (newlines excluded): width 1; OK styled like
/// LVGL's own (LV_KEYBOARD_CTRL_BUTTON_FLAGS).
static DIGIT_CTRL: [sys::lv_buttonmatrix_ctrl_t; 12] = {
    const K: sys::lv_buttonmatrix_ctrl_t = sys::lv_buttonmatrix_ctrl_t_LV_BUTTONMATRIX_CTRL_NO_REPEAT
        | sys::lv_buttonmatrix_ctrl_t_LV_BUTTONMATRIX_CTRL_CLICK_TRIG
        | sys::lv_buttonmatrix_ctrl_t_LV_BUTTONMATRIX_CTRL_CHECKED;
    [1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, K | 1]
};

/// See [`Ui::open_memory_display`].
pub struct MemDisplay {
    display: *mut sys::lv_display_t,
    buf: &'static mut [u32],
    width: i32,
    height: i32,
}

// The scripted pointer of the memory display. LVGL reads it from
// `lv_timer_handler` on the UI thread.
static MEM_PTR_X: AtomicI32 = AtomicI32::new(0);
static MEM_PTR_Y: AtomicI32 = AtomicI32::new(0);
static MEM_PTR_DOWN: AtomicBool = AtomicBool::new(false);

unsafe extern "C" fn mem_flush(
    disp: *mut sys::lv_display_t,
    _area: *const sys::lv_area_t,
    _px: *mut u8,
) {
    // DIRECT mode renders straight into the buffer; nothing to copy.
    unsafe { sys::lv_display_flush_ready(disp) };
}

unsafe extern "C" fn mem_pointer_read(
    _indev: *mut sys::lv_indev_t,
    data: *mut sys::lv_indev_data_t,
) {
    let data = unsafe { &mut *data };
    data.point.x = MEM_PTR_X.load(Ordering::SeqCst);
    data.point.y = MEM_PTR_Y.load(Ordering::SeqCst);
    data.state = if MEM_PTR_DOWN.load(Ordering::SeqCst) {
        sys::lv_indev_state_t_LV_INDEV_STATE_PRESSED
    } else {
        sys::lv_indev_state_t_LV_INDEV_STATE_RELEASED
    };
}

impl MemDisplay {
    pub fn size(&self) -> (u32, u32) {
        (self.width as u32, self.height as u32)
    }

    /// Put the scripted pointer at (`x`, `y`), pressed or not. Takes
    /// effect at LVGL's next input read (drive it with
    /// [`Ui::advance`]).
    pub fn set_pointer(&self, x: i32, y: i32, pressed: bool) {
        MEM_PTR_X.store(x, Ordering::SeqCst);
        MEM_PTR_Y.store(y, Ordering::SeqCst);
        MEM_PTR_DOWN.store(pressed, Ordering::SeqCst);
    }

    /// Redraw everything now and return the frame as packed RGB8,
    /// row by row.
    pub fn rgb(&mut self) -> Vec<u8> {
        unsafe {
            sys::lv_obj_invalidate(sys::lv_screen_active());
            sys::lv_refr_now(self.display);
        }
        let cf = unsafe { sys::lv_display_get_color_format(self.display) };
        let n = (self.width * self.height) as usize;
        let bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(self.buf.as_ptr().cast::<u8>(), self.buf.len() * 4)
        };
        let mut out = Vec::with_capacity(n * 3);
        match cf {
            sys::lv_color_format_t_LV_COLOR_FORMAT_RGB565 => {
                for px in bytes[..n * 2].chunks_exact(2) {
                    let v = u16::from_le_bytes([px[0], px[1]]);
                    let r = ((v >> 11) & 0x1f) as u8;
                    let g = ((v >> 5) & 0x3f) as u8;
                    let b = (v & 0x1f) as u8;
                    out.extend_from_slice(&[(r << 3) | (r >> 2), (g << 2) | (g >> 4), (b << 3) | (b >> 2)]);
                }
            }
            sys::lv_color_format_t_LV_COLOR_FORMAT_RGB888 => {
                for px in bytes[..n * 3].chunks_exact(3) {
                    out.extend_from_slice(&[px[2], px[1], px[0]]);
                }
            }
            _ => {
                // XRGB8888 / ARGB8888: B, G, R, X in memory.
                for px in bytes[..n * 4].chunks_exact(4) {
                    out.extend_from_slice(&[px[2], px[1], px[0]]);
                }
            }
        }
        out
    }
}

/// A display plus (optionally) one attached pointer input device.
/// Attached to the [`Ui`]; dropped before `lv_deinit` runs. LVGL
/// owns the underlying objects through its global registries.
pub struct Window {
    display: *mut sys::lv_display_t,
    _input: Option<*mut sys::lv_indev_t>,
}

impl Window {
    pub fn display(&self) -> *mut sys::lv_display_t {
        self.display
    }
}

/// A borrowed LVGL object handle. Lifetime is tied to the [`Ui`] that
/// owns the global state; `Obj` never outlives it.
pub struct Obj<'ui> {
    pub(crate) raw: *mut sys::lv_obj_t,
    _ui: PhantomData<&'ui Ui>,
}

impl<'ui> Obj<'ui> {
    pub fn raw(&self) -> *mut sys::lv_obj_t {
        self.raw
    }

    /// A plain container object. Children are laid out according to
    /// the flex / grid / absolute settings applied below.
    pub fn new_container(&self) -> Obj<'ui> {
        let raw = unsafe { sys::lv_obj_create(self.raw) };
        assert!(!raw.is_null());
        Obj { raw, _ui: PhantomData }
    }

    /// A layout container (`row`, `column`, `stack`, `grid`,
    /// `scroll`): transparent and borderless, so only what the config
    /// (or a theme style) sets shows.
    pub fn new_layout(&self) -> Obj<'ui> {
        let obj = self.new_container();
        unsafe {
            sys::lv_obj_set_style_bg_opa(obj.raw, sys::_lv_opacity_level_t_LV_OPA_TRANSP as sys::lv_opa_t, 0);
            sys::lv_obj_set_style_border_width(obj.raw, 0, 0);
        }
        obj
    }

    /// A row with no style of its own (no theme look, so the text
    /// color and font are inherited), children centered across it,
    /// `gap` px apart; taps go through to the parent. Holds a button's
    /// icon and label side by side.
    pub fn new_plain_row(&self, gap: i32) -> Obj<'ui> {
        let obj = self.new_container();
        unsafe { sys::lv_obj_remove_style_all(obj.raw) };
        obj.set_enabled(false);
        obj.set_size(Size::Content, Size::Content);
        obj.set_flex_row();
        obj.set_flex_align(FlexAlign::Start, FlexAlign::Center);
        obj.set_gap(gap);
        obj
    }

    /// Fit a button's shortcut `label` (its child, or the label of a
    /// [`Self::new_plain_row`] after an icon) to the button's width on
    /// every resize: one line, shortened with `...` when it does not
    /// fit; beside an icon, with less room left than two lines' height
    /// (and less than the text), only the icon shows.
    pub fn fit_shortcut_label(&self, label: &Obj<'ui>) {
        unsafe extern "C" fn fit(e: *mut sys::lv_event_t) {
            // SAFETY: registered below on a live button with its own
            // descendant label as user data; both live while it does.
            unsafe {
                let btn = sys::lv_event_get_target(e) as *mut sys::lv_obj_t;
                let label = sys::lv_event_get_user_data(e) as *mut sys::lv_obj_t;
                let main = sys::lv_part_t_LV_PART_MAIN as sys::lv_part_t;
                let font = sys::lv_obj_get_style_text_font(label, main);
                if font.is_null() {
                    return;
                }
                let mut size = sys::lv_point_t::default();
                sys::lv_text_get_size(
                    &mut size,
                    sys::lv_label_get_text(label),
                    font,
                    sys::lv_obj_get_style_text_letter_space(label, main),
                    0,
                    sys::LV_COORD_MAX as i32,
                    sys::lv_text_flag_t_LV_TEXT_FLAG_NONE,
                );
                let parent = sys::lv_obj_get_parent(label);
                let mut room = sys::lv_obj_get_content_width(btn);
                // Beside an icon (a theme's unknown one leaves none).
                let icon = if parent == btn { std::ptr::null_mut() } else { sys::lv_obj_get_child(parent, 0) };
                if !icon.is_null() && icon != label {
                    room -= sys::lv_obj_get_width(icon) + sys::lv_obj_get_style_pad_column(parent, main);
                    let hide = room < size.x && room < 2 * (*font).line_height;
                    if sys::lv_obj_is_hidden(label) != hide {
                        sys::lv_obj_set_hidden(label, hide);
                    }
                }
                let width = if size.x <= room { LV_SIZE_CONTENT_LOCAL } else { room.max(0) };
                if sys::lv_obj_get_style_width(label, main) != width {
                    sys::lv_obj_set_width(label, width);
                }
            }
        }
        /// The label goes (a running button shows a spinner instead):
        /// the button must not fit it any more.
        unsafe extern "C" fn forget(e: *mut sys::lv_event_t) {
            // SAFETY: registered below on the label with its button as
            // user data; a parent outlives its children's DELETE.
            unsafe {
                let label = sys::lv_event_get_target(e) as *mut sys::lv_obj_t;
                let btn = sys::lv_event_get_user_data(e) as *mut sys::lv_obj_t;
                sys::lv_obj_remove_event_cb_with_user_data(btn, Some(fit), label.cast());
            }
        }
        unsafe {
            sys::lv_label_set_long_mode(label.raw, sys::lv_label_long_mode_t_LV_LABEL_LONG_MODE_DOTS);
            sys::lv_label_set_max_lines(label.raw, 1);
            sys::lv_obj_add_event_cb(self.raw, Some(fit), sys::lv_event_code_t_LV_EVENT_SIZE_CHANGED, label.raw.cast());
            sys::lv_obj_add_event_cb(label.raw, Some(forget), sys::lv_event_code_t_LV_EVENT_DELETE, self.raw.cast());
        }
    }

    /// Center this object in its parent (a button's shortcut content).
    pub fn center(&self) {
        unsafe { sys::lv_obj_center(self.raw) };
    }

    /// A text label. The text is copied by LVGL; the Rust string
    /// does not need to outlive the call.
    pub fn new_label(&self, text: &str) -> Obj<'ui> {
        let raw = unsafe { sys::lv_label_create(self.raw) };
        assert!(!raw.is_null());
        let c = CString::new(text).unwrap_or_else(|_| CString::new("<nul>").unwrap());
        unsafe { sys::lv_label_set_text(raw, c.as_ptr()) };
        Obj { raw, _ui: PhantomData }
    }

    /// A tappable button; usually gets a child label.
    pub fn new_button(&self) -> Obj<'ui> {
        let raw = unsafe { sys::lv_button_create(self.raw) };
        assert!(!raw.is_null());
        Obj { raw, _ui: PhantomData }
    }

    /// A horizontal / vertical fill-bar widget. Driver sets range and
    /// value through [`Self::set_bar_value`] and [`Self::set_range`].
    pub fn new_bar(&self) -> Obj<'ui> {
        let raw = unsafe { sys::lv_bar_create(self.raw) };
        assert!(!raw.is_null());
        Obj { raw, _ui: PhantomData }
    }

    /// A chart (`lv_chart`); set it up with [`Obj::chart_setup`] and
    /// [`Obj::chart_add_series`].
    pub fn new_chart(&self) -> Obj<'ui> {
        let raw = unsafe { sys::lv_chart_create(self.raw) };
        assert!(!raw.is_null());
        // A chart freed earlier may have had this address.
        CHART_ALPHA.with(|m| m.borrow_mut().retain(|(c, _)| *c != raw));
        Obj { raw, _ui: PhantomData }
    }

    /// Kind, point count, value range of the primary y axis, and the
    /// number of horizontal / vertical division lines. A line chart
    /// draws no dot on each point.
    pub fn chart_setup(&self, bars: bool, points: u32, min: i32, max: i32, hdiv: u32, vdiv: u32) {
        let ty = if bars {
            sys::lv_chart_type_t_LV_CHART_TYPE_BAR
        } else {
            sys::lv_chart_type_t_LV_CHART_TYPE_LINE
        };
        unsafe {
            sys::lv_chart_set_type(self.raw, ty);
            sys::lv_chart_set_point_count(self.raw, points.max(1));
            sys::lv_chart_set_axis_range(self.raw, sys::lv_chart_axis_t_LV_CHART_AXIS_PRIMARY_Y, min, max);
            sys::lv_chart_set_div_line_count(self.raw, hdiv, vdiv);
            if !bars {
                // LVGL draws a point only when both are non-zero.
                let indicator = sys::lv_part_t_LV_PART_INDICATOR as sys::lv_style_selector_t;
                sys::lv_obj_set_style_width(self.raw, 0, indicator);
                sys::lv_obj_set_style_height(self.raw, 0, indicator);
            }
        }
    }

    /// Fill under each line series (`chart { type = area }`): LVGL draws
    /// lines only, so the strip from each segment down to the chart's
    /// bottom is drawn after it, in the series color, translucent.
    pub fn chart_fill_area(&self) {
        unsafe extern "C" fn fill(e: *mut sys::lv_event_t) {
            // SAFETY: registered on a chart below; LVGL hands the layer it
            // is drawing into for DRAW_MAIN_END.
            unsafe {
                let chart = sys::lv_event_get_target(e) as *mut sys::lv_obj_t;
                let layer = sys::lv_event_get_layer(e);
                let mut coords: sys::lv_area_t = std::mem::zeroed();
                sys::lv_obj_get_coords(chart, &mut coords);
                let mut content: sys::lv_area_t = std::mem::zeroed();
                sys::lv_obj_get_content_coords(chart, &mut content);
                let bottom = content.y2;
                let clip = *sys::lv_paneld_layer_clip_area(layer);
                let count = sys::lv_chart_get_point_count(chart);
                // Dense series: a segment at least every 2 px is enough.
                let width = (content.x2 - content.x1).max(1) as u32;
                let step = (count / (width / 2).max(1)).max(1);
                let mut ser = sys::lv_chart_get_series_next(chart, std::ptr::null_mut());
                // Series come in the order added: their index.
                let mut idx = 0;
                while !ser.is_null() {
                    let ys = sys::lv_chart_get_series_y_array(chart, ser);
                    let color = sys::lv_chart_get_series_color(chart, ser);
                    let opa = scale_opa(80, series_alpha(chart, idx));
                    idx += 1;
                    let at = |i: u32| {
                        let mut p: sys::lv_point_t = std::mem::zeroed();
                        sys::lv_chart_get_point_pos_by_id(chart, ser, i, &mut p);
                        (coords.x1 + p.x, coords.y1 + p.y)
                    };
                    let mut i = 0;
                    while i + 1 < count {
                        let j = (i + step).min(count - 1);
                        let (a, b) = (i, j);
                        i = j;
                        let none = |k: u32| *ys.add(k as usize) == sys::LV_CHART_POINT_NONE as i32;
                        if none(a) || none(b) {
                            continue;
                        }
                        let ((x0, y0), (x1, y1)) = (at(a), at(b));
                        // Only what is being redrawn.
                        if x1 < clip.x1 || x0 > clip.x2 {
                            continue;
                        }
                        // A narrow segment as a plain column (much cheaper to
                        // draw in software); a wide one exactly.
                        if x1 - x0 <= 4 {
                            let mut dsc: sys::lv_draw_rect_dsc_t = std::mem::zeroed();
                            sys::lv_draw_rect_dsc_init(&mut dsc);
                            dsc.bg_color = color;
                            dsc.bg_opa = opa;
                            let area = sys::lv_area_t { x1: x0, y1: (y0 + y1) / 2, x2: (x1 - 1).max(x0), y2: bottom };
                            sys::lv_draw_rect(layer, &dsc, &area);
                            continue;
                        }
                        for tri in [[(x0, y0), (x1, y1), (x1, bottom)], [(x0, y0), (x1, bottom), (x0, bottom)]] {
                            let mut dsc: sys::lv_draw_triangle_dsc_t = std::mem::zeroed();
                            sys::lv_draw_triangle_dsc_init(&mut dsc);
                            dsc.color = color;
                            dsc.opa = opa;
                            for (k, (x, y)) in tri.into_iter().enumerate() {
                                dsc.p[k] = sys::lv_point_precise_t { x, y };
                            }
                            sys::lv_draw_triangle(layer, &dsc);
                        }
                    }
                    ser = sys::lv_chart_get_series_next(chart, ser);
                }
            }
        }
        unsafe { sys::lv_obj_add_event_cb(self.raw, Some(fill), sys::lv_event_code_t_LV_EVENT_DRAW_MAIN_END, std::ptr::null_mut()) };
    }

    /// Add a series in `rgb` at `opa` (255 opaque). `None` values are
    /// gaps. The chart owns the series and copies the values.
    pub fn chart_add_series(&self, rgb: u32, opa: u8, values: &[Option<i32>]) {
        /// LVGL has no per-series opacity: a translucent series gets its
        /// alpha on each of its draw tasks, found by the series index
        /// LVGL puts in `id1` for a line and a bar (we never hide one).
        unsafe extern "C" fn series_opa(e: *mut sys::lv_event_t) {
            // SAFETY: registered on a chart below for DRAW_TASK_ADDED, so
            // LVGL hands a live draw task; its descriptor matches its type.
            unsafe {
                let chart = sys::lv_event_get_target(e) as *mut sys::lv_obj_t;
                let task = sys::lv_event_get_draw_task(e);
                let base = sys::lv_draw_task_get_draw_dsc(task) as *const sys::lv_draw_dsc_base_t;
                if base.is_null() || (*base).part != sys::lv_part_t_LV_PART_ITEMS {
                    return;
                }
                let ty = sys::lv_draw_task_get_type(task);
                let alpha = series_alpha(chart, (*base).id1);
                if ty == sys::lv_draw_task_type_t_LV_DRAW_TASK_TYPE_LINE {
                    let d = sys::lv_draw_task_get_line_dsc(task);
                    (*d).opa = scale_opa((*d).opa, alpha);
                } else if ty == sys::lv_draw_task_type_t_LV_DRAW_TASK_TYPE_FILL {
                    let d = sys::lv_draw_task_get_fill_dsc(task);
                    (*d).opa = scale_opa((*d).opa, alpha);
                }
            }
        }
        let points: Vec<i32> = values.iter().map(|v| v.unwrap_or(sys::LV_CHART_POINT_NONE as i32)).collect();
        unsafe {
            let color = sys::lv_color_hex(rgb);
            let ser = sys::lv_chart_add_series(self.raw, color, sys::lv_chart_axis_t_LV_CHART_AXIS_PRIMARY_Y);
            if ser.is_null() {
                return;
            }
            // Every series is recorded, opaque ones too, so the index
            // lines up; the hook goes on with the first translucent one.
            let raw = self.raw;
            let hook = CHART_ALPHA.with(|m| {
                let mut m = m.borrow_mut();
                let at = match m.iter().position(|(c, _)| *c == raw) {
                    Some(i) => i,
                    None => {
                        m.push((raw, Vec::new()));
                        m.len() - 1
                    }
                };
                let alphas = &mut m[at].1;
                let had = alphas.iter().any(|&a| a < 255);
                alphas.push(opa);
                opa < 255 && !had
            });
            if hook {
                sys::lv_obj_add_flag(raw, sys::lv_obj_flag_t_LV_OBJ_FLAG_SEND_DRAW_TASK_EVENTS);
                sys::lv_obj_add_event_cb(raw, Some(series_opa), sys::lv_event_code_t_LV_EVENT_DRAW_TASK_ADDED, std::ptr::null_mut());
            }
            sys::lv_chart_set_all_values(self.raw, ser, sys::LV_CHART_POINT_NONE as i32);
            sys::lv_chart_set_series_values(self.raw, ser, points.as_ptr(), points.len());
            sys::lv_chart_refresh(self.raw);
        }
    }

    /// A polyline through `points` (px, relative to the object).
    pub fn new_line(&self, points: &[(i32, i32)]) -> Obj<'ui> {
        let raw = unsafe { sys::lv_line_create(self.raw) };
        assert!(!raw.is_null());
        let pts: Box<[sys::lv_point_precise_t]> =
            points.iter().map(|&(x, y)| sys::lv_point_precise_t { x, y }).collect();
        let (ptr, len) = (pts.as_ptr(), pts.len() as u32);
        // LVGL keeps the pointer: the array lives until the screen is
        // cleared, like the grid tracks.
        LINE_MEM.with(|m| m.borrow_mut().push(pts));
        unsafe { sys::lv_line_set_points(raw, ptr, len) };
        Obj { raw, _ui: PhantomData }
    }

    /// Line width and rounded ends of a `line`, in `color` (rgb, alpha).
    pub fn set_line_style(&self, width: i32, rounded: bool, color: Option<(u32, u8)>) {
        unsafe {
            sys::lv_obj_set_style_line_width(self.raw, width, 0);
            sys::lv_obj_set_style_line_rounded(self.raw, rounded, 0);
            if let Some((rgb, opa)) = color {
                sys::lv_obj_set_style_line_color(self.raw, sys::lv_color_hex(rgb), 0);
                sys::lv_obj_set_style_line_opa(self.raw, opa, 0);
            }
        }
    }

    /// An image read from `path` (PNG); relative to the asset folder
    /// (see [`set_asset_dir`]). Returns `None` for a path LVGL cannot
    /// take (an interior NUL).
    pub fn new_image(&self, path: &str, fit: ImageFit) -> Option<Obj<'ui>> {
        let full = ASSET_DIR.with(|d| d.borrow().join(path));
        // Decoded, an image takes 4 bytes a pixel whatever its file
        // size: a small PNG of 8000x8000 would take 250 MB. Up to twice
        // the screen's pixels is allowed.
        let (w, h) = png_size(&full)?;
        let screen = unsafe {
            let d = sys::lv_display_get_default();
            (sys::lv_display_get_horizontal_resolution(d) as u64) * (sys::lv_display_get_vertical_resolution(d) as u64)
        };
        // And no larger than the decoded-image cache (4 bytes a pixel):
        // a bigger one is decoded again on every redraw, or not drawn.
        let cap = (2 * screen.max(800 * 480)).min(sys::LV_CACHE_DEF_SIZE as u64 / 4);
        if (w as u64) * (h as u64) > cap {
            eprintln!("paneld: image {}: {w}x{h} is too large for this screen; not shown", full.display());
            return None;
        }
        // `A:` is the POSIX file driver (lv_conf.h); the rest is the
        // path as is.
        let src = CString::new(format!("A:{}", full.display())).ok()?;
        let raw = unsafe { sys::lv_image_create(self.raw) };
        assert!(!raw.is_null());
        let align = match fit {
            ImageFit::Contain => sys::lv_image_align_t_LV_IMAGE_ALIGN_CONTAIN,
            ImageFit::Cover => sys::lv_image_align_t_LV_IMAGE_ALIGN_COVER,
            ImageFit::Fill => sys::lv_image_align_t_LV_IMAGE_ALIGN_STRETCH,
            ImageFit::None => sys::lv_image_align_t_LV_IMAGE_ALIGN_CENTER,
        };
        unsafe {
            // LVGL copies a file path.
            sys::lv_image_set_src(raw, src.as_ptr().cast());
            sys::lv_image_set_inner_align(raw, align);
        }
        Some(Obj { raw, _ui: PhantomData })
    }

    /// A theme icon from the PNG `path` (relative to the asset folder),
    /// `size` x `size` px. Decoded once and kept at the drawn size (see
    /// [`ICON_CACHE`]), so a redraw neither decodes it again nor holds
    /// a large file's pixels. Past the cache's budget it is read as an
    /// ordinary image.
    pub fn new_icon_image(&self, path: &str, size: u16) -> Option<Obj<'ui>> {
        let full = ASSET_DIR.with(|d| d.borrow().join(path));
        let mtime = std::fs::metadata(&full).and_then(|m| m.modified()).ok()?;
        let key = (full.clone(), size, mtime);
        let cached = ICON_CACHE.with(|c| c.borrow().entries.iter().find(|(k, _)| *k == key).map(|(_, b)| *b));
        let buf = match cached {
            Some(b) => b,
            None => {
                let budget_left = ICON_CACHE.with(|c| c.borrow().bytes) < ICON_CACHE_BYTES;
                let src = CString::new(format!("A:{}", full.display())).ok()?;
                let b = if budget_left {
                    unsafe { sys::lv_paneld_load_scaled(src.as_ptr(), i32::from(size)) }
                } else {
                    std::ptr::null_mut()
                };
                if b.is_null() {
                    let obj = self.new_image(path, ImageFit::Contain)?;
                    obj.set_size(Size::Px(i32::from(size)), Size::Px(i32::from(size)));
                    return Some(obj);
                }
                let bytes = unsafe { (*b).data_size } as usize;
                ICON_CACHE.with(|c| {
                    let mut c = c.borrow_mut();
                    c.bytes += bytes;
                    c.entries.push((key, b));
                });
                b
            }
        };
        let raw = unsafe { sys::lv_image_create(self.raw) };
        assert!(!raw.is_null());
        unsafe {
            // A draw buffer is laid out as an `lv_image_dsc_t`; it stays
            // in the cache for the life of the process.
            sys::lv_image_set_src(raw, buf as *const std::ffi::c_void);
            sys::lv_image_set_inner_align(raw, sys::lv_image_align_t_LV_IMAGE_ALIGN_CONTAIN);
        }
        let obj = Obj { raw, _ui: PhantomData };
        obj.set_size(Size::Px(i32::from(size)), Size::Px(i32::from(size)));
        Some(obj)
    }

    /// Tint an image with `rgb` (icons drawn in one color); `opa` is
    /// how much of the tint covers the image.
    pub fn set_image_recolor(&self, rgb: u32, opa: u8) {
        unsafe {
            sys::lv_obj_set_style_image_recolor(self.raw, sys::lv_color_hex(rgb), 0);
            sys::lv_obj_set_style_image_recolor_opa(self.raw, opa, 0);
        }
    }

    /// A built-in icon (schema 8.3): `glyph` from the Montserrat font
    /// nearest to `size` px (the sizes in `lv_conf.h`). Above the
    /// largest (32) the glyph is scaled up, inside a box of the scaled
    /// size so the layout makes room for it; the box is returned then.
    /// The color is the inherited text color unless set on the result.
    pub fn new_icon_glyph(&self, glyph: char, size: u16) -> Obj<'ui> {
        let sizes = paneld_config::icons::FONT_SIZES;
        let largest = *sizes.last().unwrap_or(&32);
        let font_px = *sizes.iter().min_by_key(|s| (i32::from(**s) - i32::from(size)).abs()).unwrap_or(&16);
        let mut buf = [0u8; 4];
        let text = glyph.encode_utf8(&mut buf);
        if size <= largest {
            let label = self.new_label(text);
            label.set_font_size(font_px);
            return label;
        }
        let size = size.min(paneld_config::icons::MAX_SIZE);
        let font: *const sys::lv_font_t = unsafe { &sys::lv_font_montserrat_32 };
        // The glyph's own box at 32 px, then scaled by size / 32.
        let (w, h) = unsafe {
            (
                i32::from(sys::lv_font_get_glyph_width(font, glyph as u32, 0)),
                sys::lv_font_get_line_height(font),
            )
        };
        let scale = i32::from(size) * 256 / i32::from(largest);
        // No style of its own (inherits the text color) and no clicks
        // (a tap goes to the enclosing button).
        let boxed = self.new_container();
        unsafe { sys::lv_obj_remove_style_all(boxed.raw) };
        boxed.set_enabled(false);
        boxed.set_size(Size::Px(w * scale / 256), Size::Px(h * scale / 256));
        let label = boxed.new_label(text);
        label.set_font_size(largest);
        unsafe {
            sys::lv_obj_set_style_transform_scale(label.raw, scale, 0);
            sys::lv_obj_set_style_transform_pivot_x(label.raw, 0, 0);
            sys::lv_obj_set_style_transform_pivot_y(label.raw, 0, 0);
        }
        boxed
    }

    /// An image's own opacity, 255 opaque.
    pub fn set_image_opa(&self, opa: u8) {
        unsafe { sys::lv_obj_set_style_image_opa(self.raw, opa, 0) };
    }

    /// This object's text color as it resolves now (its own, else
    /// inherited): the default tint of an icon image.
    pub fn text_color(&self) -> (u32, u8) {
        unsafe {
            let part = sys::lv_part_t_LV_PART_MAIN;
            let c = sys::lv_obj_get_style_text_color(self.raw, part);
            let rgb = (u32::from(c.red) << 16) | (u32::from(c.green) << 8) | u32::from(c.blue);
            (rgb, sys::lv_obj_get_style_text_opa(self.raw, part))
        }
    }

    /// A circular arc gauge (dial) for display: no knob, and a press
    /// (an `on_tap` makes it clickable) never drags its value.
    pub fn new_arc(&self) -> Obj<'ui> {
        unsafe extern "C" fn no_drag(e: *mut sys::lv_event_t) {
            // SAFETY: registered below as a preprocess handler; stopping
            // here keeps the arc's own PRESSING handler from running.
            unsafe { sys::lv_event_stop_processing(e) };
        }
        let raw = unsafe { sys::lv_arc_create(self.raw) };
        assert!(!raw.is_null());
        unsafe {
            sys::lv_obj_remove_style(raw, std::ptr::null_mut(), sys::lv_part_t_LV_PART_KNOB as sys::lv_style_selector_t);
            sys::lv_obj_set_clickable(raw, false);
            let code = sys::lv_event_code_t_LV_EVENT_PRESSING | sys::lv_event_code_t_LV_EVENT_PREPROCESS;
            sys::lv_obj_add_event_cb(raw, Some(no_drag), code, std::ptr::null_mut());
        }
        Obj { raw, _ui: PhantomData }
    }

    /// A built-in spinning indicator; no value, just animates.
    pub fn new_spinner(&self) -> Obj<'ui> {
        let raw = unsafe { sys::lv_spinner_create(self.raw) };
        assert!(!raw.is_null());
        Obj { raw, _ui: PhantomData }
    }

    /// A two-state toggle switch (schema `toggle`). State is driven
    /// through [`Self::set_toggle_state`] and read back by the
    /// change-event trampoline.
    pub fn new_toggle(&self) -> Obj<'ui> {
        let raw = unsafe { sys::lv_switch_create(self.raw) };
        assert!(!raw.is_null());
        Obj { raw, _ui: PhantomData }
    }

    /// Add or remove `LV_STATE_CHECKED` so the switch displays `on`.
    pub fn set_toggle_state(&self, on: bool) {
        unsafe {
            if on {
                sys::lv_obj_add_state(self.raw, sys::lv_state_t_LV_STATE_CHECKED);
            } else {
                sys::lv_obj_remove_state(self.raw, sys::lv_state_t_LV_STATE_CHECKED);
            }
        }
    }

    /// A QR code widget (schema `qr`). Size is in pixels (square);
    /// data is set via [`Self::set_qrcode_data`].
    pub fn new_qrcode(&self, size: i32) -> Obj<'ui> {
        let raw = unsafe { sys::lv_qrcode_create(self.raw) };
        assert!(!raw.is_null());
        unsafe { sys::lv_qrcode_set_size(raw, size) };
        Obj { raw, _ui: PhantomData }
    }

    /// Encode `data` into the QR code. LVGL copies the bytes.
    pub fn set_qrcode_data(&self, data: &str) {
        let c = CString::new(data).unwrap_or_else(|_| CString::new("").unwrap());
        unsafe { sys::lv_qrcode_set_data(self.raw, c.as_ptr()) };
        // `set_data` only stashes the string; `render` turns it into
        // a bitmap. In immediate update mode this would be automatic;
        // in deferred it is not. Force a render either way so the
        // widget shows a valid code before the next LVGL tick.
        unsafe {
            let _ = sys::lv_qrcode_render(self.raw);
        }
    }

    /// A dropdown (schema `select`, default `style = dropdown`).
    /// Options come as a newline-separated string -- lvgl's own
    /// delimiter. `set_dropdown_options` and
    /// `set_dropdown_selected` configure it.
    pub fn new_dropdown(&self) -> Obj<'ui> {
        let raw = unsafe { sys::lv_dropdown_create(self.raw) };
        assert!(!raw.is_null());
        Obj { raw, _ui: PhantomData }
    }

    pub fn set_dropdown_options(&self, options: &str) {
        let c = match CString::new(options) {
            Ok(c) => c,
            Err(_) => CString::new(options.replace('\0', "")).unwrap(),
        };
        unsafe { sys::lv_dropdown_set_options(self.raw, c.as_ptr()) };
    }

    /// A roller (`select { style = roller }`) showing `options`, one per
    /// line, with `rows` visible.
    pub fn new_roller(&self, options: &str, rows: u32) -> Obj<'ui> {
        let raw = unsafe { sys::lv_roller_create(self.raw) };
        assert!(!raw.is_null());
        let c = CString::new(options).unwrap_or_default();
        unsafe {
            sys::lv_roller_set_options(raw, c.as_ptr(), sys::lv_roller_mode_t_LV_ROLLER_MODE_NORMAL);
            sys::lv_roller_set_visible_row_count(raw, rows);
        }
        Obj { raw, _ui: PhantomData }
    }

    pub fn set_roller_selected(&self, idx: u32) {
        unsafe { sys::lv_roller_set_selected(self.raw, idx, false) };
    }

    /// One row of buttons (`select { style = buttons }`), exactly one
    /// checked: the selected option.
    pub fn new_button_row(&self, options: &[String], selected: Option<usize>) -> Obj<'ui> {
        let raw = unsafe { sys::lv_buttonmatrix_create(self.raw) };
        assert!(!raw.is_null());
        // LVGL keeps pointers to the labels: they live with the widget
        // and are freed when LVGL deletes it.
        // An empty label is LVGL's end of the map: show it as a space.
        let labels: Vec<CString> = options
            .iter()
            .map(|o| CString::new(if o.is_empty() { " " } else { o.as_str() }).unwrap_or_default())
            .collect();
        let mut map: Vec<*const std::os::raw::c_char> = labels.iter().map(|c| c.as_ptr()).collect();
        map.push(c"".as_ptr());
        let owned: Box<ButtonLabels> = Box::new((labels, map));
        unsafe extern "C" fn free_map(e: *mut sys::lv_event_t) {
            // SAFETY: the user data is the Box leaked below, taken once.
            // LVGL still reads the map while it finishes deleting the
            // widget, so it is freed at the next pump step, not here.
            let p = unsafe { sys::lv_event_get_user_data(e) } as *mut ButtonLabels;
            let labels = unsafe { Box::from_raw(p) };
            RETIRED_LABELS.with(|r| r.borrow_mut().push(*labels));
        }
        unsafe {
            sys::lv_buttonmatrix_set_map(raw, owned.1.as_ptr());
            // On release (not on press, not repeated while held), like a
            // tap anywhere else.
            sys::lv_buttonmatrix_set_button_ctrl_all(
                raw,
                sys::lv_buttonmatrix_ctrl_t_LV_BUTTONMATRIX_CTRL_CHECKABLE
                    | sys::lv_buttonmatrix_ctrl_t_LV_BUTTONMATRIX_CTRL_CLICK_TRIG
                    | sys::lv_buttonmatrix_ctrl_t_LV_BUTTONMATRIX_CTRL_NO_REPEAT,
            );
            // Room for the buttons in a short row.
            sys::lv_obj_set_style_pad_all(raw, 4, 0);
            sys::lv_obj_set_style_pad_gap(raw, 4, 0);
            sys::lv_buttonmatrix_set_one_checked(raw, true);
            if let Some(i) = selected {
                sys::lv_buttonmatrix_set_button_ctrl(raw, i as u32, sys::lv_buttonmatrix_ctrl_t_LV_BUTTONMATRIX_CTRL_CHECKED);
            }
            ROW_CHOICE.with(|c| c.borrow_mut().insert(raw as usize, selected.map(|i| i as u32)));
            sys::lv_obj_add_event_cb(raw, Some(free_map), sys::lv_event_code_t_LV_EVENT_DELETE, Box::into_raw(owned).cast());
        }
        Obj { raw, _ui: PhantomData }
    }

    pub fn set_dropdown_selected(&self, idx: u16) {
        unsafe { sys::lv_dropdown_set_selected(self.raw, idx.into()) };
    }

    /// A textarea (schema `keypad`'s display line). `masked` turns on
    /// password mode so digits render as dots; `max_length` of 0 is
    /// treated as unlimited.
    pub fn new_textarea(&self, masked: bool, max_length: u32) -> Obj<'ui> {
        // LVGL keeps the pointer, not a copy, so the accepted set must
        // live for the whole program.
        static DIGITS: &std::ffi::CStr = c"0123456789";
        let raw = unsafe { sys::lv_textarea_create(self.raw) };
        assert!(!raw.is_null());
        unsafe {
            sys::lv_textarea_set_password_mode(raw, masked);
            sys::lv_textarea_set_one_line(raw, true);
            // Schema: a keypad takes digits. The number keyboard also
            // has `.` and `+/-` keys; the textarea ignores them.
            sys::lv_textarea_set_accepted_chars(raw, DIGITS.as_ptr());
            if max_length > 0 {
                sys::lv_textarea_set_max_length(raw, max_length);
            }
        }
        Obj { raw, _ui: PhantomData }
    }

    /// Remember this textarea as a page input whose typed text should
    /// survive a re-render of the same page (see [`snapshot_inputs`]).
    pub fn register_input(&self) {
        INPUTS.with(|v| v.borrow_mut().push(self.raw));
    }

    pub fn align_top_mid(&self, y: i32) {
        unsafe { sys::lv_obj_align(self.raw, sys::lv_align_t_LV_ALIGN_TOP_MID, 0, y) };
    }

    pub fn align_bottom_mid(&self, y: i32) {
        unsafe { sys::lv_obj_align(self.raw, sys::lv_align_t_LV_ALIGN_BOTTOM_MID, 0, y) };
    }

    /// LVGL's top layer: drawn above every screen and not touched
    /// when the screen is cleared for a re-render. Used for toasts.
    pub fn top_layer(&self) -> Obj<'ui> {
        let raw = unsafe { sys::lv_layer_top() };
        assert!(!raw.is_null());
        Obj { raw, _ui: PhantomData }
    }

    /// Make this toggle, slider or dropdown the widget of an ask
    /// dialog; for a keypad's keyboard, its OK submits the dialog.
    pub fn on_change_ask(&self, widget: ValueWidget, sink: Sender<UiEvent>) {
        let (binding, code) = match widget {
            ValueWidget::Toggle => (Binding::AskValue { kind: ChangeKind::Toggle }, sys::lv_event_code_t_LV_EVENT_VALUE_CHANGED),
            ValueWidget::Slider => (Binding::AskValue { kind: ChangeKind::Slider }, sys::lv_event_code_t_LV_EVENT_VALUE_CHANGED),
            ValueWidget::Dropdown => (Binding::AskValue { kind: ChangeKind::Dropdown }, sys::lv_event_code_t_LV_EVENT_VALUE_CHANGED),
            ValueWidget::Roller => (Binding::AskValue { kind: ChangeKind::Roller }, sys::lv_event_code_t_LV_EVENT_VALUE_CHANGED),
            ValueWidget::Buttons => (Binding::AskValue { kind: ChangeKind::Buttons }, sys::lv_event_code_t_LV_EVENT_VALUE_CHANGED),
            ValueWidget::Keypad => (Binding::AskButton { accept: true, sink }, sys::lv_event_code_t_LV_EVENT_READY),
        };
        let idx = {
            let mut g = bindings().lock().unwrap();
            g.push(binding);
            g.len() - 1
        };
        unsafe {
            sys::lv_obj_add_event_cb(self.raw, Some(on_change_trampoline), code, idx as *mut std::ffi::c_void);
        }
    }

    /// An ask dialog's footer button (OK / Cancel).
    pub fn msgbox_add_ask_button(&self, label: &str, accept: bool, sink: Sender<UiEvent>) -> Obj<'ui> {
        let c = CString::new(label).unwrap_or_else(|_| CString::new("").unwrap());
        let raw = unsafe { sys::lv_msgbox_add_footer_button(self.raw, c.as_ptr()) };
        assert!(!raw.is_null());
        let idx = {
            let mut g = bindings().lock().unwrap();
            g.push(Binding::AskButton { accept, sink });
            g.len() - 1
        };
        unsafe {
            sys::lv_obj_add_event_cb(raw, Some(on_tap_trampoline), sys::lv_event_code_t_LV_EVENT_CLICKED, idx as *mut std::ffi::c_void);
        }
        Obj { raw, _ui: PhantomData }
    }

    /// `on_change` / `on_submit` calling `action`: the widget's value
    /// goes along as the `value` and `event.value` args, after `args`.
    /// Fires when an inline `set` would (see the `on_change_*`
    /// methods).
    pub fn on_change_action(&self, widget: ValueWidget, sink: Sender<UiEvent>, action: String, args: Vec<ActionArg>) {
        let (kind, code) = match widget {
            ValueWidget::Toggle => (ChangeKind::Toggle, sys::lv_event_code_t_LV_EVENT_VALUE_CHANGED),
            ValueWidget::Slider => (ChangeKind::Slider, sys::lv_event_code_t_LV_EVENT_RELEASED),
            ValueWidget::Dropdown => (ChangeKind::Select, sys::lv_event_code_t_LV_EVENT_VALUE_CHANGED),
            ValueWidget::Roller => (ChangeKind::Roller, sys::lv_event_code_t_LV_EVENT_VALUE_CHANGED),
            ValueWidget::Buttons => (ChangeKind::Buttons, sys::lv_event_code_t_LV_EVENT_VALUE_CHANGED),
            ValueWidget::Keypad => (ChangeKind::Keypad, sys::lv_event_code_t_LV_EVENT_READY),
        };
        let idx = {
            let mut g = bindings().lock().unwrap();
            g.push(Binding::ChangeAction { kind, action, args, sink });
            g.len() - 1
        };
        unsafe {
            sys::lv_obj_add_event_cb(self.raw, Some(on_change_trampoline), code, idx as *mut std::ffi::c_void);
        }
    }

    /// A full-screen layer for a dialog, on top of what is drawn: it
    /// takes every tap (the page under it stays as it is, inert), and
    /// looks as the theme's `dialog_backdrop` says ([`set_dialog_backdrop`]).
    /// The dialog goes inside it.
    pub fn new_dialog_layer(&self) -> Obj<'ui> {
        // One dialog at a time: the next (ask, then PIN, then confirm;
        // a wrong PIN asked again) replaces the one before.
        let old = DIALOG.with(|d| d.replace(std::ptr::null_mut()));
        if !old.is_null() {
            unsafe { sys::lv_obj_delete(old) };
        }
        let layer = self.new_container();
        DIALOG.with(|d| d.set(layer.raw));
        unsafe extern "C" fn forget(e: *mut sys::lv_event_t) {
            // SAFETY: the target is the layer being deleted.
            let target = unsafe { sys::lv_event_get_target(e) } as *mut sys::lv_obj_t;
            DIALOG.with(|d| {
                if d.get() == target {
                    d.set(std::ptr::null_mut());
                }
            });
        }
        unsafe { sys::lv_obj_add_event_cb(layer.raw, Some(forget), sys::lv_event_code_t_LV_EVENT_DELETE, std::ptr::null_mut()) };
        layer.set_size(Size::Percent(100), Size::Percent(100));
        layer.set_padding(0);
        layer.set_radius(0);
        unsafe {
            sys::lv_obj_set_style_border_width(layer.raw, 0, 0);
            sys::lv_obj_set_pos(layer.raw, 0, 0);
            sys::lv_obj_set_clickable(layer.raw, true);
            sys::lv_obj_set_scrollable(layer.raw, false);
            match DIALOG_BACKDROP.with(|b| b.get()) {
                Backdrop::Transparent => {
                    sys::lv_obj_set_style_bg_opa(layer.raw, sys::_lv_opacity_level_t_LV_OPA_TRANSP as sys::lv_opa_t, 0);
                }
                Backdrop::Color(rgb, alpha) => {
                    sys::lv_obj_set_style_bg_color(layer.raw, sys::lv_color_hex(rgb), 0);
                    sys::lv_obj_set_style_bg_opa(layer.raw, alpha, 0);
                }
                Backdrop::Blur(radius) => {
                    // The backdrop blur needs a part that is drawn but not
                    // opaque.
                    sys::lv_obj_set_style_bg_color(layer.raw, sys::lv_color_hex(0), 0);
                    sys::lv_obj_set_style_bg_opa(layer.raw, 1, 0);
                    sys::lv_obj_set_style_blur_backdrop(layer.raw, true, 0);
                    sys::lv_obj_set_style_blur_radius(layer.raw, radius, 0);
                }
            }
        }
        layer
    }

    /// A modal message box (schema's action `confirm`). Call
    /// `add_title`, `add_text` and `add_confirm_button` / `add_cancel_button`
    /// to flesh it out. `parent = screen` makes it an overlay.
    pub fn new_msgbox(&self) -> Obj<'ui> {
        let raw = unsafe { sys::lv_msgbox_create(self.raw) };
        assert!(!raw.is_null());
        Obj { raw, _ui: PhantomData }
    }

    pub fn msgbox_add_title(&self, text: &str) {
        let c = CString::new(text).unwrap_or_else(|_| CString::new("").unwrap());
        unsafe { sys::lv_msgbox_add_title(self.raw, c.as_ptr()) };
    }

    /// The msgbox's content area, for adding custom children (the
    /// PIN modal's textarea lives here).
    pub fn msgbox_content(&self) -> Obj<'ui> {
        let raw = unsafe { sys::lv_msgbox_get_content(self.raw) };
        assert!(!raw.is_null());
        Obj { raw, _ui: PhantomData }
    }

    pub fn msgbox_add_text(&self, text: &str) {
        let c = CString::new(text).unwrap_or_else(|_| CString::new("").unwrap());
        unsafe { sys::lv_msgbox_add_text(self.raw, c.as_ptr()) };
    }

    /// Add a footer button to a msgbox and register a `Confirm`
    /// binding for `accept`. Returns the created button so the
    /// caller can style it.
    pub fn msgbox_add_confirm_button(
        &self,
        label: &str,
        accept: bool,
        sink: Sender<UiEvent>,
    ) -> Obj<'ui> {
        let c = CString::new(label).unwrap_or_else(|_| CString::new("").unwrap());
        let raw = unsafe { sys::lv_msgbox_add_footer_button(self.raw, c.as_ptr()) };
        assert!(!raw.is_null());
        let btn = Obj { raw, _ui: PhantomData };
        let idx = {
            let mut g = bindings().lock().unwrap();
            g.push(Binding::Confirm { accept, sink });
            g.len() - 1
        };
        unsafe {
            sys::lv_obj_add_event_cb(
                raw,
                Some(on_tap_trampoline),
                sys::lv_event_code_t_LV_EVENT_CLICKED,
                idx as *mut std::ffi::c_void,
            );
        }
        btn
    }

    /// A keyboard, pinned to numeric mode for the `keypad` widget.
    pub fn new_number_keyboard(&self) -> Obj<'ui> {
        let raw = unsafe { sys::lv_keyboard_create(self.raw) };
        assert!(!raw.is_null());
        unsafe {
            // Digits only: LVGL's own number map also has `+/-`, `.`
            // and cursor keys, which a PIN or digit keypad cannot use.
            sys::lv_keyboard_set_map(
                raw,
                sys::lv_keyboard_mode_t_LV_KEYBOARD_MODE_NUMBER,
                DIGIT_MAP.0.as_ptr(),
                DIGIT_CTRL.as_ptr(),
            );
            sys::lv_keyboard_set_mode(raw, sys::lv_keyboard_mode_t_LV_KEYBOARD_MODE_NUMBER);
        }
        Obj { raw, _ui: PhantomData }
    }

    /// Attach the keyboard self references to the given textarea so
    /// keypad presses populate it.
    pub fn attach_keyboard_to(&self, textarea: &Obj<'ui>) {
        unsafe { sys::lv_keyboard_set_textarea(self.raw, textarea.raw) };
    }

    /// A horizontal slider (schema `slider`). Range and value set
    /// through [`Self::set_slider_range`] / [`Self::set_slider_value`].
    pub fn new_slider(&self) -> Obj<'ui> {
        let raw = unsafe { sys::lv_slider_create(self.raw) };
        assert!(!raw.is_null());
        Obj { raw, _ui: PhantomData }
    }

    /// Keep `label` showing this slider's value as it moves.
    pub fn show_value_in(&self, label: &Obj<'ui>) {
        unsafe extern "C" fn update(e: *mut sys::lv_event_t) {
            // SAFETY: registered below on a slider with its label (a
            // sibling in the same dialog, deleted with it) as user data.
            unsafe {
                let slider = sys::lv_event_get_target(e) as *mut sys::lv_obj_t;
                let label = sys::lv_event_get_user_data(e) as *mut sys::lv_obj_t;
                let text = std::ffi::CString::new(sys::lv_slider_get_value(slider).to_string()).unwrap_or_default();
                sys::lv_label_set_text(label, text.as_ptr());
            }
        }
        unsafe {
            sys::lv_obj_add_event_cb(self.raw, Some(update), sys::lv_event_code_t_LV_EVENT_VALUE_CHANGED, label.raw as *mut std::ffi::c_void);
        }
    }

    pub fn set_slider_range(&self, min: i32, max: i32) {
        unsafe { sys::lv_slider_set_range(self.raw, min, max) };
    }

    pub fn set_slider_value(&self, v: i32) {
        unsafe { sys::lv_slider_set_value(self.raw, v, false) };
    }


    /// Set a bar's current fill value (no animation).
    pub fn set_bar_value(&self, v: i32) {
        unsafe { sys::lv_bar_set_value(self.raw, v, false) };
    }

    /// Set a bar's min / max range.
    pub fn set_bar_range(&self, min: i32, max: i32) {
        unsafe { sys::lv_bar_set_range(self.raw, min, max) };
    }

    /// Set an arc's current value.
    pub fn set_arc_value(&self, v: i32) {
        unsafe { sys::lv_arc_set_value(self.raw, v) };
    }

    /// Set an arc's min / max range.
    pub fn set_arc_range(&self, min: i32, max: i32) {
        unsafe { sys::lv_arc_set_range(self.raw, min, max) };
    }

    /// Where an arc's track starts and ends, in degrees: 0 is 3
    /// o'clock, clockwise. A span that is a non-zero multiple of 360
    /// is a full circle.
    pub fn set_arc_angles(&self, start: i32, end: i32) {
        let span = arc_span(start, end);
        unsafe {
            // Turned to `start`, the track runs from 0 to the span, so
            // a full circle from any start stays one.
            sys::lv_arc_set_rotation(self.raw, start.rem_euclid(360));
            sys::lv_arc_set_bg_angles(self.raw, 0, span);
        }
    }

    /// Width of an arc's track and indicator, in px.
    pub fn set_arc_thickness(&self, px: i32) {
        let indicator = sys::lv_part_t_LV_PART_INDICATOR as sys::lv_style_selector_t;
        unsafe {
            sys::lv_obj_set_style_arc_width(self.raw, px, sel());
            sys::lv_obj_set_style_arc_width(self.raw, px, sel() | indicator);
        }
    }

    /// Round (or square) ends of an arc's track and indicator.
    pub fn set_arc_rounded(&self, rounded: bool) {
        let indicator = sys::lv_part_t_LV_PART_INDICATOR as sys::lv_style_selector_t;
        unsafe {
            sys::lv_obj_set_style_arc_rounded(self.raw, rounded, sel());
            sys::lv_obj_set_style_arc_rounded(self.raw, rounded, sel() | indicator);
        }
    }

    /// An arc's track (the background arc) in `rgb` at `opa`.
    pub fn set_arc_track(&self, rgb: u32, opa: u8) {
        unsafe {
            sys::lv_obj_set_style_arc_color(self.raw, sys::lv_color_hex(rgb), sel());
            sys::lv_obj_set_style_arc_opa(self.raw, opa, sel());
        }
    }

    /// An arc's indicator (the value) in `rgb` at `opa`.
    pub fn set_arc_indicator(&self, rgb: u32, opa: u8) {
        let indicator = sys::lv_part_t_LV_PART_INDICATOR as sys::lv_style_selector_t;
        unsafe {
            sys::lv_obj_set_style_arc_color(self.raw, sys::lv_color_hex(rgb), sel() | indicator);
            sys::lv_obj_set_style_arc_opa(self.raw, opa, sel() | indicator);
        }
    }

    /// A bar's track (the unfilled part) in `rgb` at `opa`.
    pub fn set_bar_track(&self, rgb: u32, opa: u8) {
        self.set_background_hex(rgb, opa);
    }

    /// A bar's indicator (the filled part) in `rgb` at `opa`.
    pub fn set_bar_indicator(&self, rgb: u32, opa: u8) {
        let indicator = sys::lv_part_t_LV_PART_INDICATOR as sys::lv_style_selector_t;
        unsafe {
            sys::lv_obj_set_style_bg_color(self.raw, sys::lv_color_hex(rgb), sel() | indicator);
            sys::lv_obj_set_style_bg_opa(self.raw, opa, sel() | indicator);
        }
    }

    /// Fill a bar bottom to top. Its theme size (wide and low) is
    /// turned upright; a `width` / `height` set after wins.
    pub fn set_bar_vertical(&self) {
        unsafe {
            sys::lv_bar_set_orientation(self.raw, sys::lv_bar_orientation_t_LV_BAR_ORIENTATION_VERTICAL);
            let (w, h) = (sys::lv_obj_get_style_width(self.raw, 0), sys::lv_obj_get_style_height(self.raw, 0));
            sys::lv_obj_set_size(self.raw, h, w);
        }
    }

    /// A chart without LVGL's white box: transparent, borderless, its
    /// division lines in `lines` (rgb, alpha). The node's own
    /// `background` / `border` (applied after) still win.
    pub fn set_chart_look(&self, lines: (u32, u8)) {
        unsafe {
            sys::lv_obj_set_style_bg_opa(self.raw, sys::_lv_opacity_level_t_LV_OPA_TRANSP as sys::lv_opa_t, 0);
            sys::lv_obj_set_style_border_width(self.raw, 0, 0);
            sys::lv_obj_set_style_line_color(self.raw, sys::lv_color_hex(lines.0), 0);
            sys::lv_obj_set_style_line_opa(self.raw, lines.1, 0);
        }
    }

    /// Place this object in its parent (a `stack`) at `anchor`, moved
    /// by the `x` / `y` offset set with [`Self::set_x`] / [`Self::set_y`].
    pub fn set_anchor(&self, anchor: Anchor) {
        unsafe { sys::lv_obj_set_align(self.raw, anchor.lvgl()) };
    }

    /// Offset from the anchor point, px (right is positive) or a
    /// percentage of the parent's width.
    pub fn set_x(&self, x: Size) {
        unsafe { sys::lv_obj_set_x(self.raw, x.to_lvgl()) };
    }

    /// Offset from the anchor point, px (down is positive) or a
    /// percentage of the parent's height.
    pub fn set_y(&self, y: Size) {
        unsafe { sys::lv_obj_set_y(self.raw, y.to_lvgl()) };
    }

    /// Padding per side, px.
    pub fn set_padding_sides(&self, top: i32, right: i32, bottom: i32, left: i32) {
        unsafe {
            sys::lv_obj_set_style_pad_top(self.raw, top, sel());
            sys::lv_obj_set_style_pad_right(self.raw, right, sel());
            sys::lv_obj_set_style_pad_bottom(self.raw, bottom, sel());
            sys::lv_obj_set_style_pad_left(self.raw, left, sel());
        }
    }

    /// Space between rows (of a grid, or of wrapped flex tracks).
    pub fn set_row_gap(&self, px: i32) {
        unsafe { sys::lv_obj_set_style_pad_row(self.raw, px, sel()) };
    }

    /// Space between columns.
    pub fn set_column_gap(&self, px: i32) {
        unsafe { sys::lv_obj_set_style_pad_column(self.raw, px, sel()) };
    }

    /// `align = stretch` on a row / column (`column`: its children's
    /// width, else their height): each child whose size across the
    /// flow is not set in px or % fills it. Call it after the children
    /// exist.
    pub fn stretch_children(&self, column: bool) {
        let prop = if column { sys::_lv_style_id_t_LV_STYLE_WIDTH } else { sys::_lv_style_id_t_LV_STYLE_HEIGHT };
        let full = Size::Percent(100).to_lvgl();
        unsafe {
            for i in 0..sys::lv_obj_get_child_count(self.raw) {
                let child = sys::lv_obj_get_child(self.raw, i as i32);
                // Its own `align_self` wins.
                if local_self_align(child).is_some() {
                    continue;
                }
                let mut v = sys::lv_style_value_t::default();
                let found = sys::lv_obj_get_local_style_prop(child, prop as sys::lv_style_prop_t, &mut v, 0);
                // SAFETY: a size is stored as `num`.
                if found == sys::lv_style_res_t_LV_STYLE_RES_FOUND && v.num != LV_SIZE_CONTENT_LOCAL {
                    continue;
                }
                if column {
                    sys::lv_obj_set_width(child, full);
                } else {
                    sys::lv_obj_set_height(child, full);
                }
            }
        }
    }

    /// Let a row / column wrap its children onto more lines (only
    /// with a fixed main size: LVGL does not wrap a content-sized one).
    pub fn set_flex_wrap(&self, column: bool) {
        let flow = if column {
            sys::lv_flex_flow_t_LV_FLEX_FLOW_COLUMN_WRAP
        } else {
            sys::lv_flex_flow_t_LV_FLEX_FLOW_ROW_WRAP
        };
        unsafe { sys::lv_obj_set_flex_flow(self.raw, flow) };
    }

    /// What a label does with text longer than its width. See
    /// [`Self::fit_dots`] for `dots`.
    pub fn set_text_overflow(&self, mode: TextOverflow) {
        let m = match mode {
            TextOverflow::Wrap => sys::lv_label_long_mode_t_LV_LABEL_LONG_MODE_WRAP,
            TextOverflow::Clip => sys::lv_label_long_mode_t_LV_LABEL_LONG_MODE_CLIP,
            TextOverflow::Dots => sys::lv_label_long_mode_t_LV_LABEL_LONG_MODE_DOTS,
            TextOverflow::Scroll => sys::lv_label_long_mode_t_LV_LABEL_LONG_MODE_SCROLL,
        };
        unsafe { sys::lv_label_set_long_mode(self.raw, m) };
    }

    /// A `dots` label keeps to one line unless its height is set (by
    /// the node, a theme style or a component call): call it once all
    /// of those are applied.
    pub fn fit_dots(&self) {
        unsafe {
            if sys::lv_label_get_long_mode(self.raw) != sys::lv_label_long_mode_t_LV_LABEL_LONG_MODE_DOTS {
                return;
            }
            let content = sys::lv_obj_get_style_height(self.raw, 0) == LV_SIZE_CONTENT_LOCAL;
            sys::lv_label_set_max_lines(self.raw, if content { 1 } else { -1 });
        }
    }

    /// Put this object in grid cell `col`, `row` (from 0), spanning
    /// `colspan` x `rowspan` cells, once its parent is laid out as a
    /// grid ([`Self::layout_grid`]).
    pub fn set_grid_cell(&self, col: u8, row: u8, colspan: u8, rowspan: u8) {
        unsafe {
            sys::lv_obj_set_style_grid_cell_column_pos(self.raw, col as i32, 0);
            sys::lv_obj_set_style_grid_cell_row_pos(self.raw, row as i32, 0);
            sys::lv_obj_set_style_grid_cell_column_span(self.raw, colspan.max(1) as i32, 0);
            sys::lv_obj_set_style_grid_cell_row_span(self.raw, rowspan.max(1) as i32, 0);
        }
    }

    /// `align_self`: this object's place across its parent's flow (a
    /// row's height, a column's width), or in its grid cell vertically.
    /// Kept as the object's grid cell y align, which a flex parent
    /// reads after each layout; `stretch` in a flex parent is the full
    /// cross size.
    pub fn set_align_self(&self, align: SelfAlign) {
        /// After a flex layout, move each child with an `align_self`
        /// across the flow by a translate: LVGL flex has no per-child
        /// cross alignment. Setting it lays out again, and the second
        /// pass finds nothing to change.
        unsafe extern "C" fn place(e: *mut sys::lv_event_t) {
            // SAFETY: registered below on a live flex container; its
            // children are live objects for the length of the event.
            unsafe {
                let cont = sys::lv_event_get_target(e) as *mut sys::lv_obj_t;
                if sys::lv_obj_get_style_layout(cont, 0) as u32 != sys::lv_layout_t_LV_LAYOUT_FLEX {
                    return;
                }
                let column = sys::lv_obj_get_style_flex_flow(cont, 0) as u32 & sys::_LV_FLEX_COLUMN != 0;
                let mut area: sys::lv_area_t = std::mem::zeroed();
                sys::lv_obj_get_content_coords(cont, &mut area);
                let (lo, hi) = if column { (area.x1, area.x2) } else { (area.y1, area.y2) };
                for i in 0..sys::lv_obj_get_child_count(cont) {
                    let child = sys::lv_obj_get_child(cont, i as i32);
                    let Some(align) = local_self_align(child).filter(|a| *a != SelfAlign::Stretch) else { continue };
                    let mut c: sys::lv_area_t = std::mem::zeroed();
                    sys::lv_obj_get_coords(child, &mut c);
                    let (start, end) = if column { (c.x1, c.x2) } else { (c.y1, c.y2) };
                    let size = end - start + 1;
                    let shift = if column {
                        sys::lv_obj_get_style_translate_x(child, 0)
                    } else {
                        sys::lv_obj_get_style_translate_y(child, 0)
                    };
                    let target = match align {
                        SelfAlign::Center => lo + (hi - lo + 1 - size) / 2,
                        SelfAlign::End => hi + 1 - size,
                        _ => lo,
                    };
                    let want = target - (start - shift);
                    if want != shift {
                        if column {
                            sys::lv_obj_set_style_translate_x(child, want, 0);
                        } else {
                            sys::lv_obj_set_style_translate_y(child, want, 0);
                        }
                    }
                }
            }
        }
        unsafe {
            sys::lv_obj_set_style_grid_cell_y_align(self.raw, align.grid(), 0);
            let parent = sys::lv_obj_get_parent(self.raw);
            if parent.is_null() || sys::lv_obj_get_style_layout(parent, 0) as u32 != sys::lv_layout_t_LV_LAYOUT_FLEX {
                return;
            }
            let column = sys::lv_obj_get_style_flex_flow(parent, 0) as u32 & sys::_LV_FLEX_COLUMN != 0;
            if align == SelfAlign::Stretch {
                let full = Size::Percent(100).to_lvgl();
                if column {
                    sys::lv_obj_set_width(self.raw, full);
                } else {
                    sys::lv_obj_set_height(self.raw, full);
                }
                return;
            }
            // Once per container, found by its user data.
            let mark = &PLACE_MARK as *const u8 as *mut std::ffi::c_void;
            let has = (0..sys::lv_obj_get_event_count(parent))
                .any(|i| sys::lv_event_dsc_get_user_data(sys::lv_obj_get_event_dsc(parent, i)) == mark);
            if !has {
                sys::lv_obj_add_event_cb(parent, Some(place), sys::lv_event_code_t_LV_EVENT_LAYOUT_CHANGED, mark);
            }
        }
    }

    pub fn set_size(&self, w: Size, h: Size) {
        unsafe { sys::lv_obj_set_size(self.raw, w.to_lvgl(), h.to_lvgl()) };
    }

    pub fn set_width(&self, w: Size) {
        unsafe { sys::lv_obj_set_width(self.raw, w.to_lvgl()) };
    }

    pub fn set_height(&self, h: Size) {
        unsafe { sys::lv_obj_set_height(self.raw, h.to_lvgl()) };
    }

    pub fn set_min_width(&self, w: Size) {
        unsafe { sys::lv_obj_set_style_min_width(self.raw, w.to_lvgl(), sel()) };
    }

    pub fn set_max_width(&self, w: Size) {
        unsafe { sys::lv_obj_set_style_max_width(self.raw, w.to_lvgl(), sel()) };
    }

    pub fn set_min_height(&self, h: Size) {
        unsafe { sys::lv_obj_set_style_min_height(self.raw, h.to_lvgl(), sel()) };
    }

    pub fn set_max_height(&self, h: Size) {
        unsafe { sys::lv_obj_set_style_max_height(self.raw, h.to_lvgl(), sel()) };
    }

    /// Scroll only along one axis (`scroll`), snapping children to
    /// `snap` when a scroll ends.
    pub fn set_scroll(&self, horizontal: bool, snap: ScrollSnap) {
        let snap = match snap {
            ScrollSnap::None => sys::lv_scroll_snap_t_LV_SCROLL_SNAP_NONE,
            ScrollSnap::Start => sys::lv_scroll_snap_t_LV_SCROLL_SNAP_START,
            ScrollSnap::Center => sys::lv_scroll_snap_t_LV_SCROLL_SNAP_CENTER,
        };
        unsafe {
            if horizontal {
                sys::lv_obj_set_scroll_dir(self.raw, sys::lv_dir_t_LV_DIR_HOR as sys::lv_dir_t);
                sys::lv_obj_set_scroll_snap_x(self.raw, snap);
            } else {
                sys::lv_obj_set_scroll_dir(self.raw, sys::lv_dir_t_LV_DIR_VER as sys::lv_dir_t);
                sys::lv_obj_set_scroll_snap_y(self.raw, snap);
            }
        }
    }

    pub fn set_flex_row(&self) {
        unsafe { sys::lv_obj_set_flex_flow(self.raw, sys::lv_flex_flow_t_LV_FLEX_FLOW_ROW) };
    }

    pub fn set_flex_column(&self) {
        unsafe { sys::lv_obj_set_flex_flow(self.raw, sys::lv_flex_flow_t_LV_FLEX_FLOW_COLUMN) };
    }

    /// Share of the free space along the parent's flex axis.
    pub fn set_flex_grow(&self, grow: u8) {
        unsafe { sys::lv_obj_set_flex_grow(self.raw, grow) };
    }

    /// Main-axis placement (`justify`) and cross-axis placement
    /// (`align`) of this flex container's children.
    pub fn set_flex_align(&self, main: FlexAlign, cross: FlexAlign) {
        // Spacing modes only exist on the main axis; LVGL leaves them
        // undefined across it.
        let cross = match cross {
            FlexAlign::Between | FlexAlign::Around | FlexAlign::Evenly => FlexAlign::Start,
            other => other,
        };
        unsafe {
            sys::lv_obj_set_flex_align(self.raw, main.lvgl(), cross.lvgl(), cross.lvgl())
        };
    }

    pub fn set_padding(&self, px: i32) {
        unsafe { sys::lv_obj_set_style_pad_all(self.raw, px, sel()) };
    }

    pub fn set_gap(&self, px: i32) {
        unsafe { sys::lv_obj_set_style_pad_gap(self.raw, px, sel()) };
    }

    /// Background in `rgb` at `opa` (255 opaque, from `#rrggbbaa`).
    pub fn set_background_hex(&self, rgb: u32, opa: u8) {
        let color = unsafe { sys::lv_color_hex(rgb) };
        unsafe {
            sys::lv_obj_set_style_bg_color(self.raw, color, sel());
            // Layout containers start transparent.
            sys::lv_obj_set_style_bg_opa(self.raw, opa, sel());
        }
    }

    pub fn set_radius(&self, px: i32) {
        unsafe { sys::lv_obj_set_style_radius(self.raw, px, sel()) };
    }

    /// `border = <width> [<color>]`; width 0 removes the border. The
    /// color is (rgb, alpha).
    pub fn set_border(&self, width: i32, color: Option<(u32, u8)>) {
        unsafe {
            sys::lv_obj_set_style_border_width(self.raw, width, sel());
            if let Some((rgb, opa)) = color {
                sys::lv_obj_set_style_border_color(self.raw, sys::lv_color_hex(rgb), sel());
                sys::lv_obj_set_style_border_opa(self.raw, opa, sel());
            }
        }
    }

    /// `scale`: 1.0 is the node's own size (a pressed button at 0.96).
    pub fn set_scale(&self, scale: f64) {
        let v = (scale.clamp(0.1, 10.0) * 256.0).round() as i32;
        unsafe { sys::lv_obj_set_style_transform_scale(self.raw, v, sel()) };
    }

    /// 0.0 transparent .. 1.0 opaque.
    pub fn set_opacity(&self, opacity: f64) {
        let opa = (opacity.clamp(0.0, 1.0) * 255.0).round() as sys::lv_opa_t;
        unsafe { sys::lv_obj_set_style_opa(self.raw, opa, sel()) };
    }

    /// Text in `rgb` at `opa` (255 opaque).
    pub fn set_text_color_hex(&self, rgb: u32, opa: u8) {
        let color = unsafe { sys::lv_color_hex(rgb) };
        unsafe {
            sys::lv_obj_set_style_text_color(self.raw, color, sel());
            sys::lv_obj_set_style_text_opa(self.raw, opa, sel());
        }
    }

    /// The bitmap Montserrat nearest to `size` for this object (the
    /// sizes in `lv_conf.h`, [`paneld_config::icons::FONT_SIZES`]).
    /// Icons use it: those glyphs are only in the bitmap fonts.
    pub fn set_font_size(&self, size: u16) {
        unsafe { sys::lv_obj_set_style_text_font(self.raw, bitmap_font(size), sel()) };
    }

    /// Text in `font` (ADR 0015): a built-in or theme TTF drawn by
    /// TinyTTF, created on first use and kept (see [`FONTS`]). A font
    /// that cannot be loaded draws as the bitmap Montserrat nearest its
    /// size (logged once).
    pub fn set_font(&self, font: &paneld_config::fonts::Font) {
        let f = ttf_font(font).unwrap_or_else(|| bitmap_font(font.size));
        unsafe { sys::lv_obj_set_style_text_font(self.raw, f, sel()) };
    }

    /// Free every TTF font, so a reloaded config starts with an empty
    /// cache (and sees edited `.ttf` files). Call on the screen after
    /// its children and the top layer's are deleted: the fonts the
    /// screen and the top layer have themselves are reset here, and no
    /// other object may still point at one.
    pub fn release_fonts(&self) {
        let default = bitmap_font(16);
        unsafe {
            sys::lv_obj_set_style_text_font(self.raw, default, 0);
            sys::lv_obj_set_style_text_font(sys::lv_layer_top(), default, 0);
        }
        let entries = FONTS.with(|c| std::mem::take(&mut c.borrow_mut().entries));
        // Fonts falling back to others first; the bytes go after.
        for e in entries.into_iter().rev() {
            // SAFETY: made by `lv_tiny_ttf_create_data_ex`, and (per the
            // caller) used by no object; a font only falls back to one
            // made before it, which is destroyed after it.
            unsafe { sys::lv_tiny_ttf_destroy(e.font) };
        }
    }

    pub fn set_text_align_left(&self) {
        unsafe {
            sys::lv_obj_set_style_text_align(self.raw, sys::lv_text_align_t_LV_TEXT_ALIGN_LEFT, sel())
        };
    }
    pub fn set_text_align_center(&self) {
        unsafe {
            sys::lv_obj_set_style_text_align(
                self.raw,
                sys::lv_text_align_t_LV_TEXT_ALIGN_CENTER,
                sel(),
            )
        };
    }
    pub fn set_text_align_right(&self) {
        unsafe {
            sys::lv_obj_set_style_text_align(
                self.raw,
                sys::lv_text_align_t_LV_TEXT_ALIGN_RIGHT,
                sel(),
            )
        };
    }

    pub fn align_center(&self) {
        unsafe { sys::lv_obj_align(self.raw, sys::lv_align_t_LV_ALIGN_CENTER, 0, 0) };
    }

    pub fn child_count(&self) -> u32 {
        unsafe { sys::lv_obj_get_child_count(self.raw) }
    }

    /// The `i`th child, if there is one.
    pub fn child(&self, i: u32) -> Option<Obj<'ui>> {
        let raw = unsafe { sys::lv_obj_get_child(self.raw, i as i32) };
        (!raw.is_null()).then_some(Obj { raw, _ui: PhantomData })
    }

    /// Remove every child of this object but keep the object itself,
    /// so a caller can rebuild the tree in place. Used by the live
    /// re-render path when a source value changes.
    pub fn clear_children(&self) {
        unsafe { sys::lv_obj_clean(self.raw) };
        // With the whole screen cleared no grid is left that could
        // point at the track arrays.
        if self.raw == unsafe { sys::lv_screen_active() } {
            LAYOUT_MEM.with(|m| m.borrow_mut().clear());
            LINE_MEM.with(|m| m.borrow_mut().clear());
            CHART_ALPHA.with(|m| m.borrow_mut().clear());
        }
    }

    /// Lay this object's current children out as a grid: `cols` (a
    /// count of equal columns, or a list of tracks), `rows` (the same;
    /// by default rows sized to their content). A child with a cell
    /// ([`Self::set_grid_cell`]) goes there; the others fill the free
    /// cells row by row. Rows are added as the children need them.
    /// Call it after the children exist (so `repeat` output is counted).
    pub fn layout_grid(&self, cols: &Tracks, rows: Option<&Tracks>) {
        // Bounded: `columns` / `rows` can come from a source.
        const MAX_COLS: usize = 64;
        const MAX_ROWS: usize = 1024;
        let col_tracks: Vec<Track> = match cols {
            Tracks::Count(n) => vec![Track::Fr(1); (*n).clamp(1, MAX_COLS as i32) as usize],
            Tracks::List(l) if l.is_empty() => vec![Track::Fr(1)],
            Tracks::List(l) => l.iter().copied().take(MAX_COLS).collect(),
        };
        let ncols = col_tracks.len();
        // LVGL skips hidden children in a grid; place only the shown
        // ones so `visible = false` leaves no hole.
        let shown: Vec<*mut sys::lv_obj_t> = (0..unsafe { sys::lv_obj_get_child_count(self.raw) })
            .map(|i| unsafe { sys::lv_obj_get_child(self.raw, i as i32) })
            .filter(|&c| !c.is_null() && !unsafe { sys::lv_obj_is_hidden(c) })
            .collect();
        // Cells taken, row by row; explicit cells first, then the rest
        // in the free ones.
        let mut taken: Vec<Vec<bool>> = Vec::new();
        let take = |taken: &mut Vec<Vec<bool>>, c: usize, r: usize, cs: usize, rs: usize| {
            while taken.len() < r + rs {
                taken.push(vec![false; ncols]);
            }
            for row in taken.iter_mut().skip(r).take(rs) {
                for cell in row.iter_mut().skip(c).take(cs) {
                    *cell = true;
                }
            }
        };
        let mut placed: Vec<(usize, usize, usize, usize)> = vec![(0, 0, 1, 1); shown.len()];
        let explicit: Vec<Option<(i32, i32, i32, i32)>> = shown.iter().map(|&c| local_grid_cell(c)).collect();
        for (i, cell) in explicit.iter().enumerate() {
            if let Some((c, r, cs, rs)) = *cell {
                let c = (c.max(0) as usize).min(ncols - 1);
                let r = (r.max(0) as usize).min(MAX_ROWS - 1);
                let cs = (cs as usize).clamp(1, ncols - c);
                let rs = (rs as usize).clamp(1, MAX_ROWS - r);
                take(&mut taken, c, r, cs, rs);
                placed[i] = (c, r, cs, rs);
            }
        }
        let mut next = 0usize;
        for (i, cell) in explicit.iter().enumerate() {
            if cell.is_some() {
                continue;
            }
            while next / ncols < taken.len() && taken[next / ncols][next % ncols] {
                next += 1;
            }
            let (c, r) = (next % ncols, (next / ncols).min(MAX_ROWS - 1));
            take(&mut taken, c, r, 1, 1);
            placed[i] = (c, r, 1, 1);
            next += 1;
        }
        let needed = taken.len().clamp(1, MAX_ROWS);
        let mut row_tracks: Vec<Track> = match rows {
            Some(Tracks::Count(n)) => vec![Track::Content; (*n).clamp(1, MAX_ROWS as i32) as usize],
            Some(Tracks::List(l)) => l.iter().copied().take(MAX_ROWS).collect(),
            None => Vec::new(),
        };
        while row_tracks.len() < needed {
            row_tracks.push(Track::Content);
        }
        let mut col_dsc: Vec<i32> = col_tracks.iter().map(|t| t.lvgl()).collect();
        col_dsc.push(GRID_TEMPLATE_LAST);
        let mut row_dsc: Vec<i32> = row_tracks.iter().map(|t| t.lvgl()).collect();
        row_dsc.push(GRID_TEMPLATE_LAST);
        // LVGL keeps these pointers, so they live in LAYOUT_MEM until
        // the screen is cleared.
        let col_dsc = col_dsc.into_boxed_slice();
        let row_dsc = row_dsc.into_boxed_slice();
        let (col_ptr, row_ptr) = (col_dsc.as_ptr(), row_dsc.as_ptr());
        LAYOUT_MEM.with(|m| {
            let mut m = m.borrow_mut();
            m.push(col_dsc);
            m.push(row_dsc);
        });
        unsafe {
            sys::lv_obj_set_layout(self.raw, sys::lv_layout_t_LV_LAYOUT_GRID);
            sys::lv_obj_set_grid_dsc_array(self.raw, col_ptr, row_ptr);
            for (&child, &(c, r, cs, rs)) in shown.iter().zip(&placed) {
                let y_align = local_self_align(child).unwrap_or(SelfAlign::Stretch).grid();
                sys::lv_obj_set_grid_cell(
                    child,
                    sys::lv_grid_align_t_LV_GRID_ALIGN_STRETCH,
                    c as i32,
                    cs as i32,
                    y_align,
                    r as i32,
                    rs as i32,
                );
            }
        }
    }

    /// Mark this object clickable (plain `obj` and `label` need this;
    /// `button` has it on by default).
    pub fn set_clickable(&self) {
        unsafe { sys::lv_obj_set_clickable(self.raw, true) };
    }

    /// Hide / show the object.
    pub fn set_hidden(&self, hidden: bool) {
        unsafe { sys::lv_obj_set_hidden(self.raw, hidden) };
    }

    /// Enable / disable click handling.
    pub fn set_enabled(&self, enabled: bool) {
        unsafe { sys::lv_obj_set_clickable(self.raw, enabled) };
    }

    /// The `enabled` property (schema 8.1): disabled takes no input and
    /// is in LVGL's disabled state, so a theme's `:disabled` style shows.
    pub fn set_disabled(&self, disabled: bool) {
        self.set_enabled(!disabled);
        unsafe { sys::lv_obj_set_state(self.raw, sys::lv_state_t_LV_STATE_DISABLED as _, disabled) };
    }

    /// Put a button in its `:running` or `:failed` state (its action
    /// call runs, or its last run failed).
    pub fn set_run_state(&self, running: bool, failed: bool) {
        unsafe {
            sys::lv_obj_set_state(self.raw, sys::lv_state_t_LV_STATE_USER_1 as _, running);
            sys::lv_obj_set_state(self.raw, sys::lv_state_t_LV_STATE_USER_2 as _, failed);
        }
    }

    /// A spinner in place of a running button's content: as tall as
    /// the button's text and in its color (as styled now, so in its
    /// `:running` state), centered, over a faint track.
    pub fn new_busy_spinner(&self) -> Obj<'ui> {
        let main = sys::lv_part_t_LV_PART_MAIN as sys::lv_part_t;
        let (color, size) = unsafe {
            let font = sys::lv_obj_get_style_text_font(self.raw, main);
            let size = if font.is_null() { 24 } else { (*font).line_height };
            (sys::lv_obj_get_style_text_color(self.raw, main), size)
        };
        let raw = unsafe { sys::lv_spinner_create(self.raw) };
        assert!(!raw.is_null());
        let indicator = sys::lv_part_t_LV_PART_INDICATOR as sys::lv_style_selector_t;
        unsafe {
            sys::lv_obj_set_size(raw, size, size);
            sys::lv_obj_center(raw);
            sys::lv_obj_set_style_arc_width(raw, 3, 0);
            sys::lv_obj_set_style_arc_width(raw, 3, indicator);
            sys::lv_obj_set_style_arc_color(raw, color, 0);
            sys::lv_obj_set_style_arc_opa(raw, sys::_lv_opacity_level_t_LV_OPA_30 as sys::lv_opa_t, 0);
            sys::lv_obj_set_style_arc_color(raw, color, indicator);
        }
        Obj { raw, _ui: PhantomData }
    }

    /// On `trigger`, push a [`UiEvent::Action`] carrying `args`
    /// (already evaluated at render time; empty for `on_tap = name`).
    pub fn bind_action(
        &self,
        trigger: Trigger,
        sink: Sender<UiEvent>,
        action_name: &str,
        args: Vec<ActionArg>,
    ) {
        self.register_binding(
            trigger,
            Binding::Action { name: action_name.to_string(), args, sink },
        );
    }

    /// On `trigger`, push a [`UiEvent::Goto`].
    pub fn bind_goto(&self, trigger: Trigger, sink: Sender<UiEvent>, page: &str) {
        self.register_binding(trigger, Binding::Goto { page: page.to_string(), sink });
    }

    /// On `trigger`, push a [`UiEvent::InlineSet`]. The `assigns` are
    /// evaluated by the main loop, so the UI thread does no CEL work.
    pub fn bind_inline_set(
        &self,
        trigger: Trigger,
        sink: Sender<UiEvent>,
        assigns: Vec<InlineSetItem>,
    ) {
        self.register_binding(trigger, Binding::InlineSet { assigns, sink });
    }

    /// Register an LVGL VALUE_CHANGED callback on a toggle (switch)
    /// widget. On change the trampoline reads the switch's new state
    /// and pushes [`UiEvent::ChangeInlineSet`] on `sink`, with the
    /// state carried as `event.value`.
    pub fn on_change_toggle_inline_set(
        &self,
        sink: Sender<UiEvent>,
        assigns: Vec<InlineSetItem>,
    ) {
        self.register_change_binding(
            ChangeKind::Toggle,
            assigns,
            sink,
            sys::lv_event_code_t_LV_EVENT_VALUE_CHANGED,
        );
    }

    /// Register an LVGL RELEASED callback on a slider; on release the
    /// trampoline reads the slider's value and pushes an
    /// [`UiEvent::ChangeInlineSet`] with `event.value` as an int.
    /// RELEASED (not VALUE_CHANGED) matches the schema: on_change
    /// fires only when the user lets go of the knob.
    pub fn on_change_slider_inline_set(
        &self,
        sink: Sender<UiEvent>,
        assigns: Vec<InlineSetItem>,
    ) {
        self.register_change_binding(
            ChangeKind::Slider,
            assigns,
            sink,
            sys::lv_event_code_t_LV_EVENT_RELEASED,
        );
    }

    /// Register a VALUE_CHANGED callback on a dropdown; on pick, the
    /// trampoline reads the selected option and pushes an
    /// [`UiEvent::ChangeInlineSet`] with `event.value` as its text.
    pub fn on_change_dropdown_inline_set(
        &self,
        sink: Sender<UiEvent>,
        assigns: Vec<InlineSetItem>,
    ) {
        self.on_change_select_inline_set(ValueWidget::Dropdown, sink, assigns);
    }

    /// The same for any `select` style.
    pub fn on_change_select_inline_set(&self, widget: ValueWidget, sink: Sender<UiEvent>, assigns: Vec<InlineSetItem>) {
        let kind = match widget {
            ValueWidget::Roller => ChangeKind::Roller,
            ValueWidget::Buttons => ChangeKind::Buttons,
            _ => ChangeKind::Select,
        };
        self.register_change_binding(
            kind,
            assigns,
            sink,
            sys::lv_event_code_t_LV_EVENT_VALUE_CHANGED,
        );
    }

    /// Register a READY callback on the PIN modal's keyboard; on OK
    /// the trampoline pushes [`UiEvent::PinEntered`] with the digits.
    pub fn on_submit_pin(&self, sink: Sender<UiEvent>) {
        self.register_change_binding(
            ChangeKind::PinEntry,
            Vec::new(),
            sink,
            sys::lv_event_code_t_LV_EVENT_READY,
        );
    }

    /// Register a READY callback on a keyboard; on OK the trampoline
    /// reads the attached textarea's content and pushes an
    /// [`UiEvent::ChangeInlineSet`] with `event.value` as a string.
    pub fn on_submit_keyboard_inline_set(
        &self,
        sink: Sender<UiEvent>,
        assigns: Vec<InlineSetItem>,
    ) {
        self.register_change_binding(
            ChangeKind::Keypad,
            assigns,
            sink,
            sys::lv_event_code_t_LV_EVENT_READY,
        );
    }

    fn register_change_binding(
        &self,
        kind: ChangeKind,
        assigns: Vec<InlineSetItem>,
        sink: Sender<UiEvent>,
        event_code: sys::lv_event_code_t,
    ) {
        let idx = {
            let mut g = bindings().lock().unwrap();
            g.push(Binding::Change { kind, assigns, sink });
            g.len() - 1
        };
        unsafe {
            sys::lv_obj_add_event_cb(
                self.raw,
                Some(on_change_trampoline),
                event_code,
                idx as *mut std::ffi::c_void,
            );
        }
    }

    fn register_binding(&self, trigger: Trigger, binding: Binding) {
        let idx = {
            let mut g = bindings().lock().unwrap();
            g.push(binding);
            g.len() - 1
        };
        unsafe {
            sys::lv_obj_add_event_cb(
                self.raw,
                Some(on_tap_trampoline),
                trigger.code(),
                idx as *mut std::ffi::c_void,
            );
        }
    }
}

/// Flex placement, schema 8.2 `justify` / `align` values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlexAlign {
    Start,
    Center,
    End,
    Between,
    Around,
    Evenly,
}

impl FlexAlign {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim() {
            // LVGL flex has no cross-axis stretch; start is closest.
            "start" | "stretch" => FlexAlign::Start,
            "center" => FlexAlign::Center,
            "end" => FlexAlign::End,
            "between" => FlexAlign::Between,
            "around" => FlexAlign::Around,
            "evenly" => FlexAlign::Evenly,
            _ => return None,
        })
    }

    fn lvgl(self) -> sys::lv_flex_align_t {
        match self {
            FlexAlign::Start => sys::lv_flex_align_t_LV_FLEX_ALIGN_START,
            FlexAlign::Center => sys::lv_flex_align_t_LV_FLEX_ALIGN_CENTER,
            FlexAlign::End => sys::lv_flex_align_t_LV_FLEX_ALIGN_END,
            FlexAlign::Between => sys::lv_flex_align_t_LV_FLEX_ALIGN_SPACE_BETWEEN,
            FlexAlign::Around => sys::lv_flex_align_t_LV_FLEX_ALIGN_SPACE_AROUND,
            FlexAlign::Evenly => sys::lv_flex_align_t_LV_FLEX_ALIGN_SPACE_EVENLY,
        }
    }
}

/// The track of an arc from `start` to `end` degrees: 0 to 359, or
/// 360 for a span that is a non-zero multiple of it.
pub fn arc_span(start: i32, end: i32) -> i32 {
    let span = end.wrapping_sub(start);
    match span.rem_euclid(360) {
        0 if span != 0 => 360,
        s => s,
    }
}

/// The marker user data of the `align_self` layout hook.
static PLACE_MARK: u8 = 0;

/// The `align_self` kept on `obj` ([`Obj::set_align_self`]), if any.
fn local_self_align(obj: *mut sys::lv_obj_t) -> Option<SelfAlign> {
    let mut v = sys::lv_style_value_t::default();
    // SAFETY: `obj` is a live object (a child being laid out).
    let found = unsafe {
        sys::lv_obj_get_local_style_prop(obj, sys::_lv_style_id_t_LV_STYLE_GRID_CELL_Y_ALIGN as sys::lv_style_prop_t, &mut v, 0)
    };
    if found != sys::lv_style_res_t_LV_STYLE_RES_FOUND {
        return None;
    }
    // SAFETY: an align is stored as `num`.
    let n = unsafe { v.num } as u32;
    Some(match n {
        x if x == sys::lv_grid_align_t_LV_GRID_ALIGN_CENTER => SelfAlign::Center,
        x if x == sys::lv_grid_align_t_LV_GRID_ALIGN_END => SelfAlign::End,
        x if x == sys::lv_grid_align_t_LV_GRID_ALIGN_STRETCH => SelfAlign::Stretch,
        _ => SelfAlign::Start,
    })
}

/// The explicit grid cell set on `obj` ([`Obj::set_grid_cell`]):
/// column, row, column span, row span.
fn local_grid_cell(obj: *mut sys::lv_obj_t) -> Option<(i32, i32, i32, i32)> {
    let get = |prop: sys::_lv_style_id_t| {
        let mut v = sys::lv_style_value_t::default();
        // SAFETY: `obj` is a live child of the grid being laid out.
        let found = unsafe { sys::lv_obj_get_local_style_prop(obj, prop as sys::lv_style_prop_t, &mut v, 0) };
        // SAFETY: grid positions and spans are stored as `num` (the
        // default value is zeroed when not found).
        (found == sys::lv_style_res_t_LV_STYLE_RES_FOUND).then_some(unsafe { v.num })
    };
    Some((
        get(sys::_lv_style_id_t_LV_STYLE_GRID_CELL_COLUMN_POS)?,
        get(sys::_lv_style_id_t_LV_STYLE_GRID_CELL_ROW_POS)?,
        get(sys::_lv_style_id_t_LV_STYLE_GRID_CELL_COLUMN_SPAN).unwrap_or(1).max(1),
        get(sys::_lv_style_id_t_LV_STYLE_GRID_CELL_ROW_SPAN).unwrap_or(1).max(1),
    ))
}

/// A grid track (schema 8.2 `columns` / `rows` list items).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Track {
    Px(i32),
    /// A share of the free space (`1fr`, `2fr`).
    Fr(u8),
    /// Sized to its content.
    Content,
}

impl Track {
    /// `120`, `1fr`, `2fr`, `content`.
    pub fn parse(s: &str) -> Option<Track> {
        let s = s.trim();
        if s == "content" {
            return Some(Track::Content);
        }
        if let Some(n) = s.strip_suffix("fr") {
            let n: u8 = n.trim().parse().ok()?;
            return (1..=100).contains(&n).then_some(Track::Fr(n));
        }
        let px: i32 = s.trim_end_matches("px").parse().ok()?;
        (0..=10_000).contains(&px).then_some(Track::Px(px))
    }

    fn lvgl(self) -> i32 {
        match self {
            Track::Px(px) => px,
            Track::Fr(n) => grid_fr(n as i32),
            Track::Content => GRID_CONTENT,
        }
    }
}

/// Grid columns or rows: a count of equal tracks, or a list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Tracks {
    Count(i32),
    List(Vec<Track>),
}

/// `stack` child placement, schema 8.1 `anchor`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Anchor {
    TopLeft,
    TopCenter,
    TopRight,
    Left,
    Center,
    Right,
    BottomLeft,
    BottomCenter,
    BottomRight,
}

impl Anchor {
    pub fn parse(s: &str) -> Option<Anchor> {
        Some(match s.trim() {
            "top_left" => Anchor::TopLeft,
            "top_center" => Anchor::TopCenter,
            "top_right" => Anchor::TopRight,
            "left" => Anchor::Left,
            "center" => Anchor::Center,
            "right" => Anchor::Right,
            "bottom_left" => Anchor::BottomLeft,
            "bottom_center" => Anchor::BottomCenter,
            "bottom_right" => Anchor::BottomRight,
            _ => return None,
        })
    }

    fn lvgl(self) -> sys::lv_align_t {
        match self {
            Anchor::TopLeft => sys::lv_align_t_LV_ALIGN_TOP_LEFT,
            Anchor::TopCenter => sys::lv_align_t_LV_ALIGN_TOP_MID,
            Anchor::TopRight => sys::lv_align_t_LV_ALIGN_TOP_RIGHT,
            Anchor::Left => sys::lv_align_t_LV_ALIGN_LEFT_MID,
            Anchor::Center => sys::lv_align_t_LV_ALIGN_CENTER,
            Anchor::Right => sys::lv_align_t_LV_ALIGN_RIGHT_MID,
            Anchor::BottomLeft => sys::lv_align_t_LV_ALIGN_BOTTOM_LEFT,
            Anchor::BottomCenter => sys::lv_align_t_LV_ALIGN_BOTTOM_MID,
            Anchor::BottomRight => sys::lv_align_t_LV_ALIGN_BOTTOM_RIGHT,
        }
    }
}

/// Schema 8.1 `align_self`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelfAlign {
    Start,
    Center,
    End,
    Stretch,
}

impl SelfAlign {
    pub fn parse(s: &str) -> Option<SelfAlign> {
        Some(match s.trim() {
            "start" => SelfAlign::Start,
            "center" => SelfAlign::Center,
            "end" => SelfAlign::End,
            "stretch" => SelfAlign::Stretch,
            _ => return None,
        })
    }

    fn grid(self) -> sys::lv_grid_align_t {
        match self {
            SelfAlign::Start => sys::lv_grid_align_t_LV_GRID_ALIGN_START,
            SelfAlign::Center => sys::lv_grid_align_t_LV_GRID_ALIGN_CENTER,
            SelfAlign::End => sys::lv_grid_align_t_LV_GRID_ALIGN_END,
            SelfAlign::Stretch => sys::lv_grid_align_t_LV_GRID_ALIGN_STRETCH,
        }
    }
}

/// Schema 8.3 `text { overflow }`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextOverflow {
    Wrap,
    Clip,
    Dots,
    Scroll,
}

impl TextOverflow {
    pub fn parse(s: &str) -> Option<TextOverflow> {
        Some(match s.trim() {
            "wrap" => TextOverflow::Wrap,
            "clip" => TextOverflow::Clip,
            "dots" => TextOverflow::Dots,
            "scroll" => TextOverflow::Scroll,
            _ => return None,
        })
    }
}

/// Which pointer gesture fires a handler.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trigger {
    /// Any click, however long the press (`on_tap` on an object
    /// without `on_hold`).
    Tap,
    /// A click that was not a long press (`on_tap` next to `on_hold`;
    /// LVGL's CLICKED would also fire after a hold).
    ShortTap,
    /// A long press (`on_hold`).
    Hold,
}

impl Trigger {
    fn code(self) -> sys::lv_event_code_t {
        match self {
            Trigger::Tap => sys::lv_event_code_t_LV_EVENT_CLICKED,
            Trigger::ShortTap => sys::lv_event_code_t_LV_EVENT_SHORT_CLICKED,
            Trigger::Hold => sys::lv_event_code_t_LV_EVENT_LONG_PRESSED,
        }
    }
}

/// A size value for an LVGL object. Mirrors the schema's `Size` type
/// but at the engine side so the renderer does not need to re-parse.
#[derive(Clone, Copy, Debug)]
pub enum Size {
    Px(i32),
    /// Of the parent's size; negative only for an offset (`x`, `y`).
    Percent(i16),
    Content,
}

thread_local! {
    /// Each button row's chosen option, so a tap on it again is no
    /// change. Keyed by the widget's address; a new row there resets it.
    static ROW_CHOICE: std::cell::RefCell<std::collections::HashMap<usize, Option<u32>>> = std::cell::RefCell::new(std::collections::HashMap::new());
}

/// A button row's labels and the pointer map LVGL reads.
type ButtonLabels = (Vec<CString>, Vec<*const std::os::raw::c_char>);

thread_local! {
    /// Labels of deleted button rows, freed at the next pump step.
    static RETIRED_LABELS: std::cell::RefCell<Vec<ButtonLabels>> = const { std::cell::RefCell::new(Vec::new()) };
}

thread_local! {
    /// The style selector the property setters write to: the default
    /// state, or a state (`pressed`, ...) while a theme's `style
    /// button:pressed { }` is applied ([`with_state`]).
    static SELECTOR: std::cell::Cell<sys::lv_style_selector_t> = const { std::cell::Cell::new(0) };
}

/// The alpha of `chart`'s series number `idx` (255 when unknown).
fn series_alpha(chart: *mut sys::lv_obj_t, idx: u32) -> u8 {
    CHART_ALPHA.with(|m| {
        m.borrow()
            .iter()
            .find(|(c, _)| *c == chart)
            .and_then(|(_, a)| a.get(idx as usize).copied())
            .unwrap_or(255)
    })
}

/// `opa` scaled by `alpha` (both 0..=255).
fn scale_opa(opa: u8, alpha: u8) -> u8 {
    ((opa as u16 * alpha as u16 + 127) / 255) as u8
}

fn sel() -> sys::lv_style_selector_t {
    SELECTOR.with(|s| s.get())
}

/// A state style is being applied: properties LVGL cannot hold per
/// state (size, grow, visibility, enabled) must not be set.
pub fn in_state() -> bool {
    sel() != 0
}

/// The LVGL state for a style selector's `:state`.
fn state_of(name: &str) -> Option<sys::lv_style_selector_t> {
    Some(match name {
        "pressed" => sys::lv_state_t_LV_STATE_PRESSED,
        "checked" => sys::lv_state_t_LV_STATE_CHECKED,
        "disabled" => sys::lv_state_t_LV_STATE_DISABLED,
        "focused" => sys::lv_state_t_LV_STATE_FOCUSED,
        "hover" => sys::lv_state_t_LV_STATE_HOVERED,
        // A button's action call: running, or its last run failed.
        "running" => sys::lv_state_t_LV_STATE_USER_1,
        "failed" => sys::lv_state_t_LV_STATE_USER_2,
        _ => return None,
    } as sys::lv_style_selector_t)
}

/// Run `f` with the property setters writing to state `state`
/// (`pressed`, `checked`, `disabled`, `focused`, `hover`, `running`,
/// `failed`); does nothing
/// for an unknown state.
pub fn with_state(state: &str, f: impl FnOnce()) {
    let Some(s) = state_of(state) else { return };
    let before = SELECTOR.with(|c| c.replace(s));
    f();
    SELECTOR.with(|c| c.set(before));
}

/// The largest backdrop blur: the Pi draws it in software, over the
/// whole screen, on every change under the dialog.
pub const BLUR_MAX: i32 = 20;

/// How a dialog's backdrop looks (theme token `dialog_backdrop`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backdrop {
    /// The page shows through, unchanged.
    Transparent,
    /// RGB and opacity over the page.
    Color(u32, u8),
    /// The page blurred by this radius in px.
    Blur(i32),
}

impl Backdrop {
    /// `transparent`, `#rrggbb`, `#rrggbbaa` or `blur <px>`; `None` for
    /// anything else.
    pub fn parse(s: &str) -> Option<Backdrop> {
        let s = s.trim();
        if s == "transparent" || s.is_empty() {
            return Some(Backdrop::Transparent);
        }
        if let Some(px) = s.strip_prefix("blur") {
            let px: i32 = px.trim().trim_end_matches("px").parse().ok()?;
            return (0..=BLUR_MAX).contains(&px).then_some(Backdrop::Blur(px));
        }
        let hex = s.strip_prefix('#')?;
        if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
        match hex.len() {
            6 => Some(Backdrop::Color(u32::from_str_radix(hex, 16).ok()?, 255)),
            8 => Some(Backdrop::Color(u32::from_str_radix(&hex[..6], 16).ok()?, byte(6)?)),
            _ => None,
        }
    }
}

thread_local! {
    /// The open dialog's layer, null when none (cleared when LVGL
    /// deletes it, as a page redraw does).
    static DIALOG: std::cell::Cell<*mut sys::lv_obj_t> = const { std::cell::Cell::new(std::ptr::null_mut()) };
    static DIALOG_BACKDROP: std::cell::Cell<Backdrop> = const { std::cell::Cell::new(Backdrop::Transparent) };
}

/// The look of the next dialogs' backdrop, from the active theme.
pub fn set_dialog_backdrop(b: Backdrop) {
    DIALOG_BACKDROP.with(|c| c.set(b));
}

/// `scroll { snap = ... }`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollSnap {
    None,
    Start,
    Center,
}

impl Size {
    fn to_lvgl(self) -> i32 {
        match self {
            Size::Px(px) => px,
            Size::Percent(p) => {
                // LV_PCT(x): `x | LV_COORD_TYPE_SPEC` for x >= 0, and
                // `(LV_PCT_POS_MAX - x) | LV_COORD_TYPE_SPEC` below 0.
                const PCT_POS_MAX: i32 = (LV_COORD_MAX_LOCAL - 1) / 2;
                let p = (p as i32).clamp(-1000, 1000);
                let stored = if p < 0 { PCT_POS_MAX - p } else { p };
                stored | LV_COORD_TYPE_SPEC
            }
            Size::Content => LV_SIZE_CONTENT_LOCAL,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arc_spans_keep_a_full_circle() {
        assert_eq!(arc_span(135, 45), 270);
        assert_eq!(arc_span(0, 360), 360);
        assert_eq!(arc_span(90, 450), 360);
        assert_eq!(arc_span(-90, 270), 360);
        assert_eq!(arc_span(10, 10), 0);
        assert_eq!(arc_span(180, 0), 180);
    }
    use std::io::Write;
    use std::os::unix::io::IntoRawFd;

    /// One `struct input_event` (64-bit Linux): time, type, code, value.
    fn event(ty: u16, code: u16, value: i32) -> Vec<u8> {
        let mut e = vec![0u8; 16];
        e.extend_from_slice(&ty.to_ne_bytes());
        e.extend_from_slice(&code.to_ne_bytes());
        e.extend_from_slice(&value.to_ne_bytes());
        e
    }

    #[test]
    fn touch_is_read_when_it_arrives_and_idle_steps_sleep() {
        let ui = Ui::init().unwrap();
        let _display = ui.open_memory_display(320, 240);
        // `shot`'s scripted pointer reads on a timer; the panel has none.
        let scripted = unsafe { sys::lv_indev_get_next(std::ptr::null_mut()) };
        unsafe { sys::lv_indev_set_mode(scripted, sys::lv_indev_mode_t_LV_INDEV_MODE_EVENT) };
        // A socket stands in for the evdev device.
        let (ours, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
        assert!(add_pointer_fd(theirs.into_raw_fd()));
        let indev = TOUCH.with(|t| t.borrow()[0].1);
        let mut pump = ui.pump();
        // Settle LVGL's start-up work, then an idle step takes the
        // full idle time: no read timer runs in event mode.
        for _ in 0..5 {
            pump.step();
        }
        let t = Instant::now();
        pump.step();
        assert!(t.elapsed() >= Duration::from_millis(150), "an idle step woke early: {:?}", t.elapsed());
        // A press: read as soon as it is written.
        let (ev_syn, ev_key, ev_abs) = (0u16, 1u16, 3u16);
        let mut press = Vec::new();
        press.extend(event(ev_abs, 0, 100));
        press.extend(event(ev_abs, 1, 50));
        press.extend(event(ev_key, 0x14a, 1));
        press.extend(event(ev_syn, 0, 0));
        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            (&ours).write_all(&press).unwrap();
            ours
        });
        let t = Instant::now();
        pump.step();
        let ours = writer.join().unwrap();
        assert!(t.elapsed() < Duration::from_millis(150), "the touch did not wake the step: {:?}", t.elapsed());
        assert_eq!(unsafe { sys::lv_indev_get_state(indev) }, sys::lv_indev_state_t_LV_INDEV_STATE_PRESSED);
        // News for the main loop from another thread wakes the step.
        let waker = std::thread::spawn(|| {
            std::thread::sleep(Duration::from_millis(50));
            wake();
        });
        let t = Instant::now();
        pump.step();
        waker.join().unwrap();
        assert!(t.elapsed() < Duration::from_millis(150), "wake() did not wake the step: {:?}", t.elapsed());
        // The device going away: dropped, not polled in a busy loop.
        drop(ours);
        pump.step();
        assert!(TOUCH.with(|t| t.borrow().is_empty()), "a hung-up device is still polled");
        let t = Instant::now();
        for _ in 0..3 {
            pump.step();
        }
        assert!(t.elapsed() >= Duration::from_millis(300), "the loop spins after the device went away");
    }
}
