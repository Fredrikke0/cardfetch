use super::{title_contains, title_to_slug, Store, StoreResult};
use crate::shipping::{self, EUR_TO_NOK, VAT_MULTIPLIER};
use anyhow::Context;
use base64::Engine;
use rand::Rng;
use scraper::Html;
use std::sync::{atomic::AtomicBool, atomic::Ordering, Mutex};
use std::time::Duration;

const CARD_URL: &str = "https://www.cardmarket.com/en/Magic/Cards";
const HOMEPAGE_URL: &str = "https://www.cardmarket.com/en/Magic";
const STORE_PREFIX: &str = "cardmarket.com";
const STORE_PREFIX_INT: &str = "cardmarket-int.com";
const STORE_PREFIX_INT_PRIVATE: &str = "cardmarket-int-private.com";
const AJAX_URL: &str = "https://www.cardmarket.com/en/Magic/AjaxAction/Metacard_LoadMoreArticles";

const SELLER_COUNTRY: &str = "24";
const SELLER_TYPE_INT: &str = "1,2";
const SELLER_TYPE_PRIVATE_INT: &str = "0";

const TIMEOUT_SECS: u64 = 30;
const MAX_LOAD_MORE_PAGES: u32 = 10;

/// Minimum delay between successive CardMarket requests (milliseconds).
const CM_DELAY_MIN_MS: u64 = 800;
/// Additional random jitter added on top of the minimum delay (milliseconds).
const CM_DELAY_JITTER_MS: u64 = 1200;

/// A realistic Chrome-on-Windows user agent.
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

pub struct CardMarket {
    client: reqwest::blocking::Client,
    verbose: bool,
    /// Tracks when the last request was sent so we can pace ourselves.
    last_request: Mutex<std::time::Instant>,
    /// Whether we've done the initial homepage warmup yet.
    warmed_up: AtomicBool,
}

impl CardMarket {
    pub fn new(verbose: bool) -> Self {
        let client = reqwest::blocking::Client::builder()
            .user_agent(USER_AGENT)
            .cookie_store(true)
            .http2_prior_knowledge()
            .timeout(std::time::Duration::from_secs(TIMEOUT_SECS))
            .default_headers({
                let mut h = reqwest::header::HeaderMap::new();
                h.insert(
                    reqwest::header::ACCEPT,
                    "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,image/apng,*/*;q=0.8,application/signed-exchange;v=b3;q=0.7"
                        .parse()
                        .unwrap(),
                );
                h.insert(
                    reqwest::header::ACCEPT_LANGUAGE,
                    "en-US,en;q=0.9,nb;q=0.8".parse().unwrap(),
                );
                h.insert(
                    "sec-ch-ua",
                    "\"Google Chrome\";v=\"131\", \"Chromium\";v=\"131\", \"Not_A Brand\";v=\"24\""
                        .parse()
                        .unwrap(),
                );
                h.insert("sec-ch-ua-mobile", "?0".parse().unwrap());
                h.insert("sec-ch-ua-platform", "\"Windows\"".parse().unwrap());
                h.insert(
                    reqwest::header::UPGRADE_INSECURE_REQUESTS,
                    "1".parse().unwrap(),
                );
                h
            })
            .build()
            .expect("Failed to build CardMarket HTTP client");
        CardMarket {
            client,
            verbose,
            last_request: Mutex::new(std::time::Instant::now()),
            warmed_up: AtomicBool::new(false),
        }
    }

    /// Pace requests so we don't hammer the server.
    /// Sleeps a random amount if the last request was too recent.
    fn throttle(&self) {
        let mut last = self.last_request.lock().unwrap();
        let elapsed = last.elapsed();
        let min_delay = Duration::from_millis(CM_DELAY_MIN_MS);
        if elapsed < min_delay {
            let extra = rand::thread_rng().gen_range(0..CM_DELAY_JITTER_MS);
            std::thread::sleep(min_delay - elapsed + Duration::from_millis(extra));
        }
        *last = std::time::Instant::now();
    }

