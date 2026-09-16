#ifndef CLP_S_SEARCH_CLPPSHAPEDECOMPOSER_HPP
#define CLP_S_SEARCH_CLPPSHAPEDECOMPOSER_HPP

#include <algorithm>
#include <cstddef>
#include <cstdint>
#include <limits>
#include <span>
#include <string>
#include <string_view>
#include <unordered_set>
#include <utility>
#include <vector>

#include <clp_s/search/ClppShapeQueryMatcher.hpp>
#include <clpp/RuleValueIndex.hpp>

namespace clp_s::search {
using rule_id_t = uint32_t;

/**
 * A clpp text shape as a sequence of parts: literal characters and placeholders (leaf-rule
 * references, `%qualified.name%`). Stored compactly as the unescaped literal text plus the part
 * index and interned rule of each placeholder, so a shape costs one byte per literal character.
 *
 * Part `i` is a placeholder iff `i` appears in `placeholder_positions`; otherwise it is the literal
 * character at `text[i - (number of placeholders before i)]`.
 */
class CompactShape {
public:
    // Methods
    [[nodiscard]] auto num_parts() const -> size_t {
        return m_text.size() + m_placeholder_positions.size();
    }

    [[nodiscard]] auto num_placeholders() const -> size_t { return m_placeholder_positions.size(); }

    /**
     * @param i A part index (`< num_parts()`).
     * @return The index into `placeholder_positions` of the first placeholder at or after part
     * `i`, or `num_placeholders()` if there is none.
     */
    [[nodiscard]] auto placeholder_at_or_after(size_t i) const -> size_t {
        return static_cast<size_t>(
                std::lower_bound(
                        m_placeholder_positions.begin(),
                        m_placeholder_positions.end(),
                        static_cast<uint32_t>(i)
                )
                - m_placeholder_positions.begin()
        );
    }

    /**
     * @param p An index into the placeholder list (`< num_placeholders()`).
     */
    [[nodiscard]] auto placeholder_position(size_t p) const -> size_t {
        return m_placeholder_positions[p];
    }

    [[nodiscard]] auto placeholder_rule(size_t p) const -> rule_id_t {
        return m_placeholder_rules[p];
    }

    /**
     * @param i A part index.
     * @param p `placeholder_at_or_after(i)`.
     * @return Whether part `i` is a placeholder.
     */
    [[nodiscard]] auto is_placeholder(size_t i, size_t p) const -> bool {
        return p < m_placeholder_positions.size() && m_placeholder_positions[p] == i;
    }

    /**
     * @param i A literal part index.
     * @param p `placeholder_at_or_after(i)` (the number of placeholders before `i`).
     * @return The literal character of part `i`.
     */
    [[nodiscard]] auto literal(size_t i, size_t p) const -> char { return m_text[i - p]; }

    [[nodiscard]] auto text() const -> std::string_view { return m_text; }

    auto append_literal(char c) -> void { m_text.push_back(c); }

    auto append_placeholder(rule_id_t rule_id) -> void {
        m_placeholder_positions.push_back(static_cast<uint32_t>(num_parts()));
        m_placeholder_rules.push_back(rule_id);
    }

    auto clear() -> void {
        m_text.clear();
        m_placeholder_positions.clear();
        m_placeholder_rules.clear();
    }

