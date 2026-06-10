//! klcompile: compile KL `(defun ...)` forms to Rust function bodies.
//!
//! Library core behind the `klcompile` CLI. Input: KL source text.
//! Output: a Rust module with one `pub fn aot_<name>(...)` per defun,
//! plus `pub fn install(interp: &mut Interp)` that registers them all
//! (`register_native` first, then `register_aot_direct` — that order is
//! load-bearing, see `Interp::register_aot_direct`).
//!
//! The emitted code calls `shen_rust::aot::runtime::*` helpers for
//! anything that crosses a function boundary. Plain named calls go
//! through `rt::apply_direct`, which hits a raw fn pointer table for
//! every AOT-compiled function (populated at install time via
//! `register_aot_direct`). This gives the fast direct AOT-to-AOT path
//! without per-module cross references in the generated code.
//!
//! ## Byte-stability invariants (Gate 6 pins the kernel output)
//!
//! - The fresh-temp `__tN` counter is per-module and never resets
//!   between defuns.
//! - `captures_used` iterates a `HashSet` (nondeterministic order); the
//!   `captures.sort()` immediately after each call site is the ONLY
//!   thing making lambda/freeze output stable across runs. Never emit
//!   unsorted captures.
//! - Duplicate defuns dedup last-wins, but emission stays in file order.
//! - The kernel header (`//!` + the exact 12-lint `#![allow]` block +
//!   `crate::` imports) must not drift; external configs change the
//!   header but the kernel default must stay byte-identical.

use std::collections::HashSet;

use shen_rust::kl::ast::KlExpr;
use shen_rust::kl::parser::parse_all;
use shen_rust::symbol::Interner;

/// How the generated module is emitted.
pub struct CompileOptions {
    /// Path prefix for the runtime imports: `"crate"` for modules that
    /// live inside shen-rust (the kernel AOT), `"shen_rust"` for modules
    /// generated into external crates (benches, overlay crates).
    pub import_prefix: String,
    /// Emit the module-level `//!` doc header and `#![allow(...)]` inner
    /// attributes. True for standalone modules (kernel AOT); false for
    /// `include!`-style embedding where the host file owns the lints
    /// (inner attrs are emitted as a plain `//` header instead).
    pub emit_inner_lints: bool,
    /// When emitting inner lints for a module outside shen-rust, add
    /// `clippy::clone_on_copy` (covered crate-wide inside shen-rust by
    /// src/lib.rs, deliberately absent from the kernel header).
    pub external_lint_set: bool,
    /// Verbatim text for the "Auto-generated from ..." header line. The
    /// CLI passes its input-path argument unchanged (the kernel headers
    /// embed the repo-relative path).
    pub source_label: String,
    /// Which defuns to leave to the loaded engine (tree-walk/VM). A
    /// skipped defun is safe: `rt::apply_direct` falls back to the env
    /// closure, so partial coverage degrades to the loaded engine.
    pub skip: SkipPolicy,
    /// Emit an overlay manifest (`KLCOMPILE_FORMAT` / `SOURCE_FNV` /
    /// `KERNEL_FNV` / `COMPILED` consts + an `overlay()` constructor for
    /// `Interp::install_overlay*`). Overlay/external configs only — the
    /// kernel AOT modules are manifest-free and byte-frozen (Gate 6).
    pub emit_manifest: Option<ManifestInfo>,
}

/// Generation-time provenance recorded into the overlay manifest. Both
/// hashes are computed by the generation driver (it knows the `.shen`
/// inputs and the booted kernel dir); klcompile only embeds them.
pub struct ManifestInfo {
    /// FNV-1a over the `.shen` source bytes, concatenated in load order
    /// (what the host can re-hash at serve time).
    pub source_fnv: u64,
    /// `shen_rust::aot::overlay::kernel_digest` of the generating boot's
    /// kernel directory.
    pub kernel_fnv: u64,
}

/// Skip policy for defuns that should not be AOT-compiled.
#[derive(Default)]
pub struct SkipPolicy {
    /// Per-defun body size budget in KL expression nodes. Defuns over
    /// budget are skipped (huge nested data constructors compile for
    /// minutes in LLVM and run once). `None` = no budget (kernel config:
    /// the legacy skip set is named, not sized — no pure threshold
    /// reproduces it).
    pub max_body_nodes: Option<usize>,
    /// Names always skipped. The kernel invocation passes its legacy
    /// known-slow list here; the compiler core hardcodes nothing.
    pub force_skip: Vec<String>,
}

impl CompileOptions {
    /// The kernel-AOT configuration: byte-identical to the historical
    /// hardcoded behavior (Gate 6 audits this).
    pub fn kernel(source_label: impl Into<String>) -> Self {
        CompileOptions {
            import_prefix: "crate".to_string(),
            emit_inner_lints: true,
            external_lint_set: false,
            source_label: source_label.into(),
            skip: SkipPolicy {
                max_body_nodes: None,
                force_skip: KERNEL_SLOW_DEFUNS.iter().map(|s| s.to_string()).collect(),
            },
            emit_manifest: None,
        }
    }

    /// Configuration for modules generated into an external crate and
    /// `include!`d (the bench/overlay shape): `shen_rust::` imports, a
    /// plain `//` header, lints owned by the host file, and a body-size
    /// budget instead of a named skip list.
    pub fn external(source_label: impl Into<String>) -> Self {
        CompileOptions {
            import_prefix: "shen_rust".to_string(),
            emit_inner_lints: false,
            external_lint_set: true,
            source_label: source_label.into(),
            skip: SkipPolicy {
                max_body_nodes: Some(3000),
                force_skip: Vec::new(),
            },
            emit_manifest: None,
        }
    }
}

/// What got compiled and what got skipped (with reasons). Skips are
/// informational — they never appear in the generated module.
pub struct CompileReport {
    /// KL names in emission (file) order.
    pub compiled: Vec<String>,
    /// Human-readable skip reasons (`"<name>: <why>"` or a description
    /// of a non-defun top-level form).
    pub skipped: Vec<String>,
}

