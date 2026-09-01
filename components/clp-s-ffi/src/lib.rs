//! Versioned C bindings for the library-first CLP structured archive engine.
//!
//! # ABI contract
//!
//! Every exported symbol is prefixed `clp_s_v1_`; changing an existing symbol's layout or
//! semantics requires a new ABI prefix. Archive, query, KV-IR serializer/deserializer, and owned
//! event values are opaque, explicitly owned handles. A non-null handle must be live, have the
//! matching type, and must not be used after it is freed. Freeing a null handle is a no-op. Archive
//! and query handles retain only immutable local-path or parsed-query state, and each operation
//! opens a fresh archive reader; extraction and search may therefore run concurrently against the
//! same live handles. KV-IR serializers and deserializers are mutable, so calls on one such handle
//! and its callback context must be externally serialized. A successful serializer mutation
//! invalidates earlier pending-output views. Metadata views remain valid until their deserializer
//! is freed; event views remain valid until their independent event handle is freed. Freeing any
//! handle while an operation or borrow uses it is forbidden.
//!
//! Paths and queries are length-delimited UTF-8 byte strings. Archive opening accepts a local
//! regular single-file archive or a local directory archive. Operations use bounded core-library
//! defaults and own no command-line or process-global logging state.
//!
//! Fallible calls return a stable [`ClpSStatus`] category and optionally write detail into a
//! caller-owned [`ClpSErrorBuffer`]. `required` includes the terminating NUL. A short buffer gets
//! a NUL-terminated prefix without changing the primary status. A null error-buffer pointer
//! discards detail; a non-null buffer structure and every nonempty data region must be writable.
//! Success sets `required` to zero and writes an empty C string when capacity is nonzero. Embedded
//! NUL bytes in Rust error text are escaped as the two bytes `\\0`.
//! [`CLP_S_STATUS_BUFFER_TOO_SMALL`] is reserved for explicit copy APIs such as
//! [`clp_s_v1_library_version`]; truncating diagnostic text never hides the status of the operation
//! that failed.
//!
//! Extraction and search call a caller function synchronously once per complete JSON document.
//! Record bytes exclude the JSONL newline and are borrowed only until the callback returns. A
//! byte-preserved archive string may make those C++-compatible JSON-form bytes invalid UTF-8, so
//! callers that require standards-valid JSON must validate them. A callback may continue, cancel,
//! or report failure. The invocation that returns cancel/failure is included in
//! `records_delivered`. Callbacks must not unwind, retain borrowed pointers, free or reenter the
//! active handles, or use them concurrently from another thread.
//!
//! All Rust work reached through an exported fallible entry point is contained by
//! `catch_unwind`; an unexpected Rust panic maps to [`CLP_S_STATUS_PANIC`] rather than crossing
//! the C ABI. Invalid arbitrary addresses, stale handles, double-free, callback unwinding, and
//! data races remain caller contract violations because C pointer provenance cannot be validated.

#![deny(warnings)]
#![deny(unsafe_op_in_unsafe_fn)]

mod columnar;
mod kv_ir_columnar;

use std::ffi::c_void;
use std::fs;
use std::fs::File;
use std::io;
use std::io::Read;
use std::panic::AssertUnwindSafe;
use std::panic::catch_unwind;
use std::path::Path;
use std::path::PathBuf;
use std::ptr;
use std::slice;
use std::str;

use clp_s::ArchiveReader;
use clp_s::ExtractionMode;
use clp_s::ExtractionOptions;
use clp_s::JsonlRecord;
use clp_s::JsonlRecordSink;
use clp_s::archive::DirectoryArchiveReader;
use clp_s::archive::FsDirectoryArchiveSource;
use clp_s::archive::MetadataLimits;
use clp_s::archive::SingleFileArchiveReader;
use clp_s::extract_jsonl_records;
use clp_s::ingest::KvIrEncodedTextError;
use clp_s::ingest::KvIrEncoding;
use clp_s::ingest::KvIrError;
use clp_s::ingest::KvIrErrorKind;
use clp_s::ingest::KvIrItem;
use clp_s::ingest::KvIrItemKind;
use clp_s::ingest::KvIrMessagePackErrorKind;
use clp_s::ingest::KvIrNamespace;
use clp_s::ingest::KvIrOptions;
use clp_s::ingest::KvIrOwnedEvent;
use clp_s::ingest::KvIrOwnedEventError;
use clp_s::ingest::KvIrOwnedEventLimits;
use clp_s::ingest::KvIrOwnedEventMaterializer;
use clp_s::ingest::KvIrOwnedEventNode;
use clp_s::ingest::KvIrOwnedSpan;
use clp_s::ingest::KvIrOwnedValueKind;
use clp_s::ingest::KvIrReadError;
use clp_s::ingest::KvIrReader;
use clp_s::ingest::KvIrSerializer;
use clp_s::ingest::KvIrSerializerError;
use clp_s::ingest::KvIrSerializerLimits;
use clp_s::ingest::KvIrSerializerOptions;
use clp_s::json::JsonBytePolicy;
use clp_s::search::ArchiveSearchOptions;
use clp_s::search::KqlLimits;
use clp_s::search::ParsedQuery;
use clp_s::search::SearchJsonlAdapter;
use clp_s::search::SearchJsonlOptions;
use clp_s::search::SearchLimits;
use clp_s::search::SearchOptions;
use clp_s::search::parse_kql;
use clp_s::search::search_archive;

/// Numeric version of the `clp_s_v1_*` ABI.
pub const CLP_S_ABI_VERSION: u32 = 1;

/// Fixed-width status returned by every fallible ABI function.
pub type ClpSStatus = u32;

/// The operation completed successfully.
pub const CLP_S_STATUS_OK: ClpSStatus = 0;
/// An argument, flag, pointer/length shape, or local path kind was invalid.
pub const CLP_S_STATUS_INVALID_ARGUMENT: ClpSStatus = 1;
/// Opening or inspecting a local filesystem object failed.
pub const CLP_S_STATUS_IO: ClpSStatus = 2;
/// Archive validation, decoding, extraction, or search execution failed.
pub const CLP_S_STATUS_ARCHIVE: ClpSStatus = 3;
/// KQL decoding or parsing failed.
pub const CLP_S_STATUS_QUERY: ClpSStatus = 4;
/// The record callback requested normal early cancellation.
pub const CLP_S_STATUS_CANCELLED: ClpSStatus = 5;
/// The record callback reported failure or returned an unknown result.
pub const CLP_S_STATUS_CALLBACK_ERROR: ClpSStatus = 6;
/// A panic originating in Rust was contained at the ABI boundary.
pub const CLP_S_STATUS_PANIC: ClpSStatus = 7;
/// A caller-owned explicit copy buffer was too short.
pub const CLP_S_STATUS_BUFFER_TOO_SMALL: ClpSStatus = 8;
/// KV-IR metadata or `MessagePack` input was invalid or unsupported.
pub const CLP_S_STATUS_KV_IR_INVALID_DATA: ClpSStatus = 9;
/// A configured KV-IR resource limit or addressable-size limit was exceeded.
pub const CLP_S_STATUS_LIMIT_EXCEEDED: ClpSStatus = 10;
/// Rust could not reserve the memory required by the KV-IR operation.
pub const CLP_S_STATUS_ALLOCATION_FAILED: ClpSStatus = 11;
/// The KV-IR serializer or deserializer operation is invalid in its current lifecycle state.
pub const CLP_S_STATUS_INVALID_STATE: ClpSStatus = 12;
/// The caller-provided KV-IR input callback failed or violated its output contract.
pub const CLP_S_STATUS_KV_IR_INPUT_CALLBACK_ERROR: ClpSStatus = 13;
/// A KV-IR stream or serializer `MessagePack` input ended before a complete item.
pub const CLP_S_STATUS_KV_IR_INCOMPLETE: ClpSStatus = 14;
/// The first explicit KV-IR stream end was reached; no event was returned.
pub const CLP_S_STATUS_EOF: ClpSStatus = 15;
/// A root `MessagePack` input value was not a map.
pub const CLP_S_STATUS_KV_IR_ROOT_NOT_MAP: ClpSStatus = 16;

/// The callback accepted the record and requests the next one.
pub const CLP_S_CALLBACK_CONTINUE: u32 = 0;
/// The callback accepted the record and requests successful early cancellation.
pub const CLP_S_CALLBACK_CANCEL: u32 = 1;
/// The callback could not accept the record.
pub const CLP_S_CALLBACK_ERROR: u32 = 2;

/// Emit records in canonical log-event order during extraction.
pub const CLP_S_EXTRACT_LOG_ORDER: u32 = 1;
/// Fold ASCII case during query matching.
pub const CLP_S_QUERY_IGNORE_CASE: u32 = 1;

/// Use the ABI's default KV-IR encoding (four-byte in ABI v1).
pub const CLP_S_KV_IR_ENCODING_DEFAULT: u32 = 0;
/// Use four-byte encoded variables in the KV-IR stream.
pub const CLP_S_KV_IR_ENCODING_FOUR_BYTE: u32 = 4;
/// Use eight-byte encoded variables in the KV-IR stream.
pub const CLP_S_KV_IR_ENCODING_EIGHT_BYTE: u32 = 8;

/// The KV-IR input callback supplied zero or more bytes successfully.
pub const CLP_S_KV_IR_READ_OK: u32 = 0;
/// The KV-IR input callback could not supply bytes.
pub const CLP_S_KV_IR_READ_ERROR: u32 = 1;

/// One pull read into Rust-provided writable storage.
///
/// On `CLP_S_KV_IR_READ_OK`, `out_read` must be at most `capacity`; zero indicates physical EOF.
/// Any other disposition is a callback failure. The callback must not unwind or retain `dst`.
pub type ClpSKvIrReadCallback = unsafe extern "C" fn(
    context: *mut c_void,
    dst: *mut u8,
    capacity: usize,
    out_read: *mut usize,
) -> u32;

/// Caller-owned storage for optional diagnostic text.
///
/// `required` is always an output. It is zero on success and otherwise includes the trailing NUL.
/// When `capacity` is nonzero, `data` must point to that many writable bytes.
#[repr(C)]
#[derive(Debug)]
pub struct ClpSErrorBuffer {
    /// Writable bytes, or null only when `capacity` is zero.
    pub data: *mut u8,
    /// Number of writable bytes at `data`.
    pub capacity: usize,
    /// Required bytes including the trailing NUL, or zero on success.
    pub required: usize,
}

/// One complete borrowed JSON document delivered synchronously to a callback.
///
/// `json` is valid for exactly `json_length` bytes until the callback returns. Physical table and
/// row indexes are always present. `log_event_idx` is meaningful only when
/// `has_log_event_idx == 1`; all reserved bits and fields are zero in ABI v1.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct ClpSRecord {
    /// Borrowed JSON document bytes without a trailing newline.
    pub json: *const u8,
    /// Number of borrowed bytes at `json`.
    pub json_length: usize,
    /// Stable global physical table index.
    pub table_index: u64,
    /// Zero-based physical row index within the table.
    pub row_index: u64,
    /// Canonical archive-local log-event index when present.
    pub log_event_idx: u64,
    /// One when `log_event_idx` is present, otherwise zero.
    pub has_log_event_idx: u32,
    /// Reserved for ABI-compatible extension; always zero.
    pub reserved: u32,
}

/// Fixed-width construction options for a KV-IR serializer.
///
/// `struct_size` must equal `sizeof(clp_s_kv_ir_serializer_options)` for ABI v1. `encoding` is
/// `DEFAULT`, `FOUR_BYTE`, or `EIGHT_BYTE`. A zero limit selects the core-library default for that
/// resource. Every reserved element must be zero.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClpSKvIrSerializerOptions {
    /// Byte size of this structure supplied by the caller.
    pub struct_size: u32,
    /// One of the `CLP_S_KV_IR_ENCODING_*` constants.
    pub encoding: u32,
    /// Maximum bytes accepted in either `MessagePack` map, or zero for the default.
    pub max_input_bytes_per_map: u64,
    /// Maximum unconsumed serialized bytes retained by the handle, or zero for the default.
    pub max_pending_output_bytes: u64,
    /// Maximum serialized bytes added by one log event, or zero for the default.
    pub max_event_output_bytes: u64,
    /// Maximum user-defined and complete preamble metadata bytes, or zero for the default.
    pub max_metadata_bytes: u64,
    /// Maximum schema nodes retained in either namespace, or zero for the default.
    pub max_schema_nodes_per_namespace: u64,
    /// Maximum `MessagePack`/metadata nesting depth, or zero for the default.
    pub max_nesting_depth: u64,
    /// Maximum `MessagePack` values in either input map, or zero for the default.
    pub max_values_per_map: u64,
    /// Maximum bytes in one `MessagePack` scalar, or zero for the default.
    pub max_scalar_bytes: u64,
    /// Reserved for ABI-compatible extension; every element must be zero in ABI v1.
    pub reserved: [u64; 4],
}

impl Default for ClpSKvIrSerializerOptions {
    fn default() -> Self {
        Self {
            struct_size: u32::try_from(std::mem::size_of::<Self>())
                .expect("KV-IR serializer options fit u32"),
            encoding: CLP_S_KV_IR_ENCODING_FOUR_BYTE,
            max_input_bytes_per_map: 0,
            max_pending_output_bytes: 0,
            max_event_output_bytes: 0,
            max_metadata_bytes: 0,
            max_schema_nodes_per_namespace: 0,
            max_nesting_depth: 0,
            max_values_per_map: 0,
            max_scalar_bytes: 0,
            reserved: [0; 4],
        }
    }
}

