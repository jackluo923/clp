//! Archive-backed semantic compilation and physical-row matching for parsed KQL.

use std::collections::BTreeMap;
use std::collections::HashSet;
use std::error::Error;
use std::fmt;
use std::fmt::Display;
use std::fmt::Formatter;
use std::iter::FusedIterator;

use super::BooleanOperator;
use super::ColumnNamespace;
use super::ColumnPath;
use super::ComparisonOperator;
use super::ExpressionKind;
use super::ListExpression;
use super::ListOperator;
use super::Literal;
use super::NodeId;
use super::ParsedQuery;
use super::PathComponent;
use super::Predicate;
use super::TimestampQueryError;
use super::array::ArrayComparison;
use super::array::ArrayFailure;
use super::array::ArrayLimits;
use super::array::ArrayNumber;
use super::array::ArrayPredicate;
use super::array::ArrayResource;
use super::array::ArrayScratch;
use super::array::ArraySearchError;
use super::array::evaluate as evaluate_array;
use super::timestamp_query::resolve_timestamp_literal;
use super::wildcard::wildcard_match;
use crate::ExtractionPlan;
use crate::ExtractionPlanError;
use crate::ExtractionPlanLimits;
use crate::archive::ArchiveCatalog;
use crate::archive::ColumnData;
use crate::archive::DecodedSchemaTable;
use crate::archive::EncodedVariableError;
use crate::archive::NodeType;
use crate::archive::RangeIndexValue;
use crate::archive::SchemaEntry;
use crate::archive::SchemaTree;
use crate::archive::TimestampBounds;
use crate::archive::append_clp_message_bounded;
use crate::log_order::locate_log_order_column;

/// Resource limits for archive semantic compilation and one table match.
///
/// The bitmap bound counts live tri-state row bytes. Dictionary scan and match limits are
/// aggregate across all predicates in one compiled query. Callers handling unusually large schema
/// churn can raise the archive-table bound explicitly.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SearchLimits {
    schema_nodes: usize,
    archive_tables: usize,
    resolved_nodes: usize,
    path_states: usize,
    dictionary_entries: usize,
    dictionary_matches: usize,
    bitmap_bytes: usize,
    string_scratch_bytes: usize,
    range_states: usize,
    array_states: usize,
    array_nesting_depth: usize,
    array_string_bytes: usize,
}

impl SearchLimits {
    /// Creates explicit schema, table, resolution, dictionary, and bitmap limits.
    #[must_use]
    pub const fn new(
        max_schema_nodes: usize,
        max_archive_tables: usize,
        max_resolved_nodes: usize,
        max_path_states: usize,
        max_dictionary_entries_scanned: usize,
        max_dictionary_matches: usize,
        max_live_bitmap_bytes: usize,
    ) -> Self {
        Self {
            schema_nodes: max_schema_nodes,
            archive_tables: max_archive_tables,
            resolved_nodes: max_resolved_nodes,
            path_states: max_path_states,
            dictionary_entries: max_dictionary_entries_scanned,
            dictionary_matches: max_dictionary_matches,
            bitmap_bytes: max_live_bitmap_bytes,
            string_scratch_bytes: 16 * 1024 * 1024,
            range_states: 1_048_576,
            array_states: 1_048_576,
            array_nesting_depth: 256,
            array_string_bytes: 16 * 1024 * 1024,
        }
    }

    /// Replaces the per-value reconstructed CLP string and range traversal bounds.
    #[must_use]
    pub const fn with_value_limits(
        mut self,
        max_reconstructed_string_bytes: usize,
        max_range_traversal_states: usize,
    ) -> Self {
        self.string_scratch_bytes = max_reconstructed_string_bytes;
        self.range_states = max_range_traversal_states;
        self
    }

    /// Replaces per-array JSON traversal, nesting, and decoded-string scratch bounds.
    #[must_use]
    pub const fn with_array_limits(
        mut self,
        max_json_states: usize,
        max_nesting_depth: usize,
        max_decoded_string_bytes: usize,
    ) -> Self {
        self.array_states = max_json_states;
        self.array_nesting_depth = max_nesting_depth;
        self.array_string_bytes = max_decoded_string_bytes;
        self
    }

    /// Maximum schema-tree nodes indexed by semantic compilation.
    #[must_use]
    pub const fn max_schema_nodes(self) -> usize {
        self.schema_nodes
    }

    /// Maximum physical schema tables in the bound archive.
    #[must_use]
    pub const fn max_archive_tables(self) -> usize {
        self.archive_tables
    }

    /// Maximum compiled expression/list-value items, aggregate resolved schema nodes, and entries
    /// retained in one table-local index.
    ///
    /// These structures all scale with query/schema complexity, so one bound keeps the initial
    /// semantic API compact while preventing any of them from growing independently without a
    /// caller-selected limit.
    #[must_use]
    pub const fn max_resolved_nodes(self) -> usize {
        self.resolved_nodes
    }

    /// Maximum aggregate `(schema node, path component)` states visited.
    #[must_use]
    pub const fn max_path_states(self) -> usize {
        self.path_states
    }

    /// Maximum aggregate variable-dictionary entries examined.
    #[must_use]
    pub const fn max_dictionary_entries_scanned(self) -> usize {
        self.dictionary_entries
    }

    /// Maximum aggregate matching variable-dictionary IDs retained.
    #[must_use]
    pub const fn max_dictionary_matches(self) -> usize {
        self.dictionary_matches
    }

    /// Maximum live tri-state row bytes during one table evaluation.
    #[must_use]
    pub const fn max_live_bitmap_bytes(self) -> usize {
        self.bitmap_bytes
    }

    /// Maximum bytes in one reconstructed CLP string.
    #[must_use]
    pub const fn max_reconstructed_string_bytes(self) -> usize {
        self.string_scratch_bytes
    }

    /// Maximum states visited while resolving one range-index predicate over one range.
    #[must_use]
    pub const fn max_range_traversal_states(self) -> usize {
        self.range_states
    }

    /// Maximum JSON values and object keys visited in one reconstructed array.
    #[must_use]
    pub const fn max_array_json_states(self) -> usize {
        self.array_states
    }

    /// Maximum container nesting in one reconstructed array.
    #[must_use]
    pub const fn max_array_nesting_depth(self) -> usize {
        self.array_nesting_depth
    }

    /// Maximum bytes retained while decoding one array string or object key.
    #[must_use]
    pub const fn max_array_decoded_string_bytes(self) -> usize {
        self.array_string_bytes
    }
}

impl Default for SearchLimits {
    fn default() -> Self {
        Self::new(
            1_048_576,
            65_536,
            1_048_576,
            4_194_304,
            16_777_216,
            4_194_304,
            256 * 1024 * 1024,
        )
    }
}

/// Semantic matching options.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SearchOptions {
    ignore_case: bool,
    limits: SearchLimits,
    authoritative_timestamp_range: AuthoritativeTimestampRange,
}

impl SearchOptions {
    /// Creates options with an explicit case policy and resource limits.
    #[must_use]
    pub const fn new(ignore_case: bool, limits: SearchLimits) -> Self {
        Self {
            ignore_case,
            limits,
            authoritative_timestamp_range: AuthoritativeTimestampRange::unbounded(),
        }
    }

    /// Adds inclusive epoch-millisecond bounds on the archive's authoritative timestamp column.
    #[must_use]
    pub const fn with_authoritative_timestamp_range(
        mut self,
        range: AuthoritativeTimestampRange,
    ) -> Self {
        self.authoritative_timestamp_range = range;
        self
    }

    /// Returns whether string comparisons fold ASCII case.
    ///
    /// This deliberately does not perform Unicode case folding. It matches the characterized C++
    /// behavior under the repository's `C.UTF-8` locale.
    #[must_use]
    pub const fn ignore_case(self) -> bool {
        self.ignore_case
    }

    /// Returns semantic compilation and matching limits.
    #[must_use]
    pub const fn limits(self) -> SearchLimits {
        self.limits
    }

    /// Returns the inclusive authoritative timestamp range.
    #[must_use]
    pub const fn authoritative_timestamp_range(self) -> AuthoritativeTimestampRange {
        self.authoritative_timestamp_range
    }
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self::new(false, SearchLimits::default())
    }
}

/// Inclusive epoch-millisecond bounds applied to the authoritative timestamp column.
///
/// An unbounded side is represented by `None`. Compilation rejects a lower bound greater than the
/// upper bound, matching the C++ CLI contract.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AuthoritativeTimestampRange {
    begin_milliseconds: Option<i64>,
    end_milliseconds: Option<i64>,
}

impl AuthoritativeTimestampRange {
    /// Creates inclusive optional lower and upper epoch-millisecond bounds.
    #[must_use]
    pub const fn new(begin_milliseconds: Option<i64>, end_milliseconds: Option<i64>) -> Self {
        Self {
            begin_milliseconds,
            end_milliseconds,
        }
    }

    /// Creates a range with neither bound, which leaves the query unchanged.
    #[must_use]
    pub const fn unbounded() -> Self {
        Self::new(None, None)
    }

    /// Returns the inclusive lower epoch-millisecond bound.
    #[must_use]
    pub const fn begin_milliseconds(self) -> Option<i64> {
        self.begin_milliseconds
    }

    /// Returns the inclusive upper epoch-millisecond bound.
    #[must_use]
    pub const fn end_milliseconds(self) -> Option<i64> {
        self.end_milliseconds
    }

    const fn is_unbounded(self) -> bool {
        self.begin_milliseconds.is_none() && self.end_milliseconds.is_none()
    }
}

/// A resource bounded by semantic compilation or matching.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum SearchResource {
    /// Schema-tree indexing.
    SchemaNodes,
    /// Physical archive schema tables.
    ArchiveTables,
    /// Resolved schema nodes retained by predicates.
    ResolvedNodes,
    /// Schema path traversal states.
    PathStates,
    /// Variable-dictionary entries examined.
    DictionaryEntries,
    /// Matching variable-dictionary IDs retained.
    DictionaryMatches,
    /// Live tri-state row bytes.
    BitmapBytes,
    /// Reconstructed CLP string bytes.
    StringScratchBytes,
    /// Range-index path traversal states.
    RangeStates,
    /// JSON values and keys visited in one unstructured array.
    ArrayStates,
    /// Unstructured-array JSON container nesting.
    ArrayNestingDepth,
    /// Decoded unstructured-array key or string scratch bytes.
    ArrayStringBytes,
    /// Compiled expression or predicate storage.
    CompiledProgram,
    /// Table-local schema presence and column indexes.
    TableIndex,
}

impl Display for SearchResource {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::SchemaNodes => "schema nodes",
            Self::ArchiveTables => "archive schema tables",
            Self::ResolvedNodes => "resolved predicate nodes",
            Self::PathStates => "schema path traversal states",
            Self::DictionaryEntries => "variable-dictionary entries scanned",
            Self::DictionaryMatches => "matching variable-dictionary IDs",
            Self::BitmapBytes => "live search bitmap bytes",
            Self::StringScratchBytes => "reconstructed CLP string bytes",
            Self::RangeStates => "range-index traversal states",
            Self::ArrayStates => "unstructured-array JSON traversal states",
            Self::ArrayNestingDepth => "unstructured-array JSON nesting depth",
            Self::ArrayStringBytes => "decoded unstructured-array string bytes",
            Self::CompiledProgram => "compiled query program",
            Self::TableIndex => "table-local search index",
        })
    }
}

/// A feature accepted by parsing but not yet supported by archive semantic matching.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum UnsupportedSearchFeature {
    /// Legacy pre-v0.5 date-string columns.
    DeprecatedDateString,
    /// A range operation whose literal is not numeric.
    NonNumericRangeOperand,
    /// A terminal range-index value outside the first scalar subset.
    RangeIndexValue,
}

impl Display for UnsupportedSearchFeature {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::DeprecatedDateString => {
                formatter.write_str("pre-v0.5 deprecated date-string evaluation")
            }
            Self::NonNumericRangeOperand => {
                formatter.write_str("range comparison with a nonnumeric operand")
            }
            Self::RangeIndexValue => formatter.write_str("nonscalar range-index value evaluation"),
        }
    }
}

/// Structured semantic compilation or table matching failure.
#[derive(Debug)]
#[non_exhaustive]
pub enum SearchError {
    /// A configured resource bound was exceeded before dangerous growth.
    LimitExceeded {
        /// Bounded resource.
        resource: SearchResource,
        /// Required or observed amount.
        actual: usize,
        /// Configured maximum.
        limit: usize,
    },
    /// Checked arithmetic overflowed.
    SizeOverflow,
    /// A checked allocation failed.
    AllocationFailed {
        /// Allocation being grown.
        resource: SearchResource,
        /// Additional elements or bytes requested.
        requested: usize,
    },
    /// Inclusive authoritative timestamp bounds are reversed.
    InvalidAuthoritativeTimestampRange {
        /// Inclusive lower epoch-millisecond bound.
        begin_milliseconds: i64,
        /// Inclusive upper epoch-millisecond bound.
        end_milliseconds: i64,
    },
    /// Timestamp bounds were requested, but the archive has no authoritative timestamp range.
    MissingAuthoritativeTimestamp,
    /// Scaling one authoritative millisecond bound to epoch nanoseconds overflowed.
    AuthoritativeTimestampOutOfRange {
        /// Epoch-millisecond bound that cannot be represented as epoch nanoseconds.
        milliseconds: i64,
    },
    /// A parsed `timestamp(...)` literal could not be resolved.
    TimestampLiteral {
        /// Query expression containing the literal.
        node: NodeId,
        /// Timestamp pattern/value failure.
        source: TimestampQueryError,
    },
    /// A parser-supported construct is outside this evaluator milestone.
    Unsupported {
        /// Expression containing the construct.
        node: NodeId,
        /// Unsupported semantic feature.
        feature: UnsupportedSearchFeature,
    },
    /// An expression arena dependency is not before its consumer.
    MalformedExpression {
        /// Invalid consumer.
        node: NodeId,
        /// Invalid operand ID.
        operand: NodeId,
    },
    /// The query root is outside its expression arena.
    InvalidRoot {
        /// Invalid root ID.
        root: NodeId,
        /// Arena length.
        node_count: usize,
    },
    /// The decoded table did not originate from the catalog used for compilation.
    ForeignTable {
        /// Supplied physical table index.
        table_index: usize,
    },
    /// A schema's unordered structured-array/object region is not safe to evaluate.
    StructuredSchema {
        /// Supplied physical table index.
        table_index: usize,
        /// Opaque archive schema ID.
        schema_id: i32,
        /// Bounded structural validation failure.
        source: ExtractionPlanError,
    },
    /// A reconstructed CLP value was corrupt or exceeded its configured bound.
    ClpString {
        /// Query expression whose scan failed.
        node: NodeId,
        /// Physical row within the schema table.
        row: usize,
        /// Reconstruction failure.
        source: EncodedVariableError,
    },
    /// An unstructured-array value could not be reconstructed from its CLP encoding.
    UnstructuredArrayReconstruction {
        /// Query expression whose scan failed.
        node: NodeId,
        /// Physical row within the schema table.
        row: usize,
        /// Reconstruction failure.
        source: EncodedVariableError,
    },
    /// A reconstructed unstructured-array value contains corrupt JSON.
    UnstructuredArrayJson {
        /// Query expression whose scan failed.
        node: NodeId,
        /// Physical row within the schema table.
        row: usize,
        /// UTF-8 or JSON syntax failure.
        source: ArraySearchError,
    },
}

impl Display for SearchError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::LimitExceeded {
                resource,
                actual,
                limit,
            } => write!(
                formatter,
                "{resource} amount {actual} exceeds search limit {limit}"
            ),
            Self::SizeOverflow => formatter.write_str("search size arithmetic overflow"),
            Self::AllocationFailed {
                resource,
                requested,
            } => write!(
                formatter,
                "failed to reserve {requested} additional elements or bytes for {resource}"
            ),
            Self::InvalidAuthoritativeTimestampRange {
                begin_milliseconds,
                end_milliseconds,
            } => write!(
                formatter,
                "authoritative timestamp begin {begin_milliseconds} ms is after end \
                 {end_milliseconds} ms"
            ),
            Self::MissingAuthoritativeTimestamp => formatter.write_str(
                "authoritative timestamp bounds were requested, but the archive has no \
                 authoritative timestamp column",
            ),
            Self::AuthoritativeTimestampOutOfRange { milliseconds } => write!(
                formatter,
                "authoritative timestamp bound {milliseconds} ms does not fit epoch nanoseconds"
            ),
            Self::TimestampLiteral { node, source } => write!(
                formatter,
                "query expression {} has an invalid timestamp literal: {source}",
                node.index()
            ),
            Self::Unsupported { node, feature } => write!(
                formatter,
                "query expression {} uses unsupported {feature}",
                node.index()
            ),
            Self::MalformedExpression { node, operand } => write!(
                formatter,
                "query expression {} references non-prior operand {}",
                node.index(),
                operand.index()
            ),
            Self::InvalidRoot { root, node_count } => write!(
                formatter,
                "query root {} is outside its {node_count}-node arena",
                root.index()
            ),
            Self::ForeignTable { table_index } => write!(
                formatter,
                "schema table {table_index} did not originate from the compiled archive catalog"
            ),
            Self::StructuredSchema {
                table_index,
                schema_id,
                source,
            } => write!(
                formatter,
                "schema table {table_index} with schema ID {schema_id} has an invalid structured \
                 region: {source}"
            ),
            Self::ClpString { node, row, source } => write!(
                formatter,
                "query expression {} failed to reconstruct CLP string at row {row}: {source}",
                node.index()
            ),
            Self::UnstructuredArrayReconstruction { node, row, source } => write!(
                formatter,
                "query expression {} failed to reconstruct unstructured array at row {row}: \
                 {source}",
                node.index()
            ),
            Self::UnstructuredArrayJson { node, row, source } => write!(
                formatter,
                "query expression {} found corrupt unstructured-array JSON at row {row}: {source}",
                node.index()
            ),
        }
    }
}

