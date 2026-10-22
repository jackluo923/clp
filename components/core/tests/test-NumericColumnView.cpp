#include <algorithm>
#include <bit>
#include <cstdint>
#include <limits>
#include <memory>
#include <random>
#include <span>
#include <string>
#include <vector>

#include <catch2/catch_test_macros.hpp>

#include <clp_s/AdaptiveColumn.hpp>
#include <clp_s/ColumnReader.hpp>
#include <clp_s/NumericColumnView.hpp>
#include <clp_s/search/ast/ColumnDescriptor.hpp>
#include <clp_s/search/ast/FilterExpr.hpp>
#include <clp_s/search/ast/Integral.hpp>
#include <clp_s/search/ast/StringLiteral.hpp>
#include <clp_s/search/ColumnScan.hpp>

namespace {
using Bytes = std::vector<char>;
using Type = clp_s::adaptive_column::Type;

template <typename T>
auto append(Bytes& bytes, T value) -> void {
    for (size_t i{0}; i < sizeof(T); ++i) {
        bytes.push_back(static_cast<char>(value >> (i * 8)));
    }
}

auto packed(Bytes& bytes, std::span<uint64_t const> values, unsigned width) -> void {
    size_t const begin{bytes.size()};
    bytes.resize(begin + (values.size() * width + 7) / 8, 0);
    // Simple bit-at-a-time reference encoder, independent of the production code.
    for (size_t row{0}; row < values.size(); ++row) {
        for (unsigned bit{0}; bit < width; ++bit) {
            if ((values[row] >> bit) & 1) {
                size_t const position{row * width + bit};
                bytes[begin + position / 8] |= static_cast<char>(1U << (position % 8));
            }
        }
    }
}

auto check(Bytes const& encoded, Type type, std::vector<uint64_t> const& expected) -> void {
    auto const view{clp_s::NumericColumnView::try_load(encoded, type, expected.size())};
    REQUIRE(view.has_value());
    auto const decoded{clp_s::adaptive_column::decode(encoded, type, expected.size())};
    Bytes plain;
    for (auto value : expected) {
        append(plain, value);
    }
    REQUIRE(decoded == plain);
    for (size_t i{expected.size()}; i > 0; --i) {
        REQUIRE(view->bits_at(i - 1) == expected[i - 1]);
    }
    REQUIRE_THROWS(view->bits_at(expected.size()));
    for (unsigned operation{0}; operation < 6; ++operation) {
        auto compare = [&](uint64_t bits) {
            auto evaluate = [&](auto lhs, auto rhs) {
                switch (operation) {
                    case 0:
                        return lhs == rhs;
                    case 1:
                        return lhs != rhs;
                    case 2:
                        return lhs < rhs;
                    case 3:
                        return lhs > rhs;
                    case 4:
                        return lhs <= rhs;
                    default:
                        return lhs >= rhs;
                }
            };
            return type == Type::Integer ? evaluate(std::bit_cast<int64_t>(bits), int64_t{2})
                                         : evaluate(std::bit_cast<double>(bits), 0.0);
        };
        std::vector<uint8_t> bitmap(expected.size());
        for (size_t i{0}; i < bitmap.size(); ++i) {
            bitmap[i] = i % 5 == 0;
        }
        auto wanted{bitmap};
        for (size_t i{0}; i < wanted.size(); ++i) {
            wanted[i] |= compare(expected[i]);
        }
        view->or_matches(bitmap, compare);
        REQUIRE(bitmap == wanted);
    }
}

auto dictionary(
        std::span<uint64_t const> values,
        std::span<uint64_t const> ids,
        unsigned width,
        Type type
) -> Bytes {
    Bytes encoded{static_cast<char>(type == Type::Integer ? 3 : 2)};
    append(encoded, static_cast<uint32_t>(values.size()));
    for (auto value : values) {
        append(encoded, value);
    }
    encoded.push_back(static_cast<char>(width));
    packed(encoded, ids, width);
    return encoded;
}

template <typename Reader>
auto check_column_scan(Bytes const& encoded, Type type, size_t rows) -> void {
    using namespace clp_s::search;
    using namespace clp_s::search::ast;
    Reader compact{0}, ordinary{0}, other_occurrence{0};
    compact.load_encoded(*clp_s::NumericColumnView::try_load(encoded, type, rows));
    auto decoded{clp_s::adaptive_column::decode(encoded, type, rows)};
    clp_s::BufferViewReader decoded_buffer{decoded.data(), decoded.size()};
    ordinary.load(decoded_buffer, rows);
    Bytes other_values;
    for (size_t row{0}; row < rows; ++row) {
        append(other_values,
               type == Type::Integer ? row % 3 : std::bit_cast<uint64_t>(double(row % 3)));
    }
    clp_s::BufferViewReader other_buffer{other_values.data(), other_values.size()};
    other_occurrence.load(other_buffer, rows);
    auto column{ColumnDescriptor::create_from_escaped_tokens({"value"}, "")};
    column->set_column_id(0);
    column->set_matching_type(type == Type::Integer ? LiteralType::IntegerT : LiteralType::FloatT);
    auto check_filter = [&](std::shared_ptr<Literal> operand, FilterOperation operation) {
        for (bool const inverted : {false, true}) {
            auto const expression{FilterExpr::create(column, operation, operand, inverted)};
            for (bool const repeated : {false, true}) {
                ColumnScan::BasicReaderMap actual{{0, {&compact}}}, expected{{0, {&ordinary}}};
                if (repeated) {
                    // An all-or-none shortcut must preserve another occurrence's matches.
                    actual[0].insert(actual[0].begin(), &other_occurrence);
                    expected[0].insert(expected[0].begin(), &other_occurrence);
                }
                auto scan{
                        ColumnScan::
                                try_create(expression, actual, {}, {}, {}, nullptr, {}, {}, rows)
                };
                auto reference{
                        ColumnScan::
                                try_create(expression, expected, {}, {}, {}, nullptr, {}, {}, rows)
                };
                REQUIRE(scan.has_value());
                REQUIRE(reference.has_value());
                for (size_t row{0}; row < rows; ++row) {
                    REQUIRE(scan->filter(row) == reference->filter(row));
                }
            }
        }
    };
    for (auto const operation :
         {FilterOperation::EQ,
          FilterOperation::NEQ,
          FilterOperation::LT,
          FilterOperation::LTE,
          FilterOperation::GT,
          FilterOperation::GTE})
    {
        for (int64_t const value :
             {INT64_MIN,
              int64_t{-3},
              int64_t{-2},
              int64_t{0},
              int64_t{1},
              int64_t{5},
              int64_t{6},
              INT64_MAX})
        {
            check_filter(Integral::create_from_int(value), operation);
        }
        if (type == Type::Float) {
            for (double const value :
                 {std::numeric_limits<double>::quiet_NaN(),
                  -std::numeric_limits<double>::infinity(),
                  std::numeric_limits<double>::infinity()})
            {
                check_filter(Integral::create_from_float(value), operation);
            }
        }
    }
    for (auto const operation : {FilterOperation::EQ, FilterOperation::NEQ}) {
        for (auto const* pattern : {"*2*", "-*", "?", "*n*", "*0*", "*999*"}) {
            check_filter(StringLiteral::create(pattern), operation);
        }
    }
}
}  // namespace

