#include <algorithm>
#include <bit>
#include <cstdint>
#include <limits>
#include <memory>
#include <random>
#include <span>
#include <unordered_set>
#include <vector>

#include <catch2/catch_test_macros.hpp>
#include <zstd.h>

#include <clp_s/AdaptiveColumn.hpp>
#include <clp_s/PackedStreamReader.hpp>
#include <clp_s/search/DictionaryIdSet.hpp>
#include <clp_s/SingleFileArchiveDefs.hpp>

namespace {
using namespace clp_s::adaptive_column;
using Bytes = std::vector<char>;

template <typename T>
auto append(Bytes& bytes, T value) -> void {
    for (size_t i{0}; i < sizeof(T); ++i) {
        bytes.push_back(static_cast<char>(value >> (8 * i)));
    }
}

auto bytes(std::vector<uint64_t> const& values) -> Bytes {
    Bytes result;
    for (auto value : values) {
        append(result, value);
    }
    return result;
}

auto check(Bytes const& encoded, Type type, std::vector<uint64_t> const& expected) -> void {
    REQUIRE(decode(encoded, type, expected.size()) == bytes(expected));
    for (size_t i{0}; i < encoded.size(); ++i) {
        REQUIRE_THROWS(decode(std::span<char const>{encoded}.first(i), type, expected.size()));
    }
    Bytes extra(encoded.size() + 1, 0);
    std::copy(encoded.begin(), encoded.end(), extra.begin());
    REQUIRE_THROWS(decode(extra, type, expected.size()));
}
}  // namespace

