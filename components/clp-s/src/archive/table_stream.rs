use std::error::Error;
use std::fmt;
use std::fmt::Display;
use std::fmt::Formatter;
use std::iter::FusedIterator;
use std::ops::Range;

use super::column::ColumnError;
use super::column::ColumnLimits;
use super::column::SchemaTable;
use super::column::decode_schema_table;
use super::packed_stream::ColumnSlot;
use super::dictionary::ArrayDictionary;
use super::dictionary::LogTypeDictionary;
use super::dictionary::VariableDictionary;
use super::schema_map::SchemaDefinition;
use super::schema_map::SchemaMap;
use super::schema_tree::SchemaTree;
use super::table_metadata::SchemaTableMetadata;
use super::table_metadata::TableMetadata;
use super::timestamp_dictionary::TimestampDictionary;

/// One lazily decoded schema table and its validated physical metadata.
#[derive(Clone, Debug)]
pub struct DecodedSchemaTable<'stream, 'archive> {
    table_index: usize,
    metadata: &'archive SchemaTableMetadata,
    schema: &'archive SchemaDefinition,
    table: SchemaTable<'stream, 'archive>,
}

impl<'stream, 'archive> DecodedSchemaTable<'stream, 'archive> {
    /// Returns the table's global index in physical archive order.
    #[must_use]
    pub const fn table_index(&self) -> usize {
        self.table_index
    }

    /// Returns the table's validated packed-stream coordinates and schema identity.
    #[must_use]
    pub const fn metadata(&self) -> &'archive SchemaTableMetadata {
        self.metadata
    }

    /// Returns the schema definition used to decode this table.
    #[must_use]
    pub const fn schema(&self) -> &'archive SchemaDefinition {
        self.schema
    }

    /// Returns the decoded zero-copy table view.
    #[must_use]
    pub const fn table(&self) -> &SchemaTable<'stream, 'archive> {
        &self.table
    }

    /// Consumes this entry and returns its decoded zero-copy table view.
    #[must_use]
    pub fn into_table(self) -> SchemaTable<'stream, 'archive> {
        self.table
    }
}

/// Lazy zero-copy decoder for every schema table in one decompressed packed stream.
///
/// Construction validates the stream identity, exact advertised byte length, and complete
/// metadata span coverage without decoding a table. Each call to [`Iterator::next`] allocates and
/// validates only that table's column-view vector, so dropping the returned item releases all
/// per-table decoder state before the next table is requested. Table values continue to borrow the
/// caller-owned `stream_bytes`.
#[derive(Clone, Debug)]
pub struct SchemaTableStream<'stream, 'archive> {
    stream_id: u64,
    stream_bytes: &'stream [u8],
    /// Per-column load state of a projected separate-column stream, if any.
    column_layout: Option<&'stream [ColumnSlot]>,
    tables: &'archive [SchemaTableMetadata],
    first_table_index: usize,
    next_table_index: usize,
    schema_map: &'archive SchemaMap,
    schema_tree: &'archive SchemaTree,
    variable_dictionary: &'archive VariableDictionary,
    logtype_dictionary: &'archive LogTypeDictionary,
    array_dictionary: &'archive ArrayDictionary,
    timestamp_dictionary: &'archive TimestampDictionary,
    limits: ColumnLimits,
}

