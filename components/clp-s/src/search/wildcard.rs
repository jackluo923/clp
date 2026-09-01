//! Allocation-free matching for parser-cleaned CLP wildcard patterns.

#[derive(Clone, Copy)]
enum PatternToken {
    Literal(u8),
    One,
    Many,
}

/// Matches an arbitrary byte string against a parser-cleaned wildcard pattern.
///
/// Active `*` tokens match zero or more bytes, active `?` tokens match exactly one byte, and a
/// backslash makes the following byte literal. The parser guarantees that patterns do not end in
/// a dangling backslash. Case-insensitive matching folds ASCII bytes only, matching the C.UTF-8
/// behavior characterized for CLP-S.
///
/// The matcher uses constant auxiliary space and does not allocate.
pub(super) fn wildcard_match(value: &[u8], pattern: &str, ignore_case: bool) -> bool {
    let pattern = pattern.as_bytes();
    let mut value_index = 0;
    let mut pattern_index = 0;

    // If a later token fails, the most recent `*` can consume one more byte. The pattern bookmark
    // points just past that `*`; the value bookmark is the first byte it has not consumed yet.
    let mut star_pattern_bookmark = None;
    let mut star_value_bookmark = 0;

    while value_index < value.len() {
        let Some((token, next_pattern_index)) = next_token(pattern, pattern_index) else {
            if !backtrack_after_star(
                &mut value_index,
                &mut pattern_index,
                &mut star_value_bookmark,
                star_pattern_bookmark,
                value.len(),
            ) {
                return false;
            }
            continue;
        };

        match token {
            PatternToken::Many => {
                star_pattern_bookmark = Some(next_pattern_index);
                star_value_bookmark = value_index;
                pattern_index = next_pattern_index;
            }
            PatternToken::One => {
                value_index += 1;
                pattern_index = next_pattern_index;
            }
            PatternToken::Literal(expected)
                if bytes_equal(value[value_index], expected, ignore_case) =>
            {
                value_index += 1;
                pattern_index = next_pattern_index;
            }
            PatternToken::Literal(_) => {
                if !backtrack_after_star(
                    &mut value_index,
                    &mut pattern_index,
                    &mut star_value_bookmark,
                    star_pattern_bookmark,
                    value.len(),
                ) {
                    return false;
                }
            }
        }
    }

    // Once the value is exhausted, only active `*` tokens may remain.
    while let Some((PatternToken::Many, next_pattern_index)) = next_token(pattern, pattern_index) {
        pattern_index = next_pattern_index;
    }
    pattern_index == pattern.len()
}

#[inline]
fn next_token(pattern: &[u8], index: usize) -> Option<(PatternToken, usize)> {
    let byte = *pattern.get(index)?;
    Some(match byte {
        b'*' => (PatternToken::Many, index + 1),
        b'?' => (PatternToken::One, index + 1),
        b'\\' => {
            // Cleaned patterns never contain a dangling escape. Remaining defensive here keeps
            // this safe if an internal caller violates that invariant.
            let Some(&escaped) = pattern.get(index + 1) else {
                return Some((PatternToken::Literal(b'\\'), index + 1));
            };
            (PatternToken::Literal(escaped), index + 2)
        }
        literal => (PatternToken::Literal(literal), index + 1),
    })
}

#[inline]
const fn bytes_equal(value: u8, expected: u8, ignore_case: bool) -> bool {
    value == expected || (ignore_case && value.eq_ignore_ascii_case(&expected))
}

#[inline]
const fn backtrack_after_star(
    value_index: &mut usize,
    pattern_index: &mut usize,
    star_value_bookmark: &mut usize,
    star_pattern_bookmark: Option<usize>,
    value_len: usize,
) -> bool {
    let Some(star_pattern_bookmark) = star_pattern_bookmark else {
        return false;
    };
    if *star_value_bookmark == value_len {
        return false;
    }

    *star_value_bookmark += 1;
    *value_index = *star_value_bookmark;
    *pattern_index = star_pattern_bookmark;
    true
}

#[cfg(test)]
mod tests {
    use super::wildcard_match;

