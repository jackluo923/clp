//! Bounded, allocation-reusing evaluation of reconstructed unstructured JSON arrays.

use std::error::Error;
use std::fmt;
use std::fmt::Display;
use std::fmt::Formatter;
use std::str;

use super::PathComponent;
use super::wildcard::wildcard_match;

/// JSON corruption encountered while searching a reconstructed unstructured array.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ArraySearchError {
    /// The reconstructed bytes are not UTF-8, as required by the C++ JSON parser.
    InvalidUtf8 {
        /// Bytes known to be valid before the malformed sequence.
        valid_up_to: usize,
        /// Malformed sequence length, or `None` for a truncated sequence.
        error_len: Option<usize>,
    },
    /// The reconstructed value is not exactly one valid JSON array.
    Syntax {
        /// Zero-based byte offset where parsing failed.
        offset: usize,
        /// Stable syntax failure class.
        kind: ArraySyntaxErrorKind,
    },
}

impl Display for ArraySearchError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidUtf8 {
                valid_up_to,
                error_len,
            } => write!(
                formatter,
                "reconstructed array is not UTF-8 at byte {valid_up_to} (invalid length \
                 {error_len:?})"
            ),
            Self::Syntax { offset, kind } => write!(formatter, "{kind} at byte {offset}"),
        }
    }
}

impl Error for ArraySearchError {}

/// Syntax failure class for a reconstructed unstructured array.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ArraySyntaxErrorKind {
    /// The reconstructed value does not begin with an array.
    ExpectedArray,
    /// A JSON value was required.
    ExpectedValue,
    /// An object key was required.
    ExpectedObjectKey,
    /// A colon was required after an object key.
    ExpectedColon,
    /// A comma or the current container's closing delimiter was required.
    ExpectedCommaOrEnd,
    /// Input ended before the current token or container was complete.
    UnexpectedEnd,
    /// A `true`, `false`, or `null` token was malformed.
    InvalidLiteral,
    /// A JSON number was malformed.
    InvalidNumber,
    /// A syntactically valid number is outside the finite binary64/unsigned-64 domain accepted by
    /// the current C++ array scanner.
    NumberOutOfRange,
    /// A JSON string contained an unknown escape.
    InvalidStringEscape,
    /// A `\u` escape was malformed or contained an unpaired surrogate.
    InvalidUnicodeEscape,
    /// A JSON string contained an unescaped control byte.
    UnescapedControl,
    /// Bytes followed the root array's closing bracket.
    TrailingCharacters,
}

