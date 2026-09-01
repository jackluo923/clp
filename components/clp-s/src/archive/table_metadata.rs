use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::fmt::Display;
use std::fmt::Formatter;
use std::io::Read;
use std::io::Take;
use std::io::{self};
use std::ops::Range;

use super::schema_map::SchemaMap;

const COUNT_SIZE: u64 = 8;
const PACKED_STREAM_ENTRY_SIZE: u64 = 8 + 8;
const SCHEMA_TABLE_ENTRY_SIZE: u64 = 8 + 8 + 4 + 8;
const FIXED_SECTION_SIZE: u64 = COUNT_SIZE + COUNT_SIZE + COUNT_SIZE;

/// Resource limits applied while decoding a table-metadata section.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TableMetadataLimits {
    compressed: u64,
    decompressed: u64,
    packed_streams: u64,
    schema_tables: u64,
    uncompressed_stream: u64,
    total_uncompressed_streams: u64,
    messages_per_table: u64,
    total_messages: u64,
}

impl TableMetadataLimits {
    /// Creates limits for the section sizes and entry counts.
    #[must_use]
    pub const fn new(
        max_compressed_size: u64,
        max_decompressed_size: u64,
        max_packed_streams: u64,
        max_schema_tables: u64,
    ) -> Self {
        Self {
            compressed: max_compressed_size,
            decompressed: max_decompressed_size,
            packed_streams: max_packed_streams,
            schema_tables: max_schema_tables,
            uncompressed_stream: u64::MAX,
            total_uncompressed_streams: u64::MAX,
            messages_per_table: u64::MAX,
            total_messages: u64::MAX,
        }
    }

    /// Replaces the limits for advertised decompressed packed-stream sizes.
    #[must_use]
    pub const fn with_uncompressed_stream_limits(
        mut self,
        max_stream_size: u64,
        max_total_size: u64,
    ) -> Self {
        self.uncompressed_stream = max_stream_size;
        self.total_uncompressed_streams = max_total_size;
        self
    }

    /// Replaces the limits for per-table and total message counts.
    #[must_use]
    pub const fn with_message_limits(
        mut self,
        max_messages_per_table: u64,
        max_total_messages: u64,
    ) -> Self {
        self.messages_per_table = max_messages_per_table;
        self.total_messages = max_total_messages;
        self
    }

    /// Maximum compressed section bytes accepted.
    #[must_use]
    pub const fn max_compressed_size(self) -> u64 {
        self.compressed
    }

    /// Maximum decompressed section bytes accepted.
    #[must_use]
    pub const fn max_decompressed_size(self) -> u64 {
        self.decompressed
    }

    /// Maximum number of packed streams accepted.
    #[must_use]
    pub const fn max_packed_streams(self) -> u64 {
        self.packed_streams
    }

    /// Maximum number of schema tables accepted.
    #[must_use]
    pub const fn max_schema_tables(self) -> u64 {
        self.schema_tables
    }

    /// Maximum advertised decompressed size of one packed stream.
    #[must_use]
    pub const fn max_uncompressed_stream_size(self) -> u64 {
        self.uncompressed_stream
    }

    /// Maximum total advertised decompressed size of all packed streams.
    #[must_use]
    pub const fn max_total_uncompressed_stream_size(self) -> u64 {
        self.total_uncompressed_streams
    }

    /// Maximum number of messages accepted in one schema table.
    #[must_use]
    pub const fn max_messages_per_table(self) -> u64 {
        self.messages_per_table
    }

    /// Maximum total number of messages accepted across schema tables.
    #[must_use]
    pub const fn max_total_messages(self) -> u64 {
        self.total_messages
    }
}

impl Default for TableMetadataLimits {
    fn default() -> Self {
        const MEBIBYTE: u64 = 1024 * 1024;
        const GIBIBYTE: u64 = 1024 * MEBIBYTE;
        Self::new(256 * MEBIBYTE, 512 * MEBIBYTE, 1_048_576, 16_777_216)
            .with_uncompressed_stream_limits(1024 * GIBIBYTE, 16 * 1024 * GIBIBYTE)
            .with_message_limits(1_u64 << 40, 1_u64 << 44)
    }
}

/// One packed zstd stream in the `/0` section.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PackedStreamMetadata {
    file_offset: u64,
    compressed_size: u64,
    uncompressed_size: u64,
}

impl PackedStreamMetadata {
    /// Returns the stream's byte offset relative to the start of `/0`.
    #[must_use]
    pub const fn file_offset(&self) -> u64 {
        self.file_offset
    }

    /// Returns the stream's inferred compressed byte length in `/0`.
    #[must_use]
    pub const fn compressed_size(&self) -> u64 {
        self.compressed_size
    }

    /// Returns the stream's relative compressed byte range in `/0`.
    #[must_use]
    pub const fn compressed_range(&self) -> Range<u64> {
        self.file_offset..(self.file_offset + self.compressed_size)
    }

    /// Returns the exact decompressed byte length advertised by the writer.
    #[must_use]
    pub const fn uncompressed_size(&self) -> u64 {
        self.uncompressed_size
    }
}

/// One schema table packed into a decompressed stream.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SchemaTableMetadata {
    stream_id: u64,
    stream_offset: u64,
    uncompressed_size: u64,
    schema_id: i32,
    message_count: u64,
}

impl SchemaTableMetadata {
    /// Returns the zero-based packed-stream ID.
    #[must_use]
    pub const fn stream_id(&self) -> u64 {
        self.stream_id
    }

    /// Returns the table's offset in the decompressed packed stream.
    #[must_use]
    pub const fn stream_offset(&self) -> u64 {
        self.stream_offset
    }

    /// Returns the table's inferred decompressed byte length.
    #[must_use]
    pub const fn uncompressed_size(&self) -> u64 {
        self.uncompressed_size
    }

    /// Returns the opaque schema ID defined by the schema map.
    #[must_use]
    pub const fn schema_id(&self) -> i32 {
        self.schema_id
    }

    /// Returns the number of records encoded in the table.
    #[must_use]
    pub const fn message_count(&self) -> u64 {
        self.message_count
    }
}

