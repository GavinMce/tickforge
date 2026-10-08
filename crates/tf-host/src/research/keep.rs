//! What a day keeps beside its trips (E19-S33): the decision log, the strategies' traces and the market around every trade.
//!
//! These files are what the backtest view reads, so that a result can be looked at with neither the strategies' code nor the
//! store of market data to hand. Each is text, ends with a checksum, and begins with the day it is of, the configuration
//! it was made under and the day's outcome hash, so a file from another day, another run or another configuration is refused
//! when it is read, and a day is only whole if its companions are:
//!
//! ```text
//! <kind> v1
//! day <YYYY-MM-DD>
//! config <16 hex digits>
//! outcome <16 hex digits>
//! <the body>
//! end <checksum of everything above, 16 hex digits>
//! ```
//!
//! The log's body is the host's decision log ([`crate::Log::render`]); the traces' is [`tf_strategy::trace`]'s text; the
//! evidence's is below and is kept compressed (zstd).

use std::collections::BTreeMap;
use std::path::PathBuf;

use tf_capture::CaptureReplay;
use tf_core::{Dedupe, Event, InstrumentId, Nanos, SymbolTable};
use tf_provider::{Poll, Provider};

use super::trips::Trip;
use crate::def::fnv;
use crate::replay::learn_symbols;

/// How much of the market to keep around a trade.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EvidenceWindow {
    /// Before the trade's entry, in nanoseconds.
    pub before: Nanos,
    /// After its exit, in nanoseconds.
    pub after: Nanos,
}

impl Default for EvidenceWindow {
    /// Ten minutes before the entry to two after the exit.
    fn default() -> Self {
        EvidenceWindow {
            before: 600 * 1_000_000_000,
            after: 120 * 1_000_000_000,
        }
    }
}

/// One thing the market did, as the host saw it, in the integers of the engine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EvEvent {
    Trade {
        ts: Nanos,
        px: i64,
        size: u32,
    },
    Quote {
        ts: Nanos,
        bid: i64,
        ask: i64,
        bid_sz: u32,
        ask_sz: u32,
    },
    Status {
        ts: Nanos,
        kind: u8,
        lo: i64,
        hi: i64,
    },
}

impl EvEvent {
    pub fn ts(&self) -> Nanos {
        match *self {
            EvEvent::Trade { ts, .. } | EvEvent::Quote { ts, .. } | EvEvent::Status { ts, .. } => {
                ts
            }
        }
    }

    fn of(ev: &Event) -> Option<EvEvent> {
        match ev {
            Event::Trade(t) => Some(EvEvent::Trade {
                ts: t.hdr.ts_recv,
                px: t.px.raw(),
                size: t.size,
            }),
            Event::Quote(q) => Some(EvEvent::Quote {
                ts: q.hdr.ts_recv,
                bid: q.bid_px.raw(),
                ask: q.ask_px.raw(),
                bid_sz: q.bid_sz,
                ask_sz: q.ask_sz,
            }),
            Event::Status(s) => Some(EvEvent::Status {
                ts: s.hdr.ts_recv,
                kind: s.kind as u8,
                lo: s.lo.raw(),
                hi: s.hi.raw(),
            }),
            _ => None,
        }
    }

    fn line(&self) -> String {
        match *self {
            EvEvent::Trade { ts, px, size } => format!("t\t{ts}\t{px}\t{size}"),
            EvEvent::Quote {
                ts,
                bid,
                ask,
                bid_sz,
                ask_sz,
            } => format!("q\t{ts}\t{bid}\t{ask}\t{bid_sz}\t{ask_sz}"),
            EvEvent::Status { ts, kind, lo, hi } => format!("s\t{ts}\t{kind}\t{lo}\t{hi}"),
        }
    }
}

/// The market around the day's trades: for each symbol traded, every trade, quote and status in the windows around its trips.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Evidence {
    pub window: Option<EvidenceWindow>,
    pub symbols: BTreeMap<String, Vec<EvEvent>>,
}

