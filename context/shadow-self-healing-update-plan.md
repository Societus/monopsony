# Shadow Package Self-Healing Update Pipeline — Implementation Plan

**Created:** 2026-07-16
**Status:** Planning — ready for implementation
**Prerequisite:** Self-update Phase 2 is complete (check/apply CLI wired, platform asset selection, archive extraction). See `context/self-update-implementation.md`.

## Problem

The shadow package recipe format already declares rich update semantics — `pre_update`, `post_update`, `on_failure` command hooks, `health_check` (HTTP/port/process), `backup_old`, `config_paths`, `max_backups`. But the client-side `apply_update()` function ignores all of them. It downloads, swaps the binary, and returns. No health check runs. No rollback happens on failure. No config edits are preserved.

This was identified by comparing against Hermes Agent's own self-update mechanism (`hermes_cli/main.py` lines 9409–10009), which has battle-tested safety nets for: stash-and-restore of local changes, post-update validation with auto-rollback, pre-update full snapshots, and non-interactive policy control.

## Goal

Make the monopsony self-update recipe a **reference implementation** that demonstrates the full power of shadow package managed updates — not just "download and overwrite" but a safety-pipelined update with operator preservation, validation, and automatic recovery.

---

## Architecture: Three-Phase Update Pipeline

```
┌─────────────────────────────────────────────────────────────────┐
│ PHASE 1: PRE-UPDATE                                             │
│                                                                 │
│  1. Parse recipe TOML                                           │
│  2. Run pre_update hook (if set)                                │
│  3. Snapshot: backup binary + config files to versioned dir     │
│  4. Stash operator config edits (diff against known-good)       │
│  5. Download new release asset                                  │
│  6. Extract binary from archive                                 │
└──────────────────────┬──────────────────────────────────────────┘
                       │
                       ▼
┌─────────────────────────────────────────────────────────────────┐
│ PHASE 2: APPLY                                                  │
│                                                                 │
│  7. Write new binary to temp path                               │
│  8. chmod 0755                                                  │
│  9. Atomic rename (swap)                                        │
│ 10. Mark update as "pending validation"                         │
└──────────────────────┬──────────────────────────────────────────┘
                       │
                       ▼
┌─────────────────────────────────────────────────────────────────┐
│ PHASE 3: VALIDATE & COMMIT/ROLLBACK                             │
│                                                                 │
│ 11. Run health_check (HTTP/port/process) with timeout           │
│                                                                 │
│  IF HEALTHY:                                                    │
│    12a. Run post_update hook                                    │
│    12b. Restore stashed config edits (merge onto new defaults)  │
│    12c. Prune old backups beyond max_backups                    │
│    12d. Record success in DB (shadow_update_history)            │
│    12e. Print summary                                           │
│                                                                 │
│  IF UNHEALTHY:                                                  │
│    12a. Run on_failure hook                                     │
│    12b. Restore backup binary (reverse atomic swap)             │
│    12c. Restore backup config files                             │
│    12d. Record failure in DB with error detail                  │
│    12e. Print rollback summary, exit with error code            │
└─────────────────────────────────────────────────────────────────┘
```

---

## Step-by-Step Implementation

### Step 1: Add `UpdateContext` struct (`self_update.rs`)

Tracks state across the pipeline. Replaces ad-hoc `PathBuf` arguments.

```rust
/// Context for a managed update operation
pub struct UpdateContext {
    /// The parsed recipe (if available — direct binary updates skip this)
    pub recipe: Option<ShadowRecipe>,
    /// Current binary path
    pub current_path: PathBuf,
    /// Directory containing the binary (for backups + temp files)
    pub install_dir: PathBuf,
    /// Backup directory (versioned by timestamp)
    pub backup_dir: PathBuf,
    /// Version being updated FROM
    pub from_version: String,
    /// Version being updated TO
    pub to_version: String,
    /// Asset URL to download
    pub asset_url: String,
    /// Config file paths from recipe (to snapshot + preserve)
    pub config_paths: Vec<PathBuf>,
    /// Stashed config edits (path → original content) for restore after update
    config_snapshots: Vec<ConfigSnapshot>,
}
```

