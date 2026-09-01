use std::cmp::Ordering;
use std::error::Error;
use std::fmt;
use std::fmt::Display;
use std::fmt::Formatter;
use std::ops::Range;

use crate::archive::NodeType;
use crate::archive::SchemaDefinition;
use crate::archive::SchemaEntry;
use crate::archive::SchemaTree;

/// Whether an extraction operation emits an object field or an array element.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ExtractionPosition {
    /// The node's schema-tree key names an object field.
    ObjectField,
    /// The value or container is an unnamed structured-array element.
    ArrayElement,
}

/// One compact, value-independent record-extraction operation.
///
/// Node IDs address keys in the schema tree. Column indices address [`crate::archive::SchemaTable`]
/// columns in their stable wire order. The operation list never performs row access, which allows
/// a marshaller to keep sequential cursors for delta-encoded columns.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ExtractionOp {
    /// Opens an object, optionally named by `node_id` when emitted as an object field.
    BeginObject {
        /// Structural schema-tree node.
        node_id: u32,
        /// Named-field or unnamed-element placement.
        position: ExtractionPosition,
    },
    /// Closes the most recently opened object.
    EndObject,
    /// Opens an array, optionally named by `node_id` when emitted as an object field.
    BeginArray {
        /// Structural schema-tree node.
        node_id: u32,
        /// Named-field or unnamed-element placement.
        position: ExtractionPosition,
    },
    /// Closes the most recently opened array.
    EndArray,
    /// Emits one table column value.
    Value {
        /// Stable table-local column index.
        column_index: u32,
        /// Schema-tree node identifying the key and value type.
        node_id: u32,
        /// Named-field or unnamed-element placement.
        position: ExtractionPosition,
    },
    /// Emits a schema-only null value, which has no table column.
    Null {
        /// Schema-tree node identifying the key.
        node_id: u32,
        /// Named-field or unnamed-element placement.
        position: ExtractionPosition,
    },
}

/// A reusable structural program for extracting every row of one schema table.
///
/// The outer JSON object is implicit, matching the C++ serializer's document begin/end calls.
/// `operations` describe its contents. Metadata-namespace nodes are deliberately absent while
/// [`Self::column_count`] still includes their physical columns, so every referenced column index
/// remains table-local and stable.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExtractionPlan {
    schema_id: i32,
    root_node_id: Option<u32>,
    column_count: usize,
    operations: Vec<ExtractionOp>,
}

impl ExtractionPlan {
    /// Compiles a schema and its global tree into a value-independent extraction program.
    ///
    /// # Errors
    ///
    /// Returns a resource or allocation error when configured bounds are exceeded, or a
    /// structured schema error when an unordered body cannot be associated with its container,
    /// uses a C++-unsupported value type, crosses subtree boundaries, or would generate
    /// unbalanced object/array operations.
    pub fn compile(
        schema: &SchemaDefinition,
        schema_tree: &SchemaTree,
        limits: ExtractionPlanLimits,
    ) -> Result<Self, ExtractionPlanError> {
        compile_parts(
            schema.id(),
            schema.entries(),
            schema.ordered_entry_count(),
            schema_tree,
            limits,
        )
    }

    /// Returns the opaque schema ID this plan was compiled from.
    #[must_use]
    pub const fn schema_id(&self) -> i32 {
        self.schema_id
    }

    /// Returns the default-namespace object root, or `None` when C++ would emit an empty object.
    #[must_use]
    pub const fn root_node_id(&self) -> Option<u32> {
        self.root_node_id
    }

    /// Returns the complete physical column count, including omitted metadata columns.
    #[must_use]
    pub const fn column_count(&self) -> usize {
        self.column_count
    }

    /// Returns the immutable operations reused for every row.
    #[must_use]
    pub fn operations(&self) -> &[ExtractionOp] {
        &self.operations
    }

    /// Returns whether the implicit outer object has no extraction operations.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.operations.is_empty()
    }

    /// Retains only values whose schema-tree node IDs are selected, together with the enclosing
    /// object/array operations required to produce valid JSON.
    ///
    /// `selected_node_ids` must be sorted and deduplicated. The method is crate-private because
    /// public callers should use the higher-level search projection API, which resolves paths and
    /// enforces that invariant once per archive.
    pub(crate) fn project_selected_nodes(
        &self,
        selected_node_ids: &[u32],
    ) -> Result<Self, ExtractionPlanError> {
        #[derive(Clone, Copy)]
        struct Frame {
            output_start: usize,
            retained_child: bool,
        }

        let mut operations = Vec::new();
        let mut frames = Vec::new();

        for operation in &self.operations {
            match *operation {
                ExtractionOp::BeginObject { .. } | ExtractionOp::BeginArray { .. } => {
                    frames
                        .try_reserve(1)
                        .map_err(|_| ExtractionPlanError::AllocationFailed {
                            resource: ExtractionPlanResource::Operations,
                            requested: 1,
                        })?;
                    frames.push(Frame {
                        output_start: operations.len(),
                        retained_child: false,
                    });
                    push_projected_operation(&mut operations, *operation)?;
                }
                ExtractionOp::EndObject | ExtractionOp::EndArray => {
                    let frame = frames
                        .pop()
                        .ok_or(ExtractionPlanError::UnbalancedOperations {
                            operation_index: operations.len(),
                        })?;
                    if frame.retained_child {
                        push_projected_operation(&mut operations, *operation)?;
                        if let Some(parent) = frames.last_mut() {
                            parent.retained_child = true;
                        }
                    } else {
                        operations.truncate(frame.output_start);
                    }
                }
                ExtractionOp::Value { node_id, .. } | ExtractionOp::Null { node_id, .. }
                    if selected_node_ids.binary_search(&node_id).is_ok() =>
                {
                    push_projected_operation(&mut operations, *operation)?;
                    if let Some(frame) = frames.last_mut() {
                        frame.retained_child = true;
                    }
                }
                ExtractionOp::Value { .. } | ExtractionOp::Null { .. } => {}
            }
        }
        if !frames.is_empty() {
            return Err(ExtractionPlanError::UnbalancedOperations {
                operation_index: self.operations.len(),
            });
        }

        Ok(Self {
            schema_id: self.schema_id,
            root_node_id: self.root_node_id,
            column_count: self.column_count,
            operations,
        })
    }
}

fn push_projected_operation(
    operations: &mut Vec<ExtractionOp>,
    operation: ExtractionOp,
) -> Result<(), ExtractionPlanError> {
    operations
        .try_reserve(1)
        .map_err(|_| ExtractionPlanError::AllocationFailed {
            resource: ExtractionPlanResource::Operations,
            requested: 1,
        })?;
    operations.push(operation);
    Ok(())
}

/// Bounded resources used while compiling an extraction plan.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExtractionPlanLimits {
    tree_nodes: u64,
    schema_entries: u64,
    operations: u64,
    nesting_depth: u64,
    ancestry_steps: u64,
}

impl ExtractionPlanLimits {
    /// Creates explicit compilation limits.
    #[must_use]
    pub const fn new(
        max_tree_nodes: u64,
        max_schema_entries: u64,
        max_operations: u64,
        max_nesting_depth: u64,
        max_ancestry_steps: u64,
    ) -> Self {
        Self {
            tree_nodes: max_tree_nodes,
            schema_entries: max_schema_entries,
            operations: max_operations,
            nesting_depth: max_nesting_depth,
            ancestry_steps: max_ancestry_steps,
        }
    }

    /// Maximum global schema-tree nodes inspected or indexed.
    #[must_use]
    pub const fn max_tree_nodes(self) -> u64 {
        self.tree_nodes
    }

    /// Maximum flattened entries in the schema definition.
    #[must_use]
    pub const fn max_schema_entries(self) -> u64 {
        self.schema_entries
    }

    /// Maximum compiled operations.
    #[must_use]
    pub const fn max_operations(self) -> u64 {
        self.operations
    }

    /// Maximum relevant schema-tree or unordered-container nesting depth.
    #[must_use]
    pub const fn max_nesting_depth(self) -> u64 {
        self.nesting_depth
    }

    /// Maximum cumulative parent-link traversals during compilation.
    #[must_use]
    pub const fn max_ancestry_steps(self) -> u64 {
        self.ancestry_steps
    }
}

