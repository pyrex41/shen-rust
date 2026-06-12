//! Kernel boot: load all 21 KL files, set port metadata, run
//! `shen.initialise`, then publish primitive arity / lambda-form on the
//! kernel's property vector so `(fn NAME)` resolves correctly.
//!
//! The fixed file order matches `shen-ocaml/src/interp/boot.ml`. Per the
//! 41.x spec, order does not strictly matter (the files only `defun` —
//! they have no top-level effects). The fixed order makes errors easier
//! to compare against other ports.

use std::cell::RefCell;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use crate::error::{ShenError, ShenResult};
use crate::interp::eval::Interp;
use crate::kl::parser::parse_all;
use crate::value::{Stream, Value};

/// File names in the order shen-ocaml loads them. Vendored under
/// `kernel/klambda/`.
const KERNEL_FILES: &[&str] = &[
    "core.kl",
    "toplevel.kl",
    "sys.kl",
    "reader.kl",
    "prolog.kl",
    "load.kl",
    "writer.kl",
    "macros.kl",
    "declarations.kl",
    "types.kl",
    "t-star.kl",
    "sequent.kl",
    "track.kl",
    "dict.kl",
    "compiler.kl",
    "stlib.kl",
    "init.kl",
    "extension-features.kl",
    "extension-expand-dynamic.kl",
    "extension-launcher.kl",
    "yacc.kl",
];

/// Names + arities of native primitives whose `arity` and `shen.lambda-form`
/// entries need to be installed on the kernel's `*property-vector*` after
/// `shen.initialise` runs, so `(fn NAME)` lookups succeed in code generated
/// by the kernel's own reader.
const PRIMITIVE_METADATA: &[(&str, usize)] = &[
    ("intern", 1),
    ("+", 2),
    ("*", 2),
    ("-", 2),
    ("/", 2),
    ("set", 2),
    ("value", 1),
    ("simple-error", 1),
    ("tc", 1),
    ("=", 2),
    ("cons", 2),
    ("hd", 1),
    ("tl", 1),
    ("number?", 1),
    ("cons?", 1),
    ("string?", 1),
    ("vector?", 1),
    ("absvector?", 1),
    ("str", 1),
    (">", 2),
    ("<", 2),
    ("<=", 2),
    (">=", 2),
    ("cn", 2),
    ("pos", 2),
    ("tlstr", 1),
    ("n->string", 1),
    ("string->n", 1),
    ("symbol?", 1),
    ("boolean?", 1),
    ("open", 2),
    ("close", 1),
    ("read-byte", 1),
    ("write-byte", 2),
    ("get-time", 1),
    ("type", 2),
    ("eval-kl", 1),
    ("absvector", 1),
    ("<-address", 2),
    ("address->", 3),
    ("apply", 2),
    ("error-to-string", 1),
    ("hash", 2),
    ("fn", 1),
];

/// Locate the vendored `kernel/klambda` directory. Search order:
/// 1. `SHEN_KERNEL_DIR` env var.
/// 2. Walk parents looking for `kernel/klambda/core.kl`.
/// 3. CWD-relative candidates.
pub fn find_kernel_dir() -> ShenResult<PathBuf> {
    if let Ok(dir) = std::env::var("SHEN_KERNEL_DIR") {
        let p = PathBuf::from(dir);
        if p.join("core.kl").exists() {
            return Ok(p);
        }
    }
    let cwd = std::env::current_dir().map_err(|e| ShenError::new(format!("getcwd: {e}")))?;
    let mut dir = cwd.clone();
    for _ in 0..12 {
        let candidate = dir.join("kernel").join("klambda");
        if candidate.join("core.kl").exists() {
            return Ok(candidate);
        }
        if !dir.pop() {
            break;
        }
    }
    let last = cwd.join("kernel").join("klambda");
    if last.join("core.kl").exists() {
        return Ok(last);
    }
    Err(ShenError::new(
        "could not locate kernel/klambda; set SHEN_KERNEL_DIR or run from the shen-rust root",
    ))
}

