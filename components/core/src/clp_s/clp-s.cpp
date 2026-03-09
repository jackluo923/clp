#include <cctype>
#include <cstdint>
#include <cstdlib>
#include <exception>
#include <filesystem>
#include <iostream>
#include <memory>
#include <optional>
#include <sstream>
#include <string>
#include <string_view>
#include <system_error>
#include <unordered_map>
#include <utility>
#include <vector>

#include <mongocxx/instance.hpp>
#include <nlohmann/json.hpp>
#include <spdlog/sinks/stdout_sinks.h>
#include <spdlog/spdlog.h>

#if CLP_BUILD_CLP_S_ENABLE_CURL
    #include "../clp/CurlGlobalInstance.hpp"
#endif
#include "../clp/ir/constants.hpp"
#include "../clp/streaming_archive/ArchiveMetadata.hpp"
#include "../reducer/network_utils.hpp"
#include "CommandLineArguments.hpp"
#include "Defs.hpp"
#include "JsonConstructor.hpp"
#include "JsonParser.hpp"
#include "kv_ir_search.hpp"
#include "OutputHandlerImpl.hpp"
#include "search/AddTimestampConditions.hpp"
#include "search/ast/ConvertToExists.hpp"
#include "search/ast/EmptyExpr.hpp"
#include "search/ast/Expression.hpp"
#include "search/ast/NarrowTypes.hpp"
#include "search/ast/OrOfAndForm.hpp"
#include "search/ast/SearchUtils.hpp"
#include "search/ast/SetTimestampLiteralPrecision.hpp"
#include "search/ast/TimestampLiteral.hpp"
#include "search/EvaluateRangeIndexFilters.hpp"
#include "search/EvaluateTimestampIndex.hpp"
#include "search/kql/kql.hpp"
#include "search/LocalAggregateOutputHandler.hpp"
#include "search/Output.hpp"
#include "search/OutputHandler.hpp"
#include "search/Projection.hpp"
#include "search/RenamingOutputHandler.hpp"
#include "search/SchemaMatch.hpp"
#include "search/sql/sql.hpp"
#include "SingleFileArchiveDefs.hpp"

using namespace clp_s::search;
using clp_s::cArchiveFormatDevelopmentVersionFlag;
using clp_s::cEpochTimeMax;
using clp_s::cEpochTimeMin;
using clp_s::CommandLineArguments;
using clp_s::KvIrSearchError;
using clp_s::KvIrSearchErrorEnum;