impl Default for ExtractionPlanLimits {
    fn default() -> Self {
        Self::new(
            4 * 1024 * 1024,
            4 * 1024 * 1024,
            16 * 1024 * 1024,
            256,
            64 * 1024 * 1024,
        )
    }
}

/// Resource governed by [`ExtractionPlanLimits`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ExtractionPlanResource {
    /// Global schema-tree node count and node-indexed compiler state.
    TreeNodes,
    /// Flattened schema entries and entry-indexed compiler state.
    SchemaEntries,
    /// Compiled extraction operations.
    Operations,
    /// Relevant container/tree nesting.
    NestingDepth,
    /// Cumulative parent-link traversal work.
    AncestrySteps,
}

impl Display for ExtractionPlanResource {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::TreeNodes => "schema-tree nodes",
            Self::SchemaEntries => "schema entries",
            Self::Operations => "extraction operations",
            Self::NestingDepth => "extraction nesting depth",
            Self::AncestrySteps => "schema ancestry steps",
        })
    }
}

/// Failure to compile a schema-to-record extraction plan.
#[derive(Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ExtractionPlanError {
    /// A configured compilation bound was exceeded.
    LimitExceeded {
        /// Bounded resource.
        resource: ExtractionPlanResource,
        /// Actual or next required amount.
        actual: u64,
        /// Configured maximum.
        limit: u64,
    },
    /// A bounded compiler allocation failed.
    AllocationFailed {
        /// State being allocated.
        resource: ExtractionPlanResource,
        /// Requested element count.
        requested: usize,
    },
    /// Checked arithmetic or an index conversion overflowed.
    SizeOverflow,
    /// The ordered-entry count exceeds the supplied entry slice.
    OrderedEntryCountOutOfBounds {
        /// Ordered entries.
        ordered: usize,
        /// Total entries.
        total: usize,
    },
    /// An entry references a node absent from the supplied tree.
    UnknownNode {
        /// Flattened entry index, when the reference came directly from the schema.
        entry_index: Option<usize>,
        /// Missing node ID.
        node_id: u32,
        /// Supplied tree size.
        node_count: usize,
    },
    /// A tree node's parent is absent or does not precede it.
    InvalidParent {
        /// Child node.
        node_id: u32,
        /// Invalid parent.
        parent_id: usize,
        /// Supplied tree size.
        node_count: usize,
    },
    /// A relevant node is parented by a scalar or null node.
    InvalidParentType {
        /// Child node.
        node_id: u32,
        /// Parent node.
        parent_id: u32,
        /// Invalid parent type.
        parent_type: NodeType,
    },
    /// An unordered-container delimiter appears in the ordered region.
    DelimiterInOrderedRegion {
        /// Flattened entry index.
        entry_index: usize,
    },
    /// An ordered node appears more than once.
    DuplicateOrderedNode {
        /// Repeated node.
        node_id: u32,
        /// Later flattened entry.
        entry_index: usize,
    },
    /// A delimiter's body is empty, for which C++ cannot find a container root.
    EmptyContainerBody {
        /// Delimiter entry.
        entry_index: usize,
        /// Delimited type.
        node_type: NodeType,
    },
    /// A checked delimiter body exceeds its enclosing compiler slice.
    ContainerBodyOutOfBounds {
        /// Delimiter entry.
        entry_index: usize,
        /// Declared body length.
        body_len: u32,
        /// Enclosing exclusive end.
        enclosing_end: usize,
    },
    /// A delimiter body contains no non-delimiter node from which to find its root.
    ContainerBodyHasNoNode {
        /// Delimiter entry.
        entry_index: usize,
        /// Delimited type.
        node_type: NodeType,
    },
    /// No matching object/array ancestor exists for a delimiter body.
    ContainerRootNotFound {
        /// Delimiter entry.
        entry_index: usize,
        /// Required root type.
        node_type: NodeType,
        /// First concrete body node.
        first_node_id: u32,
        /// Enclosing root, or `None` for a top-level search.
        enclosing_root: Option<u32>,
    },
    /// Two top-level unordered sections resolve to the same schema-tree root.
    DuplicateContainerRoot {
        /// Repeated root.
        node_id: u32,
        /// First delimiter or bare-root entry.
        first_entry_index: usize,
        /// Later delimiter or bare-root entry.
        second_entry_index: usize,
    },
    /// A bare unordered node is not an empty object or structured array root.
    InvalidBareContainerRoot {
        /// Flattened entry index.
        entry_index: usize,
        /// Referenced node.
        node_id: u32,
        /// Node's type.
        node_type: NodeType,
    },
    /// A structured-object body contains a delimiter other than a structured array.
    InvalidObjectBodyDelimiter {
        /// Flattened delimiter entry.
        entry_index: usize,
        /// Unexpected type.
        node_type: NodeType,
    },
    /// A body node lies outside the container subtree selected by C++ root matching.
    NodeOutsideContainer {
        /// Flattened node entry.
        entry_index: usize,
        /// Referenced node.
        node_id: u32,
        /// Required ancestor container.
        container_root: u32,
    },
    /// C++ has no unordered object/array reader for this value type.
    UnsupportedNodeInContainer {
        /// Flattened node entry.
        entry_index: usize,
        /// Referenced node.
        node_id: u32,
        /// Unsupported type.
        node_type: NodeType,
    },
    /// A container-transition path contains a non-container node.
    InvalidIntersectionNode {
        /// Invalid path node.
        node_id: u32,
        /// Invalid type.
        node_type: NodeType,
    },
    /// Two transition roots do not share the required container ancestry.
    NoContainerIntersection {
        /// Current open root.
        current_root: u32,
        /// Next required root.
        next_root: u32,
    },
    /// A value node has no stable physical column mapping.
    MissingColumnMapping {
        /// Flattened entry when applicable.
        entry_index: Option<usize>,
        /// Value node.
        node_id: u32,
    },
    /// Generated object/array operations are internally unbalanced.
    UnbalancedOperations {
        /// Operation at which validation failed, or the operation count for an unclosed container.
        operation_index: usize,
    },
    /// More than one local root identifies the default object namespace.
    MultipleDefaultObjectRoots {
        /// First matching root.
        first_node_id: u32,
        /// Later matching root.
        second_node_id: u32,
    },
}

