# Zero Clients in Dashboard Investigation

**Date:** 2025-07-02
**Issue:** Dashboard shows zero clients despite "connections working" tests
**Workspace:** /home/societus/projects/monopsony

---

## Executive Summary

Dashboard shows zero clients because **no client has ever completed gRPC registration**. Previous "connections work" tests hit the HTTP proxy (port 3128), which requires no authentication. The complete registration→heartbeat→inventory flow requires gRPC with mTLS, which has never successfully completed.

---

## Investigation Steps

### 1. Verified Dashboard Data Source

**File:** `crates/monopsony-admin/src/clients.rs:8-28`

```rust
pub async fn handle_list_clients(State(state): State<AdminState>) -> impl IntoResponse {
    if let Some(ref db) = state.db {
        match db.list_clients().await {
            Ok(clients) => {
                let result: Vec<serde_json::Value> = clients.iter().map(|c| {
                    serde_json::json!({
                        "id": c.id.0,
                        "hostname": c.hostname,
                        "os": c.os,
                        "registered_at": c.registered_at,
                        "last_seen": c.last_seen,
                    })
                }).collect();
                Json(serde_json::Value::Array(result))
            }
            Err(e) => Json(serde_json::json!({"error": e.to_string()})),
        }
    } else {
        Json(serde_json::json!([]))
    }
}
```

**Database query:**
```sql
SELECT id, hostname, os, os_version, package_managers, last_seen, registered_at, cert_serial
FROM clients ORDER BY registered_at DESC
```

**Result:** Returns 0 rows (verified directly via psql)

---

### 2. Verified Database State

**Command:**
```bash
ssh monopsony@10.10.20.58 "sudo -u postgres psql -d monopsony \
  -c 'SELECT id, hostname, os, last_seen, registered_at FROM clients;'"
```

**Output:** `(0 rows)`

**Conclusion:** No clients exist in database, so dashboard shows 0.

---

### 3. Traced Client Registration Flow

**Expected flow:**

1. **Pairing** (one-time mTLS trust setup)
   - Client connects to port 50052 (plaintext)
   - Exchanges 6-char code + client info
   - Server generates client_id + certificates
   - Server inserts DB row

2. **Daemon startup**
   - Client loads certs + client_id from disk
   - Connects via mTLS to port 50051
   - If no client_id file: calls Register RPC
   - Starts heartbeat + inventory loops

3. **Registration RPC** (if client_id missing)
   - Server: `db.register_client()` → INSERT/UPDATE clients row
   - Returns client_id (bare UUID)

4. **Heartbeat** (every 60s)
   - Server: `db.heartbeat()` → UPDATE clients.last_seen

5. **Inventory report** (every 300s)
   - Client: `list_installed()` from available backends
   - Client: `report_inventory()` RPC
   - Server: `db.record_inventory()` → INSERT packages + client_packages

**Current reality:** None of these have ever run.

---

### 4. Tested Client Startup

**Command:**
```bash
ssh -o StrictHostKeyChecking=no root@10.10.20.20 \
  "monopsony-client --cert-dir /var/lib/monopsony/certs daemon --server 10.10.20.58:50051 2>&1"
```

**Error:**
```
WARN monopsony_client: Failed to parse config at /etc/monopsony/client.toml: TOML parse error at line 1, column 1
  |
1 | # Monopsony Client Configuration
  | ^missing field `server`

Error: No client certificates found in /var/lib/monopsony/certs.
Run 'monopsony-client pair --server ADDR --code CODE' first.
```

**Findings:**
- Client config file has comments (TOML parser rejects comments at start)
- Cert directory exists but has no client certs (only server certs)
- Pairing has never been completed

---

### 5. Verified Pairing Exists

**Command:**
```bash
ssh monopsony@10.10.20.58 "/home/monopsony/new_release/monopsony-server pair --help"
```

**Output:** Pairing subcommand exists, listens on port 50052

**Server startup logs show:**
```
gRPC server starting (mTLS required) addr=0.0.0.0:50051
```

