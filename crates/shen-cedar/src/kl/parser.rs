//! KL s-expression parser.
//!
//! KL files are pure s-expressions:
//! * parentheses for lists,
//! * double-quoted strings (backslash escapes a few common chars, but the
//!   kernel mostly uses literal newlines inside strings — supported),
//! * everything else split on ASCII whitespace is an atom.
//!
//! Atoms are interpreted as int, then float, then `true`/`false`, then
//! plain symbol. Numbers in KL use the standard syntax (`-5`, `3.14`,
//! `1e-3`).

use std::rc::Rc;

use crate::error::{ShenError, ShenResult};
use crate::kl::ast::KlExpr;
use crate::symbol::Interner;

/// Parse a full source string into a vector of top-level expressions.
pub fn parse_all(src: &str, interner: &mut Interner) -> ShenResult<Vec<KlExpr>> {
    let mut p = Parser::new(src, interner);
    let mut out = Vec::new();
    while p.skip_ws_and_comments() {
        out.push(p.parse_expr()?);
    }
    Ok(out)
}

/// Parse a single top-level expression. Returns an error if the source has
/// no expression at all or has trailing content after the first form.
pub fn parse_one(src: &str, interner: &mut Interner) -> ShenResult<KlExpr> {
    let mut p = Parser::new(src, interner);
    if !p.skip_ws_and_comments() {
        return Err(ShenError::new("empty input"));
    }
    let expr = p.parse_expr()?;
    if p.skip_ws_and_comments() {
        return Err(ShenError::new("trailing content after expression"));
    }
    Ok(expr)
}

struct Parser<'a, 'i> {
    src: &'a [u8],
    pos: usize,
    interner: &'i mut Interner,
}

impl<'a, 'i> Parser<'a, 'i> {
    fn new(src: &'a str, interner: &'i mut Interner) -> Self {
        Self {
            src: src.as_bytes(),
            pos: 0,
            interner,
        }
    }

    /// Advance past whitespace and Shen-style comments. Returns true if
    /// there's at least one character of real input left.
    fn skip_ws_and_comments(&mut self) -> bool {
        loop {
            while self.pos < self.src.len() && self.src[self.pos].is_ascii_whitespace() {
                self.pos += 1;
            }
            // Block comment: \* ... *\
            if self.pos + 1 < self.src.len()
                && self.src[self.pos] == b'\\'
                && self.src[self.pos + 1] == b'*'
            {
                self.pos += 2;
                while self.pos + 1 < self.src.len()
                    && !(self.src[self.pos] == b'*' && self.src[self.pos + 1] == b'\\')
                {
                    self.pos += 1;
                }
                if self.pos + 1 < self.src.len() {
                    self.pos += 2;
                }
                continue;
            }
            // Line comment: \\ ...\n (Shen source convention; rarely in KL
            // but harmless to support).
            if self.pos + 1 < self.src.len()
                && self.src[self.pos] == b'\\'
                && self.src[self.pos + 1] == b'\\'
            {
                self.pos += 2;
                while self.pos < self.src.len() && self.src[self.pos] != b'\n' {
                    self.pos += 1;
                }
                continue;
            }
            break;
        }
        self.pos < self.src.len()
    }

    fn parse_expr(&mut self) -> ShenResult<KlExpr> {
        let c = self.src[self.pos];
        if c == b'(' {
            self.parse_list()
        } else if c == b'[' {
            self.parse_bracket_list()
        } else if c == b'"' {
            self.parse_string()
        } else if c == b')' {
            Err(ShenError::new(format!(
                "unexpected ')' at byte offset {}",
                self.pos
            )))
        } else if c == b']' {
            Err(ShenError::new(format!(
                "unexpected ']' at byte offset {}",
                self.pos
            )))
        } else if c == b'{' || c == b'}' {
            // Standalone brace tokens: in Shen surface syntax, `{...}`
            // wraps a type signature in a `(define …)` form. The kernel
            // reader emits the brace characters as the symbols `{` and
            // `}` (interned), and the macro expander looks for them.
            // Our REPL parser handled them as part of atoms previously,
            // mangling type signatures like `{a --> b}` into `{a` ... `b}`.
            self.pos += 1;
            let name = if c == b'{' { "{" } else { "}" };
            Ok(KlExpr::Sym(self.interner.intern(name)))
        } else {
            self.parse_atom()
        }
    }

