//! JSON + streaming API for the admin panel, under `/api/*`.
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
use crate::settings::SettingsUpdate;

const JSON_BODY_LIMIT: usize = 8 * 1024 * 1024;

/// Route an `/api/...` request and always produce a response.
pub(crate) async fn dispatch(state: &AdminState, req: S3Request<Body>, rel: &str) -> S3Response<Body> {
    let S3Request { input: body, method, uri, headers, .. } = req;
    let query = query_map(&uri);
    let segs: Vec<&str> = rel.trim_start_matches('/').split('/').filter(|s| !s.is_empty()).collect();
    // segs[0] == "api"
    let tail: &[&str] = &segs[1..];

    // Reject browser-initiated cross-site writes before anything else runs.
    if let Err(err) = check_same_origin(&method, &headers, &uri) {
        return err.into_response();
    }

    // Login and logout are the only unauthenticated endpoints.
    let secure = request_is_secure(state, &headers, &uri);
    match (&method, tail) {
        (&Method::POST, ["login"]) => return finish(login(state, &headers, body, secure).await),
        (&Method::POST, ["logout"]) => return finish(Ok(logout(state, secure))),
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
        (&Method::PUT, ["settings"]) => update_settings(state, &headers, body).await,
        (&Method::GET, ["stats"]) => stats(state).await,

        (&Method::GET, ["buckets"]) => list_buckets(state).await,
        (&Method::POST, ["buckets"]) => create_bucket(state, &headers, body).await,
        (&Method::DELETE, ["buckets", bucket]) => delete_bucket(state, &dec(bucket)).await,
        (&Method::GET, ["buckets", bucket, "exists"]) => bucket_exists(state, &dec(bucket)).await,
        (&Method::GET, ["buckets", bucket, "location"]) => bucket_location(state, &dec(bucket)).await,

        (&Method::GET, ["objects"]) => list_objects(state, &query).await,
        (&Method::POST, ["objects", "delete"]) => delete_objects(state, &headers, body).await,

        (&Method::GET, ["object", "head"]) => head_object(state, &query).await,
        (&Method::GET, ["object", "get"]) => get_object(state, &query).await,
        (&Method::PUT, ["object", "put"]) => put_object(state, &query, &headers, body).await,
        (&Method::POST, ["object", "copy"]) => copy_object(state, &headers, body, false).await,
        (&Method::POST, ["object", "move"]) => copy_object(state, &headers, body, true).await,
        (&Method::POST, ["object", "extract"]) => extract_object(state, &headers, body).await,
        (&Method::POST, ["object", "metadata"]) => update_metadata(state, &headers, body).await,
        (&Method::GET, ["object", "presign"]) => presign_object(state, &query),
        (&Method::DELETE, ["object"]) => delete_object(state, &query).await,

        (&Method::POST, ["folder"]) => create_folder(state, &headers, body).await,

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

async fn login(state: &AdminState, headers: &HeaderMap, body: Body, secure: bool) -> Result<S3Response<Body>, ApiError> {
    let creds: LoginBody = read_json(headers, body).await?;
    // Rate-limited: a failure streak opens a cooldown during which attempts are
    // refused outright, so the endpoint cannot be brute-forced at speed or in
    // parallel. The check runs inside the throttle so the two cannot race.
    let verified = state
        .login_throttle
        .attempt(|| state.sessions.verify_credentials(&creds.access_key, &creds.secret_key));
    match verified {
        Err(retry_after) => return Err(ApiError::too_many_requests(retry_after)),
        Ok(false) => return Err(ApiError::unauthorized("invalid access key or secret key")),
        Ok(true) => {}
    }
    let token = state.sessions.issue();
    let mut resp = json_ok(serde_json::json!({ "ok": true, "access_key": state.access_key }));
    set_cookie(&mut resp.headers, &state.sessions.set_cookie(&token, secure));
    Ok(resp)
}

fn logout(state: &AdminState, secure: bool) -> S3Response<Body> {
    let mut resp = json_ok(serde_json::json!({ "ok": true }));
    set_cookie(&mut resp.headers, &state.sessions.clear_cookie(secure));
    resp
}

/// Reject a state-changing request that a browser initiated from another site.
///
/// The session cookie is `HttpOnly; SameSite=Strict`, but "site" means the
/// registrable domain, not the origin: a page served from a public bucket at
/// `files.example.com` counts as same-site with an admin panel at
/// `admin.example.com`, so the session cookie rides along with its requests. Public
/// buckets exist precisely to serve caller-supplied HTML -- the ZIP extract feature
/// unpacks uploaded static sites -- so that page is attacker-controlled content, and
/// `SameSite` alone does not keep it away from these handlers.
///
/// A request carrying no `Origin` is a non-browser client (curl, a script, an SDK)
/// and is allowed through: every current browser sends `Origin` on a non-safe
/// method, so its absence is not something a page can arrange.
fn check_same_origin(method: &Method, headers: &HeaderMap, uri: &hyper::Uri) -> Result<(), ApiError> {
    if matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS) {
        return Ok(());
    }
    let refuse = || {
        ApiError::new(
            StatusCode::FORBIDDEN,
            "CrossOrigin",
            "cross-origin request rejected: the admin API only accepts writes from its own origin",
        )
    };

    // `Sec-Fetch-Site` is decisive where the browser sends it. `same-site` is
    // exactly the sibling-subdomain case `SameSite=Strict` lets through.
    if let Some(site) = headers.get("sec-fetch-site").and_then(|v| v.to_str().ok()) {
        let site = site.trim();
        if !site.eq_ignore_ascii_case("same-origin") && !site.eq_ignore_ascii_case("none") {
            return Err(refuse());
        }
    }

    let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) else {
        return Ok(());
    };
    // Compare authorities only: behind a TLS-terminating proxy the scheme we see is
    // not the one the browser used. An opaque origin ("null") matches nothing.
    let origin_authority = origin.split_once("://").map_or(origin, |(_, rest)| rest);
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        // HTTP/2 carries the authority in `:authority` rather than a `Host` header.
        .or_else(|| uri.authority().map(hyper::http::uri::Authority::as_str))
        .unwrap_or_default();
    if host.is_empty() || !origin_authority.eq_ignore_ascii_case(host) {
        return Err(refuse());
    }
    Ok(())
}

