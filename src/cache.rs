use crate::stores::StoreResult;
use anyhow::Context;
use rusqlite::Connection;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const NEGATIVE_TTL: Duration = Duration::from_secs(24 * 3600); // 24 hours
const POSITIVE_TTL: Duration = Duration::from_secs(7 * 24 * 3600); // 7 days

/// Result of a cache lookup for a single (card, store) pair.
pub enum CacheLookup {
    /// No valid cache entry — perform a live search.
    Search,
    /// Negative cache hit — previous search had no results and is still fresh.
    Skip,
    /// Positive cache hit — return these cached results (may be multiple for
    /// stores like CardMarket that return several sellers per card).
    Hit(Vec<StoreResult>),
}

/// Thread-safe SQLite cache for store search results.
///
/// Uses a single `listings` table where each row is one product listing.
/// A (card, store) pair can have multiple rows (e.g. CardMarket sellers).
/// Negative cache is recorded as a row with `in_stock = 0`.
pub struct Cache {
    conn: Mutex<Connection>,
}

impl Cache {
    /// Open (or create) the cache database at `path`.  Enables WAL mode,
    /// drops the old two-table schema, and creates the new `listings` table.
    pub fn open(path: &str) -> anyhow::Result<Self> {
        let conn = Connection::open(path).context("Failed to open cache database")?;

        conn.execute_batch("PRAGMA journal_mode=WAL;")
            .context("Failed to enable WAL mode")?;

        // Drop old schema (no backwards compatibility needed)
        conn.execute_batch(
            "DROP TABLE IF EXISTS history;
             DROP TABLE IF EXISTS matches;",
        )
        .context("Failed to drop old tables")?;

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS listings (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                card_name   TEXT NOT NULL,
                store_name  TEXT NOT NULL,
                price       INTEGER NOT NULL,
                url         TEXT NOT NULL,
                in_stock    INTEGER NOT NULL DEFAULT 1,
                fetched_at  INTEGER NOT NULL,
                UNIQUE(card_name, store_name, url)
            );
            CREATE INDEX IF NOT EXISTS idx_listings_card
                ON listings(card_name);
            CREATE INDEX IF NOT EXISTS idx_listings_lookup
                ON listings(card_name, store_name);",
        )
        .context("Failed to create listings table")?;

