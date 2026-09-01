use std::error::Error;
use std::fmt;
use std::fmt::Display;
use std::fmt::Formatter;
use std::io;
use std::io::Read;
use std::slice;

use super::number::ValidatedJsonNumberSyntax;
use super::parser::ParseFailure;
use super::parser::StoredEvent;
use super::parser::parse_document;

const MEBIBYTE: u64 = 1024 * 1024;
const INPUT_BUFFER_BYTES: usize = 8 * 1024;

/// Hard limits applied independently to each physical-line JSON record.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NdjsonLimits {
    record_bytes: u64,
    nesting_depth: u64,
    values: u64,
    scalar_token_bytes: u64,
}

impl NdjsonLimits {
    /// Conservative defaults for untrusted input.
    pub const DEFAULT: Self = Self::new(16 * MEBIBYTE, 256, 1_000_000, 8 * MEBIBYTE);

    /// Creates a complete set of per-record limits.
    ///
    /// `max_record_bytes` counts every byte before the physical LF, including a CR in CRLF input.
    /// `max_nesting_depth` counts open arrays and objects. `max_values` counts every JSON value,
    /// including arrays and objects but excluding object keys. `max_scalar_token_bytes` applies to
    /// quoted strings and keys (including their quotes), numbers, booleans, and null.
    #[must_use]
    pub const fn new(
        max_record_bytes: u64,
        max_nesting_depth: u64,
        max_values: u64,
        max_scalar_token_bytes: u64,
    ) -> Self {
        Self {
            record_bytes: max_record_bytes,
            nesting_depth: max_nesting_depth,
            values: max_values,
            scalar_token_bytes: max_scalar_token_bytes,
        }
    }

    /// Maximum bytes in one physical-line payload, excluding LF.
    #[must_use]
    pub const fn max_record_bytes(self) -> u64 {
        self.record_bytes
    }

    /// Maximum simultaneously open JSON arrays and objects.
    #[must_use]
    pub const fn max_nesting_depth(self) -> u64 {
        self.nesting_depth
    }

    /// Maximum JSON values in one record.
    #[must_use]
    pub const fn max_values(self) -> u64 {
        self.values
    }

    /// Maximum raw bytes in one scalar token or object key.
    #[must_use]
    pub const fn max_scalar_token_bytes(self) -> u64 {
        self.scalar_token_bytes
    }
}

impl Default for NdjsonLimits {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Action taken after a malformed or over-limit physical record has been drained.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub enum InvalidRecordPolicy {
    /// Return the first invalid record as an error.
    #[default]
    Stop,
    /// Discard the complete physical line and continue at the next physical line.
    Skip,
}

/// Physical-line NDJSON reader configuration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NdjsonOptions {
    limits: NdjsonLimits,
    invalid_records: InvalidRecordPolicy,
}

impl NdjsonOptions {
    /// Creates strict options with default resource limits.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            limits: NdjsonLimits::DEFAULT,
            invalid_records: InvalidRecordPolicy::Stop,
        }
    }

    /// Replaces all per-record limits.
    #[must_use]
    pub const fn with_limits(mut self, limits: NdjsonLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Selects whether an invalid physical record stops or is skipped.
    #[must_use]
    pub const fn with_invalid_record_policy(mut self, policy: InvalidRecordPolicy) -> Self {
        self.invalid_records = policy;
        self
    }

    /// Returns the configured per-record limits.
    #[must_use]
    pub const fn limits(self) -> NdjsonLimits {
        self.limits
    }

    /// Returns the configured invalid-record policy.
    #[must_use]
    pub const fn invalid_record_policy(self) -> InvalidRecordPolicy {
        self.invalid_records
    }
}

impl Default for NdjsonOptions {
    fn default() -> Self {
        Self::new()
    }
}

