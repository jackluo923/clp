//! Direct, bounded search over borrowed KV-IR events.
//!
//! The evaluator retains only the active stream's schema trees, a query program, resolved node
//! references, and reusable per-event scratch. It never constructs an archive or a JSON DOM.
//! [`search_first_kv_ir_stream`] deliberately stops at the first explicit IR end marker: the
//! pinned C++ command ignores immediately concatenated streams even though [`KvIrReader`] can
//! decode them for library callers that want that behavior.
//!
//! C++ compatibility also requires two less-obvious rules. A path containing only one unescaped
//! `*` searches both protocol namespaces even if it carries `@`, `$`, `!`, or `#`; and unresolved
//! predicates evaluate to `Pruned`, which negation does not invert. In an `AND`, `Pruned` takes
//! precedence over `False`; in an `OR`, a `False` takes precedence when no operand is true.

use std::error::Error;
use std::fmt;
use std::fmt::Display;
use std::fmt::Formatter;
use std::io;
use std::io::Read;
use std::io::Write;
use std::path::Path;

use super::BooleanOperator;
use super::ColumnNamespace;
use super::ColumnPath;
use super::ComparisonOperator;
use super::ExpressionKind;
use super::ListOperator;
use super::Literal;
use super::NodeId;
use super::ParsedQuery;
use super::PathComponent;
use super::TimestampQueryError;
use super::timestamp_query::resolve_timestamp_literal;
use super::wildcard::wildcard_match;
use crate::ingest::KvIrEncodedText;
use crate::ingest::KvIrEncodedVariable;
use crate::ingest::KvIrEncoding;
use crate::ingest::KvIrErrorKind;
use crate::ingest::KvIrItem;
use crate::ingest::KvIrLogEvent;
use crate::ingest::KvIrNamespace;
use crate::ingest::KvIrNodeType;
use crate::ingest::KvIrPair;
use crate::ingest::KvIrReadError;
use crate::ingest::KvIrReader;
use crate::ingest::KvIrSchemaNode;
use crate::ingest::KvIrSink;
use crate::ingest::KvIrTruncatedContext;
use crate::ingest::KvIrValueKind;
use crate::ingest::NdjsonInvalidRecordKind;
use crate::ingest::NdjsonLimitResource;
use crate::ingest::json_canonical::CanonicalJsonError;
use crate::ingest::json_canonical::CanonicalJsonLimits;
use crate::ingest::json_canonical::CanonicalJsonResource;
use crate::ingest::json_canonical::CanonicalJsonScratch;
use crate::json::JsonBytePolicy;
use crate::json::JsonEscapeError;
use crate::json::JsonEscapeLimits;
use crate::json::NlohmannFloatError;
use crate::json::append_json_key_bytes;
use crate::json::append_json_string_bytes;
use crate::json::format_nlohmann_float;

const MEBIBYTE: u64 = 1024 * 1024;
const NO_PAIR: usize = usize::MAX;
const KV_IR_EXTENSION: &[u8] = b".clp.zst";

/// Returns whether C++ attempts direct KV-IR search for `path`.
///
/// This intentionally tests for a case-sensitive substring in the complete encoded path, not a
/// filename extension. A directory component or suffix such as `.clp.zst.backup` is sufficient.
#[must_use]
pub fn is_kv_ir_search_candidate(path: &Path) -> bool {
    path.as_os_str()
        .as_encoded_bytes()
        .windows(KV_IR_EXTENSION.len())
        .any(|window| window == KV_IR_EXTENSION)
}

/// Returns whether `error` is the truncation case that C++ direct KV-IR search treats as success.
///
/// The compatibility exception is deliberately narrow: C++ warns and exits successfully only
/// when EOF interrupts the payload of a schema-tree node ID. Other truncation contexts, input
/// errors, invalid protocol data, and sink failures remain fatal. Complete matches emitted before
/// this error are retained by [`KvIrSearchSink`].
#[must_use]
pub const fn is_cpp_tolerated_kv_ir_truncation<E>(error: &KvIrReadError<E>) -> bool {
    matches!(
        error,
        KvIrReadError::Reader(source)
            if matches!(
                source.kind(),
                KvIrErrorKind::Truncated {
                    context: KvIrTruncatedContext::SchemaNodeIdPayload,
                    ..
                }
            )
    )
}

/// Hard limits for retained KV-IR search state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KvIrSearchLimits {
    schema_nodes_per_namespace: u64,
    schema_key_bytes: u64,
    nesting_depth: u64,
    compiled_leaves: u64,
    resolved_node_references: u64,
    reconstructed_value_bytes: u64,
}

impl KvIrSearchLimits {
    /// Conservative defaults independent of the decoder's wire limits.
    pub const DEFAULT: Self = Self {
        schema_nodes_per_namespace: 1_000_000,
        schema_key_bytes: 64 * MEBIBYTE,
        nesting_depth: 256,
        compiled_leaves: 262_144,
        resolved_node_references: 4_000_000,
        reconstructed_value_bytes: 16 * MEBIBYTE,
    };

    /// Creates the default limit set for builder-style overrides.
    #[must_use]
    pub const fn new() -> Self {
        Self::DEFAULT
    }

    #[must_use]
    pub const fn with_max_schema_nodes_per_namespace(mut self, value: u64) -> Self {
        self.schema_nodes_per_namespace = value;
        self
    }

    #[must_use]
    pub const fn with_max_schema_key_bytes(mut self, value: u64) -> Self {
        self.schema_key_bytes = value;
        self
    }

    #[must_use]
    pub const fn with_max_nesting_depth(mut self, value: u64) -> Self {
        self.nesting_depth = value;
        self
    }

    #[must_use]
    pub const fn with_max_compiled_leaves(mut self, value: u64) -> Self {
        self.compiled_leaves = value;
        self
    }

    #[must_use]
    pub const fn with_max_resolved_node_references(mut self, value: u64) -> Self {
        self.resolved_node_references = value;
        self
    }

    #[must_use]
    pub const fn with_max_reconstructed_value_bytes(mut self, value: u64) -> Self {
        self.reconstructed_value_bytes = value;
        self
    }

    #[must_use]
    pub const fn max_schema_nodes_per_namespace(self) -> u64 {
        self.schema_nodes_per_namespace
    }

    #[must_use]
    pub const fn max_schema_key_bytes(self) -> u64 {
        self.schema_key_bytes
    }

    #[must_use]
    pub const fn max_nesting_depth(self) -> u64 {
        self.nesting_depth
    }

    #[must_use]
    pub const fn max_compiled_leaves(self) -> u64 {
        self.compiled_leaves
    }

    #[must_use]
    pub const fn max_resolved_node_references(self) -> u64 {
        self.resolved_node_references
    }

    #[must_use]
    pub const fn max_reconstructed_value_bytes(self) -> u64 {
        self.reconstructed_value_bytes
    }
}

impl Default for KvIrSearchLimits {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Direct-search behavior independent of the input reader and match destination.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct KvIrSearchOptions {
    ignore_case: bool,
    limits: KvIrSearchLimits,
}

impl KvIrSearchOptions {
    #[must_use]
    pub const fn new(ignore_case: bool, limits: KvIrSearchLimits) -> Self {
        Self {
            ignore_case,
            limits,
        }
    }

    #[must_use]
    pub const fn with_ignore_case(mut self, value: bool) -> Self {
        self.ignore_case = value;
        self
    }

    #[must_use]
    pub const fn with_limits(mut self, limits: KvIrSearchLimits) -> Self {
        self.limits = limits;
        self
    }

    #[must_use]
    pub const fn ignore_case(self) -> bool {
        self.ignore_case
    }

    #[must_use]
    pub const fn limits(self) -> KvIrSearchLimits {
        self.limits
    }
}

/// Search-owned allocation named by an error.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum KvIrSearchResource {
    CompiledProgram,
    CompiledLeaves,
    ResolvedNodes,
    SchemaNodes,
    SchemaKey,
    SchemaChildren,
    SelectionMap,
    IncludedMap,
    PathScratch,
    EvaluationScratch,
    ReconstructedText,
}

impl Display for KvIrSearchResource {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::CompiledProgram => "compiled KV-IR query program",
            Self::CompiledLeaves => "compiled KV-IR query leaves",
            Self::ResolvedNodes => "resolved KV-IR schema nodes",
            Self::SchemaNodes => "retained KV-IR schema nodes",
            Self::SchemaKey => "retained KV-IR schema key",
            Self::SchemaChildren => "retained KV-IR schema children",
            Self::SelectionMap => "KV-IR event selection map",
            Self::IncludedMap => "KV-IR event inclusion map",
            Self::PathScratch => "KV-IR schema path scratch",
            Self::EvaluationScratch => "KV-IR query evaluation scratch",
            Self::ReconstructedText => "reconstructed KV-IR encoded text",
        })
    }
}

/// Search limit domain.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum KvIrSearchLimitResource {
    SchemaNodesPerNamespace,
    SchemaKeyBytes,
    NestingDepth,
    CompiledLeaves,
    ResolvedNodeReferences,
    ReconstructedValueBytes,
}

impl Display for KvIrSearchLimitResource {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::SchemaNodesPerNamespace => "schema nodes per namespace",
            Self::SchemaKeyBytes => "aggregate schema-key bytes",
            Self::NestingDepth => "schema nesting depth",
            Self::CompiledLeaves => "compiled query leaves",
            Self::ResolvedNodeReferences => "resolved schema-node references",
            Self::ReconstructedValueBytes => "reconstructed encoded-text bytes",
        })
    }
}

/// Exact rejected resource value and configured limit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KvIrSearchLimitViolation {
    resource: KvIrSearchLimitResource,
    actual: u64,
    limit: u64,
}

impl KvIrSearchLimitViolation {
    const fn new(resource: KvIrSearchLimitResource, actual: u64, limit: u64) -> Self {
        Self {
            resource,
            actual,
            limit,
        }
    }

    #[must_use]
    pub const fn resource(self) -> KvIrSearchLimitResource {
        self.resource
    }

    #[must_use]
    pub const fn actual(self) -> u64 {
        self.actual
    }

    #[must_use]
    pub const fn limit(self) -> u64 {
        self.limit
    }
}

impl Display for KvIrSearchLimitViolation {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} value {} exceeds limit {}",
            self.resource, self.actual, self.limit
        )
    }
}

impl Error for KvIrSearchLimitViolation {}

/// Invalid callback sequence or retained-schema invariant.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum KvIrSearchInvalidData {
    NestedStreamStart,
    ItemOutsideStream,
    SchemaNodeOutOfSequence {
        namespace: KvIrNamespace,
        expected: u32,
        actual: u32,
    },
    MissingParent {
        namespace: KvIrNamespace,
        node_id: u32,
    },
    NonObjectParent {
        namespace: KvIrNamespace,
        node_id: u32,
    },
    InvalidQueryRoot {
        root: usize,
        node_count: usize,
    },
    InvalidQueryOperand {
        node: usize,
        operand: usize,
    },
    EventNodeOutOfRange {
        namespace: KvIrNamespace,
        node_id: u32,
    },
    DuplicateEventNode {
        namespace: KvIrNamespace,
        node_id: u32,
    },
}

impl Display for KvIrSearchInvalidData {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::NestedStreamStart => formatter.write_str("nested KV-IR stream start"),
            Self::ItemOutsideStream => formatter.write_str("KV-IR item outside an active stream"),
            Self::SchemaNodeOutOfSequence {
                namespace,
                expected,
                actual,
            } => write!(
                formatter,
                "{namespace} schema node {actual} arrived where node {expected} was expected"
            ),
            Self::MissingParent { namespace, node_id } => {
                write!(formatter, "{namespace} schema parent {node_id} is missing")
            }
            Self::NonObjectParent { namespace, node_id } => {
                write!(
                    formatter,
                    "{namespace} schema parent {node_id} is not an object"
                )
            }
            Self::InvalidQueryRoot { root, node_count } => write!(
                formatter,
                "query root {root} is outside the {node_count}-node arena"
            ),
            Self::InvalidQueryOperand { node, operand } => write!(
                formatter,
                "query node {node} references invalid or non-prior operand {operand}"
            ),
            Self::EventNodeOutOfRange { namespace, node_id } => {
                write!(
                    formatter,
                    "event references missing {namespace} node {node_id}"
                )
            }
            Self::DuplicateEventNode { namespace, node_id } => {
                write!(formatter, "event repeats {namespace} node {node_id}")
            }
        }
    }
}

/// Query compilation or streaming evaluation failure.
#[derive(Debug)]
#[non_exhaustive]
pub enum KvIrSearchFailure {
    Invalid(KvIrSearchInvalidData),
    Limit(KvIrSearchLimitViolation),
    TimestampLiteral {
        node: NodeId,
        source: TimestampQueryError,
    },
    EncodedText(KvIrEncodedTextError),
    AllocationFailed {
        resource: KvIrSearchResource,
        requested_additional: usize,
    },
    SizeOverflow,
}

impl Display for KvIrSearchFailure {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(source) => Display::fmt(source, formatter),
            Self::Limit(source) => Display::fmt(source, formatter),
            Self::TimestampLiteral { node, source } => {
                write!(
                    formatter,
                    "timestamp literal at query node {}: {source}",
                    node.index()
                )
            }
            Self::EncodedText(source) => Display::fmt(source, formatter),
            Self::AllocationFailed {
                resource,
                requested_additional,
            } => write!(
                formatter,
                "failed to reserve {requested_additional} additional units for {resource}"
            ),
            Self::SizeOverflow => formatter.write_str("KV-IR search size counter overflow"),
        }
    }
}

impl Error for KvIrSearchFailure {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Limit(source) => Some(source),
            Self::TimestampLiteral { source, .. } => Some(source),
            Self::EncodedText(source) => Some(source),
            _ => None,
        }
    }
}

