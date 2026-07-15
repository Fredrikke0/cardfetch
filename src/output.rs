use crate::stores::StoreResult;
use crate::wizard::WizardSolution;
use std::collections::{BTreeMap, HashMap};

/// Shorten a store name for the table header:
/// - Strip trailing ".no" / ".com" / ".net" / ".org"
/// - Replace "cardmarket.com:" with "CM:"
fn abbreviate(name: &str) -> String {
    if let Some(seller) = name.strip_prefix("cardmarket-int-private.com:") {
        return format!("CM-PRIV: {seller}");
    }
    if let Some(seller) = name.strip_prefix("cardmarket-int.com:") {
        return format!("CM-INT: {seller}");
    }
    if let Some(seller) = name.strip_prefix("cardmarket.com:") {
        return format!("CM: {seller}");
    }
    for tld in &[".no", ".com", ".net", ".org"] {
        if let Some(base) = name.strip_suffix(tld) {
            let mut c = base.chars();
            let capitalized = match c.next() {
                Some(head) => head.to_uppercase().chain(c).collect(),
                None => String::new(),
            };
            return capitalized;
        }
    }
    name.to_string()
}

/// Format a price stored as integer oere to e.g. "15,00 kr".
fn format_price(oere: u32) -> String {
    let whole = oere / 100;
    let frac = oere % 100;
    format!("{},{:02} kr", whole, frac)
}

/// Like `format_price` but for u64.
fn format_price_u64(oere: u64) -> String {
    let whole = oere / 100;
    let frac = oere % 100;
    format!("{},{:02} kr", whole, frac)
}

/// Wrap `text` in an OSC 8 hyperlink.  In modern terminals the text renders
/// normally but opens `url` on click.
fn hyperlink(url: &str, text: &str) -> String {
    format!("\x1b]8;;{}\x1b\\{}\x1b]8;;\x1b\\", url, text)
}

/// Print a padded cell.  `visible` is the user-facing text (used for width
/// calculation).  `raw` is what actually gets printed (may contain escape
/// sequences).  Pads with spaces to fill `width`.
fn print_cell(visible: &str, raw: &str, width: usize) {
    let pad = width.saturating_sub(visible.len());
    print!("{raw}{:pad$}", "", pad = pad);
}

/// Print results as a table: one card per row, one store per column.
/// Store columns are discovered dynamically from the results.
/// Prices are clickable links.  The cheapest price for each card is
/// marked green.
///
/// Cardmarket sellers are printed separately in a per-seller view
/// after the main table.
pub fn print_table<'a>(cards: &'a [String], grouped: &'a HashMap<&str, Vec<&'a StoreResult>>) {
    // ── Split Cardmarket from other stores ──────────────────────────────
    let mut non_cm: HashMap<&str, Vec<&StoreResult>> = HashMap::new();
    let mut cm: HashMap<&str, Vec<&StoreResult>> = HashMap::new();

    for (card, results) in grouped.iter() {
        let (cm_results, store_results): (Vec<_>, Vec<_>) = results.iter().partition(|r| {
            r.store_name.starts_with("cardmarket.com:")
                || r.store_name.starts_with("cardmarket-int.com:")
                || r.store_name.starts_with("cardmarket-int-private.com:")
        });
        if !store_results.is_empty() {
            non_cm.insert(card, store_results);
        }
        if !cm_results.is_empty() {
            cm.insert(card, cm_results);
        }
    }

    // ── Print non-CardMarket table ─────────────────────────────────────
    print_store_table(cards, &non_cm);

    // ── Print CardMarket sellers section ───────────────────────────────
    print_cardmarket_section(cards, &cm);

    // ── No results at all ──────────────────────────────────────────────
    if non_cm.is_empty() && cm.is_empty() {
        eprintln!("no results found for any card across any store");
    }
}

