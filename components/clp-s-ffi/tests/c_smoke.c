#include "clp_s_ffi.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define CHECK(condition, message) \
    do { \
        if (!(condition)) { \
            fprintf(stderr, "C smoke failure: %s\n", (message)); \
            return EXIT_FAILURE; \
        } \
    } while (0)

struct callback_state {
    uint64_t records;
    uint64_t cancel_after;
};

struct kv_ir_input_state {
    const uint8_t *data;
    size_t length;
    size_t offset;
    uint64_t calls;
};

static uint32_t read_kv_ir(void *context, uint8_t *dst, size_t capacity, size_t *out_read) {
    struct kv_ir_input_state *state = (struct kv_ir_input_state *)context;
    size_t length;
    if (NULL == state || NULL == dst || NULL == out_read || 0 == capacity) {
        return CLP_S_KV_IR_READ_ERROR;
    }
    ++state->calls;
    length = state->length - state->offset;
    if (capacity < length) {
        length = capacity;
    }
    if (0 != length) {
        memcpy(dst, state->data + state->offset, length);
        state->offset += length;
    }
    *out_read = length;
    return CLP_S_KV_IR_READ_OK;
}

static int event_span_equals(
        const clp_s_kv_ir_event_view *view,
        clp_s_kv_ir_event_span span,
        const char *expected,
        size_t expected_length
) {
    uint64_t end = (uint64_t)span.offset + (uint64_t)span.length;
    return NULL != view && expected_length == span.length && end <= view->arena_length
            && (0 == expected_length
                || (NULL != view->arena
                    && 0 == memcmp(view->arena + span.offset, expected, expected_length)));
}

static uint32_t count_record(void *user_data, const clp_s_record *record) {
    struct callback_state *state = (struct callback_state *)user_data;
    if (NULL == state || NULL == record || NULL == record->json || 0 == record->json_length
        || '\n' == record->json[record->json_length - 1] || 0 != record->reserved
        || 1 < record->has_log_event_idx) {
        return CLP_S_CALLBACK_ERROR;
    }
    ++state->records;
    if (0 != state->cancel_after && state->records == state->cancel_after) {
        return CLP_S_CALLBACK_CANCEL;
    }
    return CLP_S_CALLBACK_CONTINUE;
}

static int hex_nibble(int byte) {
    if ('0' <= byte && byte <= '9') {
        return byte - '0';
    }
    if ('a' <= byte && byte <= 'f') {
        return byte - 'a' + 10;
    }
    if ('A' <= byte && byte <= 'F') {
        return byte - 'A' + 10;
    }
    return -1;
}

static int load_kv_ir_oracle(
        const char *archive_path,
        uint8_t *output,
        size_t capacity,
        size_t *output_length
) {
    static const char oracle_name[] = "kv-ir-v0.1.0-four-byte-cpp.hex";
    char path[4096];
    const char *slash = strrchr(archive_path, '/');
    size_t directory_length = NULL == slash ? 0 : (size_t)(slash - archive_path);
    FILE *input;
    size_t length = 0;
    int high_nibble = -1;
    int byte;

    if (NULL == output || NULL == output_length) {
        return 0;
    }
    if (NULL == slash) {
        if (sizeof(oracle_name) > sizeof(path)) {
            return 0;
        }
        memcpy(path, oracle_name, sizeof(oracle_name));
    } else {
        if (directory_length + 1 + sizeof(oracle_name) > sizeof(path)) {
            return 0;
        }
        memcpy(path, archive_path, directory_length);
        path[directory_length] = '/';
        memcpy(path + directory_length + 1, oracle_name, sizeof(oracle_name));
    }
    input = fopen(path, "rb");
    if (NULL == input) {
        return 0;
    }
    while (EOF != (byte = fgetc(input))) {
        int nibble = hex_nibble(byte);
        if (0 > nibble) {
            if (' ' == byte || '\t' == byte || '\r' == byte || '\n' == byte) {
                continue;
            }
            fclose(input);
            return 0;
        }
        if (0 > high_nibble) {
            high_nibble = nibble;
            continue;
        }
        if (length == capacity) {
            fclose(input);
            return 0;
        }
        output[length++] = (uint8_t)((high_nibble << 4) | nibble);
        high_nibble = -1;
    }
    if (0 != fclose(input) || 0 <= high_nibble) {
        return 0;
    }
    *output_length = length;
    return 1;
}

