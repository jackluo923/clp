use std::error::Error;
use std::fmt;
use std::fmt::Display;
use std::fmt::Formatter;
use std::io;
use std::io::Read;

use super::ndjson::JsonEvents;
use super::ndjson::JsonSyntaxError;
use super::ndjson::JsonSyntaxErrorKind;
use super::ndjson::NdjsonInvalidRecordKind;
use super::ndjson::NdjsonLimitResource;
use super::ndjson::NdjsonLimits;
use super::parser::Frame;
use super::parser::ParseFailure;
use super::parser::StoredEvent;
use super::parser::parse_document;
use super::parser::parse_document_prefix;

const MEBIBYTE: u64 = 1024 * 1024;
const INPUT_BUFFER_BYTES: usize = 8 * 1024;

/// Hard limits applied independently to each object in a parse-many stream.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ParseManyLimits {
    document_bytes: u64,
    nesting_depth: u64,
    values: u64,
    scalar_token_bytes: u64,
}

impl ParseManyLimits {
    /// Defaults with the C++ CLI's 512 MiB maximum document size and conservative parser bounds.
    pub const DEFAULT: Self = Self::new(512 * MEBIBYTE, 256, 1_000_000, 8 * MEBIBYTE);

    /// Creates a complete set of per-document limits.
    ///
    /// `max_document_bytes` counts from the root `{` through its matching `}`, excluding
    /// inter-document whitespace. The remaining limits have the same semantics as their NDJSON
    /// counterparts.
    #[must_use]
    pub const fn new(
        max_document_bytes: u64,
        max_nesting_depth: u64,
        max_values: u64,
        max_scalar_token_bytes: u64,
    ) -> Self {
        Self {
            document_bytes: max_document_bytes,
            nesting_depth: max_nesting_depth,
            values: max_values,
            scalar_token_bytes: max_scalar_token_bytes,
        }
    }

    /// Maximum bytes in one root object, including its braces.
    #[must_use]
    pub const fn max_document_bytes(self) -> u64 {
        self.document_bytes
    }

    /// Maximum simultaneously open JSON arrays and objects.
    #[must_use]
    pub const fn max_nesting_depth(self) -> u64 {
        self.nesting_depth
    }

    /// Maximum JSON values in one document, including container values.
    #[must_use]
    pub const fn max_values(self) -> u64 {
        self.values
    }

    /// Maximum raw bytes in one scalar token or object key.
    #[must_use]
    pub const fn max_scalar_token_bytes(self) -> u64 {
        self.scalar_token_bytes
    }

    const fn parser_limits(self) -> NdjsonLimits {
        NdjsonLimits::new(
            self.document_bytes,
            self.nesting_depth,
            self.values,
            self.scalar_token_bytes,
        )
    }
}

impl Default for ParseManyLimits {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Handling for an incomplete final object at physical EOF.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub enum IncompleteDocumentPolicy {
    /// Report the incomplete suffix as an invalid document.
    #[default]
    Error,
    /// Ignore the incomplete suffix after accounting its bytes, matching the pinned C++ CLI.
    Ignore,
}

/// Configuration for a C++-style parse-many object stream.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ParseManyOptions {
    limits: ParseManyLimits,
    incomplete_document: IncompleteDocumentPolicy,
}

impl ParseManyOptions {
    /// Creates strict options with default limits.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            limits: ParseManyLimits::DEFAULT,
            incomplete_document: IncompleteDocumentPolicy::Error,
        }
    }

    /// Replaces all per-document limits.
    #[must_use]
    pub const fn with_limits(mut self, limits: ParseManyLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Returns the configured per-document limits.
    #[must_use]
    pub const fn limits(self) -> ParseManyLimits {
        self.limits
    }

    /// Selects whether an incomplete final object is an error or an ignored suffix.
    #[must_use]
    pub const fn with_incomplete_document_policy(
        mut self,
        policy: IncompleteDocumentPolicy,
    ) -> Self {
        self.incomplete_document = policy;
        self
    }

    /// Returns incomplete-final-object handling.
    #[must_use]
    pub const fn incomplete_document_policy(self) -> IncompleteDocumentPolicy {
        self.incomplete_document
    }
}

impl Default for ParseManyOptions {
    fn default() -> Self {
        Self::new()
    }
}

/// Resource guarded by a parse-many per-document limit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ParseManyLimitResource {
    /// Bytes from the root opening brace through its matching closing brace.
    DocumentBytes,
    /// Simultaneously open arrays and objects.
    NestingDepth,
    /// JSON values, including container values.
    Values,
    /// Raw bytes in a string, key, number, boolean, or null token.
    ScalarTokenBytes,
}

impl Display for ParseManyLimitResource {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::DocumentBytes => "document bytes",
            Self::NestingDepth => "JSON nesting depth",
            Self::Values => "JSON values",
            Self::ScalarTokenBytes => "JSON scalar token bytes",
        })
    }
}

/// Exact measurements for one rejected parse-many limit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ParseManyLimitViolation {
    resource: ParseManyLimitResource,
    actual: u64,
    limit: u64,
}