/// Searcher failure or caller-owned match-sink failure.
#[derive(Debug)]
#[non_exhaustive]
pub enum KvIrSearchError<E> {
    Search(KvIrSearchFailure),
    MatchSink(E),
}

impl<E: Display> Display for KvIrSearchError<E> {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Search(source) => Display::fmt(source, formatter),
            Self::MatchSink(source) => write!(formatter, "KV-IR match sink failed: {source}"),
        }
    }
}

impl<E: Error + 'static> Error for KvIrSearchError<E> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Search(source) => Some(source),
            Self::MatchSink(source) => Some(source),
        }
    }
}

/// Successfully committed direct-search counters.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct KvIrSearchStats {
    streams: u64,
    events: u64,
    matches: u64,
}

impl KvIrSearchStats {
    #[must_use]
    pub const fn streams(self) -> u64 {
        self.streams
    }

    #[must_use]
    pub const fn events(self) -> u64 {
        self.events
    }

    #[must_use]
    pub const fn matches(self) -> u64 {
        self.matches
    }
}

/// One retained schema node exposed to synchronous match consumers.
#[derive(Debug)]
pub struct KvIrSearchSchemaNode {
    parent: usize,
    children: Vec<usize>,
    children_dirty: bool,
    key: Vec<u8>,
    node_type: KvIrNodeType,
}

impl KvIrSearchSchemaNode {
    #[must_use]
    pub const fn parent_id(&self) -> usize {
        self.parent
    }

    #[must_use]
    pub fn key(&self) -> &[u8] {
        &self.key
    }

    #[must_use]
    pub const fn node_type(&self) -> KvIrNodeType {
        self.node_type
    }

    #[must_use]
    pub fn child_ids(&self) -> &[usize] {
        &self.children
    }
}

/// A matching borrowed event plus the searcher's retained schema and node selection.
#[derive(Clone, Copy, Debug)]
pub struct KvIrMatchedEvent<'a> {
    event: KvIrLogEvent<'a>,
    auto_schema: &'a [KvIrSearchSchemaNode],
    user_schema: &'a [KvIrSearchSchemaNode],
    selected_auto: &'a [usize],
    selected_user: &'a [usize],
    included_auto: &'a [bool],
    included_user: &'a [bool],
}

impl<'a> KvIrMatchedEvent<'a> {
    #[must_use]
    pub const fn event(self) -> KvIrLogEvent<'a> {
        self.event
    }

    #[must_use]
    pub const fn schema(self, namespace: KvIrNamespace) -> &'a [KvIrSearchSchemaNode] {
        match namespace {
            KvIrNamespace::AutoGenerated => self.auto_schema,
            KvIrNamespace::UserGenerated => self.user_schema,
        }
    }

    #[must_use]
    pub fn pair(self, namespace: KvIrNamespace, node_id: usize) -> Option<KvIrPair<'a>> {
        let selected = match namespace {
            KvIrNamespace::AutoGenerated => self.selected_auto,
            KvIrNamespace::UserGenerated => self.selected_user,
        };
        let pair_index = *selected.get(node_id)?;
        (NO_PAIR != pair_index)
            .then(|| self.event.pair(pair_index))
            .flatten()
    }
}

/// Synchronous destination for matching borrowed events.
pub trait KvIrMatchSink {
    type Error;

    /// Accepts one event whose borrowed data remains valid only for this call.
    ///
    /// # Errors
    ///
    /// Returns the destination's error if the match cannot be committed.
    fn write_match(&mut self, event: KvIrMatchedEvent<'_>) -> Result<(), Self::Error>;
}

impl<F, E> KvIrMatchSink for F
where
    F: for<'event> FnMut(KvIrMatchedEvent<'event>) -> Result<(), E>,
{
    type Error = E;

    fn write_match(&mut self, event: KvIrMatchedEvent<'_>) -> Result<(), Self::Error> {
        self(event)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
enum Tri {
    False,
    True,
    Pruned,
}

const fn tri_not(value: Tri) -> Tri {
    match value {
        Tri::False => Tri::True,
        Tri::True => Tri::False,
        Tri::Pruned => Tri::Pruned,
    }
}

const fn tri_and(left: Tri, right: Tri) -> Tri {
    if matches!(left, Tri::Pruned) || matches!(right, Tri::Pruned) {
        Tri::Pruned
    } else if matches!(left, Tri::False) || matches!(right, Tri::False) {
        Tri::False
    } else {
        Tri::True
    }
}

const fn tri_or(left: Tri, right: Tri) -> Tri {
    if matches!(left, Tri::True) || matches!(right, Tri::True) {
        Tri::True
    } else if matches!(left, Tri::False) || matches!(right, Tri::False) {
        Tri::False
    } else {
        Tri::Pruned
    }
}

const TYPE_INTEGER: u16 = 1 << 0;
const TYPE_FLOAT: u16 = 1 << 1;
const TYPE_CLP_STRING: u16 = 1 << 2;
const TYPE_VAR_STRING: u16 = 1 << 3;
const TYPE_BOOLEAN: u16 = 1 << 4;
const TYPE_ARRAY: u16 = 1 << 5;
const TYPE_NULL: u16 = 1 << 6;
const TYPE_ALL: u16 = TYPE_INTEGER
    | TYPE_FLOAT
    | TYPE_CLP_STRING
    | TYPE_VAR_STRING
    | TYPE_BOOLEAN
    | TYPE_ARRAY
    | TYPE_NULL;

#[derive(Clone, Copy)]
enum LeafMode {
    Compare,
    Exists,
    NonNullExists,
}

struct CompiledLeaf<'query> {
    path: &'query ColumnPath,
    operator: ComparisonOperator,
    literal: &'query Literal,
    types: u16,
    mode: LeafMode,
    invert: bool,
    timestamp_nanoseconds: Option<i64>,
    resolved: Vec<u32>,
}

impl CompiledLeaf<'_> {
    fn pure_wildcard(&self) -> bool {
        matches!(self.path.components(), [component] if component.is_wildcard())
    }

    const fn namespace(&self) -> Option<KvIrNamespace> {
        match self.path.namespace() {
            ColumnNamespace::Default => Some(KvIrNamespace::UserGenerated),
            ColumnNamespace::Autogenerated => Some(KvIrNamespace::AutoGenerated),
            ColumnNamespace::RangeIndex
            | ColumnNamespace::ReservedBang
            | ColumnNamespace::ReservedHash => None,
        }
    }
}

#[derive(Clone, Copy)]
enum CompiledNode {
    Leaf(usize),
    List {
        start: usize,
        end: usize,
        operator: ListOperator,
    },
    Not(usize),
    Boolean {
        operator: BooleanOperator,
        left: usize,
        right: usize,
    },
}

struct CompiledProgram<'query> {
    nodes: Vec<CompiledNode>,
    leaves: Vec<CompiledLeaf<'query>>,
    root: usize,
}

impl<'query> CompiledProgram<'query> {
    fn compile(
        query: &'query ParsedQuery,
        limits: KvIrSearchLimits,
    ) -> Result<Self, KvIrSearchFailure> {
        let root = query.root().index();
        if root >= query.nodes().len() {
            return Err(KvIrSearchFailure::Invalid(
                KvIrSearchInvalidData::InvalidQueryRoot {
                    root,
                    node_count: query.nodes().len(),
                },
            ));
        }
        let mut nodes = Vec::new();
        nodes
            .try_reserve_exact(query.nodes().len())
            .map_err(|_| allocation(KvIrSearchResource::CompiledProgram, query.nodes().len()))?;
        let mut leaves = Vec::new();

        for (node_index, node) in query.nodes().iter().enumerate() {
            let compiled = match node.kind() {
                ExpressionKind::Predicate(predicate) => {
                    let leaf = Self::push_leaf(
                        &mut leaves,
                        query,
                        node_index,
                        predicate.path(),
                        predicate.operator(),
                        predicate.value(),
                        false,
                        limits,
                    )?;
                    CompiledNode::Leaf(leaf)
                }
                ExpressionKind::List(list) => {
                    let start = leaves.len();
                    for value in list.values() {
                        Self::push_leaf(
                            &mut leaves,
                            query,
                            node_index,
                            list.path(),
                            ComparisonOperator::Equal,
                            value,
                            matches!(list.operator(), ListOperator::None),
                            limits,
                        )?;
                    }
                    CompiledNode::List {
                        start,
                        end: leaves.len(),
                        operator: list.operator(),
                    }
                }
                ExpressionKind::Not { operand } => {
                    Self::validate_operand(query, node_index, *operand)?;
                    if let Some(predicate) = direct_null_predicate(query, *operand) {
                        let leaf = Self::push_leaf(
                            &mut leaves,
                            query,
                            node_index,
                            predicate.path(),
                            predicate.operator(),
                            predicate.value(),
                            true,
                            limits,
                        )?;
                        CompiledNode::Leaf(leaf)
                    } else {
                        CompiledNode::Not(operand.index())
                    }
                }
                ExpressionKind::Boolean {
                    operator,
                    left,
                    right,
                } => {
                    Self::validate_operand(query, node_index, *left)?;
                    Self::validate_operand(query, node_index, *right)?;
                    CompiledNode::Boolean {
                        operator: *operator,
                        left: left.index(),
                        right: right.index(),
                    }
                }
            };
            nodes.push(compiled);
        }

        Ok(Self {
            nodes,
            leaves,
            root,
        })
    }

    fn validate_operand(
        query: &ParsedQuery,
        node_index: usize,
        operand: NodeId,
    ) -> Result<(), KvIrSearchFailure> {
        if operand.index() >= node_index || operand.index() >= query.nodes().len() {
            return Err(KvIrSearchFailure::Invalid(
                KvIrSearchInvalidData::InvalidQueryOperand {
                    node: node_index,
                    operand: operand.index(),
                },
            ));
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn push_leaf(
        leaves: &mut Vec<CompiledLeaf<'query>>,
        query: &ParsedQuery,
        node_index: usize,
        path: &'query ColumnPath,
        operator: ComparisonOperator,
        literal: &'query Literal,
        inverted: bool,
        limits: KvIrSearchLimits,
    ) -> Result<usize, KvIrSearchFailure> {
        let actual = leaves
            .len()
            .checked_add(1)
            .ok_or(KvIrSearchFailure::SizeOverflow)?;
        check_limit(
            KvIrSearchLimitResource::CompiledLeaves,
            actual,
            limits.compiled_leaves,
        )?;
        let timestamp_nanoseconds = match literal {
            Literal::Timestamp(value) => {
                Some(resolve_timestamp_literal(value).map_err(|source| {
                    KvIrSearchFailure::TimestampLiteral {
                        node: NodeId::new(node_index),
                        source,
                    }
                })?)
            }
            _ => None,
        };
        let (mode, types, invert) = leaf_semantics(operator, literal, inverted);
        leaves
            .try_reserve(1)
            .map_err(|_| allocation(KvIrSearchResource::CompiledLeaves, 1))?;
        let index = leaves.len();
        leaves.push(CompiledLeaf {
            path,
            operator,
            literal,
            types,
            mode,
            invert,
            timestamp_nanoseconds,
            resolved: Vec::new(),
        });
        let _ = query;
        Ok(index)
    }
}

fn direct_null_predicate(query: &ParsedQuery, operand: NodeId) -> Option<&super::Predicate> {
    let ExpressionKind::Predicate(predicate) = query.node(operand)?.kind() else {
        return None;
    };
    (ComparisonOperator::Equal == predicate.operator()
        && matches!(predicate.value(), Literal::Null))
    .then_some(predicate)
}

fn leaf_semantics(
    operator: ComparisonOperator,
    literal: &Literal,
    inverted: bool,
) -> (LeafMode, u16, bool) {
    if matches!(literal, Literal::Null) && matches!(operator, ComparisonOperator::Equal) && inverted
    {
        return (LeafMode::NonNullExists, TYPE_ALL & !TYPE_NULL, false);
    }
    if is_exists_literal(operator, literal) {
        return (LeafMode::Exists, TYPE_ALL, inverted);
    }
    (
        LeafMode::Compare,
        literal_types(operator, literal),
        inverted,
    )
}

fn is_exists_literal(operator: ComparisonOperator, literal: &Literal) -> bool {
    if !matches!(operator, ComparisonOperator::Equal) {
        return false;
    }
    matches!(literal, Literal::String(value) if string_is_star(value.wildcard_pattern()))
}

const fn string_is_star(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 1 && bytes[0] == b'*'
}

fn literal_types(operator: ComparisonOperator, literal: &Literal) -> u16 {
    let equality = matches!(operator, ComparisonOperator::Equal);
    match literal {
        Literal::Integer { .. } | Literal::Float { .. } => {
            TYPE_INTEGER
                | TYPE_FLOAT
                | if equality {
                    TYPE_VAR_STRING | TYPE_ARRAY
                } else {
                    0
                }
        }
        Literal::Boolean(_) => {
            if equality {
                TYPE_BOOLEAN | TYPE_VAR_STRING | TYPE_ARRAY
            } else {
                0
            }
        }
        Literal::Null => {
            if equality {
                TYPE_NULL | TYPE_VAR_STRING | TYPE_ARRAY
            } else {
                0
            }
        }
        Literal::String(value) => {
            if !equality {
                return 0;
            }
            let pattern = value.wildcard_pattern();
            let mut types = TYPE_ARRAY;
            if pattern.as_bytes().contains(&b' ') {
                types |= TYPE_CLP_STRING;
            } else {
                types |= TYPE_VAR_STRING;
            }
            if value.has_wildcards() {
                types |= TYPE_CLP_STRING;
            }
            types
        }
        Literal::Timestamp(_) => TYPE_INTEGER | TYPE_FLOAT,
    }
}

#[derive(Debug)]
struct SchemaTree {
    nodes: Vec<KvIrSearchSchemaNode>,
    dirty_parents: Vec<usize>,
}

impl SchemaTree {
    fn new() -> Result<Self, KvIrSearchFailure> {
        let mut tree = Self {
            nodes: Vec::new(),
            dirty_parents: Vec::new(),
        };
        tree.reset()?;
        Ok(tree)
    }

    fn reset(&mut self) -> Result<(), KvIrSearchFailure> {
        self.nodes.clear();
        self.dirty_parents.clear();
        self.nodes
            .try_reserve(1)
            .map_err(|_| allocation(KvIrSearchResource::SchemaNodes, 1))?;
        self.nodes.push(KvIrSearchSchemaNode {
            parent: 0,
            children: Vec::new(),
            children_dirty: false,
            key: Vec::new(),
            node_type: KvIrNodeType::Object,
        });
        Ok(())
    }

    fn sort_dirty(&mut self) {
        for parent in self.dirty_parents.drain(..) {
            let mut children = std::mem::take(&mut self.nodes[parent].children);
            children
                .sort_unstable_by(|left, right| self.nodes[*left].key.cmp(&self.nodes[*right].key));
            self.nodes[parent].children = children;
            self.nodes[parent].children_dirty = false;
        }
    }
}

/// Streaming query evaluator over [`KvIrItem`] callbacks.
pub struct KvIrSearchSink<'query, S> {
    sink: S,
    options: KvIrSearchOptions,
    program: CompiledProgram<'query>,
    auto_schema: SchemaTree,
    user_schema: SchemaTree,
    schema_key_bytes: u64,
    resolved_node_references: u64,
    selected_auto: Vec<usize>,
    selected_user: Vec<usize>,
    included_auto: Vec<bool>,
    included_user: Vec<bool>,
    path_scratch: Vec<usize>,
    states: Vec<Tri>,
    reconstructed: Vec<u8>,
    active_stream: bool,
    stats: KvIrSearchStats,
}

impl<'query, S> KvIrSearchSink<'query, S> {
    /// Compiles `query` once and creates a bounded streaming evaluator.
    ///
    /// # Errors
    ///
    /// Returns a structured query, timestamp, allocation, or configured-limit failure before any
    /// IR item is consumed.
    pub fn new(
        query: &'query ParsedQuery,
        sink: S,
        options: KvIrSearchOptions,
    ) -> Result<Self, KvIrSearchFailure> {
        let program = CompiledProgram::compile(query, options.limits)?;
        let mut states = Vec::new();
        states
            .try_reserve_exact(program.nodes.len())
            .map_err(|_| allocation(KvIrSearchResource::EvaluationScratch, program.nodes.len()))?;
        states.resize(program.nodes.len(), Tri::Pruned);
        Ok(Self {
            sink,
            options,
            program,
            auto_schema: SchemaTree::new()?,
            user_schema: SchemaTree::new()?,
            schema_key_bytes: 0,
            resolved_node_references: 0,
            selected_auto: Vec::new(),
            selected_user: Vec::new(),
            included_auto: Vec::new(),
            included_user: Vec::new(),
            path_scratch: Vec::new(),
            states,
            reconstructed: Vec::new(),
            active_stream: false,
            stats: KvIrSearchStats::default(),
        })
    }

