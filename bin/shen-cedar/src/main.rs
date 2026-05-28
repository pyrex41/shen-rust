//! shen-cedar REPL.
//!
//! Boots the ShenOSKernel-41.1 then routes each user line through the
//! kernel's own `eval` (which in turn does `(eval-kl (shen.shen->kl …))`).
//! Input is read one balanced s-expression at a time — partial lines are
//! buffered until the parentheses balance.

use std::io::{self, BufRead, Write};
use std::process::ExitCode;

use shen_cedar::error::ShenResult;
use shen_cedar::interp::boot::boot;
use shen_cedar::interp::eval::Interp;
use shen_cedar::kl::ast::KlExpr;
use shen_cedar::kl::parser::parse_one;
use shen_cedar::symbol::SymId;
use shen_cedar::value::Value;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().skip(1).any(|a| a == "--kernel-tests") {
        // The kernel suite hits deep AOT recursion in places (the reader,
        // YACC, type checker) which we don't trampoline through the Rust
        // stack. Bump the stack to 1 GB so those calls have room.
        let handle = std::thread::Builder::new()
            .name("kernel-tests".to_string())
            .stack_size(1024 * 1024 * 1024)
            .spawn(run_kernel_tests)
            .expect("spawn kernel-tests thread");
        return handle.join().unwrap_or(ExitCode::from(2));
    }
    // Same stack-size workaround for the REPL: `(load "...")`-ing any
    // user code that runs through the type-checker tends to recurse
    // through enough non-self-tail-call frames to blow the default
    // 8 MB stack. 64 MB is enough for most reasonable workloads.
    let handle = std::thread::Builder::new()
        .name("repl".to_string())
        .stack_size(64 * 1024 * 1024)
        .spawn(run_repl)
        .expect("spawn repl thread");
    handle.join().unwrap_or(ExitCode::from(2))
}

