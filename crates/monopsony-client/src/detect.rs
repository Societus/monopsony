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

use crate::shadow_cli::{detect_source_type, shadow_source_as_str};

/// Where a service-unit scan should look. Injectable so tests can point
/// scans at temp directories instead of touching the real /etc tree.
#[derive(Debug, Clone, Default)]
pub struct ScanPaths {
    pub system_units: Vec<PathBuf>,
    pub user_units: Vec<PathBuf>,
    pub init_d: Vec<PathBuf>,
    /// OpenRC runlevel directories (`/etc/runlevels` on Alpine). An init.d
    /// script with an entry under any of these is enabled (Sprint 31 1A);
    /// an absent entry keeps the match at Guess confidence.
    pub runlevels: Vec<PathBuf>,
    pub proc_dir: Option<PathBuf>,
    /// Injectible package-manager ownership probe. `None` (default/tests
    /// without an override) falls back to the real `probe_pm_ownership`.
    pub pm_probe: Option<fn(&Path) -> Option<bool>>,
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
        let mut runlevels = vec![PathBuf::from("/etc/runlevels")];
        runlevels.retain(|p| p.is_dir());
        let proc_dir = Some(PathBuf::from("/proc"));
        Self {
            system_units,
            user_units,
            init_d,
            runlevels,
            proc_dir,
            pm_probe: Some(probe_pm_ownership),
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
    /// How the match was made: "execstart" (systemd unit ExecStart=),
    /// "proc-exe" (/proc/<pid>/exe with a systemd slice unit), "container"
    /// (/proc/<pid>/exe inside a container cgroup scope — `unit_name` then
    /// holds the short container id and `unit_path` is prefixed with the
    /// engine name), "openrc", or "sysv" (/etc/init.d script).
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
    /// Parsed ExecStart= argv tokens (beyond argv[0]) from the matched unit.
    /// Consumed later by health checks (T1.4).
    pub exec_args: Vec<String>,
    /// `User=` from the matched unit, if any (T1.4; consumed by T1.3).
    pub unit_user: Option<String>,
    /// Package-manager ownership of the resolved binary (T1.1).
    /// `Some(true)`/`Some(false)` on a definitive probe answer; `None` when no
    /// package manager is available on the host.
    pub pm_managed: Option<bool>,
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

/// Extract `User=` from a unit file.
fn extract_unit_user(contents: &str) -> Option<String> {
    for line in contents.lines() {
        let line = line.trim();
        if let Some(user) = line.strip_prefix("User=") {
            return Some(user.trim().to_string());
        }
    }
    None
}

/// Extract config-dir/data values from ExecStart args of the form
/// `--data=/path`, `-data=/path`, `--config=/path`, `--datadir=/path`,
/// `--confdir=/path` (value attached with `=`). Used for `ConfigSource::UnitArg`.
fn extract_unit_arg_configs(contents: &str) -> Vec<String> {
    const PREFIXES: &[&str] = &["--data=", "-data=", "--config=", "--datadir=", "--confdir="];
    let mut out = Vec::new();
    for line in contents.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("ExecStart=") else {
            continue;
        };
        for tok in rest.split_whitespace().skip(1) {
            for prefix in PREFIXES {
                if let Some(value) = tok.strip_prefix(prefix) {
                    if !value.is_empty() && looks_like_path(value) {
                        out.push(value.to_string());
                    }
                }
            }
        }
    }
    out
}

/// Capitalize the first character of `s` (e.g. `sonarr` -> `Sonarr`).
fn capitalize_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
    }
}

/// Probe package-manager ownership of `path`.
///
/// Tries `dpkg -S`, `rpm -qf`, `pacman -Qo` in that order, using whichever
/// binary is available on the host. A definitive owned/unowned answer →
/// `Some(true)`/`Some(false)`; no package-manager binary found → `None`.
fn probe_pm_ownership(path: &Path) -> Option<bool> {
    const PROBES: &[(&str, &[&str])] =
        &[("dpkg", &["-S"]), ("rpm", &["-qf"]), ("pacman", &["-Qo"])];
    for &(pm, args) in PROBES {
        let mut cmd = std::process::Command::new(pm);
        cmd.args(args).arg(path);
        let output = match cmd.output() {
            Ok(o) => o,
            // Binary not on PATH → try the next package manager.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => continue,
        };
        // Exit status 0 = owned by a package; non-zero = not owned.
        return Some(output.status.success());
    }
    None
}

