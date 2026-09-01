//! Restoration of CLP-S formatted-float lexemes.
//!
//! The archive stores a finite binary64 value beside a compact descriptor that records the
//! spelling choices lost during numeric parsing. Restoration first asks Rust's locale-independent
//! formatter for the same significant-digit scientific representation used by the C++ writer,
//! then applies the descriptor without changing the rounded mantissa.

use std::error::Error;
use std::fmt;
use std::fmt::Display;
use std::fmt::Formatter;
use std::fmt::Write as _;

use super::column::FloatExponentSign;
use super::column::FloatFormat;
use super::column::FloatFormatErrorReason;
use super::column::FloatNotation;

/// Maximum bytes appended for one finite formatted-float value.
///
/// The longest value is a negative minimum subnormal rendered in decimal notation with 17
/// significant digits.
pub const MAX_FORMATTED_FLOAT_BYTES: usize = 343;

/// Appends one formatted-float lexeme using a validated wire descriptor.
///
/// The caller-owned buffer is restored to its original length on every error. Capacity is checked
/// and reserved before formatting; the formatting path performs no temporary heap allocation.
/// Negative zero is preserved.
///
/// # Errors
///
/// Returns [`FormattedFloatError`] if `value` is non-finite, output-size arithmetic or allocation
/// fails, or the standard library violates its finite scientific-format contract.
pub fn append_formatted_float(
    value: f64,
    format: FloatFormat,
    output: &mut String,
) -> Result<(), FormattedFloatError> {
    if !value.is_finite() {
        return Err(FormattedFloatError::NonFiniteValue);
    }
    output
        .len()
        .checked_add(MAX_FORMATTED_FLOAT_BYTES)
        .ok_or(FormattedFloatError::OutputSizeOverflow)?;
    output.try_reserve(MAX_FORMATTED_FLOAT_BYTES).map_err(|_| {
        FormattedFloatError::AllocationFailed {
            requested_additional: MAX_FORMATTED_FLOAT_BYTES,
        }
    })?;

    let original_len = output.len();
    let precision = usize::from(format.significant_digits() - 1);
    if write!(output, "{value:.precision$e}").is_err() {
        output.truncate(original_len);
        return Err(FormattedFloatError::ScientificFormattingFailed);
    }
    let parts = parse_scientific(
        &output.as_bytes()[original_len..],
        usize::from(format.significant_digits()),
    );
    output.truncate(original_len);
    let parts = parts?;

    append_parts(parts, format, output);
    Ok(())
}

/// Validates a raw descriptor and appends one formatted-float lexeme.
///
/// Prefer [`append_formatted_float`] when the descriptor came from a validated formatted-float
/// column.
///
/// # Errors
///
/// Returns [`FormattedFloatError::InvalidDescriptor`] for a structurally invalid descriptor and
/// otherwise the same errors as [`append_formatted_float`].
pub fn append_formatted_float_from_descriptor(
    value: f64,
    descriptor: u16,
    output: &mut String,
) -> Result<(), FormattedFloatError> {
    let format =
        FloatFormat::try_from(descriptor).map_err(FormattedFloatError::InvalidDescriptor)?;
    append_formatted_float(value, format, output)
}

#[derive(Clone, Copy)]
struct ScientificParts {
    negative: bool,
    digits: [u8; 17],
    digit_count: usize,
    exponent: i16,
}

fn parse_scientific(
    formatted: &[u8],
    expected_digits: usize,
) -> Result<ScientificParts, FormattedFloatError> {
    let (negative, unsigned) = match formatted.first() {
        Some(b'-') => (true, &formatted[1..]),
        Some(_) => (false, formatted),
        None => return Err(FormattedFloatError::ScientificFormattingFailed),
    };
    let exponent_offset = unsigned
        .iter()
        .position(|byte| b'e' == *byte)
        .ok_or(FormattedFloatError::ScientificFormattingFailed)?;
    let (mantissa, exponent_with_marker) = unsigned.split_at(exponent_offset);
    let exponent_bytes = exponent_with_marker
        .get(1..)
        .ok_or(FormattedFloatError::ScientificFormattingFailed)?;

    let mut digits = [0_u8; 17];
    if 1 == expected_digits {
        if 1 != mantissa.len() || !mantissa[0].is_ascii_digit() {
            return Err(FormattedFloatError::ScientificFormattingFailed);
        }
        digits[0] = mantissa[0];
    } else {
        if mantissa.len() != expected_digits + 1
            || b'.' != mantissa[1]
            || !mantissa[0].is_ascii_digit()
            || !mantissa[2..].iter().all(u8::is_ascii_digit)
        {
            return Err(FormattedFloatError::ScientificFormattingFailed);
        }
        digits[0] = mantissa[0];
        digits[1..expected_digits].copy_from_slice(&mantissa[2..]);
    }
    let exponent = parse_exponent(exponent_bytes)?;
    Ok(ScientificParts {
        negative,
        digits,
        digit_count: expected_digits,
        exponent,
    })
}

