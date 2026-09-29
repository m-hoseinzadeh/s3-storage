//! Integration tests for "sync from remote".
//!
//! This server is itself S3-compatible, so the remote source is a *second*
//! in-process instance of it: the sync runs against a real SigV4-authenticated
//! S3 endpoint over loopback HTTP, with no MinIO, no Docker and no network. The
//! whole path is exercised for real -- AWS SDK -> SigV4 -> HTTP -> `s3s` ->
//! backend -> response parsing -> streaming body -> local `put_object` -- which
//! also makes this a second SDK-compatibility test beside `boto3_compat.rs`.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::net::TcpListener;
use tokio::sync::oneshot;

use s3_storage::{
    Config, SettingsStore, SettingsUpdate, build_admin_service, build_api_service, open_backend, serve,
};
use s3s::service::S3Service;

const SRC_KEY: &str = "src-access";
const SRC_SECRET: &str = "src-secret-never-echoed";
const DST_KEY: &str = "admin-key";
const DST_SECRET: &str = "admin-secret";
const JSON: (&str, &str) = ("Content-Type", "application/json");

// ---- harness ----

fn unique_dir(tag: &str) -> PathBuf {
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("s3-storage-sync-{tag}-{nanos}-{n}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn config(root: PathBuf, access: &str, secret: &str) -> Config {
    Config {
        root,
        host: "127.0.0.1".to_owned(),
        port: 0,
        public_port: 0,
        access_key: Some(access.to_owned()),
        secret_key: Some(secret.to_owned()),
        admin_enabled: true,
        admin_port: 0,
        trust_proxy: false,
        worker_threads: None,
        max_blocking_threads: None,
        listen_backlog: 1024,
    }
}

async fn serve_service(service: S3Service) -> (SocketAddr, oneshot::Sender<()>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
        let _ = serve(service, listener, async {
            let _ = rx.await;
        })
        .await;
    });
    (addr, tx)
}

/// A source instance (S3 API + admin, for seeding) and a destination instance
/// (admin only, which is what drives the sync).
struct Fixture {
    /// The source's S3 API port -- the "MinIO" endpoint the sync points at.
    src_api: SocketAddr,
    /// The source's admin port, used only to seed objects.
    src_admin: SocketAddr,
    /// The destination's admin port, which owns the sync endpoints.
    dst_admin: SocketAddr,
    src_cookie: String,
    dst_cookie: String,
    _shutdowns: Vec<oneshot::Sender<()>>,
}

async fn spawn() -> Fixture {
    let src_root = unique_dir("source");
    let dst_root = unique_dir("dst");
    let src_config = config(src_root.clone(), SRC_KEY, SRC_SECRET);
    let dst_config = config(dst_root.clone(), DST_KEY, DST_SECRET);

    let src_settings = SettingsStore::open(&src_root).unwrap();
    src_settings.update(&SettingsUpdate::default()).unwrap();
    let dst_settings = SettingsStore::open(&dst_root).unwrap();
    dst_settings.update(&SettingsUpdate::default()).unwrap();

    let (src_api, s1) = serve_service(build_api_service(
        &src_config,
        open_backend(&src_config).unwrap(),
        &src_settings,
    ))
    .await;
    let (src_admin, s2) = serve_service(build_admin_service(
        &src_config,
        open_backend(&src_config).unwrap(),
        &src_settings,
    ))
    .await;
    let (dst_admin, s3) = serve_service(build_admin_service(
        &dst_config,
        open_backend(&dst_config).unwrap(),
        &dst_settings,
    ))
    .await;

    let src_cookie = login(src_admin, SRC_KEY, SRC_SECRET);
    let dst_cookie = login(dst_admin, DST_KEY, DST_SECRET);

    Fixture { src_api, src_admin, dst_admin, src_cookie, dst_cookie, _shutdowns: vec![s1, s2, s3] }
}