/// Validated packed-stream and schema-table metadata for a v0.5 archive.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TableMetadata {
    packed_streams: Vec<PackedStreamMetadata>,
    schema_tables: Vec<SchemaTableMetadata>,
    table_indexes: HashMap<i32, usize>,
    total_uncompressed_stream_size: u64,
    record_count: u64,
}

impl TableMetadata {
    /// Returns packed streams in physical `/0` order.
    #[must_use]
    pub fn packed_streams(&self) -> &[PackedStreamMetadata] {
        &self.packed_streams
    }

    /// Returns one packed stream by its zero-based ID.
    #[must_use]
    pub fn packed_stream(&self, stream_id: usize) -> Option<&PackedStreamMetadata> {
        self.packed_streams.get(stream_id)
    }

    /// Returns schema tables in physical decompression order.
    #[must_use]
    pub fn schema_tables(&self) -> &[SchemaTableMetadata] {
        &self.schema_tables
    }

    /// Finds a schema table by its opaque schema ID.
    #[must_use]
    pub fn schema_table(&self, schema_id: i32) -> Option<&SchemaTableMetadata> {
        self.table_indexes
            .get(&schema_id)
            .map(|index| &self.schema_tables[*index])
    }

    /// Returns the sum of advertised decompressed packed-stream sizes.
    #[must_use]
    pub const fn total_uncompressed_stream_size(&self) -> u64 {
        self.total_uncompressed_stream_size
    }

    /// Returns the sum of all schema-table message counts.
    #[must_use]
    pub const fn record_count(&self) -> u64 {
        self.record_count
    }
}

pub(super) fn decode_table_metadata<R: Read>(
    compressed: Take<R>,
    schema_map: &SchemaMap,
    tables_compressed_size: u64,
    limits: TableMetadataLimits,
) -> Result<TableMetadata, TableMetadataError> {
    let compressed_size = compressed.limit();
    if compressed_size > limits.compressed {
        return Err(TableMetadataError::CompressedSectionTooLarge {
            actual: compressed_size,
            limit: limits.compressed,
        });
    }

    let mut decoder = zstd::stream::read::Decoder::new(compressed)
        .map_err(TableMetadataError::Io)?
        .single_frame();
    let metadata = decode_entries(&mut decoder, schema_map, tables_compressed_size, limits)?;

    let mut trailing = [0_u8; 1];
    if 0 != decoder
        .read(&mut trailing)
        .map_err(TableMetadataError::Io)?
    {
        return Err(TableMetadataError::TrailingDecompressedData);
    }

    let compressed = decoder.finish();
    let remaining_compressed = u64::try_from(compressed.buffer().len())
        .map_err(|_| TableMetadataError::SizeOverflow)?
        .checked_add(compressed.get_ref().limit())
        .ok_or(TableMetadataError::SizeOverflow)?;
    if 0 != remaining_compressed {
        return Err(TableMetadataError::TrailingCompressedData {
            remaining: remaining_compressed,
        });
    }

    Ok(metadata)
}

fn decode_entries<R: Read>(
    reader: &mut R,
    schema_map: &SchemaMap,
    tables_compressed_size: u64,
    limits: TableMetadataLimits,
) -> Result<TableMetadata, TableMetadataError> {
    let decoded_streams = decode_packed_streams(reader, tables_compressed_size, limits)?;
    reject_separate_columns(read_u64(reader)?)?;
    let decoded_tables = decode_schema_tables(
        reader,
        schema_map,
        &decoded_streams.entries,
        decoded_streams.minimum_section_size,
        limits,
    )?;

    Ok(TableMetadata {
        packed_streams: decoded_streams.entries,
        schema_tables: decoded_tables.entries,
        table_indexes: decoded_tables.indexes,
        total_uncompressed_stream_size: decoded_streams.total_uncompressed_size,
        record_count: decoded_tables.record_count,
    })
}

struct DecodedPackedStreams {
    entries: Vec<PackedStreamMetadata>,
    minimum_section_size: u64,
    total_uncompressed_size: u64,
}

fn decode_packed_streams<R: Read>(
    reader: &mut R,
    tables_compressed_size: u64,
    limits: TableMetadataLimits,
) -> Result<DecodedPackedStreams, TableMetadataError> {
    let packed_stream_count = read_u64(reader)?;
    check_count_limit(
        TableMetadataResource::PackedStreams,
        packed_stream_count,
        limits.packed_streams,
    )?;
    let minimum_size = packed_stream_count
        .checked_mul(PACKED_STREAM_ENTRY_SIZE)
        .and_then(|size| size.checked_add(FIXED_SECTION_SIZE))
        .ok_or(TableMetadataError::SizeOverflow)?;
    check_decompressed_size(minimum_size, limits)?;
    if 0 == packed_stream_count && 0 != tables_compressed_size {
        return Err(TableMetadataError::EmptyPackedStreamList);
    }

    let stream_capacity =
        usize::try_from(packed_stream_count).map_err(|_| TableMetadataError::SizeOverflow)?;
    let mut packed_streams: Vec<PackedStreamMetadata> = Vec::new();
    packed_streams
        .try_reserve_exact(stream_capacity)
        .map_err(|_| TableMetadataError::AllocationFailed {
            requested: stream_capacity,
        })?;
    let mut total_uncompressed_stream_size = 0_u64;
    for stream_index in 0..stream_capacity {
        let file_offset = read_u64(reader)?;
        let uncompressed_size = read_u64(reader)?;
        validate_packed_stream(
            &packed_streams,
            stream_index,
            file_offset,
            uncompressed_size,
            tables_compressed_size,
            limits,
        )?;
        total_uncompressed_stream_size = total_uncompressed_stream_size
            .checked_add(uncompressed_size)
            .ok_or(TableMetadataError::SizeOverflow)?;
        if total_uncompressed_stream_size > limits.total_uncompressed_streams {
            return Err(TableMetadataError::TotalUncompressedStreamsTooLarge {
                actual: total_uncompressed_stream_size,
                limit: limits.total_uncompressed_streams,
            });
        }
        packed_streams.push(PackedStreamMetadata {
            file_offset,
            compressed_size: 0,
            uncompressed_size,
        });
    }
    infer_and_validate_compressed_stream_sizes(&mut packed_streams, tables_compressed_size)?;

    Ok(DecodedPackedStreams {
        entries: packed_streams,
        minimum_section_size: minimum_size,
        total_uncompressed_size: total_uncompressed_stream_size,
    })
}

