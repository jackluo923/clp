//! Reconstruction of CLP logtypes and eight-byte encoded variables.

use std::error::Error;
use std::fmt;
use std::fmt::Display;
use std::fmt::Formatter;

use super::column::EncodedFloatErrorReason;
use super::column::I64Column;
use super::dictionary::LogTypeDictionaryEntry;
use super::dictionary::VariableDictionary;

const INTEGER_PLACEHOLDER: u8 = 0x11;
const DICTIONARY_PLACEHOLDER: u8 = 0x12;
const FLOAT_PLACEHOLDER: u8 = 0x13;
const ESCAPE_MARKER: u8 = b'\\';
const ENCODED_FLOAT_UNUSED_BIT: u64 = 1_u64 << 62;
const ENCODED_FLOAT_DIGITS_MASK: u64 = (1_u64 << 54) - 1;
const MAX_ENCODED_FLOAT_DIGITS: u64 = 9_999_999_999_999_999;

mod sealed {
    use super::I64Column;

    pub trait Sealed {}

    impl Sealed for [i64] {}
    impl Sealed for I64Column<'_> {}
}

/// Random-access encoded-variable values consumed by a CLP logtype.
///
/// Implementations are provided for ordinary `[i64]` slices and zero-copy [`I64Column`] views.
/// The trait is sealed so both validation passes observe the same immutable values.
pub trait EncodedVariableSource: sealed::Sealed {
    /// Returns the number of encoded variables.
    #[must_use]
    fn len(&self) -> usize;

    /// Returns whether there are no encoded variables.
    #[must_use]
    fn is_empty(&self) -> bool {
        0 == self.len()
    }

    /// Returns one encoded variable by zero-based position.
    #[must_use]
    fn get(&self, index: usize) -> Option<i64>;
}

impl EncodedVariableSource for [i64] {
    fn len(&self) -> usize {
        <[i64]>::len(self)
    }

    fn get(&self, index: usize) -> Option<i64> {
        <[i64]>::get(self, index).copied()
    }
}

impl EncodedVariableSource for I64Column<'_> {
    fn len(&self) -> usize {
        (*self).len()
    }

    fn get(&self, index: usize) -> Option<i64> {
        (*self).get(index)
    }
}

/// Reconstructs one CLP message into a caller-owned byte buffer.
///
/// Dictionary values are arbitrary bytes, so this API deliberately writes to `Vec<u8>` rather
/// than requiring UTF-8. It validates all references and computes the exact appended size before
/// reserving once. The output remains unchanged on every error.
///
/// # Errors
///
/// Returns [`EncodedVariableError`] for a variable-count mismatch, a missing dictionary entry, an
/// invalid custom float, a violated validated-logtype invariant, checked size overflow, or failed
/// bounded allocation.
pub fn append_clp_message<V: EncodedVariableSource + ?Sized>(
    logtype: LogTypeDictionaryEntry<'_>,
    variable_dictionary: &VariableDictionary,
    encoded_variables: &V,
    output: &mut Vec<u8>,
) -> Result<(), EncodedVariableError> {
    append_clp_message_bounded(
        logtype,
        variable_dictionary,
        encoded_variables,
        output,
        usize::MAX,
    )
}

