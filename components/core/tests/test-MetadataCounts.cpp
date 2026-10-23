#include <sys/socket.h>
#include <unistd.h>

#include <filesystem>
#include <fstream>
#include <limits>
#include <memory>
#include <optional>
#include <sstream>
#include <string>
#include <vector>

#include <catch2/catch_test_macros.hpp>
#include <catch2/generators/catch_generators.hpp>
#include <zstd.h>

#include <clp_s/AggregationSink.hpp>
#include <clp_s/aggregators.hpp>
#include <clp_s/archive_constants.hpp>
#include <clp_s/ArchiveReader.hpp>
#include <clp_s/ArchiveReaderAdaptor.hpp>
#include <clp_s/OutputHandlerImpl.hpp>
#include <clp_s/search/AddTimestampConditions.hpp>
#include <clp_s/search/ast/SearchUtils.hpp>
#include <clp_s/search/kql/kql.hpp>
#include <clp_s/search/Output.hpp>
#include <clp_s/search/SchemaMatch.hpp>
#include <reducer/DeserializedRecordGroup.hpp>

#include "../src/clp_s/tests/clp_s_test_utils.hpp"
#include "TestOutputCleaner.hpp"

namespace {
struct Results {
    uint64_t count{};
    bool finished{};
    clp_s::search::SearchResultMetrics metrics;
};

class CountSink : public clp_s::AggregationSink {
public:
    explicit CountSink(Results& result) : m_result{result} {}

    auto write(clp_s::AggregationResult const& value)
            -> ystdlib::error_handling::Result<void> override {
        for (auto const& [key, count] : value) {
            if (key == "count") {
                m_result.count += std::get<int64_t>(count);
            }
        }
        return ystdlib::error_handling::success();
    }

    auto finish() -> ystdlib::error_handling::Result<void> override {
        m_result.finished = true;
        return ystdlib::error_handling::success();
    }

private:
    Results& m_result;
};

// Deliberately does not opt in to bulk counting, even when it needs no record bytes.
class RowSink : public clp_s::search::OutputHandler {
public:
    RowSink(Results& result, bool metadata, bool reconstruct)
            : OutputHandler{metadata, reconstruct},
              m_result{result} {}

    auto write(std::string_view value) -> void override {
        REQUIRE(value.empty() == !should_marshal_records());
        ++m_result.count;
    }

    auto write(std::string_view value, clp_s::epochtime_t timestamp, std::string_view, int64_t)
            -> void override {
        REQUIRE(timestamp >= 1000);
        write(value);
    }

