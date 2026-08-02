//! Turning spec text into tokens.
//!
//! # Why a lexer instead of splitting on punctuation
//!
//! The previous parser split statements on `;` and lists on `,`. That works
//! until a value *contains* one — and a description is prose, so it contains
//! semicolons routinely:
//!
//! ```text
//! description = "Use when summarizing; especially long files.";
//! ```
//!
//! Splitting on `;` cuts that in half and reports a confusing error about the
//! description not being a quoted string. A path containing a comma broke the
//! capability list the same way. Both were real bugs, not hypotheticals, and
//! neither is fixable by being cleverer about splitting: a separator inside a
//! string is only distinguishable from a separator between values by tracking
//! whether you are inside a string, which is what a lexer is.
//!
//! It also buys positions. "malformed spec" with no location is a poor error for
//! a file someone is editing by hand; every token here carries a line and column
//! so the parser can point at the problem.

/// A token's position in the source, for error messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    /// 1-based line.
    pub line: u32,
    /// 1-based column.
    pub column: u32,
}

impl std::fmt::Display for Span {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "line {}, column {}", self.line, self.column)
    }
}

/// What a token is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tok {
    /// A bare word: `spec`, `description`, `Ollama`, `Local_only`.
    Ident(String),
    /// A quoted string, with escapes already resolved.
    Str(String),
    /// `=`
    Equals,
    /// `{`
    OpenBrace,
    /// `}`
    CloseBrace,
    /// `[`
    OpenBracket,
    /// `]`
    CloseBracket,
    /// `,`
    Comma,
    /// `;`
    Semicolon,
}

impl Tok {
    /// How to name this in an error message.
    pub fn describe(&self) -> String {
        match self {
            Tok::Ident(name) => format!("`{name}`"),
            Tok::Str(_) => "a quoted string".into(),
            Tok::Equals => "`=`".into(),
            Tok::OpenBrace => "`{`".into(),
            Tok::CloseBrace => "`}`".into(),
            Tok::OpenBracket => "`[`".into(),
            Tok::CloseBracket => "`]`".into(),
            Tok::Comma => "`,`".into(),
            Tok::Semicolon => "`;`".into(),
        }
    }
}

/// A token and where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    /// The token.
    pub tok: Tok,
    /// Where it started.
    pub span: Span,
}

/// Why lexing stopped.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum LexError {
    /// A string had no closing quote.
    #[error("unterminated string starting at {span}")]
    UnterminatedString {
        /// Where the string began.
        span: Span,
    },
    /// A character that cannot begin any token.
    #[error("unexpected character `{ch}` at {span}")]
    UnexpectedChar {
        /// The offending character.
        ch: char,
        /// Where it is.
        span: Span,
    },
    /// A backslash escape this format does not define.
    #[error("unknown escape `\\{ch}` at {span}")]
    UnknownEscape {
        /// The character after the backslash.
        ch: char,
        /// Where the escape is.
        span: Span,
    },
}

/// Tokenize spec source.
///
/// Comments run from `#` to end of line, and whitespace is insignificant.
pub fn lex(src: &str) -> Result<Vec<Token>, LexError> {
    let mut tokens = Vec::new();
    let mut chars = src.chars().peekable();
    let (mut line, mut column) = (1u32, 1u32);

    // Consuming through a closure keeps line and column correct in one place;
    // tracking them at each call site is how they drift.
    macro_rules! bump {
        () => {{
            let c = chars.next();
            match c {
                Some('\n') => {
                    line += 1;
                    column = 1;
                }
                Some(_) => column += 1,
                None => {}
            }
            c
        }};
    }

    while let Some(&c) = chars.peek() {
        let span = Span { line, column };

        match c {
            c if c.is_whitespace() => {
                bump!();
            }
            '#' => {
                while let Some(&c) = chars.peek() {
                    if c == '\n' {
                        break;
                    }
                    bump!();
                }
            }
            '"' => {
                bump!();
                let mut value = String::new();
                loop {
                    match bump!() {
                        None => return Err(LexError::UnterminatedString { span }),
                        Some('"') => break,
                        Some('\\') => {
                            let escape_span = Span { line, column };
                            match bump!() {
                                Some('"') => value.push('"'),
                                Some('\\') => value.push('\\'),
                                Some('n') => value.push('\n'),
                                Some('t') => value.push('\t'),
                                Some(other) => {
                                    return Err(LexError::UnknownEscape {
                                        ch: other,
                                        span: escape_span,
                                    })
                                }
                                None => return Err(LexError::UnterminatedString { span }),
                            }
                        }
                        // A newline inside a string is allowed: descriptions
                        // wrap, and requiring an escape for that would make the
                        // common case awkward.
                        Some(other) => value.push(other),
                    }
                }
                tokens.push(Token {
                    tok: Tok::Str(value),
                    span,
                });
            }
            c if c.is_alphanumeric() || c == '_' || c == '.' || c == '/' || c == '-' => {
                let mut word = String::new();
                while let Some(&c) = chars.peek() {
                    if c.is_alphanumeric() || matches!(c, '_' | '.' | '/' | '-' | ':') {
                        word.push(c);
                        bump!();
                    } else {
                        break;
                    }
                }
                tokens.push(Token {
                    tok: Tok::Ident(word),
                    span,
                });
            }
            _ => {
                let tok = match c {
                    '=' => Tok::Equals,
                    '{' => Tok::OpenBrace,
                    '}' => Tok::CloseBrace,
                    '[' => Tok::OpenBracket,
                    ']' => Tok::CloseBracket,
                    ',' => Tok::Comma,
                    ';' => Tok::Semicolon,
                    other => return Err(LexError::UnexpectedChar { ch: other, span }),
                };
                bump!();
                tokens.push(Token { tok, span });
            }
        }
    }

    Ok(tokens)
}
