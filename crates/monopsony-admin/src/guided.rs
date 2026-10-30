//! Guided OS-release upgrade recipes (Sprint 29 Phase 4, grounding D6).
//!
//! Authored per-distro recipe data (repo `recipes/guided/*.toml`) is parsed
//! with the EXISTING shadow `ShadowRecipe`/`RecipeScript` schema — a
//! display-only repurpose per plan decision #4, zero new TOML parsing code.
//! At render time the client's persisted verdict fills the live variables
//! (`{from_version}`, `{to_version}`, `{from_major}`, `{to_major}`) and the
//! parsed `[script].update` lines become an ordered, paste-ready checklist.
//!
//! LOAD-BEARING: this module RENDERS TEXT and nothing else. There is no
//! command executor here and none may be added — the server renders
//! instructions, never executes them (plan §Trust model, §Proven-failure).
//!
//! Safe ordering is data + test: the authored PVE ordering (plan Phase 4
//! example, verbatim) is asserted step-for-step in the tests below, so a
//! wrong sequence ("stop corosync before the first dist-upgrade") fails the
//! suite instead of shipping.

use monopsony_common::shadow_types::{HealthCheckType, ShadowRecipe};
use regex::Regex;
use serde::Serialize;
use std::sync::OnceLock;

/// Authored recipe data, baked into the binary (same embedding rationale as
/// `DEFAULT_MONOPSONY_RECIPE`: fleet deploys ship binaries via scp, so the
/// guides must not depend on runtime data files).
pub const DEBIAN_RELEASE_RECIPE: &str = include_str!("../../../recipes/guided/debian-release.toml");
pub const UBUNTU_DO_RELEASE_UPGRADE_RECIPE: &str =
    include_str!("../../../recipes/guided/ubuntu-do-release-upgrade.toml");
pub const PROXMOX_VE_RECIPE: &str = include_str!("../../../recipes/guided/proxmox-ve.toml");

/// One rendered checklist step. Wire-shape mirrors the proto
/// `GuidedRecipeStep` message (plus nothing — keep it exactly aligned).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GuidedRecipeStep {
    pub order: u32,
    pub group: String,
    pub action: String,
    /// True when the step must be reviewed/cleared before the next one.
    pub blocking: bool,
}

/// A fully rendered guide for one client. Wire-shape mirrors the proto
/// `GuidedRecipe` message plus display metadata the dashboard needs.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GuidedRecipe {
    pub recipe_id: String,
    pub title: String,
    pub distro_label: String,
    /// Authored from the recipe's `downgrade_unsafe` flag: an OS release has
    /// no atomic rollback (plan §Proven-failure) — surfaced as a warning.
    pub downgrade_unsafe: bool,
    pub config_paths: Vec<String>,
    pub steps: Vec<GuidedRecipeStep>,
}

/// Verdict-sourced live variables for one render.
#[derive(Debug, Clone, Default)]
pub struct RenderInputs {
    pub from_version: Option<String>,
    pub to_version: Option<String>,
}

fn pve_tool_marker() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"pve[0-9]+to[0-9]+").expect("static regex is valid"))
}

/// True when the verdict notes identify a `pve<n>to<m>` vendor tool (or the
/// client-side note names the host as Proxmox VE). PVE hosts report
/// os-release ID=debian — this is what lifts the presentation above plain
/// debian (B2 polish gap, in scope for Phase 4).
fn is_pve_notes(notes: &[String]) -> bool {
    notes
        .iter()
        .any(|n| pve_tool_marker().is_match(n) || n.contains("Proxmox VE"))
}

/// Presentation label for the dashboard row/guide: "Proxmox VE" when the
/// client's verdict classifies it as PVE, else the reported distro id.
pub fn classify_distro_label(distro: &str, notes: &[String]) -> String {
    // Only a debian-reporting host can be a PVE host (PVE ships ID=debian).
    if distro == "debian" && is_pve_notes(notes) {
        "Proxmox VE".to_string()
    } else {
        distro.to_string()
    }
}