/// Name of the package manager available on this host (dpkg/rpm/pacman order),
/// used to label `pm-managed` draft comments. No process is spawned.
fn pm_name() -> Option<&'static str> {
    ["dpkg", "rpm", "pacman"].into_iter().find(|pm| {
        std::env::var_os("PATH")
            .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(pm).is_file()))
    })
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
                // Sprint 31 (1A): classify the cgroup before claiming a unit.
                // A systemd slice yields the unit name; a container scope
                // yields the short container id; neither means we must NOT
                // claim an empty unit (the authentik empty-service bug) —
                // the OpenRC/SysV tiers below still run for this binary.
                let cgroup = std::fs::read_to_string(entry.path().join("cgroup")).ok();
                if let Some(contents) = cgroup.as_deref() {
                    if let Some(unit) = cgroup_unit(contents) {
                        return Ok(Some(ServiceMatch {
                            unit_name: unit,
                            unit_path: format!("/proc/{}/exe", pid),
                            method: "proc-exe".to_string(),
                            confidence: Confidence::Medium,
                            pid: Some(pid),
                        }));
                    }
                    if let Some((engine, id)) = container_scope(contents) {
                        let engine = container_engine_hint().unwrap_or(engine);
                        return Ok(Some(ServiceMatch {
                            unit_name: id,
                            unit_path: format!("{}:/proc/{}/exe", engine, pid),
                            method: "container".to_string(),
                            confidence: Confidence::Guess,
                            pid: Some(pid),
                        }));
                    }
                }
                // No systemd unit and no container marker: fall through to
                // the init-script tiers instead of registering an empty
                // service name. Other pids still get their chance first.
            }
        }
    }

    // 3. OpenRC/SysV fallback: /etc/init.d/* mentioning the path or basename.
    // Sprint 31 (1A): the script content already in memory classifies the
    // init system — an openrc-run/runscript shebang or a depend() block is
    // OpenRC (shebang is authoritative even where systemd coexists), anything
    // else keeps the historical "sysv" label. Enablement is read from the
    // filesystem: a /etc/runlevels/<rl>/<name> entry lifts the match to
    // Medium; an unenabled script stays as a Guess so it is never silently
    // skipped. unit_name is the bare suffix-free script name, matching what
    // the Sprint 32 OpenRC renderer writes on the install side.
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
                let unit_name = script
                    .file_name()
                    .map(|f| f.to_string_lossy().to_string())
                    .unwrap_or_default();
                let method = if is_openrc_script(&contents) {
                    "openrc"
                } else {
                    "sysv"
                };
                let confidence = if runlevel_enabled(&scan.runlevels, &unit_name) {
                    Confidence::Medium
                } else {
                    Confidence::Guess
                };
                return Ok(Some(ServiceMatch {
                    unit_name,
                    unit_path: script.display().to_string(),
                    method: method.to_string(),
                    confidence,
                    pid: None,
                }));
            }
        }
    }

    // name_hint alone is only convention — never claimed as a service match.
    let _ = name_hint;
    Ok(None)
}

/// Pull the systemd unit name out of /proc/<pid>/cgroup content.
fn cgroup_unit(contents: &str) -> Option<String> {
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

/// Sprint 31 (1A): detect a container cgroup scope in /proc/<pid>/cgroup
/// content. Returns (engine, short container id) when a line carries one of
/// the known container scope prefixes; `None` for systemd slices and
/// unparseable content, so callers fall through instead of mislabeling.
fn container_scope(contents: &str) -> Option<(&'static str, String)> {
    const MARKERS: &[(&str, &str)] = &[
        ("libpod-", "podman"),
        ("docker-", "docker"),
        ("containerd-", "containerd"),
        ("crio-", "crio"),
    ];
    for line in contents.lines() {
        for (marker, engine) in MARKERS {
            if let Some(idx) = line.find(marker) {
                let rest = &line[idx + marker.len()..];
                let id = rest.split('.').next().unwrap_or("");
                if !id.is_empty() {
                    // Short id: the 12-char form `podman ps` reports.
                    let short: String = id.chars().take(12).collect();
                    return Some((engine, short));
                }
            }
        }
    }
    None
}

/// Sprint 31 (1A): engine identity from on-disk marker files, used when the
/// cgroup scope names a different runtime layer than the engine itself
/// (e.g. containerd-managed podman). Marker presence alone never claims a
/// container — it only refines the engine of an existing scope match.
fn container_engine_hint() -> Option<&'static str> {
    if Path::new("/run/.containerenv").exists() {
        return Some("podman");
    }
    if Path::new("/.dockerenv").exists() {
        return Some("docker");
    }
    None
}

/// Sprint 31 (1A): classify an /etc/init.d script as OpenRC. An
/// openrc-run/runscript shebang or a depend() block means OpenRC (the
/// shebang is authoritative even where systemd coexists); anything else is
/// treated as plain SysV.
fn is_openrc_script(contents: &str) -> bool {
    if let Some(shebang) = contents.lines().next() {
        if shebang.starts_with("#!")
            && (shebang.contains("openrc-run") || shebang.contains("runscript"))
        {
            return true;
        }
    }
    contents
        .lines()
        .any(|l| l.trim_start().starts_with("depend()"))
}

/// Sprint 31 (1A): is the init script enabled? An entry named `<name>` under
/// any runlevel directory means rc-update manages it. The configured dirs may
/// be `/etc/runlevels` itself (children = boot/default/…) or individual
/// runlevel dirs — both layouts are checked. Missing/unreadable dirs simply
/// yield false — the match stays at Guess, never an error.
fn runlevel_enabled(runlevels: &[PathBuf], name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    for dir in runlevels {
        if dir.join(name).exists() {
            return true;
        }
        let Ok(rd) = std::fs::read_dir(dir) else {
            continue;
        };
        for level in rd.flatten() {
            let level_path = level.path();
            if level_path.is_dir() && level_path.join(name).exists() {
                return true;
            }
        }
    }
    false
}

/// Sprint 31 I5: strip trailing '/' (except bare "/") so a UnitArg value
/// like "/var/lib/sonarr/" dedups against conventional "/var/lib/sonarr".
fn normalize_config_path(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        // "/" (or degenerate all-slash input): keep verbatim.
        path.to_string()
    } else {
        trimmed.to_string()
    }
}