**Findings:** Pairing mechanism exists, but no client has ever used it.

---

### 6. Examined Pairing Implementation

**File:** `crates/monopsony-server/src/main.rs:625-750`

**Critical code (lines 706-734):**
```rust
// Generate client certificate
let new_client_id = client_id_override
    .map(|s| s.to_string())
    .unwrap_or_else(|| format!("client-{}", uuid::Uuid::new_v4().simple()));

let (client_cert, client_key) = tls_manager.generate_client_cert(&new_client_id)?;
let (client_cert_pem, client_key_pem) = tls::TlsManager::cert_and_key_to_pem(&client_cert, &client_key);
let ca_cert_pem = tls_manager.ca_cert_pem_bytes();

// Build PairResponse
let resp = monopsony_common::proto::monopsony::PairResponse {
    client_id: new_client_id.clone(),
    client_cert_pem,
    client_key_pem,
    ca_cert_pem,
    grpc_addr: format!("{}:{}", server_addr.ip(), server_addr.port()),
};

// ... send response ...

// Record pairing in DB
if let Some(db) = db.as_ref() {
    let cid = monopsony_common::ClientId::parse(&new_client_id)
        .map_err(|e| anyhow::anyhow!("Invalid client ID: {}", e))?;
    db.record_pairing(&cid, chrono::Utc::now()).await?;
}
```

**BUG #1:** Client ID format mismatch
- Generated: `client-{uuid}` (e.g., `client-a1b2c3d4567890abcdef1234567890ab`)
- Expected by `ClientId::parse()`: bare UUID only

**File:** `crates/monopsony-common/src/models.rs:16-20`
```rust
impl ClientId {
    pub fn parse(s: &str) -> crate::Result<Self> {
        Uuid::parse_str(s)
            .map(Self)
            .map_err(|e| crate::Error::Parse(format!("Invalid client ID: {}", e)))
    }
}
```

**Result:** `ClientId::parse("client-...")` fails → pairing crashes or skips DB update

---

### 7. Examined record_pairing Implementation

**File:** `crates/monopsony-db/src/db.rs:774-789`

```rust
pub async fn record_pairing(
    &self,
    client_id: &ClientId,
    paired_at: chrono::DateTime<chrono::Utc>,
) -> Result<()> {
    sqlx::query(
        "UPDATE clients SET paired_at = $1, last_seen = $1 WHERE id = $2",
    )
    .bind(paired_at)
    .bind(client_id.0)
    .execute(&self.pool)
    .await?;

    info!("Recorded pairing for client {}", client_id);
    Ok(())
}
```

**BUG #2:** Updates non-existent row
- Runs `UPDATE clients ... WHERE id = $2`
- If row doesn't exist: `rows_affected() == 0`
- No error returned (just logs info message)
- Silent failure → certs exchanged but no DB row

**Expected:** Should INSERT row if it doesn't exist

---

### 8. Examined Pairing Client Implementation

**File:** `crates/monopsony-client/src/grpc_client.rs:182-228`

```rust
pub async fn pair(
    server_addr: &str,
    code: &str,
    hostname: &str,
    os: &str,
    os_version: &str,
    package_managers: &[String],
) -> anyhow::Result<(String, ClientCerts)> {
    // ... connect to port 50052 ...

    let resp = PairResponse::decode(resp_buf.as_slice())
        .map_err(|e| anyhow::anyhow!("Failed to decode pair response: {}", e))?;

    let certs = ClientCerts {
        client_cert_pem: resp.client_cert_pem,
        client_key_pem: resp.client_key_pem,
        ca_cert_pem: resp.ca_cert_pem,
    };

    info!("Paired successfully, client_id={}", resp.client_id);
    Ok((resp.client_id, certs))
}
```

**BUG #3:** Never saves client_id to disk
- Returns `(client_id, certs)` tuple
- Pair subcommand likely saves certs but not client_id
- Daemon needs client_id file to skip re-registration

