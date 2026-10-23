#include "Output.hpp"

#include <memory>
#include <vector>

#include <spdlog/spdlog.h>
#include <utils/profiling/ScopedProfiler.hpp>

#include "../../clp/type_utils.hpp"
#include "../SchemaTree.hpp"
#include "../Utils.hpp"
#include "ast/AndExpr.hpp"
#include "ast/ColumnDescriptor.hpp"
#include "ast/Expression.hpp"
#include "ast/FilterExpr.hpp"
#include "ast/FilterOperation.hpp"
#include "ast/Literal.hpp"
#include "ast/OrExpr.hpp"
#include "EvaluateTimestampIndex.hpp"

using clp_s::search::ast::AndExpr;
using clp_s::search::ast::ColumnDescriptor;
using clp_s::search::ast::DescriptorList;
using clp_s::search::ast::Expression;
using clp_s::search::ast::FilterExpr;
using clp_s::search::ast::FilterOperation;
using clp_s::search::ast::Literal;
using clp_s::search::ast::literal_type_bitmask_t;
using clp_s::search::ast::LiteralType;
using clp_s::search::ast::OpList;
using clp_s::search::ast::OrExpr;

#define eval(op, a, b) (((op) == FilterOperation::EQ) ? ((a) == (b)) : ((a) != (b)))