impl<'stream, 'archive> SchemaTableStream<'stream, 'archive> {
    /// Selects and validates the tables belonging to one decompressed packed stream.
    ///
    /// The bytes must be the exact output for `stream_id` described by `table_metadata`; callers
    /// normally obtain them from `SingleFileArchiveReader::read_packed_stream` and keep that
    /// buffer alive while iterating.
    ///
    /// # Errors
    ///
    /// Returns an error when the stream ID is absent, its byte length disagrees with metadata, or
    /// its validated table entries do not form checked, contiguous spans covering the stream.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        stream_id: u64,
        stream_bytes: &'stream [u8],
        column_layout: Option<&'stream [ColumnSlot]>,
        table_metadata: &'archive TableMetadata,
        schema_map: &'archive SchemaMap,
        schema_tree: &'archive SchemaTree,
        variable_dictionary: &'archive VariableDictionary,
        logtype_dictionary: &'archive LogTypeDictionary,
        array_dictionary: &'archive ArrayDictionary,
        timestamp_dictionary: &'archive TimestampDictionary,
        limits: ColumnLimits,
    ) -> Result<Self, TableStreamError> {
        let stream_count = table_metadata.packed_streams().len();
        let stream_index =
            usize::try_from(stream_id).map_err(|_| TableStreamError::StreamIdOutOfBounds {
                stream_id,
                stream_count,
            })?;
        let stream = table_metadata.packed_stream(stream_index).ok_or(
            TableStreamError::StreamIdOutOfBounds {
                stream_id,
                stream_count,
            },
        )?;

        let actual_size =
            u64::try_from(stream_bytes.len()).map_err(|_| TableStreamError::SizeOverflow)?;
        if actual_size != stream.uncompressed_size() {
            return Err(TableStreamError::StreamLengthMismatch {
                stream_id,
                advertised: stream.uncompressed_size(),
                actual: actual_size,
            });
        }

        let all_tables = table_metadata.schema_tables();
        let first_table_index = all_tables.partition_point(|table| table.stream_id() < stream_id);
        let table_count =
            all_tables[first_table_index..].partition_point(|table| table.stream_id() == stream_id);
        let table_end = first_table_index
            .checked_add(table_count)
            .ok_or(TableStreamError::SizeOverflow)?;
        let tables = all_tables
            .get(first_table_index..table_end)
            .ok_or(TableStreamError::SizeOverflow)?;
        if tables.is_empty() {
            return Err(TableStreamError::StreamHasNoTables { stream_id });
        }
        validate_table_spans(
            stream_id,
            tables,
            first_table_index,
            stream.uncompressed_size(),
        )?;

        Ok(Self {
            stream_id,
            stream_bytes,
            column_layout,
            tables,
            first_table_index,
            next_table_index: 0,
            schema_map,
            schema_tree,
            variable_dictionary,
            logtype_dictionary,
            array_dictionary,
            timestamp_dictionary,
            limits,
        })
    }

    /// Returns the packed-stream ID represented by this iterator.
    #[must_use]
    pub const fn stream_id(&self) -> u64 {
        self.stream_id
    }

    /// Returns the exact decompressed bytes backing all returned table views.
    #[must_use]
    pub const fn stream_bytes(&self) -> &'stream [u8] {
        self.stream_bytes
    }

    /// Returns metadata for every table in this packed stream.
    #[must_use]
    pub const fn table_metadata(&self) -> &'archive [SchemaTableMetadata] {
        self.tables
    }

    /// Returns the number of tables not yet requested.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.tables.len() - self.next_table_index
    }

    /// Returns whether every table has been requested.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        0 == self.len()
    }

    fn decode_next(
        &self,
        table: &'archive SchemaTableMetadata,
        table_index: usize,
    ) -> Result<DecodedSchemaTable<'stream, 'archive>, TableStreamError> {
        let range = checked_table_range(table, table_index, self.stream_id)?;
        let table_bytes =
            self.stream_bytes
                .get(range)
                .ok_or(TableStreamError::TableRangeOutOfBounds {
                    table_index,
                    stream_id: self.stream_id,
                    start: table.stream_offset(),
                    end: table
                        .stream_offset()
                        .checked_add(table.uncompressed_size())
                        .ok_or(TableStreamError::SizeOverflow)?,
                    stream_size: u64::try_from(self.stream_bytes.len())
                        .map_err(|_| TableStreamError::SizeOverflow)?,
                })?;
        let schema_id = table.schema_id();
        let schema = self
            .schema_map
            .get(schema_id)
            .ok_or(TableStreamError::UnknownSchemaId {
                table_index,
                schema_id,
            })?;
        let decoded = decode_schema_table(
            table_bytes,
            self.column_layout,
            schema,
            self.schema_tree,
            table.message_count(),
            self.variable_dictionary,
            self.logtype_dictionary,
            self.array_dictionary,
            self.timestamp_dictionary,
            self.limits,
        )
        .map_err(|source| TableStreamError::Column {
            table_index,
            schema_id,
            source,
        })?;
        Ok(DecodedSchemaTable {
            table_index,
            metadata: table,
            schema,
            table: decoded,
        })
    }
}