    auto shrink_to_fit() -> void {
        m_text.shrink_to_fit();
        m_placeholder_positions.shrink_to_fit();
        m_placeholder_rules.shrink_to_fit();
    }

private:
    // Data members
    std::string m_text;
    std::vector<uint32_t> m_placeholder_positions;
    std::vector<rule_id_t> m_placeholder_rules;
};

/**
 * Builds the compact form of a clpp text shape, interning each placeholder's rule name through
 * `intern`.
 *
 * Mirrors `build_shape_skeleton`'s tokenization exactly: a `%` opens a placeholder, the next `%`
 * closes it, and a `%%` pair is a literal `%`. `out` is cleared first.
 * @param shape
 * @param intern A `std::string_view -> rule_id_t` callable.
 * @param out
 * @return false if the shape is malformed (it ends inside a placeholder).
 */
template <typename RuleInterner>
[[nodiscard]] auto
build_compact_shape(std::string_view shape, RuleInterner&& intern, CompactShape& out) -> bool {
    enum class State : uint8_t {
        Text,
        Rule,
    };
    out.clear();
    State state{State::Text};
    size_t literal_start{0};
    size_t rule_start{0};
    for (size_t i{0}; i < shape.size(); ++i) {
        auto const c{shape[i]};
        if (State::Text == state) {
            if ('%' == c) {
                for (size_t j{literal_start}; j < i; ++j) {
                    out.append_literal(shape[j]);
                }
                state = State::Rule;
                rule_start = i + 1;
            }
            continue;
        }
        if ('%' == c) {
            if (rule_start == i) {
                // `%%` is an escaped literal percent.
                out.append_literal('%');
            } else {
                out.append_placeholder(intern(shape.substr(rule_start, i - rule_start)));
            }
            state = State::Text;
            literal_start = i + 1;
        }
    }
    if (State::Rule == state) {
        return false;
    }
    for (size_t j{literal_start}; j < shape.size(); ++j) {
        out.append_literal(shape[j]);
    }
    out.shrink_to_fit();
    return true;
}

/**
 * Normalizes query tokens for decomposition: collapses runs of `*` into one and appends a trailing
 * `*`, since (like the engine) the query is matched against a prefix of the message.
 * @param tokens
 */
inline auto normalize_query_tokens(std::vector<QueryToken>& tokens) -> void {
    std::vector<QueryToken> out;
    out.reserve(tokens.size() + 1);
    for (auto const& token : tokens) {
        if (QueryToken::Type::AnyMany == token.type && false == out.empty()
            && QueryToken::Type::AnyMany == out.back().type)
        {
            continue;
        }
        out.push_back(token);
    }
    if (out.empty() || QueryToken::Type::AnyMany != out.back().type) {
        out.push_back({QueryToken::Type::AnyMany, '\0'});
    }
    tokens = std::move(out);
}

/**
 * Renders a piece of query tokens as a wildcard string, escaping literal `*`, `?`, and `\` so the
 * result can be re-parsed as a wildcard query.
 * @param piece
 * @return The rendered wildcard string.
 */
inline auto render_query_piece(std::span<QueryToken const> piece) -> std::string {
    std::string out;
    out.reserve(piece.size());
    for (auto const& token : piece) {
        switch (token.type) {
            case QueryToken::Type::AnyMany:
                out.push_back('*');
                break;
            case QueryToken::Type::AnyOne:
                out.push_back('?');
                break;
            case QueryToken::Type::Literal:
                if ('*' == token.literal || '?' == token.literal || '\\' == token.literal) {
                    out.push_back('\\');
                }
                out.push_back(token.literal);
                break;
        }
    }
    return out;
}

/**
 * Returns whether some value recorded in `signature` may match the wildcard `piece`.
 *
 * This is a sound necessary condition built from the signature's k-gram and anchoring queries:
 * every literal run of the piece must be containable, a run at the piece's start (end) must be a
 * possible value start (end), the value must be long enough to host every literal and `?`, and a
 * piece with no `*` fixes the value's length exactly. A `false` result proves no recorded value
 * matches.
 * @param signature nullptr means the rule is unbounded (never pruned).
 * @param piece
 * @return Whether some value may match.
 */
[[nodiscard]] inline auto
piece_may_match(clpp::RuleSignature const* signature, std::span<QueryToken const> piece) -> bool {
    if (nullptr == signature || signature->unbounded) {
        return true;
    }
    if (piece.empty()) {
        return signature->may_be_empty();
    }

    size_t min_len{0};
    size_t num_literals{0};
    bool has_star{false};
    for (auto const& token : piece) {
        if (QueryToken::Type::AnyMany == token.type) {
            has_star = true;
        } else {
            ++min_len;
            if (QueryToken::Type::Literal == token.type) {
                ++num_literals;
            }
        }
    }
    if (min_len > signature->max_value_len) {
        return false;
    }
    if (false == has_star && false == signature->may_have_length(min_len)) {
        return false;
    }

    // Walk the literal runs.
    std::string run;
    if (num_literals == piece.size()) {
        // No wildcard at all: the value must equal the piece.
        run.reserve(piece.size());
        for (auto const& token : piece) {
            run.push_back(token.literal);
        }
        return signature->may_equal(run);
    }

    size_t run_start{0};
    for (size_t i{0}; i <= piece.size(); ++i) {
        bool const at_end{i == piece.size()};
        if (false == at_end && QueryToken::Type::Literal == piece[i].type) {
            if (run.empty()) {
                run_start = i;
            }
            run.push_back(piece[i].literal);
            continue;
        }
        if (run.empty()) {
            continue;
        }
        if (0 == run_start) {
            if (false == signature->may_start_with(run)) {
                return false;
            }
        } else if (at_end) {
            if (false == signature->may_end_with(run)) {
                return false;
            }
        } else if (false == signature->may_contain(run)) {
            return false;
        }
        run.clear();
    }
    return true;
}

/**
 * One leaf query of a decomposed shape: the placeholder's rule and the wildcard query its value
 * must satisfy.
 */
struct ShapeLeaf {
    rule_id_t rule_id{0};
    std::string query;
};

/**
 * One interpretation of a query against a shape. `segments` is the engine-style shape query (each
 * segment is literal text with wildcards, or a placeholder); `leaves` is the leaf query per
 * constrained placeholder, in shape order. A placeholder covered entirely by a `*` is unconstrained
 * and contributes no leaf (its segment is folded into the surrounding `*`).
 */
struct ShapeInterpretation {
    struct Segment {
        bool is_placeholder{false};
        // The literal text (possibly with wildcards), or the placeholder's rule ID.
        std::string text;
        rule_id_t rule_id{0};
    };