TEST_CASE("Adaptive column wire formats and malformed input", "[AdaptiveColumn]") {
    // Fixed vectors follow Rust's little-endian, least-significant-bit-first format.
    check({0}, Type::Integer, {});
    Bytes plain{0};
    append(plain, uint64_t{0x8000'0000'0000'0000});
    append(plain, uint64_t{0x7fff'ffff'ffff'ffff});
    check(plain, Type::Integer, {0x8000'0000'0000'0000, 0x7fff'ffff'ffff'ffff});
    Bytes constant{5};
    append(constant, uint64_t{42});
    check(constant, Type::Integer, {42, 42, 42});
    constant[0] = 3;
    check(constant, Type::Float, {42, 42, 42});
    Bytes frame{1};
    append(frame, uint64_t{100});
    frame.push_back(2);
    frame.push_back(0x24);  // offsets 0, 1, 2.
    check(frame, Type::Integer, {100, 101, 102});
    Bytes delta{2};
    append(delta, uint64_t{100});
    append(delta, uint64_t{3});
    delta.push_back(0);
    check(delta, Type::Integer, {100, 103, 106});
    Bytes dictionary{3};
    append(dictionary, uint32_t{2});
    append(dictionary, uint64_t{100});
    append(dictionary, uint64_t{200});
    dictionary.push_back(1);
    dictionary.push_back(2);  // indices 0, 1, 0.
    check(dictionary, Type::Integer, {100, 200, 100});
    dictionary[0] = 2;
    check(dictionary, Type::Float, {100, 200, 100});
    dictionary.back() = 3;
    dictionary[dictionary.size() - 2] = 2;
    REQUIRE_THROWS(decode(dictionary, Type::Float, 3));  // dictionary index exceeds its table
    Bytes rle{4};
    append(rle, uint32_t{2});
    append(rle, uint64_t{9});
    append(rle, uint32_t{2});
    append(rle, uint64_t{3});
    append(rle, uint32_t{1});
    check(rle, Type::Integer, {9, 9, 3});
    check(rle, Type::Float, {9, 9, 3});
    REQUIRE_THROWS(decode(rle, Type::Integer, 2));
    REQUIRE_THROWS(decode({rle.data(), rle.size()}, Type::Integer, 4));
    frame[9] = 65;
    REQUIRE_THROWS(decode(frame, Type::Integer, 3));
    REQUIRE_THROWS(decode(std::span<char const>{"\x7f", 1}, Type::Integer, 1));
    delta[1] = static_cast<char>(0xfe);
    for (size_t i{2}; i < 8; ++i) {
        delta[i] = static_cast<char>(0xff);
    }
    delta[8] = 0x7f;
    REQUIRE_THROWS(decode(delta, Type::Integer, 2));  // signed delta overflow
}

TEST_CASE("Adaptive columns preserve integer and floating point bits", "[AdaptiveColumn]") {
    std::unique_ptr<ZSTD_CCtx, decltype(&ZSTD_freeCCtx)> context{ZSTD_createCCtx(), ZSTD_freeCCtx};
    REQUIRE(context != nullptr);
    std::mt19937_64 random{123};
    for (auto type : {Type::Integer, Type::Float}) {
        for (size_t count : {0U, 1U, 7U, 64U, 1025U, 4096U}) {
            for (size_t pattern{0}; pattern < 6; ++pattern) {
                std::vector<uint64_t> values;
                for (size_t i{0}; i < count; ++i) {
                    switch (pattern) {
                        case 0:
                            values.push_back(random());
                            break;
                        case 1:
                            values.push_back(42);
                            break;
                        case 2:
                            values.push_back(1000 + i * 7);
                            break;
                        case 3:
                            values.push_back(random() % 7);
                            break;
                        case 4:
                            values.push_back(i / 200);
                            break;
                        default:
                            values.push_back(std::bit_cast<uint64_t>(double(i) / 100));
                            break;
                    }
                }
                auto const raw{bytes(values)};
                auto const frame{encode(raw, type, *context, 3)};
                Bytes encoded(frame.uncompressed_size);
                auto const size{ZSTD_decompress(
                        encoded.data(),
                        encoded.size(),
                        frame.compressed.data(),
                        frame.compressed.size()
                )};
                REQUIRE(size == encoded.size());
                REQUIRE(decode(encoded, type, count) == raw);
                Bytes plain{0};
                plain.insert(plain.end(), raw.begin(), raw.end());
                Bytes compressed(ZSTD_compressBound(plain.size()));
                auto const plain_size{ZSTD_compressCCtx(
                        context.get(),
                        compressed.data(),
                        compressed.size(),
                        plain.data(),
                        plain.size(),
                        3
                )};
                REQUIRE(frame.compressed.size() <= plain_size);
            }
        }
    }
    // Signed zero, infinities and different NaN payloads remain distinct.
    auto const raw{
            bytes({0,
                   0x8000'0000'0000'0000,
                   0x7ff0'0000'0000'0000,
                   0xfff0'0000'0000'0000,
                   0x7ff8'0000'0000'0001,
                   0x7ff8'0000'0000'1234})
    };
    auto const frame{encode(raw, Type::Float, *context, 3)};
    Bytes encoded(frame.uncompressed_size);
    REQUIRE(ZSTD_decompress(
                    encoded.data(),
                    encoded.size(),
                    frame.compressed.data(),
                    frame.compressed.size()
            )
            == encoded.size());
    REQUIRE(decode(encoded, Type::Float, 6) == raw);
}

TEST_CASE(
        "Reused adaptive buffers preserve frames across types, sizes and levels",
        "[AdaptiveColumn]"
) {
    std::unique_ptr<ZSTD_CCtx, decltype(&ZSTD_freeCCtx)> context{ZSTD_createCCtx(), ZSTD_freeCCtx};
    REQUIRE(context != nullptr);
    EncodingBuffers buffers;
    std::mt19937_64 random{123};
    for (auto level : {1, 3, 9}) {
        for (auto count : {4096, 0, 1, 1000, 2, 8192, 7}) {
            for (auto type : {Type::Float, Type::Integer}) {
                for (auto constant : {false, true}) {
                    std::vector<uint64_t> values(count, 42);
                    if (false == constant) {
                        std::ranges::generate(values, [&]() { return random(); });
                    }
                    auto const raw{bytes(values)};
                    auto const expected{encode(raw, type, *context, level)};
                    auto const& actual{encode(raw, type, *context, level, buffers)};
                    REQUIRE(actual.uncompressed_size == expected.uncompressed_size);
                    REQUIRE(actual.compressed == expected.compressed);
                    Bytes encoded(actual.uncompressed_size);
                    REQUIRE(ZSTD_decompress(
                                    encoded.data(),
                                    encoded.size(),
                                    actual.compressed.data(),
                                    actual.compressed.size()
                            )
                            == encoded.size());
                    REQUIRE(decode(encoded, type, values.size()) == raw);
                }
            }
        }
    }
}

TEST_CASE("Dictionary membership is exact for small dense and sparse sets", "[DictionaryIdSet]") {
    for (size_t count : {0U, 1U, 4U, 5U, 100U, 1000U}) {
        for (int64_t stride : {1, 3, 1'000'000}) {
            std::unordered_set<int64_t> values;
            for (size_t i{0}; i < count; ++i) {
                values.insert(int64_t(i) * stride - 20);
            }
            clp_s::search::DictionaryIdSet const set{values};
            for (int64_t i{-22}; i < 2000; ++i) {
                REQUIRE(set.contains(i) == values.contains(i));
            }
            for (auto id : values) {
                REQUIRE(set.contains(id));
            }
            REQUIRE_FALSE(set.contains(std::numeric_limits<int64_t>::min()));
            REQUIRE_FALSE(set.contains(std::numeric_limits<int64_t>::max()));
        }
    }
    std::unordered_set<int64_t> extreme{INT64_MIN, INT64_MIN + 1, -1, 0, INT64_MAX};
    clp_s::search::DictionaryIdSet const set{extreme};
    for (auto id : extreme) {
        REQUIRE(set.contains(id));
    }
    REQUIRE_FALSE(set.contains(100));
}

TEST_CASE("Separate column metadata validates frame extents", "[SeparateColumns]") {
    auto check_metadata = [](std::vector<uint64_t> const& fields, bool dictionary = false) {
        auto const raw{bytes(fields)};
        Bytes compressed(ZSTD_compressBound(raw.size()));
        auto const size{
                ZSTD_compress(compressed.data(), compressed.size(), raw.data(), raw.size(), 3)
        };
        REQUIRE_FALSE(ZSTD_isError(size));
        clp_s::ZstdDecompressor input;
        input.open(compressed.data(), size);
        clp_s::PackedStreamReader reader;
        REQUIRE_FALSE(reader.read_metadata(input).has_error());
        reader.read_separate_metadata(input, dictionary);
    };
    // Two streams, then one separate stream with one column. Its extent ends at stream 1.
    std::vector<uint64_t> valid{2, 0, 8, 20, 8, 1, 0, 1, 8, 20};
    REQUIRE_NOTHROW(check_metadata(valid));
    auto extended{valid};
    extended.insert(
            extended.begin() + 5,
            {clp_s::cSeparateColumnExtensionMarker, clp_s::cSeparateColumnExtensionVersion}
    );
    REQUIRE_NOTHROW(check_metadata(extended, true));
    REQUIRE_THROWS(check_metadata(extended, false));
    REQUIRE_THROWS(check_metadata(valid, true));
    auto unknown{extended};
    unknown[6] = 2;
    REQUIRE_THROWS(check_metadata(unknown, true));
    for (auto [index, value] :
         std::vector<std::pair<size_t, uint64_t>>{
                 {5, 3},
                 {6, 2},
                 {7, 0},
                 {8, 7},
                 {8, 9},
                 {9, 0},
                 {9, 21},
                 {9, UINT64_MAX}
         })
    {
        auto invalid{valid};
        invalid[index] = value;
        REQUIRE_THROWS(check_metadata(invalid));
    }
}
