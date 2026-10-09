//! Principals (docs/ACCOUNTS.md §2): who is calling `/api/v1/*` and `/storage/*`.
//!
//! Credentials are read once per request, in this order:
//! 1. `Authorization: Bearer <admin token | API key>`: when present it is the only
//!    credential considered (a wrong bearer is a 401 even with a valid cookie);
//! 2. `AccessKey: <API key>` (bunny-style header);
//! 3. the cookie `rms_token` (admin, set by the dashboard login), else `rms_key` (a
//!    user's API key, set by a login with a key).
//!
//! The admin token is compared in constant time; keys are looked up by their SHA-256.
//! A revoked, expired or unknown key is a 401; every key of a suspended user a 403.
//! `last_used_at` is refreshed at most once a minute (in the background).
//!
//! [`require_admin`] guards the admin API (a user key gets 403), [`require_principal`]
//! the account and storage routes (the [`Principal`] is put in the request extensions).
//! [`Principal::access`] evaluates the path permissions of a bucket.

use axum::extract::{Request, State};
use axum::http::{HeaderMap, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use chrono::Utc;

use super::auth::{COOKIE_NAME, token_matches};
use crate::db::api_keys::{
    self, Access, ApiKey, KeyOwner, Permission, SCOPE_ACCOUNT, SCOPE_STORAGE,
};
use crate::db::buckets::Bucket;
use crate::db::users;
use crate::error::ApiError;
use crate::state::AppState;

/// Cookie holding a user's API key after a dashboard login.
pub const KEY_COOKIE: &str = "rms_key";
/// Bunny-style key header.
pub const ACCESS_KEY_HEADER: &str = "accesskey";

/// An authenticated caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Principal {
    /// The admin token: everything, every user.
    Admin,
    /// A user's API key.
    User(UserPrincipal),
}

/// A user authenticated by one of its keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserPrincipal {
    pub user_id: String,
    pub username: String,
    pub display_name: String,
    pub key_id: String,
    pub scopes: Vec<String>,
    pub permissions: Vec<Permission>,
}

impl UserPrincipal {
    fn new(key: ApiKey, owner: KeyOwner) -> Self {
        Self {
            user_id: owner.user_id,
            username: owner.username,
            display_name: owner.display_name,
            key_id: key.id,
            scopes: key.scopes,
            permissions: key.permissions,
        }
    }

    pub fn has_scope(&self, scope: &str) -> bool {
        self.scopes.iter().any(|s| s == scope)
    }

    /// Effective access on `access_path` of `bucket` (see [`Principal::access`]).
    pub fn access(&self, bucket: &Bucket, access_path: &str) -> Option<Access> {
        if bucket.user_id != self.user_id {
            return None;
        }
        if self.has_scope(SCOPE_ACCOUNT) {
            return Some(Access::Admin);
        }
        if !self.has_scope(SCOPE_STORAGE) {
            return None;
        }
        self.permissions
            .iter()
            .filter(|p| p.bucket == "*" || p.bucket == bucket.id)
            .filter(|p| access_path.starts_with(&p.prefix))
            .map(|p| p.access)
            .max()
    }
}

impl Principal {
    pub fn is_admin(&self) -> bool {
        matches!(self, Principal::Admin)
    }

    pub fn user(&self) -> Option<&UserPrincipal> {
        match self {
            Principal::User(u) => Some(u),
            Principal::Admin => None,
        }
    }

    /// Effective access level on `access_path` (a file path, or a directory path with its
    /// trailing `/`, see `storage::paths::ObjectPath::access_path`) of `bucket`:
    /// - admin: `admin` everywhere;
    /// - another user's bucket: none;
    /// - `account` scope: `admin` on every own bucket;
    /// - `storage` scope: the highest access among the permission entries whose bucket is
    ///   `*` or this bucket's id and whose prefix starts `access_path`;
    /// - otherwise none.
    pub fn access(&self, bucket: &Bucket, access_path: &str) -> Option<Access> {
        match self {
            Principal::Admin => Some(Access::Admin),
            Principal::User(u) => u.access(bucket, access_path),
        }
    }

    /// `access(...) >= need`.
    pub fn can(&self, bucket: &Bucket, access_path: &str, need: Access) -> bool {
        self.access(bucket, access_path).is_some_and(|a| a >= need)
    }
}

/// Outcome of reading the credentials of a request.
#[derive(Debug)]
pub enum Resolved {
    /// No credential at all.
    Anonymous,
    Principal(Principal),
    /// Wrong admin token, unknown/revoked/expired key.
    Invalid,
    /// A valid key of a suspended user.
    Suspended,
    /// The database could not be read.
    Error(ApiError),
}