    #[must_use]
    pub const fn options(&self) -> KvIrSearchOptions {
        self.options
    }

    #[must_use]
    pub const fn stats(&self) -> KvIrSearchStats {
        self.stats
    }

    /// Returns the schema nodes retained for `namespace`, in node-ID order; node 0 is the root.
    ///
    /// Once a stream has been searched to its end this is the schema of the whole stream, so a
    /// caller can derive the field types from the same pass that produced the matches instead of
    /// decoding the stream again.
    #[must_use]
    pub const fn schema(&self, namespace: KvIrNamespace) -> &[KvIrSearchSchemaNode] {
        match namespace {
            KvIrNamespace::AutoGenerated => self.auto_schema.nodes.as_slice(),
            KvIrNamespace::UserGenerated => self.user_schema.nodes.as_slice(),
        }
    }

    #[must_use]
    pub const fn match_sink(&self) -> &S {
        &self.sink
    }

    #[must_use]
    pub const fn match_sink_mut(&mut self) -> &mut S {
        &mut self.sink
    }

    #[must_use]
    pub fn into_inner(self) -> S {
        self.sink
    }
}

impl<S: KvIrMatchSink> KvIrSearchSink<'_, S> {
    fn start_stream(&mut self) -> Result<(), KvIrSearchFailure> {
        if self.active_stream {
            return Err(KvIrSearchFailure::Invalid(
                KvIrSearchInvalidData::NestedStreamStart,
            ));
        }
        self.auto_schema.reset()?;
        self.user_schema.reset()?;
        self.schema_key_bytes = 0;
        self.resolved_node_references = 0;
        for leaf in &mut self.program.leaves {
            leaf.resolved.clear();
        }
        self.active_stream = true;
        self.stats.streams = self
            .stats
            .streams
            .checked_add(1)
            .ok_or(KvIrSearchFailure::SizeOverflow)?;
        Ok(())
    }

    fn insert_schema(&mut self, node: KvIrSchemaNode<'_>) -> Result<(), KvIrSearchFailure> {
        self.require_active()?;
        let limits = self.options.limits;
        let tree = self.tree(node.namespace());
        let expected =
            u32::try_from(tree.nodes.len()).map_err(|_| KvIrSearchFailure::SizeOverflow)?;
        if node.node_id() != expected {
            return Err(KvIrSearchFailure::Invalid(
                KvIrSearchInvalidData::SchemaNodeOutOfSequence {
                    namespace: node.namespace(),
                    expected,
                    actual: node.node_id(),
                },
            ));
        }
        check_limit(
            KvIrSearchLimitResource::SchemaNodesPerNamespace,
            tree.nodes.len(),
            limits.schema_nodes_per_namespace,
        )?;
        check_limit(
            KvIrSearchLimitResource::NestingDepth,
            node.depth(),
            limits.nesting_depth,
        )?;
        let parent =
            usize::try_from(node.parent_id()).map_err(|_| KvIrSearchFailure::SizeOverflow)?;
        let Some(parent_node) = tree.nodes.get(parent) else {
            return Err(KvIrSearchFailure::Invalid(
                KvIrSearchInvalidData::MissingParent {
                    namespace: node.namespace(),
                    node_id: node.parent_id(),
                },
            ));
        };
        if parent_node.node_type != KvIrNodeType::Object {
            return Err(KvIrSearchFailure::Invalid(
                KvIrSearchInvalidData::NonObjectParent {
                    namespace: node.namespace(),
                    node_id: node.parent_id(),
                },
            ));
        }
        let key_bytes = u64::try_from(node.key().len())
            .ok()
            .and_then(|value| self.schema_key_bytes.checked_add(value))
            .ok_or(KvIrSearchFailure::SizeOverflow)?;
        if key_bytes > limits.schema_key_bytes {
            return Err(KvIrSearchFailure::Limit(KvIrSearchLimitViolation::new(
                KvIrSearchLimitResource::SchemaKeyBytes,
                key_bytes,
                limits.schema_key_bytes,
            )));
        }

        let mut key = Vec::new();
        key.try_reserve(node.key().len())
            .map_err(|_| allocation(KvIrSearchResource::SchemaKey, node.key().len()))?;
        key.extend_from_slice(node.key());
        let node_id = tree.nodes.len();
        let tree = self.tree_mut(node.namespace());
        tree.nodes
            .try_reserve(1)
            .map_err(|_| allocation(KvIrSearchResource::SchemaNodes, 1))?;
        tree.nodes[parent]
            .children
            .try_reserve(1)
            .map_err(|_| allocation(KvIrSearchResource::SchemaChildren, 1))?;
        tree.dirty_parents
            .try_reserve(1)
            .map_err(|_| allocation(KvIrSearchResource::SchemaChildren, 1))?;
        if !tree.nodes[parent].children_dirty {
            tree.dirty_parents.push(parent);
            tree.nodes[parent].children_dirty = true;
        }
        tree.nodes[parent].children.push(node_id);
        tree.nodes.push(KvIrSearchSchemaNode {
            parent,
            children: Vec::new(),
            children_dirty: false,
            key,
            node_type: node.node_type(),
        });
        self.schema_key_bytes = key_bytes;
        self.resolve_inserted_node(node.namespace(), node_id)
    }

    fn resolve_inserted_node(
        &mut self,
        namespace: KvIrNamespace,
        node_id: usize,
    ) -> Result<(), KvIrSearchFailure> {
        self.path_scratch.clear();
        let mut cursor = node_id;
        while cursor != 0 {
            self.path_scratch
                .try_reserve(1)
                .map_err(|_| allocation(KvIrSearchResource::PathScratch, 1))?;
            self.path_scratch.push(cursor);
            cursor = match namespace {
                KvIrNamespace::AutoGenerated => self.auto_schema.nodes[cursor].parent,
                KvIrNamespace::UserGenerated => self.user_schema.nodes[cursor].parent,
            };
        }
        self.path_scratch.reverse();
        let tree = match namespace {
            KvIrNamespace::AutoGenerated => &self.auto_schema,
            KvIrNamespace::UserGenerated => &self.user_schema,
        };
        let node_type = tree.nodes[node_id].node_type;
        let leaves = &mut self.program.leaves;
        let resolved_node_references = &mut self.resolved_node_references;
        for leaf in leaves {
            if leaf.pure_wildcard()
                || leaf.namespace() != Some(namespace)
                || 0 == leaf.types & schema_type_mask(node_type)
                || !path_matches(&self.path_scratch, tree, leaf.path.components())
            {
                continue;
            }
            let actual = resolved_node_references
                .checked_add(1)
                .ok_or(KvIrSearchFailure::SizeOverflow)?;
            if actual > self.options.limits.resolved_node_references {
                return Err(KvIrSearchFailure::Limit(KvIrSearchLimitViolation::new(
                    KvIrSearchLimitResource::ResolvedNodeReferences,
                    actual,
                    self.options.limits.resolved_node_references,
                )));
            }
            leaf.resolved
                .try_reserve(1)
                .map_err(|_| allocation(KvIrSearchResource::ResolvedNodes, 1))?;
            leaf.resolved
                .push(u32::try_from(node_id).map_err(|_| KvIrSearchFailure::SizeOverflow)?);
            *resolved_node_references = actual;
        }
        Ok(())
    }

    fn prepare_event(&mut self, event: KvIrLogEvent<'_>) -> Result<(), KvIrSearchFailure> {
        resize_map(
            &mut self.selected_auto,
            self.auto_schema.nodes.len(),
            NO_PAIR,
            KvIrSearchResource::SelectionMap,
        )?;
        resize_map(
            &mut self.selected_user,
            self.user_schema.nodes.len(),
            NO_PAIR,
            KvIrSearchResource::SelectionMap,
        )?;
        resize_map(
            &mut self.included_auto,
            self.auto_schema.nodes.len(),
            false,
            KvIrSearchResource::IncludedMap,
        )?;
        resize_map(
            &mut self.included_user,
            self.user_schema.nodes.len(),
            false,
            KvIrSearchResource::IncludedMap,
        )?;

        // Only the namespace and node ID place a pair in these maps, so the pairs are read
        // without resolving a value for each one.
        for (pair_index, (namespace, raw_node_id)) in event.pair_slots().enumerate() {
            let node_id =
                usize::try_from(raw_node_id).map_err(|_| KvIrSearchFailure::SizeOverflow)?;
            let (tree, selected, included) = match namespace {
                KvIrNamespace::AutoGenerated => (
                    &self.auto_schema,
                    &mut self.selected_auto,
                    &mut self.included_auto,
                ),
                KvIrNamespace::UserGenerated => (
                    &self.user_schema,
                    &mut self.selected_user,
                    &mut self.included_user,
                ),
            };
            if node_id == 0 || node_id >= tree.nodes.len() {
                return Err(KvIrSearchFailure::Invalid(
                    KvIrSearchInvalidData::EventNodeOutOfRange {
                        namespace,
                        node_id: raw_node_id,
                    },
                ));
            }
            if selected[node_id] != NO_PAIR {
                return Err(KvIrSearchFailure::Invalid(
                    KvIrSearchInvalidData::DuplicateEventNode {
                        namespace,
                        node_id: raw_node_id,
                    },
                ));
            }
            selected[node_id] = pair_index;
            let mut ancestor = node_id;
            loop {
                included[ancestor] = true;
                if ancestor == 0 {
                    break;
                }
                ancestor = tree.nodes[ancestor].parent;
            }
        }
        Ok(())
    }

    fn evaluate_event(&mut self, event: KvIrLogEvent<'_>) -> Result<Tri, KvIrSearchFailure> {
        for node_index in 0..self.program.nodes.len() {
            let node = self.program.nodes[node_index];
            self.states[node_index] = match node {
                CompiledNode::Leaf(leaf) => self.evaluate_leaf(leaf, event)?,
                CompiledNode::List {
                    start,
                    end,
                    operator,
                } => self.evaluate_list(start, end, operator, event)?,
                CompiledNode::Not(operand) => tri_not(self.states[operand]),
                CompiledNode::Boolean {
                    operator,
                    left,
                    right,
                } => match operator {
                    BooleanOperator::And => tri_and(self.states[left], self.states[right]),
                    BooleanOperator::Or => tri_or(self.states[left], self.states[right]),
                },
            };
        }
        Ok(self.states[self.program.root])
    }

