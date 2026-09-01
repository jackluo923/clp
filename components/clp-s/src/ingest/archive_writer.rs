//! Zero-copy adapters from validated JSON traversals into structured archive writers.

use std::error::Error;
use std::fmt;
use std::fmt::Display;
use std::fmt::Formatter;

use bumpalo::Bump;
use bumpalo::collections::Vec as BumpVec;

use super::ClassifiedJsonNumber;
use super::JsonEvents;
use super::JsonNumberClassificationError;
use super::JsonTimestampError;
use super::JsonTimestampResolver;
use super::NdjsonRecord;
use super::NdjsonRecordSink;
use super::ParseManyDocument;
use super::ParseManyDocumentSink;
use super::number::ValidatedJsonNumberSyntax;
use super::number::classify_validated_json_number;
use super::parser::StoredEvent;
use crate::writer::ArchiveSetAppendError;
use crate::writer::ArchiveSetError;
use crate::writer::ArchiveSetStatsCallback;
use crate::writer::ArchiveSetWriter;
use crate::writer::ArchiveSourceContext;
use crate::writer::FieldRef;
use crate::writer::FinalizedArchiveSink;
use crate::writer::PrevalidatedTimestampRef;
use crate::writer::RecordEventAppendError;
use crate::writer::RecordEventAppender;
use crate::writer::RecordEventConsumer;
use crate::writer::RecordEventRef;
use crate::writer::ReplayableRecordEventSource;
use crate::writer::RetainedFloatRef;
use crate::writer::UnstructuredArrayRef;
use crate::writer::ValueRef;

/// Hard limits for the temporary borrowed tree used when structurizing JSON arrays.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JsonStructuredArrayLimits {
    values: u64,
    nesting_depth: u64,
}

impl JsonStructuredArrayLimits {
    /// Defaults aligned with the bundled streaming JSON readers.
    pub const DEFAULT: Self = Self::new(1_000_000, 256);

    /// Creates limits for one JSON record.
    ///
    /// `max_values` counts every array, object, and scalar inside structured arrays. Object keys
    /// are excluded. `max_nesting_depth` counts the root array and every container nested in it.
    #[must_use]
    pub const fn new(max_values: u64, max_nesting_depth: u64) -> Self {
        Self {
            values: max_values,
            nesting_depth: max_nesting_depth,
        }
    }

    /// Maximum JSON values materialized across structured arrays in one record.
    #[must_use]
    pub const fn max_values(self) -> u64 {
        self.values
    }

    /// Maximum open array/object containers within one structured array.
    #[must_use]
    pub const fn max_nesting_depth(self) -> u64 {
        self.nesting_depth
    }
}

impl Default for JsonStructuredArrayLimits {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// JSON-to-archive value-conversion options.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JsonArchiveOptions {
    retain_float_format: bool,
    structurize_arrays: bool,
    structured_array_limits: JsonStructuredArrayLimits,
}

impl JsonArchiveOptions {
    /// Creates options matching the current C++ compressor defaults.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            retain_float_format: true,
            structurize_arrays: false,
            structured_array_limits: JsonStructuredArrayLimits::DEFAULT,
        }
    }

    /// Selects whether exact floating-point source spelling is retained.
    #[must_use]
    pub const fn with_retain_float_format(mut self, retain: bool) -> Self {
        self.retain_float_format = retain;
        self
    }

    /// Returns whether exact floating-point source spelling is retained.
    #[must_use]
    pub const fn retains_float_format(self) -> bool {
        self.retain_float_format
    }

    /// Selects whether arrays are encoded as searchable structured arrays.
    ///
    /// The default is `false`, preserving each exact array source lexeme in an unstructured-array
    /// column. Enabling this matches the C++ `--structurize-arrays` representation.
    #[must_use]
    pub const fn with_structurize_arrays(mut self, structurize: bool) -> Self {
        self.structurize_arrays = structurize;
        self
    }

    /// Replaces the per-record limits for structured-array staging.
    #[must_use]
    pub const fn with_structured_array_limits(mut self, limits: JsonStructuredArrayLimits) -> Self {
        self.structured_array_limits = limits;
        self
    }

    /// Returns whether arrays are encoded as searchable structured arrays.
    #[must_use]
    pub const fn structurizes_arrays(self) -> bool {
        self.structurize_arrays
    }

    /// Returns the per-record limits for structured-array staging.
    #[must_use]
    pub const fn structured_array_limits(self) -> JsonStructuredArrayLimits {
        self.structured_array_limits
    }
}

impl Default for JsonArchiveOptions {
    fn default() -> Self {
        Self::new()
    }
}

/// Internal traversal invariant rejected while adapting validated JSON events.
///
/// The bundled readers never produce these shapes; the variants make adapter failures structured
/// rather than panicking if its event contract changes in a later parser revision.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum JsonRecordTraversalError {
    /// The traversal did not begin with a root object.
    ExpectedRootObject,
    /// A key appeared outside an open object or before the preceding key received a value.
    UnexpectedObjectKey,
    /// An object field value appeared without a preceding key.
    MissingObjectKey,
    /// An object ended while its final key still lacked a value.
    DanglingObjectKey,
    /// An object end had no matching object start.
    UnexpectedObjectEnd,
    /// An array end was exposed outside the adapter's array-collapsing step.
    UnexpectedArrayEnd,
    /// The event sequence continued after the root object ended.
    TrailingRootEvent,
    /// The event sequence ended with an open object or array.
    UnexpectedEnd,
    /// Counting parser events overflowed the platform index domain.
    EventIndexOverflow,
}

impl Display for JsonRecordTraversalError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::ExpectedRootObject => "expected a root object start",
            Self::UnexpectedObjectKey => "unexpected object key",
            Self::MissingObjectKey => "field value has no preceding object key",
            Self::DanglingObjectKey => "object ended with a key that has no value",
            Self::UnexpectedObjectEnd => "object end has no matching object start",
            Self::UnexpectedArrayEnd => "array end has no exposed array start",
            Self::TrailingRootEvent => "event follows the complete root object",
            Self::UnexpectedEnd => "event traversal ended before its container closed",
            Self::EventIndexOverflow => "JSON event index overflow",
        })
    }
}

impl Error for JsonRecordTraversalError {}

/// Structured-array staging resource guarded by a configurable per-record limit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum JsonStructuredArrayLimitResource {
    /// Arrays, objects, and scalars inside structured arrays.
    Values,
    /// Simultaneously open arrays and objects, including the root array.
    NestingDepth,
}

impl Display for JsonStructuredArrayLimitResource {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Values => "structured-array JSON values",
            Self::NestingDepth => "structured-array nesting depth",
        })
    }
}

/// Bounded allocation used while adapting a structured JSON array.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum JsonStructuredArrayResource {
    /// Iterative array/object traversal frames.
    TraversalStack,
    /// Values staged for an array.
    ArrayValues,
    /// Fields staged for an object.
    ObjectFields,
}

impl Display for JsonStructuredArrayResource {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::TraversalStack => "structured-array traversal stack",
            Self::ArrayValues => "structured-array values",
            Self::ObjectFields => "structured-array object fields",
        })
    }
}

/// Failure to convert one validated JSON event into a writer-native record event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum JsonRecordEventError {
    /// An exact number token exceeded the current C++ compressor's numeric domain.
    Number {
        /// Zero-based index in the original JSON traversal.
        json_event_index: usize,
        /// Number grammar or range failure.
        source: JsonNumberClassificationError,
    },
    /// A scalar at the configured authoritative path was not a supported exact timestamp.
    Timestamp {
        /// Zero-based index in the original JSON traversal.
        json_event_index: usize,
        /// Timestamp recognition or epoch-range failure.
        source: JsonTimestampError,
    },
    /// The validated parser traversal violated the adapter contract.
    Traversal {
        /// Zero-based index in the original JSON traversal.
        json_event_index: usize,
        /// Broken traversal invariant.
        reason: JsonRecordTraversalError,
    },
    /// Structured-array staging exceeded a configured per-record limit.
    StructuredArrayLimit {
        /// Zero-based index in the original JSON traversal.
        json_event_index: usize,
        /// Resource whose exact count exceeded its bound.
        resource: JsonStructuredArrayLimitResource,
        /// Observed count.
        actual: u64,
        /// Configured maximum.
        limit: u64,
    },
    /// A bounded structured-array scratch allocation failed.
    StructuredArrayAllocationFailed {
        /// Zero-based index in the original JSON traversal.
        json_event_index: usize,
        /// Scratch allocation that failed.
        resource: JsonStructuredArrayResource,
        /// Additional elements requested from the arena.
        requested_additional: usize,
    },
}

impl Display for JsonRecordEventError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Number {
                json_event_index,
                source,
            } => write!(
                formatter,
                "invalid JSON number at traversal event {json_event_index}: {source}"
            ),
            Self::Timestamp {
                json_event_index,
                source,
            } => write!(
                formatter,
                "invalid authoritative timestamp at traversal event {json_event_index}: {source}"
            ),
            Self::Traversal {
                json_event_index,
                reason,
            } => write!(
                formatter,
                "invalid JSON traversal at event {json_event_index}: {reason}"
            ),
            Self::StructuredArrayLimit {
                json_event_index,
                resource,
                actual,
                limit,
            } => write!(
                formatter,
                "structured JSON array at traversal event {json_event_index} has {actual} \
                 {resource}, exceeding limit {limit}"
            ),
            Self::StructuredArrayAllocationFailed {
                json_event_index,
                resource,
                requested_additional,
            } => write!(
                formatter,
                "failed to reserve {requested_additional} additional element(s) for {resource} at \
                 traversal event {json_event_index}"
            ),
        }
    }
}

impl Error for JsonRecordEventError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Number { source, .. } => Some(source),
            Self::Timestamp { source, .. } => Some(source),
            Self::Traversal { reason, .. } => Some(reason),
            Self::StructuredArrayLimit { .. } | Self::StructuredArrayAllocationFailed { .. } => {
                None
            }
        }
    }
}

/// JSON conversion or archive-planning failure for one record.
pub type JsonArchiveAppendError = RecordEventAppendError<JsonRecordEventError>;

/// JSON conversion or archive-set failure for one record.
pub type JsonArchiveSetAppendError<S, C> = ArchiveSetAppendError<
    JsonRecordEventError,
    <S as FinalizedArchiveSink>::Error,
    <C as ArchiveSetStatsCallback>::Error,
>;

/// Source-lifecycle or archive-set failure outside a JSON record callback.
pub type JsonArchiveSetError<S, C> =
    ArchiveSetError<<S as FinalizedArchiveSink>::Error, <C as ArchiveSetStatsCallback>::Error>;

/// Reusable sink that appends NDJSON or parse-many documents into an archive writer.
///
/// Keys, decoded strings, exact floating-point tokens, and exact unstructured-array lexemes remain
/// borrowed for the duration of each callback. Default conversion uses a constant-size state
/// machine and does not allocate a per-record tree or event vector. Opt-in structured arrays stage
/// only their borrowed shape in a bounded reusable arena.
pub struct JsonArchiveSink<'archive, A> {
    archive: &'archive mut A,
    options: JsonArchiveOptions,
    structured_arena: Bump,
}

