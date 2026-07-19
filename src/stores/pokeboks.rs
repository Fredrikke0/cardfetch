use super::{title_contains, SearchProduct, Store, StoreResult};
use anyhow::Context;
use serde::Deserialize;

const API_URL: &str = "https://pokeboks.no/wp-json/elementor-pro/v1/refresh-search";
const STORE_NAME: &str = "pokeboks.no";
const TIMEOUT_SECS: u64 = 15;

/// Fixed IDs for the search widget on pokeboks.no.
const POST_ID: &str = "2493";
const WIDGET_ID: &str = "00b4fbe";

pub struct Pokeboks;

impl Pokeboks {
    pub fn new() -> Self {
        Pokeboks
    }
}

impl Store for Pokeboks {
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
        let all_products = fetch_all_pages(client, card_name)?;

        // Return all in-stock products matching the card name, so the wizard
        // can pick the cheapest variant.
        let matching: Vec<_> = all_products
            .into_iter()
            .filter(|p| p.in_stock && title_contains(card_name, &p.name))
            .map(|p| StoreResult {
                store_name: STORE_NAME.to_string(),
                card_name: card_name.to_string(),
                price: p.price,
                url: p.url,
            })
            .collect();

        Ok(matching)
    }
}

// --- API types --------------------------------------------------------------

#[derive(serde::Serialize)]
struct SearchRequest {
    post_id: String,
    widget_id: String,
    search_term: String,
    page_number: u32,
}

#[derive(Debug, Deserialize)]
struct SearchResponse {
    data: String,
    #[serde(default)]
    pagination: String,
}

// --- API fetching with pagination -------------------------------------------

fn fetch_all_pages(
    client: &reqwest::blocking::Client,
    search_term: &str,
) -> anyhow::Result<Vec<SearchProduct>> {
    let mut all_products: Vec<SearchProduct> = Vec::new();
    let mut page = 1u32;

    loop {
        let request = SearchRequest {
            post_id: POST_ID.to_string(),
            widget_id: WIDGET_ID.to_string(),
            search_term: search_term.to_string(),
            page_number: page,
        };

        let response = client
            .post(API_URL)
            .json(&request)
            .send()
            .context("Failed to send pokeboks.no search request")?;

        if !response.status().is_success() {
            anyhow::bail!(
                "pokeboks.no search returned HTTP {}",
                response.status().as_u16()
            );
        }

        // The API prefixes responses with a UTF-8 BOM, which breaks .json().
        let raw_text = response
            .text()
            .context("Failed to read pokeboks.no response body")?;
        let cleaned = raw_text.strip_prefix('\u{feff}').unwrap_or(&raw_text);
        let body: SearchResponse = serde_json::from_str(cleaned)
            .context("Failed to parse pokeboks.no search response JSON")?;

        let products = parse_search_html(&body.data)?;
        all_products.extend(products);

        // If pagination is empty, we've fetched all pages
        if body.pagination.trim().is_empty() {
            break;
        }

        page += 1;
        std::thread::sleep(std::time::Duration::from_millis(super::DELAY_MS));
    }

    Ok(all_products)
}

// --- HTML parsing -----------------------------------------------------------