TEST_CASE("Encoded numeric scans match decoded values at packed widths", "[NumericColumnView]") {
    std::mt19937_64 random{12'345};
    for (auto const width : {0U, 1U, 2U, 3U, 4U, 7U, 8U, 9U, 12U, 16U, 31U, 32U, 63U, 64U}) {
        for (auto const rows : {0U, 1U, 7U, 31U, 256U, 513U}) {
            auto const mask{
                    static_cast<uint64_t>((static_cast<unsigned __int128>(1) << width) - 1)
            };
            std::vector<uint64_t> offsets, expected;
            uint64_t const base{uint64_t{1} << 63};
            for (size_t i{0}; i < rows; ++i) {
                offsets.push_back(random() & mask);
                expected.push_back(base + offsets.back());
            }
            Bytes frame{1};
            append(frame, base);
            frame.push_back(static_cast<char>(width));
            packed(frame, offsets, width);
            check(frame, Type::Integer, expected);

            for (auto const type : {Type::Integer, Type::Float}) {
                size_t const count{std::min<uint64_t>(mask, 256) + 1};
                std::vector<uint64_t> values, ids;
                expected.clear();
                for (size_t i{0}; i < count; ++i) {
                    values.push_back(
                            type == Type::Integer ? uint64_t(i) - 2
                                                  : std::bit_cast<uint64_t>(double(i) - 2.0)
                    );
                }
                for (size_t i{0}; i < rows; ++i) {
                    ids.push_back(random() % count);
                    expected.push_back(values[ids.back()]);
                }
                auto const encoded{dictionary(values, ids, width, type)};
                check(encoded, type, expected);
            }
        }
    }
}

TEST_CASE(
        "Encoded constants and runs preserve bits and avoid repeated comparisons",
        "[NumericColumnView]"
) {
    std::vector<uint64_t> const
            values{0, uint64_t{1} << 63, 0x7ff8'0000'0000'0123, 0xfff0'0000'0000'0000, UINT64_MAX};
    for (auto const type : {Type::Integer, Type::Float}) {
        for (auto const value : values) {
            Bytes constant{static_cast<char>(type == Type::Integer ? 5 : 3)};
            append(constant, value);
            check(constant, type, std::vector<uint64_t>(1025, value));
            auto const view{clp_s::NumericColumnView::try_load(constant, type, 1025)};
            std::vector<uint8_t> bitmap(1025);
            size_t calls{};
            view->or_matches(bitmap, [&](uint64_t) {
                ++calls;
                return true;
            });
            REQUIRE(calls == 1);
            REQUIRE(std::ranges::all_of(bitmap, [](auto bit) { return bit == 1; }));
        }
        Bytes runs{4};
        append(runs, static_cast<uint32_t>(values.size()));
        std::vector<uint64_t> expected;
        for (size_t i{0}; i < values.size(); ++i) {
            append(runs, values[i]);
            append(runs, static_cast<uint32_t>(i * 100 + 1));
            expected.insert(expected.end(), i * 100 + 1, values[i]);
        }
        check(runs, type, expected);
        auto const view{clp_s::NumericColumnView::try_load(runs, type, expected.size())};
        std::vector<uint8_t> bitmap(expected.size());
        size_t calls{};
        view->or_matches(bitmap, [&](uint64_t) {
            ++calls;
            return false;
        });
        REQUIRE(calls == values.size());
        std::vector<uint64_t> ids(expected.size());
        for (size_t i{0}; i < ids.size(); ++i) {
            ids[i] = i % values.size();
            expected[i] = values[ids[i]];
        }
        auto const encoded{dictionary(values, ids, 3, type)};
        check(encoded, type, expected);
        auto const dict{clp_s::NumericColumnView::try_load(encoded, type, ids.size())};
        calls = 0;
        dict->or_matches(bitmap, [&](uint64_t) {
            ++calls;
            return false;
        });
        REQUIRE(calls == values.size());
    }
}

TEST_CASE(
        "Encoded numeric views validate malformed payloads and integer bounds",
        "[NumericColumnView]"
) {
    std::vector<Bytes> cases;
    Bytes constant{5};
    append(constant, uint64_t{42});
    cases.push_back(constant);
    Bytes frame{1};
    append(frame, uint64_t{INT64_MAX - 5});
    frame.push_back(3);
    packed(frame, std::vector<uint64_t>{0, 5, 2}, 3);
    cases.push_back(frame);
    cases.push_back(dictionary(
            std::vector<uint64_t>{10, 20, 30},
            std::vector<uint64_t>{0, 2, 1},
            2,
            Type::Integer
    ));
    Bytes runs{4};
    append(runs, uint32_t{1});
    append(runs, uint64_t{42});
    append(runs, uint32_t{3});
    cases.push_back(runs);
    for (auto const& encoded : cases) {
        REQUIRE(clp_s::NumericColumnView::try_load(encoded, Type::Integer, 3).has_value());
        for (size_t cut{0}; cut < encoded.size(); ++cut) {
            REQUIRE_THROWS(
                    clp_s::NumericColumnView::try_load(
                            std::span{encoded}.first(cut),
                            Type::Integer,
                            3
                    )
            );
        }
        auto extra{encoded};
        extra.push_back(0);
        REQUIRE_THROWS(clp_s::NumericColumnView::try_load(extra, Type::Integer, 3));
    }
    REQUIRE_FALSE(clp_s::NumericColumnView::try_load(frame, Type::Integer, 3)->integer_bounds());
    auto overflow{frame};
    overflow[10] = 7;
    REQUIRE_THROWS(clp_s::NumericColumnView::try_load(overflow, Type::Integer, 3));
    auto bad_padding{frame};
    bad_padding.back() = static_cast<char>(0x80);
    REQUIRE_THROWS(clp_s::NumericColumnView::try_load(bad_padding, Type::Integer, 3));
    auto bad_width{frame};
    bad_width[9] = 65;
    REQUIRE_THROWS(clp_s::NumericColumnView::try_load(bad_width, Type::Integer, 3));
    auto bad_id{cases[2]};
    bad_id.back() = 3;
    REQUIRE_THROWS(clp_s::NumericColumnView::try_load(bad_id, Type::Integer, 3));
    auto zero_run{runs};
    zero_run[13] = 0;
    REQUIRE_THROWS(clp_s::NumericColumnView::try_load(zero_run, Type::Integer, 3));
    Bytes bounded{1};
    append(bounded, uint64_t{100});
    bounded.push_back(2);
    bounded.push_back(0x24);
    auto const bounds{
            clp_s::NumericColumnView::try_load(bounded, Type::Integer, 3)->integer_bounds()
    };
    REQUIRE(bounds.has_value());
    REQUIRE(bounds->first == 100);
    REQUIRE(bounds->second == 103);
    REQUIRE_FALSE(clp_s::NumericColumnView::try_load(std::span{"\0", 1}, Type::Integer, 0));
    REQUIRE_FALSE(clp_s::NumericColumnView::try_load(std::span{"\2", 1}, Type::Integer, 0));
    REQUIRE_FALSE(clp_s::NumericColumnView::try_load(std::span{"\1", 1}, Type::Float, 0));
    REQUIRE_THROWS(clp_s::NumericColumnView::try_load(std::span{"\x7f", 1}, Type::Float, 0));
}

TEST_CASE(
        "Encoded column filters agree with ordinary filters and repeated field semantics",
        "[NumericColumnView]"
) {
    constexpr size_t rows{257};
    for (auto const type : {Type::Integer, Type::Float}) {
        auto verify = [&](Bytes const& encoded) {
            if (type == Type::Integer) {
                check_column_scan<clp_s::Int64ColumnReader>(encoded, type, rows);
            } else {
                check_column_scan<clp_s::FloatColumnReader>(encoded, type, rows);
            }
        };
        Bytes constant{static_cast<char>(type == Type::Integer ? 5 : 3)};
        append(constant, uint64_t{1} << 63);
        verify(constant);
        std::vector<uint64_t> const values{
                type == Type::Integer
                        ? std::vector<uint64_t>{uint64_t{1} << 63, uint64_t(-2), 0, INT64_MAX}
                        : std::vector<uint64_t>{
                                  0x7ff8'0000'0000'0123,
                                  0xfff0'0000'0000'0000,
                                  0x8000'0000'0000'0000,
                                  0,
                                  std::bit_cast<uint64_t>(1.25),
                                  0x7ff0'0000'0000'0000
                          }
        };
        std::vector<uint64_t> ids(rows);
        for (size_t row{0}; row < rows; ++row) {
            ids[row] = row % values.size();
        }
        verify(dictionary(values, ids, type == Type::Integer ? 2 : 3, type));
        Bytes runs{4};
        append(runs, uint32_t{2});
        append(runs, values[0]);
        append(runs, uint32_t{100});
        append(runs, values[2]);
        append(runs, uint32_t{rows - 100});
        verify(runs);
        if (type == Type::Integer) {
            Bytes frame{1};
            append(frame, uint64_t(-2));
            frame.push_back(3);
            for (size_t row{0}; row < rows; ++row) {
                ids[row] = row % 8;
            }
            packed(frame, ids, 3);
            verify(frame);
        }
    }
}

TEST_CASE(
        "Numeric column readers switch safely between encoded and ordinary values",
        "[NumericColumnView]"
) {
    Bytes integer{5};
    append(integer, uint64_t{1} << 63);
    clp_s::Int64ColumnReader reader{0};
    reader.load_encoded(*clp_s::NumericColumnView::try_load(integer, Type::Integer, 2));
    REQUIRE(std::get<int64_t>(reader.extract_value(1)) == INT64_MIN);
    std::string text;
    reader.extract_string_value_into_buffer(0, text);
    REQUIRE(text == "-9223372036854775808");
    Bytes plain;
    append(plain, uint64_t{7});
    clp_s::BufferViewReader buffer{plain.data(), plain.size()};
    reader.load(buffer, 1);
    REQUIRE(reader.encoded_values() == nullptr);
    REQUIRE(std::get<int64_t>(reader.extract_value(0)) == 7);

    Bytes floating{3};
    append(floating, uint64_t{0x8000'0000'0000'0000});
    clp_s::FloatColumnReader floats{1};
    floats.load_encoded(*clp_s::NumericColumnView::try_load(floating, Type::Float, 1));
    REQUIRE(std::bit_cast<uint64_t>(std::get<double>(floats.extract_value(0)))
            == 0x8000'0000'0000'0000);
    text.clear();
    floats.extract_string_value_into_buffer(0, text);
    REQUIRE(text == "-0.000000");
    plain.clear();
    append(plain, std::bit_cast<uint64_t>(1.25));
    clp_s::BufferViewReader float_buffer{plain.data(), plain.size()};
    floats.load(float_buffer, 1);
    REQUIRE(floats.encoded_values() == nullptr);
    REQUIRE(std::get<double>(floats.extract_value(0)) == 1.25);
}
