#ifndef CLP_S_PACKEDSTREAMREADER_HPP
#define CLP_S_PACKEDSTREAMREADER_HPP

#include <cstddef>
#include <map>
#include <memory>
#include <span>
#include <string>
#include <vector>

#include <ystdlib/error_handling/Result.hpp>

#include "../clp/ReaderInterface.hpp"
#include "ArchiveReaderAdaptor.hpp"
#include "TraceableException.hpp"
#include "ZstdDecompressor.hpp"

namespace clp_s {
/**
 * PackedStreamReader ensures that the tables section of an archive is read safely. Any attempt to
 * read the tables section without loading the tables metadata, and any attempt to read tables
 * section out of order will throw. As well, any incorrect usage of this class (e.g. closing without
 * opening) will throw.
 */
class PackedStreamReader {
public:
    class OperationFailed : public TraceableException {
    public:
        // Constructors
        OperationFailed(ErrorCode error_code, char const* const filename, int line_number)
                : TraceableException(error_code, filename, line_number) {}
    };

    struct PackedStreamMetadata {
        PackedStreamMetadata(size_t offset, size_t size)
                : file_offset(offset),
                  uncompressed_size(size) {}

        size_t file_offset;
        size_t uncompressed_size;
    };

    /**
     * Reads packed stream metadata from the provided compression stream. Must be invoked before
     * reading packed streams.
     * @param decompressor an open ZstdDecompressor pointing to the packed stream metadata
     * @return A void result on success, or an error code indicating the failure:
     * - Forwards `ReaderUtils::try_uint64_to_size_t`'s return values on failure.
     */
    [[nodiscard]] auto read_metadata(ZstdDecompressor& decompressor)
            -> ystdlib::error_handling::Result<void>;

    /** Reads and validates per-column frame extents immediately after stream metadata. */
    auto read_separate_metadata(ZstdDecompressor& decompressor, bool dictionary_extension = false)
            -> void;

    auto get_column_sizes(size_t stream_id) const -> std::span<size_t const> {
        auto const it{m_column_sizes.find(stream_id)};
        return it == m_column_sizes.end() ? std::span<size_t const>{} : it->second;
    }

    auto get_num_separate_streams() const -> size_t { return m_column_sizes.size(); }

    auto is_separate(size_t stream_id) const -> bool { return m_column_sizes.contains(stream_id); }

    /**
     * Opens a file reader for the tables section. Must be invoked before reading packed streams.
     * @param adaptor a reader adaptor for the archive
     */
    void open_packed_streams(std::shared_ptr<ArchiveReaderAdaptor> adaptor);

    /**
     * Closes the file reader for the tables section.
     */
    void close();

    /**
     * Decompresses a stream with a given stream_id and returns it. Streams must be requested in
     * ascending order. A separate-column stream may be read again at the same ID to load columns
     * omitted by an earlier selection. Packed streams cannot be read twice, and moving to an
     * earlier stream is rejected.
     *
     * Note: the buffer and buffer size are returned by reference. This is to support the use case
     * where the caller wants to re-use the same buffer for multiple streams to avoid allocations
     * when they already have a sufficiently large buffer. If no buffer is provided or the provided
     * buffer is too small calling read_stream will create a buffer exactly as large as the stream
     * being decompressed.
     *
     * @param stream_id
     * @param buf a shared ptr to the buffer where the stream will be read. The buffer gets resized
     * if it is too small to contain the requested stream.
     * @param buf_size the size of the underlying buffer owned by buf -- passed and updated by
     * reference
     * @param selection One flag per physical column, or empty to read all columns. Applies only
     * to separate streams; skipped buffer regions remain uninitialized.
     */
    void read_stream(
            size_t stream_id,
            std::shared_ptr<char[]>& buf,
            size_t& buf_size,
            std::span<uint8_t const> selection = {}
    );

    [[nodiscard]] size_t get_uncompressed_stream_size(size_t stream_id) const {
        return m_stream_metadata.at(stream_id).uncompressed_size;
    }

private:
    enum PackedStreamReaderState {
        Uninitialized,
        MetadataRead,
        PackedStreamsOpened,
        ReadingPackedStreams
    };

    std::vector<PackedStreamMetadata> m_stream_metadata;
    std::map<size_t, std::vector<size_t>> m_column_sizes;
    std::map<size_t, std::vector<size_t>> m_column_compressed_sizes;
    std::shared_ptr<ArchiveReaderAdaptor> m_adaptor;
    std::unique_ptr<clp::ReaderInterface> m_packed_stream_reader;
    ZstdDecompressor m_packed_stream_decompressor;
    PackedStreamReaderState m_state{PackedStreamReaderState::Uninitialized};
    size_t m_begin_offset{};
    size_t m_prev_stream_id{0ULL};
};
}  // namespace clp_s

#endif  // CLP_S_PACKEDSTREAMREADER_HPP
