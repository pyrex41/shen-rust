//! Auto-generated guard types from `specs/core.shen`.
//!
//! DO NOT EDIT — regenerate with `cargo run -p shengen-rust -- \
//!   specs/core.shen crates/shen-cedar/src/generated/guard_types.rs`.
//!
//! Every struct here uses private fields and a fallible public
//! constructor that enforces the corresponding sequent's `: verified`
//! premises. The `gate 2 (cargo build)` step is the proof: callers
//! cannot construct a value without going through `new`.

#![allow(
    dead_code,
    clippy::needless_pass_by_value,
    clippy::redundant_field_names,
    clippy::unnecessary_cast,
    clippy::neg_cmp_op_on_partial_ord
)]

/// kl-value
#[derive(Clone, Debug)]
pub struct KlValue {
    x: String,
}

impl KlValue {
    pub fn new(x: String) -> Result<Self, String> {
        Ok(Self { x: x })
    }

    pub fn x(&self) -> &String {
        &self.x
    }
}

/// interned-symbol
#[derive(Clone, Debug)]
pub struct InternedSymbol {
    name: String,
    id: f64,
}

impl InternedSymbol {
    pub fn new(name: String, id: f64) -> Result<Self, String> {
        if !(id >= (0 as f64)) {
            return Err(format!(
                "InternedSymbol: invariant `id >= 0` violated (got {})",
                id
            ));
        }
        Ok(Self { name: name, id: id })
    }

    pub fn name(&self) -> &String {
        &self.name
    }
    pub fn id(&self) -> &f64 {
        &self.id
    }
}

/// fn-binding
#[derive(Clone, Debug)]
pub struct FnBinding {
    name: InternedSymbol,
    arity: f64,
}

impl FnBinding {
    pub fn new(name: InternedSymbol, arity: f64) -> Result<Self, String> {
        if !(arity >= (0 as f64)) {
            return Err(format!(
                "FnBinding: invariant `arity >= 0` violated (got {})",
                arity
            ));
        }
        Ok(Self {
            name: name,
            arity: arity,
        })
    }

    pub fn name(&self) -> &InternedSymbol {
        &self.name
    }
    pub fn arity(&self) -> &f64 {
        &self.arity
    }
}

/// val-binding
#[derive(Clone, Debug)]
pub struct ValBinding {
    name: InternedSymbol,
}

impl ValBinding {
    pub fn new(name: InternedSymbol) -> Result<Self, String> {
        Ok(Self { name: name })
    }

    pub fn name(&self) -> &InternedSymbol {
        &self.name
    }
}

/// namespace-checked
#[derive(Clone, Debug)]
pub struct NamespaceChecked {
    b: ValBinding,
}

impl NamespaceChecked {
    pub fn new(b: ValBinding) -> Result<Self, String> {
        Ok(Self { b: b })
    }

    pub fn b(&self) -> &ValBinding {
        &self.b
    }
}

/// resolved-arity
#[derive(Clone, Debug)]
pub struct ResolvedArity {
    f: FnBinding,
    arity: f64,
}

impl ResolvedArity {
    pub fn new(f: FnBinding, arity: f64) -> Result<Self, String> {
        if !(arity > (0 as f64)) {
            return Err(format!(
                "ResolvedArity: invariant `arity > 0` violated (got {})",
                arity
            ));
        }
        Ok(Self { f: f, arity: arity })
    }

    pub fn f(&self) -> &FnBinding {
        &self.f
    }
    pub fn arity(&self) -> &f64 {
        &self.arity
    }
}

/// checked-application
#[derive(Clone, Debug)]
pub struct CheckedApplication {
    f: ResolvedArity,
    argcount: f64,
}

impl CheckedApplication {
    pub fn new(f: ResolvedArity, argcount: f64) -> Result<Self, String> {
        if !(argcount >= (0 as f64)) {
            return Err(format!(
                "CheckedApplication: invariant `argcount >= 0` violated (got {})",
                argcount
            ));
        }
        Ok(Self {
            f: f,
            argcount: argcount,
        })
    }

    pub fn f(&self) -> &ResolvedArity {
        &self.f
    }
    pub fn argcount(&self) -> &f64 {
        &self.argcount
    }
}

/// valid-kl-ast
#[derive(Clone, Debug)]
pub struct ValidKlAst {
    source: String,
}

impl ValidKlAst {
    pub fn new(source: String) -> Result<Self, String> {
        Ok(Self { source: source })
    }

    pub fn source(&self) -> &String {
        &self.source
    }
}

/// tail-annotated
#[derive(Clone, Debug)]
pub struct TailAnnotated {
    ast: ValidKlAst,
}

impl TailAnnotated {
    pub fn new(ast: ValidKlAst) -> Result<Self, String> {
        Ok(Self { ast: ast })
    }

    pub fn ast(&self) -> &ValidKlAst {
        &self.ast
    }
}

/// generated-module
#[derive(Clone, Debug)]
pub struct GeneratedModule {
    ir: TailAnnotated,
    modname: String,
}

impl GeneratedModule {
    pub fn new(ir: TailAnnotated, modname: String) -> Result<Self, String> {
        Ok(Self {
            ir: ir,
            modname: modname,
        })
    }

    pub fn ir(&self) -> &TailAnnotated {
        &self.ir
    }
    pub fn modname(&self) -> &String {
        &self.modname
    }
}

/// registered-module
#[derive(Clone, Debug)]
pub struct RegisteredModule {
    mod_: GeneratedModule,
}

impl RegisteredModule {
    pub fn new(mod_: GeneratedModule) -> Result<Self, String> {
        Ok(Self { mod_: mod_ })
    }

    pub fn mod_(&self) -> &GeneratedModule {
        &self.mod_
    }
}

/// kernel-loaded
#[derive(Clone, Debug)]
pub struct KernelLoaded {
    count: f64,
}

impl KernelLoaded {
    pub fn new(count: f64) -> Result<Self, String> {
        if !(count >= (20 as f64)) {
            return Err(format!(
                "KernelLoaded: invariant `count >= 20` violated (got {})",
                count
            ));
        }
        Ok(Self { count: count })
    }

    pub fn count(&self) -> &f64 {
        &self.count
    }
}

/// boot-complete
#[derive(Clone, Debug)]
pub struct BootComplete {
    k: KernelLoaded,
}

impl BootComplete {
    pub fn new(k: KernelLoaded) -> Result<Self, String> {
        Ok(Self { k: k })
    }

    pub fn k(&self) -> &KernelLoaded {
        &self.k
    }
}

/// eval-kl-safe
#[derive(Clone, Debug)]
pub struct EvalKlSafe {
    expr: ValidKlAst,
}

impl EvalKlSafe {
    pub fn new(expr: ValidKlAst) -> Result<Self, String> {
        Ok(Self { expr: expr })
    }

    pub fn expr(&self) -> &ValidKlAst {
        &self.expr
    }
}
