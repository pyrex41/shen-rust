//! Tree-walking KL evaluator with a trampoline for tail positions.
//!
//! `Interp` owns the symbol table, the dual-namespace `Env`, and the
//! interned IDs of every special form. Native primitives are registered
//! via `register_native`.
//!
//! Evaluation semantics:
//!
//! * Atoms (numbers, bools, strings, nil) evaluate to themselves.
//! * A bare symbol is looked up in the *lexical locals*. If unbound, the
//!   symbol value itself is returned (Shen's "innocent symbol" semantics).
//! * `(head a b c)` with `head` a `Sym` dispatches:
//!   1. Special forms (`if`, `let`, `defun`, etc.) handled inline.
//!   2. Otherwise, `head` is looked up in `env.functions` and applied.
//! * `(expr a b c)` with `expr` not a `Sym` evaluates `expr` to a value
//!   (must be a closure) and applies it.
//!
//! Tail-position re-entry is handled by mutating `current` and `locals`
//! and `continue`-ing the loop — this avoids growing the Rust stack on
//! tail calls, even for deeply self-recursive functions like
//! `shen.shen->kl-h`.

use std::rc::Rc;
use std::time::Instant;

use smallvec::SmallVec;

use crate::env::Env;
use crate::error::{ShenError, ShenResult};
use crate::kl::ast::KlExpr;
use crate::symbol::{Interner, SymId};
use crate::value::{Closure, ClosureKind, LambdaBody, NativeFn, Value};

/// Local lexical environment. A flat vector treated as a stack — innermost
/// binding wins on lookup, achieved by scanning from the end.
pub type Locals = Vec<(SymId, Value)>;

/// Transient argument vector for calls. Uses inline storage for the
/// overwhelmingly common case of ≤4 arguments (avoids a heap allocation
/// per call in the tree-walker and AOT fallback paths).
pub(crate) type ArgVec = SmallVec<[Value; 4]>;

/// Raw function pointer used in the AOT direct-call table.
pub(crate) type DirectFn = fn(&mut Interp, &[Value]) -> ShenResult<Value>;

/// Trampoline-local lexical scope.
///
/// Starts as a borrow of the caller's locals and is only promoted to an
/// owned `Vec` when a binding form (`let`) extends it or a tail call
/// installs a fresh frame. This keeps the common case — evaluating
/// atomic/symbol arguments under an unchanged scope — allocation- and
/// clone-free, which is where the old `eval_in(.., locals.clone())`
/// per-argument copy burned most of its time.
enum Scope<'a> {
    Borrowed(&'a [(SymId, Value)]),
    Owned(Locals),
}

impl Scope<'_> {
    #[inline]
    fn slice(&self) -> &[(SymId, Value)] {
        match self {
            Scope::Borrowed(s) => s,
            Scope::Owned(v) => v,
        }
    }

    /// Get the owned buffer, cloning the borrowed base on first mutation.
    #[inline]
    fn make_mut(&mut self) -> &mut Locals {
        if let Scope::Borrowed(s) = self {
            *self = Scope::Owned(s.to_vec());
        }
        match self {
            Scope::Owned(v) => v,
            Scope::Borrowed(_) => unreachable!(),
        }
    }

    /// Install a fresh frame (`captured` + `params`→`args`) for a tail
    /// call. Reuses the existing owned allocation when we already own one
    /// — the hot self-recursive tail-loop path allocates nothing per
    /// iteration.
    #[inline]
    fn enter_frame(&mut self, captured: &[(SymId, Value)], params: &[SymId], args: ArgVec) {
        let buf = match self {
            Scope::Owned(v) => {
                v.clear();
                v
            }
            Scope::Borrowed(_) => {
                *self = Scope::Owned(Vec::with_capacity(captured.len() + params.len()));
                match self {
                    Scope::Owned(v) => v,
                    Scope::Borrowed(_) => unreachable!(),
                }
            }
        };
        buf.extend(captured.iter().cloned());
        buf.extend(params.iter().copied().zip(args));
    }
}

pub struct Interp {
    pub symbols: Interner,
    pub env: Env,
    pub well_known: WellKnown,
    /// Direct-call table for AOT-compiled (and other registered native)
    /// functions. Indexed by SymId exactly like Env::functions.
    /// Populated for every name that goes through register_native / the
    /// AOT installers. Enables the fast path in rt::apply_direct.
    aot_direct: Vec<Option<DirectFn>>,
    /// Remaining instruction-count budget for the current evaluation.
    /// `None` = unbounded (the default). When `Some(0)`, the next
    /// reduction step raises `ShenError::cancelled`. Exhaustion is
    /// *sticky*: it stays at `Some(0)` until `clear_budget`, so a budget
    /// abort that is (incorrectly) caught by an error handler re-fires on
    /// the handler's very first step rather than letting it make progress.
    /// Set via [`Interp::set_budget`]; checked in both reduction engines
    /// (`eval_in`'s trampoline and `vm::exec`).
    remaining_steps: Option<u64>,
    /// Optional wall-clock deadline, checked every ~1024 steps so real
    /// time is bounded even when a step is individually cheap. `None` =
    /// no deadline. Set via [`Interp::set_deadline`].
    deadline: Option<Instant>,
    /// Free-running step counter used only to throttle the `deadline`
    /// check (so we don't call `Instant::now()` on every reduction).
    deadline_counter: u64,
}