impl Display for ArraySyntaxErrorKind {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::ExpectedArray => "expected a JSON array",
            Self::ExpectedValue => "expected a JSON value",
            Self::ExpectedObjectKey => "expected a JSON object key",
            Self::ExpectedColon => "expected ':' after a JSON object key",
            Self::ExpectedCommaOrEnd => "expected ',' or the container's closing delimiter",
            Self::UnexpectedEnd => "unexpected end of JSON array",
            Self::InvalidLiteral => "invalid JSON literal",
            Self::InvalidNumber => "invalid JSON number",
            Self::NumberOutOfRange => "JSON number is outside the searchable finite domain",
            Self::InvalidStringEscape => "invalid JSON string escape",
            Self::InvalidUnicodeEscape => "invalid JSON Unicode escape",
            Self::UnescapedControl => "unescaped JSON string control byte",
            Self::TrailingCharacters => "characters follow the JSON array",
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ArrayResource {
    States,
    NestingDepth,
    StringBytes,
}

#[derive(Debug)]
pub(super) enum ArrayFailure {
    Corrupt(ArraySearchError),
    Limit {
        resource: ArrayResource,
        actual: usize,
        limit: usize,
    },
    Allocation {
        resource: ArrayResource,
        requested: usize,
    },
    SizeOverflow,
}

#[derive(Clone, Copy, Debug)]
pub(super) enum ArrayNumber {
    Integer(i64),
    Float(f64),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ArrayComparison {
    Equal,
    NonEqual,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct ArrayPredicate<'a> {
    string_pattern: Option<&'a str>,
    number: Option<ArrayNumber>,
    boolean: Option<bool>,
    null: bool,
    comparison: ArrayComparison,
    ignore_case: bool,
}

impl<'a> ArrayPredicate<'a> {
    pub(super) const fn new(
        string_pattern: Option<&'a str>,
        number: Option<ArrayNumber>,
        boolean: Option<bool>,
        null: bool,
        comparison: ArrayComparison,
        ignore_case: bool,
    ) -> Self {
        Self {
            string_pattern,
            number,
            boolean,
            null,
            comparison,
            ignore_case,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ArrayOutcome {
    matched: bool,
    path_exists: bool,
}

impl ArrayOutcome {
    pub(super) const fn matched(self) -> bool {
        self.matched
    }

    pub(super) const fn path_exists(self) -> bool {
        self.path_exists
    }
}

#[derive(Debug, Default)]
pub(super) struct ArrayScratch {
    stack: Vec<Frame>,
    decoded_string: Vec<u8>,
}

pub(super) fn evaluate(
    source: &[u8],
    components: &[PathComponent],
    recursive: bool,
    predicate: ArrayPredicate<'_>,
    limits: ArrayLimits,
    scratch: &mut ArrayScratch,
) -> Result<ArrayOutcome, ArrayFailure> {
    if let Err(error) = str::from_utf8(source) {
        return Err(ArrayFailure::Corrupt(ArraySearchError::InvalidUtf8 {
            valid_up_to: error.valid_up_to(),
            error_len: error.error_len(),
        }));
    }
    scratch.stack.clear();
    scratch.decoded_string.clear();
    let result = Parser {
        source,
        components,
        predicate,
        limits,
        stack: &mut scratch.stack,
        decoded_string: &mut scratch.decoded_string,
        offset: 0,
        states: 0,
        matched: false,
        path_exists: components.is_empty(),
    }
    .parse(if recursive {
        MatchMode::Recursive
    } else {
        MatchMode::Precise(0)
    });
    scratch.stack.clear();
    result
}

#[derive(Clone, Copy, Debug)]
pub(super) struct ArrayLimits {
    pub(super) states: usize,
    pub(super) nesting_depth: usize,
    pub(super) string_bytes: usize,
}

#[derive(Clone, Copy, Debug)]
enum MatchMode {
    Disabled,
    Precise(usize),
    Recursive,
}

#[derive(Clone, Copy, Debug)]
enum Frame {
    Array {
        state: ArrayState,
        mode: MatchMode,
    },
    Object {
        state: ObjectState,
        mode: MatchMode,
        value_mode: MatchMode,
        matched_key: bool,
    },
}

#[derive(Clone, Copy, Debug)]
enum ArrayState {
    FirstValueOrEnd,
    ValueAfterComma,
    CommaOrEnd,
}

#[derive(Clone, Copy, Debug)]
enum ObjectState {
    FirstKeyOrEnd,
    KeyAfterComma,
    Colon,
    Value,
    CommaOrEnd,
}

#[derive(Clone, Copy, Debug)]
enum JsonNumber {
    Signed(i64),
    Unsigned(u64),
    Float(f64),
}

struct Parser<'input, 'query, 'scratch> {
    source: &'input [u8],
    components: &'query [PathComponent],
    predicate: ArrayPredicate<'query>,
    limits: ArrayLimits,
    stack: &'scratch mut Vec<Frame>,
    decoded_string: &'scratch mut Vec<u8>,
    offset: usize,
    states: usize,
    matched: bool,
    path_exists: bool,
}

impl Parser<'_, '_, '_> {
    fn parse(mut self, root_mode: MatchMode) -> Result<ArrayOutcome, ArrayFailure> {
        if self.peek() != Some(b'[') {
            return Err(self.corrupt(ArraySyntaxErrorKind::ExpectedArray));
        }
        self.visit_state()?;
        self.open_array(root_mode)?;
        loop {
            let Some(frame) = self.stack.last().copied() else {
                return self.finish();
            };
            match frame {
                Frame::Array {
                    state: ArrayState::FirstValueOrEnd,
                    mode,
                } => {
                    self.skip_whitespace();
                    if self.consume_if(b']') {
                        self.stack.pop();
                    } else {
                        self.replace_array_state(ArrayState::CommaOrEnd)?;
                        self.parse_value(mode)?;
                    }
                }
                Frame::Array {
                    state: ArrayState::ValueAfterComma,
                    mode,
                } => {
                    self.skip_whitespace();
                    self.replace_array_state(ArrayState::CommaOrEnd)?;
                    self.parse_value(mode)?;
                }
                Frame::Array {
                    state: ArrayState::CommaOrEnd,
                    ..
                } => {
                    self.skip_whitespace();
                    if self.consume_if(b',') {
                        self.replace_array_state(ArrayState::ValueAfterComma)?;
                    } else if self.consume_if(b']') {
                        self.stack.pop();
                    } else {
                        return Err(self.corrupt(ArraySyntaxErrorKind::ExpectedCommaOrEnd));
                    }
                }
                Frame::Object {
                    state: ObjectState::FirstKeyOrEnd,
                    ..
                } => {
                    self.skip_whitespace();
                    if self.consume_if(b'}') {
                        self.stack.pop();
                    } else {
                        self.parse_object_key()?;
                        self.replace_object_state(ObjectState::Colon)?;
                    }
                }
                Frame::Object {
                    state: ObjectState::KeyAfterComma,
                    ..
                } => {
                    self.skip_whitespace();
                    self.parse_object_key()?;
                    self.replace_object_state(ObjectState::Colon)?;
                }
                Frame::Object {
                    state: ObjectState::Colon,
                    ..
                } => {
                    self.skip_whitespace();
                    if !self.consume_if(b':') {
                        return Err(self.corrupt(ArraySyntaxErrorKind::ExpectedColon));
                    }
                    self.replace_object_state(ObjectState::Value)?;
                }
                Frame::Object {
                    state: ObjectState::Value,
                    value_mode,
                    ..
                } => {
                    self.replace_object_state(ObjectState::CommaOrEnd)?;
                    self.parse_value(value_mode)?;
                }
                Frame::Object {
                    state: ObjectState::CommaOrEnd,
                    ..
                } => {
                    self.skip_whitespace();
                    if self.consume_if(b',') {
                        self.replace_object_state(ObjectState::KeyAfterComma)?;
                    } else if self.consume_if(b'}') {
                        self.stack.pop();
                    } else {
                        return Err(self.corrupt(ArraySyntaxErrorKind::ExpectedCommaOrEnd));
                    }
                }
            }
        }
    }

    const fn finish(&self) -> Result<ArrayOutcome, ArrayFailure> {
        if self.offset != self.source.len() {
            return Err(self.corrupt(ArraySyntaxErrorKind::TrailingCharacters));
        }
        Ok(ArrayOutcome {
            matched: self.matched,
            path_exists: self.path_exists,
        })
    }

    fn parse_value(&mut self, mode: MatchMode) -> Result<(), ArrayFailure> {
        self.skip_whitespace();
        self.visit_state()?;
        match self.peek() {
            Some(b'{') => self.open_object(mode),
            Some(b'[') => self.open_array(mode),
            Some(b'"') => {
                self.parse_string()?;
                if self.is_candidate(mode)
                    && self.predicate.string_pattern.is_some_and(|pattern| {
                        wildcard_match(self.decoded_string, pattern, self.predicate.ignore_case)
                    })
                {
                    self.matched = true;
                }
                Ok(())
            }
            Some(b'-' | b'0'..=b'9') => {
                let number = self.parse_number()?;
                if self.is_candidate(mode) && self.number_matches(number) {
                    self.matched = true;
                }
                Ok(())
            }
            Some(b't') => {
                self.parse_literal(b"true")?;
                if self.is_candidate(mode) && self.predicate.boolean == Some(true) {
                    self.matched = true;
                }
                Ok(())
            }
            Some(b'f') => {
                self.parse_literal(b"false")?;
                if self.is_candidate(mode) && self.predicate.boolean == Some(false) {
                    self.matched = true;
                }
                Ok(())
            }
            Some(b'n') => {
                self.parse_literal(b"null")?;
                // The C++ precise-array scanner omits the terminal-path check in its null arm.
                // Disabled values are still skipped because C++ never descends through a
                // nonmatching object key.
                if !matches!(mode, MatchMode::Disabled) && self.predicate.null {
                    self.matched = true;
                }
                Ok(())
            }
            Some(_) => Err(self.corrupt(ArraySyntaxErrorKind::ExpectedValue)),
            None => Err(self.corrupt(ArraySyntaxErrorKind::UnexpectedEnd)),
        }
    }

    fn parse_object_key(&mut self) -> Result<(), ArrayFailure> {
        if self.peek() != Some(b'"') {
            return Err(self.corrupt(ArraySyntaxErrorKind::ExpectedObjectKey));
        }
        self.visit_state()?;
        self.parse_string()?;
        let Some(Frame::Object {
            mode, matched_key, ..
        }) = self.stack.last().copied()
        else {
            return Err(ArrayFailure::SizeOverflow);
        };
        let mut next_mode = MatchMode::Disabled;
        let mut now_matched = matched_key;
        match mode {
            MatchMode::Disabled => {}
            MatchMode::Recursive => next_mode = MatchMode::Recursive,
            MatchMode::Precise(component) => {
                if !matched_key
                    && self.components.get(component).is_some_and(|expected| {
                        !expected.is_wildcard()
                            && expected.value().as_bytes() == self.decoded_string.as_slice()
                    })
                {
                    let next = component.checked_add(1).ok_or(ArrayFailure::SizeOverflow)?;
                    next_mode = MatchMode::Precise(next);
                    now_matched = true;
                    if next == self.components.len() {
                        self.path_exists = true;
                    }
                }
            }
        }
        let Some(Frame::Object {
            value_mode,
            matched_key,
            ..
        }) = self.stack.last_mut()
        else {
            return Err(ArrayFailure::SizeOverflow);
        };
        *value_mode = next_mode;
        *matched_key = now_matched;
        Ok(())
    }

    fn parse_string(&mut self) -> Result<(), ArrayFailure> {
        self.decoded_string.clear();
        self.offset = self
            .offset
            .checked_add(1)
            .ok_or(ArrayFailure::SizeOverflow)?;
        let mut run_start = self.offset;
        loop {
            let Some(byte) = self.peek() else {
                return Err(self.corrupt(ArraySyntaxErrorKind::UnexpectedEnd));
            };
            match byte {
                b'"' => {
                    self.append_decoded(run_start, self.offset)?;
                    self.offset += 1;
                    return Ok(());
                }
                b'\\' => {
                    self.append_decoded(run_start, self.offset)?;
                    self.parse_escape()?;
                    run_start = self.offset;
                }
                0x00..=0x1f => {
                    return Err(self.corrupt(ArraySyntaxErrorKind::UnescapedControl));
                }
                _ => self.offset += 1,
            }
        }
    }

    fn parse_escape(&mut self) -> Result<(), ArrayFailure> {
        let escape_offset = self.offset;
        self.offset = self
            .offset
            .checked_add(1)
            .ok_or(ArrayFailure::SizeOverflow)?;
        let Some(escaped) = self.peek() else {
            return Err(self.corrupt(ArraySyntaxErrorKind::UnexpectedEnd));
        };
        let decoded = match escaped {
            b'"' | b'\\' | b'/' => Some(escaped),
            b'b' => Some(0x08),
            b'f' => Some(0x0c),
            b'n' => Some(b'\n'),
            b'r' => Some(b'\r'),
            b't' => Some(b'\t'),
            b'u' => None,
            _ => {
                return Err(ArrayFailure::Corrupt(ArraySearchError::Syntax {
                    offset: escape_offset,
                    kind: ArraySyntaxErrorKind::InvalidStringEscape,
                }));
            }
        };
        self.offset += 1;
        if let Some(decoded) = decoded {
            return self.extend_decoded(&[decoded]);
        }
        let first = self.parse_hex_quad(escape_offset)?;
        let scalar = if (0xd800..=0xdbff).contains(&first) {
            let pair_end = self
                .offset
                .checked_add(2)
                .ok_or(ArrayFailure::SizeOverflow)?;
            if self.source.get(self.offset..pair_end) != Some(br"\u") {
                return Err(ArrayFailure::Corrupt(ArraySearchError::Syntax {
                    offset: escape_offset,
                    kind: ArraySyntaxErrorKind::InvalidUnicodeEscape,
                }));
            }
            self.offset = pair_end;
            let second = self.parse_hex_quad(escape_offset)?;
            if !(0xdc00..=0xdfff).contains(&second) {
                return Err(ArrayFailure::Corrupt(ArraySearchError::Syntax {
                    offset: escape_offset,
                    kind: ArraySyntaxErrorKind::InvalidUnicodeEscape,
                }));
            }
            0x1_0000 + ((u32::from(first) - 0xd800) << 10) + (u32::from(second) - 0xdc00)
        } else if (0xdc00..=0xdfff).contains(&first) {
            return Err(ArrayFailure::Corrupt(ArraySearchError::Syntax {
                offset: escape_offset,
                kind: ArraySyntaxErrorKind::InvalidUnicodeEscape,
            }));
        } else {
            u32::from(first)
        };
        let character =
            char::from_u32(scalar).ok_or(ArrayFailure::Corrupt(ArraySearchError::Syntax {
                offset: escape_offset,
                kind: ArraySyntaxErrorKind::InvalidUnicodeEscape,
            }))?;
        let mut encoded = [0_u8; 4];
        self.extend_decoded(character.encode_utf8(&mut encoded).as_bytes())
    }

    fn parse_hex_quad(&mut self, escape_offset: usize) -> Result<u16, ArrayFailure> {
        let end = self
            .offset
            .checked_add(4)
            .ok_or(ArrayFailure::SizeOverflow)?;
        let Some(digits) = self.source.get(self.offset..end) else {
            return Err(self.corrupt(ArraySyntaxErrorKind::UnexpectedEnd));
        };
        let mut value = 0_u16;
        for &digit in digits {
            let Some(nibble) = hex_nibble(digit) else {
                return Err(ArrayFailure::Corrupt(ArraySearchError::Syntax {
                    offset: escape_offset,
                    kind: ArraySyntaxErrorKind::InvalidUnicodeEscape,
                }));
            };
            value = (value << 4) | u16::from(nibble);
        }
        self.offset = end;
        Ok(value)
    }

    fn parse_number(&mut self) -> Result<JsonNumber, ArrayFailure> {
        let start = self.offset;
        self.consume_if(b'-');
        match self.peek() {
            Some(b'0') => {
                self.offset += 1;
                if self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
                    return Err(self.corrupt(ArraySyntaxErrorKind::InvalidNumber));
                }
            }
            Some(b'1'..=b'9') => {
                self.offset += 1;
                self.consume_ascii_digits();
            }
            Some(_) => return Err(self.corrupt(ArraySyntaxErrorKind::InvalidNumber)),
            None => return Err(self.corrupt(ArraySyntaxErrorKind::UnexpectedEnd)),
        }
        let mut is_float = false;
        if self.consume_if(b'.') {
            is_float = true;
            if !self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
                return Err(self.corrupt(if self.peek().is_none() {
                    ArraySyntaxErrorKind::UnexpectedEnd
                } else {
                    ArraySyntaxErrorKind::InvalidNumber
                }));
            }
            self.consume_ascii_digits();
        }
        if self.peek().is_some_and(|byte| matches!(byte, b'e' | b'E')) {
            is_float = true;
            self.offset += 1;
            if self.peek().is_some_and(|byte| matches!(byte, b'+' | b'-')) {
                self.offset += 1;
            }
            if !self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
                return Err(self.corrupt(if self.peek().is_none() {
                    ArraySyntaxErrorKind::UnexpectedEnd
                } else {
                    ArraySyntaxErrorKind::InvalidNumber
                }));
            }
            self.consume_ascii_digits();
        }
        let raw = str::from_utf8(
            self.source
                .get(start..self.offset)
                .ok_or(ArrayFailure::SizeOverflow)?,
        )
        .map_err(|_| ArrayFailure::SizeOverflow)?;
        if is_float {
            let value = raw
                .parse::<f64>()
                .ok()
                .filter(|value| value.is_finite())
                .ok_or_else(|| self.corrupt(ArraySyntaxErrorKind::NumberOutOfRange))?;
            return Ok(JsonNumber::Float(value));
        }
        if raw.starts_with('-') {
            raw.parse::<i64>()
                .map(JsonNumber::Signed)
                .map_err(|_| self.corrupt(ArraySyntaxErrorKind::NumberOutOfRange))
        } else {
            raw.parse::<u64>()
                .map(JsonNumber::Unsigned)
                .map_err(|_| self.corrupt(ArraySyntaxErrorKind::NumberOutOfRange))
        }
    }

