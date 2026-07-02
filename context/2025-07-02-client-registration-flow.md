# Client Registration and Package Reporting Flow

**Version:** v1.0.0-rc1
**Purpose:** Complete end-to-end flow documentation

---

## Overview

Monopsony clients go through a 3-phase lifecycle:

1. **Pairing** - One-time mTLS trust establishment
2. **Registration** - Initial gRPC registration (may be skipped if paired)
3. **Runtime** - Heartbeat + inventory reporting loops

---

## Phase 1: Pairing (mTLS Trust Establishment)

### Purpose

Establish mutual TLS trust between client and server without requiring pre-shared certificates. This is the **recommended** first-time setup.

### Prerequisites

- Server running on `<server-addr>:50051` (gRPC mTLS)
- Server can listen on `<server-addr>:50052` (plaintext pairing port)
- Server has CA certificate and key (`/var/cache/monopsony/tls/` by default)

### Server-Side Commands

**Start pairing mode:**
```bash
# On server (10.10.20.58)
/home/monopsony/new_release/monopsony-server pair --server 10.10.20.58:50051
```

**Output:**
```
Server pairing code: ABC123
Share this code with the client. Valid for 5 minutes.
Pairing listener on 0.0.0.0:50052 (5-minute timeout)
```

**Server implementation:**
```rust
// File: crates/monopsony-server/src/main.rs:625-750

async fn handle_pair_command(...) -> anyhow::Result<()> {
    // Generate 6-char alphanumeric pairing code
    let charset: Vec<u8> = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789".to_vec();
    let mut random_bytes = [0u8; 6];
    getrandom::getrandom(&mut random_bytes)?;
    let pairing_code: String = random_bytes.iter()
        .map(|&b| charset[b as usize % charset.len()] as char)
        .collect();

    // Listen on port 50052 (plaintext)
    let pair_addr: SocketAddr = "0.0.0.0:50052".parse()?;
    let pair_listener = tokio::net::TcpListener::bind(pair_addr).await?;

    // Wait 5 minutes for client connection
    let timeout = tokio::time::Instant::now() + tokio::time::Duration::from_secs(300);

    tokio::select! {
        result = pair_listener.accept() => {
            let (mut socket, addr) = result?;

            // Read PairRequest (length-prefixed protobuf)
            let req = PairRequest::decode(...)?;

            // Validate code
            if req.code != pairing_code {
                return Err(anyhow::anyhow!("Invalid pairing code"));
            }

            // GENERATE CLIENT ID - CRITICAL: Use bare UUID
            let new_client_id = uuid::Uuid::new_v4().to_string();

            // Generate client certificate with SAN=client_id
            let (client_cert, client_key) = tls_manager.generate_client_cert(&new_client_id)?;

            // CREATE CLIENT ROW IN DATABASE - CRITICAL
            if let Some(db) = db.as_ref() {
                let cid = ClientId::parse(&new_client_id)?;
                db.register_client(
                    &req.hostname,
                    &req.os,
                    &req.os_version,
                    &req.package_managers,
                ).await?;
                db.record_pairing(&cid, chrono::Utc::now()).await?;
            }

            // Build PairResponse
            let resp = PairResponse {
                client_id: new_client_id.clone(),
                client_cert_pem: ...,
                client_key_pem: ...,
                ca_cert_pem: ...,
                grpc_addr: format!("{}:{}", server_addr.ip(), server_addr.port()),
            };

            // Send response (length-prefixed protobuf)
            socket.write_all(&len.to_le_bytes()).await?;
            socket.write_all(&resp_bytes).await?;

            Ok(())
        }
        _ = tokio::time::sleep_until(timeout) => {
            Err(anyhow::anyhow!("Pairing timed out (5 minutes)"))
        }
    }
}
```

### Client-Side Commands

**Pair with server:**
```bash
# On client (10.10.20.20)
monopsony-client --cert-dir /var/lib/monopsony/certs \
  pair --server 10.10.20.58:50051 --code ABC123
```

**Expected output:**
```
2025-07-02T10:30:00Z INFO monopsony_client: Pairing with server: 10.10.20.58:50051
2025-07-02T10:30:01Z INFO monopsony_client: Paired successfully, client_id=550e8400-e29b-41d4-a716-446655440000
Pairing complete! Certificates saved to /var/lib/monopsony/certs/
Client ID: 550e8400-e29b-41d4-a716-446655440000
```