const fn validate_packed_stream(
    previous_streams: &[PackedStreamMetadata],
    stream_index: usize,
    file_offset: u64,
    uncompressed_size: u64,
    tables_compressed_size: u64,
    limits: TableMetadataLimits,
) -> Result<(), TableMetadataError> {
    if 0 == stream_index && 0 != file_offset {
        return Err(TableMetadataError::InvalidFirstPackedStreamOffset {
            actual: file_offset,
        });
    }
    if file_offset > tables_compressed_size {
        return Err(TableMetadataError::PackedStreamOffsetOutOfBounds {
            stream_index,
            offset: file_offset,
            tables_compressed_size,
        });
    }
    if let Some(previous) = previous_streams.last()
        && file_offset < previous.file_offset
    {
        return Err(TableMetadataError::NonMonotonicPackedStreamOffset {
            stream_index,
            previous: previous.file_offset,
            actual: file_offset,
        });
    }
    if uncompressed_size > limits.uncompressed_stream {
        return Err(TableMetadataError::UncompressedStreamTooLarge {
            stream_index,
            actual: uncompressed_size,
            limit: limits.uncompressed_stream,
        });
    }
    Ok(())
}

const fn reject_separate_columns(count: u64) -> Result<(), TableMetadataError> {
    if 0 == count {
        Ok(())
    } else {
        Err(TableMetadataError::UnsupportedSeparateColumnSchemas { actual: count })
    }
}

struct SchemaTableDecodeState {
    entries: Vec<SchemaTableMetadata>,
    indexes: HashMap<i32, usize>,
    record_count: u64,
}

fn decode_schema_tables<R: Read>(
    reader: &mut R,
    schema_map: &SchemaMap,
    packed_streams: &[PackedStreamMetadata],
    minimum_section_size: u64,
    limits: TableMetadataLimits,
) -> Result<SchemaTableDecodeState, TableMetadataError> {
    let schema_table_count = read_u64(reader)?;
    check_count_limit(
        TableMetadataResource::SchemaTables,
        schema_table_count,
        limits.schema_tables,
    )?;
    let decompressed_size = schema_table_count
        .checked_mul(SCHEMA_TABLE_ENTRY_SIZE)
        .and_then(|size| size.checked_add(minimum_section_size))
        .ok_or(TableMetadataError::SizeOverflow)?;
    check_decompressed_size(decompressed_size, limits)?;

    let expected_schema_count =
        u64::try_from(schema_map.len()).map_err(|_| TableMetadataError::SizeOverflow)?;
    if schema_table_count != expected_schema_count {
        return Err(TableMetadataError::SchemaTableCountMismatch {
            actual: schema_table_count,
            expected: expected_schema_count,
        });
    }

    let table_capacity =
        usize::try_from(schema_table_count).map_err(|_| TableMetadataError::SizeOverflow)?;
    let mut entries = Vec::new();
    entries.try_reserve_exact(table_capacity).map_err(|_| {
        TableMetadataError::AllocationFailed {
            requested: table_capacity,
        }
    })?;
    let mut table_indexes = HashMap::new();
    table_indexes.try_reserve(table_capacity).map_err(|_| {
        TableMetadataError::AllocationFailed {
            requested: table_capacity,
        }
    })?;
    let mut state = SchemaTableDecodeState {
        entries,
        indexes: table_indexes,
        record_count: 0,
    };

    for table_index in 0..table_capacity {
        decode_schema_table(
            reader,
            schema_map,
            packed_streams,
            &mut state,
            table_index,
            limits,
        )?;
    }
    finish_schema_tables(&mut state.entries, packed_streams)?;

    Ok(state)
}

fn decode_schema_table<R: Read>(
    reader: &mut R,
    schema_map: &SchemaMap,
    packed_streams: &[PackedStreamMetadata],
    state: &mut SchemaTableDecodeState,
    table_index: usize,
    limits: TableMetadataLimits,
) -> Result<(), TableMetadataError> {
    let stream_id = read_u64(reader)?;
    let stream_offset = read_u64(reader)?;
    let schema_id = read_i32(reader)?;
    let message_count = read_u64(reader)?;
    let packed_stream_count =
        u64::try_from(packed_streams.len()).map_err(|_| TableMetadataError::SizeOverflow)?;
    let stream_index =
        usize::try_from(stream_id).map_err(|_| TableMetadataError::PackedStreamIdOutOfBounds {
            table_index,
            stream_id,
            packed_stream_count,
        })?;
    let stream =
        packed_streams
            .get(stream_index)
            .ok_or(TableMetadataError::PackedStreamIdOutOfBounds {
                table_index,
                stream_id,
                packed_stream_count,
            })?;
    if stream_offset > stream.uncompressed_size {
        return Err(TableMetadataError::TableOffsetOutOfBounds {
            table_index,
            stream_id,
            offset: stream_offset,
            stream_size: stream.uncompressed_size,
        });
    }
    validate_table_order(
        &mut state.entries,
        packed_streams,
        table_index,
        stream_id,
        stream_offset,
    )?;
    validate_schema_id(schema_map, &mut state.indexes, table_index, schema_id)?;
    update_record_count(&mut state.record_count, table_index, message_count, limits)?;

    state.entries.push(SchemaTableMetadata {
        stream_id,
        stream_offset,
        uncompressed_size: 0,
        schema_id,
        message_count,
    });
    Ok(())
}

fn validate_schema_id(
    schema_map: &SchemaMap,
    table_indexes: &mut HashMap<i32, usize>,
    table_index: usize,
    schema_id: i32,
) -> Result<(), TableMetadataError> {
    if schema_map.get(schema_id).is_none() {
        return Err(TableMetadataError::UnknownSchemaId {
            table_index,
            schema_id,
        });
    }
    if let Some(previous_table_index) = table_indexes.insert(schema_id, table_index) {
        return Err(TableMetadataError::DuplicateSchemaId {
            table_index,
            previous_table_index,
            schema_id,
        });
    }
    Ok(())
}

