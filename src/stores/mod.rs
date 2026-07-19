pub mod adamstuen;
pub mod cardmarket;
pub mod collectible;
pub mod finn;
pub mod korthaien;
pub mod midgard;
pub mod outland;
pub mod pokeboks;

use anyhow::Result;

/// Minimum shared delay between requests across all stores (in milliseconds).
pub const DELAY_MS: u64 = 300;
/// Extra random jitter added on top of DELAY_MS (in milliseconds).
pub const DELAY_JITTER_MS: u64 = 300;

/// A single result from a store search for one card.
#[derive(Debug, Clone)]
pub struct StoreResult {
    pub store_name: String,
    pub card_name: String,
    pub price: u32,
    pub url: String,
}

// ── Shared types ────────────────────────────────────────────────────────────

/// A product found during a store search, used internally by store backends.
#[derive(Debug)]
pub(crate) struct SearchProduct {
    pub name: String,
    pub price: u32,
    pub url: String,
    pub in_stock: bool,
}

// ── Shared helpers ──────────────────────────────────────────────────────────

/// Check if `card_name` appears as a case-insensitive substring of `title`.
pub(crate) fn title_contains(card_name: &str, title: &str) -> bool {
    title.to_lowercase().contains(&card_name.to_lowercase())
}

/// Percent-encode spaces as `%20` (used by most stores).
pub(crate) fn urlencode_pct(s: &str) -> String {
    s.replace(' ', "%20")
}

/// Encode spaces as `+` (used by collectible.no and midgardgames.no).
pub(crate) fn urlencode_plus(s: &str) -> String {
    s.replace(' ', "+")
}

/// Convert a card name to a CardMarket URL slug (spaces → hyphens,
/// special characters stripped).
/// E.g. "Evolving Wilds" → "Evolving-Wilds"
///      "Hydra's Growth" → "Hydras-Growth"
///      "Find // Finality" → "Find-Finality"
pub(crate) fn title_to_slug(name: &str) -> String {
    name.trim()
        .replace(" // ", "-")
        .replace("//", "-")
        .replace("'", "")
        .replace(',', "")
        .replace(' ', "-")
}

/// Trait that each store backend implements.
pub trait Store: Send + Sync {
    /// Human-readable store name, used in output.
    fn name(&self) -> &str;

    /// Timeout for HTTP requests to this store (in seconds).
    fn timeout_secs(&self) -> u64;

    /// Cache keys for this store.  Stores that search multiple independent
    /// endpoints (e.g. CardMarket, which has separate Norwegian, international
    /// powerseller, and international private searches) return one key per
    /// endpoint so each can be cached independently.
    fn cache_keys(&self) -> Vec<String> {
        vec![self.name().to_string()]
    }

    /// Search for a single card. Returns zero or more results (empty vec = no match).
    fn search(
        &self,
        client: &reqwest::blocking::Client,
        card_name: &str,
    ) -> Result<Vec<StoreResult>>;

    /// Search only a specific sub-endpoint identified by its cache key.
    /// The default delegates to `search()` — only stores with multiple
    /// `cache_keys()` need to override this.
    fn search_sub(
        &self,
        client: &reqwest::blocking::Client,
        card_name: &str,
        _sub_key: &str,
    ) -> Result<Vec<StoreResult>> {
        self.search(client, card_name)
    }
}

/// Register all store backends here.
pub fn all_stores(verbose: bool) -> Vec<Box<dyn Store>> {
    vec![
        Box::new(outland::Outland::new()),
        Box::new(finn::Finn::new()),
        Box::new(collectible::Collectible::new()),
        Box::new(korthaien::Korthaien::new()),
        Box::new(midgard::Midgard::new()),
        Box::new(pokeboks::Pokeboks::new()),
        Box::new(adamstuen::Adamstuen::new()),
        Box::new(cardmarket::CardMarket::new(verbose)),
    ]
}
