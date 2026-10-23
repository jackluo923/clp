#include <algorithm>
#include <filesystem>
#include <fstream>
#include <iterator>
#include <memory>
#include <optional>
#include <random>
#include <sstream>
#include <string>
#include <vector>

#include <catch2/catch_test_macros.hpp>
#include <catch2/generators/catch_generators.hpp>
#include <nlohmann/json.hpp>
#include <zstd.h>

#include <clp/BufferReader.hpp>
#include <clp_s/archive_constants.hpp>
#include <clp_s/ArchiveReader.hpp>
#include <clp_s/ArchiveReaderAdaptor.hpp>
#include <clp_s/DictionaryReader.hpp>
#include <clp_s/search/ast/SearchUtils.hpp>
#include <clp_s/search/kql/kql.hpp>
#include <clp_s/search/Output.hpp>
#include <clp_s/search/Projection.hpp>
#include <clp_s/search/SchemaMatch.hpp>

#include "../src/clp_s/tests/clp_s_test_utils.hpp"
#include "TestOutputCleaner.hpp"

namespace {
using Range = std::pair<size_t, size_t>;

auto bytes_from(std::filesystem::path const& path) -> std::vector<char> {
    std::ifstream input{path, std::ios::binary};
    REQUIRE(input.is_open());
    return {std::istreambuf_iterator<char>{input}, {}};
}

/** Audits physical reads, including decompressor read-ahead, over a seekable source. */
class AuditedBuffer : public clp::BufferReader {
public:
    explicit AuditedBuffer(std::vector<char> const& bytes)
            : BufferReader{bytes.data(), bytes.size()},
              reads(bytes.size()) {}

    auto try_read(char* data, size_t size, size_t& read) -> clp::ErrorCode override {
        auto const begin{get_pos()};
        auto const end{begin + std::min(size, reads.size() - begin)};
        for (auto const& [first, last] : forbidden) {
            if (first < end && begin < last) {
                throw std::runtime_error{"Search read an unused archive frame"};
            }
        }
        auto const rc{BufferReader::try_read(data, size, read)};
        for (size_t i{begin}; i < begin + read; ++i) {
            ++reads[i];
        }
        return rc;
    }

    std::vector<unsigned> reads;
    std::vector<Range> forbidden;
};

auto section_range(std::filesystem::path const& archive, std::string_view section) -> Range {
    clp_s::ArchiveReaderAdaptor adaptor{{clp_s::InputSource::Filesystem, archive.string()}, {}};
    REQUIRE(adaptor.load_archive_metadata() == clp_s::ErrorCodeSuccess);
    auto reader{adaptor.checkout_reader_for_section(section)};
    auto const begin{reader->get_pos()};
    char buffer[4096];
    size_t read{};
    while (reader->try_read(buffer, sizeof(buffer), read) == clp::ErrorCode_Success) {}
    auto const end{reader->get_pos()};
    adaptor.checkin_reader_for_section(section);
    return {begin, end};
}

auto create_archive(
        std::filesystem::path const& root,
        std::string const& records,
        bool single = true,
        bool separate = true,
        bool adaptive = true,
        bool structured = false,
        bool experimental = false
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
        out << "delimiters: \" \\t\\r\\n\"\nnumber: \"^[0-9]+$\"\n"
               "blockID (0): \"blk_(?<blockNum>[0-9]+)_(?<genStamp>[0-9]+)\"\n";
    }
    auto const output{root / "archives"};
    auto const stats{compress_archive(
            input.string(),
            output.string(),
            "ts",
            true,
            single,
            structured,
            spec,
            adaptive,
            separate ? 1 : 0,
            adaptive
    )};
    REQUIRE(stats.size() == 1);
    return output / stats[0].get_id();
}

auto open(std::filesystem::path const& archive, bool experimental = false)
        -> std::shared_ptr<clp_s::ArchiveReader> {
    auto reader{std::make_shared<clp_s::ArchiveReader>()};
    reader->open(
            {clp_s::InputSource::Filesystem, archive.string()},
            {.m_experimental = experimental}
    );
    return reader;
}

struct Results {
    std::vector<std::string> rows;
    std::vector<int64_t> timestamps;
    std::vector<int64_t> indices;
};

class Sink : public clp_s::search::OutputHandler {
public:
    Sink(Results& result, bool metadata, bool marshal)
            : OutputHandler{metadata, marshal},
              m_result{result} {}

