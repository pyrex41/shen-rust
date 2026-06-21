//! Port of `MathPretty.swift` — renders shen-cas normal-form output (a bracketed
//! S-expression such as `[Times [Power x 2] 3]`) into human-readable math
//! (`3·x²`). Anything it doesn't recognise falls back to function notation
//! (`Head(arg, …)`) rather than leaking raw brackets, and `error:` strings pass
//! through untouched. Kept behaviourally in lock-step with the Swift version so
//! the iced app reads the same as the iOS/macOS apps.

#[derive(Debug)]
enum Node {
    Atom(String),
    List(Vec<Node>),
}

/// Precedence context, so we only parenthesise when needed.
#[derive(Clone, Copy, PartialEq)]
enum Ctx {
    Top,
    Sum,
    Product,
    Power,
    Fn,
}

/// Render one CAS normal-form string to human-readable math.
pub fn render(cas_output: &str) -> String {
    let t = cas_output.trim();
    if t.is_empty() || t.starts_with("error:") {
        return cas_output.to_string();
    }
    let tokens = tokenize(t);
    let mut pos = 0;
    match parse(&tokens, &mut pos) {
        Some(node) => emit(&node, Ctx::Top),
        None => cas_output.to_string(),
    }
}

// MARK: parse

fn tokenize(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for ch in s.chars() {
        match ch {
            '[' | ']' => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
                out.push(ch.to_string());
            }
            ' ' | '\t' | '\n' => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            _ => cur.push(ch),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn parse(tokens: &[String], pos: &mut usize) -> Option<Node> {
    let tok = tokens.get(*pos)?.clone();
    *pos += 1;
    if tok == "[" {
        let mut items = Vec::new();
        while let Some(next) = tokens.get(*pos) {
            if next == "]" {
                *pos += 1;
                break;
            }
            match parse(tokens, pos) {
                Some(n) => items.push(n),
                None => break,
            }
        }
        return Some(Node::List(items));
    }
    if tok == "]" {
        return None;
    }
    Some(Node::Atom(tok))
}

// MARK: emit

fn emit(node: &Node, parent: Ctx) -> String {
    let xs = match node {
        Node::Atom(a) => return a.clone(),
        Node::List(xs) => xs,
    };

    // Infix binary forms the CAS emits directly, e.g. [3 / 2].
    if xs.len() == 3 {
        if let Node::Atom(op) = &xs[1] {
            if matches!(op.as_str(), "/" | "+" | "-" | "*" | "^") {
                let sep = if op == "/" {
                    "/".to_string()
                } else {
                    format!(" {op} ")
                };
                let s = format!(
                    "{}{}{}",
                    emit(&xs[0], Ctx::Product),
                    sep,
                    emit(&xs[2], Ctx::Product)
                );
                return if op == "/" && parent == Ctx::Power {
                    format!("({s})")
                } else {
                    s
                };
            }
        }
    }

    let head = match xs.first() {
        Some(Node::Atom(h)) => h.as_str(),
        _ => return generic(xs),
    };
    let args: Vec<&Node> = xs.iter().skip(1).collect();
    let arg0 = |c: Ctx| args.first().map(|a| emit(a, c)).unwrap_or_default();

    match head {
        "Plus" => wrap_if(parent == Ctx::Product || parent == Ctx::Power, sum(&args)),
        "Times" => product(&args, parent),
        "Power" => power(&args, parent),
        "List" => format!(
            "{{{}}}",
            args.iter()
                .map(|a| emit(a, Ctx::Top))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        "Exp" => wrap_if(parent == Ctx::Power, format!("e^{}", arg0(Ctx::Power))),
        "Log" => format!("ln({})", arg0(Ctx::Fn)),
        "Sqrt" => format!("√({})", arg0(Ctx::Fn)),
        _ if is_function(head) && args.len() == 1 => {
            format!("{}({})", head.to_lowercase(), emit(args[0], Ctx::Fn))
        }
        _ => generic(xs),
    }
}

fn is_function(h: &str) -> bool {
    matches!(
        h,
        "Sin" | "Cos"
            | "Tan"
            | "Sec"
            | "Csc"
            | "Cot"
            | "Sinh"
            | "Cosh"
            | "Tanh"
            | "Arcsin"
            | "Arccos"
            | "Arctan"
            | "Abs"
    )
}

fn sum(args: &[&Node]) -> String {
    let mut result = String::new();
    for (i, term) in args.iter().enumerate() {
        let s = emit(term, Ctx::Sum);
        if i == 0 {
            result = s;
        } else if let Some(rest) = s.strip_prefix('-') {
            result.push_str(" - ");
            result.push_str(rest.trim());
        } else {
            result.push_str(" + ");
            result.push_str(&s);
        }
    }
    result
}

fn product(args: &[&Node], parent: Ctx) -> String {
    let mut negative = false;
    let mut factors: Vec<&Node> = Vec::new();
    for &f in args {
        if let Node::Atom(a) = f {
            if a == "-1" {
                negative = !negative;
                continue;
            }
        }
        factors.push(f);
    }
    // numeric / fraction coefficients first, for natural reading (3·x²).
    let mut ordered: Vec<&Node> = Vec::with_capacity(factors.len());
    for &f in &factors {
        if is_numeric(f) {
            ordered.push(f);
        }
    }
    for &f in &factors {
        if !is_numeric(f) {
            ordered.push(f);
        }
    }
    let pieces: Vec<String> = ordered
        .iter()
        .map(|&f| {
            let s = emit(f, Ctx::Product);
            // bracket fractions used as coefficients: (1/3)·x³
            if let Node::List(l) = f {
                if l.len() == 3 {
                    if let Node::Atom(op) = &l[1] {
                        if op == "/" {
                            return format!("({s})");
                        }
                    }
                }
            }
            s
        })
        .collect();
    let mut body = if pieces.is_empty() {
        "1".to_string()
    } else {
        pieces.join("·")
    };
    if negative {
        body = format!("-{body}");
    }
    wrap_if(parent == Ctx::Power, body)
}

fn power(args: &[&Node], parent: Ctx) -> String {
    if args.len() != 2 {
        let inner = args
            .iter()
            .map(|a| emit(a, Ctx::Top))
            .collect::<Vec<_>>()
            .join(", ");
        return format!("Power({inner})");
    }
    let base_str = emit(args[0], Ctx::Power);
    if let Node::Atom(e) = args[1] {
        if let Ok(n) = e.parse::<i64>() {
            if n == 0 {
                return "1".to_string();
            }
            if n == 1 {
                return base_str;
            }
            if n < 0 {
                let denom = if n == -1 {
                    base_str
                } else {
                    format!("{base_str}{}", superscript(-n))
                };
                return wrap_if(parent == Ctx::Product, format!("1/{denom}"));
            }
            return format!("{base_str}{}", superscript(n));
        }
    }
    // non-integer exponent (fraction, symbol): base^(exp)
    format!("{base_str}^({})", emit(args[1], Ctx::Power))
}

fn is_numeric(n: &Node) -> bool {
    match n {
        Node::Atom(a) => a.parse::<f64>().is_ok(),
        // a fraction like [1 / 2]
        Node::List(l) => l.len() == 3 && matches!(&l[1], Node::Atom(op) if op == "/"),
    }
}

/// Fallback: render an unrecognised list as Head(arg, …) function notation.
fn generic(xs: &[Node]) -> String {
    match xs.first() {
        Some(Node::Atom(head)) => {
            let args: Vec<String> = xs.iter().skip(1).map(|a| emit(a, Ctx::Top)).collect();
            if args.is_empty() {
                head.clone()
            } else {
                format!("{head}({})", args.join(", "))
            }
        }
        _ => format!(
            "({})",
            xs.iter()
                .map(|a| emit(a, Ctx::Top))
                .collect::<Vec<_>>()
                .join(" ")
        ),
    }
}

fn wrap_if(cond: bool, s: String) -> String {
    if cond {
        format!("({s})")
    } else {
        s
    }
}

fn superscript(n: i64) -> String {
    n.to_string()
        .chars()
        .map(|c| match c {
            '0' => '⁰',
            '1' => '¹',
            '2' => '²',
            '3' => '³',
            '4' => '⁴',
            '5' => '⁵',
            '6' => '⁶',
            '7' => '⁷',
            '8' => '⁸',
            '9' => '⁹',
            '-' => '⁻',
            other => other,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::render;

    #[test]
    fn matches_swift_mathpretty() {
        let cases = [
            ("[Cos x]", "cos(x)"),
            ("[Times [Sin x] -1]", "-sin(x)"),
            ("[Power [Sec x] 2]", "sec(x)²"),
            ("[Times [Power x 2] 3]", "3·x²"),
            ("[Times [Power x 3] [1 / 3]]", "(1/3)·x³"),
            ("[Plus [Power x 2] [Times x 2] 1]", "x² + 2·x + 1"),
            ("[Times [Plus x 1] [Plus x -1]]", "(x + 1)·(x - 1)"),
            ("[List 2 -2]", "{2, -2}"),
            ("[3 / 2]", "3/2"),
            ("1024", "1024"),
            ("[Log x]", "ln(x)"),
            ("[Power x -1]", "1/x"),
            ("[Exp x]", "e^x"),
            ("error: bad char .", "error: bad char ."),
        ];
        for (input, want) in cases {
            assert_eq!(render(input), want, "render({input:?})");
        }
    }
}