    std::vector<Segment> segments;
    std::vector<ShapeLeaf> leaves;
};

enum class DecomposeStatus : uint8_t {
    // No alignment of the query against the shape is certified by the value index.
    NoMatch,
    // Every certified interpretation was emitted.
    Complete,
    // The interpretation cap was hit; the emitted list is incomplete.
    Capped,
};

/**
 * Decomposes a wildcard query against clpp text shapes, certifying each placeholder's piece of the
 * query with the rule value index.
 *
 * The query tokens are aligned against the shape as the engine does: literal parts must match the
 * query character-for-character (`?` matches any one character, `*` any run), and each placeholder
 * absorbs a contiguous piece of the query which becomes the leaf query for its value. A `*` may be
 * shared between a placeholder and its neighbours, in which case the leaf is open on that side. The
 * enumerator only follows alignments whose every piece the value index cannot rule out, so it emits
 * exactly the interpretations that some recorded value could satisfy (plus, possibly, some that
 * none does -- the index is a necessary condition).
 *
 * A leafless interpretation (every placeholder unconstrained) matches every message of the shape,
 * so when one exists it is the only interpretation emitted.
 */
class ShapeDecomposer {
public:
    // Constructors
    /**
     * @param tokens Normalized query tokens (see `normalize_query_tokens`); must outlive `this`.
     * @param signatures Per rule ID: the rule's value signature, or nullptr if unbounded; must
     * outlive `this`.
     */
    ShapeDecomposer(
            std::span<QueryToken const> tokens,
            std::span<clpp::RuleSignature const* const> signatures
    )
            : m_tokens{tokens},
              m_signatures{signatures},
              m_num_cols{tokens.size() + 1},
              m_piece_memo(signatures.size() * m_num_cols * m_num_cols, cUnknown) {}

    // Methods
    /**
     * Decomposes the query against `shape`.
     * @param shape
     * @param max_interpretations
     * @param out Cleared first.
     * @return The status of the decomposition.
     */
    [[nodiscard]] auto decompose(
            CompactShape const& shape,
            size_t max_interpretations,
            std::vector<ShapeInterpretation>& out
    ) -> DecomposeStatus;

