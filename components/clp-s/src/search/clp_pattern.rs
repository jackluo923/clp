//! Anchored string predicates over logtypes and encoded variables.
//!
//! Template literals are checked once. Per-row matching visits only the variables needed by
//! the anchor, borrowing dictionary bytes and formatting integers into a stack buffer. Complex
//! patterns and custom floats fall back to the general reconstructed-message evaluator.

use super::wildcard::exact_pattern;
use crate::archive::{ClpStringColumn, LogTypeDictionary, VariableDictionary};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

const PLAN_BUDGET: usize = 8 << 20;

/// Shared only by compilations of the same query against the same archive. The dictionary
/// length is part of the key because projection may load dictionaries after the first compile.
#[derive(Debug, Default)]
pub(super) struct PatternCache {
    entries: HashMap<(String, bool, usize), HashMap<Vec<u8>, Arc<PreparedTemplate>>>,
    bytes: usize,
}

pub(super) type SharedPatternCache = Arc<Mutex<PatternCache>>;

#[derive(Debug)]
struct PreparedTemplate {
    template: Template,
    dictionary: Option<DictionaryPlan>,
}

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
    templates: Vec<Option<Arc<PreparedTemplate>>>,
    cache: Option<SharedPatternCache>,
    cache_key: (String, bool, usize),
    dictionary_plan_bytes: usize,
}

#[derive(Debug)]
struct DictionaryPlan {
    index: usize,
    len: usize,
    matches: Vec<u64>,
    any_match: bool,
    /// False means the mask only rejects impossible boundary values; survivors
    /// still need the remaining variables checked.
    complete: bool,
}

impl DictionaryPlan {
    fn build(
        template: &Template,
        literal: &[u8],
        anchor: Anchor,
        ignore_case: bool,
        dictionary: &crate::archive::VariableDictionary,
        budget: &mut usize,
    ) -> Option<Self> {
        let Template::Check { parts, consumed } = template else {
            return None;
        };
        let mut variables = parts.iter().filter_map(|p| match p {
            Part::Variable { index, kind } => Some((*index, *kind)),
            _ => None,
        });
        let (index, 0x12) = variables.next()? else {
            return None;
        };
        let complete = variables.next().is_none();
        let words = dictionary.len().div_ceil(64);
        let bytes = words.checked_mul(8)?;
        // Bound optional per-template acceleration independently of archive dictionary limits.
        if bytes > (8 << 20usize) - *budget {
            return None;
        }
        let mut matches = Vec::new();
        matches.try_reserve_exact(words).ok()?;
        matches.resize(words, 0);
        let mut any_match = false;
        for entry in dictionary.entries() {
            if single_variable_matches(
                parts,
                *consumed,
                entry.value(),
                literal,
                anchor,
                ignore_case,
            ) {
                any_match = true;
                let id = entry.id() as usize;
                matches[id / 64] |= 1 << (id % 64);
            }
        }
        *budget += bytes;
        Some(Self {
            index,
            len: dictionary.len(),
            matches,
            any_match,
            complete,
        })
    }

    fn contains(&self, id: u64) -> Option<bool> {
        let id = usize::try_from(id).ok()?;
        (id < self.len).then(|| self.matches[id / 64] & (1 << (id % 64)) != 0)
    }
}

