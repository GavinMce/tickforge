//! Daily bars (Databento `ohlcv-1d` CSV) and the symbology that says which symbol an id was on a day.

use tf_alpaca::json::Json;

/// One symbol's day. Prices are raw (1e-9 dollars).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bar {
    /// Days since 1970-01-01 of the session.
    pub day: i64,
    pub id: u32,
    pub open: i64,
    pub high: i64,
    pub low: i64,
    pub close: i64,
    pub volume: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadError {
    pub line: usize,
    pub why: String,
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "line {}: {}", self.line, self.why)
    }
}

impl std::error::Error for ReadError {}

const HEADER: &str = "ts_event,rtype,publisher_id,instrument_id,open,high,low,close,volume";
const DAY_NS: u64 = 86_400_000_000_000;

/// Read the CSV exactly as Databento writes it with fixed-point prices. Anything else is refused:
/// a line that does not parse is an error, not a skipped row, since a missing bar changes an average.
pub fn read_bars(text: &str) -> Result<Vec<Bar>, ReadError> {
    let mut lines = text.lines().enumerate();
    match lines.next() {
        Some((_, h)) if h == HEADER => {}
        _ => {
            return Err(ReadError {
                line: 1,
                why: format!("expected the header `{HEADER}` (fixed-point prices, no symbols)"),
            });
        }
    }
    let mut out = Vec::new();
    for (i, line) in lines {
        let err = |why: String| ReadError { line: i + 1, why };
        let c: Vec<&str> = line.split(',').collect();
        if c.len() != 9 {
            return Err(err(format!("{} fields, expected 9", c.len())));
        }
        let n = |k: usize| {
            c[k].parse::<u64>()
                .map_err(|_| err(format!("`{}` is not a number", c[k])))
        };
        let p =
            |k: usize| i64::try_from(n(k)?).map_err(|_| err(format!("`{}` is too large", c[k])));
        if n(1)? != 35 {
            return Err(err(format!("rtype {} is not a daily bar (35)", c[1])));
        }
        let ts = n(0)?;
        if ts % DAY_NS != 0 {
            return Err(err("a daily bar starts at midnight UTC".to_owned()));
        }
        let (open, high, low, close) = (p(4)?, p(5)?, p(6)?, p(7)?);
        if high < open.max(close) || low > open.min(close) {
            return Err(err(
                "the bar's high and low do not contain its open and close".to_owned(),
            ));
        }
        out.push(Bar {
            day: i64::try_from(ts / DAY_NS).map_err(|_| err("date out of range".to_owned()))?,
            id: u32::try_from(n(3)?).map_err(|_| err("instrument id too large".to_owned()))?,
            open,
            high,
            low,
            close,
            volume: n(8)?,
        });
    }
    Ok(out)
}

/// Days since 1970-01-01 to `YYYY-MM-DD` (proleptic Gregorian).
pub fn date_text(days: i64) -> String {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

/// `YYYY-MM-DD` to days since 1970-01-01.
pub fn date_days(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let (y, m, d) = (
        s[0..4].parse::<i64>().ok()?,
        s[5..7].parse::<i64>().ok()?,
        s[8..10].parse::<i64>().ok()?,
    );
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let y = y - i64::from(m <= 2);
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    (date_text(days) == s).then_some(days)
}

/// Which symbol an instrument id was on a given day (Databento's `symbology.resolve` result).
#[derive(Clone, Debug, Default)]
pub struct Symbology {
    /// `(id, first day, last day exclusive, symbol)`, sorted by id then first day.
    rows: Vec<(u32, i64, i64, String)>,
}

impl Symbology {
    pub fn parse(text: &str) -> Result<Symbology, String> {
        let json = Json::parse(text).map_err(|e| e.to_string())?;
        let Some(Json::Obj(result)) = json.get("result") else {
            return Err("no `result` object".to_owned());
        };
        let mut rows = Vec::new();
        for (sym, spans) in result {
            let Json::Arr(spans) = spans else {
                return Err(format!("{sym}: expected a list of date ranges"));
            };
            for s in spans {
                let day = |k: &str| {
                    s.str_at(k)
                        .and_then(date_days)
                        .ok_or_else(|| format!("{sym}: bad date `{k}`"))
                };
                let id = s
                    .str_at("s")
                    .and_then(|v| v.parse::<u32>().ok())
                    .ok_or_else(|| format!("{sym}: bad instrument id"))?;
                rows.push((id, day("d0")?, day("d1")?, sym.clone()));
            }
        }
        rows.sort();
        Ok(Symbology { rows })
    }

    pub fn symbol(&self, id: u32, day: i64) -> Option<&str> {
        let start = self.rows.partition_point(|r| r.0 < id);
        self.rows[start..]
            .iter()
            .take_while(|r| r.0 == id)
            .find(|r| r.1 <= day && day < r.2)
            .map(|r| r.3.as_str())
    }

    /// Every `(id, symbol)` in force on `day`, by id (one symbol for an id on a day, the first if the vendor says two).
    pub fn names_on(&self, day: i64) -> Vec<(u32, &str)> {
        let mut out: Vec<(u32, &str)> = Vec::new();
        for r in &self.rows {
            if r.1 <= day && day < r.2 && out.last().is_none_or(|l| l.0 != r.0) {
                out.push((r.0, r.3.as_str()));
            }
        }
        out
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}
