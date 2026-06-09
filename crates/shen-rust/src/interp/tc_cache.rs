//! L1 load-time typecheck memoization (spike).
//!
//! Under `(tc +)`, `load` spends almost all of its time in per-form
//! `shen.typecheck` proof search (~64% of `--kernel-tests` wall, measured
//! 2026-06-09). The verdict of each check is a small type value used only
//! for the `name : type` print; the signature side effects that later
//! forms depend on come from `shen.assumetypes` (the `{...}` annotations),
//! which runs *before* any check. So the checks are memoizable: replay the
//! recorded type values and skip the proof search, while translation,
//! eval, declares, output, and the `unwind-types` error path all still run
//! for real through the unmodified kernel `load`.
//!
//! Mechanism: two thin native wrappers installed over the kernel AOT
//! functions (both `env.functions` and the `aot_direct` table — kernel
//! code calls `shen.typecheck` via `rt::apply_direct`, so overriding only
//! one table would be silently bypassed):
//!
//!   * `load_wrapper` computes a cache key and opens a per-load context
//!     (replay on hit, record on miss), then delegates to the original
//!     `aot_load`;
//!   * `typecheck_wrapper` consults the context: on replay, if the next
//!     recorded entry's argument hash matches the live call, it returns
//!     the recorded type without proof search; any mismatch poisons the
//!     context and falls through to the real `shen.typecheck` (correct,
//!     just uncached) — misalignment can never produce a wrong verdict.
//!
//! Soundness of the key: a verdict is valid only against the type
//! environment (signatures, datatypes, synonyms) in force when it was
//! recorded. That environment is built deterministically by the sequence
//! of prior loads, so the key is a rolling FNV chain over every completed
//! `load` (file-content hash + tc flag) plus this file's content hash.
//! Same prefix + same file ⇒ same environment ⇒ replay is sound. Mid-file
//! effects (a `datatype` earlier in the same file) reproduce because eval
//! still runs in order. Nested `(load ...)` during recording marks the
//! outer file uncacheable (its key can't see the nested file's content)
//! while still extending the chain, so later files key correctly.
//!
//! Failure containment: only loads that complete successfully are written;
//! verdicts of `false` (type errors) and values outside the serializable
//! subset (sym/str/int/float/bool/cons/nil) are recorded as "check for
//! real" entries. A corrupt or mismatched cache file is a miss.
//!
//! GC NOTE: replayed/recorded type `Value`s live in `Interp::tc_cache`,
//! which the collector does not scan. That is safe while collection is
//! grow-only (GC Step 3); when Step 4 turns collection on, these entries
//! must be registered as roots.
//!
//! Off by default. Enable with `SHEN_RUST_TC_CACHE=<dir>`; add
//! `SHEN_RUST_TC_CACHE_STATS=1` for per-load diagnostics on stderr.

use std::fs;
use std::path::{Path, PathBuf};

use crate::error::ShenResult;
use crate::interp::eval::Interp;
use crate::value::Value;

/// On-disk format / keying-scheme version. Bump on any change. The crate
/// version is folded into the key as a proxy for "the kernel or the
/// type-checker changed" — a stale cache after a kernel change without a
/// version bump must be cleared by hand (spike limitation).
const FORMAT: &str = "shentc2";

pub struct TcCacheState {
    dir: PathBuf,
    /// Rolling hash over every completed `load` this session. See module
    /// doc: identical chain + identical file ⇒ identical type environment.
    chain: u64,
    ctx: Option<LoadCtx>,
    stats_on: bool,
    hits: u64,
    misses: u64,
}

struct LoadCtx {
    key: u64,
    /// `(value shen.*gensym*)` at load start. Read-time macro expansion
    /// gensyms variables into the forms (Y3331, S4507, …), and replayed
    /// sessions reach each load with a *lower* counter (skipped proof
    /// searches consume gensyms). Recording the start value and
    /// fast-forwarding to it on replay makes expansion reproduce the
    /// recording exactly. Forward-only (`max`), so it can never collide
    /// existing gensyms.
    gensym_start: i64,
    mode: Mode,
}

enum Mode {
    Replay {
        entries: Vec<Entry>,
        cursor: usize,
        /// Set on any mismatch with the live call sequence; every
        /// remaining check runs for real.
        poisoned: bool,
    },
    Record {
        entries: Vec<Entry>,
        /// A nested `load` ran while recording: this file's verdicts
        /// depend on content outside its key, so don't write it.
        uncacheable: bool,
    },
}

