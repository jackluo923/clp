//! Current C++ CLP string classification and encoded-variable construction.

use smallvec::SmallVec;

use super::primitive::AppendError;
use super::primitive::AppendResource;

pub(super) const INTEGER_PLACEHOLDER: u8 = 0x11;
pub(super) const DICTIONARY_PLACEHOLDER: u8 = 0x12;
pub(super) const FLOAT_PLACEHOLDER: u8 = 0x13;
const ESCAPE_MARKER: u8 = b'\\';
const ENCODED_FLOAT_DIGITS_MASK: u64 = (1_u64 << 54) - 1;
const MARKER_SCAN_WORD_BYTES: usize = std::mem::size_of::<u64>();
const BYTE_ONES: u64 = u64::from_ne_bytes([1; MARKER_SCAN_WORD_BYTES]);
const BYTE_HIGH_BITS: u64 = u64::from_ne_bytes([0x80; MARKER_SCAN_WORD_BYTES]);
const ESCAPE_MARKER_BYTES: u64 = u64::from_ne_bytes([ESCAPE_MARKER; MARKER_SCAN_WORD_BYTES]);
const INTEGER_PLACEHOLDER_BYTES: u64 =
    u64::from_ne_bytes([INTEGER_PLACEHOLDER; MARKER_SCAN_WORD_BYTES]);
const DICTIONARY_PLACEHOLDER_BYTES: u64 =
    u64::from_ne_bytes([DICTIONARY_PLACEHOLDER; MARKER_SCAN_WORD_BYTES]);
const FLOAT_PLACEHOLDER_BYTES: u64 =
    u64::from_ne_bytes([FLOAT_PLACEHOLDER; MARKER_SCAN_WORD_BYTES]);

#[derive(Debug)]
pub(super) struct EncodedClpString {
    pub(super) logtype: SmallVec<[u8; 128]>,
    pub(super) variables: SmallVec<[i64; 4]>,
}

#[derive(Debug)]
pub(super) struct PreencodedClpString<'a> {
    pub(super) logtype: PreencodedLogtype<'a>,
    pub(super) variables: SmallVec<[i64; 4]>,
}

#[derive(Debug)]
pub(super) enum PreencodedLogtype<'a> {
    Borrowed(&'a [u8]),
    Owned(SmallVec<[u8; 128]>),
}

