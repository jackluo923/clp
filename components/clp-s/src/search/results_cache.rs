//! Library-first search adapters for the C++ results-cache contract.
//!
//! MongoDB is deliberately kept out of this module. Callers provide a synchronous batch sink, so
//! bindings can publish to another service or test batching and per-archive top-N behavior without
//! a database. The CLI supplies the optional MongoDB adapter.

use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::fmt::Display;
use std::fmt::Formatter;
use std::io;

use super::AggregationPlan;
use super::AggregationResultRef;
use super::AggregationSink;
use super::ArchiveMatchSink;
use super::ArchiveTableMatches;
use super::Projection;
use super::ProjectionError;
use super::projection::ResolvedProjection;
use crate::ExtractionPlan;
use crate::ExtractionPlanError;
use crate::ExtractionPlanLimits;
use crate::LogOrderError;
use crate::LogOrderLocator;
use crate::RecordBindError;
use crate::RecordCompileError;
use crate::RecordError;
use crate::RecordLimits;
use crate::RecordProgram;
use crate::RecordScratch;
use crate::RecordWriter;
use crate::archive::Column;
use crate::archive::ColumnData;
use crate::archive::DeltaI64Values;
use crate::json::JsonBytePolicy;

const DEFAULT_BATCH_SIZE: usize = 1000;
const DEFAULT_MAX_NUM_RESULTS: usize = 1000;
const NANOSECONDS_PER_MILLISECOND: i64 = 1_000_000;
const MILLISECONDS_PER_SECOND: f64 = 1_000.0;

/// Projection, reconstruction, batching, and top-N configuration for results-cache search.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchResultsCacheOptions {
    projection: Projection,
    plan: ExtractionPlanLimits,
    record: RecordLimits,
    byte_policy: JsonBytePolicy,
    batch_size: usize,
    max_num_results: usize,
}

impl SearchResultsCacheOptions {
    /// Creates options with explicit C++ results-cache batch and per-archive result limits.
    ///
    /// # Errors
    ///
    /// Returns an error when either limit is zero.
    pub fn new(
        projection: Projection,
        batch_size: usize,
        max_num_results: usize,
    ) -> Result<Self, ResultsCacheOptionsError> {
        if 0 == batch_size {
            return Err(ResultsCacheOptionsError::ZeroBatchSize);
        }
        if 0 == max_num_results {
            return Err(ResultsCacheOptionsError::ZeroMaxNumResults);
        }
        Ok(Self {
            projection,
            plan: ExtractionPlanLimits::default(),
            record: RecordLimits::default(),
            // BSON strings must be UTF-8. Valid archives retain the exact C++ JSON spelling.
            byte_policy: JsonBytePolicy::StrictUtf8,
            batch_size,
            max_num_results,
        })
    }

    /// Replaces extraction-plan compilation limits.
    #[must_use]
    pub const fn with_plan_limits(mut self, limits: ExtractionPlanLimits) -> Self {
        self.plan = limits;
        self
    }

    /// Replaces record-program and per-record limits.
    #[must_use]
    pub const fn with_record_limits(mut self, limits: RecordLimits) -> Self {
        self.record = limits;
        self
    }

    /// Selects the archive-byte policy used during JSON reconstruction.
    #[must_use]
    pub const fn with_byte_policy(mut self, byte_policy: JsonBytePolicy) -> Self {
        self.byte_policy = byte_policy;
        self
    }

    /// Returns the archive-independent result projection.
    #[must_use]
    pub const fn projection(&self) -> &Projection {
        &self.projection
    }

    /// Returns extraction-plan compilation limits.
    #[must_use]
    pub const fn plan_limits(&self) -> ExtractionPlanLimits {
        self.plan
    }

    /// Returns record-program and per-record limits.
    #[must_use]
    pub const fn record_limits(&self) -> RecordLimits {
        self.record
    }

    /// Returns the archive-byte output policy.
    #[must_use]
    pub const fn byte_policy(&self) -> JsonBytePolicy {
        self.byte_policy
    }

    /// Returns the maximum documents in one sink call.
    #[must_use]
    pub const fn batch_size(&self) -> usize {
        self.batch_size
    }

    /// Returns the maximum matching records retained for each archive.
    #[must_use]
    pub const fn max_num_results(&self) -> usize {
        self.max_num_results
    }
}

impl Default for SearchResultsCacheOptions {
    fn default() -> Self {
        Self::new(
            Projection::all(),
            DEFAULT_BATCH_SIZE,
            DEFAULT_MAX_NUM_RESULTS,
        )
        .expect("nonzero built-in results-cache limits")
    }
}

/// Invalid results-cache batching or retention configuration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ResultsCacheOptionsError {
    /// A zero-sized batch cannot make progress.
    ZeroBatchSize,
    /// A zero result limit is rejected by the C++ command-line contract.
    ZeroMaxNumResults,
}

impl Display for ResultsCacheOptionsError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::ZeroBatchSize => "results-cache batch size cannot be zero",
            Self::ZeroMaxNumResults => "results-cache maximum result count cannot be zero",
        })
    }
}

impl Error for ResultsCacheOptionsError {}

/// One retained search result, excluding archive and dataset strings shared by the adapter.
///
/// Sharing those two strings until a batch is materialized avoids the C++ implementation's two
/// per-result string allocations while preserving its MongoDB document schema.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResultsCacheSearchResult {
    message: String,
    timestamp: i64,
    log_event_index: i64,
}

impl ResultsCacheSearchResult {
    /// Creates one result from reconstructed JSON, authoritative epoch milliseconds, and the
    /// archive-global log-event index. The JSON message includes the C++ trailing newline.
    #[must_use]
    pub const fn new(message: String, timestamp: i64, log_event_index: i64) -> Self {
        Self {
            message,
            timestamp,
            log_event_index,
        }
    }

    /// Returns the reconstructed JSON record, including its trailing newline.
    #[must_use]
    pub const fn message(&self) -> &str {
        self.message.as_str()
    }

    /// Returns the authoritative timestamp in epoch milliseconds, or zero when absent.
    #[must_use]
    pub const fn timestamp(&self) -> i64 {
        self.timestamp
    }

    /// Returns the archive-global log-event index, or zero when absent.
    #[must_use]
    pub const fn log_event_index(&self) -> i64 {
        self.log_event_index
    }

    /// Consumes the result without copying its reconstructed JSON allocation.
    #[must_use]
    pub fn into_parts(self) -> (String, i64, i64) {
        (self.message, self.timestamp, self.log_event_index)
    }
}