impl SchemaTableStream<'_, '_> {
    /// Positions the stream so the next item is the table at `table_index`.
    ///
    /// Decoding is lazy but sequential, so a consumer resuming a partially read stream would
    /// otherwise decode every table it skips past. Seeking is sound because
    /// [`SchemaTableStream::new`] has already validated that this stream's tables form contiguous,
    /// in-bounds spans covering the whole buffer, so no table's position depends on having decoded
    /// the one before it.
    ///
    /// `table_index` is archive-wide and physical, matching [`DecodedSchemaTable::table_index`].
    /// One past the last table is accepted and leaves the stream exhausted. Returns whether the
    /// index named a valid position; anything else leaves the stream where it was.
    pub const fn seek_to_table(&mut self, table_index: usize) -> bool {
        let Some(relative) = table_index.checked_sub(self.first_table_index) else {
            return false;
        };
        if relative > self.tables.len() {
            return false;
        }
        self.next_table_index = relative;
        true
    }

    /// Returns the archive-wide index of the table this stream would decode next.
    #[must_use]
    pub const fn next_table_index(&self) -> usize {
        self.first_table_index + self.next_table_index
    }
}

impl<'stream, 'archive> Iterator for SchemaTableStream<'stream, 'archive> {
    type Item = Result<DecodedSchemaTable<'stream, 'archive>, TableStreamError>;

    fn next(&mut self) -> Option<Self::Item> {
        let relative_index = self.next_table_index;
        let table = self.tables.get(relative_index)?;
        self.next_table_index += 1;
        let Some(table_index) = self.first_table_index.checked_add(relative_index) else {
            return Some(Err(TableStreamError::SizeOverflow));
        };
        Some(self.decode_next(table, table_index))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.len();
        (remaining, Some(remaining))
    }
}

impl ExactSizeIterator for SchemaTableStream<'_, '_> {
    fn len(&self) -> usize {
        Self::len(self)
    }
}

impl FusedIterator for SchemaTableStream<'_, '_> {}

fn validate_table_spans(
    stream_id: u64,
    tables: &[SchemaTableMetadata],
    first_table_index: usize,
    stream_size: u64,
) -> Result<(), TableStreamError> {
    let mut expected_offset = 0_u64;
    for (relative_index, table) in tables.iter().enumerate() {
        let table_index = first_table_index
            .checked_add(relative_index)
            .ok_or(TableStreamError::SizeOverflow)?;
        if table.stream_id() != stream_id {
            return Err(TableStreamError::UnexpectedTableStream {
                table_index,
                expected: stream_id,
                actual: table.stream_id(),
            });
        }
        if table.stream_offset() != expected_offset {
            return Err(TableStreamError::NonContiguousTable {
                table_index,
                stream_id,
                expected_offset,
                actual_offset: table.stream_offset(),
            });
        }
        let end = table
            .stream_offset()
            .checked_add(table.uncompressed_size())
            .ok_or(TableStreamError::SizeOverflow)?;
        if end > stream_size {
            return Err(TableStreamError::TableRangeOutOfBounds {
                table_index,
                stream_id,
                start: table.stream_offset(),
                end,
                stream_size,
            });
        }
        expected_offset = end;
    }
    if expected_offset != stream_size {
        return Err(TableStreamError::TableCoverageMismatch {
            stream_id,
            covered: expected_offset,
            stream_size,
        });
    }
    Ok(())
}

fn checked_table_range(
    table: &SchemaTableMetadata,
    table_index: usize,
    stream_id: u64,
) -> Result<Range<usize>, TableStreamError> {
    let end = table
        .stream_offset()
        .checked_add(table.uncompressed_size())
        .ok_or(TableStreamError::SizeOverflow)?;
    let start = usize::try_from(table.stream_offset()).map_err(|_| {
        TableStreamError::TableCoordinateOverflow {
            table_index,
            stream_id,
            offset: table.stream_offset(),
            size: table.uncompressed_size(),
        }
    })?;
    let end = usize::try_from(end).map_err(|_| TableStreamError::TableCoordinateOverflow {
        table_index,
        stream_id,
        offset: table.stream_offset(),
        size: table.uncompressed_size(),
    })?;
    Ok(start..end)
}

