//! Sprint 22 Phase B — local package discovery producing a DRAFT recipe TOML.
//!
//! `package detect <path>` is strictly read-only: it inspects a binary,
//! probes its version output, matches it against systemd/SysV service
//! definitions, and finds related config files. It NEVER registers anything
//! with the server/tracker — it only renders a draft recipe for operator
//! review.

use anyhow::Result;
use regex::Regex;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// Where a service-unit scan should look. Injectable so tests can point
/// scans at temp directories instead of touching the real /etc tree.
#[derive(Debug, Clone, Default)]
pub struct ScanPaths {
    pub system_units: Vec<PathBuf>,
    pub user_units: Vec<PathBuf>,
    pub init_d: Vec<PathBuf>,
    pub proc_dir: Option<PathBuf>,
}

impl ScanPaths {
    /// Real system scan paths.
    pub fn system() -> Self {
        let mut system_units = vec![
            PathBuf::from("/etc/systemd/system"),
            PathBuf::from("/usr/lib/systemd/system"),
        ];
        system_units.retain(|p| p.is_dir());
        let mut user_units = vec![];
        if let Some(home) = std::env::var_os("HOME") {
            let p = PathBuf::from(home).join(".config/systemd/user");
            if p.is_dir() {
                user_units.push(p);
            }
        }
        let mut init_d = vec![PathBuf::from("/etc/init.d")];
        init_d.retain(|p| p.is_dir());
        let proc_dir = Some(PathBuf::from("/proc"));
        Self {
            system_units,
            user_units,
            init_d,
            proc_dir,
        }
    }
}

/// Confidence level attached to every detection finding and every draft field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    /// Binary evidence: exact resolved-path match in a unit file.
    High,
    /// Inference: basename match, /proc exe match, SysV script, etc.
    Medium,
    /// Convention only: nothing on disk confirmed it.
    Guess,
}

impl Confidence {
    pub fn as_str(&self) -> &'static str {
        match self {
            Confidence::High => "high",
            Confidence::Medium => "medium",
            Confidence::Guess => "guess",
        }
    }
}

/// Parsed ELF header info (first 20 bytes only — hand-rolled by design).
#[derive(Debug, Clone, Serialize)]
pub struct ElfInfo {
    pub arch: String,
    pub osabi: String,
}

/// Where a config-path finding came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigSource {
    UnitEnv,
    #[allow(dead_code)] // kept for config-source reporting parity
    UnitArg,
    Conventional,
}

impl ConfigSource {
    #[allow(dead_code)] // config-source reporting parity; not yet surfaced in CLI output
    pub fn as_str(&self) -> &'static str {
        match self {
            ConfigSource::UnitEnv => "unit-env",
            ConfigSource::UnitArg => "unit-arg",
            ConfigSource::Conventional => "conventional",
        }
    }
}

/// A candidate config file discovered for the package.
#[derive(Debug, Clone, Serialize)]
pub struct ConfigFinding {
    pub path: String,
    pub source: ConfigSource,
    pub exists: bool,
    pub confidence: Confidence,
}

/// A systemd unit whose ExecStart references the binary.
#[derive(Debug, Clone, Serialize)]
pub struct ServiceMatch {
    pub unit_name: String,
    pub unit_path: String,
    /// How the match was made: "execstart-path", "execstart-basename",
    /// "proc-exe", or "sysv".
    pub method: String,
    pub confidence: Confidence,
    /// PID matched via /proc/<pid>/exe (proc-exe method only).
    pub pid: Option<u32>,
}

/// Version-probe outcome.
#[derive(Debug, Clone, Serialize)]
#[allow(dead_code)] // version probe report struct; fields serialized only in some paths
pub struct VersionProbe {
    pub version: Option<String>,
    pub detect_command: Option<String>,
    pub detect_pattern: Option<String>,
}

/// Full report produced by [`detect`].
#[derive(Debug, Clone, Serialize)]
pub struct DetectReport {
    /// The path argument as given on the command line.
    pub path_arg: String,
    /// Canonicalized path (None if canonicalize failed).
    pub resolved_path: Option<String>,
    pub sha256: Option<String>,
    pub size: Option<u64>,
    pub elf: Option<ElfInfo>,
    pub version: Option<String>,
    /// Proposed [version] detect_command ("<resolved> --version" style).
    pub detect_command: Option<String>,
    /// Proposed [version] detect_pattern.
    pub detect_pattern: Option<String>,
    /// Set when the binary could not be probed (foreign arch, daemon context).
    pub needs_manual_verify: bool,
    pub service: Option<ServiceMatch>,
    pub config_findings: Vec<ConfigFinding>,
}