/// Kernel defuns whose source is so large (deeply nested cons literals)
/// that LLVM optimization takes minutes per release build. They run
/// once at boot, so AOT gives no speedup — leave them tree-walked.
/// This is the KERNEL invocation's policy (see `CompileOptions::kernel`),
/// not a property of the compiler.
pub const KERNEL_SLOW_DEFUNS: &[&str] = &[
    "stlib.initialise-sources",
    "stlib.initialise-types",
    "stlib.initialise-environment",
    "stlib.initialise-arities",
];

/// Compile KL source text to a Rust module. Pure: no filesystem access.
/// One `Codegen` per call, so `__tN` numbering matches the historical
/// per-module counter exactly.
pub fn compile_kl(src: &str, opts: &CompileOptions) -> Result<(String, CompileReport), String> {
    let mut interner = Interner::new();
    let forms = parse_all(src, &mut interner).map_err(|e| format!("parse: {e}"))?;

    // KL allows multiple `(defun NAME …)` for the same NAME — last wins.
    // Find the last index per defun name and skip earlier duplicates so
    // the emitted Rust module only contains one definition per function.
    let mut last_index = std::collections::HashMap::new();
    for (i, form) in forms.iter().enumerate() {
        if let Some(name) = defun_name(form, &interner) {
            last_index.insert(name, i);
        }
    }

    let mut g = Codegen::new(&interner);
    g.emit_header(opts);
    let mut compiled = Vec::new();
    let mut skipped = Vec::new();
    let mut arities: Vec<(String, usize)> = Vec::new();
    for (i, form) in forms.iter().enumerate() {
        if let Some(name) = defun_name(form, &interner) {
            if last_index.get(&name) != Some(&i) {
                continue;
            }
            if opts.skip.force_skip.contains(&name) {
                skipped.push(format!("{name}: known-slow-to-compile (left tree-walked)"));
                continue;
            }
            if let Some(budget) = opts.skip.max_body_nodes {
                let nodes = body_node_count(form);
                if nodes > budget {
                    skipped.push(format!(
                        "{name}: body too large ({nodes} nodes > {budget} budget, left to the loaded engine)"
                    ));
                    continue;
                }
            }
        }
        match g.compile_top(form) {
            Ok((name, arity)) => {
                arities.push((name.clone(), arity));
                compiled.push(name);
            }
            Err(reason) => skipped.push(reason),
        }
    }
    g.emit_install(&compiled);
    if let Some(manifest) = &opts.emit_manifest {
        g.emit_manifest(opts, manifest, &arities);
    }

    Ok((g.finish(), CompileReport { compiled, skipped }))
}

/// Count the KL expression nodes in a defun's body (items[3]). Used by
/// `SkipPolicy::max_body_nodes`.
fn body_node_count(form: &KlExpr) -> usize {
    fn count(e: &KlExpr) -> usize {
        match e {
            KlExpr::App(items) => 1 + items.iter().map(count).sum::<usize>(),
            _ => 1,
        }
    }
    match form {
        KlExpr::App(items) if items.len() == 4 => count(&items[3]),
        _ => 0,
    }
}

struct Codegen<'a> {
    out: String,
    interner: &'a Interner,
    counter: usize,
    /// Set while compiling a `(defun NAME ...)`. Used by the tail-call
    /// detector to turn self-calls in tail position into `continue`s.
    current_fn: Option<String>,
    current_params: Vec<String>,
}

impl<'a> Codegen<'a> {
    fn new(interner: &'a Interner) -> Self {
        Self {
            out: String::new(),
            interner,
            counter: 0,
            current_fn: None,
            current_params: Vec::new(),
        }
    }

    fn emit_header(&mut self, opts: &CompileOptions) {
        if opts.emit_inner_lints {
            self.out.push_str("//! Auto-generated by `klcompile` from ");
            self.out.push_str(&opts.source_label);
            self.out.push_str(".\n");
            self.out.push_str("//!\n");
            self.out
                .push_str("//! DO NOT EDIT. Re-run klcompile to regenerate.\n\n");
            self.out
                .push_str("#![allow(unused_variables, unused_braces, unused_imports,\n");
            self.out
                .push_str("    clippy::let_and_return, clippy::needless_question_mark,\n");
            self.out
                .push_str("    clippy::redundant_clone, clippy::needless_late_init,\n");
            self.out
                .push_str("    clippy::len_zero, clippy::needless_borrow,\n");
            self.out
                .push_str("    clippy::approx_constant, clippy::redundant_closure_call,\n");
            if opts.external_lint_set {
                // shen-rust carries clone_on_copy crate-wide; external
                // crates need it on the generated module itself.
                self.out.push_str("    clippy::clone_on_copy,\n");
            }
            self.out.push_str("    non_snake_case)]\n\n");
        } else {
            // include!-style embedding: the host file owns lints; inner
            // attrs/doc comments would be illegal or misplaced, so emit a
            // plain comment header.
            self.out.push_str("// Auto-generated by `klcompile` from ");
            self.out.push_str(&opts.source_label);
            self.out.push_str(".\n");
            self.out
                .push_str("// DO NOT EDIT. Re-run klcompile to regenerate.\n\n");
        }
        let p = &opts.import_prefix;
        self.out
            .push_str(&format!("use {p}::aot::runtime as rt;\n"));
        self.out
            .push_str(&format!("use {p}::error::{{ShenError, ShenResult}};\n"));
        self.out
            .push_str(&format!("use {p}::interp::eval::Interp;\n"));
        self.out.push_str(&format!("use {p}::value::Value;\n"));
        self.out.push_str("use std::rc::Rc;\n\n");
    }

