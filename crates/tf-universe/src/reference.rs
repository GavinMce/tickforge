//! The reference snapshot: what is known about each symbol before the session opens.
//!
//! A CSV with a first line `# as_of YYYY-MM-DD` (the last day whose data it holds, so a replay can
//! tell it was never used for a day it would not have been known) and a header naming the columns it
//! has. A column that is not in the header is *absent*: no value for any symbol, and a universe that
//! asks for it refuses to run. An empty cell is *unknown* for that one symbol, and fails any
//! condition on it. [`Snapshot::render`] is canonical (columns in a fixed order, rows by symbol) and
//! [`Snapshot::fingerprint`] identifies the snapshot a run used.

use std::collections::BTreeSet;
use std::fmt::Write as _;

use crate::feature::{Kind, STATIC_FEATURES, StaticFeature, parse_value, render_value};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RefRow {
    pub symbol: String,
    /// Raw price (1e-9 dollars).
    pub price: Option<i64>,
    pub adv_dollar: Option<i64>,
    pub adv_shares: Option<i64>,
    pub atr_permille: Option<i64>,
    pub exchange: Option<String>,
    pub etf: Option<bool>,
    pub shortable: Option<bool>,
    pub easy_to_borrow: Option<bool>,
    pub tradable: Option<bool>,
    pub float: Option<i64>,
    pub short_interest: Option<i64>,
}

impl RefRow {
    pub fn num(&self, f: StaticFeature) -> Option<i64> {
        match f {
            StaticFeature::Price => self.price,
            StaticFeature::AdvDollar => self.adv_dollar,
            StaticFeature::AdvShares => self.adv_shares,
            StaticFeature::AtrPermille => self.atr_permille,
            StaticFeature::Float => self.float,
            StaticFeature::ShortInterest => self.short_interest,
            _ => None,
        }
    }

    pub fn flag(&self, f: StaticFeature) -> Option<bool> {
        match f {
            StaticFeature::Etf => self.etf,
            StaticFeature::Shortable => self.shortable,
            StaticFeature::EasyToBorrow => self.easy_to_borrow,
            StaticFeature::Tradable => self.tradable,
            _ => None,
        }
    }

    pub fn text(&self, f: StaticFeature) -> Option<&str> {
        match f {
            StaticFeature::Exchange => self.exchange.as_deref(),
            _ => None,
        }
    }

    fn set(&mut self, f: StaticFeature, cell: &str) -> Result<(), String> {
        if cell.is_empty() {
            return Ok(());
        }
        match f.kind() {
            Kind::Price | Kind::Int => {
                let v = parse_value(f.kind(), cell)?;
                match f {
                    StaticFeature::Price => self.price = Some(v),
                    StaticFeature::AdvDollar => self.adv_dollar = Some(v),
                    StaticFeature::AdvShares => self.adv_shares = Some(v),
                    StaticFeature::AtrPermille => self.atr_permille = Some(v),
                    StaticFeature::Float => self.float = Some(v),
                    StaticFeature::ShortInterest => self.short_interest = Some(v),
                    _ => {}
                }
            }
            Kind::Flag => {
                let v = parse_value(Kind::Flag, cell)? != 0;
                match f {
                    StaticFeature::Etf => self.etf = Some(v),
                    StaticFeature::Shortable => self.shortable = Some(v),
                    StaticFeature::EasyToBorrow => self.easy_to_borrow = Some(v),
                    StaticFeature::Tradable => self.tradable = Some(v),
                    _ => {}
                }
            }
            Kind::Text => {
                if !valid_name(cell) {
                    return Err(format!(
                        "`{cell}` is not an exchange name (upper case letters, digits, . _ -)"
                    ));
                }
                self.exchange = Some(cell.to_owned());
            }
        }
        Ok(())
    }

    fn cell(&self, f: StaticFeature) -> String {
        match f.kind() {
            Kind::Price | Kind::Int => self
                .num(f)
                .map_or(String::new(), |v| render_value(f.kind(), v)),
            Kind::Flag => self
                .flag(f)
                .map_or(String::new(), |v| render_value(Kind::Flag, i64::from(v))),
            Kind::Text => self.text(f).unwrap_or("").to_owned(),
        }
    }
}

