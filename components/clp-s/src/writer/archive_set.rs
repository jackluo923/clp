//! Record-aligned archive rotation without filesystem or process-global policy.
//!
//! The session owns a canonical in-memory archive at a time. A completed archive is encoded once,
//! then offered by reference to a caller-owned publisher. The same encoded member buffers can be
//! published as a directory archive or concatenated as an SFA. Publication and statistics
//! callbacks are separate retryable phases, and dropping a session never publishes output.
//!
//! A caller may bracket records with an [`ArchiveSourceContext`]. When log order is enabled, the
//! session emits the same archive-local range-index packet as the C++ ingestion path, including
//! canonical filename, source split number, archive-creator ID, and caller-supplied KV metadata.
//! Archive identity remains caller policy; [`ArchiveSetStats::archive_index`] is only a
//! deterministic session-local index.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::fmt::Display;
use std::fmt::Formatter;
use std::io::Write;
use std::sync::Arc;

use serde::Serialize;
use serde::Serializer;

use super::AppendError;
use super::EncodedDirectoryArchive;
use super::OpenDirectoryArchive;
use super::RecordEventAppendError;
use super::RecordEventRef;
use super::RecordRef;
use super::ReplayableRecordEventSource;
use super::WriterError;
use super::WriterOptions;
use crate::archive::DirectoryArchiveMember;
use crate::archive::RangeIndex;
use crate::archive::RangeIndexError;
use crate::archive::RangeIndexLimits;
use crate::archive::RangeIndexValue;

const ARCHIVE_CREATOR_ID_FIELD: &str = "_archive_creator_id";
const FILE_SPLIT_NUMBER_FIELD: &str = "_file_split_number";
const FILENAME_FIELD: &str = "_filename";

/// Metadata shared by every archive-local range produced from one input source.
///
/// The split number is owned by [`ArchiveSetWriter`] and starts at zero for each context. It
/// advances whenever that source crosses an archive boundary. Additional fields model KV-IR
/// user-defined metadata; the three reserved C++ field names cannot be replaced.
#[derive(Clone, Debug, PartialEq)]
pub struct ArchiveSourceContext {
    canonical_filename: String,
    archive_creator_id: String,
    fields: BTreeMap<String, RangeIndexValue>,
}

impl ArchiveSourceContext {
    /// Creates source context with no user-defined metadata.
    #[must_use]
    pub fn new(
        canonical_filename: impl Into<String>,
        archive_creator_id: impl Into<String>,
    ) -> Self {
        Self {
            canonical_filename: canonical_filename.into(),
            archive_creator_id: archive_creator_id.into(),
            fields: BTreeMap::new(),
        }
    }

    /// Returns the canonical filename stored in every range.
    #[must_use]
    pub fn canonical_filename(&self) -> &str {
        &self.canonical_filename
    }

    /// Returns the parser-invocation creator ID stored in every range.
    #[must_use]
    pub fn archive_creator_id(&self) -> &str {
        &self.archive_creator_id
    }

    /// Returns caller-supplied metadata, excluding the three reserved source fields.
    #[must_use]
    pub const fn fields(&self) -> &BTreeMap<String, RangeIndexValue> {
        &self.fields
    }

    /// Adds one user-defined metadata field.
    ///
    /// # Errors
    ///
    /// Returns [`ArchiveSourceContextError::ReservedField`] for a C++ source field, or
    /// [`ArchiveSourceContextError::DuplicateField`] when the key was already supplied.
    pub fn insert_field(
        &mut self,
        name: impl Into<String>,
        value: RangeIndexValue,
    ) -> Result<(), ArchiveSourceContextError> {
        let name = name.into();
        if is_reserved_source_field(&name) {
            return Err(ArchiveSourceContextError::ReservedField { name });
        }
        if self.fields.contains_key(&name) {
            return Err(ArchiveSourceContextError::DuplicateField { name });
        }
        self.fields.insert(name, value);
        Ok(())
    }

    /// Adds one user-defined field and returns the updated context.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::insert_field`].
    pub fn with_field(
        mut self,
        name: impl Into<String>,
        value: RangeIndexValue,
    ) -> Result<Self, ArchiveSourceContextError> {
        self.insert_field(name, value)?;
        Ok(self)
    }

    fn fields_for_split(&self, split_number: u64) -> BTreeMap<String, RangeIndexValue> {
        let mut fields = self.fields.clone();
        fields.insert(
            ARCHIVE_CREATOR_ID_FIELD.to_owned(),
            RangeIndexValue::String(self.archive_creator_id.clone()),
        );
        fields.insert(
            FILE_SPLIT_NUMBER_FIELD.to_owned(),
            RangeIndexValue::Unsigned(split_number),
        );
        fields.insert(
            FILENAME_FIELD.to_owned(),
            RangeIndexValue::String(self.canonical_filename.clone()),
        );
        fields
    }
}

const fn is_reserved_source_field(name: &str) -> bool {
    matches!(
        name.as_bytes(),
        b"_archive_creator_id" | b"_file_split_number" | b"_filename"
    )
}

/// Invalid caller-supplied source metadata.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ArchiveSourceContextError {
    /// A caller attempted to replace one of the three C++ source fields.
    ReservedField {
        /// Reserved field name.
        name: String,
    },
    /// A caller supplied the same user-defined key more than once.
    DuplicateField {
        /// Repeated field name.
        name: String,
    },
}

impl Display for ArchiveSourceContextError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReservedField { name } => {
                write!(formatter, "source metadata field {name:?} is reserved")
            }
            Self::DuplicateField { name } => {
                write!(formatter, "duplicate source metadata field {name:?}")
            }
        }
    }
}

impl Error for ArchiveSourceContextError {}

/// One half-open archive-local source range reported in archive statistics.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ArchiveSetRange {
    #[serde(rename = "e")]
    end_index: u64,
    #[serde(rename = "f")]
    fields: BTreeMap<String, RangeIndexValue>,
    #[serde(rename = "s")]
    start_index: u64,
}

impl ArchiveSetRange {
    /// Returns the inclusive archive-local start index.
    #[must_use]
    pub const fn start_index(&self) -> u64 {
        self.start_index
    }

    /// Returns the exclusive archive-local end index.
    #[must_use]
    pub const fn end_index(&self) -> u64 {
        self.end_index
    }

    /// Returns the source and caller-supplied metadata object.
    #[must_use]
    pub const fn fields(&self) -> &BTreeMap<String, RangeIndexValue> {
        &self.fields
    }

    /// Looks up one metadata field.
    #[must_use]
    pub fn field(&self, name: &str) -> Option<&RangeIndexValue> {
        self.fields.get(name)
    }
}

impl Serialize for RangeIndexValue {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer, {
        match self {
            Self::Null => serializer.serialize_unit(),
            Self::Boolean(value) => serializer.serialize_bool(*value),
            Self::Signed(value) => serializer.serialize_i64(*value),
            Self::Unsigned(value) => serializer.serialize_u64(*value),
            Self::Float(value) => serializer.serialize_f64(*value),
            Self::String(value) => serializer.serialize_str(value),
            Self::Binary(value) => serializer.serialize_bytes(value),
            Self::Array(values) => values.serialize(serializer),
            Self::Object(values) => values.serialize(serializer),
        }
    }
}

#[derive(Debug)]
struct ActiveSource {
    context: ArchiveSourceContext,
    split_number: u64,
    start_index: u64,
}

impl ActiveSource {
    fn close_at(&self, end_index: u64) -> ArchiveSetRange {
        ArchiveSetRange {
            end_index,
            fields: self.context.fields_for_split(self.split_number),
            start_index: self.start_index,
        }
    }
}

/// Configuration shared by every archive in one rotation session.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ArchiveSetOptions {
    writer: WriterOptions,
    target_encoded_size: u64,
    range_index_limits: RangeIndexLimits,
}

impl ArchiveSetOptions {
    /// Creates an archive-set policy.
    ///
    /// The target is the C++ `get_data_size` metric, not compressed output size or resident
    /// memory. A zero target rotates after every committed record.
    #[must_use]
    pub const fn new(writer: WriterOptions, target_encoded_size: u64) -> Self {
        Self {
            writer,
            target_encoded_size,
            range_index_limits: RangeIndexLimits::DEFAULT,
        }
    }

    /// Replaces limits applied to the serialized source range index before publication.
    #[must_use]
    pub const fn with_range_index_limits(mut self, limits: RangeIndexLimits) -> Self {
        self.range_index_limits = limits;
        self
    }

    /// Returns the options copied into each newly opened archive.
    #[must_use]
    pub const fn writer_options(self) -> WriterOptions {
        self.writer
    }

    /// Returns the post-record rotation target.
    #[must_use]
    pub const fn target_encoded_size(self) -> u64 {
        self.target_encoded_size
    }

    /// Returns the source range-index validation limits.
    #[must_use]
    pub const fn range_index_limits(self) -> RangeIndexLimits {
        self.range_index_limits
    }
}

