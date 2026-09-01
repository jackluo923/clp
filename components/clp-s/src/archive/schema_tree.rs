use std::collections::HashMap;
use std::error::Error;
use std::fmt::Display;
use std::fmt::Formatter;
use std::fmt::{self};
use std::io::Read;
use std::io::Take;
use std::io::{self};
use std::str::Utf8Error;
use std::sync::Arc;

use super::schema::NodeType;
use super::schema::UnknownNodeType;

const NODE_FIXED_SIZE: u64 = 4 + 8 + 1;
const NODE_COUNT_SIZE: u64 = 8;

/// Resource limits applied while decoding a schema-tree section.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SchemaTreeLimits {
    compressed: u64,
    decompressed: u64,
    nodes: u64,
    key: u64,
    total_key_bytes: u64,
}

impl SchemaTreeLimits {
    /// Creates explicit schema-tree resource limits.
    #[must_use]
    pub const fn new(
        max_compressed_size: u64,
        max_decompressed_size: u64,
        max_nodes: u64,
        max_key_size: u64,
        max_total_key_bytes: u64,
    ) -> Self {
        Self {
            compressed: max_compressed_size,
            decompressed: max_decompressed_size,
            nodes: max_nodes,
            key: max_key_size,
            total_key_bytes: max_total_key_bytes,
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

    /// Maximum number of schema nodes accepted.
    #[must_use]
    pub const fn max_nodes(self) -> u64 {
        self.nodes
    }

    /// Maximum bytes accepted in one node key.
    #[must_use]
    pub const fn max_key_size(self) -> u64 {
        self.key
    }

    /// Maximum cumulative key bytes accepted.
    #[must_use]
    pub const fn max_total_key_bytes(self) -> u64 {
        self.total_key_bytes
    }
}

impl Default for SchemaTreeLimits {
    fn default() -> Self {
        const MEBIBYTE: u64 = 1024 * 1024;
        Self::new(
            256 * MEBIBYTE,
            512 * MEBIBYTE,
            4 * 1024 * 1024,
            MEBIBYTE,
            256 * MEBIBYTE,
        )
    }
}

/// One validated node in a CLP-S schema tree.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SchemaNode {
    parent: Option<usize>,
    key: Arc<[u8]>,
    node_type: NodeType,
}

impl SchemaNode {
    /// Returns the zero-based ID of this node's parent, or `None` for a namespace root.
    #[must_use]
    pub const fn parent_id(&self) -> Option<usize> {
        self.parent
    }

    /// Returns the length-delimited key bytes exactly as stored in the archive.
    #[must_use]
    pub fn key_bytes(&self) -> &[u8] {
        &self.key
    }

    /// Interprets the node key as UTF-8 without changing its bytes.
    ///
    /// # Errors
    ///
    /// Returns an error when a corrupt or non-JSON archive contains an invalid UTF-8 key.
    pub fn key_str(&self) -> Result<&str, Utf8Error> {
        std::str::from_utf8(&self.key)
    }

    /// Returns the node's stable archive-format type.
    #[must_use]
    pub const fn node_type(&self) -> NodeType {
        self.node_type
    }
}

/// A schema tree whose implicit node IDs and parent relationships have been validated.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SchemaTree {
    nodes: Vec<SchemaNode>,
}

impl SchemaTree {
    /// Returns all nodes in implicit wire-ID order.
    #[must_use]
    pub fn nodes(&self) -> &[SchemaNode] {
        &self.nodes
    }

    /// Returns a node by its implicit wire ID.
    #[must_use]
    pub fn get(&self, node_id: usize) -> Option<&SchemaNode> {
        self.nodes.get(node_id)
    }

    /// Returns the number of nodes.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Returns whether the tree contains no nodes.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }
}

