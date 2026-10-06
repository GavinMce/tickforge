//! The decision log, and the check that a replay reproduced it (E18-S06).
//!
//! A live day writes a log of what the host decided: every intent with the gateway's answer, every tier
//! change, every fill and every operator action, each with the number of the event it happened at and
//! its time. After the day the capture of the market is replayed through the same host and the second
//! log is compared with the first, record by record. They are the same, or the first difference is
//! reported with its time and symbol.
//!
//! The log is text, one record a line, so it can be kept, diffed and read by a person.

use std::fmt::Write as _;

use tf_core::{InstrumentId, Nanos, SymbolTable};
use tf_strategy::intent::{Purpose, Side};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Answer {
    /// The gateway accepted it as this order.
    Accepted(u64),
    /// The gateway refused it, for this reason (`tf_risk::reason_name`).
    Rejected(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Rec {
    /// A strategy's (or the host's own) intent and the gateway's answer.
    Decision {
        idx: u64,
        ts: Nanos,
        strategy: u16,
        seq: u64,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        purpose: Purpose,
        /// The worst price the intent allows, raw.
        limit: i64,
        reason: u16,
        answer: Answer,
    },
    /// A promotion or demotion in Tier 1.
    Tier {
        idx: u64,
        ts: Nanos,
        instrument: InstrumentId,
        promote: bool,
        reason: u8,
        score: i64,
    },
    /// A fill reported by a broker.
    Fill {
        idx: u64,
        ts: Nanos,
        order: u64,
        instrument: InstrumentId,
        qty: u32,
        px: i64,
    },
    /// Something an operator or the host's own schedule did: adding a strategy, killing one, the kill
    /// switch, the end of the day.
    Action { idx: u64, ts: Nanos, what: String },
}

impl Rec {
    pub fn idx(&self) -> u64 {
        match self {
            Rec::Decision { idx, .. }
            | Rec::Tier { idx, .. }
            | Rec::Fill { idx, .. }
            | Rec::Action { idx, .. } => *idx,
        }
    }

    pub fn ts(&self) -> Nanos {
        match self {
            Rec::Decision { ts, .. }
            | Rec::Tier { ts, .. }
            | Rec::Fill { ts, .. }
            | Rec::Action { ts, .. } => *ts,
        }
    }

    pub fn instrument(&self) -> Option<InstrumentId> {
        match self {
            Rec::Decision { instrument, .. }
            | Rec::Tier { instrument, .. }
            | Rec::Fill { instrument, .. } => Some(*instrument),
            Rec::Action { .. } => None,
        }
    }

    pub fn line(&self) -> String {
        match self {
            Rec::Decision {
                idx,
                ts,
                strategy,
                seq,
                instrument,
                side,
                qty,
                purpose,
                limit,
                reason,
                answer,
            } => {
                let side = match side {
                    Side::Buy => 'B',
                    Side::Sell => 'S',
                    Side::SellShort => 'X',
                };
                let purpose = match purpose {
                    Purpose::Open => 'O',
                    Purpose::Close => 'C',
                };
                let answer = match answer {
                    Answer::Accepted(o) => format!("A{o}"),
                    Answer::Rejected(r) => format!("R:{r}"),
                };
                format!(
                    "d {idx} {ts} {strategy} {seq} {instrument} {side} {qty} {purpose} {limit} {reason} {answer}"
                )
            }
            Rec::Tier {
                idx,
                ts,
                instrument,
                promote,
                reason,
                score,
            } => {
                format!(
                    "t {idx} {ts} {instrument} {} {reason} {score}",
                    if *promote { "promote" } else { "demote" }
                )
            }
            Rec::Fill {
                idx,
                ts,
                order,
                instrument,
                qty,
                px,
            } => format!("f {idx} {ts} {order} {instrument} {qty} {px}"),
            Rec::Action { idx, ts, what } => format!("a {idx} {ts} {what}"),
        }
    }

    pub fn parse(line: &str) -> Result<Rec, String> {
        let w: Vec<&str> = line.split(' ').collect();
        let n = |i: usize| -> Result<u64, String> {
            w.get(i)
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| format!("field {i} of `{line}`"))
        };
        let i = |k: usize| -> Result<i64, String> {
            w.get(k)
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| format!("field {k} of `{line}`"))
        };
        let u32_ = |k: usize| -> Result<u32, String> {
            u32::try_from(n(k)?).map_err(|_| format!("field {k} of `{line}` is too large"))
        };
        match w.first().copied() {
            Some("d") if w.len() == 12 => {
                let side = match w[6] {
                    "B" => Side::Buy,
                    "S" => Side::Sell,
                    "X" => Side::SellShort,
                    o => return Err(format!("side `{o}`")),
                };
                let purpose = match w[8] {
                    "O" => Purpose::Open,
                    "C" => Purpose::Close,
                    o => return Err(format!("purpose `{o}`")),
                };
                let answer = if let Some(o) = w[11].strip_prefix('A') {
                    Answer::Accepted(o.parse().map_err(|_| format!("answer `{}`", w[11]))?)
                } else if let Some(r) = w[11].strip_prefix("R:") {
                    Answer::Rejected(r.to_owned())
                } else {
                    return Err(format!("answer `{}`", w[11]));
                };
                Ok(Rec::Decision {
                    idx: n(1)?,
                    ts: n(2)?,
                    strategy: u16::try_from(n(3)?).map_err(|_| "strategy".to_owned())?,
                    seq: n(4)?,
                    instrument: u32_(5)?,
                    side,
                    qty: u32_(7)?,
                    purpose,
                    limit: i(9)?,
                    reason: u16::try_from(n(10)?).map_err(|_| "reason".to_owned())?,
                    answer,
                })
            }
            Some("t") if w.len() == 7 => Ok(Rec::Tier {
                idx: n(1)?,
                ts: n(2)?,
                instrument: u32_(3)?,
                promote: match w[4] {
                    "promote" => true,
                    "demote" => false,
                    o => return Err(format!("tier action `{o}`")),
                },
                reason: u8::try_from(n(5)?).map_err(|_| "reason".to_owned())?,
                score: i(6)?,
            }),
            Some("f") if w.len() == 7 => Ok(Rec::Fill {
                idx: n(1)?,
                ts: n(2)?,
                order: n(3)?,
                instrument: u32_(4)?,
                qty: u32_(5)?,
                px: i(6)?,
            }),
            Some("a") if w.len() >= 4 => Ok(Rec::Action {
                idx: n(1)?,
                ts: n(2)?,
                what: w[3..].join(" "),
            }),
            _ => Err(format!("a line that is not a record: `{line}`")),
        }
    }
}

