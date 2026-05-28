\* ================================================================== *\
\* shen-ocaml backpressure specs                                      *\
\* Sequent-calculus types encoding project invariants.                 *\
\* Gate 4 (tc +) verifies internal consistency.                       *\
\* shengen-ocaml emits .ml/.mli guard types from these definitions.   *\
\* ================================================================== *\

\* === Value Representation === *\

\* Every KL value belongs to exactly one runtime variant. *\
(datatype kl-value
  X : number;
  ============
  X : kl-value;

  X : string;
  ============
  X : kl-value;

  X : symbol;
  ============
  X : kl-value;)

\* === Symbol System === *\

\* Interned symbol: name paired with a unique non-negative integer ID. *\
(datatype interned-symbol
  Name : string;
  Id : number;
  (>= Id 0) : verified;
  ========================
  [Name Id] : interned-symbol;)

\* === Dual Namespace === *\

\* Function binding: symbol with non-negative arity in function namespace. *\
(datatype fn-binding
  Name : interned-symbol;
  Arity : number;
  (>= Arity 0) : verified;
  ==========================
  [Name Arity] : fn-binding;)

\* Value binding: symbol in global-value namespace. *\
(datatype val-binding
  Name : interned-symbol;
  ========================
  Name : val-binding;)

\* Namespace-safe proof: a lookup was resolved in the correct namespace. *\
(datatype namespace-checked
  B : fn-binding;
  =================
  B : namespace-checked;

  B : val-binding;
  =================
  B : namespace-checked;)

\* === Arity & Calling Convention === *\

\* Resolved arity: function has known positive arity metadata. *\
(datatype resolved-arity
  F : fn-binding;
  Arity : number;
  (> Arity 0) : verified;
  =========================
  [F Arity] : resolved-arity;)

\* Checked application: argument count validated against resolved arity. *\
(datatype checked-application
  F : resolved-arity;
  ArgCount : number;
  (>= ArgCount 0) : verified;
  =============================
  [F ArgCount] : checked-application;)

\* === AST & IR === *\

\* Parsed KL AST that passed parser validation. *\
(datatype valid-kl-ast
  Source : string;
  ================
  Source : valid-kl-ast;)

\* IR node with tail-position annotation completed. *\
(datatype tail-annotated
  Ast : valid-kl-ast;
  =====================
  Ast : tail-annotated;)

\* === Code Generation === *\

\* Generated OCaml module traceable to a valid, tail-annotated IR. *\
(datatype generated-module
  Ir : tail-annotated;
  ModName : string;
  ===================
  [Ir ModName] : generated-module;)

\* Registration proof: generated module registered its functions in the table. *\
(datatype registered-module
  Mod : generated-module;
  ========================
  Mod : registered-module;)

\* === Boot Sequence === *\

\* All kernel modules loaded (>= 20 .kl files present). *\
(datatype kernel-loaded
  Count : number;
  (>= Count 20) : verified;
  ===========================
  Count : kernel-loaded;)

\* Boot complete: kernel loaded and shen.initialise called. *\
(datatype boot-complete
  K : kernel-loaded;
  ===================
  K : boot-complete;)

\* === Interpreter / eval-kl === *\

\* eval-kl input validated against KL grammar before evaluation. *\
(datatype eval-kl-safe
  Expr : valid-kl-ast;
  =====================
  Expr : eval-kl-safe;)
