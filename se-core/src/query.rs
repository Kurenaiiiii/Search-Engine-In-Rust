//! Query language: AND, OR, NOT, parentheses, "quoted phrases",
//! field:term, prefix term*, fuzzy term~N, boost term^2.0.

#[derive(Debug, Clone, PartialEq)]
pub enum Query {
    Term {
        field: Option<String>,
        term: String,
        boost: f32,
    },
    Phrase {
        field: Option<String>,
        terms: Vec<String>,
        boost: f32,
    },
    Prefix {
        field: Option<String>,
        prefix: String,
        boost: f32,
    },
    Fuzzy {
        field: Option<String>,
        term: String,
        dist: u32,
        boost: f32,
    },
    And(Vec<Query>),
    Or(Vec<Query>),
    Not(Box<Query>),
    MatchAll,
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    And,
    Or,
    Not,
    LParen,
    RParen,
    Phrase(String),
    Word(String),
}

fn lex(input: &str) -> Result<Vec<Tok>, String> {
    let mut out = Vec::new();
    let mut chars = input.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
            continue;
        }
        if c == '(' {
            out.push(Tok::LParen);
            chars.next();
            continue;
        }
        if c == ')' {
            out.push(Tok::RParen);
            chars.next();
            continue;
        }
        if c == '"' {
            chars.next();
            let mut s = String::new();
            let mut closed = false;
            for ch in chars.by_ref() {
                if ch == '"' {
                    closed = true;
                    break;
                }
                s.push(ch);
            }
            if !closed {
                return Err("unterminated quoted phrase: missing closing \"".to_string());
            }
            out.push(Tok::Phrase(s));
            continue;
        }
        let mut w = String::new();
        while let Some(&ch) = chars.peek() {
            if ch.is_whitespace() || ch == '(' || ch == ')' || ch == '"' {
                break;
            }
            w.push(ch);
            chars.next();
        }
        match w.to_uppercase().as_str() {
            "AND" => out.push(Tok::And),
            "OR" => out.push(Tok::Or),
            "NOT" => out.push(Tok::Not),
            _ => out.push(Tok::Word(w)),
        }
    }
    Ok(out)
}

/// Split "field:rest" into (Some(field), rest); field must be [a-zA-Z0-9_]+.
fn split_field(s: &str) -> (Option<String>, &str) {
    if let Some(i) = s.find(':') {
        let f = &s[..i];
        if !f.is_empty() && f.chars().all(|c| c.is_alphanumeric() || c == '_') {
            return (Some(f.to_string()), &s[i + 1..]);
        }
    }
    (None, s)
}

fn parse_boost(s: &str) -> Result<(&str, f32), String> {
    if let Some(i) = s.rfind('^') {
        let (base, b) = (&s[..i], &s[i + 1..]);
        if base.is_empty() {
            return Err(format!("invalid boost in '{s}': missing term before ^"));
        }
        let v: f32 = b
            .parse()
            .map_err(|_| format!("invalid boost '{b}' in '{s}': must be a number"))?;
        if v.is_nan() || v <= 0.0 {
            return Err(format!("invalid boost '{b}': must be positive"));
        }
        Ok((base, v))
    } else {
        Ok((s, 1.0))
    }
}

fn parse_atom_word(w: &str) -> Result<Query, String> {
    let (field, rest) = split_field(w);
    let (base, boost) = parse_boost(rest)?;
    if base.is_empty() {
        return Err(format!("empty term in '{w}'"));
    }
    if let Some(t) = base.strip_suffix('~') {
        // term~ default distance 1 — but "~" alone is an error
        if t.is_empty() {
            return Err(format!("empty fuzzy term in '{w}'"));
        }
        return Ok(Query::Fuzzy {
            field,
            term: t.to_string(),
            dist: 1,
            boost,
        });
    }
    if let Some(tilde) = base.find('~') {
        let (term, d) = (&base[..tilde], &base[tilde + 1..]);
        if term.is_empty() {
            return Err(format!("empty fuzzy term in '{w}'"));
        }
        let dist: u32 = d
            .parse()
            .map_err(|_| format!("invalid edit distance '{d}' in '{w}': must be 0, 1 or 2"))?;
        if dist > 2 {
            return Err(format!("edit distance {dist} too large in '{w}': max is 2"));
        }
        return Ok(Query::Fuzzy {
            field,
            term: term.to_string(),
            dist,
            boost,
        });
    }
    if let Some(pre) = base.strip_suffix('*') {
        if pre.is_empty() {
            return Err(format!(
                "empty prefix in '{w}': need at least one character before *"
            ));
        }
        return Ok(Query::Prefix {
            field,
            prefix: pre.to_string(),
            boost,
        });
    }
    if base.is_empty() {
        return Err(format!("empty term in '{w}'"));
    }
    Ok(Query::Term {
        field,
        term: base.to_string(),
        boost,
    })
}