impl Error for SearchError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ClpString { source, .. }
            | Self::UnstructuredArrayReconstruction { source, .. } => Some(source),
            Self::UnstructuredArrayJson { source, .. } => Some(source),
            Self::TimestampLiteral { source, .. } => Some(source),
            Self::StructuredSchema { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// A parsed query compiled once against one archive's catalog.
///
/// Schema paths and dictionary matches are resolved once. The resulting program can match every
/// lazily decoded table from the same catalog without serializing records as JSON.
#[derive(Debug)]
pub struct CompiledQuery<'query, 'archive> {
    query: &'query ParsedQuery,
    catalog: &'archive ArchiveCatalog,
    options: SearchOptions,
    nodes: Vec<CompiledNode>,
    predicates: Vec<CompiledPredicate>,
    lists: Vec<CompiledList>,
    authoritative_timestamp_range: Option<CompiledAuthoritativeTimestampRange>,
}

impl ParsedQuery {
    /// Compiles this archive-independent AST against one validated archive catalog.
    ///
    /// # Errors
    ///
    /// Returns [`SearchError`] for configured limits, checked allocation/arithmetic failures,
    /// malformed arena dependencies, or an explicitly unsupported parsed feature. Timestamp
    /// literals and pre-v0.5 deprecated date semantics are deliberately not approximated.
    pub fn compile_for_archive<'query, 'archive>(
        &'query self,
        catalog: &'archive ArchiveCatalog,
        options: SearchOptions,
    ) -> Result<CompiledQuery<'query, 'archive>, SearchError> {
        CompiledQuery::compile(self, catalog, options, None)
    }

    /// Compiles against one archive, reusing the dictionary matches in `matches` and recording
    /// any it had to resolve.
    ///
    /// A scan that compiles the same query once per packed stream pays for the dictionary once
    /// this way rather than once per stream. The cache belongs to one query and one archive;
    /// using it with either changed answers from the wrong dictionary.
    ///
    /// # Errors
    ///
    /// As for [`Self::compile_for_archive`].
    pub fn compile_for_archive_reusing<'query, 'archive>(
        &'query self,
        catalog: &'archive ArchiveCatalog,
        options: SearchOptions,
        matches: &mut DictionaryMatches,
    ) -> Result<CompiledQuery<'query, 'archive>, SearchError> {
        CompiledQuery::compile(self, catalog, options, Some(matches))
    }
}

impl<'query, 'archive> CompiledQuery<'query, 'archive> {
    fn compile(
        query: &'query ParsedQuery,
        catalog: &'archive ArchiveCatalog,
        options: SearchOptions,
        matches: Option<&mut DictionaryMatches>,
    ) -> Result<Self, SearchError> {
        let limits = options.limits();
        check_limit(
            SearchResource::CompiledProgram,
            query.nodes().len(),
            limits.max_resolved_nodes(),
        )?;
        let schema_index = SchemaIndex::new(catalog.schema_tree(), limits)?;
        validate_archive_tables(catalog, limits)?;
        let authoritative_timestamp_range =
            compile_authoritative_timestamp_range(catalog, options)?;
        let mut builder = Compiler::new(query, catalog, options, schema_index, matches)?;
        builder.compile_nodes()?;
        Ok(Self {
            query,
            catalog,
            options,
            nodes: builder.nodes,
            predicates: builder.predicates,
            lists: builder.lists,
            authoritative_timestamp_range,
        })
    }

    /// Matches one lazily decoded schema table in physical row order.
    ///
    /// The supplied table must originate from the catalog used to compile this query. Returned
    /// row indexes are table-local; callers can pair them with `DecodedSchemaTable::table_index()`
    /// while preserving the archive's physical schema-table order.
    ///
    /// # Errors
    ///
    /// Returns a structured error for a foreign table, bitmap/scratch limits, checked allocation,
    /// unsupported terminal range-index values, or corrupt CLP reconstruction.
    /// Returns the node index of a range-index predicate every matching row must satisfy.
    ///
    /// Only the root's top-level conjunction qualifies. Under an `OR` a rejected row can still
    /// match through the other branch, and under a `NOT` the accepted rows are the complement of a
    /// run rather than a run, so neither is descended into.
    fn guarded_range_index_node(&self) -> Option<usize> {
        let mut pending = vec![self.query.root()];
        let mut found = None;
        while let Some(node) = pending.pop() {
            let index = node.index();
            match self.nodes.get(index)? {
                CompiledNode::Boolean {
                    operator: BooleanOperator::And,
                    left,
                    right,
                } => {
                    pending.push(*left);
                    pending.push(*right);
                }
                CompiledNode::Predicate(predicate_index) => {
                    let predicate = self.predicates.get(*predicate_index)?;
                    if matches!(predicate.resolved, ResolvedPath::RangeIndex) && !predicate.negated
                    {
                        if found.is_some() {
                            // Two selectors accept two runs whose union need not be one run.
                            return None;
                        }
                        found = Some(index);
                    }
                }
                _ => {}
            }
        }
        found
    }

    /// Returns the log-event ranges this query's range-index selector accepts.
    ///
    /// `None` means no single selector governs every match, so every row of every table is a
    /// candidate. Otherwise a row whose `log_event_idx` falls outside every returned range cannot
    /// match, which lets a reader stop inflating a column once it is past the last such row.
    ///
    /// # Errors
    ///
    /// Returns the same evaluation errors testing the selector against the range index would.
    pub fn accepted_log_event_ranges(&self) -> Result<Option<Vec<(u64, u64)>>, SearchError> {
        let Some(node_index) = self.guarded_range_index_node() else {
            return Ok(None);
        };
        let Some(CompiledNode::Predicate(predicate_index)) = self.nodes.get(node_index).copied()
        else {
            return Ok(None);
        };
        let compiled = self
            .predicates
            .get(predicate_index)
            .ok_or(SearchError::SizeOverflow)?;
        let Some(range_index) = self.catalog.metadata().range_index() else {
            return Ok(None);
        };
        let expression = self
            .query
            .node(compiled.expression)
            .ok_or(SearchError::SizeOverflow)?;
        let ExpressionKind::Predicate(predicate) = expression.kind() else {
            return Ok(None);
        };
        let predicate = PredicateRef::from_predicate(predicate);
        let mut work = Vec::new();
        let mut accepted = Vec::new();
        for entry in range_index.entries() {
            let result = evaluate_range_fields(
                entry.fields(),
                compiled.expression,
                compiled.negated,
                predicate,
                self.options,
                &mut work,
            )?;
            if Tri::True == result {
                accepted.push((entry.start_index(), entry.end_index()));
            }
        }
        Ok(Some(accepted))
    }

    pub fn match_table(
        &self,
        decoded: &DecodedSchemaTable<'_, '_>,
    ) -> Result<MatchBitmap, SearchError> {
        self.validate_table(decoded)?;
        let table_index = TableIndex::new(decoded, self.catalog, self.options.limits())?;
        Evaluator::new(self, decoded, table_index).evaluate()
    }

    /// Returns the semantic options captured at compilation.
    #[must_use]
    pub const fn options(&self) -> SearchOptions {
        self.options
    }

    /// Returns whether C++'s metadata, timestamp-index, and schema preflight reaches output setup.
    ///
    /// This is deliberately weaker than [`Self::may_match_archive`]. In particular, a predicate
    /// whose value is absent from the variable dictionary still reaches output setup before C++
    /// proves that it has no matching rows. Keeping that boundary explicit lets streaming sinks
    /// reproduce observable setup effects without decompressing a packed stream.
    pub(crate) fn reaches_match_sink(&self) -> Result<bool, SearchError> {
        if self
            .authoritative_timestamp_range
            .as_ref()
            .is_some_and(|range| range.archive_disjoint)
            || Tri::False == self.evaluate_range_preflight()?
            || Tri::False == self.evaluate_timestamp_preflight()?
        {
            return Ok(false);
        }
        self.evaluate_schema_preflight()
    }

    fn evaluate_range_preflight(&self) -> Result<Tri, SearchError> {
        let mut states = preflight_states(self.nodes.len())?;
        let mut range_work = Vec::new();
        for (index, node) in self.nodes.iter().copied().enumerate() {
            let value = match node {
                CompiledNode::Predicate(predicate_index) => {
                    self.evaluate_range_predicate(predicate_index, &mut range_work)?
                }
                CompiledNode::List(list_index) => {
                    self.evaluate_range_list(list_index, &mut range_work)?
                }
                CompiledNode::Not(operand) => {
                    invert_tri(preflight_operand(&states, NodeId::new(index), operand)?)
                }
                CompiledNode::Boolean {
                    operator,
                    left,
                    right,
                } => combine_tri(
                    preflight_operand(&states, NodeId::new(index), left)?,
                    preflight_operand(&states, NodeId::new(index), right)?,
                    operator,
                ),
            };
            states.push(value);
        }
        preflight_root(self.query, &states)
    }

    fn evaluate_range_predicate(
        &self,
        predicate_index: usize,
        range_work: &mut Vec<RangeState<'archive>>,
    ) -> Result<Tri, SearchError> {
        let compiled = self
            .predicates
            .get(predicate_index)
            .ok_or(SearchError::SizeOverflow)?;
        let predicate = self.predicate_ref(compiled)?;
        self.evaluate_range_leaf(compiled.expression, compiled.negated, predicate, range_work)
    }

    fn evaluate_range_list(
        &self,
        list_index: usize,
        range_work: &mut Vec<RangeState<'archive>>,
    ) -> Result<Tri, SearchError> {
        let compiled = self
            .lists
            .get(list_index)
            .ok_or(SearchError::SizeOverflow)?;
        let list = self.list_ref(compiled)?;
        let Some(ResolvedPath::RangeIndex) = &compiled.resolved else {
            return Ok(Tri::Unknown);
        };
        if list.values().is_empty() {
            return Ok(match list.operator() {
                ListOperator::Any => Tri::False,
                ListOperator::All | ListOperator::None => Tri::True,
            });
        }
        let (operator, invert_each) = match list.operator() {
            ListOperator::Any => (BooleanOperator::Or, false),
            ListOperator::All => (BooleanOperator::And, false),
            ListOperator::None => (BooleanOperator::And, true),
        };
        let mut aggregate = None;
        for literal in list.values() {
            let predicate = PredicateRef::equality(list.path(), literal);
            let mut value = self.evaluate_range_leaf(
                compiled.expression,
                compiled.negated ^ invert_each,
                predicate,
                range_work,
            )?;
            if invert_each {
                value = invert_tri(value);
            }
            aggregate =
                Some(aggregate.map_or(value, |current| combine_tri(current, value, operator)));
        }
        aggregate.ok_or(SearchError::SizeOverflow)
    }

    fn evaluate_range_leaf(
        &self,
        expression: NodeId,
        negated: bool,
        predicate: PredicateRef<'_>,
        range_work: &mut Vec<RangeState<'archive>>,
    ) -> Result<Tri, SearchError> {
        self.evaluate_range_leaf_in_span(expression, negated, predicate, range_work, None)
    }

    /// Evaluates one range-index leaf, optionally restricted to a record span.
    ///
    /// `span` limits the entries considered to those overlapping `[start, end)`. Entries outside
    /// it cannot decide rows inside it, so ignoring them is what lets a caller ask whether a span
    /// is worth reading at all.
    fn evaluate_range_leaf_in_span(
        &self,
        expression: NodeId,
        negated: bool,
        predicate: PredicateRef<'_>,
        range_work: &mut Vec<RangeState<'archive>>,
        span: Option<(u64, u64)>,
    ) -> Result<Tri, SearchError> {
        if ColumnNamespace::RangeIndex != predicate.path().namespace() {
            return Ok(Tri::Unknown);
        }
        let Some(range_index) = self.catalog.metadata().range_index() else {
            return Ok(Tri::False);
        };
        for entry in range_index.entries() {
            if let Some((start, end)) = span
                && (entry.end_index() <= start || entry.start_index() >= end)
            {
                continue;
            }
            if Tri::True
                == evaluate_range_fields(
                    entry.fields(),
                    expression,
                    negated,
                    predicate,
                    self.options,
                    range_work,
                )?
            {
                // C++ rewrites a matching metadata range to `_log_event_idx` bounds. Those bounds
                // remain data-dependent until table evaluation, so they are not a constant true.
                return Ok(Tri::Unknown);
            }
        }
        Ok(Tri::False)
    }

    fn evaluate_timestamp_preflight(&self) -> Result<Tri, SearchError> {
        let mut states = preflight_states(self.nodes.len())?;
        for (index, node) in self.nodes.iter().copied().enumerate() {
            let value = match node {
                CompiledNode::Predicate(predicate_index) => {
                    self.evaluate_timestamp_predicate(predicate_index)?
                }
                CompiledNode::List(list_index) => self.evaluate_timestamp_list(list_index)?,
                CompiledNode::Not(operand) => {
                    invert_tri(preflight_operand(&states, NodeId::new(index), operand)?)
                }
                CompiledNode::Boolean {
                    operator,
                    left,
                    right,
                } => combine_tri(
                    preflight_operand(&states, NodeId::new(index), left)?,
                    preflight_operand(&states, NodeId::new(index), right)?,
                    operator,
                ),
            };
            states.push(value);
        }
        preflight_root(self.query, &states)
    }

    fn evaluate_timestamp_predicate(&self, predicate_index: usize) -> Result<Tri, SearchError> {
        let compiled = self
            .predicates
            .get(predicate_index)
            .ok_or(SearchError::SizeOverflow)?;
        let predicate = self.predicate_ref(compiled)?;
        Ok(self.evaluate_timestamp_leaf(&compiled.resolved, &compiled.value, predicate))
    }

    fn evaluate_timestamp_list(&self, list_index: usize) -> Result<Tri, SearchError> {
        let compiled = self
            .lists
            .get(list_index)
            .ok_or(SearchError::SizeOverflow)?;
        let list = self.list_ref(compiled)?;
        let Some(resolved) = &compiled.resolved else {
            return Ok(match list.operator() {
                ListOperator::Any => Tri::False,
                ListOperator::All | ListOperator::None => Tri::True,
            });
        };
        if list.values().is_empty() {
            return Ok(match list.operator() {
                ListOperator::Any => Tri::False,
                ListOperator::All | ListOperator::None => Tri::True,
            });
        }
        if compiled.values.len() != list.values().len() {
            return Err(SearchError::SizeOverflow);
        }
        let (operator, invert_each) = match list.operator() {
            ListOperator::Any => (BooleanOperator::Or, false),
            ListOperator::All => (BooleanOperator::And, false),
            ListOperator::None => (BooleanOperator::And, true),
        };
        let mut aggregate = None;
        for (literal, compiled_value) in list.values().iter().zip(&compiled.values) {
            let predicate = PredicateRef::equality(list.path(), literal);
            let mut value = self.evaluate_timestamp_leaf(resolved, compiled_value, predicate);
            if invert_each {
                value = invert_tri(value);
            }
            aggregate =
                Some(aggregate.map_or(value, |current| combine_tri(current, value, operator)));
        }
        aggregate.ok_or(SearchError::SizeOverflow)
    }

    fn evaluate_timestamp_leaf(
        &self,
        resolved: &ResolvedPath,
        compiled: &CompiledValue,
        predicate: PredicateRef<'_>,
    ) -> Tri {
        let ResolvedPath::Schema(schema) = resolved else {
            return Tri::Unknown;
        };
        if predicate
            .path()
            .components()
            .iter()
            .any(PathComponent::is_wildcard)
        {
            return Tri::Unknown;
        }
        let Some(range) = self
            .catalog
            .metadata()
            .timestamp_dictionary()
            .ranges()
            .iter()
            .find(|range| {
                range.column_ids().iter().any(|column_id| {
                    u32::try_from(*column_id)
                        .is_ok_and(|column_id| schema.nodes.binary_search(&column_id).is_ok())
                })
            })
        else {
            return Tri::Unknown;
        };
        timestamp_bounds_filter(range.bounds(), compiled, predicate)
    }

    fn evaluate_schema_preflight(&self) -> Result<bool, SearchError> {
        for schema in self.catalog.schema_map().schemas() {
            if Tri::False != self.evaluate_schema_program(schema)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn evaluate_schema_program(
        &self,
        schema: &crate::archive::SchemaDefinition,
    ) -> Result<Tri, SearchError> {
        let mut states = preflight_states(self.nodes.len())?;
        for (index, node) in self.nodes.iter().copied().enumerate() {
            let value = match node {
                CompiledNode::Predicate(predicate_index) => {
                    self.evaluate_schema_predicate(predicate_index, schema)?
                }
                CompiledNode::List(list_index) => self.evaluate_schema_list(list_index, schema)?,
                // Schema matching computes applicability sets. Negation changes value matching,
                // not whether a schema contains the required typed column.
                CompiledNode::Not(operand) => {
                    preflight_operand(&states, NodeId::new(index), operand)?
                }
                CompiledNode::Boolean {
                    operator,
                    left,
                    right,
                } => combine_tri(
                    preflight_operand(&states, NodeId::new(index), left)?,
                    preflight_operand(&states, NodeId::new(index), right)?,
                    operator,
                ),
            };
            states.push(value);
        }
        preflight_root(self.query, &states)
    }

    fn evaluate_schema_predicate(
        &self,
        predicate_index: usize,
        schema: &crate::archive::SchemaDefinition,
    ) -> Result<Tri, SearchError> {
        let compiled = self
            .predicates
            .get(predicate_index)
            .ok_or(SearchError::SizeOverflow)?;
        let predicate = self.predicate_ref(compiled)?;
        Ok(self.evaluate_schema_leaf(&compiled.resolved, compiled.negated, predicate, schema))
    }

    fn evaluate_schema_list(
        &self,
        list_index: usize,
        schema: &crate::archive::SchemaDefinition,
    ) -> Result<Tri, SearchError> {
        let compiled = self
            .lists
            .get(list_index)
            .ok_or(SearchError::SizeOverflow)?;
        let list = self.list_ref(compiled)?;
        let Some(resolved) = &compiled.resolved else {
            return Ok(match list.operator() {
                ListOperator::Any => Tri::False,
                ListOperator::All | ListOperator::None => Tri::True,
            });
        };
        if list.values().is_empty() {
            return Ok(match list.operator() {
                ListOperator::Any => Tri::False,
                ListOperator::All | ListOperator::None => Tri::True,
            });
        }
        let operator = match list.operator() {
            ListOperator::Any => BooleanOperator::Or,
            ListOperator::All | ListOperator::None => BooleanOperator::And,
        };
        let mut aggregate = None;
        for literal in list.values() {
            let predicate = PredicateRef::equality(list.path(), literal);
            let value = self.evaluate_schema_leaf(resolved, compiled.negated, predicate, schema);
            aggregate =
                Some(aggregate.map_or(value, |current| combine_tri(current, value, operator)));
        }
        aggregate.ok_or(SearchError::SizeOverflow)
    }

    fn evaluate_schema_leaf(
        &self,
        resolved: &ResolvedPath,
        negated: bool,
        predicate: PredicateRef<'_>,
        schema: &crate::archive::SchemaDefinition,
    ) -> Tri {
        let ResolvedPath::Schema(resolved) = resolved else {
            return Tri::Unknown;
        };
        if resolved.nodes.is_empty() && resolved.arrays.is_empty() {
            return Tri::False;
        }
        let any_path_present = resolved
            .nodes
            .iter()
            .copied()
            .any(|node_id| schema_contains_node(schema, node_id))
            || resolved
                .arrays
                .iter()
                .any(|target| schema_contains_node(schema, target.node_id));
        if is_exists(predicate) && negated {
            return if any_path_present {
                Tri::False
            } else {
                Tri::Unknown
            };
        }
        let compatible_node_present = resolved.nodes.iter().copied().any(|node_id| {
            if !schema_contains_node(schema, node_id) {
                return false;
            }
            self.catalog
                .schema_tree()
                .get(node_id as usize)
                .is_some_and(|node| {
                    let node_type = node.node_type();
                    if is_exists(predicate) || negated && matches!(predicate.value(), Literal::Null)
                    {
                        is_search_value_node(node_type)
                    } else {
                        (!opposite_string_class(node_type, predicate)
                            && node_compatible(node_type, predicate))
                            || (negated && opposite_string_class(node_type, predicate))
                    }
                })
        });
        if compatible_node_present
            || resolved
                .arrays
                .iter()
                .any(|target| schema_contains_node(schema, target.node_id))
        {
            Tri::Unknown
        } else {
            Tri::False
        }
    }

    fn predicate_ref<'compiled>(
        &'compiled self,
        compiled: &CompiledPredicate,
    ) -> Result<PredicateRef<'compiled>, SearchError> {
        let expression = self
            .query
            .node(compiled.expression)
            .ok_or(SearchError::SizeOverflow)?;
        let ExpressionKind::Predicate(predicate) = expression.kind() else {
            return Err(SearchError::SizeOverflow);
        };
        Ok(PredicateRef::from_predicate(predicate))
    }

    fn list_ref<'compiled>(
        &'compiled self,
        compiled: &CompiledList,
    ) -> Result<&'compiled ListExpression, SearchError> {
        let expression = self
            .query
            .node(compiled.expression)
            .ok_or(SearchError::SizeOverflow)?;
        let ExpressionKind::List(list) = expression.kind() else {
            return Err(SearchError::SizeOverflow);
        };
        Ok(list)
    }

    /// Returns whether the compiled root might match at least one physical archive row.
    ///
    /// This deliberately recognizes only roots that can be proven empty without table data. A
    /// conservative `true` keeps all other expressions on the ordinary table evaluator path.
    pub(crate) fn may_match_archive(&self) -> bool {
        let predicate_index = match self.nodes.get(self.query.root().index()).copied() {
            Some(CompiledNode::Predicate(predicate_index)) => predicate_index,
            Some(CompiledNode::Not(operand)) => {
                let Some(CompiledNode::Predicate(predicate_index)) =
                    self.nodes.get(operand.index()).copied()
                else {
                    return true;
                };
                let Some(compiled) = self.predicates.get(predicate_index) else {
                    return true;
                };
                let ResolvedPath::Schema(schema) = &compiled.resolved else {
                    return true;
                };
                // The C++ schema matcher rejects a wholly unresolved path even below a root NOT.
                return self.schema_path_present(schema);
            }
            _ => return true,
        };
        let Some(compiled) = self.predicates.get(predicate_index) else {
            return true;
        };
        if compiled.negated {
            return true;
        }
        let Some(expression) = self.query.node(compiled.expression) else {
            return true;
        };
        let ExpressionKind::Predicate(predicate) = expression.kind() else {
            return true;
        };
        let ResolvedPath::Schema(schema) = &compiled.resolved else {
            return true;
        };
        let predicate = PredicateRef::from_predicate(predicate);
        if schema
            .arrays
            .iter()
            .any(|target| self.archive_has_node(target.node_id))
        {
            return true;
        }
        schema.nodes.iter().any(|&node_id| {
            if !self.archive_has_node(node_id) {
                return false;
            }
            let Some(node) = self.catalog.schema_tree().get(node_id as usize) else {
                return true;
            };
            if is_exists(predicate) {
                return is_search_value_node(node.node_type());
            }
            node_may_match_without_table(node.node_type(), predicate, &compiled.value)
        })
    }

    fn schema_path_present(&self, schema: &ResolvedSchemaPath) -> bool {
        schema
            .nodes
            .iter()
            .any(|&node_id| self.archive_has_node(node_id))
            || schema
                .arrays
                .iter()
                .any(|target| self.archive_has_node(target.node_id))
    }

    fn archive_has_node(&self, node_id: u32) -> bool {
        self.catalog
            .table_metadata()
            .schema_tables()
            .iter()
            .filter(|metadata| 0 != metadata.message_count())
            .any(|metadata| {
                self.catalog
                    .schema_map()
                    .get(metadata.schema_id())
                    .is_none_or(|schema| {
                        schema
                            .entries()
                            .iter()
                            .any(|entry| matches!(entry, SchemaEntry::Node(id) if node_id == *id))
                    })
            })
    }

    fn validate_table(&self, decoded: &DecodedSchemaTable<'_, '_>) -> Result<(), SearchError> {
        let table_index = decoded.table_index();
        let Some(metadata) = self
            .catalog
            .table_metadata()
            .schema_tables()
            .get(table_index)
        else {
            return Err(SearchError::ForeignTable { table_index });
        };
        let schema = self.catalog.schema_map().get(metadata.schema_id());
        if !std::ptr::eq(metadata, decoded.metadata())
            || schema.is_none_or(|schema| !std::ptr::eq(schema, decoded.schema()))
        {
            return Err(SearchError::ForeignTable { table_index });
        }
        Ok(())
    }
}

