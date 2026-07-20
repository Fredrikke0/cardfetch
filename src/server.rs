use crate::cache::{Cache, CacheLookup};
use crate::stores::{self, Store, StoreResult, DELAY_JITTER_MS, DELAY_MS};
use crate::wizard::{self, Strategy, WizardConfig, WizardSolution};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tower_http::cors::CorsLayer;

// ── Request types ────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct FetchRequest {
    #[serde(default)]
    pub stores: Vec<String>,
    pub cards: Vec<String>,
    /// If true, only return cached data — never creates a job, never 503.
    /// The response may be incomplete (some stores not yet fetched).
    #[serde(default)]
    pub cache_only: bool,
    /// Max results per store endpoint per card. 0 = no cap (default).
    /// Applied at response time only — scraping always fetches everything
    /// so the wizard has full data.
    #[serde(default)]
    pub max_per_store: usize,
}

#[derive(Deserialize)]
pub struct WizardRequest {
    pub cards: Vec<String>,
    #[serde(default)]
    pub tolerance: usize,
    #[serde(default)]
    pub eu_destination: bool,
    #[serde(default = "default_strategy")]
    pub strategy: String,
    #[serde(default)]
    pub exhaustive: bool,
}

fn default_strategy() -> String {
    "cheapest".to_string()
}

// ── Response types ───────────────────────────────────────────────────────────

/// Normalize a CardMarket store name: returns (seller_name, category)
/// where category is "n" (Norwegian), "i" (int powerseller), "p" (int private),
/// or None for non-CardMarket stores.
fn split_cm_store(store_name: &str) -> (&str, Option<&'static str>) {
    if let Some(seller) = store_name.strip_prefix("cardmarket-int-private.com: ") {
        (seller, Some("p"))
    } else if let Some(seller) = store_name.strip_prefix("cardmarket-int.com: ") {
        (seller, Some("i"))
    } else if let Some(seller) = store_name.strip_prefix("cardmarket.com: ") {
        (seller, Some("n"))
    } else {
        (store_name, None)
    }
}

#[derive(Serialize, Clone)]
pub struct CardResultEntry {
    /// Store identifier -- seller name for CardMarket, domain for storefronts.
    #[serde(rename = "s")]
    pub store: String,
    /// Price in integer oere.
    #[serde(rename = "p")]
    pub price: u32,
    /// Full URL to the listing.
    #[serde(rename = "u")]
    pub url: String,
    /// CardMarket category: "n" (Norwegian), "i" (int powerseller), "p" (int private).
    /// Absent for non-CardMarket stores.
    #[serde(rename = "c", skip_serializing_if = "Option::is_none")]
    pub category: Option<&'static str>,
}

impl From<StoreResult> for CardResultEntry {
    fn from(r: StoreResult) -> Self {
        let (store, category) = split_cm_store(&r.store_name);
        Self {
            store: store.to_string(),
            price: r.price,
            url: r.url,
            category,
        }
    }
}

#[derive(Serialize, Clone)]
pub struct FetchResultData {
    #[serde(rename = "r")]
    pub results: HashMap<String, Vec<CardResultEntry>>,
    #[serde(rename = "u")]
    pub unrecognized: Vec<String>,
}

#[derive(Serialize, Clone)]
pub struct WizardResultData {
    #[serde(rename = "r")]
    pub results: Vec<WizardResponseData>,
    #[serde(rename = "u")]
    pub unrecognized: Vec<String>,
}

#[derive(Serialize, Clone)]
#[serde(untagged)]
pub enum JobResult {
    Fetch(FetchResultData),
    Wizard(WizardResultData),
}

#[derive(Serialize, Clone)]
pub struct WizardResponseData {
    #[serde(rename = "a")]
    pub assignments: Vec<WizardCardAssignment>,
    #[serde(rename = "sk")]
    pub skipped: Vec<String>,
    #[serde(rename = "st")]
    pub stores: Vec<WizardStoreSummary>,
    #[serde(rename = "tc")]
    pub total_card_cost: u64,
    #[serde(rename = "ts")]
    pub total_shipping: u64,
    #[serde(rename = "ns")]
    pub num_stores: usize,
}