/// One borrowed view of the serializer's currently pending output.
///
/// `data` is null exactly when `length` is zero. A successful append, UTC-offset change, consume,
/// finish, or free invalidates every earlier view. The bytes must not be modified or retained
/// beyond that point.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct ClpSKvIrPendingView {
    /// Borrowed pending bytes, or null when there are none.
    pub data: *const u8,
    /// Number of borrowed bytes at `data`.
    pub length: usize,
}

/// Cumulative committed KV-IR serializer statistics.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ClpSKvIrSerializerStats {
    /// Number of committed log events.
    pub log_events: u64,
    /// Number of committed non-root schema nodes across both namespaces.
    pub schema_nodes: u64,
    /// Number of committed UTC-offset packets.
    pub utc_offset_changes: u64,
    /// Total bytes committed since construction, including consumed bytes and the preamble.
    pub serialized_bytes: u64,
    /// Bytes currently available through a pending view.
    pub pending_bytes: u64,
    /// One after the end-of-stream byte has been committed, otherwise zero.
    pub is_finished: u32,
    /// Reserved for ABI-compatible extension; always zero in ABI v1.
    pub reserved: u32,
}

/// Fixed-width construction options for a pull-based KV-IR deserializer.
///
/// `struct_size` must equal `sizeof(clp_s_kv_ir_deserializer_options)`. Zero limits select core
/// defaults. `max_read_chunk_bytes` caps each input-callback request; zero leaves the core reader's
/// request size unchanged. Every reserved field must be zero.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClpSKvIrDeserializerOptions {
    /// Byte size of this structure supplied by the caller.
    pub struct_size: u32,
    /// Reserved alignment/extension field; must be zero.
    pub reserved0: u32,
    /// Maximum bytes requested in one input callback, or zero for the core default.
    pub max_read_chunk_bytes: u64,
    /// Maximum DFS nodes materialized in one returned event, or zero for the core default.
    pub max_materialized_nodes: u64,
    /// Maximum key/value bytes retained by one event, or zero for the core default.
    pub max_event_arena_bytes: u64,
    /// Maximum bytes reconstructed for one encoded-text value, or zero for the core default.
    pub max_reconstructed_value_bytes: u64,
    /// Reserved for ABI-compatible extension; every element must be zero in ABI v1.
    pub reserved: [u64; 4],
}

impl Default for ClpSKvIrDeserializerOptions {
    fn default() -> Self {
        Self {
            struct_size: u32::try_from(std::mem::size_of::<Self>())
                .expect("KV-IR deserializer options fit u32"),
            reserved0: 0,
            max_read_chunk_bytes: 0,
            max_materialized_nodes: 0,
            max_event_arena_bytes: 0,
            max_reconstructed_value_bytes: 0,
            reserved: [0; 4],
        }
    }
}

/// Generic borrowed byte view used by immutable KV-IR handles.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct ClpSKvIrByteView {
    /// Borrowed bytes, or null exactly when `length` is zero.
    pub data: *const u8,
    /// Number of borrowed bytes at `data`.
    pub length: u64,
}

/// Zero-copy C view of one core-owned arena span.
pub type ClpSKvIrEventSpan = KvIrOwnedSpan;

/// Zero-copy C view of one core-owned DFS event node.
///
/// Its ABI-v1 representation is frozen at 32 bytes by the core library and the public C header.
pub type ClpSKvIrEventNode = KvIrOwnedEventNode;

/// One immutable view over an independently owned KV-IR event.
///
/// Every pointer remains valid until the event handle is freed. Node spans index `arena`; nodes in
/// each namespace are DFS preorder. The event remains valid after reader advancement or
/// deserializer destruction.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct ClpSKvIrEventView {
    /// Shared key/value byte arena, or null when empty.
    pub arena: *const u8,
    /// Bytes at `arena`.
    pub arena_length: u64,
    /// Auto-generated DFS nodes, or null when empty.
    pub auto_nodes: *const ClpSKvIrEventNode,
    /// Number of nodes at `auto_nodes`.
    pub auto_node_count: u64,
    /// User-generated DFS nodes, or null when empty.
    pub user_nodes: *const ClpSKvIrEventNode,
    /// Number of nodes at `user_nodes`.
    pub user_node_count: u64,
    /// Active UTC offset when this event was decoded.
    pub utc_offset_millis: i64,
    /// Zero-based stream index; ABI deserializers stop after index zero.
    pub stream_index: u64,
    /// Zero-based protocol unit index within the stream.
    pub unit_index: u64,
    /// Zero-based log-event index within the stream.
    pub event_index: u64,
    /// Absolute input offset of this event's first byte.
    pub input_offset: u64,
    /// Reserved for ABI-compatible extension; every element is zero in ABI v1.
    pub reserved: [u64; 4],
}

/// Object container selected because it owns a selected descendant.
pub const CLP_S_KV_IR_VALUE_OBJECT: u32 = 0;
/// Signed 64-bit integer stored as two's-complement `scalar_bits`.
pub const CLP_S_KV_IR_VALUE_INTEGER: u32 = 1;
/// IEEE-754 value preserving its exact `scalar_bits`.
pub const CLP_S_KV_IR_VALUE_FLOAT: u32 = 2;
/// Boolean with `scalar_bits` equal to zero or one.
pub const CLP_S_KV_IR_VALUE_BOOLEAN: u32 = 3;
/// String bytes selected by the node's value span.
pub const CLP_S_KV_IR_VALUE_STRING: u32 = 4;
/// Reconstructed unstructured-array JSON bytes selected by the node's value span.
pub const CLP_S_KV_IR_VALUE_ARRAY_JSON: u32 = 5;
/// JSON null; scalar and value span are zero.
pub const CLP_S_KV_IR_VALUE_NULL: u32 = 6;
/// Explicit empty object; scalar and value span are zero.
pub const CLP_S_KV_IR_VALUE_EMPTY_OBJECT: u32 = 7;

/// Synchronous record callback.
///
/// The return value must be one of [`CLP_S_CALLBACK_CONTINUE`], [`CLP_S_CALLBACK_CANCEL`], or
/// [`CLP_S_CALLBACK_ERROR`].
pub type ClpSRecordCallback = unsafe extern "C" fn(*mut c_void, *const ClpSRecord) -> u32;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ArchiveKind {
    SingleFile,
    Directory,
}

/// Opaque local-archive handle.
///
/// C callers see only a forward declaration. The handle stores immutable validated source state;
/// each extraction or search opens a new core reader.
#[derive(Debug)]
pub struct ClpSArchive {
    path: PathBuf,
    kind: ArchiveKind,
}

/// Opaque parsed-query handle.
///
/// A query is archive-independent, immutable, and reusable across archive operations.
#[derive(Debug)]
pub struct ClpSQuery {
    parsed: ParsedQuery,
    options: ArchiveSearchOptions,
}

/// Opaque mutable KV-IR serializer handle.
///
/// Calls using one handle must be externally serialized. A handle must not be freed while any
/// operation or borrowed pending view is active.
#[derive(Debug)]
pub struct ClpSKvIrSerializer {
    serializer: KvIrSerializer,
}

struct CallbackInput {
    callback: ClpSKvIrReadCallback,
    context: *mut c_void,
    max_read_chunk: usize,
}

/// Opaque mutable pull-based KV-IR deserializer.
///
/// The callback context and callback function must remain valid until this handle is freed. Calls
/// on one deserializer must be externally serialized.
pub struct ClpSKvIrDeserializer {
    reader: KvIrReader<CallbackInput>,
    metadata_json: Vec<u8>,
    event_materializer: KvIrOwnedEventMaterializer,
    event_limits: KvIrOwnedEventLimits,
    ended: bool,
    failed: bool,
}

/// Opaque immutable self-contained KV-IR event.
#[derive(Debug)]
pub struct ClpSKvIrEvent {
    event: KvIrOwnedEvent,
}

const _: () = {
    assert!(std::mem::offset_of!(ClpSErrorBuffer, data) == 0);
    assert!(std::mem::offset_of!(ClpSErrorBuffer, capacity) == std::mem::size_of::<*mut u8>());
    assert!(
        std::mem::offset_of!(ClpSErrorBuffer, required)
            == std::mem::size_of::<*mut u8>() + std::mem::size_of::<usize>()
    );
    assert!(
        std::mem::size_of::<ClpSErrorBuffer>()
            == std::mem::size_of::<*mut u8>() + 2 * std::mem::size_of::<usize>()
    );

    assert!(std::mem::offset_of!(ClpSRecord, json) == 0);
    assert!(std::mem::offset_of!(ClpSRecord, json_length) == std::mem::size_of::<*const u8>());
    assert!(
        std::mem::offset_of!(ClpSRecord, table_index)
            == std::mem::size_of::<*const u8>() + std::mem::size_of::<usize>()
    );
    assert!(
        std::mem::offset_of!(ClpSRecord, row_index)
            == std::mem::offset_of!(ClpSRecord, table_index) + std::mem::size_of::<u64>()
    );
    assert!(
        std::mem::offset_of!(ClpSRecord, log_event_idx)
            == std::mem::offset_of!(ClpSRecord, row_index) + std::mem::size_of::<u64>()
    );
    assert!(
        std::mem::offset_of!(ClpSRecord, has_log_event_idx)
            == std::mem::offset_of!(ClpSRecord, log_event_idx) + std::mem::size_of::<u64>()
    );
    assert!(
        std::mem::offset_of!(ClpSRecord, reserved)
            == std::mem::offset_of!(ClpSRecord, has_log_event_idx) + std::mem::size_of::<u32>()
    );
    assert!(
        std::mem::size_of::<ClpSRecord>()
            == std::mem::size_of::<*const u8>()
                + std::mem::size_of::<usize>()
                + 3 * std::mem::size_of::<u64>()
                + 2 * std::mem::size_of::<u32>()
    );

    assert!(std::mem::offset_of!(ClpSKvIrSerializerOptions, struct_size) == 0);
    assert!(std::mem::offset_of!(ClpSKvIrSerializerOptions, encoding) == 4);
    assert!(std::mem::offset_of!(ClpSKvIrSerializerOptions, max_input_bytes_per_map) == 8);
    assert!(std::mem::offset_of!(ClpSKvIrSerializerOptions, max_pending_output_bytes) == 16);
    assert!(std::mem::offset_of!(ClpSKvIrSerializerOptions, max_event_output_bytes) == 24);
    assert!(std::mem::offset_of!(ClpSKvIrSerializerOptions, max_metadata_bytes) == 32);
    assert!(std::mem::offset_of!(ClpSKvIrSerializerOptions, max_schema_nodes_per_namespace) == 40);
    assert!(std::mem::offset_of!(ClpSKvIrSerializerOptions, max_nesting_depth) == 48);
    assert!(std::mem::offset_of!(ClpSKvIrSerializerOptions, max_values_per_map) == 56);
    assert!(std::mem::offset_of!(ClpSKvIrSerializerOptions, max_scalar_bytes) == 64);
    assert!(std::mem::offset_of!(ClpSKvIrSerializerOptions, reserved) == 72);
    assert!(std::mem::size_of::<ClpSKvIrSerializerOptions>() == 104);

    assert!(std::mem::offset_of!(ClpSKvIrPendingView, data) == 0);
    assert!(std::mem::offset_of!(ClpSKvIrPendingView, length) == std::mem::size_of::<*const u8>());
    assert!(
        std::mem::size_of::<ClpSKvIrPendingView>()
            == std::mem::size_of::<*const u8>() + std::mem::size_of::<usize>()
    );

    assert!(std::mem::offset_of!(ClpSKvIrSerializerStats, log_events) == 0);
    assert!(std::mem::offset_of!(ClpSKvIrSerializerStats, schema_nodes) == 8);
    assert!(std::mem::offset_of!(ClpSKvIrSerializerStats, utc_offset_changes) == 16);
    assert!(std::mem::offset_of!(ClpSKvIrSerializerStats, serialized_bytes) == 24);
    assert!(std::mem::offset_of!(ClpSKvIrSerializerStats, pending_bytes) == 32);
    assert!(std::mem::offset_of!(ClpSKvIrSerializerStats, is_finished) == 40);
    assert!(std::mem::offset_of!(ClpSKvIrSerializerStats, reserved) == 44);
    assert!(std::mem::size_of::<ClpSKvIrSerializerStats>() == 48);

    assert!(std::mem::offset_of!(ClpSKvIrDeserializerOptions, struct_size) == 0);
    assert!(std::mem::offset_of!(ClpSKvIrDeserializerOptions, reserved0) == 4);
    assert!(std::mem::offset_of!(ClpSKvIrDeserializerOptions, max_read_chunk_bytes) == 8);
    assert!(std::mem::offset_of!(ClpSKvIrDeserializerOptions, max_materialized_nodes) == 16);
    assert!(std::mem::offset_of!(ClpSKvIrDeserializerOptions, max_event_arena_bytes) == 24);
    assert!(std::mem::offset_of!(ClpSKvIrDeserializerOptions, max_reconstructed_value_bytes) == 32);
    assert!(std::mem::offset_of!(ClpSKvIrDeserializerOptions, reserved) == 40);
    assert!(std::mem::size_of::<ClpSKvIrDeserializerOptions>() == 72);

    assert!(std::mem::offset_of!(ClpSKvIrByteView, data) == 0);
    assert!(std::mem::offset_of!(ClpSKvIrByteView, length) == 8);
    assert!(std::mem::size_of::<ClpSKvIrByteView>() == 16);
    assert!(std::mem::size_of::<ClpSKvIrEventSpan>() == 8);

    assert!(std::mem::size_of::<ClpSKvIrEventNode>() == 32);
    assert!(std::mem::align_of::<ClpSKvIrEventNode>() == 8);
    assert!(KvIrOwnedValueKind::Object as u32 == CLP_S_KV_IR_VALUE_OBJECT);
    assert!(KvIrOwnedValueKind::Integer as u32 == CLP_S_KV_IR_VALUE_INTEGER);
    assert!(KvIrOwnedValueKind::Float as u32 == CLP_S_KV_IR_VALUE_FLOAT);
    assert!(KvIrOwnedValueKind::Boolean as u32 == CLP_S_KV_IR_VALUE_BOOLEAN);
    assert!(KvIrOwnedValueKind::String as u32 == CLP_S_KV_IR_VALUE_STRING);
    assert!(KvIrOwnedValueKind::ArrayJson as u32 == CLP_S_KV_IR_VALUE_ARRAY_JSON);
    assert!(KvIrOwnedValueKind::Null as u32 == CLP_S_KV_IR_VALUE_NULL);
    assert!(KvIrOwnedValueKind::EmptyObject as u32 == CLP_S_KV_IR_VALUE_EMPTY_OBJECT);

    assert!(std::mem::offset_of!(ClpSKvIrEventView, arena) == 0);
    assert!(std::mem::offset_of!(ClpSKvIrEventView, arena_length) == 8);
    assert!(std::mem::offset_of!(ClpSKvIrEventView, auto_nodes) == 16);
    assert!(std::mem::offset_of!(ClpSKvIrEventView, auto_node_count) == 24);
    assert!(std::mem::offset_of!(ClpSKvIrEventView, user_nodes) == 32);
    assert!(std::mem::offset_of!(ClpSKvIrEventView, user_node_count) == 40);
    assert!(std::mem::offset_of!(ClpSKvIrEventView, utc_offset_millis) == 48);
    assert!(std::mem::offset_of!(ClpSKvIrEventView, stream_index) == 56);
    assert!(std::mem::offset_of!(ClpSKvIrEventView, unit_index) == 64);
    assert!(std::mem::offset_of!(ClpSKvIrEventView, event_index) == 72);
    assert!(std::mem::offset_of!(ClpSKvIrEventView, input_offset) == 80);
    assert!(std::mem::offset_of!(ClpSKvIrEventView, reserved) == 88);
    assert!(std::mem::size_of::<ClpSKvIrEventView>() == 120);
};

