//! Borrowed records, CLP dictionaries, and their in-memory schema tables.

use std::convert::Infallible;
use std::error::Error;
use std::fmt;
use std::fmt::Display;
use std::fmt::Formatter;
use std::io::Write;

use ahash::AHashMap as HashMap;
use ahash::RandomState;
use hashbrown::HashTable;
use smallvec::SmallVec;

use super::WriterError;
use super::WriterLimits;
use super::WriterOptions;
use super::WriterResource;
use super::array;
use super::array::ArrayValidationScratch;
use super::array::ResolvedStructuredArrayValue;
use super::array::StructuredArrayNodeResolver;
use super::array::StructuredArrayPlan;
use super::array::StructuredArrayPlanError;
use super::array::StructuredArrayPlanLimits;
use super::array::StructuredArrayPlanResource;
use super::array::UnstructuredArrayError;
use super::array::UnstructuredArrayRef;
use super::check_limit;
use super::clp;
use super::encode_empty_dictionary;
use super::len_u64;
use super::retained_float;
use super::retained_float::RetainedFloatEncoding;
use super::retained_float::RetainedFloatError;
use super::retained_float::RetainedFloatRef;
use super::retained_float::RetainedFloatScratch;
use super::timestamp::PrevalidatedTimestampRef;
use super::timestamp::TimestampDictionaryBuilder;
use super::timestamp::TimestampError;
use super::timestamp::TimestampPlan;
use super::timestamp::TimestampRef;
use super::timestamp::TimestampReservations;
use super::timestamp::prepare_reservations as prepare_timestamp_reservations;
use crate::LOG_EVENT_IDX_KEY;
use crate::archive::NodeType;
use crate::archive::SchemaEntry;
use crate::ingest::KvIrEncodedVariable;
use crate::ingest::KvIrEncoding;
use crate::ingest::KvIrLogEvent;
use crate::ingest::KvIrNamespace;
use crate::ingest::KvIrValueKind;

const MAX_SCHEMA_NODE_ID: u64 = 0x00ff_ffff;
const MAX_SCHEMA_ID: u64 = 2_147_483_647;
const MAX_LOG_TYPE_ID: u64 = (1_u64 << 24) - 1;
const MAX_ENCODED_VARIABLE_OFFSET: u64 = (1_u64 << 40) - 1;
const MAX_ENCODED_VARIABLE_COUNT: u64 = 1_u64 << 40;
const MAX_LOG_EVENT_INDEX: u64 = i64::MAX as u64;
const OLDER_FIXED_DICTIONARY_HINT_LIMIT: usize = 2;
const LINEAR_DUPLICATE_FIELD_LIMIT: usize = 16;
const COMMON_OBJECT_NESTING_CAPACITY: usize = 4;
const COMMON_RECORD_FIELD_CAPACITY: usize = 16;
const COMMON_DICTIONARY_PLAN_ENTRY_CAPACITY: usize = 4;
const COMMON_DICTIONARY_PLAN_VALUE_CAPACITY: usize = 128;
const DICTIONARY_ENCODE_BUFFER_CAPACITY: usize = 128 * 1024;
const TABLE_METADATA_FIXED_SIZE: usize = 3 * size_of::<u64>();
const PACKED_STREAM_METADATA_SIZE: usize = 2 * size_of::<u64>();
const SCHEMA_TABLE_METADATA_SIZE: usize =
    2 * size_of::<u64>() + size_of::<i32>() + size_of::<u64>();
type TreeIdentity = (Option<u32>, NodeType, u64);
type TreeBucket = (TreeIdentity, Vec<u32>);
type EventFrames = SmallVec<[EventObjectFrame; COMMON_OBJECT_NESTING_CAPACITY]>;
type SeenEventFields<'record> = SmallVec<[SeenEventField<'record>; COMMON_RECORD_FIELD_CAPACITY]>;
type RecordEntries = Vec<u32>;
type PendingFields<'record> = Vec<PendingField<'record>>;
type BorrowedRecordValues<'record> = Vec<BorrowedPlannedValue<'record>>;
type PlannedRecordValues = Vec<PlannedValue>;
type SortedValuePositions = SmallVec<[usize; COMMON_RECORD_FIELD_CAPACITY]>;
type PlannedDictionaryEntries =
    SmallVec<[PlannedDictionaryEntry; COMMON_DICTIONARY_PLAN_ENTRY_CAPACITY]>;
type PlannedDictionaryValues = SmallVec<[u8; COMMON_DICTIONARY_PLAN_VALUE_CAPACITY]>;

/// One borrowed object record.
///
/// Field names are arbitrary bytes because the SFA schema-tree wire format is byte-oriented.
/// Callers producing JSON should supply valid UTF-8 names. Fields retain their supplied ordering
/// only while the schema is discovered; CLP-S stores columns in schema-node order.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RecordRef<'a> {
    fields: &'a [FieldRef<'a>],
}

impl<'a> RecordRef<'a> {
    /// Borrows the fields of one root object.
    #[must_use]
    pub const fn new(fields: &'a [FieldRef<'a>]) -> Self {
        Self { fields }
    }

    /// Returns the root object's fields.
    #[must_use]
    pub const fn fields(self) -> &'a [FieldRef<'a>] {
        self.fields
    }
}

/// One borrowed object field.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FieldRef<'a> {
    key: &'a [u8],
    value: ValueRef<'a>,
}

impl<'a> FieldRef<'a> {
    /// Creates a field from exact key bytes and a borrowed or copied value.
    #[must_use]
    pub const fn new(key: &'a [u8], value: ValueRef<'a>) -> Self {
        Self { key, value }
    }

    /// Returns the exact field-name bytes.
    #[must_use]
    pub const fn key(self) -> &'a [u8] {
        self.key
    }

    /// Returns the field value.
    #[must_use]
    pub const fn value(self) -> ValueRef<'a> {
        self.value
    }
}

/// One event in a borrowed, flat object-record traversal.
///
/// The root object is implicit. [`Self::ObjectStart`] and [`Self::ObjectEnd`] delimit nested
/// objects; [`Self::Value`] represents one root or nested field. This form lets streaming parsers
/// append arbitrarily nested objects without constructing self-referential [`FieldRef`] slices.
#[derive(Clone, Copy, Debug, PartialEq)]
#[non_exhaustive]
pub enum RecordEventRef<'a> {
    /// A scalar, exact timestamp, unstructured array, or prebuilt borrowed object field.
    Value(FieldRef<'a>),
    /// Starts a nested object with the given field-name bytes.
    ObjectStart(&'a [u8]),
    /// Ends the most recently started nested object.
    ObjectEnd,
}

impl<'a> RecordEventRef<'a> {
    /// Creates a value event from a field name and borrowed or copied value.
    #[must_use]
    pub const fn value(key: &'a [u8], value: ValueRef<'a>) -> Self {
        Self::Value(FieldRef::new(key, value))
    }

    /// Starts a nested object field.
    #[must_use]
    pub const fn object_start(key: &'a [u8]) -> Self {
        Self::ObjectStart(key)
    }
}

/// A value supported by the borrowed record writer.
///
/// String classification intentionally matches the current C++ JSON parser: decoded bytes that
/// contain a literal ASCII space (`0x20`) use a CLP logtype column; every other byte string uses a
/// variable-dictionary column. Tabs, newlines, and Unicode whitespace are not ASCII spaces.
/// [`Self::Array`] uses the C++ structured-array schema representation, retaining heterogeneous and
/// repeated element occurrences. Floating-point values must be finite.
#[derive(Clone, Copy, Debug, PartialEq)]
#[non_exhaustive]
pub enum ValueRef<'a> {
    /// JSON null; this is represented in the schema and has no physical column.
    Null,
    /// A signed 64-bit integer.
    I64(i64),
    /// A finite binary64 value stored in an ordinary `Float` column without source spelling.
    F64(f64),
    /// A finite binary64 value paired with its exact JSON number token.
    ///
    /// Compactly restorable tokens use `FormattedFloat`; every other consistent finite token uses
    /// `DictionaryFloat` so extraction remains byte-exact.
    RetainedFloat(RetainedFloatRef<'a>),
    /// A Boolean value.
    Bool(bool),
    /// A nested object borrowing its fields.
    Object(&'a [FieldRef<'a>]),
    /// Exact decoded string bytes, classified with the C++ literal-ASCII-space rule.
    String(&'a [u8]),
    /// A structured array whose elements are flattened into the schema's unordered region.
    Array(&'a [Self]),
    /// An exact default-mode JSON array lexeme stored through `/array.dict`.
    UnstructuredArray(UnstructuredArrayRef<'a>),
    /// An epoch value paired with its exact lexeme, resolved pattern, and range descriptor.
    Timestamp(TimestampRef<'a>),
    /// A crate-generated timestamp whose exact lexeme has already been validated.
    #[doc(hidden)]
    PrevalidatedTimestamp(PrevalidatedTimestampRef<'a>),
}

/// A legacy unsupported-value category retained for source compatibility.
///
/// Structured [`ValueRef::Array`] values are supported. New unsupported modeled values, if any,
/// will be added without reusing existing discriminants.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum UnsupportedValue {
    /// Legacy structured-array category; no longer emitted by the writer.
    Array,
}

impl Display for UnsupportedValue {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Array => "array",
        })
    }
}

/// A record resource bounded during append.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum AppendResource {
    /// Records across the archive.
    Records,
    /// Typed object/structured-array nesting depth, including the root object.
    NestingDepth,
    /// Interned schema-tree nodes.
    SchemaNodes,
    /// Distinct record schemas and their physical tables.
    Schemas,
    /// Physical columns across all schema tables.
    Columns,
    /// Flattened unordered schema entries contributed by structured arrays in one record.
    StructuredArraySchemaEntries,
    /// Owned key, schema-entry, dictionary-value, and encoded-column payload bytes.
    ResidentBytes,
    /// Distinct values in `/var.dict`.
    VariableDictionaryEntries,
    /// Distinct escaped templates in `/log.dict`.
    LogTypeDictionaryEntries,
    /// Distinct escaped templates in `/array.dict`.
    ArrayDictionaryEntries,
    /// Bytes in one variable or escaped-logtype dictionary entry.
    DictionaryEntryBytes,
    /// Cumulative dictionary value bytes across `/var.dict`, `/log.dict`, and `/array.dict`.
    DictionaryValueBytes,
    /// Encoded variables accumulated in one CLP string column.
    EncodedVariablesPerColumn,
    /// Encoded variables accumulated across all CLP string columns.
    TotalEncodedVariables,
    /// Timestamp range entries associated with schema-tree columns.
    TimestampRanges,
    /// Distinct resolved timestamp patterns.
    TimestampPatterns,
    /// UTF-8 bytes in one authoritative timestamp descriptor.
    TimestampRangeKeyBytes,
    /// UTF-8 bytes in one resolved timestamp pattern.
    TimestampPatternBytes,
    /// Cumulative resolved timestamp-pattern bytes.
    TimestampPatternValueBytes,
    /// UTF-8 bytes in one exact timestamp lexeme.
    TimestampLexemeBytes,
    /// Bytes in one exact unstructured-array JSON lexeme.
    UnstructuredArrayLexemeBytes,
    /// Nested array/object containers in one unstructured-array JSON lexeme.
    UnstructuredArrayNestingDepth,
    /// Reusable parser stack for validating unstructured-array JSON lexemes.
    ArrayValidationStack,
}

impl Display for AppendResource {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Records => "records",
            Self::NestingDepth => "typed container nesting depth",
            Self::SchemaNodes => "schema-tree nodes",
            Self::Schemas => "schemas",
            Self::Columns => "physical columns",
            Self::StructuredArraySchemaEntries => "structured-array schema entries",
            Self::ResidentBytes => "resident payload bytes",
            Self::VariableDictionaryEntries => "variable-dictionary entries",
            Self::LogTypeDictionaryEntries => "logtype-dictionary entries",
            Self::ArrayDictionaryEntries => "array-dictionary entries",
            Self::DictionaryEntryBytes => "dictionary entry bytes",
            Self::DictionaryValueBytes => "dictionary value bytes",
            Self::EncodedVariablesPerColumn => "encoded variables in one column",
            Self::TotalEncodedVariables => "encoded variables across columns",
            Self::TimestampRanges => "timestamp ranges",
            Self::TimestampPatterns => "timestamp patterns",
            Self::TimestampRangeKeyBytes => "timestamp range-key bytes",
            Self::TimestampPatternBytes => "timestamp pattern bytes",
            Self::TimestampPatternValueBytes => "timestamp pattern value bytes",
            Self::TimestampLexemeBytes => "timestamp lexeme bytes",
            Self::UnstructuredArrayLexemeBytes => "unstructured-array lexeme bytes",
            Self::UnstructuredArrayNestingDepth => "unstructured-array nesting depth",
            Self::ArrayValidationStack => "unstructured-array validation stack",
        })
    }
}

/// A fixed SFA wire domain exceeded while planning a record.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum AppendDomain {
    /// Schema-tree IDs must remain nonnegative and leave the high byte clear for schema delimiters.
    SchemaNodeId,
    /// Schema IDs are signed 32-bit values assigned from zero.
    SchemaId,
    /// Schema entry counts are unsigned 32-bit values.
    SchemaEntries,
    /// Unordered object/array delimiter bodies occupy 24 bits.
    UnorderedContainerBodyLength,
    /// Table message counts are unsigned 64-bit values.
    TableMessages,
    /// CLP logtype dictionary IDs occupy the low 24 descriptor bits.
    LogTypeId,
    /// CLP encoded-variable offsets occupy the high 40 descriptor bits.
    EncodedVariableOffset,
    /// A CLP column's encoded-variable count has a fixed 40-bit addressable domain.
    EncodedVariableCount,
    /// Archive-global log-event indexes are nonnegative signed 64-bit values.
    LogEventIndex,
}

impl Display for AppendDomain {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::SchemaNodeId => "schema-node ID",
            Self::SchemaId => "schema ID",
            Self::SchemaEntries => "schema-entry count",
            Self::UnorderedContainerBodyLength => "24-bit unordered-container body length",
            Self::TableMessages => "table message count",
            Self::LogTypeId => "24-bit logtype ID",
            Self::EncodedVariableOffset => "40-bit encoded-variable offset",
            Self::EncodedVariableCount => "40-bit encoded-variable count",
            Self::LogEventIndex => "log-event index",
        })
    }
}

/// Structural failure in a flat [`RecordEventRef`] traversal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum RecordEventError {
    /// An object-end event attempted to close the implicit root object.
    UnexpectedObjectEnd,
    /// The event stream ended before all nested objects were closed.
    UnclosedObjects {
        /// Number of still-open nested objects, excluding the implicit root.
        count: usize,
    },
    /// A KV-IR namespace boundary appeared inside a nested object.
    KvIrNamespaceInsideObject {
        /// Number of open nested objects below the namespace root.
        depth: usize,
    },
}

impl Display for RecordEventError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnexpectedObjectEnd => {
                formatter.write_str("object-end event attempted to close the implicit root")
            }
            Self::UnclosedObjects { count } => {
                write!(
                    formatter,
                    "event stream ended with {count} unclosed nested objects"
                )
            }
            Self::KvIrNamespaceInsideObject { depth } => write!(
                formatter,
                "KV-IR namespace boundary appeared inside {depth} nested objects"
            ),
        }
    }
}

impl Error for RecordEventError {}

/// Failure from a fallible flat record-event source or archive planning.
#[derive(Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum RecordEventAppendError<E> {
    /// The caller-owned event source failed before the archive changed.
    Source {
        /// Zero-based index of the event the writer requested.
        event_index: usize,
        /// Caller-owned source failure.
        source: E,
    },
    /// A complete event supplied by the source failed archive validation or planning.
    Append(AppendError),
}

impl<E: Display> Display for RecordEventAppendError<E> {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Source {
                event_index,
                source,
            } => write!(
                formatter,
                "record event source failed at event {event_index}: {source}"
            ),
            Self::Append(source) => Display::fmt(source, formatter),
        }
    }
}

impl<E: Error + 'static> Error for RecordEventAppendError<E> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Source { source, .. } => Some(source),
            Self::Append(source) => Some(source),
        }
    }
}

impl<E> From<AppendError> for RecordEventAppendError<E> {
    fn from(source: AppendError) -> Self {
        Self::Append(source)
    }
}

/// Internal push interface used by replayable, parser-owned record sources.
///
/// Unlike [`RecordEventRef`], this interface lets a source dispatch directly to the planner
/// without constructing an intermediate event that the planner immediately matches again.
pub trait RecordEventConsumer<'record> {
    fn value(&mut self, field: FieldRef<'record>) -> Result<(), AppendError>;

    #[doc(hidden)]
    fn kv_ir_namespace(&mut self, namespace: KvIrNamespace) -> Result<(), AppendError>;

    fn kv_ir_encoded_text(
        &mut self,
        key: &'record [u8],
        event: &'record KvIrLogEvent<'record>,
        pair_index: usize,
    ) -> Result<(), AppendError>;

    fn object_start(&mut self, key: &'record [u8]) -> Result<(), AppendError>;

    fn object_end(&mut self) -> Result<(), AppendError>;
}

/// Internal source whose immutable parser storage can be replayed before archive state changes.
///
/// Replay is used only for an optimistic cached-layout proof. Every failed proof is discarded and
/// consumed again through the ordinary duplicate-validating traversal.
pub trait ReplayableRecordEventSource<'record>: Clone {
    type Error;

    fn consume<C>(self, consumer: &mut C) -> Result<(), RecordEventAppendError<Self::Error>>
    where
        C: RecordEventConsumer<'record>;

    fn supports_cached_layout_proof(&self) -> bool;
}

/// Failure to validate, reserve, or atomically append one borrowed record.
#[derive(Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum AppendError {
    /// The borrowed value is modeled but its required writer is not implemented yet.
    UnsupportedValue {
        /// Unsupported value category.
        value: UnsupportedValue,
    },
    /// Two fields in the same object have identical key bytes.
    DuplicateField {
        /// One-based nesting depth of the containing object.
        object_depth: u64,
        /// Index of the first occurrence.
        previous_index: usize,
        /// Index of the duplicate occurrence.
        field_index: usize,
    },
    /// A flat record event stream was not structurally balanced.
    InvalidRecordEvents {
        /// Zero-based event index, or the event count when the stream ended early.
        event_index: usize,
        /// Structural failure at that position.
        reason: RecordEventError,
    },
    /// A floating-point field is NaN or infinite.
    NonFiniteFloat {
        /// Implicit schema-node ID planned for the field.
        node_id: u32,
    },
    /// A retained floating-point value or source token is invalid or inconsistent.
    RetainedFloat {
        /// Validation failure, reported before archive state changes.
        reason: RetainedFloatError,
    },
    /// An exact timestamp value, pattern, lexeme, or range descriptor is invalid or inconsistent.
    Timestamp {
        /// Implicit schema-tree node ID planned for the timestamp column.
        node_id: u32,
        /// Validation failure, reported before archive state changes.
        reason: TimestampError,
    },
    /// An exact unstructured-array lexeme is not valid JSON or violates its type contract.
    UnstructuredArray {
        /// Implicit schema-tree node ID planned for the array column.
        node_id: u32,
        /// Validation failure, reported before archive state changes.
        reason: UnstructuredArrayError,
    },
    /// A caller-configured append limit was exceeded.
    LimitExceeded {
        /// Bounded resource.
        resource: AppendResource,
        /// Proposed value after the append.
        actual: u64,
        /// Configured maximum.
        limit: u64,
    },
    /// A fixed wire-format domain was exceeded.
    FormatDomainExceeded {
        /// Wire domain.
        domain: AppendDomain,
        /// Proposed value.
        actual: u64,
        /// Largest representable value.
        limit: u64,
    },
    /// Checked arithmetic or a platform-size conversion overflowed.
    SizeOverflow,
    /// A bounded staging or destination allocation could not be reserved.
    AllocationFailed {
        /// Resource whose backing storage could not be reserved.
        resource: AppendResource,
        /// Number of elements or bytes requested from the allocator.
        requested: usize,
    },
}

impl Display for AppendError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedValue { value } => {
                write!(
                    formatter,
                    "{value} values are not supported by this writer milestone"
                )
            }
            Self::DuplicateField {
                object_depth,
                previous_index,
                field_index,
            } => write!(
                formatter,
                "object at depth {object_depth} repeats field {previous_index} at index \
                 {field_index}"
            ),
            Self::InvalidRecordEvents {
                event_index,
                reason,
            } => write!(
                formatter,
                "invalid record event stream at event {event_index}: {reason}"
            ),
            Self::NonFiniteFloat { node_id } => {
                write!(
                    formatter,
                    "schema node {node_id} contains a non-finite float"
                )
            }
            Self::RetainedFloat { reason } => {
                write!(formatter, "invalid retained floating-point value: {reason}")
            }
            Self::Timestamp { node_id, reason } => {
                write!(
                    formatter,
                    "invalid timestamp at schema node {node_id}: {reason}"
                )
            }
            Self::UnstructuredArray { node_id, reason } => {
                write!(
                    formatter,
                    "invalid unstructured array at schema node {node_id}: {reason}"
                )
            }
            Self::LimitExceeded {
                resource,
                actual,
                limit,
            } => write!(
                formatter,
                "{resource} {actual} exceeds writer limit {limit}"
            ),
            Self::FormatDomainExceeded {
                domain,
                actual,
                limit,
            } => write!(
                formatter,
                "{domain} value {actual} exceeds wire-format maximum {limit}"
            ),
            Self::SizeOverflow => formatter.write_str("record append size overflow"),
            Self::AllocationFailed {
                resource,
                requested,
            } => write!(
                formatter,
                "failed to reserve {requested} elements or bytes for {resource}"
            ),
        }
    }
}

impl Error for AppendError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::RetainedFloat { reason } => Some(reason),
            Self::Timestamp { reason, .. } => Some(reason),
            Self::UnstructuredArray { reason, .. } => Some(reason),
            Self::InvalidRecordEvents { reason, .. } => Some(reason),
            _ => None,
        }
    }
}

