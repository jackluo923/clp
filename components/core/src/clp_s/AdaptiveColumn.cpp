#include "AdaptiveColumn.hpp"

#include <algorithm>
#include <bit>
#include <cstring>
#include <limits>
#include <stdexcept>
#include <unordered_map>
#include <utility>

namespace clp_s::adaptive_column {
namespace {
constexpr size_t cMaxDictionarySize{1U << 16};
using Bytes = std::vector<char>;

auto corrupt() -> void {
    throw std::runtime_error{"Invalid adaptive column"};
}

template <typename T>
auto append(Bytes& out, T value) -> void {
    // Archive primitives use little-endian encoding, including when the host is big-endian.
    for (size_t i{0}; i < sizeof(T); ++i) {
        out.push_back(static_cast<char>(value >> (i * 8)));
    }
}

class Input {
public:
    explicit Input(std::span<char const> bytes) : m_bytes{bytes} {}

    template <typename T>
    auto read() -> T {
        if (m_bytes.size() < sizeof(T)) {
            corrupt();
        }
        T value{};
        for (size_t i{0}; i < sizeof(T); ++i) {
            value |= static_cast<T>(static_cast<unsigned char>(m_bytes[i])) << (i * 8);
        }
        m_bytes = m_bytes.subspan(sizeof(T));
        return value;
    }

