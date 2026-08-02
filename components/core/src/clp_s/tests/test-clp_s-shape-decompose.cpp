#include <filesystem>
#include <memory>
#include <sstream>
#include <stdexcept>
#include <string>
#include <string_view>
#include <vector>

#include <catch2/catch_test_macros.hpp>
#include <nlohmann/json.hpp>

#include "../src/clp_s/ArchiveReader.hpp"
#include "../src/clp_s/InputConfig.hpp"
#include "../src/clp_s/OutputHandlerImpl.hpp"
#include "../src/clp_s/search/ast/ColumnDescriptor.hpp"
#include "../src/clp_s/search/ast/ConvertToExists.hpp"
#include "../src/clp_s/search/ast/Expression.hpp"
#include "../src/clp_s/search/ast/FunctionCall.hpp"
#include "../src/clp_s/search/ast/NarrowTypes.hpp"
#include "../src/clp_s/search/ast/OrOfAndForm.hpp"
#include "../src/clp_s/search/kql/kql.hpp"
#include "../src/clp_s/search/Output.hpp"
#include "../src/clp_s/search/Projection.hpp"
#include "../src/clp_s/search/SchemaMatch.hpp"
#include "clp_s_test_utils.hpp"
#include "TestOutputCleaner.hpp"

constexpr std::string_view cTestShapeArchiveDirectory{"test-clp-s-shape-decompose-archive"};
constexpr std::string_view cTestInputFileDirectory{"test_log_files"};
constexpr std::string_view cTestShapeInputFile{"test_shape_decompose.jsonl"};
constexpr std::string_view cTestIdxKey{"idx"};

// The single fixture record's reconstructed values and their expected decomposed forms.
constexpr std::string_view cMsgValue{"user 42 logged in from 10.0.0.1 after 3.5 seconds"};
constexpr std::string_view cMsgShape{"user %int% logged in from %str% after %float% seconds"};
constexpr std::string_view cInnerShape{"task %int% finished in %float% sec"};

namespace {
auto get_test_input_local_path(std::string_view test_input_path) -> std::string {
    std::filesystem::path const current_file_path{__FILE__};
    auto const tests_dir{current_file_path.parent_path()};
    return (tests_dir / std::filesystem::path{cTestInputFileDirectory} / test_input_path).string();
}

/**
 * Runs a query against the fixture archive with a given set of projection columns and returns the
 * emitted records parsed as JSON.
 *
 * The projection columns use the same syntax as `clp-s s --projection`, so a column may be a plain
 * name (`msg`) or a function call (`decompose(msg)`).
 *
 * @param query
 * @param projection_columns Empty means return all columns.
 * @return The emitted records, parsed as JSON.
 */
auto
search_with_projection(std::string const& query, std::vector<std::string> const& projection_columns)
        -> std::vector<nlohmann::json> {
    namespace ast = clp_s::search::ast;
    using clp_s::search::Projection;

    auto query_stream = std::istringstream{query};
    auto expr = clp_s::search::kql::parse_kql_expression(query_stream);
    REQUIRE(nullptr != expr);

    ast::OrOfAndForm standardize_pass;
    expr = standardize_pass.run(expr);
    ast::NarrowTypes narrow_pass;
    expr = narrow_pass.run(expr);
    ast::ConvertToExists convert_pass;
    expr = convert_pass.run(expr);
    REQUIRE(nullptr != expr);

    std::vector<clp_s::VectorOutputHandler::QueryResult> results;
    for (auto const& entry : std::filesystem::directory_iterator(cTestShapeArchiveDirectory)) {
        auto archive_reader = std::make_shared<clp_s::ArchiveReader>();
        archive_reader->open(
                clp_s::Path{.source{clp_s::InputSource::Filesystem}, .path{entry.path().string()}},
                clp_s::ArchiveReader::Options{}
        );

        auto projection = std::make_shared<Projection>(
                projection_columns.empty() ? Projection::Mode::ReturnAllColumns
                                           : Projection::Mode::ReturnSelectedColumns
        );
        for (auto const& column : projection_columns) {
            auto parsed{clp_s::search::kql::parse_projection_column(column)};
            REQUIRE(nullptr != parsed);
            if (auto func_call{std::dynamic_pointer_cast<ast::FunctionCall>(parsed)}) {
                projection->add_column(func_call);
            } else {
                auto col_desc{std::dynamic_pointer_cast<ast::ColumnDescriptor>(parsed)};
                REQUIRE(nullptr != col_desc);
                projection->add_column(col_desc, Projection::NodeMask::Mode::Value);
            }
        }
        projection->resolve_columns(*archive_reader->get_schema_tree());
        archive_reader->set_projection(projection);

        auto archive_expr = expr->copy();
        auto match_pass = std::make_shared<clp_s::search::SchemaMatch>(archive_reader, false);
        archive_expr = match_pass->run(archive_expr);
        REQUIRE(nullptr != archive_expr);

        clp_s::search::Output output_pass{
                match_pass,
                archive_expr,
                archive_reader,
                std::make_unique<clp_s::VectorOutputHandler>(results),
                false
        };
        output_pass.filter();
        archive_reader->close();
    }

    std::vector<nlohmann::json> parsed_results;
    parsed_results.reserve(results.size());
    for (auto const& result : results) {
        parsed_results.emplace_back(nlohmann::json::parse(result.message));
    }
    return parsed_results;
}

/**
 * Compresses the fixture and runs a projection against it, returning the single expected record.
 * @param projection_columns
 * @return The emitted record.
 */
auto project_single_record(std::vector<std::string> const& projection_columns) -> nlohmann::json {
    auto results = search_with_projection("idx: 0", projection_columns);
    REQUIRE(1 == results.size());
    return results.at(0);
}
}  // namespace