#[derive(Debug, Default)]
struct RecordPlanScratch {
    entries: RecordEntries,
    values: PlannedRecordValues,
}

impl RecordPlanScratch {
    fn recycle(mut entries: RecordEntries, mut values: PlannedRecordValues) -> Self {
        entries.clear();
        values.clear();
        if entries.capacity() > COMMON_RECORD_FIELD_CAPACITY {
            entries = RecordEntries::new();
        }
        if values.capacity() > COMMON_RECORD_FIELD_CAPACITY {
            values = PlannedRecordValues::new();
        }
        Self { entries, values }
    }
}

#[derive(Debug, Default)]
pub(super) struct PrimitiveArchive {
    tree: SchemaTreeBuilder,
    tables: TableSet,
    variable_dictionary: DictionaryBuilder,
    log_type_dictionary: DictionaryBuilder,
    array_dictionary: DictionaryBuilder,
    timestamp_dictionary: TimestampDictionaryBuilder,
    timestamp_scratch: String,
    array_validation_scratch: ArrayValidationScratch,
    retained_float_scratch: RetainedFloatScratch,
    record_plan_scratch: RecordPlanScratch,
    node_path_cache: Vec<u32>,
    record_layout_cache: Option<RecordLayoutCache>,
    last_record_layout_cache_hit: bool,
    record_count: u64,
    resident_bytes: u64,
    encoded_data_size: u64,
}

impl PrimitiveArchive {
    pub(super) const fn record_count(&self) -> u64 {
        self.record_count
    }

    pub(super) const fn schema_count(&self) -> usize {
        self.tables.tables.len()
    }

    pub(super) const fn resident_bytes(&self) -> u64 {
        self.resident_bytes
    }

    pub(super) const fn encoded_data_size(&self) -> u64 {
        self.encoded_data_size
    }

    pub(super) fn timestamp_bounds(&self) -> (i64, i64) {
        self.timestamp_dictionary.archive_bounds()
    }

    pub(super) fn append(
        &mut self,
        record: RecordRef<'_>,
        limits: WriterLimits,
        record_log_order: bool,
    ) -> Result<(), AppendError> {
        let mut timestamp_scratch = std::mem::take(&mut self.timestamp_scratch);
        let mut array_validation_scratch = std::mem::take(&mut self.array_validation_scratch);
        let mut retained_float_scratch = std::mem::take(&mut self.retained_float_scratch);
        let record_plan_scratch = std::mem::take(&mut self.record_plan_scratch);
        let plan = RecordPlan::build(
            self,
            record,
            limits,
            record_log_order,
            record_plan_scratch,
            &mut timestamp_scratch,
            &mut array_validation_scratch,
            &mut retained_float_scratch,
        );
        self.timestamp_scratch = timestamp_scratch;
        self.array_validation_scratch = array_validation_scratch;
        self.retained_float_scratch = retained_float_scratch;
        let plan = plan?;
        let reservations = CommitReservations::prepare(self, &plan)?;
        self.commit(plan, reservations);
        Ok(())
    }

    pub(super) fn append_events<'record, I>(
        &mut self,
        events: I,
        limits: WriterLimits,
        record_log_order: bool,
    ) -> Result<(), AppendError>
    where
        I: IntoIterator<Item = RecordEventRef<'record>>, {
        self.try_append_events(
            events.into_iter().map(Ok::<_, Infallible>),
            limits,
            record_log_order,
        )
        .map_err(|error| match error {
            RecordEventAppendError::Source { source, .. } => match source {},
            RecordEventAppendError::Append(source) => source,
        })
    }

    pub(super) fn try_append_events<'record, I, E>(
        &mut self,
        events: I,
        limits: WriterLimits,
        record_log_order: bool,
    ) -> Result<(), RecordEventAppendError<E>>
    where
        I: IntoIterator<Item = Result<RecordEventRef<'record>, E>>, {
        let mut timestamp_scratch = std::mem::take(&mut self.timestamp_scratch);
        let mut array_validation_scratch = std::mem::take(&mut self.array_validation_scratch);
        let mut retained_float_scratch = std::mem::take(&mut self.retained_float_scratch);
        let record_plan_scratch = std::mem::take(&mut self.record_plan_scratch);
        let plan = RecordPlan::build_events(
            self,
            events,
            limits,
            record_log_order,
            record_plan_scratch,
            &mut timestamp_scratch,
            &mut array_validation_scratch,
            &mut retained_float_scratch,
        );
        self.timestamp_scratch = timestamp_scratch;
        self.array_validation_scratch = array_validation_scratch;
        self.retained_float_scratch = retained_float_scratch;
        let plan = plan?;
        let reservations = CommitReservations::prepare(self, &plan)?;
        self.commit(plan, reservations);
        Ok(())
    }

    pub(super) fn try_append_replayable_events<'record, S>(
        &mut self,
        source: S,
        limits: WriterLimits,
        record_log_order: bool,
    ) -> Result<(), RecordEventAppendError<S::Error>>
    where
        S: ReplayableRecordEventSource<'record>, {
        let mut timestamp_scratch = std::mem::take(&mut self.timestamp_scratch);
        let mut array_validation_scratch = std::mem::take(&mut self.array_validation_scratch);
        let mut retained_float_scratch = std::mem::take(&mut self.retained_float_scratch);
        let record_plan_scratch = std::mem::take(&mut self.record_plan_scratch);
        let plan = RecordPlan::build_replayable_events(
            self,
            source,
            limits,
            record_log_order,
            record_plan_scratch,
            &mut timestamp_scratch,
            &mut array_validation_scratch,
            &mut retained_float_scratch,
        );
        self.timestamp_scratch = timestamp_scratch;
        self.array_validation_scratch = array_validation_scratch;
        self.retained_float_scratch = retained_float_scratch;
        let plan = plan?;
        let reservations = CommitReservations::prepare(self, &plan)?;
        self.commit(plan, reservations);
        Ok(())
    }

    fn commit(&mut self, plan: RecordPlan, reservations: CommitReservations) {
        let cached_layout_matched = plan.cached_layout_matched;
        self.tree.commit(plan.nodes, reservations.tree);
        if let Some(node_path_cache) = plan.node_path_cache {
            self.node_path_cache = node_path_cache;
        }
        self.variable_dictionary
            .commit(plan.variable_dictionary, reservations.variable_dictionary);
        self.log_type_dictionary
            .commit(plan.log_type_dictionary, reservations.log_type_dictionary);
        self.array_dictionary
            .commit(plan.array_dictionary, reservations.array_dictionary);
        self.timestamp_dictionary
            .commit(plan.timestamp_dictionary, reservations.timestamp_dictionary);
        self.record_plan_scratch = self.tables.commit(plan.table, reservations.table);
        if let Some(record_layout_cache) = plan.record_layout_cache {
            self.record_layout_cache = Some(record_layout_cache);
        }
        self.last_record_layout_cache_hit = cached_layout_matched;
        self.record_count += 1;
        self.resident_bytes = plan.resulting_resident_bytes;
        self.encoded_data_size = plan.resulting_encoded_data_size;
    }

    pub(super) fn encode_sections(
        &self,
        options: WriterOptions,
    ) -> Result<([Vec<u8>; 7], Vec<u8>), WriterError> {
        let schema_tree = self.tree.encode(options.compression_level())?;
        let schema_map = self.tables.encode_schema_map(options.compression_level())?;
        let (table_metadata, tables) = self.tables.encode_tables(options)?;
        let sections = [
            schema_tree,
            schema_map,
            table_metadata,
            self.variable_dictionary
                .encode(options.compression_level())?,
            self.log_type_dictionary
                .encode(options.compression_level())?,
            self.array_dictionary.encode(options.compression_level())?,
            tables,
        ];
        check_section_limits(&sections, options.limits())?;
        Ok((sections, self.timestamp_dictionary.encode()?))
    }
}

#[derive(Debug)]
struct SchemaNodeRecord {
    parent: Option<u32>,
    key: Vec<u8>,
    node_type: NodeType,
}

#[derive(Debug, Default)]
struct SchemaTreeBuilder {
    nodes: Vec<SchemaNodeRecord>,
    identities: HashMap<TreeIdentity, Vec<u32>>,
}

impl SchemaTreeBuilder {
    fn find(&self, parent: Option<u32>, node_type: NodeType, key: &[u8]) -> Option<u32> {
        self.identities
            .get(&(parent, node_type, hash_bytes(key)))
            .and_then(|ids| {
                ids.iter().copied().find(|id| {
                    usize::try_from(*id)
                        .ok()
                        .and_then(|index| self.nodes.get(index))
                        .is_some_and(|node| node.key == key)
                })
            })
    }

    fn commit(&mut self, nodes: Vec<SchemaNodeRecord>, reservations: TreeReservations) {
        if nodes.is_empty() {
            return;
        }
        for (key, bucket) in reservations.new_buckets {
            self.identities.insert(key, bucket);
        }
        let first_id = self.nodes.len();
        for (offset, node) in nodes.into_iter().enumerate() {
            let id =
                u32::try_from(first_id + offset).expect("validated schema-node ID must fit u32");
            let identity = (node.parent, node.node_type, hash_bytes(&node.key));
            if let Some(bucket) = self.identities.get_mut(&identity)
                && !bucket.contains(&id)
            {
                bucket.push(id);
            }
            self.nodes.push(node);
        }
    }

    fn encode(&self, compression_level: i32) -> Result<Vec<u8>, WriterError> {
        let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), compression_level)
            .map_err(WriterError::Io)?;
        write_u64(&mut encoder, usize_u64(self.nodes.len())?)?;
        for node in &self.nodes {
            write_i32(
                &mut encoder,
                node.parent.map_or(-1, |parent| {
                    i32::try_from(parent).expect("validated parent ID fits i32")
                }),
            )?;
            write_u64(&mut encoder, len_u64(&node.key)?)?;
            encoder.write_all(&node.key).map_err(WriterError::Io)?;
            encoder
                .write_all(&[node.node_type as u8])
                .map_err(WriterError::Io)?;
        }
        encoder.finish().map_err(WriterError::Io)
    }
}

#[derive(Clone, Copy, Debug)]
enum DictionaryKind {
    Variable,
    LogType,
    Array,
}

impl DictionaryKind {
    const fn entries_resource(self) -> AppendResource {
        match self {
            Self::Variable => AppendResource::VariableDictionaryEntries,
            Self::LogType => AppendResource::LogTypeDictionaryEntries,
            Self::Array => AppendResource::ArrayDictionaryEntries,
        }
    }

    const fn entry_limit(self, limits: WriterLimits) -> u64 {
        match self {
            Self::Variable => limits.max_variable_dictionary_entries(),
            Self::LogType => limits.max_log_type_dictionary_entries(),
            Self::Array => limits.max_array_dictionary_entries(),
        }
    }
}

#[derive(Debug, Default)]
struct DictionaryHashBuilder {
    state: RandomState,
    #[cfg(test)]
    forced_hash: Option<u64>,
}

impl DictionaryHashBuilder {
    #[inline]
    fn hash(&self, value: &[u8]) -> u64 {
        #[cfg(test)]
        if let Some(hash) = self.forced_hash {
            return hash;
        }
        self.state.hash_one(value)
    }
}

fn dictionary_entry<'values>(
    values: &'values [u8],
    entry_ends: &[usize],
    index: usize,
) -> Option<&'values [u8]> {
    let start = if 0 == index {
        0
    } else {
        *entry_ends.get(index - 1)?
    };
    let end = *entry_ends.get(index)?;
    values.get(start..end)
}

fn dictionary_id_hash(
    hash_builder: &DictionaryHashBuilder,
    values: &[u8],
    entry_ends: &[usize],
    id: u64,
) -> u64 {
    let index = usize::try_from(id).expect("stored dictionary ID must fit usize");
    let value = dictionary_entry(values, entry_ends, index)
        .expect("stored dictionary ID must address an arena entry");
    hash_builder.hash(value)
}

#[derive(Debug, Default)]
struct DictionaryBuilder {
    values: Vec<u8>,
    entry_ends: Vec<usize>,
    index: HashTable<u64>,
    hash_builder: DictionaryHashBuilder,
    value_bytes: u64,
}

impl DictionaryBuilder {
    const fn entry_count(&self) -> usize {
        self.entry_ends.len()
    }

    fn entry(&self, id: u64) -> Option<&[u8]> {
        usize::try_from(id)
            .ok()
            .and_then(|index| dictionary_entry(&self.values, &self.entry_ends, index))
    }

    fn hash(&self, value: &[u8]) -> u64 {
        self.hash_builder.hash(value)
    }

    fn find(&self, hash: u64, value: &[u8]) -> Option<u64> {
        self.index
            .find(hash, |id| {
                self.entry(*id).is_some_and(|entry| entry == value)
            })
            .copied()
    }

    fn entry_matches(&self, id: u64, value: &[u8]) -> bool {
        self.entry(id).is_some_and(|entry| entry == value)
    }

    #[cfg(test)]
    const fn force_hash(&mut self, hash: u64) {
        self.hash_builder.forced_hash = Some(hash);
    }

    fn commit(&mut self, plan: DictionaryPlan, reservations: DictionaryReservations) {
        if plan.entries.is_empty() {
            return;
        }
        let DictionaryPlan {
            values: planned_values,
            entries: planned_entries,
            index: _,
            added_value_bytes,
            added_data_size: _,
        } = plan;
        let first_id = self.entry_count();
        let first_value_offset = self.values.len();
        self.values.extend_from_slice(&planned_values);
        for entry in &planned_entries {
            self.entry_ends.push(
                first_value_offset
                    .checked_add(entry.end)
                    .expect("reserved dictionary arena length must fit usize"),
            );
        }
        let hash_builder = &self.hash_builder;
        let values = &self.values;
        let entry_ends = &self.entry_ends;
        for (offset, entry) in planned_entries.iter().enumerate() {
            let id = u64::try_from(
                first_id
                    .checked_add(offset)
                    .expect("validated dictionary entry count must fit usize"),
            )
            .expect("validated dictionary entry count must fit u64");
            let _ = self.index.insert_unique(entry.hash, id, |id| {
                dictionary_id_hash(hash_builder, values, entry_ends, *id)
            });
        }
        self.value_bytes = self
            .value_bytes
            .checked_add(added_value_bytes)
            .expect("validated dictionary value bytes must fit u64");
        debug_assert_eq!(reservations.resulting_value_len, self.values.len());
        debug_assert_eq!(reservations.resulting_entry_count, self.entry_count());
    }

    fn encode(&self, compression_level: i32) -> Result<Vec<u8>, WriterError> {
        if 0 == self.entry_count() {
            return encode_empty_dictionary();
        }
        let mut section = Vec::new();
        section
            .try_reserve_exact(size_of::<u64>())
            .map_err(|_| WriterError::AllocationFailed {
                requested: size_of::<u64>(),
            })?;
        section.extend_from_slice(&usize_u64(self.entry_count())?.to_le_bytes());
        let mut pending = Vec::new();
        pending
            .try_reserve_exact(DICTIONARY_ENCODE_BUFFER_CAPACITY)
            .map_err(|_| WriterError::AllocationFailed {
                requested: DICTIONARY_ENCODE_BUFFER_CAPACITY,
            })?;
        let mut encoder = zstd::stream::write::Encoder::new(&mut section, compression_level)
            .map_err(WriterError::Io)?;
        for index in 0..self.entry_count() {
            let entry = dictionary_entry(&self.values, &self.entry_ends, index)
                .expect("stored dictionary offset must address an arena entry");
            write_dictionary_encode_bytes(
                &mut encoder,
                &mut pending,
                &len_u64(entry)?.to_le_bytes(),
            )?;
            write_dictionary_encode_bytes(&mut encoder, &mut pending, entry)?;
        }
        if !pending.is_empty() {
            encoder.write_all(&pending).map_err(WriterError::Io)?;
        }
        encoder.finish().map_err(WriterError::Io)?;
        Ok(section)
    }
}

#[derive(Clone, Copy, Debug)]
struct PlannedDictionaryEntry {
    end: usize,
    hash: u64,
}

#[derive(Debug, Default)]
struct DictionaryPlan {
    values: PlannedDictionaryValues,
    entries: PlannedDictionaryEntries,
    index: Option<HashTable<usize>>,
    added_value_bytes: u64,
    added_data_size: u64,
}

impl DictionaryPlan {
    fn entry(&self, index: usize) -> Option<&[u8]> {
        let start = if 0 == index {
            0
        } else {
            self.entries.get(index - 1)?.end
        };
        let end = self.entries.get(index)?.end;
        self.values.get(start..end)
    }

    fn find(&self, hash: u64, value: &[u8]) -> Option<usize> {
        self.index.as_ref().map_or_else(
            || {
                self.entries
                    .iter()
                    .enumerate()
                    .find(|(entry_index, entry)| {
                        entry.hash == hash
                            && self.entry(*entry_index).is_some_and(|entry| entry == value)
                    })
                    .map(|(entry_index, _)| entry_index)
            },
            |index| {
                index
                    .find(hash, |entry_index| {
                        self.entry(*entry_index).is_some_and(|entry| entry == value)
                    })
                    .copied()
            },
        )
    }

    fn reserve_index_for_add(&mut self, kind: DictionaryKind) -> Result<(), AppendError> {
        if let Some(index) = &mut self.index {
            let entries = &self.entries;
            index
                .try_reserve(1, |entry_index| entries[*entry_index].hash)
                .map_err(|_| append_allocation(kind.entries_resource(), 1))?;
        } else if self.entries.len() >= COMMON_DICTIONARY_PLAN_ENTRY_CAPACITY {
            let requested = self
                .entries
                .len()
                .checked_add(1)
                .ok_or(AppendError::SizeOverflow)?;
            let mut index = HashTable::<usize>::new();
            let entries = &self.entries;
            index
                .try_reserve(requested, |entry_index| entries[*entry_index].hash)
                .map_err(|_| append_allocation(kind.entries_resource(), requested))?;
            for (entry_index, entry) in self.entries.iter().enumerate() {
                let _ = index.insert_unique(entry.hash, entry_index, |entry_index| {
                    self.entries[*entry_index].hash
                });
            }
            self.index = Some(index);
        }
        Ok(())
    }

    fn add(&mut self, hash: u64, value: &[u8], kind: DictionaryKind) -> Result<usize, AppendError> {
        let added_data_size = dictionary_entry_data_size(value, kind)?;
        let new_value_bytes = self
            .added_value_bytes
            .checked_add(usize_u64_append(value.len())?)
            .ok_or(AppendError::SizeOverflow)?;
        let new_data_size = self
            .added_data_size
            .checked_add(added_data_size)
            .ok_or(AppendError::SizeOverflow)?;
        let end = self
            .values
            .len()
            .checked_add(value.len())
            .ok_or(AppendError::SizeOverflow)?;
        self.values
            .try_reserve(value.len())
            .map_err(|_| append_allocation(AppendResource::DictionaryValueBytes, value.len()))?;
        self.entries
            .try_reserve(1)
            .map_err(|_| append_allocation(kind.entries_resource(), 1))?;
        self.reserve_index_for_add(kind)?;
        let entry_index = self.entries.len();
        self.values.extend_from_slice(value);
        self.entries.push(PlannedDictionaryEntry { end, hash });
        if let Some(index) = &mut self.index {
            let entries = &self.entries;
            let _ =
                index.insert_unique(hash, entry_index, |entry_index| entries[*entry_index].hash);
        }
        self.added_value_bytes = new_value_bytes;
        self.added_data_size = new_data_size;
        Ok(entry_index)
    }
}

fn dictionary_entry_data_size(value: &[u8], kind: DictionaryKind) -> Result<u64, AppendError> {
    let value_size = usize_u64_append(value.len())?;
    let placeholder_size = match kind {
        DictionaryKind::Variable => 0,
        DictionaryKind::LogType | DictionaryKind::Array => logtype_placeholder_count(value)?
            .checked_mul(u64::try_from(size_of::<usize>()).map_err(|_| AppendError::SizeOverflow)?)
            .ok_or(AppendError::SizeOverflow)?,
    };
    u64::try_from(size_of::<u64>())
        .map_err(|_| AppendError::SizeOverflow)?
        .checked_add(value_size)
        .and_then(|size| size.checked_add(placeholder_size))
        .ok_or(AppendError::SizeOverflow)
}

fn logtype_placeholder_count(value: &[u8]) -> Result<u64, AppendError> {
    let mut count = 0_u64;
    let mut index = 0_usize;
    while index < value.len() {
        if b'\\' == value[index] {
            count = count.checked_add(1).ok_or(AppendError::SizeOverflow)?;
            index = index.checked_add(2).ok_or(AppendError::SizeOverflow)?;
        } else {
            if matches!(
                value[index],
                clp::INTEGER_PLACEHOLDER | clp::DICTIONARY_PLACEHOLDER | clp::FLOAT_PLACEHOLDER
            ) {
                count = count.checked_add(1).ok_or(AppendError::SizeOverflow)?;
            }
            index += 1;
        }
    }
    Ok(count)
}

struct DictionaryPlans<'archive> {
    variable_base: &'archive DictionaryBuilder,
    log_type_base: &'archive DictionaryBuilder,
    array_base: &'archive DictionaryBuilder,
    variable: DictionaryPlan,
    log_type: DictionaryPlan,
    array: DictionaryPlan,
    timestamp_base: &'archive TimestampDictionaryBuilder,
    timestamp: TimestampPlan,
    resulting_value_bytes: u64,
}

impl<'archive> DictionaryPlans<'archive> {
    fn new(archive: &'archive PrimitiveArchive) -> Result<Self, AppendError> {
        let resulting_value_bytes = archive
            .variable_dictionary
            .value_bytes
            .checked_add(archive.log_type_dictionary.value_bytes)
            .and_then(|size| size.checked_add(archive.array_dictionary.value_bytes))
            .ok_or(AppendError::SizeOverflow)?;
        Ok(Self {
            variable_base: &archive.variable_dictionary,
            log_type_base: &archive.log_type_dictionary,
            array_base: &archive.array_dictionary,
            variable: DictionaryPlan::default(),
            log_type: DictionaryPlan::default(),
            array: DictionaryPlan::default(),
            timestamp_base: &archive.timestamp_dictionary,
            timestamp: TimestampPlan::default(),
            resulting_value_bytes,
        })
    }

