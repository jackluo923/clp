//! High-level JSONL extraction into caller-owned byte sinks.
//!
//! [`extract_jsonl`] owns no filesystem paths, output buffering policy, or process-global state.
//! It appends complete newline-delimited JSON records to a caller-provided [`Write`] sink and does
//! not flush it. Unordered extraction retains one decompressed packed stream at a time. Log-order
//! extraction necessarily retains all participating streams and table views, but validates its
//! aggregate retention bounds from table metadata before allocating those collections or reading
//! a packed stream.

use std::error::Error;
use std::fmt;
use std::fmt::Display;
use std::fmt::Formatter;
use std::io;
use std::io::Read;
use std::io::Seek;
use std::io::Write;

use crate::ExtractionPlan;
use crate::ExtractionPlanError;
use crate::ExtractionPlanLimits;
use crate::LogOrderError;
use crate::LogOrderLocator;
use crate::OrderedMergeError;
use crate::OrderedMergeLimits;
use crate::OrderedMergeTable;
use crate::OrderedRowMerge;
use crate::RecordBindError;
use crate::RecordError;
use crate::RecordLimits;
use crate::RecordProgram;
use crate::RecordScratch;
use crate::RecordWriter;
use crate::archive::ArchiveCatalog;
use crate::archive::ArchiveCatalogError;
use crate::archive::ArchiveCatalogLimits;
use crate::archive::ArchiveMetadata;
use crate::archive::ColumnLimits;
use crate::archive::DecodedPackedStream;
use crate::archive::DecodedSchemaTable;
use crate::archive::DirectoryArchiveReader;
use crate::archive::DirectoryArchiveSource;
use crate::archive::PackedStreamError;
use crate::archive::PackedStreamLimits;
use crate::archive::SingleFileArchiveReader;
use crate::archive::TableMetadata;
use crate::archive::TableStreamError;
use crate::json::JsonBytePolicy;

const MEBIBYTE: u64 = 1024 * 1024;

/// Ordering applied while extracting an archive.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub enum ExtractionMode {
    /// Emit records in physical packed-stream and schema-table order.
    #[default]
    Unordered,
    /// Merge all tables by their canonical metadata `log_event_idx` columns.
    LogOrder,
}

/// Aggregate state retained only by [`ExtractionMode::LogOrder`].
///
/// Defaults deliberately cap schema churn; callers with unusually many streams or tables can use
/// [`Self::new`] to raise the corresponding limits explicitly.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OrderedRetentionLimits {
    streams: u64,
    tables: u64,
    records: u64,
    decoded_bytes: u64,
}

impl OrderedRetentionLimits {
    /// Creates explicit aggregate log-order retention limits.
    ///
    /// `max_decoded_bytes` is compared with the checked sum of advertised decompressed packed-
    /// stream sizes from validated table metadata, before any packed stream is read.
    #[must_use]
    pub const fn new(
        max_streams: u64,
        max_tables: u64,
        max_records: u64,
        max_decoded_bytes: u64,
    ) -> Self {
        Self {
            streams: max_streams,
            tables: max_tables,
            records: max_records,
            decoded_bytes: max_decoded_bytes,
        }
    }

    /// Maximum packed-stream buffers retained simultaneously.
    #[must_use]
    pub const fn max_streams(self) -> u64 {
        self.streams
    }

    /// Maximum decoded schema-table views retained simultaneously.
    #[must_use]
    pub const fn max_tables(self) -> u64 {
        self.tables
    }

    /// Maximum aggregate records across retained tables.
    #[must_use]
    pub const fn max_records(self) -> u64 {
        self.records
    }

    /// Maximum sum of advertised decompressed packed-stream sizes.
    #[must_use]
    pub const fn max_decoded_bytes(self) -> u64 {
        self.decoded_bytes
    }
}

impl Default for OrderedRetentionLimits {
    fn default() -> Self {
        // Bindings and services should be safe against extreme schema churn by default. Callers
        // handling intentionally larger archives can raise these limits explicitly.
        Self::new(65_536, 16_384, 128 * 1024 * 1024, 1024 * MEBIBYTE)
    }
}

/// Limits for every layer used by high-level extraction.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ExtractionLimits {
    catalog: ArchiveCatalogLimits,
    packed_stream: PackedStreamLimits,
    columns: ColumnLimits,
    plan: ExtractionPlanLimits,
    record: RecordLimits,
    ordered_retention: OrderedRetentionLimits,
    ordered_merge: OrderedMergeLimits,
}

impl ExtractionLimits {
    /// Replaces limits for loading and cross-validating non-table archive state.
    #[must_use]
    pub const fn with_catalog(mut self, limits: ArchiveCatalogLimits) -> Self {
        self.catalog = limits;
        self
    }

    /// Replaces per-packed-stream decompression limits.
    #[must_use]
    pub const fn with_packed_stream(mut self, limits: PackedStreamLimits) -> Self {
        self.packed_stream = limits;
        self
    }

    /// Replaces per-schema-table column-decoding limits.
    #[must_use]
    pub const fn with_columns(mut self, limits: ColumnLimits) -> Self {
        self.columns = limits;
        self
    }

    /// Replaces per-schema extraction-plan limits.
    #[must_use]
    pub const fn with_plan(mut self, limits: ExtractionPlanLimits) -> Self {
        self.plan = limits;
        self
    }

    /// Replaces per-program record and scratch limits.
    #[must_use]
    pub const fn with_record(mut self, limits: RecordLimits) -> Self {
        self.record = limits;
        self
    }

    /// Replaces aggregate log-order retention limits.
    #[must_use]
    pub const fn with_ordered_retention(mut self, limits: OrderedRetentionLimits) -> Self {
        self.ordered_retention = limits;
        self
    }

    /// Replaces ordered k-way merge limits and domain validation bounds.
    #[must_use]
    pub const fn with_ordered_merge(mut self, limits: OrderedMergeLimits) -> Self {
        self.ordered_merge = limits;
        self
    }

    /// Returns archive-catalog limits.
    #[must_use]
    pub const fn catalog(self) -> ArchiveCatalogLimits {
        self.catalog
    }

