#ifndef CLP_S_FFI_H
#define CLP_S_FFI_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define CLP_S_ABI_VERSION UINT32_C(1)

typedef uint32_t clp_s_status;

#define CLP_S_STATUS_OK UINT32_C(0)
#define CLP_S_STATUS_INVALID_ARGUMENT UINT32_C(1)
#define CLP_S_STATUS_IO UINT32_C(2)
#define CLP_S_STATUS_ARCHIVE UINT32_C(3)
#define CLP_S_STATUS_QUERY UINT32_C(4)
#define CLP_S_STATUS_CANCELLED UINT32_C(5)
#define CLP_S_STATUS_CALLBACK_ERROR UINT32_C(6)
#define CLP_S_STATUS_PANIC UINT32_C(7)
#define CLP_S_STATUS_BUFFER_TOO_SMALL UINT32_C(8)
#define CLP_S_STATUS_KV_IR_INVALID_DATA UINT32_C(9)
#define CLP_S_STATUS_LIMIT_EXCEEDED UINT32_C(10)
#define CLP_S_STATUS_ALLOCATION_FAILED UINT32_C(11)
#define CLP_S_STATUS_INVALID_STATE UINT32_C(12)
#define CLP_S_STATUS_KV_IR_INPUT_CALLBACK_ERROR UINT32_C(13)
#define CLP_S_STATUS_KV_IR_INCOMPLETE UINT32_C(14)
#define CLP_S_STATUS_EOF UINT32_C(15)
#define CLP_S_STATUS_KV_IR_ROOT_NOT_MAP UINT32_C(16)

#define CLP_S_CALLBACK_CONTINUE UINT32_C(0)
#define CLP_S_CALLBACK_CANCEL UINT32_C(1)
#define CLP_S_CALLBACK_ERROR UINT32_C(2)

#define CLP_S_EXTRACT_LOG_ORDER UINT32_C(1)
#define CLP_S_QUERY_IGNORE_CASE UINT32_C(1)

#define CLP_S_KV_IR_ENCODING_DEFAULT UINT32_C(0)
#define CLP_S_KV_IR_ENCODING_FOUR_BYTE UINT32_C(4)
#define CLP_S_KV_IR_ENCODING_EIGHT_BYTE UINT32_C(8)

#define CLP_S_KV_IR_READ_OK UINT32_C(0)
#define CLP_S_KV_IR_READ_ERROR UINT32_C(1)

#define CLP_S_KV_IR_VALUE_OBJECT UINT32_C(0)
#define CLP_S_KV_IR_VALUE_INTEGER UINT32_C(1)
#define CLP_S_KV_IR_VALUE_FLOAT UINT32_C(2)
#define CLP_S_KV_IR_VALUE_BOOLEAN UINT32_C(3)
#define CLP_S_KV_IR_VALUE_STRING UINT32_C(4)
#define CLP_S_KV_IR_VALUE_ARRAY_JSON UINT32_C(5)
#define CLP_S_KV_IR_VALUE_NULL UINT32_C(6)
#define CLP_S_KV_IR_VALUE_EMPTY_OBJECT UINT32_C(7)

/**
 * Caller-owned diagnostic storage.
 *
 * `required` is zero on success. On failure it includes the trailing NUL even when `capacity` is
 * too short. If `capacity` is nonzero, `data` must point to that many writable bytes; output is
 * always NUL-terminated. Embedded NUL bytes in source diagnostics are emitted as `\\0`.
 */
typedef struct clp_s_error_buffer {
    uint8_t *data;
    size_t capacity;
    size_t required;
} clp_s_error_buffer;

/**
 * One complete JSON document borrowed only for the synchronous callback invocation.
 *
 * The bytes exclude the JSONL newline and use C++-compatible JSON-form escaping. Byte-preserved
 * archive strings can contain non-UTF-8 bytes; callers requiring standards-valid UTF-8 must
 * validate and reject such a record.
 */
typedef struct clp_s_record {
    const uint8_t *json;
    size_t json_length;
    uint64_t table_index;
    uint64_t row_index;
    uint64_t log_event_idx;
    uint32_t has_log_event_idx;
    uint32_t reserved;
} clp_s_record;

/**
 * Fixed-width KV-IR serializer options.
 *
 * `struct_size` must equal sizeof(clp_s_kv_ir_serializer_options). Encoding is DEFAULT,
 * FOUR_BYTE, or EIGHT_BYTE; DEFAULT selects FOUR_BYTE in ABI v1. A zero limit selects the
 * core-library default. Every reserved element must be zero.
 */
