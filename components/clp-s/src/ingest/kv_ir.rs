//! Bounded streaming decoder for CLP's current key-value IR protocol.
//!
//! The decoder accepts both protocol magic numbers, validates the JSON preamble and current
//! `0.1.0` version, and emits complete borrowed units to a caller-owned sink. It maintains the two
//! protocol schema trees iteratively and never recurses over input. A `0x00` end marker completes
//! one stream; another magic number may follow immediately, allowing callers to process the same
//! concatenation that C++ can process by constructing another deserializer at the current reader
//! position.
//!
//! Legacy `0.0.x` streams contain unstructured-log IR rather than KV events. They are detected and
//! rejected explicitly; decoding that dialect is a later compatibility adapter.

#[cfg(test)]
use std::cell::Cell;
use std::error::Error;
use std::fmt;
use std::fmt::Display;
use std::fmt::Formatter;
use std::io;
use std::io::Read;
use std::ops::Range;
use std::slice;

use super::JsonEvent;
use super::JsonEvents;
use super::NdjsonInvalidRecordKind;
use super::NdjsonLimitResource;
use super::NdjsonLimits;
use super::NdjsonResource;
use super::parser::Frame;
use super::parser::ParseFailure;
use super::parser::StoredEvent;
use super::parser::parse_document;

const INPUT_BUFFER_BYTES: usize = 8 * 1024;
const MEBIBYTE: u64 = 1024 * 1024;
const GIBIBYTE: u64 = 1024 * MEBIBYTE;
const FOUR_BYTE_MAGIC: [u8; 4] = [0xfd, 0x2f, 0xb5, 0x29];
const EIGHT_BYTE_MAGIC: [u8; 4] = [0xfd, 0x2f, 0xb5, 0x30];
const CURRENT_VERSION: &str = "0.1.0";
const EMPTY_SCHEMA_INDEX_SLOT: u32 = 0;
const LINEAR_SCHEMA_SCAN_LIMIT: usize = 32;

/// Encoded-variable width selected by the stream magic number.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum KvIrEncoding {
    /// Four-byte encoded variables.
    FourByte,
    /// Eight-byte encoded variables.
    EightByte,
}

impl KvIrEncoding {
    /// Classifies an exact four-byte current-protocol magic number.
    ///
    /// Prefixes, buffers containing bytes after the magic number, and unknown protocol bytes are
    /// not classified. This lets callers probe a buffered input without duplicating wire constants
    /// or consuming any bytes.
    #[must_use]
    pub fn from_magic_number(magic: &[u8]) -> Option<Self> {
        if magic == FOUR_BYTE_MAGIC {
            Some(Self::FourByte)
        } else if magic == EIGHT_BYTE_MAGIC {
            Some(Self::EightByte)
        } else {
            None
        }
    }
}

/// Independent hard limits for untrusted KV-IR input.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KvIrLimits {
    stream_bytes: u64,
    metadata_bytes: u64,
    unit_bytes: u64,
    streams: u64,
    units_per_stream: u64,
    schema_nodes_per_namespace: u64,
    nesting_depth: u64,
    metadata_values: u64,
    values_per_event: u64,
    scalar_bytes: u64,
    encoded_components_per_value: u64,
}

impl KvIrLimits {
    /// Conservative defaults. The protocol itself caps metadata at 64 KiB.
    pub const DEFAULT: Self = Self {
        stream_bytes: GIBIBYTE,
        metadata_bytes: 64 * 1024,
        unit_bytes: 16 * MEBIBYTE,
        streams: 1024,
        units_per_stream: 10_000_000,
        schema_nodes_per_namespace: 1_000_000,
        nesting_depth: 256,
        metadata_values: 1_000_000,
        values_per_event: 1_000_000,
        scalar_bytes: 8 * MEBIBYTE,
        encoded_components_per_value: 1_000_000,
    };

    /// Creates the default limit set for subsequent builder-style overrides.
    #[must_use]
    pub const fn new() -> Self {
        Self::DEFAULT
    }

    /// Replaces the maximum bytes from one magic number through its end marker.
    #[must_use]
    pub const fn with_max_stream_bytes(mut self, value: u64) -> Self {
        self.stream_bytes = value;
        self
    }

    /// Replaces the maximum JSON metadata payload size.
    #[must_use]
    pub const fn with_max_metadata_bytes(mut self, value: u64) -> Self {
        self.metadata_bytes = value;
        self
    }

    /// Replaces the maximum encoded size of one schema, event, offset, or end unit.
    #[must_use]
    pub const fn with_max_unit_bytes(mut self, value: u64) -> Self {
        self.unit_bytes = value;
        self
    }

    /// Replaces the maximum number of immediately concatenated streams.
    #[must_use]
    pub const fn with_max_streams(mut self, value: u64) -> Self {
        self.streams = value;
        self
    }

    /// Replaces the maximum number of units, including the end unit, in one stream.
    #[must_use]
    pub const fn with_max_units_per_stream(mut self, value: u64) -> Self {
        self.units_per_stream = value;
        self
    }

    /// Replaces the maximum non-root schema nodes in either namespace.
    #[must_use]
    pub const fn with_max_schema_nodes_per_namespace(mut self, value: u64) -> Self {
        self.schema_nodes_per_namespace = value;
        self
    }

    /// Replaces both the metadata JSON nesting and schema-tree depth limit.
    #[must_use]
    pub const fn with_max_nesting_depth(mut self, value: u64) -> Self {
        self.nesting_depth = value;
        self
    }

    /// Replaces the maximum JSON values in the metadata object.
    #[must_use]
    pub const fn with_max_metadata_values(mut self, value: u64) -> Self {
        self.metadata_values = value;
        self
    }

    /// Replaces the maximum key-value pairs in one log event.
    #[must_use]
    pub const fn with_max_values_per_event(mut self, value: u64) -> Self {
        self.values_per_event = value;
        self
    }

    /// Replaces the maximum bytes in any key, string, dictionary variable, or logtype.
    #[must_use]
    pub const fn with_max_scalar_bytes(mut self, value: u64) -> Self {
        self.scalar_bytes = value;
        self
    }

    /// Replaces the maximum encoded and dictionary components in one CLP string.
    #[must_use]
    pub const fn with_max_encoded_components_per_value(mut self, value: u64) -> Self {
        self.encoded_components_per_value = value;
        self
    }

    /// Maximum bytes in one complete stream.
    #[must_use]
    pub const fn max_stream_bytes(self) -> u64 {
        self.stream_bytes
    }

    /// Maximum metadata payload bytes.
    #[must_use]
    pub const fn max_metadata_bytes(self) -> u64 {
        self.metadata_bytes
    }

    /// Maximum bytes in one IR unit.
    #[must_use]
    pub const fn max_unit_bytes(self) -> u64 {
        self.unit_bytes
    }

    /// Maximum concatenated streams.
    #[must_use]
    pub const fn max_streams(self) -> u64 {
        self.streams
    }

    /// Maximum units in one stream.
    #[must_use]
    pub const fn max_units_per_stream(self) -> u64 {
        self.units_per_stream
    }

    /// Maximum non-root nodes in either schema namespace.
    #[must_use]
    pub const fn max_schema_nodes_per_namespace(self) -> u64 {
        self.schema_nodes_per_namespace
    }

    /// Maximum metadata or schema nesting depth.
    #[must_use]
    pub const fn max_nesting_depth(self) -> u64 {
        self.nesting_depth
    }

    /// Maximum metadata JSON values.
    #[must_use]
    pub const fn max_metadata_values(self) -> u64 {
        self.metadata_values
    }

    /// Maximum pairs in one event.
    #[must_use]
    pub const fn max_values_per_event(self) -> u64 {
        self.values_per_event
    }

    /// Maximum bytes in one string-like scalar.
    #[must_use]
    pub const fn max_scalar_bytes(self) -> u64 {
        self.scalar_bytes
    }

    /// Maximum components in one encoded text value.
    #[must_use]
    pub const fn max_encoded_components_per_value(self) -> u64 {
        self.encoded_components_per_value
    }
}

impl Default for KvIrLimits {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Configuration for [`KvIrReader`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KvIrOptions {
    limits: KvIrLimits,
}

impl KvIrOptions {
    /// Creates strict options with conservative limits.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            limits: KvIrLimits::DEFAULT,
        }
    }

    /// Replaces all hard limits.
    #[must_use]
    pub const fn with_limits(mut self, limits: KvIrLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Returns the configured limits.
    #[must_use]
    pub const fn limits(self) -> KvIrLimits {
        self.limits
    }
}

impl Default for KvIrOptions {
    fn default() -> Self {
        Self::new()
    }
}

/// Resource guarded by a KV-IR hard limit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum KvIrLimitResource {
    StreamBytes,
    MetadataBytes,
    UnitBytes,
    Streams,
    UnitsPerStream,
    SchemaNodesPerNamespace,
    NestingDepth,
    MetadataValues,
    ValuesPerEvent,
    ScalarBytes,
    EncodedComponentsPerValue,
}

impl Display for KvIrLimitResource {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::StreamBytes => "stream bytes",
            Self::MetadataBytes => "metadata bytes",
            Self::UnitBytes => "IR unit bytes",
            Self::Streams => "concatenated streams",
            Self::UnitsPerStream => "IR units per stream",
            Self::SchemaNodesPerNamespace => "schema nodes per namespace",
            Self::NestingDepth => "nesting depth",
            Self::MetadataValues => "metadata JSON values",
            Self::ValuesPerEvent => "values per log event",
            Self::ScalarBytes => "scalar bytes",
            Self::EncodedComponentsPerValue => "encoded text components per value",
        })
    }
}

/// Exact observation that exceeded one configured limit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KvIrLimitViolation {
    resource: KvIrLimitResource,
    actual: u64,
    limit: u64,
}

impl KvIrLimitViolation {
    const fn new(resource: KvIrLimitResource, actual: u64, limit: u64) -> Self {
        Self {
            resource,
            actual,
            limit,
        }
    }

    #[must_use]
    pub const fn resource(self) -> KvIrLimitResource {
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

impl Display for KvIrLimitViolation {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} {} exceeds limit {}",
            self.actual, self.resource, self.limit
        )
    }
}

impl Error for KvIrLimitViolation {}

/// Reusable buffer whose allocation failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum KvIrResource {
    Preamble,
    Unit,
    MetadataEvents,
    MetadataDecodedStrings,
    MetadataParserStack,
    SchemaNodes,
    SchemaKey,
    EventPairs,
    EventUserNodeIds,
    EncodedVariables,
    DictionaryVariables,
    ValidationSet,
    ProtocolVersion,
}

impl Display for KvIrResource {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Preamble => "KV-IR preamble buffer",
            Self::Unit => "KV-IR unit buffer",
            Self::MetadataEvents => "metadata JSON events",
            Self::MetadataDecodedStrings => "metadata decoded strings",
            Self::MetadataParserStack => "metadata parser stack",
            Self::SchemaNodes => "KV-IR schema nodes",
            Self::SchemaKey => "KV-IR schema key",
            Self::EventPairs => "KV-IR event pairs",
            Self::EventUserNodeIds => "KV-IR user node IDs",
            Self::EncodedVariables => "KV-IR encoded variables",
            Self::DictionaryVariables => "KV-IR dictionary variables",
            Self::ValidationSet => "KV-IR event validation set",
            Self::ProtocolVersion => "KV-IR protocol version",
        })
    }
}

/// Input fragment that was truncated.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum KvIrTruncatedContext {
    MagicNumber,
    MetadataHeader,
    MetadataPayload,
    UnitTag,
    SchemaNodeIdPayload,
    IntegerPayload,
    StringLength,
    StringPayload,
    EncodedText,
}

impl Display for KvIrTruncatedContext {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::MagicNumber => "magic number",
            Self::MetadataHeader => "metadata header",
            Self::MetadataPayload => "metadata payload",
            Self::UnitTag => "IR unit tag",
            Self::SchemaNodeIdPayload => "schema-tree node ID payload",
            Self::IntegerPayload => "integer payload",
            Self::StringLength => "string length",
            Self::StringPayload => "string payload",
            Self::EncodedText => "encoded text AST",
        })
    }
}

/// Protocol-shaped data rejected after successful byte reads.
#[derive(Debug)]
#[non_exhaustive]
pub enum KvIrInvalidData {
    InvalidMagicNumber([u8; 4]),
    UnsupportedMetadataEncoding(u8),
    UnsupportedMetadataLengthTag(u8),
    InvalidMetadataJson,
    MetadataMustBeObject,
    MissingProtocolVersion,
    ProtocolVersionMustBeString,
    LegacyUnstructuredVersion(String),
    UnsupportedProtocolVersion(String),
    UserDefinedMetadataMustBeObject,
    InvalidUnitTag(u8),
    InvalidSchemaNodeType(u8),
    InvalidParentIdTag(u8),
    InvalidNodeIdTag(u8),
    MissingParentNode {
        namespace: KvIrNamespace,
        node_id: u32,
    },
    ParentNodeIsNotObject {
        namespace: KvIrNamespace,
        node_id: u32,
    },
    DuplicateSchemaNode,
    InvalidKeyGroupOrdering,
    MissingSchemaNode {
        namespace: KvIrNamespace,
        node_id: u32,
    },
    RootNodeValue {
        namespace: KvIrNamespace,
    },
    DuplicateEventNode {
        namespace: KvIrNamespace,
        node_id: u32,
    },
    DuplicateSiblingKey,
    ObjectValueHasDescendant,
    ValueTypeMismatch,
    UnknownValueTag(u8),
    InvalidStringLength,
    InvalidEncodedTextTag(u8),
}

