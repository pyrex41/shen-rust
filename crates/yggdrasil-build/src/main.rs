//! Yggdrasil stage-2 builder (Rust target).
//!
//! `yggdrasil-build <shaken-dir> <outdir>` reads a Yggdrasil shaken
//! directory (manifest v2: `yggdrasil.manifest.txt`, a shaken
//! `kernel.kl`, user `.kl` files) and scaffolds a standalone Cargo
//! project in `<outdir>`:
//!
//! - `src/kernel_aot.rs` — klcompile AOT module for the shaken kernel,
//! - `src/user_*.rs`     — klcompile AOT module per user `.kl` file,
//! - `src/kl/*.kl`       — the KL sources, embedded via `include_str!`,
//! - `src/main.rs`       — boots a minimal Interp from ONLY the shaken
//!   kernel (`shen_rust::interp::boot::boot_from_kl_source`), then runs
//!   each user file's top-level forms in source order (defuns that the
//!   AOT module covers are skipped; everything else tree-walks),
//! - `Cargo.toml`        — path-dep on this repo's `crates/shen-rust`.
//!
//! `cargo build --release` in `<outdir>` yields a single static binary.
//!
//! Defuns klcompile cannot compile (over the node budget, or in the
//! KERNEL_SLOW_DEFUNS list) are simply left tree-walked: the generated
//! main evaluates every kernel form before installing the AOT module,
//! so partial AOT coverage degrades gracefully, exactly as in the
//! normal 21-file boot.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use klcompile::{compile_kl, CompileOptions, SkipPolicy, KERNEL_SLOW_DEFUNS};

/// Parsed Yggdrasil manifest v2 (`key=value` lines; unknown keys ignored).
#[derive(Debug, Default)]
struct Manifest {
    kernel: String,
    init: String,
    /// User `.kl` files, in load order.
    users: Vec<String>,
    /// `fn=<name> <arity>` lines: one per user defun.
    fns: Vec<(String, usize)>,
}

fn parse_manifest(path: &Path) -> Result<Manifest, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let mut m = Manifest {
        kernel: "kernel.kl".to_string(),
        init: "shen.initialise".to_string(),
        ..Default::default()
    };
    for (lineno, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(format!(
                "{}:{}: not a key=value line: {line:?}",
                path.display(),
                lineno + 1
            ));
        };
        match key {
            "manifest-version" => {
                if value != "2" {
                    return Err(format!("unsupported manifest-version {value:?} (expected 2)"));
                }
            }
            "kernel" => m.kernel = value.to_string(),
            "init" => m.init = value.to_string(),
            "user" => m.users.push(value.to_string()),
            "fn" => {
                let (name, arity) = value
                    .rsplit_once(' ')
                    .ok_or_else(|| format!("bad fn= line: {value:?}"))?;
                let arity: usize = arity
                    .parse()
                    .map_err(|e| format!("bad arity in fn= line {value:?}: {e}"))?;
                m.fns.push((name.to_string(), arity));
            }
            // kernel-version, primitive, primitive-optional, global,
            // needs-eval: informational for this port (primitives and
            // eval-kl are always compiled in; *stinput*/*stoutput* are
            // always bound by boot). Unknown keys: ignored per contract.
            _ => {}
        }
    }
    if m.users.is_empty() {
        return Err("manifest has no user= entries".to_string());
    }
    Ok(m)
}

/// klcompile options for modules generated into the scaffolded crate:
/// `shen_rust::` imports, standalone-module lint header, the kernel's
/// known-slow skip list plus the external body-size budget. Skipped
/// defuns stay tree-walked.
fn compile_options(label: &str) -> CompileOptions {
    CompileOptions {
        import_prefix: "shen_rust".to_string(),
        emit_inner_lints: true,
        external_lint_set: true,
        source_label: label.to_string(),
        skip: SkipPolicy {
            max_body_nodes: Some(3000),
            force_skip: KERNEL_SLOW_DEFUNS.iter().map(|s| s.to_string()).collect(),
        },
        emit_manifest: None,
    }
}

/// Sanitize a user-file stem into a Rust module/ident suffix.
fn ident_suffix(stem: &str) -> String {
    let mut s: String = stem
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '_' })
        .collect();
    if s.chars().next().is_none_or(|c| c.is_ascii_digit()) {
        s.insert(0, 'f');
    }
    s
}

/// Sanitize an outdir basename into a Cargo package name.
fn package_name(outdir: &Path) -> String {
    let raw = outdir
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut name: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    if name.is_empty() || name.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        name = format!("yggdrasil-{name}");
    }
    name
}

