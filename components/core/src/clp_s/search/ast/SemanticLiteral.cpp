#include "SemanticLiteral.hpp"

#include <optional>
#include <string>
#include <utility>

namespace clp_s::search::ast {
SemanticLiteral::SemanticLiteral(std::string query_text, std::optional<size_t> top_k)
        : m_query_text{std::move(query_text)},
          m_top_k{top_k} {}

std::shared_ptr<Literal> SemanticLiteral::create(
        std::string query_text,
        std::optional<size_t> top_k
) {
    return std::shared_ptr<Literal>(new SemanticLiteral(std::move(query_text), top_k));
}

void SemanticLiteral::print() const {
    auto& os = get_print_stream();
    os << "semantic(\"" << m_query_text << "\"";
    if (m_top_k.has_value()) {
        os << ", " << m_top_k.value();
    }
    os << ")";
}
}  // namespace clp_s::search::ast
