#!/bin/sh
# ============================================================================
# Monopsony container entrypoint
# Detects Docker vs Podman, rootful vs rootless, adjusts UID/GID and paths
# ============================================================================

set -e

# ---------------------------------------------------------------------------
# Runtime detection
# ---------------------------------------------------------------------------
CONTAINER_ENGINE="unknown"
if [ -f /run/.containerenv ]; then
    # Podman creates this file; Docker does not
    CONTAINER_ENGINE="podman"
elif [ -f /.dockerenv ]; then
    CONTAINER_ENGINE="docker"
else
    # Fallback: check for podman cgroup
    if grep -q 'podman' /proc/1/cgroup 2>/dev/null; then
        CONTAINER_ENGINE="podman"
    else
        CONTAINER_ENGINE="docker"
    fi
fi

# Detect rootless: effective UID is not 0
if [ "$(id -u)" -eq 0 ]; then
    ROOTLESS="false"
else
    ROOTLESS="true"
fi

echo "[monopsony] engine=${CONTAINER_ENGINE} rootless=${ROOTLESS} uid=$(id -u) gid=$(id -g)"

# ---------------------------------------------------------------------------
# Configuration generation
# ---------------------------------------------------------------------------
# If no server.toml exists, generate one from the example with env overrides
CONFIG_FILE="/etc/monopsony/server.toml"

if [ ! -f "${CONFIG_FILE}" ]; then
    echo "[monopsony] No config at ${CONFIG_FILE}, generating from example..."

    # Copy example as base
    cp /etc/monopsony/server.toml.example "${CONFIG_FILE}"

    # Override database URL with env (compose sets this)
    if [ -n "${MONOPSONY_DB_URL:-}" ]; then
        sed -i "s|^url = .*|url = \"${MONOPSONY_DB_URL}\"|" "${CONFIG_FILE}"
    fi

    # Override bind addresses
    if [ -n "${MONOPSONY_HTTP_ADDR:-}" ]; then
        sed -i "s|^bind = .*|bind = \"${MONOPSONY_HTTP_ADDR}\"|" "${CONFIG_FILE}"
    fi

    # Override cache path
    if [ -n "${MONOPSONY_CACHE_DIR:-}" ]; then
        sed -i "s|^path = .*|path = \"${MONOPSONY_CACHE_DIR}\"|" "${CONFIG_FILE}"
    fi

    # Override TLS paths (if volume mounted)
    if [ -n "${MONOPSONY_TLS_DIR:-}" ]; then
        sed -i "s|^ca_path = .*|ca_path = \"${MONOPSONY_TLS_DIR}/ca.pem\"|" "${CONFIG_FILE}"
        sed -i "s|^ca_key_path = .*|ca_key_path = \"${MONOPSONY_TLS_DIR}/ca.key\"|" "${CONFIG_FILE}"
        sed -i "s|^server_cert_path = .*|server_cert_path = \"${MONOPSONY_TLS_DIR}/server.pem\"|" "${CONFIG_FILE}"
        sed -i "s|^server_key_path = .*|server_key_path = \"${MONOPSONY_TLS_DIR}/server.key\"|" "${CONFIG_FILE}"
    fi

    echo "[monopsony] Config written to ${CONFIG_FILE}"
fi

# ---------------------------------------------------------------------------
# Rootless directory permission fix
# ---------------------------------------------------------------------------
if [ "${ROOTLESS}" = "true" ]; then
    # Ensure writable cache/lib dirs for unprivileged user
    for d in /var/cache/monopsony /var/lib/monopsony /var/log/monopsony /etc/monopsony; do
        if [ -w "$d" ]; then
            :
        else
            echo "[monopsony] WARNING: ${d} is not writable. Mount with correct uid/gid."
            echo "[monopsony]          Podman rootless: podman unshare chown or set 'userns: keep-id' in compose."
        fi
    done
fi

# ---------------------------------------------------------------------------
# Execute
# ---------------------------------------------------------------------------
echo "[monopsony] starting: $*"
exec "$@"