    fn emit_install(&mut self, compiled: &[String]) {
        self.out
            .push_str("/// Register every AOT-compiled function on `interp`. Call after kernel\n");
        self.out
            .push_str("/// boot so these names override the tree-walked defuns.\n");
        self.out.push_str("pub fn install(interp: &mut Interp) {\n");
        for (rust_name, kl_name) in compiled.iter().map(|n| (sanitize_ident(n), n.clone())) {
            let _ = rust_name;
            // Each entry is registered with its declared arity by referencing
            // the helper emitted below.
            self.out.push_str(&format!(
                "    install_{}(interp);\n",
                sanitize_ident(&kl_name)
            ));
        }
        self.out.push_str("}\n\n");
    }

    fn finish(self) -> String {
        self.out
    }

    fn fresh(&mut self) -> String {
        let n = self.counter;
        self.counter += 1;
        format!("__t{n}")
    }

    /// Emit the overlay manifest: provenance consts plus an `overlay()`
    /// constructor for `Interp::install_overlay*`. Only valid for
    /// external configs (the constructor names `{prefix}::aot::overlay`).
    fn emit_manifest(
        &mut self,
        opts: &CompileOptions,
        manifest: &ManifestInfo,
        arities: &[(String, usize)],
    ) {
        let p = &opts.import_prefix;
        self.out
            .push_str("/// Overlay manifest (see `aot::overlay`): provenance recorded at\n");
        self.out
            .push_str("/// generation time, checked by `Interp::install_overlay_if_match`.\n");
        self.out.push_str(&format!(
            "pub const KLCOMPILE_FORMAT: &str = {:?};\n",
            shen_rust::aot::overlay::OVERLAY_FORMAT
        ));
        self.out.push_str(&format!(
            "pub const SOURCE_FNV: u64 = {:#018x};\n",
            manifest.source_fnv
        ));
        self.out.push_str(&format!(
            "pub const KERNEL_FNV: u64 = {:#018x};\n",
            manifest.kernel_fnv
        ));
        self.out
            .push_str("pub const COMPILED: &[(&str, usize)] = &[\n");
        for (name, arity) in arities {
            self.out.push_str(&format!("    ({name:?}, {arity}),\n"));
        }
        self.out.push_str("];\n\n");
        self.out
            .push_str("/// Bundle this module as an overlay for the verified install API.\n");
        self.out.push_str(&format!(
            "pub fn overlay() -> {p}::aot::overlay::OverlayModule {{\n"
        ));
        self.out
            .push_str(&format!("    {p}::aot::overlay::OverlayModule {{\n"));
        self.out
            .push_str(&format!("        label: {:?},\n", opts.source_label));
        self.out.push_str("        format: KLCOMPILE_FORMAT,\n");
        self.out.push_str("        source_fnv: SOURCE_FNV,\n");
        self.out.push_str("        kernel_fnv: KERNEL_FNV,\n");
        self.out.push_str("        compiled: COMPILED,\n");
        self.out.push_str("        install,\n");
        self.out.push_str("    }\n}\n");
    }

    /// Compile a top-level form. Returns the function name and arity on
    /// success or a `Skipped` reason describing why klcompile bailed.
    fn compile_top(&mut self, form: &KlExpr) -> Result<(String, usize), String> {
        let items = match form {
            KlExpr::App(items) => items,
            _ => return Err("top-level non-application".into()),
        };
        if items.len() != 4 {
            return Err(format!("non-defun (length {})", items.len()));
        }
        let head = match &items[0] {
            KlExpr::Sym(s) => self.interner.resolve(*s),
            _ => return Err("head is not a symbol".into()),
        };
        if head != "defun" {
            return Err(format!("non-defun head `{head}`"));
        }
        let name = match &items[1] {
            KlExpr::Sym(s) => self.interner.resolve(*s).to_string(),
            _ => return Err("defun name not a symbol".into()),
        };
        let params: Vec<String> = match &items[2] {
            KlExpr::Nil => Vec::new(),
            KlExpr::App(ps) => {
                let mut out = Vec::with_capacity(ps.len());
                for p in ps.iter() {
                    match p {
                        KlExpr::Sym(s) => out.push(self.interner.resolve(*s).to_string()),
                        _ => return Err("defun param not a symbol".into()),
                    }
                }
                out
            }
            _ => return Err("defun params not a list".into()),
        };
        let body = &items[3];

        let mut scope = Scope::new();
        for p in &params {
            scope.bind(p);
        }

        self.current_fn = Some(name.clone());
        self.current_params = params.clone();
        let body_src = match self.compile_tail(body, &mut scope) {
            Ok(s) => s,
            Err(e) => return Err(format!("`{name}`: {e}")),
        };
        self.current_fn = None;
        self.current_params.clear();

        let rs_name = sanitize_ident(&name);
        self.out
            .push_str(&format!("/// AOT-compiled from KL `(defun {name} ...)`\n"));
        self.out.push_str(&format!(
            "pub fn aot_{rs_name}(interp: &mut Interp, args: &[Value]) -> ShenResult<Value> {{\n"
        ));
        self.out
            .push_str(&format!("    if args.len() != {} {{\n", params.len()));
        self.out
            .push_str("        return Err(ShenError::new(format!(\n");
        self.out.push_str(&format!(
            "            \"{name}: expected {} args, got {{}}\", args.len())));\n",
            params.len()
        ));
        self.out.push_str("    }\n");
        // Params are `mut` because tail-self-calls reassign them in
        // place. Suppress the `unused_mut` warning for cases where the
        // generator doesn't actually emit a tail-self-call.
        for (i, p) in params.iter().enumerate() {
            self.out.push_str(&format!(
                "    #[allow(unused_mut)] let mut {} = args[{i}].clone();\n",
                rust_var(p)
            ));
        }
        // The body is a `loop { ... }`. compile_tail emits either
        // `break Ok(value);` for terminal expressions or
        // `{ assign params; continue; }` for self-tail-calls.
        self.out
            .push_str("    #[allow(clippy::never_loop)] loop {\n");
        self.out.push_str(&format!("        {body_src}\n"));
        self.out.push_str("    }\n");
        self.out.push_str("}\n\n");

        // Installer helper.
        self.out
            .push_str(&format!("fn install_{rs_name}(interp: &mut Interp) {{\n"));
        self.out.push_str(&format!(
            "    interp.register_native(\"{name}\", {}, aot_{rs_name});\n",
            params.len()
        ));
        // Also populate the direct-call table for the ultra-fast path.
        self.out.push_str(&format!(
            "    interp.register_aot_direct(\"{name}\", aot_{rs_name});\n"
        ));
        self.out.push_str("}\n\n");

        Ok((name, params.len()))
    }

