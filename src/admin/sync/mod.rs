//! Pull objects from a remote S3-compatible source (typically MinIO) into local
//! storage, incrementally and repeatably.
//!
//! A run lists the source bucket, decides per object whether we already hold an
//! equivalent copy (see [`plan`]), and streams the rest through the backend's
//! own `put_object`, so synced objects are indistinguishable from uploaded ones.
//!
//! # Resumption
//!
//! There is no manifest, journal or cursor, and none is needed. Every write goes
//! through `FileWriter`, which stages to a temp file and finishes with an atomic
//! rename; a failed or dropped write removes the temp file instead. A cancelled
//! or crashed run therefore leaves only *complete* objects behind, never a
//! truncated one, so simply re-running skips what landed and copies the rest.
//!
//! # Credentials
//!
//! Source credentials arrive with each request and are held only for the
//! duration of the run. They are deliberately not written to the settings
//! database: it holds no secrets today, and it sits in the data volume beside
//! the objects and inside every backup of it. Repeatability comes from the run
//! being incremental, not from the server remembering a password.

pub(crate) mod client;
pub(crate) mod engine;
pub(crate) mod plan;

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use tokio::sync::watch;

use self::client::{DEFAULT_REGION, SourceConfig};
use self::plan::SyncMode;

/// Most failures recorded per run. A systematically broken source (every object
/// 403s) must not grow this without bound.
const MAX_RECORDED_ERRORS: usize = 100;

/// Finished runs kept for the panel's history list.
const HISTORY_LIMIT: usize = 10;

/// Default number of objects copied at once.
const DEFAULT_CONCURRENCY: usize = 4;
/// Upper bound on caller-supplied concurrency. Past this the source, not us, is
/// the bottleneck, and each worker holds a whole in-flight response.
const MAX_CONCURRENCY: usize = 16;

// ---- request ----

/// The JSON body accepted by `POST /api/sync/runs` and `/api/sync/preview`.
#[derive(serde::Deserialize)]
pub(crate) struct SyncBody {
    pub endpoint: String,
    #[serde(default)]
    pub region: Option<String>,
    pub access_key: String,
    pub secret_key: String,
    #[serde(default)]
    pub session_token: Option<String>,
    #[serde(default = "default_path_style")]
    pub path_style: bool,
    #[serde(default)]
    pub ca_pem: Option<String>,

    pub src_bucket: String,
    #[serde(default)]
    pub src_prefix: Option<String>,
    pub dst_bucket: String,
    #[serde(default)]
    pub dst_prefix: Option<String>,

    #[serde(default)]
    pub mode: SyncMode,
    #[serde(default)]
    pub concurrency: Option<usize>,
    #[serde(default)]
    pub skew_secs: i64,
    #[serde(default)]
    pub verify_etag: bool,
    #[serde(default)]
    pub create_bucket: bool,
    #[serde(default)]
    pub max_objects: Option<u64>,
    #[serde(default)]
    pub max_bytes: Option<u64>,
}

/// MinIO is normally reached by host or IP without wildcard DNS, so virtual-host
/// addressing (`bucket.host`) would not resolve. Path style is the safe default.
fn default_path_style() -> bool {
    true
}

/// A validated run: the source connection on one side, the scope and options on
/// the other.
pub(crate) struct SyncRequest {
    pub source: SourceConfig,
    pub src_bucket: String,
    pub src_prefix: String,
    pub dst_bucket: String,
    pub dst_prefix: String,
    pub mode: SyncMode,
    pub concurrency: usize,
    pub skew_secs: i64,
    pub verify_etag: bool,
    pub create_bucket: bool,
    pub max_objects: Option<u64>,
    pub max_bytes: Option<u64>,
}

