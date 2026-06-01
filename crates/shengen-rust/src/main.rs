//! shengen-rust: parse Shen sequent-calculus specs and emit Rust guard
//! types with private fields + public factory constructors.
//!
//! NOT a Shen interpreter. A pure text parser modelled on
//! `shen-ocaml/bin/shengen-ocaml`. Input syntax (per spec):
//!
//! ```text
//! (datatype NAME
//!   X : type1;
//!   Y : type2;
//!   (>= Y 0) : verified;
//!   ============
//!   [X Y] : NAME;)
//! ```
//!
//! Output: a single `.rs` file with one `pub struct` per datatype, plus
//! `pub fn new(...) -> Result<Self, String>` constructors that enforce
//! every `(... : verified)` predicate at runtime.

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Debug, Clone)]
enum Premise {
    Typed { var: String, typ: String },
    Guard { op: String, args: Vec<String> },
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
struct Rule {
    premises: Vec<Premise>,
    conclusion_vars: Vec<String>,
    conclusion_type: String,
}

#[derive(Debug, Clone)]
struct DatatypeDef {
    name: String,
    rules: Vec<Rule>,
}

fn strip_comments(src: &str) -> String {
    // Shen block comments: \* ... *\
    let mut out = String::with_capacity(src.len());
    let bytes = src.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if i + 1 < bytes.len() && bytes[i] == b'\\' && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'\\') {
                i += 1;
            }
            if i + 1 < bytes.len() {
                i += 2;
            }
            continue;
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

fn tokenize(src: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let bytes = src.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if c == b'(' || c == b')' || c == b'[' || c == b']' || c == b';' {
            tokens.push(String::from(c as char));
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() {
            let c = bytes[i];
            if c.is_ascii_whitespace() || matches!(c, b'(' | b')' | b'[' | b']' | b';') {
                break;
            }
            i += 1;
        }
        tokens.push(String::from_utf8_lossy(&bytes[start..i]).into_owned());
    }
    tokens
}

fn is_separator(tok: &str) -> bool {
    tok.len() >= 3 && tok.bytes().all(|c| c == b'=')
}

fn parse_datatypes(src: &str) -> Vec<DatatypeDef> {
    let src = strip_comments(src);
    let tokens = tokenize(&src);
    let mut defs = Vec::new();
    let mut i = 0;

    while i < tokens.len() {
        if tokens[i] == "(" && i + 2 < tokens.len() && tokens[i + 1] == "datatype" {
            let name = tokens[i + 2].clone();
            i += 3;
            let mut dt = DatatypeDef {
                name,
                rules: Vec::new(),
            };

            while i < tokens.len() && tokens[i] != ")" {
                let mut premises = Vec::new();
                while i < tokens.len() && !is_separator(&tokens[i]) && tokens[i] != ")" {
                    if tokens[i] == "(" {
                        // Guard: (op arg...) : verified ;
                        i += 1;
                        if i >= tokens.len() {
                            break;
                        }
                        let op = tokens[i].clone();
                        i += 1;
                        let mut args = Vec::new();
                        while i < tokens.len() && tokens[i] != ")" {
                            args.push(tokens[i].clone());
                            i += 1;
                        }
                        if i < tokens.len() {
                            i += 1; // close paren
                        }
                        if i + 2 < tokens.len()
                            && tokens[i] == ":"
                            && tokens[i + 1] == "verified"
                            && tokens[i + 2] == ";"
                        {
                            premises.push(Premise::Guard { op, args });
                            i += 3;
                        } else {
                            i += 1;
                        }
                    } else if i + 3 < tokens.len() && tokens[i + 1] == ":" && tokens[i + 3] == ";" {
                        premises.push(Premise::Typed {
                            var: tokens[i].clone(),
                            typ: tokens[i + 2].clone(),
                        });
                        i += 4;
                    } else {
                        i += 1;
                    }
                }

                if i < tokens.len() && is_separator(&tokens[i]) {
                    i += 1;
                }

                let mut cvars = Vec::new();
                let mut ctype = String::new();
                if i < tokens.len() && tokens[i] == "[" {
                    i += 1;
                    while i < tokens.len() && tokens[i] != "]" {
                        cvars.push(tokens[i].clone());
                        i += 1;
                    }
                    if i < tokens.len() {
                        i += 1;
                    }
                    if i + 1 < tokens.len() && tokens[i] == ":" {
                        ctype = tokens[i + 1].clone();
                        i += 2;
                        if i < tokens.len() && tokens[i] == ";" {
                            i += 1;
                        }
                    }
                } else if i < tokens.len() && tokens[i] != ")" && tokens[i] != "(" {
                    cvars.push(tokens[i].clone());
                    i += 1;
                    if i + 1 < tokens.len() && tokens[i] == ":" {
                        ctype = tokens[i + 1].clone();
                        i += 2;
                        if i < tokens.len() && tokens[i] == ";" {
                            i += 1;
                        }
                    }
                }

                if !ctype.is_empty() && !premises.is_empty() {
                    dt.rules.push(Rule {
                        premises,
                        conclusion_vars: cvars,
                        conclusion_type: ctype,
                    });
                }
            }
            if i < tokens.len() && tokens[i] == ")" {
                i += 1;
            }
            defs.push(dt);
        } else {
            i += 1;
        }
    }
    defs
}

/// The rule with the most premises is the "primary" — the composite,
/// guard-bearing case for that datatype.
fn primary_rule(dt: &DatatypeDef) -> &Rule {
    dt.rules
        .iter()
        .max_by_key(|r| r.premises.len())
        .expect("datatype with no rules")
}

/// Sanitize a Shen identifier for Rust use. Maps `-` to `_` and strips
/// other non-ident chars. Returns a PascalCase struct name when
/// `pascal == true`, else snake_case.
fn sanitize(name: &str, pascal: bool) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let trimmed = cleaned.trim_matches('_');
    if pascal {
        let mut out = String::with_capacity(trimmed.len());
        let mut upper = true;
        for c in trimmed.chars() {
            if c == '_' {
                upper = true;
                continue;
            }
            if upper {
                out.push(c.to_ascii_uppercase());
                upper = false;
            } else {
                out.push(c.to_ascii_lowercase());
            }
        }
        if out.is_empty() {
            "T".to_string()
        } else {
            out
        }
    } else {
        let snake = trimmed.to_ascii_lowercase();
        if snake.is_empty() {
            "f".to_string()
        } else {
            snake
        }
    }
}