/// Pick the authored recipe for a client: (recipe id, recipe TOML). The PVE
/// classification wins over plain debian for debian-reporting PVE hosts.
pub fn recipe_for(distro: &str, notes: &[String]) -> Option<(&'static str, &'static str)> {
    if distro == "debian" && is_pve_notes(notes) {
        return Some(("proxmox-ve", PROXMOX_VE_RECIPE));
    }
    match distro {
        "debian" => Some(("debian-release", DEBIAN_RELEASE_RECIPE)),
        "ubuntu" => Some((
            "ubuntu-do-release-upgrade",
            UBUNTU_DO_RELEASE_UPGRADE_RECIPE,
        )),
        _ => None,
    }
}

/// Leading major of a version string ("12" → 12, "13.2" → 13). Same rule as
/// the Phase 2 catalog merge; unparsable versions yield no variable.
fn parse_major(version: &str) -> Option<String> {
    version
        .split('.')
        .next()
        .map(str::trim)
        .and_then(|m| m.parse::<u64>().ok())
        .map(|m| m.to_string())
}

/// Known placeholders and their verdict-sourced values. A `None` value whose
/// placeholder appears in a step withholds that step (safe placeholder, never
/// a partially filled command).
fn placeholders(inputs: &RenderInputs) -> Vec<(&'static str, Option<String>)> {
    vec![
        ("{from_version}", inputs.from_version.clone()),
        ("{to_version}", inputs.to_version.clone()),
        (
            "{from_major}",
            inputs.from_version.as_deref().and_then(parse_major),
        ),
        (
            "{to_major}",
            inputs.to_version.as_deref().and_then(parse_major),
        ),
    ]
}

/// Substitute variables into one authored line. Returns the filled text plus
/// the placeholders that could NOT be filled (verbatim, braces included, so
/// prose fallbacks can strip them).
fn fill_line(line: &str, inputs: &RenderInputs) -> (String, Vec<&'static str>) {
    let mut out = line.to_string();
    let mut missing = Vec::new();
    for (placeholder, value) in placeholders(inputs) {
        match value {
            Some(v) => out = out.replace(placeholder, &v),
            None => {
                if out.contains(placeholder) {
                    missing.push(placeholder);
                }
            }
        }
    }
    (out, missing)
}

/// Group + blocking flag for one step. Group names follow the proto comment
/// ("package", "sources", "services", "reboot"; plus "upgrade" for the
/// vendor front-end and "verify" for confirm/check steps). Blocking = the
/// step gates the rest of the guide (review before continuing). Pure
/// function — unit-tested.
fn classify_step(action: &str) -> (&'static str, bool) {
    let lower = action.to_lowercase();
    if lower.contains("reboot") {
        ("reboot", true)
    } else if lower.contains("systemctl stop") {
        ("services", true)
    } else if lower.contains("sources.list") || lower.contains("apt sources") {
        ("sources", true)
    } else if lower.contains("do-release-upgrade") {
        ("upgrade", true)
    } else if lower.contains("confirm") || lower.contains("verify") {
        ("verify", true)
    } else {
        ("package", false)
    }
}

/// Checklist step text for a parsed `[install.health_check]` (display-only
/// repurpose: the schema's health check becomes the guide's final
/// verification step).
fn health_check_action(hc: &monopsony_common::shadow_types::HealthCheck) -> String {
    match &hc.check_type {
        HealthCheckType::Port => format!(
            "Verify: a service answers on port {} (timeout {}s)",
            hc.port
                .map(|p| p.to_string())
                .unwrap_or_else(|| "?".to_string()),
            hc.timeout_secs
        ),
        HealthCheckType::Http => format!(
            "Verify: {} responds (timeout {}s)",
            hc.url.as_deref().unwrap_or("the health URL"),
            hc.timeout_secs
        ),
        HealthCheckType::Process => format!(
            "Verify: process '{}' is running (timeout {}s)",
            hc.process.as_deref().unwrap_or("the service"),
            hc.timeout_secs
        ),
    }
}