impl ParseManyLimitViolation {
    const fn new(resource: ParseManyLimitResource, actual: u64, limit: u64) -> Self {
        Self {
            resource,
            actual,
            limit,
        }
    }

    /// Returns the limited resource.
    #[must_use]
    pub const fn resource(self) -> ParseManyLimitResource {
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

impl Display for ParseManyLimitViolation {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} {} exceeds limit {}",
            self.actual, self.resource, self.limit
        )
    }
}

impl Error for ParseManyLimitViolation {}

/// Why one parse-many document was rejected.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ParseManyInvalidDocumentKind {
    /// The framed object was not valid JSON, or the next token was not an object.
    Syntax(JsonSyntaxError),
    /// A configured per-document limit was exceeded.
    Limit(ParseManyLimitViolation),
}

impl Display for ParseManyInvalidDocumentKind {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Syntax(source) => Display::fmt(source, formatter),
            Self::Limit(source) => Display::fmt(source, formatter),
        }
    }
}

impl Error for ParseManyInvalidDocumentKind {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Syntax(source) => Some(source),
            Self::Limit(source) => Some(source),
        }
    }
}

/// Context for one invalid or over-limit document.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ParseManyInvalidDocument {
    document_index: u64,
    input_offset: u64,
    kind: ParseManyInvalidDocumentKind,
}

impl ParseManyInvalidDocument {
    const fn new(
        document_index: u64,
        input_offset: u64,
        kind: ParseManyInvalidDocumentKind,
    ) -> Self {
        Self {
            document_index,
            input_offset,
            kind,
        }
    }

    /// Returns the zero-based successful-document index expected at this position.
    #[must_use]
    pub const fn document_index(self) -> u64 {
        self.document_index
    }

    /// Returns the zero-based input byte offset of the document or invalid next token.
    #[must_use]
    pub const fn input_offset(self) -> u64 {
        self.input_offset
    }

    /// Returns the rejection reason.
    #[must_use]
    pub const fn kind(self) -> ParseManyInvalidDocumentKind {
        self.kind
    }
}

impl Display for ParseManyInvalidDocument {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "invalid parse-many document {} at input byte {}: {}",
            self.document_index, self.input_offset, self.kind
        )
    }
}

impl Error for ParseManyInvalidDocument {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.kind)
    }
}

/// Reusable buffer named by a fatal parse-many allocation error.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ParseManyResource {
    /// Exact bytes of the current root object.
    DocumentBuffer,
    /// Flat parsed-event storage.
    Events,
    /// Decoded key and string storage.
    DecodedStrings,
    /// Iterative JSON parser stack.
    ParserStack,
}

impl Display for ParseManyResource {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::DocumentBuffer => "parse-many document buffer",
            Self::Events => "parse-many event buffer",
            Self::DecodedStrings => "decoded JSON string buffer",
            Self::ParserStack => "JSON parser stack",
        })
    }
}

/// Fatal parse-many framing, parsing, input, accounting, or allocation error.
#[derive(Debug)]
#[non_exhaustive]
pub enum ParseManyError {
    /// Reading the caller-owned input failed.
    Input(io::Error),
    /// The next object was malformed or exceeded a configured limit.
    InvalidDocument(ParseManyInvalidDocument),
    /// A bounded reusable buffer could not reserve more storage.
    AllocationFailed {
        /// Buffer being grown.
        resource: ParseManyResource,
        /// Additional logical bytes or entries requested.
        requested_additional: usize,
    },
    /// A stream counter could not be represented as `u64`.
    SizeOverflow,
    /// A previous input/framing/parsing failure permanently stopped this reader.
    Stopped,
}

impl Display for ParseManyError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Input(source) => write!(formatter, "failed to read parse-many input: {source}"),
            Self::InvalidDocument(source) => Display::fmt(source, formatter),
            Self::AllocationFailed {
                resource,
                requested_additional,
            } => write!(
                formatter,
                "failed to reserve {requested_additional} additional units for {resource}"
            ),
            Self::SizeOverflow => formatter.write_str("parse-many stream size counter overflow"),
            Self::Stopped => {
                formatter.write_str("parse-many reader stopped after an earlier error")
            }
        }
    }
}

impl Error for ParseManyError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Input(source) => Some(source),
            Self::InvalidDocument(source) => Some(source),
            Self::AllocationFailed { .. } | Self::SizeOverflow | Self::Stopped => None,
        }
    }
}

/// Reader or caller-owned sink failure from [`ParseManyReader`].
#[derive(Debug)]
#[non_exhaustive]
pub enum ParseManyReadError<E> {
    /// Framing, parsing, input, or allocation failed before a sink call completed.
    Reader(ParseManyError),
    /// The sink rejected a valid borrowed object.
    Sink {
        /// Zero-based input offset of the object.
        input_offset: u64,
        /// Zero-based successful-document index presented to the sink.
        document_index: u64,
        /// Caller-owned sink error.
        source: E,
    },
}

