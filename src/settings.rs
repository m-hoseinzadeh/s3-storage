//! Persisted, runtime-editable settings store, backed by SQLite.
//!
//! These settings (public buckets, virtual-host domains, custom-domain mappings,
//! the public API URL and the admin session TTL) used to be read once from
//! environment variables / CLI flags and baked into immutable structs at startup.
//! They now live in a SQLite database under the data root so the admin panel can
//! edit them live — the env/CLI flags for them have been removed entirely.
//!
//! ## Hot path
//! The per-request lookups (access checks, host parsing) read only the in-memory
//! [`Snapshot`] behind an [`RwLock`]; the database is touched solely on startup
//! ([`SettingsStore::open`]) and on rare admin edits ([`SettingsStore::update`]).
//! No lock guard is ever held across an `.await`.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex, RwLock};

use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

/// Default admin session lifetime (seconds) when none is stored.
const DEFAULT_SESSION_TTL_SECS: u64 = 3600;

/// Current schema version; bump when adding migrations keyed off `user_version`.
const SCHEMA_VERSION: i64 = 1;

/// The settings the admin panel can edit at runtime.
///
/// `Serialize` is used to render the `/api/config` response, not for storage —
/// persistence is row-based (see the schema in [`init_schema`]).
#[derive(Debug, Clone, Serialize)]
pub struct RuntimeSettings {
    /// Buckets that permit anonymous reads (served on the public port).
    pub public_buckets: Vec<String>,
    /// Base domains enabling `<bucket>.<domain>` virtual-hosted access.
    pub domains: Vec<String>,
    /// Custom-domain mappings as `host=bucket` entries.
    pub domain_map: Vec<String>,
    /// Origins (`scheme://host[:port]`, or `*` for any) allowed to read from the
    /// public endpoint cross-origin. Drives the `Access-Control-Allow-Origin` header
    /// so browsers accept fonts and other CORS-gated subresources served publicly.
    pub allowed_origins: Vec<String>,
    /// Public base URL of the S3 API, used when minting presigned links.
    /// Normalized so a blank value is `None`.
    pub api_public_url: Option<String>,
    /// Admin session lifetime in seconds.
    pub admin_session_ttl_secs: u64,
}

impl Default for RuntimeSettings {
    fn default() -> Self {
        Self {
            public_buckets: Vec::new(),
            domains: Vec::new(),
            domain_map: Vec::new(),
            allowed_origins: Vec::new(),
            api_public_url: None,
            admin_session_ttl_secs: DEFAULT_SESSION_TTL_SECS,
        }
    }
}

/// A partial update from the admin API. A `None` field is left unchanged. For
/// `api_public_url`, a blank string clears the stored value.
#[derive(Debug, Default, Deserialize)]
pub struct SettingsUpdate {
    #[serde(default)]
    pub public_buckets: Option<Vec<String>>,
    #[serde(default)]
    pub domains: Option<Vec<String>>,
    #[serde(default)]
    pub domain_map: Option<Vec<String>>,
    #[serde(default)]
    pub allowed_origins: Option<Vec<String>>,
    #[serde(default)]
    pub api_public_url: Option<String>,
    #[serde(default)]
    pub admin_session_ttl_secs: Option<u64>,
}

impl SettingsUpdate {
    /// Validate the requested changes, returning a human-readable error message
    /// suitable for a `400 Bad Request`.
    pub fn validate(&self) -> Result<(), String> {
        if let Some(entries) = &self.domain_map {
            for entry in entries {
                let (host, bucket) = entry
                    .split_once('=')
                    .ok_or_else(|| format!("domain map entry `{entry}` must be in `host=bucket` form"))?;
                if host.trim().is_empty() || bucket.trim().is_empty() {
                    return Err(format!("domain map entry `{entry}` has an empty host or bucket"));
                }
            }
        }
        if let Some(buckets) = &self.public_buckets
            && buckets.iter().any(|b| b.trim().is_empty())
        {
            return Err("public bucket names must not be empty".to_owned());
        }
        if let Some(domains) = &self.domains
            && domains.iter().any(|d| d.trim().is_empty())
        {
            return Err("domains must not be empty".to_owned());
        }
        if let Some(origins) = &self.allowed_origins
            && origins.iter().any(|o| o.trim().is_empty())
        {
            return Err("allowed origins must not be empty".to_owned());
        }
        if self.admin_session_ttl_secs == Some(0) {
            return Err("admin_session_ttl_secs must be greater than 0".to_owned());
        }
        Ok(())
    }
}

