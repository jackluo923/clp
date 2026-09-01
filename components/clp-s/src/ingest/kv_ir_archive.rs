//! Bounded, nonrecursive conversion from validated KV-IR events into archive-set records.
//!
//! The adapter retains only the current protocol schema trees plus reusable selection, traversal,
//! event-plan, and decoded-text scratch buffers. Protocol bytes from a stream preamble through a
//! log event are attributed atomically to the archive containing that event. Bytes after the last
//! event, including the explicit EOF unit, are attributed when [`KvIrItem::StreamEnd`] arrives.
//! Consequently, an EOF after an exact rotation boundary belongs to the required final empty
//! archive, matching [`ArchiveSetWriter`] source-accounting semantics.
//!
//! UTC-offset change units are validated and source-accounted but do not alter timestamp values.
//! This intentionally fixes a pinned C++ defect where the KV-IR handler treats every offset
//! change as a protocol error and truncates the input. A timestamp-aware view can promote matching
//! integer, fixed-nine float, ordinary string, and reconstructed encoded-text values without
//! reparsing JSON or allocating a record tree. Matching booleans, nulls, objects, and arrays retain
//! their ordinary values instead of reproducing the pinned C++ writer's wrong-variant failure.
//!
//! Ordinary CLP encoded-text ASTs are validated and passed directly to the archive writer without
//! reconstructing or rescanning their decoded text. Timestamp candidates and unstructured arrays
//! still reconstruct an exact lexeme because timestamp parsing and JSON-array validation require
//! it. Four-byte dictionary variables retain the C++ reclassification rules, while eight-byte
//! dictionary variables retain their dictionary representation.

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::error::Error;
use std::fmt;
use std::fmt::Display;
use std::fmt::Formatter;
use std::str;

use super::JsonEvent;
use super::JsonNumberClassificationError;
use super::JsonNumberDomain;
use super::JsonTimestampError;
use super::KvIrEncodedText;
use super::KvIrEncodedVariable;
use super::KvIrEncoding;
use super::KvIrItem;
use super::KvIrLogEvent;
use super::KvIrNamespace;
use super::KvIrNodeType;
use super::KvIrPair;
use super::KvIrSchemaNode;
use super::KvIrSink;
use super::KvIrTimestampResolver;
use super::timestamp::KvIrResolvedTimestamp;
use crate::archive::RangeIndexValue;
use crate::writer::ArchiveSetAppendError;
use crate::writer::ArchiveSetError;
use crate::writer::ArchiveSetStatsCallback;
use crate::writer::ArchiveSetWriter;
use crate::writer::ArchiveSourceContext;
use crate::writer::ArchiveSourceContextError;
use crate::writer::FieldRef;
use crate::writer::FinalizedArchiveSink;
use crate::writer::RecordEventAppendError;
use crate::writer::RecordEventConsumer;
use crate::writer::ReplayableRecordEventSource;
use crate::writer::TimestampRef;
use crate::writer::UnstructuredArrayRef;
use crate::writer::ValueRef;

const MEBIBYTE: u64 = 1024 * 1024;
const FIXED_FLOAT_BUFFER_BYTES: usize = 384;
const NO_SPARSE_INDEX: u32 = u32::MAX;

struct FixedFloatBuffer {
    bytes: [u8; FIXED_FLOAT_BUFFER_BYTES],
    len: usize,
}

impl FixedFloatBuffer {
    const fn new() -> Self {
        Self {
            bytes: [0; FIXED_FLOAT_BUFFER_BYTES],
            len: 0,
        }
    }

    const fn as_bytes(&self) -> &[u8] {
        self.bytes.split_at(self.len).0
    }
}

impl fmt::Write for FixedFloatBuffer {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        let end = self.len.checked_add(value.len()).ok_or(fmt::Error)?;
        let destination = self.bytes.get_mut(self.len..end).ok_or(fmt::Error)?;
        destination.copy_from_slice(value.as_bytes());
        self.len = end;
        Ok(())
    }
}

/// Hard limits for adapter-owned schema and per-record scratch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KvIrArchiveLimits {
    schema_nodes_per_namespace: u64,
    schema_key_bytes: u64,
    nesting_depth: u64,
    record_events: u64,
    reconstructed_value_bytes: u64,
    reconstructed_record_bytes: u64,
}

impl KvIrArchiveLimits {
    /// Conservative defaults independent of the decoder's own wire limits.
    pub const DEFAULT: Self = Self {
        schema_nodes_per_namespace: 1_000_000,
        schema_key_bytes: 64 * MEBIBYTE,
        nesting_depth: 256,
        record_events: 1_000_000,
        reconstructed_value_bytes: 16 * MEBIBYTE,
        reconstructed_record_bytes: 64 * MEBIBYTE,
    };

    /// Creates the default limit set for builder-style overrides.
    #[must_use]
    pub const fn new() -> Self {
        Self::DEFAULT
    }

    /// Replaces the maximum non-root nodes retained in either namespace.
    #[must_use]
    pub const fn with_max_schema_nodes_per_namespace(mut self, value: u64) -> Self {
        self.schema_nodes_per_namespace = value;
        self
    }

    /// Replaces the maximum aggregate schema-key bytes retained for one stream.
    #[must_use]
    pub const fn with_max_schema_key_bytes(mut self, value: u64) -> Self {
        self.schema_key_bytes = value;
        self
    }

    /// Replaces the maximum retained schema and output-object depth.
    #[must_use]
    pub const fn with_max_nesting_depth(mut self, value: u64) -> Self {
        self.nesting_depth = value;
        self
    }

    /// Replaces the maximum writer-native events planned for one record.
    #[must_use]
    pub const fn with_max_record_events(mut self, value: u64) -> Self {
        self.record_events = value;
        self
    }

    /// Replaces the maximum decoded bytes for one encoded-text value.
    #[must_use]
    pub const fn with_max_reconstructed_value_bytes(mut self, value: u64) -> Self {
        self.reconstructed_value_bytes = value;
        self
    }

    /// Replaces the maximum aggregate decoded encoded-text bytes for one record.
    #[must_use]
    pub const fn with_max_reconstructed_record_bytes(mut self, value: u64) -> Self {
        self.reconstructed_record_bytes = value;
        self
    }

    /// Maximum non-root schema nodes per namespace.
    #[must_use]
    pub const fn max_schema_nodes_per_namespace(self) -> u64 {
        self.schema_nodes_per_namespace
    }

    /// Maximum retained schema-key bytes per stream.
    #[must_use]
    pub const fn max_schema_key_bytes(self) -> u64 {
        self.schema_key_bytes
    }

    /// Maximum schema and output-object depth.
    #[must_use]
    pub const fn max_nesting_depth(self) -> u64 {
        self.nesting_depth
    }

    /// Maximum writer-native events per record.
    #[must_use]
    pub const fn max_record_events(self) -> u64 {
        self.record_events
    }

    /// Maximum reconstructed bytes in one encoded-text value.
    #[must_use]
    pub const fn max_reconstructed_value_bytes(self) -> u64 {
        self.reconstructed_value_bytes
    }

    /// Maximum reconstructed encoded-text bytes in one record.
    #[must_use]
    pub const fn max_reconstructed_record_bytes(self) -> u64 {
        self.reconstructed_record_bytes
    }
}

impl Default for KvIrArchiveLimits {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Configuration for [`KvIrArchiveSetSink`].
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct KvIrArchiveOptions {
    limits: KvIrArchiveLimits,
}

impl KvIrArchiveOptions {
    /// Creates options with conservative hard limits.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            limits: KvIrArchiveLimits::DEFAULT,
        }
    }

    /// Replaces all adapter limits.
    #[must_use]
    pub const fn with_limits(mut self, limits: KvIrArchiveLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Returns the configured hard limits.
    #[must_use]
    pub const fn limits(self) -> KvIrArchiveLimits {
        self.limits
    }
}

/// Adapter-owned resource that could not be grown.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum KvIrArchiveResource {
    /// Retained schema nodes.
    SchemaNodes,
    /// One retained schema key.
    SchemaKey,
    /// Per-event node-to-pair selection maps.
    SelectionMap,
    /// Per-event links between selected schema paths.
    SparseTraversal,
    /// Iterative schema traversal frames.
    TraversalStack,
    /// Writer-native event plan.
    RecordPlan,
    /// Reconstructed encoded-text bytes.
    ReconstructedText,
}

impl Display for KvIrArchiveResource {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::SchemaNodes => "schema nodes",
            Self::SchemaKey => "schema key",
            Self::SelectionMap => "event selection map",
            Self::SparseTraversal => "sparse event traversal",
            Self::TraversalStack => "schema traversal stack",
            Self::RecordPlan => "record event plan",
            Self::ReconstructedText => "reconstructed encoded text",
        })
    }
}

/// Adapter limit domain.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum KvIrArchiveLimitResource {
    /// Retained non-root nodes in one namespace.
    SchemaNodesPerNamespace,
    /// Aggregate schema-key bytes in one stream.
    SchemaKeyBytes,
    /// Schema or emitted object depth.
    NestingDepth,
    /// Writer-native events in one record.
    RecordEvents,
    /// Reconstructed bytes in one encoded-text value.
    ReconstructedValueBytes,
    /// Aggregate reconstructed bytes in one record.
    ReconstructedRecordBytes,
}

impl Display for KvIrArchiveLimitResource {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::SchemaNodesPerNamespace => "schema nodes per namespace",
            Self::SchemaKeyBytes => "schema key bytes",
            Self::NestingDepth => "nesting depth",
            Self::RecordEvents => "record events",
            Self::ReconstructedValueBytes => "reconstructed value bytes",
            Self::ReconstructedRecordBytes => "reconstructed record bytes",
        })
    }
}

/// One observed hard-limit violation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KvIrArchiveLimitViolation {
    resource: KvIrArchiveLimitResource,
    actual: u64,
    limit: u64,
}

impl KvIrArchiveLimitViolation {
    const fn new(resource: KvIrArchiveLimitResource, actual: u64, limit: u64) -> Self {
        Self {
            resource,
            actual,
            limit,
        }
    }

    /// Returns the bounded resource.
    #[must_use]
    pub const fn resource(self) -> KvIrArchiveLimitResource {
        self.resource
    }

    /// Returns the first rejected value.
    #[must_use]
    pub const fn actual(self) -> u64 {
        self.actual
    }

    /// Returns the configured maximum.
    #[must_use]
    pub const fn limit(self) -> u64 {
        self.limit
    }
}

impl Display for KvIrArchiveLimitViolation {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} limit exceeded: {} > {}",
            self.resource, self.actual, self.limit
        )
    }
}

impl Error for KvIrArchiveLimitViolation {}

/// Broken adapter invariant in an otherwise validated callback sequence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum KvIrArchiveInvalidData {
    /// An item arrived outside a started stream.
    ItemOutsideStream,
    /// A new stream began before the preceding stream ended.
    NestedStreamStart,
    /// An item's absolute byte position disagreed with accounted input.
    NonContiguousInput,
    /// A schema callback did not carry the next implicit node ID.
    SchemaNodeOutOfSequence {
        /// Schema namespace.
        namespace: KvIrNamespace,
        /// Expected implicit node ID.
        expected: u32,
        /// Callback node ID.
        actual: u32,
    },
    /// An event referenced a node absent from the retained schema.
    MissingSchemaNode {
        /// Schema namespace.
        namespace: KvIrNamespace,
        /// Missing node ID.
        node_id: u32,
    },
    /// A retained node's parent was not an object.
    NonObjectAncestor {
        /// Schema namespace.
        namespace: KvIrNamespace,
        /// Invalid node ID.
        node_id: u32,
    },
    /// The event changed a node selection already validated as unique.
    DuplicateEventNode {
        /// Schema namespace.
        namespace: KvIrNamespace,
        /// Duplicated node ID.
        node_id: u32,
    },
    /// A planned pair disappeared before record traversal.
    MissingPlannedPair {
        /// Zero-based protocol pair index.
        pair_index: usize,
    },
    /// The decoder's validated metadata events were not a balanced JSON value traversal.
    MetadataTraversal,
}

impl Display for KvIrArchiveInvalidData {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::ItemOutsideStream => formatter.write_str("KV-IR item arrived outside a stream"),
            Self::NestedStreamStart => {
                formatter.write_str("KV-IR stream began before the prior stream ended")
            }
            Self::NonContiguousInput => {
                formatter.write_str("KV-IR item offset is not contiguous with accounted input")
            }
            Self::SchemaNodeOutOfSequence {
                namespace,
                expected,
                actual,
            } => write!(
                formatter,
                "{namespace} schema node ID {actual} followed {expected} instead"
            ),
            Self::MissingSchemaNode { namespace, node_id } => {
                write!(formatter, "missing {namespace} schema node {node_id}")
            }
            Self::NonObjectAncestor { namespace, node_id } => {
                write!(
                    formatter,
                    "{namespace} schema node {node_id} is not an object ancestor"
                )
            }
            Self::DuplicateEventNode { namespace, node_id } => {
                write!(formatter, "duplicate {namespace} event node {node_id}")
            }
            Self::MissingPlannedPair { pair_index } => {
                write!(formatter, "planned pair {pair_index} is unavailable")
            }
            Self::MetadataTraversal => {
                formatter.write_str("validated KV-IR metadata traversal is inconsistent")
            }
        }
    }
}

impl Error for KvIrArchiveInvalidData {}

/// Encoded-text construct that cannot be converted losslessly.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum KvIrArchiveUnsupported {
    /// An ordinary KV float was NaN or infinite; archive float columns require finite values.
    NonFiniteFloat { bits: u64 },
    /// A placeholder's variable width disagreed with the AST encoding.
    EncodedVariableWidthMismatch,
    /// An integer or float placeholder had no remaining encoded variable.
    MissingEncodedVariable,
    /// A dictionary placeholder had no remaining dictionary variable.
    MissingDictionaryVariable,
    /// The logtype ended with an escape byte and no escaped byte.
    TrailingEscape,
    /// An encoded float's decimal point lies beyond its declared digits.
    EncodedFloatDecimalPosition,
    /// An eight-byte encoded float's digit field exceeds the C++ decimal domain.
    EncodedFloatDigitsTooLarge,
    /// An encoded float's digit value needs more digits than declared.
    EncodedFloatDigitCount,
}

impl Display for KvIrArchiveUnsupported {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::NonFiniteFloat { .. } => "non-finite KV float",
            Self::EncodedVariableWidthMismatch => "encoded-text variable width mismatch",
            Self::MissingEncodedVariable => "encoded-text placeholder has no encoded variable",
            Self::MissingDictionaryVariable => {
                "encoded-text placeholder has no dictionary variable"
            }
            Self::TrailingEscape => "encoded-text logtype has a trailing escape",
            Self::EncodedFloatDecimalPosition => {
                "encoded float decimal position exceeds its declared digits"
            }
            Self::EncodedFloatDigitsTooLarge => "encoded float digit field exceeds its domain",
            Self::EncodedFloatDigitCount => {
                "encoded float digit field exceeds its declared digit count"
            }
        })
    }
}

impl Error for KvIrArchiveUnsupported {}

/// Conversion failure that does not originate in the archive-set session.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum KvIrArchiveFailure {
    /// Callback sequencing or retained-schema invariant failed.
    Invalid(KvIrArchiveInvalidData),
    /// The current writer cannot represent this validated IR construct losslessly.
    Unsupported {
        /// Schema namespace of the value.
        namespace: KvIrNamespace,
        /// Schema node ID of the value.
        node_id: u32,
        /// Unsupported construct.
        source: KvIrArchiveUnsupported,
    },
    /// A matching integer, float, or string could not be resolved as an authoritative timestamp.
    Timestamp {
        /// Schema namespace of the value.
        namespace: KvIrNamespace,
        /// Schema node ID of the value.
        node_id: u32,
        /// Timestamp recognition or epoch-range failure.
        source: JsonTimestampError,
    },
    /// An explicit adapter limit was exceeded.
    Limit(KvIrArchiveLimitViolation),
    /// A bounded allocation failed.
    AllocationFailed {
        /// Buffer that could not grow.
        resource: KvIrArchiveResource,
        /// Requested additional elements or bytes.
        requested_additional: usize,
    },
    /// KV-IR user metadata conflicted with the source context.
    SourceContext(ArchiveSourceContextError),
    /// A syntactically valid metadata number exceeded the C++ JSON numeric domain.
    MetadataNumber(JsonNumberClassificationError),
    /// A checked byte, ID, event, or statistics counter overflowed.
    SizeOverflow,
}

impl Display for KvIrArchiveFailure {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(source) => Display::fmt(source, formatter),
            Self::Unsupported {
                namespace,
                node_id,
                source,
            } => write!(
                formatter,
                "unsupported {namespace} node {node_id}: {source}"
            ),
            Self::Timestamp {
                namespace,
                node_id,
                source,
            } => write!(
                formatter,
                "invalid authoritative timestamp at {namespace} node {node_id}: {source}"
            ),
            Self::Limit(source) => Display::fmt(source, formatter),
            Self::AllocationFailed {
                resource,
                requested_additional,
            } => write!(
                formatter,
                "failed to reserve {requested_additional} additional units for {resource}"
            ),
            Self::SourceContext(source) => Display::fmt(source, formatter),
            Self::MetadataNumber(source) => {
                write!(formatter, "invalid KV-IR user metadata number: {source}")
            }
            Self::SizeOverflow => formatter.write_str("KV-IR archive adapter size overflow"),
        }
    }
}