impl Display for KvIrInvalidData {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidMagicNumber(value) => {
                write!(formatter, "invalid magic number {value:02x?}")
            }
            Self::UnsupportedMetadataEncoding(tag) => {
                write!(formatter, "unsupported metadata encoding tag 0x{tag:02x}")
            }
            Self::UnsupportedMetadataLengthTag(tag) => {
                write!(formatter, "unsupported metadata length tag 0x{tag:02x}")
            }
            Self::InvalidMetadataJson => formatter.write_str("metadata is not valid JSON"),
            Self::MetadataMustBeObject => formatter.write_str("metadata must be a JSON object"),
            Self::MissingProtocolVersion => formatter.write_str("metadata is missing VERSION"),
            Self::ProtocolVersionMustBeString => {
                formatter.write_str("metadata VERSION must be a string")
            }
            Self::LegacyUnstructuredVersion(version) => write!(
                formatter,
                "legacy unstructured IR version {version} is not decoded by this KV reader"
            ),
            Self::UnsupportedProtocolVersion(version) => {
                write!(formatter, "unsupported KV-IR protocol version {version}")
            }
            Self::UserDefinedMetadataMustBeObject => {
                formatter.write_str("USER_DEFINED_METADATA must be an object")
            }
            Self::InvalidUnitTag(tag) => write!(formatter, "invalid IR unit tag 0x{tag:02x}"),
            Self::InvalidSchemaNodeType(tag) => {
                write!(formatter, "invalid schema node type tag 0x{tag:02x}")
            }
            Self::InvalidParentIdTag(tag) => {
                write!(formatter, "invalid schema parent-ID tag 0x{tag:02x}")
            }
            Self::InvalidNodeIdTag(tag) => write!(formatter, "invalid node-ID tag 0x{tag:02x}"),
            Self::MissingParentNode { namespace, node_id } => {
                write!(formatter, "missing {namespace} parent node {node_id}")
            }
            Self::ParentNodeIsNotObject { namespace, node_id } => {
                write!(
                    formatter,
                    "{namespace} parent node {node_id} is not an object"
                )
            }
            Self::DuplicateSchemaNode => formatter.write_str("duplicate schema node insertion"),
            Self::InvalidKeyGroupOrdering => {
                formatter.write_str("auto-generated node ID follows a user-generated node ID")
            }
            Self::MissingSchemaNode { namespace, node_id } => {
                write!(formatter, "missing {namespace} schema node {node_id}")
            }
            Self::RootNodeValue { namespace } => {
                write!(
                    formatter,
                    "{namespace} root node cannot carry an event value"
                )
            }
            Self::DuplicateEventNode { namespace, node_id } => {
                write!(formatter, "duplicate {namespace} event node {node_id}")
            }
            Self::DuplicateSiblingKey => {
                formatter.write_str("event selects duplicate sibling keys")
            }
            Self::ObjectValueHasDescendant => {
                formatter.write_str("null or empty object also has a selected descendant")
            }
            Self::ValueTypeMismatch => {
                formatter.write_str("event value type does not match its schema node")
            }
            Self::UnknownValueTag(tag) => write!(formatter, "unknown value tag 0x{tag:02x}"),
            Self::InvalidStringLength => formatter.write_str("invalid negative string length"),
            Self::InvalidEncodedTextTag(tag) => {
                write!(formatter, "invalid encoded text component tag 0x{tag:02x}")
            }
        }
    }
}

/// Fatal decoder error with exact stream and byte context.
#[derive(Debug)]
pub struct KvIrError {
    input_offset: u64,
    stream_index: u64,
    unit_index: Option<u64>,
    kind: KvIrErrorKind,
}

impl KvIrError {
    #[must_use]
    pub const fn input_offset(&self) -> u64 {
        self.input_offset
    }

    #[must_use]
    pub const fn stream_index(&self) -> u64 {
        self.stream_index
    }

    #[must_use]
    pub const fn unit_index(&self) -> Option<u64> {
        self.unit_index
    }

    #[must_use]
    pub const fn kind(&self) -> &KvIrErrorKind {
        &self.kind
    }
}

impl Display for KvIrError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        if let Some(unit_index) = self.unit_index {
            write!(
                formatter,
                "KV-IR stream {} unit {} at input byte {}: {}",
                self.stream_index, unit_index, self.input_offset, self.kind
            )
        } else {
            write!(
                formatter,
                "KV-IR stream {} at input byte {}: {}",
                self.stream_index, self.input_offset, self.kind
            )
        }
    }
}

impl Error for KvIrError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match &self.kind {
            KvIrErrorKind::Input(source) => Some(source),
            KvIrErrorKind::Limit(source) => Some(source),
            _ => None,
        }
    }
}

/// Fatal error category.
#[derive(Debug)]
#[non_exhaustive]
pub enum KvIrErrorKind {
    Input(io::Error),
    Truncated {
        context: KvIrTruncatedContext,
        missing_bytes: u64,
    },
    Invalid(KvIrInvalidData),
    Limit(KvIrLimitViolation),
    AllocationFailed {
        resource: KvIrResource,
        requested_additional: usize,
    },
    SizeOverflow,
}

impl Display for KvIrErrorKind {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Input(source) => write!(formatter, "input read failed: {source}"),
            Self::Truncated {
                context,
                missing_bytes,
            } => write!(
                formatter,
                "truncated {context}; missing at least {missing_bytes} byte(s)"
            ),
            Self::Invalid(source) => Display::fmt(source, formatter),
            Self::Limit(source) => Display::fmt(source, formatter),
            Self::AllocationFailed {
                resource,
                requested_additional,
            } => write!(
                formatter,
                "failed to reserve {requested_additional} additional units for {resource}"
            ),
            Self::SizeOverflow => formatter.write_str("KV-IR size counter overflow"),
        }
    }
}

/// Reader or caller-owned sink failure.
#[derive(Debug)]
#[non_exhaustive]
pub enum KvIrReadError<E> {
    Reader(KvIrError),
    Sink {
        stream_index: u64,
        unit_index: Option<u64>,
        source: E,
    },
}

impl<E: Display> Display for KvIrReadError<E> {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Reader(source) => Display::fmt(source, formatter),
            Self::Sink {
                stream_index,
                unit_index,
                source,
            } => write!(
                formatter,
                "KV-IR sink failed for stream {stream_index}, unit {unit_index:?}: {source}"
            ),
        }
    }
}

impl<E: Error + 'static> Error for KvIrReadError<E> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Reader(source) => Some(source),
            Self::Sink { source, .. } => Some(source),
        }
    }
}

/// Schema namespace encoded by the sign of a parent/node ID.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum KvIrNamespace {
    AutoGenerated,
    UserGenerated,
}

impl Display for KvIrNamespace {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::AutoGenerated => "auto-generated",
            Self::UserGenerated => "user-generated",
        })
    }
}

/// Schema node types supported by protocol version 0.1.0.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum KvIrNodeType {
    Integer,
    Float,
    Boolean,
    String,
    UnstructuredArray,
    Object,
}

/// Exact integer payload width selected by its value tag.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum KvIrIntegerWidth {
    One,
    Two,
    Four,
    Eight,
}

/// Decoded integer with its protocol width.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KvIrInteger {
    value: i64,
    width: KvIrIntegerWidth,
}

impl KvIrInteger {
    #[must_use]
    pub const fn value(self) -> i64 {
        self.value
    }

    #[must_use]
    pub const fn width(self) -> KvIrIntegerWidth {
        self.width
    }
}

/// One exact encoded variable in a CLP string.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum KvIrEncodedVariable {
    FourByte(i32),
    EightByte(i64),
}

/// Borrowed encoded-text AST components.
#[derive(Clone, Copy, Debug)]
pub struct KvIrEncodedText<'a> {
    encoding: KvIrEncoding,
    variables: &'a [StoredEncodedVariable],
    dictionary_spans: &'a [ByteSpan],
    logtype: &'a [u8],
    unit: &'a [u8],
}

impl<'a> KvIrEncodedText<'a> {
    #[must_use]
    pub const fn encoding(self) -> KvIrEncoding {
        self.encoding
    }

    #[allow(clippy::must_use_candidate)]
    pub fn encoded_variables(self) -> impl ExactSizeIterator<Item = KvIrEncodedVariable> + 'a {
        self.variables.iter().map(|value| value.value)
    }

    #[must_use]
    pub fn dictionary_variables(self) -> KvIrStrings<'a> {
        KvIrStrings {
            unit: self.unit,
            spans: self.dictionary_spans.iter(),
        }
    }

    #[must_use]
    pub const fn logtype(self) -> &'a [u8] {
        self.logtype
    }
}

/// Iterator over borrowed encoded-text dictionary variables.
#[derive(Clone, Debug)]
pub struct KvIrStrings<'a> {
    unit: &'a [u8],
    spans: slice::Iter<'a, ByteSpan>,
}

impl<'a> Iterator for KvIrStrings<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<Self::Item> {
        self.spans.next().map(|span| span.resolve(self.unit))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.spans.size_hint()
    }
}

impl ExactSizeIterator for KvIrStrings<'_> {}
impl std::iter::FusedIterator for KvIrStrings<'_> {}

/// Decoded typed value plus its exact source packet.
#[derive(Clone, Copy, Debug)]
pub struct KvIrValue<'a> {
    raw: &'a [u8],
    kind: KvIrValueKind<'a>,
}

impl<'a> KvIrValue<'a> {
    #[must_use]
    pub const fn raw_packet(self) -> &'a [u8] {
        self.raw
    }

    #[must_use]
    pub const fn kind(self) -> KvIrValueKind<'a> {
        self.kind
    }
}

/// Typed value semantics exposed by protocol 0.1.0.
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub enum KvIrValueKind<'a> {
    Integer(KvIrInteger),
    Float { bits: u64 },
    Boolean(bool),
    String(&'a [u8]),
    EncodedText(KvIrEncodedText<'a>),
    Null,
    EmptyObject,
}

/// One borrowed node-ID/value pair.
#[derive(Clone, Copy, Debug)]
pub struct KvIrPair<'a> {
    namespace: KvIrNamespace,
    node_id: u32,
    value: KvIrValue<'a>,
}

impl<'a> KvIrPair<'a> {
    #[must_use]
    pub const fn namespace(self) -> KvIrNamespace {
        self.namespace
    }

    #[must_use]
    pub const fn node_id(self) -> u32 {
        self.node_id
    }

    #[must_use]
    pub const fn value(self) -> KvIrValue<'a> {
        self.value
    }
}

/// Iterator over one event's borrowed pairs.
#[derive(Clone, Debug)]
pub struct KvIrPairs<'a> {
    unit: &'a [u8],
    stored: slice::Iter<'a, StoredPair>,
    variables: &'a [StoredEncodedVariable],
    dictionaries: &'a [ByteSpan],
}

impl<'a> Iterator for KvIrPairs<'a> {
    type Item = KvIrPair<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        self.stored
            .next()
            .map(|pair| pair.resolve(self.unit, self.variables, self.dictionaries))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.stored.size_hint()
    }
}

impl ExactSizeIterator for KvIrPairs<'_> {}
impl std::iter::FusedIterator for KvIrPairs<'_> {}

/// Validated stream preamble borrowed for one sink call.
#[derive(Clone, Copy, Debug)]
pub struct KvIrStreamHeader<'a> {
    encoding: KvIrEncoding,
    protocol_version: &'a str,
    metadata_json: &'a [u8],
    metadata_decoded: &'a str,
    metadata_events: &'a [StoredEvent],
    raw_preamble: &'a [u8],
    stream_index: u64,
    input_offset: u64,
}

impl<'a> KvIrStreamHeader<'a> {
    #[must_use]
    pub const fn encoding(self) -> KvIrEncoding {
        self.encoding
    }

    #[must_use]
    pub const fn protocol_version(self) -> &'a str {
        self.protocol_version
    }

    #[must_use]
    pub const fn metadata_json(self) -> &'a [u8] {
        self.metadata_json
    }

    /// Returns the validated metadata object as a borrowed flat JSON traversal.
    ///
    /// The traversal reuses the decoder's bounded preamble parse; callers that need structured
    /// stream metadata do not need to parse [`Self::metadata_json`] a second time.
    #[must_use]
    pub fn metadata_events(self) -> JsonEvents<'a> {
        JsonEvents::new(
            self.metadata_json,
            self.metadata_decoded,
            self.metadata_events,
        )
    }

    #[must_use]
    pub const fn raw_preamble(self) -> &'a [u8] {
        self.raw_preamble
    }

    #[must_use]
    pub const fn stream_index(self) -> u64 {
        self.stream_index
    }

    #[must_use]
    pub const fn input_offset(self) -> u64 {
        self.input_offset
    }
}

/// One inserted schema node. Its ID is the implicit insertion index in its namespace tree.
#[derive(Clone, Copy, Debug)]
pub struct KvIrSchemaNode<'a> {
    namespace: KvIrNamespace,
    node_id: u32,
    parent_id: u32,
    depth: u64,
    key: &'a [u8],
    node_type: KvIrNodeType,
    raw_unit: &'a [u8],
    stream_index: u64,
    unit_index: u64,
    input_offset: u64,
}

impl<'a> KvIrSchemaNode<'a> {
    #[must_use]
    pub const fn namespace(self) -> KvIrNamespace {
        self.namespace
    }