/// Reconstructs one CLP message with an explicit bound on bytes appended.
///
/// Validation and exact-size analysis finish before the limit is checked, and the limit is checked
/// before reserving output capacity. This lets untrusted-archive callers bound scratch allocation
/// without weakening the structural validation performed by [`append_clp_message`]. The limit
/// applies only to this call's appended message, not bytes already present in `output`.
///
/// # Errors
///
/// Returns the same failures as [`append_clp_message`], or
/// [`EncodedVariableError::OutputLimitExceeded`] when the exact reconstructed message is larger
/// than `max_appended_bytes`. The output remains unchanged on every error.
pub fn append_clp_message_bounded<V: EncodedVariableSource + ?Sized>(
    logtype: LogTypeDictionaryEntry<'_>,
    variable_dictionary: &VariableDictionary,
    encoded_variables: &V,
    output: &mut Vec<u8>,
    max_appended_bytes: usize,
) -> Result<(), EncodedVariableError> {
    let expected = logtype.placeholder_counts().encoded_variables();
    let expected_usize =
        usize::try_from(expected).map_err(|_| EncodedVariableError::OutputSizeOverflow)?;
    let actual = encoded_variables.len();
    if expected_usize != actual {
        return Err(EncodedVariableError::VariableCountMismatch { expected, actual });
    }

    let appended_size = analyze_message(logtype, variable_dictionary, encoded_variables)?;
    if appended_size > max_appended_bytes {
        return Err(EncodedVariableError::OutputLimitExceeded {
            required: appended_size,
            limit: max_appended_bytes,
        });
    }
    output
        .len()
        .checked_add(appended_size)
        .ok_or(EncodedVariableError::OutputSizeOverflow)?;
    output.try_reserve_exact(appended_size).map_err(|_| {
        EncodedVariableError::AllocationFailed {
            requested_additional: appended_size,
        }
    })?;

    let original_len = output.len();
    let result = append_validated_message(logtype, variable_dictionary, encoded_variables, output);
    if result.is_err() {
        output.truncate(original_len);
    }
    result
}

fn analyze_message<V: EncodedVariableSource + ?Sized>(
    logtype: LogTypeDictionaryEntry<'_>,
    variable_dictionary: &VariableDictionary,
    encoded_variables: &V,
) -> Result<usize, EncodedVariableError> {
    let mut output_size = 0_usize;
    walk_logtype(logtype, encoded_variables, |token| {
        let token_size = match token {
            LogTypeToken::Literal(_) => 1,
            LogTypeToken::Integer { value, .. } => i64_string_len(value),
            LogTypeToken::Dictionary { index, encoded } => variable_dictionary
                .entry(bit_cast_dictionary_id(encoded))
                .ok_or_else(|| missing_dictionary_entry(index, encoded))?
                .value()
                .len(),
            LogTypeToken::Float { index, encoded } => decode_encoded_float(encoded)
                .map_err(|reason| EncodedVariableError::InvalidEncodedFloat {
                    encoded_variable_index: index,
                    reason,
                })?
                .output_len(),
        };
        output_size = output_size
            .checked_add(token_size)
            .ok_or(EncodedVariableError::OutputSizeOverflow)?;
        Ok(())
    })?;
    Ok(output_size)
}

