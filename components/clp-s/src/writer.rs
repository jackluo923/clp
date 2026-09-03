//! Library-first construction of CLP structured archives.
//!
//! [`OpenArchive`] accepts borrowed, JSON-independent records, interns their schema shapes and CLP
//! dictionaries, and accumulates column-major tables. It deliberately owns its sink and is
//! consumed by [`OpenArchive::finish`]. Encoding and limit validation are completed before the
//! first sink write; arbitrary [`Write`] + [`Seek`] sinks cannot be rolled back after an I/O
//! failure. [`OpenDirectoryArchive`] shares the same encoder and exposes the eight canonical member
//! buffers or drives a caller-owned transactional member sink.
//!
//! The reader's metadata packet structs are currently private, so the small writer-side serde
//! structs below duplicate only their stable wire field names. They should move into a shared
//! internal metadata codec in a later shared-codec refactor.

mod archive_set;
mod array;
mod clp;
mod directory;
mod filesystem;
mod primitive;
mod retained_float;
mod timestamp;

use std::error::Error;
use std::fmt::Display;
use std::fmt::Formatter;
use std::fmt::{self};
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::io::{self};

pub use archive_set::ArchiveSetAppendError;
pub use archive_set::ArchiveSetArchive;
pub use archive_set::ArchiveSetError;
pub use archive_set::ArchiveSetFinishError;
pub use archive_set::ArchiveSetOptions;
pub use archive_set::ArchiveSetRange;
pub use archive_set::ArchiveSetStats;
pub use archive_set::ArchiveSetStatsCallback;
pub use archive_set::ArchiveSetWriter;
pub use archive_set::ArchiveSourceContext;
pub use archive_set::ArchiveSourceContextError;
pub use archive_set::FinalizedArchiveSink;
pub use archive_set::FinishedArchiveSet;
pub use archive_set::NoopArchiveSetStats;
pub use array::UnstructuredArrayError;
pub use array::UnstructuredArrayRef;
pub use array::UnstructuredArraySyntaxErrorKind;
pub use directory::DirectoryArchiveSink;
pub use directory::DirectoryWriterError;
pub use directory::EncodedDirectoryArchive;
pub use directory::FinishedDirectoryArchive;
pub use directory::OpenDirectoryArchive;
pub use filesystem::FsDirectoryArchiveSink;
pub use primitive::AppendDomain;
pub use primitive::AppendError;
pub use primitive::AppendResource;
pub use primitive::FieldRef;
pub use primitive::RecordEventAppendError;
pub(crate) use primitive::RecordEventConsumer;
pub use primitive::RecordEventError;
pub use primitive::RecordEventRef;
pub use primitive::RecordRef;
pub(crate) use primitive::ReplayableRecordEventSource;
pub use primitive::UnsupportedValue;
pub use primitive::ValueRef;
pub use retained_float::RetainedFloatError;
pub use retained_float::RetainedFloatRef;
use serde::Serialize;
#[doc(hidden)]
pub use timestamp::PrevalidatedTimestampRef;
pub use timestamp::TimestampError;
pub use timestamp::TimestampRef;

use self::primitive::PrimitiveArchive;
use crate::archive::ArchiveHeader;
use crate::archive::SFA_HEADER_SIZE;
use crate::archive::SFA_SECTION_NAMES;

const ARCHIVE_INFO_PACKET_TYPE: u8 = 0;
const ARCHIVE_FILE_INFO_PACKET_TYPE: u8 = 1;
const TIMESTAMP_DICTIONARY_PACKET_TYPE: u8 = 2;
const REQUIRED_METADATA_PACKET_COUNT: u8 = 3;
const PACKET_HEADER_SIZE: usize = 5;

const EMPTY_COUNT: [u8; size_of::<u64>()] = 0_u64.to_le_bytes();
const EMPTY_TABLE_METADATA: [u8; 3 * size_of::<u64>()] = [0; 3 * size_of::<u64>()];
const EMPTY_TIMESTAMP_DICTIONARY: [u8; 2 * size_of::<u64>()] = [0; 2 * size_of::<u64>()];

/// Resource limits applied while constructing an archive.
///
/// Record limits bound persistent payload memory and the entry counts that bound map/vector
/// overhead. Section and archive limits are checked during finalization.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WriterLimits {
    section_compressed: u64,
    metadata_decompressed: u64,
    metadata_compressed: u64,
    archive: u64,
    records: u64,
    nesting_depth: u64,
    schema_nodes: u64,
    schemas: u64,
    columns: u64,
    resident_bytes: u64,
    variable_dictionary_entries: u64,
    log_type_dictionary_entries: u64,
    array_dictionary_entries: u64,
    dictionary_entry_bytes: u64,
    dictionary_value_bytes: u64,
    encoded_variables_per_column: u64,
    total_encoded_variables: u64,
    timestamp_ranges: u64,
    timestamp_patterns: u64,
    timestamp_range_key_bytes: u64,
    timestamp_pattern_bytes: u64,
    timestamp_pattern_value_bytes: u64,
    timestamp_lexeme_bytes: u64,
    unstructured_array_lexeme_bytes: u64,
    unstructured_array_nesting_depth: u64,
    structured_array_schema_entries: u64,
}

impl WriterLimits {
    /// Conservative defaults suitable for bounded library use and small archive batches.
    pub const DEFAULT: Self = Self::new(
        Self::MEBIBYTE,
        Self::MEBIBYTE,
        Self::MEBIBYTE,
        4 * Self::MEBIBYTE,
    )
    .with_record_limits(
        1_048_576,
        256,
        1_048_576,
        131_072,
        4_194_304,
        256 * Self::MEBIBYTE,
    )
    .with_dictionary_limits(
        16 * 1024 * 1024,
        16 * 1024 * 1024,
        64 * Self::MEBIBYTE,
        1024 * Self::MEBIBYTE,
        128 * 1024 * 1024,
        128 * 1024 * 1024,
    )
    .with_timestamp_limits(
        65_536,
        1_048_576,
        Self::MEBIBYTE,
        64 * 1024,
        64 * Self::MEBIBYTE,
        64 * 1024,
    )
    .with_unstructured_array_limits(16 * 1024 * 1024, 64 * Self::MEBIBYTE, 256)
    .with_structured_array_schema_entry_limit(4_194_304);
    const MEBIBYTE: u64 = 1024 * 1024;

    /// Creates explicit writer limits, all measured in bytes.
    #[must_use]
    pub const fn new(
        max_section_compressed_size: u64,
        max_metadata_decompressed_size: u64,
        max_metadata_compressed_size: u64,
        max_archive_size: u64,
    ) -> Self {
        Self {
            section_compressed: max_section_compressed_size,
            metadata_decompressed: max_metadata_decompressed_size,
            metadata_compressed: max_metadata_compressed_size,
            archive: max_archive_size,
            records: u64::MAX,
            nesting_depth: u64::MAX,
            schema_nodes: u64::MAX,
            schemas: u64::MAX,
            columns: u64::MAX,
            resident_bytes: u64::MAX,
            variable_dictionary_entries: u64::MAX,
            log_type_dictionary_entries: u64::MAX,
            array_dictionary_entries: u64::MAX,
            dictionary_entry_bytes: u64::MAX,
            dictionary_value_bytes: u64::MAX,
            encoded_variables_per_column: u64::MAX,
            total_encoded_variables: u64::MAX,
            timestamp_ranges: u64::MAX,
            timestamp_patterns: u64::MAX,
            timestamp_range_key_bytes: u64::MAX,
            timestamp_pattern_bytes: u64::MAX,
            timestamp_pattern_value_bytes: u64::MAX,
            timestamp_lexeme_bytes: u64::MAX,
            unstructured_array_lexeme_bytes: u64::MAX,
            unstructured_array_nesting_depth: u64::MAX,
            structured_array_schema_entries: u64::MAX,
        }
    }

    /// Replaces limits used while interning schemas and accumulating column data.
    #[must_use]
    pub const fn with_record_limits(
        mut self,
        max_records: u64,
        max_nesting_depth: u64,
        max_schema_nodes: u64,
        max_schemas: u64,
        max_columns: u64,
        max_resident_bytes: u64,
    ) -> Self {
        self.records = max_records;
        self.nesting_depth = max_nesting_depth;
        self.schema_nodes = max_schema_nodes;
        self.schemas = max_schemas;
        self.columns = max_columns;
        self.resident_bytes = max_resident_bytes;
        self
    }

    /// Replaces limits for CLP dictionaries and encoded-variable columns.
    ///
    /// `max_dictionary_value_bytes` is cumulative across `/var.dict`, `/log.dict`, and
    /// `/array.dict`. Encoded variable limits count values, not bytes, and are additionally
    /// constrained by the fixed 40-bit descriptor domain.
    #[must_use]
    pub const fn with_dictionary_limits(
        mut self,
        max_variable_dictionary_entries: u64,
        max_log_type_dictionary_entries: u64,
        max_dictionary_entry_size: u64,
        max_dictionary_value_bytes: u64,
        max_encoded_variables_per_column: u64,
        max_total_encoded_variables: u64,
    ) -> Self {
        self.variable_dictionary_entries = max_variable_dictionary_entries;
        self.log_type_dictionary_entries = max_log_type_dictionary_entries;
        self.dictionary_entry_bytes = max_dictionary_entry_size;
        self.dictionary_value_bytes = max_dictionary_value_bytes;
        self.encoded_variables_per_column = max_encoded_variables_per_column;
        self.total_encoded_variables = max_total_encoded_variables;
        self
    }

    /// Replaces limits for exact timestamp values and timestamp-dictionary metadata.
    #[must_use]
    pub const fn with_timestamp_limits(
        mut self,
        max_ranges: u64,
        max_patterns: u64,
        max_range_key_bytes: u64,
        max_pattern_bytes: u64,
        max_total_pattern_bytes: u64,
        max_lexeme_bytes: u64,
    ) -> Self {
        self.timestamp_ranges = max_ranges;
        self.timestamp_patterns = max_patterns;
        self.timestamp_range_key_bytes = max_range_key_bytes;
        self.timestamp_pattern_bytes = max_pattern_bytes;
        self.timestamp_pattern_value_bytes = max_total_pattern_bytes;
        self.timestamp_lexeme_bytes = max_lexeme_bytes;
        self
    }

    /// Replaces limits for exact default-mode unstructured JSON arrays.
    ///
    /// Nesting depth counts the root array and every nested array or object. The lexeme limit is
    /// applied to each value before JSON validation and dictionary planning.
    #[must_use]
    pub const fn with_unstructured_array_limits(
        mut self,
        max_dictionary_entries: u64,
        max_lexeme_bytes: u64,
        max_nesting_depth: u64,
    ) -> Self {
        self.array_dictionary_entries = max_dictionary_entries;
        self.unstructured_array_lexeme_bytes = max_lexeme_bytes;
        self.unstructured_array_nesting_depth = max_nesting_depth;
        self
    }

    /// Replaces the per-record limit for flattened structured-array schema entries.
    ///
    /// The count includes nested object/array delimiters and structural nodes, including null and
    /// empty-container occurrences. Every individual delimiter body is additionally constrained
    /// by the format's fixed 24-bit length domain.
    #[must_use]
    pub const fn with_structured_array_schema_entry_limit(mut self, max_entries: u64) -> Self {
        self.structured_array_schema_entries = max_entries;
        self
    }

    /// Maximum compressed bytes accepted for any one canonical section.
    #[must_use]
    pub const fn max_section_compressed_size(self) -> u64 {
        self.section_compressed
    }

    /// Maximum bytes accepted before metadata compression.
    #[must_use]
    pub const fn max_metadata_decompressed_size(self) -> u64 {
        self.metadata_decompressed
    }

    /// Maximum bytes accepted after metadata compression.
    #[must_use]
    pub const fn max_metadata_compressed_size(self) -> u64 {
        self.metadata_compressed
    }

    /// Maximum total SFA size accepted.
    #[must_use]
    pub const fn max_archive_size(self) -> u64 {
        self.archive
    }

    /// Maximum records accumulated before finalization.
    #[must_use]
    pub const fn max_records(self) -> u64 {
        self.records
    }

    /// Maximum typed object/structured-array nesting depth, including the record's root object.
    #[must_use]
    pub const fn max_nesting_depth(self) -> u64 {
        self.nesting_depth
    }

    /// Maximum interned schema-tree nodes.
    #[must_use]
    pub const fn max_schema_nodes(self) -> u64 {
        self.schema_nodes
    }

    /// Maximum distinct record schemas and physical tables.
    #[must_use]
    pub const fn max_schemas(self) -> u64 {
        self.schemas
    }

    /// Maximum physical columns across all tables.
    #[must_use]
    pub const fn max_columns(self) -> u64 {
        self.columns
    }

    /// Maximum owned key, schema-entry, dictionary-value, and encoded-column payload bytes.
    #[must_use]
    pub const fn max_resident_bytes(self) -> u64 {
        self.resident_bytes
    }

    /// Maximum distinct values in `/var.dict`.
    #[must_use]
    pub const fn max_variable_dictionary_entries(self) -> u64 {
        self.variable_dictionary_entries
    }

    /// Maximum distinct escaped templates in `/log.dict`.
    #[must_use]
    pub const fn max_log_type_dictionary_entries(self) -> u64 {
        self.log_type_dictionary_entries
    }

    /// Maximum distinct escaped templates in `/array.dict`.
    #[must_use]
    pub const fn max_array_dictionary_entries(self) -> u64 {
        self.array_dictionary_entries
    }

    /// Maximum bytes in one variable or escaped-logtype dictionary entry.
    #[must_use]
    pub const fn max_dictionary_entry_size(self) -> u64 {
        self.dictionary_entry_bytes
    }

    /// Maximum cumulative value bytes across `/var.dict`, `/log.dict`, and `/array.dict`.
    #[must_use]
    pub const fn max_dictionary_value_bytes(self) -> u64 {
        self.dictionary_value_bytes
    }

    /// Maximum encoded variables accumulated in any one CLP string column.
    #[must_use]
    pub const fn max_encoded_variables_per_column(self) -> u64 {
        self.encoded_variables_per_column
    }

    /// Maximum encoded variables accumulated across all CLP string columns.
    #[must_use]
    pub const fn max_total_encoded_variables(self) -> u64 {
        self.total_encoded_variables
    }

    /// Maximum distinct timestamp range entries.
    #[must_use]
    pub const fn max_timestamp_ranges(self) -> u64 {
        self.timestamp_ranges
    }

    /// Maximum distinct resolved timestamp patterns.
    #[must_use]
    pub const fn max_timestamp_patterns(self) -> u64 {
        self.timestamp_patterns
    }

    /// Maximum UTF-8 bytes in one authoritative timestamp descriptor.
    #[must_use]
    pub const fn max_timestamp_range_key_bytes(self) -> u64 {
        self.timestamp_range_key_bytes
    }

    /// Maximum UTF-8 bytes in one resolved timestamp pattern.
    #[must_use]
    pub const fn max_timestamp_pattern_bytes(self) -> u64 {
        self.timestamp_pattern_bytes
    }

    /// Maximum cumulative resolved-pattern bytes retained by the writer.
    #[must_use]
    pub const fn max_timestamp_pattern_value_bytes(self) -> u64 {
        self.timestamp_pattern_value_bytes
    }

    /// Maximum UTF-8 bytes in one exact timestamp lexeme.
    #[must_use]
    pub const fn max_timestamp_lexeme_bytes(self) -> u64 {
        self.timestamp_lexeme_bytes
    }