/// Pre-interned ids for KL special forms and a few hot symbols.
#[derive(Debug)]
pub struct WellKnown {
    pub k_true: SymId,
    pub k_false: SymId,
    pub k_nil: SymId,
    pub k_defun: SymId,
    pub k_lambda: SymId,
    pub k_let: SymId,
    pub k_if: SymId,
    pub k_cond: SymId,
    pub k_freeze: SymId,
    pub k_thaw: SymId,
    pub k_trap_error: SymId,
    pub k_do: SymId,
    pub k_and: SymId,
    pub k_or: SymId,
    pub k_quote: SymId,
    pub k_type: SymId,
    /// Pre-interned `shen.pvar` and `shen.-null-` so the hot-path
    /// `shen.pvar?` / `shen.lazyderef` native overrides don't pay a
    /// HashMap probe per call.
    pub k_shen_pvar: SymId,
    pub k_shen_null: SymId,
    /// `shen.fail!` (return value of `(fail)`).
    pub k_shen_fail: SymId,
}

impl WellKnown {
    fn intern(interner: &mut Interner) -> Self {
        Self {
            k_true: interner.intern("true"),
            k_false: interner.intern("false"),
            k_nil: interner.intern("nil"),
            k_defun: interner.intern("defun"),
            k_lambda: interner.intern("lambda"),
            k_let: interner.intern("let"),
            k_if: interner.intern("if"),
            k_cond: interner.intern("cond"),
            k_freeze: interner.intern("freeze"),
            k_thaw: interner.intern("thaw"),
            k_trap_error: interner.intern("trap-error"),
            k_do: interner.intern("do"),
            k_and: interner.intern("and"),
            k_or: interner.intern("or"),
            k_quote: interner.intern("quote"),
            k_type: interner.intern("type"),
            k_shen_pvar: interner.intern("shen.pvar"),
            k_shen_null: interner.intern("shen.-null-"),
            k_shen_fail: interner.intern("shen.fail!"),
        }
    }
}

impl Default for Interp {
    fn default() -> Self {
        Self::new()
    }
}

impl Interp {
    pub fn new() -> Self {
        let mut symbols = Interner::new();
        let well_known = WellKnown::intern(&mut symbols);
        // Publish the true/false SymIds so the no-interpreter `shen_eq`
        // can cross-equate `Bool(true)` with `Sym("true")`. The kernel
        // reader interns the literals as symbols, while our KL parser
        // produces `Bool` — without this, `(= true (boolean? X))` etc.
        // fail to unify and the `(tc +)` type-checker rejects valid
        // boolean expressions.
        crate::value::set_boolean_sym_ids(well_known.k_true, well_known.k_false);
        let mut interp = Self {
            symbols,
            env: Env::new(),
            well_known,
            aot_direct: Vec::new(),
            remaining_steps: None,
            deadline: None,
            deadline_counter: 0,
        };
        crate::primitives::register_all(&mut interp);
        interp
    }

    pub fn intern(&mut self, name: &str) -> SymId {
        self.symbols.intern(name)
    }

    /// Resolve a `&'static str` call-target literal to its `SymId`, using
    /// the interner's pointer cache. Used by AOT-emitted call sites.
    #[inline]
    pub fn intern_static(&mut self, name: &'static str) -> SymId {
        self.symbols.intern_static(name)
    }

    pub fn resolve(&self, id: SymId) -> &str {
        self.symbols.resolve(id)
    }

    /// Cap the current evaluation at `steps` reduction steps. When the
    /// budget is exhausted, the active `eval`/`eval_in`/`vm::exec` returns
    /// `Err(ShenError::cancelled(..))` — distinguishable from an ordinary
    /// Shen error via [`ShenError::is_cancelled`], and propagated past
    /// `trap-error` (see `step_trap_error`). Budgeting is per-`Interp` and
    /// shared across nested `eval_in` re-entries, so it bounds the whole
    /// evaluation, not just one frame. Call [`Interp::clear_budget`] (or
    /// `set_budget` again) before the next evaluation.
    pub fn set_budget(&mut self, steps: u64) {
        self.remaining_steps = Some(steps);
    }