    auto write(std::string_view value) -> void override { m_result.rows.emplace_back(value); }

    auto write(std::string_view value, int64_t timestamp, std::string_view, int64_t index)
            -> void override {
        write(value);
        m_result.timestamps.push_back(timestamp);
        m_result.indices.push_back(index);
    }

private:
    Results& m_result;
};

auto search(
        std::shared_ptr<clp_s::ArchiveReader> const& reader,
        std::string const& query,
        std::vector<std::string> const& projection = {},
        bool metadata = false,
        bool marshal = true
) -> Results {
    if (!projection.empty()) {
        auto selected{std::make_shared<clp_s::search::Projection>(
                clp_s::search::Projection::Mode::ReturnSelectedColumns
        )};
        for (auto const& name : projection) {
            auto value{clp_s::search::kql::parse_projection_column(name)};
            REQUIRE(value != nullptr);
            if (auto column{std::dynamic_pointer_cast<clp_s::search::ast::ColumnDescriptor>(value)})
            {
                selected->add_column(column, clp_s::search::Projection::NodeMask::Mode::Value);
            } else {
                selected->add_column(
                        std::dynamic_pointer_cast<clp_s::search::ast::FunctionCall>(value)
                );
            }
        }
        selected->resolve_columns(*reader->get_schema_tree());
        reader->set_projection(selected);
    }
    std::istringstream input{query};
    auto expr{clp_s::search::kql::parse_kql_expression(input)};
    REQUIRE(expr != nullptr);
    expr = clp_s::search::ast::preprocess_query(expr);
    auto match{std::make_shared<clp_s::search::SchemaMatch>(reader, true)};
    expr = match->run(expr);
    Results result;
    clp_s::search::Output
            output{match, expr, reader, std::make_unique<Sink>(result, metadata, marshal), false};
    REQUIRE(output.filter());
    reader->close();
    return result;
}

auto column_frames(
        std::filesystem::path const& archive,
        std::vector<char> const& bytes,
        bool experimental = false
) -> std::vector<std::pair<std::string, Range>> {
    auto reader{open(archive, experimental)};
    reader->read_dictionaries_and_metadata();
    REQUIRE(reader->get_schema_ids().size() == 1);
    reader->open_packed_streams();
    auto& table{reader->read_schema_table(reader->get_schema_ids()[0], false, false)};
    auto const [begin, end]{section_range(archive, clp_s::constants::cArchiveTablesFile)};
    std::vector<std::pair<std::string, Range>> frames;
    size_t offset{begin};
    for (auto* column : table.get_columns()) {
        auto const size{ZSTD_findFrameCompressedSize(bytes.data() + offset, end - offset)};
        REQUIRE_FALSE(ZSTD_isError(size));
        frames.emplace_back(
                reader->get_schema_tree()->get_node(column->get_id()).get_key_name(),
                Range{offset, offset + size}
        );
        offset += size;
    }
    REQUIRE(offset == end);
    reader->close();
    return frames;
}

auto dictionary_fixture() -> std::string {
    std::ostringstream out;
    // Two matching ERTs use different prefixes; the large dictionary tail belongs to a third.
    for (size_t i{0}; i < 300; ++i) {
        out << R"({"id":1,"value":"early"})" << '\n';
    }
    std::mt19937 random{42};
    for (size_t i{0}; i < 6000; ++i) {
        if (i == 50) {
            out << R"({"id":2,"value":"later","second":true})" << '\n';
        }
        std::string value(128, 'a');
        for (auto& c : value) {
            c = static_cast<char>('a' + random() % 26);
        }
        out << "{\"id\":-1,\"unused\":\"" << value << "\"}\n";
    }
    return out.str();
}
}  // namespace

