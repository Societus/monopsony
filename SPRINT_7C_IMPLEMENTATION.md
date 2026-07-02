# Sprint 7c Implementation Details
**Date:** June 22, 2026  
**Scope:** Wire all 8 shadow gRPC RPC stubs with real implementations

---

## gRPC API Changes

### MonopsonyGrpc struct
```rust
pub struct MonopsonyGrpc {
    db: Arc<Database>,
    tls_manager: Arc<TlsManager>,
    shadow_manager: Option<Arc<ShadowPackageManager>>,  // NEW
}
```

### Constructor Signature
```rust
// BEFORE
pub fn new(db: Arc<Database>, tls_manager: Arc<TlsManager>) -> Self

// AFTER  
pub fn new(db: Arc<Database>, tls_manager: Arc<TlsManager>, shadow_manager: Option<Arc<ShadowPackageManager>>) -> Self
```

---

## Shadow RPC Implementations

### 1. list_shadow_packages
**File:** `crates/monopsony-control/src/grpc.rs:355-410`
- **Before:** `Status::unimplemented(...)`
- **After:** Full DB query returning `ShadowPackageList`
- **Features:**
  - Client ID validation
  - JSONB field parsing with timestamp handling
  - Graceful NULL handling for optional fields
  - Complex field mapping from DB rows to proto structs

### 2. register_shadow_package
**File:** `crates/monopsony-control/src/grpc.rs:412-473`  
- **Before:** `Status::unimplemented(...)`
- **After:** Full validation + DB insertion
- **Features:**
  - Input validation (name required, empty checks)
  - JSON serialization for install_paths
  - Service name masking logic
  - Automatic timestamp setting

### 3. unregister_shadow_package
**File:** `crates/monopsony-control/src/grpc.rs:475-498`
- **Before:** `Status::unimplemented(...)`
- **After:** UUID-based deletion with response
- **Features:**
  - UUID parsing with error handling
  - Return success/failure status
  - Human-readable success/error messages

### 4. check_shadow_updates
**File:** `crates/monopsony-control/src/grpc.rs:500-553`
- **Before:** `Status::unimplemented(...)`
- **After:** Real update checking with shadow manager
- **Features:**
  - Package iteration from DB
  - GitHub API integration for version checking
  - Error handling per package (log but continue)
  - Graceful fallback when shadow manager disabled

### 5. apply_shadow_update
**File:** `crates/monopsony-control/src/grpc.rs:555-580`
- **Before:** `Status::unimplemented(...)`
- **After:** Update history recording
- **Features:**
  - Database history insertion
  - Backup flag handling
  - Success response with history ID

### 6. rollback_shadow_update
**File:** `crates/monopsony-control/src/grpc.rs:582-606`
- **Before:** `Status::unimplemented(...)`
- **After:** Rollback intent recording
- **Features:**
  - Database rollback history entry
  - Graceful response for client-side rollback

### 7. get_shadow_recipe
**File:** `crates/monopsony-control/src/grpc.rs:608-660`
- **Before:** `Status::unimplemented(...)`
- **After:** Recipe lookup + ed25519 signing
- **Features:**
  - Package lookup by name for client
  - TOML recipe parsing
  - ed25519 signature generation using shadow manager key
  - Signed recipe with key ID and timestamp

### 8. download_shadow_asset
**File:** `crates/monopsony-control/src/grpc.rs:662-699`
- **Before:** `Status::unimplemented(...)`
- **After:** Chunked streaming from cache
- **Features:**
  - Cache lookup by version hash
  - 64KB chunked streaming
  - Progress tracking with offset/total_size
  - gRPC stream handling

---

## Main.rs Changes

### New CLI Arguments
```rust
struct Args {
    // ... existing args ...
    
    // Shadow package system
    #[arg(long, default_value = "/var/lib/monopsony/shadow")]
    shadow_dir: String,
    
    #[arg(long, default_value = "/var/lib/monopsony/shadow/git-mirrors")]
    shadow_git_mirror_dir: String,
    
    #[arg(long, env = "MONOPSONY_GITHUB_TOKEN")]
    github_token: Option<String>,
    
    #[arg(long, default_value = "3600")]
    shadow_poll_interval: u64,
}
```

### Shadow Manager Initialization
```rust
// Lines 330-350 in main.rs
let shared_shadow_manager = if db.is_some() {
    // Directory creation
    let signing_key = load_or_generate_signing_key(Some(&signing_key_path));
    
    // ShadowPackageManager creation
    let mgr = ShadowPackageManager::new(
        db.clone().unwrap(),
        cache_dir,
        signing_key,
        github_token.clone(),
        poll_interval,
        shadow_git_mirror_dir,
    );
    
    // Background polling start
    let mgr_arc = Arc::new(mgr);
    mgr_arc.start_polling();
    Some(mgr_arc)
} else {
    None
};
```

### gRPC Server Integration
```rust
// Line 364: Pass shadow manager to gRPC start()
let shadow_for_grpc = shared_shadow_manager.clone();
let grpc_handle = tokio::spawn(async move {
    grpc::start(grpc_addr, db_for_grpc, tls_manager, mirror_for_grpc, shadow_for_grpc).await
});
```

---

## Shadow Package Enhancements

### New ShadowPackageManager Methods
```rust
// In shadow.rs
pub fn signing_key_ref(&self) -> &ed25519_dalek::SigningKey
pub fn cache_ref(&self) -> &ShadowCache
```

### New Shadow Signing Functions
```rust
// In shadow_signing.rs
pub fn load_or_generate_signing_key(path: Option<&std::path::Path>) -> SigningKey
```

---

## Database Integration

### Tables Used
- `shadow_packages` — CRUD operations for all shadow RPCs
- `shadow_update_history` — For apply/rollback operations

### DB Query Patterns
- JSONB field parsing for install_paths and config_paths
- Timestamp handling (numeric and ISO string formats)
- UUID-based lookups with error handling
- Client-scoped queries

---

## Key Implementation Details

### 1. Error Handling
- All RPCs return proper `Status` types with descriptive messages
- Database errors converted to internal status codes
- Input validation with argument errors
- Graceful handling when shadow manager disabled

### 2. Security
- ed25519 signature generation using persistent keys
- UUID-based package identification
- Client isolation in all DB operations

### 3. Performance
- Background polling doesn't block startup
- Chunked streaming for large asset downloads
- Database connection pooling maintained

### 4. Compatibility
- Optional shadow manager (graceful fallback to empty responses)
- No breaking changes to existing proto definitions
- Backward compatible gRPC API

---

## Files Changed (406 lines total)

### Modified Files
- `crates/monopsony-control/src/grpc.rs` (+331 lines)
- `crates/monopsony-server/src/main.rs` (+51 lines)  
- `crates/monopsony-shadow/src/shadow.rs` (+10 lines)
- `crates/monopsony-shadow/src/shadow_signing.rs` (+24 lines)
- `crates/monopsony-control/Cargo.toml` (+1 line)
- `crates/monopsony-server/Cargo.toml` (+1 line)
- `Cargo.lock` (+2 lines)
- `AGENTS.md` (+8 lines)

### Added Files
- All container deployment files (see first handoff doc)

---

## Verification Commands
```bash
cargo test --workspace -- --exact shadow           # Shadow-related tests
cargo test --workspace -- --exact grpc            # gRPC integration tests  
cargo test --workspace -- --count 20              # Quick smoke test
cargo clippy --workspace -- -D warnings          # Strict linting
```