typedef struct clp_s_kv_ir_serializer_options {
    uint32_t struct_size;
    uint32_t encoding;
    uint64_t max_input_bytes_per_map;
    uint64_t max_pending_output_bytes;
    uint64_t max_event_output_bytes;
    uint64_t max_metadata_bytes;
    uint64_t max_schema_nodes_per_namespace;
    uint64_t max_nesting_depth;
    uint64_t max_values_per_map;
    uint64_t max_scalar_bytes;
    uint64_t reserved[4];
} clp_s_kv_ir_serializer_options;

/**
 * Borrowed pending KV-IR output. `data` is NULL exactly when `length` is zero.
 *
 * A successful serializer append, UTC-offset change, consume, finish, or free invalidates the
 * view. Callers must neither modify nor retain the bytes beyond that point.
 */
typedef struct clp_s_kv_ir_pending_view {
    const uint8_t *data;
    size_t length;
} clp_s_kv_ir_pending_view;

/** Cumulative committed KV-IR serializer statistics. */
typedef struct clp_s_kv_ir_serializer_stats {
    uint64_t log_events;
    uint64_t schema_nodes;
    uint64_t utc_offset_changes;
    uint64_t serialized_bytes;
    uint64_t pending_bytes;
    uint32_t is_finished;
    uint32_t reserved;
} clp_s_kv_ir_serializer_stats;

/**
 * Fixed-width pull-deserializer options. `struct_size` must equal sizeof this structure. Zero
 * limits select core defaults. `max_read_chunk_bytes` caps each callback request; zero leaves the
 * core request unchanged. All reserved fields must be zero.
 */
typedef struct clp_s_kv_ir_deserializer_options {
    uint32_t struct_size;
    uint32_t reserved0;
    uint64_t max_read_chunk_bytes;
    uint64_t max_materialized_nodes;
    uint64_t max_event_arena_bytes;
    uint64_t max_reconstructed_value_bytes;
    uint64_t reserved[4];
} clp_s_kv_ir_deserializer_options;

/** Generic immutable length-delimited byte view. */
typedef struct clp_s_kv_ir_byte_view {
    const uint8_t *data;
    uint64_t length;
} clp_s_kv_ir_byte_view;

/** Compact span indexing one owned event's shared byte arena. */
typedef struct clp_s_kv_ir_event_span {
    uint32_t offset;
    uint32_t length;
} clp_s_kv_ir_event_span;

/**
 * One zero-copy DFS event node.
 *
 * `value_kind` is CLP_S_KV_IR_VALUE_*. Integer scalar_bits are the exact two's-complement bit
 * pattern, floats retain exact IEEE-754 bits, and booleans are 0/1. Only STRING and ARRAY_JSON use
 * `value_span`. Keys and byte values are length-delimited arbitrary bytes without terminators.
 */
typedef struct clp_s_kv_ir_event_node {
    uint32_t depth;
    uint32_t value_kind;
    clp_s_kv_ir_event_span key;
    clp_s_kv_ir_event_span value_span;
    uint64_t scalar_bits;
} clp_s_kv_ir_event_node;

/** Immutable borrowed view over one independent owned event. */
typedef struct clp_s_kv_ir_event_view {
    const uint8_t *arena;
    uint64_t arena_length;
    const clp_s_kv_ir_event_node *auto_nodes;
    uint64_t auto_node_count;
    const clp_s_kv_ir_event_node *user_nodes;
    uint64_t user_node_count;
    int64_t utc_offset_millis;
    uint64_t stream_index;
    uint64_t unit_index;
    uint64_t event_index;
    uint64_t input_offset;
    uint64_t reserved[4];
} clp_s_kv_ir_event_view;

typedef struct clp_s_archive clp_s_archive;
typedef struct clp_s_query clp_s_query;
typedef struct clp_s_kv_ir_serializer clp_s_kv_ir_serializer;
typedef struct clp_s_kv_ir_deserializer clp_s_kv_ir_deserializer;
typedef struct clp_s_kv_ir_event clp_s_kv_ir_event;

/**
 * Pulls bytes into Rust-provided writable storage. On READ_OK, `out_read` must be at most capacity;
 * zero means physical EOF. Any other return is a callback failure. Do not retain `dst` or unwind.
 */
typedef uint32_t (*clp_s_kv_ir_read_callback)(
        void *context,
        uint8_t *dst,
        size_t capacity,
        size_t *out_read
);

/**
 * Return CONTINUE, CANCEL, or ERROR. The record and JSON bytes must not be retained, and the
 * callback must not throw/unwind, free or reenter active handles, or race their destruction.
 */
