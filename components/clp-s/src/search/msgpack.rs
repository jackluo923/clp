//! Streaming C++-compatible `MessagePack` search-result tuples.
//!
//! The C++ file handler concatenates independent five-element arrays without an outer container.
//! This adapter reconstructs one matching JSON record at a time, stages exactly one bounded tuple,
//! and calls the caller-owned [`Write`] destination once per complete tuple.

use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::fmt::Display;
use std::fmt::Formatter;
use std::io;
use std::io::Write;

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

const NANOSECONDS_PER_MILLISECOND: i64 = 1_000_000;
const MILLISECONDS_PER_SECOND: f64 = 1_000.0;
const TUPLE_LENGTH: u8 = 5;

/// Projection and record-formatting configuration for [`SearchMsgpackAdapter`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchMsgpackOptions {
    projection: Projection,
    plan: ExtractionPlanLimits,
    record: RecordLimits,
    byte_policy: JsonBytePolicy,
    result_metadata: bool,
}

impl SearchMsgpackOptions {
    /// Creates `MessagePack` output options for the given projection.
    #[must_use]
    pub fn new(projection: Projection) -> Self {
        Self {
            projection,
            plan: ExtractionPlanLimits::default(),
            record: RecordLimits::default(),
            byte_policy: JsonBytePolicy::StrictUtf8,
            result_metadata: true,
        }
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

    /// Selects strict UTF-8 or explicit C++ byte-preserving JSON reconstruction.
    #[must_use]
    pub const fn with_byte_policy(mut self, byte_policy: JsonBytePolicy) -> Self {
        self.byte_policy = byte_policy;
        self
    }

    /// Selects whether result tuples contain archive metadata.
    ///
    /// The C++ file handler includes the authoritative timestamp, archive identifier, and log
    /// event index. Its network handler uses the same five-element tuple framing but writes zero,
    /// an empty identifier, and zero in those fields. Disabling metadata reproduces the latter
    /// behavior and avoids resolving the metadata columns.
    #[must_use]
    pub const fn with_result_metadata(mut self, value: bool) -> Self {
        self.result_metadata = value;
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

    /// Returns whether result tuples include archive metadata.
    #[must_use]
    pub const fn result_metadata(&self) -> bool {
        self.result_metadata
    }
}

impl Default for SearchMsgpackOptions {
    fn default() -> Self {
        Self::new(Projection::all())
    }
}

/// Streams physical search matches as the C++ file handler's `MessagePack` tuples.
///
/// Create one adapter per archive search. Programs are cached per matching schema, while record
/// JSON and encoded tuple buffers are reused per row. The destination's lifetime remains entirely
/// caller-controlled, so bindings can concatenate archives or reproduce the CLI's per-archive
/// create/truncate behavior.
pub struct SearchMsgpackAdapter<'sink, 'id, 'options, W: ?Sized> {
    sink: &'sink mut W,
    archive_id: &'id [u8],
    options: &'options SearchMsgpackOptions,
    projection: Option<ResolvedProjection>,
    programs: HashMap<i32, RecordProgram>,
    log_event_index: LogEventIndex,
    scratch: RecordScratch,
    record: Vec<u8>,
    tuple: Vec<u8>,
    records_written: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LogEventIndex {
    Unresolved,
    Absent,
    Node(u32),
}

impl<'sink, 'id, 'options, W: Write + ?Sized> SearchMsgpackAdapter<'sink, 'id, 'options, W> {
    /// Creates an adapter borrowing a synchronous destination, archive ID, and configuration.
    #[must_use]
    pub fn new(
        sink: &'sink mut W,
        archive_id: &'id [u8],
        options: &'options SearchMsgpackOptions,
    ) -> Self {
        Self {
            sink,
            archive_id,
            options,
            projection: None,
            programs: HashMap::new(),
            log_event_index: LogEventIndex::Unresolved,
            scratch: RecordScratch::new(),
            record: Vec::new(),
            tuple: Vec::new(),
            records_written: 0,
        }
    }

    /// Returns the caller-owned destination.
    #[must_use]
    pub const fn sink_mut(&mut self) -> &mut W {
        self.sink
    }

    /// Returns the number of complete tuples accepted by the destination.
    #[must_use]
    pub const fn records_written(&self) -> u64 {
        self.records_written
    }

    fn prepare_projection(
        &mut self,
        matches: ArchiveTableMatches<'_, '_, '_>,
    ) -> Result<(), SearchMsgpackAdapterError> {
        if self.projection.is_none() {
            self.projection = Some(
                self.options
                    .projection()
                    .resolve(matches.catalog().schema_tree())
                    .map_err(SearchMsgpackAdapterError::Projection)?,
            );
        }
        Ok(())
    }

    fn prepare_log_event_index(
        &mut self,
        matches: ArchiveTableMatches<'_, '_, '_>,
    ) -> Result<(), SearchMsgpackAdapterError> {
        if LogEventIndex::Unresolved != self.log_event_index {
            return Ok(());
        }
        self.log_event_index = LogOrderLocator::discover(matches.catalog().schema_tree())
            .map_err(SearchMsgpackAdapterError::LogOrderDiscovery)?
            .map_or(LogEventIndex::Absent, |locator| {
                LogEventIndex::Node(locator.node_id())
            });
        Ok(())
    }

    fn compile_program(
        &self,
        matches: ArchiveTableMatches<'_, '_, '_>,
    ) -> Result<RecordProgram, SearchMsgpackAdapterError> {
        let schema_id = matches.schema_id();
        let plan = ExtractionPlan::compile(
            matches.table().schema(),
            matches.catalog().schema_tree(),
            self.options.plan_limits(),
        )
        .map_err(|source| SearchMsgpackAdapterError::Plan { schema_id, source })?;
        let plan = match self
            .projection
            .as_ref()
            .and_then(ResolvedProjection::selected_node_ids)
        {
            Some(node_ids) => plan
                .project_selected_nodes(node_ids)
                .map_err(|source| SearchMsgpackAdapterError::Plan { schema_id, source })?,
            None => plan,
        };
        RecordProgram::compile_with_byte_policy(
            &plan,
            matches.catalog().schema_tree(),
            self.options.byte_policy(),
            self.options.record_limits(),
        )
        .map_err(|source| SearchMsgpackAdapterError::Program { schema_id, source })
    }

    fn prepare_program(
        &mut self,
        matches: ArchiveTableMatches<'_, '_, '_>,
    ) -> Result<(), SearchMsgpackAdapterError> {
        let schema_id = matches.schema_id();
        if self.programs.contains_key(&schema_id) {
            return Ok(());
        }
        let program = self.compile_program(matches)?;
        self.programs
            .try_reserve(1)
            .map_err(|_| SearchMsgpackAdapterError::AllocationFailed {
                resource: SearchMsgpackResource::Programs,
                requested: self.programs.len().saturating_add(1),
            })?;
        self.programs.insert(schema_id, program);
        Ok(())
    }

    fn write_msgpack(
        &mut self,
        matches: ArchiveTableMatches<'_, '_, '_>,
    ) -> Result<(), SearchMsgpackAdapterError> {
        self.prepare_projection(matches)?;
        if self.options.result_metadata() {
            self.prepare_log_event_index(matches)?;
        }
        self.prepare_program(matches)?;
        let schema_id = matches.schema_id();
        let program = self
            .programs
            .get(&schema_id)
            .ok_or(SearchMsgpackAdapterError::SizeOverflow)?;
        let scratch = std::mem::take(&mut self.scratch);
        let mut writer = program
            .writer_with_scratch(
                matches.table().table(),
                matches.catalog().timestamp_patterns(),
                scratch,
            )
            .map_err(|source| SearchMsgpackAdapterError::Bind { schema_id, source })?;
        let mut output = TupleOutput {
            sink: &mut *self.sink,
            archive_id: if self.options.result_metadata() {
                self.archive_id
            } else {
                b""
            },
            record: &mut self.record,
            tuple: &mut self.tuple,
            records_written: &mut self.records_written,
        };
        let result = if self.options.result_metadata() {
            let log_event_indexes = locate_log_event_indexes(matches, self.log_event_index)?;
            let timestamp_column = authoritative_timestamp_column(matches)?;
            match timestamp_column.map(Column::data) {
                None => drain_rows(
                    &mut output,
                    matches,
                    &mut writer,
                    (0..matches.bitmap().len()).map(|_| 0_i64),
                    log_event_indexes,
                ),
                Some(ColumnData::Timestamp(values)) => drain_rows(
                    &mut output,
                    matches,
                    &mut writer,
                    values
                        .epochs()
                        .values()
                        .map(|epoch| epoch / NANOSECONDS_PER_MILLISECOND),
                    log_event_indexes,
                ),
                Some(ColumnData::DeprecatedDateString(values)) => drain_rows(
                    &mut output,
                    matches,
                    &mut writer,
                    values.epochs().iter(),
                    log_event_indexes,
                ),
                Some(ColumnData::Integer(values)) => drain_rows(
                    &mut output,
                    matches,
                    &mut writer,
                    values.iter(),
                    log_event_indexes,
                ),
                Some(ColumnData::DeltaInteger(values)) => drain_rows(
                    &mut output,
                    matches,
                    &mut writer,
                    values.values(),
                    log_event_indexes,
                ),
                Some(ColumnData::Float(values)) => drain_rows(
                    &mut output,
                    matches,
                    &mut writer,
                    values.iter().map(timestamp_seconds_to_milliseconds),
                    log_event_indexes,
                ),
                Some(column) => Err(SearchMsgpackAdapterError::InvalidTimestampColumn {
                    node_id: timestamp_column
                        .map(Column::node_id)
                        .ok_or(SearchMsgpackAdapterError::SizeOverflow)?,
                    node_type: column.node_type(),
                }),
            }
        } else {
            drain_rows(
                &mut output,
                matches,
                &mut writer,
                (0..matches.bitmap().len()).map(|_| 0_i64),
                None,
            )
        };
        self.scratch = writer.into_scratch();
        result
    }
}

impl<W: Write + ?Sized> ArchiveMatchSink for SearchMsgpackAdapter<'_, '_, '_, W> {
    fn write_matches(&mut self, matches: ArchiveTableMatches<'_, '_, '_>) -> io::Result<()> {
        self.write_msgpack(matches).map_err(io::Error::other)
    }
}

#[allow(clippy::cast_possible_truncation)]
fn timestamp_seconds_to_milliseconds(value: f64) -> i64 {
    (value * MILLISECONDS_PER_SECOND) as i64
}

fn authoritative_timestamp_column<'stream, 'archive>(
    matches: ArchiveTableMatches<'_, 'stream, 'archive>,
) -> Result<Option<Column<'stream, 'archive>>, SearchMsgpackAdapterError> {
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
        let node_id =
            i32::try_from(column.node_id()).map_err(|_| SearchMsgpackAdapterError::SizeOverflow)?;
        if authoritative.column_ids().contains(&node_id) {
            // The C++ reader retains the last authoritative ordered column in schema order.
            selected = Some(*column);
        }
    }
    Ok(selected)
}