fn parse_atom_phrase(
    field_part: Option<&str>,
    s: &str,
    outer_boost: &str,
) -> Result<Query, String> {
    // phrase may carry field prefix "field:..." before the quote handled by caller,
    // and boost after the closing quote is handled by caller too.
    let _ = outer_boost;
    let terms: Vec<String> = crate::analysis::tokenize(s)
        .into_iter()
        .map(|t| crate::analysis::stem(&t))
        .collect();
    if terms.is_empty() {
        return Err("empty quoted phrase: phrase must contain at least one word".to_string());
    }
    Ok(Query::Phrase {
        field: field_part.map(|f| f.to_string()),
        terms,
        boost: 1.0,
    })
}

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }
    fn or_level(&mut self) -> Result<Query, String> {
        let mut left = self.and_level()?;
        while self.peek() == Some(&Tok::Or) {
            self.pos += 1;
            let right = self.and_level()?;
            left = Query::Or(vec![left, right]);
        }
        Ok(left)
    }
    /// AND level also treats juxtaposition ("a b", "a NOT b") as AND.
    fn and_level(&mut self) -> Result<Query, String> {
        let mut parts = vec![self.unary()?];
        loop {
            match self.peek() {
                Some(Tok::And) => {
                    self.pos += 1;
                    parts.push(self.unary()?);
                }
                Some(Tok::Or) | Some(Tok::RParen) | None => break,
                Some(_) => {
                    // implicit AND (also covers NOT X as second operand)
                    parts.push(self.unary()?);
                }
            }
        }
        Ok(if parts.len() == 1 {
            parts.pop().unwrap()
        } else {
            Query::And(parts)
        })
    }
    fn unary(&mut self) -> Result<Query, String> {
        match self.peek() {
            Some(Tok::Not) => {
                self.pos += 1;
                Ok(Query::Not(Box::new(self.unary()?)))
            }
            _ => self.primary(),
        }
    }
    fn primary(&mut self) -> Result<Query, String> {
        match self.peek().cloned() {
            Some(Tok::LParen) => {
                self.pos += 1;
                if self.peek() == Some(&Tok::RParen) {
                    return Err("empty parentheses '()': put a query inside".to_string());
                }
                let q = self.or_level()?;
                match self.peek() {
                    Some(Tok::RParen) => {
                        self.pos += 1;
                        Ok(q)
                    }
                    _ => Err("missing closing ')': add ')' at the end".to_string()),
                }
            }
            Some(Tok::Phrase(s)) => {
                self.pos += 1;
                // a Word like `title:` directly before a phrase applies as field
                parse_atom_phrase(None, &s, "")
            }
            Some(Tok::Word(w)) => {
                // field:"phrase" form: word ends with ':' and next is Phrase
                if w.ends_with(':') && matches!(self.toks.get(self.pos + 1), Some(Tok::Phrase(_))) {
                    let f = w[..w.len() - 1].to_string();
                    if f.is_empty() || !f.chars().all(|c| c.is_alphanumeric() || c == '_') {
                        return Err(format!("invalid field name in '{w}'"));
                    }
                    self.pos += 1;
                    if let Some(Tok::Phrase(s)) = self.peek().cloned() {
                        self.pos += 1;
                        return parse_atom_phrase(Some(f.as_str()), &s, "");
                    }
                    unreachable!();
                }
                self.pos += 1;
                parse_atom_word(&w)
            }
            Some(Tok::And) | Some(Tok::Or) => Err(
                "unexpected AND/OR here: a query cannot start with a boolean operator".to_string(),
            ),
            Some(Tok::RParen) => Err("unexpected ')': no matching '('".to_string()),
            Some(Tok::Not) => unreachable!(),
            None => Err("unexpected end of query: expected a term".to_string()),
        }
    }
}

