//! Symbol interner.
//!
//! Shen symbols are first-class values, but their identity is the string
//! name. Comparing strings on every equality check costs more than O(1)
//! integer comparison, so we intern names into a stable `SymId` and use the
//! id everywhere in the runtime.
//!
//! `SymId` is `Copy` and `Eq`. Resolution to the string name goes through
//! `Interner::resolve`.

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};

/// Stable integer handle for a symbol. Two `SymId`s are equal iff the
/// underlying symbol names are equal (modulo interning).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SymId(pub u32);

/// FNV-1a hasher. `intern` is called on every AOT call site (to resolve a
/// callee name to its `SymId`), and the default `SipHasher` — cryptographic
/// and tuned for DoS-resistance — dominated the profile hashing short
/// symbol names. FNV-1a is far cheaper for the ~10–20 byte names we hash
/// here; collisions only cost a `memcmp`, and the interner isn't exposed to
/// adversarial input. The assigned `SymId` is insertion-order, so the hash
/// choice never changes which id a name gets.
struct FnvHasher(u64);

impl Default for FnvHasher {
    #[inline]
    fn default() -> Self {
        FnvHasher(0xcbf2_9ce4_8422_2325)
    }
}

impl Hasher for FnvHasher {
    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }

    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let mut h = self.0;
        for &b in bytes {
            h ^= b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        self.0 = h;
    }
}

type BuildFnv = BuildHasherDefault<FnvHasher>;

/// String <-> id interner. Single-threaded; one per `Interp`.
#[derive(Debug, Default)]
pub struct Interner {
    names: Vec<String>,
    by_name: HashMap<String, SymId, BuildFnv>,
    /// Pointer-keyed cache for `&'static str` call targets emitted by the
    /// AOT compiler. Every AOT call site re-resolves its callee name; since
    /// those are string literals with stable addresses, caching by pointer
    /// turns a multi-byte string hash + `memcmp` into a single-word hash +
    /// integer compare. Per-`Interner` (never process-global), so it stays
    /// correct even when several interpreters with different id assignments
    /// coexist.
    by_ptr: HashMap<usize, SymId, BuildFnv>,
}

impl Interner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Intern a name. Idempotent.
    pub fn intern(&mut self, name: &str) -> SymId {
        if let Some(&id) = self.by_name.get(name) {
            return id;
        }
        let id = SymId(self.names.len() as u32);
        self.names.push(name.to_owned());
        self.by_name.insert(name.to_owned(), id);
        id
    }

    /// Intern a `&'static str` (an AOT call-target literal), caching the
    /// result by the string's address. The first call per literal pays the
    /// normal string intern; every subsequent call is a single-word lookup.
    #[inline]
    pub fn intern_static(&mut self, name: &'static str) -> SymId {
        let key = name.as_ptr() as usize;
        if let Some(&id) = self.by_ptr.get(&key) {
            return id;
        }
        let id = self.intern(name);
        self.by_ptr.insert(key, id);
        id
    }

    /// Resolve an id to its name. Panics if `id` was minted by a different
    /// interner — that's a logic bug, not a user error.
    pub fn resolve(&self, id: SymId) -> &str {
        &self.names[id.0 as usize]
    }

    pub fn len(&self) -> usize {
        self.names.len()
    }

    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intern_idempotent() {
        let mut i = Interner::new();
        let a1 = i.intern("foo");
        let a2 = i.intern("foo");
        let b = i.intern("bar");
        assert_eq!(a1, a2);
        assert_ne!(a1, b);
        assert_eq!(i.resolve(a1), "foo");
        assert_eq!(i.resolve(b), "bar");
    }

    #[test]
    fn intern_handles_shen_specials() {
        // Shen symbols can contain characters that aren't valid Rust idents.
        let mut i = Interner::new();
        let a = i.intern("shen.<rule>");
        let b = i.intern("element?");
        let c = i.intern("@p");
        assert_eq!(i.resolve(a), "shen.<rule>");
        assert_eq!(i.resolve(b), "element?");
        assert_eq!(i.resolve(c), "@p");
    }
}
