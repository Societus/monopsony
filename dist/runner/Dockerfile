# Monopsony Forgejo Runner Image
#
# Alpine-based image with Rust + build deps pre-installed.
# Eliminates per-job rustup download that was causing OOM kills (exit 137).
#
# Build:
#   docker build -t monopsony-runner:latest dist/runner/
#
# Run (adjust FORGEJO_URL and RUNNER_TOKEN):
#   docker run -d --name monopsony-runner \
#     -v runner-data:/data \
#     monopsony-runner:latest \
#     forgejo-runner daemon
#
# First-time registration:
#   docker run --rm monopsony-runner:latest \
#     forgejo-runner register \
#     --instance https://base.societus.club \
#     --token YOUR_RUNNER_TOKEN \
#     --name landhunter \
#     --labels alpine-latest,node-20

FROM alpine:3.21

# Build dependencies for Monopsony (protobuf, OpenSSL, musl, cross-compilation)
RUN apk add --no-cache \
    protobuf-dev protobuf \
    curl gcc musl-dev pkgconf \
    openssl-dev openssl-libs-static \
    gcc-aarch64-none-elf \
    git bash ca-certificates \
    nodejs npm \
    shadow

# Install Rust with minimal profile (no docs, no rustfmt — cuts ~50% download)
ENV RUSTUP_HOME=/usr/local/rustup
ENV CARGO_HOME=/usr/local/cargo
ENV PATH="/usr/local/cargo/bin:${PATH}"
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- \
    -y --default-toolchain stable --profile minimal && \
    rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl && \
    rustup component add clippy && \
    chmod -R a+rwX /usr/local/rustup /usr/local/cargo

# Install Forgejo Runner binary
# Check https://code.forgejo.org/forgejo/runner/releases for latest version
RUN curl -L -o /usr/local/bin/forgejo-runner \
    https://code.forgejo.org/forgejo/runner/releases/download/v6.3.1/forgejo-runner-6.3.1-linux-amd64 && \
    chmod +x /usr/local/bin/forgejo-runner

# Add swapfile support (for CI jobs that need extra headroom)
RUN apk add --no-cache util-linux

# Non-root runner user (Forgejo runner runs as this user)
RUN adduser -D -s /bin/bash runner && \
    mkdir -p /home/runner/.cargo && \
    chown -R runner:runner /home/runner

# Create configurable swap (1G by default, can be disabled with -e SWAP_SIZE=0)
ENV SWAP_SIZE=1024
COPY entrypoint.sh /entrypoint.sh
RUN chmod +x /entrypoint.sh

USER runner
WORKDIR /home/runner

ENTRYPOINT ["/entrypoint.sh"]
CMD ["forgejo-runner", "daemon"]