    auto finish() -> clp_s::ErrorCode override {
        m_result.finished = true;
        return clp_s::ErrorCodeSuccess;
    }

private:
    Results& m_result;
};

auto run_query(
        std::filesystem::path const& archive,
        std::string const& query,
        bool experimental,
        int output_mode = 0,
        bool time_bound = false
) -> Results {
    auto reader{std::make_shared<clp_s::ArchiveReader>()};
    reader->open(
            {clp_s::InputSource::Filesystem, archive.string()},
            {.m_experimental = experimental}
    );
    std::istringstream input{query};
    auto expression{clp_s::search::kql::parse_kql_expression(input)};
    REQUIRE(expression != nullptr);
    if (time_bound) {
        clp_s::search::AddTimestampConditions bounds{
                reader->get_timestamp_dictionary()->get_authoritative_timestamp_tokenized_column(),
                2000,
                2000
        };
        expression = bounds.run(expression);
    }
    expression = clp_s::search::ast::preprocess_query(expression);
    auto match{std::make_shared<clp_s::search::SchemaMatch>(reader, true)};
    expression = match->run(expression);
    Results result;
    std::unique_ptr<clp_s::search::OutputHandler> handler;
    if (output_mode == 0) {
        handler = clp_s::make_aggregation_output_handler(
                clp_s::CountAggregator{},
                std::make_unique<CountSink>(result)
        );
    } else {
        handler = std::make_unique<RowSink>(result, output_mode == 2, output_mode == 3);
    }
    clp_s::search::Output output{match, expression, reader, std::move(handler), false};
    auto const success{output.filter()};
    REQUIRE(success);
    result.metrics = output.get_result_metrics();
    REQUIRE(result.count == result.metrics.num_archive_records_matching_query);
    if (result.metrics.num_schemas_counted_from_metadata != 0
        && result.metrics.num_schemas_scanned == 0)
    {
        REQUIRE(output.get_termination_stage() == clp_s::search::cTerminationStageMetadataCount);
    }
    reader->close();
    return result;
}

auto create_archive(
        std::filesystem::path const& root,
        bool single,
        bool separate,
        bool experimental,
        bool dictionary
) -> std::filesystem::path {
    std::filesystem::create_directories(root);
    auto const input{root / "input.jsonl"};
    {
        std::ofstream out{input};
        out << R"({"ts":1,"seq":1,"state":"busy","message":"alpha value 1","arr":[{"v":1}]})"
            << '\n'
            << R"({"ts":2,"seq":2,"state":"idle","message":"alpha value 2","arr":[{"w":1}]})"
            << '\n'
            << R"({"ts":3,"seq":3,"state":"busy","message":"beta value 3","arr":[{"v":2}]})" << '\n'
            << R"({"ts":4,"seq":4,"state":"idle","message":"beta value 4","arr":[]})" << '\n'
            << R"({"ts":5,"seq":5,"state":"busy","arr":[]})" << '\n';
    }
    std::optional<std::filesystem::path> spec;
    if (experimental) {
        spec = root / "parsing.spec";
        std::ofstream out{*spec};
        out << "delimiters: \" \\t\\r\\n\"\nnumber: \"^[0-9]+$\"\ntag: \"value\"\n";
    }
    auto const directory{root / "archives"};
    auto const stats{compress_archive(
            input.string(),
            directory.string(),
            "ts",
            false,
            single,
            false,
            spec,
            separate,
            separate ? 1 : 0,
            dictionary
    )};
    REQUIRE(stats.size() == 1);
    return directory / stats[0].get_id();
}

auto create_range_archive(
        std::filesystem::path const& root,
        bool single,
        bool separate,
        bool experimental,
        bool structured,
        std::string const& records
) -> std::filesystem::path {
    std::filesystem::create_directories(root);
    auto const input{root / "input.jsonl"};
    {
        std::ofstream out{input};
        out << records;
    }
    std::optional<std::filesystem::path> spec;
    if (experimental) {
        spec = root / "parsing.spec";
        std::ofstream out{*spec};
        out << "delimiters: \" \\t\\r\\n\"\nnumber: \"^[0-9]+$\"\n";
    }
    auto const directory{root / "archives"};
    auto const stats{compress_archive(
            input.string(),
            directory.string(),
            "ts",
            false,
            single,
            structured,
            spec,
            separate,
            separate ? 1 : 0,
            separate
    )};
    REQUIRE(stats.size() == 1);
    return directory / stats[0].get_id();
}

auto corrupt_tables(
        std::filesystem::path const& archive,
        std::optional<size_t> physical_column = std::nullopt
) -> void {
    auto file{archive};
    size_t offset{};
    if (std::filesystem::is_directory(archive)) {
        file += clp_s::constants::cArchiveTablesFile;
    } else {
        clp_s::ArchiveReaderAdaptor adaptor{{clp_s::InputSource::Filesystem, archive.string()}, {}};
        REQUIRE(adaptor.load_archive_metadata() == clp_s::ErrorCodeSuccess);
        offset = adaptor.checkout_reader_for_section(clp_s::constants::cArchiveTablesFile)
                         ->get_pos();
        adaptor.checkin_reader_for_section(clp_s::constants::cArchiveTablesFile);
    }
    std::fstream stream{file, std::ios::in | std::ios::out | std::ios::binary};
    REQUIRE(stream.is_open());
    if (physical_column.has_value()) {
        std::vector<char> bytes(std::filesystem::file_size(file) - offset);
        stream.seekg(static_cast<std::streamoff>(offset));
        stream.read(bytes.data(), static_cast<std::streamsize>(bytes.size()));
        REQUIRE(stream.good());
        size_t position{};
        for (size_t column{}; column < *physical_column; ++column) {
            auto const size{
                    ZSTD_findFrameCompressedSize(bytes.data() + position, bytes.size() - position)
            };
            REQUIRE_FALSE(ZSTD_isError(size));
            position += size;
            REQUIRE(position < bytes.size());
        }
        stream.seekp(static_cast<std::streamoff>(offset + position));
        stream.put(0);  // Damage only this frame's Zstd magic.
        REQUIRE(stream.good());
        return;
    }
    stream.seekp(static_cast<std::streamoff>(offset));
    // Damage every frame, including frames other than the first physical column.
    std::vector<char> zeros(std::filesystem::file_size(file) - offset);
    stream.write(zeros.data(), static_cast<std::streamsize>(zeros.size()));
    REQUIRE(stream.good());
}

#if CLP_BUILD_CLPP_DECOMPOSITION
// This helper requires a single ERT with separate column frames.
auto find_physical_column(std::filesystem::path const& archive, std::string const& field)
        -> size_t {
    clp_s::ArchiveReader reader;
    reader.open({clp_s::InputSource::Filesystem, archive.string()}, {.m_experimental = true});
    reader.read_dictionaries_and_metadata();
    REQUIRE(reader.get_schema_ids().size() == 1);
    auto const schema_id{reader.get_schema_ids().front()};
    REQUIRE(reader.has_separate_columns(schema_id));
    reader.open_packed_streams();
    auto& table{reader.read_schema_table(schema_id, false, false)};
    for (auto const& node : reader.get_schema_tree()->get_nodes()) {
        if (node.get_key_name() != field) {
            continue;
        }
        auto const selection{table.select_columns({node.get_id()})};
        for (size_t i{}; i < selection.size(); ++i) {
            if (selection[i]) {
                reader.close();
                return i;
            }
        }
    }
    FAIL("No physical column found for " << field);
    return 0;
}
#endif
}  // namespace

