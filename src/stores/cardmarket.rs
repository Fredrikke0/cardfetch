use super::{title_contains, title_to_slug, Store, StoreResult};
use crate::shipping::{self, EUR_TO_NOK, VAT_MULTIPLIER};
use anyhow::Context;
use base64::Engine;
use scraper::Html;
use serde::Deserialize;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Mutex;
use wreq_util::Emulation;

const CARD_URL: &str = "https://www.cardmarket.com/en/Magic/Cards";
const STORE_PREFIX: &str = "cardmarket.com";
const STORE_PREFIX_INT: &str = "cardmarket-int.com";
const STORE_PREFIX_INT_PRIVATE: &str = "cardmarket-int-private.com";
const AJAX_URL: &str = "https://www.cardmarket.com/en/Magic/AjaxAction/Metacard_LoadMoreArticles";

const SELLER_COUNTRY: &str = "24";
const SELLER_TYPE_INT: &str = "1,2";
const SELLER_TYPE_PRIVATE_INT: &str = "0";

const TIMEOUT_SECS: u64 = 30;
const MAX_LOAD_MORE_PAGES: u32 = 10;

/// How many Cloudflare blocks before we stop trying CardMarket for this run.
const BLOCK_LIMIT: u32 = 3;

/// Global counter for Cloudflare blocks during this run.
static BLOCK_COUNT: AtomicU32 = AtomicU32::new(0);

/// Whether semi-manual rescue mode is active.
static SEMI_MANUAL: AtomicBool = AtomicBool::new(false);

/// Enable or disable semi-manual rescue mode.
pub(crate) fn set_semi_manual(v: bool) {
    SEMI_MANUAL.store(v, Ordering::Relaxed);
}

/// Check whether CardMarket has hit the block limit and should be skipped.
/// In semi-manual mode, never give up — failed fetches get queued for rescue.
pub fn is_blocked() -> bool {
    if SEMI_MANUAL.load(Ordering::Relaxed) {
        return false;
    }
    BLOCK_COUNT.load(Ordering::Relaxed) >= BLOCK_LIMIT
}

/// Record a Cloudflare block.
fn record_cloudflare_block() {
    BLOCK_COUNT.fetch_add(1, Ordering::Relaxed);
}

/// Check if an error is a Cloudflare challenge block.
fn is_cloudflare_error(err: &anyhow::Error) -> bool {
    err.chain()
        .any(|c| c.to_string().contains("Cloudflare challenge"))
}

// ── Rescue queue ────────────────────────────────────────────────────────────

/// A card+endpoint that needs manual rescue via browser.
#[derive(Debug, Clone)]
pub(crate) struct RescueItem {
    pub card_name: String,
    pub sub_key: String,
    pub url: String,
}

/// Queue of (card, endpoint) pairs blocked by Cloudflare, pending manual rescue.
static RESCUE_QUEUE: Mutex<Vec<RescueItem>> = Mutex::new(Vec::new());

/// Push a rescue item onto the queue.
fn queue_rescue(card_name: &str, sub_key: &str, url: &str) {
    RESCUE_QUEUE.lock().unwrap().push(RescueItem {
        card_name: card_name.to_string(),
        sub_key: sub_key.to_string(),
        url: url.to_string(),
    });
}

/// Drain and return all queued rescue items.
pub(crate) fn drain_rescue_queue() -> Vec<RescueItem> {
    std::mem::take(&mut *RESCUE_QUEUE.lock().unwrap())
}

/// Error type signalling a fetch is queued for manual rescue.
#[derive(Debug)]
pub(crate) struct RescuePending;

impl std::fmt::Display for RescuePending {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "queued for semi-manual rescue")
    }
}

impl std::error::Error for RescuePending {}

/// Return the JS snippet the user pastes into the browser console.
pub(crate) fn rescue_js_snippet() -> &'static str {
    "copy(JSON.stringify(\
     [...document.querySelectorAll('.article-row')].map(row=>{\
       const a=row.querySelector('.seller-name a');\
       const p=row.querySelector('.col-offer span.color-primary.fw-bold,.col-offer span.color-primary');\
       const c=row.querySelector('.col-offer span.item-count');\
       const h=a?.getAttribute('href')||'';\
       return {\
         n:a?.textContent?.trim()||'',\
         p:p?.textContent?.trim()||'',\
         c:parseInt(c?.textContent?.trim())||1,\
         u:h.startsWith('http')?h:'https://www.cardmarket.com'+h\
       };\
     })\
   ));"
}

pub struct CardMarket {
    client: wreq::Client,
    rt: tokio::runtime::Runtime,
    verbose: bool,
}

