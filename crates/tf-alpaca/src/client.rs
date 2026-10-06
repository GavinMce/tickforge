//! The conversation with Alpaca, over a [`Transport`] this crate does not provide.
//!
//! The important rule is about an order whose submission got no answer (a timeout, a dropped
//! connection, a 5xx): it may or may not exist at Alpaca. It is looked up by its client order id,
//! and if it cannot be found it is submitted again under the **same** client order id, which Alpaca
//! refuses if the first one did arrive. It is never submitted under a new name. After two attempts
//! the outcome is reported as unknown and left to reconciliation (E09-S06); the order is not closed
//! in the ledger on a guess.

use tf_strategy::intent::Intent;
use tf_strategy::lifecycle::OrderId;

use crate::events::{BrokerOrder, ParseError, Tracker, parse_order};
use crate::json::Json;
use crate::wire::{RequestError, order_request};

/// The paper trading REST endpoint.
pub const PAPER_URL: &str = "https://paper-api.alpaca.markets";
/// The paper trading stream.
pub const PAPER_STREAM: &str = "wss://paper-api.alpaca.markets/stream";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
    Delete,
}

#[derive(Clone, PartialEq, Eq)]
pub struct HttpRequest {
    pub method: Method,
    /// Path and query, from the base URL.
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
}

impl std::fmt::Debug for HttpRequest {
    /// Header values are not shown: they carry the account's keys.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpRequest")
            .field("method", &self.method)
            .field("path", &self.path)
            .field(
                "headers",
                &self
                    .headers
                    .iter()
                    .map(|(k, _)| k.as_str())
                    .collect::<Vec<_>>(),
            )
            .field("body", &self.body)
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub body: String,
    /// Seconds, from a `Retry-After` header.
    pub retry_after: Option<u64>,
}

/// A request that got no usable answer. For anything that changes something, the result is unknown.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TransportError {
    Timeout,
    Connection(String),
}

/// Sends one HTTP request and returns the response. The adapter does not retry inside it.
pub trait Transport {
    fn send(&mut self, req: &HttpRequest) -> Result<HttpResponse, TransportError>;
}

/// The account's keys and where to reach it. The secret is never shown by `Debug`.
#[derive(Clone)]
pub struct Config {
    key_id: String,
    secret: String,
    /// Put in front of every client order id, so a new ledger on an old account does not collide.
    pub id_prefix: String,
}

impl Config {
    pub fn new(key_id: &str, secret: &str, id_prefix: &str) -> Config {
        Config {
            key_id: key_id.to_owned(),
            secret: secret.to_owned(),
            id_prefix: id_prefix.to_owned(),
        }
    }

    fn headers(&self) -> Vec<(String, String)> {
        vec![
            ("APCA-API-KEY-ID".to_owned(), self.key_id.clone()),
            ("APCA-API-SECRET-KEY".to_owned(), self.secret.clone()),
        ]
    }

    /// The first message to send on the stream.
    pub fn auth_message(&self) -> String {
        format!(
            "{{\"action\":\"auth\",\"key\":{},\"secret\":{}}}",
            crate::json::quote(&self.key_id),
            crate::json::quote(&self.secret)
        )
    }
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("key_id", &"…")
            .field("secret", &"…")
            .field("id_prefix", &self.id_prefix)
            .finish()
    }
}

/// The second message to send on the stream, after it says it is authorized.
pub const LISTEN_MESSAGE: &str =
    "{\"action\":\"listen\",\"data\":{\"streams\":[\"trade_updates\"]}}";

/// How a submission ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Submission {
    /// Alpaca has the order.
    Accepted(Box<BrokerOrder>),
    /// Alpaca will not take it, and said why. The order did not happen.
    Refused { status: u16, message: String },
    /// Too many requests: nothing was done. Try again after this many seconds.
    RateLimited { retry_after: u64 },
    /// No answer, twice, and the order could not be found: it may still arrive. Not to be closed.
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SubmitError {
    /// The intent cannot be expressed as an Alpaca order.
    Request(RequestError),
    /// Alpaca answered with something that is not an order.
    Answer(ParseError),
}

impl std::fmt::Display for SubmitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SubmitError::Request(e) => write!(f, "{e}"),
            SubmitError::Answer(e) => write!(f, "an answer that is not an order: {e}"),
        }
    }
}

/// How a cancel ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Cancel {
    /// Alpaca took the request; the `canceled` update says when it is done.
    Requested,
    /// The order is already finished or unknown to Alpaca.
    Finished,
    Unknown,
}

pub struct Alpaca<T: Transport> {
    cfg: Config,
    transport: T,
    pub tracker: Tracker,
}

