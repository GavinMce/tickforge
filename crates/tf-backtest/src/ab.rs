//! A/B runs: a tuned strategy against a fixed-parameter shadow on the same feed.
//!
//! When an agent tunes parameters live, the question is whether the tuning helped.
//! Judging it needs a control that saw exactly the same market: a **shadow** copy of
//! the strategy with the baseline parameters, which never changes, fed the same
//! events, trading against its own simulated broker with virtual fills and its own
//! gateway with the same limits. The tuned side carries a [`ParamStore`]; the shadow
//! has none, and parameter-change events are not shown to it.
//!
//! [`run_ab`] steps both sides event by event, so a caller can watch the two equity
//! curves as they diverge (the auto-revert policy, E12-S03, is built on that). The
//! proposals are scripted here (a time and a [`Proposal`]); each is checked against
//! the tuned side's store when its time comes, and an accepted one becomes a
//! `ParamChange` event, recorded in [`AbResult::tape`] so the tuned side can be
//! replayed exactly.
//!
//! An optional [`RevertPolicy`] watches the two equities. When the tuned side has fallen
//! the configured amount behind its own best showing against the shadow, the loop asks
//! the store for the events that return every changed parameter to baseline
//! ([`ParamStore::revert_events`]), applies and records them like any other change, and
//! tuning is locked out for the store's lockout. Positions already open keep the
//! parameters they were entered with (ADR 0020); the revert governs new entries.
//!
//! Comparison figures are tuned minus shadow, in the same units as the reports.

use std::collections::BTreeMap;

use tf_core::{Event, Nanos};
use tf_params::{Applied, ParamStore, Proposal, Reject, RevertPolicy, Trip};
use tf_risk::Gateway;
use tf_strategy::report::ReportError;
use tf_strategy::{Host, MomentumLong, SimBroker, Strategy, StrategyId, tunable_specs};

use crate::{BacktestConfig, BacktestResult, Loop};

/// A proposal and when it is made, as event time (nanoseconds since the epoch). It is
/// made at the first event at or after that time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Scheduled {
    pub at: Nanos,
    pub proposal: Proposal,
}

/// A proposal that was refused, with when and why.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Refused {
    pub at: Nanos,
    pub proposal: Proposal,
    pub why: Reject,
}

/// The reason code on revert events written by the auto-revert policy.
pub const REASON_AUTO_REVERT: u16 = 9001;

/// An auto-revert.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Revert {
    /// The event that tripped it.
    pub at: Nanos,
    pub trip: Trip,
    /// How many parameters (global values and overrides) were returned to baseline.
    /// Always at least one: a trip with nothing tuned is not recorded.
    pub parameters: usize,
}

/// Both sides' equity (realised plus marked profit and loss) after an event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub ts: Nanos,
    pub tuned: i128,
    pub shadow: i128,
}

/// Tuned minus shadow.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Comparison {
    pub pnl_net: i128,
    pub pnl_realized: i128,
    pub max_drawdown: i128,
    pub slippage_cost: i128,
    pub trades: i64,
    pub fills: i64,
    /// Hit rate difference in permille, if both sides made a trade.
    pub hit_rate_permille: Option<i64>,
    /// Net P&L difference per label.
    pub pnl_net_by_label: BTreeMap<String, i128>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AbResult {
    pub tuned: BacktestResult,
    pub shadow: BacktestResult,
    pub comparison: Comparison,
    /// Changes applied to the tuned side, in order.
    pub changes: Vec<Applied>,
    pub refused: Vec<Refused>,
    /// Times the auto-revert policy tripped.
    pub reverts: Vec<Revert>,
    /// The tuned side's input as a tape would hold it: every market event, with the
    /// parameter-change events at the places they took effect. Replaying it on a fresh
    /// tuned side reproduces `tuned` exactly.
    pub tape: Vec<Event>,
    /// Change events the tuned side's store refused when applying them (zero unless the
    /// declarations changed between check and apply).
    pub param_errors: u64,
}