/// Synchronous destination for ordered batches of ordinary results-cache search documents.
pub trait SearchResultsCacheBatchSink {
    /// Initializes per-archive output after search preflight reaches the output stage.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when the destination cannot initialize the archive.
    fn begin_archive(&mut self) -> io::Result<()> {
        Ok(())
    }

    /// Accepts one nonempty batch in ascending timestamp order.
    ///
    /// `archive_id` and `dataset` apply to every result in `results`. The batch is transferred by
    /// value so a service adapter can move message allocations directly into its document type.
    /// The C++ `orig_file_path` field is always the empty string and is therefore left for the
    /// service adapter to add.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when the destination rejects the batch.
    fn insert_search_batch(
        &mut self,
        archive_id: &str,
        dataset: &str,
        results: Vec<ResultsCacheSearchResult>,
    ) -> io::Result<()>;
}

/// Synchronous destination for ordered batches of typed results-cache aggregation documents.
pub trait AggregationResultsCacheBatchSink {
    /// Initializes per-archive output after search preflight reaches the output stage.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when the destination cannot initialize the archive.
    fn begin_archive(&mut self) -> io::Result<()> {
        Ok(())
    }

    /// Accepts one nonempty batch in the aggregation's C++ result order.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when the destination rejects the batch.
    fn insert_aggregation_batch(
        &mut self,
        archive_id: &str,
        results: &[AggregationResultRef<'_>],
    ) -> io::Result<()>;
}

/// Retains and batches the latest matching records for one archive.
///
/// The adapter keeps at most `max_num_results` messages. Archive ID and dataset remain shared, and
/// records older than the retained minimum skip JSON reconstruction entirely. Call [`Self::finish`]
/// only after [`super::search_archive`] succeeds.
pub struct SearchResultsCacheAdapter<'sink, 'text, 'options, S: ?Sized> {
    sink: &'sink mut S,
    archive_id: &'text str,
    dataset: &'text str,
    options: &'options SearchResultsCacheOptions,
    projection: Option<ResolvedProjection>,
    programs: HashMap<i32, RecordProgram>,
    log_event_index: LogEventIndex,
    scratch: RecordScratch,
    record: Vec<u8>,
    latest: BinaryHeap<RetainedResult>,
    next_sequence: u64,
    records_written: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LogEventIndex {
    Unresolved,
    Absent,
    Node(u32),
}

#[derive(Debug)]
struct RetainedResult {
    result: ResultsCacheSearchResult,
    sequence: u64,
}

struct RetentionOutput<'a> {
    record: &'a mut Vec<u8>,
    latest: &'a mut BinaryHeap<RetainedResult>,
    next_sequence: &'a mut u64,
    max_num_results: usize,
}

impl PartialEq for RetainedResult {
    fn eq(&self, other: &Self) -> bool {
        self.result.timestamp == other.result.timestamp && self.sequence == other.sequence
    }
}

impl Eq for RetainedResult {}

