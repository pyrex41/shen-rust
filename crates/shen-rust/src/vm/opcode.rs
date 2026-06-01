//! Opcode set for the bytecode VM.
//!
//! Roughly mirrors shen-go's instruction set (`../shen-go/kl/vm.go:10–34`)
//! with Shen-specific additions. Operands are encoded inline in the enum
//! variant; the enum is a flat tag + payload, dispatched by `match` in
//! the exec loop.
//!
//! Naming convention: opcodes that consume their argument list from the
//! operand stack name the count in their operand (`Call(n)` etc.).

/// A single bytecode instruction.
///
/// Variants carry their inline operands. The enum is `Copy + Clone` so
/// the exec loop can read a single byte/word at a time without touching
/// the heap. Jump offsets are signed PC-relative; positive moves forward
/// from the instruction after the jump.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    // ---- Frame / stack -------------------------------------------------
    /// Push `consts[idx]` onto the stack.
    LoadConst(u16),
    /// Push `locals[slot]` onto the stack.
    LoadLocal(u16),
    /// Pop the top of stack into `locals[slot]`.
    StoreLocal(u16),
    /// Push the closure's upvalue at `idx` onto the stack.
    LoadUpval(u16),
    /// Resolve `consts[idx]` (which must be a `Value::Sym`) through the
    /// function namespace and push the resulting `Value::Closure`. Used
    /// for the head position of a call to a globally-bound function.
    LoadGlobal(u16),
    /// Discard the top of stack.
    Pop,

    // ---- Control flow --------------------------------------------------
    /// Unconditional PC-relative jump (signed delta applied after the
    /// post-increment from reading this opcode).
    Jump(i16),
    /// Pop a value; if it is `Value::bool(false)` or the symbol `false`,
    /// jump by `delta`. Otherwise fall through. Non-boolean produces an
    /// error.
    JumpFalse(i16),
    /// Return the top of stack from the current function (or `Nil` if
    /// the stack is empty).
    Return,

    // ---- Calls ---------------------------------------------------------
    /// Pop `n` args + the callee from the stack (callee under the args),
    /// invoke, push the result. Non-tail position.
    Call(u8),
    /// Like `Call(n)` but in tail position — yields control to the outer
    /// trampoline with a sentinel; avoids growing the Rust stack on
    /// mutual recursion.
    TailCall(u8),
    /// Self-recursive tail call: copy the top `n` args into
    /// `locals[0..n]` and reset `pc = 0`. No trampoline involved, no
    /// stack growth.
    SelfTailCall(u8),

    // ---- Closures ------------------------------------------------------
    /// Pop `n_upvals` values from the stack, package them with
    /// `consts[fn_idx]` (a `BytecodeFn` constant) into a new closure
    /// value, push the closure.
    MakeClosure { fn_idx: u16, n_upvals: u8 },

    // ---- Inlined primitives -------------------------------------------
    // Mirror klcompile's `inlinable()` table: when the compiler sees a
    // bare reference to one of these primitives (not shadowed by a
    // local), it emits the dedicated opcode instead of
    // `LoadGlobal + args + Call(n)`. Semantics are identical to the
    // matching `aot::runtime::*` helpers (`rt::add`, `rt::hd`, etc.).
    /// Pop b, a; push `a + b`. Int+Int with overflow → Float.
    Add,
    /// Pop b, a; push `a - b`.
    Sub,
    /// Pop b, a; push `a * b`.
    Mul,
    /// Pop b, a; push `a / b`. Int/Int yielding integer result stays Int.
    Div,
    /// Pop b, a; push `Bool(a < b)`. Errors on NaN.
    Lt,
    /// Pop b, a; push `Bool(a <= b)`.
    Le,
    /// Pop b, a; push `Bool(a > b)`.
    Gt,
    /// Pop b, a; push `Bool(a >= b)`.
    Ge,
    /// Pop b, a; push `Bool(shen_eq(a, b))`. Infallible.
    Eq,
    /// Pop b, a; push `Cons(a, b)`. Infallible.
    Cons,
    /// Pop a; push `head(a)`. Errors if a is not a cons.
    Hd,
    /// Pop a; push `tail(a)`. Errors if a is not a cons.
    Tl,
    /// Pop a; push `Bool(is_cons(a))`. Infallible.
    IsCons,
    /// Pop a; push `Bool(is_number(a))`.
    IsNumber,
    /// Pop a; push `Bool(is_string(a))`.
    IsString,
    /// Pop a; push `Bool(is_symbol(a))`.
    IsSymbol,
    /// Pop a; push `Bool(is_absvector(a))`. `vector?` aliases here too.
    IsAbsvector,
}