impl<E: Display> Display for ParseManyReadError<E> {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Reader(source) => Display::fmt(source, formatter),
            Self::Sink {
                input_offset,
                document_index,
                source,
            } => write!(
                formatter,
                "parse-many sink failed for document {document_index} at input byte \
                 {input_offset}: {source}"
            ),
        }
    }
}

impl<E: Error + 'static> Error for ParseManyReadError<E> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Reader(source) => Some(source),
            Self::Sink { source, .. } => Some(source),
        }
    }
}

/// One complete JSON root object borrowed for a sink call.
#[derive(Clone, Copy, Debug)]
pub struct ParseManyDocument<'a> {
    json: &'a [u8],
    decoded: &'a str,
    events: &'a [StoredEvent],
    input_offset: u64,
    document_index: u64,
}

impl<'a> ParseManyDocument<'a> {
    /// Returns exact source bytes from the opening through closing root brace.
    #[must_use]
    pub const fn json_bytes(self) -> &'a [u8] {
        self.json
    }

    /// Returns the document's balanced, depth-first flat events.
    #[must_use]
    pub fn events(self) -> JsonEvents<'a> {
        JsonEvents::new(self.json, self.decoded, self.events)
    }

    /// Returns the zero-based input offset of the root opening brace.
    #[must_use]
    pub const fn input_offset(self) -> u64 {
        self.input_offset
    }

    /// Returns the zero-based index among successfully accepted sink documents.
    #[must_use]
    pub const fn document_index(self) -> u64 {
        self.document_index
    }
}

/// Synchronous destination for complete borrowed parse-many objects.
pub trait ParseManyDocumentSink {
    /// Caller-selected sink error.
    type Error;

    /// Accepts one fully framed and validated borrowed object.
    ///
    /// # Errors
    ///
    /// Returns the caller's error when the object cannot be accepted.
    fn write_document(&mut self, document: ParseManyDocument<'_>) -> Result<(), Self::Error>;
}

impl<F, E> ParseManyDocumentSink for F
where
    F: for<'document> FnMut(ParseManyDocument<'document>) -> Result<(), E>,
{
    type Error = E;

    fn write_document(&mut self, document: ParseManyDocument<'_>) -> Result<(), Self::Error> {
        self(document)
    }
}

/// Successful parse-many stream counters.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ParseManyStats {
    input_bytes: u64,
    separator_bytes: u64,
    documents: u64,
    truncated_bytes: u64,
}

impl ParseManyStats {
    /// Returns bytes logically consumed from the input.
    #[must_use]
    pub const fn input_bytes(self) -> u64 {
        self.input_bytes
    }

    /// Returns JSON whitespace consumed before, between, or after objects.
    #[must_use]
    pub const fn separator_bytes(self) -> u64 {
        self.separator_bytes
    }

    /// Returns valid objects whose sink call completed successfully.
    #[must_use]
    pub const fn documents(self) -> u64 {
        self.documents
    }

    /// Returns bytes in an ignored incomplete final object, or zero in strict mode.
    #[must_use]
    pub const fn truncated_bytes(self) -> u64 {
        self.truncated_bytes
    }
}

/// Bounded streaming reader for the C++ CLP-S parse-many object framing contract.
///
/// Root objects may span physical lines and may be directly adjacent (`}{`) with no separator.
/// Outside objects, only JSON whitespace is accepted. Strict defaults report an incomplete suffix
/// at EOF as invalid; [`IncompleteDocumentPolicy::Ignore`] provides the pinned C++ CLI behavior.
/// Arrays and scalars are rejected as roots, matching the C++ ingestion layer rather than
/// simdjson's lower-level document stream.
pub struct ParseManyReader<R> {
    input: R,
    options: ParseManyOptions,
    input_buffer: [u8; INPUT_BUFFER_BYTES],
    input_start: usize,
    input_end: usize,
    reached_eof: bool,
    stopped: bool,
    document: Vec<u8>,
    decoded: String,
    events: Vec<StoredEvent>,
    parser_stack: Vec<Frame>,
    stats: ParseManyStats,
}

enum PreparedDocument {
    InputBuffer {
        input_offset: u64,
        start: usize,
        end: usize,
    },
    Owned {
        input_offset: u64,
    },
}

impl<R: Read> ParseManyReader<R> {
    /// Creates a reader over a caller-owned byte stream.
    #[must_use]
    pub fn new(input: R, options: ParseManyOptions) -> Self {
        Self {
            input,
            options,
            input_buffer: [0; INPUT_BUFFER_BYTES],
            input_start: 0,
            input_end: 0,
            reached_eof: false,
            stopped: false,
            document: Vec::new(),
            decoded: String::new(),
            events: Vec::new(),
            parser_stack: Vec::new(),
            stats: ParseManyStats::default(),
        }
    }

    /// Returns the immutable reader configuration.
    #[must_use]
    pub const fn options(&self) -> ParseManyOptions {
        self.options
    }

    /// Returns counters accumulated so far.
    #[must_use]
    pub const fn stats(&self) -> ParseManyStats {
        self.stats
    }

    /// Returns the underlying input, discarding unread buffered bytes.
    #[must_use]
    pub fn into_inner(self) -> R {
        self.input
    }