    /// Bound the current evaluation by wall-clock time. The deadline is
    /// checked roughly every 1024 reduction steps. Combine with
    /// [`Interp::set_budget`] for both a step and a time ceiling.
    pub fn set_deadline(&mut self, deadline: Instant) {
        self.deadline = Some(deadline);
    }

    /// Remove any step budget and wall-clock deadline, restoring unbounded
    /// evaluation. Call between ticks so a prior cancellation doesn't leak
    /// into the next evaluation.
    pub fn clear_budget(&mut self) {
        self.remaining_steps = None;
        self.deadline = None;
        self.deadline_counter = 0;
    }

    /// Charge one reduction step against the budget/deadline. Returns
    /// `Err(cancelled)` once exhausted. Hot path: when no budget is set
    /// (the default), this is two never-taken branches and is free.
    ///
    /// Exhaustion is sticky — `remaining_steps` parks at `Some(0)` so a
    /// cancellation that slips past a `trap-error` handler re-fires on the
    /// handler's first step instead of letting evaluation resume.
    #[inline]
    pub(crate) fn charge_step(&mut self) -> ShenResult<()> {
        if let Some(n) = self.remaining_steps {
            if n == 0 {
                return Err(ShenError::cancelled("cancelled: step budget exhausted"));
            }
            self.remaining_steps = Some(n - 1);
        }
        if let Some(deadline) = self.deadline {
            self.deadline_counter = self.deadline_counter.wrapping_add(1);
            if self.deadline_counter & 1023 == 0 && Instant::now() >= deadline {
                // Park the step budget at zero so the cancellation stays
                // sticky even if the caller set no step budget.
                self.remaining_steps = Some(0);
                return Err(ShenError::cancelled(
                    "cancelled: wall-clock deadline exceeded",
                ));
            }
        }
        Ok(())
    }

    pub fn register_native<F>(&mut self, name: &str, arity: usize, f: F)
    where
        F: Fn(&mut Interp, &[Value]) -> ShenResult<Value> + 'static,
    {
        let sym = self.intern(name);
        let closure = Closure {
            name: Some(sym),
            arity,
            partial: Vec::new(),
            kind: ClosureKind::Native(Rc::new(f) as Rc<NativeFn>),
        };
        self.env.set_fn(sym, Value::Closure(Rc::new(closure)));
        // Grow direct table in lockstep (slot left empty for closure-based
        // natives; real fn pointers are installed via register_aot_direct).
        let idx = sym.0 as usize;
        if idx >= self.aot_direct.len() {
            self.aot_direct.resize(idx + 1, None);
        }
    }

    /// Register a raw fn pointer for the ultra-fast AOT (and hot native)
    /// direct call path. Called from generated AOT installers in addition
    /// to the normal register_native so that apply_direct can bypass the
    /// Closure Rc + kind matching entirely.
    pub fn register_aot_direct(&mut self, name: &str, f: DirectFn) {
        let sym = self.intern(name);
        let idx = sym.0 as usize;
        if idx >= self.aot_direct.len() {
            self.aot_direct.resize(idx + 1, None);
        }
        self.aot_direct[idx] = Some(f);
    }

    /// Look up a direct fn pointer by SymId (used by rt::apply_direct).
    #[inline]
    pub fn get_aot_direct(&self, sym: SymId) -> Option<DirectFn> {
        self.aot_direct.get(sym.0 as usize).copied().flatten()
    }

    /// Top-level entry. Evaluates a KL expression with no lexical locals.
    pub fn eval(&mut self, expr: &KlExpr) -> ShenResult<Value> {
        self.eval_in(expr, &[])
    }

    /// Evaluate under an explicit lexical environment.
    ///
    /// `locals` is borrowed: non-`App` forms return immediately without
    /// touching it, and only `let`/lambda/tail-call promote the scope to
    /// an owned buffer (see [`Scope`]).
    pub fn eval_in(&mut self, expr: &KlExpr, locals: &[(SymId, Value)]) -> ShenResult<Value> {
        // Non-`App` forms never trampoline and never need an owned scope.
        match expr {
            KlExpr::Nil => return Ok(Value::Nil),
            KlExpr::Bool(b) => return Ok(Value::Bool(*b)),
            KlExpr::Int(n) => return Ok(Value::Int(*n)),
            KlExpr::Float(x) => return Ok(Value::Float(*x)),
            KlExpr::Str(s) => return Ok(Value::Str(s.clone())),
            KlExpr::Sym(s) => return Ok(self.eval_symbol(*s, locals)),
            KlExpr::App(_) => {}
        }

        let mut current: KlExpr = expr.clone();
        let mut scope = Scope::Borrowed(locals);
        loop {
            match &current {
                KlExpr::Nil => return Ok(Value::Nil),
                KlExpr::Bool(b) => return Ok(Value::Bool(*b)),
                KlExpr::Int(n) => return Ok(Value::Int(*n)),
                KlExpr::Float(x) => return Ok(Value::Float(*x)),
                KlExpr::Str(s) => return Ok(Value::Str(s.clone())),
                KlExpr::Sym(s) => return Ok(self.eval_symbol(*s, scope.slice())),
                KlExpr::App(items) => {
                    if items.is_empty() {
                        return Ok(Value::Nil);
                    }
                    self.charge_step()?;
                    let items = items.clone();
                    match self.step(&items, &mut scope, &mut current)? {
                        StepOutcome::Done(v) => return Ok(v),
                        StepOutcome::Continue => continue,
                    }
                }
            }
        }
    }

