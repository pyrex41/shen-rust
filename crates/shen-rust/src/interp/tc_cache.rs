//! Load-time typecheck memoization with nesting-sound keying.
//!
//! Under `(tc +)`, `load` spends almost all of its time in per-form
//! `shen.typecheck` proof search (~64% of `--kernel-tests` wall, measured
//! 2026-06-09). The verdicts are deterministic functions of the load
//! history (which builds the type/macro environment) and the
//! `shen.*gensym*` counter, so they are memoizable per load: replay the
//! recorded types and skip the proof search, while translation, eval,
//! declares, output, and the `unwind-types` error path all still run for
//! real through the unmodified kernel `load`.
//!
//! (Measured dead ends, don't re-litigate without new evidence: replaying
//! `shen.shen->kl` translations and `shen.compile-prolog` codegen through
//! this same stream machinery was built and measured 2026-06-09 — both
//! were wall-neutral on the suite; replay hashing/deserialization costs
//! about what the walk-shaped work saves. Write-set capture for
//! `shen.process-datatype` is a non-starter: the kernel `put` mutates the
//! property-vector dict in place via `address->`/`vec_set`, bypassing
//! `Env`, and registry values include closures. See git history of this
//! file for the working three-stream implementation.)
//!
//! Mechanism: thin native wrappers installed over the kernel AOT
//! functions (in both `env.functions` and the `aot_direct` table — kernel
//! code calls these via `rt::apply_direct`, so overriding only one table
//! would be silently bypassed):
//!
//!   * `load_wrapper` computes a cache key, pushes a per-load context
//!     (replay on hit, record on miss) onto a stack — loads nest:
//!     `runme.shen` loads `kerneltests.shen` loads the test files — and
//!     delegates to the original `aot_load`;
//!   * `typecheck_wrapper` consults the top context: on replay, if the
//!     next recorded entry's argument hash matches the live call, the
//!     recorded verdict is returned without proof search; ANY mismatch
//!     poisons the context and every remaining call runs for real —
//!     misalignment can never produce a wrong verdict.
//!
//! Soundness of the key. A recorded verdict is valid only against the
//! type/macro environment in force when it was recorded. That environment
//! is built deterministically by (a) the sequence of prior completed
//! loads — covered by a rolling FNV chain over each completed load's
//! content hash + tc flag, seeded from the kernel sources; (b) for a
//! nested load, the enclosing loads' progress so far — covered by folding
//! each enclosing context's file hash and entry digest (a running hash of
//! every consumed/recorded entry) into the key; and (c) the gensym
//! counter at load start — folded into the key, and consistent across
//! sessions because replay pins the counter at load boundaries and after
//! every served entry (see below). Same key ⇒ same environment ⇒ replay
//! is sound. Mid-file effects (a `datatype` earlier in the same file)
//! reproduce because eval still runs in order.
//!
//! Nested-load validation: when a nested load completes, the new chain
//! value is checkpointed into the parent context. On replay the
//! checkpoint must equal the recording's — so editing an inner file
//! poisons the parent's remaining entries instead of replaying verdicts
//! recorded against the old inner content. A nested load that *fails*
//! poisons the parent outright.
//!
//! Gensym pinning: read-time macro expansion gensyms variables into the
//! forms (Y3331, S4507, …), and a replaying session reaches each load
//! with a lower counter (skipped work consumes gensyms). Each recording
//! stores the counter at load start and end, and after every entry;
//! replay fast-forwards (never backward) at each of those points, so
//! expansion reproduces the recording exactly and mixed replay/live runs
//! never interleave divergent gensym numbering.
//!
//! Failure containment: only loads that complete successfully are
//! written; `false` verdicts and values outside the serializable subset
//! (sym/str/int/float/bool/cons/nil) are recorded as "do it for real"
//! entries. A corrupt or mismatched cache file is a miss.
//!
//! Known limitation: interactive sessions that mutate the type/macro
//! environment between loads *without* consuming a gensym or changing any
//! load content can replay stale verdicts. Load-driven flows (CLI,
//! served, kernel tests) are unaffected.
//!
//! GC NOTE: replayed/recorded `Value`s live in `Interp::tc_cache`, a heap
//! container the conservative stack scan cannot see. GC Step 4 therefore
//! enumerates them as precise roots via [`TcCacheState::push_gc_roots`]
//! (wired into `Interp::gc_roots`).
//!
//! Off by default. Enable with `SHEN_RUST_TC_CACHE=<dir>`; add
//! `SHEN_RUST_TC_CACHE_STATS=1` for per-load diagnostics on stderr.

