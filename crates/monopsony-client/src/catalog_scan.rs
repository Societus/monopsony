//! Client-side shadow catalog scanning (Sprint 22, slice 22-C2).
//!
//! Pulls the server's shadow catalog hash, scans local install paths for each
//! catalog entry, and reports findings back. Hash math mirrors
//! `monopsony-shadow/src/catalog_hash.rs` exactly.

use std::path::Path;
use std::time::{Duration, Instant};

/// SHA-256 of `data`, lowercase hex.
fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect()
}

/// Stable hash for a single catalog entry: sha256 over canonical JSON
/// `{"name":...,"recipe_toml":...}` (serde_json Value sorts map keys, no spaces).
pub fn entry_hash(name: &str, recipe_toml: &str) -> String {
    let mut map = serde_json::Map::new();
    map.insert(
        "name".to_string(),
        serde_json::Value::String(name.to_string()),
    );
    map.insert(
        "recipe_toml".to_string(),
        serde_json::Value::String(recipe_toml.to_string()),
    );
    let json = serde_json::Value::Object(map).to_string();
    sha256_hex(json.as_bytes())
}

/// Stable hash for a whole catalog: sha256 over canonical JSON of the
/// sorted vector of entry hashes (order-independent).
pub fn catalog_hash(entry_hashes: &[String]) -> String {
    let mut sorted: Vec<String> = entry_hashes.to_vec();
    sorted.sort();
    let json = serde_json::to_string(&sorted).unwrap_or_else(|_| "[]".to_string());
    sha256_hex(json.as_bytes())
}

/// One scan result for a single catalog entry.
#[derive(Debug, Clone)]
pub struct ScanFinding {
    pub recipe_name: String,
    pub installed: bool,
    pub detected_version: String,
    pub method: String,
}

/// Scan one catalog entry locally. Never panics: any parse/exec failure
/// yields `method = "error"`, `installed = false`.
pub fn scan_entry(entry_name: &str, recipe_toml: &str) -> ScanFinding {
    match scan_entry_inner(entry_name, recipe_toml) {
        Ok(f) => f,
        Err(_) => ScanFinding {
            recipe_name: entry_name.to_string(),
            installed: false,
            detected_version: String::new(),
            method: "error".to_string(),
        },
    }
}

fn scan_entry_inner(entry_name: &str, recipe_toml: &str) -> Result<ScanFinding, String> {
    let doc: toml::Value = toml::from_str(recipe_toml).map_err(|e| e.to_string())?;
    let table = doc.as_table().ok_or_else(|| "recipe is not a table".to_string())?;

    // install_paths lives under [install]; tolerate a top-level key too.
    let mut paths: Vec<String> = table
        .get("install")
        .and_then(|i| i.get("install_paths"))
        .or_else(|| table.get("install_paths"))
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    // Detect-generated recipes carry [install].target_dir instead of an
    // install_paths array (install_paths lives in the per-client row, not
    // the catalog TOML). Derive the conventional candidate path from
    // target_dir + package name so adopt can confirm such recipes.
    if paths.is_empty() {
        if let Some(target_dir) = table
            .get("install")
            .and_then(|i| i.get("target_dir"))
            .and_then(|c| c.as_str())
        {
            paths.push(format!("{}/{}", target_dir.trim_end_matches('/'), entry_name));
        }
    }

    let installed = paths.iter().any(|p| Path::new(p).exists());
    let mut finding = ScanFinding {
        recipe_name: entry_name.to_string(),
        installed,
        detected_version: String::new(),
        method: "install_path".to_string(),
    };

    if installed {
        if let Some(cmd) = table
            .get("version")
            .and_then(|v| v.get("detect_command"))
            .and_then(|c| c.as_str())
        {
            if let Some(ver) = run_detect_sync(cmd) {
                finding.detected_version = ver;
                finding.method = "detect_command".to_string();
            }
        }
    }

    Ok(finding)
}

/// Run a version detect command via `bash -c` (2s timeout), returning the
/// first `X.Y` / `X.Y.Z` match in stdout.
pub async fn run_detect(command: &str, workdir: Option<&Path>) -> Option<String> {
    let mut cmd = tokio::process::Command::new("bash");
    cmd.arg("-c")
        .arg(command)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    if let Some(dir) = workdir {
        cmd.current_dir(dir);
    }

    let attempt = async {
        let out = cmd.output().await.ok()?;
        if !out.status.success() {
            return None;
        }
        extract_version(&String::from_utf8_lossy(&out.stdout))
    };

    tokio::time::timeout(Duration::from_secs(2), attempt)
        .await
        .ok()
        .flatten()
}

/// Blocking wrapper around [`run_detect`]; safe to call from inside or
/// outside a tokio runtime (uses a dedicated thread + runtime).
fn run_detect_sync(command: &str) -> Option<String> {
    std::thread::scope(|s| {
        s.spawn(|| {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .ok()?;
            rt.block_on(run_detect(command, None))
        })
        .join()
        .ok()?
    })
}

/// First `X.Y` or `X.Y.Z` sequence in `s` (equivalent to regex
/// `([0-9]+\.[0-9]+(\.[0-9]+)?)`).
fn extract_version(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let mut j = i;
            let mut dots = 0;
            while j < bytes.len()
                && (bytes[j].is_ascii_digit()
                    || (bytes[j] == b'.'
                        && dots < 2
                        && j + 1 < bytes.len()
                        && bytes[j + 1].is_ascii_digit()))
            {
                if bytes[j] == b'.' {
                    dots += 1;
                }
                j += 1;
            }
            let candidate = &s[i..j];
            if candidate.contains('.') {
                return Some(candidate.to_string());
            }
            i = j;
        } else {
            i += 1;
        }
    }
    None
}