    /// Maximum bytes in one exact unstructured-array JSON lexeme.
    #[must_use]
    pub const fn max_unstructured_array_lexeme_bytes(self) -> u64 {
        self.unstructured_array_lexeme_bytes
    }

    /// Maximum nested array/object containers in one unstructured-array JSON lexeme.
    #[must_use]
    pub const fn max_unstructured_array_nesting_depth(self) -> u64 {
        self.unstructured_array_nesting_depth
    }

    /// Maximum flattened structured-array schema entries in one record.
    #[must_use]
    pub const fn max_structured_array_schema_entries(self) -> u64 {
        self.structured_array_schema_entries
    }
}

impl Default for WriterLimits {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Configuration for one archive finalization.
///
/// Archive-global record-order columns are recorded by default, matching the C++ CLI. They may be
/// disabled when smaller archives and physical schema-table extraction are sufficient.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WriterOptions {
    compression_level: i32,
    limits: WriterLimits,
    minimum_packed_stream_size: u64,
    uncompressed_size: u64,
    record_log_order: bool,
    separate_columns_min_size: u64,
}

impl WriterOptions {
    const DEFAULT_MINIMUM_PACKED_STREAM_SIZE: u64 = 1024 * 1024;

    /// Creates options using the requested zstd compression level and default limits.
    #[must_use]
    pub const fn new(compression_level: i32) -> Self {
        Self {
            compression_level,
            limits: WriterLimits::DEFAULT,
            minimum_packed_stream_size: Self::DEFAULT_MINIMUM_PACKED_STREAM_SIZE,
            uncompressed_size: 0,
            record_log_order: true,
            separate_columns_min_size: 0,
        }
    }

    /// Writes each schema table at least this many uncompressed bytes as its own packed stream,
    /// with one zstd frame per column, so a reader can inflate only the columns a query needs.
    ///
    /// Zero, the default, keeps every table in the shared per-stream frame and produces the same
    /// bytes as before. Archives written with this set are readable only by readers that decode
    /// the separate-column section of the table metadata.
    #[must_use]
    pub const fn with_separate_columns_min_size(mut self, size: u64) -> Self {
        self.separate_columns_min_size = size;
        self
    }

    /// Returns the separate-column threshold; zero disables it.
    #[must_use]
    pub const fn separate_columns_min_size(self) -> u64 {
        self.separate_columns_min_size
    }

    /// Replaces the writer resource limits.
    #[must_use]
    pub const fn with_limits(mut self, limits: WriterLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Replaces the target minimum uncompressed size of a packed `/0` zstd frame.
    ///
    /// Like the C++ writer, a frame closes after its accumulated table size is strictly greater
    /// than this threshold. Individual schema tables are never split.
    #[must_use]
    pub const fn with_minimum_packed_stream_size(mut self, size: u64) -> Self {
        self.minimum_packed_stream_size = size;
        self
    }

    /// Records the caller-known size of the uncompressed source represented by this archive.
    ///
    /// Borrowed typed records do not retain their original serialization, so this defaults to
    /// zero. Supplying the source byte count makes the header match a source-ingestion writer.
    #[must_use]
    pub const fn with_uncompressed_size(mut self, size: u64) -> Self {
        self.uncompressed_size = size;
        self
    }

    /// Enables or disables archive-global record-order columns.
    ///
    /// Recording is enabled by default, matching the current C++ CLI. Disabling it preserves the
    /// smaller legacy writer output and makes log-order extraction unavailable.
    #[must_use]
    pub const fn with_log_order(mut self, enabled: bool) -> Self {
        self.record_log_order = enabled;
        self
    }

    /// Zstd compression level used for every frame.
    #[must_use]
    pub const fn compression_level(self) -> i32 {
        self.compression_level
    }

    /// Resource limits applied during finalization.
    #[must_use]
    pub const fn limits(self) -> WriterLimits {
        self.limits
    }

    /// Target minimum uncompressed bytes per packed table stream.
    #[must_use]
    pub const fn minimum_packed_stream_size(self) -> u64 {
        self.minimum_packed_stream_size
    }

    /// Caller-supplied uncompressed source bytes stored in the archive header.
    #[must_use]
    pub const fn uncompressed_size(self) -> u64 {
        self.uncompressed_size
    }

    /// Returns whether archive-global record order is recorded.
    #[must_use]
    pub const fn records_log_order(self) -> bool {
        self.record_log_order
    }
}

impl Default for WriterOptions {
    fn default() -> Self {
        Self::new(3)
    }
}

/// Archive destination that can consume a fallible flat borrowed record traversal.
///
/// The generic source keeps borrowed values zero-copy and reports parser/adapter failures before
/// archive state changes. Both single-file and directory archive writers implement this trait.
pub trait RecordEventAppender {
    /// Atomically appends one implicit-root object from a fallible event source.
    ///
    /// # Errors
    ///
    /// Returns the caller-owned source error with its event index, or a structured archive append
    /// error. No record state changes on failure.
    fn try_append_record_events<'record, I, E>(
        &mut self,
        events: I,
    ) -> Result<(), RecordEventAppendError<E>>
    where
        I: IntoIterator<Item = Result<RecordEventRef<'record>, E>>;
}

/// An archive whose mutable writer state has not yet been finalized.
///
/// No bytes are written by [`OpenArchive::new`]. The sink must still be empty when `finish` begins,
/// preventing stale suffix bytes on sinks that cannot be truncated through [`Write`] + [`Seek`].
#[derive(Debug)]
#[must_use = "an open archive must be finished or explicitly aborted"]
pub struct OpenArchive<W> {
    output: W,
    options: WriterOptions,
    records: PrimitiveArchive,
}

impl<W> OpenArchive<W> {
    /// Creates an empty open archive without touching `output`.
    pub fn new(output: W, options: WriterOptions) -> Self {
        Self {
            output,
            options,
            records: PrimitiveArchive::default(),
        }
    }

    /// Validates and atomically appends one borrowed record.
    ///
    /// The record's data is copied into reusable column buffers; no borrow escapes this call.
    /// Validation and all fallible destination reservations complete before schema, table, record
    /// count, or resident-byte state changes.
    ///
    /// # Errors
    ///
    /// Returns a structured error for duplicate sibling fields, invalid or inconsistent retained
    /// floats or exact timestamps, non-finite ordinary floats, configured limits, fixed wire
    /// domains, checked arithmetic, or bounded allocation failure.
    pub fn append_record(&mut self, record: RecordRef<'_>) -> Result<(), AppendError> {
        self.records
            .append(record, self.options.limits, self.options.record_log_order)
    }

    /// Validates and atomically appends one flat borrowed record traversal.
    ///
    /// The root object is implicit. Nested objects use balanced [`RecordEventRef::ObjectStart`] and
    /// [`RecordEventRef::ObjectEnd`] events. This avoids a self-referential allocation when a
    /// streaming parser already exposes a flat traversal.
    ///
    /// # Errors
    ///
    /// Returns [`AppendError`] for an unbalanced traversal or any value, limit, domain, or
    /// allocation failure reported by [`Self::append_record`].
    pub fn append_record_events<'record, I>(&mut self, events: I) -> Result<(), AppendError>
    where
        I: IntoIterator<Item = RecordEventRef<'record>>, {
        self.records
            .append_events(events, self.options.limits, self.options.record_log_order)
    }

    /// Validates and atomically appends a fallible flat borrowed record traversal.
    ///
    /// # Errors
    ///
    /// Returns [`RecordEventAppendError::Source`] if the source fails while being consumed, or
    /// [`RecordEventAppendError::Append`] for archive validation or planning failures.
    pub fn try_append_record_events<'record, I, E>(
        &mut self,
        events: I,
    ) -> Result<(), RecordEventAppendError<E>>
    where
        I: IntoIterator<Item = Result<RecordEventRef<'record>, E>>, {
        self.records
            .try_append_events(events, self.options.limits, self.options.record_log_order)
    }

    /// Returns the number of successfully appended records.
    #[must_use]
    pub const fn record_count(&self) -> u64 {
        self.records.record_count()
    }

    /// Returns the number of distinct accumulated schema shapes.
    #[must_use]
    pub const fn schema_count(&self) -> usize {
        self.records.schema_count()
    }

    /// Returns owned key, schema-entry, dictionary-value, and encoded-column payload bytes.
    #[must_use]
    pub const fn resident_bytes(&self) -> u64 {
        self.records.resident_bytes()
    }

    /// Returns the C++ `get_data_size` archive-rotation metric.
    ///
    /// This is dictionary entry data plus bytes appended by encoded messages. It intentionally
    /// excludes schema, column headers, archive headers, metadata, and container overhead.
    #[must_use]
    pub const fn encoded_data_size(&self) -> u64 {
        self.records.encoded_data_size()
    }

    /// Adds caller-known source bytes to the archive header's uncompressed-size statistic.
    ///
    /// # Errors
    ///
    /// Returns [`WriterError::SizeOverflow`] without changing the statistic if it exceeds `u64`.
    pub fn add_uncompressed_bytes(&mut self, bytes: u64) -> Result<(), WriterError> {
        self.options.uncompressed_size = self
            .options
            .uncompressed_size
            .checked_add(bytes)
            .ok_or(WriterError::SizeOverflow)?;
        Ok(())
    }

    /// Returns caller-accounted source bytes for this open archive.
    #[must_use]
    pub const fn uncompressed_size(&self) -> u64 {
        self.options.uncompressed_size
    }

    /// Abandons this archive without writing and returns the sink.
    #[must_use]
    pub fn abort(self) -> W {
        self.output
    }
}

impl<W> RecordEventAppender for OpenArchive<W> {
    fn try_append_record_events<'record, I, E>(
        &mut self,
        events: I,
    ) -> Result<(), RecordEventAppendError<E>>
    where
        I: IntoIterator<Item = Result<RecordEventRef<'record>, E>>, {
        Self::try_append_record_events(self, events)
    }
}

impl<W: Write + Seek> OpenArchive<W> {
    /// Finalizes a canonical v0.5 SFA and returns the sink.
    ///
    /// The output contains the required metadata packets and all seven canonical sections. A
    /// zero-record writer preserves the canonical empty C++ byte stream. Nonempty writers emit one
    /// table per distinct schema and one or more bounded packed zstd streams.
    ///
    /// # Errors
    ///
    /// Returns an error if the sink is nonempty, `MessagePack` or zstd encoding fails, a checked
    /// size calculation or allocation fails, a configured limit is exceeded, or output I/O fails.
    pub fn finish(mut self) -> Result<FinishedArchive<W>, WriterError> {
        let output_size = self
            .output
            .seek(SeekFrom::End(0))
            .map_err(WriterError::Io)?;
        if 0 != output_size {
            return Err(WriterError::NonEmptyOutput {
                actual_size: output_size,
            });
        }

        let encoded = encode_records(&self.records, self.options)?;
        self.output
            .seek(SeekFrom::Start(0))
            .map_err(WriterError::Io)?;
        self.output
            .write_all(&[0_u8; SFA_HEADER_SIZE])
            .map_err(WriterError::Io)?;
        self.output
            .write_all(&encoded.metadata)
            .map_err(WriterError::Io)?;
        for section in &encoded.sections {
            self.output.write_all(section).map_err(WriterError::Io)?;
        }

        self.output
            .seek(SeekFrom::Start(0))
            .map_err(WriterError::Io)?;
        self.output
            .write_all(&encoded.header.encode())
            .map_err(WriterError::Io)?;
        self.output
            .seek(SeekFrom::Start(encoded.archive_size))
            .map_err(WriterError::Io)?;
        self.output.flush().map_err(WriterError::Io)?;

        Ok(FinishedArchive {
            output: self.output,
            header: encoded.header,
        })
    }
}

fn encode_records(
    records: &PrimitiveArchive,
    options: WriterOptions,
) -> Result<EncodedEmptyArchive, WriterError> {
    if 0 == records.record_count() {
        EncodedEmptyArchive::new(options)
    } else {
        let (sections, timestamp_dictionary) = records.encode_sections(options)?;
        EncodedEmptyArchive::from_sections_with_timestamp(options, sections, &timestamp_dictionary)
    }
}

/// A successfully finalized archive and its still-owned sink.
#[derive(Debug)]
pub struct FinishedArchive<W> {
    output: W,
    header: ArchiveHeader,
}

impl<W> FinishedArchive<W> {
    /// Returns the header that was backpatched into the output.
    #[must_use]
    pub const fn header(&self) -> &ArchiveHeader {
        &self.header
    }

    /// Consumes this result and returns the output sink.
    #[must_use]
    pub fn into_inner(self) -> W {
        self.output
    }
}

#[derive(Debug)]
struct EncodedEmptyArchive {
    header: ArchiveHeader,
    metadata: Vec<u8>,
    sections: [Vec<u8>; SFA_SECTION_NAMES.len()],
    archive_size: u64,
}

impl EncodedEmptyArchive {
    fn new(options: WriterOptions) -> Result<Self, WriterError> {
        let sections = encode_empty_sections(options)?;
        Self::from_sections(options, sections)
    }

    fn from_sections(
        options: WriterOptions,
        sections: [Vec<u8>; SFA_SECTION_NAMES.len()],
    ) -> Result<Self, WriterError> {
        Self::from_sections_with_timestamp(options, sections, &EMPTY_TIMESTAMP_DICTIONARY)
    }

    fn from_sections_with_timestamp(
        options: WriterOptions,
        sections: [Vec<u8>; SFA_SECTION_NAMES.len()],
        timestamp_dictionary: &[u8],
    ) -> Result<Self, WriterError> {
        let offsets = section_offsets(&sections)?;
        let metadata_payload = encode_metadata_payload(offsets, timestamp_dictionary)?;
        check_limit(
            WriterResource::DecompressedMetadata,
            len_u64(&metadata_payload)?,
            options.limits.metadata_decompressed,
        )?;
        let metadata = compress(&metadata_payload, options.compression_level)?;
        let metadata_size = len_u64(&metadata)?;
        check_limit(
            WriterResource::CompressedMetadata,
            metadata_size,
            options.limits.metadata_compressed,
        )?;
        let metadata_size_u32 =
            u32::try_from(metadata_size).map_err(|_| WriterError::SizeOverflow)?;
        let files_size = total_section_size(&sections)?;
        let archive_size = u64::try_from(SFA_HEADER_SIZE)
            .map_err(|_| WriterError::SizeOverflow)?
            .checked_add(metadata_size)
            .and_then(|size| size.checked_add(files_size))
            .ok_or(WriterError::SizeOverflow)?;
        check_limit(
            WriterResource::Archive,
            archive_size,
            options.limits.archive,
        )?;

        Ok(Self {
            header: ArchiveHeader::new(options.uncompressed_size, archive_size, metadata_size_u32),
            metadata,
            sections,
            archive_size,
        })
    }
}

fn encode_empty_sections(
    options: WriterOptions,
) -> Result<[Vec<u8>; SFA_SECTION_NAMES.len()], WriterError> {
    let sections = [
        compress(&EMPTY_COUNT, options.compression_level)?,
        compress(&EMPTY_COUNT, options.compression_level)?,
        compress(&EMPTY_TABLE_METADATA, options.compression_level)?,
        encode_empty_dictionary()?,
        encode_empty_dictionary()?,
        encode_empty_dictionary()?,
        Vec::new(),
    ];
    let resources = [
        WriterResource::SchemaTree,
        WriterResource::SchemaMap,
        WriterResource::TableMetadata,
        WriterResource::VariableDictionary,
        WriterResource::LogTypeDictionary,
        WriterResource::ArrayDictionary,
        WriterResource::Tables,
    ];
    for (section, resource) in sections.iter().zip(resources) {
        check_limit(
            resource,
            len_u64(section)?,
            options.limits.section_compressed,
        )?;
    }
    Ok(sections)
}

