//! `api_keys` repository (phase 8A, docs/ACCOUNTS.md §1-§2).
//!
//! A key is `rms_` + 40 hex characters, shown once at creation; only its SHA-256 (hex)
//! is stored (`key_hash`, unique: requests look keys up by hash, so no secret comparison
//! happens in Rust) plus the first 12 characters for lists (`key_prefix`). `scopes` and
//! `permissions` are JSON arrays. Revoking sets `revoked_at` (the row stays for the
//! audit trail); `last_used_at` is written at most once a minute.

use chrono::{DateTime, Utc};
use rusqlite::{OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx_core::row::Row as _;
use sqlx_postgres::PgRow;

use super::pg::{q, q_dyn};
use super::{Db, DbResult, fmt_ts, now_ts, parse_ts};

/// Scope: manage the user's keys and buckets; implies `admin` on every own bucket.
pub const SCOPE_ACCOUNT: &str = "account";
/// Scope: use the storage API within `permissions`.
pub const SCOPE_STORAGE: &str = "storage";
pub const SCOPES: [&str; 2] = [SCOPE_ACCOUNT, SCOPE_STORAGE];

/// Prefix of every key.
pub const KEY_PREFIX: &str = "rms_";
/// Hex characters after [`KEY_PREFIX`].
pub const KEY_HEX_LEN: usize = 40;
/// Characters kept in `key_prefix` (`rms_ab12cd34`).
pub const DISPLAY_PREFIX_LEN: usize = 12;
/// `last_used_at` is refreshed at most this often.
pub const TOUCH_INTERVAL_SECS: i64 = 60;

/// Access level of a permission entry: `read < write < admin`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Access {
    /// List directories, download files, sign playback URLs.
    Read,
    /// + upload/overwrite/delete files, (re)encode.
    Write,
    /// + delete directories recursively.
    Admin,
}

impl Access {
    pub fn as_str(self) -> &'static str {
        match self {
            Access::Read => "read",
            Access::Write => "write",
            Access::Admin => "admin",
        }
    }
}

/// One entry of `api_keys.permissions`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Permission {
    /// Bucket id, or `*` = every bucket of the user.
    pub bucket: String,
    /// Path prefix (`/` = whole bucket, `/videos/` = that directory and below).
    pub prefix: String,
    pub access: Access,
}

/// One row of `api_keys` (never the secret).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ApiKey {
    pub id: String,
    pub user_id: String,
    pub name: String,
    /// First 12 characters of the key (`rms_ab12cd34`).
    pub key_prefix: String,
    pub scopes: Vec<String>,
    pub permissions: Vec<Permission>,
    pub expires_at: Option<DateTime<Utc>>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub revoked_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

impl ApiKey {
    pub fn has_scope(&self, scope: &str) -> bool {
        self.scopes.iter().any(|s| s == scope)
    }

    pub fn is_revoked(&self) -> bool {
        self.revoked_at.is_some()
    }

    pub fn is_expired(&self, now: DateTime<Utc>) -> bool {
        self.expires_at.is_some_and(|t| t <= now)
    }

    /// Neither revoked nor expired.
    pub fn is_usable(&self, now: DateTime<Utc>) -> bool {
        !self.is_revoked() && !self.is_expired(now)
    }
}

/// The owner of a key, read together with it by [`find_by_hash`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyOwner {
    pub user_id: String,
    pub username: String,
    pub display_name: String,
    /// `users.status`.
    pub status: String,
}

/// A freshly generated key: the secret (returned to the client once) and what is stored.
#[derive(Debug, Clone)]
pub struct KeySecret {
    pub plain: String,
    pub hash: String,
    pub prefix: String,
}

/// SHA-256 (lowercase hex) of a key: the stored `key_hash`.
pub fn hash_key(plain: &str) -> String {
    hex::encode(Sha256::digest(plain.as_bytes()))
}