    /// Compile a KL expression at tail position. Output is a Rust
    /// *statement block* that either `break`s the enclosing `loop` with
    /// `Ok(value)` or `continue`s after re-binding params (for
    /// self-tail-calls). The trailing-semicolon contract is the caller's
    /// responsibility (we generate one ourselves where needed).
    fn compile_tail(&mut self, expr: &KlExpr, scope: &mut Scope) -> Result<String, String> {
        if let KlExpr::App(items) = expr {
            if !items.is_empty() {
                let head_name = if let KlExpr::Sym(s) = &items[0] {
                    Some(self.interner.resolve(*s).to_string())
                } else {
                    None
                };
                if let Some(head) = head_name.as_deref() {
                    match head {
                        "if" => return self.compile_tail_if(&items[1..], scope),
                        "let" => return self.compile_tail_let(&items[1..], scope),
                        "cond" => return self.compile_tail_cond(&items[1..], scope),
                        "do" => return self.compile_tail_do(&items[1..], scope),
                        "and" => return self.compile_tail_and(&items[1..], scope),
                        "or" => return self.compile_tail_or(&items[1..], scope),
                        "type" => {
                            if items.len() < 2 {
                                return Err("type: needs at least 1 arg".into());
                            }
                            return self.compile_tail(&items[1], scope);
                        }
                        _ => {}
                    }
                    // Self-tail-call?
                    if self.current_fn.as_deref() == Some(head)
                        && !scope.has(head)
                        && items.len() - 1 == self.current_params.len()
                    {
                        return self.compile_self_tail_call(&items[1..], scope);
                    }
                }
            }
        }
        // Default: compute as a value expression and `break Ok(value)`.
        let src = self.compile_expr(expr, scope)?;
        Ok(format!("break Ok({src});"))
    }

    fn compile_self_tail_call(
        &mut self,
        args: &[KlExpr],
        scope: &mut Scope,
    ) -> Result<String, String> {
        let (binds, names) = self.bind_args(args, scope)?;
        let params = self.current_params.clone();
        let mut assigns = String::new();
        for (p, n) in params.iter().zip(names.iter()) {
            assigns.push_str(&format!("{} = {n}; ", rust_var(p)));
        }
        Ok(format!("{{ {binds}{assigns}continue; }}"))
    }

    fn compile_tail_if(&mut self, args: &[KlExpr], scope: &mut Scope) -> Result<String, String> {
        if args.len() != 3 {
            return Err("if: expected 3 args".into());
        }
        let c = self.compile_expr(&args[0], scope)?;
        let tv = self.fresh();
        let t = self.compile_tail(&args[1], scope)?;
        let e = self.compile_tail(&args[2], scope)?;
        Ok(format!(
            "{{ let {tv} = {c}; if match rt::is_truthy(interp, &{tv}) {{ Ok(b) => b, Err(e) => break Err(e), }} {{ {t} }} else {{ {e} }} }}"
        ))
    }

    fn compile_tail_let(&mut self, args: &[KlExpr], scope: &mut Scope) -> Result<String, String> {
        if args.len() != 3 {
            return Err("let: expected 3 args".into());
        }
        let var = match &args[0] {
            KlExpr::Sym(s) => self.interner.resolve(*s).to_string(),
            _ => return Err("let: var must be symbol".into()),
        };
        let value_src = self.compile_expr(&args[1], scope)?;
        scope.bind(&var);
        let body_src = self.compile_tail(&args[2], scope)?;
        scope.unbind(&var);
        Ok(format!(
            "{{ let {} = {value_src}; {body_src} }}",
            rust_var(&var)
        ))
    }

    fn compile_tail_cond(&mut self, args: &[KlExpr], scope: &mut Scope) -> Result<String, String> {
        let mut acc = String::from("break Err(ShenError::new(\"cond: no clause matched\"));");
        for clause in args.iter().rev() {
            let items = match clause {
                KlExpr::App(items) if items.len() == 2 => items,
                _ => return Err("cond: clause must be (test action)".into()),
            };
            let t = self.compile_expr(&items[0], scope)?;
            let v = self.compile_tail(&items[1], scope)?;
            let tv = self.fresh();
            acc = format!(
                "{{ let {tv} = {t}; if match rt::is_truthy(interp, &{tv}) {{ Ok(b) => b, Err(e) => break Err(e), }} {{ {v} }} else {{ {acc} }} }}"
            );
        }
        Ok(acc)
    }

    fn compile_tail_do(&mut self, args: &[KlExpr], scope: &mut Scope) -> Result<String, String> {
        if args.is_empty() {
            return Ok("break Ok(Value::nil());".into());
        }
        let mut block = String::from("{ ");
        for (i, a) in args.iter().enumerate() {
            if i + 1 == args.len() {
                let tail = self.compile_tail(a, scope)?;
                block.push_str(&tail);
            } else {
                let src = self.compile_expr(a, scope)?;
                block.push_str(&format!("let _ = {src}; "));
            }
        }
        block.push_str(" }");
        Ok(block)
    }

