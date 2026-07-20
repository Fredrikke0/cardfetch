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
use indicatif::{ProgressBar, ProgressStyle};
use rand::rngs::StdRng;
use rand::Rng;
use rand::SeedableRng;
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::AtomicU64;

/// Incremented by `SubsetEvaluator::try_subset` on every combo tested.
/// Server mode reads this to report exhaustive wizard progress.
pub(crate) static EXHAUSTIVE_COMBO_COUNT: AtomicU64 = AtomicU64::new(0);

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

/// Values that are constant for the duration of a `best_neighbor` scan.
/// Computing them once and threading them into `delta_from_info` avoids
/// redundant `saturating_sub` and multiplication in the inner loop.
struct DeltaPrecompute {
    base_excess: usize,
    base_store_penalty: i64, // Cheapest strategy only; zero for Simplest
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
    #[allow(clippy::too_many_arguments)]
    fn try_single_move_delta_pre_fast(
        &self,
        ci: usize,
        old_price: u32,
        old_si: Option<usize>,
        new_oi: Option<usize>,
        input: &WizardInput,
        config: &WizardConfig,
        pre: &DeltaPrecompute,
    ) -> i64 {
        let card = &input.cards[ci];
        let new_price = new_oi.map_or(0, |oi| card.options[oi].price);
        let new_si = new_oi.map(|oi| card.options[oi].store_idx);
        let info = MoveInfo {
            old_price,
            new_price,
            old_si,
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
/// score_store_subset) funnels through this one function to guarantee
/// consistency across the exhaustive and heuristic optimizers.
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
    /// Enumerate store combinations from a pruned candidate pool and greedily
    /// assign cards within each combination.  Candidates are pre-computed by
    /// `select_candidate_stores` and reused across tolerance levels.
    Exhaustive { candidates: Vec<usize> },
}

/// Maximum number of stores to try in a combination during exhaustive search.
const EXHAUSTIVE_MAX_STORES: usize = 5;

// ── Candidate store selection ──────────────────────────────────────────────────

/// Compute the strategy-specific base score contribution for a k-store subset.
/// This is the part of the score that depends only on k (not on which stores
/// or cards are chosen), used for lower-bound rejection in exhaustive search.
#[inline]
fn k_base_score(k: usize, strategy: Strategy) -> u64 {
    match strategy {
        Strategy::Simplest => (k as u64) * PRICE_WEIGHT,
        Strategy::Cheapest => (k.saturating_sub(1) as u64) * STORE_PENALTY,
    }
}

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
    let n_cards = input.card_count();
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
            let sj_dominates_si = (0..n_cards).all(|ci| match input.cheapest_at(ci, si) {
                None => true,
                Some((_, si_price)) => input
                    .cheapest_at(ci, sj)
                    .is_some_and(|(_, sj_price)| sj_price <= si_price),
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

/// Estimate total store combinations for a candidate set, matching the
/// calculation in `optimize_exhaustive`.  Used by server mode to report
/// progress (combos_total).
pub(crate) fn exhaustive_combo_total(candidates: &[usize]) -> u64 {
    let max_stores = EXHAUSTIVE_MAX_STORES.min(candidates.len());
    (1..=max_stores)
        .map(|k| {
            let nf = candidates.len() as u64;
            let mut c = 1u64;
            for i in 0..k as u64 {
                c = c * (nf - i) / (i + 1);
            }
            c
        })
        .sum()
}

/// Maximum number of top solutions to keep per tolerance level.
const TOP_N: usize = 3;

/// Score a candidate subset during exhaustive search with O(1) lower-bound
/// rejection, progress-bar flushing, and best-tracking.
///
/// This is the inner loop of the exhaustive search, shared by k=4 parallel,
/// k=5 parallel, and the generic sequential sections.
struct SubsetEvaluator<'a> {
    input: &'a WizardInput,
    config: &'a WizardConfig,
    choices_buf: &'a mut [Option<usize>],
    totals_buf: &'a mut [u64],
    global_mins_sum: u64,
    min_shipping_base: u64,
    /// Sorted best scores (ascending), up to TOP_N entries.
    /// Each entry: (subset, choices, score).
    local_best: Vec<(Vec<usize>, Vec<Option<usize>>, u64)>,
    bar_acc: u64,
    bar: &'a ProgressBar,
}

impl<'a> SubsetEvaluator<'a> {
    fn new(
        input: &'a WizardInput,
        config: &'a WizardConfig,
        choices_buf: &'a mut [Option<usize>],
        totals_buf: &'a mut [u64],
        global_mins_sum: u64,
        min_shipping_base: u64,
        bar: &'a ProgressBar,
    ) -> Self {
        Self {
            input,
            config,
            choices_buf,
            totals_buf,
            global_mins_sum,
            min_shipping_base,
            local_best: Vec::with_capacity(TOP_N),
            bar_acc: 0,
            bar,
        }
    }

    /// Worst score among the top N so far (u64::MAX if fewer than N).
    fn worst_best_score(&self) -> u64 {
        if self.local_best.len() >= TOP_N {
            self.local_best
                .last()
                .map(|(_, _, s)| *s)
                .unwrap_or(u64::MAX)
        } else {
            u64::MAX
        }
    }

    #[inline]
    fn try_subset(&mut self, subset: &[usize]) {
        self.bar_acc += 1;
        EXHAUSTIVE_COMBO_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if self.bar_acc >= 100_000 {
            self.bar.inc(self.bar_acc);
            self.bar_acc = 0;
        }

        let k_base = k_base_score(subset.len(), self.config.strategy);
        if self.global_mins_sum + self.min_shipping_base + k_base >= self.worst_best_score() {
            return;
        }

        let score = score_store_subset(
            self.input,
            self.config,
            subset,
            self.choices_buf,
            self.totals_buf,
        );

        // Insert into sorted top-N list.
        let entry = (subset.to_vec(), self.choices_buf.to_vec(), score);
        let pos = self.local_best.partition_point(|(_, _, s)| *s < score);
        if pos < TOP_N {
            self.local_best.insert(pos, entry);
            self.local_best.truncate(TOP_N);
        }
    }

    fn finish(self) -> Vec<(Vec<usize>, Vec<Option<usize>>, u64)> {
        if self.bar_acc > 0 {
            self.bar.inc(self.bar_acc);
        }
        self.local_best
    }
}

/// Score a specific store subset: greedily assigns each card to the cheapest
/// store in the subset that carries it (or skips the card).
///
/// Writes the resulting option choices into `choices_out` (must be at least
/// `input.card_count()` elements) and card subtotals into `store_totals_out`
/// (must be at least `store_subset.len()` elements; the first k entries are
/// zeroed on entry).  Returns the composite score.
///
/// The caller owns the buffers and can reuse them across calls to avoid
/// per-combination allocations.
#[allow(clippy::needless_range_loop)]
fn score_store_subset(
    input: &WizardInput,
    config: &WizardConfig,
    store_subset: &[usize],
    choices_out: &mut [Option<usize>],
    store_totals_out: &mut [u64],
) -> u64 {
    let n = input.card_count();
    let k = store_subset.len();
    debug_assert!(choices_out.len() >= n);
    debug_assert!(store_totals_out.len() >= k);

    store_totals_out[..k].fill(0);
    // Track card counts per store for shipping tier calculation.
    let mut card_counts = [0usize; 6];
    debug_assert!(k <= 6);
    let mut num_skipped = 0usize;

    // Unrolled fast paths for common k values — avoids loop overhead
    // and enumerate() per card.  option_indices are only read for the
    // winning store, saving (k-1)/k of the oi memory bandwidth.
    if k == 3 {
        let s0 = store_subset[0];
        let s1 = store_subset[1];
        let s2 = store_subset[2];
        let n_st = input.store_names.len();
        for ci in 0..n {
            let off = ci * n_st;
            let p0 = input.prices[off + s0];
            let p1 = input.prices[off + s1];
            let p2 = input.prices[off + s2];
            let mut best_price = u32::MAX;
            let mut best_oi = None;
            let mut best_pos = 0usize;
            if p0 < best_price {
                best_price = p0;
                best_oi = Some(input.option_indices[off + s0] as usize);
            }
            if p1 < best_price {
                best_price = p1;
                best_oi = Some(input.option_indices[off + s1] as usize);
                best_pos = 1;
            }
            if p2 < best_price {
                best_price = p2;
                best_oi = Some(input.option_indices[off + s2] as usize);
                best_pos = 2;
            }
            choices_out[ci] = best_oi;
            if best_price != u32::MAX {
                store_totals_out[best_pos] += best_price as u64;
                card_counts[best_pos] += 1;
            } else {
                num_skipped += 1;
            }
        }
    } else if k == 4 {
        let s0 = store_subset[0];
        let s1 = store_subset[1];
        let s2 = store_subset[2];
        let s3 = store_subset[3];
        let n_st = input.store_names.len();
        for ci in 0..n {
            let off = ci * n_st;
            let p0 = input.prices[off + s0];
            let p1 = input.prices[off + s1];
            let p2 = input.prices[off + s2];
            let p3 = input.prices[off + s3];

            let mut best_price = u32::MAX;
            let mut best_oi = None;
            let mut best_pos = 0usize;

            if p0 < best_price {
                best_price = p0;
                best_oi = Some(input.option_indices[off + s0] as usize);
            }
            if p1 < best_price {
                best_price = p1;
                best_oi = Some(input.option_indices[off + s1] as usize);
                best_pos = 1;
            }
            if p2 < best_price {
                best_price = p2;
                best_oi = Some(input.option_indices[off + s2] as usize);
                best_pos = 2;
            }
            if p3 < best_price {
                best_price = p3;
                best_oi = Some(input.option_indices[off + s3] as usize);
                best_pos = 3;
            }

            choices_out[ci] = best_oi;
            if best_price != u32::MAX {
                store_totals_out[best_pos] += best_price as u64;
                card_counts[best_pos] += 1;
            } else {
                num_skipped += 1;
            }
        }
    } else {
        let n_st = input.store_names.len();
        for ci in 0..n {
            let mut best_price = u32::MAX;
            let mut best_oi = None;
            let mut best_pos = 0usize;
            let off = ci * n_st;
            for (pos, &si) in store_subset.iter().enumerate() {
                let p = input.prices[off + si];
                if p < best_price {
                    best_price = p;
                    best_oi = Some(input.option_indices[off + si] as usize);
                    best_pos = pos;
                }
            }
            choices_out[ci] = best_oi;
            if best_price != u32::MAX {
                store_totals_out[best_pos] += best_price as u64;
                card_counts[best_pos] += 1;
            } else {
                num_skipped += 1;
            }
        }
    }

    let card_total: u64 = store_totals_out[..k].iter().sum();
    let shipping: u64 = store_subset
        .iter()
        .zip(store_totals_out[..k].iter())
        .zip(card_counts[..k].iter())
        .map(|((&si, &total), &count)| input.shipping_cost(si, total, count))
        .sum();

    let num_stores_used = card_counts[..k].iter().filter(|&&c| c > 0).count();
    compute_raw_score(
        card_total + shipping,
        num_stores_used,
        num_skipped,
        config.tolerance,
        config.strategy,
    )
}

/// Exhaustively search for the optimal store assignment by enumerating
/// store combinations from a pruned candidate pool.
///
/// `candidates` should be pre-computed by `select_candidate_stores` and
/// reused across tolerance levels — it depends only on `input`, not on
/// `config`.
fn optimize_exhaustive(
    input: &WizardInput,
    config: &WizardConfig,
    candidates: &[usize],
) -> Option<Vec<WizardSolution>> {
    let n = input.card_count();
    let n_stores = input.store_names.len();

    let max_stores = EXHAUSTIVE_MAX_STORES.min(candidates.len());

    // Estimate total combinations for the progress bar.
    let total_combos: u64 = (1..=max_stores)
        .map(|k| {
            let nf = candidates.len() as u64;
            let mut c = 1u64;
            for i in 0..k as u64 {
                c = c * (nf - i) / (i + 1);
            }
            c
        })
        .sum();

    eprintln!(
        "  [exhaustive] {} cards x {} stores -> {} candidates, {} combos (max {} stores)",
        n,
        n_stores,
        candidates.len(),
        total_combos,
        max_stores,
    );
    if candidates.len() > 500 {
        eprintln!(
            "  [exhaustive] Very large candidate pool ({}). Consider filtering with --stores.",
            candidates.len()
        );
    }

    // Precompute global minima for O(1) lower-bound rejection.
    // Even if every card got its global best price from one store with
    // minimum shipping, can a k-store combo beat the current best?  If
    // not, the full O(n*k) scoring is skipped.  As local_best_score
    // improves, more combos are rejected for free.
    let global_mins_sum: u64 = (0..n)
        .map(|ci| {
            candidates
                .iter()
                .filter_map(|&si| input.cheapest_at(ci, si).map(|(_, p)| p as u64))
                .min()
                .unwrap_or(0)
        })
        .sum();
    let min_shipping_base: u64 = candidates
        .iter()
        .map(|&si| input.shipping_bases[si] as u64)
        .min()
        .unwrap_or(0);

    let bar = ProgressBar::new(total_combos).with_style(
        ProgressStyle::with_template(
            "{spinner:.green} [{elapsed_precise}] [{bar:30.cyan/blue}] {pos}/{len} ({per_sec} combo/s)",
        )
        .unwrap()
        .progress_chars("#>-"),
    );

    // Process each k in parallel; within each k, generate combinations
    // sequentially on-the-fly to avoid pre-allocating millions of Vecs.
    //
    // k=4 dominates combinatorially (~99.9% of combos), so we parallelize
    // it over the first combination index `a`.  k=1..3 use the existing
    // sequential lexicographic enumeration with reusable buffers.

    // ── k=4: parallelize over the first index ───────────────────────────
    // Use thread-local buffers to avoid per-task Vec allocation.
    thread_local! {
        static CHOICES_BUF: std::cell::RefCell<Vec<Option<usize>>> =
            const { std::cell::RefCell::new(Vec::new()) };
        static CHOICES_BUF_K5: std::cell::RefCell<Vec<Option<usize>>> =
            const { std::cell::RefCell::new(Vec::new()) };
        static CHOICES_BUF_K6: std::cell::RefCell<Vec<Option<usize>>> =
            const { std::cell::RefCell::new(Vec::new()) };
    }
    // Helper: merge two top-N lists, keeping only the best N entries.
    fn merge_top_n(
        a: &mut Vec<(Vec<usize>, Vec<Option<usize>>, u64)>,
        b: &mut Vec<(Vec<usize>, Vec<Option<usize>>, u64)>,
    ) {
        a.append(b);
        a.sort_by_key(|(_, _, s)| *s);
        a.truncate(TOP_N);
    }

    let best_k4: Vec<(Vec<usize>, Vec<Option<usize>>, u64)> = if max_stores >= 4 {
        let n_c = candidates.len();
        let n_a = n_c.saturating_sub(4) + 1;
        let card_count = input.card_count();
        (0..n_a)
            .into_par_iter()
            .with_min_len(1) // fine-grained splitting for load balance
            .fold(
                Vec::new,
                |mut acc: Vec<(Vec<usize>, Vec<Option<usize>>, u64)>, a| {
                    CHOICES_BUF.with(|buf| {
                        let mut choices_buf = buf.borrow_mut();
                        choices_buf.resize(card_count, None);
                        let mut totals_buf = [0u64; 4];
                        let mut eval = SubsetEvaluator::new(
                            input,
                            config,
                            &mut choices_buf,
                            &mut totals_buf,
                            global_mins_sum,
                            min_shipping_base,
                            &bar,
                        );

                        for b in (a + 1)..=(n_c.saturating_sub(3)) {
                            for c in (b + 1)..=(n_c.saturating_sub(2)) {
                                for d in (c + 1)..=(n_c.saturating_sub(1)) {
                                    eval.try_subset(&[
                                        candidates[a],
                                        candidates[b],
                                        candidates[c],
                                        candidates[d],
                                    ]);
                                }
                            }
                        }

                        acc.extend(eval.finish());
                        acc.sort_by_key(|(_, _, s)| *s);
                        acc.truncate(TOP_N);
                        acc
                    })
                },
            )
            .reduce(Vec::new, |mut a, mut b| {
                merge_top_n(&mut a, &mut b);
                a
            })
    } else {
        Vec::new()
    };

    let best_k5: Vec<(Vec<usize>, Vec<Option<usize>>, u64)> = if max_stores >= 5 {
        let n_c = candidates.len();
        let n_a = n_c.saturating_sub(5) + 1;
        let card_count = input.card_count();
        (0..n_a)
            .into_par_iter()
            .with_min_len(1)
            .fold(
                Vec::new,
                |mut acc: Vec<(Vec<usize>, Vec<Option<usize>>, u64)>, a| {
                    CHOICES_BUF_K5.with(|buf| {
                        let mut choices_buf = buf.borrow_mut();
                        choices_buf.resize(card_count, None);
                        let mut totals_buf = [0u64; 5];
                        let mut eval = SubsetEvaluator::new(
                            input,
                            config,
                            &mut choices_buf,
                            &mut totals_buf,
                            global_mins_sum,
                            min_shipping_base,
                            &bar,
                        );

                        for b in (a + 1)..=(n_c.saturating_sub(4)) {
                            for c in (b + 1)..=(n_c.saturating_sub(3)) {
                                for d in (c + 1)..=(n_c.saturating_sub(2)) {
                                    for e in (d + 1)..=(n_c.saturating_sub(1)) {
                                        eval.try_subset(&[
                                            candidates[a],
                                            candidates[b],
                                            candidates[c],
                                            candidates[d],
                                            candidates[e],
                                        ]);
                                    }
                                }
                            }
                        }

                        acc.extend(eval.finish());
                        acc.sort_by_key(|(_, _, s)| *s);
                        acc.truncate(TOP_N);
                        acc
                    })
                },
            )
            .reduce(Vec::new, |mut a, mut b| {
                merge_top_n(&mut a, &mut b);
                a
            })
    } else {
        Vec::new()
    };

    let best_k6: Vec<(Vec<usize>, Vec<Option<usize>>, u64)> = if max_stores >= 6 {
        let n_c = candidates.len();
        let n_a = n_c.saturating_sub(6) + 1;
        let card_count = input.card_count();
        (0..n_a)
            .into_par_iter()
            .with_min_len(1)
            .fold(
                Vec::new,
                |mut acc: Vec<(Vec<usize>, Vec<Option<usize>>, u64)>, a| {
                    CHOICES_BUF_K6.with(|buf| {
                        let mut choices_buf = buf.borrow_mut();
                        choices_buf.resize(card_count, None);
                        let mut totals_buf = [0u64; 6];
                        let mut eval = SubsetEvaluator::new(
                            input,
                            config,
                            &mut choices_buf,
                            &mut totals_buf,
                            global_mins_sum,
                            min_shipping_base,
                            &bar,
                        );

                        for b in (a + 1)..=(n_c.saturating_sub(5)) {
                            for c in (b + 1)..=(n_c.saturating_sub(4)) {
                                for d in (c + 1)..=(n_c.saturating_sub(3)) {
                                    for e in (d + 1)..=(n_c.saturating_sub(2)) {
                                        for f in (e + 1)..=(n_c.saturating_sub(1)) {
                                            eval.try_subset(&[
                                                candidates[a],
                                                candidates[b],
                                                candidates[c],
                                                candidates[d],
                                                candidates[e],
                                                candidates[f],
                                            ]);
                                        }
                                    }
                                }
                            }
                        }

                        acc.extend(eval.finish());
                        acc.sort_by_key(|(_, _, s)| *s);
                        acc.truncate(TOP_N);
                        acc
                    })
                },
            )
            .reduce(Vec::new, |mut a, mut b| {
                merge_top_n(&mut a, &mut b);
                a
            })
    } else {
        Vec::new()
    };

    // ── k=1..3 and k=7+: sequential lexicographic enumeration ───────────
    // Uses thread-local buffer for choices_out (same as k=4/k=5 sections).
    let best_small: Vec<(Vec<usize>, Vec<Option<usize>>, u64)> = (1..=max_stores)
        .into_par_iter()
        .fold(
            Vec::new,
            |mut acc: Vec<(Vec<usize>, Vec<Option<usize>>, u64)>, k| {
                if k == 4 || k == 5 || k == 6 {
                    return acc;
                }
                let n_c = candidates.len();
                if k > n_c {
                    return acc;
                }

                let card_count = input.card_count();
                CHOICES_BUF.with(|buf| {
                    let mut choices_buf = buf.borrow_mut();
                    choices_buf.resize(card_count, None);
                    // totals_buf is small (max 5 elements) — keep on stack.
                    let mut totals_buf = [0u64; EXHAUSTIVE_MAX_STORES];
                    let mut subset_buf: Vec<usize> = Vec::with_capacity(k);
                    let mut eval = SubsetEvaluator::new(
                        input,
                        config,
                        &mut choices_buf,
                        &mut totals_buf,
                        global_mins_sum,
                        min_shipping_base,
                        &bar,
                    );
                    let mut combo: Vec<usize> = (0..k).collect();

                    loop {
                        subset_buf.clear();
                        for &i in &combo {
                            subset_buf.push(candidates[i]);
                        }

                        eval.try_subset(&subset_buf);

                        // Next combination (lexicographic order).
                        let mut i = k;
                        while i > 0 {
                            i -= 1;
                            if combo[i] != i + n_c - k {
                                break;
                            }
                        }
                        if i == 0 && combo[0] == n_c - k {
                            break;
                        }
                        combo[i] += 1;
                        for j in (i + 1)..k {
                            combo[j] = combo[j - 1] + 1;
                        }
                    }

                    acc.extend(eval.finish());
                    acc.sort_by_key(|(_, _, s)| *s);
                    acc.truncate(TOP_N);
                    acc
                }) // CHOICES_BUF.with()
            },
        )
        .reduce(Vec::new, |mut a, mut b| {
            merge_top_n(&mut a, &mut b);
            a
        });

    // ── Merge results ───────────────────────────────────────────────────
    let mut merged: Vec<(Vec<usize>, Vec<Option<usize>>, u64)> = Vec::new();
    let mut parts = [best_k4, best_k5, best_k6, best_small];
    for part in &mut parts {
        merged.append(part);
    }
    merged.sort_by_key(|(_, _, s)| *s);
    // Deduplicate by card assignment — different store subsets can
    // produce the same greedy assignment.  Keep only the best-scoring
    // instance of each unique choice vector.
    let mut uniq: Vec<Vec<Option<usize>>> = Vec::with_capacity(merged.len());
    merged.retain(|(_, choices, _)| {
        if uniq.iter().any(|u| u == choices) {
            false
        } else {
            uniq.push(choices.clone());
            true
        }
    });
    merged.truncate(TOP_N);

    bar.finish();

    let solutions: Vec<WizardSolution> = merged
        .into_iter()
        .map(|(_subset, choices, _score)| solution_from_choices(&choices, input, config))
        .collect();

    if solutions.is_empty() {
        None
    } else {
        Some(solutions)
    }
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
/// **Exhaustive mode**: enumerates all k-combinations of candidate stores
/// for k=1..5, greedily assigning each card to the cheapest store in the
/// subset.  Note that the card assignment within each combination is greedy
/// (not globally optimal), so the result is a lower bound on the true
/// optimum rather than a proof of optimality.
///
/// **Heuristic mode**:
/// **Phase 1 — Multi-start hill climbing**: 50 random restarts.
/// **Phase 2 — Iterated Local Search + Simulated Annealing**: up to 12
/// iterations, stopping after 3 consecutive non-improvements.
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

    let mut rng = rand::thread_rng();
    let mut best: Option<ScoredAssignment> = None;
    let mut best_score: Option<u64> = None;

    // If seeded from a previous tolerance, use it as a candidate.
    // This guarantees the cost can only go down (or stay equal) as
    // tolerance increases.
    if let Some(choices) = seed {
        let raw = Assignment {
            choices: choices.to_vec(),
        };
        let current = hill_climb(ScoredAssignment::new(raw, input, config), input, config);
        best_score = Some(current.score);
        best = Some(current);
    }

    // ── Phase 1: Multi-start hill climbing (parallel) ────────────────────
    // Front-load effort on unseeded tolerances.  Higher tolerances are
    // seeded from the previous level's solution and need less exploration.
    let num_restarts = (800 / (config.tolerance + 1)).max(50);
    let parallel_best = (0..num_restarts)
        .into_par_iter()
        .map(|seed_idx| {
            let raw = initial_assignment(input, seed_idx);
            hill_climb(ScoredAssignment::new(raw, input, config), input, config)
        })
        .min_by_key(|c| c.score);

    if let Some(par_best) = parallel_best {
        if best_score.is_none_or(|s| par_best.score < s) {
            best_score = Some(par_best.score);
            best = Some(par_best);
        }
    }

    // ── Phase 2: Iterated Local Search ──────────────────────────────────
    let mut current_best = best.clone().expect("phase 1 always produces a solution");
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

    best.map(|a| build_solution(&a, input))
        .into_iter()
        .collect()
}

// ── Initial assignment builders ──────────────────────────────────────────────

fn initial_assignment(input: &WizardInput, seed: usize) -> Assignment {
    // Both strategies start from greedy set cover — this gives a strong
    // baseline with few stores and low shipping.  The strategy-specific
    // score function then guides the hill climb in different directions.
    initial_simplest(input, seed)
}

/// Greedy set cover: repeatedly pick the store covering the most unassigned cards.
#[allow(clippy::needless_range_loop)]
fn initial_simplest(input: &WizardInput, seed: usize) -> Assignment {
    let n = input.card_count();
    let mut assigned = vec![false; n];
    let mut choices: Vec<Option<usize>> = vec![None; n];
    let mut picked = vec![false; input.store_names.len()];

    loop {
        // Find the store that covers the most still-unassigned cards
        let mut best_count = 0;
        let mut candidates: Vec<usize> = Vec::new();

        for si in 0..input.store_names.len() {
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
) -> Option<ScoredAssignment> {
    // Precompute values that are constant for the entire neighbor scan.
    let pre = DeltaPrecompute {
        base_excess: current.num_skipped.saturating_sub(config.tolerance),
        base_store_penalty: if matches!(config.strategy, Strategy::Cheapest) {
            (current.num_stores.saturating_sub(1)) as i64 * STORE_PENALTY as i64
        } else {
            0
        },
    };

    let mut best_delta: i64 = 0;
    let mut best_move: Option<(usize, Option<usize>)> = None;
    let n = input.card_count();

    // ── Move 1: Reassign a single card (delta scoring, O(1) per candidate) ─
    let n_st = input.store_names.len();
    for ci in 0..n {
        let card = &input.cards[ci];
        let cur = current.choices[ci];

        // Hoist old price & store to avoid per-option card.options[] lookups.
        let old_price = cur.map_or(0, |oi| card.options[oi].price);
        let old_si = cur.map(|oi| card.options[oi].store_idx);

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
            let delta = current.try_single_move_delta_pre_fast(
                ci,
                old_price,
                old_si,
                Some(oi),
                input,
                config,
                &pre,
            );
            if delta < best_delta {
                best_delta = delta;
                best_move = Some((ci, Some(oi)));
            }
        }

        if cur.is_some() {
            let delta = current
                .try_single_move_delta_pre_fast(ci, old_price, old_si, None, input, config, &pre);
            if delta < best_delta {
                best_delta = delta;
                best_move = Some((ci, None));
            }
        }

        if cur.is_none() && !card.options.is_empty() {
            // Options are sorted by price, so [0] is the global cheapest.
            let delta =
                current.try_single_move_delta_pre_fast(ci, 0, None, Some(0), input, config, &pre);
            if delta < best_delta {
                best_delta = delta;
                best_move = Some((ci, Some(0)));
            }
        }
    }

    // Build the best result seen so far from the best single-card move (Move 1).
    // Subsequent move types (swap, consolidate, bulk-merge) compete against this.
    let mut best_result: Option<ScoredAssignment> = best_move.map(|(ci, new_oi)| {
        let mut nb = current.clone();
        nb.apply_single_move(ci, new_oi, input, config);
        nb
    });
    let mut best_score = best_result.as_ref().map_or(current.score, |r| r.score);

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
                        Some((oi, _)) => Some(oi),
                        None => continue,
                    };
                    let delta_ci =
                        current.try_single_move_delta_pre(ci, ci_at_sj, input, config, &pre);
                    // Quick filter: even the first move alone must not be too
                    // expensive (the second move can only help marginally).
                    if (current.score as i64 + delta_ci) >= (best_score as i64) {
                        continue;
                    }
                    for &cj in &cards_at[sj] {
                        let cj_at_si = match input.cheapest_at(cj, si) {
                            Some((oi, _)) => Some(oi),
                            None => continue,
                        };
                        // Combined delta = delta_ci + delta_cj (independent stores).
                        let delta_cj =
                            current.try_single_move_delta_pre(cj, cj_at_si, input, config, &pre);
                        if (current.score as i64 + delta_ci + delta_cj) < (best_score as i64) {
                            let mut nb = current.clone();
                            nb.apply_single_move(ci, ci_at_sj, input, config);
                            nb.apply_single_move(cj, cj_at_si, input, config);
                            best_score = nb.score;
                            best_result = Some(nb);
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
                // Fast O(1) delta check before cloning.
                let delta =
                    current.try_single_move_delta_pre(ci, Some(alt_oi), input, config, &pre);
                if (current.score as i64 + delta) < (best_score as i64) {
                    let mut nb = current.clone();
                    nb.apply_single_move(ci, Some(alt_oi), input, config);
                    best_score = nb.score;
                    best_result = Some(nb);
                }
            }
            // Try skipping the card instead.
            let delta = current.try_single_move_delta_pre(ci, None, input, config, &pre);
            if (current.score as i64 + delta) < (best_score as i64) {
                let mut nb = current.clone();
                nb.apply_single_move(ci, None, input, config);
                best_score = nb.score;
                best_result = Some(nb);
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
            Some(n) if n.score < current.score => current = n,
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
