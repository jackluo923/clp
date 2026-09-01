use std::collections::HashMap;
use std::collections::HashSet;
use std::error::Error;
use std::fmt::Display;
use std::fmt::Formatter;
use std::fmt::{self};
use std::io::Read;
use std::io::Take;
use std::io::{self};

use super::schema::NodeType;
use super::schema::UnknownNodeType;
use super::schema_tree::SchemaTree;

const SCHEMA_COUNT_SIZE: u64 = 8;
const SCHEMA_FIXED_SIZE: u64 = 4 + 4 + 4;
const SCHEMA_ENTRY_SIZE: u64 = 4;
const DELIMITER_TYPE_MASK: u32 = 0xff00_0000;
const DELIMITER_LENGTH_MASK: u32 = 0x00ff_ffff;
const DELIMITER_TYPE_SHIFT: u32 = 24;

/// Resource limits applied while decoding a schema-map section.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SchemaMapLimits {
    compressed: u64,
    decompressed: u64,
    schemas: u64,
    entries_per_schema: u32,
    total_entries: u64,
}

impl SchemaMapLimits {
    /// Creates explicit schema-map resource limits.
    #[must_use]
    pub const fn new(
        max_compressed_size: u64,
        max_decompressed_size: u64,
        max_schemas: u64,
        max_entries_per_schema: u32,
        max_total_entries: u64,
    ) -> Self {
        Self {
            compressed: max_compressed_size,
            decompressed: max_decompressed_size,
            schemas: max_schemas,
            entries_per_schema: max_entries_per_schema,
            total_entries: max_total_entries,
        }
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

    /// Maximum number of schemas accepted.
    #[must_use]
    pub const fn max_schemas(self) -> u64 {
        self.schemas
    }

    /// Maximum flattened entries accepted in one schema.
    #[must_use]
    pub const fn max_entries_per_schema(self) -> u32 {
        self.entries_per_schema
    }

    /// Maximum flattened entries accepted across the section.
    #[must_use]
    pub const fn max_total_entries(self) -> u64 {
        self.total_entries
    }
}

impl Default for SchemaMapLimits {
    fn default() -> Self {
        const MEBIBYTE: u64 = 1024 * 1024;
        Self::new(
            256 * MEBIBYTE,
            512 * MEBIBYTE,
            1_048_576,
            4 * 1024 * 1024,
            16 * 1024 * 1024,
        )
    }
}

/// One flattened schema entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum SchemaEntry {
    /// A schema-tree node ID.
    Node(u32),
    /// A delimiter for a flattened unordered object or structured-array body.
    UnorderedContainer {
        /// Structural node type represented by the body.
        node_type: NodeType,
        /// Number of immediately following flattened entries in the body.
        body_len: u32,
    },
}

impl SchemaEntry {
    /// Returns the exact signed 32-bit wire representation.
    #[must_use]
    pub const fn wire_value(self) -> i32 {
        let bits = match self {
            Self::Node(node_id) => node_id,
            Self::UnorderedContainer {
                node_type,
                body_len,
            } => ((node_type as u32) << DELIMITER_TYPE_SHIFT) | body_len,
        };
        i32::from_le_bytes(bits.to_le_bytes())
    }
}

/// One validated schema and its opaque archive ID.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SchemaDefinition {
    id: i32,
    entries: Vec<SchemaEntry>,
    ordered_entry_count: usize,
}

impl SchemaDefinition {
    /// Returns the opaque signed schema ID.
    #[must_use]
    pub const fn id(&self) -> i32 {
        self.id
    }

    /// Returns all flattened entries in wire order.
    #[must_use]
    pub fn entries(&self) -> &[SchemaEntry] {
        &self.entries
    }

    /// Returns the ordered region, whose entries are unique schema-tree node IDs.
    #[must_use]
    pub fn ordered_entries(&self) -> &[SchemaEntry] {
        &self.entries[..self.ordered_entry_count]
    }

    /// Returns the flattened unordered region.
    #[must_use]
    pub fn unordered_entries(&self) -> &[SchemaEntry] {
        &self.entries[self.ordered_entry_count..]
    }

    /// Returns the number of entries in the ordered region.
    #[must_use]
    pub const fn ordered_entry_count(&self) -> usize {
        self.ordered_entry_count
    }
}