fn compare(t: &BacktestResult, s: &BacktestResult) -> Comparison {
    let (a, b) = (&t.report.total, &s.report.total);
    let mut by_label = BTreeMap::new();
    let labels: std::collections::BTreeSet<&String> = t
        .report
        .by_label
        .keys()
        .chain(s.report.by_label.keys())
        .collect();
    for l in labels {
        let net = |r: &BacktestResult| r.report.by_label.get(l).map_or(0, |x| x.net_pnl());
        by_label.insert(l.clone(), net(t) - net(s));
    }
    Comparison {
        pnl_net: a.net_pnl() - b.net_pnl(),
        pnl_realized: a.realized - b.realized,
        max_drawdown: t.report.max_drawdown as i128 - s.report.max_drawdown as i128,
        slippage_cost: a.slippage_cost - b.slippage_cost,
        trades: a.trades as i64 - b.trades as i64,
        fills: a.shares as i64 - b.shares as i64,
        hit_rate_permille: a
            .hit_rate_permille()
            .zip(b.hit_rate_permille())
            .map(|(x, y)| x as i64 - y as i64),
        pnl_net_by_label: by_label,
    }
}

impl AbResult {
    /// Named integers for storing as run-result metrics: each side's report under
    /// `tuned.` and `shadow.`, the differences under `delta.`, and what happened to the
    /// parameters under `params.`.
    pub fn metrics(&self) -> Vec<(String, i64)> {
        let clamp = |v: i128| v.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64;
        let mut m = Vec::new();
        for (side, r) in [("tuned", &self.tuned), ("shadow", &self.shadow)] {
            m.extend(
                r.report
                    .metrics()
                    .into_iter()
                    .map(|(k, v)| (format!("{side}.{k}"), v)),
            );
            m.push((format!("{side}.intents"), r.intents as i64));
            m.push((format!("{side}.accepted"), r.accepted as i64));
            m.push((format!("{side}.outcome_hash"), r.outcome_hash as i64));
            for (why, n) in &r.rejections {
                m.push((format!("{side}.refused.{why}"), *n as i64));
            }
        }
        let c = &self.comparison;
        m.push(("delta.pnl_net".into(), clamp(c.pnl_net)));
        m.push(("delta.pnl_realized".into(), clamp(c.pnl_realized)));
        m.push(("delta.max_drawdown".into(), clamp(c.max_drawdown)));
        m.push(("delta.slippage_cost".into(), clamp(c.slippage_cost)));
        m.push(("delta.trades".into(), c.trades));
        m.push(("delta.shares".into(), c.fills));
        if let Some(h) = c.hit_rate_permille {
            m.push(("delta.hit_rate_permille".into(), h));
        }
        for (label, d) in &c.pnl_net_by_label {
            m.push((format!("delta.group.{label}.pnl_net"), clamp(*d)));
        }
        m.push(("params.applied".into(), self.changes.len() as i64));
        m.push(("params.refused".into(), self.refused.len() as i64));
        m.push(("params.reverts".into(), self.reverts.len() as i64));
        m.push((
            "params.reverted".into(),
            self.reverts.iter().map(|r| r.parameters).sum::<usize>() as i64,
        ));
        m.push(("params.errors".into(), self.param_errors as i64));
        m
    }
}

/// Everything one side of the run is made of.
pub struct Side<'a, S: Strategy> {
    pub host: &'a mut Host<S>,
    pub broker: &'a mut SimBroker,
    pub gateway: &'a mut Gateway,
}

