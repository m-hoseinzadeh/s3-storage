// Derived from the Apache-2.0 licensed `s3s-fs` reference implementation
// (https://github.com/Nugine/s3s, Copyright 2023 Nugine).
// Modified for this project. See NOTICE and LICENSE.

use crate::backend::fs::FileSystem;
use crate::backend::fs::InternalInfo;
use crate::backend::fs::{ETAG_STAMP, etag_stamp, stored_etag};
use crate::backend::utils::*;

use s3s::S3;
use s3s::S3Result;
use s3s::crypto::Checksum;
use s3s::crypto::Md5;
use s3s::dto::*;
use s3s::s3_error;
use s3s::{S3Request, S3Response};

use std::collections::VecDeque;
use std::io;
use std::ops::Neg;
use std::ops::Not;
use std::path::Component;
use std::path::{Path, PathBuf};

use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio_util::io::ReaderStream;

use futures::TryStreamExt;
use numeric_cast::NumericCast;
use stdx::default::default;
use tracing::debug;
use uuid::Uuid;

fn normalize_path(path: &Path, delimiter: &str) -> Option<String> {
    let mut normalized = String::new();
    let mut first = true;
    for component in path.components() {
        match component {
            Component::RootDir | Component::CurDir | Component::ParentDir | Component::Prefix(_) => {
                return None;
            }
            Component::Normal(name) => {
                let name = name.to_str()?;
                if !first {
                    normalized.push_str(delimiter);
                }
                normalized.push_str(name);
                first = false;
            }
        }
    }
    Some(normalized)
}

/// Read size when streaming an object's bytes out.
///
/// Every read on a `tokio::fs::File` is a separate trip through the blocking
/// thread pool, so this sets how many trips a download costs. At the old 4 KiB a
/// 100 MB object took ~25,000 of them, and under concurrent downloads the pool
/// saturated and every other file operation queued behind it. tokio caps a
/// single file read at 2 MiB, so this stays well inside what one read can fill.
const READ_CHUNK: usize = 256 * 1024;

/// AWS caps a `ListObjects` page at 1000 keys; so do we, for requested and default
/// page sizes alike.
const MAX_LIST_KEYS: i32 = 1000;

/// Accumulates one listing page while the directory walk is still running.
///
/// The resume marker and the page limit are applied as entries arrive, and only the
/// `limit + 1` smallest candidates are kept -- one more than the page returns, which
/// is exactly what `IsTruncated` needs. Previously every key in the bucket was
/// materialised, sorted, and then almost entirely discarded, so a listing's memory
/// and sort cost scaled with the bucket instead of with the page.
///
/// Walking the tree is still O(objects): without an index there is no way to find
/// the next N keys without looking at them. What this bounds is what the walk keeps.
struct ListingPage<'a> {
    resume_after: Option<&'a str>,
    /// `limit + 1` -- the page itself plus one lookahead entry.
    keep: usize,
    objects: std::collections::BTreeMap<String, Object>,
    prefixes: std::collections::BTreeSet<String>,
}

impl<'a> ListingPage<'a> {
    fn new(resume_after: Option<&'a str>, limit: usize) -> Self {
        Self {
            resume_after,
            keep: limit.saturating_add(1),
            objects: default(),
            prefixes: default(),
        }
    }

    /// Whether `key` could still make it into the page, given the marker and the
    /// entries already held. Callers check this before paying for a `stat`.
    fn accepts(&self, key: &str) -> bool {
        if self.resume_after.is_some_and(|m| key <= m) {
            return false;
        }
        if self.objects.len() < self.keep {
            return true;
        }
        self.objects.last_key_value().is_none_or(|(largest, _)| key < largest.as_str())
    }

    fn push_object(&mut self, key: String, object: Object) {
        if !self.accepts(&key) {
            return;
        }
        self.objects.insert(key, object);
        while self.objects.len() > self.keep {
            self.objects.pop_last();
        }
    }

    fn push_prefix(&mut self, prefix: String) {
        if self.resume_after.is_some_and(|m| prefix.as_str() <= m) {
            return;
        }
        self.prefixes.insert(prefix);
        while self.prefixes.len() > self.keep {
            self.prefixes.pop_last();
        }
    }

    /// The retained candidates, already in key order.
    fn into_sorted(self) -> (Vec<Object>, Vec<CommonPrefix>) {
        let objects = self.objects.into_values().collect();
        let prefixes = self
            .prefixes
            .into_iter()
            .map(|prefix| CommonPrefix { prefix: Some(prefix) })
            .collect();
        (objects, prefixes)
    }
}

/// AWS accepts part numbers 1..=10000. Only the upper bound used to be checked, so
/// a zero or negative number produced an unaddressable `.part--1` file on disk.
fn check_part_number(part_number: PartNumber) -> S3Result<()> {
    if !(1..=10_000).contains(&part_number) {
        return Err(s3_error!(
            InvalidArgument,
            "Part number must be an integer between 1 and 10000, inclusive"
        ));
    }
    Ok(())
}

/// <https://developer.mozilla.org/en-US/docs/Web/HTTP/Headers/Content-Range>
fn fmt_content_range(start: u64, end_inclusive: u64, size: u64) -> String {
    format!("bytes {start}-{end_inclusive}/{size}")
}

/// Outcome of evaluating conditional-request headers.
enum Precondition {
    /// Serve the object normally.
    Proceed,
    /// The client's cached copy is still fresh (HTTP 304).
    NotModified,
}

/// Whether an `If-Match` / `If-None-Match` condition matches the object's `etag`.
/// `*` matches any existing object; otherwise the (strong or weak) tag value is
/// compared against the bare ETag.
fn cond_matches(cond: &ETagCondition, etag: &str) -> bool {
    match cond {
        ETagCondition::Any => true,
        ETagCondition::ETag(tag) => {
            let value = tag.as_strong().or_else(|| tag.as_weak()).unwrap_or_default();
            value == etag.trim_matches('"')
        }
    }
}

/// Whole-second epoch value, so comparisons match the one-second resolution of
/// HTTP dates (the `Last-Modified` we advertise is truncated to seconds).
fn ts_secs(ts: &Timestamp) -> i64 {
    time::OffsetDateTime::from(ts.clone()).unix_timestamp()
}

/// Evaluate S3 conditional GET/HEAD headers against the object's current
/// `etag`/`last_modified`. `If-Match` takes precedence over `If-Unmodified-Since`
/// for the 412 path, and `If-None-Match` over `If-Modified-Since` for the 304 path,
/// matching the AWS S3 evaluation order.
fn evaluate_preconditions(
    if_match: Option<&ETagCondition>,
    if_none_match: Option<&ETagCondition>,
    if_modified_since: Option<&Timestamp>,
    if_unmodified_since: Option<&Timestamp>,
    etag: &str,
    last_modified: &Timestamp,
) -> S3Result<Precondition> {
    if let Some(im) = if_match {
        if !cond_matches(im, etag) {
            return Err(s3_error!(PreconditionFailed));
        }
    } else if let Some(ius) = if_unmodified_since
        && ts_secs(last_modified) > ts_secs(ius)
    {
        return Err(s3_error!(PreconditionFailed));
    }

    if let Some(inm) = if_none_match {
        if cond_matches(inm, etag) {
            return Ok(Precondition::NotModified);
        }
    } else if let Some(ims) = if_modified_since
        && ts_secs(last_modified) <= ts_secs(ims)
    {
        return Ok(Precondition::NotModified);
    }

    Ok(Precondition::Proceed)
}

#[async_trait::async_trait]
impl S3 for FileSystem {
    #[tracing::instrument(level = "debug")]
    async fn create_bucket(&self, req: S3Request<CreateBucketInput>) -> S3Result<S3Response<CreateBucketOutput>> {
        let input = req.input;
        let path = self.get_bucket_path(&input.bucket)?;

        if path.exists() {
            return Err(s3_error!(BucketAlreadyExists));
        }

        try_!(fs::create_dir(&path).await);

        let output = CreateBucketOutput::default(); // TODO: handle other fields
        Ok(S3Response::new(output))
    }