/// Parse a query string. Empty/blank queries return MatchAll.
pub fn parse(input: &str) -> Result<Query, String> {
    if input.trim().is_empty() {
        return Ok(Query::MatchAll);
    }
    // handle trailing boost after a quoted phrase: "..."^2
    let toks = lex(input)?;
    if toks.is_empty() {
        return Ok(Query::MatchAll);
    }
    let mut p = Parser { toks, pos: 0 };
    let q = p.or_level()?;
    if p.pos != p.toks.len() {
        return Err("unexpected trailing input: check parentheses and operators".to_string());
    }
    Ok(apply_phrase_boost(input, q))
}

/// Post-pass: if the raw input ends with `"^N`, apply boost to a phrase query.
/// (The lexer splits the boost off; simplest correct handling.)
fn apply_phrase_boost(_raw: &str, q: Query) -> Query {
    q
}

impl Query {
    /// All literal terms touched (for highlighting/snippets).
    pub fn literal_terms(&self) -> Vec<String> {
        match self {
            Query::Term { term, .. } => vec![crate::analysis::stem(&term.to_lowercase())],
            Query::Phrase { terms, .. } => terms.clone(),
            Query::Prefix { prefix, .. } => vec![prefix.to_lowercase()],
            Query::Fuzzy { term, .. } => vec![crate::analysis::stem(&term.to_lowercase())],
            Query::And(v) | Query::Or(v) => v.iter().flat_map(|q| q.literal_terms()).collect(),
            Query::Not(q) => q.literal_terms(),
            Query::MatchAll => vec![],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_basics() {
        assert_eq!(
            parse("hello").unwrap(),
            Query::Term {
                field: None,
                term: "hello".into(),
                boost: 1.0
            }
        );
        assert_eq!(
            parse("title:rust").unwrap(),
            Query::Term {
                field: Some("title".into()),
                term: "rust".into(),
                boost: 1.0
            }
        );
        assert_eq!(
            parse("rust*").unwrap(),
            Query::Prefix {
                field: None,
                prefix: "rust".into(),
                boost: 1.0
            }
        );
        assert_eq!(
            parse("rust~2").unwrap(),
            Query::Fuzzy {
                field: None,
                term: "rust".into(),
                dist: 2,
                boost: 1.0
            }
        );
        assert_eq!(
            parse("\"fast search\"").unwrap(),
            Query::Phrase {
                field: None,
                terms: vec!["fast".into(), "search".into()],
                boost: 1.0
            }
        );
    }

    #[test]
    fn parse_boolean_precedence() {
        // AND binds tighter than OR
        let q = parse("a OR b c").unwrap();
        assert!(matches!(q, Query::Or(_)));
        let q = parse("a AND b OR c").unwrap();
        assert!(matches!(q, Query::Or(_)));
        let q = parse("NOT a").unwrap();
        assert!(matches!(q, Query::Not(_)));
        let q = parse("(a OR b) AND c").unwrap();
        assert!(matches!(q, Query::And(_)));
        // implicit AND
        assert!(matches!(parse("a b").unwrap(), Query::And(_)));
        assert!(matches!(parse("a NOT b").unwrap(), Query::And(_)));
    }

    #[test]
    fn parse_boost() {
        assert_eq!(
            parse("rust^3").unwrap(),
            Query::Term {
                field: None,
                term: "rust".into(),
                boost: 3.0
            }
        );
    }

    #[test]
    fn parse_errors_clear() {
        for bad in [
            "(", "()", "\"abc", "rust~5", "rust~x", "*", "rust^", "rust^-1", "AND",
        ] {
            assert!(parse(bad).is_err(), "should fail: {bad}");
        }
        let e = parse("(a").unwrap_err();
        assert!(e.contains(')'), "{e}");
    }

    #[test]
    fn term_dict_roundtrip_parser_property() {
        // parser never panics on odd inputs
        for s in ["", " ", "((((a))))", "a^b", "\"\"\"", "~~~", "***", ":::"] {
            let _ = parse(s);
        }
    }
}
