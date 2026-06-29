use super::{title_contains, urlencode_pct, SearchProduct, Store, StoreResult};
use anyhow::Context;

const SEARCH_URL: &str = "https://adamstuenretro.no/index.php";
const STORE_NAME: &str = "adamstuenretro.no";
const TIMEOUT_SECS: u64 = 15;

pub struct Adamstuen;

impl Adamstuen {
    pub fn new() -> Self {
        Adamstuen
    }
}

impl Store for Adamstuen {
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
    ) -> anyhow::Result<Option<StoreResult>> {
        let all_products = fetch_search_results(client, card_name)?;

        // Pick the cheapest in-stock product whose title contains the card name
        let best = match all_products
            .iter()
            .filter(|p| p.in_stock && title_contains(card_name, &p.name))
            .min_by_key(|p| p.price)
        {
            Some(p) => p,
            None => return Ok(None),
        };

        Ok(Some(StoreResult {
            store_name: STORE_NAME.to_string(),
            card_name: card_name.to_string(),
            price: best.price,
            url: best.url.clone(),
        }))
    }
}

// --- HTML scraping ---------------------------------------------------------

fn fetch_search_results(
    client: &reqwest::blocking::Client,
    search_term: &str,
) -> anyhow::Result<Vec<SearchProduct>> {
    let url = format!(
        "{}?route=product/search&search={}",
        SEARCH_URL,
        urlencode_pct(search_term)
    );

    let response = client
        .get(&url)
        .send()
        .context("Failed to send adamstuenretro.no search request")?;

    if !response.status().is_success() {
        anyhow::bail!(
            "adamstuenretro.no search returned HTTP {}",
            response.status().as_u16()
        );
    }

    let html = response
        .text()
        .context("Failed to read adamstuenretro.no response body")?;

    parse_search_results(&html)
}

fn parse_search_results(html: &str) -> anyhow::Result<Vec<SearchProduct>> {
    let document = scraper::Html::parse_document(html);

    let card_sel = scraper::Selector::parse("div.product-layout")
        .map_err(|e| anyhow::anyhow!("CSS 'div.product-layout': {}", e))?;

    let name_link_sel = scraper::Selector::parse("div.name a")
        .map_err(|e| anyhow::anyhow!("CSS 'div.name a': {}", e))?;

    let price_sel = scraper::Selector::parse("span.price-normal")
        .map_err(|e| anyhow::anyhow!("CSS 'span.price-normal': {}", e))?;

    let cart_sel = scraper::Selector::parse("a.btn-cart")
        .map_err(|e| anyhow::anyhow!("CSS 'a.btn-cart': {}", e))?;

    let mut products: Vec<SearchProduct> = Vec::new();

    for card in document.select(&card_sel) {
        // Check for out-of-stock class
        let class_list = card.value().attr("class").unwrap_or("");
        if class_list.contains("out-of-stock") {
            continue;
        }

        // Get product name and URL
        let name_link = match card.select(&name_link_sel).next() {
            Some(el) => el,
            None => continue,
        };
        let name = name_link.text().collect::<String>().trim().to_string();
        let url = name_link.value().attr("href").unwrap_or("").to_string();

        if name.is_empty() || url.is_empty() {
            continue;
        }

        // Get price
        let price = match card.select(&price_sel).next() {
            Some(el) => {
                let text = el.text().collect::<String>();
                parse_price(&text)
            }
            None => continue,
        };
        let Some(price) = price else {
            continue;
        };

        // Out-of-stock products lack the "Add to cart" button
        let in_stock = card.select(&cart_sel).next().is_some();

        products.push(SearchProduct {
            name,
            price,
            url,
            in_stock,
        });
    }

    Ok(products)
}

// --- Price parsing ---------------------------------------------------------

/// Parse a price string like "4 NOK" or "1,499 NOK" into integer oere.
fn parse_price(raw: &str) -> Option<u32> {
    let cleaned = raw.replace("NOK", "").replace(" ", "").trim().to_string();

    if cleaned.is_empty() {
        return None;
    }

    // "1,499" -> thousands separator, remove commas and parse as whole
    let num_str: String = cleaned.chars().filter(|c| c.is_ascii_digit()).collect();
    let whole: u32 = num_str.parse().ok()?;
    Some(whole * 100)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_store_name() {
        let store = Adamstuen::new();
        assert_eq!(store.name(), "adamstuenretro.no");
    }

    #[test]
    fn test_parse_price() {
        assert_eq!(parse_price("4 NOK"), Some(400));
        assert_eq!(parse_price("1,499 NOK"), Some(149900));
        assert_eq!(parse_price("  6 NOK  "), Some(600));
        assert_eq!(parse_price("3,499 NOK"), Some(349900));
        assert_eq!(parse_price(""), None);
        assert_eq!(parse_price("abc"), None);
    }

    #[test]
    fn test_title_contains() {
        assert!(title_contains(
            "Snakeskin Veil",
            "Snakeskin Veil (Kaldheim)"
        ));
        assert!(title_contains(
            "snakeskin veil",
            "Snakeskin Veil (Strixhaven Mystical Archive)"
        ));
        assert!(!title_contains("Black Lotus", "Snakeskin Veil (Kaldheim)"));
    }

    #[test]
    fn test_parse_search_results() {
        let html = r#"<div class="product-layout has-extra-button">
            <div class="product-thumb">
                <div class="caption">
                    <div class="name"><a href="https://adamstuenretro.no/kaldheim-snakeskin-veil">Snakeskin Veil (Kaldheim)</a></div>
                    <div class="price">
                        <div>
                            <span class="price-normal">4 NOK</span>
                        </div>
                    </div>
                    <div class="buttons-wrapper">
                        <div class="button-group">
                            <div class="cart-group">
                                <a class="btn btn-cart" onclick="cart.add('193784')"><span class="btn-text">Legg i handlevogn</span></a>
                            </div>
                        </div>
                    </div>
                </div>
            </div>
        </div>"#;

        let products = parse_search_results(html).unwrap();
        assert_eq!(products.len(), 1);
        assert_eq!(products[0].name, "Snakeskin Veil (Kaldheim)");
        assert_eq!(products[0].price, 400);
        assert!(products[0].in_stock);
        assert_eq!(
            products[0].url,
            "https://adamstuenretro.no/kaldheim-snakeskin-veil"
        );
    }

    #[test]
    fn test_parse_search_results_out_of_stock() {
        // Out-of-stock products lack the btn-cart
        let html = r#"<div class="product-layout">
            <div class="product-thumb">
                <div class="caption">
                    <div class="name"><a href="https://adamstuenretro.no/test">Test Card</a></div>
                    <div class="price">
                        <span class="price-normal">10 NOK</span>
                    </div>
                </div>
            </div>
        </div>"#;

        let products = parse_search_results(html).unwrap();
        assert_eq!(products.len(), 1);
        assert!(!products[0].in_stock);
    }
}