    /// Reads and validates the next object, then invokes `sink` exactly once.
    ///
    /// Returns `Ok(true)` after a successful sink call and `Ok(false)` after trailing whitespace
    /// at end of input.
    ///
    /// # Errors
    ///
    /// Returns the first reader or sink failure. Input/framing/parser failures permanently stop
    /// the reader because parse-many has no safe resynchronization boundary.
    pub fn read_document<S: ParseManyDocumentSink + ?Sized>(
        &mut self,
        sink: &mut S,
    ) -> Result<bool, ParseManyReadError<S::Error>> {
        let Some(prepared) = self
            .prepare_document()
            .map_err(ParseManyReadError::Reader)?
        else {
            return Ok(false);
        };
        let (input_offset, json) = match prepared {
            PreparedDocument::InputBuffer {
                input_offset,
                start,
                end,
            } => (input_offset, &self.input_buffer[start..end]),
            PreparedDocument::Owned { input_offset } => (input_offset, self.document.as_slice()),
        };
        let document_index = self.stats.documents;
        let document = ParseManyDocument {
            json,
            decoded: &self.decoded,
            events: &self.events,
            input_offset,
            document_index,
        };
        sink.write_document(document)
            .map_err(|source| ParseManyReadError::Sink {
                input_offset,
                document_index,
                source,
            })?;
        self.stats.documents = self
            .stats
            .documents
            .checked_add(1)
            .ok_or(ParseManyReadError::Reader(ParseManyError::SizeOverflow))?;
        Ok(true)
    }

    /// Drains every object into a caller-owned sink.
    ///
    /// # Errors
    ///
    /// Returns the first reader or sink failure. Statistics count only successful sink calls.
    pub fn read_to_end<S: ParseManyDocumentSink + ?Sized>(
        &mut self,
        sink: &mut S,
    ) -> Result<ParseManyStats, ParseManyReadError<S::Error>> {
        while self.read_document(sink)? {}
        Ok(self.stats)
    }

    #[cfg(test)]
    pub(super) const fn buffer_capacities(&self) -> (usize, usize, usize, usize) {
        (
            self.document.capacity(),
            self.decoded.capacity(),
            self.events.capacity(),
            self.parser_stack.capacity(),
        )
    }

    fn prepare_document(&mut self) -> Result<Option<PreparedDocument>, ParseManyError> {
        if self.stopped {
            return Err(ParseManyError::Stopped);
        }
        let result = self.try_prepare_document();
        if result.is_err() {
            self.stopped = true;
        }
        result
    }

    fn try_prepare_document(&mut self) -> Result<Option<PreparedDocument>, ParseManyError> {
        self.skip_separators()?;
        let input_offset = self.stats.input_bytes;
        let Some(first) = self.peek_byte()? else {
            return Ok(None);
        };
        if first != b'{' {
            return Err(self.invalid(
                input_offset,
                ParseManyInvalidDocumentKind::Syntax(JsonSyntaxError::new(
                    0,
                    JsonSyntaxErrorKind::ExpectedObject,
                )),
            ));
        }

        if let Some((start, end)) = self.try_prepare_input_document()? {
            return Ok(Some(PreparedDocument::InputBuffer {
                input_offset,
                start,
                end,
            }));
        }

        if !self.frame_object(input_offset)? {
            return Ok(None);
        }
        match self.parse_buffered_document() {
            Ok(()) => Ok(Some(PreparedDocument::Owned { input_offset })),
            Err(ParseFailure::Invalid(kind)) => {
                Err(self.invalid(input_offset, map_invalid_kind(kind)))
            }
            Err(ParseFailure::AllocationFailed {
                resource,
                requested_additional,
            }) => Err(ParseManyError::AllocationFailed {
                resource: map_parse_resource(resource),
                requested_additional,
            }),
            Err(ParseFailure::SizeOverflow) => Err(ParseManyError::SizeOverflow),
        }
    }

    fn try_prepare_input_document(&mut self) -> Result<Option<(usize, usize)>, ParseManyError> {
        let start = self.input_start;
        let available_bytes = self
            .input_end
            .checked_sub(start)
            .ok_or(ParseManyError::SizeOverflow)?;
        let document_limit =
            usize::try_from(self.options.limits.document_bytes).unwrap_or(usize::MAX);
        let speculative_bytes = available_bytes.min(document_limit);
        let speculative_end = start
            .checked_add(speculative_bytes)
            .ok_or(ParseManyError::SizeOverflow)?;

        self.decoded.clear();
        self.events.clear();
        self.parser_stack.clear();
        match parse_document_prefix(
            &self.input_buffer[start..speculative_end],
            self.options.limits.parser_limits(),
            &mut self.decoded,
            &mut self.events,
            &mut self.parser_stack,
        ) {
            Ok(document_bytes) => {
                let end = start
                    .checked_add(document_bytes)
                    .ok_or(ParseManyError::SizeOverflow)?;
                self.consume_input(document_bytes)?;
                Ok(Some((start, end)))
            }
            Err(ParseFailure::Invalid(_)) => Ok(None),
            Err(ParseFailure::AllocationFailed {
                resource,
                requested_additional,
            }) => Err(ParseManyError::AllocationFailed {
                resource: map_parse_resource(resource),
                requested_additional,
            }),
            Err(ParseFailure::SizeOverflow) => Err(ParseManyError::SizeOverflow),
        }
    }