const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<ClpSArchive>();
    assert_send_sync::<ClpSQuery>();
    assert_send_sync::<ClpSKvIrSerializer>();
    assert_send_sync::<ClpSKvIrEvent>();
};

#[derive(Debug)]
enum ApiError {
    InvalidArgument(String),
    Io(String),
    Archive(String),
    Query(String),
    Cancelled(String),
    Callback(String),
    KvIrInvalidData(String),
    LimitExceeded(String),
    AllocationFailed(String),
    InvalidState(String),
    KvIrInputCallback(String),
    KvIrIncomplete(String),
    KvIrRootNotMap(String),
}

impl ApiError {
    const fn status(&self) -> ClpSStatus {
        match self {
            Self::InvalidArgument(_) => CLP_S_STATUS_INVALID_ARGUMENT,
            Self::Io(_) => CLP_S_STATUS_IO,
            Self::Archive(_) => CLP_S_STATUS_ARCHIVE,
            Self::Query(_) => CLP_S_STATUS_QUERY,
            Self::Cancelled(_) => CLP_S_STATUS_CANCELLED,
            Self::Callback(_) => CLP_S_STATUS_CALLBACK_ERROR,
            Self::KvIrInvalidData(_) => CLP_S_STATUS_KV_IR_INVALID_DATA,
            Self::LimitExceeded(_) => CLP_S_STATUS_LIMIT_EXCEEDED,
            Self::AllocationFailed(_) => CLP_S_STATUS_ALLOCATION_FAILED,
            Self::InvalidState(_) => CLP_S_STATUS_INVALID_STATE,
            Self::KvIrInputCallback(_) => CLP_S_STATUS_KV_IR_INPUT_CALLBACK_ERROR,
            Self::KvIrIncomplete(_) => CLP_S_STATUS_KV_IR_INCOMPLETE,
            Self::KvIrRootNotMap(_) => CLP_S_STATUS_KV_IR_ROOT_NOT_MAP,
        }
    }

    fn message(&self) -> &str {
        match self {
            Self::InvalidArgument(message)
            | Self::Io(message)
            | Self::Archive(message)
            | Self::Query(message)
            | Self::Cancelled(message)
            | Self::Callback(message)
            | Self::KvIrInvalidData(message)
            | Self::LimitExceeded(message)
            | Self::AllocationFailed(message)
            | Self::InvalidState(message)
            | Self::KvIrInputCallback(message)
            | Self::KvIrIncomplete(message)
            | Self::KvIrRootNotMap(message) => message,
        }
    }
}

#[derive(Clone, Copy)]
struct ErrorWriter {
    structure: *mut ClpSErrorBuffer,
    data: *mut u8,
    capacity: usize,
}

impl ErrorWriter {
    /// Validates the nullable buffer structure and its pointer/length shape.
    ///
    /// # Safety
    ///
    /// A non-null `structure` must point to a writable [`ClpSErrorBuffer`]. Its nonempty `data`
    /// region must remain writable through the ABI call.
    unsafe fn new(structure: *mut ClpSErrorBuffer) -> Result<Self, ()> {
        if structure.is_null() {
            return Ok(Self {
                structure,
                data: ptr::null_mut(),
                capacity: 0,
            });
        }
        // SAFETY: The caller contract for a non-null structure guarantees readable fields.
        let (data, capacity) = unsafe { ((*structure).data, (*structure).capacity) };
        if (0 != capacity && data.is_null()) || capacity > isize::MAX as usize {
            // SAFETY: The structure itself is writable even when its data-region shape is invalid.
            unsafe {
                (*structure).required = INVALID_ERROR_BUFFER_MESSAGE.len() + 1;
            }
            return Err(());
        }
        Ok(Self {
            structure,
            data,
            capacity,
        })
    }

    /// Writes a diagnostic with embedded NUL bytes escaped as `\\0`.
    ///
    /// # Safety
    ///
    /// This writer was created by [`Self::new`] and its caller-owned regions remain live.
    #[cold]
    #[inline(never)]
    unsafe fn write_message(self, message: &str) {
        if self.structure.is_null() {
            return;
        }
        let sanitized_length = message.bytes().fold(0_usize, |length, byte| {
            length.saturating_add(if 0 == byte { 2 } else { 1 })
        });
        // SAFETY: `structure` was validated as writable by `new`.
        unsafe {
            (*self.structure).required = sanitized_length.saturating_add(1);
        }
        if 0 == self.capacity {
            return;
        }

        let mut written = 0_usize;
        let maximum = self.capacity - 1;
        for byte in message.bytes() {
            let escaped: &[u8] = if 0 == byte {
                b"\\0"
            } else {
                slice::from_ref(&byte)
            };
            for escaped_byte in escaped {
                if written == maximum {
                    break;
                }
                // SAFETY: `written < capacity`; `data` names the validated writable region.
                unsafe {
                    self.data.add(written).write(*escaped_byte);
                }
                written += 1;
            }
            if written == maximum {
                break;
            }
        }
        // SAFETY: `written <= capacity - 1`, leaving room for this terminator.
        unsafe {
            self.data.add(written).write(0);
        }
    }

    /// Clears diagnostics after success.
    ///
    /// # Safety
    ///
    /// This writer was created by [`Self::new`] and its caller-owned regions remain live.
    unsafe fn clear(self) {
        if self.structure.is_null() {
            return;
        }
        // SAFETY: `structure` and any nonempty `data` region were validated by `new`.
        unsafe {
            (*self.structure).required = 0;
            if 0 != self.capacity {
                self.data.write(0);
            }
        }
    }
}

const INVALID_ERROR_BUFFER_MESSAGE: &str = "error buffer data is null or its capacity is invalid";
const PANIC_MESSAGE: &str = "an internal Rust panic was contained";

#[cold]
#[inline(never)]
unsafe fn finish_api_error(writer: ErrorWriter, source: &ApiError) -> ClpSStatus {
    let status = source.status();
    // SAFETY: `writer` remains valid until the synchronous ABI operation returns.
    unsafe {
        writer.write_message(source.message());
    }
    status
}

#[cold]
#[inline(never)]
unsafe fn finish_panic(error: *mut ClpSErrorBuffer) -> ClpSStatus {
    // A second containment boundary ensures an invalid panic payload formatter or a diagnostic
    // write cannot leak a Rust unwind across the exported C ABI.
    let _ = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: Forwarded from the exported function's caller contract. If only the data-region
        // shape is invalid, `new` safely reports that through the structure.
        if let Ok(writer) = unsafe { ErrorWriter::new(error) } {
            // SAFETY: `writer` was just validated.
            unsafe {
                writer.write_message(PANIC_MESSAGE);
            }
        }
    }));
    CLP_S_STATUS_PANIC
}

/// Executes one fallible ABI operation and contains every panic originating in Rust.
///
/// # Safety
///
/// `error` obeys the nullable caller-owned error-buffer contract for the duration of this call.
unsafe fn ffi_entry(
    error: *mut ClpSErrorBuffer,
    operation: impl FnOnce() -> Result<(), ApiError>,
) -> ClpSStatus {
    let result = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: Forwarded from this function's caller contract.
        let Ok(writer) = (unsafe { ErrorWriter::new(error) }) else {
            return CLP_S_STATUS_INVALID_ARGUMENT;
        };
        match operation() {
            Ok(()) => {
                // SAFETY: `writer` remains valid until this synchronous operation returns.
                unsafe {
                    writer.clear();
                }
                CLP_S_STATUS_OK
            }
            Err(source) => {
                // SAFETY: `writer` remains valid until this synchronous operation returns.
                unsafe { finish_api_error(writer, &source) }
            }
        }
    }));

    match result {
        Ok(status) => status,
        // SAFETY: Forwarded from this function's caller contract.
        Err(_payload) => unsafe { finish_panic(error) },
    }
}

/// Converts one nullable pointer/length pair without constructing a null Rust slice.
///
/// # Safety
///
/// For nonzero `length`, `data` must name a readable allocation of at least that many bytes for
/// the returned borrow's lifetime.
unsafe fn borrowed_bytes<'a>(
    data: *const u8,
    length: usize,
    label: &str,
) -> Result<&'a [u8], ApiError> {
    if 0 == length {
        return Ok(&[]);
    }
    if data.is_null() {
        return Err(ApiError::InvalidArgument(format!(
            "{label} is null but its length is nonzero"
        )));
    }
    if length > isize::MAX as usize {
        return Err(ApiError::InvalidArgument(format!(
            "{label} length exceeds the addressable Rust slice limit"
        )));
    }
    // SAFETY: Non-nullness, length bounds, and readable storage are established above and by the
    // caller contract.
    Ok(unsafe { slice::from_raw_parts(data, length) })
}

fn kv_ir_api_error(source: &KvIrSerializerError) -> ApiError {
    let message = source.to_string();
    match source {
        KvIrSerializerError::MetadataTooLarge(_)
        | KvIrSerializerError::Limit(_)
        | KvIrSerializerError::SizeOverflow => ApiError::LimitExceeded(message),
        KvIrSerializerError::AllocationFailed { .. } => ApiError::AllocationFailed(message),
        KvIrSerializerError::Finished => ApiError::InvalidState(message),
        KvIrSerializerError::MessagePack {
            kind: KvIrMessagePackErrorKind::Truncated,
            ..
        } => ApiError::KvIrIncomplete(message),
        KvIrSerializerError::MessagePack {
            kind: KvIrMessagePackErrorKind::RootMustBeMap,
            ..
        } => ApiError::KvIrRootNotMap(message),
        _ => ApiError::KvIrInvalidData(message),
    }
}

const fn configured_or_default(configured: u64, default: u64) -> u64 {
    if 0 == configured { default } else { configured }
}