impl Error for KvIrArchiveFailure {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Invalid(source) => Some(source),
            Self::Unsupported { source, .. } => Some(source),
            Self::Timestamp { source, .. } => Some(source),
            Self::Limit(source) => Some(source),
            Self::SourceContext(source) => Some(source),
            Self::MetadataNumber(source) => Some(source),
            Self::AllocationFailed { .. } | Self::SizeOverflow => None,
        }
    }
}

/// Adapter conversion or archive-set failure.
#[derive(Debug)]
#[non_exhaustive]
pub enum KvIrArchiveErrorKind<S, C> {
    /// Input-to-record conversion failed before append.
    Conversion(KvIrArchiveFailure),
    /// The archive-set session rejected or failed after the planned record.
    ArchiveSet(ArchiveSetError<S, C>),
}

impl<S, C> KvIrArchiveErrorKind<S, C> {
    /// Returns whether the triggering log event committed before the failure.
    #[must_use]
    pub const fn record_committed(&self) -> bool {
        match self {
            Self::Conversion(_) => false,
            Self::ArchiveSet(source) => source.record_committed(),
        }
    }
}

impl<S: Display, C: Display> Display for KvIrArchiveErrorKind<S, C> {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Conversion(source) => Display::fmt(source, formatter),
            Self::ArchiveSet(source) => Display::fmt(source, formatter),
        }
    }
}

impl<S: Error + 'static, C: Error + 'static> Error for KvIrArchiveErrorKind<S, C> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Conversion(source) => Some(source),
            Self::ArchiveSet(source) => Some(source),
        }
    }
}

/// A conversion failure located at the triggering protocol item.
#[derive(Debug)]
pub struct KvIrArchiveError<S, C> {
    stream_index: u64,
    unit_index: Option<u64>,
    event_index: Option<u64>,
    input_offset: u64,
    kind: KvIrArchiveErrorKind<S, C>,
}

impl<S, C> KvIrArchiveError<S, C> {
    /// Returns the zero-based concatenated stream index.
    #[must_use]
    pub const fn stream_index(&self) -> u64 {
        self.stream_index
    }

    /// Returns the unit index, absent only for a preamble failure.
    #[must_use]
    pub const fn unit_index(&self) -> Option<u64> {
        self.unit_index
    }

    /// Returns the log-event index when the failure belongs to a record.
    #[must_use]
    pub const fn event_index(&self) -> Option<u64> {
        self.event_index
    }

    /// Returns the absolute input offset of the triggering item.
    #[must_use]
    pub const fn input_offset(&self) -> u64 {
        self.input_offset
    }

    /// Returns the structured failure kind.
    #[must_use]
    pub const fn kind(&self) -> &KvIrArchiveErrorKind<S, C> {
        &self.kind
    }

    /// Returns whether the triggering log event committed before the failure.
    #[must_use]
    pub const fn record_committed(&self) -> bool {
        self.kind.record_committed()
    }
}

impl<S: Display, C: Display> Display for KvIrArchiveError<S, C> {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "KV-IR archive conversion failed at byte {}, stream {}, unit {:?}, event {:?}: {}",
            self.input_offset, self.stream_index, self.unit_index, self.event_index, self.kind
        )
    }
}

impl<S: Error + 'static, C: Error + 'static> Error for KvIrArchiveError<S, C> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.kind)
    }
}

/// Successfully committed adapter counters.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct KvIrArchiveStats {
    streams: u64,
    records: u64,
    source_bytes: u64,
}

impl KvIrArchiveStats {
    /// Returns started streams.
    #[must_use]
    pub const fn streams(self) -> u64 {
        self.streams
    }

    /// Returns records committed to the archive set.
    #[must_use]
    pub const fn records(self) -> u64 {
        self.records
    }

    /// Returns source bytes attributed to archive headers.
    #[must_use]
    pub const fn source_bytes(self) -> u64 {
        self.source_bytes
    }
}

#[derive(Debug)]
struct SchemaNode {
    key: Vec<u8>,
    parent: u32,
    timestamp_prefix_length: Option<u32>,
    node_type: KvIrNodeType,
}

impl SchemaNode {
    const fn root() -> Self {
        Self {
            parent: 0,
            key: Vec::new(),
            node_type: KvIrNodeType::Object,
            timestamp_prefix_length: None,
        }
    }

    fn parent(&self) -> usize {
        usize::try_from(self.parent).expect("validated schema parent ID fits usize")
    }

    fn timestamp_prefix_length(&self) -> Option<usize> {
        self.timestamp_prefix_length
            .and_then(|length| usize::try_from(length).ok())
    }
}

#[derive(Debug, Default)]
struct SchemaTree {
    nodes: Vec<SchemaNode>,
}

impl SchemaTree {
    fn reset(&mut self) -> Result<(), KvIrArchiveFailure> {
        self.nodes.clear();
        self.nodes
            .try_reserve(1)
            .map_err(|_| KvIrArchiveFailure::AllocationFailed {
                resource: KvIrArchiveResource::SchemaNodes,
                requested_additional: 1,
            })?;
        self.nodes.push(SchemaNode::root());
        Ok(())
    }

    fn insert(
        &mut self,
        node: KvIrSchemaNode<'_>,
        resolver: Option<&KvIrTimestampResolver>,
    ) -> Result<(), KvIrArchiveFailure> {
        let actual =
            usize::try_from(node.node_id()).map_err(|_| KvIrArchiveFailure::SizeOverflow)?;
        if actual != self.nodes.len() {
            let expected =
                u32::try_from(self.nodes.len()).map_err(|_| KvIrArchiveFailure::SizeOverflow)?;
            return Err(KvIrArchiveFailure::Invalid(
                KvIrArchiveInvalidData::SchemaNodeOutOfSequence {
                    namespace: node.namespace(),
                    expected,
                    actual: node.node_id(),
                },
            ));
        }
        let parent =
            usize::try_from(node.parent_id()).map_err(|_| KvIrArchiveFailure::SizeOverflow)?;
        if !matches!(self.nodes.get(parent), Some(value) if value.node_type == KvIrNodeType::Object)
        {
            return Err(KvIrArchiveFailure::Invalid(
                KvIrArchiveInvalidData::NonObjectAncestor {
                    namespace: node.namespace(),
                    node_id: node.parent_id(),
                },
            ));
        }

        let mut key = Vec::new();
        key.try_reserve(node.key().len())
            .map_err(|_| KvIrArchiveFailure::AllocationFailed {
                resource: KvIrArchiveResource::SchemaKey,
                requested_additional: node.key().len(),
            })?;
        key.extend_from_slice(node.key());
        self.nodes
            .try_reserve(1)
            .map_err(|_| KvIrArchiveFailure::AllocationFailed {
                resource: KvIrArchiveResource::SchemaNodes,
                requested_additional: 1,
            })?;
        self.nodes.push(SchemaNode {
            key,
            parent: node.parent_id(),
            node_type: node.node_type(),
            timestamp_prefix_length: None,
        });
        self.update_timestamp_prefix(actual, node.namespace(), resolver);
        Ok(())
    }

    fn configure_timestamp(
        &mut self,
        namespace: KvIrNamespace,
        resolver: Option<&KvIrTimestampResolver>,
    ) {
        if let Some(root) = self.nodes.first_mut() {
            root.timestamp_prefix_length = resolver
                .is_some_and(|resolver| resolver.namespace() == Some(namespace))
                .then_some(0);
        }
        for node_id in 1..self.nodes.len() {
            self.update_timestamp_prefix(node_id, namespace, resolver);
        }
    }

    fn update_timestamp_prefix(
        &mut self,
        node_id: usize,
        namespace: KvIrNamespace,
        resolver: Option<&KvIrTimestampResolver>,
    ) {
        let Some(resolver) = resolver.filter(|value| value.namespace() == Some(namespace)) else {
            self.nodes[node_id].timestamp_prefix_length = None;
            return;
        };
        let parent = self.nodes[node_id].parent();
        let Some(prefix_length) = self.nodes[parent].timestamp_prefix_length else {
            self.nodes[node_id].timestamp_prefix_length = None;
            return;
        };
        let prefix_index = usize::try_from(prefix_length).ok();
        self.nodes[node_id].timestamp_prefix_length = prefix_index
            .and_then(|prefix_index| resolver.path().components().get(prefix_index))
            .is_some_and(|component| component.as_bytes() == self.nodes[node_id].key)
            .then(|| prefix_length.checked_add(1))
            .flatten();
    }
}

#[derive(Clone, Copy, Debug)]
struct ScratchSpan {
    start: usize,
    end: usize,
}

#[derive(Clone, Copy, Debug)]
struct PlannedTimestamp {
    epoch_nanoseconds: i64,
    pattern: &'static str,
}

#[derive(Clone, Copy, Debug)]
struct ReconstructedText {
    lexeme: ScratchSpan,
    decoded: ScratchSpan,
}

#[derive(Clone, Copy, Debug)]
enum PlannedEvent {
    ObjectStart {
        namespace: KvIrNamespace,
        node_id: usize,
    },
    ObjectEnd,
    Value {
        namespace: KvIrNamespace,
        node_id: usize,
        pair_index: usize,
        reconstructed: Option<ScratchSpan>,
        direct_encoded_text: bool,
        unstructured_array: bool,
        timestamp: Option<PlannedTimestamp>,
    },
}

#[derive(Clone, Copy, Debug)]
struct WalkFrame {
    next_child: Option<usize>,
    close_object: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SparseLinks {
    first_child: u32,
    next_sibling: u32,
}

impl SparseLinks {
    const EMPTY: Self = Self {
        first_child: NO_SPARSE_INDEX,
        next_sibling: NO_SPARSE_INDEX,
    };
}

const _: () = {
    assert!(std::mem::size_of::<SparseLinks>() == std::mem::size_of::<u64>());
};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct SelectionSlot {
    epoch: u32,
    pair_index_plus_one: u32,
    active_index_plus_one: u32,
}

impl SelectionSlot {
    const fn is_included(self, epoch: u32) -> bool {
        self.epoch == epoch
    }

    const fn selected_pair_index(self, epoch: u32) -> Option<u32> {
        if self.is_included(epoch) {
            self.pair_index_plus_one.checked_sub(1)
        } else {
            None
        }
    }

    const fn active_index(self, epoch: u32) -> Option<u32> {
        if self.is_included(epoch) {
            self.active_index_plus_one.checked_sub(1)
        } else {
            None
        }
    }

    fn select(&mut self, epoch: u32, pair_index: usize) -> Result<(), KvIrArchiveFailure> {
        let pair_index = u32::try_from(pair_index)
            .map_err(|_| KvIrArchiveFailure::SizeOverflow)?
            .checked_add(1)
            .ok_or(KvIrArchiveFailure::SizeOverflow)?;
        self.epoch = epoch;
        self.pair_index_plus_one = pair_index;
        Ok(())
    }

    fn set_active_index(
        &mut self,
        epoch: u32,
        active_index: usize,
    ) -> Result<(), KvIrArchiveFailure> {
        debug_assert!(self.is_included(epoch));
        self.active_index_plus_one = u32::try_from(active_index)
            .map_err(|_| KvIrArchiveFailure::SizeOverflow)?
            .checked_add(1)
            .ok_or(KvIrArchiveFailure::SizeOverflow)?;
        Ok(())
    }

    const fn include(&mut self, epoch: u32) -> bool {
        if self.epoch == epoch {
            false
        } else {
            self.epoch = epoch;
            self.pair_index_plus_one = 0;
            self.active_index_plus_one = 0;
            true
        }
    }
}

const _: () = {
    assert!(std::mem::size_of::<SelectionSlot>() == 3 * std::mem::size_of::<u32>());
};

/// Reusable KV-IR sink that appends complete events to a caller-owned archive-set session.
pub struct KvIrArchiveSetSink<'archive, S, C> {
    archive_set: &'archive mut ArchiveSetWriter<S, C>,
    options: KvIrArchiveOptions,
    source_context: Option<ArchiveSourceContext>,
    auto_schema: SchemaTree,
    user_schema: SchemaTree,
    schema_key_bytes: u64,
    selection_auto: Vec<SelectionSlot>,
    selection_user: Vec<SelectionSlot>,
    selection_epoch: u32,
    active_auto: Vec<u32>,
    active_user: Vec<u32>,
    sparse_links: Vec<SparseLinks>,
    traversal: Vec<WalkFrame>,
    plan: Vec<PlannedEvent>,
    reconstructed: Vec<u8>,
    direct_reconstructed_bytes: usize,
    active_stream: bool,
    accounted_input_bytes: u64,
    stats: KvIrArchiveStats,
}

/// Timestamp-aware view of a KV-IR archive-set sink.
pub struct TimestampedKvIrArchiveSetSink<'resolver, 'archive, S, C> {
    inner: KvIrArchiveSetSink<'archive, S, C>,
    resolver: &'resolver KvIrTimestampResolver,
}