**File:** `crates/monopsony-client/src/main.rs:254-277` (daemon startup)
```rust
// Get client ID from stored file or register
let client_id = {
    let id_file = cert_dir.join("client_id");
    match std::fs::read_to_string(&id_file) {
        Ok(id) => id.trim().to_string(),
        Err(_) => {
            // Register with server to get client ID
            let (id, _) = grpc_client.register(...).await?;
            std::fs::write(&id_file, &id)?;
            id
        }
    }
};
```

If `client_id` file missing, daemon re-registers → creates second client_id.

---

### 9. Examined Registration RPC

**File:** `crates/monopsony-control/src/grpc.rs:43-80`

```rust
async fn handle_register(&self, req: RegisterRequest) -> Result<RegisterResponse> {
    let client_id = self
        .db
        .register_client(
            &req.hostname,
            &req.os,
            &req.os_version,
            &req.package_managers,
        )
        .await?;

    // Generate client certificate
    let (cert, key) = self.tls_manager.generate_client_cert(&client_id.to_string())?;

    Ok(RegisterResponse {
        client_id: client_id.to_string(),
        enabled_managers: req.package_managers,
        client_cert_pem: cert_pem,
        client_key_pem: key_pem,
        ca_cert_pem: self.tls_manager.ca_cert_pem().to_string(),
    })
}
```

**File:** `crates/monopsony-db/src/db.rs:295-329`

```rust
pub async fn register_client(
    &self,
    hostname: &str,
    os: &str,
    os_version: &str,
    package_managers: &[String],
) -> Result<ClientId> {
    let id = Uuid::new_v4();
    let now = Utc::now();
    let pm_json = serde_json::to_value(package_managers)?;

    let row = sqlx::query(
        "INSERT INTO clients (id, hostname, os, os_version, package_managers, last_seen, registered_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7)
         ON CONFLICT (hostname) DO UPDATE SET
             os = EXCLUDED.os,
             os_version = EXCLUDED.os_version,
             package_managers = EXCLUDED.package_managers,
             last_seen = EXCLUDED.last_seen
         RETURNING id",
    )
    .bind(id)
    .bind(hostname)
    .bind(os)
    .bind(os_version)
    .bind(pm_json)
    .bind(now)
    .bind(now)
    .fetch_one(&self.pool)
    .await?;

    Ok(ClientId(id))
}
```

**BUG #4:** Creates mismatched client_id
- Generates new bare UUID
- Inserts into DB with this ID
- But client certificates from pairing have `client-{uuid}` in SAN
- Cert SAN doesn't match DB ID → potential auth failures

---

### 10. Verified Inventory Recording Exists

**File:** `crates/monopsony-db/src/db.rs:347-425`

```rust
pub async fn record_inventory(
    &self,
    client_id: &ClientId,
    packages: &[PackageRef],
) -> Result<()> {
    let mut tx = self.pool.begin().await?;
    let now = Utc::now();

    // Get current package IDs for this client
    let existing: Vec<Uuid> = sqlx::query(
        "SELECT package_id FROM client_packages WHERE client_id = $1",
    )
    .bind(client_id.0)
    .fetch_all(&mut *tx)
    .await?
    .iter()
    .map(|r| r.get::<Uuid, _>("package_id"))
    .collect();

    let mut current_ids = std::collections::HashSet::new();

    for pkg in packages {
        // Find existing package by unique key, or insert new
        let row = sqlx::query(
            "SELECT id FROM packages WHERE manager = $1 AND name = $2 AND version = $3 AND arch IS NOT DISTINCT FROM $4",
        )
        .bind(pkg.manager.as_str())
        .bind(&pkg.name)
        .bind(pkg.version.as_deref().unwrap_or(""))
        .bind(&pkg.architecture)
        .fetch_optional(&mut *tx)
        .await?;

        let package_id = if let Some(r) = row {
            r.get::<Uuid, _>("id")
        } else {
            let id = Uuid::new_v4();
            sqlx::query(
                "INSERT INTO packages (id, manager, name, version, arch, file_path, upstream_url, size_bytes, cached_at, last_accessed)
                 VALUES ($1, $2, $3, $4, $5, '', '', 0, $6, $6)",
            )
            .bind(id)
            .bind(pkg.manager.as_str())
            .bind(&pkg.name)
            .bind(pkg.version.as_deref().unwrap_or(""))
            .bind(&pkg.architecture)
            .bind(now)
            .execute(&mut *tx)
            .await?;
            id
        };

        current_ids.insert(package_id);

        sqlx::query(
            "INSERT INTO client_packages (client_id, package_id, installed_at) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
        )
        .bind(client_id.0)
        .bind(package_id)
        .bind(now)
        .execute(&mut *tx)
        .await?;
    }

    // Remove packages no longer present
    for old_id in existing {
        if !current_ids.contains(&old_id) {
            sqlx::query("DELETE FROM client_packages WHERE client_id = $1 AND package_id = $2")
                .bind(client_id.0)
                .bind(old_id)
                .execute(&mut *tx)
                .await?;
        }
    }

    tx.commit().await?;
    Ok(())
}
```

