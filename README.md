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
- **Sync from a remote S3/MinIO bucket** — pull an existing bucket into this
  server from the admin panel, incrementally: re-running copies only what is new
  or changed, and an interrupted run is safe to re-run. See
  [Sync from a remote source](#sync-from-a-remote-source).
- **Docker-first** — small distroless runtime image, one volume at `/data`.

## Quick start (Docker Compose)

```bash
# Generate credentials. Compose requires them: it publishes all three ports on
# every interface, so there is no safe shared default to fall back to.
printf 'S3_ACCESS_KEY=%s\nS3_SECRET_KEY=%s\n' \
  "$(openssl rand -hex 12)" "$(openssl rand -base64 32)" > .env

# Build and run
docker compose up --build -d

# Configure the AWS CLI against it
export $(grep -v '^#' .env | xargs)
export AWS_ACCESS_KEY_ID="$S3_ACCESS_KEY"
export AWS_SECRET_ACCESS_KEY="$S3_SECRET_KEY"
aws --endpoint-url http://localhost:8080 s3 mb s3://demo
echo "hello" | aws --endpoint-url http://localhost:8080 s3 cp - s3://demo/hello.txt
aws --endpoint-url http://localhost:8080 s3 ls s3://demo/
aws --endpoint-url http://localhost:8080 s3 cp s3://demo/hello.txt -
```

> The AWS CLI/SDKs must use **path-style** addressing against the default URL.
> For the CLI this is automatic with `--endpoint-url`; SDKs need `force_path_style`
> (or `addressing_style = "path"`), shown below.

## Endpoints (three ports)

The server runs three single-purpose listeners so each can sit behind its own
domain/subdomain (e.g. via nginx):

| Port | Default | Serves | Auth |
|------|---------|--------|------|
| **API** | `8080` | The full S3 wire protocol for SDK clients (`ListObjects`, `PutObject`, multipart, …). | SigV4 only — **anonymous access is rejected**. |
| **Admin** | `8081` | The web admin panel + its JSON API, served at the **root** of the port. | Session cookie. |
| **Public** | `8082` | Anonymous `GET`/`HEAD` of buckets marked **public** in the admin panel. Ideal for a CDN/asset domain. | None (read-only). |

Anonymous public-bucket reads are served **only** on the public port; the API port
always requires a valid signature.

## Configuration

Startup settings — the bind address, ports, credentials, and whether the admin
panel is enabled — are CLI flags / environment variables. Everything else (public
buckets, domains, custom-domain mappings, the public API URL and the admin session
lifetime) is managed in the admin panel and persisted in a SQLite database; see
[Runtime settings](#runtime-settings).

| Env var             | Flag              | Default   | Description |
|---------------------|-------------------|-----------|-------------|
| `S3_ROOT`           | `--root`          | `/data`   | Data directory (mount a volume here). |
| `S3_HOST`           | `--host`          | `0.0.0.0` | Bind address (shared by all three ports). |
| `S3_PORT`           | `--port`          | `8080`    | Authenticated S3 API port. |
| `S3_ADMIN_PORT`     | `--admin-port`    | `8081`    | Admin panel port (panel served at root). |
| `S3_PUBLIC_PORT`    | `--public-port`   | `8082`    | Public read-only port (anonymous reads of public buckets). |
| `S3_ACCESS_KEY`     | `--access-key`    | —         | SigV4 access key (set with the secret). |
| `S3_SECRET_KEY`     | `--secret-key`    | —         | SigV4 secret key (set with the access key). |
| `S3_ADMIN_ENABLED`  | `--admin-enabled` | `false`   | Enable the embedded web admin panel (requires credentials). |

Notes:
- If `S3_ACCESS_KEY`/`S3_SECRET_KEY` are **unset**, the API port runs fully open and
  unauthenticated (handy for local development only).
- **Access mode is per-bucket.** A bucket is private by default; mark it public in
  the admin panel (Buckets page, or the Settings page) to allow anonymous
  `GET`/`HEAD` on the public port. Writes always require a valid signature (API port).
- **Custom domains** map a `Host` header to a bucket via the panel's domain map.
  Point the domain's DNS/your reverse proxy at the public port and preserve the
  original `Host` header.
- **Presigned links** are SigV4-signed over their host, so the admin panel must mint
  them against the host SDK clients actually reach. Set the **public API URL** in the
  panel settings. It is **required** to generate presigned links: when unset the
  panel returns a clear error rather than emitting a link that points at the admin
  port (which does not serve S3).

### Runtime settings

These deployment-facing settings live in a SQLite database at
`{root}/.s3-storage/settings.db` and are edited entirely through the admin panel
(no restart needed — changes take effect live):

- **Public buckets** — which buckets allow anonymous `GET`/`HEAD` on the public port.
- **Domains** — base domains for `<bucket>.<domain>` virtual-hosting.
- **Custom domain map** — `host=bucket` mappings that point a host straight at a bucket.
- **Allowed CORS origins** — origins (`scheme://host[:port]`, or `*` for any) allowed
  to read from the public port cross-origin. Sets `Access-Control-Allow-Origin` (and
  answers `OPTIONS` preflights) so browsers accept fonts and other CORS-gated
  subresources; blank means no CORS headers are sent. With `*` the header is sent on
  every public read, including requests that carry no `Origin` at all — otherwise a
  CDN or browser cache could store the header-less copy that a plain `<img>` fetch
  produces and replay it to a request that is in CORS mode.
- **Public API URL** — the API's public base URL used to mint presigned links.
- **Admin session lifetime** — how long a login stays valid (applies to new logins).

The database is created automatically on first start with empty defaults. Because
it lives under the data root, it travels with your storage volume and is included in
backups. Bind address, ports and credentials are deliberately **not** stored here —
they bootstrap the panel itself and stay on the CLI/env.

## Admin panel

An optional web admin panel ships inside the binary. Enable it with
`S3_ADMIN_ENABLED=true` (it also requires `S3_ACCESS_KEY`/`S3_SECRET_KEY` — there
is nothing to log in with otherwise). It is served at the **root of its own port**
(`S3_ADMIN_PORT`, default `8081`), so open `http://host:8081/`.

```bash
cargo run -- --root ./data --access-key key --secret-key secret --admin-enabled
# then browse to http://localhost:8081/ and log in with key / secret
```

- **Login** uses your S3 access key + secret key; a signed, `HttpOnly` session
  cookie (lifetime set in the panel's Settings page) keeps you signed in. The cookie is
  marked `Secure` only when the request arrives over HTTPS — from the request
  scheme, or from `X-Forwarded-Proto` when you run with `--trust-proxy`
  (`S3_TRUST_PROXY=true`) to say a TLS-terminating reverse proxy sets that header.
  Set it behind such a proxy; leave it off otherwise, since any client can send the
  header itself. The panel works over plain HTTP too — though serving it over
  **HTTPS** is strongly recommended so the session cookie is never sent in the
  clear. No SigV4 signing
  happens in the browser — the panel calls a same-origin JSON API (`/api/*` on the
  admin port) that reuses the storage backend directly, so no CORS setup is needed.
- **Login is rate-limited.** After the first few failures each further one opens a
  cooldown (250ms, doubling to 5s) during which attempts are refused with `429` and
  a `Retry-After`, so the secret key cannot be brute-forced through the panel.
  Attempts are refused rather than delayed, so a flood costs the server nothing,
  and it is a cooldown rather than an account lockout: with a single credential
  pair, locking out would let anyone who can reach the port deny you access. A
  successful login clears the streak.
- **Writes are same-origin only.** `SameSite=Strict` scopes the session cookie to
  the registrable domain, which still counts a public bucket served from a sibling
  subdomain (`files.example.com` vs `admin.example.com`) as same-site — and public
  buckets serve caller-supplied HTML. So every non-`GET` `/api/*` request is also
  checked against `Origin` / `Sec-Fetch-Site`, and JSON endpoints require
  `Content-Type: application/json` (which an HTML form cannot send). Scripting the
  admin API with curl or an SDK is unaffected: a request with no `Origin` is not
  browser-initiated and passes.
- **Covers every server feature**: dashboard stats, bucket create/delete with a
  live public/private toggle, a Settings page for domains / custom-domain map /
  public API URL / session lifetime, an object browser (folder navigation, drag-and-drop
  upload, download, byte-range, copy/move/rename, batch delete, folders, metadata
  editor, checksums, presigned GET/PUT share links), multipart session
  management (list parts, abort), and a Sync page for pulling a bucket in from a
  remote S3/MinIO endpoint (see [Sync from a remote source](#sync-from-a-remote-source)).
- **Extract ZIP archives server-side.** Any `.zip` object shows an *Extract*
  action that unpacks it into individual objects in the same bucket — each file
  becomes its own object, archive folders are preserved as key prefixes, and a
  Content-Type is guessed per extension (so unpacked static sites serve correctly).
  Choose a destination prefix and whether to overwrite existing objects; the
  archive itself is left in place. Extraction is bounded against zip-slip paths and
  oversized/zip-bomb archives.
- **Dedicated port, no path shadowing.** The panel owns its own port, so it never
  collides with bucket names and you can front it with its own domain. Presigned
  links it generates target the API port — set the **public API URL** in the panel
  settings so they point at the host clients actually reach (see
  [Runtime settings](#runtime-settings)).
- If credentials are not configured the panel stays disabled (a warning is logged)
  and the API port continues to serve open/unauthenticated.

The frontend source lives in `admin-ui/` (React + Vite + Tailwind). The Docker
build compiles it automatically. For a local `cargo run`/`cargo build` only an
empty `admin-ui/dist/` is committed (a `.gitkeep`, so that rust-embed finds the
directory and the crate compiles from a fresh clone) — the panel then answers
"Admin UI is not built" until you run `npm --prefix admin-ui install && npm
--prefix admin-ui run build` to embed the real UI.

### docker-compose.yml

```yaml
services:
  s3-storage:
    build: .
    image: s3-storage:latest
    ports:
      - "8080:8080"   # API
      - "8081:8081"   # admin panel
      - "8082:8082"   # public reads
    environment:
      # Required — from a .env file beside the compose file, never a literal.
      S3_ACCESS_KEY: ${S3_ACCESS_KEY:?set S3_ACCESS_KEY}
      S3_SECRET_KEY: ${S3_SECRET_KEY:?set S3_SECRET_KEY}
      S3_ADMIN_ENABLED: "true"             # then set public buckets / domains in the panel
      S3_TRUST_PROXY: "false"              # true only behind a TLS-terminating proxy
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
export S3_ACCESS_KEY="$(openssl rand -hex 12)"
export S3_SECRET_KEY="$(openssl rand -base64 32)"
docker run -d --name s3-storage -p 8080:8080 -p 8081:8081 -p 8082:8082 \
  -e S3_ACCESS_KEY -e S3_SECRET_KEY \
  -e S3_ADMIN_ENABLED=true \
  -v s3data:/data \
  s3-storage
# then open http://localhost:8081/ and mark buckets public / add domains in the panel
```

## Sync from a remote source

The admin panel can pull objects out of an existing MinIO (or any S3-compatible)
bucket into this server. It is meant for migrating onto `s3-storage`, and for
keeping a copy topped up afterwards.

Open the panel, go to **Sync from Remote**, and give it the source endpoint,
credentials, and the bucket to read. **Preview** shows exactly which objects
would be copied, why, and how many bytes that is, before anything is written.

What it does:

- **Incremental.** A re-run copies only objects that are missing, a different
  size, or newer on the source; everything else is skipped. The second run over
  an unchanged bucket copies nothing.
- **Resumable for free.** Objects are staged to a temporary file and put in place
  with an atomic rename, so a cancelled or interrupted run leaves only *complete*
  objects behind — never a truncated one. Re-run it and the rest is copied.
- **Streaming.** Objects are piped from the source straight to disk, so object
  size is bounded by your disk, not by memory.
- **Preserves** content type, content encoding, content disposition, content
  language, cache control, expiry and user metadata.

Three modes: **New & changed** (the default), **Skip existing** (never replace
what is already here), and **Overwrite all** (copy everything unconditionally —
the escape hatch if you suspect the comparison was wrong).

Notes worth knowing:

- **Credentials are never stored.** They are supplied per run and held only for
  its duration; the panel remembers the endpoint and scope in your browser, but
  never the secret key. Repeatability comes from the sync being incremental, not
  from the server keeping a password.
- **One run at a time.** A second start is refused (`409`) rather than queued —
  two syncs into one destination would race to write the same key.
- **A run does not survive a restart**, because it is held in memory. That is
  safe precisely because re-running resumes; just start it again.
- **Self-signed or private CA?** Paste the CA certificate into the *Advanced*
  box on the source card. There is deliberately no "skip certificate
  verification" switch.
- **Checksum verification** is opt-in and expensive: it re-reads every local
  object to hash it. It also cannot verify objects that were uploaded to the
  source in multiple parts — their ETag is a composite this server cannot
  reproduce — so those fall back to the size-and-time comparison.
- **There is no size cap.** A sync will happily fill the disk; use *Preview* to
  see the byte total first. Neither that nor the object/byte limits substitute
  for a disk quota.

The same thing is available over the API if you would rather script it:
`POST /api/sync/preview`, `POST /api/sync/runs`, `GET /api/sync/runs/current`,
`GET /api/sync/runs`, and `POST /api/sync/runs/{id}/cancel` — all behind the
panel's session cookie.

## Client examples

### Python (boto3)

```python
import os

import boto3
from botocore.config import Config

s3 = boto3.client(
    "s3",
    endpoint_url="http://localhost:8080",
    aws_access_key_id=os.environ["S3_ACCESS_KEY"],
    aws_secret_access_key=os.environ["S3_SECRET_KEY"],
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
        std::env::var("S3_ACCESS_KEY")?, std::env::var("S3_SECRET_KEY")?, None, None, "static"))
    .force_path_style(true)
    .build();
let client = aws_sdk_s3::Client::from_conf(conf);
```

### Anonymous / public bucket

If `assets` is public, objects are readable without credentials on the public port:

```bash
curl http://localhost:8082/assets/logo.png
# or via a mapped custom domain pointed at the public port:
curl http://files.example.com/logo.png
```

## Development

```bash
cargo run -- --root ./data --port 8080            # open mode (no auth)
cargo run -- --root ./data --access-key key --secret-key secret --admin-enabled
# then open http://localhost:8081/ to mark buckets public and add domain mappings
```

## Tests

```bash
cargo test
```

> Tests write their data roots under the system temp directory and, following the
> existing convention in this suite, do not remove them afterwards. Repeated runs
> accumulate; clear them with `rm -rf ${TMPDIR:-/tmp}/s3-storage-*` if space gets
> tight.

- `tests/integration.rs` — dependency-free raw-HTTP tests for bucket/object CRUD,
  listing (prefix + delimiter), public/private anonymous access, and custom-domain
  routing.
- `tests/admin.rs` — the admin panel's JSON API: session auth, object lifecycle,
  archive extraction, presigned-link round-trips, live settings changes.
- `tests/sync.rs` — remote sync. Because this server is itself S3-compatible, the
  "remote MinIO" is a *second* in-process instance of it, so the tests exercise a
  real SigV4-authenticated S3 endpoint over loopback with no external service.
  One test in that file additionally runs against a **real** MinIO when you point
  it at one, and is **skipped** otherwise:

  ```bash
  MINIO_ENDPOINT=http://127.0.0.1:9000 \
  MINIO_ACCESS_KEY=... MINIO_SECRET_KEY=... MINIO_BUCKET=some-bucket \
    cargo test --test sync syncs_from_a_real_minio -- --nocapture
  ```

  It only reads from the source bucket. Point it at one holding a
  multipart-uploaded object to cover the composite-ETag case.
- `tests/boto3_compat.rs` + `tests/smoke_boto3.py` — cross-language SDK
  compatibility via boto3 (full SigV4, streaming upload, multipart). Automatically
  **skipped** if `python3`/`boto3` are not installed.

## Security & operational notes

This server implements S3 authentication and access control, but like the
underlying `s3s` adapter it has **no built-in network hardening**. Before exposing
it to untrusted networks:

- **The admin port is a control plane — never expose it publicly.** Beyond full
  read/write access to storage, the sync feature makes the *server* open outbound
  connections to an endpoint the operator supplies, so anyone who reaches the
  panel can point it at hosts the server can see. The scheme is restricted to
  `http`/`https` and every run's endpoint is logged, but the real control is
  keeping the port off untrusted networks.

- **Terminate TLS** at a reverse proxy (nginx/Caddy/Traefik) and forward to it;
  preserve the original `Host` header. SigV4 signs it on the API port, and the
  admin port compares it against `Origin` to reject cross-site writes — a proxy
  that rewrites `Host` (to `localhost:8081`, say) will make the panel answer `403`
  to every write. In nginx: `proxy_set_header Host $host;`.
- **Set `S3_TRUST_PROXY=true` only once such a proxy is in front**, so
  `X-Forwarded-Proto` can be believed and the admin session cookie is marked
  `Secure`. With the server directly reachable, leave it off — any client can send
  that header.
- **Limit upload size / disk usage** — object uploads are streamed to disk with no
  size cap; an unauthenticated public bucket or a compromised key could fill the
  volume. Add request-size limits and rate limiting at the proxy, and monitor disk.
- **Use strong, rotated credentials** via `S3_ACCESS_KEY`/`S3_SECRET_KEY`. Never
  run with auth disabled (no credentials) on a public network.
- **Keep public buckets read-only by intent** — anonymous access is limited to
  `GET`/`HEAD` on buckets you explicitly mark public in the admin panel; writes always
  require a valid signature.
- **Public reads are served `X-Content-Type-Options: nosniff`**, so a browser
  honours the stored `Content-Type` instead of sniffing caller-supplied bytes into
  something more dangerous. Objects stored without a `Content-Type` are therefore
  not sniffed either — set one on upload (SDKs do) or from the panel's metadata
  editor if you serve a static site.
- **The admin panel sends a strict CSP** (`script-src 'self'`,
  `frame-ancestors 'none'`), so it cannot be framed and clickjacked into one-click
  destructive actions. Its webfonts come from Google Fonts; if you must run without
  external requests, self-host them and tighten `style-src`/`font-src`.

Report vulnerabilities privately via the repository's security contact rather than
a public issue.

## License

Apache-2.0 — see [LICENSE](LICENSE) and [NOTICE](NOTICE). The `src/backend/` storage
engine is derived from [`s3s` / `s3s-fs`](https://github.com/Nugine/s3s)
(Copyright 2023 Nugine, Apache-2.0) and has been modified for this project.
