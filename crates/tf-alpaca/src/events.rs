//! What Alpaca says happened, and what it means for the ledger.
//!
//! [`parse_frame`] reads one message of the `trade_updates` stream and [`parse_order`] the order
//! object of a REST response. A [`Tracker`] then turns updates into [`BrokerEvent`]s for orders we
//! placed: acknowledged, filled for this many shares at this price, ended. It does this carefully:
//! a fill delivered twice is applied once, a fill that does not add up to what Alpaca says the order
//! has filled is reported rather than applied, and an order we did not place is reported, never
//! guessed at. Protective legs of a bracket are recognised and reported apart: the ledger has no way
//! yet to record a fill for an order the gateway never saw (E09-S11).

use std::collections::{HashMap, HashSet};

use tf_core::{Nanos, Px};
use tf_strategy::lifecycle::{OrderId, OrderState};

use crate::json::{Json, JsonError};
use crate::wire::parse_px;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParseError {
    Json(JsonError),
    /// A field that must be there is not, or is not what it should be.
    Field(&'static str),
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::Json(e) => e.fmt(f),
            ParseError::Field(n) => write!(f, "missing or unreadable field `{n}`"),
        }
    }
}

impl From<JsonError> for ParseError {
    fn from(e: JsonError) -> ParseError {
        ParseError::Json(e)
    }
}

/// Whole shares from text like `100`, `100.0` or `100.000000000`; a fraction is refused (this system
/// trades whole shares).
pub fn whole_shares(text: &str) -> Option<u32> {
    let (whole, frac) = text.split_once('.').unwrap_or((text, ""));
    if whole.is_empty()
        || !whole.bytes().all(|b| b.is_ascii_digit())
        || !frac.bytes().all(|b| b == b'0')
    {
        return None;
    }
    whole.parse().ok()
}

/// An RFC 3339 time (`2022-04-19T17:45:05.024916716Z`, or with an offset such as `-04:00`) as
/// nanoseconds since the epoch. `None` for anything else, and for times before 1970.
pub fn parse_time(text: &str) -> Option<Nanos> {
    let b = text.as_bytes();
    let num = |from: usize, to: usize| -> Option<i64> {
        let s = text.get(from..to)?;
        s.bytes()
            .all(|c| c.is_ascii_digit())
            .then(|| s.parse().ok())?
    };
    if b.len() < 20
        || b[4] != b'-'
        || b[7] != b'-'
        || !matches!(b[10], b'T' | b't')
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }
    let (y, mo, d, h, mi, s) = (
        num(0, 4)?,
        num(5, 7)?,
        num(8, 10)?,
        num(11, 13)?,
        num(14, 16)?,
        num(17, 19)?,
    );
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || s > 60 {
        return None;
    }
    let mut i = 19;
    let mut nanos = 0i64;
    if b.get(i) == Some(&b'.') {
        let from = i + 1;
        i = from;
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        if i == from || i - from > 9 {
            return None;
        }
        nanos = text[from..i].parse::<i64>().ok()? * 10i64.pow((9 - (i - from)) as u32);
    }
    let offset = match b.get(i)? {
        b'Z' | b'z' if i + 1 == b.len() => 0,
        sign @ (b'+' | b'-') if i + 6 == b.len() && b[i + 3] == b':' => {
            let (oh, om) = (num(i + 1, i + 3)?, num(i + 4, i + 6)?);
            if oh > 23 || om > 59 {
                return None;
            }
            (oh * 3_600 + om * 60) * if *sign == b'+' { 1 } else { -1 }
        }
        _ => return None,
    };
    // Days from 1970-01-01 (Howard Hinnant's days_from_civil).
    let (y2, m2) = if mo <= 2 {
        (y - 1, mo + 9)
    } else {
        (y, mo - 3)
    };
    let era = y2.div_euclid(400);
    let yoe = y2.rem_euclid(400);
    let doy = (153 * m2 + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + h * 3_600 + mi * 60 + s - offset;
    u64::try_from(secs)
        .ok()?
        .checked_mul(1_000_000_000)?
        .checked_add(nanos as u64)
}