typedef uint32_t (*clp_s_record_callback)(void *user_data, const clp_s_record *record);

uint32_t clp_s_v1_abi_version(void);

/**
 * Copies the NUL-terminated library package version. A NULL buffer is allowed only when capacity
 * is zero. `required` is mandatory. A probe or short buffer returns BUFFER_TOO_SMALL.
 */
clp_s_status clp_s_v1_library_version(
        uint8_t *buffer,
        size_t capacity,
        size_t *required
);

/** Opens a length-delimited UTF-8 local SFA or directory path into an immutable handle. */
clp_s_status clp_s_v1_archive_open(
        const uint8_t *path,
        size_t path_length,
        clp_s_archive **out_archive,
        clp_s_error_buffer *error
);

/** NULL is a no-op. A non-NULL handle must be live and unused by concurrent operations. */
void clp_s_v1_archive_free(clp_s_archive *archive);

/** Compiles length-delimited UTF-8 KQL. Flags accept only CLP_S_QUERY_IGNORE_CASE. */
clp_s_status clp_s_v1_query_compile(
        const uint8_t *query,
        size_t query_length,
        uint32_t flags,
        clp_s_query **out_query,
        clp_s_error_buffer *error
);

/** NULL is a no-op. A non-NULL handle must be live and unused by concurrent operations. */
void clp_s_v1_query_free(clp_s_query *query);

/**
 * Creates a bounded serializer and stages its preamble. NULL options select four-byte defaults.
 * NULL metadata with zero length means no user metadata; non-NULL empty metadata is invalid JSON.
 * `out_serializer` is always initialized to NULL before validation.
 */
clp_s_status clp_s_v1_kv_ir_serializer_new(
        const clp_s_kv_ir_serializer_options *options,
        const uint8_t *user_defined_metadata_json,
        size_t user_defined_metadata_json_length,
        clp_s_kv_ir_serializer **out_serializer,
        clp_s_error_buffer *error
);

/** NULL is a no-op. A non-NULL handle must have no active operation or borrowed pending view. */
void clp_s_v1_kv_ir_serializer_free(clp_s_kv_ir_serializer *serializer);

/**
 * Transactionally appends the first root MessagePack map from each length-delimited input;
 * compatibility bytes after that root map are ignored. On success `event_bytes` includes any new
 * schema packets. On failure it remains zero and no schema or output is committed.
 */
clp_s_status clp_s_v1_kv_ir_serializer_append_msgpack_maps(
        clp_s_kv_ir_serializer *serializer,
        const uint8_t *auto_generated,
        size_t auto_generated_length,
        const uint8_t *user_generated,
        size_t user_generated_length,
        uint64_t *event_bytes,
        clp_s_error_buffer *error
);

/** Appends one signed-millisecond UTC-offset packet; `packet_bytes` is nine on success. */
clp_s_status clp_s_v1_kv_ir_serializer_change_utc_offset(
        clp_s_kv_ir_serializer *serializer,
        int64_t utc_offset_millis,
        uint64_t *packet_bytes,
        clp_s_error_buffer *error
);

/**
 * Borrows all pending bytes without copying. Mutating calls and free invalidate the returned view;
 * read-only stats and total-byte calls do not. Calls on one handle must not run concurrently.
 */
clp_s_status clp_s_v1_kv_ir_serializer_pending_view(
        const clp_s_kv_ir_serializer *serializer,
        clp_s_kv_ir_pending_view *out_view,
        clp_s_error_buffer *error
);

/** Consumes one pending prefix. `bytes` must not exceed the last/current pending length. */
clp_s_status clp_s_v1_kv_ir_serializer_consume(
        clp_s_kv_ir_serializer *serializer,
        size_t bytes,
        clp_s_error_buffer *error
);

/** Appends the one-byte end marker once. A repeated call returns INVALID_STATE. */
clp_s_status clp_s_v1_kv_ir_serializer_finish(
        clp_s_kv_ir_serializer *serializer,
        uint64_t *eof_bytes,
        clp_s_error_buffer *error
);

/** Copies cumulative committed statistics; output is zeroed before validation. */
clp_s_status clp_s_v1_kv_ir_serializer_stats(
        const clp_s_kv_ir_serializer *serializer,
        clp_s_kv_ir_serializer_stats *out_stats,
        clp_s_error_buffer *error
);

/** Returns cumulative committed bytes, including preamble and bytes already consumed. */
clp_s_status clp_s_v1_kv_ir_serializer_total_bytes(
        const clp_s_kv_ir_serializer *serializer,
        uint64_t *out_total_bytes,
        clp_s_error_buffer *error
);