fn append_validated_message<V: EncodedVariableSource + ?Sized>(
    logtype: LogTypeDictionaryEntry<'_>,
    variable_dictionary: &VariableDictionary,
    encoded_variables: &V,
    output: &mut Vec<u8>,
) -> Result<(), EncodedVariableError> {
    walk_logtype(logtype, encoded_variables, |token| {
        match token {
            LogTypeToken::Literal(byte) => output.push(byte),
            LogTypeToken::Integer { value, .. } => append_i64(value, output),
            LogTypeToken::Dictionary { index, encoded } => {
                let entry = variable_dictionary
                    .entry(bit_cast_dictionary_id(encoded))
                    .ok_or_else(|| missing_dictionary_entry(index, encoded))?;
                output.extend_from_slice(entry.value());
            }
            LogTypeToken::Float { index, encoded } => {
                let decoded = decode_encoded_float(encoded).map_err(|reason| {
                    EncodedVariableError::InvalidEncodedFloat {
                        encoded_variable_index: index,
                        reason,
                    }
                })?;
                decoded.append_to(output);
            }
        }
        Ok(())
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LogTypeToken {
    Literal(u8),
    Integer { index: usize, value: i64 },
    Dictionary { index: usize, encoded: i64 },
    Float { index: usize, encoded: i64 },
}

fn walk_logtype<V, F>(
    logtype: LogTypeDictionaryEntry<'_>,
    encoded_variables: &V,
    mut visit: F,
) -> Result<(), EncodedVariableError>
where
    V: EncodedVariableSource + ?Sized,
    F: FnMut(LogTypeToken) -> Result<(), EncodedVariableError>, {
    let bytes = logtype.escaped_value();
    let mut byte_offset = 0_usize;
    let mut variable_index = 0_usize;
    while byte_offset < bytes.len() {
        match bytes[byte_offset] {
            ESCAPE_MARKER => {
                let literal_offset = byte_offset
                    .checked_add(1)
                    .ok_or(EncodedVariableError::OutputSizeOverflow)?;
                let literal = bytes
                    .get(literal_offset)
                    .copied()
                    .ok_or(EncodedVariableError::InvalidLogTypeEscape { byte_offset })?;
                visit(LogTypeToken::Literal(literal))?;
                byte_offset = literal_offset
                    .checked_add(1)
                    .ok_or(EncodedVariableError::OutputSizeOverflow)?;
            }
            INTEGER_PLACEHOLDER | DICTIONARY_PLACEHOLDER | FLOAT_PLACEHOLDER => {
                let encoded = encoded_variables.get(variable_index).ok_or(
                    EncodedVariableError::MissingEncodedVariable {
                        encoded_variable_index: variable_index,
                    },
                )?;
                let token = match bytes[byte_offset] {
                    INTEGER_PLACEHOLDER => LogTypeToken::Integer {
                        index: variable_index,
                        value: encoded,
                    },
                    DICTIONARY_PLACEHOLDER => LogTypeToken::Dictionary {
                        index: variable_index,
                        encoded,
                    },
                    FLOAT_PLACEHOLDER => LogTypeToken::Float {
                        index: variable_index,
                        encoded,
                    },
                    _ => unreachable!(),
                };
                visit(token)?;
                variable_index = variable_index
                    .checked_add(1)
                    .ok_or(EncodedVariableError::OutputSizeOverflow)?;
                byte_offset = byte_offset
                    .checked_add(1)
                    .ok_or(EncodedVariableError::OutputSizeOverflow)?;
            }
            literal => {
                visit(LogTypeToken::Literal(literal))?;
                byte_offset = byte_offset
                    .checked_add(1)
                    .ok_or(EncodedVariableError::OutputSizeOverflow)?;
            }
        }
    }
    if variable_index != encoded_variables.len() {
        let expected =
            u64::try_from(variable_index).map_err(|_| EncodedVariableError::OutputSizeOverflow)?;
        return Err(EncodedVariableError::VariableCountMismatch {
            expected,
            actual: encoded_variables.len(),
        });
    }
    Ok(())
}

const fn missing_dictionary_entry(index: usize, encoded: i64) -> EncodedVariableError {
    EncodedVariableError::MissingDictionaryEntry {
        encoded_variable_index: index,
        id: bit_cast_dictionary_id(encoded),
    }
}

const fn bit_cast_dictionary_id(encoded: i64) -> u64 {
    u64::from_le_bytes(encoded.to_le_bytes())
}

fn i64_string_len(value: i64) -> usize {
    usize::from(value.is_negative()) + u64_digit_count(value.unsigned_abs())
}

const fn u64_digit_count(mut value: u64) -> usize {
    let mut count = 1_usize;
    while value >= 10 {
        value /= 10;
        count += 1;
    }
    count
}

fn append_i64(value: i64, output: &mut Vec<u8>) {
    if value < 0 {
        output.push(b'-');
    }
    let mut digits = [0_u8; 20];
    let start = write_u64_digits(value.unsigned_abs(), &mut digits);
    output.extend_from_slice(&digits[start..]);
}

fn write_u64_digits(mut value: u64, digits: &mut [u8; 20]) -> usize {
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DecodedEncodedFloat {
    negative: bool,
    digits: u64,
    digit_count: u8,
    decimal_position: u8,
}

impl DecodedEncodedFloat {
    fn output_len(self) -> usize {
        usize::from(self.digit_count) + 1 + usize::from(self.negative)
    }

    fn append_to(self, output: &mut Vec<u8>) {
        if self.negative {
            output.push(b'-');
        }
        let digit_count = usize::from(self.digit_count);
        let mut buffer = [b'0'; 16];
        let mut digits = self.digits;
        for digit in buffer[..digit_count].iter_mut().rev() {
            *digit = b'0' + u8::try_from(digits % 10).unwrap_or_default();
            digits /= 10;
        }
        let decimal_offset = digit_count - usize::from(self.decimal_position);
        output.extend_from_slice(&buffer[..decimal_offset]);
        output.push(b'.');
        output.extend_from_slice(&buffer[decimal_offset..digit_count]);
    }
}

fn decode_encoded_float(encoded: i64) -> Result<DecodedEncodedFloat, EncodedFloatErrorReason> {
    let bits = u64::from_le_bytes(encoded.to_le_bytes());
    if 0 != bits & ENCODED_FLOAT_UNUSED_BIT {
        return Err(EncodedFloatErrorReason::ReservedBit);
    }
    let decimal_position = u8::try_from((bits & 0x0f) + 1)
        .map_err(|_| EncodedFloatErrorReason::DecimalPositionExceedsDigits)?;
    let digit_count = u8::try_from(((bits >> 4) & 0x0f) + 1)
        .map_err(|_| EncodedFloatErrorReason::DigitsTooLarge)?;
    let digits = (bits >> 8) & ENCODED_FLOAT_DIGITS_MASK;
    if digits > MAX_ENCODED_FLOAT_DIGITS {
        return Err(EncodedFloatErrorReason::DigitsTooLarge);
    }
    if decimal_position > digit_count {
        return Err(EncodedFloatErrorReason::DecimalPositionExceedsDigits);
    }
    let exclusive_limit = 10_u64
        .checked_pow(u32::from(digit_count))
        .ok_or(EncodedFloatErrorReason::DigitsTooLarge)?;
    if digits >= exclusive_limit {
        return Err(EncodedFloatErrorReason::DigitValueExceedsDeclaredDigits);
    }
    Ok(DecodedEncodedFloat {
        negative: 0 != bits & (1_u64 << 63),
        digits,
        digit_count,
        decimal_position,
    })
}

/// Failure to reconstruct a CLP logtype and encoded-variable sequence.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum EncodedVariableError {
    /// The logtype's unescaped placeholders do not match the supplied value count.
    VariableCountMismatch {
        /// Variables required by the logtype.
        expected: u64,
        /// Variables supplied by the caller.
        actual: usize,
    },
    /// An encoded-variable source violated its validated random-access invariant.
    MissingEncodedVariable {
        /// Missing zero-based encoded-variable position.
        encoded_variable_index: usize,
    },
    /// A dictionary placeholder contains a bit-cast ID absent from `/var.dict`.
    MissingDictionaryEntry {
        /// Zero-based position in the supplied encoded variables.
        encoded_variable_index: usize,
        /// Missing unsigned dictionary ID.
        id: u64,
    },
    /// A custom float violates its reserved-bit or digit-range structure.
    InvalidEncodedFloat {
        /// Zero-based position in the supplied encoded variables.
        encoded_variable_index: usize,
        /// Structural failure.
        reason: EncodedFloatErrorReason,
    },
    /// A supposedly validated logtype ends in an unpaired escape marker.
    InvalidLogTypeEscape {
        /// Byte offset of the escape marker.
        byte_offset: usize,
    },
    /// Checked output-size or cursor arithmetic overflowed.
    OutputSizeOverflow,
    /// The exact reconstructed message exceeds the caller's append bound.
    OutputLimitExceeded {
        /// Exact bytes required for this message.
        required: usize,
        /// Maximum bytes the caller permits this call to append.
        limit: usize,
    },
    /// The caller-owned byte vector could not reserve the exact appended size.
    AllocationFailed {
        /// Additional bytes requested.
        requested_additional: usize,
    },
}

impl Display for EncodedVariableError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::VariableCountMismatch { expected, actual } => write!(
                formatter,
                "CLP logtype requires {expected} encoded variables but {actual} were supplied"
            ),
            Self::MissingEncodedVariable {
                encoded_variable_index,
            } => write!(
                formatter,
                "encoded-variable source omitted declared index {encoded_variable_index}"
            ),
            Self::MissingDictionaryEntry {
                encoded_variable_index,
                id,
            } => write!(
                formatter,
                "encoded variable {encoded_variable_index} references missing var.dict ID {id}"
            ),
            Self::InvalidEncodedFloat {
                encoded_variable_index,
                reason,
            } => write!(
                formatter,
                "encoded variable {encoded_variable_index} has invalid float: {reason}"
            ),
            Self::InvalidLogTypeEscape { byte_offset } => write!(
                formatter,
                "validated CLP logtype has a dangling escape at byte {byte_offset}"
            ),
            Self::OutputSizeOverflow => formatter.write_str("CLP message output size overflow"),
            Self::OutputLimitExceeded { required, limit } => write!(
                formatter,
                "CLP message requires {required} output bytes, exceeding limit {limit}"
            ),
            Self::AllocationFailed {
                requested_additional,
            } => write!(
                formatter,
                "could not reserve {requested_additional} bytes for CLP message output"
            ),
        }
    }
}