impl Display for ExtractionPlanError {
    // Keeping every public corruption variant visibly one-to-one with its diagnostic is clearer
    // than routing half the enum through catch-all helper matches.
    #[allow(clippy::too_many_lines)]
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
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
                "could not reserve {requested} elements for {resource}"
            ),
            Self::SizeOverflow => formatter.write_str("extraction-plan size overflow"),
            Self::OrderedEntryCountOutOfBounds { ordered, total } => write!(
                formatter,
                "schema has {ordered} ordered entries but only {total} total entries"
            ),
            Self::UnknownNode {
                entry_index,
                node_id,
                node_count,
            } => {
                if let Some(entry_index) = entry_index {
                    write!(
                        formatter,
                        "schema entry {entry_index} references node {node_id}"
                    )?;
                } else {
                    write!(formatter, "extraction path references node {node_id}")?;
                }
                write!(formatter, ", but the tree has {node_count} nodes")
            }
            Self::InvalidParent {
                node_id,
                parent_id,
                node_count,
            } => write!(
                formatter,
                "schema node {node_id} has invalid parent {parent_id} in a {node_count}-node tree"
            ),
            Self::InvalidParentType {
                node_id,
                parent_id,
                parent_type,
            } => write!(
                formatter,
                "schema node {node_id} has non-container parent {parent_id} of type \
                 {parent_type:?}"
            ),
            Self::DelimiterInOrderedRegion { entry_index } => {
                write!(
                    formatter,
                    "schema entry {entry_index} is a delimiter in the ordered region"
                )
            }
            Self::DuplicateOrderedNode {
                node_id,
                entry_index,
            } => write!(
                formatter,
                "ordered schema entry {entry_index} repeats node {node_id}"
            ),
            Self::EmptyContainerBody {
                entry_index,
                node_type,
            } => write!(
                formatter,
                "schema delimiter {entry_index} has an empty {node_type:?} body"
            ),
            Self::ContainerBodyOutOfBounds {
                entry_index,
                body_len,
                enclosing_end,
            } => write!(
                formatter,
                "schema delimiter {entry_index} body length {body_len} exceeds enclosing end \
                 {enclosing_end}"
            ),
            Self::ContainerBodyHasNoNode {
                entry_index,
                node_type,
            } => write!(
                formatter,
                "schema delimiter {entry_index} {node_type:?} body contains no node"
            ),
            Self::ContainerRootNotFound {
                entry_index,
                node_type,
                first_node_id,
                enclosing_root,
            } => write!(
                formatter,
                "schema delimiter {entry_index} cannot find {node_type:?} root above node \
                 {first_node_id} within {enclosing_root:?}"
            ),
            Self::DuplicateContainerRoot {
                node_id,
                first_entry_index,
                second_entry_index,
            } => write!(
                formatter,
                "unordered entries {first_entry_index} and {second_entry_index} both resolve to \
                 container root {node_id}"
            ),
            Self::InvalidBareContainerRoot {
                entry_index,
                node_id,
                node_type,
            } => write!(
                formatter,
                "bare unordered entry {entry_index} node {node_id} has non-container type \
                 {node_type:?}"
            ),
            Self::InvalidObjectBodyDelimiter {
                entry_index,
                node_type,
            } => write!(
                formatter,
                "structured-object entry {entry_index} contains {node_type:?} delimiter"
            ),
            Self::NodeOutsideContainer {
                entry_index,
                node_id,
                container_root,
            } => write!(
                formatter,
                "schema entry {entry_index} node {node_id} is outside container root \
                 {container_root}"
            ),
            Self::UnsupportedNodeInContainer {
                entry_index,
                node_id,
                node_type,
            } => write!(
                formatter,
                "schema entry {entry_index} node {node_id} has unsupported unordered type \
                 {node_type:?}"
            ),
            Self::InvalidIntersectionNode { node_id, node_type } => write!(
                formatter,
                "container transition node {node_id} has non-container type {node_type:?}"
            ),
            Self::NoContainerIntersection {
                current_root,
                next_root,
            } => write!(
                formatter,
                "container roots {current_root} and {next_root} have no valid intersection"
            ),
            Self::MissingColumnMapping {
                entry_index,
                node_id,
            } => write!(
                formatter,
                "value node {node_id} at schema entry {entry_index:?} has no column mapping"
            ),
            Self::UnbalancedOperations { operation_index } => write!(
                formatter,
                "generated containers are unbalanced at operation {operation_index}"
            ),
            Self::MultipleDefaultObjectRoots {
                first_node_id,
                second_node_id,
            } => write!(
                formatter,
                "schema has multiple default object roots {first_node_id} and {second_node_id}"
            ),
        }
    }
}

impl Error for ExtractionPlanError {}

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

#[derive(Clone, Debug)]
struct ContainerDescriptor {
    root_id: usize,
    entry_index: usize,
    body: Range<usize>,
}

fn compile_parts<T: TreeView + ?Sized>(
    schema_id: i32,
    entries: &[SchemaEntry],
    ordered_entry_count: usize,
    tree: &T,
    limits: ExtractionPlanLimits,
) -> Result<ExtractionPlan, ExtractionPlanError> {
    let compiler = Compiler::new(entries, ordered_entry_count, tree, limits)?;
    compiler.compile(schema_id)
}

struct Compiler<'a, T: TreeView + ?Sized> {
    entries: &'a [SchemaEntry],
    ordered_entry_count: usize,
    tree: &'a T,
    limits: ExtractionPlanLimits,
    ancestry_steps: u64,
    depths: Vec<usize>,
    entry_columns: Vec<Option<u32>>,
    ordered_columns: Vec<Option<u32>>,
    column_count: usize,
    containers: Vec<ContainerDescriptor>,
    local_present: Vec<bool>,
    local_first_child: Vec<Option<usize>>,
    local_last_child: Vec<Option<usize>>,
    local_next_sibling: Vec<Option<usize>>,
    path: Vec<usize>,
    operations: Vec<ExtractionOp>,
}

impl<'a, T: TreeView + ?Sized> Compiler<'a, T> {
    fn new(
        entries: &'a [SchemaEntry],
        ordered_entry_count: usize,
        tree: &'a T,
        limits: ExtractionPlanLimits,
    ) -> Result<Self, ExtractionPlanError> {
        if ordered_entry_count > entries.len() {
            return Err(ExtractionPlanError::OrderedEntryCountOutOfBounds {
                ordered: ordered_entry_count,
                total: entries.len(),
            });
        }
        check_len_limit(
            ExtractionPlanResource::TreeNodes,
            tree.len(),
            limits.tree_nodes,
        )?;
        check_len_limit(
            ExtractionPlanResource::SchemaEntries,
            entries.len(),
            limits.schema_entries,
        )?;

        let depths = build_depths(tree)?;
        let entry_columns = filled_vec(entries.len(), None, ExtractionPlanResource::SchemaEntries)?;
        let ordered_columns = filled_vec(tree.len(), None, ExtractionPlanResource::TreeNodes)?;
        let local_present = filled_vec(tree.len(), false, ExtractionPlanResource::TreeNodes)?;
        let local_first_child = filled_vec(tree.len(), None, ExtractionPlanResource::TreeNodes)?;
        let local_last_child = filled_vec(tree.len(), None, ExtractionPlanResource::TreeNodes)?;
        let local_next_sibling = filled_vec(tree.len(), None, ExtractionPlanResource::TreeNodes)?;

        let mut compiler = Self {
            entries,
            ordered_entry_count,
            tree,
            limits,
            ancestry_steps: 0,
            depths,
            entry_columns,
            ordered_columns,
            column_count: 0,
            containers: Vec::new(),
            local_present,
            local_first_child,
            local_last_child,
            local_next_sibling,
            path: Vec::new(),
            operations: Vec::new(),
        };
        compiler.index_columns()?;
        compiler.discover_top_level_containers()?;
        Ok(compiler)
    }

    fn compile(mut self, schema_id: i32) -> Result<ExtractionPlan, ExtractionPlanError> {
        for (entry_index, entry) in self.entries[..self.ordered_entry_count]
            .iter()
            .copied()
            .enumerate()
        {
            let SchemaEntry::Node(node_id) = entry else {
                return Err(ExtractionPlanError::DelimiterInOrderedRegion { entry_index });
            };
            self.insert_local_path(node_id, Some(entry_index))?;
        }

        for container_index in 0..self.containers.len() {
            let root_id = self.containers[container_index].root_id;
            let root_id = u32::try_from(root_id).map_err(|_| ExtractionPlanError::SizeOverflow)?;
            self.insert_local_path(root_id, None)?;
        }

        let root = self.find_default_object_root()?;
        if let Some(root_id) = root {
            self.generate_json_template(root_id, 0)?;
        }
        self.validate_operation_nesting()?;

        Ok(ExtractionPlan {
            schema_id,
            root_node_id: root
                .map(u32::try_from)
                .transpose()
                .map_err(|_| ExtractionPlanError::SizeOverflow)?,
            column_count: self.column_count,
            operations: self.operations,
        })
    }