pub(super) fn decode_schema_tree<R: Read>(
    compressed: Take<R>,
    limits: SchemaTreeLimits,
) -> Result<SchemaTree, SchemaTreeError> {
    let compressed_size = compressed.limit();
    if compressed_size > limits.compressed {
        return Err(SchemaTreeError::CompressedSectionTooLarge {
            actual: compressed_size,
            limit: limits.compressed,
        });
    }

    let mut decoder = zstd::stream::read::Decoder::new(compressed)
        .map_err(SchemaTreeError::Io)?
        .single_frame();
    let tree = decode_nodes(&mut decoder, limits)?;

    let mut trailing = [0_u8; 1];
    if 0 != decoder.read(&mut trailing).map_err(SchemaTreeError::Io)? {
        return Err(SchemaTreeError::TrailingDecompressedData);
    }

    let compressed = decoder.finish();
    let remaining_compressed = u64::try_from(compressed.buffer().len())
        .map_err(|_| SchemaTreeError::SizeOverflow)?
        .checked_add(compressed.get_ref().limit())
        .ok_or(SchemaTreeError::SizeOverflow)?;
    if 0 != remaining_compressed {
        return Err(SchemaTreeError::TrailingCompressedData {
            remaining: remaining_compressed,
        });
    }

    Ok(tree)
}

fn decode_nodes<R: Read>(
    reader: &mut R,
    limits: SchemaTreeLimits,
) -> Result<SchemaTree, SchemaTreeError> {
    let node_count = read_u64(reader)?;
    if node_count > limits.nodes {
        return Err(SchemaTreeError::NodeCountTooLarge {
            actual: node_count,
            limit: limits.nodes,
        });
    }

    let minimum_size = node_count
        .checked_mul(NODE_FIXED_SIZE)
        .and_then(|size| size.checked_add(NODE_COUNT_SIZE))
        .ok_or(SchemaTreeError::SizeOverflow)?;
    if minimum_size > limits.decompressed {
        return Err(SchemaTreeError::DecompressedSectionTooLarge {
            actual: minimum_size,
            limit: limits.decompressed,
        });
    }

    let node_count = usize::try_from(node_count).map_err(|_| SchemaTreeError::SizeOverflow)?;
    let mut nodes = Vec::new();
    nodes
        .try_reserve_exact(node_count)
        .map_err(|_| SchemaTreeError::AllocationFailed {
            requested: node_count,
        })?;
    let mut identities = HashMap::new();
    identities
        .try_reserve(node_count)
        .map_err(|_| SchemaTreeError::AllocationFailed {
            requested: node_count,
        })?;

    let mut decompressed_size = NODE_COUNT_SIZE;
    let mut total_key_bytes = 0_u64;
    for node_id in 0..node_count {
        let raw_parent = read_i32(reader)?;
        let parent = decode_parent(node_id, raw_parent)?;
        let key_size = read_u64(reader)?;
        if key_size > limits.key {
            return Err(SchemaTreeError::KeyTooLarge {
                node_id,
                actual: key_size,
                limit: limits.key,
            });
        }
        total_key_bytes = total_key_bytes
            .checked_add(key_size)
            .ok_or(SchemaTreeError::SizeOverflow)?;
        if total_key_bytes > limits.total_key_bytes {
            return Err(SchemaTreeError::TotalKeyBytesTooLarge {
                actual: total_key_bytes,
                limit: limits.total_key_bytes,
            });
        }
        decompressed_size = decompressed_size
            .checked_add(NODE_FIXED_SIZE)
            .and_then(|size| size.checked_add(key_size))
            .ok_or(SchemaTreeError::SizeOverflow)?;
        if decompressed_size > limits.decompressed {
            return Err(SchemaTreeError::DecompressedSectionTooLarge {
                actual: decompressed_size,
                limit: limits.decompressed,
            });
        }

        let key_size = usize::try_from(key_size).map_err(|_| SchemaTreeError::SizeOverflow)?;
        let mut key = Vec::new();
        key.try_reserve_exact(key_size)
            .map_err(|_| SchemaTreeError::AllocationFailed {
                requested: key_size,
            })?;
        key.resize(key_size, 0);
        reader.read_exact(&mut key).map_err(SchemaTreeError::Io)?;
        let node_type = NodeType::try_from(read_u8(reader)?)
            .map_err(|source| SchemaTreeError::UnknownNodeType { node_id, source })?;
        let key = Arc::<[u8]>::from(key);
        let identity = (parent, node_type, Arc::clone(&key));
        if let Some(previous_node_id) = identities.insert(identity, node_id) {
            return Err(SchemaTreeError::DuplicateNode {
                node_id,
                previous_node_id,
            });
        }
        nodes.push(SchemaNode {
            parent,
            key,
            node_type,
        });
    }

    Ok(SchemaTree { nodes })
}