    fn evaluate_list(
        &mut self,
        start: usize,
        end: usize,
        operator: ListOperator,
        event: KvIrLogEvent<'_>,
    ) -> Result<Tri, KvIrSearchFailure> {
        if start == end {
            return Ok(match operator {
                ListOperator::Any => Tri::False,
                ListOperator::All | ListOperator::None => Tri::True,
            });
        }
        let mut result = match operator {
            ListOperator::Any => Tri::Pruned,
            ListOperator::All | ListOperator::None => Tri::True,
        };
        for leaf in start..end {
            let value = self.evaluate_leaf(leaf, event)?;
            result = match operator {
                ListOperator::Any => tri_or(result, value),
                ListOperator::All | ListOperator::None => tri_and(result, value),
            };
            if matches!(result, Tri::True) && matches!(operator, ListOperator::Any)
                || matches!(result, Tri::Pruned)
                    && matches!(operator, ListOperator::All | ListOperator::None)
            {
                break;
            }
        }
        Ok(result)
    }

    fn evaluate_leaf(
        &mut self,
        leaf_index: usize,
        event: KvIrLogEvent<'_>,
    ) -> Result<Tri, KvIrSearchFailure> {
        let leaf = &self.program.leaves[leaf_index];
        let mut result = Tri::Pruned;
        if leaf.pure_wildcard() {
            for pair in event.pairs() {
                let node_id =
                    usize::try_from(pair.node_id()).map_err(|_| KvIrSearchFailure::SizeOverflow)?;
                let node_type = match pair.namespace() {
                    KvIrNamespace::AutoGenerated => &self.auto_schema,
                    KvIrNamespace::UserGenerated => &self.user_schema,
                }
                .nodes
                .get(node_id)
                .ok_or_else(|| {
                    KvIrSearchFailure::Invalid(KvIrSearchInvalidData::EventNodeOutOfRange {
                        namespace: pair.namespace(),
                        node_id: pair.node_id(),
                    })
                })?
                .node_type;
                let value = evaluate_pair(
                    leaf,
                    pair,
                    node_type,
                    self.options.ignore_case,
                    &mut self.reconstructed,
                    self.options.limits.reconstructed_value_bytes,
                )?;
                result = aggregate_leaf(result, value);
                if matches!(result, Tri::True) {
                    break;
                }
            }
        } else if let Some(namespace) = leaf.namespace() {
            let selected = match namespace {
                KvIrNamespace::AutoGenerated => &self.selected_auto,
                KvIrNamespace::UserGenerated => &self.selected_user,
            };
            let schema = match namespace {
                KvIrNamespace::AutoGenerated => &self.auto_schema,
                KvIrNamespace::UserGenerated => &self.user_schema,
            };
            for &node_id in &leaf.resolved {
                let node_id =
                    usize::try_from(node_id).map_err(|_| KvIrSearchFailure::SizeOverflow)?;
                let pair_index = selected[node_id];
                if pair_index == NO_PAIR {
                    continue;
                }
                let pair = event
                    .pair(pair_index)
                    .ok_or(KvIrSearchFailure::SizeOverflow)?;
                let node_type = schema.nodes[node_id].node_type;
                let value = evaluate_pair(
                    leaf,
                    pair,
                    node_type,
                    self.options.ignore_case,
                    &mut self.reconstructed,
                    self.options.limits.reconstructed_value_bytes,
                )?;
                result = aggregate_leaf(result, value);
                if matches!(result, Tri::True) {
                    break;
                }
            }
        }
        Ok(if leaf.invert { tri_not(result) } else { result })
    }

    fn append_event(&mut self, event: KvIrLogEvent<'_>) -> Result<(), KvIrSearchError<S::Error>> {
        self.require_active().map_err(KvIrSearchError::Search)?;
        let events = self
            .stats
            .events
            .checked_add(1)
            .ok_or(KvIrSearchFailure::SizeOverflow)
            .map_err(KvIrSearchError::Search)?;
        self.prepare_event(event).map_err(KvIrSearchError::Search)?;
        self.reconstructed.clear();
        let matched = matches!(
            self.evaluate_event(event)
                .map_err(KvIrSearchError::Search)?,
            Tri::True
        );
        self.stats.events = events;
        if !matched {
            return Ok(());
        }
        self.auto_schema.sort_dirty();
        self.user_schema.sort_dirty();
        let match_count = self
            .stats
            .matches
            .checked_add(1)
            .ok_or(KvIrSearchFailure::SizeOverflow)
            .map_err(KvIrSearchError::Search)?;
        let matched_event = KvIrMatchedEvent {
            event,
            auto_schema: &self.auto_schema.nodes,
            user_schema: &self.user_schema.nodes,
            selected_auto: &self.selected_auto,
            selected_user: &self.selected_user,
            included_auto: &self.included_auto,
            included_user: &self.included_user,
        };
        self.sink
            .write_match(matched_event)
            .map_err(KvIrSearchError::MatchSink)?;
        self.stats.matches = match_count;
        Ok(())
    }

    const fn require_active(&self) -> Result<(), KvIrSearchFailure> {
        if self.active_stream {
            Ok(())
        } else {
            Err(KvIrSearchFailure::Invalid(
                KvIrSearchInvalidData::ItemOutsideStream,
            ))
        }
    }

    const fn tree(&self, namespace: KvIrNamespace) -> &SchemaTree {
        match namespace {
            KvIrNamespace::AutoGenerated => &self.auto_schema,
            KvIrNamespace::UserGenerated => &self.user_schema,
        }
    }

    const fn tree_mut(&mut self, namespace: KvIrNamespace) -> &mut SchemaTree {
        match namespace {
            KvIrNamespace::AutoGenerated => &mut self.auto_schema,
            KvIrNamespace::UserGenerated => &mut self.user_schema,
        }
    }
}

impl<S: KvIrMatchSink> KvIrSink for KvIrSearchSink<'_, S> {
    type Error = KvIrSearchError<S::Error>;

    fn write_item(&mut self, item: KvIrItem<'_>) -> Result<(), Self::Error> {
        match item {
            KvIrItem::StreamStart(_) => self.start_stream().map_err(KvIrSearchError::Search),
            KvIrItem::SchemaNode(node) => self.insert_schema(node).map_err(KvIrSearchError::Search),
            KvIrItem::LogEvent(event) => self.append_event(event),
            KvIrItem::UtcOffsetChange(_) => self.require_active().map_err(KvIrSearchError::Search),
            KvIrItem::StreamEnd(_) => {
                self.require_active().map_err(KvIrSearchError::Search)?;
                self.active_stream = false;
                Ok(())
            }
        }
    }
}

const fn aggregate_leaf(previous: Tri, value: Tri) -> Tri {
    if matches!(value, Tri::True) {
        Tri::True
    } else if matches!(previous, Tri::False) || matches!(value, Tri::False) {
        Tri::False
    } else {
        Tri::Pruned
    }
}

fn resize_map<T: Clone>(
    values: &mut Vec<T>,
    len: usize,
    fill: T,
    resource: KvIrSearchResource,
) -> Result<(), KvIrSearchFailure> {
    if values.len() < len {
        values
            .try_reserve(len - values.len())
            .map_err(|_| allocation(resource, len - values.len()))?;
        values.resize(len, fill.clone());
    }
    values[..len].fill(fill);
    Ok(())
}

const fn schema_type_mask(node_type: KvIrNodeType) -> u16 {
    match node_type {
        KvIrNodeType::Integer => TYPE_INTEGER,
        KvIrNodeType::Float => TYPE_FLOAT,
        KvIrNodeType::Boolean => TYPE_BOOLEAN,
        KvIrNodeType::String => TYPE_VAR_STRING | TYPE_CLP_STRING,
        KvIrNodeType::UnstructuredArray => TYPE_ARRAY,
        KvIrNodeType::Object => TYPE_NULL,
    }
}

fn path_matches(path: &[usize], tree: &SchemaTree, pattern: &[PathComponent]) -> bool {
    let mut path_index = 0;
    let mut pattern_index = 0;
    let mut star_pattern = None;
    let mut star_path = 0;
    while path_index < path.len() {
        if let Some(component) = pattern.get(pattern_index) {
            if component.is_wildcard() {
                star_pattern = Some(pattern_index);
                star_path = path_index;
                pattern_index += 1;
                continue;
            }
            if component.value().as_bytes() == tree.nodes[path[path_index]].key {
                path_index += 1;
                pattern_index += 1;
                continue;
            }
        }
        let Some(star) = star_pattern else {
            return false;
        };
        star_path += 1;
        path_index = star_path;
        pattern_index = star + 1;
    }
    while pattern
        .get(pattern_index)
        .is_some_and(PathComponent::is_wildcard)
    {
        pattern_index += 1;
    }
    pattern_index == pattern.len()
}

fn evaluate_pair(
    leaf: &CompiledLeaf<'_>,
    pair: KvIrPair<'_>,
    node_type: KvIrNodeType,
    ignore_case: bool,
    reconstructed: &mut Vec<u8>,
    reconstructed_limit: u64,
) -> Result<Tri, KvIrSearchFailure> {
    let runtime_type = match pair.value().kind() {
        KvIrValueKind::Integer(_) => TYPE_INTEGER,
        KvIrValueKind::Float { .. } => TYPE_FLOAT,
        KvIrValueKind::Boolean(_) => TYPE_BOOLEAN,
        KvIrValueKind::String(_) => TYPE_VAR_STRING,
        KvIrValueKind::EncodedText(_) if node_type == KvIrNodeType::UnstructuredArray => TYPE_ARRAY,
        KvIrValueKind::EncodedText(_) => TYPE_CLP_STRING,
        KvIrValueKind::Null => TYPE_NULL,
        KvIrValueKind::EmptyObject => 0,
    };
    if 0 == leaf.types & runtime_type {
        return Ok(Tri::Pruned);
    }
    if matches!(leaf.mode, LeafMode::Exists | LeafMode::NonNullExists) {
        return Ok(Tri::True);
    }

    let matched = match pair.value().kind() {
        KvIrValueKind::Integer(value) => integer_matches(value.value(), leaf),
        KvIrValueKind::Float { bits } => float_matches(f64::from_bits(bits), leaf),
        KvIrValueKind::Boolean(value) => {
            matches!(leaf.literal, Literal::Boolean(expected) if value == *expected)
        }
        KvIrValueKind::String(value) => {
            string_pattern(leaf).is_some_and(|pattern| wildcard_match(value, pattern, ignore_case))
        }
        KvIrValueKind::EncodedText(value) if node_type == KvIrNodeType::String => {
            let Some(pattern) = clp_pattern(leaf) else {
                return Ok(Tri::Pruned);
            };
            reconstruct_encoded_text(value, reconstructed, reconstructed_limit)
                .map_err(KvIrSearchFailure::EncodedText)?;
            wildcard_match(reconstructed, pattern, ignore_case)
        }
        KvIrValueKind::EncodedText(_) => false,
        KvIrValueKind::Null => matches!(leaf.literal, Literal::Null),
        KvIrValueKind::EmptyObject => return Ok(Tri::Pruned),
    };
    Ok(if matched { Tri::True } else { Tri::False })
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

fn integer_matches(value: i64, leaf: &CompiledLeaf<'_>) -> bool {
    if let Some(timestamp) = leaf.timestamp_nanoseconds {
        return compare(value, timestamp / 1_000_000, leaf.operator);
    }
    match numeric_literal(leaf.literal) {
        Some(NumericLiteral::Integer(operand)) => compare(value, operand, leaf.operator),
        Some(NumericLiteral::Float(operand)) => compare_i64_float(value, operand, leaf.operator),
        None => false,
    }
}

#[allow(clippy::cast_precision_loss)]
fn float_matches(value: f64, leaf: &CompiledLeaf<'_>) -> bool {
    let operand = if let Some(timestamp) = leaf.timestamp_nanoseconds {
        timestamp as f64 / 1_000_000_000.0
    } else {
        match numeric_literal(leaf.literal) {
            Some(NumericLiteral::Integer(value)) => value as f64,
            Some(NumericLiteral::Float(value)) => value,
            None => return false,
        }
    };
    compare(value, operand, leaf.operator)
}

fn string_pattern<'leaf>(leaf: &'leaf CompiledLeaf<'_>) -> Option<&'leaf str> {
    if !matches!(leaf.operator, ComparisonOperator::Equal) || matches!(leaf.mode, LeafMode::Exists)
    {
        return None;
    }
    match leaf.literal {
        Literal::Integer { source, .. } | Literal::Float { source, .. } => Some(source),
        Literal::Boolean(true) => Some("true"),
        Literal::Boolean(false) => Some("false"),
        Literal::Null => Some("null"),
        Literal::String(value) => Some(value.wildcard_pattern()),
        Literal::Timestamp(_) => None,
    }
}

fn clp_pattern<'leaf>(leaf: &'leaf CompiledLeaf<'_>) -> Option<&'leaf str> {
    let Literal::String(value) = leaf.literal else {
        return None;
    };
    let pattern = value.wildcard_pattern();
    (pattern.as_bytes().contains(&b' ') || value.has_wildcards()).then_some(pattern)
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
    // C++ first coerces the double operand to `value_int_t`. Its x86-64 conversion instruction
    // yields `i64::MIN` for a finite value outside the signed range; preserving that quirk matters
    // for predicates at and above positive 2^63.
    const LOWER: f64 = -9_223_372_036_854_775_808.0;
    const UPPER_EXCLUSIVE: f64 = 9_223_372_036_854_775_808.0;
    let rounded = match operator {
        ComparisonOperator::Equal => operand,
        ComparisonOperator::Less | ComparisonOperator::GreaterOrEqual => operand.ceil(),
        ComparisonOperator::LessOrEqual | ComparisonOperator::Greater => operand.floor(),
    };
    let integer_operand = if rounded < LOWER || rounded >= UPPER_EXCLUSIVE {
        i64::MIN
    } else {
        rounded as i64
    };
    if matches!(operator, ComparisonOperator::Equal) && operand != integer_operand as f64 {
        return false;
    }
    compare(value, integer_operand, operator)
}

/// Malformed encoded-text AST that cannot be reconstructed losslessly.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum KvIrEncodedTextError {
    EncodedVariableWidthMismatch,
    MissingEncodedVariable,
    MissingDictionaryVariable,
    ExtraEncodedVariable,
    ExtraDictionaryVariable,
    TrailingEscape,
    EncodedFloatDecimalPosition,
    EncodedFloatDigitsTooLarge,
    EncodedFloatDigitCount,
    Limit(KvIrSearchLimitViolation),
    AllocationFailed { requested_additional: usize },
    SizeOverflow,
}