    #[must_use]
    pub const fn node_id(self) -> u32 {
        self.node_id
    }

    #[must_use]
    pub const fn parent_id(self) -> u32 {
        self.parent_id
    }

    #[must_use]
    pub const fn depth(self) -> u64 {
        self.depth
    }

    #[must_use]
    pub const fn key(self) -> &'a [u8] {
        self.key
    }

    #[must_use]
    pub const fn node_type(self) -> KvIrNodeType {
        self.node_type
    }

    #[must_use]
    pub const fn raw_unit(&self) -> &'a [u8] {
        self.raw_unit
    }

    #[must_use]
    pub const fn stream_index(self) -> u64 {
        self.stream_index
    }

    #[must_use]
    pub const fn unit_index(self) -> u64 {
        self.unit_index
    }

    #[must_use]
    pub const fn input_offset(self) -> u64 {
        self.input_offset
    }
}

/// One immutable entry in the schema visible to a decoded log event.
///
/// Unlike [`KvIrSchemaNode`], this view does not retain the schema insertion unit. It allows a
/// synchronous consumer to resolve an event's node IDs without maintaining a second schema copy.
#[derive(Clone, Copy, Debug)]
pub struct KvIrSchemaEntry<'a> {
    namespace: KvIrNamespace,
    node_id: u32,
    node: &'a SchemaNodeOwned,
    key: &'a [u8],
}

impl<'a> KvIrSchemaEntry<'a> {
    #[must_use]
    pub const fn namespace(self) -> KvIrNamespace {
        self.namespace
    }

    #[must_use]
    pub const fn node_id(self) -> u32 {
        self.node_id
    }

    /// Returns the parent node ID, or `None` for the namespace root.
    #[must_use]
    pub const fn parent_id(self) -> Option<u32> {
        if self.node_id == 0 {
            None
        } else {
            Some(self.node.parent_id)
        }
    }

    #[must_use]
    #[allow(clippy::cast_lossless)]
    pub const fn depth(self) -> u64 {
        self.node.depth as u64
    }

    #[must_use]
    pub const fn key(self) -> &'a [u8] {
        self.key
    }

    #[must_use]
    pub const fn node_type(self) -> KvIrNodeType {
        self.node.node_type
    }
}

/// One complete validated key-value log-event unit.
#[derive(Clone, Copy, Debug)]
pub struct KvIrLogEvent<'a> {
    raw_unit: &'a [u8],
    pairs: &'a [StoredPair],
    variables: &'a [StoredEncodedVariable],
    dictionaries: &'a [ByteSpan],
    auto_schema: &'a [SchemaNodeOwned],
    user_schema: &'a [SchemaNodeOwned],
    schema_keys: &'a [u8],
    utc_offset_millis: i64,
    stream_index: u64,
    unit_index: u64,
    event_index: u64,
    input_offset: u64,
}

impl<'a> KvIrLogEvent<'a> {
    #[must_use]
    pub const fn raw_unit(self) -> &'a [u8] {
        self.raw_unit
    }

    #[must_use]
    pub fn pairs(&self) -> KvIrPairs<'a> {
        KvIrPairs {
            unit: self.raw_unit,
            stored: self.pairs.iter(),
            variables: self.variables,
            dictionaries: self.dictionaries,
        }
    }

    /// Returns each pair's namespace and node ID, leaving its value in the unit buffer.
    ///
    /// [`Self::pairs`] rebuilds a whole [`KvIrPair`] per element: a match over the value kind and
    /// two byte-span resolutions. A caller that only files pairs under their node never reads the
    /// value, and resolving one for it is the largest avoidable cost in a scan, so this yields the
    /// two fields such a caller does read.
    pub fn pair_slots(&self) -> impl ExactSizeIterator<Item = (KvIrNamespace, u32)> + 'a {
        self.pairs.iter().map(|pair| (pair.namespace, pair.node_id))
    }

    /// Returns the number of node-ID/value pairs in this event.
    #[must_use]
    pub const fn pair_count(&self) -> usize {
        self.pairs.len()
    }

    /// Returns one pair by its zero-based protocol order.
    #[must_use]
    pub fn pair(&self, index: usize) -> Option<KvIrPair<'a>> {
        self.pairs
            .get(index)
            .map(|pair| pair.resolve(self.raw_unit, self.variables, self.dictionaries))
    }

    /// Returns the number of schema entries in one namespace, including its root node.
    #[must_use]
    pub const fn schema_node_count(&self, namespace: KvIrNamespace) -> usize {
        match namespace {
            KvIrNamespace::AutoGenerated => self.auto_schema.len(),
            KvIrNamespace::UserGenerated => self.user_schema.len(),
        }
    }

    /// Resolves a protocol node ID against the current stream schema.
    #[must_use]
    pub fn schema_node(
        &self,
        namespace: KvIrNamespace,
        node_id: u32,
    ) -> Option<KvIrSchemaEntry<'a>> {
        let schema = match namespace {
            KvIrNamespace::AutoGenerated => self.auto_schema,
            KvIrNamespace::UserGenerated => self.user_schema,
        };
        let node_index = usize::try_from(node_id).ok()?;
        let node = schema.get(node_index)?;
        let key = node.key(self.schema_keys)?;
        Some(KvIrSchemaEntry {
            namespace,
            node_id,
            node,
            key,
        })
    }

    #[must_use]
    pub const fn utc_offset_millis(&self) -> i64 {
        self.utc_offset_millis
    }

    #[must_use]
    pub const fn stream_index(&self) -> u64 {
        self.stream_index
    }

    #[must_use]
    pub const fn unit_index(&self) -> u64 {
        self.unit_index
    }

    #[must_use]
    pub const fn event_index(&self) -> u64 {
        self.event_index
    }

    #[must_use]
    pub const fn input_offset(&self) -> u64 {
        self.input_offset
    }
}

/// UTC-offset change unit.
#[derive(Clone, Copy, Debug)]
pub struct KvIrUtcOffsetChange<'a> {
    old_offset_millis: i64,
    new_offset_millis: i64,
    raw_unit: &'a [u8],
    stream_index: u64,
    unit_index: u64,
    input_offset: u64,
}

impl<'a> KvIrUtcOffsetChange<'a> {
    #[must_use]
    pub const fn old_offset_millis(self) -> i64 {
        self.old_offset_millis
    }

    #[must_use]
    pub const fn new_offset_millis(self) -> i64 {
        self.new_offset_millis
    }

    #[must_use]
    pub const fn raw_unit(self) -> &'a [u8] {
        self.raw_unit
    }

    #[must_use]
    pub const fn stream_index(self) -> u64 {
        self.stream_index
    }

    #[must_use]
    pub const fn unit_index(self) -> u64 {
        self.unit_index
    }

    #[must_use]
    pub const fn input_offset(self) -> u64 {
        self.input_offset
    }
}

/// Explicit `0x00` end unit.
#[derive(Clone, Copy, Debug)]
pub struct KvIrStreamEnd<'a> {
    raw_unit: &'a [u8],
    stream_index: u64,
    unit_index: u64,
    input_offset: u64,
    stream_bytes: u64,
}

impl<'a> KvIrStreamEnd<'a> {
    #[must_use]
    pub const fn raw_unit(self) -> &'a [u8] {
        self.raw_unit
    }

    #[must_use]
    pub const fn stream_index(self) -> u64 {
        self.stream_index
    }

    #[must_use]
    pub const fn unit_index(self) -> u64 {
        self.unit_index
    }

    #[must_use]
    pub const fn input_offset(self) -> u64 {
        self.input_offset
    }

    #[must_use]
    pub const fn stream_bytes(self) -> u64 {
        self.stream_bytes
    }
}

/// Borrowed callback item. Every byte view is valid only for the current sink call.
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub enum KvIrItem<'a> {
    StreamStart(KvIrStreamHeader<'a>),
    SchemaNode(KvIrSchemaNode<'a>),
    LogEvent(KvIrLogEvent<'a>),
    UtcOffsetChange(KvIrUtcOffsetChange<'a>),
    StreamEnd(KvIrStreamEnd<'a>),
}

impl KvIrItem<'_> {
    /// Returns the stable kind of this borrowed item.
    #[must_use]
    pub const fn kind(self) -> KvIrItemKind {
        match self {
            Self::StreamStart(_) => KvIrItemKind::StreamStart,
            Self::SchemaNode(_) => KvIrItemKind::SchemaNode,
            Self::LogEvent(_) => KvIrItemKind::LogEvent,
            Self::UtcOffsetChange(_) => KvIrItemKind::UtcOffsetChange,
            Self::StreamEnd(_) => KvIrItemKind::StreamEnd,
        }
    }
}

/// Kind emitted by one incremental decoder operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum KvIrItemKind {
    StreamStart,
    SchemaNode,
    LogEvent,
    UtcOffsetChange,
    StreamEnd,
}

/// Synchronous destination for validated borrowed KV-IR items.
pub trait KvIrSink {
    type Error;

    /// Accepts one complete item.
    ///
    /// # Errors
    ///
    /// Returns the caller's error if the item cannot be accepted.
    fn write_item(&mut self, item: KvIrItem<'_>) -> Result<(), Self::Error>;
}

impl<F, E> KvIrSink for F
where
    F: for<'item> FnMut(KvIrItem<'item>) -> Result<(), E>,
{
    type Error = E;

    fn write_item(&mut self, item: KvIrItem<'_>) -> Result<(), Self::Error> {
        self(item)
    }
}

/// Counters completed so far.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct KvIrStats {
    input_bytes: u64,
    streams: u64,
    units: u64,
    schema_nodes: u64,
    log_events: u64,
    utc_offset_changes: u64,
}

impl KvIrStats {
    #[must_use]
    pub const fn input_bytes(self) -> u64 {
        self.input_bytes
    }

    #[must_use]
    pub const fn streams(self) -> u64 {
        self.streams
    }

    #[must_use]
    pub const fn units(self) -> u64 {
        self.units
    }

    #[must_use]
    pub const fn schema_nodes(self) -> u64 {
        self.schema_nodes
    }

    #[must_use]
    pub const fn log_events(self) -> u64 {
        self.log_events
    }

    #[must_use]
    pub const fn utc_offset_changes(self) -> u64 {
        self.utc_offset_changes
    }
}

#[derive(Clone, Copy, Debug)]
struct ByteSpan {
    start: usize,
    end: usize,
}

impl ByteSpan {
    const fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }

    fn resolve(self, source: &[u8]) -> &[u8] {
        &source[self.start..self.end]
    }
}

#[derive(Clone, Copy, Debug)]
struct StoredEncodedVariable {
    value: KvIrEncodedVariable,
}

#[derive(Clone, Debug)]
enum StoredValueKind {
    Integer(KvIrInteger),
    Float {
        bits: u64,
    },
    Boolean(bool),
    String(ByteSpan),
    EncodedText {
        encoding: KvIrEncoding,
        variables: Range<usize>,
        dictionaries: Range<usize>,
        logtype: ByteSpan,
    },
    Null,
    EmptyObject,
}

#[derive(Clone, Copy)]
enum MetadataPendingKey {
    Version,
    UserMetadata,
    Ignore,
}

#[derive(Clone, Copy)]
enum MetadataFieldState {
    Missing,
    Valid,
    Invalid,
}

#[derive(Clone, Debug)]
struct StoredPair {
    namespace: KvIrNamespace,
    node_id: u32,
    raw: ByteSpan,
    kind: StoredValueKind,
}

impl StoredPair {
    fn resolve<'a>(
        &'a self,
        unit: &'a [u8],
        variables: &'a [StoredEncodedVariable],
        dictionaries: &'a [ByteSpan],
    ) -> KvIrPair<'a> {
        let kind = match &self.kind {
            StoredValueKind::Integer(value) => KvIrValueKind::Integer(*value),
            StoredValueKind::Float { bits } => KvIrValueKind::Float { bits: *bits },
            StoredValueKind::Boolean(value) => KvIrValueKind::Boolean(*value),
            StoredValueKind::String(span) => KvIrValueKind::String(span.resolve(unit)),
            StoredValueKind::EncodedText {
                encoding,
                variables: variable_range,
                dictionaries: dictionary_range,
                logtype,
            } => KvIrValueKind::EncodedText(KvIrEncodedText {
                encoding: *encoding,
                variables: &variables[variable_range.clone()],
                dictionary_spans: &dictionaries[dictionary_range.clone()],
                logtype: logtype.resolve(unit),
                unit,
            }),
            StoredValueKind::Null => KvIrValueKind::Null,
            StoredValueKind::EmptyObject => KvIrValueKind::EmptyObject,
        };
        KvIrPair {
            namespace: self.namespace,
            node_id: self.node_id,
            value: KvIrValue {
                raw: self.raw.resolve(unit),
                kind,
            },
        }
    }
}

#[derive(Debug)]
struct SchemaNodeOwned {
    parent_id: u32,
    depth: u32,
    key_start: usize,
    key_length: u32,
    node_type: KvIrNodeType,
    sibling_group_id: u32,
    last_event_epoch: u32,
}

impl SchemaNodeOwned {
    const fn root() -> Self {
        Self {
            parent_id: 0,
            depth: 0,
            key_start: 0,
            key_length: 0,
            node_type: KvIrNodeType::Object,
            sibling_group_id: u32::MAX,
            last_event_epoch: 0,
        }
    }

    fn key<'a>(&self, keys: &'a [u8]) -> Option<&'a [u8]> {
        let length = usize::try_from(self.key_length).ok()?;
        let end = self.key_start.checked_add(length)?;
        keys.get(self.key_start..end)
    }
}