fn message_of(body: &str) -> String {
    Json::parse(body)
        .ok()
        .and_then(|j| j.str_at("message").map(str::to_owned))
        .unwrap_or_else(|| body.chars().take(200).collect())
}

fn unsure(r: &Result<HttpResponse, TransportError>) -> bool {
    match r {
        Err(_) => true,
        Ok(resp) => resp.status >= 500,
    }
}

impl<T: Transport> Alpaca<T> {
    pub fn new(cfg: Config, transport: T) -> Alpaca<T> {
        let tracker = Tracker::new(&cfg.id_prefix);
        Alpaca {
            cfg,
            transport,
            tracker,
        }
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    /// The transport, for a caller that needs to look at it (and for tests).
    pub fn transport(&self) -> &T {
        &self.transport
    }

    fn call(
        &mut self,
        method: Method,
        path: String,
        body: Option<String>,
    ) -> Result<HttpResponse, TransportError> {
        let mut headers = self.cfg.headers();
        if body.is_some() {
            headers.push(("Content-Type".to_owned(), "application/json".to_owned()));
        }
        self.transport.send(&HttpRequest {
            method,
            path,
            headers,
            body,
        })
    }

    /// Look an order up by the name we gave it: `Ok(None)` if Alpaca has no such order.
    pub fn find(&mut self, order: OrderId) -> Result<Option<BrokerOrder>, FindError> {
        let id = crate::wire::client_order_id(&self.cfg.id_prefix, order);
        let r = self.call(
            Method::Get,
            format!("/v2/orders:by_client_order_id?client_order_id={id}"),
            None,
        );
        match r {
            Err(_) => Err(FindError::NoAnswer),
            Ok(resp) if resp.status == 404 => Ok(None),
            Ok(resp) if resp.status == 200 => {
                let order = parse_order(
                    &Json::parse(&resp.body).map_err(|e| FindError::Unreadable(e.to_string()))?,
                )
                .map_err(|e| FindError::Unreadable(e.to_string()))?;
                Ok(Some(order))
            }
            Ok(resp) => Err(FindError::Status(resp.status)),
        }
    }

    /// Send the order the gateway accepted as `order`. See the module notes on an answer that never
    /// came.
    pub fn submit(
        &mut self,
        intent: &Intent,
        order: OrderId,
        symbol: &str,
    ) -> Result<Submission, SubmitError> {
        let req = order_request(intent, order, symbol, &self.cfg.id_prefix)
            .map_err(SubmitError::Request)?;
        for attempt in 0..2 {
            let r = self.call(
                Method::Post,
                "/v2/orders".to_owned(),
                Some(req.body.clone()),
            );
            if !unsure(&r) {
                let resp = r.expect("not an error when it is sure");
                // Refused as a duplicate on the second try means the first one did arrive.
                if attempt > 0 && resp.status == 422 {
                    if let Ok(Some(o)) = self.find(order) {
                        self.tracker.register(order, &o);
                        return Ok(Submission::Accepted(Box::new(o)));
                    }
                }
                return match resp.status {
                    200 => {
                        let parsed =
                            Json::parse(&resp.body).map_err(|e| SubmitError::Answer(e.into()))?;
                        let o = parse_order(&parsed).map_err(SubmitError::Answer)?;
                        self.tracker.register(order, &o);
                        Ok(Submission::Accepted(Box::new(o)))
                    }
                    429 => Ok(Submission::RateLimited {
                        retry_after: resp.retry_after.unwrap_or(1),
                    }),
                    s => Ok(Submission::Refused {
                        status: s,
                        message: message_of(&resp.body),
                    }),
                };
            }
            // No usable answer: is it there anyway?
            match self.find(order) {
                Ok(Some(o)) => {
                    self.tracker.register(order, &o);
                    return Ok(Submission::Accepted(Box::new(o)));
                }
                Ok(None) if attempt == 0 => continue, // not there: ask again under the same name
                _ => break,
            }
        }
        Ok(Submission::Unknown)
    }

    /// Ask Alpaca to cancel an order by its broker id.
    pub fn cancel(&mut self, broker_id: &str) -> Cancel {
        match self.call(Method::Delete, format!("/v2/orders/{broker_id}"), None) {
            Ok(r) if r.status == 204 || r.status == 200 => Cancel::Requested,
            Ok(r) if r.status == 404 || r.status == 422 => Cancel::Finished,
            _ => Cancel::Unknown,
        }
    }
}

/// Why an order could not be looked up.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FindError {
    NoAnswer,
    Status(u16),
    Unreadable(String),
}