    fn parse_literal(&mut self, literal: &[u8]) -> Result<(), ArrayFailure> {
        let end = self
            .offset
            .checked_add(literal.len())
            .ok_or(ArrayFailure::SizeOverflow)?;
        if self.source.get(self.offset..end) != Some(literal) {
            let kind = if end > self.source.len()
                && self
                    .source
                    .get(self.offset..)
                    .is_some_and(|suffix| literal.starts_with(suffix))
            {
                ArraySyntaxErrorKind::UnexpectedEnd
            } else {
                ArraySyntaxErrorKind::InvalidLiteral
            };
            return Err(self.corrupt(kind));
        }
        self.offset = end;
        Ok(())
    }

    #[allow(clippy::cast_precision_loss, clippy::float_cmp)]
    fn number_matches(&self, value: JsonNumber) -> bool {
        let Some(operand) = self.predicate.number else {
            return false;
        };
        let equal = match (value, operand) {
            (JsonNumber::Signed(value), ArrayNumber::Integer(operand)) => value == operand,
            (JsonNumber::Unsigned(value), ArrayNumber::Integer(operand)) => {
                u64::try_from(operand) == Ok(value)
            }
            (JsonNumber::Float(value), ArrayNumber::Integer(operand)) => {
                #[allow(clippy::cast_precision_loss)]
                let operand = operand as f64;
                value == operand
            }
            (JsonNumber::Signed(value), ArrayNumber::Float(operand)) => {
                integer_equals_float(value, operand)
            }
            (JsonNumber::Unsigned(value), ArrayNumber::Float(operand)) => {
                unsigned_equals_float(value, operand)
            }
            (JsonNumber::Float(value), ArrayNumber::Float(operand)) => value == operand,
        };
        match self.predicate.comparison {
            ArrayComparison::Equal => equal,
            // This intentionally pins the C++ `eval` macro used by array scanning: every
            // non-equality numeric operation currently means inequality.
            ArrayComparison::NonEqual => !equal,
        }
    }

