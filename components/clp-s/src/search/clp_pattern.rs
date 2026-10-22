//! Anchored string predicates over logtypes and encoded variables.
//!
//! Template literals are checked once. Per-row matching visits only the variables needed by
//! the anchor, borrowing dictionary bytes and formatting integers into a stack buffer. Complex
//! patterns and custom floats fall back to the general reconstructed-message evaluator.

use super::wildcard::exact_pattern;
use crate::archive::{ClpStringColumn, LogTypeDictionary};

#[derive(Clone, Copy, Debug, PartialEq)]
enum Anchor {
    Prefix,
    Suffix,
    Exact,
}

#[derive(Debug)]
enum Part {
    Text(Vec<u8>),
    Variable { index: usize, kind: u8 },
}

#[derive(Debug)]
enum Template {
    Constant(bool),
    Check { parts: Vec<Part>, consumed: usize },
}

pub(super) struct ClpPattern {
    anchor: Anchor,
    literal: Vec<u8>,
    ignore_case: bool,
    templates: Vec<Option<Template>>,
}

impl ClpPattern {
    pub(super) fn new(pattern: &str, ignore_case: bool, dictionary_size: usize) -> Option<Self> {
        let (anchor, literal) = if let Some(literal) = exact_pattern(pattern) {
            (Anchor::Exact, literal)
        } else if let Some(literal) = pattern.strip_prefix('*').and_then(exact_pattern) {
            (Anchor::Suffix, literal)
        } else if let Some(literal) = pattern.strip_suffix('*').and_then(exact_pattern) {
            // A final escaped star must remain literal. exact_pattern on the whole pattern
            // above handles that case unless an earlier active wildcard makes this ineligible.
            if pattern[..pattern.len() - 1]
                .bytes()
                .rev()
                .take_while(|b| *b == b'\\')
                .count()
                % 2
                != 0
            {
                return None;
            }
            (Anchor::Prefix, literal)
        } else {
            return None;
        };
        let mut templates = Vec::new();
        templates.try_reserve_exact(dictionary_size).ok()?;
        templates.resize_with(dictionary_size, || None);
        Some(Self {
            anchor,
            literal,
            ignore_case,
            templates,
        })
    }

    /// A rejection is a proof for every row using this dictionary. Variables are left unknown;
    /// only literal template mismatches can reject an entry. Empty/unloaded dictionaries keep
    /// the ordinary reader path, including its archive validation errors.
    pub(super) fn dictionary_may_match(&self, dictionary: &LogTypeDictionary) -> bool {
        dictionary.is_empty()
            || dictionary.entries().any(|entry| {
                !matches!(
                    compile_template(
                        entry.escaped_value(),
                        &self.literal,
                        self.anchor,
                        self.ignore_case
                    ),
                    Some(Template::Constant(false))
                )
            })
    }

    /// None means the original evaluator must decide (unsupported variable representation).
    pub(super) fn matches(
        &mut self,
        column: ClpStringColumn<'_, '_>,
        row: usize,
        dictionary: &LogTypeDictionary,
    ) -> Option<bool> {
        let id = column.descriptor(row)?.logtype_id() as usize;
        let slot = self.templates.get_mut(id)?;
        if slot.is_none() {
            *slot = Some(compile_template(
                dictionary.entry(id as u64)?.escaped_value(),
                &self.literal,
                self.anchor,
                self.ignore_case,
            )?);
        }
        match slot.as_ref()? {
            Template::Constant(value) => Some(*value),
            Template::Check { parts, consumed } => {
                let record = column.record(row)?;
                let variables = record.encoded_variables();
                let mut consumed = *consumed;
                let mut integer = itoa::Buffer::new();
                for part in parts {
                    let value = match part {
                        Part::Text(bytes) => bytes.as_slice(),
                        Part::Variable { index, kind } => {
                            let value = variables.get(*index)?;
                            match kind {
                                0x11 => integer.format(value).as_bytes(),
                                0x12 => column.variable_dictionary().entry(value as u64)?.value(),
                                _ => return None,
                            }
                        }
                    };
                    if !consume(
                        value,
                        &self.literal,
                        &mut consumed,
                        self.anchor,
                        self.ignore_case,
                    ) {
                        return Some(false);
                    }
                    if consumed == self.literal.len() && self.anchor != Anchor::Exact {
                        return Some(true);
                    }
                }
                Some(consumed == self.literal.len())
            }
        }
    }
}

fn compile_template(
    bytes: &[u8],
    literal: &[u8],
    anchor: Anchor,
    ignore_case: bool,
) -> Option<Template> {
    let mut parts = Vec::new();
    let mut text = Vec::new();
    let mut variables = 0;
    let mut bytes = bytes.iter().copied();
    while let Some(byte) = bytes.next() {
        match byte {
            b'\\' => text.push(bytes.next()?),
            kind @ (0x11 | 0x12 | 0x13) => {
                if !text.is_empty() {
                    parts.push(Part::Text(std::mem::take(&mut text)));
                }
                parts.push(Part::Variable {
                    index: variables,
                    kind,
                });
                variables += 1;
            }
            byte => text.push(byte),
        }
    }
    if !text.is_empty() {
        parts.push(Part::Text(text));
    }
    if anchor == Anchor::Suffix {
        parts.reverse();
    }
    let mut consumed = 0;
    let mut checked = 0;
    for part in &parts {
        if consumed == literal.len() && anchor != Anchor::Exact {
            return Some(Template::Constant(true));
        }
        let Part::Text(bytes) = part else {
            break;
        };
        if !consume(bytes, literal, &mut consumed, anchor, ignore_case) {
            return Some(Template::Constant(false));
        }
        checked += 1;
    }
    if checked == parts.len() {
        return Some(Template::Constant(consumed == literal.len()));
    }
    parts.drain(..checked);
    Some(Template::Check { parts, consumed })
}

