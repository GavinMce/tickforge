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
    // ---- from one-minute history (E19-S04), regular session 09:30 to 16:00 unless it says otherwise ----
    /// The previous regular session's high, low and close.
    PrevHigh,
    PrevLow,
    PrevClose,
    /// Average true range over 14 sessions, in price units (the mean of 14 true ranges, each against the
    /// session before it).
    Atr14,
    /// The state of an EMA(100) over regular-session hourly closes: the average in raw price units shifted left 16
    /// bits (its exact state between samples), and how many closes it has seen.
    Ema100hState,
    Ema100hCount,
    /// Average volume of the first minute and of the first five minutes of the regular session, and of the
    /// premarket (04:00 to 09:30), over the last sessions.
    VolFirst1,
    VolFirst5,
    VolPremarket,
    /// Average cumulative regular-session volume up to 09:35, 10:00, 10:30, 11:00, 12:00, 14:00 and 15:30.
    CumVol0935,
    CumVol1000,
    CumVol1030,
    CumVol1100,
    CumVol1200,
    CumVol1400,
    CumVol1530,
}

pub const STATIC_FEATURES: [(StaticFeature, &str, Kind); 27] = [
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
    (StaticFeature::PrevHigh, "prev_high", Kind::Price),
    (StaticFeature::PrevLow, "prev_low", Kind::Price),
    (StaticFeature::PrevClose, "prev_close", Kind::Price),
    (StaticFeature::Atr14, "atr14", Kind::Price),
    (StaticFeature::Ema100hState, "ema100h_state", Kind::Int),
    (StaticFeature::Ema100hCount, "ema100h_count", Kind::Int),
    (StaticFeature::VolFirst1, "vol_first1", Kind::Int),
    (StaticFeature::VolFirst5, "vol_first5", Kind::Int),
    (StaticFeature::VolPremarket, "vol_pre", Kind::Int),
    (StaticFeature::CumVol0935, "cumvol_0935", Kind::Int),
    (StaticFeature::CumVol1000, "cumvol_1000", Kind::Int),
    (StaticFeature::CumVol1030, "cumvol_1030", Kind::Int),
    (StaticFeature::CumVol1100, "cumvol_1100", Kind::Int),
    (StaticFeature::CumVol1200, "cumvol_1200", Kind::Int),
    (StaticFeature::CumVol1400, "cumvol_1400", Kind::Int),
    (StaticFeature::CumVol1530, "cumvol_1530", Kind::Int),
];

/// The cumulative-volume columns and how many minutes after the 09:30 open each one counts to (a bar belongs to a
/// checkpoint when it starts before it).
pub const CUMVOL_CHECKPOINTS: [(StaticFeature, u32); 7] = [
    (StaticFeature::CumVol0935, 5),
    (StaticFeature::CumVol1000, 30),
    (StaticFeature::CumVol1030, 60),
    (StaticFeature::CumVol1100, 90),
    (StaticFeature::CumVol1200, 150),
    (StaticFeature::CumVol1400, 270),
    (StaticFeature::CumVol1530, 360),
];

/// The columns that come from one-minute history (E19-S04), in table order.
pub const HISTORY_FEATURES: [StaticFeature; 16] = [
    StaticFeature::PrevHigh,
    StaticFeature::PrevLow,
    StaticFeature::PrevClose,
    StaticFeature::Atr14,
    StaticFeature::Ema100hState,
    StaticFeature::Ema100hCount,
    StaticFeature::VolFirst1,
    StaticFeature::VolFirst5,
    StaticFeature::VolPremarket,
    StaticFeature::CumVol0935,
    StaticFeature::CumVol1000,
    StaticFeature::CumVol1030,
    StaticFeature::CumVol1100,
    StaticFeature::CumVol1200,
    StaticFeature::CumVol1400,
    StaticFeature::CumVol1530,
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
