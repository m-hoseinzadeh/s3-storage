//! Integration tests for the embedded admin panel, driving its JSON API over raw
//! HTTP (session-cookie auth, bucket/object lifecycle, presigned-URL round-trip).

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::net::TcpListener;
use tokio::sync::oneshot;

use s3_storage::{
    Config, SettingsStore, SettingsUpdate, SharedSettings, build_admin_service, build_api_service,
    build_public_service, open_backend, serve,
};
use s3s::service::S3Service;

struct TestServer {
    addr: SocketAddr,
    _shutdown: oneshot::Sender<()>,
}

/// An admin service paired with a strict API service over the same data root, used
/// to verify presigned links (signed for the API host) against the API port.
struct AdminWithApi {
    admin: SocketAddr,
    api: SocketAddr,
    _admin_shutdown: oneshot::Sender<()>,
    _api_shutdown: oneshot::Sender<()>,
}

/// An admin service paired with a public read-only service sharing one settings
/// store, to verify that toggling a bucket public via the admin API serves it on
/// the public port live (no restart).
struct AdminWithPublic {
    admin: SocketAddr,
    public: SocketAddr,
    root: PathBuf,
    _admin_shutdown: oneshot::Sender<()>,
    _public_shutdown: oneshot::Sender<()>,
}