namespace {
/**
 * Compresses the input files specified by the command line arguments into an archive.
 * @param command_line_arguments
 * @return Whether compression was successful
 */
bool compress(CommandLineArguments const& command_line_arguments);

/**
 * Decompresses the archive specified by the given JsonConstructorOption.
 * @param json_constructor_option
 */
void decompress_archive(clp_s::JsonConstructorOption const& json_constructor_option);

/**
 * Searches the given archive.
 * @param command_line_arguments
 * @param archive_reader
 * @param expr A copy of the search AST which may be modified
 * @param reducer_socket_fd
 * @param extra_projection_columns Additional projection columns (e.g., from SQL SELECT)
 * @param result_limit Maximum number of results to return (e.g., from SQL LIMIT)
 * @param aggregation_specs SQL aggregate functions to compute (e.g., COUNT(*), MIN, MAX)
 * @param column_aliases SQL AS aliases for column renaming (original path → alias)
 * @return Whether the search succeeded
 */
bool search_archive(
        CommandLineArguments const& command_line_arguments,
        std::shared_ptr<clp_s::ArchiveReader> const& archive_reader,
        std::shared_ptr<ast::Expression> expr,
        int reducer_socket_fd,
        std::vector<std::string> const& extra_projection_columns = {},
        std::optional<int64_t> result_limit = std::nullopt,
        std::vector<sql::AggregateSpec> const& aggregation_specs = {},
        std::unordered_map<std::string, std::string> const& column_aliases = {}
);

bool compress(CommandLineArguments const& command_line_arguments) {
    auto archives_dir = std::filesystem::path(command_line_arguments.get_archives_dir());

    // Create output directory in case it doesn't exist
    try {
        std::filesystem::create_directory(archives_dir.string());
    } catch (std::exception& e) {
        SPDLOG_ERROR(
                "Failed to create archives directory {} - {}",
                archives_dir.string(),
                e.what()
        );
        return false;
    }

    clp_s::JsonParserOption option{};
    option.input_paths_and_canonical_filenames
            = command_line_arguments.get_input_paths_and_canonical_filenames();
    option.network_auth = command_line_arguments.get_network_auth();
    option.archives_dir = archives_dir.string();
    option.target_encoded_size = command_line_arguments.get_target_encoded_size();
    option.max_document_size = command_line_arguments.get_max_document_size();
    option.min_table_size = command_line_arguments.get_minimum_table_size();
    option.compression_level = command_line_arguments.get_compression_level();
    option.timestamp_key = command_line_arguments.get_timestamp_key();
    option.print_archive_stats = command_line_arguments.print_archive_stats();
    option.retain_float_format = command_line_arguments.get_retain_float_format();
    option.single_file_archive = command_line_arguments.get_single_file_archive();
    option.structurize_arrays = command_line_arguments.get_structurize_arrays();
    option.record_log_order = command_line_arguments.get_record_log_order();

    clp_s::JsonParser parser(option);
    if (false == parser.ingest()) {
        SPDLOG_ERROR("Encountered error while parsing input.");
        return false;
    }
    std::ignore = parser.store();
    return true;
}

void decompress_archive(clp_s::JsonConstructorOption const& json_constructor_option) {
    clp_s::JsonConstructor constructor(json_constructor_option);
    constructor.store();
}

bool search_archive(
        CommandLineArguments const& command_line_arguments,
        std::shared_ptr<clp_s::ArchiveReader> const& archive_reader,
        std::shared_ptr<ast::Expression> expr,
        int reducer_socket_fd,
        std::vector<std::string> const& extra_projection_columns,
        std::optional<int64_t> result_limit,
        std::vector<sql::AggregateSpec> const& aggregation_specs,
        std::unordered_map<std::string, std::string> const& column_aliases
) {
    auto const& query = command_line_arguments.get_query();

    auto timestamp_dict = archive_reader->get_timestamp_dictionary();
    AddTimestampConditions add_timestamp_conditions(
            timestamp_dict->get_authoritative_timestamp_tokenized_column(),
            command_line_arguments.get_search_begin_ts(),
            command_line_arguments.get_search_end_ts()
    );
    if (expr = add_timestamp_conditions.run(expr); std::dynamic_pointer_cast<ast::EmptyExpr>(expr))
    {
        SPDLOG_ERROR(
                "Query '{}' specified timestamp filters tge {} tle {}, but no authoritative "
                "timestamp column was found for this archive",
                query,
                command_line_arguments.get_search_begin_ts().value_or(cEpochTimeMin),
                command_line_arguments.get_search_end_ts().value_or(cEpochTimeMax)
        );
        return false;
    }

    ast::OrOfAndForm standardize_pass;
    if (expr = standardize_pass.run(expr); std::dynamic_pointer_cast<ast::EmptyExpr>(expr)) {
        SPDLOG_ERROR("Query '{}' is logically false", query);
        return false;
    }

    ast::NarrowTypes narrow_pass;
    if (expr = narrow_pass.run(expr); std::dynamic_pointer_cast<ast::EmptyExpr>(expr)) {
        SPDLOG_ERROR("Query '{}' is logically false", query);
        return false;
    }

    ast::ConvertToExists convert_pass;
    if (expr = convert_pass.run(expr); std::dynamic_pointer_cast<ast::EmptyExpr>(expr)) {
        SPDLOG_ERROR("Query '{}' is logically false", query);
        return false;
    }

    EvaluateRangeIndexFilters metadata_filter_pass{
            archive_reader->get_range_index(),
            false == command_line_arguments.get_ignore_case()
    };
    if (expr = metadata_filter_pass.run(expr); std::dynamic_pointer_cast<ast::EmptyExpr>(expr)) {
        SPDLOG_INFO("No matching metadata ranges for query '{}'", query);
        return true;
    }

    // skip decompressing the archive if we won't match based on
    // the timestamp index
    EvaluateTimestampIndex timestamp_index(timestamp_dict);
    if (clp_s::EvaluatedValue::False == timestamp_index.run(expr)) {
        SPDLOG_INFO("No matching timestamp ranges for query '{}'", query);
        return true;
    }

    if (archive_reader->has_deprecated_timestamp_format()) {
        ast::SetTimestampLiteralPrecision date_precision_pass{
                ast::TimestampLiteral::Precision::Milliseconds
        };
        expr = date_precision_pass.run(expr);
    }

    // Narrow against schemas
    auto match_pass = std::make_shared<SchemaMatch>(
            archive_reader->get_schema_tree(),
            archive_reader->get_schema_map()
    );
    if (expr = match_pass->run(expr); std::dynamic_pointer_cast<ast::EmptyExpr>(expr)) {
        SPDLOG_INFO("No matching schemas for query '{}'", query);
        return true;
    }

    // Merge CLI and SQL-derived projection columns
    auto const& cli_projection_columns = command_line_arguments.get_projection_columns();
    auto const& effective_projection_columns
            = cli_projection_columns.empty() ? extra_projection_columns : cli_projection_columns;

    // Populate projection
    auto projection = std::make_shared<Projection>(
            effective_projection_columns.empty() ? ProjectionMode::ReturnAllColumns
                                                 : ProjectionMode::ReturnSelectedColumns
    );
    try {
        for (auto const& column : effective_projection_columns) {
            std::vector<std::string> descriptor_tokens;
            std::string descriptor_namespace;
            if (false
                == clp_s::search::ast::tokenize_column_descriptor(
                        column,
                        descriptor_tokens,
                        descriptor_namespace
                ))
            {
                SPDLOG_ERROR("Can not tokenize invalid column: \"{}\"", column);
                return false;
            }
            projection->add_column(
                    ast::ColumnDescriptor::create_from_escaped_tokens(
                            descriptor_tokens,
                            descriptor_namespace
                    )
            );
        }
    } catch (std::exception const& e) {
        SPDLOG_ERROR("{}", e.what());
        return false;
    }
    projection->resolve_columns(archive_reader->get_schema_tree());
    archive_reader->set_projection(projection);

    std::unique_ptr<OutputHandler> output_handler;
    try {
        switch (command_line_arguments.get_output_handler_type()) {
            case CommandLineArguments::OutputHandlerType::File:
                output_handler = std::make_unique<clp_s::FileOutputHandler>(
                        command_line_arguments.get_file_output_path(),
                        true
                );
                break;
            case CommandLineArguments::OutputHandlerType::Network:
                output_handler = std::make_unique<clp_s::NetworkOutputHandler>(
                        command_line_arguments.get_network_dest_host(),
                        command_line_arguments.get_network_dest_port()
                );
                break;
            case CommandLineArguments::OutputHandlerType::Reducer:
                if (command_line_arguments.do_count_results_aggregation()) {
                    output_handler = std::make_unique<clp_s::CountOutputHandler>(reducer_socket_fd);
                } else if (command_line_arguments.do_count_by_time_aggregation()) {
                    output_handler = std::make_unique<clp_s::CountByTimeOutputHandler>(
                            reducer_socket_fd,
                            command_line_arguments.get_count_by_time_bucket_size()
                    );
                } else {
                    SPDLOG_ERROR("Unhandled aggregation type.");
                    return false;
                }
                break;
            case CommandLineArguments::OutputHandlerType::ResultsCache:
                output_handler = std::make_unique<clp_s::ResultsCacheOutputHandler>(
                        command_line_arguments.get_mongodb_uri(),
                        command_line_arguments.get_mongodb_collection(),
                        command_line_arguments.get_batch_size(),
                        command_line_arguments.get_max_num_results(),
                        command_line_arguments.get_dataset()
                );
                break;
            case CommandLineArguments::OutputHandlerType::Stdout:
                output_handler = std::make_unique<clp_s::StandardOutputHandler>();
                break;
            default:
                SPDLOG_ERROR("Unhandled OutputHandlerType.");
                return false;
        }
    } catch (std::exception const& e) {
        SPDLOG_ERROR("Failed to create output handler - {}", e.what());
        return false;
    }

    // Override with aggregate output handler if aggregation is requested
    if (false == aggregation_specs.empty()) {
        output_handler = std::make_unique<LocalAggregateOutputHandler>(aggregation_specs);
    }

    // Wrap with renaming handler if SQL AS aliases are present
    if (false == column_aliases.empty()) {
        output_handler
                = std::make_unique<RenamingOutputHandler>(std::move(output_handler), column_aliases);
    }

    // output result
    Output output(
            match_pass,
            expr,
            archive_reader,
            std::move(output_handler),
            command_line_arguments.get_ignore_case(),
            result_limit
    );
    return output.filter();
}
}  // namespace