    /// Look up a symbol in the lexical environment; if unbound, the symbol
    /// value itself is returned (innocent-symbol semantics).
    fn eval_symbol(&self, sym: SymId, locals: &[(SymId, Value)]) -> Value {
        self.lookup_local(sym, locals).unwrap_or(Value::Sym(sym))
    }

    fn lookup_local(&self, sym: SymId, locals: &[(SymId, Value)]) -> Option<Value> {
        locals
            .iter()
            .rev()
            .find(|(s, _)| *s == sym)
            .map(|(_, v)| v.clone())
    }

    /// Process one `App` form. Either produce a final value (`Done`) or
    /// signal that `current` and `locals` have been rewritten to the next
    /// position to evaluate (`Continue`).
    fn step(
        &mut self,
        items: &Rc<[KlExpr]>,
        scope: &mut Scope,
        current: &mut KlExpr,
    ) -> ShenResult<StepOutcome> {
        let head = &items[0];
        let args = &items[1..];

        // Special-form dispatch is keyed on the head symbol id.
        if let KlExpr::Sym(sym) = head {
            let sym = *sym;
            let wk = &self.well_known;

            if sym == wk.k_if {
                return self.step_if(args, scope, current);
            }
            if sym == wk.k_let {
                return self.step_let(args, scope, current);
            }
            if sym == wk.k_lambda {
                return Ok(StepOutcome::Done(self.build_lambda(args, scope.slice())?));
            }
            if sym == wk.k_defun {
                return Ok(StepOutcome::Done(self.do_defun(args)?));
            }
            if sym == wk.k_cond {
                return self.step_cond(args, scope, current);
            }
            if sym == wk.k_do {
                return self.step_do(args, scope, current);
            }
            if sym == wk.k_and {
                return self.step_and(args, scope, current);
            }
            if sym == wk.k_or {
                return self.step_or(args, scope, current);
            }
            if sym == wk.k_freeze {
                return Ok(StepOutcome::Done(self.build_freeze(args, scope.slice())?));
            }
            if sym == wk.k_thaw {
                return self.step_thaw(args, scope);
            }
            if sym == wk.k_trap_error {
                return self.step_trap_error(args, scope);
            }
            if sym == wk.k_quote {
                if args.len() != 1 {
                    return Err(ShenError::new("quote: expected 1 arg"));
                }
                return Ok(StepOutcome::Done(self.quote_value(&args[0])));
            }
            if sym == wk.k_type {
                // `(type X T)` — type annotation, evaluate X.
                if args.is_empty() {
                    return Err(ShenError::new("type: expected at least 1 arg"));
                }
                *current = args[0].clone();
                return Ok(StepOutcome::Continue);
            }

            // Plain function call. Locals shadow the function namespace
            // (a lambda parameter named `F` should be callable as
            // `(F x y)` from the body).
            let f = self
                .lookup_local(sym, scope.slice())
                .or_else(|| self.env.get_fn(sym).cloned())
                .ok_or_else(|| {
                    ShenError::new(format!("undefined function: {}", self.resolve(sym)))
                })?;
            let argv = self.eval_args(args, scope.slice())?;
            return self.tail_apply(f, argv, scope, current);
        }

        // Head is an expression that evaluates to a closure.
        let f = self.eval_in(head, scope.slice())?;
        let argv = self.eval_args(args, scope.slice())?;
        self.tail_apply(f, argv, scope, current)
    }

    fn eval_args(&mut self, args: &[KlExpr], locals: &[(SymId, Value)]) -> ShenResult<ArgVec> {
        let mut out = ArgVec::with_capacity(args.len());
        for a in args {
            out.push(self.eval_in(a, locals)?);
        }
        Ok(out)
    }

