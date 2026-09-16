#ifndef CLP_S_SEARCH_CLPPSHAPEQUERYMATCHER_HPP
#define CLP_S_SEARCH_CLPPSHAPEQUERYMATCHER_HPP

#include <cstddef>
#include <cstdint>
#include <deque>
#include <string_view>
#include <utility>
#include <vector>

namespace clp_s::search {
/**
 * A token of a clpp text shape.
 *
 * `Literal` is an exact character. `Placeholder` is a leaf-rule reference (`%qualified.name%`) and
 * stands for an arbitrary (possibly empty) string.
 */
struct ShapeToken {
    enum class Type : uint8_t {
        Literal,
        Placeholder,
    };

    Type type{Type::Literal};
    char literal{'\0'};
};

/**
 * A token of a wildcard query, mirroring log-surgeon's `SymbolicChar`.
 *
 * `Literal` is an exact character (an escaped `*`, `?`, or `\` is also a literal). `AnyOne` is `?`
 * and matches exactly one character. `AnyMany` is `*` and matches zero or more characters.
 */
struct QueryToken {
    enum class Type : uint8_t {
        Literal,
        AnyOne,
        AnyMany,
    };

    Type type{Type::Literal};
    char literal{'\0'};
};

/**
 * Builds the token sequence for a clpp text shape.
 *
 * Mirrors log-surgeon's `ParsingSpec::automata_for_shape` tokenization exactly: a `%` opens a
 * placeholder, the next `%` closes it, and a `%%` pair is a literal `%`. `out` is cleared first.
 * @param shape
 * @param out
 * @return false if the shape is malformed (it ends inside a placeholder), in which case the engine
 * has no well-defined language for it and callers must treat the shape as an unconditional
 * candidate.
 */
[[nodiscard]] inline auto
build_shape_skeleton(std::string_view shape, std::vector<ShapeToken>& out) -> bool {
    enum class State : uint8_t {
        Text,
        Rule,
    };
    out.clear();
    State state{State::Text};
    bool rule_name_empty{true};
    size_t literal_start{0};
    for (size_t i{0}; i < shape.size(); ++i) {
        auto const c{shape.at(i)};
        if (State::Text == state) {
            if ('%' == c) {
                for (size_t j{literal_start}; j < i; ++j) {
                    out.push_back({ShapeToken::Type::Literal, shape.at(j)});
                }
                state = State::Rule;
                rule_name_empty = true;
            }
            continue;
        }
        if ('%' == c) {
            if (rule_name_empty) {
                // `%%` is an escaped literal percent.
                out.push_back({ShapeToken::Type::Literal, '%'});
            } else {
                out.push_back({ShapeToken::Type::Placeholder, '\0'});
            }
            state = State::Text;
            literal_start = i + 1;
        } else {
            rule_name_empty = false;
        }
    }
    if (State::Rule == state) {
        return false;
    }
    for (size_t j{literal_start}; j < shape.size(); ++j) {
        out.push_back({ShapeToken::Type::Literal, shape.at(j)});
    }
    return true;
}

/**
 * Parses a wildcard query into tokens, mirroring log-surgeon's `SearchString::parse`.
 * @param query
 * @param out Cleared first.
 * @return false if the query ends with a dangling escape character.
 */
[[nodiscard]] inline auto
parse_shape_query(std::string_view query, std::vector<QueryToken>& out) -> bool {
    out.clear();
    bool last_was_escape{false};
    for (auto const c : query) {
        if ('*' == c || '?' == c || '\\' == c) {
            if (last_was_escape) {
                out.push_back({QueryToken::Type::Literal, c});
            } else if ('*' == c) {
                out.push_back({QueryToken::Type::AnyMany, '\0'});
            } else if ('?' == c) {
                out.push_back({QueryToken::Type::AnyOne, '\0'});
            } else {
                last_was_escape = true;
                continue;
            }
        } else if (last_was_escape) {
            return false;
        } else {
            out.push_back({QueryToken::Type::Literal, c});
        }
        last_was_escape = false;
    }
    if (last_was_escape) {
        return false;
    }
    return true;
}

/**
 * Returns whether `query` can match some message belonging to `skeleton`'s shape language.
 *
 * This is a sound necessary condition for the engine's per-shape query intersection: skeleton
 * placeholders stand for arbitrary text (a superset of the leaf rule's language) and, like the
 * engine, the query is matched against a prefix of the message, so a `false` result proves the shape
 * cannot produce an interpretation for `query`. A `true` result only means the shape must still be
 * checked by the engine.
 *
 * Matches log-surgeon's `search_by_log_shapes` anchoring, which was verified empirically: `ab` and
 * `a?c` match the shape `abc`, while `bc` and `b` do not. The engine drops one trailing `*`; the
 * token sequence here keeps it, which accepts the same language.
 *
 * Implemented as a reachability search over pairs of token positions in the product of the two token
 * sequences; no per-shape automaton is built.
 * @param skeleton
 * @param query
 * @return Whether the query may match.
 */
[[nodiscard]] inline auto
shape_may_match(std::vector<ShapeToken> const& skeleton, std::vector<QueryToken> const& query)
        -> bool {
    size_t const num_shape{skeleton.size()};
    size_t const num_query{query.size()};
    if (0 == num_query) {
        // An empty query matches every shape (the engine rejects empty queries before reaching
        // here, so this is only a safe over-approximation).
        return true;
    }
    auto const query_at = [&](size_t j) -> QueryToken const& { return query.at(j); };
    // `j` may reach `num_query` (the accept state), so the table needs a column for it -- including
    // when `i == num_shape`.
    size_t const num_query_cols{num_query + 1};
    std::vector<uint8_t> visited((num_shape + 1) * num_query_cols, 0);
    auto const index = [&](size_t i, size_t j) -> size_t { return (i * num_query_cols) + j; };
    std::deque<std::pair<size_t, size_t>> worklist;
    auto const push = [&](size_t i, size_t j) -> void {
        auto const idx{index(i, j)};
        if (0 == visited.at(idx)) {
            visited.at(idx) = 1;
            worklist.emplace_back(i, j);
        }
    };
    push(0, 0);
    while (false == worklist.empty()) {
        auto const [i, j] = worklist.front();
        worklist.pop_front();
        if (num_query == j) {
            // The whole query is consumed as a prefix; the remaining shape tokens can match the
            // rest of the message.
            return true;
        }
        auto const shape_star{num_shape > i
                              && ShapeToken::Type::Placeholder == skeleton.at(i).type};
        auto const query_star{QueryToken::Type::AnyMany == query_at(j).type};
        // Skip a wildcard without consuming a character.
        if (shape_star) {
            push(i + 1, j);
        }
        if (query_star) {
            push(i, j + 1);
        }
        if (num_shape > i) {
            if (shape_star && false == query_star) {
                // The placeholder absorbs the character the query token consumes.
                push(i, j + 1);
            } else if (query_star && false == shape_star) {
                // The query wildcard absorbs the character the shape token consumes.
                push(i + 1, j);
            } else if (false == shape_star && false == query_star) {
                auto const& shape_token{skeleton.at(i)};
                auto const& query_token{query_at(j)};
                if (QueryToken::Type::Literal == query_token.type) {
                    if (shape_token.literal == query_token.literal) {
                        push(i + 1, j + 1);
                    }
                } else {
                    // AnyOne matches the shape's literal character.
                    push(i + 1, j + 1);
                }
            }
        }
    }
    return false;
}
}  // namespace clp_s::search

#endif  // CLP_S_SEARCH_CLPPSHAPEQUERYMATCHER_HPP
