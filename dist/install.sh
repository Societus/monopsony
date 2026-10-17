#!/usr/bin/env bash
# ============================================================================
# Monopsony Installer — curl | bash one-liner installer
# ============================================================================
# Usage:
#   curl -fsSL https://monopsony.dev/install.sh | bash                        # client (default)
#   curl -fsSL https://monopsony.dev/install.sh | bash -s -- --server         # server install
#   curl -fsSL https://monopsony.dev/install.sh | bash -s -- --uninstall
#   curl -fsSL https://monopsony.dev/install.sh | bash -s -- --upgrade
#   curl -fsSL https://monopsony.dev/install.sh | bash -s -- --reconfigure
#   bash install.sh --detect-init          # print detected init system + evidence
#   bash install.sh --check-templates      # verify embedded templates match dist/init/
#   bash install.sh --emit-templates DIR   # regenerate dist/init/ from embedded copies
#
# Init systems (auto-detected): systemd, OpenRC, runit, s6, SysV init.d, launchd (macOS).
# Pin explicitly with MONOPSONY_INIT_SYSTEM=<name> for testing or unusual setups.
#
# Supported OSes: Arch, Debian 12+, Ubuntu 22.04+, Rocky/Alma/RHEL 9+, Alpine 3.x,
#                 openSUSE (Tumbleweed/Leap), Void Linux, Gentoo, Clear Linux, Solus,
#                 NixOS (client only), macOS (server via launchd)
# Graceful bail:  FreeBSD (not Linux/macOS)
# Supported arches: x86_64, aarch64 (Linux), x86_64/arm64 (macOS)
# ============================================================================

set -euo pipefail

# ---------------------------------------------------------------------------
# Globals
# ---------------------------------------------------------------------------
# Pinned fallback when the latest-release lookup fails (no network / API down).
# Must match the newest tagged release: bump when cutting releases.
readonly VERSION_FALLBACK="1.0.0-rc2"
# Release artifacts are hosted on the PUBLIC GitHub mirror — the Forgejo
# instance is internal (forward-auth gated) and cannot serve curl|bash.
readonly DOWNLOAD_BASE="https://github.com/Societus/monopsony/releases/download"
readonly RELEASES_API="https://api.github.com/repos/Societus/monopsony/releases/latest"
readonly BINARY_NAME="monopsony-server"
# Mutable: macOS re-points to /usr/local/bin in set_platform_paths()
INSTALL_DIR="/usr/bin"
readonly CONFIG_DIR="/etc/monopsony"
readonly CACHE_DIR="/var/cache/monopsony"
readonly LIB_DIR="/var/lib/monopsony"
readonly LOG_DIR="/var/log/monopsony"
readonly USER_NAME="monopsony"
readonly GROUP_NAME="monopsony"
readonly DB_NAME="monopsony"
readonly DB_USER="monopsony"
readonly PG_HBA_TEMPLATE="local monopsony monopsony    trust
host    monopsony monopsony 127.0.0.1/32  trust
host    monopsony monopsony ::1/128      trust"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

# ---------------------------------------------------------------------------
# Logging helpers
# ---------------------------------------------------------------------------
info()  { echo -e "${BLUE}[INFO]${NC}  $*"; }
ok()    { echo -e "${GREEN}[OK]${NC}    $*"; }
warn()  { echo -e "${YELLOW}[WARN]${NC}  $*"; }
error() { echo -e "${RED}[ERROR]${NC} $*" >&2; }

# Run command as postgres user (works with or without sudo)
run_as_postgres() {
    # Run a command as the postgres user. Works with or without sudo.
    if command -v sudo >/dev/null 2>&1; then
        sudo -u postgres "$@"
    else
        local cmd
        cmd=$(printf "%q " "$@")
        su -s /bin/sh postgres -c "$cmd"
    fi
}

# ---------------------------------------------------------------------------
# OS detection
# ---------------------------------------------------------------------------
detect_os() {
    # macOS has no /etc/os-release — check uname first
    if [[ "$(uname -s)" == "Darwin" ]]; then
        local mac_ver
        mac_ver="$(sw_vers -productVersion 2>/dev/null || echo "unknown")"
        echo "macos:${mac_ver}"
        return 0
    fi
    if [[ -f /etc/os-release ]]; then
        local _id _version_id
        _id=$(grep "^ID=" /etc/os-release | cut -d= -f2 | tr -d '"' | tr '[:upper:]' '[:lower:]')
        _version_id=$(grep "^VERSION_ID=" /etc/os-release | cut -d= -f2 | tr -d '"')
        echo "${_id}:${_version_id}"
    elif [[ -f /etc/arch-release ]]; then
        echo "arch:rolling"
    elif uname -s | grep -qi "freebsd"; then
        error "FreeBSD is not supported. Monopsony uses musl-static Linux binaries."
        error "Use a Linux jail/VM or check https://base.societus.club/societus/monopsony for native support."
        exit 1
    else
        error "Unsupported OS. Please install manually."
        exit 1
    fi
}

detect_arch() {
    local arch
    arch="$(uname -m)"
    case "${arch}" in
        x86_64)   echo "x86_64" ;;
        aarch64)  echo "aarch64" ;;
        arm64)    echo "aarch64" ;;  # some distros report arm64
        *)
            error "Unsupported architecture: ${arch}"
            exit 1
            ;;
    esac
}

# ---------------------------------------------------------------------------
# Platform paths
# ---------------------------------------------------------------------------
set_platform_paths() {
    if [[ "$(uname -s)" == "Darwin" ]]; then
        INSTALL_DIR="/usr/local/bin"
    fi
}

# ---------------------------------------------------------------------------
# Privilege check
# ---------------------------------------------------------------------------
require_root() {
    if [[ $EUID -ne 0 ]]; then
        error "This script must be run as root (use sudo)."
        exit 1
    fi
}

# ---------------------------------------------------------------------------
# Prerequisites check
# ---------------------------------------------------------------------------
check_prereqs() {
    local missing=()
    for cmd in curl openssl; do
        if ! command -v "${cmd}" &>/dev/null; then
            missing+=("${cmd}")
        fi
    done

    if [[ ${#missing[@]} -gt 0 ]]; then
        warn "Missing prerequisites: ${missing[*]}"
        warn "Installing..."
        local os_ver
        os_ver="$(detect_os | cut -d: -f1)"
        case "${os_ver}" in
            arch)
                pacman -Syu --noconfirm curl openssl 2>/dev/null || {
                    error "Failed to install prerequisites via pacman."
                    exit 1
                }
                ;;
            debian|ubuntu)
                apt-get update -qq && apt-get install -y -qq curl openssl 2>/dev/null || {
                    error "Failed to install prerequisites via apt."
                    exit 1
                }
                ;;
            rocky|almalinux|rhel|centos)
                dnf install -y curl openssl 2>/dev/null || {
                    error "Failed to install prerequisites via dnf."
                    exit 1
                }
                ;;
            alpine)
                apk add bash curl openssl 2>/dev/null || {
                    error "Failed to install prerequisites via apk."
                    exit 1
                }
                ;;
            opensuse-tumbleweed|opensuse-leap|sles)
                zypper -n in curl openssl 2>/dev/null || {
                    error "Failed to install prerequisites via zypper."
                    exit 1
                }
                ;;
            void)
                xbps-install -Sy curl openssl 2>/dev/null || {
                    error "Failed to install prerequisites via xbps-install."
                    exit 1
                }
                ;;
            gentoo)
                emerge --quiet net-misc/curl dev-libs/openssl 2>/dev/null || {
                    error "Failed to install prerequisites via emerge."
                    exit 1
                }
                ;;
            clear-linux-os)
                swupd bundle-add curl openssl 2>/dev/null || {
                    error "Failed to install prerequisites via swupd."
                    exit 1
                }
                ;;
            solus)
                eopkg it curl openssl 2>/dev/null || {
                    error "Failed to install prerequisites via eopkg."
                    exit 1
                }
                ;;
            nixos)
                # NixOS handles packages declaratively; no extra packages needed for client setup.
                info "NixOS detected — skipping package prerequisites."
                ;;
            macos)
                # macOS ships curl + openssl (LibreSSL) with the base system
                info "macOS detected — skipping package prerequisites."
                ;;
            *)
                error "Cannot auto-install prerequisites on this OS. Install curl and openssl manually."
                exit 1
                ;;
        esac
    fi
}

# ---------------------------------------------------------------------------
# Check if monopsony is already installed
# ---------------------------------------------------------------------------
is_installed() {
    [[ -f "${INSTALL_DIR}/${BINARY_NAME}" ]]
}

get_installed_version() {
    if is_installed; then
        "${INSTALL_DIR}/${BINARY_NAME}" --version 2>/dev/null | grep -oP '\d+\.\d+\.\d+' || echo "unknown"
    else
        echo "none"
    fi
}

# ---------------------------------------------------------------------------
# Version resolution (pairing-v2 Phase 0)
# ---------------------------------------------------------------------------
# Resolution order: MONOPSONY_VERSION env (also set by --version) >
# latest release tag from the GitHub API > VERSION_FALLBACK.
# Proven failure mode this fixes: a hardcoded VERSION fetches a nonexistent
# release on re-run (v0.1.0 404).
resolve_version() {
    if [[ -n "${MONOPSONY_VERSION:-}" ]]; then
        echo "${MONOPSONY_VERSION#v}"
        return 0
    fi

    local tag=""
    tag="$(curl -fsSL --retry 2 --max-time 15 "${RELEASES_API}" 2>/dev/null \
        | grep -o '"tag_name"[^,]*' | head -1 \
        | sed 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/')" || true
    if [[ -n "${tag}" ]]; then
        echo "${tag#v}"
        return 0
    fi

    warn "Could not resolve the latest release; falling back to v${VERSION_FALLBACK}."
    echo "${VERSION_FALLBACK}"
}

download_binary() {
    local kind="${1:-server}"   # server | client
    local arch os_ver
    arch="$(detect_arch)"
    os_ver="$(detect_os | cut -d: -f1)"

    # Map OS/arch to release asset name
    local asset_name os_type
    os_type="$(uname -s)"
    if [[ "${os_type}" == "Darwin" ]]; then
        if [[ "${arch}" == "aarch64" ]]; then
            asset_name="monopsony-${kind}-v${VERSION}-darwin-arm64.tar.gz"
        else
            asset_name="monopsony-${kind}-v${VERSION}-darwin-x86_64.tar.gz"
        fi
    else
        asset_name="monopsony-${kind}-v${VERSION}-linux-x86_64.tar.gz"
        if [[ "${arch}" == "aarch64" ]]; then
            asset_name="monopsony-${kind}-v${VERSION}-linux-aarch64.tar.gz"
        fi
    fi

    local url="${DOWNLOAD_BASE}/v${VERSION}/${asset_name}"
    local tmpdir
    tmpdir="$(mktemp -d)"

    info "Downloading ${asset_name}..."
    if ! curl -fsSL --retry 3 -o "${tmpdir}/${asset_name}" "${url}"; then
        error "Download failed: ${url}"
        rm -rf "${tmpdir}"
        exit 1
    fi

    # Verify checksum if available
    local sha_url="${DOWNLOAD_BASE}/v${VERSION}/SHA256SUMS"
    local expected_sha
    if curl -fsSL --retry 3 -o "${tmpdir}/SHA256SUMS" "${sha_url}"; then
        local expected
        expected="$(grep "${asset_name}" "${tmpdir}/SHA256SUMS" 2>/dev/null | awk '{print $1}')"
        if [[ -n "${expected}" ]]; then
            local actual
            actual="$(sha256sum "${tmpdir}/${asset_name}" | awk '{print $1}')"
            if [[ "${actual}" != "${expected}" ]]; then
                error "Checksum mismatch! Expected ${expected}, got ${actual}"
                rm -rf "${tmpdir}"
                exit 1
            fi
            ok "Checksum verified."
        else
            warn "SHA256SUMS file found but no entry for ${asset_name}. Skipping verification."
        fi

        # Verify GPG signature on SHA256SUMS if available
        local sig_url="${DOWNLOAD_BASE}/v${VERSION}/SHA256SUMS.sig"
        if curl -fsSL --retry 3 -o "${tmpdir}/SHA256SUMS.sig" "${sig_url}"; then
            if command -v gpg >/dev/null 2>&1; then
                # Import Monopsony release signing key
                local gpg_key_url="https://monopsony.dev/gpg/monopsony-release.asc"
                if curl -fsSL --retry 3 -o "${tmpdir}/release.asc" "${gpg_key_url}"; then
                    gpg --batch --no-autostart --import "${tmpdir}/release.asc" 2>/dev/null || true
                    if gpg --batch --no-autostart --verify "${tmpdir}/SHA256SUMS.sig" "${tmpdir}/SHA256SUMS" 2>/dev/null; then
                        ok "SHA256SUMS GPG signature verified."
                    else
                        warn "SHA256SUMS GPG signature verification failed. Proceeding with checksum-only verification."
                    fi
                else
                    warn "Could not download release signing key. Skipping GPG verification."
                fi
            else
                warn "gpg not found. Skipping SHA256SUMS signature verification."
            fi
        else
            warn "SHA256SUMS.sig not available. Skipping GPG verification."
        fi
    else
        warn "SHA256SUMS not available. Skipping checksum verification."
    fi

    # Extract and install
    info "Extracting and installing..."
    tar xzf "${tmpdir}/${asset_name}" -C "${tmpdir}/"
    local bin_name="monopsony-${kind}"
    # /usr/local/bin does not exist by default on Apple Silicon — create it
    mkdir -p "${INSTALL_DIR}"
    mv "${tmpdir}/${bin_name}" "${INSTALL_DIR}/${bin_name}"
    chmod 755 "${INSTALL_DIR}/${bin_name}"
    rm -rf "${tmpdir}"
    ok "Binary installed to ${INSTALL_DIR}/${bin_name}"
}