    fn compile_tail_and(&mut self, args: &[KlExpr], scope: &mut Scope) -> Result<String, String> {
        if args.len() != 2 {
            return Err("and: expected 2 args".into());
        }
        let a = self.compile_expr(&args[0], scope)?;
        let av = self.fresh();
        let tail_b = self.compile_tail(&args[1], scope)?;
        Ok(format!(
            "{{ let {av} = {a}; if !match rt::is_truthy(interp, &{av}) {{ Ok(b) => b, Err(e) => break Err(e), }} {{ break Ok(Value::bool(false)); }} else {{ {tail_b} }} }}"
        ))
    }

    fn compile_tail_or(&mut self, args: &[KlExpr], scope: &mut Scope) -> Result<String, String> {
        if args.len() != 2 {
            return Err("or: expected 2 args".into());
        }
        let a = self.compile_expr(&args[0], scope)?;
        let av = self.fresh();
        let tail_b = self.compile_tail(&args[1], scope)?;
        Ok(format!(
            "{{ let {av} = {a}; if match rt::is_truthy(interp, &{av}) {{ Ok(b) => b, Err(e) => break Err(e), }} {{ break Ok(Value::bool(true)); }} else {{ {tail_b} }} }}"
        ))
    }

    /// Compile a KL expression. Returns a Rust expression string that
    /// evaluates to `Value`. May use `?` against `ShenResult<Value>`
    /// values — the caller is responsible for being inside a function
    /// returning `ShenResult<Value>`.
    fn compile_expr(&mut self, expr: &KlExpr, scope: &mut Scope) -> Result<String, String> {
        Ok(match expr {
            KlExpr::Nil => "Value::nil()".into(),
            KlExpr::Bool(b) => format!("Value::bool({b})"),
            KlExpr::Int(n) => format!("Value::int({n}i64)"),
            KlExpr::Float(x) => format!("Value::float({x}f64)"),
            KlExpr::Str(s) => format!("Value::str({:?})", s.as_ref()),
            KlExpr::Sym(id) => {
                let name = self.interner.resolve(*id);
                if scope.has(name) {
                    format!("{}.clone()", rust_var(name))
                } else {
                    // Free symbol — innocent symbol semantics: evaluate
                    // to the symbol value itself.
                    format!("Value::sym(interp.intern({:?}))", name)
                }
            }
            KlExpr::App(items) => self.compile_app(items, scope)?,
        })
    }

    fn compile_app(&mut self, items: &[KlExpr], scope: &mut Scope) -> Result<String, String> {
        if items.is_empty() {
            return Ok("Value::nil()".into());
        }
        let head_name = match &items[0] {
            KlExpr::Sym(s) => Some(self.interner.resolve(*s).to_string()),
            _ => None,
        };
        let args = &items[1..];

        if let Some(head) = head_name.as_deref() {
            match head {
                "if" => return self.compile_if(args, scope),
                "let" => return self.compile_let(args, scope),
                "cond" => return self.compile_cond(args, scope),
                "do" => return self.compile_do(args, scope),
                "and" => return self.compile_and(args, scope),
                "or" => return self.compile_or(args, scope),
                "lambda" => return self.compile_lambda(args, scope),
                "freeze" => return self.compile_freeze(args, scope),
                "thaw" => return self.compile_thaw(args, scope),
                "trap-error" => return self.compile_trap_error(args, scope),
                "type" => {
                    if args.is_empty() {
                        return Err("type: needs at least 1 arg".into());
                    }
                    return self.compile_expr(&args[0], scope);
                }
                "quote" => {
                    if args.len() != 1 {
                        return Err("quote: needs 1 arg".into());
                    }
                    return self.compile_quote(&args[0]);
                }
                _ => {}
            }

            // Locals shadow the function namespace: if `head` is a local
            // var, it's a value (closure) and we apply it.
            if scope.has(head) {
                let (binds, names) = self.bind_args(args, scope)?;
                return Ok(format!(
                    "{{ {binds}rt::apply_value(interp, {}.clone(), &[{}])? }}",
                    rust_var(head),
                    names.join(", ")
                ));
            }

            // Inlinable primitive? Emit a direct call to the rt:: helper,
            // bypassing the env-routed apply_named path. release-mode LLVM
            // inlines the helper body so e.g. `(+ X 1)` becomes a direct
            // Value::Int match.
            if let Some((rust_helper, fallible)) = inlinable(head, args.len()) {
                let (binds, names) = self.bind_args(args, scope)?;
                let refs: Vec<String> = names.iter().map(|n| format!("&{n}")).collect();
                let q = if fallible { "?" } else { "" };
                return Ok(format!(
                    "{{ {binds}rt::{rust_helper}({}){q} }}",
                    refs.join(", ")
                ));
            }

            // Plain named call. Uses the direct table (populated for every
            // AOT function at install time) for the fast path when the
            // callee is an AOT-compiled kernel function.
            let (binds, names) = self.bind_args(args, scope)?;
            return Ok(format!(
                "{{ {binds}rt::apply_direct(interp, {:?}, &[{}])? }}",
                head,
                names.join(", ")
            ));
        }

        // Head is itself an expression — must evaluate to a closure.
        let head_src = self.compile_expr(&items[0], scope)?;
        let head_tv = self.fresh();
        let (binds, names) = self.bind_args(args, scope)?;
        Ok(format!(
            "{{ let {head_tv} = {head_src}; {binds}rt::apply_value(interp, {head_tv}, &[{}])? }}",
            names.join(", ")
        ))
    }

    /// Compile each argument and bind it to a fresh local so nested calls
    /// don't overlap mutable borrows of `interp`. Returns the binding
    /// statements and the list of bound names to use as call args.
    fn bind_args(
        &mut self,
        args: &[KlExpr],
        scope: &mut Scope,
    ) -> Result<(String, Vec<String>), String> {
        let mut binds = String::new();
        let mut names = Vec::with_capacity(args.len());
        for a in args {
            let src = self.compile_expr(a, scope)?;
            let tv = self.fresh();
            binds.push_str(&format!("let {tv} = {src}; "));
            names.push(tv);
        }
        Ok((binds, names))
    }