/// SHA-256 hex digest of raw bytes. Exposed for tests (golden vector).
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    hex::encode(h.finalize())
}

/// Parse the first 20 bytes of a file as an ELF header.
///
/// Returns `None` for non-ELF content — a shell script is a legal detection
/// target, not an error.
pub fn parse_elf_header(first20: &[u8]) -> Option<ElfInfo> {
    if first20.len() < 20 {
        return None;
    }
    if first20[0..4] != [0x7f, b'E', b'L', b'F'] {
        return None;
    }
    let e_machine = u16::from_le_bytes([first20[18], first20[19]]);
    let arch = match e_machine {
        0x3e => "x86_64".to_string(),
        0xb7 => "aarch64".to_string(),
        other => format!("unknown(0x{:x})", other),
    };
    let osabi = match first20[7] {
        0 => "sysv".to_string(),
        3 => "linux".to_string(),
        9 => "freebsd".to_string(),
        _ => "unknown".to_string(),
    };
    Some(ElfInfo { arch, osabi })
}

/// Extract a version string from probe output.
///
/// Matches e.g. `2.3.1`, `v1.10.2-beta.1`, `0.4`; returns `None` when the
/// text contains no plausible version token.
pub fn extract_version(text: &str) -> Option<String> {
    let re = Regex::new(r"(?i)\bv?[0-9]+\.[0-9]+(\.[0-9]+)?([-+][0-9A-Za-z.]+)?").ok()?;
    re.find(text)
        .map(|m| m.as_str().trim_start_matches(['v', 'V']))
        .filter(|s| s.as_bytes().first().is_some_and(|c| c.is_ascii_digit()))
        .map(|s| s.to_string())
}

/// Run `<path> --version` (and fallback argvs), bounded by a 2s timeout each.
///
/// Returns the extracted version plus the argv that produced it. The argv
/// slice is injectable for tests.
pub async fn probe_version(
    path: &Path,
    argvs: &[&[&str]],
    timeout: std::time::Duration,
) -> Result<Option<(String, Vec<String>)>> {
    for argv in argvs {
        let args: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
        match run_capture(path, &args, timeout).await {
            Some((stdout, stderr)) => {
                let combined = format!("{}\n{}", stdout, stderr);
                if let Some(v) = extract_version(&combined) {
                    let mut full = vec![path.display().to_string()];
                    full.extend(args.clone());
                    return Ok(Some((v, full)));
                }
            }
            None => continue,
        }
    }
    Ok(None)
}

/// Spawn the binary with `args`, capturing stdout+stderr. Returns None on
/// spawn failure or timeout.
async fn run_capture(
    path: &Path,
    args: &[String],
    timeout: std::time::Duration,
) -> Option<(String, String)> {
    use tokio::io::AsyncReadExt;
    let mut cmd = tokio::process::Command::new(path);
    cmd.args(args);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().ok()?;
    let mut stdout = String::new();
    let mut stderr = String::new();
    let mut out_pipe = child.stdout.take()?;
    let mut err_pipe = child.stderr.take()?;
    let read_out = async { out_pipe.read_to_string(&mut stdout).await };
    let read_err = async { err_pipe.read_to_string(&mut stderr).await };
    let (r1, r2) = tokio::join!(read_out, read_err);
    let _ = (r1, r2);
    // Bound the whole thing by timeout — a hung daemon must not block us.
    if tokio::time::timeout(timeout, child.wait()).await.is_err() {
        let _ = child.start_kill();
        return None;
    }
    Some((stdout, stderr))
}

/// Extract path-looking tokens from a unit file's Environment= lines and
/// ExecStart= argument list.
fn extract_unit_paths(contents: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in contents.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("Environment=") {
            for tok in rest.split_whitespace() {
                if let Some((_k, v)) = tok.split_once('=') {
                    let v = v.trim_matches('"');
                    if looks_like_path(v) {
                        out.push(v.to_string());
                    }
                }
            }
        } else if let Some(rest) = line.strip_prefix("ExecStart=") {
            for tok in rest.split_whitespace().skip(1) {
                if looks_like_path(tok) {
                    out.push(tok.to_string());
                }
            }
        }
    }
    out
}