    /// Apply `f` to `argv` in tail position. For user lambdas the loop is
    /// continued by rewriting `current` and `locals`; for natives the
    /// result is `Done`.
    fn tail_apply(
        &mut self,
        f: Value,
        argv: ArgVec,
        scope: &mut Scope,
        current: &mut KlExpr,
    ) -> ShenResult<StepOutcome> {
        let closure = match f {
            Value::Closure(c) => c,
            other => return Err(ShenError::new(format!("not callable: {other:?}"))),
        };

        // Combine partial-application args with the new ones. The common
        // case has no partial args, so `argv` is already the full vector —
        // move it through instead of re-collecting (the per-call `total`
        // Vec was the single hottest allocation in the profile).
        let mut total_args: ArgVec = if closure.partial.is_empty() {
            argv
        } else {
            closure.partial.iter().cloned().chain(argv).collect()
        };

        if total_args.len() < closure.arity {
            // Under-application: build a new partial closure.
            let new = Closure {
                name: closure.name,
                arity: closure.arity,
                partial: total_args.into_vec(), // keep the rare partial path on Vec for API compat
                kind: clone_kind(&closure.kind),
            };
            return Ok(StepOutcome::Done(Value::Closure(Rc::new(new))));
        }

        if total_args.len() == closure.arity {
            match &closure.kind {
                ClosureKind::Native(f) => {
                    // Call in-place: `f` borrows `closure.kind`, disjoint from
                    // `self`; no `Rc::clone` needed on this hot path.
                    let v = f(self, &total_args)?;
                    return Ok(StepOutcome::Done(v));
                }
                ClosureKind::Lambda(body) => {
                    let body = Rc::clone(body);
                    scope.enter_frame(&body.captured, &body.params, total_args);
                    *current = body.body.clone();
                    return Ok(StepOutcome::Continue);
                }
                ClosureKind::Bytecode(bf, upvals) => {
                    let v = crate::vm::exec::exec(self, bf, upvals, &total_args)?;
                    return Ok(StepOutcome::Done(v));
                }
            }
        }

        // Over-application: invoke with the first `arity` args, then apply
        // the result to the rest.
        let extra: Vec<_> = total_args.drain(closure.arity..).collect();
        let first = self.call_strict(&closure, total_args)?;
        let v = self.apply(first, extra)?;
        Ok(StepOutcome::Done(v))
    }

    /// Apply a closure expecting exactly `arity` args (used internally for
    /// the over-application path).
    fn call_strict(&mut self, closure: &Closure, args: ArgVec) -> ShenResult<Value> {
        match &closure.kind {
            ClosureKind::Native(f) => f(self, &args),
            ClosureKind::Lambda(body) => {
                let mut locals: Locals = body.captured.clone();
                for (p, a) in body.params.iter().zip(args) {
                    locals.push((*p, a));
                }
                self.eval_in(&body.body, &locals)
            }
            ClosureKind::Bytecode(bf, upvals) => crate::vm::exec::exec(self, bf, upvals, &args),
        }
    }

    /// Public apply — used by primitives like `apply` and by higher-order
    /// callers. Non-tail-position (always returns a final value).
    pub fn apply(&mut self, f: Value, args: Vec<Value>) -> ShenResult<Value> {
        let closure = match f {
            Value::Closure(c) => c,
            other => return Err(ShenError::new(format!("not callable: {other:?}"))),
        };

        // Convert once at the public boundary; internal paths stay on ArgVec.
        let mut total: ArgVec = if closure.partial.is_empty() {
            args.into()
        } else {
            closure.partial.iter().cloned().chain(args).collect()
        };

        if total.len() < closure.arity {
            let new = Closure {
                name: closure.name,
                arity: closure.arity,
                partial: total.into_vec(),
                kind: clone_kind(&closure.kind),
            };
            return Ok(Value::Closure(Rc::new(new)));
        }

        if total.len() == closure.arity {
            return self.call_strict(&closure, total);
        }

        let extra: Vec<_> = total.drain(closure.arity..).collect();
        let first = self.call_strict(&closure, total)?;
        self.apply(first, extra)
    }

    // --- Special-form helpers ---

    fn step_if(
        &mut self,
        args: &[KlExpr],
        scope: &Scope,
        current: &mut KlExpr,
    ) -> ShenResult<StepOutcome> {
        if args.len() != 3 {
            return Err(ShenError::new("if: expected 3 args"));
        }
        let cond = self.eval_in(&args[0], scope.slice())?;
        let taken = match cond {
            Value::Bool(true) => &args[1],
            Value::Bool(false) => &args[2],
            Value::Sym(s) if s == self.well_known.k_true => &args[1],
            Value::Sym(s) if s == self.well_known.k_false => &args[2],
            other => return Err(ShenError::new(format!("if: not a boolean: {other:?}"))),
        };
        *current = taken.clone();
        Ok(StepOutcome::Continue)
    }

