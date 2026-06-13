//! Port-authored I/O coverage suite (NOT the canonical kernel suite).
//!
//! Mirror of shen-go's `kl/io_coverage_test.go`: open(in/out) / close,
//! write-byte → close → read-byte round trip, read-byte EOF returns -1,
//! read-file, load-file side effects, and get-time. Driven through the
//! native primitives `Interp::new()` registers (no kernel boot needed),
//! the shen-rust analogue of shen-go's bare `evalString`.

use std::io::Write;

use shen_rust::error::ShenResult;
use shen_rust::interp::eval::Interp;
use shen_rust::kl::parser::parse_one;
use shen_rust::value::Value;

fn run(interp: &mut Interp, src: &str) -> ShenResult<Value> {
    let e = parse_one(src, &mut interp.symbols).unwrap_or_else(|e| panic!("parse {src:?}: {e}"));
    interp.eval(&e)
}

fn ok(interp: &mut Interp, src: &str) -> Value {
    run(interp, src).unwrap_or_else(|e| panic!("eval {src:?}: {e}"))
}

/// `read-file-as-string` / `read-file-as-bytelist` are registered as native
/// FAST PATHS via `register_hot_overrides` (normally installed during kernel
/// boot), not by the base `register_all` that `Interp::new()` runs. Install
/// them explicitly so the file-read tests can exercise them without a full
/// kernel boot.
fn interp_with_hot() -> Interp {
    let mut i = Interp::new();
    shen_rust::primitives::register_hot_overrides(&mut i);
    i
}

/// A unique temp path under the OS temp dir. We avoid pulling in a tempdir
/// crate (no new deps); the pid + a counter keeps parallel test threads from
/// colliding.
fn temp_path(tag: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    std::env::temp_dir().join(format!("shen-rust-io-{tag}-{pid}-{n}"))
}

// ---------------------------------------------------------------------------
// write-byte → close → read-byte round trip, then EOF == -1.
// Mirror of shen-go's TestStreamRoundTrip.
// ---------------------------------------------------------------------------

#[test]
fn stream_write_read_round_trip_and_eof() {
    let path = temp_path("rt");
    let path_str = path.to_str().unwrap().replace('\\', "\\\\");
    let mut i = Interp::new();

    // Open for output, write "Hi" (72, 105), close.
    ok(&mut i, &format!(r#"(set out (open "{path_str}" out))"#));
    ok(&mut i, "(write-byte 72 (value out))");
    ok(&mut i, "(write-byte 105 (value out))");
    ok(&mut i, "(close (value out))");

    // Re-open for input; read back 72, 105, then -1 (EOF).
    ok(&mut i, &format!(r#"(set in (open "{path_str}" in))"#));
    assert_eq!(ok(&mut i, "(read-byte (value in))").as_int(), Some(72));
    assert_eq!(ok(&mut i, "(read-byte (value in))").as_int(), Some(105));
    // EOF returns -1 (mirror of shen-go's read-byte EOF contract).
    assert_eq!(ok(&mut i, "(read-byte (value in))").as_int(), Some(-1));
    // EOF is sticky.
    assert_eq!(ok(&mut i, "(read-byte (value in))").as_int(), Some(-1));
    ok(&mut i, "(close (value in))");

    std::fs::remove_file(&path).ok();
}

#[test]
fn write_to_input_stream_errors() {
    let path = temp_path("wbad");
    std::fs::write(&path, b"x").unwrap();
    let path_str = path.to_str().unwrap().replace('\\', "\\\\");
    let mut i = Interp::new();
    ok(&mut i, &format!(r#"(set in (open "{path_str}" in))"#));
    // write-byte to an input stream is a catchable error, not a panic.
    assert!(run(&mut i, "(write-byte 65 (value in))").is_err());
    // read-byte from an output stream likewise errors.
    let path2 = temp_path("rbad");
    let path2_str = path2.to_str().unwrap().replace('\\', "\\\\");
    ok(&mut i, &format!(r#"(set o (open "{path2_str}" out))"#));
    assert!(run(&mut i, "(read-byte (value o))").is_err());
    ok(&mut i, "(close (value in))");
    ok(&mut i, "(close (value o))");
    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&path2).ok();
}

#[test]
fn open_bad_direction_errors() {
    let path = temp_path("dir");
    let path_str = path.to_str().unwrap().replace('\\', "\\\\");
    let mut i = Interp::new();
    assert!(run(&mut i, &format!(r#"(open "{path_str}" sideways)"#)).is_err());
}

// ---------------------------------------------------------------------------
// read-file: shen-rust exposes read-file-as-string / read-file-as-bytelist
// (the hot-path primitives). Mirror of shen-go's TestFileReadPrimitives.
// ---------------------------------------------------------------------------

#[test]
fn read_file_as_string_and_bytelist() {
    let path = temp_path("rf");
    {
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(b"AB").unwrap();
    }
    let path_str = path.to_str().unwrap().replace('\\', "\\\\");
    let mut i = interp_with_hot();

    let s = ok(&mut i, &format!(r#"(read-file-as-string "{path_str}")"#));
    assert_eq!(s.as_str(), Some("AB"));

    // 'A'=65, 'B'=66 → byte list (65 66).
    let bl = ok(&mut i, &format!(r#"(read-file-as-bytelist "{path_str}")"#));
    assert_eq!(bl.head().and_then(|h| h.as_int()), Some(65));
    assert_eq!(
        bl.tail().and_then(|t| t.head()).and_then(|h| h.as_int()),
        Some(66)
    );

    std::fs::remove_file(&path).ok();
}

#[test]
fn read_file_missing_errors() {
    let path = temp_path("missing");
    let path_str = path.to_str().unwrap().replace('\\', "\\\\");
    let mut i = interp_with_hot();
    // A nonexistent path must surface a catchable error, not a panic.
    assert!(run(&mut i, &format!(r#"(read-file-as-string "{path_str}")"#)).is_err());
}

// ---------------------------------------------------------------------------
// get-time: both unix and run arms yield numbers
// (mirror of shen-go's TestGetTime, kept here too since it is I/O-adjacent).
// ---------------------------------------------------------------------------

#[test]
fn get_time_both_arms() {
    let mut i = Interp::new();
    assert!(ok(&mut i, "(get-time unix)").is_number());
    assert!(ok(&mut i, "(get-time run)").is_number());
}
