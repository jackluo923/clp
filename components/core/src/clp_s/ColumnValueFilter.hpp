#ifndef CLP_S_COLUMNVALUEFILTER_HPP
#define CLP_S_COLUMNVALUEFILTER_HPP

#include <cstddef>
#include <cstdint>
#include <limits>
#include <optional>
#include <span>

#include <ystdlib/error_handling/Result.hpp>

#include <clp_s/filter/BloomFilter.hpp>
#include <clp_s/ZstdCompressor.hpp>
#include <clp_s/ZstdDecompressor.hpp>

namespace clp_s {
/**
 * A summary of the distinct values stored in one column of one schema table, used at search time
 * to skip tables that cannot satisfy an equality predicate on the column.
 *
 * Values are identified by a 64-bit key: the value itself for integer columns and the dictionary ID
 * for variable string columns. The filter records the key range and a Bloom filter over the keys,
 * so `may_contain` never reports a stored key as absent.
 */
class ColumnValueFilter {
public:
    // Constants
    static constexpr double cTargetFalsePositiveRate{0.01};
    // Bloom filters are sized for the target false positive rate but never past this many bits; a
    // column with more distinct keys than that supports gets a filter with a higher rate instead.
    static constexpr size_t cMaxBloomBits{size_t{1} << 22};

    // Factory functions
    /**
     * Builds a filter over `keys`.
     * @param keys The column's distinct keys.
     * @return The filter, or an error code indicating the failure:
     * - Forwards `filter::BloomFilter::create`'s return values.
     */
    [[nodiscard]] static auto create(std::span<int64_t const> keys)
            -> ystdlib::error_handling::Result<ColumnValueFilter>;

    /**
     * Reads a filter written by `compress`.
     * @param decompressor
     * @return The filter, or an error code indicating the failure:
     * - filter::ErrorCodeEnum::ReadFailure if the payload is truncated.
     * - Forwards `filter::BloomFilter::try_read`'s return values.
     */
    [[nodiscard]] static auto decompress(ZstdDecompressor& decompressor)
            -> ystdlib::error_handling::Result<ColumnValueFilter>;

    // Methods
    /**
     * @param key
     * @return false if `key` is definitely not stored in the column, true if it may be.
     */
    [[nodiscard]] auto may_contain(int64_t key) const -> bool;

    /**
     * @param low
     * @param high
     * @return false if no stored key lies in `[low, high]`, true if one may.
     */
    [[nodiscard]] auto may_contain_in_range(int64_t low, int64_t high) const -> bool {
        return 0 != m_num_keys && low <= m_max_key && high >= m_min_key;
    }

    [[nodiscard]] auto num_keys() const -> size_t { return m_num_keys; }

    /**
     * Writes the filter.
     * @param compressor
     */
    auto compress(ZstdCompressor& compressor) const -> void;

private:
    // Constructors
    ColumnValueFilter() = default;

    // Data members
    uint64_t m_num_keys{0};
    int64_t m_min_key{std::numeric_limits<int64_t>::max()};
    int64_t m_max_key{std::numeric_limits<int64_t>::min()};
    std::optional<filter::BloomFilter> m_bloom;
};
}  // namespace clp_s

#endif  // CLP_S_COLUMNVALUEFILTER_HPP