    fn parse_list(&mut self) -> ShenResult<KlExpr> {
        // consume '('
        self.pos += 1;
        let mut items = Vec::new();
        loop {
            if !self.skip_ws_and_comments() {
                return Err(ShenError::new("unclosed '('"));
            }
            if self.src[self.pos] == b')' {
                self.pos += 1;
                return Ok(if items.is_empty() {
                    KlExpr::Nil
                } else {
                    KlExpr::App(items.into())
                });
            }
            items.push(self.parse_expr()?);
        }
    }

    /// Shen surface syntax: `[a b c]` desugars to `(cons a (cons b (cons c ())))`,
    /// `[a | b]` to `(cons a b)`, `[a b | c]` to `(cons a (cons b c))`, `[]`
    /// to `()`. Required because user-typed Shen code at the REPL never
    /// goes through the kernel's reader — our parser is the only thing
    /// that converts source to AST.
    fn parse_bracket_list(&mut self) -> ShenResult<KlExpr> {
        // consume '['
        self.pos += 1;
        let mut items: Vec<KlExpr> = Vec::new();
        let mut tail: Option<KlExpr> = None;
        loop {
            if !self.skip_ws_and_comments() {
                return Err(ShenError::new("unclosed '['"));
            }
            let c = self.src[self.pos];
            if c == b']' {
                self.pos += 1;
                break;
            }
            // `|` separator: the next expression is the tail of an
            // improper list, then we expect ']'.
            if c == b'|' && self.is_lone_pipe() {
                self.pos += 1;
                if !self.skip_ws_and_comments() {
                    return Err(ShenError::new("expected expression after '|'"));
                }
                tail = Some(self.parse_expr()?);
                if !self.skip_ws_and_comments() {
                    return Err(ShenError::new("expected ']' after improper list tail"));
                }
                if self.src[self.pos] != b']' {
                    return Err(ShenError::new(format!(
                        "expected ']' after '|' tail at byte offset {}",
                        self.pos
                    )));
                }
                self.pos += 1;
                break;
            }
            items.push(self.parse_expr()?);
        }
        let cons_sym = self.interner.intern("cons");
        let mut result = tail.unwrap_or(KlExpr::Nil);
        for item in items.into_iter().rev() {
            let triple: Rc<[KlExpr]> = Rc::from(vec![KlExpr::Sym(cons_sym), item, result]);
            result = KlExpr::App(triple);
        }
        Ok(result)
    }

    /// `|` is a standalone separator only when surrounded by whitespace
    /// or brackets — otherwise it could be part of an atom name (e.g.
    /// `|hello|` is sometimes used as a quoted symbol in Lisp dialects).
    /// For Shen, `|` inside `[...]` is always the cons separator when
    /// alone; that's the only case we need to detect.
    fn is_lone_pipe(&self) -> bool {
        if self.pos >= self.src.len() || self.src[self.pos] != b'|' {
            return false;
        }
        if self.pos + 1 >= self.src.len() {
            return true;
        }
        let next = self.src[self.pos + 1];
        next.is_ascii_whitespace() || next == b']' || next == b'(' || next == b'['
    }

    fn parse_string(&mut self) -> ShenResult<KlExpr> {
        let open_offset = self.pos;
        // consume opening '"'
        self.pos += 1;
        let start = self.pos;
        while self.pos < self.src.len() && self.src[self.pos] != b'"' {
            self.pos += 1;
        }
        if self.pos >= self.src.len() {
            return Err(ShenError::new(format!(
                "unterminated string opened at byte offset {open_offset}"
            )));
        }
        // Shen .kl strings have no backslash escapes — every byte between
        // the delimiters is part of the content verbatim. Embedded
        // newlines and backslashes are common in error messages.
        let bytes = &self.src[start..self.pos];
        let text = std::str::from_utf8(bytes).map_err(|_| {
            ShenError::new(format!(
                "non-UTF-8 string body opened at byte offset {open_offset}"
            ))
        })?;
        // consume closing '"'
        self.pos += 1;
        Ok(KlExpr::Str(Rc::from(text)))
    }

    fn parse_atom(&mut self) -> ShenResult<KlExpr> {
        let start = self.pos;
        while self.pos < self.src.len() {
            let c = self.src[self.pos];
            if c.is_ascii_whitespace()
                || c == b'('
                || c == b')'
                || c == b'['
                || c == b']'
                || c == b'{'
                || c == b'}'
                || c == b'"'
            {
                break;
            }
            self.pos += 1;
        }
        let text = std::str::from_utf8(&self.src[start..self.pos])
            .map_err(|_| ShenError::new("non-UTF-8 atom"))?;
        Ok(atom_from_text(text, self.interner))
    }
}

