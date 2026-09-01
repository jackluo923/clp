//! Streaming, typed aggregations over archive search matches.
//!
//! Aggregations consume [`ArchiveTableMatches`] directly. Numeric and dictionary-backed scalar
//! columns are read without marshalling a record as JSON; CLP strings and timestamp scalars are
//! reconstructed only when they are the selected aggregation field.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::fmt::Display;
use std::fmt::Formatter;
use std::io;
use std::str;

use serde::Serialize;
use serde::Serializer;
use serde::ser::SerializeMap;

use super::ArchiveMatchSink;
use super::ArchiveTableMatches;
use super::ColumnNamespace;
use super::Projection;
use super::ProjectionError;
use super::ProjectionLimits;
use super::projection::ResolvedProjection;
use crate::archive::ClpStringColumn;
use crate::archive::Column;
use crate::archive::ColumnData;
use crate::archive::DictionaryIdColumn;
use crate::archive::EncodedVariableError;
use crate::archive::NodeType;
use crate::archive::SchemaTree;
use crate::archive::TimestampColumn;
use crate::ingest::ClassifiedJsonNumber;
use crate::ingest::classify_json_number;
use crate::json::JsonEscapeError;
use crate::json::JsonEscapeLimits;
use crate::json::NlohmannFloatError;
use crate::json::append_json_string;
use crate::json::format_nlohmann_float;
use crate::timestamp_catalog::TimestampCatalogFormatError;
use crate::timestamp_catalog::TimestampPatternCatalog;

const NANOSECONDS_PER_MILLISECOND: i64 = 1_000_000;
const MILLISECONDS_PER_SECOND: f64 = 1_000.0;
const I64_UPPER_BOUND_AS_F64: f64 = 9_223_372_036_854_775_808.0;
const I64_MIN_AS_F64: f64 = -9_223_372_036_854_775_808.0;

/// Kind of aggregation compiled by an [`AggregationPlan`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum AggregationKind {
    /// Number of matching records.
    Count,
    /// Number of matching records in fixed epoch-millisecond buckets.
    CountByTime,
    /// Minimum numeric value of one field.
    Minimum,
    /// Maximum numeric value of one field.
    Maximum,
    /// Distinct scalar values of one field.
    Unique,
}

/// Resource bounds for compiling and executing one aggregation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AggregationLimits {
    projection: ProjectionLimits,
    time_buckets: usize,
    unique_values: usize,
    unique_string_bytes: usize,
    reconstructed_string_bytes: usize,
}

impl AggregationLimits {
    /// Creates explicit field-resolution, bucket, unique-value, and string bounds.
    #[must_use]
    pub const fn new(
        projection: ProjectionLimits,
        max_time_buckets: usize,
        max_unique_values: usize,
        max_total_unique_string_bytes: usize,
        max_reconstructed_string_bytes: usize,
    ) -> Self {
        Self {
            projection,
            time_buckets: max_time_buckets,
            unique_values: max_unique_values,
            unique_string_bytes: max_total_unique_string_bytes,
            reconstructed_string_bytes: max_reconstructed_string_bytes,
        }
    }

    /// Field-descriptor parsing and schema-resolution limits.
    #[must_use]
    pub const fn projection(self) -> ProjectionLimits {
        self.projection
    }

    /// Maximum distinct time buckets retained by count-by-time.
    #[must_use]
    pub const fn max_time_buckets(self) -> usize {
        self.time_buckets
    }

    /// Maximum distinct scalar values retained by unique.
    #[must_use]
    pub const fn max_unique_values(self) -> usize {
        self.unique_values
    }

    /// Maximum aggregate UTF-8 bytes retained by unique string values.
    #[must_use]
    pub const fn max_total_unique_string_bytes(self) -> usize {
        self.unique_string_bytes
    }

    /// Maximum bytes reconstructed for one CLP or timestamp string value.
    #[must_use]
    pub const fn max_reconstructed_string_bytes(self) -> usize {
        self.reconstructed_string_bytes
    }
}

impl Default for AggregationLimits {
    fn default() -> Self {
        Self::new(
            ProjectionLimits::default(),
            1_048_576,
            1_048_576,
            256 * 1024 * 1024,
            16 * 1024 * 1024,
        )
    }
}

/// A reusable, archive-independent aggregation request.
///
/// Compile a plan once, then call [`Self::start`] for each archive. Per-archive state and resolved
/// schema node IDs live in the returned sink rather than in the reusable plan.
#[derive(Clone, Debug)]
pub struct AggregationPlan {
    operation: PlanOperation,
    limits: AggregationLimits,
}

impl AggregationPlan {
    /// Compiles a record count using default limits.
    #[must_use]
    pub fn count() -> Self {
        Self {
            operation: PlanOperation::Count,
            limits: AggregationLimits::default(),
        }
    }

    /// Compiles a fixed-width epoch-millisecond count using default limits.
    ///
    /// # Errors
    ///
    /// Returns [`AggregationPlanError::InvalidBucketSize`] when `bucket_size_milliseconds` is not
    /// positive.
    pub fn count_by_time(bucket_size_milliseconds: i64) -> Result<Self, AggregationPlanError> {
        Self::count_by_time_with_limits(bucket_size_milliseconds, AggregationLimits::default())
    }

    /// Compiles a fixed-width epoch-millisecond count with explicit limits.
    ///
    /// # Errors
    ///
    /// Returns [`AggregationPlanError::InvalidBucketSize`] when `bucket_size_milliseconds` is not
    /// positive.
    pub const fn count_by_time_with_limits(
        bucket_size_milliseconds: i64,
        limits: AggregationLimits,
    ) -> Result<Self, AggregationPlanError> {
        if bucket_size_milliseconds <= 0 {
            return Err(AggregationPlanError::InvalidBucketSize {
                bucket_size_milliseconds,
            });
        }
        Ok(Self {
            operation: PlanOperation::CountByTime {
                bucket_size_milliseconds,
            },
            limits,
        })
    }

    /// Compiles a numeric minimum using the C++ aggregation field grammar and default limits.
    ///
    /// # Errors
    ///
    /// Returns a field syntax, namespace, wildcard, resource, or allocation error.
    pub fn minimum(field: &str) -> Result<Self, AggregationPlanError> {
        Self::minimum_with_limits(field, AggregationLimits::default())
    }

    /// Compiles a numeric minimum with explicit limits.
    ///
    /// # Errors
    ///
    /// Returns a field syntax, namespace, wildcard, resource, or allocation error.
    pub fn minimum_with_limits(
        field: &str,
        limits: AggregationLimits,
    ) -> Result<Self, AggregationPlanError> {
        Ok(Self {
            operation: PlanOperation::Extreme {
                find_maximum: false,
                field: AggregationField::compile(field, limits.projection())?,
            },
            limits,
        })
    }

    /// Compiles a numeric maximum using the C++ aggregation field grammar and default limits.
    ///
    /// # Errors
    ///
    /// Returns a field syntax, namespace, wildcard, resource, or allocation error.
    pub fn maximum(field: &str) -> Result<Self, AggregationPlanError> {
        Self::maximum_with_limits(field, AggregationLimits::default())
    }

    /// Compiles a numeric maximum with explicit limits.
    ///
    /// # Errors
    ///
    /// Returns a field syntax, namespace, wildcard, resource, or allocation error.
    pub fn maximum_with_limits(
        field: &str,
        limits: AggregationLimits,
    ) -> Result<Self, AggregationPlanError> {
        Ok(Self {
            operation: PlanOperation::Extreme {
                find_maximum: true,
                field: AggregationField::compile(field, limits.projection())?,
            },
            limits,
        })
    }

    /// Compiles a distinct scalar-value aggregation using default limits.
    ///
    /// # Errors
    ///
    /// Returns a field syntax, namespace, wildcard, resource, or allocation error.
    pub fn unique(field: &str) -> Result<Self, AggregationPlanError> {
        Self::unique_with_limits(field, AggregationLimits::default())
    }

    /// Compiles a distinct scalar-value aggregation with explicit limits.
    ///
    /// # Errors
    ///
    /// Returns a field syntax, namespace, wildcard, resource, or allocation error.
    pub fn unique_with_limits(
        field: &str,
        limits: AggregationLimits,
    ) -> Result<Self, AggregationPlanError> {
        Ok(Self {
            operation: PlanOperation::Unique {
                field: AggregationField::compile(field, limits.projection())?,
            },
            limits,
        })
    }

    /// Returns the compiled operation kind.
    #[must_use]
    pub const fn kind(&self) -> AggregationKind {
        match self.operation {
            PlanOperation::Count => AggregationKind::Count,
            PlanOperation::CountByTime { .. } => AggregationKind::CountByTime,
            PlanOperation::Extreme {
                find_maximum: false,
                ..
            } => AggregationKind::Minimum,
            PlanOperation::Extreme {
                find_maximum: true, ..
            } => AggregationKind::Maximum,
            PlanOperation::Unique { .. } => AggregationKind::Unique,
        }
    }

    /// Returns the execution limits retained by this plan.
    #[must_use]
    pub const fn limits(&self) -> AggregationLimits {
        self.limits
    }

    /// Starts independent state for one archive search.
    #[must_use]
    pub fn start(&self) -> AggregationSink<'_> {
        let state = match self.operation {
            PlanOperation::Count => AggregationState::Count(0),
            PlanOperation::CountByTime { .. } => AggregationState::CountByTime(BTreeMap::new()),
            PlanOperation::Extreme { .. } => AggregationState::Extreme(None),
            PlanOperation::Unique { .. } => AggregationState::Unique(UniqueValues::default()),
        };
        AggregationSink {
            plan: self,
            state,
            resolved_nodes: None,
            byte_scratch: Vec::new(),
            timestamp_scratch: String::new(),
            decoded_string_scratch: String::new(),
        }
    }
}

