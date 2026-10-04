//! The historical strict flag validates explicit Elastic style expressions.
//! Both native search modes evaluate the admitted Boolean form over bounded files.

#[derive(Clone)]
enum Expr {
    Term(String),
    Phrase(String),
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Not(Box<Expr>),
}

#[derive(Clone)]
enum Token {
    Word(String),
    Phrase(String),
    And,
    Or,
    Not,
    Open,
    Close,
}

pub struct StrictQuery {
    expr: Expr,
    positive_terms: Vec<String>,
}

impl StrictQuery {
    /// Validate the pinned strict syntax and prepare a bounded Boolean query.
    pub fn parse(query: &str) -> Result<Self, String> {
        let query = query.trim();
        if query.is_empty() {
            return Err("strict query cannot be empty".into());
        }
        // Probe v0.6.0-rc339 query_validator.rs rejects vague multiword
        // input and unquoted snake/camel case before the normal query path.
        if query.split_whitespace().count() > 1
            && ![" AND ", " OR ", " NOT "]
                .iter()
                .any(|operator| query.contains(operator))
            && !(query.starts_with('"') && query.ends_with('"'))
        {
            return Err(
                "strict query requires explicit AND/OR operators or a quoted phrase".into(),
            );
        }
        let tokens = tokenize(query, true)?;
        Self::from_tokens(&tokens)
    }

    /// Ordinary keyword bags keep their existing admission. Reserved Boolean
    /// tokens, groups, and quotes opt into the same expression evaluator.
    pub fn for_search(query: &str) -> Result<Option<Self>, String> {
        let tokens = tokenize(query, false)?;
        if tokens.iter().all(|token| matches!(token, Token::Word(_))) {
            return Ok(None);
        }
        Self::from_tokens(&tokens).map(Some)
    }

    fn from_tokens(tokens: &[Token]) -> Result<Self, String> {
        let mut parser = Parser { tokens, at: 0 };
        let expr = parser.or_expr()?;
        if parser.at != tokens.len() {
            return Err("strict query has trailing syntax".into());
        }
        let mut positive_terms = Vec::new();
        collect_positive(&expr, false, &mut positive_terms);
        if positive_terms.is_empty() || positive_terms.len() > 32 {
            return Err("strict query requires 1-32 positive terms".into());
        }
        Ok(Self {
            expr,
            positive_terms,
        })
    }

    pub fn positive_terms(&self) -> &[String] {
        &self.positive_terms
    }

    pub fn matches(&self, source: &str, filename: Option<&str>) -> bool {
        eval(&self.expr, source, filename)
    }
}

fn tokenize(query: &str, strict: bool) -> Result<Vec<Token>, String> {
    let mut chars = query.chars().peekable();
    let mut tokens = Vec::new();
    while let Some(ch) = chars.next() {
        if ch.is_whitespace() {
            continue;
        }
        let token = match ch {
            '(' => Token::Open,
            ')' => Token::Close,
            '"' => {
                let mut phrase = String::new();
                let mut closed = false;
                for next in chars.by_ref() {
                    if next == '"' {
                        closed = true;
                        break;
                    }
                    phrase.push(next);
                }
                if !closed || phrase.is_empty() {
                    return Err("strict query has an empty or unclosed quote".into());
                }
                Token::Phrase(phrase.to_lowercase())
            }
            _ => {
                let mut word = ch.to_string();
                while let Some(next) = chars.peek() {
                    if next.is_whitespace() || matches!(next, '(' | ')' | '"') {
                        break;
                    }
                    word.push(*next);
                    chars.next();
                }
                match word.as_str() {
                    "AND" => Token::And,
                    "OR" => Token::Or,
                    "NOT" => Token::Not,
                    _ => {
                        let mixed_case = word.chars().any(char::is_uppercase)
                            && word.chars().any(char::is_lowercase);
                        if strict && (word.contains('_') || (word.len() > 1 && mixed_case)) {
                            return Err("strict query requires quotes around terms with underscores or mixed case".into());
                        }
                        Token::Word(word.to_lowercase())
                    }
                }
            }
        };
        tokens.push(token);
        if tokens.len() > 96 {
            return Err("strict query exceeds its token cap".into());
        }
    }
    Ok(tokens)
}

struct Parser<'a> {
    tokens: &'a [Token],
    at: usize,
}

impl Parser<'_> {
    fn or_expr(&mut self) -> Result<Expr, String> {
        let mut left = self.and_expr()?;
        while matches!(self.tokens.get(self.at), Some(Token::Or)) {
            self.at += 1;
            left = Expr::Or(Box::new(left), Box::new(self.and_expr()?));
        }
        Ok(left)
    }

    fn and_expr(&mut self) -> Result<Expr, String> {
        let mut left = self.unary()?;
        while matches!(self.tokens.get(self.at), Some(Token::And | Token::Not)) {
            if matches!(self.tokens.get(self.at), Some(Token::And)) {
                self.at += 1;
            }
            left = Expr::And(Box::new(left), Box::new(self.unary()?));
        }
        Ok(left)
    }

    fn unary(&mut self) -> Result<Expr, String> {
        if matches!(self.tokens.get(self.at), Some(Token::Not)) {
            self.at += 1;
            return Ok(Expr::Not(Box::new(self.unary()?)));
        }
        match self.tokens.get(self.at) {
            Some(Token::Word(word)) => {
                self.at += 1;
                Ok(Expr::Term(word.clone()))
            }
            Some(Token::Phrase(phrase)) => {
                self.at += 1;
                Ok(Expr::Phrase(phrase.clone()))
            }
            Some(Token::Open) => {
                self.at += 1;
                let expr = self.or_expr()?;
                if !matches!(self.tokens.get(self.at), Some(Token::Close)) {
                    return Err("strict query has an unclosed group".into());
                }
                self.at += 1;
                Ok(expr)
            }
            _ => Err("strict query has an invalid operand".into()),
        }
    }
}

fn collect_positive(expr: &Expr, negated: bool, terms: &mut Vec<String>) {
    match expr {
        Expr::Term(term) if !negated => push_term(terms, term),
        Expr::Phrase(phrase) if !negated => {
            for term in phrase.split(|ch: char| !ch.is_alphanumeric() && ch != '_') {
                if !term.is_empty() {
                    push_term(terms, term);
                }
            }
        }
        Expr::And(left, right) | Expr::Or(left, right) => {
            collect_positive(left, negated, terms);
            collect_positive(right, negated, terms);
        }
        Expr::Not(inner) => collect_positive(inner, !negated, terms),
        _ => {}
    }
}

fn push_term(terms: &mut Vec<String>, term: &str) {
    if !terms.iter().any(|known| known == term) {
        terms.push(term.to_owned());
    }
}

fn eval(expr: &Expr, source: &str, filename: Option<&str>) -> bool {
    match expr {
        Expr::Term(term) => [Some(source), filename].into_iter().flatten().any(|text| {
            text.to_lowercase()
                .split(|ch: char| !ch.is_alphanumeric() && ch != '_')
                .any(|word| word == term)
        }),
        Expr::Phrase(phrase) => [Some(source), filename]
            .into_iter()
            .flatten()
            .any(|text| text.to_lowercase().contains(phrase)),
        Expr::And(left, right) => eval(left, source, filename) && eval(right, source, filename),
        Expr::Or(left, right) => eval(left, source, filename) || eval(right, source, filename),
        Expr::Not(inner) => !eval(inner, source, filename),
    }
}