#[derive(Serialize, Clone)]
pub struct WizardCardAssignment {
    #[serde(rename = "c")]
    pub card: String,
    #[serde(rename = "s", skip_serializing_if = "Option::is_none")]
    pub store: Option<String>,
    #[serde(rename = "p", skip_serializing_if = "Option::is_none")]
    pub price: Option<u32>,
    #[serde(rename = "u", skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(rename = "t", skip_serializing_if = "Option::is_none")]
    pub category: Option<&'static str>,
}

#[derive(Serialize, Clone)]
pub struct WizardStoreSummary {
    #[serde(rename = "n")]
    pub name: String,
    #[serde(rename = "ct")]
    pub card_total: u32,
    #[serde(rename = "sh")]
    pub shipping: u32,
    #[serde(rename = "c", skip_serializing_if = "Option::is_none")]
    pub category: Option<&'static str>,
}

impl WizardResponseData {
    fn from_solution(sol: &WizardSolution) -> Self {
        WizardResponseData {
            assignments: sol
                .assignments
                .iter()
                .map(|(card, a)| {
                    let (store, category) = match a {
                        Some((name, _price, _url)) => {
                            let (s, c) = split_cm_store(name);
                            (Some(s.to_string()), c)
                        }
                        None => (None, None),
                    };
                    WizardCardAssignment {
                        card: card.clone(),
                        store,
                        price: a.as_ref().map(|s| s.1),
                        url: a.as_ref().map(|s| s.2.clone()),
                        category,
                    }
                })
                .collect(),
            skipped: sol.skipped.clone(),
            stores: sol
                .store_names
                .iter()
                .enumerate()
                .map(|(i, name)| {
                    let (store_name, category) = split_cm_store(name);
                    WizardStoreSummary {
                        name: store_name.to_string(),
                        card_total: sol.card_subtotals[i],
                        shipping: sol.shipping_costs[i],
                        category,
                    }
                })
                .collect(),
            total_card_cost: sol.total_card_cost,
            total_shipping: sol.total_shipping,
            num_stores: sol.num_stores,
        }
    }

    fn from_solutions(solutions: &[WizardSolution]) -> Vec<Self> {
        solutions.iter().map(Self::from_solution).collect()
    }
}

// ── Job system ───────────────────────────────────────────────────────────────

/// Maximum number of concurrent pending+running jobs.
/// Set to 1 to respect per-store rate limiting.
const MAX_ACTIVE_JOBS: usize = 1;

#[derive(Clone, Copy, PartialEq)]
enum JobKind {
    Fetch,
    Wizard,
}

struct Job {
    kind: JobKind,
    status: String,
    created_at: std::time::Instant,
    /// Fetch: card×store pairs done (no longer used for progress; see `progress_done`).
    cards_done: usize,
    /// Fetch: total card×store pairs.
    cards_total: usize,
    /// Fetch: current store being queried (no longer used; see `progress_current`).
    current_store: String,
    /// Fetch: current card being searched.
    current_card: String,
    /// Fetch: live progress counter, updated by each store thread per card.
    progress_done: Option<Arc<std::sync::atomic::AtomicUsize>>,
    /// Fetch: live (store_name, card_name), updated by each store thread.
    progress_current: Option<Arc<Mutex<(String, String)>>>,
    /// Wizard: tolerance level currently being optimized.
    tolerance_done: usize,
    /// Wizard: total tolerance levels.
    tolerance_total: usize,
    /// Wizard (exhaustive): total store subsets to evaluate.
    combos_total: u64,
    result: Option<JobResult>,
    error: Option<String>,
}