/// Extract argv tokens (beyond argv[0]) from an ExecStart= line.
#[allow(dead_code)] // unit-file ExecStart parser; used when unit-arg config sources are enabled
fn extract_execstart_args(contents: &str) -> Vec<String> {
    for line in contents.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("ExecStart=") {
            return rest
                .split_whitespace()
                .skip(1)
                .map(|s| s.to_string())
                .collect();
        }
    }
    Vec::new()
}

fn looks_like_path(s: &str) -> bool {
    if s.len() < 2 || s.contains('`') || s.contains('$') {
        return false;
    }
    (s.starts_with('/') || s.starts_with('~')) && !s.contains('%')
}

/// Scan for a service unit matching the resolved path or its basename.
pub async fn match_service(
    resolved: Option<&Path>,
    raw: &Path,
    name_hint: Option<&str>,
    scan: &ScanPaths,
) -> Result<Option<ServiceMatch>> {
    let basename = raw
        .file_name()
        .map(|f| f.to_string_lossy().to_string())
        .unwrap_or_default();

    // 1. systemd unit files: ExecStart= contains resolved path or basename.
    let mut unit_dirs: Vec<&PathBuf> = scan.system_units.iter().collect();
    unit_dirs.extend(scan.user_units.iter());
    for dir in unit_dirs {
        let mut entries: Vec<PathBuf> = match std::fs::read_dir(dir) {
            Ok(rd) => rd.flatten().map(|e| e.path()).collect(),
            Err(_) => continue,
        };
        entries.sort();
        for unit in entries {
            let ext = unit.extension().map(|e| e.to_string_lossy().to_string());
            if ext.as_deref() != Some("service") {
                continue;
            }
            let contents = match std::fs::read_to_string(&unit) {
                Ok(c) => c,
                Err(_) => continue,
            };
            for line in contents.lines() {
                let line = line.trim();
                let Some(rest) = line.strip_prefix("ExecStart=") else {
                    continue;
                };
                let mut confidence = None;
                if let Some(res) = resolved {
                    if rest.contains(res.to_string_lossy().as_ref()) {
                        confidence = Some(Confidence::High);
                    }
                }
                if confidence.is_none() && !basename.is_empty() {
                    // Token-boundary match so "/opt/bin/foo" does not match
                    // "/opt/bin/foobar".
                    if rest.split_whitespace().any(|tok| {
                        tok.trim_start_matches('/')
                            .rsplit('/')
                            .next()
                            .map(|t| t == basename)
                            .unwrap_or(false)
                    }) {
                        confidence = Some(Confidence::Medium);
                    }
                }
                if let Some(conf) = confidence {
                    return Ok(Some(ServiceMatch {
                        unit_name: unit
                            .file_name()
                            .map(|f| f.to_string_lossy().to_string())
                            .unwrap_or_default(),
                        unit_path: unit.display().to_string(),
                        method: "execstart".to_string(),
                        confidence: conf,
                        pid: None,
                    }));
                }
            }
        }
    }

    // 2. /proc/*/exe readlink matches → running process.
    if let Some(proc_dir) = &scan.proc_dir {
        if let Ok(rd) = std::fs::read_dir(proc_dir) {
            for entry in rd.flatten() {
                let fname = entry.file_name();
                let fname = fname.to_string_lossy();
                let pid: u32 = match fname.parse() {
                    Ok(p) => p,
                    Err(_) => continue,
                };
                let exe_link = entry.path().join("exe");
                let Ok(target) = std::fs::read_link(&exe_link) else {
                    continue;
                };
                let target_str = target.to_string_lossy().to_string();
                let matched = resolved
                    .map(|r| target_str == r.to_string_lossy())
                    .unwrap_or(false)
                    || (!basename.is_empty()
                        && target_str.rsplit('/').next() == Some(basename.as_str()));
                if !matched {
                    continue;
                }
                let unit = cgroup_unit(&entry.path());
                return Ok(Some(ServiceMatch {
                    unit_name: unit.unwrap_or_default(),
                    unit_path: format!("/proc/{}/exe", pid),
                    method: "proc-exe".to_string(),
                    confidence: Confidence::Medium,
                    pid: Some(pid),
                }));
            }
        }
    }

    // 3. SysV fallback: /etc/init.d/* mentioning the path or basename.
    for dir in &scan.init_d {
        let mut entries: Vec<PathBuf> = match std::fs::read_dir(dir) {
            Ok(rd) => rd.flatten().map(|e| e.path()).collect(),
            Err(_) => continue,
        };
        entries.sort();
        for script in entries {
            let Ok(contents) = std::fs::read_to_string(&script) else {
                continue;
            };
            let hit = contents.lines().any(|l| {
                (resolved
                    .map(|r| l.contains(r.to_string_lossy().as_ref()))
                    .unwrap_or(false))
                    || (!basename.is_empty()
                        && l.split_whitespace()
                            .any(|tok| tok.rsplit('/').next() == Some(basename.as_str())))
            });
            if hit {
                return Ok(Some(ServiceMatch {
                    unit_name: script
                        .file_name()
                        .map(|f| f.to_string_lossy().to_string())
                        .unwrap_or_default(),
                    unit_path: script.display().to_string(),
                    method: "sysv".to_string(),
                    confidence: Confidence::Medium,
                    pid: None,
                }));
            }
        }
    }

    // name_hint alone is only convention — never claimed as a service match.
    let _ = name_hint;
    Ok(None)
}