impl Fixture {
    /// Create a bucket on the source and fill it with objects.
    fn seed_bucket(&self, bucket: &str) {
        let body = format!(r#"{{"name":"{bucket}"}}"#);
        let r = request(
            self.src_admin,
            "POST",
            "/api/buckets",
            &[JSON, ("Cookie", &self.src_cookie)],
            Some(body.as_bytes()),
        );
        assert_eq!(r.status, 200, "seed bucket: {}", r.text());
    }

    fn seed_object(&self, bucket: &str, key: &str, content: &[u8], content_type: &str) {
        let path = format!(
            "/api/object/put?bucket={bucket}&key={}&content_type={}",
            urlencode(key),
            urlencode(content_type)
        );
        let r = request(
            self.src_admin,
            "PUT",
            &path,
            &[("Cookie", &self.src_cookie)],
            Some(content),
        );
        assert_eq!(r.status, 200, "seed object {key}: {}", r.text());
    }

    fn make_dest_bucket(&self, bucket: &str) {
        let body = format!(r#"{{"name":"{bucket}"}}"#);
        let r = request(
            self.dst_admin,
            "POST",
            "/api/buckets",
            &[JSON, ("Cookie", &self.dst_cookie)],
            Some(body.as_bytes()),
        );
        assert_eq!(r.status, 200, "dest bucket: {}", r.text());
    }

    /// The JSON body for a sync request, with sensible defaults for the tests.
    fn sync_body(&self, src_bucket: &str, dst_bucket: &str, extra: &str) -> String {
        format!(
            r#"{{"endpoint":"http://{}","region":"us-east-1","access_key":"{SRC_KEY}",
                 "secret_key":"{SRC_SECRET}","path_style":true,
                 "src_bucket":"{src_bucket}","dst_bucket":"{dst_bucket}"{extra}}}"#,
            self.src_api
        )
    }

    fn post_sync(&self, path: &str, body: &str) -> Resp {
        request(
            self.dst_admin,
            "POST",
            path,
            &[JSON, ("Cookie", &self.dst_cookie)],
            Some(body.as_bytes()),
        )
    }

    fn get_dst(&self, path: &str) -> Resp {
        request(self.dst_admin, "GET", path, &[("Cookie", &self.dst_cookie)], None)
    }

    /// Start a run and wait for it to reach a terminal state.
    fn run_sync(&self, body: &str) -> serde_json::Value {
        let started = self.post_sync("/api/sync/runs", body);
        assert_eq!(started.status, 202, "start sync: {}", started.text());
        self.await_terminal()
    }

    fn await_terminal(&self) -> serde_json::Value {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            let resp = self.get_dst("/api/sync/runs/current");
            let value: serde_json::Value = serde_json::from_slice(&resp.body).expect("status json");
            let job = value.get("job").cloned().unwrap_or(serde_json::Value::Null);
            let status = job.get("status").and_then(|s| s.as_str()).unwrap_or("");
            if status != "running" && !status.is_empty() {
                return job;
            }
            assert!(std::time::Instant::now() < deadline, "sync did not finish in time: {job}");
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    fn head(&self, bucket: &str, key: &str) -> Resp {
        self.get_dst(&format!("/api/object/head?bucket={bucket}&key={}", urlencode(key)))
    }

    fn get_body(&self, bucket: &str, key: &str) -> Vec<u8> {
        let r = self.get_dst(&format!("/api/object/get?bucket={bucket}&key={}", urlencode(key)));
        assert_eq!(r.status, 200, "get {key}: {}", r.text());
        r.body
    }
}

fn n(job: &serde_json::Value, field: &str) -> u64 {
    job.get(field).and_then(serde_json::Value::as_u64).unwrap_or_else(|| panic!("missing {field} in {job}"))
}

fn login(addr: SocketAddr, access: &str, secret: &str) -> String {
    let body = format!(r#"{{"access_key":"{access}","secret_key":"{secret}"}}"#);
    let resp = request(addr, "POST", "/api/login", &[JSON], Some(body.as_bytes()));
    assert_eq!(resp.status, 200, "login: {}", resp.text());
    resp.cookie().expect("login must set a session cookie")
}

fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ---- minimal HTTP client (no dependency, as elsewhere in this suite) ----

struct Resp {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Resp {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
    fn cookie(&self) -> Option<String> {
        self.header("set-cookie").map(|c| c.split(';').next().unwrap_or("").to_owned())
    }
    fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap_or(serde_json::Value::Null)
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn request(addr: SocketAddr, method: &str, path: &str, extra: &[(&str, &str)], body: Option<&[u8]>) -> Resp {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream.set_read_timeout(Some(std::time::Duration::from_secs(30))).unwrap();

    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n");
    for (k, v) in extra {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    if let Some(b) = body {
        req.push_str(&format!("Content-Length: {}\r\n", b.len()));
    }
    req.push_str("\r\n");
    stream.write_all(req.as_bytes()).unwrap();
    if let Some(b) = body {
        stream.write_all(b).unwrap();
    }
    stream.flush().unwrap();

    let mut raw = Vec::new();
    let mut tmp = [0u8; 8192];
    let header_end = loop {
        if let Some(pos) = find(&raw, b"\r\n\r\n") {
            break pos;
        }
        let n = stream.read(&mut tmp).expect("read headers");
        assert!(n != 0, "connection closed before response headers");
        raw.extend_from_slice(&tmp[..n]);
    };

    let head = std::str::from_utf8(&raw[..header_end]).unwrap();
    let mut lines = head.lines();
    let status: u16 = lines.next().unwrap().split_whitespace().nth(1).unwrap().parse().unwrap();
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':').map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned())))
        .collect();

    let header = |name: &str| headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str());
    let content_length: Option<usize> = header("content-length").and_then(|v| v.parse().ok());
    let mut body_buf = raw[header_end + 4..].to_vec();