impl Evidence {
    /// The events of `symbol` from `from` to `to`, inclusive.
    pub fn slice(&self, symbol: &str, from: Nanos, to: Nanos) -> Vec<EvEvent> {
        self.symbols
            .get(symbol)
            .map(|v| {
                v.iter()
                    .copied()
                    .filter(|e| e.ts() >= from && e.ts() <= to)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The trips with nothing kept around them, by day, symbol, strategy and entry time.
    pub fn missing(&self, day: &str, trips: &[Trip], w: EvidenceWindow) -> Vec<String> {
        trips
            .iter()
            .filter(|t| {
                self.slice(
                    &t.symbol,
                    t.entry_ts.saturating_sub(w.before),
                    t.exit_ts.saturating_add(w.after),
                )
                .is_empty()
            })
            .map(|t| format!("{day} {} {} {}", t.symbol, t.name, t.entry_ts))
            .collect()
    }

    fn body(&self) -> String {
        let w = self.window.unwrap_or_default();
        let mut s = format!("window\t{}\t{}\n", w.before, w.after);
        for (sym, evs) in &self.symbols {
            s.push_str(&format!("symbol\t{sym}\t{}\n", evs.len()));
            for e in evs {
                s.push_str(&e.line());
                s.push('\n');
            }
        }
        s
    }

    fn from_body(body: &str) -> Result<Evidence, String> {
        let bad = |m: String| m;
        let mut lines = body.lines();
        let w: Vec<&str> = lines.next().unwrap_or("").split('\t').collect();
        let ["window", before, after] = w.as_slice() else {
            return Err(bad("no `window` line".into()));
        };
        let num = |s: &str| {
            s.parse::<u64>()
                .map_err(|_| format!("`{s}` is not a number"))
        };
        let inum = |s: &str| {
            s.parse::<i64>()
                .map_err(|_| format!("`{s}` is not a number"))
        };
        let mut ev = Evidence {
            window: Some(EvidenceWindow {
                before: num(before)?,
                after: num(after)?,
            }),
            symbols: BTreeMap::new(),
        };
        let mut open: Option<(String, usize)> = None;
        for line in lines {
            let w: Vec<&str> = line.split('\t').collect();
            match w.as_slice() {
                ["symbol", name, n] => {
                    if let Some((s, want)) = &open {
                        if ev.symbols[s].len() != *want {
                            return Err(format!(
                                "{s} has {} events, not {want}",
                                ev.symbols[s].len()
                            ));
                        }
                    }
                    let n = num(n)? as usize;
                    if ev.symbols.insert((*name).to_owned(), Vec::new()).is_some() {
                        return Err(format!("{name} twice"));
                    }
                    open = Some(((*name).to_owned(), n));
                }
                [kind, rest @ ..] => {
                    let Some((sym, _)) = &open else {
                        return Err("an event before any `symbol`".into());
                    };
                    let e = match (*kind, rest) {
                        ("t", [ts, px, size]) => EvEvent::Trade {
                            ts: num(ts)?,
                            px: inum(px)?,
                            size: num(size)? as u32,
                        },
                        ("q", [ts, bid, ask, bs, asz]) => EvEvent::Quote {
                            ts: num(ts)?,
                            bid: inum(bid)?,
                            ask: inum(ask)?,
                            bid_sz: num(bs)? as u32,
                            ask_sz: num(asz)? as u32,
                        },
                        ("s", [ts, kind, lo, hi]) => EvEvent::Status {
                            ts: num(ts)?,
                            kind: num(kind)? as u8,
                            lo: inum(lo)?,
                            hi: inum(hi)?,
                        },
                        _ => return Err(format!("a line this version does not know: `{line}`")),
                    };
                    ev.symbols.get_mut(sym).expect("opened above").push(e);
                }
                [] => {}
            }
        }
        if let Some((s, want)) = &open {
            if ev.symbols[s].len() != *want {
                return Err(format!(
                    "{s} has {} events, not {want}",
                    ev.symbols[s].len()
                ));
            }
        }
        Ok(ev)
    }
}

/// Read the day's files once more, in order, and keep what the market did around each trip: for each symbol traded, the
/// events (as the host saw them: the same removal of what the gateway sent twice) in the windows around its trips.
pub fn gather_evidence(
    files: &[PathBuf],
    trips: &[Trip],
    w: EvidenceWindow,
) -> Result<Evidence, String> {
    gather_evidence_with(files, &learn_symbols(files), trips, w)
}

/// [`gather_evidence`] for files whose symbols are known already (the run that made the trips read them): one read of the
/// files, not two.
pub fn gather_evidence_with(
    files: &[PathBuf],
    symbols: &SymbolTable,
    trips: &[Trip],
    w: EvidenceWindow,
) -> Result<Evidence, String> {
    let mut wins: BTreeMap<InstrumentId, Vec<(Nanos, Nanos)>> = BTreeMap::new();
    for t in trips {
        if let Some(id) = symbols.get(&t.symbol) {
            wins.entry(id).or_default().push((
                t.entry_ts.saturating_sub(w.before),
                t.exit_ts.saturating_add(w.after),
            ));
        }
    }
    // Overlapping windows of one symbol are one.
    for v in wins.values_mut() {
        v.sort_unstable();
        let mut merged: Vec<(Nanos, Nanos)> = Vec::new();
        for &(a, b) in v.iter() {
            match merged.last_mut() {
                Some(last) if a <= last.1 => last.1 = last.1.max(b),
                _ => merged.push((a, b)),
            }
        }
        *v = merged;
    }
    let mut kept: BTreeMap<InstrumentId, Vec<EvEvent>> =
        wins.keys().map(|&k| (k, Vec::new())).collect();
    let mut source = CaptureReplay::from_files(files.to_vec());
    let mut dedupe = Dedupe::new();
    let mut raw: Vec<Event> = Vec::new();
    loop {
        raw.clear();
        match source.poll(&mut raw, 4096) {
            Poll::Events(_) => {}
            Poll::Idle => continue,
            _ => break,
        }
        for ev in raw.iter().filter(|e| dedupe.admit(e)) {
            let Some(ws) = wins.get(&ev.instrument()) else {
                continue;
            };
            let ts = ev.ts_recv();
            let i = ws.partition_point(|&(_, end)| end < ts);
            if ws.get(i).is_some_and(|&(start, _)| start <= ts) {
                if let Some(e) = EvEvent::of(ev) {
                    kept.get_mut(&ev.instrument())
                        .expect("a window's symbol")
                        .push(e);
                }
            }
        }
    }
    if let Some(why) = source.failure() {
        return Err(format!("the data cannot be read: {why}"));
    }
    Ok(Evidence {
        window: Some(w),
        symbols: kept
            .into_iter()
            .filter_map(|(id, v)| symbols.name(id).map(|n| (n.to_owned(), v)))
            .collect(),
    })
}

/// A companion file's text: the day it is of, the configuration and the outcome it belongs to, the body, and a checksum.
pub(crate) fn wrap(kind: &str, day: &str, config: u64, outcome: u64, body: &str) -> String {
    let s = format!("{kind} v1\nday {day}\nconfig {config:016x}\noutcome {outcome:016x}\n{body}");
    format!("{s}end {:016x}\n", fnv(&[s.as_bytes()]))
}

/// The body of a companion file, if the file is whole, of this kind, and of this day, configuration and outcome.
pub(crate) fn unwrap<'a>(
    kind: &str,
    text: &'a str,
    day: &str,
    config: u64,
    outcome: u64,
) -> Result<&'a str, String> {
    let end = text
        .rfind("end ")
        .filter(|&i| i == 0 || text.as_bytes()[i - 1] == b'\n')
        .ok_or("cut short: no `end`")?;
    let (head, tail) = text.split_at(end);
    if tail.matches('\n').count() != 1 || !tail.ends_with('\n') {
        return Err("text after `end`".into());
    }
    let want = u64::from_str_radix(tail[4..tail.len() - 1].trim(), 16)
        .map_err(|_| "the checksum is not hexadecimal")?;
    if fnv(&[head.as_bytes()]) != want {
        return Err("the checksum does not match: edited or damaged".into());
    }
    let mut it = head.splitn(5, '\n');
    let expect = [
        format!("{kind} v1"),
        format!("day {day}"),
        format!("config {config:016x}"),
        format!("outcome {outcome:016x}"),
    ];
    for (i, want) in expect.iter().enumerate() {
        let got = it.next().unwrap_or("");
        if got != want {
            return Err(match i {
                0 => format!("not `{want}`"),
                1 => format!("it is of another day: `{got}`, wanted `{want}`"),
                2 => format!("made under another configuration: `{got}`, wanted `{want}`"),
                _ => format!("of another outcome: `{got}`, wanted `{want}`"),
            });
        }
    }
    Ok(it.next().unwrap_or(""))
}

/// The evidence as a companion file, compressed.
pub(crate) fn evidence_file(day: &str, config: u64, outcome: u64, e: &Evidence) -> Vec<u8> {
    let text = wrap("research evidence", day, config, outcome, &e.body());
    zstd::encode_all(text.as_bytes(), 3).expect("compressing into memory")
}

/// Evidence from its compressed file, checked as [`unwrap`] checks.
pub(crate) fn read_evidence(
    bytes: &[u8],
    day: &str,
    config: u64,
    outcome: u64,
) -> Result<Evidence, String> {
    let raw = zstd::decode_all(bytes).map_err(|e| format!("not compressed text: {e}"))?;
    let text = String::from_utf8(raw).map_err(|_| "not text".to_owned())?;
    Evidence::from_body(unwrap("research evidence", &text, day, config, outcome)?)
}
