# s3-storage

A minimal, S3-compatible file server that stores raw uploaded files directly on
disk — no database. It speaks enough of the Amazon S3 REST API to work with common
AWS S3 SDKs across languages (boto3, AWS SDK for Java/JS/.NET/Go/Rust, the AWS CLI),
and is built to run in a container with a single volume for storage.

The S3 wire protocol (AWS Signature V4 — header, presigned, and streaming/chunked
uploads — plus XML, routing, and multipart dispatch) is handled by the
[`s3s`](https://github.com/Nugine/s3s) crate. The on-disk storage engine under
`src/backend/` is adapted from the Apache-2.0 `s3s-fs` reference implementation; the
deployment-facing layers (authentication, public/private access, path-style and
custom-domain routing) are this project's own.

## Features

- **Raw files on disk** — a bucket is a directory, an object is a file. Object keys
  with `/` become nested directories. Small JSON sidecars hold per-object
  metadata/checksums; the data root is the only state.
- **Broad SDK compatibility** — full SigV4 verification incl. per-chunk streaming
  signatures and the modern `STREAMING-UNSIGNED-PAYLOAD-TRAILER` upload, presigned
  URLs, ETags, ranged GETs, and multipart upload.
- **Buckets** with:
  - **default path-style URLs** — `http://host:8080/<bucket>/<key>`
  - **optional custom-domain mapping** — `http://files.example.com/<key>` → a bucket
  - **public or private access** — public buckets allow anonymous reads
- **Essential APIs only** — `ListBuckets`, `CreateBucket`, `HeadBucket`,
  `DeleteBucket`, `PutObject`, `GetObject`, `HeadObject`, `DeleteObject`,
  `ListObjectsV2`, and multipart (`CreateMultipartUpload` / `UploadPart` /
  `CompleteMultipartUpload` / `AbortMultipartUpload` / `ListParts`).
- **Web admin panel** (optional) — an embedded React dashboard for buckets, the
  object browser (upload/download/copy/move/metadata/checksums/presigned links),
  and multipart sessions. Served from the same binary, no extra service. See
  [Admin panel](#admin-panel).
- **Docker-first** — small distroless runtime image, one volume at `/data`.

## Quick start (Docker Compose)

```bash
# Build and run
docker compose up --build -d

# Configure the AWS CLI against it
export AWS_ACCESS_KEY_ID=s3storage
export AWS_SECRET_ACCESS_KEY=s3storage-secret
aws --endpoint-url http://localhost:8080 s3 mb s3://demo
echo "hello" | aws --endpoint-url http://localhost:8080 s3 cp - s3://demo/hello.txt
aws --endpoint-url http://localhost:8080 s3 ls s3://demo/
aws --endpoint-url http://localhost:8080 s3 cp s3://demo/hello.txt -
```

> The AWS CLI/SDKs must use **path-style** addressing against the default URL.
> For the CLI this is automatic with `--endpoint-url`; SDKs need `force_path_style`
> (or `addressing_style = "path"`), shown below.

## Configuration

All settings are available as CLI flags and environment variables.

| Env var             | Flag              | Default   | Description |
|---------------------|-------------------|-----------|-------------|
| `S3_ROOT`           | `--root`          | `/data`   | Data directory (mount a volume here). |
| `S3_HOST`           | `--host`          | `0.0.0.0` | Bind address. |
| `S3_PORT`           | `--port`          | `8080`    | Listen port. |
| `S3_ACCESS_KEY`     | `--access-key`    | —         | SigV4 access key (set with the secret). |
| `S3_SECRET_KEY`     | `--secret-key`    | —         | SigV4 secret key (set with the access key). |
| `S3_PUBLIC_BUCKETS` | `--public-bucket` | —         | Comma-separated buckets that allow anonymous reads. |
| `S3_DOMAINS`        | `--domain`        | —         | Comma-separated base domains for `<bucket>.<domain>` virtual-hosting. |
| `S3_DOMAIN_MAP`     | `--domain-map`    | —         | Comma-separated `host=bucket` custom-domain mappings. |
| `S3_ADMIN_ENABLED`  | `--admin-enabled` | `false`   | Enable the embedded web admin panel (requires credentials). |
| `S3_ADMIN_PATH`     | `--admin-path`    | `/admin`  | URL path prefix the admin panel is served under. |
| `S3_ADMIN_SESSION_TTL` | `--admin-session-ttl` | `3600` | Admin session lifetime, in seconds. |

Notes:
- If `S3_ACCESS_KEY`/`S3_SECRET_KEY` are **unset**, the server runs fully open and
  unauthenticated (handy for local development only).
- **Access mode is per-bucket and configuration-driven.** A bucket is private by
  default; list it in `S3_PUBLIC_BUCKETS` to allow anonymous `GET`/`HEAD`. Writes
  always require a valid signature.
- **Custom domains** map a `Host` header to a bucket via `S3_DOMAIN_MAP`. Point the
  domain's DNS/your reverse proxy at this server and preserve the original `Host`
  header.

## Admin panel

An optional web admin panel ships inside the binary. Enable it with
`S3_ADMIN_ENABLED=true` (it also requires `S3_ACCESS_KEY`/`S3_SECRET_KEY` — there
is nothing to log in with otherwise) and open `http://host:8080/admin`.

```bash
cargo run -- --root ./data --access-key key --secret-key secret --admin-enabled
# then browse to http://localhost:8080/admin and log in with key / secret
```

- **Login** uses your S3 access key + secret key; a signed, `HttpOnly` session
  cookie (lifetime `S3_ADMIN_SESSION_TTL`) keeps you signed in. No SigV4 signing
  happens in the browser — the panel calls a same-origin JSON API that reuses the
  storage backend directly, so no CORS setup is needed.
- **Covers every server feature**: dashboard stats, bucket create/delete with
  public/private status, an object browser (folder navigation, drag-and-drop
  upload, download, byte-range, copy/move/rename, batch delete, folders, metadata
  editor, checksums, presigned GET/PUT share links), and multipart session
  management (list parts, abort).
- **Path-style shadowing caveat:** while enabled, the admin path prefix (default
  `/admin`) is reserved — a bucket with that exact name is not reachable via
  path-style URLs. Pick a different `S3_ADMIN_PATH`, or avoid naming a bucket
  `admin`. Virtual-hosted/custom-domain access to such a bucket is unaffected.
- If credentials are not configured the panel stays disabled (a warning is logged)
  and the plain S3 API continues to serve open/unauthenticated.

The frontend source lives in `admin-ui/` (React + Vite + Tailwind). The Docker
build compiles it automatically; for local `cargo run`/`cargo build` a placeholder
shell is committed, so run `npm --prefix admin-ui install && npm --prefix admin-ui
run build` to embed the real UI.

### docker-compose.yml

```yaml
services:
  s3-storage:
    build: .
    image: s3-storage:latest
    ports:
      - "8080:8080"
    environment:
      S3_ACCESS_KEY: s3storage
      S3_SECRET_KEY: s3storage-secret
      S3_PUBLIC_BUCKETS: assets            # anonymous reads on "assets"
      S3_DOMAIN_MAP: files.example.com=assets
      RUST_LOG: info
    volumes:
      - s3data:/data                       # or:  - ./data:/data
    restart: unless-stopped

volumes:
  s3data:
```

### Plain Docker

```bash
docker build -t s3-storage .
docker run -d --name s3-storage -p 8080:8080 \
  -e S3_ACCESS_KEY=s3storage -e S3_SECRET_KEY=s3storage-secret \
  -e S3_PUBLIC_BUCKETS=assets \
  -v s3data:/data \
  s3-storage
```

## Client examples

### Python (boto3)

```python
import boto3
from botocore.config import Config

s3 = boto3.client(
    "s3",
    endpoint_url="http://localhost:8080",
    aws_access_key_id="s3storage",
    aws_secret_access_key="s3storage-secret",
    region_name="us-east-1",
    config=Config(s3={"addressing_style": "path"}),
)
s3.create_bucket(Bucket="demo")
s3.put_object(Bucket="demo", Key="hello.txt", Body=b"hello")
print(s3.get_object(Bucket="demo", Key="hello.txt")["Body"].read())
```

### Rust (aws-sdk-s3)

```rust
let conf = aws_sdk_s3::config::Builder::new()
    .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
    .endpoint_url("http://localhost:8080")
    .region(aws_sdk_s3::config::Region::new("us-east-1"))
    .credentials_provider(aws_sdk_s3::config::Credentials::new(
        "s3storage", "s3storage-secret", None, None, "static"))
    .force_path_style(true)
    .build();
let client = aws_sdk_s3::Client::from_conf(conf);
```

### Anonymous / public bucket

If `assets` is public, objects are readable without credentials:

```bash
curl http://localhost:8080/assets/logo.png
# or via a mapped custom domain:
curl http://files.example.com/logo.png
```

## Development

```bash
cargo run -- --root ./data --port 8080            # open mode (no auth)
cargo run -- --root ./data --access-key key --secret-key secret \
             --public-bucket assets --domain-map files.example.com=assets
```

## Tests

```bash
cargo test
```

- `tests/integration.rs` — dependency-free raw-HTTP tests for bucket/object CRUD,
  listing (prefix + delimiter), public/private anonymous access, and custom-domain
  routing.
- `tests/boto3_compat.rs` + `tests/smoke_boto3.py` — cross-language SDK
  compatibility via boto3 (full SigV4, streaming upload, multipart). Automatically
  **skipped** if `python3`/`boto3` are not installed.

## Security & operational notes

This server implements S3 authentication and access control, but like the
underlying `s3s` adapter it has **no built-in network hardening**. Before exposing
it to untrusted networks:

- **Terminate TLS** at a reverse proxy (nginx/Caddy/Traefik) and forward to it;
  preserve the original `Host` header (SigV4 signs it).
- **Limit upload size / disk usage** — object uploads are streamed to disk with no
  size cap; an unauthenticated public bucket or a compromised key could fill the
  volume. Add request-size limits and rate limiting at the proxy, and monitor disk.
- **Use strong, rotated credentials** via `S3_ACCESS_KEY`/`S3_SECRET_KEY`. Never
  run with auth disabled (no credentials) on a public network.
- **Keep public buckets read-only by intent** — anonymous access is limited to
  `GET`/`HEAD` on buckets you explicitly list in `S3_PUBLIC_BUCKETS`; writes always
  require a valid signature.

Report vulnerabilities privately via the repository's security contact rather than
a public issue.

## License

Apache-2.0 — see [LICENSE](LICENSE) and [NOTICE](NOTICE). The `src/backend/` storage
engine is derived from [`s3s` / `s3s-fs`](https://github.com/Nugine/s3s)
(Copyright 2023 Nugine, Apache-2.0) and has been modified for this project.