TEST_CASE("Projected output reads each required column once", "[clp-s][OutputColumns]") {
    auto const adaptive{GENERATE(false, true)};
    std::filesystem::path const root{"test-projected-output-reads"};
    TestOutputCleaner cleanup{{root.string()}};
    auto const archive{create_archive(
            root,
            "{\"ts\":1,\"id\":1,\"keep\":7,\"unused\":\"unused string\"}\n"
            "{\"ts\":3,\"id\":3,\"keep\":9,\"unused\":\"another string\"}\n",
            true,
            true,
            adaptive
    )};
    auto const bytes{bytes_from(archive)};
    auto const frames{column_frames(archive, bytes)};
    for (auto const& query : {"id:1", "id:2", "id:*"}) {
        for (auto const& projected : {"keep", "missing"}) {
            auto source{std::make_shared<AuditedBuffer>(bytes)};
            for (auto const& [name, range] : frames) {
                if (name != "id" && !(name == projected && std::string_view{query} != "id:2")) {
                    source->forbidden.push_back(range);
                }
            }
            for (auto const section :
                 {clp_s::constants::cArchiveVarDictFile,
                  clp_s::constants::cArchiveLogDictFile,
                  clp_s::constants::cArchiveArrayDictFile})
            {
                source->forbidden.push_back(section_range(archive, section));
            }
            auto reader{std::make_shared<clp_s::ArchiveReader>()};
            reader->open(source, "audit", {});
            auto const result{search(reader, query, {projected})};
            auto const count{
                    std::string_view{query} == "id:2"   ? 0U
                    : std::string_view{query} == "id:1" ? 1U
                                                        : 2U
            };
            REQUIRE(result.rows.size() == count);
            for (size_t i{0}; i < result.rows.size(); ++i) {
                REQUIRE(nlohmann::json::parse(result.rows[i])
                        == (std::string_view{projected} == "missing"
                                    ? nlohmann::json::object()
                                    : nlohmann::json{{"keep", i == 0 ? 7 : 9}}));
            }
            auto const [begin, end]{section_range(archive, clp_s::constants::cArchiveTablesFile)};
            REQUIRE(*std::max_element(source->reads.begin() + begin, source->reads.begin() + end)
                    <= 1);
        }
    }
}

TEST_CASE(
        "Output-only variable dictionaries keep their unused tail unread",
        "[clp-s][DictionaryPrefix]"
) {
    std::filesystem::path const root{"test-output-dictionary-prefix"};
    TestOutputCleaner cleanup{{root.string()}};
    auto const archive{create_archive(root, dictionary_fixture())};
    auto const bytes{bytes_from(archive)};
    auto const [begin, end]{section_range(archive, clp_s::constants::cArchiveVarDictFile)};
    REQUIRE(end - begin > 128 * 1024);
    for (bool const projected : {false, true}) {
        auto source{std::make_shared<AuditedBuffer>(bytes)};
        // A Zstd block may consume substantially more compressed input than its requested
        // output prefix. The large dictionary tail must still remain unread.
        source->forbidden.emplace_back(end - 64 * 1024, end);
        auto reader{std::make_shared<clp_s::ArchiveReader>()};
        reader->open(source, "prefix", {});
        auto const result{
                search(reader,
                       "id > 0",
                       projected ? std::vector<std::string>{"value"} : std::vector<std::string>{})
        };
        REQUIRE(result.rows.size() == 301);
        size_t early{}, later{};
        for (auto const& row : result.rows) {
            auto const json = nlohmann::json::parse(row);
            early += json["value"] == "early";
            later += json["value"] == "later";
        }
        REQUIRE(early == 300);
        REQUIRE(later == 1);
        REQUIRE(*std::max_element(source->reads.begin() + begin, source->reads.begin() + end) == 1);
    }
    REQUIRE(search(open(archive), "NOT value:early", {"id"}).rows.size() == 1);
    REQUIRE(search(open(archive), "value:lat*", {"id"}).rows.size() == 1);
}

TEST_CASE(
        "Dictionary prefixes resume across interleaved section reads",
        "[clp-s][DictionaryPrefix]"
) {
    auto const single{GENERATE(false, true)};
    std::filesystem::path const root{"test-dictionary-prefix-resume"};
    TestOutputCleaner cleanup{{root.string()}};
    auto const archive{create_archive(root, dictionary_fixture(), single)};
    clp_s::ArchiveReaderAdaptor adaptor{{clp_s::InputSource::Filesystem, archive.string()}, {}};
    REQUIRE(adaptor.load_archive_metadata() == clp_s::ErrorCodeSuccess);
    clp_s::VariableDictionaryReader dictionary{adaptor};
    dictionary.open(std::string{clp_s::constants::cArchiveVarDictFile});
    dictionary.read_entries_prefix(1);
    REQUIRE(dictionary.get_entries().size() == 1);
    REQUIRE(dictionary.get_value(0) == "early");
    REQUIRE_FALSE(dictionary.is_complete());
    auto table{adaptor.open_independent_reader_for_section(clp_s::constants::cArchiveTablesFile)};
    char byte{};
    size_t read{};
    REQUIRE(table->try_read(&byte, 1, read) == clp::ErrorCode_Success);
    dictionary.read_entries_prefix(53);
    REQUIRE(dictionary.get_entries().size() == 53);
    REQUIRE(dictionary.get_value(51) == "later");
    REQUIRE(table->try_read(&byte, 1, read) == clp::ErrorCode_Success);
    dictionary.read_entries();
    REQUIRE(dictionary.is_complete());
    REQUIRE(dictionary.get_entries().size() == 6002);
    REQUIRE(dictionary.get_value(0) == "early");
    REQUIRE(dictionary.get_value(51) == "later");
    dictionary.close();
}

