//! Purchase wizard — finds optimal store assignments for a set of wanted cards.
//!
//! Supports two strategies:
//! - **Simplest**: minimize number of stores (treating ~500 kr as equivalent to 1 store).
//! - **Cheapest**: minimize total cost (penalizing each extra store by ~50 kr).
//!
//! Two-phase optimization:
//! 1. **Multi-start hill climbing** (50 random restarts) establishes a baseline.
//! 2. **Iterated Local Search (ILS)** perturbs the best solution and re-optimizes,
//!    using **simulated annealing** (~1,360 steps) to escape local optima.  The
//!    Simplest strategy also uses store-consolidation perturbations to find
//!    fewer-store solutions.  Stops after 3 consecutive non-improving ILS iters.

use crate::shipping::{self, ShippingInfo};
use crate::stores::StoreResult;
use rand::rngs::StdRng;
use rand::Rng;
use rand::SeedableRng;
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::AtomicU64;

/// Incremented by `try_store_set` during the store-swap refinement phase.
/// Server mode reads this to report exhaustive wizard progress.
pub(crate) static SWAP_TRIAL_COUNT: AtomicU64 = AtomicU64::new(0);

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
}

impl Default for WizardConfig {
    fn default() -> Self {
        Self {
            strategy: Strategy::Cheapest,
            tolerance: 0,
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
    pub(crate) shipping_bases: Vec<u32>,
    pub(crate) shipping_free_thresholds: Vec<u32>,
    pub(crate) min_orders: Vec<u32>,
    /// Card-count threshold before shipping surcharge applies (0 = no tier).
    pub(crate) shipping_card_limits: Vec<u32>,
    /// Extra shipping cost when card count exceeds the threshold.
    pub(crate) shipping_card_surcharges: Vec<u32>,
    /// Flat price matrix: prices[ci * store_names.len() + si] = cheapest
    /// price at store `si` for card `ci`.  `u32::MAX` means N/A.
    pub(crate) prices: Vec<u32>,
    /// Parallel to `prices`: the CardOption index for URL reconstruction.
    pub(crate) option_indices: Vec<u32>,
    /// Precomputed mapping: store_cards[store_idx] = sorted unique card indices
    /// available at that store.  Used by `initial_simplest` for greedy set cover.
    pub(crate) store_cards: Vec<Vec<usize>>,
}

impl WizardInput {
    pub(crate) fn from_results_and_wants(
        results: Vec<StoreResult>,
        wanted_cards: &[String],
        eu_destination: bool,
    ) -> Self {
        // Filter out blacklisted sellers (don't ship to Norway) unless the
        // destination is within the EU, where they may ship.
        let results: Vec<StoreResult> = if eu_destination {
            results
        } else {
            let mut blacklisted: Vec<String> = Vec::new();
            let filtered: Vec<StoreResult> = results
                .into_iter()
                .filter(|r| {
                    let seller = shipping::extract_seller_name(&r.store_name);
                    if shipping::is_blacklisted(seller) {
                        blacklisted.push(seller.to_string());
                        false
                    } else {
                        true
                    }
                })
                .collect();
            if !blacklisted.is_empty() {
                blacklisted.sort();
                blacklisted.dedup();
                eprintln!(
                    "  [wizard] skipping blacklisted seller(s): {}",
                    blacklisted.join(", ")
                );
            }
            filtered
        };

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

            // Skip results with no price (e.g. Finn listings that don't
            // specify a price — they default to 0).
            if price == 0 {
                continue;
            }

            // If EU destination, strip 25% VAT from international seller prices.
            if eu_destination
                && (r.store_name.starts_with("cardmarket-int.com:")
                    || r.store_name.starts_with("cardmarket-int-private.com:"))
            {
                price = (price as f64 / shipping::VAT_MULTIPLIER).round() as u32;
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

        // Precompute cheapest option per (card, store) as flat arrays.
        let n_stores = store_names.len();
        let n_cards = cards.len();
        let mut prices = vec![u32::MAX; n_cards * n_stores];
        let mut option_indices = vec![0u32; n_cards * n_stores];

        for (ci, card) in cards.iter().enumerate() {
            let row_start = ci * n_stores;
            for (oi, opt) in card.options.iter().enumerate() {
                let idx = row_start + opt.store_idx;
                if opt.price < prices[idx] {
                    prices[idx] = opt.price;
                    option_indices[idx] = oi as u32;
                }
            }
        }

        // Precompute store → card index mapping (used by greedy set cover).
        let mut store_cards: Vec<Vec<usize>> = vec![Vec::new(); store_names.len()];
        for (ci, card) in cards.iter().enumerate() {
            for opt in &card.options {
                store_cards[opt.store_idx].push(ci);
            }
        }
        // Deduplicate and sort each store's card list.
        for cards in store_cards.iter_mut() {
            cards.sort_unstable();
            cards.dedup();
        }

        let shipping_bases: Vec<u32> = shipping.iter().map(|s| s.base).collect();
        let shipping_free_thresholds: Vec<u32> =
            shipping.iter().map(|s| s.free_threshold).collect();
        let min_orders: Vec<u32> = shipping.iter().map(|s| s.min_order).collect();
        let shipping_card_limits: Vec<u32> = shipping.iter().map(|s| s.card_limit).collect();
        let shipping_card_surcharges: Vec<u32> =
            shipping.iter().map(|s| s.card_surcharge).collect();

        WizardInput {
            cards,
            store_names,
            shipping_bases,
            shipping_free_thresholds,
            min_orders,
            shipping_card_limits,
            shipping_card_surcharges,
            prices,
            option_indices,
            store_cards,
        }
    }

    fn card_count(&self) -> usize {
        self.cards.len()
    }

    pub(crate) fn n_stores(&self) -> usize {
        self.store_names.len()
    }

    /// Cold-path: return (option_idx, price) if store carries this card.
    pub(crate) fn cheapest_at(&self, ci: usize, si: usize) -> Option<(usize, u32)> {
        let idx = ci * self.n_stores() + si;
        let p = self.prices[idx];
        if p == u32::MAX {
            None
        } else {
            Some((self.option_indices[idx] as usize, p))
        }
    }

    /// Cold-path: does store `si` carry card `ci`?
    pub(crate) fn has_card(&self, ci: usize, si: usize) -> bool {
        self.prices[ci * self.n_stores() + si] != u32::MAX
    }

    /// Flat-array inlined shipping cost.  Avoids struct indirection and
    /// function-call overhead compared to `shipping::shipping_cost`.
    /// Returns a prohibitive penalty (SKIP_PENALTY) when the store is used
    /// but the card total is below min_order, so the optimizer naturally
    /// avoids under-minimum orders.
    ///
    /// `card_count` is the number of distinct cards bought from this store;
    /// exceeding the store's card_limit triggers a surcharge (e.g. larger
    /// envelope for >20 cards).
    #[inline(always)]
    fn shipping_cost(&self, si: usize, total: u64, card_count: usize) -> u64 {
        if total == 0 {
            return 0;
        }
        let min = self.min_orders[si] as u64;
        if total < min {
            return SKIP_PENALTY;
        }
        let base = self.shipping_bases[si] as u64;
        let mut cost = if self.shipping_free_thresholds[si] > 0
            && total >= self.shipping_free_thresholds[si] as u64
        {
            0
        } else {
            base
        };
        // Card-count tier: if the store has a limit and we exceed it, add surcharge.
        let limit = self.shipping_card_limits[si] as usize;
        if limit > 0 && card_count > limit {
            cost += self.shipping_card_surcharges[si] as u64;
        }
        cost
    }
}

// ── Assignment ───────────────────────────────────────────────────────────────

/// Scoring constants (in oere).
pub(crate) const PRICE_WEIGHT: u64 = 50000; // 1 store ≈ 500 kr in "simplest"
pub(crate) const STORE_PENALTY: u64 = 2000; // 1 extra store costs 20 kr in "cheapest"
pub(crate) const SKIP_PENALTY: u64 = 500000; // 1 skipped card ≈ 5000 kr

// ── Simulated annealing parameters ───────────────────────────────────────────

/// Initial temperature (in score units).  High enough to accept most worsening
/// moves early in the cooling schedule; decays geometrically each iteration.
const SA_INITIAL_TEMP: f64 = 30_000.0;
/// Stop when temperature drops below this threshold.
const SA_MIN_TEMP: f64 = 500.0;

/// Lightweight assignment: which option was chosen for each card.
/// Used only to build the initial assignment; the optimizer works with
/// `ScoredAssignment` which caches all derived state.
#[derive(Debug, Clone)]
struct Assignment {
    choices: Vec<Option<usize>>,
}

// ── Scored assignment (cached state for fast delta scoring) ───────────────────

/// Pre-extracted components of a card move, computed once and shared between
/// delta scoring and state mutation to avoid recomputation.
struct MoveInfo {
    old_price: u32,
    new_price: u32,
    old_si: Option<usize>,
    new_si: Option<usize>,
}

/// Arguments for `try_single_move_delta_pre_fast`, bundling the per-move
/// fields that the caller already has from the outer loop of `best_neighbor`.
struct FastMove {
    ci: usize,
    old_price: u32,
    old_si: Option<usize>,
    new_oi: Option<usize>,
}

/// Values that are constant for the duration of a `best_neighbor` scan.
/// Computing them once and threading them into `delta_from_info` avoids
/// redundant `saturating_sub` and multiplication in the inner loop.
struct DeltaPrecompute {
    base_excess: usize,
    base_store_penalty: i64, // Cheapest strategy only; zero for Simplest
    /// `shipping_cost(si, store_totals[si], store_card_counts[si])` for the
    /// current assignment, with 0 for unused stores.  Lets the fast delta path
    /// avoid recomputing the "before" shipping term on every candidate move.
    store_shipping: Vec<u64>,
}

/// Per-card constants for the specialized Move-1 delta in `best_neighbor`.
/// Computed once per card (outside the per-option loop) to keep the inner
/// loop branch-light.
struct Move1Hoist {
    old_price: u32,
    old_si: Option<usize>,
    old_store_empties: bool,
    old_ship_delta: i64,
    skip_delta: i64,
}

/// A move produced by `best_neighbor`: the sequence of single-card moves to
/// apply, plus the exact score the resulting assignment will have.  Returning
/// this descriptor (instead of a fully cloned/applied `ScoredAssignment`)
/// avoids cloning the large per-store vectors on every candidate.
struct Neighbor {
    moves: Vec<(usize, Option<usize>)>,
    score: u64,
}

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
    /// Cached total card price (sum of all assigned card prices).
    card_total: u64,
    /// Reverse index: cards_at[store_idx] = list of card indices assigned
    /// to that store.  Maintained incrementally by `apply_single_move`.
    cards_at: Vec<Vec<usize>>,
    /// Auxiliary index: card_pos[ci] = position within cards_at[store_idx].
    /// Makes swap_remove O(1) instead of O(k) per card-at-store scan.
    card_pos: Vec<usize>,
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
        let n_cards = input.card_count();
        let mut choices = raw.choices;
        // Defensive: cached seeds may have a different card count if the
        // deck changed between runs.  Truncate excess or pad with skips.
        if choices.len() > n_cards {
            choices.truncate(n_cards);
        } else if choices.len() < n_cards {
            choices.resize(n_cards, None);
        }

        let mut card_total: u64 = 0;
        let mut cards_at: Vec<Vec<usize>> = vec![Vec::new(); num_stores];
        let mut card_pos = vec![0usize; choices.len()];
        for (ci, opt_idx) in choices.iter_mut().enumerate() {
            let valid = opt_idx.is_some_and(|oi| oi < input.cards[ci].options.len());
            if valid {
                let oi = opt_idx.unwrap();
                let opt = &input.cards[ci].options[oi];
                let si = opt.store_idx;
                if store_card_counts[si] == 0 {
                    num_stores_used += 1;
                }
                store_totals[si] += opt.price;
                store_card_counts[si] += 1;
                card_total += opt.price as u64;
                card_pos[ci] = cards_at[si].len();
                cards_at[si].push(ci);
            } else {
                *opt_idx = None;
                num_skipped += 1;
            }
        }

        let score = Self::compute_score_static(
            card_total,
            &store_totals,
            &store_card_counts,
            num_stores_used,
            num_skipped,
            input,
            config,
        );

        ScoredAssignment {
            choices,
            store_totals,
            store_card_counts,
            num_stores: num_stores_used,
            num_skipped,
            card_total,
            cards_at,
            card_pos,
            score,
        }
    }

    /// Compute score from pre-aggregated totals (no per-card iteration).
    fn compute_score_static(
        card_total: u64,
        store_totals: &[u32],
        store_card_counts: &[usize],
        num_stores: usize,
        num_skipped: usize,
        input: &WizardInput,
        config: &WizardConfig,
    ) -> u64 {
        let shipping: u64 = store_totals
            .iter()
            .enumerate()
            .filter(|(_, &total)| total > 0)
            .map(|(si, &total)| input.shipping_cost(si, total as u64, store_card_counts[si]))
            .sum();

        compute_raw_score(
            card_total + shipping,
            num_stores,
            num_skipped,
            config.tolerance,
            config.strategy,
        )
    }

    /// Extract the price and store components for a potential move.
    /// `old_oi` and `new_oi` are passed explicitly so the caller (which
    /// already knows them) doesn't pay for another choices[] lookup.
    #[inline]
    fn move_info(
        &self,
        ci: usize,
        old_oi: Option<usize>,
        new_oi: Option<usize>,
        input: &WizardInput,
    ) -> MoveInfo {
        let card = &input.cards[ci];
        let old_price = old_oi.map_or(0, |oi| card.options[oi].price);
        let new_price = new_oi.map_or(0, |oi| card.options[oi].price);
        let old_si = old_oi.map(|oi| card.options[oi].store_idx);
        let new_si = new_oi.map(|oi| card.options[oi].store_idx);
        MoveInfo {
            old_price,
            new_price,
            old_si,
            new_si,
        }
    }

    /// Compute score delta from pre-extracted MoveInfo (avoids recomputing
    /// prices and store indices).  Used by both `try_single_move_delta` and
    /// `apply_single_move`.
    fn delta_from_info(&self, info: &MoveInfo, input: &WizardInput, config: &WizardConfig) -> i64 {
        let price_delta = info.new_price as i64 - info.old_price as i64;

        // ── Skip count delta ─────────────────────────────────────────────
        let skip_delta = info.new_si.is_none() as i64 - info.old_si.is_none() as i64;
        let new_skipped = (self.num_skipped as i64 + skip_delta) as usize;

        // ── Store count delta ────────────────────────────────────────────
        let store_delta: i64 = if info.old_si == info.new_si {
            0
        } else {
            let mut d: i64 = 0;
            if let Some(si) = info.old_si {
                if self.store_card_counts[si] == 1 {
                    d -= 1;
                }
            }
            if let Some(si) = info.new_si {
                if self.store_card_counts[si] == 0 {
                    d += 1;
                }
            }
            d
        };
        let new_num_stores = (self.num_stores as i64 + store_delta) as usize;

        // ── Shipping delta ───────────────────────────────────────────────
        let shipping_delta: i64 = if info.old_si == info.new_si {
            if let Some(si) = info.old_si {
                if input.shipping_free_thresholds[si] == 0 && input.shipping_card_limits[si] == 0 {
                    0
                } else {
                    let old_total = self.store_totals[si] as u64;
                    let new_total = (old_total as i64 + price_delta) as u64;
                    let count = self.store_card_counts[si];
                    input.shipping_cost(si, new_total, count) as i64
                        - input.shipping_cost(si, old_total, count) as i64
                }
            } else {
                0
            }
        } else {
            let mut d: i64 = 0;
            if let Some(si) = info.old_si {
                let old_total = self.store_totals[si] as u64;
                let new_total = old_total.saturating_sub(info.old_price as u64);
                let old_count = self.store_card_counts[si];
                let new_count = old_count.saturating_sub(1);
                d += input.shipping_cost(si, new_total, new_count) as i64
                    - input.shipping_cost(si, old_total, old_count) as i64;
            }
            if let Some(si) = info.new_si {
                let old_total = self.store_totals[si] as u64;
                let new_total = old_total + info.new_price as u64;
                let old_count = self.store_card_counts[si];
                let new_count = old_count + 1;
                d += input.shipping_cost(si, new_total, new_count) as i64
                    - input.shipping_cost(si, old_total, old_count) as i64;
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
        let info = self.move_info(ci, old_oi, new_oi, input);
        self.delta_from_info(&info, input, config)
    }

    /// Variant of `try_single_move_delta` that uses precomputed per-scan
    /// constants to save redundant arithmetic in the inner loop of
    /// `best_neighbor`.
    #[inline]
    fn try_single_move_delta_pre(
        &self,
        ci: usize,
        new_oi: Option<usize>,
        input: &WizardInput,
        config: &WizardConfig,
        pre: &DeltaPrecompute,
    ) -> i64 {
        let old_oi = self.choices[ci];
        if old_oi == new_oi {
            return 0;
        }
        let info = self.move_info(ci, old_oi, new_oi, input);
        self.delta_from_info_pre(&info, input, config, pre)
    }

    /// Variant of `try_single_move_delta_pre` that avoids re-extracting
    /// `old_price` and `old_si` from `input.cards[ci].options` — the caller
    /// already has them from the outer loop of `best_neighbor` Move 1.
    #[inline]
    fn try_single_move_delta_pre_fast(
        &self,
        mv: &FastMove,
        input: &WizardInput,
        config: &WizardConfig,
        pre: &DeltaPrecompute,
    ) -> i64 {
        let card = &input.cards[mv.ci];
        let new_price = mv.new_oi.map_or(0, |oi| card.options[oi].price);
        let new_si = mv.new_oi.map(|oi| card.options[oi].store_idx);
        let info = MoveInfo {
            old_price: mv.old_price,
            new_price,
            old_si: mv.old_si,
            new_si,
        };
        self.delta_from_info_pre(&info, input, config, pre)
    }

    /// Variant of `delta_from_info` that avoids redundant computation of
    /// values that are constant for the duration of a neighbor scan.
    #[inline]
    fn delta_from_info_pre(
        &self,
        info: &MoveInfo,
        input: &WizardInput,
        config: &WizardConfig,
        pre: &DeltaPrecompute,
    ) -> i64 {
        let price_delta = info.new_price as i64 - info.old_price as i64;

        // ── Skip count delta ─────────────────────────────────────────────
        let skip_delta = info.new_si.is_none() as i64 - info.old_si.is_none() as i64;
        let new_skipped = (self.num_skipped as i64 + skip_delta) as usize;

        // ── Store count delta ────────────────────────────────────────────
        let store_delta: i64 = if info.old_si == info.new_si {
            0
        } else {
            let mut d: i64 = 0;
            if let Some(si) = info.old_si {
                if self.store_card_counts[si] == 1 {
                    d -= 1;
                }
            }
            if let Some(si) = info.new_si {
                if self.store_card_counts[si] == 0 {
                    d += 1;
                }
            }
            d
        };
        let new_num_stores = (self.num_stores as i64 + store_delta) as usize;

        // ── Shipping delta ───────────────────────────────────────────────
        let shipping_delta: i64 = if info.old_si == info.new_si {
            if let Some(si) = info.old_si {
                if input.shipping_free_thresholds[si] == 0 && input.shipping_card_limits[si] == 0 {
                    0
                } else {
                    let old_total = self.store_totals[si] as u64;
                    let new_total = (old_total as i64 + price_delta) as u64;
                    let count = self.store_card_counts[si];
                    input.shipping_cost(si, new_total, count) as i64 - pre.store_shipping[si] as i64
                }
            } else {
                0
            }
        } else {
            let mut d: i64 = 0;
            if let Some(si) = info.old_si {
                let old_total = self.store_totals[si] as u64;
                let new_total = old_total.saturating_sub(info.old_price as u64);
                let count = self.store_card_counts[si];
                d += input.shipping_cost(si, new_total, count.saturating_sub(1)) as i64
                    - pre.store_shipping[si] as i64;
            }
            if let Some(si) = info.new_si {
                let old_total = self.store_totals[si] as u64;
                let new_total = old_total + info.new_price as u64;
                let count = self.store_card_counts[si];
                d += input.shipping_cost(si, new_total, count + 1) as i64
                    - pre.store_shipping[si] as i64;
            }
            d
        };

        // ── Excess-skip penalty delta (precomputed base) ─────────────────
        let new_excess = new_skipped.saturating_sub(config.tolerance);
        let skip_cost_delta = (new_excess as i64 - pre.base_excess as i64) * SKIP_PENALTY as i64;

        // ── Strategy-specific delta ──────────────────────────────────────
        let cost_delta = price_delta + shipping_delta;
        let strategy_delta = match config.strategy {
            Strategy::Simplest => store_delta * PRICE_WEIGHT as i64,
            Strategy::Cheapest => {
                let new_pen = (new_num_stores.saturating_sub(1)) as i64 * STORE_PENALTY as i64;
                new_pen - pre.base_store_penalty
            }
        };

        cost_delta + strategy_delta + skip_cost_delta
    }

    /// Build per-card constants for the Move-1 inner loop.  The old-side
    /// shipping delta, store-empties flag, and skip-penalty delta depend only
    /// on the card's current choice, so they are computed once per card.
    #[inline]
    fn move1_hoist(
        &self,
        ci: usize,
        input: &WizardInput,
        config: &WizardConfig,
        pre: &DeltaPrecompute,
    ) -> Move1Hoist {
        let card = &input.cards[ci];
        let cur = self.choices[ci];
        let old_price = cur.map_or(0, |oi| card.options[oi].price);
        let old_si = cur.map(|oi| card.options[oi].store_idx);

        let old_store_empties = old_si.is_some_and(|si| self.store_card_counts[si] == 1);
        let old_ship_delta = match old_si {
            Some(si) => {
                let old_total = self.store_totals[si] as u64;
                let new_total = old_total.saturating_sub(old_price as u64);
                let count = self.store_card_counts[si];
                input.shipping_cost(si, new_total, count.saturating_sub(1)) as i64
                    - pre.store_shipping[si] as i64
            }
            None => 0,
        };
        let skip_delta = if old_si.is_none() {
            let new_excess = (self.num_skipped.saturating_sub(1)).saturating_sub(config.tolerance);
            (new_excess as i64 - pre.base_excess as i64) * SKIP_PENALTY as i64
        } else {
            0
        };

        Move1Hoist {
            old_price,
            old_si,
            old_store_empties,
            old_ship_delta,
            skip_delta,
        }
    }

    /// Specialized Move-1 delta for `new_oi == Some(..)`.  `new_si` and
    /// `new_price` come straight from the candidate option, avoiding the
    /// generic `move_info`/`delta_from_info_pre` indirection.  Must match
    /// `delta_from_info_pre` exactly for the `new_si = Some` case.
    #[inline]
    fn move1_delta(
        &self,
        hoist: &Move1Hoist,
        new_si: usize,
        new_price: u32,
        input: &WizardInput,
        config: &WizardConfig,
        pre: &DeltaPrecompute,
    ) -> i64 {
        let price_delta = new_price as i64 - hoist.old_price as i64;

        let store_delta: i64 = if hoist.old_si == Some(new_si) {
            0
        } else {
            -(hoist.old_store_empties as i64) + (self.store_card_counts[new_si] == 0) as i64
        };

        let shipping_delta: i64 = if hoist.old_si == Some(new_si) {
            if input.shipping_free_thresholds[new_si] == 0
                && input.shipping_card_limits[new_si] == 0
            {
                0
            } else {
                let old_total = self.store_totals[new_si] as u64;
                let new_total = (old_total as i64 + price_delta) as u64;
                input.shipping_cost(new_si, new_total, self.store_card_counts[new_si]) as i64
                    - pre.store_shipping[new_si] as i64
            }
        } else {
            hoist.old_ship_delta
                + input.shipping_cost(
                    new_si,
                    self.store_totals[new_si] as u64 + new_price as u64,
                    self.store_card_counts[new_si] + 1,
                ) as i64
                - pre.store_shipping[new_si] as i64
        };

        let cost_delta = price_delta + shipping_delta;
        let strategy_delta = match config.strategy {
            Strategy::Simplest => store_delta * PRICE_WEIGHT as i64,
            Strategy::Cheapest => {
                let new_num_stores = (self.num_stores as i64 + store_delta) as usize;
                (new_num_stores.saturating_sub(1)) as i64 * STORE_PENALTY as i64
                    - pre.base_store_penalty
            }
        };

        cost_delta + strategy_delta + hoist.skip_delta
    }

    /// Exact delta for a card swap: move `ci` (currently at store `si`) to
    /// `cj`'s store, and `cj` (currently at store `sj`) to `ci`'s store.
    /// Both cards stay assigned, so store counts, `num_stores`, and
    /// `num_skipped` are all unchanged — only the two store totals change.
    fn swap_delta(
        &self,
        ci: usize,
        ci_new_oi: usize,
        cj: usize,
        cj_new_oi: usize,
        input: &WizardInput,
        pre: &DeltaPrecompute,
    ) -> i64 {
        let card_ci = &input.cards[ci];
        let card_cj = &input.cards[cj];
        let old_oi_ci = self.choices[ci].expect("swap card must be assigned");
        let old_oi_cj = self.choices[cj].expect("swap card must be assigned");

        let si = card_ci.options[old_oi_ci].store_idx;
        let sj = card_cj.options[old_oi_cj].store_idx;

        let old_price_ci = card_ci.options[old_oi_ci].price as u64;
        let old_price_cj = card_cj.options[old_oi_cj].price as u64;
        let new_price_ci = card_ci.options[ci_new_oi].price as u64;
        let new_price_cj = card_cj.options[cj_new_oi].price as u64;

        let price_delta = (new_price_ci as i64 - old_price_ci as i64)
            + (new_price_cj as i64 - old_price_cj as i64);

        let count_si = self.store_card_counts[si];
        let new_total_si = self.store_totals[si] as u64 - old_price_ci + new_price_cj;
        let ship_delta_si =
            input.shipping_cost(si, new_total_si, count_si) as i64 - pre.store_shipping[si] as i64;

        let count_sj = self.store_card_counts[sj];
        let new_total_sj = self.store_totals[sj] as u64 - old_price_cj + new_price_ci;
        let ship_delta_sj =
            input.shipping_cost(sj, new_total_sj, count_sj) as i64 - pre.store_shipping[sj] as i64;

        price_delta + ship_delta_si + ship_delta_sj
    }

    /// Exact score after bulk-merging `from_si` into `to_si` (moving every
    /// card in `moves`).  `from_si` empties, so `num_stores` drops by one and
    /// `num_skipped` is unchanged.
    fn merge_score(
        &self,
        from_si: usize,
        to_si: usize,
        moves: &[(usize, usize)],
        input: &WizardInput,
        config: &WizardConfig,
        pre: &DeltaPrecompute,
    ) -> u64 {
        let mut card_total_delta: i64 = 0;
        let mut new_total_to = self.store_totals[to_si] as u64;
        for &(ci, oi) in moves {
            let card = &input.cards[ci];
            let old_oi = self.choices[ci].expect("merge card must be assigned");
            card_total_delta += card.options[oi].price as i64 - card.options[old_oi].price as i64;
            new_total_to += card.options[oi].price as u64;
        }
        let new_count_to = self.store_card_counts[to_si] + moves.len();

        // `from_si` drops to zero cards (shipping 0); only `to_si` changes
        // on the positive side.  Other stores are untouched.
        let shipping_delta = input.shipping_cost(to_si, new_total_to, new_count_to) as i64
            - pre.store_shipping[to_si] as i64
            - pre.store_shipping[from_si] as i64;

        let strategy_delta = match config.strategy {
            Strategy::Simplest => -(PRICE_WEIGHT as i64),
            Strategy::Cheapest => -(STORE_PENALTY as i64),
        };

        self.score
            .wrapping_add_signed(card_total_delta + shipping_delta + strategy_delta)
    }

    /// Apply a list of single-card moves in sequence, updating all cached
    /// state.  Used to materialize the neighbor returned by `best_neighbor`.
    fn apply_moves(
        &mut self,
        moves: &[(usize, Option<usize>)],
        input: &WizardInput,
        config: &WizardConfig,
    ) {
        for &(ci, new_oi) in moves {
            self.apply_single_move(ci, new_oi, input, config);
        }
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

        // Extract move components once; shared between delta computation
        // and state mutation.
        let info = self.move_info(ci, old_oi, new_oi, input);
        let delta = self.delta_from_info(&info, input, config);

        // Update old store
        if let Some(si) = info.old_si {
            self.store_totals[si] = self.store_totals[si].saturating_sub(info.old_price);
            self.store_card_counts[si] -= 1;
            if self.store_card_counts[si] == 0 {
                self.num_stores -= 1;
            }
            // O(1) removal via precomputed position index.
            let pos = self.card_pos[ci];
            self.cards_at[si].swap_remove(pos);
            if pos < self.cards_at[si].len() {
                let swapped = self.cards_at[si][pos];
                self.card_pos[swapped] = pos;
            }
        }

        // Update new store
        if let Some(si) = info.new_si {
            if self.store_card_counts[si] == 0 {
                self.num_stores += 1;
            }
            self.store_totals[si] += info.new_price;
            self.store_card_counts[si] += 1;
            self.card_pos[ci] = self.cards_at[si].len();
            self.cards_at[si].push(ci);
        }

        // Update skip count
        match (info.old_si.is_some(), info.new_si.is_some()) {
            (true, false) => self.num_skipped += 1,
            (false, true) => self.num_skipped -= 1,
            _ => {}
        }

        self.choices[ci] = new_oi;
        self.card_total =
            (self.card_total as i64 + info.new_price as i64 - info.old_price as i64) as u64;
        self.score = (self.score as i64 + delta) as u64;
    }
}

// ── Public result type ───────────────────────────────────────────────────────

/// Per-card result: (card_name, None if skipped, or Some(store, price, url)).
pub(crate) type CardAssignment = (String, Option<(String, u32, String)>);

#[derive(Clone)]
pub struct WizardSolution {
    /// Per-card assignment: (card_name, None if skipped, or Some(store, price, url)).
    pub assignments: Vec<CardAssignment>,
    /// Raw option indices, parallel to `assignments`.  Used to seed the next
    /// tolerance level for monotonic cost guarantees.
    pub(crate) raw_choices: Vec<Option<usize>>,
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
    pub score: u64,
}

/// Canonical scoring function: computes the composite score from pre-aggregated
/// totals.  Every score path in the module (compute_score_static,
/// build_solution) funnels through this one function to guarantee consistency.
#[inline]
pub(crate) fn compute_raw_score(
    total_cost: u64,
    num_stores: usize,
    num_skipped: usize,
    tolerance: usize,
    strategy: Strategy,
) -> u64 {
    let excess_skipped = num_skipped.saturating_sub(tolerance);
    let skip_cost = (excess_skipped as u64) * SKIP_PENALTY;
    match strategy {
        Strategy::Simplest => (num_stores as u64) * PRICE_WEIGHT + total_cost + skip_cost,
        Strategy::Cheapest => {
            total_cost + (num_stores.saturating_sub(1) as u64) * STORE_PENALTY + skip_cost
        }
    }
}

// ── Entry point ──────────────────────────────────────────────────────────────

/// Controls which optimization strategy to use.
pub(crate) enum SearchMode {
    /// Multi-start hill climbing + iterated local search with simulated annealing.
    /// Optionally seeded from a previous tolerance level's solution for monotonic
    /// cost guarantees.
    Heuristic { seed: Option<Vec<Option<usize>>> },
    /// Heuristic baseline + store-swap refinement.  Candidates are pre-computed
    /// by `select_candidate_stores` and reused across tolerance levels.
    Exhaustive { candidates: Vec<usize> },
}

// ── Candidate store selection ──────────────────────────────────────────────────

/// When evaluating INT sellers for saturation: a seller is "useful" if it
/// carries at least this many cards.  Consolidation only matters if the
/// seller covers a meaningful fraction of the deck.
const INT_MIN_COVERAGE: usize = 8;

/// A seller is "useful" if at least one card it carries is within this
/// multiple of the candidate pool's current best price.
const INT_PRICE_MARGIN: f64 = 1.25;

/// Stop adding INT sellers after this many consecutive non-useful sellers.
const INT_SATURATION_WINDOW: usize = 40;

/// Select a compact candidate store pool for store-combination enumeration.
/// Returns store indices that could plausibly appear in an optimal solution.
pub(crate) fn select_candidate_stores(input: &WizardInput) -> Vec<usize> {
    let num_stores = input.store_names.len();
    if num_stores <= 40 {
        return (0..num_stores)
            .filter(|&si| !input.store_names[si].starts_with("cardmarket-int-private.com:"))
            .collect();
    }

    let n_cards = input.card_count();
    let mut candidates: Vec<usize> = Vec::new();
    let mut seen: HashSet<usize> = HashSet::new();

    // 1. All Norwegian storefronts (not on CardMarket).
    for si in 0..num_stores {
        let name = &input.store_names[si];
        if !name.starts_with("cardmarket") {
            candidates.push(si);
            seen.insert(si);
        }
    }

    // 2. NO CardMarket sellers: top 80 by card count.
    let mut no_sellers: Vec<(usize, usize)> = (0..num_stores)
        .filter(|&si| input.store_names[si].starts_with("cardmarket.com:"))
        .map(|si| (si, input.store_cards[si].len()))
        .collect();
    no_sellers.sort_by_key(|(_, c)| std::cmp::Reverse(*c));
    for (si, _) in no_sellers.into_iter().take(80) {
        if seen.insert(si) {
            candidates.push(si);
        }
    }

    // 3. INT professional sellers: saturation-based selection.
    //    Instead of a hardcoded top-120, keep adding sellers ranked by card
    //    count as long as they provide competitively-priced alternatives.
    //    Stops when INT_SATURATION_WINDOW consecutive sellers add nothing
    //    useful — the long tail of small/expensive sellers is irrelevant.
    //
    //    Precompute best price per card from the pool so far (storefronts +
    //    NO sellers).  Update incrementally as useful INT sellers join.
    let mut pool_best: Vec<u32> = (0..n_cards)
        .map(|ci| {
            candidates
                .iter()
                .filter_map(|&si| input.cheapest_at(ci, si).map(|(_, p)| p))
                .min()
                .unwrap_or(u32::MAX)
        })
        .collect();

    let mut int_sellers: Vec<(usize, usize)> = (0..num_stores)
        .filter(|&si| input.store_names[si].starts_with("cardmarket-int.com:"))
        .map(|si| (si, input.store_cards[si].len()))
        .collect();
    int_sellers.sort_by_key(|(_, c)| std::cmp::Reverse(*c));

    let mut int_added = 0usize;
    let mut consecutive_fails = 0usize;
    // Reusable buffer to avoid per-seller Vec allocation.
    let mut seller_prices: Vec<Option<u32>> = vec![None; n_cards];
    for (si, coverage) in int_sellers {
        if coverage < INT_MIN_COVERAGE {
            consecutive_fails += 1;
            if consecutive_fails >= INT_SATURATION_WINDOW {
                break;
            }
            continue;
        }

        // Single pass: collect prices AND check usefulness.
        let mut useful = false;
        for ci in 0..n_cards {
            let price = input.cheapest_at(ci, si).map(|(_, p)| p);
            seller_prices[ci] = price;
            if !useful {
                if let Some(price) = price {
                    if pool_best[ci] == u32::MAX {
                        useful = true;
                    } else {
                        useful = (price as f64) <= (pool_best[ci] as f64) * INT_PRICE_MARGIN;
                    }
                }
            }
        }

        if useful {
            if seen.insert(si) {
                candidates.push(si);
                int_added += 1;
                for (ci, best) in pool_best.iter_mut().enumerate() {
                    if let Some(price) = seller_prices[ci] {
                        if price < *best {
                            *best = price;
                        }
                    }
                }
            }
            consecutive_fails = 0;
        } else {
            consecutive_fails += 1;
            if consecutive_fails >= INT_SATURATION_WINDOW {
                break;
            }
        }
    }

    if int_added > 0 {
        eprintln!(
            "  [exhaustive] saturation kept {} INT seller(s) (window {}, margin {}%)",
            int_added,
            INT_SATURATION_WINDOW,
            ((INT_PRICE_MARGIN - 1.0) * 100.0) as u32,
        );
    }

    // 4. Safety net: for each card, include its cheapest overall store
    //    (guarantees at least one option per card).
    for ci in 0..input.card_count() {
        if let Some(first_opt) = input.cards[ci].options.first() {
            let si = first_opt.store_idx;
            if seen.insert(si) && !input.store_names[si].starts_with("cardmarket-int-private.com:")
            {
                candidates.push(si);
            }
        }
    }

    // 5. Safety net: for any card not yet covered by candidate stores,
    //    add its cheapest non-private store.
    for ci in 0..input.card_count() {
        let covered = candidates.iter().any(|&si| input.has_card(ci, si));
        if !covered {
            if let Some(best_si) = input.cards[ci]
                .options
                .iter()
                .filter(|o| {
                    !input.store_names[o.store_idx].starts_with("cardmarket-int-private.com:")
                })
                .map(|o| o.store_idx)
                .next()
            {
                if seen.insert(best_si) {
                    candidates.push(best_si);
                }
            }
        }
    }

    // Filter out blacklisted sellers (don't ship to Norway).
    // Belt-and-suspenders: this should already be filtered in
    // from_results_and_wants, but exhaustive mode can reach here too.
    candidates.retain(|&si| {
        !shipping::is_blacklisted(shipping::extract_seller_name(&input.store_names[si]))
    });

    // ── Pairwise dominance pruning ───────────────────────────────────────
    // Store S is dominated by store T if T has ≤ shipping AND for every
    // card S carries, T also carries it at ≤ price.  A dominated store
    // can never improve a solution: swapping it for its dominator never
    // increases card cost, shipping cost, or loses coverage.
    //
    // Skip for single-card searches — every store that carries the card
    // is a distinct alternative the user wants to see.
    if input.card_count() <= 1 {
        return candidates;
    }
    let mut dominated: HashSet<usize> = HashSet::new();
    for (i, &si) in candidates.iter().enumerate() {
        if dominated.contains(&si) {
            continue;
        }
        let si_shipping = input.shipping_bases[si];
        for (j, &sj) in candidates.iter().enumerate() {
            if i == j {
                continue;
            }
            if dominated.contains(&sj) {
                continue;
            }
            // sj must have ≤ shipping to dominate si.
            if input.shipping_bases[sj] > si_shipping {
                continue;
            }
            // sj must carry every card that si carries, at ≤ price.
            // Only iterate the cards si actually carries (store_cards), not
            // every card in the deck.
            let sj_dominates_si = input.store_cards[si].iter().all(|&ci| {
                let si_price = input.cheapest_at(ci, si).map(|(_, p)| p).unwrap();
                input
                    .cheapest_at(ci, sj)
                    .is_some_and(|(_, sj_price)| sj_price <= si_price)
            });
            if sj_dominates_si {
                dominated.insert(si);
                break; // si is dominated, no need to check other sj against it
            }
        }
    }

    if !dominated.is_empty() {
        let before = candidates.len();
        candidates.retain(|si| !dominated.contains(si));
        eprintln!(
            "  [exhaustive] pairwise dominance pruned {} store(s) ({} -> {})",
            before - candidates.len(),
            before,
            candidates.len(),
        );
    }

    candidates
}

/// Reconstruct a `WizardSolution` from stored `raw_choices`.
/// Used to display a previous run's solution that beat the current one.
pub(crate) fn solution_from_choices(
    choices: &[Option<usize>],
    input: &WizardInput,
    config: &WizardConfig,
) -> WizardSolution {
    let raw = Assignment {
        choices: choices.to_vec(),
    };
    let scored = ScoredAssignment::new(raw, input, config);
    build_solution(&scored, input)
}

/// Run the optimizer on a pre-built input.
///
/// **Exhaustive mode**: runs the heuristic (multi-start + ILS + SA) to
/// establish a baseline, then iteratively refines the store set via
/// add/swap/remove operations — for each trial set, does greedy card
/// assignment followed by hill-climbing.  Converges when no store-set
/// change improves the score.
///
/// **Heuristic mode**:
/// **Phase 1 — Multi-start hill climbing**: 50-800 random restarts scaled
/// by tolerance.
/// **Phase 2 — Iterated Local Search + Simulated Annealing**: 8-100
/// iterations scaled by tolerance, stopping after 3 consecutive
/// non-improvements.
///
/// Simplest strategy also uses store-consolidation perturbations.
pub(crate) fn optimize_input(
    input: &WizardInput,
    config: &WizardConfig,
    mode: &SearchMode,
) -> Vec<WizardSolution> {
    // ── Exhaustive path ──────────────────────────────────────────────────
    if let SearchMode::Exhaustive { candidates } = mode {
        if let Some(solutions) = optimize_exhaustive(input, config, candidates) {
            return solutions;
        }
    }

    // ── Heuristic path ───────────────────────────────────────────────────
    let seed: Option<&[Option<usize>]> = match mode {
        SearchMode::Heuristic { seed } => seed.as_deref(),
        SearchMode::Exhaustive { .. } => None,
    };

    let best = run_heuristic(input, config, seed, 3);
    best.into_iter()
        .map(|a| build_solution(&a, input))
        .collect()
}

/// Run the heuristic optimizer: multi-start hill climbing + iterated local
/// search with simulated annealing.  Returns the best assignments found (up
/// to `max_count` distinct solutions).
fn run_heuristic(
    input: &WizardInput,
    config: &WizardConfig,
    seed: Option<&[Option<usize>]>,
    max_count: usize,
) -> Vec<ScoredAssignment> {
    let mut top = TopSolutions::new(max_count);
    let mut rng = rand::thread_rng();

    // If seeded from a previous tolerance, use it as a candidate.
    // This guarantees the cost can only go down (or stay equal) as
    // tolerance increases.
    if let Some(choices) = seed {
        let raw = Assignment {
            choices: choices.to_vec(),
        };
        let current = hill_climb(ScoredAssignment::new(raw, input, config), input, config);
        top.insert(current);
    }

    // ── Phase 1: Multi-start hill climbing (parallel) ────────────────────
    // Front-load effort on unseeded tolerances.  Higher tolerances are
    // seeded from the previous level's solution and need less exploration.
    let num_restarts = (800 / (config.tolerance + 1)).max(50);
    let parallel_results: Vec<ScoredAssignment> = (0..num_restarts)
        .into_par_iter()
        .map(|seed_idx| {
            let raw = initial_assignment(input, seed_idx);
            hill_climb(ScoredAssignment::new(raw, input, config), input, config)
        })
        .collect();

    for sa in parallel_results {
        top.insert(sa);
    }

    // ── Phase 2: Iterated Local Search ──────────────────────────────────
    let mut current_best = top.best().clone();
    // Taper ILS effort: tolerance 0 gets full exploration, higher
    // tolerances are seeded and need fewer perturbation cycles.
    let ils_iterations: u32 = (100 / (config.tolerance as u32 + 1)).max(8);
    let sa_cooling: f64 = match config.tolerance {
        0 => 0.9993,
        1 => 0.999,
        _ => 0.997,
    };
    let perturb_frac: f64 = match config.tolerance {
        0 => 0.50,
        1 => 0.40,
        _ => 0.30,
    };
    let mut best_score = current_best.score;
    let mut no_improve = 0u32;

    for i in 0..ils_iterations {
        if no_improve >= 3 {
            break;
        }

        // Perturb the best-so-far.
        // For Simplest: alternate scattering and consolidating.
        // For Cheapest: always scatter (explore different cost profiles).
        let mut candidate = match config.strategy {
            Strategy::Simplest if i % 2 == 0 => {
                // Even iterations: try consolidating a store
                perturb_consolidate(&current_best, input, config, &mut rng).unwrap_or_else(|| {
                    perturb_with_frac(&current_best, input, config, &mut rng, perturb_frac)
                })
            }
            _ => perturb_with_frac(&current_best, input, config, &mut rng, perturb_frac),
        };

        // Hill-climb from the perturbed starting point.
        candidate = hill_climb(candidate, input, config);

        // Use simulated annealing to escape the local optimum.
        // Skip SA for Simplest when consolidating — the consolidation already
        // provides a strong structural change, and SA's single-card random walk
        // tends to re-scatter cards across stores.
        let sa_candidate = match config.strategy {
            Strategy::Simplest if i % 2 == 0 => candidate,
            _ => {
                // Multi-walker SA: run several parallel SA walks from the same
                // starting point.  SA is stochastic, so different random seeds
                // explore different escape paths.
                let num_walkers = 4usize;
                let seeds: Vec<u64> = (0..num_walkers).map(|_| rng.gen()).collect();
                seeds
                    .into_par_iter()
                    .map(|seed| {
                        let mut wrng = StdRng::seed_from_u64(seed);
                        simulated_annealing_with(&candidate, input, config, &mut wrng, sa_cooling)
                    })
                    .min_by_key(|c| c.score)
                    .unwrap_or(candidate)
            }
        };

        // Hill-climb again from wherever SA ended up.
        let final_candidate = hill_climb(sa_candidate, input, config);

        top.insert(final_candidate.clone());

        // Keep if improved.
        if final_candidate.score < best_score {
            best_score = final_candidate.score;
            current_best = final_candidate;
            no_improve = 0;
        } else {
            no_improve += 1;
        }
    }

    top.into_vec()
}

/// Tracks the best N distinct scored assignments, sorted by score ascending.
struct TopSolutions {
    entries: Vec<ScoredAssignment>,
    max_count: usize,
}

impl TopSolutions {
    fn new(max_count: usize) -> Self {
        Self {
            entries: Vec::with_capacity(max_count),
            max_count,
        }
    }

    /// Insert a scored assignment, keeping only the best `max_count` distinct
    /// entries (deduplicated by `raw_choices`).
    fn insert(&mut self, sa: ScoredAssignment) {
        if self.entries.iter().any(|e| e.choices == sa.choices) {
            return;
        }
        let pos = self.entries.partition_point(|e| e.score < sa.score);
        if pos < self.max_count {
            self.entries.insert(pos, sa);
            self.entries.truncate(self.max_count);
        }
    }

    /// Like `insert`, but clones the assignment only when it actually makes
    /// the top `max_count`.  Avoids a full `ScoredAssignment` clone for the
    /// (usually many) trials that are not retained.
    fn insert_ref(&mut self, sa: &ScoredAssignment) {
        if self.entries.iter().any(|e| e.choices == sa.choices) {
            return;
        }
        let pos = self.entries.partition_point(|e| e.score < sa.score);
        if pos < self.max_count {
            self.entries.insert(pos, sa.clone());
            self.entries.truncate(self.max_count);
        }
    }

    /// The single best assignment (panics if empty).
    fn best(&self) -> &ScoredAssignment {
        &self.entries[0]
    }

    fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn into_vec(self) -> Vec<ScoredAssignment> {
        self.entries
    }
}

/// Greedily assign each card to the cheapest store in `store_set` that
/// carries it.  Cards not available in any store in the set are skipped.
fn greedy_assign(input: &WizardInput, store_set: &[usize]) -> Assignment {
    let n = input.card_count();
    let mut choices = vec![None; n];
    for (ci, choice) in choices.iter_mut().enumerate().take(n) {
        // Options are sorted by price ascending, so the first option whose
        // store is in `store_set` is the cheapest available option.
        for (oi, opt) in input.cards[ci].options.iter().enumerate() {
            if store_set.contains(&opt.store_idx) {
                *choice = Some(oi);
                break;
            }
        }
    }
    Assignment { choices }
}

/// Extract the set of store indices currently used in a scored assignment.
fn used_store_indices(sa: &ScoredAssignment) -> Vec<usize> {
    sa.cards_at
        .iter()
        .enumerate()
        .filter(|(_, cards)| !cards.is_empty())
        .map(|(si, _)| si)
        .collect()
}

/// Build a trial store set by adding and/or removing one store, run greedy
/// assignment + hill-climb, and return the resulting scored assignment.
///
/// `hill_climb` is deterministic, so two trials that seed the same greedy
/// assignment produce identical results.  When that happens (recorded in
/// `seen_seeds`), this returns `None` without paying for the climb again.
fn try_store_set(
    input: &WizardInput,
    config: &WizardConfig,
    current: &[usize],
    add: Option<usize>,
    remove: Option<usize>,
    seen_seeds: &mut HashSet<Vec<Option<usize>>>,
) -> Option<ScoredAssignment> {
    let mut trial: Vec<usize> = current
        .iter()
        .enumerate()
        .filter(|(i, _)| Some(*i) != remove)
        .map(|(_, &s)| s)
        .collect();
    if let Some(si) = add {
        trial.push(si);
    }
    SWAP_TRIAL_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let raw = greedy_assign(input, &trial);
    if !seen_seeds.insert(raw.choices.clone()) {
        return None;
    }
    Some(hill_climb(
        ScoredAssignment::new(raw, input, config),
        input,
        config,
    ))
}

/// Explore alternative store combinations by trying add/swap/remove
/// operations on the store set.  For each trial set, does greedy assignment
/// followed by hill-climbing.  Continues until no improvement is found.
/// Populates `top` with the best solutions discovered during the search.
fn store_swap_search(
    input: &WizardInput,
    config: &WizardConfig,
    candidates: &[usize],
    initial: &ScoredAssignment,
    top: &mut TopSolutions,
) {
    let mut best = initial.clone();
    top.insert_ref(&best);

    // `try_store_set` is deterministic, so deduplicate by its greedy seed
    // across the whole search (not just within one iteration).
    let mut seen_seeds: HashSet<Vec<Option<usize>>> = HashSet::new();

    loop {
        let old_score = best.score;
        let current_stores = used_store_indices(&best);
        let mut best_trial: Option<ScoredAssignment> = None;

        // Add a store.
        for &new_si in candidates {
            if current_stores.contains(&new_si) {
                continue;
            }
            let Some(trial) = try_store_set(
                input,
                config,
                &current_stores,
                Some(new_si),
                None,
                &mut seen_seeds,
            ) else {
                continue;
            };
            top.insert_ref(&trial);
            if trial.score < old_score && best_trial.as_ref().is_none_or(|b| trial.score < b.score)
            {
                best_trial = Some(trial);
            }
        }

        // Swap a store (replace one current store with a candidate).
        for (idx, _) in current_stores.iter().enumerate() {
            for &new_si in candidates {
                if current_stores.contains(&new_si) {
                    continue;
                }
                let Some(trial) = try_store_set(
                    input,
                    config,
                    &current_stores,
                    Some(new_si),
                    Some(idx),
                    &mut seen_seeds,
                ) else {
                    continue;
                };
                top.insert_ref(&trial);
                if trial.score < old_score
                    && best_trial.as_ref().is_none_or(|b| trial.score < b.score)
                {
                    best_trial = Some(trial);
                }
            }
        }

        // Remove a store (if more than one).
        if current_stores.len() > 1 {
            for (idx, _) in current_stores.iter().enumerate() {
                let Some(trial) = try_store_set(
                    input,
                    config,
                    &current_stores,
                    None,
                    Some(idx),
                    &mut seen_seeds,
                ) else {
                    continue;
                };
                top.insert_ref(&trial);
                if trial.score < old_score
                    && best_trial.as_ref().is_none_or(|b| trial.score < b.score)
                {
                    best_trial = Some(trial);
                }
            }
        }

        match best_trial {
            Some(trial) => best = trial,
            None => break,
        }
    }
}

/// Exhaustively search for the optimal store assignment.
///
/// Phase 1: heuristic (multi-start + ILS + SA) for a baseline and top
/// alternatives.
/// Phase 2: store-swap local search that tries add/swap/remove operations
/// on the store set, collecting more alternatives.
/// Returns up to 3 distinct solutions, best first.
fn optimize_exhaustive(
    input: &WizardInput,
    config: &WizardConfig,
    candidates: &[usize],
) -> Option<Vec<WizardSolution>> {
    let n = input.card_count();
    let n_stores = input.store_names.len();

    eprintln!(
        "  [exhaustive] {} cards x {} stores -> {} candidates",
        n,
        n_stores,
        candidates.len(),
    );

    let mut top = TopSolutions::new(3);

    // Phase 1: heuristic baseline + alternatives.
    for sa in run_heuristic(input, config, None, 3) {
        top.insert(sa);
    }
    let baseline_stores = used_store_indices(top.best()).len();
    eprintln!(
        "  [exhaustive] phase 1 (heuristic): {} stores, score {}",
        baseline_stores,
        top.best().score,
    );

    // Phase 2: store-swap refinement, collecting more alternatives.
    let before_score = top.best().score;
    let initial_for_swap = top.best().clone();
    store_swap_search(input, config, candidates, &initial_for_swap, &mut top);
    let after_stores = used_store_indices(top.best()).len();
    if top.best().score < before_score {
        eprintln!(
            "  [exhaustive] phase 2 (store-swap): improved to {} stores, score {}",
            after_stores,
            top.best().score,
        );
    } else {
        eprintln!(
            "  [exhaustive] phase 2 (store-swap): no improvement, keeping {} stores",
            after_stores,
        );
    }

    if top.is_empty() {
        None
    } else {
        Some(
            top.into_vec()
                .into_iter()
                .map(|a| build_solution(&a, input))
                .collect(),
        )
    }
}

// ── Initial assignment builders ──────────────────────────────────────────────
fn initial_assignment(input: &WizardInput, seed: usize) -> Assignment {
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
    let mut picked = vec![false; input.store_names.len()];

    loop {
        // Find the store that covers the most still-unassigned cards
        let mut best_count = 0;
        let mut candidates: Vec<usize> = Vec::new();

        for (si, _) in input.store_names.iter().enumerate() {
            if picked[si] {
                continue;
            }
            let count = input.store_cards[si]
                .iter()
                .filter(|&&ci| !assigned[ci])
                .count();
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
        picked[pick] = true;

        // Assign all uncovered cards that this store has
        for &ci in &input.store_cards[pick] {
            if !assigned[ci] {
                if let Some((oi, _)) = input.cheapest_at(ci, pick) {
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
) -> Option<Neighbor> {
    // Precompute values that are constant for the entire neighbor scan.
    let mut store_shipping = vec![0u64; input.store_names.len()];
    for (si, &count) in current.store_card_counts.iter().enumerate() {
        if count > 0 {
            store_shipping[si] = input.shipping_cost(si, current.store_totals[si] as u64, count);
        }
    }
    let pre = DeltaPrecompute {
        base_excess: current.num_skipped.saturating_sub(config.tolerance),
        base_store_penalty: if matches!(config.strategy, Strategy::Cheapest) {
            (current.num_stores.saturating_sub(1)) as i64 * STORE_PENALTY as i64
        } else {
            0
        },
        store_shipping,
    };

    let mut best: Option<Neighbor> = None;
    let mut best_score = current.score;
    let n = input.card_count();

    // ── Move 1: Reassign a single card (delta scoring, O(1) per candidate) ─
    let n_st = input.store_names.len();
    for ci in 0..n {
        let card = &input.cards[ci];
        let cur = current.choices[ci];

        // Hoist per-card constants once (old price/store, old-side shipping
        // delta, store-empties flag, skip penalty), then score each candidate
        // option with the specialized Move-1 delta.
        let hoist = current.move1_hoist(ci, input, config, &pre);
        let old_price = hoist.old_price;
        let old_si = hoist.old_si;

        let row_off = ci * n_st;
        for oi in 0..card.options.len() {
            if Some(oi) == cur {
                continue;
            }
            let opt = &card.options[oi];
            // Skip non-cheapest options at this store — only the cheapest
            // per store can ever be the winning move for that store.
            if opt.price > input.prices[row_off + opt.store_idx] {
                continue;
            }
            // Same store + same-or-higher price → can never improve.
            if Some(opt.store_idx) == old_si && opt.price >= old_price {
                continue;
            }
            let delta = current.move1_delta(&hoist, opt.store_idx, opt.price, input, config, &pre);
            let score = current.score.wrapping_add_signed(delta);
            if score < best_score {
                best_score = score;
                best = Some(Neighbor {
                    moves: vec![(ci, Some(oi))],
                    score,
                });
            }
        }

        if cur.is_some() {
            let delta = current.try_single_move_delta_pre_fast(
                &FastMove {
                    ci,
                    old_price,
                    old_si,
                    new_oi: None,
                },
                input,
                config,
                &pre,
            );
            let score = current.score.wrapping_add_signed(delta);
            if score < best_score {
                best_score = score;
                best = Some(Neighbor {
                    moves: vec![(ci, None)],
                    score,
                });
            }
        }

        if cur.is_none() && !card.options.is_empty() {
            // Options are sorted by price, so [0] is the global cheapest.
            let delta = current.try_single_move_delta_pre_fast(
                &FastMove {
                    ci,
                    old_price: 0,
                    old_si: None,
                    new_oi: Some(0),
                },
                input,
                config,
                &pre,
            );
            let score = current.score.wrapping_add_signed(delta);
            if score < best_score {
                best_score = score;
                best = Some(Neighbor {
                    moves: vec![(ci, Some(0))],
                    score,
                });
            }
        }
    }

    // ── Precompute card-to-store index for all moves below ────────────────
    // Fast path: use the incrementally-maintained cards_at from ScoredAssignment.
    let cards_at = &current.cards_at;
    let num_stores = input.store_names.len();

    // ── Move 1.5: Swap cards between different stores (Cheapest only) ────
    // Swap ci at store A with cj at store B in one atomic step.
    // Single-card moves can't express this because each intermediate state
    // (move ci first, or cj first) may look worse even when the combined
    // swap is beneficial.  Skipped for Simplest — consolidation matters more.
    //
    // Bounded to store pairs with ≤ 10 cards each to keep O(K²) manageable.
    // Uses O(1) delta pre-check before cloning.
    if !matches!(config.strategy, Strategy::Simplest) {
        const SWAP_MAX_PER_STORE: usize = 10;

        for si in 0..num_stores {
            if current.store_card_counts[si] > SWAP_MAX_PER_STORE {
                continue;
            }
            if cards_at[si].is_empty() {
                continue;
            }
            for sj in (si + 1)..num_stores {
                if current.store_card_counts[sj] > SWAP_MAX_PER_STORE {
                    continue;
                }
                if cards_at[sj].is_empty() {
                    continue;
                }
                for &ci in &cards_at[si] {
                    let ci_at_sj = match input.cheapest_at(ci, sj) {
                        Some((oi, _)) => oi,
                        None => continue,
                    };
                    for &cj in &cards_at[sj] {
                        let cj_at_si = match input.cheapest_at(cj, si) {
                            Some((oi, _)) => oi,
                            None => continue,
                        };
                        let score = current.score.wrapping_add_signed(
                            current.swap_delta(ci, ci_at_sj, cj, cj_at_si, input, &pre),
                        );
                        if score < best_score {
                            best_score = score;
                            best = Some(Neighbor {
                                moves: vec![(ci, Some(ci_at_sj)), (cj, Some(cj_at_si))],
                                score,
                            });
                        }
                    }
                }
            }
        }
    }

    // ── Move 2: Consolidate small stores (delta scoring, O(1) per candidate) ─
    let small_stores: Vec<usize> = current
        .store_card_counts
        .iter()
        .enumerate()
        .filter(|&(_, &count)| (1..=2).contains(&count))
        .map(|(si, _)| si)
        .collect();

    for &from_si in &small_stores {
        for &ci in &cards_at[from_si] {
            for (alt_oi, alt_opt) in input.cards[ci].options.iter().enumerate() {
                if alt_opt.store_idx == from_si {
                    continue;
                }
                let delta =
                    current.try_single_move_delta_pre(ci, Some(alt_oi), input, config, &pre);
                let score = current.score.wrapping_add_signed(delta);
                if score < best_score {
                    best_score = score;
                    best = Some(Neighbor {
                        moves: vec![(ci, Some(alt_oi))],
                        score,
                    });
                }
            }
            // Try skipping the card instead.
            let delta = current.try_single_move_delta_pre(ci, None, input, config, &pre);
            let score = current.score.wrapping_add_signed(delta);
            if score < best_score {
                best_score = score;
                best = Some(Neighbor {
                    moves: vec![(ci, None)],
                    score,
                });
            }
        }
    }

    // ── Move 3: Bulk-merge entire stores ───────────────────────────────────
    // Try moving all cards from store A to store B in one step.  This lets
    // the climber escape local optima where single-card moves can't
    // eliminate a store because each intermediate step looks worse.
    let used_stores: Vec<usize> = current
        .store_card_counts
        .iter()
        .enumerate()
        .filter(|&(_, &c)| c > 0)
        .map(|(si, _)| si)
        .collect();

    for &from_si in &used_stores {
        for &to_si in &used_stores {
            if from_si == to_si {
                continue;
            }

            // Fast pre-check: all cards at from_si must be available at to_si.
            let mut moves: Vec<(usize, usize)> = Vec::with_capacity(cards_at[from_si].len());
            let mut merge_ok = true;
            for &ci in &cards_at[from_si] {
                match input.cheapest_at(ci, to_si) {
                    Some((alt_oi, _)) => moves.push((ci, alt_oi)),
                    None => {
                        merge_ok = false;
                        break;
                    }
                }
            }

            if !merge_ok || moves.is_empty() {
                continue;
            }

            let score = current.merge_score(from_si, to_si, &moves, input, config, &pre);
            if score < best_score {
                best_score = score;
                let moves: Vec<(usize, Option<usize>)> =
                    moves.into_iter().map(|(ci, oi)| (ci, Some(oi))).collect();
                best = Some(Neighbor { moves, score });
            }
        }
    }

    best
}

// ── Simulated annealing ──────────────────────────────────────────────────────

/// Pick a uniformly-random single-card move that differs from the current choice.
/// Returns `None` if the picked card has no alternatives.
fn random_move(
    current: &ScoredAssignment,
    input: &WizardInput,
    rng: &mut impl Rng,
) -> Option<(usize, Option<usize>)> {
    let n = input.card_count();
    let ci = rng.gen_range(0..n);
    let card = &input.cards[ci];
    let cur = current.choices[ci];

    // Total distinct choices: every available option + skip
    let total_choices = card.options.len() + 1;
    if total_choices <= 1 {
        return None; // card has no alternatives
    }

    loop {
        let pick = rng.gen_range(0..total_choices);
        let new_oi = if pick < card.options.len() {
            Some(pick)
        } else {
            None // skip
        };
        if new_oi != cur {
            return Some((ci, new_oi));
        }
    }
}

/// Run simulated annealing starting from `initial`.  Accepts worsening moves
/// with probability exp(-Δ/T) where T starts at `SA_INITIAL_TEMP` and decays
/// geometrically by `cooling_rate` each step.  Returns the best assignment
/// encountered during the walk.
fn simulated_annealing_with(
    initial: &ScoredAssignment,
    input: &WizardInput,
    config: &WizardConfig,
    rng: &mut impl Rng,
    cooling_rate: f64,
) -> ScoredAssignment {
    let mut current = initial.clone();
    let mut best_choices: Option<Vec<Option<usize>>> = None;
    let mut best_score = current.score;
    let mut temp = SA_INITIAL_TEMP;

    while temp > SA_MIN_TEMP {
        let Some((ci, new_oi)) = random_move(&current, input, rng) else {
            temp *= cooling_rate;
            continue;
        };

        let delta = current.try_single_move_delta(ci, new_oi, input, config) as f64;

        // Accept if improving, or probabilistically if worsening
        if delta <= 0.0 || rng.gen::<f64>() < (-delta / temp).exp() {
            current.apply_single_move(ci, new_oi, input, config);
            if current.score < best_score {
                best_score = current.score;
                best_choices = Some(current.choices.clone());
            }
        }

        temp *= cooling_rate;
    }

    // Reconstruct the best assignment from stored choices if one was found
    // (avoids cloning the full ScoredAssignment on every improvement step).
    if let Some(choices) = best_choices {
        let raw = Assignment { choices };
        let reconstructed = ScoredAssignment::new(raw, input, config);
        if reconstructed.score <= current.score {
            return reconstructed;
        }
    }
    current
}

// ── Iterated Local Search (ILS) ──────────────────────────────────────────────

// Thread-local buffer for Fisher-Yates indices in `perturb_with_frac`.
// Avoids per-call Vec allocation.
thread_local! {
    static PERTURB_INDICES: std::cell::RefCell<Vec<usize>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Perturb an assignment by randomly reassigning a fraction of cards to
/// different options (including skipping), producing a new starting point
/// for another hill-climb.
fn perturb_with_frac(
    current: &ScoredAssignment,
    input: &WizardInput,
    config: &WizardConfig,
    rng: &mut impl Rng,
    fraction: f64,
) -> ScoredAssignment {
    let mut perturbed = current.clone();
    let n = input.card_count();
    let perturb_count = ((n as f64) * fraction).ceil() as usize;

    // Pick perturb_count distinct card indices via Fisher-Yates partial shuffle.
    // Use thread-local buffer to avoid per-call Vec allocation.
    PERTURB_INDICES.with(|buf| {
        let mut indices = buf.borrow_mut();
        indices.clear();
        indices.extend(0..n);
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
    });

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

    // Fast path: use the cached reverse index from ScoredAssignment.
    let cards_at_store = &current.cards_at[from_si];

    if cards_at_store.is_empty() {
        return None;
    }

    // Quick feasibility check: at least one card must have an alternative store.
    let can_consolidate = cards_at_store.iter().any(|&ci| {
        input.cards[ci]
            .options
            .iter()
            .any(|opt| opt.store_idx != from_si)
    });
    if !can_consolidate {
        return None;
    }

    let mut result = current.clone();
    let mut any_moved = false;

    for &ci in cards_at_store {
        let card = &input.cards[ci];
        // Find the cheapest option at any store other than from_si.
        let mut best_alt: Option<(usize, u32)> = None;
        for (oi, opt) in card.options.iter().enumerate() {
            if opt.store_idx != from_si && best_alt.is_none_or(|(_, p)| opt.price < p) {
                best_alt = Some((oi, opt.price));
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

// ── Hill-climbing helper ────────────────────────────────────────────────────

/// Hill-climb from `current` in-place, returning the local optimum.
/// Repeatedly applies the best available single-card / swap / merge move
/// until no improving neighbor exists.
#[inline]
fn hill_climb(
    mut current: ScoredAssignment,
    input: &WizardInput,
    config: &WizardConfig,
) -> ScoredAssignment {
    loop {
        let neighbor = best_neighbor(&current, input, config);
        match neighbor {
            Some(n) if n.score < current.score => {
                current.apply_moves(&n.moves, input, config);
                debug_assert_eq!(current.score, n.score, "analytic neighbor score mismatch");
            }
            _ => break,
        }
    }
    current
}

// ── Solution bookkeeping ─────────────────────────────────────────────────────

fn build_solution(scored: &ScoredAssignment, input: &WizardInput) -> WizardSolution {
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
            input.shipping_cost(
                si,
                scored.store_totals[si] as u64,
                scored.store_card_counts[si],
            ) as u32
        })
        .collect();

    let total_card_cost: u64 = scored.card_total;
    let total_shipping: u64 = shipping_costs.iter().map(|&v| v as u64).sum();

    // Build assignments: found cards first, skipped at end
    let mut found: Vec<CardAssignment> = Vec::new();
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
        raw_choices: scored.choices.clone(),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn test_input() -> WizardInput {
        WizardInput {
            store_names: vec!["s0".into(), "s1".into(), "s2".into()],
            cards: vec![
                WizardCard {
                    name: "c0".into(),
                    options: vec![
                        CardOption {
                            store_idx: 0,
                            price: 100,
                            url: "u".into(),
                        },
                        CardOption {
                            store_idx: 1,
                            price: 90,
                            url: "u".into(),
                        },
                        CardOption {
                            store_idx: 2,
                            price: 95,
                            url: "u".into(),
                        },
                    ],
                },
                WizardCard {
                    name: "c1".into(),
                    options: vec![
                        CardOption {
                            store_idx: 0,
                            price: 80,
                            url: "u".into(),
                        },
                        CardOption {
                            store_idx: 1,
                            price: 70,
                            url: "u".into(),
                        },
                    ],
                },
                WizardCard {
                    name: "c2".into(),
                    options: vec![
                        CardOption {
                            store_idx: 0,
                            price: 60,
                            url: "u".into(),
                        },
                        CardOption {
                            store_idx: 2,
                            price: 55,
                            url: "u".into(),
                        },
                    ],
                },
            ],
            shipping_bases: vec![50, 80, 0],
            shipping_free_thresholds: vec![100, 0, 0],
            min_orders: vec![0, 0, 50],
            shipping_card_limits: vec![0, 2, 0],
            shipping_card_surcharges: vec![0, 30, 0],
            prices: vec![],
            option_indices: vec![],
            store_cards: vec![Vec::new(); 3],
        }
    }

    fn move_info(card: &WizardCard, old_oi: Option<usize>, new_oi: Option<usize>) -> MoveInfo {
        let old = old_oi.map(|oi| &card.options[oi]);
        let new = new_oi.map(|oi| &card.options[oi]);
        MoveInfo {
            old_price: old.map_or(0, |o| o.price),
            new_price: new.map_or(0, |o| o.price),
            old_si: old.map(|o| o.store_idx),
            new_si: new.map(|o| o.store_idx),
        }
    }

    #[test]
    fn delta_pre_matches_reference() {
        for strategy in [Strategy::Cheapest, Strategy::Simplest] {
            let input = test_input();
            let config = WizardConfig {
                strategy,
                tolerance: 1,
            };
            let choices = vec![Some(1), Some(0), None];
            let scored = ScoredAssignment::new(Assignment { choices }, &input, &config);

            let mut store_shipping = vec![0u64; input.store_names.len()];
            for (si, &count) in scored.store_card_counts.iter().enumerate() {
                if count > 0 {
                    store_shipping[si] =
                        input.shipping_cost(si, scored.store_totals[si] as u64, count);
                }
            }
            let pre = DeltaPrecompute {
                base_excess: scored.num_skipped.saturating_sub(config.tolerance),
                base_store_penalty: if matches!(strategy, Strategy::Cheapest) {
                    (scored.num_stores.saturating_sub(1)) as i64 * STORE_PENALTY as i64
                } else {
                    0
                },
                store_shipping,
            };

            for (ci, card) in input.cards.iter().enumerate() {
                let old_oi = scored.choices[ci];
                let hoist = scored.move1_hoist(ci, &input, &config, &pre);
                for new_oi_raw in 0..=card.options.len() {
                    let new_oi = if new_oi_raw < card.options.len() {
                        Some(new_oi_raw)
                    } else {
                        None
                    };
                    if old_oi == new_oi {
                        continue;
                    }
                    let info = move_info(card, old_oi, new_oi);
                    let reference = scored.delta_from_info(&info, &input, &config);
                    let fast = scored.delta_from_info_pre(&info, &input, &config, &pre);
                    assert_eq!(
                        reference, fast,
                        "{strategy:?} ci={ci} old={old_oi:?} new={new_oi:?}"
                    );

                    if let Some(oi) = new_oi {
                        let opt = &card.options[oi];
                        let move1 = scored.move1_delta(
                            &hoist,
                            opt.store_idx,
                            opt.price,
                            &input,
                            &config,
                            &pre,
                        );
                        assert_eq!(
                            fast, move1,
                            "move1 {strategy:?} ci={ci} old={old_oi:?} new={new_oi:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn swap_and_merge_scores_match_apply() {
        for strategy in [Strategy::Cheapest, Strategy::Simplest] {
            let input = test_input();
            let config = WizardConfig {
                strategy,
                tolerance: 1,
            };
            // c0 -> store1, c1 -> store0, c2 skipped.  Two used stores to swap/merge.
            let choices = vec![Some(1), Some(0), None];
            let scored = ScoredAssignment::new(Assignment { choices }, &input, &config);

            let mut store_shipping = vec![0u64; input.store_names.len()];
            for (si, &count) in scored.store_card_counts.iter().enumerate() {
                if count > 0 {
                    store_shipping[si] =
                        input.shipping_cost(si, scored.store_totals[si] as u64, count);
                }
            }
            let pre = DeltaPrecompute {
                base_excess: scored.num_skipped.saturating_sub(config.tolerance),
                base_store_penalty: if matches!(strategy, Strategy::Cheapest) {
                    (scored.num_stores.saturating_sub(1)) as i64 * STORE_PENALTY as i64
                } else {
                    0
                },
                store_shipping,
            };

            // Swap c0 (store1) <-> c1 (store0).
            {
                let ci = 0;
                let ci_new_oi = 0; // c0 -> store0
                let cj = 1;
                let cj_new_oi = 1; // c1 -> store1
                let analytic = scored.score.wrapping_add_signed(
                    scored.swap_delta(ci, ci_new_oi, cj, cj_new_oi, &input, &pre),
                );
                let mut nb = scored.clone();
                nb.apply_single_move(ci, Some(ci_new_oi), &input, &config);
                nb.apply_single_move(cj, Some(cj_new_oi), &input, &config);
                assert_eq!(nb.score, analytic, "swap {strategy:?}");
            }

            // Merge store0 (holds c1) into store1.
            {
                let from_si = 0;
                let to_si = 1;
                let moves = vec![(1usize, 1usize)]; // c1 -> store1 cheapest
                let analytic = scored.merge_score(from_si, to_si, &moves, &input, &config, &pre);
                let mut nb = scored.clone();
                for &(ci, oi) in &moves {
                    nb.apply_single_move(ci, Some(oi), &input, &config);
                }
                assert_eq!(nb.score, analytic, "merge {strategy:?}");
            }
        }
    }
}