/// Full boot sequence. After this returns `Ok(())`, the interpreter is
/// running a fully-initialised Shen kernel.
pub fn boot(interp: &mut Interp) -> ShenResult<()> {
    let dir = find_kernel_dir()?;
    boot_with_kernel(interp, &dir)
}

pub fn boot_with_kernel(interp: &mut Interp, kernel_dir: &Path) -> ShenResult<()> {
    // Witness the generated guard types so a shengen-rust signature
    // change breaks Gate 2 (`cargo build`) here. See
    // `interp::guard_types_link` for the rationale.
    crate::interp::guard_types_link::witness();

    set_port_metadata(interp);
    set_home_directory(interp)?;
    set_standard_streams(interp);

    for name in KERNEL_FILES {
        load_kl_file(interp, &kernel_dir.join(name))?;
    }

    run_shen_initialise(interp)?;
    register_all_metadata(interp)?;

    // Install AOT-compiled versions of every kernel function over the
    // tree-walked ones. The kernel's property-vector setup happens
    // during `shen.initialise` (above), which uses the tree-walked
    // defuns; only the function-namespace closures get replaced here.
    // `SHEN_RUST_NO_AOT` (env var) disables this — useful when
    // debugging whether a bug is in the AOT codegen or the tree-walker.
    if std::env::var_os("SHEN_RUST_NO_AOT").is_none() {
        crate::aot::kernel::install_all(interp);
    }

    // After AOT installs, override the small set of upstream
    // call-frequency leaders (`element?`, `shen.pvar?`,
    // `shen.lazyderef`, `fail`, `value/or`, `<-address/or`, bulk file
    // I/O) with native Rust. See `register_hot_overrides` for rationale.
    crate::primitives::register_hot_overrides(interp);

    // Stage J1: tier one hand-JIT'd kernel leaf (`shen.length-h`) into the
    // direct-call table, overriding its AOT version. Gated by the `jit` feature
    // *and* the `SHEN_RUST_JIT` env var, so it's a no-op unless both are on.
    // See `design/jit-j1-handoff.md`.
    #[cfg(feature = "jit")]
    crate::jit::install_jit(interp);

    // L1 load-time typecheck memoization, off unless `SHEN_RUST_TC_CACHE`
    // is set. Installed last so its `load` / `shen.typecheck` wrappers
    // land over the AOT registrations above. See `interp::tc_cache`.
    crate::interp::tc_cache::install_from_env(interp, kernel_dir);

    Ok(())
}

/// Boot-from-subset (Ratatoskr stage 2): bring up an interpreter from a
/// single shaken `kernel.kl` (KL source text) instead of the 21 vendored
/// kernel files. The sequence mirrors `boot_with_kernel` exactly, minus
/// the on-disk kernel dir, the JIT tier, and the tc-cache:
///
/// 1. port metadata / home dir / standard streams,
/// 2. tree-walk every form in `kernel_src` (defuns only, per the 41.x
///    contract — registering closures is cheap),
/// 3. `(shen.initialise)`,
/// 4. primitive arity / `shen.lambda-form` metadata on `*property-vector*`,
/// 5. the caller's AOT installer (klcompile output for the same shaken
///    kernel) over the tree-walked closures — partial coverage is fine,
///    skipped defuns stay tree-walked,
/// 6. `register_hot_overrides` LAST so the native fast paths
///    (`shen.lazyderef`, `shen.pvar?`, `fail`, …) shadow the AOT defuns.
pub fn boot_from_kl_source(
    interp: &mut Interp,
    kernel_src: &str,
    install_aot: Option<fn(&mut Interp)>,
) -> ShenResult<()> {
    crate::interp::guard_types_link::witness();

    set_port_metadata(interp);
    set_home_directory(interp)?;
    set_standard_streams(interp);

    eval_kl_source(interp, kernel_src, "shaken kernel", &[])?;

    run_shen_initialise(interp)?;
    register_all_metadata(interp)?;

    if let Some(install) = install_aot {
        install(interp);
    }

    crate::primitives::register_hot_overrides(interp);
    Ok(())
}