**File:** `crates/monopsony-client/src/self_update.rs`
**Estimated:** ~40 lines of struct + impl

### Step 2: Implement snapshot/backup logic

Before any file mutation, save current state.

```rust
struct ConfigSnapshot {
    path: PathBuf,
    /// Content before update (for rollback)
    original_content: Vec<u8>,
    /// Whether the file existed before update
    existed: bool,
}

impl UpdateContext {
    /// Snapshot binary + config files to backup_dir
    pub fn create_snapshot(&mut self) -> Result<()> {
        std::fs::create_dir_all(&self.backup_dir)?;
        
        // Backup binary
        if self.current_path.exists() {
            let bin_backup = self.backup_dir.join("binary.bak");
            std::fs::copy(&self.current_path, &bin_backup)?;
        }
        
        // Backup config files and stash their content for diff/restore
        for config_path in &self.config_paths {
            if config_path.exists() {
                let content = std::fs::read(config_path)?;
                self.config_snapshots.push(ConfigSnapshot {
                    path: config_path.clone(),
                    original_content: content,
                    existed: true,
                });
                // Also copy to backup dir
                let backup_name = config_path.file_name()
                    .map(|n| format!("{}.bak", n.to_string_lossy()))
                    .unwrap_or_else(|| "config.bak".to_string());
                std::fs::write(self.backup_dir.join(&backup_name), &content)?;
            }
        }
        Ok(())
    }
}
```

**File:** `crates/monopsony-client/src/self_update.rs`
**Estimated:** ~50 lines

### Step 3: Implement health check execution

The recipe already has `HealthCheck` with `check_type` (Http/Port/Process) and `timeout_secs`. Currently unused.

```rust
/// Run the health check defined in the recipe
async fn run_health_check(check: &HealthCheck) -> Result<()> {
    match check.check_type {
        HealthCheckType::Http => {
            let url = check.url.as_deref()
                .ok_or_else(|| anyhow!("HTTP health check requires url"))?;
            let client = reqwest::Client::new();
            let timeout = Duration::from_secs(check.timeout_secs);
            
            // Poll until timeout — service may take a few seconds to start
            let start = Instant::now();
            loop {
                match client.get(url).timeout(Duration::from_secs(5)).send().await {
                    Ok(resp) if resp.status().is_success() => return Ok(()),
                    Ok(resp) => {
                        warn!("Health check returned {}", resp.status());
                    }
                    Err(e) => {
                        debug!("Health check connection failed: {}", e);
                    }
                }
                if start.elapsed() >= timeout {
                    anyhow::bail!("Health check timed out after {}s", timeout.as_secs());
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
        HealthCheckType::Port => {
            let port = check.port
                .ok_or_else(|| anyhow!("Port health check requires port"))?;
            let timeout = Duration::from_secs(check.timeout_secs);
            let start = Instant::now();
            loop {
                if std::net::TcpStream::connect(format!("127.0.0.1:{}", port)).is_ok() {
                    return Ok(());
                }
                if start.elapsed() >= timeout {
                    anyhow::bail!("Port {} not responding after {}s", port, timeout.as_secs());
                }
                std::thread::sleep(Duration::from_secs(2));
            }
        }
        HealthCheckType::Process => {
            // Check if process name appears in /proc or via `pgrep`
            let process = check.process.as_deref()
                .ok_or_else(|| anyhow!("Process health check requires process name"))?;
            let timeout = Duration::from_secs(check.timeout_secs);
            let start = Instant::now();
            loop {
                let output = std::process::Command::new("pgrep")
                    .arg("-x")
                    .arg(process)
                    .output()?;
                if output.status.success() {
                    return Ok(());
                }
                if start.elapsed() >= timeout {
                    anyhow::bail!("Process '{}' not running after {}s", process, timeout.as_secs());
                }
                std::thread::sleep(Duration::from_secs(2));
            }
        }
    }
}
```

**File:** `crates/monopsony-client/src/self_update.rs`
**Estimated:** ~70 lines

