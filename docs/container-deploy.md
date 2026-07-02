# Container Deployment

Run Monopsony server in Docker or Podman. Works rootful and rootless.

## Quick Start

### Docker

```bash
git clone https://github.com/Societus/monopsony.git
cd monopsony

# Set a strong password
export POSTGRES_PASSWORD="$(openssl rand -hex 16)"

# Build and start
docker compose up -d

# Verify
curl http://localhost:3128/
curl http://localhost:9090/metrics
```

### Podman (rootful)

```bash
git clone https://github.com/Societus/monopsony.git
cd monopsony

export POSTGRES_PASSWORD="$(openssl rand -hex 16)"

podman-compose up -d
```

### Podman (rootless)

Rootless Podman runs containers as your user. The entrypoint detects this and
warns about permission issues. Two approaches:

**Option A: keep-id user namespace (recommended)**

Uncomment `userns: keep-id` in `docker-compose.yml` under the `server` service.
Then:

```bash
podman-compose up -d
```

This maps your host UID/GID into the container, so volume permissions match.

**Option B: chown volumes before starting**

```bash
# Find the subuid/subgid range podman uses
podman unshare cat /proc/self/uid_map

# chown the volume mount points to the mapped UID (typically first subuid)
podman unshare chown -R 0:0 ./  # or specific uid/gid

podman-compose up -d
```

## Ports

| Port  | Service               | Description                        |
|-------|-----------------------|------------------------------------|
| 3128  | HTTP proxy            | Package manager traffic            |
| 50051 | gRPC control plane    | Client registration (mTLS)         |
| 9090  | Prometheus metrics    | /health and /metrics endpoints     |
| 3129  | Mirror HTTP           | Distribution mirror serving        |

Override with environment variables in compose:

```yaml
environment:
  MONOPSONY_PROXY_PORT: 8080
  MONOPSONY_GRPC_PORT: 50051
  MONOPSONY_METRICS_PORT: 9090
  MONOPSONY_MIRROR_PORT: 3129
```

## Volumes

| Volume     | Container Path         | Purpose                          |
|------------|------------------------|----------------------------------|
| cache-data | /var/cache/monopsony   | Cached package files             |
| lib-data   | /var/lib/monopsony     | TLS certs, CA, server state      |
| db-data    | /var/lib/postgresql/data | PostgreSQL database            |

Data persists across container restarts. To wipe and start fresh:

```bash
docker compose down -v
```

## Configuration

The container auto-generates `/etc/monopsony/server.toml` on first run using
the example config with environment variable overrides. To provide your own:

1. Copy the example:
   ```bash
   cp dist/server.toml.example server.toml
   ```

2. Edit `server.toml` to taste.

3. Mount it in compose:
   ```yaml
   volumes:
     - ./server.toml:/etc/monopsony/server.toml:ro
   ```

### Environment Overrides

When no mounted config exists, these env vars override the generated config:

| Variable              | Default                           | Description            |
|-----------------------|-----------------------------------|------------------------|
| MONOPSONY_DB_URL      | postgres://monopsony:***@db:5432/monopsony | PostgreSQL URL |
| MONOPSONY_HTTP_ADDR  | 0.0.0.0:3128                     | Proxy bind address     |
| MONOPSONY_CACHE_DIR   | /var/cache/monopsony             | Cache directory        |
| MONOPSONY_TLS_DIR     | (none)                            | TLS cert directory     |

### TLS / mTLS

By default the server runs in open auth mode. To enable mTLS:

1. Generate certs (one-time, from inside the container):
   ```bash
   docker compose exec server monopsony-server setup
   ```

2. Mount the TLS directory (compose already has the commented-out line).

3. Set auth mode in your `server.toml`:
   ```toml
   [server]
   auth_mode = "mtls"
   ```

## Interacting with the Container

### Shell access

```bash
docker compose exec server sh
# or podman:
podman-compose exec server sh
```

### View logs

```bash
docker compose logs -f server
```

### Check health

```bash
curl http://localhost:9090/health
curl http://localhost:9090/metrics | grep monopsony_cache
```

### Pair a client

```bash
# From inside the server container
docker compose exec server monopsony-server pair --server localhost:50051

# Or from a client machine pointing at the container host
monopsony-client pair --server YOUR_HOST_IP:50051
```

### Tracker (optional)

Uncomment the `tracker` service in `docker-compose.yml` to enable mirror
federation. The tracker runs on port 3130.

## Building from Source (without compose)

```bash
docker build -t monopsony:latest .

docker run -d \
  --name monopsony \
  -e MONOPSONY_DB_URL=postgres://monopsony:secret@my-pg-host:5432/monopsony \
  -p 3128:3128 -p 50051:50051 -p 9090:9090 -p 3129:3129 \
  -v monopsony-cache:/var/cache/monopsony \
  -v monopsony-lib:/var/lib/monopsony \
  monopsony:latest
```

## OCI Registry (preparation)

The image is built with standard Docker conventions (multi-stage Alpine,
non-root user, OCI-compliant layering). To push to a registry when ready:

```bash
# Build for current arch
docker build -t ghcr.io/societus/monopsony:latest .

# Multi-arch (requires buildx or podman build)
docker buildx build --platform linux/amd64,linux/arm64 \
  -t ghcr.io/societus/monopsony:latest \
  -t ghcr.io/societus/monopsony:v1.0.0 \
  --push .

# Podman equivalent
podman build --manifest ghcr.io/societus/monopsony:latest .
podman manifest push ghcr.io/societus/monopsony:latest
```

Tag scheme: `vX.Y.Z` for releases, `latest` tracks stable branch.
Registry: `ghcr.io/societus/monopsony` (GitHub Container Registry).