/// Statistics for one successfully encoded archive.
///
/// Timestamp bounds describe the first committed authoritative timestamp range in deterministic
/// schema traversal order. They are both zero when no authoritative timestamp was committed.
#[derive(Clone, Debug, PartialEq)]
pub struct ArchiveSetStats {
    archive_index: u64,
    record_count: u64,
    encoded_data_size: u64,
    begin_timestamp: i64,
    end_timestamp: i64,
    uncompressed_size: u64,
    compressed_size: u64,
    is_split: bool,
    range_index: Arc<[ArchiveSetRange]>,
}

impl ArchiveSetStats {
    /// Returns the zero-based archive index within this session.
    #[must_use]
    pub const fn archive_index(&self) -> u64 {
        self.archive_index
    }

    /// Returns the number of committed records in this archive.
    #[must_use]
    pub const fn record_count(&self) -> u64 {
        self.record_count
    }

    /// Returns the exact C++ `get_data_size` rotation metric at finalization.
    #[must_use]
    pub const fn encoded_data_size(&self) -> u64 {
        self.encoded_data_size
    }

    /// Returns the beginning of the first authoritative timestamp range in epoch milliseconds.
    ///
    /// This is zero when the archive contains no authoritative timestamp.
    #[must_use]
    pub const fn begin_timestamp(&self) -> i64 {
        self.begin_timestamp
    }

    /// Returns the end of the first authoritative timestamp range in epoch milliseconds.
    ///
    /// This is zero when the archive contains no authoritative timestamp.
    #[must_use]
    pub const fn end_timestamp(&self) -> i64 {
        self.end_timestamp
    }

    /// Returns caller-accounted original source bytes stored in the archive header.
    #[must_use]
    pub const fn uncompressed_size(&self) -> u64 {
        self.uncompressed_size
    }

    /// Returns aggregate encoded bytes across the canonical physical members.
    #[must_use]
    pub const fn compressed_size(&self) -> u64 {
        self.compressed_size
    }

    /// Returns whether this archive closed because it reached the rotation target.
    #[must_use]
    pub const fn is_split(&self) -> bool {
        self.is_split
    }

    /// Returns source ranges in archive-local index order.
    ///
    /// This is empty when log-order recording is disabled or no source context was opened.
    #[must_use]
    pub fn range_index(&self) -> &[ArchiveSetRange] {
        &self.range_index
    }
}

/// One fully encoded archive offered to a caller-owned publisher.
#[derive(Debug)]
pub struct ArchiveSetArchive {
    stats: ArchiveSetStats,
    encoded: EncodedDirectoryArchive,
}

impl ArchiveSetArchive {
    /// Returns final archive statistics.
    #[must_use]
    pub fn stats(&self) -> ArchiveSetStats {
        self.stats.clone()
    }

    /// Returns the reusable canonical member representation.
    #[must_use]
    pub const fn encoded(&self) -> &EncodedDirectoryArchive {
        &self.encoded
    }

    /// Iterates all eight canonical physical members in SFA concatenation order.
    #[must_use = "iterators are lazy and do nothing unless consumed"]
    pub fn members(
        &self,
    ) -> impl ExactSizeIterator<Item = (DirectoryArchiveMember, &[u8])> + DoubleEndedIterator {
        self.encoded.members()
    }

    /// Writes an SFA by concatenating the already encoded canonical members.
    ///
    /// This does not re-encode records or allocate a complete second archive buffer. The caller is
    /// responsible for transactional publication if a partially written destination is unsafe.
    ///
    /// # Errors
    ///
    /// Returns the first output error.
    pub fn write_sfa<W: Write>(&self, output: &mut W) -> Result<(), std::io::Error> {
        for (_, contents) in self.members() {
            output.write_all(contents)?;
        }
        output.flush()
    }
}

/// Caller-owned publication callback for a finalized archive.
pub trait FinalizedArchiveSink {
    /// Publication failure.
    type Error;

    /// Publishes one already encoded archive.
    ///
    /// The archive remains owned by the session if this returns an error, so
    /// [`ArchiveSetWriter::retry_pending`] can offer it again without re-encoding records.
    /// Implementations should publish transactionally or make retry behavior explicit.
    ///
    /// # Errors
    ///
    /// Returns a caller-defined publication error.
    fn publish(&mut self, archive: &ArchiveSetArchive) -> Result<(), Self::Error>;
}

impl<F, E> FinalizedArchiveSink for F
where
    F: FnMut(&ArchiveSetArchive) -> Result<(), E>,
{
    type Error = E;

    fn publish(&mut self, archive: &ArchiveSetArchive) -> Result<(), Self::Error> {
        self(archive)
    }
}

/// Caller-owned callback invoked after an archive has been published successfully.
pub trait ArchiveSetStatsCallback {
    /// Callback failure.
    type Error;

    /// Observes final statistics after successful publication.
    ///
    /// # Errors
    ///
    /// Returns a caller-defined callback error. A retry resumes at this callback and never
    /// republishes the archive; callbacks that may fail after external side effects should be
    /// idempotent.
    fn on_archive(&mut self, stats: ArchiveSetStats) -> Result<(), Self::Error>;
}

impl<F, E> ArchiveSetStatsCallback for F
where
    F: FnMut(ArchiveSetStats) -> Result<(), E>,
{
    type Error = E;

    fn on_archive(&mut self, stats: ArchiveSetStats) -> Result<(), Self::Error> {
        self(stats)
    }
}

/// Infallible statistics callback for callers that do not need notifications.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoopArchiveSetStats;

impl ArchiveSetStatsCallback for NoopArchiveSetStats {
    type Error = std::convert::Infallible;

