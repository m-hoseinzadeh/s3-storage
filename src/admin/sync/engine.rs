//! Lists the source bucket and copies what [`super::plan::decide`] selects.
//!
//! The shape is a bounded producer/consumer: one lister task pages the remote
//! `ListObjectsV2` into a small channel, and a fixed pool of workers drains it.
//! The channel provides backpressure for free -- the lister stops paging when
//! the workers are saturated -- and the fixed pool is what bounds concurrency.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::UNIX_EPOCH;

use s3s::S3;
use s3s::dto::{
    CreateBucketInput, HeadBucketInput, Metadata, PutObjectInput, StreamingBlob, Timestamp,
};
use tokio::sync::mpsc;

use super::plan::{LocalObject, SourceObject, SyncMode, decide};
use super::{SyncJob, SyncRequest, SyncStatus};
use crate::admin::AdminState;

/// Objects listed per remote request. 1000 is the S3 maximum.
const LIST_PAGE_SIZE: i32 = 1000;

/// Planned actions returned by a preview. Enough to see the shape of a run
/// without rendering a listing of unbounded size.
const PREVIEW_LIMIT: usize = 200;

/// One decided object, ready to copy.
struct Task {
    src: SourceObject,
    dst_key: String,
}

/// Run a sync to completion, reporting progress into `job`.
///
/// Per-object failures are recorded and the run continues; only a failure to
/// reach or enumerate the source is fatal.
pub(crate) async fn run(state: Arc<AdminState>, req: SyncRequest, job: Arc<SyncJob>) {
    let (status, fatal) = match execute(&state, &req, &job).await {
        Ok(()) if job.is_cancelled() => (SyncStatus::Cancelled, None),
        Ok(()) if job.progress.failed.load(Ordering::Relaxed) > 0 => (SyncStatus::Failed, None),
        Ok(()) => (SyncStatus::Completed, None),
        Err(e) => (SyncStatus::Failed, Some(e)),
    };
    state.sync.finish(&job, status, fatal);
}

async fn execute(state: &Arc<AdminState>, req: &SyncRequest, job: &Arc<SyncJob>) -> Result<(), String> {
    let client = super::client::build_client(&req.source)?;
    ensure_destination_bucket(state, req).await?;
    // One cheap round trip so a wrong endpoint, region or key pair is reported
    // in seconds rather than after a page of retries against every object.
    probe_source(&client, req).await?;

    let (tx, rx) = mpsc::channel::<Task>(req.concurrency * 2);
    let rx = Arc::new(tokio::sync::Mutex::new(rx));

    let lister = {
        let client = client.clone();
        let job = Arc::clone(job);
        let req_src = ListScope::from(req);
        tokio::spawn(async move { list_source(&client, &req_src, &job, tx).await })
    };

    let mut workers = Vec::with_capacity(req.concurrency);
    for _ in 0..req.concurrency {
        let state = Arc::clone(state);
        let client = client.clone();
        let job = Arc::clone(job);
        let rx = Arc::clone(&rx);
        let scope = CopyScope::from(req);
        workers.push(tokio::spawn(async move {
            loop {
                // The guard is scoped so the queue is not held across the copy,
                // which is the whole point of having several workers.
                let Some(task) = rx.lock().await.recv().await else { break };
                if job.is_cancelled() {
                    break;
                }

                // Deciding here rather than in the lister keeps the local stat
                // -- and, under verify_etag, the whole-object hash -- off the
                // listing path and spread across the pool.
                let local = local_view(&state, &scope.dst_bucket, &task.dst_key, scope.verify_etag).await;
                if !decide(scope.mode, &task.src, local.as_ref(), scope.skew_secs).is_copy() {
                    job.progress.skipped.fetch_add(1, Ordering::Relaxed);
                    continue;
                }

                // A byte budget is checked against what has actually landed, so
                // it bounds the run rather than merely the plan.
                if let Some(max) = scope.max_bytes
                    && job.progress.bytes.load(Ordering::Relaxed) >= max
                {
                    break;
                }

                job.progress.set_current(Some(task.dst_key.clone()));
                match copy_one(&state, &client, &scope, &task, &job).await {
                    Ok(Some(bytes)) => {
                        job.progress.copied.fetch_add(1, Ordering::Relaxed);
                        job.progress.bytes.fetch_add(bytes, Ordering::Relaxed);
                    }
                    // Cancelled mid-object; nothing landed.
                    Ok(None) => break,
                    Err(e) => job.progress.record_error(&task.src.key, e),
                }
            }
        }));
    }

    // The workers own the queue from here. Holding this handle would keep the
    // receiver alive after they exit, and the lister would then block forever in
    // `send` on a queue nobody drains.
    drop(rx);

    let listing = lister.await.map_err(|e| format!("listing task failed: {e}"))?;
    for worker in workers {
        let _ = worker.await;
    }
    job.progress.set_current(None);
    listing
}

