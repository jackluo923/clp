// Tests for `clp_s::ColumnValueFilter`: soundness of membership/range checks and the serialization
// round trip.

#include <cstddef>
#include <cstdint>
#include <filesystem>
#include <limits>
#include <span>
#include <utility>
#include <vector>

#include <catch2/catch_test_macros.hpp>

#include <clp_s/ColumnValueFilter.hpp>
#include <clp_s/ErrorCode.hpp>
#include <clp_s/FileWriter.hpp>
#include <clp_s/ZstdCompressor.hpp>
#include <clp_s/ZstdDecompressor.hpp>

using clp_s::ColumnValueFilter;

namespace {
auto make_filter(std::vector<int64_t> const& keys) -> ColumnValueFilter {
    auto result{ColumnValueFilter::create(std::span<int64_t const>{keys})};
    REQUIRE(false == result.has_error());
    return std::move(result.value());
}

auto round_trip(ColumnValueFilter const& filter) -> ColumnValueFilter {
    auto const path{std::filesystem::temp_directory_path() / "clp-s-column-value-filter.zst"};
    {
        clp_s::FileWriter writer;
        writer.open(path.string(), clp_s::FileWriter::OpenMode::CreateForWriting);
        clp_s::ZstdCompressor compressor;
        compressor.open(writer);
        filter.compress(compressor);
        compressor.close();
        writer.close();
    }
    clp_s::ZstdDecompressor decompressor;
    REQUIRE(clp_s::ErrorCodeSuccess == decompressor.open(path.string()));
    auto result{ColumnValueFilter::decompress(decompressor)};
    decompressor.close();
    std::filesystem::remove(path);
    REQUIRE(false == result.has_error());
    return std::move(result.value());
}
}  // namespace

TEST_CASE("clp_s_column_value_filter_membership", "[clpp][ColumnValueFilter]") {
    std::vector<int64_t> keys;
    constexpr int64_t cBase{1'073'741'825};
    constexpr size_t cNumKeys{10'000};
    keys.reserve(cNumKeys);
    for (size_t i{0}; i < cNumKeys; ++i) {
        keys.push_back(cBase + static_cast<int64_t>(3 * i));
    }
    keys.push_back(std::numeric_limits<int64_t>::min());
    keys.push_back(-1);
    auto const filter{make_filter(keys)};
    REQUIRE(keys.size() == filter.num_keys());

    // Stored keys are never reported absent.
    for (auto const key : keys) {
        REQUIRE(filter.may_contain(key));
    }
    // Keys outside the range are rejected without consulting the Bloom filter.
    REQUIRE(false == filter.may_contain(std::numeric_limits<int64_t>::max()));
    REQUIRE(false == filter.may_contain(cBase + 3 * static_cast<int64_t>(cNumKeys) + 100));
    // Most absent keys inside the range are rejected too (1% target false positive rate).
    size_t false_positives{0};
    for (size_t i{0}; i < cNumKeys; ++i) {
        if (filter.may_contain(cBase + static_cast<int64_t>(3 * i) + 1)) {
            ++false_positives;
        }
    }
    REQUIRE(false_positives < cNumKeys / 20);

    // The range check only knows the key bounds, so gaps inside them are not detected.
    REQUIRE(filter.may_contain_in_range(-1, -1));
    REQUIRE(filter.may_contain_in_range(0, cBase));
    REQUIRE(filter.may_contain_in_range(0, cBase - 1));
    auto const max_key{cBase + 3 * static_cast<int64_t>(cNumKeys - 1)};
    REQUIRE(filter.may_contain_in_range(max_key, std::numeric_limits<int64_t>::max()));
    REQUIRE(false
            == filter.may_contain_in_range(max_key + 1, std::numeric_limits<int64_t>::max()));
}

TEST_CASE("clp_s_column_value_filter_empty", "[clpp][ColumnValueFilter]") {
    auto const filter{make_filter({})};
    REQUIRE(0 == filter.num_keys());
    REQUIRE(false == filter.may_contain(0));
    REQUIRE(false
            == filter.may_contain_in_range(
                    std::numeric_limits<int64_t>::min(),
                    std::numeric_limits<int64_t>::max()
            ));
    auto const read{round_trip(filter)};
    REQUIRE(0 == read.num_keys());
    REQUIRE(false == read.may_contain(0));
}

TEST_CASE("clp_s_column_value_filter_round_trip", "[clpp][ColumnValueFilter]") {
    std::vector<int64_t> const keys{5667, 5668, 1'073'746'491, 42, 0, -7};
    auto const filter{make_filter(keys)};
    auto const read{round_trip(filter)};
    REQUIRE(keys.size() == read.num_keys());
    for (auto const key : keys) {
        REQUIRE(read.may_contain(key));
    }
    REQUIRE(false == read.may_contain(-8));
    REQUIRE(false == read.may_contain(1'073'746'492));
    REQUIRE(read.may_contain_in_range(5000, 5999));
    REQUIRE(read.may_contain_in_range(-100, -7));
    REQUIRE(false == read.may_contain_in_range(-100, -8));
    REQUIRE(false == read.may_contain_in_range(1'073'746'492, 2'000'000'000));
}

TEST_CASE("clp_s_column_value_filter_large_column", "[clpp][ColumnValueFilter]") {
    // More keys than the bit budget supports at the target rate: the filter stays sound and
    // its Bloom filter is capped rather than growing without bound.
    constexpr size_t cNumKeys{2'000'000};
    std::vector<int64_t> keys(cNumKeys);
    for (size_t i{0}; i < cNumKeys; ++i) {
        keys[i] = static_cast<int64_t>(i * 2);
    }
    auto const filter{make_filter(keys)};
    for (size_t i{0}; i < cNumKeys; i += 997) {
        REQUIRE(filter.may_contain(keys[i]));
    }
    size_t false_positives{0};
    constexpr size_t cNumProbes{100'000};
    for (size_t i{0}; i < cNumProbes; ++i) {
        if (filter.may_contain(static_cast<int64_t>(i * 2 + 1))) {
            ++false_positives;
        }
    }
    // Degraded but still useful.
    REQUIRE(false_positives < cNumProbes / 2);
}