    /// Returns per-packed-stream limits.
    #[must_use]
    pub const fn packed_stream(self) -> PackedStreamLimits {
        self.packed_stream
    }

    /// Returns per-table column limits.
    #[must_use]
    pub const fn columns(self) -> ColumnLimits {
        self.columns
    }

    /// Returns extraction-plan limits.
    #[must_use]
    pub const fn plan(self) -> ExtractionPlanLimits {
        self.plan
    }

    /// Returns record-program and serialization limits.
    #[must_use]
    pub const fn record(self) -> RecordLimits {
        self.record
    }

    /// Returns aggregate log-order retention limits.
    #[must_use]
    pub const fn ordered_retention(self) -> OrderedRetentionLimits {
        self.ordered_retention
    }

    /// Returns ordered merge limits.
    #[must_use]
    pub const fn ordered_merge(self) -> OrderedMergeLimits {
        self.ordered_merge
    }
}

/// High-level extraction configuration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExtractionOptions {
    mode: ExtractionMode,
    byte_policy: JsonBytePolicy,
    limits: ExtractionLimits,
}

impl ExtractionOptions {
    /// Creates options for the requested ordering with strict UTF-8 and default limits.
    #[must_use]
    pub fn new(mode: ExtractionMode) -> Self {
        Self {
            mode,
            byte_policy: JsonBytePolicy::StrictUtf8,
            limits: ExtractionLimits::default(),
        }
    }

    /// Selects strict UTF-8 or explicit C++ byte-preserving extraction.
    #[must_use]
    pub const fn with_byte_policy(mut self, byte_policy: JsonBytePolicy) -> Self {
        self.byte_policy = byte_policy;
        self
    }

    /// Replaces every extraction-layer limit.
    #[must_use]
    pub const fn with_limits(mut self, limits: ExtractionLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Returns the requested extraction order.
    #[must_use]
    pub const fn mode(self) -> ExtractionMode {
        self.mode
    }

    /// Returns the archive-byte policy.
    #[must_use]
    pub const fn byte_policy(self) -> JsonBytePolicy {
        self.byte_policy
    }

    /// Returns all configured limits.
    #[must_use]
    pub const fn limits(self) -> ExtractionLimits {
        self.limits
    }
}

impl Default for ExtractionOptions {
    fn default() -> Self {
        Self::new(ExtractionMode::Unordered)
    }
}

/// Successful extraction counters.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ExtractionStats {
    streams: u64,
    tables: u64,
    records: u64,
    decoded_bytes: u64,
    output_bytes: u64,
}

impl ExtractionStats {
    /// Returns successfully decoded packed streams.
    #[must_use]
    pub const fn streams(self) -> u64 {
        self.streams
    }

    /// Returns successfully decoded schema tables.
    #[must_use]
    pub const fn tables(self) -> u64 {
        self.tables
    }

    /// Returns complete records successfully written to the sink.
    #[must_use]
    pub const fn records(self) -> u64 {
        self.records
    }

    /// Returns the sum of advertised decompressed sizes for successfully decoded packed streams.
    #[must_use]
    pub const fn decoded_bytes(self) -> u64 {
        self.decoded_bytes
    }

    /// Returns JSONL bytes counted only after a complete successful record-sink call.
    #[must_use]
    pub const fn output_bytes(self) -> u64 {
        self.output_bytes
    }

    fn add_stream(&mut self, decoded_bytes: u64) -> Result<(), ExtractionError> {
        self.streams = self
            .streams
            .checked_add(1)
            .ok_or(ExtractionError::SizeOverflow)?;
        self.decoded_bytes = self
            .decoded_bytes
            .checked_add(decoded_bytes)
            .ok_or(ExtractionError::SizeOverflow)?;
        Ok(())
    }

    fn add_table(&mut self) -> Result<(), ExtractionError> {
        self.tables = self
            .tables
            .checked_add(1)
            .ok_or(ExtractionError::SizeOverflow)?;
        Ok(())
    }
}

/// Extraction resource named by a limit or bounded-allocation error.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ExtractionResource {
    /// Retained decompressed packed streams.
    PackedStreams,
    /// Retained decoded schema-table views.
    SchemaTables,
    /// Aggregate advertised records.
    Records,
    /// Sum of advertised decompressed packed-stream sizes.
    DecodedBytes,
    /// Compiled record-program collection.
    RecordPrograms,
    /// Table-bound record-writer collection.
    RecordWriters,
    /// Ordered-merge table coordinates.
    MergeTables,
    /// Reusable record buffer and JSONL newline.
    RecordBuffer,
}

impl Display for ExtractionResource {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::PackedStreams => "extraction packed streams",
            Self::SchemaTables => "extraction schema tables",
            Self::Records => "extraction records",
            Self::DecodedBytes => "extraction decoded stream bytes",
            Self::RecordPrograms => "extraction record programs",
            Self::RecordWriters => "extraction record writers",
            Self::MergeTables => "extraction merge tables",
            Self::RecordBuffer => "JSONL record buffer",
        })
    }
}

/// One complete borrowed JSONL record passed to a caller-owned sink.
///
/// The bytes include the trailing newline and remain valid only for the duration of
/// [`JsonlRecordSink::write_record`]. Log-event indexes are present only in
/// [`ExtractionMode::LogOrder`], where the ordered merge has validated their complete contiguous
/// archive domain.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JsonlRecord<'a> {
    bytes: &'a [u8],
    table_index: usize,
    row_index: usize,
    log_event_idx: Option<u64>,
}

impl<'a> JsonlRecord<'a> {
    pub(crate) const fn new(
        bytes: &'a [u8],
        table_index: usize,
        row_index: usize,
        log_event_idx: Option<u64>,
    ) -> Self {
        Self {
            bytes,
            table_index,
            row_index,
            log_event_idx,
        }
    }