/// Read /proc/<pid>/cgroup and pull the systemd unit name out of it.
fn cgroup_unit(proc_entry: &Path) -> Option<String> {
    let contents = std::fs::read_to_string(proc_entry.join("cgroup")).ok()?;
    for line in contents.lines() {
        // e.g. 0::/system.slice/foo.service
        if let Some(idx) = line.find("system.slice/") {
            let unit = &line[idx + "system.slice/".len()..];
            if !unit.is_empty() {
                return Some(unit.to_string());
            }
        }
    }
    None
}

/// Discover config files for the package.
pub async fn discover_configs(
    name: &str,
    service: Option<&ServiceMatch>,
    _scan: &ScanPaths,
) -> Result<Vec<ConfigFinding>> {
    let mut findings: Vec<ConfigFinding> = Vec::new();
    let mut seen = std::collections::HashSet::new();

    let mut push = |path: String, source: ConfigSource, confidence: Confidence| {
        if seen.insert(path.clone()) {
            findings.push(ConfigFinding {
                exists: Path::new(&path).exists(),
                path,
                source,
                confidence,
            });
        }
    };

    // 1. Paths referenced by the matched unit (Environment= and ExecStart args).
    if let Some(svc) = service {
        if svc.method == "execstart" {
            if let Ok(contents) = std::fs::read_to_string(&svc.unit_path) {
                for p in extract_unit_paths(&contents) {
                    push(p, ConfigSource::UnitEnv, Confidence::High);
                }
            }
        }
    }

    // 2. Conventional locations.
    let home = std::env::var("HOME").unwrap_or_default();
    let conventional = vec![
        format!("/etc/{}", name),
        format!("{}/.{}", home, name),
        format!("{}/.config/{}", home, name),
    ];
    for p in conventional {
        push(p, ConfigSource::Conventional, Confidence::Medium);
    }

    Ok(findings)
}

/// Read-only local detection: identity → version probe → service match →
/// config discovery. Never registers anything.
pub async fn detect(path: &Path, name_hint: Option<&str>) -> Result<DetectReport> {
    detect_with_scan(path, name_hint, &ScanPaths::system()).await
}

/// Like [`detect`] but with injectable scan paths (tests use temp trees).
pub async fn detect_with_scan(
    path: &Path,
    name_hint: Option<&str>,
    scan: &ScanPaths,
) -> Result<DetectReport> {
    let path_arg = path.display().to_string();

    // --- identity ---
    let resolved = std::fs::canonicalize(path).ok();
    let (sha, size) = match std::fs::read(path) {
        Ok(bytes) => (Some(sha256_hex(&bytes)), Some(bytes.len() as u64)),
        Err(_) => (None, None),
    };
    let elf = std::fs::read(path)
        .ok()
        .and_then(|bytes| parse_elf_header(&bytes[..bytes.len().min(20)]));

    // --- version probe ---
    let argvs: Vec<&[&str]> = vec![&["--version"], &["-V"], &["version"], &["-h"]];
    let probe = if path.exists() {
        probe_version(path, &argvs, std::time::Duration::from_secs(2)).await?
    } else {
        None
    };
    let (version, detect_command, detect_pattern, needs_manual_verify) = match probe {
        Some((v, argv)) => {
            let cmd = argv.join(" ");
            (Some(v), Some(cmd.clone()), Some(default_pattern()), false)
        }
        None => (None, None, None, true),
    };
    let _ = &detect_pattern;

    // --- service match ---
    let service = match_service(resolved.as_deref().map(Path::new), path, name_hint, scan).await?;

    // --- config discovery ---
    let name = name_hint
        .map(|s| s.to_string())
        .unwrap_or_else(|| basename_of(path));
    let config_findings = discover_configs(&name, service.as_ref(), scan).await?;

    Ok(DetectReport {
        path_arg,
        resolved_path: resolved.map(|p| p.display().to_string()),
        sha256: sha,
        size,
        elf,
        version,
        detect_command,
        detect_pattern: Some(default_pattern()),
        needs_manual_verify,
        service,
        config_findings,
    })
}