impl<'archive, S, C> KvIrArchiveSetSink<'archive, S, C>
where
    S: FinalizedArchiveSink,
    C: ArchiveSetStatsCallback,
{
    /// Creates a bounded adapter over one archive-set session.
    #[must_use]
    pub fn new(
        archive_set: &'archive mut ArchiveSetWriter<S, C>,
        options: KvIrArchiveOptions,
    ) -> Self {
        Self::new_inner(archive_set, options, None)
    }

    /// Creates an adapter that brackets each decoded KV-IR stream with exact source metadata.
    ///
    /// The canonical filename and archive-creator ID come from `source`. At each validated stream
    /// header, the adapter adds `USER_DEFINED_METADATA` fields to a fresh clone of that context,
    /// then begins its range. Concatenated streams therefore form adjacent source contexts and
    /// each starts its own C++ file-split sequence at zero.
    #[must_use]
    pub fn for_source(
        archive_set: &'archive mut ArchiveSetWriter<S, C>,
        options: KvIrArchiveOptions,
        source: ArchiveSourceContext,
    ) -> Self {
        Self::new_inner(archive_set, options, Some(source))
    }

    /// Converts this sink into a timestamp-aware adapter without moving the archive set.
    #[must_use]
    pub fn with_timestamp_resolver<'resolver>(
        mut self,
        resolver: &'resolver KvIrTimestampResolver,
    ) -> TimestampedKvIrArchiveSetSink<'resolver, 'archive, S, C> {
        self.configure_timestamp(Some(resolver));
        TimestampedKvIrArchiveSetSink {
            inner: self,
            resolver,
        }
    }

    fn new_inner(
        archive_set: &'archive mut ArchiveSetWriter<S, C>,
        options: KvIrArchiveOptions,
        source_context: Option<ArchiveSourceContext>,
    ) -> Self {
        Self {
            archive_set,
            options,
            source_context,
            auto_schema: SchemaTree::default(),
            user_schema: SchemaTree::default(),
            schema_key_bytes: 0,
            selection_auto: Vec::new(),
            selection_user: Vec::new(),
            selection_epoch: 0,
            active_auto: Vec::new(),
            active_user: Vec::new(),
            sparse_links: Vec::new(),
            traversal: Vec::new(),
            plan: Vec::new(),
            reconstructed: Vec::new(),
            direct_reconstructed_bytes: 0,
            active_stream: false,
            accounted_input_bytes: 0,
            stats: KvIrArchiveStats::default(),
        }
    }

    fn configure_timestamp(&mut self, resolver: Option<&KvIrTimestampResolver>) {
        self.auto_schema
            .configure_timestamp(KvIrNamespace::AutoGenerated, resolver);
        self.user_schema
            .configure_timestamp(KvIrNamespace::UserGenerated, resolver);
    }

    /// Returns the immutable adapter options.
    #[must_use]
    pub const fn options(&self) -> KvIrArchiveOptions {
        self.options
    }

    /// Returns successfully committed counters.
    #[must_use]
    pub const fn stats(&self) -> KvIrArchiveStats {
        self.stats
    }

    /// Returns exact source bytes already attributed to the archive set.
    #[must_use]
    pub const fn accounted_input_bytes(&self) -> u64 {
        self.accounted_input_bytes
    }

    /// Returns the underlying archive-set session.
    #[must_use = "the borrowed archive-set session should be used"]
    pub const fn archive_set(&self) -> &ArchiveSetWriter<S, C> {
        self.archive_set
    }

    /// Returns the underlying archive-set session mutably.
    pub const fn archive_set_mut(&mut self) -> &mut ArchiveSetWriter<S, C> {
        self.archive_set
    }

    /// Consumes the adapter and returns the underlying archive-set session.
    ///
    /// If decoding stopped inside a source-aware stream, this leaves its source context open so
    /// the caller can explicitly recover or abandon the archive-set session.
    #[must_use = "the recovered archive-set session should be used"]
    pub fn into_inner(self) -> &'archive mut ArchiveSetWriter<S, C> {
        self.archive_set
    }

    const fn failure(
        stream_index: u64,
        unit_index: Option<u64>,
        event_index: Option<u64>,
        input_offset: u64,
        source: KvIrArchiveFailure,
    ) -> KvIrArchiveError<S::Error, C::Error> {
        KvIrArchiveError {
            stream_index,
            unit_index,
            event_index,
            input_offset,
            kind: KvIrArchiveErrorKind::Conversion(source),
        }
    }

    const fn require_active(
        &self,
        stream_index: u64,
        unit_index: Option<u64>,
        event_index: Option<u64>,
        input_offset: u64,
    ) -> Result<(), KvIrArchiveError<S::Error, C::Error>> {
        if self.active_stream {
            Ok(())
        } else {
            Err(Self::failure(
                stream_index,
                unit_index,
                event_index,
                input_offset,
                KvIrArchiveFailure::Invalid(KvIrArchiveInvalidData::ItemOutsideStream),
            ))
        }
    }

    fn start_stream(
        &mut self,
        header: super::KvIrStreamHeader<'_>,
        resolver: Option<&KvIrTimestampResolver>,
    ) -> Result<(), KvIrArchiveError<S::Error, C::Error>> {
        if self.active_stream {
            return Err(Self::failure(
                header.stream_index(),
                None,
                None,
                header.input_offset(),
                KvIrArchiveFailure::Invalid(KvIrArchiveInvalidData::NestedStreamStart),
            ));
        }
        if header.input_offset() != self.accounted_input_bytes {
            return Err(Self::failure(
                header.stream_index(),
                None,
                None,
                header.input_offset(),
                KvIrArchiveFailure::Invalid(KvIrArchiveInvalidData::NonContiguousInput),
            ));
        }
        let streams = self.stats.streams.checked_add(1).ok_or_else(|| {
            Self::failure(
                header.stream_index(),
                None,
                None,
                header.input_offset(),
                KvIrArchiveFailure::SizeOverflow,
            )
        })?;
        self.auto_schema.reset().map_err(|source| {
            Self::failure(
                header.stream_index(),
                None,
                None,
                header.input_offset(),
                source,
            )
        })?;
        self.user_schema.reset().map_err(|source| {
            Self::failure(
                header.stream_index(),
                None,
                None,
                header.input_offset(),
                source,
            )
        })?;
        self.configure_timestamp(resolver);
        let source = self.source_context_for_header(header).map_err(|source| {
            Self::failure(
                header.stream_index(),
                None,
                None,
                header.input_offset(),
                source,
            )
        })?;
        if let Some(source) = source {
            self.archive_set
                .begin_source(source)
                .map_err(|source| KvIrArchiveError {
                    stream_index: header.stream_index(),
                    unit_index: None,
                    event_index: None,
                    input_offset: header.input_offset(),
                    kind: KvIrArchiveErrorKind::ArchiveSet(source),
                })?;
        }
        self.schema_key_bytes = 0;
        self.active_stream = true;
        self.stats.streams = streams;
        Ok(())
    }

    fn source_context_for_header(
        &self,
        header: super::KvIrStreamHeader<'_>,
    ) -> Result<Option<ArchiveSourceContext>, KvIrArchiveFailure> {
        let Some(mut source) = self.source_context.clone() else {
            return Ok(None);
        };
        if !self
            .archive_set
            .options()
            .writer_options()
            .records_log_order()
        {
            return Ok(Some(source));
        }
        for (name, value) in user_defined_metadata(header)? {
            source
                .insert_field(name, value)
                .map_err(KvIrArchiveFailure::SourceContext)?;
        }
        Ok(Some(source))
    }

    fn insert_schema(
        &mut self,
        node: KvIrSchemaNode<'_>,
        resolver: Option<&KvIrTimestampResolver>,
    ) -> Result<(), KvIrArchiveError<S::Error, C::Error>> {
        self.require_active(
            node.stream_index(),
            Some(node.unit_index()),
            None,
            node.input_offset(),
        )?;
        let tree = match node.namespace() {
            KvIrNamespace::AutoGenerated => &mut self.auto_schema,
            KvIrNamespace::UserGenerated => &mut self.user_schema,
        };
        let actual_nodes = u64::try_from(tree.nodes.len()).map_err(|_| {
            Self::failure(
                node.stream_index(),
                Some(node.unit_index()),
                None,
                node.input_offset(),
                KvIrArchiveFailure::SizeOverflow,
            )
        })?;
        if actual_nodes > self.options.limits.schema_nodes_per_namespace {
            return Err(Self::failure(
                node.stream_index(),
                Some(node.unit_index()),
                None,
                node.input_offset(),
                KvIrArchiveFailure::Limit(KvIrArchiveLimitViolation::new(
                    KvIrArchiveLimitResource::SchemaNodesPerNamespace,
                    actual_nodes,
                    self.options.limits.schema_nodes_per_namespace,
                )),
            ));
        }
        if node.depth() > self.options.limits.nesting_depth {
            return Err(Self::failure(
                node.stream_index(),
                Some(node.unit_index()),
                None,
                node.input_offset(),
                KvIrArchiveFailure::Limit(KvIrArchiveLimitViolation::new(
                    KvIrArchiveLimitResource::NestingDepth,
                    node.depth(),
                    self.options.limits.nesting_depth,
                )),
            ));
        }
        let key_bytes = u64::try_from(node.key().len())
            .ok()
            .and_then(|value| self.schema_key_bytes.checked_add(value))
            .ok_or_else(|| {
                Self::failure(
                    node.stream_index(),
                    Some(node.unit_index()),
                    None,
                    node.input_offset(),
                    KvIrArchiveFailure::SizeOverflow,
                )
            })?;
        if key_bytes > self.options.limits.schema_key_bytes {
            return Err(Self::failure(
                node.stream_index(),
                Some(node.unit_index()),
                None,
                node.input_offset(),
                KvIrArchiveFailure::Limit(KvIrArchiveLimitViolation::new(
                    KvIrArchiveLimitResource::SchemaKeyBytes,
                    key_bytes,
                    self.options.limits.schema_key_bytes,
                )),
            ));
        }
        tree.insert(node, resolver).map_err(|source| {
            Self::failure(
                node.stream_index(),
                Some(node.unit_index()),
                None,
                node.input_offset(),
                source,
            )
        })?;
        self.schema_key_bytes = key_bytes;
        Ok(())
    }

    fn prepare_selection(&mut self, event: KvIrLogEvent<'_>) -> Result<(), KvIrArchiveFailure> {
        self.active_auto.clear();
        self.active_user.clear();
        self.next_selection_epoch();
        resize_selection(&mut self.selection_auto, self.auto_schema.nodes.len())?;
        resize_selection(&mut self.selection_user, self.user_schema.nodes.len())?;
        for (pair_index, pair) in event.pairs().enumerate() {
            self.select_pair(pair, pair_index)?;
        }
        order_active(
            &mut self.selection_auto,
            &mut self.active_auto,
            self.selection_epoch,
        )?;
        order_active(
            &mut self.selection_user,
            &mut self.active_user,
            self.selection_epoch,
        )?;
        Ok(())
    }

    fn select_pair(
        &mut self,
        pair: KvIrPair<'_>,
        pair_index: usize,
    ) -> Result<(), KvIrArchiveFailure> {
        let node_id =
            usize::try_from(pair.node_id()).map_err(|_| KvIrArchiveFailure::SizeOverflow)?;
        let selection_epoch = self.selection_epoch;
        let (tree, selection, active) = match pair.namespace() {
            KvIrNamespace::AutoGenerated => (
                &self.auto_schema,
                &mut self.selection_auto,
                &mut self.active_auto,
            ),
            KvIrNamespace::UserGenerated => (
                &self.user_schema,
                &mut self.selection_user,
                &mut self.active_user,
            ),
        };
        if node_id == 0 || node_id >= tree.nodes.len() {
            return Err(KvIrArchiveFailure::Invalid(
                KvIrArchiveInvalidData::MissingSchemaNode {
                    namespace: pair.namespace(),
                    node_id: pair.node_id(),
                },
            ));
        }
        if selection[node_id]
            .selected_pair_index(selection_epoch)
            .is_some()
        {
            return Err(KvIrArchiveFailure::Invalid(
                KvIrArchiveInvalidData::DuplicateEventNode {
                    namespace: pair.namespace(),
                    node_id: pair.node_id(),
                },
            ));
        }
        let mut ancestor = node_id;
        loop {
            if !selection[ancestor].include(selection_epoch) {
                break;
            }
            active
                .try_reserve(1)
                .map_err(|_| KvIrArchiveFailure::AllocationFailed {
                    resource: KvIrArchiveResource::SparseTraversal,
                    requested_additional: 1,
                })?;
            active.push(u32::try_from(ancestor).map_err(|_| KvIrArchiveFailure::SizeOverflow)?);
            if ancestor == 0 {
                break;
            }
            ancestor = tree.nodes[ancestor].parent();
        }
        selection[node_id].select(selection_epoch, pair_index)?;
        Ok(())
    }

    fn push_plan(&mut self, event: PlannedEvent) -> Result<(), KvIrArchiveFailure> {
        let actual = self
            .plan
            .len()
            .checked_add(1)
            .ok_or(KvIrArchiveFailure::SizeOverflow)?;
        let actual_u64 = u64::try_from(actual).map_err(|_| KvIrArchiveFailure::SizeOverflow)?;
        if actual_u64 > self.options.limits.record_events {
            return Err(KvIrArchiveFailure::Limit(KvIrArchiveLimitViolation::new(
                KvIrArchiveLimitResource::RecordEvents,
                actual_u64,
                self.options.limits.record_events,
            )));
        }
        self.plan
            .try_reserve(1)
            .map_err(|_| KvIrArchiveFailure::AllocationFailed {
                resource: KvIrArchiveResource::RecordPlan,
                requested_additional: 1,
            })?;
        self.plan.push(event);
        Ok(())
    }

    fn plan_namespace(
        &mut self,
        namespace: KvIrNamespace,
        event: KvIrLogEvent<'_>,
        resolver: Option<&KvIrTimestampResolver>,
    ) -> Result<(), KvIrArchiveFailure> {
        self.build_sparse_links(namespace)?;
        self.traversal.clear();
        let first_child = self
            .sparse_links
            .first()
            .map_or(Ok(None), |links| sparse_index(links.first_child))?;
        self.push_frame(WalkFrame {
            next_child: first_child,
            close_object: false,
        })?;
        while let Some(frame_index) = self.traversal.len().checked_sub(1) {
            let Some(active_index) = self.traversal[frame_index].next_child else {
                let close_object = self.traversal.pop().expect("frame exists").close_object;
                if close_object {
                    self.push_plan(PlannedEvent::ObjectEnd)?;
                }
                continue;
            };
            let node_id = usize::try_from(self.active(namespace)[active_index])
                .map_err(|_| KvIrArchiveFailure::SizeOverflow)?;
            let links = self.sparse_links[active_index];
            let next_sibling = sparse_index(links.next_sibling)?;
            self.traversal[frame_index].next_child = next_sibling;
            let selection = self.selection(namespace)[node_id];
            debug_assert!(selection.is_included(self.selection_epoch));
            if let Some(pair_index) = selection.selected_pair_index(self.selection_epoch) {
                let pair_index =
                    usize::try_from(pair_index).map_err(|_| KvIrArchiveFailure::SizeOverflow)?;
                self.plan_selected(namespace, node_id, pair_index, event, resolver)?;
                continue;
            }
            if self.tree(namespace).nodes[node_id].node_type != KvIrNodeType::Object {
                return Err(KvIrArchiveFailure::Invalid(
                    KvIrArchiveInvalidData::NonObjectAncestor {
                        namespace,
                        node_id: u32::try_from(node_id)
                            .map_err(|_| KvIrArchiveFailure::SizeOverflow)?,
                    },
                ));
            }
            let child = sparse_index(links.first_child)?;
            self.push_plan(PlannedEvent::ObjectStart { namespace, node_id })?;
            self.push_frame(WalkFrame {
                next_child: child,
                close_object: true,
            })?;
        }
        Ok(())
    }

    fn build_sparse_links(&mut self, namespace: KvIrNamespace) -> Result<(), KvIrArchiveFailure> {
        let (tree, active, selection) = match namespace {
            KvIrNamespace::AutoGenerated => {
                (&self.auto_schema, &self.active_auto, &self.selection_auto)
            }
            KvIrNamespace::UserGenerated => {
                (&self.user_schema, &self.active_user, &self.selection_user)
            }
        };
        if active.first().is_some_and(|node_id| *node_id != 0) {
            return Err(KvIrArchiveFailure::SizeOverflow);
        }
        self.sparse_links.clear();
        self.sparse_links.try_reserve(active.len()).map_err(|_| {
            KvIrArchiveFailure::AllocationFailed {
                resource: KvIrArchiveResource::SparseTraversal,
                requested_additional: active.len(),
            }
        })?;
        self.sparse_links.resize(active.len(), SparseLinks::EMPTY);
        for child_index in (1..active.len()).rev() {
            let node_id = usize::try_from(active[child_index])
                .map_err(|_| KvIrArchiveFailure::SizeOverflow)?;
            let parent_id = tree.nodes[node_id].parent();
            let parent_index = selection
                .get(parent_id)
                .and_then(|slot| slot.active_index(self.selection_epoch))
                .ok_or(KvIrArchiveFailure::SizeOverflow)?;
            let parent_index =
                usize::try_from(parent_index).map_err(|_| KvIrArchiveFailure::SizeOverflow)?;
            let child_index_u32 =
                u32::try_from(child_index).map_err(|_| KvIrArchiveFailure::SizeOverflow)?;
            if child_index_u32 == NO_SPARSE_INDEX {
                return Err(KvIrArchiveFailure::SizeOverflow);
            }
            self.sparse_links[child_index].next_sibling =
                self.sparse_links[parent_index].first_child;
            self.sparse_links[parent_index].first_child = child_index_u32;
        }
        Ok(())
    }

    fn push_frame(&mut self, frame: WalkFrame) -> Result<(), KvIrArchiveFailure> {
        self.traversal
            .try_reserve(1)
            .map_err(|_| KvIrArchiveFailure::AllocationFailed {
                resource: KvIrArchiveResource::TraversalStack,
                requested_additional: 1,
            })?;
        self.traversal.push(frame);
        Ok(())
    }

    fn plan_selected(
        &mut self,
        namespace: KvIrNamespace,
        node_id: usize,
        pair_index: usize,
        event: KvIrLogEvent<'_>,
        resolver: Option<&KvIrTimestampResolver>,
    ) -> Result<(), KvIrArchiveFailure> {
        let pair = event.pair(pair_index).ok_or(KvIrArchiveFailure::Invalid(
            KvIrArchiveInvalidData::MissingPlannedPair { pair_index },
        ))?;
        let mut reconstructed = None;
        let mut direct_encoded_text = false;
        let mut timestamp = None;
        let unstructured_array =
            self.tree(namespace).nodes[node_id].node_type == KvIrNodeType::UnstructuredArray;
        let timestamp_resolver = resolver.filter(|resolver| {
            self.tree(namespace).nodes[node_id].timestamp_prefix_length()
                == Some(resolver.path().components().len())
        });
        match pair.value().kind() {
            super::KvIrValueKind::Float { bits } if !f64::from_bits(bits).is_finite() => {
                return Err(KvIrArchiveFailure::Unsupported {
                    namespace,
                    node_id: pair.node_id(),
                    source: KvIrArchiveUnsupported::NonFiniteFloat { bits },
                });
            }
            super::KvIrValueKind::Integer(value) if timestamp_resolver.is_some() => {
                let span = self.reconstruct_integer(value.value())?;
                timestamp = Some(Self::resolve_integer_timestamp(
                    namespace,
                    pair.node_id(),
                    value.value(),
                )?);
                reconstructed = Some(span);
            }
            super::KvIrValueKind::Float { bits } if timestamp_resolver.is_some() => {
                let span = self.reconstruct_fixed_nine_float(f64::from_bits(bits))?;
                timestamp = Some(self.resolve_float_timestamp(namespace, pair.node_id(), span)?);
                reconstructed = Some(span);
            }
            super::KvIrValueKind::String(value) if timestamp_resolver.is_some() => {
                let span = self.reconstruct_quoted_string(value)?;
                timestamp = Some(Self::resolve_string_timestamp(
                    namespace,
                    pair.node_id(),
                    value,
                )?);
                reconstructed = Some(span);
            }
            super::KvIrValueKind::EncodedText(text)
                if timestamp_resolver.is_some() && !unstructured_array =>
            {
                let text = self.reconstruct_text(namespace, pair.node_id(), text, true)?;
                timestamp = Some(Self::resolve_string_timestamp(
                    namespace,
                    pair.node_id(),
                    &self.reconstructed[text.decoded.start..text.decoded.end],
                )?);
                reconstructed = Some(text.lexeme);
            }
            super::KvIrValueKind::EncodedText(text) if unstructured_array => {
                reconstructed = Some(
                    self.reconstruct_text(namespace, pair.node_id(), text, false)?
                        .lexeme,
                );
            }
            super::KvIrValueKind::EncodedText(text) => {
                self.validate_direct_encoded_text(namespace, pair.node_id(), text)?;
                direct_encoded_text = true;
            }
            super::KvIrValueKind::EmptyObject => {
                self.push_plan(PlannedEvent::ObjectStart { namespace, node_id })?;
                return self.push_plan(PlannedEvent::ObjectEnd);
            }
            _ => {}
        }
        self.push_plan(PlannedEvent::Value {
            namespace,
            node_id,
            pair_index,
            reconstructed,
            direct_encoded_text,
            unstructured_array,
            timestamp,
        })
    }

    fn reconstruct_integer(&mut self, value: i64) -> Result<ScratchSpan, KvIrArchiveFailure> {
        let start = self.reconstructed.len();
        let mut digits = [0_u8; 20];
        let mut cursor = digits.len();
        let mut magnitude = value.unsigned_abs();
        loop {
            cursor -= 1;
            digits[cursor] = b'0' + u8::try_from(magnitude % 10).expect("decimal digit fits u8");
            magnitude /= 10;
            if 0 == magnitude {
                break;
            }
        }
        if value.is_negative() {
            self.append_reconstructed(start, b"-")?;
        }
        self.append_reconstructed(start, &digits[cursor..])?;
        Ok(ScratchSpan {
            start,
            end: self.reconstructed.len(),
        })
    }

    fn reconstruct_fixed_nine_float(
        &mut self,
        value: f64,
    ) -> Result<ScratchSpan, KvIrArchiveFailure> {
        let mut buffer = FixedFloatBuffer::new();
        fmt::write(&mut buffer, format_args!("{value:.9}"))
            .map_err(|_| KvIrArchiveFailure::SizeOverflow)?;
        let start = self.reconstructed.len();
        self.append_reconstructed(start, buffer.as_bytes())?;
        Ok(ScratchSpan {
            start,
            end: self.reconstructed.len(),
        })
    }

    fn reconstruct_quoted_string(
        &mut self,
        value: &[u8],
    ) -> Result<ScratchSpan, KvIrArchiveFailure> {
        let start = self.reconstructed.len();
        self.append_reconstructed(start, b"\"")?;
        self.append_reconstructed(start, value)?;
        self.append_reconstructed(start, b"\"")?;
        Ok(ScratchSpan {
            start,
            end: self.reconstructed.len(),
        })
    }

    fn resolve_integer_timestamp(
        namespace: KvIrNamespace,
        node_id: u32,
        value: i64,
    ) -> Result<PlannedTimestamp, KvIrArchiveFailure> {
        KvIrTimestampResolver::resolve_integer(value)
            .map(planned_timestamp)
            .map_err(|source| timestamp_failure(namespace, node_id, source))
    }

    fn resolve_float_timestamp(
        &self,
        namespace: KvIrNamespace,
        node_id: u32,
        lexeme: ScratchSpan,
    ) -> Result<PlannedTimestamp, KvIrArchiveFailure> {
        let lexeme = str::from_utf8(&self.reconstructed[lexeme.start..lexeme.end])
            .expect("fixed float formatting is UTF-8");
        KvIrTimestampResolver::resolve_fixed_nine_float(lexeme)
            .map(planned_timestamp)
            .map_err(|source| timestamp_failure(namespace, node_id, source))
    }

    fn resolve_string_timestamp(
        namespace: KvIrNamespace,
        node_id: u32,
        value: &[u8],
    ) -> Result<PlannedTimestamp, KvIrArchiveFailure> {
        KvIrTimestampResolver::resolve_string(value)
            .map(planned_timestamp)
            .map_err(|source| timestamp_failure(namespace, node_id, source))
    }

    fn validate_direct_encoded_text(
        &mut self,
        namespace: KvIrNamespace,
        node_id: u32,
        text: KvIrEncodedText<'_>,
    ) -> Result<(), KvIrArchiveFailure> {
        let unsupported = |source| KvIrArchiveFailure::Unsupported {
            namespace,
            node_id,
            source,
        };
        let mut value_len = 0_usize;
        let mut encoded = text.encoded_variables();
        let mut dictionaries = text.dictionary_variables();
        let logtype = text.logtype();
        let mut position = 0_usize;
        while position < logtype.len() {
            let byte = logtype[position];
            if !matches!(byte, b'\\' | 0x11..=0x13) {
                let literal_start = position;
                position = position
                    .checked_add(1)
                    .ok_or(KvIrArchiveFailure::SizeOverflow)?;
                while position < logtype.len() && !matches!(logtype[position], b'\\' | 0x11..=0x13)
                {
                    position += 1;
                }
                let additional = position
                    .checked_sub(literal_start)
                    .ok_or(KvIrArchiveFailure::SizeOverflow)?;
                value_len = self.check_direct_reconstructed_growth(value_len, additional)?;
                continue;
            }
            if byte == b'\\' {
                position = position
                    .checked_add(1)
                    .ok_or(KvIrArchiveFailure::SizeOverflow)?;
                if logtype.get(position).is_none() {
                    return Err(unsupported(KvIrArchiveUnsupported::TrailingEscape));
                }
                value_len = self.check_direct_reconstructed_growth(value_len, 1)?;
            } else if byte == 0x12 {
                let additional = dictionaries
                    .next()
                    .ok_or_else(|| unsupported(KvIrArchiveUnsupported::MissingDictionaryVariable))?
                    .len();
                value_len = self.check_direct_reconstructed_growth(value_len, additional)?;
            } else {
                let value = encoded
                    .next()
                    .ok_or_else(|| unsupported(KvIrArchiveUnsupported::MissingEncodedVariable))?;
                if byte == 0x11 {
                    let (negative, digit_count) =
                        encoded_integer_parts(text.encoding(), value).map_err(unsupported)?;
                    if negative {
                        value_len = self.check_direct_reconstructed_growth(value_len, 1)?;
                    }
                    value_len = self.check_direct_reconstructed_growth(value_len, digit_count)?;
                } else {
                    let properties =
                        decode_float_properties(text.encoding(), value).map_err(unsupported)?;
                    let additional = usize::from(properties.digit_count)
                        .checked_add(1)
                        .and_then(|value| value.checked_add(usize::from(properties.negative)))
                        .ok_or(KvIrArchiveFailure::SizeOverflow)?;
                    value_len = self.check_direct_reconstructed_growth(value_len, additional)?;
                    validate_encoded_float_digit_count(properties).map_err(unsupported)?;
                }
            }
            position = position
                .checked_add(1)
                .ok_or(KvIrArchiveFailure::SizeOverflow)?;
        }
        self.direct_reconstructed_bytes = self
            .direct_reconstructed_bytes
            .checked_add(value_len)
            .ok_or(KvIrArchiveFailure::SizeOverflow)?;
        Ok(())
    }

    fn check_direct_reconstructed_growth(
        &self,
        current_value_len: usize,
        additional: usize,
    ) -> Result<usize, KvIrArchiveFailure> {
        let value_len = current_value_len
            .checked_add(additional)
            .ok_or(KvIrArchiveFailure::SizeOverflow)?;
        check_limit(
            KvIrArchiveLimitResource::ReconstructedValueBytes,
            value_len,
            self.options.limits.reconstructed_value_bytes,
        )?;
        let record_len = self
            .reconstructed
            .len()
            .checked_add(self.direct_reconstructed_bytes)
            .and_then(|value| value.checked_add(value_len))
            .ok_or(KvIrArchiveFailure::SizeOverflow)?;
        check_limit(
            KvIrArchiveLimitResource::ReconstructedRecordBytes,
            record_len,
            self.options.limits.reconstructed_record_bytes,
        )?;
        Ok(value_len)
    }

    fn reconstruct_text(
        &mut self,
        namespace: KvIrNamespace,
        node_id: u32,
        text: KvIrEncodedText<'_>,
        quoted: bool,
    ) -> Result<ReconstructedText, KvIrArchiveFailure> {
        let start = self.reconstructed.len();
        if quoted {
            self.append_reconstructed(start, b"\"")?;
        }
        let decoded_start = self.reconstructed.len();
        let mut encoded = text.encoded_variables();
        let mut dictionaries = text.dictionary_variables();
        let logtype = text.logtype();
        let mut position = 0;
        while position < logtype.len() {
            let byte = logtype[position];
            if !matches!(byte, b'\\' | 0x11..=0x13) {
                let literal_start = position;
                position += 1;
                while position < logtype.len() && !matches!(logtype[position], b'\\' | 0x11..=0x13)
                {
                    position += 1;
                }
                self.append_reconstructed(start, &logtype[literal_start..position])?;
                continue;
            }
            if byte == b'\\' {
                position = position
                    .checked_add(1)
                    .ok_or(KvIrArchiveFailure::SizeOverflow)?;
                let Some(escaped) = logtype.get(position).copied() else {
                    return Err(KvIrArchiveFailure::Unsupported {
                        namespace,
                        node_id,
                        source: KvIrArchiveUnsupported::TrailingEscape,
                    });
                };
                self.append_reconstructed(start, &[escaped])?;
            } else if byte == 0x12 {
                let value = dictionaries.next().ok_or(KvIrArchiveFailure::Unsupported {
                    namespace,
                    node_id,
                    source: KvIrArchiveUnsupported::MissingDictionaryVariable,
                })?;
                self.append_reconstructed(start, value)?;
            } else {
                let value = encoded.next().ok_or(KvIrArchiveFailure::Unsupported {
                    namespace,
                    node_id,
                    source: KvIrArchiveUnsupported::MissingEncodedVariable,
                })?;
                if byte == 0x11 {
                    self.append_encoded_integer(start, text.encoding(), value, namespace, node_id)?;
                } else {
                    self.append_encoded_float(start, text.encoding(), value, namespace, node_id)?;
                }
            }
            position = position
                .checked_add(1)
                .ok_or(KvIrArchiveFailure::SizeOverflow)?;
        }
        let decoded_end = self.reconstructed.len();
        if quoted {
            self.append_reconstructed(start, b"\"")?;
        }
        Ok(ReconstructedText {
            lexeme: ScratchSpan {
                start,
                end: self.reconstructed.len(),
            },
            decoded: ScratchSpan {
                start: decoded_start,
                end: decoded_end,
            },
        })
    }

    fn append_encoded_integer(
        &mut self,
        value_start: usize,
        encoding: KvIrEncoding,
        value: KvIrEncodedVariable,
        namespace: KvIrNamespace,
        node_id: u32,
    ) -> Result<(), KvIrArchiveFailure> {
        let integer = match (encoding, value) {
            (KvIrEncoding::FourByte, KvIrEncodedVariable::FourByte(value)) => i64::from(value),
            (KvIrEncoding::EightByte, KvIrEncodedVariable::EightByte(value)) => value,
            _ => {
                return Err(KvIrArchiveFailure::Unsupported {
                    namespace,
                    node_id,
                    source: KvIrArchiveUnsupported::EncodedVariableWidthMismatch,
                });
            }
        };
        let mut digits = [0_u8; 20];
        let mut cursor = digits.len();
        let mut magnitude = integer.unsigned_abs();
        loop {
            cursor -= 1;
            digits[cursor] = b'0' + u8::try_from(magnitude % 10).expect("decimal digit fits u8");
            magnitude /= 10;
            if magnitude == 0 {
                break;
            }
        }
        if integer.is_negative() {
            self.append_reconstructed(value_start, b"-")?;
        }
        self.append_reconstructed(value_start, &digits[cursor..])
    }

    fn append_encoded_float(
        &mut self,
        value_start: usize,
        encoding: KvIrEncoding,
        value: KvIrEncodedVariable,
        namespace: KvIrNamespace,
        node_id: u32,
    ) -> Result<(), KvIrArchiveFailure> {
        let properties = decode_float_properties(encoding, value).map_err(|source| {
            KvIrArchiveFailure::Unsupported {
                namespace,
                node_id,
                source,
            }
        })?;
        let sign_bytes = usize::from(properties.negative);
        let output_len = usize::from(properties.digit_count)
            .checked_add(1)
            .and_then(|value| value.checked_add(sign_bytes))
            .ok_or(KvIrArchiveFailure::SizeOverflow)?;
        let output_start = self.reconstructed.len();
        self.append_repeated_zeroes(value_start, output_len)?;
        if properties.negative {
            self.reconstructed[output_start] = b'-';
        }
        let decimal_index = output_start
            .checked_add(sign_bytes)
            .and_then(|value| value.checked_add(usize::from(properties.digit_count)))
            .and_then(|value| value.checked_sub(usize::from(properties.decimal_position)))
            .ok_or(KvIrArchiveFailure::SizeOverflow)?;
        self.reconstructed[decimal_index] = b'.';
        let digit_floor = output_start
            .checked_add(sign_bytes)
            .ok_or(KvIrArchiveFailure::SizeOverflow)?;
        let mut cursor = self.reconstructed.len();
        let mut digits = properties.digits;
        while digits != 0 {
            cursor = cursor
                .checked_sub(1)
                .ok_or(KvIrArchiveFailure::Unsupported {
                    namespace,
                    node_id,
                    source: KvIrArchiveUnsupported::EncodedFloatDigitCount,
                })?;
            if cursor == decimal_index {
                cursor = cursor
                    .checked_sub(1)
                    .ok_or(KvIrArchiveFailure::Unsupported {
                        namespace,
                        node_id,
                        source: KvIrArchiveUnsupported::EncodedFloatDigitCount,
                    })?;
            }
            if cursor < digit_floor {
                return Err(KvIrArchiveFailure::Unsupported {
                    namespace,
                    node_id,
                    source: KvIrArchiveUnsupported::EncodedFloatDigitCount,
                });
            }
            self.reconstructed[cursor] =
                b'0' + u8::try_from(digits % 10).expect("decimal digit fits u8");
            digits /= 10;
        }
        Ok(())
    }

    fn append_repeated_zeroes(
        &mut self,
        value_start: usize,
        count: usize,
    ) -> Result<(), KvIrArchiveFailure> {
        self.check_reconstructed_growth(value_start, count)?;
        self.reconstructed.try_reserve(count).map_err(|_| {
            KvIrArchiveFailure::AllocationFailed {
                resource: KvIrArchiveResource::ReconstructedText,
                requested_additional: count,
            }
        })?;
        let end = self
            .reconstructed
            .len()
            .checked_add(count)
            .ok_or(KvIrArchiveFailure::SizeOverflow)?;
        self.reconstructed.resize(end, b'0');
        Ok(())
    }

    fn append_reconstructed(
        &mut self,
        value_start: usize,
        bytes: &[u8],
    ) -> Result<(), KvIrArchiveFailure> {
        self.check_reconstructed_growth(value_start, bytes.len())?;
        self.reconstructed.try_reserve(bytes.len()).map_err(|_| {
            KvIrArchiveFailure::AllocationFailed {
                resource: KvIrArchiveResource::ReconstructedText,
                requested_additional: bytes.len(),
            }
        })?;
        self.reconstructed.extend_from_slice(bytes);
        Ok(())
    }

    fn check_reconstructed_growth(
        &self,
        value_start: usize,
        additional: usize,
    ) -> Result<(), KvIrArchiveFailure> {
        let new_len = self
            .reconstructed
            .len()
            .checked_add(additional)
            .ok_or(KvIrArchiveFailure::SizeOverflow)?;
        let value_len = new_len
            .checked_sub(value_start)
            .ok_or(KvIrArchiveFailure::SizeOverflow)?;
        check_limit(
            KvIrArchiveLimitResource::ReconstructedValueBytes,
            value_len,
            self.options.limits.reconstructed_value_bytes,
        )?;
        check_limit(
            KvIrArchiveLimitResource::ReconstructedRecordBytes,
            new_len
                .checked_add(self.direct_reconstructed_bytes)
                .ok_or(KvIrArchiveFailure::SizeOverflow)?,
            self.options.limits.reconstructed_record_bytes,
        )
    }

    #[allow(clippy::too_many_lines)]
    fn append_event(
        &mut self,
        event: KvIrLogEvent<'_>,
        resolver: Option<&KvIrTimestampResolver>,
    ) -> Result<(), KvIrArchiveError<S::Error, C::Error>> {
        self.require_active(
            event.stream_index(),
            Some(event.unit_index()),
            Some(event.event_index()),
            event.input_offset(),
        )?;
        if event.input_offset() < self.accounted_input_bytes {
            return Err(Self::failure(
                event.stream_index(),
                Some(event.unit_index()),
                Some(event.event_index()),
                event.input_offset(),
                KvIrArchiveFailure::Invalid(KvIrArchiveInvalidData::NonContiguousInput),
            ));
        }
        let input_end =
            item_end(event.input_offset(), event.raw_unit().len()).map_err(|source| {
                Self::failure(
                    event.stream_index(),
                    Some(event.unit_index()),
                    Some(event.event_index()),
                    event.input_offset(),
                    source,
                )
            })?;
        let source_bytes = input_end
            .checked_sub(self.accounted_input_bytes)
            .ok_or_else(|| {
                Self::failure(
                    event.stream_index(),
                    Some(event.unit_index()),
                    Some(event.event_index()),
                    event.input_offset(),
                    KvIrArchiveFailure::Invalid(KvIrArchiveInvalidData::NonContiguousInput),
                )
            })?;
        let records = self.stats.records.checked_add(1).ok_or_else(|| {
            Self::failure(
                event.stream_index(),
                Some(event.unit_index()),
                Some(event.event_index()),
                event.input_offset(),
                KvIrArchiveFailure::SizeOverflow,
            )
        })?;

        self.plan.clear();
        self.reconstructed.clear();
        self.direct_reconstructed_bytes = 0;
        self.prepare_selection(event).map_err(|source| {
            Self::failure(
                event.stream_index(),
                Some(event.unit_index()),
                Some(event.event_index()),
                event.input_offset(),
                source,
            )
        })?;
        self.plan_namespace(KvIrNamespace::AutoGenerated, event, resolver)
            .and_then(|()| self.plan_namespace(KvIrNamespace::UserGenerated, event, resolver))
            .map_err(|source| {
                Self::failure(
                    event.stream_index(),
                    Some(event.unit_index()),
                    Some(event.event_index()),
                    event.input_offset(),
                    source,
                )
            })?;

        let events = PlannedRecordEvents {
            event: &event,
            auto_schema: &self.auto_schema.nodes,
            user_schema: &self.user_schema.nodes,
            plan: &self.plan,
            reconstructed: &self.reconstructed,
            timestamp_resolver: resolver,
        };
        let result = self
            .archive_set
            .try_append_replayable_record_events_with_source_bytes(events, source_bytes);
        let committed = match &result {
            Ok(()) => true,
            Err(source) => source.record_committed(),
        };
        if committed {
            self.accounted_input_bytes = input_end;
            self.stats.source_bytes = input_end;
            self.stats.records = records;
        }
        result.map_err(|source| {
            let source = match source {
                ArchiveSetAppendError::Source { source, .. } => match source {},
                ArchiveSetAppendError::ArchiveSet(source) => source,
            };
            KvIrArchiveError {
                stream_index: event.stream_index(),
                unit_index: Some(event.unit_index()),
                event_index: Some(event.event_index()),
                input_offset: event.input_offset(),
                kind: KvIrArchiveErrorKind::ArchiveSet(source),
            }
        })
    }

    fn end_stream(
        &mut self,
        end: super::KvIrStreamEnd<'_>,
    ) -> Result<(), KvIrArchiveError<S::Error, C::Error>> {
        self.require_active(
            end.stream_index(),
            Some(end.unit_index()),
            None,
            end.input_offset(),
        )?;
        if end.input_offset() < self.accounted_input_bytes {
            return Err(Self::failure(
                end.stream_index(),
                Some(end.unit_index()),
                None,
                end.input_offset(),
                KvIrArchiveFailure::Invalid(KvIrArchiveInvalidData::NonContiguousInput),
            ));
        }
        let input_end = item_end(end.input_offset(), end.raw_unit().len()).map_err(|source| {
            Self::failure(
                end.stream_index(),
                Some(end.unit_index()),
                None,
                end.input_offset(),
                source,
            )
        })?;
        let source_bytes = input_end
            .checked_sub(self.accounted_input_bytes)
            .ok_or_else(|| {
                Self::failure(
                    end.stream_index(),
                    Some(end.unit_index()),
                    None,
                    end.input_offset(),
                    KvIrArchiveFailure::Invalid(KvIrArchiveInvalidData::NonContiguousInput),
                )
            })?;
        let result = if self.source_context.is_some() {
            self.archive_set
                .end_source_with_uncompressed_bytes(source_bytes)
        } else {
            self.archive_set.add_uncompressed_bytes(source_bytes)
        };
        result.map_err(|source| KvIrArchiveError {
            stream_index: end.stream_index(),
            unit_index: Some(end.unit_index()),
            event_index: None,
            input_offset: end.input_offset(),
            kind: KvIrArchiveErrorKind::ArchiveSet(source),
        })?;
        self.accounted_input_bytes = input_end;
        self.stats.source_bytes = input_end;
        self.active_stream = false;
        Ok(())
    }

    fn write_item_with_resolver(
        &mut self,
        item: KvIrItem<'_>,
        resolver: Option<&KvIrTimestampResolver>,
    ) -> Result<(), KvIrArchiveError<S::Error, C::Error>> {
        match item {
            KvIrItem::StreamStart(header) => self.start_stream(header, resolver),
            KvIrItem::SchemaNode(node) => self.insert_schema(node, resolver),
            KvIrItem::LogEvent(event) => self.append_event(event, resolver),
            KvIrItem::UtcOffsetChange(offset) => self.require_active(
                offset.stream_index(),
                Some(offset.unit_index()),
                None,
                offset.input_offset(),
            ),
            KvIrItem::StreamEnd(end) => self.end_stream(end),
        }
    }

    const fn tree(&self, namespace: KvIrNamespace) -> &SchemaTree {
        match namespace {
            KvIrNamespace::AutoGenerated => &self.auto_schema,
            KvIrNamespace::UserGenerated => &self.user_schema,
        }
    }

    fn selection(&self, namespace: KvIrNamespace) -> &[SelectionSlot] {
        match namespace {
            KvIrNamespace::AutoGenerated => &self.selection_auto,
            KvIrNamespace::UserGenerated => &self.selection_user,
        }
    }

    fn active(&self, namespace: KvIrNamespace) -> &[u32] {
        match namespace {
            KvIrNamespace::AutoGenerated => &self.active_auto,
            KvIrNamespace::UserGenerated => &self.active_user,
        }
    }

    fn next_selection_epoch(&mut self) {
        self.selection_epoch = self.selection_epoch.wrapping_add(1);
        if self.selection_epoch == 0 {
            self.selection_auto.fill(SelectionSlot::default());
            self.selection_user.fill(SelectionSlot::default());
            self.selection_epoch = 1;
        }
    }
}

