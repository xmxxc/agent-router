//! Submit one batch to the explicitly selected OpenCode controller session.
use super::Outcome;
use anyhow::{ensure, Context, Result};
use curl::easy::{Easy2, Handler, HttpVersion, List, NetRc};
use http::Uri;
use serde_json::json;
use std::time::Duration;

pub(super) struct Target<'a> {
    pub url: &'a str,
    pub session: &'a str,
}

#[derive(Default)]
struct ResponseHeaders {
    final_response: bool,
    complete: bool,
}

impl Handler for ResponseHeaders {
    fn header(&mut self, line: &[u8]) -> bool {
        if let Some(status) = line.strip_prefix(b"HTTP/") {
            self.final_response = status
                .split(|b| b.is_ascii_whitespace())
                .filter(|word| !word.is_empty())
                .nth(1)
                .is_some_and(|code| code.len() == 3 && matches!(code.first(), Some(b'2'..=b'5')));
        }
        // prompt_async confirms the submission in the final response headers.
        // Stop reading at that boundary, without waiting for a response body.
        self.complete = self.final_response && matches!(line, b"\r\n" | b"\n");
        !self.complete
    }
}

impl<'a> Target<'a> {
    pub fn parse(url: &'a str, session: &'a str) -> Result<Self> {
        // Keep the caller's origin and optional path prefix. Query, fragment,
        // and userinfo could change where the appended session path is sent.
        let invalid = "give an HTTP(S) --opencode-url without credentials, query or fragment";
        let uri: Uri = url.parse().map_err(|_| anyhow::anyhow!(invalid))?;
        let authority = uri.authority().context(invalid)?;
        ensure!(
            matches!(uri.scheme_str(), Some("http" | "https"))
                && !authority.as_str().contains('@')
                && !authority.host().is_empty()
                && !url.contains(['?', '#', '\\']),
            "{invalid}"
        );
        // Authority::port() returns None for both absent and malformed ports.
        // The host includes brackets for IPv6, so only its suffix can be a port.
        let suffix = &authority.as_str()[authority.host().len()..];
        ensure!(
            suffix.is_empty()
                || suffix.strip_prefix(':').is_some_and(|port| {
                    port.bytes().all(|b| b.is_ascii_digit())
                        && port.parse::<u16>().is_ok_and(|number| number != 0)
                }),
            "--opencode-url must use a decimal port in 1..=65535"
        );
        ensure!(
            !matches!(session, "." | "..")
                && session
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-._~".contains(&b)),
            "--opencode-session must be a single session ID"
        );
        Ok(Self { url, session })
    }

    pub fn deliver(&self, text: &str, limit: Duration) -> Outcome {
        if limit.is_zero() {
            return Outcome::Failed;
        }
        let mut request = match self.request(text, limit) {
            Ok(request) => request,
            Err(error) => {
                println!(
                    "router relay: could not prepare OpenCode request (curl code {})",
                    error.code()
                );
                return Outcome::Failed;
            }
        };

        let result = request.perform();
        if request.get_ref().complete {
            match request.response_code() {
                Ok(200..=299) => return Outcome::Delivered,
                Ok(status) => {
                    println!(
                        "router relay: OpenCode answered HTTP {status}; delivery is uncertain"
                    );
                }
                Err(_) => println!(
                    "router relay: OpenCode response status is unavailable; delivery is uncertain"
                ),
            }
        } else if let Err(error) = result {
            // Only an initial connect failure with ECONNREFUSED proves that
            // nothing accepted the POST. A fresh handle has no pooled socket.
            if error.is_couldnt_connect() && request.os_errno().ok() == Some(libc::ECONNREFUSED) {
                println!("router relay: OpenCode connection refused; messages stay waiting");
                return Outcome::Failed;
            }
            println!(
                "router relay: OpenCode request failed (curl code {}); delivery is uncertain",
                error.code()
            );
        } else {
            println!("router relay: OpenCode response was incomplete; delivery is uncertain");
        }
        Outcome::Uncertain
    }

    fn request(&self, text: &str, limit: Duration) -> Result<Easy2<ResponseHeaders>, curl::Error> {
        let path = format!(
            "{}/session/{}/prompt_async",
            self.url.trim_end_matches('/'),
            self.session
        );
        let body = json!({"parts": [{"type": "text", "text": text}]}).to_string();
        let mut headers = List::new();
        headers.append("Content-Type: application/json")?;
        headers.append("Expect:")?;

        let mut request = Easy2::new(ResponseHeaders::default());
        request.url(&path)?;
        request.proxy("")?;
        request.netrc(NetRc::Ignored)?;
        request.follow_location(false)?;
        request.http_version(HttpVersion::V11)?;
        request.timeout(limit.max(Duration::from_millis(1)))?;
        request.http_headers(headers)?;
        request.post(true)?;
        request.post_fields_copy(body.as_bytes())?;
        Ok(request)
    }
}