impl Resolved {
    /// The principal, or the error response (401/403/500).
    pub fn into_result(self) -> Result<Principal, ApiError> {
        match self {
            Resolved::Principal(p) => Ok(p),
            Resolved::Anonymous | Resolved::Invalid => Err(ApiError::Unauthorized),
            Resolved::Suspended => Err(ApiError::forbidden("the user is suspended")),
            Resolved::Error(e) => Err(e),
        }
    }
}

/// Value of cookie `name`.
pub fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|s| s.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(k, v)| k.trim() == name && !v.trim().is_empty())
        .map(|(_, v)| v.trim().to_string())
}

fn bearer(headers: &HeaderMap) -> Option<String> {
    let v = headers.get(header::AUTHORIZATION)?.to_str().ok()?.trim();
    let t = v
        .strip_prefix("Bearer ")
        .or_else(|| v.strip_prefix("bearer "))?
        .trim();
    (!t.is_empty()).then(|| t.to_string())
}

fn access_key(headers: &HeaderMap) -> Option<String> {
    let v = headers.get(ACCESS_KEY_HEADER)?.to_str().ok()?.trim();
    (!v.is_empty()).then(|| v.to_string())
}

/// Resolve the credentials of a request (see the module docs for the order).
pub async fn resolve(state: &AppState, headers: &HeaderMap) -> Resolved {
    if let Some(t) = bearer(headers) {
        if token_matches(&state.admin_token, &t) {
            return Resolved::Principal(Principal::Admin);
        }
        return resolve_key(state, &t).await;
    }
    if let Some(k) = access_key(headers) {
        return resolve_key(state, &k).await;
    }
    let admin_cookie = cookie(headers, COOKIE_NAME);
    if admin_cookie
        .as_deref()
        .is_some_and(|t| token_matches(&state.admin_token, t))
    {
        return Resolved::Principal(Principal::Admin);
    }
    if let Some(k) = cookie(headers, KEY_COOKIE) {
        return resolve_key(state, &k).await;
    }
    if admin_cookie.is_some() {
        Resolved::Invalid
    } else {
        Resolved::Anonymous
    }
}

/// Look an API key up (by hash) and check it and its owner.
pub async fn resolve_key(state: &AppState, plain: &str) -> Resolved {
    if !api_keys::looks_like_key(plain) {
        return Resolved::Invalid;
    }
    let (key, owner) = match api_keys::find_by_hash(&state.db, &api_keys::hash_key(plain)).await {
        Ok(Some(found)) => found,
        Ok(None) => return Resolved::Invalid,
        Err(e) => return Resolved::Error(e.into()),
    };
    let now = Utc::now();
    if !key.is_usable(now) {
        return Resolved::Invalid;
    }
    if owner.status != users::STATUS_ACTIVE {
        return Resolved::Suspended;
    }
    let stale = key
        .last_used_at
        .is_none_or(|t| (now - t).num_seconds() >= api_keys::TOUCH_INTERVAL_SECS);
    if stale {
        let (db, id) = (state.db.clone(), key.id.clone());
        tokio::spawn(async move {
            if let Err(e) = api_keys::touch_last_used(&db, &id).await {
                tracing::debug!(error = %e, "cannot update last_used_at of an API key");
            }
        });
    }
    Resolved::Principal(Principal::User(UserPrincipal::new(key, owner)))
}

/// Middleware of the admin API: the admin token only (401 without valid credentials,
/// 403 for a user's key).
pub async fn require_admin(State(state): State<AppState>, req: Request, next: Next) -> Response {
    match resolve(&state, req.headers()).await.into_result() {
        Ok(Principal::Admin) => next.run(req).await,
        Ok(Principal::User(_)) => {
            ApiError::forbidden("this endpoint needs the admin token").into_response()
        }
        Err(e) => e.into_response(),
    }
}

