//! Authentication endpoints and the admin token (ARCHITECTURE.md §8, docs/ACCOUNTS.md §2).
//!
//! Admin endpoints accept `Authorization: Bearer <token>` or the `rms_token` cookie set
//! by `POST /api/v1/auth/login {token}`. The token comes from `server.admin_token`
//! (or `RMS_ADMIN_TOKEN`); when empty, it is read from `data/admin_token`, or generated
//! at first start, stored there (mode 0600) and printed in the log.
//!
//! The login also accepts a user's API key (phase 8A): it then sets the `rms_key`
//! cookie. Login and check answer `200 {role: "admin"}` or `200 {role: "user", user:
//! {id, username, display_name}, scopes}`; the request middlewares live in
//! [`super::principal`].

use std::path::Path;
use std::time::Duration;

use anyhow::Context;
use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::principal::{KEY_COOKIE, Principal, Resolved, UserPrincipal, resolve, resolve_key};
use super::{ApiJson, BaseUrl};
use crate::config::Config;
use crate::db::{api_keys, users};
use crate::error::ApiError;
use crate::state::AppState;

pub const COOKIE_NAME: &str = "rms_token";
/// Login cookie lifetime (30 days).
const COOKIE_MAX_AGE_SECS: u64 = 30 * 24 * 3600;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/auth/login", post(login))
        .route("/auth/logout", post(logout))
        .route("/auth/check", get(check))
}

/// Where the effective admin token came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenSource {
    /// `server.admin_token` / `RMS_ADMIN_TOKEN`.
    Config,
    /// Existing `data/admin_token` file.
    File,
    /// Freshly generated and written to `data/admin_token`.
    Generated,
}

/// Resolve the admin token (see module docs). The data dir must exist. In cluster mode
/// the token must come from the configuration (identical on every node): the per-node
/// file is neither read nor generated.
pub fn bootstrap_admin_token(config: &Config) -> anyhow::Result<(String, TokenSource)> {
    if !config.server.admin_token.is_empty() {
        return Ok((config.server.admin_token.clone(), TokenSource::Config));
    }
    if config.cluster.enabled {
        anyhow::bail!(
            "cluster mode requires server.admin_token (RMS_ADMIN_TOKEN), identical on every node"
        );
    }
    let path = config.admin_token_path();
    if path.exists() {
        let token = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?
            .trim()
            .to_string();
        if !token.is_empty() {
            return Ok((token, TokenSource::File));
        }
    }
    let bytes: [u8; 24] = rand::random();
    let token = hex::encode(bytes);
    write_secret_file(&path, &token).with_context(|| format!("writing {}", path.display()))?;
    Ok((token, TokenSource::Generated))
}

fn write_secret_file(path: &Path, content: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path)?;
    f.write_all(content.as_bytes())?;
    f.write_all(b"\n")?;
    Ok(())
}