### Step 4: Implement hook execution (pre_update, post_update, on_failure)

The recipe already has these fields. They're shell commands.

```rust
/// Execute a recipe hook command. Returns Ok if exit code 0, Err otherwise.
fn run_hook(hook_name: &str, command: &str) -> Result<()> {
    info!("Running {} hook: {}", hook_name, command);
    
    #[cfg(unix)]
    let result = std::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .output();
    
    #[cfg(windows)]
    let result = std::process::Command::new("cmd")
        .arg("/C")
        .arg(command)
        .output();
    
    match result {
        Ok(output) if output.status.success() => {
            if !output.stdout.is_empty() {
                debug!("{} stdout: {}", hook_name, 
                       String::from_utf8_lossy(&output.stdout).trim());
            }
            Ok(())
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("{} hook failed (exit {}): {}", 
                          hook_name, output.status, stderr.trim());
        }
        Err(e) => {
            anyhow::bail!("Failed to execute {} hook: {}", hook_name, e);
        }
    }
}
```

**File:** `crates/monopsony-client/src/self_update.rs`
**Estimated:** ~30 lines
**Note:** The client-side `shadow.rs` module already has `run_hook()` — check if it can be reused rather than duplicated.

### Step 5: Implement rollback logic

Reverse the swap + restore config files.

```rust
impl UpdateContext {
    /// Roll back to pre-update state
    pub fn rollback(&self, error: &str) -> Result<()> {
        warn!("Update failed: {}. Rolling back...", error);
        
        // Restore binary
        let bin_backup = self.backup_dir.join("binary.bak");
        if bin_backup.exists() && self.current_path.exists() {
            std::fs::copy(&bin_backup, &self.current_path)
                .context("Failed to restore binary during rollback")?;
            
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(
                    &self.current_path,
                    std::fs::Permissions::from_mode(0o755),
                )?;
            }
            
            info!("Binary restored to previous version");
        }
        
        // Restore config files
        for snapshot in &self.config_snapshots {
            if snapshot.existed {
                std::fs::write(&snapshot.path, &snapshot.original_content)
                    .warn(&format!("Failed to restore config: {}", snapshot.path.display()));
            }
        }
        
        info!("Rollback complete. System restored to pre-update state.");
        Ok(())
    }
}
```

**File:** `crates/monopsony-client/src/self_update.rs`
**Estimated:** ~35 lines

### Step 6: Implement config edit preservation

This is the pattern adapted from Hermes' stash-and-restore. Instead of git stash, we:

1. Before update: snapshot config file contents
2. After update: if the new release shipped a new default config, attempt to merge operator edits
3. If merge fails: preserve operator's original config (don't lose their changes)

```rust
/// Attempt to restore operator config edits after update.
/// Strategy: if config file still exists after update, operator's version wins.
/// If the new release overwrote it (e.g., extract method replaced it),
/// restore the operator's original content.
fn restore_config_edits(snapshots: &[ConfigSnapshot]) -> Result<()> {
    for snapshot in snapshots {
        if !snapshot.existed {
            continue;
        }
        
        // Read current content (may have been overwritten by update)
        let current = std::fs::read(&snapshot.path).unwrap_or_default();
        
        if current == snapshot.original_content {
            // Unchanged — nothing to do
            continue;
        }
        
        // Config was modified during update. The operator's original 
        // config is more valuable than new defaults — restore it.
        // (Advanced: could attempt a 3-way merge in the future)
        warn!("Config file {} was modified during update. Restoring operator's version.",
              snapshot.path.display());
        std::fs::write(&snapshot.path, &snapshot.original_content)?;
    }
    Ok(())
}
```

**File:** `crates/monopsony-client/src/self_update.rs`
**Estimated:** ~30 lines

### Step 7: Implement backup pruning

The recipe has `max_backups` (default 3). Keep only N most recent backup dirs.