/// Validated schemas indexed by their opaque IDs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SchemaMap {
    schemas: Vec<SchemaDefinition>,
    indexes: HashMap<i32, usize>,
}

impl SchemaMap {
    /// Returns schemas in physical serialization order.
    #[must_use]
    pub fn schemas(&self) -> &[SchemaDefinition] {
        &self.schemas
    }

    /// Finds a schema by its opaque ID.
    #[must_use]
    pub fn get(&self, schema_id: i32) -> Option<&SchemaDefinition> {
        self.indexes
            .get(&schema_id)
            .map(|index| &self.schemas[*index])
    }

    /// Returns the number of schemas.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.schemas.len()
    }

    /// Returns whether no schemas were serialized.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.schemas.is_empty()
    }
}

pub(super) fn decode_schema_map<R: Read>(
    compressed: Take<R>,
    schema_tree: &SchemaTree,
    limits: SchemaMapLimits,
) -> Result<SchemaMap, SchemaMapError> {
    let compressed_size = compressed.limit();
    if compressed_size > limits.compressed {
        return Err(SchemaMapError::CompressedSectionTooLarge {
            actual: compressed_size,
            limit: limits.compressed,
        });
    }

    let mut decoder = zstd::stream::read::Decoder::new(compressed)
        .map_err(SchemaMapError::Io)?
        .single_frame();
    let schema_map = decode_schemas(&mut decoder, schema_tree, limits)?;

    let mut trailing = [0_u8; 1];
    if 0 != decoder.read(&mut trailing).map_err(SchemaMapError::Io)? {
        return Err(SchemaMapError::TrailingDecompressedData);
    }

    let compressed = decoder.finish();
    let remaining_compressed = u64::try_from(compressed.buffer().len())
        .map_err(|_| SchemaMapError::SizeOverflow)?
        .checked_add(compressed.get_ref().limit())
        .ok_or(SchemaMapError::SizeOverflow)?;
    if 0 != remaining_compressed {
        return Err(SchemaMapError::TrailingCompressedData {
            remaining: remaining_compressed,
        });
    }

    Ok(schema_map)
}

fn decode_schemas<R: Read>(
    reader: &mut R,
    schema_tree: &SchemaTree,
    limits: SchemaMapLimits,
) -> Result<SchemaMap, SchemaMapError> {
    let schema_count = read_u64(reader)?;
    if schema_count > limits.schemas {
        return Err(SchemaMapError::SchemaCountTooLarge {
            actual: schema_count,
            limit: limits.schemas,
        });
    }
    let minimum_size = schema_count
        .checked_mul(SCHEMA_FIXED_SIZE)
        .and_then(|size| size.checked_add(SCHEMA_COUNT_SIZE))
        .ok_or(SchemaMapError::SizeOverflow)?;
    if minimum_size > limits.decompressed {
        return Err(SchemaMapError::DecompressedSectionTooLarge {
            actual: minimum_size,
            limit: limits.decompressed,
        });
    }

    let schema_count = usize::try_from(schema_count).map_err(|_| SchemaMapError::SizeOverflow)?;
    let mut schemas = Vec::new();
    schemas
        .try_reserve_exact(schema_count)
        .map_err(|_| SchemaMapError::AllocationFailed {
            requested: schema_count,
        })?;
    let mut indexes = HashMap::new();
    indexes
        .try_reserve(schema_count)
        .map_err(|_| SchemaMapError::AllocationFailed {
            requested: schema_count,
        })?;

    let mut decompressed_size = SCHEMA_COUNT_SIZE;
    let mut total_entries = 0_u64;
    for schema_index in 0..schema_count {
        let id = read_i32(reader)?;
        if let Some(previous_schema_index) = indexes.get(&id) {
            return Err(SchemaMapError::DuplicateSchemaId {
                schema_index,
                previous_schema_index: *previous_schema_index,
                id,
            });
        }
        let entry_count = read_u32(reader)?;
        let ordered_entry_count = read_u32(reader)?;
        if ordered_entry_count > entry_count {
            return Err(SchemaMapError::OrderedEntryCountOutOfBounds {
                schema_index,
                ordered: ordered_entry_count,
                total: entry_count,
            });
        }
        if entry_count > limits.entries_per_schema {
            return Err(SchemaMapError::EntriesPerSchemaTooLarge {
                schema_index,
                actual: entry_count,
                limit: limits.entries_per_schema,
            });
        }
        total_entries = total_entries
            .checked_add(u64::from(entry_count))
            .ok_or(SchemaMapError::SizeOverflow)?;
        if total_entries > limits.total_entries {
            return Err(SchemaMapError::TotalEntriesTooLarge {
                actual: total_entries,
                limit: limits.total_entries,
            });
        }
        decompressed_size = u64::from(entry_count)
            .checked_mul(SCHEMA_ENTRY_SIZE)
            .and_then(|size| size.checked_add(SCHEMA_FIXED_SIZE))
            .and_then(|size| decompressed_size.checked_add(size))
            .ok_or(SchemaMapError::SizeOverflow)?;
        if decompressed_size > limits.decompressed {
            return Err(SchemaMapError::DecompressedSectionTooLarge {
                actual: decompressed_size,
                limit: limits.decompressed,
            });
        }

        let entry_count = usize::try_from(entry_count).map_err(|_| SchemaMapError::SizeOverflow)?;
        let ordered_entry_count =
            usize::try_from(ordered_entry_count).map_err(|_| SchemaMapError::SizeOverflow)?;
        let entries = decode_entries(
            reader,
            schema_tree,
            schema_index,
            entry_count,
            ordered_entry_count,
        )?;
        indexes.insert(id, schema_index);
        schemas.push(SchemaDefinition {
            id,
            entries,
            ordered_entry_count,
        });
    }

    Ok(SchemaMap { schemas, indexes })
}

