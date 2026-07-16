//! Purchase wizard — finds optimal store assignments for a set of wanted cards.
//!
//! Supports two strategies:
//! - **Simplest**: minimize number of stores (treating ~500 kr as equivalent to 1 store).
//! - **Cheapest**: minimize total cost (penalizing each extra store by ~50 kr).
//!
//! Two-phase optimization:
//! 1. **Multi-start hill climbing** (30 random restarts) establishes a baseline.
//! 2. **Iterated Local Search (ILS)** perturbs the best solution and re-optimizes,
//!    using **simulated annealing** to escape local optima.  The Simplest strategy
//!    also uses store-consolidation perturbations to find fewer-store solutions.
//!    Stops after 5 consecutive non-improving ILS iterations.

use crate::shipping::{self, ShippingInfo};
use crate::stores::StoreResult;
use rand::Rng;
use std::collections::{HashMap, HashSet};

// ── Configuration ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Strategy {
    /// Minimize number of stores; treat each ~500 kr as equivalent to one store.
    Simplest,
    /// Minimize total cost; penalize each extra store by ~50 kr.
    Cheapest,
}

#[derive(Debug, Clone)]
pub struct WizardConfig {
    pub strategy: Strategy,
    /// Maximum number of wanted cards the solution is allowed to skip.
    pub tolerance: usize,
    /// Assume delivery within the EU — removes 25% VAT and customs fees
    /// from international CardMarket sellers.  Applied when building
    /// WizardInput; not read by the optimizer directly.
    #[allow(dead_code)]
    pub eu_destination: bool,
}

impl Default for WizardConfig {
    fn default() -> Self {
        Self {
            strategy: Strategy::Cheapest,
            tolerance: 0,
            eu_destination: false,
        }
    }
}

// ── Input model ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub(crate) struct CardOption {
    store_idx: usize,
    pub(crate) price: u32, // oere
    pub(crate) url: String,
}

#[derive(Debug, Clone)]
pub(crate) struct WizardCard {
    pub(crate) name: String,
    pub(crate) options: Vec<CardOption>,
}

pub(crate) struct WizardInput {
    pub(crate) cards: Vec<WizardCard>,
    pub(crate) store_names: Vec<String>,
    pub(crate) shipping: Vec<ShippingInfo>,
    /// Precomputed lookup: cheapest_at[card_idx][store_idx] = Some((option_idx, price)).
    pub(crate) cheapest_at: Vec<Vec<Option<(usize, u32)>>>,
}

impl WizardInput {
    pub(crate) fn from_results_and_wants(
        results: Vec<StoreResult>,
        wanted_cards: &[String],
        eu_destination: bool,
    ) -> Self {
        let mut store_names: Vec<String> = Vec::new();
        let mut store_idx: HashMap<String, usize> = HashMap::new();
        for r in &results {
            if !store_idx.contains_key(&r.store_name) {
                store_idx.insert(r.store_name.clone(), store_names.len());
                store_names.push(r.store_name.clone());
            }
        }

        let mut card_map: HashMap<String, Vec<CardOption>> = HashMap::new();
        for r in &results {
            let si = store_idx[&r.store_name];
            let mut price = r.price;
            // If EU destination, strip 25% VAT from international seller prices.
            if eu_destination {
                if r.store_name.starts_with("cardmarket-int.com:")
                    || r.store_name.starts_with("cardmarket-int-private.com:")
                {
                    price = (price as f64 / shipping::VAT_MULTIPLIER).round() as u32;
                }
            }
            card_map
                .entry(r.card_name.clone())
                .or_default()
                .push(CardOption {
                    store_idx: si,
                    price,
                    url: r.url.clone(),
                });
        }

        // Build card list from wanted_cards order; cards not in cache get empty options
        let mut cards: Vec<WizardCard> = wanted_cards
            .iter()
            .map(|name| {
                let mut opts = card_map.remove(name).unwrap_or_default();
                opts.sort_by_key(|o| o.price);
                WizardCard {
                    name: name.clone(),
                    options: opts,
                }
            })
            .collect();

        // Include any remaining cached cards not in wanted_cards (shouldn't normally happen)
        for (name, mut opts) in card_map {
            opts.sort_by_key(|o| o.price);
            cards.push(WizardCard {
                name,
                options: opts,
            });
        }

        let mut shipping: Vec<ShippingInfo> = store_names
            .iter()
            .map(|n| shipping::shipping_for(n))
            .collect();

        // If EU destination, strip VAT and customs fee from international shipping.
        if eu_destination {
            for (si, info) in shipping.iter_mut().enumerate() {
                let name = &store_names[si];
                if name.starts_with("cardmarket-int-private.com:") {
                    // Remove VAT from shipping and subtract customs fee.
                    let base_without_customs =
                        info.base.saturating_sub(shipping::CUSTOMS_FEE as u32);
                    info.base =
                        (base_without_customs as f64 / shipping::VAT_MULTIPLIER).round() as u32;
                } else if name.starts_with("cardmarket-int.com:") {
                    // Remove VAT from shipping.
                    info.base = (info.base as f64 / shipping::VAT_MULTIPLIER).round() as u32;
                }
            }
        }

        // Precompute cheapest option per (card, store).
        let cheapest_at: Vec<Vec<Option<(usize, u32)>>> = cards
            .iter()
            .map(|card| {
                let mut row = vec![None; store_names.len()];
                for (oi, opt) in card.options.iter().enumerate() {
                    let entry = &mut row[opt.store_idx];
                    if entry.is_none_or(|(_, p)| opt.price < p) {
                        *entry = Some((oi, opt.price));
                    }
                }
                row
            })
            .collect();

        WizardInput {
            cards,
            store_names,
            shipping,
            cheapest_at,
        }
    }