    /// Visit the homepage once to establish a session (cookies + cf_clearance).
    fn warmup(&self) {
        if self.warmed_up.swap(true, Ordering::Relaxed) {
            return;
        }
        if self.verbose {
            eprintln!("  [cardmarket.com] warming up session...");
        }
        let _ = self.client.get(HOMEPAGE_URL).send();
        // Small pause after warmup before hitting product pages
        std::thread::sleep(Duration::from_millis(1500));
    }
}

impl Store for CardMarket {
    fn name(&self) -> &str {
        STORE_PREFIX
    }

    fn timeout_secs(&self) -> u64 {
        TIMEOUT_SECS
    }

    fn cache_keys(&self) -> Vec<String> {
        vec![
            STORE_PREFIX.to_string(),
            STORE_PREFIX_INT.to_string(),
            STORE_PREFIX_INT_PRIVATE.to_string(),
        ]
    }

    fn search(
        &self,
        _client: &reqwest::blocking::Client,
        card_name: &str,
    ) -> anyhow::Result<Vec<StoreResult>> {
        let slug = title_to_slug(card_name);
        let mut results = self.fetch_norwegian(card_name, &slug)?;
        results.extend(self.fetch_int_powerseller(card_name, &slug)?);
        results.extend(self.fetch_int_private(card_name, &slug)?);
        Ok(results)
    }

    fn search_sub(
        &self,
        _client: &reqwest::blocking::Client,
        card_name: &str,
        sub_key: &str,
    ) -> anyhow::Result<Vec<StoreResult>> {
        let slug = title_to_slug(card_name);
        match sub_key {
            STORE_PREFIX => self.fetch_norwegian(card_name, &slug),
            STORE_PREFIX_INT => self.fetch_int_powerseller(card_name, &slug),
            STORE_PREFIX_INT_PRIVATE => self.fetch_int_private(card_name, &slug),
            _ => anyhow::bail!("Unknown CardMarket sub-store: {}", sub_key),
        }
    }
}

impl CardMarket {
    fn fetch_norwegian(&self, card_name: &str, slug: &str) -> anyhow::Result<Vec<StoreResult>> {
        self.warmup();
        self.throttle();
        let url = format!("{CARD_URL}/{slug}?sellerCountry={SELLER_COUNTRY}&language=1");
        fetch_page_sellers(&self.client, &url, card_name, false, false)
            .context("CardMarket (NO) failed")
    }

    fn fetch_int_powerseller(
        &self,
        card_name: &str,
        slug: &str,
    ) -> anyhow::Result<Vec<StoreResult>> {
        self.warmup();
        self.throttle();
        let url = format!("{CARD_URL}/{slug}?sellerType={SELLER_TYPE_INT}&language=1");
        let filter = r#"{"sellerStatus":[1,2],"idLanguage":{"1":1}}"#;
        fetch_all_sellers(
            &self.client,
            &url,
            card_name,
            filter,
            true,
            false,
            self.verbose,
        )
        .context("CardMarket (INT) failed")
    }

    fn fetch_int_private(&self, card_name: &str, slug: &str) -> anyhow::Result<Vec<StoreResult>> {
        self.warmup();
        self.throttle();
        let url = format!("{CARD_URL}/{slug}?sellerType={SELLER_TYPE_PRIVATE_INT}&language=1");
        let filter = r#"{"sellerStatus":[0],"idLanguage":{"1":1}}"#;
        fetch_all_sellers(
            &self.client,
            &url,
            card_name,
            filter,
            true,
            true,
            self.verbose,
        )
        .context("CardMarket (PRIV) failed")
    }
}