fn locate_log_event_indexes<'stream>(
    matches: ArchiveTableMatches<'_, 'stream, '_>,
    locator: LogEventIndex,
) -> Result<Option<DeltaI64Values<'stream>>, SearchMsgpackAdapterError> {
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
            return Err(SearchMsgpackAdapterError::InvalidLogEventIndexColumn {
                node_id,
                node_type: column.node_type(),
            });
        };
        selected = Some(values.values());
    }
    Ok(selected)
}

struct TupleOutput<'a, W: ?Sized> {
    sink: &'a mut W,
    archive_id: &'a [u8],
    record: &'a mut Vec<u8>,
    tuple: &'a mut Vec<u8>,
    records_written: &'a mut u64,
}

#[allow(clippy::too_many_lines)]
fn drain_rows<W, I>(
    output: &mut TupleOutput<'_, W>,
    matches: ArchiveTableMatches<'_, '_, '_>,
    writer: &mut RecordWriter<'_, '_, '_, '_>,
    mut timestamps: I,
    mut log_event_indexes: Option<DeltaI64Values<'_>>,
) -> Result<(), SearchMsgpackAdapterError>
where
    W: Write + ?Sized,
    I: ExactSizeIterator<Item = i64>, {
    let schema_id = matches.schema_id();
    let row_count = matches.bitmap().len();
    if timestamps.len() != row_count {
        return Err(SearchMsgpackAdapterError::TimestampRowCountMismatch {
            schema_id,
            expected: row_count,
            actual: timestamps.len(),
        });
    }
    if let Some(values) = &log_event_indexes
        && values.len() != row_count
    {
        return Err(SearchMsgpackAdapterError::LogEventIndexRowCountMismatch {
            schema_id,
            expected: row_count,
            actual: values.len(),
        });
    }

    let mut pending_skips = 0_usize;
    for (row_index, matched) in matches.bitmap().as_bytes().iter().copied().enumerate() {
        if 0 == matched {
            pending_skips = pending_skips
                .checked_add(1)
                .ok_or(SearchMsgpackAdapterError::SizeOverflow)?;
            continue;
        }

        let skip_start = row_index
            .checked_sub(pending_skips)
            .ok_or(SearchMsgpackAdapterError::SizeOverflow)?;
        skip_auxiliary_rows(
            &mut timestamps,
            &mut log_event_indexes,
            schema_id,
            skip_start,
            pending_skips,
        )?;
        if !writer.skip_records(pending_skips).map_err(|source| {
            SearchMsgpackAdapterError::Record {
                schema_id,
                row_index: skip_start,
                source,
            }
        })? {
            return Err(SearchMsgpackAdapterError::RecordRowCountMismatch {
                schema_id,
                expected: row_count,
                actual: writer.next_row_index(),
            });
        }
        pending_skips = 0;

        let timestamp =
            timestamps
                .next()
                .ok_or(SearchMsgpackAdapterError::MissingTimestampValue {
                    schema_id,
                    row_index,
                })?;
        let log_event_index = match &mut log_event_indexes {
            Some(values) => {
                values
                    .next()
                    .ok_or(SearchMsgpackAdapterError::MissingLogEventIndexValue {
                        schema_id,
                        row_index,
                    })?
            }
            None => 0,
        };
        output.record.clear();
        if !writer.append_next_record(output.record).map_err(|source| {
            SearchMsgpackAdapterError::Record {
                schema_id,
                row_index,
                source,
            }
        })? {
            return Err(SearchMsgpackAdapterError::RecordRowCountMismatch {
                schema_id,
                expected: row_count,
                actual: row_index,
            });
        }
        if output.record.last() != Some(&b'\n') {
            output.record.try_reserve(1).map_err(|_| {
                SearchMsgpackAdapterError::AllocationFailed {
                    resource: SearchMsgpackResource::Record,
                    requested: output.record.len().saturating_add(1),
                }
            })?;
            output.record.push(b'\n');
        }
        encode_tuple(
            output.tuple,
            timestamp,
            output.record,
            output.archive_id,
            log_event_index,
        )?;
        let attempted_bytes = u64::try_from(output.tuple.len())
            .map_err(|_| SearchMsgpackAdapterError::SizeOverflow)?;
        output.sink.write_all(output.tuple).map_err(|source| {
            SearchMsgpackAdapterError::Output {
                schema_id,
                row_index,
                completed_records: *output.records_written,
                attempted_bytes,
                source,
            }
        })?;
        *output.records_written = (*output.records_written)
            .checked_add(1)
            .ok_or(SearchMsgpackAdapterError::SizeOverflow)?;
    }
    let skip_start = row_count
        .checked_sub(pending_skips)
        .ok_or(SearchMsgpackAdapterError::SizeOverflow)?;
    skip_auxiliary_rows(
        &mut timestamps,
        &mut log_event_indexes,
        schema_id,
        skip_start,
        pending_skips,
    )?;
    if !writer
        .skip_records(pending_skips)
        .map_err(|source| SearchMsgpackAdapterError::Record {
            schema_id,
            row_index: skip_start,
            source,
        })?
    {
        return Err(SearchMsgpackAdapterError::RecordRowCountMismatch {
            schema_id,
            expected: row_count,
            actual: writer.next_row_index(),
        });
    }
    if 0 != writer.remaining() {
        return Err(SearchMsgpackAdapterError::RecordRowCountMismatch {
            schema_id,
            expected: row_count,
            actual: row_count - writer.remaining(),
        });
    }
    Ok(())
}