/// `rms_` + 40 lowercase hex characters.
pub fn looks_like_key(s: &str) -> bool {
    s.strip_prefix(KEY_PREFIX).is_some_and(|h| {
        h.len() == KEY_HEX_LEN
            && h.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

/// A new random key (160 bits).
pub fn generate() -> KeySecret {
    let bytes: [u8; KEY_HEX_LEN / 2] = rand::random();
    let plain = format!("{KEY_PREFIX}{}", hex::encode(bytes));
    KeySecret {
        hash: hash_key(&plain),
        prefix: plain[..DISPLAY_PREFIX_LEN].to_string(),
        plain,
    }
}

const COLUMNS: &str = "id, user_id, name, key_prefix, scopes, permissions, expires_at, \
     last_used_at, revoked_at, created_at";
/// [`COLUMNS`] qualified with `k.` + the owner columns (for the join with `users u`).
const JOINED: &str = "k.id, k.user_id, k.name, k.key_prefix, k.scopes, k.permissions, \
     k.expires_at, k.last_used_at, k.revoked_at, k.created_at, u.username, u.display_name, u.status";

fn json_list<T: serde::de::DeserializeOwned>(s: &str) -> Vec<T> {
    // Malformed JSON (manual edits) grants nothing.
    serde_json::from_str(s).unwrap_or_default()
}

fn opt_ts(s: Option<String>) -> Option<DateTime<Utc>> {
    s.map(|s| parse_ts(&s))
}

fn from_row(r: &Row<'_>) -> rusqlite::Result<ApiKey> {
    Ok(ApiKey {
        id: r.get(0)?,
        user_id: r.get(1)?,
        name: r.get(2)?,
        key_prefix: r.get(3)?,
        scopes: json_list(&r.get::<_, String>(4)?),
        permissions: json_list(&r.get::<_, String>(5)?),
        expires_at: opt_ts(r.get(6)?),
        last_used_at: opt_ts(r.get(7)?),
        revoked_at: opt_ts(r.get(8)?),
        created_at: parse_ts(&r.get::<_, String>(9)?),
    })
}

fn pg_row(r: &PgRow) -> DbResult<ApiKey> {
    Ok(ApiKey {
        id: r.try_get(0)?,
        user_id: r.try_get(1)?,
        name: r.try_get(2)?,
        key_prefix: r.try_get(3)?,
        scopes: json_list(&r.try_get::<String, _>(4)?),
        permissions: json_list(&r.try_get::<String, _>(5)?),
        expires_at: opt_ts(r.try_get(6)?),
        last_used_at: opt_ts(r.try_get(7)?),
        revoked_at: opt_ts(r.try_get(8)?),
        created_at: parse_ts(&r.try_get::<String, _>(9)?),
    })
}

fn to_json<T: Serialize>(v: &T) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "[]".into())
}

/// Insert a key row with its hash.
pub async fn insert(db: &Db, key: &ApiKey, hash: &str) -> DbResult<()> {
    let k = key.clone();
    let hash = hash.to_string();
    let (scopes, perms) = (to_json(&k.scopes), to_json(&k.permissions));
    if let Some(pool) = db.pg() {
        q(
            "INSERT INTO api_keys (id, user_id, name, key_prefix, key_hash, scopes, permissions, \
           expires_at, last_used_at, revoked_at, created_at) \
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
        )
        .bind(k.id)
        .bind(k.user_id)
        .bind(k.name)
        .bind(k.key_prefix)
        .bind(hash)
        .bind(scopes)
        .bind(perms)
        .bind(k.expires_at.as_ref().map(fmt_ts))
        .bind(k.last_used_at.as_ref().map(fmt_ts))
        .bind(k.revoked_at.as_ref().map(fmt_ts))
        .bind(fmt_ts(&k.created_at))
        .execute(pool)
        .await?;
        return Ok(());
    }
    db.call(move |c| {
        c.execute(
            "INSERT INTO api_keys (id, user_id, name, key_prefix, key_hash, scopes, permissions, \
             expires_at, last_used_at, revoked_at, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                k.id,
                k.user_id,
                k.name,
                k.key_prefix,
                hash,
                scopes,
                perms,
                k.expires_at.as_ref().map(fmt_ts),
                k.last_used_at.as_ref().map(fmt_ts),
                k.revoked_at.as_ref().map(fmt_ts),
                fmt_ts(&k.created_at),
            ],
        )?;
        Ok(())
    })
    .await
}

/// Generate and store a new key; returns the row and the plain secret (shown once).
pub async fn create(
    db: &Db,
    user_id: &str,
    name: &str,
    scopes: Vec<String>,
    permissions: Vec<Permission>,
    expires_at: Option<DateTime<Utc>>,
) -> DbResult<(ApiKey, String)> {
    let secret = generate();
    let key = ApiKey {
        id: uuid::Uuid::new_v4().hyphenated().to_string(),
        user_id: user_id.to_string(),
        name: name.to_string(),
        key_prefix: secret.prefix.clone(),
        scopes,
        permissions,
        expires_at,
        last_used_at: None,
        revoked_at: None,
        created_at: parse_ts(&now_ts()),
    };
    insert(db, &key, &secret.hash).await?;
    Ok((key, secret.plain))
}