/// Failure to select or lazily decode schema tables from one packed stream.
#[derive(Debug)]
#[non_exhaustive]
pub enum TableStreamError {
    /// The requested packed-stream ID is absent from table metadata.
    StreamIdOutOfBounds {
        /// Requested ID.
        stream_id: u64,
        /// Number of advertised streams.
        stream_count: usize,
    },
    /// The supplied decompressed bytes disagree with the stream's advertised size.
    StreamLengthMismatch {
        /// Requested stream.
        stream_id: u64,
        /// Advertised decompressed bytes.
        advertised: u64,
        /// Supplied bytes.
        actual: u64,
    },
    /// No table-metadata entry belongs to an advertised packed stream.
    StreamHasNoTables {
        /// Stream without a table.
        stream_id: u64,
    },
    /// A selected table unexpectedly names a different packed stream.
    UnexpectedTableStream {
        /// Global table-metadata index.
        table_index: usize,
        /// Requested stream ID.
        expected: u64,
        /// Table's stream ID.
        actual: u64,
    },
    /// Table spans do not form the canonical contiguous layout for a stream.
    NonContiguousTable {
        /// Global table-metadata index.
        table_index: usize,
        /// Requested stream ID.
        stream_id: u64,
        /// Required offset after the preceding table.
        expected_offset: u64,
        /// Table's advertised offset.
        actual_offset: u64,
    },
    /// A checked table span extends beyond the supplied stream bytes.
    TableRangeOutOfBounds {
        /// Global table-metadata index.
        table_index: usize,
        /// Requested stream ID.
        stream_id: u64,
        /// Inclusive start offset.
        start: u64,
        /// Exclusive end offset.
        end: u64,
        /// Supplied stream byte length.
        stream_size: u64,
    },
    /// Table spans do not consume the exact advertised stream length.
    TableCoverageMismatch {
        /// Requested stream ID.
        stream_id: u64,
        /// Exclusive end of the final table.
        covered: u64,
        /// Advertised stream length.
        stream_size: u64,
    },
    /// A table's 64-bit coordinates cannot address this platform's byte slice.
    TableCoordinateOverflow {
        /// Global table-metadata index.
        table_index: usize,
        /// Requested stream ID.
        stream_id: u64,
        /// Table start offset.
        offset: u64,
        /// Table byte length.
        size: u64,
    },
    /// The supplied schema map has no definition for a table's validated schema ID.
    UnknownSchemaId {
        /// Global table-metadata index.
        table_index: usize,
        /// Missing opaque schema ID.
        schema_id: i32,
    },
    /// A selected table failed typed column validation.
    Column {
        /// Global table-metadata index.
        table_index: usize,
        /// Table's opaque schema ID.
        schema_id: i32,
        /// Column-layer failure.
        source: ColumnError,
    },
    /// Checked size arithmetic or conversion overflowed.
    SizeOverflow,
}

impl Display for TableStreamError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::StreamIdOutOfBounds {
                stream_id,
                stream_count,
            } => write!(
                formatter,
                "packed stream ID {stream_id} is outside {stream_count} streams"
            ),
            Self::StreamLengthMismatch {
                stream_id,
                advertised,
                actual,
            } => write!(
                formatter,
                "packed stream {stream_id} advertises {advertised} decompressed bytes but \
                 received {actual}"
            ),
            Self::StreamHasNoTables { stream_id } => {
                write!(formatter, "packed stream {stream_id} has no schema tables")
            }
            Self::UnexpectedTableStream {
                table_index,
                expected,
                actual,
            } => write!(
                formatter,
                "schema table {table_index} belongs to stream {actual}, expected stream {expected}"
            ),
            Self::NonContiguousTable {
                table_index,
                stream_id,
                expected_offset,
                actual_offset,
            } => write!(
                formatter,
                "schema table {table_index} in stream {stream_id} starts at {actual_offset}, \
                 expected {expected_offset}"
            ),
            Self::TableRangeOutOfBounds {
                table_index,
                stream_id,
                start,
                end,
                stream_size,
            } => write!(
                formatter,
                "schema table {table_index} range {start}..{end} is outside stream {stream_id} \
                 size {stream_size}"
            ),
            Self::TableCoverageMismatch {
                stream_id,
                covered,
                stream_size,
            } => write!(
                formatter,
                "schema tables cover bytes 0..{covered} of stream {stream_id}, whose size is \
                 {stream_size}"
            ),
            Self::TableCoordinateOverflow {
                table_index,
                stream_id,
                offset,
                size,
            } => write!(
                formatter,
                "schema table {table_index} coordinates {offset}+{size} in stream {stream_id} \
                 cannot address this platform"
            ),
            Self::UnknownSchemaId {
                table_index,
                schema_id,
            } => write!(
                formatter,
                "schema table {table_index} references missing schema ID {schema_id}"
            ),
            Self::Column {
                table_index,
                schema_id,
                source,
            } => write!(
                formatter,
                "schema table {table_index} (schema {schema_id}) is corrupt: {source}"
            ),
            Self::SizeOverflow => formatter.write_str("packed-stream table size overflow"),
        }
    }
}