fn preflight_states(node_count: usize) -> Result<Vec<Tri>, SearchError> {
    let mut states = Vec::new();
    states
        .try_reserve_exact(node_count)
        .map_err(|_| allocation(SearchResource::CompiledProgram, node_count))?;
    Ok(states)
}

fn preflight_operand(states: &[Tri], node: NodeId, operand: NodeId) -> Result<Tri, SearchError> {
    states
        .get(operand.index())
        .copied()
        .ok_or(SearchError::MalformedExpression { node, operand })
}

fn preflight_root(query: &ParsedQuery, states: &[Tri]) -> Result<Tri, SearchError> {
    states
        .get(query.root().index())
        .copied()
        .ok_or_else(|| SearchError::InvalidRoot {
            root: query.root(),
            node_count: states.len(),
        })
}

const fn invert_tri(value: Tri) -> Tri {
    match value {
        Tri::False => Tri::True,
        Tri::True => Tri::False,
        Tri::Unknown => Tri::Unknown,
    }
}

const fn combine_tri(left: Tri, right: Tri, operator: BooleanOperator) -> Tri {
    let value = match operator {
        BooleanOperator::And => tri_and(left as u8, right as u8),
        BooleanOperator::Or => tri_or(left as u8, right as u8),
    };
    match value {
        value if value == Tri::False as u8 => Tri::False,
        value if value == Tri::True as u8 => Tri::True,
        _ => Tri::Unknown,
    }
}

fn schema_contains_node(schema: &crate::archive::SchemaDefinition, node_id: u32) -> bool {
    schema
        .entries()
        .iter()
        .any(|entry| matches!(entry, SchemaEntry::Node(candidate) if *candidate == node_id))
}

#[allow(clippy::cast_precision_loss)]
fn timestamp_bounds_filter(
    bounds: TimestampBounds,
    compiled: &CompiledValue,
    predicate: PredicateRef<'_>,
) -> Tri {
    if is_exists(predicate) {
        return Tri::Unknown;
    }
    match bounds {
        TimestampBounds::Unknown => Tri::Unknown,
        TimestampBounds::Epoch { start, end } => {
            let operand = compiled.timestamp_nanoseconds.map_or_else(
                || match numeric_literal(predicate.value()) {
                    Some(NumericLiteral::Integer(value)) => Some(value),
                    Some(NumericLiteral::Float(_)) | None => None,
                },
                |nanoseconds| Some(nanoseconds / 1_000_000),
            );
            operand.map_or(Tri::Unknown, |operand| {
                evaluate_timestamp_bounds(start, end, operand, predicate.operator())
            })
        }
        TimestampBounds::DoubleEpoch { start, end } => {
            let operand = compiled.timestamp_nanoseconds.map_or_else(
                || match numeric_literal(predicate.value()) {
                    Some(NumericLiteral::Integer(value)) => Some(value as f64),
                    Some(NumericLiteral::Float(value)) => Some(value),
                    None => None,
                },
                |nanoseconds| Some(timestamp_seconds(nanoseconds)),
            );
            operand.map_or(Tri::Unknown, |operand| {
                evaluate_timestamp_bounds(start, end, operand, predicate.operator())
            })
        }
    }
}

fn evaluate_timestamp_bounds<T: Copy + PartialEq + PartialOrd>(
    start: T,
    end: T,
    operand: T,
    operator: ComparisonOperator,
) -> Tri {
    match operator {
        ComparisonOperator::Equal => {
            if operand >= start && operand <= end {
                Tri::Unknown
            } else {
                Tri::False
            }
        }
        ComparisonOperator::Less => {
            if operand > end {
                Tri::True
            } else if operand <= start {
                Tri::False
            } else {
                Tri::Unknown
            }
        }
        ComparisonOperator::LessOrEqual => {
            if operand >= end {
                Tri::True
            } else if operand < start {
                Tri::False
            } else {
                Tri::Unknown
            }
        }
        ComparisonOperator::Greater => {
            if operand < start {
                Tri::True
            } else if operand >= end {
                Tri::False
            } else {
                Tri::Unknown
            }
        }
        ComparisonOperator::GreaterOrEqual => {
            if operand <= start {
                Tri::True
            } else if operand > end {
                Tri::False
            } else {
                Tri::Unknown
            }
        }
    }
}

fn node_may_match_without_table(
    node_type: NodeType,
    predicate: PredicateRef<'_>,
    value: &CompiledValue,
) -> bool {
    if opposite_string_class(node_type, predicate) || !node_compatible(node_type, predicate) {
        return false;
    }
    match node_type {
        NodeType::VarString => !value.variable_ids.is_empty(),
        NodeType::DictionaryFloat => !value.dictionary_float_ids.is_empty(),
        // Data-dependent types remain possible. In particular, evaluation deliberately reports
        // deprecated date strings instead of pruning them.
        _ => true,
    }
}

/// Reusable byte bitmap of matching physical rows.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MatchBitmap {
    bytes: Vec<u8>,
    match_count: usize,
}

impl MatchBitmap {
    /// Returns the number of rows represented.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Returns whether the table contained no rows.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Returns whether one table-local physical row matches.
    #[must_use]
    pub fn get(&self, row: usize) -> Option<bool> {
        self.bytes.get(row).map(|value| 0 != *value)
    }

    /// Returns the number of matching rows.
    #[must_use]
    pub const fn match_count(&self) -> usize {
        self.match_count
    }

    /// Iterates matching table-local physical row indexes without allocation.
    #[must_use]
    pub fn matching_rows(&self) -> MatchingRows<'_> {
        MatchingRows {
            bytes: self.bytes.iter().enumerate(),
            remaining: self.match_count,
        }
    }

    /// Returns the canonical `0`/`1` row bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// Iterator over matching table-local row indexes.
#[derive(Clone, Debug)]
pub struct MatchingRows<'a> {
    bytes: std::iter::Enumerate<std::slice::Iter<'a, u8>>,
    remaining: usize,
}

impl Iterator for MatchingRows<'_> {
    type Item = usize;

    fn next(&mut self) -> Option<Self::Item> {
        for (row, value) in &mut self.bytes {
            if 0 != *value {
                self.remaining -= 1;
                return Some(row);
            }
        }
        None
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl ExactSizeIterator for MatchingRows<'_> {}
impl FusedIterator for MatchingRows<'_> {}

#[derive(Clone, Copy, Debug)]
enum CompiledNode {
    Predicate(usize),
    List(usize),
    Not(NodeId),
    Boolean {
        operator: BooleanOperator,
        left: NodeId,
        right: NodeId,
    },
}

#[derive(Debug)]
struct CompiledPredicate {
    expression: NodeId,
    negated: bool,
    resolved: ResolvedPath,
    value: CompiledValue,
}

#[derive(Debug)]
struct CompiledList {
    expression: NodeId,
    negated: bool,
    resolved: Option<ResolvedPath>,
    values: Vec<CompiledValue>,
}

#[derive(Debug)]
struct CompiledValue {
    variable_ids: Vec<u64>,
    dictionary_float_ids: Vec<u64>,
    timestamp_nanoseconds: Option<i64>,
}

#[derive(Debug)]
struct CompiledAuthoritativeTimestampRange {
    node_ids: Vec<u32>,
    begin_nanoseconds: Option<i64>,
    end_nanoseconds: Option<i64>,
    archive_disjoint: bool,
}

fn compile_authoritative_timestamp_range(
    catalog: &ArchiveCatalog,
    options: SearchOptions,
) -> Result<Option<CompiledAuthoritativeTimestampRange>, SearchError> {
    let requested = options.authoritative_timestamp_range();
    if requested.is_unbounded() {
        return Ok(None);
    }
    if let (Some(begin), Some(end)) = (requested.begin_milliseconds(), requested.end_milliseconds())
        && begin > end
    {
        return Err(SearchError::InvalidAuthoritativeTimestampRange {
            begin_milliseconds: begin,
            end_milliseconds: end,
        });
    }
    let authoritative = catalog
        .metadata()
        .timestamp_dictionary()
        .authoritative_range()
        .ok_or(SearchError::MissingAuthoritativeTimestamp)?;
    check_limit(
        SearchResource::ResolvedNodes,
        authoritative.column_ids().len(),
        options.limits().max_resolved_nodes(),
    )?;
    let mut node_ids = Vec::new();
    node_ids
        .try_reserve_exact(authoritative.column_ids().len())
        .map_err(|_| {
            allocation(
                SearchResource::ResolvedNodes,
                authoritative.column_ids().len(),
            )
        })?;
    for &node_id in authoritative.column_ids() {
        node_ids.push(u32::try_from(node_id).map_err(|_| SearchError::SizeOverflow)?);
    }
    let begin_nanoseconds = requested
        .begin_milliseconds()
        .map(scale_milliseconds)
        .transpose()?;
    let end_nanoseconds = requested
        .end_milliseconds()
        .map(scale_milliseconds)
        .transpose()?;
    let archive_disjoint = authoritative_range_is_disjoint(requested, authoritative.bounds());
    Ok(Some(CompiledAuthoritativeTimestampRange {
        node_ids,
        begin_nanoseconds,
        end_nanoseconds,
        archive_disjoint,
    }))
}

fn scale_milliseconds(milliseconds: i64) -> Result<i64, SearchError> {
    milliseconds
        .checked_mul(1_000_000)
        .ok_or(SearchError::AuthoritativeTimestampOutOfRange { milliseconds })
}

#[allow(clippy::cast_precision_loss)]
fn authoritative_range_is_disjoint(
    requested: AuthoritativeTimestampRange,
    bounds: TimestampBounds,
) -> bool {
    match bounds {
        TimestampBounds::Unknown => false,
        TimestampBounds::Epoch { start, end } => {
            requested
                .begin_milliseconds()
                .is_some_and(|begin| begin > end)
                || requested
                    .end_milliseconds()
                    .is_some_and(|finish| finish < start)
        }
        TimestampBounds::DoubleEpoch { start, end } => {
            requested
                .begin_milliseconds()
                .is_some_and(|begin| begin as f64 / 1_000.0 > end)
                || requested
                    .end_milliseconds()
                    .is_some_and(|finish| finish as f64 / 1_000.0 < start)
        }
    }
}

#[derive(Debug)]
enum ResolvedPath {
    Schema(ResolvedSchemaPath),
    RangeIndex,
}

#[derive(Debug)]
struct ResolvedSchemaPath {
    nodes: Vec<u32>,
    arrays: Vec<ArrayTarget>,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct ArrayTarget {
    node_id: u32,
    /// First path component interpreted inside the reconstructed array.
    component: usize,
    /// A terminal schema wildcard recursively searches object values as well as array elements.
    recursive: bool,
}

#[derive(Clone, Copy)]
struct PredicateRef<'a> {
    path: &'a ColumnPath,
    operator: ComparisonOperator,
    value: &'a Literal,
}

impl<'a> PredicateRef<'a> {
    const fn from_predicate(predicate: &'a Predicate) -> Self {
        Self {
            path: predicate.path(),
            operator: predicate.operator(),
            value: predicate.value(),
        }
    }

    const fn equality(path: &'a ColumnPath, value: &'a Literal) -> Self {
        Self {
            path,
            operator: ComparisonOperator::Equal,
            value,
        }
    }

    const fn path(self) -> &'a ColumnPath {
        self.path
    }

    const fn operator(self) -> ComparisonOperator {
        self.operator
    }

    const fn value(self) -> &'a Literal {
        self.value
    }
}

#[derive(Clone, Copy)]
struct EvaluationFilter<'value, 'query> {
    expression: NodeId,
    negated: bool,
    value: &'value CompiledValue,
    predicate: PredicateRef<'query>,
}

struct Compiler<'query, 'archive, 'cache> {
    matches: Option<&'cache mut DictionaryMatches>,
    query: &'query ParsedQuery,
    catalog: &'archive ArchiveCatalog,
    options: SearchOptions,
    schema_index: SchemaIndex,
    nodes: Vec<CompiledNode>,
    predicates: Vec<CompiledPredicate>,
    lists: Vec<CompiledList>,
    counters: CompileCounters,
    path_work: PathWork,
    negated: Vec<bool>,
}

#[derive(Default)]
struct CompileCounters {
    compiled_program_items: usize,
    resolved_nodes: usize,
    path_states: usize,
    dictionary_entries: usize,
    dictionary_matches: usize,
}

impl<'query, 'archive, 'cache> Compiler<'query, 'archive, 'cache> {
    fn new(
        query: &'query ParsedQuery,
        catalog: &'archive ArchiveCatalog,
        options: SearchOptions,
        schema_index: SchemaIndex,
        matches: Option<&'cache mut DictionaryMatches>,
    ) -> Result<Self, SearchError> {
        let mut nodes = Vec::new();
        nodes
            .try_reserve_exact(query.nodes().len())
            .map_err(|_| allocation(SearchResource::CompiledProgram, query.nodes().len()))?;
        let mut predicates = Vec::new();
        predicates
            .try_reserve_exact(query.nodes().len())
            .map_err(|_| allocation(SearchResource::CompiledProgram, query.nodes().len()))?;
        let list_count = query
            .nodes()
            .iter()
            .filter(|node| matches!(node.kind(), ExpressionKind::List(_)))
            .count();
        let mut lists = Vec::new();
        lists
            .try_reserve_exact(list_count)
            .map_err(|_| allocation(SearchResource::CompiledProgram, list_count))?;
        let negated = compute_negations(query)?;
        Ok(Self {
            matches,
            query,
            catalog,
            options,
            schema_index,
            nodes,
            predicates,
            lists,
            counters: CompileCounters {
                compiled_program_items: query.nodes().len(),
                ..CompileCounters::default()
            },
            path_work: PathWork::default(),
            negated,
        })
    }

    fn compile_nodes(&mut self) -> Result<(), SearchError> {
        for (index, expression) in self.query.nodes().iter().enumerate() {
            let node = NodeId::new(index);
            let compiled = match expression.kind() {
                ExpressionKind::Predicate(predicate) => {
                    let predicate_index = self.compile_predicate(node, predicate)?;
                    CompiledNode::Predicate(predicate_index)
                }
                ExpressionKind::List(list) => {
                    let list_index = self.compile_list(node, list)?;
                    CompiledNode::List(list_index)
                }
                ExpressionKind::Not { operand } => {
                    check_prior(node, *operand)?;
                    CompiledNode::Not(*operand)
                }
                ExpressionKind::Boolean {
                    operator,
                    left,
                    right,
                } => {
                    check_prior(node, *left)?;
                    check_prior(node, *right)?;
                    CompiledNode::Boolean {
                        operator: *operator,
                        left: *left,
                        right: *right,
                    }
                }
            };
            self.nodes.push(compiled);
        }
        if self.query.root().index() >= self.nodes.len() {
            return Err(SearchError::InvalidRoot {
                root: self.query.root(),
                node_count: self.nodes.len(),
            });
        }
        Ok(())
    }

    fn compile_predicate(
        &mut self,
        expression: NodeId,
        predicate: &Predicate,
    ) -> Result<usize, SearchError> {
        let predicate = PredicateRef::from_predicate(predicate);
        let resolved = self.resolve_path(predicate.path())?;
        let value = self.compile_value(expression, 0, predicate, &resolved)?;
        let compiled = CompiledPredicate {
            expression,
            negated: *self
                .negated
                .get(expression.index())
                .ok_or(SearchError::SizeOverflow)?,
            resolved,
            value,
        };
        let index = self.predicates.len();
        self.predicates.push(compiled);
        Ok(index)
    }

    fn compile_list(
        &mut self,
        expression: NodeId,
        list: &ListExpression,
    ) -> Result<usize, SearchError> {
        add_limited(
            &mut self.counters.compiled_program_items,
            list.values().len(),
            self.options.limits().max_resolved_nodes(),
            SearchResource::CompiledProgram,
        )?;
        let negated = *self
            .negated
            .get(expression.index())
            .ok_or(SearchError::SizeOverflow)?;
        let resolved = if list.values().is_empty() {
            None
        } else {
            Some(self.resolve_path(list.path())?)
        };
        let mut values = Vec::new();
        values
            .try_reserve_exact(list.values().len())
            .map_err(|_| allocation(SearchResource::CompiledProgram, list.values().len()))?;
        if let Some(resolved) = &resolved {
            for (value_index, value) in list.values().iter().enumerate() {
                let predicate = PredicateRef::equality(list.path(), value);
                values.push(self.compile_value(expression, value_index, predicate, resolved)?);
            }
        }
        let index = self.lists.len();
        self.lists.push(CompiledList {
            expression,
            negated,
            resolved,
            values,
        });
        Ok(index)
    }

    fn resolve_path(&mut self, path: &ColumnPath) -> Result<ResolvedPath, SearchError> {
        let resolved = if ColumnNamespace::RangeIndex == path.namespace() {
            ResolvedPath::RangeIndex
        } else {
            let schema = self.schema_index.resolve(
                self.catalog.schema_tree(),
                path,
                &mut self.path_work,
                &mut self.counters,
                self.options.limits(),
            )?;
            ResolvedPath::Schema(schema)
        };
        Ok(resolved)
    }

    fn compile_value(
        &mut self,
        expression: NodeId,
        value_index: usize,
        predicate: PredicateRef<'_>,
        resolved: &ResolvedPath,
    ) -> Result<CompiledValue, SearchError> {
        let timestamp_nanoseconds = compile_literal(expression, predicate)?;
        let mut compiled = CompiledValue {
            variable_ids: Vec::new(),
            dictionary_float_ids: Vec::new(),
            timestamp_nanoseconds,
        };
        let ResolvedPath::Schema(schema) = resolved else {
            return Ok(compiled);
        };
        let tree = self.catalog.schema_tree();
        let has_var_string = schema.nodes.iter().any(|node_id| {
            tree.get(*node_id as usize)
                .is_some_and(|node| NodeType::VarString == node.node_type())
        });
        let has_dictionary_float = schema.nodes.iter().any(|node_id| {
            tree.get(*node_id as usize)
                .is_some_and(|node| NodeType::DictionaryFloat == node.node_type())
        });
        if has_var_string && variable_pattern(predicate).is_some() {
            compiled.variable_ids = self.matching_dictionary_ids(
                expression,
                value_index,
                predicate,
                DictionaryMatchKind::String,
            )?;
        }
        if has_dictionary_float {
            if numeric_literal(predicate.value()).is_some() {
                compiled.dictionary_float_ids = self.matching_dictionary_ids(
                    expression,
                    value_index,
                    predicate,
                    DictionaryMatchKind::Float,
                )?;
            } else if let Some(timestamp_nanoseconds) = compiled.timestamp_nanoseconds {
                compiled.dictionary_float_ids = self.matching_dictionary_ids(
                    expression,
                    value_index,
                    predicate,
                    DictionaryMatchKind::Timestamp(timestamp_nanoseconds),
                )?;
            }
        }
        Ok(compiled)
    }

    /// Returns the dictionary IDs a predicate accepts, reading the dictionary only if this
    /// query has not already resolved this value against this archive.
    fn matching_dictionary_ids(
        &mut self,
        expression: NodeId,
        value_index: usize,
        predicate: PredicateRef<'_>,
        kind: DictionaryMatchKind,
    ) -> Result<Vec<u64>, SearchError> {
        let node = expression.index();
        if let Some(matches) = self.matches.as_deref()
            && let Some(ids) = matches.get(node, value_index, kind)
        {
            return Ok(ids.to_vec());
        }
        let ids = self.match_dictionary(predicate, kind)?;
        if let Some(matches) = self.matches.as_deref_mut() {
            matches.insert(node, value_index, kind, &ids);
        }
        Ok(ids)
    }