/// A resource guarded by a user-configurable per-record limit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum NdjsonLimitResource {
    /// Bytes before a physical line's LF terminator.
    RecordBytes,
    /// Simultaneously open arrays and objects.
    NestingDepth,
    /// JSON values, including container values.
    Values,
    /// Raw bytes in a string, key, number, boolean, or null token.
    ScalarTokenBytes,
}

impl Display for NdjsonLimitResource {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::RecordBytes => "record bytes",
            Self::NestingDepth => "JSON nesting depth",
            Self::Values => "JSON values",
            Self::ScalarTokenBytes => "JSON scalar token bytes",
        })
    }
}

/// Exact measurements for one rejected per-record limit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NdjsonLimitViolation {
    resource: NdjsonLimitResource,
    actual: u64,
    limit: u64,
}

impl NdjsonLimitViolation {
    pub(super) const fn new(resource: NdjsonLimitResource, actual: u64, limit: u64) -> Self {
        Self {
            resource,
            actual,
            limit,
        }
    }

    /// Returns the limited resource.
    #[must_use]
    pub const fn resource(self) -> NdjsonLimitResource {
        self.resource
    }

    /// Returns the observed size or count.
    #[must_use]
    pub const fn actual(self) -> u64 {
        self.actual
    }

    /// Returns the configured maximum.
    #[must_use]
    pub const fn limit(self) -> u64 {
        self.limit
    }
}

impl Display for NdjsonLimitViolation {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} {} exceeds limit {}",
            self.actual, self.resource, self.limit
        )
    }
}

impl Error for NdjsonLimitViolation {}

/// Classification of a JSON grammar or encoding error.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum JsonSyntaxErrorKind {
    /// A JSON object was required by an object-stream adapter.
    ExpectedObject,
    /// A JSON value was required.
    ExpectedValue,
    /// A quoted object key was required.
    ExpectedObjectKey,
    /// A colon was required after an object key.
    ExpectedColon,
    /// A comma or the current container's closing delimiter was required.
    ExpectedCommaOrEnd,
    /// The record ended before the current token or container was complete.
    UnexpectedEnd,
    /// Non-whitespace bytes followed the complete root value.
    TrailingCharacters,
    /// A `true`, `false`, or `null` token was malformed.
    InvalidLiteral,
    /// A number did not follow the JSON number grammar.
    InvalidNumber,
    /// A reverse-solidus escape was not one of JSON's defined escapes.
    InvalidEscape,
    /// A `\\u` escape did not contain four hexadecimal digits.
    InvalidUnicodeEscape,
    /// A UTF-16 surrogate escape was missing its required pair.
    UnpairedSurrogate,
    /// A string contained an unescaped ASCII control byte.
    UnescapedControlCharacter,
    /// Unescaped string bytes were not valid UTF-8.
    InvalidUtf8,
}

impl Display for JsonSyntaxErrorKind {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::ExpectedObject => "expected a JSON object",
            Self::ExpectedValue => "expected a JSON value",
            Self::ExpectedObjectKey => "expected a quoted object key",
            Self::ExpectedColon => "expected ':' after object key",
            Self::ExpectedCommaOrEnd => "expected ',' or a closing delimiter",
            Self::UnexpectedEnd => "unexpected end of record",
            Self::TrailingCharacters => "trailing characters after root JSON value",
            Self::InvalidLiteral => "invalid JSON literal",
            Self::InvalidNumber => "invalid JSON number",
            Self::InvalidEscape => "invalid JSON string escape",
            Self::InvalidUnicodeEscape => "invalid JSON Unicode escape",
            Self::UnpairedSurrogate => "unpaired JSON Unicode surrogate",
            Self::UnescapedControlCharacter => "unescaped control byte in JSON string",
            Self::InvalidUtf8 => "invalid UTF-8 in JSON string",
        })
    }
}

/// A syntax error at a zero-based byte offset within the trimmed JSON document.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JsonSyntaxError {
    offset: usize,
    kind: JsonSyntaxErrorKind,
}