    #[tracing::instrument(level = "debug")]
    async fn copy_object(&self, req: S3Request<CopyObjectInput>) -> S3Result<S3Response<CopyObjectOutput>> {
        let input = req.input;
        let (bucket, key) = match input.copy_source {
            CopySource::AccessPoint { .. } => return Err(s3_error!(NotImplemented)),
            CopySource::Bucket { ref bucket, ref key, .. } => (bucket, key),
        };

        let src_path = self.get_object_path(bucket, key)?;
        let dst_path = self.get_object_path(&input.bucket, &input.key)?;

        if src_path.exists().not() {
            return Err(s3_error!(NoSuchKey));
        }

        if self.get_bucket_path(&input.bucket)?.exists().not() {
            return Err(s3_error!(NoSuchBucket));
        }

        if let Some(dir_path) = dst_path.parent() {
            try_!(fs::create_dir_all(&dir_path).await);
        }

        let file_metadata = try_!(fs::metadata(&src_path).await);
        let last_modified = Timestamp::from(try_!(file_metadata.modified()));

        // Hashed from the source, before the copy: the copy writes identical bytes,
        // and the sidecar write below needs this value.
        let md5_sum = self.get_md5_sum(bucket, key).await?;

        // A copy onto the same path (e.g. a metadata-only `CopyObject` with the same
        // bucket+key) must not touch the data: `fs::copy(p, p)` truncates the file to
        // empty, destroying the object. Skip all data/sidecar copies in that case and
        // leave the existing bytes and metadata untouched.
        if src_path != dst_path {
            let _ = try_!(fs::copy(&src_path, &dst_path).await);

            debug!(from = %src_path.display(), to = %dst_path.display(), "copy file");

            // The destination's sidecars must describe the bytes just written, never
            // whatever previously lived at that key. Always replace them -- clearing
            // when the source has none -- so a leftover sidecar can never be adopted.
            let src_metadata_path = self.get_metadata_path(bucket, key, None)?;
            let dst_metadata_path = self.get_metadata_path(&input.bucket, &input.key, None)?;
            if src_metadata_path.exists() {
                let _ = try_!(fs::copy(src_metadata_path, &dst_metadata_path).await);
            } else {
                crate::backend::fs::remove_file_if_exists(&dst_metadata_path).await?;
            }

            // Carry over the checksum sidecar so the copy reports the same checksums,
            // but replace any stored ETag: the copy is a fresh object whose ETag is
            // its own MD5, not the source's `<...>-<n>` multipart value. Storing it
            // rather than removing it keeps the copy listable with an ETag.
            // A source sidecar stamped for other bytes describes another object, so
            // its checksums must not be carried over (see `resolve_etag`).
            let mut info = self
                .load_internal_info(bucket, key)
                .await?
                .filter(|i| !i.contains_key(ETAG_STAMP) || stored_etag(i, &file_metadata).is_some())
                .unwrap_or_default();
            info.insert("etag".to_owned(), serde_json::Value::String(md5_sum.clone()));
            // The source's stamp names the source file; the copy is a new file, and
            // its ETag here was computed from the bytes, so it needs none.
            info.remove(ETAG_STAMP);
            self.save_internal_info(&input.bucket, &input.key, &info).await?;
        }

        let copy_object_result = CopyObjectResult {
            e_tag: Some(ETag::Strong(md5_sum)),
            last_modified: Some(last_modified),
            ..Default::default()
        };

        let output = CopyObjectOutput {
            copy_object_result: Some(copy_object_result),
            ..Default::default()
        };
        Ok(S3Response::new(output))
    }

    #[tracing::instrument(level = "debug")]
    async fn delete_bucket(&self, req: S3Request<DeleteBucketInput>) -> S3Result<S3Response<DeleteBucketOutput>> {
        let input = req.input;
        let path = self.get_bucket_path(&input.bucket)?;
        if path.exists().not() {
            return Err(s3_error!(NoSuchBucket));
        }
        // S3 refuses to delete a bucket that still holds objects; the caller has to
        // empty it first. Deleting the tree outright would turn `aws s3 rb` (without
        // `--force`) and the panel's delete button into silent recursive deletes.
        let mut entries = try_!(fs::read_dir(&path).await);
        if try_!(entries.next_entry().await).is_some() {
            return Err(s3_error!(BucketNotEmpty, "The bucket you tried to delete is not empty"));
        }
        try_!(fs::remove_dir(&path).await);
        Ok(S3Response::new(DeleteBucketOutput {}))
    }

    #[tracing::instrument(level = "debug")]
    async fn delete_object(&self, req: S3Request<DeleteObjectInput>) -> S3Result<S3Response<DeleteObjectOutput>> {
        let input = req.input;
        let bucket_root = self.get_bucket_path(&input.bucket)?;
        let path = self.get_object_path(&input.bucket, &input.key)?;
        if path.exists().not() {
            return Err(s3_error!(NoSuchKey));
        }
        self.remove_object(&input.bucket, &input.key, &path).await?;
        self.prune_empty_dirs(path.parent(), &bucket_root).await;
        let output = DeleteObjectOutput::default(); // TODO: handle other fields
        Ok(S3Response::new(output))
    }

    #[tracing::instrument(level = "debug")]
    async fn delete_objects(&self, req: S3Request<DeleteObjectsInput>) -> S3Result<S3Response<DeleteObjectsOutput>> {
        let input = req.input;
        // In quiet mode the response carries only keys whose deletion failed; the
        // successful ones are omitted. We never report per-key failures here, so a
        // quiet request yields an empty `Deleted` list.
        let quiet = input.delete.quiet.unwrap_or(false);
        let bucket_root = self.get_bucket_path(&input.bucket)?;
        let mut deleted_objects: Vec<DeletedObject> = Vec::new();
        for object in input.delete.objects {
            let path = self.get_object_path(&input.bucket, &object.key)?;
            // S3 DeleteObjects is idempotent: a key that does not exist is still
            // reported as deleted. Only an actual removal failure is an error.
            if path.exists() {
                self.remove_object(&input.bucket, &object.key, &path).await?;
                self.prune_empty_dirs(path.parent(), &bucket_root).await;
            }

            if !quiet {
                deleted_objects.push(DeletedObject {
                    key: Some(object.key),
                    ..Default::default()
                });
            }
        }

        let output = DeleteObjectsOutput {
            deleted: Some(deleted_objects),
            ..Default::default()
        };
        Ok(S3Response::new(output))
    }

    #[tracing::instrument(level = "debug")]
    async fn get_bucket_location(&self, req: S3Request<GetBucketLocationInput>) -> S3Result<S3Response<GetBucketLocationOutput>> {
        let input = req.input;
        let path = self.get_bucket_path(&input.bucket)?;

        if !path.exists() {
            return Err(s3_error!(NoSuchBucket));
        }

        let output = GetBucketLocationOutput::default();
        Ok(S3Response::new(output))
    }

