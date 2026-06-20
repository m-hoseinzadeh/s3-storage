//! JSON + streaming API for the admin panel, under `{prefix}/api/*`.
//!
//! Every handler (except login) is gated by a valid session cookie. Handlers build
//! authenticated [`S3Request`]s and call the shared backend, then translate the
//! `s3s` DTOs to/from JSON. Uploads/downloads stream straight through the backend.

use std::collections::HashMap;

use bytes::Bytes;
use hyper::header::{
    self, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, HeaderValue,
};
use hyper::{HeaderMap, Method, StatusCode};
use s3s::dto::*;
use s3s::{Body, S3, S3Request, S3Response};
use serde::de::DeserializeOwned;

use super::auth::token_from_cookies;
use super::{ApiError, AdminState, finish, json_ok, presign};
use crate::backend::ObjectAttributes;

const JSON_BODY_LIMIT: usize = 8 * 1024 * 1024;

/// Route a `{prefix}/api/...` request and always produce a response.
pub(crate) async fn dispatch(state: &AdminState, req: S3Request<Body>, rel: &str) -> S3Response<Body> {
    let S3Request { input: body, method, uri, headers, .. } = req;
    let query = query_map(&uri);
    let segs: Vec<&str> = rel.trim_start_matches('/').split('/').filter(|s| !s.is_empty()).collect();
    // segs[0] == "api"
    let tail: &[&str] = &segs[1..];

    // Login and logout are the only unauthenticated endpoints.
    match (&method, tail) {
        (&Method::POST, ["login"]) => return finish(login(state, body).await),
        (&Method::POST, ["logout"]) => return finish(Ok(logout(state))),
        _ => {}
    }

    // Everything else requires a valid session.
    if !is_authenticated(state, &headers) {
        return ApiError::unauthorized("login required").into_response();
    }

    let result = match (&method, tail) {
        (&Method::GET, ["session"]) => Ok(json_ok(serde_json::json!({
            "authenticated": true, "access_key": state.access_key,
        }))),
        (&Method::GET, ["config"]) => Ok(config(state)),
        (&Method::GET, ["stats"]) => stats(state).await,

        (&Method::GET, ["buckets"]) => list_buckets(state).await,
        (&Method::POST, ["buckets"]) => create_bucket(state, body).await,
        (&Method::DELETE, ["buckets", bucket]) => delete_bucket(state, &dec(bucket)).await,
        (&Method::GET, ["buckets", bucket, "exists"]) => bucket_exists(state, &dec(bucket)).await,
        (&Method::GET, ["buckets", bucket, "location"]) => bucket_location(state, &dec(bucket)).await,

        (&Method::GET, ["objects"]) => list_objects(state, &query).await,
        (&Method::POST, ["objects", "delete"]) => delete_objects(state, body).await,

        (&Method::GET, ["object", "head"]) => head_object(state, &query).await,
        (&Method::GET, ["object", "get"]) => get_object(state, &query).await,
        (&Method::PUT, ["object", "put"]) => put_object(state, &query, &headers, body).await,
        (&Method::POST, ["object", "copy"]) => copy_object(state, body, false).await,
        (&Method::POST, ["object", "move"]) => copy_object(state, body, true).await,
        (&Method::POST, ["object", "metadata"]) => update_metadata(state, body).await,
        (&Method::GET, ["object", "presign"]) => presign_object(state, &query, &headers),
        (&Method::DELETE, ["object"]) => delete_object(state, &query).await,

        (&Method::POST, ["folder"]) => create_folder(state, body).await,

        (&Method::GET, ["multipart"]) => list_multipart(state, &query).await,
        (&Method::DELETE, ["multipart"]) => abort_multipart(state, &query).await,
        (&Method::GET, ["multipart", "parts"]) => list_parts(state, &query).await,

        _ => Err(ApiError::not_found("unknown admin API endpoint")),
    };
    finish(result)
}

// ---- auth endpoints ----

#[derive(serde::Deserialize)]
struct LoginBody {
    access_key: String,
    secret_key: String,
}