    fn index_columns(&mut self) -> Result<(), ExtractionPlanError> {
        let mut column_index = 0_usize;
        for (entry_index, entry) in self.entries.iter().copied().enumerate() {
            match entry {
                SchemaEntry::Node(node_id) => {
                    let node = self.node_from_entry(entry_index, node_id)?;
                    if is_value_bearing(node.node_type) {
                        let compact_index = u32::try_from(column_index)
                            .map_err(|_| ExtractionPlanError::SizeOverflow)?;
                        self.entry_columns[entry_index] = Some(compact_index);
                        if entry_index < self.ordered_entry_count {
                            let node_index = usize::try_from(node_id)
                                .map_err(|_| ExtractionPlanError::SizeOverflow)?;
                            if self.ordered_columns[node_index]
                                .replace(compact_index)
                                .is_some()
                            {
                                return Err(ExtractionPlanError::DuplicateOrderedNode {
                                    node_id,
                                    entry_index,
                                });
                            }
                        }
                        column_index = column_index
                            .checked_add(1)
                            .ok_or(ExtractionPlanError::SizeOverflow)?;
                    } else if entry_index < self.ordered_entry_count {
                        let node_index = usize::try_from(node_id)
                            .map_err(|_| ExtractionPlanError::SizeOverflow)?;
                        if self.ordered_columns[node_index].is_some() {
                            return Err(ExtractionPlanError::DuplicateOrderedNode {
                                node_id,
                                entry_index,
                            });
                        }
                    }
                }
                SchemaEntry::UnorderedContainer { .. }
                    if entry_index < self.ordered_entry_count =>
                {
                    return Err(ExtractionPlanError::DelimiterInOrderedRegion { entry_index });
                }
                SchemaEntry::UnorderedContainer { .. } => {}
            }
        }

        // Structural ordered duplicates need a separate check because they have no column slot.
        let mut seen = filled_vec(self.tree.len(), false, ExtractionPlanResource::TreeNodes)?;
        for (entry_index, entry) in self.entries[..self.ordered_entry_count]
            .iter()
            .copied()
            .enumerate()
        {
            let SchemaEntry::Node(node_id) = entry else {
                return Err(ExtractionPlanError::DelimiterInOrderedRegion { entry_index });
            };
            let node_index =
                usize::try_from(node_id).map_err(|_| ExtractionPlanError::SizeOverflow)?;
            if seen[node_index] {
                return Err(ExtractionPlanError::DuplicateOrderedNode {
                    node_id,
                    entry_index,
                });
            }
            seen[node_index] = true;
        }
        self.column_count = column_index;
        Ok(())
    }

    fn discover_top_level_containers(&mut self) -> Result<(), ExtractionPlanError> {
        let reserve = self.entries.len().saturating_sub(self.ordered_entry_count);
        self.containers.try_reserve(reserve).map_err(|_| {
            ExtractionPlanError::AllocationFailed {
                resource: ExtractionPlanResource::SchemaEntries,
                requested: reserve,
            }
        })?;

        let mut entry_index = self.ordered_entry_count;
        while entry_index < self.entries.len() {
            let descriptor = match self.entries[entry_index] {
                SchemaEntry::UnorderedContainer {
                    node_type,
                    body_len,
                } => {
                    if 0 == body_len {
                        return Err(ExtractionPlanError::EmptyContainerBody {
                            entry_index,
                            node_type,
                        });
                    }
                    let body = checked_body_range(entry_index, body_len, self.entries.len())?;
                    let first_node_id = self.first_node_in_body(entry_index, node_type, &body)?;
                    let root_id =
                        self.find_matching_root(entry_index, first_node_id, node_type, None)?;
                    ContainerDescriptor {
                        root_id,
                        entry_index,
                        body,
                    }
                }
                SchemaEntry::Node(node_id) => {
                    let node = self.node_from_entry(entry_index, node_id)?;
                    if !matches!(node.node_type, NodeType::Object | NodeType::StructuredArray) {
                        return Err(ExtractionPlanError::InvalidBareContainerRoot {
                            entry_index,
                            node_id,
                            node_type: node.node_type,
                        });
                    }
                    let root_id =
                        usize::try_from(node_id).map_err(|_| ExtractionPlanError::SizeOverflow)?;
                    let after_entry = entry_index
                        .checked_add(1)
                        .ok_or(ExtractionPlanError::SizeOverflow)?;
                    ContainerDescriptor {
                        root_id,
                        entry_index,
                        body: after_entry..after_entry,
                    }
                }
            };
            let after_entry = entry_index
                .checked_add(1)
                .ok_or(ExtractionPlanError::SizeOverflow)?;
            entry_index = descriptor.body.end.max(after_entry);
            self.containers.push(descriptor);
        }

        self.containers.sort_unstable_by_key(|entry| entry.root_id);
        for pair in self.containers.windows(2) {
            if pair[0].root_id == pair[1].root_id {
                return Err(ExtractionPlanError::DuplicateContainerRoot {
                    node_id: u32::try_from(pair[0].root_id)
                        .map_err(|_| ExtractionPlanError::SizeOverflow)?,
                    first_entry_index: pair[0].entry_index,
                    second_entry_index: pair[1].entry_index,
                });
            }
        }
        Ok(())
    }

    fn insert_local_path(
        &mut self,
        raw_node_id: u32,
        entry_index: Option<usize>,
    ) -> Result<(), ExtractionPlanError> {
        let mut node_id =
            usize::try_from(raw_node_id).map_err(|_| ExtractionPlanError::SizeOverflow)?;
        self.require_node(node_id, entry_index)?;
        if self.local_present[node_id] {
            return Ok(());
        }

        self.path.clear();
        loop {
            self.check_node_depth(node_id)?;
            push_fallible(
                &mut self.path,
                node_id,
                ExtractionPlanResource::NestingDepth,
            )?;
            let node = self.require_node(node_id, entry_index)?;
            let Some(parent_id) = node.parent else {
                break;
            };
            self.validate_parent(node_id, parent_id)?;
            self.bump_ancestry()?;
            if self.local_present[parent_id] {
                break;
            }
            node_id = parent_id;
        }

        while let Some(insert_id) = self.path.pop() {
            let parent = self.require_node(insert_id, entry_index)?.parent;
            self.local_present[insert_id] = true;
            if let Some(parent_id) = parent {
                if !self.local_present[parent_id] {
                    return Err(ExtractionPlanError::InvalidParent {
                        node_id: to_node_id(insert_id)?,
                        parent_id,
                        node_count: self.tree.len(),
                    });
                }
                if let Some(previous) = self.local_last_child[parent_id] {
                    self.local_next_sibling[previous] = Some(insert_id);
                } else {
                    self.local_first_child[parent_id] = Some(insert_id);
                }
                self.local_last_child[parent_id] = Some(insert_id);
            }
        }
        Ok(())
    }

    fn find_default_object_root(&self) -> Result<Option<usize>, ExtractionPlanError> {
        let mut root = None;
        for node_id in 0..self.tree.len() {
            if !self.local_present[node_id] {
                continue;
            }
            let node = self.require_node(node_id, None)?;
            if node.parent.is_none()
                && NodeType::Object == node.node_type
                && node.key.is_empty()
                && let Some(first_node_id) = root.replace(node_id)
            {
                return Err(ExtractionPlanError::MultipleDefaultObjectRoots {
                    first_node_id: to_node_id(first_node_id)?,
                    second_node_id: to_node_id(node_id)?,
                });
            }
        }
        Ok(root)
    }

    fn generate_json_template(
        &mut self,
        root_id: usize,
        generation_depth: u64,
    ) -> Result<(), ExtractionPlanError> {
        self.check_generation_depth(generation_depth)?;
        let mut child = self.local_first_child[root_id];
        while let Some(child_id) = child {
            let node = self.require_node(child_id, None)?;
            match node.node_type {
                NodeType::Object => {
                    self.emit(ExtractionOp::BeginObject {
                        node_id: to_node_id(child_id)?,
                        position: ExtractionPosition::ObjectField,
                    })?;
                    self.generate_json_template(child_id, increment_depth(generation_depth)?)?;
                    self.emit(ExtractionOp::EndObject)?;
                }
                NodeType::StructuredArray => {
                    self.emit(ExtractionOp::BeginArray {
                        node_id: to_node_id(child_id)?,
                        position: ExtractionPosition::ObjectField,
                    })?;
                    if let Some(container) = self.container(child_id).cloned() {
                        self.generate_structured_array(
                            child_id,
                            container.body,
                            increment_depth(generation_depth)?,
                        )?;
                    }
                    self.emit(ExtractionOp::EndArray)?;
                }
                NodeType::Null => self.emit(ExtractionOp::Null {
                    node_id: to_node_id(child_id)?,
                    position: ExtractionPosition::ObjectField,
                })?,
                NodeType::Metadata => {}
                node_type if is_value_bearing(node_type) => {
                    let column_index = self.ordered_columns[child_id].ok_or(
                        ExtractionPlanError::MissingColumnMapping {
                            entry_index: None,
                            node_id: to_node_id(child_id)?,
                        },
                    )?;
                    self.emit(ExtractionOp::Value {
                        column_index,
                        node_id: to_node_id(child_id)?,
                        position: ExtractionPosition::ObjectField,
                    })?;
                }
                _ => {
                    return Err(ExtractionPlanError::InvalidIntersectionNode {
                        node_id: to_node_id(child_id)?,
                        node_type: node.node_type,
                    });
                }
            }
            child = self.local_next_sibling[child_id];
        }
        Ok(())
    }

