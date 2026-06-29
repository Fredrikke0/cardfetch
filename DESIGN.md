# CardFetch — Design Document

## Overview

A Rust CLI tool that reads a typical MTG decklist (newline-separated, with
quantities), searches each card on multiple online stores in parallel, and
prints in-stock results grouped by card.

## Architecture

```
┌──────────────┐
│  cards.txt   │
│ (one/line)   │
└──────┬───────┘
       │
       ▼
┌─────────────────────────────────────────────────┐
│                   main                           │
│  1. Parse decklist (strip quantities)            │
│  2. Spawn one thread per store                   │
│  3. Collect results via channel                  │
│  4. Print grouped by card                        │
└──────┬──────────┬──────────┬────────────────────┘
       │          │          │
       ▼          ▼          ▼
┌──────────┐ ┌──────────┐ ┌──────────┐
│ outland  │ │ store 2  │ │ store N  │
│  thread  │ │  thread  │ │  thread  │
│          │ │          │ │          │
│ card1 ───│─│ card1 ───│─│ card1 ───│──▶ mpsc channel
│ card2 ───│─│ card2 ───│─│ card2 ───│──▶
│ ...      │ │ ...      │ │ ...      │
└──────────┘ └──────────┘ └──────────┘
```

- Each store runs in its own thread, processing cards sequentially with a
  shared rate-limit delay (`DELAY_MS = 200`).
- Each thread builds its own `reqwest::blocking::Client` with a per-store
  timeout. Connections are pooled per-client.
- Results sent to main thread via `mpsc::channel`.
- One store failing does not affect others.

## CLI Interface

```
cardfetch --input cards.txt
```

Input format (typical MTG decklist):
```
1 Snakeskin Veil
4 Lightning Bolt
2 Black Lotus
```

Flags:
```
--input <FILE>    Path to decklist file (required)
--help, -h        Print help
```

## Store system

### Trait

```rust
pub trait Store: Send + Sync {
    /// Human-readable store name, used in output.
    fn name(&self) -> &str;

    /// Timeout for HTTP requests to this store (in seconds).
    fn timeout_secs(&self) -> u64;

    /// Search for a single card. Returns None if no in-stock match found.
    fn search(
        &self,
        client: &reqwest::blocking::Client,
        card_name: &str,
    ) -> anyhow::Result<Option<StoreResult>>;
}

pub struct StoreResult {
    pub store_name: String,  // e.g. "outland.no"
    pub card_name: String,   // normalized matched name
    pub price: u32,          // price in oere (e.g. 200 = 2,00 kr)
    pub url: String,         // product page URL
}

pub struct SearchProduct {
    pub name: String,
    pub price: u32,
    pub url: String,
    pub in_stock: bool,
}
```

### Adding a store

1. Create `src/stores/<name>.rs`.
2. Implement the `Store` trait.
3. Register in `src/stores/mod.rs` → `all_stores()`.

Example registration:
```rust
pub fn all_stores() -> Vec<Box<dyn Store>> {
    vec![
        Box::new(outland::Outland::new()),
        // Box::new(other::Other::new()),
    ]
}
```

### Per-store responsibilities

| Concern | Where |
|---|---|
| GraphQL/REST query format | Per-store |
| URL/endpoint | Per-store |
| Response deserialization | Per-store (own types) |
| Name normalization & matching | Per-store |
| Price formatting | Central (`output::format_price`) |
| Rate limiting (delay) | Shared (`DELAY_MS` constant) |
| URL encoding helpers | Shared (`urlencode_pct`, `urlencode_plus`) |
| Substring matching | Shared (`title_contains`) |
| Parsed product type | Shared (`SearchProduct`) |

The `graphql.rs` module is now `stores/outland.rs`, self-contained with
outland-specific GraphQL types, query template, and matching logic.

## Data flow

1. **Parse CLI args** → `--input <file>`.
2. **Read decklist** → one card name per line, strip quantity prefix
   (`1 Card Name` → `Card Name`), deduplicate, trim.
3. **Spawn store threads** — one per store.
   Each thread iterates over all cards, calls `store.search()`, sends
   `StoreResult` via channel.
4. **Collect results** — main thread receives from channel until all
   threads finish.
5. **Group by card** — aggregate results per card name.
6. **Print** — grouped output.

## Output format

```
Snakeskin Veil:
  outland.no  NOK 2,00    https://www.outland.no/p-snakeskin-veil-...
  store2      USD 1.50    https://store2.com/...

Lightning Bolt:
  outland.no  NOK 15,00   https://www.outland.no/p-lightning-bolt-...
  store2      USD 3.20    https://store2.com/...

Found 5 cards across 2 stores
```

- Only cards with at least one in-stock match are printed.
- Cards with no matches anywhere are silent.
- Card name used as header, store results indented beneath.

## Outland.no store (`stores/outland.rs`)

### API details

- **Endpoint**: `POST https://www.outland.no/api/graphql`
- **Query**: Magento `ProductList` (verbatim from website)
- **Filters applied**:
  - `category_uid: { in: ["MTI1MA=="] }` — "Kortspill & samlekort" category
  - `in_stock: { in: ["1"] }` — only in-stock products
- **Pagination**: `pageSize: 48`, fetches all pages
- **Rate limit**: 200ms between requests
- **Timeout**: 30s

### Name matching

- API returns names like `"Snakeskin Veil (Enkeltkort)"`
- Strip trailing ` (Enkeltkort)` / ` (enkeltkort)`
- Case-insensitive exact match against search term

## Error handling

| Scenario | Behavior |
|---|---|
| Network error / timeout | `eprintln!` warning, skip card, continue |
| Non-200 HTTP response | `eprintln!` warning with status code, skip |
| JSON parse error | `eprintln!` warning, skip |
| Store thread panic | Other stores continue unaffected |
| Empty input file | Error message, exit |
| Input file not found | Error message, exit |

## Crate dependencies

| Crate | Purpose |
|---|---|
| `reqwest` (blocking) | HTTP client (shared, thread-safe) |
| `serde` + `serde_json` | JSON deserialization |
| `clap` | CLI argument parsing |
| `anyhow` | Error handling |

## Future considerations

- **Async**: Switch to `tokio` tasks instead of threads for lighter concurrency.
- **Output formats**: JSON, CSV for machine consumption.
- **Caching**: Cache API responses to avoid re-fetching duplicate cards within
  a session.
- **Card-name dedup**: Already on the radar — the decklist may repeat cards;
  we should search each unique name once.