    /// Returns the complete JSONL record, including its trailing newline.
    #[must_use]
    pub const fn jsonl_bytes(self) -> &'a [u8] {
        self.bytes
    }

    /// Returns the JSON document without its trailing newline.
    #[must_use]
    pub fn json_bytes(self) -> &'a [u8] {
        self.bytes.strip_suffix(b"\n").unwrap_or(self.bytes)
    }

    /// Returns the stable global physical table index.
    #[must_use]
    pub const fn table_index(self) -> usize {
        self.table_index
    }

    /// Returns the zero-based row index within the physical table.
    #[must_use]
    pub const fn row_index(self) -> usize {
        self.row_index
    }

    /// Returns the canonical archive-local log-event index in ordered mode.
    #[must_use]
    pub const fn log_event_idx(self) -> Option<u64> {
        self.log_event_idx
    }
}

/// Synchronous borrowed-record destination used by high-level extraction.
///
/// Implementations can rotate files, call an FFI callback, frame network messages, or aggregate
/// statistics without rediscovering JSONL record boundaries. The extraction engine invokes this
/// method exactly once for each completely formatted record and never retains sink-owned state.
pub trait JsonlRecordSink {
    /// Consumes one borrowed record.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when the destination cannot accept the record. The sink may have
    /// accepted a prefix before returning an error.
    fn write_record(&mut self, record: JsonlRecord<'_>) -> io::Result<()>;
}

/// Format-independent archive operations required by high-level extraction.
///
/// The trait is object-safe so a binding or service can select a single-file or directory reader
/// at runtime without duplicating the extraction pipeline. Implementations retain ownership of
/// their underlying seekable sources and load `/0` one packed stream at a time.
pub trait ArchiveReader {
    /// Loads and cross-validates all non-table archive state.
    ///
    /// # Errors
    ///
    /// Returns a section-specific decode or cross-validation error.
    fn read_catalog(
        &mut self,
        limits: ArchiveCatalogLimits,
    ) -> Result<ArchiveCatalog, ArchiveCatalogError>;

    /// Reads and validates one packed stream described by `catalog` state.
    ///
    /// # Errors
    ///
    /// Returns an error for inconsistent metadata, I/O, decompression, corruption, or a resource
    /// limit violation.
    fn read_packed_stream(
        &mut self,
        metadata: &ArchiveMetadata,
        table_metadata: &TableMetadata,
        stream_id: usize,
        limits: PackedStreamLimits,
    ) -> Result<DecodedPackedStream, PackedStreamError>;
}

impl<R: Read + Seek> ArchiveReader for SingleFileArchiveReader<R> {
    fn read_catalog(
        &mut self,
        limits: ArchiveCatalogLimits,
    ) -> Result<ArchiveCatalog, ArchiveCatalogError> {
        Self::read_catalog(self, limits)
    }

    fn read_packed_stream(
        &mut self,
        metadata: &ArchiveMetadata,
        table_metadata: &TableMetadata,
        stream_id: usize,
        limits: PackedStreamLimits,
    ) -> Result<DecodedPackedStream, PackedStreamError> {
        Self::read_packed_stream(self, metadata, table_metadata, stream_id, limits)
    }
}

impl<S: DirectoryArchiveSource> ArchiveReader for DirectoryArchiveReader<S> {
    fn read_catalog(
        &mut self,
        limits: ArchiveCatalogLimits,
    ) -> Result<ArchiveCatalog, ArchiveCatalogError> {
        Self::read_catalog(self, limits)
    }

    fn read_packed_stream(
        &mut self,
        metadata: &ArchiveMetadata,
        table_metadata: &TableMetadata,
        stream_id: usize,
        limits: PackedStreamLimits,
    ) -> Result<DecodedPackedStream, PackedStreamError> {
        Self::read_packed_stream(self, metadata, table_metadata, stream_id, limits)
    }
}

/// Extracts one archive into a caller-owned JSONL sink.
///
/// The sink is appended to and is never flushed by this function. Each row is fully formatted in
/// a reusable buffer before [`Write::write_all`] is called. A sink error can still leave a partial
/// final record because generic [`Write`] implementations are not transactional; output statistics
/// count a record's bytes only after `write_all` succeeds.
///
/// # Errors
///
/// Returns a contextual error from archive loading, stream/table decoding, plan/program creation,
/// record formatting, log-order validation, bounded allocation, or sink output.
pub fn extract_jsonl<A: ArchiveReader + ?Sized, W: Write + ?Sized>(
    reader: &mut A,
    output: &mut W,
    options: ExtractionOptions,
) -> Result<ExtractionStats, ExtractionError> {
    extract_jsonl_records(reader, &mut WriteRecordSink(output), options)
}

/// Extracts one archive through a caller-owned record-boundary sink.
///
/// The reusable JSONL buffer is borrowed only during each sink call. This function owns no paths,
/// performs no flushing, and does not retain sink data. Ordered records include their validated
/// canonical log-event indexes; unordered records expose physical table and row coordinates only.
///
/// # Errors
///
/// Returns a contextual error from archive loading, stream/table decoding, plan/program creation,
/// record formatting, log-order validation, bounded allocation, or the record sink.
pub fn extract_jsonl_records<A: ArchiveReader + ?Sized, S: JsonlRecordSink + ?Sized>(
    reader: &mut A,
    sink: &mut S,
    options: ExtractionOptions,
) -> Result<ExtractionStats, ExtractionError> {
    let catalog = reader
        .read_catalog(options.limits.catalog)
        .map_err(ExtractionError::Catalog)?;
    let mut state = RunState {
        sink,
        record: Vec::new(),
        stats: ExtractionStats::default(),
        options,
    };
    match options.mode {
        ExtractionMode::Unordered => extract_unordered(reader, &catalog, &mut state)?,
        ExtractionMode::LogOrder => extract_ordered(reader, &catalog, &mut state)?,
    }
    validate_final_record_count(&catalog, state.stats.records)?;
    Ok(state.stats)
}

struct WriteRecordSink<'a, W: ?Sized>(&'a mut W);

impl<W: Write + ?Sized> JsonlRecordSink for WriteRecordSink<'_, W> {
    fn write_record(&mut self, record: JsonlRecord<'_>) -> io::Result<()> {
        self.0.write_all(record.jsonl_bytes())
    }
}

