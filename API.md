# CardFetch API — Frontend Implementation Guide

## Base URL

```
http://localhost:3000
```

All endpoints return JSON. The server only listens on `127.0.0.1` — no network access.

---

## Data model: store identifiers

Store entries use compact field names. The `s` field is the **store identifier**
and the optional `c` field is the **CardMarket category**:

| `c` value | CardMarket type | Full store name |
|---|---|---|
| `"n"` | Norwegian seller | `cardmarket.com: {s}` |
| `"i"` | International powerseller | `cardmarket-int.com: {s}` |
| `"p"` | International private seller | `cardmarket-int-private.com: {s}` |
| _(absent)_ | Not CardMarket | Use `s` as-is (e.g. `"outland.no"`) |

**Example:**
```json
{"s": "AbyssalGames", "p": 220, "u": "https://…", "c": "i"}
{"s": "outland.no",    "p": 500, "u": "https://…"}
```

---

## Endpoints

### `GET /stores`

Returns the list of available store names.

**Response:** `200 OK`
```json
[
  "outland.no",
  "finn.no",
  "collectible.no",
  "korthaien.no",
  "midgardgames.no",
  "pokeboks.no",
  "adamstuenretro.no",
  "cardmarket.com"
]
```

---

### `POST /fetch`

Submits a card search. Card names are resolved to canonical names via the
[Scryfall autocomplete API](https://scryfall.com/docs/api/cards/autocomplete)
before searching stores (e.g. `"Lightning"` → `"Lightning Bolt"`).
Names that cannot be resolved are returned in the `u` field.

Behavior depends on `cache_only`:

- **`cache_only: false`** (default): If all requested cards are already cached,
  returns results immediately. Otherwise creates a background job.
- **`cache_only: true`**: Always returns cached results immediately — never
  creates a job, never returns 503. Results may be incomplete if some stores
  haven't been fetched yet.

**Request:**
```json
{
  "cards": ["Lightning Bolt", "Counterspell"],
  "stores": ["outland", "cardmarket"],
  "cache_only": false,
  "max_per_store": 0
}
```

| Field | Type | Default | Notes |
|---|---|---|---|
| `cards` | `string[]` | — | Max 100 unique cards. Duplicates are ignored. |
| `stores` | `string[]` | `[]` | Substring match against store names. Empty = all stores. |
| `cache_only` | `boolean` | `false` | If true, only return cached data — never blocks. |
| `max_per_store` | `number` | `0` | Max results per store endpoint per card. `0` = no cap. Applied at response time — scraping always fetches everything. |

**Response (all cached, or `cache_only: true`):**
```json
{
  "r": {
    "Lightning Bolt": [
      {"s": "outland.no", "p": 200, "u": "https://…"}
    ]
  },
  "u": []
}
```

| Field | Type | Notes |
|---|---|---|
| `r` | `object` | Card name → array of store entries. |
| `u` | `string[]` | Card names that Scryfall could not resolve. |

**Store entry fields:**

| Field | Type | Notes |
|---|---|---|
| `s` | `string` | Store identifier (see [Data model](#data-model-store-identifiers)). |
| `p` | `number` | Price in integer øre. Divide by 100 to display. |
| `u` | `string` | Full URL to the listing. |
| `c` | `string?` | CardMarket category: `"n"`, `"i"`, or `"p"`. Absent for non-CM stores. |

**Response (some uncached — background job):**
```json
{
  "job_id": "XsAcq8nz"
}
```

**Errors:**

| HTTP | Body | When |
|---|---|---|
| 400 | `"Too many cards: 101. Max is 100."` | Cards exceed limit |
| 400 | `"No cards provided."` | Empty card list |
| 400 | `"No recognized Magic card names found after resolution."` | All card names failed Scryfall resolution |
| 503 | `{"error":"Server busy: …","existing_job_id":"XsAcq8nz"}` | Another fetch job is running (never with `cache_only`). The `existing_job_id` lets other users poll the running job's progress. A wizard job does not block fetch. |

---

### `POST /wizard`

Finds the optimal store assignments from previously fetched (cached) card listings.
Like `/fetch`, card names are resolved via Scryfall before lookup.

Returns a response with `r` (array of up to **3 solutions**, sorted best-first)
and `u` (names that couldn't be resolved).

If a suitably cached solution exists (computed exhaustively), it is returned
immediately without creating a background job.

**Two-phase computation:** When a background job is created, the wizard runs in two
phases for the best user experience:
1. **Heuristic phase** — fast, returns a good solution. Populated as `partial_result`
   in the job response, available within seconds.
2. **Exhaustive phase** — store-swap refinement that may improve upon the heuristic
   result. Updates `result` when complete.

The frontend should display `partial_result` as soon as it appears and swap to
`result` when it arrives.

**Prerequisite:** Run `/fetch` first for the same cards so listings are in the cache.

**Request:**
```json
{
  "cards": ["Lightning Bolt", "Counterspell"],
  "tolerance": 2,
  "eu_destination": false,
  "strategy": "cheapest"
}
```

| Field | Type | Default | Notes |
|---|---|---|---|
| `cards` | `string[]` | — | Max 100. Must have been fetched first. |
| `tolerance` | `number` | `0` | Max cards the solution may skip. **Max 5.** |
| `eu_destination` | `boolean` | `false` | Removes 25% VAT from non-Norwegian sellers. |
| `strategy` | `string` | `"cheapest"` | `"cheapest"` or `"simplest"` |

**Response (cached solution hit — no job):**
```json
{
  "r": [
    {
      "a": [
        {"c": "Lightning Bolt", "s": "outland.no", "p": 200, "u": "https://…"},
        {"c": "Counterspell"}
      ],
      "sk": ["Counterspell"],
      "st": [
        {"n": "outland.no", "ct": 200, "sh": 2900}
      ],
      "tc": 200,
      "ts": 2900,
      "ns": 1
    }
  ],
  "u": []
}
```

The `r` array contains up to **3 solutions**, sorted best-first (lowest internal
score). Each solution object has the same shape:

| Field | Type | Notes |
|---|---|---|
| `a` | `object[]` | Assignments, one per card. See below for entry fields. |
| `sk` | `string[]` | Card names that couldn't be assigned within tolerance. |
| `st` | `object[]` | Stores used. See below for entry fields. |
| `tc` | `number` | Sum of assigned card prices (øre). |
| `ts` | `number` | Sum of shipping costs (øre). |
| `ns` | `number` | Number of distinct stores used. |

**Assignment entry (`a[i]`):**

| Field | Type | Notes |
|---|---|---|
| `c` | `string` | Card name. |
| `s` | `string?` | Store identifier (see [Data model](#data-model-store-identifiers)). `null`/absent if skipped. |
| `p` | `number?` | Price in øre. `null`/absent if skipped. |
| `u` | `string?` | Listing URL. `null`/absent if skipped. |
| `t` | `string?` | CardMarket category (`"n"`/`"i"`/`"p"`). Absent for non-CM or skipped cards. |

Skipped cards have only the `c` field — all other fields are omitted.

**Store summary entry (`st[i]`):**

| Field | Type | Notes |
|---|---|---|
| `n` | `string` | Store identifier (see [Data model](#data-model-store-identifiers)). |
| `ct` | `number` | Card subtotal for this store (øre). |
| `sh` | `number` | Shipping cost for this store (øre). |
| `c` | `string?` | CardMarket category (`"n"`/`"i"`/`"p"`). Absent for non-CM stores. |

**Response (cache miss — background job):**
```json
{
  "job_id": "aB3dEfGh"
}
```

**Errors:**

| HTTP | Body | When |
|---|---|---|
| 400 | `"Tolerance too high: 6. Max is 5."` | Tolerance > 5 |
| 400 | `"No cached listings found. Run a /fetch first."` | No listings in cache |
| 400 | `"No recognized Magic card names found after resolution."` | All card names failed Scryfall resolution |
| 503 | `{"error":"Server busy: …","existing_job_id":"aB3dEfGh"}` | Another wizard job is running. The `existing_job_id` lets other users poll the running job's progress. |

---

### `GET /jobs/{id}`

Polls for job progress. Call this every 1–3 seconds while the job is running.

Every job response includes a `kind` field (`"fetch"` or `"wizard"`) so the
frontend knows which progress fields to display.

**Response when running (fetch job):**
```json
{
  "status": "running",
  "kind": "fetch",
  "cards_done": 12,
  "cards_total": 560,
  "current_store": "outland.no",
  "current_card": "Lightning Bolt",
  "tolerance_done": 0,
  "tolerance_total": 0,
  "combos_done": 0,
  "combos_total": 0,
  "store_statuses": [
    { "store": "outland.no",       "status": "fetching", "cards_found": 3 },
    { "store": "finn.no",          "status": "fetching", "cards_found": 1 },
    { "store": "collectible.no",   "status": "pending",  "cards_found": 0 },
    { "store": "adamstuenretro.no","status": "pending",  "cards_found": 0 },
    { "store": "cardmarket.com",   "status": "pending",  "cards_found": 0 }
  ]
}
```

**Response when running (wizard — heuristic phase):**
```json
{
  "status": "running",
  "kind": "wizard",
  "cards_done": 0,
  "cards_total": 0,
  "current_store": "",
  "current_card": "",
  "tolerance_done": 1,
  "tolerance_total": 3,
  "combos_done": 0,
  "combos_total": 9000
}
```

**Response when running (wizard — after heuristic, exhaustive in progress):**
```json
{
  "status": "running",
  "kind": "wizard",
  "cards_done": 0,
  "cards_total": 0,
  "current_store": "",
  "current_card": "",
  "tolerance_done": 1,
  "tolerance_total": 3,
  "combos_done": 1847,
  "combos_total": 9000,
  "partial_result": {
    "r": [
      {
        "a": [
          {"c": "Lightning Bolt", "s": "outland.no", "p": 200, "u": "https://…"},
          {"c": "Counterspell"}
        ],
        "sk": ["Counterspell"],
        "st": [{"n": "outland.no", "ct": 200, "sh": 2900}],
        "tc": 200,
        "ts": 2900,
        "ns": 1
      }
    ],
    "u": []
  }
}
```
`partial_result` is populated as soon as the heuristic phase completes, so the
frontend can display a good solution without waiting for the exhaustive pass.
`tolerance_done` resets to 0 at the start of phase 2 and counts up again.

**Field reference:**

| Field | Kind | Meaning |
|---|---|---|
| `status` | both | `"pending"` → `"running"` → `"done"` / `"failed"` |
| `kind` | both | `"fetch"` or `"wizard"` |
| `cards_done` | fetch | Card×store pairs processed so far |
| `cards_total` | fetch | Total card×store pairs. Progress% = done/total×100 |
| `current_store` | fetch | Store currently being queried (e.g. `"outland.no"`) |
| `current_card` | fetch | Card currently being searched |
| `tolerance_done` | wizard | Tolerance levels completed in current phase |
| `tolerance_total` | wizard | Total tolerance levels to try |
| `combos_done` | wizard | Store-swap trials attempted so far (phase 2) |
| `combos_total` | wizard | Estimated total store-swap trials (always populated for wizard) |
| `partial_result` | wizard | Heuristic result, available early while exhaustive runs in background |
| `result` | both | Final result object (only when `"done"`) — matches the `/fetch` or `/wizard` response format |
| `error` | both | Error message (only when `"failed"`) |
| `store_statuses` | fetch | Per-store progress breakdown. Status: `"pending"` → `"fetching"` → `"done"`. `cards_found` is total listing count for that store so far. Present on every fetch-job poll. |

**Done response (fetch):**
```json
{
  "status": "done",
  "kind": "fetch",
  "cards_done": 560,
  "cards_total": 560,
  "current_store": "outland.no",
  "current_card": "Lightning Bolt",
  "tolerance_done": 0,
  "tolerance_total": 0,
  "combos_done": 0,
  "combos_total": 0,
  "store_statuses": [
    { "store": "outland.no",       "status": "done", "cards_found": 145 },
    { "store": "cardmarket.com",   "status": "done", "cards_found": 82  }
  ],
  "result": {
    "r": {
      "Lightning Bolt": [
        {"s": "outland.no", "p": 200, "u": "https://…"},
        {"s": "SellerX",    "p": 350, "u": "https://…", "c": "n"}
      ],
      "Counterspell": [
        {"s": "SellerY", "p": 120, "u": "https://…", "c": "i"}
      ]
    },
    "u": []
  }
}
```

**Done response (wizard):**
```json
{
  "status": "done",
  "kind": "wizard",
  "cards_done": 0,
  "cards_total": 0,
  "current_store": "",
  "current_card": "",
  "tolerance_done": 3,
  "tolerance_total": 3,
  "combos_done": 9000,
  "combos_total": 9000,
  "result": {
    "r": [
      {
        "a": [
          {"c": "Lightning Bolt", "s": "outland.no", "p": 200, "u": "https://…"},
          {"c": "Counterspell"}
        ],
        "sk": ["Counterspell"],
        "st": [
          {"n": "outland.no", "ct": 200, "sh": 2900}
        ],
        "tc": 200,
        "ts": 2900,
        "ns": 1
      }
    ],
    "u": []
  }
}
```

The `result.r` array contains up to **3 solutions**, sorted best-first.

**Failed response:**
```json
{
  "status": "failed",
  "kind": "wizard",
  "cards_done": 0,
  "cards_total": 0,
  "current_store": "",
  "current_card": "",
  "tolerance_done": 1,
  "tolerance_total": 3,
  "combos_done": 0,
  "combos_total": 0,
  "error": "No valid solutions found."
}
```

**Error responses:**
| Status | Body | When |
|---|---|---|
| 404 | `"Job 'abc123' not found."` | Invalid or expired job ID |

All prices are in **integer øre** (NOK cents). Divide by 100 to display.

---

## Job queue behavior

### Concurrency limit

**Max 1 job per kind** can run at a time. A running `/fetch` won't block `/wizard` and vice versa — but starting a second job of the same kind returns `503`. This keeps per-store rate limiting intact for fetch jobs while allowing the wizard to run independently. The frontend should show a "Server busy" message and retry.

### Job lifecycle

```
POST /fetch  →  status: "pending"   (instant)
                  ↓
               status: "running"    (background thread starts)
                  ↓
               status: "done"        (results available)
```

### Cleanup

Completed and failed jobs are automatically removed **30 minutes** after creation. Polling a deleted job returns `404`.

### No persistence

Jobs live in memory only. Restarting the server loses all active and completed jobs. The SQLite cache (`cache.db`) persists fetch results, so a re-fetch after restart is fast (cache hits).

---

## Displaying store names

To reconstruct human-readable store names from the compact format:

```
function storeLabel(entry) {
  if (entry.c === "n") return `CM: ${entry.s}`;
  if (entry.c === "i") return `CM-INT: ${entry.s}`;
  if (entry.c === "p") return `CM-PRIV: ${entry.s}`;
  return entry.s;  // "outland.no", "finn.no", etc.
}
```

---

## Error handling checklist

- [x] Server unreachable → show connection error
- [x] `503` (server busy) → parse `existing_job_id` from the JSON body and poll that job to show progress to other users. Show "Server busy" message + retry button.
- [x] `400` (too many cards / tolerance too high / no recognized cards) → show validation before submitting
- [x] `404` on `/jobs/{id}` → show "Job expired" (30min cleanup)
- [x] `status: "failed"` → show the `error` field
- [x] Network timeout during poll → don't crash, just try next interval
- [x] `u` field in response → show warnings for misspelled/invalid card names
