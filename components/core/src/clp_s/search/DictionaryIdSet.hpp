#ifndef CLP_S_SEARCH_DICTIONARYIDSET_HPP
#define CLP_S_SEARCH_DICTIONARYIDSET_HPP

#include <algorithm>
#include <array>
#include <cstdint>
#include <unordered_set>
#include <vector>

namespace clp_s::search {
/** Chooses an exact membership representation once, before scanning a dictionary-ID column. */
class DictionaryIdSet {
public:
    explicit DictionaryIdSet(std::unordered_set<int64_t> const& ids) : m_ids{ids} {
        if (ids.size() <= m_small.size()) {
            std::copy(ids.begin(), ids.end(), m_small.begin());
            return;
        }
        auto const [min, max]{std::minmax_element(ids.begin(), ids.end())};
        m_min = *min;
        auto const span{static_cast<uint64_t>(*max) - static_cast<uint64_t>(*min)};
        // Cap at 128 KiB and avoid paying for a sparse bitmap.
        if (span >= (1U << 20) || span / 64 >= ids.size() * 4) {
            return;
        }
        m_bits.resize(span / 64 + 1);
        for (auto id : ids) {
            auto const index{static_cast<uint64_t>(id) - static_cast<uint64_t>(m_min)};
            m_bits[index / 64] |= uint64_t{1} << (index % 64);
        }
    }

    [[nodiscard]] auto contains(int64_t id) const -> bool {
        if (m_ids.size() <= m_small.size()) {
            return std::find(m_small.begin(), m_small.begin() + m_ids.size(), id)
                   != m_small.begin() + m_ids.size();
        }
        if (m_bits.empty()) {
            return m_ids.contains(id);
        }
        auto const index{static_cast<uint64_t>(id) - static_cast<uint64_t>(m_min)};
        return id >= m_min && index / 64 < m_bits.size()
               && (m_bits[index / 64] & (uint64_t{1} << (index % 64))) != 0;
    }

private:
    std::unordered_set<int64_t> const& m_ids;
    std::array<int64_t, 4> m_small{};
    int64_t m_min{};
    std::vector<uint64_t> m_bits;
};
}  // namespace clp_s::search
#endif  // CLP_S_SEARCH_DICTIONARYIDSET_HPP
