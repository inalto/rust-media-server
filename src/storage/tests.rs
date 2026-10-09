//! End-to-end tests of the phase 8A HTTP surface (in-process router, SQLite): users,
//! keys, principals, buckets, the storage API, encodings of bucket files and signed
//! playback.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{HeaderMap, Request, StatusCode, header};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

use crate::config::Config;
use crate::db::Db;
use crate::db::vod as vod_repo;
use crate::hub::Hub;
use crate::metrics::ViewerTracker;
use crate::packager::PackagerManager;
use crate::recorder::RecorderManager;
use crate::state::{AppState, AppStateParts};
use crate::storage::sign;

const ADMIN: &str = "test-admin-token";

struct T {
    state: AppState,
    dir: PathBuf,
}

impl Drop for T {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn setup(f: impl FnOnce(&mut Config)) -> T {
    let dir = std::env::temp_dir().join(format!("rms-storage-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut config = Config::default();
    config.server.data_dir = dir.clone();
    config.server.admin_token = ADMIN.into();
    // No ffmpeg: encodings stay queued (no runner in the tests).
    config.ffmpeg.path = "/nonexistent/ffmpeg".into();
    f(&mut config);
    let config = Arc::new(config);
    let db = Db::open_in_memory().unwrap();
    let hub = Hub::new();
    let tracker = ViewerTracker::new(&config, None, hub.events());
    let packager = PackagerManager::new(config.clone(), hub.clone(), tracker.clone());
    let recorder = RecorderManager::new(config.clone(), db.clone(), hub.clone());
    let webrtc = crate::webrtc::WebRtcManager::new(config.clone(), hub.clone(), tracker.clone());
    let state = AppState::new(AppStateParts {
        config,
        db,
        hub,
        tracker,
        packager,
        recorder,
        webrtc,
        admin_token: ADMIN.into(),
        shutdown: CancellationToken::new(),
    });
    T { state, dir }
}

struct Resp {
    status: StatusCode,
    headers: HeaderMap,
    body: Vec<u8>,
}

impl Resp {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

impl T {
    fn app(&self) -> Router {
        Router::new()
            .merge(crate::api::router(&self.state))
            .merge(crate::storage::api::router(&self.state))
            .merge(crate::vod::router())
            .with_state(self.state.clone())
    }

    async fn send(&self, mut req: Request<Body>) -> Resp {
        req.extensions_mut()
            .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 40000))));
        let resp = self.app().oneshot(req).await.unwrap();
        let status = resp.status();
        let headers = resp.headers().clone();
        let body = axum::body::to_bytes(resp.into_body(), 64 << 20)
            .await
            .unwrap()
            .to_vec();
        Resp {
            status,
            headers,
            body,
        }
    }

    async fn call(&self, method: &str, uri: &str, auth: Option<&str>, body: Option<Value>) -> Resp {
        let mut b = Request::builder().method(method).uri(uri);
        if let Some(a) = auth {
            b = b.header(header::AUTHORIZATION, format!("Bearer {a}"));
        }
        let req = match body {
            Some(v) => b
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(v.to_string()))
                .unwrap(),
            None => b.body(Body::empty()).unwrap(),
        };
        self.send(req).await
    }

    /// `PUT /storage/...` with the `AccessKey` header.
    async fn put(&self, key: &str, uri: &str, data: &[u8], headers: &[(&str, &str)]) -> Resp {
        let mut b = Request::builder()
            .method("PUT")
            .uri(uri)
            .header("AccessKey", key)
            .header(header::CONTENT_LENGTH, data.len());
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        self.send(b.body(Body::from(data.to_vec())).unwrap()).await
    }

    async fn key_req(&self, method: &str, uri: &str, key: &str) -> Resp {
        self.send(
            Request::builder()
                .method(method)
                .uri(uri)
                .header("AccessKey", key)
                .body(Body::empty())
                .unwrap(),
        )
        .await
    }

    /// Admin creates a user and an `account` key; returns (user id, key).
    async fn user_with_account_key(&self, username: &str) -> (String, String) {
        let r = self
            .call(
                "POST",
                "/api/v1/users",
                Some(ADMIN),
                Some(json!({"username": username})),
            )
            .await;
        assert_eq!(r.status, StatusCode::CREATED, "{}", r.text());
        let id = r.json()["id"].as_str().unwrap().to_string();
        let r = self
            .call(
                "POST",
                &format!("/api/v1/users/{id}/keys"),
                Some(ADMIN),
                Some(json!({"name": "dashboard", "scopes": ["account"]})),
            )
            .await;
        assert_eq!(r.status, StatusCode::CREATED, "{}", r.text());
        (id, r.json()["key"].as_str().unwrap().to_string())
    }
}