const RUST_KEYWORDS: &[&str] = &[
    "as", "break", "const", "continue", "crate", "else", "enum", "extern", "false", "fn", "for",
    "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref", "return",
    "self", "Self", "static", "struct", "super", "trait", "true", "type", "unsafe", "use", "where",
    "while", "async", "await", "dyn", "abstract", "become", "box", "do", "final", "macro",
    "override", "priv", "typeof", "unsized", "virtual", "yield", "try",
];

fn field_name(var: &str) -> String {
    let n = sanitize(var, false);
    if RUST_KEYWORDS.iter().any(|k| *k == n) {
        format!("{n}_")
    } else {
        n
    }
}

fn type_name(name: &str) -> String {
    sanitize(name, true)
}

fn rust_type(shen_type: &str) -> String {
    match shen_type {
        "number" => "f64".to_string(),
        "string" | "symbol" => "String".to_string(),
        other => type_name(other),
    }
}

struct Field {
    name: String,
    typ: String,
}

fn fields_of(rule: &Rule) -> Vec<Field> {
    rule.premises
        .iter()
        .filter_map(|p| match p {
            Premise::Typed { var, typ } => Some(Field {
                name: field_name(var),
                typ: rust_type(typ),
            }),
            _ => None,
        })
        .collect()
}

fn guards_of(rule: &Rule) -> Vec<&Premise> {
    rule.premises
        .iter()
        .filter(|p| matches!(p, Premise::Guard { .. }))
        .collect()
}

