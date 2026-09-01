use std::error::Error;
use std::fmt;
use std::fmt::Display;
use std::fmt::Formatter;
use std::iter::FusedIterator;

use crate::archive::ColumnData;
use crate::archive::NodeType;
use crate::archive::SchemaDefinition;
use crate::archive::SchemaEntry;
use crate::archive::SchemaTable;
use crate::archive::SchemaTree;

const I64_SIZE: usize = size_of::<i64>();

/// Wire key of the archive-local log-event index in the default metadata namespace.
///
/// Documentation sometimes writes the qualified field as `_log_event_idx`; the leading
/// underscore denotes the metadata namespace and is not stored in the schema-tree key.
pub const LOG_EVENT_IDX_KEY: &[u8] = b"log_event_idx";

/// The canonical archive-level location of `_log_event_idx`.
///
/// Discover this once per [`SchemaTree`], then use [`Self::locate`] for each decoded schema table.
/// An absent locator means the archive does not advertise log-order information. An absent column
/// returned by `locate` means that particular schema does not contain the advertised field.
#[derive(Clone, Copy, Debug)]
pub struct LogOrderLocator<'tree> {
    schema_tree: &'tree SchemaTree,
    metadata_root_node_id: u32,
    node_id: u32,
}

impl<'tree> LogOrderLocator<'tree> {
    /// Discovers the canonical default metadata root and its direct `log_event_idx` child.
    ///
    /// # Errors
    ///
    /// Returns a typed error for duplicate default metadata roots, duplicate reserved children,
    /// a reserved child whose type is not [`NodeType::DeltaInteger`], or an index conversion
    /// overflow. A missing metadata root or missing reserved child is valid and returns `None`.
    pub fn discover(schema_tree: &'tree SchemaTree) -> Result<Option<Self>, LogOrderError> {
        let Some(nodes) = discover_nodes(schema_tree)? else {
            return Ok(None);
        };
        Ok(Some(Self {
            schema_tree,
            metadata_root_node_id: nodes.metadata_root_node_id,
            node_id: nodes.node_id,
        }))
    }

    /// Returns the default metadata namespace root's schema-tree node ID.
    #[must_use]
    pub const fn metadata_root_node_id(self) -> u32 {
        self.metadata_root_node_id
    }

    /// Returns the canonical `log_event_idx` schema-tree node ID.
    #[must_use]
    pub const fn node_id(self) -> u32 {
        self.node_id
    }

    /// Validates a schema/table pair and locates its zero-copy log-order column.
    ///
    /// # Errors
    ///
    /// Returns a typed error when the schema references an unknown node, its ordered boundary is
    /// invalid, the reserved field is repeated or placed in the unordered region, the table does
    /// not correspond exactly to the schema's value-bearing entries, or the delta byte length
    /// disagrees with the table's message count.
    pub fn locate<'table>(
        self,
        schema: &SchemaDefinition,
        table: &SchemaTable<'table, '_>,
    ) -> Result<Option<LogOrderColumn<'table>>, LogOrderError> {
        locate_in_table(
            self.schema_tree,
            schema.id(),
            schema.entries(),
            schema.ordered_entry_count(),
            table,
            Some(LocatedNodes {
                metadata_root_node_id: self.metadata_root_node_id,
                node_id: self.node_id,
            }),
        )
    }
}

/// Discovers and locates `_log_event_idx` while validating the supplied schema/table pair.
///
/// This convenience function validates table correspondence even when the archive has no
/// canonical log-order field. Call [`LogOrderLocator::discover`] directly when processing many
/// tables so the schema tree is scanned only once.
///
/// # Errors
///
/// Returns any archive-level discovery or table-level correspondence error described by
/// [`LogOrderError`].
pub fn locate_log_order_column<'table>(
    schema_tree: &SchemaTree,
    schema: &SchemaDefinition,
    table: &SchemaTable<'table, '_>,
) -> Result<Option<LogOrderColumn<'table>>, LogOrderError> {
    let nodes = discover_nodes(schema_tree)?;
    locate_in_table(
        schema_tree,
        schema.id(),
        schema.entries(),
        schema.ordered_entry_count(),
        table,
        nodes,
    )
}

/// A validated zero-copy `_log_event_idx` column in one schema table.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LogOrderColumn<'table> {
    schema_id: i32,
    node_id: u32,
    schema_entry_index: usize,
    column_index: usize,
    message_count: usize,
    encoded_deltas: &'table [u8],
}

impl<'table> LogOrderColumn<'table> {
    /// Returns the opaque schema ID of the containing table.
    #[must_use]
    pub const fn schema_id(self) -> i32 {
        self.schema_id
    }

    /// Returns the reserved field's schema-tree node ID.
    #[must_use]
    pub const fn node_id(self) -> u32 {
        self.node_id
    }

