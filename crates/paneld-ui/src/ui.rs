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
/// [`Obj::on_tap`] / [`Obj::on_tap_goto`].
#[derive(Debug, Clone)]
pub enum UiEvent {
    /// `on_tap = action_name` -- run that action's `run` command.
    Action { name: String },
    /// `on_tap { goto = page_name }` -- switch to that page.
    Goto { page: String },
}

/// Back-compat alias for the pre-nav payload.
#[allow(dead_code)]
pub type ActionTrigger = UiEvent;

#[derive(Clone)]
enum Binding {
    Action { name: String, sink: Sender<UiEvent> },
    Goto { page: String, sink: Sender<UiEvent> },
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
        Some(Binding::Action { name, sink }) => {
            let _ = sink.send(UiEvent::Action { name });
        }
        Some(Binding::Goto { page, sink }) => {
            let _ = sink.send(UiEvent::Goto { page });
        }
        None => {}
    }
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
        // SAFETY: `lv_sdl_window_create` is safe to call after
        // `lv_init`. The returned display is owned by LVGL's global
        // registry; we store the raw pointer for identification. SDL
        // only supports setting the window title via its own call --
        // LVGL 9.6 exposes `lv_sdl_window_set_title`.
        let display = unsafe { sys::lv_sdl_window_create(width, height) };
        assert!(!display.is_null(), "lv_sdl_window_create returned null");
        let c_title = CString::new(title).expect("window title contains a NUL byte");
        unsafe { sys::lv_sdl_window_set_title(display, c_title.as_ptr()) };
        let mouse = unsafe { sys::lv_sdl_mouse_create() };
        assert!(!mouse.is_null(), "lv_sdl_mouse_create returned null");
        Window { display, _mouse: mouse }
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

/// An SDL2 window plus its mouse input device. Attached to the
/// [`Ui`]; dropped before `lv_deinit` runs.
pub struct Window {
    display: *mut sys::lv_display_t,
    // Kept so the input device isn't considered unused; LVGL owns
    // its lifetime through its global indev list.
    _mouse: *mut sys::lv_indev_t,
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
    /// [`UiEvent::Action`] on `sink` when the object is tapped.
    pub fn on_tap(&self, sink: Sender<UiEvent>, action_name: &str) {
        self.register_binding(Binding::Action { name: action_name.to_string(), sink });
    }

    /// Register an LVGL CLICKED callback that pushes a
    /// [`UiEvent::Goto`] on `sink` when the object is tapped.
    pub fn on_tap_goto(&self, sink: Sender<UiEvent>, page: &str) {
        self.register_binding(Binding::Goto { page: page.to_string(), sink });
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
