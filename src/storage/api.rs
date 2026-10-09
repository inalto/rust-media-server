//! The storage API, bunny.net Storage-style (docs/ACCOUNTS.md §3):
//! `/storage/{bucket}/{path...}` with an API key (`AccessKey` header, `Authorization:
//! Bearer`, or the dashboard cookie) or the admin token.
//!
//! | Request | Needs | Response |
//! |---|---|---|
//! | `GET /storage/{b}/` · `GET /storage/{b}/{dir}/` | read | `200 [ObjectView]` (directories first; a missing directory lists empty) |
//! | `GET /storage/{b}/{file}` | read | the file (Range, ETag = `"<sha256>"`, `?download=1` → attachment) |
//! | `GET /storage/{b}/{file}?info=1[&expires_secs=]` | read | `200 ObjectInfo` (+ `signed` URLs when the encoding is private) |
//! | `HEAD /storage/{b}/{file}` | read | headers only |
//! | `PUT /storage/{b}/{file}` | write | `201 ObjectView` (headers `Checksum`, `X-Rms-Encode`, `X-Rms-Visibility`) |
//! | `PUT /storage/{b}/{dir}/` | write | `201 ObjectView` |
//! | `DELETE /storage/{b}/{file}` | write | `204` (file + encoding) |
//! | `DELETE /storage/{b}/{dir}/` | admin | `204` (recursive) |
//! | `POST /storage/{b}/{file}?action=encode` `{ladder?, visibility?}` | write | `202 ObjectView` (with `encoding`) |
//! | `POST /storage/{b}/{file}?action=sign` `{expires_secs?}` | read | `200 SignedUrls` |
//!
//! JSON responses carry `Cache-Control: no-store`, file bodies `private, max-age=0`;
//! CORS is permissive (keys travel in headers). Handlers only parse and check access;
//! the work is in [`crate::storage`].

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Extension, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware;
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use serde::{Deserialize, Serialize};
use tower::ServiceExt;
use tower_http::cors::CorsLayer;
use tower_http::services::ServeFile;

use super::paths::{self, ObjectPath};
use super::{
    EncodeChoice, ObjectView, PutRequest, SignedUrls, object_views, parse_visibility, signed_urls,
    storage_url,
};
use crate::api::BaseUrl;
use crate::api::principal::{Principal, require_principal};
use crate::db::api_keys::Access;
use crate::db::buckets::{self, Bucket};
use crate::db::objects::{self, BucketObject};
use crate::db::vod as vod_repo;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

/// Small JSON bodies of the `?action=` requests.
const ACTION_BODY_MAX: usize = 64 * 1024;

pub fn router(state: &AppState) -> Router<AppState> {
    Router::new()
        .route("/storage/{bucket}", any(handle))
        .route("/storage/{bucket}/", any(handle))
        .route("/storage/{bucket}/{*path}", any(handle))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_principal,
        ))
        .layer(CorsLayer::permissive())
}

/// JSON with `Cache-Control: no-store`.
pub fn json<T: Serialize>(status: StatusCode, value: &T) -> Response {
    let mut r = (status, axum::Json(value)).into_response();
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

/// Bucket name and the raw (percent-encoded) path after it.
fn split_uri_path(uri_path: &str) -> Option<(&str, &str)> {
    let rest = uri_path.strip_prefix("/storage/")?;
    Some(match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    })
}

/// Query parameters of the storage API.
#[derive(Debug, Default, Deserialize)]
struct StorageQuery {
    info: Option<String>,
    download: Option<String>,
    action: Option<String>,
    expires_secs: Option<u64>,
}

fn flag(v: &Option<String>) -> bool {
    v.as_deref()
        .map(|s| s.trim().to_ascii_lowercase())
        .is_some_and(|s| !matches!(s.as_str(), "0" | "false" | "no" | "off"))
}

/// Resolve the bucket and check `need` on the path (403 otherwise).
fn check_access(
    principal: &Principal,
    bucket: &Bucket,
    path: &ObjectPath,
    need: Access,
) -> ApiResult<()> {
    if principal.can(bucket, &path.access_path(), need) {
        Ok(())
    } else {
        Err(ApiError::forbidden(format!(
            "access denied: this key needs {} access to {}",
            need.as_str(),
            path.access_path()
        )))
    }
}

async fn handle(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    req: Request,
) -> Response {
    match dispatch(&state, &principal, req).await {
        Ok(r) => r,
        Err(e) => e.into_response(),
    }
}

