//! Lightweight Mixpanel HTTP tracking client.
//!
//! This is a minimal replacement for `mixpanel-rs` that uses `reqwest 0.12`
//! instead of `reqwest 0.11`, avoiding a duplicate HTTP stack in the binary.
//!
//! Privacy build: `track` / `engage` are hard no-ops — product analytics
//! never leave the host. The client API surface is kept so construction
//! sites compile, but nothing is transmitted.

#![deny(clippy::indexing_slicing)]

use base64::Engine;
use std::collections::HashMap;

/// Mixpanel client for sending track events.
#[derive(Clone)]
pub struct Mixpanel {
    token: String,
    client: reqwest::Client,
    base_url: String,
}

/// Error type for Mixpanel operations.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("JSON serialization failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("mixpanel rejected the request: {0}")]
    Rejected(String),
}

const API_BASE_URL: &str = "https://api.mixpanel.com";
/// Retained for a future re-enable; the privacy no-op path never transmits.
#[allow(dead_code)]
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

impl Mixpanel {
    /// Create a new Mixpanel client with the given project token.
    #[allow(clippy::disallowed_methods)] // transport-neutral crate; the grok CLI injects a policy client via with_client
    pub fn new(token: impl Into<String>) -> Self {
        Self {
            token: token.into(),
            client: reqwest::Client::new(),
            base_url: API_BASE_URL.to_owned(),
        }
    }

    /// Create a new Mixpanel client with a shared reqwest client.
    pub fn with_client(token: impl Into<String>, client: reqwest::Client) -> Self {
        Self {
            token: token.into(),
            client,
            base_url: API_BASE_URL.to_owned(),
        }
    }

    #[cfg(test)]
    fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Scrub property string values in place, then inject the project
    /// token. Split out from [`Self::track`] so the scrub-then-inject
    /// ordering is testable.
    ///
    /// Not called by [`Self::track`] in the privacy build (track is a hard
    /// no-op), but kept + unit-tested so a future re-enable cannot drop
    /// scrub order.
    #[allow(dead_code)] // intentionally unused by privacy no-op track path
    fn prepare_properties(
        &self,
        mut properties: HashMap<String, serde_json::Value>,
    ) -> HashMap<String, serde_json::Value> {
        for v in properties.values_mut() {
            xai_grok_secrets::redact_json_string_values(v);
        }
        properties.insert("token".to_owned(), serde_json::json!(self.token));
        properties
    }

    /// Track an event. Properties should include `distinct_id`. The
    /// project `token` is injected after scrubbing, so it isn't redacted.
    ///
    /// **Privacy build:** always a no-op — product analytics never leave
    /// the host.
    pub async fn track(
        &self,
        _event: &str,
        _properties: Option<HashMap<String, serde_json::Value>>,
    ) -> Result<(), Error> {
        let _ = (&self.token, &self.client);
        Ok(())
    }

    /// Create or update a user profile via Mixpanel's Engage API.
    /// String values in `set` are scrubbed for secrets before sending.
    /// The project `token` is injected automatically.
    ///
    /// **Privacy build:** always a no-op.
    pub async fn engage(
        &self,
        _distinct_id: &str,
        _set: HashMap<String, serde_json::Value>,
    ) -> Result<(), Error> {
        let _ = (&self.token, &self.client);
        Ok(())
    }

    /// Mixpanel answers a rejected payload with HTTP 200; `verbose=1` makes the
    /// body `{"status": 0|1, "error": ...}` instead of a bare `0`/`1`.
    #[allow(dead_code)] // privacy no-op track/engage never call this; tests exercise it directly
    async fn post(&self, path: &str, payload: &serde_json::Value) -> Result<(), Error> {
        let json_bytes = serde_json::to_vec(payload)?;
        let encoded = base64::engine::general_purpose::STANDARD.encode(&json_bytes);

        let body = self
            .client
            .post(format!("{}{path}", self.base_url))
            .query(&[("verbose", "1")])
            .timeout(REQUEST_TIMEOUT)
            .form(&[("data", &encoded)])
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;

        check_reply(&body)
    }
}

