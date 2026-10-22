#include "PackedStreamReader.hpp"

#include <cstddef>
#include <cstdint>
#include <limits>

#include <ystdlib/error_handling/Result.hpp>

#include "../clp/BoundedReader.hpp"
#include "../clp/ErrorCode.hpp"
#include "archive_constants.hpp"
#include "ArchiveReaderAdaptor.hpp"
#include "ErrorCode.hpp"
#include "ReaderUtils.hpp"
#include "TraceableException.hpp"

namespace clp_s {
auto PackedStreamReader::read_metadata(ZstdDecompressor& decompressor)
        -> ystdlib::error_handling::Result<void> {
    switch (m_state) {
        case PackedStreamReaderState::Uninitialized:
            m_state = PackedStreamReaderState::MetadataRead;
            break;
        default:
            throw OperationFailed(ErrorCodeNotReady, __FILENAME__, __LINE__);
    }

    uint64_t num_streams_u64{0};
    if (auto const error{decompressor.try_read_numeric_value(num_streams_u64)};
        ErrorCodeSuccess != error)
    {
        throw OperationFailed(error, __FILENAME__, __LINE__);
    }

    auto const num_streams{
            YSTDLIB_ERROR_HANDLING_TRYX(ReaderUtils::try_uint64_to_size_t(num_streams_u64))
    };
    m_stream_metadata.reserve(num_streams);

    for (size_t i{0}; i < num_streams; ++i) {
        uint64_t file_offset_u64{0};
        uint64_t uncompressed_size_u64{0};

        if (auto const error{decompressor.try_read_numeric_value(file_offset_u64)};
            ErrorCodeSuccess != error)
        {
            throw OperationFailed(error, __FILENAME__, __LINE__);
        }

        if (auto const error{decompressor.try_read_numeric_value(uncompressed_size_u64)};
            ErrorCodeSuccess != error)
        {
            throw OperationFailed(error, __FILENAME__, __LINE__);
        }

        auto const file_offset{
                YSTDLIB_ERROR_HANDLING_TRYX(ReaderUtils::try_uint64_to_size_t(file_offset_u64))
        };
        auto const uncompressed_size{YSTDLIB_ERROR_HANDLING_TRYX(
                ReaderUtils::try_uint64_to_size_t(uncompressed_size_u64)
        )};
        m_stream_metadata.emplace_back(file_offset, uncompressed_size);
    }
    return ystdlib::error_handling::success();
}

auto PackedStreamReader::read_separate_metadata(ZstdDecompressor& decompressor) -> void {
    auto read_size = [&]() -> size_t {
        uint64_t value{};
        auto const rc{decompressor.try_read_numeric_value(value)};
        if (rc != ErrorCodeSuccess || value > std::numeric_limits<size_t>::max()) {
            throw OperationFailed(ErrorCodeCorrupt, __FILENAME__, __LINE__);
        }
        return static_cast<size_t>(value);
    };
    auto const count{read_size()};
    if (count > m_stream_metadata.size()) {
        throw OperationFailed(ErrorCodeCorrupt, __FILENAME__, __LINE__);
    }
    size_t previous{};
    for (size_t i{0}; i < count; ++i) {
        auto const id{read_size()};
        auto const columns{read_size()};
        if (id >= m_stream_metadata.size() || (i != 0 && id <= previous) || columns == 0) {
            throw OperationFailed(ErrorCodeCorrupt, __FILENAME__, __LINE__);
        }
        previous = id;
        auto& sizes{m_column_sizes[id]};
        auto& compressed_sizes{m_column_compressed_sizes[id]};
        size_t total{};
        size_t compressed_total{};
        for (size_t j{0}; j < columns; ++j) {
            auto const size{read_size()};
            auto const compressed{read_size()};
            if (size > m_stream_metadata[id].uncompressed_size - total
                || compressed > std::numeric_limits<size_t>::max() - compressed_total
                || ((size == 0) != (compressed == 0)))
            {
                throw OperationFailed(ErrorCodeCorrupt, __FILENAME__, __LINE__);
            }
            total += size;
            compressed_total += compressed;
            sizes.push_back(size);
            compressed_sizes.push_back(compressed);
        }
        if (total != m_stream_metadata[id].uncompressed_size
            || (id + 1 < m_stream_metadata.size()
                && (m_stream_metadata[id + 1].file_offset < m_stream_metadata[id].file_offset
                    || compressed_total
                               != m_stream_metadata[id + 1].file_offset
                                          - m_stream_metadata[id].file_offset)))
        {
            throw OperationFailed(ErrorCodeCorrupt, __FILENAME__, __LINE__);
        }
    }
}

void PackedStreamReader::open_packed_streams(std::shared_ptr<ArchiveReaderAdaptor> adaptor) {
    switch (m_state) {
        case PackedStreamReaderState::MetadataRead:
            m_state = PackedStreamReaderState::PackedStreamsOpened;
            break;
        case PackedStreamReaderState::Uninitialized:
        default:
            throw OperationFailed(ErrorCodeNotReady, __FILENAME__, __LINE__);
    }
    m_adaptor = adaptor;
    m_packed_stream_reader = m_adaptor->checkout_reader_for_section(constants::cArchiveTablesFile);
    if (auto rc = m_packed_stream_reader->try_get_pos(m_begin_offset);
        clp::ErrorCode::ErrorCode_Success != rc)
    {
        throw OperationFailed(static_cast<ErrorCode>(rc), __FILENAME__, __LINE__);
    }
}

void PackedStreamReader::close() {
    bool needs_checkin{false};
    switch (m_state) {
        case PackedStreamReaderState::PackedStreamsOpened:
        case PackedStreamReaderState::ReadingPackedStreams:
            needs_checkin = true;
            break;
        default:
            needs_checkin = false;
            break;
    }
    if (needs_checkin) {
        m_adaptor->checkin_reader_for_section(constants::cArchiveTablesFile);
    }
    m_adaptor.reset();
    m_prev_stream_id = 0ULL;
    m_begin_offset = 0ULL;
    m_stream_metadata.clear();
    m_column_sizes.clear();
    m_column_compressed_sizes.clear();
    m_state = PackedStreamReaderState::Uninitialized;
}

void PackedStreamReader::read_stream(
        size_t stream_id,
        std::shared_ptr<char[]>& buf,
        size_t& buf_size,
        std::span<uint8_t const> selection
) {
    constexpr size_t cDecompressorFileReadBufferCapacity = 64 * 1024;  // 64 KiB
    if (stream_id >= m_stream_metadata.size()) {
        throw OperationFailed(ErrorCodeCorrupt, __FILENAME__, __LINE__);
    }

    switch (m_state) {
        case PackedStreamReaderState::PackedStreamsOpened:
            m_state = PackedStreamReaderState::ReadingPackedStreams;
            break;
        case PackedStreamReaderState::ReadingPackedStreams:
            if (m_prev_stream_id > stream_id
                || (m_prev_stream_id == stream_id && !is_separate(stream_id)))
            {
                throw OperationFailed(ErrorCodeBadParam, __FILENAME__, __LINE__);
            }
            break;
        default:
            throw OperationFailed(ErrorCodeNotReady, __FILENAME__, __LINE__);
    }
    m_prev_stream_id = stream_id;

    auto& [file_offset, uncompressed_size] = m_stream_metadata[stream_id];
    if (file_offset > std::numeric_limits<size_t>::max() - m_begin_offset) {
        throw OperationFailed(ErrorCodeCorrupt, __FILENAME__, __LINE__);
    }
    size_t adjusted_file_offset = m_begin_offset + file_offset;
    if (auto error = m_packed_stream_reader->try_seek_from_begin(adjusted_file_offset);
        clp::ErrorCode::ErrorCode_Success != error)
    {
        throw OperationFailed(static_cast<ErrorCode>(error), __FILENAME__, __LINE__);
    }

    auto const end_pos_result{
            ReaderUtils::try_uint64_to_size_t(m_adaptor->get_header().compressed_size)
    };
    if (end_pos_result.has_error()) {
        throw OperationFailed(ErrorCodeOutOfBounds, __FILENAME__, __LINE__);
    }
    size_t end_pos{end_pos_result.value()};

    if ((stream_id + 1) < m_stream_metadata.size()) {
        auto const next{m_stream_metadata[stream_id + 1].file_offset};
        if (next > std::numeric_limits<size_t>::max() - m_begin_offset || next < file_offset) {
            throw OperationFailed(ErrorCodeCorrupt, __FILENAME__, __LINE__);
        }
        end_pos = m_begin_offset + next;
    }
    if (adjusted_file_offset > end_pos) {
        throw OperationFailed(ErrorCodeCorrupt, __FILENAME__, __LINE__);
    }
    clp::BoundedReader bounded_reader{m_packed_stream_reader.get(), end_pos};

    if (buf_size < uncompressed_size) {
        // make_shared is supposed to work here for c++20, but it seems like the compiler version
        // we use doesn't support it, so we convert a unique_ptr to a shared_ptr instead.
        buf = std::make_unique_for_overwrite<char[]>(uncompressed_size);
        buf_size = uncompressed_size;
    }
    if (is_separate(stream_id)) {
        auto const& sizes{m_column_sizes.at(stream_id)};
        auto const& compressed_sizes{m_column_compressed_sizes.at(stream_id)};
        if (!selection.empty() && selection.size() != sizes.size()) {
            throw OperationFailed(ErrorCodeBadParam, __FILENAME__, __LINE__);
        }
        size_t output_offset{};
        size_t input_offset{adjusted_file_offset};
        for (size_t i{0}; i < sizes.size(); ++i) {
            if (compressed_sizes[i] > end_pos - input_offset) {
                throw OperationFailed(ErrorCodeCorrupt, __FILENAME__, __LINE__);
            }
            auto const frame_end{input_offset + compressed_sizes[i]};
            if (sizes[i] != 0 && (selection.empty() || selection[i])) {
                m_packed_stream_reader->seek_from_begin(input_offset);
                clp::BoundedReader frame_reader{m_packed_stream_reader.get(), frame_end};
                m_packed_stream_decompressor.open(
                        frame_reader,
                        cDecompressorFileReadBufferCapacity
                );
                auto const rc{m_packed_stream_decompressor
                                      .try_read_exact_length(buf.get() + output_offset, sizes[i])};
                if (rc != ErrorCodeSuccess) {
                    throw OperationFailed(rc, __FILENAME__, __LINE__);
                }
                char extra{};
                size_t read{};
                auto const tail{m_packed_stream_decompressor.try_read(&extra, 1, read)};
                if (read != 0 || (tail != ErrorCodeSuccess && tail != ErrorCodeEndOfFile)) {
                    throw OperationFailed(ErrorCodeCorrupt, __FILENAME__, __LINE__);
                }
                m_packed_stream_decompressor.close_for_reuse();
            }
            input_offset = frame_end;
            output_offset += sizes[i];
        }
        return;
    }
    m_packed_stream_decompressor.open(bounded_reader, cDecompressorFileReadBufferCapacity);
    if (auto error
        = m_packed_stream_decompressor.try_read_exact_length(buf.get(), uncompressed_size);
        ErrorCodeSuccess != error)
    {
        throw OperationFailed(error, __FILENAME__, __LINE__);
    }
    m_packed_stream_decompressor.close_for_reuse();
}
}  // namespace clp_s
