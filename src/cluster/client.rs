//! HTTP client of the internal routes of the other nodes (`/internal/*` on their
//! `cluster.advertise_url`, `Authorization: Bearer <cluster.internal_secret>`).
//!
//! One shared `reqwest` client (rustls with the ring provider, no proxy, connection
//! pooling). Every call has a bound: [`CONNECT_TIMEOUT`] to connect, [`CALL_TIMEOUT`]
//! for the small JSON calls; the relay stream uses [`READ_TIMEOUT`] between reads (an
//! owner that stops sending for that long is considered gone).

use std::sync::OnceLock;
use std::time::Duration;

use axum::http::{HeaderMap, StatusCode};

use super::RemoteStreamStatus;

pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
pub const CALL_TIMEOUT: Duration = Duration::from_secs(3);
/// Silence on the relay stream after which the owner is considered gone.
pub const READ_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("internal request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("internal request answered {0}")]
    Status(StatusCode),
    #[error("cannot build the internal HTTP client: {0}")]
    Build(String),
    #[error("invalid internal response: {0}")]
    Decode(String),
}

/// Install the process-wide rustls crypto provider (ring) once; reqwest is built
/// without a default provider (no aws-lc-rs).
pub fn install_crypto_provider() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

pub struct InternalClient {
    secret: String,
    http: OnceLock<Result<reqwest::Client, String>>,
}

impl InternalClient {
    pub fn new(secret: &str) -> Self {
        Self {
            secret: secret.to_string(),
            http: OnceLock::new(),
        }
    }

    fn http(&self) -> Result<&reqwest::Client, ClientError> {
        self.http
            .get_or_init(|| {
                install_crypto_provider();
                reqwest::Client::builder()
                    .no_proxy()
                    .connect_timeout(CONNECT_TIMEOUT)
                    .read_timeout(READ_TIMEOUT)
                    .tcp_nodelay(true)
                    .pool_idle_timeout(Duration::from_secs(30))
                    .user_agent(concat!("rms-cluster/", env!("CARGO_PKG_VERSION")))
                    .build()
                    .map_err(|e| e.to_string())
            })
            .as_ref()
            .map_err(|e| ClientError::Build(e.clone()))
    }

    fn url(base: &str, path: &str) -> String {
        format!("{}{path}", base.trim_end_matches('/'))
    }

    fn get(&self, base: &str, path: &str) -> Result<reqwest::RequestBuilder, ClientError> {
        Ok(self
            .http()?
            .get(Self::url(base, path))
            .bearer_auth(&self.secret))
    }

    fn post(&self, base: &str, path: &str) -> Result<reqwest::RequestBuilder, ClientError> {
        Ok(self
            .http()?
            .post(Self::url(base, path))
            .bearer_auth(&self.secret))
    }

    /// `POST /internal/kick/{name}`: `true` when the owner kicked its publisher, `false`
    /// when it has none (404).
    pub async fn kick(&self, base: &str, name: &str) -> Result<bool, ClientError> {
        let resp = self
            .post(base, &format!("/internal/kick/{name}"))?
            .timeout(CALL_TIMEOUT)
            .send()
            .await?;
        match resp.status() {
            s if s.is_success() => Ok(true),
            StatusCode::NOT_FOUND => Ok(false),
            s => Err(ClientError::Status(s)),
        }
    }

    /// `GET /internal/status/{name}`: `None` when the node does not publish it (404).
    pub async fn status(
        &self,
        base: &str,
        name: &str,
    ) -> Result<Option<RemoteStreamStatus>, ClientError> {
        let resp = self
            .get(base, &format!("/internal/status/{name}"))?
            .timeout(CALL_TIMEOUT)
            .send()
            .await?;
        match resp.status() {
            StatusCode::OK => {
                let body = resp.bytes().await?;
                serde_json::from_slice(&body)
                    .map(Some)
                    .map_err(|e| ClientError::Decode(e.to_string()))
            }
            StatusCode::NOT_FOUND => Ok(None),
            s => Err(ClientError::Status(s)),
        }
    }

    /// `GET /internal/relay/{name}.flv`: the streaming response (any status; the
    /// caller checks it). No total timeout, only [`READ_TIMEOUT`] between reads.
    pub async fn relay(&self, base: &str, name: &str) -> Result<reqwest::Response, ClientError> {
        Ok(self
            .get(base, &format!("/internal/relay/{name}.flv"))?
            .send()
            .await?)
    }

    /// `POST /internal/whep/delete/{session}?name=…`: the owner node's status
    /// (200 closed, 404 unknown).
    pub async fn whep_delete(
        &self,
        base: &str,
        name: &str,
        session: &str,
    ) -> Result<StatusCode, ClientError> {
        let resp = self
            .post(
                base,
                &format!(
                    "/internal/whep/delete/{}?name={}",
                    percent_encode(session),
                    percent_encode(name)
                ),
            )?
            .timeout(CALL_TIMEOUT)
            .send()
            .await?;
        Ok(resp.status())
    }

    /// `GET /internal/files/{kind}/{path}` with the client's `Range`/conditional headers
    /// forwarded; the streaming response.
    pub async fn file(
        &self,
        base: &str,
        kind: &str,
        rel_path: &str,
        headers: &HeaderMap,
    ) -> Result<reqwest::Response, ClientError> {
        let encoded: Vec<String> = rel_path
            .split('/')
            .filter(|s| !s.is_empty())
            .map(percent_encode)
            .collect();
        let mut req = self.get(
            base,
            &format!("/internal/files/{kind}/{}", encoded.join("/")),
        )?;
        for h in [
            axum::http::header::RANGE,
            axum::http::header::IF_RANGE,
            axum::http::header::IF_NONE_MATCH,
            axum::http::header::IF_MODIFIED_SINCE,
        ] {
            if let Some(v) = headers.get(&h) {
                req = req.header(h, v);
            }
        }
        Ok(req.send().await?)
    }
}

fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_and_encoding() {
        assert_eq!(
            InternalClient::url("http://n1:8081/", "/internal/ping"),
            "http://n1:8081/internal/ping"
        );
        assert_eq!(percent_encode("a b+ü.mp4"), "a%20b%2B%C3%BC.mp4");
        let c = InternalClient::new(&"x".repeat(32));
        assert!(c.http().is_ok(), "client builds with the ring provider");
    }
}