    fn card_count(&self) -> usize {
        self.cards.len()
    }
}

// ── Assignment ───────────────────────────────────────────────────────────────

/// Scoring constants (in oere).
const PRICE_WEIGHT: u64 = 50000; // 1 store ≈ 500 kr in "simplest"
const STORE_PENALTY: u64 = 5000; // 1 extra store costs 50 kr in "cheapest"
const SKIP_PENALTY: u64 = 500000; // 1 skipped card ≈ 5000 kr

// ── Simulated annealing parameters ───────────────────────────────────────────

/// Initial temperature (in score units).  High enough to accept most worsening
/// moves early in the cooling schedule; decays geometrically each iteration.
const SA_INITIAL_TEMP: f64 = 50_000.0;
/// Per-iteration cooling multiplier.  Closer to 1.0 = slower cooling, more exploration.
const SA_COOLING_RATE: f64 = 0.999;
/// Stop when temperature drops below this threshold.
const SA_MIN_TEMP: f64 = 100.0;

/// Fraction of cards to randomly reassign during an ILS perturbation step.
const ILS_PERTURB_FRACTION: f64 = 0.30;

/// Lightweight assignment: which option was chosen for each card.
/// Used only to build the initial assignment; the optimizer works with
/// `ScoredAssignment` which caches all derived state.
#[derive(Debug, Clone)]
struct Assignment {
    choices: Vec<Option<usize>>,
}

// ── Scored assignment (cached state for fast delta scoring) ───────────────────

/// An assignment with all derived state precomputed, enabling O(1) delta
/// scoring for single-card moves.
#[derive(Debug, Clone)]
struct ScoredAssignment {
    choices: Vec<Option<usize>>,
    /// Total card price per store (indexed by store_idx).
    store_totals: Vec<u32>,
    /// Number of cards assigned to each store.
    store_card_counts: Vec<usize>,
    /// Number of stores with at least one card.
    num_stores: usize,
    /// Number of skipped cards.
    num_skipped: usize,
    /// Cached composite score.
    score: u64,
}

impl ScoredAssignment {
    /// Build a scored assignment from a raw `Assignment`, computing all
    /// derived state in one O(N + S) pass.
    fn new(raw: Assignment, input: &WizardInput, config: &WizardConfig) -> Self {
        let num_stores = input.store_names.len();
        let mut store_totals = vec![0u32; num_stores];
        let mut store_card_counts = vec![0usize; num_stores];
        let mut num_skipped = 0;
        let mut num_stores_used = 0;

        for (ci, opt_idx) in raw.choices.iter().enumerate() {
            match opt_idx {
                Some(oi) => {
                    let opt = &input.cards[ci].options[*oi];
                    if store_card_counts[opt.store_idx] == 0 {
                        num_stores_used += 1;
                    }
                    store_totals[opt.store_idx] += opt.price;
                    store_card_counts[opt.store_idx] += 1;
                }
                None => num_skipped += 1,
            }
        }

        let score =
            Self::compute_score_static(&store_totals, num_stores_used, num_skipped, input, config);

        ScoredAssignment {
            choices: raw.choices,
            store_totals,
            store_card_counts,
            num_stores: num_stores_used,
            num_skipped,
            score,
        }
    }

