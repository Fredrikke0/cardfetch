use crate::cache::{Cache, CacheLookup};
use crate::stores::{self, Store, StoreResult, DELAY_JITTER_MS, DELAY_MS};
use crate::wizard::{self, Strategy, WizardConfig, WizardSolution};
use axum::extract::{Path, State};
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

#[derive(Serialize, Clone)]
pub struct CardResultEntry {
    pub store: String,
    pub price: u32,
    pub url: String,
}

impl From<StoreResult> for CardResultEntry {
    fn from(r: StoreResult) -> Self {
        Self {
            store: r.store_name,
            price: r.price,
            url: r.url,
        }
    }
}

#[derive(Serialize, Clone)]
#[serde(untagged)]
pub enum JobResult {
    Fetch(HashMap<String, Vec<CardResultEntry>>),
    Wizard(WizardResponseData),
}

#[derive(Serialize, Clone)]
pub struct WizardResponseData {
    pub assignments: Vec<WizardCardAssignment>,
    pub skipped: Vec<String>,
    pub stores: Vec<WizardStoreSummary>,
    pub total_card_cost: u64,
    pub total_shipping: u64,
    pub num_stores: usize,
}

#[derive(Serialize, Clone)]
pub struct WizardCardAssignment {
    pub card: String,
    pub store: Option<String>,
    pub price: Option<u32>,
    pub url: Option<String>,
}

#[derive(Serialize, Clone)]
pub struct WizardStoreSummary {
    pub name: String,
    pub card_total: u32,
    pub shipping: u32,
}

impl WizardResponseData {
    fn from_solution(sol: &WizardSolution) -> Self {
        WizardResponseData {
            assignments: sol
                .assignments
                .iter()
                .map(|(card, a)| WizardCardAssignment {
                    card: card.clone(),
                    store: a.as_ref().map(|s| s.0.clone()),
                    price: a.as_ref().map(|s| s.1),
                    url: a.as_ref().map(|s| s.2.clone()),
                })
                .collect(),
            skipped: sol.skipped.clone(),
            stores: sol
                .store_names
                .iter()
                .enumerate()
                .map(|(i, name)| WizardStoreSummary {
                    name: name.clone(),
                    card_total: sol.card_subtotals[i],
                    shipping: sol.shipping_costs[i],
                })
                .collect(),
            total_card_cost: sol.total_card_cost,
            total_shipping: sol.total_shipping,
            num_stores: sol.num_stores,
        }
    }
}

// ── Job system ───────────────────────────────────────────────────────────────

/// Maximum number of concurrent pending+running jobs.
const MAX_ACTIVE_JOBS: usize = 5;

/// A job tracks a long-running fetch or wizard operation.
struct Job {
    status: String, // "pending" | "running" | "done" | "failed"
    created_at: std::time::Instant,
    cards_done: usize,
    cards_total: usize,
    current_store: String,
    current_card: String,
    tolerance_done: usize,
    tolerance_total: usize,
    result: Option<JobResult>,
    error: Option<String>,
}