struct Entry {
    /// FNV over both `shen.typecheck` arguments (form + expected type),
    /// `None` when a value outside the hashable subset appeared. Invariant:
    /// `arg_hash == None` ⇒ `ty == None`.
    arg_hash: Option<u64>,
    /// The recorded verdict, `None` for "run the real check here".
    ty: Option<Value>,
}

// ---------------------------------------------------------------- FNV-1a

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

#[derive(Clone, Copy)]
struct Fnv(u64);

impl Fnv {
    fn new() -> Self {
        Fnv(FNV_OFFSET)
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 ^= b as u64;
            self.0 = self.0.wrapping_mul(FNV_PRIME);
        }
    }
    fn u64(&mut self, v: u64) {
        self.write(&v.to_le_bytes());
    }
    fn finish(self) -> u64 {
        self.0
    }
}

fn fnv_bytes(bytes: &[u8]) -> u64 {
    let mut h = Fnv::new();
    h.write(bytes);
    h.finish()
}

/// Hash a value tree. Spine-iterative so long lists don't recurse deep.
/// Returns false (hash unusable) on any value outside the supported subset.
fn hash_value(interp: &Interp, v: &Value, h: &mut Fnv) -> bool {
    let mut cur = v.clone();
    loop {
        if let Some(head) = cur.head() {
            h.write(b"c");
            let head = head.clone();
            let tail = cur.tail().expect("cons has tail").clone();
            if !hash_value(interp, &head, h) {
                return false;
            }
            cur = tail;
            continue;
        }
        if cur.is_nil() {
            h.write(b"n");
        } else if let Some(b) = cur.as_bool() {
            h.write(if b { b"t" } else { b"u" });
        } else if let Some(i) = cur.as_int() {
            h.write(b"i");
            h.u64(i as u64);
        } else if let Some(s) = cur.as_sym() {
            let name = interp.symbols.resolve(s);
            h.write(b"y");
            h.u64(name.len() as u64);
            h.write(name.as_bytes());
        } else if let Some(s) = cur.as_str() {
            h.write(b"s");
            h.u64(s.len() as u64);
            h.write(s.as_bytes());
        } else if let Some(f) = cur.as_float() {
            h.write(b"f");
            h.u64(f.to_bits());
        } else {
            return false;
        }
        return true;
    }
}

fn hash_args(interp: &Interp, args: &[Value]) -> Option<u64> {
    let mut h = Fnv::new();
    h.u64(args.len() as u64);
    for a in args {
        if !hash_value(interp, a, &mut h) {
            return None;
        }
    }
    Some(h.finish())
}

// ------------------------------------------------------- (de)serialization
//
// Length-prefixed, structural (no separators, newline-safe):
//   n nil | t/u bool | i<i64>; | f<f64-bits-hex>; | y<len>:<sym-name>
//   s<len>:<string>  | c<car><cdr>

