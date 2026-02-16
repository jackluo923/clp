#ifndef CLP_S_SEARCH_SEMANTICLITERAL_HPP
#define CLP_S_SEARCH_SEMANTICLITERAL_HPP

#include <cstddef>
#include <memory>
#include <optional>
#include <string>

#include "Literal.hpp"

namespace clp_s::search::ast {
/**
 * Literal representing a semantic search expression in the AST.
 * Stores the query text and an optional top-K override.
 */
class SemanticLiteral : public Literal {
public:
    /**
     * Creates a SemanticLiteral.
     * @param query_text The semantic query string
     * @param top_k Optional top-K override for nearest neighbor search
     * @return A shared_ptr to the newly created SemanticLiteral
     */
    static std::shared_ptr<Literal> create(std::string query_text, std::optional<size_t> top_k);

    SemanticLiteral(SemanticLiteral const&) = delete;
    SemanticLiteral& operator=(SemanticLiteral const&) = delete;

    // Methods inherited from Value
    void print() const override;

    // Methods inherited from Literal
    bool matches_type(LiteralType type) override { return type == LiteralType::ClpStringT; }

    bool matches_any(literal_type_bitmask_t mask) override {
        return mask & LiteralType::ClpStringT;
    }

    bool matches_exactly(literal_type_bitmask_t mask) override {
        return mask == LiteralType::ClpStringT;
    }

    bool as_clp_string(std::string& ret, FilterOperation op) override { return false; }

    bool as_var_string(std::string& ret, FilterOperation op) override { return false; }

    // Accessors
    [[nodiscard]] std::string const& get_query_text() const { return m_query_text; }

    [[nodiscard]] std::optional<size_t> get_top_k() const { return m_top_k; }

private:
    SemanticLiteral(std::string query_text, std::optional<size_t> top_k);

    std::string m_query_text;
    std::optional<size_t> m_top_k;
};
}  // namespace clp_s::search::ast

#endif  // CLP_S_SEARCH_SEMANTICLITERAL_HPP
