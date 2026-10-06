//! The text form of a ledger record.
//!
//! One record is one line of space-separated tokens, integers only (prices are raw units), so a
//! record is exact and readable. The grammar:
//!
//! ```text
//! start <instruments> <name>=<value> ...             first record: what the ledger is for
//! mark <instrument> <px>
//! decide <now> <strategy> <seq> <instrument> <side> <qty> <purpose> <pricing> <protect> <tif> <ts> <reason> => <outcome>
//! ack <order> <ts>
//! fill <order> <qty> <px> <ts>
//! close <order> <cancelled|rejected|expired> <ts>
//! kill <ts>
//! newday <ts>
//! losscheck <ts>
//! schedule <ts> off
//! schedule <ts> <group>:<share>:<soft>:<hard>/<id>:<share>/...;...
//! rebalance <ts> <floor> <ceiling> <balance|->
//! budgets <ts> off
//! budgets <ts> <balance> <number>=<id>,... <group>:<share>:<soft>:<hard>/<id>:<share>/...;...
//! ```
//!
//! with `side` buy|sell|short, `purpose` open|close, `pricing` `limit:<px>` or
//! `collar:<reference>:<permille>`, `protect` `-` or `stop:<trigger>:<limit|->:<target|->`,
//! `tif` day|ioc, and `outcome` `ok:<order>`, `rej:<reason>` or `invalid:<why>`.

use tf_budget::{Bounds, Group, LossLimits, Strategy as BudgetStrategy, Tree};
use tf_core::{InstrumentId, Nanos, Px};
use tf_risk::Budgets;
use tf_strategy::intent::{
    Intent, IntentError, IntentId, Pricing, Protective, Purpose, Side, StrategyId, Tif,
};
use tf_strategy::lifecycle::{Decision, OrderId, OrderState, RejectReason};