/// Streaming JSON adapter that attributes exact consumed source bytes before archive rotation.
///
/// One adapter instance represents one input stream whose offsets begin at zero. Leading and
/// inter-document bytes are charged to the archive containing the following record. Use
/// [`Self::for_source`] and [`Self::finish_source`] to bracket those records with exact range
/// metadata and atomically charge bytes after the final record. [`Self::new`] retains the lower-
/// level, untracked behavior for callers that manage source lifecycle themselves.
pub struct JsonArchiveSetSink<'archive, S, C> {
    archive_set: &'archive mut ArchiveSetWriter<S, C>,
    options: JsonArchiveOptions,
    accounted_input_bytes: u64,
    structured_arena: Bump,
}

/// Timestamp-aware view of a JSON archive sink.
///
/// The resolver is compiled once and borrowed by every record callback. Path matching is
/// iterative and performs no per-record allocation.
pub struct TimestampedJsonArchiveSink<'resolver, 'archive, A> {
    inner: JsonArchiveSink<'archive, A>,
    resolver: &'resolver JsonTimestampResolver,
}

/// Timestamp-aware view of a rotating JSON archive-set sink.
pub struct TimestampedJsonArchiveSetSink<'resolver, 'archive, S, C> {
    inner: JsonArchiveSetSink<'archive, S, C>,
    resolver: &'resolver JsonTimestampResolver,
}

impl<'archive, S, C> JsonArchiveSetSink<'archive, S, C>
where
    S: FinalizedArchiveSink,
    C: ArchiveSetStatsCallback,
{
    /// Creates an adapter for one zero-offset input stream.
    #[must_use]
    pub fn new(
        archive_set: &'archive mut ArchiveSetWriter<S, C>,
        options: JsonArchiveOptions,
    ) -> Self {
        Self {
            archive_set,
            options,
            accounted_input_bytes: 0,
            structured_arena: Bump::new(),
        }
    }

    /// Creates a zero-offset adapter and opens its source context in the archive set.
    ///
    /// Canonical filename and archive-creator identity remain caller policy. The context follows
    /// post-record archive rotation and receives the exact C++ file-split numbering maintained by
    /// [`ArchiveSetWriter`].
    ///
    /// # Errors
    ///
    /// Returns a source-lifecycle or archive-set state error without constructing the adapter.
    pub fn for_source(
        archive_set: &'archive mut ArchiveSetWriter<S, C>,
        options: JsonArchiveOptions,
        source: ArchiveSourceContext,
    ) -> Result<Self, JsonArchiveSetError<S, C>> {
        archive_set.begin_source(source)?;
        Ok(Self::new(archive_set, options))
    }

    /// Returns the immutable JSON conversion options.
    #[must_use]
    pub const fn options(&self) -> JsonArchiveOptions {
        self.options
    }

    /// Returns source bytes already attributed through successful record callbacks.
    #[must_use]
    pub const fn accounted_input_bytes(&self) -> u64 {
        self.accounted_input_bytes
    }

    /// Returns the underlying archive-set session.
    pub const fn archive_set(&self) -> &ArchiveSetWriter<S, C> {
        self.archive_set
    }

    /// Returns the underlying archive-set session mutably.
    pub const fn archive_set_mut(&mut self) -> &mut ArchiveSetWriter<S, C> {
        self.archive_set
    }

    /// Consumes the adapter and returns the underlying archive-set session.
    ///
    /// This does not close a source opened by [`Self::for_source`]. It permits a caller to recover
    /// or explicitly abandon source state after a reader failure.
    pub fn into_inner(self) -> &'archive mut ArchiveSetWriter<S, C> {
        self.archive_set
    }

    /// Atomically charges source bytes after the final record, closes the source, and returns the
    /// underlying archive-set session.
    ///
    /// `total_input_bytes` is the reader's absolute zero-based input counter after consuming the
    /// stream. It must not precede [`Self::accounted_input_bytes`].
    ///
    /// # Errors
    ///
    /// Returns a checked-size or source-lifecycle error. On failure, neither trailing bytes nor
    /// source closure is committed and the adapter is consumed; the error does not own the
    /// archive set, which remains with the original caller.
    pub fn finish_source(
        self,
        total_input_bytes: u64,
    ) -> Result<&'archive mut ArchiveSetWriter<S, C>, JsonArchiveSetError<S, C>> {
        let trailing_bytes = total_input_bytes
            .checked_sub(self.accounted_input_bytes)
            .ok_or(ArchiveSetError::SizeOverflow)?;
        self.archive_set
            .end_source_with_uncompressed_bytes(trailing_bytes)?;
        Ok(self.archive_set)
    }

    fn append_events_with_resolver(
        &mut self,
        events: JsonEvents<'_>,
        input_end: u64,
        resolver: Option<&JsonTimestampResolver>,
    ) -> Result<(), JsonArchiveSetAppendError<S, C>> {
        let source_bytes = input_end.checked_sub(self.accounted_input_bytes).ok_or(
            ArchiveSetAppendError::ArchiveSet(crate::writer::ArchiveSetError::SizeOverflow),
        )?;
        let result = self
            .archive_set
            .try_append_replayable_record_events_with_source_bytes(
                JsonRecordEvents::new(events, self.options, resolver, &self.structured_arena),
                source_bytes,
            );
        self.structured_arena.reset();
        let should_advance = match &result {
            Ok(()) => true,
            Err(error) => error.record_committed(),
        };
        if should_advance {
            self.accounted_input_bytes = input_end;
        }
        result
    }

    /// Converts this sink into a timestamp-aware adapter without moving the archive set.
    #[must_use]
    pub const fn with_timestamp_resolver<'resolver>(
        self,
        resolver: &'resolver JsonTimestampResolver,
    ) -> TimestampedJsonArchiveSetSink<'resolver, 'archive, S, C> {
        TimestampedJsonArchiveSetSink {
            inner: self,
            resolver,
        }
    }
}

impl<'archive, A: RecordEventAppender> JsonArchiveSink<'archive, A> {
    /// Creates an adapter over a caller-owned open archive.
    #[must_use]
    pub fn new(archive: &'archive mut A, options: JsonArchiveOptions) -> Self {
        Self {
            archive,
            options,
            structured_arena: Bump::new(),
        }
    }

    /// Returns the immutable conversion options.
    #[must_use]
    pub const fn options(&self) -> JsonArchiveOptions {
        self.options
    }

    /// Returns the underlying open archive.
    #[must_use]
    pub const fn archive(&self) -> &A {
        self.archive
    }

    /// Returns the underlying open archive mutably.
    pub const fn archive_mut(&mut self) -> &mut A {
        self.archive
    }

    /// Consumes the adapter and returns the underlying open archive.
    #[must_use]
    pub fn into_inner(self) -> &'archive mut A {
        self.archive
    }

    /// Converts this sink into a timestamp-aware adapter without moving the archive writer.
    #[must_use]
    pub const fn with_timestamp_resolver<'resolver>(
        self,
        resolver: &'resolver JsonTimestampResolver,
    ) -> TimestampedJsonArchiveSink<'resolver, 'archive, A> {
        TimestampedJsonArchiveSink {
            inner: self,
            resolver,
        }
    }

    /// Appends one validated root-object traversal.
    ///
    /// # Errors
    ///
    /// Returns a located number/traversal conversion error or a structured archive append error.
    /// The archive remains unchanged for this record on every error.
    pub fn append_events(&mut self, events: JsonEvents<'_>) -> Result<(), JsonArchiveAppendError> {
        self.append_events_with_resolver(events, None)
    }

    fn append_events_with_resolver(
        &mut self,
        events: JsonEvents<'_>,
        resolver: Option<&JsonTimestampResolver>,
    ) -> Result<(), JsonArchiveAppendError> {
        let result = self.archive.try_append_record_events(JsonRecordEvents::new(
            events,
            self.options,
            resolver,
            &self.structured_arena,
        ));
        self.structured_arena.reset();
        result
    }
}

impl<'archive, A: RecordEventAppender> TimestampedJsonArchiveSink<'_, 'archive, A> {
    /// Returns the immutable timestamp resolver.
    #[must_use]
    pub const fn resolver(&self) -> &JsonTimestampResolver {
        self.resolver
    }

    /// Returns the underlying open archive.
    #[must_use]
    pub const fn archive(&self) -> &A {
        self.inner.archive()
    }

    /// Returns the underlying open archive mutably.
    pub const fn archive_mut(&mut self) -> &mut A {
        self.inner.archive_mut()
    }

    /// Appends one validated traversal with authoritative timestamp recognition.
    ///
    /// # Errors
    ///
    /// Returns a located conversion error or a structured archive append error. The archive is
    /// unchanged for this record on every error.
    pub fn append_events(&mut self, events: JsonEvents<'_>) -> Result<(), JsonArchiveAppendError> {
        self.inner
            .append_events_with_resolver(events, Some(self.resolver))
    }

    /// Removes timestamp recognition and returns the ordinary adapter.
    #[must_use]
    pub fn without_timestamp_resolver(self) -> JsonArchiveSink<'archive, A> {
        self.inner
    }
}

impl<'archive, S, C> TimestampedJsonArchiveSetSink<'_, 'archive, S, C>
where
    S: FinalizedArchiveSink,
    C: ArchiveSetStatsCallback,
{
    /// Returns the immutable timestamp resolver.
    #[must_use]
    pub const fn resolver(&self) -> &JsonTimestampResolver {
        self.resolver
    }

    /// Returns source bytes already attributed through successful record callbacks.
    #[must_use]
    pub const fn accounted_input_bytes(&self) -> u64 {
        self.inner.accounted_input_bytes()
    }

    /// Returns the underlying archive-set session.
    pub const fn archive_set(&self) -> &ArchiveSetWriter<S, C> {
        self.inner.archive_set()
    }

    /// Returns the underlying archive-set session mutably.
    pub const fn archive_set_mut(&mut self) -> &mut ArchiveSetWriter<S, C> {
        self.inner.archive_set_mut()
    }

    /// Atomically charges trailing source bytes, closes the source, and returns the archive set.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`JsonArchiveSetSink::finish_source`].
    pub fn finish_source(
        self,
        total_input_bytes: u64,
    ) -> Result<&'archive mut ArchiveSetWriter<S, C>, JsonArchiveSetError<S, C>> {
        self.inner.finish_source(total_input_bytes)
    }

    /// Removes timestamp recognition and returns the ordinary adapter.
    #[must_use]
    pub fn without_timestamp_resolver(self) -> JsonArchiveSetSink<'archive, S, C> {
        self.inner
    }
}

impl<A: RecordEventAppender> NdjsonRecordSink for JsonArchiveSink<'_, A> {
    type Error = JsonArchiveAppendError;

    fn write_record(&mut self, record: NdjsonRecord<'_>) -> Result<(), Self::Error> {
        self.append_events(record.events())
    }
}

impl<A: RecordEventAppender> ParseManyDocumentSink for JsonArchiveSink<'_, A> {
    type Error = JsonArchiveAppendError;

    fn write_document(&mut self, document: ParseManyDocument<'_>) -> Result<(), Self::Error> {
        self.append_events(document.events())
    }
}

impl<A: RecordEventAppender> NdjsonRecordSink for TimestampedJsonArchiveSink<'_, '_, A> {
    type Error = JsonArchiveAppendError;

