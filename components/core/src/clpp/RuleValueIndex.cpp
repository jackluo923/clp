#include "RuleValueIndex.hpp"

#include <cstddef>
#include <cstdint>
#include <string>
#include <string_view>
#include <vector>

#include <ystdlib/error_handling/Result.hpp>

#include <clp_s/ErrorCode.hpp>
#include <clp_s/ZstdCompressor.hpp>
#include <clp_s/ZstdDecompressor.hpp>
#include <clpp/ErrorCode.hpp>

namespace clpp {
namespace {
/**
 * @return The exact index of the `(a, b)` byte pair in a bigram bitset.
 *
 * There are exactly 2^16 byte pairs and the bigram bitsets have 2^16 bits, so a bigram is only
 * ever reported present if it was actually observed.
 */
[[nodiscard]] constexpr auto bigram_index(unsigned char a, unsigned char b) -> size_t {
    return (static_cast<size_t>(a) << 8) | static_cast<size_t>(b);
}

/**
 * @return The index of the `(a, b, c)` byte triple in the trigram bitset.
 *
 * There are 2^24 trigrams but only 2^21 bits, so the hash is folded. Collisions only cause a
 * trigram to be treated as present when it was not, which over-retains but never prunes wrongly.
 */
[[nodiscard]] constexpr auto trigram_index(unsigned char a, unsigned char b, unsigned char c)
        -> size_t {
    constexpr uint64_t fnv_offset_basis{14695981039346656037ULL};
    constexpr uint64_t fnv_prime{1099511628211ULL};
    uint64_t h{fnv_offset_basis};
    h = (h ^ a) * fnv_prime;
    h = (h ^ b) * fnv_prime;
    h = (h ^ c) * fnv_prime;
    h ^= h >> 29;
    return static_cast<size_t>(h) & RuleSignature::cNumTrigramMask;
}

[[nodiscard]] auto test_bit(std::vector<uint32_t> const& words, size_t bit) -> bool {
    return 0 != ((words[bit >> 5] >> (bit & 31)) & 1U);
}

auto set_bit(std::vector<uint32_t>& words, size_t bit) -> void {
    words[bit >> 5] |= (uint32_t{1} << (bit & 31));
}

[[nodiscard]] auto byte_at(std::string_view s, size_t i) -> unsigned char {
    return static_cast<unsigned char>(s[i]);
}

[[nodiscard]] auto bigram_present(std::vector<uint32_t> const& bits, std::string_view s, size_t i)
        -> bool {
    return false == bits.empty() && test_bit(bits, bigram_index(byte_at(s, i), byte_at(s, i + 1)));
}

// Serialization helpers. Bitsets are written as a presence flag followed by their raw words, so an
// unallocated bitset costs one byte.
auto write_bitset(clp_s::ZstdCompressor& compressor, std::vector<uint32_t> const& bits) -> void {
    compressor.write_numeric_value<uint8_t>(bits.empty() ? 0 : 1);
    if (false == bits.empty()) {
        compressor.write(
                reinterpret_cast<char const*>(bits.data()),
                bits.size() * sizeof(uint32_t)
        );
    }
}

[[nodiscard]] auto
read_bitset(clp_s::ZstdDecompressor& decompressor, size_t num_words, std::vector<uint32_t>& bits)
        -> bool {
    uint8_t present{};
    if (clp_s::ErrorCodeSuccess != decompressor.try_read_numeric_value(present)) {
        return false;
    }
    if (0 == present) {
        bits.clear();
        return true;
    }
    bits.resize(num_words);
    return clp_s::ErrorCodeSuccess
           == decompressor.try_read_exact_length(
                   reinterpret_cast<char*>(bits.data()),
                   num_words * sizeof(uint32_t)
           );
}

auto write_byte_set(clp_s::ZstdCompressor& compressor, RuleSignature::ByteSet const& set) -> void {
    compressor.write_numeric_value(set.bits[0]);
    compressor.write_numeric_value(set.bits[1]);
    compressor.write_numeric_value<uint8_t>(set.non_ascii ? 1 : 0);
}

[[nodiscard]] auto read_byte_set(clp_s::ZstdDecompressor& decompressor, RuleSignature::ByteSet& set)
        -> bool {
    uint8_t non_ascii{};
    if (clp_s::ErrorCodeSuccess != decompressor.try_read_numeric_value(set.bits[0])
        || clp_s::ErrorCodeSuccess != decompressor.try_read_numeric_value(set.bits[1])
        || clp_s::ErrorCodeSuccess != decompressor.try_read_numeric_value(non_ascii))
    {
        return false;
    }
    set.non_ascii = 0 != non_ascii;
    return true;
}
}  // namespace

auto RuleSignature::add_value(std::string_view value) -> void {
    auto const len{value.size()};
    if (0 == length_bits) {
        // First value: both bounds start here.
        min_value_len = len;
    } else if (len < min_value_len) {
        min_value_len = len;
    }
    if (len > max_value_len) {
        max_value_len = len;
    }
    length_bits |= uint64_t{1} << (len < cMaxExactLength ? len : cMaxExactLength);
    if (0 == len) {
        return;
    }

    first_unigrams.add(byte_at(value, 0));
    last_unigrams.add(byte_at(value, len - 1));
    for (size_t i{0}; i < len; ++i) {
        unigrams.add(byte_at(value, i));
    }
    if (len >= 2) {
        if (bigram_bits.empty()) {
            bigram_bits.assign(cNumBigramWords, 0);
            first_bigram_bits.assign(cNumBigramWords, 0);
            last_bigram_bits.assign(cNumBigramWords, 0);
        }
        set_bit(first_bigram_bits, bigram_index(byte_at(value, 0), byte_at(value, 1)));
        set_bit(last_bigram_bits, bigram_index(byte_at(value, len - 2), byte_at(value, len - 1)));
        for (size_t i{0}; i + 1 < len; ++i) {
            set_bit(bigram_bits, bigram_index(byte_at(value, i), byte_at(value, i + 1)));
        }
    }
    if (len >= 3) {
        if (trigram_bits.empty()) {
            trigram_bits.assign(cNumTrigramWords, 0);
        }
        for (size_t i{0}; i + 2 < len; ++i) {
            set_bit(trigram_bits,
                    trigram_index(byte_at(value, i), byte_at(value, i + 1), byte_at(value, i + 2)));
        }
    }
}

auto RuleSignature::kgrams_present(std::string_view span) const -> bool {
    if (span.empty()) {
        return true;
    }
    if (1 == span.size()) {
        return unigrams.contains(byte_at(span, 0));
    }
    if (2 == span.size()) {
        return bigram_present(bigram_bits, span, 0);
    }
    if (trigram_bits.empty()) {
        return false;
    }
    for (size_t i{0}; i + 2 < span.size(); ++i) {
        if (false
            == test_bit(
                    trigram_bits,
                    trigram_index(byte_at(span, i), byte_at(span, i + 1), byte_at(span, i + 2))
            ))
        {
            return false;
        }
    }
    return true;
}

auto RuleSignature::may_contain(std::string_view span) const -> bool {
    if (unbounded || span.empty()) {
        // Every string (including every indexed value) contains the empty span.
        return true;
    }
    if (span.size() > max_value_len) {
        return false;
    }
    return kgrams_present(span);
}

auto RuleSignature::may_start_with(std::string_view span) const -> bool {
    if (unbounded || span.empty()) {
        return true;
    }
    if (span.size() > max_value_len) {
        return false;
    }
    if (false == first_unigrams.contains(byte_at(span, 0))) {
        return false;
    }
    if (span.size() >= 2 && false == bigram_present(first_bigram_bits, span, 0)) {
        return false;
    }
    return kgrams_present(span);
}

auto RuleSignature::may_end_with(std::string_view span) const -> bool {
    if (unbounded || span.empty()) {
        return true;
    }
    if (span.size() > max_value_len) {
        return false;
    }
    auto const n{span.size()};
    if (false == last_unigrams.contains(byte_at(span, n - 1))) {
        return false;
    }
    if (n >= 2 && false == bigram_present(last_bigram_bits, span, n - 2)) {
        return false;
    }
    return kgrams_present(span);
}

auto RuleSignature::may_equal(std::string_view span) const -> bool {
    if (unbounded) {
        return true;
    }
    auto const n{span.size()};
    if (false == may_have_length(n)) {
        return false;
    }
    if (0 == n) {
        return true;
    }
    if (false == first_unigrams.contains(byte_at(span, 0))
        || false == last_unigrams.contains(byte_at(span, n - 1)))
    {
        return false;
    }
    if (n >= 2
        && (false == bigram_present(first_bigram_bits, span, 0)
            || false == bigram_present(last_bigram_bits, span, n - 2)))
    {
        return false;
    }
    return kgrams_present(span);
}

auto RuleSignature::compress(clp_s::ZstdCompressor& compressor) const
        -> ystdlib::error_handling::Result<void> {
    compressor.write_numeric_value<uint8_t>(unbounded ? 1 : 0);
    compressor.write_numeric_value<uint64_t>(min_value_len);
    compressor.write_numeric_value<uint64_t>(max_value_len);
    compressor.write_numeric_value<uint64_t>(length_bits);
    write_byte_set(compressor, unigrams);
    write_byte_set(compressor, first_unigrams);
    write_byte_set(compressor, last_unigrams);
    write_bitset(compressor, bigram_bits);
    write_bitset(compressor, first_bigram_bits);
    write_bitset(compressor, last_bigram_bits);
    write_bitset(compressor, trigram_bits);
    return ystdlib::error_handling::success();
}

auto RuleSignature::decompress(clp_s::ZstdDecompressor& decompressor)
        -> ystdlib::error_handling::Result<RuleSignature> {
    RuleSignature sig;
    uint8_t unbounded{};
    uint64_t min_len{};
    uint64_t max_len{};
    if (clp_s::ErrorCodeSuccess != decompressor.try_read_numeric_value(unbounded)
        || clp_s::ErrorCodeSuccess != decompressor.try_read_numeric_value(min_len)
        || clp_s::ErrorCodeSuccess != decompressor.try_read_numeric_value(max_len)
        || clp_s::ErrorCodeSuccess != decompressor.try_read_numeric_value(sig.length_bits))
    {
        return ClppErrorCode{ClppErrorCodeEnum::Failure};
    }
    sig.unbounded = 0 != unbounded;
    sig.min_value_len = min_len;
    sig.max_value_len = max_len;
    if (false == read_byte_set(decompressor, sig.unigrams)
        || false == read_byte_set(decompressor, sig.first_unigrams)
        || false == read_byte_set(decompressor, sig.last_unigrams)
        || false == read_bitset(decompressor, cNumBigramWords, sig.bigram_bits)
        || false == read_bitset(decompressor, cNumBigramWords, sig.first_bigram_bits)
        || false == read_bitset(decompressor, cNumBigramWords, sig.last_bigram_bits)
        || false == read_bitset(decompressor, cNumTrigramWords, sig.trigram_bits))
    {
        return ClppErrorCode{ClppErrorCodeEnum::Failure};
    }
    return sig;
}

auto RuleValueIndex::add_value(std::string_view rule, std::string_view value) -> void {
    auto& sig{get_or_create(rule)};
    if (sig.unbounded) {
        return;
    }
    auto seen_it{m_seen_values.find(rule)};
    if (seen_it == m_seen_values.end()) {
        seen_it = m_seen_values.emplace(std::string{rule}, ValueSet{}).first;
    }
    auto& seen{seen_it->second};
    if (seen.size() < cMaxDedupedValuesPerRule) {
        if (false == seen.emplace(value).second) {
            return;
        }
    } else if (seen.find(value) != seen.end()) {
        return;
    }
    sig.add_value(value);
}

auto RuleValueIndex::get_or_create(std::string_view rule) -> RuleSignature& {
    auto const it{m_rules.find(rule)};
    if (it != m_rules.end()) {
        return it->second;
    }
    auto [inserted, _]{m_rules.emplace(std::string{rule}, RuleSignature{})};
    return inserted->second;
}

auto RuleValueIndex::compress(clp_s::ZstdCompressor& compressor) const
        -> ystdlib::error_handling::Result<void> {
    compressor.write_numeric_value<uint64_t>(m_rules.size());
    for (auto const& [name, sig] : m_rules) {
        compressor.write_numeric_value<uint64_t>(name.size());
        compressor.write_string(name);
        YSTDLIB_ERROR_HANDLING_TRYV(sig.compress(compressor));
    }
    return ystdlib::error_handling::success();
}

auto RuleValueIndex::decompress(clp_s::ZstdDecompressor& decompressor)
        -> ystdlib::error_handling::Result<RuleValueIndex> {
    RuleValueIndex index;
    uint64_t num_rules{};
    if (clp_s::ErrorCodeSuccess != decompressor.try_read_numeric_value(num_rules)) {
        return ClppErrorCode{ClppErrorCodeEnum::Failure};
    }
    for (uint64_t i{0}; i < num_rules; ++i) {
        uint64_t name_size{};
        if (clp_s::ErrorCodeSuccess != decompressor.try_read_numeric_value(name_size)) {
            return ClppErrorCode{ClppErrorCodeEnum::Failure};
        }
        std::string name;
        name.resize(name_size);
        if (clp_s::ErrorCodeSuccess != decompressor.try_read_exact_length(name.data(), name_size)) {
            return ClppErrorCode{ClppErrorCodeEnum::Failure};
        }
        auto sig{YSTDLIB_ERROR_HANDLING_TRYX(RuleSignature::decompress(decompressor))};
        index.m_rules.emplace(std::move(name), std::move(sig));
    }
    return index;
}
}  // namespace clpp