    /// Compute score from pre-aggregated totals (no per-card iteration).
    fn compute_score_static(
        store_totals: &[u32],
        num_stores: usize,
        num_skipped: usize,
        input: &WizardInput,
        config: &WizardConfig,
    ) -> u64 {
        let card_total: u64 = store_totals.iter().map(|&v| v as u64).sum();
        let shipping: u64 = store_totals
            .iter()
            .enumerate()
            .filter(|(_, &total)| total > 0)
            .map(|(si, &total)| shipping::shipping_cost(si, total as u64, &input.shipping))
            .sum();

        let total_cost = card_total + shipping;
        let excess_skipped = num_skipped.saturating_sub(config.tolerance);
        let skip_cost = (excess_skipped as u64) * SKIP_PENALTY;

        match config.strategy {
            Strategy::Simplest => (num_stores as u64) * PRICE_WEIGHT + total_cost + skip_cost,
            Strategy::Cheapest => {
                total_cost + (num_stores.saturating_sub(1) as u64) * STORE_PENALTY + skip_cost
            }
        }
    }

    /// Recompute the cached score from current derived state (O(S), not O(N)).
    fn recompute_score(&mut self, input: &WizardInput, config: &WizardConfig) {
        self.score = Self::compute_score_static(
            &self.store_totals,
            self.num_stores,
            self.num_skipped,
            input,
            config,
        );
    }

    /// Compute the score *delta* for moving a single card `ci` to `new_oi`.
    /// Returns the signed change (negative = improvement).  O(1) — no
    /// iteration over cards or stores.
    fn try_single_move_delta(
        &self,
        ci: usize,
        new_oi: Option<usize>,
        input: &WizardInput,
        config: &WizardConfig,
    ) -> i64 {
        let old_oi = self.choices[ci];
        if old_oi == new_oi {
            return 0;
        }

        let card = &input.cards[ci];

        // ── Price delta ──────────────────────────────────────────────────
        let old_price = old_oi.map_or(0, |oi| card.options[oi].price) as i64;
        let new_price = new_oi.map_or(0, |oi| card.options[oi].price) as i64;
        let price_delta = new_price - old_price;

        // ── Store identity ───────────────────────────────────────────────
        let old_si = old_oi.map(|oi| card.options[oi].store_idx);
        let new_si = new_oi.map(|oi| card.options[oi].store_idx);

        // ── Skip count delta ─────────────────────────────────────────────
        let skip_delta = new_oi.is_none() as i64 - old_oi.is_none() as i64;
        let new_skipped = (self.num_skipped as i64 + skip_delta) as usize;

        // ── Store count delta ────────────────────────────────────────────
        let store_delta: i64 = if old_si == new_si {
            0
        } else {
            let mut d: i64 = 0;
            if let Some(si) = old_si {
                if self.store_card_counts[si] == 1 {
                    d -= 1;
                }
            }
            if let Some(si) = new_si {
                if self.store_card_counts[si] == 0 {
                    d += 1;
                }
            }
            d
        };
        let new_num_stores = (self.num_stores as i64 + store_delta) as usize;

        // ── Shipping delta ───────────────────────────────────────────────
        let shipping_delta: i64 = if old_si == new_si {
            // Same store (or both None) — only total changes
            if let Some(si) = old_si {
                let old_total = self.store_totals[si] as u64;
                let new_total = (old_total as i64 + price_delta) as u64;
                shipping::shipping_cost(si, new_total, &input.shipping) as i64
                    - shipping::shipping_cost(si, old_total, &input.shipping) as i64
            } else {
                0
            }
        } else {
            let mut d: i64 = 0;
            // Old store loses a card
            if let Some(si) = old_si {
                let old_total = self.store_totals[si] as u64;
                let new_total = old_total.saturating_sub(old_price as u64);
                d += shipping::shipping_cost(si, new_total, &input.shipping) as i64
                    - shipping::shipping_cost(si, old_total, &input.shipping) as i64;
            }
            // New store gains a card
            if let Some(si) = new_si {
                let old_total = self.store_totals[si] as u64;
                let new_total = old_total + new_price as u64;
                d += shipping::shipping_cost(si, new_total, &input.shipping) as i64
                    - shipping::shipping_cost(si, old_total, &input.shipping) as i64;
            }
            d
        };

        // ── Excess-skip penalty delta ────────────────────────────────────
        let old_excess = self.num_skipped.saturating_sub(config.tolerance);
        let new_excess = new_skipped.saturating_sub(config.tolerance);
        let skip_cost_delta = (new_excess as i64 - old_excess as i64) * SKIP_PENALTY as i64;

        // ── Strategy-specific delta ──────────────────────────────────────
        let cost_delta = price_delta + shipping_delta;

        let strategy_delta = match config.strategy {
            Strategy::Simplest => store_delta * PRICE_WEIGHT as i64,
            Strategy::Cheapest => {
                let old_pen = (self.num_stores.saturating_sub(1)) as i64 * STORE_PENALTY as i64;
                let new_pen = (new_num_stores.saturating_sub(1)) as i64 * STORE_PENALTY as i64;
                new_pen - old_pen
            }
        };

        cost_delta + strategy_delta + skip_cost_delta
    }