/// Run `tuned` (whose host must have a [`ParamStore`]) and `shadow` over the same
/// `events`, applying the scheduled `proposals` to the tuned side, and calling
/// `observe` after each market event with both equities.
pub fn run_ab<S: Strategy>(
    tuned: Side<'_, S>,
    shadow: Side<'_, S>,
    labels: Vec<String>,
    events: impl IntoIterator<Item = Event>,
    proposals: &[Scheduled],
    mut policy: Option<RevertPolicy>,
    mut observe: impl FnMut(&Snapshot),
) -> Result<AbResult, ReportError> {
    let mut a = Loop::new(tuned.host, tuned.broker, tuned.gateway, labels.clone())?;
    let mut b = Loop::new(shadow.host, shadow.broker, shadow.gateway, labels)?;
    let mut due: Vec<Scheduled> = proposals.to_vec();
    due.sort_by_key(|p| p.at);
    let mut due = due.into_iter().peekable();
    let (mut tape, mut refused, mut reverts) = (Vec::new(), Vec::new(), Vec::new());
    for ev in events {
        let ts = ev.ts_recv();
        while let Some(p) = due.next_if(|p| p.at <= ts) {
            let checked = a
                .host
                .params()
                .map_or(Err(Reject::UnknownParam(p.proposal.param)), |s| {
                    s.check(&p.proposal, ts)
                });
            match checked {
                Ok(change) => {
                    let e = Event::ParamChange(change);
                    a.step(&e);
                    tape.push(e);
                }
                Err(why) => refused.push(Refused {
                    at: ts,
                    proposal: p.proposal,
                    why,
                }),
            }
        }
        a.step(&ev);
        b.step(&ev);
        tape.push(ev);
        let snap = Snapshot {
            ts,
            tuned: a.equity(),
            shadow: b.equity(),
        };
        observe(&snap);
        if let Some(trip) = policy
            .as_mut()
            .and_then(|p| p.observe(snap.tuned, snap.shadow))
        {
            let evidence = u64::try_from(trip.drawdown).unwrap_or(u64::MAX);
            let events = a.host.params().map_or_else(Vec::new, |s| {
                s.revert_events(ts, REASON_AUTO_REVERT, evidence)
            });
            // A trip with nothing tuned is not a revert: the shadow is simply ahead, and
            // there is nothing to take away. (The policy has re-armed itself either way.)
            if events.is_empty() {
                continue;
            }
            for change in &events {
                let e = Event::ParamChange(*change);
                a.step(&e);
                tape.push(e);
            }
            reverts.push(Revert {
                at: ts,
                trip,
                parameters: events.len(),
            });
        }
    }
    let changes = a
        .host
        .params()
        .map_or_else(Vec::new, |s| s.history().to_vec());
    let param_errors = a.host.param_errors();
    let (tuned, shadow) = (a.finish(), b.finish());
    Ok(AbResult {
        comparison: compare(&tuned, &shadow),
        tuned,
        shadow,
        changes,
        refused,
        reverts,
        tape,
        param_errors,
    })
}

/// What the safety policy does in [`momentum_ab_with`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RevertConfig {
    /// Trip when the tuned side is this far (raw price units) behind its best showing
    /// against the shadow.
    pub max_drawdown: u128,
    /// After a revert, refuse all tuning for this long.
    pub lockout: Nanos,
}

/// [`momentum_ab_with`] without a safety policy.
pub fn momentum_ab(
    events: impl IntoIterator<Item = Event>,
    labels: Vec<String>,
    cfg: &BacktestConfig,
    proposals: &[Scheduled],
) -> Result<AbResult, String> {
    momentum_ab_with(events, labels, cfg, proposals, None)
}

/// Strategy 1 (long side) tuned through a store of its [`tunable_specs`] against the
/// same strategy with `cfg.params` fixed, each with its own simulator and gateway built
/// from `cfg`, over `events`, with an optional auto-revert policy on the tuned side.
pub fn momentum_ab_with(
    events: impl IntoIterator<Item = Event>,
    labels: Vec<String>,
    cfg: &BacktestConfig,
    proposals: &[Scheduled],
    revert: Option<RevertConfig>,
) -> Result<AbResult, String> {
    let n = labels.len();
    let mut store = ParamStore::new(tunable_specs(&cfg.params)).map_err(|e| format!("{e:?}"))?;
    let mut policy = None;
    if let Some(r) = revert {
        store = store.with_lockout(r.lockout);
        policy = Some(RevertPolicy::new(r.max_drawdown).map_err(|e| format!("{e:?}"))?);
    }
    let strategy = || MomentumLong::new(StrategyId(1), cfg.params, n).map_err(|e| e.0.to_owned());
    let mut tuned_host = Host::new(strategy()?, n).with_params(store);
    let mut shadow_host = Host::new(strategy()?, n);
    let (mut tb, mut sb) = (SimBroker::new(cfg.sim, n), SimBroker::new(cfg.sim, n));
    let (mut tg, mut sg) = (Gateway::new(cfg.limits, n), Gateway::new(cfg.limits, n));
    run_ab(
        Side {
            host: &mut tuned_host,
            broker: &mut tb,
            gateway: &mut tg,
        },
        Side {
            host: &mut shadow_host,
            broker: &mut sb,
            gateway: &mut sg,
        },
        labels,
        events,
        proposals,
        policy,
        |_| {},
    )
    .map_err(|e| format!("{e:?}"))
}