impl<'resolver, 'archive, S, C> TimestampedKvIrArchiveSetSink<'resolver, 'archive, S, C>
where
    S: FinalizedArchiveSink,
    C: ArchiveSetStatsCallback,
{
    /// Returns the immutable timestamp resolver.
    #[must_use]
    pub const fn resolver(&self) -> &'resolver KvIrTimestampResolver {
        self.resolver
    }

    /// Returns the immutable adapter options.
    #[must_use]
    pub const fn options(&self) -> KvIrArchiveOptions {
        self.inner.options()
    }

    /// Returns successfully committed counters.
    #[must_use]
    pub const fn stats(&self) -> KvIrArchiveStats {
        self.inner.stats()
    }

    /// Returns exact source bytes already attributed to the archive set.
    #[must_use]
    pub const fn accounted_input_bytes(&self) -> u64 {
        self.inner.accounted_input_bytes()
    }

    /// Returns the underlying archive-set session.
    #[must_use = "the borrowed archive-set session should be used"]
    pub const fn archive_set(&self) -> &ArchiveSetWriter<S, C> {
        self.inner.archive_set()
    }

    /// Returns the underlying archive-set session mutably.
    pub const fn archive_set_mut(&mut self) -> &mut ArchiveSetWriter<S, C> {
        self.inner.archive_set_mut()
    }

    /// Removes timestamp recognition and returns the ordinary adapter.
    #[must_use]
    pub fn without_timestamp_resolver(mut self) -> KvIrArchiveSetSink<'archive, S, C> {
        self.inner.configure_timestamp(None);
        self.inner
    }

    /// Consumes the adapter and returns the underlying archive-set session.
    ///
    /// If decoding stopped inside a source-aware stream, this leaves its source context open so
    /// the caller can explicitly recover or abandon the archive-set session.
    #[must_use = "the recovered archive-set session should be used"]
    pub fn into_inner(self) -> &'archive mut ArchiveSetWriter<S, C> {
        self.inner.into_inner()
    }
}

