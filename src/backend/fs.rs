// Derived from the Apache-2.0 licensed `s3s-fs` reference implementation
// (https://github.com/Nugine/s3s, Copyright 2023 Nugine).
// Modified for this project. See NOTICE and LICENSE.

use crate::backend::error::*;
use crate::backend::utils::hex;

use s3s::auth::Credentials;
use s3s::crypto::Checksum;
use s3s::crypto::Md5;
use s3s::dto;
use s3s::dto::PartNumber;

use std::collections::HashMap;
use std::env;
use std::fs::Metadata;
use std::ops::Not;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use tokio::fs;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufWriter};

use path_absolutize::Absolutize;
use uuid::Uuid;

#[derive(Debug)]
pub struct FileSystem {
    pub(crate) root: PathBuf,
    tmp_file_counter: AtomicU64,
    /// Serializes ETag backfills per object; see `resolve_etag`.
    pub(crate) etag_locks: KeyLocks,
}

/// One async lock per path, held in the map only while someone holds or waits
/// for it, so the map stays as small as the set of paths currently contended.
#[derive(Debug, Default)]
pub(crate) struct KeyLocks {
    locks: Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>,
}

impl KeyLocks {
    /// Wait for exclusive use of `path`. Released when the guard drops; a request
    /// cancelled while still waiting cleans up after itself the same way.
    pub(crate) async fn lock(&self, path: &Path) -> KeyGuard<'_> {
        let entry = KeyEntry {
            lock: Arc::clone(self.locks.lock().unwrap().entry(path.to_owned()).or_default()),
            locks: self,
            path: path.to_owned(),
        };
        let guard = Arc::clone(&entry.lock).lock_owned().await;
        KeyGuard { _guard: guard, _entry: entry }
    }
}

/// Holds the lock. Fields drop in order: the mutex is released first, then the
/// entry decides whether the map still needs it.
pub(crate) struct KeyGuard<'a> {
    _guard: tokio::sync::OwnedMutexGuard<()>,
    _entry: KeyEntry<'a>,
}

/// One holder's or waiter's reference to a path's lock.
struct KeyEntry<'a> {
    locks: &'a KeyLocks,
    path: PathBuf,
    lock: Arc<tokio::sync::Mutex<()>>,
}

impl Drop for KeyEntry<'_> {
    fn drop(&mut self) {
        let mut locks = self.locks.locks.lock().unwrap();
        // Every holder and waiter clones the `Arc` under this map lock, so the
        // count is stable here: two means only the map and this entry are left.
        if Arc::strong_count(&self.lock) == 2 {
            locks.remove(&self.path);
        }
    }
}

pub(crate) type InternalInfo = serde_json::Map<String, serde_json::Value>;

/// An in-progress multipart upload, reconstructed from the on-disk session files.
///
/// `s3s`/this backend does not implement `ListMultipartUploads`, so the admin
/// panel reconstructs the list by scanning the data root for `.upload-{uuid}.json`
/// session markers and the matching `*.upload-{uuid}.metadata.json` sidecars.
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct MultipartUploadInfo {
    pub upload_id: String,
    pub bucket: Option<String>,
    pub key: Option<String>,
    pub initiated_unix: Option<i64>,
}

/// Stores standard object attributes alongside user metadata
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct ObjectAttributes {
    /// User-defined metadata (x-amz-meta-*)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_metadata: Option<dto::Metadata>,

    /// Standard object attributes
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_encoding: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_disposition: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_language: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub website_redirect_location: Option<String>,
}

impl ObjectAttributes {
    /// Convert expires Timestamp to String for storage
    pub fn set_expires_timestamp(&mut self, expires: Option<dto::Timestamp>) {
        self.expires = expires.and_then(|ts| {
            let mut buf = Vec::new();
            match ts.format(dto::TimestampFormat::DateTime, &mut buf) {
                Ok(()) => Some(String::from_utf8_lossy(&buf).into_owned()),
                Err(_) => None,
            }
        });
    }

