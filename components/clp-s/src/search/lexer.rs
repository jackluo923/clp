use std::fmt;
use std::fmt::Display;
use std::fmt::Formatter;

use super::KqlError;
use super::KqlLimits;
use super::KqlResource;
use super::SourceSpan;

/// One token in the pinned CLP-S KQL grammar.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KqlToken {
    pub(crate) kind: KqlTokenKind,
    pub(crate) span: SourceSpan,
}

impl KqlToken {
    /// Returns the token category.
    #[must_use]
    pub const fn kind(self) -> KqlTokenKind {
        self.kind
    }

    /// Returns the token's original-input byte span.
    #[must_use]
    pub const fn span(self) -> SourceSpan {
        self.span
    }

    /// Returns this token's original lexeme when `input` is the string passed to [`lex_kql`].
    #[must_use]
    pub fn lexeme(self, input: &str) -> Option<&str> {
        input.get(self.span.range())
    }
}

/// KQL token category.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum KqlTokenKind {
    /// Quoted or unquoted literal. The flag records whether surrounding quotes are present.
    Literal {
        /// Whether the source token has surrounding double quotes.
        quoted: bool,
    },
    /// Exact lowercase, adjacent `timestamp(` grammar token.
    TimestampStart,
    /// Case-insensitive whole-token `AND`.
    And,
    /// Case-insensitive whole-token `OR`.
    Or,
    /// Case-insensitive whole-token `NOT`.
    Not,
    /// `:`.
    Colon,
    /// `<`.
    Less,
    /// `<=`.
    LessOrEqual,
    /// `>`.
    Greater,
    /// `>=`.
    GreaterOrEqual,
    /// `{`.
    LeftBrace,
    /// `}`.
    RightBrace,
    /// `(`.
    LeftParenthesis,
    /// `)`.
    RightParenthesis,
    /// A standalone comma, used only in `timestamp(...)`.
    Comma,
}

impl Display for KqlTokenKind {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Literal { quoted: true } => "quoted literal",
            Self::Literal { quoted: false } => "unquoted literal",
            Self::TimestampStart => "'timestamp('",
            Self::And => "AND",
            Self::Or => "OR",
            Self::Not => "NOT",
            Self::Colon => "':'",
            Self::Less => "'<'",
            Self::LessOrEqual => "'<='",
            Self::Greater => "'>'",
            Self::GreaterOrEqual => "'>='",
            Self::LeftBrace => "'{'",
            Self::RightBrace => "'}'",
            Self::LeftParenthesis => "'('",
            Self::RightParenthesis => "')'",
            Self::Comma => "','",
        })
    }
}

/// Lexes UTF-8 KQL into byte-spanned tokens without copying literal contents.
///
/// # Errors
///
/// Returns a bounded allocation, input/token limit, invalid unquoted escape, unterminated quoted
/// token, or unexpected-character error. Quoted value escapes are decoded and validated by
/// [`super::parse_kql`], because the timestamp-pattern grammar deliberately uses its own escapes.
pub fn lex_kql(input: &str, limits: KqlLimits) -> Result<Vec<KqlToken>, KqlError> {
    if input.len() > limits.max_input_bytes() {
        return Err(KqlError::InputTooLong {
            actual: input.len(),
            limit: limits.max_input_bytes(),
        });
    }

    let mut tokens = Vec::new();
    let mut cursor = 0;
    while cursor < input.len() {
        let byte = input.as_bytes()[cursor];
        if matches!(byte, b' ' | b'\t' | b'\r' | b'\n') {
            cursor += 1;
            continue;
        }

        let start = cursor;
        let kind = match byte {
            b'"' => {
                cursor = scan_quoted(input, start)?;
                KqlTokenKind::Literal { quoted: true }
            }
            b':' => {
                cursor += 1;
                KqlTokenKind::Colon
            }
            b'{' => {
                cursor += 1;
                KqlTokenKind::LeftBrace
            }
            b'}' => {
                cursor += 1;
                KqlTokenKind::RightBrace
            }
            b'(' => {
                cursor += 1;
                KqlTokenKind::LeftParenthesis
            }
            b')' => {
                cursor += 1;
                KqlTokenKind::RightParenthesis
            }
            b'<' => {
                cursor += 1;
                if input.as_bytes().get(cursor) == Some(&b'=') {
                    cursor += 1;
                    KqlTokenKind::LessOrEqual
                } else {
                    KqlTokenKind::Less
                }
            }
            b'>' => {
                cursor += 1;
                if input.as_bytes().get(cursor) == Some(&b'=') {
                    cursor += 1;
                    KqlTokenKind::GreaterOrEqual
                } else {
                    KqlTokenKind::Greater
                }
            }
            _ if input[start..].starts_with("timestamp(") => {
                cursor += "timestamp(".len();
                KqlTokenKind::TimestampStart
            }
            _ => {
                cursor = scan_unquoted(input, start)?;
                classify_unquoted(&input[start..cursor])
            }
        };
        push_token(
            &mut tokens,
            KqlToken {
                kind,
                span: SourceSpan::new(start, cursor),
            },
            limits,
        )?;
    }
    Ok(tokens)
}