fn default_pattern() -> String {
    r"(?i)\bv?[0-9]+\.[0-9]+(\.[0-9]+)?([-+][0-9A-Za-z.]+)?".to_string()
}

fn basename_of(p: &Path) -> String {
    p.file_name()
        .map(|f| f.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

/// Render a DRAFT ShadowRecipe TOML for operator review.
///
/// Every field carries a `# confidence: ...` comment. Conventional paths that
/// do NOT exist on disk are emitted only as suggestion comment lines — never
/// as uncommented `config_paths` entries.
pub fn render_draft_toml(
    report: &DetectReport,
    recipe_name: &str,
    source_url: Option<&str>,
) -> String {
    let mut out = String::new();
    out.push_str("# DRAFT — operator review required; detection never auto-registers\n");
    out.push_str(&format!(
        "# Generated by `package detect {}`\n",
        report.path_arg
    ));
    if let Some(sha) = &report.sha256 {
        out.push_str(&format!("# Detected binary sha256: {}\n", sha));
    }
    if report.needs_manual_verify {
        out.push_str("# NOTE: version could not be probed automatically; verify manually.\n");
    }
    out.push('\n');

    // --- [package] ---
    let source = source_url.unwrap_or("REPLACE_ME");
    let source_type = if source == "REPLACE_ME" {
        "direct_url"
    } else if source.contains("github.com") && !source.ends_with(".git") {
        "github_release"
    } else if source.ends_with(".git") {
        "git_build"
    } else {
        "direct_url"
    };
    out.push_str("[package]\n");
    out.push_str(&format!(
        "name = \"{}\" # confidence: {}\n",
        recipe_name,
        if source_url.is_some() {
            "medium"
        } else {
            "guess"
        }
    ));
    if let Some(v) = &report.version {
        out.push_str(&format!("# current installed version: {}\n", v));
    }
    out.push_str(&format!(
        "source_type = \"{}\" # confidence: guess\n",
        source_type
    ));
    out.push_str(&format!(
        "source_url = \"{}\" # confidence: {}\n",
        source,
        if source_url.is_some() {
            "medium"
        } else {
            "guess"
        }
    ));
    out.push('\n');

    // --- [install] ---
    out.push_str("[install]\n");
    out.push_str("method = \"extract\" # confidence: guess\n");
    let target_dir = report
        .resolved_path
        .as_deref()
        .and_then(|p| Path::new(p).parent())
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "/usr/local/bin".to_string());
    out.push_str(&format!(
        "target_dir = \"{}\" # confidence: {}\n",
        target_dir,
        if report.resolved_path.is_some() {
            "medium"
        } else {
            "guess"
        }
    ));

    if let Some(svc) = &report.service {
        out.push_str(&format!(
            "service = \"{}\" # confidence: {} (matched via {})\n",
            svc.unit_name,
            svc.confidence.as_str(),
            svc.method
        ));
    }

    // config_paths: only paths confirmed to exist are uncommented.
    let existing: Vec<&ConfigFinding> =
        report.config_findings.iter().filter(|f| f.exists).collect();
    if !existing.is_empty() {
        out.push_str("config_paths = [");
        for (i, f) in existing.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            out.push_str(&format!("\"{}\"", f.path));
        }
        out.push_str("] # confidence: ");
        let worst = existing
            .iter()
            .map(|f| f.confidence)
            .min_by_key(|c| match c {
                Confidence::High => 0,
                Confidence::Medium => 1,
                Confidence::Guess => 2,
            })
            .unwrap_or(Confidence::Guess);
        out.push_str(worst.as_str());
        out.push('\n');
    }
    // Nonexistent conventional suggestions → comment lines only.
    for f in report.config_findings.iter().filter(|f| !f.exists) {
        out.push_str(&format!(
            "# suggestion (confidence: guess): \"{}\"\n",
            f.path
        ));
    }
    out.push('\n');

    // --- [version] ---
    out.push_str("[version]\n");
    match (&report.detect_command, &report.detect_pattern) {
        (Some(cmd), Some(pat)) => {
            // TOML basic strings must escape backslash and double-quote;
            // regex patterns are full of backslashes.
            let esc_cmd = cmd.replace('\\', "\\\\").replace('"', "\\\"");
            let esc_pat = pat.replace('\\', "\\\\").replace('"', "\\\"");
            out.push_str(&format!(
                "detect_command = \"{}\" # confidence: {}\n",
                esc_cmd,
                if report.needs_manual_verify {
                    "guess"
                } else {
                    "high"
                }
            ));
            out.push_str(&format!(
                "detect_pattern = \"{}\" # confidence: high\n",
                esc_pat
            ));
        }
        _ => {
            out.push_str("# detect_command = \"<fill in>\" # confidence: guess (probe failed)\n");
            out.push_str("# detect_pattern = \"<fill in>\" # confidence: guess (probe failed)\n");
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const EMPTY_SHA: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    #[test]
    fn sha256_empty_golden() {
        assert_eq!(sha256_hex(b""), EMPTY_SHA);
    }

    #[test]
    fn sha256_abc_golden() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    fn elf_bytes(osabi: u8) -> Vec<u8> {
        let mut b = vec![0x7fu8, b'E', b'L', b'F', 2, 1, 1, osabi];
        b.extend_from_slice(&[0u8; 8]);
        b.extend_from_slice(&[2, 0]); // e_type = ET_EXEC
        b.extend_from_slice(&[0x3e, 0]); // e_machine = x86_64
        b
    }

    #[test]
    fn elf_parse_x86_64_linux() {
        let info = parse_elf_header(&elf_bytes(3)).expect("should parse");
        assert_eq!(info.arch, "x86_64");
        assert_eq!(info.osabi, "linux");
    }

    #[test]
    fn elf_parse_non_elf_is_none() {
        assert!(parse_elf_header(b"#!/bin/sh\necho hi").is_none());
        assert!(parse_elf_header(b"short").is_none());
    }

    #[test]
    fn version_extract_simple() {
        assert_eq!(extract_version("thingamajig 2.3.1"), Some("2.3.1".into()));
    }

    #[test]
    fn version_extract_prerelease() {
        assert_eq!(
            extract_version("v1.10.2-beta.1"),
            Some("1.10.2-beta.1".into())
        );
    }

    #[test]
    fn version_extract_none() {
        assert_eq!(extract_version("no version here"), None);
    }

    fn write_unit(dir: &Path, name: &str, exec_start: &str) -> PathBuf {
        let p = dir.join(name);
        let mut f = std::fs::File::create(&p).unwrap();
        writeln!(f, "[Unit]\nDescription=test").unwrap();
        writeln!(f, "[Service]\nExecStart={}", exec_start).unwrap();
        p
    }

    fn scan_with(units: Vec<PathBuf>) -> ScanPaths {
        ScanPaths {
            system_units: units,
            user_units: vec![],
            init_d: vec![],
            proc_dir: None,
        }
    }

    #[tokio::test]
    async fn unit_scan_finds_service() {
        let tmp = tempfile::tempdir().unwrap();
        let unit = write_unit(
            tmp.path(),
            "foo.service",
            "/opt/bin/foo --config /etc/foo/foo.conf",
        );
        std::fs::create_dir_all("/etc/foo").ok();
        std::fs::write("/etc/foo/foo.conf", b"key=val\n").ok();
        let scan = scan_with(vec![tmp.path().to_path_buf()]);
        let m = match_service(None, Path::new("/opt/bin/foo"), None, &scan)
            .await
            .unwrap();
        assert!(m.is_some(), "expected basename match");
        let m = m.unwrap();
        assert_eq!(m.unit_name, "foo.service");
        assert_eq!(m.unit_path, unit.display().to_string());
    }

    #[tokio::test]
    async fn unit_scan_no_false_positive_on_prefix_basename() {
        let tmp = tempfile::tempdir().unwrap();
        write_unit(tmp.path(), "foo.service", "/opt/bin/foobar --serve");
        let scan = scan_with(vec![tmp.path().to_path_buf()]);
        let m = match_service(None, Path::new("/opt/bin/foo"), None, &scan)
            .await
            .unwrap();
        assert!(m.is_none(), "basename foo must not match foobar");
    }

    #[tokio::test]
    async fn config_discovery_from_unit_args() {
        let tmp = tempfile::tempdir().unwrap();
        // Create the referenced config INSIDE the temp tree so the test does
        // not depend on /etc. The unit points at the temp conf path.
        let conf = tmp.path().join("foo.conf");
        std::fs::write(&conf, b"k=v\n").unwrap();
        let unit = tmp.path().join("foo.service");
        std::fs::write(
            &unit,
            format!(
                "[Service]\nExecStart=/opt/bin/foo --config {}\n",
                conf.display()
            ),
        )
        .unwrap();
        let scan = scan_with(vec![tmp.path().to_path_buf()]);
        let svc = match_service(None, Path::new("/opt/bin/foo"), None, &scan)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(svc.unit_name, "foo.service");
        let findings = discover_configs("foo", Some(&svc), &scan).await.unwrap();
        let found = findings
            .iter()
            .find(|f| f.path == conf.display().to_string())
            .expect("unit-arg config should be discovered");
        assert!(found.exists);
        assert_eq!(found.source, ConfigSource::UnitEnv);
        assert_eq!(found.confidence, Confidence::High);
    }

    #[test]
    fn draft_toml_markers() {
        let report = DetectReport {
            path_arg: "/tmp/foo".into(),
            resolved_path: Some("/tmp/foo".into()),
            sha256: Some("deadbeef".into()),
            size: Some(10),
            elf: None,
            version: Some("1.2.3".into()),
            detect_command: Some("/tmp/foo --version".into()),
            detect_pattern: Some("([0-9]+\\.[0-9]+)".into()),
            needs_manual_verify: false,
            service: None,
            config_findings: vec![],
        };
        let toml = render_draft_toml(&report, "foo", None);
        assert!(toml.contains("# DRAFT"));
        assert!(toml.contains("confidence"));
        // detect_command line must carry a confidence comment
        assert!(toml
            .lines()
            .any(|l| l.starts_with("detect_command") && l.contains("# confidence:")));
    }

    #[test]
    fn draft_toml_never_emits_nonexistent_config_uncommented() {
        let report = DetectReport {
            path_arg: "/tmp/foo".into(),
            resolved_path: Some("/tmp/foo".into()),
            sha256: None,
            size: None,
            elf: None,
            version: None,
            detect_command: None,
            detect_pattern: None,
            needs_manual_verify: true,
            service: None,
            config_findings: vec![ConfigFinding {
                path: "/nonexistent/definitely/not/here.toml".into(),
                source: ConfigSource::Conventional,
                exists: false,
                confidence: Confidence::Guess,
            }],
        };
        let toml = render_draft_toml(&report, "foo", None);
        for line in toml.lines() {
            if line.starts_with("config_paths") {
                panic!("nonexistent path leaked into uncommented config_paths");
            }
        }
        assert!(toml.contains("# suggestion (confidence: guess)"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn probe_script_version() {
        let tmp = tempfile::tempdir().unwrap();
        let script = tmp.path().join("tool");
        {
            let mut f = std::fs::File::create(&script).unwrap();
            write!(f, "#!/bin/sh\necho \"tool 1.2.3\"\n").unwrap();
        }
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let argvs: Vec<&[&str]> = vec![&["--version"]];
        let result = probe_version(&script, &argvs, std::time::Duration::from_secs(5))
            .await
            .unwrap();
        let (version, _) = result.expect("probe should succeed");
        assert_eq!(version, "1.2.3");
    }

    #[tokio::test]
    async fn detect_nonexistent_path_flags_manual_verify() {
        let scan = ScanPaths::default();
        let report = detect_with_scan(Path::new("/nonexistent/path/binary"), None, &scan)
            .await
            .unwrap();
        assert!(report.needs_manual_verify);
        assert!(report.version.is_none());
    }
}
