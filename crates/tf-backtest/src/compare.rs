//! Line two runs of the same session up, trade by trade.
//!
//! A trade in one run is the same trade in the other when it is on the same instrument and
//! the two holds overlap in time; failing that, the earliest left-over trade on the same
//! instrument (a different entry on the same symbol is a changed trade). Each trade then reads as the same, changed (and in what),
//! or present in only one run, in which case the note says what the other run did with that
//! symbol before then: the evidence for "this rule set passed on it".

use tf_core::{InstrumentId, Nanos};
use tf_strategy::{Decline, DeclineReason};

use crate::export::RoundTrip;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Same,
    Changed,
    OnlyA,
    OnlyB,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Same => "same",
            Kind::Changed => "changed",
            Kind::OnlyA => "only_a",
            Kind::OnlyB => "only_b",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pair {
    /// Indices into each run's round trips.
    pub a: Option<usize>,
    pub b: Option<usize>,
    pub kind: Kind,
    /// What differs, for a changed pair.
    pub diff: Vec<&'static str>,
    /// For a trade in one run only: what the other run did with the symbol before.
    pub note: String,
}

fn overlaps(a: &RoundTrip, b: &RoundTrip) -> bool {
    a.instrument == b.instrument
        && a.t_in <= b.t_out.unwrap_or(Nanos::MAX)
        && b.t_in <= a.t_out.unwrap_or(Nanos::MAX)
}

fn differences(a: &RoundTrip, b: &RoundTrip) -> Vec<&'static str> {
    let mut d = Vec::new();
    if a.t_in != b.t_in {
        d.push("entry time");
    }
    if a.entry_px != b.entry_px {
        d.push("entry price");
    }
    if a.qty != b.qty {
        d.push("size");
    }
    if a.t_out != b.t_out {
        d.push("exit time");
    }
    if a.exit_px != b.exit_px {
        d.push("exit price");
    }
    d
}

fn what_the_other_did(
    other: &[Decline],
    instrument: InstrumentId,
    before: Nanos,
    t0: Nanos,
) -> String {
    match other
        .iter()
        .filter(|d| d.instrument == instrument && d.ts <= before)
        .max_by_key(|d| d.ts)
    {
        Some(d) => format!(
            "the other run gave up on it at {:.1} s: {}",
            (i128::from(d.ts) - i128::from(t0)) as f64 / 1e9,
            match d.reason {
                DeclineReason::Dangerous => "dangerous pullback",
                DeclineReason::TooOld => "pullback went on too long",
            }
        ),
        None => "the other run never entered or gave up on it".to_owned(),
    }
}

/// Compare run A with run B. `t0` is the session start, used only in the notes.
pub fn compare(
    a: &[RoundTrip],
    a_declines: &[Decline],
    b: &[RoundTrip],
    b_declines: &[Decline],
    t0: Nanos,
) -> Vec<Pair> {
    let mut used_b = vec![false; b.len()];
    let mut partner: Vec<Option<(usize, bool)>> = vec![None; a.len()];
    // First by overlap, then (for what is left) by symbol in order: the same symbol traded at a
    // different time is a changed trade, not an unrelated one.
    for pass in 0..2 {
        for (i, ta) in a.iter().enumerate() {
            if partner[i].is_some() {
                continue;
            }
            let hit = b.iter().enumerate().find(|(j, tb)| {
                !used_b[*j]
                    && if pass == 0 {
                        overlaps(ta, tb)
                    } else {
                        ta.instrument == tb.instrument
                    }
            });
            if let Some((j, _)) = hit {
                used_b[j] = true;
                partner[i] = Some((j, pass == 1));
            }
        }
    }
    let mut pairs = Vec::new();
    for (i, ta) in a.iter().enumerate() {
        match partner[i] {
            Some((j, apart)) => {
                let diff = differences(ta, &b[j]);
                pairs.push(Pair {
                    a: Some(i),
                    b: Some(j),
                    kind: if diff.is_empty() {
                        Kind::Same
                    } else {
                        Kind::Changed
                    },
                    diff,
                    note: if apart {
                        "same symbol, but the two holds did not overlap".to_owned()
                    } else {
                        String::new()
                    },
                });
            }
            None => pairs.push(Pair {
                a: Some(i),
                b: None,
                kind: Kind::OnlyA,
                diff: Vec::new(),
                note: what_the_other_did(b_declines, ta.instrument, ta.t_in, t0),
            }),
        }
    }
    for (j, tb) in b.iter().enumerate().filter(|(j, _)| !used_b[*j]) {
        pairs.push(Pair {
            a: None,
            b: Some(j),
            kind: Kind::OnlyB,
            diff: Vec::new(),
            note: what_the_other_did(a_declines, tb.instrument, tb.t_in, t0),
        });
    }
    pairs.sort_by_key(|p| {
        let t = |side: &[RoundTrip], i: Option<usize>| i.map(|i| side[i].t_in);
        t(a, p.a).or(t(b, p.b)).unwrap_or(0)
    });
    pairs
}

/// The comparison as JSON, for the viewer. Times are seconds since `t0`.
pub fn to_json(pairs: &[Pair]) -> String {
    let mut s = String::from("{\"a\":0,\"b\":1,\"pairs\":[");
    for (k, p) in pairs.iter().enumerate() {
        if k > 0 {
            s.push(',');
        }
        let idx = |v: Option<usize>| v.map_or("null".to_owned(), |v| v.to_string());
        let diff: Vec<String> = p.diff.iter().map(|d| format!("\"{d}\"")).collect();
        s.push_str(&format!(
            "{{\"a\":{},\"b\":{},\"kind\":\"{}\",\"diff\":[{}],\"note\":\"{}\"}}",
            idx(p.a),
            idx(p.b),
            p.kind.name(),
            diff.join(","),
            p.note.replace('\\', "\\\\").replace('"', "\\\"")
        ));
    }
    s.push_str("]}");
    s
}