impl JsonSyntaxError {
    pub(super) const fn new(offset: usize, kind: JsonSyntaxErrorKind) -> Self {
        Self { offset, kind }
    }

    /// Returns the zero-based byte offset within [`NdjsonRecord::json_bytes`].
    #[must_use]
    pub const fn byte_offset(self) -> usize {
        self.offset
    }

    /// Returns the grammar or encoding error classification.
    #[must_use]
    pub const fn kind(self) -> JsonSyntaxErrorKind {
        self.kind
    }
}

impl Display for JsonSyntaxError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} at document byte {}", self.kind, self.offset)
    }
}

impl Error for JsonSyntaxError {}

/// Why one complete physical record was rejected.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum NdjsonInvalidRecordKind {
    /// The document was not valid JSON.
    Syntax(JsonSyntaxError),
    /// A configured per-record limit was exceeded.
    Limit(NdjsonLimitViolation),
}

impl Display for NdjsonInvalidRecordKind {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Syntax(source) => Display::fmt(source, formatter),
            Self::Limit(source) => Display::fmt(source, formatter),
        }
    }
}

impl Error for NdjsonInvalidRecordKind {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Syntax(source) => Some(source),
            Self::Limit(source) => Some(source),
        }
    }
}

/// Context for one invalid physical line.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NdjsonInvalidRecord {
    line_number: u64,
    input_offset: u64,
    kind: NdjsonInvalidRecordKind,
}

impl NdjsonInvalidRecord {
    const fn new(line_number: u64, input_offset: u64, kind: NdjsonInvalidRecordKind) -> Self {
        Self {
            line_number,
            input_offset,
            kind,
        }
    }

    /// Returns the one-based physical line number.
    #[must_use]
    pub const fn line_number(self) -> u64 {
        self.line_number
    }

    /// Returns the zero-based stream offset of the physical line.
    #[must_use]
    pub const fn input_offset(self) -> u64 {
        self.input_offset
    }

    /// Returns the reason the record was rejected.
    #[must_use]
    pub const fn kind(self) -> NdjsonInvalidRecordKind {
        self.kind
    }
}

impl Display for NdjsonInvalidRecord {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "invalid NDJSON record on physical line {} at input byte {}: {}",
            self.line_number, self.input_offset, self.kind
        )
    }
}

impl Error for NdjsonInvalidRecord {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.kind)
    }
}

/// Internal allocation or buffer resource named by a fatal reader error.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum NdjsonResource {
    /// Reusable physical-record bytes.
    RecordBuffer,
    /// Flat parsed-event storage.
    Events,
    /// Decoded key and string storage.
    DecodedStrings,
    /// Iterative container-state storage.
    ParserStack,
}

impl Display for NdjsonResource {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::RecordBuffer => "NDJSON record buffer",
            Self::Events => "NDJSON event buffer",
            Self::DecodedStrings => "decoded JSON string buffer",
            Self::ParserStack => "JSON parser stack",
        })
    }
}

/// Fatal input, accounting, or allocation error.
#[derive(Debug)]
#[non_exhaustive]
pub enum NdjsonError {
    /// Reading the caller-owned input failed.
    Input(io::Error),
    /// The configured stop policy rejected a complete physical record.
    InvalidRecord(NdjsonInvalidRecord),
    /// A bounded reusable buffer could not reserve more storage.
    AllocationFailed {
        /// Buffer being grown.
        resource: NdjsonResource,
        /// Exact additional logical bytes or entries requested.
        requested_additional: usize,
    },
    /// A stream counter could not be represented as `u64`.
    SizeOverflow,
}

impl Display for NdjsonError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Input(source) => write!(formatter, "failed to read NDJSON input: {source}"),
            Self::InvalidRecord(source) => Display::fmt(source, formatter),
            Self::AllocationFailed {
                resource,
                requested_additional,
            } => write!(
                formatter,
                "failed to reserve {requested_additional} additional units for {resource}"
            ),
            Self::SizeOverflow => formatter.write_str("NDJSON stream size counter overflow"),
        }
    }
}

