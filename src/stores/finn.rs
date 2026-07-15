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

        // Fetch each ad's full description from the HTML and check for card name
        for doc in &docs {
            std::thread::sleep(Duration::from_millis(super::DELAY_MS));

            let description = match fetch_ad_description(client, &doc.id) {
                Ok(desc) => desc,
                Err(_) => {
                    // Non-fatal: skip this ad, try the next one.
                    continue;
                }
            };

            if title_contains(card_name, &description) {
                let price_oere = doc.price_amount().unwrap_or(0);
                let url = doc
                    .canonical_url
                    .clone()
                    .unwrap_or_else(|| format!("{}/{}", ITEM_URL, doc.id));

                return Ok(vec![StoreResult {
                    store_name: STORE_NAME.to_string(),
                    card_name: card_name.to_string(),
                    price: price_oere,
                    url,
                }]);
            }
        }

        Ok(vec![])
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
            .and_then(|p| u32::try_from(p.amount).ok())
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

/// Fetch an ad page and extract the full description from the rendered HTML.
fn fetch_ad_description(client: &reqwest::blocking::Client, ad_id: &str) -> anyhow::Result<String> {
    let url = format!("{}/{}", ITEM_URL, ad_id);

    let response = client
        .get(&url)
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

    // The description lives in: section[data-testid="description"] div.whitespace-pre-wrap
    let selector =
        scraper::Selector::parse("section[data-testid=\"description\"] div.whitespace-pre-wrap")
            .map_err(|e| anyhow::anyhow!("Invalid CSS selector: {}", e))?;

    let texts: Vec<String> = document
        .select(&selector)
        .flat_map(|el| el.text())
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect();

    if texts.is_empty() {
        anyhow::bail!("No description element found on ad page");
    }

    Ok(texts.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_store_name() {
        let store = Finn::new();
        assert_eq!(store.name(), "finn.no");
    }

    #[test]
    fn test_urlencode() {
        assert_eq!(
            urlencode_pct("undergrowth champion"),
            "undergrowth%20champion"
        );
        assert_eq!(urlencode_pct("snakeskin veil"), "snakeskin%20veil");
    }

    #[test]
    fn test_description_match() {
        assert!(title_contains(
            "Hydra's Growth",
            "Jeg selger Hydra's Growth og andre grønne kort"
        ));
        assert!(title_contains("hydra's growth", "Hydra's Growth (NM)"));
        assert!(!title_contains(
            "Hydra's Growth",
            "Jeg selger noen Magic kort"
        ));
    }
}
