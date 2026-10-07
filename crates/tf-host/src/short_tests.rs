//! Short sales through the multi-strategy host (E19-S06): the broker's borrow rules from the snapshot, the short-sale
//! restriction from the feed, and the daily report.

use tf_core::{Event, Header, Nanos, ProviderId, Px, Status, StatusKind};
use tf_ledger::MemStore;
use tf_risk::{GapRule, Limits};
use tf_strategy::intent::{Pricing, Protective, Purpose, Side, StrategyId, Tif};
use tf_strategy::{CrossStrategy, Ctx, MemberView, Request};
use tf_universe::{LiveFeature, Snapshot};

use crate::tests::*;
use crate::{
    DailyReport, Host, HostConfig, Reference, Route, StrategyDef, SystemInputs, Verdict, certify,
    compare, replay_events, runner,
};

/// Sells short 40 of the member with the most trades at its first review.
struct Shorter {
    id: u16,
    done: bool,
}

impl CrossStrategy for Shorter {
    fn id(&self) -> StrategyId {
        StrategyId(self.id)
    }
    fn period(&self) -> Nanos {
        SEC
    }
    fn on_review(&mut self, ctx: &mut Ctx<'_>, view: &MemberView<'_>) {
        if self.done {
            return;
        }
        let Some((_, id)) = view.top_by(LiveFeature::Trades, 1, true).first().copied() else {
            return;
        };
        let Some(last) = view.state(id).and_then(|s| s.last_px) else {
            return;
        };
        let req = Request {
            side: Side::SellShort,
            qty: 40,
            purpose: Purpose::Open,
            pricing: Pricing::Limit(Px::from_raw(last.raw() - 50_000_000)),
            protect: Some(Protective {
                stop_trigger: Px::from_raw(last.raw() * 2),
                stop_limit: None,
                take_profit: None,
            }),
            tif: Tif::Day,
            reason: 3,
        };
        self.done = ctx.submit(id, req).is_ok();
    }
}

fn def(id: u16, universe: &str) -> StrategyDef {
    StrategyDef {
        id,
        name: format!("shorter{id}"),
        params: String::new(),
        universe: tf_universe::Spec::parse(universe).unwrap(),
        priority: 1,
        route: Route::Sim,
        build: Box::new(move || runner(Shorter { id, done: false })),
    }
}

/// The ordinary snapshot with the broker's flags: every name easy to borrow but S02, which is hard to borrow, and
/// S05, which cannot be sold short.
fn flagged() -> Reference {
    let mut text =
        String::from("# as_of 2026-10-02\nsymbol,price,adv_shares,shortable,easy_to_borrow\n");
    for i in 0..SYMBOLS {
        let (short, easy) = match i {
            2 => ("yes", "no"),
            5 => ("no", "no"),
            _ => ("yes", "yes"),
        };
        text.push_str(&format!("S{i:02},20.00,{},{short},{easy}\n", (i + 1) * 100));
    }
    Reference {
        symbols: names(),
        snapshot: Snapshot::parse(&text).unwrap(),
    }
}

fn cfg() -> HostConfig {
    let mut c = config(2);
    c.limits = Limits::new(50_000 * D, 100_000, 5_000_000 * D, 90_000 * D, 10_000, SEC)
        .unwrap()
        .with_gap_rule(GapRule::new(100_000 * D, 10_000, 1000).unwrap());
    c
}

fn status(sec_x10: u64, instrument: u32, kind: StatusKind) -> Event {
    let ts = T0 + sec_x10 * SEC / 10;
    Event::Status(Status {
        hdr: Header {
            ts_event: ts,
            ts_recv: ts,
            seq: ts,
            instrument,
            provider: ProviderId::Synthetic,
        },
        kind,
        lo: Px::ZERO,
        hi: Px::ZERO,
    })
}

fn day(
    cfg: &HostConfig,
    reference: &Reference,
    defs: &[StrategyDef],
    tape: &[Event],
) -> Host<MemStore> {
    let mut h = Host::new(
        cfg.clone(),
        reference.clone(),
        MemStore::from_records(vec![]),
    )
    .unwrap()
    .record();
    for d in defs {
        let cert = certify(d, cfg, reference, tape, 7).unwrap();
        h.add_strategy(d, &cert).unwrap();
    }
    for e in tape {
        h.on_event(e).unwrap();
    }
    h.end_of_day(tape.last().unwrap().ts_recv()).unwrap();
    h
}

