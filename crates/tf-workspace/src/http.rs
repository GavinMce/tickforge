//! Routes, sign-in and a small server, std only.
//!
//! Sign-in is one shared token: sent as `Authorization: Bearer <token>` by programs, or typed once
//! into the login page, which sets an HttpOnly, SameSite=Strict cookie holding it. The service
//! speaks plain HTTP and binds to loopback unless told otherwise: put TLS and a real identity
//! provider in front of it before exposing it (E13). The only request that is not a GET is the
//! login form, and it changes nothing but the caller's cookie.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

use crate::{ExplorerError, Source, js, overview, run_detail, runs};

const MAX_HEAD: usize = 16 * 1024;
const MAX_BODY: usize = 1024;
const COOKIE: &str = "tf_session";
const PAGE_CSP: &str = "default-src 'none'; script-src 'self'; connect-src 'self'; style-src 'unsafe-inline'; form-action 'self'; base-uri 'none'; frame-ancestors 'none'";
/// The explorer is one self-contained page with its own inline script and style, and the data it
/// shows is embedded in it: it may run that script but can load nothing and send nothing anywhere.
const EXPLORER_CSP: &str = "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; img-src data:; connect-src 'none'; form-action 'none'; base-uri 'none'; frame-ancestors 'none'";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub query: String,
    pub authorization: Option<String>,
    pub cookie: Option<String>,
    pub body: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub content_type: &'static str,
    pub extra: Vec<(&'static str, String)>,
    pub body: String,
    pub csp: &'static str,
}

impl Response {
    fn new(status: u16, content_type: &'static str, body: impl Into<String>) -> Response {
        Response {
            status,
            content_type,
            extra: Vec::new(),
            body: body.into(),
            csp: PAGE_CSP,
        }
    }

    fn json(status: u16, body: impl Into<String>) -> Response {
        Response::new(status, "application/json", body)
    }

    fn error(status: u16, why: &str) -> Response {
        Response::json(status, format!("{{\"error\":{}}}", js(why)))
    }

    fn with(mut self, k: &'static str, v: impl Into<String>) -> Response {
        self.extra.push((k, v.into()));
        self
    }

    fn reason(&self) -> &'static str {
        match self.status {
            200 => "OK",
            303 => "See Other",
            400 => "Bad Request",
            401 => "Unauthorized",
            404 => "Not Found",
            422 => "Unprocessable Content",
            405 => "Method Not Allowed",
            413 => "Payload Too Large",
            _ => "Internal Server Error",
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut h = format!(
            "HTTP/1.1 {} {}\r\nContent-Type: {}; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nContent-Security-Policy: {}\r\nConnection: close\r\n",
            self.status,
            self.reason(),
            self.content_type,
            self.body.len(),
            self.csp
        );
        for (k, v) in &self.extra {
            h.push_str(&format!("{k}: {v}\r\n"));
        }
        h.push_str("\r\n");
        let mut out = h.into_bytes();
        out.extend_from_slice(self.body.as_bytes());
        out
    }
}

/// A token is at least 16 characters of letters, digits, `-` and `_`.
pub fn valid_token(t: &str) -> bool {
    t.len() >= 16
        && t.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Equal without stopping at the first difference.
fn same(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let mut d = u32::from(a.len() != b.len());
    for i in 0..a.len().max(b.len()) {
        d |= u32::from(a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0));
    }
    d == 0
}

fn signed_in(req: &Request, token: &str) -> bool {
    let bearer = req
        .authorization
        .as_deref()
        .and_then(|a| a.strip_prefix("Bearer "))
        .is_some_and(|t| same(t.trim(), token));
    let cookie = req.cookie.as_deref().is_some_and(|c| {
        c.split(';')
            .filter_map(|p| p.trim().strip_prefix(&format!("{COOKIE}=")))
            .any(|v| same(v, token))
    });
    bearer || cookie
}

