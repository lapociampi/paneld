//! Collect top-level `action NAME { run = ... }` blocks and run them
//! on tap. Supports only the simplest shape today: an action's
//! single `run` string executed through [`crate::runner`]. Named
//! action-call args, `then`/`else`, `refresh`, `confirm`, `pin`,
//! `set` / `env`, and secret resolution come in later phases.

use std::collections::HashMap;
use std::sync::mpsc::Receiver;
use std::thread;
use std::time::Duration;

use paneld_config::parse::{AssignTarget, Document, Item};
use paneld_ui::Context;

use crate::runner;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_MAX_BYTES: usize = 64 * 1024;

/// Scan `doc` for top-level `action NAME { run = ... }` blocks.
/// Returns a map from the action's name to its raw `run` command.
pub fn collect(doc: &Document) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for item in &doc.items {
        let Item::Block(b) = item else { continue };
        if b.kind.text != "action" {
            continue;
        }
        let Some(name) = &b.name else { continue };
        if let Some(run) = find_simple(b, "run") {
            out.insert(name.text.clone(), run.to_string());
        }
    }
    out
}

/// Spawn a worker thread that owns the receiver side of the action
/// trigger channel. For each incoming trigger it looks up the
/// action's `run` command in `actions` and executes it in its own
/// short-lived thread so a slow action never blocks the UI event
/// stream.
pub fn spawn_worker(
    actions: HashMap<String, String>,
    ctx: Context,
    rx: Receiver<String>,
) {
    thread::spawn(move || {
        for action_name in rx {
            let Some(raw) = actions.get(&action_name).cloned() else {
                eprintln!("paneld: action `{action_name}`: unknown action");
                continue;
            };
            // Interpolate secrets and other context into the shell
            // command before executing. The file/base lookup is a
            // stub here -- actions are collected without their
            // original spans; a later phase threads the real span
            // for better diagnostics on CEL parse errors.
            let file = paneld_config::source::FileId::default();
            let cmd = paneld_ui::interpolate(&raw, file, 0, &ctx);
            let name = action_name;
            thread::spawn(move || match runner::exec(&cmd, DEFAULT_TIMEOUT, DEFAULT_MAX_BYTES) {
                Ok(r) => {
                    eprintln!(
                        "paneld: action `{name}`: exit {:?}, timed_out {}",
                        r.exit_code, r.timed_out
                    );
                    if !r.stdout.trim().is_empty() {
                        eprintln!("paneld: action `{name}` stdout: {}", r.stdout.trim());
                    }
                    if !r.stderr.trim().is_empty() {
                        eprintln!("paneld: action `{name}` stderr: {}", r.stderr.trim());
                    }
                }
                Err(e) => eprintln!("paneld: action `{name}`: {e}"),
            });
        }
    });
}

fn find_simple<'a>(
    node: &'a paneld_config::parse::Block,
    key: &str,
) -> Option<&'a str> {
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