    fn generate_structured_array(
        &mut self,
        array_root: usize,
        body: Range<usize>,
        generation_depth: u64,
    ) -> Result<(), ExtractionPlanError> {
        self.check_generation_depth(generation_depth)?;
        let array_depth = self.depths[array_root];
        let mut entry_index = body.start;
        while entry_index < body.end {
            match self.entries[entry_index] {
                SchemaEntry::UnorderedContainer {
                    node_type,
                    body_len,
                } => {
                    let sub_body = checked_body_range(entry_index, body_len, body.end)?;
                    let first = self.first_node_in_body(entry_index, node_type, &sub_body)?;
                    let sub_root =
                        self.find_matching_root(entry_index, first, node_type, Some(array_root))?;
                    match node_type {
                        NodeType::StructuredArray => {
                            self.emit(ExtractionOp::BeginArray {
                                node_id: to_node_id(sub_root)?,
                                position: ExtractionPosition::ArrayElement,
                            })?;
                            self.generate_structured_array(
                                sub_root,
                                sub_body.clone(),
                                increment_depth(generation_depth)?,
                            )?;
                            self.emit(ExtractionOp::EndArray)?;
                        }
                        NodeType::Object => {
                            self.emit(ExtractionOp::BeginObject {
                                node_id: to_node_id(sub_root)?,
                                position: ExtractionPosition::ArrayElement,
                            })?;
                            self.generate_structured_object(
                                sub_root,
                                sub_body.clone(),
                                increment_depth(generation_depth)?,
                            )?;
                            self.emit(ExtractionOp::EndObject)?;
                        }
                        _ => {
                            return Err(ExtractionPlanError::InvalidObjectBodyDelimiter {
                                entry_index,
                                node_type,
                            });
                        }
                    }
                    entry_index = sub_body.end;
                }
                SchemaEntry::Node(raw_node_id) => {
                    let node_id = usize::try_from(raw_node_id)
                        .map_err(|_| ExtractionPlanError::SizeOverflow)?;
                    let node_type = self.node_from_entry(entry_index, raw_node_id)?.node_type;
                    self.ensure_descendant(entry_index, node_id, array_root)?;
                    match node_type {
                        NodeType::Object => {
                            self.fix_brackets(array_root, node_id)?;
                            let close_count = self.depths[node_id]
                                .checked_sub(array_depth)
                                .ok_or(ExtractionPlanError::SizeOverflow)?;
                            for _ in 0..close_count {
                                self.emit(ExtractionOp::EndObject)?;
                            }
                        }
                        NodeType::StructuredArray => {
                            self.emit(ExtractionOp::BeginArray {
                                node_id: raw_node_id,
                                position: ExtractionPosition::ArrayElement,
                            })?;
                            self.emit(ExtractionOp::EndArray)?;
                        }
                        NodeType::Null => self.emit(ExtractionOp::Null {
                            node_id: raw_node_id,
                            position: ExtractionPosition::ArrayElement,
                        })?,
                        node_type if is_supported_unordered_value(node_type) => {
                            self.emit_entry_value(
                                entry_index,
                                raw_node_id,
                                ExtractionPosition::ArrayElement,
                            )?;
                        }
                        node_type => {
                            return Err(ExtractionPlanError::UnsupportedNodeInContainer {
                                entry_index,
                                node_id: raw_node_id,
                                node_type,
                            });
                        }
                    }
                    entry_index = entry_index
                        .checked_add(1)
                        .ok_or(ExtractionPlanError::SizeOverflow)?;
                }
            }
        }
        Ok(())
    }

    fn generate_structured_object(
        &mut self,
        object_root: usize,
        body: Range<usize>,
        generation_depth: u64,
    ) -> Result<(), ExtractionPlanError> {
        self.check_generation_depth(generation_depth)?;
        let mut current_root = object_root;
        let mut entry_index = body.start;
        while entry_index < body.end {
            match self.entries[entry_index] {
                SchemaEntry::UnorderedContainer {
                    node_type,
                    body_len,
                } => {
                    if NodeType::StructuredArray != node_type {
                        return Err(ExtractionPlanError::InvalidObjectBodyDelimiter {
                            entry_index,
                            node_type,
                        });
                    }
                    let array_body = checked_body_range(entry_index, body_len, body.end)?;
                    let first = self.first_node_in_body(entry_index, node_type, &array_body)?;
                    let array_root = self.find_matching_root(
                        entry_index,
                        first,
                        NodeType::StructuredArray,
                        Some(object_root),
                    )?;
                    self.fix_brackets(current_root, array_root)?;
                    self.generate_structured_array(
                        array_root,
                        array_body.clone(),
                        increment_depth(generation_depth)?,
                    )?;
                    self.emit(ExtractionOp::EndArray)?;
                    current_root = self
                        .require_node(array_root, Some(entry_index))?
                        .parent
                        .ok_or(ExtractionPlanError::NoContainerIntersection {
                            current_root: to_node_id(current_root)?,
                            next_root: to_node_id(array_root)?,
                        })?;
                    entry_index = array_body.end;
                }
                SchemaEntry::Node(raw_node_id) => {
                    let node_id = usize::try_from(raw_node_id)
                        .map_err(|_| ExtractionPlanError::SizeOverflow)?;
                    let (parent, node_type) = {
                        let node = self.node_from_entry(entry_index, raw_node_id)?;
                        (node.parent, node.node_type)
                    };
                    self.ensure_descendant(entry_index, node_id, object_root)?;
                    let next_root = parent.ok_or(ExtractionPlanError::NodeOutsideContainer {
                        entry_index,
                        node_id: raw_node_id,
                        container_root: to_node_id(object_root)?,
                    })?;
                    self.fix_brackets(current_root, next_root)?;
                    current_root = next_root;
                    match node_type {
                        NodeType::Object => {
                            self.emit(ExtractionOp::BeginObject {
                                node_id: raw_node_id,
                                position: ExtractionPosition::ObjectField,
                            })?;
                            self.emit(ExtractionOp::EndObject)?;
                        }
                        NodeType::StructuredArray => {
                            self.emit(ExtractionOp::BeginArray {
                                node_id: raw_node_id,
                                position: ExtractionPosition::ObjectField,
                            })?;
                            self.emit(ExtractionOp::EndArray)?;
                        }
                        NodeType::Null => self.emit(ExtractionOp::Null {
                            node_id: raw_node_id,
                            position: ExtractionPosition::ObjectField,
                        })?,
                        node_type if is_supported_unordered_value(node_type) => {
                            self.emit_entry_value(
                                entry_index,
                                raw_node_id,
                                ExtractionPosition::ObjectField,
                            )?;
                        }
                        node_type => {
                            return Err(ExtractionPlanError::UnsupportedNodeInContainer {
                                entry_index,
                                node_id: raw_node_id,
                                node_type,
                            });
                        }
                    }
                    entry_index = entry_index
                        .checked_add(1)
                        .ok_or(ExtractionPlanError::SizeOverflow)?;
                }
            }
        }
        self.fix_brackets(current_root, object_root)
    }

