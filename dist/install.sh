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
#
# Supported OSes: Arch, Debian 12+, Ubuntu 22.04+, Rocky/Alma/RHEL 9+, Alpine 3.x,
#                 openSUSE (Tumbleweed/Leap), Void Linux, Gentoo, Clear Linux, Solus,
#                 NixOS (client only)
# Graceful bail:  FreeBSD (not Linux)
# Supported arches: x86_64, aarch64
# ============================================================================

set -euo pipefail

# ---------------------------------------------------------------------------
# Globals
# ---------------------------------------------------------------------------
readonly VERSION="0.1.0"
readonly DOWNLOAD_BASE="https://base.societus.club/societus/monopsony/releases/download"
readonly BINARY_NAME="monopsony-server"
readonly INSTALL_DIR="/usr/bin"
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
# Download binary
# ---------------------------------------------------------------------------
download_binary() {
    local arch os_ver
    arch="$(detect_arch)"
    os_ver="$(detect_os | cut -d: -f1)"

    # Map OS to release asset name
    local asset_name
    case "${os_ver}" in
        arch)  asset_name="monopsony-server-v${VERSION}-linux-x86_64.tar.gz" ;;
        debian|ubuntu) asset_name="monopsony-server-v${VERSION}-linux-x86_64.tar.gz" ;;
        *)     asset_name="monopsony-server-v${VERSION}-linux-x86_64.tar.gz" ;;
    esac

    # If aarch64, use that asset
    if [[ "${arch}" == "aarch64" ]]; then
        asset_name="monopsony-server-v${VERSION}-linux-aarch64.tar.gz"
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
    mv "${tmpdir}/monopsony-server" "${INSTALL_DIR}/${BINARY_NAME}"
    chmod 755 "${INSTALL_DIR}/${BINARY_NAME}"
    rm -rf "${tmpdir}"
    ok "Binary installed to ${INSTALL_DIR}/${BINARY_NAME}"
}

# ---------------------------------------------------------------------------
# Create user, groups, directories
# ---------------------------------------------------------------------------
setup_user_and_dirs() {
    local os_ver
    os_ver="$(detect_os | cut -d: -f1)"

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
# PostgreSQL setup — local
# ---------------------------------------------------------------------------
setup_local_postgres() {
    info "Setting up local PostgreSQL..."

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
        # Replace the *** placeholder with the actual password using sed
        local escaped_pw
        escaped_pw=$(printf '%s\n' "$password" | sed 's/[&/\]/\\&/g')
        sed -i "s|postgres://monopsony:\*\*\*@localhost/monopsony|postgres://monopsony:${escaped_pw}@localhost/monopsony|" "${config_file}"
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
    echo "  url = \"postgres://monopsony:${password}@${host}:${port}/monopsony\""
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
        local escaped_password
        escaped_password="$(printf '%s' "${password}" | sed 's/[&\/\\]/\\&/g')"
        sed -i "s|url = \"postgres://monopsony:CHANGE_ME@localhost/monopsony\"|url = \"postgres://monopsony:${escaped_password}@${host}:${port}/monopsony\"|" "${config_file}"
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
# Service setup
# ---------------------------------------------------------------------------
setup_service() {
    local os_ver
    os_ver="$(detect_os | cut -d: -f1)"

    case "${os_ver}" in
        alpine)
            setup_service_openrc
            ;;
        void)
            setup_service_runit
            ;;
        gentoo)
            # Gentoo can use either systemd or OpenRC
            if command -v systemctl >/dev/null 2>&1; then
                setup_service_systemd
            else
                setup_service_openrc
            fi
            ;;
        *)
            setup_service_systemd
            ;;
    esac
}

setup_service_systemd() {
    info "Setting up systemd service..."

    local service_file="${CONFIG_DIR}/monopsony-server.service"

    # Copy service file
    if [[ -f "/usr/lib/systemd/system/monopsony-server.service" ]]; then
        info "Service file already installed."
    else
        # Find the service file in dist/ or use installed one
        local dist_service="/usr/share/monopsony/dist/monopsony-server.service"
        if [[ -f "${dist_service}" ]]; then
            cp "${dist_service}" "/etc/systemd/system/monopsony-server.service"
        else
            # Generate inline
            cat > "/etc/systemd/system/monopsony-server.service" <<'SERVICE'
[Unit]
Description=Monopsony Caching Proxy Server
Documentation=https://base.societus.club/societus/monopsony
After=network-online.target postgresql.service
Wants=network-online.target

[Service]
Type=simple
ExecStart=/usr/bin/monopsony-server --config /etc/monopsony/server.toml
ExecReload=/bin/kill -HUP $MAINPID
Restart=on-failure
RestartSec=5

LimitNOFILE=65536
ReadWritePaths=/var/cache/monopsony /var/lib/monopsony
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true

User=monopsony
Group=monopsony

Environment=RUST_LOG=info
Environment=RUST_BACKTRACE=1

[Install]
WantedBy=multi-user.target
SERVICE
        fi
    fi

    # Reload systemd
    systemctl daemon-reload 2>/dev/null || warn "systemd daemon-reload failed (may not be running in container)."

    # Enable and start
    systemctl enable monopsony-server 2>/dev/null || warn "Failed to enable service."
    systemctl start monopsony-server 2>/dev/null || warn "Failed to start service. Check logs: journalctl -u monopsony-server"

    ok "Service configured. Status: $(systemctl is-active monopsony-server 2>/dev/null || echo 'unknown')"
}

