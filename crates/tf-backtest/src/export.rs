//! Export a backtest as one self-contained JSON document for the trade explorer.
//!
//! The viewer is a static page, so everything it draws is in the file: per instrument the
//! one-second bars, every trade print, a per-second quote, the scanner's hits, the
//! promoter's tier moves and a per-second series of pullback features; per order the
//! intent with the gateway's answer and its fills; per round trip the stop's path; and,
//! for each entry, the strategy's own **decision trace** (the features it saw and the
//! thresholds in force, with each condition's pass or fail), and for each symbol it
//! watched and declined, why.
//!
//! The decision traces and declines come from the strategy itself, recorded at the moment
//! of the decision. The feature *series* is the exporter's own, recomputed by replaying the
//! tape through a Tier 1 tracker that sees the whole session, so it can differ a little
//! from what the strategy's tracker (which starts when the strategy begins watching) saw;
//! the viewer labels it as context.
//!
//! Times are seconds since the first event; prices are dollars to four decimals (display
//! only: the engine's integers stay in the engine).

use std::fmt::Write;

use tf_core::{Event, InstrumentId, NANOS_PER_SEC, Nanos};
use tf_engine::{Promoter, PromoterConfig, Quote1, Scanner, ScannerConfig, Tier0, Tier1Symbol};
use tf_strategy::rules::{Evaluation, Mode, RuleSet};
use tf_strategy::{Decision, Decline, DeclineReason, EntryTrace, Intent, Purpose, Side};

use crate::{BacktestConfig, BacktestResult};

/// What the export is of.
pub struct ExportMeta<'a> {
    pub strategy: &'a str,
    pub seed: u64,
    pub secs: u64,
}

fn px(raw: i64) -> String {
    // Dollars to four decimals, rounded half away from zero, from integers.
    let scaled = (i128::from(raw).abs() + 50_000) / 100_000;
    let sign = if raw < 0 && scaled != 0 { "-" } else { "" };
    format!("{sign}{}.{:04}", scaled / 10_000, scaled % 10_000)
}

fn secs(ns: i128) -> String {
    let ms = if ns >= 0 {
        (ns + 500_000) / 1_000_000
    } else {
        -((-ns + 500_000) / 1_000_000)
    };
    let sign = if ms < 0 { "-" } else { "" };
    format!("{sign}{}.{:03}", ms.abs() / 1000, ms.abs() % 1000)
}

fn esc(s: &str) -> String {
    s.chars()
        .flat_map(|c| match c {
            '"' => vec!['\\', '"'],
            '\\' => vec!['\\', '\\'],
            '\n' => vec!['\\', 'n'],
            c if (c as u32) < 0x20 => vec![' '],
            c => vec![c],
        })
        .collect()
}

struct W(String);

impl W {
    fn raw(&mut self, s: &str) {
        self.0.push_str(s);
    }
    fn f(&mut self, args: std::fmt::Arguments<'_>) {
        let _ = self.0.write_fmt(args);
    }
}

fn opt<T: std::fmt::Display>(v: Option<T>) -> String {
    v.map_or("null".to_owned(), |x| x.to_string())
}

fn write_features(w: &mut W, f: &tf_engine::PullbackFeatures) {
    w.f(format_args!(
        "{{\"low\":{},\"high\":{},\"pullback_low\":{},\"last\":{},\"impulse_secs\":{},\"secs_since_high\":{},\"depth\":{},\"retrace\":{},\"volume_ratio\":{},\"higher_lows\":{},\"tape\":{},\"tape_ratio\":{},\"bid_support\":{},\"spread\":{}}}",
        px(f.impulse_low.raw()),
        px(f.impulse_high.raw()),
        px(f.pullback_low.raw()),
        px(f.last.raw()),
        f.impulse_secs,
        f.secs_since_high,
        f.depth_permille,
        f.retrace_now_permille,
        opt(f.volume_ratio_permille),
        f.higher_lows,
        f.recent_trades_per_sec_x1000,
        opt(f.tape_ratio_permille),
        opt(f.bid_support_permille),
        f.spread.map_or("null".to_owned(), px),
    ));
}

