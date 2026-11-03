//! Safe wrapper over `lvgl-sys`. All `unsafe` for the UI module lives
//! here. Only this crate links to `lvgl_sys`.
//!
//! LVGL is single-threaded by design (ADR 0010): one [`Ui`] on the
//! main thread owns the global LVGL state. Dropping it calls
//! `lv_deinit`.

use std::ffi::CString;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicBool, Ordering};
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
    /// The PIN modal's keyboard OK was pressed. Carries the typed
    /// digits; the main loop verifies them and must never log them.
    PinEntered { digits: String },
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
}

#[derive(Clone, Copy, Debug)]
enum ChangeKind {
    Toggle,
    Slider,
    Dropdown,
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
        _ => {}
    }
}

unsafe extern "C" fn on_change_trampoline(e: *mut sys::lv_event_t) {
    let idx = unsafe { sys::lv_event_get_user_data(e) } as usize;
    let target = unsafe { sys::lv_event_get_target(e) } as *mut sys::lv_obj_t;
    let bind = {
        let guard = bindings().lock().unwrap();
        guard.get(idx).cloned()
    };
    if let Some(Binding::Change { kind, assigns, sink }) = bind {
        let event_value = match kind {
            ChangeKind::Toggle => {
                let on = unsafe {
                    sys::lv_obj_has_state(target, sys::lv_state_t_LV_STATE_CHECKED)
                };
                EventValue::Bool(on)
            }
            ChangeKind::Slider => {
                let v = unsafe { sys::lv_slider_get_value(target) };
                EventValue::Int(v as i64)
            }
            ChangeKind::Dropdown => {
                let v = unsafe { sys::lv_dropdown_get_selected(target) };
                EventValue::Int(v as i64)
            }
            ChangeKind::Keypad => EventValue::String(keyboard_text(target)),
            ChangeKind::PinEntry => {
                let digits = keyboard_text(target);
                let _ = sink.send(UiEvent::PinEntered { digits });
                return;
            }
        };
        let _ = sink.send(UiEvent::ChangeInlineSet { assigns, event_value });
    }
}

/// Read the text of the textarea attached to keyboard `kb`. Empty
/// when no textarea is linked.
fn keyboard_text(kb: *mut sys::lv_obj_t) -> String {
    let ta = unsafe { sys::lv_keyboard_get_textarea(kb) };
    if ta.is_null() {
        return String::new();
    }
    let cstr = unsafe { sys::lv_textarea_get_text(ta) };
    if cstr.is_null() {
        return String::new();
    }
    unsafe { std::ffi::CStr::from_ptr(cstr) }
        .to_string_lossy()
        .into_owned()
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
        let c_dev = match CString::new(device) {
            Ok(c) => c,
            Err(_) => return false,
        };
        let indev = unsafe {
            sys::lv_evdev_create(sys::lv_indev_type_t_LV_INDEV_TYPE_POINTER, c_dev.as_ptr())
        };
        !indev.is_null()
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
        let now = Instant::now();
        let elapsed = now.duration_since(self.last).as_millis() as u32;
        if elapsed > 0 {
            unsafe { sys::lv_tick_inc(elapsed) };
            self.last = now;
        }
        let sleep_ms = unsafe { sys::lv_timer_handler() };
        let sleep = Duration::from_millis(sleep_ms.min(100) as u64);
        std::thread::sleep(sleep);
        sleep
    }
}