/// Decodes a nullable fixed-width serializer-options structure.
///
/// # Safety
///
/// A non-null `options` must point to a readable [`ClpSKvIrSerializerOptions`] that remains live
/// for this call.
unsafe fn kv_ir_serializer_options(
    options: *const ClpSKvIrSerializerOptions,
) -> Result<KvIrSerializerOptions, ApiError> {
    if options.is_null() {
        return Ok(KvIrSerializerOptions::default());
    }
    // SAFETY: The caller contract guarantees readable storage for the complete ABI-v1 structure.
    let options = unsafe { &*options };
    let expected_size = u32::try_from(std::mem::size_of::<ClpSKvIrSerializerOptions>())
        .expect("KV-IR serializer options fit u32");
    if options.struct_size != expected_size {
        return Err(ApiError::InvalidArgument(format!(
            "KV-IR serializer options struct_size is {}, expected {expected_size}",
            options.struct_size
        )));
    }
    if options.reserved.iter().any(|value| 0 != *value) {
        return Err(ApiError::InvalidArgument(
            "KV-IR serializer options reserved fields must be zero".to_owned(),
        ));
    }
    let encoding = match options.encoding {
        CLP_S_KV_IR_ENCODING_DEFAULT | CLP_S_KV_IR_ENCODING_FOUR_BYTE => KvIrEncoding::FourByte,
        CLP_S_KV_IR_ENCODING_EIGHT_BYTE => KvIrEncoding::EightByte,
        actual => {
            return Err(ApiError::InvalidArgument(format!(
                "unknown KV-IR serializer encoding {actual}"
            )));
        }
    };
    let defaults = KvIrSerializerLimits::default();
    let limits = KvIrSerializerLimits::new()
        .with_max_input_bytes_per_map(configured_or_default(
            options.max_input_bytes_per_map,
            defaults.max_input_bytes_per_map(),
        ))
        .with_max_pending_output_bytes(configured_or_default(
            options.max_pending_output_bytes,
            defaults.max_pending_output_bytes(),
        ))
        .with_max_event_output_bytes(configured_or_default(
            options.max_event_output_bytes,
            defaults.max_event_output_bytes(),
        ))
        .with_max_metadata_bytes(configured_or_default(
            options.max_metadata_bytes,
            defaults.max_metadata_bytes(),
        ))
        .with_max_schema_nodes_per_namespace(configured_or_default(
            options.max_schema_nodes_per_namespace,
            defaults.max_schema_nodes_per_namespace(),
        ))
        .with_max_nesting_depth(configured_or_default(
            options.max_nesting_depth,
            defaults.max_nesting_depth(),
        ))
        .with_max_values_per_map(configured_or_default(
            options.max_values_per_map,
            defaults.max_values_per_map(),
        ))
        .with_max_scalar_bytes(configured_or_default(
            options.max_scalar_bytes,
            defaults.max_scalar_bytes(),
        ));
    Ok(KvIrSerializerOptions::new(encoding).with_limits(limits))
}

/// Decodes nullable metadata, preserving the distinction between absent and present-but-empty.
///
/// # Safety
///
/// A non-null `data` must name a readable region of `length` bytes for the returned lifetime.
unsafe fn optional_borrowed_bytes<'a>(
    data: *const u8,
    length: usize,
    label: &str,
) -> Result<Option<&'a [u8]>, ApiError> {
    if data.is_null() {
        if 0 == length {
            return Ok(None);
        }
        return Err(ApiError::InvalidArgument(format!(
            "{label} is null but its length is nonzero"
        )));
    }
    // SAFETY: Forwarded from this function's contract and validated by `borrowed_bytes`.
    unsafe { borrowed_bytes(data, length, label) }.map(Some)
}

fn kv_ir_stats(serializer: &KvIrSerializer) -> Result<ClpSKvIrSerializerStats, ApiError> {
    let stats = serializer.stats();
    let pending_bytes = u64::try_from(serializer.pending_output().len()).map_err(|_| {
        ApiError::LimitExceeded("pending KV-IR output length exceeds u64".to_owned())
    })?;
    Ok(ClpSKvIrSerializerStats {
        log_events: stats.log_events(),
        schema_nodes: stats.schema_nodes(),
        utc_offset_changes: stats.utc_offset_changes(),
        serialized_bytes: stats.serialized_bytes(),
        pending_bytes,
        is_finished: u32::from(serializer.is_finished()),
        reserved: 0,
    })
}

impl Read for CallbackInput {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let capacity = buffer.len().min(self.max_read_chunk);
        let mut read = 0_usize;
        // SAFETY: `buffer` is writable for `capacity` bytes and `read` remains live for the
        // synchronous callback. The foreign callback contract forbids unwinding and retention.
        let disposition =
            unsafe { (self.callback)(self.context, buffer.as_mut_ptr(), capacity, &raw mut read) };
        if disposition != CLP_S_KV_IR_READ_OK {
            return Err(io::Error::other(format!(
                "KV-IR input callback returned failure disposition {disposition}"
            )));
        }
        if read > capacity {
            return Err(io::Error::other(format!(
                "KV-IR input callback reported {read} byte(s) for capacity {capacity}"
            )));
        }
        Ok(read)
    }
}

fn kv_ir_reader_api_error(source: &KvIrError) -> ApiError {
    let message = source.to_string();
    match source.kind() {
        KvIrErrorKind::Input(_) => ApiError::KvIrInputCallback(message),
        KvIrErrorKind::Truncated { .. } => ApiError::KvIrIncomplete(message),
        KvIrErrorKind::Limit(_) | KvIrErrorKind::SizeOverflow => ApiError::LimitExceeded(message),
        KvIrErrorKind::AllocationFailed { .. } => ApiError::AllocationFailed(message),
        _ => ApiError::KvIrInvalidData(message),
    }
}

fn kv_ir_owned_event_api_error(source: &KvIrOwnedEventError) -> ApiError {
    let message = source.to_string();
    match source {
        KvIrOwnedEventError::Limit { .. } | KvIrOwnedEventError::SizeOverflow => {
            ApiError::LimitExceeded(message)
        }
        KvIrOwnedEventError::AllocationFailed { .. }
        | KvIrOwnedEventError::EncodedText {
            source: KvIrEncodedTextError::AllocationFailed { .. },
            ..
        } => ApiError::AllocationFailed(message),
        KvIrOwnedEventError::EncodedText {
            source: KvIrEncodedTextError::Limit { .. } | KvIrEncodedTextError::SizeOverflow,
            ..
        } => ApiError::LimitExceeded(message),
        _ => ApiError::KvIrInvalidData(message),
    }
}

fn kv_ir_read_api_error(source: KvIrReadError<ApiError>) -> ApiError {
    match source {
        KvIrReadError::Reader(source) => kv_ir_reader_api_error(&source),
        KvIrReadError::Sink { source, .. } => source,
        _ => ApiError::InvalidState("unknown incremental KV-IR reader error".to_owned()),
    }
}

/// Decodes a nullable fixed-width deserializer-options structure.
///
/// # Safety
///
/// A non-null `options` must point to a readable [`ClpSKvIrDeserializerOptions`] that remains live
/// for this call.
unsafe fn kv_ir_deserializer_options(
    options: *const ClpSKvIrDeserializerOptions,
) -> Result<(usize, KvIrOwnedEventLimits), ApiError> {
    if options.is_null() {
        return Ok((usize::MAX, KvIrOwnedEventLimits::default()));
    }
    // SAFETY: The caller contract guarantees readable storage for the complete ABI-v1 structure.
    let options = unsafe { &*options };
    let expected_size = u32::try_from(std::mem::size_of::<ClpSKvIrDeserializerOptions>())
        .expect("KV-IR deserializer options fit u32");
    if options.struct_size != expected_size {
        return Err(ApiError::InvalidArgument(format!(
            "KV-IR deserializer options struct_size is {}, expected {expected_size}",
            options.struct_size
        )));
    }
    if options.reserved0 != 0 || options.reserved.iter().any(|value| 0 != *value) {
        return Err(ApiError::InvalidArgument(
            "KV-IR deserializer options reserved fields must be zero".to_owned(),
        ));
    }
    let max_read_chunk = if options.max_read_chunk_bytes == 0 {
        usize::MAX
    } else {
        usize::try_from(options.max_read_chunk_bytes).map_err(|_| {
            ApiError::InvalidArgument(
                "KV-IR max_read_chunk_bytes exceeds the platform usize".to_owned(),
            )
        })?
    };
    let defaults = KvIrOwnedEventLimits::default();
    let event_limits = KvIrOwnedEventLimits::new()
        .with_max_materialized_nodes(configured_or_default(
            options.max_materialized_nodes,
            defaults.max_materialized_nodes(),
        ))
        .with_max_arena_bytes(configured_or_default(
            options.max_event_arena_bytes,
            defaults.max_arena_bytes(),
        ))
        .with_max_reconstructed_value_bytes(configured_or_default(
            options.max_reconstructed_value_bytes,
            defaults.max_reconstructed_value_bytes(),
        ));
    Ok((max_read_chunk, event_limits))
}

impl ClpSKvIrDeserializer {
    fn create(
        callback: ClpSKvIrReadCallback,
        context: *mut c_void,
        max_read_chunk: usize,
        event_limits: KvIrOwnedEventLimits,
    ) -> Result<Self, ApiError> {
        let input = CallbackInput {
            callback,
            context,
            max_read_chunk,
        };
        let mut reader = KvIrReader::new(input, KvIrOptions::default());
        let event_materializer = KvIrOwnedEventMaterializer::new()
            .map_err(|source| kv_ir_owned_event_api_error(&source))?;
        let mut metadata_json = None;
        let kind = reader
            .read_next_item(&mut |item: KvIrItem<'_>| match item {
                KvIrItem::StreamStart(header) => {
                    let source = header.metadata_json();
                    let mut owned = Vec::new();
                    owned.try_reserve_exact(source.len()).map_err(|_| {
                        ApiError::AllocationFailed(format!(
                            "failed to allocate {} byte(s) for KV-IR metadata",
                            source.len()
                        ))
                    })?;
                    owned.extend_from_slice(source);
                    metadata_json = Some(owned);
                    Ok(())
                }
                _ => Err(ApiError::InvalidState(
                    "incremental KV-IR reader did not emit StreamStart first".to_owned(),
                )),
            })
            .map_err(kv_ir_read_api_error)?;
        if kind != Some(KvIrItemKind::StreamStart) {
            return Err(ApiError::InvalidState(
                "incremental KV-IR reader did not return StreamStart first".to_owned(),
            ));
        }
        let metadata_json = metadata_json.ok_or_else(|| {
            ApiError::InvalidState("KV-IR StreamStart omitted metadata".to_owned())
        })?;
        Ok(Self {
            reader,
            metadata_json,
            event_materializer,
            event_limits,
            ended: false,
            failed: false,
        })
    }

    fn next_owned_event(&mut self) -> Result<Option<KvIrOwnedEvent>, ApiError> {
        if self.ended {
            return Ok(None);
        }
        if self.failed {
            return Err(ApiError::InvalidState(
                "KV-IR deserializer is terminal after a reader error".to_owned(),
            ));
        }

        loop {
            let mut event = None;
            let mut saw_stream_end = false;
            let event_limits = self.event_limits;
            let event_materializer = &mut self.event_materializer;
            let result = self.reader.read_next_item(&mut |item: KvIrItem<'_>| {
                match item {
                    KvIrItem::LogEvent(borrowed) => {
                        event = Some(
                            event_materializer
                                .materialize(borrowed, event_limits)
                                .map_err(|source| kv_ir_owned_event_api_error(&source))?,
                        );
                    }
                    KvIrItem::StreamEnd(_) => saw_stream_end = true,
                    KvIrItem::StreamStart(_) => {
                        return Err(ApiError::InvalidState(
                            "unexpected concatenated KV-IR StreamStart".to_owned(),
                        ));
                    }
                    _ => {}
                }
                Ok(())
            });
            match result {
                Ok(None) => {
                    self.ended = true;
                    return Ok(None);
                }
                Ok(Some(_kind)) => {}
                Err(KvIrReadError::Reader(source)) => {
                    self.failed = true;
                    return Err(kv_ir_reader_api_error(&source));
                }
                Err(KvIrReadError::Sink { source, .. }) => {
                    self.failed = true;
                    return Err(source);
                }
                Err(_) => {
                    self.failed = true;
                    return Err(ApiError::InvalidState(
                        "unknown incremental KV-IR reader error".to_owned(),
                    ));
                }
            }
            if let Some(event) = event {
                return Ok(Some(event));
            }
            if saw_stream_end {
                self.ended = true;
                return Ok(None);
            }
        }
    }
}

const fn borrowed_or_null<T>(values: &[T]) -> *const T {
    if values.is_empty() {
        ptr::null()
    } else {
        values.as_ptr()
    }
}

fn kv_ir_event_view(event: &KvIrOwnedEvent) -> Result<ClpSKvIrEventView, ApiError> {
    let arena = event.arena();
    let auto_nodes = event.nodes(KvIrNamespace::AutoGenerated);
    let user_nodes = event.nodes(KvIrNamespace::UserGenerated);
    Ok(ClpSKvIrEventView {
        arena: borrowed_or_null(arena),
        arena_length: u64::try_from(arena.len())
            .map_err(|_| ApiError::LimitExceeded("event arena length exceeds u64".to_owned()))?,
        auto_nodes: borrowed_or_null(auto_nodes),
        auto_node_count: u64::try_from(auto_nodes.len())
            .map_err(|_| ApiError::LimitExceeded("auto node count exceeds u64".to_owned()))?,
        user_nodes: borrowed_or_null(user_nodes),
        user_node_count: u64::try_from(user_nodes.len())
            .map_err(|_| ApiError::LimitExceeded("user node count exceeds u64".to_owned()))?,
        utc_offset_millis: event.utc_offset_millis(),
        stream_index: event.stream_index(),
        unit_index: event.unit_index(),
        event_index: event.event_index(),
        input_offset: event.input_offset(),
        reserved: [0; 4],
    })
}

const fn empty_kv_ir_event_view() -> ClpSKvIrEventView {
    ClpSKvIrEventView {
        arena: ptr::null(),
        arena_length: 0,
        auto_nodes: ptr::null(),
        auto_node_count: 0,
        user_nodes: ptr::null(),
        user_node_count: 0,
        utc_offset_millis: 0,
        stream_index: 0,
        unit_index: 0,
        event_index: 0,
        input_offset: 0,
        reserved: [0; 4],
    }
}