    /// Returns the reserved field's index in the flattened schema definition.
    #[must_use]
    pub const fn schema_entry_index(self) -> usize {
        self.schema_entry_index
    }

    /// Returns the stable table-local physical column index.
    #[must_use]
    pub const fn column_index(self) -> usize {
        self.column_index
    }

    /// Returns the number of log-event indexes.
    #[must_use]
    pub const fn len(self) -> usize {
        self.message_count
    }

    /// Returns whether the table contains no records.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        0 == self.message_count
    }

    /// Returns the exact little-endian delta bytes borrowed from the schema table.
    #[must_use]
    pub const fn encoded_deltas(self) -> &'table [u8] {
        self.encoded_deltas
    }

    /// Creates an independent forward cursor over reconstructed archive-local indexes.
    ///
    /// Each cursor advances through the encoded deltas exactly once. It never uses the
    /// random-access delta-column accessor, so consuming all values is linear time.
    #[must_use]
    pub fn cursor(self) -> LogOrderCursor<'table> {
        LogOrderCursor {
            deltas: self.encoded_deltas.as_chunks::<I64_SIZE>().0.iter(),
            row_count: self.message_count,
            current: 0,
        }
    }
}

/// A cloneable, zero-allocation, forward cursor over one table's log-event indexes.
///
/// A later heap merge can hold one cursor per decoded table. Before calling `next`,
/// [`Self::next_row_index`] identifies the row whose index will be returned.
#[derive(Clone, Debug)]
pub struct LogOrderCursor<'table> {
    deltas: std::slice::Iter<'table, [u8; I64_SIZE]>,
    row_count: usize,
    current: i64,
}

impl LogOrderCursor<'_> {
    /// Returns the row index that the next successful call to [`Iterator::next`] consumes.
    ///
    /// Once exhausted, this equals the table's message count.
    #[must_use]
    pub fn next_row_index(&self) -> usize {
        self.row_count - self.deltas.len()
    }

    /// Returns the number of rows already consumed by this cursor.
    #[must_use]
    pub fn rows_consumed(&self) -> usize {
        self.next_row_index()
    }
}

impl Iterator for LogOrderCursor<'_> {
    type Item = i64;

    fn next(&mut self) -> Option<Self::Item> {
        let delta = i64::from_le_bytes(*self.deltas.next()?);
        // SchemaTable rejects delta overflow while decoding. Wrapping addition here therefore has
        // identical results for every constructible public LogOrderColumn and avoids a Result in
        // the hot merge loop.
        self.current = self.current.wrapping_add(delta);
        Some(self.current)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.deltas.size_hint()
    }
}

impl ExactSizeIterator for LogOrderCursor<'_> {}
impl FusedIterator for LogOrderCursor<'_> {}