    fn resolve_variable(
        &mut self,
        value: &[u8],
        hint: Option<&TableColumn>,
        limits: WriterLimits,
    ) -> Result<u64, AppendError> {
        resolve_dictionary(
            self.variable_base,
            &mut self.variable,
            &mut self.resulting_value_bytes,
            DictionaryKind::Variable,
            value,
            hint,
            limits,
        )
    }

    fn resolve_log_type(
        &mut self,
        value: &[u8],
        hint: Option<&TableColumn>,
        limits: WriterLimits,
    ) -> Result<u64, AppendError> {
        let id = resolve_dictionary(
            self.log_type_base,
            &mut self.log_type,
            &mut self.resulting_value_bytes,
            DictionaryKind::LogType,
            value,
            hint,
            limits,
        )?;
        validate_log_type_id(id)?;
        Ok(id)
    }

    fn resolve_array_log_type(
        &mut self,
        value: &[u8],
        hint: Option<&TableColumn>,
        limits: WriterLimits,
    ) -> Result<u64, AppendError> {
        let id = resolve_dictionary(
            self.array_base,
            &mut self.array,
            &mut self.resulting_value_bytes,
            DictionaryKind::Array,
            value,
            hint,
            limits,
        )?;
        validate_log_type_id(id)?;
        Ok(id)
    }

    fn resolve_timestamp(
        &mut self,
        node_id: u32,
        value: TimestampRef<'_>,
        prevalidated: bool,
        limits: WriterLimits,
        scratch: &mut String,
    ) -> Result<u64, AppendError> {
        self.timestamp.resolve(
            self.timestamp_base,
            node_id,
            value,
            prevalidated,
            limits,
            scratch,
        )
    }
}

const fn validate_log_type_id(id: u64) -> Result<(), AppendError> {
    if id > MAX_LOG_TYPE_ID {
        Err(AppendError::FormatDomainExceeded {
            domain: AppendDomain::LogTypeId,
            actual: id,
            limit: MAX_LOG_TYPE_ID,
        })
    } else {
        Ok(())
    }
}

fn validate_log_event_index(index: u64) -> Result<i64, AppendError> {
    if index > MAX_LOG_EVENT_INDEX {
        return Err(AppendError::FormatDomainExceeded {
            domain: AppendDomain::LogEventIndex,
            actual: index,
            limit: MAX_LOG_EVENT_INDEX,
        });
    }
    i64::try_from(index).map_err(|_| AppendError::SizeOverflow)
}

fn resolve_dictionary(
    base: &DictionaryBuilder,
    plan: &mut DictionaryPlan,
    resulting_value_bytes: &mut u64,
    kind: DictionaryKind,
    value: &[u8],
    hint: Option<&TableColumn>,
    limits: WriterLimits,
) -> Result<u64, AppendError> {
    if let Some(column) = hint {
        let last_id = column.last_dictionary_id();
        if let Some(id) = last_id
            && base.entry_matches(id, value)
        {
            return Ok(id);
        }
        for id in column.older_fixed_dictionary_ids() {
            if Some(id) != last_id && base.entry_matches(id, value) {
                return Ok(id);
            }
        }
    }
    let hash = base.hash(value);
    if let Some(id) = base.find(hash, value) {
        return Ok(id);
    }
    if let Some(index) = plan.find(hash, value) {
        return usize_u64_append(
            base.entry_count()
                .checked_add(index)
                .ok_or(AppendError::SizeOverflow)?,
        );
    }
    let value_len = usize_u64_append(value.len())?;
    check_append_limit(
        AppendResource::DictionaryEntryBytes,
        value_len,
        limits.max_dictionary_entry_size(),
    )?;
    let proposed_entries = usize_u64_append(base.entry_count())?
        .checked_add(usize_u64_append(plan.entries.len())?)
        .and_then(|count| count.checked_add(1))
        .ok_or(AppendError::SizeOverflow)?;
    check_append_limit(
        kind.entries_resource(),
        proposed_entries,
        kind.entry_limit(limits),
    )?;
    let proposed_value_bytes = resulting_value_bytes
        .checked_add(value_len)
        .ok_or(AppendError::SizeOverflow)?;
    check_append_limit(
        AppendResource::DictionaryValueBytes,
        proposed_value_bytes,
        limits.max_dictionary_value_bytes(),
    )?;
    let index = plan.add(hash, value, kind)?;
    *resulting_value_bytes = proposed_value_bytes;
    usize_u64_append(
        base.entry_count()
            .checked_add(index)
            .ok_or(AppendError::SizeOverflow)?,
    )
}

#[derive(Clone, Copy, Debug)]
struct BorrowedPlannedValue<'a> {
    node_id: u32,
    value: BorrowedScalarValue<'a>,
}

#[derive(Clone, Copy, Debug)]
enum BorrowedScalarValue<'a> {
    I64(i64),
    DeltaI64(i64),
    F64(f64),
    FormattedFloat {
        value: f64,
        descriptor: u16,
    },
    DictionaryFloat(&'a [u8]),
    Bool(bool),
    String {
        value: &'a [u8],
        node_type: NodeType,
    },
    KvIrEncodedText {
        event: &'a KvIrLogEvent<'a>,
        pair_index: usize,
    },
    UnstructuredArray(UnstructuredArrayRef<'a>),
    Timestamp(TimestampRef<'a>),
    PrevalidatedTimestamp(TimestampRef<'a>),
}

impl BorrowedScalarValue<'_> {
    const fn node_type(self) -> NodeType {
        match self {
            Self::I64(_) => NodeType::Integer,
            Self::DeltaI64(_) => NodeType::DeltaInteger,
            Self::F64(_) => NodeType::Float,
            Self::FormattedFloat { .. } => NodeType::FormattedFloat,
            Self::DictionaryFloat(_) => NodeType::DictionaryFloat,
            Self::Bool(_) => NodeType::Boolean,
            Self::String { node_type, .. } => node_type,
            Self::KvIrEncodedText { .. } => NodeType::ClpString,
            Self::UnstructuredArray(_) => NodeType::UnstructuredArray,
            Self::Timestamp(_) | Self::PrevalidatedTimestamp(_) => NodeType::Timestamp,
        }
    }
}

#[derive(Debug)]
struct PlannedValue {
    node_id: u32,
    value: EncodedValue,
}

#[derive(Debug)]
enum EncodedValue {
    I64(i64),
    DeltaI64 {
        value: i64,
        delta: i64,
    },
    F64(f64),
    FormattedFloat {
        value: f64,
        descriptor: u16,
    },
    DictionaryFloat(u64),
    Bool(bool),
    VarString(u64),
    ClpString {
        node_type: NodeType,
        log_type_id: u64,
        encoded_variable_offset: u64,
        variables: SmallVec<[i64; 4]>,
    },
    Timestamp {
        value: i64,
        delta: i64,
        pattern_id: u64,
    },
}

impl EncodedValue {
    const fn node_type(&self) -> NodeType {
        match self {
            Self::I64(_) => NodeType::Integer,
            Self::DeltaI64 { .. } => NodeType::DeltaInteger,
            Self::F64(_) => NodeType::Float,
            Self::FormattedFloat { .. } => NodeType::FormattedFloat,
            Self::DictionaryFloat(_) => NodeType::DictionaryFloat,
            Self::Bool(_) => NodeType::Boolean,
            Self::VarString(_) => NodeType::VarString,
            Self::ClpString { node_type, .. } => *node_type,
            Self::Timestamp { .. } => NodeType::Timestamp,
        }
    }

    fn appended_size(&self) -> Result<u64, AppendError> {
        match self {
            Self::I64(_)
            | Self::DeltaI64 { .. }
            | Self::F64(_)
            | Self::DictionaryFloat(_)
            | Self::VarString(_) => Ok(8),
            Self::FormattedFloat { .. } => Ok(10),
            Self::Timestamp { .. } => Ok(16),
            Self::Bool(_) => Ok(1),
            Self::ClpString { variables, .. } => usize_u64_append(variables.len())?
                .checked_mul(8)
                .and_then(|size| size.checked_add(8))
                .ok_or(AppendError::SizeOverflow),
        }
    }

    fn new_column_size(&self) -> Result<u64, AppendError> {
        let appended = self.appended_size()?;
        if matches!(self, Self::ClpString { .. }) {
            appended.checked_add(8).ok_or(AppendError::SizeOverflow)
        } else {
            Ok(appended)
        }
    }
}

#[derive(Debug)]
struct RecordPlan {
    nodes: Vec<SchemaNodeRecord>,
    node_path_cache: Option<Vec<u32>>,
    record_layout_cache: Option<RecordLayoutCache>,
    variable_dictionary: DictionaryPlan,
    log_type_dictionary: DictionaryPlan,
    array_dictionary: DictionaryPlan,
    timestamp_dictionary: TimestampPlan,
    table: TablePlan,
    cached_layout_matched: bool,
    resulting_resident_bytes: u64,
    resulting_encoded_data_size: u64,
}

#[derive(Debug)]
struct RecordLayoutCache {
    table_index: usize,
    entry_count: usize,
    ordered_entry_count: usize,
    value_count: usize,
    /// Empty when traversal order already is physical column order.
    sorted_to_traversal: SortedValuePositions,
}

impl RecordPlan {
    #[allow(clippy::too_many_arguments)]
    fn build(
        archive: &PrimitiveArchive,
        record: RecordRef<'_>,
        limits: WriterLimits,
        record_log_order: bool,
        record_plan_scratch: RecordPlanScratch,
        timestamp_scratch: &mut String,
        array_validation_scratch: &mut ArrayValidationScratch,
        retained_float_scratch: &mut RetainedFloatScratch,
    ) -> Result<Self, AppendError> {
        check_append_limit(
            AppendResource::Records,
            archive
                .record_count
                .checked_add(1)
                .ok_or(AppendError::SizeOverflow)?,
            limits.max_records(),
        )?;
        check_append_limit(AppendResource::NestingDepth, 1, limits.max_nesting_depth())?;

        let mut builder =
            RecordPlanBuilder::new(archive, limits, record_plan_scratch, retained_float_scratch)?;
        if record_log_order {
            let log_event_index = validate_log_event_index(archive.record_count)?;
            let metadata_root = builder.resolve_node(None, NodeType::Metadata, b"")?;
            let log_event_index_node = builder.resolve_node(
                Some(metadata_root),
                NodeType::DeltaInteger,
                LOG_EVENT_IDX_KEY,
            )?;
            builder.push_scalar(
                log_event_index_node,
                BorrowedScalarValue::DeltaI64(log_event_index),
            )?;
        }
        let root = builder.resolve_node(None, NodeType::Object, b"")?;
        if record.fields().is_empty() {
            builder.push_entry(root)?;
        } else {
            builder.push_fields(record.fields(), root, 1)?;
            builder.walk()?;
        }
        builder.finish(timestamp_scratch, array_validation_scratch)
    }

    #[allow(clippy::too_many_arguments)]
    fn build_events<'record, I, E>(
        archive: &PrimitiveArchive,
        events: I,
        limits: WriterLimits,
        record_log_order: bool,
        record_plan_scratch: RecordPlanScratch,
        timestamp_scratch: &mut String,
        array_validation_scratch: &mut ArrayValidationScratch,
        retained_float_scratch: &mut RetainedFloatScratch,
    ) -> Result<Self, RecordEventAppendError<E>>
    where
        I: IntoIterator<Item = Result<RecordEventRef<'record>, E>>, {
        check_append_limit(
            AppendResource::Records,
            archive
                .record_count
                .checked_add(1)
                .ok_or(AppendError::SizeOverflow)?,
            limits.max_records(),
        )?;
        check_append_limit(AppendResource::NestingDepth, 1, limits.max_nesting_depth())?;

        let mut builder =
            RecordPlanBuilder::new(archive, limits, record_plan_scratch, retained_float_scratch)?;
        if record_log_order {
            let log_event_index = validate_log_event_index(archive.record_count)?;
            let metadata_root = builder.resolve_node(None, NodeType::Metadata, b"")?;
            let log_event_index_node = builder.resolve_node(
                Some(metadata_root),
                NodeType::DeltaInteger,
                LOG_EVENT_IDX_KEY,
            )?;
            builder.push_scalar(
                log_event_index_node,
                BorrowedScalarValue::DeltaI64(log_event_index),
            )?;
        }
        let root = builder.resolve_node(None, NodeType::Object, b"")?;
        let mut traversal = EventTraversal::new(&mut builder, root, limits)?;
        traversal.consume(events)?;
        traversal.finish()?;
        builder
            .finish(timestamp_scratch, array_validation_scratch)
            .map_err(RecordEventAppendError::Append)
    }

    #[allow(clippy::too_many_arguments)]
    fn build_replayable_events<'record, S>(
        archive: &PrimitiveArchive,
        source: S,
        limits: WriterLimits,
        record_log_order: bool,
        record_plan_scratch: RecordPlanScratch,
        timestamp_scratch: &mut String,
        array_validation_scratch: &mut ArrayValidationScratch,
        retained_float_scratch: &mut RetainedFloatScratch,
    ) -> Result<Self, RecordEventAppendError<S::Error>>
    where
        S: ReplayableRecordEventSource<'record>, {
        check_append_limit(
            AppendResource::Records,
            archive
                .record_count
                .checked_add(1)
                .ok_or(AppendError::SizeOverflow)?,
            limits.max_records(),
        )?;
        check_append_limit(AppendResource::NestingDepth, 1, limits.max_nesting_depth())?;

        let record_plan_scratch =
            if archive.last_record_layout_cache_hit && source.supports_cached_layout_proof() {
                if let Ok(Some(plan)) = Self::build_replayable_attempt(
                    archive,
                    source.clone(),
                    limits,
                    record_log_order,
                    record_plan_scratch,
                    timestamp_scratch,
                    array_validation_scratch,
                    retained_float_scratch,
                    EventFieldValidation::CachedLayoutProof,
                    true,
                ) {
                    return Ok(plan);
                }
                RecordPlanScratch::default()
            } else {
                record_plan_scratch
            };

        Self::build_replayable_attempt(
            archive,
            source,
            limits,
            record_log_order,
            record_plan_scratch,
            timestamp_scratch,
            array_validation_scratch,
            retained_float_scratch,
            EventFieldValidation::Full,
            false,
        )
        .map(|plan| plan.expect("full record traversal always produces a plan"))
    }

    #[allow(clippy::too_many_arguments)]
    fn build_replayable_attempt<'record, S>(
        archive: &PrimitiveArchive,
        source: S,
        limits: WriterLimits,
        record_log_order: bool,
        record_plan_scratch: RecordPlanScratch,
        timestamp_scratch: &mut String,
        array_validation_scratch: &mut ArrayValidationScratch,
        retained_float_scratch: &mut RetainedFloatScratch,
        field_validation: EventFieldValidation,
        require_cached_layout: bool,
    ) -> Result<Option<Self>, RecordEventAppendError<S::Error>>
    where
        S: ReplayableRecordEventSource<'record>, {
        debug_assert_eq!(
            require_cached_layout,
            matches!(field_validation, EventFieldValidation::CachedLayoutProof)
        );
        let mut builder =
            RecordPlanBuilder::new(archive, limits, record_plan_scratch, retained_float_scratch)?;
        if record_log_order {
            let log_event_index = validate_log_event_index(archive.record_count)?;
            let metadata_root = builder.resolve_node(None, NodeType::Metadata, b"")?;
            let log_event_index_node = builder.resolve_node(
                Some(metadata_root),
                NodeType::DeltaInteger,
                LOG_EVENT_IDX_KEY,
            )?;
            builder.push_scalar(
                log_event_index_node,
                BorrowedScalarValue::DeltaI64(log_event_index),
            )?;
        }
        let root = builder.resolve_node(None, NodeType::Object, b"")?;
        let mut traversal =
            EventTraversal::new_with_validation(&mut builder, root, limits, field_validation)?;
        source.consume(&mut traversal)?;
        traversal.finish()?;
        if require_cached_layout && !builder.cached_record_layout_matches() {
            return Ok(None);
        }
        builder
            .finish(timestamp_scratch, array_validation_scratch)
            .map(Some)
            .map_err(RecordEventAppendError::Append)
    }
}

#[derive(Clone, Copy)]
struct PendingField<'a> {
    field: FieldRef<'a>,
    parent: u32,
    parent_depth: u64,
}

#[derive(Clone, Copy)]
struct EventObjectFrame {
    node_id: u32,
    depth: u64,
    scope_id: usize,
    seen_start: usize,
    field_count: usize,
    has_fields: bool,
    fields_are_indexed: bool,
}

#[derive(Clone, Copy)]
struct SeenEventField<'a> {
    key: &'a [u8],
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum EventFieldValidation {
    Full,
    CachedLayoutProof,
}

struct EventTraversal<'builder, 'archive, 'record, 'scratch> {
    builder: &'builder mut RecordPlanBuilder<'archive, 'record, 'scratch>,
    limits: WriterLimits,
    root: u32,
    kv_ir_namespace_selected: bool,
    frames: EventFrames,
    seen_fields: SeenEventFields<'record>,
    field_indexes: HashMap<(usize, &'record [u8]), usize>,
    next_scope_id: usize,
    event_index: usize,
    field_validation: EventFieldValidation,
}

impl<'builder, 'archive, 'record, 'scratch> EventTraversal<'builder, 'archive, 'record, 'scratch> {
    fn new(
        builder: &'builder mut RecordPlanBuilder<'archive, 'record, 'scratch>,
        root: u32,
        limits: WriterLimits,
    ) -> Result<Self, AppendError> {
        Self::new_with_validation(builder, root, limits, EventFieldValidation::Full)
    }

    fn new_with_validation(
        builder: &'builder mut RecordPlanBuilder<'archive, 'record, 'scratch>,
        root: u32,
        limits: WriterLimits,
        field_validation: EventFieldValidation,
    ) -> Result<Self, AppendError> {
        let mut frames = EventFrames::new();
        frames
            .try_reserve_exact(COMMON_OBJECT_NESTING_CAPACITY)
            .map_err(|_| {
                append_allocation(AppendResource::NestingDepth, COMMON_OBJECT_NESTING_CAPACITY)
            })?;
        frames.push(EventObjectFrame {
            node_id: root,
            depth: 1,
            scope_id: 0,
            seen_start: 0,
            field_count: 0,
            has_fields: false,
            fields_are_indexed: false,
        });
        let mut seen_fields = SeenEventFields::new();
        if matches!(field_validation, EventFieldValidation::Full) {
            seen_fields
                .try_reserve_exact(COMMON_RECORD_FIELD_CAPACITY)
                .map_err(|_| {
                    append_allocation(AppendResource::Columns, COMMON_RECORD_FIELD_CAPACITY)
                })?;
        }
        Ok(Self {
            builder,
            limits,
            root,
            kv_ir_namespace_selected: false,
            frames,
            seen_fields,
            field_indexes: HashMap::new(),
            next_scope_id: 1,
            event_index: 0,
            field_validation,
        })
    }

    fn consume<I, E>(&mut self, events: I) -> Result<(), RecordEventAppendError<E>>
    where
        I: IntoIterator<Item = Result<RecordEventRef<'record>, E>>, {
        for event in events {
            let event = event.map_err(|source| RecordEventAppendError::Source {
                event_index: self.event_index,
                source,
            })?;
            self.process(event)?;
        }
        Ok(())
    }

    fn process(&mut self, event: RecordEventRef<'record>) -> Result<(), AppendError> {
        match event {
            RecordEventRef::Value(field) => RecordEventConsumer::value(self, field),
            RecordEventRef::ObjectStart(key) => RecordEventConsumer::object_start(self, key),
            RecordEventRef::ObjectEnd => RecordEventConsumer::object_end(self),
        }
    }

    fn advance_event_index(&mut self) -> Result<(), AppendError> {
        self.event_index = self
            .event_index
            .checked_add(1)
            .ok_or(AppendError::SizeOverflow)?;
        Ok(())
    }

    fn register_field(&mut self, key: &'record [u8]) -> Result<(), AppendError> {
        if matches!(
            self.field_validation,
            EventFieldValidation::CachedLayoutProof
        ) {
            self.frames
                .last_mut()
                .expect("the implicit root frame remains open")
                .has_fields = true;
            return Ok(());
        }
        let frame = *self
            .frames
            .last()
            .expect("the implicit root frame remains open");
        let skip_duplicate_check = self
            .builder
            .next_cached_key_matches(Some(frame.node_id), key);
        register_event_field(
            &mut self.frames,
            &mut self.seen_fields,
            &mut self.field_indexes,
            key,
            skip_duplicate_check,
        )
    }

    fn value(&mut self, field: FieldRef<'record>) -> Result<(), AppendError> {
        let frame = *self
            .frames
            .last()
            .expect("the implicit root frame remains open");
        self.register_field(field.key())?;
        self.builder.visit_field(PendingField {
            field,
            parent: frame.node_id,
            parent_depth: frame.depth,
        })?;
        self.builder.walk()
    }

    fn kv_ir_encoded_text(
        &mut self,
        key: &'record [u8],
        event: &'record KvIrLogEvent<'record>,
        pair_index: usize,
    ) -> Result<(), AppendError> {
        let frame = *self
            .frames
            .last()
            .expect("the implicit root frame remains open");
        self.register_field(key)?;
        let node_id = self
            .builder
            .resolve_node(Some(frame.node_id), NodeType::ClpString, key)?;
        self.builder.push_entry(node_id)?;
        self.builder.push_planned_scalar(
            node_id,
            BorrowedScalarValue::KvIrEncodedText { event, pair_index },
        )
    }