const _: () = {
    assert!(std::mem::size_of::<SchemaNodeOwned>() <= 32);
};

#[derive(Debug)]
struct SchemaSiblingGroup {
    key_node_id: u32,
    node_type_mask: u8,
    last_event_epoch: u32,
}

#[derive(Debug)]
struct SchemaIndex {
    slots: Vec<u32>,
    sibling_groups: Vec<SchemaSiblingGroup>,
    #[cfg(test)]
    probes: Cell<u64>,
}

impl SchemaIndex {
    const fn new() -> Self {
        Self {
            slots: Vec::new(),
            sibling_groups: Vec::new(),
            #[cfg(test)]
            probes: Cell::new(0),
        }
    }

    fn clear(&mut self) {
        self.slots.fill(EMPTY_SCHEMA_INDEX_SLOT);
        self.sibling_groups.clear();
    }

    fn locate_group(
        &self,
        schema: &[SchemaNodeOwned],
        keys: &[u8],
        hash_builder: &ahash::RandomState,
        parent_id: u32,
        key: &[u8],
    ) -> (u64, Option<u32>) {
        if self.slots.is_empty() {
            for (group_index, group) in self.sibling_groups.iter().enumerate() {
                let Some(key_node_index) = usize::try_from(group.key_node_id).ok() else {
                    continue;
                };
                let Some(key_node) = schema.get(key_node_index) else {
                    continue;
                };
                if key_node.parent_id == parent_id
                    && key_node.key(keys).is_some_and(|candidate| candidate == key)
                {
                    return (0, u32::try_from(group_index).ok());
                }
            }
            return (0, None);
        }
        let hash = Self::hash_key(hash_builder, parent_id, key);

        let mask = self.slots.len() - 1;
        let mut slot_index = Self::starting_slot(hash, mask);
        #[cfg(test)]
        let mut probes = 0_u64;
        loop {
            let encoded_group_id = self.slots[slot_index];
            if encoded_group_id == EMPTY_SCHEMA_INDEX_SLOT {
                #[cfg(test)]
                self.probes.set(self.probes.get().saturating_add(probes));
                return (hash, None);
            }

            #[cfg(test)]
            {
                probes = probes.saturating_add(1);
            }
            let group_id = encoded_group_id - 1;
            let Some(group) = usize::try_from(group_id)
                .ok()
                .and_then(|index| self.sibling_groups.get(index))
            else {
                #[cfg(test)]
                self.probes.set(self.probes.get().saturating_add(probes));
                return (hash, None);
            };
            let Some(key_node_index) = usize::try_from(group.key_node_id).ok() else {
                #[cfg(test)]
                self.probes.set(self.probes.get().saturating_add(probes));
                return (hash, None);
            };
            let Some(key_node) = schema.get(key_node_index) else {
                #[cfg(test)]
                self.probes.set(self.probes.get().saturating_add(probes));
                return (hash, None);
            };
            if key_node.parent_id == parent_id
                && key_node.key(keys).is_some_and(|candidate| candidate == key)
            {
                #[cfg(test)]
                self.probes.set(self.probes.get().saturating_add(probes));
                return (hash, Some(group_id));
            }
            slot_index = slot_index.wrapping_add(1) & mask;
        }
    }

    fn try_prepare_group(
        &mut self,
        schema: &[SchemaNodeOwned],
        keys: &[u8],
        hash_builder: &ahash::RandomState,
    ) -> Result<(), SchemaIndexGrowthError> {
        self.sibling_groups.try_reserve(1).map_err(|_| {
            SchemaIndexGrowthError::AllocationFailed {
                requested_additional: 1,
            }
        })?;
        let next_group_count = self
            .sibling_groups
            .len()
            .checked_add(1)
            .ok_or(SchemaIndexGrowthError::SizeOverflow)?;
        if self.slots.is_empty() && next_group_count <= LINEAR_SCHEMA_SCAN_LIMIT {
            return Ok(());
        }
        let mut required_slots = self.slots.len();
        if required_slots == 0 {
            required_slots = 16;
        }
        while next_group_count > required_slots / 4 * 3 {
            required_slots = required_slots
                .checked_mul(2)
                .ok_or(SchemaIndexGrowthError::SizeOverflow)?;
        }
        if required_slots == self.slots.len() {
            return Ok(());
        }
        self.rebuild_slots(schema, keys, hash_builder, required_slots)
    }

    #[cold]
    #[inline(never)]
    fn rebuild_slots(
        &mut self,
        schema: &[SchemaNodeOwned],
        keys: &[u8],
        hash_builder: &ahash::RandomState,
        required_slots: usize,
    ) -> Result<(), SchemaIndexGrowthError> {
        let requested_additional = required_slots.saturating_sub(self.slots.len());
        let mut new_slots = Vec::new();
        new_slots.try_reserve_exact(required_slots).map_err(|_| {
            SchemaIndexGrowthError::AllocationFailed {
                requested_additional,
            }
        })?;
        new_slots.resize(required_slots, EMPTY_SCHEMA_INDEX_SLOT);
        for (group_index, group) in self.sibling_groups.iter().enumerate() {
            let key_node_index = usize::try_from(group.key_node_id)
                .map_err(|_| SchemaIndexGrowthError::SizeOverflow)?;
            let key_node = schema
                .get(key_node_index)
                .ok_or(SchemaIndexGrowthError::SizeOverflow)?;
            let key = key_node
                .key(keys)
                .ok_or(SchemaIndexGrowthError::SizeOverflow)?;
            let hash = Self::hash_key(hash_builder, key_node.parent_id, key);
            let group_id =
                u32::try_from(group_index).map_err(|_| SchemaIndexGrowthError::SizeOverflow)?;
            Self::insert_slot(&mut new_slots, hash, group_id);
        }
        self.slots = new_slots;
        Ok(())
    }

    fn insert_group(&mut self, hash: u64, key_node_id: u32, node_type_mask: u8, group_id: u32) {
        debug_assert_eq!(
            usize::try_from(group_id).ok(),
            Some(self.sibling_groups.len())
        );
        self.sibling_groups.push(SchemaSiblingGroup {
            key_node_id,
            node_type_mask,
            last_event_epoch: 0,
        });
        if !self.slots.is_empty() {
            Self::insert_slot(&mut self.slots, hash, group_id);
        }
    }

    fn insert_slot(slots: &mut [u32], hash: u64, group_id: u32) {
        let mask = slots.len() - 1;
        let mut slot_index = Self::starting_slot(hash, mask);
        while slots[slot_index] != EMPTY_SCHEMA_INDEX_SLOT {
            slot_index = slot_index.wrapping_add(1) & mask;
        }
        slots[slot_index] = group_id + 1;
    }

    fn hash_key(hash_builder: &ahash::RandomState, parent_id: u32, key: &[u8]) -> u64 {
        hash_builder.hash_one((parent_id, key))
    }

    fn starting_slot(hash: u64, mask: usize) -> usize {
        let mask_u64 = u64::try_from(mask).unwrap_or(u64::MAX);
        usize::try_from(hash & mask_u64).unwrap_or_default()
    }

    #[cfg(test)]
    const fn probes(&self) -> u64 {
        self.probes.get()
    }
}

#[derive(Debug)]
enum SchemaIndexGrowthError {
    SizeOverflow,
    AllocationFailed { requested_additional: usize },
}

/// Streaming protocol decoder with fixed-size input buffering and bounded reusable unit storage.
pub struct KvIrReader<R> {
    input: R,
    options: KvIrOptions,
    input_buffer: [u8; INPUT_BUFFER_BYTES],
    input_start: usize,
    input_end: usize,
    reached_eof: bool,
    input_offset: u64,
    stream_bytes: u64,
    preamble: Vec<u8>,
    metadata_start: usize,
    metadata_decoded: String,
    metadata_events: Vec<StoredEvent>,
    metadata_stack: Vec<Frame>,
    protocol_version: String,
    unit: Vec<u8>,
    event_pairs: Vec<StoredPair>,
    user_node_ids: Vec<u32>,
    encoded_variables: Vec<StoredEncodedVariable>,
    dictionary_spans: Vec<ByteSpan>,
    auto_schema: Vec<SchemaNodeOwned>,
    user_schema: Vec<SchemaNodeOwned>,
    schema_keys: Vec<u8>,
    auto_schema_index: SchemaIndex,
    user_schema_index: SchemaIndex,
    schema_hash_builder: ahash::RandomState,
    validation_epoch: u32,
    current_utc_offset: i64,
    current_stream_index: u64,
    current_unit_index: Option<u64>,
    next_unit_index: u64,
    next_event_index: u64,
    in_stream: bool,
    stats: KvIrStats,
    finished: bool,
}

impl<R: Read> KvIrReader<R> {
    /// Creates a decoder over a caller-owned byte stream.
    #[must_use]
    pub fn new(input: R, options: KvIrOptions) -> Self {
        Self {
            input,
            options,
            input_buffer: [0; INPUT_BUFFER_BYTES],
            input_start: 0,
            input_end: 0,
            reached_eof: false,
            input_offset: 0,
            stream_bytes: 0,
            preamble: Vec::new(),
            metadata_start: 0,
            metadata_decoded: String::new(),
            metadata_events: Vec::new(),
            metadata_stack: Vec::new(),
            protocol_version: String::new(),
            unit: Vec::new(),
            event_pairs: Vec::new(),
            user_node_ids: Vec::new(),
            encoded_variables: Vec::new(),
            dictionary_spans: Vec::new(),
            auto_schema: Vec::new(),
            user_schema: Vec::new(),
            schema_keys: Vec::new(),
            auto_schema_index: SchemaIndex::new(),
            user_schema_index: SchemaIndex::new(),
            schema_hash_builder: ahash::RandomState::new(),
            validation_epoch: 0,
            current_utc_offset: 0,
            current_stream_index: 0,
            current_unit_index: None,
            next_unit_index: 0,
            next_event_index: 0,
            in_stream: false,
            stats: KvIrStats::default(),
            finished: false,
        }
    }

    /// Returns the immutable reader configuration.
    #[must_use]
    pub const fn options(&self) -> KvIrOptions {
        self.options
    }

    /// Returns counters accumulated so far.
    #[must_use]
    pub const fn stats(&self) -> KvIrStats {
        // Carried here rather than stored on every consumed byte, which put a write in the
        // innermost read loop for a counter only this accessor reads.
        let mut stats = self.stats;
        stats.input_bytes = self.input_offset;
        stats
    }

    /// Returns the underlying input, discarding unread buffered bytes.
    #[must_use]
    pub fn into_inner(self) -> R {
        self.input
    }

    /// Decodes all immediately concatenated streams and synchronously emits complete items.
    ///
    /// The input must contain at least one stream. Borrowed item data is invalidated by the next
    /// reader operation and must be copied by a sink that needs to retain it.
    ///
    /// # Errors
    ///
    /// Returns [`KvIrReadError::Reader`] for I/O, truncation, protocol, limit, accounting, or
    /// allocation failures. [`KvIrReadError::Sink`] preserves the caller's sink error.
    pub fn read_to_end<S: KvIrSink + ?Sized>(
        &mut self,
        sink: &mut S,
    ) -> Result<KvIrStats, KvIrReadError<S::Error>> {
        while self.read_next_item(sink)?.is_some() {
            // The incremental operation owns all protocol state transitions.
        }
        Ok(self.stats())
    }

    /// Decodes and synchronously emits at most one complete item.
    ///
    /// Returns the emitted item kind, or `None` after all immediately concatenated streams have
    /// ended. The input must contain at least one stream. A sink error occurs after the item has
    /// been consumed and committed, so a later call resumes at the following item. Reader errors
    /// may leave a partially consumed item and should be treated as terminal.
    ///
    /// # Errors
    ///
    /// Returns [`KvIrReadError::Reader`] for I/O, truncation, protocol, limit, accounting, or
    /// allocation failures. [`KvIrReadError::Sink`] preserves the caller's sink error.
    pub fn read_next_item<S: KvIrSink + ?Sized>(
        &mut self,
        sink: &mut S,
    ) -> Result<Option<KvIrItemKind>, KvIrReadError<S::Error>> {
        if self.finished {
            return Ok(None);
        }
        if !self.in_stream {
            return self.read_next_stream_start(sink);
        }
        self.read_next_unit(sink).map(Some)
    }

    fn read_next_stream_start<S: KvIrSink + ?Sized>(
        &mut self,
        sink: &mut S,
    ) -> Result<Option<KvIrItemKind>, KvIrReadError<S::Error>> {
        if !self.has_input().map_err(KvIrReadError::Reader)? {
            if self.stats.streams == 0 {
                return Err(KvIrReadError::Reader(self.error(
                    KvIrErrorKind::Truncated {
                        context: KvIrTruncatedContext::MagicNumber,
                        missing_bytes: 4,
                    },
                )));
            }
            self.finished = true;
            return Ok(None);
        }

        let stream_actual = self
            .stats
            .streams
            .checked_add(1)
            .ok_or_else(|| KvIrReadError::Reader(self.error(KvIrErrorKind::SizeOverflow)))?;
        if stream_actual > self.options.limits.streams {
            return Err(KvIrReadError::Reader(self.error(KvIrErrorKind::Limit(
                KvIrLimitViolation::new(
                    KvIrLimitResource::Streams,
                    stream_actual,
                    self.options.limits.streams,
                ),
            ))));
        }

        self.current_stream_index = self.stats.streams;
        self.current_unit_index = None;
        self.next_unit_index = 0;
        self.next_event_index = 0;
        self.stream_bytes = 0;
        self.current_utc_offset = 0;
        self.preamble.clear();
        self.reset_schemas().map_err(KvIrReadError::Reader)?;
        self.in_stream = true;
        self.read_preamble(sink)?;
        Ok(Some(KvIrItemKind::StreamStart))
    }

