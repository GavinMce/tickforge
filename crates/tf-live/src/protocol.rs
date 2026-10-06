//! The gateway's text protocol (Databento live API, raw), written from the published description and the
//! official client's behaviour.
//!
//! The gateway speaks first: a greeting line (`lsg_version=...`) and a challenge line (`cram=...`). The
//! client answers with one line, `auth=SHA256(challenge|key)-bucket|dataset=...|encoding=dbn|...`, where
//! the bucket is the last five characters of the key. The reply is one line of `key=value` pairs
//! separated by `|`, with `success=1` and a `session_id`, or `success=0` and an `error`. Then the client
//! sends subscription lines (`schema=...|stype_in=...|symbols=...|snapshot=0|is_last=1[|start=ns][|id=n]`,
//! at most 500 symbols a line, `is_last=1` on the last line of a subscription) and `start_session`; from
//! then on the gateway sends a DBN stream.

use std::fmt;

use crate::sha256::{hex, sha256};

#[derive(Debug)]
pub enum LiveError {
    /// A key that cannot be one.
    BadKey(&'static str),
    Connect(std::io::Error),
    Io(std::io::Error),
    /// The gateway said something this client does not follow.
    Protocol(String),
    /// The gateway refused the login; its words.
    Auth(String),
    /// No data and no heartbeat for too long.
    Stalled(u64),
}

impl fmt::Display for LiveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LiveError::BadKey(w) => write!(f, "the API key is not valid: {w}"),
            LiveError::Connect(e) => write!(f, "cannot connect to the gateway: {e}"),
            LiveError::Io(e) => write!(f, "gateway i/o: {e}"),
            LiveError::Protocol(m) => write!(f, "gateway protocol: {m}"),
            LiveError::Auth(m) => write!(f, "the gateway refused the login: {m}"),
            LiveError::Stalled(s) => write!(f, "nothing, not even a heartbeat, for {s} seconds"),
        }
    }
}

impl std::error::Error for LiveError {}

/// A Databento API key. Never printed whole.
#[derive(Clone)]
pub struct ApiKey(String);

const KEY_LEN: usize = 32;
const BUCKET_LEN: usize = 5;

impl ApiKey {
    pub fn new(key: &str) -> Result<ApiKey, LiveError> {
        let key = key.trim();
        if key == "$YOUR_API_KEY" {
            return Err(LiveError::BadKey("that is the placeholder"));
        }
        if key.len() != KEY_LEN {
            return Err(LiveError::BadKey("expected 32 characters"));
        }
        if !key.is_ascii()
            || key
                .bytes()
                .any(|b| b.is_ascii_control() || b == b'|' || b == b' ')
        {
            return Err(LiveError::BadKey(
                "expected printable ASCII with no spaces or bars",
            ));
        }
        Ok(ApiKey(key.to_owned()))
    }

    pub fn bucket_id(&self) -> &str {
        &self.0[KEY_LEN - BUCKET_LEN..]
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "\"...{}\"", self.bucket_id())
    }
}

/// `xnas.basic` -> `xnas-basic.lsg.databento.com:13000`.
pub fn gateway_for(dataset: &str) -> String {
    format!(
        "{}.lsg.databento.com:13000",
        dataset.replace('.', "-").to_ascii_lowercase()
    )
}

/// The challenge in the gateway's second line.
pub fn parse_challenge(line: &str) -> Result<&str, LiveError> {
    line.trim_end()
        .strip_prefix("cram=")
        .filter(|c| !c.is_empty())
        .ok_or_else(|| {
            LiveError::Protocol(format!(
                "expected a `cram=` challenge, got `{}`",
                line.trim_end().chars().take(80).collect::<String>()
            ))
        })
}

/// The login line, with its newline.
pub fn auth_line(
    key: &ApiKey,
    dataset: &str,
    challenge: &str,
    heartbeat_s: Option<u32>,
    client: &str,
) -> String {
    let digest = hex(&sha256(format!("{challenge}|{}", key.0).as_bytes()));
    let mut s = format!(
        "auth={digest}-{}|dataset={dataset}|encoding=dbn|ts_out=0|client={client}",
        key.bucket_id()
    );
    if let Some(h) = heartbeat_s {
        s.push_str(&format!("|heartbeat_interval_s={h}"));
    }
    s.push('\n');
    s
}

/// The session id from a successful login reply, or the gateway's error.
pub fn parse_auth_response(line: &str) -> Result<String, LiveError> {
    let pairs: Vec<(&str, &str)> = line
        .trim_end()
        .split('|')
        .filter_map(|kv| kv.split_once('='))
        .collect();
    let get = |k: &str| pairs.iter().find(|p| p.0 == k).map(|p| p.1);
    match get("success") {
        Some("1") => Ok(get("session_id").unwrap_or("").to_owned()),
        _ => Err(LiveError::Auth(get("error").map_or_else(
            || line.trim_end().chars().take(200).collect(),
            str::to_owned,
        ))),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Symbols {
    All,
    List(Vec<String>),
}

/// One subscription: a schema over some symbols.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sub {
    pub schema: String,
    pub stype_in: String,
    pub symbols: Symbols,
    /// Replay from this time (nanoseconds since the epoch) before going live, if the plan allows.
    pub start: Option<u64>,
    pub snapshot: bool,
}

impl Sub {
    /// Every symbol of a dataset for one schema, by raw symbol.
    pub fn all(schema: &str) -> Sub {
        Sub {
            schema: schema.to_owned(),
            stype_in: "raw_symbol".to_owned(),
            symbols: Symbols::All,
            start: None,
            snapshot: false,
        }
    }
}

pub const MAX_SYMBOLS_PER_LINE: usize = 500;

/// The lines for a subscription: one, or one per 500 symbols with `is_last` on the final one.
pub fn sub_lines(
    sub: &Sub,
    id: u32,
    start_override: Option<u64>,
) -> Result<Vec<String>, LiveError> {
    let start = start_override.or(sub.start);
    if sub.snapshot && start.is_some() {
        return Err(LiveError::Protocol(
            "a snapshot cannot be asked for with a start time".to_owned(),
        ));
    }
    let chunks: Vec<String> = match &sub.symbols {
        Symbols::All => vec!["ALL_SYMBOLS".to_owned()],
        Symbols::List(l) if l.is_empty() => {
            return Err(LiveError::Protocol(
                "a subscription with no symbols".to_owned(),
            ));
        }
        Symbols::List(l) => {
            if let Some(bad) = l
                .iter()
                .find(|s| s.is_empty() || s.contains([',', '|', '\n', ' ']))
            {
                return Err(LiveError::Protocol(format!(
                    "`{bad}` cannot be sent as a symbol"
                )));
            }
            l.chunks(MAX_SYMBOLS_PER_LINE)
                .map(|c| c.join(","))
                .collect()
        }
    };
    let last = chunks.len() - 1;
    Ok(chunks
        .into_iter()
        .enumerate()
        .map(|(i, syms)| {
            let mut s = format!(
                "schema={}|stype_in={}|symbols={syms}|snapshot={}|is_last={}",
                sub.schema,
                sub.stype_in,
                u8::from(sub.snapshot),
                u8::from(i == last)
            );
            if let Some(t) = start {
                s.push_str(&format!("|start={t}"));
            }
            s.push_str(&format!("|id={id}\n"));
            s
        })
        .collect())
}

pub const START_SESSION: &str = "start_session\n";