    #[test]
    fn empty_and_single_wildcard_cases() {
        assert!(wildcard_match(b"", "", false));
        assert!(!wildcard_match(b"x", "", false));
        assert!(!wildcard_match(b"", "?", false));
        assert!(wildcard_match(b"", "*", false));
        assert!(wildcard_match(b"anything", "*", false));
        assert!(wildcard_match(b"abcd", "a*", false));
        assert!(wildcard_match(b"abcd", "*d", false));
        assert!(wildcard_match(b"abcd", "*b*", false));
    }

    #[test]
    fn question_mark_matches_exactly_one_byte() {
        assert!(wildcard_match(b"abcd", "a?cd", false));
        assert!(wildcard_match(b"abcd", "??cd", false));
        assert!(wildcard_match(b"abcdef", "a?c?ef", false));
        assert!(!wildcard_match(b"a", "??", false));
        assert!(wildcard_match(b"ab", "?*?", false));
        assert!(wildcard_match(b"abcd", "?b*??", false));
        assert!(!wildcard_match(b"abcd", "?a*??", false));
    }

    #[test]
    fn escaped_wildcards_and_backslashes_are_literal() {
        assert!(wildcard_match(b"a*cd", r"a\*cd", false));
        assert!(wildcard_match(b"a?cd", r"a\?cd", false));
        assert!(wildcard_match(b"a?c*e", r"a\?c\*e", false));
        assert!(wildcard_match(br"a\cd", r"a\\cd", false));
        assert!(wildcard_match(b"abc?e", r"a*\?e", false));
        assert!(wildcard_match(b"abc*e", r"a*\*e", false));
        assert!(wildcard_match(br"abc\e", r"a*\\e", false));

        // Cleaning removes the escape before an ordinary character; accepting one here preserves
        // the C++ matcher's defensive behavior as well.
        assert!(wildcard_match(b"ab?d", r"\ab?d", false));
    }

    #[test]
    fn backtracking_handles_repeated_groups() {
        let matches = [
            ("abcccd", "*ccd"),
            ("mississipissippi", "*issip*ss*"),
            ("xxxxzzzzzzzzyf", "xxxx*zzy*f"),
            ("xyxyxyzyxyz", "xy*z*xyz"),
            ("mississippi", "mi*sip*"),
            ("ababac", "*abac*"),
            ("aaazz", "a*zz*"),
            ("a12b12", "*12*12*"),
            ("aaabbaabbaab", "*aabbaa*a*"),
        ];
        for (value, pattern) in matches {
            assert!(wildcard_match(value.as_bytes(), pattern, false));
        }

        let misses = [
            ("xxxx*zzzzzzzzy*f", "xxxx*zzy*fffff"),
            ("xxxxzzzzzzzzyf", "xxxx*zzy*fffff"),
            ("a12b12", "*12*23"),
            ("a12b12", "a12b"),
            ("a*ar", "a*aar"),
        ];
        for (value, pattern) in misses {
            assert!(!wildcard_match(value.as_bytes(), pattern, false));
        }
    }

    #[test]
    fn case_folding_is_ascii_only() {
        assert!(wildcard_match(b"MiXeD", "mixed", true));
        assert!(!wildcard_match(b"MiXeD", "mixed", false));
        assert!(wildcard_match("École".as_bytes(), "ÉCOLE", true));
        assert!(!wildcard_match("École".as_bytes(), "éCOLE", true));
        assert!(wildcard_match("É".as_bytes(), "??", false));
        assert!(!wildcard_match("É".as_bytes(), "?", false));
    }

    #[test]
    fn arbitrary_non_utf8_values_match_as_bytes() {
        assert!(wildcard_match(&[0xff, b'A', 0x80], "*a*", true));
        assert!(wildcard_match(&[0xff, b'?', 0x80], r"*\?*", false));
        assert!(!wildcard_match(&[0xff], "? ?", false));
    }

    #[test]
    fn many_repeated_wildcards_do_not_require_recursion() {
        assert!(wildcard_match(
            b"aaaaaaaaaaaaaaaaab",
            "*a*a*a*a*a*a*a*a*a*a*a*a*a*a*a*a*b",
            false,
        ));
        assert!(!wildcard_match(
            b"aaaaaaaaaaaaaaaa",
            "*a*a*a*a*a*a*a*a*a*a*a*a*a*a*a*a*a*",
            false,
        ));
    }
}