fn push_token(
    tokens: &mut Vec<KqlToken>,
    token: KqlToken,
    limits: KqlLimits,
) -> Result<(), KqlError> {
    if tokens.len() >= limits.max_tokens() {
        return Err(KqlError::TokenLimitExceeded {
            offset: token.span.start(),
            limit: limits.max_tokens(),
        });
    }
    if tokens.len() == tokens.capacity() {
        tokens
            .try_reserve(1)
            .map_err(|_| KqlError::AllocationFailed {
                offset: token.span.start(),
                resource: KqlResource::Tokens,
                requested: 1,
            })?;
    }
    tokens.push(token);
    Ok(())
}

fn scan_quoted(input: &str, start: usize) -> Result<usize, KqlError> {
    let bytes = input.as_bytes();
    let mut cursor = start + 1;
    let mut escaped = false;
    while cursor < bytes.len() {
        if bytes[cursor] == b'"' && !escaped {
            return Ok(cursor + 1);
        }
        if bytes[cursor] == b'\\' {
            escaped = !escaped;
        } else {
            escaped = false;
        }
        cursor += next_char_len(input, cursor)?;
    }
    Err(KqlError::UnterminatedQuotedString { offset: start })
}

fn scan_unquoted(input: &str, start: usize) -> Result<usize, KqlError> {
    let bytes = input.as_bytes();
    let mut cursor = start;
    while cursor < bytes.len() {
        let byte = bytes[cursor];
        if byte == b'\\' {
            cursor = scan_unquoted_escape(bytes, cursor)?;
        } else if is_unquoted_delimiter(byte) {
            break;
        } else {
            cursor += next_char_len(input, cursor)?;
        }
    }

    if cursor == start {
        let character = input[start..]
            .chars()
            .next()
            .ok_or(KqlError::UnexpectedCharacter {
                offset: start,
                character: '\0',
            })?;
        return Err(KqlError::UnexpectedCharacter {
            offset: start,
            character,
        });
    }
    Ok(cursor)
}

const fn is_unquoted_delimiter(byte: u8) -> bool {
    matches!(
        byte,
        b'\\'
            | b'('
            | b')'
            | b':'
            | b'<'
            | b'>'
            | b'"'
            | b'{'
            | b'}'
            | b' '
            | b'\r'
            | b'\n'
            | b'\t'
    )
}

fn scan_unquoted_escape(bytes: &[u8], slash: usize) -> Result<usize, KqlError> {
    let Some(&escaped) = bytes.get(slash + 1) else {
        return Err(KqlError::InvalidEscape { offset: slash });
    };
    if matches!(escaped, b't' | b'r' | b'n') || is_special_escape(escaped) {
        return Ok(slash + 2);
    }
    if escaped == b'u' {
        let Some(end) = slash.checked_add(6) else {
            return Err(KqlError::SizeOverflow { offset: slash });
        };
        let Some(hex) = bytes.get(slash + 2..end) else {
            return Err(KqlError::InvalidUnicodeEscape { offset: slash });
        };
        if hex.iter().all(u8::is_ascii_hexdigit) {
            return Ok(end);
        }
        return Err(KqlError::InvalidUnicodeEscape { offset: slash });
    }
    Err(KqlError::InvalidEscape { offset: slash })
}

