#include "clp_s_container_shim.h"

#include <archive.h>
#include <archive_entry.h>

#include <dlfcn.h>
#include <errno.h>
#include <limits.h>
#include <stdlib.h>
#include <string.h>

#ifndef CLP_S_CONTAINER_LIBARCHIVE_PATH
#error "CLP_S_CONTAINER_LIBARCHIVE_PATH must name the pkg-config-selected shared library"
#endif

struct libarchive_api {
    void* library;
    __typeof__(&archive_read_new) archive_read_new;
    __typeof__(&archive_read_support_filter_all) archive_read_support_filter_all;
    __typeof__(&archive_read_support_format_all) archive_read_support_format_all;
    __typeof__(&archive_read_support_format_raw) archive_read_support_format_raw;
    __typeof__(&archive_set_error) archive_set_error;
    __typeof__(&archive_read_open2) archive_read_open2;
    __typeof__(&archive_read_next_header) archive_read_next_header;
    __typeof__(&archive_read_data_skip) archive_read_data_skip;
    __typeof__(&archive_read_data_block) archive_read_data_block;
    __typeof__(&archive_read_close) archive_read_close;
    __typeof__(&archive_read_free) archive_read_free;
    __typeof__(&archive_entry_filetype) archive_entry_filetype;
    __typeof__(&archive_entry_hardlink) archive_entry_hardlink;
    __typeof__(&archive_entry_size_is_set) archive_entry_size_is_set;
    __typeof__(&archive_entry_size) archive_entry_size;
    __typeof__(&archive_entry_pathname) archive_entry_pathname;
    __typeof__(&archive_format) archive_format;
    __typeof__(&archive_filter_count) archive_filter_count;
    __typeof__(&archive_errno) archive_errno;
    __typeof__(&archive_error_string) archive_error_string;
    __typeof__(&archive_version_number) archive_version_number;
};

