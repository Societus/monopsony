//! Sprint 32 Phase 3 — update diffing (pure recipe-struct diff).
//!
//! Compares two [`ShadowRecipe`] values field-class by field-class and emits
//! one [`ShadowDiffWarning`] per changed diffable class: `install.service`,
//! hooks (`pre_update` / `post_update` / `on_failure`), `install.config_paths`,
//! `install.health_check`, `install.exec_start`, `build.requires`,
//! `build.steps`, `script.update`, and the `[version]` detect pair.
//!
//! Contract (grounding: `recipes/shadow/fixtures/phase3-update-diffing/README.md`):
//! - identical recipes yield an EMPTY vec — a clean patch release must stay
//!   silent (the negative gate: warning noise trains operators to ignore real
//!   warnings);
//! - every diffable field change yields a warning naming the old and new
//!   values;
//! - ordering of the returned warnings is NOT load-bearing — callers must not
//!   index into the vec positionally.
//!
//! Package-level fields (`description`, `homepage`, source, asset selector,
//! notifications) are deliberately NOT diffed: they are not operating
//! requirements. The rendered-service-script diff (ExecStart flags / env) is
//! a Phase 2 boundary concern and is deferred — templates are baked into the
//! binary, not per-recipe. This module is pure and lives in `common` so both
//! client and server can consume it later.
//!
//! Equality uses direct field comparison where the types already derive
//! `PartialEq`, and serialization comparison (`serde_json`) where they do not
//! (`HealthCheck`). Order-insensitive collections (`config_paths`,
//! `build.requires`) are compared as sorted lists; `build.steps` order IS
//! execution order and is compared as-is.
//!
//! `recipe_edit_required` is `true` for every class except
//! [`ShadowDiffClass::VersionDetect`]: all others change what the host must
//! run or monitor (unit names, hooks, provisioning, health target) and so
//! require the registered recipe / operator runbook to be edited; a detect
//! pair change only alters bookkeeping.

use crate::shadow_types::{HealthCheck, ShadowRecipe};

/// Classification of a recipe requirement change (one variant per diffable
/// field class).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShadowDiffClass {
    /// `install.service` unit name changed.
    ServiceUnit,
    /// A `pre_update` / `post_update` / `on_failure` hook command changed.
    Hook,
    /// `install.config_paths` changed.
    ConfigPaths,
    /// `install.health_check` changed (type, target, timeout, or stability).
    HealthCheck,
    /// `install.exec_start` changed (Phase 2 first-class service command).
    ExecStart,
    /// `build.requires` changed.
    BuildRequires,
    /// `build.steps` changed (count or content; order is execution order).
    BuildSteps,
    /// `script.update` changed.
    Script,
    /// The `[version]` detect pair (`detect_command` + `detect_pattern`) changed.
    VersionDetect,
}

impl ShadowDiffClass {
    /// Stable snake_case label used in `diff warning: <class>: <message>` log lines.
    pub fn label(&self) -> &'static str {
        match self {
            ShadowDiffClass::ServiceUnit => "service_unit",
            ShadowDiffClass::Hook => "hook",
            ShadowDiffClass::ConfigPaths => "config_paths",
            ShadowDiffClass::HealthCheck => "health_check",
            ShadowDiffClass::ExecStart => "exec_start",
            ShadowDiffClass::BuildRequires => "build_requires",
            ShadowDiffClass::BuildSteps => "build_steps",
            ShadowDiffClass::Script => "script",
            ShadowDiffClass::VersionDetect => "version_detect",
        }
    }
}

/// One requirement change between two recipe versions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShadowDiffWarning {
    /// Which diffable field class changed.
    pub class: ShadowDiffClass,
    /// Human-readable description naming the old and new values.
    pub message: String,
    /// True when the change requires a recipe/operator edit (every class
    /// except `VersionDetect` — see the module doc).
    pub recipe_edit_required: bool,
}