impl Error for NdjsonError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Input(source) => Some(source),
            Self::InvalidRecord(source) => Some(source),
            Self::AllocationFailed { .. } | Self::SizeOverflow => None,
        }
    }
}

/// Reader or caller-owned sink failure from [`NdjsonReader::read_record`] and
/// [`NdjsonReader::read_to_end`].
#[derive(Debug)]
#[non_exhaustive]
pub enum NdjsonReadError<E> {
    /// Framing, parsing, input, or allocation failed before a sink call completed.
    Reader(NdjsonError),
    /// The sink rejected a valid borrowed record.
    Sink {
        /// One-based source physical line number.
        line_number: u64,
        /// Zero-based successful-record index presented to the sink.
        record_index: u64,
        /// Caller-owned sink error.
        source: E,
    },
}

impl<E: Display> Display for NdjsonReadError<E> {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Reader(source) => Display::fmt(source, formatter),
            Self::Sink {
                line_number,
                record_index,
                source,
            } => write!(
                formatter,
                "NDJSON sink failed for record {record_index} on physical line {line_number}: \
                 {source}"
            ),
        }
    }
}

impl<E: Error + 'static> Error for NdjsonReadError<E> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Reader(source) => Some(source),
            Self::Sink { source, .. } => Some(source),
        }
    }
}

impl<E> From<NdjsonError> for NdjsonReadError<E> {
    fn from(source: NdjsonError) -> Self {
        Self::Reader(source)
    }
}

/// One decoded JSON string or object key with its exact quoted source token.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JsonString<'a> {
    raw_json: &'a [u8],
    decoded: &'a [u8],
}

impl<'a> JsonString<'a> {
    pub(super) const fn new(raw_json: &'a [u8], decoded: &'a str) -> Self {
        Self::new_bytes(raw_json, decoded.as_bytes())
    }

    pub(super) const fn new_bytes(raw_json: &'a [u8], decoded: &'a [u8]) -> Self {
        Self { raw_json, decoded }
    }

    /// Returns the exact quoted JSON token, including escapes and quote delimiters.
    #[must_use]
    pub const fn raw_json(self) -> &'a [u8] {
        self.raw_json
    }

    /// Returns the decoded Unicode scalar string.
    ///
    /// # Panics
    ///
    /// Panics if an internal producer violates the invariant that decoded bytes are valid UTF-8.
    /// Values returned by the bundled readers always satisfy this invariant.
    #[must_use]
    pub fn decoded(self) -> &'a str {
        std::str::from_utf8(self.decoded)
            .expect("decoded JSON string bytes must contain validated UTF-8")
    }

    /// Returns the decoded UTF-8 bytes without revalidating them.
    #[must_use]
    pub const fn decoded_bytes(self) -> &'a [u8] {
        self.decoded
    }
}

/// One validated JSON array borrowing its exact source lexeme.
///
/// The slice begins with `[` and ends with its matching `]`. It preserves every byte between those
/// delimiters, including insignificant whitespace and the original spelling of strings and
/// numbers. The surrounding record or document buffer owns the bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JsonArrayRef<'a> {
    raw_json: &'a [u8],
}

impl<'a> JsonArrayRef<'a> {
    pub(super) const fn new(raw_json: &'a [u8]) -> Self {
        Self { raw_json }
    }

    /// Returns the exact validated array token, including both delimiters.
    #[must_use]
    pub const fn raw_json(self) -> &'a [u8] {
        self.raw_json
    }
}

