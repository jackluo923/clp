use std::str;

use super::ndjson::JsonArrayRef;
use super::ndjson::JsonEvent;
use super::ndjson::JsonString;
use super::ndjson::JsonSyntaxError;
use super::ndjson::JsonSyntaxErrorKind;
use super::ndjson::NdjsonInvalidRecordKind;
use super::ndjson::NdjsonLimitResource;
use super::ndjson::NdjsonLimitViolation;
use super::ndjson::NdjsonLimits;
use super::ndjson::NdjsonResource;
use super::number::ValidatedJsonNumberSyntax;

const STRING_SCAN_WORD_BYTES: usize = std::mem::size_of::<u64>();
const BYTE_ONES: u64 = 0x0101_0101_0101_0101;
const BYTE_HIGH_BITS: u64 = 0x8080_8080_8080_8080;
const QUOTE_BYTES: u64 = 0x2222_2222_2222_2222;
const BACKSLASH_BYTES: u64 = 0x5c5c_5c5c_5c5c_5c5c;
const CONTROL_LIMIT_BYTES: u64 = 0x2020_2020_2020_2020;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Span {
    start: usize,
    end: usize,
}

impl Span {
    const fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }

    pub(super) fn bytes(self, source: &[u8]) -> &[u8] {
        &source[self.start..self.end]
    }

    pub(super) fn string(self, source: &str) -> &str {
        &source[self.start..self.end]
    }

    pub(super) fn inner_bytes(self, source: &[u8]) -> &[u8] {
        &source[self.start + 1..self.end - 1]
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum StoredEvent {
    ObjectStart,
    ObjectEnd,
    ArrayStart(Span),
    ArrayEnd,
    ObjectKeySource(Span),
    ObjectKeyDecoded {
        raw: Span,
        decoded: Span,
    },
    StringSource(Span),
    StringDecoded {
        raw: Span,
        decoded: Span,
    },
    Number {
        raw: Span,
        syntax: ValidatedJsonNumberSyntax,
    },
    Boolean(bool),
    Null,
}

impl StoredEvent {
    pub(super) fn resolve<'a>(self, raw_json: &'a [u8], decoded: &'a str) -> JsonEvent<'a> {
        match self {
            Self::ObjectStart => JsonEvent::ObjectStart,
            Self::ObjectEnd => JsonEvent::ObjectEnd,
            Self::ArrayStart(raw) => JsonEvent::ArrayStart(JsonArrayRef::new(raw.bytes(raw_json))),
            Self::ArrayEnd => JsonEvent::ArrayEnd,
            Self::ObjectKeySource(raw) => JsonEvent::ObjectKey(JsonString::new_bytes(
                raw.bytes(raw_json),
                raw.inner_bytes(raw_json),
            )),
            Self::ObjectKeyDecoded {
                raw,
                decoded: decoded_span,
            } => JsonEvent::ObjectKey(JsonString::new(
                raw.bytes(raw_json),
                decoded_span.string(decoded),
            )),
            Self::StringSource(raw) => JsonEvent::String(JsonString::new_bytes(
                raw.bytes(raw_json),
                raw.inner_bytes(raw_json),
            )),
            Self::StringDecoded {
                raw,
                decoded: decoded_span,
            } => JsonEvent::String(JsonString::new(
                raw.bytes(raw_json),
                decoded_span.string(decoded),
            )),
            Self::Number { raw, .. } => JsonEvent::Number(raw.bytes(raw_json)),
            Self::Boolean(value) => JsonEvent::Boolean(value),
            Self::Null => JsonEvent::Null,
        }
    }

    pub(super) const fn number_syntax(self) -> Option<ValidatedJsonNumberSyntax> {
        match self {
            Self::Number { syntax, .. } => Some(syntax),
            _ => None,
        }
    }
}

// Sparse tags keep optimized parser-state dispatch as predictable conditional branches instead of
// one high-miss indirect jump. These values are private implementation details, not wire tags.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(super) enum ObjectState {
    FirstKeyOrEnd = 0,
    KeyAfterComma = 17,
    Colon = 67,
    Value = 149,
    CommaOrEnd = 251,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(super) enum ArrayState {
    FirstValueOrEnd = 0,
    ValueAfterComma = 127,
    CommaOrEnd = 255,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct ArrayFrame {
    state: ArrayState,
    start_event_index: usize,
}

#[derive(Clone, Copy, Debug)]
pub(super) enum Frame {
    Object(ObjectState),
    Array(ArrayFrame),
}

#[derive(Debug)]
pub(super) enum ParseFailure {
    Invalid(NdjsonInvalidRecordKind),
    AllocationFailed {
        resource: NdjsonResource,
        requested_additional: usize,
    },
    SizeOverflow,
}

pub(super) fn parse_document(
    source: &[u8],
    limits: NdjsonLimits,
    decoded: &mut String,
    events: &mut Vec<StoredEvent>,
    stack: &mut Vec<Frame>,
) -> Result<(), ParseFailure> {
    Parser {
        source,
        limits,
        decoded,
        events,
        stack,
        offset: 0,
        values: 0,
    }
    .parse()
}

/// Parses one root value and returns its exclusive end without inspecting trailing bytes.
pub(super) fn parse_document_prefix(
    source: &[u8],
    limits: NdjsonLimits,
    decoded: &mut String,
    events: &mut Vec<StoredEvent>,
    stack: &mut Vec<Frame>,
) -> Result<usize, ParseFailure> {
    Parser {
        source,
        limits,
        decoded,
        events,
        stack,
        offset: 0,
        values: 0,
    }
    .parse_prefix()
}

struct Parser<'input, 'output> {
    source: &'input [u8],
    limits: NdjsonLimits,
    decoded: &'output mut String,
    events: &'output mut Vec<StoredEvent>,
    stack: &'output mut Vec<Frame>,
    offset: usize,
    values: u64,
}

impl Parser<'_, '_> {
    fn parse(&mut self) -> Result<(), ParseFailure> {
        self.parse_prefix()?;
        self.skip_whitespace();
        if self.offset == self.source.len() {
            Ok(())
        } else {
            Err(Self::syntax(
                JsonSyntaxErrorKind::TrailingCharacters,
                self.offset,
            ))
        }
    }

    fn parse_prefix(&mut self) -> Result<usize, ParseFailure> {
        let mut root_started = false;
        loop {
            let Some(frame) = self.stack.last().copied() else {
                if root_started {
                    return Ok(self.offset);
                }
                self.parse_value()?;
                root_started = true;
                continue;
            };

            match frame {
                Frame::Object(ObjectState::FirstKeyOrEnd) => {
                    self.skip_whitespace();
                    if self.consume_if(b'}') {
                        self.stack.pop();
                        self.push_event(StoredEvent::ObjectEnd)?;
                    } else {
                        self.parse_object_key()?;
                        self.replace_top(Frame::Object(ObjectState::Colon));
                    }
                }
                Frame::Object(ObjectState::KeyAfterComma) => {
                    self.skip_whitespace();
                    self.parse_object_key()?;
                    self.replace_top(Frame::Object(ObjectState::Colon));
                }
                Frame::Object(ObjectState::Colon) => {
                    self.skip_whitespace();
                    if !self.consume_if(b':') {
                        return Err(Self::syntax(
                            JsonSyntaxErrorKind::ExpectedColon,
                            self.offset,
                        ));
                    }
                    self.replace_top(Frame::Object(ObjectState::Value));
                }
                Frame::Object(ObjectState::Value) => {
                    self.replace_top(Frame::Object(ObjectState::CommaOrEnd));
                    self.parse_value()?;
                }
                Frame::Object(ObjectState::CommaOrEnd) => {
                    self.skip_whitespace();
                    if self.consume_if(b',') {
                        self.replace_top(Frame::Object(ObjectState::KeyAfterComma));
                    } else if self.consume_if(b'}') {
                        self.stack.pop();
                        self.push_event(StoredEvent::ObjectEnd)?;
                    } else {
                        return Err(Self::syntax(
                            JsonSyntaxErrorKind::ExpectedCommaOrEnd,
                            self.offset,
                        ));
                    }
                }
                Frame::Array(frame) if ArrayState::FirstValueOrEnd == frame.state => {
                    self.skip_whitespace();
                    if self.consume_if(b']') {
                        self.close_array(frame.start_event_index)?;
                    } else {
                        self.replace_top(Frame::Array(ArrayFrame {
                            state: ArrayState::CommaOrEnd,
                            ..frame
                        }));
                        self.parse_value()?;
                    }
                }
                Frame::Array(frame) if ArrayState::ValueAfterComma == frame.state => {
                    self.skip_whitespace();
                    self.replace_top(Frame::Array(ArrayFrame {
                        state: ArrayState::CommaOrEnd,
                        ..frame
                    }));
                    self.parse_value()?;
                }
                Frame::Array(frame) => {
                    self.skip_whitespace();
                    if self.consume_if(b',') {
                        self.replace_top(Frame::Array(ArrayFrame {
                            state: ArrayState::ValueAfterComma,
                            ..frame
                        }));
                    } else if self.consume_if(b']') {
                        self.close_array(frame.start_event_index)?;
                    } else {
                        return Err(Self::syntax(
                            JsonSyntaxErrorKind::ExpectedCommaOrEnd,
                            self.offset,
                        ));
                    }
                }
            }
        }
    }

    fn parse_object_key(&mut self) -> Result<(), ParseFailure> {
        if self.peek() != Some(b'"') {
            return Err(Self::syntax(
                JsonSyntaxErrorKind::ExpectedObjectKey,
                self.offset,
            ));
        }
        let (raw, decoded) = self.parse_string()?;
        let event = decoded.map_or(StoredEvent::ObjectKeySource(raw), |decoded| {
            StoredEvent::ObjectKeyDecoded { raw, decoded }
        });
        self.push_event(event)
    }

    fn parse_value(&mut self) -> Result<(), ParseFailure> {
        self.skip_whitespace();
        let Some(first) = self.peek() else {
            return Err(Self::syntax(
                JsonSyntaxErrorKind::ExpectedValue,
                self.offset,
            ));
        };
        match first {
            b'{' => {
                self.add_value()?;
                self.open_container(
                    StoredEvent::ObjectStart,
                    Frame::Object(ObjectState::FirstKeyOrEnd),
                )
            }
            b'[' => {
                self.add_value()?;
                let start = self.offset;
                let start_event_index = self.events.len();
                self.open_container(
                    StoredEvent::ArrayStart(Span::new(start, start)),
                    Frame::Array(ArrayFrame {
                        state: ArrayState::FirstValueOrEnd,
                        start_event_index,
                    }),
                )
            }
            b'"' => {
                self.add_value()?;
                let (raw, decoded) = self.parse_string()?;
                let event = decoded.map_or(StoredEvent::StringSource(raw), |decoded| {
                    StoredEvent::StringDecoded { raw, decoded }
                });
                self.push_event(event)
            }
            b'-' | b'0'..=b'9' => {
                self.add_value()?;
                let (raw, syntax) = self.parse_number()?;
                self.push_event(StoredEvent::Number { raw, syntax })
            }
            b't' => {
                self.add_value()?;
                self.parse_literal(b"true")?;
                self.push_event(StoredEvent::Boolean(true))
            }
            b'f' => {
                self.add_value()?;
                self.parse_literal(b"false")?;
                self.push_event(StoredEvent::Boolean(false))
            }
            b'n' => {
                self.add_value()?;
                self.parse_literal(b"null")?;
                self.push_event(StoredEvent::Null)
            }
            _ => Err(Self::syntax(
                JsonSyntaxErrorKind::ExpectedValue,
                self.offset,
            )),
        }
    }

    fn open_container(&mut self, event: StoredEvent, frame: Frame) -> Result<(), ParseFailure> {
        let current_depth =
            u64::try_from(self.stack.len()).map_err(|_| ParseFailure::SizeOverflow)?;
        let depth = current_depth
            .checked_add(1)
            .ok_or(ParseFailure::SizeOverflow)?;
        if depth > self.limits.max_nesting_depth() {
            return Err(Self::limit(
                NdjsonLimitResource::NestingDepth,
                depth,
                self.limits.max_nesting_depth(),
            ));
        }
        self.stack
            .try_reserve(1)
            .map_err(|_| ParseFailure::AllocationFailed {
                resource: NdjsonResource::ParserStack,
                requested_additional: 1,
            })?;
        self.reserve_event()?;
        self.offset = self
            .offset
            .checked_add(1)
            .ok_or(ParseFailure::SizeOverflow)?;
        self.events.push(event);
        self.stack.push(frame);
        Ok(())
    }

    fn close_array(&mut self, start_event_index: usize) -> Result<(), ParseFailure> {
        self.stack.pop();
        let start = match self.events.get(start_event_index) {
            Some(StoredEvent::ArrayStart(span)) => span.start,
            _ => unreachable!("an array frame must reference its start event"),
        };
        self.events[start_event_index] = StoredEvent::ArrayStart(Span::new(start, self.offset));
        self.push_event(StoredEvent::ArrayEnd)
    }

    fn parse_literal(&mut self, literal: &[u8]) -> Result<(), ParseFailure> {
        let start = self.offset;
        let end = start
            .checked_add(literal.len())
            .ok_or(ParseFailure::SizeOverflow)?;
        if self.source.get(start..end) != Some(literal) {
            let kind = if end > self.source.len()
                && self
                    .source
                    .get(start..)
                    .is_some_and(|suffix| literal.starts_with(suffix))
            {
                JsonSyntaxErrorKind::UnexpectedEnd
            } else {
                JsonSyntaxErrorKind::InvalidLiteral
            };
            return Err(Self::syntax(kind, start));
        }
        self.check_scalar_token(start, end)?;
        self.offset = end;
        Ok(())
    }

    fn parse_number(&mut self) -> Result<(Span, ValidatedJsonNumberSyntax), ParseFailure> {
        let start = self.offset;
        self.consume_if(b'-');
        match self.peek() {
            Some(b'0') => {
                self.offset += 1;
                if self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
                    return Err(Self::syntax(
                        JsonSyntaxErrorKind::InvalidNumber,
                        self.offset,
                    ));
                }
            }
            Some(b'1'..=b'9') => {
                self.offset += 1;
                self.consume_ascii_digits();
            }
            Some(_) => {
                return Err(Self::syntax(
                    JsonSyntaxErrorKind::InvalidNumber,
                    self.offset,
                ));
            }
            None => {
                return Err(Self::syntax(
                    JsonSyntaxErrorKind::UnexpectedEnd,
                    self.offset,
                ));
            }
        }

        let mut dot_position = None;
        if self.peek() == Some(b'.') {
            dot_position = Some(self.offset - start);
            self.offset += 1;
            if !self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
                let kind = if self.peek().is_none() {
                    JsonSyntaxErrorKind::UnexpectedEnd
                } else {
                    JsonSyntaxErrorKind::InvalidNumber
                };
                return Err(Self::syntax(kind, self.offset));
            }
            self.consume_ascii_digits();
        }

        let mut exponent_position = None;
        if self.peek().is_some_and(|byte| matches!(byte, b'e' | b'E')) {
            exponent_position = Some(self.offset - start);
            self.offset += 1;
            if self.peek().is_some_and(|byte| matches!(byte, b'+' | b'-')) {
                self.offset += 1;
            }
            if !self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
                let kind = if self.peek().is_none() {
                    JsonSyntaxErrorKind::UnexpectedEnd
                } else {
                    JsonSyntaxErrorKind::InvalidNumber
                };
                return Err(Self::syntax(kind, self.offset));
            }
            self.consume_ascii_digits();
        }

        self.check_scalar_token(start, self.offset)?;
        Ok((
            Span::new(start, self.offset),
            ValidatedJsonNumberSyntax::new(dot_position, exponent_position),
        ))
    }

    fn parse_string(&mut self) -> Result<(Span, Option<Span>), ParseFailure> {
        let raw_start = self.offset;
        let (raw_end, contains_escape, contains_non_ascii) = self.find_string_end(raw_start)?;
        self.check_scalar_token(raw_start, raw_end)?;
        let decoded = if contains_escape {
            let decoded_start = self.decoded.len();
            self.decode_string(raw_start + 1, raw_end - 1)?;
            Some(Span::new(decoded_start, self.decoded.len()))
        } else {
            if contains_non_ascii {
                self.validate_utf8_run(raw_start + 1, raw_end - 1)?;
            }
            None
        };
        self.offset = raw_end;
        Ok((Span::new(raw_start, raw_end), decoded))
    }

    fn find_string_end(&self, start: usize) -> Result<(usize, bool, bool), ParseFailure> {
        let mut cursor = start.checked_add(1).ok_or(ParseFailure::SizeOverflow)?;
        let mut contains_escape = false;
        let mut contains_non_ascii = false;
        while self.source.get(cursor).is_some() {
            if cursor
                .checked_add(STRING_SCAN_WORD_BYTES)
                .is_some_and(|end| end <= self.source.len())
            {
                let word_end = cursor + STRING_SCAN_WORD_BYTES;
                let bytes: [u8; STRING_SCAN_WORD_BYTES] = self.source[cursor..word_end]
                    .try_into()
                    .expect("the string scan word has an exact checked length");
                let word = u64::from_le_bytes(bytes);
                let Some(special_position) = first_string_special_position(word) else {
                    contains_non_ascii |= 0 != word & BYTE_HIGH_BITS;
                    cursor = word_end;
                    continue;
                };
                contains_non_ascii |= word_prefix_contains_high_bit(word, special_position);
                cursor += special_position;
            }
            let byte = self.source[cursor];
            match byte {
                b'"' => {
                    let end = cursor.checked_add(1).ok_or(ParseFailure::SizeOverflow)?;
                    return Ok((end, contains_escape, contains_non_ascii));
                }
                b'\\' => {
                    contains_escape = true;
                    cursor = cursor.checked_add(2).ok_or(ParseFailure::SizeOverflow)?;
                }
                0x00..=0x1f => {
                    return Err(Self::syntax(
                        JsonSyntaxErrorKind::UnescapedControlCharacter,
                        cursor,
                    ));
                }
                _ => {
                    contains_non_ascii |= !byte.is_ascii();
                    cursor = cursor.checked_add(1).ok_or(ParseFailure::SizeOverflow)?;
                }
            }
        }
        Err(Self::syntax(
            JsonSyntaxErrorKind::UnexpectedEnd,
            self.source.len(),
        ))
    }

    fn decode_string(&mut self, start: usize, end: usize) -> Result<(), ParseFailure> {
        let mut cursor = start;
        let mut run_start = start;
        while cursor < end {
            if self.source[cursor] != b'\\' {
                cursor += 1;
                continue;
            }
            self.append_utf8_run(run_start, cursor)?;
            let escape_offset = cursor;
            let escape = self
                .source
                .get(cursor + 1)
                .copied()
                .ok_or_else(|| Self::syntax(JsonSyntaxErrorKind::UnexpectedEnd, cursor + 1))?;
            match escape {
                b'"' => self.push_decoded_char('"')?,
                b'\\' => self.push_decoded_char('\\')?,
                b'/' => self.push_decoded_char('/')?,
                b'b' => self.push_decoded_char('\u{0008}')?,
                b'f' => self.push_decoded_char('\u{000c}')?,
                b'n' => self.push_decoded_char('\n')?,
                b'r' => self.push_decoded_char('\r')?,
                b't' => self.push_decoded_char('\t')?,
                b'u' => {
                    let first = self.parse_hex_quad(cursor + 2, end, escape_offset)?;
                    cursor = cursor.checked_add(6).ok_or(ParseFailure::SizeOverflow)?;
                    let scalar = if (0xd800..=0xdbff).contains(&first) {
                        if cursor.checked_add(6).is_none_or(|pair_end| pair_end > end)
                            || self.source.get(cursor) != Some(&b'\\')
                            || self.source.get(cursor + 1) != Some(&b'u')
                        {
                            return Err(Self::syntax(
                                JsonSyntaxErrorKind::UnpairedSurrogate,
                                escape_offset,
                            ));
                        }
                        let second = self.parse_hex_quad(cursor + 2, end, cursor)?;
                        if !(0xdc00..=0xdfff).contains(&second) {
                            return Err(Self::syntax(
                                JsonSyntaxErrorKind::UnpairedSurrogate,
                                escape_offset,
                            ));
                        }
                        cursor = cursor.checked_add(6).ok_or(ParseFailure::SizeOverflow)?;
                        0x1_0000
                            + ((u32::from(first) - 0xd800) << 10)
                            + (u32::from(second) - 0xdc00)
                    } else if (0xdc00..=0xdfff).contains(&first) {
                        return Err(Self::syntax(
                            JsonSyntaxErrorKind::UnpairedSurrogate,
                            escape_offset,
                        ));
                    } else {
                        u32::from(first)
                    };
                    let character = char::from_u32(scalar).ok_or_else(|| {
                        Self::syntax(JsonSyntaxErrorKind::InvalidUnicodeEscape, escape_offset)
                    })?;
                    self.push_decoded_char(character)?;
                    run_start = cursor;
                    continue;
                }
                _ => {
                    return Err(Self::syntax(
                        JsonSyntaxErrorKind::InvalidEscape,
                        escape_offset,
                    ));
                }
            }
            cursor = cursor.checked_add(2).ok_or(ParseFailure::SizeOverflow)?;
            run_start = cursor;
        }
        self.append_utf8_run(run_start, end)
    }

    fn parse_hex_quad(
        &self,
        start: usize,
        string_end: usize,
        error_offset: usize,
    ) -> Result<u16, ParseFailure> {
        let end = start.checked_add(4).ok_or(ParseFailure::SizeOverflow)?;
        if end > string_end {
            return Err(Self::syntax(
                JsonSyntaxErrorKind::InvalidUnicodeEscape,
                error_offset,
            ));
        }
        let mut value = 0_u16;
        for byte in &self.source[start..end] {
            let Some(digit) = hex_digit(*byte) else {
                return Err(Self::syntax(
                    JsonSyntaxErrorKind::InvalidUnicodeEscape,
                    error_offset,
                ));
            };
            value = (value << 4) | u16::from(digit);
        }
        Ok(value)
    }

    fn append_utf8_run(&mut self, start: usize, end: usize) -> Result<(), ParseFailure> {
        let bytes = &self.source[start..end];
        let value = str::from_utf8(bytes).map_err(|source| {
            Self::syntax(
                JsonSyntaxErrorKind::InvalidUtf8,
                start + source.valid_up_to(),
            )
        })?;
        self.decoded
            .try_reserve(value.len())
            .map_err(|_| ParseFailure::AllocationFailed {
                resource: NdjsonResource::DecodedStrings,
                requested_additional: value.len(),
            })?;
        self.decoded.push_str(value);
        Ok(())
    }

    fn validate_utf8_run(&self, start: usize, end: usize) -> Result<(), ParseFailure> {
        str::from_utf8(&self.source[start..end])
            .map(|_| ())
            .map_err(|source| {
                Self::syntax(
                    JsonSyntaxErrorKind::InvalidUtf8,
                    start + source.valid_up_to(),
                )
            })
    }

    fn push_decoded_char(&mut self, value: char) -> Result<(), ParseFailure> {
        let bytes = value.len_utf8();
        self.decoded
            .try_reserve(bytes)
            .map_err(|_| ParseFailure::AllocationFailed {
                resource: NdjsonResource::DecodedStrings,
                requested_additional: bytes,
            })?;
        self.decoded.push(value);
        Ok(())
    }

    fn check_scalar_token(&self, start: usize, end: usize) -> Result<(), ParseFailure> {
        let bytes = end.checked_sub(start).ok_or(ParseFailure::SizeOverflow)?;
        let actual = u64::try_from(bytes).map_err(|_| ParseFailure::SizeOverflow)?;
        let limit = self.limits.max_scalar_token_bytes();
        if actual > limit {
            return Err(Self::limit(
                NdjsonLimitResource::ScalarTokenBytes,
                actual,
                limit,
            ));
        }
        Ok(())
    }

    fn add_value(&mut self) -> Result<(), ParseFailure> {
        let actual = self
            .values
            .checked_add(1)
            .ok_or(ParseFailure::SizeOverflow)?;
        let limit = self.limits.max_values();
        if actual > limit {
            return Err(Self::limit(NdjsonLimitResource::Values, actual, limit));
        }
        self.values = actual;
        Ok(())
    }

    fn push_event(&mut self, event: StoredEvent) -> Result<(), ParseFailure> {
        self.reserve_event()?;
        self.events.push(event);
        Ok(())
    }

    fn reserve_event(&mut self) -> Result<(), ParseFailure> {
        self.events
            .try_reserve(1)
            .map_err(|_| ParseFailure::AllocationFailed {
                resource: NdjsonResource::Events,
                requested_additional: 1,
            })
    }

    fn replace_top(&mut self, frame: Frame) {
        if let Some(top) = self.stack.last_mut() {
            *top = frame;
        }
    }

    fn consume_ascii_digits(&mut self) {
        while self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
            self.offset += 1;
        }
    }

    fn skip_whitespace(&mut self) {
        while self.peek().is_some_and(is_json_whitespace) {
            self.offset += 1;
        }
    }

    fn consume_if(&mut self, expected: u8) -> bool {
        if self.peek() == Some(expected) {
            self.offset += 1;
            true
        } else {
            false
        }
    }

    fn peek(&self) -> Option<u8> {
        self.source.get(self.offset).copied()
    }

    const fn syntax(kind: JsonSyntaxErrorKind, offset: usize) -> ParseFailure {
        ParseFailure::Invalid(NdjsonInvalidRecordKind::Syntax(JsonSyntaxError::new(
            offset, kind,
        )))
    }

    const fn limit(resource: NdjsonLimitResource, actual: u64, limit: u64) -> ParseFailure {
        ParseFailure::Invalid(NdjsonInvalidRecordKind::Limit(NdjsonLimitViolation::new(
            resource, actual, limit,
        )))
    }
}