/// An order as Alpaca reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BrokerOrder {
    pub id: String,
    pub client_order_id: String,
    pub symbol: String,
    pub status: String,
    /// `limit`, `stop`, `stop_limit` ... (`order_type` in the stream, `type` in REST).
    pub kind: String,
    pub qty: Option<u32>,
    pub filled_qty: u32,
    pub filled_avg_price: Option<Px>,
    pub parent: Option<String>,
    pub legs: Vec<BrokerOrder>,
}

pub fn parse_order(v: &Json) -> Result<BrokerOrder, ParseError> {
    let need = |k: &'static str| v.str_at(k).map(str::to_owned).ok_or(ParseError::Field(k));
    let filled_qty = match v.str_at("filled_qty") {
        Some(t) => whole_shares(t).ok_or(ParseError::Field("filled_qty"))?,
        None => 0,
    };
    let filled_avg_price = match v.get("filled_avg_price") {
        None | Some(Json::Null) => None,
        Some(p) => Some(
            p.text()
                .and_then(parse_px)
                .ok_or(ParseError::Field("filled_avg_price"))?,
        ),
    };
    let qty = match v.get("qty") {
        None | Some(Json::Null) => None,
        Some(q) => Some(
            q.text()
                .and_then(whole_shares)
                .ok_or(ParseError::Field("qty"))?,
        ),
    };
    let legs = match v.get("legs") {
        Some(Json::Arr(items)) => items
            .iter()
            .map(parse_order)
            .collect::<Result<Vec<_>, _>>()?,
        _ => Vec::new(),
    };
    Ok(BrokerOrder {
        id: need("id")?,
        client_order_id: v.str_at("client_order_id").unwrap_or_default().to_owned(),
        symbol: need("symbol")?,
        status: need("status")?,
        kind: v
            .str_at("order_type")
            .or_else(|| v.str_at("type"))
            .unwrap_or_default()
            .to_owned(),
        qty,
        filled_qty,
        filled_avg_price,
        parent: v.str_at("parent_order_id").map(str::to_owned),
        legs,
    })
}

/// One `trade_updates` event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Update {
    pub event: String,
    pub order: BrokerOrder,
    /// The price and size of this execution, for fills.
    pub price: Option<Px>,
    pub qty: Option<u32>,
    pub at: Option<Nanos>,
    pub execution_id: Option<String>,
}

/// One message of the stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Frame {
    Authorized,
    Unauthorized,
    Listening(Vec<String>),
    Update(Box<Update>),
    /// A stream this adapter does not use.
    Other(String),
}

pub fn parse_frame(text: &str) -> Result<Frame, ParseError> {
    let v = Json::parse(text)?;
    let stream = v.str_at("stream").ok_or(ParseError::Field("stream"))?;
    let data = v.obj_at("data").ok_or(ParseError::Field("data"))?;
    match stream {
        "authorization" => match data.str_at("status") {
            Some("authorized") => Ok(Frame::Authorized),
            _ => Ok(Frame::Unauthorized),
        },
        "listening" => {
            let names = match data.get("streams") {
                Some(Json::Arr(a)) => a
                    .iter()
                    .filter_map(|s| s.text().map(str::to_owned))
                    .collect(),
                _ => Vec::new(),
            };
            Ok(Frame::Listening(names))
        }
        "trade_updates" => {
            let event = data
                .str_at("event")
                .ok_or(ParseError::Field("event"))?
                .to_owned();
            let order = parse_order(data.obj_at("order").ok_or(ParseError::Field("order"))?)?;
            let price = match data.get("price") {
                None | Some(Json::Null) => None,
                Some(p) => Some(
                    p.text()
                        .and_then(parse_px)
                        .ok_or(ParseError::Field("price"))?,
                ),
            };
            let qty = match data.get("qty") {
                None | Some(Json::Null) => None,
                Some(q) => Some(
                    q.text()
                        .and_then(whole_shares)
                        .ok_or(ParseError::Field("qty"))?,
                ),
            };
            let at = match data.str_at("timestamp") {
                Some(t) => Some(parse_time(t).ok_or(ParseError::Field("timestamp"))?),
                None => None,
            };
            Ok(Frame::Update(Box::new(Update {
                event,
                order,
                price,
                qty,
                at,
                execution_id: data.str_at("execution_id").map(str::to_owned),
            })))
        }
        other => Ok(Frame::Other(other.to_owned())),
    }
}

