//! Minimal provisioning-step template engine for shadow recipes.
//!
//! Expands `[build].steps[]` strings (see docs/design/shadow-packages.md §3 invariant #1
//! and §5 `[build]`). This is a deliberately small engine — variable substitution plus
//! simple conditionals — explicitly NOT a full Jinja/Ansible dependency swap.
//!
//! Supported syntax, and nothing more:
//! - `{{var_name}}` — substitute from [`StepContext`] (whitespace-tolerant: `{{ var }}`).
//!   Unknown variables are a hard error — never silently substituted with empty.
//! - `{{#if var == "literal"}}...{{/if}}` — include the body only when the variable's
//!   value equals the literal. No else, no nesting, no boolean operators.
//! - Everything else passes through verbatim.
//!
//! Backwards compatibility: newer recipes may emit constructs older engine versions
//! reject; the engine fails loud rather than silently mangling a step.

use std::fmt;

/// Context values available to a provisioning-step template.
#[derive(Debug, Clone)]
pub struct StepContext {
    pub version: String,
    pub target_dir: String,
    pub arch: String,
    pub os: String,
    pub libc: String,
}

/// Errors raised while rendering a provisioning step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TemplateError {
    /// A `{{name}}` or condition references a variable not present in the context.
    UnknownVariable { name: String },
    /// The template uses syntax outside the supported subset or is malformed.
    MalformedTemplate { message: String },
}

impl fmt::Display for TemplateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TemplateError::UnknownVariable { name } => {
                write!(f, "Unknown variable in step template: {}", name)
            }
            TemplateError::MalformedTemplate { message } => {
                write!(f, "Malformed step template: {}", message)
            }
        }
    }
}

impl std::error::Error for TemplateError {}

impl StepContext {
    fn lookup(&self, name: &str) -> Option<&str> {
        match name {
            "version" => Some(&self.version),
            "target_dir" => Some(&self.target_dir),
            "arch" => Some(&self.arch),
            "os" => Some(&self.os),
            "libc" => Some(&self.libc),
            _ => None,
        }
    }
}

/// Render a single provisioning step.
///
/// Fails loud on unknown variables and malformed constructs — silent-empty
/// substitution is a known bug class and is deliberately avoided here.
pub fn render_step(template: &str, ctx: &StepContext) -> Result<String, TemplateError> {
    render_inner(template, ctx, true)
}

/// Core walker. `allow_if` is true only at the top level: an `{{#if}}` body is
/// rendered for substitutions but must not contain a nested conditional.
fn render_inner(
    template: &str,
    ctx: &StepContext,
    allow_if: bool,
) -> Result<String, TemplateError> {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;

    while let Some(start) = rest.find("{{") {
        // Verbatim text before the directive.
        out.push_str(&rest[..start]);
        let after_open = &rest[start + 2..];

        // Conditional: {{#if var == "literal"}} ... {{/if}}
        if let Some(cond_rest) = after_open.strip_prefix("#if ") {
            if !allow_if {
                return Err(TemplateError::MalformedTemplate {
                    message: "nested {{#if}} inside an {{#if}} body is not supported".to_string(),
                });
            }
            let cond_end =
                cond_rest
                    .find("}}")
                    .ok_or_else(|| TemplateError::MalformedTemplate {
                        message: "unclosed {{#if ...}} directive".to_string(),
                    })?;
            let (var, literal) = parse_condition(&cond_rest[..cond_end])?;
            let value = ctx
                .lookup(var)
                .ok_or_else(|| TemplateError::UnknownVariable {
                    name: var.to_string(),
                })?;
            let after_cond = &cond_rest[cond_end + 2..];

            let close_tag = "{{/if}}";
            let body_end =
                after_cond
                    .find(close_tag)
                    .ok_or_else(|| TemplateError::MalformedTemplate {
                        message: format!(
                            "missing {} for {{{{#if}}}} on variable {}",
                            close_tag, var
                        ),
                    })?;
            if value == literal {
                out.push_str(&render_inner(&after_cond[..body_end], ctx, false)?);
            }
            rest = &after_cond[body_end + close_tag.len()..];
            continue;
        }

        // Plain substitution: {{var}}
        let end = after_open
            .find("}}")
            .ok_or_else(|| TemplateError::MalformedTemplate {
                message: "unclosed {{ directive".to_string(),
            })?;
        let inner = after_open[..end].trim();
        if inner.is_empty() {
            return Err(TemplateError::MalformedTemplate {
                message: "empty {{}} directive".to_string(),
            });
        }
        let value = ctx
            .lookup(inner)
            .ok_or_else(|| TemplateError::UnknownVariable {
                name: inner.to_string(),
            })?;
        out.push_str(value);
        rest = &after_open[end + 2..];
    }

    out.push_str(rest);
    Ok(out)
}

