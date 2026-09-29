# ---- Admin UI build stage ----
# Builds the React admin panel; the output is embedded into the binary by
# rust-embed at compile time, so it must exist before the Rust build.
FROM node:22-slim AS ui
WORKDIR /ui
# App version stamped into the UI bundle (set by CI on each push; "dev" otherwise).
ARG APP_VERSION=dev
ENV APP_VERSION=$APP_VERSION
COPY admin-ui/package.json admin-ui/package-lock.json* ./
RUN npm install
COPY admin-ui/ ./
RUN npm run build

# ---- Build stage ----
# Rust 1.92+ is required (edition 2024). `rust:1-slim-bookworm` tracks latest stable.
FROM rust:1-slim-bookworm AS builder

WORKDIR /app

# Build dependencies first for better layer caching.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src \
    && echo 'fn main() {}' > src/main.rs \
    && echo '' > src/lib.rs \
    && cargo build --release --bin s3-storage 2>/dev/null || true
RUN rm -rf src

# Build the real sources, with the built admin UI in place for rust-embed.
COPY src ./src
COPY --from=ui /ui/dist ./admin-ui/dist
RUN touch src/main.rs src/lib.rs && cargo build --release --bin s3-storage

# Staged here only so the runtime stage can COPY it in with the right ownership:
# distroless has no shell, so there is no way to mkdir/chown the data directory
# there. A fresh named volume inherits this ownership when Docker initialises it
# from the image.
RUN mkdir -p /data

# ---- Runtime stage ----
# distroless/cc provides glibc + libgcc with no shell or package manager. The
# `nonroot` variant runs as uid/gid 65532 instead of root, so a container escape
# does not start out with root on the host side of the namespace.
FROM gcr.io/distroless/cc-debian12:nonroot

COPY --from=builder /app/target/release/s3-storage /usr/local/bin/s3-storage
COPY --from=builder --chown=nonroot:nonroot /data /data

# Defaults; override via environment (see README / docker-compose.yml).
# Three single-purpose ports: API (8080), admin panel (8081), public reads (8082).
# The remote-sync client is the only thing here that makes outbound HTTPS
# connections, and it finds its roots through the platform trust store. The
# distroless base already sets SSL_CERT_FILE to the bundle it ships; we restate
# it alongside SSL_CERT_DIR (which the base does not set) so the trust-store
# location is visible in this file -- there is no shell in the image to go and
# look, and a miss shows up only as a TLS handshake failure at sync time.
#
# Concurrency: every file read runs on a blocking-pool thread, so
# S3_MAX_BLOCKING_THREADS caps how many downloads can be reading at once (tokio's
# own default is 512). S3_LISTEN_BACKLOG only takes effect up to the kernel's
# `net.core.somaxconn`, which the image cannot set -- it is per container, see the
# `sysctls:` in docker-compose.yml. S3_WORKER_THREADS is left unset: one per CPU
# core, as the container sees them.
ENV S3_ROOT=/data \
    S3_HOST=0.0.0.0 \
    S3_PORT=8080 \
    S3_ADMIN_PORT=8081 \
    S3_PUBLIC_PORT=8082 \
    S3_MAX_BLOCKING_THREADS=1024 \
    S3_LISTEN_BACKLOG=4096 \
    SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt \
    SSL_CERT_DIR=/etc/ssl/certs \
    RUST_LOG=info

VOLUME ["/data"]
EXPOSE 8080 8081 8082

# Explicit for the reader; the `nonroot` base already selects this user. A
# *bind*-mounted host directory keeps its own ownership, so chown it to 65532
# (`chown 65532:65532 ./data`) or the server cannot write to it.
USER nonroot:nonroot

ENTRYPOINT ["/usr/local/bin/s3-storage"]