/// One event in a flat, balanced traversal of a JSON document.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum JsonEvent<'a> {
    /// Start of an object value.
    ObjectStart,
    /// End of an object value.
    ObjectEnd,
    /// Start of an array value, borrowing the complete exact array lexeme.
    ArrayStart(JsonArrayRef<'a>),
    /// End of an array value.
    ArrayEnd,
    /// An object key, preserving both source token and decoded text.
    ObjectKey(JsonString<'a>),
    /// A string value, preserving both source token and decoded text.
    String(JsonString<'a>),
    /// An exact JSON number lexeme, without numeric conversion.
    Number(&'a [u8]),
    /// A JSON boolean value.
    Boolean(bool),
    /// A JSON null value.
    Null,
}

/// Iterator over one record's borrowed flat events.
#[derive(Clone, Debug)]
pub struct JsonEvents<'a> {
    raw_json: &'a [u8],
    decoded: &'a str,
    events: slice::Iter<'a, StoredEvent>,
}

impl<'a> JsonEvents<'a> {
    pub(super) fn new(raw_json: &'a [u8], decoded: &'a str, events: &'a [StoredEvent]) -> Self {
        Self {
            raw_json,
            decoded,
            events: events.iter(),
        }
    }

    pub(super) fn next_with_number_syntax(
        &mut self,
    ) -> Option<(JsonEvent<'a>, Option<ValidatedJsonNumberSyntax>)> {
        self.next_stored().map(|stored| {
            (
                stored.resolve(self.raw_json, self.decoded),
                stored.number_syntax(),
            )
        })
    }

    pub(super) fn next_stored(&mut self) -> Option<StoredEvent> {
        self.events.next().copied()
    }

    pub(super) const fn raw_json(&self) -> &'a [u8] {
        self.raw_json
    }

    pub(super) const fn decoded(&self) -> &'a str {
        self.decoded
    }
}

impl<'a> Iterator for JsonEvents<'a> {
    type Item = JsonEvent<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        self.next_with_number_syntax().map(|(event, _)| event)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.events.size_hint()
    }
}

impl ExactSizeIterator for JsonEvents<'_> {}
impl std::iter::FusedIterator for JsonEvents<'_> {}

/// One complete valid physical-line JSON record borrowed for a sink call.
///
/// None of the returned byte/string/event views outlive the current sink invocation. A sink must
/// copy any data it wants to retain. The physical line excludes LF but retains all other bytes;
/// the JSON document view removes surrounding JSON whitespace.
#[derive(Clone, Copy, Debug)]
pub struct NdjsonRecord<'a> {
    line_bytes: &'a [u8],
    json_bytes: &'a [u8],
    decoded: &'a str,
    events: &'a [StoredEvent],
    line_number: u64,
    input_offset: u64,
    record_index: u64,
}

impl<'a> NdjsonRecord<'a> {
    /// Returns the exact physical-line payload, excluding LF.
    #[must_use]
    pub const fn line_bytes(self) -> &'a [u8] {
        self.line_bytes
    }

    /// Returns the complete JSON document without surrounding JSON whitespace.
    #[must_use]
    pub const fn json_bytes(self) -> &'a [u8] {
        self.json_bytes
    }

    /// Returns the record's balanced, depth-first flat events.
    #[must_use]
    pub fn events(self) -> JsonEvents<'a> {
        JsonEvents::new(self.json_bytes, self.decoded, self.events)
    }

    /// Returns the one-based source physical line number.
    #[must_use]
    pub const fn line_number(self) -> u64 {
        self.line_number
    }

    /// Returns the zero-based stream offset of the physical line.
    #[must_use]
    pub const fn input_offset(self) -> u64 {
        self.input_offset
    }

    /// Returns the zero-based index among successfully accepted sink records.
    #[must_use]
    pub const fn record_index(self) -> u64 {
        self.record_index
    }
}

/// Synchronous destination for complete borrowed NDJSON records.
///
/// The reader invokes the sink only after framing, syntax, UTF-8, and all configured limits have
/// been validated. Borrowed record data is invalidated by the next reader operation and therefore
/// must be copied if it needs to be retained.
pub trait NdjsonRecordSink {
    /// Caller-selected sink error.
    type Error;