#[derive(Serialize)]
struct JobResponse {
    status: String,
    kind: String,
    cards_done: usize,
    cards_total: usize,
    current_store: String,
    current_card: String,
    tolerance_done: usize,
    tolerance_total: usize,
    combos_done: u64,
    combos_total: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<JobResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl Job {
    fn to_response(&self, combos_done: u64) -> JobResponse {
        let kind = match self.kind {
            JobKind::Fetch => "fetch",
            JobKind::Wizard => "wizard",
        };
        let (cards_done, current_store, current_card) = if kind == "fetch" {
            let done = self
                .progress_done
                .as_ref()
                .map(|a| a.load(std::sync::atomic::Ordering::Relaxed))
                .unwrap_or(self.cards_done);
            let (store, card) = self
                .progress_current
                .as_ref()
                .map(|a| a.lock().unwrap().clone())
                .unwrap_or_else(|| (self.current_store.clone(), self.current_card.clone()));
            (done, store, card)
        } else {
            (0, String::new(), String::new())
        };
        JobResponse {
            status: self.status.clone(),
            kind: kind.into(),
            cards_done,
            cards_total: self.cards_total,
            current_store,
            current_card,
            tolerance_done: self.tolerance_done,
            tolerance_total: self.tolerance_total,
            combos_done,
            combos_total: self.combos_total,
            result: self.result.clone(),
            error: self.error.clone(),
        }
    }
}

type JobMap = Arc<Mutex<HashMap<String, Arc<Mutex<Job>>>>>;

/// Generate a short random job ID.
fn new_job_id() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    (0..8)
        .map(|_| rng.sample(rand::distributions::Alphanumeric) as char)
        .collect()
}

// ── App state ────────────────────────────────────────────────────────────────

pub struct AppState {
    pub stores: Arc<Vec<Box<dyn Store>>>,
    pub cache: Option<Arc<Cache>>,
    jobs: JobMap,
}

impl AppState {
    pub fn new(stores: Arc<Vec<Box<dyn Store>>>, cache: Option<Arc<Cache>>) -> Self {
        AppState {
            stores,
            cache,
            jobs: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Try to reserve a job slot for a given kind.
    /// Cleans up completed/failed jobs older than 30 minutes.
    /// Returns Err with 503 and the blocking job IDs if too many jobs of the same
    /// kind are already active.
    fn reserve_slot(&self, kind: JobKind) -> Result<(), (StatusCode, String, Vec<String>)> {
        let mut jobs = self.jobs.lock().unwrap();
        let now = std::time::Instant::now();
        let ttl = Duration::from_secs(1800); // 30 minutes

        // Remove expired completed/failed jobs
        jobs.retain(|_, j| {
            let j = j.lock().unwrap();
            !(j.status == "done" || j.status == "failed") || now.duration_since(j.created_at) < ttl
        });

        // Fast path: count without allocating (same as before).
        let active = jobs
            .values()
            .filter(|j| {
                let j = j.lock().unwrap();
                (j.status == "pending" || j.status == "running") && j.kind == kind
            })
            .count();

        let kind_name = match kind {
            JobKind::Fetch => "fetch",
            JobKind::Wizard => "wizard",
        };

        if active >= MAX_ACTIVE_JOBS {
            // Only collect IDs on the (rare) error path.
            let active_ids: Vec<String> = jobs
                .iter()
                .filter(|(_, j)| {
                    let j = j.lock().unwrap();
                    (j.status == "pending" || j.status == "running") && j.kind == kind
                })
                .map(|(id, _)| id.clone())
                .collect();
            Err((
                StatusCode::SERVICE_UNAVAILABLE,
                format!(
                    "Server busy: {} {kind_name} job(s) already running. Try again in a moment.",
                    active
                ),
                active_ids,
            ))
        } else {
            Ok(())
        }
    }
}

// ── Router ───────────────────────────────────────────────────────────────────

pub fn build_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/stores", get(get_stores))
        .route("/fetch", post(start_fetch))
        .route("/wizard", post(start_wizard))
        .route("/jobs/{id}", get(get_job))
        .layer(CorsLayer::permissive())
        .with_state(state)
}

// ── Handlers ─────────────────────────────────────────────────────────────────

async fn get_stores(State(state): State<Arc<AppState>>) -> Json<Vec<String>> {
    let names: Vec<String> = state.stores.iter().map(|s| s.name().to_string()).collect();
    Json(names)
}

/// Build the list of all (card_name, store_key) pairs that need checking.
fn all_cache_pairs(
    cards: &[String],
    all_stores: &[Box<dyn Store>],
    store_indices: &[usize],
) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    for &si in store_indices {
        let store = &all_stores[si];
        for key in &store.cache_keys() {
            for card_name in cards {
                pairs.push((card_name.clone(), key.clone()));
            }
        }
    }
    pairs
}

