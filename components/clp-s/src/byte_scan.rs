//! Finding the next byte of interest in text that mostly has none.
//!
//! Reconstructing a message and escaping it as JSON both copy long runs of ordinary text between
//! a few special bytes. Testing eight bytes at a time with word arithmetic (SWAR) skips most of
//! that text; a word that may hold a special byte is then checked one byte at a time, so a false
//! alarm costs only that word.

const ONES: u64 = 0x0101_0101_0101_0101;
const HIGHS: u64 = 0x8080_8080_8080_8080;

/// Whether any byte of `word` is below `limit`, which must be at most 128.
#[inline]
const fn has_byte_below(word: u64, limit: u8) -> bool {
    0 != (word.wrapping_sub(ONES * limit as u64) & !word & HIGHS)
}

/// Whether any byte of `word` equals `byte`.
#[inline]
const fn has_byte(word: u64, byte: u8) -> bool {
    has_byte_below(word ^ (ONES * byte as u64), 1)
}

/// The position of the first byte at or after `from` for which `is_target` holds.
///
/// `word_may_contain` must hold for every eight-byte word that contains a target; it may also
/// hold for words that do not.
#[inline]
fn find_from(
    source: &[u8],
    from: usize,
    word_may_contain: impl Fn(u64) -> bool,
    is_target: impl Fn(u8) -> bool,
) -> Option<usize> {
    let mut offset = from;
    while let Some(chunk) = source.get(offset..offset + 8) {
        let word = u64::from_le_bytes(chunk.try_into().expect("eight-byte chunk"));
        if word_may_contain(word)
            && let Some(position) = chunk.iter().position(|byte| is_target(*byte))
        {
            return Some(offset + position);
        }
        offset += 8;
    }
    source[offset..]
        .iter()
        .position(|byte| is_target(*byte))
        .map(|position| offset + position)
}

/// The next byte JSON requires escaped: a control byte, `"` or `\`.
#[inline]
pub fn next_json_escape(source: &[u8], from: usize) -> Option<usize> {
    find_from(
        source,
        from,
        |word| has_byte_below(word, 0x20) || has_byte(word, b'"') || has_byte(word, b'\\'),
        |byte| byte < 0x20 || b'"' == byte || b'\\' == byte,
    )
}

/// The next CLP logtype marker: the escape `\` or a placeholder `0x11`, `0x12` or `0x13`.
#[inline]
pub fn next_logtype_marker(source: &[u8], from: usize) -> Option<usize> {
    find_from(
        source,
        from,
        // XOR with 0x10 maps 0x10..=0x13 to 0..=3, so one test covers all three placeholders and
        // flags only the rare 0x10 besides.
        |word| has_byte(word, b'\\') || has_byte_below(word ^ (ONES * 0x10), 4),
        |byte| b'\\' == byte || (0x11..=0x13).contains(&byte),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn naive(source: &[u8], from: usize, is_target: impl Fn(u8) -> bool) -> Option<usize> {
        source[from..]
            .iter()
            .position(|byte| is_target(*byte))
            .map(|position| from + position)
    }

    #[test]
    fn agrees_with_a_byte_at_a_time_scan_for_every_byte_value_and_position() {
        for special in 0_u8..=255 {
            for length in 0..20 {
                for at in 0..=length {
                    let mut source = vec![b'a'; length];
                    if at < length {
                        source[at] = special;
                    }
                    // Include high bytes, which the word tests must not mistake for low ones.
                    if 1 < length {
                        source[0] = 0xff;
                    }
                    for from in 0..=length {
                        assert_eq!(
                            naive(&source, from, |byte| byte < 0x20
                                || b'"' == byte
                                || b'\\' == byte),
                            next_json_escape(&source, from),
                            "json: byte {special:#x} at {at} of {length}, from {from}"
                        );
                        assert_eq!(
                            naive(&source, from, |byte| b'\\' == byte
                                || (0x11..=0x13).contains(&byte)),
                            next_logtype_marker(&source, from),
                            "logtype: byte {special:#x} at {at} of {length}, from {from}"
                        );
                    }
                }
            }
        }
    }
}
