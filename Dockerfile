# ---- Admin UI build stage ----
# Builds the React admin panel; the output is embedded into the binary by
# rust-embed at compile time, so it must exist before the Rust build.
FROM node:22-slim AS ui
WORKDIR /ui
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

# ---- Runtime stage ----
# distroless/cc provides glibc + libgcc with no shell or package manager.
FROM gcr.io/distroless/cc-debian12

COPY --from=builder /app/target/release/s3-storage /usr/local/bin/s3-storage

# Defaults; override via environment (see README / docker-compose.yml).
# Three single-purpose ports: API (8080), admin panel (8081), public reads (8082).
ENV S3_ROOT=/data \
    S3_HOST=0.0.0.0 \
    S3_PORT=8080 \
    S3_ADMIN_PORT=8081 \
    S3_PUBLIC_PORT=8082 \
    RUST_LOG=info

VOLUME ["/data"]
EXPOSE 8080 8081 8082

ENTRYPOINT ["/usr/local/bin/s3-storage"]
