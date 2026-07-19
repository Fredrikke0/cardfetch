# CardFetch

Fetch Magic: The Gathering card prices across Norwegian and European stores. Includes a purchase wizard that finds the optimal store assignments to minimize cost.

## Prerequisites

- **Rust** (1.80+) — [rustup.rs](https://rustup.rs)
- **Chrome or Chromium** — required for CardMarket (uses Cloudflare bypass)
- **Xvfb** — virtual display for headless Chrome on Linux

```bash
# Ubuntu/Debian
sudo apt install chromium-browser xvfb

# Fedora
sudo dnf install chromium xorg-x11-server-Xvfb

# macOS (no Xvfb needed — falls back to headless mode automatically)
# Just install Chrome/Chromium
```

## Install

```bash
git clone <repo-url>
cd cardfetch
cargo build --release
```

The binary is at `target/release/cardfetch`.

## CLI mode

Search a decklist file against all stores:

```bash
cardfetch --input cards.txt
```

### Decklist format

One card per line. The leading quantity is stripped automatically:

```
1 Lightning Bolt
4 Counterspell
2 Black Lotus
```

Or pass a single card directly:

```bash
cardfetch --input "Lightning Bolt"
```

### CLI flags

| Flag | Description |
|---|---|
| `--input`, `-i <FILE\|CARD>` | Decklist file or single card name |
| `--stores <a,b,...>` | Filter stores (substring match, e.g. `--stores outland,cardmarket`) |
| `--no-cache` | Skip cache, always fetch live |
| `--wizard` | Run purchase optimizer (reads from cache) |
| `--tolerance <N>` | Max cards the wizard may skip (default 0) |
| `--strategy <cheapest\|simplest>` | Optimization goal (default cheapest) |
| `--eu-destination` | Assume EU delivery — removes 25% VAT from non-Norwegian sellers |
| `--exhaustive` | Guaranteed optimal (≤12 cards only, slow) |
| `--verbose`, `-v` | Print per-request diagnostics |
| `--server` | Start HTTP API server |
| `--port <PORT>` | Server port (default 3000) |

### Example workflow

```bash
# 1. Fetch prices and populate the cache
cardfetch --input cards.txt

# 2. Run the wizard on cached data
cardfetch --input cards.txt --wizard --tolerance 2
```

## Server mode

Starts an HTTP API for a separate frontend:

```bash
cardfetch --server --port 3000
```

```
CardFetch API server on http://127.0.0.1:3000
  GET  /stores
  POST /fetch
  POST /wizard
  GET  /jobs/{id}
```

The server listens on **localhost only** (`127.0.0.1`). Full API documentation: [`API.md`](API.md)

### Quick curl test

```bash
# List stores
curl http://localhost:3000/stores

# Start a fetch (returns job ID immediately)
curl -X POST http://localhost:3000/fetch \
  -H "Content-Type: application/json" \
  -d '{"cards":["Lightning Bolt","Counterspell"],"stores":["outland"]}'

# Poll progress
curl http://localhost:3000/jobs/<job_id>
```

### Systemd service (optional)

```
# /etc/systemd/system/cardfetch.service
[Unit]
Description=CardFetch API
After=network.target

[Service]
ExecStart=/path/to/cardfetch --server --port 3000
Restart=always
User=cardfetch

[Install]
WantedBy=multi-user.target
```

## Stores

| Store | Method | Notes |
|---|---|---|
| outland.no | GraphQL API | Fast |
| finn.no | HTML scraping | |
| collectible.no | HTML scraping | |
| korthaien.no | HTML scraping | |
| midgardgames.no | HTML scraping | |
| pokeboks.no | HTML scraping | |
| adamstuenretro.no | HTML scraping | |
| cardmarket.com | Headless Chrome | Slowest — launches a real browser to bypass Cloudflare. Needs Chrome + Xvfb. |

## Cache

Results are stored in `cache.db` (SQLite) in the working directory. Subsequent runs hit the cache and skip live fetching for cards that haven't changed. Use `--no-cache` to force a fresh fetch.

The wizard also caches its solutions, so re-running with the same parameters is instant.

## Architecture

See [`DESIGN.md`](DESIGN.md) for the internal architecture, trait system, and how to add new stores.
