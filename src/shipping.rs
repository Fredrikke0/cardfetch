//! Hardcoded shipping costs, seller blacklists, and currency constants.
//!
//! Centralizes per-store/seller configuration so it can be maintained in one
//! place regardless of whether the consumer is the wizard optimizer or the
//! CardMarket scraper.

// ── Currency & VAT constants ──────────────────────────────────────────────────

/// Hardcoded EUR → NOK conversion rate.
pub(crate) const EUR_TO_NOK: f64 = 11.0;

/// VAT multiplier for international orders (25% on price + shipping).
/// CardMarket adds this for powersellers/professionals shipping to Norway;
/// we include it in the card price, and the wizard adds it to shipping.
pub(crate) const VAT_MULTIPLIER: f64 = 1.25;

/// Customs declaration fee for private international sellers (300 kr in oere).
/// Private sellers don't handle VAT at the border, so Posten charges this
/// flat fee per shipment on top of the 25% import VAT.
pub(crate) const CUSTOMS_FEE: u64 = 30000;

// ── Shipping tiers ────────────────────────────────────────────────────────────

/// Number of cards before shipping switches to a larger envelope/parcel.
pub(crate) const CARD_TIER_THRESHOLD: u32 = 20;

/// Extra shipping cost when exceeding the card tier threshold, for
/// Norwegian CardMarket sellers (30 kr).
pub(crate) const NO_CARD_SURCHARGE: u32 = 3000;

/// Extra shipping cost when exceeding the card tier threshold, for
/// international CardMarket sellers (20 kr).
pub(crate) const INT_CARD_SURCHARGE: u32 = 2000;

// ── Seller blacklist ──────────────────────────────────────────────────────────

/// Sellers that don't ship to Norway — filtered out during scraping.
const BLACKLISTED_SELLERS: &[&str] = &[
    "Itaca",
    "QuestVille-LP",
    "Najada-Games",
    "TCG-entertainment",
    "Goat-enterprise",
    "AsgardBCN",
    "templars-arena",
    "The-Archivist",
    "LGRMasterC",
    "ReCollectibles",
];

/// Check if a seller name is on the blacklist (case-insensitive).
pub(crate) fn is_blacklisted(name: &str) -> bool {
    let lower = name.to_lowercase();
    BLACKLISTED_SELLERS
        .iter()
        .any(|b| lower == b.to_lowercase())
}

// ── Seller name extraction ────────────────────────────────────────────────────

/// Extract the seller name from a store identifier.
/// For CardMarket sellers like "cardmarket-int.com: MTGSPOT-DE",
/// returns "MTGSPOT-DE".  For storefronts like "outland.no",
/// returns the full name as-is.
pub(crate) fn extract_seller_name(store_name: &str) -> &str {
    store_name
        .strip_prefix("cardmarket.com: ")
        .or_else(|| store_name.strip_prefix("cardmarket-int.com: "))
        .or_else(|| store_name.strip_prefix("cardmarket-int-private.com: "))
        .unwrap_or(store_name)
}

// ── Shipping info ─────────────────────────────────────────────────────────────

/// Shipping cost and free-shipping threshold for a single store or seller.
#[derive(Debug, Clone)]
pub(crate) struct ShippingInfo {
    pub(crate) base: u32,
    pub(crate) free_threshold: u32,
    pub(crate) min_order: u32,
    /// Number of cards before a surcharge applies (0 = no tier).
    pub(crate) card_limit: u32,
    /// Extra shipping cost (in oere) when card count exceeds card_limit.
    pub(crate) card_surcharge: u32,
}

/// Look up shipping costs for a store (or seller) by name.
pub(crate) fn shipping_for(store_name: &str) -> ShippingInfo {
    if store_name.starts_with("cardmarket.com:") {
        // Norwegian sellers: 2.8 EUR = 30.80 kr = 3080 oere.
        return ShippingInfo {
            base: 3080,
            free_threshold: 0,
            min_order: 0,
            card_limit: CARD_TIER_THRESHOLD,
            card_surcharge: NO_CARD_SURCHARGE,
        };
    }
    if store_name.starts_with("cardmarket-int-private.com:") {
        // ── Private international: same base as professional, plus customs fee ─
        let seller = store_name
            .strip_prefix("cardmarket-int-private.com: ")
            .unwrap_or("");
        let prof_base = int_professional_shipping_base(seller);
        return ShippingInfo {
            base: (prof_base as u64 + CUSTOMS_FEE) as u32,
            free_threshold: 0,
            min_order: 0,
            card_limit: CARD_TIER_THRESHOLD,
            card_surcharge: INT_CARD_SURCHARGE,
        };
    }
    if store_name.starts_with("cardmarket-int.com:") {
        let seller = store_name
            .strip_prefix("cardmarket-int.com: ")
            .unwrap_or("");
        return ShippingInfo {
            base: int_professional_shipping_base(seller),
            free_threshold: 0,
            min_order: 0,
            card_limit: CARD_TIER_THRESHOLD,
            card_surcharge: INT_CARD_SURCHARGE,
        };
    }
    match store_name {
        "finn.no" => ShippingInfo {
            base: 5000,
            free_threshold: 0,
            min_order: 0,
            card_limit: 0,
            card_surcharge: 0,
        },
        "midgardgames.no" => ShippingInfo {
            base: 10000,
            free_threshold: 0,
            min_order: 0,
            card_limit: 0,
            card_surcharge: 0,
        },
        "outland.no" => ShippingInfo {
            base: 5900,
            free_threshold: 0,
            min_order: 0,
            card_limit: 0,
            card_surcharge: 0,
        },
        "pokeboks.no" => ShippingInfo {
            base: 5000,
            free_threshold: 0,
            min_order: 60000,
            card_limit: 0,
            card_surcharge: 0,
        },
        "korthaien.no" => ShippingInfo {
            base: 5500,
            free_threshold: 0,
            min_order: 0,
            card_limit: 0,
            card_surcharge: 0,
        },
        "adamstuenretro.no" => ShippingInfo {
            base: 5000,
            free_threshold: 0,
            min_order: 0,
            card_limit: 0,
            card_surcharge: 0,
        },
        "collectible.no" => ShippingInfo {
            base: 4900,
            free_threshold: 200000,
            min_order: 0,
            card_limit: 0,
            card_surcharge: 0,
        },
        _ => ShippingInfo {
            base: 0,
            free_threshold: 0,
            min_order: 0,
            card_limit: 0,
            card_surcharge: 0,
        },
    }
}

