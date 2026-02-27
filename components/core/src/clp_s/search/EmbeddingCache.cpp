#include "EmbeddingCache.hpp"

#include <algorithm>
#include <cstddef>
#include <cstdint>
#include <cstring>
#include <filesystem>
#include <string>
#include <unordered_map>
#include <vector>

#include <spdlog/spdlog.h>

#include "../../clp/ErrorCode.hpp"

namespace clp_s::search {
namespace {
constexpr char const* cCacheDir = "/tmp/clp";
constexpr char const* cCacheFileName = "embedding_cache.db";

// Schema version — bump when the table layout changes so that stale caches
// (which live in /tmp) are transparently recreated.
constexpr int cSchemaVersion = 3;

constexpr char const* cCreateTableSQL
        = "CREATE TABLE IF NOT EXISTS embedding_cache ("
          "logtype_hash BLOB    PRIMARY KEY,"
          "embedding    BLOB    NOT NULL"
          ") WITHOUT ROWID";

// 2 params per row × 200 rows = 400 params, well within SQLite's 999 default limit
constexpr size_t cInsertBatchSize = 200;

// Max placeholders for SELECT ... WHERE logtype_hash IN (?, ?, ...)
constexpr size_t cSelectBatchSize = 500;

/**
 * Builds a batch SELECT: SELECT logtype_hash, embedding FROM embedding_cache
 * WHERE logtype_hash IN (?, ?, ...)
 * @param num_keys Number of placeholders
 */
std::string build_batch_select_sql(size_t num_keys) {
    std::string sql
            = "SELECT logtype_hash, embedding FROM embedding_cache"
              " WHERE logtype_hash IN (";
    for (size_t i = 0; i < num_keys; ++i) {
        if (i > 0) {
            sql += ',';
        }
        sql += '?';
    }
    sql += ')';
    return sql;
}

/**
 * Builds a multi-row INSERT statement: INSERT OR REPLACE INTO embedding_cache
 * (logtype_hash, embedding) VALUES (?,?), (?,?), ...
 * @param num_rows Number of value tuples
 */
std::string build_batch_insert_sql(size_t num_rows) {
    std::string sql = "INSERT OR REPLACE INTO embedding_cache"
                      " (logtype_hash, embedding) VALUES ";
    for (size_t i = 0; i < num_rows; ++i) {
        if (i > 0) {
            sql += ',';
        }
        sql += "(?,?)";
    }
    return sql;
}
}  // namespace

EmbeddingCache::EmbeddingCache(std::string const& model_name) {
    auto const cache_dir = std::filesystem::path(cCacheDir) / model_name;
    std::error_code ec;
    std::filesystem::create_directories(cache_dir, ec);
    if (ec) {
        SPDLOG_WARN("Embedding cache disabled: can't create '{}'", cache_dir.string());
        return;
    }
    auto const cache_path = cache_dir / cCacheFileName;

    try {
        m_db.open(cache_path.string());
    } catch (clp::SQLiteDB::OperationFailed const& e) {
        SPDLOG_WARN(
                "Embedding cache disabled: failed to open '{}' - {}",
                cache_path.string(),
                e.what()
        );
        return;
    }

    try {
        // page_size must be set before journal_mode=WAL, which writes the WAL
        // header and permanently fixes the page size for existing databases.
        // 64 KB pages reduce B-tree height for faster lookups on larger datasets.
        auto pragma_page = m_db.prepare_statement("PRAGMA page_size=65536");
        pragma_page.step();

        auto pragma_wal = m_db.prepare_statement("PRAGMA journal_mode=WAL");
        pragma_wal.step();

        auto pragma_sync = m_db.prepare_statement("PRAGMA synchronous=NORMAL");
        pragma_sync.step();

        // Memory-map up to 1 GB of the database file for faster reads.
        auto pragma_mmap = m_db.prepare_statement("PRAGMA mmap_size=1073741824");
        pragma_mmap.step();

        // Migrate stale schema: if user_version doesn't match, drop and recreate.
        auto version_stmt = m_db.prepare_statement("PRAGMA user_version");
        version_stmt.step();
        int const current_version = version_stmt.column_int(0);
        if (current_version != cSchemaVersion) {
            auto drop_stmt = m_db.prepare_statement("DROP TABLE IF EXISTS embedding_cache");
            drop_stmt.step();
        }

        auto create_table = m_db.prepare_statement(cCreateTableSQL);
        create_table.step();

        if (current_version != cSchemaVersion) {
            auto set_version = m_db.prepare_statement(
                    "PRAGMA user_version=" + std::to_string(cSchemaVersion)
            );
            set_version.step();
        }

        m_enabled = true;
    } catch (clp::SQLitePreparedStatement::OperationFailed const& e) {
        SPDLOG_WARN("Embedding cache disabled: setup failed - {}", e.what());
        m_db.close();
    }
}

std::vector<std::vector<float>> EmbeddingCache::try_load_batch(
        std::vector<uint64_t> const& logtype_hashes,
        size_t embedding_dim
) const {
    size_t const total = logtype_hashes.size();
    std::vector<std::vector<float>> result(total);

    if (false == m_enabled || 0 == total) {
        return result;
    }

    size_t const expected_blob_size = embedding_dim * sizeof(float);

    try {
        // Build a map from hash → list of indices (handles duplicate hashes)
        std::unordered_map<uint64_t, std::vector<size_t>> hash_to_indices;
        hash_to_indices.reserve(total);
        for (size_t i = 0; i < total; ++i) {
            hash_to_indices[logtype_hashes[i]].push_back(i);
        }

        // Collect unique hashes for querying
        std::vector<uint64_t> unique_hashes;
        unique_hashes.reserve(hash_to_indices.size());
        for (auto const& [hash, _] : hash_to_indices) {
            unique_hashes.push_back(hash);
        }

        // Query in batches to stay within SQLite parameter limits
        for (size_t batch_start = 0; batch_start < unique_hashes.size();
             batch_start += cSelectBatchSize)
        {
            size_t const batch_end
                    = std::min(batch_start + cSelectBatchSize, unique_hashes.size());
            size_t const batch_size = batch_end - batch_start;

            auto const sql = build_batch_select_sql(batch_size);
            auto stmt = m_db.prepare_statement(sql);

            for (size_t i = 0; i < batch_size; ++i) {
                stmt.bind_blob(
                        static_cast<int>(i + 1),
                        &unique_hashes[batch_start + i],
                        sizeof(uint64_t),
                        false
                );
            }

            while (stmt.step()) {
                // Column 0: logtype_hash
                int const hash_blob_size = stmt.column_blob_size(0);
                if (static_cast<size_t>(hash_blob_size) != sizeof(uint64_t)) {
                    continue;
                }
                uint64_t row_hash{};
                std::memcpy(&row_hash, stmt.column_blob(0), sizeof(uint64_t));

                // Column 1: embedding
                int const emb_blob_size = stmt.column_blob_size(1);
                if (static_cast<size_t>(emb_blob_size) != expected_blob_size) {
                    SPDLOG_WARN(
                            "Embedding cache: unexpected blob size {} (expected {})",
                            emb_blob_size,
                            expected_blob_size
                    );
                    continue;
                }

                void const* emb_data = stmt.column_blob(1);
                if (nullptr == emb_data) {
                    continue;
                }

                // Fill all indices that share this hash
                auto it = hash_to_indices.find(row_hash);
                if (it != hash_to_indices.end()) {
                    for (size_t idx : it->second) {
                        result[idx].resize(embedding_dim);
                        std::memcpy(result[idx].data(), emb_data, expected_blob_size);
                    }
                }
            }
        }

        size_t hits = 0;
        for (auto const& emb : result) {
            if (false == emb.empty()) {
                ++hits;
            }
        }
        SPDLOG_DEBUG("Embedding cache: {}/{} logtype hits", hits, total);
    } catch (clp::SQLitePreparedStatement::OperationFailed const& e) {
        SPDLOG_WARN("Embedding cache read failed - {}", e.what());
    }

    return result;
}

void EmbeddingCache::store_batch(
        std::vector<uint64_t> const& logtype_hashes,
        std::vector<std::vector<float>> const& embeddings
) {
    if (false == m_enabled || logtype_hashes.empty()) {
        return;
    }

    try {
        // BEGIN IMMEDIATE to fail fast if another writer holds the lock
        auto begin_stmt = m_db.prepare_statement("BEGIN IMMEDIATE");
        begin_stmt.step();
    } catch (clp::SQLitePreparedStatement::OperationFailed const& e) {
        // SQLITE_BUSY — another process is writing; skip caching
        SPDLOG_DEBUG("Embedding cache: skipping store (another writer active)");
        return;
    }

    try {
        size_t const total = logtype_hashes.size();
        for (size_t batch_start = 0; batch_start < total; batch_start += cInsertBatchSize) {
            size_t const batch_end = std::min(batch_start + cInsertBatchSize, total);
            size_t const batch_size = batch_end - batch_start;

            auto const sql = build_batch_insert_sql(batch_size);
            auto stmt = m_db.prepare_statement(sql);

            // Bind 2 parameters per row: (logtype_hash, embedding)
            int param = 1;
            for (size_t i = batch_start; i < batch_end; ++i) {
                stmt.bind_blob(param++, &logtype_hashes[i], sizeof(uint64_t), false);
                stmt.bind_blob(
                        param++,
                        embeddings[i].data(),
                        embeddings[i].size() * sizeof(float),
                        false  // SQLITE_STATIC: vectors outlive step()
                );
            }
            stmt.step();
        }

        auto commit_stmt = m_db.prepare_statement("COMMIT");
        commit_stmt.step();

        SPDLOG_DEBUG("Stored {} logtype embeddings in cache", total);
    } catch (clp::SQLitePreparedStatement::OperationFailed const& e) {
        SPDLOG_WARN("Embedding cache write failed, rolling back - {}", e.what());
        try {
            auto rollback_stmt = m_db.prepare_statement("ROLLBACK");
            rollback_stmt.step();
        } catch (...) {
            // Best effort rollback
        }
    }
}

uint64_t fnv1a_hash(std::string const& logtype) {
    constexpr uint64_t cFnvOffsetBasis = 0xcbf29ce484222325ULL;
    constexpr uint64_t cFnvPrime = 0x100000001b3ULL;

    uint64_t hash = cFnvOffsetBasis;
    for (auto const c : logtype) {
        hash ^= static_cast<uint8_t>(c);
        hash *= cFnvPrime;
    }

    return hash;
}
}  // namespace clp_s::search