    fn match_dictionary(
        &mut self,
        predicate: PredicateRef<'_>,
        kind: DictionaryMatchKind,
    ) -> Result<Vec<u64>, SearchError> {
        let dictionary = self.catalog.variable_dictionary();
        add_limited(
            &mut self.counters.dictionary_entries,
            dictionary.len(),
            self.options.limits().max_dictionary_entries_scanned(),
            SearchResource::DictionaryEntries,
        )?;
        let mut matching_ids = Vec::new();
        for entry in dictionary.entries() {
            let is_match = match kind {
                DictionaryMatchKind::String => {
                    string_value_matches(entry.value(), predicate, self.options.ignore_case())
                }
                DictionaryMatchKind::Float => std::str::from_utf8(entry.value())
                    .ok()
                    .and_then(|value| value.parse::<f64>().ok())
                    .is_some_and(|value| numeric_value_matches_f64(value, predicate)),
                DictionaryMatchKind::Timestamp(timestamp_nanoseconds) => {
                    std::str::from_utf8(entry.value())
                        .ok()
                        .and_then(|value| value.parse::<f64>().ok())
                        .is_some_and(|value| {
                            compare(
                                value,
                                timestamp_seconds(timestamp_nanoseconds),
                                predicate.operator(),
                            )
                        })
                }
            };
            if is_match {
                add_limited(
                    &mut self.counters.dictionary_matches,
                    1,
                    self.options.limits().max_dictionary_matches(),
                    SearchResource::DictionaryMatches,
                )?;
                matching_ids
                    .try_reserve(1)
                    .map_err(|_| allocation(SearchResource::DictionaryMatches, 1))?;
                matching_ids.push(entry.id());
            }
        }
        Ok(matching_ids)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DictionaryMatchKind {
    String,
    Float,
    Timestamp(i64),
}

/// Dictionary matches one query has already resolved against one archive.
///
/// Finding which dictionary entries a predicate accepts means reading every entry, which on a
/// string-heavy archive costs more than the rest of compiling the query. Neither the dictionary
/// nor the query changes while one archive is scanned, so a caller that compiles the same query
/// again for the same archive can carry this across and pay for each distinct scan once.
#[derive(Debug, Default)]
pub struct DictionaryMatches {
    entries: Vec<(usize, usize, DictionaryMatchKind, Vec<u64>)>,
}

impl DictionaryMatches {
    /// Returns an empty cache.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    fn get(&self, node: usize, value: usize, kind: DictionaryMatchKind) -> Option<&[u64]> {
        self.entries
            .iter()
            .find(|(n, v, k, _)| *n == node && *v == value && *k == kind)
            .map(|(_, _, _, ids)| ids.as_slice())
    }

    fn insert(&mut self, node: usize, value: usize, kind: DictionaryMatchKind, ids: &[u64]) {
        if self.entries.try_reserve(1).is_err() {
            return;
        }
        self.entries.push((node, value, kind, ids.to_vec()));
    }
}

fn compile_literal(
    expression: NodeId,
    predicate: PredicateRef<'_>,
) -> Result<Option<i64>, SearchError> {
    let timestamp = match predicate.value() {
        Literal::Timestamp(literal) => {
            Some(resolve_timestamp_literal(literal).map_err(|source| {
                SearchError::TimestampLiteral {
                    node: expression,
                    source,
                }
            })?)
        }
        _ => None,
    };
    if ComparisonOperator::Equal != predicate.operator()
        && numeric_literal(predicate.value()).is_none()
        && timestamp.is_none()
    {
        return Err(SearchError::Unsupported {
            node: expression,
            feature: UnsupportedSearchFeature::NonNumericRangeOperand,
        });
    }
    Ok(timestamp)
}

fn compute_negations(query: &ParsedQuery) -> Result<Vec<bool>, SearchError> {
    if query.root().index() >= query.nodes().len() {
        return Err(SearchError::InvalidRoot {
            root: query.root(),
            node_count: query.nodes().len(),
        });
    }
    let mut values = Vec::new();
    values
        .try_reserve_exact(query.nodes().len())
        .map_err(|_| allocation(SearchResource::CompiledProgram, query.nodes().len()))?;
    values.resize(query.nodes().len(), None);
    let mut pending = Vec::new();
    pending
        .try_reserve_exact(query.nodes().len())
        .map_err(|_| allocation(SearchResource::CompiledProgram, query.nodes().len()))?;
    pending.push((query.root(), false));
    while let Some((node, negated)) = pending.pop() {
        let slot = values
            .get_mut(node.index())
            .ok_or_else(|| SearchError::InvalidRoot {
                root: node,
                node_count: query.nodes().len(),
            })?;
        if let Some(previous) = *slot {
            if previous != negated {
                return Err(SearchError::MalformedExpression {
                    node,
                    operand: node,
                });
            }
            continue;
        }
        *slot = Some(negated);
        match query
            .node(node)
            .ok_or_else(|| SearchError::InvalidRoot {
                root: node,
                node_count: query.nodes().len(),
            })?
            .kind()
        {
            ExpressionKind::Not { operand } => pending.push((*operand, !negated)),
            ExpressionKind::Boolean { left, right, .. } => {
                pending.push((*left, negated));
                pending.push((*right, negated));
            }
            ExpressionKind::Predicate(_) | ExpressionKind::List(_) => {}
        }
    }
    let mut negations = Vec::new();
    negations
        .try_reserve_exact(values.len())
        .map_err(|_| allocation(SearchResource::CompiledProgram, values.len()))?;
    for value in values {
        negations.push(value.unwrap_or_default());
    }
    Ok(negations)
}

const fn check_prior(node: NodeId, operand: NodeId) -> Result<(), SearchError> {
    if operand.index() < node.index() {
        Ok(())
    } else {
        Err(SearchError::MalformedExpression { node, operand })
    }
}

#[derive(Debug)]
struct SchemaIndex {
    child_offsets: Vec<usize>,
    children: Vec<usize>,
    roots: Vec<usize>,
    inside_structured_array: Vec<bool>,
}

impl SchemaIndex {
    fn new(tree: &SchemaTree, limits: SearchLimits) -> Result<Self, SearchError> {
        check_limit(
            SearchResource::SchemaNodes,
            tree.len(),
            limits.max_schema_nodes(),
        )?;
        let mut counts = checked_zeroes(tree.len(), SearchResource::SchemaNodes)?;
        let mut roots = Vec::new();
        roots
            .try_reserve_exact(tree.len())
            .map_err(|_| allocation(SearchResource::SchemaNodes, tree.len()))?;
        for node in tree.nodes() {
            if let Some(parent) = node.parent_id() {
                let count = counts.get_mut(parent).ok_or(SearchError::SizeOverflow)?;
                *count = count.checked_add(1).ok_or(SearchError::SizeOverflow)?;
            }
        }
        for (node_id, node) in tree.nodes().iter().enumerate() {
            if node.parent_id().is_none() {
                roots.push(node_id);
            }
        }
        let mut inside_structured_array = Vec::new();
        inside_structured_array
            .try_reserve_exact(tree.len())
            .map_err(|_| allocation(SearchResource::SchemaNodes, tree.len()))?;
        inside_structured_array.resize(tree.len(), false);
        for (node_id, node) in tree.nodes().iter().enumerate() {
            let Some(parent_id) = node.parent_id() else {
                continue;
            };
            let parent = tree.get(parent_id).ok_or(SearchError::SizeOverflow)?;
            inside_structured_array[node_id] = NodeType::StructuredArray == parent.node_type()
                || *inside_structured_array
                    .get(parent_id)
                    .ok_or(SearchError::SizeOverflow)?;
        }
        let offset_len = tree.len().checked_add(1).ok_or(SearchError::SizeOverflow)?;
        let mut offsets: Vec<usize> = Vec::new();
        offsets
            .try_reserve_exact(offset_len)
            .map_err(|_| allocation(SearchResource::SchemaNodes, offset_len))?;
        offsets.push(0);
        for count in &counts {
            let next = offsets
                .last()
                .copied()
                .ok_or(SearchError::SizeOverflow)?
                .checked_add(*count)
                .ok_or(SearchError::SizeOverflow)?;
            offsets.push(next);
        }
        let mut children = checked_zeroes(
            offsets.last().copied().unwrap_or(0),
            SearchResource::SchemaNodes,
        )?;
        let mut cursors = Vec::new();
        cursors
            .try_reserve_exact(tree.len())
            .map_err(|_| allocation(SearchResource::SchemaNodes, tree.len()))?;
        cursors.extend_from_slice(&offsets[..tree.len()]);
        for (node_id, node) in tree.nodes().iter().enumerate() {
            if let Some(parent) = node.parent_id() {
                let cursor = cursors.get_mut(parent).ok_or(SearchError::SizeOverflow)?;
                let slot = children.get_mut(*cursor).ok_or(SearchError::SizeOverflow)?;
                *slot = node_id;
                *cursor = cursor.checked_add(1).ok_or(SearchError::SizeOverflow)?;
            }
        }
        Ok(Self {
            child_offsets: offsets,
            children,
            roots,
            inside_structured_array,
        })
    }

    fn resolve(
        &self,
        tree: &SchemaTree,
        path: &ColumnPath,
        work: &mut PathWork,
        counters: &mut CompileCounters,
        limits: SearchLimits,
    ) -> Result<ResolvedSchemaPath, SearchError> {
        work.clear();
        let path_state_limit = limits
            .max_path_states()
            .checked_sub(counters.path_states)
            .ok_or(SearchError::SizeOverflow)?;
        let resolved_limit = limits
            .max_resolved_nodes()
            .checked_sub(counters.resolved_nodes)
            .ok_or(SearchError::SizeOverflow)?;
        let bounds = PathBounds {
            states: path_state_limit,
            resolved: resolved_limit,
        };
        let namespace = namespace_bytes(path.namespace());
        let initial_component = usize::from(
            path.components()
                .first()
                .is_some_and(PathComponent::is_wildcard),
        );
        for root_id in &self.roots {
            let root = tree.get(*root_id).ok_or(SearchError::SizeOverflow)?;
            if root.node_type() == NodeType::Metadata || root.key_bytes() != namespace {
                continue;
            }
            for child in self.children_of(*root_id)? {
                work.push(*child, 0, path_state_limit)?;
                if 0 != initial_component {
                    work.push(*child, initial_component, path_state_limit)?;
                }
            }
        }
        let mut resolved = Vec::new();
        let mut arrays = Vec::new();
        while let Some((node_id, component)) = work.pending.pop() {
            if work.visited.contains(&(node_id, component)) {
                continue;
            }
            let path_states = counters
                .path_states
                .checked_add(1)
                .ok_or(SearchError::SizeOverflow)?;
            check_limit(
                SearchResource::PathStates,
                path_states,
                limits.max_path_states(),
            )?;
            work.visited
                .try_reserve(1)
                .map_err(|_| allocation(SearchResource::PathStates, 1))?;
            work.visited.insert((node_id, component));
            counters.path_states = path_states;
            let mut results = PathResults {
                nodes: &mut resolved,
                arrays: &mut arrays,
            };
            self.advance_path_state(
                tree,
                path.components(),
                (node_id, component),
                work,
                &mut results,
                bounds,
            )?;
        }
        resolved.sort_unstable();
        resolved.dedup();
        if !path.is_default_wildcard() && path.components().iter().any(PathComponent::is_wildcard) {
            resolved.retain(|&node_id| {
                let Ok(node_index) = usize::try_from(node_id) else {
                    return false;
                };
                tree.get(node_index).is_some_and(|node| {
                    node.key_bytes().is_empty()
                        || !self
                            .inside_structured_array
                            .get(node_index)
                            .copied()
                            .unwrap_or(true)
                })
            });
        }
        arrays.sort_unstable();
        arrays.dedup();
        let retained = resolved
            .len()
            .checked_add(arrays.len())
            .ok_or(SearchError::SizeOverflow)?;
        check_limit(SearchResource::ResolvedNodes, retained, bounds.resolved)?;
        add_limited(
            &mut counters.resolved_nodes,
            retained,
            limits.max_resolved_nodes(),
            SearchResource::ResolvedNodes,
        )?;
        Ok(ResolvedSchemaPath {
            nodes: resolved,
            arrays,
        })
    }

    fn advance_path_state(
        &self,
        tree: &SchemaTree,
        components: &[PathComponent],
        state: (usize, usize),
        work: &mut PathWork,
        results: &mut PathResults<'_>,
        bounds: PathBounds,
    ) -> Result<(), SearchError> {
        let (node_id, component) = state;
        let node = tree.get(node_id).ok_or(SearchError::SizeOverflow)?;
        let path_component = components.get(component);
        let next = component
            .checked_add(usize::from(path_component.is_some()))
            .ok_or(SearchError::SizeOverflow)?;
        if NodeType::UnstructuredArray == node.node_type() {
            let Some(path_component) = path_component else {
                return Ok(());
            };
            if path_component.is_wildcard() {
                if next == components.len() {
                    push_array_target(results.arrays, node_id, next, true, bounds.resolved)?;
                }
            } else if node.key_bytes() == path_component.value().as_bytes()
                && (next == components.len() || !components.iter().any(PathComponent::is_wildcard))
            {
                push_array_target(results.arrays, node_id, next, false, bounds.resolved)?;
            }
            return Ok(());
        }

        let key_is_empty = node.key_bytes().is_empty();
        let wildcard = path_component.is_some_and(PathComponent::is_wildcard);
        let accepted = key_is_empty
            || path_component.is_some_and(|component| {
                wildcard || node.key_bytes() == component.value().as_bytes()
            });
        if !accepted {
            return Ok(());
        }
        if next == components.len() {
            push_resolved(results.nodes, node_id, bounds.resolved)?;
        }
        if components.get(next).is_some_and(PathComponent::is_wildcard) {
            work.push(node_id, next, bounds.states)?;
        }
        for child in self.children_of(node_id)? {
            if key_is_empty {
                work.push(*child, component, bounds.states)?;
            } else {
                work.push(*child, next, bounds.states)?;
                if wildcard {
                    work.push(*child, component, bounds.states)?;
                }
            }
        }
        Ok(())
    }

    fn children_of(&self, node_id: usize) -> Result<&[usize], SearchError> {
        let start = *self
            .child_offsets
            .get(node_id)
            .ok_or(SearchError::SizeOverflow)?;
        let end = *self
            .child_offsets
            .get(node_id + 1)
            .ok_or(SearchError::SizeOverflow)?;
        self.children
            .get(start..end)
            .ok_or(SearchError::SizeOverflow)
    }
}

fn push_array_target(
    arrays: &mut Vec<ArrayTarget>,
    node_id: usize,
    component: usize,
    recursive: bool,
    limit: usize,
) -> Result<(), SearchError> {
    let retained = arrays
        .len()
        .checked_add(1)
        .ok_or(SearchError::SizeOverflow)?;
    check_limit(SearchResource::ResolvedNodes, retained, limit)?;
    arrays
        .try_reserve(1)
        .map_err(|_| allocation(SearchResource::ResolvedNodes, 1))?;
    arrays.push(ArrayTarget {
        node_id: u32::try_from(node_id).map_err(|_| SearchError::SizeOverflow)?,
        component,
        recursive,
    });
    Ok(())
}

#[derive(Clone, Copy)]
struct PathBounds {
    states: usize,
    resolved: usize,
}

struct PathResults<'a> {
    nodes: &'a mut Vec<u32>,
    arrays: &'a mut Vec<ArrayTarget>,
}

#[derive(Default)]
struct PathWork {
    pending: Vec<(usize, usize)>,
    visited: HashSet<(usize, usize)>,
}

impl PathWork {
    fn clear(&mut self) {
        self.pending.clear();
        self.visited.clear();
    }

    fn push(&mut self, node_id: usize, component: usize, limit: usize) -> Result<(), SearchError> {
        let retained = self
            .visited
            .len()
            .checked_add(self.pending.len())
            .and_then(|value| value.checked_add(1))
            .ok_or(SearchError::SizeOverflow)?;
        check_limit(SearchResource::PathStates, retained, limit)?;
        self.pending
            .try_reserve(1)
            .map_err(|_| allocation(SearchResource::PathStates, 1))?;
        self.pending.push((node_id, component));
        Ok(())
    }
}

const fn namespace_bytes(namespace: ColumnNamespace) -> &'static [u8] {
    match namespace {
        ColumnNamespace::Default => b"",
        ColumnNamespace::Autogenerated => b"@",
        ColumnNamespace::RangeIndex => b"$",
        ColumnNamespace::ReservedBang => b"!",
        ColumnNamespace::ReservedHash => b"#",
    }
}

fn push_resolved(resolved: &mut Vec<u32>, node_id: usize, limit: usize) -> Result<(), SearchError> {
    let retained = resolved
        .len()
        .checked_add(1)
        .ok_or(SearchError::SizeOverflow)?;
    check_limit(SearchResource::ResolvedNodes, retained, limit)?;
    let node_id = u32::try_from(node_id).map_err(|_| SearchError::SizeOverflow)?;
    resolved
        .try_reserve(1)
        .map_err(|_| allocation(SearchResource::ResolvedNodes, 1))?;
    resolved.push(node_id);
    Ok(())
}

fn validate_archive_tables(
    catalog: &ArchiveCatalog,
    limits: SearchLimits,
) -> Result<(), SearchError> {
    let tables = catalog.table_metadata().schema_tables();
    check_limit(
        SearchResource::ArchiveTables,
        tables.len(),
        limits.max_archive_tables(),
    )?;
    let mut next = 0_u64;
    for table in tables {
        next = next
            .checked_add(table.message_count())
            .ok_or(SearchError::SizeOverflow)?;
    }
    Ok(())
}

#[derive(Debug)]
struct TableIndex {
    present_nodes: Vec<u32>,
    columns_by_node: Vec<(u32, usize)>,
}

impl TableIndex {
    fn new(
        decoded: &DecodedSchemaTable<'_, '_>,
        catalog: &ArchiveCatalog,
        limits: SearchLimits,
    ) -> Result<Self, SearchError> {
        if !decoded.schema().unordered_entries().is_empty() {
            let plan_limits = ExtractionPlanLimits::new(
                usize_to_u64(limits.max_schema_nodes())?,
                usize_to_u64(limits.max_resolved_nodes())?,
                usize_to_u64(limits.max_path_states())?,
                usize_to_u64(limits.max_array_nesting_depth())?,
                usize_to_u64(limits.max_path_states())?,
            );
            ExtractionPlan::compile(decoded.schema(), catalog.schema_tree(), plan_limits).map_err(
                |source| SearchError::StructuredSchema {
                    table_index: decoded.table_index(),
                    schema_id: decoded.schema().id(),
                    source,
                },
            )?;
        }
        check_limit(
            SearchResource::TableIndex,
            decoded.schema().entries().len(),
            limits.max_resolved_nodes(),
        )?;
        let mut present_nodes = Vec::new();
        present_nodes
            .try_reserve_exact(decoded.schema().entries().len())
            .map_err(|_| {
                allocation(SearchResource::TableIndex, decoded.schema().entries().len())
            })?;
        for entry in decoded.schema().entries() {
            if let SchemaEntry::Node(node_id) = *entry {
                present_nodes.push(node_id);
            }
        }
        present_nodes.sort_unstable();
        present_nodes.dedup();
        check_limit(
            SearchResource::TableIndex,
            present_nodes.len(),
            limits.max_resolved_nodes(),
        )?;

        let columns = decoded.table().columns();
        check_limit(
            SearchResource::TableIndex,
            columns.len(),
            limits.max_resolved_nodes(),
        )?;
        let mut columns_by_node = Vec::new();
        columns_by_node
            .try_reserve_exact(columns.len())
            .map_err(|_| allocation(SearchResource::TableIndex, columns.len()))?;
        for (column_index, column) in columns.iter().enumerate() {
            if catalog
                .schema_tree()
                .get(column.node_id() as usize)
                .is_none()
            {
                return Err(SearchError::ForeignTable {
                    table_index: decoded.table_index(),
                });
            }
            columns_by_node.push((column.node_id(), column_index));
        }
        columns_by_node.sort_unstable();
        Ok(Self {
            present_nodes,
            columns_by_node,
        })
    }

    fn is_present(&self, node_id: u32) -> bool {
        self.present_nodes.binary_search(&node_id).is_ok()
    }