fn serialize_value(interp: &Interp, v: &Value, out: &mut String) -> bool {
    let mut cur = v.clone();
    loop {
        if let Some(head) = cur.head() {
            out.push('c');
            let head = head.clone();
            let tail = cur.tail().expect("cons has tail").clone();
            if !serialize_value(interp, &head, out) {
                return false;
            }
            cur = tail;
            continue;
        }
        if cur.is_nil() {
            out.push('n');
        } else if let Some(b) = cur.as_bool() {
            out.push(if b { 't' } else { 'u' });
        } else if let Some(i) = cur.as_int() {
            out.push('i');
            out.push_str(&i.to_string());
            out.push(';');
        } else if let Some(s) = cur.as_sym() {
            let name = interp.symbols.resolve(s);
            out.push('y');
            out.push_str(&name.len().to_string());
            out.push(':');
            out.push_str(name);
        } else if let Some(s) = cur.as_str() {
            out.push('s');
            out.push_str(&s.len().to_string());
            out.push(':');
            out.push_str(s);
        } else if let Some(f) = cur.as_float() {
            out.push('f');
            out.push_str(&format!("{:016x}", f.to_bits()));
            out.push(';');
        } else {
            return false;
        }
        return true;
    }
}

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn byte(&mut self) -> Option<u8> {
        let b = *self.bytes.get(self.pos)?;
        self.pos += 1;
        Some(b)
    }

    /// Read digits up to `stop`, consuming it.
    fn number_until(&mut self, stop: u8) -> Option<&'a str> {
        let start = self.pos;
        while *self.bytes.get(self.pos)? != stop {
            self.pos += 1;
        }
        let s = std::str::from_utf8(&self.bytes[start..self.pos]).ok()?;
        self.pos += 1; // consume stop
        Some(s)
    }

    fn take(&mut self, n: usize) -> Option<&'a str> {
        let s = self.bytes.get(self.pos..self.pos + n)?;
        self.pos += n;
        std::str::from_utf8(s).ok()
    }

    fn value(&mut self, interp: &mut Interp) -> Option<Value> {
        // Collect the cons spine iteratively, then fold back.
        let mut spine: Vec<Value> = Vec::new();
        loop {
            match self.byte()? {
                b'c' => {
                    let car = self.value(interp)?;
                    spine.push(car);
                }
                tag => {
                    let mut v = self.atom(tag, interp)?;
                    while let Some(car) = spine.pop() {
                        v = Value::cons(car, v);
                    }
                    return Some(v);
                }
            }
        }
    }

    fn atom(&mut self, tag: u8, interp: &mut Interp) -> Option<Value> {
        match tag {
            b'n' => Some(Value::nil()),
            b't' => Some(Value::bool(true)),
            b'u' => Some(Value::bool(false)),
            b'i' => self.number_until(b';')?.parse::<i64>().ok().map(Value::int),
            b'f' => {
                let bits = u64::from_str_radix(self.number_until(b';')?, 16).ok()?;
                Some(Value::float(f64::from_bits(bits)))
            }
            b'y' => {
                let len = self.number_until(b':')?.parse::<usize>().ok()?;
                let name = self.take(len)?.to_string();
                Some(Value::sym(interp.intern(&name)))
            }
            b's' => {
                let len = self.number_until(b':')?.parse::<usize>().ok()?;
                Some(Value::str(self.take(len)?))
            }
            _ => None,
        }
    }
}

// ------------------------------------------------------------- cache files

fn cache_path(dir: &Path, key: u64) -> PathBuf {
    dir.join(format!("{key:016x}.tc"))
}

fn read_cache(interp: &mut Interp, path: &Path) -> Option<(i64, Vec<Entry>)> {
    let bytes = fs::read(path).ok()?;
    let header = format!("{FORMAT} ");
    let rest = bytes.strip_prefix(header.as_bytes())?;
    let mut p = Parser {
        bytes: rest,
        pos: 0,
    };
    let gensym_start: i64 = p.number_until(b' ')?.parse().ok()?;
    let count: usize = p.number_until(b'\n')?.parse().ok()?;
    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        let arg_hash = match p.byte()? {
            b'+' => Some(u64::from_str_radix(p.take(16)?, 16).ok()?),
            b'-' => None,
            _ => return None,
        };
        let ty = match p.byte()? {
            b'+' => Some(p.value(interp)?),
            b'-' => None,
            _ => return None,
        };
        if arg_hash.is_none() && ty.is_some() {
            return None; // violates the recording invariant: corrupt
        }
        entries.push(Entry { arg_hash, ty });
    }
    Some((gensym_start, entries))
}

fn write_cache(interp: &Interp, path: &Path, gensym_start: i64, entries: &[Entry]) {
    let mut out = format!("{FORMAT} {gensym_start} {}\n", entries.len());
    for e in entries {
        match e.arg_hash {
            Some(h) => {
                out.push('+');
                out.push_str(&format!("{h:016x}"));
            }
            None => out.push('-'),
        }
        // Serialize the verdict; a failure downgrades the entry to
        // "check for real" rather than aborting the file.
        let mut ty_text = String::new();
        match &e.ty {
            Some(v) if e.arg_hash.is_some() && serialize_value(interp, v, &mut ty_text) => {
                out.push('+');
                out.push_str(&ty_text);
            }
            _ => out.push('-'),
        }
    }
    // Write-then-rename so a crash never leaves a torn file behind.
    let tmp = path.with_extension("tc.tmp");
    if fs::write(&tmp, out)
        .and_then(|_| fs::rename(&tmp, path))
        .is_err()
    {
        let _ = fs::remove_file(&tmp);
    }
}