# ---------------------------------------------------------------------------
# Create user, groups, directories — macOS (dscl)
# ---------------------------------------------------------------------------
next_free_dscl_id() {
    # $1 = /Users or /Groups, $2 = UniqueID or PrimaryGroupID; scans 300-399
    local node="$1" key="$2" id
    for id in $(seq 300 399); do
        if [[ -z "$(dscl . -search "${node}" "${key}" "${id}" 2>/dev/null || true)" ]]; then
            echo "${id}"
            return 0
        fi
    done
    echo ""
}

setup_user_and_dirs_macos() {
    if ! dscl . -read /Groups/monopsony >/dev/null 2>&1; then
        local gid
        gid="$(next_free_dscl_id /Groups PrimaryGroupID)"
        if [[ -z "${gid}" ]]; then
            error "No free GID in 300-399 range."
            exit 1
        fi
        dscl . -create /Groups/monopsony PrimaryGroupID "${gid}"
        dscl . -create /Groups/monopsony RealName "Monopsony"
        ok "Created group monopsony (gid ${gid})"
    fi
    if ! dscl . -read /Users/monopsony >/dev/null 2>&1; then
        local uid gid
        uid="$(next_free_dscl_id /Users UniqueID)"
        gid="$(dscl . -read /Groups/monopsony PrimaryGroupID 2>/dev/null | awk '{print $2}')"
        if [[ -z "${uid}" ]]; then
            error "No free UID in 300-399 range."
            exit 1
        fi
        dscl . -create /Users/monopsony UniqueID "${uid}"
        dscl . -create /Users/monopsony PrimaryGroupID "${gid}"
        dscl . -create /Users/monopsony RealName "Monopsony Server"
        dscl . -create /Users/monopsony UserShell /usr/bin/false
        dscl . -create /Users/monopsony NFSHomeDirectory /var/lib/monopsony
        ok "Created user monopsony (uid ${uid})"
    fi

    for dir in "${CONFIG_DIR}" "${CACHE_DIR}" "${LIB_DIR}" "${LOG_DIR}"; do
        mkdir -p "${dir}"
        chown monopsony:monopsony "${dir}" 2>/dev/null || true
        chmod 750 "${dir}" 2>/dev/null || true
    done
}

# ---------------------------------------------------------------------------
# Create user, groups, directories
# ---------------------------------------------------------------------------
setup_user_and_dirs() {
    local os_ver
    os_ver="$(detect_os | cut -d: -f1)"

    if [[ "${os_ver}" == "macos" ]]; then
        setup_user_and_dirs_macos
        return 0
    fi

    # Create group if it doesn't exist
    if ! getent group "${GROUP_NAME}" &>/dev/null; then
        case "${os_ver}" in
            alpine) addgroup "${GROUP_NAME}" ;;
            *)      groupadd "${GROUP_NAME}" ;;
        esac
        ok "Created group ${GROUP_NAME}"
    fi

    # Create user if it doesn't exist
    if ! id "${USER_NAME}" &>/dev/null; then
        case "${os_ver}" in
            alpine) adduser -S -G "${GROUP_NAME}" -s /sbin/nologin -h "${LIB_DIR}" -D "${USER_NAME}" ;;
            *)      useradd -r -g "${GROUP_NAME}" -s /usr/sbin/nologin -d "${LIB_DIR}" "${USER_NAME}" ;;
        esac
        ok "Created user ${USER_NAME}"
    fi

    # Create directories
    for dir in "${CONFIG_DIR}" "${CACHE_DIR}" "${LIB_DIR}" "${LOG_DIR}"; do
        if [[ ! -d "${dir}" ]]; then
            mkdir -p "${dir}"
            chown "${USER_NAME}:${GROUP_NAME}" "${dir}"
            chmod 750 "${dir}"
            ok "Created ${dir}"
        else
            chown "${USER_NAME}:${GROUP_NAME}" "${dir}"
        fi
    done

    # Create log subdirectory
    if [[ ! -d "${LOG_DIR}" ]]; then
        mkdir -p "${LOG_DIR}"
        chown "${USER_NAME}:${GROUP_NAME}" "${LOG_DIR}"
        ok "Created ${LOG_DIR}"
    fi
}

# ---------------------------------------------------------------------------
# Generate config
# ---------------------------------------------------------------------------
generate_config() {
    local config_file="${CONFIG_DIR}/server.toml"

    if [[ -f "${config_file}" ]]; then
        warn "Config already exists at ${config_file}. Skipping generation."
        warn "To reconfigure, run: $0 --reconfigure"
        return 0
    fi

    info "Generating default config at ${config_file}..."
    cp "${CONFIG_DIR}/server.toml.example" "${config_file}" 2>/dev/null || true

    # If no example file, generate from template
    if [[ ! -f "${config_file}" ]]; then
        cat > "${config_file}" <<'TOML'
# Monopsony Server Configuration — auto-generated by installer

[server]
bind = "0.0.0.0:3128"

[grpc]
bind = "0.0.0.0:50051"
# ws_url = "wss://monopsony.example.com/ws/tunnel"  # Set for Cloudflare Tunnel mode

[database]
url = "postgres://monopsony:CHANGE_ME@localhost/monopsony"
max_connections = 10

[cache]
path = "/var/cache/monopsony"
max_size_gb = 50

[tls]
ca_path = "/var/lib/monopsony/ca.pem"
ca_key_path = "/var/lib/monopsony/ca.key"
server_cert_path = "/var/lib/monopsony/server.pem"
server_key_path = "/var/lib/monopsony/server.key"
# san_names = ["monopsony.example.com"]  # Add your public hostname for CF Tunnel mode

[discovery]
mdns_enabled = true
mdns_service_name = "monopsony-server"

[metrics]
enabled = true
bind = "0.0.0.0:9090"
path = "/metrics"

[logging]
level = "info"
format = "json"
TOML
    fi

    chown "${USER_NAME}:${GROUP_NAME}" "${config_file}"
    chmod 640 "${config_file}"
    ok "Config generated at ${config_file}"
}

# ---------------------------------------------------------------------------
# PostgreSQL setup — local, macOS (Homebrew)
# ---------------------------------------------------------------------------
setup_local_postgres_macos() {
    info "Setting up local PostgreSQL via Homebrew..."

    local brew
    brew="$(command -v brew 2>/dev/null || true)"
    if [[ -z "${brew}" && -x /opt/homebrew/bin/brew ]]; then brew=/opt/homebrew/bin/brew; fi
    if [[ -z "${brew}" && -x /usr/local/bin/brew ]]; then brew=/usr/local/bin/brew; fi
    if [[ -z "${brew}" ]]; then
        error "Homebrew not found."
        error "Install Homebrew (https://brew.sh), then: brew install postgresql@16"
        error "Re-run this installer with --reconfigure afterwards."
        return 1
    fi

    if ! "${brew}" list --versions postgresql@16 >/dev/null 2>&1; then
        info "Installing postgresql@16 via brew..."
        "${brew}" install postgresql@16 || { error "brew install postgresql@16 failed."; return 1; }
    fi

    "${brew}" services start postgresql@16 2>/dev/null || { error "brew services start postgresql@16 failed."; return 1; }

    local pgbin
    pgbin="$("${brew}" --prefix postgresql@16)/bin"

    # Homebrew's PostgreSQL trusts the local admin user — no run_as_postgres needed
    if ! "${pgbin}/psql" -d postgres -tAc "SELECT 1 FROM pg_roles WHERE rolname='${DB_USER}'" 2>/dev/null | grep -q 1; then
        info "Creating role '${DB_USER}' and database '${DB_NAME}'..."
        local password
        password="$(openssl rand -base64 24)"
        "${pgbin}/psql" -d postgres -c "CREATE USER ${DB_USER} WITH PASSWORD '${password}';" || return 1
        "${pgbin}/psql" -d postgres -c "CREATE DATABASE ${DB_NAME} OWNER ${DB_USER};" || return 1
        "${pgbin}/psql" -d "${DB_NAME}" -c "GRANT ALL ON SCHEMA public TO ${DB_USER};" 2>/dev/null || true
        update_config_db_url "${password}"
        ok "PostgreSQL role + database created."
    else
        info "Role '${DB_USER}' already exists. Skipping."
    fi
}