fn skip_auxiliary_rows<I>(
    timestamps: &mut I,
    log_event_indexes: &mut Option<DeltaI64Values<'_>>,
    schema_id: i32,
    row_index: usize,
    count: usize,
) -> Result<(), SearchMsgpackAdapterError>
where
    I: ExactSizeIterator<Item = i64>, {
    if 0 == count {
        return Ok(());
    }
    if timestamps.len() < count {
        return Err(SearchMsgpackAdapterError::MissingTimestampValue {
            schema_id,
            row_index: row_index
                .checked_add(timestamps.len())
                .ok_or(SearchMsgpackAdapterError::SizeOverflow)?,
        });
    }
    let _ = timestamps.nth(count - 1);
    if let Some(values) = log_event_indexes {
        if values.len() < count {
            return Err(SearchMsgpackAdapterError::MissingLogEventIndexValue {
                schema_id,
                row_index: row_index
                    .checked_add(values.len())
                    .ok_or(SearchMsgpackAdapterError::SizeOverflow)?,
            });
        }
        let _ = values.nth(count - 1);
    }
    Ok(())
}

fn encode_tuple(
    output: &mut Vec<u8>,
    timestamp: i64,
    message: &[u8],
    archive_id: &[u8],
    log_event_index: i64,
) -> Result<(), SearchMsgpackAdapterError> {
    let message_len =
        u32::try_from(message.len()).map_err(|_| SearchMsgpackAdapterError::StringTooLong {
            field: SearchMsgpackString::Message,
            actual: message.len(),
        })?;
    let archive_id_len =
        u32::try_from(archive_id.len()).map_err(|_| SearchMsgpackAdapterError::StringTooLong {
            field: SearchMsgpackString::ArchiveId,
            actual: archive_id.len(),
        })?;
    let required = 1_usize
        .checked_add(encoded_i64_len(timestamp))
        .and_then(|size| size.checked_add(encoded_string_len(message_len)))
        .and_then(|size| size.checked_add(1))
        .and_then(|size| size.checked_add(encoded_string_len(archive_id_len)))
        .and_then(|size| size.checked_add(encoded_i64_len(log_event_index)))
        .ok_or(SearchMsgpackAdapterError::SizeOverflow)?;

    output.clear();
    output
        .try_reserve(required)
        .map_err(|_| SearchMsgpackAdapterError::AllocationFailed {
            resource: SearchMsgpackResource::Tuple,
            requested: required,
        })?;
    output.push(0x90 | TUPLE_LENGTH);
    append_i64(output, timestamp);
    append_string(output, message, message_len);
    output.push(0xa0);
    append_string(output, archive_id, archive_id_len);
    append_i64(output, log_event_index);
    debug_assert_eq!(required, output.len());
    Ok(())
}