/// Rate-limits how often the daemon attempts a catalog scan pass.
pub struct ScanGuard {
    last: Option<Instant>,
    min_interval: Duration,
}

impl ScanGuard {
    pub fn new(min_interval: Duration) -> Self {
        Self {
            last: None,
            min_interval,
        }
    }

    /// True if never run, or at least `min_interval` has elapsed since the
    /// last true return. Records `now` when returning true.
    pub fn ready(&mut self) -> bool {
        let ready = match self.last {
            None => true,
            Some(t) => t.elapsed() >= self.min_interval,
        };
        if ready {
            self.last = Some(Instant::now());
        }
        ready
    }
}

/// Load the last reported catalog hash from `cert_dir/catalog-state.json`.
pub fn load_last_hash(cert_dir: &Path) -> Option<String> {
    let data = std::fs::read_to_string(cert_dir.join("catalog-state.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&data).ok()?;
    v.get("catalog_hash")?.as_str().map(|s| s.to_string())
}

/// Persist the reported catalog hash to `cert_dir/catalog-state.json`.
/// All errors are silent (best-effort state).
pub fn save_last_hash(cert_dir: &Path, hash: &str) {
    let _ = std::fs::create_dir_all(cert_dir);
    let v = serde_json::json!({ "catalog_hash": hash });
    let _ = std::fs::write(cert_dir.join("catalog-state.json"), v.to_string());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tempdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "mono-catscan-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    const GOLDEN_NAME: &str = "ripgrep";
    const GOLDEN_TOML: &str = "[package]\nname = \"ripgrep\"\nversion = \"14.1.1\"\n";
    const GOLDEN_HASH: &str = "da796477338559e73eaa0258940b671d45918f4ab9c7a0f77a91985cbc5415bf";

    #[test]
    fn entry_hash_golden() {
        assert_eq!(entry_hash(GOLDEN_NAME, GOLDEN_TOML), GOLDEN_HASH);
    }

    #[test]
    fn entry_hash_mismatch_on_different_toml() {
        let a = entry_hash(GOLDEN_NAME, GOLDEN_TOML);
        let b = entry_hash(GOLDEN_NAME, "[package]\nname = \"ripgrep\"\nversion = \"14.1.0\"\n");
        assert_ne!(a, b);
    }

    #[test]
    fn catalog_hash_order_independent_and_deterministic() {
        let hash_a = entry_hash(GOLDEN_NAME, GOLDEN_TOML);
        let hash_b = entry_hash("fzf", "[package]\nname = \"fzf\"\n");
        let ab = catalog_hash(&[hash_a.clone(), hash_b.clone()]);
        let ba = catalog_hash(&[hash_b, hash_a]);
        assert_eq!(ab, ba);
        // determinism across calls
        assert_eq!(ab, catalog_hash(&[entry_hash(GOLDEN_NAME, GOLDEN_TOML), entry_hash("fzf", "[package]\nname = \"fzf\"\n")]));
    }

    #[test]
    fn scan_existing_install_path_installed() {
        let dir = tempdir("exists");
        let file = dir.join("tool.bin");
        std::fs::write(&file, b"x").unwrap();
        let recipe = format!(
            "[install]\ninstall_paths = [\"{}\", \"/nonexistent/zz\"]\n",
            file.display()
        );
        let f = scan_entry("tool", &recipe);
        assert!(f.installed);
        assert_eq!(f.method, "install_path");
        assert_eq!(f.detected_version, "");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scan_missing_install_path_not_installed() {
        let recipe = "[install]\ninstall_paths = [\"/nonexistent/mono-catscan-missing\"]\n";
        let f = scan_entry("tool", recipe);
        assert!(!f.installed);
        assert_eq!(f.method, "install_path");
    }

    #[test]
    fn scan_target_dir_fallback_derives_candidate_path() {
        // Detect-generated recipes carry [install].target_dir, no
        // install_paths array — adopt must still confirm the install.
        let dir = tempdir("targetdir");
        let bin = dir.join("tool");
        std::fs::write(&bin, b"#!/bin/sh\n").unwrap();
        let recipe = format!("[install]\ntarget_dir = \"{}\"\n", dir.display());
        let f = scan_entry("tool", &recipe);
        assert!(f.installed);
        assert_eq!(f.method, "install_path");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scan_target_dir_fallback_missing_binary_not_installed() {
        let recipe = "[install]\ntarget_dir = \"/nonexistent/mono-catscan-tdir\"\n";
        let f = scan_entry("tool", recipe);
        assert!(!f.installed);
    }

    #[test]
    fn scan_missing_install_section_not_installed_no_panic() {
        let f = scan_entry("tool", "[package]\nname = \"tool\"\n");
        assert!(!f.installed);
        assert_eq!(f.method, "install_path");
    }

    #[test]
    fn scan_garbage_toml_error_method_no_panic() {
        let f = scan_entry("tool", "this is ]][ not toml {{{");
        assert!(!f.installed);
        assert_eq!(f.method, "error");
    }

    #[tokio::test]
    async fn run_detect_golden_echo_version() {
        let v = run_detect("echo 1.2.3", None).await;
        assert_eq!(v, Some("1.2.3".to_string()));
    }

    #[test]
    fn scan_guard_interval_logic() {
        let mut g = ScanGuard::new(Duration::from_millis(200));
        assert!(g.ready()); // never run
        assert!(!g.ready()); // too soon
        std::thread::sleep(Duration::from_millis(250));
        assert!(g.ready()); // elapsed
    }

    #[test]
    fn save_load_last_hash_roundtrip() {
        let dir = tempdir("state");
        assert_eq!(load_last_hash(&dir), None);
        save_last_hash(&dir, "abc123");
        assert_eq!(load_last_hash(&dir), Some("abc123".to_string()));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