// ---------------------------------------------------------------- wrappers

fn original_load(interp: &mut Interp, args: &[Value]) -> ShenResult<Value> {
    crate::aot::kernel::load::aot_load(interp, args)
}

fn original_typecheck(interp: &mut Interp, args: &[Value]) -> ShenResult<Value> {
    crate::aot::kernel::t_star::aot_shen_x2e_typecheck(interp, args)
}

/// Is `(value shen.*tc*)` anything other than `false`? Mirrors
/// `shen.load-help`'s `(= false TC)` test.
fn tc_is_on(interp: &mut Interp) -> Option<bool> {
    let sym = interp.intern("shen.*tc*");
    let v = interp.env.get_global(sym)?;
    Some(v.as_bool() != Some(false))
}

fn chain_step(chain: u64, file_hash: u64, tc_on: bool) -> u64 {
    let mut h = Fnv::new();
    h.u64(chain);
    h.u64(file_hash);
    h.write(if tc_on { b"+" } else { b"-" });
    h.finish()
}

pub fn load_wrapper(interp: &mut Interp, args: &[Value]) -> ShenResult<Value> {
    if interp.tc_cache.is_none() {
        return original_load(interp, args);
    }
    // Resolve the file exactly as `read-file` will (relative to cwd). If we
    // can't read it, the kernel load will raise its own error — delegate.
    let Some(path) = args.first().and_then(|v| v.as_str()).map(str::to_string) else {
        return original_load(interp, args);
    };
    let Ok(bytes) = fs::read(&path) else {
        return original_load(interp, args);
    };
    let file_hash = fnv_bytes(&bytes);
    let Some(tc_on) = tc_is_on(interp) else {
        return original_load(interp, args);
    };

    let nested = interp.tc_cache.as_ref().is_some_and(|st| st.ctx.is_some());
    if nested || !tc_on {
        // Nested loads and tc-off loads run unwrapped, but still extend the
        // chain: their evals mutate the type environment (datatypes etc.)
        // that later files' verdicts depend on.
        let r = original_load(interp, args);
        if r.is_ok() {
            if let Some(st) = interp.tc_cache.as_mut() {
                st.chain = chain_step(st.chain, file_hash, tc_on);
                if nested {
                    match st.ctx.as_mut().map(|c| &mut c.mode) {
                        Some(Mode::Record { uncacheable, .. }) => *uncacheable = true,
                        Some(Mode::Replay { poisoned, .. }) => *poisoned = true,
                        None => {}
                    }
                }
            }
        }
        return r;
    }

    // Open a record/replay context for this load.
    let (key, dir, stats_on) = {
        let st = interp.tc_cache.as_ref().expect("checked above");
        let mut h = Fnv::new();
        h.write(FORMAT.as_bytes());
        h.write(env!("CARGO_PKG_VERSION").as_bytes());
        h.u64(st.chain);
        h.u64(file_hash);
        (h.finish(), st.dir.clone(), st.stats_on)
    };
    let gensym_sym = interp.intern("shen.*gensym*");
    let live_gensym = interp
        .env
        .get_global(gensym_sym)
        .and_then(|v| v.as_int())
        .unwrap_or(0);
    let (gensym_start, mode) = match read_cache(interp, &cache_path(&dir, key)) {
        Some((recorded_gensym, entries)) => {
            interp.tc_cache.as_mut().expect("checked").hits += 1;
            // Fast-forward (never backward) so read-time macro expansion
            // gensyms the same names the recording saw. See `LoadCtx`.
            if recorded_gensym > live_gensym {
                interp
                    .env
                    .set_global(gensym_sym, Value::int(recorded_gensym));
            }
            (
                live_gensym.max(recorded_gensym),
                Mode::Replay {
                    entries,
                    cursor: 0,
                    poisoned: false,
                },
            )
        }
        None => {
            interp.tc_cache.as_mut().expect("checked").misses += 1;
            (
                live_gensym,
                Mode::Record {
                    entries: Vec::new(),
                    uncacheable: false,
                },
            )
        }
    };
    interp.tc_cache.as_mut().expect("checked").ctx = Some(LoadCtx {
        key,
        gensym_start,
        mode,
    });

    let r = original_load(interp, args);

    let ctx = interp
        .tc_cache
        .as_mut()
        .expect("checked")
        .ctx
        .take()
        .expect("ctx set above");
    if r.is_ok() {
        if let Some(st) = interp.tc_cache.as_mut() {
            st.chain = chain_step(st.chain, file_hash, tc_on);
        }
        if stats_on {
            describe_load(interp, &path, &ctx);
        }
        if let Mode::Record {
            entries,
            uncacheable: false,
        } = &ctx.mode
        {
            write_cache(
                interp,
                &cache_path(&dir, ctx.key),
                ctx.gensym_start,
                entries,
            );
        }
    }
    r
}

