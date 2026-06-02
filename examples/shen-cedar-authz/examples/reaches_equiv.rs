//! Example 5 — **one relation, two evaluations, differentially tested**.
//!
//! Role reachability ("is role A a member of, i.e. does it reach, role B over
//! the role DAG?") is specified *twice* and checked for agreement over the
//! whole finite domain:
//!
//!   * **Prolog** — the kernel's REAL built-in Prolog engine. `reaches` is a
//!     `defprolog` procedure (reflexivity + parent-transitivity) over `parent`
//!     facts for the role DAG. Each `(role, role)` pair is decided by running a
//!     ground goal through the engine via `(prolog? (reaches A B))`, which the
//!     served VM evaluates to `true`/`false`.
//!
//!   * **Functional** — the `reaches` *defun* ported verbatim from `verify.rs`:
//!     a `[child parent]` edge-list reachability fold (`reaches` / `reaches-list`
//!     / `parents-of`). No unification, no backtracking — a plain recursive
//!     membership-closure over the same DAG.
//!
//! Both run in the SAME served Shen VM, in one Rust process. The gate
//! enumerates every ordered pair over the role set, evaluates both relations,
//! prints the truth table, and asserts the two agree on every cell. Exit 0 iff
//! the relations are identical over the domain; exit 1 on any divergence —
//! the same gate style as `equiv.rs`.
//!
//! Run: `cargo run -p shen-cedar-authz --example reaches_equiv`

use shen_cedar_authz::ShenHost;
use shen_rust::value::Value;

/// The role set. Every ordered pair (49 of them) is checked.
const ROLES: &[&str] = &[
    "Analyst", "Auditor", "Admin", "Lead", "Staff", "Intern", "Manager",
];

/// The role DAG as `child in parent` edges. Shared by both evaluations:
///   * the Prolog side reads it as `parent/2` facts (lower-cased symbols);
///   * the functional side reads it as a `[child parent]` Shen edge list.
///
/// A small diamond + chain so reflexivity, single-hop, multi-hop and
/// no-reverse-edge cases are all exercised.
const EDGES: &[(&str, &str)] = &[
    ("Analyst", "Staff"),
    ("Auditor", "Staff"),
    ("Intern", "Staff"),
    ("Staff", "Lead"),
    ("Lead", "Manager"),
    ("Admin", "Manager"),
];

/// The FUNCTIONAL `reaches`, ported verbatim from `verify.rs`: a `[child parent]`
/// edge-list reachability fold. `a reaches b` == `a` is-in `b` (a == b, or some
/// parent of a reaches b).
const FUNCTIONAL: &[&str] = &[
    "(defun parents-of (x es) (if (= es []) [] \
       (if (= (hd (hd es)) x) (cons (hd (tl (hd es))) (parents-of x (tl es))) \
         (parents-of x (tl es)))))",
    "(defun reaches (a b es) (if (= a b) true (reaches-list (parents-of a es) b es)))",
    "(defun reaches-list (ps b es) (if (= ps []) false \
       (if (reaches (hd ps) b es) true (reaches-list (tl ps) b es))))",
];

/// The role-reachability relation in the kernel's REAL Prolog engine.
///
/// `prolog-reaches` is the procedure name (kept distinct from the functional
/// `reaches` defun so both can coexist in one VM). `;` is spaced away from
/// `<--` because the embedding parses these forms with the KL reader, which —
/// unlike Shen's own `.shen` reader — does not split a trailing `<--;` token.
///
/// `parent/2` carries the same edges as `EDGES`, written as lower-cased
/// symbols (Prolog atoms) and generated below so the two sources cannot drift.
fn prolog_source() -> String {
    let mut facts = String::from("(defprolog parent\n");
    for (c, p) in EDGES {
        facts.push_str(&format!(
            "  {} {} <-- ;\n",
            c.to_lowercase(),
            p.to_lowercase()
        ));
    }
    facts.push_str(")\n");
    // reflexivity + parent-transitivity
    facts.push_str(
        "(defprolog prolog-reaches\n  \
           X X <-- ;\n  \
           X Y <-- (parent X Z) (prolog-reaches Z Y) ;)",
    );
    facts
}