    fn kv_ir_namespace(&mut self, namespace: KvIrNamespace) -> Result<(), AppendError> {
        if 1 != self.frames.len() {
            return Err(AppendError::InvalidRecordEvents {
                event_index: self.event_index,
                reason: RecordEventError::KvIrNamespaceInsideObject {
                    depth: self.frames.len() - 1,
                },
            });
        }

        let previous = self
            .frames
            .pop()
            .expect("the prior namespace root frame exists");
        if matches!(self.field_validation, EventFieldValidation::Full) {
            if previous.fields_are_indexed {
                for field in &self.seen_fields[previous.seen_start..] {
                    let removed = self.field_indexes.remove(&(previous.scope_id, field.key));
                    debug_assert!(removed.is_some());
                }
            }
            self.seen_fields.truncate(previous.seen_start);
        }
        if self.kv_ir_namespace_selected && !previous.has_fields {
            self.builder.push_entry(previous.node_id)?;
        }

        let key = match namespace {
            KvIrNamespace::AutoGenerated => b"@".as_slice(),
            KvIrNamespace::UserGenerated => b"".as_slice(),
        };
        let root = self.builder.resolve_node(None, NodeType::Object, key)?;
        let scope_id = self.next_scope_id;
        self.next_scope_id = self
            .next_scope_id
            .checked_add(1)
            .ok_or(AppendError::SizeOverflow)?;
        self.root = root;
        self.frames.push(EventObjectFrame {
            node_id: root,
            depth: 1,
            scope_id,
            seen_start: self.seen_fields.len(),
            field_count: 0,
            has_fields: false,
            fields_are_indexed: false,
        });
        self.kv_ir_namespace_selected = true;
        Ok(())
    }

    fn object_start(&mut self, key: &'record [u8]) -> Result<(), AppendError> {
        let parent = *self
            .frames
            .last()
            .expect("the implicit root frame remains open");
        self.register_field(key)?;
        let depth = parent
            .depth
            .checked_add(1)
            .ok_or(AppendError::SizeOverflow)?;
        check_append_limit(
            AppendResource::NestingDepth,
            depth,
            self.limits.max_nesting_depth(),
        )?;
        let node_id = self
            .builder
            .resolve_node(Some(parent.node_id), NodeType::Object, key)?;
        self.frames
            .try_reserve(1)
            .map_err(|_| append_allocation(AppendResource::NestingDepth, 1))?;
        let scope_id = self.next_scope_id;
        self.next_scope_id = self
            .next_scope_id
            .checked_add(1)
            .ok_or(AppendError::SizeOverflow)?;
        self.frames.push(EventObjectFrame {
            node_id,
            depth,
            scope_id,
            seen_start: self.seen_fields.len(),
            field_count: 0,
            has_fields: false,
            fields_are_indexed: false,
        });
        Ok(())
    }

    fn object_end(&mut self) -> Result<(), AppendError> {
        if 1 == self.frames.len() {
            return Err(AppendError::InvalidRecordEvents {
                event_index: self.event_index,
                reason: RecordEventError::UnexpectedObjectEnd,
            });
        }
        let frame = self.frames.pop().expect("a nested object frame exists");
        if matches!(self.field_validation, EventFieldValidation::Full) && frame.fields_are_indexed {
            for field in &self.seen_fields[frame.seen_start..] {
                let removed = self.field_indexes.remove(&(frame.scope_id, field.key));
                debug_assert!(removed.is_some());
            }
        }
        if matches!(self.field_validation, EventFieldValidation::Full) {
            self.seen_fields.truncate(frame.seen_start);
        }
        if !frame.has_fields {
            self.builder.push_entry(frame.node_id)?;
        }
        Ok(())
    }

    fn finish(mut self) -> Result<(), AppendError> {
        if 1 != self.frames.len() {
            return Err(AppendError::InvalidRecordEvents {
                event_index: self.event_index,
                reason: RecordEventError::UnclosedObjects {
                    count: self.frames.len() - 1,
                },
            });
        }
        let root_frame = self.frames.pop().expect("the implicit root frame exists");
        if !root_frame.has_fields {
            self.builder.push_entry(self.root)?;
        }
        Ok(())
    }
}

impl<'record> RecordEventConsumer<'record> for EventTraversal<'_, '_, 'record, '_> {
    fn value(&mut self, field: FieldRef<'record>) -> Result<(), AppendError> {
        EventTraversal::value(self, field)?;
        self.advance_event_index()
    }

    fn kv_ir_namespace(&mut self, namespace: KvIrNamespace) -> Result<(), AppendError> {
        EventTraversal::kv_ir_namespace(self, namespace)?;
        self.advance_event_index()
    }

    fn kv_ir_encoded_text(
        &mut self,
        key: &'record [u8],
        event: &'record KvIrLogEvent<'record>,
        pair_index: usize,
    ) -> Result<(), AppendError> {
        EventTraversal::kv_ir_encoded_text(self, key, event, pair_index)?;
        self.advance_event_index()
    }

    fn object_start(&mut self, key: &'record [u8]) -> Result<(), AppendError> {
        EventTraversal::object_start(self, key)?;
        self.advance_event_index()
    }

    fn object_end(&mut self) -> Result<(), AppendError> {
        EventTraversal::object_end(self)?;
        self.advance_event_index()
    }
}

fn register_event_field<'record>(
    frames: &mut [EventObjectFrame],
    seen_fields: &mut SeenEventFields<'record>,
    field_indexes: &mut HashMap<(usize, &'record [u8]), usize>,
    key: &'record [u8],
    skip_duplicate_check: bool,
) -> Result<(), AppendError> {
    let frame = *frames.last().expect("the implicit root frame remains open");
    if !skip_duplicate_check {
        if !frame.fields_are_indexed && frame.field_count < LINEAR_DUPLICATE_FIELD_LIMIT {
            if let Some(previous_index) = seen_fields[frame.seen_start..]
                .iter()
                .position(|field| field.key == key)
            {
                return Err(AppendError::DuplicateField {
                    object_depth: frame.depth,
                    previous_index,
                    field_index: frame.field_count,
                });
            }
        } else {
            if !frame.fields_are_indexed {
                let additional = frame
                    .field_count
                    .checked_add(1)
                    .ok_or(AppendError::SizeOverflow)?;
                field_indexes
                    .try_reserve(additional)
                    .map_err(|_| append_allocation(AppendResource::Columns, additional))?;
                for (field_index, field) in seen_fields[frame.seen_start..].iter().enumerate() {
                    let previous = field_indexes.insert((frame.scope_id, field.key), field_index);
                    debug_assert!(previous.is_none());
                }
                frames
                    .last_mut()
                    .expect("the implicit root frame remains open")
                    .fields_are_indexed = true;
            }
            if let Some(previous_index) =
                field_indexes.insert((frame.scope_id, key), frame.field_count)
            {
                return Err(AppendError::DuplicateField {
                    object_depth: frame.depth,
                    previous_index,
                    field_index: frame.field_count,
                });
            }
        }
    }
    seen_fields
        .try_reserve(1)
        .map_err(|_| append_allocation(AppendResource::Columns, 1))?;
    seen_fields.push(SeenEventField { key });
    let frame = frames
        .last_mut()
        .expect("the implicit root frame remains open");
    frame.field_count = frame
        .field_count
        .checked_add(1)
        .ok_or(AppendError::SizeOverflow)?;
    frame.has_fields = true;
    Ok(())
}

struct RecordPlanBuilder<'archive, 'record, 'scratch> {
    archive: &'archive PrimitiveArchive,
    retained_float_scratch: &'scratch mut RetainedFloatScratch,
    limits: WriterLimits,
    nodes: Vec<SchemaNodeRecord>,
    staged_node_ids: HashMap<(Option<u32>, NodeType, &'record [u8]), u32>,
    entries: RecordEntries,
    encoded_values: PlannedRecordValues,
    unordered_entries: Vec<SchemaEntry>,
    values: BorrowedRecordValues<'record>,
    unordered_values: BorrowedRecordValues<'record>,
    pending_fields: PendingFields<'record>,
    node_path_position: usize,
    replacement_node_path: Option<Vec<u32>>,
    added_key_bytes: u64,
}

impl<'archive, 'record, 'scratch> RecordPlanBuilder<'archive, 'record, 'scratch> {
    fn new(
        archive: &'archive PrimitiveArchive,
        limits: WriterLimits,
        record_plan_scratch: RecordPlanScratch,
        retained_float_scratch: &'scratch mut RetainedFloatScratch,
    ) -> Result<Self, AppendError> {
        let RecordPlanScratch {
            mut entries,
            values: encoded_values,
        } = record_plan_scratch;
        debug_assert_eq!(0, entries.len());
        debug_assert_eq!(0, encoded_values.len());
        entries
            .try_reserve_exact(COMMON_RECORD_FIELD_CAPACITY)
            .map_err(|_| {
                append_allocation(AppendResource::Columns, COMMON_RECORD_FIELD_CAPACITY)
            })?;
        let mut values = BorrowedRecordValues::new();
        values
            .try_reserve_exact(COMMON_RECORD_FIELD_CAPACITY)
            .map_err(|_| {
                append_allocation(AppendResource::Columns, COMMON_RECORD_FIELD_CAPACITY)
            })?;
        Ok(Self {
            archive,
            retained_float_scratch,
            limits,
            nodes: Vec::new(),
            staged_node_ids: HashMap::new(),
            entries,
            encoded_values,
            unordered_entries: Vec::new(),
            values,
            unordered_values: BorrowedRecordValues::new(),
            pending_fields: PendingFields::new(),
            node_path_position: 0,
            replacement_node_path: None,
            added_key_bytes: 0,
        })
    }

    fn resolve_node(
        &mut self,
        parent: Option<u32>,
        node_type: NodeType,
        key: &'record [u8],
    ) -> Result<u32, AppendError> {
        let path_position = self.node_path_position;
        self.node_path_position = self
            .node_path_position
            .checked_add(1)
            .ok_or(AppendError::SizeOverflow)?;
        if self.replacement_node_path.is_none()
            && let Some(&cached_id) = self.archive.node_path_cache.get(path_position)
            && self.cached_node_matches(cached_id, parent, node_type, key)
        {
            return Ok(cached_id);
        }
        self.start_replacement_node_path(path_position)?;
        let id = self.intern_node(parent, node_type, key)?;
        self.push_replacement_node(id)?;
        Ok(id)
    }

    fn resolve_unordered_node(
        &mut self,
        parent: u32,
        node_type: NodeType,
        key: &'record [u8],
    ) -> Result<u32, AppendError> {
        self.intern_node(Some(parent), node_type, key)
    }

    fn intern_node(
        &mut self,
        parent: Option<u32>,
        node_type: NodeType,
        key: &'record [u8],
    ) -> Result<u32, AppendError> {
        if let Some(id) = self.archive.tree.find(parent, node_type, key) {
            return Ok(id);
        }
        if let Some(id) = self.staged_node_ids.get(&(parent, node_type, key)) {
            return Ok(*id);
        }
        let proposed_id = usize_u64_append(
            self.archive
                .tree
                .nodes
                .len()
                .checked_add(self.nodes.len())
                .ok_or(AppendError::SizeOverflow)?,
        )?;
        if proposed_id > MAX_SCHEMA_NODE_ID {
            return Err(AppendError::FormatDomainExceeded {
                domain: AppendDomain::SchemaNodeId,
                actual: proposed_id,
                limit: MAX_SCHEMA_NODE_ID,
            });
        }
        let resulting_nodes = proposed_id
            .checked_add(1)
            .ok_or(AppendError::SizeOverflow)?;
        check_append_limit(
            AppendResource::SchemaNodes,
            resulting_nodes,
            self.limits.max_schema_nodes(),
        )?;

        let added_key_bytes = self
            .added_key_bytes
            .checked_add(usize_u64_append(key.len())?)
            .ok_or(AppendError::SizeOverflow)?;
        let staged_resident_bytes = self
            .archive
            .resident_bytes
            .checked_add(added_key_bytes)
            .ok_or(AppendError::SizeOverflow)?;
        check_append_limit(
            AppendResource::ResidentBytes,
            staged_resident_bytes,
            self.limits.max_resident_bytes(),
        )?;
        let mut owned_key = Vec::new();
        owned_key
            .try_reserve_exact(key.len())
            .map_err(|_| append_allocation(AppendResource::ResidentBytes, key.len()))?;
        owned_key.extend_from_slice(key);
        self.nodes
            .try_reserve(1)
            .map_err(|_| append_allocation(AppendResource::SchemaNodes, 1))?;
        self.staged_node_ids
            .try_reserve(1)
            .map_err(|_| append_allocation(AppendResource::SchemaNodes, 1))?;
        self.added_key_bytes = added_key_bytes;
        self.nodes.push(SchemaNodeRecord {
            parent,
            key: owned_key,
            node_type,
        });
        let id = u32::try_from(proposed_id).map_err(|_| AppendError::SizeOverflow)?;
        let previous = self.staged_node_ids.insert((parent, node_type, key), id);
        debug_assert!(previous.is_none());
        Ok(id)
    }

    fn next_cached_key_matches(&self, parent: Option<u32>, key: &[u8]) -> bool {
        self.replacement_node_path.is_none()
            && self
                .archive
                .node_path_cache
                .get(self.node_path_position)
                .and_then(|id| usize::try_from(*id).ok())
                .and_then(|index| self.archive.tree.nodes.get(index))
                .is_some_and(|node| node.parent == parent && node.key == key)
    }

    fn cached_node_matches(
        &self,
        cached_id: u32,
        parent: Option<u32>,
        node_type: NodeType,
        key: &[u8],
    ) -> bool {
        usize::try_from(cached_id)
            .ok()
            .and_then(|index| self.archive.tree.nodes.get(index))
            .is_some_and(|node| {
                node.parent == parent && node.node_type == node_type && node.key == key
            })
    }

    fn cached_record_layout_matches(&self) -> bool {
        let Some(cache) = self.archive.record_layout_cache.as_ref() else {
            return false;
        };
        if !self.unordered_entries.is_empty()
            || self.replacement_node_path.is_some()
            || self.node_path_position != self.archive.node_path_cache.len()
            || self.archive.tables.last_table_index != Some(cache.table_index)
            || self.entries.len() != cache.entry_count
            || self.values.len() != cache.value_count
        {
            return false;
        }
        let Some(table) = self.archive.tables.tables.get(cache.table_index) else {
            return false;
        };
        if table.entries.len() != cache.entry_count
            || table.ordered_entry_count != cache.ordered_entry_count
            || table.columns.len() != cache.value_count
        {
            return false;
        }
        if !cache.sorted_to_traversal.is_empty()
            && (cache.sorted_to_traversal.len() != cache.value_count
                || cache
                    .sorted_to_traversal
                    .iter()
                    .any(|&position| position >= cache.value_count))
        {
            return false;
        }
        debug_assert!(
            table
                .columns
                .iter()
                .enumerate()
                .all(|(column_index, column)| {
                    let traversal_index = cache
                        .sorted_to_traversal
                        .get(column_index)
                        .copied()
                        .unwrap_or(column_index);
                    self.values.get(traversal_index).is_some_and(|value| {
                        column.node_id == value.node_id
                            && column.node_type == value.value.node_type()
                    })
                })
        );
        true
    }

    fn start_replacement_node_path(&mut self, path_position: usize) -> Result<(), AppendError> {
        if self.replacement_node_path.is_some() {
            return Ok(());
        }
        let capacity = self
            .archive
            .node_path_cache
            .len()
            .max(self.node_path_position);
        let mut replacement = Vec::new();
        replacement
            .try_reserve_exact(capacity)
            .map_err(|_| append_allocation(AppendResource::SchemaNodes, capacity))?;
        replacement.extend_from_slice(&self.archive.node_path_cache[..path_position]);
        self.replacement_node_path = Some(replacement);
        Ok(())
    }

    fn push_replacement_node(&mut self, node_id: u32) -> Result<(), AppendError> {
        let replacement = self
            .replacement_node_path
            .as_mut()
            .expect("a cache miss starts a replacement node path");
        replacement
            .try_reserve(1)
            .map_err(|_| append_allocation(AppendResource::SchemaNodes, 1))?;
        replacement.push(node_id);
        Ok(())
    }

