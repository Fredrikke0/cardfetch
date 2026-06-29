use crate::stores::StoreResult;
use std::collections::HashMap;

/// Format a price stored as integer oere to e.g. "15,00 kr".
fn format_price(oere: u32) -> String {
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
/// Prices are clickable links.  The cheapest price for each card is
/// marked with a trailing `*`.
pub fn print_table<'a>(
    cards: &[String],
    grouped: &HashMap<&str, Vec<&'a StoreResult>>,
    store_names: &[&str],
) {
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

        let by_store: HashMap<&str, &StoreResult> = results
            .iter()
            .map(|r| (r.store_name.as_str(), *r))
            .collect();

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
                    .map_or(false, |(a, b)| a == b);
                (opt, is_cheap)
            })
            .collect();

        rows.push(CardRow { card_name, cells });
    }

    if rows.is_empty() {
        eprintln!("No cards found in any store.");
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
            name.len().max(max_cell).max(1)
        })
        .collect();

    // ── Separator ──────────────────────────────────────────────────────
    let n = store_names.len();
    let total_width = card_width + store_widths.iter().sum::<usize>() + 3 * n;
    let sep = "-".repeat(total_width.min(200));

    // ── Header ─────────────────────────────────────────────────────────
    print!("{:<card_width$}", "Card");
    for (i, name) in store_names.iter().enumerate() {
        print!(" | {:<w$}", name, w = store_widths[i]);
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
