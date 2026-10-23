//! Init-system service script support for shadow recipes (Sprint 32 sa-C #3,
//! docs/design/shadow-packages.md §3 invariant #2).
//!
//! Recipes describe how software RUNS. v1 targets systemd and OpenRC only;
//! SysV/init.d, runit, s6 and friends are deliberately deferred to later small
//! phases rather than stubbed — a half-supported init script that "looks"
//! installed is worse than an explicit error.
//!
//! A default "no package" template set ships IN THE CLIENT BINARY
//! ([`SYSTEMD_UNIT_TEMPLATE`] / [`OPENRC_SCRIPT_TEMPLATE`]) so operators can
//! generate a blank service file per init system ([`blank_service_file`]) or
//! render a filled-in one from a [`ServiceSpec`] ([`render_service_file`]).
//! Location substitution into these scripts is part of rendering an install.
//!
//! This is a service-specific renderer, NOT a reuse of the provisioning-step
//! engine (`monopsony-common::shadow_template`): that engine substitutes from
//! a fixed 5-field build context (version/target_dir/arch/os/libc) which
//! cannot express unit_name/description/exec_start/working_dir. The
//! fail-loud philosophy is shared — an unknown `{{var}}` is a hard error,
//! never a silent-empty substitution.

// Library layer only for this slice: the executor/CLI wiring is a later
// phase (same situation as the other shadow modules pre-wiring), so the
// public API is intentionally unused within the binary for now.
#![allow(dead_code)]

use std::fmt;

/// Default systemd unit template, embedded at compile time.
pub const SYSTEMD_UNIT_TEMPLATE: &str = include_str!("templates/monopsony.service.tmpl");

/// Default OpenRC init script template, embedded at compile time.
pub const OPENRC_SCRIPT_TEMPLATE: &str = include_str!("templates/monopsony.openrc.tmpl");

/// The init systems v1 supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitSystem {
    Systemd,
    OpenRC,
}

impl fmt::Display for InitSystem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InitSystem::Systemd => write!(f, "systemd"),
            InitSystem::OpenRC => write!(f, "openrc"),
        }
    }
}

/// Detect the init system from the CONTENT of /etc/os-release.
///
/// Pure function: never reads the file itself; callers supply the content.
///
/// Default choice: **systemd**. Fleet reality is that the overwhelming
/// majority of managed Linux hosts (Debian, Ubuntu, RHEL, Alma, Rocky,
/// Fedora, Arch, SUSE) run systemd, so an unrecognized or absent os-release
/// lands on the systemd path rather than erroring; OpenRC hosts (Alpine,
/// Gentoo) are a deliberate, explicitly-detected minority.
pub fn detect_init_system(os_release: &str) -> InitSystem {
    let mut id: Option<String> = None;
    let mut id_like: Option<String> = None;

    for line in os_release.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            let value = value.trim().trim_matches('"').to_string();
            match key {
                "ID" => id = Some(value),
                "ID_LIKE" => id_like = Some(value),
                _ => {}
            }
        }
    }

    let id = id.as_deref().unwrap_or("");
    let id_like = id_like.as_deref().unwrap_or("");

    // ID_LIKE inheritance first: derivatives announce their parent family.
    let like_tokens: Vec<&str> = id_like.split_whitespace().collect();
    if like_tokens.iter().any(|t| matches!(*t, "arch" | "fedora")) {
        return InitSystem::Systemd;
    }
    if like_tokens.contains(&"alpine") {
        return InitSystem::OpenRC;
    }

    match id {
        "arch" | "fedora" | "almalinux" | "rocky" | "rhel" => InitSystem::Systemd,
        "alpine" | "gentoo" => InitSystem::OpenRC,
        // debian/ubuntu and everything unknown default to systemd (see doc
        // comment above).
        _ => InitSystem::Systemd,
    }
}

/// The four values a service template needs. Deliberately NOT the
/// provisioning-step context: recipes run services, they do not build them.
#[derive(Debug, Clone)]
pub struct ServiceSpec {
    pub unit_name: String,
    pub description: String,
    pub exec_start: String,
    pub working_dir: String,
}

/// Errors raised while rendering a service file. Same shape as the sibling
/// engine's `TemplateError` but local — the variable sets differ, so the
/// types are not interchangeable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceTemplateError {
    /// A `{{name}}` placeholder references a value not present in the spec.
    UnknownVariable { name: String },
    /// The template uses syntax outside the supported subset or is malformed.
    MalformedTemplate { message: String },
}

impl fmt::Display for ServiceTemplateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ServiceTemplateError::UnknownVariable { name } => {
                write!(f, "Unknown variable in service template: {}", name)
            }
            ServiceTemplateError::MalformedTemplate { message } => {
                write!(f, "Malformed service template: {}", message)
            }
        }
    }
}

impl std::error::Error for ServiceTemplateError {}