fn update_record_count(
    record_count: &mut u64,
    table_index: usize,
    message_count: u64,
    limits: TableMetadataLimits,
) -> Result<(), TableMetadataError> {
    if message_count > limits.messages_per_table {
        return Err(TableMetadataError::MessagesPerTableTooLarge {
            table_index,
            actual: message_count,
            limit: limits.messages_per_table,
        });
    }
    *record_count = record_count
        .checked_add(message_count)
        .ok_or(TableMetadataError::SizeOverflow)?;
    if *record_count > limits.total_messages {
        return Err(TableMetadataError::TotalMessagesTooLarge {
            actual: *record_count,
            limit: limits.total_messages,
        });
    }
    Ok(())
}

fn finish_schema_tables(
    schema_tables: &mut [SchemaTableMetadata],
    packed_streams: &[PackedStreamMetadata],
) -> Result<(), TableMetadataError> {
    let Some(final_table) = schema_tables.last_mut() else {
        return if packed_streams.is_empty() {
            Ok(())
        } else {
            Err(TableMetadataError::PackedStreamWithoutTables { stream_index: 0 })
        };
    };
    let final_stream_index =
        usize::try_from(final_table.stream_id).map_err(|_| TableMetadataError::SizeOverflow)?;
    if final_stream_index + 1 != packed_streams.len() {
        return Err(TableMetadataError::PackedStreamWithoutTables {
            stream_index: final_stream_index + 1,
        });
    }
    final_table.uncompressed_size = packed_streams[final_stream_index]
        .uncompressed_size
        .checked_sub(final_table.stream_offset)
        .ok_or(TableMetadataError::SizeOverflow)?;
    Ok(())
}

fn infer_and_validate_compressed_stream_sizes(
    streams: &mut [PackedStreamMetadata],
    tables_compressed_size: u64,
) -> Result<(), TableMetadataError> {
    for stream_index in 0..streams.len() {
        let end = streams
            .get(stream_index + 1)
            .map_or(tables_compressed_size, |stream| stream.file_offset);
        let stream = &mut streams[stream_index];
        stream.compressed_size = end
            .checked_sub(stream.file_offset)
            .ok_or(TableMetadataError::SizeOverflow)?;
        match (stream.uncompressed_size, stream.compressed_size) {
            (0, compressed_size) if 0 != compressed_size => {
                return Err(TableMetadataError::EmptyStreamHasCompressedData {
                    stream_index,
                    compressed_size,
                });
            }
            (uncompressed_size, 0) if 0 != uncompressed_size => {
                return Err(TableMetadataError::NonEmptyStreamHasNoCompressedData {
                    stream_index,
                    uncompressed_size,
                });
            }
            _ => {}
        }
    }
    Ok(())
}

fn validate_table_order(
    tables: &mut [SchemaTableMetadata],
    packed_streams: &[PackedStreamMetadata],
    table_index: usize,
    stream_id: u64,
    stream_offset: u64,
) -> Result<(), TableMetadataError> {
    let Some(previous) = tables.last_mut() else {
        if 0 != stream_id {
            return Err(TableMetadataError::InvalidFirstSchemaTableStreamId { actual: stream_id });
        }
        if 0 != stream_offset {
            return Err(TableMetadataError::InvalidFirstTableOffset {
                table_index,
                stream_id,
                actual: stream_offset,
            });
        }
        return Ok(());
    };

    if stream_id == previous.stream_id {
        if stream_offset < previous.stream_offset {
            return Err(TableMetadataError::NonMonotonicTableOffset {
                table_index,
                stream_id,
                previous: previous.stream_offset,
                actual: stream_offset,
            });
        }
        previous.uncompressed_size = stream_offset
            .checked_sub(previous.stream_offset)
            .ok_or(TableMetadataError::SizeOverflow)?;
        return Ok(());
    }

    let expected = previous
        .stream_id
        .checked_add(1)
        .ok_or(TableMetadataError::SizeOverflow)?;
    if stream_id != expected {
        return Err(TableMetadataError::NonSequentialSchemaTableStreamId {
            table_index,
            previous: previous.stream_id,
            actual: stream_id,
        });
    }
    if 0 != stream_offset {
        return Err(TableMetadataError::InvalidFirstTableOffset {
            table_index,
            stream_id,
            actual: stream_offset,
        });
    }
    let previous_stream_index =
        usize::try_from(previous.stream_id).map_err(|_| TableMetadataError::SizeOverflow)?;
    previous.uncompressed_size = packed_streams[previous_stream_index]
        .uncompressed_size
        .checked_sub(previous.stream_offset)
        .ok_or(TableMetadataError::SizeOverflow)?;
    Ok(())
}

const fn check_count_limit(
    resource: TableMetadataResource,
    actual: u64,
    limit: u64,
) -> Result<(), TableMetadataError> {
    if actual > limit {
        Err(TableMetadataError::CountTooLarge {
            resource,
            actual,
            limit,
        })
    } else {
        Ok(())
    }
}

const fn check_decompressed_size(
    actual: u64,
    limits: TableMetadataLimits,
) -> Result<(), TableMetadataError> {
    if actual > limits.decompressed {
        Err(TableMetadataError::DecompressedSectionTooLarge {
            actual,
            limit: limits.decompressed,
        })
    } else {
        Ok(())
    }
}

fn read_i32<R: Read>(reader: &mut R) -> Result<i32, TableMetadataError> {
    let mut bytes = [0_u8; 4];
    reader
        .read_exact(&mut bytes)
        .map_err(TableMetadataError::Io)?;
    Ok(i32::from_le_bytes(bytes))
}

fn read_u64<R: Read>(reader: &mut R) -> Result<u64, TableMetadataError> {
    let mut bytes = [0_u8; 8];
    reader
        .read_exact(&mut bytes)
        .map_err(TableMetadataError::Io)?;
    Ok(u64::from_le_bytes(bytes))
}

/// Counted collection constrained by [`TableMetadataLimits`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum TableMetadataResource {
    /// Packed zstd streams in `/0`.
    PackedStreams,
    /// Schema tables distributed across packed streams.
    SchemaTables,
}