/// Fetch just the initial page (no load-more). Used for Norwegian sellers
/// where the AJAX country filter format is unknown.
fn fetch_page_sellers(
    client: &reqwest::blocking::Client,
    url: &str,
    card_name: &str,
    is_international: bool,
    is_private: bool,
) -> anyhow::Result<Vec<StoreResult>> {
    let response = client
        .get(url)
        .header(
            reqwest::header::REFERER,
            "https://www.cardmarket.com/en/Magic",
        )
        .send()
        .context("GET card page failed")?;
    if !response.status().is_success() {
        anyhow::bail!("CardMarket returned HTTP {}", response.status().as_u16());
    }
    let html = response.text().context("Failed to read page body")?;

    // Detect Cloudflare challenge page
    if html.contains("challenges.cloudflare.com") || html.contains("_cf_chl_opt") {
        anyhow::bail!("Cloudflare challenge detected — try again later or from a different IP");
    }

    let document = Html::parse_document(&html);
    let entries = try_extract_sellers(&document)?;

    let store_prefix = if is_private {
        STORE_PREFIX_INT_PRIVATE
    } else if is_international {
        STORE_PREFIX_INT
    } else {
        STORE_PREFIX
    };

    // h1 check
    let heading_sel =
        scraper::Selector::parse("h1").map_err(|e| anyhow::anyhow!("CSS 'h1': {}", e))?;
    if let Some(heading) = document.select(&heading_sel).next() {
        let heading_text = heading.text().collect::<String>().trim().to_string();
        if !heading_text.is_empty() && !title_contains(card_name, &heading_text) {
            anyhow::bail!(
                "h1 mismatch: got '{}', expected '{}'",
                heading_text,
                card_name
            );
        }
    }

    Ok(entries
        .into_iter()
        .filter(|e| e.item_count > 0 && !shipping::is_blacklisted(&e.name))
        .map(|e| {
            let mut price_oere = (e.price_eur_cents as f64 * EUR_TO_NOK).round() as u32;
            if is_international {
                price_oere = (price_oere as f64 * VAT_MULTIPLIER).round() as u32;
            }
            StoreResult {
                store_name: format!("{}: {}", store_prefix, e.name),
                card_name: card_name.to_string(),
                price: price_oere,
                url: e.url,
            }
        })
        .collect())
}

// ── Fetch with load-more support ───────────────────────────────────────────

fn fetch_all_sellers(
    client: &reqwest::blocking::Client,
    url: &str,
    card_name: &str,
    filter_settings: &str,
    is_international: bool,
    is_private: bool,
    verbose: bool,
) -> anyhow::Result<Vec<StoreResult>> {
    let response = client
        .get(url)
        .header(
            reqwest::header::REFERER,
            "https://www.cardmarket.com/en/Magic",
        )
        .send()
        .context("GET card page failed")?;
    if !response.status().is_success() {
        anyhow::bail!("CardMarket returned HTTP {}", response.status().as_u16());
    }
    let html = response.text().context("Failed to read page body")?;

    // Detect Cloudflare challenge page
    if html.contains("challenges.cloudflare.com") || html.contains("_cf_chl_opt") {
        anyhow::bail!("Cloudflare challenge detected — try again later or from a different IP");
    }

    let document = Html::parse_document(&html);

    let csrf = extract_csrf(&document)?;
    let metacard_id = extract_metacard_id(&document)?;
    let mut all_entries = try_extract_sellers(&document)?;

    for page in 1..MAX_LOAD_MORE_PAGES {
        let page_str = page.to_string();
        let id_str = metacard_id.to_string();
        let form = [
            ("__cmtkn", csrf.as_str()),
            ("page", page_str.as_str()),
            ("filterSettings", filter_settings),
            ("idMetacard", id_str.as_str()),
        ];
        let resp = client
            .post(AJAX_URL)
            .form(&form)
            .header("Referer", url)
            .header("Origin", "https://www.cardmarket.com")
            .send()
            .context("POST load more failed")?;
        let ajax_html = resp.text().context("Failed to read AJAX response")?;
        // The AJAX response is XML with base64-encoded HTML in <rows>
        let decoded = if let Some(rows) = extract_ajax_rows(&ajax_html) {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(rows)
                .context("Failed to base64-decode AJAX response")?;
            String::from_utf8(bytes).context("AJAX response not valid UTF-8")?
        } else {
            String::new()
        };
        let ajax_doc = Html::parse_document(&decoded);
        let new_entries = try_extract_sellers(&ajax_doc)?;
        if verbose {
            eprintln!(
                "  [cardmarket.com] load-more page {page}: {} new sellers",
                new_entries.len()
            );
        }
        if new_entries.is_empty() {
            break;
        }
        all_entries.extend(new_entries);
    }

    let heading_sel =
        scraper::Selector::parse("h1").map_err(|e| anyhow::anyhow!("CSS 'h1': {}", e))?;
    if let Some(heading) = document.select(&heading_sel).next() {
        let heading_text = heading.text().collect::<String>().trim().to_string();
        if !heading_text.is_empty() && !title_contains(card_name, &heading_text) {
            anyhow::bail!(
                "h1 mismatch: got '{}', expected '{}'",
                heading_text,
                card_name
            );
        }
    }

    let store_prefix = if is_private {
        STORE_PREFIX_INT_PRIVATE
    } else if is_international {
        STORE_PREFIX_INT
    } else {
        STORE_PREFIX
    };

    Ok(all_entries
        .into_iter()
        .filter(|e| e.item_count > 0 && !shipping::is_blacklisted(&e.name))
        .map(|e| {
            let mut price_oere = (e.price_eur_cents as f64 * EUR_TO_NOK).round() as u32;
            if is_international {
                price_oere = (price_oere as f64 * VAT_MULTIPLIER).round() as u32;
            }
            StoreResult {
                store_name: format!("{}: {}", store_prefix, e.name),
                card_name: card_name.to_string(),
                price: price_oere,
                url: e.url,
            }
        })
        .collect())
}