    /// Parse expires String back to Timestamp
    pub fn get_expires_timestamp(&self) -> Option<dto::Timestamp> {
        self.expires
            .as_ref()
            .and_then(|s| dto::Timestamp::parse(dto::TimestampFormat::DateTime, s).ok())
    }
}

/// Remove `path`, treating "already gone" as success.
pub(crate) async fn remove_file_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Sidecar field recording which bytes a *backfilled* ETag was computed from.
///
/// Write paths store the ETag together with the bytes, so theirs is trusted as is.
/// A read that finds none computes it later and stores it, but by then an upload
/// may have replaced the object and written its own sidecar, which the backfill
/// would then overwrite with the old bytes' ETag and checksums. The stamp lets a
/// later read notice that and throw the stale sidecar away instead of serving it.
pub(crate) const ETAG_STAMP: &str = "etag_stamp";

/// Identifies one version of an object file. Every write lands via tmp + rename,
/// so a rewrite always gets a new inode even when size and mtime collide.
pub(crate) fn etag_stamp(meta: &Metadata) -> String {
    #[cfg(unix)]
    let ino = std::os::unix::fs::MetadataExt::ino(meta);
    #[cfg(not(unix))]
    let ino = 0;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos());
    format!("{}:{ino}:{mtime}", meta.len())
}

/// The ETag in `info`, unless it was backfilled for bytes other than the ones
/// `meta` describes -- in which case the whole sidecar describes another object.
pub(crate) fn stored_etag<'a>(info: &'a InternalInfo, meta: &Metadata) -> Option<&'a str> {
    if let Some(stamp) = info.get(ETAG_STAMP).and_then(|v| v.as_str())
        && stamp != etag_stamp(meta)
    {
        return None;
    }
    info.get("etag")?.as_str()
}