/// Failure to discover or locate canonical log-order information.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum LogOrderError {
    /// Checked size arithmetic or conversion overflowed.
    SizeOverflow,
    /// More than one root claims the default metadata namespace.
    DuplicateMetadataRoots {
        /// First matching root node.
        first_node_id: u32,
        /// Later matching root node.
        second_node_id: u32,
    },
    /// More than one direct metadata child uses the reserved wire key.
    DuplicateLogEventIndexNodes {
        /// Default metadata root.
        metadata_root_node_id: u32,
        /// First matching child.
        first_node_id: u32,
        /// Later matching child.
        second_node_id: u32,
    },
    /// The reserved metadata child is not delta encoded.
    WrongLogEventIndexNodeType {
        /// Reserved child node.
        node_id: u32,
        /// Actual schema-tree type.
        actual: NodeType,
    },
    /// The schema's ordered-entry boundary exceeds its complete entry count.
    OrderedEntryCountOutOfBounds {
        /// Opaque schema ID.
        schema_id: i32,
        /// Ordered entries.
        ordered: usize,
        /// Total flattened entries.
        total: usize,
    },
    /// A schema entry references a node absent from the supplied schema tree.
    UnknownSchemaNode {
        /// Opaque schema ID.
        schema_id: i32,
        /// Flattened schema-entry index.
        schema_entry_index: usize,
        /// Missing node ID.
        node_id: u32,
        /// Supplied tree size.
        node_count: usize,
    },
    /// The number of table columns differs from the schema's value-bearing entries.
    ColumnCountMismatch {
        /// Opaque schema ID.
        schema_id: i32,
        /// Columns implied by the schema.
        expected: usize,
        /// Columns in the table.
        actual: usize,
    },
    /// A table reports enough columns but cannot return one at the requested index.
    MissingTableColumn {
        /// Opaque schema ID.
        schema_id: i32,
        /// Missing table-local column index.
        column_index: usize,
    },
    /// A table column records the wrong flattened schema-entry index.
    ColumnSchemaEntryMismatch {
        /// Opaque schema ID.
        schema_id: i32,
        /// Table-local column index.
        column_index: usize,
        /// Expected flattened entry.
        expected: usize,
        /// Column's recorded flattened entry.
        actual: usize,
    },
    /// A table column records the wrong schema-tree node ID.
    ColumnNodeMismatch {
        /// Opaque schema ID.
        schema_id: i32,
        /// Table-local column index.
        column_index: usize,
        /// Expected schema-tree node.
        expected: u32,
        /// Column's recorded schema-tree node.
        actual: u32,
    },
    /// A table column's data variant disagrees with the schema-tree node type.
    ColumnNodeTypeMismatch {
        /// Opaque schema ID.
        schema_id: i32,
        /// Table-local column index.
        column_index: usize,
        /// Expected node type.
        expected: NodeType,
        /// Actual column type.
        actual: NodeType,
    },
    /// The reserved schema-tree node occurs more than once in one schema.
    DuplicateLogEventIndexEntries {
        /// Opaque schema ID.
        schema_id: i32,
        /// First occurrence.
        first_entry_index: usize,
        /// Later occurrence.
        second_entry_index: usize,
    },
    /// The reserved node occurs outside the schema's ordered region.
    LogEventIndexOutsideOrderedRegion {
        /// Opaque schema ID.
        schema_id: i32,
        /// Flattened schema-entry index.
        schema_entry_index: usize,
        /// Ordered-region exclusive end.
        ordered_entry_count: usize,
    },
    /// The reserved column is not represented by delta-encoded signed integers.
    MissingDeltaRepresentation {
        /// Opaque schema ID.
        schema_id: i32,
        /// Table-local column index.
        column_index: usize,
    },
    /// The reserved column byte length disagrees with the table's message count.
    LogEventIndexLengthMismatch {
        /// Opaque schema ID.
        schema_id: i32,
        /// Table-local column index.
        column_index: usize,
        /// Expected delta bytes.
        expected_bytes: usize,
        /// Actual delta bytes.
        actual_bytes: usize,
    },
}

impl Display for LogOrderError {
    // Exhaustive, field-specific diagnostics keep every public corruption variant actionable.
    #[allow(clippy::too_many_lines)]
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::SizeOverflow => formatter.write_str("log-order size overflow"),
            Self::DuplicateMetadataRoots {
                first_node_id,
                second_node_id,
            } => write!(
                formatter,
                "schema tree has duplicate default metadata roots {first_node_id} and \
                 {second_node_id}"
            ),
            Self::DuplicateLogEventIndexNodes {
                metadata_root_node_id,
                first_node_id,
                second_node_id,
            } => write!(
                formatter,
                "metadata root {metadata_root_node_id} has duplicate log_event_idx children \
                 {first_node_id} and {second_node_id}"
            ),
            Self::WrongLogEventIndexNodeType { node_id, actual } => write!(
                formatter,
                "metadata node {node_id} names log_event_idx but has type {actual:?}"
            ),
            Self::OrderedEntryCountOutOfBounds {
                schema_id,
                ordered,
                total,
            } => write!(
                formatter,
                "schema {schema_id} has {ordered} ordered entries but only {total} total entries"
            ),
            Self::UnknownSchemaNode {
                schema_id,
                schema_entry_index,
                node_id,
                node_count,
            } => write!(
                formatter,
                "schema {schema_id} entry {schema_entry_index} references node {node_id}, but the \
                 tree has {node_count} nodes"
            ),
            Self::ColumnCountMismatch {
                schema_id,
                expected,
                actual,
            } => write!(
                formatter,
                "schema {schema_id} implies {expected} columns but its table has {actual}"
            ),
            Self::MissingTableColumn {
                schema_id,
                column_index,
            } => write!(
                formatter,
                "schema {schema_id} table cannot return advertised column {column_index}"
            ),
            Self::ColumnSchemaEntryMismatch {
                schema_id,
                column_index,
                expected,
                actual,
            } => write!(
                formatter,
                "schema {schema_id} column {column_index} records entry {actual}, expected \
                 {expected}"
            ),
            Self::ColumnNodeMismatch {
                schema_id,
                column_index,
                expected,
                actual,
            } => write!(
                formatter,
                "schema {schema_id} column {column_index} records node {actual}, expected \
                 {expected}"
            ),
            Self::ColumnNodeTypeMismatch {
                schema_id,
                column_index,
                expected,
                actual,
            } => write!(
                formatter,
                "schema {schema_id} column {column_index} has type {actual:?}, expected \
                 {expected:?}"
            ),
            Self::DuplicateLogEventIndexEntries {
                schema_id,
                first_entry_index,
                second_entry_index,
            } => write!(
                formatter,
                "schema {schema_id} repeats log_event_idx at entries {first_entry_index} and \
                 {second_entry_index}"
            ),
            Self::LogEventIndexOutsideOrderedRegion {
                schema_id,
                schema_entry_index,
                ordered_entry_count,
            } => write!(
                formatter,
                "schema {schema_id} places log_event_idx at entry {schema_entry_index}, outside \
                 ordered end {ordered_entry_count}"
            ),
            Self::MissingDeltaRepresentation {
                schema_id,
                column_index,
            } => write!(
                formatter,
                "schema {schema_id} log-order column {column_index} has no delta representation"
            ),
            Self::LogEventIndexLengthMismatch {
                schema_id,
                column_index,
                expected_bytes,
                actual_bytes,
            } => write!(
                formatter,
                "schema {schema_id} log-order column {column_index} has {actual_bytes} bytes, \
                 expected {expected_bytes}"
            ),
        }
    }
}