#[derive(Clone, Debug)]
enum PlanOperation {
    Count,
    CountByTime {
        bucket_size_milliseconds: i64,
    },
    Extreme {
        find_maximum: bool,
        field: AggregationField,
    },
    Unique {
        field: AggregationField,
    },
}

#[derive(Clone, Debug)]
struct AggregationField {
    source: String,
    projection: Projection,
}

impl AggregationField {
    fn compile(source: &str, limits: ProjectionLimits) -> Result<Self, AggregationPlanError> {
        let projection =
            Projection::selected(&[source], limits).map_err(AggregationPlanError::InvalidField)?;
        let column = projection
            .selected_columns()
            .and_then(|columns| columns.first())
            .ok_or(AggregationPlanError::MissingField)?;
        if ColumnNamespace::Default != column.namespace() {
            return Err(AggregationPlanError::UnsupportedNamespace {
                namespace: column.namespace(),
            });
        }
        let mut owned = String::new();
        owned.try_reserve_exact(source.len()).map_err(|_| {
            AggregationPlanError::AllocationFailed {
                requested: source.len(),
            }
        })?;
        owned.push_str(source);
        Ok(Self {
            source: owned,
            projection,
        })
    }
}

/// Failure while compiling an aggregation request.
#[derive(Debug)]
#[non_exhaustive]
pub enum AggregationPlanError {
    /// Count-by-time requires a positive bucket size.
    InvalidBucketSize {
        /// Rejected epoch-millisecond bucket size.
        bucket_size_milliseconds: i64,
    },
    /// The aggregation field was empty, malformed, wildcarded, or exceeded a configured limit.
    InvalidField(ProjectionError),
    /// Aggregation fields are restricted to the ordinary/default namespace.
    UnsupportedNamespace {
        /// Rejected namespace.
        namespace: ColumnNamespace,
    },
    /// A parsed field unexpectedly contained no descriptor.
    MissingField,
    /// A bounded plan allocation failed.
    AllocationFailed {
        /// Requested byte count.
        requested: usize,
    },
}

impl Display for AggregationPlanError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidBucketSize {
                bucket_size_milliseconds,
            } => write!(
                formatter,
                "count-by-time bucket size must be positive, got {bucket_size_milliseconds}"
            ),
            Self::InvalidField(source) => write!(formatter, "invalid aggregation field: {source}"),
            Self::UnsupportedNamespace { namespace } => write!(
                formatter,
                "aggregation fields must use the default namespace, not {namespace:?}"
            ),
            Self::MissingField => formatter.write_str("aggregation field is empty"),
            Self::AllocationFailed { requested } => write!(
                formatter,
                "failed to allocate {requested} byte(s) for the aggregation plan"
            ),
        }
    }
}

impl Error for AggregationPlanError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidField(source) => Some(source),
            Self::InvalidBucketSize { .. }
            | Self::UnsupportedNamespace { .. }
            | Self::MissingField
            | Self::AllocationFailed { .. } => None,
        }
    }
}

/// Per-archive aggregation state and typed search-match sink.
///
/// After [`super::search_archive`] succeeds, [`Self::results`] streams borrowed result records in
/// the exact ordering used by the C++ aggregators. Zero matches produce no result records.
pub struct AggregationSink<'plan> {
    plan: &'plan AggregationPlan,
    state: AggregationState,
    resolved_nodes: Option<Vec<u32>>,
    byte_scratch: Vec<u8>,
    timestamp_scratch: String,
    decoded_string_scratch: String,
}

impl AggregationSink<'_> {
    /// Returns the reusable plan backing this per-archive state.
    #[must_use]
    pub const fn plan(&self) -> &AggregationPlan {
        self.plan
    }

    /// Streams borrowed result records without first collecting a result vector.
    #[must_use = "iterators are lazy and do nothing unless consumed"]
    pub fn results(&self) -> AggregationResults<'_> {
        let inner = match (&self.plan.operation, &self.state) {
            (PlanOperation::Count, AggregationState::Count(0)) => ResultsInner::Empty,
            (PlanOperation::Count, AggregationState::Count(count)) => {
                ResultsInner::Count(Some(*count))
            }
            (PlanOperation::CountByTime { .. }, AggregationState::CountByTime(counts)) => {
                ResultsInner::CountByTime(counts.iter())
            }
            (
                PlanOperation::Extreme {
                    find_maximum,
                    field,
                },
                AggregationState::Extreme(extreme),
            ) => ResultsInner::Extreme {
                field: &field.source,
                find_maximum: *find_maximum,
                value: *extreme,
            },
            (PlanOperation::Unique { field }, AggregationState::Unique(values)) => {
                ResultsInner::Unique(UniqueResults::new(&field.source, values))
            }
            _ => ResultsInner::Empty,
        };
        AggregationResults { inner }
    }

    fn prepare_field(
        &mut self,
        matches: ArchiveTableMatches<'_, '_, '_>,
        field: &AggregationField,
    ) -> Result<(), AggregationError> {
        if self.resolved_nodes.is_some() {
            return Ok(());
        }
        let resolved = field
            .projection
            .resolve(matches.catalog().schema_tree())
            .map_err(AggregationError::FieldResolution)?;
        let ResolvedProjection::Selected(mut nodes) = resolved else {
            return Err(AggregationError::InvalidResolvedField);
        };
        retain_object_reachable_nodes(&mut nodes, matches.catalog().schema_tree())?;
        self.resolved_nodes = Some(nodes);
        Ok(())
    }

    fn write_aggregation(
        &mut self,
        matches: ArchiveTableMatches<'_, '_, '_>,
    ) -> Result<(), AggregationError> {
        match &self.plan.operation {
            PlanOperation::Count => {
                let AggregationState::Count(count) = &mut self.state else {
                    return Err(AggregationError::InvalidState);
                };
                *count = add_match_count(*count, matches.bitmap().match_count())?;
            }
            PlanOperation::CountByTime {
                bucket_size_milliseconds,
            } => {
                let AggregationState::CountByTime(counts) = &mut self.state else {
                    return Err(AggregationError::InvalidState);
                };
                count_by_time(matches, *bucket_size_milliseconds, self.plan.limits, counts)?;
            }
            PlanOperation::Extreme {
                find_maximum,
                field,
            } => {
                self.prepare_field(matches, field)?;
                let nodes = self
                    .resolved_nodes
                    .as_deref()
                    .ok_or(AggregationError::InvalidResolvedField)?;
                let AggregationState::Extreme(extreme) = &mut self.state else {
                    return Err(AggregationError::InvalidState);
                };
                scan_extreme(
                    matches,
                    nodes,
                    *find_maximum,
                    extreme,
                    &mut self.timestamp_scratch,
                )?;
            }
            PlanOperation::Unique { field } => {
                self.prepare_field(matches, field)?;
                let nodes = self
                    .resolved_nodes
                    .as_deref()
                    .ok_or(AggregationError::InvalidResolvedField)?;
                let AggregationState::Unique(values) = &mut self.state else {
                    return Err(AggregationError::InvalidState);
                };
                let mut scratch = StringScratch {
                    bytes: &mut self.byte_scratch,
                    timestamp: &mut self.timestamp_scratch,
                    decoded: &mut self.decoded_string_scratch,
                };
                scan_unique(matches, nodes, self.plan.limits, values, &mut scratch)?;
            }
        }
        Ok(())
    }
}

fn retain_object_reachable_nodes(
    nodes: &mut Vec<u32>,
    schema_tree: &SchemaTree,
) -> Result<(), AggregationError> {
    let mut retained = 0;
    for index in 0..nodes.len() {
        let node_id = nodes[index];
        if !has_structured_array_ancestor(node_id, schema_tree)? {
            nodes[retained] = node_id;
            retained += 1;
        }
    }
    nodes.truncate(retained);
    Ok(())
}

fn has_structured_array_ancestor(
    node_id: u32,
    schema_tree: &SchemaTree,
) -> Result<bool, AggregationError> {
    let node_id = usize::try_from(node_id).map_err(|_| AggregationError::SizeOverflow)?;
    let mut ancestor_id = schema_tree
        .get(node_id)
        .ok_or(AggregationError::SizeOverflow)?
        .parent_id();
    while let Some(node_id) = ancestor_id {
        let ancestor = schema_tree
            .get(node_id)
            .ok_or(AggregationError::SizeOverflow)?;
        if NodeType::StructuredArray == ancestor.node_type() {
            return Ok(true);
        }
        ancestor_id = ancestor.parent_id();
    }
    Ok(false)
}

impl ArchiveMatchSink for AggregationSink<'_> {
    fn write_matches(&mut self, matches: ArchiveTableMatches<'_, '_, '_>) -> io::Result<()> {
        self.write_aggregation(matches).map_err(io::Error::other)
    }
}

#[derive(Debug)]
enum AggregationState {
    Count(i64),
    CountByTime(BTreeMap<i64, i64>),
    Extreme(Option<AggregationNumber>),
    Unique(UniqueValues),
}

/// Numeric value retained by min or max.
#[derive(Clone, Copy, Debug, PartialEq)]
#[non_exhaustive]
pub enum AggregationNumber {
    /// Signed 64-bit JSON integer.
    Integer(i64),
    /// Finite binary64 JSON number.
    Float(f64),
}

impl Serialize for AggregationNumber {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Integer(value) => serializer.serialize_i64(*value),
            Self::Float(value) => serializer.serialize_f64(*value),
        }
    }
}