/// Parse `src` as KL and evaluate every top-level form in order, except
/// `(defun NAME …)` forms whose NAME is in `skip_defuns` (used by the
/// Ratatoskr builder to avoid re-tree-walking defuns its generated AOT
/// module already registered). `label` is used in error messages only.
pub fn eval_kl_source(
    interp: &mut Interp,
    src: &str,
    label: &str,
    skip_defuns: &[&str],
) -> ShenResult<()> {
    let forms = parse_all(src, &mut interp.symbols)
        .map_err(|e| ShenError::new(format!("parse {label}: {e}")))?;
    for (i, form) in forms.iter().enumerate() {
        if !skip_defuns.is_empty() {
            if let Some(name) = top_level_defun_name(interp, form) {
                if skip_defuns.iter().any(|s| *s == name) {
                    continue;
                }
            }
        }
        interp
            .eval(form)
            .map_err(|e| ShenError::new(format!("{label}: form {} (1-based): {e}", i + 1)))?;
    }
    Ok(())
}

/// If `form` is `(defun NAME …)`, return NAME's text.
fn top_level_defun_name(interp: &Interp, form: &crate::kl::ast::KlExpr) -> Option<String> {
    use crate::kl::ast::KlExpr;
    let KlExpr::App(items) = form else {
        return None;
    };
    if items.len() != 4 {
        return None;
    }
    let KlExpr::Sym(head) = &items[0] else {
        return None;
    };
    if interp.resolve(*head) != "defun" {
        return None;
    }
    let KlExpr::Sym(name) = &items[1] else {
        return None;
    };
    Some(interp.resolve(*name).to_string())
}

/// Publish `arity` + `shen.lambda-form` entries on `*property-vector*`
/// for already-registered functions, exactly as `register_all_metadata`
/// does for primitives. The Ratatoskr builder calls this for user defuns
/// (manifest `fn=` lines) after installing their AOT module, so
/// `(fn NAME)` / partial application resolve for user code too.
pub fn register_fn_metadata(interp: &mut Interp, fns: &[(&str, usize)]) -> ShenResult<()> {
    register_metadata_entries(interp, fns.iter().copied())
}

/// Bind `*stinput*`, `*stoutput*`, `*sterror*` to host stdio streams.
/// The kernel reads these inside `shen.initialise` to install the default
/// `stinput`/`stoutput` global accessor functions.
fn set_standard_streams(interp: &mut Interp) {
    let stdin = Value::stream(Rc::new(RefCell::new(Stream::In(
        Box::new(std::io::stdin()),
    ))));
    let stdout = Value::stream(Rc::new(RefCell::new(Stream::Out(Box::new(
        std::io::stdout(),
    )))));
    let stderr = Value::stream(Rc::new(RefCell::new(Stream::Out(Box::new(
        std::io::stderr(),
    )))));
    let stin = interp.intern("*stinput*");
    let stout = interp.intern("*stoutput*");
    let sterr = interp.intern("*sterror*");
    interp.env.set_global(stin, stdin);
    interp.env.set_global(stout, stdout);
    interp.env.set_global(sterr, stderr);
}

/// Set globals the kernel reads at startup. `*version*` reflects the
/// upstream Shen kernel; `*language*` / `*implementation*` / `*port*` /
/// `*porters*` identify this port.
fn set_port_metadata(interp: &mut Interp) {
    let pairs: &[(&str, &str)] = &[
        ("*version*", "41.2"),
        ("*language*", "Rust"),
        ("*implementation*", "shen-rust"),
        ("*release*", "0.1.0"),
        ("*port*", "0.1.0"),
        ("*porters*", "Reuben Brooks"),
        ("*os*", os_name()),
    ];
    for (name, value) in pairs {
        let sym = interp.intern(name);
        interp.env.set_global(sym, Value::str(Rc::from(*value)));
    }
    let tc_sym = interp.intern("*tc*");
    interp.env.set_global(tc_sym, Value::bool(false));
}