impl Display for KvIrEncodedTextError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::EncodedVariableWidthMismatch => {
                formatter.write_str("encoded-text variable width mismatch")
            }
            Self::MissingEncodedVariable => {
                formatter.write_str("encoded-text placeholder has no encoded variable")
            }
            Self::MissingDictionaryVariable => {
                formatter.write_str("encoded-text placeholder has no dictionary variable")
            }
            Self::ExtraEncodedVariable => {
                formatter.write_str("encoded text has unused encoded variables")
            }
            Self::ExtraDictionaryVariable => {
                formatter.write_str("encoded text has unused dictionary variables")
            }
            Self::TrailingEscape => {
                formatter.write_str("encoded-text logtype has a trailing escape")
            }
            Self::EncodedFloatDecimalPosition => {
                formatter.write_str("encoded float decimal position exceeds its declared digits")
            }
            Self::EncodedFloatDigitsTooLarge => {
                formatter.write_str("encoded float digit field exceeds its domain")
            }
            Self::EncodedFloatDigitCount => {
                formatter.write_str("encoded float digit field exceeds its declared digit count")
            }
            Self::Limit(source) => Display::fmt(source, formatter),
            Self::AllocationFailed {
                requested_additional,
            } => write!(
                formatter,
                "failed to reserve {requested_additional} encoded-text bytes"
            ),
            Self::SizeOverflow => formatter.write_str("encoded-text output size overflow"),
        }
    }
}

impl Error for KvIrEncodedTextError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Limit(source) => Some(source),
            _ => None,
        }
    }
}

fn reconstruct_encoded_text(
    text: KvIrEncodedText<'_>,
    output: &mut Vec<u8>,
    limit: u64,
) -> Result<(), KvIrEncodedTextError> {
    output.clear();
    let mut encoded = text.encoded_variables();
    let mut dictionaries = text.dictionary_variables();
    let logtype = text.logtype();
    let mut position = 0;
    while position < logtype.len() {
        let byte = logtype[position];
        if !matches!(byte, b'\\' | 0x11..=0x13) {
            let start = position;
            position += 1;
            while position < logtype.len() && !matches!(logtype[position], b'\\' | 0x11..=0x13) {
                position += 1;
            }
            append_reconstructed(output, &logtype[start..position], limit)?;
            continue;
        }
        if byte == b'\\' {
            position = position
                .checked_add(1)
                .ok_or(KvIrEncodedTextError::SizeOverflow)?;
            let escaped = logtype
                .get(position)
                .copied()
                .ok_or(KvIrEncodedTextError::TrailingEscape)?;
            append_reconstructed(output, &[escaped], limit)?;
        } else if byte == 0x12 {
            let value = dictionaries
                .next()
                .ok_or(KvIrEncodedTextError::MissingDictionaryVariable)?;
            append_reconstructed(output, value, limit)?;
        } else {
            let value = encoded
                .next()
                .ok_or(KvIrEncodedTextError::MissingEncodedVariable)?;
            if byte == 0x11 {
                append_encoded_integer(output, text.encoding(), value, limit)?;
            } else {
                append_encoded_float(output, text.encoding(), value, limit)?;
            }
        }
        position = position
            .checked_add(1)
            .ok_or(KvIrEncodedTextError::SizeOverflow)?;
    }
    if encoded.next().is_some() {
        return Err(KvIrEncodedTextError::ExtraEncodedVariable);
    }
    if dictionaries.next().is_some() {
        return Err(KvIrEncodedTextError::ExtraDictionaryVariable);
    }
    Ok(())
}

fn append_reconstructed(
    output: &mut Vec<u8>,
    bytes: &[u8],
    limit: u64,
) -> Result<(), KvIrEncodedTextError> {
    let end = output
        .len()
        .checked_add(bytes.len())
        .ok_or(KvIrEncodedTextError::SizeOverflow)?;
    let actual = u64::try_from(end).map_err(|_| KvIrEncodedTextError::SizeOverflow)?;
    if actual > limit {
        return Err(KvIrEncodedTextError::Limit(KvIrSearchLimitViolation::new(
            KvIrSearchLimitResource::ReconstructedValueBytes,
            actual,
            limit,
        )));
    }
    output
        .try_reserve(bytes.len())
        .map_err(|_| KvIrEncodedTextError::AllocationFailed {
            requested_additional: bytes.len(),
        })?;
    output.extend_from_slice(bytes);
    Ok(())
}

fn append_encoded_integer(
    output: &mut Vec<u8>,
    encoding: KvIrEncoding,
    value: KvIrEncodedVariable,
    limit: u64,
) -> Result<(), KvIrEncodedTextError> {
    let integer = match (encoding, value) {
        (KvIrEncoding::FourByte, KvIrEncodedVariable::FourByte(value)) => i64::from(value),
        (KvIrEncoding::EightByte, KvIrEncodedVariable::EightByte(value)) => value,
        _ => return Err(KvIrEncodedTextError::EncodedVariableWidthMismatch),
    };
    let mut buffer = itoa::Buffer::new();
    append_reconstructed(output, buffer.format(integer).as_bytes(), limit)
}

fn append_encoded_float(
    output: &mut Vec<u8>,
    encoding: KvIrEncoding,
    value: KvIrEncodedVariable,
    limit: u64,
) -> Result<(), KvIrEncodedTextError> {
    let properties = decode_float_properties(encoding, value)?;
    let sign_bytes = usize::from(properties.negative);
    let output_len = usize::from(properties.digit_count)
        .checked_add(1)
        .and_then(|value| value.checked_add(sign_bytes))
        .ok_or(KvIrEncodedTextError::SizeOverflow)?;
    let output_start = output.len();
    append_repeated_zeroes(output, output_len, limit)?;
    if properties.negative {
        output[output_start] = b'-';
    }
    let decimal_index = output_start
        .checked_add(sign_bytes)
        .and_then(|value| value.checked_add(usize::from(properties.digit_count)))
        .and_then(|value| value.checked_sub(usize::from(properties.decimal_position)))
        .ok_or(KvIrEncodedTextError::SizeOverflow)?;
    output[decimal_index] = b'.';
    let digit_floor = output_start
        .checked_add(sign_bytes)
        .ok_or(KvIrEncodedTextError::SizeOverflow)?;
    let mut cursor = output.len();
    let mut digits = properties.digits;
    while digits != 0 {
        cursor = cursor
            .checked_sub(1)
            .ok_or(KvIrEncodedTextError::EncodedFloatDigitCount)?;
        if cursor == decimal_index {
            cursor = cursor
                .checked_sub(1)
                .ok_or(KvIrEncodedTextError::EncodedFloatDigitCount)?;
        }
        if cursor < digit_floor {
            return Err(KvIrEncodedTextError::EncodedFloatDigitCount);
        }
        output[cursor] = b'0' + u8::try_from(digits % 10).expect("decimal digit fits u8");
        digits /= 10;
    }
    Ok(())
}

fn append_repeated_zeroes(
    output: &mut Vec<u8>,
    count: usize,
    limit: u64,
) -> Result<(), KvIrEncodedTextError> {
    let end = output
        .len()
        .checked_add(count)
        .ok_or(KvIrEncodedTextError::SizeOverflow)?;
    let actual = u64::try_from(end).map_err(|_| KvIrEncodedTextError::SizeOverflow)?;
    if actual > limit {
        return Err(KvIrEncodedTextError::Limit(KvIrSearchLimitViolation::new(
            KvIrSearchLimitResource::ReconstructedValueBytes,
            actual,
            limit,
        )));
    }
    output
        .try_reserve(count)
        .map_err(|_| KvIrEncodedTextError::AllocationFailed {
            requested_additional: count,
        })?;
    output.resize(end, b'0');
    Ok(())
}

#[derive(Clone, Copy, Debug)]
struct FloatProperties {
    negative: bool,
    digits: u64,
    digit_count: u8,
    decimal_position: u8,
}

fn decode_float_properties(
    encoding: KvIrEncoding,
    value: KvIrEncodedVariable,
) -> Result<FloatProperties, KvIrEncodedTextError> {
    let properties = match (encoding, value) {
        (KvIrEncoding::FourByte, KvIrEncodedVariable::FourByte(value)) => {
            let encoded = u32::from_ne_bytes(value.to_ne_bytes());
            FloatProperties {
                negative: encoded >> 31 != 0,
                digits: u64::from((encoded >> 6) & ((1_u32 << 25) - 1)),
                digit_count: u8::try_from(((encoded >> 3) & 0x07) + 1)
                    .expect("three-bit digit count fits u8"),
                decimal_position: u8::try_from((encoded & 0x07) + 1)
                    .expect("three-bit decimal position fits u8"),
            }
        }
        (KvIrEncoding::EightByte, KvIrEncodedVariable::EightByte(value)) => {
            let encoded = u64::from_ne_bytes(value.to_ne_bytes());
            let digits = (encoded >> 8) & ((1_u64 << 54) - 1);
            if digits > 9_999_999_999_999_999 {
                return Err(KvIrEncodedTextError::EncodedFloatDigitsTooLarge);
            }
            FloatProperties {
                negative: encoded >> 63 != 0,
                digits,
                digit_count: u8::try_from(((encoded >> 4) & 0x0f) + 1)
                    .expect("four-bit digit count fits u8"),
                decimal_position: u8::try_from((encoded & 0x0f) + 1)
                    .expect("four-bit decimal position fits u8"),
            }
        }
        _ => return Err(KvIrEncodedTextError::EncodedVariableWidthMismatch),
    };
    if properties.decimal_position > properties.digit_count {
        return Err(KvIrEncodedTextError::EncodedFloatDecimalPosition);
    }
    Ok(properties)
}

/// JSONL formatting limits for one direct KV-IR match.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KvIrJsonlLimits {
    record_bytes: u64,
    reconstructed_value_bytes: u64,
    nesting_depth: u64,
}

impl KvIrJsonlLimits {
    pub const DEFAULT: Self = Self {
        record_bytes: 64 * MEBIBYTE,
        reconstructed_value_bytes: 16 * MEBIBYTE,
        nesting_depth: 256,
    };

    #[must_use]
    pub const fn new() -> Self {
        Self::DEFAULT
    }

    #[must_use]
    pub const fn with_max_record_bytes(mut self, value: u64) -> Self {
        self.record_bytes = value;
        self
    }

    #[must_use]
    pub const fn with_max_reconstructed_value_bytes(mut self, value: u64) -> Self {
        self.reconstructed_value_bytes = value;
        self
    }

    #[must_use]
    pub const fn with_max_nesting_depth(mut self, value: u64) -> Self {
        self.nesting_depth = value;
        self
    }

    #[must_use]
    pub const fn max_record_bytes(self) -> u64 {
        self.record_bytes
    }

    #[must_use]
    pub const fn max_reconstructed_value_bytes(self) -> u64 {
        self.reconstructed_value_bytes
    }

    #[must_use]
    pub const fn max_nesting_depth(self) -> u64 {
        self.nesting_depth
    }
}

impl Default for KvIrJsonlLimits {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Direct KV-IR JSONL formatting configuration.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct KvIrJsonlOptions {
    limits: KvIrJsonlLimits,
    byte_policy: JsonBytePolicy,
}

impl KvIrJsonlOptions {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            limits: KvIrJsonlLimits::DEFAULT,
            byte_policy: JsonBytePolicy::StrictUtf8,
        }
    }

    #[must_use]
    pub const fn with_limits(mut self, limits: KvIrJsonlLimits) -> Self {
        self.limits = limits;
        self
    }

    #[must_use]
    pub const fn with_byte_policy(mut self, byte_policy: JsonBytePolicy) -> Self {
        self.byte_policy = byte_policy;
        self
    }

    #[must_use]
    pub const fn limits(self) -> KvIrJsonlLimits {
        self.limits
    }

    #[must_use]
    pub const fn byte_policy(self) -> JsonBytePolicy {
        self.byte_policy
    }
}

/// Direct JSONL adapter failure.
#[derive(Debug)]
#[non_exhaustive]
pub enum KvIrJsonlError {
    Output(io::Error),
    Escape(JsonEscapeError),
    Float(NlohmannFloatError),
    EncodedText(KvIrEncodedTextError),
    InvalidArrayJson,
    Limit {
        resource: KvIrJsonlLimitResource,
        actual: u64,
        limit: u64,
    },
    InvalidSelection,
    AllocationFailed {
        resource: KvIrJsonlResource,
        requested_additional: usize,
    },
    SizeOverflow,
}

impl Display for KvIrJsonlError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Output(source) => write!(formatter, "KV-IR JSONL output failed: {source}"),
            Self::Escape(source) => Display::fmt(source, formatter),
            Self::Float(source) => Display::fmt(source, formatter),
            Self::EncodedText(source) => Display::fmt(source, formatter),
            Self::InvalidArrayJson => {
                formatter.write_str("KV-IR unstructured array is not canonicalizable JSON")
            }
            Self::Limit {
                resource,
                actual,
                limit,
            } => write!(formatter, "{resource} value {actual} exceeds limit {limit}"),
            Self::InvalidSelection => {
                formatter.write_str("KV-IR match selection is inconsistent with its schema")
            }
            Self::AllocationFailed {
                resource,
                requested_additional,
            } => write!(
                formatter,
                "failed to reserve {requested_additional} additional units for {resource}"
            ),
            Self::SizeOverflow => formatter.write_str("KV-IR JSONL size counter overflow"),
        }
    }
}