pub use tf_strategy::broker::{BrokerEvent, Kind, Leg};

/// What reading an update gave.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Event(BrokerEvent),
    /// Nothing to record, said for the audit trail.
    Note(String),
    /// Something that should not happen, and was not acted on: for a person or reconciliation.
    Anomaly(String),
}

#[derive(Debug)]
pub struct Tracker {
    prefix: String,
    acked: HashSet<OrderId>,
    applied: HashMap<OrderId, u32>,
    seen: HashSet<String>,
    legs: HashMap<String, (OrderId, Leg)>,
    /// Broker id of each order we submitted, to recognise a leg that names its parent.
    parents: HashMap<String, OrderId>,
}

fn leg_kind(o: &BrokerOrder) -> Option<Leg> {
    match o.kind.as_str() {
        "limit" => Some(Leg::Target),
        "stop" | "stop_limit" => Some(Leg::Stop),
        _ => None,
    }
}

impl Tracker {
    pub fn new(id_prefix: &str) -> Tracker {
        Tracker {
            prefix: id_prefix.to_owned(),
            acked: HashSet::new(),
            applied: HashMap::new(),
            seen: HashSet::new(),
            legs: HashMap::new(),
            parents: HashMap::new(),
        }
    }

    /// The order a client order id names, if it is one of ours.
    pub fn ours(&self, client_order_id: &str) -> Option<OrderId> {
        client_order_id
            .strip_prefix(&self.prefix)?
            .parse()
            .ok()
            .map(OrderId)
    }

    /// Learn the legs of a bracket from the response to its submission.
    pub fn register(&mut self, parent: OrderId, submitted: &BrokerOrder) {
        self.parents.insert(submitted.id.clone(), parent);
        for l in &submitted.legs {
            if let Some(k) = leg_kind(l) {
                self.legs.insert(l.id.clone(), (parent, k));
            }
        }
    }

    /// Start from what is already known after a restart: the shares of each order already recorded
    /// as filled, and which are acknowledged. Executions seen before are not known; a re-delivered one
    /// is caught by the totals instead.
    pub fn resume(&mut self, order: OrderId, acked: bool, filled: u32) {
        if acked {
            self.acked.insert(order);
        }
        self.applied.insert(order, filled);
    }

    pub fn translate(&mut self, u: &Update) -> Vec<Outcome> {
        let ts = u.at.unwrap_or(0);
        // A protective leg we know, or one that says whose it is.
        let leg = self.legs.get(&u.order.id).copied().or_else(|| {
            let parent = self.parents.get(u.order.parent.as_ref()?)?;
            Some((*parent, leg_kind(&u.order)?))
        });
        if let Some((parent, which)) = leg {
            return self.leg_update(parent, which, u, ts);
        }
        let Some(order) = self.ours(&u.order.client_order_id) else {
            return vec![Outcome::Anomaly(format!(
                "`{}` for an order we did not place (client order id `{}`, broker id {})",
                u.event, u.order.client_order_id, u.order.id
            ))];
        };
        let mut out = Vec::new();
        let ack = |t: &mut Tracker, out: &mut Vec<Outcome>| {
            if t.acked.insert(order) {
                out.push(Outcome::Event(BrokerEvent {
                    order,
                    ts,
                    kind: Kind::Ack,
                }));
            }
        };
        match u.event.as_str() {
            "new" | "accepted" | "pending_new" | "accepted_for_bidding" => {
                if self.acked.contains(&order) {
                    out.push(Outcome::Note(format!(
                        "`{}` for an order already acknowledged",
                        u.event
                    )));
                } else {
                    ack(self, &mut out);
                }
            }
            "fill" | "partial_fill" => self.fill(order, u, ts, &mut out, ack),
            "canceled" => self.end(order, OrderState::Cancelled, ts, &mut out, ack),
            "expired" => self.end(order, OrderState::Expired, ts, &mut out, ack),
            // A rejection comes before any acknowledgement and the order was never accepted, so there
            // is nothing to acknowledge first (an acknowledged order cannot become rejected).
            "rejected" => self.end(order, OrderState::Rejected, ts, &mut out, |_, _| {}),
            "done_for_day"
            | "replaced"
            | "stopped"
            | "suspended"
            | "calculated"
            | "pending_cancel"
            | "pending_replace"
            | "order_cancel_rejected"
            | "order_replace_rejected" => {
                out.push(Outcome::Note(format!("`{}` (not recorded)", u.event)));
            }
            other => out.push(Outcome::Anomaly(format!(
                "an event this adapter does not know: `{other}`"
            ))),
        }
        out
    }

