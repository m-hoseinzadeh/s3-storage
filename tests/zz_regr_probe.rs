//! TEMPORARY regression probe. Delete after the review.
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::net::TcpListener;
use tokio::sync::oneshot;

use s3_storage::{Config, SettingsStore, build_admin_service, build_api_service, open_backend, serve};

fn unique_dir() -> PathBuf {
    use std::time::{SystemTime, UNIX_EPOCH};
    static C: AtomicU64 = AtomicU64::new(0);
    let n = C.fetch_add(1, Ordering::SeqCst);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let d = std::env::temp_dir().join(format!("s3-regr-{nanos}-{n}"));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn cfg(root: PathBuf, creds: bool) -> Config {
    Config {
        root,
        host: "127.0.0.1".into(),
        port: 0,
        public_port: 0,
        access_key: creds.then(|| "k".to_owned()),
        secret_key: creds.then(|| "s".to_owned()),
        admin_enabled: creds,
        admin_port: 0,
        trust_proxy: false,
    }
}

async fn serve_svc(svc: s3s::service::S3Service) -> (SocketAddr, oneshot::Sender<()>) {
    let l = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = l.local_addr().unwrap();
    let (tx, rx) = oneshot::channel::<()>();
    tokio::spawn(async move { let _ = serve(svc, l, async { let _ = rx.await; }).await; });
    (addr, tx)
}

fn find(h: &[u8], n: &[u8]) -> Option<usize> { h.windows(n.len()).position(|w| w == n) }

fn req(addr: SocketAddr, method: &str, path: &str, extra: &[(&str, &str)], body: Option<&[u8]>) -> (u16, String) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(std::time::Duration::from_secs(20))).unwrap();
    let mut r = format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n");
    for (k, v) in extra { r.push_str(&format!("{k}: {v}\r\n")); }
    if let Some(b) = body { r.push_str(&format!("Content-Length: {}\r\n", b.len())); }
    r.push_str("\r\n");
    s.write_all(r.as_bytes()).unwrap();
    if let Some(b) = body { s.write_all(b).unwrap(); }
    s.flush().unwrap();
    let mut raw = Vec::new();
    let mut t = [0u8; 8192];
    loop { match s.read(&mut t) { Ok(0) | Err(_) => break, Ok(n) => raw.extend_from_slice(&t[..n]) } }
    let he = find(&raw, b"\r\n\r\n").unwrap();
    let head = String::from_utf8_lossy(&raw[..he]).into_owned();
    let status: u16 = head.lines().next().unwrap().split_whitespace().nth(1).unwrap().parse().unwrap();
    (status, String::from_utf8_lossy(&raw[he + 4..]).into_owned())
}