/// Derived, read-optimized view rebuilt only when settings change, so per-request
/// reads never parse strings or allocate maps.
#[derive(Debug)]
struct Snapshot {
    settings: RuntimeSettings,
    public_set: HashSet<String>,
    domain_map_parsed: HashMap<String, String>,
    base_domains_lc: Vec<String>,
    /// Normalized allowed-origin set (wildcard excluded; see `allow_any_origin`).
    allowed_origins: HashSet<String>,
    /// True when `*` is configured: any origin is allowed.
    allow_any_origin: bool,
}

impl Snapshot {
    fn build(settings: RuntimeSettings) -> Self {
        let public_set = settings.public_buckets.iter().cloned().collect();
        let domain_map_parsed = parse_domain_map(&settings.domain_map);
        let base_domains_lc = settings.domains.iter().map(|d| d.to_ascii_lowercase()).collect();
        let normalized = normalize_origins(&settings.allowed_origins);
        let allow_any_origin = normalized.iter().any(|o| o == "*");
        let allowed_origins = normalized.into_iter().filter(|o| o != "*").collect();
        Self {
            settings,
            public_set,
            domain_map_parsed,
            base_domains_lc,
            allowed_origins,
            allow_any_origin,
        }
    }
}

/// The settings store: a SQLite connection for load/update plus an in-memory
/// snapshot for the hot path.
#[derive(Debug)]
pub struct SettingsStore {
    /// Serializes the rare load/update operations. `Connection` is `Send` but not
    /// `Sync`, so it lives behind a `Mutex`; it is never touched on the hot path.
    conn: Mutex<Connection>,
    /// Hot-path reads. Republished wholesale on every successful update.
    snapshot: RwLock<Snapshot>,
}

/// Shared handle threaded into every service.
pub type SharedSettings = Arc<SettingsStore>;

impl SettingsStore {
    /// Open (creating if needed) the settings DB at `{root}/.s3-storage/settings.db`,
    /// ensure the schema exists, and load the current values into memory.
    pub fn open(root: &Path) -> io::Result<SharedSettings> {
        let dir = root.join(".s3-storage");
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("settings.db");
        let conn = Connection::open(&path).map_err(io::Error::other)?;
        init_schema(&conn).map_err(io::Error::other)?;
        let settings = load_settings(&conn).map_err(io::Error::other)?;
        Ok(Arc::new(Self {
            conn: Mutex::new(conn),
            snapshot: RwLock::new(Snapshot::build(settings)),
        }))
    }

    /// Whether `bucket` permits anonymous reads.
    #[must_use]
    pub fn is_public(&self, bucket: &str) -> bool {
        self.snapshot.read().unwrap().public_set.contains(bucket)
    }

    /// Resolve the `Access-Control-Allow-Origin` value to return for a request
    /// carrying the given `Origin`, or `None` if the allow-list does not permit it.
    /// Returns `"*"` when a wildcard is configured, otherwise the (normalized)
    /// matching origin so it can be echoed back.
    #[must_use]
    pub fn cors_allow_origin(&self, origin: &str) -> Option<String> {
        let snap = self.snapshot.read().unwrap();
        if snap.allow_any_origin {
            return Some("*".to_owned());
        }
        let origin = origin.trim().trim_end_matches('/');
        if snap.allowed_origins.contains(origin) {
            return Some(origin.to_owned());
        }
        None
    }