    fn on_archive(&mut self, _stats: ArchiveSetStats) -> Result<(), Self::Error> {
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FollowingState {
    Open,
    Finished,
}

#[derive(Debug)]
enum ArchiveSetState {
    Open(Box<OpenDirectoryArchive>),
    PublishPending {
        archive: Box<ArchiveSetArchive>,
        following: FollowingState,
    },
    StatsPending {
        stats: ArchiveSetStats,
        following: FollowingState,
    },
    Failed,
    Finished,
}

/// A library-first compression session that rotates only after complete committed records.
#[derive(Debug)]
#[must_use = "an archive-set session must be finished or explicitly aborted"]
pub struct ArchiveSetWriter<S, C> {
    sink: S,
    stats_callback: C,
    options: ArchiveSetOptions,
    state: ArchiveSetState,
    next_archive_index: u64,
    closed_ranges: Vec<ArchiveSetRange>,
    active_source: Option<ActiveSource>,
}

impl<S: FinalizedArchiveSink, C: ArchiveSetStatsCallback> ArchiveSetWriter<S, C> {
    /// Creates a session without invoking either callback.
    pub fn new(sink: S, stats_callback: C, options: ArchiveSetOptions) -> Self {
        let writer = options.writer.with_uncompressed_size(0);
        Self {
            sink,
            stats_callback,
            options,
            state: ArchiveSetState::Open(Box::new(OpenDirectoryArchive::new(writer))),
            next_archive_index: 0,
            closed_ranges: Vec::new(),
            active_source: None,
        }
    }

    /// Returns the session options.
    #[must_use]
    pub const fn options(&self) -> ArchiveSetOptions {
        self.options
    }

    /// Returns the index that will identify the current or pending archive.
    #[must_use]
    pub const fn next_archive_index(&self) -> u64 {
        self.next_archive_index
    }

    /// Returns the number of records in the open archive, if output is not pending or failed.
    #[must_use]
    pub const fn current_record_count(&self) -> Option<u64> {
        match &self.state {
            ArchiveSetState::Open(archive) => Some(archive.record_count()),
            ArchiveSetState::PublishPending { .. }
            | ArchiveSetState::StatsPending { .. }
            | ArchiveSetState::Failed
            | ArchiveSetState::Finished => None,
        }
    }

    /// Returns the open archive's exact rotation metric.
    #[must_use]
    pub const fn current_encoded_data_size(&self) -> Option<u64> {
        match &self.state {
            ArchiveSetState::Open(archive) => Some(archive.encoded_data_size()),
            ArchiveSetState::PublishPending { .. }
            | ArchiveSetState::StatsPending { .. }
            | ArchiveSetState::Failed
            | ArchiveSetState::Finished => None,
        }
    }

    /// Returns caller-accounted bytes in the open archive.
    #[must_use]
    pub const fn current_uncompressed_size(&self) -> Option<u64> {
        match &self.state {
            ArchiveSetState::Open(archive) => Some(archive.uncompressed_size()),
            ArchiveSetState::PublishPending { .. }
            | ArchiveSetState::StatsPending { .. }
            | ArchiveSetState::Failed
            | ArchiveSetState::Finished => None,
        }
    }

    /// Opens a source range at the current archive-local record index.
    ///
    /// The context remains active across automatic archive rotation. Each rotated archive closes
    /// its current range and the following archive starts a new range at zero with an incremented
    /// file split number. Empty ranges are retained, matching the C++ ingestion adapter.
    ///
    /// When log-order recording is disabled, this still enforces source lifecycle and split
    /// numbering but intentionally emits no range-index metadata.
    ///
    /// # Errors
    ///
    /// Returns a state error or [`ArchiveSetError::SourceAlreadyOpen`].
    pub fn begin_source(
        &mut self,
        context: ArchiveSourceContext,
    ) -> Result<(), ArchiveSetError<S::Error, C::Error>> {
        let start_index = self.open()?.record_count();
        if self.active_source.is_some() {
            return Err(ArchiveSetError::SourceAlreadyOpen);
        }
        self.active_source = Some(ActiveSource {
            context,
            split_number: 0,
            start_index,
        });
        Ok(())
    }

    /// Closes the active source at the current archive-local record index.
    ///
    /// # Errors
    ///
    /// Returns a state error or [`ArchiveSetError::NoSourceOpen`].
    pub fn end_source(&mut self) -> Result<(), ArchiveSetError<S::Error, C::Error>> {
        self.end_source_with_uncompressed_bytes(0)
    }

    /// Atomically attributes trailing source bytes and closes the active source.
    ///
    /// This is the source-lifecycle counterpart to the append methods' `source_bytes` argument.
    /// It is intended for separators, trailing whitespace, and stream terminators observed only
    /// after the final record. Both the byte counter and range closure remain unchanged if
    /// validation fails.
    ///
    /// # Errors
    ///
    /// Returns a checked-size or state error, including [`ArchiveSetError::NoSourceOpen`], without
    /// changing either the uncompressed-size statistic or source lifecycle.
    pub fn end_source_with_uncompressed_bytes(
        &mut self,
        bytes: u64,
    ) -> Result<(), ArchiveSetError<S::Error, C::Error>> {
        let end_index = self.open()?.record_count();
        if self.active_source.is_none() {
            return Err(ArchiveSetError::NoSourceOpen);
        }
        self.prepare_source_bytes(bytes)?;
        let source = self
            .active_source
            .take()
            .ok_or(ArchiveSetError::NoSourceOpen)?;
        let add_result = match self.open_mut() {
            Ok(archive) => archive
                .add_uncompressed_bytes(bytes)
                .map_err(ArchiveSetError::Finalization),
            Err(error) => Err(error),
        };
        if let Err(error) = add_result {
            self.active_source = Some(source);
            return Err(error);
        }
        if self.options.writer.records_log_order() {
            self.closed_ranges.push(source.close_at(end_index));
        }
        Ok(())
    }

    /// Returns the active source's current zero-based file split number.
    #[must_use]
    pub fn current_source_split_number(&self) -> Option<u64> {
        self.active_source
            .as_ref()
            .map(|source| source.split_number)
    }

    /// Atomically appends a record and its caller-attributed source bytes, then rotates if due.
    ///
    /// The byte addition is validated before record planning. A rejected record changes neither
    /// record state nor the uncompressed-size statistic. Once the record commits, a publication
    /// failure leaves an explicitly retryable pending archive.
    ///
    /// # Errors
    ///
    /// Returns an append, accounting, finalization, publication, callback, or state error.
    pub fn append_record_with_source_bytes(
        &mut self,
        record: RecordRef<'_>,
        source_bytes: u64,
    ) -> Result<(), ArchiveSetError<S::Error, C::Error>> {
        self.prepare_source_bytes(source_bytes)?;
        let archive = self.open_mut()?;
        archive
            .append_record(record)
            .map_err(ArchiveSetError::Append)?;
        archive
            .add_uncompressed_bytes(source_bytes)
            .map_err(ArchiveSetError::Finalization)?;
        self.rotate_if_due()
    }

    /// Appends a record with no source-byte attribution.
    ///
    /// # Errors
    ///
    /// Returns the same failures as [`Self::append_record_with_source_bytes`].
    pub fn append_record(
        &mut self,
        record: RecordRef<'_>,
    ) -> Result<(), ArchiveSetError<S::Error, C::Error>> {
        self.append_record_with_source_bytes(record, 0)
    }

    /// Atomically appends a flat record traversal and source-byte attribution.
    ///
    /// # Errors
    ///
    /// Returns an append, accounting, finalization, publication, callback, or state error.
    pub fn append_record_events_with_source_bytes<'record, I>(
        &mut self,
        events: I,
        source_bytes: u64,
    ) -> Result<(), ArchiveSetError<S::Error, C::Error>>
    where
        I: IntoIterator<Item = RecordEventRef<'record>>, {
        self.prepare_source_bytes(source_bytes)?;
        let archive = self.open_mut()?;
        archive
            .append_record_events(events)
            .map_err(ArchiveSetError::Append)?;
        archive
            .add_uncompressed_bytes(source_bytes)
            .map_err(ArchiveSetError::Finalization)?;
        self.rotate_if_due()
    }

