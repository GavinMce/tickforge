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
    // ---- from one-minute history (E19-S53): the time-of-day profile, in quarter hours ----
    /// Average volume of the quarter hour `k` of the day, 0 to 47: 04:00 to 04:15 is 0, the open's quarter (09:30 to 09:45) is 22, the last (15:45 to
    /// 16:00) is 47.
    QVol(u8),
    /// The normal absolute move over the 60 minutes from the start of regular-session quarter hour `j`, 0 to 22 (09:30 is 0, 15:00 is 22), in
    /// hundred-thousandths of the price at its start (0.1 basis points).
    QMove(u8),
}

/// Quarter hours of volume the profile has: the premarket 04:00 to 09:30 (22) and the regular session 09:30 to 16:00 (26).
pub const QVOL_COUNT: usize = 48;
/// The first regular-session quarter hour among them.
pub const QVOL_FIRST_REGULAR: usize = 22;
/// Quarter hours of the regular session a 60-minute move can start in (09:30 to 15:00).
pub const QMOVE_COUNT: usize = 23;
/// Minutes after midnight New York time of 04:00, of 09:30 and of 16:00.
pub const PREMARKET_START_MIN: u32 = 240;
pub const OPEN_MIN: u32 = 570;
pub const CLOSE_MIN: u32 = 960;

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

/// Column names of the quarter-hour volume profile.
pub const QVOL_NAMES: [&str; 48] = [
    "qvol_0400",
    "qvol_0415",
    "qvol_0430",
    "qvol_0445",
    "qvol_0500",
    "qvol_0515",
    "qvol_0530",
    "qvol_0545",
    "qvol_0600",
    "qvol_0615",
    "qvol_0630",
    "qvol_0645",
    "qvol_0700",
    "qvol_0715",
    "qvol_0730",
    "qvol_0745",
    "qvol_0800",
    "qvol_0815",
    "qvol_0830",
    "qvol_0845",
    "qvol_0900",
    "qvol_0915",
    "qvol_0930",
    "qvol_0945",
    "qvol_1000",
    "qvol_1015",
    "qvol_1030",
    "qvol_1045",
    "qvol_1100",
    "qvol_1115",
    "qvol_1130",
    "qvol_1145",
    "qvol_1200",
    "qvol_1215",
    "qvol_1230",
    "qvol_1245",
    "qvol_1300",
    "qvol_1315",
    "qvol_1330",
    "qvol_1345",
    "qvol_1400",
    "qvol_1415",
    "qvol_1430",
    "qvol_1445",
    "qvol_1500",
    "qvol_1515",
    "qvol_1530",
    "qvol_1545",
];
/// Column names of the 60-minute move profile.
pub const QMOVE_NAMES: [&str; 23] = [
    "qmove_0930",
    "qmove_0945",
    "qmove_1000",
    "qmove_1015",
    "qmove_1030",
    "qmove_1045",
    "qmove_1100",
    "qmove_1115",
    "qmove_1130",
    "qmove_1145",
    "qmove_1200",
    "qmove_1215",
    "qmove_1230",
    "qmove_1245",
    "qmove_1300",
    "qmove_1315",
    "qmove_1330",
    "qmove_1345",
    "qmove_1400",
    "qmove_1415",
    "qmove_1430",
    "qmove_1445",
    "qmove_1500",
];

/// The columns of the time-of-day profile (E19-S53), in table order.
pub fn profile_features() -> impl Iterator<Item = StaticFeature> {
    (0..QVOL_COUNT as u8)
        .map(StaticFeature::QVol)
        .chain((0..QMOVE_COUNT as u8).map(StaticFeature::QMove))
}

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
        match self {
            StaticFeature::QVol(k) => {
                return QVOL_NAMES.get(usize::from(k)).copied().unwrap_or("?");
            }
            StaticFeature::QMove(j) => {
                return QMOVE_NAMES.get(usize::from(j)).copied().unwrap_or("?");
            }
            _ => {}
        }
        STATIC_FEATURES
            .iter()
            .find(|f| f.0 == self)
            .map_or("?", |f| f.1)
    }

    pub fn kind(self) -> Kind {
        if matches!(self, StaticFeature::QVol(_) | StaticFeature::QMove(_)) {
            return Kind::Int;
        }
        STATIC_FEATURES
            .iter()
            .find(|f| f.0 == self)
            .map_or(Kind::Int, |f| f.2)
    }

    pub fn parse(name: &str) -> Option<StaticFeature> {
        if let Some(k) = QVOL_NAMES.iter().position(|n| *n == name) {
            return Some(StaticFeature::QVol(k as u8));
        }
        if let Some(j) = QMOVE_NAMES.iter().position(|n| *n == name) {
            return Some(StaticFeature::QMove(j as u8));
        }
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