fn decode_parent(node_id: usize, raw_parent: i32) -> Result<Option<usize>, SchemaTreeError> {
    if -1 == raw_parent {
        return Ok(None);
    }
    let parent = usize::try_from(raw_parent).map_err(|_| SchemaTreeError::InvalidParent {
        node_id,
        actual: raw_parent,
    })?;
    if parent >= node_id {
        return Err(SchemaTreeError::ParentDoesNotPrecedeNode { node_id, parent });
    }
    Ok(Some(parent))
}

fn read_u8<R: Read>(reader: &mut R) -> Result<u8, SchemaTreeError> {
    let mut bytes = [0_u8; 1];
    reader.read_exact(&mut bytes).map_err(SchemaTreeError::Io)?;
    Ok(bytes[0])
}

fn read_i32<R: Read>(reader: &mut R) -> Result<i32, SchemaTreeError> {
    let mut bytes = [0_u8; 4];
    reader.read_exact(&mut bytes).map_err(SchemaTreeError::Io)?;
    Ok(i32::from_le_bytes(bytes))
}

fn read_u64<R: Read>(reader: &mut R) -> Result<u64, SchemaTreeError> {
    let mut bytes = [0_u8; 8];
    reader.read_exact(&mut bytes).map_err(SchemaTreeError::Io)?;
    Ok(u64::from_le_bytes(bytes))
}

/// Failure to decompress or validate a schema-tree section.
#[derive(Debug)]
#[non_exhaustive]
pub enum SchemaTreeError {
    /// The compressed section exceeds the configured limit.
    CompressedSectionTooLarge {
        /// Compressed section bytes.
        actual: u64,
        /// Configured maximum compressed bytes.
        limit: u64,
    },
    /// The decompressed section exceeds the configured limit.
    DecompressedSectionTooLarge {
        /// Decompressed bytes implied by the section framing.
        actual: u64,
        /// Configured maximum decompressed bytes.
        limit: u64,
    },
    /// The node count exceeds the configured limit.
    NodeCountTooLarge {
        /// Declared node count.
        actual: u64,
        /// Configured maximum node count.
        limit: u64,
    },
    /// One key exceeds the configured limit.
    KeyTooLarge {
        /// Implicit node ID.
        node_id: usize,
        /// Declared key bytes.
        actual: u64,
        /// Configured maximum key bytes.
        limit: u64,
    },
    /// Cumulative key bytes exceed the configured limit.
    TotalKeyBytesTooLarge {
        /// Cumulative declared key bytes.
        actual: u64,
        /// Configured cumulative key-byte maximum.
        limit: u64,
    },
    /// Input, decompression, or seeking failed.
    Io(io::Error),
    /// Checked size arithmetic or conversion overflowed.
    SizeOverflow,
    /// A bounded allocation could not be reserved.
    AllocationFailed {
        /// Elements or bytes requested by the failed reservation.
        requested: usize,
    },
    /// A parent ID was below the only root sentinel, `-1`.
    InvalidParent {
        /// Implicit node ID.
        node_id: usize,
        /// Invalid wire parent ID.
        actual: i32,
    },
    /// A non-root parent did not precede its child.
    ParentDoesNotPrecedeNode {
        /// Implicit child node ID.
        node_id: usize,
        /// Referenced parent node ID.
        parent: usize,
    },
    /// A node type is not supported by this library version.
    UnknownNodeType {
        /// Implicit node ID.
        node_id: usize,
        /// Unknown discriminant.
        source: UnknownNodeType,
    },
    /// A `(parent, key, type)` identity appeared more than once.
    DuplicateNode {
        /// Duplicate implicit node ID.
        node_id: usize,
        /// Earlier node with the same identity.
        previous_node_id: usize,
    },
    /// Decompressed bytes followed the declared node sequence.
    TrailingDecompressedData,
    /// Compressed bytes followed the one schema-tree zstd frame.
    TrailingCompressedData {
        /// Bytes remaining inside the bounded section.
        remaining: u64,
    },
    /// The supplied metadata did not contain the schema-tree section.
    MissingSection,
    /// The supplied schema-tree range was outside this archive's files region.
    SectionOutsideArchive,
}