/// What happened to the gateway or an order: the inputs a ledger replays.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Input {
    /// The last price of an instrument holding a position (logged when a decision needs it).
    Mark {
        instrument: InstrumentId,
        px: i64,
    },
    /// An intent put to the gateway at event time `now`.
    Decide {
        intent: Intent,
        now: Nanos,
    },
    /// The broker acknowledged an order.
    Ack {
        order: OrderId,
        ts: Nanos,
    },
    Fill {
        order: OrderId,
        qty: u32,
        px: i64,
        ts: Nanos,
    },
    /// An order ended without filling the rest: cancelled, rejected by the broker, or expired.
    Close {
        order: OrderId,
        state: OrderState,
        ts: Nanos,
    },
    Kill {
        ts: Nanos,
    },
    NewDay {
        ts: Nanos,
    },
    /// Every strategy's loss was checked against its limits and at least one crossed (only such
    /// checks are written: a check that crosses nothing changes no state).
    LossCheck {
        ts: Nanos,
    },
    /// A person's pending change to the budget tree: it takes effect at the next rebalance, not before
    /// (`None` withdraws it).
    Schedule {
        tree: Option<Tree>,
        ts: Nanos,
    },
    /// The end-of-session rebalance: applies the scheduled change if there is one, otherwise moves each
    /// strategy's realised profit since the last rebalance into its own budget within `bounds`.
    /// `balance` is the account's real balance if the broker reported one, else it is computed.
    Rebalance {
        ts: Nanos,
        bounds: Bounds,
        balance: Option<u128>,
    },
    /// The budgets the gateway enforces from here on (`None`: none).
    Budgets {
        budgets: Option<Budgets>,
        ts: Nanos,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Record {
    /// First record of a ledger: the universe size and the limits it is for.
    Start {
        instruments: u32,
        limits: Vec<(String, String)>,
    },
    /// An input and, for a decision, the gateway's answer (kept so a replay can be checked).
    Event {
        input: Input,
        outcome: Option<Decision>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodecError(pub String);

impl std::fmt::Display for CodecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CodecError {}

fn err<T>(m: impl Into<String>) -> Result<T, CodecError> {
    Err(CodecError(m.into()))
}

const REASONS: [(&str, RejectReason); 20] = [
    ("kill_switch", RejectReason::KillSwitch),
    ("max_notional", RejectReason::MaxNotional),
    ("max_position", RejectReason::MaxPosition),
    ("daily_loss", RejectReason::DailyLossLimit),
    ("order_rate", RejectReason::OrderRate),
    ("not_shortable", RejectReason::NotShortable),
    ("short_sale_restricted", RejectReason::ShortSaleRestricted),
    ("halted", RejectReason::Halted),
    ("outside_luld", RejectReason::OutsideLuldBand),
    ("spread_too_wide", RejectReason::SpreadTooWide),
    ("run_up", RejectReason::RunUpTooLarge),
    ("gap_risk", RejectReason::GapRisk),
    ("opposing_position", RejectReason::OpposingPosition),
    ("nothing_to_close", RejectReason::NothingToClose),
    ("unknown_instrument", RejectReason::UnknownInstrument),
    ("broker", RejectReason::Broker),
    ("no_budget", RejectReason::NoBudget),
    ("strategy_budget", RejectReason::StrategyBudget),
    ("group_budget", RejectReason::GroupBudget),
    ("strategy_loss_limit", RejectReason::StrategyLossLimit),
];

const INVALID: [(&str, IntentError); 9] = [
    ("zero_qty", IntentError::ZeroQty),
    ("bad_price", IntentError::BadPrice),
    ("bad_collar", IntentError::BadCollar),
    ("side_and_purpose", IntentError::SideAndPurpose),
    ("missing_protection", IntentError::MissingProtection),
    ("unexpected_protection", IntentError::UnexpectedProtection),
    ("stop_on_wrong_side", IntentError::StopOnWrongSide),
    ("target_on_wrong_side", IntentError::TargetOnWrongSide),
    (
        "stop_limit_on_wrong_side",
        IntentError::StopLimitOnWrongSide,
    ),
];

fn outcome_text(d: Decision) -> Result<String, CodecError> {
    Ok(match d {
        Decision::Accepted(o) => format!("ok:{}", o.0),
        Decision::Rejected(RejectReason::Invalid(e)) => {
            let name = INVALID.iter().find(|(_, x)| *x == e).map(|x| x.0);
            format!(
                "invalid:{}",
                name.ok_or_else(|| CodecError(format!("unrecorded intent error {e:?}")))?
            )
        }
        Decision::Rejected(r) => {
            let name = REASONS.iter().find(|(_, x)| *x == r).map(|x| x.0);
            format!(
                "rej:{}",
                name.ok_or_else(|| CodecError(format!("unrecorded reject reason {r:?}")))?
            )
        }
    })
}

fn parse_outcome(t: &str) -> Result<Decision, CodecError> {
    let Some((kind, v)) = t.split_once(':') else {
        return err(format!("outcome `{t}` is not `kind:value`"));
    };
    match kind {
        "ok" => Ok(Decision::Accepted(OrderId(num(v, "order")?))),
        "rej" => REASONS
            .iter()
            .find(|(n, _)| *n == v)
            .map(|(_, r)| Decision::Rejected(*r))
            .ok_or_else(|| CodecError(format!("unknown reject reason `{v}`"))),
        "invalid" => INVALID
            .iter()
            .find(|(n, _)| *n == v)
            .map(|(_, e)| Decision::Rejected(RejectReason::Invalid(*e)))
            .ok_or_else(|| CodecError(format!("unknown intent error `{v}`"))),
        other => err(format!("unknown outcome kind `{other}`")),
    }
}

fn num<T: std::str::FromStr>(t: &str, what: &str) -> Result<T, CodecError> {
    t.parse::<T>()
        .map_err(|_| CodecError(format!("{what} `{t}` is not a number")))
}

fn opt_px(t: &str) -> Result<Option<Px>, CodecError> {
    if t == "-" {
        Ok(None)
    } else {
        Ok(Some(Px::from_raw(num(t, "price")?)))
    }
}

fn px_text(p: Option<Px>) -> String {
    p.map_or("-".to_owned(), |p| p.raw().to_string())
}

fn intent_tokens(i: &Intent) -> String {
    let side = match i.side {
        Side::Buy => "buy",
        Side::Sell => "sell",
        Side::SellShort => "short",
    };
    let purpose = match i.purpose {
        Purpose::Open => "open",
        Purpose::Close => "close",
    };
    let pricing = match i.pricing {
        Pricing::Limit(p) => format!("limit:{}", p.raw()),
        Pricing::Collar {
            reference,
            collar_permille,
        } => format!("collar:{}:{collar_permille}", reference.raw()),
    };
    let protect = i.protect.map_or("-".to_owned(), |p| {
        format!(
            "stop:{}:{}:{}",
            p.stop_trigger.raw(),
            px_text(p.stop_limit),
            px_text(p.take_profit)
        )
    });
    let tif = match i.tif {
        Tif::Day => "day",
        Tif::Ioc => "ioc",
    };
    format!(
        "{} {} {} {side} {} {purpose} {pricing} {protect} {tif} {} {}",
        i.id.strategy.0, i.id.seq, i.instrument, i.qty, i.ts, i.reason
    )
}

fn parse_intent(t: &[&str]) -> Result<Intent, CodecError> {
    let [
        strategy,
        seq,
        instrument,
        side,
        qty,
        purpose,
        pricing,
        protect,
        tif,
        ts,
        reason,
    ] = t
    else {
        return err(format!("an intent has 11 fields, found {}", t.len()));
    };
    let side = match *side {
        "buy" => Side::Buy,
        "sell" => Side::Sell,
        "short" => Side::SellShort,
        o => return err(format!("unknown side `{o}`")),
    };
    let purpose = match *purpose {
        "open" => Purpose::Open,
        "close" => Purpose::Close,
        o => return err(format!("unknown purpose `{o}`")),
    };
    let pricing = {
        let p: Vec<&str> = pricing.split(':').collect();
        match p[..] {
            ["limit", px] => Pricing::Limit(Px::from_raw(num(px, "price")?)),
            ["collar", r, c] => Pricing::Collar {
                reference: Px::from_raw(num(r, "reference")?),
                collar_permille: num(c, "collar")?,
            },
            _ => return err(format!("unknown pricing `{pricing}`")),
        }
    };
    let protect = if *protect == "-" {
        None
    } else {
        let p: Vec<&str> = protect.split(':').collect();
        let ["stop", trig, lim, tp] = p[..] else {
            return err(format!("unknown protection `{protect}`"));
        };
        Some(Protective {
            stop_trigger: Px::from_raw(num(trig, "stop")?),
            stop_limit: opt_px(lim)?,
            take_profit: opt_px(tp)?,
        })
    };
    let tif = match *tif {
        "day" => Tif::Day,
        "ioc" => Tif::Ioc,
        o => return err(format!("unknown time in force `{o}`")),
    };
    Ok(Intent {
        id: IntentId {
            strategy: StrategyId(num(strategy, "strategy")?),
            seq: num(seq, "sequence")?,
        },
        instrument: num(instrument, "instrument")?,
        side,
        qty: num(qty, "quantity")?,
        purpose,
        pricing,
        protect,
        tif,
        ts: num(ts, "time")?,
        reason: num(reason, "reason")?,
    })
}

fn tree_token(tree: &Tree) -> String {
    let groups: Vec<String> = tree
        .groups()
        .iter()
        .map(|g| {
            let mut t = format!("{}:{}:{}:{}", g.id, g.share, g.loss.soft, g.loss.hard);
            for s in &g.strategies {
                t.push_str(&format!("/{}:{}", s.id, s.share));
            }
            t
        })
        .collect();
    if groups.is_empty() {
        "-".to_owned()
    } else {
        groups.join(";")
    }
}

fn budgets_text(b: &Budgets) -> String {
    let ids: Vec<String> = b.ids().iter().map(|(n, id)| format!("{n}={id}")).collect();
    format!(
        "{} {} {}",
        b.balance(),
        if ids.is_empty() {
            "-".to_owned()
        } else {
            ids.join(",")
        },
        tree_token(b.tree())
    )
}

fn parse_tree(tree: &str) -> Result<Tree, CodecError> {
    let mut groups = Vec::new();
    if tree != "-" {
        for g in tree.split(';') {
            let mut parts = g.split('/');
            let head: Vec<&str> = parts.next().unwrap_or("").split(':').collect();
            let [id, share, soft, hard] = head[..] else {
                return err(format!(
                    "a budget group is `id:share:soft:hard`, found `{g}`"
                ));
            };
            let mut strategies = Vec::new();
            for s in parts {
                let Some((sid, sshare)) = s.split_once(':') else {
                    return err(format!("a budget strategy is `id:share`, found `{s}`"));
                };
                strategies.push(BudgetStrategy {
                    id: sid.to_owned(),
                    share: num(sshare, "share")?,
                });
            }
            groups.push(Group {
                id: id.to_owned(),
                share: num(share, "share")?,
                loss: LossLimits {
                    soft: num(soft, "soft limit")?,
                    hard: num(hard, "hard limit")?,
                },
                strategies,
            });
        }
    }
    Tree::new(groups).map_err(|e| CodecError(format!("budgets: {e}")))
}

fn parse_budgets(balance: &str, ids: &str, tree: &str) -> Result<Budgets, CodecError> {
    let tree = parse_tree(tree)?;
    let mut pairs = Vec::new();
    if ids != "-" {
        for kv in ids.split(',') {
            let Some((n, id)) = kv.split_once('=') else {
                return err(format!("a strategy mapping is `number=id`, found `{kv}`"));
            };
            pairs.push((num::<u16>(n, "strategy number")?, id.to_owned()));
        }
    }
    Budgets::new(tree, num(balance, "balance")?, pairs)
        .map_err(|e| CodecError(format!("budgets: {e:?}")))
}

fn state_name(s: OrderState) -> Option<&'static str> {
    match s {
        OrderState::Cancelled => Some("cancelled"),
        OrderState::Rejected => Some("rejected"),
        OrderState::Expired => Some("expired"),
        _ => None,
    }
}

impl Record {
    /// The one-line text of this record.
    pub fn encode(&self) -> Result<String, CodecError> {
        Ok(match self {
            Record::Start {
                instruments,
                limits,
            } => {
                let mut s = format!("start {instruments}");
                for (k, v) in limits {
                    if k.contains(['=', ' ']) || v.contains(' ') || k.is_empty() {
                        return err(format!("limit `{k}={v}` cannot be written on one line"));
                    }
                    s.push_str(&format!(" {k}={v}"));
                }
                s
            }
            Record::Event { input, outcome } => match (input, outcome) {
                (Input::Mark { instrument, px }, None) => format!("mark {instrument} {px}"),
                (Input::Decide { intent, now }, Some(o)) => {
                    format!(
                        "decide {now} {} => {}",
                        intent_tokens(intent),
                        outcome_text(*o)?
                    )
                }
                (Input::Ack { order, ts }, None) => format!("ack {} {ts}", order.0),
                (Input::Fill { order, qty, px, ts }, None) => {
                    format!("fill {} {qty} {px} {ts}", order.0)
                }
                (Input::Close { order, state, ts }, None) => format!(
                    "close {} {} {ts}",
                    order.0,
                    state_name(*state)
                        .ok_or_else(|| CodecError(format!("an order cannot close as {state:?}")))?
                ),
                (Input::Kill { ts }, None) => format!("kill {ts}"),
                (Input::NewDay { ts }, None) => format!("newday {ts}"),
                (Input::LossCheck { ts }, None) => format!("losscheck {ts}"),
                (Input::Schedule { tree: None, ts }, None) => format!("schedule {ts} off"),
                (Input::Schedule { tree: Some(t), ts }, None) => {
                    format!("schedule {ts} {}", tree_token(t))
                }
                (
                    Input::Rebalance {
                        ts,
                        bounds,
                        balance,
                    },
                    None,
                ) => format!(
                    "rebalance {ts} {} {} {}",
                    bounds.floor,
                    bounds.ceiling,
                    balance.map_or("-".to_owned(), |b| b.to_string())
                ),
                (Input::Budgets { budgets: None, ts }, None) => format!("budgets {ts} off"),
                (
                    Input::Budgets {
                        budgets: Some(b),
                        ts,
                    },
                    None,
                ) => {
                    format!("budgets {ts} {}", budgets_text(b))
                }
                (Input::Decide { .. }, None) => return err("a decision record needs its outcome"),
                (_, Some(_)) => return err("only a decision has an outcome"),
            },
        })
    }

    pub fn decode(line: &str) -> Result<Record, CodecError> {
        let t: Vec<&str> = line.split(' ').collect();
        let Some(kind) = t.first() else {
            return err("empty record");
        };
        let ev = |input| {
            Ok(Record::Event {
                input,
                outcome: None,
            })
        };
        match (*kind, &t[1..]) {
            ("start", [n, rest @ ..]) => {
                let mut limits = Vec::new();
                for kv in rest {
                    let Some((k, v)) = kv.split_once('=') else {
                        return err(format!("`{kv}` is not name=value"));
                    };
                    limits.push((k.to_owned(), v.to_owned()));
                }
                Ok(Record::Start {
                    instruments: num(n, "instruments")?,
                    limits,
                })
            }
            ("mark", [i, px]) => ev(Input::Mark {
                instrument: num(i, "instrument")?,
                px: num(px, "price")?,
            }),
            ("decide", [now, rest @ ..]) => {
                let Some(arrow) = rest.iter().position(|x| *x == "=>") else {
                    return err("a decision needs `=> outcome`");
                };
                let (fields, out) = (&rest[..arrow], &rest[arrow + 1..]);
                let [out] = out else {
                    return err("a decision has one outcome");
                };
                Ok(Record::Event {
                    input: Input::Decide {
                        intent: parse_intent(fields)?,
                        now: num(now, "time")?,
                    },
                    outcome: Some(parse_outcome(out)?),
                })
            }
            ("ack", [o, ts]) => ev(Input::Ack {
                order: OrderId(num(o, "order")?),
                ts: num(ts, "time")?,
            }),
            ("fill", [o, q, px, ts]) => ev(Input::Fill {
                order: OrderId(num(o, "order")?),
                qty: num(q, "quantity")?,
                px: num(px, "price")?,
                ts: num(ts, "time")?,
            }),
            ("close", [o, st, ts]) => ev(Input::Close {
                order: OrderId(num(o, "order")?),
                state: match *st {
                    "cancelled" => OrderState::Cancelled,
                    "rejected" => OrderState::Rejected,
                    "expired" => OrderState::Expired,
                    other => return err(format!("an order cannot close as `{other}`")),
                },
                ts: num(ts, "time")?,
            }),
            ("kill", [ts]) => ev(Input::Kill {
                ts: num(ts, "time")?,
            }),
            ("newday", [ts]) => ev(Input::NewDay {
                ts: num(ts, "time")?,
            }),
            ("schedule", [ts, "off"]) => ev(Input::Schedule {
                tree: None,
                ts: num(ts, "time")?,
            }),
            ("schedule", [ts, tree]) => ev(Input::Schedule {
                tree: Some(parse_tree(tree)?),
                ts: num(ts, "time")?,
            }),
            ("rebalance", [ts, floor, ceiling, balance]) => ev(Input::Rebalance {
                ts: num(ts, "time")?,
                bounds: Bounds::new(num(floor, "floor")?, num(ceiling, "ceiling")?)
                    .map_err(|e| CodecError(format!("rebalance: {e}")))?,
                balance: if *balance == "-" {
                    None
                } else {
                    Some(num(balance, "balance")?)
                },
            }),
            ("losscheck", [ts]) => ev(Input::LossCheck {
                ts: num(ts, "time")?,
            }),
            ("budgets", [ts, "off"]) => ev(Input::Budgets {
                budgets: None,
                ts: num(ts, "time")?,
            }),
            ("budgets", [ts, balance, ids, tree]) => ev(Input::Budgets {
                budgets: Some(parse_budgets(balance, ids, tree)?),
                ts: num(ts, "time")?,
            }),
            (k, rest) => err(format!(
                "`{k}` with {} field(s) is not a record",
                rest.len()
            )),
        }
    }
}