async fn dispatch(state: &AppState, principal: &Principal, req: Request) -> ApiResult<Response> {
    let uri_path = req.uri().path().to_string();
    let (bucket_name, raw_path) =
        split_uri_path(&uri_path).ok_or_else(|| ApiError::not_found("not found"))?;
    if !crate::api::account::is_valid_slug(bucket_name, crate::api::account::BUCKET_NAME_MAX) {
        return Err(ApiError::not_found("bucket not found"));
    }
    let bucket = buckets::find_by_name(&state.db, bucket_name)
        .await?
        .ok_or_else(|| ApiError::not_found("bucket not found"))?;
    let path =
        paths::parse_url_path(raw_path).map_err(|e| ApiError::invalid("path", e.to_string()))?;
    let query: StorageQuery = axum::extract::Query::try_from_uri(req.uri())
        .map(|axum::extract::Query(q)| q)
        .map_err(|e| ApiError::bad_request(e.body_text()))?;
    let base = BaseUrl::from_request(&state.config, req.headers());
    let method = req.method().clone();
    match method {
        Method::GET | Method::HEAD => {
            get(state, principal, &bucket, path, &query, &base, req).await
        }
        Method::PUT => put(state, principal, &bucket, path, &base, req).await,
        Method::DELETE => {
            if path.dir {
                check_access(principal, &bucket, &path, Access::Admin)?;
                super::delete_dir(state, &bucket, &path).await?;
            } else {
                check_access(principal, &bucket, &path, Access::Write)?;
                super::delete_file(state, &bucket, &path).await?;
            }
            let mut r = StatusCode::NO_CONTENT.into_response();
            r.headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            Ok(r)
        }
        Method::POST => post(state, principal, &bucket, path, &query, &base, req).await,
        _ => {
            let mut r = ApiError::bad_request("method not allowed").into_response();
            *r.status_mut() = StatusCode::METHOD_NOT_ALLOWED;
            r.headers_mut().insert(
                header::ALLOW,
                HeaderValue::from_static("GET, HEAD, PUT, DELETE, POST, OPTIONS"),
            );
            Ok(r)
        }
    }
}

/// `GET`/`HEAD`: listing, file, or `?info=1`.
async fn get(
    state: &AppState,
    principal: &Principal,
    bucket: &Bucket,
    mut path: ObjectPath,
    query: &StorageQuery,
    base: &BaseUrl,
    req: Request,
) -> ApiResult<Response> {
    let object = if path.is_root() {
        None
    } else {
        objects::get(&state.db, &bucket.id, &path.path).await?
    };
    // `GET /storage/b/videos` of a directory lists it, like `/videos/`.
    if object.as_ref().is_some_and(|o| o.is_dir) {
        path.dir = true;
    }
    if path.dir {
        check_access(principal, bucket, &path, Access::Read)?;
        if object.as_ref().is_some_and(|o| !o.is_dir) {
            return Err(ApiError::not_found("not a directory"));
        }
        let list = objects::list_dir(&state.db, &bucket.id, &path.path).await?;
        let views = object_views(state, base, list).await?;
        return Ok(json(StatusCode::OK, &views));
    }
    check_access(principal, bucket, &path, Access::Read)?;
    let object = object.ok_or_else(|| ApiError::not_found("file not found"))?;
    if flag(&query.info) {
        let secs = super::expires_secs("expires_secs", query.expires_secs)?;
        return Ok(json(
            StatusCode::OK,
            &info(state, bucket, object, base, secs).await?,
        ));
    }
    serve_file(state, bucket, &object, flag(&query.download), req).await
}

/// `?info=1`: the object, its URLs and (private encodings) signed playback URLs.
#[derive(Debug, Clone, Serialize)]
pub struct ObjectInfo {
    #[serde(flatten)]
    pub object: ObjectView,
    pub bucket: BucketRef,
    /// Storage API URL of the file.
    pub url: String,
    pub download_url: String,
    /// Signed playback URLs (private encodings only).
    pub signed: Option<SignedUrls>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BucketRef {
    pub id: String,
    pub name: String,
}

async fn info(
    state: &AppState,
    bucket: &Bucket,
    object: BucketObject,
    base: &BaseUrl,
    secs: u64,
) -> ApiResult<ObjectInfo> {
    let url = storage_url(base, bucket, &object.path, false);
    let view = object_views(state, base, vec![object])
        .await?
        .pop()
        .ok_or_else(|| ApiError::internal("object view missing"))?;
    let signed = view
        .encoding
        .as_ref()
        .filter(|e| e.visibility == vod_repo::VISIBILITY_PRIVATE)
        .map(|e| signed_urls(base, &e.vod_id, bucket, secs));
    Ok(ObjectInfo {
        object: view,
        bucket: BucketRef {
            id: bucket.id.clone(),
            name: bucket.name.clone(),
        },
        download_url: format!("{url}?download=1"),
        url,
        signed,
    })
}

/// `Content-Disposition: attachment` with an RFC 5987 UTF-8 file name.
fn attachment(name: &str) -> Option<HeaderValue> {
    let ascii: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ' ') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let encoded = paths::encode_path(name);
    HeaderValue::from_str(&format!(
        "attachment; filename=\"{ascii}\"; filename*=UTF-8''{encoded}"
    ))
    .ok()
}