impl ServiceSpec {
    fn lookup(&self, name: &str) -> Option<&str> {
        match name {
            "unit_name" => Some(&self.unit_name),
            "description" => Some(&self.description),
            "exec_start" => Some(&self.exec_start),
            "working_dir" => Some(&self.working_dir),
            _ => None,
        }
    }
}

/// Render the service file for `init` with `spec` substituted into the
/// embedded default template.
///
/// Fails loud on unknown placeholders and malformed constructs — a template
/// placeholder with no matching spec field is a hard error, never a
/// silent-empty substitution (same discipline as
/// `monopsony-common::shadow_template::render_step`).
pub fn render_service_file(
    init: InitSystem,
    spec: &ServiceSpec,
) -> Result<String, ServiceTemplateError> {
    let template = match init {
        InitSystem::Systemd => SYSTEMD_UNIT_TEMPLATE,
        InitSystem::OpenRC => OPENRC_SCRIPT_TEMPLATE,
    };
    render_template(template, spec)
}

/// Core substitution walker. Minimal subset, deliberately: `{{var_name}}`
/// only (whitespace-tolerant), no conditionals — service files don't need
/// them, and every feature added here is a feature recipes can rely on
/// across engine versions.
fn render_template(template: &str, spec: &ServiceSpec) -> Result<String, ServiceTemplateError> {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;

    while let Some(start) = rest.find("{{") {
        // Verbatim text before the directive.
        out.push_str(&rest[..start]);
        let after_open = &rest[start + 2..];

        let end = after_open
            .find("}}")
            .ok_or_else(|| ServiceTemplateError::MalformedTemplate {
                message: "unclosed {{ directive".to_string(),
            })?;
        let inner = after_open[..end].trim();
        if inner.is_empty() {
            return Err(ServiceTemplateError::MalformedTemplate {
                message: "empty {{}} directive".to_string(),
            });
        }
        let value = spec
            .lookup(inner)
            .ok_or_else(|| ServiceTemplateError::UnknownVariable {
                name: inner.to_string(),
            })?;
        out.push_str(value);
        rest = &after_open[end + 2..];
    }

    out.push_str(rest);
    Ok(out)
}

