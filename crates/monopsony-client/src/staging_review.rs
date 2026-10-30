//! Pure helpers for the shadow staging review flow (`shadow view` / `shadow
//! accept`). Extracted verbatim from the inline CLI flow in `main.rs` so the
//! decision, override, and render logic is unit-testable without a live gRPC
//! server. The CLI functions call these — do not fork the logic back inline.

use monopsony_common::proto::monopsony::{FirewallRuleInfo, ShadowPackageInfo, ShadowStagingInfo};

/// Find a pending staged item by name-or-id selector (the `--name` argument
/// matches either the recipe name or the staging row id).
pub fn find_staged<'a>(
    pending: &'a [ShadowStagingInfo],
    name: &str,
) -> Option<&'a ShadowStagingInfo> {
    pending
        .iter()
        .find(|s| s.recipe_name == name || s.id == name)
}

/// Parse the interactive menu selection. `Ok(None)` = quit ("q"/"quit"),
/// `Ok(Some(i))` = 0-based index into the pending list, `Err` = invalid input
/// or out-of-range number (messages match the CLI bail texts).
pub fn parse_menu_selection(choice: &str, len: usize) -> Result<Option<usize>, String> {
    let t = choice.trim();
    if t == "q" || t == "quit" {
        return Ok(None);
    }
    let idx: usize = t.parse().map_err(|_| format!("Invalid selection: '{t}'"))?;
    if idx == 0 || idx > len {
        return Err(format!("Selection out of range: {idx}"));
    }
    Ok(Some(idx - 1))
}

/// Keep the suggested value when the operator input is blank, else the trimmed
/// input (target dir and health check override behavior).
pub fn keep_or_replace(input: &str, suggested: &str) -> String {
    if input.trim().is_empty() {
        suggested.to_string()
    } else {
        input.trim().to_string()
    }
}

/// Keep the suggested list on blank input, else parse a comma-separated list,
/// trimming each element and dropping empties (accessory dirs and firewall
/// rule override behavior).
pub fn parse_csv_or_keep(input: &str, suggested: &[String]) -> Vec<String> {
    if input.trim().is_empty() {
        suggested.to_vec()
    } else {
        input
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    }
}

/// Non-interactive accept collapses suggested firewall rules to port-only
/// strings (the client cannot type overrides).
pub fn firewall_strings_non_interactive(rules: &[FirewallRuleInfo]) -> Vec<String> {
    rules.iter().map(|fw| fw.port.to_string()).collect()
}

/// Interactive firewall override: on blank input keep the suggested rules
/// rendered as "port/protocol/direction", else parse the typed comma-separated
/// list.
pub fn firewall_rules_or_keep(input: &str, suggested: &[FirewallRuleInfo]) -> Vec<String> {
    if input.trim().is_empty() {
        suggested
            .iter()
            .map(|fw| format!("{}/{}/{}", fw.port, fw.protocol, fw.direction))
            .collect()
    } else {
        input
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    }
}

/// Parse "port/protocol/direction/description" strings back into
/// `FirewallRuleInfo` — the interactive review round-trip format. Entries
/// with fewer than two segments are dropped; an unparsable port becomes 0; a
/// missing direction defaults to "inbound".
pub fn parse_firewall_rule_strings(rules: &[String]) -> Vec<FirewallRuleInfo> {
    rules
        .iter()
        .filter_map(|s| {
            let parts: Vec<&str> = s.split('/').collect();
            if parts.len() >= 2 {
                Some(FirewallRuleInfo {
                    port: parts[0].parse().unwrap_or(0),
                    protocol: parts[1].to_string(),
                    direction: parts.get(2).unwrap_or(&"inbound").to_string(),
                    description: parts.get(3).map(|d| d.to_string()).unwrap_or_default(),
                })
            } else {
                None
            }
        })
        .collect()
}