    fn step_let(
        &mut self,
        args: &[KlExpr],
        scope: &mut Scope,
        current: &mut KlExpr,
    ) -> ShenResult<StepOutcome> {
        if args.len() != 3 {
            return Err(ShenError::new("let: expected 3 args"));
        }
        let var = match &args[0] {
            KlExpr::Sym(s) => *s,
            other => {
                return Err(ShenError::new(format!(
                    "let: var must be a symbol, got {other:?}"
                )))
            }
        };
        let value = self.eval_in(&args[1], scope.slice())?;
        scope.make_mut().push((var, value));
        *current = args[2].clone();
        Ok(StepOutcome::Continue)
    }

    fn step_cond(
        &mut self,
        args: &[KlExpr],
        scope: &Scope,
        current: &mut KlExpr,
    ) -> ShenResult<StepOutcome> {
        for clause in args {
            let pair = match clause {
                KlExpr::App(items) if items.len() == 2 => items,
                _ => return Err(ShenError::new("cond: clauses must be 2-element lists")),
            };
            let test = self.eval_in(&pair[0], scope.slice())?;
            let truthy = match test {
                Value::Bool(b) => b,
                Value::Sym(s) if s == self.well_known.k_true => true,
                Value::Sym(s) if s == self.well_known.k_false => false,
                other => return Err(ShenError::new(format!("cond: not a boolean: {other:?}"))),
            };
            if truthy {
                *current = pair[1].clone();
                return Ok(StepOutcome::Continue);
            }
        }
        Err(ShenError::new("cond: no clause matched"))
    }

    fn step_do(
        &mut self,
        args: &[KlExpr],
        scope: &Scope,
        current: &mut KlExpr,
    ) -> ShenResult<StepOutcome> {
        if args.is_empty() {
            return Ok(StepOutcome::Done(Value::Nil));
        }
        // Evaluate all but the last for side effects; tail-eval the last.
        for e in &args[..args.len() - 1] {
            self.eval_in(e, scope.slice())?;
        }
        *current = args[args.len() - 1].clone();
        Ok(StepOutcome::Continue)
    }

    fn step_and(
        &mut self,
        args: &[KlExpr],
        scope: &Scope,
        current: &mut KlExpr,
    ) -> ShenResult<StepOutcome> {
        if args.len() != 2 {
            return Err(ShenError::new("and: expected 2 args"));
        }
        let a = self.eval_in(&args[0], scope.slice())?;
        let truthy = self.truthy(&a)?;
        if !truthy {
            return Ok(StepOutcome::Done(Value::Bool(false)));
        }
        *current = args[1].clone();
        Ok(StepOutcome::Continue)
    }

    fn step_or(
        &mut self,
        args: &[KlExpr],
        scope: &Scope,
        current: &mut KlExpr,
    ) -> ShenResult<StepOutcome> {
        if args.len() != 2 {
            return Err(ShenError::new("or: expected 2 args"));
        }
        let a = self.eval_in(&args[0], scope.slice())?;
        let truthy = self.truthy(&a)?;
        if truthy {
            return Ok(StepOutcome::Done(Value::Bool(true)));
        }
        *current = args[1].clone();
        Ok(StepOutcome::Continue)
    }

    fn truthy(&self, v: &Value) -> ShenResult<bool> {
        match v {
            Value::Bool(b) => Ok(*b),
            Value::Sym(s) if *s == self.well_known.k_true => Ok(true),
            Value::Sym(s) if *s == self.well_known.k_false => Ok(false),
            other => Err(ShenError::new(format!("not a boolean: {other:?}"))),
        }
    }

    fn build_lambda(&mut self, args: &[KlExpr], locals: &[(SymId, Value)]) -> ShenResult<Value> {
        if args.len() != 2 {
            return Err(ShenError::new("lambda: expected (lambda PARAM BODY)"));
        }
        let param = match &args[0] {
            KlExpr::Sym(s) => *s,
            other => {
                return Err(ShenError::new(format!(
                    "lambda: param must be a symbol, got {other:?}"
                )))
            }
        };
        Ok(self.build_closure(&[param], 1, &args[1], locals))
    }

    fn build_freeze(&mut self, args: &[KlExpr], locals: &[(SymId, Value)]) -> ShenResult<Value> {
        if args.len() != 1 {
            return Err(ShenError::new("freeze: expected 1 arg"));
        }
        // (freeze E) ~ (lambda V E) with V fresh and ignored. We model
        // freeze as a 0-arity lambda so `(thaw f)` calls it with no args.
        Ok(self.build_closure(&[], 0, &args[0], locals))
    }