impl PartialOrd for RetainedResult {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for RetainedResult {
    fn cmp(&self, other: &Self) -> Ordering {
        // Reverse both keys so the heap exposes the oldest result and emits equal timestamps in
        // stable physical match order. C++ only promises the timestamp comparison itself.
        other
            .result
            .timestamp
            .cmp(&self.result.timestamp)
            .then_with(|| other.sequence.cmp(&self.sequence))
    }
}

impl<'sink, 'text, 'options, S: SearchResultsCacheBatchSink + ?Sized>
    SearchResultsCacheAdapter<'sink, 'text, 'options, S>
{
    /// Creates a per-archive adapter borrowing its destination and shared document strings.
    #[must_use]
    pub fn new(
        sink: &'sink mut S,
        archive_id: &'text str,
        dataset: &'text str,
        options: &'options SearchResultsCacheOptions,
    ) -> Self {
        Self {
            sink,
            archive_id,
            dataset,
            options,
            projection: None,
            programs: HashMap::new(),
            log_event_index: LogEventIndex::Unresolved,
            scratch: RecordScratch::new(),
            record: Vec::new(),
            latest: BinaryHeap::new(),
            next_sequence: 0,
            records_written: 0,
        }
    }

    /// Returns the number of documents accepted by the destination.
    #[must_use]
    pub const fn records_written(&self) -> u64 {
        self.records_written
    }

    /// Drains retained results in ascending timestamp order and writes C++-sized batches.
    ///
    /// Zero retained matches make no insert call. Calling this more than once is harmless.
    ///
    /// # Errors
    ///
    /// Returns an allocation, checked-arithmetic, or destination error.
    pub fn finish(&mut self) -> Result<(), SearchResultsCacheAdapterError> {
        let mut batch = Vec::new();
        let capacity = self.latest.len().min(self.options.batch_size());
        batch.try_reserve(capacity).map_err(|_| {
            SearchResultsCacheAdapterError::AllocationFailed {
                resource: ResultsCacheResource::Batch,
                requested: capacity,
            }
        })?;
        while let Some(retained) = self.latest.pop() {
            batch.push(retained.result);
            if batch.len() == self.options.batch_size() {
                self.flush_batch(&mut batch)?;
            }
        }
        self.flush_batch(&mut batch)
    }

    fn flush_batch(
        &mut self,
        batch: &mut Vec<ResultsCacheSearchResult>,
    ) -> Result<(), SearchResultsCacheAdapterError> {
        if batch.is_empty() {
            return Ok(());
        }
        let attempted =
            u64::try_from(batch.len()).map_err(|_| SearchResultsCacheAdapterError::SizeOverflow)?;
        let next_capacity = self.latest.len().min(self.options.batch_size());
        let mut next_batch = Vec::new();
        next_batch.try_reserve(next_capacity).map_err(|_| {
            SearchResultsCacheAdapterError::AllocationFailed {
                resource: ResultsCacheResource::Batch,
                requested: next_capacity,
            }
        })?;
        let outbound = std::mem::replace(batch, next_batch);
        self.sink
            .insert_search_batch(self.archive_id, self.dataset, outbound)
            .map_err(|source| SearchResultsCacheAdapterError::Output {
                completed_records: self.records_written,
                attempted_records: attempted,
                source,
            })?;
        self.records_written = self
            .records_written
            .checked_add(attempted)
            .ok_or(SearchResultsCacheAdapterError::SizeOverflow)?;
        batch.clear();
        Ok(())
    }

    fn prepare_projection(
        &mut self,
        matches: ArchiveTableMatches<'_, '_, '_>,
    ) -> Result<(), SearchResultsCacheAdapterError> {
        if self.projection.is_none() {
            self.projection = Some(
                self.options
                    .projection()
                    .resolve(matches.catalog().schema_tree())
                    .map_err(SearchResultsCacheAdapterError::Projection)?,
            );
        }
        Ok(())
    }

    fn prepare_log_event_index(
        &mut self,
        matches: ArchiveTableMatches<'_, '_, '_>,
    ) -> Result<(), SearchResultsCacheAdapterError> {
        if LogEventIndex::Unresolved != self.log_event_index {
            return Ok(());
        }
        self.log_event_index = LogOrderLocator::discover(matches.catalog().schema_tree())
            .map_err(SearchResultsCacheAdapterError::LogOrderDiscovery)?
            .map_or(LogEventIndex::Absent, |locator| {
                LogEventIndex::Node(locator.node_id())
            });
        Ok(())
    }

    fn compile_program(
        &self,
        matches: ArchiveTableMatches<'_, '_, '_>,
    ) -> Result<RecordProgram, SearchResultsCacheAdapterError> {
        let schema_id = matches.schema_id();
        let plan = ExtractionPlan::compile(
            matches.table().schema(),
            matches.catalog().schema_tree(),
            self.options.plan_limits(),
        )
        .map_err(|source| SearchResultsCacheAdapterError::Plan { schema_id, source })?;
        let plan = match self
            .projection
            .as_ref()
            .and_then(ResolvedProjection::selected_node_ids)
        {
            Some(node_ids) => plan
                .project_selected_nodes(node_ids)
                .map_err(|source| SearchResultsCacheAdapterError::Plan { schema_id, source })?,
            None => plan,
        };
        RecordProgram::compile_with_byte_policy(
            &plan,
            matches.catalog().schema_tree(),
            self.options.byte_policy(),
            self.options.record_limits(),
        )
        .map_err(|source| SearchResultsCacheAdapterError::Program { schema_id, source })
    }

    fn prepare_program(
        &mut self,
        matches: ArchiveTableMatches<'_, '_, '_>,
    ) -> Result<(), SearchResultsCacheAdapterError> {
        let schema_id = matches.schema_id();
        if self.programs.contains_key(&schema_id) {
            return Ok(());
        }
        let program = self.compile_program(matches)?;
        self.programs.try_reserve(1).map_err(|_| {
            SearchResultsCacheAdapterError::AllocationFailed {
                resource: ResultsCacheResource::Programs,
                requested: self.programs.len().saturating_add(1),
            }
        })?;
        self.programs.insert(schema_id, program);
        Ok(())
    }

    fn write_results_cache(
        &mut self,
        matches: ArchiveTableMatches<'_, '_, '_>,
    ) -> Result<(), SearchResultsCacheAdapterError> {
        self.prepare_projection(matches)?;
        self.prepare_log_event_index(matches)?;
        self.prepare_program(matches)?;
        let schema_id = matches.schema_id();
        let program = self
            .programs
            .get(&schema_id)
            .ok_or(SearchResultsCacheAdapterError::SizeOverflow)?;
        let scratch = std::mem::take(&mut self.scratch);
        let mut writer = program
            .writer_with_scratch(
                matches.table().table(),
                matches.catalog().timestamp_patterns(),
                scratch,
            )
            .map_err(|source| SearchResultsCacheAdapterError::Bind { schema_id, source })?;
        let log_event_indexes = locate_log_event_indexes(matches, self.log_event_index)?;
        let timestamp_column = authoritative_timestamp_column(matches)?;
        let mut output = RetentionOutput {
            record: &mut self.record,
            latest: &mut self.latest,
            next_sequence: &mut self.next_sequence,
            max_num_results: self.options.max_num_results(),
        };
        let result = match timestamp_column.map(Column::data) {
            None => Self::drain_rows(
                &mut output,
                matches,
                &mut writer,
                (0..matches.bitmap().len()).map(|_| 0_i64),
                log_event_indexes,
            ),
            Some(ColumnData::Timestamp(values)) => Self::drain_rows(
                &mut output,
                matches,
                &mut writer,
                values
                    .epochs()
                    .values()
                    .map(|epoch| epoch / NANOSECONDS_PER_MILLISECOND),
                log_event_indexes,
            ),
            Some(ColumnData::DeprecatedDateString(values)) => Self::drain_rows(
                &mut output,
                matches,
                &mut writer,
                values.epochs().iter(),
                log_event_indexes,
            ),
            Some(ColumnData::Integer(values)) => Self::drain_rows(
                &mut output,
                matches,
                &mut writer,
                values.iter(),
                log_event_indexes,
            ),
            Some(ColumnData::DeltaInteger(values)) => Self::drain_rows(
                &mut output,
                matches,
                &mut writer,
                values.values(),
                log_event_indexes,
            ),
            Some(ColumnData::Float(values)) => Self::drain_rows(
                &mut output,
                matches,
                &mut writer,
                values.iter().map(timestamp_seconds_to_milliseconds),
                log_event_indexes,
            ),
            Some(column) => Err(SearchResultsCacheAdapterError::InvalidTimestampColumn {
                node_id: timestamp_column
                    .map(Column::node_id)
                    .ok_or(SearchResultsCacheAdapterError::SizeOverflow)?,
                node_type: column.node_type(),
            }),
        };
        self.scratch = writer.into_scratch();
        result
    }

    #[allow(clippy::too_many_lines)]
    fn drain_rows<I>(
        output: &mut RetentionOutput<'_>,
        matches: ArchiveTableMatches<'_, '_, '_>,
        writer: &mut RecordWriter<'_, '_, '_, '_>,
        mut timestamps: I,
        mut log_event_indexes: Option<DeltaI64Values<'_>>,
    ) -> Result<(), SearchResultsCacheAdapterError>
    where
        I: ExactSizeIterator<Item = i64>, {
        let schema_id = matches.schema_id();
        let row_count = matches.bitmap().len();
        if timestamps.len() != row_count {
            return Err(SearchResultsCacheAdapterError::TimestampRowCountMismatch {
                schema_id,
                expected: row_count,
                actual: timestamps.len(),
            });
        }
        if let Some(values) = &log_event_indexes
            && values.len() != row_count
        {
            return Err(
                SearchResultsCacheAdapterError::LogEventIndexRowCountMismatch {
                    schema_id,
                    expected: row_count,
                    actual: values.len(),
                },
            );
        }

        let mut pending_skips = 0_usize;
        for (row_index, matched) in matches.bitmap().as_bytes().iter().copied().enumerate() {
            if 0 == matched {
                pending_skips = pending_skips
                    .checked_add(1)
                    .ok_or(SearchResultsCacheAdapterError::SizeOverflow)?;
                continue;
            }

            let skip_start = row_index
                .checked_sub(pending_skips)
                .ok_or(SearchResultsCacheAdapterError::SizeOverflow)?;
            skip_auxiliary_rows(
                &mut timestamps,
                &mut log_event_indexes,
                schema_id,
                skip_start,
                pending_skips,
            )?;
            skip_record_rows(writer, schema_id, skip_start, pending_skips, row_count)?;
            pending_skips = 0;

            let timestamp =
                timestamps
                    .next()
                    .ok_or(SearchResultsCacheAdapterError::MissingTimestampValue {
                        schema_id,
                        row_index,
                    })?;
            let log_event_index = match &mut log_event_indexes {
                Some(values) => values.next().ok_or(
                    SearchResultsCacheAdapterError::MissingLogEventIndexValue {
                        schema_id,
                        row_index,
                    },
                )?,
                None => 0,
            };
            let sequence = *output.next_sequence;
            *output.next_sequence = output
                .next_sequence
                .checked_add(1)
                .ok_or(SearchResultsCacheAdapterError::SizeOverflow)?;
            let retain = should_retain(output.latest, output.max_num_results, timestamp);
            if !retain {
                skip_record_rows(writer, schema_id, row_index, 1, row_count)?;
                continue;
            }

            output.record.clear();
            if !writer.append_next_record(output.record).map_err(|source| {
                SearchResultsCacheAdapterError::Record {
                    schema_id,
                    row_index,
                    source,
                }
            })? {
                return Err(SearchResultsCacheAdapterError::RecordRowCountMismatch {
                    schema_id,
                    expected: row_count,
                    actual: row_index,
                });
            }
            if output.record.last() != Some(&b'\n') {
                output.record.try_reserve(1).map_err(|_| {
                    SearchResultsCacheAdapterError::AllocationFailed {
                        resource: ResultsCacheResource::Record,
                        requested: output.record.len().saturating_add(1),
                    }
                })?;
                output.record.push(b'\n');
            }
            let message_bytes = std::mem::take(output.record);
            let message = String::from_utf8(message_bytes).map_err(|source| {
                SearchResultsCacheAdapterError::InvalidMessageUtf8 {
                    schema_id,
                    row_index,
                    source,
                }
            })?;
            if let Some(replaced_message) = retain_latest(
                output.latest,
                output.max_num_results,
                ResultsCacheSearchResult::new(message, timestamp, log_event_index),
                sequence,
            )? {
                *output.record = replaced_message.into_bytes();
                output.record.clear();
            }
        }

        let skip_start = row_count
            .checked_sub(pending_skips)
            .ok_or(SearchResultsCacheAdapterError::SizeOverflow)?;
        skip_auxiliary_rows(
            &mut timestamps,
            &mut log_event_indexes,
            schema_id,
            skip_start,
            pending_skips,
        )?;
        skip_record_rows(writer, schema_id, skip_start, pending_skips, row_count)?;
        if 0 != writer.remaining() {
            return Err(SearchResultsCacheAdapterError::RecordRowCountMismatch {
                schema_id,
                expected: row_count,
                actual: row_count - writer.remaining(),
            });
        }
        Ok(())
    }
}

fn should_retain(latest: &BinaryHeap<RetainedResult>, limit: usize, timestamp: i64) -> bool {
    latest.len() < limit
        || latest
            .peek()
            .is_some_and(|oldest| oldest.result.timestamp < timestamp)
}

fn retain_latest(
    latest: &mut BinaryHeap<RetainedResult>,
    limit: usize,
    result: ResultsCacheSearchResult,
    sequence: u64,
) -> Result<Option<String>, SearchResultsCacheAdapterError> {
    if latest.len() < limit {
        latest
            .try_reserve(1)
            .map_err(|_| SearchResultsCacheAdapterError::AllocationFailed {
                resource: ResultsCacheResource::LatestResults,
                requested: latest.len().saturating_add(1),
            })?;
    }
    let replacement = if latest.len() == limit {
        latest.pop()
    } else {
        None
    };
    latest.push(RetainedResult { result, sequence });
    Ok(replacement.map(|replaced| replaced.result.message))
}

impl<S: SearchResultsCacheBatchSink + ?Sized> ArchiveMatchSink
    for SearchResultsCacheAdapter<'_, '_, '_, S>
{
    fn begin_archive(&mut self) -> io::Result<()> {
        self.sink.begin_archive()
    }

    fn write_matches(&mut self, matches: ArchiveTableMatches<'_, '_, '_>) -> io::Result<()> {
        self.write_results_cache(matches).map_err(io::Error::other)
    }
}

/// Batches one archive's typed aggregation results for a results-cache destination.
pub struct AggregationResultsCacheAdapter<'sink, 'text, 'plan, S: ?Sized> {
    sink: &'sink mut S,
    archive_id: &'text str,
    aggregation: AggregationSink<'plan>,
    batch_size: usize,
    records_written: u64,
}

