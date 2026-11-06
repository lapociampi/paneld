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
pub mod perf;
pub mod motion;
pub mod nodes;
pub mod render;
pub mod ui;

pub use expr::{ago_due, eval, Context, EvalError, Scope, Value};
pub use nodes::Changes;
pub use render::{
    interpolate, page_ago_due, render_page, render_toast, resolve_value, resolve_value_in, scratch_page, show_screen, delete_screen, set_call_states, set_styles, update_page, verify_page,
    CallState, RunState, Updated,
};
pub use ui::{
    begin_page_change, finish_motion, restless_areas, hide_sleep_cover, idle_hint_ms, inactive_ms, page_transition_running, run_page_transition, running_animations, restore_inputs, set_ask_value, set_asset_dir, set_dialog_backdrop, set_wake_sink, show_sleep_cover, wake_within, interacting,
    snapshot_inputs, close_dialog, trigger_activity, wake, ActionArg, ActionTrigger, Backdrop, EventValue, InlineSetItem, MemDisplay, Ui, UiEvent, WakingSender,
    Window,
};
