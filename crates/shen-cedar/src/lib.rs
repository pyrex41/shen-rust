//! shen-cedar — Shen language port hosted in Rust with AWS Cedar integration.

// GC Step 3: `Value` is now a `Copy` tagged word, so the ~20k `.clone()` calls
// the AOT generator (klcompile) emits on `Value`s — and the handful in
// hand-written code — are no-op copies that trip `clippy::clone_on_copy`. They
// are harmless (cloning a `Copy` type can never change behavior); allowing the
// lint crate-wide lands the flip without a 20k-site codemod that the next AOT
// regen would undo anyway. A future cleanup can teach klcompile to stop
// emitting `.clone()` on `Value`. See `design/gc-step3-value-flip-handoff.md` §4e.
#![allow(clippy::clone_on_copy)]

pub mod aot;
pub mod cedar;
pub mod env;
pub mod error;
pub mod gc;
pub mod generated;
pub mod interp;
pub mod kl;
pub mod primitives;
pub mod symbol;
pub mod value;
pub mod vm;