/// Extract the cache key from a store name for per-store capping.
/// CardMarket sub-stores collapse to their base prefix while
/// other stores use the full name.
fn store_cache_key(store_name: &str) -> &str {
    if store_name.starts_with("cardmarket-int-private.com") {
        "cardmarket-int-private.com"
    } else if store_name.starts_with("cardmarket-int.com") {
        "cardmarket-int.com"
    } else if store_name.starts_with("cardmarket.com") {
        "cardmarket.com"
    } else {
        store_name
    }
}

/// Always returns cached results, skipping any entries that aren't cached.
/// Used by `cache_only` mode — never returns None.
fn serve_cache_partial(
    cache: &Cache,
    cards: &[String],
    all_stores: &[Box<dyn Store>],
    store_indices: &[usize],
    max_per_store: usize,
) -> std::collections::HashMap<String, Vec<CardResultEntry>> {
    let pairs = all_cache_pairs(cards, all_stores, store_indices);
    let batch = match cache.lookup_batch(&pairs) {
        Ok(b) => b,
        Err(_) => return std::collections::HashMap::new(),
    };

    // Group by (card, cache_key) so we can cap per store endpoint
    let mut per_key: std::collections::HashMap<(String, String), Vec<CardResultEntry>> =
        std::collections::HashMap::new();
    for ((card_name, key), lookup) in batch {
        if let CacheLookup::Hit(results) = lookup {
            let entries: Vec<CardResultEntry> = results.into_iter().map(|r| r.into()).collect();
            per_key
                .entry((card_name.clone(), key))
                .or_default()
                .extend(entries);
        }
    }

    // Apply per-store cap (0 = no cap)
    if max_per_store > 0 {
        for entries in per_key.values_mut() {
            entries.sort_by_key(|e| e.price);
            entries.truncate(max_per_store);
        }
    }

    // Flatten to card-name keyed map
    let mut grouped: std::collections::HashMap<String, Vec<CardResultEntry>> =
        std::collections::HashMap::new();
    for ((card_name, _key), entries) in per_key {
        grouped.entry(card_name).or_default().extend(entries);
    }

    grouped
}

/// Check whether every card×store pair is in the cache.  If so, return
/// the grouped results.  If any entry is missing, return None.
fn try_serve_from_cache(
    cache: &Cache,
    cards: &[String],
    all_stores: &[Box<dyn Store>],
    store_indices: &[usize],
    max_per_store: usize,
) -> Option<std::collections::HashMap<String, Vec<CardResultEntry>>> {
    let pairs = all_cache_pairs(cards, all_stores, store_indices);
    let batch = cache.lookup_batch(&pairs).ok()?;

    // Check: every pair must be cached (Hit or Skip)
    for (_pair, lookup) in &batch {
        if matches!(lookup, CacheLookup::Search) {
            return None;
        }
    }

    // Group by (card, cache_key) so we can cap per store endpoint
    let mut per_key: std::collections::HashMap<(String, String), Vec<CardResultEntry>> =
        std::collections::HashMap::new();
    for ((card_name, key), lookup) in batch {
        if let CacheLookup::Hit(results) = lookup {
            let entries: Vec<CardResultEntry> = results.into_iter().map(|r| r.into()).collect();
            per_key.entry((card_name, key)).or_default().extend(entries);
        }
    }

    // Apply per-store cap (0 = no cap)
    if max_per_store > 0 {
        for entries in per_key.values_mut() {
            entries.sort_by_key(|e| e.price);
            entries.truncate(max_per_store);
        }
    }

    // Flatten to card-name keyed map
    let mut grouped: std::collections::HashMap<String, Vec<CardResultEntry>> =
        std::collections::HashMap::new();
    for ((card_name, _key), entries) in per_key {
        grouped.entry(card_name).or_default().extend(entries);
    }

    Some(grouped)
}