/// Failure to decompress or validate a table-metadata section.
#[derive(Debug)]
#[non_exhaustive]
pub enum TableMetadataError {
    /// The compressed section exceeds the configured limit.
    CompressedSectionTooLarge { actual: u64, limit: u64 },
    /// The structurally implied decompressed size exceeds the configured limit.
    DecompressedSectionTooLarge { actual: u64, limit: u64 },
    /// An archive-provided collection count exceeds its configured limit.
    CountTooLarge {
        resource: TableMetadataResource,
        actual: u64,
        limit: u64,
    },
    /// A packed stream advertises too many decompressed bytes.
    UncompressedStreamTooLarge {
        stream_index: usize,
        actual: u64,
        limit: u64,
    },
    /// The sum of advertised packed-stream sizes exceeds its configured limit.
    TotalUncompressedStreamsTooLarge { actual: u64, limit: u64 },
    /// One schema table advertises too many messages.
    MessagesPerTableTooLarge {
        table_index: usize,
        actual: u64,
        limit: u64,
    },
    /// The sum of schema-table message counts exceeds its configured limit.
    TotalMessagesTooLarge { actual: u64, limit: u64 },
    /// Input or decompression failed.
    Io(io::Error),
    /// Checked size arithmetic or conversion overflowed.
    SizeOverflow,
    /// A bounded allocation could not be reserved.
    AllocationFailed { requested: usize },
    /// `/0` contains bytes despite table metadata declaring no packed streams.
    EmptyPackedStreamList,
    /// The reserved separate-column facility is not supported by v0.5.
    UnsupportedSeparateColumnSchemas { actual: u64 },
    /// No schema table was available where a table was required.
    EmptySchemaTableList,
    /// The first packed stream did not begin at offset zero in `/0`.
    InvalidFirstPackedStreamOffset { actual: u64 },
    /// A packed-stream file offset exceeded the compressed `/0` size.
    PackedStreamOffsetOutOfBounds {
        stream_index: usize,
        offset: u64,
        tables_compressed_size: u64,
    },
    /// Packed-stream file offsets moved backwards.
    NonMonotonicPackedStreamOffset {
        stream_index: usize,
        previous: u64,
        actual: u64,
    },
    /// A zero-byte logical stream unexpectedly owns physical bytes.
    EmptyStreamHasCompressedData {
        stream_index: usize,
        compressed_size: u64,
    },
    /// A nonempty logical stream owns no physical bytes.
    NonEmptyStreamHasNoCompressedData {
        stream_index: usize,
        uncompressed_size: u64,
    },
    /// Table and schema-map counts disagree.
    SchemaTableCountMismatch { actual: u64, expected: u64 },
    /// A schema table referenced a packed-stream ID outside the stream list.
    PackedStreamIdOutOfBounds {
        table_index: usize,
        stream_id: u64,
        packed_stream_count: u64,
    },
    /// The first schema table was not assigned to stream zero.
    InvalidFirstSchemaTableStreamId { actual: u64 },
    /// Schema-table stream IDs were not contiguous and ordered.
    NonSequentialSchemaTableStreamId {
        table_index: usize,
        previous: u64,
        actual: u64,
    },
    /// The first schema table in a stream did not begin at offset zero.
    InvalidFirstTableOffset {
        table_index: usize,
        stream_id: u64,
        actual: u64,
    },
    /// Table offsets moved backwards within one decompressed stream.
    NonMonotonicTableOffset {
        table_index: usize,
        stream_id: u64,
        previous: u64,
        actual: u64,
    },
    /// A table offset exceeded its advertised decompressed stream size.
    TableOffsetOutOfBounds {
        table_index: usize,
        stream_id: u64,
        offset: u64,
        stream_size: u64,
    },
    /// A table referenced a schema absent from the schema map.
    UnknownSchemaId { table_index: usize, schema_id: i32 },
    /// A schema ID appeared in more than one table record.
    DuplicateSchemaId {
        table_index: usize,
        previous_table_index: usize,
        schema_id: i32,
    },
    /// At least one declared packed stream had no schema table.
    PackedStreamWithoutTables { stream_index: usize },
    /// Decompressed bytes followed the declared table metadata.
    TrailingDecompressedData,
    /// Compressed bytes followed the one table-metadata zstd frame.
    TrailingCompressedData { remaining: u64 },
    /// The supplied archive metadata did not contain `/table_metadata`.
    MissingSection,
    /// The supplied archive metadata did not contain `/0`.
    MissingTablesSection,
    /// `/table_metadata` was outside this archive's files region.
    SectionOutsideArchive,
    /// `/0` was outside this archive's files region.
    TablesSectionOutsideArchive,
}

impl Display for TableMetadataError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::CompressedSectionTooLarge { .. }
            | Self::DecompressedSectionTooLarge { .. }
            | Self::CountTooLarge { .. }
            | Self::UncompressedStreamTooLarge { .. }
            | Self::TotalUncompressedStreamsTooLarge { .. }
            | Self::MessagesPerTableTooLarge { .. }
            | Self::TotalMessagesTooLarge { .. }
            | Self::Io(_)
            | Self::SizeOverflow
            | Self::AllocationFailed { .. } => format_resource_error(self, formatter),
            Self::EmptyPackedStreamList
            | Self::InvalidFirstPackedStreamOffset { .. }
            | Self::PackedStreamOffsetOutOfBounds { .. }
            | Self::NonMonotonicPackedStreamOffset { .. }
            | Self::EmptyStreamHasCompressedData { .. }
            | Self::NonEmptyStreamHasNoCompressedData { .. } => {
                format_packed_stream_error(self, formatter)
            }
            Self::EmptySchemaTableList
            | Self::SchemaTableCountMismatch { .. }
            | Self::PackedStreamIdOutOfBounds { .. }
            | Self::InvalidFirstSchemaTableStreamId { .. }
            | Self::NonSequentialSchemaTableStreamId { .. }
            | Self::InvalidFirstTableOffset { .. }
            | Self::NonMonotonicTableOffset { .. }
            | Self::TableOffsetOutOfBounds { .. }
            | Self::UnknownSchemaId { .. }
            | Self::DuplicateSchemaId { .. }
            | Self::PackedStreamWithoutTables { .. } => format_schema_table_error(self, formatter),
            Self::UnsupportedSeparateColumnSchemas { .. }
            | Self::TrailingDecompressedData
            | Self::TrailingCompressedData { .. }
            | Self::MissingSection
            | Self::MissingTablesSection
            | Self::SectionOutsideArchive
            | Self::TablesSectionOutsideArchive => format_section_error(self, formatter),
        }
    }
}

