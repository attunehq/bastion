//! The GitHub REST boundary, behind an injectable seam.
//!
//! The adapter talks to GitHub over a tiny slice of the REST API: list, create,
//! and update issue comments, and create check runs. To keep the reporting logic
//! testable without a live GitHub, the actual HTTP lives behind [`GitHubApi`].
//! Production wires it to [`RestClient`] (a real `reqwest` client); tests drive a
//! recording double or a local fake server through the same interface, exactly as
//! the backend boundary drives a fake agent through [`crate::backend::command`].
//!
//! Following that same pattern, [`ApiRequest`] is the parsed, proof-carrying form
//! the reporting layer builds (method, path, JSON body), so the request shapes are
//! unit-testable as plain data and the client only has to send them.

use color_eyre::eyre::{Context, Result, bail, eyre};
use serde::Deserialize;

/// The REST verbs the adapter uses. Deliberately just the three Bastion needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// Read (listing comments).
    Get,
    /// Create (a comment or a check run).
    Post,
    /// Update (an existing comment in place).
    Patch,
}

impl Method {
    /// The HTTP method name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Post => "POST",
            Method::Patch => "PATCH",
        }
    }
}

/// A fully-resolved GitHub REST call: the proof-carrying form the reporting layer
/// hands to a [`GitHubApi`].
///
/// Mirrors [`crate::backend::command::CommandSpec`]: the method, the API-relative
/// path, and the JSON body are all resolved here, so the client only sends it and
/// every request shape can be asserted as plain data in a test.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiRequest {
    /// The HTTP method.
    pub method: Method,
    /// Path beginning with `/`, relative to the API base
    /// (e.g. `/repos/acme/app/check-runs`).
    pub path: String,
    /// JSON body for `POST`/`PATCH`; `None` for `GET`.
    pub body: Option<serde_json::Value>,
}

impl ApiRequest {
    /// A `GET` with no body.
    pub fn get(path: impl Into<String>) -> Self {
        Self {
            method: Method::Get,
            path: path.into(),
            body: None,
        }
    }

    /// A `POST` carrying a JSON body.
    pub fn post(path: impl Into<String>, body: serde_json::Value) -> Self {
        Self {
            method: Method::Post,
            path: path.into(),
            body: Some(body),
        }
    }

    /// A `PATCH` carrying a JSON body.
    pub fn patch(path: impl Into<String>, body: serde_json::Value) -> Self {
        Self {
            method: Method::Patch,
            path: path.into(),
            body: Some(body),
        }
    }
}

/// A captured GitHub REST response: the status and the parsed JSON body.
#[derive(Debug, Clone)]
pub struct ApiResponse {
    /// The HTTP status code.
    pub status: u16,
    /// The parsed JSON body, or [`serde_json::Value::Null`] for an empty body.
    pub body: serde_json::Value,
}

impl ApiResponse {
    /// Whether the status is in the 2xx success range.
    #[must_use]
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// GitHub's error `message` field, when the body carries one. Used to build a
    /// legible error for a non-2xx response.
    #[must_use]
    pub fn error_message(&self) -> Option<&str> {
        self.body.get("message").and_then(serde_json::Value::as_str)
    }
}

/// One issue comment, parsed from a list response: enough to find Bastion's own
/// sticky comment and address an update to it.
#[derive(Debug, Clone, Deserialize)]
pub struct IssueComment {
    /// The comment id (the address for an in-place update).
    pub id: u64,
    /// The comment body (scanned for Bastion's hidden marker).
    #[serde(default)]
    pub body: String,
}

/// The seam over GitHub REST calls: send a resolved [`ApiRequest`], capture the
/// [`ApiResponse`].
///
/// Production wires this to [`RestClient`]; tests drive a recording double or a
/// local fake server, so the reporting layer never special-cases being under test.
#[allow(
    async_fn_in_trait,
    reason = "single-crate trait consumed internally, not across a public API boundary"
)]
pub trait GitHubApi: Send + Sync {
    /// Send the request and capture its status and JSON body.
    ///
    /// # Errors
    ///
    /// Returns an error only if the request could not be sent or its response could
    /// not be read. A non-2xx status is *not* an error here: it is reported via
    /// [`ApiResponse::status`] so the caller decides what it means.
    async fn send(&self, req: &ApiRequest) -> Result<ApiResponse>;
}

