//! Runs the boto3 cross-language SDK compatibility smoke test against a live
//! in-process instance. Skipped (test passes) when python3 or boto3 is missing,
//! so it never blocks `cargo test` in a minimal environment.

use std::path::PathBuf;
use std::process::Command;

use tokio::net::TcpListener;
use tokio::sync::oneshot;

use s3_storage::{Config, build_api_service, open_backend, serve};

const ACCESS_KEY: &str = "boto3-access";
const SECRET_KEY: &str = "boto3-secret";

fn boto3_available() -> bool {
    Command::new("python3")
        .args(["-c", "import boto3"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[tokio::test]
async fn boto3_sdk_compatibility() {
    if !boto3_available() {
        eprintln!("skipping boto3_sdk_compatibility: python3 + boto3 not available");
        return;
    }

    // Unique data root.
    let root = std::env::temp_dir().join(format!(
        "s3-storage-boto3-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).unwrap();

    let config = Config {
        root,
        host: "127.0.0.1".to_owned(),
        port: 0,
        public_port: 0,
        access_key: Some(ACCESS_KEY.to_owned()),
        secret_key: Some(SECRET_KEY.to_owned()),
        domains: vec![],
        public_buckets: vec![],
        domain_map: vec![],
        admin_enabled: false,
        admin_port: 0,
        admin_session_ttl_secs: 3600,
        api_public_url: None,
    };

    let service = build_api_service(&config, open_backend(&config).unwrap());
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
        let _ = serve(service, listener, async {
            let _ = rx.await;
        })
        .await;
    });

    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/smoke_boto3.py");
    let output = tokio::task::spawn_blocking(move || {
        Command::new("python3")
            .arg(&script)
            .env("S3_ENDPOINT", format!("http://{addr}"))
            .env("S3_ACCESS_KEY", ACCESS_KEY)
            .env("S3_SECRET_KEY", SECRET_KEY)
            .output()
            .expect("failed to run python3")
    })
    .await
    .unwrap();

    let _ = tx.send(());

    if !output.status.success() {
        panic!(
            "boto3 smoke test failed:\n--- stdout ---\n{}\n--- stderr ---\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    println!("{}", String::from_utf8_lossy(&output.stdout).trim());
}