**Client implementation:**
```rust
// File: crates/monopsony-client/src/grpc_client.rs:182-228

pub async fn pair(
    server_addr: &str,
    code: &str,
    hostname: &str,
    os: &str,
    os_version: &str,
    package_managers: &[String],
) -> anyhow::Result<(String, ClientCerts)> {
    // Connect to port 50052 (plaintext)
    let mut socket = tokio::net::TcpStream::connect(format!("{}:50052", server_addr)).await?;

    // Build PairRequest
    let req = PairRequest {
        code: code.to_string(),
        hostname: hostname.to_string(),
        os: os.to_string(),
        os_version: os_version.to_string(),
        package_managers: package_managers.to_vec(),
    };

    // Send request (length-prefixed protobuf)
    socket.write_all(&len.to_le_bytes()).await?;
    socket.write_all(&req_bytes).await?;

    // Read PairResponse (length-prefixed protobuf)
    socket.read_exact(&mut len_buf).await?;
    let resp = PairResponse::decode(resp_buf.as_slice())?;

    // EXTRACT AND SAVE CERTS - CRITICAL
    let certs = ClientCerts {
        client_cert_pem: resp.client_cert_pem,
        client_key_pem: resp.client_key_pem,
        ca_cert_pem: resp.ca_cert_pem,
    };

    // SAVE CLIENT ID TO FILE - CRITICAL
    let client_id = resp.client_id.clone();

    info!("Paired successfully, client_id={}", client_id);
    Ok((client_id, certs))
}
```

**Pair subcommand handler:**
```rust
// File: crates/monopsony-client/src/main.rs

Commands::Pair { server, code } => {
    let hostname = hostname::get()?.to_string_lossy().to_string();
    let backends = monopsony_common::package_manager::get_available_backends();
    let managers: Vec<String> = backends.iter().map(|b| b.kind().as_str().to_string()).collect();

    let (client_id, certs) = grpc_client::pair(
        &server,
        &code,
        &hostname,
        std::env::consts::OS,
        std::env::consts::ARCH,
        &managers,
    ).await?;

    // SAVE CERTS TO DISK
    std::fs::create_dir_all(&cert_dir)?;
    std::fs::write(cert_dir.join("client.crt"), &certs.client_cert_pem)?;
    std::fs::write(cert_dir.join("client.key"), &certs.client_key_pem)?;
    std::fs::write(cert_dir.join("ca.crt"), &certs.ca_cert_pem)?;

    // SAVE CLIENT ID TO FILE - CRITICAL
    std::fs::write(cert_dir.join("client_id"), &client_id)?;

    println!("Pairing complete! Certificates saved to {}", cert_dir.display());
    println!("Client ID: {}", client_id);
}
```

### Files Created on Client

```
/var/lib/monopsony/certs/
├── client.crt     # Client certificate (SAN=client_id)
├── client.key     # Client private key
├── ca.crt         # CA certificate for server verification
└── client_id      # Text file containing bare UUID client_id
```

### Database State After Pairing

```sql
SELECT id, hostname, os, last_seen, registered_at, paired_at FROM clients;
```

**Output:**
```
id                                   | hostname  | os      | last_seen           | registered_at        | paired_at
-------------------------------------+-----------+---------+---------------------+----------------------+--------------------
550e8400-e29b-41d4-a716-446655440000 | archtest  | linux   | 2025-07-02 10:30:01 | 2025-07-02 10:30:01 | 2025-07-02 10:30:01
```

---

## Phase 2: Registration (Initial gRPC Registration)

### Purpose

First-time gRPC connection to exchange metadata and confirm client identity. **Skipped** if `client_id` file exists from pairing.

### When This Runs

- Client daemon starts without `client_id` file
- Client has certificates but never called pairing

### Server-Side Implementation

```rust
// File: crates/monopsony-control/src/grpc.rs:43-80

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
    let (cert_pem, key_pem) = TlsManager::cert_and_key_to_pem(&cert, &key);

    Ok(RegisterResponse {
        client_id: client_id.to_string(),
        enabled_managers: req.package_managers,
        client_cert_pem: cert_pem,
        client_key_pem: key_pem,
        ca_cert_pem: self.tls_manager.ca_cert_pem().to_string(),
    })
}
```