use std::fs;
use std::path::{Path, PathBuf};

use crate::error::ShenResult;
use crate::interp::eval::Interp;
use crate::value::Value;

/// On-disk format / keying-scheme version. Bump on any change.
const FORMAT: &str = "shentc3";

pub struct TcCacheState {
    dir: PathBuf,
    /// Rolling hash over every completed `load` this session, seeded from
    /// the kernel sources. See module doc.
    chain: u64,
    /// In-flight load contexts, innermost last.
    stack: Vec<LoadCtx>,
    stats_on: bool,
    hits: u64,
    misses: u64,
}

impl TcCacheState {
    /// Push every `Value` held by in-flight load contexts into `out` (GC
    /// Step 4 precise-root enumeration). Plain field walks — no heap access.
    pub fn push_gc_roots(&self, out: &mut Vec<Value>) {
        for ctx in &self.stack {
            out.extend(ctx.tc.entries.iter().filter_map(|e| e.val));
        }
    }
}

struct LoadCtx {
    key: u64,
    file_hash: u64,
    /// `(value shen.*gensym*)` at load start (recorded / pinned).
    gensym_start: i64,
    /// Replay only: the recording's counter at load end; pinned forward
    /// on completion so subsequent loads stay aligned with the recording.
    gensym_end_recorded: i64,
    replay: bool,
    /// Replay: live calls diverged from the recording — serve nothing
    /// more. Record: a nested load failed or was unkeyable — don't write.
    poisoned: bool,
    /// Running fingerprint of this load's progress (every entry hash and
    /// child checkpoint, in order). Folded into nested loads' keys.
    digest: Fnv,
    /// Typecheck verdicts: a guarded stream over the recording.
    tc: Stream,
    /// Chain values at each nested load's completion. Replay validates,
    /// record accumulates.
    ckpt: Vec<u64>,
    ckpt_cursor: usize,
    /// Re-entrancy depth per wrapped function: only depth-0 calls are
    /// recorded/replayed, so nested self-calls (which a replayed parent
    /// call would skip entirely) never desync the streams.
    tc_depth: u32,
}

struct Stream {
    entries: Vec<Entry>,
    cursor: usize,
}

impl Stream {
    fn empty() -> Self {
        Stream {
            entries: Vec::new(),
            cursor: 0,
        }
    }
}

struct Entry {
    /// FNV over the call's arguments, `None` when a value outside the
    /// hashable subset appeared. Invariant: `arg_hash == None` ⇒
    /// `val == None`.
    arg_hash: Option<u64>,
    /// The recorded result, `None` for "do it for real here".
    val: Option<Value>,
    /// `shen.*gensym*` immediately after the recorded call. Replay pins
    /// the counter forward to this after serving, so calls interleaved
    /// with other gensym consumers (compile-prolog runs *during* read)
    /// keep the rest of the recording aligned — and gensym-named GLOBAL
    /// artifacts (shen.memberNNN) never mix recorded and live numbering.
    gensym_after: i64,
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

    fn entry(&mut self, interp: &mut Interp) -> Option<Entry> {
        let arg_hash = match self.byte()? {
            b'+' => Some(u64::from_str_radix(self.take(16)?, 16).ok()?),
            b'-' => None,
            _ => return None,
        };
        let val = match self.byte()? {
            b'+' => Some(self.value(interp)?),
            b'-' => None,
            _ => return None,
        };
        if arg_hash.is_none() && val.is_some() {
            return None; // violates the recording invariant: corrupt
        }
        if self.byte()? != b'g' {
            return None;
        }
        let gensym_after: i64 = self.number_until(b';')?.parse().ok()?;
        Some(Entry {
            arg_hash,
            val,
            gensym_after,
        })
    }
}

// ------------------------------------------------------------- cache files

fn cache_path(dir: &Path, key: u64) -> PathBuf {
    dir.join(format!("{key:016x}.tc"))
}

