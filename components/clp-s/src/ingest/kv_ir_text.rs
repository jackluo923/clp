use std::error::Error;
use std::fmt;
use std::fmt::Display;
use std::fmt::Formatter;
use std::ops::Range;

use super::KvIrEncodedText;
use super::KvIrEncodedVariable;
use super::KvIrEncoding;

/// Failure while losslessly reconstructing a KV-IR encoded-text value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum KvIrEncodedTextError {
    /// A placeholder's variable width disagreed with the encoded-text width.
    EncodedVariableWidthMismatch,
    /// An integer or float placeholder had no remaining encoded variable.
    MissingEncodedVariable,
    /// A dictionary placeholder had no remaining dictionary variable.
    MissingDictionaryVariable,
    /// The logtype ended with an escape byte and no escaped byte.
    TrailingEscape,
    /// An encoded float's decimal point lies beyond its declared digits.
    EncodedFloatDecimalPosition,
    /// An eight-byte encoded float's digit field exceeds the C++ decimal domain.
    EncodedFloatDigitsTooLarge,
    /// An encoded float's digit value needs more digits than declared.
    EncodedFloatDigitCount,
    /// The reconstructed value exceeded the caller's byte limit.
    Limit { actual: usize, limit: usize },
    /// The output buffer could not reserve more bytes.
    AllocationFailed { requested_additional: usize },
    /// A size calculation overflowed.
    SizeOverflow,
}

impl Display for KvIrEncodedTextError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::EncodedVariableWidthMismatch => {
                formatter.write_str("encoded-text variable width mismatch")
            }
            Self::MissingEncodedVariable => {
                formatter.write_str("encoded-text placeholder has no encoded variable")
            }
            Self::MissingDictionaryVariable => {
                formatter.write_str("encoded-text placeholder has no dictionary variable")
            }
            Self::TrailingEscape => {
                formatter.write_str("encoded-text logtype has a trailing escape")
            }
            Self::EncodedFloatDecimalPosition => {
                formatter.write_str("encoded float decimal position exceeds its declared digits")
            }
            Self::EncodedFloatDigitsTooLarge => {
                formatter.write_str("encoded float digit field exceeds its domain")
            }
            Self::EncodedFloatDigitCount => {
                formatter.write_str("encoded float digit field exceeds its declared digit count")
            }
            Self::Limit { actual, limit } => {
                write!(
                    formatter,
                    "reconstructed encoded text uses {actual} bytes; limit is {limit}"
                )
            }
            Self::AllocationFailed {
                requested_additional,
            } => write!(
                formatter,
                "failed to reserve {requested_additional} additional reconstructed bytes"
            ),
            Self::SizeOverflow => formatter.write_str("reconstructed encoded-text size overflow"),
        }
    }
}

impl Error for KvIrEncodedTextError {}

impl KvIrEncodedText<'_> {
    /// Appends the losslessly reconstructed value to `output`.
    ///
    /// The returned range selects the appended bytes. `max_decoded_bytes` limits only this value,
    /// not bytes already present in `output`. On failure, `output` is restored to its original
    /// length. Escapes and placeholders follow the CLP encoded-text AST; no character decoding is
    /// performed, so arbitrary byte strings are preserved.
    ///
    /// # Errors
    ///
    /// Returns a structured error for malformed AST components, size/limit violations, or failed
    /// allocation.
    pub fn append_decoded_to(
        self,
        output: &mut Vec<u8>,
        max_decoded_bytes: usize,
    ) -> Result<Range<usize>, KvIrEncodedTextError> {
        let start = output.len();
        match append_decoded(self, output, start, max_decoded_bytes) {
            Ok(()) => Ok(start..output.len()),
            Err(error) => {
                output.truncate(start);
                Err(error)
            }
        }
    }
}