/// POST /fetch — create a background fetch job, return the job ID immediately.
/// If all requested cards are already in cache, returns results directly
/// (even while another job is running).
async fn start_fetch(
    State(state): State<Arc<AppState>>,
    Json(req): Json<FetchRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let mut cards = req.cards;
    cards.sort();
    cards.dedup();

    if cards.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "No cards provided.".into()));
    }
    if cards.len() > 100 {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("Too many cards: {}. Max is 100.", cards.len()),
        ));
    }

    // Resolve partial/ambiguous card names via Scryfall autocomplete.
    let unrecognized: Vec<String>;
    {
        let client = reqwest::blocking::Client::builder()
            .user_agent("CardFetch/0.1")
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let cache_ref = state.cache.as_ref().map(|c| c.as_ref());
        let (resolved, unres) = crate::scryfall::resolve_with_cache(&client, &cards, &cache_ref)
            .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
        cards = resolved;
        unrecognized = unres;
    }

    // Determine which store indices are active.
    let stores: Vec<usize> = state
        .stores
        .iter()
        .enumerate()
        .filter(|(_, s)| {
            if req.stores.is_empty() {
                return true;
            }
            let name = s.name().to_lowercase();
            req.stores
                .iter()
                .any(|f| name.contains(&f.trim().to_lowercase()))
        })
        .map(|(i, _)| i)
        .collect();

    // cache_only: always return whatever is cached, never 503.
    if req.cache_only {
        let results = state
            .cache
            .as_ref()
            .map(|c| serve_cache_partial(c, &cards, &state.stores, &stores, req.max_per_store))
            .unwrap_or_default();
        return Ok(Json(serde_json::json!({
            "r": results,
            "u": unrecognized,
        })));
    }

    // If we have a cache, check whether everything is already cached.
    // If so, return results immediately — no job needed.
    if let Some(ref cache) = state.cache {
        if let Some(results) =
            try_serve_from_cache(cache, &cards, &state.stores, &stores, req.max_per_store)
        {
            return Ok(Json(serde_json::json!({
                "r": results,
                "u": unrecognized,
            })));
        }
    }

    let cards_total = cards.len() * stores.len();
    let cards_done = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let current = Arc::new(Mutex::new((String::new(), String::new())));

    state
        .reserve_slot(JobKind::Fetch)
        .map_err(|(code, msg, ids)| {
            let body = serde_json::json!({
                "error": msg,
                "existing_job_id": ids.first().map(|s| s.as_str()).unwrap_or(""),
            })
            .to_string();
            (code, body)
        })?;

    let job_id = new_job_id();

    let job = Arc::new(Mutex::new(Job {
        kind: JobKind::Fetch,
        status: "pending".into(),
        created_at: std::time::Instant::now(),
        cards_done: 0,
        cards_total,
        current_store: String::new(),
        current_card: String::new(),
        progress_done: Some(cards_done.clone()),
        progress_current: Some(current.clone()),
        tolerance_done: 0,
        tolerance_total: 0,
        combos_total: 0,
        result: None,
        error: None,
    }));

    state
        .jobs
        .lock()
        .unwrap()
        .insert(job_id.clone(), job.clone());

    // Spawn background work — one thread per store, results via mpsc.
    let job_ref = job.clone();
    let cache = state.cache.clone();
    let stores_arc = state.stores.clone();
    let stores_indices = stores;
    let cards_for_thread = cards.clone();
    let cards_done = cards_done.clone();
    let current = current.clone();
    let unrecognized_for_job = unrecognized.clone();
    let max_per_store = req.max_per_store;

    std::thread::spawn(move || {
        {
            let mut j = job_ref.lock().unwrap();
            j.status = "running".into();
        }

        let result = run_search_parallel(
            &cards_for_thread,
            stores_arc.clone(),
            &stores_indices,
            cache.clone(),
            cards_done,
            current,
        );

        // Group results by (card, cache_key) so we can cap per store endpoint
        let mut per_key: HashMap<(String, String), Vec<CardResultEntry>> = HashMap::new();
        for r in result {
            let key = store_cache_key(&r.store_name);
            per_key
                .entry((r.card_name.clone(), key.to_string()))
                .or_default()
                .push(r.into());
        }

        // Apply per-store cap (0 = no cap)
        if max_per_store > 0 {
            for entries in per_key.values_mut() {
                entries.sort_by_key(|e| e.price);
                entries.truncate(max_per_store);
            }
        }

        // Flatten to card-name keyed map
        let mut grouped: HashMap<String, Vec<CardResultEntry>> = HashMap::new();
        for ((card_name, _key), entries) in per_key {
            grouped.entry(card_name).or_default().extend(entries);
        }

        let mut j = job_ref.lock().unwrap();
        j.status = "done".into();
        j.cards_done = cards_total;
        j.result = Some(JobResult::Fetch(FetchResultData {
            results: grouped,
            unrecognized: unrecognized_for_job,
        }));
    });

    Ok(Json(serde_json::json!({ "job_id": job_id })))
}