#define LOAD_API_SYMBOL(api, symbol_name)                                      \
    do {                                                                       \
        void* const symbol_address = dlsym((api)->library, #symbol_name);       \
        if (NULL == symbol_address) {                                           \
            return 0;                                                          \
        }                                                                      \
        _Static_assert(                                                        \
                sizeof((api)->symbol_name) == sizeof(symbol_address),          \
                "POSIX function and object pointers must have equal size"     \
        );                                                                     \
        memcpy(&(api)->symbol_name, &symbol_address, sizeof(symbol_address));   \
    } while (0)

static int load_libarchive_api(struct libarchive_api* api) {
    memset(api, 0, sizeof(*api));
    api->library = dlopen(CLP_S_CONTAINER_LIBARCHIVE_PATH, RTLD_NOW | RTLD_LOCAL);
    if (NULL == api->library) {
        return 0;
    }
    LOAD_API_SYMBOL(api, archive_read_new);
    LOAD_API_SYMBOL(api, archive_read_support_filter_all);
    LOAD_API_SYMBOL(api, archive_read_support_format_all);
    LOAD_API_SYMBOL(api, archive_read_support_format_raw);
    LOAD_API_SYMBOL(api, archive_set_error);
    LOAD_API_SYMBOL(api, archive_read_open2);
    LOAD_API_SYMBOL(api, archive_read_next_header);
    LOAD_API_SYMBOL(api, archive_read_data_skip);
    LOAD_API_SYMBOL(api, archive_read_data_block);
    LOAD_API_SYMBOL(api, archive_read_close);
    LOAD_API_SYMBOL(api, archive_read_free);
    LOAD_API_SYMBOL(api, archive_entry_filetype);
    LOAD_API_SYMBOL(api, archive_entry_hardlink);
    LOAD_API_SYMBOL(api, archive_entry_size_is_set);
    LOAD_API_SYMBOL(api, archive_entry_size);
    LOAD_API_SYMBOL(api, archive_entry_pathname);
    LOAD_API_SYMBOL(api, archive_format);
    LOAD_API_SYMBOL(api, archive_filter_count);
    LOAD_API_SYMBOL(api, archive_errno);
    LOAD_API_SYMBOL(api, archive_error_string);
    LOAD_API_SYMBOL(api, archive_version_number);
    return 1;
}

static int unload_libarchive_api(struct libarchive_api* api) {
    if (NULL == api->library) {
        return 0;
    }
    int const status = dlclose(api->library);
    api->library = NULL;
    return status;
}

struct clp_s_container_archive {
    struct libarchive_api api;
    struct archive* archive;
    struct archive_entry* entry;
    void* client_data;
    clp_s_container_read_callback read_callback;
    int last_status;
};

static int normalize_status(clp_s_container_archive* wrapper, int status) {
    wrapper->last_status = status;
    if (ARCHIVE_OK == status) {
        return CLP_S_CONTAINER_RESULT_OK;
    }
    if (ARCHIVE_EOF == status) {
        return CLP_S_CONTAINER_RESULT_EOF;
    }
    return CLP_S_CONTAINER_RESULT_ERROR;
}

static clp_s_container_archive* checked_wrapper(clp_s_container_archive* wrapper) {
    if (NULL == wrapper || NULL == wrapper->archive) {
        return NULL;
    }
    return wrapper;
}

static clp_s_container_archive const* checked_const_wrapper(
        clp_s_container_archive const* wrapper
) {
    if (NULL == wrapper || NULL == wrapper->archive) {
        return NULL;
    }
    return wrapper;
}

static la_ssize_t libarchive_read_callback(
        struct archive* archive,
        void* client_data,
        void const** buffer
) {
    clp_s_container_archive* wrapper = client_data;
    uint8_t const* rust_buffer = NULL;
    int64_t const bytes_read = wrapper->read_callback(wrapper->client_data, &rust_buffer);
    if (bytes_read < 0) {
        wrapper->api.archive_set_error(archive, EIO, "%s", "Rust input callback failed");
        return (la_ssize_t)-1;
    }
    if ((uint64_t)bytes_read > (uint64_t)SSIZE_MAX) {
        wrapper->api.archive_set_error(
                archive,
                EOVERFLOW,
                "%s",
                "Rust input callback size overflow"
        );
        return (la_ssize_t)-1;
    }
    if (0 < bytes_read && NULL == rust_buffer) {
        wrapper->api.archive_set_error(
                archive,
                EFAULT,
                "%s",
                "Rust input callback returned a null buffer"
        );
        return (la_ssize_t)-1;
    }
    *buffer = rust_buffer;
    return (la_ssize_t)bytes_read;
}

clp_s_container_archive* clp_s_container_archive_new(void) {
    clp_s_container_archive* wrapper = calloc(1U, sizeof(*wrapper));
    if (NULL == wrapper) {
        return NULL;
    }
    if (0 == load_libarchive_api(&wrapper->api)) {
        unload_libarchive_api(&wrapper->api);
        free(wrapper);
        return NULL;
    }
    wrapper->archive = wrapper->api.archive_read_new();
    if (NULL == wrapper->archive) {
        unload_libarchive_api(&wrapper->api);
        free(wrapper);
        return NULL;
    }
    wrapper->last_status = ARCHIVE_OK;
    return wrapper;
}

int clp_s_container_archive_configure(clp_s_container_archive* wrapper, int policy) {
    if (NULL == checked_wrapper(wrapper)) {
        return CLP_S_CONTAINER_RESULT_ERROR;
    }
    int status = wrapper->api.archive_read_support_filter_all(wrapper->archive);
    if (ARCHIVE_OK != status) {
        return normalize_status(wrapper, status);
    }
    status = wrapper->api.archive_read_support_format_all(wrapper->archive);
    if (ARCHIVE_OK != status) {
        return normalize_status(wrapper, status);
    }
    if (CLP_S_CONTAINER_POLICY_CPP_COMPATIBLE == policy) {
        status = wrapper->api.archive_read_support_format_raw(wrapper->archive);
        return normalize_status(wrapper, status);
    }
    if (CLP_S_CONTAINER_POLICY_STRICT != policy) {
        wrapper->api.archive_set_error(
                wrapper->archive,
                EINVAL,
                "%s",
                "Unknown container format policy"
        );
        return normalize_status(wrapper, ARCHIVE_FATAL);
    }
    return normalize_status(wrapper, ARCHIVE_OK);
}

int clp_s_container_archive_open(
        clp_s_container_archive* wrapper,
        void* client_data,
        clp_s_container_read_callback read_callback
) {
    if (NULL == checked_wrapper(wrapper) || NULL == read_callback) {
        return CLP_S_CONTAINER_RESULT_ERROR;
    }
    wrapper->client_data = client_data;
    wrapper->read_callback = read_callback;
    return normalize_status(
            wrapper,
            wrapper->api.archive_read_open2(
                    wrapper->archive,
                    wrapper,
                    NULL,
                    libarchive_read_callback,
                    NULL,
                    NULL
            )
    );
}

int clp_s_container_archive_next_header(clp_s_container_archive* wrapper) {
    if (NULL == checked_wrapper(wrapper)) {
        return CLP_S_CONTAINER_RESULT_ERROR;
    }
    return normalize_status(
            wrapper,
            wrapper->api.archive_read_next_header(wrapper->archive, &wrapper->entry)
    );
}

int clp_s_container_archive_data_skip(clp_s_container_archive* wrapper) {
    if (NULL == checked_wrapper(wrapper)) {
        return CLP_S_CONTAINER_RESULT_ERROR;
    }
    return normalize_status(wrapper, wrapper->api.archive_read_data_skip(wrapper->archive));
}

int clp_s_container_archive_data_block(
        clp_s_container_archive* wrapper,
        uint8_t const** buffer,
        size_t* length,
        int64_t* offset
) {
    if (NULL == checked_wrapper(wrapper) || NULL == buffer || NULL == length || NULL == offset) {
        return CLP_S_CONTAINER_RESULT_ERROR;
    }
    void const* native_buffer = NULL;
    la_int64_t native_offset = 0;
    int const status = wrapper->api.archive_read_data_block(
            wrapper->archive,
            &native_buffer,
            length,
            &native_offset
    );
    *buffer = native_buffer;
    *offset = (int64_t)native_offset;
    return normalize_status(wrapper, status);
}

int clp_s_container_archive_close(clp_s_container_archive* wrapper) {
    if (NULL == checked_wrapper(wrapper)) {
        return CLP_S_CONTAINER_RESULT_ERROR;
    }
    return normalize_status(wrapper, wrapper->api.archive_read_close(wrapper->archive));
}

int clp_s_container_archive_free(clp_s_container_archive* wrapper) {
    if (NULL == wrapper) {
        return CLP_S_CONTAINER_RESULT_OK;
    }
    int status = ARCHIVE_OK;
    if (NULL != wrapper->archive) {
        status = wrapper->api.archive_read_free(wrapper->archive);
        wrapper->archive = NULL;
    }
    int const unload_status = unload_libarchive_api(&wrapper->api);
    free(wrapper);
    return ARCHIVE_OK == status && 0 == unload_status ? CLP_S_CONTAINER_RESULT_OK
                                                      : CLP_S_CONTAINER_RESULT_ERROR;
}

int clp_s_container_archive_current_is_regular(clp_s_container_archive const* wrapper) {
    if (NULL == checked_const_wrapper(wrapper) || NULL == wrapper->entry) {
        return 0;
    }
    return AE_IFREG == wrapper->api.archive_entry_filetype(wrapper->entry) ? 1 : 0;
}

int clp_s_container_archive_current_is_hardlink(clp_s_container_archive const* wrapper) {
    if (NULL == checked_const_wrapper(wrapper) || NULL == wrapper->entry) {
        return 0;
    }
    return NULL != wrapper->api.archive_entry_hardlink(wrapper->entry) ? 1 : 0;
}

int clp_s_container_archive_current_size(
        clp_s_container_archive const* wrapper,
        int* is_set,
        int64_t* size
) {
    if (NULL == checked_const_wrapper(wrapper) || NULL == wrapper->entry || NULL == is_set
        || NULL == size)
    {
        return CLP_S_CONTAINER_RESULT_ERROR;
    }
    *is_set = wrapper->api.archive_entry_size_is_set(wrapper->entry);
    *size = (int64_t)wrapper->api.archive_entry_size(wrapper->entry);
    return CLP_S_CONTAINER_RESULT_OK;
}

int clp_s_container_archive_current_path_length(
        clp_s_container_archive const* wrapper,
        size_t* length
) {
    if (NULL == checked_const_wrapper(wrapper) || NULL == wrapper->entry || NULL == length) {
        return CLP_S_CONTAINER_RESULT_ERROR;
    }
    char const* path = wrapper->api.archive_entry_pathname(wrapper->entry);
    if (NULL == path) {
        return CLP_S_CONTAINER_RESULT_ERROR;
    }
    *length = strlen(path);
    return CLP_S_CONTAINER_RESULT_OK;
}

int clp_s_container_archive_current_path_copy(
        clp_s_container_archive const* wrapper,
        uint8_t* output,
        size_t length
) {
    if (NULL == checked_const_wrapper(wrapper) || NULL == wrapper->entry
        || (0U < length && NULL == output))
    {
        return CLP_S_CONTAINER_RESULT_ERROR;
    }
    char const* path = wrapper->api.archive_entry_pathname(wrapper->entry);
    if (NULL == path || strlen(path) != length) {
        return CLP_S_CONTAINER_RESULT_ERROR;
    }
    if (0U < length) {
        memcpy(output, path, length);
    }
    return CLP_S_CONTAINER_RESULT_OK;
}

int clp_s_container_archive_is_raw(clp_s_container_archive const* wrapper) {
    if (NULL == checked_const_wrapper(wrapper)) {
        return 0;
    }
    return ARCHIVE_FORMAT_RAW == wrapper->api.archive_format(wrapper->archive) ? 1 : 0;
}

int clp_s_container_archive_is_mtree(clp_s_container_archive const* wrapper) {
    if (NULL == checked_const_wrapper(wrapper)) {
        return 0;
    }
    return ARCHIVE_FORMAT_MTREE
                           == (wrapper->api.archive_format(wrapper->archive)
                               & ARCHIVE_FORMAT_BASE_MASK)
                   ? 1
                   : 0;
}

int clp_s_container_archive_has_format(clp_s_container_archive const* wrapper) {
    if (NULL == checked_const_wrapper(wrapper)) {
        return 0;
    }
    return 0 != wrapper->api.archive_format(wrapper->archive) ? 1 : 0;
}

int clp_s_container_archive_filter_count(
        clp_s_container_archive const* wrapper,
        int* count
) {
    if (NULL == checked_const_wrapper(wrapper) || NULL == count) {
        return CLP_S_CONTAINER_RESULT_ERROR;
    }
    *count = wrapper->api.archive_filter_count(wrapper->archive);
    return CLP_S_CONTAINER_RESULT_OK;
}

int clp_s_container_archive_last_status(clp_s_container_archive const* wrapper) {
    return NULL == wrapper ? ARCHIVE_FATAL : wrapper->last_status;
}

int clp_s_container_archive_errno(clp_s_container_archive const* wrapper) {
    if (NULL == checked_const_wrapper(wrapper)) {
        return EINVAL;
    }
    return wrapper->api.archive_errno(wrapper->archive);
}

int clp_s_container_archive_error_length(
        clp_s_container_archive const* wrapper,
        size_t* length
) {
    if (NULL == checked_const_wrapper(wrapper) || NULL == length) {
        return CLP_S_CONTAINER_RESULT_ERROR;
    }
    char const* message = wrapper->api.archive_error_string(wrapper->archive);
    *length = NULL == message ? 0U : strlen(message);
    return CLP_S_CONTAINER_RESULT_OK;
}

int clp_s_container_archive_error_copy(
        clp_s_container_archive const* wrapper,
        uint8_t* output,
        size_t length
) {
    if (NULL == checked_const_wrapper(wrapper) || (0U < length && NULL == output)) {
        return CLP_S_CONTAINER_RESULT_ERROR;
    }
    char const* message = wrapper->api.archive_error_string(wrapper->archive);
    if (NULL == message) {
        return 0U == length ? CLP_S_CONTAINER_RESULT_OK : CLP_S_CONTAINER_RESULT_ERROR;
    }
    if (strlen(message) != length) {
        return CLP_S_CONTAINER_RESULT_ERROR;
    }
    if (0U < length) {
        memcpy(output, message, length);
    }
    return CLP_S_CONTAINER_RESULT_OK;
}

int clp_s_container_runtime_version(void) {
    struct libarchive_api api;
    if (0 == load_libarchive_api(&api)) {
        unload_libarchive_api(&api);
        return 0;
    }
    int const version = api.archive_version_number();
    if (0 != unload_libarchive_api(&api)) {
        return 0;
    }
    return version;
}
