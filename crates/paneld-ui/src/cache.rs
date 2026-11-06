//! The page cache (ADR 0024 section 3): the pages visited last are
//! kept, each in its own page holder, hidden under the screen, with
//! their node tables, so going back to one updates it with what
//! changed while it was away instead of building it again.
//!
//! A page kept is not updated: each set of changes the main loop
//! hands on is checked against what the page reads (its dep index)
//! and kept as its pending changes when it hits. Nothing of it runs:
//! motion's tweens on it end at their targets, LVGL's own animations
//! (a spinner, a scrolling label) wait in a stash, and a hidden object
//! takes no touch. Shown again, it is updated for its pending changes
//! (and the clock's, by how long it was away) with every value at
//! once, as on a fresh visit; what an update cannot do builds it
//! afresh, as before.
//!
//! At most `app:page_cache` pages (the one on screen included) and
//! `app:page_cache_bytes` of them (estimated) are kept, least recently
//! shown dropped first, and those that hold images when the image
//! cache is past its budget. A reload, a theme switch and a cleared
//! screen drop them all.

use std::cell::{Cell, RefCell};
use std::sync::mpsc::Sender;

use paneld_config::parse::Document;

use crate::expr::Context;
use crate::nodes::{self, Changes, Dep};
use crate::render::Updated;
use crate::ui::{Obj, UiEvent};

/// `app:page_cache` by default: the page on screen and two more.
pub const DEFAULT_PAGES: usize = 3;
/// `app:page_cache_bytes` by default.
pub const DEFAULT_BYTES: usize = 1024 * 1024;
/// The most `app:page_cache` allows.
pub const MAX_PAGES: usize = 8;

/// What the node records and the saved looks of each object cost, on
/// top of the objects (an estimate, per record and per object).
const RECORD_BYTES: usize = 200;
const MADE_BYTES: usize = 160;

struct Parked {
    page: String,
    holder: *mut lvgl_sys::lv_obj_t,
    table: nodes::Saved,
    motion: crate::motion::Saved,
    kept: crate::ui::Kept,
    deps: Vec<Dep>,
    /// What changed, of what it reads, since it was last drawn.
    pending: Changes,
    /// When it was last drawn (Unix seconds): the clock moved since.
    drawn_at: i64,
    used: u64,
    bytes: usize,
}

thread_local! {
    static LIMITS: Cell<(usize, usize)> = const { Cell::new((DEFAULT_PAGES, DEFAULT_BYTES)) };
    static PARKED: RefCell<Vec<Parked>> = const { RefCell::new(Vec::new()) };
    static TICK: Cell<u64> = const { Cell::new(0) };
    /// Pages shown from the cache since start.
    static HITS: Cell<u64> = const { Cell::new(0) };
}

/// Keep at most `pages` pages (the one on screen included; 0 or 1:
/// none is kept, as before ADR 0024's cache) and `bytes` of them.
pub fn set_limits(pages: usize, bytes: usize) {
    LIMITS.with(|l| l.set((pages.min(MAX_PAGES), bytes)));
    enforce();
}

/// Whether a page left can be kept at all.
pub fn enabled() -> bool {
    LIMITS.with(|l| l.get().0 >= 2)
}

/// Pages kept off screen, and their estimated bytes (`ctl status`).
pub fn stats() -> (usize, usize) {
    PARKED.with(|p| {
        let p = p.borrow();
        (p.len(), p.iter().map(|x| x.bytes).sum())
    })
}

/// Pages shown from the cache since start.
pub fn hits() -> u64 {
    HITS.with(|h| h.get())
}

/// Whether page `name` is kept off screen.
pub fn has(name: &str) -> bool {
    PARKED.with(|p| p.borrow().iter().any(|x| x.page == name))
}

/// `holder` is a page kept off screen.
pub(crate) fn is_parked(holder: *mut lvgl_sys::lv_obj_t) -> bool {
    PARKED.with(|p| p.borrow().iter().any(|x| x.holder == holder))
}

/// `changes` happened: each page kept that reads any of it has it
/// pending, for when it shows again.
pub fn note(changes: &Changes) {
    if changes.is_empty() {
        return;
    }
    PARKED.with(|p| {
        for x in p.borrow_mut().iter_mut() {
            if x.deps.iter().any(|d| changes.hits(d)) {
                x.pending.merge(changes.clone());
            }
        }
    });
}

/// The page on screen is left (for another page, or to be drawn
/// again). With `keep` it is kept in the cache, with `pending` (what
/// changed since it was drawn, at `now`); else it goes. Returns what
/// stays on screen for a transition to take off (`transition`): its
/// holder, hidden or deleted at the transition's end. Without one it
/// is hidden or deleted now.
pub fn leave<'u>(screen: &Obj<'u>, keep: bool, pending: &Changes, now: i64, transition: bool) -> Vec<Obj<'u>> {
    let Some(holder) = nodes::holder() else {
        // No page holder (a page that was not found draws a label on
        // the screen): everything goes, as before the cache.
        clear();
        nodes::forget();
        if transition {
            return (0..screen.child_count()).filter_map(|i| screen.child(i)).collect();
        }
        screen.clear_children();
        return Vec::new();
    };
    let saved = if keep && enabled() { nodes::take_table() } else { None };
    match saved {
        Some(table) => {
            let kept = crate::ui::park_holder(holder);
            let motion = crate::motion::park();
            let (records, made) = table.size();
            let bytes = crate::ui::tree_bytes(holder) + records * RECORD_BYTES + made * MADE_BYTES;
            let used = TICK.with(|t| {
                t.set(t.get() + 1);
                t.get()
            });
            let deps = table.deps();
            let mut mine = Changes::default();
            if deps.iter().any(|d| pending.hits(d)) {
                mine.merge(pending.clone());
            }
            PARKED.with(|p| {
                p.borrow_mut().push(Parked {
                    page: table.page().to_string(),
                    holder,
                    table,
                    motion,
                    kept,
                    deps,
                    pending: mine,
                    drawn_at: now,
                    used,
                    bytes,
                })
            });
            if !transition {
                crate::ui::hide_holder(holder);
            }
        }
        None => {
            nodes::forget();
            if !transition {
                crate::ui::delete_holder(holder);
            }
        }
    }
    if transition {
        vec![Obj::from_raw(holder)]
    } else {
        Vec::new()
    }
}