    fn fix_brackets(
        &mut self,
        mut current_root: usize,
        mut next_root: usize,
    ) -> Result<(), ExtractionPlanError> {
        let original_current = current_root;
        let original_next = next_root;
        self.path.clear();
        while self.require_node(current_root, None)?.parent
            != self.require_node(next_root, None)?.parent
        {
            let current_depth = self.depths[current_root];
            let next_depth = self.depths[next_root];
            match current_depth.cmp(&next_depth) {
                Ordering::Greater => {
                    current_root = self.ascend_for_intersection(
                        current_root,
                        original_current,
                        original_next,
                    )?;
                    self.emit(ExtractionOp::EndObject)?;
                }
                Ordering::Less => {
                    push_fallible(
                        &mut self.path,
                        next_root,
                        ExtractionPlanResource::NestingDepth,
                    )?;
                    next_root =
                        self.ascend_for_intersection(next_root, original_current, original_next)?;
                }
                Ordering::Equal => {
                    current_root = self.ascend_for_intersection(
                        current_root,
                        original_current,
                        original_next,
                    )?;
                    self.emit(ExtractionOp::EndObject)?;
                    push_fallible(
                        &mut self.path,
                        next_root,
                        ExtractionPlanResource::NestingDepth,
                    )?;
                    next_root =
                        self.ascend_for_intersection(next_root, original_current, original_next)?;
                }
            }
        }

        if current_root != next_root {
            self.emit(ExtractionOp::EndObject)?;
            push_fallible(
                &mut self.path,
                next_root,
                ExtractionPlanResource::NestingDepth,
            )?;
        }

        let mut path_index = self.path.len();
        while 0 != path_index {
            path_index -= 1;
            let node_id = self.path[path_index];
            let node = self.require_node(node_id, None)?;
            let position = if node.key.is_empty() {
                ExtractionPosition::ArrayElement
            } else {
                ExtractionPosition::ObjectField
            };
            match node.node_type {
                NodeType::Object => self.emit(ExtractionOp::BeginObject {
                    node_id: to_node_id(node_id)?,
                    position,
                })?,
                NodeType::StructuredArray => self.emit(ExtractionOp::BeginArray {
                    node_id: to_node_id(node_id)?,
                    position,
                })?,
                node_type => {
                    return Err(ExtractionPlanError::InvalidIntersectionNode {
                        node_id: to_node_id(node_id)?,
                        node_type,
                    });
                }
            }
        }
        self.path.clear();
        Ok(())
    }

    fn ascend_for_intersection(
        &mut self,
        node_id: usize,
        original_current: usize,
        original_next: usize,
    ) -> Result<usize, ExtractionPlanError> {
        self.bump_ancestry()?;
        self.require_node(node_id, None)?.parent.ok_or(
            ExtractionPlanError::NoContainerIntersection {
                current_root: to_node_id(original_current)?,
                next_root: to_node_id(original_next)?,
            },
        )
    }

    fn emit_entry_value(
        &mut self,
        entry_index: usize,
        node_id: u32,
        position: ExtractionPosition,
    ) -> Result<(), ExtractionPlanError> {
        let column_index =
            self.entry_columns[entry_index].ok_or(ExtractionPlanError::MissingColumnMapping {
                entry_index: Some(entry_index),
                node_id,
            })?;
        self.emit(ExtractionOp::Value {
            column_index,
            node_id,
            position,
        })
    }

    fn emit(&mut self, operation: ExtractionOp) -> Result<(), ExtractionPlanError> {
        let next_len = self
            .operations
            .len()
            .checked_add(1)
            .ok_or(ExtractionPlanError::SizeOverflow)?;
        check_len_limit(
            ExtractionPlanResource::Operations,
            next_len,
            self.limits.operations,
        )?;
        push_fallible(
            &mut self.operations,
            operation,
            ExtractionPlanResource::Operations,
        )
    }

    fn validate_operation_nesting(&self) -> Result<(), ExtractionPlanError> {
        let mut stack = Vec::new();
        stack.try_reserve(self.operations.len()).map_err(|_| {
            ExtractionPlanError::AllocationFailed {
                resource: ExtractionPlanResource::Operations,
                requested: self.operations.len(),
            }
        })?;
        for (operation_index, operation) in self.operations.iter().enumerate() {
            match operation {
                ExtractionOp::BeginObject { .. } => stack.push(NodeType::Object),
                ExtractionOp::BeginArray { .. } => stack.push(NodeType::StructuredArray),
                ExtractionOp::EndObject => {
                    if stack.pop() != Some(NodeType::Object) {
                        return Err(ExtractionPlanError::UnbalancedOperations { operation_index });
                    }
                }
                ExtractionOp::EndArray => {
                    if stack.pop() != Some(NodeType::StructuredArray) {
                        return Err(ExtractionPlanError::UnbalancedOperations { operation_index });
                    }
                }
                ExtractionOp::Value { .. } | ExtractionOp::Null { .. } => {}
            }
        }
        if stack.is_empty() {
            Ok(())
        } else {
            Err(ExtractionPlanError::UnbalancedOperations {
                operation_index: self.operations.len(),
            })
        }
    }

    fn first_node_in_body(
        &self,
        entry_index: usize,
        node_type: NodeType,
        body: &Range<usize>,
    ) -> Result<u32, ExtractionPlanError> {
        self.entries[body.clone()]
            .iter()
            .find_map(|entry| match entry {
                SchemaEntry::Node(node_id) => Some(*node_id),
                SchemaEntry::UnorderedContainer { .. } => None,
            })
            .ok_or(ExtractionPlanError::ContainerBodyHasNoNode {
                entry_index,
                node_type,
            })
    }

    fn find_matching_root(
        &mut self,
        entry_index: usize,
        first_node_id: u32,
        node_type: NodeType,
        enclosing_root: Option<usize>,
    ) -> Result<usize, ExtractionPlanError> {
        let mut current =
            Some(usize::try_from(first_node_id).map_err(|_| ExtractionPlanError::SizeOverflow)?);
        let mut earliest = None;
        while current != enclosing_root {
            let Some(node_id) = current else {
                return Err(ExtractionPlanError::ContainerRootNotFound {
                    entry_index,
                    node_type,
                    first_node_id,
                    enclosing_root: enclosing_root.map(to_node_id).transpose()?,
                });
            };
            let node = self.require_node(node_id, Some(entry_index))?;
            self.check_node_depth(node_id)?;
            if node.node_type == node_type {
                earliest = Some(node_id);
            }
            current = node.parent;
            self.bump_ancestry()?;
        }
        earliest.ok_or(ExtractionPlanError::ContainerRootNotFound {
            entry_index,
            node_type,
            first_node_id,
            enclosing_root: enclosing_root.map(to_node_id).transpose()?,
        })
    }

    fn ensure_descendant(
        &mut self,
        entry_index: usize,
        node_id: usize,
        container_root: usize,
    ) -> Result<(), ExtractionPlanError> {
        if node_id == container_root {
            return Err(ExtractionPlanError::NodeOutsideContainer {
                entry_index,
                node_id: to_node_id(node_id)?,
                container_root: to_node_id(container_root)?,
            });
        }
        let mut current = node_id;
        loop {
            self.check_node_depth(current)?;
            let node = self.require_node(current, Some(entry_index))?;
            let Some(parent_id) = node.parent else {
                return Err(ExtractionPlanError::NodeOutsideContainer {
                    entry_index,
                    node_id: to_node_id(node_id)?,
                    container_root: to_node_id(container_root)?,
                });
            };
            self.validate_parent(current, parent_id)?;
            self.bump_ancestry()?;
            if parent_id == container_root {
                return Ok(());
            }
            current = parent_id;
        }
    }

    fn container(&self, root_id: usize) -> Option<&ContainerDescriptor> {
        self.containers
            .binary_search_by_key(&root_id, |entry| entry.root_id)
            .ok()
            .map(|index| &self.containers[index])
    }