/// Borrowed scalar in a unique result.
#[derive(Clone, Copy, Debug, PartialEq)]
#[non_exhaustive]
pub enum AggregationValueRef<'a> {
    /// Signed 64-bit JSON integer.
    Integer(i64),
    /// Finite binary64 JSON number.
    Float(f64),
    /// UTF-8 JSON string value.
    String(&'a str),
    /// JSON Boolean value.
    Boolean(bool),
}

impl Serialize for AggregationValueRef<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Integer(value) => serializer.serialize_i64(*value),
            Self::Float(value) => serializer.serialize_f64(*value),
            Self::String(value) => serializer.serialize_str(value),
            Self::Boolean(value) => serializer.serialize_bool(*value),
        }
    }
}

/// One borrowed typed aggregation result document, excluding the archive ID supplied by a sink.
#[derive(Clone, Copy, Debug, PartialEq)]
#[non_exhaustive]
pub enum AggregationResultRef<'a> {
    /// One nonzero archive count.
    Count {
        /// Number of matching records.
        count: i64,
    },
    /// One nonempty time bucket.
    CountByTime {
        /// Bucket start in epoch milliseconds.
        timestamp: i64,
        /// Number of matching records in the bucket.
        count: i64,
    },
    /// One numeric minimum.
    Minimum {
        /// Original escaped aggregation field descriptor.
        field: &'a str,
        /// Minimum value.
        value: AggregationNumber,
    },
    /// One numeric maximum.
    Maximum {
        /// Original escaped aggregation field descriptor.
        field: &'a str,
        /// Maximum value.
        value: AggregationNumber,
    },
    /// One distinct scalar value.
    Unique {
        /// Original escaped aggregation field descriptor.
        field: &'a str,
        /// Distinct value.
        value: AggregationValueRef<'a>,
    },
}

impl<'a> AggregationResultRef<'a> {
    /// Wraps this result with an archive ID for direct result-document serialization.
    #[must_use]
    pub const fn with_archive_id(self, archive_id: &'a str) -> AggregationResultDocument<'a> {
        AggregationResultDocument {
            archive_id,
            result: self,
        }
    }
}

/// Serializable aggregation result with the archive identity required by C++ stdout/results-cache
/// documents.
///
/// The [`Serialize`] implementation emits keys in the same lexicographic order as
/// `nlohmann::json`'s default object type. A serializer remains free to choose its own binary64
/// notation; use [`Self::append_compact_json`] when byte-for-byte C++ stdout compatibility is
/// required.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AggregationResultDocument<'a> {
    archive_id: &'a str,
    result: AggregationResultRef<'a>,
}

impl AggregationResultDocument<'_> {
    /// Returns the archive ID attached to this result.
    #[must_use]
    pub const fn archive_id(&self) -> &str {
        self.archive_id
    }

    /// Returns the typed aggregation result.
    #[must_use]
    pub const fn result(&self) -> AggregationResultRef<'_> {
        self.result
    }

    /// Appends the exact compact JSON document emitted by the C++ stdout aggregation sink.
    ///
    /// This includes nlohmann-json's binary64 notation thresholds and exponent spelling. The
    /// destination is restored to its original length on every error.
    ///
    /// # Errors
    ///
    /// Returns a bounded JSON-string escaping, size-overflow, number-format, or allocation error.
    pub fn append_compact_json(
        &self,
        destination: &mut String,
    ) -> Result<(), AggregationJsonError> {
        let original_len = destination.len();
        let result = self.append_compact_json_inner(destination);
        if result.is_err() {
            destination.truncate(original_len);
        }
        result
    }

    fn append_compact_json_inner(
        &self,
        destination: &mut String,
    ) -> Result<(), AggregationJsonError> {
        append_literal(destination, r#"{"archive_id":"#)?;
        append_json_string(self.archive_id, destination, JsonEscapeLimits::default())?;
        match self.result {
            AggregationResultRef::Count { count } => {
                append_literal(destination, ",\"count\":")?;
                append_i64(destination, count)?;
            }
            AggregationResultRef::CountByTime { timestamp, count } => {
                append_literal(destination, ",\"count\":")?;
                append_i64(destination, count)?;
                append_literal(destination, ",\"timestamp\":")?;
                append_i64(destination, timestamp)?;
            }
            AggregationResultRef::Minimum { field, value } => {
                append_field(destination, field)?;
                append_literal(destination, ",\"min\":")?;
                append_number(destination, value)?;
            }
            AggregationResultRef::Maximum { field, value } => {
                append_field(destination, field)?;
                append_literal(destination, ",\"max\":")?;
                append_number(destination, value)?;
            }
            AggregationResultRef::Unique { field, value } => {
                append_field(destination, field)?;
                append_literal(destination, ",\"value\":")?;
                append_value(destination, value)?;
            }
        }
        append_literal(destination, "}")
    }
}

impl Serialize for AggregationResultDocument<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self.result {
            AggregationResultRef::Count { count } => {
                let mut map = serializer.serialize_map(Some(2))?;
                map.serialize_entry("archive_id", self.archive_id)?;
                map.serialize_entry("count", &count)?;
                map.end()
            }
            AggregationResultRef::CountByTime { timestamp, count } => {
                let mut map = serializer.serialize_map(Some(3))?;
                map.serialize_entry("archive_id", self.archive_id)?;
                map.serialize_entry("count", &count)?;
                map.serialize_entry("timestamp", &timestamp)?;
                map.end()
            }
            AggregationResultRef::Minimum { field, value } => {
                let mut map = serializer.serialize_map(Some(3))?;
                map.serialize_entry("archive_id", self.archive_id)?;
                map.serialize_entry("field", field)?;
                map.serialize_entry("min", &value)?;
                map.end()
            }
            AggregationResultRef::Maximum { field, value } => {
                let mut map = serializer.serialize_map(Some(3))?;
                map.serialize_entry("archive_id", self.archive_id)?;
                map.serialize_entry("field", field)?;
                map.serialize_entry("max", &value)?;
                map.end()
            }
            AggregationResultRef::Unique { field, value } => {
                let mut map = serializer.serialize_map(Some(3))?;
                map.serialize_entry("archive_id", self.archive_id)?;
                map.serialize_entry("field", field)?;
                map.serialize_entry("value", &value)?;
                map.end()
            }
        }
    }
}

fn append_field(destination: &mut String, field: &str) -> Result<(), AggregationJsonError> {
    append_literal(destination, ",\"field\":")?;
    append_json_string(field, destination, JsonEscapeLimits::default())?;
    Ok(())
}

fn append_value(
    destination: &mut String,
    value: AggregationValueRef<'_>,
) -> Result<(), AggregationJsonError> {
    match value {
        AggregationValueRef::Integer(value) => append_i64(destination, value),
        AggregationValueRef::Float(value) => append_f64(destination, value),
        AggregationValueRef::String(value) => {
            append_json_string(value, destination, JsonEscapeLimits::default())?;
            Ok(())
        }
        AggregationValueRef::Boolean(true) => append_literal(destination, "true"),
        AggregationValueRef::Boolean(false) => append_literal(destination, "false"),
    }
}

fn append_number(
    destination: &mut String,
    value: AggregationNumber,
) -> Result<(), AggregationJsonError> {
    match value {
        AggregationNumber::Integer(value) => append_i64(destination, value),
        AggregationNumber::Float(value) => append_f64(destination, value),
    }
}

fn append_i64(destination: &mut String, value: i64) -> Result<(), AggregationJsonError> {
    let mut buffer = itoa::Buffer::new();
    append_literal(destination, buffer.format(value))
}

fn append_f64(destination: &mut String, value: f64) -> Result<(), AggregationJsonError> {
    let formatted = format_nlohmann_float(value).map_err(AggregationJsonError::from)?;
    append_literal(destination, formatted.as_str())
}

fn append_literal(destination: &mut String, value: &str) -> Result<(), AggregationJsonError> {
    destination
        .len()
        .checked_add(value.len())
        .ok_or(AggregationJsonError::SizeOverflow)?;
    destination.try_reserve_exact(value.len()).map_err(|_| {
        AggregationJsonError::AllocationFailed {
            requested: value.len(),
        }
    })?;
    destination.push_str(value);
    Ok(())
}

/// Borrowed, allocation-free iterator over completed aggregation results.
pub struct AggregationResults<'a> {
    inner: ResultsInner<'a>,
}