    /// Build a runtime closure for `(lambda ...)` / `(freeze ...)`. When the
    /// VM is enabled, compile the body to bytecode with its free variables
    /// captured as upvals (`ClosureKind::Bytecode`); on any compiler error
    /// (e.g. a body using `trap-error` / `thaw`, which the VM doesn't lower)
    /// fall back to the tree-walked `ClosureKind::Lambda`. With the VM off,
    /// always build the tree-walked closure.
    ///
    /// The kernel type-checker's hot continuations are exactly these
    /// `freeze`/`lambda` closures, so this is where compiling them to
    /// bytecode pays off (A3 in `design/perf-handoff.md`).
    fn build_closure(
        &mut self,
        params: &[SymId],
        arity: usize,
        body: &KlExpr,
        locals: &[(SymId, Value)],
    ) -> Value {
        let kind = self
            .try_compile_closure(params, body, locals)
            .unwrap_or_else(|| {
                ClosureKind::Lambda(Rc::new(LambdaBody {
                    captured: capture_used(body, locals),
                    params: params.to_vec(),
                    body: body.clone(),
                }))
            });
        Value::Closure(Rc::new(Closure {
            name: None,
            arity,
            partial: Vec::new(),
            kind,
        }))
    }

    /// Attempt to compile a runtime closure body to bytecode. Returns
    /// `None` when the VM is disabled or the body can't be lowered (caller
    /// falls back to the tree-walker).
    fn try_compile_closure(
        &mut self,
        params: &[SymId],
        body: &KlExpr,
        locals: &[(SymId, Value)],
    ) -> Option<ClosureKind> {
        if !vm_enabled() {
            return None;
        }
        // Determine the captured free variables: symbols referenced in the
        // body that are bound in the surrounding `locals`, in first-mention
        // order. Each captures its innermost binding (matching the
        // tree-walker's reverse-scan `lookup_local`). Names that aren't in
        // `locals` are left to resolve as globals / self-evaluating symbols,
        // exactly as in the tree-walker.
        let mut used: SmallVec<[SymId; 16]> = SmallVec::new();
        collect_used_syms(body, &mut used);
        let mut upval_names: Vec<SymId> = Vec::new();
        let mut captured: Vec<Value> = Vec::new();
        for s in used {
            if params.contains(&s) {
                continue;
            }
            if let Some(v) = self.lookup_local(s, locals) {
                upval_names.push(s);
                captured.push(v);
            }
        }
        match crate::vm::compile_closure(self, params, &upval_names, body) {
            Ok(bf) => Some(ClosureKind::Bytecode(Rc::new(bf), captured)),
            Err(_) => None,
        }
    }

    fn step_thaw(&mut self, args: &[KlExpr], scope: &Scope) -> ShenResult<StepOutcome> {
        if args.len() != 1 {
            return Err(ShenError::new("thaw: expected 1 arg"));
        }
        let f = self.eval_in(&args[0], scope.slice())?;
        let v = self.apply(f, Vec::new())?;
        Ok(StepOutcome::Done(v))
    }

    fn step_trap_error(&mut self, args: &[KlExpr], scope: &Scope) -> ShenResult<StepOutcome> {
        if args.len() != 2 {
            return Err(ShenError::new("trap-error: expected 2 args"));
        }
        match self.eval_in(&args[0], scope.slice()) {
            Ok(v) => Ok(StepOutcome::Done(v)),
            // A budget/deadline cancellation is not a Shen-level error: it
            // must propagate past `trap-error` so the scheduler sees the
            // abort rather than having a user handler swallow it.
            Err(e) if e.is_cancelled() => Err(e),
            Err(e) => {
                let handler = self.eval_in(&args[1], scope.slice())?;
                let err_val = Value::Error(e.message.clone());
                let v = self.apply(handler, vec![err_val])?;
                Ok(StepOutcome::Done(v))
            }
        }
    }

