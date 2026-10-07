//! A research run over the days of a history store, with a placeholder strategy, to see that the pipeline reads real
//! data and how fast it goes. The strategy is not an idea: at the 600th second of its life it buys 100 shares of the
//! busiest symbol it watches, and sells them at the 1,200th.
//!
//! `cargo run --release -p tf-host --example research_real -- STORE DATASET SCHEMA FROM TO OUT SYM1,SYM2 [--assume-sec-rate]`
//!
//! `--twelve-months` is a speed test, not a result: it runs the one stored day (FROM) as every trading day from
//! 2025-10-01 to 2026-09-30, to see how long a year of days of that size takes and what the results directory holds.
//!
//! `--assume-sec-rate` carries the last Section 31 rate in the cost model on past the date it is known through, which is
//! wrong for a real result and fine for a timing run.

use std::path::PathBuf;
use std::time::Instant;

use tf_budget::{Group, LossLimits, Strategy as BudgetStrategy, Tree};
use tf_core::{Nanos, Px};
use tf_engine::{PromoterConfig, ScannerConfig};
use tf_host::research::{CostModel, DayInput, DaySource, Setup, run};
use tf_host::{HostConfig, Route, StrategyDef, runner};
use tf_risk::{Budgets, Limits};
use tf_strategy::intent::{Pricing, Purpose, Side, StrategyId, Tif};
use tf_strategy::sim::SimConfig;
use tf_strategy::{CrossStrategy, Ctx, MemberView, Request};
use tf_universe::{LiveFeature, Snapshot, Spec};

const D: u128 = 1_000_000_000;
const SEC: Nanos = 1_000_000_000;

struct Placeholder {
    reviews: u32,
    held: Option<u32>,
}

impl CrossStrategy for Placeholder {
    fn id(&self) -> StrategyId {
        StrategyId(1)
    }

    fn period(&self) -> Nanos {
        SEC
    }

    fn on_review(&mut self, ctx: &mut Ctx<'_>, view: &MemberView<'_>) {
        self.reviews += 1;
        // A limit one percent through the last price, so that it crosses the spread.
        let (side, purpose, sign) = match self.reviews {
            600 => (Side::Buy, Purpose::Open, 1),
            1_200 => (Side::Sell, Purpose::Close, -1),
            _ => return,
        };
        let id = match self.held {
            Some(id) => id,
            None => match view.top_by(LiveFeature::Trades, 1, true).first() {
                Some((_, id)) => *id,
                None => return,
            },
        };
        let Some(last) = view.state(id).and_then(|s| s.last_px) else {
            return;
        };
        self.held = Some(id);
        let _ = ctx.submit(
            id,
            Request {
                side,
                qty: 100,
                purpose,
                pricing: Pricing::Limit(Px::from_raw(last.raw() + sign * last.raw() / 100)),
                protect: None,
                tif: Tif::Day,
                reason: 1,
            },
        );
    }
}

struct Store {
    dir: PathBuf,
    days: Vec<tf_history::Day>,
    symbols: Vec<String>,
    /// Run the first stored day as each of these dates.
    pretend: Option<Vec<String>>,
}

impl Store {
    fn day(&self, date: &str) -> Result<&tf_history::Day, String> {
        match &self.pretend {
            Some(_) => self.days.first(),
            None => self.days.iter().find(|d| d.date == date),
        }
        .ok_or_else(|| "not stored".to_owned())
    }
}

impl DaySource for Store {
    fn dates(&self) -> Vec<String> {
        match &self.pretend {
            Some(v) => v.clone(),
            None => self.days.iter().map(|d| d.date.clone()).collect(),
        }
    }

    fn data_id(&self, date: &str) -> Result<String, String> {
        Ok(self.day(date)?.sha256.clone())
    }

    fn load(&mut self, date: &str) -> Result<DayInput, String> {
        let d = self.day(date)?;
        let mut text = format!("# as_of {date}\nsymbol,price,adv_shares\n");
        for s in &self.symbols {
            text.push_str(&format!("{s},100.00,1000000\n"));
        }
        Ok(DayInput {
            files: vec![d.path(&self.dir)],
            snapshot: Snapshot::parse(&text).map_err(|e| e.to_string())?,
        })
    }
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let dir = PathBuf::from(&a[1]);
    let (dataset, schema, from, to) = (&a[2], &a[3], &a[4], &a[5]);
    let out = PathBuf::from(&a[6]);
    let symbols: Vec<String> = a[7].split(',').map(str::to_owned).collect();
    let store = tf_history::Store::read(&dir).expect("the store");
    let days: Vec<tf_history::Day> = store
        .of(dataset, schema)
        .filter(|d| d.date.as_str() >= from.as_str() && d.date.as_str() <= to.as_str())
        .cloned()
        .collect();
    let records: u64 = days.iter().map(|d| d.records).sum();
    let pretend = a.iter().any(|x| x == "--twelve-months").then(|| {
        let cal = tf_calendar::Calendar::us_equities();
        let mut d = tf_calendar::Date::new(2025, 10, 1).unwrap();
        let mut v = Vec::new();
        while d <= tf_calendar::Date::new(2026, 9, 30).unwrap() {
            if cal.is_trading_day(d).unwrap() {
                v.push(d.to_string());
            }
            d = d.next();
        }
        v
    });
    let mut source = Store {
        dir,
        days,
        symbols,
        pretend,
    };
    let tree = Tree::new(vec![Group {
        id: "g".into(),
        share: 10_000,
        loss: LossLimits {
            soft: 300,
            hard: 600,
        },
        strategies: vec![BudgetStrategy {
            id: "s1".into(),
            share: 10_000,
        }],
    }])
    .unwrap();
    let host = HostConfig {
        id_space: 4096,
        limits: Limits::new(50_000 * D, 100_000, 5_000_000 * D, 90_000 * D, 10_000, SEC).unwrap(),
        budgets: Some(Budgets::new(tree, 100_000 * D, [(1u16, "s1".to_owned())]).unwrap()),
        promoter: PromoterConfig::default(),
        scanner: ScannerConfig::default(),
        sim: SimConfig {
            latency_ns: 0,
            borrow_bps_per_year: 0,
        },
        min_certified_events: 1,
        start_ts: 0,
        bars: None,
        day: None,
    };
    let mut cost = CostModel::published();
    if a.iter().any(|x| x == "--assume-sec-rate") {
        cost.sec_through = "2099-12-31".into();
    }
    let defs = vec![StrategyDef {
        id: 1,
        name: "placeholder".into(),
        params: "buy at 600 s, sell at 1200 s".into(),
        universe: Spec::parse("universe v1\nstatic adv_shares >= 1\n").unwrap(),
        priority: 1,
        route: Route::Sim,
        build: Box::new(|| {
            runner(Placeholder {
                reviews: 0,
                held: None,
            })
        }),
    }];
    let t = Instant::now();
    let rep = run(
        &Setup {
            host: &host,
            cost: &cost,
            defs: &defs,
        },
        &mut source,
        &out,
    )
    .expect("the run");
    let secs = t.elapsed().as_secs_f64();
    println!(
        "{} days run, {} skipped; {} events ({} records stored); {} round trips; {:.2} s, {:.0} events/s",
        rep.ran.len(),
        rep.skipped.len(),
        rep.events,
        records,
        rep.trips,
        secs,
        rep.events as f64 / secs.max(1e-9)
    );
}
