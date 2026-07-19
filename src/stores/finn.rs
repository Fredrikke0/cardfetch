use super::{title_contains, urlencode_pct, Store, StoreResult};
use anyhow::Context;
use serde::Deserialize;
use std::time::Duration;

const SEARCH_URL: &str =
    "https://www.finn.no/recommerce/forsale/search/api/search/SEARCH_ID_BAP_COMMON";
const STORE_NAME: &str = "finn.no";
const TIMEOUT_SECS: u64 = 30;
const ITEM_URL: &str = "https://www.finn.no/recommerce/forsale/item";

/// Category "Fritid, hobby og underholdning" > "Samleobjekter" > "Samlekort"
const CATEGORY: &str = "0.86";
const PRODUCT_CATEGORY: &str = "2.86.285.396";

pub struct Finn;

impl Finn {
    pub fn new() -> Self {
        Finn
    }
}

impl Store for Finn {
    fn name(&self) -> &str {
        STORE_NAME
    }

    fn timeout_secs(&self) -> u64 {
        TIMEOUT_SECS
    }

    fn search(
        &self,
        client: &reqwest::blocking::Client,
        card_name: &str,
    ) -> anyhow::Result<Vec<StoreResult>> {
        let docs = fetch_search_results(client, card_name)?;

        // Fetch each ad's full description from the HTML and collect all
        // matching results (different sellers may list at different prices).
        let mut results: Vec<StoreResult> = Vec::new();
        for doc in &docs {
            std::thread::sleep(Duration::from_millis(super::DELAY_MS));

            let detail = match fetch_ad_detail(client, &doc.id) {
                Ok(d) => d,
                Err(_) => {
                    // Non-fatal: skip this ad, try the next one.
                    continue;
                }
            };

            // Skip Pokémon-related listings
            if detail.description.to_lowercase().contains("pokemon") {
                continue;
            }

            if title_contains(card_name, &detail.description) {
                let price_oere = doc.price_amount().unwrap_or(0);
                let url = doc
                    .canonical_url
                    .clone()
                    .unwrap_or_else(|| format!("{}/{}", ITEM_URL, doc.id));

                results.push(StoreResult {
                    store_name: format!("{}: {}", STORE_NAME, detail.seller_name),
                    card_name: card_name.to_string(),
                    price: price_oere,
                    url,
                });
            }
        }

        Ok(results)
    }
}

// ── Search API types ─────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct SearchResponse {
    #[serde(default)]
    docs: Vec<SearchDoc>,
    #[serde(default)]
    metadata: Option<Metadata>,
}

#[derive(Debug, Deserialize)]
struct SearchDoc {
    #[serde(default)]
    canonical_url: Option<String>,
    #[serde(default, rename = "price")]
    price_raw: Option<PriceRaw>,
    id: String,
}

impl SearchDoc {
    fn price_amount(&self) -> Option<u32> {
        self.price_raw
            .as_ref()
            .and_then(|p| u32::try_from(p.amount * 100).ok())
    }
}

#[derive(Debug, Deserialize)]
struct PriceRaw {
    amount: i64,
}

#[derive(Debug, Deserialize)]
struct Metadata {
    paging: Paging,
}

#[derive(Debug, Deserialize)]
struct Paging {
    last: u32,
}

// ── Search fetching ──────────────────────────────────────────────────────

fn fetch_search_results(
    client: &reqwest::blocking::Client,
    search_term: &str,
) -> anyhow::Result<Vec<SearchDoc>> {
    let mut all_docs: Vec<SearchDoc> = Vec::new();
    let mut page = 1u32;

    loop {
        let url = format!(
            "{}?q={}&category={}&product_category={}&page={}",
            SEARCH_URL,
            urlencode_pct(search_term),
            CATEGORY,
            PRODUCT_CATEGORY,
            page
        );

        let response = client
            .get(&url)
            .send()
            .context("Failed to send Finn.no search request")?;

        if !response.status().is_success() {
            anyhow::bail!(
                "Finn.no search returned HTTP {}",
                response.status().as_u16()
            );
        }

        let response_text = response
            .text()
            .context("Failed to read Finn.no response body")?;

        let body: SearchResponse = serde_json::from_str(&response_text).map_err(|e| {
            let snippet: String = response_text.chars().take(500).collect();
            anyhow::anyhow!(
                "Failed to parse Finn.no response: {}. Body preview: {}",
                e,
                snippet
            )
        })?;

        let last_page = body.metadata.as_ref().map(|m| m.paging.last).unwrap_or(1);

        all_docs.extend(body.docs);

        if page >= last_page {
            break;
        }

        page += 1;
        std::thread::sleep(Duration::from_millis(super::DELAY_MS));
    }

    Ok(all_docs)
}

