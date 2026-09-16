#ifndef CLPP_RULEVALUEINDEX_HPP
#define CLPP_RULEVALUEINDEX_HPP

#include <array>
#include <cstddef>
#include <cstdint>
#include <functional>
#include <map>
#include <string>
#include <string_view>
#include <unordered_set>
#include <vector>

#include <ystdlib/error_handling/Result.hpp>

#include <clp_s/ZstdCompressor.hpp>
#include <clp_s/ZstdDecompressor.hpp>

namespace clpp {
/**
 * The value k-gram signature of a single leaf rule (log shape placeholder).
 *
 * The signature records, across every value the rule took in an archive: the set of bytes
 * (unigrams), byte pairs (bigrams), and byte triples (trigrams) observed anywhere; the first and
 * last bytes and byte pairs; and the set of value lengths. Bitsets are allocated lazily so rules
 * that only ever see short values stay cheap.
 *
 * Soundness contract: for every value `v` passed to `RuleValueIndex::add_value`, every k-gram of
 * `v` has its bit set, its first/last k-grams have their anchored bits set, and its length is
 * recorded. Therefore a query returning `false` proves that no indexed value satisfies it. Hash
 * collisions and k-grams spread across different values only ever set more bits, so they can cause
 * over-retention but never unsound pruning.
 */
struct RuleSignature {
    // Types
    /**
     * A 128-bit set of ASCII bytes plus a flag standing for every non-ASCII byte.
     */
    struct ByteSet {
        std::array<uint64_t, 2> bits{0, 0};
        bool non_ascii{false};

        auto add(unsigned char b) -> void {
            if (b < 128) {
                bits.at(b >> 6) |= (uint64_t{1} << (b & 63));
            } else {
                non_ascii = true;
            }
        }

        [[nodiscard]] auto contains(unsigned char b) const -> bool {
            if (b < 128) {
                return 0 != ((bits.at(b >> 6) >> (b & 63)) & 1ULL);
            }
            return non_ascii;
        }
    };

    // Constants
    static constexpr size_t cNumBigramBits{1ULL << 16};
    static constexpr size_t cNumBigramWords{cNumBigramBits / 32};
    static constexpr size_t cNumTrigramBits{1ULL << 21};
    static constexpr size_t cNumTrigramWords{cNumTrigramBits / 32};
    static constexpr size_t cNumTrigramMask{cNumTrigramBits - 1};
    // Lengths at or above this are all recorded in the last bit of `length_bits`.
    static constexpr size_t cMaxExactLength{63};

    // Methods
    /**
     * Records `value` in the signature.
     * @param value
     */
    auto add_value(std::string_view value) -> void;

    /**
     * @param span
     * @return Whether some indexed value may contain `span`.
     */
    [[nodiscard]] auto may_contain(std::string_view span) const -> bool;

    /**
     * @param span
     * @return Whether some indexed value may start with `span`.
     */
    [[nodiscard]] auto may_start_with(std::string_view span) const -> bool;

    /**
     * @param span
     * @return Whether some indexed value may end with `span`.
     */
    [[nodiscard]] auto may_end_with(std::string_view span) const -> bool;

    /**
     * @param span
     * @return Whether some indexed value may equal `span`.
     */
    [[nodiscard]] auto may_equal(std::string_view span) const -> bool;

    /**
     * @return Whether some indexed value may be empty.
     */
    [[nodiscard]] auto may_be_empty() const -> bool { return may_have_length(0); }

    /**
     * @param len
     * @return Whether some indexed value may have length `len`.
     */
    [[nodiscard]] auto may_have_length(size_t len) const -> bool {
        if (unbounded) {
            return true;
        }
        if (len < min_value_len || len > max_value_len) {
            return false;
        }
        return 0 != ((length_bits >> (len < cMaxExactLength ? len : cMaxExactLength)) & 1ULL);
    }

    [[nodiscard]] auto compress(clp_s::ZstdCompressor& compressor) const
            -> ystdlib::error_handling::Result<void>;
    [[nodiscard]] static auto decompress(clp_s::ZstdDecompressor& decompressor)
            -> ystdlib::error_handling::Result<RuleSignature>;

