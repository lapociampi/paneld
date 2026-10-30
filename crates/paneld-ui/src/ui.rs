//! Safe wrapper over `lvgl-sys`. All `unsafe` for the UI module lives
//! here. Only this crate links to `lvgl_sys`.
//!
//! LVGL is single-threaded by design (ADR 0010): one [`Ui`] on the
//! main thread owns the global LVGL state. Dropping it calls
//! `lv_deinit`.

use std::ffi::CString;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use lvgl_sys as sys;

// LVGL's `LV_SIZE_CONTENT` and `LV_PCT(x)` are C macros that bindgen
// doesn't translate; mirror the exact bit layout from
// `vendor/lvgl/include/lvgl/core/lv_area.h` here.
const LV_COORD_TYPE_SPEC: i32 = 1 << 29;
const LV_COORD_MAX_LOCAL: i32 = (1 << 29) - 1;
const LV_SIZE_CONTENT_LOCAL: i32 = LV_COORD_MAX_LOCAL | LV_COORD_TYPE_SPEC;

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

    pub fn align_center(&self) {
        unsafe { sys::lv_obj_align(self.raw, sys::lv_align_t_LV_ALIGN_CENTER, 0, 0) };
    }

    /// Remove every child of this object but keep the object itself,
    /// so a caller can rebuild the tree in place. Used by the live
    /// re-render path when a source value changes.
    pub fn clear_children(&self) {
        unsafe { sys::lv_obj_clean(self.raw) };
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