    auto remaining() const -> size_t { return m_bytes.size(); }

private:
    std::span<char const> m_bytes;
};

auto word(std::span<char const> bytes, size_t i) -> uint64_t {
    Input input{bytes.subspan(i * 8, 8)};
    return input.read<uint64_t>();
}

template <typename Get>
auto pack(Bytes& out, size_t count, unsigned width, Get get) -> void {
    // At most seven pending bits plus one 64-bit value.
    unsigned __int128 pending{};
    unsigned bits{};
    for (size_t i{0}; i < count; ++i) {
        pending |= static_cast<unsigned __int128>(get(i)) << bits;
        bits += width;
        while (bits >= 8) {
            out.push_back(static_cast<char>(pending));
            pending >>= 8;
            bits -= 8;
        }
    }
    if (bits != 0) {
        out.push_back(static_cast<char>(pending));
    }
}

template <typename Emit>
auto unpack(Input& in, size_t count, unsigned width, Emit emit) -> void {
    if (width > 64 || count > (std::numeric_limits<size_t>::max() - 7) / 64
        || (count * width + 7) / 8 > in.remaining())
    {
        corrupt();
    }
    unsigned __int128 pending{};
    unsigned bits{};
    auto const mask{(static_cast<unsigned __int128>(1) << width) - 1};
    for (size_t i{0}; i < count; ++i) {
        while (bits < width) {
            pending |= static_cast<unsigned __int128>(in.read<uint8_t>()) << bits;
            bits += 8;
        }
        emit(static_cast<uint64_t>(pending & mask));
        pending >>= width;
        bits -= width;
    }
    if (pending != 0) {
        corrupt();
    }
}
}  // namespace

auto encode(std::span<char const> values, Type type, ZSTD_CCtx& context, int level) -> Frame {
    EncodingBuffers buffers;
    encode(values, type, context, level, buffers);
    return std::move(buffers.best);
}

auto encode(
        std::span<char const> values,
        Type type,
        ZSTD_CCtx& context,
        int level,
        EncodingBuffers& buffers
) -> Frame const& {
    if (values.size() % 8 != 0) {
        corrupt();
    }
    size_t const count{values.size() / 8};
    auto& best{buffers.best};
    auto& candidate{buffers.candidate};
    auto& compressed{buffers.compressed};
    best.compressed.clear();
    best.uncompressed_size = 0;
    candidate.clear();
    auto measure = [&]() {
        compressed.resize(ZSTD_compressBound(candidate.size()));
        auto const size{ZSTD_compressCCtx(
                &context,
                compressed.data(),
                compressed.size(),
                candidate.data(),
                candidate.size(),
                level
        )};
        if (ZSTD_isError(size)) {
            throw std::runtime_error{ZSTD_getErrorName(size)};
        }
        compressed.resize(size);
        if (best.compressed.empty() || size < best.compressed.size()) {
            best.compressed.swap(compressed);
            best.uncompressed_size = candidate.size();
        }
        candidate.clear();
    };
    candidate.push_back(0);
    candidate.insert(candidate.end(), values.begin(), values.end());
    measure();
    if (count == 0) {
        return best;
    }

    uint64_t const first{word(values, 0)};
    int64_t min{std::bit_cast<int64_t>(first)}, max{min};
    int64_t min_delta{std::numeric_limits<int64_t>::max()};
    int64_t max_delta{std::numeric_limits<int64_t>::min()};
    bool valid_delta{count > 1};
    size_t runs{1};
    uint64_t previous{first};
    std::unordered_map<uint64_t, uint64_t> dictionary;
    std::vector<uint64_t> entries;
    bool small_dictionary{true};
    for (size_t i{0}; i < count; ++i) {
        auto const value{word(values, i)};
        auto const signed_value{std::bit_cast<int64_t>(value)};
        min = std::min(min, signed_value);
        max = std::max(max, signed_value);
        if (small_dictionary && !dictionary.contains(value)) {
            if (entries.size() == cMaxDictionarySize) {
                small_dictionary = false;
                dictionary.clear();
                entries.clear();
            } else {
                dictionary.emplace(value, entries.size());
                entries.push_back(value);
            }
        }
        if (i != 0) {
            runs += (value != previous);
            auto const delta{
                    static_cast<__int128>(signed_value) - std::bit_cast<int64_t>(previous)
            };
            if (delta < std::numeric_limits<int64_t>::min()
                || delta > std::numeric_limits<int64_t>::max())
            {
                valid_delta = false;
            } else {
                min_delta = std::min(min_delta, static_cast<int64_t>(delta));
                max_delta = std::max(max_delta, static_cast<int64_t>(delta));
            }
        }
        previous = value;
    }
    if (runs == 1) {
        candidate.push_back(Type::Integer == type ? 5 : 3);
        append(candidate, first);
        measure();
        return best;
    }
    if (Type::Float == type) {
        candidate.push_back(1);  // Byte-stream split preserves every IEEE-754 bit.
        for (size_t plane{0}; plane < 8; ++plane) {
            for (size_t i{0}; i < count; ++i) {
                candidate.push_back(values[i * 8 + plane]);
            }
        }
        measure();
    } else {
        auto const width{std::bit_width(static_cast<uint64_t>(max) - static_cast<uint64_t>(min))};
        candidate.push_back(1);
        append(candidate, static_cast<uint64_t>(min));
        candidate.push_back(static_cast<char>(width));
        pack(candidate, count, width, [&](size_t i) {
            return word(values, i) - static_cast<uint64_t>(min);
        });
        measure();
        if (valid_delta) {
            auto const delta_width{std::bit_width(
                    static_cast<uint64_t>(max_delta) - static_cast<uint64_t>(min_delta)
            )};
            candidate.push_back(2);
            append(candidate, first);
            append(candidate, static_cast<uint64_t>(min_delta));
            candidate.push_back(static_cast<char>(delta_width));
            pack(candidate, count - 1, delta_width, [&](size_t i) {
                return word(values, i + 1) - word(values, i) - static_cast<uint64_t>(min_delta);
            });
            measure();
        }
    }
    if (small_dictionary && entries.size() < count) {
        candidate.push_back(Type::Integer == type ? 3 : 2);
        append(candidate, static_cast<uint32_t>(entries.size()));
        for (auto value : entries) {
            append(candidate, value);
        }
        auto const width{std::bit_width(entries.size() - 1)};
        candidate.push_back(static_cast<char>(width));
        pack(candidate, count, width, [&](size_t i) { return dictionary.at(word(values, i)); });
        measure();
        if (Type::Float == type) {
            // A value-ordered dictionary with byte-aligned codes often compresses gradual signals
            // better than the smallest bit width. Ordering operates on bits to retain NaN payloads.
            auto order = [](uint64_t bits) {
                return (bits >> 63) ? ~bits : bits ^ (uint64_t{1} << 63);
            };
            std::ranges::sort(entries, [&](auto lhs, auto rhs) { return order(lhs) < order(rhs); });
            candidate.push_back(2);
            append(candidate, static_cast<uint32_t>(entries.size()));
            for (size_t i{0}; i < entries.size(); ++i) {
                dictionary[entries[i]] = i;
                append(candidate, entries[i]);
            }
            auto const byte_width{(width + 7) / 8 * 8};
            candidate.push_back(static_cast<char>(byte_width));
            pack(candidate, count, byte_width, [&](size_t i) {
                return dictionary.at(word(values, i));
            });
            measure();
        }
    }
    if (runs < count / 2 && count <= std::numeric_limits<uint32_t>::max()) {
        candidate.push_back(4);
        append(candidate, static_cast<uint32_t>(runs));
        for (size_t i{0}; i < count;) {
            auto const value{word(values, i)};
            size_t end{i + 1};
            while (end < count && word(values, end) == value) {
                ++end;
            }
            append(candidate, value);
            append(candidate, static_cast<uint32_t>(end - i));
            i = end;
        }
        measure();
    }
    return best;
}

auto decode(std::span<char const> encoded, Type type, size_t count) -> Bytes {
    if (count > std::numeric_limits<size_t>::max() / 8) {
        corrupt();
    }
    Input in{encoded};
    auto const scheme{in.read<uint8_t>()};
    Bytes out;
    auto emit = [&](uint64_t value) { append(out, value); };
    if (scheme == 0) {
        if (in.remaining() != count * 8) {
            corrupt();
        }
        return Bytes{encoded.begin() + 1, encoded.end()};
    }
    if (scheme == (Type::Integer == type ? 5 : 3)) {
        auto const value{in.read<uint64_t>()};
        if (in.remaining() != 0) {
            corrupt();
        }
        out.reserve(count * 8);
        for (size_t i{0}; i < count; ++i) {
            emit(value);
        }
    } else if (scheme == 1 && Type::Float == type) {
        if (in.remaining() != count * 8) {
            corrupt();
        }
        out.resize(count * 8);
        for (size_t plane{0}; plane < 8; ++plane) {
            for (size_t i{0}; i < count; ++i) {
                out[i * 8 + plane] = static_cast<char>(in.read<uint8_t>());
            }
        }
    } else if (scheme == 1 || (scheme == 2 && Type::Integer == type)) {
        uint64_t previous{};
        if (scheme == 2) {
            if (count == 0) {
                corrupt();
            }
            previous = in.read<uint64_t>();
        }
        auto const base{std::bit_cast<int64_t>(in.read<uint64_t>())};
        auto const width{in.read<uint8_t>()};
        if (scheme == 2) {
            emit(previous);
        }
        unpack(in, count - (scheme == 2), width, [&](uint64_t offset) {
            auto value{static_cast<__int128>(base) + offset};
            if (value > std::numeric_limits<int64_t>::max()) {
                corrupt();
            }
            if (scheme == 2) {
                value += std::bit_cast<int64_t>(previous);
            }
            if (value < std::numeric_limits<int64_t>::min()
                || value > std::numeric_limits<int64_t>::max())
            {
                corrupt();
            }
            previous = static_cast<uint64_t>(value);
            emit(previous);
        });
    } else if (scheme == (Type::Integer == type ? 3 : 2)) {
        auto const size{in.read<uint32_t>()};
        if (size == 0 || size > cMaxDictionarySize || size > in.remaining() / 8) {
            corrupt();
        }
        std::vector<uint64_t> entries;
        entries.reserve(size);
        for (size_t i{0}; i < size; ++i) {
            entries.push_back(in.read<uint64_t>());
        }
        auto const width{in.read<uint8_t>()};
        unpack(in, count, width, [&](uint64_t id) {
            if (id >= entries.size()) {
                corrupt();
            }
            emit(entries[id]);
        });
    } else if (scheme == 4) {
        auto const runs{in.read<uint32_t>()};
        if (runs > count || runs > in.remaining() / 12) {
            corrupt();
        }
        for (size_t i{0}; i < runs; ++i) {
            auto const value{in.read<uint64_t>()};
            auto const length{in.read<uint32_t>()};
            if (length == 0 || length > count - out.size() / 8) {
                corrupt();
            }
            for (size_t j{0}; j < length; ++j) {
                emit(value);
            }
        }
    } else {
        corrupt();
    }
    if (in.remaining() != 0 || out.size() != count * 8) {
        corrupt();
    }
    return out;
}
}  // namespace clp_s::adaptive_column