fn decode_entries<R: Read>(
    reader: &mut R,
    schema_tree: &SchemaTree,
    schema_index: usize,
    entry_count: usize,
    ordered_entry_count: usize,
) -> Result<Vec<SchemaEntry>, SchemaMapError> {
    let mut entries = Vec::new();
    entries
        .try_reserve_exact(entry_count)
        .map_err(|_| SchemaMapError::AllocationFailed {
            requested: entry_count,
        })?;
    let mut ordered_nodes = HashSet::new();
    ordered_nodes
        .try_reserve(ordered_entry_count)
        .map_err(|_| SchemaMapError::AllocationFailed {
            requested: ordered_entry_count,
        })?;

    for entry_index in 0..entry_count {
        let entry = decode_entry(read_i32(reader)?, schema_index, entry_index)?;
        match entry {
            SchemaEntry::Node(node_id) => {
                let node_index =
                    usize::try_from(node_id).map_err(|_| SchemaMapError::SizeOverflow)?;
                if schema_tree.get(node_index).is_none() {
                    return Err(SchemaMapError::UnknownSchemaNode {
                        schema_index,
                        entry_index,
                        node_id,
                        node_count: schema_tree.len(),
                    });
                }
                if entry_index < ordered_entry_count && !ordered_nodes.insert(node_id) {
                    return Err(SchemaMapError::DuplicateOrderedNode {
                        schema_index,
                        entry_index,
                        node_id,
                    });
                }
            }
            SchemaEntry::UnorderedContainer { .. } if entry_index < ordered_entry_count => {
                return Err(SchemaMapError::DelimiterInOrderedRegion {
                    schema_index,
                    entry_index,
                });
            }
            SchemaEntry::UnorderedContainer { .. } => {}
        }
        entries.push(entry);
    }
    validate_delimiter_nesting(schema_index, ordered_entry_count, &entries)?;
    Ok(entries)
}

fn decode_entry(
    raw: i32,
    schema_index: usize,
    entry_index: usize,
) -> Result<SchemaEntry, SchemaMapError> {
    let bits = u32::from_le_bytes(raw.to_le_bytes());
    if 0 == bits & DELIMITER_TYPE_MASK {
        return Ok(SchemaEntry::Node(bits));
    }

    let raw_node_type =
        u8::try_from(bits >> DELIMITER_TYPE_SHIFT).map_err(|_| SchemaMapError::SizeOverflow)?;
    let node_type = NodeType::try_from(raw_node_type).map_err(|source| {
        SchemaMapError::UnknownDelimiterType {
            schema_index,
            entry_index,
            source,
        }
    })?;
    if !matches!(node_type, NodeType::Object | NodeType::StructuredArray) {
        return Err(SchemaMapError::InvalidDelimiterType {
            schema_index,
            entry_index,
            node_type,
        });
    }
    Ok(SchemaEntry::UnorderedContainer {
        node_type,
        body_len: bits & DELIMITER_LENGTH_MASK,
    })
}