    /// Appends a flat record traversal with no source-byte attribution.
    ///
    /// # Errors
    ///
    /// Returns the same failures as [`Self::append_record_events_with_source_bytes`].
    pub fn append_record_events<'record, I>(
        &mut self,
        events: I,
    ) -> Result<(), ArchiveSetError<S::Error, C::Error>>
    where
        I: IntoIterator<Item = RecordEventRef<'record>>, {
        self.append_record_events_with_source_bytes(events, 0)
    }

    /// Atomically consumes a fallible flat traversal and source-byte attribution.
    ///
    /// # Errors
    ///
    /// Returns a located source failure, or the same session failures as the infallible method.
    pub fn try_append_record_events_with_source_bytes<'record, I, E>(
        &mut self,
        events: I,
        source_bytes: u64,
    ) -> Result<(), ArchiveSetAppendError<E, S::Error, C::Error>>
    where
        I: IntoIterator<Item = Result<RecordEventRef<'record>, E>>, {
        self.prepare_source_bytes(source_bytes)
            .map_err(ArchiveSetAppendError::ArchiveSet)?;
        let archive = self.open_mut().map_err(ArchiveSetAppendError::ArchiveSet)?;
        archive
            .try_append_record_events(events)
            .map_err(|error| match error {
                RecordEventAppendError::Source {
                    event_index,
                    source,
                } => ArchiveSetAppendError::Source {
                    event_index,
                    source,
                },
                RecordEventAppendError::Append(source) => {
                    ArchiveSetAppendError::ArchiveSet(ArchiveSetError::Append(source))
                }
            })?;
        archive
            .add_uncompressed_bytes(source_bytes)
            .map_err(ArchiveSetError::Finalization)
            .map_err(ArchiveSetAppendError::ArchiveSet)?;
        self.rotate_if_due()
            .map_err(ArchiveSetAppendError::ArchiveSet)
    }

    #[allow(clippy::type_complexity)]
    pub(crate) fn try_append_replayable_record_events_with_source_bytes<'record, R>(
        &mut self,
        source: R,
        source_bytes: u64,
    ) -> Result<(), ArchiveSetAppendError<R::Error, S::Error, C::Error>>
    where
        R: ReplayableRecordEventSource<'record>, {
        self.prepare_source_bytes(source_bytes)
            .map_err(ArchiveSetAppendError::ArchiveSet)?;
        let archive = self.open_mut().map_err(ArchiveSetAppendError::ArchiveSet)?;
        archive
            .try_append_replayable_record_events(source)
            .map_err(|error| match error {
                RecordEventAppendError::Source {
                    event_index,
                    source,
                } => ArchiveSetAppendError::Source {
                    event_index,
                    source,
                },
                RecordEventAppendError::Append(source) => {
                    ArchiveSetAppendError::ArchiveSet(ArchiveSetError::Append(source))
                }
            })?;
        archive
            .add_uncompressed_bytes(source_bytes)
            .map_err(ArchiveSetError::Finalization)
            .map_err(ArchiveSetAppendError::ArchiveSet)?;
        self.rotate_if_due()
            .map_err(ArchiveSetAppendError::ArchiveSet)
    }

    /// Consumes a fallible traversal with no source-byte attribution.
    ///
    /// # Errors
    ///
    /// Returns the same failures as [`Self::try_append_record_events_with_source_bytes`].
    pub fn try_append_record_events<'record, I, E>(
        &mut self,
        events: I,
    ) -> Result<(), ArchiveSetAppendError<E, S::Error, C::Error>>
    where
        I: IntoIterator<Item = Result<RecordEventRef<'record>, E>>, {
        self.try_append_record_events_with_source_bytes(events, 0)
    }

    /// Adds separators, trailing whitespace, or other caller-attributed source bytes.
    ///
    /// This does not trigger rotation because no record was committed. Prefer an append method's
    /// `source_bytes` argument for bytes that precede the post-record threshold check.
    ///
    /// # Errors
    ///
    /// Returns a checked-size or state error without changing the statistic.
    pub fn add_uncompressed_bytes(
        &mut self,
        bytes: u64,
    ) -> Result<(), ArchiveSetError<S::Error, C::Error>> {
        self.prepare_source_bytes(bytes)?;
        self.open_mut()?
            .add_uncompressed_bytes(bytes)
            .map_err(ArchiveSetError::Finalization)?;
        Ok(())
    }

    /// Retries a pending publication or statistics callback.
    ///
    /// A statistics retry never republishes the archive.
    ///
    /// # Errors
    ///
    /// Returns the repeated callback failure or an unavailable-state error.
    pub fn retry_pending(&mut self) -> Result<(), ArchiveSetError<S::Error, C::Error>> {
        if !matches!(
            self.state,
            ArchiveSetState::PublishPending { .. } | ArchiveSetState::StatsPending { .. }
        ) {
            return Err(match self.state {
                ArchiveSetState::Open(_) => ArchiveSetError::NothingPending,
                ArchiveSetState::Failed => ArchiveSetError::Failed,
                ArchiveSetState::Finished => ArchiveSetError::Finished,
                ArchiveSetState::PublishPending { .. } | ArchiveSetState::StatsPending { .. } => {
                    unreachable!("pending states were matched before this branch")
                }
            });
        }
        self.drive_pending()
    }

    /// Explicitly finalizes the current archive, including a final empty archive after an exact
    /// boundary rotation, and returns both caller-owned callbacks.
    ///
    /// # Errors
    ///
    /// Returns a failure that owns the session. [`ArchiveSetFinishError::into_writer`] permits a
    /// pending sink or statistics callback to be retried. Encoding failures are terminal.
    pub fn finish(mut self) -> Result<FinishedArchiveSet<S, C>, ArchiveSetFinishError<S, C>> {
        let result = self.finish_inner();
        if let Err(error) = result {
            return Err(ArchiveSetFinishError {
                reason: error,
                writer: Box::new(self),
            });
        }
        let archive_count = self.next_archive_index;
        Ok(FinishedArchiveSet {
            sink: self.sink,
            stats_callback: self.stats_callback,
            archive_count,
        })
    }

    /// Abandons open or pending state without invoking either callback.
    #[must_use]
    pub fn abort(self) -> (S, C) {
        (self.sink, self.stats_callback)
    }

    fn finish_inner(&mut self) -> Result<(), ArchiveSetError<S::Error, C::Error>> {
        if matches!(
            self.state,
            ArchiveSetState::PublishPending { .. } | ArchiveSetState::StatsPending { .. }
        ) {
            self.drive_pending()?;
        }
        match self.state {
            ArchiveSetState::Open(_) => self.finalize_current(false, FollowingState::Finished),
            ArchiveSetState::Finished => Ok(()),
            ArchiveSetState::Failed
            | ArchiveSetState::PublishPending { .. }
            | ArchiveSetState::StatsPending { .. } => Err(self.state_error()),
        }
    }

    fn prepare_source_bytes(
        &self,
        source_bytes: u64,
    ) -> Result<(), ArchiveSetError<S::Error, C::Error>> {
        self.next_archive_index
            .checked_add(1)
            .ok_or(ArchiveSetError::SizeOverflow)?;
        let archive = self.open()?;
        archive
            .uncompressed_size()
            .checked_add(source_bytes)
            .ok_or(ArchiveSetError::SizeOverflow)?;
        Ok(())
    }

    const fn open(&self) -> Result<&OpenDirectoryArchive, ArchiveSetError<S::Error, C::Error>> {
        match &self.state {
            ArchiveSetState::Open(archive) => Ok(archive),
            ArchiveSetState::PublishPending { .. } => Err(ArchiveSetError::PublicationPending),
            ArchiveSetState::StatsPending { .. } => Err(ArchiveSetError::StatisticsPending),
            ArchiveSetState::Failed => Err(ArchiveSetError::Failed),
            ArchiveSetState::Finished => Err(ArchiveSetError::Finished),
        }
    }

    const fn open_mut(
        &mut self,
    ) -> Result<&mut OpenDirectoryArchive, ArchiveSetError<S::Error, C::Error>> {
        match &mut self.state {
            ArchiveSetState::Open(archive) => Ok(archive),
            ArchiveSetState::PublishPending { .. } => Err(ArchiveSetError::PublicationPending),
            ArchiveSetState::StatsPending { .. } => Err(ArchiveSetError::StatisticsPending),
            ArchiveSetState::Failed => Err(ArchiveSetError::Failed),
            ArchiveSetState::Finished => Err(ArchiveSetError::Finished),
        }
    }

    fn rotate_if_due(&mut self) -> Result<(), ArchiveSetError<S::Error, C::Error>> {
        let archive = self.open()?;
        if archive.encoded_data_size() >= self.options.target_encoded_size {
            self.finalize_current(true, FollowingState::Open)
        } else {
            Ok(())
        }
    }

    fn finalize_current(
        &mut self,
        is_split: bool,
        following: FollowingState,
    ) -> Result<(), ArchiveSetError<S::Error, C::Error>> {
        let following_index = self
            .next_archive_index
            .checked_add(1)
            .ok_or(ArchiveSetError::SizeOverflow)?;
        if FollowingState::Open == following
            && let Some(source) = &self.active_source
        {
            source
                .split_number
                .checked_add(1)
                .ok_or(ArchiveSetError::SizeOverflow)?;
        }
        let state = std::mem::replace(&mut self.state, ArchiveSetState::Failed);
        let ArchiveSetState::Open(open) = state else {
            return Err(self.state_error());
        };
        let record_count = open.record_count();
        let encoded_data_size = open.encoded_data_size();
        let (begin_timestamp, end_timestamp) = open.timestamp_bounds();
        let uncompressed_size = open.uncompressed_size();
        let range_index = self.take_range_index(record_count);
        let range_index_packet = if range_index.is_empty() {
            None
        } else {
            let payload = rmp_serde::to_vec_named(range_index.as_ref())
                .map_err(ArchiveSetError::RangeIndexEncoding)?;
            let packet = RangeIndex::decode(payload, self.options.range_index_limits)
                .map_err(ArchiveSetError::RangeIndexValidation)?;
            packet
                .validate_record_domain(record_count)
                .map_err(ArchiveSetError::RangeIndexValidation)?;
            Some(packet)
        };
        let mut encoded = (*open).finish().map_err(ArchiveSetError::Finalization)?;
        if let Some(packet) = range_index_packet {
            encoded = encoded
                .with_range_index(
                    packet.encoded_bytes(),
                    self.options.writer.compression_level(),
                    self.options.writer.limits(),
                )
                .map_err(ArchiveSetError::Finalization)?;
        }
        let archive_stats = ArchiveSetStats {
            archive_index: self.next_archive_index,
            record_count,
            encoded_data_size,
            begin_timestamp,
            end_timestamp,
            uncompressed_size,
            compressed_size: encoded.total_size(),
            is_split,
            range_index,
        };
        self.state = ArchiveSetState::PublishPending {
            archive: Box::new(ArchiveSetArchive {
                stats: archive_stats,
                encoded,
            }),
            following,
        };
        let result = self.drive_pending();
        if result.is_ok() {
            debug_assert_eq!(following_index, self.next_archive_index);
        }
        result
    }

    fn take_range_index(&mut self, end_index: u64) -> Arc<[ArchiveSetRange]> {
        if self.options.writer.records_log_order() {
            if let Some(source) = &self.active_source {
                self.closed_ranges.push(source.close_at(end_index));
            }
        } else {
            debug_assert_eq!(0, self.closed_ranges.len());
        }
        Arc::from(std::mem::take(&mut self.closed_ranges))
    }

    fn drive_pending(&mut self) -> Result<(), ArchiveSetError<S::Error, C::Error>> {
        if let ArchiveSetState::PublishPending { archive, .. } = &self.state {
            self.sink
                .publish(archive)
                .map_err(ArchiveSetError::Publication)?;
            let state = std::mem::replace(&mut self.state, ArchiveSetState::Failed);
            let ArchiveSetState::PublishPending { archive, following } = state else {
                unreachable!("publication state was checked before callback")
            };
            self.state = ArchiveSetState::StatsPending {
                stats: archive.stats,
                following,
            };
        }
        if let ArchiveSetState::StatsPending { stats, .. } = &self.state {
            self.stats_callback
                .on_archive(stats.clone())
                .map_err(ArchiveSetError::Statistics)?;
            let state = std::mem::replace(&mut self.state, ArchiveSetState::Failed);
            let ArchiveSetState::StatsPending { following, .. } = state else {
                unreachable!("statistics state was checked before callback")
            };
            self.next_archive_index = self
                .next_archive_index
                .checked_add(1)
                .ok_or(ArchiveSetError::SizeOverflow)?;
            self.state = match following {
                FollowingState::Open => {
                    if let Some(source) = &mut self.active_source {
                        source.split_number = source
                            .split_number
                            .checked_add(1)
                            .expect("source split overflow was checked before finalization");
                        source.start_index = 0;
                    }
                    ArchiveSetState::Open(Box::new(OpenDirectoryArchive::new(
                        self.options.writer.with_uncompressed_size(0),
                    )))
                }
                FollowingState::Finished => ArchiveSetState::Finished,
            };
        }
        Ok(())
    }

    const fn state_error(&self) -> ArchiveSetError<S::Error, C::Error> {
        match self.state {
            ArchiveSetState::Open(_) | ArchiveSetState::Finished => ArchiveSetError::Finished,
            ArchiveSetState::PublishPending { .. } => ArchiveSetError::PublicationPending,
            ArchiveSetState::StatsPending { .. } => ArchiveSetError::StatisticsPending,
            ArchiveSetState::Failed => ArchiveSetError::Failed,
        }
    }
}

