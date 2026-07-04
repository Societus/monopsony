# Package Update Success Telemetry — Investigation & Design

Created: 2026-07-04
Status: Design investigation. Not implemented.

## Goal

Track which package versions a client has **successfully installed** (not just
downloaded), so the server can:
- Know the latest version confirmed working on each client
- Build a "known-good" version map across the fleet
- Improve prefetch decisions (don't prefetch versions clients already have)
- Feed into shadow package rollouts (only push updates for confirmed-broken versions)

This telemetry must be **disableable** by either the client or the server.

## Existing Telemetry Infrastructure

### What exists today

1. **Inventory reports** (`proto/monopsony.proto:69-84`):
   - `InventoryReport` — full package list with versions, sent periodically
   - `InventoryDelta` — incremental add/remove
   - Client calls `report_inventory()` every ~6 minutes (configurable)
   - Server stores in `client_packages` table (client_id, package_id, installed_at)

2. **Cache hit tracking** (`proxy.rs`):
   - `cache_entries.hit_count` increments on every cache HIT
   - `cache_entries.last_accessed` timestamp updated
   - But this only tracks cache activity, not install success

3. **Heartbeat** — client → server every 30s, carries no package data

### What's missing

No mechanism exists to signal "I just successfully installed package X version
Y from cache entry Z." The server can infer that a client fetched a package
(cache HIT/MISS), but cannot confirm the installation succeeded.

## Proposed Design: Install Confirmation Telemetry

### Approach: piggyback on existing inventory delta

The simplest path with minimal new infrastructure:

1. **Client detects package changes** — the client daemon already periodically
   runs `list_installed()`. After each scan, it compares against the last
   reported inventory. New/updated packages are sent as `InventoryDelta`.

2. **Add optional fields to `PackageEntry`** in the proto:

```protobuf
message PackageEntry {
    string manager = 1;
    string name = 2;
    string version = 3;
    string arch = 4;
    // New: when this version was confirmed installed
    int64 installed_at = 5;
    // New: source of the install (proxy cache hit, proxy cache miss, direct)
    string install_source = 6;
}
```

3. **Server stores `installed_at`** in `client_packages.installed_at` (column
   already exists). When the inventory report comes in with a newer
   `installed_at` than what's stored, update it.

4. **Server-side telemetry view**: A new query joins `client_packages` →
   `packages` → `cache_entries` to answer: "what's the latest version of
   package X confirmed installed across all clients?"

### Why this is easy

- **No new gRPC RPC needed** — reuses `ReportInventory` / `InventoryDelta`
- **No new DB table** — `client_packages.installed_at` already exists
- **No new client-side detection logic** — the client already detects
  version changes between inventory scans
- **Proto change is backward-compatible** — new fields are optional

### Disable mechanism

#### Client-side opt-out

Add to `ClientConfig` (`config.rs:386`):

```rust
pub struct ClientIdentityConfig {
    // ...existing fields...

    /// Disable reporting install confirmations to the server.
    /// When true, inventory reports omit installed_at timestamps.
    /// The server will still receive package names/versions (for cache
    /// prefetch purposes) but cannot track install success timing.
    #[serde(default = "default_true")]
    pub report_install_telemetry: bool,
}
```

Config file (`/etc/monopsony/client.toml`):
```toml
[client]
report_install_telemetry = false  # opt out
```

When disabled, the client sends `installed_at = 0` (unset) for all entries.
The server treats 0 as "telemetry not available."

#### Server-side kill switch

Add to `ServerConfig` (`config.rs:47`):

```rust
pub struct ServerConfig {
    // ...existing fields...

    /// Disable collecting install confirmation telemetry from clients.
    /// When false, the server ignores installed_at fields in inventory
    /// reports and does not update timestamps. Existing data is retained.
    #[serde(default = "default_true")]
    pub telemetry: TelemetryConfig,
}

pub struct TelemetryConfig {
    /// Enable install success tracking
    #[serde(default = "default_true")]
    pub install_tracking: bool,
    /// Enable aggregate fleet metrics (counts, not per-client)
    #[serde(default = "default_true")]
    pub fleet_metrics: bool,
}
```

Server config (`/etc/monopsony/server.toml`):
```toml
[telemetry]
install_tracking = false  # server-side kill switch
```

When `install_tracking = false`, the server's `handle_inventory()` handler
strips `installed_at` from incoming reports before storing. Dashboard API
hides the telemetry views.

#### Dashboard control

The dashboard settings page exposes a toggle. Changing it writes to
`server.toml` and signals the server to reload config (existing pattern).

### Data flow

```
Client installs package (via proxy cache)
  ↓
Client daemon's periodic scan detects version change
  ↓
Client sends InventoryDelta with PackageEntry {
    name: "bash",
    version: "5.3.15-1",
    installed_at: <unix_timestamp>,
    install_source: "proxy",  // or "direct" if bypassed
}
  ↓ (if client telemetry enabled)
Server handle_inventory_delta()
  ↓ (if server telemetry enabled)
Records installed_at in client_packages table
  ↓
Telemetry query: "latest confirmed version of bash across fleet"
  → SELECT MAX(installed_at), version FROM client_packages
    JOIN packages ON ... WHERE name = 'bash'
```

### What "successfully installed" means

The client's `list_installed()` only reports packages that are **currently
installed** — if `apk add` or `pacman -S` fails, the package won't appear in
the next inventory scan. So the telemetry is inherently "confirmed working":
a package only appears in the report if the package manager accepted it.

This is a weaker signal than "the service restarted cleanly after the update"
but it's a strong baseline. It tells you the package installed without error.

### Limitations of this approach

- **Timing imprecision**: inventory scans run every ~6 minutes, so
  `installed_at` is "when the scan detected it," not "when dpkg/apk/pacman
  finished." For precise timing, we'd need a post-install hook — more complex.
- **No rollback detection**: if a client installs v2, then rolls back to v1,
  the next inventory report shows v1 again. The server sees the version
  revert but doesn't know it was a rollback vs. a reinstall.
- **`install_source` is best-effort**: the client can detect if its proxy
  is running and infer the install went through cache, but can't distinguish
  HIT vs MISS without the server telling it.

### Alternative considered: post-install hook

Install a pacman hook / apt postinst script that calls the client daemon's
local API to report "just installed X." More precise timing but:
- Requires root-level package manager hooks on every client
- Fragile across package managers (pacman hooks ≠ apt hooks ≠ apk triggers)
- More moving parts

**Recommendation**: Start with the inventory-piggyback approach. It's the
minimum viable telemetry with the least client-side complexity. Add hooks
later if timing precision becomes important.

## Implementation steps (estimated effort)

1. Add proto fields to `PackageEntry` (5 min)
2. Regenerate gRPC stubs (1 min)
3. Update `handle_inventory` / `handle_inventory_delta` to store `installed_at` (30 min)
4. Add `TelemetryConfig` to `ServerConfig` (15 min)
5. Add `report_install_telemetry` to `ClientIdentityConfig` (10 min)
6. Update client inventory reporting to include `installed_at` (20 min)
7. Add dashboard API endpoint for telemetry queries (30 min)
8. Add dashboard UI toggle for server-side kill switch (20 min)

Total: ~2.5 hours of focused work.

## Files to touch

- `proto/monopsony.proto` — add fields to PackageEntry
- `crates/monopsony-common/src/config.rs` — TelemetryConfig, client opt-out
- `crates/monopsony-control/src/grpc.rs` — respect server-side kill switch
- `crates/monopsony-db/src/db.rs` — record_inventory() stores installed_at
- `crates/monopsony-client/src/grpc_client.rs` — populate installed_at
- `crates/monopsony-client/src/main.rs` — respect client-side opt-out
- `crates/monopsony-admin/src/` — dashboard API + settings toggle