// ── Ad detail HTML scraping ─────────────────────────────────────────────

struct AdDetail {
    description: String,
    seller_name: String,
}

/// Fetch an ad page and extract the full description and seller name
/// from the rendered HTML.
fn fetch_ad_detail(client: &reqwest::blocking::Client, ad_id: &str) -> anyhow::Result<AdDetail> {
    let url = format!("{}/{}", ITEM_URL, ad_id);

    let response = client
        .get(&url)
        .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36")
        .header("Accept", "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8")
        .header("Accept-Language", "nb-NO,nb;q=0.9,no;q=0.8,en;q=0.7")
        .send()
        .context("Failed to fetch Finn.no ad page")?;

    if !response.status().is_success() {
        anyhow::bail!(
            "Finn.no ad page returned HTTP {}",
            response.status().as_u16()
        );
    }

    let html_text = response
        .text()
        .context("Failed to read Finn.no ad page body")?;

    let document = scraper::Html::parse_document(&html_text);

    // Extract description
    let desc_selector =
        scraper::Selector::parse("section[data-testid=\"description\"] div.whitespace-pre-wrap")
            .map_err(|e| anyhow::anyhow!("Invalid CSS selector: {}", e))?;

    let texts: Vec<String> = document
        .select(&desc_selector)
        .flat_map(|el| el.text())
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect();

    if texts.is_empty() {
        anyhow::bail!("No description element found on ad page");
    }
    let description = texts.join(" ");

    // Extract seller name from profile link
    let seller_name = extract_seller_name(&document);

    Ok(AdDetail {
        description,
        seller_name,
    })
}

/// Try to extract the seller's display name from the ad page.
/// Tries multiple strategies in order of reliability.
/// Falls back to "unknown" if no name can be found.
fn extract_seller_name(document: &scraper::Html) -> String {
    // Strategy 1: JSON-LD structured data (Product > seller > name)
    if let Some(name) = extract_from_ld_json(document) {
        if !name.is_empty() {
            return name;
        }
    }

    // Strategy 2: Profile link in the seller section.
    // Finn recommence uses href="/profile/ads?userId=..." with the seller name as text.
    // Scope to links that are inside a profile/seller context by preferring
    // the more specific "/profile/ads" pattern first.
    let selectors = [
        "a[href*='/profile/ads']",
        "a[href*='/profile/']",
        "a[href*='/profil/']",
        "a[href*='/user/']",
    ];
    for sel_str in &selectors {
        if let Ok(sel) = scraper::Selector::parse(sel_str) {
            for el in document.select(&sel) {
                let name = el.text().collect::<String>().trim().to_string();
                if !name.is_empty() && name.len() < 80 {
                    return name;
                }
            }
        }
    }

    "unknown".to_string()
}

/// Try to extract seller name from JSON-LD structured data.
fn extract_from_ld_json(document: &scraper::Html) -> Option<String> {
    let sel = scraper::Selector::parse("script[type='application/ld+json']").ok()?;

    for el in document.select(&sel) {
        let json_str: String = el.text().collect();
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&json_str) {
            // Try Product.seller.name
            if let Some(name) = value
                .get("seller")
                .and_then(|s| s.get("name"))
                .and_then(|n| n.as_str())
            {
                return Some(name.to_string());
            }
            // Try @graph array (sometimes used for multiple entities)
            if let Some(graph) = value.get("@graph").and_then(|g| g.as_array()) {
                for item in graph {
                    if let Some(name) = item
                        .get("seller")
                        .and_then(|s| s.get("name"))
                        .and_then(|n| n.as_str())
                    {
                        return Some(name.to_string());
                    }
                }
            }
        }
    }
    None
}