fn extract_csrf(document: &scraper::Html) -> anyhow::Result<String> {
    let sel = scraper::Selector::parse("input[name=__cmtkn]")
        .map_err(|e| anyhow::anyhow!("CSS: {}", e))?;
    document
        .select(&sel)
        .next()
        .and_then(|el| el.value().attr("value"))
        .map(str::to_string)
        .context("CSRF token not found on page")
}

fn extract_metacard_id(document: &scraper::Html) -> anyhow::Result<u32> {
    let sel = scraper::Selector::parse("input[name=idMetacard]")
        .map_err(|e| anyhow::anyhow!("CSS: {}", e))?;
    document
        .select(&sel)
        .next()
        .and_then(|el| el.value().attr("value"))
        .and_then(|v| v.parse::<u32>().ok())
        .context("idMetacard not found on page")
}

// ── HTML scraping ──────────────────────────────────────────────────────────

struct SellerEntry {
    name: String,
    price_eur_cents: u32,
    item_count: u32,
    url: String,
}

fn parse_sellers_verbose(
    html: &str,
    card_name: &str,
    is_international: bool,
    is_private: bool,
) -> anyhow::Result<(Vec<StoreResult>, bool, String)> {
    let document = scraper::Html::parse_document(html);

    let heading_sel =
        scraper::Selector::parse("h1").map_err(|e| anyhow::anyhow!("CSS 'h1': {}", e))?;
    let mut heading_mismatch = false;
    let mut heading_text = String::new();
    if let Some(heading) = document.select(&heading_sel).next() {
        heading_text = heading.text().collect::<String>().trim().to_string();
        if !heading_text.is_empty() && !title_contains(card_name, &heading_text) {
            return Ok((vec![], true, heading_text));
        }
    } else {
        heading_mismatch = true;
    }

    let entries = try_extract_sellers(&document)?;

    if entries.is_empty() {
        return Ok((vec![], heading_mismatch, heading_text));
    }

    let store_prefix = if is_private {
        STORE_PREFIX_INT_PRIVATE
    } else if is_international {
        STORE_PREFIX_INT
    } else {
        STORE_PREFIX
    };

    let results: Vec<StoreResult> = entries
        .into_iter()
        .filter(|e| e.item_count > 0 && !shipping::is_blacklisted(&e.name))
        .map(|e| {
            let mut price_oere = (e.price_eur_cents as f64 * EUR_TO_NOK).round() as u32;
            if is_international {
                price_oere = (price_oere as f64 * VAT_MULTIPLIER).round() as u32;
            }
            StoreResult {
                store_name: format!("{}: {}", store_prefix, e.name),
                card_name: card_name.to_string(),
                price: price_oere,
                url: e.url,
            }
        })
        .collect();

    Ok((results, false, heading_text))
}