    fn compile_if(&mut self, args: &[KlExpr], scope: &mut Scope) -> Result<String, String> {
        if args.len() != 3 {
            return Err("if: expected 3 args".into());
        }
        let c = self.compile_expr(&args[0], scope)?;
        let t = self.compile_expr(&args[1], scope)?;
        let e = self.compile_expr(&args[2], scope)?;
        let tv = self.fresh();
        Ok(format!(
            "{{ let {tv} = {c}; if rt::is_truthy(interp, &{tv})? {{ {t} }} else {{ {e} }} }}"
        ))
    }

    fn compile_let(&mut self, args: &[KlExpr], scope: &mut Scope) -> Result<String, String> {
        if args.len() != 3 {
            return Err("let: expected 3 args".into());
        }
        let var = match &args[0] {
            KlExpr::Sym(s) => self.interner.resolve(*s).to_string(),
            _ => return Err("let: var must be symbol".into()),
        };
        let value_src = self.compile_expr(&args[1], scope)?;
        scope.bind(&var);
        let body_src = self.compile_expr(&args[2], scope)?;
        scope.unbind(&var);
        Ok(format!(
            "{{ let {} = {value_src}; {body_src} }}",
            rust_var(&var)
        ))
    }

    fn compile_cond(&mut self, args: &[KlExpr], scope: &mut Scope) -> Result<String, String> {
        // Compile (cond (T1 V1) (T2 V2) ... (true VN)) as nested if/else.
        // We don't have a "no-clause-matched" trap in this fast path —
        // the kernel always ends with a `(true ...)` clause.
        let mut acc = String::from("return Err(ShenError::new(\"cond: no clause matched\"))");
        for clause in args.iter().rev() {
            let items = match clause {
                KlExpr::App(items) if items.len() == 2 => items,
                _ => return Err("cond: clause must be (test action)".into()),
            };
            let t = self.compile_expr(&items[0], scope)?;
            let v = self.compile_expr(&items[1], scope)?;
            let tv = self.fresh();
            acc = format!(
                "{{ let {tv} = {t}; if rt::is_truthy(interp, &{tv})? {{ {v} }} else {{ {acc} }} }}"
            );
        }
        Ok(acc)
    }

    fn compile_do(&mut self, args: &[KlExpr], scope: &mut Scope) -> Result<String, String> {
        if args.is_empty() {
            return Ok("Value::nil()".into());
        }
        let mut block = String::from("{ ");
        for (i, a) in args.iter().enumerate() {
            let src = self.compile_expr(a, scope)?;
            if i + 1 == args.len() {
                block.push_str(&src);
            } else {
                block.push_str(&format!("let _ = {src}; "));
            }
        }
        block.push_str(" }");
        Ok(block)
    }

    fn compile_and(&mut self, args: &[KlExpr], scope: &mut Scope) -> Result<String, String> {
        if args.len() != 2 {
            return Err("and: expected 2 args".into());
        }
        let a = self.compile_expr(&args[0], scope)?;
        let b = self.compile_expr(&args[1], scope)?;
        let av = self.fresh();
        let bv = self.fresh();
        Ok(format!(
            "{{ let {av} = {a}; if !rt::is_truthy(interp, &{av})? {{ Value::bool(false) }} else {{ let {bv} = {b}; Value::bool(rt::is_truthy(interp, &{bv})?) }} }}"
        ))
    }

    fn compile_or(&mut self, args: &[KlExpr], scope: &mut Scope) -> Result<String, String> {
        if args.len() != 2 {
            return Err("or: expected 2 args".into());
        }
        let a = self.compile_expr(&args[0], scope)?;
        let b = self.compile_expr(&args[1], scope)?;
        let av = self.fresh();
        let bv = self.fresh();
        Ok(format!(
            "{{ let {av} = {a}; if rt::is_truthy(interp, &{av})? {{ Value::bool(true) }} else {{ let {bv} = {b}; Value::bool(rt::is_truthy(interp, &{bv})?) }} }}"
        ))
    }

    fn compile_lambda(&mut self, args: &[KlExpr], scope: &mut Scope) -> Result<String, String> {
        if args.len() != 2 {
            return Err("lambda: expected (lambda VAR BODY)".into());
        }
        let var = match &args[0] {
            KlExpr::Sym(s) => self.interner.resolve(*s).to_string(),
            _ => return Err("lambda: param not a symbol".into()),
        };
        // Capture only outer vars the body actually references. Sort for
        // stable codegen output across runs.
        let mut captures = self.captures_used(&args[1], scope);
        captures.sort();

        let mut inner_scope = Scope::new();
        for c in &captures {
            inner_scope.bind(c);
        }
        inner_scope.bind(&var);

        let body_src = self.compile_expr(&args[1], &mut inner_scope)?;

        // Emit a Rust closure. `move` captures all clones by value. Alongside,
        // emit a traceable shadow capture vec of the same handles so the GC can
        // reach `Value`s sealed inside the opaque `dyn Fn` (GC Step 3 §5). Since
        // `Value` is `Copy`, the move-closure and the vec hold copies of the
        // same tagged words.
        let mut clone_list = String::new();
        for c in &captures {
            clone_list.push_str(&format!("let {0} = {0}.clone(); ", rust_var(c)));
        }
        let capture_vec = capture_vec_src(&captures);
        Ok(format!(
            "{{ {clone_list}rt::make_aot_closure(\"<lambda>\", 1, move |interp, args| {{ let {} = args[0].clone(); Ok({body_src}) }}, {capture_vec}, interp) }}",
            rust_var(&var)
        ))
    }