    fn parse_buffered_document(&mut self) -> Result<(), ParseFailure> {
        self.decoded.clear();
        self.events.clear();
        self.parser_stack.clear();
        parse_document(
            &self.document,
            self.options.limits.parser_limits(),
            &mut self.decoded,
            &mut self.events,
            &mut self.parser_stack,
        )
    }

    fn skip_separators(&mut self) -> Result<(), ParseManyError> {
        loop {
            if !self.fill_input()? {
                return Ok(());
            }
            let separator_bytes = self.input_buffer[self.input_start..self.input_end]
                .iter()
                .take_while(|byte| is_json_whitespace(**byte))
                .count();
            if 0 == separator_bytes {
                return Ok(());
            }
            self.consume_input(separator_bytes)?;
            let separator_bytes_u64 =
                u64::try_from(separator_bytes).map_err(|_| ParseManyError::SizeOverflow)?;
            self.stats.separator_bytes = self
                .stats
                .separator_bytes
                .checked_add(separator_bytes_u64)
                .ok_or(ParseManyError::SizeOverflow)?;
        }
    }

    fn frame_object(&mut self, input_offset: u64) -> Result<bool, ParseManyError> {
        self.document.clear();
        let mut document_bytes = 0_u64;
        let mut state = ObjectScanState::default();

        loop {
            let remaining = self
                .options
                .limits
                .document_bytes
                .checked_sub(document_bytes)
                .ok_or(ParseManyError::SizeOverflow)?;
            if 0 == remaining {
                if self.peek_byte()?.is_none() {
                    return self.handle_incomplete_document(input_offset);
                }
                self.consume_input(1)?;
                let actual = document_bytes
                    .checked_add(1)
                    .ok_or(ParseManyError::SizeOverflow)?;
                return Err(self.document_limit(input_offset, actual));
            }
            if !self.fill_input()? {
                return self.handle_incomplete_document(input_offset);
            }

            let available_bytes = self
                .input_end
                .checked_sub(self.input_start)
                .ok_or(ParseManyError::SizeOverflow)?;
            let scan_bytes = usize::try_from(remaining)
                .unwrap_or(usize::MAX)
                .min(available_bytes);
            let scan_end = self
                .input_start
                .checked_add(scan_bytes)
                .ok_or(ParseManyError::SizeOverflow)?;
            let scan = scan_object_chunk(
                &self.input_buffer[self.input_start..scan_end],
                self.document.len(),
                &mut state,
            );
            self.document.try_reserve(scan.consumed).map_err(|_| {
                ParseManyError::AllocationFailed {
                    resource: ParseManyResource::DocumentBuffer,
                    requested_additional: scan.consumed,
                }
            })?;
            let end = self
                .input_start
                .checked_add(scan.consumed)
                .ok_or(ParseManyError::SizeOverflow)?;
            self.document
                .extend_from_slice(&self.input_buffer[self.input_start..end]);
            self.consume_input(scan.consumed)?;
            let consumed_u64 =
                u64::try_from(scan.consumed).map_err(|_| ParseManyError::SizeOverflow)?;
            document_bytes = document_bytes
                .checked_add(consumed_u64)
                .ok_or(ParseManyError::SizeOverflow)?;

            match scan.outcome {
                ObjectScanOutcome::Continue => {}
                ObjectScanOutcome::Complete => return Ok(true),
                ObjectScanOutcome::Invalid(source) => {
                    return Err(
                        self.invalid(input_offset, ParseManyInvalidDocumentKind::Syntax(source))
                    );
                }
                ObjectScanOutcome::SizeOverflow => return Err(ParseManyError::SizeOverflow),
            }
        }
    }

    fn handle_incomplete_document(&mut self, input_offset: u64) -> Result<bool, ParseManyError> {
        if IncompleteDocumentPolicy::Error == self.options.incomplete_document {
            return Err(self.unexpected_end(input_offset));
        }
        self.stats.truncated_bytes =
            u64::try_from(self.document.len()).map_err(|_| ParseManyError::SizeOverflow)?;
        Ok(false)
    }

    fn peek_byte(&mut self) -> Result<Option<u8>, ParseManyError> {
        if self.fill_input()? {
            Ok(self.input_buffer.get(self.input_start).copied())
        } else {
            Ok(None)
        }
    }

    fn fill_input(&mut self) -> Result<bool, ParseManyError> {
        loop {
            if self.input_start < self.input_end {
                return Ok(true);
            }
            if self.reached_eof {
                return Ok(false);
            }
            match self.input.read(&mut self.input_buffer) {
                Ok(0) => self.reached_eof = true,
                Ok(read) => {
                    self.input_start = 0;
                    self.input_end = read;
                }
                Err(source) if source.kind() == io::ErrorKind::Interrupted => {}
                Err(source) => return Err(ParseManyError::Input(source)),
            }
        }
    }