impl Error for LogOrderError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct LocatedNodes {
    metadata_root_node_id: u32,
    node_id: u32,
}

#[derive(Clone, Copy)]
struct NodeView<'a> {
    parent: Option<usize>,
    key: &'a [u8],
    node_type: NodeType,
}

trait TreeView {
    fn len(&self) -> usize;
    fn node(&self, node_id: usize) -> Option<NodeView<'_>>;
}

impl TreeView for SchemaTree {
    fn len(&self) -> usize {
        Self::len(self)
    }

    fn node(&self, node_id: usize) -> Option<NodeView<'_>> {
        let node = self.get(node_id)?;
        Some(NodeView {
            parent: node.parent_id(),
            key: node.key_bytes(),
            node_type: node.node_type(),
        })
    }
}

#[derive(Clone, Copy)]
struct TableColumnView<'table> {
    schema_entry_index: usize,
    node_id: u32,
    node_type: NodeType,
    encoded_deltas: Option<&'table [u8]>,
}

trait TableView<'table> {
    fn message_count(&self) -> usize;
    fn len(&self) -> usize;
    fn column(&self, column_index: usize) -> Option<TableColumnView<'table>>;
}

impl<'table> TableView<'table> for SchemaTable<'table, '_> {
    fn message_count(&self) -> usize {
        Self::message_count(self)
    }

    fn len(&self) -> usize {
        Self::len(self)
    }

    fn column(&self, column_index: usize) -> Option<TableColumnView<'table>> {
        let column = *Self::column(self, column_index)?;
        let data = column.data();
        Some(TableColumnView {
            schema_entry_index: column.schema_entry_index(),
            node_id: column.node_id(),
            node_type: column.node_type(),
            encoded_deltas: match data {
                ColumnData::DeltaInteger(values) => Some(values.deltas().encoded_bytes()),
                _ => None,
            },
        })
    }
}

fn discover_nodes<T: TreeView + ?Sized>(
    schema_tree: &T,
) -> Result<Option<LocatedNodes>, LogOrderError> {
    let mut metadata_root = None;
    for node_index in 0..schema_tree.len() {
        let node = require_node(schema_tree, node_index, None, 0)?;
        if node.parent.is_none() && NodeType::Metadata == node.node_type && node.key.is_empty() {
            let node_id = to_node_id(node_index)?;
            if let Some(first_node_id) = metadata_root.replace(node_id) {
                return Err(LogOrderError::DuplicateMetadataRoots {
                    first_node_id,
                    second_node_id: node_id,
                });
            }
        }
    }
    let Some(metadata_root_node_id) = metadata_root else {
        return Ok(None);
    };
    let metadata_root_index =
        usize::try_from(metadata_root_node_id).map_err(|_| LogOrderError::SizeOverflow)?;

    let mut field = None;
    for node_index in 0..schema_tree.len() {
        let node = require_node(schema_tree, node_index, None, 0)?;
        if node.parent == Some(metadata_root_index) && node.key == LOG_EVENT_IDX_KEY {
            let node_id = to_node_id(node_index)?;
            if let Some(first_node_id) = field.replace(node_id) {
                return Err(LogOrderError::DuplicateLogEventIndexNodes {
                    metadata_root_node_id,
                    first_node_id,
                    second_node_id: node_id,
                });
            }
        }
    }
    let Some(node_id) = field else {
        return Ok(None);
    };
    let field_node = require_node(
        schema_tree,
        usize::try_from(node_id).map_err(|_| LogOrderError::SizeOverflow)?,
        None,
        0,
    )?;
    if NodeType::DeltaInteger != field_node.node_type {
        return Err(LogOrderError::WrongLogEventIndexNodeType {
            node_id,
            actual: field_node.node_type,
        });
    }

    Ok(Some(LocatedNodes {
        metadata_root_node_id,
        node_id,
    }))
}