fn append_decoded(
    text: KvIrEncodedText<'_>,
    output: &mut Vec<u8>,
    start: usize,
    limit: usize,
) -> Result<(), KvIrEncodedTextError> {
    let mut encoded = text.encoded_variables();
    let mut dictionaries = text.dictionary_variables();
    let logtype = text.logtype();
    let mut position = 0;
    while position < logtype.len() {
        let byte = logtype[position];
        if !matches!(byte, b'\\' | 0x11..=0x13) {
            let literal_start = position;
            position = position
                .checked_add(1)
                .ok_or(KvIrEncodedTextError::SizeOverflow)?;
            while position < logtype.len() && !matches!(logtype[position], b'\\' | 0x11..=0x13) {
                position += 1;
            }
            append_bytes(output, start, limit, &logtype[literal_start..position])?;
            continue;
        }

        if byte == b'\\' {
            position = position
                .checked_add(1)
                .ok_or(KvIrEncodedTextError::SizeOverflow)?;
            let escaped = logtype
                .get(position)
                .copied()
                .ok_or(KvIrEncodedTextError::TrailingEscape)?;
            append_bytes(output, start, limit, &[escaped])?;
        } else if byte == 0x12 {
            let value = dictionaries
                .next()
                .ok_or(KvIrEncodedTextError::MissingDictionaryVariable)?;
            append_bytes(output, start, limit, value)?;
        } else {
            let value = encoded
                .next()
                .ok_or(KvIrEncodedTextError::MissingEncodedVariable)?;
            if byte == 0x11 {
                append_integer(output, start, limit, text.encoding(), value)?;
            } else {
                append_float(output, start, limit, text.encoding(), value)?;
            }
        }
        position = position
            .checked_add(1)
            .ok_or(KvIrEncodedTextError::SizeOverflow)?;
    }
    Ok(())
}

fn append_integer(
    output: &mut Vec<u8>,
    start: usize,
    limit: usize,
    encoding: KvIrEncoding,
    value: KvIrEncodedVariable,
) -> Result<(), KvIrEncodedTextError> {
    let integer = match (encoding, value) {
        (KvIrEncoding::FourByte, KvIrEncodedVariable::FourByte(value)) => i64::from(value),
        (KvIrEncoding::EightByte, KvIrEncodedVariable::EightByte(value)) => value,
        _ => return Err(KvIrEncodedTextError::EncodedVariableWidthMismatch),
    };
    let mut digits = [0_u8; 20];
    let mut cursor = digits.len();
    let mut magnitude = integer.unsigned_abs();
    loop {
        cursor -= 1;
        digits[cursor] = b'0' + u8::try_from(magnitude % 10).expect("decimal digit fits u8");
        magnitude /= 10;
        if magnitude == 0 {
            break;
        }
    }
    if integer.is_negative() {
        append_bytes(output, start, limit, b"-")?;
    }
    append_bytes(output, start, limit, &digits[cursor..])
}

fn append_float(
    output: &mut Vec<u8>,
    start: usize,
    limit: usize,
    encoding: KvIrEncoding,
    value: KvIrEncodedVariable,
) -> Result<(), KvIrEncodedTextError> {
    let properties = decode_float_properties(encoding, value)?;
    let sign_bytes = usize::from(properties.negative);
    let output_len = usize::from(properties.digit_count)
        .checked_add(1)
        .and_then(|value| value.checked_add(sign_bytes))
        .ok_or(KvIrEncodedTextError::SizeOverflow)?;
    let output_start = output.len();
    append_zeroes(output, start, limit, output_len)?;
    if properties.negative {
        output[output_start] = b'-';
    }
    let decimal_index = output_start
        .checked_add(sign_bytes)
        .and_then(|value| value.checked_add(usize::from(properties.digit_count)))
        .and_then(|value| value.checked_sub(usize::from(properties.decimal_position)))
        .ok_or(KvIrEncodedTextError::SizeOverflow)?;
    output[decimal_index] = b'.';
    let digit_floor = output_start
        .checked_add(sign_bytes)
        .ok_or(KvIrEncodedTextError::SizeOverflow)?;
    let mut cursor = output.len();
    let mut digits = properties.digits;
    while digits != 0 {
        cursor = cursor
            .checked_sub(1)
            .ok_or(KvIrEncodedTextError::EncodedFloatDigitCount)?;
        if cursor == decimal_index {
            cursor = cursor
                .checked_sub(1)
                .ok_or(KvIrEncodedTextError::EncodedFloatDigitCount)?;
        }
        if cursor < digit_floor {
            return Err(KvIrEncodedTextError::EncodedFloatDigitCount);
        }
        output[cursor] = b'0' + u8::try_from(digits % 10).expect("decimal digit fits u8");
        digits /= 10;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug)]