struct RunState<'a, S: ?Sized> {
    sink: &'a mut S,
    record: Vec<u8>,
    stats: ExtractionStats,
    options: ExtractionOptions,
}

fn extract_unordered<A: ArchiveReader + ?Sized, S: JsonlRecordSink + ?Sized>(
    reader: &mut A,
    catalog: &ArchiveCatalog,
    state: &mut RunState<'_, S>,
) -> Result<(), ExtractionError> {
    let mut scratch = RecordScratch::new();
    let stream_count = catalog.table_metadata().packed_streams().len();
    for stream_index in 0..stream_count {
        let stream = read_stream(reader, catalog, stream_index, &state.options.limits)?;
        state.stats.add_stream(len_u64(stream.len())?)?;
        let stream_id = len_u64(stream_index)?;
        let tables = catalog
            .schema_tables(stream_id, &stream, state.options.limits.columns)
            .map_err(|source| ExtractionError::TableStream { stream_id, source })?;
        for decoded in tables {
            let decoded =
                decoded.map_err(|source| ExtractionError::TableStream { stream_id, source })?;
            state.stats.add_table()?;
            let plan = compile_plan(&decoded, catalog, &state.options)?;
            let program = compile_program(&plan, &decoded, catalog, &state.options)?;
            let mut writer = program
                .writer_with_scratch(decoded.table(), catalog.timestamp_patterns(), scratch)
                .map_err(|source| ExtractionError::RecordBind {
                    table_index: decoded.table_index(),
                    schema_id: decoded.schema().id(),
                    source,
                })?;
            drain_writer(&mut writer, decoded.table_index(), state)?;
            scratch = writer.into_scratch();
        }
    }
    Ok(())
}

fn extract_ordered<A: ArchiveReader + ?Sized, S: JsonlRecordSink + ?Sized>(
    reader: &mut A,
    catalog: &ArchiveCatalog,
    state: &mut RunState<'_, S>,
) -> Result<(), ExtractionError> {
    let locator = LogOrderLocator::discover(catalog.schema_tree())
        .map_err(ExtractionError::LogOrderDiscovery)?
        .ok_or(ExtractionError::MissingLogOrderColumn)?;
    let counts = ordered_preflight(catalog, state.options.limits.ordered_retention)?;
    let streams = read_all_streams(reader, catalog, state, counts.streams)?;
    let tables = decode_all_tables(catalog, &streams, state, counts.tables)?;
    let programs = compile_all_programs(catalog, &tables, &state.options)?;
    let merge_tables = build_merge_tables(locator, &tables)?;
    let mut writers = bind_all_writers(catalog, &programs, &tables)?;
    let mut merge = OrderedRowMerge::new(&merge_tables, state.options.limits.ordered_merge)
        .map_err(ExtractionError::OrderedMerge)?;
    for row in &mut merge {
        let row = row.map_err(ExtractionError::OrderedMerge)?;
        let writer = writers.get_mut(row.table_index()).ok_or_else(|| {
            ExtractionError::OrderedRowMissingWriter {
                table_index: row.table_index(),
                row_index: row.row_index(),
            }
        })?;
        if writer.next_row_index() != row.row_index() {
            return Err(ExtractionError::OrderedRowMismatch {
                table_index: row.table_index(),
                expected: writer.next_row_index(),
                actual: row.row_index(),
            });
        }
        let log_event_idx =
            u64::try_from(row.log_event_idx()).map_err(|_| ExtractionError::SizeOverflow)?;
        if !write_one_record(writer, row.table_index(), Some(log_event_idx), state)? {
            return Err(ExtractionError::OrderedRowExhausted {
                table_index: row.table_index(),
                row_index: row.row_index(),
            });
        }
    }
    validate_writers_consumed(&writers)?;
    Ok(())
}

#[derive(Clone, Copy)]
struct OrderedCounts {
    streams: usize,
    tables: usize,
}

fn ordered_preflight(
    catalog: &ArchiveCatalog,
    limits: OrderedRetentionLimits,
) -> Result<OrderedCounts, ExtractionError> {
    let metadata = catalog.table_metadata();
    let stream_count = len_u64(metadata.packed_streams().len())?;
    let table_count = len_u64(metadata.schema_tables().len())?;
    let record_count = metadata.record_count();
    let decoded_bytes = metadata.total_uncompressed_stream_size();
    check_limit(
        ExtractionResource::PackedStreams,
        stream_count,
        limits.streams,
    )?;
    check_limit(ExtractionResource::SchemaTables, table_count, limits.tables)?;
    check_limit(ExtractionResource::Records, record_count, limits.records)?;
    check_limit(
        ExtractionResource::DecodedBytes,
        decoded_bytes,
        limits.decoded_bytes,
    )?;
    Ok(OrderedCounts {
        streams: metadata.packed_streams().len(),
        tables: metadata.schema_tables().len(),
    })
}

fn read_stream<A: ArchiveReader + ?Sized>(
    reader: &mut A,
    catalog: &ArchiveCatalog,
    stream_index: usize,
    limits: &ExtractionLimits,
) -> Result<DecodedPackedStream, ExtractionError> {
    reader
        .read_packed_stream(
            catalog.metadata(),
            catalog.table_metadata(),
            stream_index,
            limits.packed_stream,
        )
        .map_err(|source| ExtractionError::PackedStream {
            stream_id: stream_index,
            source,
        })
}

fn read_all_streams<A: ArchiveReader + ?Sized, S: JsonlRecordSink + ?Sized>(
    reader: &mut A,
    catalog: &ArchiveCatalog,
    state: &mut RunState<'_, S>,
    stream_count: usize,
) -> Result<Vec<DecodedPackedStream>, ExtractionError> {
    let mut streams = reserve_vector(stream_count, ExtractionResource::PackedStreams)?;
    for stream_index in 0..stream_count {
        let stream = read_stream(reader, catalog, stream_index, &state.options.limits)?;
        state.stats.add_stream(len_u64(stream.len())?)?;
        streams.push(stream);
    }
    Ok(streams)
}