fn format_resource_error(error: &TableMetadataError, formatter: &mut Formatter<'_>) -> fmt::Result {
    match error {
        TableMetadataError::CompressedSectionTooLarge { actual, limit } => write!(
            formatter,
            "compressed table-metadata size {actual} exceeds limit {limit}"
        ),
        TableMetadataError::DecompressedSectionTooLarge { actual, limit } => write!(
            formatter,
            "decompressed table-metadata size {actual} exceeds limit {limit}"
        ),
        TableMetadataError::CountTooLarge {
            resource,
            actual,
            limit,
        } => write!(
            formatter,
            "table-metadata {resource:?} count {actual} exceeds limit {limit}"
        ),
        TableMetadataError::UncompressedStreamTooLarge {
            stream_index,
            actual,
            limit,
        } => write!(
            formatter,
            "packed stream {stream_index} decompressed size {actual} exceeds limit {limit}"
        ),
        TableMetadataError::TotalUncompressedStreamsTooLarge { actual, limit } => write!(
            formatter,
            "total packed-stream decompressed size {actual} exceeds limit {limit}"
        ),
        TableMetadataError::MessagesPerTableTooLarge {
            table_index,
            actual,
            limit,
        } => write!(
            formatter,
            "schema table {table_index} message count {actual} exceeds limit {limit}"
        ),
        TableMetadataError::TotalMessagesTooLarge { actual, limit } => write!(
            formatter,
            "total schema-table message count {actual} exceeds limit {limit}"
        ),
        TableMetadataError::Io(error) => {
            write!(formatter, "table-metadata I/O failed: {error}")
        }
        TableMetadataError::SizeOverflow => formatter.write_str("table-metadata size overflow"),
        TableMetadataError::AllocationFailed { requested } => write!(
            formatter,
            "could not reserve bounded table-metadata allocation of {requested} elements"
        ),
        _ => unreachable!("resource formatter called with a structural table-metadata error"),
    }
}

fn format_packed_stream_error(
    error: &TableMetadataError,
    formatter: &mut Formatter<'_>,
) -> fmt::Result {
    match error {
        TableMetadataError::EmptyPackedStreamList => {
            formatter.write_str("table metadata contains no packed streams for nonempty /0 data")
        }
        TableMetadataError::InvalidFirstPackedStreamOffset { actual } => write!(
            formatter,
            "first packed stream begins at /0 offset {actual}, not zero"
        ),
        TableMetadataError::PackedStreamOffsetOutOfBounds {
            stream_index,
            offset,
            tables_compressed_size,
        } => write!(
            formatter,
            "packed stream {stream_index} offset {offset} exceeds /0 size {tables_compressed_size}"
        ),
        TableMetadataError::NonMonotonicPackedStreamOffset {
            stream_index,
            previous,
            actual,
        } => write!(
            formatter,
            "packed stream {stream_index} offset {actual} precedes previous offset {previous}"
        ),
        TableMetadataError::EmptyStreamHasCompressedData {
            stream_index,
            compressed_size,
        } => write!(
            formatter,
            "empty packed stream {stream_index} owns {compressed_size} compressed bytes"
        ),
        TableMetadataError::NonEmptyStreamHasNoCompressedData {
            stream_index,
            uncompressed_size,
        } => write!(
            formatter,
            "packed stream {stream_index} advertises {uncompressed_size} decompressed bytes but \
             owns no compressed bytes"
        ),
        _ => unreachable!("packed-stream formatter called with another table-metadata error"),
    }
}

fn format_schema_table_error(
    error: &TableMetadataError,
    formatter: &mut Formatter<'_>,
) -> fmt::Result {
    match error {
        TableMetadataError::EmptySchemaTableList => {
            formatter.write_str("table metadata contains no schema tables")
        }
        TableMetadataError::SchemaTableCountMismatch { actual, expected } => write!(
            formatter,
            "table metadata has {actual} schema tables but the schema map has {expected}"
        ),
        TableMetadataError::PackedStreamIdOutOfBounds {
            table_index,
            stream_id,
            packed_stream_count,
        } => write!(
            formatter,
            "schema table {table_index} references stream {stream_id}, but there are \
             {packed_stream_count} streams"
        ),
        TableMetadataError::InvalidFirstSchemaTableStreamId { actual } => write!(
            formatter,
            "first schema table references packed stream {actual}, not zero"
        ),
        TableMetadataError::NonSequentialSchemaTableStreamId {
            table_index,
            previous,
            actual,
        } => write!(
            formatter,
            "schema table {table_index} moves from packed stream {previous} to {actual}"
        ),
        TableMetadataError::InvalidFirstTableOffset {
            table_index,
            stream_id,
            actual,
        } => write!(
            formatter,
            "first schema table {table_index} in stream {stream_id} begins at {actual}, not zero"
        ),
        TableMetadataError::NonMonotonicTableOffset {
            table_index,
            stream_id,
            previous,
            actual,
        } => write!(
            formatter,
            "schema table {table_index} offset {actual} in stream {stream_id} precedes previous \
             offset {previous}"
        ),
        TableMetadataError::TableOffsetOutOfBounds {
            table_index,
            stream_id,
            offset,
            stream_size,
        } => write!(
            formatter,
            "schema table {table_index} offset {offset} exceeds stream {stream_id} size \
             {stream_size}"
        ),
        TableMetadataError::UnknownSchemaId {
            table_index,
            schema_id,
        } => write!(
            formatter,
            "schema table {table_index} references unknown schema ID {schema_id}"
        ),
        TableMetadataError::DuplicateSchemaId {
            table_index,
            previous_table_index,
            schema_id,
        } => write!(
            formatter,
            "schema table {table_index} repeats schema ID {schema_id} from table \
             {previous_table_index}"
        ),
        TableMetadataError::PackedStreamWithoutTables { stream_index } => write!(
            formatter,
            "packed stream {stream_index} has no schema-table metadata"
        ),
        _ => unreachable!("schema-table formatter called with another table-metadata error"),
    }
}