    /// Resolve a host (without port) to a bucket via custom-domain mapping or
    /// `<bucket>.<base-domain>` virtual-hosting. `None` means path-style.
    #[must_use]
    pub fn resolve_host(&self, host_only: &str) -> Option<String> {
        let snap = self.snapshot.read().unwrap();
        // 1. Explicit custom-domain mapping.
        if let Some(bucket) = snap.domain_map_parsed.get(host_only) {
            return Some(bucket.clone());
        }
        // 2. `<bucket>.<base>` virtual-hosted style.
        for base in &snap.base_domains_lc {
            if host_only == base {
                // The base domain itself addresses no bucket (path-style).
                return None;
            }
            if let Some(prefix) = host_only.strip_suffix(base.as_str()).and_then(|h| h.strip_suffix('.'))
                && !prefix.is_empty()
            {
                return Some(prefix.to_owned());
            }
        }
        // 3. Default: path-style.
        None
    }

    /// The normalized public API URL, if configured.
    #[must_use]
    pub fn api_public_url(&self) -> Option<String> {
        self.snapshot.read().unwrap().settings.api_public_url.clone()
    }

    /// The current admin session lifetime in seconds.
    #[must_use]
    pub fn session_ttl_secs(&self) -> u64 {
        self.snapshot.read().unwrap().settings.admin_session_ttl_secs
    }

    /// A clone of the current settings, for the `/api/config` response.
    #[must_use]
    pub fn snapshot(&self) -> RuntimeSettings {
        self.snapshot.read().unwrap().settings.clone()
    }

    /// Apply an update in a single transaction, then republish the in-memory
    /// snapshot from what was actually committed (the DB stays authoritative).
    ///
    /// This performs blocking SQLite I/O; callers on an async runtime should run
    /// it via `spawn_blocking`. The connection lock is released before the
    /// snapshot is published, and neither lock is held across an `.await`.
    pub fn update(&self, upd: &SettingsUpdate) -> rusqlite::Result<()> {
        let settings = {
            let mut conn = self.conn.lock().unwrap();
            let tx = conn.transaction()?;
            apply(&tx, upd)?;
            tx.commit()?;
            load_settings(&conn)?
        };
        *self.snapshot.write().unwrap() = Snapshot::build(settings);
        Ok(())
    }
}

fn init_schema(conn: &Connection) -> rusqlite::Result<()> {
    // WAL improves durability/concurrency; the PRAGMA returns the resulting mode,
    // so read it back via query_row rather than pragma_update.
    let _: String = conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS public_buckets  (bucket TEXT PRIMARY KEY);
         CREATE TABLE IF NOT EXISTS domains         (domain TEXT PRIMARY KEY);
         CREATE TABLE IF NOT EXISTS domain_map      (host TEXT PRIMARY KEY, bucket TEXT NOT NULL);
         CREATE TABLE IF NOT EXISTS allowed_origins (origin TEXT PRIMARY KEY);
         CREATE TABLE IF NOT EXISTS settings_kv     (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
    )?;
    // Future migrations branch on the stored `user_version` before bumping it.
    conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    Ok(())
}