    // Keeping the complete unit dispatch visible makes the protocol state transition auditable.
    #[allow(clippy::too_many_lines)]
    fn read_next_unit<S: KvIrSink + ?Sized>(
        &mut self,
        sink: &mut S,
    ) -> Result<KvIrItemKind, KvIrReadError<S::Error>> {
        let limits = self.options.limits;
        let unit_index = self.next_unit_index;
        let unit_actual = unit_index
            .checked_add(1)
            .ok_or_else(|| KvIrReadError::Reader(self.error(KvIrErrorKind::SizeOverflow)))?;
        if unit_actual > limits.units_per_stream {
            return Err(KvIrReadError::Reader(self.error(KvIrErrorKind::Limit(
                KvIrLimitViolation::new(
                    KvIrLimitResource::UnitsPerStream,
                    unit_actual,
                    limits.units_per_stream,
                ),
            ))));
        }
        let total_units = self
            .stats
            .units
            .checked_add(1)
            .ok_or_else(|| KvIrReadError::Reader(self.error(KvIrErrorKind::SizeOverflow)))?;

        self.current_unit_index = Some(unit_index);
        self.unit.clear();
        let unit_offset = self.input_offset;
        let tag = self
            .read_unit_byte(KvIrTruncatedContext::UnitTag)
            .map_err(KvIrReadError::Reader)?;

        let (kind, ended, next_event_index, result) = match tag {
            0x00 => {
                let end = KvIrStreamEnd {
                    raw_unit: &self.unit,
                    stream_index: self.current_stream_index,
                    unit_index,
                    input_offset: unit_offset,
                    stream_bytes: self.stream_bytes,
                };
                let result = sink.write_item(KvIrItem::StreamEnd(end)).map_err(|source| {
                    KvIrReadError::Sink {
                        stream_index: self.current_stream_index,
                        unit_index: Some(unit_index),
                        source,
                    }
                });
                (KvIrItemKind::StreamEnd, true, None, result)
            }
            0x3f => {
                let old_offset = self.current_utc_offset;
                let new_offset = self
                    .read_i64(KvIrTruncatedContext::IntegerPayload)
                    .map_err(KvIrReadError::Reader)?;
                let utc_offset_changes =
                    self.stats
                        .utc_offset_changes
                        .checked_add(1)
                        .ok_or_else(|| {
                            KvIrReadError::Reader(self.error(KvIrErrorKind::SizeOverflow))
                        })?;
                self.current_utc_offset = new_offset;
                self.stats.utc_offset_changes = utc_offset_changes;
                let item = KvIrUtcOffsetChange {
                    old_offset_millis: old_offset,
                    new_offset_millis: new_offset,
                    raw_unit: &self.unit,
                    stream_index: self.current_stream_index,
                    unit_index,
                    input_offset: unit_offset,
                };
                let result = sink
                    .write_item(KvIrItem::UtcOffsetChange(item))
                    .map_err(|source| KvIrReadError::Sink {
                        stream_index: self.current_stream_index,
                        unit_index: Some(unit_index),
                        source,
                    });
                (KvIrItemKind::UtcOffsetChange, false, None, result)
            }
            0x71..=0x7f => (
                KvIrItemKind::SchemaNode,
                false,
                None,
                self.read_schema_node(tag, unit_index, unit_offset, sink),
            ),
            0x5e | 0x65..=0x67 => {
                let event_index = self.next_event_index;
                let event_actual = event_index.checked_add(1).ok_or_else(|| {
                    KvIrReadError::Reader(self.error(KvIrErrorKind::SizeOverflow))
                })?;
                (
                    KvIrItemKind::LogEvent,
                    false,
                    Some(event_actual),
                    self.read_log_event(tag, unit_index, event_index, unit_offset, sink),
                )
            }
            _ => {
                return Err(KvIrReadError::Reader(self.error(KvIrErrorKind::Invalid(
                    KvIrInvalidData::InvalidUnitTag(tag),
                ))));
            }
        };

        if matches!(&result, Ok(()) | Err(KvIrReadError::Sink { .. })) {
            self.stats.units = total_units;
            self.next_unit_index = unit_actual;
            if let Some(event_index) = next_event_index {
                self.next_event_index = event_index;
            }
            if ended {
                self.in_stream = false;
            }
        }
        result?;
        Ok(kind)
    }

    fn read_preamble<S: KvIrSink + ?Sized>(
        &mut self,
        sink: &mut S,
    ) -> Result<(), KvIrReadError<S::Error>> {
        let stream_offset = self.input_offset;
        let mut magic = [0_u8; 4];
        for byte in &mut magic {
            *byte = self
                .read_preamble_byte(KvIrTruncatedContext::MagicNumber)
                .map_err(KvIrReadError::Reader)?;
        }
        let Some(encoding) = KvIrEncoding::from_magic_number(&magic) else {
            return Err(KvIrReadError::Reader(self.error(KvIrErrorKind::Invalid(
                KvIrInvalidData::InvalidMagicNumber(magic),
            ))));
        };

        let metadata_encoding = self
            .read_preamble_byte(KvIrTruncatedContext::MetadataHeader)
            .map_err(KvIrReadError::Reader)?;
        if metadata_encoding != 0x01 {
            return Err(KvIrReadError::Reader(self.error(KvIrErrorKind::Invalid(
                KvIrInvalidData::UnsupportedMetadataEncoding(metadata_encoding),
            ))));
        }
        let length_tag = self
            .read_preamble_byte(KvIrTruncatedContext::MetadataHeader)
            .map_err(KvIrReadError::Reader)?;
        let metadata_len = match length_tag {
            0x11 => usize::from(
                self.read_preamble_byte(KvIrTruncatedContext::MetadataHeader)
                    .map_err(KvIrReadError::Reader)?,
            ),
            0x12 => {
                let high = self
                    .read_preamble_byte(KvIrTruncatedContext::MetadataHeader)
                    .map_err(KvIrReadError::Reader)?;
                let low = self
                    .read_preamble_byte(KvIrTruncatedContext::MetadataHeader)
                    .map_err(KvIrReadError::Reader)?;
                usize::from(u16::from_be_bytes([high, low]))
            }
            tag => {
                return Err(KvIrReadError::Reader(self.error(KvIrErrorKind::Invalid(
                    KvIrInvalidData::UnsupportedMetadataLengthTag(tag),
                ))));
            }
        };
        let metadata_len_u64 = u64::try_from(metadata_len)
            .map_err(|_| KvIrReadError::Reader(self.error(KvIrErrorKind::SizeOverflow)))?;
        if metadata_len_u64 > self.options.limits.metadata_bytes {
            return Err(KvIrReadError::Reader(self.error(KvIrErrorKind::Limit(
                KvIrLimitViolation::new(
                    KvIrLimitResource::MetadataBytes,
                    metadata_len_u64,
                    self.options.limits.metadata_bytes,
                ),
            ))));
        }
        self.metadata_start = self.preamble.len();
        self.read_preamble_bytes(metadata_len, KvIrTruncatedContext::MetadataPayload)
            .map_err(KvIrReadError::Reader)?;
        self.parse_metadata().map_err(KvIrReadError::Reader)?;

        let streams = self
            .stats
            .streams
            .checked_add(1)
            .ok_or_else(|| KvIrReadError::Reader(self.error(KvIrErrorKind::SizeOverflow)))?;
        self.stats.streams = streams;
        let header = KvIrStreamHeader {
            encoding,
            protocol_version: &self.protocol_version,
            metadata_json: &self.preamble[self.metadata_start..],
            metadata_decoded: &self.metadata_decoded,
            metadata_events: &self.metadata_events,
            raw_preamble: &self.preamble,
            stream_index: self.current_stream_index,
            input_offset: stream_offset,
        };
        sink.write_item(KvIrItem::StreamStart(header))
            .map_err(|source| KvIrReadError::Sink {
                stream_index: self.current_stream_index,
                unit_index: None,
                source,
            })?;
        Ok(())
    }

