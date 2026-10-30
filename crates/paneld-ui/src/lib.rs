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
pub use render::{interpolate, render_page};
pub use ui::{reset_action_bindings, ActionTrigger, Ui, UiEvent, Window};
