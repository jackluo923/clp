#include <filesystem>
#include <fstream>
#include <iterator>
#include <memory>
#include <sstream>
#include <string>
#include <string_view>
#include <vector>

#include <catch2/catch_test_macros.hpp>
#include <catch2/generators/catch_generators.hpp>

#include <clp/BufferReader.hpp>
#include <clp/FileReader.hpp>
#include <clp_s/archive_constants.hpp>
#include <clp_s/ArchiveReader.hpp>
#include <clp_s/ArchiveReaderAdaptor.hpp>
#include <clp_s/search/ast/SearchUtils.hpp>
#include <clp_s/search/kql/kql.hpp>
#include <clp_s/search/Output.hpp>
#include <clp_s/search/OutputHandler.hpp>
#include <clp_s/search/SchemaMatch.hpp>

#include "../src/clp_s/tests/clp_s_test_utils.hpp"
#include "TestOutputCleaner.hpp"

namespace {
class CountRows : public clp_s::search::OutputHandler {
public:
    CountRows(uint64_t& rows, bool reconstruct) : OutputHandler{false, reconstruct}, m_rows{rows} {}

    auto write(std::string_view message) -> void override {
        REQUIRE(message.empty() == !should_marshal_records());
        ++m_rows;
    }

    auto write(std::string_view message, clp_s::epochtime_t, std::string_view, int64_t)
            -> void override {
        write(message);
    }

private:
    uint64_t& m_rows;
};

/** Models a streaming input that cannot revisit bytes it has already consumed. */
class ForwardOnlyReader : public clp::ReaderInterface {
public:
    explicit ForwardOnlyReader(std::string const& path) : m_file{path} {}

    auto try_read(char* data, size_t size, size_t& read) -> clp::ErrorCode override {
        return m_file.try_read(data, size, read);
    }

    auto try_seek_from_begin(size_t pos) -> clp::ErrorCode override {
        if (pos < m_file.get_pos()) {
            ++backward_seeks;
            return clp::ErrorCode_Unsupported;
        }
        return m_file.try_seek_from_begin(pos);
    }

    auto try_get_pos(size_t& pos) -> clp::ErrorCode override { return m_file.try_get_pos(pos); }

    size_t backward_seeks{};

private:
    clp::FileReader m_file;
};

auto run_query(
        std::shared_ptr<clp_s::ArchiveReader> const& reader,
        std::string const& query,
        bool reconstruct = false
) -> uint64_t {
    std::istringstream input{query};
    auto expression{clp_s::search::kql::parse_kql_expression(input)};
    REQUIRE(expression != nullptr);
    expression = clp_s::search::ast::preprocess_query(expression);
    auto match{std::make_shared<clp_s::search::SchemaMatch>(reader, true)};
    expression = match->run(expression);
    uint64_t rows{};
    clp_s::search::Output output{
            match,
            expression,
            reader,
            std::make_unique<CountRows>(rows, reconstruct),
            false
    };
    auto const success{output.filter()};
    REQUIRE(success);
    reader->close();
    return rows;
}

auto open(std::filesystem::path const& path) -> std::shared_ptr<clp_s::ArchiveReader> {
    auto reader{std::make_shared<clp_s::ArchiveReader>()};
    reader->open({clp_s::InputSource::Filesystem, path.string()}, {});
    return reader;
}

auto create_archive(std::filesystem::path const& root, bool single_file, bool separate)
        -> std::filesystem::path {
    std::filesystem::create_directories(root);
    auto const input{root / "input.jsonl"};
    {
        std::ofstream out{input};
        out << R"({"seq":1,"state":"busy","message":"Alpha request 123","arr":["needle","foo bar"]})"
            << '\n'
            << R"({"seq":2,"state":"idle","message":"Beta response 456","arr":["other","baz quux"]})"
            << '\n';
    }
    auto const archives{root / "archives"};
    auto const stats{compress_archive(
            input.string(),
            archives.string(),
            std::nullopt,
            false,
            single_file,
            false,
            std::nullopt,
            separate,
            separate ? 1 : 0
    )};
    REQUIRE(stats.size() == 1);
    return archives / stats[0].get_id();
}

/** Damages only the dictionary's Zstd magic, preserving metadata and the entry count. */
auto corrupt_dictionary(std::filesystem::path const& archive, std::string_view section) -> void {
    auto file{archive};
    size_t offset{sizeof(uint64_t)};
    if (std::filesystem::is_directory(archive)) {
        file += section;
    } else {
        clp_s::ArchiveReaderAdaptor adaptor{{clp_s::InputSource::Filesystem, archive.string()}, {}};
        REQUIRE(adaptor.load_archive_metadata() == clp_s::ErrorCodeSuccess);
        auto reader{adaptor.checkout_reader_for_section(section)};
        offset += reader->get_pos();
        adaptor.checkin_reader_for_section(section);
    }
    std::fstream stream{file, std::ios::in | std::ios::out | std::ios::binary};
    REQUIRE(stream.is_open());
    stream.seekp(static_cast<std::streamoff>(offset));
    stream.put(0);
    REQUIRE(stream.good());
}
}  // namespace

