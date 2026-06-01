//! KL abstract syntax tree.
//!
//! KL is the s-expression intermediate language Shen kernels compile down
//! to. There are only a handful of distinct expression shapes; everything
//! else is dispatched by the head symbol at eval time (matching the
//! `shen-ocaml` approach where special forms are recognized by name in
//! `eval_app`, not by AST tag).

use std::rc::Rc;

use crate::symbol::SymId;

/// A KL expression. `App` uses `Rc<[KlExpr]>` so the trampoline can keep an
/// owned `KlExpr` it advances across tail calls without re-cloning the
/// child slice on every iteration.
#[derive(Clone, Debug)]
pub enum KlExpr {
    /// `()` — empty list literal.
    Nil,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(Rc<str>),
    /// A symbol reference. Whether it resolves to a function, a global, or
    /// a local is decided at eval time.
    Sym(SymId),
    /// A function/operator application or a special form. Element 0 is the
    /// head; element 1.. are arguments. Special forms (`defun`, `lambda`,
    /// `let`, `if`, etc.) are dispatched on the head `Sym(...)` in `eval`.
    App(Rc<[KlExpr]>),
}

impl KlExpr {
    /// Build a list-form expression from a head symbol and arguments.
    pub fn app(items: Vec<KlExpr>) -> Self {
        KlExpr::App(items.into())
    }
}