/// Send a request and treat any non-2xx status as an error, surfacing GitHub's own
/// message. The fail-closed posture: a call GitHub rejected is a real failure, not
/// something to swallow. Both halves of the adapter route through this: reporting
/// (`super::report::post`) sends writes, gathering (`super::context`) and signing
/// (`super::signing`) send reads, so the status check lives once.
///
/// # Errors
///
/// Returns an error if the request could not be sent or its status was not 2xx.
pub(super) async fn send_checked<A: GitHubApi + ?Sized>(
    api: &A,
    req: &ApiRequest,
) -> Result<ApiResponse> {
    let resp = api.send(req).await?;
    if !resp.is_success() {
        bail!(
            "GitHub {} {} returned {}: {}",
            req.method.as_str(),
            req.path,
            resp.status,
            resp.error_message().unwrap_or("(no message)"),
        );
    }
    Ok(resp)
}

/// The GitHub REST base URL in Actions, exposed as `GITHUB_API_URL`
/// (`https://api.github.com` on github.com, a host-specific URL on GHES). Tests
/// point it at a local fake server.
pub const API_URL_ENV: &str = "GITHUB_API_URL";

/// The token Actions exposes to the workflow, read by the report command.
pub const TOKEN_ENV: &str = "GITHUB_TOKEN";

/// The default API base used when [`API_URL_ENV`] is unset.
pub const DEFAULT_API_URL: &str = "https://api.github.com";

/// A [`GitHubApi`] backed by a real `reqwest` client.
#[derive(Debug, Clone)]
pub struct RestClient {
    http: reqwest::Client,
    base_url: String,
    token: String,
}

impl RestClient {
    /// Build a client against `base_url`, authenticating with `token`.
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying HTTP client cannot be constructed.
    pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .build()
            .wrap_err("building the GitHub HTTP client")?;
        Ok(Self {
            http,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            token: token.into(),
        })
    }

    /// Build a client from the Actions environment: `GITHUB_API_URL` (falling back
    /// to [`DEFAULT_API_URL`]) and `GITHUB_TOKEN`.
    ///
    /// # Errors
    ///
    /// Returns an error if `GITHUB_TOKEN` is unset or empty, or if the HTTP client
    /// cannot be built. A missing token fails closed rather than posting anonymously.
    pub fn from_env() -> Result<Self> {
        let base_url = std::env::var(API_URL_ENV)
            .ok()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| DEFAULT_API_URL.to_string());
        let token = std::env::var(TOKEN_ENV)
            .ok()
            .filter(|v| !v.is_empty())
            .ok_or_else(|| {
                eyre!("{TOKEN_ENV} is unset or empty; the report needs a token with pull-requests and checks write access")
            })?;
        Self::new(base_url, token)
    }
}

impl GitHubApi for RestClient {
    async fn send(&self, req: &ApiRequest) -> Result<ApiResponse> {
        let url = format!("{}{}", self.base_url, req.path);
        let method = match req.method {
            Method::Get => reqwest::Method::GET,
            Method::Post => reqwest::Method::POST,
            Method::Patch => reqwest::Method::PATCH,
        };
        let mut builder = self
            .http
            .request(method, &url)
            // GitHub requires a User-Agent and the explicit API version; the Accept
            // header selects the stable v3 JSON media type.
            .header(reqwest::header::USER_AGENT, "bastion")
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .bearer_auth(&self.token);
        if let Some(body) = &req.body {
            builder = builder.json(body);
        }

        let response = builder
            .send()
            .await
            .wrap_err_with(|| format!("sending {} {}", req.method.as_str(), req.path))?;
        let status = response.status().as_u16();
        let text = response.text().await.wrap_err_with(|| {
            format!(
                "reading the response to {} {}",
                req.method.as_str(),
                req.path
            )
        })?;
        let body = if text.trim().is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_str(&text).unwrap_or(serde_json::Value::Null)
        };
        Ok(ApiResponse { status, body })
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    //! A recording [`GitHubApi`] double and a minimal in-process GitHub server, so
    //! both the reporting orchestration and the real `reqwest` client are exercised
    //! against real fixtures rather than mocked collaborators.