    // Data members
    /// If true, the rule must never be pruned (its values were not all collected).
    bool unbounded{false};
    /// The length of the shortest and longest indexed value.
    size_t min_value_len{0};
    size_t max_value_len{0};
    /// Bit `L` is set iff some indexed value has length `L` (`L < cMaxExactLength`); the last bit
    /// is set iff some value has length `>= cMaxExactLength`.
    uint64_t length_bits{0};
    /// Bytes appearing anywhere, first, and last in some indexed value.
    ByteSet unigrams;
    ByteSet first_unigrams;
    ByteSet last_unigrams;
    /// 2^16-bit sets of byte pairs appearing anywhere, first, and last (exact), allocated on first
    /// use.
    std::vector<uint32_t> bigram_bits;
    std::vector<uint32_t> first_bigram_bits;
    std::vector<uint32_t> last_bigram_bits;
    /// 2^21-bit set of hashed byte triples appearing anywhere, allocated on first use.
    std::vector<uint32_t> trigram_bits;

private:
    // Methods
    /**
     * @param span
     * @return Whether every k-gram of `span` (for the applicable k) is present. Does not check the
     * length bounds.
     */
    [[nodiscard]] auto kgrams_present(std::string_view span) const -> bool;
};

/**
 * A per-rule k-gram signature index used to bound what text a log shape placeholder can contain.
 *
 * This is deliberately a *necessary-condition* oracle: each query returns `true` if some indexed
 * value of the rule might satisfy it, and may return `true` even when no value actually does. It
 * only returns `false` when it can prove that no indexed value satisfies the query. Callers must
 * treat `false` as proof and `true` as "do not prune".
 *
 * A rule absent from the index is treated as unbounded, which is the safe widening for any rule
 * whose values were never collected.
 */
class RuleValueIndex {
public:
    // Constants
    // Distinct values remembered per rule to avoid re-indexing repeats; past this the rule's
    // values are indexed on every occurrence instead (slower, same result).
    static constexpr size_t cMaxDedupedValuesPerRule{1ULL << 21};

    // Methods
    /**
     * Indexes `value` for `rule`.
     * @param rule
     * @param value
     */
    auto add_value(std::string_view rule, std::string_view value) -> void;

    /**
     * Marks `rule` as unbounded so every query on it returns `true`.
     * @param rule
     */
    auto mark_unbounded(std::string_view rule) -> void { get_or_create(rule).unbounded = true; }

    /**
     * @param rule
     * @return The signature of `rule`, or nullptr if `rule` is absent (and therefore unbounded).
     */
    [[nodiscard]] auto find(std::string_view rule) const -> RuleSignature const* {
        auto const it{m_rules.find(rule)};
        return it == m_rules.end() ? nullptr : &it->second;
    }

    [[nodiscard]] auto has_rule(std::string_view rule) const -> bool {
        return m_rules.find(rule) != m_rules.end();
    }

    [[nodiscard]] auto may_contain(std::string_view rule, std::string_view span) const -> bool {
        auto const* sig{find(rule)};
        return nullptr == sig || sig->may_contain(span);
    }

    [[nodiscard]] auto may_start_with(std::string_view rule, std::string_view span) const
            -> bool {
        auto const* sig{find(rule)};
        return nullptr == sig || sig->may_start_with(span);
    }

    [[nodiscard]] auto may_end_with(std::string_view rule, std::string_view span) const -> bool {
        auto const* sig{find(rule)};
        return nullptr == sig || sig->may_end_with(span);
    }

    [[nodiscard]] auto may_equal(std::string_view rule, std::string_view span) const -> bool {
        auto const* sig{find(rule)};
        return nullptr == sig || sig->may_equal(span);
    }

    [[nodiscard]] auto size() const -> size_t { return m_rules.size(); }

    [[nodiscard]] auto empty() const -> bool { return m_rules.empty(); }

    /**
     * Releases the per-rule sets of remembered values (only useful while indexing).
     */
    auto clear_dedup_state() -> void { m_seen_values.clear(); }

    [[nodiscard]] auto compress(clp_s::ZstdCompressor& compressor) const
            -> ystdlib::error_handling::Result<void>;
    [[nodiscard]] static auto decompress(clp_s::ZstdDecompressor& decompressor)
            -> ystdlib::error_handling::Result<RuleValueIndex>;

private:
    // Types
    /**
     * A transparent string hash so `std::string` sets can be probed with a `std::string_view`.
     */
    struct TransparentStringHash {
        using is_transparent = void;

        auto operator()(std::string_view s) const -> size_t {
            return std::hash<std::string_view>{}(s);
        }
    };

    using ValueSet = std::unordered_set<std::string, TransparentStringHash, std::equal_to<>>;

    // Methods
    [[nodiscard]] auto get_or_create(std::string_view rule) -> RuleSignature&;

    // Data members
    // `std::less<>` enables heterogeneous lookup by `std::string_view` without allocating.
    std::map<std::string, RuleSignature, std::less<>> m_rules;
    // Per rule: the distinct values already indexed, so repeated values are not re-hashed. Only
    // populated while indexing.
    std::map<std::string, ValueSet, std::less<>> m_seen_values;
};
}  // namespace clpp

#endif  // CLPP_RULEVALUEINDEX_HPP