    fn column_range(&self, node_id: u32) -> std::ops::Range<usize> {
        let start = self
            .columns_by_node
            .partition_point(|(candidate, _)| *candidate < node_id);
        let end = self.columns_by_node[start..]
            .partition_point(|(candidate, _)| *candidate == node_id)
            + start;
        start..end
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
enum Tri {
    False = 0,
    True = 1,
    Unknown = 2,
}

struct Evaluator<'compiled, 'query, 'archive, 'table> {
    compiled: &'compiled CompiledQuery<'query, 'archive>,
    decoded: &'compiled DecodedSchemaTable<'table, 'archive>,
    table_index: TableIndex,
    states: Vec<Option<Vec<u8>>>,
    live_bitmap_bytes: usize,
    clp_scratch: Vec<u8>,
    array_scratch: ArrayScratch,
    range_work: Vec<RangeState<'archive>>,
    /// Rows a guarded range-index predicate can accept, as a half-open table-local range.
    ///
    /// A query whose root conjunction carries a `$`-namespace predicate cannot match a row that
    /// predicate rejects, and rows inside a table are in log order, so the rows it accepts are one
    /// contiguous run. Column scans write only inside that run and leave the rest of each leaf's
    /// bitmap at its default, which the final conjunction turns into a non-match either way.
    ///
    /// Per-row failures outside the run are no longer raised, since those rows are never read.
    /// Schema-level refusals still are: every leaf is still evaluated, over an empty row range.
    row_span: Option<(usize, usize)>,
}

impl<'compiled, 'query, 'archive, 'table> Evaluator<'compiled, 'query, 'archive, 'table> {
    fn new(
        compiled: &'compiled CompiledQuery<'query, 'archive>,
        decoded: &'compiled DecodedSchemaTable<'table, 'archive>,
        table_index: TableIndex,
    ) -> Self {
        Self {
            compiled,
            decoded,
            table_index,
            states: Vec::new(),
            live_bitmap_bytes: 0,
            clp_scratch: Vec::new(),
            array_scratch: ArrayScratch::default(),
            range_work: Vec::new(),
            row_span: None,
        }
    }

    fn guarded_range_index_node(&self) -> Option<usize> {
        self.compiled.guarded_range_index_node()
    }

    fn evaluate_node(&mut self, index: usize) -> Result<Vec<u8>, SearchError> {
        match self.compiled.nodes.get(index).copied() {
            Some(CompiledNode::Predicate(predicate)) => self.evaluate_predicate(predicate),
            Some(CompiledNode::List(list)) => self.evaluate_list(list),
            _ => Err(SearchError::SizeOverflow),
        }
    }

    fn evaluate(mut self) -> Result<MatchBitmap, SearchError> {
        self.states
            .try_reserve_exact(self.compiled.nodes.len())
            .map_err(|_| allocation(SearchResource::CompiledProgram, self.compiled.nodes.len()))?;
        self.states.resize_with(self.compiled.nodes.len(), || None);
        // Answer the range-index selector before anything is decoded. It alone decides which rows
        // of this table the query can reach, so a table it excludes needs no column read at all,
        // and a table it narrows needs the other predicates evaluated over that run only.
        if let Some(node_index) = self.guarded_range_index_node() {
            let bitmap = self.evaluate_node(node_index)?;
            // A table the selector excludes yields an empty span rather than an early answer, so
            // no column byte is read while the schema checks every other leaf performs still run
            // and still report what they refuse to evaluate.
            self.row_span = Some(true_span(&bitmap).unwrap_or((0, 0)));
            self.states[node_index] = Some(bitmap);
        }
        for (index, node) in self.compiled.nodes.iter().copied().enumerate() {
            if self.states[index].is_some() {
                // The range-index pre-pass already produced this node's rows.
                continue;
            }
            let expression = NodeId::new(index);
            let bitmap = match node {
                CompiledNode::Predicate(predicate) => self.evaluate_predicate(predicate)?,
                CompiledNode::List(list) => self.evaluate_list(list)?,
                CompiledNode::Not(operand) => {
                    let mut bitmap = self.take_state(expression, operand)?;
                    invert(&mut bitmap);
                    bitmap
                }
                CompiledNode::Boolean {
                    operator,
                    left,
                    right,
                } => {
                    let mut left = self.take_state(expression, left)?;
                    let right = self.take_state(expression, right)?;
                    combine(&mut left, &right, operator);
                    self.release_bitmap(right.len());
                    left
                }
            };
            self.states[index] = Some(bitmap);
        }
        let root = self.compiled.query.root();
        let mut bitmap = self.states[root.index()]
            .take()
            .ok_or(SearchError::InvalidRoot {
                root,
                node_count: self.states.len(),
            })?;
        if let Some(authoritative) = &self.compiled.authoritative_timestamp_range {
            if authoritative.archive_disjoint {
                bitmap.fill(Tri::False as u8);
            } else {
                let timestamp_bitmap = self.evaluate_authoritative_timestamp_range(
                    &authoritative.node_ids,
                    authoritative.begin_nanoseconds,
                    authoritative.end_nanoseconds,
                )?;
                combine(&mut bitmap, &timestamp_bitmap, BooleanOperator::And);
                self.release_bitmap(timestamp_bitmap.len());
            }
        }
        let mut match_count = 0_usize;
        for value in &mut bitmap {
            *value = u8::from(*value == Tri::True as u8);
            match_count += usize::from(*value);
        }
        Ok(MatchBitmap {
            bytes: bitmap,
            match_count,
        })
    }

    fn evaluate_authoritative_timestamp_range(
        &mut self,
        node_ids: &[u32],
        begin_nanoseconds: Option<i64>,
        end_nanoseconds: Option<i64>,
    ) -> Result<Vec<u8>, SearchError> {
        let row_count = self.decoded.table().message_count();
        let mut bitmap = self.new_bitmap(row_count, Tri::False)?;
        for &node_id in node_ids {
            if !self.table_index.is_present(node_id) {
                continue;
            }
            let column_range = self.table_index.column_range(node_id);
            for position in column_range {
                let column_index = self
                    .table_index
                    .columns_by_node
                    .get(position)
                    .ok_or(SearchError::SizeOverflow)?
                    .1;
                let column = self
                    .decoded
                    .table()
                    .column(column_index)
                    .ok_or(SearchError::SizeOverflow)?;
                scan_authoritative_column(
                    column.data(),
                    begin_nanoseconds,
                    end_nanoseconds,
                    &mut bitmap,
                    self.compiled.query.root(),
                )?;
            }
        }
        Ok(bitmap)
    }

    fn evaluate_predicate(&mut self, predicate_index: usize) -> Result<Vec<u8>, SearchError> {
        let compiled = self
            .compiled
            .predicates
            .get(predicate_index)
            .ok_or(SearchError::SizeOverflow)?;
        let expression = self
            .compiled
            .query
            .node(compiled.expression)
            .ok_or(SearchError::SizeOverflow)?;
        let ExpressionKind::Predicate(predicate) = expression.kind() else {
            return Err(SearchError::SizeOverflow);
        };
        self.evaluate_filter(
            compiled.expression,
            compiled.negated,
            &compiled.resolved,
            &compiled.value,
            PredicateRef::from_predicate(predicate),
        )
    }

    fn evaluate_list(&mut self, list_index: usize) -> Result<Vec<u8>, SearchError> {
        let compiled = self
            .compiled
            .lists
            .get(list_index)
            .ok_or(SearchError::SizeOverflow)?;
        let expression = self
            .compiled
            .query
            .node(compiled.expression)
            .ok_or(SearchError::SizeOverflow)?;
        let ExpressionKind::List(list) = expression.kind() else {
            return Err(SearchError::SizeOverflow);
        };
        if list.values().is_empty() {
            let fill = match list.operator() {
                ListOperator::Any => Tri::False,
                ListOperator::All | ListOperator::None => Tri::True,
            };
            return self.new_bitmap(self.decoded.table().message_count(), fill);
        }
        let resolved = compiled
            .resolved
            .as_ref()
            .ok_or(SearchError::SizeOverflow)?;
        if compiled.values.len() != list.values().len() {
            return Err(SearchError::SizeOverflow);
        }
        let (operator, invert_each) = match list.operator() {
            ListOperator::Any => (BooleanOperator::Or, false),
            ListOperator::All => (BooleanOperator::And, false),
            ListOperator::None => (BooleanOperator::And, true),
        };
        let value_negated = compiled.negated ^ invert_each;
        let mut aggregate: Option<Vec<u8>> = None;
        for (literal, value) in list.values().iter().zip(&compiled.values) {
            let predicate = PredicateRef::equality(list.path(), literal);
            let mut bitmap = self.evaluate_filter(
                compiled.expression,
                value_negated,
                resolved,
                value,
                predicate,
            )?;
            if invert_each {
                invert(&mut bitmap);
            }
            if let Some(accumulator) = &mut aggregate {
                combine(accumulator, &bitmap, operator);
                self.release_bitmap(bitmap.len());
            } else {
                aggregate = Some(bitmap);
            }
        }
        aggregate.ok_or(SearchError::SizeOverflow)
    }

    fn evaluate_filter(
        &mut self,
        expression: NodeId,
        negated: bool,
        resolved: &ResolvedPath,
        value: &CompiledValue,
        predicate: PredicateRef<'_>,
    ) -> Result<Vec<u8>, SearchError> {
        match resolved {
            ResolvedPath::Schema(schema) => {
                self.evaluate_schema_filter(expression, negated, value, predicate, schema)
            }
            ResolvedPath::RangeIndex => self.evaluate_range_filter(expression, negated, predicate),
        }
    }

    fn evaluate_schema_filter(
        &mut self,
        expression: NodeId,
        negated: bool,
        compiled: &CompiledValue,
        predicate: PredicateRef<'_>,
        schema: &ResolvedSchemaPath,
    ) -> Result<Vec<u8>, SearchError> {
        let row_count = self.decoded.table().message_count();
        if is_exists(predicate) {
            if negated
                && schema.arrays.iter().any(|target| {
                    target.component < predicate.path().components().len()
                        && self.table_index.is_present(target.node_id)
                })
            {
                // C++ schema matching treats unresolved NEXISTS inside an unstructured array as
                // unmatchable, even when the nested key is absent. The explicit NOT node later
                // inverts this all-true leaf to no rows.
                return self.new_bitmap(row_count, Tri::True);
            }
            let terminal_array_present = schema.arrays.iter().any(|target| {
                target.component == predicate.path().components().len()
                    && self.table_index.is_present(target.node_id)
            });
            let mut scalar_present = false;
            for node_id in &schema.nodes {
                let node_type = self
                    .compiled
                    .catalog
                    .schema_tree()
                    .get(*node_id as usize)
                    .ok_or(SearchError::SizeOverflow)?
                    .node_type();
                scalar_present |=
                    is_search_value_node(node_type) && self.table_index.is_present(*node_id);
            }
            if terminal_array_present || scalar_present {
                return self.new_bitmap(row_count, Tri::True);
            }
            let mut bitmap = self.new_bitmap(row_count, Tri::False)?;
            for target in &schema.arrays {
                if self.table_index.is_present(target.node_id) {
                    self.scan_array_existence(expression, predicate, *target, &mut bitmap)?;
                }
            }
            return Ok(bitmap);
        }
        let mut bitmap = self.new_bitmap(row_count, Tri::Unknown)?;
        let mut compatible = false;
        let filter = EvaluationFilter {
            expression,
            negated,
            value: compiled,
            predicate,
        };
        let non_null_occurrence_present = negated
            && matches!(predicate.value(), Literal::Null)
            && schema.nodes.iter().any(|node_id| {
                if !self.table_index.is_present(*node_id) {
                    return false;
                }
                self.compiled
                    .catalog
                    .schema_tree()
                    .get(*node_id as usize)
                    .is_some_and(|node| {
                        NodeType::Null != node.node_type() && is_search_value_node(node.node_type())
                    })
            });
        for node_id in &schema.nodes {
            if !self.table_index.is_present(*node_id) {
                continue;
            }
            let node_type = self
                .compiled
                .catalog
                .schema_tree()
                .get(*node_id as usize)
                .ok_or(SearchError::SizeOverflow)?
                .node_type();
            if non_null_occurrence_present && NodeType::Null == node_type {
                // A structured array may repeat one resolved path with both null and non-null
                // occurrences. C++ treats `NOT path:null` as an independent existential over the
                // non-null occurrences, rather than as the complement of any-null.
                continue;
            }
            compatible |= self.scan_node(filter, *node_id, node_type, &mut bitmap)?;
        }
        if schema
            .arrays
            .iter()
            .any(|target| self.table_index.is_present(target.node_id))
        {
            compatible = true;
            self.scan_arrays(filter, &schema.arrays, &mut bitmap)?;
        }
        if compatible {
            for value in &mut bitmap {
                if *value == Tri::Unknown as u8 {
                    *value = Tri::False as u8;
                }
            }
        }
        Ok(bitmap)
    }

    fn scan_node(
        &mut self,
        filter: EvaluationFilter<'_, '_>,
        node_id: u32,
        node_type: NodeType,
        bitmap: &mut [u8],
    ) -> Result<bool, SearchError> {
        if NodeType::Null == node_type {
            if matches!(filter.predicate.value(), Literal::Null) {
                bitmap.fill(Tri::True as u8);
                return Ok(true);
            }
            return Ok(false);
        }
        if matches!(filter.predicate.value(), Literal::Null) && filter.negated {
            return Ok(is_search_value_node(node_type));
        }
        if opposite_string_class(node_type, filter.predicate) {
            return Ok(true);
        }
        if NodeType::DeprecatedDateString == node_type
            && matches!(filter.predicate.value(), Literal::Timestamp(_))
        {
            return Err(SearchError::Unsupported {
                node: filter.expression,
                feature: UnsupportedSearchFeature::DeprecatedDateString,
            });
        }
        let compatible = node_compatible(node_type, filter.predicate);
        if !compatible {
            return Ok(false);
        }
        let column_range = self.table_index.column_range(node_id);
        for position in column_range {
            let column_index = self
                .table_index
                .columns_by_node
                .get(position)
                .ok_or(SearchError::SizeOverflow)?
                .1;
            let column = self
                .decoded
                .table()
                .column(column_index)
                .ok_or(SearchError::SizeOverflow)?;
            self.scan_column(filter, column.data(), bitmap)?;
        }
        Ok(true)
    }

    fn scan_column(
        &mut self,
        filter: EvaluationFilter<'_, '_>,
        column: ColumnData<'table, 'archive>,
        bitmap: &mut [u8],
    ) -> Result<(), SearchError> {
        // Rows outside the guarded range-index run cannot match, so they are neither read nor
        // written and keep this leaf's default.
        let rows = bitmap.len();
        let (skip, bitmap) = match self.row_span {
            Some((start, end)) if start < rows => (start, &mut bitmap[start..end.min(rows)]),
            Some(_) => (0, &mut bitmap[..0]),
            None => (0, bitmap),
        };
        match column {
            ColumnData::Integer(column) => {
                or_i64_matches(
                    bitmap,
                    column.iter().skip(skip),
                    filter.predicate,
                    filter.value.timestamp_nanoseconds,
                );
            }
            ColumnData::DeltaInteger(column) => {
                or_i64_matches(
                    bitmap,
                    column.values().skip(skip),
                    filter.predicate,
                    filter.value.timestamp_nanoseconds,
                );
            }
            ColumnData::Float(column) => {
                or_matches(bitmap, column.iter().skip(skip), |value| {
                    scalar_f64_matches(value, filter.predicate, filter.value.timestamp_nanoseconds)
                });
            }
            ColumnData::FormattedFloat(column) => {
                or_matches(bitmap, column.values().iter().skip(skip), |value| {
                    scalar_f64_matches(value, filter.predicate, filter.value.timestamp_nanoseconds)
                });
            }
            ColumnData::DictionaryFloat(column) => {
                or_matches(bitmap, column.ids().iter().skip(skip), |id| {
                    filter.value.dictionary_float_ids.binary_search(&id).is_ok()
                });
            }
            ColumnData::Boolean(column) => {
                let expected = match filter.predicate.value() {
                    Literal::Boolean(value) => *value,
                    _ => return Ok(()),
                };
                or_matches(bitmap, column.iter().skip(skip), |value| value == expected);
            }
            ColumnData::VarString(column) => {
                or_matches(bitmap, column.ids().iter().skip(skip), |id| {
                    filter.value.variable_ids.binary_search(&id).is_ok()
                });
            }
            ColumnData::ClpString(column) => {
                self.scan_clp(filter.expression, filter.predicate, column, skip, bitmap)?;
            }
            ColumnData::UnstructuredArray(_) => return Err(SearchError::SizeOverflow),
            ColumnData::DeprecatedDateString(_) => {
                return Err(SearchError::Unsupported {
                    node: filter.expression,
                    feature: UnsupportedSearchFeature::DeprecatedDateString,
                });
            }
            ColumnData::Timestamp(column) => {
                let Some(timestamp_nanoseconds) = filter.value.timestamp_nanoseconds else {
                    or_matches(bitmap, column.epochs().values().skip(skip), |value| {
                        numeric_value_matches_i64(value, filter.predicate)
                    });
                    return Ok(());
                };
                or_matches(bitmap, column.epochs().values().skip(skip), |value| {
                    compare(value, timestamp_nanoseconds, filter.predicate.operator())
                });
            }
        }
        Ok(())
    }

    fn scan_clp(
        &mut self,
        expression: NodeId,
        predicate: PredicateRef<'_>,
        column: crate::archive::ClpStringColumn<'table, 'archive>,
        row_offset: usize,
        bitmap: &mut [u8],
    ) -> Result<(), SearchError> {
        for (offset, destination) in bitmap.iter_mut().enumerate() {
            let row = row_offset + offset;
            let record = column.record(row).ok_or(SearchError::SizeOverflow)?;
            self.clp_scratch.clear();
            append_clp_message_bounded(
                record.logtype(),
                column.variable_dictionary(),
                &record.encoded_variables(),
                &mut self.clp_scratch,
                self.compiled
                    .options
                    .limits()
                    .max_reconstructed_string_bytes(),
            )
            .map_err(|source| SearchError::ClpString {
                node: expression,
                row,
                source,
            })?;
            if string_value_matches(
                &self.clp_scratch,
                predicate,
                self.compiled.options.ignore_case(),
            ) {
                *destination = Tri::True as u8;
            }
        }
        Ok(())
    }

    fn scan_arrays(
        &mut self,
        filter: EvaluationFilter<'_, '_>,
        targets: &[ArrayTarget],
        bitmap: &mut [u8],
    ) -> Result<(), SearchError> {
        let has_present_target = targets
            .iter()
            .any(|target| self.table_index.is_present(target.node_id));
        if !has_present_target {
            return Ok(());
        }
        if filter.negated && !matches!(filter.predicate.value(), Literal::Null) {
            // C++'s transformed inverted array-value filters constant-propagate to true for every
            // row in a schema containing the array. The explicit NOT node applies after this leaf,
            // so an all-false leaf preserves that observed result without changing JSON matching.
            bitmap.fill(Tri::False as u8);
            return Ok(());
        }
        if filter.negated && matches!(filter.predicate.value(), Literal::Null) {
            // `NOT array.path:null` is transformed to path existence by C++. This leaf is inverted
            // later, so construct the complement of existence here.
            bitmap.fill(Tri::True as u8);
            for target in targets {
                if self.table_index.is_present(target.node_id) {
                    self.scan_array_existence_value(
                        filter.expression,
                        filter.predicate,
                        *target,
                        Tri::False,
                        bitmap,
                    )?;
                }
            }
            return Ok(());
        }

        for target in targets {
            if !self.table_index.is_present(target.node_id) {
                continue;
            }
            let predicate = make_array_predicate(
                filter,
                self.compiled.options.ignore_case(),
                !target.recursive,
            );
            let column_range = self.table_index.column_range(target.node_id);
            for position in column_range {
                let column_index = self
                    .table_index
                    .columns_by_node
                    .get(position)
                    .ok_or(SearchError::SizeOverflow)?
                    .1;
                let column = self
                    .decoded
                    .table()
                    .column(column_index)
                    .ok_or(SearchError::SizeOverflow)?;
                let ColumnData::UnstructuredArray(column) = column.data() else {
                    return Err(SearchError::SizeOverflow);
                };
                for (row, destination) in bitmap.iter_mut().enumerate() {
                    let outcome = self.evaluate_array_row(
                        filter.expression,
                        row,
                        column,
                        filter.predicate,
                        *target,
                        predicate,
                    )?;
                    let null_transform = !target.recursive
                        && matches!(filter.predicate.value(), Literal::Null)
                        && outcome.path_exists();
                    if outcome.matched() || null_transform {
                        *destination = Tri::True as u8;
                    }
                }
            }
        }
        Ok(())
    }

    fn scan_array_existence(
        &mut self,
        expression: NodeId,
        predicate: PredicateRef<'_>,
        target: ArrayTarget,
        bitmap: &mut [u8],
    ) -> Result<(), SearchError> {
        self.scan_array_existence_value(expression, predicate, target, Tri::True, bitmap)
    }

    fn scan_array_existence_value(
        &mut self,
        expression: NodeId,
        predicate: PredicateRef<'_>,
        target: ArrayTarget,
        value: Tri,
        bitmap: &mut [u8],
    ) -> Result<(), SearchError> {
        let column_range = self.table_index.column_range(target.node_id);
        for position in column_range {
            let column_index = self
                .table_index
                .columns_by_node
                .get(position)
                .ok_or(SearchError::SizeOverflow)?
                .1;
            let column = self
                .decoded
                .table()
                .column(column_index)
                .ok_or(SearchError::SizeOverflow)?;
            let ColumnData::UnstructuredArray(column) = column.data() else {
                return Err(SearchError::SizeOverflow);
            };
            let no_match = ArrayPredicate::new(
                None,
                None,
                None,
                false,
                ArrayComparison::Equal,
                self.compiled.options.ignore_case(),
            );
            for (row, destination) in bitmap.iter_mut().enumerate() {
                let outcome =
                    self.evaluate_array_row(expression, row, column, predicate, target, no_match)?;
                if outcome.path_exists() {
                    *destination = value as u8;
                }
            }
        }
        Ok(())
    }

    fn evaluate_array_row(
        &mut self,
        expression: NodeId,
        row: usize,
        column: crate::archive::ClpStringColumn<'table, 'archive>,
        query: PredicateRef<'_>,
        target: ArrayTarget,
        predicate: ArrayPredicate<'_>,
    ) -> Result<super::array::ArrayOutcome, SearchError> {
        let record = column.record(row).ok_or(SearchError::SizeOverflow)?;
        self.clp_scratch.clear();
        append_clp_message_bounded(
            record.logtype(),
            column.variable_dictionary(),
            &record.encoded_variables(),
            &mut self.clp_scratch,
            self.compiled
                .options
                .limits()
                .max_reconstructed_string_bytes(),
        )
        .map_err(|source| SearchError::UnstructuredArrayReconstruction {
            node: expression,
            row,
            source,
        })?;
        let components = query
            .path()
            .components()
            .get(target.component..)
            .ok_or(SearchError::SizeOverflow)?;
        let limits = self.compiled.options.limits();
        evaluate_array(
            &self.clp_scratch,
            components,
            target.recursive,
            predicate,
            ArrayLimits {
                states: limits.max_array_json_states(),
                nesting_depth: limits.max_array_nesting_depth(),
                string_bytes: limits.max_array_decoded_string_bytes(),
            },
            &mut self.array_scratch,
        )
        .map_err(|failure| map_array_failure(expression, row, failure))
    }

    fn evaluate_range_filter(
        &mut self,
        expression: NodeId,
        negated: bool,
        predicate: PredicateRef<'_>,
    ) -> Result<Vec<u8>, SearchError> {
        let row_count = self.decoded.table().message_count();
        let default = if is_exists(predicate) {
            Tri::False
        } else {
            Tri::Unknown
        };
        let mut bitmap = self.new_bitmap(row_count, default)?;
        let Some(range_index) = self.compiled.catalog.metadata().range_index() else {
            return Ok(bitmap);
        };

        // A range-index entry addresses a span of LOG event indices, the order events were
        // written in. A table addresses rows in PHYSICAL order, which groups them by schema. The
        // two coincide only in an archive holding a single schema, so a row's position in its
        // table says nothing about which entry covers it. C++ resolves this by rewriting a
        // matching entry into bounds on the `log_event_idx` metadata column and testing each row's
        // own value against them; this does the same.
        let mut evaluated = Vec::new();
        evaluated
            .try_reserve_exact(range_index.entries().len())
            .map_err(|_| allocation(SearchResource::BitmapBytes, range_index.entries().len()))?;
        for entry in range_index.entries() {
            let result = evaluate_range_fields(
                entry.fields(),
                expression,
                negated,
                predicate,
                self.compiled.options,
                &mut self.range_work,
            )?;
            evaluated.push((entry.start_index(), entry.end_index(), result));
        }

        let located = locate_log_order_column(
            self.compiled.catalog.schema_tree(),
            self.decoded.schema(),
            self.decoded.table(),
        )
        .map_err(|_| SearchError::SizeOverflow)?;
        let Some(column) = located else {
            // Without the metadata column a row cannot be placed in log order, so no entry can be
            // shown to cover or exclude it. Leaving the default keeps the predicate unproven
            // rather than answering it from positions that do not mean what it needs.
            return Ok(bitmap);
        };

        // A truncated read stopped at the last row any accepted source file reaches, so walking
        // past it would only relabel rows already known not to match.
        let row_count = row_count.min(self.decoded.table().matchable_rows());
        // The column is delta encoded, so it is walked once rather than indexed per row. Entries
        // are validated sorted and disjoint and a table's rows are written in log order, so the
        // entry covering the previous row is at or before the one covering this row and a single
        // forward cursor replaces a scan of the entries per row. A stream whose indexes are not
        // monotonic restarts the cursor rather than reporting the wrong entry.
        let mut cursor = 0_usize;
        let mut previous = 0_u64;
        for (row, log_event_idx) in column.cursor().enumerate().take(row_count) {
            if log_event_idx < 0 {
                continue;
            }
            let idx = log_event_idx.cast_unsigned();
            if idx < previous {
                cursor = 0;
            }
            previous = idx;
            while cursor < evaluated.len() && idx >= evaluated[cursor].1 {
                cursor += 1;
            }
            let Some((start, _, result)) = evaluated.get(cursor) else {
                // Past every entry. A later row with a smaller index still resets the cursor.
                continue;
            };
            if idx >= *start {
                bitmap[row] = *result as u8;
            }
        }
        Ok(bitmap)
    }

    fn new_bitmap(&mut self, len: usize, fill: Tri) -> Result<Vec<u8>, SearchError> {
        let required = self
            .live_bitmap_bytes
            .checked_add(len)
            .ok_or(SearchError::SizeOverflow)?;
        check_limit(
            SearchResource::BitmapBytes,
            required,
            self.compiled.options.limits().max_live_bitmap_bytes(),
        )?;
        let mut bitmap = Vec::new();
        bitmap
            .try_reserve_exact(len)
            .map_err(|_| allocation(SearchResource::BitmapBytes, len))?;
        bitmap.resize(len, fill as u8);
        self.live_bitmap_bytes = required;
        Ok(bitmap)
    }

    fn take_state(&mut self, node: NodeId, operand: NodeId) -> Result<Vec<u8>, SearchError> {
        self.states
            .get_mut(operand.index())
            .and_then(Option::take)
            .ok_or(SearchError::MalformedExpression { node, operand })
    }

    const fn release_bitmap(&mut self, len: usize) {
        self.live_bitmap_bytes -= len;
    }
}

/// Returns the half-open run of rows a range-index bitmap marks true, or `None` for no row.
fn true_span(bitmap: &[u8]) -> Option<(usize, usize)> {
    let first = bitmap.iter().position(|value| Tri::True as u8 == *value)?;
    let last = bitmap.iter().rposition(|value| Tri::True as u8 == *value)?;
    Some((first, last + 1))
}

fn or_matches<T>(
    bitmap: &mut [u8],
    values: impl Iterator<Item = T>,
    mut matches: impl FnMut(T) -> bool,
) {
    for (destination, value) in bitmap.iter_mut().zip(values) {
        if matches(value) {
            *destination = Tri::True as u8;
        }
    }
}

fn or_i64_matches(
    bitmap: &mut [u8],
    values: impl Iterator<Item = i64>,
    predicate: PredicateRef<'_>,
    timestamp_nanoseconds: Option<i64>,
) {
    if timestamp_nanoseconds.is_none()
        && ComparisonOperator::Equal == predicate.operator()
        && let Literal::Integer { value: operand, .. } = predicate.value()
    {
        or_matches(bitmap, values, |value| value == *operand);
    } else {
        or_matches(bitmap, values, |value| {
            scalar_i64_matches(value, predicate, timestamp_nanoseconds)
        });
    }
}

fn make_array_predicate<'query>(
    filter: EvaluationFilter<'_, 'query>,
    ignore_case: bool,
    allow_numeric: bool,
) -> ArrayPredicate<'query> {
    let equality = ComparisonOperator::Equal == filter.predicate.operator();
    let string_pattern = if equality {
        match filter.predicate.value() {
            Literal::Integer { source, .. } | Literal::Float { source, .. } => {
                Some(source.as_str())
            }
            Literal::Boolean(true) => Some("true"),
            Literal::Boolean(false) => Some("false"),
            Literal::Null => Some("null"),
            Literal::String(string) => Some(string.wildcard_pattern()),
            Literal::Timestamp(_) => None,
        }
    } else {
        None
    };
    let number = allow_numeric.then(|| {
        filter.value.timestamp_nanoseconds.map_or_else(
            || match numeric_literal(filter.predicate.value()) {
                Some(NumericLiteral::Integer(value)) => Some(ArrayNumber::Integer(value)),
                Some(NumericLiteral::Float(value)) => Some(ArrayNumber::Float(value)),
                None => None,
            },
            |value| Some(ArrayNumber::Integer(value)),
        )
    });
    let boolean = if equality {
        match filter.predicate.value() {
            Literal::Boolean(value) => Some(*value),
            _ => None,
        }
    } else {
        None
    };
    ArrayPredicate::new(
        string_pattern,
        number.flatten(),
        boolean,
        equality && matches!(filter.predicate.value(), Literal::Null),
        if equality {
            ArrayComparison::Equal
        } else {
            ArrayComparison::NonEqual
        },
        ignore_case,
    )
}