TEST_CASE(
        "Metadata counts preserve full predicates and output requirements",
        "[clp-s][MetadataCounts]"
) {
    auto const single{GENERATE(false, true)};
    auto const separate{GENERATE(false, true)};
#if CLP_BUILD_CLPP_DECOMPOSITION
    auto const experimental{GENERATE(false, true)};
#else
    bool const experimental{false};
#endif
    auto const dictionary{GENERATE(false, true)};
    std::filesystem::path const root{"test-metadata-counts"};
    TestOutputCleaner cleanup{{root.string()}};
    auto const archive{create_archive(root, single, separate, experimental, dictionary)};
    for (auto const& [query, count] :
         std::vector<std::pair<std::string, uint64_t>>{
                 {"seq: *", 5},
                 {"state: b*", 3},
                 {"NOT state: idle", 3},
                 {"message: *", 4},
                 {"NOT message: *", 1},
                 {"message: * AND seq: *", 4},
                 {"message: * OR seq: 5", 5},
                 {"NOT (seq: 1 OR seq: 2)", 3},
                 {"message: alpha*", 2},
                 {"message: alpha* AND seq: 1", 1},
                 {"message: alpha* OR message: beta*", 4},
                 {"message: *value*", 4},
                 {"message: *1*", 1},
                 {"arr.v: *", 2},
                 {"seq: 999", 0}
         })
    {
        CAPTURE(single, separate, experimental, dictionary, query);
        REQUIRE(run_query(archive, query, experimental).count == count);
        REQUIRE(run_query(archive, query, experimental, 1).count == count);
    }
    auto all{run_query(archive, "message: * AND seq: *", experimental)};
    REQUIRE(all.count == 4);
    REQUIRE(all.finished);
    REQUIRE(all.metrics.num_records_counted_from_metadata == 4);
    REQUIRE(all.metrics.num_schemas_scanned == 0);
    if (experimental) {
        auto shape{run_query(archive, "message: alpha*", true)};
        REQUIRE(shape.metrics.num_records_counted_from_metadata == 2);
        REQUIRE(shape.metrics.num_messages_evaluated == 0);
    }
    auto bounded{run_query(archive, "message: *", experimental, 0, true)};
    REQUIRE(bounded.count == 1);
    REQUIRE(bounded.metrics.num_records_counted_from_metadata == 0);
    auto array{run_query(archive, "arr.v: *", experimental)};
    REQUIRE(array.metrics.num_records_counted_from_metadata == 0);
    for (int mode : {1, 2, 3}) {
        auto rows{run_query(archive, "message: *", experimental, mode)};
        REQUIRE(rows.count == 4);
        REQUIRE(rows.metrics.num_records_counted_from_metadata == 0);
        REQUIRE(rows.metrics.num_schemas_scanned > 0);
    }
    corrupt_tables(archive);
    REQUIRE(run_query(archive, "message: * AND seq: *", experimental).count == 4);
    if (experimental) {
        REQUIRE(run_query(archive, "message: alpha*", true).count == 2);
    }
    REQUIRE_THROWS(run_query(archive, "seq: 2", experimental));
    REQUIRE_THROWS(run_query(archive, "message: *", experimental, 0, true));
    REQUIRE_THROWS(run_query(archive, "message: *", experimental, 3));
}