fn encode_empty_dictionary() -> Result<Vec<u8>, WriterError> {
    let capacity = EMPTY_COUNT.len();
    let mut section = Vec::new();
    section
        .try_reserve_exact(capacity)
        .map_err(|_| WriterError::AllocationFailed {
            requested: capacity,
        })?;
    section.extend_from_slice(&EMPTY_COUNT);
    Ok(section)
}

fn compress(input: &[u8], compression_level: i32) -> Result<Vec<u8>, WriterError> {
    zstd::stream::encode_all(input, compression_level).map_err(WriterError::Io)
}

fn section_offsets(
    sections: &[Vec<u8>; SFA_SECTION_NAMES.len()],
) -> Result<[u64; SFA_SECTION_NAMES.len()], WriterError> {
    let mut offsets = [0_u64; SFA_SECTION_NAMES.len()];
    let mut next_offset = 0_u64;
    for (index, section) in sections.iter().enumerate() {
        offsets[index] = next_offset;
        next_offset = next_offset
            .checked_add(len_u64(section)?)
            .ok_or(WriterError::SizeOverflow)?;
    }
    Ok(offsets)
}

fn total_section_size(sections: &[Vec<u8>; SFA_SECTION_NAMES.len()]) -> Result<u64, WriterError> {
    sections.iter().try_fold(0_u64, |total, section| {
        total
            .checked_add(len_u64(section)?)
            .ok_or(WriterError::SizeOverflow)
    })
}

#[derive(Serialize)]
struct ArchiveInfoPacket {
    num_segments: u64,
}

#[derive(Serialize)]
struct ArchiveFileInfoPacket<'a> {
    files: [ArchiveFileInfo<'a>; SFA_SECTION_NAMES.len()],
}

#[derive(Serialize)]
struct ArchiveFileInfo<'a> {
    #[serde(rename = "n")]
    name: &'a str,
    #[serde(rename = "o")]
    offset: u64,
}

fn encode_metadata_payload(
    offsets: [u64; SFA_SECTION_NAMES.len()],
    timestamp_dictionary: &[u8],
) -> Result<Vec<u8>, WriterError> {
    let archive_info = encode_named_packet(
        MetadataPacket::ArchiveInfo,
        &ArchiveInfoPacket { num_segments: 1 },
    )?;
    let files = std::array::from_fn(|index| ArchiveFileInfo {
        name: SFA_SECTION_NAMES[index],
        offset: offsets[index],
    });
    let archive_file_info = encode_named_packet(
        MetadataPacket::ArchiveFileInfo,
        &ArchiveFileInfoPacket { files },
    )?;
    let capacity = 1_usize
        .checked_add(PACKET_HEADER_SIZE)
        .and_then(|size| size.checked_add(archive_info.len()))
        .and_then(|size| size.checked_add(PACKET_HEADER_SIZE))
        .and_then(|size| size.checked_add(archive_file_info.len()))
        .and_then(|size| size.checked_add(PACKET_HEADER_SIZE))
        .and_then(|size| size.checked_add(timestamp_dictionary.len()))
        .ok_or(WriterError::SizeOverflow)?;
    let mut metadata = Vec::new();
    metadata
        .try_reserve_exact(capacity)
        .map_err(|_| WriterError::AllocationFailed {
            requested: capacity,
        })?;
    metadata.push(REQUIRED_METADATA_PACKET_COUNT);
    append_packet(&mut metadata, ARCHIVE_INFO_PACKET_TYPE, &archive_info)?;
    append_packet(
        &mut metadata,
        ARCHIVE_FILE_INFO_PACKET_TYPE,
        &archive_file_info,
    )?;
    append_packet(
        &mut metadata,
        TIMESTAMP_DICTIONARY_PACKET_TYPE,
        timestamp_dictionary,
    )?;
    Ok(metadata)
}

fn encode_named_packet<T: Serialize>(
    packet: MetadataPacket,
    value: &T,
) -> Result<Vec<u8>, WriterError> {
    rmp_serde::to_vec_named(value).map_err(|source| WriterError::MessagePack { packet, source })
}

fn append_packet(
    destination: &mut Vec<u8>,
    packet_type: u8,
    payload: &[u8],
) -> Result<(), WriterError> {
    let payload_size = u32::try_from(payload.len()).map_err(|_| WriterError::SizeOverflow)?;
    destination.push(packet_type);
    destination.extend_from_slice(&payload_size.to_le_bytes());
    destination.extend_from_slice(payload);
    Ok(())
}

fn len_u64(bytes: &[u8]) -> Result<u64, WriterError> {
    u64::try_from(bytes.len()).map_err(|_| WriterError::SizeOverflow)
}

const fn check_limit(resource: WriterResource, actual: u64, limit: u64) -> Result<(), WriterError> {
    if actual > limit {
        Err(WriterError::LimitExceeded {
            resource,
            actual,
            limit,
        })
    } else {
        Ok(())
    }
}

/// Metadata packet whose `MessagePack` serialization failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum MetadataPacket {
    /// Archive-wide segment information.
    ArchiveInfo,
    /// Canonical file names and relative offsets.
    ArchiveFileInfo,
}

impl Display for MetadataPacket {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::ArchiveInfo => formatter.write_str("archive-info"),
            Self::ArchiveFileInfo => formatter.write_str("archive-file-info"),
        }
    }
}

/// Bounded writer resource reported by [`WriterError::LimitExceeded`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum WriterResource {
    /// Compressed schema-tree section.
    SchemaTree,
    /// Compressed schema-map section.
    SchemaMap,
    /// Compressed table-metadata section.
    TableMetadata,
    /// Variable-dictionary section, including its raw count prefix.
    VariableDictionary,
    /// Logtype-dictionary section, including its raw count prefix.
    LogTypeDictionary,
    /// Array-logtype-dictionary section, including its raw count prefix.
    ArrayDictionary,
    /// Concatenated packed-table frames.
    Tables,
    /// Metadata bytes before zstd compression.
    DecompressedMetadata,
    /// Metadata zstd frame.
    CompressedMetadata,
    /// Complete single-file archive.
    Archive,
}

impl Display for WriterResource {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::SchemaTree => "schema-tree section",
            Self::SchemaMap => "schema-map section",
            Self::TableMetadata => "table-metadata section",
            Self::VariableDictionary => "variable-dictionary section",
            Self::LogTypeDictionary => "logtype-dictionary section",
            Self::ArrayDictionary => "array-dictionary section",
            Self::Tables => "packed-tables section",
            Self::DecompressedMetadata => "decompressed metadata",
            Self::CompressedMetadata => "compressed metadata",
            Self::Archive => "archive",
        };
        formatter.write_str(name)
    }
}

/// Failure to encode or finalize a structured single-file archive.
#[derive(Debug)]
#[non_exhaustive]
pub enum WriterError {
    /// The generic sink already contained bytes and cannot be safely truncated.
    NonEmptyOutput {
        /// Existing sink length.
        actual_size: u64,
    },
    /// A bounded resource exceeded its configured limit.
    LimitExceeded {
        /// Resource that exceeded its limit.
        resource: WriterResource,
        /// Actual byte count.
        actual: u64,
        /// Configured maximum byte count.
        limit: u64,
    },
    /// Checked size arithmetic or conversion overflowed.
    SizeOverflow,
    /// A bounded allocation could not be reserved.
    AllocationFailed {
        /// Bytes requested by the failed allocation.
        requested: usize,
    },
    /// A metadata packet could not be serialized as `MessagePack`.
    MessagePack {
        /// Packet being encoded.
        packet: MetadataPacket,
        /// `MessagePack` encoder error.
        source: rmp_serde::encode::Error,
    },
    /// Zstd encoding, output writing, seeking, or flushing failed.
    Io(io::Error),
}

impl Display for WriterError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::NonEmptyOutput { actual_size } => write!(
                formatter,
                "archive output must be empty, but contains {actual_size} bytes"
            ),
            Self::LimitExceeded {
                resource,
                actual,
                limit,
            } => write!(
                formatter,
                "{resource} size {actual} exceeds writer limit {limit}"
            ),
            Self::SizeOverflow => formatter.write_str("structured archive size overflow"),
            Self::AllocationFailed { requested } => write!(
                formatter,
                "failed to reserve {requested} bytes while encoding structured archive"
            ),
            Self::MessagePack { packet, source } => {
                write!(formatter, "failed to encode {packet} metadata: {source}")
            }
            Self::Io(error) => write!(formatter, "structured archive output I/O failed: {error}"),
        }
    }
}