async fn login(state: &AdminState, body: Body) -> Result<S3Response<Body>, ApiError> {
    let creds: LoginBody = read_json(body).await?;
    if !state.sessions.verify_credentials(&creds.access_key, &creds.secret_key) {
        return Err(ApiError::unauthorized("invalid access key or secret key"));
    }
    let token = state.sessions.issue();
    let mut resp = json_ok(serde_json::json!({ "ok": true, "access_key": state.access_key }));
    set_cookie(&mut resp.headers, &state.sessions.set_cookie(&token));
    Ok(resp)
}

fn logout(state: &AdminState) -> S3Response<Body> {
    let mut resp = json_ok(serde_json::json!({ "ok": true }));
    set_cookie(&mut resp.headers, &state.sessions.clear_cookie());
    resp
}

fn is_authenticated(state: &AdminState, headers: &HeaderMap) -> bool {
    headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(token_from_cookies)
        .and_then(|t| state.sessions.verify(t))
        .is_some()
}

// ---- config & stats ----

fn config(state: &AdminState) -> S3Response<Body> {
    json_ok(serde_json::json!({
        "access_key": state.access_key,
        "public_buckets": state.public_buckets,
        "domains": state.domains,
        "domain_map": state.domain_map,
        "admin_path": state.prefix,
        "version": state.version,
    }))
}

async fn stats(state: &AdminState) -> Result<S3Response<Body>, ApiError> {
    let buckets_resp = state.fs.list_buckets(state.s3_request(ListBucketsInput::default())).await?;
    let buckets = buckets_resp.output.buckets.unwrap_or_default();

    let mut total_objects: u64 = 0;
    let mut total_size: u64 = 0;
    let mut bucket_stats = Vec::new();

    for b in buckets {
        let Some(name) = b.name else { continue };
        let (count, size) = bucket_usage(state, &name).await?;
        total_objects += count;
        total_size += size;
        bucket_stats.push(serde_json::json!({
            "name": name,
            "objects": count,
            "size": size,
            "public": state.is_public(&name),
            "creation_date": b.creation_date.as_ref().and_then(ts_iso),
        }));
    }

    Ok(json_ok(serde_json::json!({
        "bucket_count": bucket_stats.len(),
        "object_count": total_objects,
        "total_size": total_size,
        "public_bucket_count": state.public_buckets.len(),
        "buckets": bucket_stats,
    })))
}

/// Sum object count and bytes for a bucket (paginated, capped to avoid runaway).
async fn bucket_usage(state: &AdminState, bucket: &str) -> Result<(u64, u64), ApiError> {
    let mut count: u64 = 0;
    let mut size: u64 = 0;
    let mut token: Option<String> = None;
    for _ in 0..1000 {
        let input = ListObjectsV2Input {
            bucket: bucket.to_owned(),
            max_keys: Some(1000),
            continuation_token: token.clone(),
            ..Default::default()
        };
        let out = state.fs.list_objects_v2(state.s3_request(input)).await?.output;
        for obj in out.contents.unwrap_or_default() {
            count += 1;
            size += u64::try_from(obj.size.unwrap_or(0)).unwrap_or(0);
        }
        if out.is_truncated == Some(true) {
            token = out.next_continuation_token;
            if token.is_none() {
                break;
            }
        } else {
            break;
        }
    }
    Ok((count, size))
}

// ---- bucket endpoints ----

async fn list_buckets(state: &AdminState) -> Result<S3Response<Body>, ApiError> {
    let out = state.fs.list_buckets(state.s3_request(ListBucketsInput::default())).await?.output;
    let buckets: Vec<_> = out
        .buckets
        .unwrap_or_default()
        .into_iter()
        .filter_map(|b| {
            let name = b.name?;
            Some(serde_json::json!({
                "name": name,
                "creation_date": b.creation_date.as_ref().and_then(ts_iso),
                "public": state.is_public(&name),
            }))
        })
        .collect();
    Ok(json_ok(serde_json::json!({ "buckets": buckets })))
}

#[derive(serde::Deserialize)]
struct NameBody {
    name: String,
}