impl PreencodedLogtype<'_> {
    pub(super) fn as_slice(&self) -> &[u8] {
        match self {
            Self::Borrowed(value) => value,
            Self::Owned(value) => value,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PreencodedWidth {
    FourByte,
    EightByte,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PreencodedVariable {
    FourByte(i32),
    EightByte(i64),
}

/// Matches `JsonParser`'s deliberately byte-oriented string classification.
///
/// Only a literal ASCII space selects CLP encoding. Tabs, newlines, and Unicode whitespace do not.
pub(super) fn node_is_clp_string(value: &[u8]) -> bool {
    value.contains(&b' ')
}

pub(super) fn encode_clp_string<F>(
    message: &[u8],
    max_logtype_bytes: u64,
    max_variables: u64,
    mut resolve_dictionary: F,
) -> Result<EncodedClpString, AppendError>
where
    F: FnMut(&[u8]) -> Result<u64, AppendError>, {
    let reserve = message
        .len()
        .min(usize::try_from(max_logtype_bytes).unwrap_or(usize::MAX));
    let mut logtype = SmallVec::new();
    logtype
        .try_reserve(reserve)
        .map_err(|_| allocation_failed(AppendResource::DictionaryValueBytes, reserve))?;
    let mut variables = SmallVec::new();
    let mut previous_end = 0_usize;
    let mut scan_end = 0_usize;
    let mut constant_escapes = 0_usize;
    let contains_markers = contains_marker(message);
    loop {
        let variable = if contains_markers {
            next_variable_with_markers(message, scan_end, &mut constant_escapes)
        } else {
            next_variable_without_markers(message, scan_end)
        };
        let Some(variable) = variable else {
            break;
        };
        append_escaped_constant(
            &message[previous_end..variable.begin],
            constant_escapes,
            &mut logtype,
            max_logtype_bytes,
        )?;
        let token = &message[variable.begin..variable.end];
        let (placeholder, encoded) = if let Some(integer) = encode_integer(token) {
            (INTEGER_PLACEHOLDER, integer)
        } else if let Some(float) = encode_float(token) {
            (FLOAT_PLACEHOLDER, float)
        } else {
            let id = resolve_dictionary(token)?;
            (DICTIONARY_PLACEHOLDER, i64::from_le_bytes(id.to_le_bytes()))
        };
        append_logtype_byte(placeholder, &mut logtype, max_logtype_bytes)?;
        let proposed = u64::try_from(variables.len())
            .map_err(|_| AppendError::SizeOverflow)?
            .checked_add(1)
            .ok_or(AppendError::SizeOverflow)?;
        if proposed > max_variables {
            return Err(AppendError::LimitExceeded {
                resource: AppendResource::EncodedVariablesPerColumn,
                actual: proposed,
                limit: max_variables,
            });
        }
        variables
            .try_reserve(1)
            .map_err(|_| allocation_failed(AppendResource::EncodedVariablesPerColumn, 1))?;
        variables.push(encoded);
        previous_end = variable.end;
        scan_end = variable.end;
        constant_escapes = 0;
    }
    append_escaped_constant(
        &message[previous_end..],
        constant_escapes,
        &mut logtype,
        max_logtype_bytes,
    )?;
    Ok(EncodedClpString { logtype, variables })
}

/// Converts a validated KV-IR encoded-text AST directly into the archive's eight-byte CLP form.
///
/// This mirrors the C++ `EncodedVariableInterpreter` path: four-byte integer and float variables
/// are widened, eight-byte dictionary variables remain dictionary variables, and four-byte
/// dictionary variables are reclassified as integers or floats when possible. The caller must
/// validate placeholder counts, escape syntax, and encoded-float domains before calling this
/// function.
#[allow(clippy::too_many_lines)]
pub(super) fn encode_preencoded_clp_string<'a, E, D, F>(
    source_logtype: &'a [u8],
    width: PreencodedWidth,
    encoded_variables: E,
    dictionary_variables: D,
    max_logtype_bytes: u64,
    max_variables: u64,
    mut resolve_dictionary: F,
) -> Result<PreencodedClpString<'a>, AppendError>
where
    E: IntoIterator<Item = PreencodedVariable>,
    D: IntoIterator<Item = &'a [u8]>,
    F: FnMut(&[u8]) -> Result<u64, AppendError>, {
    let mut owned_logtype = None;
    let mut logtype_len = 0_usize;
    let mut variables = SmallVec::new();
    let mut encoded = encoded_variables.into_iter();
    let mut dictionaries = dictionary_variables.into_iter();
    let mut position = 0_usize;
    while position < source_logtype.len() {
        let byte = source_logtype[position];
        if ESCAPE_MARKER == byte {
            let escaped = source_logtype
                .get(position + 1)
                .copied()
                .ok_or(AppendError::SizeOverflow)?;
            append_preencoded_logtype_byte(
                ESCAPE_MARKER,
                &mut owned_logtype,
                &mut logtype_len,
                max_logtype_bytes,
            )?;
            append_preencoded_logtype_byte(
                escaped,
                &mut owned_logtype,
                &mut logtype_len,
                max_logtype_bytes,
            )?;
            position = position.checked_add(2).ok_or(AppendError::SizeOverflow)?;
            continue;
        }
        if !matches!(
            byte,
            INTEGER_PLACEHOLDER | DICTIONARY_PLACEHOLDER | FLOAT_PLACEHOLDER
        ) {
            append_preencoded_logtype_byte(
                byte,
                &mut owned_logtype,
                &mut logtype_len,
                max_logtype_bytes,
            )?;
            position += 1;
            continue;
        }

        let (placeholder, variable) = match byte {
            INTEGER_PLACEHOLDER => {
                let value = encoded.next().ok_or(AppendError::SizeOverflow)?;
                let widened = match (width, value) {
                    (PreencodedWidth::FourByte, PreencodedVariable::FourByte(value)) => {
                        i64::from(value)
                    }
                    (PreencodedWidth::EightByte, PreencodedVariable::EightByte(value)) => value,
                    _ => return Err(AppendError::SizeOverflow),
                };
                (INTEGER_PLACEHOLDER, widened)
            }
            FLOAT_PLACEHOLDER => {
                let value = encoded.next().ok_or(AppendError::SizeOverflow)?;
                let widened = match (width, value) {
                    (PreencodedWidth::FourByte, PreencodedVariable::FourByte(value)) => {
                        widen_four_byte_float(value)
                    }
                    (PreencodedWidth::EightByte, PreencodedVariable::EightByte(value)) => value,
                    _ => return Err(AppendError::SizeOverflow),
                };
                (FLOAT_PLACEHOLDER, widened)
            }
            DICTIONARY_PLACEHOLDER => {
                let value = dictionaries.next().ok_or(AppendError::SizeOverflow)?;
                if matches!(width, PreencodedWidth::FourByte)
                    && let Some(integer) = encode_integer(value)
                {
                    (INTEGER_PLACEHOLDER, integer)
                } else if matches!(width, PreencodedWidth::FourByte)
                    && let Some(float) = encode_float(value)
                {
                    (FLOAT_PLACEHOLDER, float)
                } else {
                    let id = resolve_dictionary(value)?;
                    (DICTIONARY_PLACEHOLDER, i64::from_le_bytes(id.to_le_bytes()))
                }
            }
            _ => unreachable!("all CLP placeholders are matched above"),
        };
        if placeholder != byte && owned_logtype.is_none() {
            owned_logtype = Some(copy_preencoded_logtype_prefix(
                source_logtype,
                position,
                max_logtype_bytes,
            )?);
        }
        append_preencoded_variable(
            placeholder,
            variable,
            &mut owned_logtype,
            &mut logtype_len,
            &mut variables,
            max_logtype_bytes,
            max_variables,
        )?;
        position += 1;
    }
    debug_assert_eq!(source_logtype.len(), logtype_len);
    let logtype = owned_logtype.map_or(
        PreencodedLogtype::Borrowed(source_logtype),
        PreencodedLogtype::Owned,
    );
    Ok(PreencodedClpString { logtype, variables })
}

fn copy_preencoded_logtype_prefix(
    source: &[u8],
    prefix_len: usize,
    limit: u64,
) -> Result<SmallVec<[u8; 128]>, AppendError> {
    let reserve = source
        .len()
        .min(usize::try_from(limit).unwrap_or(usize::MAX));
    let mut logtype = SmallVec::new();
    logtype
        .try_reserve(reserve)
        .map_err(|_| allocation_failed(AppendResource::DictionaryValueBytes, reserve))?;
    logtype.extend_from_slice(source.get(..prefix_len).ok_or(AppendError::SizeOverflow)?);
    Ok(logtype)
}

fn append_preencoded_variable(
    placeholder: u8,
    value: i64,
    logtype: &mut Option<SmallVec<[u8; 128]>>,
    logtype_len: &mut usize,
    variables: &mut SmallVec<[i64; 4]>,
    max_logtype_bytes: u64,
    max_variables: u64,
) -> Result<(), AppendError> {
    append_preencoded_logtype_byte(placeholder, logtype, logtype_len, max_logtype_bytes)?;
    let proposed = u64::try_from(variables.len())
        .map_err(|_| AppendError::SizeOverflow)?
        .checked_add(1)
        .ok_or(AppendError::SizeOverflow)?;
    if proposed > max_variables {
        return Err(AppendError::LimitExceeded {
            resource: AppendResource::EncodedVariablesPerColumn,
            actual: proposed,
            limit: max_variables,
        });
    }
    variables
        .try_reserve(1)
        .map_err(|_| allocation_failed(AppendResource::EncodedVariablesPerColumn, 1))?;
    variables.push(value);
    Ok(())
}

fn append_preencoded_logtype_byte(
    byte: u8,
    owned: &mut Option<SmallVec<[u8; 128]>>,
    current_len: &mut usize,
    limit: u64,
) -> Result<(), AppendError> {
    let resulting = current_len
        .checked_add(1)
        .ok_or(AppendError::SizeOverflow)?;
    check_logtype_size(resulting, limit)?;
    if let Some(logtype) = owned {
        logtype
            .try_reserve(1)
            .map_err(|_| allocation_failed(AppendResource::DictionaryValueBytes, 1))?;
        logtype.push(byte);
    }
    *current_len = resulting;
    Ok(())
}

fn widen_four_byte_float(value: i32) -> i64 {
    let encoded = u32::from_ne_bytes(value.to_ne_bytes());
    let negative = u64::from(encoded >> 31);
    let digits = u64::from((encoded >> 6) & ((1_u32 << 25) - 1));
    let digit_count_minus_one = u64::from((encoded >> 3) & 0x07);
    let decimal_position_minus_one = u64::from(encoded & 0x07);
    let widened = (negative << 63)
        | (digits << 8)
        | (digit_count_minus_one << 4)
        | decimal_position_minus_one;
    i64::from_ne_bytes(widened.to_ne_bytes())
}

fn append_escaped_constant(
    constant: &[u8],
    escapes: usize,
    logtype: &mut SmallVec<[u8; 128]>,
    limit: u64,
) -> Result<(), AppendError> {
    let additional = constant
        .len()
        .checked_add(escapes)
        .ok_or(AppendError::SizeOverflow)?;
    let resulting = logtype
        .len()
        .checked_add(additional)
        .ok_or(AppendError::SizeOverflow)?;
    check_logtype_size(resulting, limit)?;
    logtype
        .try_reserve(additional)
        .map_err(|_| allocation_failed(AppendResource::DictionaryValueBytes, additional))?;
    if 0 == escapes {
        logtype.extend_from_slice(constant);
        return Ok(());
    }
    for byte in constant {
        if is_marker(*byte) {
            logtype.push(ESCAPE_MARKER);
        }
        logtype.push(*byte);
    }
    Ok(())
}

fn append_logtype_byte(
    byte: u8,
    logtype: &mut SmallVec<[u8; 128]>,
    limit: u64,
) -> Result<(), AppendError> {
    let resulting = logtype
        .len()
        .checked_add(1)
        .ok_or(AppendError::SizeOverflow)?;
    check_logtype_size(resulting, limit)?;
    logtype
        .try_reserve(1)
        .map_err(|_| allocation_failed(AppendResource::DictionaryValueBytes, 1))?;
    logtype.push(byte);
    Ok(())
}

fn check_logtype_size(actual: usize, limit: u64) -> Result<(), AppendError> {
    let actual = u64::try_from(actual).map_err(|_| AppendError::SizeOverflow)?;
    if actual > limit {
        Err(AppendError::LimitExceeded {
            resource: AppendResource::DictionaryEntryBytes,
            actual,
            limit,
        })
    } else {
        Ok(())
    }
}

const fn is_marker(byte: u8) -> bool {
    matches!(
        byte,
        ESCAPE_MARKER | INTEGER_PLACEHOLDER | DICTIONARY_PLACEHOLDER | FLOAT_PLACEHOLDER
    )
}

fn contains_marker(bytes: &[u8]) -> bool {
    let (words, remainder) = bytes.as_chunks::<MARKER_SCAN_WORD_BYTES>();
    words
        .iter()
        .any(|bytes| word_contains_marker(u64::from_ne_bytes(*bytes)))
        || remainder.iter().copied().any(is_marker)
}

const fn word_contains_marker(word: u64) -> bool {
    0 != word_zero_byte_high_bits(word ^ ESCAPE_MARKER_BYTES)
        | word_zero_byte_high_bits(word ^ INTEGER_PLACEHOLDER_BYTES)
        | word_zero_byte_high_bits(word ^ DICTIONARY_PLACEHOLDER_BYTES)
        | word_zero_byte_high_bits(word ^ FLOAT_PLACEHOLDER_BYTES)
}

const fn word_zero_byte_high_bits(word: u64) -> u64 {
    word.wrapping_sub(BYTE_ONES) & !word & BYTE_HIGH_BITS
}

#[derive(Clone, Copy)]
struct VariableSpan {
    begin: usize,
    end: usize,
}

fn next_variable_with_markers(
    message: &[u8],
    mut end: usize,
    constant_escapes: &mut usize,
) -> Option<VariableSpan> {
    while end < message.len() {
        let mut begin = end;
        while begin < message.len() && is_delimiter(message[begin]) {
            *constant_escapes += usize::from(is_marker(message[begin]));
            begin += 1;
        }
        if begin == message.len() {
            return None;
        }

        let mut contains_decimal_digit = false;
        let mut contains_alphabet = false;
        let mut all_hexadecimal = true;
        let mut token_escapes = 0_usize;
        end = begin;
        while end < message.len() {
            let byte = message[end];
            if byte.is_ascii_digit() {
                contains_decimal_digit = true;
            } else if byte.is_ascii_alphabetic() {
                contains_alphabet = true;
            } else if is_delimiter(byte) {
                break;
            }
            all_hexadecimal &= byte.is_ascii_hexdigit();
            token_escapes += usize::from(is_marker(byte));
            end += 1;
        }
        if contains_decimal_digit
            || (0 < begin && b'=' == message[begin - 1] && contains_alphabet)
            || (2 <= end - begin && all_hexadecimal)
        {
            return Some(VariableSpan { begin, end });
        }
        *constant_escapes += token_escapes;
    }
    None
}

fn next_variable_without_markers(message: &[u8], mut end: usize) -> Option<VariableSpan> {
    while end < message.len() {
        let mut begin = end;
        while begin < message.len() && is_delimiter(message[begin]) {
            begin += 1;
        }
        if begin == message.len() {
            return None;
        }

        let mut contains_decimal_digit = false;
        let mut contains_alphabet = false;
        end = begin;
        while end < message.len() {
            let byte = message[end];
            if byte.is_ascii_digit() {
                contains_decimal_digit = true;
            } else if byte.is_ascii_alphabetic() {
                contains_alphabet = true;
            } else if is_delimiter(byte) {
                break;
            }
            end += 1;
        }
        if contains_decimal_digit
            || (0 < begin && b'=' == message[begin - 1] && contains_alphabet)
            || could_be_multi_digit_hex_value(&message[begin..end])
        {
            return Some(VariableSpan { begin, end });
        }
    }
    None
}

fn could_be_multi_digit_hex_value(value: &[u8]) -> bool {
    if value.len() < 2 {
        return false;
    }
    for byte in value {
        if !byte.is_ascii_hexdigit() {
            return false;
        }
    }
    true
}

const fn is_delimiter(byte: u8) -> bool {
    !matches!(
        byte,
        b'+' | b'-' | b'.' | b'0'..=b'9' | b'A'..=b'Z' | b'\\' | b'_' | b'a'..=b'z'
    )
}

fn encode_integer(value: &[u8]) -> Option<i64> {
    let (negative, start) = if value.first() == Some(&b'-') {
        if !value
            .get(1)
            .is_some_and(|byte| (b'1'..=b'9').contains(byte))
        {
            return None;
        }
        (true, 1)
    } else {
        let first = *value.first()?;
        if !first.is_ascii_digit() || 1 < value.len() && b'0' == first {
            return None;
        }
        (false, 0)
    };
    let mut magnitude = 0_u64;
    for digit in &value[start..] {
        if !digit.is_ascii_digit() {
            return None;
        }
        magnitude = magnitude
            .checked_mul(10)?
            .checked_add(u64::from(*digit - b'0'))?;
    }
    if negative {
        if magnitude == 1_u64 << 63 {
            Some(i64::MIN)
        } else {
            let magnitude = i64::try_from(magnitude).ok()?;
            Some(-magnitude)
        }
    } else {
        i64::try_from(magnitude).ok()
    }
}

fn encode_float(value: &[u8]) -> Option<i64> {
    if value.is_empty() {
        return None;
    }
    let negative = b'-' == value[0];
    let start = usize::from(negative);
    let max_length = 17_usize + usize::from(negative);
    if value.len() > max_length {
        return None;
    }

    let mut digits = 0_u64;
    let mut digit_count = 0_usize;
    let mut decimal_position = None;
    for (position, byte) in value.iter().copied().enumerate().skip(start) {
        if byte.is_ascii_digit() {
            digits = digits
                .checked_mul(10)?
                .checked_add(u64::from(byte - b'0'))?;
            digit_count += 1;
        } else if b'.' == byte && decimal_position.is_none() {
            decimal_position = value.len().checked_sub(position + 1);
        } else {
            return None;
        }
    }
    let decimal_position = decimal_position?;
    if 0 == decimal_position || 0 == digit_count || 16 < digit_count {
        return None;
    }

    let mut encoded = u64::from(negative);
    encoded <<= 55;
    encoded |= digits & ENCODED_FLOAT_DIGITS_MASK;
    encoded <<= 4;
    encoded |= u64::try_from(digit_count - 1).ok()? & 0x0f;
    encoded <<= 4;
    encoded |= u64::try_from(decimal_position - 1).ok()? & 0x0f;
    Some(i64::from_le_bytes(encoded.to_le_bytes()))
}

const fn allocation_failed(resource: AppendResource, requested: usize) -> AppendError {
    AppendError::AllocationFailed {
        resource,
        requested,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode(message: &[u8]) -> EncodedClpString {
        encode_clp_string(message, u64::MAX, u64::MAX, |_| Ok(7)).expect("encode CLP string")
    }

    fn assert_marker_free_scanners_match(message: &[u8]) {
        assert!(!contains_marker(message), "test input contains a marker");
        let mut marker_free_end = 0_usize;
        let mut marker_aware_end = 0_usize;
        let mut constant_escapes = 0_usize;
        loop {
            let marker_free = next_variable_without_markers(message, marker_free_end);
            let marker_aware =
                next_variable_with_markers(message, marker_aware_end, &mut constant_escapes);
            assert_eq!(
                marker_free.map(|span| (span.begin, span.end)),
                marker_aware.map(|span| (span.begin, span.end)),
                "scanner mismatch for {message:?}"
            );
            assert_eq!(
                0, constant_escapes,
                "marker-aware scanner counted an escape in {message:?}"
            );
            let Some(span) = marker_free else {
                break;
            };
            marker_free_end = span.end;
            marker_aware_end = span.end;
        }
    }

    #[test]
    fn word_marker_detector_matches_scalar_for_every_adjacent_byte_pair() {
        for first in u8::MIN..=u8::MAX {
            for second in u8::MIN..=u8::MAX {
                for position in 0..MARKER_SCAN_WORD_BYTES - 1 {
                    let mut bytes = [b'a'; MARKER_SCAN_WORD_BYTES];
                    bytes[position] = first;
                    bytes[position + 1] = second;
                    assert_eq!(
                        bytes.iter().copied().any(is_marker),
                        word_contains_marker(u64::from_ne_bytes(bytes)),
                        "bytes {first:#04x}, {second:#04x} at word position {position}"
                    );
                }
            }
        }
    }

    #[test]
    fn marker_detector_handles_every_word_and_tail_position() {
        let markers = [
            ESCAPE_MARKER,
            INTEGER_PLACEHOLDER,
            DICTIONARY_PLACEHOLDER,
            FLOAT_PLACEHOLDER,
        ];
        for prefix_len in 0..MARKER_SCAN_WORD_BYTES {
            let body_len = 3 * MARKER_SCAN_WORD_BYTES + 1;
            let mut source = vec![b'a'; prefix_len + body_len];
            let body = &source[prefix_len..];
            assert!(!contains_marker(body));
            for position in 0..body_len {
                for marker in markers {
                    source[prefix_len + position] = marker;
                    assert!(
                        contains_marker(&source[prefix_len..]),
                        "marker {marker:#04x} at position {position} after prefix {prefix_len}"
                    );
                    source[prefix_len + position] = b'a';
                }
            }
        }
    }

    #[test]
    fn marker_free_scanner_matches_marker_aware_fallback_for_all_byte_pairs() {
        for first in u8::MIN..=u8::MAX {
            if is_marker(first) {
                continue;
            }
            assert_marker_free_scanners_match(&[first]);
            assert_marker_free_scanners_match(&[b'=', first]);
            assert_marker_free_scanners_match(&[b' ', first, b' ']);
            for second in u8::MIN..=u8::MAX {
                if is_marker(second) {
                    continue;
                }
                assert_marker_free_scanners_match(&[first, second]);
                assert_marker_free_scanners_match(&[b'=', first, second]);
                assert_marker_free_scanners_match(&[b' ', first, second, b' ']);
                assert_marker_free_scanners_match(&[b'a', first, second, b'z']);
            }
        }
    }

    #[test]
    fn classification_matches_literal_ascii_space_policy() {
        assert!(node_is_clp_string(b"a b"));
        assert!(!node_is_clp_string(b"a\tb"));
        assert!(!node_is_clp_string(b"a\nb"));
        assert!(!node_is_clp_string("a\u{a0}b".as_bytes()));
        assert!(!node_is_clp_string("a\u{2003}b".as_bytes()));
    }

    #[test]
    fn common_results_stay_inline_and_large_results_spill_without_truncation() {
        let inline = encode(b"worker 1 completed request 2 in 3.000 ms");
        assert!(!inline.logtype.spilled());
        assert!(!inline.variables.spilled());

        let large_constant = vec![b'g'; 129];
        let large = encode(&large_constant);
        assert!(large.logtype.spilled());
        assert_eq!(large_constant, large.logtype.as_slice());

        let many_variables = encode(b"0 1 2 3 4");
        assert!(many_variables.variables.spilled());
        assert_eq!([0, 1, 2, 3, 4], many_variables.variables.as_slice());
    }

    #[test]
    fn restores_integer_float_and_dictionary_variable_forms() {
        let encoded = encode(b"count -42 ratio 001.2300 user=alice overflow 9223372036854775808");
        assert_eq!(
            b"count \x11 ratio \x13 user=\x12 overflow \x12",
            encoded.logtype.as_slice()
        );
        assert_eq!(-42, encoded.variables[0]);
        assert_eq!(7, encoded.variables[2]);
        assert_eq!(7, encoded.variables[3]);
        assert_eq!(Some(encoded.variables[1]), encode_float(b"001.2300"));
    }

    #[test]
    fn preencoded_four_byte_values_are_widened_and_dictionary_values_are_reclassified() {
        let four_byte_float = {
            let negative = 1_u32;
            let digits = 1234_u32;
            let digit_count_minus_one = 3_u32;
            let decimal_position_minus_one = 1_u32;
            let encoded = (((negative << 25) | digits) << 3 | digit_count_minus_one) << 3
                | decimal_position_minus_one;
            i32::from_ne_bytes(encoded.to_ne_bytes())
        };
        let encoded = encode_preencoded_clp_string(
            b"integer=\x11 float=\x13 numeric-dictionary=\x12 escaped=\\\x11",
            PreencodedWidth::FourByte,
            [
                PreencodedVariable::FourByte(-42),
                PreencodedVariable::FourByte(four_byte_float),
            ],
            [b"123".as_slice()],
            u64::MAX,
            u64::MAX,
            |_| panic!("numeric four-byte dictionary variables must not enter the dictionary"),
        )
        .expect("convert four-byte encoded text");
        assert_eq!(
            b"integer=\x11 float=\x13 numeric-dictionary=\x11 escaped=\\\x11",
            encoded.logtype.as_slice()
        );
        assert!(matches!(&encoded.logtype, PreencodedLogtype::Owned(_)));
        assert_eq!(
            [-42, encode_float(b"-12.34").expect("encoded float"), 123],
            encoded.variables.as_slice()
        );
    }

    #[test]
    fn preencoded_eight_byte_dictionary_values_remain_dictionary_values() {
        let encoded = encode_preencoded_clp_string(
            b"integer=\x11 numeric-dictionary=\x12",
            PreencodedWidth::EightByte,
            [PreencodedVariable::EightByte(-42)],
            [b"123".as_slice()],
            u64::MAX,
            u64::MAX,
            |value| {
                assert_eq!(b"123", value);
                Ok(17)
            },
        )
        .expect("convert eight-byte encoded text");
        assert_eq!(
            b"integer=\x11 numeric-dictionary=\x12",
            encoded.logtype.as_slice()
        );
        assert!(matches!(&encoded.logtype, PreencodedLogtype::Borrowed(_)));
        assert_eq!([-42, 17], encoded.variables.as_slice());
    }

    #[test]
    fn preencoded_four_byte_nonnumeric_dictionary_values_stay_dictionary_values() {
        let encoded = encode_preencoded_clp_string(
            b"name=\x12",
            PreencodedWidth::FourByte,
            std::iter::empty(),
            [b"alice".as_slice()],
            u64::MAX,
            u64::MAX,
            |value| {
                assert_eq!(b"alice", value);
                Ok(23)
            },
        )
        .expect("convert nonnumeric four-byte dictionary value");
        assert_eq!(b"name=\x12", encoded.logtype.as_slice());
        assert!(matches!(&encoded.logtype, PreencodedLogtype::Borrowed(_)));
        assert_eq!([23], encoded.variables.as_slice());
    }

    #[test]
    fn preencoded_eight_byte_float_bits_are_preserved_exactly() {
        let raw = (1_u64 << 62) | (125_u64 << 8) | (2_u64 << 4) | 1;
        let raw = i64::from_ne_bytes(raw.to_ne_bytes());
        let encoded = encode_preencoded_clp_string(
            b"float=\x13",
            PreencodedWidth::EightByte,
            [PreencodedVariable::EightByte(raw)],
            std::iter::empty(),
            u64::MAX,
            u64::MAX,
            |_| unreachable!("float values do not use the variable dictionary"),
        )
        .expect("convert eight-byte encoded float");
        assert_eq!(b"float=\x13", encoded.logtype.as_slice());
        assert_eq!([raw], encoded.variables.as_slice());
    }

    #[test]
    fn preencoded_limits_are_checked_at_exact_boundaries() {
        encode_preencoded_clp_string(
            b"a\x11",
            PreencodedWidth::FourByte,
            [PreencodedVariable::FourByte(1)],
            std::iter::empty(),
            2,
            1,
            |_| unreachable!("integer values do not use the variable dictionary"),
        )
        .expect("exact logtype and variable limits are accepted");

        assert!(matches!(
            encode_preencoded_clp_string(
                b"a\x11",
                PreencodedWidth::FourByte,
                [PreencodedVariable::FourByte(1)],
                std::iter::empty(),
                1,
                1,
                |_| unreachable!("integer values do not use the variable dictionary"),
            ),
            Err(AppendError::LimitExceeded {
                resource: AppendResource::DictionaryEntryBytes,
                actual: 2,
                limit: 1,
            })
        ));
        assert!(matches!(
            encode_preencoded_clp_string(
                b"\x11\x11",
                PreencodedWidth::FourByte,
                [
                    PreencodedVariable::FourByte(1),
                    PreencodedVariable::FourByte(2),
                ],
                std::iter::empty(),
                2,
                1,
                |_| unreachable!("integer values do not use the variable dictionary"),
            ),
            Err(AppendError::LimitExceeded {
                resource: AppendResource::EncodedVariablesPerColumn,
                actual: 2,
                limit: 1,
            })
        ));
        assert!(matches!(
            encode_preencoded_clp_string(
                b"a\x11z",
                PreencodedWidth::FourByte,
                [PreencodedVariable::FourByte(1)],
                std::iter::empty(),
                2,
                0,
                |_| unreachable!("integer values do not use the variable dictionary"),
            ),
            Err(AppendError::LimitExceeded {
                resource: AppendResource::EncodedVariablesPerColumn,
                actual: 1,
                limit: 0,
            })
        ));
        assert!(matches!(
            encode_preencoded_clp_string(
                b"\\a",
                PreencodedWidth::EightByte,
                std::iter::empty(),
                std::iter::empty(),
                1,
                0,
                |_| unreachable!("constant logtypes do not use the variable dictionary"),
            ),
            Err(AppendError::LimitExceeded {
                resource: AppendResource::DictionaryEntryBytes,
                actual: 2,
                limit: 1,
            })
        ));
    }

    #[test]
    fn integer_conversion_matches_cpp_canonical_rules_and_range() {
        assert_eq!(Some(0), encode_integer(b"0"));
        assert_eq!(Some(i64::MIN), encode_integer(b"-9223372036854775808"));
        assert_eq!(Some(i64::MAX), encode_integer(b"9223372036854775807"));
        for value in [
            b"-0".as_slice(),
            b"01",
            b"+1",
            b"9223372036854775808",
            b"-9223372036854775809",
        ] {
            assert_eq!(None, encode_integer(value), "{value:?}");
        }
    }

    #[test]
    fn float_conversion_matches_cpp_custom_format_boundaries() {
        for value in [
            b".123".as_slice(),
            b"-.123",
            b"0.0",
            b"001.2300",
            b"123456789012345.6",
        ] {
            assert!(encode_float(value).is_some(), "{value:?}");
        }
        for value in [
            b"1".as_slice(),
            b"1.",
            b"+1.0",
            b"1e2",
            b"1234567890123456.7",
        ] {
            assert_eq!(None, encode_float(value), "{value:?}");
        }
        assert_eq!(Some(0x100), encode_float(b".1"));
        assert_eq!(Some(0x27_0f31), encode_float(b"99.99"));
        assert_eq!(
            Some(i64::from_le_bytes(0x8000_0000_0000_0031_u64.to_le_bytes())),
            encode_float(b"-00.00")
        );
    }

    #[test]
    fn noncanonical_numeric_and_token_rule_matches_fall_back_to_dictionary() {
        let message = b"01 -0 +1 1. 1e3 9223372036854775808 ff deadbeef =foo abc123";
        let mut dictionary = Vec::<Vec<u8>>::new();
        let encoded = encode_clp_string(message, u64::MAX, u64::MAX, |value| {
            let id = u64::try_from(dictionary.len()).expect("test dictionary ID");
            dictionary.push(value.to_vec());
            Ok(id)
        })
        .expect("encode fallback matrix");
        let expected_entries: &[&[u8]] = &[
            b"01",
            b"-0",
            b"+1",
            b"1.",
            b"1e3",
            b"9223372036854775808",
            b"ff",
            b"deadbeef",
            b"foo",
            b"abc123",
        ];
        assert_eq!(
            expected_entries,
            dictionary.iter().map(Vec::as_slice).collect::<Vec<_>>()
        );
        assert_eq!(
            b"\x12 \x12 \x12 \x12 \x12 \x12 \x12 \x12 =\x12 \x12",
            encoded.logtype.as_slice()
        );
        assert_eq!(
            (0_i64..10).collect::<Vec<_>>().as_slice(),
            encoded.variables.as_slice()
        );
    }

    #[test]
    fn escapes_literal_markers_and_backslashes() {
        let encoded = encode(b"literal \\ \x11 \x12 \x13 9");
        assert_eq!(
            b"literal \\\\ \\\x11 \\\x12 \\\x13 \x11",
            encoded.logtype.as_slice()
        );
        assert_eq!([9], encoded.variables.as_slice());
    }
}