/// Show page `name` from the cache, if it is kept: updated in place
/// for what changed while it was away, from `ctx`, with every value at
/// once; with the changes it was updated for. `None`: it was not kept;
/// [`Updated::Rebuild`]: it could not be updated in place, and is
/// dropped. The caller builds it then.
pub fn show(doc: &Document, name: &str, ctx: &Context, sink: &Sender<UiEvent>, now: i64) -> Option<(Updated, Changes)> {
    let p = PARKED.with(|p| {
        let mut p = p.borrow_mut();
        let i = p.iter().position(|x| x.page == name)?;
        Some(p.remove(i))
    })?;
    let Parked { holder, table, motion, kept, mut pending, drawn_at, .. } = p;
    // The clock moved while it was away: what reads it looks again.
    if now != drawn_at {
        pending.now_second = true;
        pending.ago = true;
    }
    if now.div_euclid(60) != drawn_at.div_euclid(60) {
        pending.now_minute = true;
    }
    nodes::put_table(table);
    crate::motion::unpark(motion);
    crate::ui::unpark_holder(kept);
    // As a page drawn afresh on a visit: no value moves, nothing enters.
    let instant = crate::motion::instant_pending();
    crate::motion::next_render_instant();
    let updated = crate::render::update_page(doc, name, ctx, sink, &pending);
    crate::motion::cancel_instant();
    match updated {
        Updated::Rebuild(why) => {
            // After a sleep, the page built instead is drawn at once too
            // (shown, the update took it: nothing more is instant).
            if instant {
                crate::motion::next_render_instant();
            }
            nodes::forget();
            crate::ui::delete_holder(holder);
            // The page built instead is a first visit.
            crate::motion::forget();
            Some((Updated::Rebuild(why), pending))
        }
        u => {
            crate::ui::show_holder(holder);
            HITS.with(|h| h.set(h.get() + 1));
            Some((u, pending))
        }
    }
}

/// The holder of the page drawn, if it has one (a transition brings
/// it in).
pub fn shown_holder<'u>() -> Option<Obj<'u>> {
    nodes::holder().map(Obj::from_raw)
}

/// Keep the cache within its limits: least recently shown first while
/// there are more pages or bytes than allowed, and those holding
/// images while the image cache is past its budget.
pub fn enforce() {
    let (pages, bytes) = LIMITS.with(|l| l.get());
    let most = pages.saturating_sub(1);
    loop {
        let victim = PARKED.with(|p| {
            let p = p.borrow();
            let total: usize = p.iter().map(|x| x.bytes).sum();
            if p.len() > most || total > bytes {
                return p.iter().enumerate().min_by_key(|(_, x)| x.used).map(|(i, _)| i);
            }
            if !crate::ui::images_over_budget() {
                return None;
            }
            // Only a page whose images no other object shows frees any.
            p.iter()
                .enumerate()
                .filter(|(_, x)| !crate::ui::leaving(x.holder) && crate::ui::images_only_under(x.holder) > 0)
                .min_by_key(|(_, x)| x.used)
                .map(|(i, _)| i)
        });
        let Some(i) = victim else { break };
        let x = PARKED.with(|p| p.borrow_mut().remove(i));
        drop_parked(x);
        // What it showed is freed now: the budget is looked at again.
        crate::ui::trim_images();
    }
}

/// Take off again what runs on the pages kept (nothing should: an
/// object restyled from above, a label or a keypad, starts again).
/// Called after a page is built.
pub fn restash() {
    PARKED.with(|p| {
        for x in p.borrow_mut().iter_mut() {
            crate::ui::restash_holder(x.holder, &mut x.kept);
        }
    });
}

/// Drop every page kept (a reload, a theme switch: they were drawn
/// with what no longer is).
pub fn clear() {
    let all = PARKED.with(|p| std::mem::take(&mut *p.borrow_mut()));
    for x in all {
        drop_parked(x);
    }
    crate::ui::trim_images();
}

/// Forget every page kept, whose objects are going with the screen's.
pub(crate) fn forget() {
    PARKED.with(|p| p.borrow_mut().clear());
}

fn drop_parked(x: Parked) {
    let holder = x.holder;
    // Its records, motion and stash go first; none touches an object.
    drop(x);
    // A page the transition under way is taking off is deleted at its
    // end (it is no longer kept).
    if !crate::ui::leaving(holder) {
        crate::ui::delete_holder(holder);
    }
}