impl SyncRequest {
    /// Validate a request body, or explain what is wrong with it.
    ///
    /// Bucket names are *not* checked here -- the destination goes through
    /// `check_bucket` in the API layer, which is the same guard every other
    /// admin write uses.
    pub(crate) fn from_body(body: SyncBody) -> Result<Self, String> {
        client::validate_endpoint(&body.endpoint)?;
        if body.access_key.is_empty() || body.secret_key.is_empty() {
            return Err("source access key and secret key are both required".to_owned());
        }
        if body.src_bucket.is_empty() {
            return Err("a source bucket is required".to_owned());
        }
        if let Some(c) = body.concurrency
            && (c == 0 || c > MAX_CONCURRENCY)
        {
            return Err(format!("concurrency must be between 1 and {MAX_CONCURRENCY}"));
        }

        let region = body
            .region
            .filter(|r| !r.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_REGION.to_owned());

        Ok(Self {
            source: SourceConfig {
                endpoint: body.endpoint,
                region,
                access_key: body.access_key,
                secret_key: body.secret_key,
                session_token: body.session_token,
                path_style: body.path_style,
                ca_pem: body.ca_pem,
            },
            src_bucket: body.src_bucket,
            src_prefix: body.src_prefix.unwrap_or_default(),
            dst_bucket: body.dst_bucket,
            dst_prefix: normalize_prefix(body.dst_prefix.unwrap_or_default()),
            mode: body.mode,
            concurrency: body.concurrency.unwrap_or(DEFAULT_CONCURRENCY).clamp(1, MAX_CONCURRENCY),
            skew_secs: body.skew_secs,
            verify_etag: body.verify_etag,
            create_bucket: body.create_bucket,
            max_objects: body.max_objects,
            max_bytes: body.max_bytes,
        })
    }

    /// A human-readable source description. Never includes credentials -- it is
    /// echoed back in every status poll and stored in the run history.
    pub(crate) fn source_label(&self) -> String {
        format!("{}/{}/{}", self.source.endpoint.trim_end_matches('/'), self.src_bucket, self.src_prefix)
    }

    pub(crate) fn destination_label(&self) -> String {
        format!("{}/{}", self.dst_bucket, self.dst_prefix)
    }
}

/// Never absolute, and always ends with `/` when non-empty -- the same shape the
/// archive-extract handler applies to its destination prefix.
pub(crate) fn normalize_prefix(prefix: String) -> String {
    let p = prefix.trim_start_matches('/').to_owned();
    if p.is_empty() || p.ends_with('/') { p } else { format!("{p}/") }
}

// ---- live state ----

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SyncStatus {
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl SyncStatus {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            SyncStatus::Running => "running",
            SyncStatus::Completed => "completed",
            SyncStatus::Failed => "failed",
            SyncStatus::Cancelled => "cancelled",
        }
    }
}

/// Counters written by the workers and read by every status poll.
#[derive(Debug, Default)]
pub(crate) struct SyncProgress {
    pub listed: AtomicU64,
    pub copied: AtomicU64,
    pub skipped: AtomicU64,
    pub failed: AtomicU64,
    pub bytes: AtomicU64,
    current: Mutex<Option<String>>,
    errors: Mutex<Vec<(String, String)>>,
    errors_truncated: AtomicBool,
}

impl SyncProgress {
    pub(crate) fn set_current(&self, key: Option<String>) {
        if let Ok(mut guard) = self.current.lock() {
            *guard = key;
        }
    }

    /// Record a per-object failure. The run continues; only listing and
    /// connection failures are fatal.
    pub(crate) fn record_error(&self, key: &str, message: String) {
        self.failed.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut errors) = self.errors.lock() {
            if errors.len() < MAX_RECORDED_ERRORS {
                errors.push((key.to_owned(), message));
            } else {
                self.errors_truncated.store(true, Ordering::Relaxed);
            }
        }
    }

    fn errors_json(&self) -> Vec<serde_json::Value> {
        self.errors
            .lock()
            .map(|e| {
                e.iter()
                    .map(|(key, message)| serde_json::json!({ "key": key, "message": message }))
                    .collect()
            })
            .unwrap_or_default()
    }
}

#[derive(Debug)]
struct JobState {
    status: SyncStatus,
    finished_unix: Option<i64>,
    /// Set when the whole run aborted rather than individual objects failing.
    fatal: Option<String>,
}

/// One sync run.
#[derive(Debug)]
pub(crate) struct SyncJob {
    pub id: String,
    pub started_unix: i64,
    pub source: String,
    pub destination: String,
    pub progress: Arc<SyncProgress>,
    state: Mutex<JobState>,
    cancel: watch::Sender<bool>,
}

impl SyncJob {
    fn new(id: String, source: String, destination: String) -> Self {
        let (cancel, _) = watch::channel(false);
        Self {
            id,
            started_unix: now_unix(),
            source,
            destination,
            progress: Arc::new(SyncProgress::default()),
            state: Mutex::new(JobState { status: SyncStatus::Running, finished_unix: None, fatal: None }),
            cancel,
        }
    }