enum ResultsInner<'a> {
    Empty,
    Count(Option<i64>),
    CountByTime(std::collections::btree_map::Iter<'a, i64, i64>),
    Extreme {
        field: &'a str,
        find_maximum: bool,
        value: Option<AggregationNumber>,
    },
    Unique(UniqueResults<'a>),
}

impl<'a> Iterator for AggregationResults<'a> {
    type Item = AggregationResultRef<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        match &mut self.inner {
            ResultsInner::Empty => None,
            ResultsInner::Count(count) => count
                .take()
                .map(|count| AggregationResultRef::Count { count }),
            ResultsInner::CountByTime(counts) => counts
                .next()
                .map(|(&timestamp, &count)| AggregationResultRef::CountByTime { timestamp, count }),
            ResultsInner::Extreme {
                field,
                find_maximum,
                value,
            } => value.take().map(|value| {
                if *find_maximum {
                    AggregationResultRef::Maximum { field, value }
                } else {
                    AggregationResultRef::Minimum { field, value }
                }
            }),
            ResultsInner::Unique(values) => values.next(),
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let length = match &self.inner {
            ResultsInner::Empty => 0,
            ResultsInner::Count(value) => usize::from(value.is_some()),
            ResultsInner::CountByTime(values) => values.len(),
            ResultsInner::Extreme { value, .. } => usize::from(value.is_some()),
            ResultsInner::Unique(values) => values.len(),
        };
        (length, Some(length))
    }
}

impl ExactSizeIterator for AggregationResults<'_> {}

#[derive(Clone, Copy, Debug)]
struct FiniteFloat(f64);

impl FiniteFloat {
    const fn new(value: f64) -> Self {
        Self(value)
    }
}

impl PartialEq for FiniteFloat {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl Eq for FiniteFloat {}

impl PartialOrd for FiniteFloat {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for FiniteFloat {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.partial_cmp(&other.0).unwrap_or(Ordering::Equal)
    }
}

#[derive(Debug, Default)]
struct UniqueValues {
    integers: BTreeSet<i64>,
    floats: BTreeSet<FiniteFloat>,
    strings: BTreeSet<String>,
    booleans: u8,
    string_bytes: usize,
}

impl UniqueValues {
    fn len(&self) -> usize {
        self.integers.len()
            + self.floats.len()
            + self.strings.len()
            + usize::from(0 != self.booleans & 1)
            + usize::from(0 != self.booleans & 2)
    }

    fn check_new_value(&self, limits: AggregationLimits) -> Result<(), AggregationError> {
        let required = self
            .len()
            .checked_add(1)
            .ok_or(AggregationError::SizeOverflow)?;
        if required > limits.max_unique_values() {
            return Err(AggregationError::LimitExceeded {
                resource: AggregationResource::UniqueValues,
                actual: required,
                limit: limits.max_unique_values(),
            });
        }
        Ok(())
    }

    fn insert_integer(
        &mut self,
        value: i64,
        limits: AggregationLimits,
    ) -> Result<(), AggregationError> {
        if self.integers.contains(&value) {
            return Ok(());
        }
        self.check_new_value(limits)?;
        self.integers.insert(value);
        Ok(())
    }

    fn insert_float(
        &mut self,
        value: f64,
        limits: AggregationLimits,
    ) -> Result<(), AggregationError> {
        let value = FiniteFloat::new(value);
        if self.floats.contains(&value) {
            return Ok(());
        }
        self.check_new_value(limits)?;
        self.floats.insert(value);
        Ok(())
    }

    fn insert_string(
        &mut self,
        value: &str,
        limits: AggregationLimits,
    ) -> Result<(), AggregationError> {
        if self.strings.contains(value) {
            return Ok(());
        }
        self.check_new_value(limits)?;
        let required = self
            .string_bytes
            .checked_add(value.len())
            .ok_or(AggregationError::SizeOverflow)?;
        if required > limits.max_total_unique_string_bytes() {
            return Err(AggregationError::LimitExceeded {
                resource: AggregationResource::UniqueStringBytes,
                actual: required,
                limit: limits.max_total_unique_string_bytes(),
            });
        }
        let mut owned = String::new();
        owned
            .try_reserve_exact(value.len())
            .map_err(|_| AggregationError::AllocationFailed {
                resource: AggregationResource::UniqueStringBytes,
                requested: value.len(),
            })?;
        owned.push_str(value);
        self.strings.insert(owned);
        self.string_bytes = required;
        Ok(())
    }

    fn insert_boolean(
        &mut self,
        value: bool,
        limits: AggregationLimits,
    ) -> Result<(), AggregationError> {
        let bit = if value { 2 } else { 1 };
        if 0 != self.booleans & bit {
            return Ok(());
        }
        self.check_new_value(limits)?;
        self.booleans |= bit;
        Ok(())
    }
}

struct UniqueResults<'a> {
    field: &'a str,
    integers: std::collections::btree_set::Iter<'a, i64>,
    floats: std::collections::btree_set::Iter<'a, FiniteFloat>,
    strings: std::collections::btree_set::Iter<'a, String>,
    booleans: u8,
}

impl<'a> UniqueResults<'a> {
    fn new(field: &'a str, values: &'a UniqueValues) -> Self {
        Self {
            field,
            integers: values.integers.iter(),
            floats: values.floats.iter(),
            strings: values.strings.iter(),
            booleans: values.booleans,
        }
    }

    fn len(&self) -> usize {
        self.integers.len()
            + self.floats.len()
            + self.strings.len()
            + usize::from(0 != self.booleans & 1)
            + usize::from(0 != self.booleans & 2)
    }

    fn next(&mut self) -> Option<AggregationResultRef<'a>> {
        let value = if let Some(&value) = self.integers.next() {
            AggregationValueRef::Integer(value)
        } else if let Some(value) = self.floats.next() {
            AggregationValueRef::Float(value.0)
        } else if let Some(value) = self.strings.next() {
            AggregationValueRef::String(value)
        } else if 0 != self.booleans & 1 {
            self.booleans &= !1;
            AggregationValueRef::Boolean(false)
        } else if 0 != self.booleans & 2 {
            self.booleans &= !2;
            AggregationValueRef::Boolean(true)
        } else {
            return None;
        };
        Some(AggregationResultRef::Unique {
            field: self.field,
            value,
        })
    }
}

fn add_match_count(current: i64, additional: usize) -> Result<i64, AggregationError> {
    let additional = i64::try_from(additional).map_err(|_| AggregationError::CountOverflow)?;
    current
        .checked_add(additional)
        .ok_or(AggregationError::CountOverflow)
}

#[allow(clippy::cast_possible_truncation)]
fn count_by_time(
    matches: ArchiveTableMatches<'_, '_, '_>,
    bucket_size: i64,
    limits: AggregationLimits,
    counts: &mut BTreeMap<i64, i64>,
) -> Result<(), AggregationError> {
    let timestamp_column = authoritative_timestamp_column(matches)?;
    let Some(column) = timestamp_column else {
        return add_bucket_count(counts, 0, matches.bitmap().match_count(), limits);
    };
    let bitmap = matches.bitmap().as_bytes();
    match column.data() {
        ColumnData::Timestamp(values) => {
            for (row, ((value, _), matched)) in values.encoded_values().zip(bitmap).enumerate() {
                if 0 != *matched {
                    let milliseconds = value / NANOSECONDS_PER_MILLISECOND;
                    add_timestamp(counts, milliseconds, bucket_size, limits, row)?;
                }
            }
        }
        ColumnData::DeprecatedDateString(values) => {
            for (row, (value, matched)) in values.epochs().iter().zip(bitmap).enumerate() {
                if 0 != *matched {
                    add_timestamp(counts, value, bucket_size, limits, row)?;
                }
            }
        }
        ColumnData::Integer(values) => {
            for (row, (value, matched)) in values.iter().zip(bitmap).enumerate() {
                if 0 != *matched {
                    add_timestamp(counts, value, bucket_size, limits, row)?;
                }
            }
        }
        ColumnData::DeltaInteger(values) => {
            for (row, (value, matched)) in values.values().zip(bitmap).enumerate() {
                if 0 != *matched {
                    add_timestamp(counts, value, bucket_size, limits, row)?;
                }
            }
        }
        ColumnData::Float(values) => {
            for (row, (value, matched)) in values.iter().zip(bitmap).enumerate() {
                if 0 != *matched {
                    let milliseconds = (value * MILLISECONDS_PER_SECOND) as i64;
                    add_timestamp(counts, milliseconds, bucket_size, limits, row)?;
                }
            }
        }
        ColumnData::FormattedFloat(_)
        | ColumnData::DictionaryFloat(_)
        | ColumnData::Boolean(_)
        | ColumnData::VarString(_)
        | ColumnData::ClpString(_)
        | ColumnData::UnstructuredArray(_) => {
            return Err(AggregationError::InvalidTimestampColumn {
                node_id: column.node_id(),
            });
        }
    }
    Ok(())
}

fn authoritative_timestamp_column<'stream, 'archive>(
    matches: ArchiveTableMatches<'_, 'stream, 'archive>,
) -> Result<Option<Column<'stream, 'archive>>, AggregationError> {
    let Some(authoritative) = matches
        .catalog()
        .metadata()
        .timestamp_dictionary()
        .authoritative_range()
    else {
        return Ok(None);
    };
    let ordered_entries = matches.table().schema().ordered_entry_count();
    let mut selected = None;
    for column in matches.table().table().columns() {
        if column.schema_entry_index() >= ordered_entries {
            continue;
        }
        let node_id =
            i32::try_from(column.node_id()).map_err(|_| AggregationError::SizeOverflow)?;
        if authoritative.column_ids().contains(&node_id) {
            // The C++ schema reader marks columns in schema order and retains the last match.
            selected = Some(*column);
        }
    }
    Ok(selected)
}

fn add_timestamp(
    counts: &mut BTreeMap<i64, i64>,
    timestamp: i64,
    bucket_size: i64,
    limits: AggregationLimits,
    _row: usize,
) -> Result<(), AggregationError> {
    // Signed integer division truncates toward zero in both Rust and C++. This is intentionally
    // not Euclidean/floor bucketing for negative timestamps.
    let bucket = (timestamp / bucket_size)
        .checked_mul(bucket_size)
        .ok_or(AggregationError::SizeOverflow)?;
    add_bucket_count(counts, bucket, 1, limits)
}

fn add_bucket_count(
    counts: &mut BTreeMap<i64, i64>,
    bucket: i64,
    additional: usize,
    limits: AggregationLimits,
) -> Result<(), AggregationError> {
    if !counts.contains_key(&bucket) && counts.len() >= limits.max_time_buckets() {
        return Err(AggregationError::LimitExceeded {
            resource: AggregationResource::TimeBuckets,
            actual: counts.len().saturating_add(1),
            limit: limits.max_time_buckets(),
        });
    }
    let count = counts.entry(bucket).or_default();
    *count = add_match_count(*count, additional)?;
    Ok(())
}

