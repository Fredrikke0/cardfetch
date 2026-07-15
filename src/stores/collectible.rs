use super::{title_contains, urlencode_plus, Store, StoreResult};
use anyhow::Context;
use std::time::Duration;

const SEARCH_URL: &str = "https://collectible.no/";
const STORE_NAME: &str = "collectible.no";
const TIMEOUT_SECS: u64 = 30;

pub struct Collectible;

impl Collectible {
    pub fn new() -> Self {
        Collectible
    }
}

impl Store for Collectible {
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
        let search_url = format!(
            "{}?s={}&post_type=product&stock_status=instock",
            SEARCH_URL,
            urlencode_plus(card_name)
        );

        // Build a non-redirect client for the search request. A 302 means
        // a single-result redirect to the product page, whose "recommended
        // products" section would contaminate the scraper.
        let search_client = reqwest::blocking::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(TIMEOUT_SECS))
            .build()
            .context("Failed to build non-redirect HTTP client")?;

        let response = search_client
            .get(&search_url)
            .send()
            .context("Failed to send collectible.no search request")?;

        let status = response.status().as_u16();

        if status == 301 || status == 302 || status == 303 || status == 307 || status == 308 {
            let product_url = response
                .headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string())
                .context("collectible.no redirect missing Location header")?;

            let (price, product_title) = fetch_product_details(client, &product_url)?;

            // Verify the redirect target actually matches the searched card.
            // collectible.no sometimes redirects to a fuzzy "best match" that
            // is a completely different card.
            if !super::title_contains(card_name, &product_title) {
                return Ok(vec![]);
            }

            return Ok(vec![StoreResult {
                store_name: STORE_NAME.to_string(),
                card_name: card_name.to_string(),
                price,
                url: product_url,
            }]);
        }

        if !response.status().is_success() {
            anyhow::bail!("collectible.no search returned HTTP {}", status);
        }

        let html_text = response
            .text()
            .context("Failed to read collectible.no response body")?;

        let products = parse_search_results(&html_text)?;

        let matching = products.iter().find(|p| title_contains(card_name, &p.name));

        if let Some(product) = matching {
            return Ok(vec![StoreResult {
                store_name: STORE_NAME.to_string(),
                card_name: card_name.to_string(),
                price: product.price,
                url: product.url.clone(),
            }]);
        }

        Ok(vec![])
    }
}

// ── Search results page scraping ───────────────────────────────────────────

fn parse_search_results(html_text: &str) -> anyhow::Result<Vec<super::SearchProduct>> {
    let document = scraper::Html::parse_document(html_text);

    let product_selector = scraper::Selector::parse("div.product-small")
        .map_err(|e| anyhow::anyhow!("CSS 'div.product-small': {}", e))?;

    let title_link_selector = scraper::Selector::parse("p.name.product-title a")
        .map_err(|e| anyhow::anyhow!("CSS title link: {}", e))?;

    let price_selector =
        scraper::Selector::parse("span.price span.woocommerce-Price-amount.amount bdi")
            .map_err(|e| anyhow::anyhow!("CSS price: {}", e))?;

    let mut products: Vec<super::SearchProduct> = Vec::new();

    for product_el in document.select(&product_selector) {
        let title_link = product_el.select(&title_link_selector).next();

        let title = title_link.map(|el| el.text().collect::<String>().trim().to_string());

        let url = title_link
            .and_then(|el| el.value().attr("href"))
            .map(|s| s.to_string());

        let price = product_el
            .select(&price_selector)
            .next()
            .and_then(|el| parse_price(&el.text().collect::<String>()));

        if let (Some(name), Some(price), Some(url)) = (title, price, url) {
            products.push(super::SearchProduct {
                name,
                price,
                url,
                in_stock: true,
            });
        }
    }

    Ok(products)
}

// ── Single product page scraping ───────────────────────────────────────────

/// Fetch both price and product title from a single product page.
fn fetch_product_details(
    client: &reqwest::blocking::Client,
    product_url: &str,
) -> anyhow::Result<(u32, String)> {
    let response = client
        .get(product_url)
        .send()
        .context("Failed to fetch collectible.no product page")?;

    if !response.status().is_success() {
        anyhow::bail!(
            "collectible.no product page returned HTTP {}",
            response.status().as_u16()
        );
    }

    let html_text = response
        .text()
        .context("Failed to read collectible.no product page body")?;

    let document = scraper::Html::parse_document(&html_text);

    // Extract product title from the <h1> heading.
    let title_sel = scraper::Selector::parse("h1.product-title")
        .map_err(|e| anyhow::anyhow!("CSS 'h1.product-title': {}", e))?;
    let title = document
        .select(&title_sel)
        .next()
        .map(|el| el.text().collect::<String>().trim().to_string())
        .unwrap_or_default();

    for selector_str in &[
        "p.price span.woocommerce-Price-amount.amount bdi",
        "span.price span.woocommerce-Price-amount.amount bdi",
    ] {
        let sel = scraper::Selector::parse(selector_str)
            .map_err(|e| anyhow::anyhow!("CSS '{}': {}", selector_str, e))?;

        if let Some(el) = document.select(&sel).next() {
            let text: String = el.text().collect();
            if let Some(price) = parse_price(&text) {
                return Ok((price, title));
            }
        }
    }

    anyhow::bail!("Could not find price on collectible.no product page");
}

// ── Helpers ────────────────────────────────────────────────────────────────

fn parse_price(raw: &str) -> Option<u32> {
    let raw = raw.trim();
    let num_str = raw.split_whitespace().next()?.trim_end_matches(',');
    let (whole, frac) = num_str.split_once(',')?;
    let whole: u32 = whole.parse().ok()?;
    let frac: u32 = frac.parse().ok()?;
    if frac >= 100 {
        return None;
    }
    Some(whole * 100 + frac)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_store_name() {
        let store = Collectible::new();
        assert_eq!(store.name(), "collectible.no");
    }

    #[test]
    fn test_urlencode() {
        assert_eq!(urlencode_plus("Opal Palace"), "Opal+Palace");
        assert_eq!(urlencode_plus("snakeskin veil"), "snakeskin+veil");
    }

    #[test]
    fn test_parse_price() {
        assert_eq!(parse_price("3,90 kr"), Some(390));
        assert_eq!(parse_price("  3,90  kr  "), Some(390));
        assert_eq!(parse_price("15,00"), Some(1500));
        assert_eq!(parse_price("0,50 kr"), Some(50));
    }

    #[test]
    fn test_title_contains() {
        assert!(title_contains(
            "Opal Palace",
            "Commander Legends - 352 - Opal Palace - Common - C - Non-foil"
        ));
        assert!(title_contains(
            "opal palace",
            "Commander Legends - 352 - Opal Palace - Common - C - Non-foil"
        ));
        assert!(!title_contains(
            "Black Lotus",
            "Commander Legends - 352 - Opal Palace - Common - C - Non-foil"
        ));
    }
}