/// The subset of a request the lister needs, so it can move into its own task.
struct ListScope {
    src_bucket: String,
    src_prefix: String,
    dst_prefix: String,
    max_objects: Option<u64>,
}

impl From<&SyncRequest> for ListScope {
    fn from(r: &SyncRequest) -> Self {
        Self {
            src_bucket: r.src_bucket.clone(),
            src_prefix: r.src_prefix.clone(),
            dst_prefix: r.dst_prefix.clone(),
            max_objects: r.max_objects,
        }
    }
}

/// The subset a copy worker needs: enough to decide, and enough to copy.
struct CopyScope {
    src_bucket: String,
    dst_bucket: String,
    mode: SyncMode,
    skew_secs: i64,
    verify_etag: bool,
    max_bytes: Option<u64>,
}

impl From<&SyncRequest> for CopyScope {
    fn from(r: &SyncRequest) -> Self {
        Self {
            src_bucket: r.src_bucket.clone(),
            dst_bucket: r.dst_bucket.clone(),
            mode: r.mode,
            skew_secs: r.skew_secs,
            verify_etag: r.verify_etag,
            max_bytes: r.max_bytes,
        }
    }
}

/// Create the destination bucket, or confirm it exists.
async fn ensure_destination_bucket(state: &AdminState, req: &SyncRequest) -> Result<(), String> {
    let head = HeadBucketInput { bucket: req.dst_bucket.clone(), ..Default::default() };
    if state.fs.head_bucket(state.s3_request(head)).await.is_ok() {
        return Ok(());
    }
    if !req.create_bucket {
        return Err(format!(
            "destination bucket `{}` does not exist (enable \"create destination\" to make it)",
            req.dst_bucket
        ));
    }
    let create = CreateBucketInput { bucket: req.dst_bucket.clone(), ..Default::default() };
    state
        .fs
        .create_bucket(state.s3_request(create))
        .await
        .map(|_| ())
        .map_err(|e| format!("could not create destination bucket `{}`: {e}", req.dst_bucket))
}

/// Verify we can actually talk to the source before enumerating it.
async fn probe_source(client: &aws_sdk_s3::Client, req: &SyncRequest) -> Result<(), String> {
    client
        .list_objects_v2()
        .bucket(&req.src_bucket)
        .max_keys(1)
        .send()
        .await
        .map(|_| ())
        .map_err(|e| describe_source_error("could not read the source bucket", &e))
}