# ---------------------------------------------------------------------------
# PostgreSQL setup — local
# ---------------------------------------------------------------------------
setup_local_postgres() {
    info "Setting up local PostgreSQL..."

    if [[ "$(uname -s)" == "Darwin" ]]; then
        setup_local_postgres_macos
        return 0
    fi

    # Check if postgres is installed
    if ! command -v psql &>/dev/null; then
        warn "PostgreSQL not found. Installing..."
        local os_ver
        os_ver="$(detect_os | cut -d: -f1)"
        case "${os_ver}" in
            arch)
                pacman -S --noconfirm postgresql 2>/dev/null || {
                    error "Failed to install PostgreSQL via pacman."
                    exit 1
                }
                ;;
            debian|ubuntu)
                apt-get install -y -qq postgresql postgresql-client 2>/dev/null || {
                    error "Failed to install PostgreSQL via apt."
                    exit 1
                }
                ;;
            rocky|almalinux|rhel|centos)
                dnf install -y postgresql-server 2>/dev/null || {
                    error "Failed to install PostgreSQL via dnf."
                    exit 1
                }
                # RHEL-family needs initdb before first start
                if [[ ! -d /var/lib/pgsql/data ]] || [[ -z "$(ls -A /var/lib/pgsql/data 2>/dev/null)" ]]; then
                    postgresql-setup --initdb 2>/dev/null || {
                        error "Failed to initialize PostgreSQL database."
                        exit 1
                    }
                fi
                ;;
            alpine)
                apk add postgresql 2>/dev/null || {
                    error "Failed to install PostgreSQL via apk."
                    exit 1
                }
                # Alpine requires initdb before first start
                if [[ ! -d /var/lib/postgresql/data ]] || [[ -z "$(ls -A /var/lib/postgresql/data 2>/dev/null)" ]]; then
                    su -s /bin/sh postgres -c "initdb --locale=C.UTF-8 --encoding=UTF8 -D '/var/lib/postgresql/data'" || {
                        error "Failed to initialize PostgreSQL database cluster."
                        exit 1
                    }
                fi
                ;;
            opensuse-tumbleweed|opensuse-leap|sles)
                zypper -n in postgresql-server 2>/dev/null || {
                    error "Failed to install PostgreSQL via zypper."
                    exit 1
                }
                # openSUSE needs initdb before first start
                if [[ ! -d /var/lib/pgsql/data ]] || [[ -z "$(ls -A /var/lib/pgsql/data 2>/dev/null)" ]]; then
                    su -s /bin/sh postgres -c "initdb --locale=C.UTF-8 --encoding=UTF8 -D '/var/lib/pgsql/data'" || {
                        error "Failed to initialize PostgreSQL database."
                        exit 1
                    }
                fi
                ;;
            void)
                xbps-install -Sy postgresql 2>/dev/null || {
                    error "Failed to install PostgreSQL via xbps-install."
                    exit 1
                }
                # Void needs initdb before first start
                if [[ ! -d /var/lib/postgresql/data ]] || [[ -z "$(ls -A /var/lib/postgresql/data 2>/dev/null)" ]]; then
                    su -s /bin/sh postgres -c "initdb --locale=C.UTF-8 --encoding=UTF8 -D '/var/lib/postgresql/data'" || {
                        error "Failed to initialize PostgreSQL database cluster."
                        exit 1
                    }
                fi
                ;;
            gentoo)
                emerge --quiet dev-db/postgresql 2>/dev/null || {
                    error "Failed to install PostgreSQL via emerge."
                    exit 1
                }
                # Gentoo needs initdb before first start
                if [[ ! -d /var/lib/postgresql/data ]] || [[ -z "$(ls -A /var/lib/postgresql/data 2>/dev/null)" ]]; then
                    su -s /bin/sh postgres -c "initdb --locale=C.UTF-8 --encoding=UTF8 -D '/var/lib/postgresql/data'" || {
                        error "Failed to initialize PostgreSQL database cluster."
                        exit 1
                    }
                fi
                ;;
            clear-linux-os)
                swupd bundle-add postgresql 2>/dev/null || {
                    error "Failed to install PostgreSQL via swupd."
                    exit 1
                }
                ;;
            solus)
                eopkg it postgresql 2>/dev/null || {
                    error "Failed to install PostgreSQL via eopkg."
                    exit 1
                }
                ;;
            *)
                error "Cannot auto-install PostgreSQL on this OS."
                exit 1
                ;;
        esac
    fi

    # Start PostgreSQL
    local os_ver
    os_ver="$(detect_os | cut -d: -f1)"
    case "${os_ver}" in
        arch)
            if systemctl is-active --quiet postgresql; then
                info "PostgreSQL already running."
            else
                # Arch requires initdb before first start
                if [[ ! -d /var/lib/postgres/data ]] || [[ -z "$(ls -A /var/lib/postgres/data 2>/dev/null)" ]]; then
                    info "Initializing PostgreSQL database cluster..."
                    su -s /bin/sh postgres -c "initdb --locale=C.UTF-8 --encoding=UTF8 -D '/var/lib/postgres/data'" || {
                        error "Failed to initialize PostgreSQL database cluster."
                        exit 1
                    }
                fi
                systemctl start postgresql
                systemctl enable postgresql
                ok "Started and enabled PostgreSQL."
            fi
            ;;
        debian|ubuntu)
            if systemctl is-active --quiet postgresql; then
                info "PostgreSQL already running."
            else
                systemctl start postgresql
                systemctl enable postgresql
                ok "Started and enabled PostgreSQL."
            fi
            ;;
        rocky|almalinux|rhel|centos)
            if systemctl is-active --quiet postgresql; then
                info "PostgreSQL already running."
            else
                systemctl start postgresql
                systemctl enable postgresql
                ok "Started and enabled PostgreSQL."
            fi
            ;;
        alpine)
            if rc-service postgresql status >/dev/null 2>&1; then
                info "PostgreSQL already running."
            else
                rc-service postgresql start
                rc-update add postgresql default
                ok "Started and enabled PostgreSQL."
            fi
            ;;
        void)
            # Void uses runit
            if sv status postgresql >/dev/null 2>&1; then
                info "PostgreSQL already running."
            else
                ln -sf /etc/sv/postgresql /var/service/ 2>/dev/null
                sv start postgresql 2>/dev/null
                ok "Started and enabled PostgreSQL."
            fi
            ;;
        gentoo)
            # Gentoo can use systemd or OpenRC
            if command -v systemctl >/dev/null 2>&1; then
                if systemctl is-active --quiet postgresql; then
                    info "PostgreSQL already running."
                else
                    systemctl start postgresql
                    systemctl enable postgresql
                    ok "Started and enabled PostgreSQL."
                fi
            else
                if rc-service postgresql status >/dev/null 2>&1; then
                    info "PostgreSQL already running."
                else
                    rc-service postgresql start
                    rc-update add postgresql default
                    ok "Started and enabled PostgreSQL."
                fi
            fi
            ;;
        opensuse-tumbleweed|opensuse-leap|sles|clear-linux-os|solus)
            # systemd-based distros
            if systemctl is-active --quiet postgresql; then
                info "PostgreSQL already running."
            else
                systemctl start postgresql
                systemctl enable postgresql
                ok "Started and enabled PostgreSQL."
            fi
            ;;
    esac

    # Check if DB/user already exist
    if run_as_postgres psql -tAc "SELECT 1 FROM pg_database WHERE datname='${DB_NAME}'" 2>/dev/null | grep -q 1; then
        info "Database '${DB_NAME}' already exists. Skipping."
    else
        info "Creating database '${DB_NAME}'..."
        run_as_postgres createdb "${DB_NAME}" || {
            error "Failed to create database '${DB_NAME}'."
            exit 1
        }
        ok "Database '${DB_NAME}' created."
    fi

    # Check if user exists
    if run_as_postgres psql -tAc "SELECT 1 FROM pg_roles WHERE rolname='${DB_USER}'" 2>/dev/null | grep -q 1; then
        info "User '${DB_USER}' already exists. Skipping."
    else
        info "Creating user '${DB_USER}'..."
        local password
        password="$(openssl rand -base64 24)"
        run_as_postgres createuser --createdb "${DB_USER}" || {
            error "Failed to create user '${DB_USER}'."
            exit 1
        }
        run_as_postgres psql -c "ALTER USER ${DB_USER} WITH PASSWORD '${password}';" || {
            error "Failed to set password for '${DB_USER}'."
            exit 1
        }
        run_as_postgres psql -c "GRANT ALL PRIVILEGES ON DATABASE monopsony TO monopsony;" || {
            error "Failed to grant privileges."
            exit 1
        }
        # PostgreSQL 15+ revokes CREATE on schema public by default
        run_as_postgres psql -d monopsony -c "GRANT ALL ON SCHEMA public TO monopsony;" || {
            warn "Failed to grant schema public permissions (may need manual fix)."
        }
        info "User '${DB_USER}' created with random password."
        # Update config with the password
        update_config_db_url "${password}"
    fi

    # Update pg_hba.conf if needed
    update_pg_hba
}

update_config_db_url() {
    local password="$1"
    local config_file="${CONFIG_DIR}/server.toml"

    if [[ -f "${config_file}" ]]; then
        # Replace the CHANGE_ME placeholder with the actual password.
        # We use CHANGE_ME (not ***) because GNU sed has a known parser bug
        # where literal *** inside URL-like strings fails to match.
        # Escape sed-special characters in the password.
        local escaped_pw
        escaped_pw=$(printf '%s' "${password}" | sed 's/[&|/]/\\&/g')
        sed -i "s|CHANGE_ME|${escaped_pw}|" "${config_file}"
        ok "Updated database URL in config."
    fi
}