/// POST /wizard — create a background wizard job, return the job ID immediately.
/// If a suitable cached solution exists, returns results directly without creating a job.
async fn start_wizard(
    State(state): State<Arc<AppState>>,
    Json(req): Json<WizardRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let mut cards = req.cards;
    cards.sort();
    cards.dedup();

    if cards.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "No cards provided.".into()));
    }
    if cards.len() > 100 {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("Too many cards: {}. Max is 100.", cards.len()),
        ));
    }

    if req.tolerance > 5 {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("Tolerance too high: {}. Max is 5.", req.tolerance),
        ));
    }

    // Resolve card names via Scryfall so they match the /fetch cache keys.
    let unrecognized: Vec<String>;
    {
        let client = reqwest::blocking::Client::builder()
            .user_agent("CardFetch/0.1")
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let cache_ref = state.cache.as_ref().map(|c| c.as_ref());
        let (resolved, unres) = crate::scryfall::resolve_with_cache(&client, &cards, &cache_ref)
            .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
        cards = resolved;
        unrecognized = unres;
    }

    let strategy = match req.strategy.as_str() {
        "simplest" => Strategy::Simplest,
        "cheapest" => Strategy::Cheapest,
        other => {
            return Err((
                StatusCode::BAD_REQUEST,
                format!(
                    "Unknown strategy '{}'. Use 'cheapest' or 'simplest'.",
                    other
                ),
            ))
        }
    };

    // Quick check: do we have cache entries?
    let cache = state.cache.as_ref().ok_or_else(|| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Cache is not available (server started with --no-cache?).".into(),
        )
    })?;

    // Prune wizard solutions computed on stale listing data before
    // checking the cache — this prevents solutions from a different
    // card set from being reused.
    let _ = cache.prune_stale_solutions();

    let listings = cache.get_listings(&cards).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to read cache: {}", e),
        )
    })?;

    if listings.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "No cached listings found. Run a /fetch first.".into(),
        ));
    }

    let strategy_name = match strategy {
        Strategy::Simplest => "simplest",
        Strategy::Cheapest => "cheapest",
    };

    // Check if we have suitable cached solutions for the exact parameters.
    // Only skip computation if the cache is exhaustive, or if the current
    // request is also non-exhaustive (heuristic).
    let cached = cache
        .get_cached_solutions(strategy_name, req.tolerance, req.eu_destination)
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Cache error: {}", e),
            )
        })?;

    // Only use cached solutions if they were computed for the same number
    // of cards as the current request, otherwise the choices vector will
    // be silently truncated/padded by solution_from_choices.
    let card_count_match = cached
        .first()
        .is_some_and(|(h, _)| h.raw_choices.len() == cards.len());
    if !cached.is_empty() && card_count_match {
        let can_use_cache =
            cached.iter().any(|(_, was_exhaustive)| *was_exhaustive) || !req.exhaustive;
        if can_use_cache {
            let input =
                wizard::WizardInput::from_results_and_wants(listings, &cards, req.eu_destination);
            let config = WizardConfig {
                strategy,
                tolerance: req.tolerance,
            };
            let solutions: Vec<_> = cached
                .iter()
                .map(|(history, _)| {
                    wizard::solution_from_choices(&history.raw_choices, &input, &config)
                })
                .collect();
            return Ok(Json(serde_json::json!({
                "r": WizardResponseData::from_solutions(&solutions),
                "u": unrecognized,
            })));
        }
        // Cache was non-exhaustive but request is exhaustive — fall through to compute.
    }

    let tolerance_total = req.tolerance + 1;

    // Pre-compute exhaustive combo count if needed.
    let input = wizard::WizardInput::from_results_and_wants(listings, &cards, req.eu_destination);
    let combos_total = if req.exhaustive {
        let candidates = wizard::select_candidate_stores(&input);
        wizard::exhaustive_combo_total(&candidates)
    } else {
        0
    };

    state
        .reserve_slot(JobKind::Wizard)
        .map_err(|(code, msg, ids)| {
            let body = serde_json::json!({
                "error": msg,
                "existing_job_id": ids.first().map(|s| s.as_str()).unwrap_or(""),
            })
            .to_string();
            (code, body)
        })?;

    let job_id = new_job_id();

    let job = Arc::new(Mutex::new(Job {
        kind: JobKind::Wizard,
        status: "pending".into(),
        created_at: std::time::Instant::now(),
        cards_done: 0,
        cards_total: 0,
        current_store: String::new(),
        current_card: String::new(),
        progress_done: None,
        progress_current: None,
        tolerance_done: 0,
        tolerance_total,
        combos_total,
        result: None,
        error: None,
    }));

    state
        .jobs
        .lock()
        .unwrap()
        .insert(job_id.clone(), job.clone());

    let job_ref = job.clone();
    let cache_clone = cache.clone();
    let exhaustive = req.exhaustive;
    let tolerance = req.tolerance;
    let eu = req.eu_destination;
    let unrecognized_for_job = unrecognized.clone();

    std::thread::spawn(move || {
        {
            let mut j = job_ref.lock().unwrap();
            j.status = "running".into();
        }

        let exhaustive_candidates = if exhaustive {
            let c = wizard::select_candidate_stores(&input);
            // Reset the global combo counter before the exhaustive run.
            crate::wizard::EXHAUSTIVE_COMBO_COUNT.store(0, std::sync::atomic::Ordering::Relaxed);
            Some(c)
        } else {
            None
        };

        let mut search_mode: wizard::SearchMode = wizard::SearchMode::Heuristic { seed: None };
        let mut solutions: Vec<(usize, WizardSolution)> = Vec::new();

        for t in 0..=tolerance {
            if let Some(ref candidates) = exhaustive_candidates {
                // Reset counter at the start of each tolerance level.
                crate::wizard::EXHAUSTIVE_COMBO_COUNT
                    .store(0, std::sync::atomic::Ordering::Relaxed);
                search_mode = wizard::SearchMode::Exhaustive {
                    candidates: candidates.clone(),
                };
            }
            let config = WizardConfig {
                strategy,
                tolerance: t,
            };
            let results = wizard::optimize_input(&input, &config, &search_mode);
            if !results.is_empty() {
                let best = &results[0];
                if exhaustive_candidates.is_none() {
                    search_mode = wizard::SearchMode::Heuristic {
                        seed: Some(best.raw_choices.clone()),
                    };
                }
                for sol in results {
                    solutions.push((t, sol));
                }
            }

            {
                let mut j = job_ref.lock().unwrap();
                j.tolerance_done = t + 1;
            }
        }

        let mut j = job_ref.lock().unwrap();

        if solutions.is_empty() {
            j.status = "failed".into();
            j.error = Some("No valid solutions found.".into());
            return;
        }

        // Save all solutions to cache with rank.
        let mut tol_counter: std::collections::HashMap<usize, usize> =
            std::collections::HashMap::new();
        for (t, sol) in &solutions {
            let rank = tol_counter.entry(*t).or_insert(0);
            *rank += 1;
            let _ = cache_clone.save_wizard_solution(strategy_name, *t, eu, exhaustive, *rank, sol);
        }

        // Return all solutions for the max tolerance, best first.
        let max_tol_solutions: Vec<WizardSolution> = solutions
            .iter()
            .filter(|(t, _)| *t == tolerance)
            .map(|(_, s)| s.clone())
            .collect();

        j.status = "done".into();
        j.tolerance_done = tolerance_total;
        j.result = Some(JobResult::Wizard(WizardResultData {
            results: WizardResponseData::from_solutions(&max_tol_solutions),
            unrecognized: unrecognized_for_job,
        }));
    });

    Ok(Json(serde_json::json!({ "job_id": job_id })))
}

