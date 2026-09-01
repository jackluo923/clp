//! C++-compatible classification of syntactically valid JSON number tokens.
//!
//! `simdjson::ondemand::number`, which the C++ CLP-S compressor uses, distinguishes signed
//! integers, unsigned integers, and binary64 values. CLP-S then reinterprets an unsigned integer
//! as signed two's-complement bits. Consequently `9223372036854775808` becomes `i64::MIN` and
//! `18446744073709551615` becomes `-1`; an integer outside the combined signed/unsigned 64-bit
//! domain is rejected instead of being rounded through binary64. This module preserves that
//! compatibility boundary without allocating.

use std::error::Error;
use std::fmt;
use std::fmt::Display;
use std::fmt::Formatter;
use std::str;

/// A JSON number after applying the C++ CLP-S numeric-domain rules.
#[derive(Clone, Copy, Debug, PartialEq)]
#[non_exhaustive]
pub enum ClassifiedJsonNumber<'a> {
    /// A signed integer, including the two's-complement reinterpretation of a JSON `u64`.
    Integer(i64),
    /// A finite binary64 value and its exact source token.
    Float {
        /// Parsed binary64 value.
        value: f64,
        /// Exact JSON token used for retained-float encoding.
        source: &'a [u8],
    },
}

impl<'a> ClassifiedJsonNumber<'a> {
    /// Returns the signed integer when this token belongs to the 64-bit integer domain.
    #[must_use]
    pub const fn integer(self) -> Option<i64> {
        match self {
            Self::Integer(value) => Some(value),
            Self::Float { .. } => None,
        }
    }

    /// Returns the finite binary64 value and exact token when this is a floating-point number.
    #[must_use]
    pub const fn float(self) -> Option<(f64, &'a [u8])> {
        match self {
            Self::Integer(_) => None,
            Self::Float { value, source } => Some((value, source)),
        }
    }
}

/// Numeric domain whose finite representation was exceeded.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum JsonNumberDomain {
    /// Signed-negative or unsigned-nonnegative 64-bit integer domain.
    Integer,
    /// Finite IEEE-754 binary64 domain.
    Float,
}

impl Display for JsonNumberDomain {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Integer => "64-bit integer",
            Self::Float => "finite binary64",
        })
    }
}

/// Failure to validate or classify one JSON number token.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum JsonNumberClassificationError {
    /// The token does not follow the JSON number grammar.
    InvalidSyntax {
        /// Zero-based byte offset at which the grammar first failed.
        byte_offset: usize,
    },
    /// The token is syntactically valid but exceeds the C++ compressor's numeric domain.
    OutOfRange {
        /// Numeric domain selected by the token's grammar.
        domain: JsonNumberDomain,
    },
}

impl JsonNumberClassificationError {
    /// Returns the syntax-error offset, if this is a grammar error.
    #[must_use]
    pub const fn byte_offset(self) -> Option<usize> {
        match self {
            Self::InvalidSyntax { byte_offset } => Some(byte_offset),
            Self::OutOfRange { .. } => None,
        }
    }

    /// Returns the exceeded numeric domain, if this is a range error.
    #[must_use]
    pub const fn domain(self) -> Option<JsonNumberDomain> {
        match self {
            Self::InvalidSyntax { .. } => None,
            Self::OutOfRange { domain } => Some(domain),
        }
    }
}

impl Display for JsonNumberClassificationError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSyntax { byte_offset } => {
                write!(formatter, "invalid JSON number at byte {byte_offset}")
            }
            Self::OutOfRange { domain } => {
                write!(formatter, "JSON number is outside the {domain} domain")
            }
        }
    }
}

impl Error for JsonNumberClassificationError {}

/// Validates and classifies one exact JSON number token without allocating.
///
/// Nonnegative integer tokens are first parsed as `u64` and then reinterpreted as signed
/// two's-complement bits, matching the current C++ CLP-S compressor. Tokens containing a fraction
/// or exponent are parsed as binary64 and must remain finite.
///
/// # Errors
///
/// Returns [`JsonNumberClassificationError::InvalidSyntax`] for a non-JSON token and
/// [`JsonNumberClassificationError::OutOfRange`] when an integer does not fit the signed-negative
/// or unsigned-nonnegative 64-bit domain, or when a floating-point token overflows binary64.
pub fn classify_json_number(
    source: &[u8],
) -> Result<ClassifiedJsonNumber<'_>, JsonNumberClassificationError> {
    let syntax = validate_number(source)?;
    classify_validated_json_number(source, syntax)
}