impl Error for TableStreamError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Column { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::io::Read;
    use std::io::Take;

    use super::*;
    use crate::archive::ArchiveCatalog;
    use crate::archive::ArchiveCatalogLimits;
    use crate::archive::ColumnCorruption;
    use crate::archive::ColumnData;
    use crate::archive::DecodedPackedStream;
    use crate::archive::DictionaryLimits;
    use crate::archive::NodeType;
    use crate::archive::PackedStreamLimits;
    use crate::archive::SchemaMapLimits;
    use crate::archive::SchemaTreeLimits;
    use crate::archive::SingleFileArchiveReader;
    use crate::archive::TableMetadataLimits;
    use crate::archive::TimestampDictionaryLimits;
    use crate::archive::dictionary::decode_array_dictionary;
    use crate::archive::dictionary::decode_logtype_dictionary;
    use crate::archive::dictionary::decode_variable_dictionary;
    use crate::archive::schema_map::decode_schema_map;
    use crate::archive::schema_tree::decode_schema_tree;
    use crate::archive::table_metadata::decode_table_metadata;

    const CPP_FIXTURE: &[u8] = include_bytes!("../../tests/fixtures/sfa-v0.5.0-minimal-cpp.bin");

    type TestTable = (u64, u64, i32, u64);

    struct Resources {
        variable: VariableDictionary,
        logtype: LogTypeDictionary,
        array: ArrayDictionary,
        timestamp: TimestampDictionary,
    }

    fn take(bytes: &[u8]) -> Take<Cursor<&[u8]>> {
        Cursor::new(bytes).take(u64::try_from(bytes.len()).expect("test byte length fits u64"))
    }

    fn empty_resources() -> Resources {
        let empty_dictionary = 0_u64.to_le_bytes();
        Resources {
            variable: decode_variable_dictionary(
                take(&empty_dictionary),
                DictionaryLimits::default(),
            )
            .expect("empty variable dictionary"),
            logtype: decode_logtype_dictionary(
                take(&empty_dictionary),
                DictionaryLimits::default(),
            )
            .expect("empty logtype dictionary"),
            array: decode_array_dictionary(take(&empty_dictionary), DictionaryLimits::default())
                .expect("empty array dictionary"),
            timestamp: TimestampDictionary::decode(
                vec![0_u8; 16],
                TimestampDictionaryLimits::default(),
            )
            .expect("empty timestamp dictionary"),
        }
    }

    fn schema_tree(nodes: &[(i32, &[u8], NodeType)]) -> SchemaTree {
        let mut raw = u64::try_from(nodes.len())
            .expect("test node count fits u64")
            .to_le_bytes()
            .to_vec();
        for &(parent, key, node_type) in nodes {
            raw.extend_from_slice(&parent.to_le_bytes());
            raw.extend_from_slice(
                &u64::try_from(key.len())
                    .expect("test key length fits u64")
                    .to_le_bytes(),
            );
            raw.extend_from_slice(key);
            raw.push(node_type as u8);
        }
        let compressed =
            zstd::stream::encode_all(raw.as_slice(), 3).expect("compress synthetic schema tree");
        decode_schema_tree(take(&compressed), SchemaTreeLimits::default())
            .expect("decode synthetic schema tree")
    }