fn single_variable_matches(
    parts: &[Part],
    mut consumed: usize,
    variable: &[u8],
    literal: &[u8],
    anchor: Anchor,
    ignore_case: bool,
) -> bool {
    let mut seen_variable = false;
    for part in parts {
        let value = match part {
            Part::Text(bytes) => bytes.as_slice(),
            Part::Variable { .. } if seen_variable => return true,
            Part::Variable { .. } => { seen_variable = true; variable },
        };
        if !consume(value, literal, &mut consumed, anchor, ignore_case) {
            return false;
        }
        if consumed == literal.len() && anchor != Anchor::Exact {
            return true;
        }
    }
    consumed == literal.len()
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
            cache: None,
            cache_key: (pattern.to_owned(), ignore_case, 0),
            dictionary_plan_bytes: 0,
        })
    }

    pub(super) fn with_cache(mut self, cache: &SharedPatternCache) -> Self {
        self.cache = Some(Arc::clone(cache));
        self
    }

    fn prepare(
        &mut self,
        bytes: &[u8],
        dictionary: &VariableDictionary,
    ) -> Option<Arc<PreparedTemplate>> {
        // Most logtypes are rejected by a short boundary comparison. Do not allocate cache keys
        // or hold a lock for these, or for constant true templates.
        if let Some(value) = boundary_match(bytes, &self.literal, self.anchor, self.ignore_case) {
            return Some(Arc::new(PreparedTemplate {
                template: Template::Constant(value),
                dictionary: None,
            }));
        }
        self.cache_key.2 = dictionary.len();
        if let Some(hit) = self.cache.as_ref().and_then(|cache| {
            cache.lock().ok().and_then(|cache| {
                cache
                    .entries
                    .get(&self.cache_key)
                    .and_then(|entries| entries.get(bytes))
                    .cloned()
            })
        }) {
            return Some(hit);
        }
        let template = compile_template(bytes, &self.literal, self.anchor, self.ignore_case)?;
        // An empty dictionary can mean it has not been loaded yet. Never use it as a rejection.
        let plan = if dictionary.is_empty() {
            None
        } else {
            DictionaryPlan::build(
                &template,
                &self.literal,
                self.anchor,
                self.ignore_case,
                dictionary,
                &mut self.dictionary_plan_bytes,
            )
        };
        let cost = bytes.len()
            + self.cache_key.0.len()
            + 256
            + match &template {
                Template::Check { parts, .. } => {
                    parts.capacity() * size_of::<Part>()
                        + parts
                            .iter()
                            .map(|p| match p {
                                Part::Text(t) => t.capacity(),
                                _ => 0,
                            })
                            .sum::<usize>()
                }
                _ => 0,
            }
            + plan.as_ref().map_or(0, |p| p.matches.len() * 8);
        let prepared = Arc::new(PreparedTemplate {
            template,
            dictionary: plan,
        });
        if let Some(cache) = &self.cache {
            if let Ok(mut cache) = cache.lock() {
                if cost <= PLAN_BUDGET.saturating_sub(cache.bytes) {
                    cache
                        .entries
                        .entry(self.cache_key.clone())
                        .or_default()
                        .insert(bytes.to_vec(), Arc::clone(&prepared));
                    cache.bytes += cost;
                }
            }
        }
        Some(prepared)
    }

    /// False proves no row using this template can match. A boundary-variable
    /// mask is a necessary condition even when other variables remain unknown.
    pub(super) fn template_may_match_with_variables(
        &mut self,
        bytes: &[u8],
        variables: &VariableDictionary,
    ) -> bool {
        if variables.is_empty() {
            return self.template_may_match(bytes);
        }
        if let Some(value) = boundary_match(bytes, &self.literal, self.anchor, self.ignore_case) {
            return value;
        }
        self.prepare(bytes, variables).is_none_or(|p| {
            !matches!(p.template, Template::Constant(false))
                && p.dictionary.as_ref().is_none_or(|d| d.any_match)
        })
    }

    /// Empty/unloaded dictionaries retain the ordinary reader and its validation errors.
    pub(super) fn dictionary_may_match(
        &mut self,
        dictionary: &LogTypeDictionary,
        variables: &VariableDictionary,
    ) -> bool {
        dictionary.is_empty()
            || dictionary.entries().any(|entry| {
                self.template_may_match_with_variables(entry.escaped_value(), variables)
            })
    }

    /// False proves this escaped template cannot satisfy the anchored pattern, for any variables.
    pub(super) fn template_may_match(&self, template: &[u8]) -> bool {
        !matches!(
            compile_template(template, &self.literal, self.anchor, self.ignore_case),
            Some(Template::Constant(false))
        )
    }

    pub(super) fn template_matches_all(&self, template: &[u8]) -> bool {
        boundary_match(template, &self.literal, self.anchor, self.ignore_case) == Some(true)
    }

    /// None means the original evaluator must decide (unsupported variable representation).
    pub(super) fn matches(
        &mut self,
        column: ClpStringColumn<'_, '_>,
        descriptor: crate::archive::ClpDescriptor,
        next_descriptor: Option<crate::archive::ClpDescriptor>,
        dictionary: &LogTypeDictionary,
    ) -> Option<bool> {
        let id = descriptor.logtype_id() as usize;
        if self.templates.get(id)?.is_none() {
            let plan = self.prepare(
                dictionary.entry(id as u64)?.escaped_value(),
                column.variable_dictionary(),
            )?;
            self.templates[id] = Some(plan);
        }
        let prepared = self.templates[id].as_ref()?;
        if let Some(plan) = &prepared.dictionary {
            let offset = usize::try_from(descriptor.encoded_variable_offset()).ok()?;
            let end = match next_descriptor {
                Some(d) => usize::try_from(d.encoded_variable_offset()).ok()?,
                None => column.encoded_variables().len(),
            };
            let at = offset.checked_add(plan.index)?;
            if at >= end {
                return None;
            }
            let value = column.encoded_variables().get(at)?;
            let eligible = plan.contains(value as u64)?;
            if !eligible || plan.complete { return Some(eligible); }
        }
        match &prepared.template {
            Template::Constant(value) => Some(*value),
            Template::Check { parts, consumed } => {
                let record = column.record_from_descriptors(descriptor, next_descriptor)?;
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

    /// Filter a constant-template, one-variable column directly over its packed
    /// IDs. Dictionary/run codecs test distinct IDs/runs instead of each record.
    pub(super) fn match_single_variable_column(
        &mut self, column: ClpStringColumn<'_, '_>, dictionary: &LogTypeDictionary,
        skip: usize, bitmap: &mut [u8], truth: u8,
    ) -> bool {
        let Some(id) = column.single_variable_logtype() else { return false; };
        let Some(entry) = dictionary.entry(u64::from(id)) else { return false; };
        let Some(prepared) = self.prepare(entry.escaped_value(), column.variable_dictionary()) else { return false; };
        if let Template::Constant(value) = prepared.template {
            if value { bitmap.fill(truth); }
            return true;
        }
        let Some(plan) = &prepared.dictionary else { return false; };
        if !plan.complete || plan.index != 0 { return false; }
        let values = column.encoded_variables();
        // Keep malformed-ID errors on the established evaluator path, even
        // when unrelated unloaded dictionaries disabled full column validation.
        if values.data.max_u64().is_none_or(|id| id >= plan.len as u64) { return false; }
        values.or_encoded_matches(skip, bitmap, truth, |id| plan.contains(id as u64) == Some(true))
    }
}

/// Decide from the anchored literal boundary without allocating a parsed template. Stop at
/// the first real placeholder; escaped placeholders and backslashes remain literal bytes.
fn boundary_match(bytes: &[u8], literal: &[u8], anchor: Anchor, ignore_case: bool) -> Option<bool> {
    // Most hinted log templates have long, ordinary literal boundaries. Compare
    // those as byte spans before walking escape runs and placeholders.
    let length = bytes.len().min(literal.len());
    let (boundary, expected, escaped_start) = if anchor == Anchor::Suffix {
        let start = bytes.len() - length;
        (&bytes[start..], &literal[literal.len() - length..],
            start != 0 && bytes[start - 1] == b'\\')
    } else {
        (&bytes[..length], &literal[..length], false)
    };
    if !escaped_start && !boundary.iter().any(|b| matches!(b, b'\\' | 0x11 | 0x12 | 0x13)) {
        let equal = if ignore_case { boundary.eq_ignore_ascii_case(expected) } else { boundary == expected };
        if !equal || bytes.len() < literal.len() {
            return Some(false);
        }
        if anchor != Anchor::Exact || bytes.len() == literal.len() {
            return Some(true);
        }
    }
    let mut start = 0;
    let mut end = bytes.len();
    let mut consumed = 0;
    let mut pending_slashes = 0;
    while start < end || pending_slashes != 0 {
        if consumed == literal.len() && anchor != Anchor::Exact {
            return Some(true);
        }
        let (byte, escaped) = if anchor == Anchor::Suffix {
            if pending_slashes != 0 {
                pending_slashes -= 1;
                (b'\\', true)
            } else {
                end -= 1;
                let byte = bytes[end];
                let slashes = bytes[..end]
                    .iter()
                    .rev()
                    .take_while(|&&b| b == b'\\')
                    .count();
                end -= slashes;
                // Consume an entire escape run once; revisiting it for every decoded slash
                // would make suffix checks quadratic on long backslash sequences.
                pending_slashes = slashes / 2;
                (byte, slashes % 2 != 0)
            }
        } else {
            let byte = bytes[start];
            start += 1;
            if byte == b'\\' {
                let byte = *bytes.get(start)?;
                start += 1;
                (byte, true)
            } else {
                (byte, false)
            }
        };
        if !escaped && matches!(byte, 0x11 | 0x12 | 0x13) {
            return None;
        }
        let expected = if anchor == Anchor::Suffix {
            literal.get(literal.len().checked_sub(consumed + 1)?)
        } else {
            literal.get(consumed)
        };
        let Some(&expected) = expected else {
            return Some(false);
        };
        if byte != expected && !(ignore_case && byte.eq_ignore_ascii_case(&expected)) {
            return Some(false);
        }
        consumed += 1;
    }
    Some(consumed == literal.len())
}

fn compile_template(
    bytes: &[u8],
    literal: &[u8],
    anchor: Anchor,
    ignore_case: bool,
) -> Option<Template> {
    if let Some(matched) = boundary_match(bytes, literal, anchor, ignore_case) {
        return Some(Template::Constant(matched));
    }
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
    // A signed integer is never empty, ends in a decimal digit, and starts with a digit
    // or '-'. Reject impossible boundaries once rather than formatting every row.
    if matches!(parts.first(), Some(Part::Variable { kind: 0x11, .. })) {
        let next = if anchor == Anchor::Suffix {
            literal.get(literal.len() - consumed - 1)
        } else {
            literal.get(consumed)
        };
        if next.is_none_or(|b| !b.is_ascii_digit() && !(anchor != Anchor::Suffix && *b == b'-')) {
            return Some(Template::Constant(false));
        }
    }
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
    fn dictionary_plans_match_reference_and_obey_memory_budget() {
        use crate::archive::{ArchiveCatalogLimits, SingleFileArchiveReader};
        use crate::writer::{FieldRef, OpenArchive, RecordRef, ValueRef, WriterOptions};
        use std::io::Cursor;
        let mut archive = OpenArchive::new(Cursor::new(Vec::new()), WriterOptions::default());
        for i in 0..140 {
            let message = format!("sendHeartbeat took {i}ms\n");
            archive
                .append_record(RecordRef::new(&[FieldRef::new(
                    b"msg",
                    ValueRef::String(message.as_bytes()),
                )]))
                .unwrap();
        }
        let bytes = archive.finish().unwrap().into_inner().into_inner();
        let mut reader = SingleFileArchiveReader::open(Cursor::new(bytes)).unwrap();
        let catalog = reader
            .read_catalog(ArchiveCatalogLimits::default())
            .unwrap();
        let dictionary = catalog.variable_dictionary();
        assert!(dictionary.len() > 128);
        let template = b"sendHeartbeat took \x12\n";
        for anchor in [Anchor::Prefix, Anchor::Suffix, Anchor::Exact] {
            for needle in [
                b"2ms\n".as_slice(),
                b"sendHeartbeat took 2ms\n",
                b"sendHeartbeat took 1",
                b"MS\n",
                b"absent",
            ] {
                for ignore_case in [false, true] {
                    let compiled = compile_template(template, needle, anchor, ignore_case).unwrap();
                    let mut budget = 0;
                    let Some(plan) = DictionaryPlan::build(
                        &compiled,
                        needle,
                        anchor,
                        ignore_case,
                        dictionary,
                        &mut budget,
                    ) else {
                        continue;
                    };
                    for entry in dictionary.entries() {
                        let mut message = b"sendHeartbeat took ".to_vec();
                        message.extend_from_slice(entry.value());
                        message.push(b'\n');
                        let escaped = String::from_utf8(needle.to_vec()).unwrap();
                        let pattern = match anchor {
                            Anchor::Prefix => format!("{escaped}*"),
                            Anchor::Suffix => format!("*{escaped}"),
                            Anchor::Exact => escaped,
                        };
                        assert_eq!(
                            plan.contains(entry.id()),
                            Some(wildcard_match(&message, &pattern, ignore_case))
                        );
                    }
                    assert_eq!(plan.contains(dictionary.len() as u64), None);
                    assert_eq!(plan.contains(u64::MAX), None);
                    assert!(budget > 0);
                    budget = 8 << 20;
                    assert!(
                        DictionaryPlan::build(
                            &compiled,
                            needle,
                            anchor,
                            ignore_case,
                            dictionary,
                            &mut budget
                        )
                        .is_none()
                    );
                }
            }
        }
    }

    #[test]
    fn cached_dictionary_proofs_are_exact_and_bounded() {
        use crate::archive::{ArchiveCatalogLimits, SingleFileArchiveReader};
        use crate::writer::{FieldRef, OpenArchive, RecordRef, ValueRef, WriterOptions};
        use std::io::Cursor;
        let mut archive = OpenArchive::new(Cursor::new(Vec::new()), WriterOptions::default());
        for message in [b"took 1ms\n".as_slice(), b"took 2ms\n"] {
            archive
                .append_record(RecordRef::new(&[FieldRef::new(
                    b"msg",
                    ValueRef::String(message),
                )]))
                .unwrap();
        }
        let bytes = archive.finish().unwrap().into_inner().into_inner();
        let mut reader = SingleFileArchiveReader::open(Cursor::new(bytes)).unwrap();
        let catalog = reader
            .read_catalog(ArchiveCatalogLimits::default())
            .unwrap();
        let dictionary = catalog.variable_dictionary();
        let cache = SharedPatternCache::default();
        let template = b"took \x12\n";
        let mut unloaded = ClpPattern::new("*3ms\n", false, 0)
            .unwrap()
            .with_cache(&cache);
        assert!(unloaded.template_may_match_with_variables(template, &VariableDictionary::empty()));
        assert!(!unloaded.template_may_match_with_variables(template, dictionary));
        for (pattern, ignore_case, expected) in [
            ("*3ms\n", false, false),
            ("*2ms\n", false, true),
            ("*2MS\n", false, false),
            ("*2MS\n", true, true),
        ] {
            let mut first = ClpPattern::new(pattern, ignore_case, 0)
                .unwrap()
                .with_cache(&cache);
            assert_eq!(
                first.template_may_match_with_variables(template, dictionary),
                expected
            );
            let cached_bytes = cache.lock().unwrap().bytes;
            let mut second = ClpPattern::new(pattern, ignore_case, 0)
                .unwrap()
                .with_cache(&cache);
            assert_eq!(
                second.template_may_match_with_variables(template, dictionary),
                expected
            );
            assert_eq!(cached_bytes, cache.lock().unwrap().bytes);
            assert_eq!(
                0, second.dictionary_plan_bytes,
                "reuse rather than rescan dictionary"
            );
        }
        // Floats remain conservative. Multiple variables can now be rejected
        // when their boundary variable has no eligible dictionary value.
        for (template, expected) in [(b"took \x13\n".as_slice(), true), (b"took \x12\x12\n", false)] {
            assert_eq!(
                ClpPattern::new("*3ms\n", false, 0)
                    .unwrap()
                    .with_cache(&cache)
                    .template_may_match_with_variables(template, dictionary), expected
            );
        }
        let mut locked = cache.lock().unwrap();
        locked.entries.clear();
        locked.bytes = PLAN_BUDGET;
        drop(locked);
        let mut plan = ClpPattern::new("*3ms\n", false, 0)
            .unwrap()
            .with_cache(&cache);
        assert!(!plan.template_may_match_with_variables(template, dictionary));
        assert!(cache.lock().unwrap().entries.is_empty());
    }

    #[test]
    fn multi_variable_masks_are_necessary_not_sufficient() {
        use crate::archive::{ArchiveCatalogLimits, SingleFileArchiveReader};
        use crate::writer::{FieldRef, OpenArchive, RecordRef, ValueRef, WriterOptions};
        use std::io::Cursor;
        let mut archive = OpenArchive::new(Cursor::new(Vec::new()), WriterOptions::default());
        for value in [b"ab".as_slice(), b"", b"bar", b"foo", b"1ms", b"2ms", b"\xc3\xa9"] {
            archive.append_record(RecordRef::new(&[FieldRef::new(b"tag", ValueRef::String(value))])).unwrap();
        }
        let mut reader = SingleFileArchiveReader::open(Cursor::new(archive.finish().unwrap().into_inner().into_inner())).unwrap();
        let catalog = reader.read_catalog(ArchiveCatalogLimits::default()).unwrap();
        let dictionary = catalog.variable_dictionary();
        for anchor in [Anchor::Prefix, Anchor::Suffix, Anchor::Exact] {
            for needle in [b"ab|bar".as_slice(), b"foo|bar", b"bar", b"|", b"ms", b"\xc3\xa9"] {
                let template = compile_template(b"\x12|\x12", needle, anchor, false).unwrap();
                let plan = DictionaryPlan::build(&template, needle, anchor, false, dictionary, &mut 0).unwrap();
                assert!(!plan.complete);
                for left in dictionary.entries() {
                    for right in dictionary.entries() {
                        let mut message = left.value().to_vec(); message.push(b'|'); message.extend_from_slice(right.value());
                        let matches = match anchor {
                            Anchor::Prefix => message.starts_with(needle),
                            Anchor::Suffix => message.ends_with(needle),
                            Anchor::Exact => message == needle,
                        };
                        let boundary = if anchor == Anchor::Suffix { right.id() } else { left.id() };
                        assert!(!matches || plan.contains(boundary) == Some(true), "false negative: {message:?}, {needle:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn boundary_proofs_preserve_escaped_bytes() {
        let long = vec![b'\\'; 2048];
        assert_eq!(
            boundary_match(&long, &vec![b'\\'; 1024], Anchor::Suffix, false),
            Some(true)
        );
        // Include runs of backslashes, literal placeholders, UTF-8, empty needles, and every
        // byte boundary. A fast proof must agree with the independently decoded literal text.
        for text in [
            b"".as_slice(),
            b"a\\\\b\\",
            b"\x11\x12\x13",
            "É😀".as_bytes(),
        ] {
            let encoded: Vec<u8> = text
                .iter()
                .flat_map(|&b| {
                    if matches!(b, b'\\' | 0x11 | 0x12 | 0x13) {
                        vec![b'\\', b]
                    } else {
                        vec![b]
                    }
                })
                .collect();
            for start in 0..=text.len() {
                for end in start..=text.len() {
                    let needle = &text[start..end];
                    for (anchor, expected) in [
                        (Anchor::Prefix, text.starts_with(needle)),
                        (Anchor::Suffix, text.ends_with(needle)),
                        (Anchor::Exact, text == needle),
                    ] {
                        assert_eq!(
                            boundary_match(&encoded, needle, anchor, false),
                            Some(expected),
                            "{text:?} {needle:?} {anchor:?}"
                        );
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