impl Display for SchemaTreeError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::CompressedSectionTooLarge { actual, limit } => write!(
                formatter,
                "compressed schema-tree size {actual} exceeds limit {limit}"
            ),
            Self::DecompressedSectionTooLarge { actual, limit } => write!(
                formatter,
                "decompressed schema-tree size {actual} exceeds limit {limit}"
            ),
            Self::NodeCountTooLarge { actual, limit } => {
                write!(
                    formatter,
                    "schema-tree node count {actual} exceeds limit {limit}"
                )
            }
            Self::KeyTooLarge {
                node_id,
                actual,
                limit,
            } => write!(
                formatter,
                "schema-tree node {node_id} key size {actual} exceeds limit {limit}"
            ),
            Self::TotalKeyBytesTooLarge { actual, limit } => write!(
                formatter,
                "schema-tree key bytes {actual} exceed limit {limit}"
            ),
            Self::Io(error) => write!(formatter, "schema-tree I/O failed: {error}"),
            Self::SizeOverflow => formatter.write_str("schema-tree size overflow"),
            Self::AllocationFailed { requested } => write!(
                formatter,
                "could not reserve bounded schema-tree allocation of {requested} elements or bytes"
            ),
            Self::InvalidParent { node_id, actual } => write!(
                formatter,
                "schema-tree node {node_id} has invalid parent ID {actual}"
            ),
            Self::ParentDoesNotPrecedeNode { node_id, parent } => write!(
                formatter,
                "schema-tree node {node_id} references non-preceding parent {parent}"
            ),
            Self::UnknownNodeType { node_id, source } => {
                write!(formatter, "schema-tree node {node_id} has {source}")
            }
            Self::DuplicateNode {
                node_id,
                previous_node_id,
            } => write!(
                formatter,
                "schema-tree node {node_id} duplicates node {previous_node_id}"
            ),
            Self::TrailingDecompressedData => {
                formatter.write_str("data follows the declared schema-tree nodes")
            }
            Self::TrailingCompressedData { remaining } => write!(
                formatter,
                "{remaining} compressed bytes follow the schema-tree zstd frame"
            ),
            Self::MissingSection => {
                formatter.write_str("archive metadata has no schema-tree section")
            }
            Self::SectionOutsideArchive => {
                formatter.write_str("schema-tree section is outside the archive files region")
            }
        }
    }
}

impl Error for SchemaTreeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::UnknownNodeType { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    fn node(parent: i32, key: &[u8], node_type: NodeType, output: &mut Vec<u8>) {
        output.extend_from_slice(&parent.to_le_bytes());
        output.extend_from_slice(
            &u64::try_from(key.len())
                .expect("test key length fits u64")
                .to_le_bytes(),
        );
        output.extend_from_slice(key);
        output.push(node_type as u8);
    }

    fn section(nodes: &[(i32, &[u8], NodeType)]) -> Vec<u8> {
        let mut decompressed = Vec::new();
        decompressed.extend_from_slice(
            &u64::try_from(nodes.len())
                .expect("test node count fits u64")
                .to_le_bytes(),
        );
        for &(parent, key, node_type) in nodes {
            node(parent, key, node_type, &mut decompressed);
        }
        zstd::stream::encode_all(decompressed.as_slice(), 3).expect("compress test schema tree")
    }

    fn decode(bytes: &[u8], limits: SchemaTreeLimits) -> Result<SchemaTree, SchemaTreeError> {
        let mut source = Cursor::new(bytes);
        let compressed_size = u64::try_from(bytes.len()).expect("test section length fits u64");
        decode_schema_tree(source.by_ref().take(compressed_size), limits)
    }

    #[test]
    fn decodes_valid_nodes_and_preserves_implicit_ids() {
        let compressed = section(&[
            (-1, b"", NodeType::Object),
            (0, b"count", NodeType::Integer),
            (-1, b"", NodeType::Metadata),
            (2, b"log_event_idx", NodeType::DeltaInteger),
        ]);

        let tree = decode(&compressed, SchemaTreeLimits::default()).expect("valid schema tree");

        assert_eq!(4, tree.len());
        assert_eq!(None, tree.get(0).expect("root").parent_id());
        assert_eq!(Some(0), tree.get(1).expect("count").parent_id());
        assert_eq!(b"count", tree.get(1).expect("count").key_bytes());
        assert_eq!(Ok("count"), tree.get(1).expect("count").key_str());
        assert_eq!(
            NodeType::DeltaInteger,
            tree.get(3).expect("log event index").node_type()
        );
    }