    fn schema_map(tree: &SchemaTree, schemas: &[(i32, &[u32])]) -> SchemaMap {
        let mut raw = u64::try_from(schemas.len())
            .expect("test schema count fits u64")
            .to_le_bytes()
            .to_vec();
        for &(schema_id, entries) in schemas {
            raw.extend_from_slice(&schema_id.to_le_bytes());
            let entry_count =
                u32::try_from(entries.len()).expect("test schema entry count fits u32");
            raw.extend_from_slice(&entry_count.to_le_bytes());
            raw.extend_from_slice(&entry_count.to_le_bytes());
            for &node_id in entries {
                raw.extend_from_slice(&node_id.to_le_bytes());
            }
        }
        let compressed =
            zstd::stream::encode_all(raw.as_slice(), 3).expect("compress synthetic schema map");
        decode_schema_map(take(&compressed), tree, SchemaMapLimits::default())
            .expect("decode synthetic schema map")
    }

    fn table_metadata(
        schemas: &SchemaMap,
        stream_size: u64,
        tables: &[TestTable],
    ) -> TableMetadata {
        let mut raw = 1_u64.to_le_bytes().to_vec();
        raw.extend_from_slice(&0_u64.to_le_bytes());
        raw.extend_from_slice(&stream_size.to_le_bytes());
        raw.extend_from_slice(&0_u64.to_le_bytes());
        raw.extend_from_slice(
            &u64::try_from(tables.len())
                .expect("test table count fits u64")
                .to_le_bytes(),
        );
        for &(stream_id, stream_offset, schema_id, message_count) in tables {
            raw.extend_from_slice(&stream_id.to_le_bytes());
            raw.extend_from_slice(&stream_offset.to_le_bytes());
            raw.extend_from_slice(&schema_id.to_le_bytes());
            raw.extend_from_slice(&message_count.to_le_bytes());
        }
        let compressed =
            zstd::stream::encode_all(raw.as_slice(), 3).expect("compress test table metadata");
        let tables_compressed_size = u64::from(0 != stream_size);
        decode_table_metadata(
            take(&compressed),
            schemas,
            tables_compressed_size,
            TableMetadataLimits::default(),
        )
        .expect("decode synthetic table metadata")
    }

    fn cpp_catalog_and_stream() -> (ArchiveCatalog, DecodedPackedStream) {
        let mut archive = SingleFileArchiveReader::open(Cursor::new(CPP_FIXTURE))
            .expect("open committed C++ fixture");
        let catalog = archive
            .read_catalog(ArchiveCatalogLimits::default())
            .expect("decode committed C++ fixture catalog");
        let stream = archive
            .read_packed_stream(
                catalog.metadata(),
                catalog.table_metadata(),
                0,
                PackedStreamLimits::default(),
            )
            .expect("decode committed C++ fixture stream");
        (catalog, stream)
    }

