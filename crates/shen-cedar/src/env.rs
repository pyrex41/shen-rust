//! Dual-namespace environment.
//!
//! Shen and KL keep function bindings (`defun`, `define`) and global value
//! bindings (`set`, `value`) in separate tables — see the porting guide's
//! "Compiling into Single Namespace Languages" section.
//!
//! Function and global tables are `Vec`s indexed directly by `SymId`.
//! `SymId`s are dense, sequential `u32`s minted by the interner, so a slot
//! lookup is a branchless O(1) index with no hashing — shen-go's
//! "direct slot" dispatch. This removes the per-call `HashMap<SymId, _>`
//! probe that the profile showed as ~8% of CPU (SipHash on the hot path).
//!
//! Property-list metadata (used by `put`/`get`) stays in a `HashMap` keyed
//! by `(SymId, SymId)` — that key space is sparse, so a map is still the
//! right choice.

use std::collections::HashMap;

use crate::symbol::SymId;
use crate::value::Value;

#[derive(Default)]
pub struct Env {
    functions: Vec<Option<Value>>,
    globals: Vec<Option<Value>>,
    pub properties: HashMap<(SymId, SymId), Value>,
}

impl Env {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get_fn(&self, name: SymId) -> Option<&Value> {
        self.functions.get(name.0 as usize)?.as_ref()
    }

    pub fn set_fn(&mut self, name: SymId, value: Value) {
        Self::set_slot(&mut self.functions, name, value);
    }

    pub fn get_global(&self, name: SymId) -> Option<&Value> {
        self.globals.get(name.0 as usize)?.as_ref()
    }

    pub fn set_global(&mut self, name: SymId, value: Value) {
        Self::set_slot(&mut self.globals, name, value);
    }

    /// Write `value` into the slot for `name`, growing the table to fit.
    fn set_slot(table: &mut Vec<Option<Value>>, name: SymId, value: Value) {
        let idx = name.0 as usize;
        if idx >= table.len() {
            table.resize(idx + 1, None);
        }
        table[idx] = Some(value);
    }

    pub fn get_property(&self, target: SymId, key: SymId) -> Option<&Value> {
        self.properties.get(&(target, key))
    }

    pub fn set_property(&mut self, target: SymId, key: SymId, value: Value) {
        self.properties.insert((target, key), value);
    }
}