impl<'sink, 'text, 'plan, S: AggregationResultsCacheBatchSink + ?Sized>
    AggregationResultsCacheAdapter<'sink, 'text, 'plan, S>
{
    /// Creates a per-archive aggregation adapter.
    ///
    /// # Errors
    ///
    /// Returns an error when `batch_size` is zero.
    pub fn new(
        sink: &'sink mut S,
        archive_id: &'text str,
        plan: &'plan AggregationPlan,
        batch_size: usize,
    ) -> Result<Self, ResultsCacheOptionsError> {
        if 0 == batch_size {
            return Err(ResultsCacheOptionsError::ZeroBatchSize);
        }
        Ok(Self {
            sink,
            archive_id,
            aggregation: plan.start(),
            batch_size,
            records_written: 0,
        })
    }

    /// Returns the number of aggregation documents accepted by the destination.
    #[must_use]
    pub const fn records_written(&self) -> u64 {
        self.records_written
    }

    /// Batches completed results in the aggregation's C++ order.
    ///
    /// Zero aggregation results make no insert call. Calling this more than once repeats the
    /// completed aggregation and is therefore unsupported.
    ///
    /// # Errors
    ///
    /// Returns an allocation, checked-arithmetic, or destination error.
    pub fn finish(&mut self) -> Result<(), AggregationResultsCacheAdapterError> {
        let Self {
            sink,
            archive_id,
            aggregation,
            batch_size,
            records_written,
        } = self;
        let results = aggregation.results();
        let capacity = results.len().min(*batch_size);
        let mut batch = Vec::new();
        batch.try_reserve(capacity).map_err(|_| {
            AggregationResultsCacheAdapterError::AllocationFailed {
                requested: capacity,
            }
        })?;
        for result in results {
            batch.push(result);
            if batch.len() == *batch_size {
                flush_aggregation_batch(*sink, archive_id, &mut batch, records_written)?;
            }
        }
        flush_aggregation_batch(*sink, archive_id, &mut batch, records_written)
    }
}