fn check_reply(body: &str) -> Result<(), Error> {
    let reply: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
    if reply.get("status").and_then(serde_json::Value::as_i64) == Some(1) {
        return Ok(());
    }
    let reason = reply
        .get("error")
        .and_then(serde_json::Value::as_str)
        .unwrap_or(body);
    Err(Error::Rejected(reason.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_string_contains, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const ACCEPTED: &str = r#"{"error":null,"status":1}"#;
    const REJECTED: &str = r#"{"error":"token, missing or empty","status":0}"#;

    async fn mock(route: &str, response: ResponseTemplate) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(route))
            .and(query_param("verbose", "1"))
            .and(body_string_contains("data="))
            .respond_with(response)
            .mount(&server)
            .await;
        server
    }

    fn client(server: &MockServer) -> Mixpanel {
        Mixpanel::new("test-token").with_base_url(server.uri())
    }

    /// Privacy build: `track` / `engage` are hard no-ops — nothing reaches
    /// the wire even when a live mock server is configured.
    #[tokio::test]
    async fn track_and_engage_never_transmit_under_privacy() {
        let server = MockServer::start().await;
        let mp = client(&server);
        mp.track("e", None).await.unwrap();
        mp.engage("u", HashMap::new()).await.unwrap();
        let received = server.received_requests().await.unwrap_or_default();
        assert!(
            received.is_empty(),
            "privacy build transmitted: {received:?}"
        );
    }

    // Transport-layer assertions below exercise `post` directly: the privacy
    // no-op path never calls it, but it is retained for a future re-enable.

    fn track_payload() -> serde_json::Value {
        serde_json::json!([{
            "event": "e",
            "properties": {"distinct_id": "u"},
        }])
    }

    fn engage_payload() -> serde_json::Value {
        serde_json::json!([{
            "$token": "test-token",
            "$distinct_id": "u",
            "$set": {},
        }])
    }

    #[tokio::test]
    async fn post_track_ok_on_status_1() {
        let server = mock(
            "/track",
            ResponseTemplate::new(200).set_body_string(ACCEPTED),
        )
        .await;
        client(&server)
            .post("/track", &track_payload())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn post_track_rejected_on_status_0() {
        let server = mock(
            "/track",
            ResponseTemplate::new(200).set_body_string(REJECTED),
        )
        .await;
        let err = client(&server)
            .post("/track", &track_payload())
            .await
            .unwrap_err();
        assert!(
            matches!(err, Error::Rejected(ref reason) if reason == "token, missing or empty"),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn post_track_http_error_on_401() {
        let server = mock("/track", ResponseTemplate::new(401)).await;
        let err = client(&server)
            .post("/track", &track_payload())
            .await
            .unwrap_err();
        assert!(
            matches!(err, Error::Http(ref e) if e.status() == Some(reqwest::StatusCode::UNAUTHORIZED)),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn post_track_http_error_on_transport_failure() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let closed = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let err = Mixpanel::new("test-token")
            .with_base_url(closed)
            .post("/track", &track_payload())
            .await
            .unwrap_err();
        assert!(
            matches!(err, Error::Http(ref e) if e.is_connect()),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn post_engage_rejected_on_status_0() {
        let server = mock(
            "/engage",
            ResponseTemplate::new(200).set_body_string(REJECTED),
        )
        .await;
        let err = client(&server)
            .post("/engage", &engage_payload())
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Rejected(_)), "{err:?}");
    }

    /// Project token is deliberately Bearer-shaped: it would be redacted if `prepare_properties` ran the scrubber after token
    /// injection. The `error` value catches the inverse regression: if the scrub loop is dropped, the user-supplied Bearer
    /// leaks.
    #[test]
    fn prepare_properties_scrubs_then_injects_token() {
        let project_token = "Bearer fake-project-token-abcdef0123456789";
        let mp = Mixpanel::new(project_token);

        let mut props = HashMap::new();
        props.insert("error".into(), "Bearer abcdef0123456789abcdef".into());

        let prepared = mp.prepare_properties(props);

        assert_eq!(
            prepared.get("token"),
            Some(&serde_json::json!(project_token)),
            "project token redacted"
        );
        let Some(error) = prepared.get("error").and_then(|v| v.as_str()) else {
            panic!("missing json key error: {prepared:?}");
        };
        assert!(
            !error.contains("abcdef0123456789abcdef"),
            "secret leaked: {error}"
        );
    }
}
