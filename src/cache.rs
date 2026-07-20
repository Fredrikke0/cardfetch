use crate::shipping;
use crate::stores::StoreResult;
use crate::wizard::WizardSolution;
use anyhow::Context;
use rusqlite::Connection;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const NEGATIVE_TTL: Duration = Duration::from_secs(24 * 3600); // 24 hours
const POSITIVE_TTL: Duration = Duration::from_secs(7 * 24 * 3600); // 7 days
const SCRYFALL_TTL: Duration = Duration::from_secs(24 * 3600); // 24 hours (per Scryfall recommendation)

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

/// A single wizard solution record loaded from the database.
#[derive(Debug, Clone)]
pub struct WizardHistory {
    pub tolerance: usize,
    pub num_stores: usize,
    pub num_skipped: usize,
    pub total_cost: u64,
    pub raw_choices: Vec<Option<usize>>,
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
                ON listings(card_name, store_name);

            CREATE TABLE IF NOT EXISTS wizard_solutions (
                            id              INTEGER PRIMARY KEY AUTOINCREMENT,
                            strategy        TEXT NOT NULL,
                            tolerance       INTEGER NOT NULL,
                            eu_destination  INTEGER NOT NULL DEFAULT 0,
                            was_exhaustive  INTEGER NOT NULL DEFAULT 0,
                            rank            INTEGER NOT NULL DEFAULT 1,
                            num_stores      INTEGER NOT NULL,
                            num_found       INTEGER NOT NULL,
                            num_skipped     INTEGER NOT NULL,
                            total_card_cost INTEGER NOT NULL,
                            total_shipping  INTEGER NOT NULL,
                            total_cost      INTEGER NOT NULL,
                            per_card_cost   INTEGER NOT NULL,
                            assignments_json TEXT NOT NULL DEFAULT '[]',
                            created_at      INTEGER NOT NULL,
                            is_current      INTEGER NOT NULL DEFAULT 0,
                            UNIQUE(strategy, tolerance, eu_destination, rank)
                        );
            CREATE INDEX IF NOT EXISTS idx_wizard_solutions_strat
                ON wizard_solutions(strategy, tolerance);