fn write_evaluations(w: &mut W, evals: &[Evaluation]) {
    w.raw("[");
    for (k, e) in evals.iter().enumerate() {
        if k > 0 {
            w.raw(",");
        }
        let param = e
            .condition
            .threshold
            .param_name()
            .map_or("null".to_owned(), |n| format!("\"{}\"", esc(n)));
        w.f(format_args!(
            "{{\"stage\":\"{}\",\"mode\":\"{}\",\"feature\":\"{}\",\"value\":{},\"op\":\"{}\",\"threshold\":{},\"param\":{},\"pass\":{}}}",
            e.stage.name(),
            if e.mode == Mode::All { "all" } else { "any" },
            e.condition.feature.name(),
            opt(e.value),
            e.condition.cmp.symbol(),
            e.limit,
            param,
            e.pass
        ));
    }
    w.raw("]");
}

/// Build the JSON document.
pub fn export_json(
    events: &[Event],
    labels: &[String],
    cfg: &BacktestConfig,
    result: &BacktestResult,
    entries: &[EntryTrace],
    declines: &[Decline],
    meta: &ExportMeta<'_>,
) -> String {
    let n = labels.len();
    let t0 = events.first().map_or(0, |e| e.ts_recv());
    let rel = |ts: Nanos| i128::from(ts) - i128::from(t0);

    // Instruments worth showing: any that traded, were watched, or were not quiet.
    let mut shown: Vec<InstrumentId> = (0..n as u32)
        .filter(|&i| labels[i as usize] != "quiet")
        .collect();
    shown.sort_unstable();
    shown.truncate(8);

    // A replay of the tape through Tier 0, a scanner and a promoter, and a full-session Tier 1 tracker
    // per shown instrument (for the context series).
    let mut tier0 = Tier0::new(n);
    let mut scanner =
        Scanner::new(ScannerConfig::default(), n).expect("default scanner config is valid");
    let mut promoter = Promoter::new(PromoterConfig::default(), ScannerConfig::default(), n)
        .expect("default promoter config is valid");
    let mut hits = Vec::new();
    let mut tier = Vec::new();
    let mut trackers: Vec<(InstrumentId, Tier1Symbol, u64)> = shown
        .iter()
        .map(|&i| (i, Tier1Symbol::new(), u64::MAX))
        .collect();
    let mut series: Vec<Vec<(u64, tf_engine::PullbackFeatures)>> = vec![Vec::new(); shown.len()];
    let mut bars: Vec<Vec<[i64; 6]>> = vec![Vec::new(); n];
    let mut ticks: Vec<Vec<(Nanos, i64, u32)>> = vec![Vec::new(); n];
    let mut quotes: Vec<Vec<(u64, i64, i64)>> = vec![Vec::new(); n];
    for ev in events {
        tier0.on_event(ev);
        scanner.on_event(&tier0, ev, &mut hits);
        promoter.on_event(&tier0, ev, &mut tier);
        let id = ev.instrument();
        let sec = (ev.ts_recv() - t0) / NANOS_PER_SEC;
        match ev {
            Event::Trade(t) if (id as usize) < n => {
                let p = t.px.raw();
                ticks[id as usize].push((t.hdr.ts_recv, p, t.size));
                let b = &mut bars[id as usize];
                match b.last_mut() {
                    Some(last) if last[0] == sec as i64 => {
                        last[2] = last[2].max(p);
                        last[3] = last[3].min(p);
                        last[4] = p;
                        last[5] += i64::from(t.size);
                    }
                    _ => b.push([sec as i64, p, p, p, p, i64::from(t.size)]),
                }
            }
            Event::Quote(q) if (id as usize) < n => {
                let qs = &mut quotes[id as usize];
                let rec = (sec, q.bid_px.raw(), q.ask_px.raw());
                match qs.last_mut() {
                    Some(last) if last.0 == sec => *last = rec,
                    _ => qs.push(rec),
                }
            }
            _ => {}
        }
        if let Some(k) = trackers.iter().position(|(i, _, _)| *i == id) {
            let tr = &mut trackers[k].1;
            match ev {
                Event::Trade(t) => tr.on_trade(t.hdr.ts_recv, t.px, t.size),
                Event::Quote(q) => tr.on_quote(Quote1 {
                    ts: q.hdr.ts_recv,
                    bid: q.bid_px,
                    ask: q.ask_px,
                    bid_sz: q.bid_sz,
                    ask_sz: q.ask_sz,
                }),
                _ => {}
            }
            if trackers[k].2 != sec {
                trackers[k].2 = sec;
                if let Some(f) = trackers[k].1.features() {
                    series[k].push((sec, f));
                }
            }
        }
    }

    let mut w = W(String::new());
    w.f(format_args!(
        "{{\"meta\":{{\"strategy\":\"{}\",\"seed\":{},\"secs\":{},\"fills\":{},\"intents\":{},\"accepted\":{},\"net_pnl\":{}}},",
        esc(meta.strategy),
        meta.seed,
        meta.secs,
        result.fills,
        result.intents,
        result.accepted,
        px(result.report.total.net_pnl().clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64),
    ));
    let p = cfg.params;
    w.f(format_args!(
        "\"params\":{{\"trail_permille\":{},\"max_hold_secs\":{},\"collar_permille\":{},\"stop_buffer_permille\":{},\"entry_notional\":{}}},",
        p.trail_permille,
        p.max_hold_secs,
        p.collar_permille,
        p.stop_buffer_permille,
        px((p.entry_notional.min(i64::MAX as u128)) as i64),
    ));

    let rules = cfg.rules.clone().unwrap_or_else(RuleSet::momentum);
    w.f(format_args!(
        "\"rules\":{{\"id\":\"{:016x}\",\"text\":\"{}\"}},",
        rules.fingerprint(),
        esc(&rules.render()),
    ));

    // Instruments.
    w.raw("\"instruments\":[");
    for (k, &id) in shown.iter().enumerate() {
        let i = id as usize;
        if k > 0 {
            w.raw(",");
        }
        w.f(format_args!(
            "{{\"id\":{id},\"label\":\"{}\",\"name\":\"{}{}\",",
            esc(&labels[i]),
            esc(&labels[i].to_uppercase()),
            id
        ));
        w.raw("\"bars\":[");
        for (j, b) in bars[i].iter().enumerate() {
            if j > 0 {
                w.raw(",");
            }
            w.f(format_args!(
                "[{},{},{},{},{},{}]",
                b[0],
                px(b[1]),
                px(b[2]),
                px(b[3]),
                px(b[4]),
                b[5]
            ));
        }
        w.raw("],\"ticks\":[");
        for (j, (ts, p, sz)) in ticks[i].iter().enumerate() {
            if j > 0 {
                w.raw(",");
            }
            w.f(format_args!("[{},{},{}]", secs(rel(*ts)), px(*p), sz));
        }
        w.raw("],\"quotes\":[");
        for (j, (s, b, a)) in quotes[i].iter().enumerate() {
            if j > 0 {
                w.raw(",");
            }
            w.f(format_args!("[{},{},{}]", s, px(*b), px(*a)));
        }
        w.raw("],\"hits\":[");
        let mut first = true;
        for h in hits.iter().filter(|h| h.instrument == id) {
            if !first {
                w.raw(",");
            }
            first = false;
            w.f(format_args!(
                "[{},{},{},{}]",
                secs(rel(h.ts)),
                h.z_milli,
                h.change_permille,
                h.volume
            ));
        }
        w.raw("],\"tier\":[");
        let mut first = true;
        for c in tier.iter().filter(|c| c.hdr.instrument == id) {
            if !first {
                w.raw(",");
            }
            first = false;
            let a = match c.action {
                tf_core::TierAction::Promote => "promote",
                tf_core::TierAction::Demote => "demote",
            };
            w.f(format_args!(
                "[{},\"{a}\",{}]",
                secs(rel(c.hdr.ts_recv)),
                c.score
            ));
        }
        w.raw("],\"features\":[");
        for (j, (s, f)) in series[k].iter().enumerate() {
            if j > 0 {
                w.raw(",");
            }
            w.f(format_args!(
                "[{},{},{},{},{},{},{},{},{},{}]",
                s,
                f.depth_permille,
                f.retrace_now_permille,
                opt(f.volume_ratio_permille),
                f.higher_lows,
                opt(f.tape_ratio_permille),
                opt(f.bid_support_permille),
                px(f.impulse_high.raw()),
                px(f.impulse_low.raw()),
                px(f.pullback_low.raw()),
            ));
        }
        w.raw("],\"declines\":[");
        let mut first = true;
        for d in declines.iter().filter(|d| d.instrument == id) {
            if !first {
                w.raw(",");
            }
            first = false;
            let why = match d.reason {
                DeclineReason::Dangerous => "dangerous pullback",
                DeclineReason::TooOld => "pullback went on too long",
            };
            w.f(format_args!(
                "{{\"t\":{},\"reason\":\"{why}\",\"features\":",
                secs(rel(d.ts))
            ));
            write_features(&mut w, &d.features);
            w.raw(",\"conditions\":");
            write_evaluations(&mut w, &d.evaluations);
            w.raw("}");
        }
        w.raw("]}");
    }
    w.raw("],");

    // Orders: each intent with the gateway's answer and its fills.
    w.raw("\"orders\":[");
    for (k, (intent, decision)) in result.decisions.iter().enumerate() {
        if k > 0 {
            w.raw(",");
        }
        write_order(&mut w, intent, *decision, result, t0);
    }
    w.raw("],");

    // Round trips: an entry order and, if any, its exit, with the stop's path.
    w.raw("\"trades\":[");
    let mut first = true;
    for (k, (intent, decision)) in result.decisions.iter().enumerate() {
        let entry_fills: Vec<_> = result
            .fill_log
            .iter()
            .filter(|f| f.intent == intent.id)
            .collect();
        if intent.purpose != Purpose::Open
            || !matches!(decision, Decision::Accepted(_))
            || entry_fills.is_empty()
        {
            continue;
        }
        let inst = intent.instrument;
        let exit = result
            .decisions
            .iter()
            .enumerate()
            .skip(k + 1)
            .find(|(_, (i, d))| {
                i.instrument == inst
                    && i.purpose == Purpose::Close
                    && matches!(d, Decision::Accepted(_))
            });
        let exit_fills: Vec<_> = exit
            .map(|(_, (i, _))| {
                result
                    .fill_log
                    .iter()
                    .filter(|f| f.intent == i.id)
                    .collect()
            })
            .unwrap_or_default();
        let qty: u32 = entry_fills.iter().map(|f| f.qty).sum();
        let avg = |fs: &[&tf_strategy::Fill]| -> Option<i64> {
            let q: i128 = fs.iter().map(|f| i128::from(f.qty)).sum();
            (q > 0).then(|| {
                (fs.iter()
                    .map(|f| i128::from(f.px.raw()) * i128::from(f.qty))
                    .sum::<i128>()
                    / q) as i64
            })
        };
        let (entry_px, exit_px) = (avg(&entry_fills).unwrap_or(0), avg(&exit_fills));
        let pnl = exit_px.map(|e| i128::from(e - entry_px) * i128::from(qty));
        let t_in = entry_fills[0].ts;
        let t_out = exit_fills.last().map(|f| f.ts);
        if !first {
            w.raw(",");
        }
        first = false;
        w.f(format_args!(
            "{{\"instrument\":{inst},\"label\":\"{}\",\"order\":{k},\"exit_order\":{},\"qty\":{qty},\"entry_px\":{},\"exit_px\":{},\"t_in\":{},\"t_out\":{},\"pnl\":{},",
            esc(&labels[inst as usize]),
            opt(exit.map(|(j, _)| j)),
            px(entry_px),
            exit_px.map_or("null".to_owned(), px),
            secs(rel(t_in)),
            t_out.map_or("null".to_owned(), |t| secs(rel(t))),
            pnl.map_or("null".to_owned(), |v| px((v.clamp(i128::from(i64::MIN), i128::from(i64::MAX))) as i64)),
        ));
        // The stop: the broker-side stop at entry, then the trailing stop from the high since entry.
        let trail = entries
            .iter()
            .find(|e| e.intent == intent.id)
            .map_or(cfg.params.trail_permille, |e| e.params.trail_permille);
        w.f(format_args!(
            "\"initial_stop\":{},\"stop_path\":[",
            intent
                .protect
                .map_or("null".to_owned(), |p| px(p.stop_trigger.raw()))
        ));
        let end = t_out.unwrap_or(events.last().map_or(t_in, |e| e.ts_recv()));
        let mut high = entry_px;
        let mut last_sec = None;
        let mut firstp = true;
        for (ts, p, _) in ticks[inst as usize]
            .iter()
            .filter(|(ts, _, _)| *ts >= t_in && *ts <= end)
        {
            high = high.max(*p);
            let s = (*ts - t0) / NANOS_PER_SEC;
            if last_sec != Some(s) {
                last_sec = Some(s);
                if !firstp {
                    w.raw(",");
                }
                firstp = false;
                let level = (i128::from(high) * i128::from(1000 - trail) / 1000) as i64;
                w.f(format_args!("[{},{}]", s, px(level)));
            }
        }
        w.f(format_args!("],\"trail_permille\":{trail},\"trace\":"));
        match entries.iter().find(|e| e.intent == intent.id) {
            Some(e) => {
                w.f(format_args!(
                    "{{\"t\":{},\"impulse\":{},\"features\":",
                    secs(rel(e.ts)),
                    e.impulse_permille
                ));
                write_features(&mut w, &e.features);
                w.raw(",\"conditions\":");
                write_evaluations(&mut w, &e.evaluations);
                w.raw("}");
            }
            None => w.raw("null"),
        }
        w.raw("}");
    }
    w.raw("]}");
    w.0
}

