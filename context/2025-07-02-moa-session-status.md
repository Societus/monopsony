# MoA Session Status - July 2, 2025

**Commit:** `0a531d1` - "fix: client registration bugs (partial WIP)"
**Session Type:** Mixture of Agents (OmenLM + Nvidia NIM + zai GLM-5.2)
**Status:** Partial fix committed, testing blocked by incomplete fixes

---

## Changes Committed

### Bug 1: Client ID Format ✅ FIXED

**File:** `crates/monopsony-server/src/main.rs`

**Before:**
```rust
let new_client_id = format!("client-{}", uuid::Uuid::new_v4().simple());
```

**After:**
```rust
let new_client_id = uuid::Uuid::new_v4().simple().to_string();
```

**Impact:** Client IDs are now bare UUIDs instead of `client-{uuid}` format, matching what `ClientId::parse()` expects.

---

### Bug 2: DB Row Creation ⚠️ PARTIALLY FIXED

**File:** `crates/monopsony-server/src/main.rs`

**Changes made:**
1. Parse client_id early with `ClientId::parse(&new_client_id)`
2. Call `db.record_pairing(&cid, chrono::Utc::now())` before sending response

**Remaining issue:**
- `record_pairing()` still uses `UPDATE` instead of `INSERT`
- First pairing will fail silently (0 rows affected)
- Second pairing would succeed if row existed from elsewhere

**Code:**
```rust
// Record pairing in DB BEFORE sending response — ensures the
// DB row exists before the client receives its credentials.
if let Some(db) = db.as_ref() {
    db.record_pairing(&cid, chrono::Utc::now()).await?;
}
```

**Should be:**
```rust
// Create client row first, then record pairing
if let Some(db) = db.as_ref() {
    db.register_client(
        &req.hostname,
        &req.os,
        &req.os_version,
        &req.package_managers,
    ).await?;
    db.record_pairing(&cid, chrono::Utc::now()).await?;
}
```

---

### Bug 3: Client ID File Not Saved ❌ NOT FIXED

**File:** `crates/monopsony-client/src/main.rs` (Pair subcommand handler)

**Status:** No changes made

**Impact:**
- Pairing saves certs but not `client_id` to disk
- Daemon will skip re-registration only if `client_id` file exists
- Without this file, daemon re-registers, creating second client_id
- Cert SAN won't match registered client_id

**Required change:**
```rust
// After saving certs
std::fs::write(cert_dir.join("client_id"), &client_id)?;
```

---

### Bug 4: Re-registration Mismatch ❌ BLOCKED

**Status:** Blocked by Bug 3

Once Bug 3 is fixed, daemon will skip re-registration because `client_id` file will exist.

---

## Additional Changes Committed

### TLS Crypto Provider Fix

**Files:**
- `crates/monopsony-server/src/main.rs`
- `crates/monopsony-client/src/main.rs`

**Change:**
```rust
rustls::crypto::ring::default_provider()
    .install_default()
    .expect("Failed to install rustls crypto provider");
```

**Impact:** Fixes "CryptoProvider already initialized" panic when using `ring` provider.

---

### Dashboard Wiring

**File:** `crates/monopsony-server/src/main.rs`

**Changes:**
- Added `--dashboard-addr` argument (default: `0.0.0.0:3130`)
- Imported `AdminState` and `DashboardConfig` from `monopsony_admin`
- Wired dashboard into server startup

---

### Shadow Packages Support

**File:** `crates/monopsony-server/src/main.rs`

**Changes:**
- Added `--shadow-dir`, `--shadow-git-mirror-dir`, `--github-token`, `--shadow-poll-interval` arguments
- Imported `ShadowPackageManager` from `monopsony_shadow`

---

### MidnightBSD Support

**Files:**
- `crates/monopsony-common/src/package_manager/mport.rs`
- `crates/monopsony-common/src/package_manager/bsd_privilege.rs`

**Status:** New package manager backend for MidnightBSD

---

## Current Database State

```sql
-- Verified on 10.10.20.58
SELECT COUNT(*) FROM clients;  -- Returns: 0
SELECT COUNT(*) FROM packages; -- Returns: 0
SELECT COUNT(*) FROM client_packages; -- Returns: 0
```

**Dashboard:** Shows 0 clients (correctly reflecting DB)

---

## Binary Status

**Built:** July 1, 06:34 UTC
- `target/release/monopsony-client` (16 MB)
- `target/release/monopsony-server` (31 MB)

**Deployed to test hosts:**
- Server (10.10.20.58): Not yet updated
- Clients (all 5): Not yet updated

---

## Next Steps

### Immediate (before testing):

1. **Fix Bug 2 completely** - Replace `record_pairing()` with `register_client() + record_pairing()`
2. **Fix Bug 3** - Add `std::fs::write(cert_dir.join("client_id"), &client_id)?` to Pair subcommand
3. **Rebuild binaries** - `cargo build --release`
4. **Deploy to server** - Copy updated `monopsony-server` to 10.10.20.58

### Testing sequence:

```bash
# 1. Stop old server
ssh monopsony@10.10.20.58 "pkill monopsony-server"

# 2. Deploy new server
scp target/release/monopsony-server monopsony@10.10.20.58:~/new_release/

# 3. Start server
ssh monopsony@10.10.20.58 "~/new_release/monopsony-server \
  --config /etc/monopsony/server.toml \
  --database-url postgres://monopsony:***@localhost/monopsony > /tmp/monopsony-server.log 2>&1 &"

# 4. Generate pairing code
ssh monopsony@10.10.20.58 "~/new_release/monopsony-server pair --server 10.10.20.58:50051"
# Note: CODE (e.g., ABC123)

# 5. Pair client (Arch Linux - easiest)
ssh root@10.10.20.20 "monopsony-client --cert-dir /var/lib/monopsony/certs \
  pair --server 10.10.20.58:50051 --code ABC123"

# 6. Verify files
ssh root@10.10.20.20 "ls -la /var/lib/monopsony/certs/
cat /var/lib/monopsony/certs/client_id"

# 7. Start daemon
ssh root@10.10.20.20 "monopsony-client --cert-dir /var/lib/monopsony/certs \
  daemon --server 10.10.20.58:50051 > /var/log/monopsony-client.log 2>&1 &"

# 8. Wait 5 minutes for inventory report

# 9. Check dashboard
curl -u admin:testpass123 http://10.10.20.58:3130/api/v1/clients

# 10. Check database
ssh monopsony@10.10.20.58 "sudo -u postgres psql -d monopsony \
  -c 'SELECT id, hostname, os, last_seen, registered_at FROM clients;'"
```

---

## Expected Results After All Fixes

| Check | Expected |
|-------|----------|
| Pairing completes | ✅ Certs + client_id file saved |
| Daemon connects | ✅ mTLS handshake succeeds |
| Heartbeat sent | ✅ Every 60s |
| Inventory reported | ✅ Every 300s, packages listed |
| Database | ✅ 1 row in `clients`, N rows in `packages`, N rows in `client_packages` |
| Dashboard | ✅ Shows 1 client with package count |

---

## Files Referenced

- `context/2025-07-02-zero-clients-investigation.md` - Full investigation details
- `context/2025-07-02-client-registration-flow.md` - Complete flow documentation
- `context/monopsony-fix-history.md` - Previous failed attempts (TLS issues)
- `BUGS-CLIENT-REGISTRATION.md` - Bug report with test steps

---

**Last updated:** July 2, 2025 - After MoA session and commit `0a531d1`