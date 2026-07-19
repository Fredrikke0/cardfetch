# CardFetch API — Frontend Implementation Guide

## Base URL

```
http://localhost:3000
```

All endpoints return JSON. The server only listens on `127.0.0.1` — no network access.

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

Submits a card search. Behavior depends on `cache_only`:

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
  "cache_only": false
}
```

| Field | Type | Default | Notes |
|---|---|---|---|
| `cards` | `string[]` | — | Max 100 unique cards. Duplicates are ignored. |
| `stores` | `string[]` | `[]` | Substring match against store names. Empty = all stores. |
| `cache_only` | `boolean` | `false` | If true, only return cached data — never blocks. |

**Response (all cached, or `cache_only: true`):**
```json
{
  "results": {
    "Lightning Bolt": [
      { "store": "outland.no", "price": 200, "url": "https://…" }
    ]
  }
}
```

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
| 503 | `"Server busy: 1 fetch job(s) already running. …"` | Another fetch job is running (never with `cache_only`). A wizard job does not block fetch. |

---

### `POST /wizard`

Finds the optimal store assignments from previously fetched (cached) card listings.
Returns an array of up to **3 solutions**, sorted best-first. Exhaustive mode
always returns up to 3 alternatives (different store combinations with similar cost);
heuristic mode returns 1.

If a suitable previously-computed solution is cached, it is returned immediately without
creating a background job. A cached solution is "suitable" if it was computed
exhaustively, or if the current request is also non-exhaustive (heuristic).
A non-exhaustive cached solution triggers a fresh computation only when the
current request is exhaustive.

**Prerequisite:** Run `/fetch` first for the same cards so listings are in the cache.

**Request:**
```json
{
  "cards": ["Lightning Bolt", "Counterspell"],
  "tolerance": 2,
  "eu_destination": false,
  "strategy": "cheapest",
  "exhaustive": false
}
```

| Field | Type | Default | Notes |
|---|---|---|---|
| `cards` | `string[]` | — | Max 100. Must have been fetched first. |
| `tolerance` | `number` | `0` | Max cards the solution may skip. **Max 5.** |
| `eu_destination` | `boolean` | `false` | Removes 25% VAT from non-Norwegian sellers. |
| `strategy` | `string` | `"cheapest"` | `"cheapest"` or `"simplest"` |
| `exhaustive` | `boolean` | `false` | Guaranteed optimal, but only works for ≤12 cards. Slow. |

**Response (cached solution hit — no job):**
```json
{
  "results": [
    {
      "assignments": [
        { "card": "Lightning Bolt", "store": "outland.no", "price": 200, "url": "https://…" }
      ],
      "skipped": ["Counterspell"],
      "stores": [{ "name": "outland.no", "card_total": 200, "shipping": 2900 }],
      "total_card_cost": 200,
      "total_shipping": 2900,
      "num_stores": 1
    }
  ]
}
```

The `results` array contains up to **3 solutions**, sorted best-first (lowest internal score).
Exhaustive mode always returns up to 3 alternatives; heuristic mode returns 1.
Each solution object has the same shape:

| Field | Type | Notes |
|---|---|---|
| `assignments` | `object[]` | One per card. `store`/`price`/`url` are `null` if skipped. |
| `skipped` | `string[]` | Card names that couldn't be assigned within tolerance. |
| `stores` | `object[]` | Stores used. `card_total` and `shipping` are in integer øre. |
| `total_card_cost` | `number` | Sum of assigned card prices (øre). |
| `total_shipping` | `number` | Sum of shipping costs (øre). |
| `num_stores` | `number` | Number of distinct stores used. |

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
  "combos_total": 0
}
```

**Response when running (wizard job, heuristic):**
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
  "combos_total": 0
}
```

**Response when running (wizard job, exhaustive):**
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
  "combos_done": 523,
  "combos_total": 2048
}
```

**Field reference:**

| Field | Kind | Meaning |
|---|---|---|
| `status` | both | `"pending"` → `"running"` → `"done"` / `"failed"` |
| `kind` | both | `"fetch"` or `"wizard"` |
| `cards_done` | fetch | Card×store pairs processed so far |
| `cards_total` | fetch | Total card×store pairs. Progress% = done/total×100 |
| `current_store` | fetch | Store currently being queried (e.g. `"outland.no"`) |
| `current_card` | fetch | Card currently being searched |
| `tolerance_done` | wizard | Tolerance levels completed so far |
| `tolerance_total` | wizard | Total tolerance levels to try |
| `combos_done` | wizard | Store combinations evaluated (exhaustive only) |
| `combos_total` | wizard | Total store combinations to evaluate (exhaustive only; 0 for heuristic) |
| `result` | both | Final result object (only when `"done"`) |
| `error` | both | Error message (only when `"failed"`) |

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
  "result": {
    "Lightning Bolt": [
      { "store": "outland.no",    "price": 200, "url": "https://…" },
      { "store": "cardmarket.com", "price": 350, "url": "https://…" }
    ],
    "Counterspell": [
      { "store": "cardmarket.com", "price": 120, "url": "https://…" }
    ]
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
  "combos_done": 2048,
  "combos_total": 2048,
  "result": [
    {
      "assignments": [
        { "card": "Lightning Bolt", "store": "outland.no",    "price": 200, "url": "https://…" },
        { "card": "Counterspell",   "store": null,             "price": null, "url": null }
      ],
      "skipped": ["Counterspell"],
      "stores": [
        { "name": "outland.no", "card_total": 200, "shipping": 2900 }
      ],
      "total_card_cost": 200,
      "total_shipping": 2900,
      "num_stores": 1
    }
  ]
}
```

The `result` array contains up to **3 solutions**, sorted best-first.
Exhaustive mode always returns up to 3 alternatives; heuristic mode returns 1.

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

All prices are in **integer øre** (NOK cents) or euro cents. Divide by 100 to display.

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

## Error handling checklist

- [x] Server unreachable → show connection error
- [x] `503` (server busy) → show "Server busy" message + retry button
- [x] `400` (too many cards / tolerance too high) → show validation before submitting
- [x] `404` on `/jobs/{id}` → show "Job expired" (30min cleanup)
- [x] `status: "failed"` → show the `error` field
- [x] Network timeout during poll → don't crash, just try next interval