    fn consume_input(&mut self, bytes: usize) -> Result<(), ParseManyError> {
        let available = self
            .input_end
            .checked_sub(self.input_start)
            .ok_or(ParseManyError::SizeOverflow)?;
        if bytes > available {
            return Err(ParseManyError::SizeOverflow);
        }
        self.input_start = self
            .input_start
            .checked_add(bytes)
            .ok_or(ParseManyError::SizeOverflow)?;
        let bytes_u64 = u64::try_from(bytes).map_err(|_| ParseManyError::SizeOverflow)?;
        self.stats.input_bytes = self
            .stats
            .input_bytes
            .checked_add(bytes_u64)
            .ok_or(ParseManyError::SizeOverflow)?;
        Ok(())
    }

    const fn unexpected_end(&self, input_offset: u64) -> ParseManyError {
        self.invalid(
            input_offset,
            ParseManyInvalidDocumentKind::Syntax(JsonSyntaxError::new(
                self.document.len(),
                JsonSyntaxErrorKind::UnexpectedEnd,
            )),
        )
    }

    const fn document_limit(&self, input_offset: u64, actual: u64) -> ParseManyError {
        self.invalid(
            input_offset,
            ParseManyInvalidDocumentKind::Limit(ParseManyLimitViolation::new(
                ParseManyLimitResource::DocumentBytes,
                actual,
                self.options.limits.document_bytes,
            )),
        )
    }

    const fn invalid(
        &self,
        input_offset: u64,
        kind: ParseManyInvalidDocumentKind,
    ) -> ParseManyError {
        ParseManyError::InvalidDocument(ParseManyInvalidDocument::new(
            self.stats.documents,
            input_offset,
            kind,
        ))
    }
}

#[derive(Default)]
struct ObjectScanState {
    object_depth: u64,
    in_string: bool,
    escaped: bool,
}

struct ObjectScan {
    consumed: usize,
    outcome: ObjectScanOutcome,
}

enum ObjectScanOutcome {
    Continue,
    Complete,
    Invalid(JsonSyntaxError),
    SizeOverflow,
}

fn scan_object_chunk(bytes: &[u8], base_offset: usize, state: &mut ObjectScanState) -> ObjectScan {
    for (index, byte) in bytes.iter().copied().enumerate() {
        let Some(consumed) = index.checked_add(1) else {
            return ObjectScan {
                consumed: index,
                outcome: ObjectScanOutcome::SizeOverflow,
            };
        };
        let Some(offset) = base_offset.checked_add(index) else {
            return ObjectScan {
                consumed,
                outcome: ObjectScanOutcome::SizeOverflow,
            };
        };

        if state.in_string {
            if state.escaped {
                if byte < 0x20 {
                    return ObjectScan {
                        consumed,
                        outcome: ObjectScanOutcome::Invalid(JsonSyntaxError::new(
                            offset.saturating_sub(1),
                            JsonSyntaxErrorKind::InvalidEscape,
                        )),
                    };
                }
                state.escaped = false;
            } else {
                match byte {
                    b'\\' => state.escaped = true,
                    b'"' => state.in_string = false,
                    0x00..=0x1f => {
                        return ObjectScan {
                            consumed,
                            outcome: ObjectScanOutcome::Invalid(JsonSyntaxError::new(
                                offset,
                                JsonSyntaxErrorKind::UnescapedControlCharacter,
                            )),
                        };
                    }
                    _ => {}
                }
            }
            continue;
        }

        match byte {
            b'"' => state.in_string = true,
            b'{' => {
                let Some(depth) = state.object_depth.checked_add(1) else {
                    return ObjectScan {
                        consumed,
                        outcome: ObjectScanOutcome::SizeOverflow,
                    };
                };
                state.object_depth = depth;
            }
            b'}' => {
                let Some(depth) = state.object_depth.checked_sub(1) else {
                    return ObjectScan {
                        consumed,
                        outcome: ObjectScanOutcome::Invalid(JsonSyntaxError::new(
                            offset,
                            JsonSyntaxErrorKind::ExpectedObject,
                        )),
                    };
                };
                state.object_depth = depth;
                if 0 == depth {
                    return ObjectScan {
                        consumed,
                        outcome: ObjectScanOutcome::Complete,
                    };
                }
            }
            _ => {}
        }
    }

    ObjectScan {
        consumed: bytes.len(),
        outcome: ObjectScanOutcome::Continue,
    }
}

const fn map_invalid_kind(kind: NdjsonInvalidRecordKind) -> ParseManyInvalidDocumentKind {
    match kind {
        NdjsonInvalidRecordKind::Syntax(source) => ParseManyInvalidDocumentKind::Syntax(source),
        NdjsonInvalidRecordKind::Limit(source) => {
            let resource = match source.resource() {
                NdjsonLimitResource::RecordBytes => ParseManyLimitResource::DocumentBytes,
                NdjsonLimitResource::NestingDepth => ParseManyLimitResource::NestingDepth,
                NdjsonLimitResource::Values => ParseManyLimitResource::Values,
                NdjsonLimitResource::ScalarTokenBytes => ParseManyLimitResource::ScalarTokenBytes,
            };
            ParseManyInvalidDocumentKind::Limit(ParseManyLimitViolation::new(
                resource,
                source.actual(),
                source.limit(),
            ))
        }
    }
}

