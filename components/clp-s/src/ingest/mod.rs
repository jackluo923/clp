//! Bounded, library-first readers for structured input.
//!
//! [`NdjsonReader`] deliberately implements physical-line NDJSON framing: one non-blank physical
//! line is one JSON document. This makes malformed-record skipping deterministic and bounded.
//! [`ParseManyReader`] is the separate C++-compatible framing adapter for multi-line and directly
//! adjacent root objects. Both readers share the same iterative JSON parser and flat borrowed
//! event representation.

mod archive_writer;
#[cfg(feature = "container")]
mod container_archive;
mod input;
pub(crate) mod json_canonical;
mod kv_ir;
mod kv_ir_archive;
mod kv_ir_owned_event;
mod kv_ir_serializer;
mod kv_ir_text;
mod ndjson;
mod number;
mod parse_many;
mod parser;
mod source_path;
mod structured_stream;
mod timestamp;

pub use archive_writer::JsonArchiveAppendError;
pub use archive_writer::JsonArchiveOptions;
pub use archive_writer::JsonArchiveSetAppendError;
pub use archive_writer::JsonArchiveSetError;
pub use archive_writer::JsonArchiveSetSink;
pub use archive_writer::JsonArchiveSink;
pub use archive_writer::JsonRecordEventError;
pub use archive_writer::JsonRecordTraversalError;
pub use archive_writer::JsonStructuredArrayLimitResource;
pub use archive_writer::JsonStructuredArrayLimits;
pub use archive_writer::JsonStructuredArrayResource;
pub use archive_writer::TimestampedJsonArchiveSetSink;
pub use archive_writer::TimestampedJsonArchiveSink;
#[cfg(feature = "container")]
pub use container_archive::ContainerArchiveError;
#[cfg(feature = "container")]
pub use container_archive::ContainerArchiveOptions;
#[cfg(feature = "container")]
pub use container_archive::ContainerArchiveOutcome;
#[cfg(feature = "container")]
pub use container_archive::ContainerError;
#[cfg(feature = "container")]
pub use container_archive::ContainerLimits;
#[cfg(feature = "container")]
pub use container_archive::ContainerMemberError;
#[cfg(feature = "container")]
pub use container_archive::ContainerOptions;
#[cfg(feature = "container")]
pub use container_archive::EntryMetadata;
#[cfg(feature = "container")]
pub use container_archive::FormatPolicy;
#[cfg(feature = "container")]
pub use container_archive::VisitOutcome;
#[cfg(feature = "container")]
pub use container_archive::ingest_container_archive_set;
pub use input::DecodedInput;
pub use input::InputCompression;
pub use input::InputCompressionPolicy;
pub use input::InputDecodeError;
pub use input::InputDecodeErrorKind;
pub use input::InputError;
pub use input::InputLimitResource;
pub use input::InputLimitViolation;
pub use input::InputLimits;
pub use input::InputStats;
pub use kv_ir::KvIrEncodedText;
pub use kv_ir::KvIrEncodedVariable;
pub use kv_ir::KvIrEncoding;
pub use kv_ir::KvIrError;
pub use kv_ir::KvIrErrorKind;
pub use kv_ir::KvIrInteger;
pub use kv_ir::KvIrIntegerWidth;
pub use kv_ir::KvIrInvalidData;
pub use kv_ir::KvIrItem;
pub use kv_ir::KvIrItemKind;
pub use kv_ir::KvIrLimitResource;
pub use kv_ir::KvIrLimitViolation;
pub use kv_ir::KvIrLimits;
pub use kv_ir::KvIrLogEvent;
pub use kv_ir::KvIrNamespace;
pub use kv_ir::KvIrNodeType;
pub use kv_ir::KvIrOptions;
pub use kv_ir::KvIrPair;
pub use kv_ir::KvIrPairs;
pub use kv_ir::KvIrReadError;
pub use kv_ir::KvIrReader;
pub use kv_ir::KvIrResource;
pub use kv_ir::KvIrSchemaEntry;
pub use kv_ir::KvIrSchemaNode;
pub use kv_ir::KvIrSink;
pub use kv_ir::KvIrStats;
pub use kv_ir::KvIrStreamEnd;
pub use kv_ir::KvIrStreamHeader;
pub use kv_ir::KvIrStrings;
pub use kv_ir::KvIrTruncatedContext;
pub use kv_ir::KvIrUtcOffsetChange;
pub use kv_ir::KvIrValue;
pub use kv_ir::KvIrValueKind;
pub use kv_ir_archive::KvIrArchiveError;
pub use kv_ir_archive::KvIrArchiveErrorKind;
pub use kv_ir_archive::KvIrArchiveFailure;
pub use kv_ir_archive::KvIrArchiveInvalidData;
pub use kv_ir_archive::KvIrArchiveLimitResource;
pub use kv_ir_archive::KvIrArchiveLimitViolation;
pub use kv_ir_archive::KvIrArchiveLimits;
pub use kv_ir_archive::KvIrArchiveOptions;
pub use kv_ir_archive::KvIrArchiveResource;
pub use kv_ir_archive::KvIrArchiveSetSink;
pub use kv_ir_archive::KvIrArchiveStats;
pub use kv_ir_archive::KvIrArchiveUnsupported;
pub use kv_ir_archive::TimestampedKvIrArchiveSetSink;
pub use kv_ir_owned_event::KvIrOwnedEvent;
pub use kv_ir_owned_event::KvIrOwnedEventError;
pub use kv_ir_owned_event::KvIrOwnedEventLimitResource;
pub use kv_ir_owned_event::KvIrOwnedEventLimits;
pub use kv_ir_owned_event::KvIrOwnedEventMaterializer;
pub use kv_ir_owned_event::KvIrOwnedEventNode;
pub use kv_ir_owned_event::KvIrOwnedEventResource;
pub use kv_ir_owned_event::KvIrOwnedSpan;
pub use kv_ir_owned_event::KvIrOwnedValue;
pub use kv_ir_owned_event::KvIrOwnedValueKind;
pub use kv_ir_serializer::KvIrMessagePackErrorKind;
pub use kv_ir_serializer::KvIrSerializer;
pub use kv_ir_serializer::KvIrSerializerError;
pub use kv_ir_serializer::KvIrSerializerInput;
pub use kv_ir_serializer::KvIrSerializerLimitResource;
pub use kv_ir_serializer::KvIrSerializerLimitViolation;
pub use kv_ir_serializer::KvIrSerializerLimits;
pub use kv_ir_serializer::KvIrSerializerOptions;
pub use kv_ir_serializer::KvIrSerializerStats;
pub use kv_ir_text::KvIrEncodedTextError;
pub use ndjson::InvalidRecordPolicy;
pub use ndjson::JsonArrayRef;
pub use ndjson::JsonEvent;
pub use ndjson::JsonEvents;
pub use ndjson::JsonString;
pub use ndjson::JsonSyntaxError;
pub use ndjson::JsonSyntaxErrorKind;
pub use ndjson::NdjsonError;
pub use ndjson::NdjsonInvalidRecord;
pub use ndjson::NdjsonInvalidRecordKind;
pub use ndjson::NdjsonLimitResource;
pub use ndjson::NdjsonLimitViolation;
pub use ndjson::NdjsonLimits;
pub use ndjson::NdjsonOptions;
pub use ndjson::NdjsonReadError;
pub use ndjson::NdjsonReader;
pub use ndjson::NdjsonRecord;
pub use ndjson::NdjsonRecordSink;
pub use ndjson::NdjsonResource;
pub use ndjson::NdjsonStats;
pub use number::ClassifiedJsonNumber;
pub use number::JsonNumberClassificationError;
pub use number::JsonNumberDomain;
pub use number::classify_json_number;
pub use parse_many::IncompleteDocumentPolicy;
pub use parse_many::ParseManyDocument;
pub use parse_many::ParseManyDocumentSink;
pub use parse_many::ParseManyError;
pub use parse_many::ParseManyInvalidDocument;
pub use parse_many::ParseManyInvalidDocumentKind;
pub use parse_many::ParseManyLimitResource;
pub use parse_many::ParseManyLimitViolation;
pub use parse_many::ParseManyLimits;
pub use parse_many::ParseManyOptions;
pub use parse_many::ParseManyReadError;
pub use parse_many::ParseManyReader;
pub use parse_many::ParseManyResource;
pub use parse_many::ParseManyStats;
pub use source_path::SourcePathContextError;
pub use source_path::SourcePathTransform;
pub use source_path::SourcePathTransformError;
pub use structured_stream::ProbedStructuredInput;
pub use structured_stream::StructuredInputKind;
pub use structured_stream::StructuredStreamError;
pub use structured_stream::StructuredStreamOptions;
pub use structured_stream::StructuredStreamStats;
pub use structured_stream::ingest_structured_stream;
pub use structured_stream::probe_structured_input;
pub use timestamp::JsonTimestampError;
pub use timestamp::JsonTimestampPath;
pub use timestamp::JsonTimestampPathError;
pub use timestamp::JsonTimestampPathLimits;
pub use timestamp::JsonTimestampPathResource;
pub use timestamp::JsonTimestampResolver;
pub use timestamp::JsonTimestampScalarKind;
pub use timestamp::KvIrTimestampResolver;

#[cfg(test)]
mod kv_ir_tests;
#[cfg(test)]
mod tests;