/// Archive-set failure not originating in a fallible record traversal.
#[derive(Debug)]
#[non_exhaustive]
pub enum ArchiveSetError<S, C> {
    /// Record validation or resource planning failed atomically.
    Append(AppendError),
    /// Source-byte, archive-index, or session accounting overflowed.
    SizeOverflow,
    /// Source ranges could not be serialized as the v0.5 `MessagePack` packet.
    RangeIndexEncoding(rmp_serde::encode::Error),
    /// Source ranges violated structural, value, or caller-configured packet limits.
    RangeIndexValidation(RangeIndexError),
    /// Encoding failed before publication; the session is terminal.
    Finalization(WriterError),
    /// The finalized-archive publisher failed; encoded bytes remain pending.
    Publication(S),
    /// The post-publication statistics callback failed and remains pending.
    Statistics(C),
    /// A prior publication failure must be retried or explicitly aborted.
    PublicationPending,
    /// A prior statistics failure must be retried or explicitly aborted.
    StatisticsPending,
    /// A source context is already active in the open archive.
    SourceAlreadyOpen,
    /// No source context is active in the open archive.
    NoSourceOpen,
    /// No publication or statistics callback is pending.
    NothingPending,
    /// A prior encoding failure made the session unusable.
    Failed,
    /// The session was already finished.
    Finished,
}

impl<S, C> ArchiveSetError<S, C> {
    /// Returns whether the triggering record committed before this failure.
    ///
    /// Finalization and either external callback run only after record and source-byte commit.
    /// Append, accounting, and state failures occur before commit. This distinction lets streaming
    /// adapters advance input accounting after resolving a pending publication or statistics
    /// callback without replaying a record.
    #[must_use]
    pub const fn record_committed(&self) -> bool {
        matches!(
            self,
            Self::RangeIndexEncoding(_)
                | Self::RangeIndexValidation(_)
                | Self::Finalization(_)
                | Self::Publication(_)
                | Self::Statistics(_)
        )
    }
}

impl<S: Display, C: Display> Display for ArchiveSetError<S, C> {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Append(source) => write!(formatter, "archive record append failed: {source}"),
            Self::SizeOverflow => formatter.write_str("archive-set size counter overflow"),
            Self::RangeIndexEncoding(source) => {
                write!(formatter, "archive range-index encoding failed: {source}")
            }
            Self::RangeIndexValidation(source) => {
                write!(formatter, "archive range-index validation failed: {source}")
            }
            Self::Finalization(source) => {
                write!(formatter, "archive finalization failed: {source}")
            }
            Self::Publication(source) => write!(formatter, "archive publication failed: {source}"),
            Self::Statistics(source) => {
                write!(formatter, "archive statistics callback failed: {source}")
            }
            Self::PublicationPending => formatter.write_str("archive publication is pending retry"),
            Self::StatisticsPending => {
                formatter.write_str("archive statistics callback is pending retry")
            }
            Self::SourceAlreadyOpen => formatter.write_str("an archive source is already open"),
            Self::NoSourceOpen => formatter.write_str("no archive source is open"),
            Self::NothingPending => formatter.write_str("archive set has no pending callback"),
            Self::Failed => formatter.write_str("archive set failed during finalization"),
            Self::Finished => formatter.write_str("archive set is already finished"),
        }
    }
}

impl<S: Error + 'static, C: Error + 'static> Error for ArchiveSetError<S, C> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Append(source) => Some(source),
            Self::RangeIndexEncoding(source) => Some(source),
            Self::RangeIndexValidation(source) => Some(source),
            Self::Finalization(source) => Some(source),
            Self::Publication(source) => Some(source),
            Self::Statistics(source) => Some(source),
            Self::SizeOverflow
            | Self::PublicationPending
            | Self::StatisticsPending
            | Self::SourceAlreadyOpen
            | Self::NoSourceOpen
            | Self::NothingPending
            | Self::Failed
            | Self::Finished => None,
        }
    }
}

/// Failure while consuming a caller-owned fallible record traversal.
#[derive(Debug)]
#[non_exhaustive]
pub enum ArchiveSetAppendError<E, S, C> {
    /// The event source failed before the record committed.
    Source {
        /// Zero-based failing event index.
        event_index: usize,
        /// Caller-owned source error.
        source: E,
    },
    /// Archive planning, rotation, publication, or callback failed.
    ArchiveSet(ArchiveSetError<S, C>),
}

impl<E, S, C> ArchiveSetAppendError<E, S, C> {
    /// Returns whether the record committed before this failure.
    #[must_use]
    pub const fn record_committed(&self) -> bool {
        match self {
            Self::Source { .. } => false,
            Self::ArchiveSet(source) => source.record_committed(),
        }
    }
}

impl<E: Display, S: Display, C: Display> Display for ArchiveSetAppendError<E, S, C> {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Source {
                event_index,
                source,
            } => write!(
                formatter,
                "record event source failed at event {event_index}: {source}"
            ),
            Self::ArchiveSet(source) => Display::fmt(source, formatter),
        }
    }
}

impl<E: Error + 'static, S: Error + 'static, C: Error + 'static> Error
    for ArchiveSetAppendError<E, S, C>
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Source { source, .. } => Some(source),
            Self::ArchiveSet(source) => Some(source),
        }
    }
}

/// Consuming finalization failure that preserves the session for explicit retry or abort.
pub struct ArchiveSetFinishError<S: FinalizedArchiveSink, C: ArchiveSetStatsCallback> {
    reason: ArchiveSetError<S::Error, C::Error>,
    writer: Box<ArchiveSetWriter<S, C>>,
}

impl<S, C> fmt::Debug for ArchiveSetFinishError<S, C>
where
    S: FinalizedArchiveSink,
    C: ArchiveSetStatsCallback,
    S::Error: fmt::Debug,
    C::Error: fmt::Debug,
{
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ArchiveSetFinishError")
            .field("reason", &self.reason)
            .finish_non_exhaustive()
    }
}

impl<S: FinalizedArchiveSink, C: ArchiveSetStatsCallback> ArchiveSetFinishError<S, C> {
    /// Returns the failure reason.
    #[must_use]
    pub const fn reason(&self) -> &ArchiveSetError<S::Error, C::Error> {
        &self.reason
    }

    /// Recovers the session. Only pending publication/statistics failures are retryable.
    pub fn into_writer(self) -> ArchiveSetWriter<S, C> {
        *self.writer
    }
}

impl<S, C> Display for ArchiveSetFinishError<S, C>
where
    S: FinalizedArchiveSink,
    C: ArchiveSetStatsCallback,
    S::Error: Display,
    C::Error: Display,
{
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        Display::fmt(&self.reason, formatter)
    }
}

impl<S, C> Error for ArchiveSetFinishError<S, C>
where
    S: FinalizedArchiveSink,
    C: ArchiveSetStatsCallback,
    S::Error: Error + 'static,
    C::Error: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.reason)
    }
}

/// A completed session and its still caller-owned callbacks.
#[derive(Debug)]
pub struct FinishedArchiveSet<S, C> {
    sink: S,
    stats_callback: C,
    archive_count: u64,
}

impl<S, C> FinishedArchiveSet<S, C> {
    /// Returns the number of published archives, including the required final empty archive.
    #[must_use]
    pub const fn archive_count(&self) -> u64 {
        self.archive_count
    }

    /// Returns the publisher.
    #[must_use]
    pub const fn sink(&self) -> &S {
        &self.sink
    }

    /// Returns the statistics callback.
    #[must_use]
    pub const fn stats_callback(&self) -> &C {
        &self.stats_callback
    }

