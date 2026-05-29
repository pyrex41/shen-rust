//! Cons-cell representation (A/B baseline: reference-counted `Rc`).
//!
//! `Value::Cons` holds a [`ConsCell`] so the cons allocation strategy lives in
//! one place. This variant is the normal `Rc<(Value, Value)>`: full refcount
//! on clone, recursive drop + `free` on the last reference. It is the churn
//! baseline against which the leaked-bump-arena spike is measured. See
//! `design/value-representation.md`.

use std::ops::Deref;
use std::rc::Rc;

use crate::value::Value;

/// A reference-counted cons cell: head and tail in a single shared allocation.
/// Derefs to `(Value, Value)` so `.0` (head) / `.1` (tail) access works.
#[derive(Clone)]
pub struct ConsCell(Rc<(Value, Value)>);

impl ConsCell {
    #[inline]
    pub fn new(head: Value, tail: Value) -> Self {
        ConsCell(Rc::new((head, tail)))
    }

    #[inline]
    pub fn ptr_eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }
}

impl Deref for ConsCell {
    type Target = (Value, Value);
    #[inline]
    fn deref(&self) -> &(Value, Value) {
        &self.0
    }
}
