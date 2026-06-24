//! C-ABI bindings for embedding shen-rust in non-Rust hosts (Swift / iOS).
//!
//! The whole surface is four functions:
//!
//! ```c
//! ShenCtx *shen_boot(const char *kernel_dir);   // NULL dir -> auto-locate
//! char    *shen_eval(ShenCtx *ctx, const char *src);
//! void     shen_string_free(char *s);
//! void     shen_free(ShenCtx *ctx);
//! ```
//!
//! `shen_eval` evaluates a *Shen-level* expression (not raw KLambda) by driving
//! the kernel's own pipeline — `read-from-string` -> `head` -> `eval` — and then
//! rendering the result to a string with `shen.app`. The returned C string is
//! heap-allocated by Rust and must be released with `shen_string_free`.

use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::path::PathBuf;

use shen_rust::interp::boot::{boot, boot_from_kl_source, boot_with_kernel, eval_kl_source};
use shen_rust::interp::eval::Interp;
use shen_rust::value::Value;

/// Opaque handle to a booted Shen image.
pub struct ShenCtx {
    interp: Interp,
}

/// The ShenOSKernel-41.2 `.kl` sources, embedded into the binary at compile
/// time in shen-rust's load order. Lets `shen_boot_embedded` bring up the full
/// kernel with **no filesystem access** — the iOS-friendly path (no bundled
/// resources, no sandbox path juggling).
const KERNEL_PARTS: &[&str] = &[
    include_str!("../../../kernel/klambda/core.kl"),
    include_str!("../../../kernel/klambda/toplevel.kl"),
    include_str!("../../../kernel/klambda/sys.kl"),
    include_str!("../../../kernel/klambda/reader.kl"),
    include_str!("../../../kernel/klambda/prolog.kl"),
    include_str!("../../../kernel/klambda/load.kl"),
    include_str!("../../../kernel/klambda/writer.kl"),
    include_str!("../../../kernel/klambda/macros.kl"),
    include_str!("../../../kernel/klambda/declarations.kl"),
    include_str!("../../../kernel/klambda/types.kl"),
    include_str!("../../../kernel/klambda/t-star.kl"),
    include_str!("../../../kernel/klambda/sequent.kl"),
    include_str!("../../../kernel/klambda/track.kl"),
    include_str!("../../../kernel/klambda/dict.kl"),
    include_str!("../../../kernel/klambda/compiler.kl"),
    include_str!("../../../kernel/klambda/stlib.kl"),
    include_str!("../../../kernel/klambda/init.kl"),
    include_str!("../../../kernel/klambda/extension-features.kl"),
    include_str!("../../../kernel/klambda/extension-expand-dynamic.kl"),
    include_str!("../../../kernel/klambda/extension-launcher.kl"),
    include_str!("../../../kernel/klambda/yacc.kl"),
];

/// Boots a Shen kernel and returns an owning handle, or NULL on failure.
///
/// `kernel_dir` is an optional path to a directory of `.kl` kernel files; pass
/// NULL to let shen-rust auto-locate its vendored kernel.
///
/// # Safety
/// `kernel_dir` must be NULL or a valid NUL-terminated C string.
#[no_mangle]
pub extern "C" fn shen_boot(kernel_dir: *const c_char) -> *mut ShenCtx {
    let mut interp = Interp::new();
    let result = if kernel_dir.is_null() {
        boot(&mut interp)
    } else {
        let dir = unsafe { CStr::from_ptr(kernel_dir) }
            .to_string_lossy()
            .into_owned();
        boot_with_kernel(&mut interp, &PathBuf::from(dir))
    };
    match result {
        Ok(()) => Box::into_raw(Box::new(ShenCtx { interp })),
        Err(_) => std::ptr::null_mut(),
    }
}

/// Boots the full kernel from the **embedded** `.kl` sources — no filesystem
/// access at all. This is the recommended entry point on iOS. Returns NULL on
/// failure; free with `shen_free`.
#[no_mangle]
pub extern "C" fn shen_boot_embedded() -> *mut ShenCtx {
    let mut interp = Interp::new();
    let src = KERNEL_PARTS.join("\n");
    match boot_from_kl_source(&mut interp, &src, Some(shen_rust::aot::kernel::install_all)) {
        Ok(()) => Box::into_raw(Box::new(ShenCtx { interp })),
        Err(_) => std::ptr::null_mut(),
    }
}

