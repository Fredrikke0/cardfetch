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

/// Reset the Cloudflare block counter so the next fetch batch gets a fresh
/// chance instead of remaining permanently blocked.  Called at the end of each
/// batch from `CardMarket::teardown`.
fn reset_block_count() {
    BLOCK_COUNT.store(0, Ordering::Relaxed);
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

/// Holds a running Chrome browser, its virtual display, and the reusable tab.
/// Created fresh for each request batch and torn down afterwards, so Chrome
/// never lives longer than a single fetch operation.
struct BrowserSession {
    _browser: Browser,
    tab: Arc<headless_chrome::Tab>,
    xvfb: Option<std::process::Child>,
}

impl Drop for BrowserSession {
    fn drop(&mut self) {
        let _ = self.tab.close(false);
        if let Some(ref mut child) = self.xvfb {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

pub struct CardMarket {
    /// Serializes access to the single browser tab.  The session is created
    /// at the start of a request batch and torn down afterwards.
    session: Mutex<Option<BrowserSession>>,
    card_cache: Mutex<std::collections::HashMap<String, CardResults>>,
    /// Optional persistent SQLite cache — when set, CardMarket persists
    /// results for ALL three sub-endpoints (not just the requested one)
    /// so subsequent runs hit the cache for every sub-key.
    persistent_cache: Option<Arc<crate::cache::Cache>>,
    verbose: bool,
}

/// Results from all three CardMarket sub-endpoints for a single card.
struct CardResults {
    norwegian: Vec<StoreResult>,
    int_powerseller: Vec<StoreResult>,
    int_private: Vec<StoreResult>,
}

impl CardMarket {
    pub fn new(verbose: bool, persistent_cache: Option<Arc<crate::cache::Cache>>) -> Self {
        CardMarket {
            session: Mutex::new(None),
            card_cache: Mutex::new(std::collections::HashMap::new()),
            persistent_cache,
            verbose,
        }
    }

    /// Lazily create a `BrowserSession` (Chrome + Xvfb + tab).
    /// Must be called while holding the `session` lock.
    fn create_session(verbose: bool) -> anyhow::Result<BrowserSession> {
        let (xvfb_child, display) = start_xvfb(verbose);

        if let Some(ref d) = display {
            std::env::set_var("DISPLAY", d);
        } else {
            std::env::remove_var("DISPLAY");
        }

        let launch_opts = LaunchOptions::default_builder()
            .headless(display.is_none())
            .sandbox(false)
            .window_size(Some((1920, 1080)))
            .args(vec![
                std::ffi::OsStr::new("--disable-dev-shm-usage"),
                std::ffi::OsStr::new("--disable-blink-features=AutomationControlled"),
                std::ffi::OsStr::new("--disable-gpu"),
                std::ffi::OsStr::new("--no-first-run"),
                std::ffi::OsStr::new("--no-default-browser-check"),
                std::ffi::OsStr::new("--disable-features=TranslateUI"),
                // Cap V8 heap at 512 MB and keep a single renderer process
                // to bound memory usage in headless mode.
                std::ffi::OsStr::new("--js-flags=--max-old-space-size=512"),
                std::ffi::OsStr::new("--renderer-process-limit=1"),
            ])
            .build()
            .expect("Failed to build Chrome launch options");

        let browser = Browser::new(launch_opts)
            .context("Failed to launch Chrome. Is google-chrome or chromium installed?")?;

        let tab = browser.new_tab().context("Failed to create browser tab")?;

        // Warm up the tab.
        let _ = tab.navigate_to("about:blank");
        let _ = tab.wait_until_navigated();
        if verbose {
            eprintln!("  [cardmarket] tab ready");
        }

        if verbose {
            if display.is_some() {
                eprintln!("  [cardmarket] Chrome launched on Xvfb virtual display");
            } else {
                eprintln!("  [cardmarket] Chrome launched in headless mode (no Xvfb)");
            }
        }

        Ok(BrowserSession {
            _browser: browser,
            tab,
            xvfb: xvfb_child,
        })
    }

    /// Ensure a browser session exists, creating one if necessary.
    /// Returns a reference to the tab.  The caller must be holding the
    /// `session` lock and must keep it locked while using the tab.
    fn ensure_tab(
        session_guard: &mut Option<BrowserSession>,
        verbose: bool,
    ) -> anyhow::Result<Arc<headless_chrome::Tab>> {
        if session_guard.is_none() {
            if verbose {
                eprintln!("  [cardmarket] starting browser...");
            }
            *session_guard = Some(Self::create_session(verbose)?);
        }
        // Unwrap safe: we just ensured it's Some
        Ok(session_guard.as_ref().unwrap().tab.clone())
    }

    /// Check whether an error indicates the browser process has died.
    fn is_browser_dead(err: &anyhow::Error) -> bool {
        let msg = err.to_string().to_lowercase();
        msg.contains("connection is closed")
            || msg.contains("connection closed")
            || msg.contains("no such tab")
            || msg.contains("browser has been closed")
    }
}

/// Launch Xvfb on an unused display.  Returns (child_process, display_string).
fn start_xvfb(verbose: bool) -> (Option<std::process::Child>, Option<String>) {
    use std::process::Command;

    // Kill leftover Xvfb processes from previous runs by checking lock files.
    for n in 90..=99 {
        let lock_path = format!("/tmp/.X{n}-lock");
        if let Ok(lock) = std::fs::read_to_string(&lock_path) {
            if let Some(pid_str) = lock
                .lines()
                .next()
                .and_then(|l| l.strip_prefix("    ").or(Some(l)).map(|s| s.trim()))
            {
                if let Ok(pid) = pid_str.parse::<i32>() {
                    let _ = Command::new("kill")
                        .arg(pid.to_string())
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .status();
                }
            }
            let _ = std::fs::remove_file(&lock_path);
        }
    }
    // Give killed processes a moment to clean up.
    std::thread::sleep(std::time::Duration::from_millis(300));

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
        // Serialize and ensure browser session.
        let mut session_guard = self.session.lock().unwrap();
        let tab = Self::ensure_tab(&mut session_guard, self.verbose)?;
        let slug = title_to_slug(card_name);
        let results = self.fetch_all_in_one_tab(&tab, card_name, &slug)?;
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

        let mut session_guard = self.session.lock().unwrap();

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
        // Cloudflare session carries over.  The browser is created fresh per
        // request batch, so idle death is not a concern; one retry exists as
        // a safety net for rare mid-batch crashes.
        let slug = title_to_slug(card_name);

        for attempt in 0..2 {
            let tab = match Self::ensure_tab(&mut session_guard, self.verbose) {
                Ok(t) => t,
                Err(e) => {
                    let msg = e.to_string();
                    eprintln!("  [{}] Failed to start browser: {}", sub_key, msg);
                    return Err(anyhow::anyhow!("{msg}"));
                }
            };

            match self.fetch_all_in_one_tab(&tab, card_name, &slug) {
                Ok(results) => {
                    let sub = match sub_key {
                        STORE_PREFIX => results.norwegian.clone(),
                        STORE_PREFIX_INT => results.int_powerseller.clone(),
                        STORE_PREFIX_INT_PRIVATE => results.int_private.clone(),
                        _ => anyhow::bail!("Unknown CardMarket sub-store: {}", sub_key),
                    };
                    self.persist_all_sub_results(card_name, &results);
                    self.card_cache
                        .lock()
                        .unwrap()
                        .insert(card_name.to_string(), results);
                    return Ok(sub);
                }
                Err(ref e) if is_cloudflare_error(e) => {
                    record_cloudflare_block();
                    if SEMI_MANUAL.load(Ordering::Relaxed) {
                        return Err(anyhow::Error::new(RescuePending));
                    } else {
                        return Err(anyhow::anyhow!("Cloudflare challenge"));
                    }
                }
                Err(e) if Self::is_browser_dead(&e) && attempt == 0 => {
                    // Browser crashed mid-batch — recreate and retry once.
                    *session_guard = None;
                    self.card_cache.lock().unwrap().clear();
                    continue;
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
                    return Err(anyhow::anyhow!("{detail}"));
                }
            }
        }

        unreachable!()
    }

    fn teardown(&self) {
        let mut guard = self.session.lock().unwrap();
        *guard = None;
        self.card_cache.lock().unwrap().clear();
        reset_block_count();
    }
}

impl CardMarket {
    /// Persist all three sub-endpoints' results to the SQLite cache.
    /// Results are grouped by seller store-name so that `cache.store()`
    /// receives each seller's entries as a single batch (DELETE + INSERT).
    fn persist_all_sub_results(&self, card_name: &str, results: &CardResults) {
        let cache = match self.persistent_cache.as_ref() {
            Some(c) => c,
            None => return,
        };

        // Group entries by store_name so each seller's rows are
        // replaced atomically (cache.store DELETEs the old rows first).
        let mut by_seller: std::collections::HashMap<String, Vec<StoreResult>> =
            std::collections::HashMap::new();

        for result in results
            .norwegian
            .iter()
            .chain(results.int_powerseller.iter())
            .chain(results.int_private.iter())
        {
            by_seller
                .entry(result.store_name.clone())
                .or_default()
                .push(result.clone());
        }

        for (store_name, grouped) in &by_seller {
            // Best-effort — don't let a cache write failure kill the search.
            let _ = cache.store(card_name, store_name, Some(grouped.as_slice()));
        }
    }

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

// headless_chrome uses synchronous tungstenite (no tokio runtime), so
// BrowserSession drop is safe from any thread context.
impl Drop for CardMarket {
    fn drop(&mut self) {
        let mut guard = self.session.lock().unwrap();
        *guard = None;
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

    let heading_text = match document.select(&heading_sel).next() {
        Some(heading) => heading.text().collect::<String>().trim().to_string(),
        None => {
            // A genuine CardMarket card page always carries the card name in
            // an <h1>. If it's absent and we also found no sellers, we almost
            // certainly landed on a Cloudflare/error page rather than a real
            // "no results" page, so bail instead of letting this empty result
            // be negative-cached.
            if entries.is_empty() {
                anyhow::bail!("no <h1> found — page does not look like a CardMarket card page");
            }
            String::new()
        }
    };

    if !heading_text.is_empty() && !title_contains(card_name, &heading_text) {
        anyhow::bail!(
            "h1 mismatch: got '{}', expected '{}'",
            heading_text,
            card_name
        );
    }

    if entries.is_empty() && heading_text.is_empty() {
        anyhow::bail!("empty <h1> — page does not look like a CardMarket card page");
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

/// Convert a CardMarket user‑profile URL into a filtered Singles offer URL
/// that scopes results to a particular card name.
///
/// Input:  `https://www.cardmarket.com/en/Magic/Users/91since`
/// Output: `https://www.cardmarket.com/en/Magic/Users/91since/Offers/Singles?name=Ambitious%20augmenter&sortBy=name_asc`
fn make_cardmarket_offer_url(seller_url: &str, card_name: &str) -> String {
    let encoded = percent_encode_card_name(card_name);
    format!("{seller_url}/Offers/Singles?name={encoded}&sortBy=name_asc")
}

/// Percent-encode a card name for use in a CardMarket query parameter.
fn percent_encode_card_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' || c == '~' {
                c.to_string()
            } else {
                format!("%{:02X}", c as u8)
            }
        })
        .collect()
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
                url: make_cardmarket_offer_url(&e.url, card_name),
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
