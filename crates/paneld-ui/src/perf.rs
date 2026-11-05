//! PANELD_PERF fine timers for a page rebuild (docs/deploy.md,
//! Diagnostics). Read once; with it unset every probe is one branch.

use std::cell::Cell;
use std::time::Instant;

pub fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("PANELD_PERF").is_some())
}

#[derive(Clone, Copy)]
pub enum Slot {
    Parse = 0,
    Eval = 1,
    Clone = 2,
    Props = 3,
    Font = 4,
}

thread_local! {
    static US: [Cell<u64>; 5] = const { [Cell::new(0), Cell::new(0), Cell::new(0), Cell::new(0), Cell::new(0)] };
    static N: [Cell<u64>; 5] = const { [Cell::new(0), Cell::new(0), Cell::new(0), Cell::new(0), Cell::new(0)] };
    static DEPTH: [Cell<u32>; 5] = const { [Cell::new(0), Cell::new(0), Cell::new(0), Cell::new(0), Cell::new(0)] };
}

/// Time `f` into `slot`. Only the outermost call of a slot is timed
/// (an evaluation recursing into its operands counts once); the
/// nested ones run as they are.
#[inline]
pub fn time<R>(slot: Slot, f: impl FnOnce() -> R) -> R {
    if !on() {
        return f();
    }
    let i = slot as usize;
    let outer = DEPTH.with(|d| d[i].get() == 0);
    if !outer {
        return f();
    }
    DEPTH.with(|d| d[i].set(1));
    let t0 = Instant::now();
    let r = f();
    let us = t0.elapsed().as_micros() as u64;
    DEPTH.with(|d| d[i].set(0));
    US.with(|u| u[i].set(u[i].get() + us));
    N.with(|n| n[i].set(n[i].get() + 1));
    r
}

/// `parse=us/n eval=us/n clone=us/n` since the last take.
pub fn take() -> String {
    let us: Vec<u64> = US.with(|u| u.iter().map(|c| c.replace(0)).collect());
    let n: Vec<u64> = N.with(|u| u.iter().map(|c| c.replace(0)).collect());
    format!(
        "parse={}/{} eval={}/{} clone={}/{} props={}/{} font={}/{}",
        us[0], n[0], us[1], n[1], us[2], n[2], us[3], n[3], us[4], n[4]
    )
}

/// CLOCK_MONOTONIC in us, as the C frame lines' `t`.
pub fn mono_us() -> u64 {
    crate::ui::mono_us()
}