#[test]
fn a_short_sale_of_a_name_that_is_not_easy_to_borrow_is_refused_and_the_report_says_why() {
    let tape = market(12, flat);
    // Strategy 1 shorts S02 (hard to borrow); strategy 2 shorts S08 (easy to borrow).
    let defs = [def(1, LOW), def(2, HIGH)];
    let h = day(&cfg(), &flagged(), &defs, &tape);
    let g = h.journal().gateway();
    assert_eq!(g.strategy_position(1, 2), 0, "the broker would not take it");
    assert_eq!(g.strategy_position(2, 8), -40);
    assert_eq!(h.stats_of(1).unwrap().refused_by_broker, 1);
    assert_eq!(h.stats_of(2).unwrap().refused_by_broker, 0);
    let refusals = h.broker_refusals_of(1);
    assert_eq!(refusals.len(), 1);
    assert!(refusals[0].0, "it was a short sale");
    assert!(refusals[0].1.contains("hard to borrow"), "{refusals:?}");
    assert!(h.broker_refusals_of(2).is_empty());
    let text = DailyReport::build(&h, "x", SystemInputs::default(), None).render();
    assert!(
        text.contains("short sale refused 1x: the asset is hard to borrow"),
        "{text}"
    );
    assert!(!text.contains("order refused"), "{text}");
    // The log of the day replays equal: the broker's rules are part of the run.
    let log = h.log().unwrap();
    let again = replay_events(log, &cfg(), &flagged(), &defs, &tape).unwrap();
    assert_eq!(
        compare(log, &again.log, &flagged().symbols),
        Verdict::Equal {
            records: log.recs.len()
        }
    );
}

#[test]
fn a_name_that_cannot_be_shorted_is_refused_and_a_snapshot_without_the_flags_asks_nothing() {
    // S05 cannot be sold short: the member of LOW with the most trades is S02, so ask for the others.
    let only_s05 = "universe v1\nstatic adv_shares >= 600; adv_shares <= 600\n";
    let defs = [def(1, only_s05)];
    let tape = market(8, flat);
    let h = day(&cfg(), &flagged(), &defs, &tape);
    assert_eq!(h.stats_of(1).unwrap().refused_by_broker, 1);
    assert!(
        h.broker_refusals_of(1)[0]
            .1
            .contains("cannot be sold short")
    );
    // The ordinary snapshot has no borrow flags: the same short is taken, as it always was.
    let h = day(&cfg(), &reference(), &defs, &tape);
    assert_eq!(h.stats_of(1).unwrap().refused_by_broker, 0);
    assert_eq!(h.journal().gateway().strategy_position(1, 5), -40);
}

#[test]
fn a_restriction_from_the_feed_holds_a_short_sale_until_it_is_lifted() {
    let mut tape = market(20, flat);
    // The restriction on S08 is in force from the start (as a carry-over from the day before would say) and ends at
    // second 10.
    tape.push(status(1, 8, StatusKind::ShortSaleRestriction));
    tape.push(status(100, 8, StatusKind::ShortSaleRestrictionLifted));
    tape.sort_by_key(Event::ts_recv);
    let defs = [def(2, HIGH)];
    let cfg = {
        let mut c = cfg();
        c.budgets = None;
        c
    };
    let mut h = Host::new(cfg.clone(), flagged(), MemStore::from_records(vec![]))
        .unwrap()
        .record();
    let cert = certify(&defs[0], &cfg, &flagged(), &tape, 7).unwrap();
    h.add_strategy(&defs[0], &cert).unwrap();
    let mut first_short = None;
    for e in &tape {
        h.on_event(e).unwrap();
        if first_short.is_none() && h.journal().gateway().strategy_position(2, 8) != 0 {
            first_short = Some(e.ts_recv());
        }
    }
    // Held through the restriction, and made as soon as it ended (the first quote after the lift).
    let at = first_short.expect("the short was made after the restriction ended");
    assert!(
        at >= T0 + 10 * SEC,
        "not before the lift at second 10: {at}"
    );
    assert!(at < T0 + 11 * SEC, "and soon after it: {at}");
    assert_eq!(h.journal().gateway().strategy_position(2, 8), -40);
    // Without the restriction events it is made at once.
    let calm = market(20, flat);
    let h2 = day(&cfg, &flagged(), &defs, &calm);
    assert_eq!(h2.journal().gateway().strategy_position(2, 8), -40);
    let _ = h2;
}
