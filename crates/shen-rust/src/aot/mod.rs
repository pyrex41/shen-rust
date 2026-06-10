//! AOT-compiled KL.
//!
//! `runtime` provides the small surface that compiled functions call
//! into: `apply_named` for env-routed calls, `is_truthy` for Shen
//! boolean dispatch, `make_aot_closure` for compiled lambdas.
//!
//! `generated` holds the output of `klcompile`. Regenerate with
//! `cargo run -p klcompile -- INPUT.kl crates/shen-rust/src/aot/generated.rs`.
//! The committed file is for the smoke-test inputs in
//! `tests/aot_smoke.rs`; production use would target the full kernel.

pub mod generated;
pub mod kernel;
pub mod overlay;
pub mod runtime;