    /// Apply a single-card move and update all cached state.
    /// Does NOT check that the move differs from current — caller must ensure
    /// `old_oi != new_oi` or this will corrupt state.
    fn apply_single_move(
        &mut self,
        ci: usize,
        new_oi: Option<usize>,
        input: &WizardInput,
        config: &WizardConfig,
    ) {
        let old_oi = self.choices[ci];
        debug_assert!(old_oi != new_oi, "apply_single_move called with no-op");

        let card = &input.cards[ci];
        let old_price = old_oi.map_or(0, |oi| card.options[oi].price);
        let new_price = new_oi.map_or(0, |oi| card.options[oi].price);
        let old_si = old_oi.map(|oi| card.options[oi].store_idx);
        let new_si = new_oi.map(|oi| card.options[oi].store_idx);

        // Update old store
        if let Some(si) = old_si {
            self.store_totals[si] = self.store_totals[si].saturating_sub(old_price);
            self.store_card_counts[si] -= 1;
            if self.store_card_counts[si] == 0 {
                self.num_stores -= 1;
            }
        }

        // Update new store
        if let Some(si) = new_si {
            if self.store_card_counts[si] == 0 {
                self.num_stores += 1;
            }
            self.store_totals[si] += new_price;
            self.store_card_counts[si] += 1;
        }

        // Update skip count
        match (old_oi.is_some(), new_oi.is_some()) {
            (true, false) => self.num_skipped += 1,
            (false, true) => self.num_skipped -= 1,
            _ => {}
        }

        self.choices[ci] = new_oi;
        self.recompute_score(input, config);
    }
}

// ── Public result type ───────────────────────────────────────────────────────

pub struct WizardSolution {
    /// Per-card assignment: (card_name, None if skipped, or Some(store, price, url)).
    pub assignments: Vec<(String, Option<(String, u32, String)>)>,
    /// Store names used (sorted).
    pub store_names: Vec<String>,
    /// Card subtotal per store (parallel to store_names).
    pub card_subtotals: Vec<u32>,
    /// Shipping cost per store (parallel to store_names).
    pub shipping_costs: Vec<u32>,
    /// Skipped card names.
    pub skipped: Vec<String>,
    /// Total card cost (sum of assigned card prices, in oere).
    pub total_card_cost: u64,
    /// Total shipping cost (in oere).
    pub total_shipping: u64,
    /// Number of stores used.
    pub num_stores: usize,
    /// Optimizer's internal score (lower is better).
    #[allow(dead_code)]
    pub score: u64,
}

// ── Entry point ──────────────────────────────────────────────────────────────

