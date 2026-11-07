//! `kfm login`: the ladder's CLI login (docs/ladder-api.md §5.1). A listener
//! on an ephemeral 127.0.0.1 port, the browser sent to
//! `<server>/auth/cli?port=&state=&host=`, the redirect back to
//! `/callback?state=&code=` checked against our state, and the code
//! exchanged for the token with `POST /auth/cli/exchange`.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::api::{enc, Api};
use crate::error::{other, Error, Result};

/// How long `kfm login` waits for the browser.
pub const WAIT: Duration = Duration::from_secs(600);

/// 256 random bits in hex: the `state` that ties the redirect to us.
pub fn new_state() -> Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| other(format!("no random source: {e}")))?;
    Ok(hex::encode(bytes))
}

/// This computer's name for the token (`kfm on <host>`).
pub fn host_name() -> String {
    hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .unwrap_or_default()
}

/// The page `kfm login` opens.
pub fn login_url(server: &str, port: u16, state: &str, host: &str) -> String {
    let mut url = format!("{server}/auth/cli?port={port}&state={state}");
    if !host.is_empty() {
        url.push_str("&host=");
        url.push_str(&enc(host));
    }
    url
}

/// What came back to the listener.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Callback {
    Code(String),
    /// The browser said no (`access_denied`) or the ladder could not make the token.
    Refused(String),
}

/// Reads `GET /callback?...` off a request line: `None` for anything else
/// (a favicon, a stranger's guess at the state).
pub fn parse_request_line(line: &str, state: &str) -> Option<Callback> {
    let mut parts = line.split_whitespace();
    if parts.next()? != "GET" {
        return None;
    }
    let target = parts.next()?;
    let (path, query) = target.split_once('?')?;
    if path != "/callback" {
        return None;
    }
    let mut got_state = None;
    let mut code = None;
    let mut error = None;
    for (k, v) in url::form_urlencoded::parse(query.as_bytes()) {
        match &*k {
            "state" => got_state = Some(v.into_owned()),
            "code" => code = Some(v.into_owned()),
            "error" => error = Some(v.into_owned()),
            _ => {}
        }
    }
    if got_state.as_deref() != Some(state) {
        return None;
    }
    match (code, error) {
        (_, Some(error)) => Some(Callback::Refused(error)),
        (Some(code), None) => Some(Callback::Code(code)),
        (None, None) => None,
    }
}

fn respond(stream: &mut TcpStream, status: &str, title: &str, body: &str) {
    let html = format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><title>{title}</title>\
         <style>body{{font:16px/1.5 system-ui,sans-serif;max-width:40em;margin:4em auto;padding:0 1em}}</style>\
         </head><body><h1>{title}</h1><p>{body}</p></body></html>"
    );
    let _ = write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nConnection: close\r\n\r\n{html}",
        html.len()
    );
    let _ = stream.flush();
}

/// Serves the listener until our callback arrives or `WAIT` passes.
fn wait_for_callback(listener: &TcpListener, state: &str, deadline: Instant) -> Result<Callback> {
    listener.set_nonblocking(true).map_err(other)?;
    loop {
        if Instant::now() > deadline {
            return Err(other(
                "timed out waiting for the browser; run kfm login again",
            ));
        }
        let mut stream = match listener.accept() {
            Ok((stream, _)) => stream,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
            Err(e) => return Err(other(format!("the login listener failed: {e}"))),
        };
        let _ = stream.set_nonblocking(false);
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let mut line = String::new();
        let mut reader = BufReader::new(match stream.try_clone() {
            Ok(s) => s,
            Err(_) => continue,
        });
        if reader.read_line(&mut line).is_err() {
            continue;
        }
        // Drain the headers so the browser sees a clean answer.
        let mut header = String::new();
        while reader.read_line(&mut header).is_ok_and(|n| n > 2) {
            header.clear();
        }
        match parse_request_line(&line, state) {
            Some(Callback::Code(code)) => {
                respond(
                    &mut stream,
                    "200 OK",
                    "kfm is logged in",
                    "The kfm command line tool has its token. You can close this tab.",
                );
                return Ok(Callback::Code(code));
            }
            Some(Callback::Refused(why)) => {
                respond(
                    &mut stream,
                    "200 OK",
                    "kfm login stopped",
                    "The login was not completed. You can close this tab.",
                );
                return Ok(Callback::Refused(why));
            }
            None => respond(
                &mut stream,
                "404 Not Found",
                "Not found",
                "This is kfm login's listener; it waits for the ladder's redirect only.",
            ),
        }
    }
}