/// A fingerprint of the symbols' names by instrument id, so a replay that numbered instruments
/// differently is told apart from one that decided differently.
pub fn symbols_fingerprint(t: &SymbolTable) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for id in 0..t.len() as u32 {
        for b in t
            .name(id)
            .unwrap_or("")
            .bytes()
            .chain(std::iter::once(0xff))
        {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    h
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Log {
    pub id_space: usize,
    pub symbols: u64,
    pub symbol_count: u64,
    pub recs: Vec<Rec>,
}

impl Log {
    pub fn new(id_space: usize, symbols: &SymbolTable) -> Log {
        Log {
            id_space,
            symbols: symbols_fingerprint(symbols),
            symbol_count: symbols.len() as u64,
            recs: Vec::new(),
        }
    }

    /// The first three lines of the text form.
    pub fn header(&self) -> String {
        format!(
            "decisions v1\nid_space {}\nsymbols {:016x} {}\n",
            self.id_space, self.symbols, self.symbol_count
        )
    }

    /// The last line, which says how many records there were.
    pub fn footer(&self) -> String {
        format!("end {}\n", self.recs.len())
    }

    pub fn render(&self) -> String {
        let mut s = self.header();
        for r in &self.recs {
            let _ = writeln!(s, "{}", r.line());
        }
        s.push_str(&self.footer());
        s
    }

    /// Read a log that may have been cut short (the process died): the records there are, and whether
    /// the `end` line was found. A damaged line is still an error.
    pub fn parse_partial(text: &str) -> Result<(Log, bool), String> {
        match Log::parse(text) {
            Ok(l) => Ok((l, true)),
            Err(e) if e.contains("cut short") => {
                let n = text.lines().skip(3).filter(|l| !l.is_empty()).count();
                let closed = format!("{}\nend {n}\n", text.trim_end_matches('\n'));
                Log::parse(&closed).map(|l| (l, false))
            }
            Err(e) => Err(e),
        }
    }

    pub fn parse(text: &str) -> Result<Log, String> {
        let mut lines = text.lines();
        if lines.next() != Some("decisions v1") {
            return Err("expected `decisions v1`".to_owned());
        }
        let id_space = lines
            .next()
            .and_then(|l| l.strip_prefix("id_space "))
            .and_then(|v| v.parse().ok())
            .ok_or("expected `id_space N`")?;
        let sym = lines
            .next()
            .and_then(|l| l.strip_prefix("symbols "))
            .ok_or("expected `symbols FP COUNT`")?;
        let (fp, count) = sym.split_once(' ').ok_or("expected `symbols FP COUNT`")?;
        let symbols = u64::from_str_radix(fp, 16).map_err(|_| "symbols fingerprint")?;
        let symbol_count = count.parse().map_err(|_| "symbols count")?;
        let mut recs = Vec::new();
        let mut end = None;
        for l in lines {
            if let Some(n) = l.strip_prefix("end ") {
                end = Some(n.parse::<usize>().map_err(|_| "end count")?);
                continue;
            }
            if end.is_some() {
                return Err("records after `end`".to_owned());
            }
            recs.push(Rec::parse(l)?);
        }
        match end {
            Some(n) if n == recs.len() => Ok(Log {
                id_space,
                symbols,
                symbol_count,
                recs,
            }),
            Some(n) => Err(format!(
                "`end {n}` but {} records: the log is damaged",
                recs.len()
            )),
            None => Err("no `end` line: the log was cut short".to_owned()),
        }
    }
}

/// Where two logs first differ.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Difference {
    /// Which record (0 for a difference in the header).
    pub record: usize,
    /// The event it happened at, and its time, from the live record if there is one.
    pub event: u64,
    pub ts: Nanos,
    pub symbol: Option<String>,
    pub live: String,
    pub replay: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    Equal { records: usize },
    Differs(Difference),
}

pub(crate) fn clock(ts: Nanos) -> String {
    let day = 86_400_000_000_000u64;
    let t = ts % day;
    let s = t / 1_000_000_000;
    format!(
        "{:02}:{:02}:{:02}.{:09} UTC",
        s / 3600,
        s / 60 % 60,
        s % 60,
        t % 1_000_000_000
    )
}

impl Verdict {
    pub fn is_equal(&self) -> bool {
        matches!(self, Verdict::Equal { .. })
    }

    pub fn report(&self) -> String {
        match self {
            Verdict::Equal { records } => format!(
                "the replay reproduced all {records} decisions, answers, tier changes and fills\n"
            ),
            Verdict::Differs(d) => format!(
                "the replay differs at record {} (event {}, {}{}):\n  live:   {}\n  replay: {}\n",
                d.record,
                d.event,
                clock(d.ts),
                d.symbol
                    .as_ref()
                    .map_or(String::new(), |s| format!(", {s}")),
                d.live,
                d.replay
            ),
        }
    }
}

/// Compare a live log with a replay's. `names` says what a symbol is called.
pub fn compare(live: &Log, replay: &Log, names: &SymbolTable) -> Verdict {
    let header = |what: &str, a: String, b: String| {
        Verdict::Differs(Difference {
            record: 0,
            event: 0,
            ts: 0,
            symbol: None,
            live: format!("{what} {a}"),
            replay: format!("{what} {b}"),
        })
    };
    if live.id_space != replay.id_space {
        return header(
            "id_space",
            live.id_space.to_string(),
            replay.id_space.to_string(),
        );
    }
    if (live.symbols, live.symbol_count) != (replay.symbols, replay.symbol_count) {
        return header(
            "symbols",
            format!("{:016x} {}", live.symbols, live.symbol_count),
            format!("{:016x} {}", replay.symbols, replay.symbol_count),
        );
    }
    let n = live.recs.len().max(replay.recs.len());
    for i in 0..n {
        let (a, b) = (live.recs.get(i), replay.recs.get(i));
        if a == b {
            continue;
        }
        let at = a.or(b).expect("one of them exists");
        return Verdict::Differs(Difference {
            record: i + 1,
            event: at.idx(),
            ts: at.ts(),
            symbol: at.instrument().map(|id| {
                names
                    .name(id)
                    .map_or_else(|| format!("instrument {id}"), str::to_owned)
            }),
            live: a.map_or_else(|| "(nothing: the live log ends here)".to_owned(), Rec::line),
            replay: b.map_or_else(|| "(nothing: the replay ends here)".to_owned(), Rec::line),
        });
    }
    Verdict::Equal {
        records: live.recs.len(),
    }
}