fn load_settings(conn: &Connection) -> rusqlite::Result<RuntimeSettings> {
    let public_buckets = load_column(conn, "SELECT bucket FROM public_buckets ORDER BY bucket")?;
    let domains = load_column(conn, "SELECT domain FROM domains ORDER BY domain")?;
    let allowed_origins = load_column(conn, "SELECT origin FROM allowed_origins ORDER BY origin")?;
    let domain_map = {
        let mut stmt = conn.prepare("SELECT host, bucket FROM domain_map ORDER BY host")?;
        let rows = stmt.query_map([], |r| {
            Ok(format!("{}={}", r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let api_public_url = load_kv(conn, "api_public_url")?
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty());
    let admin_session_ttl_secs = load_kv(conn, "admin_session_ttl_secs")?
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_SESSION_TTL_SECS);
    Ok(RuntimeSettings {
        public_buckets,
        domains,
        domain_map,
        allowed_origins,
        api_public_url,
        admin_session_ttl_secs,
    })
}

fn apply(tx: &rusqlite::Transaction<'_>, upd: &SettingsUpdate) -> rusqlite::Result<()> {
    if let Some(buckets) = &upd.public_buckets {
        tx.execute("DELETE FROM public_buckets", [])?;
        for b in normalize_list(buckets) {
            tx.execute("INSERT OR IGNORE INTO public_buckets (bucket) VALUES (?1)", [&b])?;
        }
    }
    if let Some(domains) = &upd.domains {
        tx.execute("DELETE FROM domains", [])?;
        for d in normalize_list(domains) {
            tx.execute("INSERT OR IGNORE INTO domains (domain) VALUES (?1)", [&d])?;
        }
    }
    if let Some(origins) = &upd.allowed_origins {
        tx.execute("DELETE FROM allowed_origins", [])?;
        for o in normalize_origins(origins) {
            tx.execute("INSERT OR IGNORE INTO allowed_origins (origin) VALUES (?1)", [&o])?;
        }
    }
    if let Some(entries) = &upd.domain_map {
        tx.execute("DELETE FROM domain_map", [])?;
        for (host, bucket) in normalize_domain_map(entries) {
            tx.execute(
                "INSERT OR REPLACE INTO domain_map (host, bucket) VALUES (?1, ?2)",
                [&host, &bucket],
            )?;
        }
    }
    if let Some(url) = &upd.api_public_url {
        let v = url.trim();
        if v.is_empty() {
            tx.execute("DELETE FROM settings_kv WHERE key = 'api_public_url'", [])?;
        } else {
            set_kv(tx, "api_public_url", v)?;
        }
    }
    if let Some(ttl) = upd.admin_session_ttl_secs {
        set_kv(tx, "admin_session_ttl_secs", &ttl.to_string())?;
    }
    Ok(())
}

fn set_kv(tx: &rusqlite::Transaction<'_>, key: &str, value: &str) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO settings_kv (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        [key, value],
    )?;
    Ok(())
}

fn load_column(conn: &Connection, sql: &str) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    rows.collect()
}

fn load_kv(conn: &Connection, key: &str) -> rusqlite::Result<Option<String>> {
    conn.query_row("SELECT value FROM settings_kv WHERE key = ?1", [key], |r| r.get::<_, String>(0))
        .optional()
}

/// Trim, drop empties, and dedup while preserving order.
fn normalize_list(items: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    items
        .iter()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .filter(|s| seen.insert(s.clone()))
        .collect()
}

/// Trim, strip any trailing `/`, drop empties, and dedup origins while preserving
/// order. Keeping a canonical form (no trailing slash) lets request `Origin` headers
/// — which never carry a trailing slash — match stored entries reliably.
fn normalize_origins(items: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    items
        .iter()
        .map(|s| s.trim().trim_end_matches('/').to_owned())
        .filter(|s| !s.is_empty())
        .filter(|s| seen.insert(s.clone()))
        .collect()
}

/// Parse `host=bucket` entries into `(lowercased host, bucket)` pairs, skipping
/// invalid ones. Shared by snapshot building and update normalization.
fn parse_domain_map(entries: &[String]) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for (host, bucket) in normalize_domain_map(entries) {
        map.insert(host, bucket);
    }
    map
}

fn normalize_domain_map(entries: &[String]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for entry in entries {
        if let Some((host, bucket)) = entry.split_once('=') {
            let host = host.trim().to_ascii_lowercase();
            let bucket = bucket.trim().to_owned();
            if !host.is_empty() && !bucket.is_empty() {
                out.push((host, bucket));
            }
        }
    }
    out
}