fn atom_from_text(text: &str, interner: &mut Interner) -> KlExpr {
    if text == "true" {
        KlExpr::Bool(true)
    } else if text == "false" {
        KlExpr::Bool(false)
    } else if let Ok(n) = text.parse::<i64>() {
        KlExpr::Int(n)
    } else if let Ok(x) = text.parse::<f64>() {
        // Only accept as float if there's at least a dot or exponent;
        // otherwise things like "1" would have already parsed as int.
        // f64::parse_str accepts plain integers too, so we get here only
        // when int parse failed (out of range).
        if text.contains('.') || text.contains('e') || text.contains('E') {
            KlExpr::Float(x)
        } else {
            // Integer literal that overflows i64 — keep as float to avoid
            // a parse error.
            KlExpr::Float(x)
        }
    } else {
        KlExpr::Sym(interner.intern(text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_one_str(src: &str) -> KlExpr {
        let mut i = Interner::new();
        parse_one(src, &mut i).unwrap()
    }

    #[test]
    fn atoms() {
        assert!(matches!(parse_one_str("123"), KlExpr::Int(123)));
        assert!(matches!(parse_one_str("-5"), KlExpr::Int(-5)));
        if let KlExpr::Float(x) = parse_one_str("2.5") {
            assert!((x - 2.5).abs() < 1e-9);
        } else {
            panic!("expected float");
        }
        assert!(matches!(parse_one_str("true"), KlExpr::Bool(true)));
        assert!(matches!(parse_one_str("false"), KlExpr::Bool(false)));
    }

    #[test]
    fn empty_list_is_nil() {
        assert!(matches!(parse_one_str("()"), KlExpr::Nil));
    }

    #[test]
    fn nested_app() {
        let e = parse_one_str("(+ 1 (* 2 3))");
        if let KlExpr::App(items) = e {
            assert_eq!(items.len(), 3);
        } else {
            panic!("expected app");
        }
    }

    #[test]
    fn shen_symbol_with_special_chars() {
        let mut i = Interner::new();
        let e = parse_one("(shen.<rule> element? @p)", &mut i).unwrap();
        if let KlExpr::App(items) = e {
            assert_eq!(items.len(), 3);
            for item in items.iter() {
                assert!(matches!(item, KlExpr::Sym(_)));
            }
        } else {
            panic!("expected app");
        }
    }

    #[test]
    fn string_no_escapes() {
        // KL strings have no backslash escapes: a literal backslash in
        // the source is a literal backslash in the value, and the first
        // closing quote ends the string.
        let s = parse_one_str("\"line1\\nline2\"");
        if let KlExpr::Str(s) = s {
            assert_eq!(&*s, "line1\\nline2");
        } else {
            panic!("expected str");
        }
    }

    #[test]
    fn string_with_embedded_newline() {
        // Real newline characters inside strings are common in kernel
        // error messages and must be preserved verbatim.
        let s = parse_one_str("\"line1\nline2\"");
        if let KlExpr::Str(s) = s {
            assert_eq!(&*s, "line1\nline2");
        } else {
            panic!("expected str");
        }
    }

    #[test]
    fn empty_string() {
        let s = parse_one_str("\"\"");
        if let KlExpr::Str(s) = s {
            assert_eq!(&*s, "");
        } else {
            panic!("expected str");
        }
    }

    #[test]
    fn line_comment() {
        let mut i = Interner::new();
        let v = parse_all("\\\\ this is a comment\n(+ 1 2)\n", &mut i).unwrap();
        assert_eq!(v.len(), 1);
    }

    #[test]
    fn block_comment() {
        let mut i = Interner::new();
        let v = parse_all("\\* block *\\\n(+ 1 2)", &mut i).unwrap();
        assert_eq!(v.len(), 1);
    }

    #[test]
    fn parse_all_multiple_forms() {
        let mut i = Interner::new();
        let v = parse_all("(defun a () 1)\n(defun b () 2)", &mut i).unwrap();
        assert_eq!(v.len(), 2);
    }

    #[test]
    fn unclosed_paren_errors() {
        let mut i = Interner::new();
        assert!(parse_one("(+ 1 2", &mut i).is_err());
    }
}