/**
 * Creates a pull deserializer and parses the first StreamStart immediately. Empty/truncated
 * preambles return KV_IR_INCOMPLETE. The callback/context must outlive the returned handle. Calls
 * on one deserializer and its callback context must not overlap.
 */
clp_s_status clp_s_v1_kv_ir_deserializer_new(
        const clp_s_kv_ir_deserializer_options *options,
        clp_s_kv_ir_read_callback callback,
        void *context,
        clp_s_kv_ir_deserializer **out_deserializer,
        clp_s_error_buffer *error
);

/** NULL is a no-op. Returned event handles remain valid after deserializer destruction. */
void clp_s_v1_kv_ir_deserializer_free(clp_s_kv_ir_deserializer *deserializer);

/** Borrows the exact validated full metadata JSON until the deserializer is freed. */
clp_s_status clp_s_v1_kv_ir_deserializer_metadata_view(
        const clp_s_kv_ir_deserializer *deserializer,
        clp_s_kv_ir_byte_view *out_view,
        clp_s_error_buffer *error
);

/**
 * Pulls through schema/UTC units and returns one independent event. The first explicit stream end
 * and every later call return EOF with a NULL event; concatenated streams are not decoded.
 */
clp_s_status clp_s_v1_kv_ir_deserializer_next_event(
        clp_s_kv_ir_deserializer *deserializer,
        clp_s_kv_ir_event **out_event,
        clp_s_error_buffer *error
);

/**
 * Pulls one independent event and returns its zero-copy view in the same call. This has the same
 * EOF and ownership semantics as `clp_s_v1_kv_ir_deserializer_next_event`; on every non-success
 * return the event is NULL and the view is empty.
 */
clp_s_status clp_s_v1_kv_ir_deserializer_next_event_with_view(
        clp_s_kv_ir_deserializer *deserializer,
        clp_s_kv_ir_event **out_event,
        clp_s_kv_ir_event_view *out_view,
        clp_s_error_buffer *error
);

/** Borrows zero-copy DFS arrays and their shared arena until the event is freed. */
clp_s_status clp_s_v1_kv_ir_event_view(
        const clp_s_kv_ir_event *event,
        clp_s_kv_ir_event_view *out_view,
        clp_s_error_buffer *error
);

/** NULL is a no-op. A non-NULL handle must have no active view use. */
void clp_s_v1_kv_ir_event_free(clp_s_kv_ir_event *event);

/**
 * Extracts through one synchronous callback per JSON document. Flags accept only
 * CLP_S_EXTRACT_LOG_ORDER. `records_delivered` may be NULL and includes a callback invocation
 * that returns CANCEL or ERROR.
 */
clp_s_status clp_s_v1_extract(
        const clp_s_archive *archive,
        uint32_t flags,
        clp_s_record_callback callback,
        void *user_data,
        uint64_t *records_delivered,
        clp_s_error_buffer *error
);

/**
 * Searches in physical order and projects all fields through one callback per JSON document.
 * `records_delivered` may be NULL and includes a callback invocation that returns CANCEL or ERROR.
 */
clp_s_status clp_s_v1_search(
        const clp_s_archive *archive,
        const clp_s_query *query,
        clp_s_record_callback callback,
        void *user_data,
        uint64_t *records_delivered,
        clp_s_error_buffer *error
);

/**
 * ABI v2: projected columnar scanning.
 *
 * `clp_s_v1_search` delivers one complete JSON document per match, so a caller wanting a few
 * scalar fields pays to reconstruct, serialize, and reparse every other field. These entry points
 * read only the requested paths out of the decoded columns instead.
 */

#define CLP_S_V2_VALUE_ABSENT 0u
#define CLP_S_V2_VALUE_BOOLEAN 1u
#define CLP_S_V2_VALUE_INTEGER 2u
#define CLP_S_V2_VALUE_FLOAT 3u
#define CLP_S_V2_VALUE_STRING 4u
#define CLP_S_V2_VALUE_TIMESTAMP 5u
#define CLP_S_V2_VALUE_UNSUPPORTED 6u

typedef struct clp_s_v2_scanner clp_s_v2_scanner;
typedef struct clp_s_v2_kv_ir_scanner clp_s_v2_kv_ir_scanner;

/** One requested projection path, as an escaped dot descriptor. */
typedef struct clp_s_v2_projected_field {
    const uint8_t *path;
    size_t path_length;
} clp_s_v2_projected_field;