/// Per-seller base shipping cost for international professional CardMarket sellers
/// (all include 25% VAT).  Extracted so private-seller lookup can reuse it
/// without the recursive allocation.
fn int_professional_shipping_base(seller: &str) -> u32 {
    // Sellers that require tracked parcels: 16 EUR + 25% VAT = 220 kr.
    if seller.eq_ignore_ascii_case("MagicBarcelona") || seller.eq_ignore_ascii_case("Mazvigosl") {
        return 22000;
    }
    // Yardbirds: 8.15 EUR + 25% VAT = 112.06 kr.
    if seller.eq_ignore_ascii_case("Yardbirds") {
        return 11206;
    }
    // HamelinGames: 2.6 EUR + 25% VAT = 35.75 kr.
    if seller.eq_ignore_ascii_case("HamelinGames") {
        return 3575;
    }
    // TrinketMage: 1.55 EUR + 25% VAT = 21.31 kr.
    if seller.eq_ignore_ascii_case("TrinketMage") || seller.eq_ignore_ascii_case("MTGSPOT-DE") {
        return 2131;
    }
    // Default international: 30 kr + 25% VAT = 37.50 kr.
    3750
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_eur_to_nok_conversion() {
        let eur_cents = 1250u32;
        let nok_oere = (eur_cents as f64 * EUR_TO_NOK).round() as u32;
        assert_eq!(nok_oere, 13750);
    }

    #[test]
    fn test_blacklisted_seller_filtered() {
        assert!(is_blacklisted("Itaca"));
        assert!(is_blacklisted("itaca"));
        assert!(is_blacklisted("ITACA"));
        assert!(is_blacklisted("QuestVille-LP"));
        assert!(is_blacklisted("questville-lp"));
        assert!(!is_blacklisted("MagicBarcelona"));
        assert!(!is_blacklisted("SomeOtherSeller"));
    }

    #[test]
    fn test_shipping_lookup_norwegian_seller() {
        let s = shipping_for("cardmarket.com: SomeSeller");
        assert_eq!(s.base, 3080);
    }

    #[test]
    fn test_shipping_lookup_int_default() {
        let s = shipping_for("cardmarket-int.com: UnknownSeller");
        assert_eq!(s.base, 3750);
    }

    #[test]
    fn test_shipping_lookup_mazvigosl() {
        let s = shipping_for("cardmarket-int.com: Mazvigosl");
        assert_eq!(s.base, 22000);
    }

    #[test]
    fn test_shipping_lookup_hamelin() {
        let s = shipping_for("cardmarket-int.com: HamelinGames");
        assert_eq!(s.base, 3575);
    }

    #[test]
    fn test_shipping_lookup_yardbirds() {
        let s = shipping_for("cardmarket-int.com: Yardbirds");
        assert_eq!(s.base, 11206);
    }

    #[test]
    fn test_shipping_lookup_trinketmage() {
        let s = shipping_for("cardmarket-int.com: TrinketMage");
        assert_eq!(s.base, 2131);
    }

    #[test]
    fn test_shipping_lookup_mtgspot_de() {
        let s = shipping_for("cardmarket-int.com: MTGSPOT-DE");
        assert_eq!(s.base, 2131);
    }

    #[test]
    fn test_blacklisted_najada() {
        assert!(is_blacklisted("Najada-Games"));
        assert!(is_blacklisted("najada-games"));
        assert!(is_blacklisted("NAJADA-GAMES"));
    }

    #[test]
    fn test_blacklisted_tcg_entertainment() {
        assert!(is_blacklisted("TCG-entertainment"));
        assert!(is_blacklisted("tcg-entertainment"));
        assert!(is_blacklisted("TCG-ENTERTAINMENT"));
    }

    #[test]
    fn test_blacklisted_goat_enterprise() {
        assert!(is_blacklisted("Goat-enterprise"));
        assert!(is_blacklisted("goat-enterprise"));
        assert!(is_blacklisted("GOAT-ENTERPRISE"));
    }

    #[test]
    fn test_shipping_lookup_private_int_default() {
        let s = shipping_for("cardmarket-int-private.com: UnknownSeller");
        // Default int shipping (3750) + customs fee (30000) = 33750
        assert_eq!(s.base, 33750);
    }

    #[test]
    fn test_shipping_lookup_private_int_mazvigosl() {
        let s = shipping_for("cardmarket-int-private.com: Mazvigosl");
        // Mazvigosl shipping (22000) + customs fee (30000) = 52000
        assert_eq!(s.base, 52000);
    }

    #[test]
    fn test_shipping_lookup_private_int_hamelin() {
        let s = shipping_for("cardmarket-int-private.com: HamelinGames");
        // HamelinGames shipping (3575) + customs fee (30000) = 33575
        assert_eq!(s.base, 33575);
    }
}
