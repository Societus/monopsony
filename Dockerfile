# ============================================================================
# Monopsony Server Container
# Multi-stage: Alpine + Rust build, minimal runtime image
# Works with Docker, Podman (rootful and rootless)
# ============================================================================

# --- Build stage ---
FROM rust:1.84-alpine3.21 AS builder

# Build dependencies (protobuf + musl + openssl)
RUN apk add --no-cache \
    protobuf-dev protobuf gcc musl-dev pkgconf \
    openssl-dev openssl-libs-static

WORKDIR /build

# Copy manifests first (layer cache for deps)
COPY Cargo.toml Cargo.lock ./
COPY crates/monopsony-common/Cargo.toml crates/monopsony-common/
COPY crates/monopsony-db/Cargo.toml crates/monopsony-db/
COPY crates/monopsony-proxy/Cargo.toml crates/monopsony-proxy/
COPY crates/monopsony-control/Cargo.toml crates/monopsony-control/
COPY crates/monopsony-mirror/Cargo.toml crates/monopsony-mirror/
COPY crates/monopsony-shadow/Cargo.toml crates/monopsony-shadow/
COPY crates/monopsony-server/Cargo.toml crates/monopsony-server/
COPY crates/monopsony-client/Cargo.toml crates/monopsony-client/
COPY crates/monopsony-tracker/Cargo.toml crates/monopsony-tracker/

# Create dummy source files so cargo can resolve deps without full source
RUN mkdir -p crates/monopsony-common/src && echo "" > crates/monopsony-common/src/lib.rs && \
    mkdir -p crates/monopsony-db/src && echo "" > crates/monopsony-db/src/lib.rs && \
    mkdir -p crates/monopsony-proxy/src && echo "" > crates/monopsony-proxy/src/lib.rs && \
    mkdir -p crates/monopsony-control/src && echo "" > crates/monopsony-control/src/lib.rs && \
    mkdir -p crates/monopsony-mirror/src && echo "" > crates/monopsony-mirror/src/lib.rs && \
    mkdir -p crates/monopsony-shadow/src && echo "" > crates/monopsony-shadow/src/lib.rs && \
    mkdir -p crates/monopsony-server/src && echo "fn main() {}" > crates/monopsony-server/src/main.rs && \
    mkdir -p crates/monopsony-client/src && echo "fn main() {}" > crates/monopsony-client/src/main.rs && \
    mkdir -p crates/monopsony-tracker/src && echo "fn main() {}" > crates/monopsony-tracker/src/main.rs

# Copy proto for tonic-build
COPY proto/ proto/

# Pre-build deps (this layer is cached unless Cargo.toml/lock changes)
RUN cargo build --release --locked 2>/dev/null || true

# Copy full source (invalidates cache only when source changes)
COPY . .
RUN touch crates/monopsony-common/src/lib.rs \
          crates/monopsony-db/src/lib.rs \
          crates/monopsony-proxy/src/lib.rs \
          crates/monopsony-control/src/lib.rs \
          crates/monopsony-mirror/src/lib.rs \
          crates/monopsony-shadow/src/lib.rs
RUN cargo build --release --locked --bin monopsony-server \
                              --bin monopsony-client \
                              --bin monopsony-tracker

# --- Runtime stage ---
FROM alpine:3.21

# Runtime dependencies: ca-certificates for HTTPS upstream fetches
RUN apk add --no-cache ca-certificates tzdata

# Create non-root user
RUN addgroup -S monopsony && adduser -S -G monopsony -H monopsony

# OCI-compliant directory structure
RUN mkdir -p /etc/monopsony \
             /var/cache/monopsony/mirrors \
             /var/lib/monopsony \
             /var/log/monopsony && \
    chown -R monopsony:monopsony /etc/monopsony \
                                    /var/cache/monopsony \
                                    /var/lib/monopsony \
                                    /var/log/monopsony

# Copy binaries from builder
COPY --from=builder /build/target/release/monopsony-server /usr/local/bin/
COPY --from=builder /build/target/release/monopsony-client /usr/local/bin/
COPY --from=builder /build/target/release/monopsony-tracker /usr/local/bin/

# Copy default config and example mirrors
COPY dist/server.toml.example /etc/monopsony/server.toml.example
COPY dist/mirrors.toml.example /etc/monopsony/mirrors.toml.example

# Copy entrypoint
COPY dist/container/entrypoint.sh /entrypoint.sh
RUN chmod +x /entrypoint.sh

EXPOSE 3128 50051 9090 3129

VOLUME ["/var/cache/monopsony", "/var/lib/monopsony", "/etc/monopsony"]

USER monopsony
WORKDIR /home/monopsony

ENTRYPOINT ["/entrypoint.sh"]
CMD ["monopsony-server"]
