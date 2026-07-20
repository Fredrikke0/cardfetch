use super::{title_contains, urlencode_plus, SearchProduct, Store, StoreResult};
use anyhow::Context;

const SEARCH_URL: &str = "https://midgardgames.no/search";
const STORE_NAME: &str = "midgardgames.no";
const STORE_BASE_URL: &str = "https://midgardgames.no";
const TIMEOUT_SECS: u64 = 15;

pub struct Midgard;

impl Midgard {
    pub fn new() -> Self {
        Midgard
    }
}

impl Store for Midgard {
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
        let all_products = fetch_search_results(client, card_name)?;

        // Filter by card name in the product title, then verify each match
        // by fetching the product page and checking for "Wizards of the Coast"
        // to confirm it's actually a Magic card (not a sleeve, playmat, etc.).
        let mut matching: Vec<StoreResult> = Vec::new();
        for p in all_products {
            if !p.in_stock || !title_contains(card_name, &p.name) {
                continue;
            }
            match is_magic_card(client, &p.url) {
                Ok(true) => {
                    matching.push(StoreResult {
                        store_name: STORE_NAME.to_string(),
                        card_name: card_name.to_string(),
                        price: p.price,
                        url: p.url,
                    });
                }
                Ok(false) => {
                    // Product page exists but isn't a Magic card — skip.
                }
                Err(e) => {
                    // If we can't check the product page, skip it rather
                    // than returning a potentially false result.
                    eprintln!(
                        "Warning: failed to verify midgardgames.no product {}: {e}",
                        p.url
                    );
                }
            }
        }

        Ok(matching)
    }
}

// --- Product page verification -----------------------------------------------

/// Fetch the product page and check if it's a Magic: The Gathering card
/// by looking for "Wizards of the Coast" in the HTML.
fn is_magic_card(client: &reqwest::blocking::Client, product_url: &str) -> anyhow::Result<bool> {
    let response = client
        .get(product_url)
        .send()
        .context("Failed to fetch midgardgames.no product page")?;

    if !response.status().is_success() {
        anyhow::bail!(
            "midgardgames.no product page returned HTTP {}",
            response.status().as_u16()
        );
    }

    let html = response
        .text()
        .context("Failed to read midgardgames.no product page body")?;

    Ok(html.contains("Wizards of the Coast"))
}

// --- HTML scraping ---------------------------------------------------------

fn fetch_search_results(
    client: &reqwest::blocking::Client,
    search_term: &str,
) -> anyhow::Result<Vec<SearchProduct>> {
    let url = format!("{}?q={}", SEARCH_URL, urlencode_plus(search_term));

    let response = client
        .get(&url)
        .send()
        .context("Failed to send midgardgames.no search request")?;

    if !response.status().is_success() {
        anyhow::bail!(
            "midgardgames.no search returned HTTP {}",
            response.status().as_u16()
        );
    }

    let html = response
        .text()
        .context("Failed to read midgardgames.no response body")?;

    parse_search_results(&html)
}

fn parse_search_results(html: &str) -> anyhow::Result<Vec<SearchProduct>> {
    let document = scraper::Html::parse_document(html);

    let card_sel = scraper::Selector::parse("div.product.grid__item")
        .map_err(|e| anyhow::anyhow!("CSS 'div.product.grid__item': {}", e))?;

    let title_link_sel = scraper::Selector::parse("div.product__title a")
        .map_err(|e| anyhow::anyhow!("CSS 'div.product__title a': {}", e))?;

    let price_sel = scraper::Selector::parse("span.product__price")
        .map_err(|e| anyhow::anyhow!("CSS 'span.product__price': {}", e))?;

    let mut products: Vec<SearchProduct> = Vec::new();

    for card in document.select(&card_sel) {
        // Check for sold-out class on the card
        let class_list = card.value().attr("class").unwrap_or("");
        if class_list.contains("sold-out") {
            continue;
        }

        // Get product name and URL
        let title_link = match card.select(&title_link_sel).next() {
            Some(el) => el,
            None => continue,
        };
        let name = title_link.text().collect::<String>().trim().to_string();
        let relative_url = title_link.value().attr("href").unwrap_or("").to_string();

        if name.is_empty() || relative_url.is_empty() {
            continue;
        }

        let url = if relative_url.starts_with("http") {
            relative_url
        } else {
            format!("{}{}", STORE_BASE_URL, relative_url)
        };

        // Get price
        let price_el = match card.select(&price_sel).next() {
            Some(el) => el,
            None => continue,
        };
        let price_text: String = price_el.text().collect();
        let price_text = price_text.trim();

        // If price text contains "Utsolgt", skip (out of stock)
        if price_text.to_lowercase().contains("utsolgt") {
            continue;
        }

        let Some(price) = parse_price(price_text) else {
            continue;
        };

        products.push(SearchProduct {
            name,
            price,
            url,
            in_stock: true,
        });
    }

    Ok(products)
}

