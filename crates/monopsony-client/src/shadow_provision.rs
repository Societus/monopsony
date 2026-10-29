//! Provisioning support for Build-method shadow recipes (Sprint 32 Phase 1).
//!
//! Three concerns live here, all operating on `[build].steps` text:
//! - legacy brace translation: already-registered recipes use single-brace
//!   `{target_dir}`-style tokens (the asset-template style); the Phase 1
//!   engine is double-brace (`{{target_dir}}`). Translation happens at the
//!   executor boundary so a step is never interpreted by both engines
//!   (decision D1 in docs/plans/sprint32-grounding.md).
//! - `--source` resolution: exists → use as-is, missing → pull the fallback
//!   (contract: recipes/shadow/fixtures/phase1-source-location.md).
//! - build-step execution under the recipe's network policy.
//!
//! Minimal-subset discipline: steps stay plain strings; nothing here is a
//! task framework. Render FIRST (engine in monopsony-common), then resolve
//! `--source` on the rendered text, then execute.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};

use crate::shadow::{self, NetworkPolicy};

/// The five StepContext names eligible for legacy single-brace translation.
const LEGACY_NAMES: [&str; 5] = ["target_dir", "version", "arch", "os", "libc"];

/// Rewrite legacy single-brace tokens (`{version}`) into the Phase 1
/// engine's double-brace form (`{{version}}`) for the five known context
/// names. Exact-token matching only: a `{` that is part of an existing
/// `{{...}}` directive, a name outside the five, or a token whose closing
/// brace is itself doubled passes through untouched, so no step is ever
/// interpreted by both engines.
pub fn translate_legacy_braces(step: &str) -> String {
    let bytes = step.as_bytes();
    let mut out = String::with_capacity(step.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{' && !(i > 0 && bytes[i - 1] == b'{') {
            let mut matched = false;
            for name in LEGACY_NAMES {
                let name_end = i + 1 + name.len();
                // Needs `{name}` exactly: the name followed by a single `}`
                // (a `}}` would mean the step already uses the new engine).
                if name_end < bytes.len()
                    && &step[i + 1..name_end] == name
                    && bytes[name_end] == b'}'
                    && !(name_end + 1 < bytes.len() && bytes[name_end + 1] == b'}')
                {
                    out.push_str("{{");
                    out.push_str(name);
                    out.push_str("}}");
                    i = name_end + 1;
                    matched = true;
                    break;
                }
            }
            if matched {
                continue;
            }
        }
        let ch = step[i..].chars().next().unwrap_or('{');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Expand a leading `~` / `~/` against the invoking user's home dir.
/// D2: tilde stays literal through rendering; expansion is executor-side
/// (existence checks here; the steps themselves run under bash, which
/// expands `$HOME`/`~` naturally).
pub fn expand_tilde(path: &str) -> PathBuf {
    let trimmed = path.trim();
    if trimmed == "~" {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home);
        }
    } else if let Some(rest) = trimmed.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return Path::new(&home).join(rest);
        }
    }
    PathBuf::from(trimmed)
}

/// One whitespace-delimited token plus its byte span in the step string.
struct Token<'a> {
    start: usize,
    end: usize,
    text: &'a str,
}

fn tokens(step: &str) -> Vec<Token<'_>> {
    let mut out = Vec::new();
    let mut cursor = 0;
    while let Some(rel) = step[cursor..].find(|c: char| !c.is_whitespace()) {
        let start = cursor + rel;
        let end = step[start..]
            .find(char::is_whitespace)
            .map_or(step.len(), |e| start + e);
        out.push(Token {
            start,
            end,
            text: &step[start..end],
        });
        cursor = end;
    }
    out
}