fn parse_search_html(html: &str) -> anyhow::Result<Vec<SearchProduct>> {
    let document = scraper::Html::parse_fragment(html);

    let loop_item_sel = scraper::Selector::parse("[data-elementor-type=\"loop-item\"]")
        .map_err(|e| anyhow::anyhow!("CSS '[data-elementor-type=\"loop-item\"]': {}", e))?;

    let name_sel = scraper::Selector::parse("h4.elementor-heading-title")
        .map_err(|e| anyhow::anyhow!("CSS 'h4.elementor-heading-title': {}", e))?;

    let price_sel = scraper::Selector::parse("span.woocommerce-Price-amount")
        .map_err(|e| anyhow::anyhow!("CSS 'span.woocommerce-Price-amount': {}", e))?;

    let link_sel = scraper::Selector::parse("a").map_err(|e| anyhow::anyhow!("CSS 'a': {}", e))?;

    let mut products: Vec<SearchProduct> = Vec::new();

    for item in document.select(&loop_item_sel) {
        // Check stock status from the class list
        let class_list = item.value().attr("class").unwrap_or("");
        let in_stock = class_list.contains("instock");

        // Get product name
        let name = match item.select(&name_sel).next() {
            Some(el) => el.text().collect::<String>().trim().to_string(),
            None => continue,
        };
        if name.is_empty() {
            continue;
        }

        // Get product URL from the first anchor inside the loop item
        let url = match item.select(&link_sel).next() {
            Some(el) => el.value().attr("href").unwrap_or("").to_string(),
            None => continue,
        };
        if url.is_empty() {
            continue;
        }

        // Get price
        let price = match item.select(&price_sel).next() {
            Some(el) => {
                let text = el.text().collect::<String>();
                parse_price(&text)
            }
            None => continue,
        };
        let Some(price) = price else {
            continue;
        };

        products.push(SearchProduct {
            name,
            price,
            url,
            in_stock,
        });
    }

    Ok(products)
}

// --- Price parsing ----------------------------------------------------------

/// Parse a price string like "2,99 kr" or "2,99&nbsp;kr" into integer oere.
fn parse_price(raw: &str) -> Option<u32> {
    // Strip HTML entities and whitespace, take the number part
    let cleaned = raw
        .replace("&nbsp;", "")
        .replace("kr", "")
        .trim()
        .to_string();

    // Take characters until we hit a non-digit, non-comma char
    let num_str: String = cleaned
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == ',')
        .collect();

    let (whole_str, frac_str) = num_str.split_once(',')?;
    let whole: u32 = whole_str.parse().ok()?;
    let frac: u32 = frac_str.parse().ok()?;
    if frac >= 100 {
        return None;
    }
    Some(whole * 100 + frac)
}

// --- Tests ------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_price() {
        assert_eq!(parse_price("2,99 kr"), Some(299));
        assert_eq!(parse_price("2,99&nbsp;kr"), Some(299));
        assert_eq!(parse_price("  15,00 kr  "), Some(1500));
        assert_eq!(parse_price("0,50"), Some(50));
        assert_eq!(parse_price("abc"), None);
    }

    #[test]
    fn test_parse_search_html() {
        let html = r#"<div data-elementor-type="loop-item" data-elementor-id="2000" class="elementor elementor-2000 e-loop-item e-loop-item-61001 post-61001 product type-product status-publish has-post-thumbnail product_cat-kaldheim first instock taxable shipping-taxable purchasable product-type-simple">
            <a class="elementor-element elementor-element-fb7d3f8 e-con-full e-flex e-con e-parent" href="https://pokeboks.no/produkt/mtg-khm-194-normal-en/">
                <h4 class="elementor-heading-title elementor-size-default">Snakeskin Veil #194 — Kaldheim</h4>
                <span class="woocommerce-Price-amount amount"><bdi>2,99&nbsp;<span class="woocommerce-Price-currencySymbol">kr</span></bdi></span>
            </a>
        </div>"#;

        let products = parse_search_html(html).unwrap();
        assert_eq!(products.len(), 1);
        assert_eq!(products[0].name, "Snakeskin Veil #194 — Kaldheim");
        assert_eq!(products[0].price, 299);
        assert!(products[0].in_stock);
        assert_eq!(
            products[0].url,
            "https://pokeboks.no/produkt/mtg-khm-194-normal-en/"
        );
    }

    #[test]
    fn test_parse_search_html_out_of_stock() {
        let html = r#"<div data-elementor-type="loop-item" class="product outofstock">
            <a href="https://pokeboks.no/produkt/test/">
                <h4 class="elementor-heading-title">Test Card #1 — Set</h4>
                <span class="woocommerce-Price-amount"><bdi>10,00 kr</bdi></span>
            </a>
        </div>"#;

        let products = parse_search_html(html).unwrap();
        assert_eq!(products.len(), 1);
        assert!(!products[0].in_stock);
    }
}