impl ClpSArchive {
    fn validate(path: PathBuf) -> Result<Self, ApiError> {
        let metadata = fs::metadata(&path).map_err(|source| {
            ApiError::Io(format!(
                "failed to inspect archive path {}: {source}",
                path.display()
            ))
        })?;
        let kind = if metadata.is_file() {
            ArchiveKind::SingleFile
        } else if metadata.is_dir() {
            ArchiveKind::Directory
        } else {
            return Err(ApiError::InvalidArgument(format!(
                "archive path {} is neither a regular file nor a directory",
                path.display()
            )));
        };
        let archive = Self { path, kind };
        drop(archive.open_reader()?);
        Ok(archive)
    }

    fn open_reader(&self) -> Result<Box<dyn ArchiveReader>, ApiError> {
        match self.kind {
            ArchiveKind::SingleFile => {
                let file = File::open(&self.path).map_err(|source| {
                    ApiError::Io(format!(
                        "failed to open archive {}: {source}",
                        self.path.display()
                    ))
                })?;
                let reader = SingleFileArchiveReader::open(file).map_err(|source| {
                    ApiError::Archive(format!(
                        "failed to validate single-file archive {}: {source}",
                        self.path.display()
                    ))
                })?;
                Ok(Box::new(reader))
            }
            ArchiveKind::Directory => {
                let source = FsDirectoryArchiveSource::new(&self.path);
                let reader = DirectoryArchiveReader::open(source, MetadataLimits::default())
                    .map_err(|source| {
                        ApiError::Archive(format!(
                            "failed to validate directory archive {}: {source}",
                            self.path.display()
                        ))
                    })?;
                Ok(Box::new(reader))
            }
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
enum CallbackStop {
    Cancelled,
    Error(String),
}

struct CallbackSink {
    callback: ClpSRecordCallback,
    user_data: *mut c_void,
    delivered: u64,
    stop: Option<CallbackStop>,
}

impl CallbackSink {
    const fn new(callback: ClpSRecordCallback, user_data: *mut c_void) -> Self {
        Self {
            callback,
            user_data,
            delivered: 0,
            stop: None,
        }
    }
}

impl JsonlRecordSink for CallbackSink {
    fn write_record(&mut self, record: JsonlRecord<'_>) -> io::Result<()> {
        let json = record
            .jsonl_bytes()
            .strip_suffix(b"\n")
            .ok_or_else(|| io::Error::other("core JSONL record is missing its terminal newline"))?;
        let table_index = u64::try_from(record.table_index())
            .map_err(|_| io::Error::other("physical table index exceeds the v1 ABI"))?;
        let row_index = u64::try_from(record.row_index())
            .map_err(|_| io::Error::other("physical row index exceeds the v1 ABI"))?;
        let (log_event_idx, has_log_event_idx) =
            record.log_event_idx().map_or((0, 0), |index| (index, 1));
        let abi_record = ClpSRecord {
            json: json.as_ptr(),
            json_length: json.len(),
            table_index,
            row_index,
            log_event_idx,
            has_log_event_idx,
            reserved: 0,
        };
        self.delivered = self
            .delivered
            .checked_add(1)
            .ok_or_else(|| io::Error::other("callback record count overflow"))?;
        // SAFETY: The callback pointer was checked when the operation began. `abi_record` and its
        // borrowed JSON bytes remain live until this synchronous foreign call returns. The C ABI
        // contract forbids a foreign unwind.
        let disposition = unsafe { (self.callback)(self.user_data, &raw const abi_record) };
        match disposition {
            CLP_S_CALLBACK_CONTINUE => Ok(()),
            CLP_S_CALLBACK_CANCEL => {
                self.stop = Some(CallbackStop::Cancelled);
                Err(io::Error::other("record callback requested cancellation"))
            }
            CLP_S_CALLBACK_ERROR => {
                self.stop = Some(CallbackStop::Error(
                    "record callback reported failure".to_owned(),
                ));
                Err(io::Error::other("record callback reported failure"))
            }
            actual => {
                let message = format!("record callback returned unknown disposition {actual}");
                self.stop = Some(CallbackStop::Error(message.clone()));
                Err(io::Error::other(message))
            }
        }
    }
}

fn callback_result(stop: Option<CallbackStop>, archive_error: impl std::fmt::Display) -> ApiError {
    match stop {
        Some(CallbackStop::Cancelled) => {
            ApiError::Cancelled("record callback requested cancellation".to_owned())
        }
        Some(CallbackStop::Error(message)) => ApiError::Callback(message),
        None => ApiError::Archive(archive_error.to_string()),
    }
}

/// Returns [`CLP_S_ABI_VERSION`].
#[unsafe(no_mangle)]
pub const extern "C" fn clp_s_v1_abi_version() -> u32 {
    CLP_S_ABI_VERSION
}

/// Copies the Cargo package version as a NUL-terminated UTF-8 string.
///
/// A null `buffer` is permitted only when `capacity` is zero. `required` is mandatory and is set
/// to the complete byte count including NUL. A short valid buffer receives a terminated prefix and
/// returns [`CLP_S_STATUS_BUFFER_TOO_SMALL`].
///
/// # Safety
///
/// `required` must point to a writable `usize`. A nonempty `buffer` must remain writable for
/// `capacity` bytes and must not overlap `required`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn clp_s_v1_library_version(
    buffer: *mut u8,
    capacity: usize,
    required: *mut usize,
) -> ClpSStatus {
    let result = catch_unwind(AssertUnwindSafe(|| {
        if required.is_null() {
            return CLP_S_STATUS_INVALID_ARGUMENT;
        }
        let version = concat!(env!("CARGO_PKG_VERSION"), "\0").as_bytes();
        // SAFETY: The caller supplies writable `required` storage.
        unsafe {
            required.write(version.len());
        }
        if (0 != capacity && buffer.is_null()) || capacity > isize::MAX as usize {
            return CLP_S_STATUS_INVALID_ARGUMENT;
        }
        if 0 != capacity {
            let copied = version.len().saturating_sub(1).min(capacity - 1);
            // SAFETY: Both regions are valid for `copied` bytes and cannot overlap by contract.
            unsafe {
                ptr::copy_nonoverlapping(version.as_ptr(), buffer, copied);
                buffer.add(copied).write(0);
            }
        }
        if capacity < version.len() {
            CLP_S_STATUS_BUFFER_TOO_SMALL
        } else {
            CLP_S_STATUS_OK
        }
    }));
    result.unwrap_or(CLP_S_STATUS_PANIC)
}

/// Opens and validates one local single-file or directory archive.
///
/// `out_archive` is initialized to null before path decoding or filesystem work.
///
/// # Safety
///
/// `out_archive` must point to writable handle storage and must not overlap any other argument.
/// `path` follows the pointer/length contract in the crate-level documentation. `error` follows
/// [`ClpSErrorBuffer`]'s contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn clp_s_v1_archive_open(
    path: *const u8,
    path_length: usize,
    out_archive: *mut *mut ClpSArchive,
    error: *mut ClpSErrorBuffer,
) -> ClpSStatus {
    if out_archive.is_null() {
        // SAFETY: Forwarded from the function contract.
        return unsafe {
            ffi_entry(error, || {
                Err(ApiError::InvalidArgument(
                    "out_archive must not be null".to_owned(),
                ))
            })
        };
    }
    // SAFETY: The caller provides writable output storage. This happens before fallible work.
    unsafe {
        out_archive.write(ptr::null_mut());
    }
    // SAFETY: All raw arguments are governed by this function's contract.
    unsafe {
        ffi_entry(error, || {
            // SAFETY: Forwarded from the function contract and checked by `borrowed_bytes`.
            let path_bytes = borrowed_bytes(path, path_length, "path")?;
            let path_text = str::from_utf8(path_bytes).map_err(|source| {
                ApiError::InvalidArgument(format!("path is not valid UTF-8: {source}"))
            })?;
            if path_text.is_empty() {
                return Err(ApiError::InvalidArgument(
                    "archive path must not be empty".to_owned(),
                ));
            }
            if path_text.as_bytes().contains(&0) {
                return Err(ApiError::InvalidArgument(
                    "archive path contains an embedded NUL byte".to_owned(),
                ));
            }
            let archive = Box::new(ClpSArchive::validate(Path::new(path_text).to_path_buf())?);
            // SAFETY: `out_archive` is valid writable storage and still contains null.
            out_archive.write(Box::into_raw(archive));
            Ok(())
        })
    }
}

/// Frees an archive handle. A null handle is a no-op.
///
/// # Safety
///
/// A non-null pointer must have been returned by [`clp_s_v1_archive_open`], must not have been
/// freed already, and must not be in use by any concurrent operation.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn clp_s_v1_archive_free(archive: *mut ClpSArchive) {
    if archive.is_null() {
        return;
    }
    let _ = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: The caller transfers unique ownership of one live handle.
        drop(unsafe { Box::from_raw(archive) });
    }));
}

/// Parses an archive-independent KQL query into an immutable handle.
///
/// `flags` accepts only [`CLP_S_QUERY_IGNORE_CASE`]. `out_query` is initialized to null before
/// query decoding or parsing.
///
/// # Safety
///
/// `out_query` must point to writable handle storage and must not overlap any other argument.
/// `query` follows the pointer/length contract in the crate-level documentation. `error` follows
/// [`ClpSErrorBuffer`]'s contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn clp_s_v1_query_compile(
    query: *const u8,
    query_length: usize,
    flags: u32,
    out_query: *mut *mut ClpSQuery,
    error: *mut ClpSErrorBuffer,
) -> ClpSStatus {
    if out_query.is_null() {
        // SAFETY: Forwarded from the function contract.
        return unsafe {
            ffi_entry(error, || {
                Err(ApiError::InvalidArgument(
                    "out_query must not be null".to_owned(),
                ))
            })
        };
    }
    // SAFETY: The caller provides writable output storage. This happens before fallible work.
    unsafe {
        out_query.write(ptr::null_mut());
    }
    // SAFETY: All raw arguments are governed by this function's contract.
    unsafe {
        ffi_entry(error, || {
            if 0 != flags & !CLP_S_QUERY_IGNORE_CASE {
                return Err(ApiError::InvalidArgument(format!(
                    "unknown query flags 0x{flags:08x}"
                )));
            }
            // SAFETY: Forwarded from the function contract and checked by `borrowed_bytes`.
            let query_bytes = borrowed_bytes(query, query_length, "query")?;
            let query_text = str::from_utf8(query_bytes).map_err(|source| {
                ApiError::InvalidArgument(format!("query is not valid UTF-8: {source}"))
            })?;
            let parsed = parse_kql(query_text, KqlLimits::default())
                .map_err(|source| ApiError::Query(format!("failed to parse KQL: {source}")))?;
            let search = SearchOptions::new(
                0 != flags & CLP_S_QUERY_IGNORE_CASE,
                SearchLimits::default(),
            );
            let handle = Box::new(ClpSQuery {
                parsed,
                options: ArchiveSearchOptions::default().with_search(search),
            });
            // SAFETY: `out_query` is valid writable storage and still contains null.
            out_query.write(Box::into_raw(handle));
            Ok(())
        })
    }
}

/// One projected value handed to C.
///
/// `text` borrows the scanner's batch arena and stays valid only until the next
/// [`clp_s_v2_scanner_next_row`] call on the same scanner.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct ClpSV2Value {
    /// One of the `CLP_S_V2_VALUE_*` constants.
    pub kind: u32,
    pub reserved: u32,
    /// Booleans use 0/1, integers their value, timestamps epoch nanoseconds.
    pub integer: i64,
    pub real: f64,
    pub text: *const u8,
    pub text_length: usize,
}

/// One requested projection path, as an escaped dot descriptor.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct ClpSV2ProjectedField {
    pub path: *const u8,
    pub path_length: usize,
}

/// Opaque projected-scan handle.
pub struct ClpSV2Scanner {
    inner: columnar::ProjectedScanner,
    values: Vec<ClpSV2Value>,
}

