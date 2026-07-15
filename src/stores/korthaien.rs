use super::{urlencode_pct, SearchProduct, Store, StoreResult};
use anyhow::Context;

const SEARCH_URL: &str = "https://www.korthaien.no/search_result";
const STORE_NAME: &str = "korthaien.no";
const TIMEOUT_SECS: u64 = 15;

pub struct Korthaien;

impl Korthaien {
    pub fn new() -> Self {
        Korthaien
    }
}

impl Store for Korthaien {
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

        // Filter to in-stock products whose name matches (case-insensitive,
        // ignoring foil suffix).
        let matching: Vec<&SearchProduct> = all_products
            .iter()
            .filter(|p| p.in_stock && names_match(card_name, &p.name))
            .collect();

        if matching.is_empty() {
            return Ok(vec![]);
        }

        // Prefer non-foil (cheapest), then foil (cheapest)
        let best = pick_best(&matching, card_name).unwrap();

        Ok(vec![StoreResult {
            store_name: STORE_NAME.to_string(),
            card_name: card_name.to_string(),
            price: best.price,
            url: best.url.clone(),
        }])
    }
}

// ── HTML scraping ─────────────────────────────────────────────────────────

fn fetch_search_results(
    client: &reqwest::blocking::Client,
    search_term: &str,
) -> anyhow::Result<Vec<SearchProduct>> {
    let url = format!("{}?keywords={}", SEARCH_URL, urlencode_pct(search_term));

    let response = client
        .get(&url)
        .send()
        .context("Failed to send korthaien.no search request")?;

    if !response.status().is_success() {
        anyhow::bail!(
            "korthaien.no search returned HTTP {}",
            response.status().as_u16()
        );
    }

    let html = response
        .text()
        .context("Failed to read korthaien.no response body")?;

    parse_search_results(&html)
}

fn parse_search_results(html: &str) -> anyhow::Result<Vec<SearchProduct>> {
    let document = scraper::Html::parse_document(html);

    let article_sel = scraper::Selector::parse("article.product-thumb-info")
        .map_err(|e| anyhow::anyhow!("CSS 'article.product-thumb-info': {}", e))?;

    let name_sel = scraper::Selector::parse("a.pb_title")
        .map_err(|e| anyhow::anyhow!("CSS 'a.pb_title': {}", e))?;

    let price_sel = scraper::Selector::parse("p.text-xl.font-bold")
        .map_err(|e| anyhow::anyhow!("CSS 'p.text-xl.font-bold': {}", e))?;

    let mut products: Vec<SearchProduct> = Vec::new();

    for article in document.select(&article_sel) {
        // Check stock from data-stock-quantity attribute
        let stock_qty: u32 = article
            .value()
            .attr("data-stock-quantity")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);

        // Get product name and URL
        let name_link = match article.select(&name_sel).next() {
            Some(el) => el,
            None => continue,
        };
        let name = name_link.text().collect::<String>().trim().to_string();
        let url = name_link.value().attr("href").unwrap_or("").to_string();

        if name.is_empty() || url.is_empty() {
            continue;
        }

        // Get price
        let price = article
            .select(&price_sel)
            .next()
            .and_then(|el| parse_price(&el.text().collect::<String>()));

        let Some(price) = price else {
            continue;
        };

        products.push(SearchProduct {
            name,
            price,
            url,
            in_stock: stock_qty > 0,
        });
    }

    Ok(products)
}

// ── Name matching ─────────────────────────────────────────────────────────

fn strip_foil(name: &str) -> &str {
    let suffixes = [" (foil)", " (Foil)", " (FOIL)"];
    for suffix in &suffixes {
        if let Some(stripped) = name.strip_suffix(suffix) {
            return stripped;
        }
    }
    name
}

fn names_match(searched: &str, product_name: &str) -> bool {
    let stripped = strip_foil(product_name);
    searched.eq_ignore_ascii_case(stripped)
}

/// Pick the best match: non-foil first (cheapest), then foil (cheapest).
fn pick_best<'a>(products: &[&'a SearchProduct], searched: &str) -> Option<&'a SearchProduct> {
    products
        .iter()
        .min_by_key(|p| {
            let is_foil = !searched.eq_ignore_ascii_case(p.name.trim());
            (is_foil, p.price)
        })
        .copied()
}