fn query_value(q: &str, key: &str) -> Option<String> {
    q.split('&')
        .filter_map(|p| p.split_once('='))
        .find(|(k, _)| *k == key)
        .map(|(_, v)| decode(v))
}

pub(crate) fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let (mut out, mut i) = (Vec::new(), 0);
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                let h = std::str::from_utf8(&b[i + 1..i + 3])
                    .ok()
                    .and_then(|h| u8::from_str_radix(h, 16).ok());
                match h {
                    Some(v) => {
                        out.push(v);
                        i += 2;
                    }
                    None => out.push(b'%'),
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

const LOGIN: &str = "<!doctype html><meta charset=utf-8><meta name=viewport content=\"width=device-width,initial-scale=1\"><title>Workspace sign-in</title><style>body{font:16px system-ui;margin:3rem auto;max-width:22rem;padding:0 1rem}input,button{font:inherit;padding:.5rem;width:100%;box-sizing:border-box;margin:.25rem 0}</style><h1>Workspace</h1><form method=post action=/login><label>Access token<input type=password name=token autocomplete=current-password autofocus></label><button>Sign in</button></form>";

const HOME: &str = include_str!("ui/index.html");
const APP_JS: &str = include_str!("ui/app.js");

fn escape(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '&' => "&amp;".to_owned(),
            '<' => "&lt;".to_owned(),
            '>' => "&gt;".to_owned(),
            '"' => "&quot;".to_owned(),
            '\'' => "&#39;".to_owned(),
            c => c.to_string(),
        })
        .collect()
}

fn notice(status: u16, title: &str, why: &str) -> Response {
    Response::new(
        status,
        "text/html",
        format!(
            "<!doctype html><meta charset=utf-8><meta name=viewport content=\"width=device-width,initial-scale=1\"><title>{t}</title><style>body{{font:16px system-ui;margin:3rem auto;max-width:40rem;padding:0 1rem}}</style><h1>{t}</h1><p>{w}</p><p><a href=/>Back to the overview</a></p>",
            t = escape(title),
            w = escape(why)
        ),
    )
}

/// A stored run in the trade explorer: the whole page, replayed and checked against what was stored.
fn explorer(src: &Source, run: &str) -> Response {
    if run.len() < 8 || run.len() > 64 || !run.bytes().all(|b| b.is_ascii_hexdigit()) {
        return notice(
            404,
            "No such run",
            "A run is named by its hash, at least 8 hex digits.",
        );
    }
    let Some(e) = &src.explorer else {
        return notice(
            404,
            "No run store",
            "This service was not given a run store to open runs from.",
        );
    };
    match e.open(run) {
        Ok(page) => {
            let mut r = Response::new(200, "text/html", page);
            r.csp = EXPLORER_CSP;
            r
        }
        Err(ExplorerError::NotFound(m)) => notice(404, "No such run", &m),
        Err(ExplorerError::Refused(m)) => notice(422, "This run cannot be opened", &m),
    }
}