/// Locate `crates/shen-rust` relative to this builder's own manifest dir
/// (compiled in), overridable with `YGGDRASIL_SHEN_RUST` for relocated
/// installs.
fn shen_rust_path() -> Result<PathBuf, String> {
    if let Ok(p) = env::var("YGGDRASIL_SHEN_RUST") {
        let p = PathBuf::from(p);
        if p.join("Cargo.toml").exists() {
            return Ok(p);
        }
        return Err(format!("YGGDRASIL_SHEN_RUST={}: no Cargo.toml there", p.display()));
    }
    let here = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let candidate = here.join("..").join("shen-rust");
    let candidate = candidate
        .canonicalize()
        .map_err(|e| format!("canonicalize {}: {e}", candidate.display()))?;
    if candidate.join("Cargo.toml").exists() {
        Ok(candidate)
    } else {
        Err(format!(
            "could not find crates/shen-rust (looked at {}); set YGGDRASIL_SHEN_RUST",
            candidate.display()
        ))
    }
}

struct UserModule {
    /// Rust module name (`user_<stem>`).
    module: String,
    /// KL file name as named in the manifest (and copied under src/kl/).
    kl_file: String,
    /// Defun names the AOT module covers (skipped during tree-walk).
    compiled: Vec<String>,
}

fn run() -> Result<(), String> {
    let args: Vec<String> = env::args().collect();
    if args.len() != 3 {
        return Err("Usage: yggdrasil-build <shaken-dir> <outdir>".to_string());
    }
    let shaken = PathBuf::from(&args[1]);
    let outdir = PathBuf::from(&args[2]);

    let manifest_path = shaken.join("yggdrasil.manifest.txt");
    let manifest = parse_manifest(&manifest_path)?;
    if manifest.init != "shen.initialise" {
        // boot_from_kl_source hard-codes shen.initialise (41.1 contract).
        return Err(format!("unsupported init function {:?}", manifest.init));
    }

    let src_dir = outdir.join("src");
    let kl_dir = src_dir.join("kl");
    fs::create_dir_all(&kl_dir).map_err(|e| format!("mkdir {}: {e}", kl_dir.display()))?;

    // --- kernel: copy + AOT-compile ---------------------------------
    let kernel_src_path = shaken.join(&manifest.kernel);
    let kernel_src = fs::read_to_string(&kernel_src_path)
        .map_err(|e| format!("read {}: {e}", kernel_src_path.display()))?;
    fs::write(kl_dir.join("kernel.kl"), &kernel_src)
        .map_err(|e| format!("write kernel.kl: {e}"))?;

    let opts = compile_options(&format!("{} (Yggdrasil shaken kernel)", manifest.kernel));
    let (kernel_module, kernel_report) =
        compile_kl(&kernel_src, &opts).map_err(|e| format!("klcompile {}: {e}", manifest.kernel))?;
    fs::write(src_dir.join("kernel_aot.rs"), kernel_module)
        .map_err(|e| format!("write kernel_aot.rs: {e}"))?;
    eprintln!(
        "kernel: {} defuns AOT-compiled, {} left tree-walked",
        kernel_report.compiled.len(),
        kernel_report.skipped.len()
    );
    for s in &kernel_report.skipped {
        eprintln!("  tree-walked: {s}");
    }

    // --- user files: copy + AOT-compile ------------------------------
    let mut user_modules = Vec::new();
    for user in &manifest.users {
        let path = shaken.join(user);
        let src = fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
        let kl_name = Path::new(user)
            .file_name()
            .ok_or_else(|| format!("bad user file name {user:?}"))?
            .to_string_lossy()
            .into_owned();
        fs::write(kl_dir.join(&kl_name), &src).map_err(|e| format!("write {kl_name}: {e}"))?;

        let stem = Path::new(&kl_name)
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let module = format!("user_{}", ident_suffix(&stem));
        let opts = compile_options(&format!("{user} (Yggdrasil user code)"));
        let (module_src, report) =
            compile_kl(&src, &opts).map_err(|e| format!("klcompile {user}: {e}"))?;
        fs::write(src_dir.join(format!("{module}.rs")), module_src)
            .map_err(|e| format!("write {module}.rs: {e}"))?;
        eprintln!(
            "{user}: {} defuns AOT-compiled ({} other top-level forms tree-walk at startup)",
            report.compiled.len(),
            report.skipped.len()
        );
        user_modules.push(UserModule {
            module,
            kl_file: kl_name,
            compiled: report.compiled,
        });
    }

    // --- main.rs ------------------------------------------------------
    let main_rs = generate_main(&manifest, &user_modules);
    fs::write(src_dir.join("main.rs"), main_rs).map_err(|e| format!("write main.rs: {e}"))?;

    // --- Cargo.toml / .gitignore --------------------------------------
    let shen_rust = shen_rust_path()?;
    let cargo_toml = format!(
        r#"# Auto-generated by yggdrasil-build. DO NOT EDIT.
[package]
name = "{name}"
version = "0.1.0"
edition = "2021"

[dependencies]
shen-rust = {{ path = "{shen_rust}" }}

# Standalone: never join an enclosing workspace.
[workspace]

[profile.release]
lto = "thin"
opt-level = 3
codegen-units = 1
"#,
        name = package_name(&outdir),
        shen_rust = shen_rust.display(),
    );
    fs::write(outdir.join("Cargo.toml"), cargo_toml)
        .map_err(|e| format!("write Cargo.toml: {e}"))?;
    fs::write(outdir.join(".gitignore"), "/target\n").ok();

    eprintln!("scaffolded {}", outdir.display());
    eprintln!("build: cargo build --release --manifest-path {}/Cargo.toml", outdir.display());
    Ok(())
}