    fn node_from_entry(
        &self,
        entry_index: usize,
        node_id: u32,
    ) -> Result<NodeView<'_>, ExtractionPlanError> {
        let node_index = usize::try_from(node_id).map_err(|_| ExtractionPlanError::SizeOverflow)?;
        self.require_node(node_index, Some(entry_index))
    }

    fn require_node(
        &self,
        node_id: usize,
        entry_index: Option<usize>,
    ) -> Result<NodeView<'_>, ExtractionPlanError> {
        self.tree
            .node(node_id)
            .ok_or(ExtractionPlanError::UnknownNode {
                entry_index,
                node_id: to_node_id(node_id)?,
                node_count: self.tree.len(),
            })
    }

    fn validate_parent(&self, node_id: usize, parent_id: usize) -> Result<(), ExtractionPlanError> {
        if parent_id >= node_id || parent_id >= self.tree.len() {
            return Err(ExtractionPlanError::InvalidParent {
                node_id: to_node_id(node_id)?,
                parent_id,
                node_count: self.tree.len(),
            });
        }
        let parent = self.require_node(parent_id, None)?;
        if !matches!(
            parent.node_type,
            NodeType::Object | NodeType::StructuredArray | NodeType::Metadata
        ) {
            return Err(ExtractionPlanError::InvalidParentType {
                node_id: to_node_id(node_id)?,
                parent_id: to_node_id(parent_id)?,
                parent_type: parent.node_type,
            });
        }
        Ok(())
    }

    fn check_node_depth(&self, node_id: usize) -> Result<(), ExtractionPlanError> {
        let depth =
            u64::try_from(self.depths[node_id]).map_err(|_| ExtractionPlanError::SizeOverflow)?;
        if depth > self.limits.nesting_depth {
            Err(ExtractionPlanError::LimitExceeded {
                resource: ExtractionPlanResource::NestingDepth,
                actual: depth,
                limit: self.limits.nesting_depth,
            })
        } else {
            Ok(())
        }
    }

    const fn check_generation_depth(&self, depth: u64) -> Result<(), ExtractionPlanError> {
        if depth > self.limits.nesting_depth {
            Err(ExtractionPlanError::LimitExceeded {
                resource: ExtractionPlanResource::NestingDepth,
                actual: depth,
                limit: self.limits.nesting_depth,
            })
        } else {
            Ok(())
        }
    }

    fn bump_ancestry(&mut self) -> Result<(), ExtractionPlanError> {
        self.ancestry_steps = self
            .ancestry_steps
            .checked_add(1)
            .ok_or(ExtractionPlanError::SizeOverflow)?;
        if self.ancestry_steps > self.limits.ancestry_steps {
            Err(ExtractionPlanError::LimitExceeded {
                resource: ExtractionPlanResource::AncestrySteps,
                actual: self.ancestry_steps,
                limit: self.limits.ancestry_steps,
            })
        } else {
            Ok(())
        }
    }
}

fn build_depths<T: TreeView + ?Sized>(tree: &T) -> Result<Vec<usize>, ExtractionPlanError> {
    let mut depths = filled_vec(tree.len(), 0_usize, ExtractionPlanResource::TreeNodes)?;
    for node_id in 0..tree.len() {
        let node = tree.node(node_id).ok_or(ExtractionPlanError::UnknownNode {
            entry_index: None,
            node_id: to_node_id(node_id)?,
            node_count: tree.len(),
        })?;
        if let Some(parent_id) = node.parent {
            if parent_id >= node_id || parent_id >= tree.len() {
                return Err(ExtractionPlanError::InvalidParent {
                    node_id: to_node_id(node_id)?,
                    parent_id,
                    node_count: tree.len(),
                });
            }
            depths[node_id] = depths[parent_id]
                .checked_add(1)
                .ok_or(ExtractionPlanError::SizeOverflow)?;
        }
    }
    Ok(depths)
}

fn checked_body_range(
    entry_index: usize,
    body_len: u32,
    enclosing_end: usize,
) -> Result<Range<usize>, ExtractionPlanError> {
    let start = entry_index
        .checked_add(1)
        .ok_or(ExtractionPlanError::SizeOverflow)?;
    let end = start
        .checked_add(usize::try_from(body_len).map_err(|_| ExtractionPlanError::SizeOverflow)?)
        .ok_or(ExtractionPlanError::SizeOverflow)?;
    if end > enclosing_end {
        Err(ExtractionPlanError::ContainerBodyOutOfBounds {
            entry_index,
            body_len,
            enclosing_end,
        })
    } else {
        Ok(start..end)
    }
}

const fn is_value_bearing(node_type: NodeType) -> bool {
    matches!(
        node_type,
        NodeType::Integer
            | NodeType::DeltaInteger
            | NodeType::Float
            | NodeType::FormattedFloat
            | NodeType::DictionaryFloat
            | NodeType::Boolean
            | NodeType::VarString
            | NodeType::ClpString
            | NodeType::UnstructuredArray
            | NodeType::DeprecatedDateString
            | NodeType::Timestamp
    )
}

const fn is_supported_unordered_value(node_type: NodeType) -> bool {
    matches!(
        node_type,
        NodeType::Integer
            | NodeType::DeltaInteger
            | NodeType::Float
            | NodeType::FormattedFloat
            | NodeType::DictionaryFloat
            | NodeType::Boolean
            | NodeType::VarString
            | NodeType::ClpString
    )
}

fn check_len_limit(
    resource: ExtractionPlanResource,
    actual: usize,
    limit: u64,
) -> Result<(), ExtractionPlanError> {
    let actual = u64::try_from(actual).map_err(|_| ExtractionPlanError::SizeOverflow)?;
    if actual > limit {
        Err(ExtractionPlanError::LimitExceeded {
            resource,
            actual,
            limit,
        })
    } else {
        Ok(())
    }
}

fn filled_vec<T: Clone>(
    len: usize,
    value: T,
    resource: ExtractionPlanResource,
) -> Result<Vec<T>, ExtractionPlanError> {
    let mut values = Vec::new();
    values
        .try_reserve_exact(len)
        .map_err(|_| ExtractionPlanError::AllocationFailed {
            resource,
            requested: len,
        })?;
    values.resize(len, value);
    Ok(values)
}

fn push_fallible<T>(
    values: &mut Vec<T>,
    value: T,
    resource: ExtractionPlanResource,
) -> Result<(), ExtractionPlanError> {
    if values.len() == values.capacity() {
        let requested = values
            .len()
            .checked_add(1)
            .ok_or(ExtractionPlanError::SizeOverflow)?;
        values
            .try_reserve(1)
            .map_err(|_| ExtractionPlanError::AllocationFailed {
                resource,
                requested,
            })?;
    }
    values.push(value);
    Ok(())
}

fn to_node_id(node_id: usize) -> Result<u32, ExtractionPlanError> {
    u32::try_from(node_id).map_err(|_| ExtractionPlanError::SizeOverflow)
}