async fn create_bucket(state: &AdminState, body: Body) -> Result<S3Response<Body>, ApiError> {
    let b: NameBody = read_json(body).await?;
    if b.name.trim().is_empty() {
        return Err(ApiError::bad_request("bucket name is required"));
    }
    let input = CreateBucketInput { bucket: b.name.clone(), ..Default::default() };
    state.fs.create_bucket(state.s3_request(input)).await?;
    Ok(json_ok(serde_json::json!({ "ok": true, "name": b.name })))
}

async fn delete_bucket(state: &AdminState, bucket: &str) -> Result<S3Response<Body>, ApiError> {
    let input = DeleteBucketInput { bucket: bucket.to_owned(), ..Default::default() };
    state.fs.delete_bucket(state.s3_request(input)).await?;
    Ok(json_ok(serde_json::json!({ "ok": true })))
}

async fn bucket_exists(state: &AdminState, bucket: &str) -> Result<S3Response<Body>, ApiError> {
    let input = HeadBucketInput { bucket: bucket.to_owned(), ..Default::default() };
    let exists = state.fs.head_bucket(state.s3_request(input)).await.is_ok();
    Ok(json_ok(serde_json::json!({ "exists": exists })))
}

async fn bucket_location(state: &AdminState, bucket: &str) -> Result<S3Response<Body>, ApiError> {
    let input = GetBucketLocationInput { bucket: bucket.to_owned(), ..Default::default() };
    let out = state.fs.get_bucket_location(state.s3_request(input)).await?.output;
    Ok(json_ok(serde_json::json!({
        "location": out.location_constraint.map(|c| c.as_str().to_owned()).unwrap_or_default(),
    })))
}

// ---- object listing & browse ----

async fn list_objects(state: &AdminState, q: &Query) -> Result<S3Response<Body>, ApiError> {
    let bucket = q.require("bucket")?;
    let input = ListObjectsV2Input {
        bucket,
        prefix: q.opt("prefix"),
        delimiter: q.opt("delimiter"),
        continuation_token: q.opt("token"),
        start_after: q.opt("start_after"),
        max_keys: q.opt("max").and_then(|s| s.parse::<i32>().ok()).or(Some(1000)),
        ..Default::default()
    };
    let out = state.fs.list_objects_v2(state.s3_request(input)).await?.output;

    let objects: Vec<_> = out
        .contents
        .unwrap_or_default()
        .into_iter()
        .map(|o| {
            serde_json::json!({
                "key": o.key,
                "size": o.size,
                "last_modified": o.last_modified.as_ref().and_then(ts_iso),
                "etag": o.e_tag,
            })
        })
        .collect();
    let prefixes: Vec<_> = out
        .common_prefixes
        .unwrap_or_default()
        .into_iter()
        .filter_map(|p| p.prefix)
        .collect();

    Ok(json_ok(serde_json::json!({
        "objects": objects,
        "prefixes": prefixes,
        "is_truncated": out.is_truncated.unwrap_or(false),
        "next_token": out.next_continuation_token,
        "key_count": out.key_count,
    })))
}

// ---- object metadata / head ----

async fn head_object(state: &AdminState, q: &Query) -> Result<S3Response<Body>, ApiError> {
    let bucket = q.require("bucket")?;
    let key = q.require("key")?;
    let input = HeadObjectInput { bucket, key, ..Default::default() };
    let out = state.fs.head_object(state.s3_request(input)).await?.output;
    Ok(json_ok(serde_json::json!({
        "content_type": out.content_type,
        "content_length": out.content_length,
        "last_modified": out.last_modified.as_ref().and_then(ts_iso),
        "etag": out.e_tag,
        "cache_control": out.cache_control,
        "content_disposition": out.content_disposition,
        "content_encoding": out.content_encoding,
        "content_language": out.content_language,
        "expires": out.expires.as_ref().and_then(ts_iso),
        "metadata": out.metadata,
        "checksums": {
            "crc32": out.checksum_crc32,
            "crc32c": out.checksum_crc32c,
            "crc64nvme": out.checksum_crc64nvme,
            "sha1": out.checksum_sha1,
            "sha256": out.checksum_sha256,
        },
    })))
}

// ---- download ----