/// An exchange or a symbol: upper case letters, digits, `.`, `_` and `-`, 1 to 16 long.
pub fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 16
        && s.bytes().all(|b| {
            b.is_ascii_uppercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-')
        })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotError {
    pub line: usize,
    pub why: String,
}

impl std::fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "reference snapshot line {}: {}", self.line, self.why)
    }
}

impl std::error::Error for SnapshotError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    /// `YYYY-MM-DD`.
    pub as_of: String,
    pub columns: BTreeSet<StaticFeature>,
    /// Sorted by symbol, one row each.
    pub rows: Vec<RefRow>,
}

fn valid_date(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b.iter()
            .enumerate()
            .all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit())
        && matches!(
            &s[5..7],
            "01" | "02" | "03" | "04" | "05" | "06" | "07" | "08" | "09" | "10" | "11" | "12"
        )
        && (1..=31).contains(&s[8..10].parse::<u32>().unwrap_or(0))
}

impl Snapshot {
    pub fn parse(text: &str) -> Result<Snapshot, SnapshotError> {
        let err = |line: usize, why: String| SnapshotError { line, why };
        let mut lines = text
            .lines()
            .enumerate()
            .map(|(i, l)| (i + 1, l))
            .filter(|(_, l)| !l.trim().is_empty());
        let (n, first) = lines.next().ok_or_else(|| err(1, "empty".to_owned()))?;
        let as_of = first
            .strip_prefix("# as_of ")
            .filter(|d| valid_date(d))
            .ok_or_else(|| err(n, "the first line must be `# as_of YYYY-MM-DD`".to_owned()))?
            .to_owned();
        let (n, header) = lines
            .next()
            .ok_or_else(|| err(n + 1, "no header line".to_owned()))?;
        let names: Vec<&str> = header.split(',').collect();
        if names.first() != Some(&"symbol") {
            return Err(err(n, "the header must start with `symbol`".to_owned()));
        }
        let mut cols: Vec<StaticFeature> = Vec::new();
        for name in &names[1..] {
            let f = StaticFeature::parse(name)
                .ok_or_else(|| err(n, format!("unknown column `{name}`")))?;
            if cols.contains(&f) {
                return Err(err(n, format!("column `{name}` appears twice")));
            }
            cols.push(f);
        }
        let mut rows: Vec<RefRow> = Vec::new();
        for (n, line) in lines {
            let cells: Vec<&str> = line.split(',').collect();
            if cells.len() != names.len() {
                return Err(err(
                    n,
                    format!("{} cells, the header has {}", cells.len(), names.len()),
                ));
            }
            if !valid_name(cells[0]) {
                return Err(err(n, format!("`{}` is not a symbol", cells[0])));
            }
            let mut row = RefRow {
                symbol: cells[0].to_owned(),
                ..RefRow::default()
            };
            for (f, cell) in cols.iter().zip(&cells[1..]) {
                row.set(*f, cell)
                    .map_err(|why| err(n, format!("{}: {why}", f.name())))?;
            }
            rows.push(row);
        }
        rows.sort_by(|a, b| a.symbol.cmp(&b.symbol));
        if let Some(w) = rows.windows(2).find(|w| w[0].symbol == w[1].symbol) {
            return Err(err(0, format!("symbol {} appears twice", w[0].symbol)));
        }
        Ok(Snapshot {
            as_of,
            columns: cols.into_iter().collect(),
            rows,
        })
    }

    /// The canonical text.
    pub fn render(&self) -> String {
        let cols: Vec<StaticFeature> = STATIC_FEATURES
            .iter()
            .map(|f| f.0)
            .filter(|f| self.columns.contains(f))
            .collect();
        let mut s = format!("# as_of {}\nsymbol", self.as_of);
        for c in &cols {
            let _ = write!(s, ",{}", c.name());
        }
        s.push('\n');
        for r in &self.rows {
            s.push_str(&r.symbol);
            for c in &cols {
                let _ = write!(s, ",{}", r.cell(*c));
            }
            s.push('\n');
        }
        s
    }

    /// FNV-1a 64 of the canonical text.
    pub fn fingerprint(&self) -> u64 {
        fnv(self.render().as_bytes())
    }

    pub fn row(&self, symbol: &str) -> Option<&RefRow> {
        self.rows
            .binary_search_by(|r| r.symbol.as_str().cmp(symbol))
            .ok()
            .map(|i| &self.rows[i])
    }
}

pub(crate) fn fnv(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}