setup_service_openrc() {
    info "Setting up OpenRC service..."

    cat > "/etc/init.d/monopsony-server" <<'OPENRC'
#!/sbin/openrc-run

name="monopsony-server"
description="Monopsony Caching Proxy Server"
command="/usr/bin/monopsony-server"
command_args="--config /etc/monopsony/server.toml"
command_user="monopsony:monopsony"
pidfile="/run/${RC_SVCNAME}.pid"
command_background="yes"
start_stop_daemon_args="--make-pidfile"

depend() {
    need net
    after postgresql
}

OPENRC
    chmod 755 "/etc/init.d/monopsony-server"

    rc-update add monopsony-server default 2>/dev/null || warn "Failed to enable service."
    rc-service monopsony-server start 2>/dev/null || warn "Failed to start service."

    ok "Service configured. Status: $(rc-service monopsony-server status 2>/dev/null || echo 'unknown')"
}

setup_service_runit() {
    info "Setting up runit service..."

    mkdir -p /etc/sv/monopsony-server

    cat > "/etc/sv/monopsony-server/run" <<'RUNIT'
#!/bin/sh
exec chpst -u monopsony:monopsony /usr/bin/monopsony-server --config /etc/monopsony/server.toml 2>&1
RUNIT
    chmod 755 "/etc/sv/monopsony-server/run"

    # Create log service
    mkdir -p /etc/sv/monopsony-server/log
    cat > "/etc/sv/monopsony-server/log/run" <<'RUNIT_LOG'
#!/bin/sh
exec chpst -u monopsony:monopsony svlogd -tt /var/log/monopsony
RUNIT_LOG
    chmod 755 "/etc/sv/monopsony-server/log/run"

    # Enable service
    ln -sf /etc/sv/monopsony-server /var/service/ 2>/dev/null || warn "Failed to enable service."

    sv start monopsony-server 2>/dev/null || warn "Failed to start service."

    ok "Service configured. Status: $(sv status monopsony-server 2>/dev/null || echo 'unknown')"
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
    warn "Check logs: journalctl -u monopsony-server --since '5 minutes ago'"
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
    local os_ver
    os_ver="$(detect_os | cut -d: -f1)"
    case "${os_ver}" in
        alpine|gentoo)
            if ! command -v systemctl >/dev/null 2>&1; then
                echo "  Service:    rc-service monopsony-server status"
                echo "  Logs:       cat /var/log/monopsony/*.log"
            else
                echo "  Service:    systemctl status monopsony-server"
                echo "  Logs:       journalctl -u monopsony-server -f"
            fi
            ;;
        void)
            echo "  Service:    sv status monopsony-server"
            echo "  Logs:       svlogd tail /var/log/monopsony"
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

    # Stop service
    if systemctl is-active --quiet monopsony-server 2>/dev/null; then
        systemctl stop monopsony-server
        systemctl disable monopsony-server
        ok "Service stopped and disabled."
    fi

    # Remove systemd unit
    if [[ -f "/etc/systemd/system/monopsony-server.service" ]]; then
        rm -f "/etc/systemd/system/monopsony-server.service"
        systemctl daemon-reload 2>/dev/null || true
        ok "Removed systemd unit."
    fi

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
            userdel "${USER_NAME}" 2>/dev/null || true
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
    systemctl restart monopsony-server 2>/dev/null || warn "Service restart failed."

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

    # Restart service
    systemctl restart monopsony-server 2>/dev/null || warn "Service restart failed."

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
# Main
# ---------------------------------------------------------------------------
main() {
    local action="client"

    # Parse arguments
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --server)        action="server" ;;
            --client)        action="client" ;;
            --uninstall)     action="uninstall" ;;
            --upgrade)       action="upgrade" ;;
            --reconfigure)   action="reconfigure" ;;
            --help|-h)
                echo "Usage: $0 [--server|--client|--uninstall|--upgrade|--reconfigure]"
                echo ""
                echo "  (no args)       Configure this machine as a client (proxy setup)"
                echo "  --client        Same as default — client proxy setup"
                echo "  --server        Install Monopsony caching proxy server"
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

    case "${action}" in
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
            setup_service
            wait_for_health
            print_info
            ;;
    esac
}

main "$@"