    /// Accepts one complete borrowed record.
    ///
    /// # Errors
    ///
    /// Returns the caller's error when the record cannot be accepted.
    fn write_record(&mut self, record: NdjsonRecord<'_>) -> Result<(), Self::Error>;
}

impl<F, E> NdjsonRecordSink for F
where
    F: for<'record> FnMut(NdjsonRecord<'record>) -> Result<(), E>,
{
    type Error = E;

    fn write_record(&mut self, record: NdjsonRecord<'_>) -> Result<(), Self::Error> {
        self(record)
    }
}

/// Successful physical-line ingestion counters.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct NdjsonStats {
    input_bytes: u64,
    physical_lines: u64,
    blank_lines: u64,
    skipped_invalid_records: u64,
    records: u64,
}

impl NdjsonStats {
    /// Returns bytes consumed from the input, including LF terminators.
    #[must_use]
    pub const fn input_bytes(self) -> u64 {
        self.input_bytes
    }

    /// Returns complete physical lines consumed, including blank and invalid lines.
    #[must_use]
    pub const fn physical_lines(self) -> u64 {
        self.physical_lines
    }

    /// Returns whitespace-only physical lines ignored by the reader.
    #[must_use]
    pub const fn blank_lines(self) -> u64 {
        self.blank_lines
    }

    /// Returns invalid physical records discarded under [`InvalidRecordPolicy::Skip`].
    #[must_use]
    pub const fn skipped_invalid_records(self) -> u64 {
        self.skipped_invalid_records
    }

    /// Returns valid records whose sink call completed successfully.
    #[must_use]
    pub const fn records(self) -> u64 {
        self.records
    }
}

/// Streaming, bounded reader for physical-line NDJSON.
///
/// The reader owns no filesystem paths or output policy. It uses a fixed input chunk, reuses one
/// bounded record buffer, and parses with an explicit stack rather than recursive descent. An
/// escaped `\\n` is ordinary record content; only an actual LF byte ends a physical record.
pub struct NdjsonReader<R> {
    input: R,
    options: NdjsonOptions,
    input_buffer: [u8; INPUT_BUFFER_BYTES],
    input_start: usize,
    input_end: usize,
    reached_eof: bool,
    record: Vec<u8>,
    decoded: String,
    events: Vec<StoredEvent>,
    parser_stack: Vec<super::parser::Frame>,
    stats: NdjsonStats,
}

impl<R: Read> NdjsonReader<R> {
    /// Creates a reader over a caller-owned byte stream.
    #[must_use]
    pub fn new(input: R, options: NdjsonOptions) -> Self {
        Self {
            input,
            options,
            input_buffer: [0; INPUT_BUFFER_BYTES],
            input_start: 0,
            input_end: 0,
            reached_eof: false,
            record: Vec::new(),
            decoded: String::new(),
            events: Vec::new(),
            parser_stack: Vec::new(),
            stats: NdjsonStats::default(),
        }
    }

    /// Returns the immutable reader configuration.
    #[must_use]
    pub const fn options(&self) -> NdjsonOptions {
        self.options
    }

    /// Returns counters accumulated so far.
    #[must_use]
    pub const fn stats(&self) -> NdjsonStats {
        self.stats
    }

    /// Returns the underlying input, discarding unread buffered bytes.
    #[must_use]
    pub fn into_inner(self) -> R {
        self.input
    }

    #[cfg(test)]
    pub(super) const fn buffer_capacities(&self) -> (usize, usize, usize, usize) {
        (
            self.record.capacity(),
            self.decoded.capacity(),
            self.events.capacity(),
            self.parser_stack.capacity(),
        )
    }