fn run_repl() -> ExitCode {
    let mut interp = Interp::new();
    eprint!("shen-cedar booting kernel… ");
    if let Err(e) = boot(&mut interp) {
        eprintln!("FAILED\n  {e}");
        return ExitCode::from(2);
    }
    eprintln!("ready.");
    print_banner(&interp);
    if let Err(e) = repl_loop(&mut interp) {
        eprintln!("repl: {e}");
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}

/// Boot the kernel, override `y-or-n?` to always answer yes, then run
/// `kernel/tests/runme.shen` non-interactively. Returns a non-zero exit
/// code if `(value test-harness.*failed*)` is positive at the end.
fn run_kernel_tests() -> ExitCode {
    let mut interp = Interp::new();
    eprint!("shen-cedar booting kernel… ");
    if let Err(e) = boot(&mut interp) {
        eprintln!("FAILED\n  {e}");
        return ExitCode::from(2);
    }
    eprintln!("ready.");

    // The harness's `failed` definition asks `(y-or-n? "failed; continue?")`
    // and calls `(error "kill")` if the user answers no. Override with a
    // native primitive that unconditionally answers yes, so the suite
    // runs to completion instead of stopping at the first failure.
    interp.register_native("y-or-n?", 1, |_, _args| Ok(Value::Bool(true)));

    // The kernel's `cd` only updates `*home-directory*` — it doesn't
    // chdir the process. `(load "runme.shen")` opens the file with a
    // bare relative path, so we need the process cwd to actually be in
    // kernel/tests/. Do the chdir at the host level.
    if let Err(e) = std::env::set_current_dir("kernel/tests") {
        eprintln!("kernel-tests: chdir kernel/tests: {e}");
        return ExitCode::from(1);
    }

    // Load harness.shen first so `reset`/`passed`/`failed` are defined.
    // `kerneltests.shen` ends with a `(reset)` call that zeroes the
    // counters before we can read them, so override `reset` to a no-op
    // between the two loads.
    let steps: &[&str] = &[
        "(cd \"\")",
        "(load \"harness.shen\")",
        // Defined by the harness inside the test-harness package.
        // After our override, the harness's terminal `(reset)` becomes
        // a no-op so the `*passed*`/`*failed*` counters survive.
        "",
        "(load \"kerneltests.shen\")",
    ];
    for src in steps {
        if src.is_empty() {
            interp.register_native("reset", 0, |_, _args| Ok(Value::Nil));
            continue;
        }
        match parse_one(src, &mut interp.symbols) {
            Ok(expr) => {
                if let Err(e) = dispatch_through_kernel_eval(&mut interp, &expr) {
                    eprintln!("kernel-tests: {src} failed: {e}");
                    return ExitCode::from(1);
                }
            }
            Err(e) => {
                eprintln!("kernel-tests: parse {src}: {e}");
                return ExitCode::from(1);
            }
        }
    }

    // The harness lives in the `test-harness` package and exports
    // `passed`/`failed`/`reset`, but `*passed*`/`*failed*` are not in
    // the export list, so Shen's package macro prefixes them.
    //
    // An unbound global means the corresponding counter was never
    // incremented (e.g. zero failures → `*failed*` never `set`'d). Treat
    // unbound as 0 rather than an error.
    let read_int = |interp: &mut Interp, name: &str| -> i64 {
        let sym = interp.intern(name);
        match interp.env.get_global(sym) {
            Some(Value::Int(n)) => *n,
            _ => 0,
        }
    };
    let passed = read_int(&mut interp, "test-harness.*passed*");
    let failed = read_int(&mut interp, "test-harness.*failed*");

    println!("kernel-tests: passed: {passed}, failed: {failed}");
    if failed > 0 {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

fn print_banner(interp: &Interp) {
    let get = |name: &str| -> String {
        let mut symbols = interp.symbols.clone_for_lookup();
        let sym = symbols.intern(name);
        match interp.env.get_global(sym) {
            Some(Value::Str(s)) => s.to_string(),
            _ => String::from("?"),
        }
    };
    let version = get("*version*");
    let port = get("*port*");
    let lang = get("*language*");
    eprintln!("\nShen {version}, ©2021–2026 Mark Tarver  (shen-cedar {port}, {lang})\n");
}

fn repl_loop(interp: &mut Interp) -> ShenResult<()> {
    let stdin = io::stdin();
    let mut handle = stdin.lock();
    let mut stdout = io::stdout();
    let mut buf = String::new();
    loop {
        if buf.is_empty() {
            print!("(0-) ");
        } else {
            print!("...  ");
        }
        stdout.flush().ok();
        let mut line = String::new();
        match handle.read_line(&mut line) {
            Ok(0) => {
                println!();
                return Ok(());
            }
            Ok(_) => {}
            Err(e) => {
                eprintln!("read: {e}");
                return Ok(());
            }
        }
        buf.push_str(&line);
        if !parens_balanced(&buf) {
            continue;
        }
        let trimmed = buf.trim();
        if trimmed.is_empty() {
            buf.clear();
            continue;
        }
        match parse_one(trimmed, &mut interp.symbols) {
            Ok(expr) => match dispatch_through_kernel_eval(interp, &expr) {
                Ok(v) => println!("{}", render(interp, &v)),
                Err(e) => println!("error: {e}"),
            },
            Err(e) => println!("parse error: {e}"),
        }
        buf.clear();
    }
}

/// Wrap the parsed expression in `(eval EXPR)` so the kernel's reader
/// pipeline (macro expansion + process-applications) runs. Fall back to
/// raw `eval` if the kernel's `eval` isn't bound yet.
fn dispatch_through_kernel_eval(interp: &mut Interp, expr: &KlExpr) -> ShenResult<Value> {
    let eval_sym = interp.intern("eval");
    if interp.env.get_fn(eval_sym).is_some() {
        let quoted = klexpr_to_value(expr);
        let f = interp.env.get_fn(eval_sym).cloned().unwrap();
        interp.apply(f, vec![quoted])
    } else {
        interp.eval(expr)
    }
}

fn klexpr_to_value(e: &KlExpr) -> Value {
    use shen_cedar::value::Value as V;
    match e {
        KlExpr::Nil => V::Nil,
        KlExpr::Bool(b) => V::Bool(*b),
        KlExpr::Int(n) => V::Int(*n),
        KlExpr::Float(x) => V::Float(*x),
        KlExpr::Str(s) => V::Str(s.clone()),
        KlExpr::Sym(s) => V::Sym(*s),
        KlExpr::App(items) => V::list(items.iter().map(klexpr_to_value)),
    }
}

fn parens_balanced(src: &str) -> bool {
    let mut depth: i32 = 0;
    let mut in_string = false;
    let mut in_block_comment = false;
    let bytes = src.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if in_block_comment {
            if c == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'\\' {
                in_block_comment = false;
                i += 2;
                continue;
            }
            i += 1;
            continue;
        }
        if in_string {
            if c == b'"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        if c == b'"' {
            in_string = true;
            i += 1;
            continue;
        }
        if c == b'\\' && i + 1 < bytes.len() {
            match bytes[i + 1] {
                b'*' => {
                    in_block_comment = true;
                    i += 2;
                    continue;
                }
                b'\\' => {
                    while i < bytes.len() && bytes[i] != b'\n' {
                        i += 1;
                    }
                    continue;
                }
                _ => {}
            }
        }
        if c == b'(' {
            depth += 1;
        } else if c == b')' {
            depth -= 1;
            if depth < 0 {
                return true; // let parser report the error
            }
        }
        i += 1;
    }
    depth == 0
}

fn render(interp: &Interp, v: &Value) -> String {
    match v {
        Value::Nil => "()".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Int(n) => n.to_string(),
        Value::Float(x) => format_float(*x),
        Value::Str(s) => s.to_string(),
        Value::Sym(s) => interp.resolve(*s).to_string(),
        Value::Cons(_) => render_list(interp, v),
        Value::Vec(cells) => render_vec(interp, cells),
        Value::Closure(_) => "<closure>".to_string(),
        Value::Stream(_) => "<stream>".to_string(),
        Value::Error(s) => format!("<error: {s}>"),
        Value::Foreign(_) => "<foreign>".to_string(),
    }
}

/// Print whole-number floats with `.0` so `4000.0` doesn't display as
/// `4000` (which is what Rust's default `{x}` format produces). The
/// kernel test suite compares results with `=`, and several tests
/// expect float results like `4000.0` for `(* 5000 .8)`.
fn format_float(x: f64) -> String {
    if x.is_finite() && x == x.trunc() && x.abs() < 1e16 {
        format!("{x:.1}")
    } else {
        format!("{x}")
    }
}

/// Shen "print vectors" are absvectors whose slot 0 holds a printer
/// symbol (e.g. `shen.printF`) and slot 1 holds the string to display.
/// They're the standard representation for `(fn NAME)` results from
/// `defun` and similar.
fn render_vec(interp: &Interp, cells: &shen_cedar::value::AbsVec) -> String {
    let borrow = cells.borrow();
    if borrow.len() == 2 {
        if let (Value::Sym(_), Value::Str(s)) = (&borrow[0], &borrow[1]) {
            return s.to_string();
        }
    }
    let inner: Vec<String> = borrow.iter().map(|c| render(interp, c)).collect();
    format!("<vector {}>", inner.join(" "))
}

fn render_list(interp: &Interp, v: &Value) -> String {
    let mut out = String::from("(");
    let mut cur = v.clone();
    let mut first = true;
    loop {
        match cur {
            Value::Nil => break,
            Value::Cons(p) => {
                if !first {
                    out.push(' ');
                }
                out.push_str(&render(interp, &p.0));
                first = false;
                cur = p.1.clone();
            }
            _ => {
                out.push_str(" . ");
                out.push_str(&render(interp, &cur));
                break;
            }
        }
    }
    out.push(')');
    out
}

// We don't actually expose a clone on Interner; cheat by interning into a
// fresh helper for the banner read-only path.
trait InternerHelper {
    fn clone_for_lookup(&self) -> SymIdLookup;
}
struct SymIdLookup {
    map: std::collections::HashMap<String, SymId>,
}
impl SymIdLookup {
    fn intern(&mut self, name: &str) -> SymId {
        *self.map.get(name).unwrap_or(&SymId(u32::MAX))
    }
}
impl InternerHelper for shen_cedar::symbol::Interner {
    fn clone_for_lookup(&self) -> SymIdLookup {
        // Re-build a name->id map by iterating self.len(). Since
        // `Interner` doesn't expose iteration, fall back to the slow
        // path: not used in hot code (only for banner).
        SymIdLookup {
            map: (0..self.len())
                .map(|i| (self.resolve(SymId(i as u32)).to_string(), SymId(i as u32)))
                .collect(),
        }
    }
}
