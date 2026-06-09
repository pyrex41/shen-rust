//! Differential oracle for the L1 typecheck cache (`interp::tc_cache`):
//! a `(tc +)` load must behave identically whether its verdicts are
//! computed by real proof search (record) or replayed from the cache —
//! and editing the file must invalidate, not replay stale verdicts.
//!
//! Each scenario runs in a fresh process-independent `Interp` (fresh
//! interner, fresh gensym state) over a shared on-disk cache dir, exactly
//! like separate CLI sessions. Uses `tc_cache::install` directly instead
//! of the env-var path so parallel tests can't race on process env.

use std::path::{Path, PathBuf};

use shen_rust::interp::boot::boot_with_kernel;
use shen_rust::interp::eval::Interp;
use shen_rust::interp::tc_cache;
use shen_rust::kl::parser::parse_one;
use shen_rust::value::Value;

fn kernel_klambda_dir() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop(); // crates
    p.pop(); // workspace root
    p.push("kernel");
    p.push("klambda");
    p
}

fn fresh_cached_interp(cache_dir: &Path) -> Interp {
    let mut interp = Interp::new();
    let kernel = kernel_klambda_dir();
    boot_with_kernel(&mut interp, &kernel).unwrap_or_else(|e| panic!("kernel boot failed: {e}"));
    tc_cache::install(&mut interp, cache_dir.to_path_buf(), false, &kernel);
    interp
}

fn eval(interp: &mut Interp, src: &str) -> Value {
    let expr = parse_one(src, &mut interp.symbols).unwrap_or_else(|e| panic!("parse {src:?}: {e}"));
    interp
        .eval(&expr)
        .unwrap_or_else(|e| panic!("eval {src:?}: {e}"))
}

/// Unique-per-call scratch dir under the target temp dir.
fn scratch(label: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("shen-tc-cache-test-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

const PROGRAM_V1: &str =
    "(define idf\n  {A --> A}\n  X -> X)\n\n(define dup\n  {A --> (list A)}\n  X -> [X X])\n";
/// Same file, edited: `dup` now triples.
const PROGRAM_V2: &str =
    "(define idf\n  {A --> A}\n  X -> X)\n\n(define dup\n  {A --> (list A)}\n  X -> [X X X])\n";

/// `(tc +)` then load the file; return `(dup 7)`'s list length as the
/// observable behaviour of the loaded code.
fn tc_load_and_probe(interp: &mut Interp, file: &Path) -> i64 {
    eval(interp, "(tc +)");
    let loaded = eval(interp, &format!("(load \"{}\")", file.display()));
    assert_eq!(
        loaded
            .as_sym()
            .map(|s| interp.symbols.resolve(s).to_string()),
        Some("loaded".to_string())
    );
    eval(interp, "(tc -)");
    let n = eval(interp, "(length (dup 7))");
    n.as_int().expect("length is an int")
}

#[test]
fn record_replay_and_invalidate() {
    // The type-checker recurses deep through non-tail frames; give the
    // whole scenario the big worker stack the real CLI uses.
    std::thread::Builder::new()
        .stack_size(512 * 1024 * 1024)
        .spawn(scenario)
        .expect("spawn")
        .join()
        .expect("scenario thread");
}

fn scenario() {
    let cache = scratch("differential");
    let file = cache.join("prog.shen");
    std::fs::write(&file, PROGRAM_V1).expect("write program");

    // Session 1: cold cache — records.
    let mut a = fresh_cached_interp(&cache);
    assert_eq!(tc_load_and_probe(&mut a, &file), 2);
    assert_eq!(tc_cache::stats(&a), Some((0, 1)), "first load must miss");
    let files_after_record = std::fs::read_dir(&cache)
        .unwrap()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "tc"))
        .count();
    assert_eq!(files_after_record, 1, "record writes one cache file");

    // Session 2: fresh interner/gensym state — must REPLAY, with
    // identical observable behaviour.
    let mut b = fresh_cached_interp(&cache);
    assert_eq!(tc_load_and_probe(&mut b, &file), 2);
    assert_eq!(tc_cache::stats(&b), Some((1, 0)), "second load must hit");

    // Session 3: edit the file — the verdicts no longer apply, so the
    // load must MISS and re-record, and the new behaviour must show.
    std::fs::write(&file, PROGRAM_V2).expect("rewrite program");
    let mut c = fresh_cached_interp(&cache);
    assert_eq!(tc_load_and_probe(&mut c, &file), 3, "edited code behaviour");
    assert_eq!(tc_cache::stats(&c), Some((0, 1)), "edited file must miss");

    // And the original cache entry is still intact for the v1 content.
    std::fs::write(&file, PROGRAM_V1).expect("restore program");
    let mut d = fresh_cached_interp(&cache);
    assert_eq!(tc_load_and_probe(&mut d, &file), 2);
    assert_eq!(tc_cache::stats(&d), Some((1, 0)), "v1 entry replays again");

    let _ = std::fs::remove_dir_all(&cache);
}

/// A file whose form fails to typecheck must fail identically on a warm
/// cache: failed loads are never recorded, so there is nothing to replay.
#[test]
fn type_errors_are_never_cached() {
    std::thread::Builder::new()
        .stack_size(512 * 1024 * 1024)
        .spawn(|| {
            let cache = scratch("type-errors");
            let file = cache.join("bad.shen");
            std::fs::write(
                &file,
                "(define bad\n  {number --> number}\n  X -> (cn X \"!\"))\n",
            )
            .expect("write program");

            for round in 0..2 {
                let mut interp = fresh_cached_interp(&cache);
                eval(&mut interp, "(tc +)");
                let expr = parse_one(
                    &format!("(load \"{}\")", file.display()),
                    &mut interp.symbols,
                )
                .expect("parse load");
                let r = interp.eval(&expr);
                assert!(r.is_err(), "round {round}: ill-typed load must error");
            }
            let cache_files = std::fs::read_dir(&cache)
                .unwrap()
                .flatten()
                .filter(|e| e.path().extension().is_some_and(|x| x == "tc"))
                .count();
            assert_eq!(cache_files, 0, "failed loads must never be recorded");
            let _ = std::fs::remove_dir_all(&cache);
        })
        .expect("spawn")
        .join()
        .expect("scenario thread");
}
