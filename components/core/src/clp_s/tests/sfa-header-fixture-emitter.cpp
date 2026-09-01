#include <bit>
#include <cstddef>
#include <cstdint>
#include <fstream>
#include <iostream>

#include <clp_s/SingleFileArchiveDefs.hpp>

namespace {
constexpr uint64_t cUncompressedSize{0x0102'0304'0506'0708ULL};
constexpr uint64_t cCompressedSize{0x1112'1314'1516'1718ULL};
constexpr uint32_t cMetadataSectionSize{0x2122'2324U};
}  // namespace

auto main(int argc, char const* argv[]) -> int {
    static_assert(std::endian::native == std::endian::little);
    static_assert(sizeof(clp_s::ArchiveHeader) == 64);
    static_assert(offsetof(clp_s::ArchiveHeader, version) == 4);
    static_assert(offsetof(clp_s::ArchiveHeader, uncompressed_size) == 8);
    static_assert(offsetof(clp_s::ArchiveHeader, compressed_size) == 16);
    static_assert(offsetof(clp_s::ArchiveHeader, reserved_padding) == 24);
    static_assert(offsetof(clp_s::ArchiveHeader, metadata_section_size) == 56);
    static_assert(offsetof(clp_s::ArchiveHeader, compression_type) == 60);
    static_assert(offsetof(clp_s::ArchiveHeader, padding) == 62);

    if (2 != argc) {
        std::cerr << "Usage: " << argv[0] << " OUTPUT_PATH\n";
        return 2;
    }

    clp_s::ArchiveHeader const header{
            clp_s::cArchiveVersion,
            cUncompressedSize,
            cCompressedSize,
            cMetadataSectionSize,
            static_cast<uint16_t>(clp_s::ArchiveCompressionType::Zstd)
    };

    std::ofstream output{argv[1], std::ios::binary | std::ios::trunc};
    if (false == output.is_open()) {
        std::cerr << "Failed to open output path: " << argv[1] << '\n';
        return 1;
    }
    output.write(reinterpret_cast<char const*>(&header), sizeof(header));
    if (false == output.good()) {
        std::cerr << "Failed to write output path: " << argv[1] << '\n';
        return 1;
    }
    output.close();
    if (false == output.good()) {
        std::cerr << "Failed to close output path: " << argv[1] << '\n';
        return 1;
    }

    return 0;
}