pub(super) fn classify_validated_json_number(
    source: &[u8],
    syntax: ValidatedJsonNumberSyntax,
) -> Result<ClassifiedJsonNumber<'_>, JsonNumberClassificationError> {
    let source_text = str::from_utf8(source)
        .expect("a parser-validated JSON number token contains only ASCII bytes");
    classify_validated_json_number_text(source_text, syntax)
}

pub(super) fn classify_validated_json_number_text(
    source: &str,
    syntax: ValidatedJsonNumberSyntax,
) -> Result<ClassifiedJsonNumber<'_>, JsonNumberClassificationError> {
    if syntax.is_float() {
        let value =
            source
                .parse::<f64>()
                .map_err(|_| JsonNumberClassificationError::OutOfRange {
                    domain: JsonNumberDomain::Float,
                })?;
        if !value.is_finite() {
            return Err(JsonNumberClassificationError::OutOfRange {
                domain: JsonNumberDomain::Float,
            });
        }
        return Ok(ClassifiedJsonNumber::Float {
            value,
            source: source.as_bytes(),
        });
    }

    let value = if source.as_bytes().first() == Some(&b'-') {
        source
            .parse::<i64>()
            .map_err(|_| JsonNumberClassificationError::OutOfRange {
                domain: JsonNumberDomain::Integer,
            })?
    } else {
        source
            .parse::<u64>()
            .map_err(|_| JsonNumberClassificationError::OutOfRange {
                domain: JsonNumberDomain::Integer,
            })?
            .cast_signed()
    };
    Ok(ClassifiedJsonNumber::Integer(value))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ValidatedJsonNumberSyntax {
    dot_position: usize,
    exponent_position: usize,
}

impl ValidatedJsonNumberSyntax {
    const MISSING_POSITION: usize = usize::MAX;

    pub(super) const fn new(dot_position: Option<usize>, exponent_position: Option<usize>) -> Self {
        Self {
            dot_position: match dot_position {
                Some(position) => position,
                None => Self::MISSING_POSITION,
            },
            exponent_position: match exponent_position {
                Some(position) => position,
                None => Self::MISSING_POSITION,
            },
        }
    }

    pub(super) const fn is_float(self) -> bool {
        self.dot_position != Self::MISSING_POSITION
            || self.exponent_position != Self::MISSING_POSITION
    }

    pub(super) const fn dot_position(self) -> Option<usize> {
        if self.dot_position == Self::MISSING_POSITION {
            None
        } else {
            Some(self.dot_position)
        }
    }

    pub(super) const fn exponent_position(self) -> Option<usize> {
        if self.exponent_position == Self::MISSING_POSITION {
            None
        } else {
            Some(self.exponent_position)
        }
    }
}

fn validate_number(
    source: &[u8],
) -> Result<ValidatedJsonNumberSyntax, JsonNumberClassificationError> {
    let invalid = |byte_offset| JsonNumberClassificationError::InvalidSyntax { byte_offset };
    let mut offset = 0;
    if source.first() == Some(&b'-') {
        offset += 1;
    }

    match source.get(offset).copied() {
        Some(b'0') => {
            offset += 1;
            if source.get(offset).is_some_and(u8::is_ascii_digit) {
                return Err(invalid(offset));
            }
        }
        Some(b'1'..=b'9') => {
            offset += 1;
            while source.get(offset).is_some_and(u8::is_ascii_digit) {
                offset += 1;
            }
        }
        _ => return Err(invalid(offset)),
    }

    let mut dot_position = None;
    if source.get(offset) == Some(&b'.') {
        dot_position = Some(offset);
        offset += 1;
        let fraction_start = offset;
        while source.get(offset).is_some_and(u8::is_ascii_digit) {
            offset += 1;
        }
        if offset == fraction_start {
            return Err(invalid(offset));
        }
    }

    let mut exponent_position = None;
    if matches!(source.get(offset), Some(b'e' | b'E')) {
        exponent_position = Some(offset);
        offset += 1;
        if matches!(source.get(offset), Some(b'+' | b'-')) {
            offset += 1;
        }
        let exponent_start = offset;
        while source.get(offset).is_some_and(u8::is_ascii_digit) {
            offset += 1;
        }
        if offset == exponent_start {
            return Err(invalid(offset));
        }
    }

    if offset != source.len() {
        return Err(invalid(offset));
    }
    Ok(ValidatedJsonNumberSyntax::new(
        dot_position,
        exponent_position,
    ))
}

