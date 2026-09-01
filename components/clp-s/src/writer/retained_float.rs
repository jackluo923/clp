//! Validation and C++-compatible classification of retained floating-point lexemes.
//!
//! JSON parsing deliberately remains outside the archive writer. Callers provide both the finite
//! binary64 value selected by their parser and the exact JSON number token. This module validates
//! the token and bit-exact value agreement before deciding whether the current C++ format stores
//! it as a compact formatted float or in the variable dictionary.

use std::error::Error;
use std::fmt;
use std::fmt::Display;
use std::fmt::Formatter;
use std::str;

use crate::archive::FloatFormat;
use crate::archive::FormattedFloatError;
use crate::archive::append_formatted_float;

const LOWERCASE_SCIENTIFIC: u16 = 0b01 << 14;
const UPPERCASE_SCIENTIFIC: u16 = 0b11 << 14;
const PLUS_EXPONENT: u16 = 0b01 << 12;
const MINUS_EXPONENT: u16 = 0b10 << 12;
const EXPONENT_DIGITS_POSITION: u32 = 10;
const SIGNIFICANT_DIGITS_POSITION: u32 = 5;
const MAX_EXPONENT_DIGITS: usize = 4;
const MAX_SIGNIFICANT_DIGITS: usize = 17;

/// A parsed finite binary64 value paired with its exact source number token.
///
/// Construction only borrows the pair. [`super::OpenArchive::append_record`] validates JSON
/// number syntax, requires a decimal point or exponent (integer tokens belong in
/// [`super::ValueRef::I64`]), parses the token to the same binary64 bits, and rejects non-finite
/// results before changing archive state.
#[derive(Clone, Copy)]
pub struct RetainedFloatRef<'a> {
    value: f64,
    source: &'a [u8],
    trusted_syntax: u32,
}

impl<'a> RetainedFloatRef<'a> {
    /// Borrows a parsed value and its exact JSON number token for validation during append.
    #[must_use]
    pub const fn new(value: f64, source: &'a [u8]) -> Self {
        Self {
            value,
            source,
            trusted_syntax: 0,
        }
    }

    pub(crate) fn new_trusted(
        value: f64,
        source: &'a [u8],
        dot_position: Option<usize>,
        exponent_position: Option<usize>,
    ) -> Self {
        const POSITION_BITS: u32 = 15;
        const POSITION_MASK_USIZE: usize = (1 << POSITION_BITS) - 1;
        const TRUSTED: u32 = 1 << 31;
        const TOO_LONG: u32 = 1 << 30;

        let encode_position = |position: Option<usize>| {
            position.map_or(Some(0), |position| {
                position
                    .checked_add(1)
                    .filter(|encoded| *encoded <= POSITION_MASK_USIZE)
                    .map(|encoded| u32::try_from(encoded).expect("position is bounded by 15 bits"))
            })
        };
        let trusted_syntax = match (
            encode_position(dot_position),
            encode_position(exponent_position),
        ) {
            (Some(dot), Some(exponent)) => TRUSTED | dot | (exponent << POSITION_BITS),
            _ => TRUSTED | TOO_LONG,
        };
        Self {
            value,
            source,
            trusted_syntax,
        }
    }

    /// Returns the caller-parsed binary64 value.
    #[must_use]
    pub const fn value(self) -> f64 {
        self.value
    }

    /// Returns the exact source token.
    #[must_use]
    pub const fn source(self) -> &'a [u8] {
        self.source
    }

    fn trusted_float_syntax(self) -> TrustedFloatSyntax {
        const POSITION_BITS: u32 = 15;
        const POSITION_MASK: u32 = (1 << POSITION_BITS) - 1;
        const TRUSTED: u32 = 1 << 31;
        const TOO_LONG: u32 = 1 << 30;

        if 0 == self.trusted_syntax & TRUSTED {
            return TrustedFloatSyntax::Untrusted;
        }
        if 0 != self.trusted_syntax & TOO_LONG {
            return TrustedFloatSyntax::Dictionary;
        }
        let decode_position =
            |encoded| (0 != encoded).then(|| usize::try_from(encoded - 1).expect("u32 fits usize"));
        TrustedFloatSyntax::Validated(FloatTokenSyntax {
            dot_position: decode_position(self.trusted_syntax & POSITION_MASK),
            exponent_position: decode_position(
                (self.trusted_syntax >> POSITION_BITS) & POSITION_MASK,
            ),
        })
    }
}

impl fmt::Debug for RetainedFloatRef<'_> {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RetainedFloatRef")
            .field("value", &self.value)
            .field("source", &self.source)
            .finish()
    }
}

impl PartialEq for RetainedFloatRef<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value && self.source == other.source
    }
}