fn generate_main(manifest: &Manifest, users: &[UserModule]) -> String {
    let mut out = String::new();
    out.push_str(
        "//! Auto-generated by yggdrasil-build. DO NOT EDIT.\n\
         //!\n\
         //! Boots a minimal shen-rust Interp from the shaken kernel only\n\
         //! (no 21-file kernel), runs (shen.initialise), installs the AOT\n\
         //! modules, then evaluates each user file's top-level forms in\n\
         //! source order.\n\n",
    );
    out.push_str("mod kernel_aot;\n");
    for u in users {
        out.push_str(&format!("mod {};\n", u.module));
    }
    out.push_str("\nuse shen_rust::interp::boot;\nuse shen_rust::interp::eval::Interp;\n\n");
    out.push_str("const KERNEL_KL: &str = include_str!(\"kl/kernel.kl\");\n");
    for (i, u) in users.iter().enumerate() {
        out.push_str(&format!(
            "const USER_KL_{i}: &str = include_str!({:?});\n",
            format!("kl/{}", u.kl_file)
        ));
        out.push_str(&format!("const USER_SKIP_{i}: &[&str] = &[\n"));
        for name in &u.compiled {
            out.push_str(&format!("    {name:?},\n"));
        }
        out.push_str("];\n");
    }
    out.push_str("\n/// Manifest `fn=` lines: arity + shen.lambda-form metadata for user\n/// defuns, so `(fn NAME)` / partial application resolve.\n");
    out.push_str("const USER_FN_METADATA: &[(&str, usize)] = &[\n");
    for (name, arity) in &manifest.fns {
        out.push_str(&format!("    ({name:?}, {arity}),\n"));
    }
    out.push_str("];\n\n");

    out.push_str("fn run() -> Result<(), String> {\n");
    out.push_str("    let mut interp = Interp::new();\n");
    out.push_str(
        "    boot::boot_from_kl_source(&mut interp, KERNEL_KL, Some(kernel_aot::install))\n\
         \x20       .map_err(|e| format!(\"boot: {e}\"))?;\n",
    );
    for u in users {
        out.push_str(&format!("    {}::install(&mut interp);\n", u.module));
    }
    out.push_str(
        "    boot::register_fn_metadata(&mut interp, USER_FN_METADATA)\n\
         \x20       .map_err(|e| format!(\"fn metadata: {e}\"))?;\n",
    );
    for (i, u) in users.iter().enumerate() {
        out.push_str(&format!(
            "    boot::eval_kl_source(&mut interp, USER_KL_{i}, {:?}, USER_SKIP_{i})\n\
             \x20       .map_err(|e| format!(\"{{e}}\"))?;\n",
            u.kl_file
        ));
    }
    out.push_str("    Ok(())\n}\n\n");

    out.push_str(
        "fn main() {\n\
         \x20   // User code (e.g. prolog CPS) may recurse deeply: run on a\n\
         \x20   // 1 GB stack thread, like bin/shen-rust does.\n\
         \x20   let handle = std::thread::Builder::new()\n\
         \x20       .name(\"shen\".to_string())\n\
         \x20       .stack_size(1024 * 1024 * 1024)\n\
         \x20       .spawn(run)\n\
         \x20       .expect(\"spawn shen thread\");\n\
         \x20   match handle.join() {\n\
         \x20       Ok(Ok(())) => {}\n\
         \x20       Ok(Err(e)) => {\n\
         \x20           eprintln!(\"error: {e}\");\n\
         \x20           std::process::exit(1);\n\
         \x20       }\n\
         \x20       Err(_) => {\n\
         \x20           eprintln!(\"error: shen thread panicked\");\n\
         \x20           std::process::exit(101);\n\
         \x20       }\n\
         \x20   }\n\
         }\n",
    );
    out
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("yggdrasil-build: {e}");
            ExitCode::from(1)
        }
    }
}