/// Run the optimizer on a pre-built input.
///
/// **Phase 1 — Multi-start hill climbing**: 30 random restarts establish a
/// strong baseline solution.
///
/// **Phase 2 — Iterated Local Search (ILS)**: perturb the best solution,
/// hill-climb, then escape local optima with simulated annealing.  Runs up
/// to 20 ILS iterations, stopping early after 5 consecutive non-improvements.
///
/// For the **Simplest** strategy, perturbations alternate between scattering
/// (random reassignment) and consolidating (merging one store into others),
/// since reducing store count is the primary objective.
///
/// Returns the best solution found, or `None` if all cards must be skipped
/// (e.g. tolerance too low and skip-penalty dominates).
pub(crate) fn optimize_input(input: &WizardInput, config: &WizardConfig) -> Option<WizardSolution> {
    let mut rng = rand::thread_rng();
    let mut best: Option<ScoredAssignment> = None;
    let mut best_score: Option<u64> = None;

    // ── Phase 1: Multi-start hill climbing ──────────────────────────────
    let num_restarts = 30;
    for seed in 0..num_restarts {
        let raw = initial_assignment(input, config, seed);
        let mut current = ScoredAssignment::new(raw, input, config);

        loop {
            let neighbor = best_neighbor(&current, input, config);
            match neighbor {
                Some(n) if n.score < current.score => {
                    current = n;
                }
                _ => break,
            }
        }

        if best_score.is_none_or(|s| current.score < s) {
            best_score = Some(current.score);
            best = Some(current.clone());
        }
    }

    // ── Phase 2: Iterated Local Search ──────────────────────────────────
    let mut current_best = best.clone().expect("phase 1 always produces a solution");
    let ils_iterations = 20;
    let mut no_improve = 0u32;

    for i in 0..ils_iterations {
        if no_improve >= 5 {
            break;
        }

        // Perturb the best-so-far.
        // For Simplest: alternate scattering and consolidating.
        // For Cheapest: always scatter (explore different cost profiles).
        let mut candidate = match config.strategy {
            Strategy::Simplest if i % 2 == 0 => {
                // Even iterations: try consolidating a store
                perturb_consolidate(&current_best, input, config, &mut rng)
                    .unwrap_or_else(|| perturb(&current_best, input, config, &mut rng))
            }
            _ => perturb(&current_best, input, config, &mut rng),
        };

        // Hill-climb from the perturbed starting point.
        loop {
            let neighbor = best_neighbor(&candidate, input, config);
            match neighbor {
                Some(n) if n.score < candidate.score => {
                    candidate = n;
                }
                _ => break,
            }
        }

        // Use simulated annealing to escape the local optimum.
        // Skip SA for Simplest when consolidating — the consolidation already
        // provides a strong structural change, and SA's single-card random walk
        // tends to re-scatter cards across stores.
        let sa_candidate = match config.strategy {
            Strategy::Simplest if i % 2 == 0 => candidate,
            _ => simulated_annealing(&candidate, input, config, &mut rng),
        };

        // Hill-climb again from wherever SA ended up.
        let mut final_candidate = sa_candidate;
        loop {
            let neighbor = best_neighbor(&final_candidate, input, config);
            match neighbor {
                Some(n) if n.score < final_candidate.score => {
                    final_candidate = n;
                }
                _ => break,
            }
        }

        // Keep if improved.
        if final_candidate.score < best_score.unwrap() {
            best_score = Some(final_candidate.score);
            best = Some(final_candidate.clone());
            current_best = final_candidate;
            no_improve = 0;
        } else {
            no_improve += 1;
        }
    }

    best.map(|a| build_solution(&a, input, config))
}

// ── Initial assignment builders ──────────────────────────────────────────────

fn initial_assignment(input: &WizardInput, _config: &WizardConfig, seed: usize) -> Assignment {
    // Both strategies start from greedy set cover — this gives a strong
    // baseline with few stores and low shipping.  The strategy-specific
    // score function then guides the hill climb in different directions.
    initial_simplest(input, seed)
}