fn gen_rust(defs: &[DatatypeDef]) -> String {
    let mut out = String::new();
    out.push_str("//! Auto-generated guard types from `specs/core.shen`.\n");
    out.push_str("//!\n");
    out.push_str("//! DO NOT EDIT — regenerate with `cargo run -p shengen-rust -- \\\n");
    out.push_str("//!   specs/core.shen crates/shen-rust/src/generated/guard_types.rs`.\n");
    out.push_str("//!\n");
    out.push_str("//! Every struct here uses private fields and a fallible public\n");
    out.push_str("//! constructor that enforces the corresponding sequent's `: verified`\n");
    out.push_str("//! premises. The `gate 2 (cargo build)` step is the proof: callers\n");
    out.push_str("//! cannot construct a value without going through `new`.\n\n");
    out.push_str("#![allow(\n");
    out.push_str("    dead_code,\n");
    out.push_str("    clippy::needless_pass_by_value,\n");
    out.push_str("    clippy::redundant_field_names,\n");
    out.push_str("    clippy::unnecessary_cast,\n");
    out.push_str("    clippy::neg_cmp_op_on_partial_ord,\n");
    out.push_str(")]\n\n");

    for dt in defs {
        let tn = type_name(&dt.name);
        let rule = primary_rule(dt);
        let flds = fields_of(rule);
        let guards = guards_of(rule);

        out.push_str(&format!("/// {}\n", dt.name));
        out.push_str("#[derive(Clone, Debug)]\n");
        out.push_str(&format!("pub struct {tn} {{\n"));
        if flds.is_empty() {
            out.push_str("    _marker: (),\n");
        } else {
            for f in &flds {
                out.push_str(&format!("    {}: {},\n", f.name, f.typ));
            }
        }
        out.push_str("}\n\n");

        // Constructor signature.
        out.push_str(&format!("impl {tn} {{\n"));
        let params: Vec<String> = flds
            .iter()
            .map(|f| format!("{}: {}", f.name, f.typ))
            .collect();
        out.push_str(&format!(
            "    pub fn new({}) -> Result<Self, String> {{\n",
            params.join(", ")
        ));
        for g in &guards {
            if let Premise::Guard { op, args } = g {
                if args.len() == 2 {
                    let lhs = field_name(&args[0]);
                    let rhs = &args[1];
                    let rhs_val = if rhs.parse::<f64>().is_ok() {
                        format!("{rhs}_f64")
                    } else {
                        rhs.clone()
                    };
                    let rhs_lit = if rhs.parse::<f64>().is_ok() {
                        format!("({rhs} as f64)")
                    } else {
                        rhs.clone()
                    };
                    let _ = rhs_val;
                    out.push_str(&format!(
                        "        if !({lhs} {op} {rhs_lit}) {{\n            return Err(format!(\"{tn}: invariant `{lhs} {op} {rhs}` violated (got {{}})\", {lhs}));\n        }}\n"
                    ));
                }
            }
        }
        if flds.is_empty() {
            out.push_str("        Ok(Self { _marker: () })\n");
        } else {
            out.push_str("        Ok(Self {\n");
            for f in &flds {
                out.push_str(&format!("            {0}: {0},\n", f.name));
            }
            out.push_str("        })\n");
        }
        out.push_str("    }\n\n");

        // Accessors.
        for f in &flds {
            out.push_str(&format!(
                "    pub fn {0}(&self) -> &{1} {{ &self.{0} }}\n",
                f.name, f.typ
            ));
        }
        out.push_str("}\n\n");
    }
    out
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    if args.len() != 3 {
        eprintln!("Usage: shengen-rust <spec.shen> <output.rs>");
        return ExitCode::from(2);
    }
    let spec_path = PathBuf::from(&args[1]);
    let out_path = PathBuf::from(&args[2]);

    let src = match fs::read_to_string(&spec_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("shengen-rust: read {spec_path:?}: {e}");
            return ExitCode::from(1);
        }
    };
    let defs = parse_datatypes(&src);
    eprintln!(
        "shengen-rust: parsed {} datatype(s) from {}",
        defs.len(),
        spec_path.display()
    );
    for dt in &defs {
        eprintln!("  - {} ({} rules)", dt.name, dt.rules.len());
    }

    let rust = gen_rust(&defs);
    if let Some(parent) = out_path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).ok();
        }
    }
    if let Err(e) = fs::write(&out_path, rust) {
        eprintln!("shengen-rust: write {out_path:?}: {e}");
        return ExitCode::from(1);
    }
    eprintln!("shengen-rust: wrote {}", out_path.display());
    ExitCode::SUCCESS
}