/// R1: the admin panel's Multipart page lists parts for an upload created over the
/// S3 API. `list_parts` now demands session ownership — does the admin path match?
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn r1_admin_list_parts_still_works() {
    let root = unique_dir();
    let config = cfg(root.clone(), true);
    let settings = SettingsStore::open(&root).unwrap();
    let fs = open_backend(&config).unwrap();
    let (api, _a) = serve_svc(build_api_service(&config, std::sync::Arc::clone(&fs), &settings)).await;
    let (admin, _b) = serve_svc(build_admin_service(&config, fs, &settings)).await;

    // Create bucket + a multipart upload through the admin API / backend.
    let (_, head) = req(admin, "POST", "/api/login", &[("Content-Type", "application/json")],
        Some(br#"{"access_key":"k","secret_key":"s"}"#));
    let _ = head;
    // login cookie
    let mut s = TcpStream::connect(admin).unwrap();
    s.set_read_timeout(Some(std::time::Duration::from_secs(10))).unwrap();
    let body = br#"{"access_key":"k","secret_key":"s"}"#;
    let r = format!("POST /api/login HTTP/1.1\r\nHost: {admin}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n", body.len());
    s.write_all(r.as_bytes()).unwrap(); s.write_all(body).unwrap();
    let mut raw = Vec::new(); let mut t = [0u8; 8192];
    loop { match s.read(&mut t) { Ok(0) | Err(_) => break, Ok(n) => raw.extend_from_slice(&t[..n]) } }
    let head = String::from_utf8_lossy(&raw).into_owned();
    let cookie = head.lines().find(|l| l.to_ascii_lowercase().starts_with("set-cookie"))
        .unwrap().split_once(':').unwrap().1.split(';').next().unwrap().trim().to_owned();
    let auth = [("Cookie", cookie.as_str()), ("Content-Type", "application/json")];
    assert_eq!(req(admin, "POST", "/api/buckets", &auth, Some(br#"{"name":"mpbkt"}"#)).0, 200);

    // Multipart upload over the API port would need SigV4; instead drive the backend
    // through the admin service's own credentials by creating the session on the API
    // port in open mode is not possible here, so use the API port unsigned: with
    // credentials configured it will reject. So we check the admin path directly.
    let (st, body) = req(admin, "GET", "/api/multipart", &[("Cookie", cookie.as_str())], None);
    println!("admin list uploads -> {st} {body}");

    // Now the important bit: a bogus upload id must not 500, and the endpoint must
    // still be reachable (403/400, not a panic).
    let (st2, b2) = req(admin, "GET", "/api/multipart/parts?bucket=mpbkt&key=x&upload_id=not-a-uuid",
        &[("Cookie", cookie.as_str())], None);
    println!("admin list parts (bad id) -> {st2} {b2}");
    let (st3, b3) = req(admin, "GET",
        "/api/multipart/parts?bucket=mpbkt&key=x&upload_id=00000000-0000-4000-8000-000000000000",
        &[("Cookie", cookie.as_str())], None);
    println!("admin list parts (unknown id) -> {st3} {b3}");
    let _ = api;
}

/// R5: batch-deleting the key "a/" when a *file* named "a" exists.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn r5_batch_delete_trailing_slash_over_a_file() {
    let root = unique_dir();
    let config = cfg(root.clone(), false);
    let settings = SettingsStore::open(&root).unwrap();
    let (api, _a) = serve_svc(build_api_service(&config, open_backend(&config).unwrap(), &settings)).await;

    assert_eq!(req(api, "PUT", "/bkt", &[], None).0, 200);
    assert_eq!(req(api, "PUT", "/bkt/a", &[], Some(b"file")).0, 200);

    let (st1, b1) = req(api, "POST", "/bkt?delete", &[],
        Some(b"<Delete><Object><Key>a/</Key></Object></Delete>"));
    println!("batch delete 'a/' over a file -> {st1}\n{}", &b1[..b1.len().min(300)]);

    let (st2, b2) = req(api, "DELETE", "/bkt/a/", &[], None);
    println!("single delete 'a/' over a file -> {st2}\n{}", &b2[..b2.len().min(300)]);

    let (st3, _) = req(api, "GET", "/bkt/a", &[], None);
    println!("object 'a' still present -> {st3}");
}

/// R4: prefix listing must return exactly the keys starting with the prefix, after
/// the prefix-directed descent optimisation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn r4_prefix_listing_matches_brute_force() {
    let root = unique_dir();
    let config = cfg(root.clone(), false);
    let settings = SettingsStore::open(&root).unwrap();
    let (api, _a) = serve_svc(build_api_service(&config, open_backend(&config).unwrap(), &settings)).await;
    assert_eq!(req(api, "PUT", "/bkt", &[], None).0, 200);

    // No key may be both a file and a directory prefix: this backend stores keys as
    // real paths, so "a" and "a/b" cannot coexist (pre-existing limitation).
    let keys = [
        "a", "ab", "zz", "a-x", "a.x",
        "d/b", "d/bc", "d/b2/c", "d/b3/c/e", "db/c", "dbc/d/e", "e/a", "d/x/y/z",
    ];
    for k in keys {
        assert_eq!(req(api, "PUT", &format!("/bkt/{k}"), &[], Some(b"x")).0, 200, "put {k}");
    }

    for prefix in ["", "a", "d", "d/", "d/b", "d/b2", "d/b2/", "db", "dbc", "e", "zz", "d/x", "q", "a-"] {
        let (_, body) = req(api, "GET", &format!("/bkt?list-type=2&max-keys=1000&prefix={prefix}"), &[], None);
        let mut got: Vec<String> = body
            .split("<Key>").skip(1)
            .map(|c| c.split_once("</Key>").unwrap().0.to_owned())
            .collect();
        got.sort();
        let mut want: Vec<String> = keys.iter().filter(|k| k.starts_with(prefix)).map(|s| s.to_string()).collect();
        want.sort();
        assert_eq!(got, want, "prefix {prefix:?}");
        println!("prefix {prefix:?} -> {} keys ok", got.len());
    }
}

/// R2: does the login throttle pin connections? Fire many bad logins concurrently
/// and see how long they take to all come back.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn r2_concurrent_bad_logins() {
    let root = unique_dir();
    let config = cfg(root.clone(), true);
    let settings = SettingsStore::open(&root).unwrap();
    let (admin, _b) = serve_svc(build_admin_service(&config, open_backend(&config).unwrap(), &settings)).await;

    let start = std::time::Instant::now();
    let mut handles = Vec::new();
    for _ in 0..12 {
        handles.push(std::thread::spawn(move || {
            req(admin, "POST", "/api/login", &[("Content-Type", "application/json")],
                Some(br#"{"access_key":"k","secret_key":"nope"}"#)).0
        }));
    }
    let codes: Vec<u16> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    println!("12 concurrent bad logins took {:?}, codes {codes:?}", start.elapsed());
}