    fn write_record(&mut self, record: NdjsonRecord<'_>) -> Result<(), Self::Error> {
        self.append_events(record.events())
    }
}

impl<A: RecordEventAppender> ParseManyDocumentSink for TimestampedJsonArchiveSink<'_, '_, A> {
    type Error = JsonArchiveAppendError;

    fn write_document(&mut self, document: ParseManyDocument<'_>) -> Result<(), Self::Error> {
        self.append_events(document.events())
    }
}

impl<S, C> NdjsonRecordSink for JsonArchiveSetSink<'_, S, C>
where
    S: FinalizedArchiveSink,
    C: ArchiveSetStatsCallback,
{
    type Error = JsonArchiveSetAppendError<S, C>;

    fn write_record(&mut self, record: NdjsonRecord<'_>) -> Result<(), Self::Error> {
        let line_bytes = u64::try_from(record.line_bytes().len()).map_err(|_| {
            ArchiveSetAppendError::ArchiveSet(crate::writer::ArchiveSetError::SizeOverflow)
        })?;
        let input_end = record.input_offset().checked_add(line_bytes).ok_or(
            ArchiveSetAppendError::ArchiveSet(crate::writer::ArchiveSetError::SizeOverflow),
        )?;
        self.append_events_with_resolver(record.events(), input_end, None)
    }
}

impl<S, C> ParseManyDocumentSink for JsonArchiveSetSink<'_, S, C>
where
    S: FinalizedArchiveSink,
    C: ArchiveSetStatsCallback,
{
    type Error = JsonArchiveSetAppendError<S, C>;

    fn write_document(&mut self, document: ParseManyDocument<'_>) -> Result<(), Self::Error> {
        let document_bytes = u64::try_from(document.json_bytes().len()).map_err(|_| {
            ArchiveSetAppendError::ArchiveSet(crate::writer::ArchiveSetError::SizeOverflow)
        })?;
        let input_end = document.input_offset().checked_add(document_bytes).ok_or(
            ArchiveSetAppendError::ArchiveSet(crate::writer::ArchiveSetError::SizeOverflow),
        )?;
        self.append_events_with_resolver(document.events(), input_end, None)
    }
}

impl<S, C> NdjsonRecordSink for TimestampedJsonArchiveSetSink<'_, '_, S, C>
where
    S: FinalizedArchiveSink,
    C: ArchiveSetStatsCallback,
{
    type Error = JsonArchiveSetAppendError<S, C>;

    fn write_record(&mut self, record: NdjsonRecord<'_>) -> Result<(), Self::Error> {
        let line_bytes = u64::try_from(record.line_bytes().len()).map_err(|_| {
            ArchiveSetAppendError::ArchiveSet(crate::writer::ArchiveSetError::SizeOverflow)
        })?;
        let input_end = record.input_offset().checked_add(line_bytes).ok_or(
            ArchiveSetAppendError::ArchiveSet(crate::writer::ArchiveSetError::SizeOverflow),
        )?;
        self.inner
            .append_events_with_resolver(record.events(), input_end, Some(self.resolver))
    }
}

impl<S, C> ParseManyDocumentSink for TimestampedJsonArchiveSetSink<'_, '_, S, C>
where
    S: FinalizedArchiveSink,
    C: ArchiveSetStatsCallback,
{
    type Error = JsonArchiveSetAppendError<S, C>;

    fn write_document(&mut self, document: ParseManyDocument<'_>) -> Result<(), Self::Error> {
        let document_bytes = u64::try_from(document.json_bytes().len()).map_err(|_| {
            ArchiveSetAppendError::ArchiveSet(crate::writer::ArchiveSetError::SizeOverflow)
        })?;
        let input_end = document.input_offset().checked_add(document_bytes).ok_or(
            ArchiveSetAppendError::ArchiveSet(crate::writer::ArchiveSetError::SizeOverflow),
        )?;
        self.inner
            .append_events_with_resolver(document.events(), input_end, Some(self.resolver))
    }
}

#[derive(Clone)]
struct JsonRecordEvents<'record> {
    events: JsonEvents<'record>,
    pending_key: Option<&'record [u8]>,
    object_depth: usize,
    json_event_index: usize,
    root_state: RootState,
    options: JsonArchiveOptions,
    timestamp_resolver: Option<&'record JsonTimestampResolver>,
    unmatched_object_depth: Option<usize>,
    structured_arena: &'record Bump,
    structured_values: u64,
}

#[derive(Clone, Copy)]
enum RootState {
    Initial,
    Open,
    Finished,
    Stopped,
}

enum JsonRecordDispatchError {
    Source(JsonRecordEventError),
    Append(crate::writer::AppendError),
}

impl From<JsonRecordEventError> for JsonRecordDispatchError {
    fn from(source: JsonRecordEventError) -> Self {
        Self::Source(source)
    }
}

#[derive(Default)]
struct CapturedRecordEvent<'record> {
    event: Option<RecordEventRef<'record>>,
}

impl<'record> RecordEventConsumer<'record> for CapturedRecordEvent<'record> {
    fn value(&mut self, field: FieldRef<'record>) -> Result<(), crate::writer::AppendError> {
        self.event = Some(RecordEventRef::Value(field));
        Ok(())
    }

    fn kv_ir_namespace(
        &mut self,
        _namespace: super::KvIrNamespace,
    ) -> Result<(), crate::writer::AppendError> {
        unreachable!("the JSON record source cannot emit KV-IR namespace boundaries")
    }

    fn kv_ir_encoded_text(
        &mut self,
        _key: &'record [u8],
        _event: &'record super::KvIrLogEvent<'record>,
        _pair_index: usize,
    ) -> Result<(), crate::writer::AppendError> {
        unreachable!("the JSON record source cannot emit KV-IR encoded text")
    }

    fn object_start(&mut self, key: &'record [u8]) -> Result<(), crate::writer::AppendError> {
        self.event = Some(RecordEventRef::ObjectStart(key));
        Ok(())
    }

    fn object_end(&mut self) -> Result<(), crate::writer::AppendError> {
        self.event = Some(RecordEventRef::ObjectEnd);
        Ok(())
    }
}

enum StructuredJsonFrame<'record> {
    Array(BumpVec<'record, ValueRef<'record>>),
    Object {
        fields: BumpVec<'record, FieldRef<'record>>,
        pending_key: Option<&'record [u8]>,
    },
}

impl<'record> StructuredJsonFrame<'record> {
    fn array(arena: &'record Bump) -> Self {
        Self::Array(BumpVec::new_in(arena))
    }

    fn object(arena: &'record Bump) -> Self {
        Self::Object {
            fields: BumpVec::new_in(arena),
            pending_key: None,
        }
    }

    fn into_value(self) -> ValueRef<'record> {
        match self {
            Self::Array(values) => ValueRef::Array(values.into_bump_slice()),
            Self::Object { fields, .. } => ValueRef::Object(fields.into_bump_slice()),
        }
    }
}

impl<'record> JsonRecordEvents<'record> {
    const fn new(
        events: JsonEvents<'record>,
        options: JsonArchiveOptions,
        timestamp_resolver: Option<&'record JsonTimestampResolver>,
        structured_arena: &'record Bump,
    ) -> Self {
        Self {
            events,
            pending_key: None,
            object_depth: 0,
            json_event_index: 0,
            root_state: RootState::Initial,
            options,
            timestamp_resolver,
            unmatched_object_depth: None,
            structured_arena,
            structured_values: 0,
        }
    }

    fn next_stored_event(&mut self) -> Result<Option<(usize, StoredEvent)>, JsonRecordEventError> {
        let Some(event) = self.events.next_stored() else {
            return Ok(None);
        };
        let index = self.json_event_index;
        self.json_event_index = self
            .json_event_index
            .checked_add(1)
            .ok_or_else(|| Self::traversal(index, JsonRecordTraversalError::EventIndexOverflow))?;
        Ok(Some((index, event)))
    }

    const fn traversal(
        json_event_index: usize,
        reason: JsonRecordTraversalError,
    ) -> JsonRecordEventError {
        JsonRecordEventError::Traversal {
            json_event_index,
            reason,
        }
    }