fn describe_load(interp: &Interp, path: &str, ctx: &LoadCtx) {
    match &ctx.mode {
        Mode::Replay {
            entries,
            cursor,
            poisoned,
        } => eprintln!(
            "tc-cache: {path}: replayed {cursor}/{} verdicts{}",
            entries.len(),
            if *poisoned { " (POISONED)" } else { "" }
        ),
        Mode::Record {
            entries,
            uncacheable,
        } => {
            let cached = entries.iter().filter(|e| e.ty.is_some()).count();
            eprintln!(
                "tc-cache: {path}: recorded {cached}/{} verdicts{}",
                entries.len(),
                if *uncacheable {
                    " (UNCACHEABLE: nested load)"
                } else {
                    ""
                }
            );
        }
    }
    let _ = interp;
}

pub fn typecheck_wrapper(interp: &mut Interp, args: &[Value]) -> ShenResult<Value> {
    let in_ctx = interp.tc_cache.as_ref().is_some_and(|st| st.ctx.is_some());
    if !in_ctx {
        return original_typecheck(interp, args);
    }

    let arg_hash = hash_args(interp, args);
    // Diagnostic context for divergence reports (stats mode only): the
    // head symbol of the form under check.
    let dbg_head: Option<String> = interp
        .tc_cache
        .as_ref()
        .filter(|st| st.stats_on)
        .and_then(|_| args.first())
        .and_then(|f| f.head().cloned())
        .and_then(|h| h.as_sym())
        .map(|s| interp.symbols.resolve(s).to_string());
    let dbg_form: Option<String> = interp
        .tc_cache
        .as_ref()
        .filter(|st| st.stats_on)
        .and_then(|_| args.first())
        .map(|f| {
            let mut s = String::new();
            serialize_value(interp, f, &mut s);
            s.chars().take(400).collect()
        });

    enum Plan {
        Real,
        RealAndRecord,
        Cached(Value),
    }
    let plan = {
        let st = interp.tc_cache.as_mut().expect("checked");
        match st.ctx.as_mut().map(|c| &mut c.mode) {
            Some(Mode::Replay {
                entries,
                cursor,
                poisoned,
            }) => {
                if *poisoned {
                    Plan::Real
                } else if let Some(e) = entries.get(*cursor) {
                    if e.arg_hash == arg_hash {
                        *cursor += 1;
                        match &e.ty {
                            Some(v) => Plan::Cached(v.clone()),
                            None => Plan::Real,
                        }
                    } else {
                        // Live call sequence diverged from the recording
                        // (different forms, an extra typecheck from eval'd
                        // code, …). Never guess: full proof search from here.
                        *poisoned = true;
                        if st.stats_on {
                            eprintln!(
                                "tc-cache: DIVERGE at entry {} (recorded {:016x?}, live {:016x?}, form head {dbg_head:?})\n  live form: {}",
                                *cursor,
                                e.arg_hash,
                                arg_hash,
                                dbg_form.as_deref().unwrap_or("?")
                            );
                        }
                        Plan::Real
                    }
                } else {
                    *poisoned = true;
                    Plan::Real
                }
            }
            Some(Mode::Record { .. }) => Plan::RealAndRecord,
            None => Plan::Real,
        }
    };

    match plan {
        Plan::Cached(v) => Ok(v),
        Plan::Real => original_typecheck(interp, args),
        Plan::RealAndRecord => {
            let r = original_typecheck(interp, args)?;
            // Cache only successful verdicts; `false` (type error) and
            // unhashable calls stay "check for real" so a replayed load
            // can never skip a check that should fail.
            let ty = if arg_hash.is_some() && r.as_bool() != Some(false) {
                Some(r.clone())
            } else {
                None
            };
            if let Some(st) = interp.tc_cache.as_mut() {
                if let Some(Mode::Record { entries, .. }) = st.ctx.as_mut().map(|c| &mut c.mode) {
                    entries.push(Entry { arg_hash, ty });
                }
            }
            Ok(r)
        }
    }
}