/// Return the embedded default template RAW (with `{{placeholders}}` intact)
/// for the given init system, for operators to hand-edit. The `# REPLACE_ME`
/// guidance comment at the top of each template matches the repo's existing
/// REPLACE_ME draft convention (shadow_cli.rs render_draft_toml).
pub fn blank_service_file(init: InitSystem) -> &'static str {
    match init {
        InitSystem::Systemd => SYSTEMD_UNIT_TEMPLATE,
        InitSystem::OpenRC => OPENRC_SCRIPT_TEMPLATE,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> ServiceSpec {
        ServiceSpec {
            unit_name: "myapp.service".to_string(),
            description: "My Application".to_string(),
            exec_start: "/opt/myapp/bin/myapp --serve".to_string(),
            working_dir: "/opt/myapp".to_string(),
        }
    }

    // ---- detection (hermetic: content strings only, never file reads) ----

    #[test]
    fn detects_arch_as_systemd() {
        let content = "NAME=\"Arch Linux\"\nID=arch\nID_LIKE=\"arch\"\n";
        assert_eq!(detect_init_system(content), InitSystem::Systemd);
    }

    #[test]
    fn detects_fedora_as_systemd() {
        let content = "NAME=\"Fedora Linux\"\nID=fedora\n";
        assert_eq!(detect_init_system(content), InitSystem::Systemd);
    }

    #[test]
    fn detects_arch_like_derivative_as_systemd() {
        // Artix-style: ID is unknown but ID_LIKE carries the arch family.
        let content = "ID=artix\nID_LIKE=\"arch\"\n";
        assert_eq!(detect_init_system(content), InitSystem::Systemd);
    }

    #[test]
    fn detects_alpine_as_openrc() {
        let content = "NAME=\"Alpine Linux\"\nID=alpine\n";
        assert_eq!(detect_init_system(content), InitSystem::OpenRC);
    }

    #[test]
    fn detects_gentoo_as_openrc() {
        let content = "NAME=\"Gentoo\"\nID=gentoo\n";
        assert_eq!(detect_init_system(content), InitSystem::OpenRC);
    }

    #[test]
    fn detects_debian_as_systemd() {
        let content = "NAME=\"Debian GNU/Linux\"\nID=debian\n";
        assert_eq!(detect_init_system(content), InitSystem::Systemd);
    }

    #[test]
    fn detects_ubuntu_as_systemd() {
        let content = "NAME=\"Ubuntu\"\nID=ubuntu\nID_LIKE=\"debian\"\n";
        assert_eq!(detect_init_system(content), InitSystem::Systemd);
    }

    #[test]
    fn detects_rhel_family_as_systemd() {
        for id in ["almalinux", "rocky", "rhel"] {
            let content = format!("ID={}\n", id);
            assert_eq!(detect_init_system(&content), InitSystem::Systemd);
        }
    }

    #[test]
    fn unknown_id_defaults_to_systemd() {
        let content = "NAME=\"Plan 9\"\nID=plan9\n";
        assert_eq!(detect_init_system(content), InitSystem::Systemd);
    }

    #[test]
    fn absent_fields_default_to_systemd() {
        assert_eq!(detect_init_system(""), InitSystem::Systemd);
        assert_eq!(detect_init_system("NAME=\"Bare\"\n"), InitSystem::Systemd);
    }

    #[test]
    fn display_strings() {
        assert_eq!(InitSystem::Systemd.to_string(), "systemd");
        assert_eq!(InitSystem::OpenRC.to_string(), "openrc");
    }

    // ---- rendering ----

    #[test]
    fn renders_systemd_unit_end_to_end() {
        let out = render_service_file(InitSystem::Systemd, &spec()).unwrap();
        assert!(out.contains("[Unit]"));
        assert!(out.contains("Description=My Application"));
        assert!(out.contains("After=network.target"));
        assert!(out.contains("[Service]"));
        assert!(out.contains("Type=simple"));
        assert!(out.contains("ExecStart=/opt/myapp/bin/myapp --serve"));
        assert!(out.contains("WorkingDirectory=/opt/myapp"));
        assert!(out.contains("Restart=on-failure"));
        assert!(out.contains("WantedBy=multi-user.target"));
        assert!(!out.contains("{{"));
    }

    #[test]
    fn renders_openrc_script_end_to_end() {
        let out = render_service_file(InitSystem::OpenRC, &spec()).unwrap();
        assert!(out.starts_with("#!/sbin/openrc-run"));
        assert!(out.contains("description=\"My Application\""));
        assert!(out.contains("command=\"/opt/myapp/bin/myapp --serve\""));
        assert!(out.contains("command_background=yes"));
        assert!(out.contains("pidfile=\"/run/${RC_SVCNAME}.pid\""));
        assert!(out.contains("depend() {"));
        assert!(out.contains("need net"));
        assert!(!out.contains("{{"));
    }

    #[test]
    fn unknown_placeholder_fails_loud() {
        // Tamper the embedded template string in-test to prove the error
        // fires — the shipped templates must never gain an unrenderable
        // placeholder without a compile-time-visible failure here.
        let tampered = SYSTEMD_UNIT_TEMPLATE.replace("{{description}}", "{{bogus_field}}");
        let err = render_template(&tampered, &spec()).unwrap_err();
        assert_eq!(
            err,
            ServiceTemplateError::UnknownVariable {
                name: "bogus_field".to_string()
            }
        );
        assert_eq!(
            err.to_string(),
            "Unknown variable in service template: bogus_field"
        );
    }

    #[test]
    fn unclosed_directive_is_malformed() {
        // Stripping every closing brace guarantees the FIRST directive is
        // unclosed, so the walker must report malformed (no later `}}` can
        // be mistaken for this directive's closer).
        let tampered = OPENRC_SCRIPT_TEMPLATE.replace("}}", "");
        let err = render_template(&tampered, &spec()).unwrap_err();
        assert!(matches!(
            err,
            ServiceTemplateError::MalformedTemplate { .. }
        ));
    }

    #[test]
    fn whitespace_tolerant_placeholders() {
        let s = spec();
        assert_eq!(
            render_template("{{ unit_name }}", &s).unwrap(),
            "myapp.service"
        );
        assert_eq!(
            render_template("{{  working_dir  }}", &s).unwrap(),
            "/opt/myapp"
        );
    }

    // ---- blank template generation ----

    #[test]
    fn blank_systemd_template_keeps_placeholders() {
        let raw = blank_service_file(InitSystem::Systemd);
        assert!(raw.contains("# REPLACE_ME"));
        for var in [
            "{{unit_name}}",
            "{{description}}",
            "{{exec_start}}",
            "{{working_dir}}",
        ] {
            assert!(raw.contains(var), "systemd template missing {}", var);
        }
    }

    #[test]
    fn blank_openrc_template_keeps_placeholders() {
        let raw = blank_service_file(InitSystem::OpenRC);
        assert!(raw.contains("# REPLACE_ME"));
        assert!(raw.starts_with("#!/sbin/openrc-run"));
        for var in [
            "{{unit_name}}",
            "{{description}}",
            "{{exec_start}}",
            "{{working_dir}}",
        ] {
            assert!(raw.contains(var), "openrc template missing {}", var);
        }
    }

    #[test]
    fn every_embedded_placeholder_renders() {
        // Guard against a template gaining a placeholder the fixed 4-field
        // spec cannot satisfy — that would turn every install into a
        // fail-loud error.
        let s = spec();
        for init in [InitSystem::Systemd, InitSystem::OpenRC] {
            render_service_file(init, &s)
                .unwrap_or_else(|e| panic!("{} template unrenderable: {}", init, e));
        }
    }
}