/// Boots any Ratatoskr-shaken program: a shaken `kernel.kl` slice plus an
/// optional program `.kl`. Pass NULL `prog_kl` for kernel-only.
///
/// Follows the builder contract — boot the kernel and run `(shen.initialise)`
/// first, then load the program (whose top-level forms need the initialised
/// environment). The program load runs with `*hush*` set, so the program's own
/// "…loaded" chatter to stdout is suppressed (file writes still happen).
///
/// # Safety
/// `kernel_kl` must be a valid C string; `prog_kl` NULL or a valid C string.
#[no_mangle]
pub extern "C" fn shen_boot_shaken(
    kernel_kl: *const c_char,
    prog_kl: *const c_char,
) -> *mut ShenCtx {
    if kernel_kl.is_null() {
        return std::ptr::null_mut();
    }
    let kernel = unsafe { CStr::from_ptr(kernel_kl) }
        .to_string_lossy()
        .into_owned();
    let prog = if prog_kl.is_null() {
        None
    } else {
        Some(
            unsafe { CStr::from_ptr(prog_kl) }
                .to_string_lossy()
                .into_owned(),
        )
    };
    match boot_shaken_inner(&kernel, prog.as_deref()) {
        Ok(interp) => Box::into_raw(Box::new(ShenCtx { interp })),
        Err(e) => {
            eprintln!("shen_boot_shaken error: {e}");
            std::ptr::null_mut()
        }
    }
}

fn boot_shaken_inner(kernel: &str, prog: Option<&str>) -> Result<Interp, String> {
    let mut interp = Interp::new();
    boot_from_kl_source(&mut interp, kernel, None).map_err(|e| e.to_string())?;
    if let Some(p) = prog {
        // Silence the program's load-time stdout messages (e.g. shen-cas's
        // "…loaded" lines). shen-rust's `pr` honours `*hush*` for stdout only,
        // so file writes during load are unaffected. Restore afterward.
        let hush = interp.intern("*hush*");
        interp.env.set_global(hush, Value::bool(true));
        let r = eval_kl_source(&mut interp, p, "shaken program", &[]).map_err(|e| e.to_string());
        interp.env.set_global(hush, Value::bool(false));
        r?;
    }
    Ok(interp)
}

/// Evaluates a Shen expression and returns its rendered value as a freshly
/// allocated C string (release with `shen_string_free`). On error the string is
/// `"error: <message>"`. Returns NULL only if the arguments are NULL.
///
/// # Safety
/// `ctx` must be a handle from `shen_boot` (not yet freed) and `src` a valid
/// NUL-terminated C string.
#[no_mangle]
pub extern "C" fn shen_eval(ctx: *mut ShenCtx, src: *const c_char) -> *mut c_char {
    if ctx.is_null() || src.is_null() {
        return std::ptr::null_mut();
    }
    let ctx = unsafe { &mut *ctx };
    let src = unsafe { CStr::from_ptr(src) }
        .to_string_lossy()
        .into_owned();
    let out = match eval_shen(&mut ctx.interp, &src) {
        Ok(s) => s,
        Err(e) => format!("error: {e}"),
    };
    // CString::new fails only on interior NULs; fall back to empty in that case.
    CString::new(out)
        .unwrap_or_else(|_| CString::new("").unwrap())
        .into_raw()
}

/// Releases a string returned by `shen_eval`.
///
/// # Safety
/// `s` must be NULL or a pointer previously returned by `shen_eval`.
#[no_mangle]
pub extern "C" fn shen_string_free(s: *mut c_char) {
    if !s.is_null() {
        unsafe { drop(CString::from_raw(s)) };
    }
}

/// Releases a handle returned by `shen_boot`.
///
/// # Safety
/// `ctx` must be NULL or a handle from `shen_boot` that has not been freed.
#[no_mangle]
pub extern "C" fn shen_free(ctx: *mut ShenCtx) {
    if !ctx.is_null() {
        unsafe { drop(Box::from_raw(ctx)) };
    }
}

/// Evaluate one Shen-level expression and render the result to a string,
/// entirely through kernel functions so the semantics match the REPL.
fn eval_shen(interp: &mut Interp, src: &str) -> Result<String, String> {
    // Intern every symbol up front (mutable borrow), then resolve + apply each
    // kernel function in turn (the get_fn borrow ends at each `.cloned()`).
    let read_sym = interp.intern("read-from-string");
    let head_sym = interp.intern("head");
    let eval_sym = interp.intern("eval");
    let app_sym = interp.intern("shen.app");
    let mode = Value::sym(interp.intern("shen.s"));

    let read_fn = interp
        .env
        .get_fn(read_sym)
        .cloned()
        .ok_or_else(|| "read-from-string is undefined".to_string())?;
    let forms = interp
        .apply(read_fn, vec![Value::str(src)])
        .map_err(|e| e.to_string())?;

    let head_fn = interp
        .env
        .get_fn(head_sym)
        .cloned()
        .ok_or_else(|| "head is undefined".to_string())?;
    let first = interp
        .apply(head_fn, vec![forms])
        .map_err(|e| e.to_string())?;

    let eval_fn = interp
        .env
        .get_fn(eval_sym)
        .cloned()
        .ok_or_else(|| "eval is undefined".to_string())?;
    let result = interp
        .apply(eval_fn, vec![first])
        .map_err(|e| e.to_string())?;

    let app_fn = interp
        .env
        .get_fn(app_sym)
        .cloned()
        .ok_or_else(|| "shen.app is undefined".to_string())?;
    let rendered = interp
        .apply(app_fn, vec![result, Value::str(""), mode])
        .map_err(|e| e.to_string())?;

    rendered
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| "result did not render to a string".to_string())
}