const fn first_string_special_position(word: u64) -> Option<usize> {
    let high_bits = word_zero_byte_high_bits(word ^ QUOTE_BYTES)
        | word_zero_byte_high_bits(word ^ BACKSLASH_BYTES)
        | word.wrapping_sub(CONTROL_LIMIT_BYTES) & !word & BYTE_HIGH_BITS;
    if 0 == high_bits {
        None
    } else {
        Some(high_bits.trailing_zeros() as usize / u8::BITS as usize)
    }
}

const fn word_zero_byte_high_bits(word: u64) -> u64 {
    word.wrapping_sub(BYTE_ONES) & !word & BYTE_HIGH_BITS
}

const fn word_prefix_contains_high_bit(word: u64, prefix_bytes: usize) -> bool {
    if 0 == prefix_bytes {
        false
    } else {
        let unscanned_bytes = STRING_SCAN_WORD_BYTES - prefix_bytes;
        let unscanned_bits = unscanned_bytes * u8::BITS as usize;
        0 != word & (BYTE_HIGH_BITS >> unscanned_bits)
    }
}

#[cfg(test)]
const fn is_string_special(byte: u8) -> bool {
    byte < b' ' || matches!(byte, b'"' | b'\\')
}

const fn is_json_whitespace(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r')
}

const fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::JsonSyntaxErrorKind;
    use super::NdjsonInvalidRecordKind;
    use super::NdjsonLimits;
    use super::ParseFailure;
    use super::STRING_SCAN_WORD_BYTES;
    use super::StoredEvent;
    use super::first_string_special_position;
    use super::is_string_special;
    use super::parse_document;
    use super::parse_document_prefix;

    #[test]
    fn word_detector_finds_every_byte_at_every_position() {
        for byte in u8::MIN..=u8::MAX {
            for position in 0..STRING_SCAN_WORD_BYTES {
                let mut bytes = [b'a'; STRING_SCAN_WORD_BYTES];
                bytes[position] = byte;
                assert_eq!(
                    bytes.iter().position(|byte| is_string_special(*byte)),
                    first_string_special_position(u64::from_le_bytes(bytes)),
                    "byte {byte:#04x} at word position {position}"
                );
            }
        }
    }

    #[test]
    fn word_detector_finds_first_special_for_every_adjacent_byte_pair() {
        for first in u8::MIN..=u8::MAX {
            for second in u8::MIN..=u8::MAX {
                for position in 0..STRING_SCAN_WORD_BYTES - 1 {
                    let mut bytes = [b'a'; STRING_SCAN_WORD_BYTES];
                    bytes[position] = first;
                    bytes[position + 1] = second;
                    assert_eq!(
                        bytes.iter().position(|byte| is_string_special(*byte)),
                        first_string_special_position(u64::from_le_bytes(bytes)),
                        "bytes {first:#04x}, {second:#04x} at word position {position}"
                    );
                }
            }
        }
    }

    #[test]
    fn lazy_utf8_validation_borrows_valid_unicode_and_decodes_escaped_strings() {
        for prefix_bytes in 0..=3 * STRING_SCAN_WORD_BYTES {
            let mut source = vec![b'"'];
            source.extend(std::iter::repeat_n(b'a', prefix_bytes));
            source.extend_from_slice("\u{e9}".as_bytes());
            source.push(b'"');
            let mut decoded = String::new();
            let mut events = Vec::new();
            let mut stack = Vec::new();
            parse_document(
                &source,
                NdjsonLimits::DEFAULT,
                &mut decoded,
                &mut events,
                &mut stack,
            )
            .expect("valid unescaped Unicode string");

            let [StoredEvent::StringSource(raw)] = events.as_slice() else {
                panic!("valid unescaped Unicode must remain source-backed");
            };
            assert_eq!(&source[1..source.len() - 1], raw.inner_bytes(&source));
            assert_eq!("", decoded);
        }

        let source = br#""caf\u00e9""#;
        let mut decoded = String::new();
        let mut events = Vec::new();
        let mut stack = Vec::new();
        parse_document(
            source,
            NdjsonLimits::DEFAULT,
            &mut decoded,
            &mut events,
            &mut stack,
        )
        .expect("valid escaped Unicode string");
        let [
            StoredEvent::StringDecoded {
                decoded: decoded_span,
                ..
            },
        ] = events.as_slice()
        else {
            panic!("escaped Unicode must use the decoded buffer");
        };
        assert_eq!("caf\u{e9}", decoded_span.string(&decoded));
    }

    #[test]
    fn lazy_utf8_validation_reports_exact_invalid_source_offset_at_every_alignment() {
        for prefix_bytes in 0..=3 * STRING_SCAN_WORD_BYTES {
            let mut source = vec![b'"'];
            source.extend(std::iter::repeat_n(b'a', prefix_bytes));
            source.extend_from_slice("\u{e9}".as_bytes());
            let invalid_offset = source.len();
            source.push(0xff);
            source.push(b'"');
            let error = parse_document(
                &source,
                NdjsonLimits::DEFAULT,
                &mut String::new(),
                &mut Vec::new(),
                &mut Vec::new(),
            )
            .expect_err("invalid UTF-8 must be rejected");
            let ParseFailure::Invalid(NdjsonInvalidRecordKind::Syntax(error)) = error else {
                panic!("unexpected parse error: {error:?}");
            };
            assert_eq!(JsonSyntaxErrorKind::InvalidUtf8, error.kind());
            assert_eq!(invalid_offset, error.byte_offset());
        }
    }

    #[test]
    fn prefix_parser_stops_at_the_root_end_and_matches_full_document_output() {
        let document = br#"{"message":"caf\u00e9","nested":{"array":[1,true,null]},"empty":{}}"#;
        let mut source = document.to_vec();
        source.extend_from_slice(b"\xff trailing bytes that are not JSON");

        let mut prefix_decoded = String::new();
        let mut prefix_events = Vec::new();
        let mut prefix_stack = Vec::new();
        let consumed = parse_document_prefix(
            &source,
            NdjsonLimits::DEFAULT,
            &mut prefix_decoded,
            &mut prefix_events,
            &mut prefix_stack,
        )
        .expect("the root prefix is complete and valid");

        let mut full_decoded = String::new();
        let mut full_events = Vec::new();
        let mut full_stack = Vec::new();
        parse_document(
            document,
            NdjsonLimits::DEFAULT,
            &mut full_decoded,
            &mut full_events,
            &mut full_stack,
        )
        .expect("the isolated root document is valid");

        assert_eq!(document.len(), consumed);
        assert_eq!(full_decoded, prefix_decoded);
        assert_eq!(full_events, prefix_events);
        assert!(prefix_stack.is_empty());

        let mut trailing_decoded = String::new();
        let mut trailing_events = Vec::new();
        let mut trailing_stack = Vec::new();
        let error = parse_document(
            &source,
            NdjsonLimits::DEFAULT,
            &mut trailing_decoded,
            &mut trailing_events,
            &mut trailing_stack,
        )
        .expect_err("full-document parsing must still reject the trailing bytes");
        let ParseFailure::Invalid(NdjsonInvalidRecordKind::Syntax(error)) = error else {
            panic!("unexpected full-document failure: {error:?}");
        };
        assert_eq!(JsonSyntaxErrorKind::TrailingCharacters, error.kind());
        assert_eq!(document.len(), error.byte_offset());
    }
}