#[allow(dead_code)]
fn parse_sellers(
    html: &str,
    card_name: &str,
    is_international: bool,
    is_private: bool,
) -> anyhow::Result<Vec<StoreResult>> {
    parse_sellers_verbose(html, card_name, is_international, is_private).map(|(r, _, _)| r)
}

fn try_extract_sellers(document: &scraper::Html) -> anyhow::Result<Vec<SellerEntry>> {
    let row_sel = scraper::Selector::parse("div.article-row")
        .map_err(|e| anyhow::anyhow!("CSS 'div.article-row': {}", e))?;

    let seller_link_sel = scraper::Selector::parse("span.seller-name a")
        .map_err(|e| anyhow::anyhow!("CSS 'span.seller-name a': {}", e))?;

    let price_sel = scraper::Selector::parse("div.col-offer span.color-primary.fw-bold")
        .map_err(|e| anyhow::anyhow!("CSS price: {}", e))?;

    let count_sel = scraper::Selector::parse("div.col-offer span.item-count")
        .map_err(|e| anyhow::anyhow!("CSS item count: {}", e))?;

    let mut entries: Vec<SellerEntry> = Vec::new();

    for row in document.select(&row_sel) {
        let seller_el = match row.select(&seller_link_sel).next() {
            Some(el) => el,
            None => continue,
        };

        let name = seller_el.text().collect::<String>().trim().to_string();
        let url = seller_el
            .value()
            .attr("href")
            .map(|href| {
                if href.starts_with("http") {
                    href.to_string()
                } else {
                    format!("https://www.cardmarket.com{href}")
                }
            })
            .unwrap_or_default();

        if name.is_empty() || url.is_empty() {
            continue;
        }

        let price_eur_cents = match row.select(&price_sel).next() {
            Some(el) => {
                let text = el.text().collect::<String>();
                match parse_eur_price(&text) {
                    Some(p) => p,
                    None => continue,
                }
            }
            None => continue,
        };

        let item_count = row
            .select(&count_sel)
            .next()
            .and_then(|el| {
                let text = el.text().collect::<String>();
                text.trim().parse::<u32>().ok()
            })
            .unwrap_or(1);

        entries.push(SellerEntry {
            name,
            price_eur_cents,
            item_count,
            url,
        });
    }

    Ok(entries)
}

// ── Price parsing ──────────────────────────────────────────────────────────

fn parse_eur_price(raw: &str) -> Option<u32> {
    let raw = raw.trim();

    let cleaned = raw
        .replace('€', "")
        .replace("EUR", "")
        .replace("eur", "")
        .trim()
        .to_string();

    if cleaned.is_empty() {
        return None;
    }

    let num_str: String = cleaned
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == ',' || *c == '.')
        .collect();

    if num_str.is_empty() {
        return None;
    }

    if num_str.contains(',') {
        let (whole_part, frac_part) = num_str.split_once(',')?;
        let whole_str: String = whole_part.chars().filter(|c| c.is_ascii_digit()).collect();
        let whole: u32 = if whole_str.is_empty() {
            0
        } else {
            whole_str.parse().ok()?
        };
        let frac_str: String = frac_part
            .chars()
            .filter(|c| c.is_ascii_digit())
            .take(2)
            .collect();
        let frac: u32 = match frac_str.len() {
            0 => 0,
            1 => frac_str.parse::<u32>().ok()? * 10,
            _ => frac_str.parse().ok()?,
        };
        if frac >= 100 {
            return None;
        }
        return Some(whole * 100 + frac);
    }

    if num_str.contains('.') {
        let (whole_str, frac_part) = num_str.split_once('.')?;
        let whole: u32 = whole_str.parse().ok()?;
        let frac_str: String = frac_part
            .chars()
            .filter(|c| c.is_ascii_digit())
            .take(2)
            .collect();
        let frac: u32 = match frac_str.len() {
            0 => 0,
            1 => frac_str.parse::<u32>().ok()? * 10,
            _ => frac_str.parse().ok()?,
        };
        if frac >= 100 {
            return None;
        }
        return Some(whole * 100 + frac);
    }

    let whole: u32 = num_str.parse().ok()?;
    Some(whole * 100)
}