/// Reason a retained floating-point token could not be accepted.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum RetainedFloatError {
    /// The caller-provided binary64 value is NaN or infinite.
    NonFiniteValue,
    /// The source is not one complete JSON number token.
    InvalidToken {
        /// Byte offset at which the token first violates JSON number syntax.
        byte_offset: usize,
    },
    /// The token has no decimal point or exponent and belongs in an integer value variant.
    IntegerToken,
    /// The valid JSON number token cannot be represented as a finite binary64 value.
    NonFiniteToken,
    /// The token did not parse to the exact bits supplied by the caller.
    ValueMismatch {
        /// Bits of [`RetainedFloatRef::value`].
        supplied_bits: u64,
        /// Bits obtained by parsing [`RetainedFloatRef::source`].
        token_bits: u64,
    },
    /// Compact-format round-trip validation failed internally.
    Formatting(FormattedFloatError),
}

impl Display for RetainedFloatError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::NonFiniteValue => {
                formatter.write_str("retained floating-point value is not finite")
            }
            Self::InvalidToken { byte_offset } => write!(
                formatter,
                "retained floating-point source is not a JSON number at byte {byte_offset}"
            ),
            Self::IntegerToken => formatter
                .write_str("retained floating-point source is an integer token; use ValueRef::I64"),
            Self::NonFiniteToken => formatter.write_str(
                "retained floating-point source cannot be represented as finite binary64",
            ),
            Self::ValueMismatch {
                supplied_bits,
                token_bits,
            } => write!(
                formatter,
                "retained floating-point source parsed to bits {token_bits:#018x}, not supplied \
                 bits {supplied_bits:#018x}"
            ),
            Self::Formatting(source) => {
                write!(
                    formatter,
                    "could not validate compact float formatting: {source}"
                )
            }
        }
    }
}

impl Error for RetainedFloatError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Formatting(source) => Some(source),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum RetainedFloatEncoding<'a> {
    Formatted { value: f64, descriptor: u16 },
    Dictionary { source: &'a [u8] },
}

impl RetainedFloatEncoding<'_> {
    #[cfg(test)]
    pub(super) const fn descriptor(self) -> Option<u16> {
        match self {
            Self::Formatted { descriptor, .. } => Some(descriptor),
            Self::Dictionary { .. } => None,
        }
    }
}

/// Reusable formatting storage owned by an open writer.
#[derive(Debug, Default)]
pub(super) struct RetainedFloatScratch {
    restored: String,
}

pub(super) fn classify<'a>(
    retained: RetainedFloatRef<'a>,
    scratch: &mut RetainedFloatScratch,
) -> Result<RetainedFloatEncoding<'a>, RetainedFloatError> {
    if !retained.value().is_finite() {
        return Err(RetainedFloatError::NonFiniteValue);
    }
    let syntax = match retained.trusted_float_syntax() {
        TrustedFloatSyntax::Validated(syntax) => syntax,
        TrustedFloatSyntax::Dictionary => {
            return Ok(RetainedFloatEncoding::Dictionary {
                source: retained.source(),
            });
        }
        TrustedFloatSyntax::Untrusted => {
            let syntax = validate_json_float_token(retained.source())?;
            let token = str::from_utf8(retained.source())
                .expect("a validated JSON number token contains only ASCII bytes");
            let parsed = token
                .parse::<f64>()
                .map_err(|_| RetainedFloatError::NonFiniteToken)?;
            if !parsed.is_finite() {
                return Err(RetainedFloatError::NonFiniteToken);
            }
            if parsed.to_bits() != retained.value().to_bits() {
                return Err(RetainedFloatError::ValueMismatch {
                    supplied_bits: retained.value().to_bits(),
                    token_bits: parsed.to_bits(),
                });
            }
            syntax
        }
    };

    let Some(format) = derive_format(retained.source(), syntax) else {
        return Ok(RetainedFloatEncoding::Dictionary {
            source: retained.source(),
        });
    };
    scratch.restored.clear();
    append_formatted_float(retained.value(), format, &mut scratch.restored)
        .map_err(RetainedFloatError::Formatting)?;
    if scratch.restored.as_bytes() == retained.source() {
        Ok(RetainedFloatEncoding::Formatted {
            value: retained.value(),
            descriptor: format.raw(),
        })
    } else {
        Ok(RetainedFloatEncoding::Dictionary {
            source: retained.source(),
        })
    }
}

#[derive(Clone, Copy)]
struct FloatTokenSyntax {
    dot_position: Option<usize>,
    exponent_position: Option<usize>,
}

enum TrustedFloatSyntax {
    Untrusted,
    Validated(FloatTokenSyntax),
    Dictionary,
}