const fn map_array_failure(node: NodeId, row: usize, failure: ArrayFailure) -> SearchError {
    match failure {
        ArrayFailure::Corrupt(source) => SearchError::UnstructuredArrayJson { node, row, source },
        ArrayFailure::Limit {
            resource,
            actual,
            limit,
        } => SearchError::LimitExceeded {
            resource: match resource {
                ArrayResource::States => SearchResource::ArrayStates,
                ArrayResource::NestingDepth => SearchResource::ArrayNestingDepth,
                ArrayResource::StringBytes => SearchResource::ArrayStringBytes,
            },
            actual,
            limit,
        },
        ArrayFailure::Allocation {
            resource,
            requested,
        } => SearchError::AllocationFailed {
            resource: match resource {
                ArrayResource::States => SearchResource::ArrayStates,
                ArrayResource::NestingDepth => SearchResource::ArrayNestingDepth,
                ArrayResource::StringBytes => SearchResource::ArrayStringBytes,
            },
            requested,
        },
        ArrayFailure::SizeOverflow => SearchError::SizeOverflow,
    }
}

fn scan_authoritative_column(
    column: ColumnData<'_, '_>,
    begin_nanoseconds: Option<i64>,
    end_nanoseconds: Option<i64>,
    bitmap: &mut [u8],
    expression: NodeId,
) -> Result<(), SearchError> {
    match column {
        ColumnData::Timestamp(column) => {
            or_matches(bitmap, column.epochs().values(), |value| {
                timestamp_in_range(value, begin_nanoseconds, end_nanoseconds)
            });
        }
        ColumnData::Integer(column) => {
            or_matches(bitmap, column.iter(), |value| {
                timestamp_in_range(value, begin_nanoseconds, end_nanoseconds)
            });
        }
        ColumnData::DeltaInteger(column) => {
            or_matches(bitmap, column.values(), |value| {
                timestamp_in_range(value, begin_nanoseconds, end_nanoseconds)
            });
        }
        ColumnData::Float(column) => {
            scan_authoritative_float(column.iter(), begin_nanoseconds, end_nanoseconds, bitmap);
        }
        ColumnData::DeprecatedDateString(_) => {
            return Err(SearchError::Unsupported {
                node: expression,
                feature: UnsupportedSearchFeature::DeprecatedDateString,
            });
        }
        ColumnData::FormattedFloat(_)
        | ColumnData::DictionaryFloat(_)
        | ColumnData::Boolean(_)
        | ColumnData::VarString(_)
        | ColumnData::ClpString(_)
        | ColumnData::UnstructuredArray(_) => return Err(SearchError::SizeOverflow),
    }
    Ok(())
}

fn scan_authoritative_float(
    values: impl Iterator<Item = f64>,
    begin_nanoseconds: Option<i64>,
    end_nanoseconds: Option<i64>,
    bitmap: &mut [u8],
) {
    let begin_seconds = begin_nanoseconds.map(timestamp_seconds);
    let end_seconds = end_nanoseconds.map(timestamp_seconds);
    or_matches(bitmap, values, |value| {
        begin_seconds.is_none_or(|begin| value >= begin)
            && end_seconds.is_none_or(|end| value <= end)
    });
}

fn timestamp_in_range(
    value: i64,
    begin_nanoseconds: Option<i64>,
    end_nanoseconds: Option<i64>,
) -> bool {
    begin_nanoseconds.is_none_or(|begin| value >= begin)
        && end_nanoseconds.is_none_or(|end| value <= end)
}

fn invert(bitmap: &mut [u8]) {
    for value in bitmap {
        *value = match *value {
            value if value == Tri::False as u8 => Tri::True as u8,
            value if value == Tri::True as u8 => Tri::False as u8,
            _ => Tri::Unknown as u8,
        };
    }
}

fn combine(left: &mut [u8], right: &[u8], operator: BooleanOperator) {
    for (left, right) in left.iter_mut().zip(right) {
        *left = match operator {
            BooleanOperator::And => tri_and(*left, *right),
            BooleanOperator::Or => tri_or(*left, *right),
        };
    }
}

const fn tri_and(left: u8, right: u8) -> u8 {
    if left == Tri::False as u8 || right == Tri::False as u8 {
        Tri::False as u8
    } else if left == Tri::True as u8 && right == Tri::True as u8 {
        Tri::True as u8
    } else {
        Tri::Unknown as u8
    }
}

const fn tri_or(left: u8, right: u8) -> u8 {
    if left == Tri::True as u8 || right == Tri::True as u8 {
        Tri::True as u8
    } else if left == Tri::False as u8 && right == Tri::False as u8 {
        Tri::False as u8
    } else {
        Tri::Unknown as u8
    }
}

fn is_exists(predicate: PredicateRef<'_>) -> bool {
    if ComparisonOperator::Equal != predicate.operator() {
        return false;
    }
    matches!(
        predicate.value(),
        Literal::String(string) if string.wildcard_pattern().as_bytes() == b"*"
    )
}

const fn is_search_value_node(node_type: NodeType) -> bool {
    !matches!(
        node_type,
        NodeType::Object | NodeType::StructuredArray | NodeType::Metadata
    )
}

fn node_compatible(node_type: NodeType, predicate: PredicateRef<'_>) -> bool {
    match node_type {
        NodeType::Integer
        | NodeType::DeltaInteger
        | NodeType::Float
        | NodeType::FormattedFloat
        | NodeType::DictionaryFloat
        | NodeType::Timestamp => {
            numeric_literal(predicate.value()).is_some()
                || matches!(predicate.value(), Literal::Timestamp(_))
        }
        NodeType::Boolean => matches!(predicate.value(), Literal::Boolean(_)),
        NodeType::VarString => variable_pattern(predicate).is_some(),
        NodeType::ClpString => clp_pattern(predicate).is_some(),
        NodeType::Null => matches!(predicate.value(), Literal::Null),
        NodeType::UnstructuredArray => matches!(
            predicate.value(),
            Literal::Integer { .. }
                | Literal::Float { .. }
                | Literal::Boolean(_)
                | Literal::Null
                | Literal::String(_)
                | Literal::Timestamp(_)
        ),
        NodeType::DeprecatedDateString => matches!(predicate.value(), Literal::Timestamp(_)),
        NodeType::Object | NodeType::StructuredArray | NodeType::Metadata => false,
    }
}

fn opposite_string_class(node_type: NodeType, predicate: PredicateRef<'_>) -> bool {
    match node_type {
        NodeType::VarString => {
            variable_pattern(predicate).is_none() && clp_pattern(predicate).is_some()
        }
        NodeType::ClpString => {
            clp_pattern(predicate).is_none() && variable_pattern(predicate).is_some()
        }
        _ => false,
    }
}

#[derive(Clone, Copy)]
enum NumericLiteral {
    Integer(i64),
    Float(f64),
}

const fn numeric_literal(literal: &Literal) -> Option<NumericLiteral> {
    match literal {
        Literal::Integer { value, .. } => Some(NumericLiteral::Integer(*value)),
        Literal::Float { value, .. } => Some(NumericLiteral::Float(*value)),
        _ => None,
    }
}

fn numeric_value_matches_i64(value: i64, predicate: PredicateRef<'_>) -> bool {
    match numeric_literal(predicate.value()) {
        Some(NumericLiteral::Integer(operand)) => compare(value, operand, predicate.operator()),
        Some(NumericLiteral::Float(operand)) => {
            compare_i64_float(value, operand, predicate.operator())
        }
        None => false,
    }
}

fn scalar_i64_matches(
    value: i64,
    predicate: PredicateRef<'_>,
    timestamp_nanoseconds: Option<i64>,
) -> bool {
    timestamp_nanoseconds.map_or_else(
        || numeric_value_matches_i64(value, predicate),
        |operand| compare(value, operand, predicate.operator()),
    )
}

#[allow(clippy::cast_precision_loss)]
fn numeric_value_matches_f64(value: f64, predicate: PredicateRef<'_>) -> bool {
    let operand = match numeric_literal(predicate.value()) {
        Some(NumericLiteral::Integer(value)) => value as f64,
        Some(NumericLiteral::Float(value)) => value,
        None => return false,
    };
    compare(value, operand, predicate.operator())
}

fn scalar_f64_matches(
    value: f64,
    predicate: PredicateRef<'_>,
    timestamp_nanoseconds: Option<i64>,
) -> bool {
    timestamp_nanoseconds.map_or_else(
        || numeric_value_matches_f64(value, predicate),
        |operand| compare(value, timestamp_seconds(operand), predicate.operator()),
    )
}

#[allow(clippy::cast_precision_loss)]
fn timestamp_seconds(timestamp_nanoseconds: i64) -> f64 {
    timestamp_nanoseconds as f64 / 1_000_000_000.0
}

#[allow(clippy::needless_pass_by_value)]
fn compare<T: PartialEq + PartialOrd>(value: T, operand: T, operator: ComparisonOperator) -> bool {
    match operator {
        ComparisonOperator::Equal => value == operand,
        ComparisonOperator::Less => value < operand,
        ComparisonOperator::LessOrEqual => value <= operand,
        ComparisonOperator::Greater => value > operand,
        ComparisonOperator::GreaterOrEqual => value >= operand,
    }
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::float_cmp
)]
fn compare_i64_float(value: i64, operand: f64, operator: ComparisonOperator) -> bool {
    const LOWER: f64 = -9_223_372_036_854_775_808.0;
    const UPPER_EXCLUSIVE: f64 = 9_223_372_036_854_775_808.0;
    match operator {
        ComparisonOperator::Equal => {
            (LOWER..UPPER_EXCLUSIVE).contains(&operand)
                && operand.fract() == 0.0
                && value == operand as i64
                && operand == (value as f64)
        }
        ComparisonOperator::Less => {
            operand > LOWER && (operand >= UPPER_EXCLUSIVE || value < operand.ceil() as i64)
        }
        ComparisonOperator::LessOrEqual => {
            operand >= LOWER && (operand >= UPPER_EXCLUSIVE || value <= operand.floor() as i64)
        }
        ComparisonOperator::Greater => {
            operand < UPPER_EXCLUSIVE && (operand < LOWER || value > operand.floor() as i64)
        }
        ComparisonOperator::GreaterOrEqual => {
            operand < UPPER_EXCLUSIVE && (operand <= LOWER || value >= operand.ceil() as i64)
        }
    }
}

fn variable_pattern(predicate: PredicateRef<'_>) -> Option<&str> {
    if ComparisonOperator::Equal != predicate.operator() || is_exists(predicate) {
        return None;
    }
    match predicate.value() {
        Literal::Integer { source, .. } | Literal::Float { source, .. } => Some(source),
        Literal::Boolean(true) => Some("true"),
        Literal::Boolean(false) => Some("false"),
        Literal::Null => Some("null"),
        Literal::String(string) if !string_pattern_is_clp_only(string.wildcard_pattern()) => {
            Some(string.wildcard_pattern())
        }
        _ => None,
    }
}

fn clp_pattern(predicate: PredicateRef<'_>) -> Option<&str> {
    if ComparisonOperator::Equal != predicate.operator() || is_exists(predicate) {
        return None;
    }
    match predicate.value() {
        Literal::String(string)
            if string.wildcard_pattern().contains(' ') || string.has_wildcards() =>
        {
            Some(string.wildcard_pattern())
        }
        _ => None,
    }
}

fn string_pattern_is_clp_only(pattern: &str) -> bool {
    pattern.contains(' ')
}

fn string_value_matches(value: &[u8], predicate: PredicateRef<'_>, ignore_case: bool) -> bool {
    let Some(pattern) = variable_pattern(predicate).or_else(|| clp_pattern(predicate)) else {
        return false;
    };
    wildcard_match(value, pattern, ignore_case)
}

#[derive(Clone, Copy)]
enum RangeCursor<'a> {
    Fields(&'a BTreeMap<String, RangeIndexValue>),
    Value(&'a RangeIndexValue),
}

#[derive(Clone, Copy)]
struct RangeState<'a> {
    component: usize,
    cursor: RangeCursor<'a>,
}

