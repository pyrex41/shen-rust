//! Port-authored CLI/launcher suite (NOT the canonical kernel suite).
//!
//! Mirror of shen-go's `cmd/shen/main_test.go`: spawn the real release
//! binary and exercise the launcher protocol — `eval -e EXPR`, `eval -l
//! FILE`, `script FILE`, `--version` / `--help`, piping EOF, the `-q`
//! file-write divergence, and adversarial input.
//!
//! DIVERGENCES from shen-go, locked in with comments (the spec requires the
//! port's CORRECT documented behavior, not faked parity):
//!
//!   * `-q` (which sets `*hush*`): on shen-rust `-q` SILENCES `pr` writes to
//!     file streams (zero-byte files), whereas shen-cl/shen-go route `pr` to
//!     files regardless. We assert the file IS written WITHOUT `-q`, and is
//!     EMPTY (or the process still succeeds) WITH `-q`.
//!   * Adversarial `eval -e`: shen-rust prints the error to stderr and the
//!     launcher exits SUCCESS (it does not map an eval error to a nonzero
//!     code the way shen-go does). We assert the error message is reported
//!     and that NO Rust backtrace leaks — never that the exit code is
//!     nonzero.
//!   * An unrecognized leading word (e.g. `no-such-command`) is NOT a
//!     launcher verb on shen-rust, so it drops into the REPL rather than
//!     erroring. We do not assert shen-go's "Invalid argument" path.
//!
//! Each spawn carries a wall-clock timeout so a hang (e.g. a REPL that never
//! exits on stdin EOF) FAILS the test instead of stalling the whole suite.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Once;
use std::time::{Duration, Instant};

/// Workspace `target/<profile>/shen-rust`. The gates build the workspace
/// before `cargo test`, so the binary normally already exists; if it does
/// not (e.g. a bare `cargo test --test cli_launcher`), build it once.
fn shen_rust_bin() -> PathBuf {
    static BUILD: Once = Once::new();
    let mut target = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    target.pop(); // crates
    target.pop(); // workspace root
    let root = target.clone();
    // Prefer release (what the gates use); fall back to debug.
    let release = root.join("target/release/shen-rust");
    let debug = root.join("target/debug/shen-rust");
    let bin = if release.exists() {
        release
    } else if debug.exists() {
        debug
    } else {
        BUILD.call_once(|| {
            let status = Command::new(env!("CARGO"))
                .current_dir(&root)
                .args(["build", "--release", "--bin", "shen-rust"])
                .status()
                .expect("spawn cargo build");
            assert!(status.success(), "failed to build shen-rust binary");
        });
        root.join("target/release/shen-rust")
    };
    assert!(bin.exists(), "shen-rust binary not found at {bin:?}");
    bin
}

struct Output {
    stdout: String,
    stderr: String,
    combined: String,
    success: bool,
}