    /// Reads and validates the next non-blank record, then invokes `sink` exactly once.
    ///
    /// Returns `Ok(true)` after a successful sink call and `Ok(false)` at end of input. Blank lines
    /// and invalid lines under [`InvalidRecordPolicy::Skip`] are consumed internally.
    ///
    /// # Errors
    ///
    /// Returns [`NdjsonReadError::Reader`] for input, syntax, limit, accounting, or allocation
    /// failures and [`NdjsonReadError::Sink`] for the caller's sink error.
    pub fn read_record<S: NdjsonRecordSink + ?Sized>(
        &mut self,
        sink: &mut S,
    ) -> Result<bool, NdjsonReadError<S::Error>> {
        let Some(context) = self.prepare_record().map_err(NdjsonReadError::Reader)? else {
            return Ok(false);
        };
        let record_index = self.stats.records;
        let record = NdjsonRecord {
            line_bytes: &self.record,
            json_bytes: &self.record[context.json_start..context.json_end],
            decoded: &self.decoded,
            events: &self.events,
            line_number: context.line_number,
            input_offset: context.input_offset,
            record_index,
        };
        sink.write_record(record)
            .map_err(|source| NdjsonReadError::Sink {
                line_number: context.line_number,
                record_index,
                source,
            })?;
        self.stats.records = self
            .stats
            .records
            .checked_add(1)
            .ok_or(NdjsonReadError::Reader(NdjsonError::SizeOverflow))?;
        Ok(true)
    }

    /// Drains the input into a caller-owned record sink.
    ///
    /// # Errors
    ///
    /// Returns the first reader or sink error. The returned statistics count only sink calls that
    /// completed successfully.
    pub fn read_to_end<S: NdjsonRecordSink + ?Sized>(
        &mut self,
        sink: &mut S,
    ) -> Result<NdjsonStats, NdjsonReadError<S::Error>> {
        while self.read_record(sink)? {}
        Ok(self.stats)
    }

    fn prepare_record(&mut self) -> Result<Option<RecordContext>, NdjsonError> {
        loop {
            let Some(line) = self.read_physical_line()? else {
                return Ok(None);
            };
            self.stats.physical_lines = self
                .stats
                .physical_lines
                .checked_add(1)
                .ok_or(NdjsonError::SizeOverflow)?;
            let context = RecordContext {
                line_number: self.stats.physical_lines,
                input_offset: line.input_offset,
                json_start: 0,
                json_end: 0,
            };

            if let Some(actual) = line.oversized_bytes {
                let invalid = NdjsonInvalidRecord::new(
                    context.line_number,
                    context.input_offset,
                    NdjsonInvalidRecordKind::Limit(NdjsonLimitViolation::new(
                        NdjsonLimitResource::RecordBytes,
                        actual,
                        self.options.limits.record_bytes,
                    )),
                );
                self.handle_invalid(invalid)?;
                continue;
            }

            let Some((json_start, json_end)) = trim_json_whitespace(&self.record) else {
                self.stats.blank_lines = self
                    .stats
                    .blank_lines
                    .checked_add(1)
                    .ok_or(NdjsonError::SizeOverflow)?;
                continue;
            };
            self.decoded.clear();
            self.events.clear();
            self.parser_stack.clear();
            let json = &self.record[json_start..json_end];
            match parse_document(
                json,
                self.options.limits,
                &mut self.decoded,
                &mut self.events,
                &mut self.parser_stack,
            ) {
                Ok(()) => {
                    return Ok(Some(RecordContext {
                        json_start,
                        json_end,
                        ..context
                    }));
                }
                Err(ParseFailure::Invalid(kind)) => {
                    let invalid =
                        NdjsonInvalidRecord::new(context.line_number, context.input_offset, kind);
                    self.handle_invalid(invalid)?;
                }
                Err(ParseFailure::AllocationFailed {
                    resource,
                    requested_additional,
                }) => {
                    return Err(NdjsonError::AllocationFailed {
                        resource,
                        requested_additional,
                    });
                }
                Err(ParseFailure::SizeOverflow) => return Err(NdjsonError::SizeOverflow),
            }
        }
    }