    #[test]
    fn preserves_invalid_utf8_key_bytes() {
        let compressed = section(&[(-1, &[0xff], NodeType::Object)]);
        let tree = decode(&compressed, SchemaTreeLimits::default()).expect("binary key is bounded");
        let root = tree.get(0).expect("root");

        assert_eq!([0xff], root.key_bytes());
        assert!(root.key_str().is_err());
    }

    #[test]
    fn rejects_parent_that_does_not_precede_child() {
        let compressed = section(&[(-1, b"", NodeType::Object), (1, b"x", NodeType::Integer)]);

        assert!(matches!(
            decode(&compressed, SchemaTreeLimits::default()),
            Err(SchemaTreeError::ParentDoesNotPrecedeNode {
                node_id: 1,
                parent: 1
            })
        ));
    }

    #[test]
    fn rejects_parent_below_root_sentinel() {
        let compressed = section(&[(-2, b"", NodeType::Object)]);

        assert!(matches!(
            decode(&compressed, SchemaTreeLimits::default()),
            Err(SchemaTreeError::InvalidParent {
                node_id: 0,
                actual: -2
            })
        ));
    }

    #[test]
    fn rejects_duplicate_node_identity() {
        let compressed = section(&[
            (-1, b"", NodeType::Object),
            (0, b"x", NodeType::Integer),
            (0, b"x", NodeType::Integer),
        ]);

        assert!(matches!(
            decode(&compressed, SchemaTreeLimits::default()),
            Err(SchemaTreeError::DuplicateNode {
                node_id: 2,
                previous_node_id: 1
            })
        ));
    }

    #[test]
    fn permits_same_key_when_type_differs() {
        let compressed = section(&[
            (-1, b"", NodeType::Object),
            (0, b"value", NodeType::Integer),
            (0, b"value", NodeType::Float),
        ]);

        assert_eq!(
            3,
            decode(&compressed, SchemaTreeLimits::default())
                .expect("type is part of identity")
                .len()
        );
    }

    #[test]
    fn enforces_node_and_key_limits_before_allocation() {
        let compressed = section(&[(-1, b"abcd", NodeType::Object)]);
        let node_limit = SchemaTreeLimits::new(u64::MAX, u64::MAX, 0, u64::MAX, u64::MAX);
        assert!(matches!(
            decode(&compressed, node_limit),
            Err(SchemaTreeError::NodeCountTooLarge {
                actual: 1,
                limit: 0
            })
        ));

        let key_limit = SchemaTreeLimits::new(u64::MAX, u64::MAX, 1, 3, u64::MAX);
        assert!(matches!(
            decode(&compressed, key_limit),
            Err(SchemaTreeError::KeyTooLarge {
                node_id: 0,
                actual: 4,
                limit: 3
            })
        ));
    }

    #[test]
    fn rejects_unknown_node_type() {
        let mut decompressed = 1_u64.to_le_bytes().to_vec();
        decompressed.extend_from_slice(&(-1_i32).to_le_bytes());
        decompressed.extend_from_slice(&0_u64.to_le_bytes());
        decompressed.push(255);
        let compressed =
            zstd::stream::encode_all(decompressed.as_slice(), 3).expect("compress invalid tree");

        assert!(matches!(
            decode(&compressed, SchemaTreeLimits::default()),
            Err(SchemaTreeError::UnknownNodeType {
                node_id: 0,
                source
            }) if 255 == source.value()
        ));
    }

    #[test]
    fn rejects_trailing_decompressed_data() {
        let mut decompressed = 0_u64.to_le_bytes().to_vec();
        decompressed.push(1);
        let compressed =
            zstd::stream::encode_all(decompressed.as_slice(), 3).expect("compress invalid tree");

        assert!(matches!(
            decode(&compressed, SchemaTreeLimits::default()),
            Err(SchemaTreeError::TrailingDecompressedData)
        ));
    }

    #[test]
    fn rejects_bytes_after_the_zstd_frame() {
        let mut compressed = section(&[]);
        compressed.extend_from_slice(b"trailing");

        assert!(matches!(
            decode(&compressed, SchemaTreeLimits::default()),
            Err(SchemaTreeError::TrailingCompressedData { .. })
        ));
    }
}