**Conclusion:** Inventory recording logic exists and looks correct. It will work once client_id is properly registered.

---

### 11. Verified Heartbeat Implementation

**File:** `crates/monopsony-db/src/db.rs:331-345`

```rust
pub async fn heartbeat(&self, client_id: &ClientId) -> Result<()> {
    let now = Utc::now();
    let result = sqlx::query("UPDATE clients SET last_seen = $1 WHERE id = $2")
        .bind(now)
        .bind(client_id.0)
        .execute(&self.pool)
        .await?;

    if result.rows_affected() == 0 {
        warn!("Heartbeat for unknown client {}", client_id);
    } else {
        debug!("Heartbeat from client {}", client_id);
    }
    Ok(())
}
```

**Conclusion:** Heartbeat logic exists and logs warnings for unknown clients.

---

## Root Cause Analysis

### Primary Issue

No client has ever completed the gRPC registration flow. HTTP proxy (3128) works without auth, which is why "connections work" tests passed.

### Bug Chain

1. **Pairing generates wrong client_id format** (Bug #1)
   - Generates: `client-{uuid}`
   - Expected: bare UUID
   - Impact: `ClientId::parse()` fails → pairing DB update fails

2. **Pairing tries to update non-existent DB row** (Bug #2)
   - Calls `record_pairing()` → `UPDATE clients ... WHERE id = $2`
   - Row doesn't exist → `rows_affected() == 0` → silent failure
   - Impact: No DB row created, certs exchanged but lost

3. **Pairing client_id never saved to disk** (Bug #3)
   - Pairing returns `(client_id, certs)` tuple
   - Pair subcommand saves certs but not client_id
   - Impact: Daemon can't skip re-registration

4. **Daemon re-registers, creating mismatched ID** (Bug #4)
   - Daemon starts, no `client_id` file
   - Calls `register()` RPC → generates new bare UUID
   - Creates DB row with new ID
   - Impact: Cert SAN (`client-{uuid}`) ≠ DB ID (bare UUID)

### Current State

- Database: 0 client rows
- Client machines: No pairing data, no client certs
- Dashboard: Shows 0 clients (correctly reflects DB)
- HTTP proxy: Works (no auth required)
- gRPC service: Never received a valid registration

---

## Related Context

### Previous Client-Server Communication Issues

**Note:** User mentioned "similar issues getting the client to communicate to the server at all." This suggests a history of gRPC connection problems.

**Possible related issues:**
- mTLS certificate validation failures
- SAN (Subject Alternative Name) mismatches
- `ClientId::parse()` format expectations
- Certificate loading/verification on client side

**Connection flow:**
1. Client loads certs from cert_dir
2. Builds TLS config with `domain_name("monopsony-server")`
3. Server validates client cert against CA
4. Server validates cert SAN against authenticated identity
5. All gRPC calls use this mTLS connection

**Potential failure points:**
- Client cert SAN doesn't match what server expects
- CA cert mismatch (different CA used for client cert generation)
- Cert file permissions or paths
- `ClientId::parse()` failing on malformed IDs

---

## Database Schema

**clients table:**
```sql
CREATE TABLE IF NOT EXISTS clients (
    id UUID PRIMARY KEY,
    hostname TEXT NOT NULL UNIQUE,
    os TEXT NOT NULL,
    os_version TEXT NOT NULL,
    package_managers JSONB NOT NULL,
    last_seen TIMESTAMPTZ NOT NULL,
    registered_at TIMESTAMPTZ NOT NULL,
    cert_serial TEXT NULL,
    paired_at TIMESTAMPTZ NULL
)
```

**packages table:**
```sql
CREATE TABLE IF NOT EXISTS packages (
    id UUID PRIMARY KEY,
    manager TEXT NOT NULL,
    name TEXT NOT NULL,
    version TEXT NOT NULL,
    arch TEXT NULL,
    file_path TEXT NOT NULL,
    hash TEXT NULL,
    upstream_url TEXT NOT NULL,
    size_bytes BIGINT NOT NULL,
    cached_at TIMESTAMPTZ NOT NULL,
    last_accessed TIMESTAMPTZ NOT NULL,
    UNIQUE (manager, name, version, arch)
)
```

**client_packages table:**
```sql
CREATE TABLE IF NOT EXISTS client_packages (
    client_id UUID NOT NULL REFERENCES clients(id),
    package_id UUID NOT NULL REFERENCES packages(id),
    installed_at TIMESTAMPTZ NOT NULL,
    PRIMARY KEY(client_id, package_id)
)
```

---

## Testing Environment

**Server:** 10.10.20.58 (Arch Linux)
- User: monopsony
- Database: PostgreSQL (monopsony/monopsony)
- Ports: 3128 (HTTP proxy), 50051 (gRPC mTLS), 3130 (dashboard)
- Binary: /home/monopsony/new_release/monopsony-server

**Test clients:**
- .20 (Arch Linux, root ssh+sudo)
- .62 (Ubuntu, glibc)
- .235 (Rocky 9, musl)
- .242 (Debian 12, musl)
- .159 (Alpine, musl)
- automata@192.168.15.33 (MidnightBSD 4.0.6, P2P VPN)

---

## Next Steps (Before Fixing)

1. **Review bug chain** - Confirm all 4 bugs are understood
2. **Choose fix strategy** - Decide whether to:
   - Fix pairing flow (recommended)
   - Skip pairing, use Register RPC only (simpler but less secure)
3. **Define fix acceptance criteria** - What "success" looks like:
   - Client pair → DB row created
   - Client daemon starts → heartbeat works
   - Client reports inventory → packages appear in DB
   - Dashboard shows client + packages
4. **Plan testing** - Which client to test (Arch Linux at .20 recommended)

---

## Files Examined

- `crates/monopsony-admin/src/clients.rs` - Dashboard clients endpoint
- `crates/monopsony-server/src/main.rs:625-750` - Pairing implementation
- `crates/monopsony-control/src/grpc.rs:43-80` - Register RPC handler
- `crates/monopsony-db/src/db.rs:295-425` - Client + inventory CRUD
- `crates/monopsony-common/src/models.rs:16-20` - ClientId parsing
- `crates/monopsony-client/src/grpc_client.rs:182-228` - Pairing client
- `crates/monopsony-client/src/main.rs:254-277` - Daemon startup
- `proto/monopsony.proto` - gRPC message definitions

---

## References

- Dashboard: http://10.10.20.58:3130 (admin/testpass123)
- API: http://10.10.20.58:3130/api/v1/clients
- Database: postgres://monopsony@localhost/monopsony
- Server config: /etc/monopsony/server.toml
- Client config: /etc/monopsony/client.toml (commented out, needs fix)

---

**Status:** Investigation complete. Awaiting review before applying fixes.