/// Middleware of the account and storage routes: any valid principal, stored in the
/// request extensions.
pub async fn require_principal(
    State(state): State<AppState>,
    mut req: Request,
    next: Next,
) -> Response {
    match resolve(&state, req.headers()).await.into_result() {
        Ok(p) => {
            req.extensions_mut().insert(p);
            next.run(req).await
        }
        Err(e) => e.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::api_keys::Access::*;

    fn user(scopes: &[&str], perms: &[(&str, &str, Access)]) -> Principal {
        Principal::User(UserPrincipal {
            user_id: "u1".into(),
            username: "alice".into(),
            display_name: "Alice".into(),
            key_id: "k1".into(),
            scopes: scopes.iter().map(|s| s.to_string()).collect(),
            permissions: perms
                .iter()
                .map(|(b, p, a)| Permission {
                    bucket: b.to_string(),
                    prefix: p.to_string(),
                    access: *a,
                })
                .collect(),
        })
    }

    fn bucket(id: &str, owner: &str) -> Bucket {
        let mut b = Bucket::new(owner, id);
        b.id = id.into();
        b
    }

    #[test]
    fn prefix_semantics() {
        let p = user(
            &["storage"],
            &[("*", "/videos/", Write), ("b1", "/public/", Read)],
        );
        let b1 = bucket("b1", "u1");
        assert_eq!(p.access(&b1, "/videos/a.mp4"), Some(Write));
        assert_eq!(
            p.access(&b1, "/videos/"),
            Some(Write),
            "listing of the directory"
        );
        assert_eq!(p.access(&b1, "/videos/2026/x.mp4"), Some(Write));
        assert_eq!(
            p.access(&b1, "/videos"),
            None,
            "the bare name is not below the prefix"
        );
        assert_eq!(p.access(&b1, "/videos-old.mp4"), None);
        assert_eq!(p.access(&b1, "/"), None, "root listing");
        assert_eq!(p.access(&b1, "/other/x.txt"), None);
        assert_eq!(p.access(&b1, "/public/"), Some(Read));
        assert!(p.can(&b1, "/public/a.jpg", Read));
        assert!(!p.can(&b1, "/public/a.jpg", Write));
        // A prefix without trailing slash is a plain string prefix.
        let q = user(&["storage"], &[("*", "/vid", Read)]);
        assert_eq!(q.access(&b1, "/videos-old.mp4"), Some(Read));
        // "/" = whole bucket.
        let r = user(&["storage"], &[("b1", "/", Admin)]);
        assert_eq!(r.access(&b1, "/"), Some(Admin));
        assert_eq!(r.access(&b1, "/deep/dir/"), Some(Admin));
    }

    #[test]
    fn access_ordering_and_buckets() {
        let p = user(
            &["storage"],
            &[
                ("*", "/", Read),
                ("b1", "/videos/", Write),
                ("b1", "/videos/raw/", Admin),
            ],
        );
        let (b1, b2) = (bucket("b1", "u1"), bucket("b2", "u1"));
        assert_eq!(p.access(&b1, "/x"), Some(Read));
        assert_eq!(
            p.access(&b1, "/videos/x"),
            Some(Write),
            "highest matching entry"
        );
        assert_eq!(p.access(&b1, "/videos/raw/"), Some(Admin));
        assert_eq!(
            p.access(&b2, "/videos/x"),
            Some(Read),
            "b1 entries do not apply to b2"
        );
        assert!(p.can(&b1, "/videos/raw/a", Write), "admin implies write");
        assert!(p.can(&b1, "/videos/raw/a", Read), "admin implies read");
        // Another user's bucket: nothing, whatever the entries say.
        let foreign = bucket("b1", "u2");
        assert_eq!(p.access(&foreign, "/x"), None);
        // Without the storage scope the permissions are ignored.
        let no_scope = user(&[], &[("*", "/", Admin)]);
        assert_eq!(no_scope.access(&b1, "/x"), None);
    }

    #[test]
    fn account_scope_and_admin() {
        let acc = user(&["account"], &[]);
        let own = bucket("b1", "u1");
        assert_eq!(acc.access(&own, "/anything/"), Some(Admin));
        assert_eq!(
            acc.access(&bucket("b9", "u2"), "/"),
            None,
            "only own buckets"
        );
        let both = user(&["account", "storage"], &[("*", "/a/", Read)]);
        assert_eq!(both.access(&own, "/b"), Some(Admin), "account wins");
        assert_eq!(
            Principal::Admin.access(&bucket("b9", "u2"), "/"),
            Some(Admin)
        );
        assert!(Principal::Admin.is_admin() && Principal::Admin.user().is_none());
        assert_eq!(acc.user().unwrap().username, "alice");
    }

    #[test]
    fn credential_parsing() {
        use axum::http::HeaderValue;
        let mut h = HeaderMap::new();
        assert_eq!(bearer(&h), None);
        h.insert(
            header::COOKIE,
            HeaderValue::from_static("a=1; rms_key=rms_x; rms_token=t"),
        );
        assert_eq!(cookie(&h, KEY_COOKIE).as_deref(), Some("rms_x"));
        assert_eq!(cookie(&h, COOKIE_NAME).as_deref(), Some("t"));
        assert_eq!(cookie(&h, "nope"), None);
        h.insert(ACCESS_KEY_HEADER, HeaderValue::from_static("  rms_y "));
        assert_eq!(access_key(&h).as_deref(), Some("rms_y"));
        h.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer  abc "),
        );
        assert_eq!(bearer(&h).as_deref(), Some("abc"));
        h.insert(header::AUTHORIZATION, HeaderValue::from_static("Basic abc"));
        assert_eq!(bearer(&h), None);
    }
}