/// Page the source listing, decide each object, and hand the copies downstream.
async fn list_source(
    client: &aws_sdk_s3::Client,
    scope: &ListScope,
    job: &Arc<SyncJob>,
    tx: mpsc::Sender<Task>,
) -> Result<(), String> {
    let mut cancel = job.cancel_signal();
    let mut token: Option<String> = None;
    let mut planned: u64 = 0;

    loop {
        if job.is_cancelled() {
            return Ok(());
        }
        let page = client
            .list_objects_v2()
            .bucket(&scope.src_bucket)
            .set_prefix(non_empty(&scope.src_prefix))
            .max_keys(LIST_PAGE_SIZE)
            .set_continuation_token(token.clone())
            .send()
            .await
            .map_err(|e| describe_source_error("listing the source bucket failed", &e))?;

        for object in page.contents() {
            if job.is_cancelled() {
                return Ok(());
            }
            let Some(key) = object.key() else { continue };
            if let Some(max) = scope.max_objects
                && planned >= max
            {
                return Ok(());
            }
            job.progress.listed.fetch_add(1, Ordering::Relaxed);

            let Some(dst_key) = remap_key(key, &scope.src_prefix, &scope.dst_prefix) else {
                job.progress
                    .record_error(key, "unsafe object key refused".to_owned());
                continue;
            };
            let src = SourceObject {
                key: key.to_owned(),
                size: u64::try_from(object.size().unwrap_or(0)).unwrap_or(0),
                etag: object.e_tag().map(ToOwned::to_owned),
                last_modified_unix: object.last_modified().map(aws_sdk_s3::primitives::DateTime::secs),
            };

            tokio::select! {
                result = tx.send(Task { src, dst_key }) => {
                    // Every worker is gone; nothing more can be copied.
                    if result.is_err() {
                        return Ok(());
                    }
                }
                // Without this the lister would sit in `send` on a full queue
                // until the workers happened to drain it.
                _ = cancel.changed() => return Ok(()),
            }
            planned += 1;
        }

        if !page.is_truncated().unwrap_or(false) {
            return Ok(());
        }
        token = page.next_continuation_token().map(ToOwned::to_owned);
        // A truncated page with no token would loop forever re-reading page one.
        if token.is_none() {
            return Ok(());
        }
    }
}

/// Copy one object. Returns the byte count written, or `None` if the run was
/// cancelled before anything landed.
async fn copy_one(
    state: &AdminState,
    client: &aws_sdk_s3::Client,
    scope: &CopyScope,
    task: &Task,
    job: &Arc<SyncJob>,
) -> Result<Option<u64>, String> {
    // A key ending in `/` is a folder placeholder, stored as a real directory.
    // `put_object` refuses a non-empty body for one, so never fetch it.
    if task.dst_key.ends_with('/') {
        let put = PutObjectInput {
            bucket: scope.dst_bucket.clone(),
            key: task.dst_key.clone(),
            body: Some(s3s::Body::empty().into()),
            content_length: Some(0),
            ..Default::default()
        };
        state
            .fs
            .put_object(state.s3_request(put))
            .await
            .map_err(|e| format!("could not create folder: {e}"))?;
        return Ok(Some(0));
    }

    let out = client
        .get_object()
        .bucket(&scope.src_bucket)
        .key(&task.src.key)
        .send()
        .await
        .map_err(|e| describe_source_error("could not read the source object", &e))?;

    let content_length = out.content_length();
    // The SDK body is piped straight into the backend: `http_body` wraps it
    // without a collection point, so an object of any size streams to disk
    // rather than being buffered in memory.
    let body: StreamingBlob = s3s::Body::http_body(out.body.into_inner()).into();

    let put = PutObjectInput {
        bucket: scope.dst_bucket.clone(),
        key: task.dst_key.clone(),
        body: Some(body),
        content_length,
        content_type: out.content_type.or_else(|| crate::admin::api::guess_content_type(&task.dst_key)),
        content_encoding: out.content_encoding,
        content_disposition: out.content_disposition,
        content_language: out.content_language,
        cache_control: out.cache_control,
        website_redirect_location: out.website_redirect_location,
        metadata: out.metadata.map(|m| m.into_iter().collect::<Metadata>()),
        expires: out.expires_string.as_deref().and_then(parse_expires),
        // checksum_* and content_md5 are left unset on purpose: the backend
        // would verify values the source computed over a different part layout,
        // and a composite ETag is not a checksum at all. storage_class is unset
        // because the backend accepts only STANDARD / REDUCED_REDUNDANCY.
        ..Default::default()
    };

    let mut cancel = job.cancel_signal();
    tokio::select! {
        result = state.fs.put_object(state.s3_request(put)) => {
            result.map_err(|e| format!("could not write the object: {e}"))?;
            // `content_length` is what the source declared; fall back to the
            // planned size when it is absent.
            let written = content_length
                .and_then(|n| u64::try_from(n).ok())
                .unwrap_or(task.src.size);
            Ok(Some(written))
        }
        // Dropping the put future mid-body abandons the staged temp file, which
        // `FileWriter`'s Drop removes; the destination object is untouched.
        _ = cancel.changed() => Ok(None),
    }
}