fn validate_delimiter_nesting(
    schema_index: usize,
    unordered_start: usize,
    entries: &[SchemaEntry],
) -> Result<(), SchemaMapError> {
    let mut enclosing_ends = Vec::new();
    enclosing_ends
        .try_reserve(entries.len().saturating_sub(unordered_start))
        .map_err(|_| SchemaMapError::AllocationFailed {
            requested: entries.len().saturating_sub(unordered_start),
        })?;

    for (entry_index, entry) in entries.iter().enumerate().skip(unordered_start) {
        while enclosing_ends.last() == Some(&entry_index) {
            enclosing_ends.pop();
        }
        let SchemaEntry::UnorderedContainer { body_len, .. } = entry else {
            continue;
        };
        let raw_body_len = *body_len;
        let body_len = usize::try_from(raw_body_len).map_err(|_| SchemaMapError::SizeOverflow)?;
        let body_end = entry_index
            .checked_add(1)
            .and_then(|index| index.checked_add(body_len))
            .ok_or(SchemaMapError::SizeOverflow)?;
        let enclosing_end = enclosing_ends.last().copied().unwrap_or(entries.len());
        if body_end > enclosing_end {
            return Err(SchemaMapError::DelimiterBodyOutOfBounds {
                schema_index,
                entry_index,
                body_len: raw_body_len,
                enclosing_end,
            });
        }
        if body_end > entry_index + 1 {
            enclosing_ends.push(body_end);
        }
    }
    Ok(())
}

fn read_i32<R: Read>(reader: &mut R) -> Result<i32, SchemaMapError> {
    let mut bytes = [0_u8; 4];
    reader.read_exact(&mut bytes).map_err(SchemaMapError::Io)?;
    Ok(i32::from_le_bytes(bytes))
}

fn read_u32<R: Read>(reader: &mut R) -> Result<u32, SchemaMapError> {
    let mut bytes = [0_u8; 4];
    reader.read_exact(&mut bytes).map_err(SchemaMapError::Io)?;
    Ok(u32::from_le_bytes(bytes))
}

fn read_u64<R: Read>(reader: &mut R) -> Result<u64, SchemaMapError> {
    let mut bytes = [0_u8; 8];
    reader.read_exact(&mut bytes).map_err(SchemaMapError::Io)?;
    Ok(u64::from_le_bytes(bytes))
}

/// Failure to decompress or validate a schema-map section.
#[derive(Debug)]
#[non_exhaustive]
pub enum SchemaMapError {
    /// The compressed section exceeds the configured limit.
    CompressedSectionTooLarge { actual: u64, limit: u64 },
    /// The decompressed section exceeds the configured limit.
    DecompressedSectionTooLarge { actual: u64, limit: u64 },
    /// The schema count exceeds the configured limit.
    SchemaCountTooLarge { actual: u64, limit: u64 },
    /// One schema's flattened entry count exceeds the configured limit.
    EntriesPerSchemaTooLarge {
        schema_index: usize,
        actual: u32,
        limit: u32,
    },
    /// The cumulative flattened entry count exceeds the configured limit.
    TotalEntriesTooLarge { actual: u64, limit: u64 },
    /// Input, decompression, or seeking failed.
    Io(io::Error),
    /// Checked size arithmetic or conversion overflowed.
    SizeOverflow,
    /// A bounded allocation could not be reserved.
    AllocationFailed { requested: usize },
    /// One opaque schema ID appeared more than once.
    DuplicateSchemaId {
        schema_index: usize,
        previous_schema_index: usize,
        id: i32,
    },
    /// The ordered-entry count exceeded the total entry count.
    OrderedEntryCountOutOfBounds {
        schema_index: usize,
        ordered: u32,
        total: u32,
    },
    /// An ordered-region entry was an unordered-container delimiter.
    DelimiterInOrderedRegion {
        schema_index: usize,
        entry_index: usize,
    },
    /// A node ID was repeated in the ordered region.
    DuplicateOrderedNode {
        schema_index: usize,
        entry_index: usize,
        node_id: u32,
    },
    /// A flattened node ID did not exist in the schema tree.
    UnknownSchemaNode {
        schema_index: usize,
        entry_index: usize,
        node_id: u32,
        node_count: usize,
    },
    /// A delimiter's high byte was not a known node type.
    UnknownDelimiterType {
        schema_index: usize,
        entry_index: usize,
        source: UnknownNodeType,
    },
    /// A known node type cannot delimit an unordered container.
    InvalidDelimiterType {
        schema_index: usize,
        entry_index: usize,
        node_type: NodeType,
    },
    /// A flattened body extended past its section or enclosing body.
    DelimiterBodyOutOfBounds {
        schema_index: usize,
        entry_index: usize,
        body_len: u32,
        enclosing_end: usize,
    },
    /// Decompressed bytes followed the declared schema sequence.
    TrailingDecompressedData,
    /// Compressed bytes followed the one schema-map zstd frame.
    TrailingCompressedData { remaining: u64 },
    /// The supplied metadata did not contain the schema-map section.
    MissingSection,
    /// The supplied schema-map range was outside this archive's files region.
    SectionOutsideArchive,
}

