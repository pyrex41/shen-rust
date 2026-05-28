//! Bytecode VM for user-defined Shen functions.
//!
//! The tree-walker (`interp::eval`) and the AOT compiler (`klcompile`)
//! together cover the kernel KL files (compiled at build time) and any
//! bootstrap interpretation that happens before AOT install. But the
//! dominant kernel-tests workload — the Shen type-checker proving
//! theorems about test files loaded at runtime — runs through
//! tree-walked Lambda closures whose bodies are built at `defun` /
//! `lambda` / `freeze` evaluation time. That's the hot loop.
//!
//! The bytecode VM compiles those runtime-defined closures to a flat
//! stream of opcodes when they're created, then executes them with
//! integer-indexed local slots (instead of an alist scanned linearly)
//! and a static jump table for control flow (instead of pattern-match
//! recursion over `KlExpr` nodes). shen-go's design doc reports 4–8×
//! from the VM alone vs a tree-walker on the same kernel.
//!
//! Module layout:
//! - [`opcode`]  – `Op` and `Instr` definitions.
//! - [`bytecode`] – `BytecodeFn` (the compiled function representation).
//! - [`exec`]    – the dispatch loop (`vm::exec`).
//!
//! B1 (current): opcodes, `BytecodeFn`, exec for literals,
//! `LoadLocal`/`StoreLocal`, `Return`, `Call(n)`/`TailCall(n)`. Compiler
//! and integration land in later phases.

pub mod bytecode;
pub mod compiler;
pub mod exec;
pub mod opcode;

pub use bytecode::BytecodeFn;
pub use compiler::compile_fn;
pub use exec::exec;
pub use opcode::Op;