/// Opens a projected scan over one archive.
///
/// Unlike [`clp_s_v1_search`], which delivers a whole JSON document per match, this reads only the
/// requested paths out of the decoded columns. A path absent from the archive is reported as
/// `CLP_S_V2_VALUE_ABSENT` on every row rather than failing, because a dataset's schemas need not
/// all carry every field.
///
/// # Safety
///
/// `archive_path` and each field path must be readable for their lengths. `query` must be a live
/// handle from [`clp_s_v1_query_compile`] and outlive this call. `out_scanner` must be writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn clp_s_v2_scanner_open(
    archive_path: *const u8,
    archive_path_length: usize,
    query: *const ClpSQuery,
    fields: *const ClpSV2ProjectedField,
    field_count: usize,
    out_scanner: *mut *mut ClpSV2Scanner,
    error: *mut ClpSErrorBuffer,
) -> ClpSStatus {
    if out_scanner.is_null() {
        // SAFETY: Forwarded from the function contract.
        return unsafe {
            ffi_entry(error, || {
                Err(ApiError::InvalidArgument(
                    "out_scanner must not be null".to_owned(),
                ))
            })
        };
    }
    // SAFETY: The caller provides writable output storage. This happens before fallible work.
    unsafe {
        out_scanner.write(ptr::null_mut());
    }
    // SAFETY: All raw arguments are governed by this function's contract.
    unsafe {
        ffi_entry(error, || {
            let query = query.as_ref().ok_or_else(|| {
                ApiError::InvalidArgument("query handle must not be null".to_owned())
            })?;
            let path_bytes = borrowed_bytes(archive_path, archive_path_length, "archive_path")?;
            let path_text = str::from_utf8(path_bytes).map_err(|source| {
                ApiError::InvalidArgument(format!("archive path is not valid UTF-8: {source}"))
            })?;
            if field_count != 0 && fields.is_null() {
                return Err(ApiError::InvalidArgument(
                    "fields must not be null when field_count is non-zero".to_owned(),
                ));
            }
            let mut paths = Vec::with_capacity(field_count);
            for index in 0..field_count {
                let field = *fields.add(index);
                let bytes = borrowed_bytes(field.path, field.path_length, "field path")?;
                let text = str::from_utf8(bytes).map_err(|source| {
                    ApiError::InvalidArgument(format!("field path is not valid UTF-8: {source}"))
                })?;
                paths.push(text.to_owned());
            }
            let archive = ClpSArchive::validate(PathBuf::from(path_text))?;
            let reader = archive.open_reader()?;
            let inner = columnar::ProjectedScanner::open(
                PathBuf::from(path_text),
                reader,
                query.parsed.clone(),
                query.options,
                &paths,
            )
            .map_err(|source| ApiError::Archive(source.to_string()))?;
            let handle = Box::new(ClpSV2Scanner {
                inner,
                values: vec![
                    ClpSV2Value {
                        kind: 0,
                        reserved: 0,
                        integer: 0,
                        real: 0.0,
                        text: ptr::null(),
                        text_length: 0,
                    };
                    field_count
                ],
            });
            // SAFETY: `out_scanner` is valid writable storage and still contains null.
            out_scanner.write(Box::into_raw(handle));
            Ok(())
        })
    }
}

/// Delivers the next matching row's projected values.
///
/// `out_values` must point to `field_count` writable elements. `out_has_row` receives 0 once the
/// archive is exhausted, at which point `out_values` is untouched.
///
/// # Safety
///
/// `scanner` must be a live handle. `out_values` must be writable for the field count the scanner
/// was opened with, and `out_has_row` must be writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn clp_s_v2_scanner_next_row(
    scanner: *mut ClpSV2Scanner,
    out_values: *mut ClpSV2Value,
    out_has_row: *mut u32,
    error: *mut ClpSErrorBuffer,
) -> ClpSStatus {
    // SAFETY: All raw arguments are governed by this function's contract.
    unsafe {
        ffi_entry(error, || {
            let scanner = scanner.as_mut().ok_or_else(|| {
                ApiError::InvalidArgument("scanner handle must not be null".to_owned())
            })?;
            if out_has_row.is_null() {
                return Err(ApiError::InvalidArgument(
                    "out_has_row must not be null".to_owned(),
                ));
            }
            out_has_row.write(0);
            let field_count = scanner.values.len();
            if field_count != 0 && out_values.is_null() {
                return Err(ApiError::InvalidArgument(
                    "out_values must not be null".to_owned(),
                ));
            }
            let Some((cells, arena)) = scanner
                .inner
                .next_row()
                .map_err(|source| ApiError::Archive(source.to_string()))?
            else {
                return Ok(());
            };
            for (index, cell) in cells.iter().enumerate() {
                let text = if 0 == cell.text_length {
                    ptr::null()
                } else {
                    arena.as_ptr().add(cell.text_offset)
                };
                scanner.values[index] = ClpSV2Value {
                    kind: cell.kind,
                    reserved: 0,
                    integer: cell.integer,
                    real: cell.real,
                    text,
                    text_length: cell.text_length,
                };
            }
            for index in 0..field_count {
                out_values.add(index).write(scanner.values[index]);
            }
            out_has_row.write(1);
            Ok(())
        })
    }
}

/// Opaque projected KV-IR scan handle.
pub struct ClpSV2KvIrScanner {
    inner: kv_ir_columnar::KvIrProjectedScanner,
    values: Vec<ClpSV2Value>,
}

/// Opens a projected scan over one KV-IR stream.
///
/// ABI v1 could decode a KV-IR stream but not filter one. This applies the compiled query and
/// returns typed values for the requested paths, the same contract
/// [`clp_s_v2_scanner_open`] provides for archives. A stream still being appended to has no end
/// marker; the complete events before the truncation are returned, which is what a reader of a
/// live segment needs.
///
/// # Safety
///
/// `stream_path` and each field path must be readable for their lengths. `query` must be a live
/// handle from [`clp_s_v1_query_compile`]. `out_scanner` must be writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn clp_s_v2_kv_ir_scanner_open(
    stream_path: *const u8,
    stream_path_length: usize,
    query: *const ClpSQuery,
    fields: *const ClpSV2ProjectedField,
    field_count: usize,
    out_scanner: *mut *mut ClpSV2KvIrScanner,
    error: *mut ClpSErrorBuffer,
) -> ClpSStatus {
    if out_scanner.is_null() {
        // SAFETY: Forwarded from the function contract.
        return unsafe {
            ffi_entry(error, || {
                Err(ApiError::InvalidArgument(
                    "out_scanner must not be null".to_owned(),
                ))
            })
        };
    }
    // SAFETY: The caller provides writable output storage. This happens before fallible work.
    unsafe {
        out_scanner.write(ptr::null_mut());
    }
    // SAFETY: All raw arguments are governed by this function's contract.
    unsafe {
        ffi_entry(error, || {
            let query = query.as_ref().ok_or_else(|| {
                ApiError::InvalidArgument("query handle must not be null".to_owned())
            })?;
            let path_bytes = borrowed_bytes(stream_path, stream_path_length, "stream_path")?;
            let path_text = str::from_utf8(path_bytes).map_err(|source| {
                ApiError::InvalidArgument(format!("stream path is not valid UTF-8: {source}"))
            })?;
            if field_count != 0 && fields.is_null() {
                return Err(ApiError::InvalidArgument(
                    "fields must not be null when field_count is non-zero".to_owned(),
                ));
            }
            let mut paths = Vec::with_capacity(field_count);
            for index in 0..field_count {
                let field = *fields.add(index);
                let bytes = borrowed_bytes(field.path, field.path_length, "field path")?;
                let text = str::from_utf8(bytes).map_err(|source| {
                    ApiError::InvalidArgument(format!("field path is not valid UTF-8: {source}"))
                })?;
                paths.push(text.to_owned());
            }
            let inner = kv_ir_columnar::KvIrProjectedScanner::open(
                std::path::Path::new(path_text),
                &query.parsed,
                query.options.search().ignore_case(),
                &paths,
            )
            .map_err(|source| ApiError::Archive(source.to_string()))?;
            let handle = Box::new(ClpSV2KvIrScanner {
                inner,
                values: vec![
                    ClpSV2Value {
                        kind: 0,
                        reserved: 0,
                        integer: 0,
                        real: 0.0,
                        text: ptr::null(),
                        text_length: 0,
                    };
                    field_count
                ],
            });
            // SAFETY: `out_scanner` is valid writable storage and still contains null.
            out_scanner.write(Box::into_raw(handle));
            Ok(())
        })
    }
}

/// Delivers the next matching event's projected values.
///
/// # Safety
///
/// `scanner` must be a live handle, `out_values` writable for its field count, and `out_has_row`
/// writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn clp_s_v2_kv_ir_scanner_next_row(
    scanner: *mut ClpSV2KvIrScanner,
    out_values: *mut ClpSV2Value,
    out_has_row: *mut u32,
    error: *mut ClpSErrorBuffer,
) -> ClpSStatus {
    // SAFETY: All raw arguments are governed by this function's contract.
    unsafe {
        ffi_entry(error, || {
            let scanner = scanner.as_mut().ok_or_else(|| {
                ApiError::InvalidArgument("scanner handle must not be null".to_owned())
            })?;
            if out_has_row.is_null() {
                return Err(ApiError::InvalidArgument(
                    "out_has_row must not be null".to_owned(),
                ));
            }
            out_has_row.write(0);
            let field_count = scanner.values.len();
            if field_count != 0 && out_values.is_null() {
                return Err(ApiError::InvalidArgument(
                    "out_values must not be null".to_owned(),
                ));
            }
            let Some((cells, arena)) = scanner.inner.next_row() else {
                return Ok(());
            };
            for (index, cell) in cells.iter().enumerate() {
                let text = if 0 == cell.text_length {
                    ptr::null()
                } else {
                    arena.as_ptr().add(cell.text_offset)
                };
                scanner.values[index] = ClpSV2Value {
                    kind: cell.kind,
                    reserved: 0,
                    integer: cell.integer,
                    real: cell.real,
                    text,
                    text_length: cell.text_length,
                };
            }
            for index in 0..field_count {
                out_values.add(index).write(scanner.values[index]);
            }
            out_has_row.write(1);
            Ok(())
        })
    }
}

/// Frees a KV-IR scanner handle. A null handle is a no-op.
///
/// # Safety
///
/// A non-null pointer must have been returned by [`clp_s_v2_kv_ir_scanner_open`] and must not have
/// been freed already.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn clp_s_v2_kv_ir_scanner_free(scanner: *mut ClpSV2KvIrScanner) {
    if scanner.is_null() {
        return;
    }
    // SAFETY: Forwarded from the function contract.
    unsafe {
        drop(Box::from_raw(scanner));
    }
}

/// Frees a scanner handle. A null handle is a no-op.
///
/// # Safety
///
/// A non-null pointer must have been returned by [`clp_s_v2_scanner_open`] and must not have been
/// freed already.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn clp_s_v2_scanner_free(scanner: *mut ClpSV2Scanner) {
    if scanner.is_null() {
        return;
    }
    // SAFETY: Forwarded from the function contract.
    unsafe {
        drop(Box::from_raw(scanner));
    }
}

/// Frees a query handle. A null handle is a no-op.
///
/// # Safety
///
/// A non-null pointer must have been returned by [`clp_s_v1_query_compile`], must not have been
/// freed already, and must not be in use by any concurrent operation.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn clp_s_v1_query_free(query: *mut ClpSQuery) {
    if query.is_null() {
        return;
    }
    let _ = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: The caller transfers unique ownership of one live handle.
        drop(unsafe { Box::from_raw(query) });
    }));
}

/// Creates a bounded KV-IR serializer and stages its preamble.
///
/// A null `options` selects all defaults, including four-byte encoding. A null metadata pointer
/// with zero length means no user-defined metadata; a non-null pointer with zero length supplies
/// invalid empty JSON. `out_serializer` is initialized to null before any validation or work.
///
/// # Safety
///
/// `out_serializer` must point to writable handle storage. A non-null `options` must point to one
/// readable complete [`ClpSKvIrSerializerOptions`]. Metadata follows the pointer/length contract.
/// Every region must remain live, disjoint, and unused concurrently for this synchronous call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn clp_s_v1_kv_ir_serializer_new(
    options: *const ClpSKvIrSerializerOptions,
    user_defined_metadata_json: *const u8,
    user_defined_metadata_json_length: usize,
    out_serializer: *mut *mut ClpSKvIrSerializer,
    error: *mut ClpSErrorBuffer,
) -> ClpSStatus {
    if out_serializer.is_null() {
        // SAFETY: Forwarded from this function's caller contract.
        return unsafe {
            ffi_entry(error, || {
                Err(ApiError::InvalidArgument(
                    "out_serializer must not be null".to_owned(),
                ))
            })
        };
    }
    // SAFETY: The caller provides writable output storage. This happens before fallible work.
    unsafe {
        out_serializer.write(ptr::null_mut());
    }
    // SAFETY: All raw arguments are governed by this function's contract.
    unsafe {
        ffi_entry(error, || {
            let options = kv_ir_serializer_options(options)?;
            let metadata = optional_borrowed_bytes(
                user_defined_metadata_json,
                user_defined_metadata_json_length,
                "user-defined metadata JSON",
            )?;
            let serializer = KvIrSerializer::new(options, metadata)
                .map_err(|source| kv_ir_api_error(&source))?;
            out_serializer.write(Box::into_raw(Box::new(ClpSKvIrSerializer { serializer })));
            Ok(())
        })
    }
}

/// Frees a KV-IR serializer handle. A null handle is a no-op.
///
/// # Safety
///
/// A non-null pointer must have been returned by [`clp_s_v1_kv_ir_serializer_new`], must not have
/// been freed already, and must not have an active operation or borrowed pending view.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn clp_s_v1_kv_ir_serializer_free(serializer: *mut ClpSKvIrSerializer) {
    if serializer.is_null() {
        return;
    }
    let _ = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: The caller transfers unique ownership of one live handle.
        drop(unsafe { Box::from_raw(serializer) });
    }));
}