const fn map_parse_resource(resource: super::ndjson::NdjsonResource) -> ParseManyResource {
    match resource {
        super::ndjson::NdjsonResource::RecordBuffer => ParseManyResource::DocumentBuffer,
        super::ndjson::NdjsonResource::Events => ParseManyResource::Events,
        super::ndjson::NdjsonResource::DecodedStrings => ParseManyResource::DecodedStrings,
        super::ndjson::NdjsonResource::ParserStack => ParseManyResource::ParserStack,
    }
}

const fn is_json_whitespace(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r')
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;

    use super::super::ndjson::JsonEvent;
    use super::super::ndjson::JsonSyntaxErrorKind;
    use super::INPUT_BUFFER_BYTES;
    use super::ParseManyDocument;
    use super::ParseManyDocumentSink;
    use super::ParseManyError;
    use super::ParseManyInvalidDocumentKind;
    use super::ParseManyLimitResource;
    use super::ParseManyLimits;
    use super::ParseManyOptions;
    use super::ParseManyReadError;
    use super::ParseManyReader;

    #[derive(Default)]
    struct BorrowCaptureSink {
        json_pointers: Vec<*const u8>,
        documents: Vec<Vec<u8>>,
        decoded_strings: Vec<String>,
        input_offsets: Vec<u64>,
    }

    impl ParseManyDocumentSink for BorrowCaptureSink {
        type Error = Infallible;

        fn write_document(&mut self, document: ParseManyDocument<'_>) -> Result<(), Self::Error> {
            self.json_pointers.push(document.json_bytes().as_ptr());
            self.documents.push(document.json_bytes().to_vec());
            self.input_offsets.push(document.input_offset());
            self.decoded_strings
                .extend(document.events().filter_map(|event| match event {
                    JsonEvent::String(value) => Some(value.decoded().to_owned()),
                    _ => None,
                }));
            Ok(())
        }
    }

    #[test]
    fn in_buffer_documents_borrow_the_reused_input_buffer_across_refills() {
        let first = br#"{"value":"first\u0020value"}"#;
        let second = br#"{"value":"second\u0020value"}"#;
        let mut input = Vec::with_capacity(INPUT_BUFFER_BYTES + second.len() + 1);
        input.extend_from_slice(first);
        input.push(b'\n');
        input.resize(INPUT_BUFFER_BYTES, b' ');
        input.extend_from_slice(second);
        input.push(b'\n');

        let mut reader = ParseManyReader::new(input.as_slice(), ParseManyOptions::default());
        let input_buffer_pointer = reader.input_buffer.as_ptr();
        let mut sink = BorrowCaptureSink::default();

        assert!(reader.read_document(&mut sink).unwrap());
        assert_eq!(input_buffer_pointer, sink.json_pointers[0]);
        assert_eq!(0, reader.document.capacity());

        assert!(reader.read_document(&mut sink).unwrap());
        assert_eq!(input_buffer_pointer, sink.json_pointers[1]);
        assert_eq!(0, reader.document.capacity());
        assert!(!reader.read_document(&mut sink).unwrap());

        assert_eq!(vec![first.to_vec(), second.to_vec()], sink.documents);
        assert_eq!(vec!["first value", "second value"], sink.decoded_strings);
        assert_eq!(2, reader.stats().documents());
        assert_eq!(
            u64::try_from(input.len()).unwrap(),
            reader.stats().input_bytes()
        );
    }

    #[test]
    fn in_buffer_multiline_document_is_zero_copy() {
        let input = br#"{
            "value":"multiline\u0020value"
        }
        "#;
        let mut reader = ParseManyReader::new(input.as_slice(), ParseManyOptions::default());
        let input_buffer_pointer = reader.input_buffer.as_ptr();
        let mut sink = BorrowCaptureSink::default();

        assert!(reader.read_document(&mut sink).unwrap());
        assert_eq!(input_buffer_pointer, sink.json_pointers[0]);
        assert_eq!(0, reader.document.capacity());
        assert_eq!(vec!["multiline value"], sink.decoded_strings);
        assert!(!reader.read_document(&mut sink).unwrap());
    }

    #[test]
    fn directly_adjacent_in_buffer_documents_are_each_zero_copy() {
        let first = br#"{"id":1}"#;
        let second = br#"{"id":2}"#;
        let mut input = first.to_vec();
        input.extend_from_slice(second);
        let mut reader = ParseManyReader::new(input.as_slice(), ParseManyOptions::default());
        let input_buffer_pointer = reader.input_buffer.as_ptr();
        let mut sink = BorrowCaptureSink::default();

        assert!(reader.read_document(&mut sink).unwrap());
        assert!(reader.read_document(&mut sink).unwrap());
        assert!(!reader.read_document(&mut sink).unwrap());

        assert_eq!(input_buffer_pointer, sink.json_pointers[0]);
        assert_eq!(
            input_buffer_pointer.wrapping_add(first.len()),
            sink.json_pointers[1]
        );
        assert_eq!(0, reader.document.capacity());
        assert_eq!(vec![first.to_vec(), second.to_vec()], sink.documents);
        assert_eq!(
            vec![0, u64::try_from(first.len()).unwrap()],
            sink.input_offsets
        );
    }

    #[test]
    fn cross_buffer_document_keeps_the_owned_fallback() {
        let mut input = br#"{"value":""#.to_vec();
        input.extend(std::iter::repeat_n(b'x', INPUT_BUFFER_BYTES));
        input.extend_from_slice(br#""}"#);
        let mut reader = ParseManyReader::new(input.as_slice(), ParseManyOptions::default());
        let input_buffer_pointer = reader.input_buffer.as_ptr();
        let mut sink = BorrowCaptureSink::default();

        assert!(reader.read_document(&mut sink).unwrap());
        assert_ne!(input_buffer_pointer, sink.json_pointers[0]);
        assert_eq!(reader.document.as_ptr(), sink.json_pointers[0]);
        assert_ne!(0, reader.document.capacity());
        assert_eq!(1, sink.documents.len());
        assert_eq!(input.as_slice(), sink.documents[0].as_slice());
        assert!(!reader.read_document(&mut sink).unwrap());
    }

    #[test]
    fn malformed_in_buffer_prefix_falls_back_to_the_exact_document_error() {
        let input = br#"{"bad":}"#;
        let mut reader = ParseManyReader::new(input.as_slice(), ParseManyOptions::default());
        let mut sink = BorrowCaptureSink::default();

        let error = reader
            .read_document(&mut sink)
            .expect_err("the object value is missing");
        let ParseManyReadError::Reader(ParseManyError::InvalidDocument(invalid)) = error else {
            panic!("unexpected malformed-document failure: {error:?}");
        };
        let ParseManyInvalidDocumentKind::Syntax(source) = invalid.kind() else {
            panic!("unexpected malformed-document kind: {:?}", invalid.kind());
        };
        assert_eq!(JsonSyntaxErrorKind::ExpectedValue, source.kind());
        assert_eq!(7, source.byte_offset());
        assert_ne!(0, reader.document.capacity());
        assert_eq!(sink.documents, Vec::<Vec<u8>>::new());
    }

    #[test]
    fn document_byte_limit_keeps_precedence_over_speculative_parser_limits() {
        let input = br#"{"a":[[[0]]]}"#;
        let limits = ParseManyLimits::new(6, 1, 2, 2);
        let mut reader = ParseManyReader::new(
            input.as_slice(),
            ParseManyOptions::new().with_limits(limits),
        );
        let mut sink = BorrowCaptureSink::default();

        let error = reader
            .read_document(&mut sink)
            .expect_err("document bytes must be checked before parser limits");
        let ParseManyReadError::Reader(ParseManyError::InvalidDocument(invalid)) = error else {
            panic!("unexpected document-limit failure: {error:?}");
        };
        let ParseManyInvalidDocumentKind::Limit(source) = invalid.kind() else {
            panic!("unexpected document-limit kind: {:?}", invalid.kind());
        };
        assert_eq!(ParseManyLimitResource::DocumentBytes, source.resource());
        assert_eq!(7, source.actual());
        assert_eq!(6, source.limit());
        assert_eq!(sink.documents, Vec::<Vec<u8>>::new());
    }

    #[test]
    fn overlimit_speculation_does_not_decode_bytes_beyond_the_document_limit() {
        let mut input = br#"{"a":""#.to_vec();
        for _ in 0..1_000 {
            input.extend_from_slice(br"\n");
        }
        input.extend_from_slice(br#""}"#);
        let limits = ParseManyLimits::new(
            6,
            256,
            1_000_000,
            ParseManyLimits::DEFAULT.max_scalar_token_bytes(),
        );
        let mut reader = ParseManyReader::new(
            input.as_slice(),
            ParseManyOptions::new().with_limits(limits),
        );
        let mut sink = BorrowCaptureSink::default();

        let error = reader
            .read_document(&mut sink)
            .expect_err("the seventh document byte exceeds the configured limit");
        let ParseManyReadError::Reader(ParseManyError::InvalidDocument(invalid)) = error else {
            panic!("unexpected document-limit failure: {error:?}");
        };
        let ParseManyInvalidDocumentKind::Limit(source) = invalid.kind() else {
            panic!("unexpected document-limit kind: {:?}", invalid.kind());
        };
        assert_eq!(ParseManyLimitResource::DocumentBytes, source.resource());
        assert_eq!(7, source.actual());
        assert_eq!(6, source.limit());
        assert_eq!(0, reader.decoded.capacity());
        assert_eq!(sink.documents, Vec::<Vec<u8>>::new());
    }
}