impl Error for EncodedVariableError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidEncodedFloat { reason, .. } => Some(reason),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::io::Read;
    use std::io::Take;

    use super::super::dictionary::DictionaryLimits;
    use super::super::dictionary::LogTypeDictionary;
    use super::super::dictionary::VariableDictionary;
    use super::super::dictionary::decode_logtype_dictionary;
    use super::super::dictionary::decode_variable_dictionary;
    use super::*;

    fn section(entries: &[&[u8]]) -> Vec<u8> {
        let mut encoded = u64::try_from(entries.len())
            .expect("test entry count fits u64")
            .to_le_bytes()
            .to_vec();
        if entries.is_empty() {
            return encoded;
        }
        let mut decompressed = Vec::new();
        for entry in entries {
            decompressed.extend_from_slice(
                &u64::try_from(entry.len())
                    .expect("test entry size fits u64")
                    .to_le_bytes(),
            );
            decompressed.extend_from_slice(entry);
        }
        encoded.extend_from_slice(
            &zstd::stream::encode_all(decompressed.as_slice(), 3)
                .expect("compress test dictionary"),
        );
        encoded
    }

    fn take(bytes: &[u8]) -> Take<Cursor<&[u8]>> {
        Cursor::new(bytes).take(u64::try_from(bytes.len()).expect("test section size fits u64"))
    }

    fn variable_dictionary(entries: &[&[u8]]) -> VariableDictionary {
        let bytes = section(entries);
        decode_variable_dictionary(take(&bytes), DictionaryLimits::default())
            .expect("valid variable dictionary")
    }

    fn logtype_dictionary(entries: &[&[u8]]) -> LogTypeDictionary {
        let bytes = section(entries);
        decode_logtype_dictionary(take(&bytes), DictionaryLimits::default())
            .expect("valid logtype dictionary")
    }

    fn bit_cast_id(id: u64) -> i64 {
        i64::from_le_bytes(id.to_le_bytes())
    }

    fn encoded_float(negative: bool, digits: u64, digit_count: u8, decimal_position: u8) -> i64 {
        let bits = (u64::from(negative) << 63)
            | (digits << 8)
            | (u64::from(digit_count - 1) << 4)
            | u64::from(decimal_position - 1);
        i64::from_le_bytes(bits.to_le_bytes())
    }

    #[test]
    fn reconstructs_the_cxx_mixed_variable_vector_and_escaped_markers() {
        let variables = variable_dictionary(&[b"184467440737095516150", b"python2.7.3", b"\\a1"]);
        let mut escaped_logtype = b"small ".to_vec();
        escaped_logtype.push(INTEGER_PLACEHOLDER);
        escaped_logtype.extend_from_slice(b" large ");
        escaped_logtype.push(DICTIONARY_PLACEHOLDER);
        escaped_logtype.extend_from_slice(b" double ");
        escaped_logtype.push(FLOAT_PLACEHOLDER);
        escaped_logtype.extend_from_slice(b" weird ");
        escaped_logtype.push(FLOAT_PLACEHOLDER);
        escaped_logtype.extend_from_slice(b" strings ");
        escaped_logtype.push(DICTIONARY_PLACEHOLDER);
        escaped_logtype.push(b' ');
        escaped_logtype.push(DICTIONARY_PLACEHOLDER);
        escaped_logtype.extend_from_slice(b" literals ");
        for literal in [
            ESCAPE_MARKER,
            INTEGER_PLACEHOLDER,
            FLOAT_PLACEHOLDER,
            DICTIONARY_PLACEHOLDER,
        ] {
            escaped_logtype.push(ESCAPE_MARKER);
            escaped_logtype.push(literal);
        }
        let logtypes = logtype_dictionary(&[&escaped_logtype]);
        let logtype = logtypes.entry(0).expect("logtype zero");
        let encoded_variables = [
            4938,
            bit_cast_id(0),
            encoded_float(true, 255_196_868_642_755, 15, 13),
            encoded_float(true, 0, 4, 2),
            bit_cast_id(1),
            bit_cast_id(2),
        ];
        let mut output = b"prefix:".to_vec();
        append_clp_message(
            logtype,
            &variables,
            encoded_variables.as_slice(),
            &mut output,
        )
        .expect("reconstruct CLP message");

        let mut expected = b"prefix:small 4938 large 184467440737095516150 double \
                            -25.5196868642755 weird -00.00 strings python2.7.3 \\a1 literals "
            .to_vec();
        expected.extend_from_slice(&[
            ESCAPE_MARKER,
            INTEGER_PLACEHOLDER,
            FLOAT_PLACEHOLDER,
            DICTIONARY_PLACEHOLDER,
        ]);
        assert_eq!(expected, output);
    }

    #[test]
    fn preserves_arbitrary_dictionary_bytes() {
        let variables = variable_dictionary(&[b"\xff\0binary"]);
        let logtypes = logtype_dictionary(&[b"before:\x12:after"]);
        let mut output = Vec::new();
        append_clp_message(
            logtypes.entry(0).unwrap(),
            &variables,
            &[bit_cast_id(0)][..],
            &mut output,
        )
        .unwrap();
        assert_eq!(b"before:\xff\0binary:after", output.as_slice());
    }

    #[test]
    fn restores_integer_and_custom_float_edges() {
        let variables = variable_dictionary(&[]);
        let logtypes = logtype_dictionary(&[b"\x11,\x11,\x13,\x13,\x13"]);
        let encoded_variables = [
            i64::MIN,
            i64::MAX,
            encoded_float(false, 1, 1, 1),
            encoded_float(true, 9_999_999_999_999_999, 16, 1),
            encoded_float(false, 9_999_999_999_999_999, 16, 16),
        ];
        let mut output = Vec::new();
        append_clp_message(
            logtypes.entry(0).unwrap(),
            &variables,
            encoded_variables.as_slice(),
            &mut output,
        )
        .unwrap();
        assert_eq!(
            b"-9223372036854775808,9223372036854775807,.1,-999999999999999.9,.9999999999999999",
            output.as_slice()
        );
    }

    #[test]
    fn rejects_variable_count_mismatches_without_mutation() {
        let variables = variable_dictionary(&[]);
        let logtypes = logtype_dictionary(&[b"\x11/\x13"]);
        for supplied in [&[][..], &[1][..], &[1, 2, 3][..]] {
            let mut output = b"unchanged".to_vec();
            assert!(matches!(
                append_clp_message(
                    logtypes.entry(0).unwrap(),
                    &variables,
                    supplied,
                    &mut output
                ),
                Err(EncodedVariableError::VariableCountMismatch {
                    expected: 2,
                    actual: 0 | 1 | 3
                })
            ));
            assert_eq!(b"unchanged", output.as_slice());
        }
    }

    #[test]
    fn rejects_missing_bit_cast_dictionary_id_without_mutation() {
        let variables = variable_dictionary(&[b"zero"]);
        let logtypes = logtype_dictionary(&[b"\x12"]);
        let mut output = b"unchanged".to_vec();
        assert_eq!(
            Err(EncodedVariableError::MissingDictionaryEntry {
                encoded_variable_index: 0,
                id: u64::MAX,
            }),
            append_clp_message(
                logtypes.entry(0).unwrap(),
                &variables,
                &[-1][..],
                &mut output
            )
        );
        assert_eq!(b"unchanged", output.as_slice());
    }

    #[test]
    fn rejects_each_custom_float_structural_failure_without_mutation() {
        let variables = variable_dictionary(&[]);
        let logtypes = logtype_dictionary(&[b"\x13"]);
        let cases = [
            (
                bit_cast_id(ENCODED_FLOAT_UNUSED_BIT),
                EncodedFloatErrorReason::ReservedBit,
            ),
            (
                bit_cast_id((MAX_ENCODED_FLOAT_DIGITS + 1) << 8 | (15 << 4)),
                EncodedFloatErrorReason::DigitsTooLarge,
            ),
            (
                bit_cast_id(1),
                EncodedFloatErrorReason::DecimalPositionExceedsDigits,
            ),
            (
                bit_cast_id(10 << 8),
                EncodedFloatErrorReason::DigitValueExceedsDeclaredDigits,
            ),
        ];
        for (encoded, reason) in cases {
            let mut output = b"unchanged".to_vec();
            assert_eq!(
                Err(EncodedVariableError::InvalidEncodedFloat {
                    encoded_variable_index: 0,
                    reason,
                }),
                append_clp_message(
                    logtypes.entry(0).unwrap(),
                    &variables,
                    &[encoded][..],
                    &mut output
                )
            );
            assert_eq!(b"unchanged", output.as_slice());
        }
    }

    #[test]
    fn appends_an_empty_logtype_without_changing_existing_bytes() {
        let variables = variable_dictionary(&[]);
        let logtypes = logtype_dictionary(&[b""]);
        let mut output = b"prefix".to_vec();
        append_clp_message(logtypes.entry(0).unwrap(), &variables, &[][..], &mut output).unwrap();
        assert_eq!(b"prefix", output.as_slice());
    }

    #[test]
    fn enforces_the_exact_append_limit_before_reserving_or_mutating() {
        let variables = variable_dictionary(&[b"dictionary-value"]);
        let logtypes = logtype_dictionary(&[b"prefix:\x12:suffix"]);
        let logtype = logtypes.entry(0).unwrap();
        let encoded = [bit_cast_id(0)];
        let required = b"prefix:dictionary-value:suffix".len();
        let mut output = b"unchanged".to_vec();
        output.shrink_to_fit();
        let original_capacity = output.capacity();

        assert_eq!(
            Err(EncodedVariableError::OutputLimitExceeded {
                required,
                limit: required - 1,
            }),
            append_clp_message_bounded(
                logtype,
                &variables,
                encoded.as_slice(),
                &mut output,
                required - 1,
            )
        );
        assert_eq!(b"unchanged", output.as_slice());
        assert_eq!(original_capacity, output.capacity());

        append_clp_message_bounded(
            logtype,
            &variables,
            encoded.as_slice(),
            &mut output,
            required,
        )
        .unwrap();
        assert_eq!(
            b"unchangedprefix:dictionary-value:suffix",
            output.as_slice()
        );
    }
}