/// What the exchange returns.
#[derive(Debug, Clone)]
pub struct Token {
    pub token: String,
    pub id: String,
    pub name: String,
    pub scope: String,
}

/// The whole flow; the token for `server`.
pub fn login(server: &str, no_browser: bool) -> Result<Token> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .map_err(|e| other(format!("cannot listen on 127.0.0.1: {e}")))?;
    let port = listener.local_addr().map_err(other)?.port();
    let state = new_state()?;
    let url = login_url(server, port, &state, &host_name());
    let opened = !no_browser && open::that_detached(&url).is_ok();
    if opened {
        eprintln!("kfm: opened the ladder in your browser; if nothing appeared, open:");
    } else {
        eprintln!("kfm: open this URL in a browser on this computer:");
    }
    eprintln!("  {url}");
    eprintln!(
        "kfm: waiting for the browser to come back to 127.0.0.1:{port} (from another machine: \
         ssh -L {port}:127.0.0.1:{port} to this one first)"
    );
    let code = match wait_for_callback(&listener, &state, Instant::now() + WAIT)? {
        Callback::Code(code) => code,
        Callback::Refused(why) if why == "access_denied" => {
            return Err(other("the login was cancelled in the browser"));
        }
        Callback::Refused(why) => {
            return Err(Error::Api {
                message: "the ladder could not complete the login".to_owned(),
                code: why,
            });
        }
    };
    let api = Api::new(server, None)?;
    let body = api.post("/auth/cli/exchange", &json!({ "code": code }))?;
    let field = |k: &str| body[k].as_str().map(str::to_owned);
    match (field("token"), field("token_id")) {
        (Some(token), Some(id)) => Ok(Token {
            token,
            id,
            name: field("name").unwrap_or_default(),
            scope: field("scope").unwrap_or_default(),
        }),
        _ => Err(other(format!(
            "the exchange sent no token: {}",
            strip(&body)
        ))),
    }
}

/// A JSON value without a `token` key, for messages.
fn strip(body: &Value) -> String {
    let mut body = body.clone();
    if let Some(o) = body.as_object_mut() {
        o.remove("token");
    }
    body.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn states_are_256_bits_of_hex_and_differ() {
        let a = new_state().unwrap();
        assert_eq!(a.len(), 64);
        assert!(a.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(a, new_state().unwrap());
    }

    #[test]
    fn the_url_names_the_listener_and_the_host() {
        assert_eq!(
            login_url("https://l.example", 5000, "abc", "my box"),
            "https://l.example/auth/cli?port=5000&state=abc&host=my+box"
        );
        assert_eq!(
            login_url("http://x", 5000, "abc", ""),
            "http://x/auth/cli?port=5000&state=abc"
        );
    }

    #[test]
    fn only_our_callback_counts() {
        let s = "S1";
        assert_eq!(
            parse_request_line("GET /callback?state=S1&code=abc HTTP/1.1\r\n", s),
            Some(Callback::Code("abc".into()))
        );
        assert_eq!(
            parse_request_line("GET /callback?state=S1&error=access_denied HTTP/1.1", s),
            Some(Callback::Refused("access_denied".into()))
        );
        assert_eq!(
            parse_request_line("GET /callback?state=S2&code=abc HTTP/1.1", s),
            None
        );
        assert_eq!(
            parse_request_line("GET /callback?code=abc HTTP/1.1", s),
            None
        );
        assert_eq!(parse_request_line("GET /favicon.ico HTTP/1.1", s), None);
        assert_eq!(
            parse_request_line("POST /callback?state=S1&code=abc HTTP/1.1", s),
            None
        );
        assert_eq!(
            parse_request_line("GET /callback?state=S1 HTTP/1.1", s),
            None
        );
    }
}