TEST_CASE(
        "Integer range summaries preserve comparison and expression semantics",
        "[clp-s][IntegerRangeProofs]"
) {
    auto const single{GENERATE(false, true)};
    auto const separate{GENERATE(false, true)};
    auto const structured{GENERATE(false, true)};
#if CLP_BUILD_CLPP_DECOMPOSITION
    auto const experimental{GENERATE(false, true)};
#else
    bool const experimental{false};
#endif
    std::filesystem::path const root{"test-integer-range-proofs"};
    TestOutputCleaner cleanup{{root.string()}};
    std::ostringstream records;
    int64_t ts{};
    for (int64_t seq : {-5, -2, 0, 2, 5}) {
        ++ts;
        records << "{\"ts\":" << ts << ",\"seq\":" << seq << ",\"other\":" << (ts % 2)
                << ",\"lo\":" << std::numeric_limits<int64_t>::min()
                << ",\"hi\":" << std::numeric_limits<int64_t>::max() << ",\"arr\":[{\"v\":" << seq
                << "},{\"v\":" << (ts - 1) << "}]}\n";
    }
    auto const archive{
            create_range_archive(root, single, separate, experimental, structured, records.str())
    };
    for (auto const& [query, count] :
         std::vector<std::pair<std::string, uint64_t>>{
                 {"seq < -5", 0},
                 {"seq <= -6", 0},
                 {"seq > 5", 0},
                 {"seq >= 6", 0},
                 {"seq < 6", 5},
                 {"seq <= 5", 5},
                 {"seq > -6", 5},
                 {"seq >= -5", 5},
                 {"seq <= -5", 1},
                 {"seq >= 5", 1},
                 {"seq < -2", 1},
                 {"seq <= -2", 2},
                 {"seq > 2", 1},
                 {"seq >= 2", 2},
                 {"seq < -1.5", 2},
                 {"seq <= -1.5", 2},
                 {"seq > -1.5", 3},
                 {"seq >= -1.5", 3},
                 {"seq < 1.5", 3},
                 {"seq <= 1.5", 3},
                 {"seq > 1.5", 2},
                 {"seq >= 1.5", 2},
                 {"seq < 5.5", 5},
                 {"seq >= 5.5", 0},
                 {"seq > -5.5", 5},
                 {"seq <= -5.5", 0},
                 {"seq >= -9223372036854775808", 5},
                 {"seq <= 9223372036854775807", 5},
                 {"lo < -9223372036854775808", 0},
                 {"lo <= -9223372036854775808", 5},
                 {"lo > -9223372036854775808", 0},
                 {"lo >= -9223372036854775808", 5},
                 {"hi < 9223372036854775807", 0},
                 {"hi <= 9223372036854775807", 5},
                 {"hi > 9223372036854775807", 0},
                 {"hi >= 9223372036854775807", 5},
                 {"NOT seq >= -5", 0},
                 {"NOT seq > 5", 5},
                 {"seq >= -5 AND seq <= 5", 5},
                 {"seq < -5 OR seq > 5", 0},
                 {"NOT (seq < -5 OR seq > 5)", 5},
                 {"seq >= -5 AND other: 1", 3},
                 {"seq < -5 OR other: 1", 3},
                 {"seq > 0 OR other: 1", 4},
                 {"NOT (seq >= -5 AND other: 1)", 2},
                 {"seq > -3 AND seq < 3", 3}
         })
    {
        CAPTURE(single, separate, structured, experimental, query);
        auto const result{run_query(archive, query, experimental)};
        REQUIRE(result.count == count);
        // Every all-or-none scalar case above is provable on this single ERT.
        bool const proven{experimental && (count == 0 || count == 5)};
        REQUIRE(result.metrics.num_records_counted_from_metadata == (proven ? count : 0));
        REQUIRE(result.metrics.num_schemas_scanned == (proven ? 0 : 1));
        REQUIRE(run_query(archive, query, experimental, 3).count == count);
    }
    for (auto const& [query, count] :
         std::vector<std::pair<std::string, uint64_t>>{
                 {"arr.v >= -5", 5},
                 {"arr.v > 5", 0},
                 {"arr.v < 0", 2},
                 {"arr.v >= 3", 2},
                 {"NOT arr.v < 0", 3}
         })
    {
        CAPTURE(single, separate, structured, experimental, query);
        auto const result{run_query(archive, query, experimental)};
        if (structured) {
            REQUIRE(result.count == count);
        } else {
            // Unresolved paths within array strings have no integer summary. Check that they
            // retain row evaluation; its existing range-comparison semantics are separate.
            REQUIRE(result.metrics.num_records_counted_from_metadata == 0);
            REQUIRE(result.metrics.num_schemas_scanned == 1);
        }
        REQUIRE(run_query(archive, query, experimental, 3).count == result.count);
    }
    auto const wildcard{run_query(archive, "* > 0", experimental)};
    REQUIRE(wildcard.count == 5);
    REQUIRE(wildcard.metrics.num_records_counted_from_metadata == 0);
    REQUIRE(wildcard.metrics.num_schemas_scanned == 1);

    corrupt_tables(archive);
    if (experimental) {
        REQUIRE(run_query(archive, "seq >= -5 AND seq <= 5", true).count == 5);
        REQUIRE(run_query(archive, "seq < -5 OR seq > 5", true).count == 0);
    } else {
        REQUIRE_THROWS(run_query(archive, "seq >= -5 AND seq <= 5", false));
    }
    REQUIRE_THROWS(run_query(archive, "seq >= 0", experimental));
    REQUIRE_THROWS(run_query(archive, "seq >= -5", experimental, 3));
}