struct FloatProperties {
    negative: bool,
    digits: u64,
    digit_count: u8,
    decimal_position: u8,
}

fn decode_float_properties(
    encoding: KvIrEncoding,
    value: KvIrEncodedVariable,
) -> Result<FloatProperties, KvIrEncodedTextError> {
    let properties = match (encoding, value) {
        (KvIrEncoding::FourByte, KvIrEncodedVariable::FourByte(value)) => {
            let encoded = u32::from_ne_bytes(value.to_ne_bytes());
            FloatProperties {
                negative: encoded >> 31 != 0,
                digits: u64::from((encoded >> 6) & ((1_u32 << 25) - 1)),
                digit_count: u8::try_from(((encoded >> 3) & 0x07) + 1)
                    .expect("three-bit digit count fits u8"),
                decimal_position: u8::try_from((encoded & 0x07) + 1)
                    .expect("three-bit decimal position fits u8"),
            }
        }
        (KvIrEncoding::EightByte, KvIrEncodedVariable::EightByte(value)) => {
            let encoded = u64::from_ne_bytes(value.to_ne_bytes());
            let digits = (encoded >> 8) & ((1_u64 << 54) - 1);
            if digits > 9_999_999_999_999_999 {
                return Err(KvIrEncodedTextError::EncodedFloatDigitsTooLarge);
            }
            FloatProperties {
                negative: encoded >> 63 != 0,
                digits,
                digit_count: u8::try_from(((encoded >> 4) & 0x0f) + 1)
                    .expect("four-bit digit count fits u8"),
                decimal_position: u8::try_from((encoded & 0x0f) + 1)
                    .expect("four-bit decimal position fits u8"),
            }
        }
        _ => return Err(KvIrEncodedTextError::EncodedVariableWidthMismatch),
    };
    if properties.decimal_position > properties.digit_count {
        return Err(KvIrEncodedTextError::EncodedFloatDecimalPosition);
    }
    Ok(properties)
}

fn append_zeroes(
    output: &mut Vec<u8>,
    start: usize,
    limit: usize,
    count: usize,
) -> Result<(), KvIrEncodedTextError> {
    check_growth(output.len(), start, count, limit)?;
    output
        .try_reserve(count)
        .map_err(|_| KvIrEncodedTextError::AllocationFailed {
            requested_additional: count,
        })?;
    let end = output
        .len()
        .checked_add(count)
        .ok_or(KvIrEncodedTextError::SizeOverflow)?;
    output.resize(end, b'0');
    Ok(())
}

fn append_bytes(
    output: &mut Vec<u8>,
    start: usize,
    limit: usize,
    bytes: &[u8],
) -> Result<(), KvIrEncodedTextError> {
    check_growth(output.len(), start, bytes.len(), limit)?;
    output
        .try_reserve(bytes.len())
        .map_err(|_| KvIrEncodedTextError::AllocationFailed {
            requested_additional: bytes.len(),
        })?;
    output.extend_from_slice(bytes);
    Ok(())
}

fn check_growth(
    current_len: usize,
    start: usize,
    additional: usize,
    limit: usize,
) -> Result<(), KvIrEncodedTextError> {
    let new_len = current_len
        .checked_add(additional)
        .ok_or(KvIrEncodedTextError::SizeOverflow)?;
    let actual = new_len
        .checked_sub(start)
        .ok_or(KvIrEncodedTextError::SizeOverflow)?;
    if actual > limit {
        return Err(KvIrEncodedTextError::Limit { actual, limit });
    }
    Ok(())
}