    pub(crate) fn status(&self) -> SyncStatus {
        self.state.lock().map(|s| s.status).unwrap_or(SyncStatus::Failed)
    }

    /// A receiver that flips to `true` when the run is asked to stop. Workers
    /// select on this so an in-flight streaming PUT is dropped mid-body; the
    /// partial temp file is then cleaned up by `FileWriter`'s `Drop`.
    pub(crate) fn cancel_signal(&self) -> watch::Receiver<bool> {
        self.cancel.subscribe()
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        *self.cancel.borrow()
    }

    pub(crate) fn request_cancel(&self) {
        // `send` fails when no receiver is alive, and workers only hold one for
        // the duration of a copy -- so a plain `send` would silently drop a
        // cancellation that arrived between two objects. `send_replace` always
        // updates the value, which is what `is_cancelled` reads.
        self.cancel.send_replace(true);
    }

    fn finish(&self, status: SyncStatus, fatal: Option<String>) {
        if let Ok(mut state) = self.state.lock() {
            state.status = status;
            state.finished_unix = Some(now_unix());
            state.fatal = fatal;
        }
        self.progress.set_current(None);
    }

    /// Render for the API. Contains no credentials by construction: only `id`,
    /// timestamps, counters, and the labels built by [`SyncRequest::source_label`].
    pub(crate) fn to_json(&self) -> serde_json::Value {
        let p = &self.progress;
        let (status, finished, fatal) = self
            .state
            .lock()
            .map(|s| (s.status, s.finished_unix, s.fatal.clone()))
            .unwrap_or((SyncStatus::Failed, None, None));
        serde_json::json!({
            "id": self.id,
            "status": status.as_str(),
            "started_unix": self.started_unix,
            "finished_unix": finished,
            "source": self.source,
            "destination": self.destination,
            "listed": p.listed.load(Ordering::Relaxed),
            "copied": p.copied.load(Ordering::Relaxed),
            "skipped": p.skipped.load(Ordering::Relaxed),
            "failed": p.failed.load(Ordering::Relaxed),
            "bytes": p.bytes.load(Ordering::Relaxed),
            "current_key": p.current.lock().ok().and_then(|c| c.clone()),
            "error": fatal,
            "errors": p.errors_json(),
            "errors_truncated": p.errors_truncated.load(Ordering::Relaxed),
        })
    }
}

/// Holds the one run that may be in flight, plus a short history of finished
/// ones.
///
/// Runs are refused rather than queued while one is active: two concurrent syncs
/// into the same destination would both see a key as missing and race to write
/// it.
#[derive(Debug, Default)]
pub(crate) struct SyncManager {
    active: Mutex<Option<Arc<SyncJob>>>,
    history: Mutex<VecDeque<serde_json::Value>>,
}

impl SyncManager {
    /// Claim the single run slot, or report the run already holding it.
    pub(crate) fn try_start(
        &self,
        source: String,
        destination: String,
    ) -> Result<Arc<SyncJob>, Arc<SyncJob>> {
        let mut active = self.active.lock().expect("sync registry poisoned");
        if let Some(existing) = active.as_ref()
            && existing.status() == SyncStatus::Running
        {
            return Err(Arc::clone(existing));
        }
        let job = Arc::new(SyncJob::new(new_job_id(), source, destination));
        *active = Some(Arc::clone(&job));
        Ok(job)
    }

    /// Mark a run terminal and push its final snapshot into the history.
    pub(crate) fn finish(&self, job: &Arc<SyncJob>, status: SyncStatus, fatal: Option<String>) {
        job.finish(status, fatal);
        if let Ok(mut history) = self.history.lock() {
            history.push_front(job.to_json());
            history.truncate(HISTORY_LIMIT);
        }
    }

    pub(crate) fn current(&self) -> Option<Arc<SyncJob>> {
        self.active.lock().ok().and_then(|a| a.clone())
    }