```rust
/// Prune old backups, keeping only the N most recent
fn prune_backups(install_dir: &Path, max_backups: usize) -> Result<()> {
    let mut backups: Vec<_> = std::fs::read_dir(install_dir)?
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_str()
                .map(|n| n.starts_with("update-backup-"))
                .unwrap_or(false)
        })
        .collect();
    
    if backups.len() <= max_backups {
        return Ok(());
    }
    
    // Sort by modified time, newest first
    backups.sort_by(|a, b| {
        b.metadata()
            .and_then(|m| m.modified())
            .ok()
            .cmp(&a.metadata().and_then(|m| m.modified()).ok())
    });
    
    // Remove oldest beyond max_backups
    for entry in backups.iter().skip(max_backups) {
        debug!("Pruning old backup: {}", entry.path().display());
        std::fs::remove_dir_all(entry.path()).ok();
    }
    
    Ok(())
}
```

**File:** `crates/monopsony-client/src/self_update.rs`
**Estimated:** ~25 lines

### Step 8: Rewrite `apply_update()` as `apply_update_managed()`

The new entrypoint orchestrates the full pipeline. The old `apply_update()` stays as a raw download+swap for backward compatibility.

```rust
/// Managed update with full safety pipeline.
/// Honors recipe fields: pre_update, post_update, on_failure, health_check,
/// backup_old, config_paths, max_backups.
pub async fn apply_update_managed(ctx: &mut UpdateContext) -> Result<UpdateOutcome> {
    // PHASE 1: PRE-UPDATE
    
    // Run pre_update hook
    if let Some(ref recipe) = ctx.recipe {
        if let Some(ref pre) = recipe.install.pre_update {
            run_hook("pre_update", pre)
                .context("pre_update hook failed — aborting update")?;
        }
    }
    
    // Create snapshot (binary + configs)
    ctx.create_snapshot()?;
    info!("Snapshot created at {}", ctx.backup_dir.display());
    
    // Download + extract
    let bytes = download_asset(&ctx.asset_url).await?;
    let binary_data = if needs_extraction(&ctx.asset_url) {
        extract_binary_from_archive(&bytes, &ctx.asset_url)?
    } else {
        bytes
    };
    
    // PHASE 2: APPLY
    
    write_and_swap(&ctx.current_path, &ctx.install_dir, &binary_data)?;
    info!("Binary swapped to version {}", ctx.to_version);
    
    // PHASE 3: VALIDATE & COMMIT/ROLLBACK
    
    if let Some(ref recipe) = ctx.recipe {
        if let Some(ref check) = recipe.install.health_check {
            info!("Running health check...");
            match run_health_check(check).await {
                Ok(()) => {
                    info!("Health check passed");
                }
                Err(e) => {
                    // HEALTH CHECK FAILED → ROLLBACK
                    if let Some(ref on_fail) = recipe.install.on_failure {
                        let _ = run_hook("on_failure", on_fail);
                    }
                    ctx.rollback(&format!("Health check failed: {}", e))?;
                    return Ok(UpdateOutcome::RolledBack(e.to_string()));
                }
            }
        }
        
        // Run post_update hook
        if let Some(ref post) = recipe.install.post_update {
            if let Err(e) = run_hook("post_update", post) {
                warn!("post_update hook failed: {}", e);
                // Don't roll back for post_update failure — binary is healthy
            }
        }
    }
    
    // Restore operator config edits
    restore_config_edits(&ctx.config_snapshots)?;
    
    // Prune old backups
    if let Some(ref recipe) = ctx.recipe {
        prune_backups(&ctx.install_dir, recipe.install.max_backups)?;
    }
    
    info!("Update committed successfully: {} → {}", 
          ctx.from_version, ctx.to_version);
    Ok(UpdateOutcome::Success)
}

pub enum UpdateOutcome {
    Success,
    RolledBack(String),
}
```

**File:** `crates/monopsony-client/src/self_update.rs`
**Estimated:** ~80 lines

### Step 9: Wire recipe parsing into `check_for_update()`

Currently `check_for_update()` doesn't parse the recipe. It needs to return the recipe (or at least the relevant fields) so `apply_update_managed` can use them.

```rust
pub struct UpdateCheckResult {
    pub current_version: String,
    pub latest_version: Option<String>,
    pub asset_url: Option<String>,
    pub changelog_url: Option<String>,
    pub recipe: Option<ShadowRecipe>,  // NEW
}
```