fn encoded_i64_len(value: i64) -> usize {
    if (0..=0x7f).contains(&value) || (-32..0).contains(&value) {
        1
    } else if value >= 0 {
        encoded_u64_len(u64::try_from(value).expect("nonnegative i64 fits u64"))
    } else if i8::try_from(value).is_ok() {
        2
    } else if i16::try_from(value).is_ok() {
        3
    } else if i32::try_from(value).is_ok() {
        5
    } else {
        9
    }
}

fn encoded_u64_len(value: u64) -> usize {
    if u8::try_from(value).is_ok() {
        2
    } else if u16::try_from(value).is_ok() {
        3
    } else if u32::try_from(value).is_ok() {
        5
    } else {
        9
    }
}

fn append_i64(output: &mut Vec<u8>, value: i64) {
    if (0..=0x7f).contains(&value) {
        output.push(u8::try_from(value).expect("positive fixint fits u8"));
    } else if (-32..0).contains(&value) {
        output.extend_from_slice(
            &i8::try_from(value)
                .expect("negative fixint fits i8")
                .to_be_bytes(),
        );
    } else if value >= 0 {
        append_u64(
            output,
            u64::try_from(value).expect("nonnegative i64 fits u64"),
        );
    } else if value >= i64::from(i8::MIN) {
        output.push(0xd0);
        output.extend_from_slice(&i8::try_from(value).expect("value fits i8").to_be_bytes());
    } else if value >= i64::from(i16::MIN) {
        output.push(0xd1);
        output.extend_from_slice(&i16::try_from(value).expect("value fits i16").to_be_bytes());
    } else if value >= i64::from(i32::MIN) {
        output.push(0xd2);
        output.extend_from_slice(&i32::try_from(value).expect("value fits i32").to_be_bytes());
    } else {
        output.push(0xd3);
        output.extend_from_slice(&value.to_be_bytes());
    }
}