/// Diff two recipes and return the requirement-change warnings. Empty vec for
/// identical recipes. Warning order is not load-bearing.
pub fn shadow_diff(old: &ShadowRecipe, new: &ShadowRecipe) -> Vec<ShadowDiffWarning> {
    let mut warnings = Vec::new();
    let old_install = &old.install;
    let new_install = &new.install;

    // Service unit rename: the unit shipped with the OLD recipe stops
    // matching, and old hooks referencing it will no-op until edited.
    if old_install.service != new_install.service {
        let mut message = format!(
            "service unit: '{}' → '{}'",
            display_opt(old_install.service.as_deref()),
            display_opt(new_install.service.as_deref())
        );
        if let Some(old_name) = &old_install.service {
            if hooks_reference_unit(old_install, old_name) {
                message.push_str(
                    "; hooks referencing the old unit (shipped with the old recipe) will no-op",
                );
            }
        }
        warnings.push(ShadowDiffWarning {
            class: ShadowDiffClass::ServiceUnit,
            message,
            recipe_edit_required: true,
        });
    }

    // Hooks run as-is: a changed command is a changed host action.
    for (name, old_hook, new_hook) in [
        ("pre_update", &old_install.pre_update, &new_install.pre_update),
        ("post_update", &old_install.post_update, &new_install.post_update),
        ("on_failure", &old_install.on_failure, &new_install.on_failure),
    ] {
        if old_hook != new_hook {
            warnings.push(ShadowDiffWarning {
                class: ShadowDiffClass::Hook,
                message: format!(
                    "{name} hook: '{}' → '{}'",
                    display_opt(old_hook.as_deref()),
                    display_opt(new_hook.as_deref())
                ),
                recipe_edit_required: true,
            });
        }
    }

    // Config paths are never touched by the executor, but a change in the
    // managed set is operator-visible (files the recipe now claims).
    if sorted_paths(&old_install.config_paths) != sorted_paths(&new_install.config_paths) {
        warnings.push(ShadowDiffWarning {
            class: ShadowDiffClass::ConfigPaths,
            message: format!(
                "config_paths: {:?} → {:?}",
                old_install.config_paths, new_install.config_paths
            ),
            recipe_edit_required: true,
        });
    }

    // Health check moved: a v-old check proves nothing against a v-new
    // service; external monitors keyed to the old target must be updated.
    if serde_json::to_string(&old_install.health_check).ok()
        != serde_json::to_string(&new_install.health_check).ok()
    {
        let old_desc = old_install
            .health_check
            .as_ref()
            .map(describe_health_check)
            .unwrap_or_else(|| "<absent>".to_string());
        let new_desc = new_install
            .health_check
            .as_ref()
            .map(describe_health_check)
            .unwrap_or_else(|| "<absent>".to_string());
        warnings.push(ShadowDiffWarning {
            class: ShadowDiffClass::HealthCheck,
            message: format!("health check: {old_desc} → {new_desc}"),
            recipe_edit_required: true,
        });
    }

    // exec_start is the rendered service command line (Phase 2): a change
    // alters what the unit actually starts.
    if old_install.exec_start != new_install.exec_start {
        warnings.push(ShadowDiffWarning {
            class: ShadowDiffClass::ExecStart,
            message: format!(
                "exec_start: '{}' → '{}'",
                display_opt(old_install.exec_start.as_deref()),
                display_opt(new_install.exec_start.as_deref())
            ),
            recipe_edit_required: true,
        });
    }

    // Build requirements are which-checked before any host change; a new
    // requirement aborts builds on hosts lacking it.
    if sorted_strings(&old.build.as_ref().map(|b| b.requires.clone()))
        != sorted_strings(&new.build.as_ref().map(|b| b.requires.clone()))
    {
        let old_reqs = old
            .build
            .as_ref()
            .map(|b| b.requires.clone())
            .unwrap_or_default();
        let new_reqs = new
            .build
            .as_ref()
            .map(|b| b.requires.clone())
            .unwrap_or_default();
        warnings.push(ShadowDiffWarning {
            class: ShadowDiffClass::BuildRequires,
            message: format!("build requires: {:?} → {:?}", old_reqs, new_reqs),
            recipe_edit_required: true,
        });
    }

    // Build steps run in order; a count or content change means new
    // provisioning requirements (env files, sidecars, …).
    let old_steps = old.build.as_ref().map(|b| b.steps.clone());
    let new_steps = new.build.as_ref().map(|b| b.steps.clone());
    if old_steps != new_steps {
        let old_n = old_steps.as_ref().map(|s| s.len()).unwrap_or(0);
        let new_n = new_steps.as_ref().map(|s| s.len()).unwrap_or(0);
        warnings.push(ShadowDiffWarning {
            class: ShadowDiffClass::BuildSteps,
            message: format!("build steps: {old_n} step(s) → {new_n} step(s)"),
            recipe_edit_required: true,
        });
    }

    // Script updates are arbitrary shell: any change is a changed host action.
    if old.script.as_ref().map(|s| s.update.as_str()) != new.script.as_ref().map(|s| s.update.as_str())
    {
        warnings.push(ShadowDiffWarning {
            class: ShadowDiffClass::Script,
            message: format!(
                "script.update: '{}' → '{}'",
                display_opt(old.script.as_ref().map(|s| s.update.as_str())),
                display_opt(new.script.as_ref().map(|s| s.update.as_str()))
            ),
            recipe_edit_required: true,
        });
    }

    // Detect pair changes only affect version bookkeeping, not the running
    // requirement set — surfaced as info, not a recipe edit.
    if old.version.detect_command != new.version.detect_command
        || old.version.detect_pattern != new.version.detect_pattern
    {
        warnings.push(ShadowDiffWarning {
            class: ShadowDiffClass::VersionDetect,
            message: format!(
                "version detect: '{}/{}' → '{}/{}'",
                old.version.detect_command,
                old.version.detect_pattern,
                new.version.detect_command,
                new.version.detect_pattern
            ),
            recipe_edit_required: false,
        });
    }

    warnings
}