fn write_order(w: &mut W, intent: &Intent, decision: Decision, result: &BacktestResult, t0: Nanos) {
    let side = match intent.side {
        Side::Buy => "buy",
        Side::Sell => "sell",
        Side::SellShort => "sell short",
    };
    let purpose = if intent.purpose == Purpose::Open {
        "open"
    } else {
        "close"
    };
    let (accepted, why) = match decision {
        Decision::Accepted(_) => (true, String::new()),
        Decision::Rejected(r) => (false, tf_risk::reason_name(&r).to_owned()),
    };
    let reference = intent.pricing.reference_price().raw();
    w.f(format_args!(
        "{{\"seq\":{},\"t\":{},\"instrument\":{},\"side\":\"{side}\",\"purpose\":\"{purpose}\",\"qty\":{},\"reference\":{},\"worst\":{},\"stop\":{},\"reason\":{},\"accepted\":{accepted},\"rejected\":\"{}\",\"fills\":[",
        intent.id.seq,
        secs(i128::from(intent.ts) - i128::from(t0)),
        intent.instrument,
        intent.qty,
        px(reference),
        px(intent.limit_price().raw()),
        intent.protect.map_or("null".to_owned(), |p| px(p.stop_trigger.raw())),
        intent.reason,
        esc(&why),
    ));
    let mut first = true;
    for f in result.fill_log.iter().filter(|f| f.intent == intent.id) {
        if !first {
            w.raw(",");
        }
        first = false;
        w.f(format_args!(
            "[{},{},{},{}]",
            secs(i128::from(f.ts) - i128::from(t0)),
            px(f.px.raw()),
            f.qty,
            px(f.slippage)
        ));
    }
    w.raw("]}");
}