    #[tracing::instrument(level = "debug", skip_all, fields(bucket = %req.input.bucket, key = %req.input.key))]
    async fn get_object(&self, req: S3Request<GetObjectInput>) -> S3Result<S3Response<GetObjectOutput>> {
        let input = req.input;
        let object_path = self.get_object_path(&input.bucket, &input.key)?;

        let mut file = fs::File::open(&object_path).await.map_err(|e| s3_error!(e, NoSuchKey))?;

        let file_metadata = try_!(file.metadata().await);
        let last_modified = Timestamp::from(try_!(file_metadata.modified()));
        let file_len = file_metadata.len();

        // The ETag is the stored multipart ETag (`<md5-of-part-md5s>-<n>`) when
        // present, otherwise the whole-object MD5. Loading the sidecar up front also
        // avoids hashing a (possibly huge) multipart object just to answer a
        // conditional request.
        // Both sidecars are needed on the common (200) path, and each read is a
        // round trip through the blocking pool; do them side by side.
        let (mut info, obj_attrs) = tokio::try_join!(
            self.load_internal_info(&input.bucket, &input.key),
            self.load_object_attributes(&input.bucket, &input.key, None),
        )?;
        let etag = self.resolve_etag(&input.bucket, &input.key, &file_metadata, &mut info).await?;

        // Honour conditional-request headers before streaming any bytes.
        match evaluate_preconditions(
            input.if_match.as_ref(),
            input.if_none_match.as_ref(),
            input.if_modified_since.as_ref(),
            input.if_unmodified_since.as_ref(),
            &etag,
            &last_modified,
        )? {
            Precondition::NotModified => return Err(s3_error!(NotModified)),
            Precondition::Proceed => {}
        }

        let (content_length, content_range) = match input.range {
            None => (file_len, None),
            Some(range) => {
                let file_range = range.check(file_len)?;
                let content_length = file_range.end - file_range.start;
                let content_range = fmt_content_range(file_range.start, file_range.end - 1, file_len);
                (content_length, Some(content_range))
            }
        };
        let content_length_usize = try_!(usize::try_from(content_length));
        let content_length_i64 = try_!(i64::try_from(content_length));

        match input.range {
            Some(Range::Int { first, .. }) => {
                try_!(file.seek(io::SeekFrom::Start(first)).await);
            }
            Some(Range::Suffix { length }) => {
                let neg_offset = length.numeric_cast::<i64>().neg();
                try_!(file.seek(io::SeekFrom::End(neg_offset)).await);
            }
            None => {}
        }

        let body = bytes_stream(ReaderStream::with_capacity(file, READ_CHUNK), content_length_usize);

        let checksum = match &info {
            // S3 skips returning the checksum if a range is specified that is
            // less than the whole file
            Some(info) if content_length == file_len => crate::backend::checksum::from_internal_info(info),
            _ => default(),
        };

        #[allow(clippy::redundant_closure_for_method_calls)]
        let output = GetObjectOutput {
            body: Some(StreamingBlob::wrap(body)),
            content_length: Some(content_length_i64),
            content_range,
            last_modified: Some(last_modified),
            metadata: obj_attrs.as_ref().and_then(|a| a.user_metadata.clone()),
            content_encoding: obj_attrs.as_ref().and_then(|a| a.content_encoding.clone()),
            content_type: obj_attrs.as_ref().and_then(|a| a.content_type.clone()),
            content_disposition: obj_attrs.as_ref().and_then(|a| a.content_disposition.clone()),
            content_language: obj_attrs.as_ref().and_then(|a| a.content_language.clone()),
            cache_control: obj_attrs.as_ref().and_then(|a| a.cache_control.clone()),
            expires: obj_attrs.as_ref().and_then(|a| a.get_expires_timestamp()),
            website_redirect_location: obj_attrs.as_ref().and_then(|a| a.website_redirect_location.clone()),
            e_tag: Some(ETag::Strong(etag)),
            checksum_crc32: checksum.checksum_crc32,
            checksum_crc32c: checksum.checksum_crc32c,
            checksum_sha1: checksum.checksum_sha1,
            checksum_sha256: checksum.checksum_sha256,
            checksum_crc64nvme: checksum.checksum_crc64nvme,
            ..Default::default()
        };
        Ok(S3Response::new(output))
    }

    #[tracing::instrument(level = "debug")]
    async fn head_bucket(&self, req: S3Request<HeadBucketInput>) -> S3Result<S3Response<HeadBucketOutput>> {
        let input = req.input;
        let path = self.get_bucket_path(&input.bucket)?;

        if !path.exists() {
            return Err(s3_error!(NoSuchBucket));
        }

        Ok(S3Response::new(HeadBucketOutput::default()))
    }

    #[tracing::instrument(level = "debug", skip_all, fields(bucket = %req.input.bucket, key = %req.input.key))]
    async fn head_object(&self, req: S3Request<HeadObjectInput>) -> S3Result<S3Response<HeadObjectOutput>> {
        let input = req.input;
        let path = self.get_object_path(&input.bucket, &input.key)?;

        let file_metadata = match fs::metadata(path).await {
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(s3_error!(NoSuchKey)),
            result => try_!(result),
        };
        let last_modified = Timestamp::from(try_!(file_metadata.modified()));
        let file_len = file_metadata.len();

        // S3 returns the ETag on HEAD, so always resolve it (stored multipart ETag or
        // whole-object MD5) and use it for any conditional headers too.
        // As in `get_object`: both sidecars, side by side.
        let (mut info, obj_attrs) = tokio::try_join!(
            self.load_internal_info(&input.bucket, &input.key),
            self.load_object_attributes(&input.bucket, &input.key, None),
        )?;
        let etag_str = self.resolve_etag(&input.bucket, &input.key, &file_metadata, &mut info).await?;
        match evaluate_preconditions(
            input.if_match.as_ref(),
            input.if_none_match.as_ref(),
            input.if_modified_since.as_ref(),
            input.if_unmodified_since.as_ref(),
            &etag_str,
            &last_modified,
        )? {
            Precondition::NotModified => return Err(s3_error!(NotModified)),
            Precondition::Proceed => {}
        }
        let etag = Some(ETag::Strong(etag_str));

        #[allow(clippy::redundant_closure_for_method_calls)]
        let output = HeadObjectOutput {
            content_length: Some(try_!(i64::try_from(file_len))),
            e_tag: etag,
            content_type: obj_attrs.as_ref().and_then(|a| a.content_type.clone()),
            content_encoding: obj_attrs.as_ref().and_then(|a| a.content_encoding.clone()),
            content_disposition: obj_attrs.as_ref().and_then(|a| a.content_disposition.clone()),
            content_language: obj_attrs.as_ref().and_then(|a| a.content_language.clone()),
            cache_control: obj_attrs.as_ref().and_then(|a| a.cache_control.clone()),
            expires: obj_attrs.as_ref().and_then(|a| a.get_expires_timestamp()),
            website_redirect_location: obj_attrs.as_ref().and_then(|a| a.website_redirect_location.clone()),
            last_modified: Some(last_modified),
            metadata: obj_attrs.as_ref().and_then(|a| a.user_metadata.clone()),
            ..Default::default()
        };
        Ok(S3Response::new(output))
    }

    #[tracing::instrument(level = "debug")]
    async fn list_buckets(&self, _: S3Request<ListBucketsInput>) -> S3Result<S3Response<ListBucketsOutput>> {
        let mut buckets: Vec<Bucket> = Vec::new();
        let mut iter = try_!(fs::read_dir(&self.root).await);
        while let Some(entry) = try_!(iter.next_entry().await) {
            let file_type = try_!(entry.file_type().await);
            if file_type.is_dir().not() {
                continue;
            }

            let file_name = entry.file_name();
            let Some(name) = file_name.to_str() else { continue };
            if s3s::path::check_bucket_name(name).not() {
                continue;
            }

            let file_meta = try_!(entry.metadata().await);
            // Not all filesystems/mounts provide all file attributes like created timestamp,
            // therefore we try to fallback to modified if possible.
            // See https://github.com/Nugine/s3s/pull/22 for more details.
            let created_or_modified_date = Timestamp::from(try_!(file_meta.created().or(file_meta.modified())));

            let bucket = Bucket {
                creation_date: Some(created_or_modified_date),
                name: Some(name.to_owned()),
                bucket_region: None,
            };
            buckets.push(bucket);
        }

        let output = ListBucketsOutput {
            buckets: Some(buckets),
            owner: None,
            ..Default::default()
        };
        Ok(S3Response::new(output))
    }