fn format_section_error(error: &TableMetadataError, formatter: &mut Formatter<'_>) -> fmt::Result {
    match error {
        TableMetadataError::UnsupportedSeparateColumnSchemas { actual } => write!(
            formatter,
            "table metadata declares {actual} unsupported separate-column schemas"
        ),
        TableMetadataError::TrailingDecompressedData => {
            formatter.write_str("data follows the declared table-metadata entries")
        }
        TableMetadataError::TrailingCompressedData { remaining } => write!(
            formatter,
            "{remaining} compressed bytes follow the table-metadata zstd frame"
        ),
        TableMetadataError::MissingSection => {
            formatter.write_str("archive metadata has no table-metadata section")
        }
        TableMetadataError::MissingTablesSection => {
            formatter.write_str("archive metadata has no packed-tables section")
        }
        TableMetadataError::SectionOutsideArchive => {
            formatter.write_str("table-metadata section is outside the archive files region")
        }
        TableMetadataError::TablesSectionOutsideArchive => {
            formatter.write_str("packed-tables section is outside the archive files region")
        }
        _ => unreachable!("section formatter called with another table-metadata error"),
    }
}

impl Error for TableMetadataError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::archive::schema_map::SchemaMapLimits;
    use crate::archive::schema_map::decode_schema_map;
    use crate::archive::schema_tree::SchemaTreeLimits;
    use crate::archive::schema_tree::decode_schema_tree;

    type Stream = (u64, u64);
    type Table = (u64, u64, i32, u64);

    fn schema_map(ids: &[i32]) -> SchemaMap {
        let tree_raw = 0_u64.to_le_bytes();
        let tree_compressed =
            zstd::stream::encode_all(tree_raw.as_slice(), 3).expect("compress empty schema tree");
        let tree_size = u64::try_from(tree_compressed.len()).expect("tree size fits u64");
        let mut tree_source = Cursor::new(tree_compressed);
        let tree = decode_schema_tree(
            tree_source.by_ref().take(tree_size),
            SchemaTreeLimits::default(),
        )
        .expect("decode empty schema tree");

        let mut map_raw = u64::try_from(ids.len())
            .expect("schema count fits u64")
            .to_le_bytes()
            .to_vec();
        for id in ids {
            map_raw.extend_from_slice(&id.to_le_bytes());
            map_raw.extend_from_slice(&0_u32.to_le_bytes());
            map_raw.extend_from_slice(&0_u32.to_le_bytes());
        }
        let map_compressed =
            zstd::stream::encode_all(map_raw.as_slice(), 3).expect("compress schema map");
        let map_size = u64::try_from(map_compressed.len()).expect("map size fits u64");
        let mut map_source = Cursor::new(map_compressed);
        decode_schema_map(
            map_source.by_ref().take(map_size),
            &tree,
            SchemaMapLimits::default(),
        )
        .expect("decode schema map")
    }

    fn raw_section(streams: &[Stream], separate: u64, tables: &[Table]) -> Vec<u8> {
        let mut raw = u64::try_from(streams.len())
            .expect("stream count fits u64")
            .to_le_bytes()
            .to_vec();
        for &(offset, size) in streams {
            raw.extend_from_slice(&offset.to_le_bytes());
            raw.extend_from_slice(&size.to_le_bytes());
        }
        raw.extend_from_slice(&separate.to_le_bytes());
        raw.extend_from_slice(
            &u64::try_from(tables.len())
                .expect("table count fits u64")
                .to_le_bytes(),
        );
        for &(stream_id, offset, schema_id, messages) in tables {
            raw.extend_from_slice(&stream_id.to_le_bytes());
            raw.extend_from_slice(&offset.to_le_bytes());
            raw.extend_from_slice(&schema_id.to_le_bytes());
            raw.extend_from_slice(&messages.to_le_bytes());
        }
        raw
    }

    fn compressed_section(streams: &[Stream], separate: u64, tables: &[Table]) -> Vec<u8> {
        zstd::stream::encode_all(raw_section(streams, separate, tables).as_slice(), 3)
            .expect("compress table metadata")
    }

    fn decode(
        compressed: &[u8],
        schemas: &[i32],
        tables_compressed_size: u64,
    ) -> Result<TableMetadata, TableMetadataError> {
        let schema_map = schema_map(schemas);
        let compressed_size =
            u64::try_from(compressed.len()).expect("compressed section size fits u64");
        let mut source = Cursor::new(compressed);
        decode_table_metadata(
            source.by_ref().take(compressed_size),
            &schema_map,
            tables_compressed_size,
            TableMetadataLimits::default(),
        )
    }

    #[test]
    fn decodes_streams_tables_and_inferred_sizes() {
        let compressed = compressed_section(
            &[(0, 20), (12, 0)],
            0,
            &[(0, 0, 7, 2), (0, 8, -3, 4), (1, 0, 12, 1)],
        );

        let metadata = decode(&compressed, &[7, -3, 12], 12).expect("valid table metadata");

        assert_eq!(7, metadata.record_count());
        assert_eq!(20, metadata.total_uncompressed_stream_size());
        assert_eq!(2, metadata.packed_streams().len());
        assert_eq!(
            0..12,
            metadata
                .packed_stream(0)
                .expect("stream zero")
                .compressed_range()
        );
        assert_eq!(
            12..12,
            metadata
                .packed_stream(1)
                .expect("stream one")
                .compressed_range()
        );
        let expected_sizes = [8, 12, 0];
        for (table, expected_size) in metadata.schema_tables().iter().zip(expected_sizes) {
            assert_eq!(expected_size, table.uncompressed_size());
            assert_eq!(Some(table), metadata.schema_table(table.schema_id()));
        }
        let first = metadata.schema_table(7).expect("schema seven table");
        assert_eq!(0, first.stream_id());
        assert_eq!(0, first.stream_offset());
        assert_eq!(2, first.message_count());
    }

    #[test]
    fn accepts_the_cxx_all_empty_tuple_and_rejects_incoherent_empty_layouts() {
        let all_empty = compressed_section(&[], 0, &[]);
        let metadata = decode(&all_empty, &[], 0).expect("C++ canonical empty table metadata");
        assert_eq!(0, metadata.packed_streams().len());
        assert_eq!(0, metadata.schema_tables().len());
        assert_eq!(0, metadata.total_uncompressed_stream_size());
        assert_eq!(0, metadata.record_count());

        let nonempty_tables_section = compressed_section(&[], 0, &[]);
        assert!(matches!(
            decode(&nonempty_tables_section, &[], 1),
            Err(TableMetadataError::EmptyPackedStreamList)
        ));

        let stream_without_table = compressed_section(&[(0, 0)], 0, &[]);
        assert!(matches!(
            decode(&stream_without_table, &[], 0),
            Err(TableMetadataError::PackedStreamWithoutTables { stream_index: 0 })
        ));

        let schema_without_table = compressed_section(&[], 0, &[]);
        assert!(matches!(
            decode(&schema_without_table, &[7], 0),
            Err(TableMetadataError::SchemaTableCountMismatch {
                actual: 0,
                expected: 1
            })
        ));
    }

    #[test]
    fn rejects_reserved_separate_columns() {
        let separate = compressed_section(&[(0, 1)], 1, &[(0, 0, 7, 1)]);
        assert!(matches!(
            decode(&separate, &[7], 1),
            Err(TableMetadataError::UnsupportedSeparateColumnSchemas { actual: 1 })
        ));
    }

    #[test]
    fn rejects_invalid_packed_stream_layout() {
        let nonzero_first = compressed_section(&[(1, 1)], 0, &[(0, 0, 7, 1)]);
        assert!(matches!(
            decode(&nonzero_first, &[7], 2),
            Err(TableMetadataError::InvalidFirstPackedStreamOffset { actual: 1 })
        ));

        let backwards = compressed_section(&[(0, 1), (3, 1), (2, 1)], 0, &[(0, 0, 7, 1)]);
        assert!(matches!(
            decode(&backwards, &[7], 4),
            Err(TableMetadataError::NonMonotonicPackedStreamOffset { .. })
        ));

        let empty_with_bytes = compressed_section(&[(0, 0)], 0, &[(0, 0, 7, 1)]);
        assert!(matches!(
            decode(&empty_with_bytes, &[7], 1),
            Err(TableMetadataError::EmptyStreamHasCompressedData { .. })
        ));
    }

    #[test]
    fn rejects_invalid_schema_table_layout() {
        let unknown = compressed_section(&[(0, 10)], 0, &[(0, 0, 8, 1)]);
        assert!(matches!(
            decode(&unknown, &[7], 1),
            Err(TableMetadataError::UnknownSchemaId {
                table_index: 0,
                schema_id: 8
            })
        ));

        let duplicate = compressed_section(&[(0, 10)], 0, &[(0, 0, 7, 1), (0, 1, 7, 1)]);
        assert!(matches!(
            decode(&duplicate, &[7, 8], 1),
            Err(TableMetadataError::DuplicateSchemaId { .. })
        ));

        let backwards =
            compressed_section(&[(0, 10)], 0, &[(0, 0, 7, 1), (0, 5, 8, 1), (0, 4, 9, 1)]);
        assert!(matches!(
            decode(&backwards, &[7, 8, 9], 1),
            Err(TableMetadataError::NonMonotonicTableOffset { .. })
        ));

        let outside_stream = compressed_section(&[(0, 10)], 0, &[(0, 11, 7, 1)]);
        assert!(matches!(
            decode(&outside_stream, &[7], 1),
            Err(TableMetadataError::TableOffsetOutOfBounds { .. })
        ));

        let missing_stream_table = compressed_section(&[(0, 5), (1, 5)], 0, &[(0, 0, 7, 1)]);
        assert!(matches!(
            decode(&missing_stream_table, &[7], 2),
            Err(TableMetadataError::PackedStreamWithoutTables { stream_index: 1 })
        ));
    }

    #[test]
    fn enforces_resource_limits_before_large_allocations() {
        let compressed = compressed_section(&[(0, 10)], 0, &[(0, 0, 7, 11)]);
        let single_schema_map = schema_map(&[7]);
        let compressed_size = u64::try_from(compressed.len()).expect("section size fits u64");
        let mut source = Cursor::new(compressed);
        let limits = TableMetadataLimits::new(compressed_size, 1_000, 1, 1)
            .with_uncompressed_stream_limits(10, 10)
            .with_message_limits(10, 10);

        assert!(matches!(
            decode_table_metadata(
                source.by_ref().take(compressed_size),
                &single_schema_map,
                1,
                limits
            ),
            Err(TableMetadataError::MessagesPerTableTooLarge {
                table_index: 0,
                actual: 11,
                limit: 10
            })
        ));

        let two_streams = compressed_section(&[(0, 1), (1, 1)], 0, &[(0, 0, 7, 1), (1, 0, 8, 1)]);
        let two_schema_map = schema_map(&[7, 8]);
        let compressed_size = u64::try_from(two_streams.len()).expect("section size fits u64");
        let mut source = Cursor::new(two_streams);
        let limits = TableMetadataLimits::new(compressed_size, 1_000, 1, 2);
        assert!(matches!(
            decode_table_metadata(
                source.by_ref().take(compressed_size),
                &two_schema_map,
                2,
                limits
            ),
            Err(TableMetadataError::CountTooLarge {
                resource: TableMetadataResource::PackedStreams,
                actual: 2,
                limit: 1
            })
        ));
    }

    #[test]
    fn rejects_trailing_decompressed_and_compressed_data() {
        let mut raw = raw_section(&[(0, 1)], 0, &[(0, 0, 7, 1)]);
        raw.push(0);
        let trailing_raw =
            zstd::stream::encode_all(raw.as_slice(), 3).expect("compress trailing raw byte");
        assert!(matches!(
            decode(&trailing_raw, &[7], 1),
            Err(TableMetadataError::TrailingDecompressedData)
        ));

        let mut trailing_frame = compressed_section(&[(0, 1)], 0, &[(0, 0, 7, 1)]);
        trailing_frame.extend_from_slice(
            &zstd::stream::encode_all([0_u8].as_slice(), 3).expect("compress second frame"),
        );
        assert!(matches!(
            decode(&trailing_frame, &[7], 1),
            Err(TableMetadataError::TrailingCompressedData { .. })
        ));
    }
}