fn append_u64(output: &mut Vec<u8>, value: u64) {
    if let Ok(value) = u8::try_from(value) {
        output.push(0xcc);
        output.push(value);
    } else if let Ok(value) = u16::try_from(value) {
        output.push(0xcd);
        output.extend_from_slice(&value.to_be_bytes());
    } else if let Ok(value) = u32::try_from(value) {
        output.push(0xce);
        output.extend_from_slice(&value.to_be_bytes());
    } else {
        output.push(0xcf);
        output.extend_from_slice(&value.to_be_bytes());
    }
}

fn encoded_string_len(length: u32) -> usize {
    let header = if length < 32 {
        1
    } else if u8::try_from(length).is_ok() {
        2
    } else if u16::try_from(length).is_ok() {
        3
    } else {
        5
    };
    header + usize::try_from(length).expect("u32 fits usize")
}

fn append_string(output: &mut Vec<u8>, value: &[u8], length: u32) {
    if length < 32 {
        output.push(0xa0 | u8::try_from(length).expect("fixstr length fits u8"));
    } else if let Ok(length) = u8::try_from(length) {
        output.push(0xd9);
        output.push(length);
    } else if let Ok(length) = u16::try_from(length) {
        output.push(0xda);
        output.extend_from_slice(&length.to_be_bytes());
    } else {
        output.push(0xdb);
        output.extend_from_slice(&length.to_be_bytes());
    }
    output.extend_from_slice(value);
}

