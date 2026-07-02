# Client Registration Bug Report

## Root Cause

Dashboard shows zero clients despite "connections working" because:
1. HTTP proxy (3128) works without gRPC auth
2. No client has ever completed the gRPC registration flow

## Bug Chain

### Bug 1: Pairing generates wrong client_id format
**File:** `crates/monopsony-server/src/main.rs:708`

**Current:**
```rust
let new_client_id = format!("client-{}", uuid::Uuid::new_v4().simple());
```

**Fix:**
```rust
let new_client_id = uuid::Uuid::new_v4().to_string();
```

**Impact:** `ClientId::parse()` expects bare UUID, not `client-` prefix. Format mismatch causes all subsequent client_id lookups to fail.

### Bug 2: Pairing updates non-existent DB row
**File:** `crates/monopsony-server/src/main.rs:729-734`

**Current:**
```rust
// Record pairing in DB
if let Some(db) = db.as_ref() {
    let cid = ClientId::parse(&new_client_id).map_err(...)?;
    db.record_pairing(&cid, chrono::Utc::now()).await?;
}
```

`db.record_pairing()` runs:
```sql
UPDATE clients SET paired_at = $1, last_seen = $1 WHERE id = $2
```

**Result:** Matches 0 rows → silently fails (no error logged)

**Fix:** Create row instead of updating non-existent row:
```rust
// Create client row during pairing
if let Some(db) = db.as_ref() {
    let cid = ClientId::parse(&new_client_id).map_err(...)?;
    db.register_client(
        &req.hostname,
        &req.os,
        &req.os_version,
        &req.package_managers,
    ).await?;
    db.record_pairing(&cid, chrono::Utc::now()).await?;
}
```

### Bug 3: Pairing client_id never saved
**File:** `crates/monopsony-client/src/main.rs` (Pair subcommand handler)

**Current:** Pairing saves certs but not client_id

**Fix:** Save client_id to cert_dir after pairing:
```rust
let (client_id, certs) = pair(...).await?;
// Save certs
std::fs::write(cert_dir.join("client.crt"), &certs.client_cert_pem)?;
std::fs::write(cert_dir.join("client.key"), &certs.client_key_pem)?;
std::fs::write(cert_dir.join("ca.crt"), &certs.ca_cert_pem)?;
// Save client_id
std::fs::write(cert_dir.join("client_id"), &client_id)?;
```

### Bug 4: Registration ID mismatches pairing ID
**File:** `crates/monopsony-client/src/main.rs:254-277`

**Current:** Daemon checks for client_id file, doesn't find it, re-registers

**Result:** Creates second client_id (bare UUID via Register RPC) while certs have first client_id (client-{uuid}) in SAN

**Impact:** Cert SAN doesn't match registered client_id, may cause auth issues

**Fix:** After Bug 3 fix, client_id file exists → daemon skips re-registration

---

## Correct Flow (After Fixes)

### 1. Pairing
```bash
# Server side
monopsony-server pair --server 10.10.20.58:50051
# Generates 6-char code, listens on 50052

# Client side
monopsony-client --cert-dir /var/lib/monopsony/certs \
  pair --server 10.10.20.58:50051 --code ABC123
```

**Server actions:**
1. Validate pairing code
2. Generate client_id = bare UUID
3. Create client cert with SAN=client_id
4. INSERT into clients table (hostname, os, package_managers, registered_at, last_seen)
5. UPDATE paired_at
6. Send client_id + certs to client

**Client actions:**
1. Save client.crt, client.key, ca.crt
2. Save client_id to file
3. Print success message

### 2. Daemon Startup
```bash
monopsony-client --cert-dir /var/lib/monopsony/certs \
  daemon --server 10.10.20.58:50051
```

**Client actions:**
1. Load certs and client_id from cert_dir
2. Connect via mTLS to 50051 (cert SAN matches client_id)
3. Check client_id file: exists, skip registration
4. Start heartbeat loop (every 60s):
   - Send HeartbeatRequest(client_id, timestamp)
   - Server UPDATE clients.last_seen
5. Start inventory loop (every 300s):
   - Get available backends (pacman, etc.)
   - Call `backend.list_installed()` for each
   - Call `report_inventory(client_id, packages, full_sync=true)`
   - Server calls `db.record_inventory()`:
     - INSERT/UPDATE packages table
     - INSERT into client_packages table
     - DELETE removed packages from client_packages

### 3. Dashboard Query
```
GET /api/v1/clients
→ db.list_clients()
→ SELECT id, hostname, os, last_seen, registered_at FROM clients
```

**Result:** Shows registered clients with package inventory

---

## Testing Steps

### Fix bugs first, then test:

```bash
# On server (10.10.20.58)
ssh monopsony@10.10.20.58
cd ~/projects/monopsony
# Apply fixes
# Rebuild server
cargo build -p monopsony-server --release
# Stop old server
pkill monopsony-server
# Start new server
./target/release/monopsony-server --config /etc/monopsony/server.toml \
  --database-url postgres://monopsony:***@localhost/monopsony

# Generate pairing code
/home/monopsony/new_release/monopsony-server pair --server 10.10.20.58:50051
# Note the 6-char code (e.g., ABC123)

# On client (10.10.20.20)
ssh root@10.10.20.20
# Copy new client binary
# Pair
monopsony-client --cert-dir /var/lib/monopsony/certs \
  pair --server 10.10.20.58:50051 --code ABC123
# Verify files saved
ls -la /var/lib/monopsony/certs/
cat /var/lib/monopsony/certs/client_id

# Start daemon
monopsony-client --cert-dir /var/lib/monopsony/certs \
  daemon --server 10.10.20.58:50051 &
# Wait 5 minutes for inventory report

# Check dashboard
curl -u admin:testpass123 http://10.10.20.58:3130/api/v1/clients

# Check database
ssh monopsony@10.10.20.58 "sudo -u postgres psql -d monopsony \
  -c 'SELECT id, hostname, os, last_seen, registered_at FROM clients;'

# Check inventory
ssh monopsony@10.10.20.58 "sudo -u postgres psql -d monopsony \
  -c \"SELECT c.hostname, p.name, p.version FROM client_packages cp
        JOIN clients c ON c.id = cp.client_id
        JOIN packages p ON p.id = cp.package_id
        WHERE c.hostname = 'archtest';
  \""
```

---

## Database Schema Verification

```sql
-- Verify clients table has correct columns
\d clients
-- Should show: id (UUID PK), hostname, os, os_version, package_managers (JSONB),
-- last_seen (TIMESTAMPTZ), registered_at (TIMESTAMPTZ), cert_serial (TEXT), paired_at (TIMESTAMPTZ)

-- Check for client rows
SELECT id, hostname, os, last_seen, registered_at FROM clients;
-- Should show at least 1 row after successful pairing

-- Check package inventory
SELECT c.hostname, COUNT(cp.package_id) as package_count
FROM clients c
LEFT JOIN client_packages cp ON c.id = cp.client_id
GROUP BY c.hostname;
```

---

## Priority

**P0 - Critical:**
- Bug 1 (client_id format)
- Bug 2 (INSERT vs UPDATE in pairing)
- Bug 3 (save client_id after pairing)

**P1 - High:**
- Bug 4 (re-registration logic, auto-fixed after P0)
- Add error logging to `db.record_pairing()` on 0 rows affected