impl Drop for Ui {
    fn drop(&mut self) {
        // SAFETY: paired with `lv_init` in `Ui::init`.
        unsafe { sys::lv_deinit() };
        LVGL_INITIALIZED.store(false, Ordering::SeqCst);
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

    /// A circular arc gauge (dial).
    pub fn new_arc(&self) -> Obj<'ui> {
        let raw = unsafe { sys::lv_arc_create(self.raw) };
        assert!(!raw.is_null());
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

    pub fn set_dropdown_selected(&self, idx: u16) {
        unsafe { sys::lv_dropdown_set_selected(self.raw, idx.into()) };
    }

    /// A textarea (schema `keypad`'s display line). `masked` turns on
    /// password mode so digits render as dots; `max_length` of 0 is
    /// treated as unlimited.
    pub fn new_textarea(&self, masked: bool, max_length: u32) -> Obj<'ui> {
        let raw = unsafe { sys::lv_textarea_create(self.raw) };
        assert!(!raw.is_null());
        unsafe {
            sys::lv_textarea_set_password_mode(raw, masked);
            sys::lv_textarea_set_one_line(raw, true);
            if max_length > 0 {
                sys::lv_textarea_set_max_length(raw, max_length);
            }
        }
        Obj { raw, _ui: PhantomData }
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

    pub fn set_size(&self, w: Size, h: Size) {
        unsafe { sys::lv_obj_set_size(self.raw, w.to_lvgl(), h.to_lvgl()) };
    }

    pub fn set_flex_row(&self) {
        unsafe { sys::lv_obj_set_flex_flow(self.raw, sys::lv_flex_flow_t_LV_FLEX_FLOW_ROW) };
    }

    pub fn set_flex_column(&self) {
        unsafe { sys::lv_obj_set_flex_flow(self.raw, sys::lv_flex_flow_t_LV_FLEX_FLOW_COLUMN) };
    }

    pub fn set_padding(&self, px: i32) {
        unsafe { sys::lv_obj_set_style_pad_all(self.raw, px, 0) };
    }

    pub fn set_gap(&self, px: i32) {
        unsafe { sys::lv_obj_set_style_pad_gap(self.raw, px, 0) };
    }

    pub fn set_background_hex(&self, rgb: u32) {
        let color = unsafe { sys::lv_color_hex(rgb) };
        unsafe { sys::lv_obj_set_style_bg_color(self.raw, color, 0) };
    }

    pub fn set_text_color_hex(&self, rgb: u32) {
        let color = unsafe { sys::lv_color_hex(rgb) };
        unsafe { sys::lv_obj_set_style_text_color(self.raw, color, 0) };
    }

    /// Pick one of the compiled Montserrat font sizes for this
    /// object. Returns `false` if the requested size isn't compiled
    /// into our LVGL build (see `lv_conf.h`).
    pub fn set_font_size(&self, size: u16) -> bool {
        let font: *const sys::lv_font_t = unsafe {
            match size {
                12 => &sys::lv_font_montserrat_12,
                14 => &sys::lv_font_montserrat_14,
                16 => &sys::lv_font_montserrat_16,
                20 => &sys::lv_font_montserrat_20,
                24 => &sys::lv_font_montserrat_24,
                32 => &sys::lv_font_montserrat_32,
                _ => return false,
            }
        };
        unsafe { sys::lv_obj_set_style_text_font(self.raw, font, 0) };
        true
    }

    pub fn set_text_align_left(&self) {
        unsafe {
            sys::lv_obj_set_style_text_align(self.raw, sys::lv_text_align_t_LV_TEXT_ALIGN_LEFT, 0)
        };
    }
    pub fn set_text_align_center(&self) {
        unsafe {
            sys::lv_obj_set_style_text_align(
                self.raw,
                sys::lv_text_align_t_LV_TEXT_ALIGN_CENTER,
                0,
            )
        };
    }
    pub fn set_text_align_right(&self) {
        unsafe {
            sys::lv_obj_set_style_text_align(
                self.raw,
                sys::lv_text_align_t_LV_TEXT_ALIGN_RIGHT,
                0,
            )
        };
    }

    pub fn align_center(&self) {
        unsafe { sys::lv_obj_align(self.raw, sys::lv_align_t_LV_ALIGN_CENTER, 0, 0) };
    }

    /// Remove every child of this object but keep the object itself,
    /// so a caller can rebuild the tree in place. Used by the live
    /// re-render path when a source value changes.
    pub fn clear_children(&self) {
        unsafe { sys::lv_obj_clean(self.raw) };
    }

    /// Make this object a grid container with `cols` equal fractional
    /// columns and `rows` equal fractional rows. The row and column
    /// descriptor arrays leak intentionally (they must outlive the
    /// LVGL object) -- children on a reload re-create the arrays and
    /// the old ones are collected by the OS at exit. The descriptor
    /// stays stable across the object's lifetime.
    pub fn set_grid(&self, cols: i32, rows: i32) {
        let cols = cols.max(1);
        let rows = rows.max(1);
        let mut col_dsc: Vec<i32> = (0..cols).map(|_| grid_fr(1)).collect();
        col_dsc.push(GRID_TEMPLATE_LAST);
        let mut row_dsc: Vec<i32> = (0..rows).map(|_| grid_fr(1)).collect();
        row_dsc.push(GRID_TEMPLATE_LAST);
        // Leak so the pointers stay valid for the LVGL object's
        // lifetime; see doc comment above.
        let col_ptr = Box::leak(col_dsc.into_boxed_slice()).as_ptr();
        let row_ptr = Box::leak(row_dsc.into_boxed_slice()).as_ptr();
        unsafe {
            sys::lv_obj_set_layout(self.raw, sys::lv_layout_t_LV_LAYOUT_GRID);
            sys::lv_obj_set_grid_dsc_array(self.raw, col_ptr, row_ptr);
        }
    }

    /// Mark this object clickable (plain `obj` and `label` need this;
    /// `button` has it on by default).
    pub fn set_clickable(&self) {
        unsafe { sys::lv_obj_add_flag(self.raw, sys::lv_obj_flag_t_LV_OBJ_FLAG_CLICKABLE) };
    }

    /// Hide / show the object.
    pub fn set_hidden(&self, hidden: bool) {
        if hidden {
            unsafe { sys::lv_obj_add_flag(self.raw, sys::lv_obj_flag_t_LV_OBJ_FLAG_HIDDEN) };
        } else {
            unsafe {
                sys::lv_obj_remove_flag(self.raw, sys::lv_obj_flag_t_LV_OBJ_FLAG_HIDDEN)
            };
        }
    }

    /// Enable / disable click handling.
    pub fn set_enabled(&self, enabled: bool) {
        if enabled {
            unsafe { sys::lv_obj_add_flag(self.raw, sys::lv_obj_flag_t_LV_OBJ_FLAG_CLICKABLE) };
        } else {
            unsafe {
                sys::lv_obj_remove_flag(self.raw, sys::lv_obj_flag_t_LV_OBJ_FLAG_CLICKABLE)
            };
        }
    }

    /// Register an LVGL CLICKED callback that pushes a
    /// [`UiEvent::Action`] with no args on `sink` when tapped.
    pub fn on_tap(&self, sink: Sender<UiEvent>, action_name: &str) {
        self.on_tap_with_args(sink, action_name, Vec::new());
    }

    /// Same as [`Self::on_tap`] but carries already-evaluated args
    /// for an action-call shortcut (`on_tap vm_stop { vm = ... }`).
    pub fn on_tap_with_args(
        &self,
        sink: Sender<UiEvent>,
        action_name: &str,
        args: Vec<ActionArg>,
    ) {
        self.register_binding(Binding::Action {
            name: action_name.to_string(),
            args,
            sink,
        });
    }

    /// Register an LVGL CLICKED callback that pushes a
    /// [`UiEvent::Goto`] on `sink` when the object is tapped.
    pub fn on_tap_goto(&self, sink: Sender<UiEvent>, page: &str) {
        self.register_binding(Binding::Goto { page: page.to_string(), sink });
    }

    /// Register an LVGL CLICKED callback that pushes a
    /// [`UiEvent::InlineSet`] on `sink` when the object is tapped.
    /// The `assigns` are evaluated and applied by the main loop so
    /// the UI thread stays free of CEL work on tap.
    pub fn on_tap_inline_set(&self, sink: Sender<UiEvent>, assigns: Vec<InlineSetItem>) {
        self.register_binding(Binding::InlineSet { assigns, sink });
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
    /// trampoline reads the selected option index and pushes an
    /// [`UiEvent::ChangeInlineSet`] with `event.value` as an int.
    pub fn on_change_dropdown_inline_set(
        &self,
        sink: Sender<UiEvent>,
        assigns: Vec<InlineSetItem>,
    ) {
        self.register_change_binding(
            ChangeKind::Dropdown,
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

    fn register_binding(&self, binding: Binding) {
        let idx = {
            let mut g = bindings().lock().unwrap();
            g.push(binding);
            g.len() - 1
        };
        unsafe {
            sys::lv_obj_add_event_cb(
                self.raw,
                Some(on_tap_trampoline),
                sys::lv_event_code_t_LV_EVENT_CLICKED,
                idx as *mut std::ffi::c_void,
            );
        }
    }
}

/// A size value for an LVGL object. Mirrors the schema's `Size` type
/// but at the engine side so the renderer does not need to re-parse.
#[derive(Clone, Copy, Debug)]
pub enum Size {
    Px(i32),
    Percent(u16),
    Content,
}

impl Size {
    fn to_lvgl(self) -> i32 {
        match self {
            Size::Px(px) => px,
            Size::Percent(p) => {
                // LV_PCT(x) encodes a non-negative percentage as
                // `x | LV_COORD_TYPE_SPEC` for x in [0, LV_PCT_POS_MAX].
                let clamped = (p as i32).min(1000);
                clamped | LV_COORD_TYPE_SPEC
            }
            Size::Content => LV_SIZE_CONTENT_LOCAL,
        }
    }
}