    #[test]
    fn lazily_decodes_the_cpp_oracle_stream_without_copying_values() {
        let (catalog, stream) = cpp_catalog_and_stream();
        let mut tables = SchemaTableStream::new(
            0,
            stream.as_bytes(),
            None,
            catalog.table_metadata(),
            catalog.schema_map(),
            catalog.schema_tree(),
            catalog.variable_dictionary(),
            catalog.log_type_dictionary(),
            catalog.array_dictionary(),
            catalog.metadata().timestamp_dictionary(),
            ColumnLimits::default(),
        )
        .expect("select C++ fixture stream tables");

        assert_eq!(0, tables.stream_id());
        assert_eq!(stream.as_bytes(), tables.stream_bytes());
        assert_eq!(1, tables.len());
        assert_eq!((1, Some(1)), tables.size_hint());
        let decoded = tables
            .next()
            .expect("one C++ fixture table")
            .expect("decode C++ fixture table");
        assert_eq!(0, decoded.table_index());
        assert_eq!(0, decoded.metadata().schema_id());
        assert_eq!(0, decoded.schema().id());
        assert_eq!(1, decoded.table().message_count());
        assert_eq!(6, decoded.table().len());

        let columns = decoded.table().columns();
        let ColumnData::DeltaInteger(log_index) = columns[0].data() else {
            panic!("first C++ column is delta integer");
        };
        assert_eq!(Some(0), log_index.get(0));
        assert_eq!(
            stream.as_bytes().as_ptr(),
            log_index.deltas().encoded_bytes().as_ptr(),
            "first column must borrow the packed-stream allocation"
        );
        let ColumnData::Timestamp(timestamp) = columns[1].data() else {
            panic!("second C++ column is timestamp");
        };
        assert_eq!(
            Some(1_700_000_000_123_000_000),
            timestamp
                .get(0)
                .map(crate::archive::TimestampValue::epoch_nanoseconds)
        );
        let ColumnData::VarString(level) = columns[2].data() else {
            panic!("third C++ column is variable string");
        };
        assert_eq!(Some(b"INFO".as_slice()), level.value(0));
        let ColumnData::ClpString(message) = columns[3].data() else {
            panic!("fourth C++ column is CLP string");
        };
        let message = message.record(0).expect("C++ CLP record");
        assert_eq!(0, message.descriptor().raw());
        assert_eq!(b"oracle fixture", message.logtype().escaped_value());
        let ColumnData::Integer(value) = columns[4].data() else {
            panic!("fifth C++ column is integer");
        };
        assert_eq!(Some(42), value.get(0));
        let ColumnData::Boolean(active) = columns[5].data() else {
            panic!("sixth C++ column is Boolean");
        };
        assert_eq!(Some(true), active.get(0));

        assert_eq!(0, tables.len());
        assert!(tables.next().is_none());
        assert!(tables.next().is_none());
    }

    #[test]
    fn rejects_unknown_stream_ids_and_exact_length_mismatches() {
        let (catalog, stream) = cpp_catalog_and_stream();
        let construct = |stream_id, bytes| {
            SchemaTableStream::new(
                stream_id,
                bytes,
                None,
                catalog.table_metadata(),
                catalog.schema_map(),
                catalog.schema_tree(),
                catalog.variable_dictionary(),
                catalog.log_type_dictionary(),
                catalog.array_dictionary(),
                catalog.metadata().timestamp_dictionary(),
                ColumnLimits::default(),
            )
        };

        assert!(matches!(
            construct(1, stream.as_bytes()),
            Err(TableStreamError::StreamIdOutOfBounds {
                stream_id: 1,
                stream_count: 1
            })
        ));
        assert!(matches!(
            construct(0, &stream.as_bytes()[..56]),
            Err(TableStreamError::StreamLengthMismatch {
                stream_id: 0,
                advertised: 57,
                actual: 56
            })
        ));
        let mut excess = stream.as_bytes().to_vec();
        excess.push(0);
        assert!(matches!(
            construct(0, &excess),
            Err(TableStreamError::StreamLengthMismatch {
                stream_id: 0,
                advertised: 57,
                actual: 58
            })
        ));
    }

    #[test]
    fn decodes_one_table_per_next_and_localizes_later_corruption() {
        let tree = schema_tree(&[
            (-1, b"", NodeType::Object),
            (0, b"value", NodeType::Integer),
            (0, b"active", NodeType::Boolean),
        ]);
        let schemas = schema_map(&tree, &[(7, &[1]), (-3, &[2])]);
        let metadata = table_metadata(&schemas, 9, &[(0, 0, 7, 1), (0, 8, -3, 1)]);
        let resources = empty_resources();
        let mut bytes = 42_i64.to_le_bytes().to_vec();
        bytes.push(2);

        let mut tables = SchemaTableStream::new(
            0,
            &bytes,
            None,
            &metadata,
            &schemas,
            &tree,
            &resources.variable,
            &resources.logtype,
            &resources.array,
            &resources.timestamp,
            ColumnLimits::default(),
        )
        .expect("span validation does not eagerly decode the second table");
        assert_eq!(2, tables.len());

        let first = tables
            .next()
            .expect("first synthetic table")
            .expect("first table is valid");
        assert_eq!(0, first.table_index());
        assert_eq!(7, first.metadata().schema_id());
        let ColumnData::Integer(values) = first.table().columns()[0].data() else {
            panic!("first synthetic table is integer");
        };
        assert_eq!(Some(42), values.get(0));
        assert_eq!(1, tables.len());

        let error = tables
            .next()
            .expect("second synthetic table")
            .expect_err("second table has a noncanonical Boolean");
        assert!(matches!(
            error,
            TableStreamError::Column {
                table_index: 1,
                schema_id: -3,
                source: ColumnError::Corrupt {
                    reason: ColumnCorruption::InvalidBoolean { actual: 2 },
                    ..
                }
            }
        ));
        assert!(tables.is_empty());
        assert!(tables.next().is_none());
    }