    use std::sync::Mutex;

    use super::{ApiRequest, ApiResponse, GitHubApi};
    use color_eyre::eyre::Result;

    /// A [`GitHubApi`] that records every request and replays canned responses.
    ///
    /// `responder` maps a request to the response to return; the default is an
    /// empty `200`. Recorded requests are available via [`RecordingClient::calls`].
    pub struct RecordingClient {
        calls: Mutex<Vec<ApiRequest>>,
        responder: Box<dyn Fn(&ApiRequest) -> ApiResponse + Send + Sync>,
    }

    impl RecordingClient {
        /// A client whose responses are produced by `responder`.
        pub fn with_responder(
            responder: impl Fn(&ApiRequest) -> ApiResponse + Send + Sync + 'static,
        ) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                responder: Box::new(responder),
            }
        }

        /// The requests sent so far, in order.
        pub fn calls(&self) -> Vec<ApiRequest> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl GitHubApi for RecordingClient {
        async fn send(&self, req: &ApiRequest) -> Result<ApiResponse> {
            self.calls.lock().unwrap().push(req.clone());
            Ok((self.responder)(req))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    #[test]
    fn api_request_constructors_set_method_and_body() {
        let get = ApiRequest::get("/repos/o/r/issues/1/comments");
        assert_eq!(get.method, Method::Get);
        assert!(get.body.is_none());

        let post = ApiRequest::post("/repos/o/r/check-runs", serde_json::json!({"name": "x"}));
        assert_eq!(post.method, Method::Post);
        assert_eq!(post.body.unwrap()["name"], "x");

        let patch = ApiRequest::patch(
            "/repos/o/r/issues/comments/9",
            serde_json::json!({"body": "y"}),
        );
        assert_eq!(patch.method, Method::Patch);
    }

    #[test]
    fn response_helpers_read_success_and_error_message() {
        let ok = ApiResponse {
            status: 201,
            body: serde_json::Value::Null,
        };
        assert!(ok.is_success());
        assert!(ok.error_message().is_none());

        let err = ApiResponse {
            status: 422,
            body: serde_json::json!({"message": "Validation Failed"}),
        };
        assert!(!err.is_success());
        assert_eq!(err.error_message(), Some("Validation Failed"));
    }

    /// Drive [`RestClient`] against a one-shot, in-process HTTP server so the real
    /// `reqwest` path (method, URL, headers, JSON body, response parsing) is
    /// exercised end to end with no network and no mock of the client itself.
    #[tokio::test]
    async fn rest_client_sends_a_real_request_and_parses_the_response() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        // The fake GitHub: accept one connection, capture the raw request, and reply
        // with a small JSON object. Runs on a blocking thread so the async client
        // can talk to it within the same test.
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let n = stream.read(&mut buf).unwrap();
            let request = String::from_utf8_lossy(&buf[..n]).into_owned();
            let body = br#"{"id":99,"message":"ok"}"#;
            let response = format!(
                "HTTP/1.1 201 Created\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
            stream.write_all(body).unwrap();
            stream.flush().unwrap();
            request
        });

        let client = RestClient::new(format!("http://{addr}"), "secret-token").unwrap();
        let resp = client
            .send(&ApiRequest::post(
                "/repos/acme/app/check-runs",
                serde_json::json!({"name": "bastion / demo"}),
            ))
            .await
            .expect("request sends");

        assert_eq!(resp.status, 201);
        assert_eq!(resp.body["id"], 99);

        let raw = server.join().unwrap();
        // The request line carries the method and the API-relative path...
        assert!(
            raw.starts_with("POST /repos/acme/app/check-runs HTTP/1.1"),
            "request was: {raw}"
        );
        // ...the required GitHub headers are present (hyper lowercases header names)...
        let lower = raw.to_ascii_lowercase();
        assert!(lower.contains("authorization: bearer secret-token"));
        assert!(lower.contains("user-agent: bastion"));
        assert!(lower.contains("accept: application/vnd.github+json"));
        assert!(lower.contains("x-github-api-version: 2022-11-28"));
        // ...and the JSON body is serialized.
        assert!(
            raw.contains(r#""name":"bastion / demo""#),
            "body missing from: {raw}"
        );
    }
}