impl Display for SchemaMapError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::CompressedSectionTooLarge { .. }
            | Self::DecompressedSectionTooLarge { .. }
            | Self::SchemaCountTooLarge { .. }
            | Self::EntriesPerSchemaTooLarge { .. }
            | Self::TotalEntriesTooLarge { .. }
            | Self::Io(_)
            | Self::SizeOverflow
            | Self::AllocationFailed { .. } => format_resource_error(self, formatter),
            Self::DuplicateSchemaId {
                schema_index,
                previous_schema_index,
                id,
            } => write!(
                formatter,
                "schema-map schema {schema_index} repeats ID {id} from schema \
                 {previous_schema_index}"
            ),
            Self::OrderedEntryCountOutOfBounds {
                schema_index,
                ordered,
                total,
            } => write!(
                formatter,
                "schema-map schema {schema_index} has {ordered} ordered entries but {total} total"
            ),
            Self::DelimiterInOrderedRegion {
                schema_index,
                entry_index,
            } => write!(
                formatter,
                "schema-map schema {schema_index} entry {entry_index} is a delimiter in the \
                 ordered region"
            ),
            Self::DuplicateOrderedNode {
                schema_index,
                entry_index,
                node_id,
            } => write!(
                formatter,
                "schema-map schema {schema_index} ordered entry {entry_index} repeats node \
                 {node_id}"
            ),
            Self::UnknownSchemaNode {
                schema_index,
                entry_index,
                node_id,
                node_count,
            } => write!(
                formatter,
                "schema-map schema {schema_index} entry {entry_index} references node {node_id}, \
                 but the schema tree has {node_count} nodes"
            ),
            Self::UnknownDelimiterType {
                schema_index,
                entry_index,
                source,
            } => write!(
                formatter,
                "schema-map schema {schema_index} entry {entry_index} has {source} in its \
                 delimiter"
            ),
            Self::InvalidDelimiterType {
                schema_index,
                entry_index,
                node_type,
            } => write!(
                formatter,
                "schema-map schema {schema_index} entry {entry_index} uses {node_type:?} as a \
                 container delimiter"
            ),
            Self::DelimiterBodyOutOfBounds {
                schema_index,
                entry_index,
                body_len,
                enclosing_end,
            } => write!(
                formatter,
                "schema-map schema {schema_index} delimiter {entry_index} body length {body_len} \
                 exceeds enclosing end {enclosing_end}"
            ),
            Self::TrailingDecompressedData => {
                formatter.write_str("data follows the declared schema-map entries")
            }
            Self::TrailingCompressedData { remaining } => write!(
                formatter,
                "{remaining} compressed bytes follow the schema-map zstd frame"
            ),
            Self::MissingSection => {
                formatter.write_str("archive metadata has no schema-map section")
            }
            Self::SectionOutsideArchive => {
                formatter.write_str("schema-map section is outside the archive files region")
            }
        }
    }
}