    // This is an iterative state machine over flat JSON events, not recursive metadata parsing.
    #[allow(clippy::too_many_lines)]
    fn parse_metadata(&mut self) -> Result<(), KvIrError> {
        self.metadata_decoded.clear();
        self.metadata_events.clear();
        self.metadata_stack.clear();
        let metadata = &self.preamble[self.metadata_start..];
        let limits = NdjsonLimits::new(
            self.options.limits.metadata_bytes,
            self.options.limits.nesting_depth,
            self.options.limits.metadata_values,
            self.options.limits.scalar_bytes,
        );
        if let Err(failure) = parse_document(
            metadata,
            limits,
            &mut self.metadata_decoded,
            &mut self.metadata_events,
            &mut self.metadata_stack,
        ) {
            return Err(self.map_metadata_failure(&failure));
        }

        let mut depth = 0_u64;
        let mut root_seen = false;
        let mut pending = None;
        let mut version_state = MetadataFieldState::Missing;
        let mut user_metadata_state = MetadataFieldState::Missing;
        let mut selected_version: Option<&str> = None;

        for stored in &self.metadata_events {
            let event = stored.resolve(metadata, &self.metadata_decoded);
            match event {
                JsonEvent::ObjectStart => {
                    if root_seen {
                        if depth == 1 {
                            match pending.take() {
                                Some(MetadataPendingKey::Version) => {
                                    version_state = MetadataFieldState::Invalid;
                                }
                                Some(MetadataPendingKey::UserMetadata) => {
                                    user_metadata_state = MetadataFieldState::Valid;
                                }
                                _ => {}
                            }
                        }
                        depth = depth
                            .checked_add(1)
                            .ok_or_else(|| self.error(KvIrErrorKind::SizeOverflow))?;
                    } else {
                        root_seen = true;
                        depth = 1;
                    }
                }
                JsonEvent::ArrayStart(_) => {
                    if !root_seen {
                        return Err(self.error(KvIrErrorKind::Invalid(
                            KvIrInvalidData::MetadataMustBeObject,
                        )));
                    }
                    if depth == 1 {
                        match pending.take() {
                            Some(MetadataPendingKey::Version) => {
                                version_state = MetadataFieldState::Invalid;
                            }
                            Some(MetadataPendingKey::UserMetadata) => {
                                user_metadata_state = MetadataFieldState::Invalid;
                            }
                            _ => {}
                        }
                    }
                    depth = depth
                        .checked_add(1)
                        .ok_or_else(|| self.error(KvIrErrorKind::SizeOverflow))?;
                }
                JsonEvent::ObjectEnd | JsonEvent::ArrayEnd => {
                    depth = depth
                        .checked_sub(1)
                        .ok_or_else(|| self.error(KvIrErrorKind::SizeOverflow))?;
                }
                JsonEvent::ObjectKey(key) if depth == 1 => {
                    pending = Some(match key.decoded() {
                        "VERSION" => MetadataPendingKey::Version,
                        "USER_DEFINED_METADATA" => MetadataPendingKey::UserMetadata,
                        _ => MetadataPendingKey::Ignore,
                    });
                }
                JsonEvent::String(value) if depth == 1 => match pending.take() {
                    Some(MetadataPendingKey::Version) => {
                        version_state = MetadataFieldState::Valid;
                        selected_version = Some(value.decoded());
                    }
                    Some(MetadataPendingKey::UserMetadata) => {
                        user_metadata_state = MetadataFieldState::Invalid;
                    }
                    _ => {}
                },
                JsonEvent::Number(_) | JsonEvent::Boolean(_) | JsonEvent::Null if depth == 1 => {
                    match pending.take() {
                        Some(MetadataPendingKey::Version) => {
                            version_state = MetadataFieldState::Invalid;
                        }
                        Some(MetadataPendingKey::UserMetadata) => {
                            user_metadata_state = MetadataFieldState::Invalid;
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }

        if !root_seen {
            return Err(self.error(KvIrErrorKind::Invalid(
                KvIrInvalidData::MetadataMustBeObject,
            )));
        }
        match version_state {
            MetadataFieldState::Missing => {
                return Err(self.error(KvIrErrorKind::Invalid(
                    KvIrInvalidData::MissingProtocolVersion,
                )));
            }
            MetadataFieldState::Invalid => {
                return Err(self.error(KvIrErrorKind::Invalid(
                    KvIrInvalidData::ProtocolVersionMustBeString,
                )));
            }
            MetadataFieldState::Valid => {}
        }
        if matches!(user_metadata_state, MetadataFieldState::Invalid) {
            return Err(self.error(KvIrErrorKind::Invalid(
                KvIrInvalidData::UserDefinedMetadataMustBeObject,
            )));
        }

        let version = selected_version.ok_or_else(|| {
            self.error(KvIrErrorKind::Invalid(
                KvIrInvalidData::MissingProtocolVersion,
            ))
        })?;
        self.protocol_version.clear();
        self.protocol_version
            .try_reserve(version.len())
            .map_err(|_| {
                self.error(KvIrErrorKind::AllocationFailed {
                    resource: KvIrResource::ProtocolVersion,
                    requested_additional: version.len(),
                })
            })?;
        self.protocol_version.push_str(version);
        if self.protocol_version == CURRENT_VERSION {
            return Ok(());
        }

        // Parsing is terminating on either error, so move the bounded version buffer into the
        // diagnostic instead of performing an unchecked allocation solely to report it.
        let version = std::mem::take(&mut self.protocol_version);
        let invalid = match version.as_str() {
            "v0.0.0" | "0.0.1" | "0.0.2" => KvIrInvalidData::LegacyUnstructuredVersion(version),
            _ => KvIrInvalidData::UnsupportedProtocolVersion(version),
        };
        Err(self.error(KvIrErrorKind::Invalid(invalid)))
    }

    const fn map_metadata_failure(&self, failure: &ParseFailure) -> KvIrError {
        match failure {
            ParseFailure::Invalid(NdjsonInvalidRecordKind::Syntax(_)) => {
                self.error(KvIrErrorKind::Invalid(KvIrInvalidData::InvalidMetadataJson))
            }
            ParseFailure::Invalid(NdjsonInvalidRecordKind::Limit(source)) => {
                let resource = match source.resource() {
                    NdjsonLimitResource::RecordBytes => KvIrLimitResource::MetadataBytes,
                    NdjsonLimitResource::NestingDepth => KvIrLimitResource::NestingDepth,
                    NdjsonLimitResource::Values => KvIrLimitResource::MetadataValues,
                    NdjsonLimitResource::ScalarTokenBytes => KvIrLimitResource::ScalarBytes,
                };
                self.error(KvIrErrorKind::Limit(KvIrLimitViolation::new(
                    resource,
                    source.actual(),
                    source.limit(),
                )))
            }
            ParseFailure::AllocationFailed {
                resource,
                requested_additional,
            } => {
                let resource = match resource {
                    NdjsonResource::Events => KvIrResource::MetadataEvents,
                    NdjsonResource::DecodedStrings => KvIrResource::MetadataDecodedStrings,
                    NdjsonResource::ParserStack => KvIrResource::MetadataParserStack,
                    NdjsonResource::RecordBuffer => KvIrResource::Preamble,
                };
                self.error(KvIrErrorKind::AllocationFailed {
                    resource,
                    requested_additional: *requested_additional,
                })
            }
            ParseFailure::SizeOverflow => self.error(KvIrErrorKind::SizeOverflow),
        }
    }

    fn reset_schemas(&mut self) -> Result<(), KvIrError> {
        self.auto_schema.clear();
        self.user_schema.clear();
        self.schema_keys.clear();
        self.auto_schema_index.clear();
        self.user_schema_index.clear();
        self.auto_schema.try_reserve(1).map_err(|_| {
            self.error(KvIrErrorKind::AllocationFailed {
                resource: KvIrResource::SchemaNodes,
                requested_additional: 1,
            })
        })?;
        self.user_schema.try_reserve(1).map_err(|_| {
            self.error(KvIrErrorKind::AllocationFailed {
                resource: KvIrResource::SchemaNodes,
                requested_additional: 1,
            })
        })?;
        self.auto_schema.push(SchemaNodeOwned::root());
        self.user_schema.push(SchemaNodeOwned::root());
        Ok(())
    }

    // Validation and insertion stay together so no callback can observe a partially inserted node.
    #[allow(clippy::too_many_lines)]
    fn read_schema_node<S: KvIrSink + ?Sized>(
        &mut self,
        tag: u8,
        unit_index: u64,
        unit_offset: u64,
        sink: &mut S,
    ) -> Result<(), KvIrReadError<S::Error>> {
        let node_type = Self::node_type(tag)
            .map_err(|kind| KvIrReadError::Reader(self.error(KvIrErrorKind::Invalid(kind))))?;
        let parent_tag = self
            .read_unit_byte(KvIrTruncatedContext::UnitTag)
            .map_err(KvIrReadError::Reader)?;
        let (namespace, parent_id) = self
            .read_node_id(parent_tag, true)
            .map_err(KvIrReadError::Reader)?;
        let key_tag = self
            .read_unit_byte(KvIrTruncatedContext::StringLength)
            .map_err(KvIrReadError::Reader)?;
        let key_span = self
            .read_string(key_tag, StringPacket::Ordinary)
            .map_err(KvIrReadError::Reader)?;

        let tree = self.schema(namespace);
        let parent_index = usize::try_from(parent_id).map_err(|_| {
            KvIrReadError::Reader(self.error(KvIrErrorKind::Invalid(
                KvIrInvalidData::MissingParentNode {
                    namespace,
                    node_id: parent_id,
                },
            )))
        })?;
        let Some(parent) = tree.get(parent_index) else {
            return Err(KvIrReadError::Reader(self.error(KvIrErrorKind::Invalid(
                KvIrInvalidData::MissingParentNode {
                    namespace,
                    node_id: parent_id,
                },
            ))));
        };
        if parent.node_type != KvIrNodeType::Object {
            return Err(KvIrReadError::Reader(self.error(KvIrErrorKind::Invalid(
                KvIrInvalidData::ParentNodeIsNotObject {
                    namespace,
                    node_id: parent_id,
                },
            ))));
        }
        let depth = parent
            .depth
            .checked_add(1)
            .ok_or_else(|| KvIrReadError::Reader(self.error(KvIrErrorKind::SizeOverflow)))?;
        let depth_u64 = u64::from(depth);
        if depth_u64 > self.options.limits.nesting_depth {
            return Err(KvIrReadError::Reader(self.error(KvIrErrorKind::Limit(
                KvIrLimitViolation::new(
                    KvIrLimitResource::NestingDepth,
                    depth_u64,
                    self.options.limits.nesting_depth,
                ),
            ))));
        }
        let key = key_span.resolve(&self.unit);
        let node_type_mask = Self::node_type_mask(node_type);
        let (mut key_hash, existing_group_id) = self.schema_index(namespace).locate_group(
            tree,
            &self.schema_keys,
            &self.schema_hash_builder,
            parent_id,
            key,
        );
        let was_indexed = !self.schema_index(namespace).slots.is_empty();
        if let Some(group_id) = existing_group_id {
            let group_index = usize::try_from(group_id)
                .map_err(|_| KvIrReadError::Reader(self.error(KvIrErrorKind::SizeOverflow)))?;
            let group = self
                .schema_index(namespace)
                .sibling_groups
                .get(group_index)
                .ok_or_else(|| KvIrReadError::Reader(self.error(KvIrErrorKind::SizeOverflow)))?;
            if group.node_type_mask & node_type_mask != 0 {
                return Err(KvIrReadError::Reader(self.error(KvIrErrorKind::Invalid(
                    KvIrInvalidData::DuplicateSchemaNode,
                ))));
            }
        }
        let node_id = u32::try_from(tree.len()).map_err(|_| {
            KvIrReadError::Reader(self.error(KvIrErrorKind::Limit(KvIrLimitViolation::new(
                KvIrLimitResource::SchemaNodesPerNamespace,
                u64::from(u32::MAX) + 1,
                self.options.limits.schema_nodes_per_namespace,
            ))))
        })?;
        let node_count = u64::from(node_id);
        if node_count > self.options.limits.schema_nodes_per_namespace {
            return Err(KvIrReadError::Reader(self.error(KvIrErrorKind::Limit(
                KvIrLimitViolation::new(
                    KvIrLimitResource::SchemaNodesPerNamespace,
                    node_count,
                    self.options.limits.schema_nodes_per_namespace,
                ),
            ))));
        }

        let key_len = key.len();
        if self.schema_keys.try_reserve(key_len).is_err() {
            return Err(KvIrReadError::Reader(self.error(
                KvIrErrorKind::AllocationFailed {
                    resource: KvIrResource::SchemaKey,
                    requested_additional: key_len,
                },
            )));
        }
        self.schema_mut(namespace).try_reserve(1).map_err(|_| {
            KvIrReadError::Reader(self.error(KvIrErrorKind::AllocationFailed {
                resource: KvIrResource::SchemaNodes,
                requested_additional: 1,
            }))
        })?;
        if existing_group_id.is_none() {
            let prepare_result = match namespace {
                KvIrNamespace::AutoGenerated => self.auto_schema_index.try_prepare_group(
                    &self.auto_schema,
                    &self.schema_keys,
                    &self.schema_hash_builder,
                ),
                KvIrNamespace::UserGenerated => self.user_schema_index.try_prepare_group(
                    &self.user_schema,
                    &self.schema_keys,
                    &self.schema_hash_builder,
                ),
            };
            if let Err(source) = prepare_result {
                let kind = match source {
                    SchemaIndexGrowthError::SizeOverflow => KvIrErrorKind::SizeOverflow,
                    SchemaIndexGrowthError::AllocationFailed {
                        requested_additional,
                    } => KvIrErrorKind::AllocationFailed {
                        resource: KvIrResource::SchemaNodes,
                        requested_additional,
                    },
                };
                return Err(KvIrReadError::Reader(self.error(kind)));
            }
            if !was_indexed && !self.schema_index(namespace).slots.is_empty() {
                key_hash = SchemaIndex::hash_key(
                    &self.schema_hash_builder,
                    parent_id,
                    key_span.resolve(&self.unit),
                );
            }
        }
        let sibling_group_id = if let Some(group_id) = existing_group_id {
            group_id
        } else {
            u32::try_from(self.schema_index(namespace).sibling_groups.len())
                .map_err(|_| KvIrReadError::Reader(self.error(KvIrErrorKind::SizeOverflow)))?
        };
        let key_start = self.schema_keys.len();
        let key_length = u32::try_from(key_len)
            .map_err(|_| KvIrReadError::Reader(self.error(KvIrErrorKind::SizeOverflow)))?;
        self.schema_keys
            .extend_from_slice(key_span.resolve(&self.unit));
        self.schema_mut(namespace).push(SchemaNodeOwned {
            parent_id,
            depth,
            key_start,
            key_length,
            node_type,
            sibling_group_id,
            last_event_epoch: 0,
        });
        if existing_group_id.is_some() {
            let group_index = usize::try_from(sibling_group_id)
                .map_err(|_| KvIrReadError::Reader(self.error(KvIrErrorKind::SizeOverflow)))?;
            self.schema_index_mut(namespace).sibling_groups[group_index].node_type_mask |=
                node_type_mask;
        } else {
            self.schema_index_mut(namespace).insert_group(
                key_hash,
                node_id,
                node_type_mask,
                sibling_group_id,
            );
        }

        let schema_nodes = self
            .stats
            .schema_nodes
            .checked_add(1)
            .ok_or_else(|| KvIrReadError::Reader(self.error(KvIrErrorKind::SizeOverflow)))?;
        self.stats.schema_nodes = schema_nodes;
        let item = KvIrSchemaNode {
            namespace,
            node_id,
            parent_id,
            depth: depth_u64,
            key: key_span.resolve(&self.unit),
            node_type,
            raw_unit: &self.unit,
            stream_index: self.current_stream_index,
            unit_index,
            input_offset: unit_offset,
        };
        sink.write_item(KvIrItem::SchemaNode(item))
            .map_err(|source| KvIrReadError::Sink {
                stream_index: self.current_stream_index,
                unit_index: Some(unit_index),
                source,
            })?;
        Ok(())
    }

    fn read_log_event<S: KvIrSink + ?Sized>(
        &mut self,
        first_tag: u8,
        unit_index: u64,
        event_index: u64,
        unit_offset: u64,
        sink: &mut S,
    ) -> Result<(), KvIrReadError<S::Error>> {
        self.event_pairs.clear();
        self.user_node_ids.clear();
        self.encoded_variables.clear();
        self.dictionary_spans.clear();

        if first_tag != 0x5e {
            let mut tag = first_tag;
            let mut saw_user = false;
            while matches!(tag, 0x65..=0x67) {
                self.check_event_pair_limit()
                    .map_err(KvIrReadError::Reader)?;
                let (namespace, node_id) = self
                    .read_node_id(tag, false)
                    .map_err(KvIrReadError::Reader)?;
                tag = self
                    .read_unit_byte(KvIrTruncatedContext::UnitTag)
                    .map_err(KvIrReadError::Reader)?;
                match namespace {
                    KvIrNamespace::AutoGenerated => {
                        if saw_user {
                            return Err(KvIrReadError::Reader(self.error(KvIrErrorKind::Invalid(
                                KvIrInvalidData::InvalidKeyGroupOrdering,
                            ))));
                        }
                        let pair = self
                            .read_value(namespace, node_id, tag)
                            .map_err(KvIrReadError::Reader)?;
                        self.push_pair(pair).map_err(KvIrReadError::Reader)?;
                        tag = self
                            .read_unit_byte(KvIrTruncatedContext::UnitTag)
                            .map_err(KvIrReadError::Reader)?;
                    }
                    KvIrNamespace::UserGenerated => {
                        saw_user = true;
                        self.user_node_ids.try_reserve(1).map_err(|_| {
                            KvIrReadError::Reader(self.error(KvIrErrorKind::AllocationFailed {
                                resource: KvIrResource::EventUserNodeIds,
                                requested_additional: 1,
                            }))
                        })?;
                        self.user_node_ids.push(node_id);
                    }
                }
            }

            if self.user_node_ids.is_empty() {
                if tag != 0x5e {
                    return Err(KvIrReadError::Reader(self.error(KvIrErrorKind::Invalid(
                        KvIrInvalidData::UnknownValueTag(tag),
                    ))));
                }
            } else {
                let user_id_count = self.user_node_ids.len();
                for index in 0..user_id_count {
                    let node_id = self.user_node_ids[index];
                    let pair = self
                        .read_value(KvIrNamespace::UserGenerated, node_id, tag)
                        .map_err(KvIrReadError::Reader)?;
                    self.push_pair(pair).map_err(KvIrReadError::Reader)?;
                    if index + 1 != user_id_count {
                        tag = self
                            .read_unit_byte(KvIrTruncatedContext::UnitTag)
                            .map_err(KvIrReadError::Reader)?;
                    }
                }
            }
        }

        self.validate_event().map_err(KvIrReadError::Reader)?;
        let log_events = self
            .stats
            .log_events
            .checked_add(1)
            .ok_or_else(|| KvIrReadError::Reader(self.error(KvIrErrorKind::SizeOverflow)))?;
        self.stats.log_events = log_events;
        let event = KvIrLogEvent {
            raw_unit: &self.unit,
            pairs: &self.event_pairs,
            variables: &self.encoded_variables,
            dictionaries: &self.dictionary_spans,
            auto_schema: &self.auto_schema,
            user_schema: &self.user_schema,
            schema_keys: &self.schema_keys,
            utc_offset_millis: self.current_utc_offset,
            stream_index: self.current_stream_index,
            unit_index,
            event_index,
            input_offset: unit_offset,
        };
        sink.write_item(KvIrItem::LogEvent(event))
            .map_err(|source| KvIrReadError::Sink {
                stream_index: self.current_stream_index,
                unit_index: Some(unit_index),
                source,
            })?;
        Ok(())
    }

    fn read_value(
        &mut self,
        namespace: KvIrNamespace,
        node_id: u32,
        tag: u8,
    ) -> Result<StoredPair, KvIrError> {
        let start = self
            .unit
            .len()
            .checked_sub(1)
            .ok_or_else(|| self.error(KvIrErrorKind::SizeOverflow))?;
        let kind = match tag {
            0x51 => StoredValueKind::Integer(KvIrInteger {
                value: i64::from(self.read_i8(KvIrTruncatedContext::IntegerPayload)?),
                width: KvIrIntegerWidth::One,
            }),
            0x52 => StoredValueKind::Integer(KvIrInteger {
                value: i64::from(self.read_i16(KvIrTruncatedContext::IntegerPayload)?),
                width: KvIrIntegerWidth::Two,
            }),
            0x53 => StoredValueKind::Integer(KvIrInteger {
                value: i64::from(self.read_i32(KvIrTruncatedContext::IntegerPayload)?),
                width: KvIrIntegerWidth::Four,
            }),
            0x54 => StoredValueKind::Integer(KvIrInteger {
                value: self.read_i64(KvIrTruncatedContext::IntegerPayload)?,
                width: KvIrIntegerWidth::Eight,
            }),
            0x56 => StoredValueKind::Float {
                bits: self.read_u64(KvIrTruncatedContext::IntegerPayload)?,
            },
            0x57 => StoredValueKind::Boolean(true),
            0x58 => StoredValueKind::Boolean(false),
            0x41..=0x43 => StoredValueKind::String(self.read_string(tag, StringPacket::Ordinary)?),
            0x59 => self.read_encoded_text(KvIrEncoding::FourByte)?,
            0x5a => self.read_encoded_text(KvIrEncoding::EightByte)?,
            0x5e => StoredValueKind::EmptyObject,
            0x5f => StoredValueKind::Null,
            _ => {
                return Err(
                    self.error(KvIrErrorKind::Invalid(KvIrInvalidData::UnknownValueTag(
                        tag,
                    ))),
                );
            }
        };
        Ok(StoredPair {
            namespace,
            node_id,
            raw: ByteSpan::new(start, self.unit.len()),
            kind,
        })
    }

    fn read_encoded_text(&mut self, encoding: KvIrEncoding) -> Result<StoredValueKind, KvIrError> {
        let variable_start = self.encoded_variables.len();
        let dictionary_start = self.dictionary_spans.len();
        let mut components = 0_u64;
        loop {
            let tag = self.read_unit_byte(KvIrTruncatedContext::EncodedText)?;
            match (encoding, tag) {
                (KvIrEncoding::FourByte, 0x18) => {
                    self.check_component_limit(&mut components)?;
                    let value = self.read_i32(KvIrTruncatedContext::IntegerPayload)?;
                    self.encoded_variables.try_reserve(1).map_err(|_| {
                        self.error(KvIrErrorKind::AllocationFailed {
                            resource: KvIrResource::EncodedVariables,
                            requested_additional: 1,
                        })
                    })?;
                    self.encoded_variables.push(StoredEncodedVariable {
                        value: KvIrEncodedVariable::FourByte(value),
                    });
                }
                (KvIrEncoding::EightByte, 0x19) => {
                    self.check_component_limit(&mut components)?;
                    let value = self.read_i64(KvIrTruncatedContext::IntegerPayload)?;
                    self.encoded_variables.try_reserve(1).map_err(|_| {
                        self.error(KvIrErrorKind::AllocationFailed {
                            resource: KvIrResource::EncodedVariables,
                            requested_additional: 1,
                        })
                    })?;
                    self.encoded_variables.push(StoredEncodedVariable {
                        value: KvIrEncodedVariable::EightByte(value),
                    });
                }
                (_, 0x11..=0x13) => {
                    self.check_component_limit(&mut components)?;
                    let span = self.read_string(tag, StringPacket::Dictionary)?;
                    self.dictionary_spans.try_reserve(1).map_err(|_| {
                        self.error(KvIrErrorKind::AllocationFailed {
                            resource: KvIrResource::DictionaryVariables,
                            requested_additional: 1,
                        })
                    })?;
                    self.dictionary_spans.push(span);
                }
                (_, 0x21..=0x23) => {
                    let logtype = self.read_string(tag, StringPacket::Logtype)?;
                    return Ok(StoredValueKind::EncodedText {
                        encoding,
                        variables: variable_start..self.encoded_variables.len(),
                        dictionaries: dictionary_start..self.dictionary_spans.len(),
                        logtype,
                    });
                }
                _ => {
                    return Err(self.error(KvIrErrorKind::Invalid(
                        KvIrInvalidData::InvalidEncodedTextTag(tag),
                    )));
                }
            }
        }
    }

    fn check_component_limit(&self, components: &mut u64) -> Result<(), KvIrError> {
        *components = components
            .checked_add(1)
            .ok_or_else(|| self.error(KvIrErrorKind::SizeOverflow))?;
        if *components > self.options.limits.encoded_components_per_value {
            return Err(self.error(KvIrErrorKind::Limit(KvIrLimitViolation::new(
                KvIrLimitResource::EncodedComponentsPerValue,
                *components,
                self.options.limits.encoded_components_per_value,
            ))));
        }
        Ok(())
    }

    fn check_event_pair_limit(&self) -> Result<(), KvIrError> {
        let existing = self
            .event_pairs
            .len()
            .checked_add(self.user_node_ids.len())
            .and_then(|value| value.checked_add(1))
            .ok_or_else(|| self.error(KvIrErrorKind::SizeOverflow))?;
        let actual =
            u64::try_from(existing).map_err(|_| self.error(KvIrErrorKind::SizeOverflow))?;
        if actual > self.options.limits.values_per_event {
            return Err(self.error(KvIrErrorKind::Limit(KvIrLimitViolation::new(
                KvIrLimitResource::ValuesPerEvent,
                actual,
                self.options.limits.values_per_event,
            ))));
        }
        Ok(())
    }

    fn push_pair(&mut self, pair: StoredPair) -> Result<(), KvIrError> {
        self.event_pairs.try_reserve(1).map_err(|_| {
            self.error(KvIrErrorKind::AllocationFailed {
                resource: KvIrResource::EventPairs,
                requested_additional: 1,
            })
        })?;
        self.event_pairs.push(pair);
        Ok(())
    }

    fn validate_event(&mut self) -> Result<(), KvIrError> {
        let event_epoch = self.next_validation_epoch();

        for pair_index in 0..self.event_pairs.len() {
            let pair = &self.event_pairs[pair_index];
            let namespace = pair.namespace;
            let node_id = pair.node_id;
            let tree = self.schema(namespace);
            let node_index = usize::try_from(pair.node_id).map_err(|_| {
                self.error(KvIrErrorKind::Invalid(KvIrInvalidData::MissingSchemaNode {
                    namespace,
                    node_id,
                }))
            })?;
            let Some(node) = tree.get(node_index) else {
                return Err(self.error(KvIrErrorKind::Invalid(
                    KvIrInvalidData::MissingSchemaNode { namespace, node_id },
                )));
            };
            if node_id == 0 {
                return Err(
                    self.error(KvIrErrorKind::Invalid(KvIrInvalidData::RootNodeValue {
                        namespace,
                    })),
                );
            }
            if !Self::value_matches_node(&pair.kind, node.node_type) {
                return Err(self.error(KvIrErrorKind::Invalid(KvIrInvalidData::ValueTypeMismatch)));
            }
            if node.last_event_epoch == event_epoch {
                return Err(self.error(KvIrErrorKind::Invalid(
                    KvIrInvalidData::DuplicateEventNode { namespace, node_id },
                )));
            }
            self.schema_mut(namespace)[node_index].last_event_epoch = event_epoch;
        }

        for pair_index in 0..self.event_pairs.len() {
            let namespace = self.event_pairs[pair_index].namespace;
            let node_id = self.event_pairs[pair_index].node_id;
            let tree = self.schema(namespace);
            let node = &tree
                [usize::try_from(node_id).map_err(|_| self.error(KvIrErrorKind::SizeOverflow))?];
            let parent_id = node.parent_id;
            let sibling_group_id = node.sibling_group_id;
            let group_index = usize::try_from(sibling_group_id)
                .map_err(|_| self.error(KvIrErrorKind::SizeOverflow))?;
            let sibling_seen = self
                .schema_index(namespace)
                .sibling_groups
                .get(group_index)
                .ok_or_else(|| self.error(KvIrErrorKind::SizeOverflow))?
                .last_event_epoch
                == event_epoch;
            if sibling_seen {
                return Err(
                    self.error(KvIrErrorKind::Invalid(KvIrInvalidData::DuplicateSiblingKey))
                );
            }
            self.schema_index_mut(namespace).sibling_groups[group_index].last_event_epoch =
                event_epoch;

            let tree = self.schema(namespace);
            let mut ancestor_id = parent_id;
            while ancestor_id != 0 {
                let ancestor_index = usize::try_from(ancestor_id)
                    .map_err(|_| self.error(KvIrErrorKind::SizeOverflow))?;
                let ancestor = tree
                    .get(ancestor_index)
                    .ok_or_else(|| self.error(KvIrErrorKind::SizeOverflow))?;
                if ancestor.last_event_epoch == event_epoch {
                    return Err(self.error(KvIrErrorKind::Invalid(
                        KvIrInvalidData::ObjectValueHasDescendant,
                    )));
                }
                ancestor_id = ancestor.parent_id;
            }
        }
        Ok(())
    }

    fn next_validation_epoch(&mut self) -> u32 {
        self.validation_epoch = self.validation_epoch.wrapping_add(1);
        if self.validation_epoch == 0 {
            for node in self
                .auto_schema
                .iter_mut()
                .chain(self.user_schema.iter_mut())
            {
                node.last_event_epoch = 0;
            }
            for group in self
                .auto_schema_index
                .sibling_groups
                .iter_mut()
                .chain(self.user_schema_index.sibling_groups.iter_mut())
            {
                group.last_event_epoch = 0;
            }
            self.validation_epoch = 1;
        }
        self.validation_epoch
    }

    const fn value_matches_node(kind: &StoredValueKind, node_type: KvIrNodeType) -> bool {
        match node_type {
            KvIrNodeType::Integer => matches!(kind, StoredValueKind::Integer(_)),
            KvIrNodeType::Float => matches!(kind, StoredValueKind::Float { .. }),
            KvIrNodeType::Boolean => matches!(kind, StoredValueKind::Boolean(_)),
            KvIrNodeType::String => matches!(
                kind,
                StoredValueKind::String(_) | StoredValueKind::EncodedText { .. }
            ),
            KvIrNodeType::UnstructuredArray => {
                matches!(kind, StoredValueKind::EncodedText { .. })
            }
            KvIrNodeType::Object => {
                matches!(kind, StoredValueKind::Null | StoredValueKind::EmptyObject)
            }
        }
    }

    const fn node_type(tag: u8) -> Result<KvIrNodeType, KvIrInvalidData> {
        match tag {
            0x71 => Ok(KvIrNodeType::Integer),
            0x72 => Ok(KvIrNodeType::Float),
            0x73 => Ok(KvIrNodeType::Boolean),
            0x74 => Ok(KvIrNodeType::String),
            0x75 => Ok(KvIrNodeType::UnstructuredArray),
            0x76 => Ok(KvIrNodeType::Object),
            value => Err(KvIrInvalidData::InvalidSchemaNodeType(value)),
        }
    }

    const fn node_type_mask(node_type: KvIrNodeType) -> u8 {
        match node_type {
            KvIrNodeType::Integer => 1 << 0,
            KvIrNodeType::Float => 1 << 1,
            KvIrNodeType::Boolean => 1 << 2,
            KvIrNodeType::String => 1 << 3,
            KvIrNodeType::UnstructuredArray => 1 << 4,
            KvIrNodeType::Object => 1 << 5,
        }
    }

    fn schema(&self, namespace: KvIrNamespace) -> &[SchemaNodeOwned] {
        match namespace {
            KvIrNamespace::AutoGenerated => &self.auto_schema,
            KvIrNamespace::UserGenerated => &self.user_schema,
        }
    }

    const fn schema_mut(&mut self, namespace: KvIrNamespace) -> &mut Vec<SchemaNodeOwned> {
        match namespace {
            KvIrNamespace::AutoGenerated => &mut self.auto_schema,
            KvIrNamespace::UserGenerated => &mut self.user_schema,
        }
    }

    const fn schema_index(&self, namespace: KvIrNamespace) -> &SchemaIndex {
        match namespace {
            KvIrNamespace::AutoGenerated => &self.auto_schema_index,
            KvIrNamespace::UserGenerated => &self.user_schema_index,
        }
    }

    const fn schema_index_mut(&mut self, namespace: KvIrNamespace) -> &mut SchemaIndex {
        match namespace {
            KvIrNamespace::AutoGenerated => &mut self.auto_schema_index,
            KvIrNamespace::UserGenerated => &mut self.user_schema_index,
        }
    }

    #[cfg(test)]
    pub(crate) const fn schema_index_probes(&self) -> u64 {
        self.auto_schema_index
            .probes()
            .saturating_add(self.user_schema_index.probes())
    }

    fn read_node_id(&mut self, tag: u8, parent: bool) -> Result<(KvIrNamespace, u32), KvIrError> {
        let signed =
            match (parent, tag) {
                (true, 0x60) | (false, 0x65) => {
                    i64::from(self.read_i8(KvIrTruncatedContext::SchemaNodeIdPayload)?)
                }
                (true, 0x61) | (false, 0x66) => {
                    i64::from(self.read_i16(KvIrTruncatedContext::SchemaNodeIdPayload)?)
                }
                (true, 0x62) | (false, 0x67) => {
                    i64::from(self.read_i32(KvIrTruncatedContext::SchemaNodeIdPayload)?)
                }
                (true, value) => {
                    return Err(self.error(KvIrErrorKind::Invalid(
                        KvIrInvalidData::InvalidParentIdTag(value),
                    )));
                }
                (false, value) => {
                    return Err(self.error(KvIrErrorKind::Invalid(
                        KvIrInvalidData::InvalidNodeIdTag(value),
                    )));
                }
            };
        let (namespace, decoded) = if signed < 0 {
            (KvIrNamespace::AutoGenerated, !signed)
        } else {
            (KvIrNamespace::UserGenerated, signed)
        };
        let node_id =
            u32::try_from(decoded).map_err(|_| self.error(KvIrErrorKind::SizeOverflow))?;
        Ok((namespace, node_id))
    }

    fn read_string(&mut self, tag: u8, packet: StringPacket) -> Result<ByteSpan, KvIrError> {
        let length = match (packet, tag) {
            (StringPacket::Ordinary, 0x41)
            | (StringPacket::Dictionary, 0x11)
            | (StringPacket::Logtype, 0x21) => {
                usize::from(self.read_u8(KvIrTruncatedContext::StringLength)?)
            }
            (StringPacket::Ordinary, 0x42)
            | (StringPacket::Dictionary, 0x12)
            | (StringPacket::Logtype, 0x22) => {
                usize::from(self.read_u16(KvIrTruncatedContext::StringLength)?)
            }
            (StringPacket::Ordinary, 0x43) => {
                usize::try_from(self.read_u32(KvIrTruncatedContext::StringLength)?)
                    .map_err(|_| self.error(KvIrErrorKind::SizeOverflow))?
            }
            (StringPacket::Dictionary, 0x13) | (StringPacket::Logtype, 0x23) => {
                let value = self.read_i32(KvIrTruncatedContext::StringLength)?;
                usize::try_from(value).map_err(|_| {
                    self.error(KvIrErrorKind::Invalid(KvIrInvalidData::InvalidStringLength))
                })?
            }
            (_, value) => {
                return Err(self.error(KvIrErrorKind::Invalid(match packet {
                    StringPacket::Ordinary => KvIrInvalidData::UnknownValueTag(value),
                    StringPacket::Dictionary | StringPacket::Logtype => {
                        KvIrInvalidData::InvalidEncodedTextTag(value)
                    }
                })));
            }
        };
        let length_u64 =
            u64::try_from(length).map_err(|_| self.error(KvIrErrorKind::SizeOverflow))?;
        if length_u64 > self.options.limits.scalar_bytes {
            return Err(self.error(KvIrErrorKind::Limit(KvIrLimitViolation::new(
                KvIrLimitResource::ScalarBytes,
                length_u64,
                self.options.limits.scalar_bytes,
            ))));
        }
        let start = self.unit.len();
        self.read_unit_bytes(length, KvIrTruncatedContext::StringPayload)?;
        Ok(ByteSpan::new(start, self.unit.len()))
    }

    fn read_i8(&mut self, context: KvIrTruncatedContext) -> Result<i8, KvIrError> {
        Ok(i8::from_be_bytes(self.read_array(context)?))
    }

    fn read_u8(&mut self, context: KvIrTruncatedContext) -> Result<u8, KvIrError> {
        Ok(u8::from_be_bytes(self.read_array(context)?))
    }

    fn read_i16(&mut self, context: KvIrTruncatedContext) -> Result<i16, KvIrError> {
        Ok(i16::from_be_bytes(self.read_array(context)?))
    }

    fn read_u16(&mut self, context: KvIrTruncatedContext) -> Result<u16, KvIrError> {
        Ok(u16::from_be_bytes(self.read_array(context)?))
    }

    fn read_i32(&mut self, context: KvIrTruncatedContext) -> Result<i32, KvIrError> {
        Ok(i32::from_be_bytes(self.read_array(context)?))
    }

    fn read_u32(&mut self, context: KvIrTruncatedContext) -> Result<u32, KvIrError> {
        Ok(u32::from_be_bytes(self.read_array(context)?))
    }

    fn read_i64(&mut self, context: KvIrTruncatedContext) -> Result<i64, KvIrError> {
        Ok(i64::from_be_bytes(self.read_array(context)?))
    }

    fn read_u64(&mut self, context: KvIrTruncatedContext) -> Result<u64, KvIrError> {
        Ok(u64::from_be_bytes(self.read_array(context)?))
    }

    #[inline]
    fn read_array<const N: usize>(
        &mut self,
        context: KvIrTruncatedContext,
    ) -> Result<[u8; N], KvIrError> {
        let start = self.unit.len();
        self.read_unit_bytes(N, context)?;
        let mut output = [0_u8; N];
        output.copy_from_slice(&self.unit[start..start + N]);
        Ok(output)
    }

    fn read_preamble_byte(&mut self, context: KvIrTruncatedContext) -> Result<u8, KvIrError> {
        self.check_preamble_growth(1)?;
        self.preamble.try_reserve(1).map_err(|_| {
            self.error(KvIrErrorKind::AllocationFailed {
                resource: KvIrResource::Preamble,
                requested_additional: 1,
            })
        })?;
        let value = self.read_stream_byte(context)?;
        self.preamble.push(value);
        Ok(value)
    }

    fn read_preamble_bytes(
        &mut self,
        count: usize,
        context: KvIrTruncatedContext,
    ) -> Result<(), KvIrError> {
        self.check_preamble_growth(count)?;
        self.check_stream_growth(count)?;
        self.preamble.try_reserve(count).map_err(|_| {
            self.error(KvIrErrorKind::AllocationFailed {
                resource: KvIrResource::Preamble,
                requested_additional: count,
            })
        })?;
        let mut remaining = count;
        while remaining != 0 {
            if !self.has_input()? {
                return Err(self.error(KvIrErrorKind::Truncated {
                    context,
                    missing_bytes: u64::try_from(remaining).unwrap_or(u64::MAX),
                }));
            }
            let available = self.input_end - self.input_start;
            let take = remaining.min(available);
            self.preamble
                .extend_from_slice(&self.input_buffer[self.input_start..self.input_start + take]);
            self.consume_buffered(take)?;
            remaining -= take;
        }
        Ok(())
    }

    #[inline]
    fn read_unit_byte(&mut self, context: KvIrTruncatedContext) -> Result<u8, KvIrError> {
        self.check_unit_growth(1)?;
        self.unit.try_reserve(1).map_err(|_| {
            self.error(KvIrErrorKind::AllocationFailed {
                resource: KvIrResource::Unit,
                requested_additional: 1,
            })
        })?;
        let value = self.read_stream_byte(context)?;
        self.unit.push(value);
        Ok(value)
    }

    #[inline]
    fn read_unit_bytes(
        &mut self,
        count: usize,
        context: KvIrTruncatedContext,
    ) -> Result<(), KvIrError> {
        self.check_unit_growth(count)?;
        self.check_stream_growth(count)?;
        self.unit.try_reserve(count).map_err(|_| {
            self.error(KvIrErrorKind::AllocationFailed {
                resource: KvIrResource::Unit,
                requested_additional: count,
            })
        })?;
        let mut remaining = count;
        while remaining != 0 {
            if !self.has_input()? {
                return Err(self.error(KvIrErrorKind::Truncated {
                    context,
                    missing_bytes: u64::try_from(remaining).unwrap_or(u64::MAX),
                }));
            }
            let available = self.input_end - self.input_start;
            let take = remaining.min(available);
            self.unit
                .extend_from_slice(&self.input_buffer[self.input_start..self.input_start + take]);
            self.consume_buffered(take)?;
            remaining -= take;
        }
        Ok(())
    }

    #[inline]
    fn read_stream_byte(&mut self, context: KvIrTruncatedContext) -> Result<u8, KvIrError> {
        self.check_stream_growth(1)?;
        if !self.has_input()? {
            return Err(self.error(KvIrErrorKind::Truncated {
                context,
                missing_bytes: 1,
            }));
        }
        let value = self.input_buffer[self.input_start];
        self.consume_buffered(1)?;
        Ok(value)
    }

    fn check_preamble_growth(&self, additional: usize) -> Result<(), KvIrError> {
        self.preamble
            .len()
            .checked_add(additional)
            .ok_or_else(|| self.error(KvIrErrorKind::SizeOverflow))?;
        Ok(())
    }

    #[inline]
    fn check_unit_growth(&self, additional: usize) -> Result<(), KvIrError> {
        let next = self
            .unit
            .len()
            .checked_add(additional)
            .ok_or_else(|| self.error(KvIrErrorKind::SizeOverflow))?;
        let next_u64 = u64::try_from(next).map_err(|_| self.error(KvIrErrorKind::SizeOverflow))?;
        if next_u64 > self.options.limits.unit_bytes {
            return Err(self.error(KvIrErrorKind::Limit(KvIrLimitViolation::new(
                KvIrLimitResource::UnitBytes,
                self.options.limits.unit_bytes.saturating_add(1),
                self.options.limits.unit_bytes,
            ))));
        }
        Ok(())
    }

    #[inline]
    fn check_stream_growth(&self, additional: usize) -> Result<(), KvIrError> {
        let additional =
            u64::try_from(additional).map_err(|_| self.error(KvIrErrorKind::SizeOverflow))?;
        let next = self
            .stream_bytes
            .checked_add(additional)
            .ok_or_else(|| self.error(KvIrErrorKind::SizeOverflow))?;
        if next > self.options.limits.stream_bytes {
            return Err(self.error(KvIrErrorKind::Limit(KvIrLimitViolation::new(
                KvIrLimitResource::StreamBytes,
                self.options.limits.stream_bytes.saturating_add(1),
                self.options.limits.stream_bytes,
            ))));
        }
        Ok(())
    }

    #[inline]
    fn consume_buffered(&mut self, count: usize) -> Result<(), KvIrError> {
        self.input_start = self
            .input_start
            .checked_add(count)
            .ok_or_else(|| self.error(KvIrErrorKind::SizeOverflow))?;
        let count = u64::try_from(count).map_err(|_| self.error(KvIrErrorKind::SizeOverflow))?;
        self.input_offset = self
            .input_offset
            .checked_add(count)
            .ok_or_else(|| self.error(KvIrErrorKind::SizeOverflow))?;
        self.stream_bytes = self
            .stream_bytes
            .checked_add(count)
            .ok_or_else(|| self.error(KvIrErrorKind::SizeOverflow))?;
        Ok(())
    }

    #[inline]
    fn has_input(&mut self) -> Result<bool, KvIrError> {
        if self.input_start != self.input_end {
            return Ok(true);
        }
        self.refill()
    }

    /// Reads the next block, and is where every byte of I/O cost lives.
    ///
    /// Split out so the buffered case above stays small enough to inline: that runs once per
    /// byte of a stream, and this runs once per 8 KiB of it.
    #[cold]
    #[inline(never)]
    fn refill(&mut self) -> Result<bool, KvIrError> {
        while self.input_start == self.input_end && !self.reached_eof {
            match self.input.read(&mut self.input_buffer) {
                Ok(0) => self.reached_eof = true,
                Ok(count) => {
                    self.input_start = 0;
                    self.input_end = count;
                }
                Err(source) if source.kind() == io::ErrorKind::Interrupted => {}
                Err(source) => return Err(self.error(KvIrErrorKind::Input(source))),
            }
        }
        Ok(self.input_start != self.input_end)
    }

    const fn error(&self, kind: KvIrErrorKind) -> KvIrError {
        KvIrError {
            input_offset: self.input_offset,
            stream_index: self.current_stream_index,
            unit_index: self.current_unit_index,
            kind,
        }
    }
}

#[derive(Clone, Copy)]
enum StringPacket {
    Ordinary,
    Dictionary,
    Logtype,
}