    /// Returns both caller-owned callbacks.
    #[must_use]
    pub fn into_parts(self) -> (S, C) {
        (self.sink, self.stats_callback)
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::io::Cursor;

    use super::*;
    use crate::ExtractionMode;
    use crate::ExtractionOptions;
    use crate::archive::ArchiveCatalogLimits;
    use crate::archive::MetadataLimits;
    use crate::archive::SingleFileArchiveReader;
    use crate::extract_jsonl;
    use crate::writer::FieldRef;
    use crate::writer::TimestampRef;
    use crate::writer::ValueRef;
    use crate::writer::WriterLimits;

    #[derive(Debug)]
    struct PublishedArchive {
        stats: ArchiveSetStats,
        sfa: Vec<u8>,
    }

    #[derive(Debug, Default)]
    struct MemorySink {
        attempts: usize,
        failures_remaining: usize,
        archives: Vec<PublishedArchive>,
    }

    impl FinalizedArchiveSink for MemorySink {
        type Error = io::Error;

        fn publish(&mut self, archive: &ArchiveSetArchive) -> Result<(), Self::Error> {
            self.attempts += 1;
            if 0 < self.failures_remaining {
                self.failures_remaining -= 1;
                return Err(io::Error::other("injected publication failure"));
            }
            let mut sfa = Vec::new();
            archive.write_sfa(&mut sfa)?;
            assert_eq!(
                archive.encoded().total_size(),
                archive.stats().compressed_size()
            );
            assert_eq!(
                archive.encoded().total_size(),
                archive
                    .members()
                    .map(|(_, bytes)| u64::try_from(bytes.len()).expect("member size fits u64"))
                    .sum::<u64>()
            );
            self.archives.push(PublishedArchive {
                stats: archive.stats(),
                sfa,
            });
            Ok(())
        }
    }

    #[derive(Debug, Default)]
    struct StatsCollector {
        attempts: usize,
        failures_remaining: usize,
        stats: Vec<ArchiveSetStats>,
    }

    impl ArchiveSetStatsCallback for StatsCollector {
        type Error = io::Error;

        fn on_archive(&mut self, stats: ArchiveSetStats) -> Result<(), Self::Error> {
            self.attempts += 1;
            if 0 < self.failures_remaining {
                self.failures_remaining -= 1;
                return Err(io::Error::other("injected statistics failure"));
            }
            self.stats.push(stats);
            Ok(())
        }
    }

    fn options(target: u64, log_order: bool) -> ArchiveSetOptions {
        ArchiveSetOptions::new(WriterOptions::default().with_log_order(log_order), target)
    }

    fn append_four_rows(
        writer: &mut ArchiveSetWriter<MemorySink, StatsCollector>,
    ) -> Result<(), ArchiveSetError<io::Error, io::Error>> {
        for (value, source_bytes) in (0_i64..4).zip([7_u64, 8, 8, 8]) {
            let fields = [FieldRef::new(b"n", ValueRef::I64(value))];
            writer.append_record_with_source_bytes(RecordRef::new(&fields), source_bytes)?;
        }
        writer.add_uncompressed_bytes(1)
    }

    fn run_four_rows(target: u64, log_order: bool) -> (MemorySink, StatsCollector) {
        let mut writer = ArchiveSetWriter::new(
            MemorySink::default(),
            StatsCollector::default(),
            options(target, log_order),
        );
        append_four_rows(&mut writer).expect("append four rows");
        writer.finish().expect("finish archive set").into_parts()
    }

    fn assert_stats(
        actual: &[ArchiveSetStats],
        records: &[u64],
        encoded_sizes: &[u64],
        uncompressed_sizes: &[u64],
        split: &[bool],
    ) {
        assert_eq!(records.len(), actual.len());
        assert_eq!(records.len(), encoded_sizes.len());
        assert_eq!(records.len(), uncompressed_sizes.len());
        assert_eq!(records.len(), split.len());
        for (index, archive) in actual.iter().enumerate() {
            assert_eq!(
                u64::try_from(index).expect("index fits u64"),
                archive.archive_index()
            );
            assert_eq!(records[index], archive.record_count());
            assert_eq!(encoded_sizes[index], archive.encoded_data_size());
            assert_eq!(0, archive.begin_timestamp());
            assert_eq!(0, archive.end_timestamp());
            assert_eq!(uncompressed_sizes[index], archive.uncompressed_size());
            assert_eq!(split[index], archive.is_split());
            assert!(0 < archive.compressed_size());
            assert_eq!(archive.range_index(), []);
        }
    }

    fn read_range_index(sfa: &[u8]) -> RangeIndex {
        let mut reader = SingleFileArchiveReader::open(Cursor::new(sfa)).expect("open range SFA");
        reader
            .read_metadata(MetadataLimits::default())
            .expect("read range metadata")
            .range_index()
            .expect("range-index packet")
            .clone()
    }

    fn assert_source_range(
        range: &ArchiveSetRange,
        start: u64,
        end: u64,
        filename: &str,
        creator: &str,
        split: u64,
    ) {
        assert_eq!(start, range.start_index());
        assert_eq!(end, range.end_index());
        assert_eq!(
            Some(filename),
            range
                .field(FILENAME_FIELD)
                .and_then(RangeIndexValue::as_str)
        );
        assert_eq!(
            Some(creator),
            range
                .field(ARCHIVE_CREATOR_ID_FIELD)
                .and_then(RangeIndexValue::as_str)
        );
        assert_eq!(
            Some(split),
            range
                .field(FILE_SPLIT_NUMBER_FIELD)
                .and_then(RangeIndexValue::as_u64)
        );
    }

    #[test]
    fn source_context_rejects_reserved_and_duplicate_user_fields() {
        let context = ArchiveSourceContext::new("input.json", "creator");
        assert!(matches!(
            context
                .clone()
                .with_field(FILENAME_FIELD, RangeIndexValue::String("other".to_owned())),
            Err(ArchiveSourceContextError::ReservedField { .. })
        ));
        let context = context
            .with_field("tenant", RangeIndexValue::Unsigned(7))
            .expect("insert user field");
        assert!(matches!(
            context.with_field("tenant", RangeIndexValue::Unsigned(8)),
            Err(ArchiveSourceContextError::DuplicateField { .. })
        ));
    }

    #[test]
    fn source_lifecycle_errors_leave_the_active_range_unchanged() {
        let mut writer = ArchiveSetWriter::new(
            MemorySink::default(),
            StatsCollector::default(),
            options(u64::MAX, true),
        );
        writer
            .begin_source(ArchiveSourceContext::new("first", "creator"))
            .expect("begin first source");
        assert!(matches!(
            writer.begin_source(ArchiveSourceContext::new("second", "creator")),
            Err(ArchiveSetError::SourceAlreadyOpen)
        ));
        writer.end_source().expect("end first source");
        assert!(matches!(
            writer.end_source(),
            Err(ArchiveSetError::NoSourceOpen)
        ));
        let (_, callback) = writer
            .finish()
            .expect("finish source lifecycle")
            .into_parts();
        assert_eq!(1, callback.stats[0].range_index().len());
        assert_source_range(
            &callback.stats[0].range_index()[0],
            0,
            0,
            "first",
            "creator",
            0,
        );
    }

    #[test]
    fn invalid_source_values_are_rejected_before_publication() {
        let source = ArchiveSourceContext::new("input.json", "creator")
            .with_field("bad", RangeIndexValue::Float(f64::NAN))
            .expect("source construction retains JSON-compatible shape");
        let mut writer = ArchiveSetWriter::new(
            MemorySink::default(),
            StatsCollector::default(),
            options(u64::MAX, true),
        );
        writer.begin_source(source).expect("begin source");
        writer.end_source().expect("end source");
        let failure = writer.finish().expect_err("non-finite range value");
        assert!(matches!(
            failure.reason(),
            ArchiveSetError::RangeIndexValidation(RangeIndexError::NonFiniteFloat { .. })
        ));
        let (sink, callback) = failure.into_writer().abort();
        assert_eq!(0, sink.attempts);
        assert_eq!(0, callback.attempts);
    }

    #[test]
    fn one_source_emits_exact_cpp_messagepack_packet_and_stats() {
        let mut writer = ArchiveSetWriter::new(
            MemorySink::default(),
            StatsCollector::default(),
            options(u64::MAX, true),
        );
        writer
            .begin_source(ArchiveSourceContext::new("input.json", "creator"))
            .expect("begin source");
        let fields = [FieldRef::new(b"n", ValueRef::I64(0))];
        writer
            .append_record(RecordRef::new(&fields))
            .expect("append source record");
        writer.end_source().expect("end source");
        let (sink, callback) = writer.finish().expect("finish source archive").into_parts();

        assert_eq!(1, callback.stats.len());
        assert_eq!(1, callback.stats[0].range_index().len());
        assert_source_range(
            &callback.stats[0].range_index()[0],
            0,
            1,
            "input.json",
            "creator",
            0,
        );
        assert_eq!(callback.stats[0], sink.archives[0].stats);
        assert_eq!(
            sink.archives[0].stats.compressed_size(),
            u64::try_from(sink.archives[0].sfa.len()).expect("SFA size fits u64")
        );

        let range_index = read_range_index(&sink.archives[0].sfa);
        assert_eq!(1, range_index.entries().len());
        assert_eq!(0..1, range_index.entries()[0].range());
        assert_eq!(
            callback.stats[0].range_index()[0].fields(),
            range_index.entries()[0].fields()
        );
        let expected_packet = [
            b"\x91\x83\xa1e\x01\xa1f\x83\xb3_archive_creator_id\xa7creator".as_slice(),
            b"\xb2_file_split_number\x00\xa9_filename\xaainput.json\xa1s\x00".as_slice(),
        ]
        .concat();
        assert_eq!(expected_packet, range_index.encoded_bytes());
    }

    #[test]
    fn source_context_makes_the_complete_log_order_archive_byte_identical_to_cpp() {
        const CPP_ARCHIVE: &[u8] =
            include_bytes!("../../tests/fixtures/sfa-v0.5.0-log-order-cpp.bin");
        const FILENAME: &str =
            "components/clp-s/tests/fixtures/sfa-v0.5.0-log-order-cpp-input.jsonl";
        const CREATOR: &str = "a2217fe8-aef3-4efe-93dc-625977a1d35a";
        let mut writer = ArchiveSetWriter::new(
            MemorySink::default(),
            StatsCollector::default(),
            options(u64::MAX, true),
        );
        writer
            .begin_source(ArchiveSourceContext::new(FILENAME, CREATOR))
            .expect("begin C++ oracle source");
        let rows = [
            (b"a".as_slice(), ValueRef::I64(10)),
            (b"b".as_slice(), ValueRef::Bool(true)),
            (b"a".as_slice(), ValueRef::I64(20)),
            (b"c".as_slice(), ValueRef::String(b"x")),
            (b"b".as_slice(), ValueRef::Bool(false)),
            (b"a".as_slice(), ValueRef::I64(30)),
        ];
        for (key, value) in rows {
            let fields = [FieldRef::new(key, value)];
            writer
                .append_record(RecordRef::new(&fields))
                .expect("append C++ oracle row");
        }
        writer
            .add_uncompressed_bytes(60)
            .expect("attribute C++ source bytes");
        writer.end_source().expect("end C++ oracle source");
        let (sink, callback) = writer.finish().expect("finish C++ oracle").into_parts();

        assert_eq!(CPP_ARCHIVE, sink.archives[0].sfa);
        assert_eq!(593, callback.stats[0].compressed_size());
        assert_eq!(60, callback.stats[0].uncompressed_size());
        assert_source_range(
            &callback.stats[0].range_index()[0],
            0,
            6,
            FILENAME,
            CREATOR,
            0,
        );
    }

    #[test]
    fn empty_and_adjacent_sources_retain_monotonic_ranges_and_user_metadata() {
        let mut writer = ArchiveSetWriter::new(
            MemorySink::default(),
            StatsCollector::default(),
            options(u64::MAX, true),
        );
        writer
            .begin_source(ArchiveSourceContext::new("empty-a", "creator"))
            .expect("begin first empty source");
        writer.end_source().expect("end first empty source");
        let source = ArchiveSourceContext::new("records", "creator")
            .with_field("tenant", RangeIndexValue::String("acme".to_owned()))
            .expect("add KV metadata");
        writer.begin_source(source).expect("begin record source");
        let fields = [FieldRef::new(b"n", ValueRef::I64(0))];
        writer
            .append_record(RecordRef::new(&fields))
            .expect("append record source");
        writer.end_source().expect("end record source");
        writer
            .begin_source(ArchiveSourceContext::new("empty-b", "creator"))
            .expect("begin second empty source");
        writer.end_source().expect("end second empty source");
        let (sink, callback) = writer.finish().expect("finish source ranges").into_parts();

        let ranges = callback.stats[0].range_index();
        assert_eq!(3, ranges.len());
        assert_source_range(&ranges[0], 0, 0, "empty-a", "creator", 0);
        assert_source_range(&ranges[1], 0, 1, "records", "creator", 0);
        assert_source_range(&ranges[2], 1, 1, "empty-b", "creator", 0);
        assert_eq!(
            Some("acme"),
            ranges[1].field("tenant").and_then(RangeIndexValue::as_str)
        );
        let decoded = read_range_index(&sink.archives[0].sfa);
        decoded
            .validate_record_domain(1)
            .expect("all source ranges fit archive domain");
        assert_eq!(3, decoded.entries().len());
    }

    #[test]
    fn active_source_rotation_and_both_callback_retries_preserve_split_ranges() {
        let sink = MemorySink {
            failures_remaining: 1,
            ..MemorySink::default()
        };
        let callback = StatsCollector {
            failures_remaining: 1,
            ..StatsCollector::default()
        };
        let mut writer = ArchiveSetWriter::new(sink, callback, options(16, true));
        writer
            .begin_source(ArchiveSourceContext::new("split.json", "creator"))
            .expect("begin split source");
        let fields = [FieldRef::new(b"n", ValueRef::I64(0))];
        assert!(matches!(
            writer.append_record(RecordRef::new(&fields)),
            Err(ArchiveSetError::Publication(_))
        ));
        assert_eq!(Some(0), writer.current_source_split_number());
        assert!(matches!(
            writer.retry_pending(),
            Err(ArchiveSetError::Statistics(_))
        ));
        assert_eq!(Some(0), writer.current_source_split_number());
        writer.retry_pending().expect("retry statistics callback");
        assert_eq!(Some(1), writer.current_source_split_number());
        writer.end_source().expect("close empty final split");
        let (sink, callback) = writer.finish().expect("finish split source").into_parts();

        assert_eq!(3, sink.attempts);
        assert_eq!(2, sink.archives.len());
        assert_eq!(3, callback.attempts);
        assert_eq!(2, callback.stats.len());
        assert_source_range(
            &callback.stats[0].range_index()[0],
            0,
            1,
            "split.json",
            "creator",
            0,
        );
        assert_source_range(
            &callback.stats[1].range_index()[0],
            0,
            0,
            "split.json",
            "creator",
            1,
        );
        assert!(callback.stats[0].is_split());
        assert!(!callback.stats[1].is_split());
        assert_eq!(callback.stats[0], sink.archives[0].stats);
        assert_eq!(callback.stats[1], sink.archives[1].stats);
        assert!(Arc::ptr_eq(
            &callback.stats[0].range_index,
            &sink.archives[0].stats.range_index
        ));
        for archive in &sink.archives {
            assert_eq!(1, read_range_index(&archive.sfa).entries().len());
        }
    }

    #[test]
    fn disabled_log_order_omits_ranges_while_preserving_source_split_lifecycle() {
        let mut writer = ArchiveSetWriter::new(
            MemorySink::default(),
            StatsCollector::default(),
            options(0, false),
        );
        writer
            .begin_source(ArchiveSourceContext::new("input.json", "creator"))
            .expect("begin unordered source");
        let fields = [FieldRef::new(b"n", ValueRef::I64(0))];
        writer
            .append_record(RecordRef::new(&fields))
            .expect("append and rotate unordered source");
        assert_eq!(Some(1), writer.current_source_split_number());
        writer.end_source().expect("end unordered source");
        let (sink, callback) = writer
            .finish()
            .expect("finish unordered source")
            .into_parts();
        assert_eq!(2, callback.stats.len());
        for (archive, stats) in sink.archives.iter().zip(&callback.stats) {
            assert_eq!(stats.range_index(), []);
            let mut reader = SingleFileArchiveReader::open(Cursor::new(&archive.sfa))
                .expect("open unordered SFA");
            assert!(
                reader
                    .read_metadata(MetadataLimits::default())
                    .expect("read unordered metadata")
                    .range_index()
                    .is_none()
            );
        }
    }

    #[test]
    fn ordered_thresholds_64_and_65_match_cpp_post_record_rotation() {
        let (boundary_sink, boundary_callback) = run_four_rows(64, true);
        assert_stats(
            &boundary_callback.stats,
            &[4, 0],
            &[64, 0],
            &[31, 1],
            &[true, false],
        );
        assert_eq!(
            boundary_callback.stats,
            boundary_sink
                .archives
                .iter()
                .map(|item| item.stats.clone())
                .collect::<Vec<_>>()
        );

        let (overshoot_sink, overshoot_callback) = run_four_rows(65, true);
        assert_stats(&overshoot_callback.stats, &[4], &[64], &[32], &[false]);
        assert_eq!(1, overshoot_sink.archives.len());
    }

    #[test]
    fn no_order_thresholds_32_33_and_zero_match_cpp_rotation_boundaries() {
        let (_, boundary) = run_four_rows(32, false);
        assert_stats(&boundary.stats, &[4, 0], &[32, 0], &[31, 1], &[true, false]);

        let (_, overshoot) = run_four_rows(33, false);
        assert_stats(&overshoot.stats, &[4], &[32], &[32], &[false]);

        let (_, zero) = run_four_rows(0, false);
        assert_stats(
            &zero.stats,
            &[1, 1, 1, 1, 0],
            &[8, 8, 8, 8, 0],
            &[7, 8, 8, 8, 1],
            &[true, true, true, true, false],
        );

        let (_, overshoots_one_record) = run_four_rows(17, false);
        assert_stats(
            &overshoots_one_record.stats,
            &[3, 1],
            &[24, 8],
            &[23, 9],
            &[true, false],
        );
    }

    #[test]
    fn timestamp_bounds_are_archive_local_and_round_negative_sub_milliseconds_outward() {
        let mut writer = ArchiveSetWriter::new(
            MemorySink::default(),
            StatsCollector::default(),
            options(0, false),
        );
        for (epoch_nanoseconds, lexeme, source_bytes) in [
            (-1_000_000_001, "-1.000000001", 239),
            (2_000_000_001, "2.000000001", 11),
        ] {
            let timestamp = TimestampRef::new(epoch_nanoseconds, lexeme, r"\E.\9", "ts");
            let fields = [FieldRef::new(b"ts", ValueRef::Timestamp(timestamp))];
            writer
                .append_record_with_source_bytes(RecordRef::new(&fields), source_bytes)
                .expect("append timestamp and rotate archive");
        }
        writer
            .add_uncompressed_bytes(1)
            .expect("attribute trailing stream byte");
        let (sink, callback) = writer
            .finish()
            .expect("finish timestamp archive set")
            .into_parts();

        assert_eq!(3, callback.stats.len());
        assert_eq!(callback.stats[0], sink.archives[0].stats);
        assert_eq!(callback.stats[1], sink.archives[1].stats);
        assert_eq!(callback.stats[2], sink.archives[2].stats);
        for (stats, expected) in callback.stats.iter().zip([
            (-1_001, -1_000, true, 239),
            (2_000, 2_001, true, 11),
            (0, 0, false, 1),
        ]) {
            assert_eq!(expected.0, stats.begin_timestamp());
            assert_eq!(expected.1, stats.end_timestamp());
            assert_eq!(expected.2, stats.is_split());
            assert_eq!(expected.3, stats.uncompressed_size());
        }
    }

    #[test]
    fn finish_always_publishes_an_initial_empty_archive() {
        let writer = ArchiveSetWriter::new(
            MemorySink::default(),
            StatsCollector::default(),
            options(64, true),
        );
        let finished = writer.finish().expect("finish empty archive set");
        assert_eq!(1, finished.archive_count());
        let (sink, callback) = finished.into_parts();
        assert_stats(&callback.stats, &[0], &[0], &[0], &[false]);
        assert_eq!(1, sink.archives.len());
    }

    #[test]
    fn every_rotated_archive_resets_schema_and_log_event_ids() {
        let (sink, callback) = run_four_rows(16, true);
        assert_stats(
            &callback.stats,
            &[1, 1, 1, 1, 0],
            &[16, 16, 16, 16, 0],
            &[7, 8, 8, 8, 1],
            &[true, true, true, true, false],
        );
        for (index, published) in sink.archives[..4].iter().enumerate() {
            let mut reader = SingleFileArchiveReader::open(Cursor::new(&published.sfa))
                .expect("open rotated SFA");
            let catalog = reader
                .read_catalog(ArchiveCatalogLimits::default())
                .expect("read rotated catalog");
            assert_eq!(1, catalog.schema_map().schemas().len());
            assert_eq!(0, catalog.schema_map().schemas()[0].id());
            let mut reader = SingleFileArchiveReader::open(Cursor::new(&published.sfa))
                .expect("reopen rotated SFA for extraction");
            let mut output = Vec::new();
            extract_jsonl(
                &mut reader,
                &mut output,
                ExtractionOptions::new(ExtractionMode::LogOrder),
            )
            .expect("extract one rotated row");
            assert_eq!(format!("{{\"n\":{index}}}\n").as_bytes(), output);
        }
    }

    #[test]
    fn identical_rotated_records_produce_identical_archive_local_ids_and_bytes() {
        let mut writer = ArchiveSetWriter::new(
            MemorySink::default(),
            StatsCollector::default(),
            options(1, true),
        );
        for _ in 0..2 {
            let fields = [FieldRef::new(b"value", ValueRef::String(b"plain"))];
            writer
                .append_record(RecordRef::new(&fields))
                .expect("append and rotate identical record");
        }
        let (sink, callback) = writer
            .finish()
            .expect("finish reset-ID archive set")
            .into_parts();
        assert_stats(
            &callback.stats,
            &[1, 1, 0],
            &[29, 29, 0],
            &[0, 0, 0],
            &[true, true, false],
        );
        assert_eq!(sink.archives[0].sfa, sink.archives[1].sfa);
    }

    #[test]
    fn dictionary_entry_metric_includes_ids_values_and_placeholder_positions() {
        let mut variable =
            OpenDirectoryArchive::new(WriterOptions::default().with_log_order(false));
        let fields = [FieldRef::new(b"value", ValueRef::String(b"plain"))];
        variable
            .append_record(RecordRef::new(&fields))
            .expect("append variable string");
        assert_eq!(8 + 5 + 8, variable.encoded_data_size());

        let mut logtype = OpenDirectoryArchive::new(WriterOptions::default().with_log_order(false));
        let fields = [FieldRef::new(b"value", ValueRef::String(b"count 42"))];
        logtype
            .append_record(RecordRef::new(&fields))
            .expect("append CLP string");
        // Dictionary ID + `count <integer-placeholder>` + one native-size placeholder position,
        // followed by the message descriptor and one encoded variable.
        assert_eq!(8 + 7 + 8 + 16, logtype.encoded_data_size());
    }

    #[test]
    fn rejected_records_and_sources_do_not_change_any_accounting() {
        let mut writer = ArchiveSetWriter::new(
            MemorySink::default(),
            StatsCollector::default(),
            options(0, false),
        );
        let duplicate = [
            FieldRef::new(b"n", ValueRef::I64(0)),
            FieldRef::new(b"n", ValueRef::I64(1)),
        ];
        assert!(matches!(
            writer.append_record_with_source_bytes(RecordRef::new(&duplicate), 7),
            Err(ArchiveSetError::Append(AppendError::DuplicateField { .. }))
        ));
        assert_eq!(Some(0), writer.current_record_count());
        assert_eq!(Some(0), writer.current_encoded_data_size());
        assert_eq!(Some(0), writer.current_uncompressed_size());

        let source_error = writer.try_append_record_events_with_source_bytes(
            [Err::<RecordEventRef<'_>, _>(io::Error::other("source"))],
            8,
        );
        assert!(matches!(
            source_error,
            Err(ArchiveSetAppendError::Source { event_index: 0, .. })
        ));
        assert_eq!(Some(0), writer.current_record_count());
        assert_eq!(Some(0), writer.current_encoded_data_size());
        assert_eq!(Some(0), writer.current_uncompressed_size());

        writer
            .add_uncompressed_bytes(u64::MAX)
            .expect("maximum source-byte counter");
        let fields = [FieldRef::new(b"n", ValueRef::I64(0))];
        assert!(matches!(
            writer.append_record_with_source_bytes(RecordRef::new(&fields), 1),
            Err(ArchiveSetError::SizeOverflow)
        ));
        assert_eq!(Some(0), writer.current_record_count());
        assert_eq!(Some(0), writer.current_encoded_data_size());
        assert_eq!(Some(u64::MAX), writer.current_uncompressed_size());
        assert!(writer.abort().0.archives.is_empty());
    }

    #[test]
    fn trailing_byte_attribution_and_source_closure_are_atomic() {
        let mut writer = ArchiveSetWriter::new(
            MemorySink::default(),
            StatsCollector::default(),
            options(u64::MAX, true),
        );
        assert!(matches!(
            writer.end_source_with_uncompressed_bytes(7),
            Err(ArchiveSetError::NoSourceOpen)
        ));
        assert_eq!(Some(0), writer.current_uncompressed_size());

        writer
            .begin_source(ArchiveSourceContext::new("input.json", "creator"))
            .expect("begin atomic source");
        writer
            .add_uncompressed_bytes(u64::MAX)
            .expect("fill source-byte counter");
        assert!(matches!(
            writer.end_source_with_uncompressed_bytes(1),
            Err(ArchiveSetError::SizeOverflow)
        ));
        assert_eq!(Some(u64::MAX), writer.current_uncompressed_size());
        assert_eq!(Some(0), writer.current_source_split_number());
        writer
            .end_source()
            .expect("source remained open after overflow");
        assert_eq!(None, writer.current_source_split_number());
        let _ = writer.abort();
    }

    #[test]
    fn publication_retry_reuses_encoded_bytes_and_stats_retry_does_not_republish() {
        let sink = MemorySink {
            failures_remaining: 1,
            ..MemorySink::default()
        };
        let mut writer = ArchiveSetWriter::new(sink, StatsCollector::default(), options(8, false));
        let fields = [FieldRef::new(b"n", ValueRef::I64(0))];
        assert!(matches!(
            writer.append_record_with_source_bytes(RecordRef::new(&fields), 7),
            Err(ArchiveSetError::Publication(_))
        ));
        assert_eq!(None, writer.current_record_count());
        writer.retry_pending().expect("retry publication");
        let (sink, callback) = writer.finish().expect("finish after retry").into_parts();
        assert_eq!(3, sink.attempts);
        assert_eq!(2, sink.archives.len());
        assert_eq!(2, callback.attempts);

        let callback = StatsCollector {
            failures_remaining: 1,
            ..StatsCollector::default()
        };
        let mut writer = ArchiveSetWriter::new(MemorySink::default(), callback, options(8, false));
        assert!(matches!(
            writer.append_record_with_source_bytes(RecordRef::new(&fields), 7),
            Err(ArchiveSetError::Statistics(_))
        ));
        assert_eq!(1, writer.sink.archives.len());
        writer.retry_pending().expect("retry statistics only");
        assert_eq!(1, writer.sink.archives.len());
        let (sink, callback) = writer
            .finish()
            .expect("finish after stats retry")
            .into_parts();
        assert_eq!(2, sink.attempts);
        assert_eq!(2, sink.archives.len());
        assert_eq!(3, callback.attempts);
        assert_eq!(2, callback.stats.len());
    }

    #[test]
    fn finalization_failure_is_terminal_and_does_not_touch_callbacks() {
        let limits = WriterLimits::new(u64::MAX, u64::MAX, u64::MAX, 0);
        let options = ArchiveSetOptions::new(
            WriterOptions::default()
                .with_log_order(false)
                .with_limits(limits),
            8,
        );
        let mut writer =
            ArchiveSetWriter::new(MemorySink::default(), StatsCollector::default(), options);
        let fields = [FieldRef::new(b"n", ValueRef::I64(0))];
        assert!(matches!(
            writer.append_record(RecordRef::new(&fields)),
            Err(ArchiveSetError::Finalization(
                WriterError::LimitExceeded { .. }
            ))
        ));
        assert!(matches!(
            writer.append_record(RecordRef::new(&fields)),
            Err(ArchiveSetError::Failed)
        ));
        let (sink, callback) = writer.abort();
        assert_eq!(0, sink.attempts);
        assert_eq!(0, callback.attempts);
    }
}
