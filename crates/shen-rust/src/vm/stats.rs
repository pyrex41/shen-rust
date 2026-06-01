//! Lightweight VM coverage counters.
//!
//! These answer the question that gates the "ship the VM for warm/served"
//! decision: when `SHEN_RUST_VM=1`, what fraction of the runtime-built
//! closure/defun bodies does the VM actually *serve* vs *bail* on (falling
//! back to the tree-walker because the body uses a form the compiler can't
//! lower — `trap-error`, `thaw`, etc.)? The kernel type-checker is a
//! backtracking CPS Prolog, so its hot continuations are `trap-error`-heavy;
//! a high bail rate would mean flipping the flag barely touches the hot path.
//!
//! Counting happens at *compile* time (once per unique body address, on the
//! cache-miss path), so it is off the hot execution loop entirely. Read the
//! totals with [`snapshot`]; reset between phases with [`reset`].

use std::sync::atomic::{AtomicU64, Ordering};

static CLOSURE_SERVED: AtomicU64 = AtomicU64::new(0);
static CLOSURE_BAILED: AtomicU64 = AtomicU64::new(0);
static DEFUN_SERVED: AtomicU64 = AtomicU64::new(0);
static DEFUN_BAILED: AtomicU64 = AtomicU64::new(0);

#[inline]
pub fn record_closure(served: bool) {
    if served {
        CLOSURE_SERVED.fetch_add(1, Ordering::Relaxed);
    } else {
        CLOSURE_BAILED.fetch_add(1, Ordering::Relaxed);
    }
}

#[inline]
pub fn record_defun(served: bool) {
    if served {
        DEFUN_SERVED.fetch_add(1, Ordering::Relaxed);
    } else {
        DEFUN_BAILED.fetch_add(1, Ordering::Relaxed);
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Snapshot {
    pub closure_served: u64,
    pub closure_bailed: u64,
    pub defun_served: u64,
    pub defun_bailed: u64,
}

impl Snapshot {
    /// Fraction of runtime closure (`lambda`/`freeze`) bodies the VM served.
    pub fn closure_coverage(&self) -> f64 {
        let total = self.closure_served + self.closure_bailed;
        if total == 0 {
            0.0
        } else {
            self.closure_served as f64 / total as f64
        }
    }
}

pub fn snapshot() -> Snapshot {
    Snapshot {
        closure_served: CLOSURE_SERVED.load(Ordering::Relaxed),
        closure_bailed: CLOSURE_BAILED.load(Ordering::Relaxed),
        defun_served: DEFUN_SERVED.load(Ordering::Relaxed),
        defun_bailed: DEFUN_BAILED.load(Ordering::Relaxed),
    }
}

pub fn reset() {
    CLOSURE_SERVED.store(0, Ordering::Relaxed);
    CLOSURE_BAILED.store(0, Ordering::Relaxed);
    DEFUN_SERVED.store(0, Ordering::Relaxed);
    DEFUN_BAILED.store(0, Ordering::Relaxed);
}