/// Read `path`, treating "not there" as `None`.
///
/// Every GET reads two sidecars through here, so it must not pre-check with the
/// synchronous `Path::exists`: that is a blocking `stat` on an async worker
/// thread, and under load it stalls the runtime on disk latency.
async fn read_if_exists(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::read(path).await {
        Ok(content) => Ok(Some(content)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn clean_old_tmp_files(root: &Path) -> std::io::Result<()> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => Ok(entries),
        Err(ref io_err) if io_err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(io_err) => Err(io_err),
    }?;
    for entry in entries {
        let entry = entry?;
        let file_name = entry.file_name();
        let Some(file_name) = file_name.to_str() else { continue };
        // See `FileSystem::prepare_file_write`
        if file_name.starts_with(".tmp.") && file_name.ends_with(".internal.part") {
            std::fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

impl FileSystem {
    pub fn new(root: impl AsRef<Path>) -> Result<Self> {
        let root = env::current_dir()?.join(root).canonicalize()?;
        clean_old_tmp_files(&root)?;
        let tmp_file_counter = AtomicU64::new(0);
        Ok(Self {
            root,
            tmp_file_counter,
            etag_locks: KeyLocks::default(),
        })
    }

    /// The canonicalized data root. Used to locate sidecar state (e.g. the
    /// settings DB) alongside the object data.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn resolve_abs_path(&self, path: impl AsRef<Path>) -> Result<PathBuf> {
        Ok(path.as_ref().absolutize_virtually(&self.root)?.into_owned())
    }

    pub(crate) fn resolve_upload_part_path(&self, upload_id: Uuid, part_number: PartNumber) -> Result<PathBuf> {
        self.resolve_abs_path(format!(".upload_id-{upload_id}.part-{part_number}"))
    }

    /// resolve object path under the virtual root
    ///
    /// The resolved path is confined to the object's *own bucket* directory, not
    /// merely the data root. A key such as `../other-bucket/secret` resolves to a
    /// path that is still inside the data root, so `absolutize_virtually` alone
    /// would accept it and leak a sibling bucket's objects (critically, anonymous
    /// reads through the public port). Reject any key that escapes its bucket.
    pub(crate) fn get_object_path(&self, bucket: &str, key: &str) -> Result<PathBuf> {
        let bucket_root = self.get_bucket_path(bucket)?;
        let dir = Path::new(&bucket);
        let file_path = Path::new(&key);
        let resolved = self.resolve_abs_path(dir.join(file_path))?;
        if !resolved.starts_with(&bucket_root) {
            return Err(Error::from_string(format!("object key escapes its bucket: {key}")));
        }
        Ok(resolved)
    }

    /// resolve bucket path under the virtual root
    ///
    /// The name is checked against the AWS bucket-naming rules first. `s3s` already
    /// rejects malformed names while parsing S3 requests, but the admin panel calls
    /// the backend directly, so without this guard a name like `.s3-storage` would
    /// resolve to an internal directory under the data root and `DeleteBucket` would
    /// happily remove it. Every bucket path in the backend funnels through here.
    pub(crate) fn get_bucket_path(&self, bucket: &str) -> Result<PathBuf> {
        if !s3s::path::check_bucket_name(bucket) {
            return Err(Error::from_string(format!("invalid bucket name: {bucket}")));
        }
        let dir = Path::new(&bucket);
        self.resolve_abs_path(dir)
    }

    /// resolve metadata path under the virtual root (custom format)
    pub(crate) fn get_metadata_path(&self, bucket: &str, key: &str, upload_id: Option<Uuid>) -> Result<PathBuf> {
        let encode = |s: &str| base64_simd::URL_SAFE_NO_PAD.encode_to_string(s);
        let u_ext = upload_id.map(|u| format!(".upload-{u}")).unwrap_or_default();
        let file_path = format!(".bucket-{}.object-{}{u_ext}.metadata.json", encode(bucket), encode(key));
        self.resolve_abs_path(file_path)
    }

    pub(crate) fn get_internal_info_path(&self, bucket: &str, key: &str) -> Result<PathBuf> {
        let encode = |s: &str| base64_simd::URL_SAFE_NO_PAD.encode_to_string(s);
        let file_path = format!(".bucket-{}.object-{}.internal.json", encode(bucket), encode(key));
        self.resolve_abs_path(file_path)
    }

    /// load object attributes from fs (with backward compatibility)
    pub(crate) async fn load_object_attributes(
        &self,
        bucket: &str,
        key: &str,
        upload_id: Option<Uuid>,
    ) -> Result<Option<ObjectAttributes>> {
        let path = self.get_metadata_path(bucket, key, upload_id)?;
        let Some(content) = read_if_exists(&path).await? else { return Ok(None) };

        // Try to deserialize as ObjectAttributes first (new format)
        if let Ok(attrs) = serde_json::from_slice::<ObjectAttributes>(&content) {
            return Ok(Some(attrs));
        }

        // Fall back to old format (just user metadata)
        if let Ok(user_metadata) = serde_json::from_slice::<dto::Metadata>(&content) {
            return Ok(Some(ObjectAttributes {
                user_metadata: Some(user_metadata),
                ..Default::default()
            }));
        }

        Ok(None)
    }

    /// save object attributes to fs
    pub(crate) async fn save_object_attributes(
        &self,
        bucket: &str,
        key: &str,
        attrs: &ObjectAttributes,
        upload_id: Option<Uuid>,
    ) -> Result<()> {
        let path = self.get_metadata_path(bucket, key, upload_id)?;
        let content = serde_json::to_vec(attrs)?;
        let mut file_writer = self.prepare_file_write(&path).await?;
        file_writer.writer().write_all(&content).await?;
        file_writer.writer().flush().await?;
        file_writer.done().await?;
        Ok(())
    }

    /// remove metadata from fs
    pub(crate) fn delete_metadata(&self, bucket: &str, key: &str, upload_id: Option<Uuid>) -> Result<()> {
        let path = self.get_metadata_path(bucket, key, upload_id)?;
        std::fs::remove_file(path)?;
        Ok(())
    }

    /// Remove both sidecars belonging to an object.
    ///
    /// The metadata and checksum sidecars live at the data root, not inside the
    /// bucket directory, so removing the object file alone leaves them behind
    /// forever. Besides the unbounded leak (their base64 names also spell out
    /// deleted keys), a survivor can later be adopted by a *different* object
    /// written to the same key and make the server advertise the dead object's
    /// checksums or multipart ETag. Missing files are not an error.
    pub(crate) async fn delete_object_sidecars(&self, bucket: &str, key: &str) -> Result<()> {
        remove_file_if_exists(&self.get_metadata_path(bucket, key, None)?).await?;
        remove_file_if_exists(&self.get_internal_info_path(bucket, key)?).await?;
        Ok(())
    }

    pub(crate) async fn load_internal_info(&self, bucket: &str, key: &str) -> Result<Option<InternalInfo>> {
        let path = self.get_internal_info_path(bucket, key)?;
        let Some(content) = read_if_exists(&path).await? else { return Ok(None) };
        let map = serde_json::from_slice(&content)?;
        Ok(Some(map))
    }

    pub(crate) async fn save_internal_info(&self, bucket: &str, key: &str, info: &InternalInfo) -> Result<()> {
        let path = self.get_internal_info_path(bucket, key)?;
        let content = serde_json::to_vec(info)?;
        let mut file_writer = self.prepare_file_write(&path).await?;
        file_writer.writer().write_all(&content).await?;
        file_writer.writer().flush().await?;
        file_writer.done().await?;
        Ok(())
    }

    /// The ETag recorded when the object was written, if there is one.
    ///
    /// Listings need an ETag per key and cannot afford `get_md5_sum` for each: that
    /// would make listing a bucket cost a full read of every object in it. So the
    /// hash is persisted at write time and simply read back here.
    ///
    /// Objects written before it was persisted have no stored ETag and list without
    /// one, rather than making every listing pay to backfill them; the first GET or
    /// HEAD of such an object stores one. Errors are swallowed for the same reason
    /// -- a listing should not fail because one sidecar is unreadable.
    ///
    /// `meta` is the object file's metadata, which the listing already holds; a
    /// backfilled ETag that no longer matches it is not reported (see [`stored_etag`]).
    pub(crate) async fn load_etag(&self, bucket: &str, key: &str, meta: &Metadata) -> Option<String> {
        let info = self.load_internal_info(bucket, key).await.ok()??;
        stored_etag(&info, meta).map(ToOwned::to_owned)
    }

    /// get md5 sum
    pub(crate) async fn get_md5_sum(&self, bucket: &str, key: &str) -> Result<String> {
        let object_path = self.get_object_path(bucket, key)?;
        let mut file = File::open(&object_path).await?;
        let mut buf = vec![0; 65536];
        let mut md5_hash = Md5::new();
        loop {
            let nread = file.read(&mut buf).await?;
            if nread == 0 {
                break;
            }
            md5_hash.update(&buf[..nread]);
        }
        Ok(hex(md5_hash.finalize()))
    }

    fn get_upload_info_path(&self, upload_id: &Uuid) -> Result<PathBuf> {
        self.resolve_abs_path(format!(".upload-{upload_id}.json"))
    }

    pub(crate) async fn create_upload_id(&self, cred: Option<&Credentials>) -> Result<Uuid> {
        let upload_id = Uuid::new_v4();
        let upload_info_path = self.get_upload_info_path(&upload_id)?;

        let ak: Option<&str> = cred.map(|c| c.access_key.as_str());

        let content = serde_json::to_vec(&ak)?;
        let mut file_writer = self.prepare_file_write(&upload_info_path).await?;
        file_writer.writer().write_all(&content).await?;
        file_writer.writer().flush().await?;
        file_writer.done().await?;

        Ok(upload_id)
    }

    pub(crate) async fn verify_upload_id(&self, cred: Option<&Credentials>, upload_id: &Uuid) -> Result<bool> {
        let upload_info_path = self.get_upload_info_path(upload_id)?;
        if upload_info_path.exists().not() {
            return Ok(false);
        }

        let content = fs::read(&upload_info_path).await?;
        let ak: Option<String> = serde_json::from_slice(&content)?;

        Ok(ak.as_deref() == cred.map(|c| c.access_key.as_str()))
    }

    pub(crate) async fn delete_upload_id(&self, upload_id: &Uuid) -> Result<()> {
        let upload_info_path = self.get_upload_info_path(upload_id)?;
        if upload_info_path.exists() {
            fs::remove_file(&upload_info_path).await?;
        }
        Ok(())
    }

    /// List all in-progress multipart uploads by scanning the data root.
    ///
    /// Pairs each `.upload-{uuid}.json` session marker with its
    /// `.bucket-{b64}.object-{b64}.upload-{uuid}.metadata.json` sidecar (when
    /// present) to recover the target bucket/key, using the marker's mtime as the
    /// initiation time.
    pub(crate) async fn list_multipart_uploads(&self) -> Result<Vec<MultipartUploadInfo>> {
        let decode = |s: &str| -> Option<String> {
            base64_simd::URL_SAFE_NO_PAD
                .decode_to_vec(s)
                .ok()
                .and_then(|b| String::from_utf8(b).ok())
        };

        let mut bucket_key: HashMap<String, (String, String)> = HashMap::new();
        let mut uploads: Vec<(String, Option<i64>)> = Vec::new();

        let mut rd = fs::read_dir(&self.root).await?;
        while let Some(entry) = rd.next_entry().await? {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };

            if let Some(rest) = name.strip_prefix(".upload-")
                && let Some(uuid) = rest.strip_suffix(".json")
            {
                let initiated = entry
                    .metadata()
                    .await
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .and_then(|d| i64::try_from(d.as_secs()).ok());
                uploads.push((uuid.to_owned(), initiated));
            } else if name.starts_with(".bucket-") && name.ends_with(".metadata.json") {
                // .bucket-{b64}.object-{b64}.upload-{uuid}.metadata.json
                let parts: Vec<&str> = name.split('.').collect();
                if parts.len() == 6
                    && let Some(b) = parts[1].strip_prefix("bucket-")
                    && let Some(k) = parts[2].strip_prefix("object-")
                    && let Some(uuid) = parts[3].strip_prefix("upload-")
                    && let (Some(bucket), Some(key)) = (decode(b), decode(k))
                {
                    bucket_key.insert(uuid.to_owned(), (bucket, key));
                }
            }
        }

        let mut result: Vec<MultipartUploadInfo> = uploads
            .into_iter()
            .map(|(uuid, initiated)| {
                let bk = bucket_key.get(&uuid);
                MultipartUploadInfo {
                    upload_id: uuid,
                    bucket: bk.map(|(b, _)| b.clone()),
                    key: bk.map(|(_, k)| k.clone()),
                    initiated_unix: initiated,
                }
            })
            .collect();
        result.sort_by(|a, b| a.upload_id.cmp(&b.upload_id));
        Ok(result)
    }

    /// Count objects and total bytes in a bucket in a single directory walk.
    ///
    /// The admin dashboard used to derive this by paging `ListObjectsV2`, but every
    /// page re-walks the whole bucket, so the cost grew quadratically with the object
    /// count. One pass, and no per-key allocation.
    pub(crate) async fn bucket_usage(&self, bucket: &str) -> Result<(u64, u64)> {
        let root = self.get_bucket_path(bucket)?;
        let mut count: u64 = 0;
        let mut size: u64 = 0;
        let mut queue = std::collections::VecDeque::from([root]);
        while let Some(dir) = queue.pop_front() {
            let mut rd = match fs::read_dir(&dir).await {
                Ok(rd) => rd,
                // The bucket (or a subdirectory) vanishing mid-walk is not an error.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            };
            while let Some(entry) = rd.next_entry().await? {
                let file_type = entry.file_type().await?;
                if file_type.is_dir() {
                    queue.push_back(entry.path());
                } else {
                    count += 1;
                    size += entry.metadata().await?.len();
                }
            }
        }
        Ok((count, size))
    }

    /// Size and modification time of a stored object, or `None` if we do not
    /// hold it.
    ///
    /// Deliberately a bare `stat`: it does not read the metadata or checksum
    /// sidecars, and it never hashes the body. The sync planner calls this once
    /// per source key, so anything more would make an incremental run cost the
    /// same as a full one. `get_object_path` does the bucket-escape check, so
    /// this is also safe to call with a key that came off the network.
    ///
    /// A key ending in `/` is a folder placeholder, stored as a real directory;
    /// it reports size 0 so it compares equal to the empty source object.
    pub(crate) async fn stat_object(&self, bucket: &str, key: &str) -> Result<Option<(u64, SystemTime)>> {
        let path = self.get_object_path(bucket, key)?;
        let meta = match fs::metadata(&path).await {
            Ok(meta) => meta,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        // A directory only counts as an object when the key says so; otherwise a
        // key that happens to name a prefix would look like a zero-byte object
        // and be skipped forever.
        if meta.is_dir() != key.ends_with('/') {
            return Ok(None);
        }
        let size = if meta.is_dir() { 0 } else { meta.len() };
        let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        Ok(Some((size, modified)))
    }

    /// Write to the filesystem atomically.
    /// This is done by first writing to a temporary location and then moving the file.
    pub(crate) async fn prepare_file_write<'a>(&self, path: &'a Path) -> Result<FileWriter<'a>> {
        let tmp_name = format!(".tmp.{}.internal.part", self.tmp_file_counter.fetch_add(1, Ordering::SeqCst));
        let tmp_path = self.resolve_abs_path(tmp_name)?;
        let file = File::create(&tmp_path).await?;
        let writer = BufWriter::new(file);
        Ok(FileWriter {
            tmp_path,
            dest_path: path,
            writer,
            clean_tmp: true,
        })
    }
}

pub(crate) struct FileWriter<'a> {
    tmp_path: PathBuf,
    dest_path: &'a Path,
    writer: BufWriter<File>,
    clean_tmp: bool,
}

impl<'a> FileWriter<'a> {
    pub(crate) fn tmp_path(&self) -> &Path {
        &self.tmp_path
    }

    pub(crate) fn dest_path(&self) -> &'a Path {
        self.dest_path
    }

    pub(crate) fn writer(&mut self) -> &mut BufWriter<File> {
        &mut self.writer
    }

    pub(crate) async fn done(mut self) -> Result<()> {
        if let Some(final_dir_path) = self.dest_path().parent() {
            fs::create_dir_all(&final_dir_path).await?;
        }

        fs::rename(&self.tmp_path, self.dest_path()).await?;
        self.clean_tmp = false;
        Ok(())
    }
}

