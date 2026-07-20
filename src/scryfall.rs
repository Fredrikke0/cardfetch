use anyhow::Context;
use serde::Deserialize;
use std::time::Duration;

const AUTOCOMPLETE_URL: &str = "https://api.scryfall.com/cards/autocomplete";
/// Minimum delay between Scryfall API requests (10/second → 100ms).
const SCRYFALL_DELAY_MS: u64 = 120;

#[derive(Debug, Deserialize)]
struct AutocompleteResponse {
    data: Vec<String>,
}

/// Resolve a user-provided card name to a canonical name via the Scryfall
/// autocomplete API.  Returns `Ok(None)` if no match is found.
///
/// Rate limit: 10 req/sec.  Callers must ensure at least 100ms between calls.
pub fn resolve_name(
    client: &reqwest::blocking::Client,
    input: &str,
) -> anyhow::Result<Option<String>> {
    let url = format!("{}?q={}", AUTOCOMPLETE_URL, urlencoding(input));

    let response = client
        .get(&url)
        .header("User-Agent", "CardFetch/0.1 (cardfetch-cli)")
        .header("Accept", "application/json;q=0.9,*/*;q=0.8")
        .send()
        .context("Failed to send Scryfall autocomplete request")?;

    if !response.status().is_success() {
        anyhow::bail!(
            "Scryfall autocomplete returned HTTP {}",
            response.status().as_u16()
        );
    }

    let body: AutocompleteResponse = response
        .json()
        .context("Failed to parse Scryfall autocomplete response")?;

    // Take the first suggestion (nearest match, sorted best-first).
    Ok(body.data.into_iter().next())
}

/// Resolve multiple card names, sleeping between requests to respect the
/// Scryfall rate limit.  Returns a list of `(original, resolved)` pairs
/// and a list of names that could not be resolved.
#[allow(dead_code)]
pub fn resolve_batch(
    client: &reqwest::blocking::Client,
    names: &[String],
) -> (Vec<(String, String)>, Vec<String>) {
    let mut resolved: Vec<(String, String)> = Vec::new();
    let mut unresolved: Vec<String> = Vec::new();

    for name in names {
        match resolve_name(client, name) {
            Ok(Some(canonical)) => {
                resolved.push((name.clone(), canonical));
            }
            Ok(None) => {
                unresolved.push(name.clone());
            }
            Err(e) => {
                eprintln!("Warning: Scryfall lookup failed for '{}': {}", name, e);
                // Don't treat network errors as unresolvable -- keep the
                // original name rather than skipping the card entirely.
                resolved.push((name.clone(), name.clone()));
            }
        }

        // Respect rate limit between requests.
        if names.len() > 1 {
            std::thread::sleep(Duration::from_millis(SCRYFALL_DELAY_MS));
        }
    }

    (resolved, unresolved)
}

/// Resolve card names via Scryfall with local SQLite caching.
///
/// Checks `cache.lookup_scryfall()` first, falls back to live API calls,
/// and stores fresh results.  Unrecognized names are skipped with a warning.
/// Returns the deduplicated list of canonical card names.
pub fn resolve_with_cache(
    client: &reqwest::blocking::Client,
    names: &[String],
    cache: &Option<&crate::cache::Cache>,
) -> anyhow::Result<Vec<String>> {
    let mut resolved: Vec<String> = Vec::new();

    for name in names {
        // Check local cache first
        let cached = cache.and_then(|c| c.lookup_scryfall(name).transpose());

        let canonical = match cached {
            Some(Ok(canonical)) => Some(canonical),
            _ => match resolve_name(client, name) {
                Ok(Some(canonical)) => {
                    if let Some(c) = cache {
                        let _ = c.store_scryfall(name, &canonical);
                    }
                    Some(canonical)
                }
                Ok(None) => {
                    eprintln!(
                        "Warning: '{}' is not a recognized Magic card -- skipping.",
                        name
                    );
                    None
                }
                Err(e) => {
                    eprintln!(
                        "Warning: Scryfall lookup failed for '{}': {} -- using original name.",
                        name, e
                    );
                    Some(name.clone())
                }
            },
        };

        if let Some(name) = canonical {
            resolved.push(name);
        }

        std::thread::sleep(Duration::from_millis(SCRYFALL_DELAY_MS));
    }

    resolved.sort();
    resolved.dedup();

    anyhow::ensure!(
        !resolved.is_empty(),
        "No recognized Magic card names found after resolution."
    );

    Ok(resolved)
}

/// Simple URL-encode for query parameters.
fn urlencoding(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => {
                result.push(ch);
            }
            ' ' => result.push_str("%20"),
            '!' => result.push_str("%21"),
            '"' => result.push_str("%22"),
            '#' => result.push_str("%23"),
            '$' => result.push_str("%24"),
            '%' => result.push_str("%25"),
            '&' => result.push_str("%26"),
            '\'' => result.push_str("%27"),
            '(' => result.push_str("%28"),
            ')' => result.push_str("%29"),
            '*' => result.push_str("%2A"),
            '+' => result.push_str("%2B"),
            ',' => result.push_str("%2C"),
            '/' => result.push_str("%2F"),
            ':' => result.push_str("%3A"),
            ';' => result.push_str("%3B"),
            '=' => result.push_str("%3D"),
            '?' => result.push_str("%3F"),
            '@' => result.push_str("%40"),
            '[' => result.push_str("%5B"),
            ']' => result.push_str("%5D"),
            // Non-ASCII: encode each UTF-8 byte as %XX
            other => {
                let mut buf = [0u8; 4];
                let encoded = other.encode_utf8(&mut buf);
                for b in encoded.bytes() {
                    result.push_str(&format!("%{:02X}", b));
                }
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_urlencoding() {
        assert_eq!(urlencoding("Lightning Bolt"), "Lightning%20Bolt");
        assert_eq!(urlencoding("Find // Finality"), "Find%20%2F%2F%20Finality");
        assert_eq!(
            urlencoding("Jace, the Mind Sculptor"),
            "Jace%2C%20the%20Mind%20Sculptor"
        );
        assert_eq!(urlencoding("Lim-Dûl's Vault"), "Lim-D%C3%BBl%27s%20Vault");
    }
}