/**
 * One projected value.
 *
 * ABSENT covers both a path this archive does not carry and an explicit null, matching how a
 * missing value reaches SQL. Booleans use 0/1 in `integer`, timestamps epoch nanoseconds. `text`
 * borrows the scanner's batch arena and is valid only until the next next-row call on, or
 * destruction of, the owning scanner.
 */
typedef struct clp_s_v2_value {
    uint32_t kind;
    uint32_t reserved;
    int64_t integer;
    double real;
    const uint8_t *text;
    size_t text_length;
} clp_s_v2_value;

/**
 * Opens a projected scan. `query` and every field descriptor are borrowed only for this call. A
 * path the archive does not carry is reported ABSENT on every row rather than refused, because a
 * dataset's schemas need not all carry every field.
 */
clp_s_status clp_s_v2_scanner_open(
        const uint8_t *archive_path,
        size_t archive_path_length,
        const clp_s_query *query,
        const clp_s_v2_projected_field *fields,
        size_t field_count,
        clp_s_v2_scanner **out_scanner,
        clp_s_error_buffer *error
);

/**
 * Delivers the next matching row. `out_values` must hold `field_count` elements. `out_has_row`
 * receives zero once the archive is exhausted, and `out_values` is then untouched.
 */
clp_s_status clp_s_v2_scanner_next_row(
        clp_s_v2_scanner *scanner,
        clp_s_v2_value *out_values,
        uint32_t *out_has_row,
        clp_s_error_buffer *error
);

/** Frees a scanner handle. A null handle is a no-op. */
void clp_s_v2_scanner_free(clp_s_v2_scanner *scanner);

/**
 * Opens a projected scan over the first stream in a local zstd-framed KV-IR file. `query` and every
 * field descriptor are borrowed only for this call. `fields` may be NULL only when `field_count`
 * is zero; `out_scanner` is initialized to NULL before validation. Missing, explicit-null, and
 * empty-object values are ABSENT; unstructured arrays and unrepresentable value kinds are
 * UNSUPPORTED. The narrow live-ingest truncation accepted by C++ retains complete preceding
 * events; every other decode, input, query, or resource failure fails this call.
 */
clp_s_status clp_s_v2_kv_ir_scanner_open(
        const uint8_t *stream_path,
        size_t stream_path_length,
        const clp_s_query *query,
        const clp_s_v2_projected_field *fields,
        size_t field_count,
        clp_s_v2_kv_ir_scanner **out_scanner,
        clp_s_error_buffer *error
);

/**
 * Delivers the next matching KV-IR event in requested-field order. `out_values` must hold the
 * scanner's `field_count` elements (and may be NULL only when that count is zero). `out_has_row`
 * receives one on success or zero at exhaustion; at exhaustion `out_values` is untouched.
 */
clp_s_status clp_s_v2_kv_ir_scanner_next_row(
        clp_s_v2_kv_ir_scanner *scanner,
        clp_s_v2_value *out_values,
        uint32_t *out_has_row,
        clp_s_error_buffer *error
);

/** Frees a KV-IR scanner handle. A null handle is a no-op. */
void clp_s_v2_kv_ir_scanner_free(clp_s_v2_kv_ir_scanner *scanner);

#if defined(__cplusplus)
#define CLP_S_ABI_STATIC_ASSERT(condition, message) static_assert((condition), message)
#elif defined(__STDC_VERSION__) && __STDC_VERSION__ >= 201112L
#define CLP_S_ABI_STATIC_ASSERT(condition, message) _Static_assert((condition), message)
#endif

#ifdef CLP_S_ABI_STATIC_ASSERT
CLP_S_ABI_STATIC_ASSERT(offsetof(clp_s_error_buffer, data) == 0, "error data offset");
CLP_S_ABI_STATIC_ASSERT(
        offsetof(clp_s_error_buffer, capacity) == sizeof(void *),
        "error capacity offset"
);
CLP_S_ABI_STATIC_ASSERT(
        offsetof(clp_s_error_buffer, required) == sizeof(void *) + sizeof(size_t),
        "error required offset"
);
CLP_S_ABI_STATIC_ASSERT(
        sizeof(clp_s_error_buffer) == sizeof(void *) + 2 * sizeof(size_t),
        "clp_s_error_buffer layout differs from ABI v1"
);

