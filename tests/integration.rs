//! Dependency-free integration tests driving the server over raw HTTP.
//!
//! These cover bucket/object CRUD and listing on the API service (in open mode,
//! where no request signing is needed) and the public/private + custom-domain
//! access logic on the public service (exercised via anonymous requests). Full
//! SigV4-signed SDK behaviour, streaming uploads and multipart are covered by the
//! boto3 test (`tests/boto3_compat.rs`).

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::net::TcpListener;
use tokio::sync::oneshot;

use s3_storage::{Config, build_api_service, build_public_service, open_backend, serve};
use s3s::service::S3Service;

struct TestServer {
    addr: SocketAddr,
    root: PathBuf,
    _shutdown: oneshot::Sender<()>,
}

fn unique_dir() -> PathBuf {
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("s3-storage-it-{nanos}-{n}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn test_config(root: PathBuf, auth: bool, public_buckets: Vec<String>, domain_map: Vec<String>) -> Config {
    let (access_key, secret_key) = if auth {
        (Some("it-access".to_owned()), Some("it-secret".to_owned()))
    } else {
        (None, None)
    };
    Config {
        root,
        host: "127.0.0.1".to_owned(),
        port: 0,
        public_port: 0,
        access_key,
        secret_key,
        domains: vec![],
        public_buckets,
        domain_map,
        admin_enabled: false,
        admin_port: 0,
        admin_session_ttl_secs: 3600,
        api_public_url: None,
    }
}

/// Bind a listener and serve `service` on it, returning the live address.
async fn serve_on(root: PathBuf, service: S3Service) -> TestServer {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
        let _ = serve(service, listener, async {
            let _ = rx.await;
        })
        .await;
    });
    TestServer { addr, root, _shutdown: tx }
}

/// Spawn the authenticated **API** service (anonymous access rejected unless `auth`
/// is false, in which case it runs fully open for the CRUD tests).
async fn spawn(auth: bool, public_buckets: Vec<String>, domain_map: Vec<String>) -> TestServer {
    let root = unique_dir();
    let config = test_config(root.clone(), auth, public_buckets, domain_map);
    let service = build_api_service(&config, open_backend(&config).unwrap());
    serve_on(root, service).await
}

/// Spawn the **public** read-only service. Credentials are configured (so `s3s`
/// runs the access stage), but requests are made anonymously: only `GET`/`HEAD` of
/// public buckets is permitted.
async fn spawn_public(public_buckets: Vec<String>, domain_map: Vec<String>) -> TestServer {
    let root = unique_dir();
    let config = test_config(root.clone(), true, public_buckets, domain_map);
    let service = build_public_service(&config, open_backend(&config).unwrap());
    serve_on(root, service).await
}

struct Resp {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Resp {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn dechunk(mut body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let Some(pos) = find(body, b"\r\n") else { break };
        let size_str = std::str::from_utf8(&body[..pos]).unwrap();
        let size = usize::from_str_radix(size_str.split(';').next().unwrap().trim(), 16).unwrap();
        body = &body[pos + 2..];
        if size == 0 {
            break;
        }
        out.extend_from_slice(&body[..size]);
        body = &body[size + 2..]; // skip data + trailing CRLF
    }
    out
}

fn request(addr: SocketAddr, method: &str, host: &str, path: &str, body: Option<&[u8]>) -> Resp {
    let mut stream = TcpStream::connect(addr).unwrap();
    // Safety net so a protocol mishap fails the test instead of hanging it.
    stream.set_read_timeout(Some(std::time::Duration::from_secs(10))).unwrap();

    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n");
    if let Some(b) = body {
        req.push_str(&format!("Content-Length: {}\r\n", b.len()));
    }
    req.push_str("\r\n");
    stream.write_all(req.as_bytes()).unwrap();
    if let Some(b) = body {
        stream.write_all(b).unwrap();
    }
    stream.flush().unwrap();

    // Read until the header terminator is seen, parse framing, then read exactly
    // the framed body (Content-Length or chunked) rather than relying on EOF.
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
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    };
    let chunked = header("transfer-encoding").is_some_and(|v| v.eq_ignore_ascii_case("chunked"));
    let content_length: Option<usize> = header("content-length").and_then(|v| v.parse().ok());

    let mut body_buf = raw[header_end + 4..].to_vec();
    let read_more = |stream: &mut TcpStream, buf: &mut Vec<u8>| {
        let mut t = [0u8; 8192];
        let n = stream.read(&mut t).expect("read body");
        if n > 0 {
            buf.extend_from_slice(&t[..n]);
        }
        n
    };

    // HEAD and 204/304 responses never carry a body, even with a Content-Length.
    let bodyless = method.eq_ignore_ascii_case("HEAD") || matches!(status, 204 | 304);

    let body = if bodyless {
        Vec::new()
    } else if chunked {
        // Read until the terminating zero-length chunk is present.
        while find(&body_buf, b"\r\n0\r\n").is_none() && !body_buf.starts_with(b"0\r\n") {
            if read_more(&mut stream, &mut body_buf) == 0 {
                break;
            }
        }
        dechunk(&body_buf)
    } else if let Some(len) = content_length {
        while body_buf.len() < len {
            if read_more(&mut stream, &mut body_buf) == 0 {
                break;
            }
        }
        body_buf.truncate(len);
        body_buf
    } else {
        body_buf
    };