impl<S: AggregationResultsCacheBatchSink + ?Sized> ArchiveMatchSink
    for AggregationResultsCacheAdapter<'_, '_, '_, S>
{
    fn begin_archive(&mut self) -> io::Result<()> {
        self.sink.begin_archive()
    }

    fn write_matches(&mut self, matches: ArchiveTableMatches<'_, '_, '_>) -> io::Result<()> {
        self.aggregation.write_matches(matches)
    }
}

fn flush_aggregation_batch<S: AggregationResultsCacheBatchSink + ?Sized>(
    sink: &mut S,
    archive_id: &str,
    batch: &mut Vec<AggregationResultRef<'_>>,
    records_written: &mut u64,
) -> Result<(), AggregationResultsCacheAdapterError> {
    if batch.is_empty() {
        return Ok(());
    }
    let attempted = u64::try_from(batch.len())
        .map_err(|_| AggregationResultsCacheAdapterError::SizeOverflow)?;
    sink.insert_aggregation_batch(archive_id, batch)
        .map_err(|source| AggregationResultsCacheAdapterError::Output {
            completed_records: *records_written,
            attempted_records: attempted,
            source,
        })?;
    *records_written = records_written
        .checked_add(attempted)
        .ok_or(AggregationResultsCacheAdapterError::SizeOverflow)?;
    batch.clear();
    Ok(())
}

fn skip_record_rows(
    writer: &mut RecordWriter<'_, '_, '_, '_>,
    schema_id: i32,
    row_index: usize,
    count: usize,
    expected: usize,
) -> Result<(), SearchResultsCacheAdapterError> {
    if !writer
        .skip_records(count)
        .map_err(|source| SearchResultsCacheAdapterError::Record {
            schema_id,
            row_index,
            source,
        })?
    {
        return Err(SearchResultsCacheAdapterError::RecordRowCountMismatch {
            schema_id,
            expected,
            actual: writer.next_row_index(),
        });
    }
    Ok(())
}

#[allow(clippy::cast_possible_truncation)]
fn timestamp_seconds_to_milliseconds(value: f64) -> i64 {
    (value * MILLISECONDS_PER_SECOND) as i64
}

fn authoritative_timestamp_column<'stream, 'archive>(
    matches: ArchiveTableMatches<'_, 'stream, 'archive>,
) -> Result<Option<Column<'stream, 'archive>>, SearchResultsCacheAdapterError> {
    let Some(authoritative) = matches
        .catalog()
        .metadata()
        .timestamp_dictionary()
        .authoritative_range()
    else {
        return Ok(None);
    };
    let ordered_entries = matches.table().schema().ordered_entry_count();
    let mut selected = None;
    for column in matches.table().table().columns() {
        if column.schema_entry_index() >= ordered_entries {
            continue;
        }
        let node_id = i32::try_from(column.node_id())
            .map_err(|_| SearchResultsCacheAdapterError::SizeOverflow)?;
        if authoritative.column_ids().contains(&node_id) {
            selected = Some(*column);
        }
    }
    Ok(selected)
}

fn locate_log_event_indexes<'stream>(
    matches: ArchiveTableMatches<'_, 'stream, '_>,
    locator: LogEventIndex,
) -> Result<Option<DeltaI64Values<'stream>>, SearchResultsCacheAdapterError> {
    let LogEventIndex::Node(node_id) = locator else {
        return Ok(None);
    };
    let ordered_entries = matches.table().schema().ordered_entry_count();
    let mut selected = None;
    for column in matches.table().table().columns() {
        if column.schema_entry_index() >= ordered_entries || column.node_id() != node_id {
            continue;
        }
        let ColumnData::DeltaInteger(values) = column.data() else {
            return Err(SearchResultsCacheAdapterError::InvalidLogEventIndexColumn {
                node_id,
                node_type: column.node_type(),
            });
        };
        selected = Some(values.values());
    }
    Ok(selected)
}

fn skip_auxiliary_rows<I>(
    timestamps: &mut I,
    log_event_indexes: &mut Option<DeltaI64Values<'_>>,
    schema_id: i32,
    row_index: usize,
    count: usize,
) -> Result<(), SearchResultsCacheAdapterError>
where
    I: ExactSizeIterator<Item = i64>, {
    if 0 == count {
        return Ok(());
    }
    if timestamps.len() < count {
        return Err(SearchResultsCacheAdapterError::MissingTimestampValue {
            schema_id,
            row_index: row_index
                .checked_add(timestamps.len())
                .ok_or(SearchResultsCacheAdapterError::SizeOverflow)?,
        });
    }
    let _ = timestamps.nth(count - 1);
    if let Some(values) = log_event_indexes {
        if values.len() < count {
            return Err(SearchResultsCacheAdapterError::MissingLogEventIndexValue {
                schema_id,
                row_index: row_index
                    .checked_add(values.len())
                    .ok_or(SearchResultsCacheAdapterError::SizeOverflow)?,
            });
        }
        let _ = values.nth(count - 1);
    }
    Ok(())
}

/// Retained allocation named by a results-cache adapter error.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ResultsCacheResource {
    /// Per-schema record programs.
    Programs,
    /// Top-N result heap.
    LatestResults,
    /// Reconstructed JSON record.
    Record,
    /// Outbound result batch.
    Batch,
}

impl Display for ResultsCacheResource {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Programs => "record programs",
            Self::LatestResults => "latest-result heap",
            Self::Record => "JSON record",
            Self::Batch => "outbound batch",
        })
    }
}

