//! One connection to the gateway: connect, log in, subscribe, start.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use crate::protocol::{
    ApiKey, LiveError, START_SESSION, Sub, auth_line, gateway_for, parse_auth_response,
    parse_challenge, sub_lines,
};

/// What it takes to open a session.
#[derive(Clone, Debug)]
pub struct Config {
    /// `host:port`; the dataset's own gateway if none.
    pub addr: Option<String>,
    pub key: ApiKey,
    pub dataset: String,
    pub subs: Vec<Sub>,
    /// The gateway sends a heartbeat when it has had nothing else to say for this long.
    pub heartbeat_secs: u32,
    pub connect_timeout: Duration,
    /// How long each step of logging in may take.
    pub login_timeout: Duration,
    /// A session with no bytes at all (data or heartbeat) for this long is dead.
    pub stall_secs: u64,
}

impl Config {
    pub fn new(key: ApiKey, dataset: &str, subs: Vec<Sub>) -> Config {
        Config {
            addr: None,
            key,
            dataset: dataset.to_owned(),
            subs,
            heartbeat_secs: 5,
            connect_timeout: Duration::from_secs(10),
            login_timeout: Duration::from_secs(10),
            stall_secs: 20,
        }
    }
}

pub struct Session {
    /// Everything the gateway sends, buffered: the DBN decoder reads from here so no byte read ahead
    /// during login is lost.
    pub reader: BufReader<TcpStream>,
    /// A second handle on the socket, to shut it down from another thread.
    pub control: TcpStream,
    pub session_id: String,
}

fn read_line(r: &mut BufReader<TcpStream>) -> Result<String, LiveError> {
    let mut line = String::new();
    // A line is short. Anything else is not the gateway.
    let n = r
        .by_ref()
        .take(4096)
        .read_line(&mut line)
        .map_err(LiveError::Io)?;
    if n == 0 {
        return Err(LiveError::Protocol(
            "the gateway closed the connection".to_owned(),
        ));
    }
    if !line.ends_with('\n') {
        return Err(LiveError::Protocol("a line that does not end".to_owned()));
    }
    Ok(line)
}

/// A logged-in connection that has not subscribed to anything yet.
pub struct Login {
    pub reader: BufReader<TcpStream>,
    pub writer: TcpStream,
    pub control: TcpStream,
    pub session_id: String,
}

/// Connect and log in. Nothing is asked for: closing it now costs nothing.
pub fn login(cfg: &Config) -> Result<Login, LiveError> {
    let addr = cfg
        .addr
        .clone()
        .unwrap_or_else(|| gateway_for(&cfg.dataset));
    let mut last = None;
    let mut stream = None;
    for a in addr.to_socket_addrs().map_err(LiveError::Connect)? {
        match TcpStream::connect_timeout(&a, cfg.connect_timeout) {
            Ok(s) => {
                stream = Some(s);
                break;
            }
            Err(e) => last = Some(e),
        }
    }
    let stream = stream.ok_or_else(|| {
        LiveError::Connect(last.unwrap_or_else(|| std::io::Error::other("no address")))
    })?;
    stream.set_nodelay(true).map_err(LiveError::Io)?;
    stream
        .set_read_timeout(Some(cfg.login_timeout))
        .map_err(LiveError::Io)?;
    stream
        .set_write_timeout(Some(cfg.login_timeout))
        .map_err(LiveError::Io)?;
    let control = stream.try_clone().map_err(LiveError::Io)?;
    let mut writer = stream.try_clone().map_err(LiveError::Io)?;
    let mut reader = BufReader::with_capacity(1 << 20, stream);

    let greeting = read_line(&mut reader)?;
    if !greeting.starts_with("lsg_version=") {
        return Err(LiveError::Protocol(format!(
            "expected a greeting, got `{}`",
            greeting.trim_end().chars().take(80).collect::<String>()
        )));
    }
    let challenge = read_line(&mut reader)?;
    let challenge = parse_challenge(&challenge)?.to_owned();
    let client = concat!("tickforge/", env!("CARGO_PKG_VERSION"));
    writer
        .write_all(
            auth_line(
                &cfg.key,
                &cfg.dataset,
                &challenge,
                Some(cfg.heartbeat_secs),
                client,
            )
            .as_bytes(),
        )
        .map_err(LiveError::Io)?;
    let session_id = parse_auth_response(&read_line(&mut reader)?)?;
    Ok(Login {
        reader,
        writer,
        control,
        session_id,
    })
}

/// Open a session and start it: the gateway is sending DBN when this returns. With `start`, every
/// subscription replays from that time (nanoseconds since the epoch).
pub fn open(cfg: &Config, start: Option<u64>) -> Result<Session, LiveError> {
    let Login {
        reader,
        mut writer,
        control,
        session_id,
    } = login(cfg)?;
    for (n, sub) in cfg.subs.iter().enumerate() {
        for line in sub_lines(sub, n as u32 + 1, start)? {
            writer.write_all(line.as_bytes()).map_err(LiveError::Io)?;
        }
    }
    writer
        .write_all(START_SESSION.as_bytes())
        .map_err(LiveError::Io)?;
    writer.flush().map_err(LiveError::Io)?;
    reader
        .get_ref()
        .set_read_timeout(Some(Duration::from_secs(cfg.stall_secs.max(1))))
        .map_err(LiveError::Io)?;
    Ok(Session {
        reader,
        control,
        session_id,
    })
}
