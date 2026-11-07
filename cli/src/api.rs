//! The ladder's HTTP API as the CLI uses it (docs/ladder-api.md): JSON in and
//! out, the token as a bearer, errors as `{error, message}`. Nothing here is
//! shared with the service but that document.

use std::time::Duration;

use reqwest::blocking::{multipart, Client, RequestBuilder, Response};
use reqwest::header::{ACCEPT, AUTHORIZATION};
use reqwest::Method;
use serde_json::Value;

use crate::error::{other, Error, Result};

pub struct Api {
    pub server: String,
    token: Option<String>,
    http: Client,
}

/// The `User-Agent` of every request.
pub const USER_AGENT: &str = concat!("kfm/", env!("CARGO_PKG_VERSION"));

impl Api {
    pub fn new(server: &str, token: Option<String>) -> Result<Api> {
        let http = Client::builder()
            .user_agent(USER_AGENT)
            .timeout(Duration::from_secs(120))
            .connect_timeout(Duration::from_secs(15))
            .build()
            .map_err(other)?;
        Ok(Api {
            server: server.to_owned(),
            token,
            http,
        })
    }

    fn request(&self, method: Method, path: &str) -> RequestBuilder {
        let mut request = self
            .http
            .request(method, format!("{}{path}", self.server))
            // GET /ladder and /matches/{m} answer HTML to a browser (§8).
            .header(ACCEPT, "application/json");
        if let Some(token) = &self.token {
            request = request.header(AUTHORIZATION, format!("Bearer {token}"));
        }
        request
    }

    fn send(&self, request: RequestBuilder) -> Result<Response> {
        let response = request.send().map_err(|e| {
            other(format!(
                "cannot reach the ladder at {}: {}",
                self.server,
                error_chain(&e)
            ))
        })?;
        if response.status().is_success() {
            return Ok(response);
        }
        Err(self.api_error(response))
    }

    fn api_error(&self, response: Response) -> Error {
        let status = response.status();
        let body: Option<Value> = response.json().ok();
        let code = body
            .as_ref()
            .and_then(|b| b["error"].as_str())
            .map(str::to_owned);
        let message = body
            .as_ref()
            .and_then(|b| b["message"].as_str())
            .map(str::to_owned);
        match (code, message) {
            (Some(code), Some(message)) => {
                let message = match code.as_str() {
                    "unauthenticated" if self.token.is_none() => {
                        "not logged in: run kfm login (or set KFM_TOKEN)".to_owned()
                    }
                    "invalid_token" => {
                        format!("{message} Run kfm login again")
                    }
                    _ => message,
                };
                Error::Api { code, message }
            }
            _ => Error::Api {
                code: format!("http_{}", status.as_u16()),
                message: format!("the ladder answered {status}"),
            },
        }
    }

    pub fn get(&self, path: &str) -> Result<Value> {
        self.send(self.request(Method::GET, path))?
            .json()
            .map_err(|e| other(format!("the ladder sent no JSON for {path}: {e}")))
    }

    pub fn get_text(&self, path: &str) -> Result<String> {
        self.send(self.request(Method::GET, path))?
            .text()
            .map_err(other)
    }

    pub fn post(&self, path: &str, body: &Value) -> Result<Value> {
        self.send(self.request(Method::POST, path).json(body))?
            .json()
            .map_err(|e| other(format!("the ladder sent no JSON for {path}: {e}")))
    }

    pub fn delete(&self, path: &str) -> Result<()> {
        self.send(self.request(Method::DELETE, path)).map(|_| ())
    }

    /// `POST /teams/{team}/submissions` with the two parts of §9.5.
    pub fn upload(&self, team: &str, manifest: &str, zip: Vec<u8>) -> Result<Value> {
        let form = multipart::Form::new()
            .part(
                "manifest.toml",
                multipart::Part::text(manifest.to_owned())
                    .file_name("manifest.toml")
                    .mime_str("application/toml")
                    .map_err(other)?,
            )
            .part(
                "source.zip",
                multipart::Part::bytes(zip)
                    .file_name("source.zip")
                    .mime_str("application/zip")
                    .map_err(other)?,
            );
        let path = format!("/teams/{team}/submissions");
        self.send(self.request(Method::POST, &path).multipart(form))?
            .json()
            .map_err(|e| other(format!("the ladder sent no JSON for {path}: {e}")))
    }

    /// Downloads `url` (off the ladder: no bearer).
    pub fn download(&self, url: &str) -> Result<Vec<u8>> {
        let response = self
            .http
            .get(url)
            .send()
            .map_err(|e| other(format!("cannot download {url}: {}", error_chain(&e))))?;
        if !response.status().is_success() {
            return Err(other(format!(
                "cannot download {url}: {}",
                response.status()
            )));
        }
        Ok(response.bytes().map_err(other)?.to_vec())
    }
}

/// An error with its sources, `a: b: c`.
pub fn error_chain(err: &dyn std::error::Error) -> String {
    let mut text = err.to_string();
    let mut source = err.source();
    while let Some(s) = source {
        let next = s.to_string();
        if !text.contains(&next) {
            text.push_str(": ");
            text.push_str(&next);
        }
        source = s.source();
    }
    text
}

/// A query value, form-urlencoded.
pub fn enc(text: &str) -> String {
    url::form_urlencoded::byte_serialize(text.as_bytes()).collect()
}