fn scan_extreme(
    matches: ArchiveTableMatches<'_, '_, '_>,
    nodes: &[u32],
    find_maximum: bool,
    extreme: &mut Option<AggregationNumber>,
    timestamp_scratch: &mut String,
) -> Result<(), AggregationError> {
    let bitmap = matches.bitmap().as_bytes();
    for column in target_columns(matches, nodes) {
        match column.data() {
            ColumnData::Integer(values) => {
                for (value, matched) in values.iter().zip(bitmap) {
                    if 0 != *matched {
                        update_extreme(extreme, AggregationNumber::Integer(value), find_maximum);
                    }
                }
            }
            ColumnData::DeltaInteger(values) => {
                for (value, matched) in values.values().zip(bitmap) {
                    if 0 != *matched {
                        update_extreme(extreme, AggregationNumber::Integer(value), find_maximum);
                    }
                }
            }
            ColumnData::Float(values) => {
                for (value, matched) in values.iter().zip(bitmap) {
                    if 0 != *matched {
                        update_extreme(extreme, AggregationNumber::Float(value), find_maximum);
                    }
                }
            }
            ColumnData::FormattedFloat(values) => {
                for (value, matched) in values.values().iter().zip(bitmap) {
                    if 0 != *matched {
                        update_extreme(extreme, AggregationNumber::Float(value), find_maximum);
                    }
                }
            }
            ColumnData::DictionaryFloat(values) => {
                for (row, matched) in bitmap.iter().enumerate() {
                    if 0 != *matched {
                        let value =
                            parse_dictionary_float(values.value(row), column.node_id(), row)?;
                        update_extreme(extreme, AggregationNumber::Float(value), find_maximum);
                    }
                }
            }
            ColumnData::Timestamp(values) => {
                for (row, ((epoch, pattern_id), matched)) in
                    values.encoded_values().zip(bitmap).enumerate()
                {
                    if 0 == *matched {
                        continue;
                    }
                    timestamp_scratch.clear();
                    matches
                        .catalog()
                        .timestamp_patterns()
                        .append_epoch_nanoseconds(pattern_id, epoch, timestamp_scratch)
                        .map_err(|source| AggregationError::TimestampFormat {
                            node_id: column.node_id(),
                            row_index: row,
                            source,
                        })?;
                    if timestamp_scratch.starts_with('"') {
                        continue;
                    }
                    let candidate =
                        classify_number(timestamp_scratch.as_bytes()).ok_or_else(|| {
                            AggregationError::InvalidTimestampScalar {
                                node_id: column.node_id(),
                                row_index: row,
                            }
                        })?;
                    update_extreme(extreme, candidate, find_maximum);
                }
            }
            ColumnData::Boolean(_)
            | ColumnData::VarString(_)
            | ColumnData::ClpString(_)
            | ColumnData::UnstructuredArray(_)
            | ColumnData::DeprecatedDateString(_) => {}
        }
    }
    Ok(())
}

fn update_extreme(
    extreme: &mut Option<AggregationNumber>,
    candidate: AggregationNumber,
    find_maximum: bool,
) {
    let should_replace = extreme.is_none_or(|current| {
        if find_maximum {
            number_is_less(current, candidate)
        } else {
            number_is_less(candidate, current)
        }
    });
    if should_replace {
        *extreme = Some(candidate);
    }
}

fn number_is_less(left: AggregationNumber, right: AggregationNumber) -> bool {
    match (left, right) {
        (AggregationNumber::Integer(left), AggregationNumber::Integer(right)) => left < right,
        (AggregationNumber::Float(left), AggregationNumber::Float(right)) => left < right,
        (AggregationNumber::Integer(left), AggregationNumber::Float(right)) => {
            integer_is_less_than_float(left, right)
        }
        (AggregationNumber::Float(left), AggregationNumber::Integer(right)) => {
            float_is_less_than_integer(left, right)
        }
    }
}

#[allow(clippy::cast_possible_truncation)]
fn integer_is_less_than_float(left: i64, right: f64) -> bool {
    if right.is_nan() {
        return false;
    }
    if right >= I64_UPPER_BOUND_AS_F64 {
        return true;
    }
    if right < I64_MIN_AS_F64 {
        return false;
    }
    let truncated = right.trunc();
    let right_integer = truncated as i64;
    if left != right_integer {
        return left < right_integer;
    }
    right > truncated
}

#[allow(clippy::cast_possible_truncation)]
fn float_is_less_than_integer(left: f64, right: i64) -> bool {
    if left.is_nan() {
        return false;
    }
    if left >= I64_UPPER_BOUND_AS_F64 {
        return false;
    }
    if left < I64_MIN_AS_F64 {
        return true;
    }
    let truncated = left.trunc();
    let left_integer = truncated as i64;
    if left_integer != right {
        return left_integer < right;
    }
    left < truncated
}

struct StringScratch<'a> {
    bytes: &'a mut Vec<u8>,
    timestamp: &'a mut String,
    decoded: &'a mut String,
}

fn scan_unique(
    matches: ArchiveTableMatches<'_, '_, '_>,
    nodes: &[u32],
    limits: AggregationLimits,
    values: &mut UniqueValues,
    scratch: &mut StringScratch<'_>,
) -> Result<(), AggregationError> {
    let bitmap = matches.bitmap().as_bytes();
    for column in target_columns(matches, nodes) {
        match column.data() {
            ColumnData::Integer(column_values) => {
                for (value, matched) in column_values.iter().zip(bitmap) {
                    if 0 != *matched {
                        values.insert_integer(value, limits)?;
                    }
                }
            }
            ColumnData::DeltaInteger(column_values) => {
                for (value, matched) in column_values.values().zip(bitmap) {
                    if 0 != *matched {
                        values.insert_integer(value, limits)?;
                    }
                }
            }
            ColumnData::Float(column_values) => {
                for (value, matched) in column_values.iter().zip(bitmap) {
                    if 0 != *matched {
                        values.insert_float(value, limits)?;
                    }
                }
            }
            ColumnData::FormattedFloat(column_values) => {
                for (value, matched) in column_values.values().iter().zip(bitmap) {
                    if 0 != *matched {
                        values.insert_float(value, limits)?;
                    }
                }
            }
            ColumnData::DictionaryFloat(column_values) => {
                scan_unique_dictionary_float(
                    column_values,
                    column.node_id(),
                    bitmap,
                    limits,
                    values,
                )?;
            }
            ColumnData::Boolean(column_values) => {
                for (value, matched) in column_values.iter().zip(bitmap) {
                    if 0 != *matched {
                        values.insert_boolean(value, limits)?;
                    }
                }
            }
            ColumnData::VarString(column_values) => {
                scan_unique_var_string(column_values, column.node_id(), bitmap, limits, values)?;
            }
            ColumnData::ClpString(column_values) => {
                scan_unique_clp_string(
                    column_values,
                    column.node_id(),
                    bitmap,
                    limits,
                    values,
                    scratch.bytes,
                )?;
            }
            ColumnData::Timestamp(column_values) => {
                scan_unique_timestamp(
                    column_values,
                    column.node_id(),
                    bitmap,
                    matches.catalog().timestamp_patterns(),
                    limits,
                    values,
                    scratch,
                )?;
            }
            ColumnData::UnstructuredArray(_) | ColumnData::DeprecatedDateString(_) => {}
        }
    }
    Ok(())
}

fn scan_unique_dictionary_float(
    column: DictionaryIdColumn<'_, '_>,
    node_id: u32,
    bitmap: &[u8],
    limits: AggregationLimits,
    values: &mut UniqueValues,
) -> Result<(), AggregationError> {
    for (row, matched) in bitmap.iter().enumerate() {
        if 0 != *matched {
            values.insert_float(
                parse_dictionary_float(column.value(row), node_id, row)?,
                limits,
            )?;
        }
    }
    Ok(())
}

fn scan_unique_var_string(
    column: DictionaryIdColumn<'_, '_>,
    node_id: u32,
    bitmap: &[u8],
    limits: AggregationLimits,
    values: &mut UniqueValues,
) -> Result<(), AggregationError> {
    for (row, matched) in bitmap.iter().enumerate() {
        if 0 == *matched {
            continue;
        }
        let raw = column
            .value(row)
            .ok_or(AggregationError::MissingColumnValue {
                node_id,
                row_index: row,
            })?;
        let value = str::from_utf8(raw).map_err(|source| AggregationError::InvalidStringUtf8 {
            node_id,
            row_index: row,
            source,
        })?;
        values.insert_string(value, limits)?;
    }
    Ok(())
}

fn scan_unique_clp_string(
    column: ClpStringColumn<'_, '_>,
    node_id: u32,
    bitmap: &[u8],
    limits: AggregationLimits,
    values: &mut UniqueValues,
    scratch: &mut Vec<u8>,
) -> Result<(), AggregationError> {
    for (row, matched) in bitmap.iter().enumerate() {
        if 0 == *matched {
            continue;
        }
        let record = column
            .record(row)
            .ok_or(AggregationError::MissingColumnValue {
                node_id,
                row_index: row,
            })?;
        scratch.clear();
        crate::archive::append_clp_message_bounded(
            record.logtype(),
            column.variable_dictionary(),
            &record.encoded_variables(),
            scratch,
            limits.max_reconstructed_string_bytes(),
        )
        .map_err(|source| AggregationError::ClpString {
            node_id,
            row_index: row,
            source,
        })?;
        let value =
            str::from_utf8(scratch).map_err(|source| AggregationError::InvalidStringUtf8 {
                node_id,
                row_index: row,
                source,
            })?;
        values.insert_string(value, limits)?;
    }
    Ok(())
}