fn validate_json_float_token(token: &[u8]) -> Result<FloatTokenSyntax, RetainedFloatError> {
    let mut index = usize::from(token.first() == Some(&b'-'));
    let Some(first_digit) = token.get(index).copied() else {
        return Err(RetainedFloatError::InvalidToken { byte_offset: index });
    };
    match first_digit {
        b'0' => {
            index += 1;
            if token.get(index).is_some_and(u8::is_ascii_digit) {
                return Err(RetainedFloatError::InvalidToken { byte_offset: index });
            }
        }
        b'1'..=b'9' => {
            index += 1;
            while token.get(index).is_some_and(u8::is_ascii_digit) {
                index += 1;
            }
        }
        _ => return Err(RetainedFloatError::InvalidToken { byte_offset: index }),
    }

    let mut dot_position = None;
    if token.get(index) == Some(&b'.') {
        dot_position = Some(index);
        index += 1;
        let fraction_start = index;
        while token.get(index).is_some_and(u8::is_ascii_digit) {
            index += 1;
        }
        if fraction_start == index {
            return Err(RetainedFloatError::InvalidToken { byte_offset: index });
        }
    }
    let mut exponent_position = None;
    if matches!(token.get(index), Some(b'e' | b'E')) {
        exponent_position = Some(index);
        index += 1;
        if matches!(token.get(index), Some(b'+' | b'-')) {
            index += 1;
        }
        let exponent_start = index;
        while token.get(index).is_some_and(u8::is_ascii_digit) {
            index += 1;
        }
        if exponent_start == index {
            return Err(RetainedFloatError::InvalidToken { byte_offset: index });
        }
    }
    if index != token.len() {
        return Err(RetainedFloatError::InvalidToken { byte_offset: index });
    }
    if dot_position.is_none() && exponent_position.is_none() {
        return Err(RetainedFloatError::IntegerToken);
    }
    Ok(FloatTokenSyntax {
        dot_position,
        exponent_position,
    })
}