    fn open_array(&mut self, mode: MatchMode) -> Result<(), ArrayFailure> {
        self.open(Frame::Array {
            state: ArrayState::FirstValueOrEnd,
            mode,
        })
    }

    fn open_object(&mut self, mode: MatchMode) -> Result<(), ArrayFailure> {
        self.open(Frame::Object {
            state: ObjectState::FirstKeyOrEnd,
            mode,
            value_mode: MatchMode::Disabled,
            matched_key: false,
        })
    }

    fn open(&mut self, frame: Frame) -> Result<(), ArrayFailure> {
        let depth = self
            .stack
            .len()
            .checked_add(1)
            .ok_or(ArrayFailure::SizeOverflow)?;
        if depth > self.limits.nesting_depth {
            return Err(ArrayFailure::Limit {
                resource: ArrayResource::NestingDepth,
                actual: depth,
                limit: self.limits.nesting_depth,
            });
        }
        self.stack
            .try_reserve(1)
            .map_err(|_| ArrayFailure::Allocation {
                resource: ArrayResource::NestingDepth,
                requested: 1,
            })?;
        self.offset = self
            .offset
            .checked_add(1)
            .ok_or(ArrayFailure::SizeOverflow)?;
        self.stack.push(frame);
        Ok(())
    }

    fn visit_state(&mut self) -> Result<(), ArrayFailure> {
        let actual = self
            .states
            .checked_add(1)
            .ok_or(ArrayFailure::SizeOverflow)?;
        if actual > self.limits.states {
            return Err(ArrayFailure::Limit {
                resource: ArrayResource::States,
                actual,
                limit: self.limits.states,
            });
        }
        self.states = actual;
        Ok(())
    }