fn unique_dir() -> PathBuf {
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("s3-storage-admin-{nanos}-{n}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn admin_config(root: PathBuf) -> Config {
    admin_config_with(root, false)
}

/// `trust_proxy` decides whether `X-Forwarded-Proto` may be believed.
fn admin_config_with(root: PathBuf, trust_proxy: bool) -> Config {
    Config {
        root,
        host: "127.0.0.1".to_owned(),
        port: 0,
        public_port: 0,
        access_key: Some("admin-key".to_owned()),
        secret_key: Some("admin-secret".to_owned()),
        admin_enabled: true,
        admin_port: 0,
        trust_proxy,
        worker_threads: None,
        max_blocking_threads: None,
        listen_backlog: 1024,
    }
}

/// Open a settings store under `root` and seed the runtime-editable values that
/// used to come from CLI flags.
fn seed_settings(root: &Path, public_buckets: Vec<String>, api_public_url: Option<String>) -> SharedSettings {
    let settings = SettingsStore::open(root).unwrap();
    settings
        .update(&SettingsUpdate { public_buckets: Some(public_buckets), api_public_url, ..Default::default() })
        .unwrap();
    settings
}

/// Bind a listener and serve `service`, returning its live address + shutdown.
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

async fn spawn() -> TestServer {
    let root = unique_dir();
    let config = admin_config(root.clone());
    // The built service holds its own `Arc` clone of the store, so the local handle
    // can drop; "assets" is seeded public for the lifecycle assertions.
    let settings = seed_settings(&root, vec!["assets".to_owned()], None);
    let service = build_admin_service(&config, open_backend(&config).unwrap(), &settings);
    let (addr, tx) = serve_service(service).await;
    TestServer { addr, _shutdown: tx }
}

/// Spawn an API service first (to learn its address), then an admin service over
/// the same data root + shared settings, with `api_public_url` pointed at the API,
/// so presigned links the panel mints can be verified against the API port.
async fn spawn_with_api() -> AdminWithApi {
    let root = unique_dir();
    let config = admin_config(root.clone());
    let settings = seed_settings(&root, vec!["assets".to_owned()], None);

    let api_service = build_api_service(&config, open_backend(&config).unwrap(), &settings);
    let (api, api_shutdown) = serve_service(api_service).await;

    // Point presigning at the now-known API address (a live settings edit).
    settings
        .update(&SettingsUpdate { api_public_url: Some(format!("http://{api}")), ..Default::default() })
        .unwrap();

    let admin_service = build_admin_service(&config, open_backend(&config).unwrap(), &settings);
    let (admin, admin_shutdown) = serve_service(admin_service).await;

    AdminWithApi { admin, api, _admin_shutdown: admin_shutdown, _api_shutdown: api_shutdown }
}

/// Spawn an admin service and a public read-only service sharing one settings store.
async fn spawn_with_public() -> AdminWithPublic {
    let root = unique_dir();
    let config = admin_config(root.clone());
    // Nothing public to start with.
    let settings = seed_settings(&root, vec![], None);

    let public_service = build_public_service(&config, open_backend(&config).unwrap(), &settings);
    let (public, public_shutdown) = serve_service(public_service).await;

    let admin_service = build_admin_service(&config, open_backend(&config).unwrap(), &settings);
    let (admin, admin_shutdown) = serve_service(admin_service).await;

    AdminWithPublic { admin, public, root, _admin_shutdown: admin_shutdown, _public_shutdown: public_shutdown }
}

/// Log in with the test credentials and return the session cookie.
fn login(addr: SocketAddr) -> String {
    let resp = request(
        addr,
        "POST",
        "/api/login",
        &[JSON],
        Some(br#"{"access_key":"admin-key","secret_key":"admin-secret"}"#),
    );
    resp.cookie().expect("login must set a session cookie")
}

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
    /// Extract the session cookie (`name=value`) from a `Set-Cookie` header.
    fn cookie(&self) -> Option<String> {
        self.header("set-cookie").map(|c| c.split(';').next().unwrap_or("").to_owned())
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn request(addr: SocketAddr, method: &str, path: &str, extra: &[(&str, &str)], body: Option<&[u8]>) -> Resp {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream.set_read_timeout(Some(std::time::Duration::from_secs(10))).unwrap();

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

    let header = |name: &str| {
        headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    };
    let content_length: Option<usize> = header("content-length").and_then(|v| v.parse().ok());
    let mut body_buf = raw[header_end + 4..].to_vec();
    let bodyless = method.eq_ignore_ascii_case("HEAD") || matches!(status, 204 | 304);

    let body = if bodyless {
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
        // read to EOF
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

const JSON: (&str, &str) = ("Content-Type", "application/json");

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_login_and_session() {
    let srv = spawn().await;
    let a = srv.addr;

    // The SPA shell is served at the root (and as a fallback for client routes).
    //
    // A fresh clone commits only an empty `admin-ui/dist/` (the `.gitkeep` that lets
    // rust-embed compile), so until `npm --prefix admin-ui run build` has run there
    // is no shell to serve and the panel says so. Skip those assertions in that
    // case rather than failing the whole login test, the same way the boto3 test
    // skips when boto3 is not installed.
    let index = request(a, "GET", "/", &[], None);
    if index.status == 404 && index.text().contains("Admin UI is not built") {
        eprintln!("note: admin UI not built - skipping SPA shell assertions");
    } else {
        assert_eq!(index.status, 200);
        assert!(index.header("content-type").is_some_and(|c| c.contains("text/html")));
        assert!(index.text().contains("<div id=\"root\">"));
        let spa_fallback = request(a, "GET", "/buckets", &[], None);
        assert_eq!(spa_fallback.status, 200, "client-side routes fall back to index.html");
    }

    // Wrong credentials are rejected.
    let bad = request(a, "POST", "/api/login", &[JSON], Some(br#"{"access_key":"x","secret_key":"y"}"#));
    assert_eq!(bad.status, 401);

    // API without a session cookie is rejected.
    assert_eq!(request(a, "GET", "/api/buckets", &[], None).status, 401);

    // Correct credentials issue a session cookie.
    let ok = request(
        a,
        "POST",
        "/api/login",
        &[JSON],
        Some(br#"{"access_key":"admin-key","secret_key":"admin-secret"}"#),
    );
    assert_eq!(ok.status, 200);
    let cookie = ok.cookie().expect("login must set a session cookie");
    assert!(cookie.starts_with("s3admin_session="));
    // Over plain HTTP the cookie must NOT be `Secure`, otherwise the browser
    // drops it and the next request is unauthenticated ("login required").
    let set_cookie = ok.header("set-cookie").unwrap();
    assert!(!set_cookie.contains("Secure"), "plain-HTTP login cookie must not be Secure: {set_cookie}");
    assert!(set_cookie.contains("HttpOnly") && set_cookie.contains("SameSite=Strict"));

    // `X-Forwarded-Proto` is just a request header, so without `--trust-proxy` it
    // must not let a client decide whether its own cookie is protected.
    let spoofed = request(
        a,
        "POST",
        "/api/login",
        &[JSON, ("X-Forwarded-Proto", "https")],
        Some(br#"{"access_key":"admin-key","secret_key":"admin-secret"}"#),
    );
    assert!(
        !spoofed.header("set-cookie").unwrap().contains("Secure"),
        "X-Forwarded-Proto must be ignored unless a proxy is trusted"
    );

    // The cookie authorizes API calls.
    let session = request(a, "GET", "/api/session", &[("Cookie", &cookie)], None);
    assert_eq!(session.status, 200);
    assert!(session.text().contains("\"authenticated\":true"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_object_lifecycle() {
    let srv = spawn().await;
    let a = srv.addr;
    let login = request(
        a,
        "POST",
        "/api/login",
        &[JSON],
        Some(br#"{"access_key":"admin-key","secret_key":"admin-secret"}"#),
    );
    let cookie = login.cookie().unwrap();
    let auth = [("Cookie", cookie.as_str())];
    let auth_json = [("Cookie", cookie.as_str()), JSON];

    // Create buckets.
    assert_eq!(request(a, "POST", "/api/buckets", &auth_json, Some(br#"{"name":"docs"}"#)).status, 200);
    assert_eq!(request(a, "POST", "/api/buckets", &auth_json, Some(br#"{"name":"assets"}"#)).status, 200);

    let buckets = request(a, "GET", "/api/buckets", &auth, None);
    assert_eq!(buckets.status, 200);
    let listing = buckets.text();
    assert!(listing.contains("\"name\":\"docs\""));
    // `assets` is configured public.
    assert!(listing.contains("\"name\":\"assets\""));
    assert!(listing.contains("\"public\":true"));

    // Upload.
    let put = request(
        a,
        "PUT",
        "/api/object/put?bucket=docs&key=hello.txt&content_type=text/plain",
        &auth,
        Some(b"hello admin world"),
    );
    assert_eq!(put.status, 200);

    // Download round-trip.
    let got = request(a, "GET", "/api/object/get?bucket=docs&key=hello.txt", &auth, None);
    assert_eq!(got.status, 200);
    assert_eq!(got.body, b"hello admin world");

    // Head metadata.
    let head = request(a, "GET", "/api/object/head?bucket=docs&key=hello.txt", &auth, None);
    assert_eq!(head.status, 200);
    assert!(head.text().contains("\"content_length\":17"));

    // Listing the bucket shows the object.
    let objs = request(a, "GET", "/api/objects?bucket=docs&delimiter=/", &auth, None);
    assert!(objs.text().contains("\"key\":\"hello.txt\""));

    // Copy into another bucket.
    let copy = request(
        a,
        "POST",
        "/api/object/copy",
        &auth_json,
        Some(br#"{"src_bucket":"docs","src_key":"hello.txt","dst_bucket":"assets","dst_key":"copy.txt"}"#),
    );
    assert_eq!(copy.status, 200);
    let assets = request(a, "GET", "/api/objects?bucket=assets", &auth, None);
    assert!(assets.text().contains("\"key\":\"copy.txt\""));

    // Copy onto the same bucket+key must preserve the bytes, not truncate them
    // (`fs::copy(p, p)` empties the file — guard against that regression).
    let self_copy = request(
        a,
        "POST",
        "/api/object/copy",
        &auth_json,
        Some(br#"{"src_bucket":"docs","src_key":"hello.txt","dst_bucket":"docs","dst_key":"hello.txt"}"#),
    );
    assert_eq!(self_copy.status, 200);
    let after = request(a, "GET", "/api/object/get?bucket=docs&key=hello.txt", &auth, None);
    assert_eq!(after.body, b"hello admin world", "self-copy must not destroy object data");

    // Update metadata, then confirm content-type changed.
    let meta = request(
        a,
        "POST",
        "/api/object/metadata",
        &auth_json,
        Some(br#"{"bucket":"docs","key":"hello.txt","content_type":"application/json","metadata":{"team":"infra"}}"#),
    );
    assert_eq!(meta.status, 200);
    let head2 = request(a, "GET", "/api/object/head?bucket=docs&key=hello.txt", &auth, None);
    assert!(head2.text().contains("\"content_type\":\"application/json\""));
    assert!(head2.text().contains("\"team\":\"infra\""));

    // Batch delete.
    let del = request(
        a,
        "POST",
        "/api/objects/delete",
        &auth_json,
        Some(br#"{"bucket":"docs","keys":["hello.txt"]}"#),
    );
    assert_eq!(del.status, 200);
    assert!(del.text().contains("\"deleted\":[\"hello.txt\"]"));
}

/// Build an in-memory ZIP (deflate) from `(name, bytes)` entries.
fn make_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    use std::io::Write;
    let mut buf = std::io::Cursor::new(Vec::new());
    let mut w = zip::ZipWriter::new(&mut buf);
    let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    for (name, data) in entries {
        w.start_file(*name, opts).unwrap();
        w.write_all(data).unwrap();
    }
    w.finish().unwrap();
    buf.into_inner()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_zip_extract() {
    let srv = spawn().await;
    let a = srv.addr;
    let cookie = login(a);
    let auth = [("Cookie", cookie.as_str())];
    let auth_json = [("Cookie", cookie.as_str()), JSON];

    request(a, "POST", "/api/buckets", &auth_json, Some(br#"{"name":"site"}"#));

    // Upload an archive with a nested folder.
    let zip = make_zip(&[("index.html", b"<h1>hi</h1>"), ("css/app.css", b"body{}")]);
    let put = request(a, "PUT", "/api/object/put?bucket=site&key=bundle.zip", &auth, Some(&zip));
    assert_eq!(put.status, 200);

    // Extract into "web/".
    let ex = request(
        a,
        "POST",
        "/api/object/extract",
        &auth_json,
        Some(br#"{"bucket":"site","key":"bundle.zip","dest_prefix":"web/"}"#),
    );
    assert_eq!(ex.status, 200, "{}", ex.text());
    assert!(ex.text().contains("\"extracted_count\":2"), "{}", ex.text());

    // Each entry became its own object; folders are preserved as key prefixes and
    // the content type is guessed from the extension.
    let html = request(a, "GET", "/api/object/get?bucket=site&key=web/index.html", &auth, None);
    assert_eq!(html.status, 200);
    assert_eq!(html.body, b"<h1>hi</h1>");
    assert!(html.header("content-type").is_some_and(|c| c.contains("text/html")), "{:?}", html.header("content-type"));

    let css = request(a, "GET", "/api/object/get?bucket=site&key=web/css/app.css", &auth, None);
    assert_eq!(css.status, 200);
    assert_eq!(css.body, b"body{}");

    // The archive itself is left in place.
    assert_eq!(request(a, "GET", "/api/object/head?bucket=site&key=bundle.zip", &auth, None).status, 200);

    // Re-extracting without overwrite skips both existing files.
    let again = request(
        a,
        "POST",
        "/api/object/extract",
        &auth_json,
        Some(br#"{"bucket":"site","key":"bundle.zip","dest_prefix":"web/"}"#),
    );
    assert_eq!(again.status, 200, "{}", again.text());
    assert!(again.text().contains("\"skipped_count\":2"), "{}", again.text());
    assert!(again.text().contains("\"extracted_count\":0"), "{}", again.text());

    // A non-archive object is rejected as a bad request, not a 500.
    request(a, "PUT", "/api/object/put?bucket=site&key=notes.txt", &auth, Some(b"not a zip"));
    let bad = request(
        a,
        "POST",
        "/api/object/extract",
        &auth_json,
        Some(br#"{"bucket":"site","key":"notes.txt"}"#),
    );
    assert_eq!(bad.status, 400, "{}", bad.text());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_zip_extract_rejects_zip_slip() {
    let srv = spawn().await;
    let a = srv.addr;
    let cookie = login(a);
    let auth = [("Cookie", cookie.as_str())];
    let auth_json = [("Cookie", cookie.as_str()), JSON];

    request(a, "POST", "/api/buckets", &auth_json, Some(br#"{"name":"slip"}"#));

    // An entry that tries to escape the destination with `..` must be refused.
    let zip = make_zip(&[("../../etc/evil.txt", b"pwned")]);
    let put = request(a, "PUT", "/api/object/put?bucket=slip&key=evil.zip", &auth, Some(&zip));
    assert_eq!(put.status, 200);

    let r = request(
        a,
        "POST",
        "/api/object/extract",
        &auth_json,
        Some(br#"{"bucket":"slip","key":"evil.zip","dest_prefix":"out/"}"#),
    );
    assert_eq!(r.status, 400, "zip-slip path must be rejected: {}", r.text());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_presign_round_trip() {
    let srv = spawn_with_api().await;
    let (admin, api) = (srv.admin, srv.api);
    let login = request(
        admin,
        "POST",
        "/api/login",
        &[JSON],
        Some(br#"{"access_key":"admin-key","secret_key":"admin-secret"}"#),
    );
    let cookie = login.cookie().unwrap();
    let auth = [("Cookie", cookie.as_str())];

    request(admin, "POST", "/api/buckets", &[("Cookie", cookie.as_str()), JSON], Some(br#"{"name":"docs"}"#));
    request(admin, "PUT", "/api/object/put?bucket=docs&key=secret.txt", &auth, Some(b"signed payload"));

    // Generate a presigned GET URL; it is signed for the configured API host.
    let presign = request(admin, "GET", "/api/object/presign?bucket=docs&key=secret.txt&method=GET", &auth, None);
    assert_eq!(presign.status, 200);
    let url = presign.text();
    let url = url.split("\"url\":\"").nth(1).unwrap().split('"').next().unwrap().replace("\\u0026", "&");
    assert!(url.contains(&api.to_string()), "presigned URL must target the API host: {url}");

    // Strip scheme+host -> path?query, then fetch from the API port anonymously
    // (no cookie, no SigV4 header) — the signature alone must authorize it.
    let path_q = url.splitn(4, '/').nth(3).map(|s| format!("/{s}")).unwrap();
    let anon = request(api, "GET", &path_q, &[], None);
    assert_eq!(anon.status, 200, "presigned URL must verify: {path_q}");
    assert_eq!(anon.body, b"signed payload");

    // Without the signature, the same private object is forbidden on the API port.
    let unsigned = request(api, "GET", "/docs/secret.txt", &[], None);
    assert_eq!(unsigned.status, 403);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_settings_persist_and_reflect() {
    let root = unique_dir();
    {
        let config = admin_config(root.clone());
        // First run: no seeding, so the store starts at empty defaults.
        let settings = SettingsStore::open(&root).unwrap();
        let service = build_admin_service(&config, open_backend(&config).unwrap(), &settings);
        let (addr, _tx) = serve_service(service).await;
        let cookie = login(addr);
        let auth = [("Cookie", cookie.as_str())];
        let auth_json = [("Cookie", cookie.as_str()), JSON];

        // Fresh defaults are empty and the DB now exists on disk.
        let cfg = request(addr, "GET", "/api/config", &auth, None);
        assert_eq!(cfg.status, 200);
        assert!(cfg.text().contains("\"public_buckets\":[]"), "fresh config: {}", cfg.text());
        assert!(root.join(".s3-storage/settings.db").is_file(), "settings.db must be created");

        // Edit every field via PUT.
        let put = request(
            addr,
            "PUT",
            "/api/settings",
            &auth_json,
            Some(
                br#"{"public_buckets":["assets"],"domains":["cdn.example.com"],"domain_map":["files.example.com=assets"],"api_public_url":"https://api.example.com","admin_session_ttl_secs":1800}"#,
            ),
        );
        assert_eq!(put.status, 200);

        let t = request(addr, "GET", "/api/config", &auth, None).text();
        assert!(t.contains("\"public_buckets\":[\"assets\"]"), "{t}");
        assert!(t.contains("\"domains\":[\"cdn.example.com\"]"), "{t}");
        assert!(t.contains("files.example.com=assets"), "{t}");
        assert!(t.contains("\"api_public_url\":\"https://api.example.com\""), "{t}");
        assert!(t.contains("\"admin_session_ttl_secs\":1800"), "{t}");
    }

    // "Restart": a fresh store over the same root reads the persisted values.
    let reopened = SettingsStore::open(&root).unwrap();
    let snap = reopened.snapshot();
    assert_eq!(snap.public_buckets, vec!["assets".to_owned()]);
    assert_eq!(snap.domains, vec!["cdn.example.com".to_owned()]);
    assert_eq!(snap.domain_map, vec!["files.example.com=assets".to_owned()]);
    assert_eq!(snap.api_public_url.as_deref(), Some("https://api.example.com"));
    assert_eq!(snap.admin_session_ttl_secs, 1800);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_settings_validation() {
    let srv = spawn().await;
    let a = srv.addr;
    let cookie = login(a);
    let auth_json = [("Cookie", cookie.as_str()), JSON];

    // Malformed domain map entry.
    let bad = request(a, "PUT", "/api/settings", &auth_json, Some(br#"{"domain_map":["no-equals"]}"#));
    assert_eq!(bad.status, 400);
    // Zero TTL.
    let bad2 = request(a, "PUT", "/api/settings", &auth_json, Some(br#"{"admin_session_ttl_secs":0}"#));
    assert_eq!(bad2.status, 400);
    // Unauthenticated edits are rejected.
    let unauth = request(a, "PUT", "/api/settings", &[JSON], Some(br#"{"domains":[]}"#));
    assert_eq!(unauth.status, 401);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_public_toggle_serves_on_public_port() {
    let srv = spawn_with_public().await;
    std::fs::create_dir_all(srv.root.join("assets")).unwrap();
    std::fs::write(srv.root.join("assets/logo.txt"), b"PUBLIC").unwrap();

    // Initially private: the public port denies the anonymous read.
    assert_eq!(request(srv.public, "GET", "/assets/logo.txt", &[], None).status, 403);

    // Toggle the bucket public through the admin API.
    let cookie = login(srv.admin);
    let auth_json = [("Cookie", cookie.as_str()), JSON];
    let put = request(srv.admin, "PUT", "/api/settings", &auth_json, Some(br#"{"public_buckets":["assets"]}"#));
    assert_eq!(put.status, 200);

    // Now served anonymously on the public port — no restart.
    let r = request(srv.public, "GET", "/assets/logo.txt", &[], None);
    assert_eq!(r.status, 200);
    assert_eq!(r.body, b"PUBLIC");
}

/// The bucket name in `/api/buckets/{name}` is a raw path segment, so the handlers
/// must apply the AWS naming rules themselves. Without that, `.s3-storage` resolves
/// to the internal state directory under the data root and `DeleteBucket` removes
/// the settings database.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_rejects_internal_and_invalid_bucket_names() {
    let srv = spawn().await;
    let a = srv.addr;
    let cookie = login(a);
    let auth = [("Cookie", cookie.as_str())];

    for name in [".s3-storage", "..", "a", "UPPER", "has/slash", "bad..name"] {
        let path = format!("/api/buckets/{}", name.replace('/', "%2F"));
        let del = request(a, "DELETE", &path, &auth, None);
        assert_eq!(del.status, 400, "DELETE {name} must be rejected, got {}", del.text());
        let loc = request(a, "GET", &format!("{path}/location"), &auth, None);
        assert_eq!(loc.status, 400, "location {name} must be rejected");
        let ex = request(a, "GET", &format!("{path}/exists"), &auth, None);
        assert_eq!(ex.status, 400, "exists {name} must be rejected");
    }

    // The settings store survived: it is still readable and still has its defaults.
    let cfg = request(a, "GET", "/api/config", &auth, None);
    assert_eq!(cfg.status, 200);
    assert!(cfg.text().contains("\"assets\""), "settings db must be intact: {}", cfg.text());
}

/// Entries stream from the inflate task to the writer, so an archive with a
/// zip-slip path must still be rejected before *any* of its entries land, and a
/// well-formed archive must extract every entry in order.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_zip_extract_is_all_or_nothing_on_unsafe_paths() {
    let srv = spawn().await;
    let a = srv.addr;
    let cookie = login(a);
    let auth = [("Cookie", cookie.as_str())];
    let auth_json = [("Cookie", cookie.as_str()), JSON];

    request(a, "POST", "/api/buckets", &auth_json, Some(br#"{"name":"slipmix"}"#));

    // A safe entry ahead of an escaping one: the safe entry must not be written.
    let zip = make_zip(&[("safe.txt", b"ok"), ("../escape.txt", b"pwned")]);
    let put = request(a, "PUT", "/api/object/put?bucket=slipmix&key=mix.zip", &auth, Some(&zip));
    assert_eq!(put.status, 200);

    let r = request(
        a,
        "POST",
        "/api/object/extract",
        &auth_json,
        Some(br#"{"bucket":"slipmix","key":"mix.zip","dest_prefix":"out/"}"#),
    );
    assert_eq!(r.status, 400, "{}", r.text());

    let head = request(a, "GET", "/api/object/head?bucket=slipmix&key=out%2Fsafe.txt", &auth, None);
    assert_eq!(head.status, 404, "nothing may be written before the archive is validated");
}

/// `SameSite=Strict` scopes the session cookie to the registrable domain, so a page
/// served from a public bucket on a sibling subdomain is "same-site" and its
/// requests carry the session. Writes must therefore be checked against the origin.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_rejects_cross_origin_writes() {
    let srv = spawn().await;
    let a = srv.addr;
    let cookie = login(a);
    let own_origin = format!("http://{a}");
    let sibling = ("Origin", "http://files.example.com");

    // A sibling-subdomain page cannot drive a write, however valid the session.
    let body = br#"{"name":"pwned"}"#;
    let attack = [("Cookie", cookie.as_str()), JSON, sibling];
    let r = request(a, "POST", "/api/buckets", &attack, Some(body));
    assert_eq!(r.status, 403, "{}", r.text());

    // `Sec-Fetch-Site: same-site` describes exactly that case and is refused too.
    let sfs = [("Cookie", cookie.as_str()), JSON, ("Sec-Fetch-Site", "same-site")];
    assert_eq!(request(a, "POST", "/api/buckets", &sfs, Some(body)).status, 403);

    // A form can only send simple content types; JSON endpoints refuse them.
    let form = [("Cookie", cookie.as_str()), ("Content-Type", "text/plain")];
    assert_eq!(request(a, "POST", "/api/buckets", &form, Some(body)).status, 415);

    // The panel's own same-origin request still works.
    let ok = [("Cookie", cookie.as_str()), JSON, ("Origin", own_origin.as_str()), ("Sec-Fetch-Site", "same-origin")];
    assert_eq!(request(a, "POST", "/api/buckets", &ok, Some(body)).status, 200);

    // So does a non-browser client, which sends no Origin at all.
    let cli = [("Cookie", cookie.as_str()), JSON];
    let r = request(a, "POST", "/api/buckets", &cli, Some(br#"{"name":"from-cli"}"#));
    assert_eq!(r.status, 200, "{}", r.text());

    // Reads are unaffected.
    let read = [("Cookie", cookie.as_str()), sibling];
    assert_eq!(request(a, "GET", "/api/buckets", &read, None).status, 200);
}

/// Every admin response — SPA shell and JSON API alike — must carry the panel's
/// security headers. `frame-ancestors` is the operationally important one: the
/// panel offers one-click destructive actions and must not be framable.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_sends_security_headers() {
    let srv = spawn().await;
    let a = srv.addr;
    let cookie = login(a);

    for (path, extra) in [("/", &[][..]), ("/api/config", &[("Cookie", cookie.as_str())][..])] {
        let r = request(a, "GET", path, extra, None);
        let csp = r.header("content-security-policy").unwrap_or_default();
        assert!(csp.contains("frame-ancestors 'none'"), "{path}: {csp}");
        assert!(csp.contains("script-src 'self'"), "{path}: {csp}");
        assert!(csp.contains("object-src 'none'"), "{path}: {csp}");
        assert_eq!(r.header("x-frame-options"), Some("DENY"), "{path}");
        assert_eq!(r.header("x-content-type-options"), Some("nosniff"), "{path}");
        assert_eq!(r.header("referrer-policy"), Some("no-referrer"), "{path}");
    }
}

/// Repeated bad logins must be refused, and refused *immediately* rather than held
/// open: an earlier version slept out the penalty while holding the request, which
/// throttled guessing but let a flood of attempts pin connections.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn admin_login_throttles_after_repeated_failures() {
    let srv = spawn().await;
    let a = srv.addr;
    let bad = br#"{"access_key":"admin-key","secret_key":"wrong"}"#;

    // The first few failures are free, so a typo is not punished.
    for i in 0..3 {
        assert_eq!(request(a, "POST", "/api/login", &[JSON], Some(bad)).status, 401, "attempt {i}");
    }

    // Past that, attempts are refused with a Retry-After instead of being answered.
    let throttled = request(a, "POST", "/api/login", &[JSON], Some(bad));
    assert_eq!(throttled.status, 401, "the fourth failure is still answered");
    let refused = request(a, "POST", "/api/login", &[JSON], Some(bad));
    assert_eq!(refused.status, 429, "{}", refused.text());
    assert!(refused.header("retry-after").is_some(), "429 must say when to retry");

    // Many attempts at once must all come back promptly -- none may be parked.
    let start = std::time::Instant::now();
    let codes: Vec<u16> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..16)
            .map(|_| scope.spawn(|| request(a, "POST", "/api/login", &[JSON], Some(bad)).status))
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let elapsed = start.elapsed();
    assert!(elapsed < std::time::Duration::from_secs(5), "16 attempts took {elapsed:?}; requests are being parked");
    assert!(codes.iter().all(|c| *c == 429 || *c == 401), "unexpected codes {codes:?}");

    // A correct login still works once the cooldown lapses, and clears the streak.
    std::thread::sleep(std::time::Duration::from_millis(1200));
    let cookie = login(a);
    assert!(!cookie.is_empty());
    assert_eq!(
        request(a, "POST", "/api/login", &[JSON], Some(bad)).status,
        401,
        "a success must clear the streak, so the next failure is free again"
    );
}

/// With `--trust-proxy` set, the operator has declared that a TLS-terminating proxy
/// sits in front and sets `X-Forwarded-Proto` itself, so it is believed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_honors_forwarded_proto_when_proxy_is_trusted() {
    let root = unique_dir();
    let config = admin_config_with(root.clone(), true);
    let settings = seed_settings(&root, vec![], None);
    let service = build_admin_service(&config, open_backend(&config).unwrap(), &settings);
    let (a, _shutdown) = serve_service(service).await;

    let body = br#"{"access_key":"admin-key","secret_key":"admin-secret"}"#;
    let https = request(a, "POST", "/api/login", &[JSON, ("X-Forwarded-Proto", "https")], Some(body));
    assert!(https.header("set-cookie").unwrap().contains("Secure"), "HTTPS login cookie must be Secure");

    // A proxy chain lists the client first.
    let chained = request(a, "POST", "/api/login", &[JSON, ("X-Forwarded-Proto", "https, http")], Some(body));
    assert!(chained.header("set-cookie").unwrap().contains("Secure"));

    // Plain HTTP through the same proxy still gets a usable, non-Secure cookie.
    let plain = request(a, "POST", "/api/login", &[JSON, ("X-Forwarded-Proto", "http")], Some(body));
    assert!(!plain.header("set-cookie").unwrap().contains("Secure"));
}