fn increment_depth(depth: u64) -> Result<u64, ExtractionPlanError> {
    depth
        .checked_add(1)
        .ok_or(ExtractionPlanError::SizeOverflow)
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::archive::ArchiveCatalogLimits;
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

    const fn node(node_id: u32) -> SchemaEntry {
        SchemaEntry::Node(node_id)
    }

    const fn delimiter(node_type: NodeType, body_len: u32) -> SchemaEntry {
        SchemaEntry::UnorderedContainer {
            node_type,
            body_len,
        }
    }

    fn compile_test(
        nodes: &[TestNode],
        entries: &[SchemaEntry],
        ordered_entry_count: usize,
    ) -> Result<ExtractionPlan, ExtractionPlanError> {
        compile_parts(
            17,
            entries,
            ordered_entry_count,
            &TestTree(nodes),
            ExtractionPlanLimits::default(),
        )
    }

    #[test]
    fn compiles_nested_objects_null_and_empty_structures() {
        let tree = [
            TestNode::new(None, b"", NodeType::Object),
            TestNode::new(Some(0), b"left", NodeType::Object),
            TestNode::new(Some(1), b"number", NodeType::Integer),
            TestNode::new(Some(0), b"right", NodeType::Object),
            TestNode::new(Some(3), b"inner", NodeType::Object),
            TestNode::new(Some(4), b"text", NodeType::VarString),
            TestNode::new(Some(0), b"empty_object", NodeType::Object),
            TestNode::new(Some(0), b"empty_array", NodeType::StructuredArray),
            TestNode::new(Some(0), b"nothing", NodeType::Null),
        ];
        let entries = [node(2), node(5), node(6), node(7), node(8)];

        let plan = compile_test(&tree, &entries, entries.len()).expect("valid nested plan");

        assert_eq!(17, plan.schema_id());
        assert_eq!(Some(0), plan.root_node_id());
        assert_eq!(2, plan.column_count());
        assert_eq!(
            [
                ExtractionOp::BeginObject {
                    node_id: 1,
                    position: ExtractionPosition::ObjectField,
                },
                ExtractionOp::Value {
                    column_index: 0,
                    node_id: 2,
                    position: ExtractionPosition::ObjectField,
                },
                ExtractionOp::EndObject,
                ExtractionOp::BeginObject {
                    node_id: 3,
                    position: ExtractionPosition::ObjectField,
                },
                ExtractionOp::BeginObject {
                    node_id: 4,
                    position: ExtractionPosition::ObjectField,
                },
                ExtractionOp::Value {
                    column_index: 1,
                    node_id: 5,
                    position: ExtractionPosition::ObjectField,
                },
                ExtractionOp::EndObject,
                ExtractionOp::EndObject,
                ExtractionOp::BeginObject {
                    node_id: 6,
                    position: ExtractionPosition::ObjectField,
                },
                ExtractionOp::EndObject,
                ExtractionOp::BeginArray {
                    node_id: 7,
                    position: ExtractionPosition::ObjectField,
                },
                ExtractionOp::EndArray,
                ExtractionOp::Null {
                    node_id: 8,
                    position: ExtractionPosition::ObjectField,
                },
            ],
            plan.operations()
        );
    }

    #[test]
    fn compiles_nested_structured_array_elements() {
        let tree = [
            TestNode::new(None, b"", NodeType::Object),
            TestNode::new(Some(0), b"items", NodeType::StructuredArray),
            TestNode::new(Some(1), b"", NodeType::Object),
            TestNode::new(Some(2), b"number", NodeType::Integer),
            TestNode::new(Some(2), b"nested", NodeType::StructuredArray),
            TestNode::new(Some(4), b"", NodeType::Integer),
            TestNode::new(Some(1), b"", NodeType::Object),
            TestNode::new(Some(1), b"", NodeType::Null),
        ];
        let entries = [
            delimiter(NodeType::StructuredArray, 6),
            delimiter(NodeType::Object, 3),
            node(3),
            delimiter(NodeType::StructuredArray, 1),
            node(5),
            node(6),
            node(7),
        ];

        let plan = compile_test(&tree, &entries, 0).expect("valid structured-array plan");

        assert_eq!(2, plan.column_count());
        assert_eq!(
            [
                ExtractionOp::BeginArray {
                    node_id: 1,
                    position: ExtractionPosition::ObjectField,
                },
                ExtractionOp::BeginObject {
                    node_id: 2,
                    position: ExtractionPosition::ArrayElement,
                },
                ExtractionOp::Value {
                    column_index: 0,
                    node_id: 3,
                    position: ExtractionPosition::ObjectField,
                },
                ExtractionOp::BeginArray {
                    node_id: 4,
                    position: ExtractionPosition::ObjectField,
                },
                ExtractionOp::Value {
                    column_index: 1,
                    node_id: 5,
                    position: ExtractionPosition::ArrayElement,
                },
                ExtractionOp::EndArray,
                ExtractionOp::EndObject,
                ExtractionOp::BeginObject {
                    node_id: 6,
                    position: ExtractionPosition::ArrayElement,
                },
                ExtractionOp::EndObject,
                ExtractionOp::Null {
                    node_id: 7,
                    position: ExtractionPosition::ArrayElement,
                },
                ExtractionOp::EndArray,
            ],
            plan.operations()
        );
    }

    #[test]
    fn repeats_unordered_bodies_with_distinct_physical_columns() {
        let tree = [
            TestNode::new(None, b"", NodeType::Object),
            TestNode::new(Some(0), b"items", NodeType::StructuredArray),
            TestNode::new(Some(1), b"", NodeType::Object),
            TestNode::new(Some(2), b"number", NodeType::Integer),
        ];
        let entries = [
            delimiter(NodeType::StructuredArray, 4),
            delimiter(NodeType::Object, 1),
            node(3),
            delimiter(NodeType::Object, 1),
            node(3),
        ];

        let plan = compile_test(&tree, &entries, 0).expect("valid repeated-body plan");

        assert_eq!(2, plan.column_count());
        assert_eq!(
            [
                ExtractionOp::BeginArray {
                    node_id: 1,
                    position: ExtractionPosition::ObjectField,
                },
                ExtractionOp::BeginObject {
                    node_id: 2,
                    position: ExtractionPosition::ArrayElement,
                },
                ExtractionOp::Value {
                    column_index: 0,
                    node_id: 3,
                    position: ExtractionPosition::ObjectField,
                },
                ExtractionOp::EndObject,
                ExtractionOp::BeginObject {
                    node_id: 2,
                    position: ExtractionPosition::ArrayElement,
                },
                ExtractionOp::Value {
                    column_index: 1,
                    node_id: 3,
                    position: ExtractionPosition::ObjectField,
                },
                ExtractionOp::EndObject,
                ExtractionOp::EndArray,
            ],
            plan.operations()
        );
    }

    #[test]
    fn appends_unordered_roots_after_ordered_paths_like_cpp() {
        let tree = [
            TestNode::new(None, b"", NodeType::Object),
            TestNode::new(Some(0), b"items", NodeType::StructuredArray),
            TestNode::new(Some(1), b"", NodeType::Integer),
            TestNode::new(Some(0), b"first", NodeType::Integer),
        ];
        let entries = [node(3), delimiter(NodeType::StructuredArray, 1), node(2)];

        let plan = compile_test(&tree, &entries, 1).expect("valid mixed-order plan");

        assert_eq!(
            [
                ExtractionOp::Value {
                    column_index: 0,
                    node_id: 3,
                    position: ExtractionPosition::ObjectField,
                },
                ExtractionOp::BeginArray {
                    node_id: 1,
                    position: ExtractionPosition::ObjectField,
                },
                ExtractionOp::Value {
                    column_index: 1,
                    node_id: 2,
                    position: ExtractionPosition::ArrayElement,
                },
                ExtractionOp::EndArray,
            ],
            plan.operations()
        );
    }

    #[test]
    fn rejects_malformed_unordered_structures_and_resource_exhaustion() {
        let tree = [
            TestNode::new(None, b"", NodeType::Object),
            TestNode::new(Some(0), b"items", NodeType::StructuredArray),
            TestNode::new(Some(1), b"", NodeType::Timestamp),
        ];

        assert_eq!(
            ExtractionPlanError::EmptyContainerBody {
                entry_index: 0,
                node_type: NodeType::StructuredArray,
            },
            compile_test(&tree, &[delimiter(NodeType::StructuredArray, 0)], 0)
                .expect_err("empty tagged container")
        );
        assert_eq!(
            ExtractionPlanError::UnsupportedNodeInContainer {
                entry_index: 1,
                node_id: 2,
                node_type: NodeType::Timestamp,
            },
            compile_test(
                &tree,
                &[delimiter(NodeType::StructuredArray, 1), node(2)],
                0,
            )
            .expect_err("timestamp has no C++ unordered reader")
        );

        let limits = ExtractionPlanLimits::new(3, 2, 0, 8, 32);
        assert_eq!(
            ExtractionPlanError::LimitExceeded {
                resource: ExtractionPlanResource::Operations,
                actual: 1,
                limit: 0,
            },
            compile_parts(17, &[node(2)], 1, &TestTree(&tree), limits)
                .expect_err("operation limit")
        );
    }

    #[test]
    fn cpp_oracle_operation_order_omits_metadata_but_keeps_column_index() {
        let mut archive = SingleFileArchiveReader::open(Cursor::new(CPP_FIXTURE))
            .expect("open committed C++ fixture");
        let catalog = archive
            .read_catalog(ArchiveCatalogLimits::default())
            .expect("read committed C++ catalog");
        let schema = catalog.schema_map().get(0).expect("fixture schema zero");

        let plan = ExtractionPlan::compile(
            schema,
            catalog.schema_tree(),
            ExtractionPlanLimits::default(),
        )
        .expect("compile C++ fixture plan");

        assert_eq!(0, plan.schema_id());
        assert_eq!(Some(2), plan.root_node_id());
        assert_eq!(6, plan.column_count());
        assert_eq!(
            [
                ExtractionOp::Value {
                    column_index: 1,
                    node_id: 3,
                    position: ExtractionPosition::ObjectField,
                },
                ExtractionOp::Value {
                    column_index: 2,
                    node_id: 4,
                    position: ExtractionPosition::ObjectField,
                },
                ExtractionOp::Value {
                    column_index: 3,
                    node_id: 5,
                    position: ExtractionPosition::ObjectField,
                },
                ExtractionOp::Value {
                    column_index: 4,
                    node_id: 6,
                    position: ExtractionPosition::ObjectField,
                },
                ExtractionOp::Value {
                    column_index: 5,
                    node_id: 7,
                    position: ExtractionPosition::ObjectField,
                },
            ],
            plan.operations()
        );
    }
}