impl CardMarket {
    pub fn new(verbose: bool) -> Self {
        let rt = tokio::runtime::Runtime::new().expect("Failed to create tokio runtime");
        let client = rt.block_on(async {
            wreq::Client::builder()
                .emulation(Emulation::Chrome124)
                .cookie_store(true)
                .timeout(std::time::Duration::from_secs(TIMEOUT_SECS))
                .default_headers({
                    let mut h = wreq::header::HeaderMap::new();
                    h.insert(
                        "accept",
                        "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,image/apng,*/*;q=0.8,application/signed-exchange;v=b3;q=0.7"
                            .parse()
                            .unwrap(),
                    );
                    h.insert(
                        "accept-language",
                        "en-US,en;q=0.9".parse().unwrap(),
                    );
                    h.insert(
                        "sec-ch-ua",
                        "\"Chromium\";v=\"124\", \"Google Chrome\";v=\"124\", \"Not-A.Brand\";v=\"99\""
                            .parse()
                            .unwrap(),
                    );
                    h.insert("sec-ch-ua-mobile", "?0".parse().unwrap());
                    h.insert(
                        "sec-ch-ua-platform",
                        "\"Windows\"".parse().unwrap(),
                    );
                    h.insert("upgrade-insecure-requests", "1".parse().unwrap());
                    h
                })
                .build()
        })
        .expect("Failed to build CardMarket wreq client");
        CardMarket {
            client,
            rt,
            verbose,
        }
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
        if is_blocked() {
            return Ok(vec![]);
        }
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
        if is_blocked() {
            return Ok(vec![]);
        }
        let slug = title_to_slug(card_name);
        let result = match sub_key {
            STORE_PREFIX => self.fetch_norwegian(card_name, &slug),
            STORE_PREFIX_INT => self.fetch_int_powerseller(card_name, &slug),
            STORE_PREFIX_INT_PRIVATE => self.fetch_int_private(card_name, &slug),
            _ => anyhow::bail!("Unknown CardMarket sub-store: {}", sub_key),
        };
        match result {
            Ok(r) => Ok(r),
            Err(ref e) if is_cloudflare_error(e) => {
                record_cloudflare_block();
                if SEMI_MANUAL.load(Ordering::Relaxed) {
                    Err(anyhow::Error::new(RescuePending))
                } else {
                    Err(anyhow::anyhow!("Cloudflare challenge"))
                }
            }
            Err(e) => {
                let detail: String = e
                    .chain()
                    .map(|c| c.to_string())
                    .collect::<Vec<_>>()
                    .join(": ");
                if self.verbose {
                    eprintln!("  [{}] {detail}", sub_key);
                }
                Err(anyhow::anyhow!("{detail}"))
            }
        }
    }
}

impl CardMarket {
    fn fetch_norwegian(&self, card_name: &str, slug: &str) -> anyhow::Result<Vec<StoreResult>> {
        let url = format!("{CARD_URL}/{slug}?sellerCountry={SELLER_COUNTRY}&language=1");
        fetch_page_sellers(
            &self.client,
            &self.rt,
            &url,
            card_name,
            false,
            false,
            self.verbose,
        )
        .context("CardMarket (NO) failed")
    }