fn parse_exponent(bytes: &[u8]) -> Result<i16, FormattedFloatError> {
    let (negative, digits) = match bytes.first() {
        Some(b'-') => (true, &bytes[1..]),
        Some(b'+') => (false, &bytes[1..]),
        Some(_) => (false, bytes),
        None => return Err(FormattedFloatError::ScientificFormattingFailed),
    };
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return Err(FormattedFloatError::ScientificFormattingFailed);
    }
    let mut magnitude = 0_i16;
    for digit in digits {
        magnitude = magnitude
            .checked_mul(10)
            .and_then(|value| value.checked_add(i16::from(*digit - b'0')))
            .ok_or(FormattedFloatError::ScientificFormattingFailed)?;
    }
    if negative {
        magnitude
            .checked_neg()
            .ok_or(FormattedFloatError::ScientificFormattingFailed)
    } else {
        Ok(magnitude)
    }
}

fn append_parts(parts: ScientificParts, format: FloatFormat, output: &mut String) {
    if parts.negative {
        output.push('-');
    }
    match format.notation() {
        FloatNotation::Decimal => append_decimal(parts, output),
        FloatNotation::LowercaseScientific => append_scientific(parts, format, 'e', output),
        FloatNotation::UppercaseScientific => append_scientific(parts, format, 'E', output),
    }
}

fn append_decimal(parts: ScientificParts, output: &mut String) {
    let decimal_position = i32::from(parts.exponent) + 1;
    if decimal_position <= 0 {
        output.push_str("0.");
        for _ in 0..decimal_position.unsigned_abs() {
            output.push('0');
        }
        append_ascii(&parts.digits[..parts.digit_count], output);
        return;
    }

    let decimal_position = usize::try_from(decimal_position).unwrap_or(usize::MAX);
    if decimal_position < parts.digit_count {
        append_ascii(&parts.digits[..decimal_position], output);
        output.push('.');
        append_ascii(&parts.digits[decimal_position..parts.digit_count], output);
    } else {
        append_ascii(&parts.digits[..parts.digit_count], output);
        for _ in parts.digit_count..decimal_position {
            output.push('0');
        }
    }
}

fn append_scientific(
    parts: ScientificParts,
    format: FloatFormat,
    exponent_marker: char,
    output: &mut String,
) {
    output.push(char::from(parts.digits[0]));
    if parts.digit_count > 1 {
        output.push('.');
        append_ascii(&parts.digits[1..parts.digit_count], output);
    }
    output.push(exponent_marker);
    match format.exponent_sign() {
        FloatExponentSign::None => {}
        FloatExponentSign::Plus => output.push('+'),
        FloatExponentSign::Minus => output.push('-'),
    }

    let mut exponent_digits = [0_u8; 4];
    let start = write_u16_digits(parts.exponent.unsigned_abs(), &mut exponent_digits);
    let actual_width = exponent_digits.len() - start;
    let requested_width = usize::from(format.exponent_digits().unwrap_or(1));
    for _ in actual_width..requested_width {
        output.push('0');
    }
    append_ascii(&exponent_digits[start..], output);
}

fn write_u16_digits(mut value: u16, digits: &mut [u8; 4]) -> usize {
    let mut cursor = digits.len();
    loop {
        cursor -= 1;
        digits[cursor] = b'0' + u8::try_from(value % 10).unwrap_or_default();
        value /= 10;
        if 0 == value {
            return cursor;
        }
    }
}

fn append_ascii(bytes: &[u8], output: &mut String) {
    for byte in bytes {
        output.push(char::from(*byte));
    }
}

/// Failure to validate or restore one formatted-float value.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum FormattedFloatError {
    /// The raw 16-bit descriptor is structurally invalid.
    InvalidDescriptor(FloatFormatErrorReason),
    /// The binary64 value is NaN or infinite.
    NonFiniteValue,
    /// Checked output-size arithmetic overflowed.
    OutputSizeOverflow,
    /// The caller-owned string could not reserve bounded capacity.
    AllocationFailed {
        /// Additional bytes requested.
        requested_additional: usize,
    },
    /// Rust's formatter returned an invalid or failed finite scientific representation.
    ScientificFormattingFailed,
}