/// Print the main store table for non-CardMarket stores.
fn print_store_table<'a>(cards: &'a [String], grouped: &'a HashMap<&str, Vec<&'a StoreResult>>) {
    // ── Discover store columns from results ───────────────────────────
    let mut store_order: BTreeMap<&str, usize> = BTreeMap::new();
    for results in grouped.values() {
        for r in results {
            let len = store_order.len();
            store_order.entry(r.store_name.as_str()).or_insert(len);
        }
    }
    let store_names: Vec<&str> = {
        let mut pairs: Vec<(&str, usize)> = store_order.iter().map(|(k, v)| (*k, *v)).collect();
        pairs.sort_by_key(|(_, idx)| *idx);
        pairs.into_iter().map(|(name, _)| name).collect()
    };

    // ── Build per-card data ────────────────────────────────────────────
    struct CardRow<'a> {
        card_name: &'a str,
        /// (result, is_cheapest) per store
        cells: Vec<(Option<&'a StoreResult>, bool)>,
    }

    let mut rows: Vec<CardRow> = Vec::new();

    for card_name in cards {
        let results = match grouped.get(card_name.as_str()) {
            Some(r) => r,
            None => continue,
        };

        // Keep only the cheapest listing per store (same seller may have
        // multiple versions — e.g. different conditions — on the same page).
        let by_store: HashMap<&str, &StoreResult> =
            results.iter().fold(HashMap::new(), |mut acc, r| {
                acc.entry(r.store_name.as_str())
                    .and_modify(|e| {
                        if r.price < e.price {
                            *e = r;
                        }
                    })
                    .or_insert(r);
                acc
            });

        let prices: Vec<Option<&StoreResult>> = store_names
            .iter()
            .map(|name| by_store.get(name).copied())
            .collect();

        let cheapest_ptr = prices
            .iter()
            .flatten()
            .min_by_key(|r| r.price)
            .map(|c| *c as *const StoreResult);

        let cells: Vec<(Option<&StoreResult>, bool)> = prices
            .into_iter()
            .map(|opt| {
                let is_cheap = opt
                    .map(|r| r as *const StoreResult)
                    .zip(cheapest_ptr)
                    .is_some_and(|(a, b)| a == b);
                (opt, is_cheap)
            })
            .collect();

        rows.push(CardRow { card_name, cells });
    }

    if rows.is_empty() {
        return;
    }

    // ── Compute column widths (visible text only) ──────────────────────
    let card_width = rows
        .iter()
        .map(|r| r.card_name.len())
        .max()
        .unwrap_or(4)
        .max(4);

    let store_widths: Vec<usize> = store_names
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let max_cell = rows
                .iter()
                .filter_map(|r| r.cells[i].0.map(|sr| format_price(sr.price).len()))
                .max()
                .unwrap_or(1);
            abbreviate(name).len().max(max_cell).max(1)
        })
        .collect();

    // ── Separator ──────────────────────────────────────────────────────
    let n = store_names.len();
    let total_width = card_width + store_widths.iter().sum::<usize>() + 3 * n;
    let sep = "-".repeat(total_width.min(200));

    // ── Header ─────────────────────────────────────────────────────────
    print!("{:<card_width$}", "Card");
    for (i, name) in store_names.iter().enumerate() {
        let abbr = abbreviate(name);
        print!(" | {:<w$}", abbr, w = store_widths[i]);
    }
    println!();
    println!("{}", sep);

    // ── Rows ───────────────────────────────────────────────────────────
    for row in &rows {
        print!("{:<card_width$}", row.card_name);

        for (i, (opt, is_cheap)) in row.cells.iter().enumerate() {
            print!(" | ");
            match opt {
                Some(sr) => {
                    let price_str = format_price(sr.price);
                    let raw = if *is_cheap {
                        format!("\x1b[32m{}\x1b[0m", price_str)
                    } else {
                        price_str.clone()
                    };
                    let linked = hyperlink(&sr.url, &raw);
                    print_cell(&price_str, &linked, store_widths[i]);
                }
                None => {
                    print_cell("-", "-", store_widths[i]);
                }
            }
        }

        println!();
    }

    // ── Totals ────────────────────────────────────────────────────────
    let totals: Vec<u32> = (0..store_names.len())
        .map(|i| {
            rows.iter()
                .filter_map(|r| r.cells[i].0.map(|sr| sr.price))
                .sum()
        })
        .collect();

    println!("{}", sep);
    print!("{:<card_width$}", "Total");
    for (i, total) in totals.iter().enumerate() {
        print!(" | ");
        if *total > 0 {
            let s = format_price(*total);
            print_cell(&s, &s, store_widths[i]);
        } else {
            print_cell("-", "-", store_widths[i]);
        }
    }
    println!();
}

// ── CardMarket seller section ────────────────────────────────────────────────

