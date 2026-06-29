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
    /// Skip this store entirely (no result to return).
    Skip,
    /// Positive cache hit — return this cached result.
    Hit(StoreResult),
}

/// Thread-safe SQLite cache for store search results.
pub struct Cache {
    conn: Mutex<Connection>,
}

impl Cache {
    /// Open (or create) the cache database at `path`.  Enables WAL mode and
    /// creates tables if they don't exist.
    pub fn open(path: &str) -> anyhow::Result<Self> {
        let conn = Connection::open(path).context("Failed to open cache database")?;

        conn.execute_batch("PRAGMA journal_mode=WAL;")
            .context("Failed to enable WAL mode")?;

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS history (
                card_name   TEXT NOT NULL,
                store_name  TEXT NOT NULL,
                searched_at INTEGER NOT NULL,
                found_match INTEGER NOT NULL,
                PRIMARY KEY (card_name, store_name)
            );
            CREATE TABLE IF NOT EXISTS matches (
                card_name   TEXT NOT NULL,
                store_name  TEXT NOT NULL,
                url         TEXT NOT NULL,
                price       INTEGER NOT NULL,
                fetched_at  INTEGER NOT NULL,
                PRIMARY KEY (card_name, store_name)
            );",
        )
        .context("Failed to create cache tables")?;

        Ok(Cache {
            conn: Mutex::new(conn),
        })
    }

    /// Check the cache for a (card, store) pair.  Returns:
    /// - `Search` if no entry exists or it's stale
    /// - `Skip` if a negative cache entry is fresh
    /// - `Hit(result)` if a positive cache entry is fresh
    pub fn lookup(&self, card_name: &str, store_name: &str) -> anyhow::Result<CacheLookup> {
        let conn = self.conn.lock().unwrap();

        let mut stmt = conn.prepare(
            "SELECT searched_at, found_match FROM history WHERE card_name = ?1 AND store_name = ?2",
        )?;

        let row = stmt
            .query_row(rusqlite::params![card_name, store_name], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
            })
            .optional()?;

        let (searched_at, found_match) = match row {
            Some(r) => r,
            None => return Ok(CacheLookup::Search),
        };

        let now = epoch_secs();
        let age = Duration::from_secs((now - searched_at).max(0) as u64);

        if found_match == 0 {
            if age < NEGATIVE_TTL {
                return Ok(CacheLookup::Skip);
            }
            return Ok(CacheLookup::Search);
        }

        // found_match == 1: check matches table for freshness
        let mut match_stmt = conn.prepare(
            "SELECT url, price FROM matches WHERE card_name = ?1 AND store_name = ?2",
        )?;

        let match_row = match_stmt
            .query_row(
                rusqlite::params![card_name, store_name],
                |row| {
                    Ok(StoreResult {
                        store_name: store_name.to_string(),
                        card_name: card_name.to_string(),
                        price: row.get::<_, u32>(1)?,
                        url: row.get::<_, String>(0)?,
                    })
                },
            )
            .optional()?;

        match match_row {
            Some(result) if age < POSITIVE_TTL => Ok(CacheLookup::Hit(result)),
            _ => Ok(CacheLookup::Search),
        }
    }

    /// Store a search result (or lack thereof) in the cache.  If `result` is
    /// `None`, a negative cache entry is recorded.  If `Some`, the match is
    /// stored in the `matches` table alongside a positive history entry.
    pub fn store(
        &self,
        card_name: &str,
        store_name: &str,
        result: Option<&StoreResult>,
    ) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        let now = epoch_secs();

        conn.execute(
            "INSERT OR REPLACE INTO history (card_name, store_name, searched_at, found_match)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![card_name, store_name, now, if result.is_some() { 1 } else { 0 }],
        )?;

        conn.execute(
            "DELETE FROM matches WHERE card_name = ?1 AND store_name = ?2",
            rusqlite::params![card_name, store_name],
        )?;

        if let Some(r) = result {
            conn.execute(
                "INSERT INTO matches (card_name, store_name, url, price, fetched_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![card_name, store_name, r.url, r.price, now],
            )?;
        }

        Ok(())
    }
}

fn epoch_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// Extension trait to get `Option<T>` from rusqlite `Result<T>`.
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