struct Recording {
    gensym_start: i64,
    gensym_end: i64,
    tc: Vec<Entry>,
    ckpt: Vec<u64>,
}

fn read_cache(interp: &mut Interp, path: &Path) -> Option<Recording> {
    let bytes = fs::read(path).ok()?;
    let header = format!("{FORMAT} ");
    let rest = bytes.strip_prefix(header.as_bytes())?;
    let mut p = Parser {
        bytes: rest,
        pos: 0,
    };
    let gensym_start: i64 = p.number_until(b' ')?.parse().ok()?;
    let gensym_end: i64 = p.number_until(b' ')?.parse().ok()?;
    let ntc: usize = p.number_until(b' ')?.parse().ok()?;
    let nckpt: usize = p.number_until(b'\n')?.parse().ok()?;
    let mut tc = Vec::with_capacity(ntc);
    for _ in 0..ntc {
        tc.push(p.entry(interp)?);
    }
    let mut ckpt = Vec::with_capacity(nckpt);
    for _ in 0..nckpt {
        ckpt.push(u64::from_str_radix(p.take(16)?, 16).ok()?);
    }
    Some(Recording {
        gensym_start,
        gensym_end,
        tc,
        ckpt,
    })
}

fn write_entries(interp: &Interp, entries: &[Entry], out: &mut String) {
    for e in entries {
        match e.arg_hash {
            Some(h) => {
                out.push('+');
                out.push_str(&format!("{h:016x}"));
            }
            None => out.push('-'),
        }
        // Serialize the value; a failure downgrades the entry to "do it
        // for real" rather than aborting the file.
        let mut text = String::new();
        match &e.val {
            Some(v) if e.arg_hash.is_some() && serialize_value(interp, v, &mut text) => {
                out.push('+');
                out.push_str(&text);
            }
            _ => out.push('-'),
        }
        out.push('g');
        out.push_str(&e.gensym_after.to_string());
        out.push(';');
    }
}