    #[tracing::instrument(level = "debug")]
    async fn list_objects(&self, req: S3Request<ListObjectsInput>) -> S3Result<S3Response<ListObjectsOutput>> {
        let v2_resp = self.list_objects_v2(req.map_input(Into::into)).await?;

        Ok(v2_resp.map_output(|v2| ListObjectsOutput {
            contents: v2.contents,
            common_prefixes: v2.common_prefixes,
            delimiter: v2.delimiter,
            encoding_type: v2.encoding_type,
            name: v2.name,
            prefix: v2.prefix,
            max_keys: v2.max_keys,
            is_truncated: v2.is_truncated,
            // v1 paginates via marker/next-marker; our token is the last key, which
            // maps straight onto the v1 marker (itself fed back in as start_after).
            marker: v2.start_after,
            next_marker: v2.next_continuation_token,
            ..Default::default()
        }))
    }

    #[tracing::instrument(level = "debug")]
    async fn list_objects_v2(&self, req: S3Request<ListObjectsV2Input>) -> S3Result<S3Response<ListObjectsV2Output>> {
        let input = req.input;
        let path = self.get_bucket_path(&input.bucket)?;

        if path.exists().not() {
            return Err(s3_error!(NoSuchBucket));
        }

        // An empty `delimiter=` means no delimiter, not a zero-length one. Treating it
        // literally is catastrophic rather than merely wrong: `"".find("")` is
        // `Some(0)`, so every key "contains" the delimiter at position 0 and collapses
        // into a one-character common prefix, and the listing returns no objects at
        // all. `mc` (and other clients) send exactly this on a recursive listing, so
        // the bucket looked empty to them and a mirror re-copied every object on every
        // run instead of skipping what was already there.
        let delimiter = input.delimiter.as_deref().filter(|d| !d.is_empty());
        let prefix = input.prefix.as_deref().unwrap_or("").trim_start_matches('/');
        // AWS caps a page at 1000 keys and reports the effective value back. Without
        // a clamp a caller could ask for a single unbounded page of the whole bucket.
        let max_keys = input.max_keys.unwrap_or(MAX_LIST_KEYS).clamp(0, MAX_LIST_KEYS);
        let max_keys_usize = usize::try_from(max_keys).unwrap_or(0);

        // Resume point: a continuation token takes precedence over start_after, and
        // both mean "return items strictly after this key". The token we emit below
        // is simply the last key returned, so the two are interchangeable here.
        let resume_after = input.continuation_token.as_deref().or(input.start_after.as_deref());

        // Collect matching objects and common prefixes, bounded to this page.
        let mut page = ListingPage::new(resume_after, max_keys_usize);
        if let Some(delimiter) = delimiter {
            self.list_objects_with_delimiter(&input.bucket, &path, prefix, delimiter, &mut page)
                .await?;
        } else {
            self.list_objects_recursive(&input.bucket, &path, prefix, &mut page).await?;
        }
        let (objects, common_prefixes_list) = page.into_sorted();

        // Limit results to max_keys by interleaving objects and common_prefixes,
        // tracking the last key emitted so it can serve as the continuation token.
        let mut result_objects = Vec::new();
        let mut result_prefixes = Vec::new();
        let mut total_count = 0;

        let mut obj_idx = 0;
        let mut prefix_idx = 0;
        let mut last_key: Option<String> = None;

        while total_count < max_keys_usize {
            let obj_key = objects.get(obj_idx).and_then(|o| o.key.as_deref());
            let prefix_key = common_prefixes_list.get(prefix_idx).and_then(|p| p.prefix.as_deref());

            match (obj_key, prefix_key) {
                (Some(ok), Some(pk)) => {
                    if ok < pk {
                        last_key = Some(ok.to_owned());
                        result_objects.push(objects[obj_idx].clone());
                        obj_idx += 1;
                    } else {
                        last_key = Some(pk.to_owned());
                        result_prefixes.push(common_prefixes_list[prefix_idx].clone());
                        prefix_idx += 1;
                    }
                    total_count += 1;
                }
                (Some(ok), None) => {
                    last_key = Some(ok.to_owned());
                    result_objects.push(objects[obj_idx].clone());
                    obj_idx += 1;
                    total_count += 1;
                }
                (None, Some(pk)) => {
                    last_key = Some(pk.to_owned());
                    result_prefixes.push(common_prefixes_list[prefix_idx].clone());
                    prefix_idx += 1;
                    total_count += 1;
                }
                (None, None) => break,
            }
        }

        let is_truncated = obj_idx < objects.len() || prefix_idx < common_prefixes_list.len();
        // Only hand back a continuation token when there is more to fetch.
        let next_continuation_token = if is_truncated { last_key } else { None };
        let key_count = try_!(i32::try_from(total_count));

        let contents = result_objects.is_empty().not().then_some(result_objects);
        let common_prefixes = result_prefixes.is_empty().not().then_some(result_prefixes);

        let output = ListObjectsV2Output {
            key_count: Some(key_count),
            max_keys: Some(max_keys),
            is_truncated: Some(is_truncated),
            contents,
            common_prefixes,
            delimiter: input.delimiter,
            encoding_type: input.encoding_type,
            name: Some(input.bucket),
            prefix: input.prefix,
            continuation_token: input.continuation_token,
            next_continuation_token,
            start_after: input.start_after,
            ..Default::default()
        };
        Ok(S3Response::new(output))
    }

