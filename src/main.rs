mod cache;
mod output;
mod shipping;
mod stores;
mod wizard;

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
use wizard::{Strategy, WizardConfig};

/// Fetch MTG card availability from online stores.
#[derive(Parser)]
#[command(name = "cardfetch")]
#[command(about = "Search online stores for Magic: The Gathering singles availability")]
struct Cli {
    /// Path to a decklist file, or a single card name in quotes
    /// (e.g. --input cards.txt  or  --input "Lightning Bolt").
    #[arg(short, long)]
    input: String,

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
}

fn main() -> anyhow::Result<()> {
    let started = Instant::now();
    let cli = Cli::parse();

    // ── Read cards: --input is either a file path or a card name ──────
    let cards: Vec<String> = {
        let path = PathBuf::from(&cli.input);
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
            vec![cli.input.trim().to_string()]
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
        let listings = cache.get_listings(&unique_cards)?;

        if listings.is_empty() {
            let hint = format!("cardfetch --input {}", cli.input);
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

        let mut solutions: Vec<(usize, wizard::WizardSolution)> = Vec::new();
        for t in 0..=cli.tolerance {
            let config = WizardConfig {
                strategy: cli.strategy,
                tolerance: t,
                eu_destination: cli.eu_destination,
            };
            if let Some(sol) = wizard::optimize_input(&input, &config) {
                solutions.push((t, sol));
            }
            bar.inc(1);
        }
        bar.finish_and_clear();

        if solutions.is_empty() {
            eprintln!("No valid solutions found.");
        } else {
            output::print_wizard_summary(&solutions, strategy_name, &unique_cards);
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
    let num_cards = unique_cards.len();

    // Open cache (unless --no-cache)
    let cache: Option<Arc<Cache>> = if cli.no_cache {
        None
    } else {
        Some(Arc::new(Cache::open("cache.db")?))
    };

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
                                    if let Some(ref cache) = cache {
                                        let _ = cache.store(card_name, key, None);
                                    }
                                } else {
                                    for result in &sub_results {
                                        if let Some(ref cache) = cache {
                                            let _ = cache.store(
                                                card_name,
                                                &result.store_name,
                                                Some(std::slice::from_ref(result)),
                                            );
                                        }
                                        results_to_send.push(result.clone());
                                    }
                                }
                            }
                            Err(e) => {
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

    // Drop the sender so the channel closes when all threads finish
    drop(tx);

    // Collect all results (buffer to avoid interleaving from slow stores)
    let all_results: Vec<StoreResult> = rx.into_iter().collect();

    // Wait for all threads to finish
    for handle in handles {
        if let Err(e) = handle.join() {
            eprintln!("  [error] Store thread panicked: {:?}", e);
        }
    }

    bar.finish_and_clear();

    // Group results by card name, preserving original card order
    let mut grouped: std::collections::HashMap<&str, Vec<&StoreResult>> =
        std::collections::HashMap::new();
    for result in &all_results {
        grouped.entry(&result.card_name).or_default().push(result);
    }

    // Print results as a table
    output::print_table(&cards_ref, &grouped);

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