impl Error for KvIrJsonlError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Output(source) => Some(source),
            Self::Escape(source) => Some(source),
            Self::Float(source) => Some(source),
            Self::EncodedText(source) => Some(source),
            _ => None,
        }
    }
}

/// JSONL-owned allocation named by an error.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum KvIrJsonlResource {
    Record,
    TraversalStack,
    CanonicalArray,
}

impl Display for KvIrJsonlResource {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Record => "KV-IR JSONL record",
            Self::TraversalStack => "KV-IR JSON traversal stack",
            Self::CanonicalArray => "KV-IR canonical array scratch",
        })
    }
}

/// JSONL limit domain.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum KvIrJsonlLimitResource {
    RecordBytes,
    NestingDepth,
    CanonicalArrayValues,
    CanonicalArrayScalarBytes,
}

impl Display for KvIrJsonlLimitResource {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::RecordBytes => "KV-IR JSONL record bytes",
            Self::NestingDepth => "KV-IR JSONL nesting depth",
            Self::CanonicalArrayValues => "KV-IR canonical array JSON values",
            Self::CanonicalArrayScalarBytes => "KV-IR canonical array scalar bytes",
        })
    }
}

#[derive(Clone, Copy)]
struct JsonFrame {
    node_id: usize,
    child_index: usize,
    wrote_child: bool,
}

/// Exact C++-compatible JSONL match sink.
pub struct KvIrJsonlMatchSink<W> {
    output: W,
    options: KvIrJsonlOptions,
    record: Vec<u8>,
    reconstructed: Vec<u8>,
    traversal: Vec<JsonFrame>,
    canonical_array: CanonicalJsonScratch,
}

impl<W> KvIrJsonlMatchSink<W> {
    #[must_use]
    pub const fn new(output: W, options: KvIrJsonlOptions) -> Self {
        Self {
            output,
            options,
            record: Vec::new(),
            reconstructed: Vec::new(),
            traversal: Vec::new(),
            canonical_array: CanonicalJsonScratch::new(),
        }
    }

    #[must_use]
    pub const fn options(&self) -> KvIrJsonlOptions {
        self.options
    }

    #[must_use]
    pub const fn output(&self) -> &W {
        &self.output
    }

    #[must_use]
    pub const fn output_mut(&mut self) -> &mut W {
        &mut self.output
    }

    #[must_use]
    pub fn into_inner(self) -> W {
        self.output
    }
}

impl<W: Write> KvIrJsonlMatchSink<W> {
    fn format_record(&mut self, event: KvIrMatchedEvent<'_>) -> Result<(), KvIrJsonlError> {
        self.record.clear();
        self.append_literal(b"{\"auto_generated_kv_pairs\":")?;
        self.append_namespace(event, KvIrNamespace::AutoGenerated)?;
        self.append_literal(b",\"user_generated_kv_pairs\":")?;
        self.append_namespace(event, KvIrNamespace::UserGenerated)?;
        self.append_literal(b"}\n")
    }

    fn append_namespace(
        &mut self,
        event: KvIrMatchedEvent<'_>,
        namespace: KvIrNamespace,
    ) -> Result<(), KvIrJsonlError> {
        self.append_literal(b"{")?;
        self.traversal.clear();
        self.push_frame(JsonFrame {
            node_id: 0,
            child_index: 0,
            wrote_child: false,
        })?;
        let schema = event.schema(namespace);
        let included = match namespace {
            KvIrNamespace::AutoGenerated => event.included_auto,
            KvIrNamespace::UserGenerated => event.included_user,
        };
        while !self.traversal.is_empty() {
            let frame_index = self.traversal.len() - 1;
            let node_id = self.traversal[frame_index].node_id;
            let children = &schema[node_id].children;
            let mut child = None;
            while self.traversal[frame_index].child_index < children.len() {
                let index = self.traversal[frame_index].child_index;
                self.traversal[frame_index].child_index += 1;
                let candidate = children[index];
                if included.get(candidate).copied().unwrap_or(false) {
                    child = Some(candidate);
                    break;
                }
            }
            let Some(child) = child else {
                self.traversal.pop();
                self.append_literal(b"}")?;
                continue;
            };
            if self.traversal[frame_index].wrote_child {
                self.append_literal(b",")?;
            }
            self.traversal[frame_index].wrote_child = true;
            self.append_key(&schema[child].key)?;
            if let Some(pair) = event.pair(namespace, child) {
                let outer_depth = self.traversal.len().saturating_sub(1);
                self.append_value(pair, schema[child].node_type, outer_depth)?;
            } else {
                if schema[child].node_type != KvIrNodeType::Object {
                    return Err(KvIrJsonlError::InvalidSelection);
                }
                self.append_literal(b"{")?;
                self.push_frame(JsonFrame {
                    node_id: child,
                    child_index: 0,
                    wrote_child: false,
                })?;
            }
        }
        Ok(())
    }

    fn append_value(
        &mut self,
        pair: KvIrPair<'_>,
        node_type: KvIrNodeType,
        outer_depth: usize,
    ) -> Result<(), KvIrJsonlError> {
        match pair.value().kind() {
            KvIrValueKind::Integer(value) => {
                let mut buffer = itoa::Buffer::new();
                self.append_literal(buffer.format(value.value()).as_bytes())
            }
            KvIrValueKind::Float { bits } => {
                let value = f64::from_bits(bits);
                if !value.is_finite() {
                    return self.append_literal(b"null");
                }
                let formatted = format_nlohmann_float(value).map_err(KvIrJsonlError::Float)?;
                self.append_literal(formatted.as_str().as_bytes())
            }
            KvIrValueKind::Boolean(true) => self.append_literal(b"true"),
            KvIrValueKind::Boolean(false) => self.append_literal(b"false"),
            KvIrValueKind::String(value) => self.append_string(value),
            KvIrValueKind::EncodedText(value) => {
                let mut reconstructed = std::mem::take(&mut self.reconstructed);
                let reconstruction = reconstruct_encoded_text(
                    value,
                    &mut reconstructed,
                    self.options.limits.reconstructed_value_bytes,
                )
                .map_err(KvIrJsonlError::EncodedText);
                let result = reconstruction.and_then(|()| {
                    if node_type == KvIrNodeType::UnstructuredArray {
                        let remaining = self.remaining_record_bytes()?;
                        let outer_depth =
                            u64::try_from(outer_depth).map_err(|_| KvIrJsonlError::SizeOverflow)?;
                        let nesting_depth = self
                            .options
                            .limits
                            .nesting_depth
                            .checked_sub(outer_depth)
                            .ok_or(KvIrJsonlError::Limit {
                                resource: KvIrJsonlLimitResource::NestingDepth,
                                actual: outer_depth,
                                limit: self.options.limits.nesting_depth,
                            })?;
                        self.canonical_array
                            .append_to(
                                &reconstructed,
                                &mut self.record,
                                CanonicalJsonLimits {
                                    input_bytes: self.options.limits.reconstructed_value_bytes,
                                    output_bytes: remaining,
                                    nesting_depth,
                                },
                            )
                            .map_err(map_canonical_json_error)
                    } else {
                        self.append_string(&reconstructed)
                    }
                });
                self.reconstructed = reconstructed;
                result
            }
            KvIrValueKind::Null => self.append_literal(b"null"),
            KvIrValueKind::EmptyObject => self.append_literal(b"{}"),
        }
    }

    fn append_key(&mut self, value: &[u8]) -> Result<(), KvIrJsonlError> {
        let remaining = self.remaining_record_bytes()?;
        append_json_key_bytes(
            value,
            &mut self.record,
            self.options.byte_policy,
            JsonEscapeLimits::new(value.len(), remaining),
        )
        .map_err(KvIrJsonlError::Escape)
    }

    fn append_string(&mut self, value: &[u8]) -> Result<(), KvIrJsonlError> {
        let remaining = self.remaining_record_bytes()?;
        append_json_string_bytes(
            value,
            &mut self.record,
            self.options.byte_policy,
            JsonEscapeLimits::new(value.len(), remaining),
        )
        .map_err(KvIrJsonlError::Escape)
    }

    fn append_literal(&mut self, value: &[u8]) -> Result<(), KvIrJsonlError> {
        let end = self
            .record
            .len()
            .checked_add(value.len())
            .ok_or(KvIrJsonlError::SizeOverflow)?;
        let actual = u64::try_from(end).map_err(|_| KvIrJsonlError::SizeOverflow)?;
        if actual > self.options.limits.record_bytes {
            return Err(KvIrJsonlError::Limit {
                resource: KvIrJsonlLimitResource::RecordBytes,
                actual,
                limit: self.options.limits.record_bytes,
            });
        }
        self.record
            .try_reserve(value.len())
            .map_err(|_| KvIrJsonlError::AllocationFailed {
                resource: KvIrJsonlResource::Record,
                requested_additional: value.len(),
            })?;
        self.record.extend_from_slice(value);
        Ok(())
    }

    fn remaining_record_bytes(&self) -> Result<usize, KvIrJsonlError> {
        let used = u64::try_from(self.record.len()).map_err(|_| KvIrJsonlError::SizeOverflow)?;
        let remaining =
            self.options
                .limits
                .record_bytes
                .checked_sub(used)
                .ok_or(KvIrJsonlError::Limit {
                    resource: KvIrJsonlLimitResource::RecordBytes,
                    actual: used,
                    limit: self.options.limits.record_bytes,
                })?;
        usize::try_from(remaining).map_err(|_| KvIrJsonlError::SizeOverflow)
    }

    fn push_frame(&mut self, frame: JsonFrame) -> Result<(), KvIrJsonlError> {
        let actual = self
            .traversal
            .len()
            .checked_add(1)
            .ok_or(KvIrJsonlError::SizeOverflow)?;
        let actual_u64 = u64::try_from(actual).map_err(|_| KvIrJsonlError::SizeOverflow)?;
        if actual_u64 > self.options.limits.nesting_depth.saturating_add(1) {
            return Err(KvIrJsonlError::Limit {
                resource: KvIrJsonlLimitResource::NestingDepth,
                actual: actual_u64.saturating_sub(1),
                limit: self.options.limits.nesting_depth,
            });
        }
        self.traversal
            .try_reserve(1)
            .map_err(|_| KvIrJsonlError::AllocationFailed {
                resource: KvIrJsonlResource::TraversalStack,
                requested_additional: 1,
            })?;
        self.traversal.push(frame);
        Ok(())
    }
}

const fn map_canonical_json_error(source: CanonicalJsonError) -> KvIrJsonlError {
    match source {
        CanonicalJsonError::Invalid(NdjsonInvalidRecordKind::Syntax(_))
        | CanonicalJsonError::NumberOutOfRange => KvIrJsonlError::InvalidArrayJson,
        CanonicalJsonError::Invalid(NdjsonInvalidRecordKind::Limit(source)) => {
            let resource = match source.resource() {
                NdjsonLimitResource::RecordBytes => KvIrJsonlLimitResource::RecordBytes,
                NdjsonLimitResource::NestingDepth => KvIrJsonlLimitResource::NestingDepth,
                NdjsonLimitResource::Values => KvIrJsonlLimitResource::CanonicalArrayValues,
                NdjsonLimitResource::ScalarTokenBytes => {
                    KvIrJsonlLimitResource::CanonicalArrayScalarBytes
                }
            };
            KvIrJsonlError::Limit {
                resource,
                actual: source.actual(),
                limit: source.limit(),
            }
        }
        CanonicalJsonError::Escape(source) => KvIrJsonlError::Escape(source),
        CanonicalJsonError::Float(source) => KvIrJsonlError::Float(source),
        CanonicalJsonError::AllocationFailed {
            resource,
            requested_additional,
        } => KvIrJsonlError::AllocationFailed {
            resource: if matches!(resource, CanonicalJsonResource::Destination) {
                KvIrJsonlResource::Record
            } else {
                KvIrJsonlResource::CanonicalArray
            },
            requested_additional,
        },
        CanonicalJsonError::SizeOverflow => KvIrJsonlError::SizeOverflow,
        CanonicalJsonError::InvalidEventSequence => KvIrJsonlError::InvalidSelection,
    }
}

impl<W: Write> KvIrMatchSink for KvIrJsonlMatchSink<W> {
    type Error = KvIrJsonlError;

    fn write_match(&mut self, event: KvIrMatchedEvent<'_>) -> Result<(), Self::Error> {
        self.format_record(event)?;
        self.output
            .write_all(&self.record)
            .map_err(KvIrJsonlError::Output)
    }
}

enum FirstStreamError<E> {
    Search(E),
    Finished,
}

struct FirstStreamSink<'searcher, 'query, S> {
    searcher: &'searcher mut KvIrSearchSink<'query, S>,
}

impl<S: KvIrMatchSink> KvIrSink for FirstStreamSink<'_, '_, S> {
    type Error = FirstStreamError<KvIrSearchError<S::Error>>;

    fn write_item(&mut self, item: KvIrItem<'_>) -> Result<(), Self::Error> {
        let finished = matches!(item, KvIrItem::StreamEnd(_));
        self.searcher
            .write_item(item)
            .map_err(FirstStreamError::Search)?;
        if finished {
            Err(FirstStreamError::Finished)
        } else {
            Ok(())
        }
    }
}