    let body = if matches!(status, 204 | 304) {
        Vec::new()
    } else if let Some(len) = content_length {
        while body_buf.len() < len {
            let mut t = [0u8; 8192];
            let n = stream.read(&mut t).expect("read body");
            if n == 0 {
                break;
            }
            body_buf.extend_from_slice(&t[..n]);
        }
        body_buf.truncate(len);
        body_buf
    } else {
        loop {
            let mut t = [0u8; 8192];
            let n = stream.read(&mut t).expect("read body");
            if n == 0 {
                break;
            }
            body_buf.extend_from_slice(&t[..n]);
        }
        body_buf
    };

    Resp { status, headers, body }
}

// ---- tests ----

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_first_run_copies_everything_and_a_second_copies_nothing() {
    let f = spawn().await;
    f.seed_bucket("photos");
    f.seed_object("photos", "a.txt", b"first", "text/plain");
    f.seed_object("photos", "nested/b.json", br#"{"x":1}"#, "application/json");
    f.seed_object("photos", "c.bin", &vec![7u8; 4096], "application/octet-stream");
    f.make_dest_bucket("photos");

    let job = f.run_sync(&f.sync_body("photos", "photos", ""));
    assert_eq!(job["status"], "completed", "{job}");
    assert_eq!(n(&job, "copied"), 3, "{job}");
    assert_eq!(n(&job, "skipped"), 0, "{job}");
    assert_eq!(n(&job, "failed"), 0, "{job}");
    assert_eq!(n(&job, "bytes"), 5 + 7 + 4096, "{job}");

    // Content and content type both survive the copy.
    assert_eq!(f.get_body("photos", "a.txt"), b"first");
    assert_eq!(f.get_body("photos", "nested/b.json"), br#"{"x":1}"#);
    assert_eq!(f.get_body("photos", "c.bin"), vec![7u8; 4096]);
    let head = f.head("photos", "nested/b.json").json();
    assert_eq!(head["content_type"], "application/json", "{head}");

    // The core incremental assertion: nothing changed, so nothing is copied.
    let again = f.run_sync(&f.sync_body("photos", "photos", ""));
    assert_eq!(again["status"], "completed", "{again}");
    assert_eq!(n(&again, "copied"), 0, "second run must copy nothing: {again}");
    assert_eq!(n(&again, "skipped"), 3, "{again}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn only_a_changed_object_is_recopied() {
    let f = spawn().await;
    f.seed_bucket("docs");
    f.seed_object("docs", "one", b"aaa", "text/plain");
    f.seed_object("docs", "two", b"bbb", "text/plain");
    f.seed_object("docs", "three", b"ccc", "text/plain");
    f.make_dest_bucket("docs");

    assert_eq!(n(&f.run_sync(&f.sync_body("docs", "docs", "")), "copied"), 3);

    // A different length is caught by the size arm of the rule.
    f.seed_object("docs", "two", b"bbbbbbbbbb", "text/plain");
    let job = f.run_sync(&f.sync_body("docs", "docs", ""));
    assert_eq!(n(&job, "copied"), 1, "only the changed object: {job}");
    assert_eq!(n(&job, "skipped"), 2, "{job}");
    assert_eq!(f.get_body("docs", "two"), b"bbbbbbbbbb");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_same_size_edit_with_a_newer_source_is_recopied() {
    let f = spawn().await;
    f.seed_bucket("docs");
    f.seed_object("docs", "same", b"aaaa", "text/plain");
    f.make_dest_bucket("docs");
    assert_eq!(n(&f.run_sync(&f.sync_body("docs", "docs", "")), "copied"), 1);

    // Same length, different content, and written after our copy: only the
    // timestamp arm of the rule can catch this.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    f.seed_object("docs", "same", b"zzzz", "text/plain");

    let job = f.run_sync(&f.sync_body("docs", "docs", ""));
    assert_eq!(n(&job, "copied"), 1, "a newer same-size source must be recopied: {job}");
    assert_eq!(f.get_body("docs", "same"), b"zzzz");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn verify_etag_catches_a_same_size_edit_that_time_says_is_synced() {
    let f = spawn().await;
    f.seed_bucket("ver");
    f.seed_object("ver", "k", b"aaaa", "text/plain");
    f.make_dest_bucket("ver");
    assert_eq!(n(&f.run_sync(&f.sync_body("ver", "ver", "")), "copied"), 1);

    // Same size, and a skew wide enough that size + time call it up to date.
    f.seed_object("ver", "k", b"zzzz", "text/plain");
    let by_time = f.post_sync("/api/sync/preview", &f.sync_body("ver", "ver", r#","skew_secs":3600"#)).json();
    assert_eq!(by_time["to_copy"], 0, "{by_time}");

    // The MD5 recorded when our copy was written tells them apart.
    let verify = r#","skew_secs":3600,"verify_etag":true"#;
    let preview = f.post_sync("/api/sync/preview", &f.sync_body("ver", "ver", verify)).json();
    assert_eq!(preview["actions"][0]["reason"], "etag_differs", "{preview}");
    let job = f.run_sync(&f.sync_body("ver", "ver", verify));
    assert_eq!(n(&job, "copied"), 1, "{job}");
    assert_eq!(f.get_body("ver", "k"), b"zzzz");

    let again = f.post_sync("/api/sync/preview", &f.sync_body("ver", "ver", verify)).json();
    assert_eq!(again["actions"][0]["reason"], "etag_matches", "{again}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn skip_existing_and_overwrite_all_bracket_the_default_mode() {
    let f = spawn().await;
    f.seed_bucket("modes");
    f.seed_object("modes", "k", b"original", "text/plain");
    f.make_dest_bucket("modes");
    assert_eq!(n(&f.run_sync(&f.sync_body("modes", "modes", "")), "copied"), 1);

    // Change the source, then refuse to touch what we already hold.
    f.seed_object("modes", "k", b"changed!", "text/plain");
    let skipped = f.run_sync(&f.sync_body("modes", "modes", r#","mode":"skip_existing""#));
    assert_eq!(n(&skipped, "copied"), 0, "{skipped}");
    assert_eq!(n(&skipped, "skipped"), 1, "{skipped}");

    // overwrite_all copies even when the rule would say "up to date".
    let forced = f.run_sync(&f.sync_body("modes", "modes", r#","mode":"overwrite_all""#));
    assert_eq!(n(&forced, "copied"), 1, "{forced}");
    assert_eq!(f.get_body("modes", "k"), b"changed!");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn prefixes_select_a_subtree_and_rebase_it() {
    let f = spawn().await;
    f.seed_bucket("tree");
    f.seed_object("tree", "keep/one.txt", b"1", "text/plain");
    f.seed_object("tree", "keep/deep/two.txt", b"2", "text/plain");
    f.seed_object("tree", "ignore/three.txt", b"3", "text/plain");
    f.make_dest_bucket("mirror");

    let job = f.run_sync(&f.sync_body("tree", "mirror", r#","src_prefix":"keep/","dst_prefix":"landed/""#));
    assert_eq!(n(&job, "copied"), 2, "only the selected subtree: {job}");
    assert_eq!(f.get_body("mirror", "landed/one.txt"), b"1");
    assert_eq!(f.get_body("mirror", "landed/deep/two.txt"), b"2");
    assert_eq!(f.head("mirror", "landed/three.txt").status, 404);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_missing_destination_bucket_is_refused_unless_creation_is_requested() {
    let f = spawn().await;
    f.seed_bucket("source");
    f.seed_object("source", "x", b"x", "text/plain");

    // No destination bucket, and no permission to make one.
    let job = f.run_sync(&f.sync_body("source", "absent", ""));
    assert_eq!(job["status"], "failed", "{job}");
    assert!(
        job["error"].as_str().unwrap_or_default().contains("does not exist"),
        "should name the missing bucket: {job}"
    );

    let created = f.run_sync(&f.sync_body("source", "absent", r#","create_bucket":true"#));
    assert_eq!(created["status"], "completed", "{created}");
    assert_eq!(n(&created, "copied"), 1, "{created}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bad_source_credentials_fail_the_run_without_echoing_the_secret() {
    let f = spawn().await;
    f.seed_bucket("beta");
    f.make_dest_bucket("beta");

    let body = format!(
        r#"{{"endpoint":"http://{}","access_key":"wrong","secret_key":"also-wrong",
             "src_bucket":"beta","dst_bucket":"beta"}}"#,
        f.src_api
    );
    let job = f.run_sync(&body);
    assert_eq!(job["status"], "failed", "{job}");

    let rendered = job.to_string();
    assert!(!rendered.contains("also-wrong"), "the secret key leaked: {rendered}");
    assert!(!rendered.contains(SRC_SECRET), "a credential leaked: {rendered}");

    // The history keeps the same redaction.
    let history = f.get_dst("/api/sync/runs").text();
    assert!(!history.contains("also-wrong"), "the secret key leaked into history");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_run_is_refused_while_one_is_in_flight() {
    let f = spawn().await;
    f.seed_bucket("many");
    for i in 0..150 {
        f.seed_object("many", &format!("obj-{i:04}"), &vec![b'x'; 2048], "text/plain");
    }
    f.make_dest_bucket("many");

    let first = f.post_sync("/api/sync/runs", &f.sync_body("many", "many", r#","concurrency":1"#));
    assert_eq!(first.status, 202, "{}", first.text());

    let second = f.post_sync("/api/sync/runs", &f.sync_body("many", "many", ""));
    assert_eq!(second.status, 409, "a concurrent run must be refused: {}", second.text());
    assert_eq!(second.json()["error"]["code"], "SyncInProgress");

    f.await_terminal();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cancelled_run_leaves_only_whole_objects_and_the_next_run_resumes() {
    let f = spawn().await;
    f.seed_bucket("bulk");
    const TOTAL: usize = 200;
    for i in 0..TOTAL {
        f.seed_object("bulk", &format!("o-{i:04}"), &vec![b'z'; 8192], "text/plain");
    }
    f.make_dest_bucket("bulk");

    let started = f.post_sync("/api/sync/runs", &f.sync_body("bulk", "bulk", r#","concurrency":1"#));
    assert_eq!(started.status, 202, "{}", started.text());
    let id = started.json()["job"]["id"].as_str().expect("job id").to_owned();

    // Cancel as soon as the run is demonstrably underway.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let job = f.get_dst("/api/sync/runs/current").json()["job"].clone();
        if job["copied"].as_u64().unwrap_or(0) > 0 || job["status"] != "running" {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "run never started copying");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let cancel = f.post_sync(&format!("/api/sync/runs/{id}/cancel"), "");
    assert_eq!(cancel.status, 200, "{}", cancel.text());

    let cancelled = f.await_terminal();
    assert_eq!(cancelled["status"], "cancelled", "{cancelled}");

    // Resumption is free: rerunning copies the remainder and skips what landed,
    // and every object is whole because writes are staged and renamed.
    let resumed = f.run_sync(&f.sync_body("bulk", "bulk", ""));
    assert_eq!(resumed["status"], "completed", "{resumed}");
    assert_eq!(
        n(&resumed, "copied") + n(&resumed, "skipped"),
        TOTAL as u64,
        "every object accounted for: {resumed}"
    );
    for i in 0..TOTAL {
        let key = format!("o-{i:04}");
        assert_eq!(f.get_body("bulk", &key).len(), 8192, "{key} must be whole");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn preview_reports_the_plan_without_copying_anything() {
    let f = spawn().await;
    f.seed_bucket("plan");
    f.seed_object("plan", "one", b"12345", "text/plain");
    f.seed_object("plan", "two", b"123", "text/plain");
    f.make_dest_bucket("plan");

    let preview = f.post_sync("/api/sync/preview", &f.sync_body("plan", "plan", "")).json();
    assert_eq!(preview["to_copy"], 2, "{preview}");
    assert_eq!(preview["to_skip"], 0, "{preview}");
    assert_eq!(preview["bytes_to_copy"], 8, "{preview}");
    assert_eq!(preview["actions"][0]["reason"], "missing", "{preview}");
    // Nothing was actually written.
    assert_eq!(f.head("plan", "one").status, 404, "preview must not copy");

    f.run_sync(&f.sync_body("plan", "plan", ""));
    let after = f.post_sync("/api/sync/preview", &f.sync_body("plan", "plan", "")).json();
    assert_eq!(after["to_copy"], 0, "{after}");
    assert_eq!(after["to_skip"], 2, "{after}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_large_object_streams_through_intact() {
    let f = spawn().await;
    f.seed_bucket("big");
    // Comfortably larger than any internal buffer, with a non-repeating pattern
    // so a truncated or mis-ordered stream cannot pass.
    let payload: Vec<u8> = (0..(12 * 1024 * 1024u32)).map(|i| (i % 251) as u8).collect();
    f.seed_object("big", "large.bin", &payload, "application/octet-stream");
    f.make_dest_bucket("big");

    let job = f.run_sync(&f.sync_body("big", "big", ""));
    assert_eq!(job["status"], "completed", "{job}");
    assert_eq!(n(&job, "bytes"), payload.len() as u64, "{job}");
    assert_eq!(f.get_body("big", "large.bin"), payload, "the object must round-trip byte for byte");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unreachable_or_malformed_endpoint_is_rejected_up_front() {
    let f = spawn().await;
    f.make_dest_bucket("beta");

    // A non-HTTP scheme never gets as far as a connection.
    let bad = f.post_sync(
        "/api/sync/runs",
        r#"{"endpoint":"file:///etc/passwd","access_key":"a","secret_key":"beta",
            "src_bucket":"beta","dst_bucket":"beta"}"#,
    );
    assert_eq!(bad.status, 400, "{}", bad.text());

    // A destination bucket name that would address the settings directory.
    let sneaky = f.post_sync("/api/sync/runs", &f.sync_body("beta", ".s3-storage", ""));
    assert_eq!(sneaky.status, 400, "{}", sneaky.text());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_sync_endpoints_require_a_session() {
    let f = spawn().await;
    for (method, path) in [
        ("POST", "/api/sync/runs"),
        ("POST", "/api/sync/preview"),
        ("GET", "/api/sync/runs"),
        ("GET", "/api/sync/runs/current"),
    ] {
        let r = request(f.dst_admin, method, path, &[JSON], Some(b"{}"));
        assert_eq!(r.status, 401, "{method} {path} must require a session: {}", r.text());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_object_cap_bounds_both_the_preview_and_the_run() {
    let f = spawn().await;
    f.seed_bucket("capped");
    for i in 0..10 {
        f.seed_object("capped", &format!("k-{i:02}"), b"xy", "text/plain");
    }
    f.make_dest_bucket("capped");

    // The cap must stop the paging loop, not merely the current page.
    let preview = f
        .post_sync("/api/sync/preview", &f.sync_body("capped", "capped", r#","max_objects":4"#))
        .json();
    assert_eq!(preview["listed"], 4, "preview must stop at the cap: {preview}");

    let job = f.run_sync(&f.sync_body("capped", "capped", r#","max_objects":4"#));
    assert_eq!(job["status"], "completed", "{job}");
    assert_eq!(n(&job, "listed"), 4, "the run must stop at the cap: {job}");
    assert_eq!(n(&job, "copied"), 4, "{job}");

    // Uncapped, the rest follows on a later run.
    let rest = f.run_sync(&f.sync_body("capped", "capped", ""));
    assert_eq!(n(&rest, "copied") + n(&rest, "skipped"), 10, "{rest}");
}

/// Sync from a real MinIO, when one is pointed at by the environment.
///
/// The rest of this file uses a second instance of this server as the source,
/// which proves the protocol path but not that a genuine MinIO agrees with it.
/// Set `MINIO_ENDPOINT`, `MINIO_ACCESS_KEY`, `MINIO_SECRET_KEY` and
/// `MINIO_BUCKET` to run this against the real thing; otherwise it skips, the
/// same way `boto3_compat.rs` skips when boto3 is not installed.
///
/// Read-only with respect to the source: it lists and gets, never writes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn syncs_from_a_real_minio_when_one_is_configured() {
    let (Ok(endpoint), Ok(access), Ok(secret), Ok(bucket)) = (
        std::env::var("MINIO_ENDPOINT"),
        std::env::var("MINIO_ACCESS_KEY"),
        std::env::var("MINIO_SECRET_KEY"),
        std::env::var("MINIO_BUCKET"),
    ) else {
        eprintln!("note: MINIO_* not set - skipping the real-MinIO sync test");
        return;
    };

    let f = spawn().await;
    f.make_dest_bucket("from-minio");

    let region = std::env::var("MINIO_REGION").unwrap_or_else(|_| "us-east-1".to_owned());
    let body = format!(
        r#"{{"endpoint":"{endpoint}","region":"{region}","access_key":"{access}",
             "secret_key":"{secret}","path_style":true,
             "src_bucket":"{bucket}","dst_bucket":"from-minio"}}"#
    );

    // Plan first, so the byte total the source *reports* in its listing can be
    // cross-checked against the bytes actually streamed to disk.
    let plan = f.post_sync("/api/sync/preview", &body).json();
    assert_eq!(plan["to_skip"], 0, "a fresh destination should plan to copy everything: {plan}");

    let job = f.run_sync(&body);
    eprintln!(
        "real MinIO: listed={} copied={} skipped={} failed={} bytes={}",
        n(&job, "listed"),
        n(&job, "copied"),
        n(&job, "skipped"),
        n(&job, "failed"),
        n(&job, "bytes"),
    );
    assert_eq!(job["status"], "completed", "sync from real MinIO failed: {job}");
    assert_eq!(n(&job, "failed"), 0, "{job}");
    // Guard against the whole test passing vacuously on an empty bucket.
    assert!(n(&job, "listed") > 0, "MINIO_BUCKET is empty; point it at a bucket with objects");
    assert_eq!(n(&job, "copied"), n(&job, "listed"), "a fresh destination must copy everything: {job}");
    assert_eq!(
        serde_json::Value::from(n(&job, "bytes")),
        plan["bytes_to_copy"],
        "bytes written must match the sizes MinIO reported in its listing: {job} vs {plan}"
    );
    assert_eq!(
        n(&job, "copied") + n(&job, "skipped"),
        n(&job, "listed"),
        "every listed object must be accounted for: {job}"
    );

    // A second run over an unchanged real bucket must copy nothing -- the
    // incremental rule holding against MinIO's own ETags and timestamps, which
    // is the part the in-process source cannot prove.
    let again = f.run_sync(&body);
    assert_eq!(again["status"], "completed", "{again}");
    assert_eq!(n(&again, "copied"), 0, "a re-run against real MinIO must copy nothing: {again}");
    assert_eq!(n(&again, "skipped"), n(&job, "listed"), "{again}");
}
