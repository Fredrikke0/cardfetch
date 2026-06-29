mod cache;
mod output;
mod stores;

use cache::{Cache, CacheLookup};
use clap::Parser;
use indicatif::{ProgressBar, ProgressStyle};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use stores::{StoreResult, DELAY_MS};

/// Fetch MTG card availability from online stores.
#[derive(Parser)]
#[command(name = "cardfetch")]
#[command(about = "Search online stores for Magic: The Gathering singles availability")]
struct Cli {
    /// Path to a newline-separated decklist file (with quantities)
    #[arg(short, long)]
    input: PathBuf,

    /// Bypass the cache and perform a fresh live search
    #[arg(long)]
    no_cache: bool,

    /// Comma-separated list of store name substrings to include (e.g. "outland,collectible")
    #[arg(long, value_delimiter = ',')]
    stores: Vec<String>,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // Read and parse decklist
    let contents = std::fs::read_to_string(&cli.input).map_err(|e| {
        anyhow::anyhow!("Failed to read input file '{}': {}", cli.input.display(), e)
    })?;

    let cards: Vec<String> = contents
        .lines()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty())
        .map(strip_quantity)
        .collect();

    if cards.is_empty() {
        anyhow::bail!("Input file is empty or contains only whitespace.");
    }

    // Deduplicate card names (search each name only once)
    let mut unique_cards = cards;
    unique_cards.sort();
    unique_cards.dedup();

    let mut stores_list = stores::all_stores();

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

    let store_names: Vec<String> = stores_list.iter().map(|s| s.name().to_string()).collect();
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

        let handle = std::thread::spawn(move || {
            let store_name = store.name().to_string();
            let delay = Duration::from_millis(DELAY_MS);
            let timeout = Duration::from_secs(store.timeout_secs());

            let store_client = reqwest::blocking::Client::builder()
                .user_agent("Mozilla/5.0 (compatible; cardfetch/0.1)")
                .timeout(timeout)
                .build()
                .expect("Failed to build per-store HTTP client");

            for (i, card_name) in cards.iter().enumerate() {
                let lookup = cache
                    .as_ref()
                    .map(|c| c.lookup(card_name, &store_name))
                    .transpose();

                match lookup {
                    Ok(Some(CacheLookup::Hit(result))) => {
                        // Positive cache hit — return immediately
                        if tx.send(result).is_err() {
                            break;
                        }
                    }
                    Ok(Some(CacheLookup::Skip)) => {
                        // Negative cache hit — no result expected
                    }
                    Ok(Some(CacheLookup::Search)) | Ok(None) => {
                        // Cache miss, stale, or disabled — perform live search
                        std::thread::sleep(delay);

                        match store.search(&store_client, card_name) {
                            Ok(Some(result)) => {
                                if let Some(ref cache) = cache {
                                    let _ = cache.store(card_name, &store_name, Some(&result));
                                }
                                if tx.send(result).is_err() {
                                    break;
                                }
                            }
                            Ok(None) => {
                                if let Some(ref cache) = cache {
                                    let _ = cache.store(card_name, &store_name, None);
                                }
                            }
                            Err(e) => {
                                bar.suspend(|| {
                                    eprintln!(
                                        "  [{}] Failed to search '{}': {}",
                                        store.name(),
                                        card_name,
                                        e
                                    );
                                });
                            }
                        }
                    }
                    Err(e) => {
                        // Cache lookup error — fall back to live search
                        bar.suspend(|| {
                            eprintln!(
                                "  [{}] Cache lookup failed for '{}': {}",
                                store.name(),
                                card_name,
                                e
                            );
                        });
                        std::thread::sleep(delay);

                        match store.search(&store_client, card_name) {
                            Ok(Some(result)) => {
                                if tx.send(result).is_err() {
                                    break;
                                }
                            }
                            Ok(None) => {}
                            Err(e) => {
                                bar.suspend(|| {
                                    eprintln!(
                                        "  [{}] Failed to search '{}': {}",
                                        store.name(),
                                        card_name,
                                        e
                                    );
                                });
                            }
                        }
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
    let store_name_refs: Vec<&str> = store_names.iter().map(|s| s.as_str()).collect();
    output::print_table(&cards_ref, &grouped, &store_name_refs);

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