/// True when `path` is operator-managed per `install.config_paths`: an exact
/// entry match, or a file living inside a config-path directory (component
/// prefix match). Tilde is expanded on the config entry the same way as on
/// the step path so both sides compare literally.
fn is_managed_config_path(path: &Path, config_paths: &[String]) -> bool {
    config_paths.iter().any(|c| {
        let managed = expand_tilde(c);
        managed == path || path.starts_with(&managed)
    })
}

/// Resolve `--source` directives across rendered build steps.
///
/// Contract (recipes/shadow/fixtures/phase1-source-location.md):
/// - path exists → used as-is; never fetched, never overwritten. This is
///   also the enforcement point for the `install.config_paths` invariant
///   (operator-managed files are seeded only when absent).
/// - path missing + http(s) fallback → fallback fetched to path (full
///   parent chain created), then the step runs.
/// - path missing + no fallback → nothing to pull; the step itself owns the
///   seed content.
/// - fetch failure aborts naming the file AND the source.
///
/// The `--source <path> [<fallback>]` fragment is stripped from the returned
/// command: resolution happens in this runner, not in a shipped helper binary.
pub async fn resolve_step_sources(
    steps: &[String],
    config_paths: &[String],
    http: &reqwest::Client,
) -> Result<Vec<String>> {
    let mut resolved = Vec::with_capacity(steps.len());
    for step in steps {
        resolved.push(resolve_one(step, config_paths, http).await?);
    }
    Ok(resolved)
}

async fn resolve_one(
    step: &str,
    config_paths: &[String],
    http: &reqwest::Client,
) -> Result<String> {
    let toks = tokens(step);
    let Some(src_idx) = toks.iter().position(|t| t.text == "--source") else {
        return Ok(step.to_string());
    };
    let path_tok = toks.get(src_idx + 1).ok_or_else(|| {
        anyhow!("--source directive in step has no path: {step}")
    })?;
    let path = expand_tilde(path_tok.text);

    if path.exists() {
        // Exists → use as-is. For config_paths entries this is the standing
        // "NEVER touched" invariant: an operator-tuned file is never
        // fetched over, on install or update alike.
        return Ok(strip_source_directive(step, &toks, src_idx));
    }

    if let Some(fallback_tok) = toks.get(src_idx + 2) {
        let fallback = fallback_tok.text;
        if !(fallback.starts_with("http://") || fallback.starts_with("https://")) {
            bail!(
                "unsupported fallback form for --source {}: '{fallback}' (only http(s) URLs are supported)",
                path.display()
            );
        }
        let seeded_config = is_managed_config_path(&path, config_paths);
        fetch_to(http, fallback, &path)
            .await
            .with_context(|| {
                format!(
                    "could not provision --source file '{}' from '{}'",
                    path.display(),
                    fallback
                )
            })?;
        if seeded_config {
            tracing::debug!(
                path = %path.display(),
                "seeded operator-managed config_paths entry (first install only)"
            );
        }
    }
    // No fallback: nothing to pull; the step itself is responsible for the
    // file's content (the operator-managed seed case).

    Ok(strip_source_directive(step, &toks, src_idx))
}