fn decode_all_tables<'stream, 'archive, S: JsonlRecordSink + ?Sized>(
    catalog: &'archive ArchiveCatalog,
    streams: &'stream [DecodedPackedStream],
    state: &mut RunState<'_, S>,
    table_count: usize,
) -> Result<Vec<DecodedSchemaTable<'stream, 'archive>>, ExtractionError> {
    let mut decoded_tables = reserve_vector(table_count, ExtractionResource::SchemaTables)?;
    for (stream_index, stream) in streams.iter().enumerate() {
        let stream_id = len_u64(stream_index)?;
        let tables = catalog
            .schema_tables(stream_id, stream, state.options.limits.columns)
            .map_err(|source| ExtractionError::TableStream { stream_id, source })?;
        for decoded in tables {
            let decoded =
                decoded.map_err(|source| ExtractionError::TableStream { stream_id, source })?;
            let expected = decoded_tables.len();
            if decoded.table_index() != expected {
                return Err(ExtractionError::PhysicalTableOrder {
                    expected,
                    actual: decoded.table_index(),
                });
            }
            state.stats.add_table()?;
            decoded_tables.push(decoded);
        }
    }
    if decoded_tables.len() != table_count {
        return Err(ExtractionError::TableCountMismatch {
            expected: table_count,
            actual: decoded_tables.len(),
        });
    }
    Ok(decoded_tables)
}

fn compile_all_programs(
    catalog: &ArchiveCatalog,
    tables: &[DecodedSchemaTable<'_, '_>],
    options: &ExtractionOptions,
) -> Result<Vec<RecordProgram>, ExtractionError> {
    let mut programs = reserve_vector(tables.len(), ExtractionResource::RecordPrograms)?;
    for table in tables {
        let plan = compile_plan(table, catalog, options)?;
        programs.push(compile_program(&plan, table, catalog, options)?);
    }
    Ok(programs)
}

fn compile_plan(
    decoded: &DecodedSchemaTable<'_, '_>,
    catalog: &ArchiveCatalog,
    options: &ExtractionOptions,
) -> Result<ExtractionPlan, ExtractionError> {
    ExtractionPlan::compile(decoded.schema(), catalog.schema_tree(), options.limits.plan).map_err(
        |source| ExtractionError::Plan {
            table_index: decoded.table_index(),
            schema_id: decoded.schema().id(),
            source,
        },
    )
}

fn compile_program(
    plan: &ExtractionPlan,
    decoded: &DecodedSchemaTable<'_, '_>,
    catalog: &ArchiveCatalog,
    options: &ExtractionOptions,
) -> Result<RecordProgram, ExtractionError> {
    RecordProgram::compile_with_byte_policy(
        plan,
        catalog.schema_tree(),
        options.byte_policy,
        options.limits.record,
    )
    .map_err(|source| ExtractionError::RecordProgram {
        table_index: decoded.table_index(),
        schema_id: decoded.schema().id(),
        source,
    })
}

fn build_merge_tables<'table>(
    locator: LogOrderLocator<'_>,
    tables: &[DecodedSchemaTable<'table, '_>],
) -> Result<Vec<OrderedMergeTable<'table>>, ExtractionError> {
    let mut merge_tables = reserve_vector(tables.len(), ExtractionResource::MergeTables)?;
    for table in tables {
        let order_column = locator
            .locate(table.schema(), table.table())
            .map_err(|source| ExtractionError::TableLogOrder {
                table_index: table.table_index(),
                schema_id: table.schema().id(),
                source,
            })?
            .ok_or_else(|| ExtractionError::TableMissingLogOrder {
                table_index: table.table_index(),
                schema_id: table.schema().id(),
            })?;
        merge_tables.push(OrderedMergeTable::new(table.table_index(), order_column));
    }
    Ok(merge_tables)
}

fn bind_all_writers<'program, 'table, 'archive, 'catalog>(
    catalog: &'catalog ArchiveCatalog,
    programs: &'program [RecordProgram],
    tables: &[DecodedSchemaTable<'table, 'archive>],
) -> Result<Vec<RecordWriter<'program, 'table, 'archive, 'catalog>>, ExtractionError> {
    let mut writers = reserve_vector(tables.len(), ExtractionResource::RecordWriters)?;
    for (program, table) in programs.iter().zip(tables) {
        writers.push(
            program
                .writer(table.table(), catalog.timestamp_patterns())
                .map_err(|source| ExtractionError::RecordBind {
                    table_index: table.table_index(),
                    schema_id: table.schema().id(),
                    source,
                })?,
        );
    }
    Ok(writers)
}

fn drain_writer<S: JsonlRecordSink + ?Sized>(
    writer: &mut RecordWriter<'_, '_, '_, '_>,
    table_index: usize,
    state: &mut RunState<'_, S>,
) -> Result<(), ExtractionError> {
    while write_one_record(writer, table_index, None, state)? {}
    Ok(())
}

fn write_one_record<S: JsonlRecordSink + ?Sized>(
    writer: &mut RecordWriter<'_, '_, '_, '_>,
    table_index: usize,
    log_event_idx: Option<u64>,
    state: &mut RunState<'_, S>,
) -> Result<bool, ExtractionError> {
    let row_index = writer.next_row_index();
    if !writer
        .append_next_record(&mut state.record)
        .map_err(|source| ExtractionError::Record {
            table_index,
            row_index,
            source,
        })?
    {
        return Ok(false);
    }
    append_newline(&mut state.record)?;
    let attempted_bytes = len_u64(state.record.len())?;
    let next_records = state
        .stats
        .records
        .checked_add(1)
        .ok_or(ExtractionError::SizeOverflow)?;
    let next_output_bytes = state
        .stats
        .output_bytes
        .checked_add(attempted_bytes)
        .ok_or(ExtractionError::SizeOverflow)?;
    let record = JsonlRecord::new(&state.record, table_index, row_index, log_event_idx);
    state
        .sink
        .write_record(record)
        .map_err(|source| ExtractionError::Output {
            table_index,
            row_index,
            log_event_idx,
            completed_records: state.stats.records,
            attempted_bytes,
            source,
        })?;
    state.stats.records = next_records;
    state.stats.output_bytes = next_output_bytes;
    state.record.clear();
    Ok(true)
}