    fn append_decoded(&mut self, start: usize, end: usize) -> Result<(), ArrayFailure> {
        let bytes = self
            .source
            .get(start..end)
            .ok_or(ArrayFailure::SizeOverflow)?;
        self.extend_decoded(bytes)
    }

    fn extend_decoded(&mut self, bytes: &[u8]) -> Result<(), ArrayFailure> {
        let required = self
            .decoded_string
            .len()
            .checked_add(bytes.len())
            .ok_or(ArrayFailure::SizeOverflow)?;
        if required > self.limits.string_bytes {
            return Err(ArrayFailure::Limit {
                resource: ArrayResource::StringBytes,
                actual: required,
                limit: self.limits.string_bytes,
            });
        }
        self.decoded_string
            .try_reserve(bytes.len())
            .map_err(|_| ArrayFailure::Allocation {
                resource: ArrayResource::StringBytes,
                requested: bytes.len(),
            })?;
        self.decoded_string.extend_from_slice(bytes);
        Ok(())
    }

    fn replace_array_state(&mut self, state: ArrayState) -> Result<(), ArrayFailure> {
        let Some(Frame::Array { state: current, .. }) = self.stack.last_mut() else {
            return Err(ArrayFailure::SizeOverflow);
        };
        *current = state;
        Ok(())
    }