int main(int argc, char const* argv[]) {
    try {
        auto stderr_logger = spdlog::stderr_logger_st("stderr");
        spdlog::set_default_logger(stderr_logger);
        spdlog::set_pattern("%Y-%m-%dT%H:%M:%S.%e%z [%l] %v");
    } catch (std::exception& e) {
        // NOTE: We can't log an exception if the logger couldn't be constructed
        return 1;
    }

    mongocxx::instance const mongocxx_instance{};
#if CLP_BUILD_CLP_S_ENABLE_CURL
    clp::CurlGlobalInstance const curl_instance{};
#endif

    CommandLineArguments command_line_arguments("clp-s");
    auto parsing_result = command_line_arguments.parse_arguments(argc, argv);
    switch (parsing_result) {
        case CommandLineArguments::ParsingResult::Failure:
            return 1;
        case CommandLineArguments::ParsingResult::InfoCommand:
            return 0;
        case CommandLineArguments::ParsingResult::Success:
            // Continue processing
            break;
    }

    if (CommandLineArguments::Command::Compress == command_line_arguments.get_command()) {
        try {
            if (false == compress(command_line_arguments)) {
                return 1;
            }
        } catch (std::exception const& e) {
            SPDLOG_ERROR("Encountered error during compression - {}", e.what());
            return 1;
        }
    } else if (CommandLineArguments::Command::Extract == command_line_arguments.get_command()) {
        clp_s::JsonConstructorOption option{};
        option.output_dir = command_line_arguments.get_output_dir();
        option.ordered = command_line_arguments.get_ordered_decompression();
        option.target_ordered_chunk_size = command_line_arguments.get_target_ordered_chunk_size();
        option.print_ordered_chunk_stats = command_line_arguments.print_ordered_chunk_stats();
        option.network_auth = command_line_arguments.get_network_auth();
        if (false == command_line_arguments.get_mongodb_uri().empty()) {
            option.metadata_db
                    = {command_line_arguments.get_mongodb_uri(),
                       command_line_arguments.get_mongodb_collection()};
        }

        try {
            for (auto const& archive_path : command_line_arguments.get_input_paths()) {
                option.archive_path = archive_path;
                decompress_archive(option);
            }
        } catch (std::exception const& e) {
            SPDLOG_ERROR("Encountered error during decompression - {}", e.what());
            return 1;
        }
    } else {
        auto const& query = command_line_arguments.get_query();

        // Auto-detect SQL syntax: check if query starts with "SELECT" (case-insensitive)
        // followed by whitespace, after skipping any leading whitespace.
        bool const is_sql_query = [&query]() {
            constexpr std::string_view cSelectPrefix{"SELECT"};
            // Skip leading whitespace
            size_t offset = 0;
            while (offset < query.size()
                   && 0 != std::isspace(static_cast<unsigned char>(query[offset])))
            {
                ++offset;
            }
            if (query.size() - offset <= cSelectPrefix.size()) {
                return false;
            }
            for (size_t i = 0; i < cSelectPrefix.size(); ++i) {
                if (std::toupper(static_cast<unsigned char>(query[offset + i]))
                    != cSelectPrefix[i])
                {
                    return false;
                }
            }
            return 0
                   != std::isspace(
                           static_cast<unsigned char>(query[offset + cSelectPrefix.size()])
                   );
        }();

        std::shared_ptr<ast::Expression> expr;
        std::vector<std::string> sql_projection_columns;
        std::optional<int64_t> sql_limit;
        std::vector<sql::AggregateSpec> sql_aggregations;
        std::unordered_map<std::string, std::string> sql_column_aliases;

        if (is_sql_query) {
            auto query_stream = std::istringstream(query);
            auto sql_spec = sql::parse_sql_query(query_stream);
            if (false == sql_spec.has_value()) {
                SPDLOG_ERROR("Failed to parse SQL query.");
                SPDLOG_ERROR(
                        "Only SELECT statements are supported. Supported syntax:\n"
                        "  SELECT [columns|*|CLP_GET_*(...)] [FROM <table>] [WHERE <cond>] "
                        "[LIMIT <n>]\n"
                        "Examples:\n"
                        "  SELECT * FROM logs WHERE level = 'ERROR' LIMIT 100\n"
                        "  SELECT CLP_GET_INT('status'), CLP_GET_STRING('msg') FROM "
                        "logs\n"
                        "  SELECT CLP_GET_JSON_STRING() FROM logs WHERE "
                        "CLP_GET_INT('status') >= 500\n"
                        "  SELECT * FROM logs WHERE CLP_WILDCARD_COLUMN() LIKE "
                        "'%error%'\n"
                        "  SELECT * FROM logs WHERE \"@timestamp\" > TIMESTAMP "
                        "'2024-01-15 10:30:00'"
                );
                return 1;
            }
            expr = sql_spec->where_expr;
            sql_projection_columns = sql_spec->select_columns;
            sql_limit = sql_spec->limit;
            sql_aggregations = sql_spec->aggregations;
            sql_column_aliases = sql_spec->column_aliases;
        } else {
            auto query_stream = std::istringstream(query);
            expr = kql::parse_kql_expression(query_stream);
        }

        if (nullptr == expr) {
            return 1;
        }

        if (std::dynamic_pointer_cast<ast::EmptyExpr>(expr)) {
            SPDLOG_ERROR("Query '{}' is logically false", query);
            return 1;
        }

        int reducer_socket_fd{-1};
        if (command_line_arguments.get_output_handler_type()
            == CommandLineArguments::OutputHandlerType::Reducer)
        {
            reducer_socket_fd = reducer::connect_to_reducer(
                    command_line_arguments.get_reducer_host(),
                    command_line_arguments.get_reducer_port(),
                    command_line_arguments.get_job_id()
            );
            if (-1 == reducer_socket_fd) {
                SPDLOG_ERROR("Failed to connect to reducer");
                return 1;
            }
        }

        auto archive_reader = std::make_shared<clp_s::ArchiveReader>();
        for (auto const& input_path : command_line_arguments.get_input_paths()) {
            if (std::string::npos != input_path.path.find(clp::ir::cIrFileExtension)) {
                auto const result{clp_s::search_kv_ir_stream(
                        input_path,
                        command_line_arguments,
                        expr->copy(),
                        reducer_socket_fd
                )};
                if (false == result.has_error()) {
                    continue;
                }

                auto const error{result.error()};
                if (std::errc::result_out_of_range == error) {
                    // To support real-time search, we will allow incomplete IR streams.
                    // TODO: Use dedicated error code for this case once issue #904 is resolved.
                    SPDLOG_WARN("IR stream `{}` is truncated", input_path.path);
                    continue;
                }

                if (KvIrSearchError{KvIrSearchErrorEnum::ProjectionSupportNotImplemented} == error
                    || KvIrSearchError{KvIrSearchErrorEnum::UnsupportedOutputHandlerType} == error
                    || KvIrSearchError{KvIrSearchErrorEnum::CountSupportNotImplemented} == error)
                {
                    // These errors are treated as non-fatal because they result from unsupported
                    // features. However, this approach may cause archives with this extension to be
                    // skipped if the search uses advanced features that are not yet implemented. To
                    // mitigate this, we log a warning and proceed to search the input as an
                    // archive.
                    SPDLOG_WARN(
                            "Attempted to search an IR stream using unsupported features. Falling"
                            " back to searching the input as an archive."
                    );
                } else if (KvIrSearchError{KvIrSearchErrorEnum::DeserializerCreationFailure}
                           != error)
                {
                    // If the error is `DeserializerCreationFailure`, we may continue to treat the
                    // input as an archive and retry. Otherwise, it should be considered as a
                    // non-recoverable failure and return directly.
                    SPDLOG_ERROR(
                            "Failed to search '{}' as an IR stream, error_category={}, error={}",
                            input_path.path,
                            error.category().name(),
                            error.message()
                    );
                    return 1;
                }
            }

            try {
                archive_reader->open(input_path, command_line_arguments.get_network_auth());
            } catch (std::exception const& e) {
                SPDLOG_ERROR("Failed to open archive - {}", e.what());
                return 1;
            }
            if (false
                == search_archive(
                        command_line_arguments,
                        archive_reader,
                        expr->copy(),
                        reducer_socket_fd,
                        sql_projection_columns,
                        sql_aggregations.empty() ? sql_limit : std::nullopt,
                        sql_aggregations,
                        sql_column_aliases
                ))
            {
                return 1;
            }
            archive_reader->close();
        }
    }

    return 0;
}