    fn take_key(&mut self, event_index: usize) -> Result<&'record [u8], JsonRecordEventError> {
        self.pending_key
            .take()
            .ok_or_else(|| Self::traversal(event_index, JsonRecordTraversalError::MissingObjectKey))
    }

    fn skip_array_contents(&mut self, start_index: usize) -> Result<(), JsonRecordEventError> {
        let mut depth = 1_usize;
        while let Some((event_index, event)) = self.next_stored_event()? {
            match event {
                StoredEvent::ArrayStart(_) => {
                    depth = depth.checked_add(1).ok_or_else(|| {
                        Self::traversal(event_index, JsonRecordTraversalError::EventIndexOverflow)
                    })?;
                }
                StoredEvent::ArrayEnd => {
                    depth -= 1;
                    if 0 == depth {
                        return Ok(());
                    }
                }
                _ => {}
            }
        }
        Err(Self::traversal(
            start_index,
            JsonRecordTraversalError::UnexpectedEnd,
        ))
    }

    const fn structured_limit(
        json_event_index: usize,
        resource: JsonStructuredArrayLimitResource,
        actual: u64,
        limit: u64,
    ) -> JsonRecordEventError {
        JsonRecordEventError::StructuredArrayLimit {
            json_event_index,
            resource,
            actual,
            limit,
        }
    }

    const fn structured_allocation(
        json_event_index: usize,
        resource: JsonStructuredArrayResource,
    ) -> JsonRecordEventError {
        JsonRecordEventError::StructuredArrayAllocationFailed {
            json_event_index,
            resource,
            requested_additional: 1,
        }
    }

    fn add_structured_value(&mut self, event_index: usize) -> Result<(), JsonRecordEventError> {
        let limit = self.options.structured_array_limits.max_values();
        let actual = self.structured_values.checked_add(1).ok_or_else(|| {
            Self::structured_limit(
                event_index,
                JsonStructuredArrayLimitResource::Values,
                u64::MAX,
                limit,
            )
        })?;
        if actual > limit {
            return Err(Self::structured_limit(
                event_index,
                JsonStructuredArrayLimitResource::Values,
                actual,
                limit,
            ));
        }
        self.structured_values = actual;
        Ok(())
    }

    fn open_structured_frame(
        &mut self,
        frames: &mut BumpVec<'record, StructuredJsonFrame<'record>>,
        frame: StructuredJsonFrame<'record>,
        event_index: usize,
    ) -> Result<(), JsonRecordEventError> {
        self.add_structured_value(event_index)?;
        let limit = self.options.structured_array_limits.max_nesting_depth();
        let actual = u64::try_from(frames.len())
            .ok()
            .and_then(|depth| depth.checked_add(1))
            .ok_or_else(|| {
                Self::structured_limit(
                    event_index,
                    JsonStructuredArrayLimitResource::NestingDepth,
                    u64::MAX,
                    limit,
                )
            })?;
        if actual > limit {
            return Err(Self::structured_limit(
                event_index,
                JsonStructuredArrayLimitResource::NestingDepth,
                actual,
                limit,
            ));
        }
        frames.try_reserve(1).map_err(|_| {
            Self::structured_allocation(event_index, JsonStructuredArrayResource::TraversalStack)
        })?;
        frames.push(frame);
        Ok(())
    }

    fn append_structured_value(
        frame: &mut StructuredJsonFrame<'record>,
        value: ValueRef<'record>,
        event_index: usize,
    ) -> Result<(), JsonRecordEventError> {
        match frame {
            StructuredJsonFrame::Array(values) => {
                values.try_reserve(1).map_err(|_| {
                    Self::structured_allocation(
                        event_index,
                        JsonStructuredArrayResource::ArrayValues,
                    )
                })?;
                values.push(value);
            }
            StructuredJsonFrame::Object {
                fields,
                pending_key,
            } => {
                let key = pending_key.ok_or_else(|| {
                    Self::traversal(event_index, JsonRecordTraversalError::MissingObjectKey)
                })?;
                fields.try_reserve(1).map_err(|_| {
                    Self::structured_allocation(
                        event_index,
                        JsonStructuredArrayResource::ObjectFields,
                    )
                })?;
                fields.push(FieldRef::new(key, value));
                *pending_key = None;
            }
        }
        Ok(())
    }

    fn structured_object_key(&self, event: StoredEvent) -> Option<&'record [u8]> {
        match event {
            StoredEvent::ObjectKeySource(raw) => Some(raw.inner_bytes(self.events.raw_json())),
            StoredEvent::ObjectKeyDecoded { decoded, .. } => {
                Some(decoded.string(self.events.decoded()).as_bytes())
            }
            _ => None,
        }
    }

    fn set_structured_object_key(
        frames: &mut BumpVec<'record, StructuredJsonFrame<'record>>,
        key: &'record [u8],
        event_index: usize,
    ) -> Result<(), JsonRecordEventError> {
        let Some(StructuredJsonFrame::Object { pending_key, .. }) = frames.last_mut() else {
            return Err(Self::traversal(
                event_index,
                JsonRecordTraversalError::UnexpectedObjectKey,
            ));
        };
        if pending_key.is_some() {
            return Err(Self::traversal(
                event_index,
                JsonRecordTraversalError::UnexpectedObjectKey,
            ));
        }
        *pending_key = Some(key);
        Ok(())
    }

    fn close_structured_frame(
        frames: &mut BumpVec<'record, StructuredJsonFrame<'record>>,
        close_array: bool,
        event_index: usize,
    ) -> Result<Option<ValueRef<'record>>, JsonRecordEventError> {
        let Some(frame) = frames.last() else {
            let reason = if close_array {
                JsonRecordTraversalError::UnexpectedArrayEnd
            } else {
                JsonRecordTraversalError::UnexpectedObjectEnd
            };
            return Err(Self::traversal(event_index, reason));
        };
        let matching_type = matches!(frame, StructuredJsonFrame::Array(_)) == close_array;
        if !matching_type {
            let reason = if close_array {
                JsonRecordTraversalError::UnexpectedArrayEnd
            } else {
                JsonRecordTraversalError::UnexpectedObjectEnd
            };
            return Err(Self::traversal(event_index, reason));
        }
        if matches!(
            frame,
            StructuredJsonFrame::Object {
                pending_key: Some(_),
                ..
            }
        ) {
            return Err(Self::traversal(
                event_index,
                JsonRecordTraversalError::DanglingObjectKey,
            ));
        }
        let value = frames
            .pop()
            .expect("validated structured container stack")
            .into_value();
        let Some(parent) = frames.last_mut() else {
            return Ok(Some(value));
        };
        Self::append_structured_value(parent, value, event_index)?;
        Ok(None)
    }

    fn structured_array_value(
        &mut self,
        start_index: usize,
    ) -> Result<ValueRef<'record>, JsonRecordEventError> {
        let mut frames = BumpVec::new_in(self.structured_arena);
        self.open_structured_frame(
            &mut frames,
            StructuredJsonFrame::array(self.structured_arena),
            start_index,
        )?;
        loop {
            let Some((event_index, event)) = self.next_stored_event()? else {
                return Err(Self::traversal(
                    start_index,
                    JsonRecordTraversalError::UnexpectedEnd,
                ));
            };
            if let Some(key) = self.structured_object_key(event) {
                Self::set_structured_object_key(&mut frames, key, event_index)?;
                continue;
            }
            let value = match event {
                StoredEvent::ObjectStart => {
                    self.open_structured_frame(
                        &mut frames,
                        StructuredJsonFrame::object(self.structured_arena),
                        event_index,
                    )?;
                    continue;
                }
                StoredEvent::ArrayStart(_) => {
                    self.open_structured_frame(
                        &mut frames,
                        StructuredJsonFrame::array(self.structured_arena),
                        event_index,
                    )?;
                    continue;
                }
                StoredEvent::ObjectEnd => {
                    if let Some(value) =
                        Self::close_structured_frame(&mut frames, false, event_index)?
                    {
                        return Ok(value);
                    }
                    continue;
                }
                StoredEvent::ArrayEnd => {
                    if let Some(value) =
                        Self::close_structured_frame(&mut frames, true, event_index)?
                    {
                        return Ok(value);
                    }
                    continue;
                }
                StoredEvent::StringSource(raw) => self.string_value(
                    event_index,
                    super::JsonString::new_bytes(
                        raw.bytes(self.events.raw_json()),
                        raw.inner_bytes(self.events.raw_json()),
                    ),
                    false,
                )?,
                StoredEvent::StringDecoded { raw, decoded } => self.string_value(
                    event_index,
                    super::JsonString::new(
                        raw.bytes(self.events.raw_json()),
                        decoded.string(self.events.decoded()),
                    ),
                    false,
                )?,
                StoredEvent::Number { raw, syntax } => self.number_value(
                    event_index,
                    raw.bytes(self.events.raw_json()),
                    syntax,
                    false,
                )?,
                StoredEvent::Boolean(value) => ValueRef::Bool(value),
                StoredEvent::Null => ValueRef::Null,
                StoredEvent::ObjectKeySource(_) | StoredEvent::ObjectKeyDecoded { .. } => {
                    unreachable!("structured object keys are handled before value dispatch")
                }
            };
            self.add_structured_value(event_index)?;
            let frame = frames
                .last_mut()
                .expect("structured scalar must have an open root array");
            Self::append_structured_value(frame, value, event_index)?;
        }
    }

    fn scalar_field(
        &mut self,
        event_index: usize,
        value: ValueRef<'record>,
    ) -> Result<FieldRef<'record>, JsonRecordEventError> {
        let key = self.take_key(event_index)?;
        Ok(FieldRef::new(key, value))
    }

    fn number_field(
        &mut self,
        event_index: usize,
        source: &'record [u8],
        syntax: ValidatedJsonNumberSyntax,
    ) -> Result<FieldRef<'record>, JsonRecordEventError> {
        let value = self.number_value(event_index, source, syntax, true)?;
        self.scalar_field(event_index, value)
    }

    fn number_value(
        &self,
        event_index: usize,
        source: &'record [u8],
        syntax: ValidatedJsonNumberSyntax,
        recognize_timestamp: bool,
    ) -> Result<ValueRef<'record>, JsonRecordEventError> {
        if let Some(resolver) = recognize_timestamp
            .then(|| self.matching_timestamp_resolver())
            .flatten()
        {
            let (timestamp, prevalidated) = resolver
                .resolve_validated_number(source, syntax)
                .map_err(|source| JsonRecordEventError::Timestamp {
                    json_event_index: event_index,
                    source,
                })?;
            return Ok(if prevalidated {
                ValueRef::PrevalidatedTimestamp(PrevalidatedTimestampRef::new(timestamp))
            } else {
                ValueRef::Timestamp(timestamp)
            });
        }
        let number = classify_validated_json_number(source, syntax).map_err(|source| {
            JsonRecordEventError::Number {
                json_event_index: event_index,
                source,
            }
        })?;
        let value = match number {
            ClassifiedJsonNumber::Integer(value) => ValueRef::I64(value),
            ClassifiedJsonNumber::Float { value, source } if self.options.retain_float_format => {
                ValueRef::RetainedFloat(RetainedFloatRef::new_trusted(
                    value,
                    source,
                    syntax.dot_position(),
                    syntax.exponent_position(),
                ))
            }
            ClassifiedJsonNumber::Float { value, .. } => ValueRef::F64(value),
        };
        Ok(value)
    }

    fn string_field(
        &mut self,
        event_index: usize,
        value: super::JsonString<'record>,
    ) -> Result<FieldRef<'record>, JsonRecordEventError> {
        let scalar = self.string_value(event_index, value, true)?;
        self.scalar_field(event_index, scalar)
    }

    fn string_value(
        &self,
        event_index: usize,
        value: super::JsonString<'record>,
        recognize_timestamp: bool,
    ) -> Result<ValueRef<'record>, JsonRecordEventError> {
        if let Some(resolver) = recognize_timestamp
            .then(|| self.matching_timestamp_resolver())
            .flatten()
        {
            let timestamp = resolver.resolve_string(value).map_err(|source| {
                JsonRecordEventError::Timestamp {
                    json_event_index: event_index,
                    source,
                }
            })?;
            Ok(ValueRef::Timestamp(timestamp))
        } else {
            Ok(ValueRef::String(value.decoded_bytes()))
        }
    }

    fn matching_timestamp_resolver(&self) -> Option<&'record JsonTimestampResolver> {
        let resolver = self.timestamp_resolver?;
        if self.unmatched_object_depth.is_some()
            || resolver.path().components().len() != self.object_depth
        {
            return None;
        }
        let key = self.pending_key?;
        resolver
            .path()
            .matches(self.object_depth.checked_sub(1)?, key)
            .then_some(resolver)
    }

    fn enter_object_path(&mut self, key: &[u8], new_depth: usize) {
        let Some(resolver) = self.timestamp_resolver else {
            return;
        };
        if self.unmatched_object_depth.is_some() {
            return;
        }
        let component_index = self.object_depth - 1;
        let is_prefix = resolver.path().components().len() > self.object_depth
            && resolver.path().matches(component_index, key);
        if !is_prefix {
            self.unmatched_object_depth = Some(new_depth);
        }
    }

    fn exit_object_path(&mut self) {
        if self.unmatched_object_depth == Some(self.object_depth) {
            self.unmatched_object_depth = None;
        }
    }

    // Keeping this hot parser-to-record state transition in one dispatch makes its invariants and
    // measured branch shape visible together; splitting match arms only to meet a line heuristic
    // would obscure both.
    #[allow(clippy::too_many_lines)]
    fn process_event<C>(
        &mut self,
        event_index: usize,
        event: StoredEvent,
        consumer: &mut C,
    ) -> Result<bool, JsonRecordDispatchError>
    where
        C: RecordEventConsumer<'record>, {
        if matches!(self.root_state, RootState::Initial)
            && !matches!(event, StoredEvent::ObjectStart)
        {
            return Err(
                Self::traversal(event_index, JsonRecordTraversalError::ExpectedRootObject).into(),
            );
        }
        if matches!(self.root_state, RootState::Finished) {
            return Err(
                Self::traversal(event_index, JsonRecordTraversalError::TrailingRootEvent).into(),
            );
        }
        match event {
            StoredEvent::ObjectStart if matches!(self.root_state, RootState::Initial) => {
                self.root_state = RootState::Open;
                self.object_depth = 1;
                Ok(false)
            }
            StoredEvent::ObjectStart => {
                let key = self.take_key(event_index)?;
                let new_depth = self.object_depth.checked_add(1).ok_or_else(|| {
                    Self::traversal(event_index, JsonRecordTraversalError::EventIndexOverflow)
                })?;
                self.enter_object_path(key, new_depth);
                self.object_depth = new_depth;
                consumer
                    .object_start(key)
                    .map_err(JsonRecordDispatchError::Append)?;
                Ok(true)
            }
            StoredEvent::ObjectEnd => {
                if 0 == self.object_depth {
                    return Err(Self::traversal(
                        event_index,
                        JsonRecordTraversalError::UnexpectedObjectEnd,
                    )
                    .into());
                }
                if self.pending_key.is_some() {
                    return Err(Self::traversal(
                        event_index,
                        JsonRecordTraversalError::DanglingObjectKey,
                    )
                    .into());
                }
                self.exit_object_path();
                self.object_depth -= 1;
                if 0 == self.object_depth {
                    self.root_state = RootState::Finished;
                    Ok(false)
                } else {
                    consumer
                        .object_end()
                        .map_err(JsonRecordDispatchError::Append)?;
                    Ok(true)
                }
            }
            StoredEvent::ObjectKeySource(raw) => {
                if 0 == self.object_depth || self.pending_key.is_some() {
                    return Err(Self::traversal(
                        event_index,
                        JsonRecordTraversalError::UnexpectedObjectKey,
                    )
                    .into());
                }
                self.pending_key = Some(raw.inner_bytes(self.events.raw_json()));
                Ok(false)
            }
            StoredEvent::ObjectKeyDecoded { decoded, .. } => {
                if 0 == self.object_depth || self.pending_key.is_some() {
                    return Err(Self::traversal(
                        event_index,
                        JsonRecordTraversalError::UnexpectedObjectKey,
                    )
                    .into());
                }
                self.pending_key = Some(decoded.string(self.events.decoded()).as_bytes());
                Ok(false)
            }
            StoredEvent::StringSource(raw) => {
                let field = self.string_field(
                    event_index,
                    super::JsonString::new_bytes(
                        raw.bytes(self.events.raw_json()),
                        raw.inner_bytes(self.events.raw_json()),
                    ),
                )?;
                consumer
                    .value(field)
                    .map_err(JsonRecordDispatchError::Append)?;
                Ok(true)
            }
            StoredEvent::StringDecoded { raw, decoded } => {
                let field = self.string_field(
                    event_index,
                    super::JsonString::new(
                        raw.bytes(self.events.raw_json()),
                        decoded.string(self.events.decoded()),
                    ),
                )?;
                consumer
                    .value(field)
                    .map_err(JsonRecordDispatchError::Append)?;
                Ok(true)
            }
            StoredEvent::Number { raw, syntax } => {
                let field =
                    self.number_field(event_index, raw.bytes(self.events.raw_json()), syntax)?;
                consumer
                    .value(field)
                    .map_err(JsonRecordDispatchError::Append)?;
                Ok(true)
            }
            StoredEvent::Boolean(value) => {
                let field = self.scalar_field(event_index, ValueRef::Bool(value))?;
                consumer
                    .value(field)
                    .map_err(JsonRecordDispatchError::Append)?;
                Ok(true)
            }
            StoredEvent::Null => {
                let field = self.scalar_field(event_index, ValueRef::Null)?;
                consumer
                    .value(field)
                    .map_err(JsonRecordDispatchError::Append)?;
                Ok(true)
            }
            StoredEvent::ArrayStart(raw) => {
                let key = self.take_key(event_index)?;
                let value = if self.options.structurize_arrays {
                    self.structured_array_value(event_index)?
                } else {
                    self.skip_array_contents(event_index)?;
                    ValueRef::UnstructuredArray(UnstructuredArrayRef::new(
                        raw.bytes(self.events.raw_json()),
                    ))
                };
                consumer
                    .value(FieldRef::new(key, value))
                    .map_err(JsonRecordDispatchError::Append)?;
                Ok(true)
            }
            StoredEvent::ArrayEnd => Err(Self::traversal(
                event_index,
                JsonRecordTraversalError::UnexpectedArrayEnd,
            )
            .into()),
        }
    }

    const fn finish_event(
        &mut self,
    ) -> Option<Result<RecordEventRef<'record>, JsonRecordEventError>> {
        let previous = self.root_state;
        self.root_state = RootState::Stopped;
        match previous {
            RootState::Initial => Some(Err(Self::traversal(
                self.json_event_index,
                JsonRecordTraversalError::ExpectedRootObject,
            ))),
            RootState::Open | RootState::Stopped => Some(Err(Self::traversal(
                self.json_event_index,
                JsonRecordTraversalError::UnexpectedEnd,
            ))),
            RootState::Finished if 0 != self.object_depth || self.pending_key.is_some() => {
                Some(Err(Self::traversal(
                    self.json_event_index,
                    JsonRecordTraversalError::UnexpectedEnd,
                )))
            }
            RootState::Finished => None,
        }
    }
}