    fn replace_object_state(&mut self, state: ObjectState) -> Result<(), ArrayFailure> {
        let Some(Frame::Object { state: current, .. }) = self.stack.last_mut() else {
            return Err(ArrayFailure::SizeOverflow);
        };
        *current = state;
        Ok(())
    }

    const fn is_candidate(&self, mode: MatchMode) -> bool {
        matches!(mode, MatchMode::Recursive)
            || matches!(mode, MatchMode::Precise(component) if component == self.components.len())
    }

    fn skip_whitespace(&mut self) {
        while self
            .peek()
            .is_some_and(|byte| matches!(byte, b' ' | b'\t' | b'\n' | b'\r'))
        {
            self.offset += 1;
        }
    }

    fn consume_ascii_digits(&mut self) {
        while self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
            self.offset += 1;
        }
    }

    fn consume_if(&mut self, expected: u8) -> bool {
        if self.peek() != Some(expected) {
            return false;
        }
        self.offset += 1;
        true
    }

    fn peek(&self) -> Option<u8> {
        self.source.get(self.offset).copied()
    }

    const fn corrupt(&self, kind: ArraySyntaxErrorKind) -> ArrayFailure {
        ArrayFailure::Corrupt(ArraySearchError::Syntax {
            offset: self.offset,
            kind,
        })
    }
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::float_cmp
)]
fn integer_equals_float(value: i64, operand: f64) -> bool {
    const LOWER: f64 = -9_223_372_036_854_775_808.0;
    const UPPER_EXCLUSIVE: f64 = 9_223_372_036_854_775_808.0;
    (LOWER..UPPER_EXCLUSIVE).contains(&operand)
        && operand.fract() == 0.0
        && value == operand as i64
        && operand == value as f64
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::float_cmp
)]
fn unsigned_equals_float(value: u64, operand: f64) -> bool {
    const UPPER_EXCLUSIVE: f64 = 18_446_744_073_709_551_616.0;
    (0.0..UPPER_EXCLUSIVE).contains(&operand)
        && operand.fract() == 0.0
        && value == operand as u64
        && operand == value as f64
}

const fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn component(value: &str) -> PathComponent {
        PathComponent {
            value: value.to_owned(),
            wildcard: false,
        }
    }

    fn predicate(pattern: Option<&str>, number: Option<ArrayNumber>) -> ArrayPredicate<'_> {
        ArrayPredicate::new(pattern, number, None, false, ArrayComparison::Equal, false)
    }

    fn defaults() -> ArrayLimits {
        ArrayLimits {
            states: 1_000,
            nesting_depth: 32,
            string_bytes: 1_000,
        }
    }

    #[test]
    fn scans_precise_paths_arrays_and_escaped_strings_without_allocating_results() {
        let source = br#"[[{"n":3}],{"n":[5,{"ignored":5}]},{"n":7},"a\u002ab"]"#;
        let mut scratch = ArrayScratch::default();
        let path = [component("n")];
        let outcome = evaluate(
            source,
            &path,
            false,
            predicate(None, Some(ArrayNumber::Integer(5))),
            defaults(),
            &mut scratch,
        )
        .expect("scan numeric path");
        assert!(outcome.matched());
        assert!(outcome.path_exists());

        let outcome = evaluate(
            source,
            &[],
            true,
            predicate(Some(r"a\*b"), None),
            defaults(),
            &mut scratch,
        )
        .expect("scan escaped string");
        assert!(outcome.matched());
    }

    #[test]
    fn rejects_corruption_and_bounds_depth_states_and_decoded_strings() {
        let mut scratch = ArrayScratch::default();
        for source in [b"[1,]".as_slice(), b"[1e999]", b"[\"\\uD800\"]"] {
            assert!(matches!(
                evaluate(
                    source,
                    &[],
                    false,
                    predicate(None, None),
                    defaults(),
                    &mut scratch,
                ),
                Err(ArrayFailure::Corrupt(_))
            ));
        }
        let limited = ArrayLimits {
            states: 2,
            nesting_depth: 1,
            string_bytes: 1,
        };
        assert!(matches!(
            evaluate(
                b"[[0]]",
                &[],
                false,
                predicate(None, None),
                limited,
                &mut scratch,
            ),
            Err(ArrayFailure::Limit {
                resource: ArrayResource::NestingDepth,
                ..
            })
        ));
        assert!(matches!(
            evaluate(
                b"[0,1]",
                &[],
                false,
                predicate(None, None),
                limited,
                &mut scratch,
            ),
            Err(ArrayFailure::Limit {
                resource: ArrayResource::States,
                ..
            })
        ));
        assert!(matches!(
            evaluate(
                b"[\"ab\"]",
                &[],
                false,
                predicate(Some("ab"), None),
                limited,
                &mut scratch,
            ),
            Err(ArrayFailure::Limit {
                resource: ArrayResource::StringBytes,
                ..
            })
        ));
    }
}
