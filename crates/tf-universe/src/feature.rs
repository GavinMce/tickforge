//! The measurements a universe may be built on.

use tf_core::Px;

/// How a feature's value is written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Dollars, exact to a billionth.
    Price,
    /// A whole number.
    Int,
    /// `yes` or `no`.
    Flag,
    /// A name.
    Text,
}

/// A fact about a symbol known before the session opens, from prior-day data and reference lists.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum StaticFeature {
    /// Last close.
    Price,
    /// Average daily dollar volume.
    AdvDollar,
    /// Average daily share volume.
    AdvShares,
    /// Average true range as permille of the price.
    AtrPermille,
    Exchange,
    Etf,
    Shortable,
    EasyToBorrow,
    Tradable,
    /// Shares available to trade. No source has it yet (E14): a reference snapshot says so by
    /// leaving the column out, and a spec that uses it refuses to run.
    Float,
    /// Shares sold short. Same story as `Float`.
    ShortInterest,
}

pub const STATIC_FEATURES: [(StaticFeature, &str, Kind); 11] = [
    (StaticFeature::Price, "price", Kind::Price),
    (StaticFeature::AdvDollar, "adv_dollar", Kind::Int),
    (StaticFeature::AdvShares, "adv_shares", Kind::Int),
    (StaticFeature::AtrPermille, "atr_permille", Kind::Int),
    (StaticFeature::Exchange, "exchange", Kind::Text),
    (StaticFeature::Etf, "etf", Kind::Flag),
    (StaticFeature::Shortable, "shortable", Kind::Flag),
    (StaticFeature::EasyToBorrow, "easy_to_borrow", Kind::Flag),
    (StaticFeature::Tradable, "tradable", Kind::Flag),
    (StaticFeature::Float, "float", Kind::Int),
    (StaticFeature::ShortInterest, "short_interest", Kind::Int),
];

impl StaticFeature {
    pub fn name(self) -> &'static str {
        STATIC_FEATURES
            .iter()
            .find(|f| f.0 == self)
            .map_or("?", |f| f.1)
    }

    pub fn kind(self) -> Kind {
        STATIC_FEATURES
            .iter()
            .find(|f| f.0 == self)
            .map_or(Kind::Int, |f| f.2)
    }

    pub fn parse(name: &str) -> Option<StaticFeature> {
        STATIC_FEATURES.iter().find(|f| f.1 == name).map(|f| f.0)
    }
}

/// A measurement of today's session so far, taken from Tier 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum LiveFeature {
    /// Last price against the prior close, permille (needs the reference price).
    GapPermille,
    /// Today's shares against the average day's, permille (needs the reference share volume).
    VolumeRatioPermille,
    /// Dollars traded today.
    DollarVolume,
    /// Day high to low as permille of the last price.
    RangePermille,
    /// Trades today.
    Trades,
}

pub const LIVE_FEATURES: [(LiveFeature, &str); 5] = [
    (LiveFeature::GapPermille, "gap_permille"),
    (LiveFeature::VolumeRatioPermille, "volume_ratio_permille"),
    (LiveFeature::DollarVolume, "dollar_volume"),
    (LiveFeature::RangePermille, "range_permille"),
    (LiveFeature::Trades, "trades"),
];

impl LiveFeature {
    pub fn name(self) -> &'static str {
        LIVE_FEATURES
            .iter()
            .find(|f| f.0 == self)
            .map_or("?", |f| f.1)
    }

    pub fn parse(name: &str) -> Option<LiveFeature> {
        LIVE_FEATURES.iter().find(|f| f.1 == name).map(|f| f.0)
    }
}

/// A value of a given kind, from text. Prices are raw (1e-9 dollars); flags are 0 or 1.
pub fn parse_value(kind: Kind, text: &str) -> Result<i64, String> {
    match kind {
        Kind::Price => Px::parse(text)
            .map(Px::raw)
            .ok_or_else(|| format!("`{text}` is not a price in dollars")),
        Kind::Int => {
            if text.is_empty()
                || !text
                    .trim_start_matches('-')
                    .bytes()
                    .all(|b| b.is_ascii_digit())
                || text == "-"
            {
                return Err(format!("`{text}` is not a whole number"));
            }
            text.parse::<i64>()
                .map_err(|_| format!("`{text}` is too large"))
        }
        Kind::Flag => match text {
            "yes" => Ok(1),
            "no" => Ok(0),
            _ => Err(format!("`{text}` is not yes or no")),
        },
        Kind::Text => Err("a name is not a number".to_owned()),
    }
}

/// The canonical text of a value.
pub fn render_value(kind: Kind, v: i64) -> String {
    match kind {
        Kind::Price => Px::from_raw(v).to_decimal(),
        Kind::Int => v.to_string(),
        Kind::Flag => if v != 0 { "yes" } else { "no" }.to_owned(),
        Kind::Text => String::new(),
    }
}