fn write_cache(interp: &Interp, path: &Path, ctx: &LoadCtx, gensym_end: i64) {
    let mut out = format!(
        "{FORMAT} {} {gensym_end} {} {}\n",
        ctx.gensym_start,
        ctx.tc.entries.len(),
        ctx.ckpt.len()
    );
    write_entries(interp, &ctx.tc.entries, &mut out);
    for c in &ctx.ckpt {
        out.push_str(&format!("{c:016x}"));
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

fn gensym_counter(interp: &mut Interp) -> i64 {
    let sym = interp.intern("shen.*gensym*");
    interp
        .env
        .get_global(sym)
        .and_then(|v| v.as_int())
        .unwrap_or(0)
}

fn pin_gensym_forward(interp: &mut Interp, to: i64) {
    if to > gensym_counter(interp) {
        let sym = interp.intern("shen.*gensym*");
        interp.env.set_global(sym, Value::int(to));
    }
}

fn chain_step(chain: u64, file_hash: u64, tc_on: bool) -> u64 {
    let mut h = Fnv::new();
    h.u64(chain);
    h.u64(file_hash);
    h.write(if tc_on { b"+" } else { b"-" });
    h.finish()
}

/// Mark the innermost in-flight context dirty: it can no longer trust or
/// produce a recording (used when a nested load fails or can't be keyed).
fn poison_top(interp: &mut Interp) {
    if let Some(st) = interp.tc_cache.as_mut() {
        if let Some(top) = st.stack.last_mut() {
            top.poisoned = true;
        }
    }
}

pub fn load_wrapper(interp: &mut Interp, args: &[Value]) -> ShenResult<Value> {
    if interp.tc_cache.is_none() {
        return original_load(interp, args);
    }
    let Some(path) = args.first().and_then(|v| v.as_str()).map(str::to_string) else {
        poison_top(interp);
        return original_load(interp, args);
    };
    let Ok(bytes) = fs::read(&path) else {
        // The kernel load will raise its own error; if it somehow
        // succeeds we couldn't have keyed it.
        poison_top(interp);
        return original_load(interp, args);
    };
    let file_hash = fnv_bytes(&bytes);
    let Some(tc_on) = tc_is_on(interp) else {
        poison_top(interp);
        return original_load(interp, args);
    };
    let live_gensym = gensym_counter(interp);

    // Key: prior completed loads (chain) + enclosing loads' identity and
    // progress (file hash + entry digest per level) + this file + tc flag
    // + the gensym counter. See module doc for why each is needed.
    let (key, dir, stats_on) = {
        let st = interp.tc_cache.as_ref().expect("checked above");
        let mut h = Fnv::new();
        h.write(FORMAT.as_bytes());
        h.write(env!("CARGO_PKG_VERSION").as_bytes());
        h.u64(st.chain);
        for ctx in &st.stack {
            h.u64(ctx.file_hash);
            h.u64(ctx.digest.finish());
        }
        h.u64(file_hash);
        h.write(if tc_on { b"+" } else { b"-" });
        h.u64(live_gensym as u64);
        (h.finish(), st.dir.clone(), st.stats_on)
    };

    let ctx = match read_cache(interp, &cache_path(&dir, key)) {
        Some(rec) => {
            // Fast-forward (never backward) so read-time macro expansion
            // gensyms the same names the recording saw.
            pin_gensym_forward(interp, rec.gensym_start);
            let st = interp.tc_cache.as_mut().expect("checked");
            st.hits += 1;
            LoadCtx {
                key,
                file_hash,
                gensym_start: live_gensym.max(rec.gensym_start),
                gensym_end_recorded: rec.gensym_end,
                replay: true,
                poisoned: false,
                digest: Fnv::new(),
                tc: Stream {
                    entries: rec.tc,
                    cursor: 0,
                },
                ckpt: rec.ckpt,
                ckpt_cursor: 0,
                tc_depth: 0,
            }
        }
        None => {
            let st = interp.tc_cache.as_mut().expect("checked");
            st.misses += 1;
            LoadCtx {
                key,
                file_hash,
                gensym_start: live_gensym,
                gensym_end_recorded: 0,
                replay: false,
                poisoned: false,
                digest: Fnv::new(),
                tc: Stream::empty(),
                ckpt: Vec::new(),
                ckpt_cursor: 0,
                tc_depth: 0,
            }
        }
    };
    interp.tc_cache.as_mut().expect("checked").stack.push(ctx);

    let r = original_load(interp, args);

    let ctx = interp
        .tc_cache
        .as_mut()
        .expect("checked")
        .stack
        .pop()
        .expect("ctx pushed above");

    if r.is_err() {
        // The parent can't trust its remaining recording: this failure's
        // partial side effects aren't validated by any checkpoint.
        poison_top(interp);
        return r;
    }

    // Completed: extend the chain, align the gensym counter with the
    // recording, persist a fresh recording, checkpoint into the parent.
    let new_chain = {
        let st = interp.tc_cache.as_mut().expect("checked");
        st.chain = chain_step(st.chain, file_hash, tc_on);
        st.chain
    };
    if ctx.replay {
        pin_gensym_forward(interp, ctx.gensym_end_recorded);
    } else if !ctx.poisoned {
        let end = gensym_counter(interp);
        write_cache(interp, &cache_path(&dir, ctx.key), &ctx, end);
    }
    {
        let st = interp.tc_cache.as_mut().expect("checked");
        if let Some(parent) = st.stack.last_mut() {
            parent.digest.u64(new_chain);
            if parent.replay {
                if parent.ckpt.get(parent.ckpt_cursor) == Some(&new_chain) {
                    parent.ckpt_cursor += 1;
                } else {
                    parent.poisoned = true;
                }
            } else {
                parent.ckpt.push(new_chain);
            }
        }
    }
    if stats_on {
        describe_load(&path, &ctx);
    }
    r
}

fn describe_load(path: &str, ctx: &LoadCtx) {
    let kind = if ctx.replay { "replayed" } else { "recorded" };
    let served = |s: &Stream| {
        if ctx.replay {
            s.cursor
        } else {
            s.entries.iter().filter(|e| e.val.is_some()).count()
        }
    };
    eprintln!(
        "tc-cache: {path}: {kind} tc {}/{}{}",
        served(&ctx.tc),
        ctx.tc.entries.len(),
        if ctx.poisoned { " (POISONED)" } else { "" }
    );
}

/// Which guarded stream a wrapper call belongs to.
#[derive(Clone, Copy)]
enum Which {
    Tc,
}

fn stream_wrapper(
    interp: &mut Interp,
    args: &[Value],
    which: Which,
    orig: fn(&mut Interp, &[Value]) -> ShenResult<Value>,
) -> ShenResult<Value> {
    // Fast path: no in-flight load context.
    let in_ctx = interp
        .tc_cache
        .as_ref()
        .is_some_and(|st| !st.stack.is_empty());
    if !in_ctx {
        return orig(interp, args);
    }

    let arg_hash = hash_args(interp, args);

    enum Plan {
        Passthrough,
        RealAndRecord,
        Cached(Value, i64),
    }
    let plan = {
        let st = interp.tc_cache.as_mut().expect("checked");
        let top = st.stack.last_mut().expect("checked");
        let depth = match which {
            Which::Tc => top.tc_depth,
        };
        if top.poisoned || depth > 0 {
            Plan::Passthrough
        } else if top.replay {
            let stream = match which {
                Which::Tc => &mut top.tc,
            };
            match stream.entries.get(stream.cursor) {
                Some(e) if e.arg_hash == arg_hash => {
                    stream.cursor += 1;
                    let val = e.val.clone();
                    let pin = e.gensym_after;
                    top.digest.u64(arg_hash.unwrap_or(0));
                    match val {
                        Some(v) => Plan::Cached(v, pin),
                        None => Plan::Passthrough,
                    }
                }
                _ => {
                    // Diverged from the recording (or ran past it).
                    // Never guess: everything runs for real from here.
                    top.poisoned = true;
                    Plan::Passthrough
                }
            }
        } else {
            Plan::RealAndRecord
        }
    };

    match plan {
        Plan::Cached(v, pin) => {
            // Keep the gensym stream aligned with the recording even
            // though the skipped work would have consumed gensyms.
            pin_gensym_forward(interp, pin);
            Ok(v)
        }
        Plan::Passthrough => with_depth(interp, which, orig, args),
        Plan::RealAndRecord => {
            let r = with_depth(interp, which, orig, args)?;
            // `false` typecheck verdicts mean the load is about to fail
            // (never persisted anyway); keep them "for real" defensively.
            let cacheable = match which {
                Which::Tc => r.as_bool() != Some(false),
            };
            let val = if arg_hash.is_some() && cacheable {
                Some(r.clone())
            } else {
                None
            };
            let gensym_after = gensym_counter(interp);
            if let Some(st) = interp.tc_cache.as_mut() {
                if let Some(top) = st.stack.last_mut() {
                    top.digest.u64(arg_hash.unwrap_or(0));
                    let stream = match which {
                        Which::Tc => &mut top.tc,
                    };
                    stream.entries.push(Entry {
                        arg_hash,
                        val,
                        gensym_after,
                    });
                }
            }
            Ok(r)
        }
    }
}

/// Run `orig` with the per-stream re-entrancy depth bumped, so nested
/// self-calls inside it are passed through rather than recorded.
fn with_depth(
    interp: &mut Interp,
    which: Which,
    orig: fn(&mut Interp, &[Value]) -> ShenResult<Value>,
    args: &[Value],
) -> ShenResult<Value> {
    fn depth_mut(interp: &mut Interp, which: Which) -> Option<&mut u32> {
        let top = interp.tc_cache.as_mut()?.stack.last_mut()?;
        Some(match which {
            Which::Tc => &mut top.tc_depth,
        })
    }
    if let Some(d) = depth_mut(interp, which) {
        *d += 1;
    }
    let r = orig(interp, args);
    if let Some(d) = depth_mut(interp, which) {
        *d = d.saturating_sub(1);
    }
    r
}

pub fn typecheck_wrapper(interp: &mut Interp, args: &[Value]) -> ShenResult<Value> {
    stream_wrapper(interp, args, Which::Tc, original_typecheck)
}

// ------------------------------------------------------------ installation

/// Install the cache if `SHEN_RUST_TC_CACHE=<dir>` is set. Called at the
/// end of boot so the wrappers land over the kernel AOT registrations.
pub fn install_from_env(interp: &mut Interp, kernel_dir: &Path) {
    let Some(dir) = std::env::var_os("SHEN_RUST_TC_CACHE") else {
        return;
    };
    if dir.is_empty() {
        return;
    }
    install(
        interp,
        PathBuf::from(dir),
        std::env::var_os("SHEN_RUST_TC_CACHE_STATS").is_some(),
        kernel_dir,
    );
}

/// Install the cache unconditionally (the testable entry point —
/// `install_from_env` is the env-gated wrapper boot uses).
pub fn install(interp: &mut Interp, dir: PathBuf, stats_on: bool, kernel_dir: &Path) {
    if let Err(e) = fs::create_dir_all(&dir) {
        eprintln!("tc-cache: create {}: {e} — disabled", dir.display());
        return;
    }
    interp.tc_cache = Some(Box::new(TcCacheState {
        dir,
        chain: kernel_seed(kernel_dir),
        stack: Vec::new(),
        stats_on,
        hits: 0,
        misses: 0,
    }));
    // Both tables, native first then direct, mirroring the kernel AOT
    // installers — kernel code reaches these via apply_direct. (Note:
    // register_native does NOT clear the direct slot today; the immediate
    // register_aot_direct overwrite is what keeps the pair coherent here.
    // do_defun clears the slot on Shen-level redefinition.)
    interp.register_native("load", 1, load_wrapper);
    interp.register_aot_direct("load", load_wrapper);
    interp.register_native("shen.typecheck", 2, typecheck_wrapper);
    interp.register_aot_direct("shen.typecheck", typecheck_wrapper);
}

/// Seed the load chain with the kernel sources, so a kernel change (which
/// can change type-checker behaviour) invalidates every cached verdict.
/// Hashes every `.kl` file in the kernel dir, sorted by name. One-time
/// ~MBs of IO at install; only paid when the cache is enabled.
fn kernel_seed(kernel_dir: &Path) -> u64 {
    let mut h = Fnv::new();
    h.write(FORMAT.as_bytes());
    let mut files: Vec<PathBuf> = fs::read_dir(kernel_dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "kl"))
        .collect();
    files.sort();
    for f in &files {
        if let Some(name) = f.file_name().and_then(|n| n.to_str()) {
            h.write(name.as_bytes());
        }
        h.u64(fs::read(f).map(|b| fnv_bytes(&b)).unwrap_or(0));
    }
    h.finish()
}

/// `(hits, misses)` so far — positive evidence for tests/diagnostics that
/// replay actually engaged rather than silently re-recording.
pub fn stats(interp: &Interp) -> Option<(u64, u64)> {
    interp.tc_cache.as_ref().map(|st| (st.hits, st.misses))
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

    #[test]
    fn recording_roundtrips_through_cache_file() {
        let mut interp = Interp::new();
        let ty = sample_type(&mut interp);
        let dir = std::env::temp_dir().join(format!("shen-tc-unit-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = cache_path(&dir, 0xdead_beef);
        let ctx = LoadCtx {
            key: 0xdead_beef,
            file_hash: 1,
            gensym_start: 41,
            gensym_end_recorded: 0,
            replay: false,
            poisoned: false,
            digest: Fnv::new(),
            tc: Stream {
                entries: vec![
                    Entry {
                        arg_hash: Some(7),
                        val: Some(ty.clone()),
                        gensym_after: 55,
                    },
                    Entry {
                        arg_hash: None,
                        val: None,
                        gensym_after: 56,
                    },
                ],
                cursor: 0,
            },
            ckpt: vec![0xabc, 0xdef],
            ckpt_cursor: 0,
            tc_depth: 0,
        };
        write_cache(&interp, &path, &ctx, 99);
        let rec = read_cache(&mut interp, &path).expect("read back");
        assert_eq!(rec.gensym_start, 41);
        assert_eq!(rec.gensym_end, 99);
        assert_eq!(rec.tc.len(), 2);
        assert_eq!(rec.ckpt, vec![0xabc, 0xdef]);
        assert_eq!(rec.tc[0].arg_hash, Some(7));
        assert!(crate::value::shen_eq(rec.tc[0].val.as_ref().unwrap(), &ty));
        assert_eq!(rec.tc[1].arg_hash, None);
        assert!(rec.tc[1].val.is_none());
        assert_eq!(rec.tc[0].gensym_after, 55);
        assert_eq!(rec.tc[1].gensym_after, 56);
        let _ = fs::remove_dir_all(&dir);
    }
}