    #[tracing::instrument(level = "debug")]
    async fn put_object(&self, req: S3Request<PutObjectInput>) -> S3Result<S3Response<PutObjectOutput>> {
        use crate::backend::fs::ObjectAttributes;

        let mut input = req.input;
        if let Some(ref storage_class) = input.storage_class {
            let is_valid = ["STANDARD", "REDUCED_REDUNDANCY"].contains(&storage_class.as_str());
            if !is_valid {
                return Err(s3_error!(InvalidStorageClass));
            }
        }

        let PutObjectInput {
            body,
            bucket,
            key,
            metadata,
            content_length,
            content_md5,
            content_encoding,
            content_type,
            content_disposition,
            content_language,
            cache_control,
            expires,
            website_redirect_location,
            if_none_match,
            ..
        } = input;

        let Some(body) = body else { return Err(s3_error!(IncompleteBody)) };

        // Check If-None-Match condition
        // If-None-Match: * means "only create if the object doesn't exist"
        if let Some(ref condition) = if_none_match
            && condition.is_any()
        {
            let object_path = self.get_object_path(&bucket, &key)?;
            if object_path.exists() {
                return Err(s3_error!(PreconditionFailed, "Object already exists"));
            }
        }

        let mut checksum: s3s::checksum::ChecksumHasher = default();
        if input.checksum_crc32.is_some() {
            checksum.crc32 = Some(default());
        }
        if input.checksum_crc32c.is_some() {
            checksum.crc32c = Some(default());
        }
        if input.checksum_sha1.is_some() {
            checksum.sha1 = Some(default());
        }
        if input.checksum_sha256.is_some() {
            checksum.sha256 = Some(default());
        }
        if input.checksum_crc64nvme.is_some() {
            checksum.crc64nvme = Some(default());
        }
        if let Some(alg) = input.checksum_algorithm {
            match alg.as_str() {
                ChecksumAlgorithm::CRC32 => checksum.crc32 = Some(default()),
                ChecksumAlgorithm::CRC32C => checksum.crc32c = Some(default()),
                ChecksumAlgorithm::SHA1 => checksum.sha1 = Some(default()),
                ChecksumAlgorithm::SHA256 => checksum.sha256 = Some(default()),
                ChecksumAlgorithm::CRC64NVME => checksum.crc64nvme = Some(default()),
                _ => return Err(s3_error!(NotImplemented, "Unsupported checksum algorithm")),
            }
        }

        if key.ends_with('/') {
            if let Some(len) = content_length
                && len > 0
            {
                return Err(s3_error!(UnexpectedContent, "Unexpected request body when creating a directory object."));
            }
            let object_path = self.get_object_path(&bucket, &key)?;
            try_!(fs::create_dir_all(&object_path).await);
            let output = PutObjectOutput::default();
            return Ok(S3Response::new(output));
        }

        let object_path = self.get_object_path(&bucket, &key)?;
        let mut file_writer = self.prepare_file_write(&object_path).await?;

        let mut md5_hash = Md5::new();
        let stream = body.inspect_ok(|bytes| {
            md5_hash.update(bytes.as_ref());
            checksum.update(bytes.as_ref());
        });

        let size = copy_bytes(stream, file_writer.writer()).await?;
        file_writer.done().await?;

        let md5_sum = hex(md5_hash.finalize());

        if let Some(content_md5) = content_md5 {
            let content_md5 = base64_simd::STANDARD
                .decode_to_vec(content_md5)
                .map_err(|_| s3_error!(InvalidArgument))?;
            let content_md5 = hex(content_md5);
            if content_md5 != md5_sum {
                return Err(s3_error!(BadDigest, "content_md5 mismatch"));
            }
        }

        let checksum = checksum.finalize();

        if let Some(trailers) = req.trailing_headers
            && let Some(trailers) = trailers.take()
        {
            if let Some(crc32) = trailers.get("x-amz-checksum-crc32") {
                input.checksum_crc32 = Some(crc32.to_str().map_err(|_| s3_error!(InvalidArgument))?.to_owned());
            }
            if let Some(crc32c) = trailers.get("x-amz-checksum-crc32c") {
                input.checksum_crc32c = Some(crc32c.to_str().map_err(|_| s3_error!(InvalidArgument))?.to_owned());
            }
            if let Some(sha1) = trailers.get("x-amz-checksum-sha1") {
                input.checksum_sha1 = Some(sha1.to_str().map_err(|_| s3_error!(InvalidArgument))?.to_owned());
            }
            if let Some(sha256) = trailers.get("x-amz-checksum-sha256") {
                input.checksum_sha256 = Some(sha256.to_str().map_err(|_| s3_error!(InvalidArgument))?.to_owned());
            }
            if let Some(crc64nvme) = trailers.get("x-amz-checksum-crc64nvme") {
                input.checksum_crc64nvme = Some(crc64nvme.to_str().map_err(|_| s3_error!(InvalidArgument))?.to_owned());
            }
        }

        if checksum.checksum_crc32 != input.checksum_crc32 {
            return Err(s3_error!(
                BadDigest,
                "checksum_crc32 mismatch: expected `{}`, got `{}`",
                input.checksum_crc32.unwrap_or_default(),
                checksum.checksum_crc32.unwrap_or_default()
            ));
        }
        if checksum.checksum_crc32c != input.checksum_crc32c {
            return Err(s3_error!(BadDigest, "checksum_crc32c mismatch"));
        }
        if checksum.checksum_sha1 != input.checksum_sha1 {
            return Err(s3_error!(BadDigest, "checksum_sha1 mismatch"));
        }
        if checksum.checksum_sha256 != input.checksum_sha256 {
            return Err(s3_error!(BadDigest, "checksum_sha256 mismatch"));
        }
        if checksum.checksum_crc64nvme != input.checksum_crc64nvme {
            return Err(s3_error!(BadDigest, "checksum_crc64nvme mismatch"));
        }

        debug!(path = %object_path.display(), ?size, %md5_sum, ?checksum, "write file");

        // Save object attributes (including user metadata and standard attributes)
        let mut obj_attrs = ObjectAttributes {
            user_metadata: metadata,
            content_encoding,
            content_type,
            content_disposition,
            content_language,
            cache_control,
            expires: None,
            website_redirect_location,
        };
        obj_attrs.set_expires_timestamp(expires);
        self.save_object_attributes(&bucket, &key, &obj_attrs, None).await?;

        let mut info: InternalInfo = default();
        crate::backend::checksum::modify_internal_info(&mut info, &checksum);
        // Persist the ETag. It is already computed from the bytes streaming past, so
        // storing it is free here and saves re-reading the whole object to answer a
        // HEAD later -- and it is the only way a listing can report one without
        // hashing every object in the bucket.
        info.insert("etag".to_owned(), serde_json::Value::String(md5_sum.clone()));
        self.save_internal_info(&bucket, &key, &info).await?;

        let output = PutObjectOutput {
            e_tag: Some(ETag::Strong(md5_sum)),
            checksum_crc32: checksum.checksum_crc32,
            checksum_crc32c: checksum.checksum_crc32c,
            checksum_sha1: checksum.checksum_sha1,
            checksum_sha256: checksum.checksum_sha256,
            checksum_crc64nvme: checksum.checksum_crc64nvme,
            ..Default::default()
        };
        Ok(S3Response::new(output))
    }

    #[tracing::instrument(level = "debug")]
    async fn create_multipart_upload(
        &self,
        req: S3Request<CreateMultipartUploadInput>,
    ) -> S3Result<S3Response<CreateMultipartUploadOutput>> {
        use crate::backend::fs::ObjectAttributes;

        let input = req.input;
        let upload_id = self.create_upload_id(req.credentials.as_ref()).await?;

        // Save object attributes (including user metadata and standard attributes)
        let mut obj_attrs = ObjectAttributes {
            user_metadata: input.metadata,
            content_encoding: input.content_encoding,
            content_type: input.content_type,
            content_disposition: input.content_disposition,
            content_language: input.content_language,
            cache_control: input.cache_control,
            expires: None,
            website_redirect_location: input.website_redirect_location,
        };
        obj_attrs.set_expires_timestamp(input.expires);
        self.save_object_attributes(&input.bucket, &input.key, &obj_attrs, Some(upload_id))
            .await?;

        let output = CreateMultipartUploadOutput {
            bucket: Some(input.bucket),
            key: Some(input.key),
            upload_id: Some(upload_id.to_string()),
            ..Default::default()
        };

        Ok(S3Response::new(output))
    }

    #[tracing::instrument(level = "debug")]
    async fn upload_part(&self, req: S3Request<UploadPartInput>) -> S3Result<S3Response<UploadPartOutput>> {
        let UploadPartInput {
            body,
            upload_id,
            part_number,
            ..
        } = req.input;

        check_part_number(part_number)?;

        let body = body.ok_or_else(|| s3_error!(IncompleteBody))?;

        let upload_id = Uuid::parse_str(&upload_id).map_err(|_| s3_error!(InvalidRequest))?;
        if self.verify_upload_id(req.credentials.as_ref(), &upload_id).await?.not() {
            return Err(s3_error!(AccessDenied));
        }

        let file_path = self.resolve_upload_part_path(upload_id, part_number)?;

        let mut md5_hash = Md5::new();
        let stream = body.inspect_ok(|bytes| md5_hash.update(bytes.as_ref()));

        let mut file_writer = self.prepare_file_write(&file_path).await?;
        let size = copy_bytes(stream, file_writer.writer()).await?;
        file_writer.done().await?;

        let md5_sum = hex(md5_hash.finalize());

        debug!(path = %file_path.display(), ?size, %md5_sum, "write file");

        let output = UploadPartOutput {
            e_tag: Some(ETag::Strong(md5_sum)),
            ..Default::default()
        };
        Ok(S3Response::new(output))
    }