/// One-line summary of an installed/managed package as `shadow view` prints it.
pub fn render_installed_summary(p: &ShadowPackageInfo) -> String {
    format!(
        "  • {} v{}  ({})  id={}  target={}",
        p.name, p.version, p.source_type, p.id, p.recipe_path
    )
}

/// One-line summary of a staged item as `shadow view` prints it.
pub fn render_staged_summary(s: &ShadowStagingInfo) -> String {
    format!(
        "  • {}  status={}  suggested_target={}  id={}",
        s.recipe_name, s.status, s.suggested_target_dir, s.id
    )
}

/// Firewall rule line as `shadow view` prints it.
pub fn render_firewall_view_line(fw: &FirewallRuleInfo) -> String {
    format!(
        "      firewall: {} {}/{} ({})",
        fw.port, fw.protocol, fw.direction, fw.description
    )
}

/// Firewall rule line as the `shadow accept` review block prints it.
pub fn render_firewall_review_line(fw: &FirewallRuleInfo) -> String {
    format!(
        "    - {} {}/{} ({})",
        fw.port, fw.protocol, fw.direction, fw.description
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn staging(id: &str, name: &str, status: &str) -> ShadowStagingInfo {
        ShadowStagingInfo {
            id: id.to_string(),
            recipe_name: name.to_string(),
            status: status.to_string(),
            suggested_target_dir: "/opt/svc".to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn find_staged_matches_recipe_name() {
        let pending = vec![staging("row-1", "authentik", "pending")];
        let found = find_staged(&pending, "authentik").expect("name match");
        assert_eq!(found.id, "row-1");
    }

    #[test]
    fn find_staged_matches_row_id() {
        let pending = vec![staging("row-1", "authentik", "pending")];
        let found = find_staged(&pending, "row-1").expect("id match");
        assert_eq!(found.recipe_name, "authentik");
    }

    #[test]
    fn find_staged_returns_none_on_miss() {
        let pending = vec![staging("row-1", "authentik", "pending")];
        assert!(find_staged(&pending, "nope").is_none());
    }

    #[test]
    fn menu_selection_quit_variants() {
        assert_eq!(parse_menu_selection("q", 2), Ok(None));
        assert_eq!(parse_menu_selection(" quit ", 2), Ok(None));
    }

    #[test]
    fn menu_selection_index_is_one_based() {
        assert_eq!(parse_menu_selection("1", 2), Ok(Some(0)));
        assert_eq!(parse_menu_selection(" 2 ", 2), Ok(Some(1)));
    }

    #[test]
    fn menu_selection_rejects_garbage_and_out_of_range() {
        assert!(parse_menu_selection("x", 2).is_err());
        assert!(parse_menu_selection("0", 2).is_err());
        assert!(parse_menu_selection("3", 2).is_err());
    }

    #[test]
    fn keep_or_replace_blank_keeps_suggested() {
        assert_eq!(keep_or_replace("", "/opt/svc"), "/opt/svc");
        assert_eq!(keep_or_replace("   ", "/opt/svc"), "/opt/svc");
    }

    #[test]
    fn keep_or_replace_value_trims_and_wins() {
        assert_eq!(keep_or_replace("  /srv/alt  ", "/opt/svc"), "/srv/alt");
    }

    #[test]
    fn csv_or_keep_blank_keeps_suggested_list() {
        let suggested = vec!["/etc/svc".to_string(), "/var/lib/svc".to_string()];
        assert_eq!(parse_csv_or_keep("", &suggested), suggested);
    }

    #[test]
    fn csv_or_keep_parses_trims_and_drops_empties() {
        let out = parse_csv_or_keep(" /a , /b ,, ,", &[]);
        assert_eq!(out, vec!["/a".to_string(), "/b".to_string()]);
    }

    #[test]
    fn non_interactive_firewall_collapses_to_ports() {
        let rules = vec![
            FirewallRuleInfo {
                port: 80,
                protocol: "tcp".into(),
                direction: "inbound".into(),
                description: "web".into(),
            },
            FirewallRuleInfo {
                port: 53,
                protocol: "udp".into(),
                direction: "inbound".into(),
                description: String::new(),
            },
        ];
        assert_eq!(
            firewall_strings_non_interactive(&rules),
            vec!["80".to_string(), "53".to_string()]
        );
    }

    #[test]
    fn firewall_round_trip_full_form() {
        let parsed = parse_firewall_rule_strings(&["8080/tcp/outbound/api".to_string()]);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].port, 8080);
        assert_eq!(parsed[0].protocol, "tcp");
        assert_eq!(parsed[0].direction, "outbound");
        assert_eq!(parsed[0].description, "api");
    }

    #[test]
    fn firewall_defaults_direction_and_description() {
        let parsed = parse_firewall_rule_strings(&["443/tcp".to_string()]);
        assert_eq!(parsed[0].direction, "inbound");
        assert_eq!(parsed[0].description, "");
    }

    #[test]
    fn firewall_unparsable_port_becomes_zero() {
        let parsed = parse_firewall_rule_strings(&["http/tcp".to_string()]);
        assert_eq!(parsed[0].port, 0);
    }

    #[test]
    fn firewall_drops_entries_without_protocol_segment() {
        let parsed = parse_firewall_rule_strings(&[
            "8080".to_string(),
            "bare-string".to_string(),
            "22/tcp".to_string(),
        ]);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].port, 22);
    }

    #[test]
    fn firewall_prompt_format_glues_description_into_direction() {
        // Documents shipped behavior: the interactive prompt advertises
        // "port/protocol/direction:description", but parsing splits on '/'
        // only, so the prompt format lands the ":description" suffix inside
        // `direction` and leaves `description` empty. Only the slash-separated
        // 4-segment form round-trips (see firewall_round_trip_full_form).
        let parsed = parse_firewall_rule_strings(&["8080/tcp/inbound:api".to_string()]);
        assert_eq!(parsed[0].direction, "inbound:api");
        assert_eq!(parsed[0].description, "");
    }

    #[test]
    fn firewall_or_keep_blank_renders_port_proto_direction() {
        let suggested = vec![FirewallRuleInfo {
            port: 9090,
            protocol: "tcp".into(),
            direction: "inbound".into(),
            description: "metrics".into(),
        }];
        assert_eq!(
            firewall_rules_or_keep("", &suggested),
            vec!["9090/tcp/inbound".to_string()]
        );
    }

    #[test]
    fn firewall_or_keep_input_parses_csv() {
        let out = firewall_rules_or_keep(" 80/tcp , 443/tcp ", &[]);
        assert_eq!(out, vec!["80/tcp".to_string(), "443/tcp".to_string()]);
    }

    #[test]
    fn view_renders_staged_summary_line() {
        let s = staging("row-9", "checkmk-server", "pending");
        assert_eq!(
            render_staged_summary(&s),
            "  • checkmk-server  status=pending  suggested_target=/opt/svc  id=row-9"
        );
    }

    #[test]
    fn view_renders_installed_summary_line() {
        let p = ShadowPackageInfo {
            name: "sonarr".into(),
            version: "4.0.0".into(),
            source_type: "direct_url".into(),
            id: "abc".into(),
            recipe_path: "/opt/sonarr".into(),
            ..Default::default()
        };
        assert_eq!(
            render_installed_summary(&p),
            "  • sonarr v4.0.0  (direct_url)  id=abc  target=/opt/sonarr"
        );
    }

    #[test]
    fn view_and_review_firewall_lines_differ_only_in_indent() {
        let fw = FirewallRuleInfo {
            port: 9090,
            protocol: "tcp".into(),
            direction: "inbound".into(),
            description: "metrics".into(),
        };
        assert_eq!(
            render_firewall_view_line(&fw),
            "      firewall: 9090 tcp/inbound (metrics)"
        );
        assert_eq!(
            render_firewall_review_line(&fw),
            "    - 9090 tcp/inbound (metrics)"
        );
    }
}