fn os_name() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        "macOS"
    }
    #[cfg(target_os = "linux")]
    {
        "Linux"
    }
    #[cfg(target_os = "windows")]
    {
        "Windows"
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        "Unknown"
    }
}

fn set_home_directory(interp: &mut Interp) -> ShenResult<()> {
    let cwd = std::env::current_dir().map_err(|e| ShenError::new(format!("getcwd: {e}")))?;
    let mut home = cwd.to_string_lossy().into_owned();
    if !home.ends_with('/') {
        home.push('/');
    }
    let sym = interp.intern("*home-directory*");
    interp.env.set_global(sym, Value::str(Rc::from(home)));
    Ok(())
}

fn load_kl_file(interp: &mut Interp, path: &Path) -> ShenResult<()> {
    let src =
        fs::read_to_string(path).map_err(|e| ShenError::new(format!("read {path:?}: {e}")))?;
    let forms = parse_all(&src, &mut interp.symbols)
        .map_err(|e| ShenError::new(format!("parse {path:?}: {e}")))?;
    for (i, form) in forms.iter().enumerate() {
        interp
            .eval(form)
            .map_err(|e| ShenError::new(format!("{path:?}: form {} (1-based): {e}", i + 1)))?;
    }
    Ok(())
}

fn run_shen_initialise(interp: &mut Interp) -> ShenResult<()> {
    let sym = interp.intern("shen.initialise");
    let f = interp
        .env
        .get_fn(sym)
        .cloned()
        .ok_or_else(|| ShenError::new("shen.initialise not defined after kernel load"))?;
    interp
        .apply(f, vec![])
        .map_err(|e| ShenError::new(format!("shen.initialise: {e}")))?;
    Ok(())
}

/// After `shen.initialise` runs, `*property-vector*` and the kernel's
/// `put` function are defined. Call `(put NAME 'arity ARITY *pv*)` and,
/// when the primitive has positive arity, `(put NAME 'shen.lambda-form
/// CLOSURE *pv*)` so `(fn NAME)` lookups succeed.
fn register_all_metadata(interp: &mut Interp) -> ShenResult<()> {
    let all = PRIMITIVE_METADATA
        .iter()
        .chain(crate::cedar::primitives::CEDAR_PRIMITIVES.iter())
        .map(|(name, arity)| (*name, *arity));
    register_metadata_entries(interp, all)
}

fn register_metadata_entries<'a>(
    interp: &mut Interp,
    entries: impl Iterator<Item = (&'a str, usize)>,
) -> ShenResult<()> {
    let pv_sym = interp.intern("*property-vector*");
    let pv = match interp.env.get_global(pv_sym).cloned() {
        Some(v) => v,
        None => return Ok(()), // kernel didn't set it — nothing to do
    };
    let put_sym = interp.intern("put");
    let put = match interp.env.get_fn(put_sym).cloned() {
        Some(v) => v,
        None => return Ok(()),
    };
    let arity_sym = interp.intern("arity");
    let lambda_form_sym = interp.intern("shen.lambda-form");

    for (name, arity) in entries {
        let name_sym = interp.intern(name);
        let closure = match interp.env.get_fn(name_sym).cloned() {
            Some(v) => v,
            None => continue,
        };
        // (put NAME 'arity ARITY *pv*)
        let args = vec![
            Value::sym(name_sym),
            Value::sym(arity_sym),
            Value::int(arity as i64),
            pv.clone(),
        ];
        interp
            .apply(put.clone(), args)
            .map_err(|e| ShenError::new(format!("register arity for {name}: {e}")))?;
        if arity > 0 {
            // (put NAME 'shen.lambda-form CLOSURE *pv*)
            let args = vec![
                Value::sym(name_sym),
                Value::sym(lambda_form_sym),
                closure,
                pv.clone(),
            ];
            interp.apply(put.clone(), args).map_err(|e| {
                ShenError::new(format!("register shen.lambda-form for {name}: {e}"))
            })?;
        }
    }
    Ok(())
}