fn derive_format(token: &[u8], syntax: FloatTokenSyntax) -> Option<FloatFormat> {
    let first_digit_position = usize::from(token.first() == Some(&b'-'));
    let dot_position = syntax.dot_position;
    let exponent_position = syntax.exponent_position;
    let significant_end = exponent_position.unwrap_or(token.len());
    let mut raw = 0_u16;

    if let Some(exponent_position) = exponent_position {
        if dot_position.is_some_and(|position| position != first_digit_position + 1) {
            return None;
        }
        raw |= if b'E' == token[exponent_position] {
            UPPERCASE_SCIENTIFIC
        } else {
            LOWERCASE_SCIENTIFIC
        };
        let sign_offset = exponent_position + 1;
        let exponent_digits_start = match token[sign_offset] {
            b'+' => {
                raw |= PLUS_EXPONENT;
                sign_offset + 1
            }
            b'-' => {
                raw |= MINUS_EXPONENT;
                sign_offset + 1
            }
            _ => sign_offset,
        };
        let exponent_digits = token.len() - exponent_digits_start;
        if 0 == exponent_digits || exponent_digits > MAX_EXPONENT_DIGITS {
            return None;
        }
        raw |= u16::try_from(exponent_digits - 1).ok()? << EXPONENT_DIGITS_POSITION;
    }

    let mut first_significant_position = first_digit_position;
    if b'0' == token[first_significant_position]
        && let Some(dot_position) = dot_position
    {
        for (position, byte) in token
            .iter()
            .enumerate()
            .take(significant_end)
            .skip(dot_position + 1)
        {
            if b'0' != *byte {
                first_significant_position = position;
                break;
            }
        }
    }
    let dot_adjustment =
        usize::from(dot_position.is_some_and(|position| first_significant_position < position));
    let significant_digits = significant_end
        .checked_sub(first_significant_position)?
        .checked_sub(dot_adjustment)?;
    if 0 == significant_digits || significant_digits > MAX_SIGNIFICANT_DIGITS {
        return None;
    }
    raw |= u16::try_from(significant_digits - 1).ok()? << SIGNIFICANT_DIGITS_POSITION;
    FloatFormat::try_from(raw).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classify_fresh(
        retained: RetainedFloatRef<'_>,
    ) -> Result<RetainedFloatEncoding<'_>, RetainedFloatError> {
        classify(retained, &mut RetainedFloatScratch::default())
    }

    #[test]
    fn trusted_parser_syntax_matches_the_defensive_public_path() {
        for (source, dot_position, exponent_position) in [
            (b"1.2300".as_slice(), Some(1), None),
            (b"-0.00E+03".as_slice(), Some(2), Some(5)),
            (b"1e-4000".as_slice(), None, Some(1)),
        ] {
            let value = str::from_utf8(source)
                .expect("ASCII fixture")
                .parse::<f64>()
                .expect("finite fixture");
            let defensive = RetainedFloatRef::new(value, source);
            let trusted =
                RetainedFloatRef::new_trusted(value, source, dot_position, exponent_position);
            assert_eq!(defensive, trusted);
            assert_eq!(format!("{defensive:?}"), format!("{trusted:?}"));
            assert_eq!(classify_fresh(defensive), classify_fresh(trusted));
        }
    }

    fn classify_source(source: &'static [u8]) -> RetainedFloatEncoding<'static> {
        let value = str::from_utf8(source)
            .expect("test source is UTF-8")
            .parse::<f64>()
            .expect("test source parses");
        classify_fresh(RetainedFloatRef::new(value, source)).expect("classify retained float")
    }

    #[test]
    fn compact_descriptors_round_trip_cpp_valid_vectors() {
        let sources: &[&[u8]] = &[
            b"0.007",
            b"-0.007",
            b"123456789.1234567",
            b"123456789.000",
            b"0.00000000000000000001234567891234567",
            b"0.00",
            b"-0.00",
            b"1.234567891234567E9",
            b"1.234567891234567e+09",
            b"1.234567891234567E-0009",
            b"1E16",
            b"-1e+0016",
            b"0E0",
            b"-0.000e-0000",
            b"1.7976931348623157E308",
            b"4.9406564584124654E-324",
        ];
        for source in sources {
            assert!(
                matches!(
                    classify_source(source),
                    RetainedFloatEncoding::Formatted { .. }
                ),
                "{source:?} should use a formatted-float column"
            );
        }
    }

    #[test]
    fn dictionary_fallback_preserves_cpp_invalid_vectors() {
        let sources: &[&[u8]] = &[
            b"123456789.123456789",
            b"123456789.123456700",
            b"12.345e6",
            b"1.0e00000",
            b"1.2345678912345679e+13",
            b"0.00000000000000000",
            b"0e1",
            b"-0E-0001",
        ];
        for source in sources {
            assert!(
                matches!(
                    classify_source(source),
                    RetainedFloatEncoding::Dictionary { .. }
                ),
                "{source:?} should use a dictionary-float column"
            );
        }
    }

    #[test]
    fn validates_syntax_finiteness_and_exact_value_bits() {
        for (source, offset) in [
            (b"".as_slice(), 0),
            (b"+1.0", 0),
            (b"01.0", 1),
            (b"1.", 2),
            (b"1e", 2),
            (b"1.0 ", 3),
            (b"NaN", 0),
            (b"\xff", 0),
        ] {
            assert_eq!(
                Err(RetainedFloatError::InvalidToken {
                    byte_offset: offset
                }),
                classify_fresh(RetainedFloatRef::new(1.0, source))
            );
        }
        assert_eq!(
            Err(RetainedFloatError::IntegerToken),
            classify_fresh(RetainedFloatRef::new(1.0, b"1"))
        );
        assert_eq!(
            Err(RetainedFloatError::NonFiniteValue),
            classify_fresh(RetainedFloatRef::new(f64::INFINITY, b"1e9999"))
        );
        assert_eq!(
            Err(RetainedFloatError::NonFiniteToken),
            classify_fresh(RetainedFloatRef::new(1.0, b"1e9999"))
        );
        assert_eq!(
            Err(RetainedFloatError::ValueMismatch {
                supplied_bits: 1.0_f64.to_bits(),
                token_bits: 2.0_f64.to_bits(),
            }),
            classify_fresh(RetainedFloatRef::new(1.0, b"2.0"))
        );
        assert!(matches!(
            classify_fresh(RetainedFloatRef::new(-0.0, b"-0.0")),
            Ok(RetainedFloatEncoding::Formatted { .. })
        ));
        assert!(matches!(
            classify_fresh(RetainedFloatRef::new(0.0, b"-0.0")),
            Err(RetainedFloatError::ValueMismatch { .. })
        ));
    }

    #[test]
    fn descriptor_spelling_captures_notation_sign_width_and_significant_digits() {
        let cases: &[(&[u8], u16)] = &[
            (b"-0.00", 2 << SIGNIFICANT_DIGITS_POSITION),
            (
                b"1.20e+0009",
                LOWERCASE_SCIENTIFIC
                    | PLUS_EXPONENT
                    | (3 << EXPONENT_DIGITS_POSITION)
                    | (2 << SIGNIFICANT_DIGITS_POSITION),
            ),
            (
                b"1.20E-09",
                UPPERCASE_SCIENTIFIC
                    | MINUS_EXPONENT
                    | (1 << EXPONENT_DIGITS_POSITION)
                    | (2 << SIGNIFICANT_DIGITS_POSITION),
            ),
        ];
        for (source, expected) in cases {
            let encoding = classify_source(source);
            assert_eq!(Some(*expected), encoding.descriptor(), "{source:?}");
        }
    }
}
