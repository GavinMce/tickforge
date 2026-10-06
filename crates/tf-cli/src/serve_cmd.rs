//! `tf serve`: the read-only workspace service.
//!
//! Reads an order ledger (without its lock: an engine may be writing it) and a run store, and
//! answers GETs behind a shared token. It cannot place an order or change a budget.

use std::net::{SocketAddr, TcpListener};
use std::path::PathBuf;

use tf_catalog::Kind;
use tf_workspace::Source;
use tf_workspace::http::{serve, valid_token};

const USAGE: &str = "usage: tf serve --token-file FILE [--ledger DIR --kind paper|live] [--store DIR] [--addr HOST:PORT]";

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Config {
    pub source_ledger: Option<(PathBuf, Kind)>,
    pub store: Option<PathBuf>,
    pub addr: SocketAddr,
    pub token_file: PathBuf,
}

pub(crate) fn parse(args: &[String]) -> Result<Config, String> {
    let (mut ledger, mut kind, mut store, mut addr, mut token) = (None, None, None, None, None);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut value = |what: &str| it.next().cloned().ok_or(format!("{what} needs a value"));
        match a.as_str() {
            "--ledger" => ledger = Some(PathBuf::from(value("--ledger")?)),
            "--kind" => {
                let k = value("--kind")?;
                kind = Some(
                    Kind::parse_session(&k).ok_or(format!("--kind is paper or live, not {k}"))?,
                );
            }
            "--store" => store = Some(PathBuf::from(value("--store")?)),
            "--addr" => {
                let v = value("--addr")?;
                addr = Some(
                    v.parse::<SocketAddr>()
                        .map_err(|e| format!("--addr {v}: {e}"))?,
                );
            }
            "--token-file" => token = Some(PathBuf::from(value("--token-file")?)),
            other => return Err(format!("unexpected argument {other}\n{USAGE}")),
        }
    }
    let token_file = token.ok_or_else(|| USAGE.to_owned())?;
    if ledger.is_none() && store.is_none() {
        return Err(format!(
            "nothing to serve: give --ledger or --store\n{USAGE}"
        ));
    }
    let source_ledger = match (ledger, kind) {
        (Some(l), Some(k)) => Some((l, k)),
        (Some(_), None) => return Err("--ledger needs --kind paper|live".to_owned()),
        (None, Some(_)) => return Err("--kind only applies to --ledger".to_owned()),
        (None, None) => None,
    };
    Ok(Config {
        source_ledger,
        store,
        addr: addr.unwrap_or_else(|| SocketAddr::from(([127, 0, 0, 1], 8787))),
        token_file,
    })
}

/// The token in `path`, which must be a plain string of at least 16 characters.
pub(crate) fn read_token(path: &std::path::Path) -> Result<String, String> {
    let t = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let t = t.trim().to_owned();
    if valid_token(&t) {
        Ok(t)
    } else {
        Err(format!(
            "{}: the token must be at least 16 letters, digits, - or _",
            path.display()
        ))
    }
}

pub(crate) fn serve_cmd(args: &[String]) -> Result<(), String> {
    let c = parse(args)?;
    let token = read_token(&c.token_file)?;
    let listener = TcpListener::bind(c.addr).map_err(|e| format!("{}: {e}", c.addr))?;
    if !c.addr.ip().is_loopback() {
        eprintln!(
            "warning: {} is not a loopback address and this service speaks plain HTTP: put TLS in front of it",
            c.addr
        );
    }
    eprintln!("serving the workspace read-only on http://{}", c.addr);
    let src = Source {
        ledger: c.source_ledger,
        store: c.store,
    };
    serve(&listener, &src, &token, None);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn arguments_are_checked_and_the_default_address_is_loopback() {
        let c = parse(&args(&[
            "--token-file",
            "t",
            "--ledger",
            "L",
            "--kind",
            "live",
            "--store",
            "S",
        ]))
        .unwrap();
        assert_eq!(c.addr, "127.0.0.1:8787".parse().unwrap());
        assert_eq!(c.source_ledger, Some((PathBuf::from("L"), Kind::Live)));
        assert_eq!(c.store, Some(PathBuf::from("S")));
        let c = parse(&args(&[
            "--token-file",
            "t",
            "--store",
            "S",
            "--addr",
            "0.0.0.0:9000",
        ]))
        .unwrap();
        assert_eq!(c.addr.port(), 9000);
        assert_eq!(c.source_ledger, None);
        let err = |a: &[&str]| parse(&args(a)).unwrap_err();
        assert!(err(&[]).starts_with("usage:"));
        assert!(
            err(&["--store", "S"]).starts_with("usage:"),
            "a token file is required"
        );
        assert!(err(&["--token-file", "t"]).contains("nothing to serve"));
        assert!(err(&["--token-file", "t", "--ledger", "L"]).contains("needs --kind"));
        assert!(
            err(&["--token-file", "t", "--store", "S", "--kind", "paper"]).contains("only applies")
        );
        assert!(
            err(&["--token-file", "t", "--ledger", "L", "--kind", "backtest"])
                .contains("paper or live")
        );
        assert!(
            err(&["--token-file", "t", "--store", "S", "--addr", "nope"]).contains("--addr nope")
        );
        assert!(err(&["--token-file"]).contains("needs a value"));
        assert!(err(&["--wat"]).contains("unexpected argument"));
    }

    #[test]
    fn a_weak_or_missing_token_file_is_refused() {
        let d = std::env::temp_dir().join(format!("tf-servecmd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let f = d.join("token");
        std::fs::write(&f, "  0123456789abcdef\n").unwrap();
        assert_eq!(read_token(&f).unwrap(), "0123456789abcdef");
        std::fs::write(&f, "short").unwrap();
        assert!(read_token(&f).unwrap_err().contains("at least 16"));
        std::fs::write(&f, "has spaces in it, long enough").unwrap();
        assert!(read_token(&f).is_err());
        assert!(read_token(&d.join("missing")).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }
}