/// Answers one request. Reads the ledger and run store named by `src`; writes nothing.
pub fn handle(src: &Source, token: &str, req: &Request) -> Response {
    if req.method == "POST" && req.path == "/login" {
        let given = req
            .body
            .strip_prefix("token=")
            .map(decode)
            .unwrap_or_default();
        return if same(given.trim(), token) {
            Response::new(303, "text/html", "")
                .with("Location", "/")
                .with(
                    "Set-Cookie",
                    format!("{COOKIE}={token}; Path=/; HttpOnly; SameSite=Strict"),
                )
        } else {
            Response::new(401, "text/html", LOGIN)
        };
    }
    if req.method != "GET" {
        return Response::error(405, "this service only reads").with("Allow", "GET");
    }
    if req.path == "/health" {
        return Response::json(200, "{\"ok\":true}");
    }
    if let Some(run) = req.path.strip_prefix("/explorer/") {
        if !signed_in(req, token) {
            return Response::new(401, "text/html", LOGIN);
        }
        return explorer(src, run);
    }
    let known = matches!(
        req.path.as_str(),
        "/" | "/app.js" | "/api/overview" | "/api/runs" | "/api/run"
    );
    if !known {
        return Response::error(404, "no such route");
    }
    if !signed_in(req, token) {
        return if req.path == "/" {
            Response::new(200, "text/html", LOGIN)
        } else {
            Response::error(401, "sign in").with("WWW-Authenticate", "Bearer")
        };
    }
    let answer = match req.path.as_str() {
        "/" => return Response::new(200, "text/html", HOME),
        "/app.js" => return Response::new(200, "text/javascript", APP_JS),
        "/api/overview" => overview(src),
        "/api/run" => {
            let (Some(strategy), Some(id)) = (
                query_value(&req.query, "strategy"),
                query_value(&req.query, "id"),
            ) else {
                return Response::error(400, "give strategy and id");
            };
            match run_detail(src, &strategy, &id) {
                Ok(Some(body)) => return Response::json(200, body),
                Ok(None) => return Response::error(404, "no such run"),
                Err(e) => Err(e),
            }
        }
        _ => runs(src, query_value(&req.query, "strategy").as_deref()),
    };
    match answer {
        Ok(body) => Response::json(200, body),
        Err(e) => Response::error(500, &e),
    }
}

fn read_request(s: &mut TcpStream) -> Result<Request, Response> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 2048];
    let head_end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i;
        }
        if buf.len() > MAX_HEAD {
            return Err(Response::error(413, "request head too large"));
        }
        match s.read(&mut chunk) {
            Ok(0) | Err(_) => return Err(Response::error(400, "incomplete request")),
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    };
    if head_end > MAX_HEAD {
        return Err(Response::error(413, "request head too large"));
    }
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut first = lines.next().unwrap_or("").split(' ');
    let (Some(method), Some(target), Some(version)) = (first.next(), first.next(), first.next())
    else {
        return Err(Response::error(400, "bad request line"));
    };
    if !version.starts_with("HTTP/1.") {
        return Err(Response::error(400, "bad request line"));
    }
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let (mut authorization, mut cookie, mut length) = (None, None, 0usize);
    for l in lines {
        let Some((k, v)) = l.split_once(':') else {
            return Err(Response::error(400, "bad header"));
        };
        match k.trim().to_ascii_lowercase().as_str() {
            "authorization" => authorization = Some(v.trim().to_owned()),
            "cookie" => cookie = Some(v.trim().to_owned()),
            "content-length" => {
                length = v
                    .trim()
                    .parse()
                    .map_err(|_| Response::error(400, "bad content-length"))?;
            }
            _ => {}
        }
    }
    if length > MAX_BODY {
        return Err(Response::error(413, "body too large"));
    }
    let mut body = buf[head_end + 4..].to_vec();
    while body.len() < length {
        match s.read(&mut chunk) {
            Ok(0) | Err(_) => return Err(Response::error(400, "incomplete body")),
            Ok(n) => body.extend_from_slice(&chunk[..n]),
        }
    }
    body.truncate(length);
    Ok(Request {
        method: method.to_owned(),
        path: path.to_owned(),
        query: query.to_owned(),
        authorization,
        cookie,
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}

/// Accepts connections one at a time and answers each, until `max` have been served (forever if
/// `None`). A client that stalls is dropped after five seconds.
pub fn serve(listener: &TcpListener, src: &Source, token: &str, max: Option<usize>) {
    let mut served = 0;
    for conn in listener.incoming() {
        let Ok(mut s) = conn else { continue };
        let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
        let _ = s.set_write_timeout(Some(Duration::from_secs(5)));
        let resp = match read_request(&mut s) {
            Ok(req) => handle(src, token, &req),
            Err(r) => r,
        };
        let _ = s.write_all(&resp.to_bytes());
        served += 1;
        if max.is_some_and(|m| served >= m) {
            return;
        }
    }
}
