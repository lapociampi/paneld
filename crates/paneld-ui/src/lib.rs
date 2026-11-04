//! Safe LVGL wrapper and config-to-UI renderer for paneld.
//!
//! Per ADR 0001 and 0011, this is the only crate that touches
//! [`lvgl_sys`]. The daemon owns one [`Ui`] on the main thread; LVGL
//! is single-threaded by design.
//!
//! Scope today (first panel milestone):
//!
//! - Open an SDL2 window and attach a mouse input device.
//! - Walk a parsed [`paneld_config::Document`] and build an LVGL tree
//!   for one page's static UI (no components, no `repeat`, no `{{ }}`
//!   expressions; those come in later phases).
//! - Drive LVGL's tick loop until the window is closed.
//!
//! Widgets wired for the first milestone: `column`, `row`, `text`,
//! `button`, `rect`. The schema accepts many more; adding one is a
//! few lines in [`render::render_node`].

pub mod expr;
pub mod render;
pub mod ui;

pub use expr::{eval, Context, EvalError, Value};
pub use render::{interpolate, render_page, render_toast, resolve_value, set_styles};
pub use ui::{
    hide_sleep_cover, inactive_ms, reset_action_bindings, restore_inputs, set_ask_value, set_asset_dir, set_dialog_backdrop, set_wake_sink, show_sleep_cover,
    snapshot_inputs, trigger_activity, wake, ActionArg, ActionTrigger, Backdrop, EventValue, InlineSetItem, MemDisplay, Ui, UiEvent, WakingSender,
    Window,
};