/// Print CardMarket sellers grouped by seller, listing which cards each has
/// and at what price.  Similar format to the wizard output.
/// Guarantees at least one seller from each category (NO, INT, PRIV) when
/// available, then fills remaining slots by overall card count.
fn print_cardmarket_section(cards: &[String], cm_grouped: &HashMap<&str, Vec<&StoreResult>>) {
    if cm_grouped.is_empty() {
        return;
    }

    // Build per-seller data, keeping cheapest listing per seller per card.
    let mut seller_cards: HashMap<&str, Vec<(&str, u32, &str)>> = HashMap::new();
    for card_name in cards {
        let results = match cm_grouped.get(card_name.as_str()) {
            Some(r) => r,
            None => continue,
        };

        // Keep cheapest per seller for this card.
        let mut cheapest: HashMap<&str, &StoreResult> = HashMap::new();
        for r in results {
            cheapest
                .entry(r.store_name.as_str())
                .and_modify(|e| {
                    if r.price < e.price {
                        *e = r;
                    }
                })
                .or_insert(r);
        }

        for (seller, r) in cheapest {
            seller_cards.entry(seller).or_default().push((
                card_name.as_str(),
                r.price,
                r.url.as_str(),
            ));
        }
    }

    // Bucket sellers by category.
    let mut no_sellers: Vec<&str> = Vec::new();
    let mut int_sellers: Vec<&str> = Vec::new();
    let mut priv_sellers: Vec<&str> = Vec::new();

    for &seller in seller_cards.keys() {
        if seller.starts_with("cardmarket-int-private.com:") {
            priv_sellers.push(seller);
        } else if seller.starts_with("cardmarket-int.com:") {
            int_sellers.push(seller);
        } else if seller.starts_with("cardmarket.com:") {
            no_sellers.push(seller);
        }
    }

    // Sort each bucket by card count (descending), then alphabetically.
    let sort_by_count = |a: &&str, b: &&str| {
        seller_cards[b]
            .len()
            .cmp(&seller_cards[a].len())
            .then_with(|| a.cmp(b))
    };
    no_sellers.sort_by(sort_by_count);
    int_sellers.sort_by(sort_by_count);
    priv_sellers.sort_by(sort_by_count);

    // Pick at least one from each non-empty category, then fill remaining
    // slots (up to MAX_SHOWN) by overall ranking.
    const MAX_SHOWN: usize = 3;

    let categories: [(&str, &Vec<&str>); 3] = [
        ("Norwegian sellers", &no_sellers),
        ("Int Powersellers", &int_sellers),
        ("Int Private sellers", &priv_sellers),
    ];

    let hline = "=".repeat(50);

    let max_name = seller_cards
        .values()
        .flat_map(|v| v.iter())
        .map(|(n, _, _)| n.len())
        .max()
        .unwrap_or(4)
        .max(10);

    for (label, cat_sellers) in categories {
        if cat_sellers.is_empty() {
            continue;
        }
        let total = cat_sellers.len();
        let visible: Vec<&&str> = cat_sellers.iter().take(MAX_SHOWN).collect();

        println!();
        println!("{hline}");
        println!("  {label} ({total} total, showing {})", visible.len());
        println!("{hline}");

        for seller in &visible {
            let entries = &seller_cards[*seller];
            let subtotal: u32 = entries.iter().map(|(_, p, _)| p).sum();
            let abbr = abbreviate(seller);
            println!(
                "  {abbr} ({} card{}):",
                entries.len(),
                if entries.len() == 1 { "" } else { "s" }
            );
            for (card_name, price, url) in entries {
                let ps = format_price(*price);
                let linked = hyperlink(url, &ps);
                let pad = max_name.saturating_sub(card_name.len());
                println!("    {card_name}{:pad$}  {linked}", "");
            }
            let sep = "-".repeat(max_name + 16);
            println!("    {sep}");
            println!("    Subtotal: {}", format_price(subtotal));
        }

        let hidden_count = total.saturating_sub(visible.len());
        if hidden_count > 0 {
            let others_total: u32 = cat_sellers[visible.len()..]
                .iter()
                .flat_map(|s| seller_cards[*s].iter())
                .map(|(_, p, _)| p)
                .sum();
            let others_cards: usize = cat_sellers[visible.len()..]
                .iter()
                .map(|s| seller_cards[*s].len())
                .sum();
            println!();
            println!(
                "  ... and {hidden_count} more seller{} ({} cards, {} total)",
                if hidden_count == 1 { "" } else { "s" },
                others_cards,
                format_price(others_total),
            );
        }
    }

    println!();
}

// ── Wizard output ────────────────────────────────────────────────────────────