// --- Price parsing ----------------------------------------------------------

/// Parse a price string like "5,00 kr" into integer oere.
/// Handles text like "Vanlig pris 5,00 kr" by finding the number within.
fn parse_price(raw: &str) -> Option<u32> {
    let raw = raw.trim();

    // Skip past any non-digit prefix (e.g. "Vanlig pris ")
    let start = raw.find(|c: char| c.is_ascii_digit())?;
    let num_part = &raw[start..];

    let cleaned = num_part
        .replace("kr", "")
        .replace("NOK", "")
        .replace(" ", "")
        .trim()
        .to_string();

    // "1.899,00" -> whole=1899, frac=00 -> 189900
    // "5,00" -> whole=5, frac=00 -> 500
    let (whole_str, frac_str) = cleaned.split_once(',')?;

    // Remove thousand separators (dots)
    let whole_str = whole_str.replace('.', "");
    let whole: u32 = whole_str.parse().ok()?;
    let frac: u32 = frac_str.parse().ok()?;
    if frac >= 100 {
        return None;
    }
    Some(whole * 100 + frac)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_magic_card() {
        // Requires network; skips gracefully on failure (e.g. CI / offline).
        let client = reqwest::blocking::Client::builder()
            .user_agent("cardfetch-test/0.1")
            .timeout(std::time::Duration::from_secs(TIMEOUT_SECS))
            .build()
            .unwrap();

        match is_magic_card(
            &client,
            "https://midgardgames.no/products/clb-870-u-skullclamp",
        ) {
            Ok(is_magic) => assert!(is_magic),
            Err(_) => eprintln!("Skipping test_is_magic_card: network failed"),
        }
    }

    #[test]
    fn test_parse_price() {
        assert_eq!(parse_price("5,00 kr"), Some(500));
        assert_eq!(parse_price("Vanlig pris 5,00 kr"), Some(500));
        assert_eq!(parse_price("1.899,00 kr"), Some(189900));
        assert_eq!(parse_price("  15,50 kr  "), Some(1550));
        assert_eq!(parse_price("0,50"), Some(50));
        assert_eq!(parse_price("abc"), None);
        assert_eq!(parse_price("Utsolgt"), None);
    }

    #[test]
    fn test_parse_search_results() {
        let html = r#"<div class="product grid__item medium-up--one-third small--one-half">
            <div class="product__title product__title--card text-center">
                <a href="/products/tdm-0159-c-snakeskin-veil">TDM 0159 C: Snakeskin Veil</a>
            </div>
            <div class="product__prices text-center">
                <span class="product__price">
                    <span class="visually-hidden">Vanlig pris</span>
                    5,00 kr
                </span>
            </div>
        </div>"#;

        let products = parse_search_results(html).unwrap();
        assert_eq!(products.len(), 1);
        assert_eq!(products[0].name, "TDM 0159 C: Snakeskin Veil");
        assert_eq!(products[0].price, 500);
        assert!(products[0].in_stock);
        assert_eq!(
            products[0].url,
            "https://midgardgames.no/products/tdm-0159-c-snakeskin-veil"
        );
    }

    #[test]
    fn test_parse_search_results_sold_out_class() {
        let html = r#"<div class="product grid__item sold-out">
            <div class="product__title"><a href="/products/test">Test Card</a></div>
            <span class="product__price">Utsolgt</span>
        </div>"#;

        let products = parse_search_results(html).unwrap();
        // sold-out class skips the card entirely
        assert_eq!(products.len(), 0);
    }

    #[test]
    fn test_parse_search_results_price_utsolgt() {
        let html = r#"<div class="product grid__item">
            <div class="product__title"><a href="/products/test">Test Card</a></div>
            <span class="product__price">Utsolgt</span>
        </div>"#;

        let products = parse_search_results(html).unwrap();
        // Price saying "Utsolgt" skips the card
        assert_eq!(products.len(), 0);
    }
}