fn consume(
    value: &[u8],
    literal: &[u8],
    consumed: &mut usize,
    anchor: Anchor,
    ignore_case: bool,
) -> bool {
    let remaining = literal.len() - *consumed;
    if anchor == Anchor::Exact && value.len() > remaining {
        return false;
    }
    let count = value.len().min(remaining);
    let (value, expected) = if anchor == Anchor::Suffix {
        (
            &value[value.len() - count..],
            &literal[remaining - count..remaining],
        )
    } else {
        (&value[..count], &literal[*consumed..*consumed + count])
    };
    if value != expected && !(ignore_case && value.eq_ignore_ascii_case(expected)) {
        return false;
    }
    *consumed += count;
    true
}

#[cfg(test)]
mod tests {
    use super::super::wildcard::wildcard_match;
    use super::*;

    // Exercise all anchor positions, including ones ending inside a variable, against the
    // independent general wildcard matcher. A dictionary variable may contain spaces, escapes,
    // placeholders, arbitrary bytes, or be empty; none are treated as template syntax.
    #[test]
    fn anchors_match_reconstructed_reference() {
        let templates: &[&[u8]] = &[
            b"",
            b"hello world",
            b"head \x11 / \x12\n",
            b"\x12\x12",
            b"literal \\*\\?\\\\ \\\x11 \x12",
            b"\x12 middle \x11 tail",
        ];
        let variables: &[&[u8]] = &[
            b"",
            b"2ms",
            b"hello world",
            b"\xff?*\\\x11",
            b"end\n",
            b"MiXeD",
            "É😀".as_bytes(),
        ];
        for template in templates {
            for var in variables {
                let mut pieces: Vec<Vec<u8>> = Vec::new();
                let mut bytes = template.iter().copied();
                while let Some(byte) = bytes.next() {
                    pieces.push(match byte {
                        b'\\' => vec![bytes.next().unwrap()],
                        0x11 => b"-9223372036854775808".to_vec(),
                        0x12 => var.to_vec(),
                        b => vec![b],
                    });
                }
                let message = pieces.concat();
                for needle in [
                    b"".as_slice(),
                    b"head ",
                    b"2ms\n",
                    b"wrong",
                    b" mixed",
                    b" tail",
                    b"*?\\",
                    message.as_slice(),
                ] {
                    for end in 0..=needle.len() {
                        let needle = &needle[..end];
                        // Patterns are UTF-8, but message bytes need not be.
                        let Ok(text) = std::str::from_utf8(needle) else {
                            continue;
                        };
                        let escaped = text
                            .replace('\\', "\\\\")
                            .replace('*', "\\*")
                            .replace('?', "\\?");
                        for anchor in [Anchor::Prefix, Anchor::Suffix, Anchor::Exact] {
                            let pattern = match anchor {
                                Anchor::Prefix => format!("{escaped}*"),
                                Anchor::Suffix => format!("*{escaped}"),
                                Anchor::Exact => escaped.clone(),
                            };
                            for ignore_case in [false, true] {
                                let plan = compile_template(template, needle, anchor, ignore_case)
                                    .unwrap();
                                let actual = match plan {
                                    Template::Constant(v) => v,
                                    Template::Check {
                                        parts,
                                        mut consumed,
                                    } => {
                                        let mut matched = true;
                                        for part in parts {
                                            let value = match &part {
                                                Part::Text(bytes) => bytes.as_slice(),
                                                Part::Variable { kind: 0x11, .. } => {
                                                    b"-9223372036854775808"
                                                }
                                                _ => var,
                                            };
                                            if !consume(
                                                value,
                                                needle,
                                                &mut consumed,
                                                anchor,
                                                ignore_case,
                                            ) {
                                                matched = false;
                                                break;
                                            }
                                            if consumed == needle.len() && anchor != Anchor::Exact {
                                                break;
                                            }
                                        }
                                        matched && consumed == needle.len()
                                    }
                                };
                                assert_eq!(
                                    actual,
                                    wildcard_match(&message, &pattern, ignore_case),
                                    "template={template:?} var={var:?} pattern={pattern:?}"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn complex_patterns_keep_general_evaluator() {
        for pattern in ["*hello*", "a?b", "a*b", r"a*\*"] {
            assert!(ClpPattern::new(pattern, false, 0).is_none(), "{pattern}");
        }
        for pattern in [
            "*",
            "hello*",
            "*hello",
            r"literal\*",
            r"literal\\*",
            r"*literal\*",
            "",
        ] {
            assert!(ClpPattern::new(pattern, false, 0).is_some(), "{pattern}");
        }
    }
}