/// Transactionally appends one log event encoded as two length-delimited `MessagePack` maps.
///
/// Each input contributes its first root map; compatibility bytes following that map are ignored.
/// `event_bytes` is initialized to zero and, on success, receives the exact number of newly
/// committed bytes, including new schema-node packets. A failed event commits no schema or output.
/// A successful call invalidates every earlier pending view.
///
/// # Safety
///
/// `serializer` must be one live handle used exclusively for this call. Both maps follow the
/// pointer/length contract. `event_bytes` is mandatory writable storage. All regions must be
/// disjoint and remain live through the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn clp_s_v1_kv_ir_serializer_append_msgpack_maps(
    serializer: *mut ClpSKvIrSerializer,
    auto_generated: *const u8,
    auto_generated_length: usize,
    user_generated: *const u8,
    user_generated_length: usize,
    event_bytes: *mut u64,
    error: *mut ClpSErrorBuffer,
) -> ClpSStatus {
    if event_bytes.is_null() {
        // SAFETY: Forwarded from this function's caller contract.
        return unsafe {
            ffi_entry(error, || {
                Err(ApiError::InvalidArgument(
                    "event_bytes must not be null".to_owned(),
                ))
            })
        };
    }
    // SAFETY: The caller provides writable output storage. This happens before fallible work.
    unsafe {
        event_bytes.write(0);
    }
    // SAFETY: All raw arguments are governed by this function's contract.
    unsafe {
        ffi_entry(error, || {
            let serializer = serializer.as_mut().ok_or_else(|| {
                ApiError::InvalidArgument("KV-IR serializer handle must not be null".to_owned())
            })?;
            let auto_generated = borrowed_bytes(
                auto_generated,
                auto_generated_length,
                "auto-generated MessagePack map",
            )?;
            let user_generated = borrowed_bytes(
                user_generated,
                user_generated_length,
                "user-generated MessagePack map",
            )?;
            let committed = serializer
                .serializer
                .serialize_log_event_from_msgpack_maps(auto_generated, user_generated)
                .map_err(|source| kv_ir_api_error(&source))?;
            let committed = u64::try_from(committed).map_err(|_| {
                ApiError::LimitExceeded("KV-IR event byte count exceeds u64".to_owned())
            })?;
            event_bytes.write(committed);
            Ok(())
        })
    }
}

/// Appends one signed-millisecond UTC-offset packet.
///
/// `packet_bytes` is initialized to zero and is nine on success. A successful call invalidates
/// every earlier pending view.
///
/// # Safety
///
/// `serializer` must be one live handle used exclusively for this call. `packet_bytes` is
/// mandatory writable storage disjoint from the handle and optional error storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn clp_s_v1_kv_ir_serializer_change_utc_offset(
    serializer: *mut ClpSKvIrSerializer,
    utc_offset_millis: i64,
    packet_bytes: *mut u64,
    error: *mut ClpSErrorBuffer,
) -> ClpSStatus {
    if packet_bytes.is_null() {
        // SAFETY: Forwarded from this function's caller contract.
        return unsafe {
            ffi_entry(error, || {
                Err(ApiError::InvalidArgument(
                    "packet_bytes must not be null".to_owned(),
                ))
            })
        };
    }
    // SAFETY: The caller provides writable output storage. This happens before fallible work.
    unsafe {
        packet_bytes.write(0);
    }
    // SAFETY: All raw arguments are governed by this function's contract.
    unsafe {
        ffi_entry(error, || {
            let serializer = serializer.as_mut().ok_or_else(|| {
                ApiError::InvalidArgument("KV-IR serializer handle must not be null".to_owned())
            })?;
            let committed = serializer
                .serializer
                .change_utc_offset(utc_offset_millis)
                .map_err(|source| kv_ir_api_error(&source))?;
            let committed = u64::try_from(committed).map_err(|_| {
                ApiError::LimitExceeded("KV-IR UTC-offset packet length exceeds u64".to_owned())
            })?;
            packet_bytes.write(committed);
            Ok(())
        })
    }
}

/// Borrows all currently pending serializer output without copying it.
///
/// `out_view` is initialized to a null, empty view before validation. Its bytes remain valid only
/// until the next successful append, UTC-offset change, consume, finish, or free. Read-only stats
/// and total-byte calls do not invalidate it.
///
/// # Safety
///
/// `serializer` must be one live handle. `out_view` is mandatory writable storage disjoint from
/// the handle and optional error storage. The handle must not be mutated or freed while the caller
/// reads the returned view.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn clp_s_v1_kv_ir_serializer_pending_view(
    serializer: *const ClpSKvIrSerializer,
    out_view: *mut ClpSKvIrPendingView,
    error: *mut ClpSErrorBuffer,
) -> ClpSStatus {
    if out_view.is_null() {
        // SAFETY: Forwarded from this function's caller contract.
        return unsafe {
            ffi_entry(error, || {
                Err(ApiError::InvalidArgument(
                    "out_view must not be null".to_owned(),
                ))
            })
        };
    }
    // SAFETY: The caller provides writable output storage. This happens before fallible work.
    unsafe {
        out_view.write(ClpSKvIrPendingView {
            data: ptr::null(),
            length: 0,
        });
    }
    // SAFETY: All raw arguments are governed by this function's contract.
    unsafe {
        ffi_entry(error, || {
            let serializer = serializer.as_ref().ok_or_else(|| {
                ApiError::InvalidArgument("KV-IR serializer handle must not be null".to_owned())
            })?;
            let pending = serializer.serializer.pending_output();
            out_view.write(ClpSKvIrPendingView {
                data: if pending.is_empty() {
                    ptr::null()
                } else {
                    pending.as_ptr()
                },
                length: pending.len(),
            });
            Ok(())
        })
    }
}

/// Consumes exactly one prefix of the current pending view without copying remaining bytes.
///
/// Consuming more bytes than are pending is an invalid argument. A successful call invalidates
/// every earlier pending view, including when `bytes` is zero.
///
/// # Safety
///
/// `serializer` must be one live handle used exclusively for this call and disjoint from optional
/// error storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn clp_s_v1_kv_ir_serializer_consume(
    serializer: *mut ClpSKvIrSerializer,
    bytes: usize,
    error: *mut ClpSErrorBuffer,
) -> ClpSStatus {
    // SAFETY: All raw arguments are governed by this function's contract.
    unsafe {
        ffi_entry(error, || {
            let serializer = serializer.as_mut().ok_or_else(|| {
                ApiError::InvalidArgument("KV-IR serializer handle must not be null".to_owned())
            })?;
            let pending = serializer.serializer.pending_output().len();
            if bytes > pending {
                return Err(ApiError::InvalidArgument(format!(
                    "cannot consume {bytes} KV-IR byte(s); only {pending} are pending"
                )));
            }
            serializer
                .serializer
                .consume_pending(bytes)
                .map_err(|source| kv_ir_api_error(&source))
        })
    }
}

/// Appends the one-byte KV-IR end-of-stream marker exactly once.
///
/// `eof_bytes` is initialized to zero and is one on success. A successful call invalidates every
/// earlier pending view. A repeated call returns [`CLP_S_STATUS_INVALID_STATE`].
///
/// # Safety
///
/// `serializer` must be one live handle used exclusively for this call. `eof_bytes` is mandatory
/// writable storage disjoint from the handle and optional error storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn clp_s_v1_kv_ir_serializer_finish(
    serializer: *mut ClpSKvIrSerializer,
    eof_bytes: *mut u64,
    error: *mut ClpSErrorBuffer,
) -> ClpSStatus {
    if eof_bytes.is_null() {
        // SAFETY: Forwarded from this function's caller contract.
        return unsafe {
            ffi_entry(error, || {
                Err(ApiError::InvalidArgument(
                    "eof_bytes must not be null".to_owned(),
                ))
            })
        };
    }
    // SAFETY: The caller provides writable output storage. This happens before fallible work.
    unsafe {
        eof_bytes.write(0);
    }
    // SAFETY: All raw arguments are governed by this function's contract.
    unsafe {
        ffi_entry(error, || {
            let serializer = serializer.as_mut().ok_or_else(|| {
                ApiError::InvalidArgument("KV-IR serializer handle must not be null".to_owned())
            })?;
            let committed = serializer
                .serializer
                .finish()
                .map_err(|source| kv_ir_api_error(&source))?;
            let committed = u64::try_from(committed).map_err(|_| {
                ApiError::LimitExceeded("KV-IR end marker length exceeds u64".to_owned())
            })?;
            eof_bytes.write(committed);
            Ok(())
        })
    }
}

/// Copies cumulative serializer statistics into caller-owned storage.
///
/// `out_stats` is zero-initialized before validation. This read-only call does not invalidate a
/// pending view.
///
/// # Safety
///
/// `serializer` must be one live handle. `out_stats` is mandatory writable storage disjoint from
/// the handle and optional error storage. No call may race handle destruction or mutation.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn clp_s_v1_kv_ir_serializer_stats(
    serializer: *const ClpSKvIrSerializer,
    out_stats: *mut ClpSKvIrSerializerStats,
    error: *mut ClpSErrorBuffer,
) -> ClpSStatus {
    if out_stats.is_null() {
        // SAFETY: Forwarded from this function's caller contract.
        return unsafe {
            ffi_entry(error, || {
                Err(ApiError::InvalidArgument(
                    "out_stats must not be null".to_owned(),
                ))
            })
        };
    }
    // SAFETY: The caller provides writable output storage. This happens before fallible work.
    unsafe {
        out_stats.write(ClpSKvIrSerializerStats::default());
    }
    // SAFETY: All raw arguments are governed by this function's contract.
    unsafe {
        ffi_entry(error, || {
            let serializer = serializer.as_ref().ok_or_else(|| {
                ApiError::InvalidArgument("KV-IR serializer handle must not be null".to_owned())
            })?;
            out_stats.write(kv_ir_stats(&serializer.serializer)?);
            Ok(())
        })
    }
}

/// Returns the cumulative committed byte count, including bytes already consumed.
///
/// `out_total_bytes` is initialized to zero before validation. This read-only call does not
/// invalidate a pending view.
///
/// # Safety
///
/// `serializer` must be one live handle. `out_total_bytes` is mandatory writable storage disjoint
/// from the handle and optional error storage. No call may race handle destruction or mutation.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn clp_s_v1_kv_ir_serializer_total_bytes(
    serializer: *const ClpSKvIrSerializer,
    out_total_bytes: *mut u64,
    error: *mut ClpSErrorBuffer,
) -> ClpSStatus {
    if out_total_bytes.is_null() {
        // SAFETY: Forwarded from this function's caller contract.
        return unsafe {
            ffi_entry(error, || {
                Err(ApiError::InvalidArgument(
                    "out_total_bytes must not be null".to_owned(),
                ))
            })
        };
    }
    // SAFETY: The caller provides writable output storage. This happens before fallible work.
    unsafe {
        out_total_bytes.write(0);
    }
    // SAFETY: All raw arguments are governed by this function's contract.
    unsafe {
        ffi_entry(error, || {
            let serializer = serializer.as_ref().ok_or_else(|| {
                ApiError::InvalidArgument("KV-IR serializer handle must not be null".to_owned())
            })?;
            out_total_bytes.write(serializer.serializer.stats().serialized_bytes());
            Ok(())
        })
    }
}

/// Creates a pull-based KV-IR deserializer and validates its first stream preamble immediately.
///
/// A null `options` selects core defaults. The callback may be invoked synchronously before this
/// function returns. `out_deserializer` is initialized to null before validation or callback work.
/// Empty or truncated preamble input returns [`CLP_S_STATUS_KV_IR_INCOMPLETE`].
///
/// # Safety
///
/// `out_deserializer` must point to writable handle storage. `callback` must obey
/// [`ClpSKvIrReadCallback`]'s contract and remain callable, with `context` valid, until the
/// returned handle is freed. A non-null options pointer must name a readable complete structure.
/// All caller regions must remain live and disjoint during this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn clp_s_v1_kv_ir_deserializer_new(
    options: *const ClpSKvIrDeserializerOptions,
    callback: Option<ClpSKvIrReadCallback>,
    context: *mut c_void,
    out_deserializer: *mut *mut ClpSKvIrDeserializer,
    error: *mut ClpSErrorBuffer,
) -> ClpSStatus {
    if out_deserializer.is_null() {
        // SAFETY: Forwarded from this function's caller contract.
        return unsafe {
            ffi_entry(error, || {
                Err(ApiError::InvalidArgument(
                    "out_deserializer must not be null".to_owned(),
                ))
            })
        };
    }
    // SAFETY: The caller provides writable output storage. This happens before fallible work.
    unsafe {
        out_deserializer.write(ptr::null_mut());
    }
    // SAFETY: All raw arguments are governed by this function's contract.
    unsafe {
        ffi_entry(error, || {
            let callback = callback.ok_or_else(|| {
                ApiError::InvalidArgument("KV-IR input callback must not be null".to_owned())
            })?;
            let (max_read_chunk, event_limits) = kv_ir_deserializer_options(options)?;
            let deserializer =
                ClpSKvIrDeserializer::create(callback, context, max_read_chunk, event_limits)?;
            out_deserializer.write(Box::into_raw(Box::new(deserializer)));
            Ok(())
        })
    }
}

/// Frees a KV-IR deserializer. A null handle is a no-op.
///
/// Events returned by this deserializer are independent and remain valid. A non-null handle must
/// have no active operation or metadata borrow.
///
/// # Safety
///
/// A non-null pointer must have been returned by [`clp_s_v1_kv_ir_deserializer_new`], must not have
/// been freed already, and must not be used concurrently.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn clp_s_v1_kv_ir_deserializer_free(deserializer: *mut ClpSKvIrDeserializer) {
    if deserializer.is_null() {
        return;
    }
    let _ = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: The caller transfers unique ownership of one live handle.
        drop(unsafe { Box::from_raw(deserializer) });
    }));
}

