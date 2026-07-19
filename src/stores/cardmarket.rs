use super::{title_contains, title_to_slug, Store, StoreResult};
use crate::shipping::{self, EUR_TO_NOK, VAT_MULTIPLIER};
use anyhow::Context;
use base64::Engine;
use headless_chrome::protocol::cdp::Network::Cookie;
use headless_chrome::{Browser, LaunchOptions};
use reqwest::cookie::Jar;
use scraper::Html;
use serde::Deserialize;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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

// ── Headless-browser CardMarket implementation ─────────────────────────────

pub struct CardMarket {
    _browser: Browser,
    /// Xvfb child process — kept alive so the virtual display exists while
    /// the browser runs.  Killed on drop.
    _xvfb: Option<std::process::Child>,
    /// A single reusable browser tab.  All card searches navigate this
    /// same tab to avoid leaking tabs in the browser process.
    tab: Arc<headless_chrome::Tab>,
    /// Per-card cache: once we fetch all three sub-endpoints in one tab
    /// session, subsequent `search_sub` calls for the same card return
    /// from here without opening new tabs.
    card_cache: Mutex<std::collections::HashMap<String, CardResults>>,
    verbose: bool,
}

/// Results from all three CardMarket sub-endpoints for a single card.
struct CardResults {
    norwegian: Vec<StoreResult>,
    int_powerseller: Vec<StoreResult>,
    int_private: Vec<StoreResult>,
}

impl CardMarket {
    pub fn new(verbose: bool) -> Self {
        // Try to start a virtual X display via Xvfb.
        // Cloudflare detects true headless Chrome; running on Xvfb makes
        // Chrome indistinguishable from a normal desktop browser.
        let (xvfb_child, display) = start_xvfb(verbose);

        if let Some(ref d) = display {
            std::env::set_var("DISPLAY", d);
        }

        let launch_opts = LaunchOptions::default_builder()
            .headless(display.is_none()) // headless only if no Xvfb
            .sandbox(false) // Often needed on Linux
            .window_size(Some((1920, 1080)))
            .args(vec![
                std::ffi::OsStr::new("--disable-dev-shm-usage"),
                std::ffi::OsStr::new("--disable-blink-features=AutomationControlled"),
            ])
            .build()
            .expect("Failed to build Chrome launch options");

        let browser = Browser::new(launch_opts).expect(
            "Failed to launch Chrome. Is google-chrome or chromium installed?\n\
             On Linux, also ensure Xvfb is available: apt install xvfb",
        );

        // Create one reusable tab that lives for the entire store session.
        let tab = browser.new_tab().expect("Failed to create browser tab");

        if verbose {
            if display.is_some() {
                eprintln!("  [cardmarket] Chrome launched on Xvfb virtual display");
            } else {
                eprintln!("  [cardmarket] Chrome launched in headless mode (no Xvfb)");
            }
        }

        CardMarket {
            _browser: browser,
            _xvfb: xvfb_child,
            tab,
            card_cache: Mutex::new(std::collections::HashMap::new()),
            verbose,
        }
    }
}