/// Searches exactly the first decoded stream and stops when its explicit end marker is decoded.
///
/// This is the direct-search compatibility wrapper. Use [`KvIrReader::read_to_end`] with a
/// [`KvIrSearchSink`] directly when all concatenated streams should be searched. The reader's
/// fixed input buffer may already have fetched bytes from a following stream;
/// [`KvIrReader::into_inner`] discards those unread buffered bytes, so this wrapper does not
/// promise a resumable input position.
///
/// # Errors
///
/// Preserves decoder failures and search/match-sink context. A missing explicit end marker is a
/// decoder truncation error even if earlier complete events were already emitted.
pub fn search_first_kv_ir_stream<R: Read, S: KvIrMatchSink>(
    reader: &mut KvIrReader<R>,
    searcher: &mut KvIrSearchSink<'_, S>,
) -> Result<KvIrSearchStats, KvIrReadError<KvIrSearchError<S::Error>>> {
    let mut first = FirstStreamSink { searcher };
    match reader.read_to_end(&mut first) {
        Err(KvIrReadError::Sink {
            source: FirstStreamError::Finished,
            ..
        })
        | Ok(_) => Ok(first.searcher.stats()),
        Err(KvIrReadError::Sink {
            stream_index,
            unit_index,
            source: FirstStreamError::Search(source),
        }) => Err(KvIrReadError::Sink {
            stream_index,
            unit_index,
            source,
        }),
        Err(KvIrReadError::Reader(source)) => Err(KvIrReadError::Reader(source)),
    }
}

fn check_limit(
    resource: KvIrSearchLimitResource,
    actual: impl TryInto<u64>,
    limit: u64,
) -> Result<(), KvIrSearchFailure> {
    let actual = actual
        .try_into()
        .map_err(|_| KvIrSearchFailure::SizeOverflow)?;
    if actual > limit {
        Err(KvIrSearchFailure::Limit(KvIrSearchLimitViolation::new(
            resource, actual, limit,
        )))
    } else {
        Ok(())
    }
}

