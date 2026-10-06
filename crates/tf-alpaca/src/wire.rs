//! The body of `POST /v2/orders` for an accepted intent, and exact decimal prices.
//!
//! Prices travel as decimal text and are never turned into floats. A price is rounded to Alpaca's
//! tick (two places from one dollar up, four below) in the direction that can only make the order
//! more conservative than the intent: an entry limit never pays more (buy) or accepts less (sell),
//! and a protective stop or target for a long rounds up and for a short rounds down, so a stop never
//! fires later than stated and a target is never demanded at a worse price.

use tf_core::Px;
use tf_strategy::intent::{Intent, Purpose, Side, Tif};
use tf_strategy::lifecycle::OrderId;

use crate::json::quote;

const DOLLAR: i64 = 1_000_000_000;

/// Which way a price is rounded to the tick.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Round {
    Down,
    Up,
}

/// Decimal text to a price, exactly. `None` for anything that is not a plain decimal, or that has
/// digits beyond a billionth of a dollar that are not zero.
pub fn parse_px(text: &str) -> Option<Px> {
    let (neg, rest) = match text.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, text),
    };
    let (whole, frac) = rest.split_once('.').unwrap_or((rest, ""));
    if whole.is_empty()
        || !whole.bytes().all(|b| b.is_ascii_digit())
        || !frac.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    if frac.len() > 9 && frac[9..].bytes().any(|b| b != b'0') {
        return None;
    }
    let frac9: String = frac
        .chars()
        .take(9)
        .chain(std::iter::repeat('0'))
        .take(9)
        .collect();
    let raw = i128::from(whole.parse::<u64>().ok()?)
        .checked_mul(i128::from(DOLLAR))?
        .checked_add(i128::from(frac9.parse::<u64>().ok()?))?;
    let raw = i64::try_from(if neg { -raw } else { raw }).ok()?;
    Some(Px::from_raw(raw))
}

/// A price as Alpaca wants it: rounded to the tick in the given direction. `None` if it is not
/// positive.
pub fn price_text(px: Px, round: Round) -> Option<String> {
    let raw = px.raw();
    if raw <= 0 {
        return None;
    }
    // Four places below a dollar, two from a dollar up: the tick is 1e-4 or 1e-2 dollars.
    let tick = if raw < DOLLAR {
        DOLLAR / 10_000
    } else {
        DOLLAR / 100
    };
    let ticks = match round {
        Round::Down => raw / tick,
        Round::Up => (raw + tick - 1) / tick,
    };
    let rounded = ticks * tick;
    if rounded == 0 {
        return None;
    }
    // Rounding up from just under a dollar lands on a dollar, which takes two places.
    let (places, unit) = if rounded < DOLLAR {
        (4, DOLLAR / 10_000)
    } else {
        (2, DOLLAR / 100)
    };
    let n = rounded / unit;
    let scale = if places == 4 { 10_000 } else { 100 };
    Some(format!(
        "{}.{:0width$}",
        n / scale,
        n % scale,
        width = places
    ))
}

/// Why an intent cannot be sent as it stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestError {
    ZeroQty,
    /// A price that is not positive, or rounds to nothing.
    BadPrice,
    /// Protective orders were asked for something that does not open a position.
    ProtectOnClose,
    /// Alpaca takes bracket and one-triggers-other orders only as `day` or `gtc`.
    ProtectNeedsDay,
}

impl std::fmt::Display for RequestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            RequestError::ZeroQty => "an order for no shares",
            RequestError::BadPrice => "a price that is not positive at Alpaca's tick",
            RequestError::ProtectOnClose => "protective orders on an order that closes a position",
            RequestError::ProtectNeedsDay => {
                "protective orders need a day order; Alpaca refuses them on an IOC"
            }
        })
    }
}

/// One order, ready to send.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OrderRequest {
    /// Alpaca keeps this as the order's own name for us: it is how an order whose submission timed
    /// out is found again, and a second order under the same name is refused.
    pub client_order_id: String,
    /// The JSON body.
    pub body: String,
}

/// The name Alpaca knows an order by: `prefix` (so a fresh ledger does not collide with an old one on
/// the same account) and the order's number.
pub fn client_order_id(prefix: &str, order: OrderId) -> String {
    format!("{prefix}{}", order.0)
}

/// The request for `intent`, already accepted by the gateway as `order`, on `symbol`.
pub fn order_request(
    intent: &Intent,
    order: OrderId,
    symbol: &str,
    prefix: &str,
) -> Result<OrderRequest, RequestError> {
    if intent.qty == 0 {
        return Err(RequestError::ZeroQty);
    }
    let long = intent.side == Side::Buy;
    let entry_round = if intent.side.is_buy() {
        Round::Down
    } else {
        Round::Up
    };
    let limit = price_text(intent.pricing.worst_price(intent.side), entry_round)
        .ok_or(RequestError::BadPrice)?;
    let side = if intent.side.is_buy() { "buy" } else { "sell" };
    let tif = match intent.tif {
        Tif::Day => "day",
        Tif::Ioc => "ioc",
    };
    let id = client_order_id(prefix, order);
    let mut body = format!(
        "{{\"symbol\":{},\"qty\":\"{}\",\"side\":\"{side}\",\"type\":\"limit\",\"time_in_force\":\"{tif}\",\"limit_price\":\"{limit}\",\"client_order_id\":{}",
        quote(symbol),
        intent.qty,
        quote(&id)
    );
    if let Some(p) = &intent.protect {
        if intent.purpose != Purpose::Open {
            return Err(RequestError::ProtectOnClose);
        }
        if intent.tif != Tif::Day {
            return Err(RequestError::ProtectNeedsDay);
        }
        // A long's exits are sells and round up; a short's are buys and round down.
        let exit = if long { Round::Up } else { Round::Down };
        let stop = price_text(p.stop_trigger, exit).ok_or(RequestError::BadPrice)?;
        let mut stop_loss = format!("{{\"stop_price\":\"{stop}\"");
        if let Some(l) = p.stop_limit {
            stop_loss.push_str(&format!(
                ",\"limit_price\":\"{}\"",
                price_text(l, exit).ok_or(RequestError::BadPrice)?
            ));
        }
        stop_loss.push('}');
        match p.take_profit {
            Some(t) => {
                let target = price_text(t, exit).ok_or(RequestError::BadPrice)?;
                body.push_str(&format!(
                    ",\"order_class\":\"bracket\",\"take_profit\":{{\"limit_price\":\"{target}\"}},\"stop_loss\":{stop_loss}"
                ));
            }
            // Only a stop: one order that triggers one other.
            None => body.push_str(&format!(
                ",\"order_class\":\"oto\",\"stop_loss\":{stop_loss}"
            )),
        }
    }
    body.push('}');
    Ok(OrderRequest {
        client_order_id: id,
        body,
    })
}