async fn get_object(state: &AdminState, q: &Query) -> Result<S3Response<Body>, ApiError> {
    let bucket = q.require("bucket")?;
    let key = q.require("key")?;
    let input = GetObjectInput { bucket, key: key.clone(), range: parse_range(q.opt("range")), ..Default::default() };
    let out = state.fs.get_object(state.s3_request(input)).await?.output;

    let body: Body = out.body.map(Into::into).unwrap_or_else(Body::empty);
    let mut resp = S3Response::new(body);
    let h = &mut resp.headers;
    set_str(h, CONTENT_TYPE, out.content_type.as_deref().or(Some("application/octet-stream")));
    if let Some(len) = out.content_length {
        set_str(h, CONTENT_LENGTH, Some(&len.to_string()));
    }
    let etag_value = out.e_tag.as_ref().map(|e| format!("\"{}\"", e.value()));
    set_str(h, header::ETAG, etag_value.as_deref());
    set_str(h, header::ACCEPT_RANGES, Some("bytes"));
    let filename = key.rsplit('/').next().unwrap_or("download");
    set_str(h, CONTENT_DISPOSITION, Some(&format!("attachment; filename=\"{}\"", filename.replace('"', ""))));
    if let Some(range) = out.content_range.as_deref() {
        set_str(h, CONTENT_RANGE, Some(range));
        resp.status = Some(StatusCode::PARTIAL_CONTENT);
    }
    Ok(resp)
}

// ---- upload ----

async fn put_object(
    state: &AdminState,
    q: &Query,
    headers: &HeaderMap,
    body: Body,
) -> Result<S3Response<Body>, ApiError> {
    let bucket = q.require("bucket")?;
    let key = q.require("key")?;
    let content_length = headers.get(CONTENT_LENGTH).and_then(|v| v.to_str().ok()).and_then(|s| s.parse::<i64>().ok());
    let input = PutObjectInput {
        bucket,
        key,
        body: Some(body.into()),
        content_type: q.opt("content_type"),
        content_length,
        ..Default::default()
    };
    let out = state.fs.put_object(state.s3_request(input)).await?.output;
    Ok(json_ok(serde_json::json!({ "ok": true, "etag": out.e_tag })))
}

// ---- copy / move ----

#[derive(serde::Deserialize)]
struct CopyBody {
    src_bucket: String,
    src_key: String,
    dst_bucket: String,
    dst_key: String,
}

async fn copy_object(state: &AdminState, body: Body, remove_source: bool) -> Result<S3Response<Body>, ApiError> {
    let c: CopyBody = read_json(body).await?;
    let input = CopyObjectInput::builder()
        .bucket(c.dst_bucket.clone())
        .key(c.dst_key.clone())
        .copy_source(CopySource::Bucket {
            bucket: c.src_bucket.clone().into(),
            key: c.src_key.clone().into(),
            version_id: None,
        })
        .build()
        .map_err(|e| ApiError::bad_request(format!("invalid copy request: {e}")))?;
    state.fs.copy_object(state.s3_request(input)).await?;

    if remove_source {
        let del = DeleteObjectInput { bucket: c.src_bucket, key: c.src_key, ..Default::default() };
        state.fs.delete_object(state.s3_request(del)).await?;
    }
    Ok(json_ok(serde_json::json!({ "ok": true })))
}

// ---- metadata update ----

#[derive(serde::Deserialize)]
struct MetadataBody {
    bucket: String,
    key: String,
    #[serde(default)]
    content_type: Option<String>,
    #[serde(default)]
    cache_control: Option<String>,
    #[serde(default)]
    content_disposition: Option<String>,
    #[serde(default)]
    content_encoding: Option<String>,
    #[serde(default)]
    content_language: Option<String>,
    #[serde(default)]
    expires: Option<String>,
    #[serde(default)]
    metadata: Option<HashMap<String, String>>,
}

