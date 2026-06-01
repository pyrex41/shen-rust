//! Compiled function representation.

use std::rc::Rc;

use crate::symbol::SymId;
use crate::value::Value;
use crate::vm::opcode::Op;

/// A compiled Shen function ready for the VM. Mirrors shen-go's
/// `BytecodeFunc` (`../shen-go/kl/types.go:44–50`).
///
/// - `arity` is the number of formal parameters; on entry the VM places
///   the args in `locals[0..arity)`.
/// - `n_locals` is the total slot count; `n_locals - arity` slots are
///   used by `let` bindings and intermediate temporaries (allocated
///   sequentially by the compiler).
/// - `code` is the linear instruction stream.
/// - `consts` is the constant pool: numbers, strings, symbol values.
/// - `fn_consts` is a sidecar pool of nested compiled functions,
///   referenced by `MakeClosure { fn_idx, .. }`. Separated from
///   `consts` so the `Value` type doesn't need a `BytecodeFn` variant.
#[derive(Debug, Clone)]
pub struct BytecodeFn {
    pub name: Option<SymId>,
    pub arity: usize,
    pub n_locals: usize,
    pub code: Vec<Op>,
    pub consts: Vec<Value>,
    pub fn_consts: Vec<Rc<BytecodeFn>>,
}