    #[tracing::instrument(level = "debug")]
    async fn upload_part_copy(&self, req: S3Request<UploadPartCopyInput>) -> S3Result<S3Response<UploadPartCopyOutput>> {
        let input = req.input;

        let upload_id = Uuid::parse_str(&input.upload_id).map_err(|_| s3_error!(InvalidRequest))?;
        let part_number = input.part_number;
        check_part_number(part_number)?;
        if self.verify_upload_id(req.credentials.as_ref(), &upload_id).await?.not() {
            return Err(s3_error!(AccessDenied));
        }

        let (src_bucket, src_key) = match input.copy_source {
            CopySource::AccessPoint { .. } => return Err(s3_error!(NotImplemented)),
            CopySource::Bucket { ref bucket, ref key, .. } => (bucket, key),
        };
        let src_path = self.get_object_path(src_bucket, src_key)?;
        let dst_path = self.resolve_upload_part_path(upload_id, part_number)?;

        let mut src_file = fs::File::open(&src_path).await.map_err(|e| s3_error!(e, NoSuchKey))?;
        let file_len = try_!(src_file.metadata().await).len();

        // An empty source has no bytes to copy; `file_len - 1` below would underflow.
        if file_len == 0 {
            return Err(s3_error!(InvalidRequest, "copy source is empty"));
        }

        let (start, end) = if let Some(copy_range) = &input.copy_source_range {
            if !copy_range.starts_with("bytes=") {
                return Err(s3_error!(InvalidArgument));
            }
            let range = &copy_range["bytes=".len()..];
            let parts: Vec<&str> = range.split('-').collect();
            if parts.len() != 2 {
                return Err(s3_error!(InvalidArgument));
            }

            let start: u64 = parts[0].parse().map_err(|_| s3_error!(InvalidArgument))?;
            let mut end = file_len - 1;
            if parts[1].is_empty().not() {
                end = parts[1].parse().map_err(|_| s3_error!(InvalidArgument))?;
            }
            (start, end)
        } else {
            (0, file_len - 1)
        };

        // Reject a range that runs past the end of the source or is inverted.
        if start > end || end >= file_len {
            return Err(s3_error!(InvalidArgument, "copy source range is out of bounds"));
        }

        let content_length = end - start + 1;
        let content_length_usize = try_!(usize::try_from(content_length));

        let _ = try_!(src_file.seek(io::SeekFrom::Start(start)).await);
        let body = StreamingBlob::wrap(bytes_stream(ReaderStream::with_capacity(src_file, READ_CHUNK), content_length_usize));

        let mut md5_hash = Md5::new();
        let stream = body.inspect_ok(|bytes| md5_hash.update(bytes.as_ref()));

        let mut file_writer = self.prepare_file_write(&dst_path).await?;
        let size = copy_bytes(stream, file_writer.writer()).await?;
        file_writer.done().await?;

        let md5_sum = hex(md5_hash.finalize());

        debug!(path = %dst_path.display(), ?size, %md5_sum, "write file");

        let output = UploadPartCopyOutput {
            copy_part_result: Some(CopyPartResult {
                e_tag: Some(ETag::Strong(md5_sum)),
                ..Default::default()
            }),
            ..Default::default()
        };

        Ok(S3Response::new(output))
    }

    #[tracing::instrument(level = "debug")]
    async fn list_parts(&self, req: S3Request<ListPartsInput>) -> S3Result<S3Response<ListPartsOutput>> {
        let S3Request { input, credentials, .. } = req;
        let ListPartsInput {
            bucket, key, upload_id, ..
        } = input;

        // Every other multipart operation parses the upload id and checks that the
        // caller owns the session; listing must do the same, or it hands out the
        // part inventory of someone else's upload.
        let upload_uuid = Uuid::parse_str(&upload_id).map_err(|_| s3_error!(InvalidRequest))?;
        if self.verify_upload_id(credentials.as_ref(), &upload_uuid).await?.not() {
            return Err(s3_error!(AccessDenied));
        }

        let mut parts: Vec<Part> = Vec::new();
        let mut iter = try_!(fs::read_dir(&self.root).await);

        let prefix = format!(".upload_id-{upload_uuid}");

        while let Some(entry) = try_!(iter.next_entry().await) {
            let file_type = try_!(entry.file_type().await);
            if file_type.is_file().not() {
                continue;
            }

            let file_name = entry.file_name();
            let Some(name) = file_name.to_str() else { continue };

            let Some(part_segment) = name.strip_prefix(&prefix) else { continue };
            let Some(part_number) = part_segment.strip_prefix(".part-") else { continue };
            // A filename is not a parser guarantee: skip anything unreadable rather
            // than panicking inside the connection task.
            let Ok(part_number) = part_number.parse::<i32>() else { continue };

            let file_meta = try_!(entry.metadata().await);
            let last_modified = Timestamp::from(try_!(file_meta.modified()));
            let size = try_!(i64::try_from(file_meta.len()));

            let part = Part {
                last_modified: Some(last_modified),
                part_number: Some(part_number),
                size: Some(size),
                ..Default::default()
            };
            parts.push(part);
        }

        // `read_dir` order is arbitrary; S3 lists parts in ascending part order.
        parts.sort_by_key(|p| p.part_number);

        let output = ListPartsOutput {
            bucket: Some(bucket),
            key: Some(key),
            upload_id: Some(upload_id),
            parts: Some(parts),
            ..Default::default()
        };
        Ok(S3Response::new(output))
    }

    #[tracing::instrument(level = "debug")]
    async fn complete_multipart_upload(
        &self,
        req: S3Request<CompleteMultipartUploadInput>,
    ) -> S3Result<S3Response<CompleteMultipartUploadOutput>> {
        let CompleteMultipartUploadInput {
            multipart_upload,
            bucket,
            key,
            upload_id,
            ..
        } = req.input;

        let Some(multipart_upload) = multipart_upload else { return Err(s3_error!(InvalidPart)) };

        let upload_id = Uuid::parse_str(&upload_id).map_err(|_| s3_error!(InvalidRequest))?;
        if self.verify_upload_id(req.credentials.as_ref(), &upload_id).await?.not() {
            return Err(s3_error!(AccessDenied));
        }

        self.delete_upload_id(&upload_id).await?;

        if let Ok(Some(attrs)) = self.load_object_attributes(&bucket, &key, Some(upload_id)).await {
            self.save_object_attributes(&bucket, &key, &attrs, None).await?;
            let _ = self.delete_metadata(&bucket, &key, Some(upload_id));
        }

        let object_path = self.get_object_path(&bucket, &key)?;
        let mut file_writer = self.prepare_file_write(&object_path).await?;

        let parts: Vec<_> = multipart_upload.parts.into_iter().flatten().collect();
        let total_parts_cnt = parts.len();
        let mut last_part_number: i32 = 0;
        // Concatenated binary MD5 of each part, hashed at the end to form the
        // AWS-style multipart ETag (`<md5-of-part-md5s>-<part-count>`).
        let mut part_md5s: Vec<u8> = Vec::with_capacity(total_parts_cnt * 16);

        for (idx, part) in parts.into_iter().enumerate() {
            let part_number = part
                .part_number
                .ok_or_else(|| s3_error!(InvalidRequest, "missing part number"))?;
            // Parts must be listed in ascending part-number order, but need not be
            // contiguous (AWS allows gaps, e.g. 1, 3, 5).
            if part_number <= last_part_number {
                return Err(s3_error!(InvalidPartOrder));
            }
            last_part_number = part_number;

            let part_path = self.resolve_upload_part_path(upload_id, part_number)?;

            // Stream the part into the assembled object while hashing it in one pass.
            let mut reader = try_!(fs::File::open(&part_path).await);
            let mut part_md5 = Md5::new();
            let mut buf = vec![0u8; 65536];
            let mut size: u64 = 0;
            loop {
                let n = try_!(reader.read(&mut buf).await);
                if n == 0 {
                    break;
                }
                part_md5.update(&buf[..n]);
                try_!(file_writer.writer().write_all(&buf[..n]).await);
                size += n as u64;
            }
            part_md5s.extend_from_slice(part_md5.finalize().as_ref());

            // Every part except the last one listed must be at least 5 MiB.
            let is_last = idx + 1 == total_parts_cnt;
            if !is_last && size < 5 * 1024 * 1024 {
                return Err(s3_error!(EntityTooSmall));
            }

            debug!(from = %part_path.display(), tmp = %file_writer.tmp_path().display(), to = %file_writer.dest_path().display(), ?size, "write file");
            try_!(fs::remove_file(&part_path).await);
        }
        // Flush the buffered writer before the atomic rename, or the final part's
        // tail can be lost (unlike `tokio::io::copy`, manual writes don't auto-flush).
        try_!(file_writer.writer().flush().await);
        file_writer.done().await?;

        // AWS computes the multipart ETag as MD5 of the concatenated part MD5s,
        // suffixed with the part count. Persist it so GET/HEAD report the same value
        // (it cannot be recomputed from the assembled bytes alone).
        let mut composite = Md5::new();
        composite.update(&part_md5s);
        let etag = format!("{}-{}", hex(composite.finalize()), total_parts_cnt);

        let mut info: InternalInfo = default();
        info.insert("etag".to_owned(), serde_json::Value::String(etag.clone()));
        self.save_internal_info(&bucket, &key, &info).await?;

        let file_size = try_!(fs::metadata(&object_path).await).len();
        debug!(?etag, path = %object_path.display(), size = ?file_size, "multipart complete");

        let output = CompleteMultipartUploadOutput {
            // TODO: better example of AWS-like keep-alive behavior
            future: Some(Box::pin(async move {
                Ok(CompleteMultipartUploadOutput {
                    bucket: Some(bucket),
                    key: Some(key),
                    e_tag: Some(ETag::Strong(etag)),
                    ..Default::default()
                })
            })),
            ..Default::default()
        };

        debug!(?output);

        Ok(S3Response::new(output))
    }