    fn compile_freeze(&mut self, args: &[KlExpr], scope: &mut Scope) -> Result<String, String> {
        if args.len() != 1 {
            return Err("freeze: expected 1 arg".into());
        }
        // 0-arity closure. Capture only outer vars the body references.
        let mut captures = self.captures_used(&args[0], scope);
        captures.sort();
        let mut inner_scope = Scope::new();
        for c in &captures {
            inner_scope.bind(c);
        }
        let body_src = self.compile_expr(&args[0], &mut inner_scope)?;
        let mut clone_list = String::new();
        for c in &captures {
            clone_list.push_str(&format!("let {0} = {0}.clone(); ", rust_var(c)));
        }
        let capture_vec = capture_vec_src(&captures);
        Ok(format!(
            "{{ {clone_list}rt::make_aot_closure(\"<freeze>\", 0, move |interp, _args| Ok({body_src}), {capture_vec}, interp) }}"
        ))
    }

    /// Pick out the subset of `scope.in_scope` that the closure body
    /// might actually look up. Walks the body collecting every
    /// `KlExpr::Sym(s)` reference and keeps only outer-scope names that
    /// appear in that set. Conservative: ignores inner `let`/`lambda`
    /// shadowing, since shadowing is resolved at runtime by
    /// innermost-wins lookup — over-capturing a slot is never wrong,
    /// just slightly larger. The big win is shrinking type-checker
    /// freezes whose body uses 3–5 vars out of a 20–50 entry scope.
    ///
    /// NOTE: iterates a `HashSet` — the returned order is
    /// NONDETERMINISTIC. Callers must sort before emitting (see the
    /// byte-stability invariants in the module docs).
    fn captures_used(&self, body: &KlExpr, scope: &Scope) -> Vec<String> {
        if scope.in_scope.is_empty() {
            return Vec::new();
        }
        let mut used: HashSet<String> = HashSet::new();
        self.collect_used_names(body, &mut used);
        if used.is_empty() {
            return Vec::new();
        }
        scope
            .in_scope
            .iter()
            .filter(|name| used.contains(name.as_str()))
            .cloned()
            .collect()
    }

    fn collect_used_names(&self, expr: &KlExpr, out: &mut HashSet<String>) {
        match expr {
            KlExpr::Sym(s) => {
                let name = self.interner.resolve(*s);
                if !out.contains(name) {
                    out.insert(name.to_string());
                }
            }
            KlExpr::App(items) => {
                for child in items.iter() {
                    self.collect_used_names(child, out);
                }
            }
            _ => {}
        }
    }

    fn compile_thaw(&mut self, args: &[KlExpr], scope: &mut Scope) -> Result<String, String> {
        if args.len() != 1 {
            return Err("thaw: expected 1 arg".into());
        }
        let f = self.compile_expr(&args[0], scope)?;
        Ok(format!("rt::apply_value(interp, {f}, &[])?"))
    }

    fn compile_trap_error(&mut self, args: &[KlExpr], scope: &mut Scope) -> Result<String, String> {
        if args.len() != 2 {
            return Err("trap-error: expected 2 args".into());
        }
        let body = self.compile_expr(&args[0], scope)?;
        let handler = self.compile_expr(&args[1], scope)?;
        // Wrap the body in an IIFE so `?` inside it propagates to the
        // closure, not the enclosing AOT function. Catch the error here
        // and dispatch to the handler.
        Ok(format!(
            "match (|| -> ShenResult<Value> {{ Ok({body}) }})() {{ Ok(v) => v, Err(e) => {{ let __h = {handler}; let __err = Value::err(e.message.clone()); rt::apply_value(interp, __h, &[__err])? }} }}"
        ))
    }

    fn compile_quote(&mut self, expr: &KlExpr) -> Result<String, String> {
        // Quote produces a literal Value. No lookups, no calls.
        Ok(match expr {
            KlExpr::Nil => "Value::nil()".into(),
            KlExpr::Bool(b) => format!("Value::bool({b})"),
            KlExpr::Int(n) => format!("Value::int({n}i64)"),
            KlExpr::Float(x) => format!("Value::float({x}f64)"),
            KlExpr::Str(s) => format!("Value::str({:?})", s.as_ref()),
            KlExpr::Sym(id) => format!(
                "Value::sym(interp.intern({:?}))",
                self.interner.resolve(*id)
            ),
            KlExpr::App(items) => {
                let elems: Vec<String> = items
                    .iter()
                    .map(|e| self.compile_quote(e))
                    .collect::<Result<Vec<_>, _>>()?;
                format!("Value::list([{}])", elems.join(", "))
            }
        })
    }
}

/// Scope tracker: which KL variable names are currently bound to a
/// Rust local. We use a flat `HashSet` because shadowing is rare in
/// kernel code and we only need to answer "is this name in scope".
struct Scope {
    in_scope: HashSet<String>,
}

impl Scope {
    fn new() -> Self {
        Self {
            in_scope: HashSet::new(),
        }
    }
    fn bind(&mut self, name: &str) {
        self.in_scope.insert(name.to_string());
    }
    fn unbind(&mut self, name: &str) {
        self.in_scope.remove(name);
    }
    fn has(&self, name: &str) -> bool {
        self.in_scope.contains(name)
    }
}

/// Map a KL identifier to a Rust variable name (`v_<sanitized>`).
fn rust_var(name: &str) -> String {
    format!("v_{}", sanitize_ident(name))
}

/// Source for a closure's traceable shadow capture vec (GC Step 3 §5): a
/// `vec![v_A, v_B, …]` of the same handles the body `move`-captures, so the GC
/// can reach `Value`s sealed inside the opaque `dyn Fn`. Empty captures emit
/// `Vec::new()` (typed from the `make_aot_closure` parameter).
fn capture_vec_src(captures: &[String]) -> String {
    if captures.is_empty() {
        return "Vec::new()".to_string();
    }
    let items: Vec<String> = captures.iter().map(|c| rust_var(c)).collect();
    format!("vec![{}]", items.join(", "))
}