impl<S, C> KvIrSink for KvIrArchiveSetSink<'_, S, C>
where
    S: FinalizedArchiveSink,
    C: ArchiveSetStatsCallback,
{
    type Error = KvIrArchiveError<S::Error, C::Error>;

    fn write_item(&mut self, item: KvIrItem<'_>) -> Result<(), Self::Error> {
        self.write_item_with_resolver(item, None)
    }
}

impl<S, C> KvIrSink for TimestampedKvIrArchiveSetSink<'_, '_, S, C>
where
    S: FinalizedArchiveSink,
    C: ArchiveSetStatsCallback,
{
    type Error = KvIrArchiveError<S::Error, C::Error>;

    fn write_item(&mut self, item: KvIrItem<'_>) -> Result<(), Self::Error> {
        self.inner
            .write_item_with_resolver(item, Some(self.resolver))
    }
}

#[derive(Clone, Copy)]
struct PlannedRecordEvents<'record> {
    event: &'record KvIrLogEvent<'record>,
    auto_schema: &'record [SchemaNode],
    user_schema: &'record [SchemaNode],
    plan: &'record [PlannedEvent],
    reconstructed: &'record [u8],
    timestamp_resolver: Option<&'record KvIrTimestampResolver>,
}

impl<'record> ReplayableRecordEventSource<'record> for PlannedRecordEvents<'record> {
    type Error = Infallible;

    fn consume<C>(self, consumer: &mut C) -> Result<(), RecordEventAppendError<Self::Error>>
    where
        C: RecordEventConsumer<'record>, {
        let mut current_namespace = None;
        for planned in self.plan.iter().copied() {
            let namespace = match planned {
                PlannedEvent::ObjectStart { namespace, .. }
                | PlannedEvent::Value { namespace, .. } => namespace,
                PlannedEvent::ObjectEnd => current_namespace
                    .expect("an object-end plan follows a namespaced object-start plan"),
            };
            if current_namespace != Some(namespace) {
                consumer.kv_ir_namespace(namespace)?;
                current_namespace = Some(namespace);
            }
            match planned {
                PlannedEvent::ObjectStart { namespace, node_id } => {
                    consumer.object_start(self.node(namespace, node_id).key.as_slice())?;
                }
                PlannedEvent::ObjectEnd => consumer.object_end()?,
                PlannedEvent::Value {
                    namespace,
                    node_id,
                    pair_index,
                    reconstructed,
                    direct_encoded_text,
                    unstructured_array,
                    timestamp,
                } => {
                    let key = self.node(namespace, node_id).key.as_slice();
                    if direct_encoded_text {
                        debug_assert!(reconstructed.is_none());
                        debug_assert!(!unstructured_array);
                        debug_assert!(timestamp.is_none());
                        consumer.kv_ir_encoded_text(key, self.event, pair_index)?;
                        continue;
                    }
                    let value =
                        self.value(pair_index, reconstructed, unstructured_array, timestamp);
                    consumer.value(FieldRef::new(key, value))?;
                }
            }
        }
        Ok(())
    }

    fn supports_cached_layout_proof(&self) -> bool {
        true
    }
}

impl<'record> PlannedRecordEvents<'record> {
    const fn node(&self, namespace: KvIrNamespace, node_id: usize) -> &'record SchemaNode {
        match namespace {
            KvIrNamespace::AutoGenerated => &self.auto_schema[node_id],
            KvIrNamespace::UserGenerated => &self.user_schema[node_id],
        }
    }

    fn value(
        self,
        pair_index: usize,
        reconstructed: Option<ScratchSpan>,
        unstructured_array: bool,
        timestamp: Option<PlannedTimestamp>,
    ) -> ValueRef<'record> {
        let pair = self
            .event
            .pair(pair_index)
            .expect("event plan only retains validated pair indices");
        timestamp.map_or_else(
            || {
                reconstructed.map_or_else(
                    || value_ref(pair),
                    |span| {
                        let bytes = &self.reconstructed[span.start..span.end];
                        if unstructured_array {
                            ValueRef::UnstructuredArray(UnstructuredArrayRef::new(bytes))
                        } else {
                            ValueRef::String(bytes)
                        }
                    },
                )
            },
            |timestamp| {
                let span = reconstructed.expect("timestamp plans retain their exact lexeme");
                let lexeme = str::from_utf8(&self.reconstructed[span.start..span.end])
                    .expect("resolved timestamp lexemes are UTF-8");
                let resolver = self
                    .timestamp_resolver
                    .expect("timestamp plans retain their resolver");
                ValueRef::Timestamp(TimestampRef::new(
                    timestamp.epoch_nanoseconds,
                    lexeme,
                    timestamp.pattern,
                    resolver.path().descriptor(),
                ))
            },
        )
    }
}

fn value_ref(pair: KvIrPair<'_>) -> ValueRef<'_> {
    match pair.value().kind() {
        super::KvIrValueKind::Integer(value) => ValueRef::I64(value.value()),
        super::KvIrValueKind::Float { bits } => ValueRef::F64(f64::from_bits(bits)),
        super::KvIrValueKind::Boolean(value) => ValueRef::Bool(value),
        super::KvIrValueKind::String(value) => ValueRef::String(value),
        super::KvIrValueKind::Null => ValueRef::Null,
        super::KvIrValueKind::EncodedText(_) | super::KvIrValueKind::EmptyObject => {
            unreachable!("encoded text and empty objects have explicit plan forms")
        }
    }
}

const fn planned_timestamp(value: KvIrResolvedTimestamp) -> PlannedTimestamp {
    PlannedTimestamp {
        epoch_nanoseconds: value.epoch_nanoseconds,
        pattern: value.pattern,
    }
}

const fn timestamp_failure(
    namespace: KvIrNamespace,
    node_id: u32,
    source: JsonTimestampError,
) -> KvIrArchiveFailure {
    KvIrArchiveFailure::Timestamp {
        namespace,
        node_id,
        source,
    }
}

fn resize_selection(
    selection: &mut Vec<SelectionSlot>,
    len: usize,
) -> Result<(), KvIrArchiveFailure> {
    if selection.len() < len {
        let additional = len - selection.len();
        selection
            .try_reserve(additional)
            .map_err(|_| KvIrArchiveFailure::AllocationFailed {
                resource: KvIrArchiveResource::SelectionMap,
                requested_additional: additional,
            })?;
        selection.resize(len, SelectionSlot::default());
    } else {
        selection.truncate(len);
    }
    Ok(())
}

/// Orders active nodes by schema ID and records their sparse positions for constant-time linking.
///
/// Sparse events sort only the nodes they touch. Once at least one eighth of a schema is active,
/// scanning the epoch-stamped selection map is cheaper and restores linear behavior for dense
/// events. Clearing `active` retains capacity for exactly the same number of included nodes, so
/// the dense path cannot allocate while rebuilding it.
fn order_active(
    selection: &mut [SelectionSlot],
    active: &mut Vec<u32>,
    epoch: u32,
) -> Result<(), KvIrArchiveFailure> {
    const DENSE_SCAN_RATIO: usize = 8;

    if active.len().saturating_mul(DENSE_SCAN_RATIO) >= selection.len() {
        let included_count = active.len();
        active.clear();
        for (node_id, slot) in selection.iter().enumerate() {
            if slot.is_included(epoch) {
                active.push(u32::try_from(node_id).map_err(|_| KvIrArchiveFailure::SizeOverflow)?);
            }
        }
        debug_assert_eq!(included_count, active.len());
    } else {
        active.sort_unstable();
    }

    for (active_index, node_id) in active.iter().copied().enumerate() {
        let node_id = usize::try_from(node_id).map_err(|_| KvIrArchiveFailure::SizeOverflow)?;
        selection
            .get_mut(node_id)
            .ok_or(KvIrArchiveFailure::SizeOverflow)?
            .set_active_index(epoch, active_index)?;
    }
    Ok(())
}

fn sparse_index(index: u32) -> Result<Option<usize>, KvIrArchiveFailure> {
    if index == NO_SPARSE_INDEX {
        Ok(None)
    } else {
        usize::try_from(index)
            .map(Some)
            .map_err(|_| KvIrArchiveFailure::SizeOverflow)
    }
}

fn encoded_integer_parts(
    encoding: KvIrEncoding,
    value: KvIrEncodedVariable,
) -> Result<(bool, usize), KvIrArchiveUnsupported> {
    let value = match (encoding, value) {
        (KvIrEncoding::FourByte, KvIrEncodedVariable::FourByte(value)) => i64::from(value),
        (KvIrEncoding::EightByte, KvIrEncodedVariable::EightByte(value)) => value,
        _ => return Err(KvIrArchiveUnsupported::EncodedVariableWidthMismatch),
    };
    let mut magnitude = value.unsigned_abs();
    let mut digits = 1_usize;
    while magnitude >= 10 {
        magnitude /= 10;
        digits += 1;
    }
    Ok((value.is_negative(), digits))
}

fn validate_encoded_float_digit_count(
    properties: FloatProperties,
) -> Result<(), KvIrArchiveUnsupported> {
    let mut remaining = properties.digits;
    let mut populated_digits = 0_u8;
    while remaining != 0 {
        populated_digits = populated_digits
            .checked_add(1)
            .ok_or(KvIrArchiveUnsupported::EncodedFloatDigitCount)?;
        remaining /= 10;
    }
    if populated_digits > properties.digit_count {
        return Err(KvIrArchiveUnsupported::EncodedFloatDigitCount);
    }
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
) -> Result<FloatProperties, KvIrArchiveUnsupported> {
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
                return Err(KvIrArchiveUnsupported::EncodedFloatDigitsTooLarge);
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
        _ => return Err(KvIrArchiveUnsupported::EncodedVariableWidthMismatch),
    };
    if properties.decimal_position > properties.digit_count {
        return Err(KvIrArchiveUnsupported::EncodedFloatDecimalPosition);
    }
    Ok(properties)
}

fn check_limit(
    resource: KvIrArchiveLimitResource,
    actual: usize,
    limit: u64,
) -> Result<(), KvIrArchiveFailure> {
    let actual = u64::try_from(actual).map_err(|_| KvIrArchiveFailure::SizeOverflow)?;
    if actual > limit {
        Err(KvIrArchiveFailure::Limit(KvIrArchiveLimitViolation::new(
            resource, actual, limit,
        )))
    } else {
        Ok(())
    }
}

fn item_end(input_offset: u64, raw_len: usize) -> Result<u64, KvIrArchiveFailure> {
    let raw_len = u64::try_from(raw_len).map_err(|_| KvIrArchiveFailure::SizeOverflow)?;
    input_offset
        .checked_add(raw_len)
        .ok_or(KvIrArchiveFailure::SizeOverflow)
}

#[derive(Debug)]
enum MetadataFrame {
    Object {
        fields: BTreeMap<String, RangeIndexValue>,
        pending_key: Option<String>,
    },
    Array(Vec<RangeIndexValue>),
}

fn user_defined_metadata(
    header: super::KvIrStreamHeader<'_>,
) -> Result<BTreeMap<String, RangeIndexValue>, KvIrArchiveFailure> {
    let value = metadata_value(header.metadata_events())?;
    let RangeIndexValue::Object(mut root) = value else {
        return Err(metadata_traversal());
    };
    match root.remove("USER_DEFINED_METADATA") {
        None => Ok(BTreeMap::new()),
        Some(RangeIndexValue::Object(fields)) => Ok(fields),
        Some(_) => Err(metadata_traversal()),
    }
}