fn scan_unique_timestamp(
    column: TimestampColumn<'_, '_>,
    node_id: u32,
    bitmap: &[u8],
    patterns: &TimestampPatternCatalog,
    limits: AggregationLimits,
    values: &mut UniqueValues,
    scratch: &mut StringScratch<'_>,
) -> Result<(), AggregationError> {
    for (row, ((epoch, pattern_id), matched)) in column.encoded_values().zip(bitmap).enumerate() {
        if 0 == *matched {
            continue;
        }
        scratch.timestamp.clear();
        patterns
            .append_epoch_nanoseconds(pattern_id, epoch, scratch.timestamp)
            .map_err(|source| AggregationError::TimestampFormat {
                node_id,
                row_index: row,
                source,
            })?;
        if scratch.timestamp.starts_with('"') {
            decode_json_string(
                scratch.timestamp,
                scratch.decoded,
                limits.max_reconstructed_string_bytes(),
            )
            .map_err(|reason| AggregationError::InvalidTimestampString {
                node_id,
                row_index: row,
                reason,
            })?;
            values.insert_string(scratch.decoded, limits)?;
        } else {
            let value = classify_number(scratch.timestamp.as_bytes()).ok_or(
                AggregationError::InvalidTimestampScalar {
                    node_id,
                    row_index: row,
                },
            )?;
            match value {
                AggregationNumber::Integer(value) => values.insert_integer(value, limits)?,
                AggregationNumber::Float(value) => values.insert_float(value, limits)?,
            }
        }
    }
    Ok(())
}

fn target_columns<'table, 'stream, 'archive>(
    matches: ArchiveTableMatches<'table, 'stream, 'archive>,
    nodes: &'table [u32],
) -> impl Iterator<Item = Column<'stream, 'archive>> + 'table {
    matches
        .table()
        .table()
        .columns()
        .iter()
        .copied()
        .filter(move |column| nodes.binary_search(&column.node_id()).is_ok())
}

fn parse_dictionary_float(
    value: Option<&[u8]>,
    node_id: u32,
    row_index: usize,
) -> Result<f64, AggregationError> {
    value
        .and_then(|value| str::from_utf8(value).ok())
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| value.is_finite())
        .ok_or(AggregationError::InvalidDictionaryFloat { node_id, row_index })
}

fn classify_number(value: &[u8]) -> Option<AggregationNumber> {
    match classify_json_number(value).ok()? {
        ClassifiedJsonNumber::Integer(value) => Some(AggregationNumber::Integer(value)),
        ClassifiedJsonNumber::Float { value, .. } => Some(AggregationNumber::Float(value)),
    }
}

fn decode_json_string(
    source: &str,
    destination: &mut String,
    limit: usize,
) -> Result<(), TimestampStringError> {
    if source.len() < 2 || !source.starts_with('"') || !source.ends_with('"') {
        return Err(TimestampStringError::InvalidQuotes);
    }
    let content = source
        .get(1..source.len() - 1)
        .ok_or(TimestampStringError::InvalidQuotes)?;
    if content.len() > limit {
        return Err(TimestampStringError::LimitExceeded {
            actual: content.len(),
            limit,
        });
    }
    destination.clear();
    destination
        .try_reserve(content.len())
        .map_err(|_| TimestampStringError::AllocationFailed {
            requested: content.len(),
        })?;
    let bytes = content.as_bytes();
    let mut cursor = 0_usize;
    let mut literal_start = 0_usize;
    while cursor < bytes.len() {
        if b'\\' != bytes[cursor] {
            cursor += 1;
            continue;
        }
        destination.push_str(
            content
                .get(literal_start..cursor)
                .ok_or(TimestampStringError::InvalidEscape { offset: cursor })?,
        );
        let escape_offset = cursor;
        cursor += 1;
        let escaped = *bytes
            .get(cursor)
            .ok_or(TimestampStringError::InvalidEscape {
                offset: escape_offset,
            })?;
        cursor += 1;
        match escaped {
            b'"' => destination.push('"'),
            b'\\' => destination.push('\\'),
            b'/' => destination.push('/'),
            b'b' => destination.push('\u{0008}'),
            b'f' => destination.push('\u{000c}'),
            b'n' => destination.push('\n'),
            b'r' => destination.push('\r'),
            b't' => destination.push('\t'),
            b'u' => {
                let (character, next) = decode_unicode_escape(content, cursor, escape_offset)?;
                destination.push(character);
                cursor = next;
            }
            _ => {
                return Err(TimestampStringError::InvalidEscape {
                    offset: escape_offset,
                });
            }
        }
        literal_start = cursor;
    }
    destination.push_str(content.get(literal_start..).ok_or(
        TimestampStringError::InvalidEscape {
            offset: literal_start,
        },
    )?);
    if destination.len() > limit {
        return Err(TimestampStringError::LimitExceeded {
            actual: destination.len(),
            limit,
        });
    }
    Ok(())
}

fn decode_unicode_escape(
    content: &str,
    digits_start: usize,
    escape_offset: usize,
) -> Result<(char, usize), TimestampStringError> {
    let first_end = digits_start
        .checked_add(4)
        .ok_or(TimestampStringError::InvalidEscape {
            offset: escape_offset,
        })?;
    let first = parse_hex_quad(content, digits_start, escape_offset)?;
    if !(0xd800..=0xdfff).contains(&first) {
        let character =
            char::from_u32(u32::from(first)).ok_or(TimestampStringError::InvalidEscape {
                offset: escape_offset,
            })?;
        return Ok((character, first_end));
    }
    if !(0xd800..=0xdbff).contains(&first) {
        return Err(TimestampStringError::InvalidEscape {
            offset: escape_offset,
        });
    }
    let bytes = content.as_bytes();
    if bytes.get(first_end..first_end + 2) != Some(b"\\u") {
        return Err(TimestampStringError::InvalidEscape {
            offset: escape_offset,
        });
    }
    let second_start = first_end + 2;
    let second = parse_hex_quad(content, second_start, escape_offset)?;
    if !(0xdc00..=0xdfff).contains(&second) {
        return Err(TimestampStringError::InvalidEscape {
            offset: escape_offset,
        });
    }
    let scalar = 0x1_0000 + ((u32::from(first) - 0xd800) << 10) + (u32::from(second) - 0xdc00);
    let character = char::from_u32(scalar).ok_or(TimestampStringError::InvalidEscape {
        offset: escape_offset,
    })?;
    Ok((character, second_start + 4))
}

fn parse_hex_quad(
    content: &str,
    start: usize,
    escape_offset: usize,
) -> Result<u16, TimestampStringError> {
    let end = start
        .checked_add(4)
        .ok_or(TimestampStringError::InvalidEscape {
            offset: escape_offset,
        })?;
    let digits = content
        .get(start..end)
        .ok_or(TimestampStringError::InvalidEscape {
            offset: escape_offset,
        })?;
    u16::from_str_radix(digits, 16).map_err(|_| TimestampStringError::InvalidEscape {
        offset: escape_offset,
    })
}

/// Failure while appending an exact compact C++ aggregation result document.
#[derive(Debug)]
#[non_exhaustive]
pub enum AggregationJsonError {
    /// Escaping one UTF-8 archive ID, field, or value failed.
    String(JsonEscapeError),
    /// Checked destination-size arithmetic overflowed.
    SizeOverflow,
    /// Reserving bounded destination capacity failed.
    AllocationFailed {
        /// Requested additional bytes.
        requested: usize,
    },
    /// An aggregation unexpectedly contained a non-finite binary64 value.
    NonFiniteFloat,
    /// The internal shortest-float representation was malformed or exceeded its fixed bound.
    InvalidFloatFormat,
}

impl From<NlohmannFloatError> for AggregationJsonError {
    fn from(source: NlohmannFloatError) -> Self {
        match source {
            NlohmannFloatError::NonFinite => Self::NonFiniteFloat,
            NlohmannFloatError::InvalidFormat => Self::InvalidFloatFormat,
        }
    }
}

impl From<JsonEscapeError> for AggregationJsonError {
    fn from(source: JsonEscapeError) -> Self {
        Self::String(source)
    }
}

impl Display for AggregationJsonError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::String(source) => {
                write!(formatter, "failed to escape aggregation JSON: {source}")
            }
            Self::SizeOverflow => formatter.write_str("aggregation JSON size overflow"),
            Self::AllocationFailed { requested } => write!(
                formatter,
                "failed to allocate {requested} byte(s) for aggregation JSON"
            ),
            Self::NonFiniteFloat => {
                formatter.write_str("aggregation JSON cannot represent a non-finite float")
            }
            Self::InvalidFloatFormat => {
                formatter.write_str("failed to format an aggregation float")
            }
        }
    }
}

impl Error for AggregationJsonError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::String(source) => Some(source),
            Self::SizeOverflow
            | Self::AllocationFailed { .. }
            | Self::NonFiniteFloat
            | Self::InvalidFloatFormat => None,
        }
    }
}

/// Resource named by a bounded aggregation failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum AggregationResource {
    /// Distinct count-by-time buckets.
    TimeBuckets,
    /// Distinct scalar values.
    UniqueValues,
    /// Aggregate retained unique string bytes.
    UniqueStringBytes,
}

impl Display for AggregationResource {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::TimeBuckets => "count-by-time buckets",
            Self::UniqueValues => "unique values",
            Self::UniqueStringBytes => "unique string bytes",
        })
    }
}