For monopsony's self-update, the recipe is `DEFAULT_MONOPSONY_RECIPE` constant in `shadow_types.rs`. For third-party shadow packages, it's fetched from the server via gRPC `GetShadowRecipe`.

Parse the recipe in `check_for_update()`:
```rust
// After determining update is available...
let recipe = monopsony_common::shadow_types::DEFAULT_MONOPSONY_RECIPE
    .parse::<ShadowRecipe>()
    .ok();
```

Actually, `DEFAULT_MONOPSONY_RECIPE` is a TOML string. Use `toml::from_str()` and `monopsony_shadow::shadow_recipe::parse_recipe_str()`.

**File:** `crates/monopsony-client/src/self_update.rs`
**Estimated:** ~15 lines change

### Step 10: Update the CLI to use `apply_update_managed()`

In `run_self_update()` (both client and server), replace the call to `apply_update()` with `apply_update_managed()`.

```rust
// Build UpdateContext from recipe + runtime info
let mut ctx = UpdateContext::new(
    recipe,
    current_exe,
    target_dir,
    current_version,
    latest_version,
    asset_url,
);

match apply_update_managed(&mut ctx).await {
    Ok(UpdateOutcome::Success) => {
        println!("Update applied successfully!");
    }
    Ok(UpdateOutcome::RolledBack(reason)) => {
        eprintln!("Update rolled back: {}", reason);
        eprintln!("Your system has been restored to the previous version.");
        std::process::exit(5); // Exit code 5 = rolled back
    }
    Err(e) => {
        eprintln!("Update failed: {}", e);
        std::process::exit(4);
    }
}
```

**Files:**
- `crates/monopsony-client/src/main.rs` — `run_self_update()` Apply branch
- `crates/monopsony-server/src/self_update.rs` — `run_self_update()` Apply branch

**Estimated:** ~30 lines each

### Step 11: Update the default monopsony recipe

Add health_check, pre/post hooks, and config_paths to `DEFAULT_MONOPSONY_RECIPE` so it serves as the reference implementation.

```toml
[install]
method = "extract"
target_dir = "/usr/local/bin"
chmod = ["monopsony-server", "monopsony-client"]
backup_old = true
max_backups = 3
config_paths = ["/etc/monopsony/server.toml", "/etc/monopsony/client.toml"]

# Pre-update: stop the service before swapping the binary
pre_update = "systemctl stop monopsony-server 2>/dev/null; true"

# Post-update: start the service after successful swap
post_update = "systemctl start monopsony-server 2>/dev/null; true"

# On failure: ensure service is running with old binary
on_failure = "systemctl start monopsony-server 2>/dev/null; true"

# Health check: verify the server responds after update
[install.health_check]
type = "port"
port = 50051
timeout_secs = 30
```

**File:** `crates/monopsony-common/src/shadow_types.rs` — `DEFAULT_MONOPSONY_RECIPE` constant
**Estimated:** ~20 lines TOML change

### Step 12: Tests