TEST_CASE("Delayed output preserves arrays, float text and metadata", "[clp-s][OutputColumns]") {
    auto const single{GENERATE(false, true)};
    auto const separate{GENERATE(false, true)};
    auto const structured{GENERATE(false, true)};
    std::filesystem::path const root{"test-delayed-output-values"};
    TestOutputCleaner cleanup{{root.string()}};
    auto const archive{create_archive(
            root,
            R"({"ts":1,"id":1,"obj":{"name":"first"},"arr":["left","right"],"float":1.25000000000000001,"message":"connected to worker-ab12:9876"})"
            "\n"
            R"({"ts":2,"id":2,"obj":{"name":"second"},"arr":["above","below"],"float":-3.12500000000000001,"message":"connected to worker-cd34:4567"})"
            "\n",
            single,
            separate,
            separate,
            structured
    )};
    auto const result{search(open(archive), "id:2", {"obj.name", "arr", "float", "message"}, true)};
    REQUIRE(result.rows.size() == 1);
    REQUIRE(result.rows[0].find("-3.12500000000000001") != std::string::npos);
    auto expected = nlohmann::json::parse(
            R"({"obj":{"name":"second"},"arr":["above","below"],"float":-3.12500000000000001,"message":"connected to worker-cd34:4567"})"
    );
    // Structured-array projection is not supported by the existing resolver. Whole-record
    // output must still load every physical occurrence of its repeated element columns.
    if (structured) {
        expected.erase("arr");
    }
    REQUIRE(nlohmann::json::parse(result.rows[0]) == expected);
    auto const whole{search(open(archive), "id:2")};
    REQUIRE(nlohmann::json::parse(whole.rows.at(0))["arr"] == nlohmann::json({"above", "below"}));
    REQUIRE(result.timestamps == std::vector<int64_t>{2000});
    REQUIRE(result.indices == std::vector<int64_t>{1});
    auto const metadata{search(open(archive), "id:2", {}, true, false)};
    REQUIRE(metadata.rows == std::vector<std::string>{""});
    REQUIRE(metadata.timestamps == result.timestamps);
    REQUIRE(metadata.indices == result.indices);
}

