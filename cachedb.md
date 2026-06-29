# CardFetch — SQLite Cache Design

## Motivation

Currently every `cardfetch` run performs a full live search across all stores,
even for cards that were searched minutes ago.  Rate‑limiting at 200 ms per
request means a decklist of 60 cards across 7 stores takes at least 84 seconds
of wall time *every* run, regardless of whether inventory changed.

A local SQLite cache lets us skip searches when results are fresh enough,
dramatically speeding up repeated runs.

## Schema

Two tables with **per‑(card, store)** granularity.  A card that was found at
Outland but not at Finn will have two independent cache entries — one can be
served from cache while the other is re‑fetched.

Since `Store::search` returns at most one result per `(card, store)`, the
`matches` table has at most one row per `(card, store)`.

```sql
CREATE TABLE history (
    card_name   TEXT NOT NULL,
    store_name  TEXT NOT NULL,
    searched_at INTEGER NOT NULL,   -- unix epoch seconds
    found_match INTEGER NOT NULL,   -- boolean: 1 if a match was stored, 0 otherwise
    PRIMARY KEY (card_name, store_name)
);

CREATE TABLE matches (
    card_name   TEXT NOT NULL,
    store_name  TEXT NOT NULL,
    url         TEXT NOT NULL,
    price       INTEGER NOT NULL,   -- integer oere (e.g. 200 = 2,00 kr)
    fetched_at  INTEGER NOT NULL,
    PRIMARY KEY (card_name, store_name)
);
```

### Why no foreign key?

SQLite foreign keys are off by default and add overhead.  We keep things
simple with application‑level consistency — we always insert/delete the two
tables together inside a transaction.

## Cache Freshness Rules

| Condition | Action |
|---|---|
| **No previous search** for this `(card, store)` | Live search → cache results |
| Previous search had **0 matches** AND is **< 24 h** old | Skip (negative cache hit) |
| Previous search had **0 matches** AND is **≥ 24 h** old | Live search → update cache |
| Previous search had **a match** AND it is **< 7 days** old | Return cached match |
| Previous search had **a match** AND it is **≥ 7 days** old | Live search → update cache |

These durations apply per `(card, store)`, so one store's stale match doesn't
force a re‑fetch of another store that was searched 2 hours ago.

### Rationale

- **24 h negative cache**: Inventory doesn't appear out of nowhere on a
  timescale shorter than a day.  If a store didn't carry "Black Lotus"
  yesterday, it almost certainly won't today.
- **7 day positive cache**: Prices and stock can change within a week.
  Editions sell out, new listings appear.  A week balances freshness against
  not hammering store APIs.

## Store Trait — No Change

`Store::search` keeps its current signature:

```rust
fn search(
    &self,
    client: &reqwest::blocking::Client,
    card_name: &str,
) -> Result<Option<StoreResult>>;
```

Only one result per store per card is expected.  The cache stores that single
result (or records a negative result if `None` is returned).

## Database Connection Strategy

- **Crate**: `rusqlite` with the `bundled` feature (bundles SQLite, no system
  dependency).
- **WAL mode**: Enabled on open for concurrent read/write from multiple
  threads.
- **Connection sharing**: `Arc<Mutex<Connection>>` held by the cache module.
  WAL mode means readers don't block writers, contention is minimal.
- **Location**: `cache.db` in the current working directory.

## Database Operations

### On cache miss (live search performed)

```sql
BEGIN TRANSACTION;
INSERT OR REPLACE INTO history (card_name, store_name, searched_at, found_match)
VALUES (?1, ?2, ?3, ?4);
DELETE FROM matches WHERE card_name = ?1 AND store_name = ?2;
-- Only if a match was found:
INSERT INTO matches (card_name, store_name, url, price, fetched_at)
VALUES (?1, ?2, ?3, ?4, ?5);
COMMIT;
```

### On cache hit (return cached match)

```sql
SELECT url, price FROM matches
WHERE card_name = ?1 AND store_name = ?2;
```

### Checking freshness

```sql
SELECT searched_at, found_match FROM history
WHERE card_name = ?1 AND store_name = ?2;
```

## Cache Lookup Logic (in `Cache::lookup`)

```
1. Query history for (card_name, store_name)
2. If no row → return Search (miss)
3. Read (searched_at, found_match)
4. If found_match == 0:
   a. If now - searched_at < 24h → return Skip (negative hit)
   b. Else → return Search (negative stale)
5. If found_match == 1:
   a. Query matches for (card_name, store_name)
   b. If fetched_at exists and now - fetched_at < 7 days → return Hit(result)
   c. Else → return Search (positive stale)
```

## CLI Changes

Add a `--no-cache` flag:

```
--no-cache    Bypass the cache and perform a fresh live search
```

When set, neither read from nor write to the database.  All searches go
directly to the stores.

Cache is **enabled by default** — this keeps the 99% use case fast without
requiring the user to remember a flag.

## Threading Design

Each store thread receives `Arc<Cache>` (or `None` if `--no-cache`).  Inside
the thread, for each card:

1. Call `cache.lookup(card, store)`.
2. If `Hit(result)` → send result via channel, continue.
3. If `Skip` → increment progress counter, continue (no result).
4. If `Search` → perform live search, call `cache.store(...)`, send result
   via channel.

Main thread changes are minimal — just pass `Arc<Cache>` to each thread.

### Progress bar handling

Cache hits (`Hit` / `Skip`) are instant and still count toward card
completion.  The store thread immediately increments the progress counter for
those before moving to the next card, so the bar advances correctly.

## Implementation Plan

1. **Add dependency**: `rusqlite` with `bundled` feature
2. **Create `src/cache.rs`**: `Cache` struct with `open`, `lookup`, `store`
3. **Wire into `main.rs`**: Pass `Arc<Cache>` to store threads
4. **Add `--no-cache` flag** to CLI
5. **Test**: Manual runs, verify freshness rules