fn metadata_value<'metadata>(
    events: impl IntoIterator<Item = JsonEvent<'metadata>>,
) -> Result<RangeIndexValue, KvIrArchiveFailure> {
    let mut stack = Vec::new();
    let mut root = None;
    for event in events {
        match event {
            JsonEvent::ObjectStart => stack.push(MetadataFrame::Object {
                fields: BTreeMap::new(),
                pending_key: None,
            }),
            JsonEvent::ArrayStart(_) => stack.push(MetadataFrame::Array(Vec::new())),
            JsonEvent::ObjectEnd => {
                let Some(MetadataFrame::Object {
                    fields,
                    pending_key: None,
                }) = stack.pop()
                else {
                    return Err(metadata_traversal());
                };
                push_metadata_value(&mut stack, &mut root, RangeIndexValue::Object(fields))?;
            }
            JsonEvent::ArrayEnd => {
                let Some(MetadataFrame::Array(values)) = stack.pop() else {
                    return Err(metadata_traversal());
                };
                push_metadata_value(&mut stack, &mut root, RangeIndexValue::Array(values))?;
            }
            JsonEvent::ObjectKey(key) => {
                let Some(MetadataFrame::Object { pending_key, .. }) = stack.last_mut() else {
                    return Err(metadata_traversal());
                };
                if pending_key.replace(key.decoded().to_owned()).is_some() {
                    return Err(metadata_traversal());
                }
            }
            JsonEvent::String(value) => push_metadata_value(
                &mut stack,
                &mut root,
                RangeIndexValue::String(value.decoded().to_owned()),
            )?,
            JsonEvent::Number(source) => {
                let value = metadata_number(source).map_err(KvIrArchiveFailure::MetadataNumber)?;
                push_metadata_value(&mut stack, &mut root, value)?;
            }
            JsonEvent::Boolean(value) => {
                push_metadata_value(&mut stack, &mut root, RangeIndexValue::Boolean(value))?;
            }
            JsonEvent::Null => {
                push_metadata_value(&mut stack, &mut root, RangeIndexValue::Null)?;
            }
        }
    }
    if !stack.is_empty() {
        return Err(metadata_traversal());
    }
    root.ok_or_else(metadata_traversal)
}

fn push_metadata_value(
    stack: &mut [MetadataFrame],
    root: &mut Option<RangeIndexValue>,
    value: RangeIndexValue,
) -> Result<(), KvIrArchiveFailure> {
    match stack.last_mut() {
        Some(MetadataFrame::Object {
            fields,
            pending_key,
        }) => {
            let Some(key) = pending_key.take() else {
                return Err(metadata_traversal());
            };
            fields.insert(key, value);
        }
        Some(MetadataFrame::Array(values)) => values.push(value),
        None if root.is_none() => *root = Some(value),
        None => return Err(metadata_traversal()),
    }
    Ok(())
}

fn metadata_number(source: &[u8]) -> Result<RangeIndexValue, JsonNumberClassificationError> {
    let text =
        str::from_utf8(source).map_err(|error| JsonNumberClassificationError::InvalidSyntax {
            byte_offset: error.valid_up_to(),
        })?;
    if source.iter().any(|byte| matches!(byte, b'.' | b'e' | b'E')) {
        let value = text
            .parse::<f64>()
            .map_err(|_| JsonNumberClassificationError::OutOfRange {
                domain: JsonNumberDomain::Float,
            })?;
        if !value.is_finite() {
            return Err(JsonNumberClassificationError::OutOfRange {
                domain: JsonNumberDomain::Float,
            });
        }
        return Ok(RangeIndexValue::Float(value));
    }
    if source.first() == Some(&b'-') {
        text.parse::<i64>()
            .map(RangeIndexValue::Signed)
            .map_err(|_| JsonNumberClassificationError::OutOfRange {
                domain: JsonNumberDomain::Integer,
            })
    } else {
        text.parse::<u64>()
            .map(RangeIndexValue::Unsigned)
            .map_err(|_| JsonNumberClassificationError::OutOfRange {
                domain: JsonNumberDomain::Integer,
            })
    }
}