CLP_S_ABI_STATIC_ASSERT(
        offsetof(clp_s_v2_projected_field, path) == 0,
        "v2 field path offset"
);
CLP_S_ABI_STATIC_ASSERT(
        offsetof(clp_s_v2_projected_field, path_length) == sizeof(void *),
        "v2 field length offset"
);
CLP_S_ABI_STATIC_ASSERT(
        sizeof(clp_s_v2_projected_field) == sizeof(void *) + sizeof(size_t),
        "v2 field size"
);
CLP_S_ABI_STATIC_ASSERT(offsetof(clp_s_v2_value, kind) == 0, "v2 value kind offset");
CLP_S_ABI_STATIC_ASSERT(
        offsetof(clp_s_v2_value, reserved) == sizeof(uint32_t),
        "v2 value reserved offset"
);
CLP_S_ABI_STATIC_ASSERT(
        offsetof(clp_s_v2_value, integer) == 2 * sizeof(uint32_t),
        "v2 value integer offset"
);
CLP_S_ABI_STATIC_ASSERT(
        offsetof(clp_s_v2_value, real) == 2 * sizeof(uint32_t) + sizeof(int64_t),
        "v2 value real offset"
);
CLP_S_ABI_STATIC_ASSERT(
        offsetof(clp_s_v2_value, text)
                == 2 * sizeof(uint32_t) + sizeof(int64_t) + sizeof(double),
        "v2 value text offset"
);
CLP_S_ABI_STATIC_ASSERT(
        offsetof(clp_s_v2_value, text_length)
                == 2 * sizeof(uint32_t) + sizeof(int64_t) + sizeof(double) + sizeof(void *),
        "v2 value text length offset"
);
CLP_S_ABI_STATIC_ASSERT(
        sizeof(clp_s_v2_value)
                == 2 * sizeof(uint32_t) + sizeof(int64_t) + sizeof(double) + sizeof(void *)
                           + sizeof(size_t),
        "v2 value size"
);

CLP_S_ABI_STATIC_ASSERT(offsetof(clp_s_record, json) == 0, "record JSON offset");
CLP_S_ABI_STATIC_ASSERT(
        offsetof(clp_s_record, json_length) == sizeof(void *),
        "record JSON length offset"
);
CLP_S_ABI_STATIC_ASSERT(
        offsetof(clp_s_record, table_index) == sizeof(void *) + sizeof(size_t),
        "record table index offset"
);
CLP_S_ABI_STATIC_ASSERT(
        offsetof(clp_s_record, row_index)
                == offsetof(clp_s_record, table_index) + sizeof(uint64_t),
        "record row index offset"
);
CLP_S_ABI_STATIC_ASSERT(
        offsetof(clp_s_record, log_event_idx)
                == offsetof(clp_s_record, row_index) + sizeof(uint64_t),
        "record log-event index offset"
);
CLP_S_ABI_STATIC_ASSERT(
        offsetof(clp_s_record, has_log_event_idx)
                == offsetof(clp_s_record, log_event_idx) + sizeof(uint64_t),
        "record log-event presence offset"
);
CLP_S_ABI_STATIC_ASSERT(
        offsetof(clp_s_record, reserved)
                == offsetof(clp_s_record, has_log_event_idx) + sizeof(uint32_t),
        "record reserved offset"
);
CLP_S_ABI_STATIC_ASSERT(sizeof(((clp_s_record *)0)->table_index) == 8, "table_index width");
CLP_S_ABI_STATIC_ASSERT(sizeof(((clp_s_record *)0)->row_index) == 8, "row_index width");
CLP_S_ABI_STATIC_ASSERT(sizeof(((clp_s_record *)0)->log_event_idx) == 8, "log_event_idx width");
CLP_S_ABI_STATIC_ASSERT(
        sizeof(clp_s_record)
                == sizeof(void *) + sizeof(size_t) + 3 * sizeof(uint64_t) + 2 * sizeof(uint32_t),
        "clp_s_record layout differs from ABI v1"
);