/// Parse `var == "literal"` — the only conditional form supported.
fn parse_condition(cond: &str) -> Result<(&str, &str), TemplateError> {
    let cond = cond.trim();
    let (var, literal_part) =
        cond.split_once("==")
            .ok_or_else(|| TemplateError::MalformedTemplate {
                message: format!("condition must be `var == \"literal\"`, got: {}", cond),
            })?;
    let var = var.trim();
    if var.is_empty() || !var.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(TemplateError::MalformedTemplate {
            message: format!("invalid variable name in condition: {}", var),
        });
    }
    let literal = literal_part.trim();
    let literal = literal
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .ok_or_else(|| TemplateError::MalformedTemplate {
            message: format!("condition literal must be double-quoted, got: {}", literal),
        })?;
    Ok((var, literal))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> StepContext {
        StepContext {
            version: "1.2.3".to_string(),
            target_dir: "/opt/myapp".to_string(),
            arch: "x86_64".to_string(),
            os: "linux".to_string(),
            libc: "glibc".to_string(),
        }
    }

    #[test]
    fn substitutes_each_variable() {
        let c = ctx();
        assert_eq!(render_step("{{version}}", &c).unwrap(), "1.2.3");
        assert_eq!(render_step("{{target_dir}}", &c).unwrap(), "/opt/myapp");
        assert_eq!(render_step("{{arch}}", &c).unwrap(), "x86_64");
        assert_eq!(render_step("{{os}}", &c).unwrap(), "linux");
        assert_eq!(render_step("{{libc}}", &c).unwrap(), "glibc");
    }

    #[test]
    fn whitespace_tolerant_directives() {
        let c = ctx();
        assert_eq!(render_step("{{ version }}", &c).unwrap(), "1.2.3");
        assert_eq!(render_step("{{  target_dir  }}", &c).unwrap(), "/opt/myapp");
        assert_eq!(
            render_step("{{version}}-{{ arch}}", &c).unwrap(),
            "1.2.3-x86_64"
        );
    }

    #[test]
    fn conditional_true_includes_body() {
        let c = ctx();
        let t = "{{#if os == \"linux\"}}systemctl enable myapp{{/if}}";
        assert_eq!(render_step(t, &c).unwrap(), "systemctl enable myapp");
    }

    #[test]
    fn conditional_false_omits_body() {
        let c = ctx();
        let t = "pre {{#if os == \"darwin\"}}launchctl load myapp{{/if}} post";
        assert_eq!(render_step(t, &c).unwrap(), "pre  post");
    }

    #[test]
    fn conditional_with_substitution_in_body() {
        let c = ctx();
        let t = "{{#if libc == \"glibc\"}}ln -s {{target_dir}}/bin /usr/local/bin/myapp{{/if}}";
        assert_eq!(
            render_step(t, &c).unwrap(),
            "ln -s /opt/myapp/bin /usr/local/bin/myapp"
        );
    }

    #[test]
    fn unknown_variable_errors() {
        let c = ctx();
        let err = render_step("install {{pkg_name}}", &c).unwrap_err();
        assert_eq!(
            err,
            TemplateError::UnknownVariable {
                name: "pkg_name".to_string()
            }
        );
    }

    #[test]
    fn unknown_variable_in_condition_errors() {
        let c = ctx();
        let err = render_step("{{#if flavor == \"vanilla\"}}x{{/if}}", &c).unwrap_err();
        assert_eq!(
            err,
            TemplateError::UnknownVariable {
                name: "flavor".to_string()
            }
        );
    }

    #[test]
    fn unclosed_substitution_errors() {
        let c = ctx();
        let err = render_step("install {{version", &c).unwrap_err();
        assert!(matches!(err, TemplateError::MalformedTemplate { .. }));
    }

    #[test]
    fn unclosed_if_errors() {
        let c = ctx();
        let err = render_step("{{#if os == \"linux\"}}systemctl enable", &c).unwrap_err();
        assert!(matches!(err, TemplateError::MalformedTemplate { .. }));
    }

    #[test]
    fn missing_endif_errors() {
        let c = ctx();
        let err = render_step("{{#if os == \"linux\"}}body without close", &c).unwrap_err();
        assert!(matches!(err, TemplateError::MalformedTemplate { .. }));
    }

    #[test]
    fn unquoted_condition_literal_errors() {
        let c = ctx();
        let err = render_step("{{#if os == linux}}x{{/if}}", &c).unwrap_err();
        assert!(matches!(err, TemplateError::MalformedTemplate { .. }));
    }

    #[test]
    fn empty_directive_errors() {
        let c = ctx();
        let err = render_step("a {{}} b", &c).unwrap_err();
        assert!(matches!(err, TemplateError::MalformedTemplate { .. }));
    }

    #[test]
    fn nested_if_is_malformed() {
        let c = ctx();
        let t = "{{#if os == \"linux\"}}{{#if libc == \"glibc\"}}x{{/if}}{{/if}}";
        let err = render_step(t, &c).unwrap_err();
        assert!(matches!(err, TemplateError::MalformedTemplate { .. }));
    }

    #[test]
    fn realistic_pacman_step() {
        let c = ctx();
        let t = "pacman -S --noconfirm {{version}}-deps && mkdir -p {{target_dir}} && cp {{target_dir}}/config /etc/myapp.conf";
        assert_eq!(
            render_step(t, &c).unwrap(),
            "pacman -S --noconfirm 1.2.3-deps && mkdir -p /opt/myapp && cp /opt/myapp/config /etc/myapp.conf"
        );
    }

    #[test]
    fn realistic_conditional_step() {
        let c = ctx();
        let t = "curl -L -o /tmp/app.tar.gz {{#if arch == \"x86_64\"}}https://example.com/app-amd64.tar.gz{{/if}}{{#if arch == \"aarch64\"}}https://example.com/app-arm64.tar.gz{{/if}} && tar -C {{target_dir}} -xzf /tmp/app.tar.gz";
        assert_eq!(
            render_step(t, &c).unwrap(),
            "curl -L -o /tmp/app.tar.gz https://example.com/app-amd64.tar.gz && tar -C /opt/myapp -xzf /tmp/app.tar.gz"
        );
    }

    #[test]
    fn no_directives_passes_through_verbatim() {
        let c = ctx();
        let t = "echo hello && touch /tmp/flag && cp app.conf /etc/app.conf";
        assert_eq!(render_step(t, &c).unwrap(), t);
    }

    #[test]
    fn empty_template_renders_empty() {
        let c = ctx();
        assert_eq!(render_step("", &c).unwrap(), "");
    }

    #[test]
    fn error_display_is_informative() {
        let c = ctx();
        let err = render_step("{{nope}}", &c).unwrap_err();
        assert_eq!(err.to_string(), "Unknown variable in step template: nope");
        let err = render_step("{{#if os == \"linux\"}}x", &c).unwrap_err();
        assert!(err.to_string().contains("Malformed step template"));
    }
}