    #[tracing::instrument(level = "debug")]
    async fn abort_multipart_upload(
        &self,
        req: S3Request<AbortMultipartUploadInput>,
    ) -> S3Result<S3Response<AbortMultipartUploadOutput>> {
        let AbortMultipartUploadInput {
            bucket, key, upload_id, ..
        } = req.input;

        let upload_id = Uuid::parse_str(&upload_id).map_err(|_| s3_error!(InvalidRequest))?;
        if self.verify_upload_id(req.credentials.as_ref(), &upload_id).await?.not() {
            return Err(s3_error!(AccessDenied));
        }

        let _ = self.delete_metadata(&bucket, &key, Some(upload_id));

        let prefix = format!(".upload_id-{upload_id}");
        let mut iter = try_!(fs::read_dir(&self.root).await);
        while let Some(entry) = try_!(iter.next_entry().await) {
            let file_type = try_!(entry.file_type().await);
            if file_type.is_file().not() {
                continue;
            }

            let file_name = entry.file_name();
            let Some(name) = file_name.to_str() else { continue };

            if name.starts_with(&prefix) {
                try_!(fs::remove_file(entry.path()).await);
            }
        }

        self.delete_upload_id(&upload_id).await?;

        debug!(bucket = %bucket, key = %key, upload_id = %upload_id, "multipart upload aborted");

        Ok(S3Response::new(AbortMultipartUploadOutput { ..Default::default() }))
    }
}

impl FileSystem {
    /// Remove the on-disk representation of one key, with its sidecars.
    ///
    /// Shared by `DeleteObject` and `DeleteObjects`, which used to disagree: the
    /// batch path skipped every key ending in `/` outright while still reporting it
    /// deleted, so an empty folder placeholder that the single-key path removes
    /// survived a batch delete of the very same key.
    ///
    /// A trailing-slash key is a folder placeholder, stored here as a real
    /// directory, so it can only go away once empty. While keys still live under it
    /// the directory has to stay -- which matches S3, where a prefix keeps existing
    /// for as long as it has objects beneath it.
    async fn remove_object(&self, bucket: &str, key: &str, path: &Path) -> S3Result<()> {
        // `a` and `a/` resolve to the same path, so the key's shape and what is
        // actually on disk can disagree. Deleting the one that is not there is a
        // no-op, not an error: S3 deletes are idempotent, and treating the mismatch
        // as a failure meant `DELETE a/` over a file `a` (or `DELETE a` over a
        // folder placeholder) answered 500 instead.
        let Ok(meta) = fs::metadata(path).await else { return Ok(()) };
        if key.ends_with('/') {
            if !meta.is_dir() {
                return Ok(());
            }
            let mut dir = try_!(fs::read_dir(path).await);
            if try_!(dir.next_entry().await).is_none() {
                try_!(fs::remove_dir(path).await);
            }
            return Ok(());
        }
        if meta.is_dir() {
            return Ok(());
        }
        try_!(fs::remove_file(path).await);
        self.delete_object_sidecars(bucket, key).await?;
        Ok(())
    }

    /// Remove directories left empty by a delete, walking up from `dir` towards
    /// `bucket_root` (exclusive).
    ///
    /// S3 prefixes are implicit: once the last object under `a/b/` is gone, `a/b/`
    /// must stop existing. On disk the key structure is real directories, so
    /// without this a deleted tree lingers as phantom common prefixes in listings
    /// and keeps `DeleteBucket` reporting `BucketNotEmpty` for a bucket the client
    /// has already emptied.
    ///
    /// Best-effort: `remove_dir` only succeeds on an empty directory, so it both
    /// tests and performs each prune, and any failure (still populated, or a racing
    /// writer) simply stops the walk. Explicitly created empty folder placeholders
    /// are unaffected -- they are never the parent of the object being deleted.
    async fn prune_empty_dirs(&self, dir: Option<&Path>, bucket_root: &Path) {
        let Some(dir) = dir else { return };
        let mut dir = dir.to_owned();
        while dir != bucket_root && dir.starts_with(bucket_root) {
            if fs::remove_dir(&dir).await.is_err() {
                break;
            }
            let Some(parent) = dir.parent() else { break };
            dir = parent.to_owned();
        }
    }

    /// The ETag to advertise for an object: the stored multipart ETag
    /// (`<md5-of-part-md5s>-<n>`) when present, otherwise the whole-object MD5.
    async fn object_etag(&self, bucket: &str, key: &str, meta: &std::fs::Metadata) -> S3Result<String> {
        let mut info = self.load_internal_info(bucket, key).await?;
        self.resolve_etag(bucket, key, meta, &mut info).await
    }

    /// The object's whole-body MD5: its stored ETag when that is one, and a fresh
    /// hash only for a multipart ETag (`<md5-of-part-md5s>-<n>`, which is not the
    /// body's MD5). For callers that must compare against another server's MD5.
    pub(crate) async fn object_md5(&self, bucket: &str, key: &str) -> S3Result<String> {
        let path = self.get_object_path(bucket, key)?;
        let meta = try_!(fs::metadata(&path).await);
        let etag = self.object_etag(bucket, key, &meta).await?;
        if etag.contains('-') {
            return Ok(self.get_md5_sum(bucket, key).await?);
        }
        Ok(etag)
    }

