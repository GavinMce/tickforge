//! Alpaca's asset list: whether a symbol can be traded and sold short there.

use std::collections::BTreeMap;

use tf_alpaca::json::Json;
use tf_universe::{RefRow, StaticFeature, valid_name};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Asset {
    pub symbol: String,
    pub exchange: Option<String>,
    pub tradable: bool,
    pub shortable: bool,
    pub easy_to_borrow: bool,
}

/// `GET /v2/assets` output. Only active US equities are kept; a flag that is missing or not a boolean
/// is an error, since guessing `true` would let a symbol into the universe.
pub fn parse_assets(text: &str) -> Result<Vec<Asset>, String> {
    let Json::Arr(items) = Json::parse(text).map_err(|e| e.to_string())? else {
        return Err("expected a list of assets".to_owned());
    };
    let mut out = Vec::new();
    for (i, a) in items.iter().enumerate() {
        let sym = a
            .str_at("symbol")
            .ok_or_else(|| format!("asset {i}: no symbol"))?;
        if a.str_at("class") != Some("us_equity") || a.str_at("status") != Some("active") {
            continue;
        }
        let flag = |k: &str| match a.get(k) {
            Some(Json::Bool(b)) => Ok(*b),
            _ => Err(format!("{sym}: `{k}` is not true or false")),
        };
        out.push(Asset {
            symbol: sym.to_owned(),
            exchange: a
                .str_at("exchange")
                .filter(|e| valid_name(e))
                .map(str::to_owned),
            tradable: flag("tradable")?,
            shortable: flag("shortable")?,
            easy_to_borrow: flag("easy_to_borrow")?,
        });
    }
    Ok(out)
}

/// The columns [`merge_assets`] fills in.
pub const ASSET_COLUMNS: [StaticFeature; 4] = [
    StaticFeature::Tradable,
    StaticFeature::Shortable,
    StaticFeature::EasyToBorrow,
    StaticFeature::Exchange,
];

/// Set the flags of every row Alpaca lists; a row it does not list keeps them unknown. Returns how
/// many rows were found.
pub fn merge_assets(rows: &mut [RefRow], assets: &[Asset]) -> usize {
    let by: BTreeMap<&str, &Asset> = assets.iter().map(|a| (a.symbol.as_str(), a)).collect();
    let mut found = 0;
    for r in rows {
        if let Some(a) = by.get(r.symbol.as_str()) {
            found += 1;
            r.tradable = Some(a.tradable);
            r.shortable = Some(a.shortable);
            r.easy_to_borrow = Some(a.easy_to_borrow);
            r.exchange = a.exchange.clone();
        }
    }
    found
}

/// A list of ETF symbols (one per line, `#` comments) the person keeps: no free source marks them.
/// Every row gets `etf` yes or no, so a symbol missing from the list is taken to be a stock.
pub fn merge_etf_list(rows: &mut [RefRow], text: &str) -> Result<usize, String> {
    let mut etfs = std::collections::BTreeSet::new();
    for (i, l) in text.lines().enumerate() {
        let l = l.split('#').next().unwrap_or("").trim();
        if l.is_empty() {
            continue;
        }
        if !valid_name(l) {
            return Err(format!("etf list line {}: `{l}` is not a symbol", i + 1));
        }
        etfs.insert(l.to_owned());
    }
    let mut n = 0;
    for r in rows {
        let e = etfs.contains(&r.symbol);
        r.etf = Some(e);
        n += usize::from(e);
    }
    Ok(n)
}