TEST_CASE("Count queries leave unrelated dictionaries unread", "[clp-s][SelectiveDictionaries]") {
    auto const single_file{GENERATE(false, true)};
    auto const separate{GENERATE(false, true)};
    std::filesystem::path const root{"test-selective-dictionaries"};
    TestOutputCleaner cleanup{{root.string()}};
    auto const archive{create_archive(root, single_file, separate)};

    struct Case {
        std::string query;
        uint64_t count;
        std::vector<std::string_view> unread;
    };

    using namespace clp_s::constants;
    for (auto const& test :
         std::vector<Case>{
                 {"seq: 1", 1, {cArchiveVarDictFile, cArchiveLogDictFile, cArchiveArrayDictFile}},
                 {"message: *",
                  2,
                  {cArchiveVarDictFile, cArchiveLogDictFile, cArchiveArrayDictFile}},
                 {"NOT seq: 1",
                  1,
                  {cArchiveVarDictFile, cArchiveLogDictFile, cArchiveArrayDictFile}},
                 {"state: b*", 1, {cArchiveLogDictFile, cArchiveArrayDictFile}},
                 {"message: Alpha*", 1, {cArchiveArrayDictFile}},
                 {"arr: *needle*", 1, {cArchiveLogDictFile}},
                 {"state: b* OR seq: 2", 2, {cArchiveLogDictFile, cArchiveArrayDictFile}},
                 {"NOT (state: b* OR seq: 2)", 0, {cArchiveLogDictFile, cArchiveArrayDictFile}}
         })
    {
        CAPTURE(single_file, separate, test.query);
        auto const trial{root / "trial"};
        std::filesystem::remove_all(trial);
        std::filesystem::create_directory(trial);
        auto const damaged{trial / archive.filename()};
        std::filesystem::copy(archive, damaged, std::filesystem::copy_options::recursive);
        for (auto const section : test.unread) {
            corrupt_dictionary(damaged, section);
        }
        REQUIRE(run_query(open(damaged), test.query) == test.count);
        // Reconstructing the same rows needs the omitted dictionaries and must reject damage.
        REQUIRE_THROWS(run_query(open(damaged), "seq: 1", true));
    }
    REQUIRE(run_query(open(archive), "*: *needle*") == 1);
    REQUIRE(run_query(open(archive), "seq: 1", true) == 1);
    // Reopening an ArchiveReader must not reuse the preceding archive's dictionary-load flags.
    auto reused{open(archive)};
    REQUIRE(run_query(reused, "state: b*") == 1);
    reused->open({clp_s::InputSource::Filesystem, archive.string()}, {});
    REQUIRE(run_query(reused, "message: Alpha*", true) == 1);
}

TEST_CASE(
        "Dictionary reads respect seekable and forward-only inputs",
        "[clp-s][SelectiveDictionaries]"
) {
    std::filesystem::path const root{"test-dictionary-input-modes"};
    TestOutputCleaner cleanup{{root.string()}};
    auto const archive{create_archive(root, true, true)};
    for (auto const* query : {"seq: 1", "state: b*", "message: Alpha*", "arr: *needle*"}) {
        auto stream{std::make_shared<ForwardOnlyReader>(archive.string())};
        auto reader{std::make_shared<clp_s::ArchiveReader>()};
        reader->open(stream, "streaming", {});
        REQUIRE(run_query(reader, query) == 1);
        REQUIRE(stream->backward_seeks == 0);
        auto output_stream{std::make_shared<ForwardOnlyReader>(archive.string())};
        auto output_reader{std::make_shared<clp_s::ArchiveReader>()};
        output_reader->open(output_stream, "streaming-output", {});
        REQUIRE(run_query(output_reader, query, true) == 1);
        REQUIRE(output_stream->backward_seeks == 0);
    }
    // A buffer supports late reads of earlier sections, as a local file does.
    corrupt_dictionary(archive, clp_s::constants::cArchiveLogDictFile);
    std::ifstream input{archive, std::ios::binary};
    std::vector<char> const bytes{std::istreambuf_iterator<char>{input}, {}};
    auto buffer{std::make_shared<clp::BufferReader>(bytes.data(), bytes.size())};
    auto reader{std::make_shared<clp_s::ArchiveReader>()};
    reader->open(buffer, "memory", {});
    REQUIRE(run_query(reader, "state: b*") == 1);
}