/// Bounded allocation named by a `MessagePack` adapter failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum SearchMsgpackResource {
    /// Cached schema programs.
    Programs,
    /// One reconstructed JSON record.
    Record,
    /// One staged `MessagePack` tuple.
    Tuple,
}

impl Display for SearchMsgpackResource {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Programs => "schema programs",
            Self::Record => "record bytes",
            Self::Tuple => "tuple bytes",
        })
    }
}

/// String position in the C++ file-handler tuple.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum SearchMsgpackString {
    /// Reconstructed JSON message.
    Message,
    /// Caller-supplied archive identifier.
    ArchiveId,
}

impl Display for SearchMsgpackString {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Message => "message",
            Self::ArchiveId => "archive ID",
        })
    }
}

/// Failure while reconstructing or streaming `MessagePack` search tuples.
#[derive(Debug)]
#[non_exhaustive]
pub enum SearchMsgpackAdapterError {
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
    /// A `MessagePack` string exceeded the format's 32-bit length field.
    StringTooLong {
        /// Tuple string position.
        field: SearchMsgpackString,
        /// Required bytes.
        actual: usize,
    },
    /// A bounded allocation failed.
    AllocationFailed {
        /// Retained resource.
        resource: SearchMsgpackResource,
        /// Requested elements or bytes.
        requested: usize,
    },
    /// Checked size or count arithmetic overflowed.
    SizeOverflow,
    /// The destination failed while accepting one staged tuple.
    ///
    /// The adapter makes one `write_all` call per tuple. A destination that performs a partial
    /// write before returning an error may therefore retain that tuple's prefix.
    Output {
        /// Opaque schema ID.
        schema_id: i32,
        /// Physical table-local row index.
        row_index: usize,
        /// Complete tuples accepted before this attempt.
        completed_records: u64,
        /// Bytes in the staged tuple.
        attempted_bytes: u64,
        /// Destination failure.
        source: io::Error,
    },
}

