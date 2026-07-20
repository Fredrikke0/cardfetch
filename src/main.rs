mod cache;
mod output;
mod scryfall;
mod server;
mod shipping;
mod stores;
mod wizard;

use anyhow::Context;
use cache::{Cache, CacheLookup};
use clap::Parser;
use indicatif::{ProgressBar, ProgressStyle};
use rand::Rng;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use stores::{StoreResult, DELAY_JITTER_MS, DELAY_MS};
use wizard::{compute_raw_score, Strategy, WizardConfig};

/// Fetch MTG card availability from online stores.
#[derive(Parser)]
#[command(name = "cardfetch")]
#[command(about = "Search online stores for Magic: The Gathering singles availability")]
struct Cli {
    /// Path to a decklist file, or a single card name in quotes
    /// (e.g. --input cards.txt  or  --input "Lightning Bolt").
    /// Not required in --server mode.
    #[arg(short, long, required_unless_present = "server")]
    input: Option<String>,

    /// Bypass the cache and perform a fresh live search
    #[arg(long)]
    no_cache: bool,

    /// Comma-separated list of store name substrings to include (e.g. "outland,collectible")
    #[arg(long, value_delimiter = ',')]
    stores: Vec<String>,

    /// Run the purchase wizard to find optimal store assignments.
    /// Reads from cache; run a normal search first to populate it.
    #[arg(long)]
    wizard: bool,

    /// Optimization strategy: "simplest" (fewest stores) or "cheapest" (lowest total).
    #[arg(long, default_value = "cheapest")]
    strategy: Strategy,

    /// Maximum number of wanted cards the solution is allowed to skip.
    #[arg(long, default_value = "0")]
    tolerance: usize,

    /// Assume delivery within the EU — removes 25% VAT and customs fees
    /// from international CardMarket sellers in the wizard calculation.
    #[arg(long)]
    eu_destination: bool,

    /// Print verbose diagnostics.
    #[arg(short, long)]
    verbose: bool,

    /// Suppress the price summary table after scraping.
    #[arg(long)]
    no_summary: bool,

    /// Enter interactive rescue mode when CardMarket is blocked by Cloudflare.
    /// Failed pages are queued; after all automated fetching finishes, you'll
    /// be prompted to open each URL in your browser, expand all sellers, run a
    /// JS snippet in the console, and paste the result back.
    #[arg(long)]
    semi_manual: bool,

    /// Use exhaustive search (only for <= 12 cards).  Enumerates every
    /// possible store assignment to guarantee the optimal solution.
    #[arg(long)]
    exhaustive: bool,

    /// Start in HTTP API server mode instead of running a one-shot search.
    #[arg(long)]
    server: bool,