#if CLP_BUILD_CLPP_DECOMPOSITION
TEST_CASE("Integer range counts combine proven and scanned ERTs", "[clp-s][IntegerRangeProofs]") {
    auto const single{GENERATE(false, true)};
    auto const separate{GENERATE(false, true)};
    std::filesystem::path const root{"test-integer-range-mixed-erts"};
    TestOutputCleaner cleanup{{root.string()}};
    auto const archive{create_range_archive(
            root,
            single,
            separate,
            true,
            false,
            "{\"seq\":-5,\"a\":1}\n{\"seq\":-4,\"a\":1}\n{\"seq\":-3,\"a\":1}\n"
            "{\"seq\":1,\"b\":1}\n{\"seq\":2,\"b\":1}\n{\"seq\":3,\"b\":1}\n"
    )};
    for (auto const& [query, count] :
         std::vector<std::pair<std::string, uint64_t>>{{"seq < 2", 4}, {"seq >= -4", 5}})
    {
        CAPTURE(single, separate, query);
        auto const result{run_query(archive, query, true)};
        REQUIRE(result.count == count);
        REQUIRE(result.metrics.num_records_counted_from_metadata == 3);
        REQUIRE(result.metrics.num_schemas_counted_from_metadata == 1);
        REQUIRE(result.metrics.num_messages_evaluated == 3);
        REQUIRE(result.metrics.num_schemas_scanned == 1);
    }
    auto const result{run_query(archive, "seq >= 0", true)};
    REQUIRE(result.count == 3);
    REQUIRE(result.metrics.num_records_counted_from_metadata == 3);
    REQUIRE(result.metrics.num_schemas_scanned == 0);
}

