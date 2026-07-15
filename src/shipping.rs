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

// ── Seller blacklist ──────────────────────────────────────────────────────────

/// Sellers that don't ship to Norway — filtered out during scraping.
const BLACKLISTED_SELLERS: &[&str] = &["Itaca", "QuestVille-LP"];

/// Check if a seller name is on the blacklist (case-insensitive).
pub(crate) fn is_blacklisted(name: &str) -> bool {
    let lower = name.to_lowercase();
    BLACKLISTED_SELLERS
        .iter()
        .any(|b| lower == b.to_lowercase())
}

// ── Shipping info ─────────────────────────────────────────────────────────────

/// Shipping cost and free-shipping threshold for a single store or seller.
#[derive(Debug, Clone)]
pub(crate) struct ShippingInfo {
    pub(crate) base: u32,
    pub(crate) free_threshold: u32,
    #[allow(dead_code)]
    min_order: u32,
}

/// Look up shipping costs for a store (or seller) by name.
pub(crate) fn shipping_for(store_name: &str) -> ShippingInfo {
    if store_name.starts_with("cardmarket.com:") {
        // Norwegian sellers: 2.8 EUR = 30.80 kr = 3080 oere.
        return ShippingInfo {
            base: 3080,
            free_threshold: 0,
            min_order: 0,
        };
    }
    if store_name.starts_with("cardmarket-int-private.com:") {
        // ── Private international: same base as professional, plus customs fee ─
        let prof_name =
            store_name.replacen("cardmarket-int-private.com:", "cardmarket-int.com:", 1);
        let prof = shipping_for(&prof_name);
        return ShippingInfo {
            base: (prof.base as u64 + CUSTOMS_FEE) as u32,
            free_threshold: prof.free_threshold,
            min_order: prof.min_order,
        };
    }
    if store_name.starts_with("cardmarket-int.com:") {
        // ── Per-seller international shipping (all include 25% VAT) ───
        let seller = store_name
            .strip_prefix("cardmarket-int.com: ")
            .unwrap_or("");
        // Sellers that require tracked parcels: 16 EUR + 25% VAT = 220 kr.
        if seller.eq_ignore_ascii_case("MagicBarcelona") || seller.eq_ignore_ascii_case("Mazvigosl")
        {
            return ShippingInfo {
                base: 22000,
                free_threshold: 0,
                min_order: 0,
            };
        }
        // HamelinGames: 2.6 EUR + 25% VAT = 35.75 kr = 3575 oere.
        if seller.eq_ignore_ascii_case("HamelinGames") {
            return ShippingInfo {
                base: 3575,
                free_threshold: 0,
                min_order: 0,
            };
        }
        // Default international: 30 kr + 25% VAT = 37.50 kr = 3750 oere.
        return ShippingInfo {
            base: 3750,
            free_threshold: 0,
            min_order: 0,
        };
    }
    match store_name {
        "finn.no" => ShippingInfo {
            base: 5000,
            free_threshold: 0,
            min_order: 0,
        },
        "midgardgames.no" => ShippingInfo {
            base: 10000,
            free_threshold: 0,
            min_order: 0,
        },
        "outland.no" => ShippingInfo {
            base: 5900,
            free_threshold: 0,
            min_order: 0,
        },
        "pokeboks.no" => ShippingInfo {
            base: 5000,
            free_threshold: 0,
            min_order: 6000,
        },
        "korthaien.no" => ShippingInfo {
            base: 5500,
            free_threshold: 0,
            min_order: 0,
        },
        "adamstuenretro.no" => ShippingInfo {
            base: 5000,
            free_threshold: 0,
            min_order: 0,
        },
        "collectible.no" => ShippingInfo {
            base: 4900,
            free_threshold: 200000,
            min_order: 0,
        },
        _ => ShippingInfo {
            base: 0,
            free_threshold: 0,
            min_order: 0,
        },
    }
}

/// Shipping cost for a store given its current card total.
/// Returns 0 if the store has no cards (total == 0) or meets the free threshold.
pub(crate) fn shipping_cost(si: usize, total: u64, shipping: &[ShippingInfo]) -> u64 {
    if total == 0 {
        return 0;
    }
    let info = &shipping[si];
    if info.free_threshold > 0 && total as u32 >= info.free_threshold {
        0
    } else {
        info.base as u64
    }
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

    #[test]
    fn test_shipping_cost_free_threshold() {
        let info = vec![ShippingInfo {
            base: 5000,
            free_threshold: 200000,
            min_order: 0,
        }];
        assert_eq!(shipping_cost(0, 200000, &info), 0);
        assert_eq!(shipping_cost(0, 199999, &info), 5000);
    }

    #[test]
    fn test_shipping_cost_zero_total() {
        let info = vec![ShippingInfo {
            base: 5000,
            free_threshold: 0,
            min_order: 0,
        }];
        assert_eq!(shipping_cost(0, 0, &info), 0);
    }
}