impl Display for SearchMsgpackAdapterError {
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
            Self::StringTooLong { field, actual } => {
                write!(
                    formatter,
                    "MessagePack {field} is {actual} bytes, exceeding u32"
                )
            }
            Self::AllocationFailed {
                resource,
                requested,
            } => write!(
                formatter,
                "failed to allocate {requested} element(s) for MessagePack {resource}"
            ),
            Self::SizeOverflow => formatter.write_str("MessagePack result size overflow"),
            Self::Output {
                schema_id,
                row_index,
                completed_records,
                attempted_bytes,
                source,
            } => write!(
                formatter,
                "output rejected {attempted_bytes}-byte schema {schema_id}, row {row_index} tuple \
                 after {completed_records} complete record(s): {source}"
            ),
        }
    }
}

impl Error for SearchMsgpackAdapterError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Projection(source) => Some(source),
            Self::Plan { source, .. } => Some(source),
            Self::Program { source, .. } => Some(source),
            Self::Bind { source, .. } => Some(source),
            Self::LogOrderDiscovery(source) => Some(source),
            Self::Record { source, .. } => Some(source),
            Self::Output { source, .. } => Some(source),
            Self::InvalidTimestampColumn { .. }
            | Self::InvalidLogEventIndexColumn { .. }
            | Self::TimestampRowCountMismatch { .. }
            | Self::LogEventIndexRowCountMismatch { .. }
            | Self::RecordRowCountMismatch { .. }
            | Self::MissingTimestampValue { .. }
            | Self::MissingLogEventIndexValue { .. }
            | Self::StringTooLong { .. }
            | Self::AllocationFailed { .. }
            | Self::SizeOverflow => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::io::Write;

    use super::*;
    use crate::archive::SingleFileArchiveReader;
    use crate::search::ArchiveSearchOptions;
    use crate::search::KqlLimits;
    use crate::search::parse_kql;
    use crate::search::search_archive;

    const MINIMAL_ARCHIVE: &[u8] =
        include_bytes!("../../tests/fixtures/sfa-v0.5.0-minimal-cpp.bin");
    const LOG_ORDER_ARCHIVE: &[u8] =
        include_bytes!("../../tests/fixtures/sfa-v0.5.0-log-order-cpp.bin");
    const STRINGS_ARCHIVE: &[u8] =
        include_bytes!("../../tests/fixtures/sfa-v0.5.0-strings-cpp.bin");
    const MINIMAL_ORACLE: &str =
        include_str!("../../tests/fixtures/search-file-v0.5.0-minimal-cpp.hex");
    const LOG_ORDER_ORACLE: &str =
        include_str!("../../tests/fixtures/search-file-v0.5.0-log-order-cpp.hex");
    const STRINGS_ORACLE: &str =
        include_str!("../../tests/fixtures/search-file-v0.5.0-strings-cpp.hex");
    const PROJECTION_ORACLE: &str =
        include_str!("../../tests/fixtures/search-file-v0.5.0-projection-cpp.hex");
    const NETWORK_ORACLE: &str =
        include_str!("../../tests/fixtures/search-network-v0.5.0-minimal-cpp.hex");

    fn search(
        archive: &[u8],
        archive_id: &[u8],
        query: &str,
        options: &SearchMsgpackOptions,
    ) -> Vec<u8> {
        let query = parse_kql(query, KqlLimits::default()).expect("parse query");
        let mut reader =
            SingleFileArchiveReader::open(Cursor::new(archive)).expect("open C++ fixture");
        let mut output = Vec::new();
        let mut adapter = SearchMsgpackAdapter::new(&mut output, archive_id, options);
        search_archive(
            &mut reader,
            &query,
            &mut adapter,
            &ArchiveSearchOptions::default(),
        )
        .expect("search fixture");
        output
    }

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

    #[test]
    fn authoritative_timestamp_message_newline_and_tuple_bytes_match_cpp() {
        assert_eq!(
            decode_hex(MINIMAL_ORACLE),
            search(
                MINIMAL_ARCHIVE,
                b"sfa-v0.5.0-minimal-cpp.bin",
                "*: *",
                &SearchMsgpackOptions::default(),
            )
        );
    }

    #[test]
    fn missing_timestamp_and_physical_log_event_order_match_cpp() {
        assert_eq!(
            decode_hex(LOG_ORDER_ORACLE),
            search(
                LOG_ORDER_ARCHIVE,
                b"sfa-v0.5.0-log-order-cpp.bin",
                "*: *",
                &SearchMsgpackOptions::default(),
            )
        );
    }

    #[test]
    fn missing_timestamp_and_log_order_metadata_both_use_zero() {
        assert_eq!(
            decode_hex(STRINGS_ORACLE),
            search(
                STRINGS_ARCHIVE,
                b"sfa-v0.5.0-strings-cpp.bin",
                "*: *",
                &SearchMsgpackOptions::default(),
            )
        );
    }

    #[test]
    fn projection_changes_only_json_message_and_retains_metadata() {
        let projection = Projection::selected(
            &["message", "ts", "missing"],
            super::super::ProjectionLimits::default(),
        )
        .expect("projection");
        let options = SearchMsgpackOptions::new(projection);
        assert_eq!(
            decode_hex(PROJECTION_ORACLE),
            search(
                MINIMAL_ARCHIVE,
                b"sfa-v0.5.0-minimal-cpp.bin",
                "*: *",
                &options,
            )
        );
    }

    #[test]
    fn network_mode_uses_cpp_tuple_framing_without_result_metadata() {
        let options = SearchMsgpackOptions::default().with_result_metadata(false);
        assert!(!options.result_metadata());
        assert_eq!(
            decode_hex(NETWORK_ORACLE),
            search(MINIMAL_ARCHIVE, b"ignored-archive-id", "*: *", &options)
        );
    }

    #[test]
    fn integer_size_accounting_matches_canonical_encoding_at_boundaries() {
        for value in [
            i64::MIN,
            i64::from(i32::MIN) - 1,
            i64::from(i32::MIN),
            i64::from(i16::MIN) - 1,
            i64::from(i16::MIN),
            i64::from(i8::MIN) - 1,
            i64::from(i8::MIN),
            -33,
            -32,
            -1,
            0,
            127,
            128,
            i64::from(u8::MAX),
            i64::from(u8::MAX) + 1,
            i64::from(u16::MAX),
            i64::from(u16::MAX) + 1,
            i64::from(u32::MAX),
            i64::from(u32::MAX) + 1,
            i64::MAX,
        ] {
            let mut encoded = Vec::new();
            append_i64(&mut encoded, value);
            assert_eq!(encoded_i64_len(value), encoded.len(), "value {value}");
        }
    }

    #[test]
    fn archive_identifier_is_an_opaque_messagepack_string() {
        let mut encoded = Vec::new();
        encode_tuple(&mut encoded, 0, b"{}\n", b"\xff", 0).expect("encode tuple");
        assert_eq!(
            [0x95, 0x00, 0xa3, b'{', b'}', b'\n', 0xa0, 0xa1, 0xff, 0x00],
            encoded.as_slice()
        );
    }

    struct PrefixThenError {
        prefix_limit: usize,
        bytes: Vec<u8>,
    }

    impl Write for PrefixThenError {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.bytes.len() == self.prefix_limit {
                return Err(io::Error::other("injected output failure"));
            }
            let count = (self.prefix_limit - self.bytes.len()).min(bytes.len());
            self.bytes.extend_from_slice(&bytes[..count]);
            Ok(count)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn one_tuple_is_staged_before_a_partial_destination_failure() {
        let query = parse_kql("*: *", KqlLimits::default()).expect("parse query");
        let mut reader =
            SingleFileArchiveReader::open(Cursor::new(MINIMAL_ARCHIVE)).expect("open C++ fixture");
        let options = SearchMsgpackOptions::default();
        let mut output = PrefixThenError {
            prefix_limit: 7,
            bytes: Vec::new(),
        };
        let mut adapter = SearchMsgpackAdapter::new(&mut output, b"archive", &options);
        let error = search_archive(
            &mut reader,
            &query,
            &mut adapter,
            &ArchiveSearchOptions::default(),
        )
        .expect_err("partial writer must fail");
        assert!(error.to_string().contains("injected output failure"));
        assert_eq!(0, adapter.records_written());
        assert_eq!(7, output.bytes.len());
    }
}