fn format_resource_error(error: &SchemaMapError, formatter: &mut Formatter<'_>) -> fmt::Result {
    match error {
        SchemaMapError::CompressedSectionTooLarge { actual, limit } => write!(
            formatter,
            "compressed schema-map size {actual} exceeds limit {limit}"
        ),
        SchemaMapError::DecompressedSectionTooLarge { actual, limit } => write!(
            formatter,
            "decompressed schema-map size {actual} exceeds limit {limit}"
        ),
        SchemaMapError::SchemaCountTooLarge { actual, limit } => {
            write!(formatter, "schema-map count {actual} exceeds limit {limit}")
        }
        SchemaMapError::EntriesPerSchemaTooLarge {
            schema_index,
            actual,
            limit,
        } => write!(
            formatter,
            "schema-map schema {schema_index} entry count {actual} exceeds limit {limit}"
        ),
        SchemaMapError::TotalEntriesTooLarge { actual, limit } => write!(
            formatter,
            "schema-map total entry count {actual} exceeds limit {limit}"
        ),
        SchemaMapError::Io(error) => write!(formatter, "schema-map I/O failed: {error}"),
        SchemaMapError::SizeOverflow => formatter.write_str("schema-map size overflow"),
        SchemaMapError::AllocationFailed { requested } => write!(
            formatter,
            "could not reserve bounded schema-map allocation of {requested} elements"
        ),
        _ => unreachable!("resource formatter called with a structural schema-map error"),
    }
}

impl Error for SchemaMapError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::UnknownDelimiterType { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::archive::schema_tree::SchemaTreeLimits;
    use crate::archive::schema_tree::decode_schema_tree;

    fn tree() -> SchemaTree {
        let nodes: [(i32, &[u8], NodeType); 6] = [
            (-1, b"", NodeType::Object),
            (0, b"count", NodeType::Integer),
            (0, b"nested", NodeType::Object),
            (2, b"text", NodeType::VarString),
            (-1, b"", NodeType::Metadata),
            (4, b"log_event_idx", NodeType::DeltaInteger),
        ];
        let mut raw = u64::try_from(nodes.len())
            .expect("test node count fits u64")
            .to_le_bytes()
            .to_vec();
        for (parent, key, node_type) in nodes {
            raw.extend_from_slice(&parent.to_le_bytes());
            raw.extend_from_slice(
                &u64::try_from(key.len())
                    .expect("test key length fits u64")
                    .to_le_bytes(),
            );
            raw.extend_from_slice(key);
            raw.push(node_type as u8);
        }
        let compressed = zstd::stream::encode_all(raw.as_slice(), 3).expect("compress test tree");
        let compressed_size =
            u64::try_from(compressed.len()).expect("test compressed size fits u64");
        let mut source = Cursor::new(compressed);
        decode_schema_tree(
            source.by_ref().take(compressed_size),
            SchemaTreeLimits::default(),
        )
        .expect("valid test tree")
    }

    fn delimiter(node_type: NodeType, body_len: u32) -> i32 {
        let bits = ((node_type as u32) << DELIMITER_TYPE_SHIFT) | body_len;
        i32::from_le_bytes(bits.to_le_bytes())
    }

    fn section(schemas: &[(i32, u32, &[i32])]) -> Vec<u8> {
        let mut raw = u64::try_from(schemas.len())
            .expect("test schema count fits u64")
            .to_le_bytes()
            .to_vec();
        for &(id, ordered, entries) in schemas {
            raw.extend_from_slice(&id.to_le_bytes());
            raw.extend_from_slice(
                &u32::try_from(entries.len())
                    .expect("test entry count fits u32")
                    .to_le_bytes(),
            );
            raw.extend_from_slice(&ordered.to_le_bytes());
            for entry in entries {
                raw.extend_from_slice(&entry.to_le_bytes());
            }
        }
        zstd::stream::encode_all(raw.as_slice(), 3).expect("compress test schema map")
    }

    fn decode(bytes: &[u8]) -> Result<SchemaMap, SchemaMapError> {
        let tree = tree();
        let mut source = Cursor::new(bytes);
        let compressed_size = u64::try_from(bytes.len()).expect("test section size fits u64");
        decode_schema_map(
            source.by_ref().take(compressed_size),
            &tree,
            SchemaMapLimits::default(),
        )
    }