    fn fill(
        &mut self,
        order: OrderId,
        u: &Update,
        ts: Nanos,
        out: &mut Vec<Outcome>,
        ack: impl Fn(&mut Tracker, &mut Vec<Outcome>),
    ) {
        let (Some(qty), Some(px)) = (u.qty, u.price) else {
            out.push(Outcome::Anomaly(format!(
                "a `{}` with no size or price",
                u.event
            )));
            return;
        };
        if qty == 0 {
            out.push(Outcome::Anomaly("a fill of no shares".to_owned()));
            return;
        }
        if let Some(id) = &u.execution_id {
            if !self.seen.insert(id.clone()) {
                out.push(Outcome::Note(format!("execution {id} was already applied")));
                return;
            }
        }
        let before = self.applied.get(&order).copied().unwrap_or(0);
        let after = before + qty;
        if u.order.filled_qty != after {
            if let Some(id) = &u.execution_id {
                self.seen.remove(id);
            }
            out.push(Outcome::Anomaly(format!(
                "order {} filled {} shares by this update's own account but {} by ours (we had {before}, this fill is {qty}): not applied",
                order.0, u.order.filled_qty, after
            )));
            return;
        }
        if u.order.qty.is_some_and(|q| after > q) {
            out.push(Outcome::Anomaly(format!(
                "order {} would be filled for more than it asked",
                order.0
            )));
            return;
        }
        ack(self, out);
        self.applied.insert(order, after);
        out.push(Outcome::Event(BrokerEvent {
            order,
            ts,
            kind: Kind::Fill { qty, px },
        }));
    }

    fn end(
        &mut self,
        order: OrderId,
        state: OrderState,
        ts: Nanos,
        out: &mut Vec<Outcome>,
        ack: impl Fn(&mut Tracker, &mut Vec<Outcome>),
    ) {
        ack(self, out);
        out.push(Outcome::Event(BrokerEvent {
            order,
            ts,
            kind: Kind::Close(state),
        }));
    }

    fn leg_update(&mut self, parent: OrderId, leg: Leg, u: &Update, ts: Nanos) -> Vec<Outcome> {
        match u.event.as_str() {
            "fill" | "partial_fill" => {
                let (Some(qty), Some(px)) = (u.qty, u.price) else {
                    return vec![Outcome::Anomaly(format!(
                        "a protective `{}` with no size or price",
                        u.event
                    ))];
                };
                if let Some(id) = &u.execution_id {
                    if !self.seen.insert(id.clone()) {
                        return vec![Outcome::Note(format!("execution {id} was already applied"))];
                    }
                }
                vec![Outcome::Event(BrokerEvent {
                    order: parent,
                    ts,
                    kind: Kind::LegFill { leg, qty, px },
                })]
            }
            other => vec![Outcome::Note(format!(
                "`{other}` for the {leg:?} leg of order {} (not recorded)",
                parent.0
            ))],
        }
    }
}