    /// Port for the HTTP server (only used with --server).
    #[arg(long, default_value = "3000")]
    port: u16,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // Server mode: start HTTP API and block forever.
    if cli.server {
        let stores = Arc::new(stores::all_stores(cli.verbose));
        let cache = if cli.no_cache {
            None
        } else {
            Some(Arc::new(Cache::open("cache.db")?))
        };

        let state = Arc::new(server::AppState::new(stores, cache));
        let app = server::build_router(state);

        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], cli.port));
        eprintln!("CardFetch API server on http://{addr}");
        eprintln!("  GET  /stores");
        eprintln!("  POST /fetch");
        eprintln!("  POST /wizard");
        eprintln!("  GET  /jobs/{{id}}");

        let rt = tokio::runtime::Runtime::new()?;
        rt.block_on(async {
            let listener = tokio::net::TcpListener::bind(&addr).await?;
            axum::serve(listener, app).await?;
            anyhow::Ok(())
        })?;
        return Ok(());
    }

    let started = Instant::now();
    let input = cli.input.as_ref().unwrap();

    // ── Read cards: --input is either a file path or a card name ──────
    let cards: Vec<String> = {
        let path = PathBuf::from(&input);
        if path.is_file() {
            let contents = std::fs::read_to_string(&path).map_err(|e| {
                anyhow::anyhow!("Failed to read input file '{}': {}", path.display(), e)
            })?;
            contents
                .lines()
                .map(|line| line.trim())
                .filter(|line| !line.is_empty())
                .map(strip_quantity)
                .collect()
        } else {
            // Not a file — treat as a single card name
            vec![input.trim().to_string()]
        }
    };

    if cards.is_empty() {
        anyhow::bail!("No cards to search.");
    }

    // Deduplicate card names (search each name only once)
    let mut unique_cards = cards;
    unique_cards.sort();
    unique_cards.dedup();

    // ── Wizard mode: run from cache ─────────────────────────────────────────
    if cli.wizard {
        let cache = Cache::open("cache.db")?;

        // Resolve card names via Scryfall before looking up listings.
        let unique_cards = {
            let client = reqwest::blocking::Client::builder()
                .user_agent("CardFetch/0.1")
                .timeout(Duration::from_secs(10))
                .build()
                .context("Failed to build Scryfall HTTP client")?;
            scryfall::resolve_with_cache(&client, &unique_cards, &Some(&cache))?
        };

        let listings = cache.get_listings(&unique_cards)?;

        if listings.is_empty() {
            let hint = format!("cardfetch --input {}", input);
            eprintln!(
                "No cached listings found for the requested cards.\n\
                 Run a normal search first: {hint}"
            );
            return Ok(());
        }

        let strategy_name = match cli.strategy {
            Strategy::Simplest => "simplest",
            Strategy::Cheapest => "cheapest",
        };

        // Prune solutions computed on stale data, solutions referencing
        // now-blacklisted sellers, and reset current-run flags.
        cache.prune_stale_solutions()?;
        let pruned = cache.prune_blacklisted_solutions()?;
        if pruned > 0 {
            eprintln!(
                "  [wizard] dropped {pruned} cached solution(s) — they referenced blacklisted sellers"
            );
        }
        cache.clear_current_solutions(strategy_name, cli.eu_destination)?;

        // Build input once — warn about uncached cards once.
        let input = wizard::WizardInput::from_results_and_wants(
            listings,
            &unique_cards,
            cli.eu_destination,
        );
        let uncached: Vec<&str> = input
            .cards
            .iter()
            .filter(|c| c.options.is_empty())
            .map(|c| c.name.as_str())
            .collect();
        if !uncached.is_empty() {
            eprintln!(
                "  [wizard] {} card(s) have no listings in cache: {}",
                uncached.len(),
                uncached.join(", ")
            );
        }

        // Run optimizer for each tolerance 0..=max.
        let num_steps = cli.tolerance + 1;
        let bar = ProgressBar::new(num_steps as u64).with_style(
            ProgressStyle::with_template(
                "{spinner:.green} [{elapsed_precise}] [{bar:30.cyan/blue}] {pos}/{len} tolerances",
            )
            .unwrap()
            .progress_chars("#>-"),
        );

        // Pre-compute exhaustive candidates once — they only depend on
        // `input`, not on the per-tolerance `config`.
        let exhaustive_candidates = if cli.exhaustive {
            Some(wizard::select_candidate_stores(&input))
        } else {
            None
        };

        // Load previous bests from BOTH strategies before the loop so we can
        // seed each tolerance from the better of current vs. cached.
        let other_strategy = match strategy_name {
            "cheapest" => "simplest",
            "simplest" => "cheapest",
            _ => strategy_name,
        };
        let mut prev_history = cache.load_best_solutions(strategy_name, cli.eu_destination)?;
        let other_history = cache.load_best_solutions(other_strategy, cli.eu_destination)?;
        for (t, rec) in other_history {
            match prev_history.get(&t) {
                Some(existing) if rec.total_cost < existing.total_cost => {
                    prev_history.insert(t, rec);
                }
                None => {
                    prev_history.insert(t, rec);
                }
                _ => {}
            }
        }
        if !prev_history.is_empty() {
            eprintln!(
                "  [wizard] {} previous solutions on record (both strategies)",
                prev_history.len()
            );
        }

        let mut solutions: Vec<(usize, wizard::WizardSolution)> = Vec::new();
        let mut search_mode: wizard::SearchMode = wizard::SearchMode::Heuristic { seed: None };
        for t in 0..=cli.tolerance {
            if let Some(ref candidates) = exhaustive_candidates {
                search_mode = wizard::SearchMode::Exhaustive {
                    candidates: candidates.clone(),
                };
            }
            let config = WizardConfig {
                strategy: cli.strategy,
                tolerance: t,
            };
            let results = wizard::optimize_input(&input, &config, &search_mode);
            if !results.is_empty() {
                let best = &results[0];
                // Seed the heuristic path for the next tolerance from whichever
                // is better: the current run's best solution or a previously cached
                // solution at this tolerance.
                if exhaustive_candidates.is_none() {
                    let seed_choices = match prev_history.get(&t) {
                        Some(prev) => {
                            let prev_score = compute_raw_score(
                                prev.total_cost,
                                prev.num_stores,
                                prev.num_skipped,
                                t,
                                cli.strategy,
                            );
                            if prev_score < best.score {
                                prev.raw_choices.clone()
                            } else {
                                best.raw_choices.clone()
                            }
                        }
                        None => best.raw_choices.clone(),
                    };
                    search_mode = wizard::SearchMode::Heuristic {
                        seed: Some(seed_choices),
                    };
                }
                for (_rank, sol) in results.into_iter().enumerate() {
                    solutions.push((t, sol));
                }
            }
            bar.inc(1);
        }
        bar.finish_and_clear();

        if solutions.is_empty() {
            eprintln!("No valid solutions found.");
        } else {
            // Save all solutions to cache with rank, and build the merged
            // display list (one row per tolerance — the best solution).
            let mut merged: Vec<(usize, wizard::WizardSolution)> = Vec::new();
            let mut seen_tol: std::collections::HashSet<usize> = std::collections::HashSet::new();
            // Track rank within each tolerance group (solutions are already
            // ordered best-first from optimize_input).
            let mut tol_rank: std::collections::HashMap<usize, usize> =
                std::collections::HashMap::new();

            for (t, cur_sol) in &solutions {
                let rank = tol_rank.entry(*t).or_insert(0);
                *rank += 1; // 1-based rank

                let is_better = match prev_history.get(t) {
                    Some(prev) => {
                        let prev_score = compute_raw_score(
                            prev.total_cost,
                            prev.num_stores,
                            prev.num_skipped,
                            *t,
                            cli.strategy,
                        );
                        cur_sol.score < prev_score
                    }
                    None => true, // no previous record
                };

                if is_better {
                    cache.save_wizard_solution(
                        strategy_name,
                        *t,
                        cli.eu_destination,
                        cli.exhaustive,
                        *rank,
                        cur_sol,
                    )?;
                }

                // Only add the best per tolerance to the display list.
                if !seen_tol.contains(t) {
                    seen_tol.insert(*t);
                    if is_better {
                        merged.push((*t, cur_sol.clone()));
                    } else {
                        // Previous solution is better — reconstruct and display it.
                        let prev = prev_history.get(t).unwrap();
                        let config = WizardConfig {
                            strategy: cli.strategy,
                            tolerance: *t,
                        };
                        let reconstructed =
                            wizard::solution_from_choices(&prev.raw_choices, &input, &config);
                        merged.push((*t, reconstructed));
                    }
                }
            }

            output::print_wizard_summary(&merged, strategy_name, &unique_cards, cli.eu_destination);
        }

        eprintln!(
            "Finished in {:.1} seconds.",
            started.elapsed().as_secs_f64()
        );
        return Ok(());
    }

    // ── Normal search mode ──────────────────────────────────────────────────

    let mut stores_list = stores::all_stores(cli.verbose);

    // Filter stores if --stores is provided
    if !cli.stores.is_empty() {
        let all_names: Vec<String> = stores_list
            .iter()
            .map(|s| s.name().to_lowercase())
            .collect();
        for filter in &cli.stores {
            let f = filter.trim().to_lowercase();
            if !all_names.iter().any(|n| n.contains(&f)) {
                anyhow::bail!("Store not found: '{}'", filter.trim());
            }
        }
        stores_list.retain(|s| {
            let name = s.name().to_lowercase();
            cli.stores
                .iter()
                .any(|f| name.contains(&f.trim().to_lowercase()))
        });
    }

    let num_stores = stores_list.len();

    // Open cache (unless --no-cache)
    let cache: Option<Arc<Cache>> = if cli.no_cache {
        None
    } else {
        Some(Arc::new(Cache::open("cache.db")?))
    };

    // Resolve card names via Scryfall autocomplete before searching stores.
    let unique_cards = {
        let client = reqwest::blocking::Client::builder()
            .user_agent("CardFetch/0.1")
            .timeout(Duration::from_secs(10))
            .build()
            .context("Failed to build Scryfall HTTP client")?;
        let cache_ref: Option<&Cache> = cache.as_ref().map(|c| c.as_ref());
        scryfall::resolve_with_cache(&client, &unique_cards, &cache_ref)?
    };

    let num_cards = unique_cards.len();

    // Per-card completion tracking: counts how many stores have finished each card
    let card_counts: Arc<Vec<AtomicUsize>> =
        Arc::new((0..num_cards).map(|_| AtomicUsize::new(0)).collect());

    // Per-store counters for the run summary
    #[derive(Default)]
    struct StoreCounts {
        cached: AtomicUsize,
        skipped: AtomicUsize,
        fetched: AtomicUsize,
        failed: AtomicUsize,
    }
    let store_stats: Arc<std::collections::HashMap<String, StoreCounts>> = Arc::new(
        stores_list
            .iter()
            .map(|s| (s.name().to_string(), StoreCounts::default()))
            .collect(),
    );

    // Spawn one thread per store
    if cli.semi_manual {
        stores::cardmarket::set_semi_manual(true);
    }
    let (tx, rx) = mpsc::channel::<StoreResult>();
    let cards_ref = Arc::new(unique_cards);
    let bar = Arc::new(
        ProgressBar::new(num_cards as u64).with_style(
            ProgressStyle::with_template(
                "{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} cards ({eta})",
            )
            .unwrap()
            .progress_chars("#>-"),
        ),
    );

    let mut handles = Vec::new();

    for store in stores_list {
        let tx = tx.clone();
        let cards = cards_ref.clone();
        let bar = bar.clone();
        let card_counts = card_counts.clone();
        let cache = cache.clone();
        let store_stats = store_stats.clone();
        let verbose = cli.verbose;

        let handle = std::thread::spawn(move || {
            let store_name = store.name().to_string();
            let timeout = Duration::from_secs(store.timeout_secs());

            let store_client = reqwest::blocking::Client::builder()
                .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36")
                .timeout(timeout)
                .build()
                .expect("Failed to build per-store HTTP client");

            let counts = &store_stats[&store_name];

            for (i, card_name) in cards.iter().enumerate() {
                // Each store may declare multiple independent cache keys
                // (e.g. CardMarket has separate keys for Norwegian,
                // international powerseller, and international private
                // searches).  Each key gets its own cache lookup and,
                // if stale, its own live fetch.
                let cache_keys = store.cache_keys();
                let mut results_to_send: Vec<StoreResult> = Vec::new();
                let mut keys_to_fetch: Vec<String> = Vec::new();

                for key in &cache_keys {
                    let lookup = cache.as_ref().map(|c| c.lookup(card_name, key)).transpose();
                    match lookup {
                        Ok(Some(CacheLookup::Hit(results))) => {
                            counts.cached.fetch_add(1, Ordering::Relaxed);
                            if verbose {
                                bar.suspend(|| {
                                    eprintln!(
                                        "  [{}] cache hit: '{}' ({} result(s))",
                                        key,
                                        card_name,
                                        results.len()
                                    );
                                });
                            }
                            results_to_send.extend(results);
                        }
                        Ok(Some(CacheLookup::Skip)) => {
                            counts.skipped.fetch_add(1, Ordering::Relaxed);
                            if verbose {
                                bar.suspend(|| {
                                    eprintln!("  [{}] cache skip: '{}'", key, card_name);
                                });
                            }
                        }
                        _ => {
                            keys_to_fetch.push(key.clone());
                        }
                    }
                }

                // Fetch any sub-stores that need a live search.
                if !keys_to_fetch.is_empty() {
                    counts
                        .fetched
                        .fetch_add(keys_to_fetch.len(), Ordering::Relaxed);
                    std::thread::sleep(Duration::from_millis(
                        DELAY_MS + rand::thread_rng().gen_range(0..DELAY_JITTER_MS),
                    ));

                    for key in &keys_to_fetch {
                        if verbose {
                            bar.suspend(|| {
                                eprintln!("  [{}] live fetch: '{}'", key, card_name);
                            });
                        }
                        match store.search_sub(&store_client, card_name, key) {
                            Ok(sub_results) => {
                                if sub_results.is_empty() {
                                    // Don't negative-cache if CardMarket is
                                    // blocked — an empty result here means
                                    // we gave up, not that there are no
                                    // sellers.  The semi-manual run needs
                                    // a clean cache lookup to queue rescues.
                                    let gave_up = key.starts_with("cardmarket")
                                        && stores::cardmarket::is_blocked();
                                    if !gave_up {
                                        if let Some(ref cache) = cache {
                                            let _ = cache.store(card_name, key, None);
                                        }
                                    }
                                } else {
                                    // Group results by store_name before caching.
                                    // A seller may have multiple offers for the
                                    // same card (different conditions/versions);
                                    // cache.store() DELETEs all old rows for the
                                    // (card_name, store_name) pair first, so
                                    // calling it per-result would keep only the
                                    // last (most expensive) offer.
                                    if let Some(ref cache) = cache {
                                        let mut by_store: std::collections::HashMap<
                                            String,
                                            Vec<StoreResult>,
                                        > = std::collections::HashMap::new();
                                        for result in &sub_results {
                                            by_store
                                                .entry(result.store_name.clone())
                                                .or_default()
                                                .push(result.clone());
                                        }
                                        for (store_name, grouped) in &by_store {
                                            let _ = cache.store(
                                                card_name,
                                                store_name,
                                                Some(grouped.as_slice()),
                                            );
                                        }
                                    }
                                    for result in sub_results {
                                        results_to_send.push(result);
                                    }
                                }
                            }
                            Err(e) => {
                                // In semi-manual mode, Cloudflare-blocked
                                // CardMarket pages are queued for rescue.
                                if e.downcast_ref::<stores::cardmarket::RescuePending>()
                                    .is_some()
                                {
                                    if verbose {
                                        bar.suspend(|| {
                                            eprintln!(
                                                "  [{}] queued for rescue: '{}'",
                                                key, card_name
                                            );
                                        });
                                    }
                                } else {
                                    counts.failed.fetch_add(1, Ordering::Relaxed);
                                    bar.suspend(|| {
                                        eprintln!(
                                            "  [{}] Failed to search '{}': {}",
                                            key, card_name, e
                                        );
                                    });
                                }
                            }
                        }
                    }
                }

                // Send all collected results (cached + freshly fetched).
                for result in results_to_send {
                    if tx.send(result).is_err() {
                        break;
                    }
                }

                // Mark this store done for card i; advance bar when all stores done
                let prev = card_counts[i].fetch_add(1, Ordering::Relaxed);
                if prev + 1 == num_stores {
                    bar.inc(1);
                }
            }
        });

        handles.push(handle);
    }

    // Wait for all store threads to finish (threads drop their tx clones
    // on exit).  We keep our tx alive for the rescue phase.
    for handle in handles {
        if let Err(e) = handle.join() {
            eprintln!("  [error] Store thread panicked: {:?}", e);
        }
    }

    // ── Semi-manual rescue phase ─────────────────────────────────────────────
    let mut rescued_count: usize = 0;
    if cli.semi_manual {
        let queue = stores::cardmarket::drain_rescue_queue();
        if !queue.is_empty() {
            bar.finish_and_clear();
            eprintln!(
                "\n=== CardMarket semi-manual rescue: {} page(s) need human help ===\n",
                queue.len()
            );
            eprintln!(
                "For each URL below:\n\
                 1. Open the URL in your browser\n\
                 2. Click \"Show more results\" until all sellers are loaded\n\
                 3. Press F12 → Console, paste this snippet, press Enter:"
            );
            eprintln!("{}", stores::cardmarket::rescue_js_snippet());
            eprintln!("  Seller data is now on your clipboard.");

            // Detect clipboard tool
            let clip_cmd: Option<&str> = {
                let candidates = ["xclip", "wl-paste"];
                candidates
                    .iter()
                    .find(|cmd| {
                        std::process::Command::new("which")
                            .arg(cmd)
                            .stdout(std::process::Stdio::null())
                            .stderr(std::process::Stdio::null())
                            .status()
                            .map(|s| s.success())
                            .unwrap_or(false)
                    })
                    .copied()
            };

            for item in &queue {
                eprintln!(
                    "[{sub}] {card}\n  {url}",
                    sub = item.sub_key,
                    card = item.card_name,
                    url = item.url
                );

                eprint!("  Run the JS snippet, then press Enter to read clipboard...");
                let _ = std::io::Write::flush(&mut std::io::stdout());
                {
                    let mut dummy = String::new();
                    let _ = std::io::stdin().read_line(&mut dummy);
                }

                let json: String = if let Some(cmd) = clip_cmd {
                    let args = if cmd == "xclip" {
                        vec!["-o", "-selection", "clipboard"]
                    } else {
                        vec![]
                    };
                    match std::process::Command::new(cmd).args(&args).output() {
                        Ok(out) if out.status.success() => match String::from_utf8(out.stdout) {
                            Ok(s) => s.trim().to_string(),
                            Err(_) => {
                                eprintln!("  Clipboard content is not valid UTF-8.");
                                continue;
                            }
                        },
                        _ => {
                            eprintln!("  Clipboard read failed.");
                            continue;
                        }
                    }
                } else {
                    eprintln!(
                        "  No clipboard tool found (install xclip or wl-clipboard).\n\
                          Type 'skip' to skip, 'quit' to stop, or paste a file path:"
                    );
                    eprint!("  > ");
                    let _ = std::io::Write::flush(&mut std::io::stdout());
                    let mut line = String::new();
                    if std::io::stdin().read_line(&mut line).is_err() {
                        continue;
                    }
                    let trimmed = line.trim();
                    if trimmed.eq_ignore_ascii_case("quit") {
                        eprintln!("  Rescue aborted.");
                        break;
                    }
                    if trimmed.eq_ignore_ascii_case("skip") {
                        eprintln!("  Skipped.");
                        continue;
                    }
                    // Try as file path
                    match std::fs::read_to_string(trimmed) {
                        Ok(s) => s,
                        Err(e) => {
                            eprintln!("  Could not read file: {e}");
                            continue;
                        }
                    }
                };

                if json.is_empty() {
                    eprintln!("  Empty clipboard. Run the JS snippet first.");
                    continue;
                }

                match stores::cardmarket::sellers_from_json(&json, &item.card_name, &item.sub_key) {
                    Ok(results) => {
                        eprintln!("  Got {} seller(s).", results.len());
                        if results.is_empty() {
                            if let Some(ref cache) = cache {
                                let _ = cache.store(&item.card_name, &item.sub_key, None);
                            }
                        } else {
                            for result in &results {
                                if let Some(ref cache) = cache {
                                    let _ = cache.store(
                                        &item.card_name,
                                        &result.store_name,
                                        Some(std::slice::from_ref(result)),
                                    );
                                }
                                if tx.send(result.clone()).is_err() {
                                    break;
                                }
                            }
                            rescued_count += 1;
                        }
                    }
                    Err(e) => {
                        eprintln!("  Parse error: {e}");
                    }
                }
            }

            if rescued_count > 0 {
                eprintln!(
                    "\n  Rescued {rescued} page(s) — re-run the wizard with --semi-manual to include them.",
                    rescued = rescued_count
                );
            }
        }
    }

    // Close the channel — all threads + rescue are done.
    drop(tx);

    // Collect all results (buffer to avoid interleaving from slow stores)
    let all_results: Vec<StoreResult> = rx.into_iter().collect();

    bar.finish_and_clear();

    // Group results by card name, preserving original card order
    let mut grouped: std::collections::HashMap<&str, Vec<&StoreResult>> =
        std::collections::HashMap::new();
    for result in &all_results {
        grouped.entry(&result.card_name).or_default().push(result);
    }

    // Print results as a table
    if !cli.no_summary {
        output::print_table(&cards_ref, &grouped);
    }

    // Per-store summary
    eprintln!();
    eprintln!(
        "  Finished in {:.1} seconds",
        started.elapsed().as_secs_f64()
    );
    let mut store_names: Vec<&String> = store_stats.keys().collect();
    store_names.sort();
    for name in &store_names {
        let s = &store_stats[*name];
        let (c, sk, f, fl) = (
            s.cached.load(Ordering::Relaxed),
            s.skipped.load(Ordering::Relaxed),
            s.fetched.load(Ordering::Relaxed),
            s.failed.load(Ordering::Relaxed),
        );
        let mut parts = vec![format!("{} lookups", c + sk + f)];
        if c > 0 {
            parts.push(format!("{} cached", c));
        }
        if sk > 0 {
            parts.push(format!("{} skipped", sk));
        }
        if f > 0 {
            parts.push(format!("{} fetched", f));
        }
        if fl > 0 {
            parts.push(format!("{} failed", fl));
        }
        eprintln!("  {:<30} {}", name, parts.join(", "));
    }

    // If CardMarket got blocked, remind the user
    if stores::cardmarket::is_blocked() {
        eprintln!();
        eprintln!("  ⚠ CardMarket is blocking fetch attempts. Try from a different IP.");
    }

    Ok(())
}

/// Strip a leading quantity from a line like "1 Snakeskin Veil" → "Snakeskin Veil".
fn strip_quantity(line: &str) -> String {
    let trimmed = line.trim();
    if let Some((first, rest)) = trimmed.split_once(char::is_whitespace) {
        if first.parse::<u32>().is_ok() {
            return rest.trim().to_string();
        }
    }
    trimmed.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_quantity() {
        assert_eq!(strip_quantity("1 Snakeskin Veil"), "Snakeskin Veil");
        assert_eq!(strip_quantity("4 Lightning Bolt"), "Lightning Bolt");
        assert_eq!(strip_quantity("Snakeskin Veil"), "Snakeskin Veil");
        assert_eq!(strip_quantity("  2  Double Space  "), "Double Space");
        assert_eq!(strip_quantity("No Number Here"), "No Number Here");
    }
}