    fn push_fields(
        &mut self,
        fields: &'record [FieldRef<'record>],
        parent: u32,
        parent_depth: u64,
    ) -> Result<(), AppendError> {
        validate_unique_fields(fields, parent_depth)?;
        self.pending_fields
            .try_reserve(fields.len())
            .map_err(|_| append_allocation(AppendResource::NestingDepth, fields.len()))?;
        self.pending_fields
            .extend(fields.iter().rev().copied().map(|field| PendingField {
                field,
                parent,
                parent_depth,
            }));
        Ok(())
    }

    fn walk(&mut self) -> Result<(), AppendError> {
        while let Some(pending) = self.pending_fields.pop() {
            self.visit_field(pending)?;
        }
        Ok(())
    }

    fn visit_field(&mut self, pending: PendingField<'record>) -> Result<(), AppendError> {
        let value = pending.field.value();
        match value {
            ValueRef::Object(fields) => {
                let node_id =
                    self.resolve_node(Some(pending.parent), NodeType::Object, pending.field.key())?;
                if fields.is_empty() {
                    self.push_entry(node_id)?;
                    return Ok(());
                }
                let depth = pending
                    .parent_depth
                    .checked_add(1)
                    .ok_or(AppendError::SizeOverflow)?;
                check_append_limit(
                    AppendResource::NestingDepth,
                    depth,
                    self.limits.max_nesting_depth(),
                )?;
                self.push_fields(fields, node_id, depth)?;
            }
            ValueRef::Array(values) => {
                let depth = pending
                    .parent_depth
                    .checked_add(1)
                    .ok_or(AppendError::SizeOverflow)?;
                check_append_limit(
                    AppendResource::NestingDepth,
                    depth,
                    self.limits.max_nesting_depth(),
                )?;
                let node_id = self.resolve_node(
                    Some(pending.parent),
                    NodeType::StructuredArray,
                    pending.field.key(),
                )?;
                let existing_structured_entries = usize_u64_append(self.unordered_entries.len())?;
                let structured_entry_limit = self.limits.max_structured_array_schema_entries();
                let plan = array::plan_structured_array(
                    values,
                    node_id,
                    depth,
                    StructuredArrayPlanLimits::new(
                        structured_entry_limit.saturating_sub(existing_structured_entries),
                        self.limits.max_nesting_depth(),
                    ),
                    self,
                )
                .map_err(|error| {
                    structured_array_append_error(
                        error,
                        existing_structured_entries,
                        structured_entry_limit,
                    )
                })?;
                self.push_structured_array_plan(plan)?;
            }
            _ => {
                let resolved = self.plan_leaf(pending.parent, pending.field.key(), value, false)?;
                self.push_entry(resolved.node_id)?;
                if let Some(value) = resolved.value {
                    self.push_planned_scalar(resolved.node_id, value)?;
                }
            }
        }
        Ok(())
    }

    fn plan_leaf(
        &mut self,
        parent: u32,
        key: &'record [u8],
        value: ValueRef<'record>,
        unordered: bool,
    ) -> Result<ResolvedStructuredArrayValue<BorrowedScalarValue<'record>>, AppendError> {
        let (node_type, scalar) = match value {
            ValueRef::Null => (NodeType::Null, None),
            ValueRef::I64(value) => (NodeType::Integer, Some(BorrowedScalarValue::I64(value))),
            ValueRef::F64(value) => (NodeType::Float, Some(BorrowedScalarValue::F64(value))),
            ValueRef::RetainedFloat(retained) => {
                match retained_float::classify(retained, self.retained_float_scratch)
                    .map_err(|reason| AppendError::RetainedFloat { reason })?
                {
                    RetainedFloatEncoding::Formatted { value, descriptor } => (
                        NodeType::FormattedFloat,
                        Some(BorrowedScalarValue::FormattedFloat { value, descriptor }),
                    ),
                    RetainedFloatEncoding::Dictionary { source } => (
                        NodeType::DictionaryFloat,
                        Some(BorrowedScalarValue::DictionaryFloat(source)),
                    ),
                }
            }
            ValueRef::Bool(value) => (NodeType::Boolean, Some(BorrowedScalarValue::Bool(value))),
            ValueRef::String(value) => {
                let node_type = if clp::node_is_clp_string(value) {
                    NodeType::ClpString
                } else {
                    NodeType::VarString
                };
                (
                    node_type,
                    Some(BorrowedScalarValue::String { value, node_type }),
                )
            }
            ValueRef::UnstructuredArray(value) => (
                NodeType::UnstructuredArray,
                Some(BorrowedScalarValue::UnstructuredArray(value)),
            ),
            ValueRef::Timestamp(value) => (
                NodeType::Timestamp,
                Some(BorrowedScalarValue::Timestamp(value)),
            ),
            ValueRef::PrevalidatedTimestamp(value) => (
                NodeType::Timestamp,
                Some(BorrowedScalarValue::PrevalidatedTimestamp(
                    value.into_inner(),
                )),
            ),
            ValueRef::Object(_) | ValueRef::Array(_) => {
                unreachable!("structured containers are planned before their leaves")
            }
        };
        let node_id = if unordered {
            self.resolve_unordered_node(parent, node_type, key)?
        } else {
            self.resolve_node(Some(parent), node_type, key)?
        };
        if matches!(scalar, Some(BorrowedScalarValue::F64(value)) if !value.is_finite()) {
            return Err(AppendError::NonFiniteFloat { node_id });
        }
        Ok(scalar.map_or_else(
            || ResolvedStructuredArrayValue::structural(node_id),
            |value| ResolvedStructuredArrayValue::physical(node_id, value),
        ))
    }

    fn push_structured_array_plan(
        &mut self,
        plan: StructuredArrayPlan<BorrowedScalarValue<'record>>,
    ) -> Result<(), AppendError> {
        let (entries, values) = plan.into_parts();
        let resulting_entry_count = usize_u64_append(self.unordered_entries.len())?
            .checked_add(usize_u64_append(entries.len())?)
            .ok_or(AppendError::SizeOverflow)?;
        check_append_limit(
            AppendResource::StructuredArraySchemaEntries,
            resulting_entry_count,
            self.limits.max_structured_array_schema_entries(),
        )?;
        let resulting_value_count = self
            .values
            .len()
            .checked_add(self.unordered_values.len())
            .and_then(|count| count.checked_add(values.len()))
            .ok_or(AppendError::SizeOverflow)?;
        check_append_limit(
            AppendResource::Columns,
            usize_u64_append(resulting_value_count)?,
            self.limits.max_columns(),
        )?;
        self.unordered_entries
            .try_reserve(entries.len())
            .map_err(|_| {
                append_allocation(AppendResource::StructuredArraySchemaEntries, entries.len())
            })?;
        self.unordered_values
            .try_reserve(values.len())
            .map_err(|_| append_allocation(AppendResource::Columns, values.len()))?;
        self.unordered_entries.extend(entries);
        self.unordered_values
            .extend(values.into_iter().map(|value| BorrowedPlannedValue {
                node_id: value.node_id(),
                value: value.value(),
            }));
        Ok(())
    }

    fn push_entry(&mut self, node_id: u32) -> Result<(), AppendError> {
        self.entries
            .try_reserve(1)
            .map_err(|_| append_allocation(AppendResource::Columns, 1))?;
        self.entries.push(node_id);
        Ok(())
    }

    fn push_scalar(
        &mut self,
        node_id: u32,
        value: BorrowedScalarValue<'record>,
    ) -> Result<(), AppendError> {
        self.push_entry(node_id)?;
        self.push_planned_scalar(node_id, value)
    }

    fn push_planned_scalar(
        &mut self,
        node_id: u32,
        value: BorrowedScalarValue<'record>,
    ) -> Result<(), AppendError> {
        let proposed_value_count = usize_u64_append(
            self.values
                .len()
                .checked_add(self.unordered_values.len())
                .ok_or(AppendError::SizeOverflow)?
                .checked_add(1)
                .ok_or(AppendError::SizeOverflow)?,
        )?;
        check_append_limit(
            AppendResource::Columns,
            proposed_value_count,
            self.limits.max_columns(),
        )?;
        self.values
            .try_reserve(1)
            .map_err(|_| append_allocation(AppendResource::Columns, 1))?;
        self.values.push(BorrowedPlannedValue { node_id, value });
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    fn finish(
        mut self,
        timestamp_scratch: &mut String,
        array_validation_scratch: &mut ArrayValidationScratch,
    ) -> Result<RecordPlan, AppendError> {
        let entry_count = usize_u64_append(self.entries.len())?
            .checked_add(usize_u64_append(self.unordered_entries.len())?)
            .ok_or(AppendError::SizeOverflow)?;
        if entry_count > u64::from(u32::MAX) {
            return Err(AppendError::FormatDomainExceeded {
                domain: AppendDomain::SchemaEntries,
                actual: entry_count,
                limit: u64::from(u32::MAX),
            });
        }
        let cached_layout_matches = self.cached_record_layout_matches();
        let ordered_entry_count = self.entries.len();
        let mut sorted_to_traversal = SortedValuePositions::new();
        let table_index = if cached_layout_matches {
            Some(
                self.archive
                    .record_layout_cache
                    .as_ref()
                    .expect("a matching record layout cache exists")
                    .table_index,
            )
        } else {
            self.prepare_uncached_record_layout(ordered_entry_count, &mut sorted_to_traversal)?
        };
        let is_new_schema = table_index.is_none();
        let value_count = self.values.len();
        let added_schema_bytes = if is_new_schema {
            entry_count
                .checked_mul(4)
                .ok_or(AppendError::SizeOverflow)?
        } else {
            0
        };
        let value_order = if cached_layout_matches {
            &self
                .archive
                .record_layout_cache
                .as_ref()
                .expect("a matching record layout cache exists")
                .sorted_to_traversal
        } else {
            &sorted_to_traversal
        };
        let mut dictionaries = DictionaryPlans::new(self.archive)?;
        let values = encode_values(
            &self.values,
            value_order,
            &mut dictionaries,
            table_index.map(|index| &self.archive.tables.tables[index]),
            self.limits,
            timestamp_scratch,
            array_validation_scratch,
            self.encoded_values,
        )?;
        let table = self.archive.tables.plan_append(
            self.entries,
            ordered_entry_count,
            values,
            table_index,
            cached_layout_matches,
            self.limits,
        )?;
        let sizes = resulting_archive_sizes(
            self.archive,
            self.limits,
            self.added_key_bytes,
            added_schema_bytes,
            &dictionaries,
            &table,
        )?;
        validate_resulting_archive_shape(self.archive, self.limits, is_new_schema, value_count)?;
        let node_path_cache = resulting_node_path_cache(
            self.archive,
            self.replacement_node_path,
            self.node_path_position,
        )?;
        let record_layout_cache = if cached_layout_matches {
            None
        } else {
            Some(RecordLayoutCache {
                table_index: table_index.unwrap_or(self.archive.tables.tables.len()),
                entry_count: usize::try_from(entry_count).map_err(|_| AppendError::SizeOverflow)?,
                ordered_entry_count,
                value_count,
                sorted_to_traversal,
            })
        };
        Ok(RecordPlan {
            nodes: self.nodes,
            node_path_cache,
            record_layout_cache,
            variable_dictionary: dictionaries.variable,
            log_type_dictionary: dictionaries.log_type,
            array_dictionary: dictionaries.array,
            timestamp_dictionary: dictionaries.timestamp,
            table,
            cached_layout_matched: cached_layout_matches,
            resulting_resident_bytes: sizes.resident_bytes,
            resulting_encoded_data_size: sizes.encoded_data_size,
        })
    }

    #[cold]
    #[inline(never)]
    fn prepare_uncached_record_layout(
        &mut self,
        ordered_entry_count: usize,
        sorted_to_traversal: &mut SortedValuePositions,
    ) -> Result<Option<usize>, AppendError> {
        self.entries.sort_unstable();
        *sorted_to_traversal = sorted_value_positions(&self.values)?;
        let ordered_value_count = self.values.len();
        if !self.unordered_values.is_empty() {
            self.values
                .try_reserve(self.unordered_values.len())
                .map_err(|_| {
                    append_allocation(AppendResource::Columns, self.unordered_values.len())
                })?;
            if !sorted_to_traversal.is_empty() {
                sorted_to_traversal
                    .try_reserve(self.unordered_values.len())
                    .map_err(|_| {
                        append_allocation(AppendResource::Columns, self.unordered_values.len())
                    })?;
                sorted_to_traversal.extend(
                    ordered_value_count
                        ..ordered_value_count
                            .checked_add(self.unordered_values.len())
                            .ok_or(AppendError::SizeOverflow)?,
                );
            }
            self.values.append(&mut self.unordered_values);
        }
        if !self.unordered_entries.is_empty() {
            self.entries
                .try_reserve(self.unordered_entries.len())
                .map_err(|_| {
                    append_allocation(
                        AppendResource::StructuredArraySchemaEntries,
                        self.unordered_entries.len(),
                    )
                })?;
            self.entries.extend(
                self.unordered_entries
                    .drain(..)
                    .map(|entry| u32::from_le_bytes(entry.wire_value().to_le_bytes())),
            );
        }
        Ok(self
            .archive
            .tables
            .find_schema(&self.entries, ordered_entry_count))
    }
}

impl<'record> StructuredArrayNodeResolver<'record> for RecordPlanBuilder<'_, 'record, '_> {
    type Error = AppendError;
    type Value = BorrowedScalarValue<'record>;

    fn resolve_container(
        &mut self,
        parent: u32,
        node_type: NodeType,
        key: &'record [u8],
    ) -> Result<u32, Self::Error> {
        self.resolve_unordered_node(parent, node_type, key)
    }

    fn resolve_leaf(
        &mut self,
        parent: u32,
        key: &'record [u8],
        value: ValueRef<'record>,
    ) -> Result<ResolvedStructuredArrayValue<Self::Value>, Self::Error> {
        self.plan_leaf(parent, key, value, true)
    }
}

const fn structured_array_append_error(
    error: StructuredArrayPlanError<AppendError>,
    existing_entries: u64,
    configured_entry_limit: u64,
) -> AppendError {
    match error {
        StructuredArrayPlanError::Resolve(error) => error,
        StructuredArrayPlanError::DuplicateField {
            object_depth,
            previous_index,
            field_index,
        } => AppendError::DuplicateField {
            object_depth,
            previous_index,
            field_index,
        },
        StructuredArrayPlanError::LimitExceeded {
            resource: StructuredArrayPlanResource::SchemaEntries,
            actual,
            ..
        } => AppendError::LimitExceeded {
            resource: AppendResource::StructuredArraySchemaEntries,
            actual: existing_entries.saturating_add(actual),
            limit: configured_entry_limit,
        },
        StructuredArrayPlanError::LimitExceeded {
            resource: StructuredArrayPlanResource::NestingDepth,
            actual,
            limit,
        } => AppendError::LimitExceeded {
            resource: AppendResource::NestingDepth,
            actual,
            limit,
        },
        StructuredArrayPlanError::LimitExceeded {
            resource:
                StructuredArrayPlanResource::TraversalStack
                | StructuredArrayPlanResource::PhysicalValues
                | StructuredArrayPlanResource::DuplicateFieldIndex,
            actual,
            limit,
        } => AppendError::LimitExceeded {
            resource: AppendResource::Columns,
            actual,
            limit,
        },
        StructuredArrayPlanError::DelimiterBodyTooLong { actual, limit, .. } => {
            AppendError::FormatDomainExceeded {
                domain: AppendDomain::UnorderedContainerBodyLength,
                actual,
                limit,
            }
        }
        StructuredArrayPlanError::SizeOverflow => AppendError::SizeOverflow,
        StructuredArrayPlanError::AllocationFailed {
            resource: StructuredArrayPlanResource::SchemaEntries,
            requested,
        } => append_allocation(AppendResource::StructuredArraySchemaEntries, requested),
        StructuredArrayPlanError::AllocationFailed {
            resource: StructuredArrayPlanResource::TraversalStack,
            requested,
        } => append_allocation(AppendResource::NestingDepth, requested),
        StructuredArrayPlanError::AllocationFailed {
            resource:
                StructuredArrayPlanResource::NestingDepth
                | StructuredArrayPlanResource::PhysicalValues
                | StructuredArrayPlanResource::DuplicateFieldIndex,
            requested,
        } => append_allocation(AppendResource::Columns, requested),
    }
}