/// Remove the `--source <path> [<fallback>]` token span from the step,
/// returning the executable remainder (empty when the step was only a
/// provisioning directive).
fn strip_source_directive(step: &str, toks: &[Token<'_>], src_idx: usize) -> String {
    let src_tok = &toks[src_idx];
    let path_tok = &toks[src_idx + 1];
    let end = toks
        .get(src_idx + 2)
        .map_or(path_tok.end, |t| t.end);
    let mut out = String::with_capacity(step.len());
    out.push_str(&step[..src_tok.start]);
    out.push_str(step[end..].trim_start());
    out.trim().to_string()
}

/// Fetch `url` and write it to `dest`, creating the full parent chain first.
async fn fetch_to(http: &reqwest::Client, url: &str, dest: &Path) -> Result<()> {
    let resp = http
        .get(url)
        .send()
        .await
        .with_context(|| format!("fetching {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        bail!("fetch of {url} returned HTTP {status}");
    }
    let bytes = resp
        .bytes()
        .await
        .with_context(|| format!("reading body of {url}"))?;
    if let Some(parent) = dest.parent() {
        // Full parent chain: seeded paths commonly nest several levels.
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(dest, &bytes).with_context(|| format!("writing {}", dest.display()))?;
    Ok(())
}

/// Run a single rendered/resolved build step under `bash -c`, honoring the
/// recipe's network policy. Deny uses the same hardening machinery as
/// build/script execution in `crate::shadow` (`unshare -n` where available,
/// proxy env-poisoning as fallback).
pub async fn run_build_step(step: &str, policy: &NetworkPolicy) -> Result<()> {
    let mut command = tokio::process::Command::new("bash");
    if *policy == NetworkPolicy::Denied {
        shadow::build_network_denial(&mut command);
    }
    let output = command
        .arg("-c")
        .arg(step)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .await
        .with_context(|| format!("spawning bash for step: {step}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "step exited {}: {stderr}",
            output.status.code().unwrap_or(-1)
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// Minimal HTTP/1.1 file server on 127.0.0.1:0 serving `body` for every
    /// GET (Connection: close). Returns (port, join-handle).
    fn spawn_http_server(body: Vec<u8>) -> (u16, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut stream = stream;
                let mut buf = [0u8; 4096];
                // Drain the request head (we never serve request bodies).
                let _ = stream.read(&mut buf);
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(header.as_bytes());
                let _ = stream.write_all(&body);
            }
        });
        (port, handle)
    }

    fn http_client() -> reqwest::Client {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .unwrap()
    }

    // --- legacy brace translation (D1) ---

    #[test]
    fn translate_legacy_braces_translates_all_five_names() {
        let step = "{target_dir} {version} {arch} {os} {libc}";
        assert_eq!(
            translate_legacy_braces(step),
            "{{target_dir}} {{version}} {{arch}} {{os}} {{libc}}"
        );
    }

    #[test]
    fn translate_legacy_braces_leaves_double_brace_untouched() {
        assert_eq!(
            translate_legacy_braces("{{target_dir}}/{{version}}"),
            "{{target_dir}}/{{version}}"
        );
    }

    #[test]
    fn translate_legacy_braces_leaves_unknown_names() {
        // A literal `{version}`-lookalike outside the five names (asset
        // template style) must not be eaten by the wrong engine.
        assert_eq!(
            translate_legacy_braces("curl {src_dir}/x -o {dst}"),
            "curl {src_dir}/x -o {dst}"
        );
    }

    #[test]
    fn translate_legacy_braces_mixed_step() {
        let step = "mkdir -p {{target_dir}} && cp app {target_dir}/app-{version}";
        assert_eq!(
            translate_legacy_braces(step),
            "mkdir -p {{target_dir}} && cp app {{target_dir}}/app-{{version}}"
        );
    }

    #[test]
    fn translate_legacy_braces_token_at_end_of_string() {
        assert_eq!(translate_legacy_braces("cd {target_dir}"), "cd {{target_dir}}");
        assert_eq!(translate_legacy_braces("cd {target_dir"), "cd {target_dir");
    }

    // --- tilde expansion (D2) ---

    #[test]
    fn expand_tilde_leaves_absolute_and_relative_paths() {
        assert_eq!(expand_tilde("/opt/app/.env"), PathBuf::from("/opt/app/.env"));
        assert_eq!(expand_tilde("rel/.env"), PathBuf::from("rel/.env"));
        // A user-anchored tilde is not the invoking user's home — literal.
        assert_eq!(expand_tilde("~other/.env"), PathBuf::from("~other/.env"));
    }

    // --- --source resolution ---

    #[tokio::test]
    async fn resolve_source_exists_uses_file_as_is() {
        let tmp = tempfile::tempdir().unwrap();
        let env_path = tmp.path().join(".env");
        std::fs::write(&env_path, b"OPERATOR-TUNED=1").unwrap();

        // Fallback server serves DIFFERENT content; it must never be fetched.
        let (port, _srv) = spawn_http_server(b"UPSTREAM=0".to_vec());
        let step = format!(
            "install-env --source {} http://127.0.0.1:{port}/.env.example",
            env_path.display()
        );
        let out = resolve_step_sources(&[step], &[], &http_client())
            .await
            .unwrap();
        assert_eq!(std::fs::read(&env_path).unwrap(), b"OPERATOR-TUNED=1");
        // The directive is stripped; the remainder is what executes.
        assert_eq!(out[0], "install-env");
    }

    #[tokio::test]
    async fn resolve_source_missing_fetches_fallback_then_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let env_path = tmp.path().join("conf").join(".env");
        let (port, _srv) = spawn_http_server(b"SEED=ok".to_vec());
        let step = format!(
            "install-env --source {} http://127.0.0.1:{port}/.env.example",
            env_path.display()
        );
        let out = resolve_step_sources(&[step], &[], &http_client())
            .await
            .unwrap();
        // Full parent chain created, content written, directive stripped.
        assert_eq!(std::fs::read(&env_path).unwrap(), b"SEED=ok");
        assert_eq!(out[0], "install-env");
    }

    #[tokio::test]
    async fn resolve_source_fetch_failure_names_file_and_source() {
        let tmp = tempfile::tempdir().unwrap();
        let env_path = tmp.path().join(".env");
        // Port 9 (discard) is never a HTTP server in the test env.
        let step = format!(
            "install-env --source {} http://127.0.0.1:9/.env.example",
            env_path.display()
        );
        let err = resolve_step_sources(&[step], &[], &http_client())
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains(&env_path.display().to_string()) && err.contains("http://127.0.0.1:9/.env.example"),
            "error must name file AND source, got: {err}"
        );
        assert!(!env_path.exists(), "failed fetch must not create the file");
    }

    #[tokio::test]
    async fn resolve_source_unsupported_fallback_form_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let env_path = tmp.path().join(".env");
        let step = format!(
            "install-env --source {} repo-relative/.env.example",
            env_path.display()
        );
        let err = resolve_step_sources(&[step], &[], &http_client())
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("unsupported fallback form") && err.contains("repo-relative/.env.example"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn resolve_source_config_path_present_is_never_overwritten() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("server.toml");
        std::fs::write(&cfg, b"operator = true").unwrap();
        let (port, _srv) = spawn_http_server(b"upstream = false".to_vec());
        let step = format!(
            "install --source {} http://127.0.0.1:{port}/server.toml",
            cfg.display()
        );
        let config_paths = vec![cfg.display().to_string()];
        resolve_step_sources(&[step], &config_paths, &http_client())
            .await
            .unwrap();
        assert_eq!(std::fs::read(&cfg).unwrap(), b"operator = true");
    }

    #[tokio::test]
    async fn resolve_source_config_path_absent_is_seeded_once() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("server.toml");
        let (port, _srv) = spawn_http_server(b"seeded = true".to_vec());
        let step = format!(
            "install --source {} http://127.0.0.1:{port}/server.toml",
            cfg.display()
        );
        let config_paths = vec![cfg.display().to_string()];
        resolve_step_sources(&[step], &config_paths, &http_client())
            .await
            .unwrap();
        assert_eq!(std::fs::read(&cfg).unwrap(), b"seeded = true");
    }

    #[tokio::test]
    async fn resolve_source_double_run_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let env_path = tmp.path().join(".env");
        let (port, _srv) = spawn_http_server(b"SEED=ok".to_vec());
        let step = format!(
            "install-env --source {} http://127.0.0.1:{port}/.env.example",
            env_path.display()
        );
        // Operator "edits" between the two runs; the second run must be a
        // no-op (file now exists → used as-is).
        let first = resolve_step_sources(std::slice::from_ref(&step), &[], &http_client())
            .await
            .unwrap();
        std::fs::write(&env_path, b"OPERATOR-EDIT").unwrap();
        let second = resolve_step_sources(&[step], &[], &http_client())
            .await
            .unwrap();
        assert_eq!(first, second);
        assert_eq!(std::fs::read(&env_path).unwrap(), b"OPERATOR-EDIT");
    }

    #[tokio::test]
    async fn resolve_source_rendered_target_dir_path_composition() {
        // Render-order contract: resolution sees FINAL literal paths, so a
        // {{target_dir}}/.env guard composed during rendering resolves.
        let tmp = tempfile::tempdir().unwrap();
        let env_path = tmp.path().join(".env");
        let (port, _srv) = spawn_http_server(b"COMPOSED=1".to_vec());
        let rendered = format!(
            "install-env --source {}/.env http://127.0.0.1:{port}/.env.example",
            tmp.path().display()
        );
        resolve_step_sources(&[rendered], &[], &http_client())
            .await
            .unwrap();
        assert!(env_path.exists());
    }

    #[tokio::test]
    async fn resolve_step_without_source_passes_through() {
        let out = resolve_step_sources(&["make -j2 && make install".to_string()], &[], &http_client())
            .await
            .unwrap();
        assert_eq!(out[0], "make -j2 && make install");
    }

    #[tokio::test]
    async fn resolve_source_directive_only_step_becomes_empty_command() {
        let tmp = tempfile::tempdir().unwrap();
        let (port, _srv) = spawn_http_server(b"x=1".to_vec());
        let step = format!(
            "--source {} http://127.0.0.1:{port}/.env",
            tmp.path().join(".env").display()
        );
        let out = resolve_step_sources(&[step], &[], &http_client())
            .await
            .unwrap();
        assert_eq!(out[0], "");
        assert!(tmp.path().join(".env").exists());
    }

    #[tokio::test]
    async fn resolve_source_without_path_errors() {
        let err = resolve_step_sources(&["install --source".to_string()], &[], &http_client())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("--source"), "got: {err}");
    }

    // --- step execution ---

    #[tokio::test]
    async fn run_build_step_runs_bash() {
        let tmp = tempfile::tempdir().unwrap();
        let marker = tmp.path().join("marker");
        let step = format!("echo ran > {}", marker.display());
        run_build_step(&step, &shadow::resolve_network_policy(false))
            .await
            .unwrap();
        assert!(marker.exists());
    }

    #[tokio::test]
    async fn run_build_step_propagates_failure_with_exit_code() {
        let err = run_build_step("exit 7", &shadow::resolve_network_policy(false))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("7"), "got: {err}");
    }

    // --- i2pd fixture migration gate ---

    #[test]
    fn i2pd_fixture_still_parses_and_uses_double_brace_steps() {
        let toml_str = std::fs::read_to_string("../../recipes/shadow/i2pd.toml")
            .expect("i2pd fixture readable from crate dir");
        let recipe: monopsony_common::shadow_types::ShadowRecipe =
            toml::from_str(&toml_str).expect("i2pd fixture parses as ShadowRecipe");
        assert_eq!(recipe.install.method, monopsony_common::shadow_types::InstallMethod::Build);
        let build = recipe.build.as_ref().expect("i2pd has [build]");
        assert!(!build.steps.is_empty());
        for step in &build.steps {
            // A migrated step must contain no lone `{` directive: every
            // engine directive is double-braced after migration.
            let stripped = step
                .replace("{{target_dir}}", "")
                .replace("{{version}}", "");
            assert!(
                !stripped.contains('{'),
                "legacy single-brace form must be migrated: {step}"
            );
        }
        assert!(
            build.steps.iter().any(|s| s.contains("{{target_dir}}")),
            "migrated steps use the Phase 1 double-brace engine"
        );
    }
}