// ------------------------------------------------------------ installation

/// Install the cache if `SHEN_RUST_TC_CACHE=<dir>` is set. Called at the
/// end of boot so the wrappers land over the kernel AOT registrations.
pub fn install_from_env(interp: &mut Interp) {
    let Some(dir) = std::env::var_os("SHEN_RUST_TC_CACHE") else {
        return;
    };
    if dir.is_empty() {
        return;
    }
    let dir = PathBuf::from(dir);
    if let Err(e) = fs::create_dir_all(&dir) {
        eprintln!("tc-cache: create {}: {e} — disabled", dir.display());
        return;
    }
    interp.tc_cache = Some(Box::new(TcCacheState {
        dir,
        chain: 0,
        ctx: None,
        stats_on: std::env::var_os("SHEN_RUST_TC_CACHE_STATS").is_some(),
        hits: 0,
        misses: 0,
    }));
    // Both tables, native first (it clears the direct slot), mirroring the
    // kernel AOT installers — kernel code reaches these via apply_direct.
    interp.register_native("load", 1, load_wrapper);
    interp.register_aot_direct("load", load_wrapper);
    interp.register_native("shen.typecheck", 2, typecheck_wrapper);
    interp.register_aot_direct("shen.typecheck", typecheck_wrapper);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_type(interp: &mut Interp) -> Value {
        // ((list number) --> (number --> string)) plus odd atoms
        let num = Value::sym(interp.intern("number"));
        let arrow = Value::sym(interp.intern("-->"));
        let list = Value::sym(interp.intern("list"));
        Value::list([
            Value::list([list, num.clone()]),
            arrow.clone(),
            Value::list([
                num,
                arrow,
                Value::str("st:r\ning"),
                Value::int(-42),
                Value::float(1.5),
                Value::bool(true),
                Value::nil(),
            ]),
        ])
    }

    #[test]
    fn serialize_roundtrip() {
        let mut interp = Interp::new();
        let v = sample_type(&mut interp);
        let mut text = String::new();
        assert!(serialize_value(&interp, &v, &mut text));
        let mut p = Parser {
            bytes: text.as_bytes(),
            pos: 0,
        };
        let back = p.value(&mut interp).expect("parse back");
        assert_eq!(p.pos, text.len(), "parser consumed everything");
        assert!(crate::value::shen_eq(&v, &back));
        // And the hash agrees with itself across the round trip.
        let mut h1 = Fnv::new();
        let mut h2 = Fnv::new();
        assert!(hash_value(&interp, &v, &mut h1));
        assert!(hash_value(&interp, &back, &mut h2));
        assert_eq!(h1.finish(), h2.finish());
    }

    #[test]
    fn long_spine_does_not_recurse() {
        let mut interp = Interp::new();
        let mut v = Value::nil();
        for i in 0..200_000 {
            v = Value::cons(Value::int(i), v);
        }
        let mut h = Fnv::new();
        assert!(hash_value(&interp, &v, &mut h));
        let mut text = String::new();
        assert!(serialize_value(&interp, &v, &mut text));
        let mut p = Parser {
            bytes: text.as_bytes(),
            pos: 0,
        };
        let back = p.value(&mut interp).expect("parse back");
        // Compare the spines iteratively — `shen_eq` recurses and would
        // itself overflow the (debug) test-thread stack at this depth.
        let (mut a, mut b) = (v, back);
        while a.is_cons() || b.is_cons() {
            let (ah, bh) = (a.head().expect("a cons"), b.head().expect("b cons"));
            assert_eq!(ah.as_int(), bh.as_int());
            let at = a.tail().expect("a cons").clone();
            let bt = b.tail().expect("b cons").clone();
            a = at;
            b = bt;
        }
        assert!(a.is_nil() && b.is_nil());
    }

    #[test]
    fn closures_are_not_serializable() {
        let interp = Interp::new();
        let v = Value::absvector(vec![Value::int(1)]);
        let mut text = String::new();
        assert!(!serialize_value(&interp, &v, &mut text));
        let mut h = Fnv::new();
        assert!(!hash_value(&interp, &v, &mut h));
    }
}