impl Display for FormattedFloatError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidDescriptor(reason) => {
                write!(formatter, "invalid formatted-float descriptor: {reason}")
            }
            Self::NonFiniteValue => formatter.write_str("formatted-float value is not finite"),
            Self::OutputSizeOverflow => formatter.write_str("formatted-float output size overflow"),
            Self::AllocationFailed {
                requested_additional,
            } => write!(
                formatter,
                "could not reserve {requested_additional} bytes for formatted-float output"
            ),
            Self::ScientificFormattingFailed => {
                formatter.write_str("could not produce a finite scientific representation")
            }
        }
    }
}

impl Error for FormattedFloatError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidDescriptor(reason) => Some(reason),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOWERCASE: u16 = 0b01 << 14;
    const UPPERCASE: u16 = 0b11 << 14;
    const PLUS: u16 = 0b01 << 12;
    const MINUS: u16 = 0b10 << 12;

    fn decimal(significant_digits: u16) -> FloatFormat {
        FloatFormat::try_from((significant_digits - 1) << 5).expect("valid decimal descriptor")
    }

    fn scientific(
        notation: u16,
        sign: u16,
        exponent_digits: u16,
        significant_digits: u16,
    ) -> FloatFormat {
        FloatFormat::try_from(
            notation | sign | ((exponent_digits - 1) << 10) | ((significant_digits - 1) << 5),
        )
        .expect("valid scientific descriptor")
    }

    fn restored(value: f64, format: FloatFormat) -> String {
        let mut output = String::from("prefix:");
        append_formatted_float(value, format, &mut output).expect("restore formatted float");
        output.split_off("prefix:".len())
    }

    #[test]
    fn restores_decimal_spelling_and_significant_zeroes() {
        assert_eq!("8.2500", restored(8.25, decimal(5)));
        assert_eq!("12.50", restored(12.5, decimal(4)));
        assert_eq!("0.001230", restored(0.001_23, decimal(4)));
        assert_eq!("123000", restored(123_000.0, decimal(3)));
        assert_eq!("1", restored(1.0, decimal(1)));
    }

    #[test]
    fn restores_scientific_marker_sign_and_exponent_width() {
        assert_eq!("1.230e3", restored(1230.0, scientific(LOWERCASE, 0, 1, 4)));
        assert_eq!(
            "1.230e+03",
            restored(1230.0, scientific(LOWERCASE, PLUS, 2, 4))
        );
        assert_eq!(
            "1.230E-003",
            restored(0.001_23, scientific(UPPERCASE, MINUS, 3, 4))
        );
        assert_eq!(
            "1.8e+308",
            restored(f64::MAX, scientific(LOWERCASE, PLUS, 1, 2))
        );
    }

    #[test]
    fn preserves_negative_zero_and_recorded_zero_exponent_sign() {
        assert_eq!("-0.000", restored(-0.0, decimal(4)));
        assert_eq!(
            "-0.00e-00",
            restored(-0.0, scientific(LOWERCASE, MINUS, 2, 3))
        );
        assert_eq!(
            "0.00E+0000",
            restored(0.0, scientific(UPPERCASE, PLUS, 4, 3))
        );
    }

    #[test]
    fn matches_cxx_rounding_vectors() {
        let cases = [
            (2.25, 2, "2.2e+00"),
            (2.35, 2, "2.4e+00"),
            (1.005, 3, "1.00e+00"),
            (9.995, 3, "9.99e+00"),
            (f64::MIN_POSITIVE, 17, "2.2250738585072014e-308"),
            (f64::from_bits(1), 17, "4.9406564584124654e-324"),
            (f64::MAX, 17, "1.7976931348623157e+308"),
        ];
        for (value, significant_digits, expected) in cases {
            let format = scientific(
                LOWERCASE,
                if value < 1.0 { MINUS } else { PLUS },
                2,
                significant_digits,
            );
            assert_eq!(expected, restored(value, format));
        }
    }

    #[test]
    fn decimal_minimum_subnormal_reaches_the_documented_bound() {
        let restored = restored(-f64::from_bits(1), decimal(17));
        assert_eq!(MAX_FORMATTED_FLOAT_BYTES, restored.len());
        assert!(restored.starts_with("-0."));
        assert!(restored.ends_with("49406564584124654"));
    }

    #[test]
    fn rejects_invalid_descriptors_and_non_finite_values_without_mutation() {
        let invalid_descriptors = [0x0001, 0x8000, 0x3000, 17 << 5, PLUS];
        for descriptor in invalid_descriptors {
            let mut output = String::from("unchanged");
            assert!(matches!(
                append_formatted_float_from_descriptor(1.0, descriptor, &mut output),
                Err(FormattedFloatError::InvalidDescriptor(_))
            ));
            assert_eq!("unchanged", output);
        }

        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let mut output = String::from("unchanged");
            assert_eq!(
                Err(FormattedFloatError::NonFiniteValue),
                append_formatted_float(value, decimal(1), &mut output)
            );
            assert_eq!("unchanged", output);
        }
    }
}