#[derive(Serialize)]
struct JobResponse {
    status: String,
    cards_done: usize,
    cards_total: usize,
    current_store: String,
    current_card: String,
    tolerance_done: usize,
    tolerance_total: usize,
    /// Present only when done.
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<JobResult>,
    /// Present only when failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl Job {
    fn to_response(&self) -> JobResponse {
        JobResponse {
            status: self.status.clone(),
            cards_done: self.cards_done,
            cards_total: self.cards_total,
            current_store: self.current_store.clone(),
            current_card: self.current_card.clone(),
            tolerance_done: self.tolerance_done,
            tolerance_total: self.tolerance_total,
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

    /// Try to reserve a job slot.  Cleans up completed/failed jobs older than
    /// 30 minutes.  Returns Err if too many jobs are already active.
    fn reserve_slot(&self) -> Result<(), String> {
        let mut jobs = self.jobs.lock().unwrap();
        let now = std::time::Instant::now();
        let ttl = Duration::from_secs(1800); // 30 minutes

        // Remove expired completed/failed jobs
        jobs.retain(|_, j| {
            let j = j.lock().unwrap();
            !(j.status == "done" || j.status == "failed") || now.duration_since(j.created_at) < ttl
        });

        let active = jobs
            .values()
            .filter(|j| {
                let s = &j.lock().unwrap().status;
                s == "pending" || s == "running"
            })
            .count();

        if active >= MAX_ACTIVE_JOBS {
            Err(format!(
                "Server busy: {} job(s) already running. Try again in a moment.",
                active
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

/// POST /fetch — create a background fetch job, return the job ID immediately.
async fn start_fetch(
    State(state): State<Arc<AppState>>,
    Json(req): Json<FetchRequest>,
) -> Result<Json<serde_json::Value>, String> {
    let mut cards = req.cards;
    cards.sort();
    cards.dedup();

    if cards.is_empty() {
        return Err("No cards provided.".into());
    }
    if cards.len() > 100 {
        return Err(format!("Too many cards: {}. Max is 100.", cards.len()));
    }

    // Build the filtered store list early so we can report total work.
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

    let cards_total = cards.len() * stores.len();

    state.reserve_slot()?;

    let job_id = new_job_id();

    let job = Arc::new(Mutex::new(Job {
        status: "pending".into(),
        created_at: std::time::Instant::now(),
        cards_done: 0,
        cards_total,
        current_store: String::new(),
        current_card: String::new(),
        tolerance_done: 0,
        tolerance_total: 0,
        result: None,
        error: None,
    }));

    state
        .jobs
        .lock()
        .unwrap()
        .insert(job_id.clone(), job.clone());

    // Spawn background work
    let job_ref = job.clone();
    let cache = state.cache.clone();
    let stores_arc = state.stores.clone();
    let stores_indices = stores;
    let cards_for_thread = cards.clone();

    std::thread::spawn(move || {
        {
            let mut j = job_ref.lock().unwrap();
            j.status = "running".into();
        }

        let result = run_search_with_progress(
            &cards_for_thread,
            &stores_arc,
            &stores_indices,
            cache.as_deref(),
            |card_idx, store_idx, store_name, card_name| {
                let mut j = job_ref.lock().unwrap();
                j.cards_done = card_idx + store_idx * cards_for_thread.len();
                j.current_store = store_name.into();
                j.current_card = card_name.into();
            },
        );

        // Group results by card
        let mut grouped: HashMap<String, Vec<CardResultEntry>> = HashMap::new();
        for r in result {
            grouped
                .entry(r.card_name.clone())
                .or_default()
                .push(r.into());
        }

        let mut j = job_ref.lock().unwrap();
        j.status = "done".into();
        j.cards_done = cards_total;
        j.result = Some(JobResult::Fetch(grouped));
    });

    Ok(Json(serde_json::json!({ "job_id": job_id })))
}

/// POST /wizard — create a background wizard job, return the job ID immediately.
async fn start_wizard(
    State(state): State<Arc<AppState>>,
    Json(req): Json<WizardRequest>,
) -> Result<Json<serde_json::Value>, String> {
    let cache = state
        .cache
        .as_ref()
        .ok_or_else(|| "Cache is not available (server started with --no-cache?)".to_string())?;

    let mut cards = req.cards;
    cards.sort();
    cards.dedup();

    if cards.is_empty() {
        return Err("No cards provided.".into());
    }
    if cards.len() > 100 {
        return Err(format!("Too many cards: {}. Max is 100.", cards.len()));
    }

    let strategy = match req.strategy.as_str() {
        "simplest" => Strategy::Simplest,
        "cheapest" => Strategy::Cheapest,
        other => {
            return Err(format!(
                "Unknown strategy '{}'. Use 'cheapest' or 'simplest'.",
                other
            ))
        }
    };

    // Quick check: do we have cache entries?
    let listings = cache
        .get_listings(&cards)
        .map_err(|e| format!("Failed to read cache: {}", e))?;

    if listings.is_empty() {
        return Err("No cached listings found. Run a /fetch first.".into());
    }

    let tolerance_total = req.tolerance + 1;

    state.reserve_slot()?;

    let job_id = new_job_id();

    let job = Arc::new(Mutex::new(Job {
        status: "pending".into(),
        created_at: std::time::Instant::now(),
        cards_done: cards.len(),
        cards_total: cards.len(),
        current_store: String::new(),
        current_card: String::new(),
        tolerance_done: 0,
        tolerance_total,
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
    let cards_for_thread = cards.clone();
    let eu = req.eu_destination;
    let exhaustive = req.exhaustive;

    std::thread::spawn(move || {
        {
            let mut j = job_ref.lock().unwrap();
            j.status = "running".into();
        }

        // Build input
        let input = wizard::WizardInput::from_results_and_wants(listings, &cards_for_thread, eu);

        let exhaustive_candidates = if exhaustive {
            Some(wizard::select_candidate_stores(&input))
        } else {
            None
        };

        let mut search_mode: wizard::SearchMode = wizard::SearchMode::Heuristic { seed: None };
        let mut solutions: Vec<(usize, WizardSolution)> = Vec::new();

        for t in 0..=req.tolerance {
            if let Some(ref candidates) = exhaustive_candidates {
                search_mode = wizard::SearchMode::Exhaustive {
                    candidates: candidates.clone(),
                };
            }
            let config = WizardConfig {
                strategy,
                tolerance: t,
            };
            if let Some(sol) = wizard::optimize_input(&input, &config, &search_mode) {
                if exhaustive_candidates.is_none() {
                    search_mode = wizard::SearchMode::Heuristic {
                        seed: Some(sol.raw_choices.clone()),
                    };
                }
                solutions.push((t, sol));
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

        let best = solutions.last().unwrap();
        let strategy_name = match strategy {
            Strategy::Simplest => "simplest",
            Strategy::Cheapest => "cheapest",
        };
        let _ = cache_clone.save_wizard_solution(strategy_name, best.0, eu, &best.1);

        j.status = "done".into();
        j.tolerance_done = tolerance_total;
        j.result = Some(JobResult::Wizard(WizardResponseData::from_solution(
            &best.1,
        )));
    });

    Ok(Json(serde_json::json!({ "job_id": job_id })))
}

/// GET /jobs/:id — poll for job status / progress / result.
async fn get_job(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<JobResponse>, String> {
    let jobs = state.jobs.lock().unwrap();
    let job = jobs
        .get(&id)
        .ok_or_else(|| format!("Job '{}' not found.", id))?;
    let response = job.lock().unwrap().to_response();
    Ok(Json(response))
}

// ── Core search logic (with progress callback) ───────────────────────────────

/// Run the search sequentially across stores.  The progress callback is called
/// after each card, passing (card_index, store_index, store_name, card_name).
fn run_search_with_progress(
    cards: &[String],
    all_stores: &[Box<dyn Store>],
    store_indices: &[usize],
    cache: Option<&Cache>,
    mut on_progress: impl FnMut(usize, usize, &str, &str),
) -> Vec<StoreResult> {
    let mut all_results = Vec::new();

    for (si, &store_idx) in store_indices.iter().enumerate() {
        let store = &all_stores[store_idx];
        let store_name = store.name().to_string();
        let timeout = Duration::from_secs(store.timeout_secs());

        let client = {
            let mut builder = reqwest::blocking::Client::builder()
                .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36")
                .timeout(timeout);
            if store_name.starts_with("cardmarket") {
                builder = builder.cookie_store(true);
            }
            builder
                .build()
                .expect("Failed to build per-store HTTP client")
        };

        for (ci, card_name) in cards.iter().enumerate() {
            on_progress(ci, si, &store_name, card_name);

            let cache_keys = store.cache_keys();
            let mut results_for_card: Vec<StoreResult> = Vec::new();
            let mut keys_to_fetch: Vec<String> = Vec::new();

            // Check cache
            for key in &cache_keys {
                let lookup = cache.map(|c| c.lookup(card_name, key)).transpose();
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

            // Fetch uncached
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
                                    if let Some(c) = cache {
                                        let _ = c.store(card_name, key, None);
                                    }
                                }
                            } else {
                                if let Some(c) = cache {
                                    let mut by_store: HashMap<String, Vec<StoreResult>> =
                                        HashMap::new();
                                    for result in &sub_results {
                                        by_store
                                            .entry(result.store_name.clone())
                                            .or_default()
                                            .push(result.clone());
                                    }
                                    for (sn, grouped) in &by_store {
                                        let _ = c.store(card_name, sn, Some(grouped.as_slice()));
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
                                eprintln!("  [{}] Failed: '{}': {}", key, card_name, e);
                            }
                        }
                    }
                }
            }

            all_results.extend(results_for_card);
        }
    }

    all_results
}