int main(int argc, char **argv) {
    uint8_t error_bytes[512] = {0};
    clp_s_error_buffer error = {error_bytes, sizeof(error_bytes), 0};
    size_t version_required = 0;
    uint8_t version[64] = {0};
    clp_s_archive *archive = NULL;
    clp_s_query *query = NULL;
    clp_s_kv_ir_serializer *serializer = NULL;
    clp_s_v2_kv_ir_scanner *kv_scanner = NULL;
    struct callback_state state = {0, 0};
    uint64_t delivered = 0;
    clp_s_status status;

    CHECK(2 == argc, "usage: c_smoke <minimal.sfa>");
    CHECK(CLP_S_ABI_VERSION == clp_s_v1_abi_version(), "ABI version mismatch");
    status = clp_s_v1_library_version(NULL, 0, &version_required);
    CHECK(CLP_S_STATUS_BUFFER_TOO_SMALL == status, "version probe status");
    CHECK(1 < version_required && version_required <= sizeof(version), "version buffer size");
    status = clp_s_v1_library_version(version, sizeof(version), &version_required);
    CHECK(CLP_S_STATUS_OK == status && '\0' != version[0], "library version copy");

    status = clp_s_v1_archive_open(
            (const uint8_t *)argv[1],
            strlen(argv[1]),
            &archive,
            &error
    );
    if (CLP_S_STATUS_OK != status) {
        fprintf(stderr, "archive open failed (%u): %s\n", status, (const char *)error_bytes);
        return EXIT_FAILURE;
    }

    state.cancel_after = 1;
    status = clp_s_v1_extract(archive, 0, count_record, &state, &delivered, &error);
    CHECK(CLP_S_STATUS_CANCELLED == status, "callback cancellation status");
    CHECK(1 == delivered && 1 == state.records, "callback cancellation count");

    state.records = 0;
    state.cancel_after = 0;
    status = clp_s_v1_extract(archive, 0, count_record, &state, &delivered, &error);
    if (CLP_S_STATUS_OK != status) {
        fprintf(stderr, "extract failed (%u): %s\n", status, (const char *)error_bytes);
        clp_s_v1_archive_free(archive);
        return EXIT_FAILURE;
    }
    CHECK(1 == delivered && 1 == state.records, "repeatable extraction count");

    status = clp_s_v1_query_compile((const uint8_t *)"*", 1, 0, &query, &error);
    if (CLP_S_STATUS_OK != status) {
        fprintf(stderr, "query compile failed (%u): %s\n", status, (const char *)error_bytes);
        clp_s_v1_archive_free(archive);
        return EXIT_FAILURE;
    }

    state.records = 0;
    status = clp_s_v1_search(archive, query, count_record, &state, &delivered, &error);
    if (CLP_S_STATUS_OK != status) {
        fprintf(stderr, "search failed (%u): %s\n", status, (const char *)error_bytes);
        clp_s_v1_query_free(query);
        clp_s_v1_archive_free(archive);
        return EXIT_FAILURE;
    }
    CHECK(1 == delivered && 1 == state.records, "search count");

    {
        static const uint8_t metadata[] = "{\"fixture\":\"rust-kv-ir-reader-v1\"}";
        static const uint8_t auto_generated[] = {
                0x82, 0xa5, 'l', 'e', 'v', 'e', 'l', 0xa4, 'i', 'n', 'f', 'o',
                0xa3, 's', 'e', 'q', 0x07,
        };
        static const uint8_t user_generated[] = {
                0x85,
                0xa5, 'e', 'm', 'p', 't', 'y', 0x80,
                0xa7, 'm', 'e', 's', 's', 'a', 'g', 'e',
                0xac, 't', 'a', 's', 'k', ' ', '4', '2', ' ', 'd', 'o', 'n', 'e',
                0xa4, 'n', 'o', 'n', 'e', 0xc0,
                0xa2, 'o', 'k', 0xc3,
                0xa5, 'r', 'a', 't', 'i', 'o', 0xcb, 0x3f, 0xf4, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00,
        };
        static const uint8_t invalid_user[] = {0x81, 0xa1, 'x', 0x81, 0x01, 0x02};
        static const uint8_t truncated_map[] = {0x81};
        static const uint8_t root_not_map[] = {0xc0};
        static const uint8_t full_metadata[] =
                "{\"USER_DEFINED_METADATA\":{\"fixture\":\"rust-kv-ir-reader-v1\"}"
                ",\"VARIABLES_SCHEMA_ID\":\"com.yscope.clp.VariablesSchemaV2\""
                ",\"VARIABLE_ENCODING_METHODS_ID\":"
                "\"com.yscope.clp.VariableEncodingMethodsV1\""
                ",\"VERSION\":\"0.1.0\"}";
        uint8_t oracle[1024];
        size_t oracle_length = 0;
        clp_s_kv_ir_pending_view before_failure = {NULL, 0};
        clp_s_kv_ir_pending_view pending = {NULL, 0};
        clp_s_kv_ir_serializer_stats serializer_stats = {0, 0, 0, 0, 0, 0, 0};
        uint64_t event_bytes = UINT64_MAX;
        uint64_t packet_bytes = UINT64_MAX;
        uint64_t eof_bytes = UINT64_MAX;
        uint64_t total_bytes = UINT64_MAX;

        CHECK(
                1 == load_kv_ir_oracle(argv[1], oracle, sizeof(oracle), &oracle_length),
                "load KV-IR C++ oracle fixture"
        );
        status = clp_s_v1_kv_ir_serializer_new(
                NULL,
                metadata,
                sizeof(metadata) - 1,
                &serializer,
                &error
        );
        if (CLP_S_STATUS_OK != status) {
            fprintf(
                    stderr,
                    "KV-IR serializer create failed (%u): %s\n",
                    status,
                    (const char *)error_bytes
            );
            clp_s_v1_query_free(query);
            clp_s_v1_archive_free(archive);
            return EXIT_FAILURE;
        }

        status = clp_s_v1_kv_ir_serializer_change_utc_offset(
                serializer,
                INT64_C(3600000),
                &packet_bytes,
                &error
        );
        CHECK(CLP_S_STATUS_OK == status && 9 == packet_bytes, "KV-IR UTC-offset packet");
        status = clp_s_v1_kv_ir_serializer_pending_view(
                serializer,
                &before_failure,
                &error
        );
        CHECK(CLP_S_STATUS_OK == status, "KV-IR pending view before rollback");

        status = clp_s_v1_kv_ir_serializer_append_msgpack_maps(
                serializer,
                auto_generated,
                sizeof(auto_generated),
                truncated_map,
                sizeof(truncated_map),
                &event_bytes,
                &error
        );
        CHECK(
                CLP_S_STATUS_KV_IR_INCOMPLETE == status && 0 == event_bytes,
                "KV-IR truncated MessagePack structured status"
        );
        status = clp_s_v1_kv_ir_serializer_append_msgpack_maps(
                serializer,
                auto_generated,
                sizeof(auto_generated),
                root_not_map,
                sizeof(root_not_map),
                &event_bytes,
                &error
        );
        CHECK(
                CLP_S_STATUS_KV_IR_ROOT_NOT_MAP == status && 0 == event_bytes,
                "KV-IR root-not-map structured status"
        );

        status = clp_s_v1_kv_ir_serializer_append_msgpack_maps(
                serializer,
                auto_generated,
                sizeof(auto_generated),
                invalid_user,
                sizeof(invalid_user),
                &event_bytes,
                &error
        );
        CHECK(
                CLP_S_STATUS_KV_IR_INVALID_DATA == status && 0 == event_bytes,
                "KV-IR failed-event status and zero byte count"
        );
        status = clp_s_v1_kv_ir_serializer_pending_view(serializer, &pending, &error);
        CHECK(CLP_S_STATUS_OK == status, "KV-IR pending view after rollback");
        CHECK(
                before_failure.length == pending.length
                        && 0 == memcmp(before_failure.data, pending.data, pending.length),
                "KV-IR failed event rolls back pending output"
        );

        status = clp_s_v1_kv_ir_serializer_append_msgpack_maps(
                serializer,
                auto_generated,
                sizeof(auto_generated),
                user_generated,
                sizeof(user_generated),
                &event_bytes,
                &error
        );
        CHECK(CLP_S_STATUS_OK == status && 0 < event_bytes, "KV-IR append valid event");
        status = clp_s_v1_kv_ir_serializer_finish(serializer, &eof_bytes, &error);
        CHECK(CLP_S_STATUS_OK == status && 1 == eof_bytes, "KV-IR finish");
        status = clp_s_v1_kv_ir_serializer_pending_view(serializer, &pending, &error);
        CHECK(CLP_S_STATUS_OK == status, "KV-IR complete pending view");
        CHECK(
                oracle_length == pending.length
                        && 0 == memcmp(oracle, pending.data, oracle_length),
                "KV-IR output exactly matches C++ oracle"
        );

        status = clp_s_v1_kv_ir_serializer_stats(serializer, &serializer_stats, &error);
        CHECK(CLP_S_STATUS_OK == status, "KV-IR serializer stats");
        CHECK(
                1 == serializer_stats.log_events && 7 == serializer_stats.schema_nodes
                        && 1 == serializer_stats.utc_offset_changes
                        && oracle_length == serializer_stats.serialized_bytes
                        && oracle_length == serializer_stats.pending_bytes
                        && 1 == serializer_stats.is_finished && 0 == serializer_stats.reserved,
                "KV-IR exact serializer stats"
        );
        status = clp_s_v1_kv_ir_serializer_total_bytes(serializer, &total_bytes, &error);
        CHECK(
                CLP_S_STATUS_OK == status && total_bytes == serializer_stats.serialized_bytes,
                "KV-IR total serialized bytes"
        );

        eof_bytes = UINT64_MAX;
        status = clp_s_v1_kv_ir_serializer_finish(serializer, &eof_bytes, &error);
        CHECK(
                CLP_S_STATUS_INVALID_STATE == status && 0 == eof_bytes,
                "KV-IR double finish is a stable state error"
        );
        status = clp_s_v1_kv_ir_serializer_consume(serializer, 13, &error);
        CHECK(CLP_S_STATUS_OK == status, "KV-IR consume pending prefix");
        status = clp_s_v1_kv_ir_serializer_pending_view(serializer, &pending, &error);
        CHECK(
                CLP_S_STATUS_OK == status && oracle_length - 13 == pending.length
                        && 0 == memcmp(oracle + 13, pending.data, pending.length),
                "KV-IR pending suffix after prefix consumption"
        );
        status = clp_s_v1_kv_ir_serializer_consume(serializer, pending.length, &error);
        CHECK(CLP_S_STATUS_OK == status, "KV-IR consume pending remainder");
        status = clp_s_v1_kv_ir_serializer_pending_view(serializer, &pending, &error);
        CHECK(
                CLP_S_STATUS_OK == status && NULL == pending.data && 0 == pending.length,
                "KV-IR empty pending view is null"
        );

        {
            clp_s_kv_ir_deserializer *deserializer = NULL;
            clp_s_kv_ir_event *event = NULL;
            clp_s_kv_ir_event *owned_event = NULL;
            clp_s_kv_ir_deserializer_options options = {
                    sizeof(clp_s_kv_ir_deserializer_options), 0, 1, 0, 0, 0, {0, 0, 0, 0},
            };
            clp_s_kv_ir_byte_view metadata_view = {NULL, 0};
            clp_s_kv_ir_event_view event_view = {
                    NULL, 0, NULL, 0, NULL, 0, 0, 0, 0, 0, 0, {0, 0, 0, 0},
            };
            struct kv_ir_input_state input = {oracle, oracle_length, 0, 0};
            uint64_t calls_at_eof;

            status = clp_s_v1_kv_ir_deserializer_new(
                    &options,
                    read_kv_ir,
                    &input,
                    &deserializer,
                    &error
            );
            CHECK(CLP_S_STATUS_OK == status && NULL != deserializer, "KV-IR deserializer create");
            status = clp_s_v1_kv_ir_deserializer_metadata_view(
                    deserializer,
                    &metadata_view,
                    &error
            );
            CHECK(
                    CLP_S_STATUS_OK == status
                            && sizeof(full_metadata) - 1 == metadata_view.length
                            && NULL != metadata_view.data
                            && 0 == memcmp(
                                full_metadata,
                                metadata_view.data,
                                sizeof(full_metadata) - 1
                            ),
                    "KV-IR exact owned metadata"
            );
            status = clp_s_v1_kv_ir_deserializer_next_event_with_view(
                    deserializer,
                    &event,
                    &event_view,
                    &error
            );
            CHECK(
                    CLP_S_STATUS_OK == status && NULL != event,
                    "KV-IR deserialize one event with view"
            );
            owned_event = event;
            event = NULL;
            CHECK(
                    2 == event_view.auto_node_count && 5 == event_view.user_node_count
                            && INT64_C(3600000) == event_view.utc_offset_millis
                            && 0 == event_view.stream_index && 0 == event_view.event_index,
                    "KV-IR event counts and metadata"
            );
            CHECK(
                    CLP_S_KV_IR_VALUE_STRING == event_view.auto_nodes[0].value_kind
                            && event_span_equals(
                                &event_view,
                                event_view.auto_nodes[0].key,
                                "level",
                                5
                            )
                            && event_span_equals(
                                &event_view,
                                event_view.auto_nodes[0].value_span,
                                "info",
                                4
                            )
                            && CLP_S_KV_IR_VALUE_INTEGER == event_view.auto_nodes[1].value_kind
                            && UINT64_C(7) == event_view.auto_nodes[1].scalar_bits,
                    "KV-IR auto nodes"
            );
            CHECK(
                    CLP_S_KV_IR_VALUE_EMPTY_OBJECT == event_view.user_nodes[0].value_kind
                            && CLP_S_KV_IR_VALUE_STRING == event_view.user_nodes[1].value_kind
                            && event_span_equals(
                                &event_view,
                                event_view.user_nodes[1].value_span,
                                "task 42 done",
                                12
                            )
                            && CLP_S_KV_IR_VALUE_NULL == event_view.user_nodes[2].value_kind
                            && CLP_S_KV_IR_VALUE_BOOLEAN == event_view.user_nodes[3].value_kind
                            && UINT64_C(1) == event_view.user_nodes[3].scalar_bits
                            && CLP_S_KV_IR_VALUE_FLOAT == event_view.user_nodes[4].value_kind
                            && UINT64_C(0x3ff4000000000000)
                                    == event_view.user_nodes[4].scalar_bits,
                    "KV-IR user node kinds, text, and scalar bits"
            );

            status = clp_s_v1_kv_ir_deserializer_next_event(deserializer, &event, &error);
            CHECK(CLP_S_STATUS_EOF == status && NULL == event, "KV-IR first EOF");
            calls_at_eof = input.calls;
            status = clp_s_v1_kv_ir_deserializer_next_event(deserializer, &event, &error);
            CHECK(
                    CLP_S_STATUS_EOF == status && NULL == event && calls_at_eof == input.calls,
                    "KV-IR stable repeated EOF"
            );
            CHECK(oracle_length == input.offset, "KV-IR exact tiny-chunk consumption");

            clp_s_v1_kv_ir_deserializer_free(deserializer);
            deserializer = NULL;
            status = clp_s_v1_kv_ir_event_view(owned_event, &event_view, &error);
            CHECK(
                    CLP_S_STATUS_OK == status
                            && event_span_equals(
                                &event_view,
                                event_view.user_nodes[1].value_span,
                                "task 42 done",
                                12
                            ),
                    "KV-IR event survives deserializer destruction"
            );
            clp_s_v1_kv_ir_event_free(owned_event);
            clp_s_v1_kv_ir_deserializer_free(deserializer);
            clp_s_v1_kv_ir_event_free(NULL);
        }
    }

    clp_s_v1_kv_ir_serializer_free(serializer);
    clp_s_v1_query_free(query);
    clp_s_v1_archive_free(archive);
    clp_s_v1_kv_ir_serializer_free(NULL);
    clp_s_v1_query_free(NULL);
    clp_s_v1_archive_free(NULL);
    clp_s_v2_kv_ir_scanner_free(kv_scanner);
    return EXIT_SUCCESS;
}