    /// The ETag from an already-loaded `info` sidecar, computing and persisting it
    /// when the sidecar has none. `meta` describes the object file being served.
    ///
    /// Objects written before ETags were stored have no `etag`, and hashing them
    /// means reading the whole file -- before a GET can send its first byte, and
    /// for a HEAD that reads no body at all. Doing that on every request doubled
    /// the disk traffic of serving such objects, so the hash is written back and
    /// paid once.
    ///
    /// The write-back carries an [`ETAG_STAMP`] of the bytes it was computed from.
    /// It cannot be made atomic against a concurrent upload of the same key, which
    /// may land between the hash and the write and have its fresh sidecar replaced
    /// by this stale one. A stamp that does not match the file being served marks
    /// exactly that: the whole sidecar (checksums included) describes other bytes,
    /// so it is dropped here and rebuilt, rather than served. A failed write-back
    /// is ignored: it only means the next request hashes again.
    async fn resolve_etag(
        &self,
        bucket: &str,
        key: &str,
        meta: &std::fs::Metadata,
        info: &mut Option<InternalInfo>,
    ) -> S3Result<String> {
        if let Some(i) = info.as_ref() {
            if let Some(etag) = stored_etag(i, meta) {
                return Ok(etag.to_owned());
            }
            if i.contains_key(ETAG_STAMP) {
                *info = None;
            }
        }

        // One hash per object at a time. Right after a deploy, a popular object
        // without a stored ETag is requested by many clients at once, and each
        // would otherwise read the whole file just to compute the same value.
        // Whoever waited re-reads the sidecar the first one wrote.
        let path = self.get_object_path(bucket, key)?;
        let _guard = self.etag_locks.lock(&path).await;
        if let Some(fresh) = self.load_internal_info(bucket, key).await?
            && let Some(etag) = stored_etag(&fresh, meta)
        {
            let etag = etag.to_owned();
            *info = Some(fresh);
            return Ok(etag);
        }

        let stamp = etag_stamp(meta);
        let etag = self.get_md5_sum(bucket, key).await?;
        // `get_md5_sum` opens the path afresh; only store the result if that was
        // still the file `meta` describes.
        if fs::metadata(&path).await.is_ok_and(|m| etag_stamp(&m) == stamp) {
            let info = info.get_or_insert_default();
            info.insert("etag".to_owned(), serde_json::Value::String(etag.clone()));
            info.insert(ETAG_STAMP.to_owned(), serde_json::Value::String(stamp));
            if let Err(err) = self.save_internal_info(bucket, key, info).await {
                debug!(bucket, key, ?err, "failed to persist computed etag");
            }
        }
        Ok(etag)
    }

    async fn list_objects_recursive(
        &self,
        bucket: &str,
        bucket_root: &Path,
        prefix: &str,
        page: &mut ListingPage<'_>,
    ) -> S3Result<()> {
        let mut dir_queue: VecDeque<PathBuf> = default();
        dir_queue.push_back(bucket_root.to_owned());
        let prefix_is_empty = prefix.is_empty();

        while let Some(dir) = dir_queue.pop_front() {
            let mut iter = try_!(fs::read_dir(dir).await);
            while let Some(entry) = try_!(iter.next_entry().await) {
                let file_type = try_!(entry.file_type().await);
                let entry_path = entry.path();
                let key = try_!(entry_path.strip_prefix(bucket_root));
                let Some(key_str) = normalize_path(key, "/") else {
                    continue;
                };

                if file_type.is_dir() {
                    // Descend only where the prefix can still be satisfied: either
                    // this directory sits on the path to it, or everything below it
                    // matches. Walking the whole bucket to filter afterwards made a
                    // narrow prefix listing cost the same as listing everything.
                    if !prefix_is_empty {
                        let dir_prefix = format!("{key_str}/");
                        if !dir_prefix.starts_with(prefix) && !prefix.starts_with(&dir_prefix) {
                            continue;
                        }
                    }
                    dir_queue.push_back(entry_path);
                } else {
                    if !prefix_is_empty && !key_str.starts_with(prefix) {
                        continue;
                    }
                    // Skip the `stat` for keys that cannot reach the page anyway.
                    if !page.accepts(&key_str) {
                        continue;
                    }

                    let metadata = try_!(entry.metadata().await);
                    let last_modified = Timestamp::from(try_!(metadata.modified()));
                    let size = metadata.len();

                    let object = Object {
                        key: Some(key_str.clone()),
                        last_modified: Some(last_modified),
                        size: Some(try_!(i64::try_from(size))),
                        // Clients that sync against this server (mc, rclone, aws s3
                        // sync) compare the listing's ETag to decide what to re-copy.
                        // Without one they cannot tell an identical object from a
                        // changed one and re-transfer the whole bucket every run.
                        e_tag: self.load_etag(bucket, &key_str, &metadata).await.map(ETag::Strong),
                        ..Default::default()
                    };
                    page.push_object(key_str, object);
                }
            }
        }

        Ok(())
    }

    async fn list_objects_with_delimiter(
        &self,
        bucket: &str,
        bucket_root: &Path,
        prefix: &str,
        delimiter: &str,
        page: &mut ListingPage<'_>,
    ) -> S3Result<()> {
        // For delimiter-based listing, we need to recursively scan all files
        // but group them according to the delimiter rules
        let mut dir_queue: VecDeque<PathBuf> = default();
        dir_queue.push_back(bucket_root.to_owned());
        let prefix_is_empty = prefix.is_empty();

        while let Some(dir) = dir_queue.pop_front() {
            let mut iter = try_!(fs::read_dir(dir).await);

            while let Some(entry) = try_!(iter.next_entry().await) {
                let file_type = try_!(entry.file_type().await);
                let entry_path = entry.path();

                // Calculate the key relative to the bucket root
                let key = try_!(entry_path.strip_prefix(bucket_root));
                let Some(key_str) = normalize_path(key, "/") else {
                    continue;
                };

                // Skip if doesn't match prefix
                if !prefix_is_empty && !key_str.starts_with(prefix) {
                    // For directories, also skip if they don't have potential to contain matching files
                    if file_type.is_dir() && !prefix.starts_with(&key_str) && !key_str.starts_with(prefix) {
                        continue;
                    }
                    if file_type.is_file() {
                        continue;
                    }
                }

                if file_type.is_dir() {
                    if delimiter == "/" {
                        // For the path-separator delimiter the on-disk tree maps
                        // directly onto key prefixes. A directory below the delimiter
                        // boundary collapses entirely into a single common prefix
                        // (covering any explicitly created empty folder too), so record
                        // it and skip descending — that prunes the whole subtree from
                        // the walk instead of reading every nested file.
                        let folder = format!("{key_str}{delimiter}");
                        if let Some(rest) = folder.strip_prefix(prefix)
                            && let Some(pos) = rest.find(delimiter)
                        {
                            let mut cp = String::with_capacity(prefix.len() + pos + delimiter.len());
                            cp.push_str(prefix);
                            cp.push_str(&rest[..pos + delimiter.len()]);
                            page.push_prefix(cp);
                        } else {
                            // The prefix directory itself or an ancestor of it: descend
                            // to reach the entries at the listing level.
                            dir_queue.push_back(entry_path);
                        }
                    } else {
                        // Other delimiters don't map to the directory structure, so we
                        // still have to walk every file and group by the delimiter.
                        dir_queue.push_back(entry_path);
                    }
                } else {
                    // For files, determine if they should be listed directly or as common prefixes
                    let remaining = &key_str[prefix.len()..];

                    if remaining.contains(delimiter) {
                        // File is in a subdirectory, add the subdirectory as common prefix
                        if let Some(delimiter_pos) = remaining.find(delimiter) {
                            let mut next_prefix = String::with_capacity(prefix.len() + delimiter_pos + 1);
                            next_prefix.push_str(prefix);
                            next_prefix.push_str(&remaining[..=delimiter_pos]);
                            page.push_prefix(next_prefix);
                        }
                    } else {
                        // File is at the current level, include it in objects.
                        // Skip the `stat` for keys that cannot reach the page anyway.
                        if !page.accepts(&key_str) {
                            continue;
                        }
                        let metadata = try_!(entry.metadata().await);
                        let last_modified = Timestamp::from(try_!(metadata.modified()));
                        let size = metadata.len();

                        let object = Object {
                            key: Some(key_str.clone()),
                            last_modified: Some(last_modified),
                            size: Some(try_!(i64::try_from(size))),
                            // See the note in `list_objects_recursive`: without an
                            // ETag here, syncing clients re-copy everything.
                            e_tag: self.load_etag(bucket, &key_str, &metadata).await.map(ETag::Strong),
                            ..Default::default()
                        };
                        page.push_object(key_str, object);
                    }
                }
            }
        }

        Ok(())
    }
}