fn append_newline(record: &mut Vec<u8>) -> Result<(), ExtractionError> {
    record
        .len()
        .checked_add(1)
        .ok_or(ExtractionError::SizeOverflow)?;
    record
        .try_reserve_exact(1)
        .map_err(|_| ExtractionError::AllocationFailed {
            resource: ExtractionResource::RecordBuffer,
            requested: 1,
        })?;
    record.push(b'\n');
    Ok(())
}

fn validate_writers_consumed(
    writers: &[RecordWriter<'_, '_, '_, '_>],
) -> Result<(), ExtractionError> {
    if let Some((table_index, writer)) = writers
        .iter()
        .enumerate()
        .find(|(_, writer)| 0 != writer.remaining())
    {
        return Err(ExtractionError::UnconsumedTableRows {
            table_index,
            remaining: writer.remaining(),
        });
    }
    Ok(())
}

const fn validate_final_record_count(
    catalog: &ArchiveCatalog,
    actual: u64,
) -> Result<(), ExtractionError> {
    let expected = catalog.table_metadata().record_count();
    if actual != expected {
        return Err(ExtractionError::RecordCountMismatch { expected, actual });
    }
    Ok(())
}

fn reserve_vector<T>(
    count: usize,
    resource: ExtractionResource,
) -> Result<Vec<T>, ExtractionError> {
    let mut values = Vec::new();
    values
        .try_reserve_exact(count)
        .map_err(|_| ExtractionError::AllocationFailed {
            resource,
            requested: count,
        })?;
    Ok(values)
}

const fn check_limit(
    resource: ExtractionResource,
    actual: u64,
    limit: u64,
) -> Result<(), ExtractionError> {
    if actual > limit {
        return Err(ExtractionError::LimitExceeded {
            resource,
            actual,
            limit,
        });
    }
    Ok(())
}

fn len_u64(value: usize) -> Result<u64, ExtractionError> {
    u64::try_from(value).map_err(|_| ExtractionError::SizeOverflow)
}

/// Failure during high-level archive extraction.
#[derive(Debug)]
#[non_exhaustive]
pub enum ExtractionError {
    /// Non-table archive loading or cross-validation failed.
    Catalog(ArchiveCatalogError),
    /// One packed stream could not be read or decompressed.
    PackedStream {
        /// Zero-based packed-stream ID.
        stream_id: usize,
        /// Stream-layer failure.
        source: PackedStreamError,
    },
    /// A packed stream's table selection or lazy table decode failed.
    TableStream {
        /// Zero-based packed-stream ID.
        stream_id: u64,
        /// Table-stream failure.
        source: TableStreamError,
    },
    /// One schema extraction plan could not be compiled.
    Plan {
        /// Global physical table index.
        table_index: usize,
        /// Opaque schema ID.
        schema_id: i32,
        /// Plan compiler failure.
        source: ExtractionPlanError,
    },
    /// One record program could not be compiled.
    RecordProgram {
        /// Global physical table index.
        table_index: usize,
        /// Opaque schema ID.
        schema_id: i32,
        /// Record compiler failure.
        source: crate::RecordCompileError,
    },
    /// A record program could not bind to its decoded table.
    RecordBind {
        /// Global physical table index.
        table_index: usize,
        /// Opaque schema ID.
        schema_id: i32,
        /// Table/program mismatch or unsupported legacy value.
        source: RecordBindError,
    },
    /// One complete record could not be formatted.
    Record {
        /// Global physical table index.
        table_index: usize,
        /// Zero-based table-local row.
        row_index: usize,
        /// Record formatting failure.
        source: RecordError,
    },
    /// Archive-level canonical log-order discovery failed.
    LogOrderDiscovery(LogOrderError),
    /// Log-order mode was requested but the archive has no canonical metadata column.
    MissingLogOrderColumn,
    /// One table's schema/column correspondence failed log-order validation.
    TableLogOrder {
        /// Global physical table index.
        table_index: usize,
        /// Opaque schema ID.
        schema_id: i32,
        /// Locator failure.
        source: LogOrderError,
    },
    /// One table omits the archive's canonical log-order column.
    TableMissingLogOrder {
        /// Global physical table index.
        table_index: usize,
        /// Opaque schema ID.
        schema_id: i32,
    },
    /// Ordered k-way merge construction or iteration failed.
    OrderedMerge(OrderedMergeError),
    /// Lazy decoding returned tables outside their global physical index order.
    PhysicalTableOrder {
        /// Required next table index.
        expected: usize,
        /// Returned table index.
        actual: usize,
    },
    /// The number of lazily decoded tables disagrees with validated metadata.
    TableCountMismatch {
        /// Advertised table count.
        expected: usize,
        /// Decoded table count.
        actual: usize,
    },
    /// An ordered row references no table-bound writer.
    OrderedRowMissingWriter {
        /// Referenced global table index.
        table_index: usize,
        /// Referenced table-local row.
        row_index: usize,
    },
    /// An ordered coordinate does not match its table writer's sequential cursor.
    OrderedRowMismatch {
        /// Referenced global table index.
        table_index: usize,
        /// Writer's required next row.
        expected: usize,
        /// Merge-provided row.
        actual: usize,
    },
    /// An ordered coordinate references a table writer that is already exhausted.
    OrderedRowExhausted {
        /// Referenced global table index.
        table_index: usize,
        /// Referenced table-local row.
        row_index: usize,
    },
    /// An ordered merge ended while one table writer still had rows.
    UnconsumedTableRows {
        /// Global table index.
        table_index: usize,
        /// Rows left in the writer.
        remaining: usize,
    },
    /// Successfully written records disagree with validated table metadata.
    RecordCountMismatch {
        /// Advertised aggregate records.
        expected: u64,
        /// Successfully written records.
        actual: u64,
    },
    /// An aggregate log-order retention bound was exceeded before growth.
    LimitExceeded {
        /// Bounded resource.
        resource: ExtractionResource,
        /// Advertised amount.
        actual: u64,
        /// Configured maximum.
        limit: u64,
    },
    /// A bounded orchestration allocation failed.
    AllocationFailed {
        /// State being allocated.
        resource: ExtractionResource,
        /// Requested elements or additional bytes.
        requested: usize,
    },
    /// Checked count, index, or byte arithmetic overflowed.
    SizeOverflow,
    /// The caller-owned sink failed while writing one already-formatted JSONL record.
    Output {
        /// Global physical table index.
        table_index: usize,
        /// Zero-based table-local row.
        row_index: usize,
        /// Canonical archive-local event index in ordered mode.
        log_event_idx: Option<u64>,
        /// Complete records successfully written before this attempt.
        completed_records: u64,
        /// Complete bytes passed to `write_all`; the sink may have accepted a prefix.
        attempted_bytes: u64,
        /// Caller sink failure.
        source: io::Error,
    },
}