/// Render an authored recipe TOML into the display checklist for a client
/// verdict. Never executes anything; parse errors surface as Err text for
/// the REST layer. Missing verdict variables WITHHOLD the affected step
/// behind an explicit non-command notice — a partial command is never
/// rendered.
pub fn render(recipe_toml: &str, inputs: &RenderInputs) -> Result<GuidedRecipe, String> {
    let recipe: ShadowRecipe =
        toml::from_str(recipe_toml).map_err(|e| format!("guided recipe parse failed: {e}"))?;

    let mut steps: Vec<GuidedRecipeStep> = Vec::new();
    if let Some(script) = &recipe.script {
        for line in script.update.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (filled, missing) = fill_line(line, inputs);
            let (group, action, blocking) = if missing.is_empty() {
                let (group, blocking) = classify_step(&filled);
                (group.to_string(), filled, blocking)
            } else {
                // Safe placeholder: the step is withheld whole. The notice is
                // prose, not a command — pasting it can never run anything.
                let names = missing
                    .iter()
                    .map(|m| m.trim_matches('{').trim_matches('}'))
                    .collect::<Vec<_>>()
                    .join(", ");
                (
                    "blocked".to_string(),
                    format!(
                        "[not rendered: missing variable '{names}' — confirm the target release, then follow the vendor release notes for this step]"
                    ),
                    true,
                )
            };
            steps.push(GuidedRecipeStep {
                order: steps.len() as u32 + 1,
                group,
                action,
                blocking,
            });
        }
    }

    if let Some(hc) = &recipe.install.health_check {
        steps.push(GuidedRecipeStep {
            order: steps.len() as u32 + 1,
            group: "verify".to_string(),
            action: health_check_action(hc),
            blocking: true,
        });
    }

    // Title is prose, not a command: unfilled placeholders degrade to
    // "unknown" instead of leaking braces into the UI.
    let mut title = recipe
        .package
        .description
        .clone()
        .unwrap_or_else(|| recipe.package.name.clone());
    let (filled_title, missing_title) = fill_line(&title, inputs);
    title = filled_title;
    for m in missing_title {
        title = title.replace(m, "unknown");
    }

    Ok(GuidedRecipe {
        recipe_id: recipe.package.name.clone(),
        title,
        distro_label: String::new(),
        downgrade_unsafe: recipe.downgrade_unsafe,
        config_paths: recipe.install.config_paths.clone().unwrap_or_default(),
        steps,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render_pve(from: &str, to: &str) -> GuidedRecipe {
        render(
            PROXMOX_VE_RECIPE,
            &RenderInputs {
                from_version: Some(from.to_string()),
                to_version: Some(to.to_string()),
            },
        )
        .expect("pve recipe renders")
    }

    /// The plan's Phase 4 safe PVE ordering, asserted step-for-step. Any
    /// authored drift in ordering or wording breaks this test.
    #[test]
    fn pve_recipe_renders_the_safe_ordering() {
        let g = render_pve("8", "9");
        assert_eq!(g.recipe_id, "proxmox-ve");
        assert!(g.downgrade_unsafe, "an OS release has no atomic rollback");
        let expected = vec![
            GuidedRecipeStep {
                order: 1,
                group: "package".into(),
                action: "apt update && apt full-upgrade -y".into(),
                blocking: false,
            },
            GuidedRecipeStep {
                order: 2,
                group: "sources".into(),
                action: "Change the apt sources to the new release slug lines: pve8 -> pve9 and the Debian 8 suite -> 9 suite in /etc/apt/sources.list and /etc/apt/sources.list.d/".into(),
                blocking: true,
            },
            GuidedRecipeStep {
                order: 3,
                group: "services".into(),
                action: "systemctl stop corosync pve-cluster ceph-mon ceph-mgr ceph-osd".into(),
                blocking: true,
            },
            GuidedRecipeStep {
                order: 4,
                group: "package".into(),
                action: "apt update && apt full-upgrade -y".into(),
                blocking: false,
            },
            GuidedRecipeStep {
                order: 5,
                group: "reboot".into(),
                action: "Confirm the upgrade completed, then reboot into the new 9 kernel".into(),
                blocking: true,
            },
            GuidedRecipeStep {
                order: 6,
                group: "verify".into(),
                action: "Verify: a service answers on port 8006 (timeout 60s)".into(),
                blocking: true,
            },
        ];
        assert_eq!(g.steps, expected);
    }

    /// Wrong-order drift must fail the assertion, not pass by accident:
    /// stopping the cluster services before the first dist-upgrade produces
    /// a DIFFERENT step list than the asserted safe ordering.
    #[test]
    fn pve_wrong_order_drift_is_caught() {
        let drifted_toml = PROXMOX_VE_RECIPE.replace(
            "apt update && apt full-upgrade -y\nChange the apt sources",
            "systemctl stop corosync pve-cluster ceph-mon ceph-mgr ceph-osd\napt update && apt full-upgrade -y\nChange the apt sources",
        );
        let drifted = render(
            &drifted_toml,
            &RenderInputs {
                from_version: Some("8".to_string()),
                to_version: Some("9".to_string()),
            },
        )
        .expect("drifted recipe still parses");
        assert_ne!(drifted.steps, render_pve("8", "9").steps);
        assert_eq!(drifted.steps[0].group, "services");
    }

    #[test]
    fn ubuntu_recipe_renders_vendor_flow_with_variables() {
        let g = render(
            UBUNTU_DO_RELEASE_UPGRADE_RECIPE,
            &RenderInputs {
                from_version: Some("22.04".to_string()),
                to_version: Some("24.04".to_string()),
            },
        )
        .expect("ubuntu recipe renders");
        assert_eq!(g.recipe_id, "ubuntu-do-release-upgrade");
        let groups: Vec<&str> = g.steps.iter().map(|s| s.group.as_str()).collect();
        assert_eq!(
            groups,
            vec!["package", "reboot", "verify", "upgrade", "reboot"]
        );
        // Settle-first ordering: the dist-upgrade precedes do-release-upgrade.
        assert!(g.steps[0].action.starts_with("apt update"));
        assert!(g.steps.iter().position(|s| s.group == "upgrade").unwrap() > 0);
        assert!(g.steps[2].action.contains("Ubuntu 24.04"));
        assert_eq!(g.steps[3].action, "do-release-upgrade");
        assert!(g.steps[3].blocking);
    }

    #[test]
    fn debian_recipe_renders_sources_retarget_flow() {
        let g = render(
            DEBIAN_RELEASE_RECIPE,
            &RenderInputs {
                from_version: Some("12".to_string()),
                to_version: Some("13".to_string()),
            },
        )
        .expect("debian recipe renders");
        assert_eq!(g.recipe_id, "debian-release");
        assert_eq!(g.steps.len(), 5);
        assert!(g.steps[0].action.starts_with("apt update"));
        assert_eq!(g.steps[1].group, "sources");
        assert!(g.steps[1].action.contains("(Debian 12)"));
        assert!(g.steps[1].action.contains("13"));
        // Second dist-upgrade must follow the sources retarget.
        assert_eq!(g.steps[2].group, "package");
        assert_eq!(g.steps[4].group, "reboot");
        assert!(g
            .config_paths
            .contains(&"/etc/apt/sources.list".to_string()));
    }

    #[test]
    fn missing_verdict_variables_withhold_steps_never_partial_commands() {
        let g = render(
            PROXMOX_VE_RECIPE,
            &RenderInputs {
                from_version: Some("8".to_string()),
                to_version: None,
            },
        )
        .expect("render succeeds without verdict data");
        for step in &g.steps {
            assert!(!step.action.contains('{'), "no raw placeholders: {step:?}");
            assert!(
                !step.action.contains("pve9"),
                "no half-filled slug: {step:?}"
            );
        }
        // Steps needing the target are withheld behind an explicit notice.
        let blocked: Vec<&GuidedRecipeStep> =
            g.steps.iter().filter(|s| s.group == "blocked").collect();
        assert_eq!(blocked.len(), 2, "sources + reboot steps need to_version");
        for step in blocked {
            assert!(step.blocking);
            assert!(step
                .action
                .starts_with("[not rendered: missing variable 'to_"));
        }
        // The variable-free steps render intact — never partial commands.
        assert_eq!(g.steps[0].action, "apt update && apt full-upgrade -y");
        assert_eq!(
            g.steps[2].action,
            "systemctl stop corosync pve-cluster ceph-mon ceph-mgr ceph-osd"
        );
    }

    #[test]
    fn all_authored_recipes_parse_with_the_real_parser() {
        for (id, toml_src) in [
            ("debian-release", DEBIAN_RELEASE_RECIPE),
            (
                "ubuntu-do-release-upgrade",
                UBUNTU_DO_RELEASE_UPGRADE_RECIPE,
            ),
            ("proxmox-ve", PROXMOX_VE_RECIPE),
        ] {
            let recipe: ShadowRecipe =
                toml::from_str(toml_src).unwrap_or_else(|e| panic!("{id}: {e}"));
            assert_eq!(recipe.package.name, id);
            assert_eq!(
                recipe.install.method,
                monopsony_common::shadow_types::InstallMethod::Script
            );
            assert!(!recipe
                .script
                .as_ref()
                .expect("script section")
                .update
                .is_empty());
        }
    }

    #[test]
    fn pve_presentation_label_lifts_debian_reporting_hosts() {
        let pve_notes = vec!["vendor check: pve8to9 (Proxmox VE preflight)".to_string()];
        assert_eq!(classify_distro_label("debian", &pve_notes), "Proxmox VE");
        // Checker-absent PVE hosts still classify via the client's note.
        let no_checker = vec![
            "no pve<n>to<m> vendor checker found on host (Proxmox VE host per /usr/bin/pveversion)"
                .to_string(),
        ];
        assert_eq!(classify_distro_label("debian", &no_checker), "Proxmox VE");
        // Plain debian and other distros keep their reported id.
        assert_eq!(classify_distro_label("debian", &[]), "debian");
        assert_eq!(classify_distro_label("ubuntu", &pve_notes), "ubuntu");
    }

    #[test]
    fn recipe_selection_follows_pve_classification_then_distro() {
        let pve_notes = vec!["vendor check: pve8to9 (Proxmox VE preflight)".to_string()];
        assert_eq!(recipe_for("debian", &pve_notes).unwrap().0, "proxmox-ve");
        assert_eq!(recipe_for("debian", &[]).unwrap().0, "debian-release");
        assert_eq!(
            recipe_for("ubuntu", &[]).unwrap().0,
            "ubuntu-do-release-upgrade"
        );
        assert!(recipe_for("arch", &[]).is_none());
    }

    #[test]
    fn step_classification_covers_the_proto_groups() {
        assert_eq!(
            classify_step("apt update && apt full-upgrade -y"),
            ("package", false)
        );
        assert_eq!(classify_step("apt autoremove -y"), ("package", false));
        assert_eq!(
            classify_step("Update /etc/apt/sources.list per the release notes"),
            ("sources", true)
        );
        assert_eq!(
            classify_step("systemctl stop corosync pve-cluster ceph-mon"),
            ("services", true)
        );
        assert_eq!(classify_step("do-release-upgrade"), ("upgrade", true));
        assert_eq!(
            classify_step("Confirm the vendor preflight verdict is ready"),
            ("verify", true)
        );
        assert_eq!(
            classify_step("Reboot into the new kernel"),
            ("reboot", true)
        );
    }
}