const fn is_special_escape(byte: u8) -> bool {
    matches!(
        byte,
        b'\\'
            | b'('
            | b')'
            | b':'
            | b'<'
            | b'>'
            | b'"'
            | b'*'
            | b'?'
            | b'{'
            | b'}'
            | b'.'
            | b'@'
            | b'$'
            | b'!'
            | b'#'
    )
}

fn next_char_len(input: &str, offset: usize) -> Result<usize, KqlError> {
    input
        .get(offset..)
        .and_then(|suffix| suffix.chars().next())
        .map(char::len_utf8)
        .ok_or(KqlError::UnexpectedCharacter {
            offset,
            character: '\0',
        })
}

fn classify_unquoted(lexeme: &str) -> KqlTokenKind {
    if lexeme.eq_ignore_ascii_case("AND") {
        KqlTokenKind::And
    } else if lexeme.eq_ignore_ascii_case("OR") {
        KqlTokenKind::Or
    } else if lexeme.eq_ignore_ascii_case("NOT") {
        KqlTokenKind::Not
    } else if lexeme == "," {
        KqlTokenKind::Comma
    } else {
        KqlTokenKind::Literal { quoted: false }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pins_keywords_timestamp_adjacency_and_spans() {
        let input = "a:timestamp(\"1\", \"\\N\") OR android and*";
        let tokens = lex_kql(input, KqlLimits::default()).expect("lex");
        let kinds: Vec<_> = tokens.iter().map(|token| token.kind()).collect();
        assert_eq!(
            [
                KqlTokenKind::Literal { quoted: false },
                KqlTokenKind::Colon,
                KqlTokenKind::TimestampStart,
                KqlTokenKind::Literal { quoted: true },
                KqlTokenKind::Comma,
                KqlTokenKind::Literal { quoted: true },
                KqlTokenKind::RightParenthesis,
                KqlTokenKind::Or,
                KqlTokenKind::Literal { quoted: false },
                KqlTokenKind::Literal { quoted: false },
            ],
            kinds.as_slice()
        );
        assert_eq!(Some("timestamp("), tokens[2].lexeme(input));
        assert_eq!(Some("OR"), tokens[7].lexeme(input));
    }

    #[test]
    fn unquoted_escapes_follow_the_antlr_grammar() {
        for invalid in [r"a\ ", r"a\b", r"a\u12x4", r"a\"] {
            assert!(matches!(
                lex_kql(invalid, KqlLimits::default()),
                Err(KqlError::InvalidEscape { .. } | KqlError::InvalidUnicodeEscape { .. })
            ));
        }
        assert!(lex_kql(r"a\.b\:\*\?\u9999", KqlLimits::default()).is_ok());
        let quoted_backslash = r#""\\""#;
        assert_eq!(
            Some(quoted_backslash),
            lex_kql(quoted_backslash, KqlLimits::default())
                .expect("two backslashes permit the closing quote")
                .first()
                .and_then(|token| token.lexeme(quoted_backslash))
        );
    }

    #[test]
    fn lexer_limits_precede_growth() {
        let limits = KqlLimits::new(3, 1, 1, 1, 1, 1, 1);
        assert!(matches!(
            lex_kql("abcd", limits),
            Err(KqlError::InputTooLong {
                actual: 4,
                limit: 3
            })
        ));
        assert!(matches!(
            lex_kql("a:b", KqlLimits::new(3, 1, 1, 1, 1, 1, 1)),
            Err(KqlError::TokenLimitExceeded {
                offset: 1,
                limit: 1
            })
        ));
    }
}