CLP_S_ABI_STATIC_ASSERT(
        offsetof(clp_s_kv_ir_serializer_options, struct_size) == 0,
        "KV-IR options struct_size offset"
);
CLP_S_ABI_STATIC_ASSERT(
        offsetof(clp_s_kv_ir_serializer_options, encoding) == 4,
        "KV-IR options encoding offset"
);
CLP_S_ABI_STATIC_ASSERT(
        offsetof(clp_s_kv_ir_serializer_options, max_input_bytes_per_map) == 8,
        "KV-IR options input limit offset"
);
CLP_S_ABI_STATIC_ASSERT(
        offsetof(clp_s_kv_ir_serializer_options, max_pending_output_bytes) == 16,
        "KV-IR options pending limit offset"
);
CLP_S_ABI_STATIC_ASSERT(
        offsetof(clp_s_kv_ir_serializer_options, max_event_output_bytes) == 24,
        "KV-IR options event limit offset"
);
CLP_S_ABI_STATIC_ASSERT(
        offsetof(clp_s_kv_ir_serializer_options, max_metadata_bytes) == 32,
        "KV-IR options metadata limit offset"
);
CLP_S_ABI_STATIC_ASSERT(
        offsetof(clp_s_kv_ir_serializer_options, max_schema_nodes_per_namespace) == 40,
        "KV-IR options schema limit offset"
);
CLP_S_ABI_STATIC_ASSERT(
        offsetof(clp_s_kv_ir_serializer_options, max_nesting_depth) == 48,
        "KV-IR options nesting limit offset"
);
CLP_S_ABI_STATIC_ASSERT(
        offsetof(clp_s_kv_ir_serializer_options, max_values_per_map) == 56,
        "KV-IR options value limit offset"
);
CLP_S_ABI_STATIC_ASSERT(
        offsetof(clp_s_kv_ir_serializer_options, max_scalar_bytes) == 64,
        "KV-IR options scalar limit offset"
);
CLP_S_ABI_STATIC_ASSERT(
        offsetof(clp_s_kv_ir_serializer_options, reserved) == 72,
        "KV-IR options reserved offset"
);
CLP_S_ABI_STATIC_ASSERT(
        sizeof(clp_s_kv_ir_serializer_options) == 104,
        "KV-IR serializer options layout differs from ABI v1"
);

CLP_S_ABI_STATIC_ASSERT(offsetof(clp_s_kv_ir_pending_view, data) == 0, "pending data offset");
CLP_S_ABI_STATIC_ASSERT(
        offsetof(clp_s_kv_ir_pending_view, length) == sizeof(void *),
        "pending length offset"
);
CLP_S_ABI_STATIC_ASSERT(
        sizeof(clp_s_kv_ir_pending_view) == sizeof(void *) + sizeof(size_t),
        "pending view layout differs from ABI v1"
);

CLP_S_ABI_STATIC_ASSERT(
        offsetof(clp_s_kv_ir_serializer_stats, log_events) == 0,
        "KV-IR stats event offset"
);
CLP_S_ABI_STATIC_ASSERT(
        offsetof(clp_s_kv_ir_serializer_stats, schema_nodes) == 8,
        "KV-IR stats schema offset"
);
CLP_S_ABI_STATIC_ASSERT(
        offsetof(clp_s_kv_ir_serializer_stats, utc_offset_changes) == 16,
        "KV-IR stats UTC offset"
);
CLP_S_ABI_STATIC_ASSERT(
        offsetof(clp_s_kv_ir_serializer_stats, serialized_bytes) == 24,
        "KV-IR stats serialized-byte offset"
);
CLP_S_ABI_STATIC_ASSERT(
        offsetof(clp_s_kv_ir_serializer_stats, pending_bytes) == 32,
        "KV-IR stats pending-byte offset"
);
CLP_S_ABI_STATIC_ASSERT(
        offsetof(clp_s_kv_ir_serializer_stats, is_finished) == 40,
        "KV-IR stats finished offset"
);
CLP_S_ABI_STATIC_ASSERT(
        offsetof(clp_s_kv_ir_serializer_stats, reserved) == 44,
        "KV-IR stats reserved offset"
);
CLP_S_ABI_STATIC_ASSERT(
        sizeof(clp_s_kv_ir_serializer_stats) == 48,
        "KV-IR serializer stats layout differs from ABI v1"
);

CLP_S_ABI_STATIC_ASSERT(
        offsetof(clp_s_kv_ir_deserializer_options, struct_size) == 0,
        "KV-IR deserializer options size offset"
);
CLP_S_ABI_STATIC_ASSERT(
        offsetof(clp_s_kv_ir_deserializer_options, reserved0) == 4,
        "KV-IR deserializer options reserved0 offset"
);
CLP_S_ABI_STATIC_ASSERT(
        offsetof(clp_s_kv_ir_deserializer_options, max_read_chunk_bytes) == 8,
        "KV-IR deserializer read-chunk offset"
);
CLP_S_ABI_STATIC_ASSERT(
        offsetof(clp_s_kv_ir_deserializer_options, max_materialized_nodes) == 16,
        "KV-IR deserializer node-limit offset"
);
CLP_S_ABI_STATIC_ASSERT(
        offsetof(clp_s_kv_ir_deserializer_options, max_event_arena_bytes) == 24,
        "KV-IR deserializer arena-limit offset"
);
CLP_S_ABI_STATIC_ASSERT(
        offsetof(clp_s_kv_ir_deserializer_options, max_reconstructed_value_bytes) == 32,
        "KV-IR deserializer reconstruction-limit offset"
);
CLP_S_ABI_STATIC_ASSERT(
        offsetof(clp_s_kv_ir_deserializer_options, reserved) == 40,
        "KV-IR deserializer reserved offset"
);
CLP_S_ABI_STATIC_ASSERT(
        sizeof(clp_s_kv_ir_deserializer_options) == 72,
        "KV-IR deserializer options layout differs from ABI v1"
);

