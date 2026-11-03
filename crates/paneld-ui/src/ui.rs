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
    /// Keypad `on_submit = <action>`; sends [`UiEvent::Action`] with
    /// the digits as the `value` arg.
    SubmitAction { action: String, sink: Sender<UiEvent> },
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
    INPUTS.with(|v| v.borrow_mut().clear());
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
    if let Some(Binding::SubmitAction { action, sink }) = &bind {
        let digits = keyboard_text(target);
        let _ = sink.send(UiEvent::Action {
            name: action.clone(),
            args: vec![ActionArg {
                name: "value".into(),
                value: crate::expr::Value::String(digits),
            }],
        });
        return;
    }
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

    /// A display that renders into memory instead of a screen, with a
    /// scriptable pointer. For `paneld shot`: render a page (and the
    /// result of taps on it) to an image without any hardware.
    pub fn open_memory_display(&self, width: i32, height: i32) -> MemDisplay {
        let display = unsafe { sys::lv_display_create(width, height) };
        assert!(!display.is_null(), "lv_display_create returned null");
        // One full-size buffer in DIRECT mode: after a refresh it holds
        // the whole frame. u32 elements keep it 4-byte aligned and big
        // enough for any color format up to 32 bits.
        let mut buf = vec![0u32; (width * height) as usize].into_boxed_slice();
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
    buf: Box<[u32]>,
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

    /// Register LV_EVENT_CANCEL (the number keyboard's close key) to
    /// send `ConfirmReply { accept: false }`.
    pub fn on_cancel_reply(&self, sink: Sender<UiEvent>) {
        let idx = {
            let mut g = bindings().lock().unwrap();
            g.push(Binding::Confirm { accept: false, sink });
            g.len() - 1
        };
        unsafe {
            sys::lv_obj_add_event_cb(
                self.raw,
                Some(on_tap_trampoline),
                sys::lv_event_code_t_LV_EVENT_CANCEL,
                idx as *mut std::ffi::c_void,
            );
        }
    }

    /// Keypad `on_submit = <action>`: on the keyboard's OK, call the
    /// action with the digits bound as `value`.
    pub fn on_submit_action(&self, sink: Sender<UiEvent>, action: String) {
        let idx = {
            let mut g = bindings().lock().unwrap();
            g.push(Binding::SubmitAction { action, sink });
            g.len() - 1
        };
        unsafe {
            sys::lv_obj_add_event_cb(
                self.raw,
                Some(on_change_trampoline),
                sys::lv_event_code_t_LV_EVENT_READY,
                idx as *mut std::ffi::c_void,
            );
        }
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

    pub fn set_width(&self, w: Size) {
        unsafe { sys::lv_obj_set_width(self.raw, w.to_lvgl()) };
    }

    pub fn set_height(&self, h: Size) {
        unsafe { sys::lv_obj_set_height(self.raw, h.to_lvgl()) };
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
    pub fn child_count(&self) -> u32 {
        unsafe { sys::lv_obj_get_child_count(self.raw) }
    }

    /// The `i`th child, if there is one.
    pub fn child(&self, i: u32) -> Option<Obj<'ui>> {
        let raw = unsafe { sys::lv_obj_get_child(self.raw, i as i32) };
        (!raw.is_null()).then_some(Obj { raw, _ui: PhantomData })
    }

    pub fn clear_children(&self) {
        unsafe { sys::lv_obj_clean(self.raw) };
        // With the whole screen cleared no grid is left that could
        // point at the track arrays.
        if self.raw == unsafe { sys::lv_screen_active() } {
            LAYOUT_MEM.with(|m| m.borrow_mut().clear());
        }
    }

    /// Lay this object's current children out as a grid: `cols` equal
    /// columns, rows sized to their content, children placed row by
    /// row. Call it after the children exist (so `repeat` output is
    /// counted). `rows` can ask for more rows than the children need.
    pub fn layout_grid(&self, cols: i32, rows: Option<i32>) {
        let cols = cols.max(1);
        let n = unsafe { sys::lv_obj_get_child_count(self.raw) } as i32;
        let needed = (n + cols - 1) / cols;
        let rows = rows.unwrap_or(needed).max(needed).max(1);
        let mut col_dsc: Vec<i32> = (0..cols).map(|_| grid_fr(1)).collect();
        col_dsc.push(GRID_TEMPLATE_LAST);
        let mut row_dsc: Vec<i32> = (0..rows).map(|_| GRID_CONTENT).collect();
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
            for i in 0..n {
                let child = sys::lv_obj_get_child(self.raw, i);
                sys::lv_obj_set_grid_cell(
                    child,
                    sys::lv_grid_align_t_LV_GRID_ALIGN_STRETCH,
                    i % cols,
                    1,
                    sys::lv_grid_align_t_LV_GRID_ALIGN_STRETCH,
                    i / cols,
                    1,
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