fn sorted_value_positions(
    values: &[BorrowedPlannedValue<'_>],
) -> Result<SortedValuePositions, AppendError> {
    if values
        .windows(2)
        .all(|pair| pair[0].node_id < pair[1].node_id)
    {
        return Ok(SortedValuePositions::new());
    }
    let mut positions = SortedValuePositions::new();
    positions
        .try_reserve_exact(values.len())
        .map_err(|_| append_allocation(AppendResource::Columns, values.len()))?;
    positions.extend(0..values.len());
    positions.sort_unstable_by_key(|&index| values[index].node_id);
    Ok(positions)
}

struct ResultingArchiveSizes {
    resident_bytes: u64,
    encoded_data_size: u64,
}

fn resulting_archive_sizes(
    archive: &PrimitiveArchive,
    limits: WriterLimits,
    added_key_bytes: u64,
    added_schema_bytes: u64,
    dictionaries: &DictionaryPlans<'_>,
    table: &TablePlan,
) -> Result<ResultingArchiveSizes, AppendError> {
    let added_timestamp_bytes = dictionaries.timestamp.added_value_bytes()?;
    let resident_bytes = archive
        .resident_bytes
        .checked_add(added_key_bytes)
        .and_then(|size| size.checked_add(added_schema_bytes))
        .and_then(|size| size.checked_add(table.added_payload_bytes()))
        .and_then(|size| size.checked_add(dictionaries.variable.added_value_bytes))
        .and_then(|size| size.checked_add(dictionaries.log_type.added_value_bytes))
        .and_then(|size| size.checked_add(dictionaries.array.added_value_bytes))
        .and_then(|size| size.checked_add(added_timestamp_bytes))
        .ok_or(AppendError::SizeOverflow)?;
    let encoded_data_size = archive
        .encoded_data_size
        .checked_add(dictionaries.variable.added_data_size)
        .and_then(|size| size.checked_add(dictionaries.log_type.added_data_size))
        .and_then(|size| size.checked_add(dictionaries.array.added_data_size))
        .and_then(|size| size.checked_add(table.added_message_bytes()))
        .ok_or(AppendError::SizeOverflow)?;
    check_append_limit(
        AppendResource::ResidentBytes,
        resident_bytes,
        limits.max_resident_bytes(),
    )?;
    Ok(ResultingArchiveSizes {
        resident_bytes,
        encoded_data_size,
    })
}

fn validate_resulting_archive_shape(
    archive: &PrimitiveArchive,
    limits: WriterLimits,
    is_new_schema: bool,
    value_count: usize,
) -> Result<(), AppendError> {
    let resulting_schemas = usize_u64_append(archive.tables.tables.len())?
        .checked_add(u64::from(is_new_schema))
        .ok_or(AppendError::SizeOverflow)?;
    check_append_limit(
        AppendResource::Schemas,
        resulting_schemas,
        limits.max_schemas(),
    )?;
    if resulting_schemas > MAX_SCHEMA_ID + 1 {
        return Err(AppendError::FormatDomainExceeded {
            domain: AppendDomain::SchemaId,
            actual: resulting_schemas - 1,
            limit: MAX_SCHEMA_ID,
        });
    }
    let added_columns = if is_new_schema {
        usize_u64_append(value_count)?
    } else {
        0
    };
    let resulting_columns = archive
        .tables
        .column_count
        .checked_add(added_columns)
        .ok_or(AppendError::SizeOverflow)?;
    check_append_limit(
        AppendResource::Columns,
        resulting_columns,
        limits.max_columns(),
    )
}

fn resulting_node_path_cache(
    archive: &PrimitiveArchive,
    replacement: Option<Vec<u32>>,
    path_position: usize,
) -> Result<Option<Vec<u32>>, AppendError> {
    if replacement.is_some() || path_position == archive.node_path_cache.len() {
        return Ok(replacement);
    }
    let mut replacement = Vec::new();
    replacement
        .try_reserve_exact(path_position)
        .map_err(|_| append_allocation(AppendResource::SchemaNodes, path_position))?;
    replacement.extend_from_slice(&archive.node_path_cache[..path_position]);
    Ok(Some(replacement))
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn encode_values(
    values: &[BorrowedPlannedValue<'_>],
    sorted_to_traversal: &[usize],
    dictionaries: &mut DictionaryPlans<'_>,
    table: Option<&TableBuilder>,
    limits: WriterLimits,
    timestamp_scratch: &mut String,
    array_validation_scratch: &mut ArrayValidationScratch,
    mut encoded: PlannedRecordValues,
) -> Result<PlannedRecordValues, AppendError> {
    debug_assert_eq!(0, encoded.len());
    encoded
        .try_reserve_exact(values.len())
        .map_err(|_| append_allocation(AppendResource::Columns, values.len()))?;
    for value_index in 0..values.len() {
        let traversal_index = sorted_to_traversal
            .get(value_index)
            .copied()
            .unwrap_or(value_index);
        let value = values[traversal_index];
        let node_id = value.node_id;
        let dictionary_hint = table.and_then(|table| table.columns.get(value_index));
        let encoded_value = match value.value {
            BorrowedScalarValue::I64(value) => EncodedValue::I64(value),
            BorrowedScalarValue::DeltaI64(value) => EncodedValue::DeltaI64 {
                value,
                delta: value,
            },
            BorrowedScalarValue::F64(value) => EncodedValue::F64(value),
            BorrowedScalarValue::FormattedFloat { value, descriptor } => {
                EncodedValue::FormattedFloat { value, descriptor }
            }
            BorrowedScalarValue::DictionaryFloat(source) => EncodedValue::DictionaryFloat(
                dictionaries.resolve_variable(source, dictionary_hint, limits)?,
            ),
            BorrowedScalarValue::Bool(value) => EncodedValue::Bool(value),
            BorrowedScalarValue::String {
                value,
                node_type: NodeType::VarString,
            } => EncodedValue::VarString(dictionaries.resolve_variable(
                value,
                dictionary_hint,
                limits,
            )?),
            BorrowedScalarValue::String {
                value,
                node_type: NodeType::ClpString,
            } => {
                let clp = clp::encode_clp_string(
                    value,
                    limits.max_dictionary_entry_size(),
                    limits.max_encoded_variables_per_column(),
                    |variable| dictionaries.resolve_variable(variable, None, limits),
                )?;
                let log_type_id = dictionaries.resolve_log_type(
                    clp.logtype.as_slice(),
                    dictionary_hint,
                    limits,
                )?;
                EncodedValue::ClpString {
                    node_type: NodeType::ClpString,
                    log_type_id,
                    encoded_variable_offset: 0,
                    variables: clp.variables,
                }
            }
            BorrowedScalarValue::KvIrEncodedText { event, pair_index } => {
                let pair = event
                    .pair(pair_index)
                    .expect("the KV-IR event plan retains a validated pair index");
                let KvIrValueKind::EncodedText(text) = pair.value().kind() else {
                    unreachable!("the direct KV-IR plan retains only encoded-text values")
                };
                let width = match text.encoding() {
                    KvIrEncoding::FourByte => clp::PreencodedWidth::FourByte,
                    KvIrEncoding::EightByte => clp::PreencodedWidth::EightByte,
                };
                let encoded_variables = text.encoded_variables().map(|value| match value {
                    KvIrEncodedVariable::FourByte(value) => {
                        clp::PreencodedVariable::FourByte(value)
                    }
                    KvIrEncodedVariable::EightByte(value) => {
                        clp::PreencodedVariable::EightByte(value)
                    }
                });
                let clp = clp::encode_preencoded_clp_string(
                    text.logtype(),
                    width,
                    encoded_variables,
                    text.dictionary_variables(),
                    limits.max_dictionary_entry_size(),
                    limits.max_encoded_variables_per_column(),
                    |variable| dictionaries.resolve_variable(variable, None, limits),
                )?;
                let log_type_id = dictionaries.resolve_log_type(
                    clp.logtype.as_slice(),
                    dictionary_hint,
                    limits,
                )?;
                EncodedValue::ClpString {
                    node_type: NodeType::ClpString,
                    log_type_id,
                    encoded_variable_offset: 0,
                    variables: clp.variables,
                }
            }
            BorrowedScalarValue::String { .. } => {
                unreachable!("string planner only creates VarString or ClpString nodes")
            }
            BorrowedScalarValue::UnstructuredArray(value) => {
                array::validate(value, limits, array_validation_scratch, node_id)?;
                let clp = clp::encode_clp_string(
                    value.raw_json(),
                    limits.max_dictionary_entry_size(),
                    limits.max_encoded_variables_per_column(),
                    |variable| dictionaries.resolve_variable(variable, None, limits),
                )?;
                let log_type_id =
                    dictionaries.resolve_array_log_type(&clp.logtype, dictionary_hint, limits)?;
                EncodedValue::ClpString {
                    node_type: NodeType::UnstructuredArray,
                    log_type_id,
                    encoded_variable_offset: 0,
                    variables: clp.variables,
                }
            }
            BorrowedScalarValue::Timestamp(value) => encode_timestamp_value(
                dictionaries,
                node_id,
                value,
                false,
                limits,
                timestamp_scratch,
            )?,
            BorrowedScalarValue::PrevalidatedTimestamp(value) => encode_timestamp_value(
                dictionaries,
                node_id,
                value,
                true,
                limits,
                timestamp_scratch,
            )?,
        };
        debug_assert_eq!(value.value.node_type(), encoded_value.node_type());
        encoded.push(PlannedValue {
            node_id,
            value: encoded_value,
        });
    }
    Ok(encoded)
}

fn encode_timestamp_value(
    dictionaries: &mut DictionaryPlans<'_>,
    node_id: u32,
    value: TimestampRef<'_>,
    prevalidated: bool,
    limits: WriterLimits,
    scratch: &mut String,
) -> Result<EncodedValue, AppendError> {
    Ok(EncodedValue::Timestamp {
        value: value.epoch_nanoseconds(),
        delta: value.epoch_nanoseconds(),
        pattern_id: dictionaries.resolve_timestamp(
            node_id,
            value,
            prevalidated,
            limits,
            scratch,
        )?,
    })
}

fn validate_unique_fields(fields: &[FieldRef<'_>], object_depth: u64) -> Result<(), AppendError> {
    let mut indexes = HashMap::<&[u8], usize>::new();
    indexes
        .try_reserve(fields.len())
        .map_err(|_| append_allocation(AppendResource::Columns, fields.len()))?;
    for (field_index, field) in fields.iter().enumerate() {
        if let Some(previous_index) = indexes.insert(field.key(), field_index) {
            return Err(AppendError::DuplicateField {
                object_depth,
                previous_index,
                field_index,
            });
        }
    }
    Ok(())
}

#[derive(Debug)]
enum TablePlan {
    Existing {
        table_index: usize,
        entries: RecordEntries,
        values: PlannedRecordValues,
        added_payload_bytes: u64,
        added_message_bytes: u64,
        added_encoded_variables: u64,
    },
    New {
        table: TableBuilder,
        added_payload_bytes: u64,
        added_message_bytes: u64,
        added_encoded_variables: u64,
    },
}

impl TablePlan {
    const fn added_payload_bytes(&self) -> u64 {
        match self {
            Self::Existing {
                added_payload_bytes,
                ..
            }
            | Self::New {
                added_payload_bytes,
                ..
            } => *added_payload_bytes,
        }
    }

    const fn added_encoded_variables(&self) -> u64 {
        match self {
            Self::Existing {
                added_encoded_variables,
                ..
            }
            | Self::New {
                added_encoded_variables,
                ..
            } => *added_encoded_variables,
        }
    }

    const fn added_message_bytes(&self) -> u64 {
        match self {
            Self::Existing {
                added_message_bytes,
                ..
            }
            | Self::New {
                added_message_bytes,
                ..
            } => *added_message_bytes,
        }
    }
}

#[derive(Debug)]
struct TableColumn {
    node_id: u32,
    node_type: NodeType,
    data: TableColumnData,
}

#[derive(Debug)]
enum TableColumnData {
    Fixed(Vec<u8>),
    DeltaI64 {
        deltas: Vec<u8>,
        current: i64,
    },
    Timestamp {
        deltas: Vec<u8>,
        current: i64,
        pattern_ids: Vec<u8>,
    },
    FormattedFloat {
        values: Vec<u8>,
        descriptors: Vec<u8>,
    },
    ClpString {
        descriptors: Vec<u8>,
        encoded_variables: Vec<u8>,
        encoded_variable_count: u64,
    },
}

impl TableColumn {
    fn last_dictionary_id(&self) -> Option<u64> {
        match (&self.data, self.node_type) {
            (TableColumnData::Fixed(values), NodeType::VarString | NodeType::DictionaryFloat) => {
                trailing_u64(values)
            }
            (
                TableColumnData::ClpString { descriptors, .. },
                NodeType::ClpString | NodeType::UnstructuredArray,
            ) => trailing_u64(descriptors).map(|descriptor| descriptor & MAX_LOG_TYPE_ID),
            _ => None,
        }
    }

    fn older_fixed_dictionary_ids(&self) -> impl Iterator<Item = u64> + '_ {
        let bytes: &[u8] = match (&self.data, self.node_type) {
            (TableColumnData::Fixed(values), NodeType::VarString | NodeType::DictionaryFloat) => {
                values
            }
            _ => &[],
        };
        let (encoded_ids, remainder) = bytes.as_chunks::<{ size_of::<u64>() }>();
        debug_assert_eq!(0, remainder.len());
        encoded_ids
            .iter()
            .rev()
            .skip(1)
            .take(OLDER_FIXED_DICTIONARY_HINT_LIMIT)
            .map(|encoded| u64::from_le_bytes(*encoded))
    }

    fn from_value(value: PlannedValue) -> Result<Self, AppendError> {
        let node_id = value.node_id;
        let node_type = value.value.node_type();
        let data = match value.value {
            EncodedValue::DeltaI64 { value, delta } => {
                let mut deltas = Vec::new();
                deltas.try_reserve_exact(size_of::<i64>()).map_err(|_| {
                    append_allocation(AppendResource::ResidentBytes, size_of::<i64>())
                })?;
                deltas.extend_from_slice(&delta.to_le_bytes());
                TableColumnData::DeltaI64 {
                    deltas,
                    current: value,
                }
            }
            EncodedValue::Timestamp {
                value,
                delta,
                pattern_id,
            } => timestamp_column_data(value, delta, pattern_id)?,
            EncodedValue::FormattedFloat { value, descriptor } => {
                let mut values = Vec::new();
                values.try_reserve_exact(size_of::<f64>()).map_err(|_| {
                    append_allocation(AppendResource::ResidentBytes, size_of::<f64>())
                })?;
                values.extend_from_slice(&value.to_le_bytes());
                let mut descriptors = Vec::new();
                descriptors
                    .try_reserve_exact(size_of::<u16>())
                    .map_err(|_| {
                        append_allocation(AppendResource::ResidentBytes, size_of::<u16>())
                    })?;
                descriptors.extend_from_slice(&descriptor.to_le_bytes());
                TableColumnData::FormattedFloat {
                    values,
                    descriptors,
                }
            }
            EncodedValue::ClpString {
                log_type_id,
                encoded_variable_offset,
                variables,
                ..
            } => {
                let variable_bytes = variables
                    .len()
                    .checked_mul(size_of::<i64>())
                    .ok_or(AppendError::SizeOverflow)?;
                let mut descriptors = Vec::new();
                descriptors
                    .try_reserve_exact(size_of::<u64>())
                    .map_err(|_| {
                        append_allocation(AppendResource::ResidentBytes, size_of::<u64>())
                    })?;
                append_u64(
                    &mut descriptors,
                    encode_descriptor(log_type_id, encoded_variable_offset),
                );
                let mut encoded_variables = Vec::new();
                encoded_variables
                    .try_reserve_exact(variable_bytes)
                    .map_err(|_| {
                        append_allocation(AppendResource::ResidentBytes, variable_bytes)
                    })?;
                for variable in &variables {
                    encoded_variables.extend_from_slice(&variable.to_le_bytes());
                }
                TableColumnData::ClpString {
                    descriptors,
                    encoded_variables,
                    encoded_variable_count: usize_u64_append(variables.len())?,
                }
            }
            value => {
                let encoded_size = usize::try_from(value.appended_size()?)
                    .map_err(|_| AppendError::SizeOverflow)?;
                let mut bytes = Vec::new();
                bytes
                    .try_reserve_exact(encoded_size)
                    .map_err(|_| append_allocation(AppendResource::ResidentBytes, encoded_size))?;
                append_fixed_value(&value, &mut bytes);
                TableColumnData::Fixed(bytes)
            }
        };
        Ok(Self {
            node_id,
            node_type,
            data,
        })
    }

    const fn encoded_variable_count(&self) -> u64 {
        match &self.data {
            TableColumnData::Fixed(_)
            | TableColumnData::DeltaI64 { .. }
            | TableColumnData::Timestamp { .. }
            | TableColumnData::FormattedFloat { .. } => 0,
            TableColumnData::ClpString {
                encoded_variable_count,
                ..
            } => *encoded_variable_count,
        }
    }

    fn reserve_append(&mut self, value: &EncodedValue) -> Result<(), AppendError> {
        match (&mut self.data, value) {
            (TableColumnData::DeltaI64 { deltas, .. }, EncodedValue::DeltaI64 { .. }) => deltas
                .try_reserve(size_of::<i64>())
                .map_err(|_| append_allocation(AppendResource::ResidentBytes, size_of::<i64>())),
            (
                TableColumnData::Timestamp {
                    deltas,
                    pattern_ids,
                    ..
                },
                EncodedValue::Timestamp { .. },
            ) => {
                deltas.try_reserve(size_of::<i64>()).map_err(|_| {
                    append_allocation(AppendResource::ResidentBytes, size_of::<i64>())
                })?;
                pattern_ids
                    .try_reserve(size_of::<u64>())
                    .map_err(|_| append_allocation(AppendResource::ResidentBytes, size_of::<u64>()))
            }
            (
                TableColumnData::FormattedFloat {
                    values,
                    descriptors,
                },
                EncodedValue::FormattedFloat { .. },
            ) => {
                values.try_reserve(size_of::<f64>()).map_err(|_| {
                    append_allocation(AppendResource::ResidentBytes, size_of::<f64>())
                })?;
                descriptors
                    .try_reserve(size_of::<u16>())
                    .map_err(|_| append_allocation(AppendResource::ResidentBytes, size_of::<u16>()))
            }
            (TableColumnData::Fixed(bytes), _) => {
                let size = usize::try_from(value.appended_size()?)
                    .map_err(|_| AppendError::SizeOverflow)?;
                bytes
                    .try_reserve(size)
                    .map_err(|_| append_allocation(AppendResource::ResidentBytes, size))
            }
            (
                TableColumnData::ClpString {
                    descriptors,
                    encoded_variables,
                    ..
                },
                EncodedValue::ClpString { variables, .. },
            ) => {
                descriptors.try_reserve(size_of::<u64>()).map_err(|_| {
                    append_allocation(AppendResource::ResidentBytes, size_of::<u64>())
                })?;
                let variable_bytes = variables
                    .len()
                    .checked_mul(size_of::<i64>())
                    .ok_or(AppendError::SizeOverflow)?;
                encoded_variables
                    .try_reserve(variable_bytes)
                    .map_err(|_| append_allocation(AppendResource::ResidentBytes, variable_bytes))
            }
            (TableColumnData::ClpString { .. }, _) => {
                unreachable!("validated table schema keeps CLP column types stable")
            }
            (TableColumnData::FormattedFloat { .. }, _) => {
                unreachable!("validated table schema keeps formatted-float columns stable")
            }
            (TableColumnData::DeltaI64 { .. }, _) => {
                unreachable!("validated table schema keeps delta-integer columns stable")
            }
            (TableColumnData::Timestamp { .. }, _) => {
                unreachable!("validated table schema keeps timestamp columns stable")
            }
        }
    }

    fn append(&mut self, value: EncodedValue) {
        match (&mut self.data, value) {
            (
                TableColumnData::DeltaI64 { deltas, current },
                EncodedValue::DeltaI64 { value, delta },
            ) => {
                deltas.extend_from_slice(&delta.to_le_bytes());
                *current = value;
            }
            (
                TableColumnData::Timestamp {
                    deltas,
                    current,
                    pattern_ids,
                },
                EncodedValue::Timestamp {
                    value,
                    delta,
                    pattern_id,
                },
            ) => {
                deltas.extend_from_slice(&delta.to_le_bytes());
                *current = value;
                pattern_ids.extend_from_slice(&pattern_id.to_le_bytes());
            }
            (
                TableColumnData::FormattedFloat {
                    values,
                    descriptors,
                },
                EncodedValue::FormattedFloat { value, descriptor },
            ) => {
                values.extend_from_slice(&value.to_le_bytes());
                descriptors.extend_from_slice(&descriptor.to_le_bytes());
            }
            (TableColumnData::Fixed(bytes), value) => append_fixed_value(&value, bytes),
            (
                TableColumnData::ClpString {
                    descriptors,
                    encoded_variables,
                    encoded_variable_count,
                },
                EncodedValue::ClpString {
                    log_type_id,
                    encoded_variable_offset,
                    variables,
                    ..
                },
            ) => {
                append_u64(
                    descriptors,
                    encode_descriptor(log_type_id, encoded_variable_offset),
                );
                for variable in &variables {
                    encoded_variables.extend_from_slice(&variable.to_le_bytes());
                }
                *encoded_variable_count += u64::try_from(variables.len())
                    .expect("validated encoded-variable count must fit u64");
            }
            (TableColumnData::ClpString { .. }, _) => {
                unreachable!("validated table schema keeps CLP column types stable")
            }
            (TableColumnData::FormattedFloat { .. }, _) => {
                unreachable!("validated table schema keeps formatted-float columns stable")
            }
            (TableColumnData::DeltaI64 { .. }, _) => {
                unreachable!("validated table schema keeps delta-integer columns stable")
            }
            (TableColumnData::Timestamp { .. }, _) => {
                unreachable!("validated table schema keeps timestamp columns stable")
            }
        }
    }

    fn write_to<W: Write>(&self, output: &mut W) -> Result<(), WriterError> {
        match &self.data {
            TableColumnData::Fixed(bytes) => output.write_all(bytes).map_err(WriterError::Io),
            TableColumnData::DeltaI64 { deltas, .. } => {
                output.write_all(deltas).map_err(WriterError::Io)
            }
            TableColumnData::Timestamp {
                deltas,
                pattern_ids,
                ..
            } => {
                output.write_all(deltas).map_err(WriterError::Io)?;
                output.write_all(pattern_ids).map_err(WriterError::Io)
            }
            TableColumnData::FormattedFloat {
                values,
                descriptors,
            } => {
                output.write_all(values).map_err(WriterError::Io)?;
                output.write_all(descriptors).map_err(WriterError::Io)
            }
            TableColumnData::ClpString {
                descriptors,
                encoded_variables,
                encoded_variable_count,
            } => {
                output.write_all(descriptors).map_err(WriterError::Io)?;
                write_u64(output, *encoded_variable_count)?;
                output.write_all(encoded_variables).map_err(WriterError::Io)
            }
        }
    }
}

fn trailing_u64(bytes: &[u8]) -> Option<u64> {
    let start = bytes.len().checked_sub(size_of::<u64>())?;
    let encoded = <[u8; size_of::<u64>()]>::try_from(bytes.get(start..)?).ok()?;
    Some(u64::from_le_bytes(encoded))
}

fn timestamp_column_data(
    value: i64,
    delta: i64,
    pattern_id: u64,
) -> Result<TableColumnData, AppendError> {
    let mut deltas = Vec::new();
    deltas
        .try_reserve_exact(size_of::<i64>())
        .map_err(|_| append_allocation(AppendResource::ResidentBytes, size_of::<i64>()))?;
    deltas.extend_from_slice(&delta.to_le_bytes());
    let mut pattern_ids = Vec::new();
    pattern_ids
        .try_reserve_exact(size_of::<u64>())
        .map_err(|_| append_allocation(AppendResource::ResidentBytes, size_of::<u64>()))?;
    pattern_ids.extend_from_slice(&pattern_id.to_le_bytes());
    Ok(TableColumnData::Timestamp {
        deltas,
        current: value,
        pattern_ids,
    })
}

fn append_fixed_value(value: &EncodedValue, output: &mut Vec<u8>) {
    match value {
        EncodedValue::I64(value) => output.extend_from_slice(&value.to_le_bytes()),
        EncodedValue::F64(value) => output.extend_from_slice(&value.to_le_bytes()),
        EncodedValue::DictionaryFloat(id) | EncodedValue::VarString(id) => {
            output.extend_from_slice(&id.to_le_bytes());
        }
        EncodedValue::Bool(value) => output.push(u8::from(*value)),
        EncodedValue::DeltaI64 { .. }
        | EncodedValue::Timestamp { .. }
        | EncodedValue::FormattedFloat { .. }
        | EncodedValue::ClpString { .. } => {
            unreachable!("split columns do not use fixed-value encoding")
        }
    }
}

const fn encode_descriptor(log_type_id: u64, encoded_variable_offset: u64) -> u64 {
    log_type_id | (encoded_variable_offset << 24)
}

#[derive(Debug)]
struct TableBuilder {
    schema_id: i32,
    entries: Vec<u32>,
    ordered_entry_count: usize,
    columns: Vec<TableColumn>,
    message_count: u64,
    uncompressed_size: usize,
}

impl TableBuilder {
    fn from_record(
        schema_id: i32,
        entries: RecordEntries,
        ordered_entry_count: usize,
        values: PlannedRecordValues,
    ) -> Result<Self, AppendError> {
        let mut columns = Vec::new();
        columns
            .try_reserve_exact(values.len())
            .map_err(|_| append_allocation(AppendResource::Columns, values.len()))?;
        let mut uncompressed_size = 0_usize;
        for value in values {
            let encoded_size = usize::try_from(value.value.new_column_size()?)
                .map_err(|_| AppendError::SizeOverflow)?;
            uncompressed_size = uncompressed_size
                .checked_add(encoded_size)
                .ok_or(AppendError::SizeOverflow)?;
            columns.push(TableColumn::from_value(value)?);
        }
        Ok(Self {
            schema_id,
            entries,
            ordered_entry_count,
            columns,
            message_count: 1,
            uncompressed_size,
        })
    }
}

#[derive(Debug, Default)]
struct TableSet {
    tables: Vec<TableBuilder>,
    schema_buckets: HashMap<u64, Vec<usize>>,
    column_count: u64,
    total_encoded_variables: u64,
    last_table_index: Option<usize>,
}

impl TableSet {
    fn find_schema(&self, entries: &[u32], ordered_entry_count: usize) -> Option<usize> {
        if let Some(index) = self.last_table_index
            && self.tables[index].entries == entries
            && self.tables[index].ordered_entry_count == ordered_entry_count
        {
            return Some(index);
        }
        self.schema_buckets
            .get(&hash_schema(entries, ordered_entry_count))
            .and_then(|indexes| {
                indexes.iter().copied().find(|index| {
                    self.tables[*index].entries == entries
                        && self.tables[*index].ordered_entry_count == ordered_entry_count
                })
            })
    }

    fn plan_append(
        &self,
        entries: RecordEntries,
        ordered_entry_count: usize,
        mut values: PlannedRecordValues,
        table_index: Option<usize>,
        cached_layout_matches: bool,
        limits: WriterLimits,
    ) -> Result<TablePlan, AppendError> {
        let Some(table_index) = table_index else {
            return self.plan_new_table_append(entries, ordered_entry_count, values, limits);
        };
        let table = &self.tables[table_index];
        if cached_layout_matches {
            debug_assert_eq!(table.columns.len(), values.len());
        } else {
            Self::validate_uncached_existing_table(table, &entries, ordered_entry_count, &values)?;
        }
        table
            .message_count
            .checked_add(1)
            .ok_or(AppendError::SizeOverflow)?;
        prepare_delta_values(table, &mut values)?;
        let added_encoded_variables = prepare_clp_offsets(table, &mut values, limits)?;
        let added_payload_bytes = values.iter().try_fold(0_u64, |size, value| {
            size.checked_add(value.value.appended_size()?)
                .ok_or(AppendError::SizeOverflow)
        })?;
        validate_total_encoded_variables(
            self.total_encoded_variables,
            added_encoded_variables,
            limits,
        )?;
        Ok(TablePlan::Existing {
            table_index,
            entries,
            values,
            added_payload_bytes,
            added_message_bytes: added_payload_bytes,
            added_encoded_variables,
        })
    }

    #[cold]
    #[inline(never)]
    fn validate_uncached_existing_table(
        table: &TableBuilder,
        entries: &RecordEntries,
        ordered_entry_count: usize,
        values: &[PlannedValue],
    ) -> Result<(), AppendError> {
        debug_assert_eq!(&table.entries, entries);
        debug_assert_eq!(table.ordered_entry_count, ordered_entry_count);
        if table.columns.len() != values.len() {
            return Err(AppendError::SizeOverflow);
        }
        for (column, value) in table.columns.iter().zip(values) {
            if column.node_id != value.node_id || column.node_type != value.value.node_type() {
                return Err(AppendError::SizeOverflow);
            }
        }
        Ok(())
    }

    #[cold]
    #[inline(never)]
    fn plan_new_table_append(
        &self,
        entries: RecordEntries,
        ordered_entry_count: usize,
        mut values: PlannedRecordValues,
        limits: WriterLimits,
    ) -> Result<TablePlan, AppendError> {
        let schema_id =
            i32::try_from(self.tables.len()).map_err(|_| AppendError::FormatDomainExceeded {
                domain: AppendDomain::SchemaId,
                actual: u64::try_from(self.tables.len()).unwrap_or(u64::MAX),
                limit: MAX_SCHEMA_ID,
            })?;
        let added_encoded_variables = prepare_new_clp_columns(&mut values, limits)?;
        validate_total_encoded_variables(
            self.total_encoded_variables,
            added_encoded_variables,
            limits,
        )?;
        let added_message_bytes = values.iter().try_fold(0_u64, |size, value| {
            size.checked_add(value.value.appended_size()?)
                .ok_or(AppendError::SizeOverflow)
        })?;
        let table = TableBuilder::from_record(schema_id, entries, ordered_entry_count, values)?;
        let added_payload_bytes = usize_u64_append(table.uncompressed_size)?;
        Ok(TablePlan::New {
            table,
            added_payload_bytes,
            added_message_bytes,
            added_encoded_variables,
        })
    }

    fn commit(&mut self, plan: TablePlan, reservations: TableReservations) -> RecordPlanScratch {
        self.total_encoded_variables += plan.added_encoded_variables();
        match plan {
            TablePlan::Existing {
                table_index,
                entries,
                mut values,
                ..
            } => {
                let table = &mut self.tables[table_index];
                // Draining moves every value while deliberately preserving the outer allocation
                // for the next record plan.
                #[allow(clippy::iter_with_drain)]
                for (column, value) in table.columns.iter_mut().zip(values.drain(..)) {
                    column.append(value.value);
                }
                table.message_count += 1;
                table.uncompressed_size = reservations.resulting_table_size;
                self.last_table_index = Some(table_index);
                RecordPlanScratch::recycle(entries, values)
            }
            TablePlan::New { table, .. } => {
                let table_index = self.tables.len();
                let schema_hash = hash_schema(&table.entries, table.ordered_entry_count);
                self.column_count += u64::try_from(table.columns.len())
                    .expect("validated column count must fit u64");
                self.tables.push(table);
                self.last_table_index = Some(table_index);
                if let Some(bucket) = self.schema_buckets.get_mut(&schema_hash) {
                    bucket.push(table_index);
                } else {
                    self.schema_buckets
                        .insert(schema_hash, reservations.new_schema_bucket);
                }
                RecordPlanScratch::default()
            }
        }
    }

    fn encode_schema_map(&self, compression_level: i32) -> Result<Vec<u8>, WriterError> {
        let mut sorted = Vec::new();
        sorted
            .try_reserve_exact(self.tables.len())
            .map_err(|_| WriterError::AllocationFailed {
                requested: self.tables.len(),
            })?;
        sorted.extend(self.tables.iter());
        sorted.sort_unstable_by(|left, right| left.entries.cmp(&right.entries));
        let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), compression_level)
            .map_err(WriterError::Io)?;
        write_u64(&mut encoder, usize_u64(sorted.len())?)?;
        for table in sorted {
            write_i32(&mut encoder, table.schema_id)?;
            let entry_count =
                u32::try_from(table.entries.len()).map_err(|_| WriterError::SizeOverflow)?;
            write_u32(&mut encoder, entry_count)?;
            write_u32(
                &mut encoder,
                u32::try_from(table.ordered_entry_count).map_err(|_| WriterError::SizeOverflow)?,
            )?;
            for entry in &table.entries {
                write_i32(&mut encoder, i32::from_le_bytes(entry.to_le_bytes()))?;
            }
        }
        encoder.finish().map_err(WriterError::Io)
    }

    fn encode_tables(&self, options: WriterOptions) -> Result<(Vec<u8>, Vec<u8>), WriterError> {
        let mut order = Vec::new();
        order
            .try_reserve_exact(self.tables.len())
            .map_err(|_| WriterError::AllocationFailed {
                requested: self.tables.len(),
            })?;
        order.extend(0..self.tables.len());
        order.sort_unstable_by(|left, right| {
            self.tables[*right]
                .uncompressed_size
                .cmp(&self.tables[*left].uncompressed_size)
                .then_with(|| {
                    self.tables[*left]
                        .schema_id
                        .cmp(&self.tables[*right].schema_id)
                })
        });
        let packed = pack_tables(&self.tables, &order, options)?;
        let metadata = encode_table_metadata(&packed, options.compression_level())?;
        Ok((metadata, packed.compressed))
    }
}

fn prepare_delta_values(
    table: &TableBuilder,
    values: &mut [PlannedValue],
) -> Result<(), AppendError> {
    for (column, value) in table.columns.iter().zip(values) {
        match (&column.data, &mut value.value) {
            (
                TableColumnData::DeltaI64 { current, .. },
                EncodedValue::DeltaI64 {
                    value: absolute,
                    delta,
                },
            )
            | (
                TableColumnData::Timestamp { current, .. },
                EncodedValue::Timestamp {
                    value: absolute,
                    delta,
                    ..
                },
            ) => {
                *delta = absolute
                    .checked_sub(*current)
                    .ok_or(AppendError::SizeOverflow)?;
            }
            (_, EncodedValue::DeltaI64 { .. } | EncodedValue::Timestamp { .. }) => {
                return Err(AppendError::SizeOverflow);
            }
            _ => {}
        }
    }
    Ok(())
}

fn prepare_clp_offsets(
    table: &TableBuilder,
    values: &mut [PlannedValue],
    limits: WriterLimits,
) -> Result<u64, AppendError> {
    let mut added_total = 0_u64;
    for (column, value) in table.columns.iter().zip(values) {
        let EncodedValue::ClpString {
            encoded_variable_offset,
            variables,
            ..
        } = &mut value.value
        else {
            continue;
        };
        let offset = column.encoded_variable_count();
        validate_encoded_variable_offset(offset)?;
        *encoded_variable_offset = offset;
        let added = usize_u64_append(variables.len())?;
        let resulting = offset.checked_add(added).ok_or(AppendError::SizeOverflow)?;
        validate_column_encoded_variables(resulting, limits)?;
        added_total = added_total
            .checked_add(added)
            .ok_or(AppendError::SizeOverflow)?;
    }
    Ok(added_total)
}

const fn validate_encoded_variable_offset(offset: u64) -> Result<(), AppendError> {
    if offset > MAX_ENCODED_VARIABLE_OFFSET {
        Err(AppendError::FormatDomainExceeded {
            domain: AppendDomain::EncodedVariableOffset,
            actual: offset,
            limit: MAX_ENCODED_VARIABLE_OFFSET,
        })
    } else {
        Ok(())
    }
}

fn prepare_new_clp_columns(
    values: &mut [PlannedValue],
    limits: WriterLimits,
) -> Result<u64, AppendError> {
    let mut added_total = 0_u64;
    for value in values {
        let EncodedValue::ClpString {
            encoded_variable_offset,
            variables,
            ..
        } = &mut value.value
        else {
            continue;
        };
        *encoded_variable_offset = 0;
        let count = usize_u64_append(variables.len())?;
        validate_column_encoded_variables(count, limits)?;
        added_total = added_total
            .checked_add(count)
            .ok_or(AppendError::SizeOverflow)?;
    }
    Ok(added_total)
}

const fn validate_column_encoded_variables(
    actual: u64,
    limits: WriterLimits,
) -> Result<(), AppendError> {
    if actual > MAX_ENCODED_VARIABLE_COUNT {
        return Err(AppendError::FormatDomainExceeded {
            domain: AppendDomain::EncodedVariableCount,
            actual,
            limit: MAX_ENCODED_VARIABLE_COUNT,
        });
    }
    check_append_limit(
        AppendResource::EncodedVariablesPerColumn,
        actual,
        limits.max_encoded_variables_per_column(),
    )
}

fn validate_total_encoded_variables(
    current: u64,
    added: u64,
    limits: WriterLimits,
) -> Result<(), AppendError> {
    let actual = current
        .checked_add(added)
        .ok_or(AppendError::SizeOverflow)?;
    check_append_limit(
        AppendResource::TotalEncodedVariables,
        actual,
        limits.max_total_encoded_variables(),
    )
}

#[derive(Debug)]
struct TreeReservations {
    new_buckets: Vec<TreeBucket>,
}

#[derive(Debug)]
struct TableReservations {
    new_schema_bucket: Vec<usize>,
    resulting_table_size: usize,
}

#[derive(Clone, Copy)]
struct DictionaryReservations {
    resulting_value_len: usize,
    resulting_entry_count: usize,
}

struct CommitReservations {
    tree: TreeReservations,
    variable_dictionary: DictionaryReservations,
    log_type_dictionary: DictionaryReservations,
    array_dictionary: DictionaryReservations,
    timestamp_dictionary: TimestampReservations,
    table: TableReservations,
}

impl CommitReservations {
    fn prepare(archive: &mut PrimitiveArchive, plan: &RecordPlan) -> Result<Self, AppendError> {
        let tree = prepare_tree_reservations(&mut archive.tree, &plan.nodes)?;
        let variable_dictionary = prepare_dictionary_reservations(
            &mut archive.variable_dictionary,
            &plan.variable_dictionary,
            DictionaryKind::Variable,
        )?;
        let log_type_dictionary = prepare_dictionary_reservations(
            &mut archive.log_type_dictionary,
            &plan.log_type_dictionary,
            DictionaryKind::LogType,
        )?;
        let array_dictionary = prepare_dictionary_reservations(
            &mut archive.array_dictionary,
            &plan.array_dictionary,
            DictionaryKind::Array,
        )?;
        let timestamp_dictionary = prepare_timestamp_reservations(
            &mut archive.timestamp_dictionary,
            &plan.timestamp_dictionary,
        )?;
        let table = prepare_table_reservations(&mut archive.tables, &plan.table)?;
        Ok(Self {
            tree,
            variable_dictionary,
            log_type_dictionary,
            array_dictionary,
            timestamp_dictionary,
            table,
        })
    }
}

fn prepare_tree_reservations(
    tree: &mut SchemaTreeBuilder,
    nodes: &[SchemaNodeRecord],
) -> Result<TreeReservations, AppendError> {
    if nodes.is_empty() {
        return Ok(TreeReservations {
            new_buckets: Vec::new(),
        });
    }
    tree.nodes
        .try_reserve(nodes.len())
        .map_err(|_| append_allocation(AppendResource::SchemaNodes, nodes.len()))?;
    let mut counts = HashMap::<TreeIdentity, usize>::new();
    counts
        .try_reserve(nodes.len())
        .map_err(|_| append_allocation(AppendResource::SchemaNodes, nodes.len()))?;
    for node in nodes {
        let key = (node.parent, node.node_type, hash_bytes(&node.key));
        let count = counts.entry(key).or_default();
        *count = count.checked_add(1).ok_or(AppendError::SizeOverflow)?;
    }
    let new_bucket_count = counts
        .iter()
        .filter(|(key, _)| !tree.identities.contains_key(*key))
        .count();
    tree.identities
        .try_reserve(new_bucket_count)
        .map_err(|_| append_allocation(AppendResource::SchemaNodes, new_bucket_count))?;
    let mut new_buckets = Vec::new();
    new_buckets
        .try_reserve_exact(new_bucket_count)
        .map_err(|_| append_allocation(AppendResource::SchemaNodes, new_bucket_count))?;
    for (key, count) in counts {
        if let Some(bucket) = tree.identities.get_mut(&key) {
            bucket
                .try_reserve(count)
                .map_err(|_| append_allocation(AppendResource::SchemaNodes, count))?;
        } else {
            let mut bucket = Vec::new();
            bucket
                .try_reserve_exact(count)
                .map_err(|_| append_allocation(AppendResource::SchemaNodes, count))?;
            new_buckets.push((key, bucket));
        }
    }
    Ok(TreeReservations { new_buckets })
}

fn prepare_dictionary_reservations(
    dictionary: &mut DictionaryBuilder,
    plan: &DictionaryPlan,
    kind: DictionaryKind,
) -> Result<DictionaryReservations, AppendError> {
    let resulting_value_len = dictionary
        .values
        .len()
        .checked_add(plan.values.len())
        .ok_or(AppendError::SizeOverflow)?;
    let resulting_entry_count = dictionary
        .entry_count()
        .checked_add(plan.entries.len())
        .ok_or(AppendError::SizeOverflow)?;
    if plan.entries.is_empty() {
        return Ok(DictionaryReservations {
            resulting_value_len,
            resulting_entry_count,
        });
    }
    dictionary
        .values
        .try_reserve(plan.values.len())
        .map_err(|_| append_allocation(AppendResource::DictionaryValueBytes, plan.values.len()))?;
    dictionary
        .entry_ends
        .try_reserve(plan.entries.len())
        .map_err(|_| append_allocation(kind.entries_resource(), plan.entries.len()))?;
    let hash_builder = &dictionary.hash_builder;
    let values = &dictionary.values;
    let entry_ends = &dictionary.entry_ends;
    dictionary
        .index
        .try_reserve(plan.entries.len(), |id| {
            dictionary_id_hash(hash_builder, values, entry_ends, *id)
        })
        .map_err(|_| append_allocation(kind.entries_resource(), plan.entries.len()))?;
    Ok(DictionaryReservations {
        resulting_value_len,
        resulting_entry_count,
    })
}

fn prepare_table_reservations(
    tables: &mut TableSet,
    plan: &TablePlan,
) -> Result<TableReservations, AppendError> {
    match plan {
        TablePlan::Existing {
            table_index,
            values,
            ..
        } => {
            let table = &mut tables.tables[*table_index];
            for (column, value) in table.columns.iter_mut().zip(values) {
                column.reserve_append(&value.value)?;
            }
            let added_table_bytes = usize::try_from(plan.added_payload_bytes())
                .map_err(|_| AppendError::SizeOverflow)?;
            let resulting_table_size = table
                .uncompressed_size
                .checked_add(added_table_bytes)
                .ok_or(AppendError::SizeOverflow)?;
            Ok(TableReservations {
                new_schema_bucket: Vec::new(),
                resulting_table_size,
            })
        }
        TablePlan::New { table, .. } => {
            tables
                .tables
                .try_reserve(1)
                .map_err(|_| append_allocation(AppendResource::Schemas, 1))?;
            let schema_hash = hash_schema(&table.entries, table.ordered_entry_count);
            let mut new_schema_bucket = Vec::new();
            if let Some(bucket) = tables.schema_buckets.get_mut(&schema_hash) {
                bucket
                    .try_reserve(1)
                    .map_err(|_| append_allocation(AppendResource::Schemas, 1))?;
            } else {
                tables
                    .schema_buckets
                    .try_reserve(1)
                    .map_err(|_| append_allocation(AppendResource::Schemas, 1))?;
                new_schema_bucket
                    .try_reserve_exact(1)
                    .map_err(|_| append_allocation(AppendResource::Schemas, 1))?;
                new_schema_bucket.push(tables.tables.len());
            }
            Ok(TableReservations {
                new_schema_bucket,
                resulting_table_size: table.uncompressed_size,
            })
        }
    }
}

struct PackedTables {
    compressed: Vec<u8>,
    streams: Vec<PackedStreamWire>,
    tables: Vec<SchemaTableWire>,
}

#[derive(Clone, Copy)]
struct PackedStreamWire {
    file_offset: u64,
    uncompressed_size: u64,
}

#[derive(Clone, Copy)]
struct SchemaTableWire {
    stream_id: u64,
    stream_offset: u64,
    schema_id: i32,
    message_count: u64,
}

fn pack_tables(
    tables: &[TableBuilder],
    order: &[usize],
    options: WriterOptions,
) -> Result<PackedTables, WriterError> {
    let mut packed = PackedTables {
        compressed: Vec::new(),
        streams: Vec::new(),
        tables: Vec::new(),
    };
    packed
        .streams
        .try_reserve(order.len())
        .map_err(|_| WriterError::AllocationFailed {
            requested: order.len(),
        })?;
    packed
        .tables
        .try_reserve_exact(order.len())
        .map_err(|_| WriterError::AllocationFailed {
            requested: order.len(),
        })?;
    let mut stream_start = 0_usize;
    let mut stream_size = 0_u64;
    for (position, table_index) in order.iter().copied().enumerate() {
        stream_size = stream_size
            .checked_add(usize_u64(tables[table_index].uncompressed_size)?)
            .ok_or(WriterError::SizeOverflow)?;
        let last = position + 1 == order.len();
        let threshold_reached = stream_size > options.minimum_packed_stream_size();
        let next_is_zero = order
            .get(position + 1)
            .is_some_and(|next| 0 == tables[*next].uncompressed_size);
        if last || threshold_reached && !next_is_zero {
            encode_table_stream(
                tables,
                &order[stream_start..=position],
                stream_size,
                options,
                &mut packed,
            )?;
            stream_start = position + 1;
            stream_size = 0;
        }
    }
    Ok(packed)
}

fn encode_table_stream(
    tables: &[TableBuilder],
    indexes: &[usize],
    uncompressed_size: u64,
    options: WriterOptions,
    packed: &mut PackedTables,
) -> Result<(), WriterError> {
    let file_offset = len_u64(&packed.compressed)?;
    let stream_id = usize_u64(packed.streams.len())?;
    let mut stream_offset = 0_u64;
    if 0 == uncompressed_size {
        for index in indexes {
            let table = &tables[*index];
            packed.tables.push(SchemaTableWire {
                stream_id,
                stream_offset: 0,
                schema_id: table.schema_id,
                message_count: table.message_count,
            });
        }
    } else {
        let mut encoder =
            zstd::stream::write::Encoder::new(&mut packed.compressed, options.compression_level())
                .map_err(WriterError::Io)?;
        for index in indexes {
            let table = &tables[*index];
            packed.tables.push(SchemaTableWire {
                stream_id,
                stream_offset,
                schema_id: table.schema_id,
                message_count: table.message_count,
            });
            for column in &table.columns {
                column.write_to(&mut encoder)?;
            }
            stream_offset = stream_offset
                .checked_add(usize_u64(table.uncompressed_size)?)
                .ok_or(WriterError::SizeOverflow)?;
        }
        encoder.finish().map_err(WriterError::Io)?;
    }
    packed.streams.push(PackedStreamWire {
        file_offset,
        uncompressed_size,
    });
    Ok(())
}

fn encode_table_metadata(
    packed: &PackedTables,
    compression_level: i32,
) -> Result<Vec<u8>, WriterError> {
    let capacity = TABLE_METADATA_FIXED_SIZE
        .checked_add(
            packed
                .streams
                .len()
                .checked_mul(PACKED_STREAM_METADATA_SIZE)
                .ok_or(WriterError::SizeOverflow)?,
        )
        .and_then(|size| {
            packed
                .tables
                .len()
                .checked_mul(SCHEMA_TABLE_METADATA_SIZE)
                .and_then(|tables| size.checked_add(tables))
        })
        .ok_or(WriterError::SizeOverflow)?;
    let mut raw = Vec::new();
    raw.try_reserve_exact(capacity)
        .map_err(|_| WriterError::AllocationFailed {
            requested: capacity,
        })?;
    append_u64(&mut raw, usize_u64(packed.streams.len())?);
    for stream in &packed.streams {
        append_u64(&mut raw, stream.file_offset);
        append_u64(&mut raw, stream.uncompressed_size);
    }
    append_u64(&mut raw, 0);
    append_u64(&mut raw, usize_u64(packed.tables.len())?);
    for table in &packed.tables {
        append_u64(&mut raw, table.stream_id);
        append_u64(&mut raw, table.stream_offset);
        raw.extend_from_slice(&table.schema_id.to_le_bytes());
        append_u64(&mut raw, table.message_count);
    }
    debug_assert_eq!(capacity, raw.len());
    zstd::stream::encode_all(raw.as_slice(), compression_level).map_err(WriterError::Io)
}

fn check_section_limits(sections: &[Vec<u8>; 7], limits: WriterLimits) -> Result<(), WriterError> {
    let resources = [
        WriterResource::SchemaTree,
        WriterResource::SchemaMap,
        WriterResource::TableMetadata,
        WriterResource::VariableDictionary,
        WriterResource::LogTypeDictionary,
        WriterResource::ArrayDictionary,
        WriterResource::Tables,
    ];
    for (section, resource) in sections.iter().zip(resources) {
        check_limit(
            resource,
            len_u64(section)?,
            limits.max_section_compressed_size(),
        )?;
    }
    Ok(())
}

fn write_dictionary_encode_bytes<W: Write>(
    writer: &mut W,
    pending: &mut Vec<u8>,
    bytes: &[u8],
) -> Result<(), WriterError> {
    debug_assert!(pending.len() <= DICTIONARY_ENCODE_BUFFER_CAPACITY);
    let remaining = DICTIONARY_ENCODE_BUFFER_CAPACITY - pending.len();
    if bytes.len() <= remaining {
        pending.extend_from_slice(bytes);
        return Ok(());
    }
    if !pending.is_empty() {
        writer.write_all(pending).map_err(WriterError::Io)?;
        pending.clear();
    }
    if bytes.len() >= DICTIONARY_ENCODE_BUFFER_CAPACITY {
        writer.write_all(bytes).map_err(WriterError::Io)
    } else {
        pending.extend_from_slice(bytes);
        Ok(())
    }
}

fn write_u64<W: Write>(writer: &mut W, value: u64) -> Result<(), WriterError> {
    writer
        .write_all(&value.to_le_bytes())
        .map_err(WriterError::Io)
}

fn write_u32<W: Write>(writer: &mut W, value: u32) -> Result<(), WriterError> {
    writer
        .write_all(&value.to_le_bytes())
        .map_err(WriterError::Io)
}

fn write_i32<W: Write>(writer: &mut W, value: i32) -> Result<(), WriterError> {
    writer
        .write_all(&value.to_le_bytes())
        .map_err(WriterError::Io)
}

fn append_u64(output: &mut Vec<u8>, value: u64) {
    output.extend_from_slice(&value.to_le_bytes());
}

const fn check_append_limit(
    resource: AppendResource,
    actual: u64,
    limit: u64,
) -> Result<(), AppendError> {
    if actual > limit {
        Err(AppendError::LimitExceeded {
            resource,
            actual,
            limit,
        })
    } else {
        Ok(())
    }
}

const fn append_allocation(resource: AppendResource, requested: usize) -> AppendError {
    AppendError::AllocationFailed {
        resource,
        requested,
    }
}

fn usize_u64(value: usize) -> Result<u64, WriterError> {
    u64::try_from(value).map_err(|_| WriterError::SizeOverflow)
}

fn usize_u64_append(value: usize) -> Result<u64, AppendError> {
    u64::try_from(value).map_err(|_| AppendError::SizeOverflow)
}

fn hash_bytes(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    let mut index = 0;
    while index < bytes.len() {
        hash ^= u64::from(bytes[index]);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        index += 1;
    }
    hash
}

fn hash_schema(entries: &[u32], ordered_entry_count: usize) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in ordered_entry_count.to_le_bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    for entry in entries {
        for byte in entry.to_le_bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    hash
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use super::*;

    #[test]
    #[cfg(target_pointer_width = "64")]
    fn borrowed_record_planning_values_remain_compact() {
        assert_eq!(64, size_of::<BorrowedScalarValue<'_>>());
        assert_eq!(72, size_of::<BorrowedPlannedValue<'_>>());
    }

    #[derive(Clone)]
    struct ReplayableEvents<'record> {
        events: &'record [RecordEventRef<'record>],
        consume_count: Rc<Cell<usize>>,
    }

    impl<'record> ReplayableRecordEventSource<'record> for ReplayableEvents<'record> {
        type Error = Infallible;

        fn consume<C>(self, consumer: &mut C) -> Result<(), RecordEventAppendError<Self::Error>>
        where
            C: RecordEventConsumer<'record>, {
            self.consume_count.set(self.consume_count.get() + 1);
            for event in self.events.iter().copied() {
                match event {
                    RecordEventRef::Value(field) => consumer.value(field),
                    RecordEventRef::ObjectStart(key) => consumer.object_start(key),
                    RecordEventRef::ObjectEnd => consumer.object_end(),
                }?;
            }
            Ok(())
        }

        fn supports_cached_layout_proof(&self) -> bool {
            true
        }
    }

    fn replayable_events<'record>(
        events: &'record [RecordEventRef<'record>],
        consume_count: &Rc<Cell<usize>>,
    ) -> ReplayableEvents<'record> {
        ReplayableEvents {
            events,
            consume_count: Rc::clone(consume_count),
        }
    }

    fn dictionary_values(dictionary: &DictionaryBuilder) -> Vec<&[u8]> {
        (0..dictionary.entry_count())
            .map(|index| {
                dictionary
                    .entry(u64::try_from(index).expect("test dictionary ID must fit u64"))
                    .expect("test dictionary ID must address an entry")
            })
            .collect()
    }

    fn append_dictionary_values(dictionary: &mut DictionaryBuilder, values: &[&[u8]]) -> Vec<u64> {
        let mut plan = DictionaryPlan::default();
        let mut resulting_value_bytes = dictionary.value_bytes;
        let ids = values
            .iter()
            .map(|value| {
                resolve_dictionary(
                    dictionary,
                    &mut plan,
                    &mut resulting_value_bytes,
                    DictionaryKind::Variable,
                    value,
                    None,
                    WriterLimits::DEFAULT,
                )
                .expect("resolve test dictionary value")
            })
            .collect();
        let reservations =
            prepare_dictionary_reservations(dictionary, &plan, DictionaryKind::Variable)
                .expect("reserve test dictionary commit");
        dictionary.commit(plan, reservations);
        ids
    }

    #[test]
    fn dictionary_arena_preserves_entry_boundaries_and_batched_encoding() {
        let binary = [0_u8, 0xff, 0x11];
        let fixed = [b'x'; 32];
        let long = vec![b'z'; DICTIONARY_ENCODE_BUFFER_CAPACITY + 17];
        let values: [&[u8]; 6] = [b"", b"a", &binary, &fixed, &long, b""];
        let mut dictionary = DictionaryBuilder::default();

        assert_eq!(
            vec![0, 1, 2, 3, 4, 0],
            append_dictionary_values(&mut dictionary, &values)
        );
        assert_eq!(5, dictionary.entry_count());
        assert_eq!(5, dictionary.index.len());
        assert_eq!(
            vec![b"".as_slice(), b"a", &binary, &fixed, &long],
            dictionary_values(&dictionary)
        );
        assert_eq!(
            vec![
                0,
                1,
                1 + binary.len(),
                1 + binary.len() + fixed.len(),
                dictionary.values.len()
            ],
            dictionary.entry_ends
        );

        let encoded = dictionary.encode(1).expect("encode packed dictionary");
        assert_eq!(
            5,
            u64::from_le_bytes(
                encoded[..size_of::<u64>()]
                    .try_into()
                    .expect("dictionary entry count header")
            )
        );
        let decoded = zstd::stream::decode_all(&encoded[size_of::<u64>()..])
            .expect("decode packed dictionary frame");
        let mut expected = Vec::new();
        for value in &values[..5] {
            append_u64(
                &mut expected,
                u64::try_from(value.len()).expect("test value length must fit u64"),
            );
            expected.extend_from_slice(value);
        }
        assert_eq!(expected, decoded);

        let mut independently_hashed = DictionaryBuilder::default();
        append_dictionary_values(&mut independently_hashed, &values);
        assert_eq!(
            encoded,
            independently_hashed
                .encode(1)
                .expect("encode independently hashed dictionary")
        );
    }

    #[test]
    fn dictionary_index_resolves_exact_values_under_forced_collisions_and_rehashes() {
        let mut dictionary = DictionaryBuilder::default();
        dictionary.force_hash(7);
        let first: [&[u8]; 7] = [b"", b"a", b"b", b"c", b"d", b"e", b"c"];
        assert_eq!(
            vec![0, 1, 2, 3, 4, 5, 3],
            append_dictionary_values(&mut dictionary, &first)
        );
        let second: [&[u8]; 10] = [b"f", b"g", b"h", b"i", b"j", b"k", b"l", b"m", b"a", b"m"];
        assert_eq!(
            vec![6, 7, 8, 9, 10, 11, 12, 13, 1, 13],
            append_dictionary_values(&mut dictionary, &second)
        );
        assert_eq!(14, dictionary.entry_count());
        assert_eq!(dictionary.entry_count(), dictionary.index.len());
        for (id, value) in [
            b"".as_slice(),
            b"a",
            b"b",
            b"c",
            b"d",
            b"e",
            b"f",
            b"g",
            b"h",
            b"i",
            b"j",
            b"k",
            b"l",
            b"m",
        ]
        .into_iter()
        .enumerate()
        {
            assert_eq!(
                Some(u64::try_from(id).expect("test dictionary ID must fit u64")),
                dictionary.find(dictionary.hash(value), value)
            );
        }
        assert_eq!(
            None,
            dictionary.find(dictionary.hash(b"missing"), b"missing")
        );
    }

    #[test]
    fn descriptor_domains_accept_their_maxima_and_reject_the_next_value() {
        assert_eq!(Ok(()), validate_log_type_id(MAX_LOG_TYPE_ID));
        assert!(matches!(
            validate_log_type_id(MAX_LOG_TYPE_ID + 1),
            Err(AppendError::FormatDomainExceeded {
                domain: AppendDomain::LogTypeId,
                ..
            })
        ));
        assert_eq!(
            Ok(()),
            validate_encoded_variable_offset(MAX_ENCODED_VARIABLE_OFFSET)
        );
        assert!(matches!(
            validate_encoded_variable_offset(MAX_ENCODED_VARIABLE_OFFSET + 1),
            Err(AppendError::FormatDomainExceeded {
                domain: AppendDomain::EncodedVariableOffset,
                ..
            })
        ));
        assert_eq!(
            u64::MAX,
            encode_descriptor(MAX_LOG_TYPE_ID, MAX_ENCODED_VARIABLE_OFFSET)
        );
        assert_eq!(Ok(i64::MAX), validate_log_event_index(MAX_LOG_EVENT_INDEX));
        assert!(matches!(
            validate_log_event_index(MAX_LOG_EVENT_INDEX + 1),
            Err(AppendError::FormatDomainExceeded {
                domain: AppendDomain::LogEventIndex,
                ..
            })
        ));
    }

    #[test]
    fn encoded_variable_count_rejects_beyond_the_40_bit_addressable_domain() {
        assert!(matches!(
            validate_column_encoded_variables(
                MAX_ENCODED_VARIABLE_COUNT + 1,
                WriterLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX,)
            ),
            Err(AppendError::FormatDomainExceeded {
                domain: AppendDomain::EncodedVariableCount,
                ..
            })
        ));
    }

    #[test]
    fn dictionary_metric_counts_native_ids_and_only_encoded_placeholder_positions() {
        assert_eq!(
            11,
            dictionary_entry_data_size(b"abc", DictionaryKind::Variable).unwrap()
        );
        assert_eq!(
            8 + 3 + 8,
            dictionary_entry_data_size(b"x \x11", DictionaryKind::LogType).unwrap()
        );
        assert_eq!(
            8 + 2 + 8,
            dictionary_entry_data_size(b"\\\x11", DictionaryKind::LogType).unwrap()
        );
        assert_eq!(
            8 + 2 + 8,
            dictionary_entry_data_size(b"\\\\", DictionaryKind::Array).unwrap()
        );
    }

    #[test]
    fn dictionary_hint_columns_preserve_last_fast_path_and_bound_older_fixed_ids() {
        let mut fixed = Vec::new();
        for id in [0, 1, 2, 3] {
            append_u64(&mut fixed, id);
        }
        for node_type in [NodeType::VarString, NodeType::DictionaryFloat] {
            let column = TableColumn {
                node_id: 0,
                node_type,
                data: TableColumnData::Fixed(fixed.clone()),
            };
            assert_eq!(Some(3), column.last_dictionary_id());
            assert_eq!(
                vec![2, 1],
                column.older_fixed_dictionary_ids().collect::<Vec<_>>()
            );
        }

        let mut repeated = Vec::new();
        for id in [0, 1, 1] {
            append_u64(&mut repeated, id);
        }
        let repeated_column = TableColumn {
            node_id: 0,
            node_type: NodeType::VarString,
            data: TableColumnData::Fixed(repeated.clone()),
        };
        assert_eq!(Some(1), repeated_column.last_dictionary_id());
        assert_eq!(
            vec![1, 0],
            repeated_column
                .older_fixed_dictionary_ids()
                .collect::<Vec<_>>()
        );
        let integer_column = TableColumn {
            node_id: 0,
            node_type: NodeType::Integer,
            data: TableColumnData::Fixed(repeated),
        };
        assert_eq!(None, integer_column.last_dictionary_id());
        assert_eq!(
            Vec::<u64>::new(),
            integer_column
                .older_fixed_dictionary_ids()
                .collect::<Vec<_>>()
        );

        let mut descriptors = Vec::new();
        append_u64(&mut descriptors, encode_descriptor(5, 17));
        append_u64(&mut descriptors, encode_descriptor(7, 23));
        for node_type in [NodeType::ClpString, NodeType::UnstructuredArray] {
            let column = TableColumn {
                node_id: 0,
                node_type,
                data: TableColumnData::ClpString {
                    descriptors: descriptors.clone(),
                    encoded_variables: Vec::new(),
                    encoded_variable_count: 0,
                },
            };
            assert_eq!(Some(7), column.last_dictionary_id());
            assert_eq!(
                Vec::<u64>::new(),
                column.older_fixed_dictionary_ids().collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn third_recent_fixed_dictionary_id_resolves_before_hash_lookup() {
        let base = DictionaryBuilder {
            values: b"targetother".to_vec(),
            entry_ends: vec![6, 11],
            value_bytes: 11,
            ..DictionaryBuilder::default()
        };
        let mut encoded_ids = Vec::new();
        for id in [0, 1, 1] {
            append_u64(&mut encoded_ids, id);
        }
        let column = TableColumn {
            node_id: 0,
            node_type: NodeType::VarString,
            data: TableColumnData::Fixed(encoded_ids),
        };
        let mut plan = DictionaryPlan::default();
        let mut resulting_value_bytes = base.value_bytes;

        assert_eq!(
            0,
            resolve_dictionary(
                &base,
                &mut plan,
                &mut resulting_value_bytes,
                DictionaryKind::Variable,
                b"target",
                Some(&column),
                WriterLimits::DEFAULT,
            )
            .expect("third recent dictionary ID should resolve")
        );
        assert_eq!(0, plan.entries.len());
        assert_eq!(base.value_bytes, resulting_value_bytes);
    }

    #[test]
    // The single end-to-end scenario intentionally keeps its state transitions together.
    #[allow(clippy::too_many_lines)]
    fn cyclic_fixed_dictionary_values_keep_canonical_ids_and_limit_atomicity() {
        let initial_values: [&[u8]; 5] = [b"INFO", b"INFO", b"INFO", b"WARN", b"ERROR"];
        let strict_limits = WriterLimits::DEFAULT.with_dictionary_limits(3, 8, 1024, 1024, 8, 8);

        for record_log_order in [false, true] {
            let mut archive = PrimitiveArchive::default();
            let mut event_reference = PrimitiveArchive::default();
            for value in initial_values {
                let fields = [FieldRef::new(b"level", ValueRef::String(value))];
                archive
                    .append(
                        RecordRef::new(&fields),
                        WriterLimits::DEFAULT,
                        record_log_order,
                    )
                    .expect("append cyclic fixed dictionary value");
                event_reference
                    .append_events(
                        [RecordEventRef::value(b"level", ValueRef::String(value))],
                        WriterLimits::DEFAULT,
                        record_log_order,
                    )
                    .expect("append event reference value");
            }

            let reused_fields = [FieldRef::new(b"level", ValueRef::String(b"INFO"))];
            archive
                .append(
                    RecordRef::new(&reused_fields),
                    strict_limits,
                    record_log_order,
                )
                .expect("reuse third recent value at the dictionary limit");
            event_reference
                .append_events(
                    [RecordEventRef::value(b"level", ValueRef::String(b"INFO"))],
                    WriterLimits::DEFAULT,
                    record_log_order,
                )
                .expect("append final event reference value");

            let record_count = archive.record_count();
            let resident_bytes = archive.resident_bytes();
            let encoded_data_size = archive.encoded_data_size();
            let dictionary_arena = archive.variable_dictionary.values.clone();
            let dictionary_ends = archive.variable_dictionary.entry_ends.clone();
            let dictionary_index_len = archive.variable_dictionary.index.len();
            let dictionary_value_bytes = archive.variable_dictionary.value_bytes;
            let rejected_fields = [FieldRef::new(b"level", ValueRef::String(b"DEBUG"))];
            assert!(matches!(
                archive
                    .append(
                        RecordRef::new(&rejected_fields),
                        strict_limits,
                        record_log_order,
                    )
                    .expect_err("a fourth dictionary value must exceed the limit"),
                AppendError::LimitExceeded {
                    resource: AppendResource::VariableDictionaryEntries,
                    actual: 4,
                    limit: 3,
                }
            ));
            assert_eq!(record_count, archive.record_count());
            assert_eq!(resident_bytes, archive.resident_bytes());
            assert_eq!(encoded_data_size, archive.encoded_data_size());
            assert_eq!(dictionary_arena, archive.variable_dictionary.values);
            assert_eq!(dictionary_ends, archive.variable_dictionary.entry_ends);
            assert_eq!(
                dictionary_index_len,
                archive.variable_dictionary.index.len()
            );
            assert_eq!(
                dictionary_value_bytes,
                archive.variable_dictionary.value_bytes
            );
            for (id, value) in [b"INFO".as_slice(), b"WARN", b"ERROR"]
                .into_iter()
                .enumerate()
            {
                assert_eq!(
                    Some(u64::try_from(id).expect("test dictionary ID must fit u64")),
                    archive
                        .variable_dictionary
                        .find(archive.variable_dictionary.hash(value), value)
                );
            }
            assert_eq!(
                None,
                archive
                    .variable_dictionary
                    .find(archive.variable_dictionary.hash(b"DEBUG"), b"DEBUG")
            );
            assert_eq!(
                vec![b"INFO".as_slice(), b"WARN".as_slice(), b"ERROR".as_slice()],
                dictionary_values(&archive.variable_dictionary)
            );

            archive
                .append(
                    RecordRef::new(&rejected_fields),
                    WriterLimits::DEFAULT,
                    record_log_order,
                )
                .expect("append rejected value after relaxing the limit");
            event_reference
                .append_events(
                    [RecordEventRef::value(b"level", ValueRef::String(b"DEBUG"))],
                    WriterLimits::DEFAULT,
                    record_log_order,
                )
                .expect("append final event reference value");
            assert_eq!(
                vec![
                    b"INFO".as_slice(),
                    b"WARN".as_slice(),
                    b"ERROR".as_slice(),
                    b"DEBUG".as_slice()
                ],
                dictionary_values(&archive.variable_dictionary)
            );
            assert_eq!(
                Some(3),
                archive
                    .variable_dictionary
                    .find(archive.variable_dictionary.hash(b"DEBUG"), b"DEBUG")
            );

            let table = archive
                .tables
                .tables
                .first()
                .expect("one stable schema table");
            let column = table
                .columns
                .iter()
                .find(|column| NodeType::VarString == column.node_type)
                .expect("variable dictionary column");
            let TableColumnData::Fixed(encoded_ids) = &column.data else {
                panic!("variable dictionary IDs must use fixed encoding");
            };
            assert_eq!(
                vec![0, 0, 0, 1, 2, 0, 3],
                encoded_ids
                    .as_chunks::<{ size_of::<u64>() }>()
                    .0
                    .iter()
                    .map(|encoded| u64::from_le_bytes(*encoded))
                    .collect::<Vec<_>>()
            );

            let options = WriterOptions::default().with_log_order(record_log_order);
            assert_eq!(
                event_reference
                    .encode_sections(options)
                    .expect("encode event reference"),
                archive
                    .encode_sections(options)
                    .expect("encode archive after rejected append")
            );
        }
    }

    #[test]
    fn cached_proof_entries_cover_empty_objects_and_log_order() {
        let events = [
            RecordEventRef::object_start(b"empty"),
            RecordEventRef::ObjectEnd,
            RecordEventRef::value(b"name", ValueRef::String(b"accepted")),
            RecordEventRef::value(b"missing", ValueRef::Null),
        ];

        for record_log_order in [false, true] {
            let consume_count = Rc::new(Cell::new(0));
            let mut actual = PrimitiveArchive::default();
            let mut expected = PrimitiveArchive::default();
            for _ in 0..3 {
                actual
                    .try_append_replayable_events(
                        replayable_events(&events, &consume_count),
                        WriterLimits::DEFAULT,
                        record_log_order,
                    )
                    .expect("append stable replayable layout");
                expected
                    .append_events(
                        events.iter().copied(),
                        WriterLimits::DEFAULT,
                        record_log_order,
                    )
                    .expect("append ordinary reference layout");
            }

            assert_eq!(3, consume_count.get());
            assert!(actual.last_record_layout_cache_hit);
            assert_eq!(1, actual.schema_count());
            let table = &actual.tables.tables[0];
            assert_eq!(usize::from(record_log_order) + 3, table.entries.len());
            assert_eq!(usize::from(record_log_order) + 1, table.columns.len());
            let options = WriterOptions::default().with_log_order(record_log_order);
            assert_eq!(
                expected.encode_sections(options).expect("encode reference"),
                actual
                    .encode_sections(options)
                    .expect("encode cached proof")
            );
        }
    }

    #[test]
    fn stable_replayable_layout_recycles_bounded_record_plan_vectors() {
        let events = [
            RecordEventRef::object_start(b"empty"),
            RecordEventRef::ObjectEnd,
            RecordEventRef::value(b"name", ValueRef::String(b"accepted")),
            RecordEventRef::value(b"missing", ValueRef::Null),
        ];

        for record_log_order in [false, true] {
            let consume_count = Rc::new(Cell::new(0));
            let mut archive = PrimitiveArchive::default();
            for _ in 0..2 {
                archive
                    .try_append_replayable_events(
                        replayable_events(&events, &consume_count),
                        WriterLimits::DEFAULT,
                        record_log_order,
                    )
                    .expect("arm stable-layout scratch reuse");
            }
            assert!(archive.last_record_layout_cache_hit);
            let entries_capacity = archive.record_plan_scratch.entries.capacity();
            let values_capacity = archive.record_plan_scratch.values.capacity();
            assert!((1..=COMMON_RECORD_FIELD_CAPACITY).contains(&entries_capacity));
            assert!((1..=COMMON_RECORD_FIELD_CAPACITY).contains(&values_capacity));
            let entries_pointer = archive.record_plan_scratch.entries.as_ptr();
            let values_pointer = archive.record_plan_scratch.values.as_ptr();

            archive
                .try_append_replayable_events(
                    replayable_events(&events, &consume_count),
                    WriterLimits::DEFAULT,
                    record_log_order,
                )
                .expect("reuse scratch through cached-layout proof");

            assert_eq!(3, consume_count.get());
            assert_eq!(
                entries_capacity,
                archive.record_plan_scratch.entries.capacity()
            );
            assert_eq!(
                values_capacity,
                archive.record_plan_scratch.values.capacity()
            );
            assert_eq!(
                entries_pointer,
                archive.record_plan_scratch.entries.as_ptr()
            );
            assert_eq!(values_pointer, archive.record_plan_scratch.values.as_ptr());
        }
    }

    #[test]
    fn record_plan_scratch_discards_capacity_beyond_the_common_record_bound() {
        let keys: Vec<String> = (0..=COMMON_RECORD_FIELD_CAPACITY)
            .map(|index| format!("field-{index}"))
            .collect();
        let fields: Vec<FieldRef<'_>> = keys
            .iter()
            .map(|key| FieldRef::new(key.as_bytes(), ValueRef::I64(1)))
            .collect();

        for record_log_order in [false, true] {
            let mut archive = PrimitiveArchive::default();
            for _ in 0..2 {
                archive
                    .append(
                        RecordRef::new(&fields),
                        WriterLimits::DEFAULT,
                        record_log_order,
                    )
                    .expect("append wider stable record");
            }
            assert!(archive.last_record_layout_cache_hit);
            assert_eq!(0, archive.record_plan_scratch.entries.capacity());
            assert_eq!(0, archive.record_plan_scratch.values.capacity());
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn cached_proof_mismatch_replays_and_limit_failure_is_atomic() {
        let original = [
            RecordEventRef::object_start(b"empty"),
            RecordEventRef::ObjectEnd,
            RecordEventRef::value(b"name", ValueRef::String(b"accepted")),
            RecordEventRef::value(b"missing", ValueRef::Null),
        ];
        let reordered = [
            RecordEventRef::value(b"name", ValueRef::String(b"accepted")),
            RecordEventRef::object_start(b"empty"),
            RecordEventRef::ObjectEnd,
            RecordEventRef::value(b"missing", ValueRef::Null),
        ];
        let rejected = [
            RecordEventRef::value(b"name", ValueRef::String(b"rejected")),
            RecordEventRef::object_start(b"empty"),
            RecordEventRef::ObjectEnd,
            RecordEventRef::value(b"missing", ValueRef::Null),
        ];
        let strict_dictionary_limit =
            WriterLimits::DEFAULT.with_dictionary_limits(1, 8, 1024, 1024, 8, 8);

        for record_log_order in [false, true] {
            let consume_count = Rc::new(Cell::new(0));
            let mut actual = PrimitiveArchive::default();
            let mut expected = PrimitiveArchive::default();
            for _ in 0..2 {
                actual
                    .try_append_replayable_events(
                        replayable_events(&original, &consume_count),
                        WriterLimits::DEFAULT,
                        record_log_order,
                    )
                    .expect("arm stable-layout proof");
                expected
                    .append_events(
                        original.iter().copied(),
                        WriterLimits::DEFAULT,
                        record_log_order,
                    )
                    .expect("append original reference layout");
            }
            assert_eq!(2, consume_count.get());
            assert!(actual.last_record_layout_cache_hit);

            actual
                .try_append_replayable_events(
                    replayable_events(&reordered, &consume_count),
                    WriterLimits::DEFAULT,
                    record_log_order,
                )
                .expect("replay mismatched layout through full validation");
            expected
                .append_events(
                    reordered.iter().copied(),
                    WriterLimits::DEFAULT,
                    record_log_order,
                )
                .expect("append reordered reference layout");
            assert_eq!(4, consume_count.get());
            assert!(!actual.last_record_layout_cache_hit);

            actual
                .try_append_replayable_events(
                    replayable_events(&reordered, &consume_count),
                    WriterLimits::DEFAULT,
                    record_log_order,
                )
                .expect("arm reordered stable-layout proof");
            expected
                .append_events(
                    reordered.iter().copied(),
                    WriterLimits::DEFAULT,
                    record_log_order,
                )
                .expect("append second reordered reference layout");
            assert_eq!(5, consume_count.get());
            assert!(actual.last_record_layout_cache_hit);

            let record_count = actual.record_count();
            let resident_bytes = actual.resident_bytes();
            let encoded_data_size = actual.encoded_data_size();
            let error = actual
                .try_append_replayable_events(
                    replayable_events(&rejected, &consume_count),
                    strict_dictionary_limit,
                    record_log_order,
                )
                .expect_err("new dictionary value must exceed the limit");
            assert!(matches!(
                error,
                RecordEventAppendError::Append(AppendError::LimitExceeded {
                    resource: AppendResource::VariableDictionaryEntries,
                    actual: 2,
                    limit: 1,
                })
            ));
            assert_eq!(7, consume_count.get());
            assert_eq!(record_count, actual.record_count());
            assert_eq!(resident_bytes, actual.resident_bytes());
            assert_eq!(encoded_data_size, actual.encoded_data_size());
            assert!(actual.last_record_layout_cache_hit);

            actual
                .try_append_replayable_events(
                    replayable_events(&reordered, &consume_count),
                    strict_dictionary_limit,
                    record_log_order,
                )
                .expect("append cached value after rejected proof");
            expected
                .append_events(
                    reordered.iter().copied(),
                    WriterLimits::DEFAULT,
                    record_log_order,
                )
                .expect("append final reference layout");
            assert_eq!(8, consume_count.get());

            let options = WriterOptions::default().with_log_order(record_log_order);
            assert_eq!(
                expected.encode_sections(options).expect("encode reference"),
                actual
                    .encode_sections(options)
                    .expect("encode after rejected proof")
            );
        }
    }
}
