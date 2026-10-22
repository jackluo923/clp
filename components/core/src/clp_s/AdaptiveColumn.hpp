#ifndef CLP_S_ADAPTIVECOLUMN_HPP
#define CLP_S_ADAPTIVECOLUMN_HPP

#include <cstddef>
#include <cstdint>
#include <span>
#include <vector>

#include <zstd.h>

namespace clp_s::adaptive_column {
/** Lossless numeric codecs matching the Rust archive format 0.6.0. */
enum class Type {
    Integer,
    Float
};

struct Frame {
    std::vector<char> compressed;
    size_t uncompressed_size{};
};

/** Measures candidates one at a time after Zstd compression; ties retain the first candidate. */
auto encode(std::span<char const> values, Type type, ZSTD_CCtx& context, int level) -> Frame;

/** Decodes exactly row_count values, rejecting truncated, overflowing and non-canonical streams. */
auto decode(std::span<char const> encoded, Type type, size_t row_count) -> std::vector<char>;
}  // namespace clp_s::adaptive_column
#endif  // CLP_S_ADAPTIVECOLUMN_HPP
