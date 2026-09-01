#ifndef CLP_S_CONTAINER_SHIM_H
#define CLP_S_CONTAINER_SHIM_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct clp_s_container_archive clp_s_container_archive;

typedef int64_t (*clp_s_container_read_callback)(
        void* client_data,
        uint8_t const** buffer
);

enum clp_s_container_policy {
    CLP_S_CONTAINER_POLICY_CPP_COMPATIBLE = 0,
    CLP_S_CONTAINER_POLICY_STRICT = 1,
};

enum clp_s_container_result {
    CLP_S_CONTAINER_RESULT_ERROR = -1,
    CLP_S_CONTAINER_RESULT_OK = 0,
    CLP_S_CONTAINER_RESULT_EOF = 1,
};

clp_s_container_archive* clp_s_container_archive_new(void);
int clp_s_container_archive_configure(clp_s_container_archive* wrapper, int policy);
int clp_s_container_archive_open(
        clp_s_container_archive* wrapper,
        void* client_data,
        clp_s_container_read_callback read_callback
);
int clp_s_container_archive_next_header(clp_s_container_archive* wrapper);
int clp_s_container_archive_data_skip(clp_s_container_archive* wrapper);
int clp_s_container_archive_data_block(
        clp_s_container_archive* wrapper,
        uint8_t const** buffer,
        size_t* length,
        int64_t* offset
);
int clp_s_container_archive_close(clp_s_container_archive* wrapper);
int clp_s_container_archive_free(clp_s_container_archive* wrapper);

int clp_s_container_archive_current_is_regular(clp_s_container_archive const* wrapper);
int clp_s_container_archive_current_is_hardlink(clp_s_container_archive const* wrapper);
int clp_s_container_archive_current_size(
        clp_s_container_archive const* wrapper,
        int* is_set,
        int64_t* size
);
int clp_s_container_archive_current_path_length(
        clp_s_container_archive const* wrapper,
        size_t* length
);
int clp_s_container_archive_current_path_copy(
        clp_s_container_archive const* wrapper,
        uint8_t* output,
        size_t length
);
int clp_s_container_archive_is_raw(clp_s_container_archive const* wrapper);
int clp_s_container_archive_is_mtree(clp_s_container_archive const* wrapper);
int clp_s_container_archive_has_format(clp_s_container_archive const* wrapper);
int clp_s_container_archive_filter_count(
        clp_s_container_archive const* wrapper,
        int* count
);

int clp_s_container_archive_last_status(clp_s_container_archive const* wrapper);
int clp_s_container_archive_errno(clp_s_container_archive const* wrapper);
int clp_s_container_archive_error_length(
        clp_s_container_archive const* wrapper,
        size_t* length
);
int clp_s_container_archive_error_copy(
        clp_s_container_archive const* wrapper,
        uint8_t* output,
        size_t length
);
int clp_s_container_runtime_version(void);

#ifdef __cplusplus
}
#endif

#endif
