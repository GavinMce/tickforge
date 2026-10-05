//! Persistent security master.
//!
//! An [`InstrumentId`] is an index into an append-only list of securities and is
//! never reused, so it stays the same across days, ticker changes and corporate
//! actions. A ticker is not an identity: it is a dated attribute of a security,
//! so a rename or a ticker being reused by another company are just date spans.
//!
//! - **Ticker change:** a new symbol span on the same security.
//! - **Ticker reuse:** the old security's span ends, another security gets the
//!   symbol from a later date; resolving by date picks the right one.
//! - **Split, dividend:** no record. They do not change identity (prices are
//!   adjusted elsewhere).
//! - **Delisting or acquisition:** the security gets a delisting date and keeps
//!   its row and id forever.
//! - **Spin-off:** a new security.
//!
//! Provider-native keys map in per provider: Databento `instrument_id`s are
//! reassigned over time, so they are dated spans; Alpaca keys by ticker, so it
//! resolves through the symbol history.
//!
//! Startup builds a [`Session`] for the trading date: dense arrays indexed by id
//! and by native key, so the per-event path does no hashing. The rest is for the
//! edges (loading, adapters, tools).
//!
//! The persisted form is a small line-based text file; see [`text`].

use std::collections::HashMap;
use std::fmt;

use tf_core::{InstrumentId, ProviderId};

pub mod text;

#[cfg(test)]
mod tests;

/// Largest provider-native key a dense session array will index. A key at or
/// above it is rejected when loading, not silently dropped.
pub const MAX_NATIVE_KEY: u32 = 1 << 22;

const NONE: InstrumentId = InstrumentId::MAX;

/// A calendar date as `YYYYMMDD`; ordered chronologically. Spans are inclusive.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Date(u32);

impl Date {
    /// Checks the month and day ranges, not the calendar (Feb 30 is accepted).
    pub fn new(yyyymmdd: u32) -> Result<Date, Error> {
        let (y, m, d) = (yyyymmdd / 10_000, yyyymmdd / 100 % 100, yyyymmdd % 100);
        if (1900..=9999).contains(&y) && (1..=12).contains(&m) && (1..=31).contains(&d) {
            Ok(Date(yyyymmdd))
        } else {
            Err(Error::BadDate(yyyymmdd))
        }
    }

    pub const fn get(self) -> u32 {
        self.0
    }
}

