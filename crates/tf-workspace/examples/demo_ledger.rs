//! Writes a small paper-account ledger for trying `tf serve` by hand:
//! `cargo run -p tf-workspace --example demo_ledger -- DIR [stress]`, then
//! `tf serve --token-file TOKEN --ledger DIR --kind paper`.

use tf_budget::{Group, LossLimits, Strategy as S, Tree};
use tf_core::Px;
use tf_ledger::{FileStore, Journal};
use tf_risk::{Budgets, GapRule, Limits};
use tf_strategy::intent::{Intent, IntentId, Pricing, Protective, Purpose, Side, StrategyId, Tif};
use tf_strategy::lifecycle::Decision;

const P: i64 = 1_000_000_000;
const SEC: u64 = 1_000_000_000;

fn intent(strategy: u16, seq: u64, side: Side, purpose: Purpose, qty: u32, px: i64) -> Intent {
    Intent {
        id: IntentId {
            strategy: StrategyId(strategy),
            seq,
        },
        instrument: 0,
        side,
        qty,
        purpose,
        pricing: Pricing::Limit(Px::from_raw(px)),
        protect: (purpose == Purpose::Open).then(|| Protective {
            stop_trigger: Px::from_raw(px * 9 / 10),
            stop_limit: None,
            take_profit: None,
        }),
        tif: Tif::Day,
        ts: 1_791_158_400 * SEC + seq * SEC,
        reason: 1,
    }
}

fn fill(j: &mut Journal<FileStore>, i: &Intent, px: i64) {
    let Decision::Accepted(o) = j.decide(i, i.ts).expect("write") else {
        panic!("refused: {i:?}")
    };
    j.ack(o, i.ts).unwrap();
    j.fill(o, i.qty, Px::from_raw(px), i.ts).unwrap();
}

fn main() {
    let dir = std::env::args().nth(1).expect("usage: demo_ledger DIR");
    let limits = Limits::new(
        50_000 * 1_000_000_000u128,
        10_000,
        200_000 * 1_000_000_000u128,
        5_000 * 1_000_000_000u128,
        60,
        10 * SEC,
    )
    .unwrap()
    .with_gap_rule(GapRule::new(100_000 * 1_000_000_000u128, 20_000, 1000).unwrap());
    let (mut j, _) = Journal::open(FileStore::open(&dir).unwrap(), limits, 1).unwrap();
    let group = |id: &str, share, members: &[(&str, u32)]| Group {
        id: id.into(),
        share,
        loss: LossLimits::default(),
        strategies: members
            .iter()
            .map(|(n, s)| S {
                id: (*n).into(),
                share: *s,
            })
            .collect(),
    };
    let tree = Tree::new(vec![
        group("day", 2_500, &[("momentum", 4_000), ("sweep", 3_000)]),
        group("swing", 3_500, &[("lows", 5_000), ("news", 5_000)]),
        group("etf", 4_000, &[("rotation", 10_000)]),
    ])
    .unwrap();
    let names = ["momentum", "sweep", "lows", "news", "rotation"];
    let ids = names
        .iter()
        .enumerate()
        .map(|(n, s)| (n as u16 + 1, (*s).to_owned()));
    j.set_budgets(
        Some(Budgets::new(tree, 100_000 * P as u128, ids).unwrap()),
        1_791_158_400 * SEC,
    )
    .unwrap();
    // Two earlier days, then today. momentum wins most days, sweep loses, lows is holding, news is
    // idle, and on one day all of sweep's orders were refused.
    let mut seq = 10;
    fn go(
        j: &mut Journal<FileStore>,
        seq: &mut u64,
        s: u16,
        qty: u32,
        entry: i64,
        exit: Option<i64>,
    ) {
        fill(
            j,
            &intent(s, *seq, Side::Buy, Purpose::Open, qty, entry),
            entry,
        );
        *seq += 20;
        if let Some(x) = exit {
            fill(j, &intent(s, *seq, Side::Sell, Purpose::Close, qty, x), x);
            *seq += 20;
        }
    }
    go(&mut j, &mut seq, 1, 200, 4 * P, Some(4 * P + P / 4));
    let too_big = intent(2, seq, Side::Buy, Purpose::Open, 1_000_000, 6 * P);
    j.decide(&too_big, too_big.ts).unwrap();
    j.new_day(1_791_158_400 * SEC + seq * SEC + 60 * SEC)
        .unwrap();
    seq += 86_400;
    go(&mut j, &mut seq, 1, 250, 4 * P, Some(3 * P + P / 2));
    go(&mut j, &mut seq, 2, 100, 6 * P, Some(6 * P + P / 5));
    j.new_day(1_791_158_400 * SEC + seq * SEC + 60 * SEC)
        .unwrap();
    seq += 86_400;
    go(&mut j, &mut seq, 1, 300, 4 * P, Some(5 * P));
    go(&mut j, &mut seq, 2, 200, 6 * P, Some(5 * P));
    go(&mut j, &mut seq, 3, 400, 25 * P, None);
    go(&mut j, &mut seq, 5, 100, 100 * P, None);
    if std::env::args().nth(2).as_deref() == Some("stress") {
        // sweep loses past its soft limit, and a person has scheduled a change.
        go(&mut j, &mut seq, 2, 100, 6 * P, Some(4 * P));
        j.check_loss_limits(1_791_158_400 * SEC + seq * SEC)
            .unwrap();
        let t = Tree::new(vec![
            group("day", 2_000, &[("momentum", 5_000), ("sweep", 2_000)]),
            group("swing", 4_000, &[("lows", 5_000), ("news", 5_000)]),
            group("etf", 4_000, &[("rotation", 10_000)]),
        ])
        .unwrap();
        j.schedule_budgets(Some(t), 1_791_158_400 * SEC + seq * SEC + 60 * SEC)
            .unwrap();
    }
    drop(j);
}
