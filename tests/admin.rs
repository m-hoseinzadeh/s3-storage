//! Integration tests for the embedded admin panel, driving its JSON API over raw
//! HTTP (session-cookie auth, bucket/object lifecycle, presigned-URL round-trip).

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::net::TcpListener;
use tokio::sync::oneshot;

use s3_storage::{Config, build_service, serve};

struct TestServer {
    addr: SocketAddr,
    _shutdown: oneshot::Sender<()>,
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

async fn spawn() -> TestServer {
    let config = Config {
        root: unique_dir(),
        host: "127.0.0.1".to_owned(),
        port: 0,
        access_key: Some("admin-key".to_owned()),
        secret_key: Some("admin-secret".to_owned()),
        domains: vec![],
        public_buckets: vec!["assets".to_owned()],
        domain_map: vec![],
        admin_enabled: true,
        admin_path: "/admin".to_owned(),
        admin_session_ttl_secs: 3600,
    };
    let service = build_service(&config).unwrap();
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
        let _ = serve(service, listener, async {
            let _ = rx.await;
        })
        .await;
    });
    TestServer { addr, _shutdown: tx }
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

    // Bare /admin redirects to /admin/.
    let redirect = request(a, "GET", "/admin", &[], None);
    assert_eq!(redirect.status, 302);
    assert_eq!(redirect.header("location"), Some("/admin/"));

    // The SPA shell is served at /admin/ (and as a fallback for client routes).
    let index = request(a, "GET", "/admin/", &[], None);
    assert_eq!(index.status, 200);
    assert!(index.header("content-type").is_some_and(|c| c.contains("text/html")));
    assert!(index.text().contains("<div id=\"root\">"));
    let spa_fallback = request(a, "GET", "/admin/buckets", &[], None);
    assert_eq!(spa_fallback.status, 200, "client-side routes fall back to index.html");

    // Wrong credentials are rejected.
    let bad = request(a, "POST", "/admin/api/login", &[JSON], Some(br#"{"access_key":"x","secret_key":"y"}"#));
    assert_eq!(bad.status, 401);

    // API without a session cookie is rejected.
    assert_eq!(request(a, "GET", "/admin/api/buckets", &[], None).status, 401);

    // Correct credentials issue a session cookie.
    let ok = request(
        a,
        "POST",
        "/admin/api/login",
        &[JSON],
        Some(br#"{"access_key":"admin-key","secret_key":"admin-secret"}"#),
    );
    assert_eq!(ok.status, 200);
    let cookie = ok.cookie().expect("login must set a session cookie");
    assert!(cookie.starts_with("s3admin_session="));

    // The cookie authorizes API calls.
    let session = request(a, "GET", "/admin/api/session", &[("Cookie", &cookie)], None);
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
        "/admin/api/login",
        &[JSON],
        Some(br#"{"access_key":"admin-key","secret_key":"admin-secret"}"#),
    );
    let cookie = login.cookie().unwrap();
    let auth = [("Cookie", cookie.as_str())];
    let auth_json = [("Cookie", cookie.as_str()), JSON];

    // Create buckets.
    assert_eq!(request(a, "POST", "/admin/api/buckets", &auth_json, Some(br#"{"name":"docs"}"#)).status, 200);
    assert_eq!(request(a, "POST", "/admin/api/buckets", &auth_json, Some(br#"{"name":"assets"}"#)).status, 200);

    let buckets = request(a, "GET", "/admin/api/buckets", &auth, None);
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
        "/admin/api/object/put?bucket=docs&key=hello.txt&content_type=text/plain",
        &auth,
        Some(b"hello admin world"),
    );
    assert_eq!(put.status, 200);

    // Download round-trip.
    let got = request(a, "GET", "/admin/api/object/get?bucket=docs&key=hello.txt", &auth, None);
    assert_eq!(got.status, 200);
    assert_eq!(got.body, b"hello admin world");

    // Head metadata.
    let head = request(a, "GET", "/admin/api/object/head?bucket=docs&key=hello.txt", &auth, None);
    assert_eq!(head.status, 200);
    assert!(head.text().contains("\"content_length\":17"));

    // Listing the bucket shows the object.
    let objs = request(a, "GET", "/admin/api/objects?bucket=docs&delimiter=/", &auth, None);
    assert!(objs.text().contains("\"key\":\"hello.txt\""));

    // Copy into another bucket.
    let copy = request(
        a,
        "POST",
        "/admin/api/object/copy",
        &auth_json,
        Some(br#"{"src_bucket":"docs","src_key":"hello.txt","dst_bucket":"assets","dst_key":"copy.txt"}"#),
    );
    assert_eq!(copy.status, 200);
    let assets = request(a, "GET", "/admin/api/objects?bucket=assets", &auth, None);
    assert!(assets.text().contains("\"key\":\"copy.txt\""));

    // Update metadata, then confirm content-type changed.
    let meta = request(
        a,
        "POST",
        "/admin/api/object/metadata",
        &auth_json,
        Some(br#"{"bucket":"docs","key":"hello.txt","content_type":"application/json","metadata":{"team":"infra"}}"#),
    );
    assert_eq!(meta.status, 200);
    let head2 = request(a, "GET", "/admin/api/object/head?bucket=docs&key=hello.txt", &auth, None);
    assert!(head2.text().contains("\"content_type\":\"application/json\""));
    assert!(head2.text().contains("\"team\":\"infra\""));

    // Batch delete.
    let del = request(
        a,
        "POST",
        "/admin/api/objects/delete",
        &auth_json,
        Some(br#"{"bucket":"docs","keys":["hello.txt"]}"#),
    );
    assert_eq!(del.status, 200);
    assert!(del.text().contains("\"deleted\":[\"hello.txt\"]"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_presign_round_trip() {
    let srv = spawn().await;
    let a = srv.addr;
    let login = request(
        a,
        "POST",
        "/admin/api/login",
        &[JSON],
        Some(br#"{"access_key":"admin-key","secret_key":"admin-secret"}"#),
    );
    let cookie = login.cookie().unwrap();
    let auth = [("Cookie", cookie.as_str())];

    request(a, "POST", "/admin/api/buckets", &[("Cookie", cookie.as_str()), JSON], Some(br#"{"name":"docs"}"#));
    request(a, "PUT", "/admin/api/object/put?bucket=docs&key=secret.txt", &auth, Some(b"signed payload"));

    // Generate a presigned GET URL.
    let presign = request(a, "GET", "/admin/api/object/presign?bucket=docs&key=secret.txt&method=GET", &auth, None);
    assert_eq!(presign.status, 200);
    let url = presign.text();
    let url = url.split("\"url\":\"").nth(1).unwrap().split('"').next().unwrap().replace("\\u0026", "&");

    // Strip scheme+host -> path?query, then fetch anonymously (no cookie, no SigV4 header).
    let path_q = url.splitn(4, '/').nth(3).map(|s| format!("/{s}")).unwrap();
    let anon = request(a, "GET", &path_q, &[], None);
    assert_eq!(anon.status, 200, "presigned URL must verify: {path_q}");
    assert_eq!(anon.body, b"signed payload");

    // Without the signature, the same private object is forbidden.
    let unsigned = request(a, "GET", "/docs/secret.txt", &[], None);
    assert_eq!(unsigned.status, 403);
}
