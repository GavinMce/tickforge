//! Per-symbol reference rows from daily bars, using only what was known at the close of `as_of`.

use std::collections::{BTreeMap, BTreeSet};

use tf_universe::{RefRow, StaticFeature, valid_name};

use crate::bars::{Bar, Symbology, date_text};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Params {
    /// Sessions the averages run over.
    pub window: usize,
    /// A symbol that traded on fewer of those sessions has no average (unknown, not zero).
    pub min_days: usize,
}

impl Default for Params {
    fn default() -> Self {
        Params {
            window: 20,
            min_days: 10,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// The last session whose bars were used.
    pub as_of_day: i64,
    pub as_of: String,
    /// Sessions in the window, oldest first, as dates.
    pub sessions: Vec<String>,
    pub bars_used: usize,
    /// Bars dated after `up_to`, never looked at.
    pub bars_after: usize,
    /// Bars whose instrument had no symbol that day.
    pub bars_unmapped: usize,
    /// Symbols that are not plain names (spaces, lower case, too long) and so cannot be listed.
    pub symbols_skipped: usize,
    pub symbols: usize,
    /// Symbols with no bar on `as_of` (their price is unknown), and with too few sessions (no averages).
    pub no_price: usize,
    pub no_average: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub enum BuildError {
    /// No bars dated on or before `up_to`.
    NoBars,
}

impl std::fmt::Display for BuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BuildError::NoBars => write!(f, "no bars on or before the requested date"),
        }
    }
}

impl std::error::Error for BuildError {}

/// The reference rows (columns price, adv_dollar, adv_shares, atr_permille) as of the last session
/// on or before `up_to` (days since 1970-01-01).
pub fn build(
    bars: &[Bar],
    symbology: &Symbology,
    up_to: i64,
    p: Params,
) -> Result<(Vec<RefRow>, Report), BuildError> {
    let mut r = Report::default();
    let known: Vec<&Bar> = bars.iter().filter(|b| b.day <= up_to).collect();
    r.bars_after = bars.len() - known.len();
    let as_of = known
        .iter()
        .map(|b| b.day)
        .max()
        .ok_or(BuildError::NoBars)?;
    r.as_of_day = as_of;
    r.as_of = date_text(as_of);
    let days: BTreeSet<i64> = known.iter().map(|b| b.day).collect();
    let window: Vec<i64> = days
        .iter()
        .rev()
        .take(p.window.max(1))
        .copied()
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    r.sessions = window.iter().map(|d| date_text(*d)).collect();
    let all_days: Vec<i64> = days.iter().copied().collect();
    let session_before = |d: i64| all_days.iter().rev().find(|x| **x < d).copied();

    let mut by_symbol: BTreeMap<&str, Vec<&Bar>> = BTreeMap::new();
    let mut skipped: BTreeSet<&str> = BTreeSet::new();
    for b in &known {
        let Some(sym) = symbology.symbol(b.id, b.day) else {
            r.bars_unmapped += 1;
            continue;
        };
        if !valid_name(sym) {
            skipped.insert(sym);
            continue;
        }
        r.bars_used += 1;
        by_symbol.entry(sym).or_default().push(b);
    }
    r.symbols_skipped = skipped.len();
    let w = i128::try_from(window.len()).unwrap_or(1);
    let mut rows = Vec::with_capacity(by_symbol.len());
    for (sym, mut v) in by_symbol {
        v.sort_by_key(|b| b.day);
        let mut row = RefRow {
            symbol: sym.to_owned(),
            ..RefRow::default()
        };
        let last = v.last().copied().filter(|b| b.day == as_of && b.close > 0);
        row.price = last.map(|b| b.close);
        if row.price.is_none() {
            r.no_price += 1;
        }
        let in_window: Vec<&&Bar> = v
            .iter()
            .filter(|b| window.binary_search(&b.day).is_ok())
            .collect();
        if in_window.len() >= p.min_days {
            let dollars: i128 = in_window
                .iter()
                .map(|b| i128::from(b.close) * i128::from(b.volume) / 1_000_000_000)
                .sum();
            let shares: i128 = in_window.iter().map(|b| i128::from(b.volume)).sum();
            row.adv_dollar = i64::try_from(dollars / w).ok();
            row.adv_shares = i64::try_from(shares / w).ok();
            // True range needs the previous *session's* close; the first bar of a symbol, or one
            // after a gap in its sessions, has none and is left out.
            let trs: Vec<i128> = in_window
                .iter()
                .filter_map(|b| {
                    let prev_day = session_before(b.day)?;
                    let pc = v.iter().find(|x| x.day == prev_day)?.close;
                    let (h, l) = (i128::from(b.high), i128::from(b.low));
                    let pc = i128::from(pc);
                    Some((h - l).max((h - pc).abs()).max((l - pc).abs()))
                })
                .collect();
            if let (Some(px), true) = (row.price, trs.len() >= p.min_days) {
                let mean = trs.iter().sum::<i128>() / i128::try_from(trs.len()).unwrap_or(1);
                row.atr_permille = i64::try_from(mean * 1000 / i128::from(px)).ok();
            }
        } else {
            r.no_average += 1;
        }
        rows.push(row);
    }
    r.symbols = rows.len();
    Ok((rows, r))
}

/// The columns [`build`] fills in.
pub const BAR_COLUMNS: [StaticFeature; 4] = [
    StaticFeature::Price,
    StaticFeature::AdvDollar,
    StaticFeature::AdvShares,
    StaticFeature::AtrPermille,
];