impl<'record> Iterator for JsonRecordEvents<'record> {
    type Item = Result<RecordEventRef<'record>, JsonRecordEventError>;

    fn next(&mut self) -> Option<Self::Item> {
        if matches!(self.root_state, RootState::Stopped) {
            return None;
        }
        loop {
            let next = match self.next_stored_event() {
                Ok(Some(next)) => next,
                Ok(None) => return self.finish_event(),
                Err(error) => {
                    self.root_state = RootState::Stopped;
                    return Some(Err(error));
                }
            };
            let mut captured = CapturedRecordEvent::default();
            match self.process_event(next.0, next.1, &mut captured) {
                Ok(true) => {
                    return Some(Ok(captured
                        .event
                        .expect("an emitted record event must be captured")));
                }
                Ok(false) => {}
                Err(JsonRecordDispatchError::Source(error)) => {
                    self.root_state = RootState::Stopped;
                    return Some(Err(error));
                }
                Err(JsonRecordDispatchError::Append(_)) => {
                    unreachable!("capturing a record event is infallible")
                }
            }
        }
    }
}

impl<'record> ReplayableRecordEventSource<'record> for JsonRecordEvents<'record> {
    type Error = JsonRecordEventError;

    fn consume<C>(mut self, consumer: &mut C) -> Result<(), RecordEventAppendError<Self::Error>>
    where
        C: RecordEventConsumer<'record>, {
        let mut record_event_index = 0_usize;
        while let Some((json_event_index, event)) =
            self.next_stored_event()
                .map_err(|source| RecordEventAppendError::Source {
                    event_index: record_event_index,
                    source,
                })?
        {
            let emitted = self
                .process_event(json_event_index, event, consumer)
                .map_err(|error| match error {
                    JsonRecordDispatchError::Source(source) => RecordEventAppendError::Source {
                        event_index: record_event_index,
                        source,
                    },
                    JsonRecordDispatchError::Append(source) => {
                        RecordEventAppendError::Append(source)
                    }
                })?;
            if emitted {
                record_event_index = record_event_index
                    .checked_add(1)
                    .ok_or(crate::writer::AppendError::SizeOverflow)?;
            }
        }
        if let Some(result) = self.finish_event() {
            let source = result.expect_err("JSON traversal completion never emits an event");
            return Err(RecordEventAppendError::Source {
                event_index: record_event_index,
                source,
            });
        }
        Ok(())
    }

    fn supports_cached_layout_proof(&self) -> bool {
        !self.options.structurize_arrays
    }
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;
    use std::fmt::Write as _;
    use std::io;
    use std::io::Cursor;

    use super::JsonArchiveOptions;
    use super::JsonArchiveSetSink;
    use super::JsonArchiveSink;
    use super::JsonRecordEventError;
    use super::JsonStructuredArrayLimitResource;
    use super::JsonStructuredArrayLimits;
    use super::JsonTimestampResolver;
    use crate::archive::RangeIndexValue;
    use crate::ingest::JsonNumberClassificationError;
    use crate::ingest::JsonNumberDomain;
    use crate::ingest::NdjsonOptions;
    use crate::ingest::NdjsonReadError;
    use crate::ingest::NdjsonReader;
    use crate::ingest::ParseManyOptions;
    use crate::ingest::ParseManyReadError;
    use crate::ingest::ParseManyReader;
    use crate::writer::AppendError;
    use crate::writer::AppendResource;
    use crate::writer::ArchiveSetAppendError;
    use crate::writer::ArchiveSetArchive;
    use crate::writer::ArchiveSetError;
    use crate::writer::ArchiveSetOptions;
    use crate::writer::ArchiveSetStats;
    use crate::writer::ArchiveSetStatsCallback;
    use crate::writer::ArchiveSetWriter;
    use crate::writer::ArchiveSourceContext;
    use crate::writer::FieldRef;
    use crate::writer::FinalizedArchiveSink;
    use crate::writer::OpenArchive;
    use crate::writer::RecordEventAppendError;
    use crate::writer::RecordRef;
    use crate::writer::RetainedFloatRef;
    use crate::writer::UnstructuredArrayRef;
    use crate::writer::ValueRef;
    use crate::writer::WriterLimits;
    use crate::writer::WriterOptions;

    #[derive(Debug, Default)]
    struct CapturedArchives(Vec<ArchiveSetStats>);

    impl FinalizedArchiveSink for CapturedArchives {
        type Error = Infallible;

        fn publish(&mut self, archive: &ArchiveSetArchive) -> Result<(), Self::Error> {
            self.0.push(archive.stats());
            Ok(())
        }
    }

    #[derive(Debug, Default)]
    struct CapturedSfas(Vec<Vec<u8>>);

    impl FinalizedArchiveSink for CapturedSfas {
        type Error = io::Error;

        fn publish(&mut self, archive: &ArchiveSetArchive) -> Result<(), Self::Error> {
            let mut bytes = Vec::new();
            archive.write_sfa(&mut bytes)?;
            self.0.push(bytes);
            Ok(())
        }
    }

    #[derive(Default)]
    struct FailOnceArchives {
        attempts: usize,
        failed: bool,
        stats: Vec<ArchiveSetStats>,
    }

    impl FinalizedArchiveSink for FailOnceArchives {
        type Error = io::Error;

        fn publish(&mut self, archive: &ArchiveSetArchive) -> Result<(), Self::Error> {
            self.attempts += 1;
            if !self.failed {
                self.failed = true;
                return Err(io::Error::other("injected publication failure"));
            }
            self.stats.push(archive.stats());
            Ok(())
        }
    }

    #[derive(Debug, Default)]
    struct CapturedStats(Vec<ArchiveSetStats>);

    impl ArchiveSetStatsCallback for CapturedStats {
        type Error = Infallible;

        fn on_archive(&mut self, stats: ArchiveSetStats) -> Result<(), Self::Error> {
            self.0.push(stats);
            Ok(())
        }
    }

    fn finished_bytes(archive: OpenArchive<Cursor<Vec<u8>>>) -> Vec<u8> {
        archive
            .finish()
            .expect("finish archive")
            .into_inner()
            .into_inner()
    }

    fn decode_ascii_hex(source: &[u8]) -> Vec<u8> {
        let mut decoded = Vec::with_capacity(source.len() / 2);
        let mut high = None;
        for byte in source
            .iter()
            .copied()
            .filter(|byte| !byte.is_ascii_whitespace())
        {
            let nibble = match byte {
                b'0'..=b'9' => byte - b'0',
                b'a'..=b'f' => byte - b'a' + 10,
                b'A'..=b'F' => byte - b'A' + 10,
                _ => panic!("fixture contains a non-hex byte"),
            };
            if let Some(high) = high.take() {
                decoded.push((high << 4) | nibble);
            } else {
                high = Some(nibble);
            }
        }
        assert!(high.is_none(), "fixture contains an odd number of nibbles");
        decoded
    }

    fn assert_json_source_range(stats: &ArchiveSetStats, end: u64, split: u64) {
        const FILENAME: &str = "input.json";
        const CREATOR: &str = "creator";
        let [range] = stats.range_index() else {
            panic!("expected exactly one source range")
        };
        assert_eq!(0, range.start_index());
        assert_eq!(end, range.end_index());
        assert_eq!(
            Some(FILENAME),
            range.field("_filename").and_then(|value| value.as_str())
        );
        assert_eq!(
            Some(CREATOR),
            range
                .field("_archive_creator_id")
                .and_then(RangeIndexValue::as_str)
        );
        assert_eq!(
            Some(split),
            range
                .field("_file_split_number")
                .and_then(RangeIndexValue::as_u64)
        );
    }

    #[test]
    fn parse_many_events_match_a_borrowed_tree_without_a_record_arena() {
        let input = concat!(
            r#"{"id":18446744073709551615,"metrics":{"ratio":-0.00,"empty":{}}"#,
            r#", "items":[1, {"message":"a b"}],"text":"hello\u0020world","none":null}"#,
        )
        .as_bytes();
        let writer_options = WriterOptions::default().with_log_order(false);
        let mut actual = OpenArchive::new(Cursor::new(Vec::new()), writer_options);
        let mut reader = ParseManyReader::new(input, ParseManyOptions::default());
        {
            let mut sink = JsonArchiveSink::new(&mut actual, JsonArchiveOptions::default());
            let stats = reader.read_to_end(&mut sink).expect("ingest parse-many");
            assert_eq!(1, stats.documents());
            assert_eq!(1, sink.archive().record_count());
        }

        let empty = [];
        let metrics = [
            FieldRef::new(
                b"ratio",
                ValueRef::RetainedFloat(RetainedFloatRef::new(-0.0, b"-0.00")),
            ),
            FieldRef::new(b"empty", ValueRef::Object(&empty)),
        ];
        let fields = [
            FieldRef::new(b"id", ValueRef::I64(-1)),
            FieldRef::new(b"metrics", ValueRef::Object(&metrics)),
            FieldRef::new(
                b"items",
                ValueRef::UnstructuredArray(UnstructuredArrayRef::new(
                    br#"[1, {"message":"a b"}]"#,
                )),
            ),
            FieldRef::new(b"text", ValueRef::String(b"hello world")),
            FieldRef::new(b"none", ValueRef::Null),
        ];
        let mut expected = OpenArchive::new(Cursor::new(Vec::new()), writer_options);
        expected
            .append_record(RecordRef::new(&fields))
            .expect("append borrowed record");
        assert_eq!(finished_bytes(expected), finished_bytes(actual));
    }

    #[test]
    fn parse_many_array_ingestion_is_byte_identical_to_the_cpp_oracle() {
        const SOURCE: &[u8] =
            include_bytes!("../../tests/fixtures/sfa-v0.5.0-unstructured-arrays-cpp-input.jsonl");
        const CPP_ORACLE: &[u8] =
            include_bytes!("../../tests/fixtures/sfa-v0.5.0-unstructured-arrays-cpp.bin");
        let source_size = u64::try_from(SOURCE.len()).expect("fixture size fits u64");
        let mut archive = OpenArchive::new(
            Cursor::new(Vec::new()),
            WriterOptions::default()
                .with_log_order(false)
                .with_uncompressed_size(source_size),
        );
        let mut reader = ParseManyReader::new(SOURCE, ParseManyOptions::default());
        {
            let mut sink = JsonArchiveSink::new(&mut archive, JsonArchiveOptions::default());
            let stats = reader
                .read_to_end(&mut sink)
                .expect("ingest C++ source fixture");
            assert_eq!(6, stats.documents());
        }
        assert_eq!(CPP_ORACLE, finished_bytes(archive));
    }

    #[test]
    fn archive_set_structured_array_ingestion_is_byte_identical_to_cpp() {
        const SOURCE: &[u8] =
            include_bytes!("../../tests/fixtures/sfa-v0.5.0-structured-arrays-cpp-input.jsonl");
        const CPP_ORACLE_HEX: &[u8] =
            include_bytes!("../../tests/fixtures/sfa-v0.5.0-structured-arrays-cpp.hex");

        let options =
            ArchiveSetOptions::new(WriterOptions::default().with_log_order(false), u64::MAX);
        let mut archive_set =
            ArchiveSetWriter::new(CapturedSfas::default(), CapturedStats::default(), options);
        let mut reader = NdjsonReader::new(SOURCE, NdjsonOptions::default());
        {
            let conversion = JsonArchiveOptions::default().with_structurize_arrays(true);
            assert!(conversion.structurizes_arrays());
            let mut sink = JsonArchiveSetSink::new(&mut archive_set, conversion);
            let stats = reader
                .read_to_end(&mut sink)
                .expect("ingest structured-array C++ source fixture");
            assert_eq!(9, stats.records());
            let source_bytes = u64::try_from(SOURCE.len()).expect("fixture length fits u64");
            let trailing_bytes = source_bytes
                .checked_sub(sink.accounted_input_bytes())
                .expect("reader accounting cannot exceed its source");
            assert_eq!(1, trailing_bytes, "fixture has one trailing LF");
            sink.archive_set_mut()
                .add_uncompressed_bytes(trailing_bytes)
                .expect("account trailing source separator");
        }

        let (published, callbacks) = archive_set
            .finish()
            .expect("finish structured-array archive set")
            .into_parts();
        assert_eq!(1, published.0.len());
        assert_eq!(decode_ascii_hex(CPP_ORACLE_HEX), published.0[0]);
        assert_eq!(1, callbacks.0.len());
        assert_eq!(9, callbacks.0[0].record_count());
    }

    #[test]
    fn structured_array_staging_limits_are_transactional_and_reusable() {
        let conversion = JsonArchiveOptions::default()
            .with_structurize_arrays(true)
            .with_structured_array_limits(JsonStructuredArrayLimits::new(2, 1));
        assert_eq!(
            JsonStructuredArrayLimits::new(2, 1),
            conversion.structured_array_limits()
        );
        let mut archive = OpenArchive::new(
            Cursor::new(Vec::<u8>::new()),
            WriterOptions::default().with_log_order(false),
        );
        {
            let mut sink = JsonArchiveSink::new(&mut archive, conversion);
            let mut too_many =
                NdjsonReader::new(b"{\"items\":[1,2]}\n".as_slice(), NdjsonOptions::default());
            let error = too_many
                .read_record(&mut sink)
                .expect_err("reject a third structured-array value");
            assert!(matches!(
                error,
                NdjsonReadError::Sink {
                    source: RecordEventAppendError::Source {
                        event_index: 0,
                        source: JsonRecordEventError::StructuredArrayLimit {
                            json_event_index: 4,
                            resource: JsonStructuredArrayLimitResource::Values,
                            actual: 3,
                            limit: 2,
                        },
                    },
                    ..
                }
            ));
            assert_eq!(0, sink.archive().record_count());

            let mut too_deep =
                NdjsonReader::new(b"{\"items\":[[]]}\n".as_slice(), NdjsonOptions::default());
            let error = too_deep
                .read_record(&mut sink)
                .expect_err("reject nested structured array at depth two");
            assert!(matches!(
                error,
                NdjsonReadError::Sink {
                    source: RecordEventAppendError::Source {
                        event_index: 0,
                        source: JsonRecordEventError::StructuredArrayLimit {
                            json_event_index: 3,
                            resource: JsonStructuredArrayLimitResource::NestingDepth,
                            actual: 2,
                            limit: 1,
                        },
                    },
                    ..
                }
            ));
            assert_eq!(0, sink.archive().record_count());

            let mut valid =
                NdjsonReader::new(b"{\"items\":[]}\n".as_slice(), NdjsonOptions::default());
            valid
                .read_to_end(&mut sink)
                .expect("reuse reset arena after rejected records");
            assert_eq!(1, sink.archive().record_count());
        }
    }

    #[test]
    fn source_aware_parse_many_is_byte_identical_to_the_complete_cpp_log_order_archive() {
        const SOURCE: &[u8] =
            include_bytes!("../../tests/fixtures/sfa-v0.5.0-log-order-cpp-input.jsonl");
        const CPP_ORACLE: &[u8] =
            include_bytes!("../../tests/fixtures/sfa-v0.5.0-log-order-cpp.bin");
        const FILENAME: &str =
            "components/clp-s/tests/fixtures/sfa-v0.5.0-log-order-cpp-input.jsonl";
        const CREATOR: &str = "a2217fe8-aef3-4efe-93dc-625977a1d35a";

        let options = ArchiveSetOptions::new(WriterOptions::default(), u64::MAX);
        let mut archive_set =
            ArchiveSetWriter::new(CapturedSfas::default(), CapturedStats::default(), options);
        let mut reader = ParseManyReader::new(SOURCE, ParseManyOptions::default());
        {
            let mut sink = JsonArchiveSetSink::for_source(
                &mut archive_set,
                JsonArchiveOptions::default(),
                ArchiveSourceContext::new(FILENAME, CREATOR),
            )
            .expect("open C++ fixture source");
            let stats = reader
                .read_to_end(&mut sink)
                .expect("ingest C++ log-order source fixture");
            assert_eq!(6, stats.documents());
            sink.finish_source(stats.input_bytes())
                .expect("close C++ fixture source");
        }
        let (published, callbacks) = archive_set
            .finish()
            .expect("finish C++ fixture archive set")
            .into_parts();
        assert_eq!(1, published.0.len());
        assert_eq!(CPP_ORACLE, published.0[0]);
        assert_eq!(1, callbacks.0.len());
        let [range] = callbacks.0[0].range_index() else {
            panic!("C++ fixture should contain one source range")
        };
        assert_eq!((0, 6), (range.start_index(), range.end_index()));
    }

    #[test]
    fn parse_many_timestamp_ingestion_is_byte_identical_to_the_cpp_oracle() {
        const SOURCE: &[u8] =
            include_bytes!("../../tests/fixtures/sfa-v0.5.0-timestamps-cpp-input.jsonl");
        const CPP_ORACLE: &[u8] =
            include_bytes!("../../tests/fixtures/sfa-v0.5.0-timestamps-cpp.bin");
        let source_size = u64::try_from(SOURCE.len()).expect("fixture size fits u64");
        let mut archive = OpenArchive::new(
            Cursor::new(Vec::new()),
            WriterOptions::default()
                .with_log_order(false)
                .with_uncompressed_size(source_size),
        );
        let resolver = JsonTimestampResolver::parse("ts").expect("compile timestamp path");
        let mut reader = ParseManyReader::new(SOURCE, ParseManyOptions::default());
        {
            let base = JsonArchiveSink::new(&mut archive, JsonArchiveOptions::default());
            let mut sink = base.with_timestamp_resolver(&resolver);
            let stats = reader
                .read_to_end(&mut sink)
                .expect("ingest C++ timestamp source fixture");
            assert_eq!(4, stats.documents());
        }
        assert_eq!(CPP_ORACLE, finished_bytes(archive));
    }

    #[test]
    fn timestamp_fast_path_keeps_noncanonical_lexemes_transactional() {
        let resolver = JsonTimestampResolver::parse("ts").expect("compile timestamp path");
        let rejected = [
            resolver
                .resolve_number(b"-0")
                .expect("resolve signed numeric zero"),
            resolver
                .resolve_number(b"9223372036854775809")
                .expect("resolve C++-compatible unsigned integer"),
            resolver
                .resolve_string(crate::ingest::JsonString::new(br#""01""#, "01"))
                .expect("resolve quoted integer with a leading zero"),
            resolver
                .resolve_string(crate::ingest::JsonString::new(br#""+1""#, "+1"))
                .expect("resolve quoted integer with a plus sign"),
            resolver
                .resolve_string(crate::ingest::JsonString::new(br#""-0""#, "-0"))
                .expect("resolve quoted signed zero"),
        ];
        let mut archive = OpenArchive::new(
            Cursor::new(Vec::<u8>::new()),
            WriterOptions::default().with_log_order(false),
        );
        for timestamp in rejected {
            let fields = [FieldRef::new(b"ts", ValueRef::Timestamp(timestamp))];
            assert!(matches!(
                archive.append_record(RecordRef::new(&fields)),
                Err(AppendError::Timestamp { .. })
            ));
            assert_eq!(0, archive.record_count());
            assert_eq!(0, archive.resident_bytes());
        }
    }

    #[test]
    fn nested_timestamp_path_preserves_missing_and_other_typed_values() {
        const SOURCE: &[u8] = concat!(
            r#"{"outer":{"ts":1700000000123},"ts":7}"#,
            "\n",
            r#"{"outer":{"other":2},"kind":"missing"}"#,
            "\n",
            r#"{"outer":{"ts":true},"kind":"boolean"}"#,
            "\n",
            r#"{"outer":{"ts":null},"kind":"null"}"#,
            "\n",
            r#"{"outer":{"ts":[1, 2]},"kind":"array"}"#,
            "\n",
        )
        .as_bytes();
        let source_size = u64::try_from(SOURCE.len()).expect("source size fits u64");
        let writer_options = WriterOptions::default()
            .with_log_order(false)
            .with_uncompressed_size(source_size);
        let mut actual = OpenArchive::new(Cursor::new(Vec::new()), writer_options);
        let resolver = JsonTimestampResolver::parse("outer.ts").expect("compile nested path");
        let mut reader = ParseManyReader::new(SOURCE, ParseManyOptions::default());
        {
            let base = JsonArchiveSink::new(&mut actual, JsonArchiveOptions::default());
            reader
                .read_to_end(&mut base.with_timestamp_resolver(&resolver))
                .expect("ingest mixed timestamp rows");
        }

        let mut expected = OpenArchive::new(Cursor::new(Vec::new()), writer_options);
        let nested = [FieldRef::new(
            b"ts",
            ValueRef::Timestamp(crate::writer::TimestampRef::new(
                1_700_000_000_123_000_000,
                "1700000000123",
                r"\L",
                "outer.ts",
            )),
        )];
        expected
            .append_record(RecordRef::new(&[
                FieldRef::new(b"outer", ValueRef::Object(&nested)),
                FieldRef::new(b"ts", ValueRef::I64(7)),
            ]))
            .expect("append nested timestamp");
        let missing = [FieldRef::new(b"other", ValueRef::I64(2))];
        expected
            .append_record(RecordRef::new(&[
                FieldRef::new(b"outer", ValueRef::Object(&missing)),
                FieldRef::new(b"kind", ValueRef::String(b"missing")),
            ]))
            .expect("append missing timestamp");
        let boolean = [FieldRef::new(b"ts", ValueRef::Bool(true))];
        expected
            .append_record(RecordRef::new(&[
                FieldRef::new(b"outer", ValueRef::Object(&boolean)),
                FieldRef::new(b"kind", ValueRef::String(b"boolean")),
            ]))
            .expect("append Boolean timestamp-path value");
        let null = [FieldRef::new(b"ts", ValueRef::Null)];
        expected
            .append_record(RecordRef::new(&[
                FieldRef::new(b"outer", ValueRef::Object(&null)),
                FieldRef::new(b"kind", ValueRef::String(b"null")),
            ]))
            .expect("append null timestamp-path value");
        let array = [FieldRef::new(
            b"ts",
            ValueRef::UnstructuredArray(UnstructuredArrayRef::new(b"[1, 2]")),
        )];
        expected
            .append_record(RecordRef::new(&[
                FieldRef::new(b"outer", ValueRef::Object(&array)),
                FieldRef::new(b"kind", ValueRef::String(b"array")),
            ]))
            .expect("append array timestamp-path value");
        assert_eq!(finished_bytes(expected), finished_bytes(actual));
    }

    #[test]
    fn timestamp_recognition_errors_are_located_and_transactional() {
        let writer_options = WriterOptions::default().with_log_order(false);
        let mut archive = OpenArchive::new(Cursor::new(Vec::new()), writer_options);
        let resolver = JsonTimestampResolver::parse("outer.ts").expect("compile nested path");
        {
            let base = JsonArchiveSink::new(&mut archive, JsonArchiveOptions::default());
            let mut sink = base.with_timestamp_resolver(&resolver);
            let mut invalid = ParseManyReader::new(
                br#"{"outer":{"ts":"not-a-time"},"kind":1}"#.as_slice(),
                ParseManyOptions::default(),
            );
            let error = invalid
                .read_document(&mut sink)
                .expect_err("reject unsupported timestamp string");
            assert!(matches!(
                error,
                ParseManyReadError::Sink {
                    document_index: 0,
                    source: RecordEventAppendError::Source {
                        event_index: 1,
                        source: JsonRecordEventError::Timestamp {
                            json_event_index: 4,
                            ..
                        },
                    },
                    ..
                }
            ));
            assert_eq!(0, sink.archive().record_count());

            let mut valid = ParseManyReader::new(
                br#"{"outer":{"ts":1700000000123},"kind":2}"#.as_slice(),
                ParseManyOptions::default(),
            );
            valid
                .read_to_end(&mut sink)
                .expect("append valid row after rejected timestamp");
            assert_eq!(1, sink.archive().record_count());
        }

        let nested = [FieldRef::new(
            b"ts",
            ValueRef::Timestamp(crate::writer::TimestampRef::new(
                1_700_000_000_123_000_000,
                "1700000000123",
                r"\L",
                "outer.ts",
            )),
        )];
        let mut expected = OpenArchive::new(Cursor::new(Vec::new()), writer_options);
        expected
            .append_record(RecordRef::new(&[
                FieldRef::new(b"outer", ValueRef::Object(&nested)),
                FieldRef::new(b"kind", ValueRef::I64(2)),
            ]))
            .expect("append expected valid row");
        assert_eq!(finished_bytes(expected), finished_bytes(archive));
    }

    #[test]
    fn timestamp_path_matching_is_iterative_at_the_reader_depth_bound() {
        const DEPTH: usize = 64;
        let mut path = String::new();
        let mut source = String::new();
        for index in 0..DEPTH {
            if 0 != index {
                path.push('.');
            }
            write!(path, "k{index}").expect("write path component");
            write!(source, "{{\"k{index}\":").expect("write nested object");
        }
        source.push_str("1700000000123");
        for _ in 0..DEPTH {
            source.push('}');
        }
        let resolver = JsonTimestampResolver::parse(&path).expect("compile deep path");
        let mut archive = OpenArchive::new(
            Cursor::new(Vec::<u8>::new()),
            WriterOptions::default().with_log_order(false),
        );
        let mut reader = ParseManyReader::new(source.as_bytes(), ParseManyOptions::default());
        {
            let base = JsonArchiveSink::new(&mut archive, JsonArchiveOptions::default());
            reader
                .read_to_end(&mut base.with_timestamp_resolver(&resolver))
                .expect("ingest deep timestamp without recursive path matching");
        }
        assert_eq!(1, archive.record_count());
    }

    #[test]
    fn parse_many_source_bytes_follow_post_record_archive_rotation() {
        const SOURCE: &[u8] = b"{\"n\":0}\n{\"n\":1}\n{\"n\":2}\n{\"n\":3}\n";
        let options = ArchiveSetOptions::new(WriterOptions::default(), 8);
        let mut archive_set = ArchiveSetWriter::new(
            CapturedArchives::default(),
            CapturedStats::default(),
            options,
        );
        let mut reader = ParseManyReader::new(SOURCE, ParseManyOptions::default());
        let stats = {
            let mut sink = JsonArchiveSetSink::for_source(
                &mut archive_set,
                JsonArchiveOptions::default(),
                ArchiveSourceContext::new("input.json", "creator"),
            )
            .expect("open rotating source");
            let stats = reader
                .read_to_end(&mut sink)
                .expect("ingest rotating input");
            sink.finish_source(stats.input_bytes())
                .expect("attribute trailing newline and close source");
            stats
        };
        assert_eq!(u64::try_from(SOURCE.len()).unwrap(), stats.input_bytes());

        let finished = archive_set.finish().expect("finish archive set");
        assert_eq!(5, finished.archive_count());
        let (published, callbacks) = finished.into_parts();
        assert_eq!(published.0, callbacks.0);
        assert_eq!(
            [7, 8, 8, 8, 1],
            callbacks
                .0
                .iter()
                .map(ArchiveSetStats::uncompressed_size)
                .collect::<Vec<_>>()
                .as_slice()
        );
        assert!(callbacks.0[..4].iter().all(ArchiveSetStats::is_split));
        assert!(!callbacks.0[4].is_split());
        assert_eq!(0, callbacks.0[4].record_count());
        for (split, stats) in callbacks.0.iter().enumerate() {
            let end = u64::from(split < 4);
            assert_json_source_range(stats, end, u64::try_from(split).unwrap());
        }
    }

    #[test]
    fn parse_many_accounting_advances_when_a_committed_rotation_needs_retry() {
        const SOURCE: &[u8] = b"{\"n\":0}\n{\"n\":1}\n";
        let options = ArchiveSetOptions::new(WriterOptions::default(), 8);
        let mut archive_set = ArchiveSetWriter::new(
            FailOnceArchives::default(),
            CapturedStats::default(),
            options,
        );
        let mut reader = ParseManyReader::new(SOURCE, ParseManyOptions::default());
        {
            let mut sink = JsonArchiveSetSink::for_source(
                &mut archive_set,
                JsonArchiveOptions::default(),
                ArchiveSourceContext::new("input.json", "creator"),
            )
            .expect("open retry source");
            let error = reader
                .read_document(&mut sink)
                .expect_err("first publication fails after record commit");
            assert!(matches!(
                error,
                ParseManyReadError::Sink {
                    source: ArchiveSetAppendError::ArchiveSet(
                        crate::writer::ArchiveSetError::Publication(_)
                    ),
                    ..
                }
            ));
            assert_eq!(7, sink.accounted_input_bytes());
            sink.archive_set_mut()
                .retry_pending()
                .expect("retry the encoded first archive");
            assert!(
                reader
                    .read_document(&mut sink)
                    .expect("append the second document")
            );
            assert_eq!(15, sink.accounted_input_bytes());
            assert!(
                !reader
                    .read_document(&mut sink)
                    .expect("consume trailing separator")
            );
            sink.finish_source(reader.stats().input_bytes())
                .expect("account trailing separator and close retry source");
        }
        let (published, callbacks) = archive_set
            .finish()
            .expect("finish after publication retry")
            .into_parts();
        assert_eq!(4, published.attempts);
        assert_eq!(published.stats, callbacks.0);
        assert_eq!(
            [7, 8, 1],
            callbacks
                .0
                .iter()
                .map(ArchiveSetStats::uncompressed_size)
                .collect::<Vec<_>>()
                .as_slice()
        );
        for (split, stats) in callbacks.0.iter().enumerate() {
            let end = u64::from(split < 2);
            assert_json_source_range(stats, end, u64::try_from(split).unwrap());
        }
    }

    #[test]
    fn no_retain_float_format_selects_an_ordinary_float_column() {
        let writer_options = WriterOptions::default().with_log_order(false);
        let mut actual = OpenArchive::new(Cursor::new(Vec::new()), writer_options);
        let mut reader = NdjsonReader::new(
            br#"{"value":1.2300}
"#
            .as_slice(),
            NdjsonOptions::default(),
        );
        {
            let mut sink = JsonArchiveSink::new(
                &mut actual,
                JsonArchiveOptions::default().with_retain_float_format(false),
            );
            reader.read_to_end(&mut sink).expect("ingest NDJSON");
        }

        let fields = [FieldRef::new(b"value", ValueRef::F64(1.23))];
        let mut expected = OpenArchive::new(Cursor::new(Vec::new()), writer_options);
        expected
            .append_record(RecordRef::new(&fields))
            .expect("append ordinary float");
        assert_eq!(finished_bytes(expected), finished_bytes(actual));
    }

    #[test]
    fn numeric_conversion_failure_does_not_mutate_the_archive() {
        let mut archive = OpenArchive::new(
            Cursor::new(Vec::<u8>::new()),
            WriterOptions::default().with_log_order(false),
        );
        let mut reader = NdjsonReader::new(
            br#"{"value":18446744073709551616}
"#
            .as_slice(),
            NdjsonOptions::default(),
        );
        let error = {
            let mut sink = JsonArchiveSink::new(&mut archive, JsonArchiveOptions::default());
            reader
                .read_record(&mut sink)
                .expect_err("out-of-range integer")
        };
        assert_eq!(0, archive.record_count());
        assert!(matches!(
            error,
            NdjsonReadError::Sink {
                record_index: 0,
                source: RecordEventAppendError::Source {
                    event_index: 0,
                    source: JsonRecordEventError::Number {
                        json_event_index: 2,
                        source: JsonNumberClassificationError::OutOfRange {
                            domain: JsonNumberDomain::Integer,
                        },
                    },
                },
                ..
            }
        ));
    }

    #[test]
    fn replayable_layout_mismatch_is_byte_exact_with_and_without_log_order() {
        const SOURCE: &[u8] = concat!(
            r#"{"a":1,"nested":{"x":"one"}}"#,
            r#"{"a":2,"nested":{"x":"two"}}"#,
            r#"{"nested":{"x":"three"},"a":3,"extra":true}"#,
            r#"{"nested":{"x":"four"},"a":4,"extra":false}"#,
            r#"{"nested":{"x":"five"},"a":5,"extra":true}"#,
        )
        .as_bytes();
        let source_size = u64::try_from(SOURCE.len()).expect("source size fits u64");

        for record_log_order in [false, true] {
            let writer_options = WriterOptions::default().with_log_order(record_log_order);
            let mut expected = OpenArchive::new(
                Cursor::new(Vec::new()),
                writer_options.with_uncompressed_size(source_size),
            );
            let mut expected_reader = ParseManyReader::new(SOURCE, ParseManyOptions::default());
            {
                let mut sink = JsonArchiveSink::new(&mut expected, JsonArchiveOptions::default());
                expected_reader
                    .read_to_end(&mut sink)
                    .expect("ingest through public iterator path");
            }
            let expected = finished_bytes(expected);

            let options = ArchiveSetOptions::new(writer_options, u64::MAX);
            let mut archive_set =
                ArchiveSetWriter::new(CapturedSfas::default(), CapturedStats::default(), options);
            let mut actual_reader = ParseManyReader::new(SOURCE, ParseManyOptions::default());
            {
                let mut sink =
                    JsonArchiveSetSink::new(&mut archive_set, JsonArchiveOptions::default());
                let stats = actual_reader
                    .read_to_end(&mut sink)
                    .expect("ingest through replayable archive-set path");
                assert_eq!(5, stats.documents());
                assert_eq!(source_size, sink.accounted_input_bytes());
            }
            let (published, callbacks) = archive_set
                .finish()
                .expect("finish replayable archive set")
                .into_parts();
            assert_eq!(1, published.0.len());
            assert_eq!(expected, published.0[0]);
            assert_eq!(1, callbacks.0.len());
        }
    }

    #[test]
    fn replayable_cached_layout_falls_back_for_root_and_nested_duplicates() {
        const ROOT_DUPLICATE: &[u8] =
            concat!(r#"{"a":1,"b":2}"#, r#"{"a":3,"b":4}"#, r#"{"a":5,"a":6}"#,).as_bytes();
        const NESTED_DUPLICATE: &[u8] = concat!(
            r#"{"outer":{"a":1,"b":2}}"#,
            r#"{"outer":{"a":3,"b":4}}"#,
            r#"{"outer":{"a":5,"a":6}}"#,
        )
        .as_bytes();

        for (source, expected_depth) in [(ROOT_DUPLICATE, 1), (NESTED_DUPLICATE, 2)] {
            let options =
                ArchiveSetOptions::new(WriterOptions::default().with_log_order(false), u64::MAX);
            let mut archive_set =
                ArchiveSetWriter::new(CapturedSfas::default(), CapturedStats::default(), options);
            let mut reader = ParseManyReader::new(source, ParseManyOptions::default());
            let mut sink = JsonArchiveSetSink::new(&mut archive_set, JsonArchiveOptions::default());
            assert!(reader.read_document(&mut sink).expect("first document"));
            assert!(reader.read_document(&mut sink).expect("second document"));
            let error = reader
                .read_document(&mut sink)
                .expect_err("duplicate field must use the full validator");
            assert!(matches!(
                error,
                ParseManyReadError::Sink {
                    document_index: 2,
                    source: ArchiveSetAppendError::ArchiveSet(ArchiveSetError::Append(
                        AppendError::DuplicateField {
                            object_depth,
                            previous_index: 0,
                            field_index: 1,
                        }
                    )),
                    ..
                } if object_depth == expected_depth
            ));
            assert_eq!(Some(2), sink.archive_set().current_record_count());
        }
    }

    #[test]
    fn replayable_cached_layout_preserves_writer_nesting_limits() {
        const SOURCE: &[u8] = concat!(
            r#"{"outer":{"value":1}}"#,
            r#"{"outer":{"value":2}}"#,
            r#"{"outer":{"inner":{"value":3}}}"#,
        )
        .as_bytes();
        let limits =
            WriterLimits::DEFAULT.with_record_limits(10, 2, u64::MAX, u64::MAX, u64::MAX, u64::MAX);
        let options = ArchiveSetOptions::new(
            WriterOptions::default()
                .with_log_order(false)
                .with_limits(limits),
            u64::MAX,
        );
        let mut archive_set =
            ArchiveSetWriter::new(CapturedSfas::default(), CapturedStats::default(), options);
        let mut reader = ParseManyReader::new(SOURCE, ParseManyOptions::default());
        let mut sink = JsonArchiveSetSink::new(&mut archive_set, JsonArchiveOptions::default());
        assert!(reader.read_document(&mut sink).expect("first document"));
        assert!(reader.read_document(&mut sink).expect("second document"));
        let error = reader
            .read_document(&mut sink)
            .expect_err("depth-three object must exceed the writer limit");
        assert!(matches!(
            error,
            ParseManyReadError::Sink {
                document_index: 2,
                source: ArchiveSetAppendError::ArchiveSet(ArchiveSetError::Append(
                    AppendError::LimitExceeded {
                        resource: AppendResource::NestingDepth,
                        actual: 3,
                        limit: 2,
                    }
                )),
                ..
            }
        ));
        assert_eq!(Some(2), sink.archive_set().current_record_count());
    }

    #[test]
    fn replayable_cached_layout_preserves_timestamp_errors_and_event_indexes() {
        const SOURCE: &[u8] = concat!(
            r#"{"ts":1700000000000}"#,
            r#"{"ts":1700000000001}"#,
            r#"{"ts":"not-a-time"}"#,
        )
        .as_bytes();
        let resolver = JsonTimestampResolver::parse("ts").expect("compile timestamp path");
        let options =
            ArchiveSetOptions::new(WriterOptions::default().with_log_order(false), u64::MAX);
        let mut archive_set =
            ArchiveSetWriter::new(CapturedSfas::default(), CapturedStats::default(), options);
        let mut reader = ParseManyReader::new(SOURCE, ParseManyOptions::default());
        let base = JsonArchiveSetSink::new(&mut archive_set, JsonArchiveOptions::default());
        let mut sink = base.with_timestamp_resolver(&resolver);
        assert!(reader.read_document(&mut sink).expect("first document"));
        assert!(reader.read_document(&mut sink).expect("second document"));
        let error = reader
            .read_document(&mut sink)
            .expect_err("invalid timestamp must remain a located source error");
        assert!(matches!(
            error,
            ParseManyReadError::Sink {
                document_index: 2,
                source: ArchiveSetAppendError::Source {
                    event_index: 0,
                    source: JsonRecordEventError::Timestamp {
                        json_event_index: 2,
                        ..
                    },
                },
                ..
            }
        ));
        assert_eq!(Some(2), sink.archive_set().current_record_count());
    }

    #[test]
    fn archive_append_errors_remain_distinct_from_conversion_errors() {
        let limits = crate::writer::WriterLimits::DEFAULT.with_record_limits(0, 1, 1, 1, 1, 1);
        let mut archive = OpenArchive::new(
            Cursor::new(Vec::<u8>::new()),
            WriterOptions::default()
                .with_log_order(false)
                .with_limits(limits),
        );
        let mut reader = NdjsonReader::new(
            br"{}
"
            .as_slice(),
            NdjsonOptions::default(),
        );
        let error = {
            let mut sink = JsonArchiveSink::new(&mut archive, JsonArchiveOptions::default());
            reader.read_record(&mut sink).expect_err("record limit")
        };
        assert!(matches!(
            error,
            NdjsonReadError::Sink {
                source: RecordEventAppendError::Append(AppendError::LimitExceeded { .. }),
                ..
            }
        ));
    }
}