/// Extract the base64-encoded content from <rows>...</rows> in the AJAX XML response.
fn extract_ajax_rows(xml: &str) -> Option<&str> {
    let start = xml.find("<rows>")? + 6;
    let end = xml[start..].find("</rows>")?;
    Some(&xml[start..start + end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_store_name() {
        let store = CardMarket::new(false);
        assert_eq!(store.name(), "cardmarket.com");
    }

    #[test]
    fn test_title_to_slug() {
        assert_eq!(title_to_slug("Evolving Wilds"), "Evolving-Wilds");
        assert_eq!(title_to_slug("Snakeskin Veil"), "Snakeskin-Veil");
    }

    #[test]
    fn test_parse_eur_price() {
        assert_eq!(parse_eur_price("0,02 €"), Some(2));
        assert_eq!(parse_eur_price("12,50 €"), Some(1250));
        assert_eq!(parse_eur_price("€ 1,50"), Some(150));
        assert_eq!(parse_eur_price("  3,99 €  "), Some(399));
        assert_eq!(parse_eur_price("5.00"), Some(500));
        assert_eq!(parse_eur_price("10 EUR"), Some(1000));
        assert_eq!(parse_eur_price("1.234,56"), Some(123456));
        assert_eq!(parse_eur_price("abc"), None);
        assert_eq!(parse_eur_price(""), None);
    }

    #[test]
    fn test_eur_to_nok_conversion() {
        let eur_cents = 1250u32;
        let nok_oere = (eur_cents as f64 * EUR_TO_NOK).round() as u32;
        assert_eq!(nok_oere, 13750);
    }

    #[test]
    fn test_parse_sellers_empty_page() {
        let html = "<html><body><h1>No results</h1></body></html>";
        let result = parse_sellers(html, "Evolving Wilds", false, false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_wrong_card_redirect() {
        let html = "<html><body><h1>Black Lotus</h1></body></html>";
        let (_, warning, heading) =
            parse_sellers_verbose(html, "Evolving Wilds", false, false).unwrap();
        assert!(warning);
        assert_eq!(heading, "Black Lotus");
    }

    #[test]
    fn test_parse_sellers_real_html() {
        let html = include_str!("cardmarket_test.html");
        let (results, warning, _heading) =
            parse_sellers_verbose(html, "Evolving Wilds", false, false).unwrap();
        assert!(!warning, "h1 mismatch");
        assert!(!results.is_empty(), "no sellers parsed");
        let first = &results[0];
        assert!(first.store_name.starts_with("cardmarket.com:"));
        assert!(first.price > 0);
        assert!(!first.url.is_empty());
    }

    #[test]
    fn test_parse_sellers_international() {
        let html = include_str!("cardmarket_test.html");
        let (results, warning, _heading) =
            parse_sellers_verbose(html, "Evolving Wilds", true, false).unwrap();
        assert!(!warning, "h1 mismatch");
        assert!(!results.is_empty(), "no sellers parsed");
        let first = &results[0];
        assert!(first.store_name.starts_with("cardmarket-int.com:"));
        assert!(first.price > 0);
    }

    #[test]
    fn test_blacklisted_seller_filtered() {
        let html = include_str!("cardmarket_test.html");
        let (results, _, _) = parse_sellers_verbose(html, "Evolving Wilds", false, false).unwrap();
        for r in &results {
            let seller = r.store_name.strip_prefix("cardmarket.com: ").unwrap_or("");
            assert!(!shipping::is_blacklisted(seller));
        }
    }
}