/// Whether the original client request reached the server over HTTPS.
///
/// Drives whether the session cookie gets `Secure`. `X-Forwarded-Proto` (possibly a
/// comma-separated proxy chain whose first entry is the client) is consulted only
/// when the operator has declared a trusted proxy via `--trust-proxy`: it is an
/// ordinary request header, so with the server directly reachable any client could
/// otherwise dictate whether its own session cookie is protected.
fn request_is_secure(state: &AdminState, headers: &HeaderMap, uri: &hyper::Uri) -> bool {
    if state.trust_proxy
        && let Some(proto) = headers.get("x-forwarded-proto").and_then(|v| v.to_str().ok())
        && let Some(first) = proto.split(',').next()
    {
        return first.trim().eq_ignore_ascii_case("https");
    }
    uri.scheme_str() == Some("https")
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
    let s = state.settings.snapshot();
    json_ok(serde_json::json!({
        "access_key": state.access_key,
        "public_buckets": s.public_buckets,
        "domains": s.domains,
        "domain_map": s.domain_map,
        "allowed_origins": s.allowed_origins,
        "api_public_url": s.api_public_url,
        "admin_session_ttl_secs": s.admin_session_ttl_secs,
        "admin_path": "/",
        "version": state.version,
    }))
}

async fn update_settings(state: &AdminState, headers: &HeaderMap, body: Body) -> Result<S3Response<Body>, ApiError> {
    let upd: SettingsUpdate = read_json(headers, body).await?;
    upd.validate().map_err(ApiError::bad_request)?;
    // rusqlite is blocking; run the transaction off the async runtime. The store is
    // an `Arc`, so cloning the handle into the blocking task is cheap.
    let settings = std::sync::Arc::clone(&state.settings);
    tokio::task::spawn_blocking(move || settings.update(&upd))
        .await
        .map_err(|e| ApiError::internal(format!("settings update task failed: {e}")))?
        .map_err(|e| ApiError::internal(format!("failed to persist settings: {e}")))?;
    Ok(json_ok(serde_json::json!({ "ok": true })))
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
        "public_bucket_count": state.settings.snapshot().public_buckets.len(),
        "buckets": bucket_stats,
    })))
}

