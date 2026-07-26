#include <cstddef>
#include <filesystem>
#include <fstream>
#include <string>
#include <string_view>
#include <vector>

#include <catch2/catch_test_macros.hpp>
#include <catch2/generators/catch_generators.hpp>
#include <nlohmann/json.hpp>

#include "../src/clp_s/InputConfig.hpp"
#include "../src/clp_s/JsonConstructor.hpp"
#include "../src/clp_s/JsonParser.hpp"
#include "TestOutputCleaner.hpp"

constexpr std::string_view cTestFileHierarchyInputDirectory{"test-file-hierarchy-input"};
constexpr std::string_view cTestFileHierarchyArchiveDirectory{"test-file-hierarchy-archive"};
constexpr std::string_view cTestFileHierarchyOutputDirectory{"test-file-hierarchy-output"};

namespace {
/**
 * Compresses several files into one archive directory so that the archive's range index contains
 * one file range per input.
 * @param file_paths
 * @param archive_directory
 * @param single_file_archive
 * @param record_log_order
 */
void compress_files(
        std::vector<std::string> const& file_paths,
        std::string const& archive_directory,
        bool single_file_archive,
        bool record_log_order = true
) {
    constexpr auto cDefaultTargetEncodedSize{8ULL * 1024 * 1024 * 1024};  // 8 GiB
    constexpr auto cDefaultMaxDocumentSize{512ULL * 1024 * 1024};  // 512 MiB
    constexpr auto cDefaultMinTableSize{1ULL * 1024 * 1024};  // 1 MiB
    constexpr auto cDefaultCompressionLevel{3};

    std::filesystem::create_directory(archive_directory);
    REQUIRE(std::filesystem::is_directory(archive_directory));

    clp_s::JsonParserOption parser_option{};
    for (auto const& file_path : file_paths) {
        parser_option.input_paths_and_canonical_filenames.emplace_back(
                clp_s::Path{.source = clp_s::InputSource::Filesystem, .path = file_path},
                file_path
        );
    }
    parser_option.archives_dir = archive_directory;
    parser_option.target_encoded_size = cDefaultTargetEncodedSize;
    parser_option.max_document_size = cDefaultMaxDocumentSize;
    parser_option.min_table_size = cDefaultMinTableSize;
    parser_option.compression_level = cDefaultCompressionLevel;
    parser_option.single_file_archive = single_file_archive;
    parser_option.record_log_order = record_log_order;

    clp_s::JsonParser parser{parser_option};
    REQUIRE(parser.ingest());
    REQUIRE_NOTHROW(parser.store());
    REQUIRE(false == std::filesystem::is_empty(archive_directory));
}

auto write_test_file(std::filesystem::path const& path, std::vector<std::string> const& lines)
        -> void {
    std::filesystem::create_directories(path.parent_path());
    std::ofstream out{path};
    REQUIRE(out.is_open());
    for (auto const& line : lines) {
        out << line << "\n";
    }
}

/**
 * Parses every line of a JSONL file.
 * @param path
 * @return The parsed records in file order.
 */
auto parse_jsonl_file(std::filesystem::path const& path) -> std::vector<nlohmann::json> {
    std::ifstream in{path};
    REQUIRE(in.is_open());
    std::vector<nlohmann::json> records;
    std::string line;
    while (std::getline(in, line)) {
        if (line.empty()) {
            continue;
        }
        records.push_back(nlohmann::json::parse(line));
    }
    return records;
}
}  // namespace

TEST_CASE("clp-s-file-hierarchy-decompression", "[clp-s][file-hierarchy]") {
    auto single_file_archive = GENERATE(true, false);

    TestOutputCleaner const test_cleanup{
            {std::string{cTestFileHierarchyInputDirectory},
             std::string{cTestFileHierarchyArchiveDirectory},
             std::string{cTestFileHierarchyOutputDirectory}}
    };

    // Two input files at different depths so extraction has to recreate nested directories.
    auto const input_dir = std::filesystem::path{cTestFileHierarchyInputDirectory};
    auto const first_file = input_dir / "nested" / "first.jsonl";
    auto const second_file = input_dir / "second.jsonl";
    write_test_file(
            first_file,
            {R"({"file": "first", "idx": 0, "msg": "hello world"})",
             R"({"file": "first", "idx": 1, "msg": "second record"})",
             R"({"file": "first", "idx": 2, "msg": "third record"})"}
    );
    write_test_file(
            second_file,
            {R"({"file": "second", "idx": 0, "flag": true})",
             R"({"file": "second", "idx": 1, "flag": false})"}
    );

    compress_files(
            {first_file.string(), second_file.string()},
            std::string{cTestFileHierarchyArchiveDirectory},
            single_file_archive
    );

    std::filesystem::create_directory(cTestFileHierarchyOutputDirectory);
    for (auto const& entry :
         std::filesystem::directory_iterator(std::string{cTestFileHierarchyArchiveDirectory}))
    {
        clp_s::JsonConstructorOption option{};
        option.archive_path = clp_s::Path{
                .source{clp_s::InputSource::Filesystem},
                .path{entry.path().string()}
        };
        option.output_dir = std::string{cTestFileHierarchyOutputDirectory};
        option.file_hierarchy = true;
        clp_s::JsonConstructor constructor{option};
        constructor.store();
    }

    // Every input file must be restored at its original relative path with the same records, and
    // nothing else (in particular no JSONL chunk files) may be emitted.
    auto const output_dir = std::filesystem::path{cTestFileHierarchyOutputDirectory};
    size_t num_output_files{0};
    for (auto const& entry : std::filesystem::recursive_directory_iterator(output_dir)) {
        if (entry.is_regular_file()) {
            ++num_output_files;
        }
    }
    REQUIRE(2ULL == num_output_files);

    for (auto const& input_file : {first_file, second_file}) {
        auto const restored_file = output_dir / input_file;
        REQUIRE(std::filesystem::exists(restored_file));
        REQUIRE(parse_jsonl_file(input_file) == parse_jsonl_file(restored_file));
    }
}

TEST_CASE(
        "clp-s-file-hierarchy-decompression-requires-log-order",
        "[clp-s][file-hierarchy]"
) {
    TestOutputCleaner const test_cleanup{
            {std::string{cTestFileHierarchyInputDirectory},
             std::string{cTestFileHierarchyArchiveDirectory},
             std::string{cTestFileHierarchyOutputDirectory}}
    };

    auto const input_file
            = std::filesystem::path{cTestFileHierarchyInputDirectory} / "input.jsonl";
    write_test_file(input_file, {R"({"idx": 0})", R"({"idx": 1})"});
    compress_files(
            {input_file.string()},
            std::string{cTestFileHierarchyArchiveDirectory},
            false,
            false
    );

    // An archive compressed without log order can't attribute records to files, so extraction
    // must fail rather than silently produce something other than the requested hierarchy.
    std::filesystem::create_directory(cTestFileHierarchyOutputDirectory);
    for (auto const& entry :
         std::filesystem::directory_iterator(std::string{cTestFileHierarchyArchiveDirectory}))
    {
        clp_s::JsonConstructorOption option{};
        option.archive_path = clp_s::Path{
                .source{clp_s::InputSource::Filesystem},
                .path{entry.path().string()}
        };
        option.output_dir = std::string{cTestFileHierarchyOutputDirectory};
        option.file_hierarchy = true;
        clp_s::JsonConstructor constructor{option};
        REQUIRE_THROWS_AS(constructor.store(), clp_s::JsonConstructor::OperationFailed);
    }
}
