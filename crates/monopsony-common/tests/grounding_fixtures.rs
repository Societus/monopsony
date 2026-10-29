//! Grounding-fixture tests (Sprint 32 Phases 1–3).
//!
//! The fixtures under `recipes/shadow/fixtures/` are the grounding data for the
//! declarative-recipes redesign (docs/plans/sprint32-grounding.md). These tests
//! keep them load-bearing:
//! 1. every TOML fixture parses as a real `ShadowRecipe` (fixtures must encode
//!    the real schema — a spec bug ships as green tests);
//! 2. the phase-1 expected-render block is TRUE against the actual
//!    `render_step` engine (linux/glibc substitution + conditional drop);
//! 3. the phase-3 negative case (v1.0.0 → v1.0.1) is byte-identical on every
//!    diffable field, machine-checking the "zero warnings" claim.

use std::path::PathBuf;

use monopsony_common::shadow_template::{render_step, StepContext};

fn fixture_path(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../recipes/shadow/fixtures")
        .join(rel)
}

fn parse_fixture(rel: &str) -> monopsony_common::shadow_types::ShadowRecipe {
    let text = std::fs::read_to_string(fixture_path(rel)).expect("fixture readable");
    toml::from_str(&text).expect("fixture must parse as ShadowRecipe")
}

fn grounding_ctx(os: &str, libc: &str) -> StepContext {
    StepContext {
        version: "2.5.0".to_string(),
        target_dir: "~/.local/lib/example-app".to_string(),
        arch: "x86_64".to_string(),
        os: os.to_string(),
        libc: libc.to_string(),
    }
}

#[test]
fn all_toml_fixtures_parse_as_shadow_recipe() {
    for rel in [
        "phase1-provisioning-steps.toml",
        "phase3-update-diffing/app-v1.0.0.toml",
        "phase3-update-diffing/app-v1.0.1.toml",
        "phase3-update-diffing/app-v2.0.0.toml",
    ] {
        let recipe = parse_fixture(rel);
        assert!(
            !recipe.package.name.is_empty(),
            "{rel}: package.name must be set"
        );
        assert!(
            !recipe.install.target_dir.is_empty(),
            "{rel}: install.target_dir must be set"
        );
    }
}

#[test]
fn phase1_expected_render_block_is_true_linux_glibc() {
    let recipe = parse_fixture("phase1-provisioning-steps.toml");
    let build = recipe.build.expect("phase1 fixture has [build]");
    let ctx = grounding_ctx("linux", "glibc");

    let rendered: Vec<String> = build
        .steps
        .iter()
        .map(|s| render_step(s, &ctx).expect("fixture steps must render"))
        .collect();

    assert_eq!(
        rendered[0],
        "mkdir -p ~/.local/lib/example-app && cd ~/.local/lib/example-app"
    );
    assert_eq!(
        rendered[1],
        "git clone --branch v2.5.0 https://github.com/example/app ."
    );
    assert_eq!(
        rendered[2],
        "make BUILD_TYPE=Release CFLAGS=\"-DAPP_VERSION=2.5.0\""
    );
    assert_eq!(
        rendered[3],
        "cp app.service /etc/systemd/system/ || true",
        "linux branch must be included"
    );
    assert_eq!(
        rendered[4],
        "ln -sf ~/.local/lib/example-app/app /usr/local/bin/example-app",
        "glibc branch must be included"
    );
}

#[test]
fn phase1_conditionals_drop_body_off_platform() {
    let recipe = parse_fixture("phase1-provisioning-steps.toml");
    let build = recipe.build.expect("phase1 fixture has [build]");

    // os=darwin: the linux-only systemd-copy step renders empty.
    let darwin = grounding_ctx("darwin", "glibc");
    assert_eq!(
        render_step(&build.steps[3], &darwin).unwrap(),
        "",
        "non-linux os must drop the conditional body"
    );

    // libc=musl: the glibc symlink step renders empty.
    let musl = grounding_ctx("linux", "musl");
    assert_eq!(
        render_step(&build.steps[4], &musl).unwrap(),
        "",
        "non-glibc libc must drop the conditional body"
    );
}

#[test]
fn phase3_negative_case_has_zero_requirement_churn() {
    let v1 = parse_fixture("phase3-update-diffing/app-v1.0.0.toml");
    let v1_patch = parse_fixture("phase3-update-diffing/app-v1.0.1.toml");

    assert_eq!(v1.install.service, v1_patch.install.service);
    assert_eq!(v1.install.pre_update, v1_patch.install.pre_update);
    assert_eq!(v1.install.post_update, v1_patch.install.post_update);
    assert_eq!(v1.install.config_paths, v1_patch.install.config_paths);
    assert_eq!(
        serde_json::to_string(&v1.install.health_check).unwrap(),
        serde_json::to_string(&v1_patch.install.health_check).unwrap()
    );
    assert_eq!(
        serde_json::to_string(&v1.build).unwrap(),
        serde_json::to_string(&v1_patch.build).unwrap(),
        "patch release: build section identical"
    );
    assert_eq!(
        serde_json::to_string(&v1.script).unwrap(),
        serde_json::to_string(&v1_patch.script).unwrap()
    );
    assert_eq!(
        serde_json::to_string(&v1.version).unwrap(),
        serde_json::to_string(&v1_patch.version).unwrap()
    );
}

#[test]
fn phase3_churn_pair_exhibits_every_expected_diff() {
    let v1 = parse_fixture("phase3-update-diffing/app-v1.0.0.toml");
    let v2 = parse_fixture("phase3-update-diffing/app-v2.0.0.toml");

    // 1. service unit renamed
    assert_eq!(v1.install.service.as_deref(), Some("app.service"));
    assert_eq!(v2.install.service.as_deref(), Some("appd.service"));
    // 2. new build requirement
    assert!(!v1.build.as_ref().unwrap().requires.contains(&"openssl".to_string()));
    assert!(v2.build.as_ref().unwrap().requires.contains(&"openssl".to_string()));
    // 3. new provisioning steps (env file + quadlet sidecar)
    assert_eq!(v1.build.as_ref().unwrap().steps.len(), 3);
    assert_eq!(v2.build.as_ref().unwrap().steps.len(), 5);
    // 4. health check moved
    assert_ne!(
        v1.install.health_check.as_ref().unwrap().port,
        v2.install.health_check.as_ref().unwrap().port
    );
}