const fn allocation(
    resource: KvIrSearchResource,
    requested_additional: usize,
) -> KvIrSearchFailure {
    KvIrSearchFailure::AllocationFailed {
        resource,
        requested_additional,
    }
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;
    use std::io::Cursor;

    use super::KvIrJsonlLimits;
    use super::KvIrJsonlMatchSink;
    use super::KvIrJsonlOptions;
    use super::KvIrMatchSink;
    use super::KvIrMatchedEvent;
    use super::KvIrSearchFailure;
    use super::KvIrSearchLimitResource;
    use super::KvIrSearchLimits;
    use super::KvIrSearchOptions;
    use super::KvIrSearchSink;
    use super::is_cpp_tolerated_kv_ir_truncation;
    use super::is_kv_ir_search_candidate;
    use super::search_first_kv_ir_stream;
    use crate::ingest::KvIrErrorKind;
    use crate::ingest::KvIrOptions;
    use crate::ingest::KvIrReadError;
    use crate::ingest::KvIrReader;
    use crate::search::KqlLimits;
    use crate::search::parse_kql;

    const FOUR_BYTE_ORACLE_HEX: &str =
        include_str!("../../tests/fixtures/kv-ir-v0.1.0-four-byte-cpp.hex");
    const EIGHT_BYTE_ORACLE_HEX: &str =
        include_str!("../../tests/fixtures/kv-ir-v0.1.0-eight-byte-cpp.hex");
    const NESTED_ORACLE_HEX: &str =
        include_str!("../../tests/fixtures/kv-ir-search-v0.1.0-nested-cpp.hex");
    const CANONICAL_OUTPUT: &[u8] = concat!(
        "{\"auto_generated_kv_pairs\":{\"level\":\"info\",\"seq\":7},",
        "\"user_generated_kv_pairs\":{\"empty\":{},\"message\":\"task 42 done\",",
        "\"none\":null,\"ok\":true,\"ratio\":1.25}}\n"
    )
    .as_bytes();
    const NESTED_FIRST: &[u8] = concat!(
        "{\"auto_generated_kv_pairs\":{},\"user_generated_kv_pairs\":{\"a\":1,",
        "\"empty\":{},\"none\":null,\"obj\":{\"a\":true,\"b\":\"bee\"},\"z\":9}}\n"
    )
    .as_bytes();
    const NESTED_SECOND: &[u8] = concat!(
        "{\"auto_generated_kv_pairs\":{},\"user_generated_kv_pairs\":",
        "{\"a\":2,\"obj\":{\"b\":\"no\"},\"z\":0}}\n"
    )
    .as_bytes();

    fn decode_hex(source: &str) -> Vec<u8> {
        let digits: Vec<u8> = source.bytes().filter(u8::is_ascii_hexdigit).collect();
        let (pairs, remainder) = digits.as_chunks::<2>();
        assert_eq!(&[] as &[u8], remainder);
        pairs
            .iter()
            .map(|pair| (hex_nibble(pair[0]) << 4) | hex_nibble(pair[1]))
            .collect()
    }

    const fn hex_nibble(value: u8) -> u8 {
        match value {
            b'0'..=b'9' => value - b'0',
            b'a'..=b'f' => value - b'a' + 10,
            b'A'..=b'F' => value - b'A' + 10,
            _ => panic!("non-hex fixture byte"),
        }
    }

    fn search(bytes: &[u8], query: &str) -> Vec<u8> {
        search_with_options(bytes, query, KvIrSearchOptions::default()).expect("direct search")
    }

    fn search_with_options(
        bytes: &[u8],
        query: &str,
        options: KvIrSearchOptions,
    ) -> Result<Vec<u8>, String> {
        let query = parse_kql(query, KqlLimits::default()).map_err(|error| error.to_string())?;
        let output = KvIrJsonlMatchSink::new(Vec::new(), KvIrJsonlOptions::default());
        let mut searcher =
            KvIrSearchSink::new(&query, output, options).map_err(|error| error.to_string())?;
        let mut reader = KvIrReader::new(Cursor::new(bytes), KvIrOptions::default());
        search_first_kv_ir_stream(&mut reader, &mut searcher).map_err(|error| error.to_string())?;
        Ok(searcher.into_inner().into_inner())
    }

    fn count_matches(bytes: &[u8], query: &str) -> usize {
        let query = parse_kql(query, KqlLimits::default()).expect("query");
        let mut matches = CountMatches(0);
        let mut searcher = KvIrSearchSink::new(&query, &mut matches, KvIrSearchOptions::default())
            .expect("compile query");
        let mut reader = KvIrReader::new(Cursor::new(bytes), KvIrOptions::default());
        search_first_kv_ir_stream(&mut reader, &mut searcher).expect("search stream");
        matches.0
    }

    #[test]
    fn exact_cpp_four_and_eight_byte_output() {
        for source in [FOUR_BYTE_ORACLE_HEX, EIGHT_BYTE_ORACLE_HEX] {
            assert_eq!(
                CANONICAL_OUTPUT,
                search(&decode_hex(source), "*:*").as_slice()
            );
        }
    }

    #[test]
    fn exact_nested_output_is_lexical_and_stream_ordered() {
        let bytes = decode_hex(NESTED_ORACLE_HEX);
        let mut expected = NESTED_FIRST.to_vec();
        expected.extend_from_slice(NESTED_SECOND);
        assert_eq!(expected, search(&bytes, "*:*").as_slice());
        assert_eq!(NESTED_FIRST, search(&bytes, "z > 0").as_slice());
        assert_eq!(NESTED_FIRST, search(&bytes, "*.b:bee").as_slice());
        assert_eq!(NESTED_SECOND, search(&bytes, "*.b:no").as_slice());
    }

    #[test]
    fn cpp_query_tristate_namespace_wildcard_and_list_semantics() {
        let bytes = decode_hex(NESTED_ORACLE_HEX);
        let mut both = NESTED_FIRST.to_vec();
        both.extend_from_slice(NESTED_SECOND);
        let cases: &[(&str, &[u8])] = &[
            ("obj.a:true", NESTED_FIRST),
            ("*.a:1", NESTED_FIRST),
            ("missing:*", b""),
            ("NOT missing:*", b""),
            ("NOT none:null", b""),
            ("NOT z:null", &both),
            ("a:(1 2)", &both),
            ("a:(OR 1 2)", &both),
            ("a:(AND 1 2)", b""),
            ("a:(NOT 1 3)", NESTED_SECOND),
            ("a:()", b""),
            ("a:(AND)", &both),
            ("a:(NOT)", &both),
            ("!*:bee", NESTED_FIRST),
            ("@*:bee", NESTED_FIRST),
            ("NOT (missing:* AND z:999)", b""),
            ("NOT (missing:* OR z:999)", &both),
        ];
        for &(query, expected) in cases {
            assert_eq!(expected, search(&bytes, query).as_slice(), "{query}");
        }
    }

    #[test]
    fn string_case_null_empty_and_typed_comparisons_match_cpp() {
        let bytes = decode_hex(FOUR_BYTE_ORACLE_HEX);
        let misses = [
            "level:info",
            "seq:7",
            "empty:*",
            "empty:null",
            "missing:*",
            "NOT missing:*",
            "ratio > 2",
            "message:TASK*",
        ];
        for query in misses {
            assert!(search(&bytes, query).is_empty(), "{query}");
        }
        let matches = [
            "@level:info",
            "@seq:7",
            "message:task*",
            "ratio > 1",
            "ok:true",
            "none:null",
            "none:*",
            "*:7",
            "#*:task*",
        ];
        for query in matches {
            assert_eq!(
                CANONICAL_OUTPUT,
                search(&bytes, query).as_slice(),
                "{query}"
            );
        }
        assert_eq!(
            CANONICAL_OUTPUT,
            search_with_options(
                &bytes,
                "message:TASK*",
                KvIrSearchOptions::default().with_ignore_case(true),
            )
            .expect("case-insensitive search")
            .as_slice()
        );
    }

    #[test]
    fn timestamp_integer_precision_and_numeric_boundaries_match_cpp() {
        let timestamps = integer_stream(&[-1, 0, 1]);
        let timestamp_cases = [
            (r#"n:timestamp("-1", "\N")"#, 1),
            (r#"n <= timestamp("-1", "\N")"#, 2),
            (r#"n > timestamp("-1", "\N")"#, 1),
            (r#"n:timestamp("-999999", "\N")"#, 1),
            (r#"n:timestamp("-1000000", "\N")"#, 1),
        ];
        for (query, expected) in timestamp_cases {
            assert_eq!(expected, count_matches(&timestamps, query), "{query}");
        }

        let integers = integer_stream(&[
            9_007_199_254_740_991,
            9_007_199_254_740_992,
            9_007_199_254_740_993,
            i64::MAX,
            i64::MIN,
            -9_007_199_254_740_993,
        ]);
        let numeric_cases = [
            ("n:9007199254740991.0", 1),
            ("n:9007199254740992.0", 1),
            ("n:9007199254740993.0", 1),
            ("n > 9007199254740992.0", 2),
            ("n < 9007199254740993.0", 3),
            ("n:9223372036854775807.0", 0),
            ("n:9223372036854775808.0", 0),
            ("n < 9223372036854775808.0", 0),
            ("n >= 9223372036854775808.0", 6),
            ("n:-9223372036854775808.0", 1),
            ("n > -9223372036854775809.0", 5),
        ];
        for (query, expected) in numeric_cases {
            assert_eq!(expected, count_matches(&integers, query), "{query}");
        }
    }

    #[test]
    fn exact_float_json_uses_nlohmann_spelling() {
        let bits = [
            0x8000_0000_0000_0000,
            0x0000_0000_0000_0000,
            0x3ff0_0000_0000_0000,
            0x419d_6f34_5400_0000,
            0x3eb0_c6f7_a0b5_ed8d,
            0x3ee4_f8b5_88e3_68f1,
            0x430c_6bf5_2634_0000,
            0x4341_c379_37e0_8000,
            0x7fe1_ccf3_85eb_c8a0,
            0x7fef_ffff_ffff_ffff,
            0x7ff0_0000_0000_0000,
            0xfff0_0000_0000_0000,
            0x7ff8_0000_0000_0000,
        ];
        let expected = concat!(
            "{\"auto_generated_kv_pairs\":{},\"user_generated_kv_pairs\":{\"f\":-0.0}}\n",
            "{\"auto_generated_kv_pairs\":{},\"user_generated_kv_pairs\":{\"f\":0.0}}\n",
            "{\"auto_generated_kv_pairs\":{},\"user_generated_kv_pairs\":{\"f\":1.0}}\n",
            "{\"auto_generated_kv_pairs\":{},\"user_generated_kv_pairs\":{\"f\":123456789.0}}\n",
            "{\"auto_generated_kv_pairs\":{},\"user_generated_kv_pairs\":{\"f\":1e-06}}\n",
            "{\"auto_generated_kv_pairs\":{},\"user_generated_kv_pairs\":{\"f\":1e-05}}\n",
            "{\"auto_generated_kv_pairs\":{},\"user_generated_kv_pairs\":{\"f\":1e+15}}\n",
            "{\"auto_generated_kv_pairs\":{},\"user_generated_kv_pairs\":{\"f\":1e+16}}\n",
            "{\"auto_generated_kv_pairs\":{},\"user_generated_kv_pairs\":{\"f\":1e+308}}\n",
            "{\"auto_generated_kv_pairs\":{},\"user_generated_kv_pairs\":",
            "{\"f\":1.7976931348623157e+308}}\n",
            "{\"auto_generated_kv_pairs\":{},\"user_generated_kv_pairs\":{\"f\":null}}\n",
            "{\"auto_generated_kv_pairs\":{},\"user_generated_kv_pairs\":{\"f\":null}}\n",
            "{\"auto_generated_kv_pairs\":{},\"user_generated_kv_pairs\":{\"f\":null}}\n",
        );
        assert_eq!(
            expected.as_bytes(),
            search(&float_stream(&bits), "*:*").as_slice()
        );
    }

    #[test]
    fn unstructured_array_is_parsed_and_canonicalized_like_nlohmann() {
        let source = concat!(
            r#"[ { "z" : 1e15, "a" : "\\u0061", "dup":1, "dup":2 }, "#,
            r#"-0, 1.0, {"b":2,"a":1}, [ 3 , 2 ], "a\\/b", "\\u00e9" ]"#,
        )
        .as_bytes();
        let expected = concat!(
            "{\"auto_generated_kv_pairs\":{},\"user_generated_kv_pairs\":{\"arr\":",
            "[{\"a\":\"a\",\"dup\":2,\"z\":1e+15},0,1.0,{\"a\":1,\"b\":2},",
            "[3,2],\"a/b\",\"é\"]}}\n",
        );
        assert_eq!(
            expected.as_bytes(),
            search(&array_stream_with_json(source), "arr:*").as_slice()
        );
    }

    #[test]
    fn canonical_array_number_domains_match_nlohmann() {
        let source = concat!(
            "[18446744073709551615,18446744073709551616,",
            "999999999999999999999999999999999999999999999999999,",
            "-9223372036854775808,-9223372036854775809]",
        );
        let expected = concat!(
            "{\"auto_generated_kv_pairs\":{},\"user_generated_kv_pairs\":{\"arr\":",
            "[18446744073709551615,1.8446744073709552e+19,1e+51,",
            "-9223372036854775808,-9.223372036854776e+18]}}\n",
        );
        assert_eq!(
            expected.as_bytes(),
            search(&array_stream_with_json(source.as_bytes()), "arr:*").as_slice()
        );
    }

    #[test]
    fn canonical_array_expansion_obeys_record_limit_transactionally() {
        let bytes = array_stream_with_json(b"[1e9]");
        let query = parse_kql("arr:*", KqlLimits::default()).expect("query");
        let output = KvIrJsonlMatchSink::new(
            Vec::new(),
            KvIrJsonlOptions::new().with_limits(KvIrJsonlLimits::new().with_max_record_bytes(76)),
        );
        let mut searcher =
            KvIrSearchSink::new(&query, output, KvIrSearchOptions::default()).expect("compile");
        let mut reader = KvIrReader::new(Cursor::new(&bytes), KvIrOptions::default());
        let error = search_first_kv_ir_stream(&mut reader, &mut searcher)
            .expect_err("canonical spelling expands beyond the record bound");
        assert!(matches!(
            error,
            KvIrReadError::Sink {
                source: super::KvIrSearchError::MatchSink(super::KvIrJsonlError::Limit {
                    resource: super::KvIrJsonlLimitResource::RecordBytes,
                    ..
                }),
                ..
            }
        ));
        assert_eq!(&[] as &[u8], searcher.match_sink().output().as_slice());
    }

    #[test]
    fn malformed_or_non_utf8_unstructured_array_is_rejected_transactionally() {
        for source in [
            &b"[1,]"[..],
            &b"[\"\xff\"]"[..],
            &b"[1e400]"[..],
            &b"[-1e400]"[..],
        ] {
            let bytes = array_stream_with_json(source);
            let query = parse_kql("arr:*", KqlLimits::default()).expect("query");
            let output = KvIrJsonlMatchSink::new(Vec::new(), KvIrJsonlOptions::default());
            let mut searcher =
                KvIrSearchSink::new(&query, output, KvIrSearchOptions::default()).expect("compile");
            let mut reader = KvIrReader::new(Cursor::new(&bytes), KvIrOptions::default());
            let error = search_first_kv_ir_stream(&mut reader, &mut searcher)
                .expect_err("invalid array JSON");
            assert!(matches!(
                error,
                KvIrReadError::Sink {
                    source: super::KvIrSearchError::MatchSink(
                        super::KvIrJsonlError::InvalidArrayJson
                    ),
                    ..
                }
            ));
            assert_eq!(&[] as &[u8], searcher.match_sink().output().as_slice());
        }
    }

    #[test]
    fn unstructured_array_exists_but_comparisons_are_false() {
        let bytes = array_stream();
        let expected =
            b"{\"auto_generated_kv_pairs\":{},\"user_generated_kv_pairs\":{\"arr\":[1,null]}}\n";
        assert_eq!(expected, search(&bytes, "arr:*").as_slice());
        assert_eq!(&[] as &[u8], search(&bytes, "arr:*null*").as_slice());
        assert_eq!(expected, search(&bytes, "NOT arr:*null*").as_slice());
    }

    #[test]
    fn compatibility_wrapper_ignores_concatenated_streams() {
        let mut bytes = decode_hex(FOUR_BYTE_ORACLE_HEX);
        let first_stream_bytes = bytes.len();
        bytes.extend_from_slice(&decode_hex(EIGHT_BYTE_ORACLE_HEX));
        assert_eq!(CANONICAL_OUTPUT, search(&bytes, "*:*").as_slice());

        let query = parse_kql("*: *", KqlLimits::default()).expect("query");
        let mut first_count = CountMatches(0);
        let mut first_searcher =
            KvIrSearchSink::new(&query, &mut first_count, KvIrSearchOptions::default())
                .expect("compile");
        let mut first_reader = KvIrReader::new(Cursor::new(&bytes), KvIrOptions::default());
        search_first_kv_ir_stream(&mut first_reader, &mut first_searcher).expect("first stream");
        drop(first_searcher);
        assert_eq!(1, first_count.0);
        assert!(
            first_reader.into_inner().position()
                > u64::try_from(first_stream_bytes).expect("fixture length fits u64"),
            "the fixed reader buffer may consume physical bytes past the logical first stream"
        );

        let query = parse_kql("*: *", KqlLimits::default()).expect("query");
        let mut count = CountMatches(0);
        let mut searcher =
            KvIrSearchSink::new(&query, &mut count, KvIrSearchOptions::default()).expect("compile");
        let mut reader = KvIrReader::new(Cursor::new(&bytes), KvIrOptions::default());
        reader
            .read_to_end(&mut searcher)
            .expect("generic sink searches concatenated streams");
        let stats = searcher.stats();
        let count = searcher.into_inner();
        assert_eq!(2, count.0);
        assert_eq!(2, stats.streams());
    }

    #[test]
    fn missing_end_marker_preserves_completed_output_then_errors() {
        let mut bytes = decode_hex(FOUR_BYTE_ORACLE_HEX);
        bytes.pop();
        let query = parse_kql("*: *", KqlLimits::default()).expect("query");
        let output = KvIrJsonlMatchSink::new(Vec::new(), KvIrJsonlOptions::default());
        let mut searcher =
            KvIrSearchSink::new(&query, output, KvIrSearchOptions::default()).expect("compile");
        let mut reader = KvIrReader::new(Cursor::new(&bytes), KvIrOptions::default());
        let error = search_first_kv_ir_stream(&mut reader, &mut searcher)
            .expect_err("missing explicit EOF is fatal");
        assert!(matches!(
            error,
            KvIrReadError::Reader(ref source)
                if matches!(source.kind(), KvIrErrorKind::Truncated { .. })
        ));
        assert_eq!(
            CANONICAL_OUTPUT,
            searcher.match_sink().output().as_slice(),
            "C++ also emits a complete preceding event before reporting truncation"
        );
    }

    #[test]
    fn cpp_tolerated_truncation_matches_exact_cut_sweeps() {
        for (name, source, tolerated_cuts) in [
            (
                "canonical",
                FOUR_BYTE_ORACLE_HEX,
                &[
                    227, 237, 245, 255, 267, 276, 283, 292, 300, 304, 306, 308, 310, 312,
                ][..],
            ),
            (
                "nested",
                NESTED_ORACLE_HEX,
                &[
                    28, 34, 42, 48, 54, 60, 69, 78, 80, 82, 84, 86, 88, 102, 104, 106,
                ][..],
            ),
        ] {
            let bytes = decode_hex(source);
            for cut in 0..=bytes.len() {
                let query = parse_kql("*: *", KqlLimits::default()).expect("query");
                let output = KvIrJsonlMatchSink::new(Vec::new(), KvIrJsonlOptions::default());
                let mut searcher =
                    KvIrSearchSink::new(&query, output, KvIrSearchOptions::default())
                        .expect("compile");
                let mut reader =
                    KvIrReader::new(Cursor::new(&bytes[..cut]), KvIrOptions::default());
                let result = search_first_kv_ir_stream(&mut reader, &mut searcher);
                let tolerated = result
                    .as_ref()
                    .is_err_and(is_cpp_tolerated_kv_ir_truncation);
                assert_eq!(
                    tolerated_cuts.contains(&cut),
                    tolerated,
                    "{name} cut {cut}: {result:?}"
                );
                if name == "nested" && [102, 104, 106].contains(&cut) {
                    assert_eq!(NESTED_FIRST, searcher.match_sink().output().as_slice());
                }
            }
        }
    }

    #[test]
    fn schema_and_output_limits_are_explicit_and_transactional() {
        let bytes = decode_hex(NESTED_ORACLE_HEX);
        let limits = KvIrSearchLimits::new().with_max_schema_nodes_per_namespace(2);
        let error = search_with_options(
            &bytes,
            "*: *",
            KvIrSearchOptions::default().with_limits(limits),
        )
        .expect_err("schema limit");
        assert!(error.contains("schema nodes per namespace"));

        let query = parse_kql("*: *", KqlLimits::default()).expect("query");
        let json_limits = KvIrJsonlLimits::new().with_max_record_bytes(16);
        let output =
            KvIrJsonlMatchSink::new(Vec::new(), KvIrJsonlOptions::new().with_limits(json_limits));
        let mut searcher =
            KvIrSearchSink::new(&query, output, KvIrSearchOptions::default()).expect("compile");
        let mut reader = KvIrReader::new(Cursor::new(&bytes), KvIrOptions::default());
        let error =
            search_first_kv_ir_stream(&mut reader, &mut searcher).expect_err("record limit");
        assert!(matches!(error, KvIrReadError::Sink { .. }));
        assert_eq!(&[] as &[u8], searcher.match_sink().output().as_slice());
        assert_eq!(0, searcher.stats().matches());
    }

    #[test]
    fn substring_candidate_quirk_includes_directories_and_suffixes() {
        assert!(is_kv_ir_search_candidate(std::path::Path::new(
            "input.clp.zst"
        )));
        assert!(is_kv_ir_search_candidate(std::path::Path::new(
            "input.clp.zst.backup"
        )));
        assert!(is_kv_ir_search_candidate(std::path::Path::new(
            "dir.clp.zst/input.bin"
        )));
        assert!(!is_kv_ir_search_candidate(std::path::Path::new(
            "input.CLP.ZST"
        )));
        assert!(!is_kv_ir_search_candidate(std::path::Path::new(
            "input.zst"
        )));
    }

    struct CountMatches(usize);

    impl KvIrMatchSink for &mut CountMatches {
        type Error = Infallible;

        fn write_match(&mut self, _event: KvIrMatchedEvent<'_>) -> Result<(), Self::Error> {
            self.0 += 1;
            Ok(())
        }
    }

    fn array_stream() -> Vec<u8> {
        array_stream_with_json(b"[1,null]")
    }

    fn array_stream_with_json(json: &[u8]) -> Vec<u8> {
        let metadata = br#"{"VERSION":"0.1.0"}"#;
        let mut bytes = vec![0xfd, 0x2f, 0xb5, 0x29, 0x01, 0x11];
        bytes.push(u8::try_from(metadata.len()).expect("small metadata"));
        bytes.extend_from_slice(metadata);
        bytes.extend_from_slice(&[0x75, 0x60, 0, 0x41, 3]);
        bytes.extend_from_slice(b"arr");
        bytes.extend_from_slice(&[0x65, 1, 0x59, 0x21]);
        bytes.push(u8::try_from(json.len()).expect("test array fits one-byte logtype"));
        bytes.extend_from_slice(json);
        bytes.push(0);
        bytes
    }

    fn float_stream(bits: &[u64]) -> Vec<u8> {
        let metadata = br#"{"VERSION":"0.1.0"}"#;
        let mut bytes = vec![0xfd, 0x2f, 0xb5, 0x29, 0x01, 0x11];
        bytes.push(u8::try_from(metadata.len()).expect("small metadata"));
        bytes.extend_from_slice(metadata);
        bytes.extend_from_slice(&[0x72, 0x60, 0, 0x41, 1, b'f']);
        for value in bits {
            bytes.extend_from_slice(&[0x65, 1, 0x56]);
            bytes.extend_from_slice(&value.to_be_bytes());
        }
        bytes.push(0);
        bytes
    }

    fn integer_stream(values: &[i64]) -> Vec<u8> {
        let metadata = br#"{"VERSION":"0.1.0"}"#;
        let mut bytes = vec![0xfd, 0x2f, 0xb5, 0x29, 0x01, 0x11];
        bytes.push(u8::try_from(metadata.len()).expect("small metadata"));
        bytes.extend_from_slice(metadata);
        bytes.extend_from_slice(&[0x71, 0x60, 0, 0x41, 1, b'n']);
        for value in values {
            bytes.extend_from_slice(&[0x65, 1, 0x54]);
            bytes.extend_from_slice(&value.to_be_bytes());
        }
        bytes.push(0);
        bytes
    }

    #[test]
    fn compiled_leaf_limit_is_reported_before_consuming_input() {
        let query = parse_kql("a:(1 2)", KqlLimits::default()).expect("query");
        let limits = KvIrSearchLimits::new().with_max_compiled_leaves(1);
        let Err(error) = KvIrSearchSink::new(
            &query,
            CountMatches(0),
            KvIrSearchOptions::default().with_limits(limits),
        ) else {
            panic!("leaf limit must fail");
        };
        let KvIrSearchFailure::Limit(limit) = error else {
            panic!("unexpected error: {error}");
        };
        assert_eq!(KvIrSearchLimitResource::CompiledLeaves, limit.resource());
    }
}