/// Discover config files for the package.
pub async fn discover_configs(
    name: &str,
    service: Option<&ServiceMatch>,
    unit_user: Option<&str>,
    _scan: &ScanPaths,
) -> Result<Vec<ConfigFinding>> {
    let mut findings: Vec<ConfigFinding> = Vec::new();
    let mut seen = std::collections::HashSet::new();

    let mut push = |path: String, source: ConfigSource, confidence: Confidence| {
        // I5: dedup on the normalized path so slash-variants collapse into one finding.
        let norm = normalize_config_path(&path);
        if seen.insert(norm.clone()) {
            findings.push(ConfigFinding {
                exists: Path::new(&norm).exists(),
                path: norm,
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
                // T1.2: `--data=/path`, `--config=/path`, etc. (value attached
                // with `=`) outrank conventional guesses.
                for p in extract_unit_arg_configs(&contents) {
                    push(p, ConfigSource::UnitArg, Confidence::High);
                }
            }
        }
    }

    // 2. Conventional locations.
    let home = std::env::var("HOME").unwrap_or_default();
    let name_cap = capitalize_first(name);
    let mut conventional: Vec<(String, Confidence)> = vec![
        (format!("/etc/{}", name), Confidence::Medium),
        (format!("/var/lib/{}", name), Confidence::Medium), // T1.3
        (format!("{}/.{}", home, name), Confidence::Medium),
        (format!("{}/.config/{}", home, name), Confidence::Medium),
    ];
    // T1.3: user-aware candidate <home>/<user>/.config/<Name>.
    if let Some(u) = unit_user {
        conventional.push((
            format!("/home/{}/.config/{}", u, name_cap),
            Confidence::Medium,
        ));
    }
    // T1.3: deprioritize /root/* when the service does not run as root.
    let root_conf = if unit_user == Some("root") || unit_user.is_none() {
        Confidence::Medium
    } else {
        Confidence::Guess
    };
    conventional.push((format!("/root/.config/{}", name_cap), root_conf));
    conventional.push((format!("/root/{}", name), root_conf));

    for (path, confidence) in conventional {
        push(path, ConfigSource::Conventional, confidence);
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

    // T1.4: structured unit facts — ExecStart argv + `User=` from the matched unit.
    let (exec_args, unit_user) = service
        .as_ref()
        .and_then(|svc| std::fs::read_to_string(&svc.unit_path).ok())
        .map(|contents| {
            (
                extract_execstart_args(&contents),
                extract_unit_user(&contents),
            )
        })
        .unwrap_or_default();

    // T1.1: PM-ownership probe (injectable via scan.pm_probe).
    let pm_path = resolved.as_deref().unwrap_or(path);
    let pm_managed = match scan.pm_probe {
        Some(probe) => probe(pm_path),
        None => probe_pm_ownership(pm_path),
    };

    // --- config discovery ---
    let name = name_hint
        .map(|s| s.to_string())
        .unwrap_or_else(|| basename_of(path));
    let config_findings =
        discover_configs(&name, service.as_ref(), unit_user.as_deref(), scan).await?;

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
        exec_args,
        unit_user,
        pm_managed,
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

/// Strip a trailing ".git" (case-insensitive) from a source URL basename so
/// "https://github.com/owner/foo.git" yields repo basename "foo".
fn strip_git_suffix(name: &str) -> &str {
    if name.len() >= 4 && name.as_bytes()[name.len() - 4..].eq_ignore_ascii_case(b".git") {
        &name[..name.len() - 4]
    } else {
        name
    }
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
    if source_url.is_none() {
        out.push_str("# registration requires --source while source_url is REPLACE_ME\n");
    }
    if report.needs_manual_verify {
        out.push_str("# NOTE: version could not be probed automatically; verify manually.\n");
    }
    // T1.1: pm-ownership draft line.
    match (report.pm_managed, pm_name()) {
        (Some(false), Some(pm)) => {
            out.push_str(&format!(
                "# pm-managed: no (unowned by {pm}) — shadow candidate\n"
            ));
        }
        (Some(true), Some(pm)) => {
            out.push_str(&format!("# pm-managed: yes (owned by {pm})\n"));
        }
        (Some(_), None) => {}
        (None, _) => {}
    }
    out.push('\n');

    // --- [package] ---
    let source = source_url.unwrap_or("REPLACE_ME");
    let source_type = detect_source_type(source);
    let source_type_str = shadow_source_as_str(&source_type);
    // A real source URL is observable evidence; a REPLACE_ME placeholder is not.
    let source_conf = if source_url.is_some() {
        Confidence::Medium
    } else {
        Confidence::Guess
    };
    // Name confidence: recipe name matching the source repo basename
    // (case-insensitive) is real evidence; otherwise convention only.
    let name_conf = source_url
        .and_then(|url| {
            Path::new(url)
                .file_name()
                .map(|f| f.to_string_lossy().to_string())
        })
        .map(|repo_basename| {
            let repo_base = strip_git_suffix(&repo_basename);
            if recipe_name.to_lowercase() == repo_base.to_lowercase() {
                Confidence::Medium
            } else {
                Confidence::Guess
            }
        })
        .unwrap_or(Confidence::Guess);
    out.push_str("[package]\n");
    out.push_str(&format!(
        "name = \"{}\" # confidence: {}\n",
        recipe_name,
        name_conf.as_str()
    ));
    if let Some(v) = &report.version {
        out.push_str(&format!("# current installed version: {}\n", v));
    }
    out.push_str(&format!(
        "source_type = \"{}\" # confidence: {}\n",
        source_type_str,
        source_conf.as_str()
    ));
    out.push_str(&format!(
        "source_url = \"{}\" # confidence: {}\n",
        source,
        source_conf.as_str()
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
        // Sprint 31 (1A): lifecycle hints follow the matched init system —
        // never a hint with an empty unit name, and never systemctl for a
        // non-execstart method.
        if !svc.unit_name.is_empty() {
            match svc.method.as_str() {
                "execstart" => {
                    out.push_str(&format!(
                        "# pre_update = \"systemctl stop {}\" # confidence: {}\n",
                        svc.unit_name,
                        svc.confidence.as_str()
                    ));
                    out.push_str(&format!(
                        "# post_update = \"systemctl start {}\" # confidence: {}\n",
                        svc.unit_name,
                        svc.confidence.as_str()
                    ));
                }
                "openrc" => {
                    out.push_str(&format!(
                        "# pre_update = \"rc-service {} stop\" # confidence: {}\n",
                        svc.unit_name,
                        svc.confidence.as_str()
                    ));
                    out.push_str(&format!(
                        "# post_update = \"rc-service {} start\" # confidence: {}\n",
                        svc.unit_name,
                        svc.confidence.as_str()
                    ));
                    // Q3: an unenabled script (no runlevel entry) registers at
                    // Guess and carries its own enable hint so it is never
                    // silently skipped.
                    if svc.confidence == Confidence::Guess {
                        out.push_str(&format!(
                            "# rc-update add {} default to enable\n",
                            svc.unit_name
                        ));
                    }
                }
                "sysv" => {
                    out.push_str(&format!(
                        "# pre_update = \"/etc/init.d/{} stop\" # confidence: {}\n",
                        svc.unit_name,
                        svc.confidence.as_str()
                    ));
                    out.push_str(&format!(
                        "# post_update = \"/etc/init.d/{} start\" # confidence: {}\n",
                        svc.unit_name,
                        svc.confidence.as_str()
                    ));
                    out.push_str(
                        "# NOTE: sysv has no supervisor restart; verify the service manually.\n",
                    );
                }
                "container" => {
                    // The runtime supervises the process; there is no init
                    // unit to stop/start. A bare engine restart is the honest
                    // placeholder until Sprint 32 P2 lands wrapper discovery
                    // (compose/quadlet units).
                    let engine = svc.unit_path.split(':').next().unwrap_or("podman");
                    out.push_str(&format!(
                        "# restart_hint = \"{} restart {}\" # confidence: guess (placeholder)\n",
                        engine, svc.unit_name
                    ));
                    out.push_str(
                        "# NOTE: wrapper discovery (compose/quadlet units) is Sprint 32 P2.\n",
                    );
                }
                _ => {}
            }
            out.push_str(&format!(
                "# health_check = \"<fill in>\" # confidence: guess (verify against service {})\n",
                svc.unit_name
            ));
        }
    }

    // config_paths: only paths confirmed to exist are uncommented. I5 T1.3: when the
    // unit runs as a non-root user, /root/* findings were already demoted to Guess —
    // exclude them so they never become uncommented config_paths.
    let root_demoted = report.unit_user.as_deref().is_some_and(|u| u != "root");
    let existing: Vec<&ConfigFinding> = report
        .config_findings
        .iter()
        .filter(|f| f.exists && !(root_demoted && f.path.starts_with("/root/")))
        .collect();
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
    // Nonexistent conventional suggestions → comment lines only. I5 T1.3: demoted-but-existing
    // /root/* paths (non-root unit) still surface as suggestion comments so they never vanish.
    for f in report
        .config_findings
        .iter()
        .filter(|f| !f.exists || (root_demoted && f.path.starts_with("/root/")))
    {
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
            runlevels: vec![],
            proc_dir: None,
            pm_probe: None,
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
        let findings = discover_configs("foo", Some(&svc), None, &scan)
            .await
            .unwrap();
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
            exec_args: vec![],
            unit_user: None,
            pm_managed: None,
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
            exec_args: vec![],
            unit_user: None,
            pm_managed: None,
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

    // --- golden tests for T0.1–T0.4 (renderer correctness) ---

    fn draft_report() -> DetectReport {
        DetectReport {
            path_arg: "/opt/bin/foo".into(),
            resolved_path: Some("/opt/bin/foo".into()),
            sha256: Some("deadbeef".into()),
            size: Some(10),
            elf: None,
            version: Some("1.2.3".into()),
            exec_args: vec![],
            unit_user: None,
            pm_managed: None,
            detect_command: Some("/opt/bin/foo --version".into()),
            detect_pattern: Some(r"([0-9]+\.[0-9]+)".into()),
            needs_manual_verify: false,
            service: None,
            config_findings: vec![],
        }
    }

    #[test]
    fn draft_toml_uses_shared_source_type() {
        // No duplication: source_type comes from shadow_cli::detect_source_type.
        let toml = render_draft_toml(&draft_report(), "foo", Some("https://github.com/owner/foo"));
        assert!(toml.contains("source_type = \"github_release\""));
        assert!(toml.contains("source_url = \"https://github.com/owner/foo\""));

        let toml = render_draft_toml(
            &draft_report(),
            "foo",
            Some("https://github.com/owner/foo.git"),
        );
        assert!(toml.contains("source_type = \"git_release\""));

        let toml = render_draft_toml(
            &draft_report(),
            "foo",
            Some("https://pkgs.example.com/stable"),
        );
        assert!(toml.contains("source_type = \"direct_url\""));

        // No source → REPLACE_ME placeholder kept, typed direct_url.
        let toml = render_draft_toml(&draft_report(), "foo", None);
        assert!(toml.contains("source_url = \"REPLACE_ME\""));
        assert!(toml.contains("source_type = \"direct_url\""));
    }

    #[test]
    fn draft_toml_source_confidence_follows_source() {
        let with_source =
            render_draft_toml(&draft_report(), "foo", Some("https://github.com/owner/foo"));
        assert!(with_source.contains("source_type = \"github_release\" # confidence: medium"));
        assert!(with_source
            .contains("source_url = \"https://github.com/owner/foo\" # confidence: medium"));

        let without = render_draft_toml(&draft_report(), "foo", None);
        assert!(without.contains("source_type = \"direct_url\" # confidence: guess"));
        assert!(without.contains("source_url = \"REPLACE_ME\" # confidence: guess"));
    }

    #[test]
    fn draft_toml_name_confidence_repo_basename_match() {
        // Case-insensitive basename match, trailing .git stripped → medium.
        let toml = render_draft_toml(
            &draft_report(),
            "Foo",
            Some("https://github.com/owner/foo.git"),
        );
        assert!(toml.contains("name = \"Foo\" # confidence: medium"));

        let toml = render_draft_toml(&draft_report(), "foo", Some("https://github.com/OWNER/Foo"));
        assert!(toml.contains("name = \"foo\" # confidence: medium"));

        // No match → guess.
        let toml = render_draft_toml(
            &draft_report(),
            "bar",
            Some("https://github.com/owner/foo.git"),
        );
        assert!(toml.contains("name = \"bar\" # confidence: guess"));

        // No source → guess (kills the arbitrary source_url.is_some() proxy).
        let toml = render_draft_toml(&draft_report(), "foo", None);
        assert!(toml.contains("name = \"foo\" # confidence: guess"));
    }

    #[test]
    fn draft_toml_lifecycle_suggestions_when_service_matched() {
        let mut report = draft_report();
        report.service = Some(ServiceMatch {
            unit_name: "foo.service".into(),
            unit_path: "/tmp/foo.service".into(),
            method: "execstart".into(),
            confidence: Confidence::Medium,
            pid: None,
        });
        let toml = render_draft_toml(&report, "foo", None);
        assert!(
            toml.contains("service = \"foo.service\" # confidence: medium (matched via execstart)")
        );
        assert!(toml.contains("# pre_update = \"systemctl stop foo.service\" # confidence: medium"));
        assert!(
            toml.contains("# post_update = \"systemctl start foo.service\" # confidence: medium")
        );
        assert!(toml.contains(
            "# health_check = \"<fill in>\" # confidence: guess (verify against service foo.service)"
        ));
        // Suggestions only: never uncommented.
        assert!(!toml.lines().any(|l| l.starts_with("pre_update =")));
        assert!(!toml.lines().any(|l| l.starts_with("post_update =")));
        assert!(!toml.lines().any(|l| l.starts_with("health_check =")));
    }

    #[test]
    fn draft_toml_replace_me_header() {
        let without = render_draft_toml(&draft_report(), "foo", None);
        assert!(without.contains("# registration requires --source while source_url is REPLACE_ME"));
        let with_source =
            render_draft_toml(&draft_report(), "foo", Some("https://github.com/owner/foo"));
        assert!(!with_source.contains("registration requires --source"));
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

    // --- T1.1: PM-ownership probe ---

    fn test_scan(pm_probe: Option<fn(&Path) -> Option<bool>>) -> ScanPaths {
        ScanPaths {
            system_units: vec![],
            user_units: vec![],
            init_d: vec![],
            runlevels: vec![],
            proc_dir: None,
            pm_probe,
        }
    }

    #[tokio::test]
    async fn detect_pm_managed_injected_unowned() {
        let report = detect_with_scan(
            Path::new("/opt/bin/foo"),
            None,
            &test_scan(Some(|_path| Some(false))),
        )
        .await
        .unwrap();
        assert_eq!(report.pm_managed, Some(false));
    }

    #[tokio::test]
    async fn detect_pm_managed_injected_owned() {
        let report = detect_with_scan(
            Path::new("/opt/bin/foo"),
            None,
            &test_scan(Some(|_path| Some(true))),
        )
        .await
        .unwrap();
        assert_eq!(report.pm_managed, Some(true));
    }

    #[tokio::test]
    async fn detect_pm_managed_injected_none() {
        let report = detect_with_scan(
            Path::new("/opt/bin/foo"),
            None,
            &test_scan(Some(|_path| None)),
        )
        .await
        .unwrap();
        assert_eq!(report.pm_managed, None);
    }

    #[tokio::test]
    async fn detect_pm_managed_real_probe_unowned() {
        // No probe injected → falls back to probe_pm_ownership. pacman is
        // available on this host, so a nonexistent path yields Some(false).
        let report = detect_with_scan(
            Path::new("/nonexistent/path/binary"),
            None,
            &test_scan(None),
        )
        .await
        .unwrap();
        assert_eq!(report.pm_managed, Some(false));
    }

    #[test]
    fn draft_toml_pm_managed_emission() {
        let host_pm = pm_name();
        let base = draft_report();

        // None → no pm-managed line.
        let mut none = base.clone();
        none.pm_managed = None;
        let toml = render_draft_toml(&none, "foo", None);
        assert!(!toml.lines().any(|l| l.starts_with("# pm-managed:")));

        // Some(false) → unowned shadow-candidate line.
        let mut unowned = base.clone();
        unowned.pm_managed = Some(false);
        let toml = render_draft_toml(&unowned, "foo", None);
        assert!(toml.contains("# pm-managed: no (unowned by"));
        assert!(toml.contains("shadow candidate"));
        if let Some(pm) = host_pm {
            assert!(toml.contains(&format!("unowned by {pm}")));
        }

        // Some(true) → owned line.
        let mut owned = base.clone();
        owned.pm_managed = Some(true);
        let toml = render_draft_toml(&owned, "foo", None);
        assert!(toml.contains("# pm-managed: yes (owned by"));
        if let Some(pm) = host_pm {
            assert!(toml.contains(&format!("owned by {pm}")));
        }
    }

    // --- T1.2: ConfigSource::UnitArg outranks conventional ---

    #[tokio::test]
    async fn config_discovery_unit_arg_outranks_conventional() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&data_dir).unwrap();
        let unit = tmp.path().join("sonarr.service");
        let exec = format!("/opt/sonarr/Sonarr --data={}", data_dir.display());
        std::fs::write(
            &unit,
            format!(
                "[Unit]\nDescription=sonarr\n[Service]\nExecStart={}\n",
                exec
            ),
        )
        .unwrap();
        let scan = scan_with(vec![tmp.path().to_path_buf()]);
        let svc = match_service(None, Path::new("/opt/sonarr/Sonarr"), None, &scan)
            .await
            .unwrap()
            .unwrap();
        let findings = discover_configs("sonarr", Some(&svc), None, &scan)
            .await
            .unwrap();
        let data = findings
            .iter()
            .find(|f| f.path == data_dir.display().to_string())
            .expect("unit-arg data dir should be discovered");
        assert_eq!(data.source, ConfigSource::UnitArg);
        assert_eq!(data.confidence, Confidence::High);
        assert!(data.exists);
        // UnitArg findings must rank before any Conventional finding.
        let unit_arg_pos = findings
            .iter()
            .position(|f| f.source == ConfigSource::UnitArg)
            .expect("unit-arg finding present");
        let first_conventional = findings
            .iter()
            .position(|f| f.source == ConfigSource::Conventional);
        assert!(first_conventional.map(|c| c > unit_arg_pos).unwrap_or(true));
    }

    // --- T1.3: user-aware search space ---

    #[tokio::test]
    async fn discover_configs_user_aware() {
        let tmp = tempfile::tempdir().unwrap();
        let unit = tmp.path().join("sonarr.service");
        std::fs::write(
            &unit,
            "[Unit]\nDescription=sonarr\n[Service]\nUser=sonarr\nExecStart=/opt/sonarr/Sonarr\n",
        )
        .unwrap();
        let scan = scan_with(vec![tmp.path().to_path_buf()]);
        let svc = match_service(None, Path::new("/opt/sonarr/Sonarr"), None, &scan)
            .await
            .unwrap()
            .unwrap();
        let findings = discover_configs("sonarr", Some(&svc), Some("sonarr"), &scan)
            .await
            .unwrap();
        let var_lib = findings
            .iter()
            .find(|f| f.path == "/var/lib/sonarr")
            .expect("/var/lib/sonarr candidate");
        assert_eq!(var_lib.source, ConfigSource::Conventional);
        assert_eq!(var_lib.confidence, Confidence::Medium);
        let home_cfg = findings
            .iter()
            .find(|f| f.path == "/home/sonarr/.config/Sonarr")
            .expect("/home/sonarr/.config/Sonarr candidate");
        assert_eq!(home_cfg.confidence, Confidence::Medium);
        // /root/* deprioritized to Guess when User != root.
        let root_cfg = findings
            .iter()
            .find(|f| f.path == "/root/.config/Sonarr")
            .expect("/root candidate present");
        assert_eq!(root_cfg.confidence, Confidence::Guess);
        let root_pos = findings
            .iter()
            .position(|f| f.path == "/root/.config/Sonarr")
            .unwrap();
        assert!(
            root_pos
                > findings
                    .iter()
                    .position(|f| f.path == "/var/lib/sonarr")
                    .unwrap()
        );
    }

    // --- I5: config_paths slash-dedup + /root demotion in the renderer ---

    #[test]
    fn normalize_config_path_strips_trailing_slash() {
        assert_eq!(normalize_config_path("/var/lib/x/"), "/var/lib/x");
        // Bare "/" stays; no-slash unchanged; all-slash input kept verbatim.
        assert_eq!(normalize_config_path("/"), "/");
        assert_eq!(normalize_config_path("//"), "//");
        assert_eq!(normalize_config_path("/var/lib/x"), "/var/lib/x");
    }

    #[test]
    fn root_demoted_existing_findings_never_uncommented_but_suggested() {
        // Existing /root/.config/Sonarr finding + non-root unit user.
        let mut report = draft_report();
        report.unit_user = Some("sonarr".to_string());
        report.config_findings = vec![ConfigFinding {
            path: "/root/.config/Sonarr".into(),
            source: ConfigSource::Conventional,
            exists: true,
            confidence: Confidence::Guess,
        }];
        let toml = render_draft_toml(&report, "sonarr", None);
        // The /root path must NOT be uncommented as a config_paths entry...
        for line in toml.lines() {
            if line.starts_with("config_paths") && line.contains("/root/.config/Sonarr") {
                panic!("demoted /root path leaked into uncommented config_paths");
            }
        }
        // ...but it still appears as a suggestion comment so it is not lost.
        assert!(toml.contains("# suggestion (confidence: guess): \"/root/.config/Sonarr\""));
    }

    #[test]
    fn root_findings_stay_uncommented_for_root_or_none_user() {
        // unit_user = Some("root") → /root path stays uncommented, no suggestion.
        let mut report = draft_report();
        report.unit_user = Some("root".to_string());
        report.config_findings = vec![ConfigFinding {
            path: "/root/.config/Sonarr".into(),
            source: ConfigSource::Conventional,
            exists: true,
            confidence: Confidence::Guess,
        }];
        let toml = render_draft_toml(&report, "sonarr", None);
        assert!(toml
            .lines()
            .any(|l| l.starts_with("config_paths") && l.contains("/root/.config/Sonarr")));
        assert!(!toml.contains("# suggestion (confidence: guess): \"/root/.config/Sonarr\""));

        // unit_user = None → same behavior.
        report.unit_user = None;
        let toml2 = render_draft_toml(&report, "sonarr", None);
        assert!(toml2
            .lines()
            .any(|l| l.starts_with("config_paths") && l.contains("/root/.config/Sonarr")));
        assert!(!toml2.contains("# suggestion (confidence: guess): \"/root/.config/Sonarr\""));
    }

    // --- T1.4: structured unit facts on DetectReport ---

    #[tokio::test]
    async fn detect_structured_unit_facts() {
        let tmp = tempfile::tempdir().unwrap();
        let unit = tmp.path().join("sonarr.service");
        std::fs::write(
            &unit,
            "[Unit]\nDescription=sonarr\n[Service]\nUser=sonarr\nExecStart=/opt/sonarr/Sonarr --data=/var/lib/sonarr --browser\n",
        )
        .unwrap();
        let scan = scan_with(vec![tmp.path().to_path_buf()]);
        let report = detect_with_scan(Path::new("/opt/sonarr/Sonarr"), Some("sonarr"), &scan)
            .await
            .unwrap();
        assert_eq!(report.unit_user, Some("sonarr".to_string()));
        assert_eq!(
            report.exec_args,
            vec![
                "--data=/var/lib/sonarr".to_string(),
                "--browser".to_string(),
            ]
        );
        assert_eq!(
            report.service.as_ref().map(|s| s.unit_name.as_str()),
            Some("sonarr.service")
        );
    }

    // --- Sprint 31 (1A): OpenRC classification + container marker ---

    fn service_scan(
        init_d: Vec<PathBuf>,
        runlevels: Vec<PathBuf>,
        proc_dir: Option<PathBuf>,
    ) -> ScanPaths {
        ScanPaths {
            system_units: vec![],
            user_units: vec![],
            init_d,
            runlevels,
            proc_dir,
            pm_probe: None,
        }
    }

    fn write_init_script(dir: &Path, name: &str, contents: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, contents).unwrap();
        p
    }

    #[cfg(unix)]
    fn fake_pid(proc_dir: &Path, pid: u32, exe_target: &str, cgroup: &str) {
        let pid_dir = proc_dir.join(pid.to_string());
        std::fs::create_dir_all(&pid_dir).unwrap();
        std::os::unix::fs::symlink(exe_target, pid_dir.join("exe")).unwrap();
        std::fs::write(pid_dir.join("cgroup"), cgroup).unwrap();
    }

    #[tokio::test]
    async fn init_script_openrc_shebang_classifies_openrc() {
        let tmp = tempfile::tempdir().unwrap();
        let init_d = tmp.path().join("init.d");
        std::fs::create_dir_all(&init_d).unwrap();
        let script = write_init_script(
            &init_d,
            "sshd",
            "#!/sbin/openrc-run\n\ncommand=/usr/sbin/sshd\ncommand_args=\"-D\"\n\ndepend() {\n\tuse logger\n}\n",
        );
        let scan = service_scan(vec![init_d], vec![], None);
        let m = match_service(None, Path::new("/usr/sbin/sshd"), None, &scan)
            .await
            .unwrap()
            .expect("init.d basename match");
        assert_eq!(m.method, "openrc");
        // Bare suffix-free name — round-trips into `rc-service` and matches
        // the Sprint 32 OpenRC renderer's write-side convention.
        assert_eq!(m.unit_name, "sshd");
        assert_eq!(m.unit_path, script.display().to_string());
        // No runlevel dirs scanned → unenabled → Guess (never silently
        // skipped — the match itself is still returned).
        assert_eq!(m.confidence, Confidence::Guess);
    }

    #[tokio::test]
    async fn init_script_depend_block_classifies_openrc() {
        // Q6: a script whose shebang is not openrc-run but which carries a
        // depend() block is OpenRC by content.
        let tmp = tempfile::tempdir().unwrap();
        let init_d = tmp.path().join("init.d");
        std::fs::create_dir_all(&init_d).unwrap();
        write_init_script(
            &init_d,
            "sshd",
            "#!/bin/sh\n\ncommand=/usr/sbin/sshd\n\ndepend() {\n\tneed net\n}\n",
        );
        let scan = service_scan(vec![init_d], vec![], None);
        let m = match_service(None, Path::new("/usr/sbin/sshd"), None, &scan)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(m.method, "openrc");
    }

    #[tokio::test]
    async fn init_script_plain_sh_stays_sysv() {
        let tmp = tempfile::tempdir().unwrap();
        let init_d = tmp.path().join("init.d");
        std::fs::create_dir_all(&init_d).unwrap();
        write_init_script(
            &init_d,
            "foo",
            "#!/bin/sh\ncase \"$1\" in\n  start) /opt/bin/foo ;;\nesac\n",
        );
        let scan = service_scan(vec![init_d], vec![], None);
        let m = match_service(None, Path::new("/opt/bin/foo"), None, &scan)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(m.method, "sysv");
        assert_eq!(m.unit_name, "foo");
        assert_eq!(m.confidence, Confidence::Guess);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn init_script_openrc_runlevel_entry_lifts_to_medium() {
        let tmp = tempfile::tempdir().unwrap();
        let init_d = tmp.path().join("init.d");
        std::fs::create_dir_all(&init_d).unwrap();
        let script = write_init_script(
            &init_d,
            "sshd",
            "#!/sbin/openrc-run\ncommand=/usr/sbin/sshd\n",
        );
        let runlevels = tmp.path().join("runlevels");
        let default = runlevels.join("default");
        std::fs::create_dir_all(&default).unwrap();
        std::os::unix::fs::symlink(&script, default.join("sshd")).unwrap();
        let scan = service_scan(vec![init_d], vec![runlevels], None);
        let m = match_service(None, Path::new("/usr/sbin/sshd"), None, &scan)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(m.method, "openrc");
        assert_eq!(m.confidence, Confidence::Medium);
        // Same tree without the runlevel entry → back to Guess.
        std::fs::remove_file(default.join("sshd")).unwrap();
        let m = match_service(None, Path::new("/usr/sbin/sshd"), None, &scan)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(m.confidence, Confidence::Guess);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn proc_exe_systemd_slice_still_claims_unit() {
        let tmp = tempfile::tempdir().unwrap();
        let proc_dir = tmp.path().join("proc");
        fake_pid(
            &proc_dir,
            4242,
            "/opt/sonarr/Sonarr",
            "0::/system.slice/sonarr.service\n",
        );
        let scan = service_scan(vec![], vec![], Some(proc_dir));
        let m = match_service(None, Path::new("/opt/sonarr/Sonarr"), None, &scan)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(m.method, "proc-exe");
        assert_eq!(m.unit_name, "sonarr.service");
        assert_eq!(m.confidence, Confidence::Medium);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn proc_exe_without_slice_or_container_claims_nothing() {
        // Authentik regression: a /proc exe match whose cgroup has no
        // system.slice unit and no container scope must NOT claim an empty
        // service name — the init-script tiers run instead (None here).
        let tmp = tempfile::tempdir().unwrap();
        let proc_dir = tmp.path().join("proc");
        fake_pid(&proc_dir, 4242, "/usr/sbin/authentik", "0::/\n");
        let scan = service_scan(vec![], vec![], Some(proc_dir));
        let m = match_service(None, Path::new("/usr/sbin/authentik"), None, &scan)
            .await
            .unwrap();
        assert!(m.is_none(), "empty-name service claim must not happen");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn proc_exe_libpod_scope_yields_container_short_id() {
        let tmp = tempfile::tempdir().unwrap();
        let proc_dir = tmp.path().join("proc");
        fake_pid(
            &proc_dir,
            99,
            "/usr/sbin/authentik",
            "0::/libpod-3fa9c4d1e5b6a7a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f6071829.scope\n",
        );
        let scan = service_scan(vec![], vec![], Some(proc_dir));
        let m = match_service(None, Path::new("/usr/sbin/authentik"), None, &scan)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(m.method, "container");
        // Short id: the 12-char form the engine CLI reports.
        assert_eq!(m.unit_name, "3fa9c4d1e5b6");
        assert_eq!(m.confidence, Confidence::Guess);
        // Engine identity rides in unit_path (Q4); the exact engine name
        // depends on host marker files, so only the shape is pinned here.
        assert!(
            m.unit_path.ends_with(":/proc/99/exe"),
            "unit_path must carry engine prefix + proc path, got {}",
            m.unit_path
        );
    }

    #[test]
    fn draft_toml_lifecycle_hints_follow_method() {
        let cases = [
            ("execstart", "# pre_update = \"systemctl stop u\""),
            ("openrc", "# pre_update = \"rc-service u stop\""),
            ("sysv", "# pre_update = \"/etc/init.d/u stop\""),
        ];
        for (method, expected_pre) in cases {
            let mut report = draft_report();
            report.service = Some(ServiceMatch {
                unit_name: "u".into(),
                unit_path: "/tmp/u".into(),
                method: method.into(),
                confidence: Confidence::Medium,
                pid: None,
            });
            let toml = render_draft_toml(&report, "foo", None);
            assert!(
                toml.contains(expected_pre),
                "method {}: expected '{}' in draft",
                method,
                expected_pre
            );
        }
    }

    #[test]
    fn draft_toml_openrc_never_emits_systemctl_and_carries_enable_hint() {
        let mut report = draft_report();
        report.service = Some(ServiceMatch {
            unit_name: "sshd".into(),
            unit_path: "/etc/init.d/sshd".into(),
            method: "openrc".into(),
            confidence: Confidence::Guess,
            pid: None,
        });
        let toml = render_draft_toml(&report, "sshd", None);
        assert!(toml.contains("# pre_update = \"rc-service sshd stop\""));
        assert!(toml.contains("# post_update = \"rc-service sshd start\""));
        // Q3: unenabled script → Guess + explicit enable comment.
        assert!(toml.contains("# rc-update add sshd default to enable"));
        assert!(!toml.contains("systemctl"));
    }

    #[test]
    fn draft_toml_container_hint_is_placeholder_not_systemctl() {
        let mut report = draft_report();
        report.service = Some(ServiceMatch {
            unit_name: "3fa9c4d1e5b6".into(),
            unit_path: "podman:/proc/99/exe".into(),
            method: "container".into(),
            confidence: Confidence::Guess,
            pid: Some(99),
        });
        let toml = render_draft_toml(&report, "authentik", None);
        assert!(toml.contains("# restart_hint = \"podman restart 3fa9c4d1e5b6\""));
        assert!(toml.contains("Sprint 32 P2"));
        assert!(!toml.contains("systemctl"));
    }

    #[test]
    fn draft_toml_never_emits_lifecycle_hint_with_empty_unit_name() {
        let mut report = draft_report();
        report.service = Some(ServiceMatch {
            unit_name: String::new(),
            unit_path: "/proc/1/exe".into(),
            method: "proc-exe".into(),
            confidence: Confidence::Medium,
            pid: Some(1),
        });
        let toml = render_draft_toml(&report, "foo", None);
        for line in toml.lines() {
            assert!(
                !line.starts_with("# pre_update")
                    && !line.starts_with("# post_update")
                    && !line.starts_with("# health_check"),
                "empty unit name leaked into a lifecycle hint: {}",
                line
            );
        }
    }
}