/// `Option<&str>` rendered for warning messages (absent must be visible).
fn display_opt(value: Option<&str>) -> &str {
    match value {
        Some(v) => v,
        None => "<absent>",
    }
}

/// Sort a `config_paths` option for order-insensitive comparison.
fn sorted_paths(paths: &Option<Vec<String>>) -> Option<Vec<String>> {
    match paths {
        Some(v) => {
            let mut sorted = v.clone();
            sorted.sort();
            Some(sorted)
        }
        None => None,
    }
}

/// Sort a `build.requires` option for order-insensitive comparison.
fn sorted_strings(reqs: &Option<Vec<String>>) -> Option<Vec<String>> {
    match reqs {
        Some(v) => {
            let mut sorted = v.clone();
            sorted.sort();
            Some(sorted)
        }
        None => None,
    }
}

/// True when any of the install hooks' commands mention `unit`.
fn hooks_reference_unit(install: &crate::shadow_types::RecipeInstall, unit: &str) -> bool {
    let mut referenced = false;
    for cmd in [&install.pre_update, &install.post_update, &install.on_failure]
        .into_iter()
        .flatten()
    {
        if cmd.contains(unit) {
            referenced = true;
        }
    }
    referenced
}

/// Compact human descriptor of a health check target for warning messages.
fn describe_health_check(hc: &HealthCheck) -> String {
    let target = if let Some(port) = hc.port {
        format!("port:{port}")
    } else if let Some(url) = &hc.url {
        format!("url:{url}")
    } else if let Some(process) = &hc.process {
        format!("process:{process}")
    } else {
        "unconfigured".to_string()
    };
    format!("{target} (timeout {}s)", hc.timeout_secs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shadow_types::ShadowRecipe;

    /// Canonical v1-shaped recipe TOML (mirrors the phase-3 fixture
    /// semantics: extract + service + hooks + port health check + build).
    fn recipe_toml() -> String {
        r#"[package]
name = "example-app"
description = "baseline"
source_type = "direct_url"
source_url = "https://github.com/example/app"

[install]
method = "extract"
target_dir = "/opt/example-app"
service = "app.service"
config_paths = ["/etc/example-app"]
backup_old = true
max_backups = 3

pre_update = "systemctl stop app.service || true"
post_update = "systemctl start app.service || true"

[install.health_check]
type = "port"
port = 8080
timeout_secs = 60

[build]
requires = ["gcc", "make"]
allow_network = true
steps = [
    "git clone --branch v{{version}} https://github.com/example/app /tmp/example-app",
    "make -C /tmp/example-app BUILD_TYPE=Release",
    "cp /tmp/example-app/app {{target_dir}}/app",
]

[direct_url]
version_discovery = "scrape"
asset_url_template = "https://downloads.example.com/app/app-{{version}}-linux.tar.gz"

[version]
detect_command = "/opt/example-app/app --version"
detect_pattern = "(\\d+\\.\\d+\\.\\d+)"
"#
        .to_string()
    }

    fn parse(toml_text: &str) -> ShadowRecipe {
        toml::from_str(toml_text).expect("test recipe must parse")
    }

    fn warnings_of_class(
        warnings: &[ShadowDiffWarning],
        class: ShadowDiffClass,
    ) -> Vec<&ShadowDiffWarning> {
        warnings.iter().filter(|w| w.class == class).collect()
    }

    #[test]
    fn identical_pair_yields_empty() {
        let v1 = parse(&recipe_toml());
        let v1_again = parse(&recipe_toml());
        assert!(
            shadow_diff(&v1, &v1_again).is_empty(),
            "identical recipes must yield ZERO warnings (the negative gate)"
        );
    }

    #[test]
    fn negative_patch_release_semantics_yield_empty() {
        // v1.0.0 → v1.0.1: every diffable field identical; only the
        // package-level description differs (not a diffable field).
        let v1 = parse(&recipe_toml());
        let v101 = parse(&recipe_toml().replace(
            "description = \"baseline\"",
            "description = \"patch release, no requirement changes\"",
        ));
        assert!(
            shadow_diff(&v1, &v101).is_empty(),
            "a clean patch release must stay silent"
        );
    }

    #[test]
    fn service_unit_rename_fires_with_old_and_new_values() {
        let v1 = parse(&recipe_toml());
        let v2 = parse(&recipe_toml().replace("app.service", "appd.service"));
        let warnings = shadow_diff(&v1, &v2);
        let hits = warnings_of_class(&warnings, ShadowDiffClass::ServiceUnit);
        assert_eq!(hits.len(), 1, "warnings: {warnings:?}");
        let w = hits[0];
        assert!(w.message.contains("app.service"), "old value: {}", w.message);
        assert!(w.message.contains("appd.service"), "new value: {}", w.message);
        assert!(
            w.recipe_edit_required,
            "unit rename requires a recipe edit"
        );
        // The old hooks reference the old unit — the rename warning says so.
        assert!(
            w.message.contains("no-op"),
            "rename must warn that old hooks referencing the unit no-op: {}",
            w.message
        );
    }

    #[test]
    fn unit_rename_also_fires_hook_warnings_for_old_name_references() {
        // Renaming the unit via replace() rewrites the hook strings too
        // (fixture semantics: hooks reference the unit name) — the Hook
        // class must fire alongside ServiceUnit.
        let v1 = parse(&recipe_toml());
        let v2 = parse(&recipe_toml().replace("app.service", "appd.service"));
        let warnings = shadow_diff(&v1, &v2);
        let hook_hits = warnings_of_class(&warnings, ShadowDiffClass::Hook);
        assert_eq!(
            hook_hits.len(),
            2,
            "pre_update AND post_update changed with the rename: {warnings:?}"
        );
        for w in &hook_hits {
            assert!(w.message.contains("app.service"), "old: {}", w.message);
            assert!(w.message.contains("appd.service"), "new: {}", w.message);
        }
    }

    #[test]
    fn hook_only_change_fires() {
        let v1 = parse(&recipe_toml());
        let v2 = parse(&recipe_toml().replace(
            "post_update = \"systemctl start app.service || true\"",
            "post_update = \"set -a && . /etc/example-app/env && systemctl start app.service || true\"",
        ));
        let warnings = shadow_diff(&v1, &v2);
        let hook_hits = warnings_of_class(&warnings, ShadowDiffClass::Hook);
        assert_eq!(hook_hits.len(), 1, "warnings: {warnings:?}");
        assert!(hook_hits[0].message.contains("post_update"));
        assert!(
            hook_hits[0].message.contains("systemctl start app.service")
                && hook_hits[0].message.contains("/etc/example-app/env"),
            "message must name old and new commands: {}",
            hook_hits[0].message
        );
        assert_eq!(
            warnings.len(),
            1,
            "only the hook changed — exactly one warning: {warnings:?}"
        );
    }

    #[test]
    fn health_check_port_move_fires_with_old_and_new_ports() {
        let v1 = parse(&recipe_toml());
        let v2 = parse(&recipe_toml().replace("port = 8080", "port = 8443"));
        let warnings = shadow_diff(&v1, &v2);
        let hits = warnings_of_class(&warnings, ShadowDiffClass::HealthCheck);
        assert_eq!(hits.len(), 1, "warnings: {warnings:?}");
        assert!(hits[0].message.contains("8080"), "old: {}", hits[0].message);
        assert!(hits[0].message.contains("8443"), "new: {}", hits[0].message);
    }

    #[test]
    fn config_paths_change_fires() {
        let v1 = parse(&recipe_toml());
        let v2 = parse(&recipe_toml().replace(
            "config_paths = [\"/etc/example-app\"]",
            "config_paths = [\"/etc/example-app\", \"/etc/example-app/extra\"]",
        ));
        let warnings = shadow_diff(&v1, &v2);
        let hits = warnings_of_class(&warnings, ShadowDiffClass::ConfigPaths);
        assert_eq!(hits.len(), 1, "warnings: {warnings:?}");
        assert!(hits[0].message.contains("/etc/example-app/extra"));
    }

    #[test]
    fn config_paths_reorder_only_yields_empty() {
        // config_paths is a set: order is not a requirement change.
        let v1 = parse(&recipe_toml().replace(
            "config_paths = [\"/etc/example-app\"]",
            "config_paths = [\"/etc/a\", \"/etc/b\"]",
        ));
        let v2 = parse(&recipe_toml().replace(
            "config_paths = [\"/etc/example-app\"]",
            "config_paths = [\"/etc/b\", \"/etc/a\"]",
        ));
        assert!(shadow_diff(&v1, &v2).is_empty());
    }

    #[test]
    fn exec_start_change_fires() {
        let v1 = parse(&recipe_toml());
        let v2 = parse(&recipe_toml().replace(
            "service = \"app.service\"",
            "service = \"app.service\"\nexec_start = \"/opt/example-app/app --serve --tls\"",
        ));
        let warnings = shadow_diff(&v1, &v2);
        let hits = warnings_of_class(&warnings, ShadowDiffClass::ExecStart);
        assert_eq!(hits.len(), 1, "warnings: {warnings:?}");
        assert!(hits[0].message.contains("<absent>"), "old: {}", hits[0].message);
        assert!(hits[0].message.contains("--tls"), "new: {}", hits[0].message);
    }

    #[test]
    fn build_requires_change_fires_naming_new_requirement() {
        let v1 = parse(&recipe_toml());
        let v2 = parse(&recipe_toml().replace(
            "requires = [\"gcc\", \"make\"]",
            "requires = [\"gcc\", \"make\", \"openssl\"]",
        ));
        let warnings = shadow_diff(&v1, &v2);
        let hits = warnings_of_class(&warnings, ShadowDiffClass::BuildRequires);
        assert_eq!(hits.len(), 1, "warnings: {warnings:?}");
        assert!(hits[0].message.contains("openssl"), "new: {}", hits[0].message);
        assert!(hits[0].message.contains("make"), "old: {}", hits[0].message);
    }

    #[test]
    fn build_requires_reorder_only_yields_empty() {
        let v1 = parse(&recipe_toml());
        let v2 = parse(&recipe_toml().replace(
            "requires = [\"gcc\", \"make\"]",
            "requires = [\"make\", \"gcc\"]",
        ));
        assert!(shadow_diff(&v1, &v2).is_empty());
    }

    #[test]
    fn build_steps_count_change_fires() {
        let v1 = parse(&recipe_toml());
        let v2 = parse(&recipe_toml().replace(
            "    \"cp /tmp/example-app/app {{target_dir}}/app\",\n]",
            "    \"cp /tmp/example-app/app {{target_dir}}/app\",\n    \"install -D -m600 /tmp/example-app/deploy/env.example /etc/example-app/env\",\n    \"install -D -m644 /tmp/example-app/deploy/app-metrics.container /etc/containers/systemd/app-metrics.container\",\n]",
        ));
        let warnings = shadow_diff(&v1, &v2);
        let hits = warnings_of_class(&warnings, ShadowDiffClass::BuildSteps);
        assert_eq!(hits.len(), 1, "warnings: {warnings:?}");
        assert!(hits[0].message.contains("3 step(s)"), "old: {}", hits[0].message);
        assert!(hits[0].message.contains("5 step(s)"), "new: {}", hits[0].message);
    }

    #[test]
    fn script_change_fires() {
        let v1 = parse(&recipe_toml());
        let v2 = parse(&recipe_toml().replace(
            "[direct_url]",
            "[script]\nupdate = \"curl -fsSL https://example.com/install.sh | bash\"\n\n[direct_url]",
        ));
        let warnings = shadow_diff(&v1, &v2);
        let hits = warnings_of_class(&warnings, ShadowDiffClass::Script);
        assert_eq!(hits.len(), 1, "warnings: {warnings:?}");
        assert!(hits[0].message.contains("<absent>"), "old: {}", hits[0].message);
        assert!(hits[0].message.contains("install.sh"), "new: {}", hits[0].message);
    }

    #[test]
    fn version_detect_change_fires_non_edit() {
        let v1 = parse(&recipe_toml());
        let v2 = parse(&recipe_toml().replace(
            "detect_command = \"/opt/example-app/app --version\"",
            "detect_command = \"/opt/example-app/bin/app --version\"",
        ));
        let warnings = shadow_diff(&v1, &v2);
        let hits = warnings_of_class(&warnings, ShadowDiffClass::VersionDetect);
        assert_eq!(hits.len(), 1, "warnings: {warnings:?}");
        assert!(
            !hits[0].recipe_edit_required,
            "detect-pair change is bookkeeping, not a recipe edit"
        );
        assert!(hits[0].message.contains("--version"));
    }

    #[test]
    fn churn_pair_reexpressed_yields_exactly_the_expected_classes() {
        // Re-expresses the grounding fixture truths (v1.0.0 → v2.0.0) through
        // the diff module: service rename (+ both hooks), health port move,
        // +openssl, 3→5 steps. Fixtures stay untouched in
        // grounding_fixtures.rs; this is the same machine-checked semantics.
        let v1 = parse(&recipe_toml());
        let v2_toml = recipe_toml()
            .replace("app.service", "appd.service")
            .replace("port = 8080", "port = 8443")
            .replace(
                "requires = [\"gcc\", \"make\"]",
                "requires = [\"gcc\", \"make\", \"openssl\"]",
            )
            .replace(
                "    \"cp /tmp/example-app/app {{target_dir}}/app\",\n]",
                "    \"cp /tmp/example-app/app {{target_dir}}/app\",\n    \"install -D -m600 /tmp/example-app/deploy/env.example /etc/example-app/env\",\n    \"install -D -m644 /tmp/example-app/deploy/app-metrics.container /etc/containers/systemd/app-metrics.container\",\n]",
            );
        let v2 = parse(&v2_toml);
        let warnings = shadow_diff(&v1, &v2);

        assert_eq!(warnings.len(), 6, "warnings: {warnings:?}");
        assert_eq!(warnings_of_class(&warnings, ShadowDiffClass::ServiceUnit).len(), 1);
        assert_eq!(warnings_of_class(&warnings, ShadowDiffClass::Hook).len(), 2);
        assert_eq!(warnings_of_class(&warnings, ShadowDiffClass::HealthCheck).len(), 1);
        assert_eq!(warnings_of_class(&warnings, ShadowDiffClass::BuildRequires).len(), 1);
        assert_eq!(warnings_of_class(&warnings, ShadowDiffClass::BuildSteps).len(), 1);
        // Not churned in the fixture pair: these classes must stay silent.
        assert!(warnings_of_class(&warnings, ShadowDiffClass::ConfigPaths).is_empty());
        assert!(warnings_of_class(&warnings, ShadowDiffClass::ExecStart).is_empty());
        assert!(warnings_of_class(&warnings, ShadowDiffClass::Script).is_empty());
        assert!(warnings_of_class(&warnings, ShadowDiffClass::VersionDetect).is_empty());
    }

    #[test]
    fn class_labels_are_stable_snake_case() {
        assert_eq!(ShadowDiffClass::ServiceUnit.label(), "service_unit");
        assert_eq!(ShadowDiffClass::Hook.label(), "hook");
        assert_eq!(ShadowDiffClass::ConfigPaths.label(), "config_paths");
        assert_eq!(ShadowDiffClass::HealthCheck.label(), "health_check");
        assert_eq!(ShadowDiffClass::ExecStart.label(), "exec_start");
        assert_eq!(ShadowDiffClass::BuildRequires.label(), "build_requires");
        assert_eq!(ShadowDiffClass::BuildSteps.label(), "build_steps");
        assert_eq!(ShadowDiffClass::Script.label(), "script");
        assert_eq!(ShadowDiffClass::VersionDetect.label(), "version_detect");
    }
}