fn evaluate_range_fields<'a>(
    fields: &'a BTreeMap<String, RangeIndexValue>,
    expression: NodeId,
    negated: bool,
    predicate: PredicateRef<'_>,
    options: SearchOptions,
    work: &mut Vec<RangeState<'a>>,
) -> Result<Tri, SearchError> {
    work.clear();
    let state_limit = options.limits().max_range_traversal_states();
    check_limit(SearchResource::RangeStates, 1, state_limit)?;
    work.try_reserve(1)
        .map_err(|_| allocation(SearchResource::RangeStates, 1))?;
    work.push(RangeState {
        component: 0,
        cursor: RangeCursor::Fields(fields),
    });
    let mut compatible = false;
    let mut visited = 0_usize;
    while let Some(state) = work.pop() {
        visited = visited.checked_add(1).ok_or(SearchError::SizeOverflow)?;
        check_limit(
            SearchResource::RangeStates,
            visited,
            options.limits().max_range_traversal_states(),
        )?;
        if state.component == predicate.path().components().len() {
            if let RangeCursor::Value(value) = state.cursor {
                if is_exists(predicate) {
                    return Ok(Tri::True);
                }
                if matches!(predicate.value(), Literal::Null) && negated {
                    compatible = true;
                    if matches!(value, RangeIndexValue::Null) {
                        return Ok(Tri::True);
                    }
                    continue;
                }
                compatible |= range_value_compatible(value, expression, predicate)?;
                if range_value_matches(value, predicate, options.ignore_case()) {
                    return Ok(Tri::True);
                }
            }
            continue;
        }
        let component = &predicate.path().components()[state.component];
        push_range_children(state, component, visited, state_limit, work)?;
    }
    Ok(if is_exists(predicate) || compatible {
        Tri::False
    } else {
        Tri::Unknown
    })
}

fn push_range_children<'a>(
    state: RangeState<'a>,
    component: &PathComponent,
    visited: usize,
    limit: usize,
    work: &mut Vec<RangeState<'a>>,
) -> Result<(), SearchError> {
    if component.is_wildcard() {
        let fields = match state.cursor {
            RangeCursor::Fields(fields) | RangeCursor::Value(RangeIndexValue::Object(fields)) => {
                Some(fields)
            }
            RangeCursor::Value(_) => None,
        };
        let additional = fields.map_or(Ok(1), |fields| {
            fields.len().checked_add(1).ok_or(SearchError::SizeOverflow)
        })?;
        check_range_work_limit(visited, work.len(), additional, limit)?;
        reserve_vec(work, additional, SearchResource::RangeStates)?;
        work.push(RangeState {
            component: state.component + 1,
            cursor: state.cursor,
        });
        if let Some(fields) = fields {
            for value in fields.values() {
                work.push(RangeState {
                    component: state.component,
                    cursor: RangeCursor::Value(value),
                });
            }
        }
    } else if let Some(value) = match state.cursor {
        RangeCursor::Fields(fields) | RangeCursor::Value(RangeIndexValue::Object(fields)) => {
            fields.get(component.value())
        }
        RangeCursor::Value(_) => None,
    } {
        check_range_work_limit(visited, work.len(), 1, limit)?;
        reserve_vec(work, 1, SearchResource::RangeStates)?;
        work.push(RangeState {
            component: state.component + 1,
            cursor: RangeCursor::Value(value),
        });
    }
    Ok(())
}

fn check_range_work_limit(
    visited: usize,
    pending: usize,
    additional: usize,
    limit: usize,
) -> Result<(), SearchError> {
    let retained = visited
        .checked_add(pending)
        .and_then(|value| value.checked_add(additional))
        .ok_or(SearchError::SizeOverflow)?;
    check_limit(SearchResource::RangeStates, retained, limit)
}

fn range_value_compatible(
    value: &RangeIndexValue,
    expression: NodeId,
    predicate: PredicateRef<'_>,
) -> Result<bool, SearchError> {
    if is_exists(predicate) {
        return Ok(true);
    }
    match value {
        RangeIndexValue::Null => Ok(matches!(predicate.value(), Literal::Null)),
        RangeIndexValue::Boolean(_) => Ok(matches!(predicate.value(), Literal::Boolean(_))),
        RangeIndexValue::Signed(_) | RangeIndexValue::Float(_) => {
            Ok(numeric_literal(predicate.value()).is_some())
        }
        RangeIndexValue::Unsigned(value) => {
            if i64::try_from(*value).is_err() && numeric_literal(predicate.value()).is_some() {
                Err(SearchError::Unsupported {
                    node: expression,
                    feature: UnsupportedSearchFeature::RangeIndexValue,
                })
            } else {
                Ok(numeric_literal(predicate.value()).is_some())
            }
        }
        RangeIndexValue::String(_) => Ok(variable_pattern(predicate).is_some()),
        RangeIndexValue::Binary(_) | RangeIndexValue::Array(_) | RangeIndexValue::Object(_) => {
            Ok(false)
        }
    }
}

fn range_value_matches(
    value: &RangeIndexValue,
    predicate: PredicateRef<'_>,
    ignore_case: bool,
) -> bool {
    match value {
        RangeIndexValue::Null => matches!(predicate.value(), Literal::Null),
        RangeIndexValue::Boolean(value) => {
            matches!(predicate.value(), Literal::Boolean(expected) if value == expected)
        }
        RangeIndexValue::Signed(value) => numeric_value_matches_i64(*value, predicate),
        RangeIndexValue::Unsigned(value) => {
            i64::try_from(*value).is_ok_and(|value| numeric_value_matches_i64(value, predicate))
        }
        RangeIndexValue::Float(value) => numeric_value_matches_f64(*value, predicate),
        RangeIndexValue::String(value) => {
            string_value_matches(value.as_bytes(), predicate, ignore_case)
        }
        RangeIndexValue::Binary(_) | RangeIndexValue::Array(_) | RangeIndexValue::Object(_) => {
            false
        }
    }
}

fn checked_zeroes(len: usize, resource: SearchResource) -> Result<Vec<usize>, SearchError> {
    let mut values = Vec::new();
    values
        .try_reserve_exact(len)
        .map_err(|_| allocation(resource, len))?;
    values.resize(len, 0);
    Ok(values)
}

fn reserve_vec<T>(
    values: &mut Vec<T>,
    additional: usize,
    resource: SearchResource,
) -> Result<(), SearchError> {
    values
        .try_reserve(additional)
        .map_err(|_| allocation(resource, additional))
}

const fn allocation(resource: SearchResource, requested: usize) -> SearchError {
    SearchError::AllocationFailed {
        resource,
        requested,
    }
}

const fn check_limit(
    resource: SearchResource,
    actual: usize,
    limit: usize,
) -> Result<(), SearchError> {
    if actual > limit {
        Err(SearchError::LimitExceeded {
            resource,
            actual,
            limit,
        })
    } else {
        Ok(())
    }
}

fn usize_to_u64(value: usize) -> Result<u64, SearchError> {
    u64::try_from(value).map_err(|_| SearchError::SizeOverflow)
}