/// Sum object count and bytes for a bucket.
///
/// This walks the bucket directory once. It used to page through `ListObjectsV2`
/// instead, but each of those pages re-walks the whole bucket, so a dashboard load
/// cost roughly `objects^2 / 1000` directory entries -- and silently under-reported
/// past a million objects, where the page loop gave up.
async fn bucket_usage(state: &AdminState, bucket: &str) -> Result<(u64, u64), ApiError> {
    state
        .fs
        .bucket_usage(bucket)
        .await
        .map_err(|e| ApiError::internal(format!("failed to measure bucket `{bucket}`: {e:?}")))
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

/// Reject a bucket name that does not satisfy the AWS naming rules.
///
/// The S3 ports validate bucket names while parsing the request, but the admin
/// panel calls the backend directly, so every handler that takes a bucket name has
/// to enforce the same rules itself. Skipping this on a read/delete path is not
/// merely cosmetic: a name such as `.s3-storage` resolves to an internal directory
/// under the data root, so an unchecked `DeleteBucket` would wipe the settings
/// database. The backend guards this too; checking here turns a 500 into a clear
/// 400 that names the problem.
fn check_bucket(bucket: &str) -> Result<(), ApiError> {
    if bucket.trim().is_empty() {
        return Err(ApiError::bad_request("bucket name is required"));
    }
    if !s3s::path::check_bucket_name(bucket) {
        return Err(ApiError::bad_request(
            "invalid bucket name (3-63 chars: lowercase letters, digits, '.', '-'; \
             must start and end alphanumeric, no '..', not an IP)",
        ));
    }
    Ok(())
}

async fn create_bucket(state: &AdminState, headers: &HeaderMap, body: Body) -> Result<S3Response<Body>, ApiError> {
    let b: NameBody = read_json(headers, body).await?;
    check_bucket(&b.name)?;
    let input = CreateBucketInput { bucket: b.name.clone(), ..Default::default() };
    state.fs.create_bucket(state.s3_request(input)).await?;
    Ok(json_ok(serde_json::json!({ "ok": true, "name": b.name })))
}

async fn delete_bucket(state: &AdminState, bucket: &str) -> Result<S3Response<Body>, ApiError> {
    check_bucket(bucket)?;
    let input = DeleteBucketInput { bucket: bucket.to_owned(), ..Default::default() };
    state.fs.delete_bucket(state.s3_request(input)).await?;
    Ok(json_ok(serde_json::json!({ "ok": true })))
}

async fn bucket_exists(state: &AdminState, bucket: &str) -> Result<S3Response<Body>, ApiError> {
    check_bucket(bucket)?;
    let input = HeadBucketInput { bucket: bucket.to_owned(), ..Default::default() };
    let exists = state.fs.head_bucket(state.s3_request(input)).await.is_ok();
    Ok(json_ok(serde_json::json!({ "exists": exists })))
}

async fn bucket_location(state: &AdminState, bucket: &str) -> Result<S3Response<Body>, ApiError> {
    check_bucket(bucket)?;
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

async fn copy_object(
    state: &AdminState,
    headers: &HeaderMap,
    body: Body,
    remove_source: bool,
) -> Result<S3Response<Body>, ApiError> {
    let c: CopyBody = read_json(headers, body).await?;
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

// ---- archive extraction ----

/// Largest compressed archive we will read back from storage into memory (1 GiB).
const MAX_ZIP_BYTES: usize = 1024 * 1024 * 1024;
/// Cap on the total decompressed payload, enforced against *actual* bytes read
/// (not the archive's self-declared sizes) so a zip bomb can't blow up memory.
const MAX_TOTAL_UNCOMPRESSED: u64 = 4 * 1024 * 1024 * 1024;
/// Cap on the number of files a single archive may expand into.
const MAX_ENTRIES: usize = 50_000;
/// Decompressed entries buffered between the blocking inflate and the writer. Kept
/// tiny on purpose: it is what keeps peak memory at a couple of files rather than
/// the whole expanded archive.
const EXTRACT_QUEUE_DEPTH: usize = 1;

#[derive(serde::Deserialize)]
struct ExtractBody {
    bucket: String,
    key: String,
    /// Where extracted files land, e.g. `site/`. Defaults to the bucket root.
    #[serde(default)]
    dest_prefix: Option<String>,
    /// Replace objects that already exist; when false (default) they are skipped.
    #[serde(default)]
    overwrite: bool,
}

/// A single decompressed file, named with the destination prefix already applied.
struct ZipEntry {
    name: String,
    data: Vec<u8>,
}

/// Extract a stored `.zip` object into individual objects in the same bucket.
///
/// The archive is read back from storage, decompressed off the async runtime, and
/// each file entry is written as its own object under `dest_prefix`. Directory
/// entries become implicit S3 prefixes (we never write the folder placeholders).
/// Paths are validated against zip-slip and bounded by the size/count caps above.
///
/// Entries are streamed from the inflate task to the writer over a depth-1 channel
/// and written as they arrive. Collecting them all first meant `MAX_TOTAL_UNCOMPRESSED`
/// was not a guard against a zip bomb blowing up memory -- it *was* the ceiling, 4 GiB
/// of resident entries on top of the 1 GiB archive.
async fn extract_object(state: &AdminState, headers: &HeaderMap, body: Body) -> Result<S3Response<Body>, ApiError> {
    let b: ExtractBody = read_json(headers, body).await?;
    if b.key.ends_with('/') {
        return Err(ApiError::bad_request("the selected key is a folder, not an archive"));
    }

    // Pull the archive object back out of storage.
    let input = GetObjectInput { bucket: b.bucket.clone(), key: b.key.clone(), ..Default::default() };
    let out = state.fs.get_object(state.s3_request(input)).await?.output;
    let blob = out.body.ok_or_else(|| ApiError::internal("archive object has no body"))?;
    let mut blob_body: Body = blob.into();
    let archive = blob_body.store_all_limited(MAX_ZIP_BYTES).await.map_err(|_| {
        ApiError::bad_request(format!(
            "archive exceeds the {} MiB extraction limit",
            MAX_ZIP_BYTES / (1024 * 1024)
        ))
    })?;

    // Normalise the destination prefix: never absolute, always ends with '/'.
    let dest = b.dest_prefix.unwrap_or_default();
    let dest = dest.trim_start_matches('/').to_owned();
    let dest = if dest.is_empty() || dest.ends_with('/') { dest } else { format!("{dest}/") };

    // ZIP parsing + inflate is blocking and CPU-bound; keep it off the runtime, and
    // hand entries over one at a time so only the file being written is resident.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<ZipEntry>(EXTRACT_QUEUE_DEPTH);
    let dest_for_task = dest.clone();
    let inflate = tokio::task::spawn_blocking(move || decompress_zip(&archive, &dest_for_task, &tx));

    let mut extracted = Vec::new();
    let mut skipped = Vec::new();
    let mut write_err = None;
    while let Some(entry) = rx.recv().await {
        match write_zip_entry(state, &b.bucket, entry, b.overwrite).await {
            Ok((name, true)) => extracted.push(name),
            Ok((name, false)) => skipped.push(name),
            Err(e) => {
                write_err = Some(e);
                break;
            }
        }
    }
    // Dropping the receiver unblocks the inflate task if we bailed out early.
    drop(rx);
    let produced = inflate
        .await
        .map_err(|e| ApiError::internal(format!("extraction task failed: {e}")))?;

    // A write failure is the more useful diagnosis, so it wins over the send error
    // the inflate task sees once we stop receiving.
    if let Some(err) = write_err {
        return Err(err);
    }
    if produced.map_err(ApiError::bad_request)? == 0 {
        return Err(ApiError::bad_request("archive contains no files to extract"));
    }

    Ok(json_ok(serde_json::json!({
        "ok": true,
        "extracted": extracted,
        "skipped": skipped,
        "extracted_count": extracted.len(),
        "skipped_count": skipped.len(),
    })))
}

/// Store one extracted file as its own object, honouring `overwrite`.
/// Returns the object key and whether it was actually written.
async fn write_zip_entry(
    state: &AdminState,
    bucket: &str,
    entry: ZipEntry,
    overwrite: bool,
) -> Result<(String, bool), ApiError> {
    if !overwrite {
        let head = HeadObjectInput { bucket: bucket.to_owned(), key: entry.name.clone(), ..Default::default() };
        if state.fs.head_object(state.s3_request(head)).await.is_ok() {
            return Ok((entry.name, false));
        }
    }
    let len = i64::try_from(entry.data.len()).unwrap_or(i64::MAX);
    let put = PutObjectInput {
        bucket: bucket.to_owned(),
        key: entry.name.clone(),
        body: Some(Body::from(entry.data).into()),
        content_type: guess_content_type(&entry.name),
        content_length: Some(len),
        ..Default::default()
    };
    state.fs.put_object(state.s3_request(put)).await?;
    Ok((entry.name, true))
}

/// Decompress every file entry in `archive`, prefixing each name with `dest`, and
/// hand each one to `sink` as soon as it is inflated. Runs inside `spawn_blocking`.
///
/// Returns the number of entries produced, or a user-facing error string on any
/// malformed entry, unsafe path, or breach of the size/count caps. A closed `sink`
/// means the consumer gave up (and will report its own error), so we stop quietly.
fn decompress_zip(archive: &[u8], dest: &str, sink: &tokio::sync::mpsc::Sender<ZipEntry>) -> Result<usize, String> {
    use std::io::Read;

    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(archive))
        .map_err(|e| format!("not a valid ZIP archive: {e}"))?;
    if zip.len() > MAX_ENTRIES {
        return Err(format!("archive has too many entries (limit {MAX_ENTRIES})"));
    }

    // Validate every path before writing anything. Reading names does not inflate,
    // so this is cheap, and it keeps an archive containing a zip-slip entry from
    // being partially extracted before the bad entry is reached.
    for i in 0..zip.len() {
        let file = zip.by_index(i).map_err(|e| format!("failed to read entry {i}: {e}"))?;
        if file.is_dir() {
            continue;
        }
        if file.enclosed_name().is_none() {
            return Err(format!("archive contains an unsafe path: {}", file.name()));
        }
    }

    let mut produced = 0usize;
    let mut total: u64 = 0;
    for i in 0..zip.len() {
        let file = zip.by_index(i).map_err(|e| format!("failed to read entry {i}: {e}"))?;
        if file.is_dir() {
            continue;
        }
        // `enclosed_name` rejects absolute paths and `..` traversal (zip-slip); the
        // pass above already proved every entry has one.
        let rel = file
            .enclosed_name()
            .ok_or_else(|| format!("archive contains an unsafe path: {}", file.name()))?
            .to_string_lossy()
            .replace('\\', "/");
        if rel.is_empty() {
            continue;
        }

        // Read at most the remaining budget plus one byte, so an entry whose header
        // under-reports its size can't slip past the cap.
        let remaining = MAX_TOTAL_UNCOMPRESSED - total;
        let mut data = Vec::new();
        file.take(remaining + 1)
            .read_to_end(&mut data)
            .map_err(|e| format!("failed to decompress {rel}: {e}"))?;
        if data.len() as u64 > remaining {
            return Err(format!(
                "extracted size exceeds the {} MiB limit",
                MAX_TOTAL_UNCOMPRESSED / (1024 * 1024)
            ));
        }
        total += data.len() as u64;
        if sink.blocking_send(ZipEntry { name: format!("{dest}{rel}"), data }).is_err() {
            return Ok(produced);
        }
        produced += 1;
    }

    Ok(produced)
}

/// Best-effort Content-Type from a file extension, so extracted sites/assets are
/// served correctly. Unknown extensions fall back to the storage default.
fn guess_content_type(name: &str) -> Option<String> {
    let (_, ext) = name.rsplit_once('.')?;
    let ct = match ext.to_ascii_lowercase().as_str() {
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "js" | "mjs" => "text/javascript",
        "json" => "application/json",
        "xml" => "application/xml",
        "txt" | "md" => "text/plain",
        "csv" => "text/csv",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "pdf" => "application/pdf",
        "wasm" => "application/wasm",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        _ => return None,
    };
    Some(ct.to_owned())
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

async fn update_metadata(state: &AdminState, headers: &HeaderMap, body: Body) -> Result<S3Response<Body>, ApiError> {
    let m: MetadataBody = read_json(headers, body).await?;
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

fn presign_object(state: &AdminState, q: &Query) -> Result<S3Response<Body>, ApiError> {
    let bucket = q.require("bucket")?;
    let key = q.require("key")?;
    let method = q.opt("method").unwrap_or_else(|| "GET".to_owned()).to_uppercase();
    if method != "GET" && method != "PUT" {
        return Err(ApiError::bad_request("method must be GET or PUT"));
    }
    let expires = q.opt("expires").and_then(|s| s.parse::<u64>().ok()).unwrap_or(3600).clamp(1, 604_800);

    // A SigV4 presigned URL is signed over its host, and the admin panel sits on a
    // different port/domain than the S3 API. So the target host must come from the
    // operator-configured public API URL, never from the (client-controlled, and
    // here always wrong) request Host header.
    let base = state.settings.api_public_url().ok_or_else(|| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "NotConfigured",
            "presigned URLs require the public API URL to be set in the admin panel settings \
             (the public base URL of the S3 API, e.g. https://api.example.com)",
        )
    })?;
    let (scheme, host) = split_scheme_host(&base);
    if host.is_empty() {
        return Err(ApiError::internal("S3_API_PUBLIC_URL is malformed (no host)"));
    }

    let url = presign::presign(&state.access_key, &state.secret_key, scheme, host, &bucket, &key, &method, expires);
    Ok(json_ok(serde_json::json!({ "url": url, "expires_in": expires, "method": method })))
}

/// Split a configured base URL (`https://api.example.com[/...]`) into its scheme
/// and host[:port], dropping any path. A missing scheme defaults to `http`.
fn split_scheme_host(base: &str) -> (&str, &str) {
    let (scheme, rest) = base.split_once("://").unwrap_or(("http", base));
    let host = rest.split('/').next().unwrap_or(rest);
    (scheme, host)
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

async fn delete_objects(state: &AdminState, headers: &HeaderMap, body: Body) -> Result<S3Response<Body>, ApiError> {
    let b: BatchDeleteBody = read_json(headers, body).await?;
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

async fn create_folder(state: &AdminState, headers: &HeaderMap, body: Body) -> Result<S3Response<Body>, ApiError> {
    let f: FolderBody = read_json(headers, body).await?;
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

/// Require `Content-Type: application/json` on a JSON endpoint.
///
/// Defence in depth behind [`check_same_origin`]: an HTML form can only ever send
/// `text/plain`, `application/x-www-form-urlencoded` or `multipart/form-data`, so
/// insisting on JSON puts every JSON handler out of reach of form-based CSRF even
/// if the origin check is somehow bypassed. `fetch` cannot set this header
/// cross-origin without a preflight, which the admin port never answers.
fn check_json_content_type(headers: &HeaderMap) -> Result<(), ApiError> {
    let ct = headers.get(CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or_default();
    // Ignore any `; charset=...` parameter.
    let essence = ct.split(';').next().unwrap_or_default().trim();
    if essence.eq_ignore_ascii_case("application/json") {
        return Ok(());
    }
    Err(ApiError::new(
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "UnsupportedMediaType",
        "this endpoint requires `Content-Type: application/json`",
    ))
}

async fn read_json<T: DeserializeOwned>(headers: &HeaderMap, body: Body) -> Result<T, ApiError> {
    check_json_content_type(headers)?;
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