async fn update_metadata(state: &AdminState, body: Body) -> Result<S3Response<Body>, ApiError> {
    let m: MetadataBody = read_json(body).await?;
    // The object must exist before we (re)write its metadata sidecar.
    let head = HeadObjectInput { bucket: m.bucket.clone(), key: m.key.clone(), ..Default::default() };
    state.fs.head_object(state.s3_request(head)).await?;

    let attrs = ObjectAttributes {
        user_metadata: m.metadata.filter(|map| !map.is_empty()).map(|map| map.into_iter().collect()),
        content_type: m.content_type,
        content_encoding: m.content_encoding,
        content_disposition: m.content_disposition,
        content_language: m.content_language,
        cache_control: m.cache_control,
        expires: m.expires,
        website_redirect_location: None,
    };
    state
        .fs
        .save_object_attributes(&m.bucket, &m.key, &attrs, None)
        .await
        .map_err(|e| ApiError::internal(format!("failed to save metadata: {e:?}")))?;
    Ok(json_ok(serde_json::json!({ "ok": true })))
}

// ---- presign ----

fn presign_object(state: &AdminState, q: &Query, headers: &HeaderMap) -> Result<S3Response<Body>, ApiError> {
    let bucket = q.require("bucket")?;
    let key = q.require("key")?;
    let method = q.opt("method").unwrap_or_else(|| "GET".to_owned()).to_uppercase();
    if method != "GET" && method != "PUT" {
        return Err(ApiError::bad_request("method must be GET or PUT"));
    }
    let expires = q.opt("expires").and_then(|s| s.parse::<u64>().ok()).unwrap_or(3600).clamp(1, 604_800);
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| ApiError::bad_request("missing Host header"))?;
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("http");

    let url = presign::presign(&state.access_key, &state.secret_key, scheme, host, &bucket, &key, &method, expires);
    Ok(json_ok(serde_json::json!({ "url": url, "expires_in": expires, "method": method })))
}

// ---- delete (single + batch) ----

async fn delete_object(state: &AdminState, q: &Query) -> Result<S3Response<Body>, ApiError> {
    let bucket = q.require("bucket")?;
    let key = q.require("key")?;
    let input = DeleteObjectInput { bucket, key, ..Default::default() };
    state.fs.delete_object(state.s3_request(input)).await?;
    Ok(json_ok(serde_json::json!({ "ok": true })))
}

#[derive(serde::Deserialize)]
struct BatchDeleteBody {
    bucket: String,
    keys: Vec<String>,
}

async fn delete_objects(state: &AdminState, body: Body) -> Result<S3Response<Body>, ApiError> {
    let b: BatchDeleteBody = read_json(body).await?;
    let objects: Vec<ObjectIdentifier> =
        b.keys.into_iter().map(|key| ObjectIdentifier { key, ..Default::default() }).collect();
    let input = DeleteObjectsInput::builder()
        .bucket(b.bucket)
        .delete(Delete { objects, quiet: Some(false) })
        .build()
        .map_err(|e| ApiError::bad_request(format!("invalid delete request: {e}")))?;
    let out = state.fs.delete_objects(state.s3_request(input)).await?.output;
    let deleted: Vec<_> = out.deleted.unwrap_or_default().into_iter().filter_map(|d| d.key).collect();
    Ok(json_ok(serde_json::json!({ "ok": true, "deleted": deleted })))
}

// ---- folder ----

#[derive(serde::Deserialize)]
struct FolderBody {
    bucket: String,
    prefix: String,
}

async fn create_folder(state: &AdminState, body: Body) -> Result<S3Response<Body>, ApiError> {
    let f: FolderBody = read_json(body).await?;
    let mut key = f.prefix;
    if !key.ends_with('/') {
        key.push('/');
    }
    let input = PutObjectInput { bucket: f.bucket, key, body: Some(Body::empty().into()), ..Default::default() };
    state.fs.put_object(state.s3_request(input)).await?;
    Ok(json_ok(serde_json::json!({ "ok": true })))
}

// ---- multipart ----