/// Invalid JSON string emitted by a corrupt timestamp pattern/value pair.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum TimestampStringError {
    /// The formatted scalar did not have matching JSON quotes.
    InvalidQuotes,
    /// A JSON escape was malformed.
    InvalidEscape {
        /// Byte offset within the unquoted content.
        offset: usize,
    },
    /// One reconstructed value exceeded its configured bound.
    LimitExceeded {
        /// Required bytes.
        actual: usize,
        /// Configured limit.
        limit: usize,
    },
    /// Reserving bounded decoded-string storage failed.
    AllocationFailed {
        /// Requested bytes.
        requested: usize,
    },
}

impl Display for TimestampStringError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidQuotes => formatter.write_str("timestamp scalar is not a JSON string"),
            Self::InvalidEscape { offset } => {
                write!(formatter, "invalid JSON string escape at byte {offset}")
            }
            Self::LimitExceeded { actual, limit } => write!(
                formatter,
                "decoded timestamp string requires {actual} bytes, exceeding limit {limit}"
            ),
            Self::AllocationFailed { requested } => write!(
                formatter,
                "failed to allocate {requested} bytes for a decoded timestamp string"
            ),
        }
    }
}

impl Error for TimestampStringError {}

/// Failure while consuming typed search matches into an aggregation.
#[derive(Debug)]
#[non_exhaustive]
pub enum AggregationError {
    /// Resolving the compiled aggregation field against an archive failed.
    FieldResolution(ProjectionError),
    /// A configured retained-state limit was exceeded.
    LimitExceeded {
        /// Bounded resource.
        resource: AggregationResource,
        /// Required count or byte size.
        actual: usize,
        /// Configured limit.
        limit: usize,
    },
    /// A bounded allocation failed.
    AllocationFailed {
        /// Resource being allocated.
        resource: AggregationResource,
        /// Requested elements or bytes.
        requested: usize,
    },
    /// A count exceeded the signed 64-bit result domain used by C++.
    CountOverflow,
    /// Checked size or bucket arithmetic overflowed.
    SizeOverflow,
    /// The archive's authoritative timestamp ID resolved to an unsupported column type.
    InvalidTimestampColumn {
        /// Schema-tree node ID.
        node_id: u32,
    },
    /// A dictionary-float value could not be decoded despite table validation.
    InvalidDictionaryFloat {
        /// Schema-tree node ID.
        node_id: u32,
        /// Table-local row index.
        row_index: usize,
    },
    /// A selected column lacked a value despite table validation.
    MissingColumnValue {
        /// Schema-tree node ID.
        node_id: u32,
        /// Table-local row index.
        row_index: usize,
    },
    /// A selected string column contained invalid UTF-8.
    InvalidStringUtf8 {
        /// Schema-tree node ID.
        node_id: u32,
        /// Table-local row index.
        row_index: usize,
        /// UTF-8 validation failure.
        source: str::Utf8Error,
    },
    /// Reconstructing a selected CLP string failed.
    ClpString {
        /// Schema-tree node ID.
        node_id: u32,
        /// Table-local row index.
        row_index: usize,
        /// CLP descriptor/variable failure.
        source: EncodedVariableError,
    },
    /// Formatting a current timestamp scalar failed.
    TimestampFormat {
        /// Schema-tree node ID.
        node_id: u32,
        /// Table-local row index.
        row_index: usize,
        /// Pattern-catalog failure.
        source: TimestampCatalogFormatError,
    },
    /// A numeric timestamp pattern emitted a non-number JSON scalar.
    InvalidTimestampScalar {
        /// Schema-tree node ID.
        node_id: u32,
        /// Table-local row index.
        row_index: usize,
    },
    /// A quoted timestamp pattern emitted an invalid JSON string.
    InvalidTimestampString {
        /// Schema-tree node ID.
        node_id: u32,
        /// Table-local row index.
        row_index: usize,
        /// String decoding failure.
        reason: TimestampStringError,
    },
    /// Field resolution did not yield selected schema nodes.
    InvalidResolvedField,
    /// Plan and sink state variants disagreed.
    InvalidState,
}

impl Display for AggregationError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::FieldResolution(source) => {
                write!(formatter, "failed to resolve aggregation field: {source}")
            }
            Self::LimitExceeded {
                resource,
                actual,
                limit,
            } => write!(
                formatter,
                "aggregation {resource} requires {actual}, exceeding limit {limit}"
            ),
            Self::AllocationFailed {
                resource,
                requested,
            } => write!(
                formatter,
                "failed to allocate {requested} element(s) for aggregation {resource}"
            ),
            Self::CountOverflow => formatter.write_str("aggregation count exceeds signed 64-bit"),
            Self::SizeOverflow => formatter.write_str("aggregation size arithmetic overflow"),
            Self::InvalidTimestampColumn { node_id } => write!(
                formatter,
                "authoritative timestamp node {node_id} has an unsupported decoded type"
            ),
            Self::InvalidDictionaryFloat { node_id, row_index } => write!(
                formatter,
                "dictionary-float node {node_id}, row {row_index} is invalid"
            ),
            Self::MissingColumnValue { node_id, row_index } => write!(
                formatter,
                "aggregation node {node_id}, row {row_index} has no decoded value"
            ),
            Self::InvalidStringUtf8 {
                node_id, row_index, ..
            } => write!(
                formatter,
                "aggregation string node {node_id}, row {row_index} is not UTF-8"
            ),
            Self::ClpString {
                node_id,
                row_index,
                source,
            } => write!(
                formatter,
                "failed to reconstruct CLP string node {node_id}, row {row_index}: {source}"
            ),
            Self::TimestampFormat {
                node_id,
                row_index,
                source,
            } => write!(
                formatter,
                "failed to format timestamp node {node_id}, row {row_index}: {source}"
            ),
            Self::InvalidTimestampScalar { node_id, row_index } => write!(
                formatter,
                "timestamp node {node_id}, row {row_index} did not format as a JSON scalar"
            ),
            Self::InvalidTimestampString {
                node_id,
                row_index,
                reason,
            } => write!(
                formatter,
                "timestamp node {node_id}, row {row_index} formatted an invalid string: {reason}"
            ),
            Self::InvalidResolvedField => {
                formatter.write_str("aggregation field resolution produced no selected-node set")
            }
            Self::InvalidState => formatter.write_str("aggregation plan and state disagree"),
        }
    }
}