/// Greedy set cover: repeatedly pick the store covering the most unassigned cards.
fn initial_simplest(input: &WizardInput, seed: usize) -> Assignment {
    let n = input.card_count();
    let mut assigned = vec![false; n];
    let mut choices: Vec<Option<usize>> = vec![None; n];

    // store_cards[si] = set of card indices available at store si
    let store_cards: Vec<HashSet<usize>> = {
        let mut sc = vec![HashSet::new(); input.store_names.len()];
        for (ci, card) in input.cards.iter().enumerate() {
            for opt in &card.options {
                sc[opt.store_idx].insert(ci);
            }
        }
        sc
    };

    loop {
        // Find the store that covers the most still-unassigned cards
        let mut best_count = 0;
        let mut candidates: Vec<usize> = Vec::new();

        for si in 0..input.store_names.len() {
            let count = store_cards[si].iter().filter(|&&ci| !assigned[ci]).count();
            if count > best_count {
                best_count = count;
                candidates.clear();
                candidates.push(si);
            } else if count == best_count && count > 0 {
                candidates.push(si);
            }
        }

        if best_count == 0 {
            break; // nothing more to cover
        }

        let pick = candidates[seed % candidates.len()];

        // Assign all uncovered cards that this store has
        for &ci in &store_cards[pick] {
            if !assigned[ci] {
                if let Some((oi, _)) = input.cheapest_at[ci][pick] {
                    choices[ci] = Some(oi);
                    assigned[ci] = true;
                }
            }
        }
    }

    Assignment { choices }
}

// ── Neighbor generation ──────────────────────────────────────────────────────

fn best_neighbor(
    current: &ScoredAssignment,
    input: &WizardInput,
    config: &WizardConfig,
) -> Option<ScoredAssignment> {
    let mut best_delta: i64 = 0;
    let mut best_move: Option<(usize, Option<usize>)> = None;
    let n = input.card_count();

    // ── Move 1: Reassign a single card (delta scoring, O(1) per candidate) ─
    for ci in 0..n {
        let card = &input.cards[ci];
        let cur = current.choices[ci];

        for oi in 0..card.options.len() {
            if Some(oi) == cur {
                continue;
            }
            let delta = current.try_single_move_delta(ci, Some(oi), input, config);
            if delta < best_delta {
                best_delta = delta;
                best_move = Some((ci, Some(oi)));
            }
        }

        if cur.is_some() {
            let delta = current.try_single_move_delta(ci, None, input, config);
            if delta < best_delta {
                best_delta = delta;
                best_move = Some((ci, None));
            }
        }

        if cur.is_none() && !card.options.is_empty() {
            // Options are sorted by price, so [0] is the global cheapest.
            let delta = current.try_single_move_delta(ci, Some(0), input, config);
            if delta < best_delta {
                best_delta = delta;
                best_move = Some((ci, Some(0)));
            }
        }
    }

    // ── Move 2: Consolidate small stores (delta scoring, O(1) per candidate) ─
    let small_stores: Vec<usize> = current
        .store_card_counts
        .iter()
        .enumerate()
        .filter(|&(_, &count)| count >= 1 && count <= 2)
        .map(|(si, _)| si)
        .collect();

    for &from_si in &small_stores {
        for ci in 0..n {
            if let Some(oi) = current.choices[ci] {
                if input.cards[ci].options[oi].store_idx != from_si {
                    continue;
                }
                for (alt_oi, alt_opt) in input.cards[ci].options.iter().enumerate() {
                    if alt_opt.store_idx == from_si {
                        continue;
                    }
                    let delta = current.try_single_move_delta(ci, Some(alt_oi), input, config);
                    if delta < best_delta {
                        best_delta = delta;
                        best_move = Some((ci, Some(alt_oi)));
                    }
                }
                let delta = current.try_single_move_delta(ci, None, input, config);
                if delta < best_delta {
                    best_delta = delta;
                    best_move = Some((ci, None));
                }
            }
        }
    }

    // ── Move 3: Bulk-merge entire stores ───────────────────────────────────
    // Try moving all cards from store A to store B in one step.  This lets
    // the climber escape local optima where single-card moves can't
    // eliminate a store because each intermediate step looks worse.
    //
    // Clone+apply_single_move approach: O(N) clone + O(cards_at_A) applies.
    let used_stores: Vec<usize> = current
        .store_card_counts
        .iter()
        .enumerate()
        .filter(|&(_, &c)| c > 0)
        .map(|(si, _)| si)
        .collect();

    // Apply best single-card move first so Move 3 competes against the
    // improved result.
    let mut best_result: Option<ScoredAssignment> = best_move.map(|(ci, new_oi)| {
        let mut nb = current.clone();
        nb.apply_single_move(ci, new_oi, input, config);
        nb
    });
    let mut best_score = best_result.as_ref().map_or(current.score, |r| r.score);

    for &from_si in &used_stores {
        for &to_si in &used_stores {
            if from_si == to_si {
                continue;
            }

            let mut merge_ok = true;
            let mut moves: Vec<(usize, usize)> = Vec::new();
            for ci in 0..n {
                if let Some(oi) = current.choices[ci] {
                    if input.cards[ci].options[oi].store_idx == from_si {
                        match input.cheapest_at[ci][to_si] {
                            Some((alt_oi, _)) => moves.push((ci, alt_oi)),
                            None => {
                                merge_ok = false;
                                break;
                            }
                        }
                    }
                }
            }

            if !merge_ok || moves.is_empty() {
                continue;
            }

            let mut nb = current.clone();
            for &(ci, oi) in &moves {
                nb.apply_single_move(ci, Some(oi), input, config);
            }
            if nb.score < best_score {
                best_score = nb.score;
                best_result = Some(nb);
            }
        }
    }

    best_result
}