const fn metadata_traversal() -> KvIrArchiveFailure {
    KvIrArchiveFailure::Invalid(KvIrArchiveInvalidData::MetadataTraversal)
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::io::Cursor;

    use super::*;
    use crate::ExtractionMode;
    use crate::ExtractionOptions;
    use crate::archive::ArchiveCatalogLimits;
    use crate::archive::MetadataLimits;
    use crate::archive::NodeType;
    use crate::archive::SingleFileArchiveReader;
    use crate::extract_jsonl;
    use crate::ingest::KvIrOptions;
    use crate::ingest::KvIrReadError;
    use crate::ingest::KvIrReader;
    use crate::writer::ArchiveSetArchive;
    use crate::writer::ArchiveSetOptions;
    use crate::writer::ArchiveSetStats;
    use crate::writer::ArchiveSourceContext;
    use crate::writer::WriterOptions;

    const FOUR_BYTE_ORACLE_HEX: &str =
        include_str!("../../tests/fixtures/kv-ir-v0.1.0-four-byte-cpp.hex");
    const EIGHT_BYTE_ORACLE_HEX: &str =
        include_str!("../../tests/fixtures/kv-ir-v0.1.0-eight-byte-cpp.hex");
    const FOUR_BYTE_TIMESTAMP_ORACLE_HEX: &str =
        include_str!("../../tests/fixtures/kv-ir-v0.1.0-timestamps-four-cpp.hex");
    const EIGHT_BYTE_TIMESTAMP_ORACLE_HEX: &str =
        include_str!("../../tests/fixtures/kv-ir-v0.1.0-timestamps-eight-cpp.hex");
    const FOUR_BYTE_TIMESTAMP_SFA_HEX: &str =
        include_str!("../../tests/fixtures/sfa-v0.5.0-kv-ir-timestamps-four-cpp.hex");
    const EIGHT_BYTE_TIMESTAMP_SFA_HEX: &str =
        include_str!("../../tests/fixtures/sfa-v0.5.0-kv-ir-timestamps-eight-cpp.hex");
    const EXPECTED_JSON: &[u8] = concat!(
        "{\"empty\":{},\"message\":\"task 42 done\",\"none\":null,",
        "\"ok\":true,\"ratio\":1.250000}\n"
    )
    .as_bytes();
    const EXPECTED_CPP_TIMESTAMP_JSON: &[u8] = concat!(
        "{\"ts\":1700000000123,\"kind\":\"int\"}\n",
        "{\"ts\":1700000000.124999046,\"kind\":\"float\"}\n",
        "{\"ts\":\"1700000000125\",\"kind\":\"plain\"}\n",
        "{\"ts\":\"2023-11-14 22:13:20.126\",\"kind\":\"encoded\"}\n",
        "{\"kind\":\"missing\"}\n",
    )
    .as_bytes();
    const EXPECTED_RUST_TIMESTAMP_JSON: &[u8] = concat!(
        "{\"kind\":\"int\",\"ts\":1700000000123}\n",
        "{\"kind\":\"float\",\"ts\":1700000000.124999046}\n",
        "{\"kind\":\"plain\",\"ts\":\"1700000000125\"}\n",
        "{\"kind\":\"encoded\",\"ts\":\"2023-11-14 22:13:20.126\"}\n",
        "{\"kind\":\"missing\"}\n",
    )
    .as_bytes();

    #[derive(Debug, Default)]
    struct MemorySink {
        archives: Vec<Vec<u8>>,
    }

    impl FinalizedArchiveSink for MemorySink {
        type Error = io::Error;

        fn publish(&mut self, archive: &ArchiveSetArchive) -> Result<(), Self::Error> {
            let mut bytes = Vec::new();
            archive.write_sfa(&mut bytes)?;
            self.archives.push(bytes);
            Ok(())
        }
    }

    #[derive(Debug, Default)]
    struct StatsSink {
        values: Vec<ArchiveSetStats>,
    }

    impl ArchiveSetStatsCallback for StatsSink {
        type Error = io::Error;

        fn on_archive(&mut self, stats: ArchiveSetStats) -> Result<(), Self::Error> {
            self.values.push(stats);
            Ok(())
        }
    }

    fn decode_hex(source: &str) -> Vec<u8> {
        let digits: Vec<u8> = source.bytes().filter(u8::is_ascii_hexdigit).collect();
        let (pairs, remainder) = digits.as_chunks::<2>();
        assert!(remainder.is_empty(), "hex fixture has an even length");
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

    fn archive_fixture(bytes: &[u8], target: u64) -> (MemorySink, StatsSink, KvIrArchiveStats) {
        archive_fixture_with_options(bytes, target, KvIrArchiveOptions::default())
    }

    fn archive_fixture_with_options(
        bytes: &[u8],
        target: u64,
        adapter_options: KvIrArchiveOptions,
    ) -> (MemorySink, StatsSink, KvIrArchiveStats) {
        let options = ArchiveSetOptions::new(WriterOptions::default(), target);
        let mut writer =
            ArchiveSetWriter::new(MemorySink::default(), StatsSink::default(), options);
        let adapter_stats;
        {
            let mut adapter = KvIrArchiveSetSink::new(&mut writer, adapter_options);
            let mut reader = KvIrReader::new(Cursor::new(bytes), KvIrOptions::default());
            reader
                .read_to_end(&mut adapter)
                .expect("C++ fixture bridges into the archive set");
            adapter_stats = adapter.stats();
        }
        let finished = writer.finish().expect("finish bridged archive set");
        let (sink, stats) = finished.into_parts();
        (sink, stats, adapter_stats)
    }

    fn archive_fixture_with_source(
        bytes: &[u8],
        target: u64,
    ) -> (MemorySink, StatsSink, KvIrArchiveStats) {
        let options = ArchiveSetOptions::new(WriterOptions::default(), target);
        let mut writer =
            ArchiveSetWriter::new(MemorySink::default(), StatsSink::default(), options);
        let adapter_stats;
        {
            let mut adapter = KvIrArchiveSetSink::for_source(
                &mut writer,
                KvIrArchiveOptions::default(),
                ArchiveSourceContext::new("input.kvir", "creator"),
            );
            let mut reader = KvIrReader::new(Cursor::new(bytes), KvIrOptions::default());
            reader
                .read_to_end(&mut adapter)
                .expect("source-aware C++ fixture bridges into the archive set");
            adapter_stats = adapter.stats();
        }
        let finished = writer
            .finish()
            .expect("finish source-aware bridged archive set");
        let (sink, stats) = finished.into_parts();
        (sink, stats, adapter_stats)
    }

    fn archive_timestamp_fixture(
        bytes: &[u8],
        descriptor: &str,
    ) -> (MemorySink, StatsSink, KvIrArchiveStats) {
        let options =
            ArchiveSetOptions::new(WriterOptions::default().with_log_order(false), u64::MAX);
        let mut writer =
            ArchiveSetWriter::new(MemorySink::default(), StatsSink::default(), options);
        let resolver = KvIrTimestampResolver::parse(descriptor).expect("compile KV timestamp path");
        let adapter_stats;
        {
            let adapter = KvIrArchiveSetSink::new(&mut writer, KvIrArchiveOptions::default());
            let mut adapter = adapter.with_timestamp_resolver(&resolver);
            let mut reader = KvIrReader::new(Cursor::new(bytes), KvIrOptions::default());
            reader
                .read_to_end(&mut adapter)
                .expect("timestamp fixture bridges into the archive set");
            adapter_stats = adapter.stats();
        }
        let finished = writer.finish().expect("finish timestamp archive set");
        let (sink, stats) = finished.into_parts();
        (sink, stats, adapter_stats)
    }

    fn assert_kv_source_range(stats: &ArchiveSetStats, end: u64, split: u64) {
        let [range] = stats.range_index() else {
            panic!("expected exactly one KV source range")
        };
        assert_eq!((0, end), (range.start_index(), range.end_index()));
        assert_eq!(
            Some("input.kvir"),
            range.field("_filename").and_then(RangeIndexValue::as_str)
        );
        assert_eq!(
            Some("creator"),
            range
                .field("_archive_creator_id")
                .and_then(RangeIndexValue::as_str)
        );
        assert_eq!(
            Some(split),
            range
                .field("_file_split_number")
                .and_then(RangeIndexValue::as_u64)
        );
    }

    fn extract(sfa: &[u8]) -> Vec<u8> {
        let mut reader =
            SingleFileArchiveReader::open(Cursor::new(sfa)).expect("open bridged archive");
        let mut output = Vec::new();
        extract_jsonl(
            &mut reader,
            &mut output,
            ExtractionOptions::new(ExtractionMode::Unordered),
        )
        .expect("extract bridged archive");
        output
    }

    fn assert_cpp_kv_namespaces(sfa: &[u8]) {
        let mut reader =
            SingleFileArchiveReader::open(Cursor::new(sfa)).expect("open namespaced KV archive");
        let catalog = reader
            .read_catalog(ArchiveCatalogLimits::default())
            .expect("read namespaced KV catalog");
        let tree = catalog.schema_tree();
        let namespace_root = |key: &[u8]| {
            tree.nodes()
                .iter()
                .position(|node| {
                    node.parent_id().is_none()
                        && node.node_type() == NodeType::Object
                        && node.key_bytes() == key
                })
                .expect("KV namespace root exists")
        };
        let auto_root = namespace_root(b"@");
        let user_root = namespace_root(b"");
        let child_keys = |root| {
            tree.nodes()
                .iter()
                .filter(|node| node.parent_id() == Some(root))
                .map(crate::archive::SchemaNode::key_bytes)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            [b"level".as_slice(), b"seq"],
            child_keys(auto_root).as_slice()
        );
        assert_eq!(
            [b"empty".as_slice(), b"message", b"none", b"ok", b"ratio",],
            child_keys(user_root).as_slice()
        );
    }

    fn assert_timestamp_metadata(sfa: &[u8]) {
        let mut reader =
            SingleFileArchiveReader::open(Cursor::new(sfa)).expect("open timestamp archive");
        let metadata = reader
            .read_metadata(MetadataLimits::default())
            .expect("read timestamp metadata");
        let dictionary = metadata.timestamp_dictionary();
        let [range] = dictionary.ranges() else {
            panic!("one authoritative timestamp range")
        };
        assert_eq!("ts", range.key());
        assert_eq!(1, range.column_ids().len());
        assert_eq!(
            crate::archive::TimestampBounds::Epoch {
                start: 1_700_000_000_123,
                end: 1_700_000_000_126,
            },
            range.bounds()
        );
        for (id, raw) in [
            (0, r"\L"),
            (1, r"\E.\9"),
            (2, r#""\L""#),
            (3, r#""\Y-\m-\d \H:\M:\S.\3""#),
        ] {
            assert_eq!(
                raw,
                dictionary
                    .pattern(id)
                    .expect("known timestamp pattern ID")
                    .raw()
            );
        }
    }

    #[test]
    fn selection_slot_preserves_a_selection_only_within_its_epoch() {
        let mut slot = SelectionSlot::default();
        assert!(!slot.is_included(1));
        assert_eq!(None, slot.selected_pair_index(1));
        assert_eq!(None, slot.active_index(1));

        assert!(slot.include(1));
        assert!(slot.is_included(1));
        assert_eq!(None, slot.selected_pair_index(1));
        slot.set_active_index(1, 3)
            .expect("small active index fits");
        assert_eq!(Some(3), slot.active_index(1));

        slot.select(1, 7).expect("small pair index fits");
        assert!(!slot.include(1));
        assert_eq!(Some(7), slot.selected_pair_index(1));
        assert_eq!(Some(3), slot.active_index(1));

        assert!(!slot.is_included(2));
        assert_eq!(None, slot.selected_pair_index(2));
        assert_eq!(None, slot.active_index(2));
        assert!(slot.include(2));
        assert!(slot.is_included(2));
        assert_eq!(None, slot.selected_pair_index(2));
        assert_eq!(None, slot.active_index(2));
    }

    #[test]
    fn selection_epoch_wrap_clears_every_stale_slot_once() {
        let options = ArchiveSetOptions::new(WriterOptions::default(), u64::MAX);
        let mut writer =
            ArchiveSetWriter::new(MemorySink::default(), StatsSink::default(), options);
        let mut adapter = KvIrArchiveSetSink::new(&mut writer, KvIrArchiveOptions::default());
        adapter.selection_epoch = u32::MAX;

        let mut selected = SelectionSlot::default();
        assert!(selected.include(u32::MAX));
        selected.select(u32::MAX, 7).expect("small pair index fits");
        selected
            .set_active_index(u32::MAX, 3)
            .expect("small active index fits");
        adapter.selection_auto.push(selected);
        adapter.selection_user.push(selected);

        adapter.next_selection_epoch();
        assert_eq!(1, adapter.selection_epoch);
        assert_eq!(
            [SelectionSlot::default()],
            adapter.selection_auto.as_slice()
        );
        assert_eq!(
            [SelectionSlot::default()],
            adapter.selection_user.as_slice()
        );
    }

    #[test]
    #[cfg(target_pointer_width = "64")]
    fn schema_nodes_do_not_retain_dense_sibling_links() {
        assert_eq!(16, std::mem::size_of::<Option<usize>>());
        assert_eq!(12, std::mem::size_of::<SelectionSlot>());
        assert_eq!(40, std::mem::size_of::<SchemaNode>());
    }

    #[test]
    fn wide_sparse_events_leave_prior_epoch_slots_untouched() {
        const SCHEMA_NODE_COUNT: i16 = 4_096;

        let mut input = stream_preamble();
        for key in 1..=SCHEMA_NODE_COUNT {
            input.extend_from_slice(&[
                0x71, // integer schema node
                0x60, 0, // user root parent
                0x41, 2, // two-byte key
            ]);
            input.extend_from_slice(&key.to_be_bytes());
        }
        for (node_id, value) in [(1_i16, 7_u8), (SCHEMA_NODE_COUNT, 8_u8)] {
            input.push(0x66);
            input.extend_from_slice(&node_id.to_be_bytes());
            input.extend_from_slice(&[0x51, value]);
        }
        input.push(0);

        let options =
            ArchiveSetOptions::new(WriterOptions::default().with_log_order(false), u64::MAX);
        let mut writer =
            ArchiveSetWriter::new(MemorySink::default(), StatsSink::default(), options);
        {
            let mut adapter = KvIrArchiveSetSink::new(&mut writer, KvIrArchiveOptions::default());
            let mut reader = KvIrReader::new(Cursor::new(&input), KvIrOptions::default());
            reader
                .read_to_end(&mut adapter)
                .expect("wide sparse stream bridges into the archive set");

            assert_eq!(2, adapter.selection_epoch);
            assert_eq!(
                usize::try_from(SCHEMA_NODE_COUNT).unwrap() + 1,
                adapter.selection_user.len()
            );

            let prior = adapter.selection_user[1];
            assert_eq!(1, prior.epoch);
            assert_eq!(Some(0), prior.selected_pair_index(1));
            assert!(!prior.is_included(adapter.selection_epoch));
            assert_eq!(SelectionSlot::default(), adapter.selection_user[2]);

            let current = adapter.selection_user[usize::try_from(SCHEMA_NODE_COUNT).unwrap()];
            assert_eq!(adapter.selection_epoch, current.epoch);
            assert_eq!(
                Some(0),
                current.selected_pair_index(adapter.selection_epoch)
            );
            assert_eq!(adapter.active_auto.as_slice(), &[]);
            assert_eq!(
                [0, u32::try_from(SCHEMA_NODE_COUNT).unwrap()],
                adapter.active_user.as_slice()
            );
            assert_eq!(
                [
                    SparseLinks {
                        first_child: 1,
                        next_sibling: NO_SPARSE_INDEX,
                    },
                    SparseLinks::EMPTY,
                ],
                adapter.sparse_links.as_slice()
            );
        }
    }

    #[test]
    fn both_cpp_encodings_round_trip_to_the_same_archive_semantics() {
        let four = decode_hex(FOUR_BYTE_ORACLE_HEX);
        let eight = decode_hex(EIGHT_BYTE_ORACLE_HEX);
        let (four_sink, four_stats, four_adapter) = archive_fixture(&four, u64::MAX);
        let (eight_sink, eight_stats, eight_adapter) = archive_fixture(&eight, u64::MAX);

        assert_eq!(EXPECTED_JSON, extract(&four_sink.archives[0]));
        assert_eq!(EXPECTED_JSON, extract(&eight_sink.archives[0]));
        assert_cpp_kv_namespaces(&four_sink.archives[0]);
        assert_cpp_kv_namespaces(&eight_sink.archives[0]);
        assert_eq!(1, four_adapter.records());
        assert_eq!(1, eight_adapter.records());
        assert_eq!(345, four_adapter.source_bytes());
        assert_eq!(349, eight_adapter.source_bytes());
        assert_eq!(345, four_stats.values[0].uncompressed_size());
        assert_eq!(349, eight_stats.values[0].uncompressed_size());
    }

    #[test]
    fn both_cpp_encodings_promote_all_supported_timestamp_scalars_exactly() {
        for (input_hex, expected_sfa_hex, expected_source_bytes) in [
            (
                FOUR_BYTE_TIMESTAMP_ORACLE_HEX,
                FOUR_BYTE_TIMESTAMP_SFA_HEX,
                377,
            ),
            (
                EIGHT_BYTE_TIMESTAMP_ORACLE_HEX,
                EIGHT_BYTE_TIMESTAMP_SFA_HEX,
                389,
            ),
        ] {
            let input = decode_hex(input_hex);
            let expected_sfa = decode_hex(expected_sfa_hex);
            let (sink, stats, adapter) = archive_timestamp_fixture(&input, "ts");

            assert_eq!(1, sink.archives.len());
            let cpp_json = extract(&expected_sfa);
            let rust_json = extract(&sink.archives[0]);
            assert_eq!(EXPECTED_CPP_TIMESTAMP_JSON, cpp_json);
            assert_eq!(EXPECTED_RUST_TIMESTAMP_JSON, rust_json);
            assert_timestamp_metadata(&expected_sfa);
            assert_timestamp_metadata(&sink.archives[0]);
            assert_eq!(5, adapter.records());
            assert_eq!(expected_source_bytes, adapter.source_bytes());
            assert_eq!(1_700_000_000_123, stats.values[0].begin_timestamp());
            assert_eq!(1_700_000_000_126, stats.values[0].end_timestamp());
            assert_eq!(expected_source_bytes, stats.values[0].uncompressed_size());
        }
    }

    #[test]
    fn namespace_matching_and_utc_offset_units_are_deterministic() {
        let input = decode_hex(FOUR_BYTE_ORACLE_HEX);
        let (auto_sink, auto_stats, auto_adapter) = archive_timestamp_fixture(&input, "@seq");
        assert_eq!(EXPECTED_JSON, extract(&auto_sink.archives[0]));
        assert_eq!(345, auto_adapter.source_bytes());
        assert_eq!(345, auto_stats.values[0].uncompressed_size());
        let mut reader = SingleFileArchiveReader::open(Cursor::new(&auto_sink.archives[0]))
            .expect("open auto timestamp archive");
        let metadata = reader
            .read_metadata(MetadataLimits::default())
            .expect("read auto timestamp metadata");
        let [range] = metadata.timestamp_dictionary().ranges() else {
            panic!("one auto-generated timestamp range")
        };
        assert_eq!("@seq", range.key());
        assert_eq!(
            crate::archive::TimestampBounds::Epoch {
                start: 7_000,
                end: 7_000,
            },
            range.bounds()
        );

        let (user_sink, _, user_adapter) = archive_timestamp_fixture(&input, "seq");
        assert_eq!(345, user_adapter.source_bytes());
        let mut reader = SingleFileArchiveReader::open(Cursor::new(&user_sink.archives[0]))
            .expect("open unmatched user timestamp archive");
        let metadata = reader
            .read_metadata(MetadataLimits::default())
            .expect("read unmatched timestamp metadata");
        assert_eq!(metadata.timestamp_dictionary().ranges(), []);
    }

    #[test]
    fn nested_kv_timestamp_paths_promote_the_matching_leaf() {
        let input = stream_with_nested_integer();
        let (sink, _, adapter) = archive_timestamp_fixture(&input, "a.b");
        assert_eq!(
            b"{\"a\":{\"b\":7}}\n",
            extract(&sink.archives[0]).as_slice()
        );
        assert_eq!(1, adapter.records());
        let mut reader = SingleFileArchiveReader::open(Cursor::new(&sink.archives[0]))
            .expect("open nested timestamp archive");
        let metadata = reader
            .read_metadata(MetadataLimits::default())
            .expect("read nested timestamp metadata");
        let [range] = metadata.timestamp_dictionary().ranges() else {
            panic!("one nested timestamp range")
        };
        assert_eq!("a.b", range.key());
        assert_eq!(
            crate::archive::TimestampBounds::Epoch {
                start: 7_000,
                end: 7_000,
            },
            range.bounds()
        );
    }

    #[test]
    fn unsupported_matching_kinds_remain_ordinary_values() {
        let input = decode_hex(FOUR_BYTE_ORACLE_HEX);
        for descriptor in ["ok", "none", "empty"] {
            let (sink, _, adapter) = archive_timestamp_fixture(&input, descriptor);
            assert_eq!(EXPECTED_JSON, extract(&sink.archives[0]));
            assert_eq!(1, adapter.records());
        }

        let input = stream_with_encoded_float_and_array(KvIrEncoding::FourByte);
        let (sink, _, adapter) = archive_timestamp_fixture(&input, "a");
        assert_eq!(
            b"{\"f\":\"1.25\",\"a\":[1,2],\"m\":\"d=word/e=\\\\\"}\n",
            extract(&sink.archives[0]).as_slice()
        );
        assert_eq!(1, adapter.records());
    }

    #[test]
    fn malformed_matching_timestamp_preserves_only_the_valid_committed_prefix() {
        let mut input = decode_hex(FOUR_BYTE_TIMESTAMP_ORACLE_HEX);
        let original = b"1700000000125";
        let replacement = b"not-a-time___";
        let offset = input
            .windows(original.len())
            .position(|window| window == original)
            .expect("plain timestamp exists in fixture");
        input[offset..offset + original.len()].copy_from_slice(replacement);
        let options =
            ArchiveSetOptions::new(WriterOptions::default().with_log_order(false), u64::MAX);
        let mut writer =
            ArchiveSetWriter::new(MemorySink::default(), StatsSink::default(), options);
        let resolver = KvIrTimestampResolver::parse("ts").expect("compile timestamp path");
        let adapter_stats;
        {
            let adapter = KvIrArchiveSetSink::new(&mut writer, KvIrArchiveOptions::default());
            let mut adapter = adapter.with_timestamp_resolver(&resolver);
            let mut reader = KvIrReader::new(Cursor::new(&input), KvIrOptions::default());
            let error = reader
                .read_to_end(&mut adapter)
                .expect_err("malformed matching timestamp must fail");
            let KvIrReadError::Sink { source, .. } = error else {
                panic!("expected timestamp adapter error")
            };
            assert!(
                matches!(
                    source.kind(),
                    KvIrArchiveErrorKind::Conversion(KvIrArchiveFailure::Timestamp {
                        namespace: KvIrNamespace::UserGenerated,
                        node_id: 4,
                        source: JsonTimestampError::UnsupportedLexeme {
                            kind: super::super::JsonTimestampScalarKind::String,
                        },
                    })
                ),
                "{:#?}",
                source.kind(),
            );
            assert!(!source.record_committed());
            adapter_stats = adapter.stats();
            assert_eq!(2, adapter_stats.records());
            assert_eq!(282, adapter.accounted_input_bytes());
            assert_eq!(282, adapter_stats.source_bytes());
        }

        let (sink, stats) = writer
            .finish()
            .expect("finish committed timestamp prefix")
            .into_parts();
        assert_eq!(1, sink.archives.len());
        assert_eq!(1, stats.values.len());
        assert_eq!(2, stats.values[0].record_count());
        assert_eq!(282, stats.values[0].uncompressed_size());
        assert_eq!(1_700_000_000_123, stats.values[0].begin_timestamp());
        assert_eq!(1_700_000_000_125, stats.values[0].end_timestamp());
        assert_eq!(
            concat!(
                "{\"kind\":\"int\",\"ts\":1700000000123}\n",
                "{\"kind\":\"float\",\"ts\":1700000000.124999046}\n",
            )
            .as_bytes(),
            extract(&sink.archives[0]),
        );
    }

    #[test]
    fn timestamp_lexeme_uses_the_same_bounded_record_scratch() {
        let input = stream_with_nested_integer();
        let limits = KvIrArchiveLimits::new().with_max_reconstructed_value_bytes(0);
        let options = ArchiveSetOptions::new(WriterOptions::default(), u64::MAX);
        let mut writer =
            ArchiveSetWriter::new(MemorySink::default(), StatsSink::default(), options);
        let resolver = KvIrTimestampResolver::parse("a.b").expect("compile timestamp path");
        let adapter =
            KvIrArchiveSetSink::new(&mut writer, KvIrArchiveOptions::new().with_limits(limits));
        let mut adapter = adapter.with_timestamp_resolver(&resolver);
        let mut reader = KvIrReader::new(Cursor::new(&input), KvIrOptions::default());
        let error = reader
            .read_to_end(&mut adapter)
            .expect_err("timestamp lexeme must obey the reconstructed-value limit");
        let KvIrReadError::Sink { source, .. } = error else {
            panic!("expected timestamp scratch limit")
        };
        assert!(matches!(
            source.kind(),
            KvIrArchiveErrorKind::Conversion(KvIrArchiveFailure::Limit(limit))
                if limit.resource() == KvIrArchiveLimitResource::ReconstructedValueBytes
                    && limit.actual() == 1
                    && limit.limit() == 0
        ));
        assert_eq!(0, adapter.stats().records());
        assert_eq!(0, adapter.accounted_input_bytes());
    }

    #[test]
    fn exact_rotation_attributes_the_eof_to_the_final_empty_archive() {
        let bytes = decode_hex(FOUR_BYTE_ORACLE_HEX);
        let (sink, stats, adapter) = archive_fixture_with_source(&bytes, 0);
        assert_eq!(2, sink.archives.len());
        assert_eq!(2, stats.values.len());
        assert_eq!(344, stats.values[0].uncompressed_size());
        assert_eq!(1, stats.values[1].uncompressed_size());
        assert_eq!(1, stats.values[0].record_count());
        assert_eq!(0, stats.values[1].record_count());
        assert_eq!(345, adapter.source_bytes());
        assert_kv_source_range(&stats.values[0], 1, 0);
        assert_kv_source_range(&stats.values[1], 0, 1);
    }

    #[test]
    fn user_defined_metadata_is_typed_exactly_in_stats_and_the_range_packet() {
        const METADATA: &[u8] = concat!(
            r#"{"VERSION":"0.1.0","USER_DEFINED_METADATA":{"#,
            r#""array":[true,-2,1.5,null],"max":18446744073709551615,"#,
            r#""nested":{"key":"value"},"test_key":"test_value"}}"#,
        )
        .as_bytes();
        let mut bytes = stream_preamble_with_metadata(KvIrEncoding::FourByte, METADATA);
        bytes.push(0);
        let (sink, stats, adapter) = archive_fixture_with_source(&bytes, u64::MAX);
        assert_eq!(u64::try_from(bytes.len()).unwrap(), adapter.source_bytes());
        assert_eq!(1, stats.values.len());
        assert_kv_source_range(&stats.values[0], 0, 0);

        let [range] = stats.values[0].range_index() else {
            panic!("expected one user-metadata range")
        };
        assert_eq!(
            Some(&RangeIndexValue::Unsigned(u64::MAX)),
            range.field("max")
        );
        assert_eq!(
            Some(&RangeIndexValue::String("test_value".to_owned())),
            range.field("test_key")
        );
        assert_eq!(
            Some(&RangeIndexValue::Array(vec![
                RangeIndexValue::Boolean(true),
                RangeIndexValue::Signed(-2),
                RangeIndexValue::Float(1.5),
                RangeIndexValue::Null,
            ])),
            range.field("array")
        );
        assert_eq!(
            Some(&RangeIndexValue::Object(BTreeMap::from([(
                "key".to_owned(),
                RangeIndexValue::String("value".to_owned()),
            )]))),
            range.field("nested")
        );

        let mut reader = SingleFileArchiveReader::open(Cursor::new(&sink.archives[0]))
            .expect("open source-aware KV archive");
        let metadata = reader
            .read_metadata(MetadataLimits::default())
            .expect("read source-aware KV metadata");
        let packet = metadata.range_index().expect("range-index packet");
        let [decoded] = packet.entries() else {
            panic!("expected one decoded KV range")
        };
        assert_eq!(range.fields(), decoded.fields());
    }

    #[test]
    fn concatenated_streams_form_adjacent_independent_source_contexts() {
        const FIRST: &[u8] = br#"{"VERSION":"0.1.0","USER_DEFINED_METADATA":{"stream":"first"}}"#;
        const SECOND: &[u8] = br#"{"VERSION":"0.1.0","USER_DEFINED_METADATA":{"stream":"second"}}"#;
        let mut bytes = stream_preamble_with_metadata(KvIrEncoding::FourByte, FIRST);
        bytes.push(0);
        bytes.extend_from_slice(&stream_preamble_with_metadata(
            KvIrEncoding::FourByte,
            SECOND,
        ));
        bytes.push(0);

        let (sink, stats, adapter) = archive_fixture_with_source(&bytes, u64::MAX);
        assert_eq!(2, adapter.streams());
        assert_eq!(u64::try_from(bytes.len()).unwrap(), adapter.source_bytes());
        assert_eq!(1, sink.archives.len());
        assert_eq!(1, stats.values.len());
        let [first, second] = stats.values[0].range_index() else {
            panic!("expected one adjacent range per KV stream")
        };
        for range in [first, second] {
            assert_eq!((0, 0), (range.start_index(), range.end_index()));
            assert_eq!(
                Some(0),
                range
                    .field("_file_split_number")
                    .and_then(RangeIndexValue::as_u64)
            );
        }
        assert_eq!(
            Some("first"),
            first.field("stream").and_then(RangeIndexValue::as_str)
        );
        assert_eq!(
            Some("second"),
            second.field("stream").and_then(RangeIndexValue::as_str)
        );
    }

    #[test]
    fn reserved_kv_metadata_is_rejected_before_opening_a_source() {
        const METADATA: &[u8] =
            br#"{"VERSION":"0.1.0","USER_DEFINED_METADATA":{"_filename":"override"}}"#;
        let mut bytes = stream_preamble_with_metadata(KvIrEncoding::FourByte, METADATA);
        bytes.push(0);
        let options = ArchiveSetOptions::new(WriterOptions::default(), u64::MAX);
        let mut writer =
            ArchiveSetWriter::new(MemorySink::default(), StatsSink::default(), options);
        let mut adapter = KvIrArchiveSetSink::for_source(
            &mut writer,
            KvIrArchiveOptions::default(),
            ArchiveSourceContext::new("input.kvir", "creator"),
        );
        let mut reader = KvIrReader::new(Cursor::new(&bytes), KvIrOptions::default());
        let error = reader
            .read_to_end(&mut adapter)
            .expect_err("reserved metadata must not replace source identity");
        let KvIrReadError::Sink { source, .. } = error else {
            panic!("expected source-context adapter error")
        };
        assert!(matches!(
            source.kind(),
            KvIrArchiveErrorKind::Conversion(KvIrArchiveFailure::SourceContext(
                ArchiveSourceContextError::ReservedField { name }
            )) if name == "_filename"
        ));
        assert_eq!(0, adapter.stats().streams());
        assert_eq!(0, adapter.accounted_input_bytes());
        assert_eq!(None, adapter.archive_set().current_source_split_number());
    }

    #[test]
    fn nested_schema_paths_become_balanced_flat_writer_events() {
        let bytes = stream_with_nested_integer();
        let (sink, stats, adapter) = archive_fixture(&bytes, u64::MAX);
        assert_eq!(
            b"{\"a\":{\"b\":7}}\n".as_slice(),
            extract(&sink.archives[0])
        );
        assert_eq!(1, adapter.records());
        assert_eq!(u64::try_from(bytes.len()).unwrap(), adapter.source_bytes());
        assert_eq!(adapter.source_bytes(), stats.values[0].uncompressed_size());
    }

    #[test]
    fn sparse_planning_preserves_nested_schema_order_for_interleaved_pairs() {
        let mut bytes = stream_preamble();
        append_test_user_schema_node(&mut bytes, KvIrNodeType::Object, 0, b"a");
        append_test_user_schema_node(&mut bytes, KvIrNodeType::Integer, 0, b"z");
        append_test_user_schema_node(&mut bytes, KvIrNodeType::Integer, 1, b"c");
        append_test_user_schema_node(&mut bytes, KvIrNodeType::Object, 1, b"b");
        append_test_user_schema_node(&mut bytes, KvIrNodeType::Integer, 4, b"y");
        append_test_user_schema_node(&mut bytes, KvIrNodeType::Integer, 1, b"d");
        append_test_user_schema_node(&mut bytes, KvIrNodeType::Integer, 0, b"m");
        append_test_user_schema_node(&mut bytes, KvIrNodeType::Integer, 4, b"x");
        bytes.extend_from_slice(&[
            0x65, 5, // a.b.y
            0x65, 7, // m
            0x65, 6, // a.d
            0x65, 8, // a.b.x
            0x65, 2, // z
            0x65, 3, // a.c
            0x51, 50, // a.b.y value
            0x51, 70, // m value
            0x51, 60, // a.d value
            0x51, 80, // a.b.x value
            0x51, 20, // z value
            0x51, 30, // a.c value
            0,
        ]);

        let (sink, _, adapter) = archive_fixture(&bytes, u64::MAX);
        assert_eq!(
            b"{\"a\":{\"c\":30,\"b\":{\"y\":50,\"x\":80},\"d\":60},\"z\":20,\"m\":70}\n",
            extract(&sink.archives[0]).as_slice()
        );
        assert_eq!(1, adapter.records());
    }

    #[test]
    fn sparse_planning_materializes_shared_deep_ancestors_once() {
        const DEPTH: u8 = 32;

        let mut bytes = stream_preamble();
        let mut parent_id = 0;
        for node_id in 1..=DEPTH {
            append_test_user_schema_node(&mut bytes, KvIrNodeType::Object, parent_id, b"o");
            parent_id = node_id;
        }
        let x_id = DEPTH + 1;
        let y_id = DEPTH + 2;
        append_test_user_schema_node(&mut bytes, KvIrNodeType::Integer, DEPTH, b"x");
        append_test_user_schema_node(&mut bytes, KvIrNodeType::Integer, DEPTH, b"y");
        bytes.extend_from_slice(&[
            0x65, y_id, // reverse wire order must not change sibling order
            0x65, x_id, 0x51, 2, // y value
            0x51, 1, // x value
            0,
        ]);

        let options =
            ArchiveSetOptions::new(WriterOptions::default().with_log_order(false), u64::MAX);
        let mut writer =
            ArchiveSetWriter::new(MemorySink::default(), StatsSink::default(), options);
        {
            let mut adapter = KvIrArchiveSetSink::new(&mut writer, KvIrArchiveOptions::default());
            let mut reader = KvIrReader::new(Cursor::new(&bytes), KvIrOptions::default());
            reader
                .read_to_end(&mut adapter)
                .expect("deep shared paths bridge into the archive set");

            let active_count = usize::from(DEPTH) + 3;
            assert_eq!(adapter.active_auto.as_slice(), &[]);
            assert_eq!(active_count, adapter.active_user.len());
            assert_eq!(active_count, adapter.sparse_links.len());
            assert_eq!(2 * usize::from(DEPTH) + 2, adapter.plan.len());
            assert_eq!(
                (0..u32::try_from(active_count).unwrap()).collect::<Vec<_>>(),
                adapter.active_user
            );
        }

        let (sink, _) = writer
            .finish()
            .expect("finish deep sparse archive")
            .into_parts();
        let mut expected = Vec::new();
        for _ in 0..DEPTH {
            expected.extend_from_slice(b"{\"o\":");
        }
        expected.extend_from_slice(b"{\"x\":1,\"y\":2}");
        expected.extend(std::iter::repeat_n(b'}', usize::from(DEPTH)));
        expected.push(b'\n');
        assert_eq!(expected, extract(&sink.archives[0]));
    }

    #[test]
    fn encoded_float_text_and_unstructured_arrays_match_in_both_widths() {
        for encoding in [KvIrEncoding::FourByte, KvIrEncoding::EightByte] {
            let bytes = stream_with_encoded_float_and_array(encoding);
            let (sink, stats, adapter) = archive_fixture(&bytes, u64::MAX);
            assert_eq!(
                b"{\"f\":\"1.25\",\"a\":[1,2],\"m\":\"d=word/e=\\\\\"}\n".as_slice(),
                extract(&sink.archives[0])
            );
            assert_eq!(1, adapter.records());
            assert_eq!(u64::try_from(bytes.len()).unwrap(), adapter.source_bytes());
            assert_eq!(adapter.source_bytes(), stats.values[0].uncompressed_size());
        }
    }

    #[test]
    fn direct_encoded_text_preserves_cpp_dictionary_width_semantics() {
        for (encoding, expected_logtype, expected_variable) in [
            (KvIrEncoding::FourByte, b"d=\x11".as_slice(), None),
            (
                KvIrEncoding::EightByte,
                b"d=\x12".as_slice(),
                Some(b"123".as_slice()),
            ),
        ] {
            let bytes = stream_with_numeric_dictionary(encoding);
            let (sink, ..) = archive_fixture(&bytes, u64::MAX);
            assert_eq!(
                b"{\"x\":\"d=123\"}\n".as_slice(),
                extract(&sink.archives[0])
            );

            let mut reader = SingleFileArchiveReader::open(Cursor::new(&sink.archives[0]))
                .expect("open direct encoded-text archive");
            let catalog = reader
                .read_catalog(ArchiveCatalogLimits::default())
                .expect("read direct encoded-text catalog");
            let logtypes = catalog.log_type_dictionary().entries().collect::<Vec<_>>();
            let [logtype] = logtypes.as_slice() else {
                panic!("expected one direct encoded-text logtype")
            };
            assert_eq!(expected_logtype, logtype.escaped_value());
            assert_eq!(
                expected_variable,
                catalog
                    .variable_dictionary()
                    .entry(0)
                    .map(crate::archive::VariableDictionaryEntry::value)
            );
            assert_eq!(
                usize::from(expected_variable.is_some()),
                catalog.variable_dictionary().len()
            );
        }
    }

    #[test]
    fn direct_encoded_text_replays_the_cached_layout_across_three_records() {
        let bytes = stream_with_repeated_direct_encoded_text();
        let (sink, stats, adapter) = archive_fixture(&bytes, u64::MAX);
        assert_eq!(3, adapter.records());
        assert_eq!(3, stats.values[0].record_count());
        assert_eq!(
            concat!(
                "{\"x\":\"value=1\"}\n",
                "{\"x\":\"value=2\"}\n",
                "{\"x\":\"value=3\"}\n",
            )
            .as_bytes(),
            extract(&sink.archives[0]),
        );
    }

    #[test]
    fn reconstructed_value_limit_is_located_and_atomic() {
        let bytes = decode_hex(FOUR_BYTE_ORACLE_HEX);
        let limits = KvIrArchiveLimits::new().with_max_reconstructed_value_bytes(11);
        let options = ArchiveSetOptions::new(WriterOptions::default(), u64::MAX);
        let mut writer =
            ArchiveSetWriter::new(MemorySink::default(), StatsSink::default(), options);
        let mut adapter =
            KvIrArchiveSetSink::new(&mut writer, KvIrArchiveOptions::new().with_limits(limits));
        let mut reader = KvIrReader::new(Cursor::new(&bytes), KvIrOptions::default());
        let error = reader
            .read_to_end(&mut adapter)
            .expect_err("decoded message exceeds the adapter limit");
        let KvIrReadError::Sink { source, .. } = error else {
            panic!("expected adapter error")
        };
        let KvIrArchiveErrorKind::Conversion(KvIrArchiveFailure::Limit(limit)) = source.kind()
        else {
            panic!("expected adapter limit, got {source:?}")
        };
        assert_eq!(
            KvIrArchiveLimitResource::ReconstructedValueBytes,
            limit.resource()
        );
        assert_eq!(12, limit.actual());
        assert_eq!(11, limit.limit());
        assert_eq!(0, adapter.stats().records());
        assert_eq!(0, adapter.accounted_input_bytes());
    }

    #[test]
    fn direct_encoded_text_preserves_incremental_reconstruction_limit_precedence() {
        for (placeholder, encoded, limit, expected_actual) in [
            (0x11, -42_i32, 0_u64, 1_u64),
            // Two populated digits in a one-digit field are malformed, but reconstruction first
            // checks the declared two-byte `0.` output just like the former materialized path.
            (0x13, 12_i32 << 6, 1_u64, 2_u64),
        ] {
            let bytes = stream_with_four_byte_encoded_variable(placeholder, encoded);
            let limits = KvIrArchiveLimits::new().with_max_reconstructed_value_bytes(limit);
            let options = ArchiveSetOptions::new(WriterOptions::default(), u64::MAX);
            let mut writer =
                ArchiveSetWriter::new(MemorySink::default(), StatsSink::default(), options);
            let mut adapter =
                KvIrArchiveSetSink::new(&mut writer, KvIrArchiveOptions::new().with_limits(limits));
            let mut reader = KvIrReader::new(Cursor::new(&bytes), KvIrOptions::default());
            let error = reader
                .read_to_end(&mut adapter)
                .expect_err("the incremental reconstructed-value limit must fail first");
            let KvIrReadError::Sink { source, .. } = error else {
                panic!("expected adapter error")
            };
            assert!(matches!(
                source.kind(),
                KvIrArchiveErrorKind::Conversion(KvIrArchiveFailure::Limit(violation))
                    if violation.resource()
                        == KvIrArchiveLimitResource::ReconstructedValueBytes
                        && violation.actual() == expected_actual
                        && violation.limit() == limit
            ));
            assert_eq!(0, adapter.stats().records());
            assert_eq!(0, adapter.accounted_input_bytes());
        }
    }

    #[test]
    fn malformed_encoded_text_is_structured_and_located_before_append() {
        let bytes = stream_with_missing_encoded_variable();
        let options = ArchiveSetOptions::new(WriterOptions::default(), u64::MAX);
        let mut writer =
            ArchiveSetWriter::new(MemorySink::default(), StatsSink::default(), options);
        let mut adapter = KvIrArchiveSetSink::new(&mut writer, KvIrArchiveOptions::default());
        let mut reader = KvIrReader::new(Cursor::new(&bytes), KvIrOptions::default());
        let error = reader
            .read_to_end(&mut adapter)
            .expect_err("missing AST variable must fail");
        let KvIrReadError::Sink { source, .. } = error else {
            panic!("expected adapter error")
        };
        assert_eq!(0, source.stream_index());
        assert_eq!(Some(0), source.event_index());
        assert!(source.input_offset() > 0);
        assert!(matches!(
            source.kind(),
            KvIrArchiveErrorKind::Conversion(KvIrArchiveFailure::Unsupported {
                namespace: KvIrNamespace::UserGenerated,
                node_id: 1,
                source: KvIrArchiveUnsupported::MissingEncodedVariable,
            })
        ));
        assert_eq!(0, adapter.stats().records());
        assert_eq!(0, adapter.accounted_input_bytes());
    }

    fn stream_with_missing_encoded_variable() -> Vec<u8> {
        let mut bytes = stream_preamble();
        bytes.extend_from_slice(&[
            0x74, 0x60, 0, 0x41, 1, b'x', // user string schema node 1
            0x65, 1, 0x59, 0x21, 1, 0x11, // event, logtype has int placeholder, no vars
            0,
        ]);
        bytes
    }

    fn stream_with_four_byte_encoded_variable(placeholder: u8, encoded: i32) -> Vec<u8> {
        let mut bytes = stream_preamble();
        bytes.extend_from_slice(&[
            0x74, 0x60, 0, 0x41, 1, b'x', // user string schema node 1
            0x65, 1, 0x59, 0x18, // event, four-byte encoded text, one i32 variable
        ]);
        bytes.extend_from_slice(&encoded.to_be_bytes());
        bytes.extend_from_slice(&[0x21, 1, placeholder, 0]);
        bytes
    }

    fn stream_with_repeated_direct_encoded_text() -> Vec<u8> {
        let mut bytes = stream_preamble();
        bytes.extend_from_slice(&[
            0x74, 0x60, 0, 0x41, 1, b'x', // user string schema node 1
        ]);
        for encoded in [1_i32, 2, 3] {
            bytes.extend_from_slice(&[
                0x65, 1, 0x59, 0x18, // event, four-byte encoded text, one i32 variable
            ]);
            bytes.extend_from_slice(&encoded.to_be_bytes());
            bytes.extend_from_slice(&[
                0x21, 7, b'v', b'a', b'l', b'u', b'e', b'=', 0x11, // logtype
            ]);
        }
        bytes.push(0);
        bytes
    }

    fn stream_with_numeric_dictionary(encoding: KvIrEncoding) -> Vec<u8> {
        let mut bytes = stream_preamble_with_encoding(encoding);
        bytes.extend_from_slice(&[
            0x74, 0x60, 0, 0x41, 1, b'x', // user string schema node 1
            0x65, 1, // event selecting node 1
        ]);
        bytes.push(match encoding {
            KvIrEncoding::FourByte => 0x59,
            KvIrEncoding::EightByte => 0x5a,
        });
        bytes.extend_from_slice(&[
            0x11, 3, b'1', b'2', b'3', // numeric-looking dictionary variable
            0x21, 3, b'd', b'=', 0x12, // logtype
            0,
        ]);
        bytes
    }

    fn append_test_user_schema_node(
        bytes: &mut Vec<u8>,
        node_type: KvIrNodeType,
        parent_id: u8,
        key: &[u8],
    ) {
        let node_type = match node_type {
            KvIrNodeType::Integer => 0x71,
            KvIrNodeType::Object => 0x76,
            _ => panic!("test helper only needs integer and object schema nodes"),
        };
        bytes.extend_from_slice(&[
            node_type,
            0x60,
            parent_id,
            0x41,
            u8::try_from(key.len()).expect("test schema key length fits u8"),
        ]);
        bytes.extend_from_slice(key);
    }

    fn stream_with_nested_integer() -> Vec<u8> {
        let mut bytes = stream_preamble();
        bytes.extend_from_slice(&[
            0x76, 0x60, 0, 0x41, 1, b'a', // user object schema node 1
            0x71, 0x60, 1, 0x41, 1, b'b', // integer child schema node 2
            0x65, 2, 0x51, 7, // event selecting node 2 with one-byte integer 7
            0,
        ]);
        bytes
    }

    fn stream_with_encoded_float_and_array(encoding: KvIrEncoding) -> Vec<u8> {
        let mut bytes = stream_preamble_with_encoding(encoding);
        bytes.extend_from_slice(&[
            0x74, 0x60, 0, 0x41, 1, b'f', // user string schema node 1
            0x75, 0x60, 0, 0x41, 1, b'a', // user array schema node 2
            0x74, 0x60, 0, 0x41, 1, b'm', // user string schema node 3
            0x65, 1, 0x65, 2, 0x65, 3, // event node IDs
        ]);
        match encoding {
            KvIrEncoding::FourByte => {
                bytes.extend_from_slice(&[0x59, 0x18]);
                bytes.extend_from_slice(&8_017_i32.to_be_bytes());
            }
            KvIrEncoding::EightByte => {
                bytes.extend_from_slice(&[0x5a, 0x19]);
                bytes.extend_from_slice(&32_033_i64.to_be_bytes());
            }
        }
        bytes.extend_from_slice(&[0x21, 1, 0x13]);
        bytes.push(match encoding {
            KvIrEncoding::FourByte => 0x59,
            KvIrEncoding::EightByte => 0x5a,
        });
        bytes.extend_from_slice(&[0x21, 5, b'[', b'1', b',', b'2', b']']);
        bytes.push(match encoding {
            KvIrEncoding::FourByte => 0x59,
            KvIrEncoding::EightByte => 0x5a,
        });
        bytes.extend_from_slice(&[
            0x11, 4, b'w', b'o', b'r', b'd', // dictionary variable
            0x21, 8, b'd', b'=', 0x12, b'/', b'e', b'=', b'\\', b'\\', // logtype
            0,
        ]);
        bytes
    }

    fn stream_preamble() -> Vec<u8> {
        stream_preamble_with_encoding(KvIrEncoding::FourByte)
    }

    fn stream_preamble_with_encoding(encoding: KvIrEncoding) -> Vec<u8> {
        let metadata = br#"{"VERSION":"0.1.0"}"#;
        stream_preamble_with_metadata(encoding, metadata)
    }

    fn stream_preamble_with_metadata(encoding: KvIrEncoding, metadata: &[u8]) -> Vec<u8> {
        let mut bytes = match encoding {
            KvIrEncoding::FourByte => vec![0xfd, 0x2f, 0xb5, 0x29],
            KvIrEncoding::EightByte => vec![0xfd, 0x2f, 0xb5, 0x30],
        };
        bytes.extend_from_slice(&[0x01, 0x11]);
        bytes.push(u8::try_from(metadata.len()).expect("metadata fits one-byte packet"));
        bytes.extend_from_slice(metadata);
        bytes
    }
}