TEST_CASE("Completing a dictionary prefix validates the frame tail", "[clp-s][DictionaryPrefix]") {
    std::filesystem::path const root{"test-dictionary-prefix-validation"};
    TestOutputCleaner cleanup{{root.string()}};
    auto const archive{create_archive(root, "{\"id\":1}\n", false)};
    auto const dictionary_path{archive / "var.dict"};
    auto append_word = [](std::vector<char>& bytes, uint64_t word) {
        for (size_t i{0}; i < 8; ++i) {
            bytes.push_back(static_cast<char>(word >> (8 * i)));
        }
    };
    std::vector<char> payload;
    append_word(payload, 2);
    payload.insert(payload.end(), {'o', 'k'});
    append_word(payload, 256 * 1024);
    std::mt19937 random{71};
    for (size_t i{0}; i < 256 * 1024; ++i) {
        payload.push_back(static_cast<char>(random()));
    }
    auto context{
            std::unique_ptr<ZSTD_CCtx, decltype(&ZSTD_freeCCtx)>{ZSTD_createCCtx(), ZSTD_freeCCtx}
    };
    REQUIRE(context != nullptr);
    REQUIRE_FALSE(ZSTD_isError(ZSTD_CCtx_setParameter(context.get(), ZSTD_c_checksumFlag, 1)));
    for (auto const mode : {"valid", "checksum", "truncated", "trailing", "extra_payload"}) {
        CAPTURE(mode);
        auto raw{payload};
        if (std::string_view{mode} == "extra_payload") {
            raw.push_back(0);
        }
        std::vector<char> compressed(ZSTD_compressBound(raw.size()));
        auto const size{ZSTD_compress2(
                context.get(),
                compressed.data(),
                compressed.size(),
                raw.data(),
                raw.size()
        )};
        REQUIRE_FALSE(ZSTD_isError(size));
        compressed.resize(size);
        if (std::string_view{mode} == "checksum") {
            compressed.back() ^= 1;
        }
        if (std::string_view{mode} == "truncated") {
            compressed.resize(compressed.size() - 2);
        }
        if (std::string_view{mode} == "trailing") {
            compressed.push_back(0);
        }
        std::vector<char> bytes;
        append_word(bytes, 2);
        bytes.insert(bytes.end(), compressed.begin(), compressed.end());
        {
            std::ofstream out{dictionary_path, std::ios::binary};
            out.write(bytes.data(), bytes.size());
        }
        clp_s::ArchiveReaderAdaptor adaptor{{clp_s::InputSource::Filesystem, archive.string()}, {}};
        REQUIRE(adaptor.load_archive_metadata() == clp_s::ErrorCodeSuccess);
        clp_s::VariableDictionaryReader dictionary{adaptor};
        dictionary.open(std::string{clp_s::constants::cArchiveVarDictFile});
        dictionary.read_entries_prefix(1);
        REQUIRE(dictionary.get_value(0) == "ok");
        REQUIRE_FALSE(dictionary.is_complete());
        if (std::string_view{mode} == "valid") {
            dictionary.read_entries();
            REQUIRE(dictionary.is_complete());
            REQUIRE(dictionary.get_value(1).size() == 256 * 1024);
        } else {
            REQUIRE_THROWS(dictionary.read_entries());
            REQUIRE_THROWS(dictionary.read_entries_prefix(1));
        }
        dictionary.close();
    }
}

#if CLP_BUILD_CLPP_DECOMPOSITION
TEST_CASE(
        "CLP+ output plans select reconstruction and shape dependencies",
        "[clpp][OutputColumns]"
) {
    std::filesystem::path const root{"test-clpp-projected-reads"};
    TestOutputCleaner cleanup{{root.string()}};
    std::string const records{
            R"({"id":1,"message":"Received 123 bytes for blk_1_2 and blk_3_4","other":"unrelated 456"})"
            "\n"
            R"({"id":2,"message":"Received 987 bytes for blk_5_6 and blk_7_8","other":"unrelated 654"})"
            "\n"
    };
    auto const packed{create_archive(root / "packed", records, true, false, false, false, true)};
    auto const separate{create_archive(root / "separate", records, true, true, true, false, true)};
    for (auto const& projection :
         std::vector<std::vector<std::string>>{
                 {},
                 {"message"},
                 {"message.number"},
                 {"message.blockID"},
                 {"message.blockID.blockNum"},
                 {"shape(message.blockID)"},
                 {"decompose(message.blockID)"},
                 {"shape(message)"},
                 {"decompose(message)"},
                 {"message", "shape(message)"},
                 {"missing"}
         })
    {
        auto const expected{search(open(packed, true), "id:2", projection)};
        auto const actual{search(open(separate, true), "id:2", projection)};
        REQUIRE(actual.rows == expected.rows);
        REQUIRE(actual.rows.size() == 1);
    }
    auto const value{search(open(separate, true), "id:2", {"message"})};
    REQUIRE(
            nlohmann::json::parse(value.rows[0])
            == nlohmann::json{{"message", {{"text", "Received 987 bytes for blk_5_6 and blk_7_8"}}}}
    );
    auto const bytes{bytes_from(separate)};
    auto source{std::make_shared<AuditedBuffer>(bytes)};
    for (auto const& [name, range] : column_frames(separate, bytes, true)) {
        if (name != "id") {
            source->forbidden.push_back(range);
        }
    }
    source->forbidden.push_back(section_range(separate, clp_s::constants::cArchiveVarDictFile));
    auto reader{std::make_shared<clp_s::ArchiveReader>()};
    reader->open(source, "clpp-shape", {.m_experimental = true});
    REQUIRE(search(reader, "id:2", {"shape(message)"}).rows.size() == 1);
}
#endif