impl fmt::Display for Date {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:08}", self.0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    BadDate(u32),
    UnknownInstrument(InstrumentId),
    BadSymbol(String),
    /// A span that ends before it starts, or lies outside the security's listing.
    BadSpan(String),
    /// Two spans that would map one key to two securities (or one security to
    /// two keys) on the same day.
    Overlap(String),
    NativeKeyTooLarge(u32),
    Parse {
        line: usize,
        msg: String,
    },
    Io(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::BadDate(d) => write!(f, "bad date {d}"),
            Error::UnknownInstrument(id) => write!(f, "unknown instrument {id}"),
            Error::BadSymbol(s) => write!(f, "bad symbol {s:?}"),
            Error::BadSpan(m) => write!(f, "bad span: {m}"),
            Error::Overlap(m) => write!(f, "overlap: {m}"),
            Error::NativeKeyTooLarge(k) => {
                write!(
                    f,
                    "native key {k} is at or above the limit {MAX_NATIVE_KEY}"
                )
            }
            Error::Parse { line, msg } => write!(f, "line {line}: {msg}"),
            Error::Io(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for Error {}

/// An inclusive date range; `to: None` is open-ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Span {
    pub(crate) from: Date,
    pub(crate) to: Option<Date>,
}

impl Span {
    fn contains(&self, d: Date) -> bool {
        self.from <= d && self.to.is_none_or(|t| d <= t)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SymbolSpan {
    pub(crate) symbol: Box<str>,
    pub(crate) span: Span,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Security {
    pub(crate) listed: Date,
    pub(crate) delisted: Option<Date>,
    pub(crate) symbols: Vec<SymbolSpan>,
}

impl Security {
    fn listing(&self) -> Span {
        Span {
            from: self.listed,
            to: self.delisted,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct NativeSpan {
    pub(crate) provider: ProviderId,
    pub(crate) key: u32,
    pub(crate) id: InstrumentId,
    pub(crate) span: Span,
}

/// True if no two spans share a day.
fn disjoint(spans: &mut [Span]) -> bool {
    spans.sort_by_key(|s| s.from);
    spans
        .windows(2)
        .all(|w| w[0].to.is_some_and(|t| t < w[1].from))
}

/// Accumulates securities, symbols and native mappings; [`Builder::build`]
/// checks them against each other. Ids are handed out in order and never reused.
#[derive(Clone, Debug, Default)]
pub struct Builder {
    securities: Vec<Security>,
    natives: Vec<NativeSpan>,
}

impl Builder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a security, returning its id: one more than the last.
    pub fn add_security(&mut self, listed: Date) -> InstrumentId {
        let id =
            InstrumentId::try_from(self.securities.len()).expect("more than u32::MAX securities");
        self.securities.push(Security {
            listed,
            delisted: None,
            symbols: Vec::new(),
        });
        id
    }

    /// Mark the last day the security trades. The row and id are kept.
    pub fn delist(&mut self, id: InstrumentId, last_day: Date) -> Result<(), Error> {
        self.security_mut(id)?.delisted = Some(last_day);
        Ok(())
    }

    /// The security trades as `symbol` from `from` to `to` inclusive (`None` is
    /// open-ended). A rename is one span ending and another starting.
    pub fn add_symbol(
        &mut self,
        id: InstrumentId,
        symbol: &str,
        from: Date,
        to: Option<Date>,
    ) -> Result<(), Error> {
        if symbol.is_empty() || !symbol.bytes().all(|b| b.is_ascii_graphic() && b != b'#') {
            return Err(Error::BadSymbol(symbol.to_owned()));
        }
        if to.is_some_and(|t| t < from) {
            return Err(Error::BadSpan(format!("{symbol} ends before it starts")));
        }
        self.security_mut(id)?.symbols.push(SymbolSpan {
            symbol: symbol.into(),
            span: Span { from, to },
        });
        Ok(())
    }

    /// `provider` knows the security as `key` from `from` to `to` inclusive.
    pub fn map_native(
        &mut self,
        provider: ProviderId,
        key: u32,
        id: InstrumentId,
        from: Date,
        to: Option<Date>,
    ) -> Result<(), Error> {
        if key >= MAX_NATIVE_KEY {
            return Err(Error::NativeKeyTooLarge(key));
        }
        if to.is_some_and(|t| t < from) {
            return Err(Error::BadSpan(format!(
                "{provider:?} key {key} ends before it starts"
            )));
        }
        self.security_mut(id)?;
        self.natives.push(NativeSpan {
            provider,
            key,
            id,
            span: Span { from, to },
        });
        Ok(())
    }

    fn security_mut(&mut self, id: InstrumentId) -> Result<&mut Security, Error> {
        self.securities
            .get_mut(id as usize)
            .ok_or(Error::UnknownInstrument(id))
    }

    pub fn build(self) -> Result<SecurityMaster, Error> {
        let mut by_symbol: HashMap<Box<str>, Vec<(Span, InstrumentId)>> = HashMap::new();
        for (i, sec) in self.securities.iter().enumerate() {
            let id = i as InstrumentId;
            if sec.delisted.is_some_and(|d| d < sec.listed) {
                return Err(Error::BadSpan(format!(
                    "security {id} is delisted before it is listed"
                )));
            }
            let mut own: Vec<Span> = sec.symbols.iter().map(|s| s.span).collect();
            if !disjoint(&mut own) {
                return Err(Error::Overlap(format!(
                    "security {id} has two symbols on one day"
                )));
            }
            let listing = sec.listing();
            for s in &sec.symbols {
                let inside = listing.contains(s.span.from)
                    && s.span
                        .to
                        .map_or(sec.delisted.is_none(), |t| listing.contains(t));
                if !inside {
                    return Err(Error::BadSpan(format!(
                        "symbol {} is outside security {id}'s listing",
                        s.symbol
                    )));
                }
                by_symbol
                    .entry(s.symbol.clone())
                    .or_default()
                    .push((s.span, id));
            }
        }
        for (symbol, spans) in &by_symbol {
            let mut only: Vec<Span> = spans.iter().map(|(s, _)| *s).collect();
            if !disjoint(&mut only) {
                return Err(Error::Overlap(format!(
                    "{symbol} maps to two securities on one day"
                )));
            }
        }

        let mut by_key: HashMap<(u8, u32), Vec<Span>> = HashMap::new();
        let mut by_id: HashMap<(u8, InstrumentId), Vec<Span>> = HashMap::new();
        for n in &self.natives {
            by_key
                .entry((n.provider.as_u8(), n.key))
                .or_default()
                .push(n.span);
            by_id
                .entry((n.provider.as_u8(), n.id))
                .or_default()
                .push(n.span);
        }
        for ((p, key), spans) in &mut by_key {
            if !disjoint(spans) {
                return Err(Error::Overlap(format!(
                    "provider {p} key {key} maps to two securities on one day"
                )));
            }
        }
        for ((p, id), spans) in &mut by_id {
            if !disjoint(spans) {
                return Err(Error::Overlap(format!(
                    "security {id} has two keys from provider {p} on one day"
                )));
            }
        }

        Ok(SecurityMaster {
            securities: self.securities,
            natives: self.natives,
            by_symbol,
        })
    }
}

/// The loaded, validated master. Lookups here search; use a [`Session`] on the
/// hot path.
#[derive(Clone, Debug)]
pub struct SecurityMaster {
    pub(crate) securities: Vec<Security>,
    pub(crate) natives: Vec<NativeSpan>,
    by_symbol: HashMap<Box<str>, Vec<(Span, InstrumentId)>>,
}

impl SecurityMaster {
    pub fn len(&self) -> usize {
        self.securities.len()
    }

    pub fn is_empty(&self) -> bool {
        self.securities.is_empty()
    }

    /// The security `symbol` named on `date`.
    pub fn resolve_symbol(&self, symbol: &str, date: Date) -> Option<InstrumentId> {
        self.by_symbol
            .get(symbol)?
            .iter()
            .find(|(span, _)| span.contains(date))
            .map(|&(_, id)| id)
    }

    /// The security a provider's native key meant on `date`.
    pub fn resolve_native(
        &self,
        provider: ProviderId,
        key: u32,
        date: Date,
    ) -> Option<InstrumentId> {
        self.natives
            .iter()
            .find(|n| n.provider == provider && n.key == key && n.span.contains(date))
            .map(|n| n.id)
    }

    /// The symbol a security traded under on `date`.
    pub fn symbol_on(&self, id: InstrumentId, date: Date) -> Option<&str> {
        self.securities
            .get(id as usize)?
            .symbols
            .iter()
            .find(|s| s.span.contains(date))
            .map(|s| &*s.symbol)
    }

    /// Whether the security is listed on `date`.
    pub fn is_listed(&self, id: InstrumentId, date: Date) -> bool {
        self.securities
            .get(id as usize)
            .is_some_and(|s| s.listing().contains(date))
    }

    /// Dense lookup tables for one trading date; build once at startup.
    pub fn session(&self, date: Date) -> Session {
        let mut names: Vec<Option<Box<str>>> = vec![None; self.securities.len()];
        let mut by_symbol = HashMap::new();
        for (i, sec) in self.securities.iter().enumerate() {
            if !sec.listing().contains(date) {
                continue;
            }
            if let Some(s) = sec.symbols.iter().find(|s| s.span.contains(date)) {
                names[i] = Some(s.symbol.clone());
                by_symbol.insert(s.symbol.clone(), i as InstrumentId);
            }
        }
        let mut by_native: [Vec<InstrumentId>; 4] = Default::default();
        for n in &self.natives {
            if !n.span.contains(date) || names[n.id as usize].is_none() {
                continue;
            }
            let table = &mut by_native[n.provider.as_u8() as usize];
            if table.len() <= n.key as usize {
                table.resize(n.key as usize + 1, NONE);
            }
            table[n.key as usize] = n.id;
        }
        Session {
            date,
            names,
            by_native,
            by_symbol,
        }
    }
}

/// The master as of one trading date, as dense arrays. Lookups by id and by
/// native key are plain array indexing: no hashing, no allocation.
#[derive(Clone, Debug)]
pub struct Session {
    date: Date,
    names: Vec<Option<Box<str>>>,
    by_native: [Vec<InstrumentId>; 4],
    by_symbol: HashMap<Box<str>, InstrumentId>,
}

impl Session {
    pub fn date(&self) -> Date {
        self.date
    }

    /// Hot path: the id for a provider's native key, if it is live today.
    #[inline]
    pub fn instrument(&self, provider: ProviderId, key: u32) -> Option<InstrumentId> {
        match self.by_native[provider.as_u8() as usize].get(key as usize) {
            Some(&id) if id != NONE => Some(id),
            _ => None,
        }
    }

    /// Hot path: the symbol an id trades under today.
    #[inline]
    pub fn symbol(&self, id: InstrumentId) -> Option<&str> {
        self.names.get(id as usize)?.as_deref()
    }

    /// For adapters that receive tickers (Alpaca): hashes, so resolve once per
    /// subscription, not per event.
    pub fn instrument_for_symbol(&self, symbol: &str) -> Option<InstrumentId> {
        self.by_symbol.get(symbol).copied()
    }

    /// Number of securities that trade today.
    pub fn live(&self) -> usize {
        self.by_symbol.len()
    }

    /// Size of the id space (live or not): arrays indexed by id need this many slots.
    pub fn id_space(&self) -> usize {
        self.names.len()
    }
}
