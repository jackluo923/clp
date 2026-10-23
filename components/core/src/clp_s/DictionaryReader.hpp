// Code from CLP

#ifndef CLP_S_DICTIONARYREADER_HPP
#define CLP_S_DICTIONARYREADER_HPP

#include <iterator>
#include <limits>
#include <memory>
#include <string>
#include <string_view>
#include <unordered_set>
#include <utility>

#include <boost/algorithm/string/case_conv.hpp>
#include <string_utils/string_utils.hpp>

#include <clp/Defs.h>
#include <clp_s/ArchiveReaderAdaptor.hpp>
#include <clp_s/DictionaryEntry.hpp>
#include <clpp/Defs.hpp>

namespace clp_s {
template <typename DictionaryIdType, typename EntryType>
class DictionaryReader {
public:
    // Types
    class OperationFailed : public TraceableException {
    public:
        // Constructors
        OperationFailed(ErrorCode error_code, char const* const filename, int line_number)
                : TraceableException(error_code, filename, line_number) {}
    };

    using dictionary_id_t = DictionaryIdType;
    using Entry = EntryType;

    // Constructors
    DictionaryReader(ArchiveReaderAdaptor& adaptor) : m_is_open(false), m_adaptor(adaptor) {}

    // Methods
    /**
     * Opens dictionary for reading
     * @param dictionary_path
     */
    void open(std::string const& dictionary_path);

    /**
     * Closes the dictionary
     */
    void close();

    /**
     * Reads all entries from disk
     */
    void read_entries(bool lazy = false);

    /**
     * Loads at least this many leading entries, resuming the decoder on later calls.
     * Missing IDs in a partial dictionary are not evidence of absence. Predicate preparation
     * must call read_entries(). Extension can invalidate entry pointers and references.
     * Forward-only sources load the complete dictionary. The unread tail is validated on demand.
     */
    auto read_entries_prefix(uint64_t minimum_entries, bool lazy = true) -> void;

    [[nodiscard]] auto is_complete() const -> bool { return m_complete; }

    /**
     * @return Loaded dictionary entries. Check is_complete() before treating missing IDs as absent.
     */
    std::vector<EntryType> const& get_entries() const { return m_entries; }

    /**
     * @param id
     * @return The entry with the given ID
     */
    EntryType& get_entry(DictionaryIdType id);

    /**
     * @param id
     * @return Value of the entry with the specified ID
     */
    std::string const& get_value(DictionaryIdType id) const;

    /**
     * Gets the entries matching the given search string
     * @param search_string
     * @param ignore_case
     * @return a vector of matching entries, or an empty vector if no entry matches.
     */
    std::vector<EntryType const*>
    get_entry_matching_value(std::string_view search_string, bool ignore_case) const;