/// Plan a run without copying anything.
///
/// Returns the totals plus the first [`PREVIEW_LIMIT`] planned actions, so an
/// operator can see what a run intends to do -- and how many bytes it would
/// pull -- before starting it.
pub(crate) async fn preview(state: &AdminState, req: &SyncRequest) -> Result<serde_json::Value, String> {
    let client = super::client::build_client(&req.source)?;
    probe_source(&client, req).await?;

    let mut token: Option<String> = None;
    let (mut listed, mut to_copy, mut to_skip, mut copy_bytes) = (0u64, 0u64, 0u64, 0u64);
    let mut actions = Vec::new();

    'paging: loop {
        let page = client
            .list_objects_v2()
            .bucket(&req.src_bucket)
            .set_prefix(non_empty(&req.src_prefix))
            .max_keys(LIST_PAGE_SIZE)
            .set_continuation_token(token.clone())
            .send()
            .await
            .map_err(|e| describe_source_error("listing the source bucket failed", &e))?;

        for object in page.contents() {
            let Some(key) = object.key() else { continue };
            listed += 1;
            let Some(dst_key) = remap_key(key, &req.src_prefix, &req.dst_prefix) else {
                continue;
            };
            let src = SourceObject {
                key: key.to_owned(),
                size: u64::try_from(object.size().unwrap_or(0)).unwrap_or(0),
                etag: object.e_tag().map(ToOwned::to_owned),
                last_modified_unix: object.last_modified().map(aws_sdk_s3::primitives::DateTime::secs),
            };
            let local = local_view(state, &req.dst_bucket, &dst_key, req.verify_etag).await;
            let decision = decide(req.mode, &src, local.as_ref(), req.skew_secs);

            if decision.is_copy() {
                to_copy += 1;
                copy_bytes += src.size;
            } else {
                to_skip += 1;
            }
            if actions.len() < PREVIEW_LIMIT {
                actions.push(serde_json::json!({
                    "key": src.key,
                    "dst_key": dst_key,
                    "size": src.size,
                    "action": decision.tag(),
                    "reason": decision.reason(),
                }));
            }
            if let Some(max) = req.max_objects
                && listed >= max
            {
                break 'paging;
            }
        }

        if !page.is_truncated().unwrap_or(false) {
            break;
        }
        token = page.next_continuation_token().map(ToOwned::to_owned);
        if token.is_none() {
            break;
        }
    }

    Ok(serde_json::json!({
        "source": req.source_label(),
        "destination": req.destination_label(),
        "listed": listed,
        "to_copy": to_copy,
        "to_skip": to_skip,
        "bytes_to_copy": copy_bytes,
        "actions": actions,
        "actions_truncated": (to_copy + to_skip) > PREVIEW_LIMIT as u64,
    }))
}

/// What we already hold at `key`, as a cheap `stat` -- plus the local MD5 only
/// when the caller asked to verify ETags, since that reads the whole object.
pub(crate) async fn local_view(
    state: &AdminState,
    bucket: &str,
    key: &str,
    verify_etag: bool,
) -> Option<LocalObject> {
    let (size, modified) = state.fs.stat_object(bucket, key).await.ok().flatten()?;
    let modified_unix = modified
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0);
    let etag = if verify_etag { state.fs.get_md5_sum(bucket, key).await.ok() } else { None };
    Some(LocalObject { size, modified_unix, etag })
}