    /**
     * @return Whether the last `decompose` call emitted a leaf on an unbounded rule (one the index
     * could not certify), in which case the caller may prefer the engine's regex-based
     * decomposition for that shape.
     */
    [[nodiscard]] auto used_unbounded_rule() const -> bool { return m_used_unbounded; }

private:
    // Constants
    // Memo cell states. `cUnknown` is zero so freshly value-initialized tables need no fill.
    static constexpr uint8_t cUnknown{0};
    static constexpr uint8_t cFalse{1};
    static constexpr uint8_t cTrue{2};
    // The per-shape reachability tables are reused across `decompose` calls without clearing:
    // each cell stores `(generation << cStateBits) | state`, and is only trusted when its
    // generation is the current call's.
    static constexpr unsigned cStateBits{2};
    static constexpr uint32_t cStateMask{(uint32_t{1} << cStateBits) - 1};

    // Methods
    /**
     * @return Whether some value of `rule_id` may match the piece `[begin, end)` of the query.
     */
    [[nodiscard]] auto piece_certified(rule_id_t rule_id, size_t begin, size_t end) -> bool;

    /**
     * @return The index of the first part at or after `i` that is not a literal (or the number of
     * parts).
     */
    [[nodiscard]] auto literal_run_end(size_t i) const -> size_t;

    /**
     * @return Whether an alignment can be completed from part `i` and token `j`.
     */
    [[nodiscard]] auto reach(size_t i, size_t j) -> bool;

    /**
     * @return Whether an alignment leaving every remaining placeholder unconstrained can be
     * completed from part `i` and token `j`.
     */
    [[nodiscard]] auto reach_leafless(size_t i, size_t j) -> bool;

    /**
     * Enumerates the certified alignments from part `i` and token `j`.
     * @return false when the interpretation cap was hit.
     */
    auto enumerate(size_t i, size_t j) -> bool;

    /**
     * Emits the current alignment as an interpretation, unless one with the same leaves was already
     * emitted.
     * @return false when the interpretation cap was hit.
     */
    auto emit() -> bool;

    auto append_text(char c) -> void;
    auto append_star() -> void;
    auto push_placeholder_segment(rule_id_t rule_id) -> void;
    auto pop_segment(size_t prior_num_segments, size_t prior_text_size) -> void;

    [[nodiscard]] auto leaves_key(size_t i, size_t j) const -> std::string;

    /**
     * @return The state stored in a reachability cell for the current call, or `cUnknown`.
     */
    [[nodiscard]] auto memo_state(uint32_t cell) const -> uint8_t {
        return (cell >> cStateBits) == m_generation ? static_cast<uint8_t>(cell & cStateMask)
                                                    : cUnknown;
    }

    [[nodiscard]] auto memo_cell(bool value) const -> uint32_t {
        return (m_generation << cStateBits) | (value ? cTrue : cFalse);
    }

    /**
     * Starts a new generation, making every reachability cell unknown again.
     */
    auto reset_reach_memos(size_t num_cells) -> void;

    // Data members
    std::span<QueryToken const> m_tokens;
    std::span<clpp::RuleSignature const* const> m_signatures;
    size_t m_num_cols;
    std::vector<uint8_t> m_piece_memo;