update_pg_hba() {
    local pg_hba_file
    # Try to find pg_hba.conf
    pg_hba_file="$(run_as_postgres psql -tAc "SHOW hba_file" 2>/dev/null | tr -d ' ')" || true

    if [[ -z "${pg_hba_file}" || ! -f "${pg_hba_file}" ]]; then
        # Common locations
        for candidate in /etc/postgresql/*/main/pg_hba.conf /var/lib/postgresql/*/main/pg_hba.conf /var/lib/pgsql/data/pg_hba.conf /var/lib/postgresql/data/pg_hba.conf /var/lib/postgresql/*/data/pg_hba.conf; do
            if [[ -f "${candidate}" ]]; then
                pg_hba_file="${candidate}"
                break
            fi
        done
    fi

    if [[ -n "${pg_hba_file}" && -f "${pg_hba_file}" ]]; then
        if ! grep -q "monopsony" "${pg_hba_file}" 2>/dev/null; then
            warn "Adding monopsony entries to pg_hba.conf (${pg_hba_file})..."
            cp "${pg_hba_file}" "${pg_hba_file}.bak"
            # Insert monopsony rules BEFORE the generic "host all all" rules
            # so they take priority (pg_hba.conf is processed top-down)
            sed -i "/^host.*all.*all.*127\.0\.0\.1/i host    monopsony monopsony 127.0.0.1/32  trust" "${pg_hba_file}"
            sed -i "/^host.*all.*all.*::1/i host    monopsony monopsony ::1/128      trust" "${pg_hba_file}"
            sed -i "/^local.*all.*all/i local   monopsony       monopsony                               trust" "${pg_hba_file}"
            ok "pg_hba.conf updated (monopsony rules inserted before generic rules)."
        else
            info "pg_hba.conf already has monopsony entries."
        fi
    else
        warn "Could not locate pg_hba.conf. Manual configuration may be needed."
        echo ""
        echo "If you get 'connection refused' errors, add these to pg_hba.conf:"
        echo "${PG_HBA_TEMPLATE}"
        echo ""
    fi
}

# ---------------------------------------------------------------------------
# PostgreSQL setup — external
# ---------------------------------------------------------------------------
setup_external_postgres() {
    info "Configuring external PostgreSQL..."

    local host port admin_user
    host="$(read_line "PostgreSQL host (default: localhost): " "localhost")"
    port="$(read_line "PostgreSQL port (default: 5432): " "5432")"
    admin_user="$(read_line "PostgreSQL admin username (default: postgres): " "postgres")"

    # Test connectivity
    info "Testing connection to ${host}:${port}..."
    if command -v psql &>/dev/null; then
        if psql -h "${host}" -p "${port}" -U "${admin_user}" -c "SELECT 1" &>/dev/null; then
            ok "Connection successful."
        else
            warn "psql connection failed. Trying raw TCP..."
            if (echo > /dev/tcp/"${host}"/"${port}") 2>/dev/null; then
                ok "TCP connection successful. psql auth may need pg_hba.conf update."
            else
                error "Cannot reach ${host}:${port}. Check firewall and PostgreSQL config."
                exit 1
            fi
        fi
    else
        warn "psql not installed. Skipping connectivity test."
        if ! (echo > /dev/tcp/"${host}"/"${port}") 2>/dev/null; then
            error "Cannot reach ${host}:${port}."
            exit 1
        fi
    fi

    # Generate SQL commands
    local password
    password="$(openssl rand -base64 24)"

    echo ""
    echo "=========================================="
    echo "  Run these SQL commands as the admin user:"
    echo "=========================================="
    echo ""
    echo "CREATE USER monopsony WITH PASSWORD '${password}';"
    echo "CREATE DATABASE monopsony OWNER monopsony;"
    echo "GRANT ALL PRIVILEGES ON DATABASE monopsony TO monopsony;"
    echo ""
    echo "Then update /etc/monopsony/server.toml:"
    echo "  url = \"postgres://monopsony:CHANGE_ME@${host}:${port}/monopsony\""
    echo ""
    echo "Also ensure pg_hba.conf allows connections from this server."
    echo "And postgresql.conf has: listen_addresses = '*'"
    echo ""

    # Update config
    update_config_db_url_ext "${password}" "${host}" "${port}"
}

update_config_db_url_ext() {
    local password="$1" host="$2" port="$3"
    local config_file="${CONFIG_DIR}/server.toml"

    if [[ -f "${config_file}" ]]; then
        # Replace the CHANGE_ME placeholder and host/port with actual values.
        # Use CHANGE_ME (not ***) because GNU sed can't reliably match ***
        # in URL-like contexts.
        local escaped_password
        escaped_password="$(printf '%s' "${password}" | sed 's/[&|/]/\\&/g')"
        local escaped_host
        escaped_host="$(printf '%s' "${host}" | sed 's/[&|/]/\\&/g')"
        sed -i "s|postgres://monopsony:CHANGE_ME@localhost/monopsony|postgres://monopsony:${escaped_password}@${escaped_host}:${port}/monopsony|" "${config_file}"
        ok "Updated database URL in config."
    fi
}

# ---------------------------------------------------------------------------
# TLS CA bootstrap
# ---------------------------------------------------------------------------
bootstrap_tls() {
    info "Setting up TLS certificates..."

    local ca_path="${LIB_DIR}/ca.pem"
    local ca_key_path="${LIB_DIR}/ca.key"
    local server_cert_path="${LIB_DIR}/server.pem"
    local server_key_path="${LIB_DIR}/server.key"

    # Check for existing CA
    if [[ -f "${ca_path}" && -f "${ca_key_path}" ]]; then
        info "Existing CA found. Skipping CA generation."
        # Still generate server cert if missing
        if [[ ! -f "${server_cert_path}" || ! -f "${server_key_path}" ]]; then
            generate_server_cert "${ca_path}" "${ca_key_path}"
        fi
        return 0
    fi

    # Generate CA
    info "Generating self-signed CA..."
    openssl genrsa -out "${ca_key_path}" 4096 2>/dev/null
    openssl req -new -x509 -days 3650 -key "${ca_key_path}" \
        -out "${ca_path}" \
        -subj "/C=US/O=Monopsony/CN=Monopsony Internal CA" 2>/dev/null
    ok "CA generated at ${ca_path}"

    # Generate server cert
    generate_server_cert "${ca_path}" "${ca_key_path}"

    # Set permissions
    chmod 600 "${ca_key_path}" "${server_key_path}"
    chmod 644 "${ca_path}" "${server_cert_path}"
    chown "${USER_NAME}:${GROUP_NAME}" "${ca_path}" "${ca_key_path}" "${server_cert_path}" "${server_key_path}"

    # Print fingerprint for manual verification
    local fingerprint
    fingerprint="$(openssl x509 -in "${ca_path}" -noout -fingerprint -sha256 2>/dev/null | cut -d= -f2)"
    echo ""
    echo "CA fingerprint (SHA-256): ${fingerprint}"
    echo "Store this to verify client connections."
    echo ""
}

generate_server_cert() {
    local ca_path="$1" ca_key_path="$2"

    # Get hostname
    local hostname
    hostname="$(hostname -f 2>/dev/null || hostname || echo "localhost")"

    # Generate key
    openssl genrsa -out "${LIB_DIR}/server.key" 2048 2>/dev/null

    # Generate CSR
    openssl req -new -key "${LIB_DIR}/server.key" \
        -out "${LIB_DIR}/server.csr" \
        -subj "/C=US/O=Monopsony/CN=${hostname}" 2>/dev/null

    # Create SAN config
    local san_config="/tmp/monopsony-san.cnf"
    cat > "${san_config}" <<EOF
[req]
distinguished_name = req_dn
req_extensions = v3_req

[req_dn]
CN = ${hostname}

[v3_req]
subjectAltName = @alt_names

[alt_names]
DNS.1 = ${hostname}
DNS.2 = localhost
IP.1 = 127.0.0.1
EOF

    # Sign with CA
    openssl x509 -req -in "${LIB_DIR}/server.csr" \
        -CA "${ca_path}" -CAkey "${ca_key_path}" -CAcreateserial \
        -out "${LIB_DIR}/server.pem" \
        -days 365 \
        -extfile "${san_config}" \
        -extensions v3_req 2>/dev/null

    rm -f "${LIB_DIR}/server.csr" "${san_config}"
    ok "Server cert generated, signed by Monopsony CA."
}

# ---------------------------------------------------------------------------
# Init system detection
# ---------------------------------------------------------------------------
# Prints one of: launchd systemd openrc runit s6 sysvinit container unknown
# Detection is capability-based (PID 1 identity, marker files, command probes),
# NOT keyed on distro name. Pin explicitly with MONOPSONY_INIT_SYSTEM=<name>
# for testing or unusual setups.
detect_init() {
    if [[ -n "${MONOPSONY_INIT_SYSTEM:-}" ]]; then
        echo "${MONOPSONY_INIT_SYSTEM}"
        return 0
    fi

    local uname_s pid1
    uname_s="$(uname -s)"
    pid1=""
    if [[ -r /proc/1/comm ]]; then
        pid1="$(tr -d '[:space:]' < /proc/1/comm 2>/dev/null || true)"
    fi

    # macOS — launchd is the only option
    if [[ "${uname_s}" == "Darwin" ]]; then
        echo "launchd"
        return 0
    fi

    # systemd — strongest signals first
    if [[ -d /run/systemd/system ]] || [[ "${pid1}" == "systemd" ]]; then
        echo "systemd"
        return 0
    fi

    # s6 — STRONG signals first: s6-svscan as PID 1, or a live s6-rc
    # database. Must outrank OpenRC's weakest heuristic (rc-service
    # installed + classic init as PID 1): a Gentoo box converted to s6
    # supervision keeps OpenRC's early-boot scripts around.
    if [[ "${pid1}" == "s6-svscan" ]] || [[ -d /run/s6-rc ]]; then
        echo "s6"
        return 0
    fi

    # OpenRC — running softlevel marker, openrc-init as PID 1,
    # or rc-service present with a classic init PID 1
    if [[ -f /run/openrc/softlevel ]] || [[ "${pid1}" == "openrc-init" ]] \
       || { command -v rc-service >/dev/null 2>&1 && [[ "${pid1}" == "init" ]]; }; then
        echo "openrc"
        return 0
    fi

    # runit — Void/Artix run runit (or runsvdir) as PID 1
    if [[ "${pid1}" == "runit" ]] || [[ "${pid1}" == "runsvdir" ]] \
       || { command -v sv >/dev/null 2>&1 && { [[ -d /var/service ]] || [[ -d /service ]]; }; }; then
        echo "runit"
        return 0
    fi

    # s6 — weak signal: toolkit installed but not confirmed running
    if command -v s6-sv >/dev/null 2>&1; then
        echo "s6"
        return 0
    fi

    # Container without an init system (Docker/Podman)
    if [[ -f /.dockerenv ]] || [[ -f /run/.containerenv ]]; then
        echo "container"
        return 0
    fi

    # Legacy SysV init.d fallback
    if [[ -d /etc/init.d ]]; then
        echo "sysvinit"
        return 0
    fi

    echo "unknown"
}

# ---------------------------------------------------------------------------
# Service templates
# ---------------------------------------------------------------------------
# Canonical copies live in dist/init/ next to this script; the embedded copies
# below keep the curl|bash one-liner self-contained. They MUST stay in sync:
#   bash dist/install.sh --check-templates     (also enforced in CI)
#   bash dist/install.sh --emit-templates DIR  (regenerate dist/init from here)
# ---------------------------------------------------------------------------
TEMPLATE_FILES=(
    monopsony-server.service
    monopsony-client.service
    monopsony-server.openrc
    monopsony-server.runit
    monopsony-server.runit-log
    monopsony-server.s6
    monopsony-server.sysv
    com.monopsony.server.plist
)

template_dir() {
    # Prefer templates shipped next to this script (repo checkout), then the
    # system-installed copy. Empty string means: use embedded copies.
    local script_dir
    script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
    if [[ -f "${script_dir}/init/monopsony-server.service" ]]; then
        echo "${script_dir}/init"
        return 0
    fi
    if [[ -f /usr/share/monopsony/dist/init/monopsony-server.service ]]; then
        echo "/usr/share/monopsony/dist/init"
        return 0
    fi
    echo ""
}

template_embedded() {
    # $1 = template name; prints the embedded fallback copy on stdout.
    # MUST match dist/init/<name> byte-for-byte (verified by --check-templates).
    case "$1" in
        monopsony-server.service)
            cat <<'TEMPLATE_EOF'
[Unit]
Description=Monopsony Caching Proxy Server
Documentation=https://base.societus.club/societus/monopsony
After=network-online.target postgresql.service
Wants=network-online.target

[Service]
Type=simple
ExecStart=@INSTALL_DIR@/monopsony-server --config /etc/monopsony/server.toml @EXTRA_ARGS@
ExecReload=/bin/kill -HUP $MAINPID
Restart=on-failure
RestartSec=5

# Security hardening
LimitNOFILE=65536
ReadWritePaths=/var/cache/monopsony /var/lib/monopsony
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true

# User/group
User=monopsony
Group=monopsony

# Environment
Environment=RUST_LOG=info
Environment=RUST_BACKTRACE=1

[Install]
WantedBy=multi-user.target
TEMPLATE_EOF
            ;;
        monopsony-client.service)
            cat <<'TEMPLATE_EOF'
[Unit]
Description=Monopsony Client Daemon
Documentation=https://base.societus.club/societus/monopsony
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=/usr/bin/monopsony-client daemon --config /etc/monopsony/client.toml
Restart=on-failure
RestartSec=5
StartLimitBurst=5
StartLimitIntervalSec=60

# Security hardening
LimitNOFILE=8192
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=read-only
PrivateTmp=true

# User — must have read access to cert dir (~/.config/monopsony)
User=root
Group=root

# Environment
Environment=RUST_LOG=info

[Install]
WantedBy=multi-user.target
TEMPLATE_EOF
            ;;
        monopsony-server.openrc)
            cat <<'TEMPLATE_EOF'
#!/sbin/openrc-run
# Monopsony Caching Proxy Server — OpenRC init script
# For Alpine, Artix, and Gentoo systems without systemd

description="Monopsony caching proxy server"
command="@INSTALL_DIR@/monopsony-server"
command_args="--config /etc/monopsony/server.toml @EXTRA_ARGS@"
command_user="monopsony:monopsony"

pidfile="/run/${RC_SVCNAME}.pid"
command_background=true
start_stop_daemon_args="--make-pidfile"

output_log="/var/log/monopsony/server.log"
error_log="/var/log/monopsony/server-error.log"

depend() {
    need net
    after postgresql
    use dns logger
}

start_pre() {
    checkpath --directory --owner monopsony:monopsony --mode 0755 \
        /var/cache/monopsony /var/lib/monopsony /var/log/monopsony
}
TEMPLATE_EOF
            ;;
        monopsony-server.runit)
            cat <<'TEMPLATE_EOF'
#!/bin/sh
# Monopsony Caching Proxy Server — runit service (run script)
# Install to: /etc/sv/monopsony-server/run  (enable: ln -s /etc/sv/monopsony-server /var/service/)
exec chpst -u monopsony:monopsony \
    @INSTALL_DIR@/monopsony-server --config /etc/monopsony/server.toml @EXTRA_ARGS@ 2>&1
TEMPLATE_EOF
            ;;
        monopsony-server.runit-log)
            cat <<'TEMPLATE_EOF'
#!/bin/sh
# Monopsony Caching Proxy Server — runit log service
# Install to: /etc/sv/monopsony-server/log/run
exec chpst -u monopsony:monopsony svlogd -tt /var/log/monopsony
TEMPLATE_EOF
            ;;
        monopsony-server.s6)
            cat <<'TEMPLATE_EOF'
#!/bin/sh
# Monopsony Caching Proxy Server — s6 longrun service
# Install to: /etc/s6/sv/monopsony-server/run  (requires a sibling `type` file containing "longrun")
exec s6-setuidgid monopsony \
    @INSTALL_DIR@/monopsony-server --config /etc/monopsony/server.toml @EXTRA_ARGS@ 2>&1
TEMPLATE_EOF
            ;;
        monopsony-server.sysv)
            cat <<'TEMPLATE_EOF'
#!/bin/sh
### BEGIN INIT INFO
# Provides:          monopsony-server
# Required-Start:    $network $remote_fs postgresql
# Required-Stop:     $network $remote_fs
# Default-Start:     2 3 4 5
# Default-Stop:      0 1 6
# Short-Description: Monopsony caching proxy server
# Description:       Centralized package caching proxy for LAN environments.
### END INIT INFO

NAME="monopsony-server"
DAEMON="@INSTALL_DIR@/monopsony-server"
DAEMON_ARGS="--config /etc/monopsony/server.toml @EXTRA_ARGS@"
DAEMON_USER="monopsony"
DAEMON_GROUP="monopsony"
PIDFILE="/run/${NAME}.pid"
LOGDIR="/var/log/monopsony"

SSD="/usr/sbin/start-stop-daemon"
[ -x "${SSD}" ] || SSD="start-stop-daemon"

is_running() {
    [ -f "${PIDFILE}" ] && kill -0 "$(cat "${PIDFILE}" 2>/dev/null)" 2>/dev/null
}

do_start() {
    if is_running; then
        echo "${NAME} already running."
        return 0
    fi
    mkdir -p "${LOGDIR}" /var/cache/monopsony /var/lib/monopsony 2>/dev/null || true
    chown "${DAEMON_USER}:${DAEMON_GROUP}" "${LOGDIR}" /var/cache/monopsony /var/lib/monopsony 2>/dev/null || true
    "${SSD}" --start --background --make-pidfile --pidfile "${PIDFILE}" \
        --chuid "${DAEMON_USER}:${DAEMON_GROUP}" \
        --exec /bin/sh -- -c "exec ${DAEMON} ${DAEMON_ARGS} >> ${LOGDIR}/server.log 2>&1"
    echo "${NAME} started."
}

do_stop() {
    if ! is_running; then
        echo "${NAME} not running."
        return 0
    fi
    "${SSD}" --stop --pidfile "${PIDFILE}" --retry TERM/10/KILL/5 --remove-pidfile
    echo "${NAME} stopped."
}

case "$1" in
    start)   do_start ;;
    stop)    do_stop ;;
    restart) do_stop; sleep 1; do_start ;;
    status)
        if is_running; then
            echo "${NAME} running (pid $(cat "${PIDFILE}" 2>/dev/null))"
        else
            echo "${NAME} not running."
            exit 3
        fi
        ;;
    *) echo "Usage: $0 {start|stop|restart|status}"; exit 1 ;;
esac
TEMPLATE_EOF
            ;;
        com.monopsony.server.plist)
            cat <<'TEMPLATE_EOF'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.monopsony.server</string>

    <key>ProgramArguments</key>
    <array>
        <string>@INSTALL_DIR@/monopsony-server</string>
        <string>--config</string>
        <string>/etc/monopsony/server.toml</string>
@PLIST_EXTRA_ARGS@
    </array>

    <key>UserName</key>
    <string>monopsony</string>

    <key>RunAtLoad</key>
    <true/>

    <key>KeepAlive</key>
    <true/>

    <key>ThrottleInterval</key>
    <integer>5</integer>

    <key>WorkingDirectory</key>
    <string>/var/lib/monopsony</string>

    <key>StandardOutPath</key>
    <string>/var/log/monopsony/server.log</string>
    <key>StandardErrorPath</key>
    <string>/var/log/monopsony/server-error.log</string>

    <key>EnvironmentVariables</key>
    <dict>
        <key>RUST_LOG</key>
        <string>info</string>
    </dict>
</dict>
</plist>
TEMPLATE_EOF
            ;;
        *)
            error "Unknown template: $1"
            return 1
            ;;
    esac
}

render_template() {
    # $1 = template name, $2 = destination path, $3 = cf_mode ("true"/"false")
    local name="$1" dest="$2" cf_mode="${3:-false}"
    local dir src blockfile extra repl tok

    dir="$(template_dir)"
    src="$(mktemp)"
    blockfile="$(mktemp)"

    if [[ -n "${dir}" && -f "${dir}/${name}" ]]; then
        cat "${dir}/${name}" > "${src}"
    else
        template_embedded "${name}" > "${src}"
    fi

    # Extra CLI args — Cloudflare Tunnel mode binds every port to localhost
    extra=""
    if [[ "${cf_mode}" == "true" ]]; then
        extra="--http-addr 127.0.0.1:3128 --grpc-addr 127.0.0.1:50051 --dashboard-addr 127.0.0.1:3130 --metrics-addr 127.0.0.1:9090 --mirror-addr 127.0.0.1:3129"
    fi
    repl=""
    if [[ -n "${extra}" ]]; then
        repl=" ${extra}"
    fi

    # plist extras become one <string> element per arg on the token line
    if grep -q '@PLIST_EXTRA_ARGS@' "${src}"; then
        for tok in ${extra}; do
            printf '        <string>%s</string>\n' "${tok}" >> "${blockfile}"
        done
    fi

    mkdir -p "$(dirname "${dest}")"
    # POSIX-safe sed (no -i): stream edit into place
    sed -e "/@PLIST_EXTRA_ARGS@/r ${blockfile}" \
        -e "/@PLIST_EXTRA_ARGS@/d" \
        -e "s|@INSTALL_DIR@|${INSTALL_DIR}|g" \
        -e "s| @EXTRA_ARGS@|${repl}|g" \
        "${src}" > "${dest}"
    rm -f "${src}" "${blockfile}"
}

# ---------------------------------------------------------------------------
# Service installers (one per init system)
# ---------------------------------------------------------------------------
install_service_systemd() {
    local cf_mode="${1:-false}"
    info "Setting up systemd service..."

    local dest
    dest="${SYSTEMD_UNIT_DIR:-/etc/systemd/system}/monopsony-server.service"
    render_template monopsony-server.service "${dest}" "${cf_mode}"
    chmod 644 "${dest}"

    systemctl daemon-reload 2>/dev/null || warn "systemctl daemon-reload failed (no systemd?)"
    systemctl enable monopsony-server 2>/dev/null || warn "Failed to enable monopsony-server."
    systemctl restart monopsony-server 2>/dev/null || warn "Failed to start monopsony-server. Check: journalctl -u monopsony-server"
    ok "systemd service installed. Status: $(systemctl is-active monopsony-server 2>/dev/null || echo 'unknown')"
}

install_service_openrc() {
    local cf_mode="${1:-false}"
    info "Setting up OpenRC service..."

    local dest
    dest="${OPENRC_INIT_DIR:-/etc/init.d}/monopsony-server"
    render_template monopsony-server.openrc "${dest}" "${cf_mode}"
    chmod 755 "${dest}"

    rc-update add monopsony-server default 2>/dev/null || warn "rc-update add failed."
    rc-service monopsony-server restart 2>/dev/null || warn "rc-service start failed."
    ok "OpenRC service installed. Status: $(rc-service monopsony-server status 2>/dev/null || echo 'unknown')"
}

install_service_runit() {
    local cf_mode="${1:-false}"
    info "Setting up runit service..."

    local svdir servicedir
    svdir="${RUNIT_SV_DIR:-/etc/sv}/monopsony-server"
    servicedir="${RUNIT_SERVICE_DIR:-/var/service}"

    mkdir -p "${svdir}/log"
    render_template monopsony-server.runit "${svdir}/run" "${cf_mode}"
    chmod 755 "${svdir}/run"
    render_template monopsony-server.runit-log "${svdir}/log/run" "${cf_mode}"
    chmod 755 "${svdir}/log/run"

    if [[ -d "${servicedir}" ]]; then
        ln -sf "${svdir}" "${servicedir}/monopsony-server"
        # give runsv a moment to create supervise/ before issuing sv commands
        # (racing it produces "unable to open supervise/ok" warnings)
        sleep 2
        sv restart monopsony-server 2>/dev/null || true
        ok "runit service installed and linked into ${servicedir}."
    else
        ok "runit service installed at ${svdir}."
        warn "No ${servicedir} scan dir — enable with: ln -s ${svdir} ${servicedir}/monopsony-server"
    fi
}

s6_refresh_db() {
    # Refresh the compiled s6-rc database from /etc/s6/sv sources and
    # live-update it. Prefers Artix's `s6-rc-bundle update default`; upstream
    # s6-rc (Gentoo/Void) has no source-recompile verb, so compile a new db
    # and s6-rc-update directly. NEVER prune old compiled dbs — s6-rc-update
    # is asynchronous and deleting the db it is migrating from corrupts
    # /run/s6-rc/state.
    if command -v s6-rc-bundle >/dev/null 2>&1 && s6-rc-bundle update default 2>/dev/null; then
        return 0
    fi
    if command -v s6-rc-compile >/dev/null 2>&1 && command -v s6-rc-update >/dev/null 2>&1; then
        local newdb="/etc/s6/rc.dbs/compiled/monopsony-$$-$(date +%s%N 2>/dev/null || date +%s)"
        mkdir -p /etc/s6/rc.dbs/compiled
        if s6-rc-compile "${newdb}" /etc/s6/sv 2>/dev/null && s6-rc-update "${newdb}" 2>/dev/null; then
            if [[ -L /etc/s6/current ]]; then
                ln -sfn "${newdb}" /etc/s6/current
            fi
            return 0
        fi
    fi
    return 1
}

install_service_s6() {
    local cf_mode="${1:-false}"
    info "Setting up s6 service..."

    local svdir
    svdir="${S6_SV_DIR:-/etc/s6/sv}/monopsony-server"
    mkdir -p "${svdir}"
    echo "longrun" > "${svdir}/type"
    render_template monopsony-server.s6 "${svdir}/run" "${cf_mode}"
    chmod 755 "${svdir}/run"

    local contents="/etc/s6/sv/bundle/contents.d"
    if [[ -d "${contents}" ]]; then
        touch "${contents}/monopsony-server"
        if s6_refresh_db; then
            ok "s6 longrun service installed at ${svdir} and registered in the boot bundle."
        else
            ok "s6 longrun service installed at ${svdir} and added to the bundle sources."
            warn "s6-rc database refresh failed — recompile your s6-rc database to activate."
        fi
    else
        ok "s6 longrun service installed at ${svdir}."
        warn "No s6-rc bundle dir found — register ${svdir} with your s6-rc database manually."
    fi

    # s6-rc-update swaps the db definition but does NOT start newly added
    # services — bring it up explicitly (newer s6-rc uses "start", older "up")
    s6-rc -b start monopsony-server 2>/dev/null \
        || s6-rc -b up monopsony-server 2>/dev/null \
        || warn "Failed to start service — run: s6-rc -b start monopsony-server"
}

install_service_sysv() {
    local cf_mode="${1:-false}"
    info "Setting up SysV init service..."

    local dest
    dest="${SYSV_INIT_DIR:-/etc/init.d}/monopsony-server"
    render_template monopsony-server.sysv "${dest}" "${cf_mode}"
    chmod 755 "${dest}"

    if command -v update-rc.d >/dev/null 2>&1; then
        update-rc.d monopsony-server defaults
    elif command -v chkconfig >/dev/null 2>&1; then
        chkconfig --add monopsony-server 2>/dev/null || warn "chkconfig --add failed."
    else
        warn "Neither update-rc.d nor chkconfig found — create runlevel links manually."
    fi

    "${dest}" start || warn "Service start failed — check /var/log/monopsony/server.log"
    ok "SysV init script installed at ${dest}."
}

install_service_launchd() {
    local cf_mode="${1:-false}"
    info "Setting up launchd daemon (macOS)..."

    local dest
    dest="${LAUNCHD_DIR:-/Library/LaunchDaemons}/com.monopsony.server.plist"
    render_template com.monopsony.server.plist "${dest}" "${cf_mode}"
    chown root:wheel "${dest}" 2>/dev/null || true
    chmod 644 "${dest}"

    # Modern launchctl first, legacy fallback for older macOS
    if launchctl bootstrap system "${dest}" 2>/dev/null; then
        launchctl enable system/com.monopsony.server 2>/dev/null || true
        launchctl kickstart -k system/com.monopsony.server 2>/dev/null || true
        ok "launchd daemon bootstrapped: ${dest}"
    elif launchctl load -w "${dest}" 2>/dev/null; then
        ok "launchd daemon loaded (legacy): ${dest}"
    else
        warn "launchctl failed — load manually: launchctl bootstrap system ${dest}"
    fi
}

# ---------------------------------------------------------------------------
# Service setup — dispatch on detected init system
# ---------------------------------------------------------------------------
setup_service() {
    local cf_mode="${1:-false}"
    local init_sys
    init_sys="$(detect_init)"
    info "Detected init system: ${init_sys}"

    case "${init_sys}" in
        systemd)  install_service_systemd "${cf_mode}" ;;
        openrc)   install_service_openrc "${cf_mode}" ;;
        runit)    install_service_runit "${cf_mode}" ;;
        s6)       install_service_s6 "${cf_mode}" ;;
        sysvinit) install_service_sysv "${cf_mode}" ;;
        launchd)  install_service_launchd "${cf_mode}" ;;
        container)
            warn "Running inside a container with no init system — skipping service setup."
            warn "Start manually: ${INSTALL_DIR}/${BINARY_NAME} --config ${CONFIG_DIR}/server.toml"
            ;;
        *)
            error "Unknown init system — cannot install a service automatically."
            echo ""
            echo "Init templates ship in dist/init/ of the repository:"
            echo "  systemd:  monopsony-server.service"
            echo "  openrc:   monopsony-server.openrc"
            echo "  runit:    monopsony-server.runit / monopsony-server.runit-log"
            echo "  s6:       monopsony-server.s6"
            echo "  sysv:     monopsony-server.sysv"
            echo "  macOS:    com.monopsony.server.plist"
            echo ""
            echo "Or pin your init system explicitly and re-run:"
            echo "  MONOPSONY_INIT_SYSTEM=systemd bash install.sh --server"
            echo ""
            return 1
            ;;
    esac
}

# ---------------------------------------------------------------------------
# Service lifecycle helpers (stop/restart across all init systems)
# ---------------------------------------------------------------------------
service_stop_and_disable() {
    case "$(detect_init)" in
        systemd)
            systemctl stop monopsony-server 2>/dev/null || true
            systemctl disable monopsony-server 2>/dev/null || true
            ;;
        openrc)
            rc-service monopsony-server stop 2>/dev/null || true
            rc-update del monopsony-server default 2>/dev/null || true
            ;;
        runit)
            sv down monopsony-server 2>/dev/null || true
            rm -f "${RUNIT_SERVICE_DIR:-/var/service}/monopsony-server" 2>/dev/null || true
            ;;
        s6)
            # tell s6-rc the service is down (newer s6-rc uses "stop", older "down")
            s6-rc -b stop monopsony-server 2>/dev/null || s6-rc -b down monopsony-server 2>/dev/null || true
            s6-svc -d /run/s6-rc/servicedirs/monopsony-server 2>/dev/null || true
            rm -f /etc/s6/sv/bundle/contents.d/monopsony-server 2>/dev/null || true
            s6_refresh_db || true
            ;;
        sysvinit)
            /etc/init.d/monopsony-server stop 2>/dev/null || true
            update-rc.d -f monopsony-server remove 2>/dev/null || chkconfig --del monopsony-server 2>/dev/null || true
            ;;
        launchd)
            launchctl bootout system/com.monopsony.server 2>/dev/null \
                || launchctl unload -w "${LAUNCHD_DIR:-/Library/LaunchDaemons}/com.monopsony.server.plist" 2>/dev/null || true
            ;;
    esac
}

service_restart() {
    case "$(detect_init)" in
        systemd)  systemctl restart monopsony-server 2>/dev/null || warn "Service restart failed." ;;
        openrc)   rc-service monopsony-server restart 2>/dev/null || warn "Service restart failed." ;;
        runit)    sv restart monopsony-server 2>/dev/null || warn "Service restart failed." ;;
        s6)       s6-svc -r /run/s6-rc/servicedirs/monopsony-server 2>/dev/null || warn "Service restart failed." ;;
        sysvinit) /etc/init.d/monopsony-server restart 2>/dev/null || warn "Service restart failed." ;;
        launchd)  launchctl kickstart -k system/com.monopsony.server 2>/dev/null || warn "Service restart failed." ;;
        *)        warn "No init system detected — restart ${BINARY_NAME} manually." ;;
    esac
}

# ---------------------------------------------------------------------------
# Client setup — configure this machine to use a remote monopsony proxy
# ---------------------------------------------------------------------------
setup_client() {
    local os_ver
    os_ver="$(detect_os | cut -d: -f1)"

    echo ""
    echo "=========================================="
    echo "  Monopsony Client Setup"
    echo "=========================================="
    echo ""
    echo "  This machine will route package traffic through a remote"
    echo "  Monopsony caching proxy server."
    echo ""

    # Prompt for monopsony server address
    local proxy_host
    proxy_host="$(read_line "Monopsony server address (IP or hostname): " "")"
    if [[ -z "${proxy_host}" ]]; then
        error "No server address provided. Aborting."
        exit 1
    fi

    local proxy_port
    proxy_port="$(read_line "Monopsony proxy port [3128]: " "3128")"

    local proxy_url="http://${proxy_host}:${proxy_port}"

    echo ""
    info "Configuring proxy: ${proxy_url}"

    # --- System-wide proxy env vars (works for all distros) ---
    # profile.d (shells, most Linux)
    cat > "/etc/profile.d/monopsony-proxy.sh" <<PROFILE
export http_proxy="${proxy_url}"
export https_proxy="${proxy_url}"
export HTTP_PROXY="${proxy_url}"
export HTTPS_PROXY="${proxy_url}"
export no_proxy="localhost,127.0.0.1,::1"
export NO_PROXY="localhost,127.0.0.1,::1"
PROFILE
    ok "System proxy env: /etc/profile.d/monopsony-proxy.sh"

    # --- Distro-specific service configuration ---
    case "${os_ver}" in
        nixos)
            # nix-daemon needs its own systemd override
            local override_dir="/etc/systemd/system/nix-daemon.service.d"
            mkdir -p "${override_dir}"
            cat > "${override_dir}/monopsony-proxy.conf" <<OVERRIDE
[Service]
Environment="http_proxy=${proxy_url}"
Environment="https_proxy=${proxy_url}"
Environment="HTTP_PROXY=${proxy_url}"
Environment="HTTPS_PROXY=${proxy_url}"
Environment="no_proxy=localhost,127.0.0.1,::1"
Environment="NO_PROXY=localhost,127.0.0.1,::1"
OVERRIDE
            ok "nix-daemon proxy: ${override_dir}/monopsony-proxy.conf"
            systemctl daemon-reload 2>/dev/null
            systemctl restart nix-daemon 2>/dev/null || warn "nix-daemon not running yet"
            ;;
        *)
            # For all other distros: set proxy for the package manager + systemd services
            # apt/dnf/pacman/zypper all respect the env vars in their service units

            # systemd environment (for services like packagekit, unattended-upgrades, etc.)
            mkdir -p /etc/systemd/system.conf.d
            cat > "/etc/systemd/system.conf.d/monopsony-proxy.conf" <<ENVOVERRIDE
[Manager]
DefaultEnvironment="http_proxy=${proxy_url}" "https_proxy=${proxy_url}" "HTTP_PROXY=${proxy_url}" "HTTPS_PROXY=${proxy_url}" "no_proxy=localhost,127.0.0.1,::1" "NO_PROXY=localhost,127.0.0.1,::1"
ENVOVERRIDE
            ok "systemd default env: /etc/systemd/system.conf.d/monopsony-proxy.conf"
            systemctl daemon-reload 2>/dev/null || warn "systemctl daemon-reload failed"

            # Distro-specific package manager config
            case "${os_ver}" in
                debian|ubuntu)
                    cat > "/etc/apt/apt.conf.d/99monopsony-proxy" <<APT
Acquire::http::Proxy "${proxy_url}";
Acquire::https::Proxy "${proxy_url}";
APT
                    ok "apt proxy: /etc/apt/apt.conf.d/99monopsony-proxy"
                    ;;
                rocky|almalinux|rhel|centos)
                    sed -i "s|^proxy=.*|proxy=${proxy_url}|" /etc/dnf/dnf.conf 2>/dev/null || \
                        echo "proxy=${proxy_url}" >> /etc/dnf/dnf.conf
                    ok "dnf proxy: /etc/dnf/dnf.conf"
                    ;;
                arch)
                    # pacman uses the env vars, already covered by profile.d
                    ok "pacman proxy: via environment variables"
                    ;;
                opensuse-tumbleweed|opensuse-leap|sles)
                    cat > "/etc/zypp.d/monopsony-proxy" <<ZYPPCONF
[main]
proxy = ${proxy_url}
ZYPPCONF
                    ok "zypper proxy: profile.d env vars"
                    ;;
                alpine)
                    # apk uses env vars, already covered by profile.d
                    ok "apk proxy: via environment variables"
                    ;;
                void)
                    # xbps uses env vars
                    ok "xbps proxy: via environment variables"
                    ;;
                gentoo)
                    # emerge uses env vars, also set in make.conf if needed
                    ok "emerge proxy: via environment variables"
                    ;;
                *)
                    ok "${os_ver}: proxy via environment variables (profile.d)"
                    ;;
            esac
            ;;
    esac

    echo ""
    echo "=========================================="
    echo "  Monopsony Client Configured"
    echo "=========================================="
    echo ""
    echo "  Proxy:  ${proxy_url}"
    echo "  Shells: source /etc/profile.d/monopsony-proxy.sh"
    echo ""
    echo "  Package downloads will route through the Monopsony cache."
    echo "  First request = cache miss (fetched from upstream)."
    echo "  Subsequent requests = instant cache hit."
    echo ""

    # Distro-specific verification hints
    case "${os_ver}" in
        nixos)
            echo "  To verify:"
            echo "    nix-channel --update"
            echo "  To remove:"
            echo "    rm /etc/systemd/system/nix-daemon.service.d/monopsony-proxy.conf"
            ;;
        debian|ubuntu)
            echo "  To verify:"
            echo "    apt update"
            echo "  To remove:"
            echo "    rm /etc/apt/apt.conf.d/99monopsony-proxy"
            ;;
        rocky|almalinux|rhel|centos)
            echo "  To verify:"
            echo "    dnf check-update"
            echo "  To remove:"
            echo "    sed -i '/^proxy=/d' /etc/dnf/dnf.conf"
            ;;
        *)
            echo "  To verify: run your package manager's update command"
            ;;
    esac
    echo "    rm /etc/profile.d/monopsony-proxy.sh"
    echo "    rm -f /etc/systemd/system.conf.d/monopsony-proxy.conf"
    echo ""
    echo "=========================================="
}

# ---------------------------------------------------------------------------
# Health check wait
# ---------------------------------------------------------------------------
wait_for_health() {
    local max_wait=30
    local waited=0

    info "Waiting for server health check..."
    while [[ ${waited} -lt ${max_wait} ]]; do
        if curl -sf http://localhost:3128/health &>/dev/null; then
            ok "Server is healthy!"
            return 0
        fi
        sleep 2
        waited=$((waited + 2))
    done

    warn "Health check timed out after ${max_wait}s."
    case "$(detect_init)" in
        systemd) warn "Check logs: journalctl -u monopsony-server --since '5 minutes ago'" ;;
        *)       warn "Check logs: /var/log/monopsony/" ;;
    esac
    return 1
}

# ---------------------------------------------------------------------------
# Print final info
# ---------------------------------------------------------------------------
print_info() {
    echo ""
    echo "=========================================="
    echo "  Monopsony Server Installed Successfully"
    echo "=========================================="
    echo ""
    echo "  Version:    ${VERSION}"
    echo "  Config:     ${CONFIG_DIR}/server.toml"
    echo "  Binary:     ${INSTALL_DIR}/${BINARY_NAME}"
    case "$(detect_init)" in
        openrc)
            echo "  Service:    rc-service monopsony-server status"
            echo "  Logs:       /var/log/monopsony/"
            ;;
        runit)
            echo "  Service:    sv status monopsony-server"
            echo "  Logs:       /var/log/monopsony/ (svlogd)"
            ;;
        s6)
            echo "  Service:    s6-svstat /run/s6-rc/servicedirs/monopsony-server"
            echo "  Logs:       /var/log/monopsony/"
            ;;
        sysvinit)
            echo "  Service:    /etc/init.d/monopsony-server status"
            echo "  Logs:       /var/log/monopsony/server.log"
            ;;
        launchd)
            echo "  Service:    launchctl print system/com.monopsony.server"
            echo "  Logs:       /var/log/monopsony/"
            ;;
        *)
            echo "  Service:    systemctl status monopsony-server"
            echo "  Logs:       journalctl -u monopsony-server -f"
            ;;
    esac
    echo ""
    echo "  Server URL: http://<server-ip>:3128"
    echo ""
    echo "  To pair a client:"
    echo "    monopsony-client pair <server-ip> <pairing-code>"
    echo ""
    echo "  To get a pairing code:"
    echo "    sudo monopsony-server pair"
    echo ""
    echo "=========================================="
    echo ""
}

# ---------------------------------------------------------------------------
# Uninstall
# ---------------------------------------------------------------------------
do_uninstall() {
    info "Starting uninstall..."

    local fully_removed=0

    # Stop and disable the service (any init system)
    service_stop_and_disable
    ok "Service stopped and disabled."

    # Remove service unit/script per detected init system
    local init_sys
    init_sys="$(detect_init)"
    case "${init_sys}" in
        systemd)
            rm -f "/etc/systemd/system/monopsony-server.service" "/usr/lib/systemd/system/monopsony-server.service"
            systemctl daemon-reload 2>/dev/null || true
            ok "Removed systemd unit."
            ;;
        openrc|sysvinit)
            rm -f "/etc/init.d/monopsony-server"
            ok "Removed init script."
            ;;
        runit)
            rm -rf "/etc/sv/monopsony-server"
            ok "Removed runit service dir."
            ;;
        s6)
            rm -rf "/etc/s6/sv/monopsony-server"
            rm -f "/etc/s6/sv/bundle/contents.d/monopsony-server" 2>/dev/null || true
            ok "Removed s6 service dir."
            ;;
        launchd)
            rm -f "${LAUNCHD_DIR:-/Library/LaunchDaemons}/com.monopsony.server.plist"
            ok "Removed launchd plist."
            ;;
    esac

    # Remove binary
    if [[ -f "${INSTALL_DIR}/${BINARY_NAME}" ]]; then
        rm -f "${INSTALL_DIR}/${BINARY_NAME}"
        ok "Removed binary."
    fi

    # Remove config (ask)
    if [[ -d "${CONFIG_DIR}" ]]; then
        read -r -p "Remove config directory ${CONFIG_DIR}? [y/N]: " confirm
        if [[ "${confirm,,}" == "y" ]]; then
            rm -rf "${CONFIG_DIR}"
            ok "Removed config directory."
        else
            warn "Kept config directory."
        fi
    fi

    # Remove cache (ask)
    if [[ -d "${CACHE_DIR}" ]]; then
        read -r -p "Remove cache directory ${CACHE_DIR}? [y/N]: " confirm
        if [[ "${confirm,,}" == "y" ]]; then
            rm -rf "${CACHE_DIR}"
            ok "Removed cache directory."
        else
            warn "Kept cache directory."
        fi
    fi

    # Remove lib dir (ask)
    if [[ -d "${LIB_DIR}" ]]; then
        read -r -p "Remove lib directory ${LIB_DIR}? [y/N]: " confirm
        if [[ "${confirm,,}" == "y" ]]; then
            rm -rf "${LIB_DIR}"
            ok "Removed lib directory."
        else
            warn "Kept lib directory."
        fi
    fi

    # Remove user (ask)
    if id "${USER_NAME}" &>/dev/null; then
        read -r -p "Remove user ${USER_NAME}? [y/N]: " confirm
        if [[ "${confirm,,}" == "y" ]]; then
            if [[ "$(uname -s)" == "Darwin" ]]; then
                dscl . -delete "/Users/${USER_NAME}" 2>/dev/null || true
                dscl . -delete "/Groups/${GROUP_NAME}" 2>/dev/null || true
            elif [[ "$(detect_os | cut -d: -f1)" == "alpine" ]]; then
                # BusyBox: deluser/delgroup, not userdel
                deluser "${USER_NAME}" 2>/dev/null || true
                delgroup "${GROUP_NAME}" 2>/dev/null || true
            else
                userdel "${USER_NAME}" 2>/dev/null || true
            fi
            ok "Removed user."
        else
            warn "Kept user."
        fi
    fi

    # Remove PostgreSQL (ask)
    if run_as_postgres psql -tAc "SELECT 1 FROM pg_database WHERE datname='${DB_NAME}'" 2>/dev/null | grep -q 1; then
        read -r -p "Drop PostgreSQL database '${DB_NAME}' and user '${DB_USER}'? [y/N]: " confirm
        if [[ "${confirm,,}" == "y" ]]; then
            run_as_postgres dropdb --if-exists "${DB_NAME}"
            run_as_postgres dropuser --if-exists "${DB_USER}"
            ok "Dropped PostgreSQL database and user."
        else
            warn "Kept PostgreSQL database and user."
        fi
    fi

    echo ""
    ok "Uninstall complete."
}

# ---------------------------------------------------------------------------
# Upgrade
# ---------------------------------------------------------------------------
do_upgrade() {
    local current_version
    current_version="$(get_installed_version)"

    if [[ "${current_version}" == "none" ]]; then
        error "No existing installation found. Run normal install instead."
        exit 1
    fi

    info "Upgrading from ${current_version} to ${VERSION}..."

    # Backup config
    if [[ -f "${CONFIG_DIR}/server.toml" ]]; then
        cp "${CONFIG_DIR}/server.toml" "${CONFIG_DIR}/server.toml.bak.$(date +%Y%m%d%H%M%S)"
        ok "Config backed up."
    fi

    # Download new binary
    download_binary

    # Restart service
    service_restart

    # Wait for health
    wait_for_health

    echo ""
    ok "Upgraded from ${current_version} to ${VERSION}."
}

# ---------------------------------------------------------------------------
# Reconfigure
# ---------------------------------------------------------------------------
do_reconfigure() {
    info "Re-running setup prompts..."

    # PostgreSQL setup choice
    echo ""
    echo "Database setup:"
    echo "  1) Local PostgreSQL (auto-setup)"
    echo "  2) External PostgreSQL (guided)"
    echo "  3) Skip database setup"
    echo ""
    local db_choice
    db_choice="$(read_line "Choose [1/2/3]: " "1")"

    case "${db_choice}" in
        1) setup_local_postgres ;;
        2) setup_external_postgres ;;
        3) info "Skipping database setup." ;;
        *) error "Invalid choice." ;;
    esac

    # TLS re-bootstrap
    read -r -p "Regenerate TLS certificates? [y/N]: " confirm
    if [[ "${confirm,,}" == "y" ]]; then
        bootstrap_tls
    else
        info "Keeping existing TLS certificates."
    fi

    # Cloudflare Tunnel (re)configuration
    setup_cloudflare_tunnel || true

    # Restart service
    service_restart

    ok "Reconfiguration complete."
}

# ---------------------------------------------------------------------------
# Read line with default
# ---------------------------------------------------------------------------
read_line() {
    local prompt="$1" default="$2"
    read -rp "${prompt}" input
    echo "${input:-${default}}"
}

# ---------------------------------------------------------------------------
# Cloudflare Tunnel setup
# ---------------------------------------------------------------------------
# When the server runs behind a Cloudflare Tunnel, only the dashboard port
# (3130) needs to be exposed. The dashboard already serves /ws/tunnel and
# /ws/pair relay endpoints that proxy all gRPC and pairing traffic over
# WebSocket binary frames. This means the server can function fully with
# zero exposed ports besides the tunnel — all client connections arrive via
# WSS through Cloudflare's edge.
#
# The tunnel connector (cloudflared) runs as a systemd service that
# establishes an outbound connection to CF's edge, so no inbound ports
# need to be opened on the host firewall.

setup_cloudflare_tunnel() {
    echo ""
    echo "=========================================="
    echo "  Cloudflare Tunnel Setup"
    echo "=========================================="
    echo ""
    echo "  A Cloudflare Tunnel exposes your Monopsony server"
    echo "  through a single public hostname without opening any"
    echo "  inbound ports. All traffic (dashboard, pairing, gRPC"
    echo "  control plane) flows through WSS connections."
    echo ""

    # Prompt: will this server be behind CF Tunnels?
    read -r -p "Will this server be behind a Cloudflare Tunnel? [y/N]: " cf_confirm
    if [[ "${cf_confirm,,}" != "y" ]]; then
        info "Skipping Cloudflare Tunnel setup."
        return 1
    fi

    # --- Collect tunnel details ---
    echo ""
    echo "  You need a Cloudflare Tunnel token. Get one at:"
    echo "    https://one.dash.cloudflare.com/ → Networks → Tunnels → Create Tunnel"
    echo ""
    read -r -p "Paste your Cloudflare Tunnel token: " cf_token
    if [[ -z "${cf_token}" ]]; then
        error "No tunnel token provided. Skipping CF Tunnel setup."
        return 1
    fi

    local cf_hostname
    cf_hostname="$(read_line "Public hostname for this server (e.g. monopsony.example.com): " "")"
    if [[ -z "${cf_hostname}" ]]; then
        error "No hostname provided. Skipping CF Tunnel setup."
        return 1
    fi

    # --- Install cloudflared if not present ---
    if ! command -v cloudflared &>/dev/null; then
        info "Installing cloudflared..."
        local os_ver arch
        os_ver="$(detect_os | cut -d: -f1)"
        arch="$(detect_arch)"

        case "${os_ver}" in
            arch)
                pacman -S --noconfirm cloudflared 2>/dev/null || install_cloudflared_binary "${arch}"
                ;;
            debian|ubuntu)
                # Download the .deb directly (works without adding repos)
                install_cloudflared_binary "${arch}"
                ;;
            rocky|almalinux|rhel|centos)
                install_cloudflared_binary "${arch}"
                ;;
            alpine)
                install_cloudflared_binary "${arch}"
                ;;
            *)
                install_cloudflared_binary "${arch}"
                ;;
        esac
    else
        ok "cloudflared already installed: $(cloudflared --version 2>&1)"
    fi

    if ! command -v cloudflared &>/dev/null; then
        error "Failed to install cloudflared. Install manually from https://github.com/cloudflare/cloudflared/releases"
        error "Then run: cloudflared service install ${cf_token}"
        return 1
    fi

    # --- Configure the tunnel service ---
    # cloudflared runs as a systemd service. The token-based connector
    # doesn't need a local config.yml — all routing is configured in the
    # Cloudflare dashboard.
    info "Configuring cloudflared service..."

    local cf_service_file="/etc/systemd/system/cloudflared.service"
    cat > "${cf_service_file}" <<CFSERVICE
[Unit]
Description=Cloudflare Tunnel (cloudflared)
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=/usr/bin/cloudflared tunnel --no-autoupdate run --token ${cf_token}
Restart=on-failure
RestartSec=5

[Install]
WantedBy=multi-user.target
CFSERVICE
    chmod 644 "${cf_service_file}"
    ok "Created cloudflared service at ${cf_service_file}"

    # Enable and start
    systemctl daemon-reload 2>/dev/null || true
    systemctl enable cloudflared 2>/dev/null || warn "Failed to enable cloudflared service."
    systemctl start cloudflared 2>/dev/null || warn "Failed to start cloudflared service."

    sleep 2
    if systemctl is-active --quiet cloudflared 2>/dev/null; then
        ok "cloudflared service is running."
    else
        warn "cloudflared service may not have started. Check: journalctl -u cloudflared"
    fi

    # --- Configure server.toml for CF Tunnel mode ---
    local config_file="${CONFIG_DIR}/server.toml"
    if [[ -f "${config_file}" ]]; then
        info "Configuring server.toml for Cloudflare Tunnel mode..."

        local ws_url="wss://${cf_hostname}/ws/tunnel"

        # Add or replace ws_url in [grpc] section
        if grep -q "^ws_url" "${config_file}" 2>/dev/null; then
            sed -i "s|^ws_url.*|ws_url = \"${ws_url}\"|" "${config_file}"
        else
            # Insert ws_url after the [grpc] bind line
            sed -i "/^\[grpc\]/,/^\[/ { /^bind/a ws_url = \"${ws_url}\"
            }" "${config_file}"
        fi

        # Add TLS SAN for the public hostname (so the inner mTLS cert matches)
        if grep -q "^san_names" "${config_file}" 2>/dev/null; then
            sed -i "s|^san_names.*|san_names = [\"${cf_hostname}\"]|" "${config_file}"
        else
            sed -i "/^\[tls\]/,/^\[/ { /server_key_path/a san_names = [\"${cf_hostname}\"]
            }" "${config_file}"
        fi

        ok "ws_url set to ${ws_url}"
        ok "TLS SAN includes ${cf_hostname}"

        # Enable always_require_login for dashboard (security: public hostname)
        if ! grep -q "always_require_login" "${config_file}" 2>/dev/null; then
            echo "" >> "${config_file}"
            echo "[dashboard]" >> "${config_file}"
            echo "always_require_login = true" >> "${config_file}"
        else
            sed -i "s|always_require_login.*|always_require_login = true|" "${config_file}"
        fi
        ok "Dashboard login required (public hostname)"
    fi

    # --- Set dashboard password if not already set ---
    if ! grep -q "password_hash" "${config_file}" 2>/dev/null || \
       grep -q 'password_hash.*""' "${config_file}" 2>/dev/null; then
        echo ""
        warn "Dashboard has no password set. Since this server will be"
        warn "publicly accessible via ${cf_hostname}, a password is REQUIRED."
        echo ""
        "${INSTALL_DIR}/${BINARY_NAME}" dashboard-set-password 2>/dev/null || {
            warn "Could not set password via CLI. Set it manually:"
            warn "  ${BINARY_NAME} dashboard-set-password"
        }
    fi

    # --- Regenerate server cert with the new SAN ---
    info "Regenerating server certificate with ${cf_hostname} SAN..."
    if [[ -f "${LIB_DIR}/ca.pem" && -f "${LIB_DIR}/ca.key" ]]; then
        generate_server_cert "${LIB_DIR}/ca.pem" "${LIB_DIR}/ca.key"
    else
        warn "CA not found. Run --reconfigure to bootstrap TLS."
    fi

    echo ""
    echo "=========================================="
    echo "  Cloudflare Tunnel Configured"
    echo "=========================================="
    echo ""
    echo "  Public URL:     https://${cf_hostname}"
    echo "  Dashboard:      https://${cf_hostname}/dashboard"
    echo "  WS Tunnel:      wss://${cf_hostname}/ws/tunnel"
    echo "  WS Pairing:     wss://${cf_hostname}/ws/pair"
    echo ""
    echo "  All ports are bound to localhost — no inbound"
    echo "  firewall rules needed."
    echo ""
    echo "  Configure your public DNS / Cloudflare Tunnel"
    echo "  routing to point to http://localhost:3130"
    echo ""
    echo "=========================================="

    return 0
}

# Install cloudflared binary from GitHub releases (fallback for all distros)
install_cloudflared_binary() {
    local arch="$1"
    local cf_arch
    case "${arch}" in
        x86_64)  cf_arch="amd64" ;;
        aarch64) cf_arch="arm64" ;;
        *)       cf_arch="amd64" ;;
    esac

    local os_ver
    os_ver="$(detect_os | cut -d: -f1)"

    local pkg_type="linux"
    local url

    case "${os_ver}" in
        debian|ubuntu)
            url="https://github.com/cloudflare/cloudflared/releases/latest/download/cloudflared-linux-${cf_arch}.deb"
            ;;
        rocky|almalinux|rhel|centos)
            url="https://github.com/cloudflare/cloudflared/releases/latest/download/cloudflared-linux-${cf_arch}.rpm"
            ;;
        *)
            # Generic binary
            url="https://github.com/cloudflare/cloudflared/releases/latest/download/cloudflared-linux-${cf_arch}"
            ;;
    esac

    local tmpfile
    tmpfile="$(mktemp)"

    info "Downloading cloudflared from ${url}..."
    if curl -fsSL --retry 3 -o "${tmpfile}" "${url}"; then
        case "${os_ver}" in
            debian|ubuntu)
                dpkg -i "${tmpfile}" 2>/dev/null || { error "Failed to install cloudflared .deb"; rm -f "${tmpfile}"; return 1; }
                ;;
            rocky|almalinux|rhel|centos)
                rpm -i "${tmpfile}" 2>/dev/null || { error "Failed to install cloudflared .rpm"; rm -f "${tmpfile}"; return 1; }
                ;;
            *)
                install -m 755 "${tmpfile}" /usr/bin/cloudflared || { error "Failed to install cloudflared binary"; rm -f "${tmpfile}"; return 1; }
                ;;
        esac
        ok "cloudflared installed."
    else
        error "Failed to download cloudflared from ${url}"
        rm -f "${tmpfile}"
        return 1
    fi
    rm -f "${tmpfile}"
}

# ---------------------------------------------------------------------------
# Installer maintenance / diagnostics
# ---------------------------------------------------------------------------
do_detect_init() {
    local init_sys
    init_sys="$(detect_init)"
    echo "init system: ${init_sys}"
    if [[ "$(uname -s)" != "Darwin" ]]; then
        local pid1=""
        if [[ -r /proc/1/comm ]]; then
            pid1="$(tr -d '[:space:]' < /proc/1/comm 2>/dev/null || true)"
        fi
        local m_systemd="no" m_openrc="no" m_runit="no" m_s6="no" m_container="no"
        if [[ -d /run/systemd/system ]]; then m_systemd="yes"; fi
        if [[ -f /run/openrc/softlevel ]]; then m_openrc="yes"; fi
        if [[ -d /var/service || -d /service ]]; then m_runit="yes"; fi
        if command -v s6-sv >/dev/null 2>&1; then m_s6="yes"; fi
        if [[ -f /.dockerenv || -f /run/.containerenv ]]; then m_container="yes"; fi
        echo "  pid 1:     ${pid1:-unknown}"
        echo "  markers:   systemd=${m_systemd} | openrc=${m_openrc} | runit-sv=${m_runit} | s6=${m_s6} | container=${m_container}"
    fi
}

do_check_templates() {
    local dir failures=0 name
    dir="$(template_dir)"
    if [[ -z "${dir}" ]]; then
        error "No dist/init/ directory found next to install.sh — nothing to check."
        exit 1
    fi
    for name in "${TEMPLATE_FILES[@]}"; do
        if ! template_embedded "${name}" | diff -q - "${dir}/${name}" >/dev/null 2>&1; then
            error "Template out of sync: ${name} (embedded copy != ${dir}/${name})"
            failures=$((failures + 1))
        fi
    done
    if [[ ${failures} -gt 0 ]]; then
        error "${failures} template(s) out of sync. Regenerate with: bash install.sh --emit-templates <dir>"
        exit 1
    fi
    ok "All ${#TEMPLATE_FILES[@]} templates in sync with ${dir}/"
}

do_emit_templates() {
    local dir="$1" name
    mkdir -p "${dir}"
    for name in "${TEMPLATE_FILES[@]}"; do
        template_embedded "${name}" > "${dir}/${name}"
    done
    ok "Emitted ${#TEMPLATE_FILES[@]} templates to ${dir}/"
}

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------
main() {
    set_platform_paths
    EMIT_TEMPLATES_DIR=""
    local action="client"
    local version_pinned=false
    local client_explicit=false

    # Parse arguments
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --server)        action="server" ;;
            --client)        action="client"; client_explicit=true ;;
            --uninstall)     action="uninstall" ;;
            --upgrade)       action="upgrade" ;;
            --reconfigure)   action="reconfigure" ;;
            --detect-init)   action="detect-init" ;;
            --check-templates) action="check-templates" ;;
            --version)
                shift
                if [[ -z "${1:-}" ]]; then
                    error "--version requires a value (e.g. 1.0.0-rc2)."
                    exit 1
                fi
                MONOPSONY_VERSION="$1"
                version_pinned=true
                ;;
            --emit-templates)
                action="emit-templates"
                shift
                if [[ -z "${1:-}" ]]; then
                    error "--emit-templates requires a directory argument."
                    exit 1
                fi
                EMIT_TEMPLATES_DIR="$1"
                ;;
            --help|-h)
                echo "Usage: $0 [--server|--client|--uninstall|--upgrade|--reconfigure]"
                echo "       $0 --detect-init | --check-templates | --emit-templates DIR"
                echo ""
                echo "  (no args)       Configure this machine as a client (proxy setup)"
                echo "  --client        Same as default — client proxy setup"
                echo "  --server        Install Monopsony caching proxy server"
                echo "  --version V     Pin the release version to install (default: latest)"
                echo "  --uninstall     Remove Monopsony"
                echo "  --upgrade       Upgrade to latest version"
                echo "  --reconfigure   Re-run setup prompts"
                exit 0
                ;;
            *)
                error "Unknown argument: $1"
                echo "Use --help for usage."
                exit 1
                ;;
        esac
        shift
    done

    # A pinned --version only makes sense for an install/upgrade action —
    # error instead of surprising the user with a default client install.
    if [[ "${version_pinned}" == "true" && "${action}" == "client" && "${client_explicit}" != "true" ]]; then
        error "--version must be combined with --server, --client, or --upgrade."
        exit 1
    fi

    # Resolve the release version only for actions that download artifacts.
    case "${action}" in
        server|client|upgrade)
            VERSION="$(resolve_version)"
            info "Monopsony release: v${VERSION}"
            ;;
    esac

    case "${action}" in
        detect-init)
            do_detect_init
            ;;
        check-templates)
            do_check_templates
            ;;
        emit-templates)
            do_emit_templates "${EMIT_TEMPLATES_DIR}"
            ;;
        uninstall)
            require_root
            do_uninstall
            ;;
        upgrade)
            require_root
            do_upgrade
            ;;
        reconfigure)
            require_root
            do_reconfigure
            ;;
        client)
            require_root
            check_prereqs
            download_binary "client"
            setup_client
            ;;
        server)
            require_root
            check_prereqs
            setup_user_and_dirs
            generate_config
            bootstrap_tls

            # Database setup
            echo ""
            echo "Database setup:"
            echo "  1) Local PostgreSQL (auto-setup)"
            echo "  2) External PostgreSQL (guided)"
            echo "  3) Skip database setup (configure manually)"
            echo ""
            local db_choice
            db_choice="$(read_line "Choose [1/2/3]: " "1")"

            case "${db_choice}" in
                1) setup_local_postgres ;;
                2) setup_external_postgres ;;
                3) info "Skipping database setup. Configure manually in ${CONFIG_DIR}/server.toml." ;;
                *) error "Invalid choice. Skipping database setup." ;;
            esac

            download_binary

            # Cloudflare Tunnel setup (interactive — prompts the user)
            local cf_enabled=false
            if setup_cloudflare_tunnel; then
                cf_enabled=true
            fi

            setup_service "${cf_enabled}"
            wait_for_health
            print_info
            ;;
    esac
}

# Allow sourcing for tests: only dispatch when executed directly
if [[ "${BASH_SOURCE[0]}" == "${0}" ]]; then
    main "$@"
fi
