#ifndef CLP_S_NUMERICCOLUMNVIEW_HPP
#define CLP_S_NUMERICCOLUMNVIEW_HPP

#include <algorithm>
#include <cstddef>
#include <cstdint>
#include <optional>
#include <span>
#include <stdexcept>
#include <utility>
#include <vector>

#include "AdaptiveColumn.hpp"

namespace clp_s {
/**
 * A validated view of a constant, frame-of-reference, dictionary, or run-length numeric column.
 * The encoded bytes must outlive the view and remain unchanged. Only cumulative run ends are
 * allocated; row values remain in their archive representation. No archive bytes or comparison
 * semantics change.
 */
class NumericColumnView {
public:
    /**
     * Validates a supported encoding without expanding row values. Returns nullopt for codecs
     * that require ordinary decoding (plain, delta, and byte-stream split).
     * @throws std::runtime_error for malformed supported encodings or unknown tags.
     */
    [[nodiscard]] static auto
    try_load(std::span<char const> encoded, adaptive_column::Type type, size_t rows)
            -> std::optional<NumericColumnView>;

    /** Returns the unchanged 64-bit value at a row, rejecting out-of-range access. */
    [[nodiscard]] auto bits_at(size_t row) const -> uint64_t;

    /** Conservative bounds for integer frame-of-reference values, when the range cannot wrap. */
    [[nodiscard]] auto integer_bounds() const -> std::optional<std::pair<int64_t, int64_t>>;

    /** Validates unsigned references against an exclusive bound without expanding row values. */
    auto validate_unsigned_range(uint64_t upper_bound) const -> void;

    /**
     * ORs matching rows into a bitmap of zero/one bytes. Existing matches from other occurrences of
     * the field remain set. The predicate receives the original integer/double bits, and is
     * evaluated once per constant, dictionary entry, or run rather than once per row.
     */
    template <typename Predicate>
    auto or_matches(std::span<uint8_t> bitmap, Predicate matches) const -> void {
        if (bitmap.size() != m_rows) {
            throw std::invalid_argument{"Numeric column bitmap size does not match row count"};
        }
        if (m_rows == 0) {
            return;
        }
        switch (m_kind) {
            case Kind::Constant:
                if (matches(m_base)) {
                    std::ranges::fill(bitmap, 1);
                }
                break;
            case Kind::FrameOfReference:
                for_each_code([&](size_t row, uint64_t offset) {
                    if (matches(m_base + offset)) {
                        bitmap[row] = 1;
                    }
                });
                break;
            case Kind::Dictionary: {
                std::vector<uint8_t> accepted(m_values.size() / 8);
                for (size_t i{0}; i < accepted.size(); ++i) {
                    accepted[i] = matches(read_word(m_values, i * 8));
                }
                or_dictionary_matches(accepted, bitmap);
                break;
            }
            case Kind::RunLength: {
                size_t begin{};
                for (size_t run{0}; run < m_run_ends.size(); ++run) {
                    auto const end{m_run_ends[run]};
                    if (matches(read_word(m_values, run * 12))) {
                        std::ranges::fill(bitmap.subspan(begin, end - begin), 1);
                    }
                    begin = end;
                }
                break;
            }
        }
    }

private:
    enum class Kind {
        Constant,
        FrameOfReference,
        Dictionary,
        RunLength
    };

    explicit NumericColumnView(Kind kind, size_t rows) : m_kind{kind}, m_rows{rows} {}

    static auto read_word(std::span<char const> bytes, size_t offset) -> uint64_t;
    auto code_at(size_t row) const -> uint64_t;
    auto validate_codes() const -> void;
    auto or_dictionary_matches(std::span<uint8_t const> accepted, std::span<uint8_t> bitmap) const
            -> void;

    template <typename Emit>
    auto for_each_code(Emit emit) const -> void {
        unsigned __int128 pending{};
        unsigned bits{};
        size_t input{};
        auto const mask{(static_cast<unsigned __int128>(1) << m_width) - 1};
        for (size_t row{0}; row < m_rows; ++row) {
            while (bits < m_width) {
                pending |= static_cast<unsigned __int128>(
                                   static_cast<unsigned char>(m_codes[input++])
                           )
                           << bits;
                bits += 8;
            }
            emit(row, static_cast<uint64_t>(pending & mask));
            pending >>= m_width;
            bits -= m_width;
        }
    }

    Kind m_kind;
    size_t m_rows;
    uint64_t m_base{};
    unsigned m_width{};
    std::span<char const> m_values;
    std::span<char const> m_codes;
    std::vector<size_t> m_run_ends;
};
}  // namespace clp_s

#endif  // CLP_S_NUMERICCOLUMNVIEW_HPP