/// Borrows the exact validated full metadata JSON payload copied during construction.
///
/// `out_view` is initialized to null and empty before validation. Its bytes remain unchanged until
/// the deserializer is freed; they need not be reparsed after reader advancement.
///
/// # Safety
///
/// `deserializer` must be one live handle. `out_view` is mandatory writable storage disjoint from
/// the handle and optional error storage. The handle must not be freed while the view is used.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn clp_s_v1_kv_ir_deserializer_metadata_view(
    deserializer: *const ClpSKvIrDeserializer,
    out_view: *mut ClpSKvIrByteView,
    error: *mut ClpSErrorBuffer,
) -> ClpSStatus {
    if out_view.is_null() {
        // SAFETY: Forwarded from this function's caller contract.
        return unsafe {
            ffi_entry(error, || {
                Err(ApiError::InvalidArgument(
                    "out_view must not be null".to_owned(),
                ))
            })
        };
    }
    // SAFETY: The caller provides writable output storage. This happens before fallible work.
    unsafe {
        out_view.write(ClpSKvIrByteView {
            data: ptr::null(),
            length: 0,
        });
    }
    // SAFETY: All raw arguments are governed by this function's contract.
    unsafe {
        ffi_entry(error, || {
            let deserializer = deserializer.as_ref().ok_or_else(|| {
                ApiError::InvalidArgument("KV-IR deserializer handle must not be null".to_owned())
            })?;
            let metadata = deserializer.metadata_json.as_slice();
            let length = u64::try_from(metadata.len()).map_err(|_| {
                ApiError::LimitExceeded("KV-IR metadata length exceeds u64".to_owned())
            })?;
            out_view.write(ClpSKvIrByteView {
                data: borrowed_or_null(metadata),
                length,
            });
            Ok(())
        })
    }
}

/// Pulls until one complete independent log event or the first explicit stream end.
///
/// Schema and UTC-offset units are processed internally. `out_event` is initialized to null. On
/// success it owns one event that survives reader advancement and deserializer destruction. The
/// first explicit stream end returns [`CLP_S_STATUS_EOF`], permanently ignores concatenated
/// streams, and every later call returns the same EOF status without invoking the callback.
///
/// # Safety
///
/// `deserializer` must be one live handle used exclusively for this call. `out_event` is mandatory
/// writable storage disjoint from the handle, callback state, and optional error storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn clp_s_v1_kv_ir_deserializer_next_event(
    deserializer: *mut ClpSKvIrDeserializer,
    out_event: *mut *mut ClpSKvIrEvent,
    error: *mut ClpSErrorBuffer,
) -> ClpSStatus {
    if out_event.is_null() {
        // SAFETY: Forwarded from this function's caller contract.
        return unsafe {
            ffi_entry(error, || {
                Err(ApiError::InvalidArgument(
                    "out_event must not be null".to_owned(),
                ))
            })
        };
    }
    // SAFETY: The caller provides writable output storage. This happens before fallible work.
    unsafe {
        out_event.write(ptr::null_mut());
    }
    let mut reached_eof = false;
    // SAFETY: All raw arguments are governed by this function's contract.
    let status = unsafe {
        ffi_entry(error, || {
            let deserializer = deserializer.as_mut().ok_or_else(|| {
                ApiError::InvalidArgument("KV-IR deserializer handle must not be null".to_owned())
            })?;
            match deserializer.next_owned_event()? {
                Some(event) => {
                    out_event.write(Box::into_raw(Box::new(ClpSKvIrEvent { event })));
                }
                None => reached_eof = true,
            }
            Ok(())
        })
    };
    if status == CLP_S_STATUS_OK && reached_eof {
        CLP_S_STATUS_EOF
    } else {
        status
    }
}

/// Pulls one independent event and returns its immutable view in the same ABI call.
///
/// This is equivalent to a successful [`clp_s_v1_kv_ir_deserializer_next_event`] immediately
/// followed by [`clp_s_v1_kv_ir_event_view`], but avoids a second panic boundary and status/error
/// round trip. `out_event` is initialized to null and `out_view` to empty before validation. On
/// success the view remains valid until its event handle is freed. EOF follows the same terminal
/// semantics as the separate next-event call and leaves both outputs empty.
///
/// # Safety
///
/// `deserializer` must be one live handle used exclusively for this call. Both outputs are
/// mandatory writable storage disjoint from each other, the handle, callback state, and optional
/// error storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn clp_s_v1_kv_ir_deserializer_next_event_with_view(
    deserializer: *mut ClpSKvIrDeserializer,
    out_event: *mut *mut ClpSKvIrEvent,
    out_view: *mut ClpSKvIrEventView,
    error: *mut ClpSErrorBuffer,
) -> ClpSStatus {
    if !out_event.is_null() {
        // SAFETY: The caller provides writable output storage.
        unsafe {
            out_event.write(ptr::null_mut());
        }
    }
    if !out_view.is_null() {
        // SAFETY: The caller provides writable output storage.
        unsafe {
            out_view.write(empty_kv_ir_event_view());
        }
    }
    if out_event.is_null() || out_view.is_null() {
        // SAFETY: Forwarded from this function's caller contract.
        return unsafe {
            ffi_entry(error, || {
                Err(ApiError::InvalidArgument(
                    "out_event and out_view must not be null".to_owned(),
                ))
            })
        };
    }

    let mut reached_eof = false;
    // SAFETY: All raw arguments are governed by this function's contract.
    let status = unsafe {
        ffi_entry(error, || {
            let deserializer = deserializer.as_mut().ok_or_else(|| {
                ApiError::InvalidArgument("KV-IR deserializer handle must not be null".to_owned())
            })?;
            match deserializer.next_owned_event()? {
                Some(event) => {
                    let view = kv_ir_event_view(&event)?;
                    let event = Box::into_raw(Box::new(ClpSKvIrEvent { event }));
                    out_event.write(event);
                    out_view.write(view);
                }
                None => reached_eof = true,
            }
            Ok(())
        })
    };
    if status == CLP_S_STATUS_OK && reached_eof {
        CLP_S_STATUS_EOF
    } else {
        status
    }
}

/// Borrows the arena, zero-copy node arrays, and scalar/index metadata of one owned event.
///
/// `out_view` is zero-initialized before validation. Every returned pointer remains valid until
/// the event is freed. Keys, strings, and reconstructed array data are arbitrary length-delimited
/// bytes and are not guaranteed to be UTF-8 or NUL-free.
///
/// # Safety
///
/// `event` must be one live event handle. `out_view` is mandatory writable storage disjoint from
/// the handle and optional error storage. No call may race event destruction.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn clp_s_v1_kv_ir_event_view(
    event: *const ClpSKvIrEvent,
    out_view: *mut ClpSKvIrEventView,
    error: *mut ClpSErrorBuffer,
) -> ClpSStatus {
    if out_view.is_null() {
        // SAFETY: Forwarded from this function's caller contract.
        return unsafe {
            ffi_entry(error, || {
                Err(ApiError::InvalidArgument(
                    "out_view must not be null".to_owned(),
                ))
            })
        };
    }
    // SAFETY: The caller provides writable output storage. This happens before fallible work.
    unsafe {
        out_view.write(empty_kv_ir_event_view());
    }
    // SAFETY: All raw arguments are governed by this function's contract.
    unsafe {
        ffi_entry(error, || {
            let event = event.as_ref().ok_or_else(|| {
                ApiError::InvalidArgument("KV-IR event handle must not be null".to_owned())
            })?;
            out_view.write(kv_ir_event_view(&event.event)?);
            Ok(())
        })
    }
}

/// Frees an independently owned KV-IR event. A null handle is a no-op.
///
/// # Safety
///
/// A non-null pointer must have been returned by [`clp_s_v1_kv_ir_deserializer_next_event`], must
/// not have been freed already, and must have no active view use.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn clp_s_v1_kv_ir_event_free(event: *mut ClpSKvIrEvent) {
    if event.is_null() {
        return;
    }
    let _ = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: The caller transfers unique ownership of one live handle.
        drop(unsafe { Box::from_raw(event) });
    }));
}

/// Extracts complete JSON documents through a synchronous callback.
///
/// `flags` accepts only [`CLP_S_EXTRACT_LOG_ORDER`]. `records_delivered` is optional, initialized
/// to zero, and updated on every ordinary return, including callback cancellation or failure.
///
/// # Safety
///
/// `archive` must be a live handle of the correct type. `callback` must not unwind and must obey
/// the callback contract. Optional output/error pointers must remain writable and must not overlap
/// the handles, callback state, or each other. No handle may be freed during this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn clp_s_v1_extract(
    archive: *const ClpSArchive,
    flags: u32,
    callback: Option<ClpSRecordCallback>,
    user_data: *mut c_void,
    records_delivered: *mut u64,
    error: *mut ClpSErrorBuffer,
) -> ClpSStatus {
    if !records_delivered.is_null() {
        // SAFETY: The caller supplies writable optional counter storage.
        unsafe {
            records_delivered.write(0);
        }
    }
    // SAFETY: All raw arguments are governed by this function's contract.
    unsafe {
        ffi_entry(error, || {
            let archive = archive.as_ref().ok_or_else(|| {
                ApiError::InvalidArgument("archive handle must not be null".to_owned())
            })?;
            if 0 != flags & !CLP_S_EXTRACT_LOG_ORDER {
                return Err(ApiError::InvalidArgument(format!(
                    "unknown extraction flags 0x{flags:08x}"
                )));
            }
            let callback = callback.ok_or_else(|| {
                ApiError::InvalidArgument("record callback must not be null".to_owned())
            })?;
            let mut reader = archive.open_reader()?;
            let mode = if 0 == flags & CLP_S_EXTRACT_LOG_ORDER {
                ExtractionMode::Unordered
            } else {
                ExtractionMode::LogOrder
            };
            let options =
                ExtractionOptions::new(mode).with_byte_policy(JsonBytePolicy::PreserveInvalidUtf8);
            let mut sink = CallbackSink::new(callback, user_data);
            let result = extract_jsonl_records(reader.as_mut(), &mut sink, options);
            if !records_delivered.is_null() {
                // SAFETY: The caller supplies writable optional counter storage.
                records_delivered.write(sink.delivered);
            }
            result
                .map(|_stats| ())
                .map_err(|source| callback_result(sink.stop, source))
        })
    }
}

/// Searches an archive in physical order and delivers complete projected JSON documents.
///
/// ABI v1 projects all fields and uses the query's case policy. `records_delivered` is optional,
/// initialized to zero, and updated on every ordinary return, including callback cancellation or
/// failure.
///
/// # Safety
///
/// `archive` and `query` must be live handles of the correct types. `callback` must not unwind and
/// must obey the callback contract. Optional output/error pointers must remain writable and must
/// not overlap the handles, callback state, or each other. No handle may be freed during this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn clp_s_v1_search(
    archive: *const ClpSArchive,
    query: *const ClpSQuery,
    callback: Option<ClpSRecordCallback>,
    user_data: *mut c_void,
    records_delivered: *mut u64,
    error: *mut ClpSErrorBuffer,
) -> ClpSStatus {
    if !records_delivered.is_null() {
        // SAFETY: The caller supplies writable optional counter storage.
        unsafe {
            records_delivered.write(0);
        }
    }
    // SAFETY: All raw arguments are governed by this function's contract.
    unsafe {
        ffi_entry(error, || {
            let archive = archive.as_ref().ok_or_else(|| {
                ApiError::InvalidArgument("archive handle must not be null".to_owned())
            })?;
            let query = query.as_ref().ok_or_else(|| {
                ApiError::InvalidArgument("query handle must not be null".to_owned())
            })?;
            let callback = callback.ok_or_else(|| {
                ApiError::InvalidArgument("record callback must not be null".to_owned())
            })?;
            let mut reader = archive.open_reader()?;
            let jsonl_options =
                SearchJsonlOptions::default().with_byte_policy(JsonBytePolicy::PreserveInvalidUtf8);
            let mut callback_sink = CallbackSink::new(callback, user_data);
            let mut jsonl = SearchJsonlAdapter::new(&mut callback_sink, &jsonl_options);
            let result = search_archive(reader.as_mut(), &query.parsed, &mut jsonl, &query.options);
            if !records_delivered.is_null() {
                // SAFETY: The caller supplies writable optional counter storage.
                records_delivered.write(callback_sink.delivered);
            }
            result
                .map(|_stats| ())
                .map_err(|source| callback_result(callback_sink.stop, source))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ffi_entry_contains_internal_panics() {
        // SAFETY: A null error buffer discards diagnostic text.
        let status = unsafe { ffi_entry(ptr::null_mut(), || panic!("contained")) };
        assert_eq!(CLP_S_STATUS_PANIC, status);
    }

    #[test]
    fn error_writer_escapes_nul_and_terminates_a_short_buffer() {
        let mut bytes = [0xff_u8; 5];
        let mut buffer = ClpSErrorBuffer {
            data: bytes.as_mut_ptr(),
            capacity: bytes.len(),
            required: 0,
        };
        // SAFETY: `buffer` and its byte region remain writable for this call.
        let status = unsafe {
            ffi_entry(&raw mut buffer, || {
                Err(ApiError::InvalidArgument("a\0bc".to_owned()))
            })
        };
        assert_eq!(CLP_S_STATUS_INVALID_ARGUMENT, status);
        assert_eq!(6, buffer.required);
        assert_eq!(b"a\\0b\0", &bytes);
    }

    #[test]
    fn null_zero_length_input_never_constructs_a_null_slice() {
        // SAFETY: Null with zero length is explicitly accepted.
        let bytes = unsafe { borrowed_bytes(ptr::null(), 0, "test") }.expect("empty input");
        assert_eq!(bytes, []);
    }
}