        Ok(Cache {
            conn: Mutex::new(conn),
        })
    }

    /// Check the cache for a (card, store) pair.
    ///
    /// The `store_name` parameter acts as a *cache key prefix*: both exact
    /// matches (negative cache entries) and `"prefix: ..."` rows (seller-
    /// specific results from stores like CardMarket) are considered.
    ///
    /// Returns `Hit(vec![...])` with all in-stock results if the positive
    /// cache is fresh, `Skip` if the negative cache is fresh, or `Search`
    /// if the cache is stale or missing.
    pub fn lookup(&self, card_name: &str, store_name: &str) -> anyhow::Result<CacheLookup> {
        let conn = self.conn.lock().unwrap();
        let now = epoch_secs();

        // Match both exact (negative entries) and prefix (seller entries like
        // "cardmarket.com: SellerName").  The LIKE pattern uses SQLite `||`
        // for concatenation so we don't need to build the pattern in Rust.
        let mut stmt = conn.prepare(
            "SELECT MAX(fetched_at),
                    COALESCE(SUM(CASE WHEN in_stock = 1 THEN 1 ELSE 0 END), 0)
             FROM listings
             WHERE card_name = ?1
               AND (store_name = ?2 OR store_name LIKE (?2 || ':%'))",
        )?;

        let row: Option<(Option<i64>, i64)> = stmt
            .query_row(rusqlite::params![card_name, store_name], |row| {
                Ok((row.get::<_, Option<i64>>(0)?, row.get::<_, i64>(1)?))
            })
            .optional()?;

        match row {
            Some((Some(fetched_at), in_stock_count)) => {
                let age = Duration::from_secs((now - fetched_at).max(0) as u64);

                if in_stock_count == 0 {
                    return Ok(if age < NEGATIVE_TTL {
                        CacheLookup::Skip
                    } else {
                        CacheLookup::Search
                    });
                }
                if age >= POSITIVE_TTL {
                    return Ok(CacheLookup::Search);
                }

                let mut res_stmt = conn.prepare(
                    "SELECT price, url, store_name FROM listings
                     WHERE card_name = ?1
                       AND (store_name = ?2 OR store_name LIKE (?2 || ':%'))
                       AND in_stock = 1",
                )?;
                let results: Vec<StoreResult> = res_stmt
                    .query_map(rusqlite::params![card_name, store_name], |row| {
                        Ok(StoreResult {
                            store_name: row.get::<_, String>(2)?,
                            card_name: card_name.to_string(),
                            price: row.get::<_, u32>(0)?,
                            url: row.get::<_, String>(1)?,
                        })
                    })?
                    .filter_map(|r| r.ok())
                    .collect();
                Ok(CacheLookup::Hit(results))
            }
            _ => Ok(CacheLookup::Search),
        }
    }

    /// Store search results (or lack thereof) in the cache.
    ///
    /// If `results` is `None` or empty, a negative cache entry is recorded.
    /// Otherwise, all results are inserted.
    pub fn store(
        &self,
        card_name: &str,
        store_name: &str,
        results: Option<&[StoreResult]>,
    ) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        let now = epoch_secs();

        // Remove all old entries for this (card, store)
        conn.execute(
            "DELETE FROM listings WHERE card_name = ?1 AND store_name = ?2",
            rusqlite::params![card_name, store_name],
        )?;

        // For CardMarket sub-sellers (e.g. "cardmarket.com: Seller"), also
        // clean up any stale negative cache entry for the base store prefix
        // (e.g. "cardmarket.com") that may have been written by a previous
        // empty-search run.
        if let Some((prefix, _)) = store_name.split_once(':') {
            conn.execute(
                "DELETE FROM listings WHERE card_name = ?1 AND store_name = ?2 AND in_stock = 0",
                rusqlite::params![card_name, prefix],
            )?;
        }

        match results {
            Some(items) if !items.is_empty() => {
                let mut stmt = conn.prepare(
                    "INSERT OR REPLACE INTO listings
                        (card_name, store_name, price, url, in_stock, fetched_at)
                     VALUES (?1, ?2, ?3, ?4, 1, ?5)",
                )?;
                for item in items {
                    stmt.execute(rusqlite::params![
                        card_name, store_name, item.price, item.url, now,
                    ])?;
                }
            }
            _ => {
                // Negative cache — single row with in_stock=0
                conn.execute(
                    "INSERT INTO listings
                        (card_name, store_name, price, url, in_stock, fetched_at)
                     VALUES (?1, ?2, 0, '', 0, ?3)",
                    rusqlite::params![card_name, store_name, now],
                )?;
            }
        }

        Ok(())
    }

    /// Get all in-stock listings for the given card names.
    ///
    /// Used by the purchase wizard to load all relevant data in one query.
    /// Results are returned unsorted; the caller groups them as needed.
    pub fn get_listings(&self, card_names: &[String]) -> anyhow::Result<Vec<StoreResult>> {
        if card_names.is_empty() {
            return Ok(Vec::new());
        }

        let conn = self.conn.lock().unwrap();

        let placeholders: Vec<String> = (1..=card_names.len()).map(|i| format!("?{i}")).collect();
        let sql = format!(
            "SELECT card_name, store_name, price, url FROM listings
             WHERE card_name IN ({}) AND in_stock = 1",
            placeholders.join(", ")
        );

        let mut stmt = conn.prepare(&sql)?;
        let params: Vec<&dyn rusqlite::types::ToSql> = card_names
            .iter()
            .map(|s| s as &dyn rusqlite::types::ToSql)
            .collect();

        let results: Vec<StoreResult> = stmt
            .query_map(params.as_slice(), |row| {
                Ok(StoreResult {
                    card_name: row.get::<_, String>(0)?,
                    store_name: row.get::<_, String>(1)?,
                    price: row.get::<_, u32>(2)?,
                    url: row.get::<_, String>(3)?,
                })
            })?
            .filter_map(|r| r.ok())
            .collect();

        Ok(results)
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn epoch_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

trait OptionalExt<T> {
    fn optional(self) -> Result<Option<T>, rusqlite::Error>;
}

impl<T> OptionalExt<T> for Result<T, rusqlite::Error> {
    fn optional(self) -> Result<Option<T>, rusqlite::Error> {
        match self {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }
}