async fn list_multipart(state: &AdminState, q: &Query) -> Result<S3Response<Body>, ApiError> {
    let filter = q.opt("bucket");
    let uploads = state
        .fs
        .list_multipart_uploads()
        .await
        .map_err(|e| ApiError::internal(format!("failed to scan uploads: {e:?}")))?;
    let uploads: Vec<_> = uploads
        .into_iter()
        .filter(|u| filter.as_ref().is_none_or(|b| u.bucket.as_deref() == Some(b.as_str())))
        .map(|u| {
            serde_json::json!({
                "upload_id": u.upload_id,
                "bucket": u.bucket,
                "key": u.key,
                "initiated": u.initiated_unix,
            })
        })
        .collect();
    Ok(json_ok(serde_json::json!({ "uploads": uploads })))
}

async fn list_parts(state: &AdminState, q: &Query) -> Result<S3Response<Body>, ApiError> {
    let bucket = q.require("bucket")?;
    let key = q.require("key")?;
    let upload_id = q.require("upload_id")?;
    let input = ListPartsInput { bucket, key, upload_id, ..Default::default() };
    let out = state.fs.list_parts(state.s3_request(input)).await?.output;
    let parts: Vec<_> = out
        .parts
        .unwrap_or_default()
        .into_iter()
        .map(|p| {
            serde_json::json!({
                "part_number": p.part_number,
                "size": p.size,
                "etag": p.e_tag,
                "last_modified": p.last_modified.as_ref().and_then(ts_iso),
            })
        })
        .collect();
    Ok(json_ok(serde_json::json!({ "parts": parts })))
}

async fn abort_multipart(state: &AdminState, q: &Query) -> Result<S3Response<Body>, ApiError> {
    let bucket = q.require("bucket")?;
    let key = q.require("key")?;
    let upload_id = q.require("upload_id")?;
    let input = AbortMultipartUploadInput { bucket, key, upload_id, ..Default::default() };
    state.fs.abort_multipart_upload(state.s3_request(input)).await?;
    Ok(json_ok(serde_json::json!({ "ok": true })))
}

// ---- helpers ----

/// Parsed query string with small ergonomic accessors.
struct Query(HashMap<String, String>);

impl Query {
    fn opt(&self, key: &str) -> Option<String> {
        self.0.get(key).filter(|s| !s.is_empty()).cloned()
    }
    fn require(&self, key: &str) -> Result<String, ApiError> {
        self.opt(key).ok_or_else(|| ApiError::bad_request(format!("missing query parameter `{key}`")))
    }
}

fn query_map(uri: &hyper::Uri) -> Query {
    let mut map = HashMap::new();
    if let Some(q) = uri.query() {
        for pair in q.split('&') {
            if let Some((k, v)) = pair.split_once('=') {
                map.insert(percent_decode(k), percent_decode(v));
            } else if !pair.is_empty() {
                map.insert(percent_decode(pair), String::new());
            }
        }
    }
    Query(map)
}

fn dec(s: &str) -> String {
    percent_decode(s)
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2]))
        {
            out.push(h * 16 + l);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn parse_range(range: Option<String>) -> Option<Range> {
    Range::parse(range?.as_str()).ok()
}

fn ts_iso(ts: &Timestamp) -> Option<serde_json::Value> {
    let mut buf = Vec::new();
    ts.format(TimestampFormat::DateTime, &mut buf).ok()?;
    String::from_utf8(buf).ok().map(serde_json::Value::String)
}

async fn read_json<T: DeserializeOwned>(body: Body) -> Result<T, ApiError> {
    let bytes = read_body(body).await?;
    serde_json::from_slice(&bytes).map_err(|e| ApiError::bad_request(format!("invalid JSON body: {e}")))
}

async fn read_body(mut body: Body) -> Result<Bytes, ApiError> {
    body.store_all_limited(JSON_BODY_LIMIT).await.map_err(|e| ApiError::bad_request(format!("failed to read body: {e}")))
}

fn set_str(headers: &mut HeaderMap, name: header::HeaderName, value: Option<&str>) {
    if let Some(v) = value
        && let Ok(hv) = HeaderValue::from_str(v)
    {
        headers.insert(name, hv);
    }
}

fn set_cookie(headers: &mut HeaderMap, value: &str) {
    if let Ok(hv) = HeaderValue::from_str(value) {
        headers.insert(header::SET_COOKIE, hv);
    }
}