#[cfg(test)]
mod tests {
    use super::ClassifiedJsonNumber;
    use super::JsonNumberClassificationError;
    use super::JsonNumberDomain;
    use super::classify_json_number;
    use super::validate_number;

    #[test]
    fn matches_cpp_integer_boundaries_and_unsigned_reinterpretation() {
        assert_eq!(
            ClassifiedJsonNumber::Integer(i64::MAX),
            classify_json_number(b"9223372036854775807").expect("i64 maximum")
        );
        assert_eq!(
            ClassifiedJsonNumber::Integer(i64::MIN),
            classify_json_number(b"9223372036854775808").expect("i64 maximum plus one")
        );
        assert_eq!(
            ClassifiedJsonNumber::Integer(-1),
            classify_json_number(b"18446744073709551615").expect("u64 maximum")
        );
        assert_eq!(
            ClassifiedJsonNumber::Integer(i64::MIN),
            classify_json_number(b"-9223372036854775808").expect("i64 minimum")
        );
        for source in [b"-9223372036854775809".as_slice(), b"18446744073709551616"] {
            assert_eq!(
                Err(JsonNumberClassificationError::OutOfRange {
                    domain: JsonNumberDomain::Integer,
                }),
                classify_json_number(source)
            );
        }
    }

    #[test]
    fn retains_floating_source_and_signed_zero() {
        let source = b"-0.00e+03";
        let ClassifiedJsonNumber::Float {
            value,
            source: actual_source,
        } = classify_json_number(source).expect("float")
        else {
            panic!("expected float")
        };
        assert!(value.is_sign_negative());
        assert_eq!(0.0_f64.to_bits() | (1_u64 << 63), value.to_bits());
        assert_eq!(source, actual_source);
        assert_eq!(
            Err(JsonNumberClassificationError::OutOfRange {
                domain: JsonNumberDomain::Float,
            }),
            classify_json_number(b"1e400")
        );
        assert_eq!(
            ClassifiedJsonNumber::Float {
                value: 0.0,
                source: b"1e-4000",
            },
            classify_json_number(b"1e-4000").expect("finite underflow")
        );
    }

    #[test]
    fn validated_syntax_retains_exact_float_delimiter_positions() {
        let syntax = validate_number(b"-12.340E+05").expect("valid float");
        assert_eq!(Some(3), syntax.dot_position());
        assert_eq!(Some(7), syntax.exponent_position());
        assert!(syntax.is_float());

        let integer = validate_number(b"-123").expect("valid integer");
        assert_eq!(None, integer.dot_position());
        assert_eq!(None, integer.exponent_position());
        assert!(!integer.is_float());
    }

    #[test]
    fn validates_the_complete_json_number_grammar() {
        for (source, offset) in [
            (b"".as_slice(), 0),
            (b"-", 1),
            (b"+1", 0),
            (b"01", 1),
            (b"1.", 2),
            (b".1", 0),
            (b"1e", 2),
            (b"1e+", 3),
            (b"1x", 1),
            (b"\xff", 0),
        ] {
            assert_eq!(
                Err(JsonNumberClassificationError::InvalidSyntax {
                    byte_offset: offset,
                }),
                classify_json_number(source),
                "source={source:?}"
            );
        }
        for source in [b"0".as_slice(), b"-0", b"10", b"0.1", b"1E-2"] {
            classify_json_number(source).expect("valid JSON number");
        }
    }
}