/// GET /jobs/:id — poll for job status / progress / result.
async fn get_job(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<JobResponse>, (StatusCode, String)> {
    let combos_done =
        crate::wizard::EXHAUSTIVE_COMBO_COUNT.load(std::sync::atomic::Ordering::Relaxed);
    let jobs = state.jobs.lock().unwrap();
    let job = jobs
        .get(&id)
        .ok_or_else(|| (StatusCode::NOT_FOUND, format!("Job '{}' not found.", id)))?;
    let response = job.lock().unwrap().to_response(combos_done);
    Ok(Json(response))
}

// ── Core search logic (parallel per store, like the CLI) ───────────────────

/// Run the search across stores in parallel (one thread per store, same as
/// the CLI).  Increments `cards_done` (atomic) after each card.  Updates
/// `current` (store_name, card_name) under a mutex for the progress display.
fn run_search_parallel(
    cards: &[String],
    all_stores: Arc<Vec<Box<dyn Store>>>,
    store_indices: &[usize],
    cache: Option<Arc<Cache>>,
    cards_done: Arc<std::sync::atomic::AtomicUsize>,
    current: Arc<Mutex<(String, String)>>,
) -> Vec<StoreResult> {
    let cards_arc = Arc::new(cards.to_vec());
    let (tx, rx) = std::sync::mpsc::channel();
    let mut handles = Vec::new();

    for &si in store_indices {
        let tx = tx.clone();
        let cards = cards_arc.clone();
        let stores = all_stores.clone();
        let cache = cache.clone();
        let cards_done = cards_done.clone();
        let current = current.clone();

        let handle = std::thread::spawn(move || {
            let store = &stores[si];
            let store_name = store.name().to_string();
            let timeout = Duration::from_secs(store.timeout_secs());

            let client = reqwest::blocking::Client::builder()
                .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36")
                .timeout(timeout)
                .build()
                .expect("Failed to build per-store HTTP client");

            for card_name in cards.iter() {
                {
                    let mut cur = current.lock().unwrap();
                    cur.0 = store_name.clone();
                    cur.1 = card_name.clone();
                }

                let cache_keys = store.cache_keys();
                let mut results_for_card: Vec<StoreResult> = Vec::new();
                let mut keys_to_fetch: Vec<String> = Vec::new();

                for key in &cache_keys {
                    let lookup = cache.as_ref().map(|c| c.lookup(card_name, key)).transpose();
                    match lookup {
                        Ok(Some(CacheLookup::Hit(hit_results))) => {
                            results_for_card.extend(hit_results);
                        }
                        Ok(Some(CacheLookup::Skip)) => {}
                        _ => {
                            keys_to_fetch.push(key.clone());
                        }
                    }
                }

                if !keys_to_fetch.is_empty() {
                    std::thread::sleep(Duration::from_millis(
                        DELAY_MS + rand::random::<u64>() % DELAY_JITTER_MS,
                    ));

                    for key in &keys_to_fetch {
                        match store.search_sub(&client, card_name, key) {
                            Ok(sub_results) => {
                                if sub_results.is_empty() {
                                    let gave_up = key.starts_with("cardmarket")
                                        && stores::cardmarket::is_blocked();
                                    if !gave_up {
                                        if let Some(ref c) = cache {
                                            let _ = c.store(card_name, key, None);
                                        }
                                    }
                                } else {
                                    if let Some(ref c) = cache {
                                        let mut by_store: HashMap<String, Vec<StoreResult>> =
                                            HashMap::new();
                                        for result in &sub_results {
                                            by_store
                                                .entry(result.store_name.clone())
                                                .or_default()
                                                .push(result.clone());
                                        }
                                        for (sn, grouped) in &by_store {
                                            let _ =
                                                c.store(card_name, sn, Some(grouped.as_slice()));
                                        }
                                    }
                                    for result in sub_results {
                                        results_for_card.push(result);
                                    }
                                }
                            }
                            Err(e) => {
                                if e.downcast_ref::<stores::cardmarket::RescuePending>()
                                    .is_some()
                                {
                                    eprintln!("  [{}] CardMarket blocked for '{}'", key, card_name);
                                } else {
                                    let msg = e.to_string();
                                    if msg.contains("connection is closed")
                                        || msg.contains("connection closed")
                                    {
                                        eprintln!(
                                            "  [{}] CardMarket browser died — restart server. '{}'",
                                            key, card_name
                                        );
                                    } else {
                                        eprintln!("  [{}] Failed: '{}': {}", key, card_name, msg);
                                    }
                                }
                            }
                        }
                    }
                }

                for result in results_for_card {
                    if tx.send(result).is_err() {
                        break;
                    }
                }

                cards_done.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        });

        handles.push(handle);
    }

    // Wait for all store threads to finish.
    for handle in handles {
        let _ = handle.join();
    }

    drop(tx);
    rx.into_iter().collect()
}
