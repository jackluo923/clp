#ifndef CLP_S_SEARCH_EMBEDDINGCACHE_HPP
#define CLP_S_SEARCH_EMBEDDINGCACHE_HPP

#include <cstddef>
#include <cstdint>
#include <string>
#include <vector>

#include "../../clp/SQLiteDB.hpp"

namespace clp_s::search {
/**
 * SQLite-backed on-disk cache for logtype embeddings. Stores embedding vectors
 * keyed by individual logtype hashes (FNV-1a of cleaned logtype string), enabling
 * per-logtype reuse across queries, archives, and process invocations.
 *
 * The cache is stored at /tmp/clp/{model_name}/embedding_cache.db, shared by
 * all clp-s instances on the same machine using the same model.
 *
 * Concurrency: uses WAL mode for concurrent readers. Writers use BEGIN IMMEDIATE
 * and silently skip on SQLITE_BUSY (another process will populate the cache).
 */
class EmbeddingCache {
public:
    /**
     * Opens (or creates) the shared cache database at
     * /tmp/clp/{model_name}/embedding_cache.db. Sets WAL mode and
     * SYNCHRONOUS=NORMAL. If the directory can't be created or isn't writable,
     * disables caching (all operations become no-ops).
     * @param model_name Model basename (e.g. "bge-small-en-v1.5")
     */
    explicit EmbeddingCache(std::string const& model_name);

    /**
     * Looks up cached embeddings for individual logtypes.
     * @param logtype_hashes FNV-1a hash of each cleaned logtype string
     * @param embedding_dim Dimension of each embedding vector
     * @return Vector where result[i] is non-empty if cached, empty if missed
     */
    [[nodiscard]] std::vector<std::vector<float>> try_load_batch(
            std::vector<uint64_t> const& logtype_hashes,
            size_t embedding_dim
    ) const;

    /**
     * Stores individual logtype embeddings. On SQLITE_BUSY, silently
     * skips — another writer will populate the cache.
     * @param logtype_hashes FNV-1a hash of each cleaned logtype string
     * @param embeddings embeddings[i] is the embedding for logtype_hashes[i]
     */
    void store_batch(
            std::vector<uint64_t> const& logtype_hashes,
            std::vector<std::vector<float>> const& embeddings
    );

private:
    bool m_enabled{false};
    mutable clp::SQLiteDB m_db;
};

/**
 * Computes FNV-1a 64-bit hash of a single cleaned logtype string.
 * @param logtype Cleaned logtype string
 * @return 64-bit hash
 */
uint64_t fnv1a_hash(std::string const& logtype);
}  // namespace clp_s::search

#endif  // CLP_S_SEARCH_EMBEDDINGCACHE_HPP