namespace clp_s::search {
bool Output::filter() {
    std::vector<int32_t> matched_schemas;

    if (auto const result{m_archive_reader->read_metadata()}; result.has_error()) {
        auto const error{result.error()};
        SPDLOG_ERROR(
                "Failed to read archive metadata: {} - {}",
                error.category().name(),
                error.message()
        );
        return false;
    }

    auto const& schema_ids = m_archive_reader->get_schema_ids();
    m_result_metrics.num_archive_schemas = schema_ids.size();
    m_result_metrics.num_clpp_interpretations = m_match->get_num_clpp_interpretations();
    auto const& clpp_stats{m_match->get_clpp_matcher_stats()};
    m_result_metrics.num_clpp_candidate_shapes = clpp_stats.num_candidate_shapes;
    m_result_metrics.num_clpp_shapes_decomposed_locally = clpp_stats.num_shapes_decomposed_locally;
    m_result_metrics.num_clpp_shapes_decomposed_by_engine
            = clpp_stats.num_shapes_decomposed_by_engine;
    for (auto schema_id : schema_ids) {
        m_result_metrics.num_archive_records
                += m_archive_reader->get_num_messages_for_schema(schema_id);
        if (m_match->schema_matched(schema_id)) {
            matched_schemas.push_back(schema_id);
            m_result_metrics.num_archive_records_matching_schemas
                    += m_archive_reader->get_num_messages_for_schema(schema_id);
        }
    }
    m_result_metrics.num_matched_schemas = matched_schemas.size();

    // Skip decompressing archive if it contains no
    // relevant schemas
    if (matched_schemas.empty()) {
        m_termination_stage = cTerminationStageSchemaMatching;
        return true;
    }

    // Skip decompressing the rest of the archive if it won't match based on the timestamp range
    // index. This check happens a second time here because some ambiguous columns may now match the
    // timestamp column after column resolution.
    EvaluateTimestampIndex timestamp_index(m_archive_reader->get_timestamp_dictionary());
    if (EvaluatedValue::False == timestamp_index.run(m_expr)) {
        m_termination_stage = cTerminationStageTimeRangeMatchingAfterColumnResolution;
        m_archive_reader->close();
        return true;
    }

    {
        PROFILE_SCOPE("search_metadata_preflight");
        if (!m_query_runner.may_match_before_dictionaries(matched_schemas)) {
            m_termination_stage = cTerminationStageMetadataPruning;
            return finish();
        }
    }
    {
        PROFILE_SCOPE("search_read_dictionaries");
        m_match->prepare_dictionaries(false);
    }
    {
        PROFILE_SCOPE("query_runner_global_init");
        m_query_runner.global_init();
    }

    std::string message;
    auto const archive_id = m_archive_reader->get_archive_id();
    bool scanned_any_ert{false};
    for (int32_t schema_id : matched_schemas) {
        EvaluatedValue schema_result;
        {
            PROFILE_SCOPE("query_runner_schema_init");
            schema_result = m_query_runner.schema_init(schema_id);
            if (EvaluatedValue::False == schema_result) {
                continue;
            }
        }
        auto const num_records{m_archive_reader->get_num_messages_for_schema(schema_id)};
        if (EvaluatedValue::True == schema_result && !m_should_marshal_records
            && !m_output_handler->should_output_metadata()
            && m_output_handler->try_write_count(num_records))
        {
            m_result_metrics.num_archive_records_matching_query += num_records;
            m_result_metrics.num_records_counted_from_metadata += num_records;
            ++m_result_metrics.num_schemas_counted_from_metadata;
            if (num_records != 0) {
                ++m_result_metrics.num_schemas_with_matches;
            }
            if (ErrorCode::ErrorCodeSuccess != m_output_handler->flush()) {
                SPDLOG_ERROR("Failed to flush metadata count output");
                return false;
            }
            continue;
        }
        // Keep the data section unopened when all ERTs can be rejected or counted from metadata.
        if (!scanned_any_ert) {
            m_archive_reader->open_packed_streams();
        }
        scanned_any_ert = true;
        ++m_result_metrics.num_schemas_scanned;
        m_result_metrics.num_messages_evaluated += num_records;

        PROFILE_SCOPE("schema_scan");
        auto& reader = [&]() -> SchemaReader& {
            PROFILE_SCOPE("read_schema_table");
            return m_archive_reader->read_schema_table(
                    schema_id,
                    m_output_handler->should_output_metadata(),
                    m_should_marshal_records,
                    m_archive_reader->has_separate_columns(schema_id)
                            ? m_query_runner.get_required_columns()
                            : std::nullopt
            );
        }();
        auto& filter = m_query_runner.prepare_filter(reader);
        if (nullptr != dynamic_cast<ColumnScan*>(&filter)) {
            ++m_result_metrics.num_column_scan_filters;
        } else {
            ++m_result_metrics.num_query_runner_filters;
        }

        bool const schema_has_match{reader.find_next_message(filter)};
        if (schema_has_match
            && (m_should_marshal_records || m_output_handler->should_output_metadata()))
        {
            m_archive_reader->prepare_schema_output(
                    m_output_handler->should_output_metadata(),
                    m_should_marshal_records
            );
        }
        if (schema_has_match && m_output_handler->should_output_metadata()) {
            epochtime_t timestamp{};
            int64_t log_event_idx{};
            do {
                reader.get_next_message_with_metadata(message, timestamp, log_event_idx);
                ++m_result_metrics.num_archive_records_matching_query;
                m_result_metrics.num_bytes_output += message.size();
                m_output_handler->write(message, timestamp, archive_id, log_event_idx);
            } while (reader.find_next_message(filter));
        } else if (schema_has_match) {
            do {
                reader.get_next_message(message);
                ++m_result_metrics.num_archive_records_matching_query;
                m_result_metrics.num_bytes_output += message.size();
                m_output_handler->write(message);
            } while (reader.find_next_message(filter));
        }
        if (schema_has_match) {
            ++m_result_metrics.num_schemas_with_matches;
        }
        auto ecode = m_output_handler->flush();
        if (ErrorCode::ErrorCodeSuccess != ecode) {
            SPDLOG_ERROR(
                    "Failed to flush output handler, error={}.",
                    clp::enum_to_underlying_type(ecode)
            );
            return false;
        }
    }
    m_termination_stage = scanned_any_ert ? cTerminationStageErtScan
                          : m_result_metrics.num_schemas_counted_from_metadata != 0
                                  ? cTerminationStageMetadataCount
                                  : cTerminationStageDictionarySearch;
    return finish();
}

auto Output::finish() -> bool {
    auto ecode = m_output_handler->finish();
    if (ErrorCode::ErrorCodeSuccess != ecode) {
        SPDLOG_ERROR(
                "Failed to flush output handler, error={}.",
                clp::enum_to_underlying_type(ecode)
        );
        return false;
    }
    return true;
}
}  // namespace clp_s::search