impl Display for ExtractionError {
    #[allow(clippy::too_many_lines)]
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Catalog(source) => write!(formatter, "archive catalog failed: {source}"),
            Self::PackedStream { stream_id, source } => {
                write!(formatter, "packed stream {stream_id} failed: {source}")
            }
            Self::TableStream { stream_id, source } => {
                write!(
                    formatter,
                    "tables in packed stream {stream_id} failed: {source}"
                )
            }
            Self::Plan {
                table_index,
                schema_id,
                source,
            } => write!(
                formatter,
                "table {table_index} schema {schema_id} extraction plan failed: {source}"
            ),
            Self::RecordProgram {
                table_index,
                schema_id,
                source,
            } => write!(
                formatter,
                "table {table_index} schema {schema_id} record program failed: {source}"
            ),
            Self::RecordBind {
                table_index,
                schema_id,
                source,
            } => write!(
                formatter,
                "table {table_index} schema {schema_id} record binding failed: {source}"
            ),
            Self::Record {
                table_index,
                row_index,
                source,
            } => write!(
                formatter,
                "table {table_index} row {row_index} formatting failed: {source}"
            ),
            Self::LogOrderDiscovery(source) => {
                write!(formatter, "log-order discovery failed: {source}")
            }
            Self::MissingLogOrderColumn => {
                formatter.write_str("archive has no canonical log-order column")
            }
            Self::TableLogOrder {
                table_index,
                schema_id,
                source,
            } => write!(
                formatter,
                "table {table_index} schema {schema_id} log-order validation failed: {source}"
            ),
            Self::TableMissingLogOrder {
                table_index,
                schema_id,
            } => write!(
                formatter,
                "table {table_index} schema {schema_id} omits the canonical log-order column"
            ),
            Self::OrderedMerge(source) => write!(formatter, "ordered merge failed: {source}"),
            Self::PhysicalTableOrder { expected, actual } => write!(
                formatter,
                "decoded physical table index {actual}, expected {expected}"
            ),
            Self::TableCountMismatch { expected, actual } => write!(
                formatter,
                "decoded {actual} schema tables, metadata advertises {expected}"
            ),
            Self::OrderedRowMissingWriter {
                table_index,
                row_index,
            } => write!(
                formatter,
                "ordered row {row_index} references absent table writer {table_index}"
            ),
            Self::OrderedRowMismatch {
                table_index,
                expected,
                actual,
            } => write!(
                formatter,
                "ordered table {table_index} supplied row {actual}, writer expects {expected}"
            ),
            Self::OrderedRowExhausted {
                table_index,
                row_index,
            } => write!(
                formatter,
                "ordered row {row_index} references exhausted table writer {table_index}"
            ),
            Self::UnconsumedTableRows {
                table_index,
                remaining,
            } => write!(
                formatter,
                "ordered merge left {remaining} rows in table writer {table_index}"
            ),
            Self::RecordCountMismatch { expected, actual } => write!(
                formatter,
                "wrote {actual} records, table metadata advertises {expected}"
            ),
            Self::LimitExceeded {
                resource,
                actual,
                limit,
            } => write!(formatter, "{resource} {actual} exceeds limit {limit}"),
            Self::AllocationFailed {
                resource,
                requested,
            } => write!(
                formatter,
                "could not reserve {requested} elements or bytes for {resource}"
            ),
            Self::SizeOverflow => formatter.write_str("extraction size overflow"),
            Self::Output {
                table_index,
                row_index,
                log_event_idx: _,
                completed_records,
                attempted_bytes,
                source,
            } => write!(
                formatter,
                "sink failed writing {attempted_bytes} bytes for table {table_index} row \
                 {row_index} after {completed_records} complete records: {source}"
            ),
        }
    }
}