impl Error for AggregationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::FieldResolution(source) => Some(source),
            Self::InvalidStringUtf8 { source, .. } => Some(source),
            Self::ClpString { source, .. } => Some(source),
            Self::TimestampFormat { source, .. } => Some(source),
            Self::InvalidTimestampString { reason, .. } => Some(reason),
            Self::LimitExceeded { .. }
            | Self::AllocationFailed { .. }
            | Self::CountOverflow
            | Self::SizeOverflow
            | Self::InvalidTimestampColumn { .. }
            | Self::InvalidDictionaryFloat { .. }
            | Self::MissingColumnValue { .. }
            | Self::InvalidTimestampScalar { .. }
            | Self::InvalidResolvedField
            | Self::InvalidState => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::archive::ArchiveCatalogLimits;
    use crate::archive::SingleFileArchiveReader;
    use crate::search::ArchiveSearchOptions;
    use crate::search::KqlLimits;
    use crate::search::parse_kql;
    use crate::search::search_archive;

    const CPP_AGGREGATION_FIXTURE: &[u8] =
        include_bytes!("../../tests/fixtures/sfa-v0.5.0-aggregations-cpp.bin");
    const CPP_MINIMAL_FIXTURE: &[u8] =
        include_bytes!("../../tests/fixtures/sfa-v0.5.0-minimal-cpp.bin");
    const CPP_RETAINED_FLOATS_FIXTURE: &[u8] =
        include_bytes!("../../tests/fixtures/sfa-v0.5.0-retained-floats-cpp.bin");
    const CPP_STRINGS_FIXTURE: &[u8] =
        include_bytes!("../../tests/fixtures/sfa-v0.5.0-strings-cpp.bin");
    const CPP_STRUCTURED_ARRAY_FIXTURE_HEX: &str =
        include_str!("../../tests/fixtures/sfa-v0.5.0-structured-arrays-cpp.hex");
    const CPP_TIMESTAMPS_FIXTURE: &[u8] =
        include_bytes!("../../tests/fixtures/sfa-v0.5.0-timestamps-cpp.bin");

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

    fn aggregate<'a>(
        fixture: &[u8],
        query: &str,
        plan: &'a AggregationPlan,
    ) -> AggregationSink<'a> {
        let query = parse_kql(query, KqlLimits::default()).expect("parse aggregation query");
        let mut reader =
            SingleFileArchiveReader::open(Cursor::new(fixture)).expect("open C++ fixture");
        let mut sink = plan.start();
        search_archive(
            &mut reader,
            &query,
            &mut sink,
            &ArchiveSearchOptions::default(),
        )
        .expect("aggregate C++ fixture");
        sink
    }

    fn json_lines(sink: &AggregationSink<'_>, archive_id: &str) -> Vec<String> {
        sink.results()
            .map(|result| {
                let mut output = String::new();
                result
                    .with_archive_id(archive_id)
                    .append_compact_json(&mut output)
                    .expect("serialize exact aggregation document");
                output
            })
            .collect()
    }

    fn assert_json_lines(sink: &AggregationSink<'_>, archive_id: &str, expected: &[&str]) {
        let actual = json_lines(sink, archive_id);
        assert_eq!(expected.len(), actual.len());
        for (expected, actual) in expected.iter().zip(actual) {
            assert_eq!(*expected, actual);
        }
    }

    #[test]
    fn count_and_zero_matches_follow_cpp_empty_result_policy() {
        let plan = AggregationPlan::count();
        let sink = aggregate(CPP_MINIMAL_FIXTURE, "level:*", &plan);
        assert_json_lines(&sink, "minimal", &[r#"{"archive_id":"minimal","count":1}"#]);

        let zero = aggregate(CPP_MINIMAL_FIXTURE, "level:NOPE", &plan);
        assert_eq!(0, zero.results().len());
    }

    #[test]
    fn count_by_time_uses_cpp_negative_truncation_and_sorted_buckets() {
        let plan = AggregationPlan::count_by_time(1_000).expect("positive bucket");
        let sink = aggregate(CPP_AGGREGATION_FIXTURE, "*: *", &plan);
        assert_json_lines(
            &sink,
            "aggregation",
            &[
                r#"{"archive_id":"aggregation","count":2,"timestamp":-1700000001000}"#,
                r#"{"archive_id":"aggregation","count":3,"timestamp":-1700000000000}"#,
                r#"{"archive_id":"aggregation","count":2,"timestamp":-1699999999000}"#,
                r#"{"archive_id":"aggregation","count":3,"timestamp":-1699999998000}"#,
                r#"{"archive_id":"aggregation","count":1,"timestamp":-1699999997000}"#,
            ],
        );
    }

    #[test]
    fn mixed_integer_float_extremes_compare_exactly_beyond_two_to_the_53() {
        let minimum = AggregationPlan::minimum("mixed").expect("minimum field");
        let minimum = aggregate(CPP_AGGREGATION_FIXTURE, "*: *", &minimum);
        assert_json_lines(
            &minimum,
            "aggregation",
            &[r#"{"archive_id":"aggregation","field":"mixed","min":9.007199254740992e+15}"#],
        );

        let maximum = AggregationPlan::maximum("mixed").expect("maximum field");
        let maximum = aggregate(CPP_AGGREGATION_FIXTURE, "*: *", &maximum);
        assert_json_lines(
            &maximum,
            "aggregation",
            &[r#"{"archive_id":"aggregation","field":"mixed","max":9007199254740993}"#],
        );
    }

    #[test]
    fn escaped_paths_resolve_and_nonscalar_or_missing_values_are_ignored() {
        let escaped = AggregationPlan::minimum(r"outer.a\.b.value").expect("escaped dot field");
        let escaped = aggregate(CPP_AGGREGATION_FIXTURE, "*: *", &escaped);
        assert_json_lines(
            &escaped,
            "aggregation",
            &[r#"{"archive_id":"aggregation","field":"outer.a\\.b.value","min":3}"#],
        );

        let target = AggregationPlan::unique("target").expect("target field");
        let target = aggregate(CPP_AGGREGATION_FIXTURE, "*: *", &target);
        assert_json_lines(
            &target,
            "aggregation",
            &[
                r#"{"archive_id":"aggregation","field":"target","value":10}"#,
                r#"{"archive_id":"aggregation","field":"target","value":2.5}"#,
            ],
        );
    }

    #[test]
    fn descendants_below_structured_arrays_are_not_json_object_field_values() {
        let fixture = decode_hex(CPP_STRUCTURED_ARRAY_FIXTURE_HEX);
        {
            let mut reader = SingleFileArchiveReader::open(Cursor::new(fixture.as_slice()))
                .expect("open fixture");
            let catalog = reader
                .read_catalog(ArchiveCatalogLimits::default())
                .expect("read structured-array catalog");
            let descendant = catalog
                .schema_tree()
                .nodes()
                .iter()
                .enumerate()
                .find(|(node_id, node)| {
                    node.key_bytes() == b"x"
                        && has_structured_array_ancestor(
                            u32::try_from(*node_id).expect("node ID fits u32"),
                            catalog.schema_tree(),
                        )
                        .expect("validated ancestor chain")
                })
                .map(|(node_id, _)| u32::try_from(node_id).expect("node ID fits u32"))
                .expect("fixture contains x below a structured array");
            let mut candidate_nodes = vec![descendant];
            retain_object_reachable_nodes(&mut candidate_nodes, catalog.schema_tree())
                .expect("filter candidate nodes");
            assert_eq!(candidate_nodes, [] as [u32; 0]);
        }

        let plans = [
            AggregationPlan::minimum("items.x").expect("minimum field"),
            AggregationPlan::maximum("items.x").expect("maximum field"),
            AggregationPlan::unique("items.x").expect("unique field"),
        ];
        for plan in &plans {
            let sink = aggregate(&fixture, "*: *", plan);
            assert_eq!(0, sink.results().len(), "{:?}", plan.kind());
        }
    }

    #[test]
    fn unique_orders_cpp_scalar_variants_and_values() {
        let plan = AggregationPlan::unique("unique").expect("unique field");
        let sink = aggregate(CPP_AGGREGATION_FIXTURE, "*: *", &plan);
        assert_json_lines(
            &sink,
            "aggregation",
            &[
                r#"{"archive_id":"aggregation","field":"unique","value":-1}"#,
                r#"{"archive_id":"aggregation","field":"unique","value":2}"#,
                r#"{"archive_id":"aggregation","field":"unique","value":-0.0}"#,
                r#"{"archive_id":"aggregation","field":"unique","value":1.5}"#,
                r#"{"archive_id":"aggregation","field":"unique","value":"a"}"#,
                r#"{"archive_id":"aggregation","field":"unique","value":"z"}"#,
                r#"{"archive_id":"aggregation","field":"unique","value":false}"#,
                r#"{"archive_id":"aggregation","field":"unique","value":true}"#,
            ],
        );
    }

    #[test]
    fn unique_reconstructs_only_the_selected_dictionary_and_clp_strings() {
        let variable = AggregationPlan::unique("v").expect("variable string field");
        let variable = aggregate(CPP_STRINGS_FIXTURE, "*: *", &variable);
        assert_json_lines(
            &variable,
            "strings",
            &[
                r#"{"archive_id":"strings","field":"v","value":"YScope"}"#,
                r#"{"archive_id":"strings","field":"v","value":"a\tb"}"#,
            ],
        );

        let clp = AggregationPlan::unique("c").expect("CLP string field");
        let clp = aggregate(CPP_STRINGS_FIXTURE, "*: *", &clp);
        assert_eq!(4, clp.results().len());
    }

    #[test]
    fn exact_float_json_matches_nlohmann_thresholds_and_exponents() {
        let plan = AggregationPlan::unique("formatted").expect("formatted float field");
        let sink = aggregate(CPP_RETAINED_FLOATS_FIXTURE, "*: *", &plan);
        assert_json_lines(
            &sink,
            "floats",
            &[
                r#"{"archive_id":"floats","field":"formatted","value":-0.0}"#,
                r#"{"archive_id":"floats","field":"formatted","value":5e-324}"#,
                r#"{"archive_id":"floats","field":"formatted","value":1.234567891234567e-20}"#,
                r#"{"archive_id":"floats","field":"formatted","value":123456789.0}"#,
                r#"{"archive_id":"floats","field":"formatted","value":1234567891.234567}"#,
                r#"{"archive_id":"floats","field":"formatted","value":1.7976931348623157e+308}"#,
            ],
        );
    }

    #[test]
    fn timestamp_scalars_preserve_json_types_without_record_marshalling() {
        let unique = AggregationPlan::unique("ts").expect("timestamp field");
        let unique = aggregate(CPP_TIMESTAMPS_FIXTURE, "*: *", &unique);
        assert_json_lines(
            &unique,
            "timestamps",
            &[
                r#"{"archive_id":"timestamps","field":"ts","value":1700000000123}"#,
                r#"{"archive_id":"timestamps","field":"ts","value":1700000001123}"#,
                r#"{"archive_id":"timestamps","field":"ts","value":"2015-02-01T01:02:03.004"}"#,
                r#"{"archive_id":"timestamps","field":"ts","value":"2015-02-01T01:02:04.004"}"#,
            ],
        );
    }

    #[test]
    fn configuration_and_retained_state_limits_fail_explicitly() {
        assert!(matches!(
            AggregationPlan::count_by_time(0),
            Err(AggregationPlanError::InvalidBucketSize { .. })
        ));
        assert!(matches!(
            AggregationPlan::unique("@timestamp"),
            Err(AggregationPlanError::UnsupportedNamespace { .. })
        ));
        assert!(AggregationPlan::unique("wild.*").is_err());

        let defaults = AggregationLimits::default();
        let limits = AggregationLimits::new(defaults.projection(), 1, 1, 1, 1024);
        let plan = AggregationPlan::unique_with_limits("unique", limits).expect("limited unique");
        let query = parse_kql("*: *", KqlLimits::default()).expect("query");
        let mut reader =
            SingleFileArchiveReader::open(Cursor::new(CPP_AGGREGATION_FIXTURE)).expect("fixture");
        let mut sink = plan.start();
        let error = search_archive(
            &mut reader,
            &query,
            &mut sink,
            &ArchiveSearchOptions::default(),
        )
        .expect_err("second unique value exceeds limit");
        let source = match error {
            crate::search::ArchiveSearchError::Sink { source, .. } => source,
            other => panic!("unexpected failure: {other}"),
        };
        assert!(matches!(
            source
                .get_ref()
                .and_then(|source| source.downcast_ref::<AggregationError>()),
            Some(AggregationError::LimitExceeded {
                resource: AggregationResource::UniqueValues,
                actual: 2,
                limit: 1,
            })
        ));
    }
}