fn locate_in_table<'table, T: TreeView + ?Sized, V: TableView<'table> + ?Sized>(
    schema_tree: &T,
    schema_id: i32,
    entries: &[SchemaEntry],
    ordered_entry_count: usize,
    table: &V,
    nodes: Option<LocatedNodes>,
) -> Result<Option<LogOrderColumn<'table>>, LogOrderError> {
    if ordered_entry_count > entries.len() {
        return Err(LogOrderError::OrderedEntryCountOutOfBounds {
            schema_id,
            ordered: ordered_entry_count,
            total: entries.len(),
        });
    }

    let expected_column_count = count_value_columns(schema_tree, schema_id, entries)?;
    if expected_column_count != table.len() {
        return Err(LogOrderError::ColumnCountMismatch {
            schema_id,
            expected: expected_column_count,
            actual: table.len(),
        });
    }

    let target_node_id = nodes.map(|location| location.node_id);
    let mut column_index = 0_usize;
    let mut first_target_entry = None;
    let mut located = None;
    for (schema_entry_index, entry) in entries.iter().copied().enumerate() {
        let SchemaEntry::Node(node_id) = entry else {
            continue;
        };
        let node = require_node(
            schema_tree,
            usize::try_from(node_id).map_err(|_| LogOrderError::SizeOverflow)?,
            Some(schema_entry_index),
            schema_id,
        )?;
        if !is_value_bearing(node.node_type) {
            continue;
        }

        let column = table
            .column(column_index)
            .ok_or(LogOrderError::MissingTableColumn {
                schema_id,
                column_index,
            })?;
        validate_column(
            schema_id,
            column_index,
            schema_entry_index,
            node_id,
            node.node_type,
            column,
        )?;

        if Some(node_id) == target_node_id {
            if let Some(first_entry_index) = first_target_entry.replace(schema_entry_index) {
                return Err(LogOrderError::DuplicateLogEventIndexEntries {
                    schema_id,
                    first_entry_index,
                    second_entry_index: schema_entry_index,
                });
            }
            if schema_entry_index >= ordered_entry_count {
                return Err(LogOrderError::LogEventIndexOutsideOrderedRegion {
                    schema_id,
                    schema_entry_index,
                    ordered_entry_count,
                });
            }
            let encoded_deltas =
                column
                    .encoded_deltas
                    .ok_or(LogOrderError::MissingDeltaRepresentation {
                        schema_id,
                        column_index,
                    })?;
            let expected_bytes = table
                .message_count()
                .checked_mul(I64_SIZE)
                .ok_or(LogOrderError::SizeOverflow)?;
            if expected_bytes != encoded_deltas.len() {
                return Err(LogOrderError::LogEventIndexLengthMismatch {
                    schema_id,
                    column_index,
                    expected_bytes,
                    actual_bytes: encoded_deltas.len(),
                });
            }
            located = Some(LogOrderColumn {
                schema_id,
                node_id,
                schema_entry_index,
                column_index,
                message_count: table.message_count(),
                encoded_deltas,
            });
        }
        column_index = column_index
            .checked_add(1)
            .ok_or(LogOrderError::SizeOverflow)?;
    }
    Ok(located)
}

fn count_value_columns<T: TreeView + ?Sized>(
    schema_tree: &T,
    schema_id: i32,
    entries: &[SchemaEntry],
) -> Result<usize, LogOrderError> {
    let mut column_count = 0_usize;
    for (schema_entry_index, entry) in entries.iter().copied().enumerate() {
        let SchemaEntry::Node(node_id) = entry else {
            continue;
        };
        let node = require_node(
            schema_tree,
            usize::try_from(node_id).map_err(|_| LogOrderError::SizeOverflow)?,
            Some(schema_entry_index),
            schema_id,
        )?;
        if is_value_bearing(node.node_type) {
            column_count = column_count
                .checked_add(1)
                .ok_or(LogOrderError::SizeOverflow)?;
        }
    }
    Ok(column_count)
}

fn validate_column(
    schema_id: i32,
    column_index: usize,
    expected_entry_index: usize,
    expected_node_id: u32,
    expected_node_type: NodeType,
    column: TableColumnView<'_>,
) -> Result<(), LogOrderError> {
    if expected_entry_index != column.schema_entry_index {
        return Err(LogOrderError::ColumnSchemaEntryMismatch {
            schema_id,
            column_index,
            expected: expected_entry_index,
            actual: column.schema_entry_index,
        });
    }
    if expected_node_id != column.node_id {
        return Err(LogOrderError::ColumnNodeMismatch {
            schema_id,
            column_index,
            expected: expected_node_id,
            actual: column.node_id,
        });
    }
    if expected_node_type != column.node_type {
        return Err(LogOrderError::ColumnNodeTypeMismatch {
            schema_id,
            column_index,
            expected: expected_node_type,
            actual: column.node_type,
        });
    }
    Ok(())
}