/// Extract the defun name from a top-level form, or None if it isn't a
/// well-formed `(defun NAME ARGS BODY)`.
fn defun_name(form: &KlExpr, interner: &Interner) -> Option<String> {
    let items = match form {
        KlExpr::App(items) => items,
        _ => return None,
    };
    if items.len() != 4 {
        return None;
    }
    let head = match &items[0] {
        KlExpr::Sym(s) => interner.resolve(*s),
        _ => return None,
    };
    if head != "defun" {
        return None;
    }
    match &items[1] {
        KlExpr::Sym(s) => Some(interner.resolve(*s).to_string()),
        _ => None,
    }
}

/// Inlinable primitive table. Returns the `rt::` helper name and whether
/// it returns `ShenResult<Value>` (needs `?`) vs. `Value` (infallible).
/// Arity must match exactly — partial application stays on the
/// apply_named path so its closure semantics are preserved.
fn inlinable(name: &str, arity: usize) -> Option<(&'static str, bool)> {
    let (helper, expected_arity, fallible) = match name {
        "+" => ("add", 2, true),
        "-" => ("sub", 2, true),
        "*" => ("mul", 2, true),
        "/" => ("div", 2, true),
        "<" => ("lt", 2, true),
        ">" => ("gt", 2, true),
        "<=" => ("lte", 2, true),
        ">=" => ("gte", 2, true),
        "=" => ("eq", 2, false),
        "cons" => ("cons", 2, false),
        "hd" => ("hd", 1, true),
        "tl" => ("tl", 1, true),
        "cons?" => ("is_cons", 1, false),
        "number?" => ("is_number", 1, false),
        "string?" => ("is_string", 1, false),
        "symbol?" => ("is_symbol", 1, false),
        "absvector?" => ("is_absvector", 1, false),
        "vector?" => ("is_absvector", 1, false),
        _ => return None,
    };
    if arity == expected_arity {
        Some((helper, fallible))
    } else {
        None
    }
}

/// Map a KL identifier to a Rust-safe form. Non-alphanumeric characters
/// are hex-escaped as `_xHH_` so distinct KL names (`string?`, `string->`,
/// `string->n`) never collapse onto the same Rust ident.
fn sanitize_ident(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 4);
    for c in name.chars() {
        if c.is_ascii_alphanumeric() || c == '_' {
            out.push(c);
        } else {
            out.push_str(&format!("_x{:02x}_", c as u32));
        }
    }
    if out.is_empty() {
        out.push('x');
    }
    if out.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        out.insert(0, '_');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn workspace_root() -> PathBuf {
        let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        p.pop();
        p.pop();
        p
    }

    /// Pin the kernel skip set: with the kernel options, the only
    /// force-skipped defuns across all kernel .kl files are exactly the
    /// legacy four. A budget heuristic must never silently change this —
    /// the kernel AOT bytes are frozen behind Gate 6.
    #[test]
    fn kernel_skip_set_is_exactly_the_legacy_four() {
        let dir = workspace_root().join("kernel").join("klambda");
        let mut slow_skipped = Vec::new();
        for entry in std::fs::read_dir(&dir).expect("kernel/klambda") {
            let path = entry.expect("dir entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("kl") {
                continue;
            }
            let src = std::fs::read_to_string(&path).expect("read .kl");
            let opts = CompileOptions::kernel(path.display().to_string());
            let (_, report) = compile_kl(&src, &opts).expect("compile");
            for s in report.skipped {
                if s.contains("known-slow-to-compile") {
                    slow_skipped.push(s.split(':').next().unwrap().to_string());
                }
            }
        }
        slow_skipped.sort();
        let mut expected: Vec<String> = KERNEL_SLOW_DEFUNS.iter().map(|s| s.to_string()).collect();
        expected.sort();
        assert_eq!(slow_skipped, expected);
    }

    /// Captures are emitted in sorted order (the HashSet iteration is
    /// nondeterministic; the sort is the byte-stability rescue).
    #[test]
    fn lambda_captures_emit_sorted() {
        let src = "(defun f (Z A) (lambda X (cons Z (cons A X))))";
        let opts = CompileOptions::external("test.kl");
        let (out, report) = compile_kl(src, &opts).expect("compile");
        assert_eq!(report.compiled, vec!["f".to_string()]);
        let a = out.find("let v_A = v_A.clone();").expect("capture A");
        let z = out.find("let v_Z = v_Z.clone();").expect("capture Z");
        assert!(a < z, "captures must be sorted: A before Z");
    }

    /// The body-node budget skips oversized defuns; they stay on the
    /// loaded engine (apply_direct falls back per-name).
    #[test]
    fn body_node_budget_skips_oversized_defuns() {
        let src = "(defun big (X) (cons 1 (cons 2 (cons 3 (cons 4 X))))) (defun small (X) X)";
        let mut opts = CompileOptions::external("test.kl");
        opts.skip.max_body_nodes = Some(5);
        let (_, report) = compile_kl(src, &opts).expect("compile");
        assert_eq!(report.compiled, vec!["small".to_string()]);
        assert_eq!(report.skipped.len(), 1);
        assert!(report.skipped[0].starts_with("big: body too large"));
    }

    /// External config emits shen_rust:: imports and a plain `//` header
    /// (no inner attrs — host file owns lints in include!-style use).
    #[test]
    fn external_config_header_shape() {
        let src = "(defun id (X) X)";
        let opts = CompileOptions::external("spec/foo.shen (bootstrapped)");
        let (out, _) = compile_kl(src, &opts).expect("compile");
        assert!(out.starts_with("// Auto-generated by `klcompile` from spec/foo.shen"));
        assert!(out.contains("use shen_rust::aot::runtime as rt;"));
        assert!(!out.contains("#![allow"));
        assert!(!out.contains("use crate::"));
    }
}