// ── Simulated annealing ──────────────────────────────────────────────────────

/// Pick a uniformly-random single-card move that differs from the current choice.
fn random_move(
    current: &ScoredAssignment,
    input: &WizardInput,
    rng: &mut impl Rng,
) -> (usize, Option<usize>) {
    let n = input.card_count();
    let ci = rng.gen_range(0..n);
    let card = &input.cards[ci];
    let cur = current.choices[ci];

    // Total distinct choices: every available option + skip
    let total_choices = card.options.len() + 1;
    if total_choices <= 1 {
        return (ci, cur); // no alternative
    }

    loop {
        let pick = rng.gen_range(0..total_choices);
        let new_oi = if pick < card.options.len() {
            Some(pick)
        } else {
            None // skip
        };
        if new_oi != cur {
            return (ci, new_oi);
        }
    }
}

/// Run simulated annealing starting from `initial`.  Accepts worsening moves
/// with probability exp(-Δ/T) where T starts at `SA_INITIAL_TEMP` and decays
/// geometrically.  Returns the best assignment encountered during the walk.
fn simulated_annealing(
    initial: &ScoredAssignment,
    input: &WizardInput,
    config: &WizardConfig,
    rng: &mut impl Rng,
) -> ScoredAssignment {
    let mut current = initial.clone();
    let mut best = current.clone();
    let mut best_score = best.score;
    let mut temp = SA_INITIAL_TEMP;

    while temp > SA_MIN_TEMP {
        let (ci, new_oi) = random_move(&current, input, rng);
        if new_oi == current.choices[ci] {
            // no-op (card has no alternatives); skip this iteration
            temp *= SA_COOLING_RATE;
            continue;
        }

        let delta = current.try_single_move_delta(ci, new_oi, input, config) as f64;

        // Accept if improving, or probabilistically if worsening
        if delta <= 0.0 || rng.gen::<f64>() < (-delta / temp).exp() {
            current.apply_single_move(ci, new_oi, input, config);
            if current.score < best_score {
                best_score = current.score;
                best = current.clone();
            }
        }

        temp *= SA_COOLING_RATE;
    }

    best
}

// ── Iterated Local Search (ILS) ──────────────────────────────────────────────

/// Perturb an assignment by randomly reassigning a fraction of cards to
/// different options (including skipping), producing a new starting point
/// for another hill-climb.
fn perturb(
    current: &ScoredAssignment,
    input: &WizardInput,
    config: &WizardConfig,
    rng: &mut impl Rng,
) -> ScoredAssignment {
    let mut perturbed = current.clone();
    let n = input.card_count();
    let perturb_count = ((n as f64) * ILS_PERTURB_FRACTION).ceil() as usize;

    // Pick perturb_count distinct card indices via Fisher-Yates partial shuffle.
    let mut indices: Vec<usize> = (0..n).collect();
    for i in 0..perturb_count.min(n) {
        let j = rng.gen_range(i..n);
        indices.swap(i, j);
    }

    for &ci in &indices[..perturb_count.min(n)] {
        let card = &input.cards[ci];
        let cur = perturbed.choices[ci];
        let total_choices = card.options.len() + 1;

        if total_choices <= 1 {
            continue;
        }

        loop {
            let pick = rng.gen_range(0..total_choices);
            let new_oi = if pick < card.options.len() {
                Some(pick)
            } else {
                None
            };
            if new_oi != cur {
                perturbed.apply_single_move(ci, new_oi, input, config);
                break;
            }
        }
    }

    perturbed
}