    /**
     * Gets the entries that match a given wildcard string
     * @param wildcard_string
     * @param ignore_case
     * @param entries Set in which to store found entries
     */
    void get_entries_matching_wildcard_string(
            std::string_view wildcard_string,
            bool ignore_case,
            std::unordered_set<EntryType const*>& entries
    ) const;

protected:
    bool m_is_open;
    ArchiveReaderAdaptor& m_adaptor;
    std::string m_dictionary_path;
    ZstdDecompressor m_dictionary_decompressor;
    std::vector<EntryType> m_entries;

private:
    std::unique_ptr<clp::ReaderInterface> m_prefix_reader;
    uint64_t m_declared_entries{};
    bool m_complete{false};
    bool m_failed{false};
};

using LogShapeDictionaryReader = DictionaryReader<clpp::log_shape_id_t, VariableDictionaryEntry>;
using VariableDictionaryReader
        = DictionaryReader<clp::variable_dictionary_id_t, VariableDictionaryEntry>;
using LogTypeDictionaryReader
        = DictionaryReader<clp::logtype_dictionary_id_t, LogTypeDictionaryEntry>;

template <typename DictionaryIdType, typename EntryType>
void DictionaryReader<DictionaryIdType, EntryType>::open(std::string const& dictionary_path) {
    if (m_is_open) {
        throw OperationFailed(ErrorCodeNotReady, __FILENAME__, __LINE__);
    }

    m_dictionary_path = dictionary_path;
    m_entries.clear();
    m_complete = false;
    m_failed = false;
    m_is_open = true;
}

template <typename DictionaryIdType, typename EntryType>
void DictionaryReader<DictionaryIdType, EntryType>::close() {
    if (false == m_is_open) {
        throw OperationFailed(ErrorCodeNotReady, __FILENAME__, __LINE__);
    }
    m_is_open = false;
    m_dictionary_decompressor.close();
    m_prefix_reader.reset();
}

template <typename DictionaryIdType, typename EntryType>
void DictionaryReader<DictionaryIdType, EntryType>::read_entries(bool lazy) {
    if (false == m_is_open) {
        throw OperationFailed(ErrorCodeNotInit, __FILENAME__, __LINE__);
    }
    if (m_failed) {
        throw OperationFailed(ErrorCodeCorrupt, __FILENAME__, __LINE__);
    }
    if (m_complete) {
        return;
    }
    if (m_prefix_reader) {
        read_entries_prefix(std::numeric_limits<uint64_t>::max(), lazy);
        return;
    }

    constexpr size_t cDecompressorFileReadBufferCapacity = 64 * 1024;  // 64 KiB
    auto dictionary_reader = m_adaptor.checkout_reader_for_section(m_dictionary_path);

    uint64_t num_dictionary_entries;
    dictionary_reader->read_numeric_value(num_dictionary_entries, false);
    m_dictionary_decompressor.open(*dictionary_reader, cDecompressorFileReadBufferCapacity);

    // Read dictionary entries
    m_entries.resize(num_dictionary_entries);
    for (size_t i = 0; i < num_dictionary_entries; ++i) {
        auto& entry = m_entries[i];
        entry.read_from_file(m_dictionary_decompressor, i, lazy);
    }

    m_dictionary_decompressor.close();
    m_adaptor.checkin_reader_for_section(m_dictionary_path);
    m_complete = true;
}

template <typename DictionaryIdType, typename EntryType>
auto DictionaryReader<DictionaryIdType, EntryType>::read_entries_prefix(
        uint64_t minimum_entries,
        bool lazy
) -> void {
    if (!m_is_open || m_failed) {
        throw OperationFailed(ErrorCodeNotReady, __FILENAME__, __LINE__);
    }
    if (m_complete || minimum_entries <= m_entries.size()) {
        return;
    }
    if (m_adaptor.requires_ordered_section_reads()) {
        read_entries(lazy);
        return;
    }
    try {
        if (!m_prefix_reader) {
            m_prefix_reader = m_adaptor.open_independent_reader_for_section(m_dictionary_path);
            m_prefix_reader->read_numeric_value(m_declared_entries, false);
            if (m_declared_entries > m_entries.max_size()) {
                throw OperationFailed(ErrorCodeCorrupt, __FILENAME__, __LINE__);
            }
            m_dictionary_decompressor.open(
                    *m_prefix_reader,
                    minimum_entries >= m_declared_entries ? 64 * 1024 : 16 * 1024
            );
        }
        auto const requested{std::min(minimum_entries, m_declared_entries)};
        auto const previous{m_entries.size()};
        // Allocate the known prefix in one step, as the full reader does. Growing one entry at
        // a time repeatedly moves the entry table when output needs most of a large dictionary.
        m_entries.resize(requested);
        for (size_t i{previous}; i < requested; ++i) {
            m_entries[i].read_from_file(m_dictionary_decompressor, i, lazy);
        }
        if (m_entries.size() == m_declared_entries) {
            auto const rc{m_dictionary_decompressor.try_finish_frame()};
            if (rc != ErrorCodeSuccess) {
                throw OperationFailed(rc, __FILENAME__, __LINE__);
            }
            m_dictionary_decompressor.close();
            m_prefix_reader.reset();
            m_complete = true;
        }
    } catch (...) {
        m_dictionary_decompressor.close();
        m_prefix_reader.reset();
        m_failed = true;
        throw;
    }
}

template <typename DictionaryIdType, typename EntryType>
EntryType& DictionaryReader<DictionaryIdType, EntryType>::get_entry(DictionaryIdType id) {
    if (false == m_is_open) {
        throw OperationFailed(ErrorCodeNotInit, __FILENAME__, __LINE__);
    }
    if (id >= m_entries.size()) {
        throw OperationFailed(ErrorCodeBadParam, __FILENAME__, __LINE__);
    }

    return m_entries[id];
}

template <typename DictionaryIdType, typename EntryType>
std::string const&
DictionaryReader<DictionaryIdType, EntryType>::get_value(DictionaryIdType id) const {
    if (id >= m_entries.size()) {
        throw OperationFailed(ErrorCodeCorrupt, __FILENAME__, __LINE__);
    }
    return m_entries[id].get_value();
}

template <typename DictionaryIdType, typename EntryType>
std::vector<EntryType const*>
DictionaryReader<DictionaryIdType, EntryType>::get_entry_matching_value(
        std::string_view search_string,
        bool ignore_case
) const {
    if (false == ignore_case) {
        // In case-sensitive match, there can be only one matched entry.
        if (auto const it = std::ranges::find_if(
                    m_entries,
                    [&](auto const& entry) { return entry.get_value() == search_string; }
            );
            m_entries.cend() != it)
        {
            return {&(*it)};
        }
        return {};
    }

    std::vector<EntryType const*> entries;
    std::string search_string_uppercase;
    std::ignore = boost::algorithm::to_upper_copy(
            std::back_inserter(search_string_uppercase),
            search_string
    );
    for (auto const& entry : m_entries) {
        if (boost::algorithm::to_upper_copy(entry.get_value()) == search_string_uppercase) {
            entries.push_back(&entry);
        }
    }
    return entries;
}

template <typename DictionaryIdType, typename EntryType>
void DictionaryReader<DictionaryIdType, EntryType>::get_entries_matching_wildcard_string(
        std::string_view wildcard_string,
        bool ignore_case,
        std::unordered_set<EntryType const*>& entries
) const {
    for (auto const& entry : m_entries) {
        if (clp::string_utils::wildcard_match_unsafe(
                    entry.get_value(),
                    wildcard_string,
                    !ignore_case
            ))
        {
            entries.insert(&entry);
        }
    }
}
}  // namespace clp_s

#endif  // CLP_S_DICTIONARYREADER_HPP
