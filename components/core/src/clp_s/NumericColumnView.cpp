#include "NumericColumnView.hpp"

#include <array>
#include <bit>
#include <cstring>
#include <limits>

namespace clp_s {
namespace {
[[noreturn]] auto corrupt() -> void {
    throw std::runtime_error{"Invalid encoded numeric column"};
}

template <typename T>
auto read(std::span<char const> bytes, size_t offset) -> T {
    if (offset > bytes.size() || sizeof(T) > bytes.size() - offset) {
        corrupt();
    }
    T value{};
    if constexpr (std::endian::native == std::endian::little) {
        std::memcpy(&value, bytes.data() + offset, sizeof(T));
    } else {
        for (size_t i{0}; i < sizeof(T); ++i) {
            value |= static_cast<T>(static_cast<unsigned char>(bytes[offset + i])) << (i * 8);
        }
    }
    return value;
}

template <unsigned Width>
auto or_dictionary_bytes(
        std::span<char const> codes,
        std::span<uint8_t const> accepted,
        std::span<uint8_t> bitmap
) -> void {
    constexpr size_t cLanes{8 / Width};
    constexpr size_t cMask{(1U << Width) - 1};
    std::array<std::array<uint8_t, cLanes>, 256> lookup{};
    for (size_t byte{0}; byte < lookup.size(); ++byte) {
        for (size_t lane{0}; lane < cLanes; ++lane) {
            auto const id{(byte >> (lane * Width)) & cMask};
            lookup[byte][lane] = id < accepted.size() ? accepted[id] : 0;
        }
    }
    size_t const full_bytes{bitmap.size() / cLanes};
    for (size_t i{0}; i < full_bytes; ++i) {
        auto const& masks{lookup[static_cast<unsigned char>(codes[i])]};
        for (size_t lane{0}; lane < cLanes; ++lane) {
            bitmap[i * cLanes + lane] |= masks[lane];
        }
    }
    if (bitmap.size() % cLanes != 0) {
        auto const& masks{lookup[static_cast<unsigned char>(codes[full_bytes])]};
        for (size_t lane{0}; lane < bitmap.size() % cLanes; ++lane) {
            bitmap[full_bytes * cLanes + lane] |= masks[lane];
        }
    }
}
}  // namespace

auto NumericColumnView::read_word(std::span<char const> bytes, size_t offset) -> uint64_t {
    return read<uint64_t>(bytes, offset);
}

auto
NumericColumnView::try_load(std::span<char const> encoded, adaptive_column::Type type, size_t rows)
        -> std::optional<NumericColumnView> {
    if (rows > std::numeric_limits<size_t>::max() / 8) {
        corrupt();
    }
    auto const tag{read<uint8_t>(encoded, 0)};
    bool const integer{type == adaptive_column::Type::Integer};
    if (tag == 0 || (integer && tag == 2) || (!integer && tag == 1)) {
        return std::nullopt;
    }
    auto const payload{encoded.subspan(1)};
    if (tag == (integer ? 5 : 3)) {
        NumericColumnView view{Kind::Constant, rows};
        view.m_base = read_word(payload, 0);
        if (payload.size() != 8) {
            corrupt();
        }
        return view;
    }
    if (integer && tag == 1) {
        NumericColumnView view{Kind::FrameOfReference, rows};
        view.m_base = read_word(payload, 0);
        view.m_width = read<uint8_t>(payload, 8);
        view.m_codes = payload.subspan(9);
        view.validate_codes();
        // Most columns prove this bound from the bit width, without scanning their offsets.
        auto const max_offset{
                uint64_t{INT64_MAX} - static_cast<uint64_t>(std::bit_cast<int64_t>(view.m_base))
        };
        auto const mask{(static_cast<unsigned __int128>(1) << view.m_width) - 1};
        if (mask > max_offset) {
            view.for_each_code([&](size_t, uint64_t offset) {
                if (offset > max_offset) {
                    corrupt();
                }
            });
        }
        return view;
    }
    if (tag == (integer ? 3 : 2)) {
        NumericColumnView view{Kind::Dictionary, rows};
        auto const count{read<uint32_t>(payload, 0)};
        if (count == 0 || count > (1U << 16) || count > (payload.size() - 4) / 8) {
            corrupt();
        }
        size_t const end{4 + size_t{count} * 8};
        view.m_values = payload.subspan(4, size_t{count} * 8);
        view.m_width = read<uint8_t>(payload, end);
        view.m_codes = payload.subspan(end + 1);
        view.validate_codes();
        if (static_cast<unsigned __int128>(count)
            < (static_cast<unsigned __int128>(1) << view.m_width))
        {
            view.for_each_code([&](size_t, uint64_t id) {
                if (id >= count) {
                    corrupt();
                }
            });
        }
        return view;
    }
    if (tag == 4) {
        NumericColumnView view{Kind::RunLength, rows};
        auto const runs{read<uint32_t>(payload, 0)};
        if (runs > rows || runs != (payload.size() - 4) / 12 || (payload.size() - 4) % 12 != 0) {
            corrupt();
        }
        view.m_values = payload.subspan(4);
        view.m_run_ends.reserve(runs);
        size_t end{};
        for (size_t run{0}; run < runs; ++run) {
            auto const length{read<uint32_t>(view.m_values, run * 12 + 8)};
            if (length == 0 || length > rows - end) {
                corrupt();
            }
            end += length;
            view.m_run_ends.push_back(end);
        }
        if (end != rows) {
            corrupt();
        }
        return view;
    }
    corrupt();
}

auto NumericColumnView::validate_codes() const -> void {
    if (m_width > 64 || m_rows > (std::numeric_limits<size_t>::max() - 7) / 64
        || (m_rows * m_width + 7) / 8 != m_codes.size())
    {
        corrupt();
    }
    unsigned const used{static_cast<unsigned>(m_rows * m_width % 8)};
    if (used != 0 && (static_cast<unsigned char>(m_codes.back()) >> used) != 0) {
        corrupt();
    }
}

auto NumericColumnView::code_at(size_t row) const -> uint64_t {
    if (m_width == 0) {
        return 0;
    }
    size_t const bit{row * m_width};
    size_t const begin{bit / 8};
    unsigned const shift{static_cast<unsigned>(bit % 8)};
    size_t const bytes{(shift + m_width + 7) / 8};
    unsigned __int128 value{};
    for (size_t i{0}; i < bytes; ++i) {
        value |= static_cast<unsigned __int128>(static_cast<unsigned char>(m_codes[begin + i]))
                 << (i * 8);
    }
    return static_cast<uint64_t>(
            (value >> shift) & ((static_cast<unsigned __int128>(1) << m_width) - 1)
    );
}

auto NumericColumnView::bits_at(size_t row) const -> uint64_t {
    if (row >= m_rows) {
        throw std::out_of_range{"Numeric column row is out of range"};
    }
    switch (m_kind) {
        case Kind::Constant:
            return m_base;
        case Kind::FrameOfReference:
            return m_base + code_at(row);
        case Kind::Dictionary:
            return read_word(m_values, code_at(row) * 8);
        case Kind::RunLength: {
            auto const run{
                    std::upper_bound(m_run_ends.begin(), m_run_ends.end(), row) - m_run_ends.begin()
            };
            return read_word(m_values, run * 12);
        }
    }
    corrupt();
}

auto NumericColumnView::integer_bounds() const -> std::optional<std::pair<int64_t, int64_t>> {
    if (m_kind != Kind::FrameOfReference || m_rows == 0) {
        return std::nullopt;
    }
    auto const low{std::bit_cast<int64_t>(m_base)};
    auto const high{static_cast<__int128>(low) + ((static_cast<__int128>(1) << m_width) - 1)};
    if (high > INT64_MAX) {
        return std::nullopt;
    }
    return std::pair{low, static_cast<int64_t>(high)};
}

auto NumericColumnView::or_dictionary_matches(
        std::span<uint8_t const> accepted,
        std::span<uint8_t> bitmap
) const -> void {
    if (std::ranges::all_of(accepted, [](auto value) { return value != 0; })) {
        std::ranges::fill(bitmap, 1);
        return;
    }
    if (std::ranges::none_of(accepted, [](auto value) { return value != 0; })) {
        return;
    }
    if (m_rows >= 256) {
        switch (m_width) {
            case 1:
                return or_dictionary_bytes<1>(m_codes, accepted, bitmap);
            case 2:
                return or_dictionary_bytes<2>(m_codes, accepted, bitmap);
            case 4:
                return or_dictionary_bytes<4>(m_codes, accepted, bitmap);
            case 8:
                return or_dictionary_bytes<8>(m_codes, accepted, bitmap);
        }
    }
    if (m_width == 16) {
        for (size_t row{0}; row < m_rows; ++row) {
            bitmap[row] |= accepted[read<uint16_t>(m_codes, row * 2)];
        }
        return;
    }
    for_each_code([&](size_t row, uint64_t id) { bitmap[row] |= accepted[id]; });
}
}  // namespace clp_s