    fn handle_invalid(&mut self, invalid: NdjsonInvalidRecord) -> Result<(), NdjsonError> {
        match self.options.invalid_records {
            InvalidRecordPolicy::Stop => Err(NdjsonError::InvalidRecord(invalid)),
            InvalidRecordPolicy::Skip => {
                self.stats.skipped_invalid_records = self
                    .stats
                    .skipped_invalid_records
                    .checked_add(1)
                    .ok_or(NdjsonError::SizeOverflow)?;
                Ok(())
            }
        }
    }

    fn read_physical_line(&mut self) -> Result<Option<PhysicalLine>, NdjsonError> {
        self.record.clear();
        let input_offset = self.stats.input_bytes;
        let mut actual_bytes = 0_u64;
        let mut saw_line = false;

        loop {
            if self.input_start == self.input_end {
                if self.reached_eof {
                    return if saw_line {
                        Ok(Some(PhysicalLine::new(
                            input_offset,
                            actual_bytes,
                            self.options.limits.record_bytes,
                        )))
                    } else {
                        Ok(None)
                    };
                }
                match self.input.read(&mut self.input_buffer) {
                    Ok(0) => {
                        self.reached_eof = true;
                        continue;
                    }
                    Ok(read) => {
                        self.input_start = 0;
                        self.input_end = read;
                    }
                    Err(source) if source.kind() == io::ErrorKind::Interrupted => continue,
                    Err(source) => return Err(NdjsonError::Input(source)),
                }
            }

            let available = &self.input_buffer[self.input_start..self.input_end];
            let newline = available.iter().position(|byte| *byte == b'\n');
            let payload_len = newline.unwrap_or(available.len());
            let payload = &available[..payload_len];
            let payload_len_u64 =
                u64::try_from(payload_len).map_err(|_| NdjsonError::SizeOverflow)?;
            let new_actual = actual_bytes
                .checked_add(payload_len_u64)
                .ok_or(NdjsonError::SizeOverflow)?;
            if new_actual <= self.options.limits.record_bytes {
                self.record.try_reserve(payload_len).map_err(|_| {
                    NdjsonError::AllocationFailed {
                        resource: NdjsonResource::RecordBuffer,
                        requested_additional: payload_len,
                    }
                })?;
                self.record.extend_from_slice(payload);
            }
            actual_bytes = new_actual;
            saw_line |= !payload.is_empty();

            let consumed = payload_len
                .checked_add(usize::from(newline.is_some()))
                .ok_or(NdjsonError::SizeOverflow)?;
            self.input_start = self
                .input_start
                .checked_add(consumed)
                .ok_or(NdjsonError::SizeOverflow)?;
            let consumed_u64 = u64::try_from(consumed).map_err(|_| NdjsonError::SizeOverflow)?;
            self.stats.input_bytes = self
                .stats
                .input_bytes
                .checked_add(consumed_u64)
                .ok_or(NdjsonError::SizeOverflow)?;

            if newline.is_some() {
                return Ok(Some(PhysicalLine::new(
                    input_offset,
                    actual_bytes,
                    self.options.limits.record_bytes,
                )));
            }
        }
    }
}

struct PhysicalLine {
    input_offset: u64,
    oversized_bytes: Option<u64>,
}

impl PhysicalLine {
    const fn new(input_offset: u64, actual_bytes: u64, limit: u64) -> Self {
        Self {
            input_offset,
            oversized_bytes: if actual_bytes > limit {
                Some(actual_bytes)
            } else {
                None
            },
        }
    }
}

#[derive(Clone, Copy)]
struct RecordContext {
    line_number: u64,
    input_offset: u64,
    json_start: usize,
    json_end: usize,
}

fn trim_json_whitespace(bytes: &[u8]) -> Option<(usize, usize)> {
    let start = bytes.iter().position(|byte| !is_json_whitespace(*byte))?;
    let end = bytes
        .iter()
        .rposition(|byte| !is_json_whitespace(*byte))?
        .checked_add(1)?;
    Some((start, end))
}

const fn is_json_whitespace(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r')
}