/// Print a compact summary table for all tolerances 0..N, then the full
/// breakdown for the max-tolerance solution.
pub fn print_wizard_summary(
    solutions: &[(usize, WizardSolution)],
    strategy: &str,
    wanted_cards: &[String],
) {
    let strategy_label = match strategy {
        "simplest" => "Simplest",
        "cheapest" => "Cheapest",
        other => other,
    };

    let total_cards = wanted_cards.len();

    let hline = "\u{2550}";
    let banner = hline.repeat(60);

    println!();
    println!("{banner}");
    println!("  Purchase Wizard \u{2014} {strategy_label} Strategy");
    println!("  {total_cards} wanted cards, {} cached sellers", {
        let count: usize = solutions
            .first()
            .map(|(_, s)| s.store_names.len())
            .unwrap_or(0);
        count
    });
    println!("{banner}");
    println!();

    // ── Summary table ──────────────────────────────────────────────────
    println!(
        "  {:>9}  {:>6}  {:>5}  {:>6}  {:>9}  {:>8}  {:>10}  {:>9}",
        "Tolerance", "Stores", "Found", "Skipped", "Cards", "Shipping", "Total", "Per card"
    );
    let sep = "\u{2500}".repeat(79);
    println!("  {sep}");

    for (t, sol) in solutions {
        let found = total_cards - sol.skipped.len();
        let grand = sol.total_card_cost + sol.total_shipping;
        let avg = if found > 0 { grand / found as u64 } else { 0 };
        println!(
            "  {:>9}  {:>6}  {:>5}  {:>6}  {:>9}  {:>8}  {:>10}  {:>9}",
            t,
            sol.num_stores,
            format!("{}/{}", found, total_cards),
            sol.skipped.len(),
            format_price_u64(sol.total_card_cost),
            format_price_u64(sol.total_shipping),
            format_price_u64(grand),
            format_price_u64(avg),
        );
    }
    println!();

    // ── Detailed breakdown for max tolerance ────────────────────────────
    let (max_t, max_sol) = solutions.last().unwrap();
    print_wizard_table(max_sol, strategy, *max_t);
}

/// Print purchase wizard result in a per-store format.
pub fn print_wizard_table(sol: &WizardSolution, strategy: &str, tolerance: usize) {
    let strategy_label = match strategy {
        "simplest" => "Simplest",
        "cheapest" => "Cheapest",
        other => other,
    };

    let hline = "\u{2550}";
    let banner = hline.repeat(50);

    println!();
    println!("{banner}");
    println!("  Purchase Wizard \u{2014} {strategy_label} Strategy (tolerance: {tolerance})");
    println!("{banner}");

    let total_cards = sol.assignments.len();
    let found = total_cards - sol.skipped.len();
    let grand = sol.total_card_cost + sol.total_shipping;

    let sep = "\u{2500}".repeat(42);
    println!("{sep}");
    println!(
        "  {} store{}, {}/{} cards, {} total",
        sol.num_stores,
        if sol.num_stores == 1 { "" } else { "s" },
        found,
        total_cards,
        format_price(grand as u32),
    );
    println!("{sep}");

    if sol.store_names.is_empty() {
        println!("  (no stores used \u{2014} all cards skipped)");
        println!();
        return;
    }

    // Group found assignments by store
    let mut store_cards: HashMap<&str, Vec<(&str, u32, &str)>> = HashMap::new();
    for (card_name, opt) in &sol.assignments {
        if let Some((store, price, url)) = opt {
            store_cards
                .entry(store.as_str())
                .or_default()
                .push((card_name, *price, url.as_str()));
        }
    }

    // Build store-index lookup for totals
    let store_totals: HashMap<&str, (u32, u32)> = sol
        .store_names
        .iter()
        .zip(sol.card_subtotals.iter())
        .zip(sol.shipping_costs.iter())
        .map(|((name, &card), &ship)| (name.as_str(), (card, ship)))
        .collect();

    // Max card name width for alignment
    let max_name = store_cards
        .values()
        .flat_map(|v| v.iter())
        .map(|(n, _, _)| n.len())
        .max()
        .unwrap_or(4)
        .max(10);

    let store_sep = "\u{2500}".repeat(max_name + 16);

    for store_name in sol.store_names.iter() {
        let cards = match store_cards.get(store_name.as_str()) {
            Some(c) => c,
            None => continue,
        };

        let (card_total, shipping) = store_totals
            .get(store_name.as_str())
            .copied()
            .unwrap_or((0, 0));
        let st = card_total + shipping;

        let abbr = abbreviate(store_name);
        println!();
        println!(
            "  {abbr} ({} card{}):",
            cards.len(),
            if cards.len() == 1 { "" } else { "s" }
        );

        for (card_name, price, url) in cards {
            let ps = format_price(*price);
            let linked = hyperlink(url, &ps);
            let pad = max_name.saturating_sub(card_name.len());
            println!("    {card_name}{:pad$}  {linked}", "");
        }

        println!("    {store_sep}");
        println!(
            "    Card subtotal: {}  |  Shipping: {}  |  Store total: {}",
            format_price(card_total),
            format_price(shipping),
            format_price(st),
        );
    }

    // Grand total
    let grand_sep = "\u{2500}".repeat(60);
    println!();
    println!("{grand_sep}");
    println!(
        "  GRAND TOTAL: {}  |  Card total: {}  |  Shipping: {}",
        format_price(grand as u32),
        format_price(sol.total_card_cost as u32),
        format_price(sol.total_shipping as u32),
    );

    if !sol.skipped.is_empty() {
        let skip_list = sol.skipped.join(", ");
        println!("  Skipped ({}): {skip_list}", sol.skipped.len());
    }

    println!();
}