```rust
// File: crates/monopsony-db/src/db.rs:295-329

pub async fn register_client(
    &self,
    hostname: &str,
    os: &str,
    os_version: &str,
    package_managers: &[String],
) -> Result<ClientId> {
    let id = Uuid::new_v4();  // Generate bare UUID
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

**Note:** Uses `ON CONFLICT (hostname)` to handle re-registrations.

### Client-Side Implementation

```rust
// File: crates/monopsony-client/src/main.rs:254-277

let client_id = {
    let id_file = cert_dir.join("client_id");
    match std::fs::read_to_string(&id_file) {
        Ok(id) => id.trim().to_string(),  // SKIP registration
        Err(_) => {
            // Register with server to get client ID
            let (id, _) = grpc_client.register(
                &hostname,
                std::env::consts::OS,
                std::env::consts::ARCH,
                &managers,
            ).await?;
            std::fs::write(&id_file, &id)?;
            id
        }
    }
};
```

---

## Phase 3: Runtime (Heartbeat + Inventory Reporting)

### Purpose

Ongoing synchronization between client and server.

### Daemon Startup

```bash
# On client
monopsony-client --cert-dir /var/lib/monopsony/certs daemon --server 10.10.20.58:50051
```

**Expected output:**
```
2025-07-02T10:35:00Z INFO monopsony_client: Starting Monopsony Client v1.0.0-rc1
2025-07-02T10:35:00Z INFO monopsony_client: Loading certificates from /var/lib/monopsony/certs
2025-07-02T10:35:01Z INFO monopsony_client: gRPC connection established
2025-07-02T10:35:01Z INFO monopsony_client: Client ID from file: 550e8400-e29b-41d4-a716-446655440000
2025-07-02T10:35:01Z INFO monopsony_client: Connected to server
2025-07-02T10:35:01Z INFO monopsony_client: Local proxy listening on 127.0.0.1:3129
2025-07-02T10:35:01Z INFO monopsony_client: Heartbeat and inventory sync running in background
2025-07-02T10:36:01Z INFO monopsony_client: Heartbeat sent
2025-07-02T10:40:01Z INFO monopsony_client: Inventory reported (473 packages)
```

### Background Loops

**File:** `crates/monopsony-client/src/main.rs:280-369`

```rust
tokio::spawn(async move {
    let mut last_inventory = std::time::Instant::now();
    let mut backoff: std::time::Duration = Duration::from_secs(1);
    const MAX_BACKOFF: Duration = Duration::from_secs(60);
    const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(60);
    const INVENTORY_INTERVAL: Duration = Duration::from_secs(300);

    loop {
        // Attempt connection with exponential backoff
        let certs = match grpc_client::ClientCerts::load(&cert_dir_for_task) {
            Some(c) => c,
            None => {
                warn!("No certificates available for heartbeat");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
                continue;
            }
        };

        let tls = tonic::transport::ClientTlsConfig::new()
            .ca_certificate(tonic::transport::Certificate::from_pem(&certs.ca_cert_pem))
            .identity(tonic::transport::Identity::from_pem(
                &certs.client_cert_pem,
                &certs.client_key_pem,
            ))
            .domain_name("monopsony-server")
            .assume_http2(true);

        let mut client = match grpc_client::MonopsonyGrpcClient::connect(&server_addr_for_task, Some(tls)).await {
            Ok(c) => {
                info!("Connected to server");
                backoff = Duration::from_secs(1); // reset on success
                c
            }
            Err(e) => {
                warn!("Connection failed (retry in {:?}): {}", backoff, e);
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
                continue;
            }
        };

        // Connected — run heartbeat loop
        let mut interval = tokio::time::interval(HEARTBEAT_INTERVAL);

        loop {
            interval.tick().await;

            // HEARTBEAT (every 60s)
            match client.heartbeat(&client_id_for_task).await {
                Ok(_) => {
                    info!("Heartbeat sent");
                }
                Err(e) => {
                    warn!("Heartbeat failed, reconnecting: {}", e);
                    break; // break inner loop → retry connection
                }
            }

            // INVENTORY REPORT (every 300s)
            if last_inventory.elapsed() > INVENTORY_INTERVAL {
                let backends = monopsony_common::package_manager::get_available_backends();
                let mut all_packages = Vec::new();
                for backend in &backends {
                    match backend.list_installed().await {
                        Ok(pkgs) => all_packages.extend(pkgs),
                        Err(e) => warn!("Failed to list packages from {:?}: {}", backend.kind(), e),
                    }
                }

                match client.report_inventory(&client_id_for_task, &all_packages, true).await {
                    Ok(_) => info!("Inventory reported ({} packages)", all_packages.len()),
                    Err(e) => {
                        warn!("Inventory report failed, reconnecting: {}", e);
                        break;
                    }
                }
                last_inventory = std::time::Instant::now();
            }
        }

        // Outer loop: backoff before retrying connection
        warn!("Disconnected (retry in {:?})", backoff);
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
});
```

### Heartbeat RPC

**Client:**
```rust
// File: crates/monopsony-client/src/grpc_client.rs:124-137

pub async fn heartbeat(&mut self, client_id: &str) -> anyhow::Result<i64> {
    let req = HeartbeatRequest {
        client_id: client_id.to_string(),
        timestamp: chrono::Utc::now().timestamp(),
    };

    debug!(client_id = client_id, "Sending heartbeat");

    let resp = self.client.heartbeat(req).await?;
    let server_time = resp.into_inner().server_time;
    debug!(server_time = server_time, "Heartbeat acknowledged");

    Ok(server_time)
}
```

**Server:**
```rust
// File: crates/monopsony-control/src/grpc.rs:82-97

async fn handle_heartbeat(&self, req: HeartbeatRequest) -> Result<HeartbeatResponse> {
    debug!(client_id = %req.client_id, "Handling heartbeat");

    let client_id = ClientId::parse(&req.client_id)
        .context("Invalid client ID in heartbeat request")?;

    self.db.heartbeat(&client_id).await?;

    Ok(HeartbeatResponse {
        server_time: chrono::Utc::now().timestamp(),
    })
}
```

```rust
// File: crates/monopsony-db/src/db.rs:331-345

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

### Inventory Report RPC

**Client:**
```rust
// File: crates/monopsony-client/src/grpc_client.rs:139-164

pub async fn report_inventory(
    &mut self,
    client_id: &str,
    packages: &[PackageRef],
    full_sync: bool,
) -> anyhow::Result<()> {
    let entries: Vec<PackageEntry> = packages
        .iter()
        .map(|p| PackageEntry {
            manager: p.manager.as_str().to_string(),
            name: p.name.clone(),
            version: p.version.clone().unwrap_or_default(),
            arch: p.architecture.clone().unwrap_or_default(),
        })
        .collect();

    let report = InventoryReport {
        client_id: client_id.to_string(),
        packages: entries.clone(),
        full_sync,
    };

    debug!("Reporting {} packages (full_sync={})", packages.len(), full_sync);

    self.client.report_inventory(report).await?;
    Ok(())
}
```

**Server:**
```rust
// File: crates/monopsony-control/src/grpc.rs:99-157

async fn handle_inventory(&self, req: InventoryReport) -> Result<InventoryResponse> {
    info!(
        client_id = %req.client_id,
        package_count = req.packages.len(),
        full_sync = req.full_sync,
        "Handling inventory report"
    );

    let client_id = ClientId::parse(&req.client_id)
        .context("Invalid client ID in inventory report")?;

    let packages: Vec<PackageRef> = req
        .packages
        .iter()
        .filter_map(|entry| {
            let manager = match PackageManagerKind::parse_kind(&entry.manager) {
                Some(m) => m,
                None => {
                    warn!(
                        manager = %entry.manager,
                        "Unknown package manager in inventory, skipping entry"
                    );
                    return None;
                }
            };
            Some(PackageRef {
                name: entry.name.clone(),
                version: if entry.version.is_empty() {
                    None
                } else {
                    Some(entry.version.clone())
                },
                architecture: if entry.arch.is_empty() {
                    None
                } else {
                    Some(entry.arch.clone())
                },
                manager,
            })
        })
        .collect();

    debug!(
        client_id = %client_id,
        mapped_packages = packages.len(),
        "Recording inventory"
    );

    self.db.record_inventory(&client_id, &packages).await?;

    Ok(InventoryResponse {
        accepted: true,
        message: format!("Recorded {} packages", packages.len()),
    })
}
```

```rust
// File: crates/monopsony-db/src/db.rs:347-425

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

---

## Dashboard Queries

### List Clients

**Endpoint:** `GET /api/v1/clients`

**Implementation:**
```rust
// File: crates/monopsony-admin/src/clients.rs:8-28

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

**Example output:**
```json
[
  {
    "id": "550e8400-e29b-41d4-a716-446655440000",
    "hostname": "archtest",
    "os": "linux",
    "registered_at": "2025-07-02T10:30:01Z",
    "last_seen": "2025-07-02T10:40:01Z"
  }
]
```

### Get Client Inventory

**Endpoint:** `GET /api/v1/clients/:id/inventory`

**Implementation:**
```rust
// File: crates/monopsony-admin/src/clients.rs:58-85

pub async fn handle_client_inventory(
    State(state): State<AdminState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if let Some(ref db) = state.db {
        match monopsony_common::ClientId::parse(&id) {
            Ok(client_id) => {
                match db.get_packages_for_client(&client_id).await {
                    Ok(packages) => {
                        let result: Vec<serde_json::Value> = packages.iter().map(|p| {
                            serde_json::json!({
                                "name": p.name,
                                "version": p.version,
                            })
                        }).collect();
                        Json(serde_json::json!({"packages": result}))
                    }
                    Err(e) => Json(serde_json::json!({"error": e.to_string()})),
                }
            }
            Err(e) => Json(serde_json::json!({"error": format!("Invalid client ID: {}", e)})),
        }
    } else {
        Json(serde_json::json!({"error": "Database not available"}))
    }
}
```

**Database query:**
```sql
SELECT p.name, p.version
FROM client_packages cp
JOIN packages p ON p.id = cp.package_id
WHERE cp.client_id = $1
ORDER BY p.name
```

**Example output:**
```json
{
  "packages": [
    {"name": "bash", "version": "5.2.026-1"},
    {"name": "coreutils", "version": "9.5-1"},
    {"name": "pacman", "version": "6.1.0-1"},
    ...
  ]
}
```

---

## Troubleshooting

### No Clients in Dashboard

**Symptom:** Dashboard shows 0 clients

**Diagnosis:**
```bash
# Check database directly
ssh monopsony@10.10.20.58 "sudo -u postgres psql -d monopsony \
  -c 'SELECT id, hostname, os, last_seen, registered_at FROM clients;'"
```

**Possible causes:**
1. Pairing never completed → No certs generated
2. Pairing completed but DB update failed (format mismatch) → Certs exist but no DB row
3. Daemon never started → No heartbeat/inventory
4. Client ID file missing → Daemon re-registered with different ID

**Remediation:**
```bash
# On client: Check if pairing was completed
ls -la /var/lib/monopsony/certs/
cat /var/lib/monopsony/certs/client_id  # Should contain bare UUID

# If missing, re-pair:
# 1. On server: monopsony-server pair --server <server>:50051
# 2. On client: monopsony-client pair --server <server>:50051 --code <CODE>
# 3. Start daemon: monopsony-client daemon --server <server>:50051
```

### Heartbeat Fails

**Symptom:** Client logs "Heartbeat failed, reconnecting"

**Diagnosis:**
```bash
# Check server logs
ssh monopsony@10.10.20.58 "tail -100 /tmp/monopsony-server.log | grep -i heartbeat"

# Check client certs
ssh root@10.10.20.20 "openssl x509 -in /var/lib/monopsony/certs/client.crt -text -noout | grep -A1 'Subject Alternative Name'"
```

**Possible causes:**
1. Cert SAN doesn't match DB client_id
2. CA cert mismatch between server and client
3. Network connectivity issue
4. Server not listening on 50051

### Inventory Not Reported

**Symptom:** Dashboard shows client but 0 packages

**Diagnosis:**
```bash
# Check client logs for inventory errors
ssh root@10.10.20.20 "journalctl -u monopsony-client -n 100 | grep -i inventory"

# Check package manager backend
ssh root@10.10.20.20 "pacman -Q | head -10"

# Check database for packages
ssh monopsony@10.10.20.58 "sudo -u postgres psql -d monopsony \
  -c \"SELECT COUNT(*) FROM client_packages;\""
```

**Possible causes:**
1. `get_available_backends()` returns empty list
2. `list_installed()` fails for all backends
3. `report_inventory()` RPC fails
4. `record_inventory()` fails (client_id mismatch)

---

**Status:** Flow documented, awaiting review before bug fixes.