/// The original file: Range (ServeFile), ETag, stored content type.
async fn serve_file(
    state: &AppState,
    bucket: &Bucket,
    object: &BucketObject,
    download: bool,
    req: Request,
) -> ApiResult<Response> {
    let file = super::object_file(&state.config, &bucket.id, &object.path);
    if !file.is_file() {
        return Err(ApiError::not_found("file not found"));
    }
    let etag = object
        .sha256
        .as_ref()
        .and_then(|s| HeaderValue::from_str(&format!("\"{s}\"")).ok());
    if let (Some(etag), Some(inm)) = (&etag, req.headers().get(header::IF_NONE_MATCH))
        && inm.to_str().ok().is_some_and(|v| {
            v.split(',')
                .any(|t| t.trim() == etag.to_str().unwrap_or("") || t.trim() == "*")
        })
    {
        let mut r = StatusCode::NOT_MODIFIED.into_response();
        r.headers_mut().insert(header::ETAG, etag.clone());
        r.headers_mut().insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static("private, max-age=0"),
        );
        return Ok(r);
    }
    let resp = match ServeFile::new(&file).oneshot(req).await {
        Ok(r) => r,
        Err(never) => match never {},
    };
    let mut resp = resp.map(Body::new);
    let h = resp.headers_mut();
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=0"),
    );
    if let Some(etag) = etag {
        h.insert(header::ETAG, etag);
    }
    if let Some(ct) = object
        .content_type
        .as_deref()
        .and_then(|c| HeaderValue::from_str(c).ok())
        && resp.status().is_success()
    {
        resp.headers_mut().insert(header::CONTENT_TYPE, ct);
    }
    if download && let Some(v) = attachment(object.name()) {
        resp.headers_mut().insert(header::CONTENT_DISPOSITION, v);
    }
    Ok(resp)
}

/// Options of an upload from its headers (validated before the body is read).
fn put_request(headers: &HeaderMap) -> ApiResult<PutRequest> {
    let text = |name: &str| -> ApiResult<Option<String>> {
        match headers.get(name) {
            None => Ok(None),
            Some(v) => v
                .to_str()
                .map(|s| Some(s.trim().to_string()).filter(|s| !s.is_empty()))
                .map_err(|_| ApiError::invalid(name, format!("{name} is not valid text"))),
        }
    };
    let checksum = match text("checksum")? {
        None => None,
        Some(c) if c.len() == 64 && c.bytes().all(|b| b.is_ascii_hexdigit()) => {
            Some(c.to_ascii_lowercase())
        }
        Some(_) => {
            return Err(ApiError::invalid(
                "Checksum",
                "Checksum must be the SHA-256 of the body (64 hex characters)",
            ));
        }
    };
    let encode = match text("x-rms-encode")? {
        None => EncodeChoice::Auto,
        Some(v) if v.eq_ignore_ascii_case("none") => EncodeChoice::Off,
        Some(v) => EncodeChoice::Ladder(v),
    };
    let visibility = text("x-rms-visibility")?
        .map(|v| parse_visibility("X-Rms-Visibility", &v))
        .transpose()?;
    let content_length = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok());
    Ok(PutRequest {
        encode,
        visibility,
        checksum,
        content_length,
    })
}

/// `PUT`: a file (streamed) or a directory.
async fn put(
    state: &AppState,
    principal: &Principal,
    bucket: &Bucket,
    path: ObjectPath,
    base: &BaseUrl,
    req: Request,
) -> ApiResult<Response> {
    check_access(principal, bucket, &path, Access::Write)?;
    if path.dir {
        let dir = super::create_dir(state, bucket, &path).await?;
        return Ok(json(StatusCode::CREATED, &ObjectView::new(dir, None)));
    }
    let opts = put_request(req.headers())?;
    let outcome = super::put_file(state, bucket, &path, req.into_body(), opts).await?;
    let encoding = outcome
        .encoding
        .as_ref()
        .map(|i| super::EncodingView::new(state, base, i));
    Ok(json(
        StatusCode::CREATED,
        &ObjectView::new(outcome.object, encoding),
    ))
}