fn sha(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

#[tokio::test]
async fn users_keys_and_principals() {
    let t = setup(|_| {});
    // Validation with field names, duplicates.
    let r = t
        .call(
            "POST",
            "/api/v1/users",
            Some(ADMIN),
            Some(json!({"username": "Bad Name"})),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(r.json()["field"], "username");
    let r = t
        .call(
            "POST",
            "/api/v1/users",
            Some(ADMIN),
            Some(json!({"username": "alice", "email": "nope"})),
        )
        .await;
    assert_eq!(r.json()["field"], "email");
    let r = t
        .call(
            "POST",
            "/api/v1/users",
            Some(ADMIN),
            Some(json!({"username": "alice", "default_ladder": "nope"})),
        )
        .await;
    assert_eq!(r.json()["field"], "default_ladder");
    let (alice, key) = t.user_with_account_key("alice").await;
    let r = t
        .call(
            "POST",
            "/api/v1/users",
            Some(ADMIN),
            Some(json!({"username": "alice"})),
        )
        .await;
    assert_eq!(r.status, StatusCode::CONFLICT);
    assert!(key.starts_with("rms_") && key.len() == 44);

    // Admin views.
    let r = t.call("GET", "/api/v1/users", Some(ADMIN), None).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json()[0]["username"], "alice");
    assert_eq!(r.json()[0]["usage"]["keys"], 1);
    let r = t
        .call("GET", "/api/v1/users/alice", Some(ADMIN), None)
        .await;
    assert_eq!(r.json()["id"], alice.as_str(), "by username too");
    let r = t
        .call(
            "GET",
            &format!("/api/v1/users/{alice}/keys"),
            Some(ADMIN),
            None,
        )
        .await;
    let keys = r.json();
    assert_eq!(keys[0]["state"], "active");
    assert!(
        keys[0].get("key_hash").is_none() && !r.text().contains(&key),
        "never the secret"
    );
    assert_eq!(keys[0]["key_prefix"], &key[..12]);

    // A user key: not on the admin API, fine on its account.
    let r = t.call("GET", "/api/v1/broadcasts", Some(&key), None).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let r = t.call("GET", "/api/v1/users", Some(&key), None).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let r = t.call("GET", "/api/v1/account", Some(&key), None).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text());
    assert_eq!(r.json()["user"]["username"], "alice");
    assert_eq!(r.json()["key"]["scopes"][0], "account");
    assert_eq!(r.json()["limits"]["max_object_mb"], 20480);
    let r = t.key_req("GET", "/api/v1/account", &key).await;
    assert_eq!(r.status, StatusCode::OK, "AccessKey header");
    // Admin on the account routes needs ?user=.
    let r = t.call("GET", "/api/v1/account", Some(ADMIN), None).await;
    assert_eq!(
        (r.status, r.json()["field"].as_str()),
        (StatusCode::BAD_REQUEST, Some("user"))
    );
    let r = t
        .call("GET", "/api/v1/account?user=alice", Some(ADMIN), None)
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.json()["key"].is_null());
    let r = t
        .call(
            "GET",
            "/api/v1/account",
            Some("rms_0000000000000000000000000000000000000000"),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    let r = t.call("GET", "/api/v1/account", None, None).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);

    // Auth check / login with a key (cookie), additive JSON for the admin.
    let r = t.call("GET", "/api/v1/auth/check", Some(ADMIN), None).await;
    assert_eq!(
        (r.status, r.json()["role"].as_str()),
        (StatusCode::OK, Some("admin"))
    );
    let r = t.call("GET", "/api/v1/auth/check", Some(&key), None).await;
    assert_eq!(r.json()["role"], "user");
    assert_eq!(r.json()["user"]["username"], "alice");
    assert_eq!(r.json()["scopes"], json!(["account"]));
    let r = t
        .call(
            "POST",
            "/api/v1/auth/login",
            None,
            Some(json!({"token": key})),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json()["role"], "user");
    let cookies: Vec<String> = r
        .headers
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|v| v.to_str().unwrap().to_string())
        .collect();
    assert!(
        cookies
            .iter()
            .any(|c| c.starts_with(&format!("rms_key={key};")) && c.contains("HttpOnly"))
    );
    assert!(cookies.iter().any(|c| c.starts_with("rms_token=;")));
    let with_cookie = |c: String| {
        Request::builder()
            .uri("/api/v1/account")
            .header(header::COOKIE, c)
            .body(Body::empty())
            .unwrap()
    };
    assert_eq!(
        t.send(with_cookie(format!("rms_key={key}"))).await.status,
        StatusCode::OK
    );
    let r = t
        .call(
            "POST",
            "/api/v1/auth/login",
            None,
            Some(json!({"token": ADMIN})),
        )
        .await;
    assert_eq!(
        (r.status, r.json()["role"].as_str()),
        (StatusCode::OK, Some("admin"))
    );
    let r = t
        .call(
            "POST",
            "/api/v1/auth/login",
            None,
            Some(json!({"token": "rms_0000000000000000000000000000000000000000"})),
        )
        .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    let u = crate::db::users::get(&t.state.db, &alice)
        .await
        .unwrap()
        .unwrap();
    assert!(u.last_login_at.is_some());

    // Keys: storage key without permissions refused; bucket names resolved to ids;
    // permissions must reference own buckets.
    let r = t
        .call(
            "POST",
            "/api/v1/account/keys",
            Some(&key),
            Some(json!({"name": "up", "scopes": ["storage"]})),
        )
        .await;
    assert_eq!(r.json()["field"], "permissions");
    let r = t
        .call(
            "POST",
            "/api/v1/account/buckets",
            Some(&key),
            Some(json!({"name": "media", "public_playback": false})),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text());
    let bucket_id = r.json()["id"].as_str().unwrap().to_string();
    assert_eq!(r.json()["signing_key"].as_str().unwrap().len(), 64);
    assert!(
        r.json()["storage_url"]
            .as_str()
            .unwrap()
            .ends_with("/storage/media/")
    );
    let r = t
        .call(
            "POST",
            "/api/v1/account/keys",
            Some(&key),
            Some(json!({"name": "up", "scopes": ["storage"], "permissions": [
                {"bucket": "media", "prefix": "/videos/", "access": "write"},
                {"bucket": "other", "prefix": "/", "access": "read"}]})),
        )
        .await;
    assert_eq!(r.json()["field"], "permissions");
    let r = t
        .call(
            "POST",
            "/api/v1/account/keys",
            Some(&key),
            Some(json!({"name": "up", "scopes": ["storage"], "expires_at": "2099-01-01T00:00:00Z", "permissions": [
                {"bucket": "media", "prefix": "/videos/", "access": "write"}]})),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text());
    let storage_key = r.json()["key"].as_str().unwrap().to_string();
    let kid = r.json()["id"].as_str().unwrap().to_string();
    assert_eq!(r.json()["permissions"][0]["bucket"], bucket_id.as_str());
    // A storage key cannot manage the account.
    let r = t
        .call("GET", "/api/v1/account/keys", Some(&storage_key), None)
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    // PATCH, revoke → 401.
    let r = t
        .call(
            "PATCH",
            &format!("/api/v1/account/keys/{kid}"),
            Some(&key),
            Some(json!({"name": "renamed", "expires_at": null})),
        )
        .await;
    assert_eq!(
        (r.status, r.json()["name"].as_str()),
        (StatusCode::OK, Some("renamed"))
    );
    assert!(r.json()["expires_at"].is_null());
    let r = t
        .call(
            "DELETE",
            &format!("/api/v1/account/keys/{kid}"),
            Some(&key),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    let r = t
        .key_req("GET", "/storage/media/videos/", &storage_key)
        .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED, "revoked");
    let r = t
        .call(
            "GET",
            &format!("/api/v1/users/{alice}/keys"),
            Some(ADMIN),
            None,
        )
        .await;
    assert!(r.text().contains("\"state\":\"revoked\""));

    // Another user cannot see alice's buckets; max_buckets.
    let (_bob, bob_key) = t.user_with_account_key("bob").await;
    let r = t
        .call("GET", "/api/v1/account/buckets/media", Some(&bob_key), None)
        .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    let r = t.key_req("GET", "/storage/media/", &bob_key).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let r = t
        .call(
            "POST",
            "/api/v1/account/buckets",
            Some(&bob_key),
            Some(json!({"name": "media"})),
        )
        .await;
    assert_eq!(r.status, StatusCode::CONFLICT, "global names");
    let r = t
        .call(
            "PATCH",
            "/api/v1/users/bob",
            Some(ADMIN),
            Some(json!({"max_buckets": 0})),
        )
        .await;
    assert_eq!(r.json()["max_buckets"], 0);
    let r = t
        .call(
            "POST",
            "/api/v1/account/buckets",
            Some(&bob_key),
            Some(json!({"name": "bobs"})),
        )
        .await;
    assert_eq!(r.status, StatusCode::CONFLICT);

    // Suspended user: every key 403 (also on login).
    let r = t
        .call(
            "PATCH",
            "/api/v1/users/alice",
            Some(ADMIN),
            Some(json!({"status": "suspended"})),
        )
        .await;
    assert_eq!(r.json()["status"], "suspended");
    let r = t.call("GET", "/api/v1/account", Some(&key), None).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let r = t.key_req("GET", "/storage/media/", &key).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let r = t
        .call(
            "POST",
            "/api/v1/auth/login",
            None,
            Some(json!({"token": key})),
        )
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let r = t
        .call(
            "PATCH",
            "/api/v1/users/alice",
            Some(ADMIN),
            Some(json!({"status": "active"})),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);

    // User delete: 409 with buckets, cascade with force.
    let r = t
        .call("DELETE", "/api/v1/users/alice", Some(ADMIN), None)
        .await;
    assert_eq!(r.status, StatusCode::CONFLICT);
    let bucket_dir = crate::storage::bucket_dir(&t.state.config, &bucket_id);
    assert!(bucket_dir.is_dir());
    let r = t
        .call("DELETE", "/api/v1/users/alice?force=1", Some(ADMIN), None)
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    assert!(!bucket_dir.exists());
    let r = t.call("GET", "/api/v1/account", Some(&key), None).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED, "keys of a deleted user");
}