    // Per `decompose` call.
    CompactShape const* m_shape{nullptr};
    uint32_t m_generation{0};
    std::vector<uint32_t> m_reach_memo;
    std::vector<uint32_t> m_reach_leafless_memo;
    std::vector<ShapeInterpretation::Segment> m_segments;
    std::vector<ShapeLeaf> m_leaves;
    std::unordered_set<std::string> m_visited;
    std::unordered_set<std::string> m_emitted;
    std::vector<ShapeInterpretation>* m_out{nullptr};
    size_t m_max_interpretations{0};
    bool m_used_unbounded{false};
};

inline auto ShapeDecomposer::piece_certified(rule_id_t rule_id, size_t begin, size_t end) -> bool {
    if (rule_id >= m_signatures.size()) {
        // Unknown rule: unbounded.
        return true;
    }
    auto const idx{((static_cast<size_t>(rule_id) * m_num_cols) + begin) * m_num_cols + end};
    auto& memo{m_piece_memo[idx]};
    if (cUnknown == memo) {
        memo = piece_may_match(m_signatures[rule_id], m_tokens.subspan(begin, end - begin))
                       ? cTrue
                       : cFalse;
    }
    return cTrue == memo;
}

inline auto ShapeDecomposer::literal_run_end(size_t i) const -> size_t {
    auto const p{m_shape->placeholder_at_or_after(i)};
    return p < m_shape->num_placeholders() ? m_shape->placeholder_position(p)
                                           : m_shape->num_parts();
}

inline auto ShapeDecomposer::reach(size_t i, size_t j) -> bool {
    size_t const last{m_tokens.size() - 1};
    if (j == last) {
        // Only the trailing `*` remains; it covers the rest of the message.
        return true;
    }
    auto const& shape{*m_shape};
    if (i >= shape.num_parts()) {
        return false;
    }
    auto& memo{m_reach_memo[(i * m_num_cols) + j]};
    if (auto const state{memo_state(memo)}; cUnknown != state) {
        return cTrue == state;
    }
    bool result{false};
    auto const p{shape.placeholder_at_or_after(i)};
    if (false == shape.is_placeholder(i, p)) {
        auto const& token{m_tokens[j]};
        if (QueryToken::Type::AnyMany == token.type) {
            // The `*` may end at any position of this literal run, or carry into the part after
            // it. Fill the whole run's column iteratively (a suffix OR) rather than recursing
            // once per character: shapes can be tens of thousands of characters long.
            size_t const run_end{literal_run_end(i)};
            bool carry{reach(run_end, j)};
            // `**` is collapsed, so the next token is a literal or `?`. Once the suffix OR is
            // true every earlier cell is too, and a literal can only be matched where the run
            // holds that character, so most positions need no recursive probe.
            auto const& next{m_tokens[j + 1]};
            bool const next_is_literal{QueryToken::Type::Literal == next.type};
            for (size_t pos{run_end}; pos-- > i;) {
                if (false == carry
                    && (false == next_is_literal || next.literal == shape.literal(pos, p)))
                {
                    carry = reach(pos, j + 1);
                }
                m_reach_memo[(pos * m_num_cols) + j] = memo_cell(carry);
            }
            return carry;
        }
        if (QueryToken::Type::AnyOne == token.type || token.literal == shape.literal(i, p)) {
            result = reach(i + 1, j + 1);
        }
    } else {
        auto const rule_id{shape.placeholder_rule(p)};
        for (size_t k{j}; k <= last; ++k) {
            if (QueryToken::Type::AnyMany == m_tokens[k].type) {
                // Open on the right: the `*` continues into the following parts.
                if (piece_certified(rule_id, j, k + 1) && reach(i + 1, k)) {
                    result = true;
                    break;
                }
            } else if (k == j || QueryToken::Type::AnyMany != m_tokens[k - 1].type) {
                // Closed on the right (a piece ending right before a `*` is subsumed by the open
                // piece that includes it).
                if (piece_certified(rule_id, j, k) && reach(i + 1, k)) {
                    result = true;
                    break;
                }
            }
        }
    }
    memo = memo_cell(result);
    return result;
}

inline auto ShapeDecomposer::reach_leafless(size_t i, size_t j) -> bool {
    size_t const last{m_tokens.size() - 1};
    if (j == last) {
        return true;
    }
    auto const& shape{*m_shape};
    if (i >= shape.num_parts()) {
        return false;
    }
    auto& memo{m_reach_leafless_memo[(i * m_num_cols) + j]};
    if (auto const state{memo_state(memo)}; cUnknown != state) {
        return cTrue == state;
    }
    bool result{false};
    auto const p{shape.placeholder_at_or_after(i)};
    auto const& token{m_tokens[j]};
    if (false == shape.is_placeholder(i, p)) {
        if (QueryToken::Type::AnyMany == token.type) {
            size_t const run_end{literal_run_end(i)};
            bool carry{reach_leafless(run_end, j)};
            auto const& next{m_tokens[j + 1]};
            bool const next_is_literal{QueryToken::Type::Literal == next.type};
            for (size_t pos{run_end}; pos-- > i;) {
                if (false == carry
                    && (false == next_is_literal || next.literal == shape.literal(pos, p)))
                {
                    carry = reach_leafless(pos, j + 1);
                }
                m_reach_leafless_memo[(pos * m_num_cols) + j] = memo_cell(carry);
            }
            return carry;
        }
        if (QueryToken::Type::AnyOne == token.type || token.literal == shape.literal(i, p)) {
            result = reach_leafless(i + 1, j + 1);
        }
    } else if (QueryToken::Type::AnyMany == token.type) {
        // The placeholder is absorbed by the `*`.
        result = reach_leafless(i + 1, j);
    }
    memo = memo_cell(result);
    return result;
}

inline auto ShapeDecomposer::append_text(char c) -> void {
    if (m_segments.empty() || m_segments.back().is_placeholder) {
        m_segments.push_back({false, {}, 0});
    }
    m_segments.back().text.push_back(c);
}

inline auto ShapeDecomposer::append_star() -> void {
    if (false == m_segments.empty() && false == m_segments.back().is_placeholder
        && false == m_segments.back().text.empty() && '*' == m_segments.back().text.back())
    {
        return;
    }
    append_text('*');
}

inline auto ShapeDecomposer::push_placeholder_segment(rule_id_t rule_id) -> void {
    m_segments.push_back({true, {}, rule_id});
}

inline auto ShapeDecomposer::pop_segment(size_t prior_num_segments, size_t prior_text_size)
        -> void {
    m_segments.resize(prior_num_segments);
    if (false == m_segments.empty() && false == m_segments.back().is_placeholder) {
        m_segments.back().text.resize(prior_text_size);
    }
}

inline auto ShapeDecomposer::leaves_key(size_t i, size_t j) const -> std::string {
    std::string key;
    key.append(reinterpret_cast<char const*>(&i), sizeof(i));
    key.append(reinterpret_cast<char const*>(&j), sizeof(j));
    for (auto const& leaf : m_leaves) {
        key.append(reinterpret_cast<char const*>(&leaf.rule_id), sizeof(leaf.rule_id));
        key.append(leaf.query);
        key.push_back('\0');
    }
    return key;
}

inline auto ShapeDecomposer::emit() -> bool {
    // Interpretations with the same leaves build the same downstream filter, so keep one.
    if (false == m_emitted.emplace(leaves_key(0, 0)).second) {
        return true;
    }
    if (m_out->size() >= m_max_interpretations) {
        return false;
    }
    for (auto const& leaf : m_leaves) {
        if (leaf.rule_id >= m_signatures.size() || nullptr == m_signatures[leaf.rule_id]
            || m_signatures[leaf.rule_id]->unbounded)
        {
            m_used_unbounded = true;
        }
    }
    m_out->push_back({m_segments, m_leaves});
    return true;
}

inline auto ShapeDecomposer::enumerate(size_t i, size_t j) -> bool {
    size_t const last{m_tokens.size() - 1};
    if (j == last) {
        append_star();
        return emit();
    }
    auto const& shape{*m_shape};
    if (i >= shape.num_parts()) {
        return true;
    }
    if (false == m_visited.emplace(leaves_key(i, j)).second) {
        // Already explored with these leaves; any interpretation from here was already emitted.
        return true;
    }
    auto const p{shape.placeholder_at_or_after(i)};
    // Snapshot the segment state so each branch can be undone.
    size_t const num_segments{m_segments.size()};
    size_t const text_size{
            (false == m_segments.empty() && false == m_segments.back().is_placeholder)
                    ? m_segments.back().text.size()
                    : 0
    };
    size_t const num_leaves{m_leaves.size()};
    auto const restore = [&]() -> void {
        pop_segment(num_segments, text_size);
        m_leaves.resize(num_leaves);
    };

    if (false == shape.is_placeholder(i, p)) {
        auto const& token{m_tokens[j]};
        if (QueryToken::Type::AnyMany == token.type) {
            // The `*` ends at some position of this literal run (and the next token resumes
            // there), or carries into the part after the run.
            size_t const run_end{literal_run_end(i)};
            auto const& next{m_tokens[j + 1]};
            bool const next_is_literal{QueryToken::Type::Literal == next.type};
            for (size_t pos{i}; pos < run_end; ++pos) {
                if ((next_is_literal && next.literal != shape.literal(pos, p))
                    || false == reach(pos, j + 1))
                {
                    continue;
                }
                append_star();
                if (false == enumerate(pos, j + 1)) {
                    return false;
                }
                restore();
            }
            if (reach(run_end, j)) {
                append_star();
                if (false == enumerate(run_end, j)) {
                    return false;
                }
                restore();
            }
        } else if (QueryToken::Type::AnyOne == token.type
                   || token.literal == shape.literal(i, p))
        {
            append_text(shape.literal(i, p));
            if (reach(i + 1, j + 1) && false == enumerate(i + 1, j + 1)) {
                return false;
            }
            restore();
        }
        return true;
    }

    auto const rule_id{shape.placeholder_rule(p)};
    for (size_t k{j}; k <= last; ++k) {
        bool const open_right{QueryToken::Type::AnyMany == m_tokens[k].type};
        if (false == open_right && k != j && QueryToken::Type::AnyMany == m_tokens[k - 1].type) {
            continue;
        }
        size_t const end{open_right ? k + 1 : k};
        if (false == piece_certified(rule_id, j, end) || false == reach(i + 1, k)) {
            continue;
        }
        auto const piece{m_tokens.subspan(j, end - j)};
        if (1 == piece.size() && QueryToken::Type::AnyMany == piece.front().type) {
            // Unconstrained: fold the placeholder into the `*`.
            append_star();
        } else {
            m_leaves.push_back({rule_id, render_query_piece(piece)});
            push_placeholder_segment(rule_id);
        }
        if (false == enumerate(i + 1, k)) {
            return false;
        }
        restore();
    }
    return true;
}

inline auto ShapeDecomposer::decompose(
        CompactShape const& shape,
        size_t max_interpretations,
        std::vector<ShapeInterpretation>& out
) -> DecomposeStatus {
    out.clear();
    m_shape = &shape;
    m_out = &out;
    m_max_interpretations = max_interpretations;
    m_used_unbounded = false;
    reset_reach_memos((shape.num_parts() + 1) * m_num_cols);
    m_segments.clear();
    m_leaves.clear();
    m_visited.clear();
    m_emitted.clear();

    if (false == reach(0, 0)) {
        return DecomposeStatus::NoMatch;
    }

    if (reach_leafless(0, 0)) {
        // Every message of the shape matches; no leaf constraint is needed.
        ShapeInterpretation interpretation;
        interpretation.segments.push_back({false, "*", 0});
        out.push_back(std::move(interpretation));
        return DecomposeStatus::Complete;
    }

    return enumerate(0, 0) ? DecomposeStatus::Complete : DecomposeStatus::Capped;
}

inline auto ShapeDecomposer::reset_reach_memos(size_t num_cells) -> void {
    // Shapes can be tens of thousands of characters long while the search touches a sliver of the
    // table, so the tables are neither reallocated nor cleared per shape: bumping the generation
    // invalidates every cell, and the tables only grow.
    constexpr uint32_t cMaxGeneration{std::numeric_limits<uint32_t>::max() >> cStateBits};
    if (m_generation >= cMaxGeneration) {
        std::ranges::fill(m_reach_memo, 0);
        std::ranges::fill(m_reach_leafless_memo, 0);
        m_generation = 0;
    }
    ++m_generation;
    if (m_reach_memo.size() < num_cells) {
        m_reach_memo.resize(num_cells);
        m_reach_leafless_memo.resize(num_cells);
    }
}
}  // namespace clp_s::search

#endif  // CLP_S_SEARCH_CLPPSHAPEDECOMPOSER_HPP