    Resp { status, headers, body }
}

fn get(addr: SocketAddr, path: &str) -> Resp {
    request(addr, "GET", &addr.to_string(), path, None)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn object_lifecycle_open_mode() {
    let srv = spawn(false, vec![], vec![]).await;
    let a = srv.addr;

    assert_eq!(request(a, "PUT", &a.to_string(), "/mybucket", None).status, 200);

    // Put + get round-trip.
    let put = request(a, "PUT", &a.to_string(), "/mybucket/hello.txt", Some(b"hello s3 world"));
    assert_eq!(put.status, 200);
    assert!(put.header("etag").is_some(), "PutObject must return an ETag");

    let got = get(a, "/mybucket/hello.txt");
    assert_eq!(got.status, 200);
    assert_eq!(got.body, b"hello s3 world");

    // HEAD.
    let head = request(a, "HEAD", &a.to_string(), "/mybucket/hello.txt", None);
    assert_eq!(head.status, 200);
    assert_eq!(head.header("content-length"), Some("14"));

    // Nested keys.
    assert_eq!(
        request(a, "PUT", &a.to_string(), "/mybucket/a/b/c.txt", Some(b"nested")).status,
        200
    );
    assert_eq!(get(a, "/mybucket/a/b/c.txt").body, b"nested");
    // Raw file is on disk at the nested path.
    assert!(srv.root.join("mybucket/a/b/c.txt").is_file());

    // List.
    let list = get(a, "/mybucket?list-type=2");
    assert_eq!(list.status, 200);
    let xml = String::from_utf8_lossy(&list.body);
    assert!(xml.contains("<Key>hello.txt</Key>"));
    assert!(xml.contains("<Key>a/b/c.txt</Key>"));

    // Delete + 404.
    assert_eq!(request(a, "DELETE", &a.to_string(), "/mybucket/hello.txt", None).status, 204);
    let gone = get(a, "/mybucket/hello.txt");
    assert_eq!(gone.status, 404);
    assert!(String::from_utf8_lossy(&gone.body).contains("NoSuchKey"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_prefix_and_delimiter_open_mode() {
    let srv = spawn(false, vec![], vec![]).await;
    let a = srv.addr;
    request(a, "PUT", &a.to_string(), "/listing", None);
    for key in ["docs/a.txt", "docs/b.txt", "docs/sub/c.txt", "root.txt"] {
        request(a, "PUT", &a.to_string(), &format!("/listing/{key}"), Some(b"x"));
    }

    let list = get(a, "/listing?list-type=2&prefix=docs/&delimiter=/");
    let xml = String::from_utf8_lossy(&list.body);
    assert!(xml.contains("<Key>docs/a.txt</Key>"));
    assert!(xml.contains("<Key>docs/b.txt</Key>"));
    assert!(!xml.contains("docs/sub/c.txt"), "delimited listing must not recurse");
    assert!(xml.contains("<Prefix>docs/sub/</Prefix>"), "expected common prefix docs/sub/");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn public_port_serves_public_buckets_only() {
    let srv = spawn_public(vec!["assets".to_owned()], vec![]).await;
    let a = srv.addr;

    // Seed objects directly on disk (the public service forbids unsigned writes).
    std::fs::create_dir_all(srv.root.join("assets")).unwrap();
    std::fs::write(srv.root.join("assets/logo.txt"), b"PUBLIC").unwrap();
    std::fs::create_dir_all(srv.root.join("secret")).unwrap();
    std::fs::write(srv.root.join("secret/data.txt"), b"PRIVATE").unwrap();

    // Anonymous read of a public bucket: allowed.
    let pub_read = get(a, "/assets/logo.txt");
    assert_eq!(pub_read.status, 200);
    assert_eq!(pub_read.body, b"PUBLIC");

    // Anonymous read of a private bucket: denied.
    assert_eq!(get(a, "/secret/data.txt").status, 403);

    // Anonymous write even to a public bucket: denied.
    let anon_put = request(a, "PUT", &a.to_string(), "/assets/new.txt", Some(b"nope"));
    assert_eq!(anon_put.status, 403);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_port_rejects_anonymous_even_for_public_buckets() {
    // The API port is strict: a "public" bucket is irrelevant without a signature.
    let srv = spawn(true, vec!["assets".to_owned()], vec![]).await;
    let a = srv.addr;
    std::fs::create_dir_all(srv.root.join("assets")).unwrap();
    std::fs::write(srv.root.join("assets/logo.txt"), b"PUBLIC").unwrap();

    assert_eq!(get(a, "/assets/logo.txt").status, 403);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn custom_domain_routes_to_bucket() {
    let srv = spawn_public(vec!["assets".to_owned()], vec!["files.example.com=assets".to_owned()]).await;
    let a = srv.addr;
    std::fs::create_dir_all(srv.root.join("assets")).unwrap();
    std::fs::write(srv.root.join("assets/index.html"), b"<html>").unwrap();

    // Host = custom domain, path = key -> resolves to the mapped (public) bucket.
    let resp = request(a, "GET", "files.example.com", "/index.html", None);
    assert_eq!(resp.status, 200, "custom domain should route to bucket");
    assert_eq!(resp.body, b"<html>");
}