    #[test]
    fn decodes_valid_schemas_and_opaque_ids() {
        let unordered = delimiter(NodeType::Object, 2);
        let compressed = section(&[(7, 2, &[1, 5, unordered, 1, 3]), (-4, 1, &[3])]);

        let schemas = decode(&compressed).expect("valid schema map");

        assert_eq!(2, schemas.len());
        let first = schemas.get(7).expect("schema seven");
        assert_eq!(7, first.id());
        assert_eq!(
            &[SchemaEntry::Node(1), SchemaEntry::Node(5)],
            first.ordered_entries()
        );
        assert_eq!(3, first.unordered_entries().len());
        assert_eq!(Some(first), schemas.schemas().first());
        assert_eq!(Some(-4), schemas.get(-4).map(SchemaDefinition::id));
    }

    #[test]
    fn permits_repeated_nodes_in_the_unordered_region() {
        let compressed = section(&[(0, 0, &[1, 1])]);
        assert_eq!(
            2,
            decode(&compressed)
                .expect("unordered repeats are valid")
                .get(0)
                .expect("schema")
                .entries()
                .len()
        );
    }

    #[test]
    fn rejects_duplicate_schema_ids() {
        let compressed = section(&[(3, 1, &[1]), (3, 1, &[3])]);
        assert!(matches!(
            decode(&compressed),
            Err(SchemaMapError::DuplicateSchemaId {
                schema_index: 1,
                previous_schema_index: 0,
                id: 3
            })
        ));
    }

    #[test]
    fn rejects_invalid_ordered_region() {
        let too_many_ordered = section(&[(0, 2, &[1])]);
        assert!(matches!(
            decode(&too_many_ordered),
            Err(SchemaMapError::OrderedEntryCountOutOfBounds { .. })
        ));

        let delimiter_in_ordered = section(&[(0, 1, &[delimiter(NodeType::Object, 0)])]);
        assert!(matches!(
            decode(&delimiter_in_ordered),
            Err(SchemaMapError::DelimiterInOrderedRegion { .. })
        ));

        let duplicate = section(&[(0, 2, &[1, 1])]);
        assert!(matches!(
            decode(&duplicate),
            Err(SchemaMapError::DuplicateOrderedNode { .. })
        ));
    }

    #[test]
    fn rejects_unknown_schema_node() {
        let compressed = section(&[(0, 1, &[99])]);
        assert!(matches!(
            decode(&compressed),
            Err(SchemaMapError::UnknownSchemaNode {
                node_id: 99,
                node_count: 6,
                ..
            })
        ));
    }

    #[test]
    fn rejects_invalid_delimiter_types() {
        let scalar = section(&[(0, 0, &[delimiter(NodeType::Float, 0)])]);
        assert!(matches!(
            decode(&scalar),
            Err(SchemaMapError::InvalidDelimiterType {
                node_type: NodeType::Float,
                ..
            })
        ));

        let unknown_bits = i32::from_le_bytes(0xff00_0000_u32.to_le_bytes());
        let unknown = section(&[(0, 0, &[unknown_bits])]);
        assert!(matches!(
            decode(&unknown),
            Err(SchemaMapError::UnknownDelimiterType { source, .. })
                if 255 == source.value()
        ));
    }

    #[test]
    fn rejects_delimiter_bodies_outside_their_container() {
        let beyond_schema = section(&[(0, 0, &[delimiter(NodeType::Object, 2), 1])]);
        assert!(matches!(
            decode(&beyond_schema),
            Err(SchemaMapError::DelimiterBodyOutOfBounds { .. })
        ));

        let overlapping = section(&[(
            0,
            0,
            &[
                delimiter(NodeType::Object, 2),
                delimiter(NodeType::StructuredArray, 2),
                1,
                3,
            ],
        )]);
        assert!(matches!(
            decode(&overlapping),
            Err(SchemaMapError::DelimiterBodyOutOfBounds { .. })
        ));
    }

    #[test]
    fn rejects_trailing_decompressed_and_compressed_data() {
        let mut raw = 0_u64.to_le_bytes().to_vec();
        raw.push(1);
        let trailing_decompressed =
            zstd::stream::encode_all(raw.as_slice(), 3).expect("compress invalid map");
        assert!(matches!(
            decode(&trailing_decompressed),
            Err(SchemaMapError::TrailingDecompressedData)
        ));

        let mut trailing_compressed = section(&[]);
        trailing_compressed.extend_from_slice(b"trailing");
        assert!(matches!(
            decode(&trailing_compressed),
            Err(SchemaMapError::TrailingCompressedData { .. })
        ));
    }
}