CLP_S_ABI_STATIC_ASSERT(offsetof(clp_s_kv_ir_byte_view, data) == 0, "byte-view data offset");
CLP_S_ABI_STATIC_ASSERT(
        offsetof(clp_s_kv_ir_byte_view, length) == 8,
        "byte-view length offset"
);
CLP_S_ABI_STATIC_ASSERT(sizeof(clp_s_kv_ir_byte_view) == 16, "byte-view ABI v1 size");

CLP_S_ABI_STATIC_ASSERT(offsetof(clp_s_kv_ir_event_span, offset) == 0, "span offset");
CLP_S_ABI_STATIC_ASSERT(offsetof(clp_s_kv_ir_event_span, length) == 4, "span length offset");
CLP_S_ABI_STATIC_ASSERT(sizeof(clp_s_kv_ir_event_span) == 8, "span ABI v1 size");
CLP_S_ABI_STATIC_ASSERT(offsetof(clp_s_kv_ir_event_node, depth) == 0, "node depth offset");
CLP_S_ABI_STATIC_ASSERT(
        offsetof(clp_s_kv_ir_event_node, value_kind) == 4,
        "node value-kind offset"
);
CLP_S_ABI_STATIC_ASSERT(offsetof(clp_s_kv_ir_event_node, key) == 8, "node key offset");
CLP_S_ABI_STATIC_ASSERT(
        offsetof(clp_s_kv_ir_event_node, value_span) == 16,
        "node value-span offset"
);
CLP_S_ABI_STATIC_ASSERT(
        offsetof(clp_s_kv_ir_event_node, scalar_bits) == 24,
        "node scalar-bits offset"
);
CLP_S_ABI_STATIC_ASSERT(sizeof(clp_s_kv_ir_event_node) == 32, "event-node ABI v1 size");

CLP_S_ABI_STATIC_ASSERT(offsetof(clp_s_kv_ir_event_view, arena) == 0, "event arena offset");
CLP_S_ABI_STATIC_ASSERT(
        offsetof(clp_s_kv_ir_event_view, arena_length) == 8,
        "event arena-length offset"
);
CLP_S_ABI_STATIC_ASSERT(
        offsetof(clp_s_kv_ir_event_view, auto_nodes) == 16,
        "event auto-nodes offset"
);
CLP_S_ABI_STATIC_ASSERT(
        offsetof(clp_s_kv_ir_event_view, auto_node_count) == 24,
        "event auto-count offset"
);
CLP_S_ABI_STATIC_ASSERT(
        offsetof(clp_s_kv_ir_event_view, user_nodes) == 32,
        "event user-nodes offset"
);
CLP_S_ABI_STATIC_ASSERT(
        offsetof(clp_s_kv_ir_event_view, user_node_count) == 40,
        "event user-count offset"
);
CLP_S_ABI_STATIC_ASSERT(
        offsetof(clp_s_kv_ir_event_view, utc_offset_millis) == 48,
        "event UTC-offset offset"
);
CLP_S_ABI_STATIC_ASSERT(
        offsetof(clp_s_kv_ir_event_view, stream_index) == 56,
        "event stream-index offset"
);
CLP_S_ABI_STATIC_ASSERT(
        offsetof(clp_s_kv_ir_event_view, unit_index) == 64,
        "event unit-index offset"
);
CLP_S_ABI_STATIC_ASSERT(
        offsetof(clp_s_kv_ir_event_view, event_index) == 72,
        "event event-index offset"
);
CLP_S_ABI_STATIC_ASSERT(
        offsetof(clp_s_kv_ir_event_view, input_offset) == 80,
        "event input-offset offset"
);
CLP_S_ABI_STATIC_ASSERT(
        offsetof(clp_s_kv_ir_event_view, reserved) == 88,
        "event reserved offset"
);
CLP_S_ABI_STATIC_ASSERT(sizeof(clp_s_kv_ir_event_view) == 120, "event-view ABI v1 size");
#undef CLP_S_ABI_STATIC_ASSERT
#endif

#ifdef __cplusplus
}
#endif

#endif
