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

Notes:
- If `S3_ACCESS_KEY`/`S3_SECRET_KEY` are **unset**, the server runs fully open and
  unauthenticated (handy for local development only).
- **Access mode is per-bucket and configuration-driven.** A bucket is private by
  default; list it in `S3_PUBLIC_BUCKETS` to allow anonymous `GET`/`HEAD`. Writes
  always require a valid signature.
- **Custom domains** map a `Host` header to a bucket via `S3_DOMAIN_MAP`. Point the
  domain's DNS/your reverse proxy at this server and preserve the original `Host`
  header.

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

## License

Apache-2.0. The `src/backend/` storage engine is adapted from
[`s3s-fs`](https://github.com/Nugine/s3s) (Apache-2.0).