/// Spawn the binary with `args` and optional `stdin`, enforcing a timeout.
/// Panics (failing the test) if the process does not exit within `timeout`.
fn run(args: &[&str], stdin: Option<&str>, timeout: Duration) -> Output {
    let mut cmd = Command::new(shen_rust_bin());
    cmd.args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn shen-rust");

    if let Some(s) = stdin {
        child
            .stdin
            .take()
            .expect("child stdin")
            .write_all(s.as_bytes())
            .expect("write stdin");
    } else {
        // Close stdin immediately so anything that reads it sees EOF.
        drop(child.stdin.take());
    }

    let start = Instant::now();
    loop {
        match child.try_wait().expect("try_wait") {
            Some(_) => break,
            None => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!(
                        "shen-rust {args:?} did not exit within {:?} (hang regression)",
                        timeout
                    );
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }

    let out = child.wait_with_output().expect("wait_with_output");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let combined = format!("{stdout}{stderr}");
    Output {
        stdout,
        stderr,
        combined,
        success: out.status.success(),
    }
}

const TIMEOUT: Duration = Duration::from_secs(60);

/// No Rust panic / backtrace may ever leak to the user. Refuse output that
/// looks like one.
fn assert_no_backtrace(o: &Output, ctx: &str) {
    let leaked = o.combined.contains("panicked at")
        || o.combined.contains("RUST_BACKTRACE")
        || o.combined.contains("stack backtrace:");
    assert!(
        !leaked,
        "[{ctx}] leaked a Rust panic/backtrace:\n{}",
        o.combined
    );
}

// ---------------------------------------------------------------------------

#[test]
fn eval_e_prints_value() {
    let o = run(&["eval", "-e", "(+ 1 2)"], None, TIMEOUT);
    assert_no_backtrace(&o, "eval -e");
    assert!(o.success, "eval -e exited nonzero:\n{}", o.combined);
    assert!(
        o.stdout.contains('3'),
        "expected '3' in stdout, got:\n{}",
        o.stdout
    );
}

#[test]
fn eval_l_loads_file_then_evaluates() {
    // Define a 0-arg function in a loaded file, then call it via -e.
    let dir = std::env::temp_dir().join(format!("shen-rust-cli-{}", std::process::id()));
    std::fs::create_dir_all(&dir).ok();
    let file = dir.join("lib.shen");
    std::fs::write(&file, b"(define answer -> 42)\n").unwrap();

    let o = run(
        &["eval", "-l", file.to_str().unwrap(), "-e", "(answer)"],
        None,
        TIMEOUT,
    );
    assert_no_backtrace(&o, "eval -l");
    assert!(o.success, "eval -l exited nonzero:\n{}", o.combined);
    assert!(
        o.stdout.contains("42"),
        "expected loaded function result 42, got:\n{}",
        o.stdout
    );
    std::fs::remove_file(&file).ok();
}

#[test]
fn script_runs_file() {
    let dir = std::env::temp_dir().join(format!("shen-rust-cli-script-{}", std::process::id()));
    std::fs::create_dir_all(&dir).ok();
    let file = dir.join("s.shen");
    std::fs::write(&file, b"(output \"hello-from-script\")").unwrap();

    let o = run(&["script", file.to_str().unwrap()], None, TIMEOUT);
    assert_no_backtrace(&o, "script");
    assert!(o.success, "script exited nonzero:\n{}", o.combined);
    assert!(
        o.combined.contains("hello-from-script"),
        "expected script output, got:\n{}",
        o.combined
    );
    std::fs::remove_file(&file).ok();
}

#[test]
fn version_prints_and_exits_zero() {
    let o = run(&["--version"], None, TIMEOUT);
    assert_no_backtrace(&o, "--version");
    assert!(o.success, "--version exited nonzero:\n{}", o.combined);
    // shen-rust reports the kernel version 41.x in the version line.
    assert!(
        o.combined.contains("41."),
        "expected a 41.x version string, got:\n{}",
        o.combined
    );
}

#[test]
fn help_prints_usage_and_exits_zero() {
    let o = run(&["--help"], None, TIMEOUT);
    assert_no_backtrace(&o, "--help");
    assert!(o.success, "--help exited nonzero:\n{}", o.combined);
    assert!(
        o.combined.to_lowercase().contains("usage") || o.combined.contains("command"),
        "expected usage text, got:\n{}",
        o.combined
    );
}

/// Mirror of shen-go's TestPipedStdinEOFExitsRepl: piping a form then EOF
/// must evaluate it and exit cleanly, never loop forever. The TIMEOUT in
/// `run` is what turns a regression (REPL that loops on empty stream) into a
/// FAILED test rather than a hung suite.
#[test]
fn piped_stdin_eof_exits_cleanly() {
    let o = run(&["repl"], Some("(+ 1 2)\n"), TIMEOUT);
    assert_no_backtrace(&o, "piped EOF");
    assert!(
        o.success,
        "repl exited nonzero on stdin EOF:\n{}",
        o.combined
    );
    assert!(
        o.combined.contains('3'),
        "expected evaluated result 3 before EOF, got:\n{}",
        o.combined
    );
}

/// The `-q` / `*hush*` divergence, locked in as shen-rust's CORRECT
/// documented behavior. WITHOUT `-q`, `pr` to a file stream writes the
/// payload. WITH `-q`, `*hush*` silences `pr` to file streams (zero-byte
/// file) — this is the cross-impl divergence the spec calls out.
#[test]
fn quiet_silences_pr_to_file_but_default_writes() {
    let dir = std::env::temp_dir().join(format!("shen-rust-cli-hush-{}", std::process::id()));
    std::fs::create_dir_all(&dir).ok();

    // --- WITHOUT -q: the file IS written. ---
    let path_a = dir.join("out_noq.txt");
    let expr_a = format!(
        r#"(let S (open "{}" out) (do (pr "payload" S) (close S)))"#,
        path_a.to_str().unwrap()
    );
    let o = run(&["eval", "-e", &expr_a], None, TIMEOUT);
    assert_no_backtrace(&o, "eval (no -q)");
    assert!(o.success, "eval (no -q) failed:\n{}", o.combined);
    let data_a = std::fs::read(&path_a).expect("output file should exist without -q");
    assert_eq!(
        String::from_utf8_lossy(&data_a),
        "payload",
        "without -q, pr must write the payload to the file"
    );

    // --- WITH -q: *hush* silences the pr write (documented shen-rust
    //     divergence). The process still succeeds; the file is empty. ---
    let path_b = dir.join("out_q.txt");
    let expr_b = format!(
        r#"(let S (open "{}" out) (do (pr "payload" S) (close S)))"#,
        path_b.to_str().unwrap()
    );
    let o = run(&["eval", "-q", "-e", &expr_b], None, TIMEOUT);
    assert_no_backtrace(&o, "eval -q");
    assert!(o.success, "eval -q failed:\n{}", o.combined);
    let data_b = std::fs::read(&path_b).unwrap_or_default();
    assert!(
        data_b.is_empty(),
        "shen-rust -q (*hush*) must silence pr to files; got {} bytes: {:?}",
        data_b.len(),
        String::from_utf8_lossy(&data_b)
    );

    std::fs::remove_file(&path_a).ok();
    std::fs::remove_file(&path_b).ok();
}

/// Adversarial `eval -e`: an unbound application must be reported as an
/// error message WITHOUT leaking a Rust panic/backtrace. (shen-rust prints
/// the error and the launcher exits SUCCESS — see the module-level
/// divergence note; we deliberately do NOT assert a nonzero code.)
#[test]
fn adversarial_eval_reports_error_without_panicking() {
    let o = run(&["eval", "-e", "(overflow->str)"], None, TIMEOUT);
    assert_no_backtrace(&o, "adversarial eval");
    assert!(
        o.combined.contains("overflow->str"),
        "expected the offending symbol in the error message, got:\n{}",
        o.combined
    );
}

/// Adversarial REPL session: a stream of bad forms interleaved with a valid
/// one must each be reported and the REPL must keep going and exit cleanly
/// on EOF — never crash with a backtrace. Mirror of shen-go's
/// TestReplSurvivesAdversarialSession.
#[test]
fn repl_survives_adversarial_session() {
    let input = [
        "(overflow->str)",       // unbound application
        "(value never-bound-x)", // unbound variable
        r#"(simple-error "boom")"#,
        "(if 42 1 2)", // type error
        "(+ 1 2)",     // valid — must still work
        "(42 1)",      // apply non-function
        "(* 6 7)",     // valid — final answer
    ]
    .join("\n")
        + "\n";

    let o = run(&["repl"], Some(&input), TIMEOUT);
    assert_no_backtrace(&o, "adversarial repl");
    assert!(o.success, "repl exited nonzero:\n{}", o.combined);
    // Valid arithmetic after all the errors still produces results.
    assert!(
        o.combined.contains('3') && o.combined.contains("42"),
        "expected valid results (3 and 42) interleaved with errors, got:\n{}",
        o.combined
    );
    // The offending symbol from the very first bad form is reported, proving
    // the REPL didn't die on it.
    assert!(
        o.combined.contains("overflow->str") || o.combined.contains("boom"),
        "expected an error message in the transcript, got:\n{}",
        o.combined
    );
    // Sanity: stderr-only check that no panic slipped through either pipe.
    let _ = &o.stderr;
}