/// Strategy-aware perturbation for the **Simplest** strategy: pick a random
/// store and move all its cards to their cheapest alternative at a *different*
/// store.  This directly reduces the store count by one, which is exactly
/// what the Simplest score function rewards.
///
/// Returns `None` if no consolidation is possible (only one store used, or
/// no cards can be relocated).
fn perturb_consolidate(
    current: &ScoredAssignment,
    input: &WizardInput,
    config: &WizardConfig,
    rng: &mut impl Rng,
) -> Option<ScoredAssignment> {
    let n = input.card_count();

    // Pick a random store that currently has cards.
    let used_stores: Vec<usize> = current
        .store_card_counts
        .iter()
        .enumerate()
        .filter(|&(_, &c)| c > 0)
        .map(|(si, _)| si)
        .collect();

    if used_stores.len() <= 1 {
        return None;
    }

    let from_si = used_stores[rng.gen_range(0..used_stores.len())];

    // Collect card indices assigned to this store.
    let cards_at_store: Vec<usize> = (0..n)
        .filter(|&ci| {
            current.choices[ci].is_some_and(|oi| input.cards[ci].options[oi].store_idx == from_si)
        })
        .collect();

    if cards_at_store.is_empty() {
        return None;
    }

    let mut result = current.clone();
    let mut any_moved = false;

    for &ci in &cards_at_store {
        let card = &input.cards[ci];
        // Find the cheapest option at any store other than from_si.
        let mut best_alt: Option<(usize, u32)> = None;
        for (oi, opt) in card.options.iter().enumerate() {
            if opt.store_idx != from_si {
                if best_alt.is_none_or(|(_, p)| opt.price < p) {
                    best_alt = Some((oi, opt.price));
                }
            }
        }

        if let Some((oi, _)) = best_alt {
            result.apply_single_move(ci, Some(oi), input, config);
            any_moved = true;
        }
        // Cards with no alternative store stay put (partial consolidation
        // can still reduce store count if enough cards move).
    }

    if any_moved {
        Some(result)
    } else {
        None
    }
}

// ── Solution bookkeeping ─────────────────────────────────────────────────────

fn build_solution(
    scored: &ScoredAssignment,
    input: &WizardInput,
    _config: &WizardConfig,
) -> WizardSolution {
    // Gather used stores from cached card counts (no per-card iteration).
    let mut used_stores: Vec<usize> = scored
        .store_card_counts
        .iter()
        .enumerate()
        .filter(|&(_, &c)| c > 0)
        .map(|(si, _)| si)
        .collect();
    used_stores.sort();

    let store_names: Vec<String> = used_stores
        .iter()
        .map(|&si| input.store_names[si].clone())
        .collect();
    let card_subtotals: Vec<u32> = used_stores
        .iter()
        .map(|&si| scored.store_totals[si])
        .collect();
    let shipping_costs: Vec<u32> = used_stores
        .iter()
        .map(|&si| {
            let total = scored.store_totals[si];
            let info = &input.shipping[si];
            if info.free_threshold > 0 && total >= info.free_threshold {
                0
            } else {
                info.base
            }
        })
        .collect();

    let total_card_cost: u64 = card_subtotals.iter().map(|&v| v as u64).sum();
    let total_shipping: u64 = shipping_costs.iter().map(|&v| v as u64).sum();

    // Build assignments: found cards first, skipped at end
    let mut found: Vec<(String, Option<(String, u32, String)>)> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();

    for (ci, card) in input.cards.iter().enumerate() {
        match scored.choices[ci] {
            Some(oi) => {
                let opt = &card.options[oi];
                found.push((
                    card.name.clone(),
                    Some((
                        input.store_names[opt.store_idx].clone(),
                        opt.price,
                        opt.url.clone(),
                    )),
                ));
            }
            None => skipped.push(card.name.clone()),
        }
    }

    let mut assignments = found;
    for name in &skipped {
        assignments.push((name.clone(), None));
    }

    WizardSolution {
        assignments,
        store_names,
        card_subtotals,
        shipping_costs,
        skipped,
        total_card_cost,
        total_shipping,
        num_stores: used_stores.len(),
        score: scored.score,
    }
}
