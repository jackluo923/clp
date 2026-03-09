#ifndef CLP_S_SEARCH_OUTPUT_HPP
#define CLP_S_SEARCH_OUTPUT_HPP

#include <cstdint>
#include <optional>
#include <utility>

#include "../ArchiveReader.hpp"
#include "../SchemaReader.hpp"
#include "../Utils.hpp"
#include "ast/Expression.hpp"
#include "ast/StringLiteral.hpp"
#include "OutputHandler.hpp"
#include "QueryRunner.hpp"
#include "SchemaMatch.hpp"

namespace clp_s::search {
/**
 * This class orchestrates the process of searching through a CLP archive,
 * filtering log messages according to a specified query, and then outputting the
 * matching messages using a provided `OutputHandler`.
 */
class Output {
public:
    Output(std::shared_ptr<SchemaMatch> const& match,
           std::shared_ptr<ast::Expression> const& expr,
           std::shared_ptr<ArchiveReader> const& archive_reader,
           std::unique_ptr<OutputHandler> output_handler,
           bool ignore_case,
           std::optional<int64_t> result_limit = std::nullopt)
            : m_query_runner(match, expr, archive_reader, ignore_case),
              m_archive_reader(archive_reader),
              m_expr(expr),
              m_match(match),
              m_output_handler(std::move(output_handler)),
              m_should_marshal_records(m_output_handler->should_marshal_records()),
              m_result_limit(result_limit) {}

    /**
     * Filters messages within the archive and outputs the filtered messages to the configured
     * OutputHandler.
     *
     * @return true if the filtering operation completed successfully; false otherwise.
     */
    auto filter() -> bool;

private:
    QueryRunner m_query_runner;
    std::shared_ptr<ArchiveReader> m_archive_reader;
    std::shared_ptr<ast::Expression> m_expr;
    std::shared_ptr<SchemaMatch> m_match;
    std::unique_ptr<OutputHandler> m_output_handler;
    bool m_should_marshal_records{true};
    std::optional<int64_t> m_result_limit;
    int64_t m_result_count{0};

    /**
     * @return Whether the result limit has been reached.
     */
    [[nodiscard]] auto is_result_limit_reached() const -> bool {
        return m_result_limit.has_value() && m_result_count >= m_result_limit.value();
    }
};
}  // namespace clp_s::search

#endif  // CLP_S_SEARCH_OUTPUT_HPP