TEST_CASE("Proven integer predicates leave their column unread", "[clp-s][IntegerRangeProofs]") {
    auto const single{GENERATE(false, true)};
    std::filesystem::path const root{"test-integer-range-columns"};
    TestOutputCleaner cleanup{{root.string()}};
    auto const archive{create_range_archive(
            root,
            single,
            true,
            true,
            false,
            "{\"seq\":1,\"other\":0}\n{\"seq\":2,\"other\":1}\n{\"seq\":3,\"other\":0}\n"
    )};
    corrupt_tables(archive, find_physical_column(archive, "seq"));
    for (auto const& query : {"seq >= 1 AND other: 0", "seq > 3 OR other: 0"}) {
        CAPTURE(single, query);
        auto const result{run_query(archive, query, true)};
        REQUIRE(result.count == 2);
        REQUIRE(result.metrics.num_schemas_scanned == 1);
        REQUIRE(result.metrics.num_records_counted_from_metadata == 0);
    }
    REQUIRE_THROWS(run_query(archive, "seq >= 2 AND other: 0", true));
    REQUIRE_THROWS(run_query(archive, "seq >= 1 AND other: 0", true, 3));
}

TEST_CASE(
        "Integer range proofs preserve mixed and missing field behavior",
        "[clp-s][IntegerRangeProofs]"
) {
    std::filesystem::path const root{"test-integer-range-types"};
    TestOutputCleaner cleanup{{root.string()}};
    std::string const records{
            "{\"seq\":-1}\n{\"seq\":1}\n{\"seq\":null}\n{}\n{\"seq\":\"word\"}\n{\"seq\":1.5}\n"
    };
    auto const baseline{
            create_range_archive(root / "baseline", false, true, false, false, records)
    };
    auto const candidate{
            create_range_archive(root / "candidate", false, true, true, false, records)
    };
    for (auto const& query :
         {"seq >= 0",
          "seq < 0",
          "seq <= 1.5",
          "NOT seq >= 0",
          "seq >= 0 OR NOT seq: *",
          "seq >= 0 AND seq: *"})
    {
        CAPTURE(query);
        auto const expected{run_query(baseline, query, false)};
        REQUIRE(run_query(candidate, query, true).count == expected.count);
        REQUIRE(run_query(candidate, query, true, 3).count == expected.count);
    }
}
#endif

TEST_CASE(
        "Count handlers combine batches and individual rows without overflow",
        "[clp-s][MetadataCounts]"
) {
    clp_s::CountAggregator count;
    count.add_count(static_cast<uint64_t>(std::numeric_limits<int64_t>::max()) - 1);
    count.add_record({}, 0);
    REQUIRE(count.get_count() == std::numeric_limits<int64_t>::max());
    REQUIRE_THROWS_AS(count.add_count(1), std::overflow_error);
    REQUIRE_THROWS_AS(count.add_count(UINT64_MAX), std::overflow_error);
    REQUIRE(count.get_count() == std::numeric_limits<int64_t>::max());

    Results results;
    auto handler{clp_s::make_aggregation_output_handler(
            clp_s::CountAggregator{},
            std::make_unique<CountSink>(results)
    )};
    REQUIRE(handler->try_write_count(100));
    handler->write("");
    REQUIRE(handler->try_write_count(7));
    REQUIRE(handler->finish() == clp_s::ErrorCodeSuccess);
    REQUIRE(results.count == 108);
    REQUIRE(results.finished);
    auto timed{clp_s::make_aggregation_output_handler(
            clp_s::CountByTimeAggregator{1000},
            std::make_unique<CountSink>(results)
    )};
    REQUIRE_FALSE(timed->try_write_count(100));

    int sockets[2];
    REQUIRE(socketpair(AF_UNIX, SOCK_STREAM, 0, sockets) == 0);
    clp_s::CountReducerOutputHandler reducer{sockets[0]};
    REQUIRE(reducer.try_write_count(100));
    reducer.write("");
    REQUIRE(reducer.finish() == clp_s::ErrorCodeSuccess);
    size_t size{};
    REQUIRE(recv(sockets[1], &size, sizeof(size), MSG_WAITALL) == sizeof(size));
    std::vector<uint8_t> bytes(size);
    REQUIRE(recv(sockets[1], bytes.data(), size, MSG_WAITALL) == size);
    close(sockets[0]);
    close(sockets[1]);
    reducer::DeserializedRecordGroup group{bytes};
    REQUIRE(group.get_tags().empty());
    REQUIRE(group.record_iter().get().get_int64_value("count") == 101);
}