#[derive(Debug, Default, Deserialize)]
struct EncodeBody {
    ladder: Option<String>,
    visibility: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct SignBody {
    expires_secs: Option<u64>,
}

/// Optional small JSON body.
async fn small_json<T: Default + serde::de::DeserializeOwned>(body: Body) -> ApiResult<T> {
    let bytes: Bytes = axum::body::to_bytes(body, ACTION_BODY_MAX)
        .await
        .map_err(|_| ApiError::bad_request("request body too large or interrupted"))?;
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(T::default());
    }
    serde_json::from_slice(&bytes)
        .map_err(|e| ApiError::bad_request(format!("invalid JSON body: {e}")))
}

/// `POST ?action=encode|sign`.
async fn post(
    state: &AppState,
    principal: &Principal,
    bucket: &Bucket,
    path: ObjectPath,
    query: &StorageQuery,
    base: &BaseUrl,
    req: Request,
) -> ApiResult<Response> {
    let action = query.action.as_deref().map(str::trim).unwrap_or("");
    let need = match action {
        "encode" => Access::Write,
        "sign" => Access::Read,
        _ => {
            return Err(ApiError::invalid(
                "action",
                "unknown action: use ?action=encode or ?action=sign",
            ));
        }
    };
    check_access(principal, bucket, &path, need)?;
    let object = match objects::get(&state.db, &bucket.id, &path.path).await? {
        Some(o) if !o.is_dir && !path.dir => o,
        Some(_) => return Err(ApiError::conflict("this is a directory, not a file")),
        None => return Err(ApiError::not_found("file not found")),
    };
    if action == "encode" {
        let body: EncodeBody = small_json(req.into_body()).await?;
        let ladder = body
            .ladder
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty());
        let visibility = body
            .visibility
            .as_deref()
            .map(|v| parse_visibility("visibility", v))
            .transpose()?;
        let item = super::encode(state, bucket, &object, ladder, visibility).await?;
        let view = ObjectView::new(object, Some(super::EncodingView::new(state, base, &item)));
        return Ok(json(StatusCode::ACCEPTED, &view));
    }
    let body: SignBody = small_json(req.into_body()).await?;
    let secs = super::expires_secs("expires_secs", body.expires_secs.or(query.expires_secs))?;
    let item = vod_repo::find_by_object(&state.db, &object.id)
        .await?
        .ok_or_else(|| ApiError::conflict("this file has no encoding: use ?action=encode first"))?;
    Ok(json(
        StatusCode::OK,
        &signed_urls(base, &item.id, bucket, secs),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_split_and_headers() {
        assert_eq!(split_uri_path("/storage/media"), Some(("media", "")));
        assert_eq!(split_uri_path("/storage/media/"), Some(("media", "/")));
        assert_eq!(
            split_uri_path("/storage/media/videos/a%20b.mp4"),
            Some(("media", "/videos/a%20b.mp4"))
        );
        assert_eq!(split_uri_path("/other"), None);
        let mut h = HeaderMap::new();
        let r = put_request(&h).unwrap();
        assert_eq!(
            (r.encode, r.visibility, r.checksum),
            (EncodeChoice::Auto, None, None)
        );
        h.insert("x-rms-encode", HeaderValue::from_static("NONE"));
        h.insert("x-rms-visibility", HeaderValue::from_static("Private"));
        h.insert(header::CONTENT_LENGTH, HeaderValue::from_static("42"));
        let sum = "A".repeat(64);
        h.insert("checksum", HeaderValue::from_str(&sum).unwrap());
        let r = put_request(&h).unwrap();
        assert_eq!(r.encode, EncodeChoice::Off);
        assert_eq!(r.visibility.as_deref(), Some("private"));
        assert_eq!(r.checksum.as_deref(), Some("a".repeat(64).as_str()));
        assert_eq!(r.content_length, Some(42));
        h.insert("x-rms-encode", HeaderValue::from_static("mobile"));
        assert_eq!(
            put_request(&h).unwrap().encode,
            EncodeChoice::Ladder("mobile".into())
        );
        h.insert("checksum", HeaderValue::from_static("abc"));
        assert!(matches!(put_request(&h), Err(ApiError::Invalid { .. })));
        h.remove("checksum");
        h.insert("x-rms-visibility", HeaderValue::from_static("secret"));
        assert!(put_request(&h).is_err());
        assert!(flag(&Some("1".into())) && flag(&Some("".into())) && !flag(&Some("0".into())));
        assert!(!flag(&None));
        let a = attachment("été 1.mp4").unwrap();
        assert_eq!(
            a.to_str().unwrap(),
            "attachment; filename=\"_t_ 1.mp4\"; filename*=UTF-8''%C3%A9t%C3%A9%201.mp4"
        );
    }
}