fn main() {
    let handle = std::thread::Builder::new()
        .stack_size(64 * 1024 * 1024)
        .spawn(run)
        .expect("spawn");
    std::process::exit(handle.join().unwrap_or(2));
}

fn run() -> i32 {
    eprint!("booting served Shen VM… ");
    let mut host = match ShenHost::new() {
        Ok(h) => h,
        Err(e) => {
            eprintln!("FAILED: {e}");
            return 1;
        }
    };

    // Load the FUNCTIONAL reaches (defun) ported from verify.rs.
    if let Err(e) = host.load_source(&FUNCTIONAL.join("\n")) {
        eprintln!("load functional reaches: {e}");
        return 1;
    }
    // Load the PROLOG reaches (defprolog) into the SAME VM.
    if let Err(e) = host.load_source(&prolog_source()) {
        eprintln!("load prolog reaches: {e}");
        return 1;
    }
    eprintln!("ready.\n");

    // Build the `[child parent]` edge list as Shen data once (functional input).
    let edges: Vec<Value> = EDGES
        .iter()
        .map(|(c, p)| host.list([host.string(c), host.string(p)]))
        .collect();
    let edges = host.list(edges);

    println!(
        "Role reachability — functional (defun) vs Prolog (defprolog), same DAG, same VM.\n\
         Legend per cell: · = neither reaches, T = both true, F = both false (agree),\n\
         X = DISAGREE.  Rows = from-role, columns = to-role.\n"
    );

    // Header row.
    print!("{:>9} |", "from \\ to");
    for to in ROLES {
        print!(" {:>7}", to);
    }
    println!();
    print!("{:->9}-+", "");
    for _ in ROLES {
        print!("{:->8}", "");
    }
    println!();

    let mut disagreements = 0usize;
    let mut true_count = 0usize;
    for from in ROLES {
        print!("{:>9} |", from);
        for to in ROLES {
            let f = functional_reaches(&mut host, from, to, edges);
            let p = prolog_reaches(&mut host, from, to);
            let (fv, pv) = match (f, p) {
                (Ok(fv), Ok(pv)) => (fv, pv),
                (Err(e), _) | (_, Err(e)) => {
                    println!("\nevaluation error at ({from},{to}): {e}");
                    return 1;
                }
            };
            if fv {
                true_count += 1;
            }
            let cell = if fv != pv {
                disagreements += 1;
                "X"
            } else if fv {
                "T"
            } else {
                "F"
            };
            print!(" {:>7}", cell);
        }
        println!();
    }

    let total = ROLES.len() * ROLES.len();
    println!(
        "\nChecked {total} ordered pairs over {} roles ({true_count} reachable, {} not).",
        ROLES.len(),
        total - true_count
    );

    if disagreements == 0 {
        println!(
            "Functional `reaches` (defun) and Prolog `prolog-reaches` (defprolog) \
             AGREE on every pair ✓"
        );
        0
    } else {
        println!("DIVERGENCE: {disagreements} pair(s) where the two relations disagree (X) ✗");
        1
    }
}

/// Decide `from reaches to` with the FUNCTIONAL defun over the `[child parent]`
/// edge list.
fn functional_reaches(
    host: &mut ShenHost,
    from: &str,
    to: &str,
    edges: Value,
) -> Result<bool, String> {
    let a = host.string(from);
    let b = host.string(to);
    let v = host
        .call("reaches", vec![a, b, edges])
        .map_err(|e| format!("functional reaches: {e}"))?;
    Ok(v.as_bool().unwrap_or(false))
}

/// Decide `from reaches to` by running a GROUND goal through the kernel's real
/// Prolog engine: `(prolog? (prolog-reaches <from> <to>))` evaluates to a
/// boolean. Roles are lower-cased to match the `parent/2` atoms.
fn prolog_reaches(host: &mut ShenHost, from: &str, to: &str) -> Result<bool, String> {
    let query = format!(
        "(prolog? (prolog-reaches {} {}))",
        from.to_lowercase(),
        to.to_lowercase()
    );
    let v = host
        .eval(&query)
        .map_err(|e| format!("prolog query: {e}"))?;
    Ok(v.as_bool().unwrap_or(false))
}