fn require_node<T: TreeView + ?Sized>(
    schema_tree: &T,
    node_index: usize,
    schema_entry_index: Option<usize>,
    schema_id: i32,
) -> Result<NodeView<'_>, LogOrderError> {
    schema_tree
        .node(node_index)
        .ok_or(LogOrderError::UnknownSchemaNode {
            schema_id,
            schema_entry_index: schema_entry_index.unwrap_or(0),
            node_id: to_node_id(node_index)?,
            node_count: schema_tree.len(),
        })
}

const fn is_value_bearing(node_type: NodeType) -> bool {
    !matches!(
        node_type,
        NodeType::Object | NodeType::Null | NodeType::StructuredArray | NodeType::Metadata
    )
}

fn to_node_id(node_index: usize) -> Result<u32, LogOrderError> {
    u32::try_from(node_index).map_err(|_| LogOrderError::SizeOverflow)
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::ExtractionOp;
    use crate::ExtractionPlan;
    use crate::ExtractionPlanLimits;
    use crate::archive::ArchiveCatalogLimits;
    use crate::archive::ColumnLimits;
    use crate::archive::PackedStreamLimits;
    use crate::archive::SingleFileArchiveReader;

    const CPP_FIXTURE: &[u8] = include_bytes!("../tests/fixtures/sfa-v0.5.0-minimal-cpp.bin");

    #[derive(Clone, Copy)]
    struct TestNode {
        parent: Option<usize>,
        key: &'static [u8],
        node_type: NodeType,
    }

    impl TestNode {
        const fn new(parent: Option<usize>, key: &'static [u8], node_type: NodeType) -> Self {
            Self {
                parent,
                key,
                node_type,
            }
        }
    }

    struct TestTree<'a>(&'a [TestNode]);

    impl TreeView for TestTree<'_> {
        fn len(&self) -> usize {
            self.0.len()
        }

        fn node(&self, node_id: usize) -> Option<NodeView<'_>> {
            let node = self.0.get(node_id)?;
            Some(NodeView {
                parent: node.parent,
                key: node.key,
                node_type: node.node_type,
            })
        }
    }

    struct TestTable<'table> {
        message_count: usize,
        columns: &'table [TableColumnView<'table>],
    }

    impl<'table> TableView<'table> for TestTable<'table> {
        fn message_count(&self) -> usize {
            self.message_count
        }

        fn len(&self) -> usize {
            self.columns.len()
        }

        fn column(&self, column_index: usize) -> Option<TableColumnView<'table>> {
            self.columns.get(column_index).copied()
        }
    }

    const fn node(node_id: u32) -> SchemaEntry {
        SchemaEntry::Node(node_id)
    }

    fn encode_deltas(deltas: &[i64]) -> Vec<u8> {
        deltas
            .iter()
            .flat_map(|delta| delta.to_le_bytes())
            .collect()
    }

    fn canonical_tree() -> [TestNode; 5] {
        [
            TestNode::new(None, b"", NodeType::Metadata),
            TestNode::new(Some(0), LOG_EVENT_IDX_KEY, NodeType::DeltaInteger),
            TestNode::new(None, b"", NodeType::Object),
            TestNode::new(Some(2), b"value", NodeType::Integer),
            TestNode::new(Some(2), b"active", NodeType::Boolean),
        ]
    }

    #[test]
    fn locates_nonzero_columns_across_schemas_and_preserves_wire_values() {
        let tree = canonical_tree();
        let tree = TestTree(&tree);
        let nodes = discover_nodes(&tree)
            .expect("valid metadata")
            .expect("log order present");
        assert_eq!(0, nodes.metadata_root_node_id);
        assert_eq!(1, nodes.node_id);

        let first_deltas = encode_deltas(&[10, -15, 3]);
        let first_columns = [
            TableColumnView {
                schema_entry_index: 0,
                node_id: 3,
                node_type: NodeType::Integer,
                encoded_deltas: None,
            },
            TableColumnView {
                schema_entry_index: 1,
                node_id: 1,
                node_type: NodeType::DeltaInteger,
                encoded_deltas: Some(&first_deltas),
            },
        ];
        let first_table = TestTable {
            message_count: 3,
            columns: &first_columns,
        };
        let first = locate_in_table(&tree, 41, &[node(3), node(1)], 2, &first_table, Some(nodes))
            .expect("valid first schema")
            .expect("first schema has log order");
        assert_eq!(41, first.schema_id());
        assert_eq!(1, first.schema_entry_index());
        assert_eq!(1, first.column_index());
        assert_eq!(3, first.len());

        let mut cursor = first.cursor();
        assert_eq!(0, cursor.next_row_index());
        assert_eq!(Some(10), cursor.next());
        assert_eq!(1, cursor.rows_consumed());
        assert_eq!(Some(-5), cursor.next());
        assert_eq!(Some(-2), cursor.next());
        assert_eq!(3, cursor.next_row_index());
        assert_eq!(None, cursor.next());
        assert_eq!(None, cursor.next());

        let second_deltas = encode_deltas(&[-9, -11]);
        let second_columns = [
            TableColumnView {
                schema_entry_index: 0,
                node_id: 1,
                node_type: NodeType::DeltaInteger,
                encoded_deltas: Some(&second_deltas),
            },
            TableColumnView {
                schema_entry_index: 1,
                node_id: 4,
                node_type: NodeType::Boolean,
                encoded_deltas: None,
            },
        ];
        let second_table = TestTable {
            message_count: 2,
            columns: &second_columns,
        };
        let second = locate_in_table(
            &tree,
            -7,
            &[node(1), node(4)],
            2,
            &second_table,
            Some(nodes),
        )
        .expect("valid second schema")
        .expect("second schema has log order");
        assert_eq!(0, second.column_index());
        assert_eq!(vec![-9, -20], second.cursor().collect::<Vec<_>>());
    }

    #[test]
    fn distinguishes_absence_from_wrong_and_duplicate_metadata() {
        let no_metadata = [
            TestNode::new(None, b"", NodeType::Object),
            TestNode::new(Some(0), b"value", NodeType::Integer),
        ];
        assert_eq!(
            None,
            discover_nodes(&TestTree(&no_metadata)).expect("metadata absence is valid")
        );

        let wrong_type = [
            TestNode::new(None, b"", NodeType::Metadata),
            TestNode::new(Some(0), LOG_EVENT_IDX_KEY, NodeType::Integer),
        ];
        assert_eq!(
            LogOrderError::WrongLogEventIndexNodeType {
                node_id: 1,
                actual: NodeType::Integer,
            },
            discover_nodes(&TestTree(&wrong_type)).expect_err("wrong reserved type")
        );

        let duplicate_field = [
            TestNode::new(None, b"", NodeType::Metadata),
            TestNode::new(Some(0), LOG_EVENT_IDX_KEY, NodeType::Integer),
            TestNode::new(Some(0), LOG_EVENT_IDX_KEY, NodeType::DeltaInteger),
        ];
        assert_eq!(
            LogOrderError::DuplicateLogEventIndexNodes {
                metadata_root_node_id: 0,
                first_node_id: 1,
                second_node_id: 2,
            },
            discover_nodes(&TestTree(&duplicate_field)).expect_err("duplicate reserved child")
        );

        let duplicate_root = [
            TestNode::new(None, b"", NodeType::Metadata),
            TestNode::new(None, b"", NodeType::Metadata),
        ];
        assert_eq!(
            LogOrderError::DuplicateMetadataRoots {
                first_node_id: 0,
                second_node_id: 1,
            },
            discover_nodes(&TestTree(&duplicate_root)).expect_err("duplicate metadata roots")
        );
    }

    #[test]
    fn table_absence_is_valid_but_malformed_placement_is_not() {
        let tree = canonical_tree();
        let tree = TestTree(&tree);
        let nodes = discover_nodes(&tree)
            .expect("valid metadata")
            .expect("log order present");

        let ordinary_columns = [TableColumnView {
            schema_entry_index: 0,
            node_id: 3,
            node_type: NodeType::Integer,
            encoded_deltas: None,
        }];
        let ordinary_table = TestTable {
            message_count: 2,
            columns: &ordinary_columns,
        };
        assert_eq!(
            None,
            locate_in_table(&tree, 2, &[node(3)], 1, &ordinary_table, Some(nodes))
                .expect("schema without metadata is valid")
        );

        let deltas = encode_deltas(&[0, 1]);
        let unordered_columns = [
            TableColumnView {
                schema_entry_index: 0,
                node_id: 3,
                node_type: NodeType::Integer,
                encoded_deltas: None,
            },
            TableColumnView {
                schema_entry_index: 1,
                node_id: 1,
                node_type: NodeType::DeltaInteger,
                encoded_deltas: Some(&deltas),
            },
        ];
        let unordered_table = TestTable {
            message_count: 2,
            columns: &unordered_columns,
        };
        assert_eq!(
            LogOrderError::LogEventIndexOutsideOrderedRegion {
                schema_id: 3,
                schema_entry_index: 1,
                ordered_entry_count: 1,
            },
            locate_in_table(
                &tree,
                3,
                &[node(3), node(1)],
                1,
                &unordered_table,
                Some(nodes),
            )
            .expect_err("reserved field must be ordered")
        );

        let duplicate_columns = [
            TableColumnView {
                schema_entry_index: 0,
                node_id: 1,
                node_type: NodeType::DeltaInteger,
                encoded_deltas: Some(&deltas),
            },
            TableColumnView {
                schema_entry_index: 1,
                node_id: 1,
                node_type: NodeType::DeltaInteger,
                encoded_deltas: Some(&deltas),
            },
        ];
        let duplicate_table = TestTable {
            message_count: 2,
            columns: &duplicate_columns,
        };
        assert_eq!(
            LogOrderError::DuplicateLogEventIndexEntries {
                schema_id: 4,
                first_entry_index: 0,
                second_entry_index: 1,
            },
            locate_in_table(
                &tree,
                4,
                &[node(1), node(1)],
                2,
                &duplicate_table,
                Some(nodes),
            )
            .expect_err("reserved field cannot repeat")
        );
    }

    #[test]
    fn validates_schema_table_correspondence_and_delta_length() {
        let tree = canonical_tree();
        let tree = TestTree(&tree);
        let nodes = discover_nodes(&tree)
            .expect("valid metadata")
            .expect("log order present");
        let deltas = encode_deltas(&[0]);

        let wrong_column = [TableColumnView {
            schema_entry_index: 0,
            node_id: 4,
            node_type: NodeType::Boolean,
            encoded_deltas: None,
        }];
        let wrong_table = TestTable {
            message_count: 1,
            columns: &wrong_column,
        };
        assert_eq!(
            LogOrderError::ColumnNodeMismatch {
                schema_id: 8,
                column_index: 0,
                expected: 3,
                actual: 4,
            },
            locate_in_table(&tree, 8, &[node(3)], 1, &wrong_table, Some(nodes))
                .expect_err("schema and table were mixed")
        );

        let short_column = [TableColumnView {
            schema_entry_index: 0,
            node_id: 1,
            node_type: NodeType::DeltaInteger,
            encoded_deltas: Some(&deltas),
        }];
        let short_table = TestTable {
            message_count: 2,
            columns: &short_column,
        };
        assert_eq!(
            LogOrderError::LogEventIndexLengthMismatch {
                schema_id: 9,
                column_index: 0,
                expected_bytes: 16,
                actual_bytes: 8,
            },
            locate_in_table(&tree, 9, &[node(1)], 1, &short_table, Some(nodes))
                .expect_err("delta length must match messages")
        );

        let empty_table = TestTable {
            message_count: 0,
            columns: &[],
        };
        assert_eq!(
            LogOrderError::ColumnCountMismatch {
                schema_id: 10,
                expected: 1,
                actual: 0,
            },
            locate_in_table(&tree, 10, &[node(1)], 1, &empty_table, Some(nodes))
                .expect_err("column count must correspond")
        );
    }

    #[test]
    fn cpp_fixture_cursor_and_extraction_plan_agree_on_metadata_omission() {
        let mut archive = SingleFileArchiveReader::open(Cursor::new(CPP_FIXTURE))
            .expect("open committed C++ fixture");
        let catalog = archive
            .read_catalog(ArchiveCatalogLimits::default())
            .expect("read committed C++ catalog");
        let stream = archive
            .read_packed_stream(
                catalog.metadata(),
                catalog.table_metadata(),
                0,
                PackedStreamLimits::default(),
            )
            .expect("read fixture stream");
        let mut tables = catalog
            .schema_tables(0, &stream, ColumnLimits::default())
            .expect("select fixture tables");
        let table = tables
            .next()
            .expect("fixture table")
            .expect("decode fixture table");
        assert!(tables.next().is_none());

        let locator = LogOrderLocator::discover(catalog.schema_tree())
            .expect("valid metadata")
            .expect("fixture records log order");
        assert_eq!(0, locator.metadata_root_node_id());
        assert_eq!(1, locator.node_id());
        let column = locator
            .locate(table.schema(), table.table())
            .expect("valid fixture schema/table")
            .expect("fixture table records log order");
        assert_eq!(0, column.column_index());
        assert_eq!(vec![0], column.cursor().collect::<Vec<_>>());

        let plan = ExtractionPlan::compile(
            table.schema(),
            catalog.schema_tree(),
            ExtractionPlanLimits::default(),
        )
        .expect("compile fixture extraction plan");
        assert_eq!(table.table().len(), plan.column_count());
        assert!(
            !plan
                .operations()
                .iter()
                .any(|operation| matches!(operation, ExtractionOp::Value { node_id: 1, .. }))
        );
    }
}