impl Drop for CardMarket {
    fn drop(&mut self) {
        // Close the reusable tab so the browser page doesn't leak.
        let _ = self.tab.close(false);
        if let Some(ref mut child) = self._xvfb {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Launch Xvfb on an unused display.  Returns (child_process, display_string).
fn start_xvfb(verbose: bool) -> (Option<std::process::Child>, Option<String>) {
    use std::process::Command;

    // Try displays 99 down to 90
    for n in (90..=99).rev() {
        let display = format!(":{n}");
        match Command::new("Xvfb")
            .arg(&display)
            .arg("-screen")
            .arg("0")
            .arg("1920x1080x24")
            .arg("-nolisten")
            .arg("tcp")
            .arg("-ac")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(child) => {
                // Give Xvfb a moment to initialize
                std::thread::sleep(std::time::Duration::from_millis(300));
                if verbose {
                    eprintln!("  [cardmarket] Xvfb started on display {display}");
                }
                return (Some(child), Some(display));
            }
            Err(_) => continue,
        }
    }

    if verbose {
        eprintln!("  [cardmarket] Xvfb not available, falling back to headless mode");
    }
    (None, None)
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
        let results = self.fetch_all_in_one_tab(&self.tab, card_name, &slug)?;
        let mut all = results.norwegian;
        all.extend(results.int_powerseller);
        all.extend(results.int_private);
        Ok(all)
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

        // Check if we already fetched this card (all 3 sub-endpoints) in one
        // browser session.
        {
            let cache = self.card_cache.lock().unwrap();
            if let Some(results) = cache.get(card_name) {
                return Ok(match sub_key {
                    STORE_PREFIX => results.norwegian.clone(),
                    STORE_PREFIX_INT => results.int_powerseller.clone(),
                    STORE_PREFIX_INT_PRIVATE => results.int_private.clone(),
                    _ => anyhow::bail!("Unknown CardMarket sub-store: {}", sub_key),
                });
            }
        }

        // Not in cache — fetch all three endpoints in a single tab so the
        // Cloudflare session (cookies, localStorage, etc.) carries over.
        let slug = title_to_slug(card_name);
        match self.fetch_all_in_one_tab(&self.tab, card_name, &slug) {
            Ok(results) => {
                let sub = match sub_key {
                    STORE_PREFIX => results.norwegian.clone(),
                    STORE_PREFIX_INT => results.int_powerseller.clone(),
                    STORE_PREFIX_INT_PRIVATE => results.int_private.clone(),
                    _ => anyhow::bail!("Unknown CardMarket sub-store: {}", sub_key),
                };
                self.card_cache
                    .lock()
                    .unwrap()
                    .insert(card_name.to_string(), results);
                Ok(sub)
            }
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

// ── Fetch methods ──────────────────────────────────────────────────────────

impl CardMarket {
    /// Fetch all three CardMarket sub-endpoints for `card_name` using the
    /// given browser tab.  The tab is reused across cards to avoid leaking
    /// tabs in the browser process.
    fn fetch_all_in_one_tab(
        &self,
        tab: &headless_chrome::Tab,
        card_name: &str,
        slug: &str,
    ) -> anyhow::Result<CardResults> {
        // 1. Norwegian sellers (no load-more)
        let no_url = format!("{CARD_URL}/{slug}?sellerCountry={SELLER_COUNTRY}&language=1");
        let (no_html, _) =
            navigate_tab(tab, &no_url, self.verbose).context("CardMarket (NO) failed")?;
        let no_entries = parse_card_page(card_name, &no_html)?;

        // 2. International powersellers (with load-more)
        let int_url = format!("{CARD_URL}/{slug}?sellerType={SELLER_TYPE_INT}&language=1");
        let int_filter = r#"{"sellerStatus":[1,2],"idLanguage":{"1":1}}"#;
        let int_entries = self
            .fetch_with_load_more(card_name, tab, &int_url, int_filter, self.verbose)
            .context("CardMarket (INT) failed")?;

        // 3. International private sellers (with load-more)
        let priv_url = format!("{CARD_URL}/{slug}?sellerType={SELLER_TYPE_PRIVATE_INT}&language=1");
        let priv_filter = r#"{"sellerStatus":[0],"idLanguage":{"1":1}}"#;
        let priv_entries = self
            .fetch_with_load_more(card_name, tab, &priv_url, priv_filter, self.verbose)
            .context("CardMarket (PRIV) failed")?;

        Ok(CardResults {
            norwegian: sellers_to_results(no_entries, card_name, false, false),
            int_powerseller: sellers_to_results(int_entries, card_name, true, false),
            int_private: sellers_to_results(priv_entries, card_name, true, true),
        })
    }

    /// Navigate to a CardMarket page in an existing tab, extract sellers,
    /// then POST AJAX load-more requests (using reqwest with the tab's
    /// cookies) to get all seller pages.
    fn fetch_with_load_more(
        &self,
        _card_name: &str,
        tab: &headless_chrome::Tab,
        url: &str,
        filter: &str,
        verbose: bool,
    ) -> anyhow::Result<Vec<SellerEntry>> {
        let (html, cookies) = navigate_tab(tab, url, verbose)?;

        let document = Html::parse_document(&html);
        let csrf = extract_csrf(&document)?;
        let metacard_id = extract_metacard_id(&document)?;
        let mut all_entries = try_extract_sellers(&document)?;

        // Build reqwest client with browser cookies for AJAX
        let ajax_client = build_ajax_client(&cookies)?;

        for page in 1..MAX_LOAD_MORE_PAGES {
            let page_str = page.to_string();
            let id_str = metacard_id.to_string();
            let form = [
                ("__cmtkn", csrf.as_str()),
                ("page", page_str.as_str()),
                ("filterSettings", filter),
                ("idMetacard", id_str.as_str()),
            ];
            let resp = ajax_client
                .post(AJAX_URL)
                .form(&form)
                .header("Referer", url)
                .header("Origin", "https://www.cardmarket.com")
                .send()
                .context("POST load more failed")?;

            if !resp.status().is_success() {
                if verbose {
                    eprintln!(
                        "  [cardmarket.com] load-more page {page}: HTTP {}",
                        resp.status().as_u16()
                    );
                }
                break;
            }

            let ajax_html = resp.text().context("Failed to read AJAX response")?;
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

        Ok(all_entries)
    }
}

/// Navigate an existing tab to `url`, wait for Cloudflare to resolve,
/// and return (html, cookies).
fn navigate_tab(
    tab: &headless_chrome::Tab,
    url: &str,
    verbose: bool,
) -> anyhow::Result<(String, Vec<Cookie>)> {
    if verbose {
        eprintln!("  [cardmarket] browser GET {url}");
    }

    tab.navigate_to(url).context("Browser navigation failed")?;
    tab.wait_until_navigated().context("Page load timeout")?;

    // Wait for Cloudflare Turnstile to resolve.
    let start = Instant::now();
    let timeout = Duration::from_secs(30);
    loop {
        let html = tab.get_content().context("Failed to read page content")?;

        if !html.contains("_cf_chl_opt") && !html.contains("challenges.cloudflare.com") {
            std::thread::sleep(Duration::from_secs(2));
            let html = tab
                .get_content()
                .context("Failed to read final page content")?;
            let cookies = tab.get_cookies().context("Failed to read cookies")?;

            if verbose {
                eprintln!(
                    "  [cardmarket] <- page loaded ({} bytes, {} cookies)",
                    html.len(),
                    cookies.len()
                );
            }

            return Ok((html, cookies));
        }

        if start.elapsed() > timeout {
            // Queue for semi-manual rescue if enabled
            let card_slug = url
                .split('/')
                .next_back()
                .unwrap_or("")
                .split('?')
                .next()
                .unwrap_or("");
            let prefix = if url.contains("sellerType=0") {
                STORE_PREFIX_INT_PRIVATE
            } else if url.contains("sellerType") {
                STORE_PREFIX_INT
            } else {
                STORE_PREFIX
            };
            queue_rescue(card_slug, prefix, url);
            anyhow::bail!("Cloudflare challenge could not be solved — try again later");
        }

        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Parse the card page HTML and extract seller entries, with h1 validation.
fn parse_card_page(card_name: &str, html: &str) -> anyhow::Result<Vec<SellerEntry>> {
    let document = Html::parse_document(html);
    let entries = try_extract_sellers(&document)?;

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

    Ok(entries)
}

// ── AJAX client helper ────────────────────────────────────────────────────

/// Build a `reqwest::blocking::Client` that carries the cookies from a
/// headless-browser session (especially the `cf_clearance` cookie).
fn build_ajax_client(cookies: &[Cookie]) -> anyhow::Result<reqwest::blocking::Client> {
    let jar = Arc::new(Jar::default());
    let base_url: reqwest::Url = "https://www.cardmarket.com".parse().unwrap();

    for cookie in cookies {
        jar.add_cookie_str(&format!("{}={}", cookie.name, cookie.value), &base_url);
    }

    reqwest::blocking::Client::builder()
        .cookie_provider(jar)
        .user_agent(
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
             (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36",
        )
        .timeout(Duration::from_secs(TIMEOUT_SECS))
        .build()
        .context("Failed to build AJAX client")
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
    fn test_title_to_slug() {
        assert_eq!(title_to_slug("Evolving Wilds"), "Evolving-Wilds");
        assert_eq!(title_to_slug("Find // Finality"), "Find-Finality");
        assert_eq!(title_to_slug("Hydra's Growth"), "Hydras-Growth");
    }

    #[test]
    fn test_parse_eur_price() {
        assert_eq!(parse_eur_price("0,02 €"), Some(2));
        assert_eq!(parse_eur_price("1,50 €"), Some(150));
        assert_eq!(parse_eur_price("10,00 €"), Some(1000));
        assert_eq!(parse_eur_price("123,45 €"), Some(12345));
        assert_eq!(parse_eur_price("0.02 EUR"), Some(2));
        assert_eq!(parse_eur_price("1.50"), Some(150));
        assert_eq!(parse_eur_price(""), None);
        assert_eq!(parse_eur_price("N/A"), None);
    }

    #[test]
    fn test_eur_to_nok_conversion() {
        // €1.00 ≈ 11.70 NOK → 1170 oere (± round)
        let eur_cents = 100u32;
        let nok_oere = (eur_cents as f64 * EUR_TO_NOK).round() as u32;
        let expected_vat = (nok_oere as f64 * VAT_MULTIPLIER).round() as u32;
        // Just verify the constants are reasonable
        assert!(EUR_TO_NOK > 10.0 && EUR_TO_NOK < 13.0);
        assert!(VAT_MULTIPLIER > 1.2 && VAT_MULTIPLIER < 1.3);
        assert!(nok_oere > 1000);
        assert!(expected_vat > nok_oere);
    }

    #[test]
    fn test_wrong_card_redirect() {
        // h1 mismatch should bail
        let html = r#"<html><body><h1>Wrong Card</h1></body></html>"#;
        let document = Html::parse_document(html);
        let heading_sel = scraper::Selector::parse("h1").unwrap();
        if let Some(heading) = document.select(&heading_sel).next() {
            let heading_text = heading.text().collect::<String>().trim().to_string();
            assert!(!title_contains("Lightning Bolt", &heading_text));
        }
    }
}