    pub(crate) fn history(&self) -> Vec<serde_json::Value> {
        self.history.lock().map(|h| h.iter().cloned().collect()).unwrap_or_default()
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

fn new_job_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body() -> SyncBody {
        serde_json::from_value(serde_json::json!({
            "endpoint": "https://minio.example.com:9000",
            "access_key": "AKIA_SOURCE",
            "secret_key": "s3cr3t-do-not-leak",
            "src_bucket": "photos",
            "dst_bucket": "photos",
        }))
        .expect("body should deserialize")
    }

    #[test]
    fn defaults_are_path_style_new_and_changed_and_bounded_concurrency() {
        let req = SyncRequest::from_body(body()).expect("valid");
        assert!(req.source.path_style);
        assert_eq!(req.mode, SyncMode::NewAndChanged);
        assert_eq!(req.concurrency, DEFAULT_CONCURRENCY);
        assert_eq!(req.source.region, DEFAULT_REGION);
        assert_eq!(req.dst_prefix, "");
    }

    #[test]
    fn a_bad_endpoint_is_refused_before_anything_else() {
        let mut b = body();
        b.endpoint = "file:///etc/passwd".to_owned();
        assert!(SyncRequest::from_body(b).is_err());
    }

    #[test]
    fn concurrency_outside_the_allowed_range_is_refused() {
        for bad in [0, MAX_CONCURRENCY + 1] {
            let mut b = body();
            b.concurrency = Some(bad);
            assert!(SyncRequest::from_body(b).is_err(), "{bad} should be refused");
        }
    }

    #[test]
    fn prefixes_are_normalised_to_a_single_trailing_slash() {
        assert_eq!(normalize_prefix(String::new()), "");
        assert_eq!(normalize_prefix("/a/b".to_owned()), "a/b/");
        assert_eq!(normalize_prefix("a/b/".to_owned()), "a/b/");
    }

    /// The job snapshot is echoed on every poll and kept in the history, so it
    /// must never carry the source credentials.
    #[test]
    fn a_rendered_job_never_contains_the_source_credentials() {
        let req = SyncRequest::from_body(body()).expect("valid");
        let manager = SyncManager::default();
        let job = manager
            .try_start(req.source_label(), req.destination_label())
            .expect("slot is free");
        job.progress.record_error("some/key", "denied".to_owned());
        manager.finish(&job, SyncStatus::Failed, Some("auth failed".to_owned()));

        let rendered = serde_json::to_string(&job.to_json()).expect("serializable");
        assert!(!rendered.contains("s3cr3t-do-not-leak"), "secret key leaked: {rendered}");
        assert!(!rendered.contains("AKIA_SOURCE"), "access key leaked: {rendered}");
        assert!(rendered.contains("minio.example.com"), "endpoint should be reported");

        let history = serde_json::to_string(&manager.history()).expect("serializable");
        assert!(!history.contains("s3cr3t-do-not-leak"), "secret key leaked into history");
    }

    #[test]
    fn a_second_run_is_refused_while_one_is_active() {
        let manager = SyncManager::default();
        let first = manager.try_start("a".to_owned(), "b".to_owned()).expect("slot is free");
        let refused = manager.try_start("c".to_owned(), "d".to_owned());
        assert!(refused.is_err(), "a second run must be refused");
        assert_eq!(refused.err().map(|j| j.id.clone()), Some(first.id.clone()));

        // Once the first is terminal the slot is free again.
        manager.finish(&first, SyncStatus::Completed, None);
        assert!(manager.try_start("c".to_owned(), "d".to_owned()).is_ok());
    }

    #[test]
    fn cancelling_flips_the_signal_workers_watch() {
        let manager = SyncManager::default();
        let job = manager.try_start("a".to_owned(), "b".to_owned()).expect("slot is free");
        let signal = job.cancel_signal();
        assert!(!*signal.borrow());
        job.request_cancel();
        assert!(*signal.borrow());
        assert!(job.is_cancelled());
    }

    #[test]
    fn recorded_errors_are_capped_and_flagged_as_truncated() {
        let progress = SyncProgress::default();
        for i in 0..(MAX_RECORDED_ERRORS + 25) {
            progress.record_error(&format!("k{i}"), "boom".to_owned());
        }
        assert_eq!(progress.errors_json().len(), MAX_RECORDED_ERRORS);
        assert!(progress.errors_truncated.load(Ordering::Relaxed));
        // Every failure still counts, even the ones not kept.
        assert_eq!(progress.failed.load(Ordering::Relaxed), (MAX_RECORDED_ERRORS + 25) as u64);
    }
}