fn add_limited(
    total: &mut usize,
    additional: usize,
    limit: usize,
    resource: SearchResource,
) -> Result<(), SearchError> {
    let actual = total
        .checked_add(additional)
        .ok_or(SearchError::SizeOverflow)?;
    check_limit(resource, actual, limit)?;
    *total = actual;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::archive::ArchiveCatalogLimits;
    use crate::archive::ColumnLimits;
    use crate::archive::PackedStreamLimits;
    use crate::archive::SingleFileArchiveReader;
    use crate::search::KqlLimits;
    use crate::search::parse_kql;
    use crate::writer::FieldRef;
    use crate::writer::OpenArchive;
    use crate::writer::RecordRef;
    use crate::writer::UnstructuredArrayRef;
    use crate::writer::ValueRef;
    use crate::writer::WriterOptions;

    const STRUCTURED_ARRAY_FIXTURE_HEX: &str =
        include_str!("../../tests/fixtures/sfa-v0.5.0-structured-arrays-cpp.hex");

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

    fn fixture() -> Vec<u8> {
        let mut archive = OpenArchive::new(Cursor::new(Vec::new()), WriterOptions::default());

        let literal_star_leaf = [FieldRef::new(b"leaf", ValueRef::String(b"literal"))];
        let path_zero = [FieldRef::new(b"*", ValueRef::Object(&literal_star_leaf))];
        let row_zero = [
            FieldRef::new(b"id", ValueRef::I64(0)),
            FieldRef::new(b"feature", ValueRef::Bool(true)),
            FieldRef::new(b"optional", ValueRef::String(b"yes")),
            FieldRef::new(b"n", ValueRef::I64(7)),
            FieldRef::new(b"s", ValueRef::String(b"MiXeD")),
            FieldRef::new(b"token", ValueRef::String(b"a*e")),
            FieldRef::new(b"flag", ValueRef::Bool(true)),
            FieldRef::new(b"nullish", ValueRef::Null),
            FieldRef::new(b"msg", ValueRef::String(b"Msg 1: Abc123")),
            FieldRef::new(b"path", ValueRef::Object(&path_zero)),
        ];
        archive
            .append_record(RecordRef::new(&row_zero))
            .expect("append row zero");

        let nested_leaf = [FieldRef::new(b"leaf", ValueRef::String(b"nested"))];
        let path_one = [FieldRef::new(b"one", ValueRef::Object(&nested_leaf))];
        let row_one = [
            FieldRef::new(b"id", ValueRef::I64(1)),
            FieldRef::new(b"feature", ValueRef::Bool(true)),
            FieldRef::new(b"optional", ValueRef::String(b"no")),
            FieldRef::new(b"n", ValueRef::I64(8)),
            FieldRef::new(b"s", ValueRef::String("ÉCOLE".as_bytes())),
            FieldRef::new(b"token", ValueRef::String(b"abcde")),
            FieldRef::new(b"flag", ValueRef::Bool(false)),
            FieldRef::new(b"nullish", ValueRef::Null),
            FieldRef::new(b"msg", ValueRef::String(b"another message")),
            FieldRef::new(b"path", ValueRef::Object(&path_one)),
        ];
        archive
            .append_record(RecordRef::new(&row_one))
            .expect("append row one");

        let direct_path = [FieldRef::new(b"leaf", ValueRef::String(b"direct"))];
        let row_two = [
            FieldRef::new(b"id", ValueRef::I64(2)),
            FieldRef::new(b"feature", ValueRef::Bool(true)),
            FieldRef::new(b"n", ValueRef::I64(7)),
            FieldRef::new(b"s", ValueRef::String(b"mixed")),
            FieldRef::new(b"msg", ValueRef::String(b"third message")),
            FieldRef::new(b"path", ValueRef::Object(&direct_path)),
        ];
        archive
            .append_record(RecordRef::new(&row_two))
            .expect("append row two");

        let row_three = [
            FieldRef::new(b"id", ValueRef::I64(3)),
            FieldRef::new(b"feature", ValueRef::Bool(true)),
            FieldRef::new(b"optional", ValueRef::String(b"later")),
            FieldRef::new(b"n", ValueRef::String(b"7")),
            FieldRef::new(b"s", ValueRef::String(b"other")),
            FieldRef::new(b"token", ValueRef::String(b"plain")),
            FieldRef::new(b"flag", ValueRef::String(b"true")),
            FieldRef::new(b"nullish", ValueRef::String(b"null")),
            FieldRef::new(b"msg", ValueRef::String(b"fourth message")),
        ];
        archive
            .append_record(RecordRef::new(&row_three))
            .expect("append row three");

        let row_four = [
            FieldRef::new(b"id", ValueRef::I64(4)),
            FieldRef::new(b"feature", ValueRef::Bool(false)),
            FieldRef::new(b"optional", ValueRef::String(b"yes")),
            FieldRef::new(b"n", ValueRef::I64(-1)),
            FieldRef::new(b"s", ValueRef::String(b"MIXED")),
            FieldRef::new(b"msg", ValueRef::String(b"fifth message")),
        ];
        archive
            .append_record(RecordRef::new(&row_four))
            .expect("append row four");

        archive
            .finish()
            .expect("finish semantic fixture")
            .into_inner()
            .into_inner()
    }

    fn characterization_array_fixture() -> Vec<u8> {
        let rows: &[(i64, &[u8])] = &[
            (0, br#"[{"n":3}]"#),
            (1, br#"[{"n":5}]"#),
            (2, br#"[{"n":7}]"#),
            (3, br#"[{"n":5}]"#),
        ];
        let mut archive = OpenArchive::new(
            Cursor::new(Vec::new()),
            WriterOptions::default().with_log_order(false),
        );
        for &(id, raw_json) in rows {
            let fields = [
                FieldRef::new(b"id", ValueRef::I64(id)),
                FieldRef::new(b"feature", ValueRef::Bool(true)),
                FieldRef::new(
                    b"arr",
                    ValueRef::UnstructuredArray(UnstructuredArrayRef::new(raw_json)),
                ),
            ];
            archive
                .append_record(RecordRef::new(&fields))
                .expect("append characterized array row");
        }
        archive
            .finish()
            .expect("finish characterized array fixture")
            .into_inner()
            .into_inner()
    }

    fn integer_equality_fixture(log_order: bool) -> Vec<u8> {
        let rows = [(i64::MIN, 7_i64), (-17, 0), (0, 7), (23, 0), (i64::MAX, 0)];
        let mut archive = OpenArchive::new(
            Cursor::new(Vec::new()),
            WriterOptions::default().with_log_order(log_order),
        );
        for (left, right) in rows {
            let fields = [
                FieldRef::new(b"left", ValueRef::I64(left)),
                FieldRef::new(b"right", ValueRef::I64(right)),
            ];
            archive
                .append_record(RecordRef::new(&fields))
                .expect("append integer equality row");
        }
        archive
            .finish()
            .expect("finish integer equality fixture")
            .into_inner()
            .into_inner()
    }

    fn single_table_bitmap(bytes: &[u8], query: &str) -> MatchBitmap {
        let parsed = parse_kql(query, KqlLimits::default()).expect("parse bitmap query");
        let mut reader =
            SingleFileArchiveReader::open(Cursor::new(bytes)).expect("open bitmap fixture");
        let catalog = reader
            .read_catalog(ArchiveCatalogLimits::default())
            .expect("read bitmap catalog");
        let compiled = parsed
            .compile_for_archive(&catalog, SearchOptions::default())
            .expect("compile bitmap query");
        let stream = reader
            .read_packed_stream(
                catalog.metadata(),
                catalog.table_metadata(),
                0,
                PackedStreamLimits::default(),
            )
            .expect("read bitmap stream");
        let mut tables = catalog
            .schema_tables(0, &stream, ColumnLimits::default())
            .expect("open bitmap tables");
        let decoded = tables
            .next()
            .expect("one bitmap table")
            .expect("decode bitmap table");
        let bitmap = compiled.match_table(&decoded).expect("match bitmap table");
        assert!(tables.next().is_none(), "fixture has exactly one table");
        bitmap
    }

    fn delta_log_index_bitmap(bytes: &[u8], expected: i64) -> MatchBitmap {
        let source = format!("left:{expected}");
        let parsed = parse_kql(&source, KqlLimits::default()).expect("parse delta query");
        let mut reader =
            SingleFileArchiveReader::open(Cursor::new(bytes)).expect("open delta fixture");
        let catalog = reader
            .read_catalog(ArchiveCatalogLimits::default())
            .expect("read delta catalog");
        let mut compiled = parsed
            .compile_for_archive(&catalog, SearchOptions::default())
            .expect("compile delta query");
        let delta_node = catalog
            .schema_tree()
            .nodes()
            .iter()
            .position(|node| NodeType::DeltaInteger == node.node_type())
            .and_then(|node| u32::try_from(node).ok())
            .expect("log-order delta node");
        let CompiledNode::Predicate(predicate_index) = compiled.nodes[parsed.root().index()] else {
            panic!("delta query root is a predicate");
        };
        compiled.predicates[predicate_index].resolved = ResolvedPath::Schema(ResolvedSchemaPath {
            nodes: vec![delta_node],
            arrays: Vec::new(),
        });

        let stream = reader
            .read_packed_stream(
                catalog.metadata(),
                catalog.table_metadata(),
                0,
                PackedStreamLimits::default(),
            )
            .expect("read delta stream");
        let mut tables = catalog
            .schema_tables(0, &stream, ColumnLimits::default())
            .expect("open delta tables");
        let decoded = tables
            .next()
            .expect("one delta table")
            .expect("decode delta table");
        let bitmap = compiled.match_table(&decoded).expect("match delta table");
        assert!(tables.next().is_none(), "fixture has exactly one table");
        bitmap
    }

    fn matched_ids(bytes: &[u8], query: &str, ignore_case: bool) -> Vec<i64> {
        matched_integer_field(
            bytes,
            query,
            b"id",
            SearchOptions::new(ignore_case, SearchLimits::default()),
        )
        .expect("match semantic query")
    }

    fn matched_integer_field(
        bytes: &[u8],
        query: &str,
        field: &[u8],
        options: SearchOptions,
    ) -> Result<Vec<i64>, SearchError> {
        let parsed = parse_kql(query, KqlLimits::default()).expect("parse query");
        let mut reader =
            SingleFileArchiveReader::open(Cursor::new(bytes)).expect("open semantic fixture");
        let catalog = reader
            .read_catalog(ArchiveCatalogLimits::default())
            .expect("read semantic catalog");
        let compiled = parsed.compile_for_archive(&catalog, options)?;
        let mut matches = Vec::new();
        for stream_id in 0..catalog.table_metadata().packed_streams().len() {
            let stream = reader
                .read_packed_stream(
                    catalog.metadata(),
                    catalog.table_metadata(),
                    stream_id,
                    PackedStreamLimits::default(),
                )
                .expect("read semantic stream");
            let tables = catalog
                .schema_tables(
                    u64::try_from(stream_id).expect("stream ID fits u64"),
                    &stream,
                    ColumnLimits::default(),
                )
                .expect("open semantic tables");
            for decoded in tables {
                let decoded = decoded.expect("decode semantic table");
                let bitmap = compiled.match_table(&decoded)?;
                let id_column = decoded
                    .table()
                    .columns()
                    .iter()
                    .find(|column| {
                        catalog
                            .schema_tree()
                            .get(column.node_id() as usize)
                            .is_some_and(|node| {
                                node.key_bytes() == field && NodeType::Integer == node.node_type()
                            })
                    })
                    .expect("integer id column");
                let ColumnData::Integer(ids) = id_column.data() else {
                    panic!("fixture ID must use an integer column");
                };
                matches.extend(
                    bitmap
                        .matching_rows()
                        .map(|row| ids.get(row).expect("ID row")),
                );
            }
        }
        matches.sort_unstable();
        Ok(matches)
    }

    fn minimal_timestamp_matches(
        query: &str,
        options: SearchOptions,
    ) -> Result<Vec<usize>, SearchError> {
        let parsed = parse_kql(query, KqlLimits::default()).expect("parse timestamp query");
        let bytes = include_bytes!("../../tests/fixtures/sfa-v0.5.0-minimal-cpp.bin");
        let mut reader =
            SingleFileArchiveReader::open(Cursor::new(bytes)).expect("open C++ timestamp fixture");
        let catalog = reader
            .read_catalog(ArchiveCatalogLimits::default())
            .expect("read C++ timestamp catalog");
        let compiled = parsed.compile_for_archive(&catalog, options)?;
        let stream = reader
            .read_packed_stream(
                catalog.metadata(),
                catalog.table_metadata(),
                0,
                PackedStreamLimits::default(),
            )
            .expect("read C++ timestamp stream");
        let mut matches = Vec::new();
        for table in catalog
            .schema_tables(0, &stream, ColumnLimits::default())
            .expect("decode C++ timestamp tables")
        {
            matches.extend(
                compiled
                    .match_table(&table.expect("timestamp table"))?
                    .matching_rows(),
            );
        }
        Ok(matches)
    }

    #[test]
    fn integer_equality_fast_path_preserves_raw_and_delta_bitmaps() {
        let raw = integer_equality_fixture(false);
        let cases = [
            (i64::MIN, [1, 0, 0, 0, 0]),
            (-17, [0, 1, 0, 0, 0]),
            (0, [0, 0, 1, 0, 0]),
            (23, [0, 0, 0, 1, 0]),
            (i64::MAX, [0, 0, 0, 0, 1]),
        ];
        for (operand, expected) in cases {
            let bitmap = single_table_bitmap(&raw, &format!("left:{operand}"));
            assert_eq!(expected, bitmap.as_bytes(), "raw operand {operand}");
            assert_eq!(1, bitmap.match_count(), "raw operand {operand}");
        }

        let delta = integer_equality_fixture(true);
        for (operand, expected) in [(0, [1, 0, 0, 0, 0]), (4, [0, 0, 0, 0, 1])] {
            let bitmap = delta_log_index_bitmap(&delta, operand);
            assert_eq!(expected, bitmap.as_bytes(), "delta operand {operand}");
            assert_eq!(1, bitmap.match_count(), "delta operand {operand}");
        }
    }

    #[test]
    fn integer_equality_fast_path_ors_columns_and_preserves_generic_routes() {
        let bytes = integer_equality_fixture(false);
        let wildcard = single_table_bitmap(&bytes, "*:7");
        assert_eq!([1, 0, 1, 0, 0], wildcard.as_bytes());
        assert_eq!(2, wildcard.match_count());

        let cases = [
            ("NOT left:0", [1, 1, 0, 1, 1], 4),
            ("left:(-17 23)", [0, 1, 0, 1, 0], 2),
            ("left > 0", [0, 0, 0, 1, 1], 2),
            ("left:0.0", [0, 0, 1, 0, 0], 1),
        ];
        for (query, expected, match_count) in cases {
            let bitmap = single_table_bitmap(&bytes, query);
            assert_eq!(expected, bitmap.as_bytes(), "{query}");
            assert_eq!(match_count, bitmap.match_count(), "{query}");
        }

        let arrays = single_table_bitmap(&characterization_array_fixture(), "arr.n:5");
        assert_eq!([0, 1, 0, 1], arrays.as_bytes());
        assert_eq!(2, arrays.match_count());
    }

    #[test]
    fn typed_scalars_dictionary_lexemes_and_ranges_match_cpp() {
        let bytes = fixture();
        let cases: &[(&str, &[i64])] = &[
            ("n:\"7\"", &[0, 2, 3]),
            ("n > \"6.5\"", &[0, 1, 2]),
            ("flag:true", &[0, 3]),
            ("nullish:null", &[0, 1, 3]),
            ("NOT id:null", &[0, 1, 2, 3, 4]),
            (r#"msg:"*Abc123*""#, &[0]),
            (r#"token:"a*e""#, &[0, 1]),
            (r#"token:"a\*e""#, &[0]),
        ];
        for (query, expected) in cases {
            assert_eq!(*expected, matched_ids(&bytes, query, false), "{query}");
        }
    }

    #[test]
    fn compact_list_modes_match_cpp_across_scalar_types() {
        let bytes = fixture();
        let cases: &[(&str, &[i64])] = &[
            ("n:(7 8)", &[0, 1, 2, 3]),
            ("n:(OR 7 8)", &[0, 1, 2, 3]),
            ("n:(AND 7 7.0)", &[0, 2]),
            ("n:(NOT 7 8)", &[4]),
            ("optional:(yes no)", &[0, 1, 4]),
            ("optional:(AND yes yes)", &[0, 4]),
            ("optional:(NOT yes no)", &[3]),
            ("NOT optional:(yes no)", &[3]),
            ("NOT optional:(AND yes no)", &[0, 1, 3, 4]),
            ("flag:(true false)", &[0, 1, 3]),
            ("flag:(NOT false)", &[0, 3]),
            ("nullish:(null other)", &[0, 1, 3]),
            ("nullish:(NOT null)", &[3]),
            (r"token:(a\*e a?cde)", &[0, 1]),
        ];
        for (query, expected) in cases {
            assert_eq!(*expected, matched_ids(&bytes, query, false), "{query}");
        }
        assert_eq!(
            [0, 2, 3, 4],
            matched_ids(&bytes, "s:(mixed other)", true).as_slice()
        );
    }

    #[test]
    fn compact_lists_preserve_empty_and_missing_field_boolean_semantics() {
        let bytes = fixture();
        // C++ normalizes an empty OR to false and an empty AND (including NOT-list's base AND) to
        // true. A standalone true expression has no descriptor for C++'s schema prefilter, so the
        // composed cases below pin the actual Boolean identities used by query evaluation.
        let cases: &[(&str, &[i64])] = &[
            ("feature:true AND absent:()", &[]),
            ("feature:true AND absent:(AND)", &[0, 1, 2, 3]),
            ("feature:true AND absent:(NOT)", &[0, 1, 2, 3]),
            ("feature:true AND NOT absent:(AND)", &[]),
            ("optional:(NOT *)", &[2]),
            ("optional:(* nope)", &[0, 1, 3, 4]),
            ("optional:(AND * yes)", &[0, 4]),
        ];
        for (query, expected) in cases {
            assert_eq!(*expected, matched_ids(&bytes, query, false), "{query}");
        }
    }

    #[test]
    fn compact_lists_reuse_wildcard_paths_and_range_namespace_resolution() {
        let bytes = fixture();
        assert_eq!(
            [0, 1, 2],
            matched_ids(&bytes, "path.*.leaf:(literal nested direct)", false).as_slice()
        );
        for query in [
            r"$_filename:(* nope)",
            r"$_filename:(AND * *minimal*)",
            r"$_filename:(NOT nope)",
        ] {
            assert_eq!(
                [0],
                minimal_timestamp_matches(query, SearchOptions::default())
                    .expect("match C++ range-index list")
                    .as_slice(),
                "{query}"
            );
        }
    }

    #[test]
    fn unstructured_array_numeric_cases_match_the_six_cpp_characterizations() {
        let bytes = characterization_array_fixture();
        let cases: &[(&str, &[i64])] = &[
            ("arr.n:5", &[1, 3]),
            ("arr.n > 5", &[0, 2]),
            ("arr.n < 5", &[0, 2]),
            ("arr.n >= 5", &[0, 2]),
            ("arr.n <= 5", &[0, 2]),
            ("feature:true AND NOT arr.missing:*", &[]),
        ];
        for (query, expected) in cases {
            assert_eq!(*expected, matched_ids(&bytes, query, false), "{query}");
        }
    }

    #[test]
    fn cpp_array_fixture_matches_nested_mixed_and_typed_values() {
        let bytes = include_bytes!("../../tests/fixtures/sfa-v0.5.0-unstructured-arrays-cpp.bin");
        let cases: &[(&str, &[i64])] = &[
            ("array:1", &[1]),
            ("array:true", &[1]),
            ("array:x", &[1]),
            ("array.k:v", &[1]),
            ("array.k:*", &[1, 2]),
            ("array.n:9", &[3]),
            ("array.x:*", &[5]),
            ("array:2", &[1, 2]),
            ("array:user=*", &[3]),
            ("array.n > 5", &[3]),
            ("array.n < 5", &[3]),
            ("array.n >= 5", &[3]),
            ("array.n <= 5", &[3]),
            ("*:v", &[1]),
            ("*:true", &[1]),
            ("*:null", &[1, 2]),
            ("*:9", &[]),
        ];
        for (query, expected) in cases {
            assert_eq!(
                *expected,
                matched_integer_field(bytes, query, b"kind", SearchOptions::default())
                    .expect("match C++ array fixture"),
                "{query}"
            );
        }
        assert_eq!(
            [1],
            matched_integer_field(
                bytes,
                "array:X",
                b"kind",
                SearchOptions::new(true, SearchLimits::default()),
            )
            .expect("match C++ ASCII ignore-case array value")
            .as_slice()
        );
    }

    #[test]
    fn array_null_transform_matches_cpp_root_nested_missing_not_and_lists() {
        let bytes = include_bytes!("../../tests/fixtures/sfa-v0.5.0-unstructured-arrays-cpp.bin");
        let cases: &[(&str, &[i64])] = &[
            ("array:null", &[0, 1, 2, 3, 4, 5]),
            ("NOT array:null", &[0, 1, 2, 3, 4, 5]),
            ("array:(null)", &[0, 1, 2, 3, 4, 5]),
            ("NOT array:(null)", &[0, 1, 2, 3, 4, 5]),
            ("array:(NOT null)", &[0, 1, 2, 3, 4, 5]),
            ("array:(AND null 9)", &[]),
            ("array.n:null", &[1, 2, 3]),
            ("NOT array.n:null", &[3]),
            ("array.n:(AND null 9)", &[3]),
            ("array.n:(NOT null)", &[3]),
            ("array.missing:null", &[1, 2]),
            ("NOT array.missing:null", &[]),
            ("array.x:null", &[1, 2, 5]),
        ];
        for (query, expected) in cases {
            assert_eq!(
                *expected,
                matched_integer_field(bytes, query, b"kind", SearchOptions::default())
                    .expect("match C++ null transform"),
                "{query}"
            );
        }
    }

    #[test]
    fn unstructured_array_search_limits_are_reported_before_growth() {
        let bytes = include_bytes!("../../tests/fixtures/sfa-v0.5.0-unstructured-arrays-cpp.bin");
        let defaults = SearchLimits::default();
        let state_limited = defaults.with_array_limits(
            0,
            defaults.max_array_nesting_depth(),
            defaults.max_array_decoded_string_bytes(),
        );
        assert!(matches!(
            matched_integer_field(
                bytes,
                "array.k:v",
                b"kind",
                SearchOptions::new(false, state_limited),
            ),
            Err(SearchError::LimitExceeded {
                resource: SearchResource::ArrayStates,
                actual: 1,
                limit: 0,
            })
        ));

        let reconstruction_limited =
            defaults.with_value_limits(1, defaults.max_range_traversal_states());
        assert!(matches!(
            matched_integer_field(
                bytes,
                "array.k:v",
                b"kind",
                SearchOptions::new(false, reconstruction_limited),
            ),
            Err(SearchError::UnstructuredArrayReconstruction { row: 0, .. })
        ));
    }

    #[test]
    fn existence_missing_and_not_use_characterized_three_valued_semantics() {
        let bytes = fixture();
        let cases: &[(&str, &[i64])] = &[
            ("feature:true AND optional:*", &[0, 1, 3]),
            ("feature:true AND NOT optional:*", &[2]),
            ("feature:true AND NOT optional:yes", &[1, 3]),
            ("NOT msg:plain", &[0, 1, 2, 3, 4]),
        ];
        for (query, expected) in cases {
            assert_eq!(*expected, matched_ids(&bytes, query, false), "{query}");
        }
    }

    #[test]
    fn component_globstar_and_literal_star_follow_schema_tree_paths() {
        let bytes = fixture();
        assert_eq!(
            [0, 1, 2],
            matched_ids(&bytes, "path.*.leaf:*", false).as_slice()
        );
        assert_eq!(
            [0],
            matched_ids(&bytes, r"path.\*.leaf:*", false).as_slice()
        );
    }

    #[test]
    fn structured_array_paths_match_cpp_repeated_empty_and_nested_occurrences() {
        let bytes = decode_hex(STRUCTURED_ARRAY_FIXTURE_HEX);
        let cases: &[(&str, &[i64])] = &[
            ("items.x:1", &[0, 1]),
            ("items.y:2", &[0, 1]),
            ("items.x:1 AND items.y:2", &[0, 1]),
            ("items.x:*", &[0, 1, 2, 4, 5]),
            ("NOT items.x:*", &[3, 6, 7, 8]),
            ("items.x:null", &[0, 4, 5]),
            ("NOT items.x:null", &[0, 1, 2]),
            ("items:*", &[0, 4]),
            ("NOT items:*", &[1, 2, 3, 5, 6, 7, 8]),
            ("items.*:*", &[0, 4]),
            ("items.*:1", &[0]),
            ("items.*:null", &[0, 4]),
            ("items.*.x:1", &[0]),
            ("items.*.nested.z:yes", &[]),
            ("items.nested.z:yes", &[7]),
            ("items.nested.z:*", &[7, 8]),
            ("items.nested.z:deep", &[8]),
            ("obj.items.x:5", &[6]),
            ("items.x > 0", &[0, 1, 2]),
            ("items.x < 1", &[1]),
            ("*:1", &[0, 1]),
        ];
        for (query, expected) in cases {
            assert_eq!(
                *expected,
                matched_integer_field(&bytes, query, b"id", SearchOptions::default())
                    .expect("match C++ structured-array fixture"),
                "{query}"
            );
        }
    }

    #[test]
    fn structured_schema_validation_uses_search_nesting_bounds() {
        let bytes = decode_hex(STRUCTURED_ARRAY_FIXTURE_HEX);
        let defaults = SearchLimits::default();
        let limits = defaults.with_array_limits(
            defaults.max_array_json_states(),
            0,
            defaults.max_array_decoded_string_bytes(),
        );

        assert!(matches!(
            matched_integer_field(&bytes, "*:*", b"id", SearchOptions::new(false, limits),),
            Err(SearchError::StructuredSchema {
                source: ExtractionPlanError::LimitExceeded {
                    resource: crate::ExtractionPlanResource::NestingDepth,
                    ..
                },
                ..
            })
        ));
    }

    #[test]
    fn ignore_case_folds_ascii_but_not_non_ascii_utf8() {
        let bytes = fixture();
        assert_eq!([0, 2, 4], matched_ids(&bytes, "s:mixed", true).as_slice());
        assert_eq!([1], matched_ids(&bytes, "s:École", true).as_slice());
        assert_eq!(Vec::<i64>::new(), matched_ids(&bytes, "s:école", true));
    }

    #[test]
    fn timestamp_expressions_match_cpp_nanosecond_and_default_pattern_vectors() {
        let cases: &[(&str, &[usize])] = &[
            (r#"ts:timestamp("1700000000123")"#, &[0]),
            (r#"ts:timestamp("1700000000.123")"#, &[0]),
            (r#"ts:timestamp("1700000000123000000", "\N")"#, &[0]),
            (r#"ts:timestamp("2023-11-14 22:13:20.123000000")"#, &[0]),
            (r#"ts < timestamp("1700000000123000001", "\N")"#, &[0]),
            (r#"ts > timestamp("1700000000123000000", "\N")"#, &[]),
            ("ts:1700000000123000000", &[0]),
        ];
        for (query, expected) in cases {
            assert_eq!(
                *expected,
                minimal_timestamp_matches(query, SearchOptions::default())
                    .expect("match timestamp expression"),
                "{query}"
            );
        }
    }

    #[test]
    fn authoritative_millisecond_bounds_are_inclusive_and_use_the_first_range() {
        let exact =
            AuthoritativeTimestampRange::new(Some(1_700_000_000_123), Some(1_700_000_000_123));
        let options = SearchOptions::default().with_authoritative_timestamp_range(exact);
        assert_eq!(
            [0],
            minimal_timestamp_matches("active:true", options)
                .expect("inclusive exact range")
                .as_slice()
        );

        for range in [
            AuthoritativeTimestampRange::new(Some(1_700_000_000_124), None),
            AuthoritativeTimestampRange::new(None, Some(1_700_000_000_122)),
        ] {
            let options = SearchOptions::default().with_authoritative_timestamp_range(range);
            assert_eq!(
                Vec::<usize>::new(),
                minimal_timestamp_matches("active:true", options).expect("disjoint range")
            );
        }
    }

    #[test]
    fn timestamp_option_and_literal_failures_are_structured() {
        let bytes = fixture();
        let mut reader =
            SingleFileArchiveReader::open(Cursor::new(bytes)).expect("open no-timestamp fixture");
        let catalog = reader
            .read_catalog(ArchiveCatalogLimits::default())
            .expect("read no-timestamp catalog");
        let query = parse_kql("feature:true", KqlLimits::default()).expect("parse base query");
        let missing = SearchOptions::default()
            .with_authoritative_timestamp_range(AuthoritativeTimestampRange::new(Some(0), None));
        assert!(matches!(
            query.compile_for_archive(&catalog, missing),
            Err(SearchError::MissingAuthoritativeTimestamp)
        ));

        let reversed = SearchOptions::default()
            .with_authoritative_timestamp_range(AuthoritativeTimestampRange::new(Some(1), Some(0)));
        assert!(matches!(
            query.compile_for_archive(&catalog, reversed),
            Err(SearchError::InvalidAuthoritativeTimestampRange {
                begin_milliseconds: 1,
                end_milliseconds: 0
            })
        ));

        assert!(matches!(
            minimal_timestamp_matches(
                r#"ts:timestamp("not a timestamp")"#,
                SearchOptions::default()
            ),
            Err(SearchError::TimestampLiteral {
                source: TimestampQueryError::IncompatibleValue,
                ..
            })
        ));
        let out_of_range = SearchOptions::default().with_authoritative_timestamp_range(
            AuthoritativeTimestampRange::new(Some(i64::MAX), None),
        );
        assert!(matches!(
            minimal_timestamp_matches("active:true", out_of_range),
            Err(SearchError::AuthoritativeTimestampOutOfRange {
                milliseconds: i64::MAX
            })
        ));
    }

    #[test]
    fn range_index_scalar_path_evaluation_is_bounded_and_typed() {
        let mut nested = BTreeMap::new();
        nested.insert(
            "name".to_owned(),
            RangeIndexValue::String("MiXeD".to_owned()),
        );
        let mut fields = BTreeMap::new();
        fields.insert("nested".to_owned(), RangeIndexValue::Object(nested));
        fields.insert("count".to_owned(), RangeIndexValue::Signed(7));
        let options = SearchOptions::new(true, SearchLimits::default());
        let cases = [
            ("$nested.name:mixed", Tri::True),
            ("$*.name:mixed", Tri::True),
            ("$count > 6.5", Tri::True),
            ("$missing:yes", Tri::Unknown),
            ("$missing:*", Tri::False),
        ];
        for (source, expected) in cases {
            let query = parse_kql(source, KqlLimits::default()).expect("parse range query");
            let ExpressionKind::Predicate(predicate) =
                query.node(query.root()).expect("root expression").kind()
            else {
                panic!("range test root must be a predicate");
            };
            let mut work = Vec::new();
            assert_eq!(
                expected,
                evaluate_range_fields(
                    &fields,
                    query.root(),
                    false,
                    PredicateRef::from_predicate(predicate),
                    options,
                    &mut work,
                )
                .expect("evaluate range predicate"),
                "{source}"
            );
        }
    }

    #[test]
    fn bitmap_limits_fail_explicitly() {
        let bytes = fixture();
        let mut reader =
            SingleFileArchiveReader::open(Cursor::new(&bytes)).expect("open semantic fixture");
        let catalog = reader
            .read_catalog(ArchiveCatalogLimits::default())
            .expect("read semantic catalog");
        let defaults = SearchLimits::default();
        let list_limits = SearchLimits::new(
            defaults.max_schema_nodes(),
            defaults.max_archive_tables(),
            2,
            defaults.max_path_states(),
            defaults.max_dictionary_entries_scanned(),
            defaults.max_dictionary_matches(),
            defaults.max_live_bitmap_bytes(),
        );
        let list = parse_kql("n:(7 8)", KqlLimits::default()).expect("parse bounded list");
        assert!(matches!(
            list.compile_for_archive(&catalog, SearchOptions::new(false, list_limits)),
            Err(SearchError::LimitExceeded {
                resource: SearchResource::CompiledProgram,
                actual: 3,
                limit: 2,
            })
        ));

        let parsed = parse_kql("feature:true", KqlLimits::default()).expect("parse limited query");
        let limits = SearchLimits::default();
        let limited = SearchLimits::new(
            limits.max_schema_nodes(),
            limits.max_archive_tables(),
            limits.max_resolved_nodes(),
            limits.max_path_states(),
            limits.max_dictionary_entries_scanned(),
            limits.max_dictionary_matches(),
            0,
        );
        let compiled = parsed
            .compile_for_archive(&catalog, SearchOptions::new(false, limited))
            .expect("compile bitmap-limited query");
        let stream = reader
            .read_packed_stream(
                catalog.metadata(),
                catalog.table_metadata(),
                0,
                PackedStreamLimits::default(),
            )
            .expect("read semantic stream");
        let decoded = catalog
            .schema_tables(0, &stream, ColumnLimits::default())
            .expect("open semantic tables")
            .next()
            .expect("first table")
            .expect("decode first table");
        assert!(matches!(
            compiled.match_table(&decoded),
            Err(SearchError::LimitExceeded {
                resource: SearchResource::BitmapBytes,
                ..
            })
        ));

        let row_count = decoded.table().message_count();
        let list_bitmap_limits = SearchLimits::new(
            limits.max_schema_nodes(),
            limits.max_archive_tables(),
            limits.max_resolved_nodes(),
            limits.max_path_states(),
            limits.max_dictionary_entries_scanned(),
            limits.max_dictionary_matches(),
            row_count,
        );
        let list = parse_kql("n:(7 8)", KqlLimits::default()).expect("parse bitmap-limited list");
        let compiled = list
            .compile_for_archive(&catalog, SearchOptions::new(false, list_bitmap_limits))
            .expect("compile bitmap-limited list");
        assert!(matches!(
            compiled.match_table(&decoded),
            Err(SearchError::LimitExceeded {
                resource: SearchResource::BitmapBytes,
                actual,
                limit,
            }) if actual == row_count * 2 && limit == row_count
        ));
    }

    #[test]
    fn match_bitmap_iterator_is_exact_and_physical() {
        let bitmap = MatchBitmap {
            bytes: vec![0, 1, 0, 1, 1],
            match_count: 3,
        };
        let mut rows = bitmap.matching_rows();
        assert_eq!((3, Some(3)), rows.size_hint());
        assert_eq!(Some(1), rows.next());
        assert_eq!(2, rows.len());
        assert_eq!([3, 4], rows.collect::<Vec<_>>().as_slice());
    }
}