impl Error for ExtractionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Catalog(source) => Some(source),
            Self::PackedStream { source, .. } => Some(source),
            Self::TableStream { source, .. } => Some(source),
            Self::Plan { source, .. } => Some(source),
            Self::RecordProgram { source, .. } => Some(source),
            Self::RecordBind { source, .. } => Some(source),
            Self::Record { source, .. } => Some(source),
            Self::LogOrderDiscovery(source) | Self::TableLogOrder { source, .. } => Some(source),
            Self::OrderedMerge(source) => Some(source),
            Self::Output { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    const FIXTURE: &[u8] =
        include_bytes!("../tests/fixtures/sfa-v0.5.0-minimal-cpp.bin").as_slice();
    const EXPECTED: &[u8] =
        include_bytes!("../tests/fixtures/sfa-v0.5.0-minimal-cpp-input.jsonl").as_slice();
    const STRUCTURED_ARRAY_FIXTURE_HEX: &str =
        include_str!("../tests/fixtures/sfa-v0.5.0-structured-arrays-cpp.hex");
    const STRUCTURED_ARRAY_EXPECTED: &[u8] =
        include_bytes!("../tests/fixtures/sfa-v0.5.0-structured-arrays-cpp-search.jsonl");

    fn decode_hex(source: &str) -> Vec<u8> {
        let digits: Vec<u8> = source
            .bytes()
            .filter(|byte| !byte.is_ascii_whitespace())
            .collect();
        let (pairs, remainder) = digits.as_chunks::<2>();
        let bytes = pairs
            .iter()
            .map(|pair| (hex_nibble(pair[0]) << 4) | hex_nibble(pair[1]))
            .collect();
        assert!(remainder.is_empty(), "hex fixture has even length");
        bytes
    }

    fn hex_nibble(byte: u8) -> u8 {
        match byte {
            b'0'..=b'9' => byte - b'0',
            b'a'..=b'f' => byte - b'a' + 10,
            b'A'..=b'F' => byte - b'A' + 10,
            _ => panic!("invalid hex fixture byte"),
        }
    }

    #[derive(Default)]
    struct TrackingSink {
        bytes: Vec<u8>,
        flushes: usize,
    }

    impl Write for TrackingSink {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            self.flushes += 1;
            Ok(())
        }
    }

    struct FailingSink;

    impl Write for FailingSink {
        fn write(&mut self, _bytes: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("synthetic sink failure"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[derive(Default)]
    struct ContextSink {
        records: Vec<CapturedRecord>,
    }

    struct CapturedRecord {
        json: Vec<u8>,
        jsonl: Vec<u8>,
        table_index: usize,
        row_index: usize,
        log_event_idx: Option<u64>,
    }

    impl JsonlRecordSink for ContextSink {
        fn write_record(&mut self, record: JsonlRecord<'_>) -> io::Result<()> {
            self.records.push(CapturedRecord {
                json: record.json_bytes().to_vec(),
                jsonl: record.jsonl_bytes().to_vec(),
                table_index: record.table_index(),
                row_index: record.row_index(),
                log_event_idx: record.log_event_idx(),
            });
            Ok(())
        }
    }

    fn reader() -> SingleFileArchiveReader<Cursor<&'static [u8]>> {
        SingleFileArchiveReader::open(Cursor::new(FIXTURE)).expect("open committed C++ fixture")
    }

    #[test]
    fn both_modes_emit_exact_cpp_jsonl_without_flushing_the_sink() {
        for mode in [ExtractionMode::Unordered, ExtractionMode::LogOrder] {
            let mut reader = reader();
            let mut output = TrackingSink::default();
            let stats = extract_jsonl(&mut reader, &mut output, ExtractionOptions::new(mode))
                .expect("extract committed C++ fixture");

            assert_eq!(EXPECTED, output.bytes);
            assert_eq!(0, output.flushes);
            assert_eq!(1, stats.streams());
            assert_eq!(1, stats.tables());
            assert_eq!(1, stats.records());
            assert_eq!(57, stats.decoded_bytes());
            assert_eq!(
                u64::try_from(EXPECTED.len()).expect("fixture size fits u64"),
                stats.output_bytes()
            );
        }
    }

    #[test]
    fn extraction_accepts_a_runtime_selected_archive_reader() {
        let mut concrete = reader();
        let reader: &mut dyn ArchiveReader = &mut concrete;
        let mut output = Vec::new();

        let stats = extract_jsonl(
            reader,
            &mut output,
            ExtractionOptions::new(ExtractionMode::Unordered),
        )
        .expect("extract through object-safe archive reader");

        assert_eq!(EXPECTED, output);
        assert_eq!(1, stats.records());
    }

    #[test]
    fn unordered_extraction_reconstructs_exact_cpp_structured_arrays() {
        let bytes = decode_hex(STRUCTURED_ARRAY_FIXTURE_HEX);
        let mut reader = SingleFileArchiveReader::open(Cursor::new(bytes))
            .expect("open C++ structured-array fixture");
        let mut output = Vec::new();

        let stats = extract_jsonl(
            &mut reader,
            &mut output,
            ExtractionOptions::new(ExtractionMode::Unordered),
        )
        .expect("extract C++ structured arrays");

        assert_eq!(STRUCTURED_ARRAY_EXPECTED, output);
        assert_eq!(9, stats.records());
        assert_eq!(9, stats.tables());
        assert_eq!(1, stats.streams());
    }

    #[test]
    fn record_sink_receives_boundaries_coordinates_and_order_indexes() {
        for (mode, expected_log_event_idx) in [
            (ExtractionMode::Unordered, None),
            (ExtractionMode::LogOrder, Some(0)),
        ] {
            let mut reader = reader();
            let mut sink = ContextSink::default();

            let stats = extract_jsonl_records(&mut reader, &mut sink, ExtractionOptions::new(mode))
                .expect("extract through record-boundary sink");

            assert_eq!(1, stats.records());
            assert_eq!(1, sink.records.len());
            let captured = &sink.records[0];
            assert_eq!(EXPECTED, captured.jsonl);
            assert_eq!(&EXPECTED[..EXPECTED.len() - 1], captured.json);
            assert_eq!(0, captured.table_index);
            assert_eq!(0, captured.row_index);
            assert_eq!(expected_log_event_idx, captured.log_event_idx);
        }
    }

    #[test]
    fn ordered_retention_is_rejected_before_output() {
        let retention = OrderedRetentionLimits::new(1, 1, 1, 56);
        let limits = ExtractionLimits::default().with_ordered_retention(retention);
        let options = ExtractionOptions::new(ExtractionMode::LogOrder).with_limits(limits);
        let mut reader = reader();
        let mut output = Vec::new();

        assert!(matches!(
            extract_jsonl(&mut reader, &mut output, options),
            Err(ExtractionError::LimitExceeded {
                resource: ExtractionResource::DecodedBytes,
                actual: 57,
                limit: 56
            })
        ));
        assert_eq!(0, output.len());
    }

    #[test]
    fn sink_errors_report_only_completed_records_and_attempted_bytes() {
        let mut reader = reader();
        let mut output = FailingSink;
        let error = extract_jsonl(
            &mut reader,
            &mut output,
            ExtractionOptions::new(ExtractionMode::Unordered),
        )
        .expect_err("synthetic sink fails");

        assert!(matches!(
            error,
            ExtractionError::Output {
                table_index: 0,
                row_index: 0,
                completed_records: 0,
                attempted_bytes,
                ..
            } if attempted_bytes == u64::try_from(EXPECTED.len()).expect("fixture size fits")
        ));
    }
}