#[tokio::test]
async fn storage_api_files_permissions_quota() {
    let t = setup(|c| c.storage.max_object_mb = 1);
    let (alice, key) = t.user_with_account_key("alice").await;
    let r = t
        .call(
            "POST",
            "/api/v1/account/buckets",
            Some(&key),
            Some(json!({"name": "media"})),
        )
        .await;
    let bucket_id = r.json()["id"].as_str().unwrap().to_string();
    let r = t
        .call(
            "POST",
            "/api/v1/account/keys",
            Some(&key),
            Some(json!({"name": "s", "scopes": ["storage"], "permissions": [
                {"bucket": bucket_id, "prefix": "/docs/", "access": "write"},
                {"bucket": "*", "prefix": "/public/", "access": "read"}]})),
        )
        .await;
    let sk = r.json()["key"].as_str().unwrap().to_string();

    // Upload with checksum, nested parents created; listing; download; Range; ETag.
    let data = b"hello storage world".to_vec();
    let r = t
        .put(
            &sk,
            "/storage/media/docs/2026/a%20b.txt",
            &data,
            &[("Checksum", &sha(&data))],
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text());
    let obj = r.json();
    assert_eq!(obj["path"], "/docs/2026/a b.txt");
    assert_eq!(obj["name"], "a b.txt");
    assert_eq!(obj["size"], data.len());
    assert_eq!(obj["sha256"], sha(&data));
    assert_eq!(obj["content_type"], "text/plain");
    assert!(obj.get("encoding").is_none(), "not a video");
    assert_eq!(r.headers[header::CACHE_CONTROL], "no-store");
    let file = crate::storage::object_file(&t.state.config, &bucket_id, "/docs/2026/a b.txt");
    assert_eq!(std::fs::read(&file).unwrap(), data);
    assert_eq!(
        std::fs::read_dir(t.dir.join("upload-tmp")).unwrap().count(),
        0,
        "no temp leftovers"
    );

    let r = t.key_req("GET", "/storage/media/docs/", &sk).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text());
    assert_eq!(r.json()[0]["path"], "/docs/2026");
    assert_eq!(r.json()[0]["is_dir"], true);
    let r = t.key_req("GET", "/storage/media/docs", &sk).await;
    assert_eq!(
        r.status,
        StatusCode::OK,
        "a directory without trailing slash lists too"
    );
    let r = t
        .key_req("GET", "/storage/media/docs/2026/a%20b.txt", &sk)
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.body, data);
    assert_eq!(r.headers[header::CACHE_CONTROL], "private, max-age=0");
    let etag = r.headers[header::ETAG].to_str().unwrap().to_string();
    assert_eq!(etag, format!("\"{}\"", sha(&data)));
    let r = t
        .send(
            Request::builder()
                .uri("/storage/media/docs/2026/a%20b.txt")
                .header("AccessKey", &sk)
                .header(header::RANGE, "bytes=0-4")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(
        (r.status, r.body.as_slice()),
        (StatusCode::PARTIAL_CONTENT, &b"hello"[..])
    );
    let r = t
        .send(
            Request::builder()
                .uri("/storage/media/docs/2026/a%20b.txt")
                .header("AccessKey", &sk)
                .header(header::IF_NONE_MATCH, &etag)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(r.status, StatusCode::NOT_MODIFIED);
    let r = t
        .key_req("HEAD", "/storage/media/docs/2026/a%20b.txt", &sk)
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.is_empty());
    assert_eq!(
        r.headers[header::CONTENT_LENGTH],
        data.len().to_string().as_str()
    );
    let r = t
        .key_req("GET", "/storage/media/docs/2026/a%20b.txt?download=1", &sk)
        .await;
    assert!(
        r.headers[header::CONTENT_DISPOSITION]
            .to_str()
            .unwrap()
            .starts_with("attachment")
    );
    let r = t
        .key_req("GET", "/storage/media/docs/2026/a%20b.txt?info=1", &sk)
        .await;
    assert_eq!(r.json()["bucket"]["name"], "media");
    assert!(
        r.json()["url"]
            .as_str()
            .unwrap()
            .ends_with("/storage/media/docs/2026/a%20b.txt")
    );
    assert!(r.json()["signed"].is_null());

    // Permissions: outside the prefixes, read-only prefix, root listing, other bucket.
    assert_eq!(
        t.put(&sk, "/storage/media/other/x.txt", b"x", &[])
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        t.put(&sk, "/storage/media/public/x.txt", b"x", &[])
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    let r = t.key_req("GET", "/storage/media/public/", &sk).await;
    assert_eq!(
        (r.status, r.json()),
        (StatusCode::OK, json!([])),
        "missing directory lists empty"
    );
    assert_eq!(
        t.key_req("GET", "/storage/media/", &sk).await.status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        t.key_req("GET", "/storage/nope/", &sk).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        t.key_req("GET", "/storage/media/docs/2026/a%20b.txt", "rms_bad")
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        t.send(
            Request::builder()
                .uri("/storage/media/")
                .body(Body::empty())
                .unwrap()
        )
        .await
        .status,
        StatusCode::UNAUTHORIZED
    );
    // Directories need admin to be deleted; files write.
    assert_eq!(
        t.key_req("DELETE", "/storage/media/docs/2026/", &sk)
            .await
            .status,
        StatusCode::FORBIDDEN
    );

    // Path validation.
    for bad in [
        "/storage/media/docs/%2e%2e/x",
        "/storage/media/docs//x",
        "/storage/media/docs/.rms/x",
        "/storage/media/docs/%ZZ",
    ] {
        let r = t.put(&sk, bad, b"x", &[]).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{bad}: {}", r.text());
        assert_eq!(r.json()["field"], "path");
    }

    // Wrong checksum → 400, nothing stored; malformed checksum → 400.
    let r = t
        .put(
            &sk,
            "/storage/media/docs/c.txt",
            b"abc",
            &[("Checksum", &sha(b"abd"))],
        )
        .await;
    assert_eq!(
        (r.status, r.json()["field"].as_str()),
        (StatusCode::BAD_REQUEST, Some("Checksum"))
    );
    assert_eq!(
        t.key_req("GET", "/storage/media/docs/c.txt", &sk)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        t.put(
            &sk,
            "/storage/media/docs/c.txt",
            b"abc",
            &[("Checksum", "xyz")]
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );

    // Size limit (1 MiB here): 413 from Content-Length and while streaming.
    let big = vec![7u8; 1024 * 1024 + 1];
    assert_eq!(
        t.put(&sk, "/storage/media/docs/big.bin", &big, &[])
            .await
            .status,
        StatusCode::PAYLOAD_TOO_LARGE
    );
    let r = t
        .send(
            Request::builder()
                .method("PUT")
                .uri("/storage/media/docs/big.bin")
                .header("AccessKey", &sk)
                .body(Body::from_stream(futures::stream::iter(
                    big.chunks(64 * 1024)
                        .map(|c| Ok::<_, std::io::Error>(c.to_vec()))
                        .collect::<Vec<_>>(),
                )))
                .unwrap(),
        )
        .await;
    assert_eq!(r.status, StatusCode::PAYLOAD_TOO_LARGE, "chunked body");
    assert_eq!(
        std::fs::read_dir(t.dir.join("upload-tmp")).unwrap().count(),
        0
    );

    // Quota: 507 (the overwritten bytes are given back).
    let r = t
        .call(
            "PATCH",
            &format!("/api/v1/users/{alice}"),
            Some(ADMIN),
            Some(json!({"quota_bytes": 40})),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    let r = t
        .put(&sk, "/storage/media/docs/q.bin", &[1u8; 30], &[])
        .await;
    assert_eq!(
        r.status,
        StatusCode::INSUFFICIENT_STORAGE,
        "19 used + 30 > 40"
    );
    let r = t
        .put(&sk, "/storage/media/docs/2026/a%20b.txt", &[1u8; 40], &[])
        .await;
    assert_eq!(
        r.status,
        StatusCode::CREATED,
        "overwrite within quota: {}",
        r.text()
    );
    let r = t.call("GET", "/api/v1/account", Some(&key), None).await;
    assert_eq!(r.json()["usage"]["bytes_used"], 40);
    assert_eq!(r.json()["usage"]["objects"], 1);
    assert_eq!(r.json()["usage"]["quota_bytes"], 40);

    // Directory create/delete with the account key (admin on own buckets).
    let r = t.put(&key, "/storage/media/empty/sub/", b"", &[]).await;
    assert_eq!(
        (r.status, r.json()["is_dir"].as_bool()),
        (StatusCode::CREATED, Some(true))
    );
    assert!(crate::storage::object_file(&t.state.config, &bucket_id, "/empty/sub").is_dir());
    assert_eq!(
        t.put(&key, "/storage/media/empty/sub", b"x", &[])
            .await
            .status,
        StatusCode::CONFLICT
    );
    assert_eq!(
        t.put(&key, "/storage/media/docs/2026/a%20b.txt/x", b"x", &[])
            .await
            .status,
        StatusCode::CONFLICT
    );
    let r = t.key_req("GET", "/storage/media/", &key).await;
    let listing = r.json();
    let names: Vec<&str> = listing
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["docs", "empty"]);
    assert_eq!(
        t.key_req("DELETE", "/storage/media/", &key).await.status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        t.key_req("DELETE", "/storage/media/docs/2026/a%20b.txt/", &key)
            .await
            .status,
        StatusCode::CONFLICT
    );
    assert_eq!(
        t.key_req("DELETE", "/storage/media/docs/2026/a%20b.txt", &sk)
            .await
            .status,
        StatusCode::NO_CONTENT
    );
    assert!(!file.exists());
    assert_eq!(
        t.key_req("DELETE", "/storage/media/docs/2026/a%20b.txt", &sk)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        t.key_req("DELETE", "/storage/media/empty/", &key)
            .await
            .status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        t.key_req("DELETE", "/storage/media/empty/", &key)
            .await
            .status,
        StatusCode::NOT_FOUND
    );

    // Counters and repair.
    t.put(&key, "/storage/media/r.bin", &[0u8; 10], &[]).await;
    let f = crate::storage::object_file(&t.state.config, &bucket_id, "/r.bin");
    std::fs::write(&f, [0u8; 12]).unwrap();
    let r = t
        .call(
            "POST",
            &format!("/api/v1/users/{alice}/recount"),
            Some(ADMIN),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text());
    assert_eq!(r.json()["report"]["sizes_fixed"], 1);
    assert_eq!(r.json()["usage"]["bytes_used"], 12);
    let r = t
        .call(
            "GET",
            &format!("/api/v1/users/{alice}/usage"),
            Some(ADMIN),
            None,
        )
        .await;
    assert_eq!(r.json()["buckets"][0]["bytes_used"], 12);

    // Bucket delete: 409 when not empty, force removes the files.
    let r = t
        .call("DELETE", "/api/v1/account/buckets/media", Some(&key), None)
        .await;
    assert_eq!(r.status, StatusCode::CONFLICT);
    let r = t
        .call(
            "DELETE",
            "/api/v1/account/buckets/media?force=1",
            Some(&key),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    assert!(!crate::storage::bucket_dir(&t.state.config, &bucket_id).exists());
    assert_eq!(
        t.key_req("GET", "/storage/media/", &key).await.status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn encodings_of_bucket_files() {
    let t = setup(|_| {});
    let (alice, key) = t.user_with_account_key("alice").await;
    let r = t
        .call(
            "POST",
            "/api/v1/account/buckets",
            Some(&key),
            Some(json!({"name": "media", "default_ladder": "mobile"})),
        )
        .await;
    let bucket_id = r.json()["id"].as_str().unwrap().to_string();

    // A video name is encoded automatically (bucket ladder, public by default).
    let r = t
        .put(
            &key,
            "/storage/media/videos/clip.mp4",
            b"not really a video",
            &[],
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text());
    let enc = r.json()["encoding"].clone();
    assert_eq!(
        (enc["status"].as_str(), enc["ladder"].as_str()),
        (Some("queued"), Some("mobile"))
    );
    assert_eq!(enc["visibility"], "public");
    let vod_id = enc["vod_id"].as_str().unwrap().to_string();
    let item = vod_repo::get(&t.state.db, &vod_id).await.unwrap().unwrap();
    assert_eq!(item.source_path, "/videos/clip.mp4");
    assert_eq!(item.title, "clip.mp4");
    assert_eq!(item.origin, vod_repo::ORIGIN_STORAGE);
    assert_eq!(item.user_id.as_deref(), Some(alice.as_str()));
    let src = t.state.vod.source_file(&item).unwrap();
    assert!(
        src.starts_with(t.dir.join("buckets").join(&bucket_id)) && src.is_file(),
        "in place, no copy"
    );

    // Admin library shows the links; listing shows the encoding.
    let r = t.call("GET", "/api/v1/vod", Some(ADMIN), None).await;
    let v = &r.json()[0];
    assert_eq!(
        (v["user_id"].as_str(), v["bucket_id"].as_str()),
        (Some(alice.as_str()), Some(bucket_id.as_str()))
    );
    assert_eq!(v["object_path"], "/videos/clip.mp4");
    assert_eq!(v["source_available"], true);
    let r = t.key_req("GET", "/storage/media/videos/", &key).await;
    assert_eq!(r.json()[0]["encoding"]["vod_id"], vod_id.as_str());
    let r = t
        .call("GET", "/api/v1/account/encodings", Some(&key), None)
        .await;
    assert_eq!(r.json()[0]["id"], vod_id.as_str());

    // Overwrite → same encoding re-queued with the new size; X-Rms-Visibility.
    assert!(
        vod_repo::claim(&t.state.db, &vod_id).await.unwrap(),
        "pretend a job runs"
    );
    let r = t
        .put(
            &key,
            "/storage/media/videos/clip.mp4",
            b"another version",
            &[("X-Rms-Visibility", "private")],
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED);
    assert_eq!(r.json()["encoding"]["vod_id"], vod_id.as_str());
    let item = vod_repo::get(&t.state.db, &vod_id).await.unwrap().unwrap();
    assert_eq!((item.status.as_str(), item.source_size), ("queued", 15));
    assert!(item.is_private());
    // The public catalogue hides private items.
    vod_repo::claim(&t.state.db, &vod_id).await.unwrap();
    vod_repo::mark_ready(&t.state.db, &vod_id, &[])
        .await
        .unwrap();
    let r = t.call("GET", "/api/v1/public/vod", None, None).await;
    assert_eq!(r.json(), json!([]));

    // ?action=encode with a ladder (202), unknown ladder 400, sign.
    let r = t
        .call(
            "POST",
            "/storage/media/videos/clip.mp4?action=encode",
            None,
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    let r = t
        .send(
            Request::builder()
                .method("POST")
                .uri("/storage/media/videos/clip.mp4?action=encode")
                .header("AccessKey", &key)
                .body(Body::from(r#"{"ladder":"single","visibility":"public"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(r.status, StatusCode::ACCEPTED, "{}", r.text());
    assert_eq!(
        (
            r.json()["encoding"]["ladder"].as_str(),
            r.json()["encoding"]["visibility"].as_str()
        ),
        (Some("single"), Some("public"))
    );
    let r = t
        .send(
            Request::builder()
                .method("POST")
                .uri("/storage/media/videos/clip.mp4?action=encode")
                .header("AccessKey", &key)
                .body(Body::from(r#"{"ladder":"nope"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(
        (r.status, r.json()["field"].as_str()),
        (StatusCode::BAD_REQUEST, Some("ladder"))
    );
    let r = t
        .key_req("POST", "/storage/media/videos/clip.mp4?action=nope", &key)
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    let r = t
        .key_req("POST", "/storage/media/videos/clip.mp4?action=sign", &key)
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(
        r.json()["hls"]
            .as_str()
            .unwrap()
            .contains(&format!("/vod/{vod_id}/hls/master.m3u8?token="))
    );

    // X-Rms-Encode: none on overwrite removes the stale encoding; a forced ladder on a
    // non-video name encodes it; unknown ladder rejected before the body.
    let r = t
        .put(
            &key,
            "/storage/media/videos/clip.mp4",
            b"v3",
            &[("X-Rms-Encode", "none")],
        )
        .await;
    assert!(r.json().get("encoding").is_none());
    assert!(vod_repo::get(&t.state.db, &vod_id).await.unwrap().is_none());
    let r = t
        .put(
            &key,
            "/storage/media/raw.bin",
            b"x",
            &[("X-Rms-Encode", "standard")],
        )
        .await;
    assert_eq!(r.json()["encoding"]["ladder"], "standard");
    let raw_vod = r.json()["encoding"]["vod_id"].as_str().unwrap().to_string();
    let r = t
        .put(
            &key,
            "/storage/media/raw2.bin",
            b"x",
            &[("X-Rms-Encode", "nope")],
        )
        .await;
    assert_eq!(
        (r.status, r.json()["field"].as_str()),
        (StatusCode::BAD_REQUEST, Some("X-Rms-Encode"))
    );
    // Without auto_encode a video is not encoded.
    t.call(
        "PATCH",
        "/api/v1/account/buckets/media",
        Some(&key),
        Some(json!({"auto_encode": false})),
    )
    .await;
    let r = t.put(&key, "/storage/media/videos/b.mov", b"x", &[]).await;
    assert!(r.json().get("encoding").is_none());

    // Deleting the file deletes the encoding (row + data/vod/<id>).
    std::fs::create_dir_all(t.state.vod.abr_dir(&raw_vod)).unwrap();
    std::fs::write(
        t.state.vod.abr_dir(&raw_vod).join("master.m3u8"),
        "#EXTM3U\n",
    )
    .unwrap();
    assert_eq!(
        t.key_req("DELETE", "/storage/media/raw.bin", &key)
            .await
            .status,
        StatusCode::NO_CONTENT
    );
    assert!(
        vod_repo::get(&t.state.db, &raw_vod)
            .await
            .unwrap()
            .is_none()
    );
    assert!(!t.state.vod.item_dir(&raw_vod).exists());

    // Recursive directory delete removes the encodings below it.
    t.call(
        "PATCH",
        "/api/v1/account/buckets/media",
        Some(&key),
        Some(json!({"auto_encode": true})),
    )
    .await;
    let r = t
        .put(&key, "/storage/media/videos/deep/c.mp4", b"x", &[])
        .await;
    let deep_vod = r.json()["encoding"]["vod_id"].as_str().unwrap().to_string();
    assert_eq!(
        t.key_req("DELETE", "/storage/media/videos/", &key)
            .await
            .status,
        StatusCode::NO_CONTENT
    );
    assert!(
        vod_repo::get(&t.state.db, &deep_vod)
            .await
            .unwrap()
            .is_none()
    );
    let r = t.key_req("GET", "/storage/media/", &key).await;
    assert_eq!(r.json(), json!([]), "raw2.bin was never stored");
    let r = t
        .call(
            "GET",
            &format!("/api/v1/users/{alice}/encodings"),
            Some(ADMIN),
            None,
        )
        .await;
    assert_eq!(r.json(), json!([]));
}

/// A private, "ready" encoding with fake output files.
async fn private_ready_item(t: &T, key: &str) -> (String, String) {
    let r = t
        .call(
            "POST",
            "/api/v1/account/buckets",
            Some(key),
            Some(json!({"name": "priv", "public_playback": false})),
        )
        .await;
    let signing_key = r.json()["signing_key"].as_str().unwrap().to_string();
    let r = t.put(key, "/storage/priv/v.mp4", b"x", &[]).await;
    assert_eq!(r.json()["encoding"]["visibility"], "private");
    let id = r.json()["encoding"]["vod_id"].as_str().unwrap().to_string();
    let abr = t.state.vod.abr_dir(&id);
    std::fs::create_dir_all(&abr).unwrap();
    std::fs::write(
        abr.join("master.m3u8"),
        "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\nmedia_0.m3u8?v=r1\n",
    )
    .unwrap();
    std::fs::write(
        abr.join("media_0.m3u8"),
        "#EXTM3U\n#EXT-X-MAP:URI=\"init-0.m4s?v=r1\"\n#EXTINF:4.0,\nchunk-0-00001.m4s?v=r1\n#EXT-X-ENDLIST\n",
    )
    .unwrap();
    std::fs::write(
        abr.join("manifest.mpd"),
        r#"<MPD><SegmentTemplate initialization="init-$RepresentationID$.m4s?v=r1" media="chunk-$RepresentationID$-$Number%05d$.m4s?v=r1"/></MPD>"#,
    )
    .unwrap();
    std::fs::write(abr.join("chunk-0-00001.m4s"), b"segment").unwrap();
    std::fs::write(t.state.vod.poster_path(&id), b"jpeg").unwrap();
    vod_repo::claim(&t.state.db, &id).await.unwrap();
    vod_repo::mark_ready(&t.state.db, &id, &[]).await.unwrap();
    t.state.vod.changed(&id);
    (id, signing_key)
}

#[tokio::test]
async fn signed_playback_of_private_encodings() {
    let t = setup(|_| {});
    let (_alice, key) = t.user_with_account_key("alice").await;
    let (id, signing_key) = private_ready_item(&t, &key).await;
    let master = format!("/vod/{id}/hls/master.m3u8");

    // No token → 401 everywhere (JSON); tampered/expired → 401.
    for uri in [
        master.clone(),
        format!("/vod/{id}/hls/chunk-0-00001.m4s"),
        format!("/vod/{id}/dash/manifest.mpd"),
        format!("/vod/{id}/poster.jpg"),
        format!("/api/v1/public/vod/{id}"),
    ] {
        let r = t.call("GET", &uri, None, None).await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED, "{uri}");
        assert!(
            r.json()["error"].as_str().unwrap().contains("private"),
            "{uri}"
        );
    }
    let r = t
        .key_req("POST", "/storage/priv/v.mp4?action=sign", &key)
        .await;
    let signed = r.json();
    let hls = signed["hls"].as_str().unwrap().to_string();
    let q = hls.split_once('?').unwrap().1.to_string();
    let exp = signed["expires"].as_i64().unwrap();
    assert!(exp > chrono::Utc::now().timestamp() + 3500);
    let r = t.call("GET", &format!("{master}?{q}"), None, None).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text());
    assert!(
        r.text().contains(&format!("media_0.m3u8?v=r1&{q}")),
        "{}",
        r.text()
    );
    assert_eq!(r.headers[header::CACHE_CONTROL], "private, no-store");
    let cookie = r.headers[header::SET_COOKIE].to_str().unwrap().to_string();
    assert!(
        cookie.starts_with(&format!("rms_play_{id}="))
            && cookie.contains(&format!("Path=/vod/{id}/")),
        "{cookie}"
    );
    let r = t
        .call(
            "GET",
            &format!("/vod/{id}/hls/media_0.m3u8?{q}"),
            None,
            None,
        )
        .await;
    assert!(
        r.text()
            .contains(&format!("#EXT-X-MAP:URI=\"init-0.m4s?v=r1&{q}\""))
    );
    assert!(r.text().contains(&format!("chunk-0-00001.m4s?v=r1&{q}")));
    let r = t
        .call(
            "GET",
            &format!("/vod/{id}/dash/manifest.mpd?{q}"),
            None,
            None,
        )
        .await;
    assert!(r.text().contains("&amp;token="), "{}", r.text());
    let r = t
        .call(
            "GET",
            &format!("/vod/{id}/hls/chunk-0-00001.m4s?{q}"),
            None,
            None,
        )
        .await;
    assert_eq!(
        (r.status, r.body.as_slice()),
        (StatusCode::OK, &b"segment"[..])
    );
    assert_eq!(
        r.headers[header::CACHE_CONTROL],
        "private, max-age=31536000, immutable"
    );
    // The cookie alone works (same-origin segment requests).
    let cookie_value = cookie.split(';').next().unwrap().to_string();
    let r = t
        .send(
            Request::builder()
                .uri(format!("/vod/{id}/hls/chunk-0-00001.m4s"))
                .header(header::COOKIE, &cookie_value)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(
        r.headers.get(header::SET_COOKIE).is_none(),
        "cookie only set from the query"
    );
    // Tampered and expired tokens.
    let token = sign::token(&signing_key, &id, exp);
    let tampered = format!(
        "{}A&expires={exp}",
        &format!("token={token}")[..token.len() + 5]
    );
    let r = t
        .call("GET", &format!("{master}?{tampered}"), None, None)
        .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    let old = chrono::Utc::now().timestamp() - 1;
    let r = t
        .call(
            "GET",
            &format!(
                "{master}?token={}&expires={old}",
                sign::token(&signing_key, &id, old)
            ),
            None,
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    // Public info with the token: token-bearing URLs.
    let r = t
        .call("GET", &format!("/api/v1/public/vod/{id}?{q}"), None, None)
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.json()["sources"]["hls"].as_str().unwrap().contains(&q));
    assert!(r.json()["sources"]["dash"].as_str().unwrap().contains(&q));
    assert!(r.json()["poster_url"].as_str().unwrap().contains(&q));
    // Account sign endpoint gives the same kind of URLs.
    let r = t
        .call(
            "POST",
            "/api/v1/account/buckets/priv/sign",
            Some(&key),
            Some(json!({"path": "/v.mp4", "expires_secs": 60})),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text());
    assert!(r.json()["expires"].as_i64().unwrap() <= chrono::Utc::now().timestamp() + 60);
    let r = t
        .call(
            "POST",
            "/api/v1/account/buckets/priv/sign",
            Some(&key),
            Some(json!({"path": "/v.mp4", "expires_secs": 0})),
        )
        .await;
    assert_eq!(r.json()["field"], "expires_secs");

    // Rotating the key invalidates every token (cookie included).
    let r = t
        .call(
            "POST",
            "/api/v1/account/buckets/priv/rotate-signing-key",
            Some(&key),
            None,
        )
        .await;
    assert_ne!(r.json()["signing_key"].as_str().unwrap(), signing_key);
    assert_eq!(
        t.call("GET", &format!("{master}?{q}"), None, None)
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    let r = t
        .send(
            Request::builder()
                .uri(format!("/vod/{id}/hls/chunk-0-00001.m4s"))
                .header(header::COOKIE, &cookie_value)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);

    // Made public: no token needed, tokens ignored; public catalogue lists it.
    let r = t
        .call(
            "PATCH",
            &format!("/api/v1/account/encodings/{id}"),
            Some(&key),
            Some(json!({"visibility": "public", "title": "Mine"})),
        )
        .await;
    assert_eq!(
        (r.status, r.json()["visibility"].as_str()),
        (StatusCode::OK, Some("public"))
    );
    let r = t.call("GET", &master, None, None).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.headers[header::CACHE_CONTROL], "public, max-age=60");
    assert!(
        !r.text().contains("token="),
        "public playlists are not rewritten"
    );
    assert_eq!(
        t.call("GET", &format!("{master}?token=x&expires=1"), None, None)
            .await
            .status,
        StatusCode::OK
    );
    let r = t.call("GET", "/api/v1/public/vod", None, None).await;
    assert_eq!(r.json()[0]["title"], "Mine");
    // The admin library cannot make an item private without a bucket.
    let mut lib = vod_repo::VodItem::new(
        "00000000-0000-4000-8000-000000000001",
        "L",
        "x/source.mp4",
        "standard",
        "upload",
    );
    lib.status = "ready".into();
    vod_repo::insert(&t.state.db, &lib).await.unwrap();
    let r = t
        .call(
            "PATCH",
            &format!("/api/v1/vod/{}", lib.id),
            Some(ADMIN),
            Some(json!({"visibility": "private"})),
        )
        .await;
    assert_eq!(
        (r.status, r.json()["field"].as_str()),
        (StatusCode::BAD_REQUEST, Some("visibility"))
    );
}

#[tokio::test]
async fn account_events_only_carry_own_encodings() {
    use futures::StreamExt;
    let t = setup(|_| {});
    let (_a, alice) = t.user_with_account_key("alice").await;
    let (_b, bob) = t.user_with_account_key("bob").await;
    for (k, name) in [(&alice, "amedia"), (&bob, "bmedia")] {
        t.call(
            "POST",
            "/api/v1/account/buckets",
            Some(k),
            Some(json!({"name": name})),
        )
        .await;
    }
    let mine = t
        .put(&alice, "/storage/amedia/a.mp4", b"x", &[])
        .await
        .json()["encoding"]["vod_id"]
        .as_str()
        .unwrap()
        .to_string();
    let theirs = t.put(&bob, "/storage/bmedia/b.mp4", b"x", &[]).await.json()["encoding"]["vod_id"]
        .as_str()
        .unwrap()
        .to_string();
    let mut req = Request::builder()
        .uri("/api/v1/account/events")
        .header("AccessKey", &alice)
        .body(Body::empty())
        .unwrap();
    req.extensions_mut()
        .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 1))));
    let resp = t.app().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let mut body = resp.into_body().into_data_stream();
    let mut next = async || {
        let chunk = tokio::time::timeout(std::time::Duration::from_secs(5), body.next())
            .await
            .expect("an event")
            .unwrap()
            .unwrap();
        String::from_utf8_lossy(&chunk).into_owned()
    };
    assert!(next().await.contains("event: heartbeat"));
    t.state.vod.changed(&theirs);
    t.state.vod.changed(&mine);
    let ev = next().await;
    assert!(
        ev.contains("event: vod") && ev.contains(&mine) && !ev.contains(&theirs),
        "{ev}"
    );
    t.state.vod.delete_item(&theirs).await.unwrap();
    t.state.vod.delete_item(&mine).await.unwrap();
    let ev = next().await;
    assert!(
        ev.contains("event: vod_deleted") && ev.contains(&mine),
        "{ev}"
    );
}