            CREATE TABLE IF NOT EXISTS scryfall_names (
                input_name    TEXT PRIMARY KEY,
                resolved_name TEXT NOT NULL,
                fetched_at    INTEGER NOT NULL
            );",
        )
        .context("Failed to create tables")?;

        // Migration: add assignments_json column if upgrading from older schema.
        {
            let has_col: bool = conn
                .prepare("SELECT assignments_json FROM wizard_solutions LIMIT 0")
                .is_ok();
            if !has_col {
                conn.execute_batch(
                    "ALTER TABLE wizard_solutions ADD COLUMN assignments_json TEXT NOT NULL DEFAULT '[]';",
                )
                .context("Failed to migrate wizard_solutions schema")?;
            }
        }

        // Migration: add eu_destination column if upgrading from older schema.
        {
            let has_col: bool = conn
                .prepare("SELECT eu_destination FROM wizard_solutions LIMIT 0")
                .is_ok();
            if !has_col {
                conn.execute_batch(
                    "ALTER TABLE wizard_solutions ADD COLUMN eu_destination INTEGER NOT NULL DEFAULT 0;",
                )
                .context("Failed to migrate wizard_solutions schema")?;
                // Existing solutions were computed without --eu-destination, so 0 is correct.
            }
        }

        // Migration: add was_exhaustive column if upgrading from older schema.
        {
            let has_col: bool = conn
                .prepare("SELECT was_exhaustive FROM wizard_solutions LIMIT 0")
                .is_ok();
            if !has_col {
                conn.execute_batch(
                    "ALTER TABLE wizard_solutions ADD COLUMN was_exhaustive INTEGER NOT NULL DEFAULT 0;",
                )
                .context("Failed to migrate wizard_solutions schema")?;
                // Existing solutions were heuristic-only, so 0 is correct.
            }
        }

        // Migration: add rank column and update unique constraint.
        {
            let has_col: bool = conn
                .prepare("SELECT rank FROM wizard_solutions LIMIT 0")
                .is_ok();
            if !has_col {
                conn.execute_batch(
                    "ALTER TABLE wizard_solutions ADD COLUMN rank INTEGER NOT NULL DEFAULT 1;",
                )
                .context("Failed to migrate wizard_solutions schema")?;
                // Rebuild the table to change the UNIQUE constraint.
                // SQLite doesn't support ALTER TABLE ... ADD CONSTRAINT,
                // so we recreate the table.
                conn.execute_batch(
                    "CREATE TABLE wizard_solutions_new (
                        id              INTEGER PRIMARY KEY AUTOINCREMENT,
                        strategy        TEXT NOT NULL,
                        tolerance       INTEGER NOT NULL,
                        eu_destination  INTEGER NOT NULL DEFAULT 0,
                        was_exhaustive  INTEGER NOT NULL DEFAULT 0,
                        rank            INTEGER NOT NULL DEFAULT 1,
                        num_stores      INTEGER NOT NULL,
                        num_found       INTEGER NOT NULL,
                        num_skipped     INTEGER NOT NULL,
                        total_card_cost INTEGER NOT NULL,
                        total_shipping  INTEGER NOT NULL,
                        total_cost      INTEGER NOT NULL,
                        per_card_cost   INTEGER NOT NULL,
                        assignments_json TEXT NOT NULL DEFAULT '[]',
                        created_at      INTEGER NOT NULL,
                        is_current      INTEGER NOT NULL DEFAULT 0,
                        UNIQUE(strategy, tolerance, eu_destination, rank)
                    );
                    INSERT INTO wizard_solutions_new SELECT * FROM wizard_solutions;
                    DROP TABLE wizard_solutions;
                    ALTER TABLE wizard_solutions_new RENAME TO wizard_solutions;
                    CREATE INDEX IF NOT EXISTS idx_wizard_solutions_strat
                        ON wizard_solutions(strategy, tolerance);",
                )
                .context("Failed to migrate wizard_solutions unique constraint")?;
            }
        }

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

        let row = stmt.query_row(rusqlite::params![card_name, store_name], |row| {
            Ok((row.get::<_, Option<i64>>(0)?, row.get::<_, i64>(1)?))
        });
        let row = optional(row)?;

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

    // ── Wizard solution history ───────────────────────────────────────────

    /// Save a wizard solution.  Uses INSERT OR REPLACE so only the best
    /// solution per (strategy, tolerance, eu_destination) survives.
    pub fn save_wizard_solution(
        &self,
        strategy: &str,
        tolerance: usize,
        eu_destination: bool,
        was_exhaustive: bool,
        rank: usize,
        solution: &WizardSolution,
    ) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        let now = epoch_secs();
        let num_found = solution.assignments.len() - solution.skipped.len();
        let total_cost = solution.total_card_cost + solution.total_shipping;
        let per_card = if num_found > 0 {
            total_cost / num_found as u64
        } else {
            0
        };
        let assignments_json = serde_json::to_string(&solution.raw_choices)?;

        conn.execute(
            "INSERT OR REPLACE INTO wizard_solutions
                (strategy, tolerance, eu_destination, was_exhaustive, rank,
                 num_stores, num_found, num_skipped,
                 total_card_cost, total_shipping, total_cost, per_card_cost,
                 assignments_json, created_at, is_current)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, 1)",
            rusqlite::params![
                strategy,
                tolerance as i64,
                eu_destination as i64,
                was_exhaustive as i64,
                rank as i64,
                solution.num_stores as i64,
                num_found as i64,
                solution.skipped.len() as i64,
                solution.total_card_cost as i64,
                solution.total_shipping as i64,
                total_cost as i64,
                per_card as i64,
                assignments_json,
                now,
            ],
        )?;
        Ok(())
    }

    /// Mark all solutions for the given strategy as not-current (prior to
    /// a new wizard run).
    pub fn clear_current_solutions(
        &self,
        strategy: &str,
        eu_destination: bool,
    ) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE wizard_solutions SET is_current = 0 WHERE strategy = ?1 AND eu_destination = ?2",
            rusqlite::params![strategy, eu_destination as i64],
        )?;
        Ok(())
    }

    /// Discard solutions whose `created_at` is before the latest listing
    /// fetch — those solutions were computed on stale data.
    pub fn prune_stale_solutions(&self) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        let latest_fetch = conn.query_row("SELECT MAX(fetched_at) FROM listings", [], |row| {
            row.get::<_, Option<i64>>(0)
        });
        let latest_fetch = optional(latest_fetch)?;

        if let Some(cutoff) = latest_fetch {
            conn.execute(
                "DELETE FROM wizard_solutions WHERE created_at < ?1",
                rusqlite::params![cutoff],
            )?;
        }
        Ok(())
    }

    /// Delete all wizard solutions if any listings reference blacklisted
    /// sellers.  This catches stale solutions after a blacklist change
    /// without requiring a full scraper re-run.
    pub fn prune_blacklisted_solutions(&self) -> anyhow::Result<usize> {
        let conn = self.conn.lock().unwrap();

        // Collect distinct store names from listings.
        let mut stmt = conn.prepare("SELECT DISTINCT store_name FROM listings")?;
        let names: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .filter_map(|r| r.ok())
            .collect();

        let has_blacklisted = names
            .iter()
            .any(|n| shipping::is_blacklisted(shipping::extract_seller_name(n)));

        if has_blacklisted {
            // Only delete non-EU solutions — blacklisted sellers may still
            // ship within the EU, so EU-destination solutions are valid.
            let deleted =
                conn.execute("DELETE FROM wizard_solutions WHERE eu_destination = 0", [])?;
            // Also delete the blacklisted listings themselves so this is a
            // one-time cleanup — the next scraper run will re-fetch them,
            // but the wizard won't keep re-pruning on every invocation.
            for name in &names {
                if shipping::is_blacklisted(shipping::extract_seller_name(name)) {
                    conn.execute(
                        "DELETE FROM listings WHERE store_name = ?1",
                        rusqlite::params![name],
                    )?;
                }
            }
            return Ok(deleted);
        }

        Ok(0)
    }

    /// Load the best-ever solution (lowest total_cost) for each tolerance
    /// of the given strategy and EU destination.  Returns a map: tolerance → record.
    pub fn load_best_solutions(
        &self,
        strategy: &str,
        eu_destination: bool,
    ) -> anyhow::Result<HashMap<usize, WizardHistory>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT tolerance, num_stores, num_skipped,
                    total_cost, assignments_json
             FROM wizard_solutions
             WHERE strategy = ?1 AND eu_destination = ?2
             ORDER BY total_cost ASC",
        )?;

        let mut map = HashMap::new();
        let rows: Vec<WizardHistory> = stmt
            .query_map(rusqlite::params![strategy, eu_destination as i64], |row| {
                let json_str: String = row.get(4)?;
                let raw_choices: Vec<Option<usize>> =
                    serde_json::from_str(&json_str).unwrap_or_default();
                Ok(WizardHistory {
                    tolerance: row.get::<_, i64>(0)? as usize,
                    num_stores: row.get::<_, i64>(1)? as usize,
                    num_skipped: row.get::<_, i64>(2)? as usize,
                    total_cost: row.get::<_, i64>(3)? as u64,
                    raw_choices,
                })
            })?
            .filter_map(|r| r.ok())
            .collect();

        for record in rows {
            // Keep only the best (first row per tolerance, ordered by total_cost ASC)
            map.entry(record.tolerance).or_insert(record);
        }

        Ok(map)
    }

    /// Look up all cached wizard solutions for the exact parameters.
    /// Returns up to TOP_N solutions (ordered by rank, best first) and
    /// whether they were computed exhaustively.
    pub fn get_cached_solutions(
        &self,
        strategy: &str,
        tolerance: usize,
        eu_destination: bool,
    ) -> anyhow::Result<Vec<(WizardHistory, bool)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT tolerance, num_stores, num_skipped,
                    total_cost, assignments_json, was_exhaustive
             FROM wizard_solutions
             WHERE strategy = ?1 AND tolerance = ?2 AND eu_destination = ?3
             ORDER BY total_cost ASC",
        )?;

        let rows: Vec<(WizardHistory, bool)> = stmt
            .query_map(
                rusqlite::params![strategy, tolerance as i64, eu_destination as i64],
                |row| {
                    let json_str: String = row.get(4)?;
                    let raw_choices: Vec<Option<usize>> =
                        serde_json::from_str(&json_str).unwrap_or_default();
                    Ok((
                        WizardHistory {
                            tolerance: row.get::<_, i64>(0)? as usize,
                            num_stores: row.get::<_, i64>(1)? as usize,
                            num_skipped: row.get::<_, i64>(2)? as usize,
                            total_cost: row.get::<_, i64>(3)? as u64,
                            raw_choices,
                        },
                        row.get::<_, i64>(5)? != 0,
                    ))
                },
            )?
            .filter_map(|r| r.ok())
            .collect();

        Ok(rows)
    }

    /// Look up a card name in the Scryfall resolution cache.
    /// Returns `Some("Canonical Name")` if a fresh entry exists, `None` if
    /// the cache is missing or stale.
    pub fn lookup_scryfall(&self, input_name: &str) -> anyhow::Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        let now = epoch_secs();

        let row = conn.query_row(
            "SELECT resolved_name, fetched_at FROM scryfall_names WHERE input_name = ?1",
            rusqlite::params![input_name],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        );
        let row = optional(row)?;

        match row {
            Some((resolved, fetched_at)) => {
                let age = Duration::from_secs((now - fetched_at).max(0) as u64);
                if age < SCRYFALL_TTL {
                    Ok(Some(resolved))
                } else {
                    Ok(None)
                }
            }
            None => Ok(None),
        }
    }

    /// Store a Scryfall name resolution in the cache.
    pub fn store_scryfall(&self, input_name: &str, resolved_name: &str) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        let now = epoch_secs();

        conn.execute(
            "INSERT OR REPLACE INTO scryfall_names (input_name, resolved_name, fetched_at)
             VALUES (?1, ?2, ?3)",
            rusqlite::params![input_name, resolved_name, now],
        )?;

        Ok(())
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn epoch_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// Convert `QueryReturnedNoRows` into `Ok(None)`, propagate other errors.
fn optional<T>(result: Result<T, rusqlite::Error>) -> Result<Option<T>, rusqlite::Error> {
    match result {
        Ok(v) => Ok(Some(v)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e),
    }
}
