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

Submits a card search. Returns a job ID **immediately** — the actual search runs in the background.

**Request:**
```json
{
  "cards": ["Lightning Bolt", "Counterspell"],
  "stores": ["outland", "cardmarket"]
}
```

| Field | Type | Required | Notes |
|---|---|---|---|
| `cards` | `string[]` | Yes | Max 100 unique cards. Duplicates are ignored. |
| `stores` | `string[]` | No | Case-insensitive substring match against store names. Omit or empty = all stores. |

**Response:** `200 OK`
```json
{
  "job_id": "XsAcq8nz"
}
```

**Errors:**

| Status | Body | When |
|---|---|---|
| 400 | `"Too many cards: 101. Max is 100."` | Cards exceed limit |
| 400 | `"No cards provided."` | Empty card list |
| 503 | `"Server busy: 5 job(s) already running. …"` | Max 5 concurrent jobs reached |

---

### `POST /wizard`

Finds the optimal store assignments from previously fetched (cached) card listings. Same job pattern as `/fetch`.

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
| `tolerance` | `number` | `0` | Max cards the solution may skip. |
| `eu_destination` | `boolean` | `false` | Removes 25% VAT from non-Norwegian sellers. |
| `strategy` | `string` | `"cheapest"` | `"cheapest"` or `"simplest"` |
| `exhaustive` | `boolean` | `false` | Guaranteed optimal, but only works for ≤12 cards. Slow. |

**Response:** `200 OK`
```json
{
  "job_id": "aB3dEfGh"
}
```

---

### `GET /jobs/{id}`

Polls for job progress. Call this every 1–3 seconds while the job is running.

**Response when running (fetch job):**
```json
{
  "status": "running",
  "cards_done": 12,
  "cards_total": 560,
  "current_store": "outland.no",
  "current_card": "Lightning Bolt",
  "tolerance_done": 0,
  "tolerance_total": 0
}
```

**Response when running (wizard job):**
```json
{
  "status": "running",
  "cards_done": 70,
  "cards_total": 70,
  "current_store": "",
  "current_card": "",
  "tolerance_done": 1,
  "tolerance_total": 3
}
```

**Response when done (fetch):**
```json
{
  "status": "done",
  "cards_done": 560,
  "cards_total": 560,
  "current_store": "outland.no",
  "current_card": "Lightning Bolt",
  "tolerance_done": 0,
  "tolerance_total": 0,
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

**Response when done (wizard):**
```json
{
  "status": "done",
  "cards_done": 70,
  "cards_total": 70,
  "tolerance_done": 3,
  "tolerance_total": 3,
  "result": {
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
}
```

**Response when failed:**
```json
{
  "status": "failed",
  "cards_done": 12,
  "cards_total": 560,
  "error": "No valid solutions found."
}
```

| Field | Always present? | Meaning |
|---|---|---|
| `status` | Yes | `"pending"` → `"running"` → `"done"` / `"failed"` |
| `cards_done` | Yes | Cards×stores processed so far (fetch) or total cards (wizard) |
| `cards_total` | Yes | Total work units. Progress % = `cards_done / cards_total × 100` |
| `current_store` | Yes | Empty string when not running |
| `current_card` | Yes | Empty string when not running |
| `tolerance_done` | Yes | Only meaningful for wizard jobs |
| `tolerance_total` | Yes | Only meaningful for wizard jobs |
| `result` | Only when `done` | The final result object |
| `error` | Only when `failed` | Human-readable error |

**Error responses:**
| Status | Body | When |
|---|---|---|
| 404 | `"Job 'abc123' not found."` | Invalid or expired job ID |

All prices are in **integer øre** (NOK cents) or euro cents. Divide by 100 to display.

---

## Job queue behavior

### Concurrency limit

**Max 5 jobs** can be pending+running at once. Attempting to start a 6th returns `503` with an error message. The frontend should show this to the user and offer a retry.

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

## Frontend pseudocode

```javascript
const API = "http://localhost:3000";

// 1. Get available stores on page load
const stores = await fetch(`${API}/stores`).then(r => r.json());

// 2. Start a fetch
async function startFetch(cards, storeFilter = []) {
  const res = await fetch(`${API}/fetch`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ cards, stores: storeFilter })
  });
  if (!res.ok) throw new Error(await res.text());
  return (await res.json()).job_id;
}

// 3. Poll until done
function pollJob(jobId, { onProgress, onDone, onError }) {
  const interval = setInterval(async () => {
    const job = await fetch(`${API}/jobs/${jobId}`).then(r => r.json());

    if (job.status === "running") {
      onProgress({
        pct: job.cards_done / job.cards_total * 100,
        currentCard: job.current_card,
        currentStore: job.current_store,
        tolerance: job.tolerance_done,
        toleranceTotal: job.tolerance_total,
      });
    }

    if (job.status === "done") {
      clearInterval(interval);
      onDone(job.result);
    }

    if (job.status === "failed") {
      clearInterval(interval);
      onError(job.error);
    }
  }, 2000);
}

// 4. Usage
const jobId = await startFetch(myDecklist, ["outland", "cardmarket"]);
pollJob(jobId, {
  onProgress: ({ pct, currentCard, currentStore }) => {
    progressBar.style.width = `${pct}%`;
    statusEl.textContent = `Searching ${currentCard} on ${currentStore}…`;
  },
  onDone: results => renderResults(results),
  onError: msg => alert(`Search failed: ${msg}`),
});
```

---

## Error handling checklist

- [x] Server unreachable → show connection error
- [x] `503` (server busy) → show "Server busy" message + retry button
- [x] `400` (too many cards) → show validation before submitting
- [x] `404` on `/jobs/{id}` → show "Job expired" (30min cleanup)
- [x] `status: "failed"` → show the `error` field
- [x] Network timeout during poll → don't crash, just try next interval