TEST_CASE("clp-s-shape-projection", "[clp-s][shape-decompose]") {
    TestOutputCleaner const test_cleanup{{std::string{cTestShapeArchiveDirectory}}};
    REQUIRE_NOTHROW(
            std::ignore = compress_archive(
                    get_test_input_local_path(cTestShapeInputFile),
                    std::string{cTestShapeArchiveDirectory},
                    std::string{cTestIdxKey},
                    false,
                    false,
                    false
            )
    );

    SECTION("A plain projection emits the reconstructed value") {
        auto const record = project_single_record({"msg"});
        REQUIRE(cMsgValue == record.at("msg").get<std::string>());
    }

    SECTION("shape(...) replaces the value with its logtype template") {
        auto const record = project_single_record({"shape(msg)"});
        REQUIRE(cMsgShape == record.at("msg").get<std::string>());
    }

    SECTION("shape(...) applies to a field nested in an object") {
        auto const record = project_single_record({"shape(nested.inner)"});
        REQUIRE(cInnerShape == record.at("nested").at("inner").get<std::string>());
    }

    SECTION("shape(...) rejects a column that is not CLP-encoded") {
        // A string without a space is stored as a VarString, which has no logtype to render.
        // The projection must fail loudly at resolution time rather than silently emit nothing.
        REQUIRE_THROWS_AS(project_single_record({"shape(nospaces)"}), std::runtime_error);
    }

    SECTION("Projecting several columns emits each of them") {
        auto const record = project_single_record({"msg", "nested.inner"});
        REQUIRE(cMsgValue == record.at("msg").get<std::string>());
        REQUIRE(record.at("nested").contains("inner"));
    }

    SECTION("Repeating a column with the same function is rejected") {
        REQUIRE_THROWS(project_single_record({"msg", "msg"}));
        REQUIRE_THROWS(project_single_record({"shape(msg)", "shape(msg)"}));
    }

    SECTION("shape(...) replaces the value rather than accompanying it") {
        // There is no second key to hold both forms, so the shape wins over a plain projection of
        // the same column.
        auto const record = project_single_record({"shape(msg)", "msg"});
        REQUIRE(cMsgShape == record.at("msg").get<std::string>());
    }

    SECTION("An unprojected query is unaffected") {
        auto const record = project_single_record({});
        REQUIRE(cMsgValue == record.at("msg").get<std::string>());
        REQUIRE(record.at("arr").is_array());
    }
}
