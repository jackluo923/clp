#include "ColumnValueFilter.hpp"

#include <algorithm>
#include <cmath>
#include <cstddef>
#include <cstdint>
#include <numbers>
#include <span>
#include <string>
#include <string_view>
#include <utility>

#include <ystdlib/error_handling/Result.hpp>

#include <clp/BufferReader.hpp>
#include <clp/ErrorCode.hpp>
#include <clp/WriterInterface.hpp>
#include <clp_s/ErrorCode.hpp>
#include <clp_s/filter/BloomFilter.hpp>
#include <clp_s/filter/ErrorCode.hpp>
#include <clp_s/ZstdCompressor.hpp>
#include <clp_s/ZstdDecompressor.hpp>

namespace clp_s {
namespace {
/**
 * A `clp::WriterInterface` that appends to an in-memory buffer, so a Bloom filter can be serialized
 * ahead of its length prefix.
 */
class StringWriter : public clp::WriterInterface {
public:
    // Methods implementing clp::WriterInterface
    auto write(char const* data, size_t data_length) -> void override {
        m_buffer.append(data, data_length);
    }

    auto flush() -> void override {}

    auto try_seek_from_begin(size_t /*pos*/) -> clp::ErrorCode override {
        return clp::ErrorCode_Unsupported;
    }

    auto try_seek_from_current(off_t /*offset*/) -> clp::ErrorCode override {
        return clp::ErrorCode_Unsupported;
    }

    auto try_get_pos(size_t& pos) const -> clp::ErrorCode override {
        pos = m_buffer.size();
        return clp::ErrorCode_Success;
    }

    // Methods
    [[nodiscard]] auto buffer() const -> std::string const& { return m_buffer; }

private:
    std::string m_buffer;
};

[[nodiscard]] auto key_bytes(int64_t const& key) -> std::string_view {
    return {reinterpret_cast<char const*>(&key), sizeof(key)};
}

/**
 * @param num_keys
 * @return The false positive rate that keeps a Bloom filter over `num_keys` keys within
 * `ColumnValueFilter::cMaxBloomBits` bits, or the target rate if that is already within budget.
 */
[[nodiscard]] auto bounded_false_positive_rate(size_t num_keys) -> double {
    if (0 == num_keys) {
        return ColumnValueFilter::cTargetFalsePositiveRate;
    }
    // Invert the optimal bit count `-n * ln(p) / ln(2)^2` for the bit budget.
    double const ln2{std::numbers::ln2_v<double>};
    double const rate_at_budget{std::exp(
            -static_cast<double>(ColumnValueFilter::cMaxBloomBits) * ln2 * ln2
            / static_cast<double>(num_keys)
    )};
    // Stay strictly below 1 so the filter stays valid for absurdly large columns.
    constexpr double cMaxRate{0.5};
    return std::clamp(rate_at_budget, ColumnValueFilter::cTargetFalsePositiveRate, cMaxRate);
}
}  // namespace

auto ColumnValueFilter::create(std::span<int64_t const> keys)
        -> ystdlib::error_handling::Result<ColumnValueFilter> {
    ColumnValueFilter filter;
    filter.m_num_keys = keys.size();
    if (keys.empty()) {
        return filter;
    }
    auto bloom{YSTDLIB_ERROR_HANDLING_TRYX(
            filter::BloomFilter::create(keys.size(), bounded_false_positive_rate(keys.size()))
    )};
    for (auto const key : keys) {
        filter.m_min_key = std::min(filter.m_min_key, key);
        filter.m_max_key = std::max(filter.m_max_key, key);
        bloom.add(key_bytes(key));
    }
    filter.m_bloom = std::move(bloom);
    return filter;
}

auto ColumnValueFilter::may_contain(int64_t key) const -> bool {
    if (false == may_contain_in_range(key, key)) {
        return false;
    }
    return false == m_bloom.has_value() || m_bloom->possibly_contains(key_bytes(key));
}

auto ColumnValueFilter::compress(ZstdCompressor& compressor) const -> void {
    compressor.write_numeric_value(m_num_keys);
    compressor.write_numeric_value(m_min_key);
    compressor.write_numeric_value(m_max_key);
    StringWriter bloom_writer;
    if (m_bloom.has_value()) {
        m_bloom->write(bloom_writer);
    }
    auto const& bloom_bytes{bloom_writer.buffer()};
    compressor.write_numeric_value<uint64_t>(bloom_bytes.size());
    compressor.write(bloom_bytes.data(), bloom_bytes.size());
}

auto ColumnValueFilter::decompress(ZstdDecompressor& decompressor)
        -> ystdlib::error_handling::Result<ColumnValueFilter> {
    ColumnValueFilter filter;
    uint64_t bloom_size{};
    if (ErrorCodeSuccess != decompressor.try_read_numeric_value(filter.m_num_keys)
        || ErrorCodeSuccess != decompressor.try_read_numeric_value(filter.m_min_key)
        || ErrorCodeSuccess != decompressor.try_read_numeric_value(filter.m_max_key)
        || ErrorCodeSuccess != decompressor.try_read_numeric_value(bloom_size))
    {
        return filter::ErrorCode{filter::ErrorCodeEnum::ReadFailure};
    }
    if (0 == bloom_size) {
        return filter;
    }
    std::string bloom_bytes(bloom_size, '\0');
    if (ErrorCodeSuccess != decompressor.try_read_exact_length(bloom_bytes.data(), bloom_size)) {
        return filter::ErrorCode{filter::ErrorCodeEnum::ReadFailure};
    }
    clp::BufferReader bloom_reader{bloom_bytes.data(), bloom_bytes.size()};
    filter.m_bloom = YSTDLIB_ERROR_HANDLING_TRYX(filter::BloomFilter::try_read(bloom_reader));
    return filter;
}
}  // namespace clp_s