    #[test]
    fn reports_a_schema_map_mixed_from_another_archive_lazily() {
        let tree = schema_tree(&[
            (-1, b"", NodeType::Object),
            (0, b"value", NodeType::Integer),
            (0, b"active", NodeType::Boolean),
        ]);
        let metadata_schemas = schema_map(&tree, &[(7, &[1]), (-3, &[2])]);
        let supplied_schemas = schema_map(&tree, &[(7, &[1]), (5, &[2])]);
        let metadata = table_metadata(&metadata_schemas, 9, &[(0, 0, 7, 1), (0, 8, -3, 1)]);
        let resources = empty_resources();
        let mut bytes = 42_i64.to_le_bytes().to_vec();
        bytes.push(1);
        let mut tables = SchemaTableStream::new(
            0,
            &bytes,
            None,
            &metadata,
            &supplied_schemas,
            &tree,
            &resources.variable,
            &resources.logtype,
            &resources.array,
            &resources.timestamp,
            ColumnLimits::default(),
        )
        .expect("stream coordinates remain valid with a mismatched schema map");

        assert!(tables.next().expect("first table").is_ok());
        assert!(matches!(
            tables.next().expect("second table"),
            Err(TableStreamError::UnknownSchemaId {
                table_index: 1,
                schema_id: -3
            })
        ));
    }

    #[test]
    fn yields_equal_offset_zero_byte_tables_by_metadata_count() {
        let tree = schema_tree(&[(-1, b"", NodeType::Object)]);
        let schemas = schema_map(&tree, &[(9, &[]), (-2, &[])]);
        let metadata = table_metadata(&schemas, 0, &[(0, 0, 9, 3), (0, 0, -2, 4)]);
        let resources = empty_resources();
        let mut tables = SchemaTableStream::new(
            0,
            &[],
            None,
            &metadata,
            &schemas,
            &tree,
            &resources.variable,
            &resources.logtype,
            &resources.array,
            &resources.timestamp,
            ColumnLimits::default(),
        )
        .expect("canonical empty stream");

        let first = tables
            .next()
            .expect("zero-byte table is still an iterator item")
            .expect("structural-only table is valid");
        assert_eq!(3, first.table().message_count());
        assert!(first.table().is_empty());
        let second = tables
            .next()
            .expect("equal-offset table is a distinct iterator item")
            .expect("second structural-only table is valid");
        assert_eq!(4, second.table().message_count());
        assert!(second.table().is_empty());
        assert!(tables.next().is_none());
    }

    #[test]
    fn seeking_repositions_the_stream_and_refuses_indices_outside_it() {
        let (catalog, stream) = cpp_catalog_and_stream();
        let construct = || {
            SchemaTableStream::new(
                0,
                stream.as_bytes(),
                None,
                catalog.table_metadata(),
                catalog.schema_map(),
                catalog.schema_tree(),
                catalog.variable_dictionary(),
                catalog.log_type_dictionary(),
                catalog.array_dictionary(),
                catalog.metadata().timestamp_dictionary(),
                ColumnLimits::default(),
            )
            .expect("select C++ fixture stream tables")
        };

        let mut tables = construct();
        let first = tables.next_table_index();
        assert!(tables.seek_to_table(first));
        assert_eq!(
            first,
            tables
                .next()
                .expect("a table remains")
                .expect("the table decodes")
                .table_index()
        );

        // One past the last table is a valid resume position and yields nothing.
        let mut tables = construct();
        let end = first + tables.len();
        assert!(tables.seek_to_table(end));
        assert_eq!(end, tables.next_table_index());
        assert!(tables.next().is_none());

        // An index outside this stream is refused and leaves the position untouched.
        let mut tables = construct();
        let before = tables.next_table_index();
        assert!(!tables.seek_to_table(end + 1));
        assert_eq!(before, tables.next_table_index());
    }
}