// ── Price parsing ─────────────────────────────────────────────────────────

/// Parse a price string like "6,-" into integer oere.
fn parse_price(price_str: &str) -> Option<u32> {
    let trimmed = price_str.trim();
    // Take leading digits, stop at comma/dash
    let digits: String = trimmed.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    let whole: u32 = digits.parse().ok()?;
    Some(whole * 100)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_foil() {
        assert_eq!(strip_foil("Snakeskin Veil (foil)"), "Snakeskin Veil");
        assert_eq!(strip_foil("Snakeskin Veil (Foil)"), "Snakeskin Veil");
        assert_eq!(strip_foil("Snakeskin Veil (FOIL)"), "Snakeskin Veil");
        assert_eq!(strip_foil("Snakeskin Veil"), "Snakeskin Veil");
    }

    #[test]
    fn test_names_match() {
        assert!(names_match("Snakeskin Veil", "Snakeskin Veil"));
        assert!(names_match("Snakeskin Veil", "Snakeskin Veil (foil)"));
        assert!(names_match("Snakeskin Veil", "Snakeskin Veil (Foil)"));
        assert!(names_match("snakeskin veil", "Snakeskin Veil"));
        assert!(!names_match("Snakeskin", "Snakeskin Veil"));
        assert!(!names_match("Other Card", "Snakeskin Veil"));
    }

    #[test]
    fn test_parse_price() {
        assert_eq!(parse_price("6,-"), Some(600));
        assert_eq!(parse_price("  12,-  "), Some(1200));
        assert_eq!(parse_price("4,-"), Some(400));
        assert_eq!(parse_price("105,-"), Some(10500));
        assert_eq!(parse_price(""), None);
        assert_eq!(parse_price("abc"), None);
    }

    #[test]
    fn test_store_name() {
        let store = Korthaien::new();
        assert_eq!(store.name(), "korthaien.no");
    }

    #[test]
    fn test_pick_best_prefers_non_foil() {
        let non_foil = SearchProduct {
            name: "Snakeskin Veil".into(),
            price: 1000,
            url: "/nf".into(),
            in_stock: true,
        };
        let foil = SearchProduct {
            name: "Snakeskin Veil (foil)".into(),
            price: 500,
            url: "/f".into(),
            in_stock: true,
        };
        let entries = [&non_foil, &foil];
        let best = pick_best(&entries, "Snakeskin Veil").unwrap();
        assert_eq!(best.url, "/nf");
    }

    #[test]
    fn test_pick_best_cheapest_non_foil_first() {
        let exp = SearchProduct {
            name: "Snakeskin Veil".into(),
            price: 1000,
            url: "/exp".into(),
            in_stock: true,
        };
        let cheap = SearchProduct {
            name: "Snakeskin Veil".into(),
            price: 400,
            url: "/cheap".into(),
            in_stock: true,
        };
        let entries = [&exp, &cheap];
        let best = pick_best(&entries, "Snakeskin Veil").unwrap();
        assert_eq!(best.url, "/cheap");
    }

    #[test]
    fn test_parse_search_results_real_html() {
        // Simulate a product card from the actual site
        let html = r#"<article data-stock-quantity="6" class="product-thumb-info relative z-0 bg-white">
            <a class="pb_v7_title pb_title block font-light text-base" href="https://www.korthaien.no/products/snakeskin-veilst">
                Snakeskin Veil
            </a>
            <p style="color:#000" class="mr-2 text-xl font-bold">5,-</p>
        </article>"#;

        let products = parse_search_results(html).unwrap();
        assert_eq!(products.len(), 1);
        assert_eq!(products[0].name, "Snakeskin Veil");
        assert_eq!(products[0].price, 500);
        assert!(products[0].in_stock);
        assert_eq!(
            products[0].url,
            "https://www.korthaien.no/products/snakeskin-veilst"
        );
    }

    #[test]
    fn test_parse_search_results_out_of_stock() {
        let html = r#"<article data-stock-quantity="0" class="product-thumb-info relative z-0 bg-white">
            <a class="pb_title" href="https://www.korthaien.no/products/golgari-signet">
                Golgari Signet
            </a>
            <p class="text-xl font-bold">15,-</p>
        </article>"#;

        let products = parse_search_results(html).unwrap();
        assert_eq!(products.len(), 1);
        assert!(!products[0].in_stock);
    }
}