    fn do_defun(&mut self, args: &[KlExpr]) -> ShenResult<Value> {
        if args.len() != 3 {
            return Err(ShenError::new("defun: expected (defun NAME PARAMS BODY)"));
        }
        let name = match &args[0] {
            KlExpr::Sym(s) => *s,
            other => {
                return Err(ShenError::new(format!(
                    "defun: name must be a symbol, got {other:?}"
                )))
            }
        };
        let params: Vec<SymId> = match &args[1] {
            KlExpr::Nil => Vec::new(),
            KlExpr::App(items) => {
                let mut ps = Vec::with_capacity(items.len());
                for it in items.iter() {
                    match it {
                        KlExpr::Sym(s) => ps.push(*s),
                        other => {
                            return Err(ShenError::new(format!("defun: bad param: {other:?}")))
                        }
                    }
                }
                ps
            }
            other => {
                return Err(ShenError::new(format!(
                    "defun: param list malformed: {other:?}"
                )))
            }
        };
        // With `SHEN_CEDAR_VM=1`, try to compile the body into bytecode,
        // falling back to the tree-walked `ClosureKind::Lambda` on any
        // compiler error (e.g. unsupported special forms like
        // `trap-error` or `thaw`). Off by default — see `vm_enabled`.
        let kind = if vm_enabled() {
            match crate::vm::compile_fn(self, Some(name), &params, &args[2]) {
                Ok(bf) => ClosureKind::Bytecode(Rc::new(bf), Vec::new()),
                Err(_) => ClosureKind::Lambda(Rc::new(LambdaBody {
                    captured: Vec::new(),
                    params: params.clone(),
                    body: args[2].clone(),
                })),
            }
        } else {
            ClosureKind::Lambda(Rc::new(LambdaBody {
                captured: Vec::new(),
                params: params.clone(),
                body: args[2].clone(),
            }))
        };
        let closure = Closure {
            name: Some(name),
            arity: params.len(),
            partial: Vec::new(),
            kind,
        };
        self.env.set_fn(name, Value::Closure(Rc::new(closure)));
        Ok(Value::Sym(name))
    }

    fn quote_value(&self, expr: &KlExpr) -> Value {
        match expr {
            KlExpr::Nil => Value::Nil,
            KlExpr::Bool(b) => Value::Bool(*b),
            KlExpr::Int(n) => Value::Int(*n),
            KlExpr::Float(x) => Value::Float(*x),
            KlExpr::Str(s) => Value::Str(s.clone()),
            KlExpr::Sym(s) => Value::Sym(*s),
            KlExpr::App(items) => Value::list(items.iter().map(|e| self.quote_value(e))),
        }
    }
}

/// Whether runtime `defun` evaluation should compile the body into
/// bytecode (the VM path) instead of building a tree-walked
/// `ClosureKind::Lambda`.
///
/// **Opt-in** (`SHEN_CEDAR_VM=1`), not the default. As of the B5
/// milestone the bytecode VM is correct (134/0 kernel-tests + full unit
/// coverage) but measured *slower* than the tree-walker on this
/// workload: kernel-tests is dominated by AOT-to-AOT dispatch, so
/// user-`defun` calls (the only place the VM runs) are a minority, and
/// the VM's per-call `Vec<Value>` locals+stack allocation costs more
/// than the tree-walker's allocation-free `Scope` COW. The VM should
/// become a win once Phase 3 (tagged `Value(u64)`, 24→8 bytes) makes
/// those per-call frames ~3× cheaper. Until then it stays behind the
/// flag so the default keeps the tree-walker's performance.
fn vm_enabled() -> bool {
    std::env::var_os("SHEN_CEDAR_VM").is_some()
}

fn clone_kind(kind: &ClosureKind) -> ClosureKind {
    match kind {
        ClosureKind::Native(f) => ClosureKind::Native(Rc::clone(f)),
        ClosureKind::Lambda(b) => ClosureKind::Lambda(Rc::clone(b)),
        ClosureKind::Bytecode(bf, upvals) => ClosureKind::Bytecode(Rc::clone(bf), upvals.clone()),
    }
}

/// Filter `locals` to only the entries the closure body might look up.
///
/// The kernel type-checker builds many `freeze`/`lambda` continuations
/// whose body references a handful of variables out of a 20–50 entry
/// outer scope. Capturing everything inflates both the snapshot clone
/// here and every subsequent `lookup_local` scan inside the body.
///
/// Conservative: any `KlExpr::Sym(s)` reference in the body marks `s` as
/// possibly looked up. Inner `let`/`lambda` shadowing isn't tracked here —
/// they bind at evaluation, and the innermost-wins reverse scan handles
/// it. Worst case we keep a slot that gets shadowed; never wrong.
fn capture_used(body: &KlExpr, locals: &[(SymId, Value)]) -> Vec<(SymId, Value)> {
    if locals.is_empty() {
        return Vec::new();
    }
    let mut used: SmallVec<[SymId; 16]> = SmallVec::new();
    collect_used_syms(body, &mut used);
    if used.is_empty() {
        return Vec::new();
    }
    locals
        .iter()
        .filter(|(s, _)| used.contains(s))
        .cloned()
        .collect()
}

fn collect_used_syms(expr: &KlExpr, out: &mut SmallVec<[SymId; 16]>) {
    match expr {
        KlExpr::Sym(s) if !out.contains(s) => {
            out.push(*s);
        }
        KlExpr::App(items) => {
            for child in items.iter() {
                collect_used_syms(child, out);
            }
        }
        _ => {}
    }
}

enum StepOutcome {
    Done(Value),
    Continue,
}
