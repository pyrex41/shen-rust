//! Conservative native-stack root discovery for GC Step 4.
//!
//! At a depth-0 safepoint the only `Value`s outside the interpreter's
//! registered containers live in **host native stack frames** (the REPL's
//! last result, a `ShenHost` local, the just-returned value of the exiting
//! funnel). This module finds them the way the §6g spike
//! (`benches/gc_roots_aot_spike.rs`) proved sound for a *non-moving*
//! collector: flush the callee-saved registers to a buffer, then walk every
//! 8-aligned word of `[current_sp, stack_base)` and treat anything that is a
//! `TAG_PTR`-tagged head-of-node address as a root. A false positive can only
//! over-retain, never corrupt; a miss is impossible for a word that is
//! actually live in a frame or callee-saved register, because the scan runs
//! *below* every live frame (the stack grows down) and the flush spills the
//! registers a value could be parked in across a call.
//!
//! ## Support matrix (deliberate, judged — see the Step-4 design record)
//!
//! * **aarch64 + macOS/Linux, not miri**: full scan ([`SCAN_SUPPORTED`] =
//!   true).
//! * **anything else**: [`SCAN_SUPPORTED`] = false and the runtime REFUSES to
//!   enable request-mode collection (the heap stays grow-only, exactly the
//!   shipped Step-3 behavior). The spike's silent no-op register flush was
//!   explicitly flagged unsound for production — a missed register root is a
//!   use-after-free — so unsupported targets get a hard refusal, not a
//!   degraded scan. An x86_64 spill (rbx/rbp/r12–r15) is a documented
//!   follow-up.
//! * **miri**: the scan reads raw (possibly uninitialized) stack words, which
//!   miri rightly rejects; collection compiles to precise-roots-only so the
//!   mark/sweep/`Drop` machinery itself stays miri-covered.

#![allow(dead_code)] // the cfg'd-out fallback keeps names alive on all targets

use std::cell::Cell;

/// Can this build conservatively scan the native stack? When `false`,
/// request-mode collection must not be enabled (see the module docs).
pub(crate) const SCAN_SUPPORTED: bool = cfg!(all(
    target_arch = "aarch64",
    any(target_os = "macos", target_os = "linux"),
    not(miri)
));

thread_local! {
    /// Cached high end of this thread's stack (0 = not yet queried). The
    /// bounds are fixed for a thread's lifetime, so one pthread query
    /// suffices.
    static STACK_BASE: Cell<usize> = const { Cell::new(0) };
}

/// The high end of the current thread's stack, queried once per thread.
///
/// # Panics
/// If the platform query fails — a wrong stack base would make the scan
/// silently unsound, so there is no fallback.
pub(crate) fn stack_base() -> usize {
    let cached = STACK_BASE.with(|b| b.get());
    if cached != 0 {
        return cached;
    }
    let base = query_stack_base();
    assert!(base != 0, "could not determine the thread stack base");
    STACK_BASE.with(|b| b.set(base));
    base
}

#[cfg(target_os = "macos")]
fn query_stack_base() -> usize {
    extern "C" {
        fn pthread_self() -> *mut core::ffi::c_void;
        fn pthread_get_stackaddr_np(thread: *mut core::ffi::c_void) -> *mut core::ffi::c_void;
    }
    // SAFETY: both calls are pure queries on the current thread's handle;
    // `pthread_get_stackaddr_np` returns the stack's high end on macOS (for
    // the main thread and for pthreads alike).
    unsafe { pthread_get_stackaddr_np(pthread_self()) as usize }
}

#[cfg(target_os = "linux")]
fn query_stack_base() -> usize {
    // A glibc `pthread_attr_t` is 56 bytes on aarch64/x86_64 (musl's is
    // smaller); 64 aligned bytes safely over-allocates the out-param.
    #[repr(C, align(8))]
    struct PthreadAttr([u8; 64]);

    extern "C" {
        fn pthread_self() -> usize;
        fn pthread_getattr_np(thread: usize, attr: *mut PthreadAttr) -> i32;
        fn pthread_attr_getstack(
            attr: *const PthreadAttr,
            stackaddr: *mut *mut core::ffi::c_void,
            stacksize: *mut usize,
        ) -> i32;
        fn pthread_attr_destroy(attr: *mut PthreadAttr) -> i32;
    }

    // SAFETY: standard pthread_getattr_np / pthread_attr_getstack sequence on
    // the current thread; the attr buffer outlives the calls and is destroyed
    // exactly once.
    unsafe {
        let mut attr = PthreadAttr([0; 64]);
        if pthread_getattr_np(pthread_self(), &mut attr) != 0 {
            return 0;
        }
        let mut addr: *mut core::ffi::c_void = std::ptr::null_mut();
        let mut size: usize = 0;
        let rc = pthread_attr_getstack(&attr, &mut addr, &mut size);
        pthread_attr_destroy(&mut attr);
        if rc != 0 {
            return 0;
        }
        // `pthread_attr_getstack` reports the LOW end; the base is low + size.
        addr as usize + size
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn query_stack_base() -> usize {
    0 // unsupported — `SCAN_SUPPORTED` is false, enable refuses before here
}

/// Approximate current stack pointer: the address of a fresh local. The stack
/// grows down, so this is the LOW end of the live region. `#[inline(never)]`
/// so the probe frame sits below the caller's.
#[inline(never)]
pub(crate) fn current_sp() -> usize {
    let probe = 0u8;
    std::hint::black_box(&probe) as *const u8 as usize
}

/// Spill the aarch64 callee-saved registers into `buf`, so a root whose only
/// home is a callee-saved register is visible to the scan. A value live
/// across a call sits EITHER in a stack slot (scanned) OR in a callee-saved
/// register (flushed here); caller-saved registers cannot hold a value across
/// the call into the collector. Covers the FULL AAPCS64 callee-saved set:
/// x19–x28 (from the §6g spike) **plus d8–d15** — LLVM can in principle park
/// a u64 in a callee-saved FP register under GPR pressure, and Boehm's
/// aarch64 flush covers them for exactly this reason (adversarial-review
/// hardening; no concrete miss was demonstrated).
#[cfg(all(target_arch = "aarch64", not(miri)))]
#[inline(never)]
pub(crate) fn flush_callee_saved(buf: &mut [u64; 18]) {
    // SAFETY: pure stores of register contents into a caller-provided buffer.
    unsafe {
        std::arch::asm!(
            "stp x19, x20, [{b}, #0]",
            "stp x21, x22, [{b}, #16]",
            "stp x23, x24, [{b}, #32]",
            "stp x25, x26, [{b}, #48]",
            "stp x27, x28, [{b}, #64]",
            "stp d8, d9, [{b}, #80]",
            "stp d10, d11, [{b}, #96]",
            "stp d12, d13, [{b}, #112]",
            "stp d14, d15, [{b}, #128]",
            b = in(reg) buf.as_mut_ptr(),
            options(nostack, preserves_flags),
        );
    }
}

#[cfg(not(all(target_arch = "aarch64", not(miri))))]
#[inline(never)]
pub(crate) fn flush_callee_saved(_buf: &mut [u64; 18]) {
    // Unsupported: deliberately unreachable in a collecting configuration —
    // `SCAN_SUPPORTED` is false and request-mode enable refuses. NOT a silent
    // degraded mode (a missed register root would be a use-after-free).
}