/// Failure while reconstructing, retaining, or batching ordinary results-cache documents.
#[derive(Debug)]
#[non_exhaustive]
pub enum SearchResultsCacheAdapterError {
    /// Resolving selected descriptors against the archive failed.
    Projection(ProjectionError),
    /// Compiling or pruning one schema extraction plan failed.
    Plan {
        /// Opaque schema ID.
        schema_id: i32,
        /// Plan failure.
        source: ExtractionPlanError,
    },
    /// Compiling a reusable record program failed.
    Program {
        /// Opaque schema ID.
        schema_id: i32,
        /// Program failure.
        source: RecordCompileError,
    },
    /// Binding a compiled program to one decoded table failed.
    Bind {
        /// Opaque schema ID.
        schema_id: i32,
        /// Bind failure.
        source: RecordBindError,
    },
    /// Discovering the canonical log-event index failed.
    LogOrderDiscovery(LogOrderError),
    /// Formatting or advancing one physical row failed.
    Record {
        /// Opaque schema ID.
        schema_id: i32,
        /// Physical table-local row index.
        row_index: usize,
        /// Record failure.
        source: RecordError,
    },
    /// A reconstructed BSON string was not valid UTF-8.
    InvalidMessageUtf8 {
        /// Opaque schema ID.
        schema_id: i32,
        /// Physical table-local row index.
        row_index: usize,
        /// UTF-8 validation failure retaining the original allocation.
        source: std::string::FromUtf8Error,
    },
    /// An authoritative timestamp column had a C++-unsupported type.
    InvalidTimestampColumn {
        /// Schema-tree node ID.
        node_id: u32,
        /// Decoded node type.
        node_type: crate::archive::NodeType,
    },
    /// The reserved log-event index column was not delta encoded.
    InvalidLogEventIndexColumn {
        /// Schema-tree node ID.
        node_id: u32,
        /// Decoded node type.
        node_type: crate::archive::NodeType,
    },
    /// Timestamp values disagreed with the table row count.
    TimestampRowCountMismatch {
        /// Opaque schema ID.
        schema_id: i32,
        /// Expected values.
        expected: usize,
        /// Actual values.
        actual: usize,
    },
    /// Log-event indexes disagreed with the table row count.
    LogEventIndexRowCountMismatch {
        /// Opaque schema ID.
        schema_id: i32,
        /// Expected values.
        expected: usize,
        /// Actual values.
        actual: usize,
    },
    /// The record writer disagreed with the table row count.
    RecordRowCountMismatch {
        /// Opaque schema ID.
        schema_id: i32,
        /// Expected rows.
        expected: usize,
        /// Actual rows.
        actual: usize,
    },
    /// A timestamp iterator ended at a validated table row.
    MissingTimestampValue {
        /// Opaque schema ID.
        schema_id: i32,
        /// Physical table-local row index.
        row_index: usize,
    },
    /// A log-event-index iterator ended at a validated table row.
    MissingLogEventIndexValue {
        /// Opaque schema ID.
        schema_id: i32,
        /// Physical table-local row index.
        row_index: usize,
    },
    /// A bounded allocation failed.
    AllocationFailed {
        /// Retained resource.
        resource: ResultsCacheResource,
        /// Requested elements or bytes.
        requested: usize,
    },
    /// Checked size or count arithmetic overflowed.
    SizeOverflow,
    /// The destination rejected one complete batch.
    Output {
        /// Complete documents accepted before this batch.
        completed_records: u64,
        /// Documents in the rejected batch.
        attempted_records: u64,
        /// Destination failure.
        source: io::Error,
    },
}

impl Display for SearchResultsCacheAdapterError {
    #[allow(clippy::too_many_lines)]
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Projection(source) => write!(formatter, "failed to resolve projection: {source}"),
            Self::Plan { schema_id, source } => {
                write!(
                    formatter,
                    "failed to compile schema {schema_id} plan: {source}"
                )
            }
            Self::Program { schema_id, source } => {
                write!(
                    formatter,
                    "failed to compile schema {schema_id} record: {source}"
                )
            }
            Self::Bind { schema_id, source } => {
                write!(
                    formatter,
                    "failed to bind schema {schema_id} record: {source}"
                )
            }
            Self::LogOrderDiscovery(source) => {
                write!(formatter, "failed to discover log-event index: {source}")
            }
            Self::Record {
                schema_id,
                row_index,
                source,
            } => write!(
                formatter,
                "failed to format schema {schema_id}, row {row_index}: {source}"
            ),
            Self::InvalidMessageUtf8 {
                schema_id,
                row_index,
                source,
            } => write!(
                formatter,
                "schema {schema_id}, row {row_index} is not valid BSON UTF-8: {source}"
            ),
            Self::InvalidTimestampColumn { node_id, node_type } => write!(
                formatter,
                "authoritative timestamp node {node_id} has type {node_type:?}"
            ),
            Self::InvalidLogEventIndexColumn { node_id, node_type } => write!(
                formatter,
                "log-event index node {node_id} has type {node_type:?}"
            ),
            Self::TimestampRowCountMismatch {
                schema_id,
                expected,
                actual,
            } => write!(
                formatter,
                "schema {schema_id} has {actual} timestamps for {expected} rows"
            ),
            Self::LogEventIndexRowCountMismatch {
                schema_id,
                expected,
                actual,
            } => write!(
                formatter,
                "schema {schema_id} has {actual} log-event indexes for {expected} rows"
            ),
            Self::RecordRowCountMismatch {
                schema_id,
                expected,
                actual,
            } => write!(
                formatter,
                "schema {schema_id} record writer consumed {actual} of {expected} rows"
            ),
            Self::MissingTimestampValue {
                schema_id,
                row_index,
            } => write!(
                formatter,
                "schema {schema_id}, row {row_index} has no timestamp value"
            ),
            Self::MissingLogEventIndexValue {
                schema_id,
                row_index,
            } => write!(
                formatter,
                "schema {schema_id}, row {row_index} has no log-event index"
            ),
            Self::AllocationFailed {
                resource,
                requested,
            } => write!(
                formatter,
                "failed to allocate {requested} element(s) for results-cache {resource}"
            ),
            Self::SizeOverflow => formatter.write_str("results-cache result size overflow"),
            Self::Output {
                completed_records,
                attempted_records,
                source,
            } => write!(
                formatter,
                "results cache rejected a {attempted_records}-document batch after \
                 {completed_records} complete document(s): {source}"
            ),
        }
    }
}

impl Error for SearchResultsCacheAdapterError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Projection(source) => Some(source),
            Self::Plan { source, .. } => Some(source),
            Self::Program { source, .. } => Some(source),
            Self::Bind { source, .. } => Some(source),
            Self::LogOrderDiscovery(source) => Some(source),
            Self::Record { source, .. } => Some(source),
            Self::InvalidMessageUtf8 { source, .. } => Some(source),
            Self::Output { source, .. } => Some(source),
            Self::InvalidTimestampColumn { .. }
            | Self::InvalidLogEventIndexColumn { .. }
            | Self::TimestampRowCountMismatch { .. }
            | Self::LogEventIndexRowCountMismatch { .. }
            | Self::RecordRowCountMismatch { .. }
            | Self::MissingTimestampValue { .. }
            | Self::MissingLogEventIndexValue { .. }
            | Self::AllocationFailed { .. }
            | Self::SizeOverflow => None,
        }
    }
}