/// Map a source key into the destination namespace, or reject it.
///
/// Keys arrive from a remote server, so they are vetted the same way archive
/// entries are before anything is written: no absolute paths, no `..`
/// components, nothing empty. `get_object_path` is the backstop.
pub(crate) fn remap_key(key: &str, src_prefix: &str, dst_prefix: &str) -> Option<String> {
    let relative = key.strip_prefix(src_prefix).unwrap_or(key);
    if relative.is_empty() || relative.starts_with('/') {
        return None;
    }
    if relative.split('/').any(|part| part == ".." || part == ".") {
        return None;
    }
    Some(format!("{dst_prefix}{relative}"))
}

fn non_empty(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_owned())
}

/// `Expires` is a date header on the source and a timestamp in the backend's
/// sidecar. Try the HTTP date form first, since that is what the header is.
fn parse_expires(value: &str) -> Option<Timestamp> {
    Timestamp::parse(s3s::dto::TimestampFormat::HttpDate, value)
        .or_else(|_| Timestamp::parse(s3s::dto::TimestampFormat::DateTime, value))
        .ok()
}

/// Turn an SDK error into something an operator can act on.
///
/// The raw `SdkError` Display is often just "service error", so the service
/// error code is dug out and the two misconfigurations that actually happen --
/// a wrong key pair and a region mismatch -- are named explicitly.
fn describe_source_error<E, R>(context: &str, err: &aws_sdk_s3::error::SdkError<E, R>) -> String
where
    E: std::error::Error + aws_sdk_s3::error::ProvideErrorMetadata,
{
    use aws_sdk_s3::error::ProvideErrorMetadata;
    let code = err.code().unwrap_or_default().to_owned();
    let message = err.message().map(ToOwned::to_owned).unwrap_or_else(|| err.to_string());
    let hint = match code.as_str() {
        "SignatureDoesNotMatch" => {
            " -- check the secret key, and the region if the source sets MINIO_REGION"
        }
        "InvalidAccessKeyId" => " -- the source does not recognise this access key",
        "NoSuchBucket" => " -- the bucket does not exist on the source",
        "AccessDenied" => " -- these credentials cannot read the source bucket",
        _ => "",
    };
    if code.is_empty() {
        format!("{context}: {message}")
    } else {
        format!("{context}: {code}: {message}{hint}")
    }
}

#[cfg(test)]
mod tests {
    use super::remap_key;

    #[test]
    fn a_key_is_rebased_from_the_source_prefix_onto_the_destination_prefix() {
        assert_eq!(remap_key("a/x.txt", "a/", "b/").as_deref(), Some("b/x.txt"));
        assert_eq!(remap_key("a/deep/x.txt", "a/", "b/").as_deref(), Some("b/deep/x.txt"));
    }

    #[test]
    fn empty_prefixes_pass_the_key_through_unchanged() {
        assert_eq!(remap_key("x.txt", "", "").as_deref(), Some("x.txt"));
        assert_eq!(remap_key("a/x.txt", "", "").as_deref(), Some("a/x.txt"));
    }

    #[test]
    fn a_key_that_is_exactly_the_prefix_is_refused() {
        assert_eq!(remap_key("a/", "a/", "b/"), None);
    }

    #[test]
    fn traversal_and_absolute_keys_are_refused() {
        assert_eq!(remap_key("../escape", "", "b/"), None);
        assert_eq!(remap_key("a/../../escape", "", "b/"), None);
        assert_eq!(remap_key("/absolute", "", "b/"), None);
        assert_eq!(remap_key("a/./x", "", "b/"), None);
        assert_eq!(remap_key("", "", "b/"), None);
    }

    #[test]
    fn a_key_outside_the_source_prefix_keeps_its_full_path() {
        // `strip_prefix` misses, so the key is rebased whole rather than
        // silently losing its leading segments.
        assert_eq!(remap_key("other/x", "a/", "b/").as_deref(), Some("b/other/x"));
    }
}