impl Drop for FileWriter<'_> {
    fn drop(&mut self) {
        if self.clean_tmp {
            let _ = std::fs::remove_file(&self.tmp_path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    #[tokio::test]
    async fn key_locks_exclude_per_path_and_forget_released_paths() {
        let locks = Arc::new(KeyLocks::default());
        let inside = Arc::new(AtomicUsize::new(0));
        let tasks: Vec<_> = (0..8)
            .map(|_| {
                let (locks, inside) = (Arc::clone(&locks), Arc::clone(&inside));
                tokio::spawn(async move {
                    let _guard = locks.lock(Path::new("a")).await;
                    assert_eq!(inside.fetch_add(1, Ordering::SeqCst), 0, "two holders at once");
                    tokio::time::sleep(Duration::from_millis(2)).await;
                    inside.fetch_sub(1, Ordering::SeqCst);
                })
            })
            .collect();
        for task in tasks {
            task.await.unwrap();
        }
        assert!(locks.locks.lock().unwrap().is_empty(), "released paths must leave the map");
    }

    #[tokio::test]
    async fn a_waiter_cancelled_before_it_gets_the_lock_leaves_nothing_behind() {
        let locks = KeyLocks::default();
        let held = locks.lock(Path::new("a")).await;
        // Give up on the second lock while the first is still held.
        let waited = tokio::time::timeout(Duration::from_millis(10), locks.lock(Path::new("a"))).await;
        assert!(waited.is_err());
        drop(held);
        assert!(locks.locks.lock().unwrap().is_empty(), "a cancelled waiter must not leak its entry");
    }
}