Add unit tests for each new function:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_snapshot_roundtrip() {
        // Create temp config, snapshot, modify, restore, verify content matches
    }

    #[tokio::test]
    async fn test_health_check_port_success() {
        // Bind a listener on a port, verify health check passes
    }

    #[tokio::test]
    async fn test_health_check_port_timeout() {
        // Don't bind anything, verify health check times out
    }

    #[test]
    fn test_run_hook_success() {
        // Run "true" command, verify Ok
    }

    #[test]
    fn test_run_hook_failure() {
        // Run "false" command, verify Err
    }

    #[test]
    fn test_prune_backups() {
        // Create N+2 backup dirs, prune to N, verify count
    }

    #[tokio::test]
    async fn test_apply_managed_with_rollback() {
        // Apply update with health check that fails, verify rollback happens
        // and original binary content is restored
    }

    #[tokio::test]
    async fn test_apply_managed_success() {
        // Apply update with health check that passes, verify success
    }
}
```

**File:** `crates/monopsony-client/src/self_update.rs`
**Estimated:** ~150 lines

---

## Exit Codes

| Code | Meaning |
|------|---------|
| 0 | Up to date (no update needed) |
| 1 | Update available (check mode) |
| 2 | Failed to check for updates |
| 3 | No compatible asset for platform |
| 4 | Update failed (download/extract/swap error) |
| 5 | Update applied but rolled back (health check failure) |

---

## Dependency Changes

None. All required crates are already in `Cargo.toml`:
- `reqwest` — HTTP for health checks and downloads
- `flate2`, `tar`, `zip` — archive extraction
- `tokio` — async runtime
- `anyhow` — error handling
- `tracing` — logging
- `chrono` — timestamps for backup dirs

---

## File Change Summary

| File | Change | Lines |
|------|--------|-------|
| `crates/monopsony-client/src/self_update.rs` | Add pipeline functions, rewrite apply | ~400 new |
| `crates/monopsony-client/src/main.rs` | Wire `apply_update_managed()` in Apply branch | ~30 changed |
| `crates/monopsony-server/src/self_update.rs` | Wire `apply_update_managed()` in Apply branch | ~30 changed |
| `crates/monopsony-common/src/shadow_types.rs` | Update `DEFAULT_MONOPSONY_RECIPE` with hooks/health_check | ~20 changed |
| `context/self-update-implementation.md` | Update status doc | — |

**Total estimated new/changed code:** ~500 lines

---

## Implementation Order

1. **Step 1** — `UpdateContext` struct (no dependencies)
2. **Step 2** — Snapshot logic (depends on Step 1)
3. **Step 3** — Health check execution (standalone)
4. **Step 4** — Hook execution (standalone — check if `shadow.rs::run_hook` can be reused)
5. **Step 5** — Rollback logic (depends on Steps 1-2)
6. **Step 6** — Config edit preservation (depends on Step 2)
7. **Step 7** — Backup pruning (standalone)
8. **Step 8** — `apply_update_managed()` orchestrator (depends on Steps 2-7)
9. **Step 9** — Recipe parsing in check_for_update (depends on Step 1)
10. **Step 10** — CLI wiring (depends on Step 8)
11. **Step 11** — Update DEFAULT_MONOPSONY_RECIPE (standalone)
12. **Step 12** — Tests (after all above)

Steps 3, 4, 7 are independent and can be done in parallel.

---

## Design Decisions

### Why operator config always wins (no 3-way merge)

Hermes uses git stash with conflict detection. Shadow packages aren't git repos, and implementing a TOML-aware 3-way merge is complex and error-prone. Decision: if the update modifies a config file that the operator had customized, restore the operator's version and log a warning. This is conservative — the operator never loses their changes, but they may miss new config options. Future enhancement could warn about missing keys.

### Why health check runs after binary swap, not before

The new binary might need different startup args or ports. Running the health check against the *new* binary validates that the update actually works. If it fails, we roll back to the known-good state.

### Why pre_update stops the service

You can't overwrite a running binary on Windows (file lock). On Linux you can (inode stays open), but the old process keeps running in memory until restarted. The `pre_update` hook stops the service, the swap happens, `post_update` starts it again. The health check verifies the new service actually came up.

### Why we keep `apply_update()` as a separate function

The raw `apply_update()` (download, backup, swap) is still useful for:
- Quick manual updates where the user doesn't want the full pipeline
- Third-party shadow packages that handle their own lifecycle
- Testing and debugging

`apply_update_managed()` wraps it with the safety net.

---

## What NOT to Implement (Out of Scope)

- **Git-based stash/merge** — shadow packages aren't git repos
- **TOML-aware config merging** — too complex for this phase, operator config wins
- **Bytecode cache clearing** — Rust compiles to native, not applicable
- **Concurrent process detection** — systemd manages the service lifecycle
- **Venv repair** — not applicable to Go/Rust binaries
- **Snapshot/restore via `hermes import`** — that's a Hermes-specific feature
- **Non-interactive policy (`stash` vs `discard`)** — future enhancement once the core pipeline works