/// Failure while batching completed aggregation documents.
#[derive(Debug)]
#[non_exhaustive]
pub enum AggregationResultsCacheAdapterError {
    /// Allocating the outbound result batch failed.
    AllocationFailed {
        /// Requested result slots.
        requested: usize,
    },
    /// Checked size or count arithmetic overflowed.
    SizeOverflow,
    /// The destination rejected one complete batch.
    Output {
        /// Complete documents accepted before this batch.
        completed_records: u64,
        /// Documents in the rejected batch.
        attempted_records: u64,
        /// Destination failure.
        source: io::Error,
    },
}

impl Display for AggregationResultsCacheAdapterError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::AllocationFailed { requested } => write!(
                formatter,
                "failed to allocate {requested} result(s) for a results-cache aggregation batch"
            ),
            Self::SizeOverflow => {
                formatter.write_str("results-cache aggregation result size overflow")
            }
            Self::Output {
                completed_records,
                attempted_records,
                source,
            } => write!(
                formatter,
                "results cache rejected a {attempted_records}-document aggregation batch after \
                 {completed_records} complete document(s): {source}"
            ),
        }
    }
}

impl Error for AggregationResultsCacheAdapterError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Output { source, .. } => Some(source),
            Self::AllocationFailed { .. } | Self::SizeOverflow => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::archive::SingleFileArchiveReader;
    use crate::search::AggregationNumber;
    use crate::search::AggregationValueRef;
    use crate::search::ArchiveSearchOptions;
    use crate::search::KqlLimits;
    use crate::search::parse_kql;
    use crate::search::search_archive;

    const AGGREGATIONS_ARCHIVE: &[u8] =
        include_bytes!("../../tests/fixtures/sfa-v0.5.0-aggregations-cpp.bin");
    const MINIMAL_ARCHIVE: &[u8] =
        include_bytes!("../../tests/fixtures/sfa-v0.5.0-minimal-cpp.bin");

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct CapturedSearchBatch {
        archive_id: String,
        dataset: String,
        results: Vec<ResultsCacheSearchResult>,
    }

    #[derive(Debug, Default)]
    struct SearchCapture {
        begins: usize,
        batches: Vec<CapturedSearchBatch>,
        fail_after_batches: Option<usize>,
    }

    impl SearchResultsCacheBatchSink for SearchCapture {
        fn begin_archive(&mut self) -> io::Result<()> {
            self.begins += 1;
            Ok(())
        }

        fn insert_search_batch(
            &mut self,
            archive_id: &str,
            dataset: &str,
            results: Vec<ResultsCacheSearchResult>,
        ) -> io::Result<()> {
            if self
                .fail_after_batches
                .is_some_and(|limit| limit == self.batches.len())
            {
                return Err(io::Error::other("injected results-cache failure"));
            }
            self.batches.push(CapturedSearchBatch {
                archive_id: archive_id.to_owned(),
                dataset: dataset.to_owned(),
                results,
            });
            Ok(())
        }
    }

    #[derive(Clone, Debug, PartialEq)]
    enum CapturedAggregation {
        Count(i64),
        CountByTime(i64, i64),
        Minimum(String, AggregationNumber),
        Maximum(String, AggregationNumber),
        Unique(String, CapturedValue),
    }

    #[derive(Clone, Debug, PartialEq)]
    enum CapturedValue {
        Integer(i64),
        Float(f64),
        String(String),
        Boolean(bool),
    }

    impl From<AggregationValueRef<'_>> for CapturedValue {
        fn from(value: AggregationValueRef<'_>) -> Self {
            match value {
                AggregationValueRef::Integer(value) => Self::Integer(value),
                AggregationValueRef::Float(value) => Self::Float(value),
                AggregationValueRef::String(value) => Self::String(value.to_owned()),
                AggregationValueRef::Boolean(value) => Self::Boolean(value),
            }
        }
    }

    impl From<AggregationResultRef<'_>> for CapturedAggregation {
        fn from(result: AggregationResultRef<'_>) -> Self {
            match result {
                AggregationResultRef::Count { count } => Self::Count(count),
                AggregationResultRef::CountByTime { timestamp, count } => {
                    Self::CountByTime(timestamp, count)
                }
                AggregationResultRef::Minimum { field, value } => {
                    Self::Minimum(field.to_owned(), value)
                }
                AggregationResultRef::Maximum { field, value } => {
                    Self::Maximum(field.to_owned(), value)
                }
                AggregationResultRef::Unique { field, value } => {
                    Self::Unique(field.to_owned(), value.into())
                }
            }
        }
    }

    #[derive(Debug, Default)]
    struct AggregationCapture {
        begins: usize,
        batches: Vec<(String, Vec<CapturedAggregation>)>,
    }

    impl AggregationResultsCacheBatchSink for AggregationCapture {
        fn begin_archive(&mut self) -> io::Result<()> {
            self.begins += 1;
            Ok(())
        }

        fn insert_aggregation_batch(
            &mut self,
            archive_id: &str,
            results: &[AggregationResultRef<'_>],
        ) -> io::Result<()> {
            self.batches.push((
                archive_id.to_owned(),
                results.iter().copied().map(Into::into).collect(),
            ));
            Ok(())
        }
    }

    fn search_results(
        archive: &[u8],
        query: &str,
        options: &SearchResultsCacheOptions,
        capture: &mut SearchCapture,
    ) -> u64 {
        let query = parse_kql(query, KqlLimits::default()).expect("parse query");
        let mut reader =
            SingleFileArchiveReader::open(Cursor::new(archive)).expect("open C++ fixture");
        let mut adapter = SearchResultsCacheAdapter::new(capture, "archive", "dataset", options);
        search_archive(
            &mut reader,
            &query,
            &mut adapter,
            &ArchiveSearchOptions::default(),
        )
        .expect("search C++ fixture");
        adapter.finish().expect("finish results-cache batches");
        adapter.records_written()
    }

    fn aggregation_results(plan: &AggregationPlan, batch_size: usize) -> AggregationCapture {
        let query = parse_kql("*: *", KqlLimits::default()).expect("parse query");
        let mut reader = SingleFileArchiveReader::open(Cursor::new(AGGREGATIONS_ARCHIVE))
            .expect("open C++ fixture");
        let mut capture = AggregationCapture::default();
        {
            let mut adapter = AggregationResultsCacheAdapter::new(
                &mut capture,
                "aggregation-archive",
                plan,
                batch_size,
            )
            .expect("valid batch size");
            search_archive(
                &mut reader,
                &query,
                &mut adapter,
                &ArchiveSearchOptions::default(),
            )
            .expect("search C++ fixture");
            adapter.finish().expect("finish aggregation batches");
        }
        capture
    }

    #[test]
    fn options_reject_zero_limits_and_retain_cpp_defaults() {
        assert_eq!(
            ResultsCacheOptionsError::ZeroBatchSize,
            SearchResultsCacheOptions::new(Projection::all(), 0, 1)
                .expect_err("zero batch must fail")
        );
        assert_eq!(
            ResultsCacheOptionsError::ZeroMaxNumResults,
            SearchResultsCacheOptions::new(Projection::all(), 1, 0)
                .expect_err("zero top-N must fail")
        );
        let defaults = SearchResultsCacheOptions::default();
        assert_eq!(1000, defaults.batch_size());
        assert_eq!(1000, defaults.max_num_results());
    }

    #[test]
    fn latest_results_are_per_archive_ordered_and_batched_without_repeated_strings() {
        let options = SearchResultsCacheOptions::new(Projection::all(), 2, 3)
            .expect("valid results-cache options");
        let mut capture = SearchCapture::default();
        assert_eq!(
            3,
            search_results(AGGREGATIONS_ARCHIVE, "*: *", &options, &mut capture)
        );
        assert_eq!(1, capture.begins);
        assert_eq!(
            vec![2, 1],
            capture
                .batches
                .iter()
                .map(|batch| batch.results.len())
                .collect::<Vec<_>>()
        );
        assert!(
            capture
                .batches
                .iter()
                .all(|batch| batch.archive_id == "archive" && batch.dataset == "dataset")
        );
        let results: Vec<_> = capture
            .batches
            .iter()
            .flat_map(|batch| batch.results.iter())
            .collect();
        assert_eq!(
            vec![-1_699_999_998_001, -1_699_999_998_000, -1_699_999_997_999],
            results
                .iter()
                .map(|result| result.timestamp())
                .collect::<Vec<_>>()
        );
        assert!(
            results
                .iter()
                .all(|result| result.message().ends_with('\n'))
        );
        assert_eq!(
            vec![0, 0, 0],
            results
                .iter()
                .map(|result| result.log_event_index())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn full_heap_rejects_equal_or_older_timestamps_and_replaces_only_strictly_newer() {
        let mut latest = BinaryHeap::new();
        for (sequence, timestamp) in [10, 10].into_iter().enumerate() {
            retain_latest(
                &mut latest,
                2,
                ResultsCacheSearchResult::new(format!("{sequence}\n"), timestamp, 0),
                u64::try_from(sequence).expect("sequence fits u64"),
            )
            .expect("retain initial result");
        }
        assert!(!should_retain(&latest, 2, 9));
        assert!(!should_retain(&latest, 2, 10));
        assert!(should_retain(&latest, 2, 11));
        let replaced = retain_latest(
            &mut latest,
            2,
            ResultsCacheSearchResult::new("new\n".to_owned(), 11, 0),
            2,
        )
        .expect("replace oldest result");
        assert_eq!(Some("0\n".to_owned()), replaced);
        assert_eq!(
            vec![(10, "1\n".to_owned()), (11, "new\n".to_owned())],
            std::iter::from_fn(|| latest.pop())
                .map(|retained| (retained.result.timestamp(), retained.result.message))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn zero_matches_open_after_preflight_but_never_insert() {
        let options = SearchResultsCacheOptions::default();
        let mut capture = SearchCapture::default();
        assert_eq!(
            0,
            search_results(
                MINIMAL_ARCHIVE,
                "level:DOES_NOT_EXIST",
                &options,
                &mut capture
            )
        );
        assert_eq!(1, capture.begins);
        assert_eq!(0, capture.batches.len());
    }

    #[test]
    fn batch_failure_reports_committed_prefix_and_emits_no_extra_batch() {
        let options = SearchResultsCacheOptions::new(Projection::all(), 2, 3)
            .expect("valid results-cache options");
        let query = parse_kql("*: *", KqlLimits::default()).expect("parse query");
        let mut reader = SingleFileArchiveReader::open(Cursor::new(AGGREGATIONS_ARCHIVE))
            .expect("open C++ fixture");
        let mut capture = SearchCapture {
            fail_after_batches: Some(1),
            ..SearchCapture::default()
        };
        let mut adapter =
            SearchResultsCacheAdapter::new(&mut capture, "archive", "dataset", &options);
        search_archive(
            &mut reader,
            &query,
            &mut adapter,
            &ArchiveSearchOptions::default(),
        )
        .expect("search C++ fixture");
        let error = adapter.finish().expect_err("second batch must fail");
        assert_eq!(2, adapter.records_written());
        assert!(error.to_string().contains("after 2 complete document(s)"));
        drop(adapter);
        assert_eq!(1, capture.batches.len());
    }

    #[test]
    fn aggregation_adapter_supports_all_cpp_kinds_and_batch_order() {
        let count = aggregation_results(&AggregationPlan::count(), 2);
        assert_eq!(1, count.begins);
        assert_eq!(vec![CapturedAggregation::Count(11)], count.batches[0].1);

        let by_time = aggregation_results(
            &AggregationPlan::count_by_time(1000).expect("valid time bucket"),
            2,
        );
        assert!(by_time.batches.iter().all(|(_, batch)| batch.len() <= 2));
        let buckets: Vec<_> = by_time
            .batches
            .iter()
            .flat_map(|(_, batch)| batch.iter())
            .filter_map(|result| match result {
                CapturedAggregation::CountByTime(timestamp, count) => Some((*timestamp, *count)),
                _ => None,
            })
            .collect();
        assert_eq!(
            vec![
                (-1_700_000_001_000, 2),
                (-1_700_000_000_000, 3),
                (-1_699_999_999_000, 2),
                (-1_699_999_998_000, 3),
                (-1_699_999_997_000, 1),
            ],
            buckets
        );

        let minimum = aggregation_results(
            &AggregationPlan::minimum("target").expect("valid minimum field"),
            2,
        );
        assert_eq!(
            CapturedAggregation::Minimum("target".to_owned(), AggregationNumber::Float(2.5)),
            minimum.batches[0].1[0]
        );
        let maximum = aggregation_results(
            &AggregationPlan::maximum("target").expect("valid maximum field"),
            2,
        );
        assert_eq!(
            CapturedAggregation::Maximum("target".to_owned(), AggregationNumber::Integer(10)),
            maximum.batches[0].1[0]
        );
        let unique = aggregation_results(
            &AggregationPlan::unique("unique").expect("valid unique field"),
            3,
        );
        assert_eq!(
            vec![3, 3, 2],
            unique
                .batches
                .iter()
                .map(|batch| batch.1.len())
                .collect::<Vec<_>>()
        );
    }
}