/// Constant-time comparison (both sides hashed first so lengths do not leak).
pub fn token_matches(expected: &str, provided: &str) -> bool {
    if expected.is_empty() {
        return false;
    }
    let a = Sha256::digest(expected.as_bytes());
    let b = Sha256::digest(provided.as_bytes());
    a.iter()
        .zip(b.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

/// Token from `Authorization: Bearer ...`, else from the `rms_token` cookie.
pub fn extract_token(headers: &HeaderMap) -> Option<String> {
    if let Some(v) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    {
        let v = v.trim();
        if let Some(t) = v
            .strip_prefix("Bearer ")
            .or_else(|| v.strip_prefix("bearer "))
        {
            let t = t.trim();
            if !t.is_empty() {
                return Some(t.to_string());
            }
        }
    }
    for value in headers.get_all(header::COOKIE) {
        let Ok(s) = value.to_str() else { continue };
        for pair in s.split(';') {
            if let Some((k, v)) = pair.trim().split_once('=')
                && k.trim() == COOKIE_NAME
                && !v.trim().is_empty()
            {
                return Some(v.trim().to_string());
            }
        }
    }
    None
}

#[derive(Debug, Deserialize)]
pub struct LoginBody {
    pub token: String,
}

fn cookie(name: &str, value: &str, max_age: u64, secure: bool) -> HeaderValue {
    let mut c = format!("{name}={value}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age}");
    if secure {
        c.push_str("; Secure");
    }
    // Token chars are hex/url-safe; an invalid header value cannot be produced from them,
    // but never panic on unexpected input.
    HeaderValue::from_str(&c)
        .unwrap_or_else(|_| HeaderValue::from_static("rms_token=; Path=/; Max-Age=0"))
}

/// The user of a [`WhoAmI`].
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct WhoAmIUser {
    pub id: String,
    pub username: String,
    pub display_name: String,
}

/// Body of `POST /auth/login` and `GET /auth/check`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct WhoAmI {
    /// `admin` | `user`
    pub role: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<WhoAmIUser>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scopes: Option<Vec<String>>,
    /// Key id (user role).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_id: Option<String>,
}

impl WhoAmI {
    fn of(p: &Principal) -> Self {
        match p {
            Principal::Admin => Self {
                role: "admin",
                user: None,
                scopes: None,
                key_id: None,
            },
            Principal::User(u) => Self::user(u),
        }
    }

    fn user(u: &UserPrincipal) -> Self {
        Self {
            role: "user",
            user: Some(WhoAmIUser {
                id: u.user_id.clone(),
                username: u.username.clone(),
                display_name: u.display_name.clone(),
            }),
            scopes: Some(u.scopes.clone()),
            key_id: Some(u.key_id.clone()),
        }
    }
}

fn no_store(mut resp: Response) -> Response {
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

/// `POST /api/v1/auth/login {token}`: the admin token → `200 {role:"admin"}` +
/// `rms_token` cookie; an API key → `200 {role:"user", user, scopes}` + `rms_key` cookie
/// (until the key expires, at most 30 days). The other cookie is cleared. 401 (after a
/// 400 ms delay) when wrong, 403 for a suspended user.
async fn login(
    State(state): State<AppState>,
    headers: HeaderMap,
    ApiJson(body): ApiJson<LoginBody>,
) -> Result<Response, ApiError> {
    let token = body.token.trim();
    let secure = BaseUrl::from_request(&state.config, &headers).https;
    if token_matches(&state.admin_token, token) {
        if token
            .bytes()
            .any(|b| !(b.is_ascii_graphic()) || b == b';' || b == b',')
        {
            return Err(ApiError::bad_request(
                "token contains characters unsuitable for a cookie; use the Authorization header",
            ));
        }
        let mut resp = no_store(Json(WhoAmI::of(&Principal::Admin)).into_response());
        let h = resp.headers_mut();
        h.append(
            header::SET_COOKIE,
            cookie(COOKIE_NAME, token, COOKIE_MAX_AGE_SECS, secure),
        );
        h.append(header::SET_COOKIE, cookie(KEY_COOKIE, "", 0, secure));
        return Ok(resp);
    }
    if api_keys::looks_like_key(token) {
        match resolve_key(&state, token).await {
            Resolved::Principal(Principal::User(user)) => {
                let max_age = match api_keys::get(&state.db, &user.user_id, &user.key_id).await {
                    Ok(Some(k)) => k
                        .expires_at
                        .map(|t| (t - chrono::Utc::now()).num_seconds().max(0) as u64)
                        .unwrap_or(COOKIE_MAX_AGE_SECS)
                        .min(COOKIE_MAX_AGE_SECS),
                    _ => COOKIE_MAX_AGE_SECS,
                };
                if let Err(e) = users::touch_login(&state.db, &user.user_id).await {
                    tracing::warn!(error = %e, "cannot store the login time");
                }
                tracing::info!(user = %user.username, "user login");
                let mut resp = no_store(Json(WhoAmI::user(&user)).into_response());
                let h = resp.headers_mut();
                h.append(
                    header::SET_COOKIE,
                    cookie(KEY_COOKIE, token, max_age, secure),
                );
                h.append(header::SET_COOKIE, cookie(COOKIE_NAME, "", 0, secure));
                return Ok(resp);
            }
            Resolved::Suspended => return Err(ApiError::forbidden("the user is suspended")),
            Resolved::Error(e) => return Err(e),
            _ => {}
        }
    }
    // Slow down brute force attempts a little.
    tokio::time::sleep(Duration::from_millis(400)).await;
    tracing::warn!("failed login attempt");
    Err(ApiError::Unauthorized)
}

/// `POST /api/v1/auth/logout` → 204 and both cookies cleared.
async fn logout(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let secure = BaseUrl::from_request(&state.config, &headers).https;
    let mut resp = StatusCode::NO_CONTENT.into_response();
    let h = resp.headers_mut();
    h.append(header::SET_COOKIE, cookie(COOKIE_NAME, "", 0, secure));
    h.append(header::SET_COOKIE, cookie(KEY_COOKIE, "", 0, secure));
    resp
}

/// `GET /api/v1/auth/check` → `200 WhoAmI` when authenticated (header or cookie), 401
/// otherwise, 403 for a suspended user.
async fn check(State(state): State<AppState>, headers: HeaderMap) -> Response {
    match resolve(&state, &headers).await.into_result() {
        Ok(p) => no_store(Json(WhoAmI::of(&p)).into_response()),
        Err(e) => e.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_compare() {
        assert!(token_matches("abc", "abc"));
        assert!(!token_matches("abc", "abd"));
        assert!(!token_matches("abc", "abcd"));
        assert!(!token_matches("", ""));
    }

    #[test]
    fn token_extraction() {
        let mut h = HeaderMap::new();
        assert_eq!(extract_token(&h), None);
        h.insert(
            header::COOKIE,
            HeaderValue::from_static("a=1; rms_token=tok123; b=2"),
        );
        assert_eq!(extract_token(&h).as_deref(), Some("tok123"));
        h.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer bearer-tok"),
        );
        assert_eq!(extract_token(&h).as_deref(), Some("bearer-tok"));
    }

    #[test]
    fn bootstrap_generates_and_reuses() {
        let dir = std::env::temp_dir().join(format!("rms-auth-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut cfg = Config::default();
        cfg.server.data_dir = dir.clone();
        let (t1, s1) = bootstrap_admin_token(&cfg).unwrap();
        assert_eq!(s1, TokenSource::Generated);
        assert_eq!(t1.len(), 48);
        let (t2, s2) = bootstrap_admin_token(&cfg).unwrap();
        assert_eq!((t2.as_str(), s2), (t1.as_str(), TokenSource::File));
        cfg.server.admin_token = "explicit".into();
        assert_eq!(
            bootstrap_admin_token(&cfg).unwrap(),
            ("explicit".into(), TokenSource::Config)
        );
        // Cluster mode never falls back to the per-node file.
        let mut cluster = cfg.clone();
        cluster.cluster.enabled = true;
        cluster.server.admin_token.clear();
        assert!(bootstrap_admin_token(&cluster).is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(cfg.admin_token_path())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}