/// One key of `user_id`.
pub async fn get(db: &Db, user_id: &str, id: &str) -> DbResult<Option<ApiKey>> {
    let (user_id, id) = (user_id.to_string(), id.to_string());
    if let Some(pool) = db.pg() {
        return q_dyn(format!(
            "SELECT {COLUMNS} FROM api_keys WHERE id = $1 AND user_id = $2"
        ))
        .bind(id)
        .bind(user_id)
        .fetch_optional(pool)
        .await?
        .as_ref()
        .map(pg_row)
        .transpose();
    }
    db.call(move |c| {
        Ok(c.query_row(
            &format!("SELECT {COLUMNS} FROM api_keys WHERE id = ?1 AND user_id = ?2"),
            params![id, user_id],
            from_row,
        )
        .optional()?)
    })
    .await
}

/// Keys of a user, newest first (revoked ones included).
pub async fn list_for_user(db: &Db, user_id: &str) -> DbResult<Vec<ApiKey>> {
    let user_id = user_id.to_string();
    if let Some(pool) = db.pg() {
        return q_dyn(format!(
            "SELECT {COLUMNS} FROM api_keys WHERE user_id = $1 ORDER BY created_at DESC, id"
        ))
        .bind(user_id)
        .fetch_all(pool)
        .await?
        .iter()
        .map(pg_row)
        .collect();
    }
    db.call(move |c| {
        let mut st = c.prepare(&format!(
            "SELECT {COLUMNS} FROM api_keys WHERE user_id = ?1 ORDER BY created_at DESC, id"
        ))?;
        let rows = st
            .query_map([user_id], from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    })
    .await
}

/// The key whose hash is `hash`, with its owner (any state: the caller checks revocation,
/// expiry and the user status).
pub async fn find_by_hash(db: &Db, hash: &str) -> DbResult<Option<(ApiKey, KeyOwner)>> {
    let hash = hash.to_string();
    if let Some(pool) = db.pg() {
        let Some(r) = q_dyn(format!(
            "SELECT {JOINED} FROM api_keys k JOIN users u ON u.id = k.user_id WHERE k.key_hash = $1"
        ))
        .bind(hash)
        .fetch_optional(pool)
        .await?
        else {
            return Ok(None);
        };
        let key = pg_row(&r)?;
        let owner = KeyOwner {
            user_id: key.user_id.clone(),
            username: r.try_get(10)?,
            display_name: r.try_get(11)?,
            status: r.try_get(12)?,
        };
        return Ok(Some((key, owner)));
    }
    db.call(move |c| {
        Ok(c.query_row(
            &format!(
                "SELECT {JOINED} FROM api_keys k JOIN users u ON u.id = k.user_id WHERE k.key_hash = ?1"
            ),
            [hash],
            |r| {
                let key = from_row(r)?;
                let owner = KeyOwner {
                    user_id: key.user_id.clone(),
                    username: r.get(10)?,
                    display_name: r.get(11)?,
                    status: r.get(12)?,
                };
                Ok((key, owner))
            },
        )
        .optional()?)
    })
    .await
}

/// `last_used_at = now` unless it was set less than [`TOUCH_INTERVAL_SECS`] ago. True
/// when the row was written.
pub async fn touch_last_used(db: &Db, id: &str) -> DbResult<bool> {
    let id = id.to_string();
    let now = Utc::now();
    let (now_s, before) = (
        fmt_ts(&now),
        fmt_ts(&(now - chrono::Duration::seconds(TOUCH_INTERVAL_SECS))),
    );
    if let Some(pool) = db.pg() {
        let n = q("UPDATE api_keys SET last_used_at = $2 \
                   WHERE id = $1 AND (last_used_at IS NULL OR last_used_at < $3)")
        .bind(id)
        .bind(now_s)
        .bind(before)
        .execute(pool)
        .await?
        .rows_affected();
        return Ok(n == 1);
    }
    db.call(move |c| {
        Ok(c.execute(
            "UPDATE api_keys SET last_used_at = ?2 \
             WHERE id = ?1 AND (last_used_at IS NULL OR last_used_at < ?3)",
            params![id, now_s, before],
        )? == 1)
    })
    .await
}

/// Write name, scopes, permissions and expiry of `key` (matched by id and user).
/// Returns the stored row, `None` when it does not exist.
pub async fn update(db: &Db, key: &ApiKey) -> DbResult<Option<ApiKey>> {
    let k = key.clone();
    let (scopes, perms) = (to_json(&k.scopes), to_json(&k.permissions));
    if let Some(pool) = db.pg() {
        return q_dyn(format!(
            "UPDATE api_keys SET name = $3, scopes = $4, permissions = $5, expires_at = $6 \
             WHERE id = $1 AND user_id = $2 RETURNING {COLUMNS}"
        ))
        .bind(k.id)
        .bind(k.user_id)
        .bind(k.name)
        .bind(scopes)
        .bind(perms)
        .bind(k.expires_at.as_ref().map(fmt_ts))
        .fetch_optional(pool)
        .await?
        .as_ref()
        .map(pg_row)
        .transpose();
    }
    db.call(move |c| {
        Ok(c.query_row(
            &format!(
                "UPDATE api_keys SET name = ?3, scopes = ?4, permissions = ?5, expires_at = ?6 \
                 WHERE id = ?1 AND user_id = ?2 RETURNING {COLUMNS}"
            ),
            params![
                k.id,
                k.user_id,
                k.name,
                scopes,
                perms,
                k.expires_at.as_ref().map(fmt_ts)
            ],
            from_row,
        )
        .optional()?)
    })
    .await
}

/// Revoke a key of `user_id` (`revoked_at = now`, kept when already revoked). Returns
/// the row, `None` when it does not exist.
pub async fn revoke(db: &Db, user_id: &str, id: &str) -> DbResult<Option<ApiKey>> {
    let (user_id, id) = (user_id.to_string(), id.to_string());
    if let Some(pool) = db.pg() {
        return q_dyn(format!(
            "UPDATE api_keys SET revoked_at = COALESCE(revoked_at, $3) \
             WHERE id = $1 AND user_id = $2 RETURNING {COLUMNS}"
        ))
        .bind(id)
        .bind(user_id)
        .bind(now_ts())
        .fetch_optional(pool)
        .await?
        .as_ref()
        .map(pg_row)
        .transpose();
    }
    db.call(move |c| {
        Ok(c.query_row(
            &format!(
                "UPDATE api_keys SET revoked_at = COALESCE(revoked_at, ?3) \
                 WHERE id = ?1 AND user_id = ?2 RETURNING {COLUMNS}"
            ),
            params![id, user_id, now_ts()],
            from_row,
        )
        .optional()?)
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::testkit::{dual_db_test, execute};
    use crate::db::users::{self, User};

    #[test]
    fn key_format() {
        let k = generate();
        assert!(looks_like_key(&k.plain), "{}", k.plain);
        assert_eq!(k.plain.len(), 44);
        assert_eq!(k.prefix.len(), DISPLAY_PREFIX_LEN);
        assert!(k.plain.starts_with(&k.prefix));
        assert_eq!(k.hash, hash_key(&k.plain));
        assert_eq!(k.hash.len(), 64);
        assert_ne!(generate().plain, k.plain);
        assert!(!looks_like_key("rms_XYZ"));
        assert!(!looks_like_key(&k.plain.to_uppercase()));
        assert!(Access::Read < Access::Write && Access::Write < Access::Admin);
        let p: Permission =
            serde_json::from_str(r#"{"bucket":"*","prefix":"/videos/","access":"write"}"#).unwrap();
        assert_eq!(p.access, Access::Write);
        assert!(
            serde_json::from_str::<Permission>(r#"{"bucket":"*","prefix":"/","access":"root"}"#)
                .is_err()
        );
    }

    async fn lifecycle_body(db: Db) {
        let alice = User::new("alice", "Alice");
        users::insert(&db, &alice).await.unwrap();
        let perms = vec![Permission {
            bucket: "*".into(),
            prefix: "/videos/".into(),
            access: Access::Write,
        }];
        let (k1, plain1) = create(
            &db,
            &alice.id,
            "upload",
            vec![SCOPE_STORAGE.into()],
            perms.clone(),
            None,
        )
        .await
        .unwrap();
        assert!(looks_like_key(&plain1));
        assert_eq!(k1.key_prefix, plain1[..12]);
        let (k2, plain2) = create(
            &db,
            &alice.id,
            "dashboard",
            vec![SCOPE_ACCOUNT.into()],
            vec![],
            Some(parse_ts("2000-01-01T00:00:00.000Z")),
        )
        .await
        .unwrap();
        assert_ne!(plain1, plain2);

        let (found, owner) = find_by_hash(&db, &hash_key(&plain1))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found, k1);
        assert_eq!(found.permissions, perms);
        assert!(found.has_scope(SCOPE_STORAGE) && !found.has_scope(SCOPE_ACCOUNT));
        assert_eq!(
            (
                owner.username.as_str(),
                owner.status.as_str(),
                owner.user_id.as_str()
            ),
            ("alice", users::STATUS_ACTIVE, alice.id.as_str())
        );
        assert!(
            find_by_hash(&db, &hash_key("rms_nope"))
                .await
                .unwrap()
                .is_none()
        );
        let (expired, _) = find_by_hash(&db, &hash_key(&plain2))
            .await
            .unwrap()
            .unwrap();
        assert!(expired.is_expired(Utc::now()) && !expired.is_usable(Utc::now()));
        assert!(found.is_usable(Utc::now()));

        let ids: Vec<String> = list_for_user(&db, &alice.id)
            .await
            .unwrap()
            .into_iter()
            .map(|k| k.id)
            .collect();
        assert_eq!(ids.len(), 2);
        assert!(list_for_user(&db, "other").await.unwrap().is_empty());
        assert_eq!(get(&db, &alice.id, &k1.id).await.unwrap().unwrap(), k1);
        assert!(
            get(&db, "other", &k1.id).await.unwrap().is_none(),
            "scoped to the user"
        );

        // last_used_at: at most once a minute.
        assert!(touch_last_used(&db, &k1.id).await.unwrap());
        assert!(!touch_last_used(&db, &k1.id).await.unwrap());
        execute(
            &db,
            &format!(
                "UPDATE api_keys SET last_used_at = '2000-01-01T00:00:00.000Z' WHERE id = '{}'",
                k1.id
            ),
        )
        .await;
        assert!(touch_last_used(&db, &k1.id).await.unwrap());
        assert!(
            get(&db, &alice.id, &k1.id)
                .await
                .unwrap()
                .unwrap()
                .last_used_at
                .unwrap()
                > parse_ts("2001-01-01T00:00:00Z")
        );

        // Update.
        let mut changed = k1.clone();
        changed.name = "renamed".into();
        changed.scopes = vec![SCOPE_STORAGE.into(), SCOPE_ACCOUNT.into()];
        changed.permissions.push(Permission {
            bucket: "b1".into(),
            prefix: "/".into(),
            access: Access::Read,
        });
        changed.expires_at = Some(parse_ts("2099-01-01T00:00:00.000Z"));
        let stored = update(&db, &changed).await.unwrap().unwrap();
        assert_eq!(
            (
                stored.name.as_str(),
                stored.scopes.len(),
                stored.permissions.len()
            ),
            ("renamed", 2, 2)
        );
        assert_eq!(stored.expires_at, changed.expires_at);
        let mut foreign = changed.clone();
        foreign.user_id = "other".into();
        assert!(update(&db, &foreign).await.unwrap().is_none());

        // Revoke (idempotent, keeps the first timestamp).
        let r = revoke(&db, &alice.id, &k1.id).await.unwrap().unwrap();
        assert!(r.is_revoked() && !r.is_usable(Utc::now()));
        let again = revoke(&db, &alice.id, &k1.id).await.unwrap().unwrap();
        assert_eq!(again.revoked_at, r.revoked_at);
        assert!(revoke(&db, "other", &k2.id).await.unwrap().is_none());
        assert!(revoke(&db, &alice.id, "nope").await.unwrap().is_none());
        assert_eq!(
            users::usage(&db, &alice.id).await.unwrap().keys,
            1,
            "revoked keys are not counted"
        );

        // Duplicate hash is refused.
        let dup = ApiKey {
            id: "x".into(),
            ..k1.clone()
        };
        assert!(
            insert(&db, &dup, &hash_key(&plain1))
                .await
                .unwrap_err()
                .is_unique_violation()
        );
    }
    dual_db_test!(lifecycle, lifecycle_body);
}