    fn fetch_int_powerseller(
        &self,
        card_name: &str,
        slug: &str,
    ) -> anyhow::Result<Vec<StoreResult>> {
        let url = format!("{CARD_URL}/{slug}?sellerType={SELLER_TYPE_INT}&language=1");
        let filter = r#"{"sellerStatus":[1,2],"idLanguage":{"1":1}}"#;
        fetch_all_sellers(
            &self.client,
            &self.rt,
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
        let url = format!("{CARD_URL}/{slug}?sellerType={SELLER_TYPE_PRIVATE_INT}&language=1");
        let filter = r#"{"sellerStatus":[0],"idLanguage":{"1":1}}"#;
        fetch_all_sellers(
            &self.client,
            &self.rt,
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

/// Fetch just the initial page (no load-more).
fn fetch_page_sellers(
    client: &wreq::Client,
    rt: &tokio::runtime::Runtime,
    url: &str,
    card_name: &str,
    is_international: bool,
    is_private: bool,
    verbose: bool,
) -> anyhow::Result<Vec<StoreResult>> {
    if verbose {
        eprintln!("  [cardmarket] GET {url}");
    }
    let response = rt
        .block_on(client.get(url).send())
        .context("GET card page failed")?;
    if verbose {
        eprintln!("  [cardmarket] <- HTTP {} ", response.status());
        for (name, value) in response.headers() {
            eprintln!("  [cardmarket]   {name}: {value:?}");
        }
    }
    let status = response.status();
    let html = rt
        .block_on(response.text())
        .context("Failed to read page body")?;

    // Check for Cloudflare challenge before rejecting on status code —
    // managed challenges often return 403 with a challenge body.
    if html.contains("challenges.cloudflare.com") || html.contains("_cf_chl_opt") {
        let prefix = store_prefix_from_flags(is_international, is_private);
        queue_rescue(card_name, prefix, url);
        anyhow::bail!("Cloudflare challenge detected — try again later or from a different IP");
    }

    if !status.is_success() {
        anyhow::bail!("CardMarket returned HTTP {}", status.as_u16());
    }

    let document = Html::parse_document(&html);
    let entries = try_extract_sellers(&document)?;

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

    Ok(sellers_to_results(
        entries,
        card_name,
        is_international,
        is_private,
    ))
}

// ── Fetch with load-more support ───────────────────────────────────────────

fn fetch_all_sellers(
    client: &wreq::Client,
    rt: &tokio::runtime::Runtime,
    url: &str,
    card_name: &str,
    filter_settings: &str,
    is_international: bool,
    is_private: bool,
    verbose: bool,
) -> anyhow::Result<Vec<StoreResult>> {
    if verbose {
        eprintln!("  [cardmarket] GET {url}");
    }
    let response = rt
        .block_on(client.get(url).send())
        .context("GET card page failed")?;
    if verbose {
        eprintln!("  [cardmarket] <- HTTP {} ", response.status());
        for (name, value) in response.headers() {
            eprintln!("  [cardmarket]   {name}: {value:?}");
        }
    }
    let status = response.status();
    let html = rt
        .block_on(response.text())
        .context("Failed to read page body")?;

    // Check for Cloudflare challenge before rejecting on status code
    if html.contains("challenges.cloudflare.com") || html.contains("_cf_chl_opt") {
        let prefix = store_prefix_from_flags(is_international, is_private);
        queue_rescue(card_name, prefix, url);
        anyhow::bail!("Cloudflare challenge detected — try again later or from a different IP");
    }

    if !status.is_success() {
        anyhow::bail!("CardMarket returned HTTP {}", status.as_u16());
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
        let resp = rt
            .block_on(
                client
                    .post(AJAX_URL)
                    .form(&form)
                    .header("Referer", url)
                    .header("Origin", "https://www.cardmarket.com")
                    .send(),
            )
            .context("POST load more failed")?;
        let ajax_html = rt
            .block_on(resp.text())
            .context("Failed to read AJAX response")?;
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

    Ok(sellers_to_results(
        all_entries,
        card_name,
        is_international,
        is_private,
    ))
}

// ── Shared helpers ──────────────────────────────────────────────────────────

/// Map is_international/is_private flags to a store prefix string.
fn store_prefix_from_flags(is_international: bool, is_private: bool) -> &'static str {
    if is_private {
        STORE_PREFIX_INT_PRIVATE
    } else if is_international {
        STORE_PREFIX_INT
    } else {
        STORE_PREFIX
    }
}

/// Convert seller entries to StoreResults with price conversion, VAT, and
/// store-prefix formatting.
fn sellers_to_results(
    entries: Vec<SellerEntry>,
    card_name: &str,
    is_international: bool,
    is_private: bool,
) -> Vec<StoreResult> {
    let store_prefix = store_prefix_from_flags(is_international, is_private);
    entries
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
        .collect()
}

/// JSON shape produced by the browser JS snippet.
#[derive(Deserialize)]
struct RescueSellerJson {
    n: String,
    p: String,
    c: u32,
    u: String,
}

/// Parse seller JSON from the browser snippet and convert to StoreResults.
/// Returns `None` if the JSON array is empty (no sellers found on the page).
pub(crate) fn sellers_from_json(
    json: &str,
    card_name: &str,
    sub_key: &str,
) -> anyhow::Result<Vec<StoreResult>> {
    let raw: Vec<RescueSellerJson> =
        serde_json::from_str(json).context("Failed to parse rescue JSON")?;

    let (is_international, is_private) = match sub_key {
        STORE_PREFIX => (false, false),
        STORE_PREFIX_INT => (true, false),
        STORE_PREFIX_INT_PRIVATE => (true, true),
        _ => anyhow::bail!("Unknown sub_key: {}", sub_key),
    };

    let entries: Vec<SellerEntry> = raw
        .into_iter()
        .filter(|s| !s.n.is_empty() && !s.p.is_empty())
        .filter_map(|s| {
            let price_eur_cents = parse_eur_price(&s.p)?;
            Some(SellerEntry {
                name: s.n,
                price_eur_cents,
                item_count: s.c,
                url: s.u,
            })
        })
        .collect();

    Ok(sellers_to_results(
        entries,
        card_name,
        is_international,
        is_private,
    ))
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