impl Error for WriterError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::MessagePack { source, .. } => Some(source),
            Self::Io(error) => Some(error),
            Self::NonEmptyOutput { .. }
            | Self::LimitExceeded { .. }
            | Self::SizeOverflow
            | Self::AllocationFailed { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::io::Read;

    use super::*;
    use crate::ExtractionError;
    use crate::ExtractionMode;
    use crate::ExtractionOptions;
    use crate::ExtractionPlan;
    use crate::ExtractionPlanLimits;
    use crate::LOG_EVENT_IDX_KEY;
    use crate::LogOrderLocator;
    use crate::OrderedMergeError;
    use crate::RecordLimits;
    use crate::RecordProgram;
    use crate::RecordScratch;
    use crate::archive::ArchiveCatalogLimits;
    use crate::archive::ColumnData;
    use crate::archive::ColumnLimits;
    use crate::archive::ColumnTruncation;
    use crate::archive::DictionaryLimits;
    use crate::archive::LogTypeDictionaryEntry;
    use crate::archive::MetadataLimits;
    use crate::archive::NodeType;
    use crate::archive::PackedStreamLimits;
    use crate::archive::SchemaEntry;
    use crate::archive::SchemaMapLimits;
    use crate::archive::SchemaTreeLimits;
    use crate::archive::SingleFileArchiveReader;
    use crate::archive::TableMetadataLimits;
    use crate::archive::TimestampBounds;
    use crate::extract_jsonl;

    fn empty_archive() -> Vec<u8> {
        OpenArchive::new(
            Cursor::new(Vec::new()),
            WriterOptions::default().with_log_order(false),
        )
        .finish()
        .expect("write canonical empty archive")
        .into_inner()
        .into_inner()
    }

    fn primitive_archive(options: WriterOptions) -> Vec<u8> {
        let mut archive = OpenArchive::new(Cursor::new(Vec::new()), options.with_log_order(false));
        let metrics_one = [
            FieldRef::new(b"load", ValueRef::F64(1.25)),
            FieldRef::new(b"ok", ValueRef::Bool(true)),
        ];
        let first = [
            FieldRef::new(b"id", ValueRef::I64(-7)),
            FieldRef::new(b"metrics", ValueRef::Object(&metrics_one)),
            FieldRef::new(b"missing", ValueRef::Null),
        ];
        archive
            .append_record(RecordRef::new(&first))
            .expect("append first primitive record");

        let metrics_two = [
            FieldRef::new(b"load", ValueRef::F64(2.5)),
            FieldRef::new(b"ok", ValueRef::Bool(false)),
        ];
        let second = [
            FieldRef::new(b"missing", ValueRef::Null),
            FieldRef::new(b"metrics", ValueRef::Object(&metrics_two)),
            FieldRef::new(b"id", ValueRef::I64(42)),
        ];
        archive
            .append_record(RecordRef::new(&second))
            .expect("append reordered instance of the first schema");

        let third = [
            FieldRef::new(b"id", ValueRef::I64(9)),
            FieldRef::new(b"enabled", ValueRef::Bool(false)),
        ];
        archive
            .append_record(RecordRef::new(&third))
            .expect("append second schema");
        assert_eq!(3, archive.record_count());
        assert_eq!(2, archive.schema_count());
        assert!(archive.resident_bytes() > 0);

        archive
            .finish()
            .expect("finish primitive archive")
            .into_inner()
            .into_inner()
    }

    fn string_archive(options: WriterOptions) -> Vec<u8> {
        let rows: &[(&[u8], &[u8])] = &[
            (b"YScope", b"uid=0 CPU=99.99 user=YScope"),
            (b"a\tb", b"uid=-9223372036854775808 CPU=-00.00 user=face"),
            (b"YScope", b"plain words"),
            (b"YScope", b"literal \\ \x11 \x12 \x13 done"),
        ];
        let mut archive = OpenArchive::new(Cursor::new(Vec::new()), options.with_log_order(false));
        for (variable, clp) in rows {
            let fields = [
                FieldRef::new(b"v", ValueRef::String(variable)),
                FieldRef::new(b"c", ValueRef::String(clp)),
            ];
            archive
                .append_record(RecordRef::new(&fields))
                .expect("append string record");
        }
        archive
            .finish()
            .expect("finish string archive")
            .into_inner()
            .into_inner()
    }

    const RETAINED_FLOAT_SOURCE: &[u8] = concat!(
        "{\"formatted\":-0.00,\"fallback\":123456789.123456789}\n",
        "{\"formatted\":123456789.000,\"fallback\":123456789.123456700}\n",
        "{\"formatted\":0.00000000000000000001234567891234567,\"fallback\":123456789.123456789}\n",
        "{\"formatted\":1.234567891234567E+0009,\"fallback\":12.345e6}\n",
        "{\"formatted\":4.9406564584124654e-324,\"fallback\":1.0e00000}\n",
        "{\"formatted\":1.7976931348623157E308,\"fallback\":1.2345678912345679e+13}\n",
    )
    .as_bytes();

    const RETAINED_FLOAT_ROWS: &[(&[u8], &[u8])] = &[
        (b"-0.00", b"123456789.123456789"),
        (b"123456789.000", b"123456789.123456700"),
        (
            b"0.00000000000000000001234567891234567",
            b"123456789.123456789",
        ),
        (b"1.234567891234567E+0009", b"12.345e6"),
        (b"4.9406564584124654e-324", b"1.0e00000"),
        (b"1.7976931348623157E308", b"1.2345678912345679e+13"),
    ];

    fn retained_float_archive(options: WriterOptions) -> Vec<u8> {
        let mut archive = OpenArchive::new(Cursor::new(Vec::new()), options.with_log_order(false));
        for (formatted, fallback) in RETAINED_FLOAT_ROWS {
            let formatted_value = std::str::from_utf8(formatted)
                .expect("formatted token is UTF-8")
                .parse::<f64>()
                .expect("formatted token parses");
            let fallback_value = std::str::from_utf8(fallback)
                .expect("fallback token is UTF-8")
                .parse::<f64>()
                .expect("fallback token parses");
            let fields = [
                FieldRef::new(
                    b"formatted",
                    ValueRef::RetainedFloat(RetainedFloatRef::new(formatted_value, formatted)),
                ),
                FieldRef::new(
                    b"fallback",
                    ValueRef::RetainedFloat(RetainedFloatRef::new(fallback_value, fallback)),
                ),
            ];
            archive
                .append_record(RecordRef::new(&fields))
                .expect("append retained-float record");
        }
        archive
            .finish()
            .expect("finish retained-float archive")
            .into_inner()
            .into_inner()
    }

    const LOG_ORDER_SOURCE: &[u8] =
        include_bytes!("../tests/fixtures/sfa-v0.5.0-log-order-cpp-input.jsonl");
    const LOG_ORDER_UNORDERED: &[u8] =
        include_bytes!("../tests/fixtures/sfa-v0.5.0-log-order-cpp-unordered.jsonl");

    fn log_order_archive(options: WriterOptions) -> Vec<u8> {
        let mut archive = OpenArchive::new(Cursor::new(Vec::new()), options);
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
            archive
                .append_record(RecordRef::new(&fields))
                .expect("append log-order record");
        }
        archive
            .finish()
            .expect("finish log-order archive")
            .into_inner()
            .into_inner()
    }

    fn separate_column_archive(rows: i64) -> Vec<u8> {
        // Every table wide enough to have columns is stored one zstd frame per column.
        let mut archive = OpenArchive::new(
            Cursor::new(Vec::new()),
            WriterOptions::default().with_separate_columns_min_size(1),
        );
        for row in 0..rows {
            let fields = [FieldRef::new(b"n".as_slice(), ValueRef::I64(row))];
            archive
                .append_record(RecordRef::new(&fields))
                .expect("append separate-column record");
        }
        archive
            .finish()
            .expect("finish separate-column archive")
            .into_inner()
            .into_inner()
    }

    /// Reads the `n` column of stream 0, however much of it the read asked for.
    fn separate_column_values(
        catalog: &crate::archive::ArchiveCatalog,
        stream: &crate::archive::DecodedPackedStream,
    ) -> Vec<i64> {
        let mut tables = catalog
            .schema_tables(0, stream, ColumnLimits::default())
            .expect("open separate-column stream");
        let decoded = tables
            .next()
            .expect("stream holds one table")
            .expect("decode separate-column table");
        for column in decoded.table().columns() {
            if let ColumnData::Integer(values) = column.data() {
                return values.iter().collect();
            }
        }
        Vec::new()
    }

    #[test]
    fn separate_column_streams_agree_whole_projected_and_truncated() {
        let rows = 64_i64;
        let bytes = separate_column_archive(rows);
        let mut reader =
            SingleFileArchiveReader::open(Cursor::new(bytes)).expect("open separate-column archive");
        let catalog = reader
            .read_catalog(ArchiveCatalogLimits::default())
            .expect("read separate-column catalog");
        let table_metadata = catalog.table_metadata();
        assert!(
            table_metadata.separate_columns_for(0).is_some(),
            "the writer stored the table column by column"
        );
        let columns = table_metadata
            .separate_columns_for(0)
            .expect("stream 0 stores one frame per column")
            .columns()
            .len();
        assert_eq!(2, columns, "log order and the value column");
        let expected: Vec<i64> = (0..rows).collect();

        // Whole read.
        let whole = reader
            .read_packed_stream(
                catalog.metadata(),
                table_metadata,
                0,
                PackedStreamLimits::default(),
            )
            .expect("read the stream whole");
        assert_eq!(expected, separate_column_values(&catalog, &whole));

        // Projected read: both columns wanted, which is what a scan filtering on log order asks
        // for. The values must not move because the stream stores them column by column.
        let projected = reader
            .read_packed_stream_frames(
                catalog.metadata(),
                table_metadata,
                0,
                Some(&[true, true]),
                None,
                PackedStreamLimits::default(),
            )
            .expect("read the stream projected");
        assert_eq!(expected, separate_column_values(&catalog, &projected));

        // Truncated read: stop where the log-event index reaches half way. The value column then
        // holds exactly those rows, and they are the same values the whole read produced.
        let half = usize::try_from(rows / 2).expect("half fits usize");
        let truncated = reader
            .read_packed_stream_frames(
                catalog.metadata(),
                table_metadata,
                0,
                Some(&[true, true]),
                Some(ColumnTruncation {
                    log_order_column: 0,
                    limit: rows.cast_unsigned() / 2,
                    total_rows: usize::try_from(rows).expect("rows fit usize"),
                    truncatable: &[false, true],
                }),
                PackedStreamLimits::default(),
            )
            .expect("read the stream truncated");
        let values = separate_column_values(&catalog, &truncated);
        assert_eq!(half, values.len(), "only the rows the query can match");
        assert_eq!(expected[..half], values[..]);
    }

    const TIMESTAMP_SOURCE: &[u8] =
        include_bytes!("../tests/fixtures/sfa-v0.5.0-timestamps-cpp-input.jsonl");
    const DATE_TIMESTAMP_PATTERN: &str = r#""\Y-\m-\dT\H:\M:\S.\3""#;
    const TIMESTAMP_ROWS: &[(i64, &str, &str, i64)] = &[
        (
            1_422_752_523_004_000_000,
            r#""2015-02-01T01:02:03.004""#,
            DATE_TIMESTAMP_PATTERN,
            1,
        ),
        (1_700_000_000_123_000_000, "1700000000123", r"\L", 2),
        (
            1_422_752_524_004_000_000,
            r#""2015-02-01T01:02:04.004""#,
            DATE_TIMESTAMP_PATTERN,
            3,
        ),
        (1_700_000_001_123_000_000, "1700000001123", r"\L", 4),
    ];

    fn timestamp_archive(options: WriterOptions) -> Vec<u8> {
        let mut archive = OpenArchive::new(Cursor::new(Vec::new()), options.with_log_order(false));
        for &(epoch, lexeme, pattern, kind) in TIMESTAMP_ROWS {
            let fields = [
                FieldRef::new(
                    b"ts",
                    ValueRef::Timestamp(TimestampRef::new(epoch, lexeme, pattern, "ts")),
                ),
                FieldRef::new(b"kind", ValueRef::I64(kind)),
            ];
            archive
                .append_record(RecordRef::new(&fields))
                .expect("append exact timestamp record");
        }
        archive
            .finish()
            .expect("finish timestamp archive")
            .into_inner()
            .into_inner()
    }

    const UNSTRUCTURED_ARRAY_SOURCE: &[u8] =
        include_bytes!("../tests/fixtures/sfa-v0.5.0-unstructured-arrays-cpp-input.jsonl");
    const UNSTRUCTURED_ARRAY_ROWS: &[&[u8]] = &[
        b"[]",
        br#"[1,true,null,"x",{"k":"v"},[2,3]]"#,
        br#"[2,false,null,"y",{"k":"w"},[4,5]]"#,
        br#"[ -7, 12.50 , "user=face", {"n": 9} ]"#,
        br#"["slash\\\\marker","\u0011\u0012\u0013"]"#,
        br#"[[],{},[{"x":[]}]]"#,
    ];

    fn unstructured_array_archive(options: WriterOptions) -> Vec<u8> {
        let mut archive = OpenArchive::new(Cursor::new(Vec::new()), options.with_log_order(false));
        for (kind, raw_json) in (0_i64..).zip(UNSTRUCTURED_ARRAY_ROWS.iter().copied()) {
            let fields = [
                FieldRef::new(
                    b"array",
                    ValueRef::UnstructuredArray(UnstructuredArrayRef::new(raw_json)),
                ),
                FieldRef::new(b"kind", ValueRef::I64(kind)),
            ];
            archive
                .append_record(RecordRef::new(&fields))
                .expect("append unstructured-array record");
        }
        archive
            .finish()
            .expect("finish unstructured-array archive")
            .into_inner()
            .into_inner()
    }

    #[test]
    fn rust_reader_accepts_every_empty_section() {
        let bytes = empty_archive();
        let mut reader =
            SingleFileArchiveReader::open(Cursor::new(bytes)).expect("open empty archive");
        assert_eq!(0, reader.header().uncompressed_size());
        assert_eq!(
            reader.header().compressed_size(),
            reader.layout().archive_size()
        );

        let metadata = reader
            .read_metadata(MetadataLimits::default())
            .expect("read metadata");
        assert_eq!(
            SFA_SECTION_NAMES,
            std::array::from_fn(|index| metadata.directory().sections()[index].name())
        );
        assert_eq!(
            &EMPTY_TIMESTAMP_DICTIONARY,
            metadata.timestamp_dictionary_bytes()
        );
        assert_eq!(0, metadata.timestamp_dictionary().ranges().len());
        assert_eq!(0, metadata.timestamp_dictionary().patterns().len());
        assert!(metadata.range_index().is_none());

        let schema_tree = reader
            .read_schema_tree(&metadata, SchemaTreeLimits::default())
            .expect("read empty schema tree");
        assert!(schema_tree.is_empty());
        let schema_map = reader
            .read_schema_map(&metadata, &schema_tree, SchemaMapLimits::default())
            .expect("read empty schema map");
        assert!(schema_map.is_empty());
        let table_metadata = reader
            .read_table_metadata(&metadata, &schema_map, TableMetadataLimits::default())
            .expect("read empty table metadata");
        assert_eq!(0, table_metadata.packed_streams().len());
        assert_eq!(0, table_metadata.schema_tables().len());
        assert!(
            reader
                .read_variable_dictionary(&metadata, DictionaryLimits::default())
                .expect("read empty variable dictionary")
                .is_empty()
        );
        assert!(
            reader
                .read_log_type_dictionary(&metadata, DictionaryLimits::default())
                .expect("read empty logtype dictionary")
                .is_empty()
        );
        assert!(
            reader
                .read_array_dictionary(&metadata, DictionaryLimits::default())
                .expect("read empty array dictionary")
                .is_empty()
        );
        assert_eq!(
            0,
            metadata
                .directory()
                .get("/0")
                .expect("tables section")
                .compressed_size()
        );
    }

    #[test]
    fn writes_the_caller_supplied_uncompressed_source_size() {
        let options = WriterOptions::default().with_uncompressed_size(152);
        let bytes = primitive_archive(options);
        let reader = SingleFileArchiveReader::open(Cursor::new(bytes))
            .expect("open sized primitive archive");
        assert_eq!(152, reader.header().uncompressed_size());
    }

    #[test]
    fn string_writer_is_byte_identical_to_the_pinned_cpp_oracle() {
        const CPP_ORACLE: &[u8] = include_bytes!("../tests/fixtures/sfa-v0.5.0-strings-cpp.bin");
        let rust = string_archive(WriterOptions::default().with_uncompressed_size(205));
        assert_eq!(CPP_ORACLE, rust);
    }

    #[test]
    fn timestamp_writer_is_byte_identical_to_the_pinned_cpp_oracle() {
        const CPP_ORACLE: &[u8] = include_bytes!("../tests/fixtures/sfa-v0.5.0-timestamps-cpp.bin");
        let source_size = u64::try_from(TIMESTAMP_SOURCE.len()).expect("source size fits u64");
        let rust = timestamp_archive(WriterOptions::default().with_uncompressed_size(source_size));
        assert_eq!(CPP_ORACLE, rust);
    }

    #[test]
    fn unstructured_array_writer_is_byte_identical_to_the_pinned_cpp_oracle() {
        const CPP_ORACLE: &[u8] =
            include_bytes!("../tests/fixtures/sfa-v0.5.0-unstructured-arrays-cpp.bin");
        let source_size =
            u64::try_from(UNSTRUCTURED_ARRAY_SOURCE.len()).expect("source size fits u64");
        let rust = unstructured_array_archive(
            WriterOptions::default().with_uncompressed_size(source_size),
        );
        assert_eq!(CPP_ORACLE, rust);
    }

    #[test]
    fn reader_and_extractor_restore_exact_unstructured_array_lexemes() {
        let bytes = unstructured_array_archive(WriterOptions::default());
        let mut reader = SingleFileArchiveReader::open(Cursor::new(bytes.clone()))
            .expect("open unstructured-array SFA");
        let catalog = reader
            .read_catalog(ArchiveCatalogLimits::default())
            .expect("read unstructured-array catalog");
        assert_eq!(6, catalog.array_dictionary().len());
        assert_eq!(2, catalog.variable_dictionary().len());
        assert_eq!(
            b"face",
            catalog
                .variable_dictionary()
                .entry(0)
                .expect("first array variable")
                .value()
        );
        assert_eq!(
            br"\u0011\u0012\u0013",
            catalog
                .variable_dictionary()
                .entry(1)
                .expect("second array variable")
                .value()
        );
        let stream = reader
            .read_packed_stream(
                catalog.metadata(),
                catalog.table_metadata(),
                0,
                PackedStreamLimits::default(),
            )
            .expect("read unstructured-array table stream");
        let decoded = catalog
            .schema_tables(0, &stream, ColumnLimits::default())
            .expect("open unstructured-array table stream")
            .next()
            .expect("unstructured-array table")
            .expect("decode unstructured-array table");
        assert_eq!(6, decoded.table().message_count());
        let ColumnData::UnstructuredArray(arrays) = decoded.table().columns()[0].data() else {
            panic!("first array-oracle column must contain unstructured arrays");
        };
        assert_eq!(UNSTRUCTURED_ARRAY_ROWS.len(), arrays.len());
        assert_eq!(
            [0, 1, 50_331_650, 100_663_299, 167_772_164, 184_549_381],
            (0..arrays.len())
                .map(|index| arrays.descriptor(index).expect("descriptor").raw())
                .collect::<Vec<_>>()
                .as_slice()
        );
        assert_eq!(
            [1, 2, 3, 2, 4, 5, -7, 320_049, 0, 9, 1],
            arrays
                .encoded_variables()
                .iter()
                .collect::<Vec<_>>()
                .as_slice()
        );
        assert!((0..arrays.len()).all(|index| arrays.record(index).is_some()));

        let mut extraction_reader = SingleFileArchiveReader::open(Cursor::new(bytes))
            .expect("open unstructured-array extraction");
        let mut extracted = Vec::new();
        extract_jsonl(
            &mut extraction_reader,
            &mut extracted,
            ExtractionOptions::new(ExtractionMode::Unordered),
        )
        .expect("extract unstructured-array records");
        assert_eq!(UNSTRUCTURED_ARRAY_SOURCE, extracted);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn reader_and_extractor_restore_exact_timestamp_lexemes() {
        let bytes = timestamp_archive(WriterOptions::default());
        let mut reader =
            SingleFileArchiveReader::open(Cursor::new(bytes.clone())).expect("open timestamp SFA");
        let catalog = reader
            .read_catalog(ArchiveCatalogLimits::default())
            .expect("read timestamp catalog");
        assert_eq!(1, catalog.metadata().timestamp_dictionary().ranges().len());
        let range = &catalog.metadata().timestamp_dictionary().ranges()[0];
        assert_eq!("ts", range.key());
        assert_eq!(&[1], range.column_ids());
        assert_eq!(
            TimestampBounds::Epoch {
                start: 1_422_752_523_004,
                end: 1_700_000_001_123,
            },
            range.bounds()
        );
        let patterns = catalog.metadata().timestamp_dictionary().patterns();
        assert_eq!(2, patterns.len());
        assert_eq!(
            (0, DATE_TIMESTAMP_PATTERN),
            (patterns[0].id(), patterns[0].raw())
        );
        assert_eq!((1, r"\L"), (patterns[1].id(), patterns[1].raw()));

        let stream = reader
            .read_packed_stream(
                catalog.metadata(),
                catalog.table_metadata(),
                0,
                PackedStreamLimits::default(),
            )
            .expect("read timestamp table stream");
        let decoded = catalog
            .schema_tables(0, &stream, ColumnLimits::default())
            .expect("open timestamp table stream")
            .next()
            .expect("timestamp table")
            .expect("decode timestamp table");
        let ColumnData::Timestamp(timestamps) = decoded.table().columns()[0].data() else {
            panic!("first timestamp-oracle column must contain timestamps");
        };
        let expected_epochs = TIMESTAMP_ROWS.iter().map(|row| row.0).collect::<Vec<_>>();
        assert_eq!(
            expected_epochs,
            timestamps.epochs().values().collect::<Vec<_>>()
        );
        assert_eq!(
            [0, 1, 0, 1],
            timestamps
                .pattern_ids()
                .iter()
                .collect::<Vec<_>>()
                .as_slice()
        );

        let mut extraction_reader =
            SingleFileArchiveReader::open(Cursor::new(bytes)).expect("open timestamp extraction");
        let mut extracted = Vec::new();
        extract_jsonl(
            &mut extraction_reader,
            &mut extracted,
            ExtractionOptions::new(ExtractionMode::Unordered),
        )
        .expect("extract timestamp records");
        assert_eq!(TIMESTAMP_SOURCE, extracted);
    }

    #[test]
    fn default_log_order_and_timestamp_metadata_share_shifted_node_ids() {
        let mut archive = OpenArchive::new(Cursor::new(Vec::<u8>::new()), WriterOptions::default());
        for (epoch, lexeme) in [(0, "0"), (1, "1")] {
            let fields = [FieldRef::new(
                b"ts",
                ValueRef::Timestamp(TimestampRef::new(epoch, lexeme, r"\N", "ts")),
            )];
            archive
                .append_record(RecordRef::new(&fields))
                .expect("append ordered timestamp");
        }
        let bytes = archive
            .finish()
            .expect("finish ordered timestamp archive")
            .into_inner()
            .into_inner();
        let mut reader = SingleFileArchiveReader::open(Cursor::new(bytes.clone()))
            .expect("open ordered timestamp archive");
        let catalog = reader
            .read_catalog(ArchiveCatalogLimits::default())
            .expect("read ordered timestamp catalog");
        assert_eq!(
            &[3],
            catalog.metadata().timestamp_dictionary().ranges()[0].column_ids()
        );
        assert_eq!(
            NodeType::Timestamp,
            catalog.schema_tree().nodes()[3].node_type()
        );

        let mut extraction_reader = SingleFileArchiveReader::open(Cursor::new(bytes))
            .expect("open ordered timestamp extraction");
        let mut extracted = Vec::new();
        extract_jsonl(
            &mut extraction_reader,
            &mut extracted,
            ExtractionOptions::new(ExtractionMode::LogOrder),
        )
        .expect("extract ordered timestamps");
        assert_eq!(b"{\"ts\":0}\n{\"ts\":1}\n", extracted.as_slice());
    }

    #[test]
    fn log_order_defaults_on_and_empty_output_remains_canonical() {
        assert!(WriterOptions::default().records_log_order());
        assert!(
            !WriterOptions::default()
                .with_log_order(false)
                .records_log_order()
        );

        let enabled = OpenArchive::new(Cursor::new(Vec::new()), WriterOptions::default())
            .finish()
            .expect("finish default-order empty archive")
            .into_inner()
            .into_inner();
        assert_eq!(empty_archive(), enabled);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn log_order_columns_span_interleaved_schema_tables_and_drive_extraction() {
        let source_size = u64::try_from(LOG_ORDER_SOURCE.len()).expect("source size fits u64");
        let bytes = log_order_archive(WriterOptions::default().with_uncompressed_size(source_size));
        let mut reader =
            SingleFileArchiveReader::open(Cursor::new(bytes.clone())).expect("open order archive");
        let catalog = reader
            .read_catalog(ArchiveCatalogLimits::default())
            .expect("read order catalog");
        let locator = LogOrderLocator::discover(catalog.schema_tree())
            .expect("discover log order")
            .expect("default writer records log order");
        assert_eq!(0, locator.metadata_root_node_id());
        assert_eq!(1, locator.node_id());
        assert_eq!(6, catalog.schema_tree().len());
        assert_eq!(
            NodeType::Metadata,
            catalog.schema_tree().nodes()[0].node_type()
        );
        assert_eq!(b"", catalog.schema_tree().nodes()[0].key_bytes());
        assert_eq!(None, catalog.schema_tree().nodes()[0].parent_id());
        assert_eq!(
            NodeType::DeltaInteger,
            catalog.schema_tree().nodes()[1].node_type()
        );
        assert_eq!(
            LOG_EVENT_IDX_KEY,
            catalog.schema_tree().nodes()[1].key_bytes()
        );
        assert_eq!(Some(0), catalog.schema_tree().nodes()[1].parent_id());
        for schema in catalog.schema_map().schemas() {
            assert_eq!(2, schema.ordered_entry_count());
            assert_eq!(Some(&SchemaEntry::Node(1)), schema.entries().first());
        }

        let expected_values: &[&[i64]] = &[&[0, 2, 5], &[1, 4], &[3]];
        let expected_deltas: &[&[i64]] = &[&[0, 2, 3], &[1, 3], &[3]];
        let mut table_index = 0_usize;
        for stream_id in 0..catalog.table_metadata().packed_streams().len() {
            let stream = reader
                .read_packed_stream(
                    catalog.metadata(),
                    catalog.table_metadata(),
                    stream_id,
                    PackedStreamLimits::default(),
                )
                .expect("decode order stream");
            for decoded in catalog
                .schema_tables(
                    u64::try_from(stream_id).expect("stream ID fits u64"),
                    &stream,
                    ColumnLimits::default(),
                )
                .expect("open order table stream")
            {
                let decoded = decoded.expect("decode order table");
                let order = locator
                    .locate(decoded.schema(), decoded.table())
                    .expect("locate table order")
                    .expect("every default-order schema contains the index");
                assert_eq!(
                    expected_values[table_index],
                    order.cursor().collect::<Vec<_>>()
                );
                let ColumnData::DeltaInteger(column) = decoded.table().columns()[0].data() else {
                    panic!("first default-order column must contain delta integers");
                };
                assert_eq!(
                    expected_deltas[table_index],
                    column.deltas().iter().collect::<Vec<_>>()
                );
                table_index += 1;
            }
        }
        assert_eq!(expected_values.len(), table_index);

        let mut unordered_reader = SingleFileArchiveReader::open(Cursor::new(bytes.clone()))
            .expect("open unordered extraction source");
        let mut unordered = Vec::new();
        extract_jsonl(
            &mut unordered_reader,
            &mut unordered,
            ExtractionOptions::new(ExtractionMode::Unordered),
        )
        .expect("extract physical table order");
        assert_eq!(LOG_ORDER_UNORDERED, unordered);

        let mut ordered_reader = SingleFileArchiveReader::open(Cursor::new(bytes))
            .expect("open ordered extraction source");
        let mut ordered = Vec::new();
        extract_jsonl(
            &mut ordered_reader,
            &mut ordered,
            ExtractionOptions::new(ExtractionMode::LogOrder),
        )
        .expect("extract archive-global record order");
        assert_eq!(LOG_ORDER_SOURCE, ordered);
    }

    #[test]
    fn disabling_log_order_omits_metadata_and_rejects_ordered_extraction() {
        let bytes = log_order_archive(WriterOptions::default().with_log_order(false));
        let mut reader = SingleFileArchiveReader::open(Cursor::new(bytes.clone()))
            .expect("open disabled-order archive");
        let catalog = reader
            .read_catalog(ArchiveCatalogLimits::default())
            .expect("read disabled-order catalog");
        assert!(
            LogOrderLocator::discover(catalog.schema_tree())
                .expect("inspect disabled-order tree")
                .is_none()
        );

        let mut ordered_reader = SingleFileArchiveReader::open(Cursor::new(bytes))
            .expect("open disabled-order extraction source");
        let error = extract_jsonl(
            &mut ordered_reader,
            &mut Vec::new(),
            ExtractionOptions::new(ExtractionMode::LogOrder),
        )
        .expect_err("disabled log order must reject ordered extraction");
        assert!(matches!(error, ExtractionError::MissingLogOrderColumn));
    }

    #[test]
    fn log_order_writer_matches_every_canonical_cpp_section() {
        const CPP_ORACLE: &[u8] = include_bytes!("../tests/fixtures/sfa-v0.5.0-log-order-cpp.bin");
        let source_size = u64::try_from(LOG_ORDER_SOURCE.len()).expect("source size fits u64");
        let rust = log_order_archive(WriterOptions::default().with_uncompressed_size(source_size));
        assert_ne!(CPP_ORACLE, rust.as_slice());
        let cpp_sections = canonical_section_bytes(CPP_ORACLE);
        let rust_sections = canonical_section_bytes(&rust);
        for (name, (cpp, rust)) in SFA_SECTION_NAMES
            .iter()
            .zip(cpp_sections.iter().zip(&rust_sections))
        {
            assert_eq!(cpp, rust, "canonical section {name}");
        }
    }

    #[test]
    fn ordered_extraction_rejects_a_corrupt_duplicate_global_index() {
        let source_size = u64::try_from(LOG_ORDER_SOURCE.len()).expect("source size fits u64");
        let options = WriterOptions::default().with_uncompressed_size(source_size);
        let valid = log_order_archive(options);
        let mut sections = canonical_section_bytes(&valid);
        let mut tables =
            zstd::stream::decode_all(sections[6].as_slice()).expect("decode packed table stream");
        tables[size_of::<i64>()..2 * size_of::<i64>()].copy_from_slice(&1_i64.to_le_bytes());
        sections[6] = zstd::stream::encode_all(tables.as_slice(), options.compression_level())
            .expect("recompress corrupt packed table stream");
        let encoded = EncodedEmptyArchive::from_sections(options, sections)
            .expect("assemble bounded corrupt archive");
        let corrupted = assembled_archive_bytes(&encoded);

        let mut reader = SingleFileArchiveReader::open(Cursor::new(corrupted))
            .expect("open corrupt-order archive");
        let error = extract_jsonl(
            &mut reader,
            &mut Vec::new(),
            ExtractionOptions::new(ExtractionMode::LogOrder),
        )
        .expect_err("duplicate global index must be rejected");
        assert!(matches!(
            error,
            ExtractionError::OrderedMerge(OrderedMergeError::DuplicateLogEventIndex {
                log_event_idx: 1,
                ..
            })
        ));
    }

    #[test]
    fn rejected_append_does_not_consume_a_log_event_index() {
        let mut archive = OpenArchive::new(Cursor::new(Vec::new()), WriterOptions::default());
        let first = [FieldRef::new(b"a", ValueRef::I64(1))];
        archive
            .append_record(RecordRef::new(&first))
            .expect("append first record");
        let duplicate = [
            FieldRef::new(b"duplicate", ValueRef::Null),
            FieldRef::new(b"duplicate", ValueRef::Bool(false)),
        ];
        archive
            .append_record(RecordRef::new(&duplicate))
            .expect_err("reject duplicate key before commit");
        let second = [FieldRef::new(b"b", ValueRef::Bool(true))];
        archive
            .append_record(RecordRef::new(&second))
            .expect("append record after rejection");
        let bytes = archive
            .finish()
            .expect("finish atomically appended archive")
            .into_inner()
            .into_inner();

        let mut reader = SingleFileArchiveReader::open(Cursor::new(bytes))
            .expect("open atomically appended archive");
        let mut ordered = Vec::new();
        extract_jsonl(
            &mut reader,
            &mut ordered,
            ExtractionOptions::new(ExtractionMode::LogOrder),
        )
        .expect("extract contiguous indexes after rejection");
        assert_eq!(b"{\"a\":1}\n{\"b\":true}\n", ordered.as_slice());
    }

    #[test]
    fn flat_record_events_match_the_borrowed_tree_encoding() {
        let options = WriterOptions::default().with_log_order(false);
        let empty = [];
        let metrics = [
            FieldRef::new(b"load", ValueRef::F64(1.25)),
            FieldRef::new(b"empty", ValueRef::Object(&empty)),
        ];
        let root = [
            FieldRef::new(b"id", ValueRef::I64(7)),
            FieldRef::new(b"metrics", ValueRef::Object(&metrics)),
            FieldRef::new(
                b"items",
                ValueRef::UnstructuredArray(UnstructuredArrayRef::new(b"[1,{\"x\":true}]")),
            ),
        ];
        let mut tree = OpenArchive::new(Cursor::new(Vec::new()), options);
        tree.append_record(RecordRef::new(&root))
            .expect("append borrowed tree");
        let expected = tree.finish().expect("finish borrowed tree").into_inner();

        let events = [
            RecordEventRef::value(b"id", ValueRef::I64(7)),
            RecordEventRef::object_start(b"metrics"),
            RecordEventRef::value(b"load", ValueRef::F64(1.25)),
            RecordEventRef::object_start(b"empty"),
            RecordEventRef::ObjectEnd,
            RecordEventRef::ObjectEnd,
            RecordEventRef::value(
                b"items",
                ValueRef::UnstructuredArray(UnstructuredArrayRef::new(b"[1,{\"x\":true}]")),
            ),
        ];
        let mut flat = OpenArchive::new(Cursor::new(Vec::new()), options);
        flat.append_record_events(events)
            .expect("append flat traversal");
        let actual = flat.finish().expect("finish flat traversal").into_inner();
        assert_eq!(expected, actual);
    }

    #[test]
    fn flat_record_event_errors_are_atomic_and_located() {
        let mut archive = OpenArchive::new(Cursor::new(Vec::<u8>::new()), WriterOptions::default());
        assert_eq!(
            Err(AppendError::InvalidRecordEvents {
                event_index: 2,
                reason: RecordEventError::UnclosedObjects { count: 1 },
            }),
            archive.append_record_events([
                RecordEventRef::object_start(b"nested"),
                RecordEventRef::value(b"value", ValueRef::I64(1)),
            ])
        );
        assert_eq!(0, archive.record_count());
        assert_eq!(
            Err(AppendError::InvalidRecordEvents {
                event_index: 0,
                reason: RecordEventError::UnexpectedObjectEnd,
            }),
            archive.append_record_events([RecordEventRef::ObjectEnd])
        );
        assert_eq!(0, archive.record_count());
        assert_eq!(
            Err(AppendError::DuplicateField {
                object_depth: 1,
                previous_index: 0,
                field_index: 1,
            }),
            archive.append_record_events([
                RecordEventRef::value(b"same", ValueRef::I64(1)),
                RecordEventRef::object_start(b"same"),
            ])
        );
        assert_eq!(0, archive.record_count());

        archive
            .append_record_events([RecordEventRef::value(b"ok", ValueRef::Bool(true))])
            .expect("append after rejected traversals");
        assert_eq!(1, archive.record_count());
    }

    #[test]
    fn cached_flat_event_schema_keeps_wide_duplicate_detection_atomic() {
        let options = WriterOptions::default().with_log_order(false);
        let keys = (0..20)
            .map(|index| format!("field-{index:02}").into_bytes())
            .collect::<Vec<_>>();
        let first = keys
            .iter()
            .enumerate()
            .map(|(index, key)| {
                RecordEventRef::value(
                    key,
                    ValueRef::I64(i64::try_from(index).expect("field index fits i64")),
                )
            })
            .collect::<Vec<_>>();
        let second = keys
            .iter()
            .enumerate()
            .map(|(index, key)| {
                RecordEventRef::value(
                    key,
                    ValueRef::I64(
                        i64::try_from(index)
                            .expect("field index fits i64")
                            .checked_add(100)
                            .expect("test value fits i64"),
                    ),
                )
            })
            .collect::<Vec<_>>();

        let mut expected = OpenArchive::new(Cursor::new(Vec::new()), options);
        expected
            .append_record_events(first.iter().copied())
            .expect("append expected first record");
        expected
            .append_record_events(second.iter().copied())
            .expect("append expected second record");

        let mut actual = OpenArchive::new(Cursor::new(Vec::new()), options);
        actual
            .append_record_events(first.iter().copied())
            .expect("append first record and populate the node-path cache");
        let mut duplicate = second.clone();
        duplicate.push(RecordEventRef::value(
            keys.last().expect("wide record has a final key"),
            ValueRef::I64(999),
        ));
        assert_eq!(
            Err(AppendError::DuplicateField {
                object_depth: 1,
                previous_index: 19,
                field_index: 20,
            }),
            actual.append_record_events(duplicate)
        );
        assert_eq!(1, actual.record_count());
        actual
            .append_record_events(second)
            .expect("append valid record after the cached duplicate is rejected");

        assert_eq!(
            expected
                .finish()
                .expect("finish expected archive")
                .into_inner()
                .into_inner(),
            actual
                .finish()
                .expect("finish atomically updated archive")
                .into_inner()
                .into_inner()
        );
    }

    #[test]
    fn repeated_schema_cache_matches_reordered_fallback_bytes() {
        let options = WriterOptions::default().with_log_order(false);
        let append_row = |archive: &mut OpenArchive<Cursor<Vec<u8>>>, row: i64, reordered: bool| {
            let empty = [];
            if reordered {
                let nested = [
                    FieldRef::new(b"label", ValueRef::String(b"same")),
                    FieldRef::new(b"enabled", ValueRef::Bool(0 == row % 2)),
                ];
                let fields = [
                    FieldRef::new(b"missing", ValueRef::Null),
                    FieldRef::new(b"empty", ValueRef::Object(&empty)),
                    FieldRef::new(b"nested", ValueRef::Object(&nested)),
                    FieldRef::new(b"id", ValueRef::I64(row)),
                ];
                archive.append_record(RecordRef::new(&fields))
            } else {
                let nested = [
                    FieldRef::new(b"enabled", ValueRef::Bool(0 == row % 2)),
                    FieldRef::new(b"label", ValueRef::String(b"same")),
                ];
                let fields = [
                    FieldRef::new(b"id", ValueRef::I64(row)),
                    FieldRef::new(b"nested", ValueRef::Object(&nested)),
                    FieldRef::new(b"empty", ValueRef::Object(&empty)),
                    FieldRef::new(b"missing", ValueRef::Null),
                ];
                archive.append_record(RecordRef::new(&fields))
            }
        };

        let mut cached = OpenArchive::new(Cursor::new(Vec::new()), options);
        let mut fallback = OpenArchive::new(Cursor::new(Vec::new()), options);
        for (row, reordered) in [false, true, true, false, false].into_iter().enumerate() {
            let row = i64::try_from(row).expect("row index fits i64");
            append_row(&mut cached, row, false).expect("append repeated cached schema");
            append_row(&mut fallback, row, reordered).expect("append reordered or repeated schema");
        }
        assert_eq!(1, cached.schema_count());
        assert_eq!(1, fallback.schema_count());
        assert_eq!(
            cached
                .finish()
                .expect("finish cached-layout archive")
                .into_inner()
                .into_inner(),
            fallback
                .finish()
                .expect("finish reordered-layout archive")
                .into_inner()
                .into_inner()
        );
    }

    #[test]
    fn rejected_cached_schema_append_preserves_layout_and_bytes() {
        let limits = WriterLimits::DEFAULT.with_dictionary_limits(1, 8, 1024, 1024, 8, 8);
        let options = WriterOptions::default()
            .with_log_order(false)
            .with_limits(limits);
        let accepted = [FieldRef::new(b"value", ValueRef::String(b"accepted"))];
        let rejected = [FieldRef::new(b"value", ValueRef::String(b"rejected"))];

        let mut expected = OpenArchive::new(Cursor::new(Vec::new()), options);
        expected
            .append_record(RecordRef::new(&accepted))
            .expect("append expected first record");
        expected
            .append_record(RecordRef::new(&accepted))
            .expect("append expected repeated record");

        let mut actual = OpenArchive::new(Cursor::new(Vec::new()), options);
        actual
            .append_record(RecordRef::new(&accepted))
            .expect("append first record and populate the layout cache");
        let resident_bytes = actual.resident_bytes();
        assert!(matches!(
            actual.append_record(RecordRef::new(&rejected)),
            Err(AppendError::LimitExceeded {
                resource: AppendResource::VariableDictionaryEntries,
                ..
            })
        ));
        assert_eq!(1, actual.record_count());
        assert_eq!(resident_bytes, actual.resident_bytes());
        actual
            .append_record(RecordRef::new(&accepted))
            .expect("append cached record after rejection");

        assert_eq!(
            expected
                .finish()
                .expect("finish expected archive")
                .into_inner()
                .into_inner(),
            actual
                .finish()
                .expect("finish archive after rejected cached append")
                .into_inner()
                .into_inner()
        );
    }

    #[test]
    fn fallible_record_event_source_is_atomic_and_located() {
        let mut archive = OpenArchive::new(Cursor::new(Vec::<u8>::new()), WriterOptions::default());
        assert_eq!(
            Err(RecordEventAppendError::Source {
                event_index: 1,
                source: "conversion failed",
            }),
            archive.try_append_record_events([
                Ok(RecordEventRef::value(b"planned", ValueRef::I64(1))),
                Err("conversion failed"),
            ])
        );
        assert_eq!(0, archive.record_count());
        archive
            .append_record_events([RecordEventRef::value(b"committed", ValueRef::I64(2))])
            .expect("append after source rejection");
        assert_eq!(1, archive.record_count());
    }

    #[test]
    fn retained_float_writer_is_byte_identical_to_the_pinned_cpp_oracle() {
        const CPP_ORACLE: &[u8] =
            include_bytes!("../tests/fixtures/sfa-v0.5.0-retained-floats-cpp.bin");
        let source_size = u64::try_from(RETAINED_FLOAT_SOURCE.len()).expect("source size fits u64");
        let rust =
            retained_float_archive(WriterOptions::default().with_uncompressed_size(source_size));
        assert_eq!(CPP_ORACLE, rust);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn reader_and_extractor_restore_retained_float_lexemes() {
        let bytes = retained_float_archive(WriterOptions::default());
        let mut reader =
            SingleFileArchiveReader::open(Cursor::new(bytes)).expect("open retained-float archive");
        let catalog = reader
            .read_catalog(ArchiveCatalogLimits::default())
            .expect("read retained-float catalog");
        assert_eq!(5, catalog.variable_dictionary().len());
        let stream = reader
            .read_packed_stream(
                catalog.metadata(),
                catalog.table_metadata(),
                0,
                PackedStreamLimits::default(),
            )
            .expect("decode retained-float packed stream");
        let decoded = catalog
            .schema_tables(0, &stream, ColumnLimits::default())
            .expect("open retained-float table stream")
            .next()
            .expect("retained-float table")
            .expect("decode retained-float table");
        let ColumnData::FormattedFloat(formatted_column) = decoded.table().columns()[0].data()
        else {
            panic!("first retained column must be formatted-float encoded");
        };
        assert_eq!(RETAINED_FLOAT_ROWS.len(), formatted_column.len());
        for (index, (expected, _)) in RETAINED_FLOAT_ROWS.iter().enumerate() {
            let value = formatted_column.get(index).expect("formatted-float value");
            let mut restored = String::new();
            crate::archive::append_formatted_float(value.value(), value.format(), &mut restored)
                .expect("restore formatted-float token");
            assert_eq!(*expected, restored.as_bytes());
        }
        let ColumnData::DictionaryFloat(dictionary_column) = decoded.table().columns()[1].data()
        else {
            panic!("second retained column must be dictionary-float encoded");
        };
        assert_eq!(
            [0, 1, 0, 2, 3, 4],
            dictionary_column
                .ids()
                .iter()
                .collect::<Vec<_>>()
                .as_slice()
        );

        let plan = ExtractionPlan::compile(
            decoded.schema(),
            catalog.schema_tree(),
            ExtractionPlanLimits::default(),
        )
        .expect("compile retained-float extraction plan");
        let program = RecordProgram::compile(&plan, catalog.schema_tree(), RecordLimits::default())
            .expect("compile retained-float record program");
        let mut writer = program
            .writer(decoded.table(), catalog.timestamp_patterns())
            .expect("bind retained-float columns");
        let mut extracted = Vec::new();
        while writer
            .append_next_record(&mut extracted)
            .expect("extract retained-float record")
        {
            extracted.push(b'\n');
        }
        assert_eq!(RETAINED_FLOAT_SOURCE, extracted);
    }

    #[test]
    fn rejected_retained_floats_leave_archive_state_unchanged() {
        let expected_empty = empty_archive();
        let mut archive = OpenArchive::new(Cursor::new(Vec::new()), WriterOptions::default());
        let cases = [
            RetainedFloatRef::new(f64::NAN, b"0.0"),
            RetainedFloatRef::new(1.0, b"1"),
            RetainedFloatRef::new(1.0, b"01.0"),
            RetainedFloatRef::new(1.0, b"2.0"),
            RetainedFloatRef::new(1.0, b"1e9999"),
        ];
        for retained in cases {
            let fields = [FieldRef::new(
                b"rejected",
                ValueRef::RetainedFloat(retained),
            )];
            assert!(matches!(
                archive.append_record(RecordRef::new(&fields)),
                Err(AppendError::RetainedFloat { .. })
            ));
            assert_eq!(0, archive.record_count());
            assert_eq!(0, archive.schema_count());
            assert_eq!(0, archive.resident_bytes());
        }
        assert_eq!(
            expected_empty,
            archive
                .finish()
                .expect("rejected retained floats leave canonical empty state")
                .into_inner()
                .into_inner()
        );
    }

    #[test]
    fn rejected_timestamps_leave_archive_state_unchanged() {
        let expected_empty = empty_archive();
        let mut archive = OpenArchive::new(
            Cursor::new(Vec::<u8>::new()),
            WriterOptions::default().with_log_order(false),
        );
        let rejected = [
            TimestampRef::new(0, "0", r"\N", ""),
            TimestampRef::new(0, "0", r"\Q", "ts"),
            TimestampRef::new(0, "1", r"\N", "ts"),
        ];
        for value in rejected {
            let fields = [FieldRef::new(b"ts", ValueRef::Timestamp(value))];
            assert!(matches!(
                archive.append_record(RecordRef::new(&fields)),
                Err(AppendError::Timestamp { .. })
            ));
            assert_eq!(0, archive.record_count());
            assert_eq!(0, archive.schema_count());
            assert_eq!(0, archive.resident_bytes());
        }
        assert_eq!(
            expected_empty,
            archive
                .finish()
                .expect("rejected timestamps leave canonical empty state")
                .into_inner()
                .into_inner()
        );

        let mut archive = OpenArchive::new(
            Cursor::new(Vec::<u8>::new()),
            WriterOptions::default().with_log_order(false),
        );
        let first = [FieldRef::new(
            b"ts",
            ValueRef::Timestamp(TimestampRef::new(0, "0", r"\N", "ts")),
        )];
        archive
            .append_record(RecordRef::new(&first))
            .expect("append first timestamp");
        let resident_bytes = archive.resident_bytes();
        let conflict = [FieldRef::new(
            b"ts",
            ValueRef::Timestamp(TimestampRef::new(1, "1", r"\N", "other.ts")),
        )];
        assert!(matches!(
            archive.append_record(RecordRef::new(&conflict)),
            Err(AppendError::Timestamp {
                reason: TimestampError::ConflictingRangeKey { .. },
                ..
            })
        ));
        assert_eq!(1, archive.record_count());
        assert_eq!(1, archive.schema_count());
        assert_eq!(resident_bytes, archive.resident_bytes());
    }

    #[test]
    fn timestamp_limits_fail_before_semantic_commit() {
        let cases = [
            (
                WriterLimits::DEFAULT.with_timestamp_limits(0, 8, 1024, 1024, 1024, 1024),
                AppendResource::TimestampRanges,
            ),
            (
                WriterLimits::DEFAULT.with_timestamp_limits(8, 0, 1024, 1024, 1024, 1024),
                AppendResource::TimestampPatterns,
            ),
            (
                WriterLimits::DEFAULT.with_timestamp_limits(8, 8, 1, 1024, 1024, 1024),
                AppendResource::TimestampRangeKeyBytes,
            ),
            (
                WriterLimits::DEFAULT.with_timestamp_limits(8, 8, 1024, 1, 1024, 1024),
                AppendResource::TimestampPatternBytes,
            ),
            (
                WriterLimits::DEFAULT.with_timestamp_limits(8, 8, 1024, 1024, 1, 1024),
                AppendResource::TimestampPatternValueBytes,
            ),
            (
                WriterLimits::DEFAULT.with_timestamp_limits(8, 8, 1024, 1024, 1024, 1),
                AppendResource::TimestampLexemeBytes,
            ),
        ];
        let expected_empty = empty_archive();
        for (limits, expected_resource) in cases {
            let mut archive = OpenArchive::new(
                Cursor::new(Vec::new()),
                WriterOptions::default()
                    .with_log_order(false)
                    .with_limits(limits),
            );
            let fields = [FieldRef::new(
                b"ts",
                ValueRef::Timestamp(TimestampRef::new(
                    1_422_752_523_004_000_000,
                    r#""2015-02-01T01:02:03.004""#,
                    DATE_TIMESTAMP_PATTERN,
                    "ts",
                )),
            )];
            assert!(matches!(
                archive.append_record(RecordRef::new(&fields)),
                Err(AppendError::LimitExceeded { resource, .. })
                    if resource == expected_resource
            ));
            assert_eq!(0, archive.record_count());
            assert_eq!(0, archive.schema_count());
            assert_eq!(0, archive.resident_bytes());
            assert_eq!(
                expected_empty,
                archive
                    .finish()
                    .expect("rejected timestamp limit leaves canonical empty state")
                    .into_inner()
                    .into_inner()
            );
        }
    }

    #[test]
    fn timestamp_delta_overflow_does_not_commit_dictionary_or_table_state() {
        let mut archive = OpenArchive::new(
            Cursor::new(Vec::new()),
            WriterOptions::default().with_log_order(false),
        );
        let maximum_lexeme = i64::MAX.to_string();
        let maximum = [FieldRef::new(
            b"ts",
            ValueRef::Timestamp(TimestampRef::new(i64::MAX, &maximum_lexeme, r"\N", "ts")),
        )];
        archive
            .append_record(RecordRef::new(&maximum))
            .expect("append maximum timestamp");
        let resident_bytes = archive.resident_bytes();

        let minimum_lexeme = i64::MIN.to_string();
        let minimum = [FieldRef::new(
            b"ts",
            ValueRef::Timestamp(TimestampRef::new(i64::MIN, &minimum_lexeme, r"\N", "ts")),
        )];
        assert!(matches!(
            archive.append_record(RecordRef::new(&minimum)),
            Err(AppendError::SizeOverflow)
        ));
        assert_eq!(1, archive.record_count());
        assert_eq!(resident_bytes, archive.resident_bytes());

        let bytes = archive
            .finish()
            .expect("finish timestamp archive after rejected overflow")
            .into_inner()
            .into_inner();
        let mut reader = SingleFileArchiveReader::open(Cursor::new(bytes))
            .expect("open timestamp overflow archive");
        let catalog = reader
            .read_catalog(ArchiveCatalogLimits::default())
            .expect("read timestamp overflow catalog");
        let range = &catalog.metadata().timestamp_dictionary().ranges()[0];
        assert_eq!(
            TimestampBounds::Epoch {
                start: 9_223_372_036_854,
                end: 9_223_372_036_855,
            },
            range.bounds()
        );
    }

    #[test]
    fn dictionary_float_fallback_obeys_dictionary_limits_atomically() {
        let limits = WriterLimits::DEFAULT.with_dictionary_limits(0, 8, 1024, 1024, 8, 8);
        let mut archive = OpenArchive::new(
            Cursor::new(Vec::<u8>::new()),
            WriterOptions::default().with_limits(limits),
        );
        let formatted = [FieldRef::new(
            b"value",
            ValueRef::RetainedFloat(RetainedFloatRef::new(-0.0, b"-0.00")),
        )];
        archive
            .append_record(RecordRef::new(&formatted))
            .expect("formatted float does not consume a dictionary entry");
        let resident_bytes = archive.resident_bytes();
        let fallback = [FieldRef::new(
            b"value",
            ValueRef::RetainedFloat(RetainedFloatRef::new(
                123_456_789.123_456_79,
                b"123456789.123456789",
            )),
        )];
        assert!(matches!(
            archive.append_record(RecordRef::new(&fallback)),
            Err(AppendError::LimitExceeded {
                resource: AppendResource::VariableDictionaryEntries,
                ..
            })
        ));
        assert_eq!(1, archive.record_count());
        assert_eq!(1, archive.schema_count());
        assert_eq!(resident_bytes, archive.resident_bytes());
    }

    #[test]
    fn reader_and_extractor_restore_nested_primitive_records() {
        let bytes = primitive_archive(WriterOptions::default());
        let mut reader =
            SingleFileArchiveReader::open(Cursor::new(bytes)).expect("open primitive archive");
        let catalog = reader
            .read_catalog(ArchiveCatalogLimits::default())
            .expect("read primitive catalog");
        assert_eq!(7, catalog.schema_tree().len());
        assert_eq!(2, catalog.schema_map().len());
        assert_eq!(3, catalog.table_metadata().record_count());
        assert_eq!(2, catalog.table_metadata().schema_tables().len());
        assert_eq!(1, catalog.table_metadata().packed_streams().len());

        let stream = reader
            .read_packed_stream(
                catalog.metadata(),
                catalog.table_metadata(),
                0,
                PackedStreamLimits::default(),
            )
            .expect("decode primitive packed stream");
        let mut extracted = Vec::new();
        let mut scratch = RecordScratch::new();
        for decoded in catalog
            .schema_tables(0, &stream, ColumnLimits::default())
            .expect("open primitive table stream")
        {
            let decoded = decoded.expect("decode primitive table");
            let plan = ExtractionPlan::compile(
                decoded.schema(),
                catalog.schema_tree(),
                ExtractionPlanLimits::default(),
            )
            .expect("compile primitive extraction plan");
            let program =
                RecordProgram::compile(&plan, catalog.schema_tree(), RecordLimits::default())
                    .expect("compile primitive record program");
            let mut output = Vec::new();
            let mut writer = program
                .writer_with_scratch(decoded.table(), catalog.timestamp_patterns(), scratch)
                .expect("bind primitive columns");
            while writer
                .append_next_record(&mut output)
                .expect("extract primitive record")
            {
                extracted.push(output.clone());
                output.clear();
            }
            scratch = writer.into_scratch();
        }
        let expected: &[&[u8]] = &[
            br#"{"id":-7,"metrics":{"load":1.250000,"ok":true},"missing":null}"#.as_slice(),
            br#"{"id":42,"metrics":{"load":2.500000,"ok":false},"missing":null}"#.as_slice(),
            br#"{"id":9,"enabled":false}"#.as_slice(),
        ];
        let actual = extracted.iter().map(Vec::as_slice).collect::<Vec<_>>();
        assert_eq!(expected, actual);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn reader_and_extractor_restore_cpp_compatible_strings() {
        let bytes = string_archive(WriterOptions::default());
        let mut reader =
            SingleFileArchiveReader::open(Cursor::new(bytes)).expect("open string archive");
        let catalog = reader
            .read_catalog(ArchiveCatalogLimits::default())
            .expect("read string catalog");
        let variable_entries = catalog
            .variable_dictionary()
            .entries()
            .map(|entry| entry.value().to_vec())
            .collect::<Vec<_>>();
        assert_eq!(
            [b"YScope".as_slice(), b"a\tb", b"face"].as_slice(),
            variable_entries
                .iter()
                .map(Vec::as_slice)
                .collect::<Vec<_>>()
        );
        let expected_logtypes: &[&[u8]] = &[
            b"uid=\x11 CPU=\x13 user=\x12",
            b"plain words",
            b"literal \\\\ \\\x11 \\\x12 \\\x13 done",
        ];
        assert_eq!(
            expected_logtypes,
            catalog
                .log_type_dictionary()
                .entries()
                .map(LogTypeDictionaryEntry::escaped_value)
                .collect::<Vec<_>>()
        );

        let stream = reader
            .read_packed_stream(
                catalog.metadata(),
                catalog.table_metadata(),
                0,
                PackedStreamLimits::default(),
            )
            .expect("decode string packed stream");
        let decoded = catalog
            .schema_tables(0, &stream, ColumnLimits::default())
            .expect("open string table stream")
            .next()
            .expect("string table")
            .expect("decode string table");
        let ColumnData::VarString(variable_column) = decoded.table().columns()[0].data() else {
            panic!("first string column must be variable-dictionary encoded");
        };
        assert_eq!(
            [0, 1, 0, 0],
            variable_column.ids().iter().collect::<Vec<_>>().as_slice()
        );
        let ColumnData::ClpString(clp_column) = decoded.table().columns()[1].data() else {
            panic!("second string column must be CLP encoded");
        };
        let descriptors = (0..clp_column.len())
            .map(|index| {
                let descriptor = clp_column.descriptor(index).expect("CLP descriptor");
                (
                    descriptor.logtype_id(),
                    descriptor.encoded_variable_offset(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!([(0, 0), (0, 3), (1, 6), (2, 6)], descriptors.as_slice());
        assert_eq!(
            [0, 2_559_793, 0, i64::MIN, -9_223_372_036_854_775_759, 2,],
            clp_column
                .encoded_variables()
                .iter()
                .collect::<Vec<_>>()
                .as_slice()
        );

        let plan = ExtractionPlan::compile(
            decoded.schema(),
            catalog.schema_tree(),
            ExtractionPlanLimits::default(),
        )
        .expect("compile string extraction plan");
        let program = RecordProgram::compile(&plan, catalog.schema_tree(), RecordLimits::default())
            .expect("compile string record program");
        let mut writer = program
            .writer(decoded.table(), catalog.timestamp_patterns())
            .expect("bind string columns");
        let mut extracted = Vec::new();
        let mut output = Vec::new();
        while writer
            .append_next_record(&mut output)
            .expect("extract string record")
        {
            extracted.push(output.clone());
            output.clear();
        }
        let expected: &[&[u8]] = &[
            br#"{"v":"YScope","c":"uid=0 CPU=99.99 user=YScope"}"#,
            b"{\"v\":\"a\\tb\",\"c\":\"uid=-9223372036854775808 CPU=-00.00 user=face\"}",
            br#"{"v":"YScope","c":"plain words"}"#,
            b"{\"v\":\"YScope\",\"c\":\"literal \\\\ \\u0011 \\u0012 \\u0013 done\"}",
        ];
        assert_eq!(
            expected,
            extracted.iter().map(Vec::as_slice).collect::<Vec<_>>()
        );
    }

    #[test]
    fn literal_ascii_space_alone_selects_clp_string_columns() {
        let mut archive = OpenArchive::new(Cursor::new(Vec::new()), WriterOptions::default());
        for value in [
            b"".as_slice(),
            b"a\tb",
            b"a\nb",
            "a\u{a0}b".as_bytes(),
            "a\u{2003}b".as_bytes(),
            b"a b",
        ] {
            let fields = [FieldRef::new(b"value", ValueRef::String(value))];
            archive
                .append_record(RecordRef::new(&fields))
                .expect("append classification boundary");
        }
        assert_eq!(2, archive.schema_count());
        let bytes = archive
            .finish()
            .expect("finish classification archive")
            .into_inner()
            .into_inner();
        let mut reader =
            SingleFileArchiveReader::open(Cursor::new(bytes)).expect("open classification archive");
        let catalog = reader
            .read_catalog(ArchiveCatalogLimits::default())
            .expect("read classification catalog");
        assert_eq!(5, catalog.variable_dictionary().len());
        assert_eq!(1, catalog.log_type_dictionary().len());
        assert!(
            catalog
                .schema_tree()
                .nodes()
                .iter()
                .any(|node| NodeType::VarString == node.node_type())
        );
        assert!(
            catalog
                .schema_tree()
                .nodes()
                .iter()
                .any(|node| NodeType::ClpString == node.node_type())
        );
    }

    #[test]
    fn variable_dictionary_preserves_arbitrary_bytes() {
        let mut archive = OpenArchive::new(Cursor::new(Vec::new()), WriterOptions::default());
        let exact = [0xff, 0x00, b'\t'];
        let fields = [FieldRef::new(b"value", ValueRef::String(&exact))];
        archive
            .append_record(RecordRef::new(&fields))
            .expect("append arbitrary string bytes");
        let bytes = archive
            .finish()
            .expect("finish arbitrary-byte archive")
            .into_inner()
            .into_inner();
        let mut reader =
            SingleFileArchiveReader::open(Cursor::new(bytes)).expect("open arbitrary-byte archive");
        let catalog = reader
            .read_catalog(ArchiveCatalogLimits::default())
            .expect("read arbitrary-byte catalog");
        assert_eq!(
            exact,
            catalog
                .variable_dictionary()
                .entry(0)
                .expect("arbitrary-byte dictionary entry")
                .value()
        );
    }

    #[test]
    fn primitive_columns_are_column_major_and_typed() {
        let bytes = primitive_archive(WriterOptions::default());
        let mut reader =
            SingleFileArchiveReader::open(Cursor::new(bytes)).expect("open primitive archive");
        let catalog = reader
            .read_catalog(ArchiveCatalogLimits::default())
            .expect("read primitive catalog");
        let stream = reader
            .read_packed_stream(
                catalog.metadata(),
                catalog.table_metadata(),
                0,
                PackedStreamLimits::default(),
            )
            .expect("decode primitive stream");
        let mut tables = catalog
            .schema_tables(0, &stream, ColumnLimits::default())
            .expect("open table stream");
        let first = tables.next().expect("first table").expect("valid table");
        assert_eq!(2, first.table().message_count());
        assert_eq!(3, first.table().len());
        let ColumnData::Integer(ids) = first.table().columns()[0].data() else {
            panic!("first column must contain integers");
        };
        assert_eq!(vec![-7, 42], ids.iter().collect::<Vec<_>>());
        let ColumnData::Float(loads) = first.table().columns()[1].data() else {
            panic!("second column must contain floats");
        };
        assert_eq!(vec![1.25, 2.5], loads.iter().collect::<Vec<_>>());
        let ColumnData::Boolean(flags) = first.table().columns()[2].data() else {
            panic!("third column must contain booleans");
        };
        assert_eq!(vec![true, false], flags.iter().collect::<Vec<_>>());
        assert!(tables.next().expect("second table").is_ok());
        assert!(tables.next().is_none());
    }

    #[test]
    fn caller_threshold_splits_whole_schema_tables() {
        let bytes = primitive_archive(WriterOptions::default().with_minimum_packed_stream_size(0));
        let mut reader =
            SingleFileArchiveReader::open(Cursor::new(bytes)).expect("open split archive");
        let catalog = reader
            .read_catalog(ArchiveCatalogLimits::default())
            .expect("read split catalog");
        assert_eq!(2, catalog.table_metadata().packed_streams().len());
        assert_eq!(2, catalog.table_metadata().schema_tables().len());
        for stream_id in 0..2 {
            let stream = reader
                .read_packed_stream(
                    catalog.metadata(),
                    catalog.table_metadata(),
                    stream_id,
                    PackedStreamLimits::default(),
                )
                .expect("decode split packed stream");
            assert_eq!(
                1,
                catalog
                    .schema_tables(
                        u64::try_from(stream_id).expect("stream ID fits u64"),
                        &stream,
                        ColumnLimits::default(),
                    )
                    .expect("open split table stream")
                    .count()
            );
        }
    }

    #[test]
    fn null_only_record_uses_a_zero_byte_logical_stream() {
        let mut archive = OpenArchive::new(
            Cursor::new(Vec::new()),
            WriterOptions::default().with_log_order(false),
        );
        let fields = [FieldRef::new(b"missing", ValueRef::Null)];
        archive
            .append_record(RecordRef::new(&fields))
            .expect("append null-only record");
        let bytes = archive
            .finish()
            .expect("finish null-only archive")
            .into_inner()
            .into_inner();
        let mut reader =
            SingleFileArchiveReader::open(Cursor::new(bytes)).expect("open null-only archive");
        let catalog = reader
            .read_catalog(ArchiveCatalogLimits::default())
            .expect("read null-only catalog");
        let packed = catalog
            .table_metadata()
            .packed_stream(0)
            .expect("zero-byte packed stream");
        assert_eq!(0, packed.uncompressed_size());
        assert_eq!(0, packed.compressed_size());
        let stream = reader
            .read_packed_stream(
                catalog.metadata(),
                catalog.table_metadata(),
                0,
                PackedStreamLimits::default(),
            )
            .expect("read zero-byte stream");
        let table = catalog
            .schema_tables(0, &stream, ColumnLimits::default())
            .expect("open null-only table stream")
            .next()
            .expect("null-only table")
            .expect("decode null-only table");
        assert_eq!(1, table.table().message_count());
        assert!(table.table().is_empty());
    }

    #[test]
    fn rejected_appends_do_not_change_empty_finalization() {
        let expected = empty_archive();
        let mut archive = OpenArchive::new(Cursor::new(Vec::new()), WriterOptions::default());
        let duplicate = [
            FieldRef::new(b"same", ValueRef::I64(1)),
            FieldRef::new(b"same", ValueRef::Bool(true)),
        ];
        assert!(matches!(
            archive.append_record(RecordRef::new(&duplicate)),
            Err(AppendError::DuplicateField { .. })
        ));
        let non_finite = [FieldRef::new(b"bad", ValueRef::F64(f64::NAN))];
        assert!(matches!(
            archive.append_record(RecordRef::new(&non_finite)),
            Err(AppendError::NonFiniteFloat { .. })
        ));
        let array_values = [ValueRef::I64(1), ValueRef::F64(f64::INFINITY)];
        let field = [FieldRef::new(b"array", ValueRef::Array(&array_values))];
        assert!(matches!(
            archive.append_record(RecordRef::new(&field)),
            Err(AppendError::NonFiniteFloat { .. })
        ));
        assert_eq!(0, archive.record_count());
        assert_eq!(0, archive.schema_count());
        assert_eq!(0, archive.resident_bytes());
        let actual = archive
            .finish()
            .expect("failed appends leave canonical empty state")
            .into_inner()
            .into_inner();
        assert_eq!(expected, actual);
    }

    #[test]
    fn malformed_unstructured_array_append_is_atomic_after_prior_records() {
        let options = WriterOptions::default().with_log_order(false);
        let mut expected = OpenArchive::new(Cursor::new(Vec::new()), options);
        let valid = [FieldRef::new(
            b"array",
            ValueRef::UnstructuredArray(UnstructuredArrayRef::new(b"[]")),
        )];
        expected
            .append_record(RecordRef::new(&valid))
            .expect("append baseline array");
        let expected = expected
            .finish()
            .expect("finish baseline array archive")
            .into_inner()
            .into_inner();

        let mut actual = OpenArchive::new(Cursor::new(Vec::new()), options);
        actual
            .append_record(RecordRef::new(&valid))
            .expect("append retained array");
        let malformed = [FieldRef::new(
            b"other",
            ValueRef::UnstructuredArray(UnstructuredArrayRef::new(b"[1,]")),
        )];
        assert!(matches!(
            actual.append_record(RecordRef::new(&malformed)),
            Err(AppendError::UnstructuredArray {
                reason: UnstructuredArrayError::Syntax {
                    kind: UnstructuredArraySyntaxErrorKind::ExpectedValue,
                    ..
                },
                ..
            })
        ));
        assert_eq!(1, actual.record_count());
        let actual = actual
            .finish()
            .expect("finish archive after rejected array")
            .into_inner()
            .into_inner();
        assert_eq!(expected, actual);
    }

    #[test]
    fn unstructured_array_limits_fail_before_semantic_commit() {
        let cases = [
            (
                WriterLimits::DEFAULT.with_unstructured_array_limits(8, 1, 8),
                b"[]".as_slice(),
                AppendResource::UnstructuredArrayLexemeBytes,
            ),
            (
                WriterLimits::DEFAULT.with_unstructured_array_limits(8, 1024, 1),
                b"[[]]".as_slice(),
                AppendResource::UnstructuredArrayNestingDepth,
            ),
            (
                WriterLimits::DEFAULT.with_unstructured_array_limits(0, 1024, 8),
                b"[]".as_slice(),
                AppendResource::ArrayDictionaryEntries,
            ),
            (
                WriterLimits::DEFAULT.with_dictionary_limits(8, 8, 1, 1024, 8, 8),
                b"[]".as_slice(),
                AppendResource::DictionaryEntryBytes,
            ),
            (
                WriterLimits::DEFAULT.with_dictionary_limits(8, 8, 1024, 1, 8, 8),
                b"[]".as_slice(),
                AppendResource::DictionaryValueBytes,
            ),
            (
                WriterLimits::DEFAULT.with_dictionary_limits(8, 8, 1024, 1024, 0, 8),
                b"[1]".as_slice(),
                AppendResource::EncodedVariablesPerColumn,
            ),
            (
                WriterLimits::DEFAULT.with_dictionary_limits(8, 8, 1024, 1024, 8, 0),
                b"[1]".as_slice(),
                AppendResource::TotalEncodedVariables,
            ),
        ];
        let expected_empty = empty_archive();
        for (limits, raw_json, expected_resource) in cases {
            let mut archive = OpenArchive::new(
                Cursor::new(Vec::<u8>::new()),
                WriterOptions::default()
                    .with_log_order(false)
                    .with_limits(limits),
            );
            let fields = [FieldRef::new(
                b"array",
                ValueRef::UnstructuredArray(UnstructuredArrayRef::new(raw_json)),
            )];
            assert!(matches!(
                archive.append_record(RecordRef::new(&fields)),
                Err(AppendError::LimitExceeded { resource, .. })
                    if resource == expected_resource
            ));
            assert_eq!(0, archive.record_count());
            assert_eq!(0, archive.schema_count());
            assert_eq!(0, archive.resident_bytes());
            assert_eq!(
                expected_empty,
                archive
                    .finish()
                    .expect("rejected array append leaves canonical empty state")
                    .into_inner()
                    .into_inner()
            );
        }
    }

    #[test]
    fn preserves_integer_extremes_and_negative_zero_bits() {
        let mut archive = OpenArchive::new(
            Cursor::new(Vec::new()),
            WriterOptions::default().with_log_order(false),
        );
        for (integer, float) in [(i64::MIN, -0.0), (i64::MAX, f64::MIN_POSITIVE)] {
            let fields = [
                FieldRef::new(b"integer", ValueRef::I64(integer)),
                FieldRef::new(b"float", ValueRef::F64(float)),
            ];
            archive
                .append_record(RecordRef::new(&fields))
                .expect("append numeric edge record");
        }
        let bytes = archive
            .finish()
            .expect("finish numeric edge archive")
            .into_inner()
            .into_inner();
        let mut reader =
            SingleFileArchiveReader::open(Cursor::new(bytes)).expect("open numeric edge archive");
        let catalog = reader
            .read_catalog(ArchiveCatalogLimits::default())
            .expect("read numeric edge catalog");
        let stream = reader
            .read_packed_stream(
                catalog.metadata(),
                catalog.table_metadata(),
                0,
                PackedStreamLimits::default(),
            )
            .expect("read numeric edge stream");
        let decoded = catalog
            .schema_tables(0, &stream, ColumnLimits::default())
            .expect("open numeric table")
            .next()
            .expect("numeric table")
            .expect("decode numeric table");
        let ColumnData::Integer(integers) = decoded.table().columns()[0].data() else {
            panic!("first numeric column must contain integers");
        };
        assert_eq!(
            vec![i64::MIN, i64::MAX],
            integers.iter().collect::<Vec<_>>()
        );
        let ColumnData::Float(floats) = decoded.table().columns()[1].data() else {
            panic!("second numeric column must contain floats");
        };
        let values = floats.iter().collect::<Vec<_>>();
        assert_eq!((-0.0_f64).to_bits(), values[0].to_bits());
        assert_eq!(f64::MIN_POSITIVE.to_bits(), values[1].to_bits());
    }

    #[test]
    fn append_limits_fail_before_semantic_commit() {
        let cases = [
            (
                WriterLimits::DEFAULT.with_record_limits(0, 8, 8, 8, 8, 1024),
                AppendResource::Records,
            ),
            (
                WriterLimits::DEFAULT.with_record_limits(8, 1, 8, 8, 8, 1024),
                AppendResource::NestingDepth,
            ),
            (
                WriterLimits::DEFAULT.with_record_limits(8, 8, 1, 8, 8, 1024),
                AppendResource::SchemaNodes,
            ),
            (
                WriterLimits::DEFAULT.with_record_limits(8, 8, 8, 0, 8, 1024),
                AppendResource::Schemas,
            ),
            (
                WriterLimits::DEFAULT.with_record_limits(8, 8, 8, 8, 0, 1024),
                AppendResource::Columns,
            ),
            (
                WriterLimits::DEFAULT.with_record_limits(8, 8, 8, 8, 8, 0),
                AppendResource::ResidentBytes,
            ),
        ];
        let nested_leaf = [FieldRef::new(b"value", ValueRef::I64(1))];
        let record = [FieldRef::new(b"nested", ValueRef::Object(&nested_leaf))];
        for (limits, expected_resource) in cases {
            let mut archive = OpenArchive::new(
                Cursor::new(Vec::<u8>::new()),
                WriterOptions::default().with_limits(limits),
            );
            assert!(matches!(
                archive.append_record(RecordRef::new(&record)),
                Err(AppendError::LimitExceeded { resource, .. }) if resource == expected_resource
            ));
            assert_eq!(0, archive.record_count());
            assert_eq!(0, archive.schema_count());
            assert_eq!(0, archive.resident_bytes());
        }
    }

    #[test]
    fn dictionary_and_clp_limits_fail_before_semantic_commit() {
        let cases = [
            (
                WriterLimits::DEFAULT.with_dictionary_limits(0, 8, 1024, 1024, 8, 8),
                b"four".as_slice(),
                AppendResource::VariableDictionaryEntries,
            ),
            (
                WriterLimits::DEFAULT.with_dictionary_limits(8, 0, 1024, 1024, 8, 8),
                b"plain words",
                AppendResource::LogTypeDictionaryEntries,
            ),
            (
                WriterLimits::DEFAULT.with_dictionary_limits(8, 8, 3, 1024, 8, 8),
                b"four",
                AppendResource::DictionaryEntryBytes,
            ),
            (
                WriterLimits::DEFAULT.with_dictionary_limits(8, 8, 1024, 3, 8, 8),
                b"four",
                AppendResource::DictionaryValueBytes,
            ),
            (
                WriterLimits::DEFAULT.with_dictionary_limits(8, 8, 1024, 1024, 0, 8),
                b"id 1",
                AppendResource::EncodedVariablesPerColumn,
            ),
            (
                WriterLimits::DEFAULT.with_dictionary_limits(8, 8, 1024, 1024, 8, 0),
                b"id 1",
                AppendResource::TotalEncodedVariables,
            ),
        ];
        let expected_empty = empty_archive();
        for (limits, value, expected_resource) in cases {
            let mut archive = OpenArchive::new(
                Cursor::new(Vec::<u8>::new()),
                WriterOptions::default().with_limits(limits),
            );
            let fields = [FieldRef::new(b"value", ValueRef::String(value))];
            assert!(matches!(
                archive.append_record(RecordRef::new(&fields)),
                Err(AppendError::LimitExceeded { resource, .. })
                    if resource == expected_resource
            ));
            assert_eq!(0, archive.record_count());
            assert_eq!(0, archive.schema_count());
            assert_eq!(0, archive.resident_bytes());
            assert_eq!(
                expected_empty,
                archive
                    .finish()
                    .expect("rejected string append leaves canonical empty state")
                    .into_inner()
                    .into_inner()
            );
        }
    }

    #[test]
    fn rejected_dictionary_append_preserves_prior_records() {
        let limits = WriterLimits::DEFAULT.with_dictionary_limits(1, 8, 1024, 1024, 8, 8);
        let mut archive = OpenArchive::new(
            Cursor::new(Vec::new()),
            WriterOptions::default().with_limits(limits),
        );
        let first = [FieldRef::new(b"value", ValueRef::String(b"first"))];
        archive
            .append_record(RecordRef::new(&first))
            .expect("append first dictionary value");
        let resident_bytes = archive.resident_bytes();
        let second = [FieldRef::new(b"value", ValueRef::String(b"second"))];
        assert!(matches!(
            archive.append_record(RecordRef::new(&second)),
            Err(AppendError::LimitExceeded {
                resource: AppendResource::VariableDictionaryEntries,
                ..
            })
        ));
        assert_eq!(1, archive.record_count());
        assert_eq!(1, archive.schema_count());
        assert_eq!(resident_bytes, archive.resident_bytes());
        let bytes = archive
            .finish()
            .expect("finish archive after rejected append")
            .into_inner()
            .into_inner();
        let mut reader = SingleFileArchiveReader::open(Cursor::new(bytes))
            .expect("open archive after rejected append");
        let catalog = reader
            .read_catalog(ArchiveCatalogLimits::default())
            .expect("read catalog after rejected append");
        assert_eq!(1, catalog.table_metadata().record_count());
        assert_eq!(1, catalog.variable_dictionary().len());
        assert_eq!(
            b"first",
            catalog.variable_dictionary().entry(0).unwrap().value()
        );
    }

    #[test]
    fn emits_named_packets_and_canonical_relative_offsets() {
        let encoded = EncodedEmptyArchive::new(WriterOptions::default()).expect("encode archive");
        let mut decoder =
            zstd::stream::read::Decoder::new(encoded.metadata.as_slice()).expect("metadata frame");
        let mut metadata = Vec::new();
        decoder
            .read_to_end(&mut metadata)
            .expect("decompress metadata");

        assert_eq!(REQUIRED_METADATA_PACKET_COUNT, metadata[0]);
        let mut cursor = 1;
        let (packet_type, archive_info) = next_packet(&metadata, &mut cursor);
        assert_eq!(ARCHIVE_INFO_PACKET_TYPE, packet_type);
        assert_eq!(0x81, archive_info[0], "archive info must be a named map");
        let (packet_type, file_info) = next_packet(&metadata, &mut cursor);
        assert_eq!(ARCHIVE_FILE_INFO_PACKET_TYPE, packet_type);
        assert_eq!(0x81, file_info[0], "file info must be a named map");
        assert!(file_info.windows(2).any(|bytes| bytes == [0xa1, b'n']));
        assert!(file_info.windows(2).any(|bytes| bytes == [0xa1, b'o']));
        let (packet_type, timestamps) = next_packet(&metadata, &mut cursor);
        assert_eq!(TIMESTAMP_DICTIONARY_PACKET_TYPE, packet_type);
        assert_eq!(&EMPTY_TIMESTAMP_DICTIONARY, timestamps);
        assert_eq!(metadata.len(), cursor);

        let offsets = section_offsets(&encoded.sections).expect("section offsets");
        assert_eq!(0, offsets[0]);
        for pair in offsets.windows(2) {
            assert!(pair[0] < pair[1]);
        }
        assert_eq!(
            total_section_size(&encoded.sections).expect("files size"),
            offsets[SFA_SECTION_NAMES.len() - 1]
        );
    }

    #[test]
    fn decodes_fixed_zero_count_payloads() {
        let encoded = EncodedEmptyArchive::new(WriterOptions::default()).expect("encode archive");
        assert_eq!(EMPTY_COUNT, decode_frame(&encoded.sections[0]).as_slice());
        assert_eq!(EMPTY_COUNT, decode_frame(&encoded.sections[1]).as_slice());
        assert_eq!(
            EMPTY_TABLE_METADATA,
            decode_frame(&encoded.sections[2]).as_slice()
        );
        for dictionary in &encoded.sections[3..6] {
            assert_eq!(EMPTY_COUNT, dictionary.as_slice());
        }
        assert_eq!(0, encoded.sections[6].len());
    }

    #[test]
    fn rejects_limits_before_touching_the_sink() {
        let limits = WriterLimits::new(0, u64::MAX, u64::MAX, u64::MAX);
        let error = OpenArchive::new(
            Cursor::new(Vec::new()),
            WriterOptions::default().with_limits(limits),
        )
        .finish()
        .expect_err("schema frame must exceed zero-byte limit");
        assert!(matches!(
            error,
            WriterError::LimitExceeded {
                resource: WriterResource::SchemaTree,
                ..
            }
        ));
    }

    #[test]
    fn rejects_a_nonempty_generic_sink() {
        let error = OpenArchive::new(Cursor::new(vec![0xaa]), WriterOptions::default())
            .finish()
            .expect_err("generic sink cannot be truncated");
        assert!(matches!(
            error,
            WriterError::NonEmptyOutput { actual_size: 1 }
        ));
    }

    fn decode_frame(frame: &[u8]) -> Vec<u8> {
        zstd::stream::decode_all(frame).expect("decode fixed frame")
    }

    fn canonical_section_bytes(bytes: &[u8]) -> [Vec<u8>; SFA_SECTION_NAMES.len()] {
        let mut reader = SingleFileArchiveReader::open(Cursor::new(bytes)).expect("open SFA");
        let metadata = reader
            .read_metadata(MetadataLimits::default())
            .expect("read SFA metadata");
        let files_start = reader.layout().files_range().start;
        let mut files = Vec::new();
        reader
            .files_reader()
            .expect("open SFA files section")
            .read_to_end(&mut files)
            .expect("read SFA files section");
        std::array::from_fn(|index| {
            let range = metadata.directory().sections()[index].range();
            let start =
                usize::try_from(range.start - files_start).expect("section start fits usize");
            let end = usize::try_from(range.end - files_start).expect("section end fits usize");
            files[start..end].to_vec()
        })
    }

    fn assembled_archive_bytes(encoded: &EncodedEmptyArchive) -> Vec<u8> {
        let capacity = usize::try_from(encoded.archive_size).expect("archive size fits usize");
        let mut bytes = Vec::with_capacity(capacity);
        bytes.extend_from_slice(&encoded.header.encode());
        bytes.extend_from_slice(&encoded.metadata);
        for section in &encoded.sections {
            bytes.extend_from_slice(section);
        }
        assert_eq!(capacity, bytes.len());
        bytes
    }

    fn next_packet<'a>(metadata: &'a [u8], cursor: &mut usize) -> (u8, &'a [u8]) {
        let packet_type = metadata[*cursor];
        *cursor += 1;
        let size = u32::from_le_bytes(
            metadata[*cursor..*cursor + size_of::<u32>()]
                .try_into()
                .expect("packet size bytes"),
        );
        *cursor += size_of::<u32>();
        let size = usize::try_from(size).expect("packet size fits usize");
        let payload = &metadata[*cursor..*cursor + size];
        *cursor += size;
        (packet_type, payload)
    }
}
