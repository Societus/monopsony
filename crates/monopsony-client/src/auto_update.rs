//! Scheduled auto-update system for shadow packages.
#![allow(dead_code)]
//!
//! Uses cron expressions to schedule automatic package updates during
//! maintenance windows, with severity-based filtering.

use std::str::FromStr;

use anyhow::{bail, Context, Result};
use chrono::Utc;
use cron::Schedule;

use monopsony_common::config::AutoUpdateConfig;

/// Validate that a cron expression can be parsed and produces upcoming times.
pub fn validate_schedule(expr: &str) -> Result<()> {
    let schedule = Schedule::from_str(expr).context("invalid cron expression")?;
    if schedule.upcoming(Utc).next().is_none() {
        bail!("cron expression produces no future times");
    }
    Ok(())
}

/// Validate that a timezone string is parseable.
pub fn validate_timezone(tz: &str) -> Result<chrono_tz::Tz> {
    tz.parse::<chrono_tz::Tz>()
        .map_err(|e| anyhow::anyhow!("invalid timezone '{}': {}", tz, e))
}

/// Check whether an update with the given severity annotation should be
/// auto-applied based on the configured max_severity.
///
/// Severity levels from highest to lowest: breaking > warning > info
/// max_severity controls the *highest* severity that is allowed:
///   "all"      -> allow everything (info, warning, breaking)
///   "warning"  -> allow info and warning, block breaking
///   "info"     -> only allow info-level updates
///   "breaking" -> block everything (never auto-update, for safety)
pub fn should_auto_update(max_severity: &str, annotation_severity: &str) -> bool {
    // "breaking" as max_severity means "block everything" — safety kill switch
    if max_severity == "breaking" {
        return false;
    }
    let max = severity_rank(max_severity);
    let actual = severity_rank(annotation_severity);
    // Both must be recognized (> 0) and actual severity must not exceed max
    max > 0 && actual > 0 && actual <= max
}

fn severity_rank(s: &str) -> u8 {
    match s {
        "info" => 1,
        "warning" => 2,
        "breaking" => 3,
        "all" => u8::MAX,
        _ => 0, // unknown — treated as unrankable, blocked
    }
}

/// Compute the delay until the next scheduled time, given a cron expression
/// and timezone. Returns None if the schedule produces no future times.
pub fn next_delay(config: &AutoUpdateConfig) -> Option<std::time::Duration> {
    let schedule = Schedule::from_str(&config.schedule).ok()?;
    let tz: chrono_tz::Tz = config.timezone.parse().ok()?;
    let now = Utc::now().with_timezone(&tz);
    let next = schedule.upcoming(tz).next()?;
    let delay = (next - now).to_std().ok()?;
    Some(delay)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_valid_cron() {
        assert!(validate_schedule("0 0 3 * * * *").is_ok()); // daily at 3 AM
        assert!(validate_schedule("0 0 0 1 * * *").is_ok()); // monthly on 1st
    }

    #[test]
    fn validate_invalid_cron() {
        assert!(validate_schedule("not a cron").is_err());
    }

    #[test]
    fn validate_timezone_valid() {
        assert!(validate_timezone("UTC").is_ok());
        assert!(validate_timezone("America/New_York").is_ok());
        assert!(validate_timezone("Europe/Warsaw").is_ok());
    }

    #[test]
    fn validate_timezone_invalid() {
        assert!(validate_timezone("Invalid/Zone").is_err());
    }

    #[test]
    fn should_auto_update_all_allows_everything() {
        assert!(should_auto_update("all", "info"));
        assert!(should_auto_update("all", "warning"));
        assert!(should_auto_update("all", "breaking"));
    }

    #[test]
    fn should_auto_update_warning_blocks_breaking() {
        assert!(should_auto_update("warning", "info"));
        assert!(should_auto_update("warning", "warning"));
        assert!(!should_auto_update("warning", "breaking"));
    }

    #[test]
    fn should_auto_update_breaking_blocks_all() {
        // "breaking" as max_severity means "only allow breaking" but since breaking
        // is the highest severity, this effectively blocks everything
        // (you'd never auto-update breaking changes)
        assert!(!should_auto_update("breaking", "breaking"));
        assert!(!should_auto_update("breaking", "warning"));
    }

    #[test]
    fn should_auto_update_unknown_severity_blocked() {
        assert!(!should_auto_update("all", "unknown"));
    }

    #[test]
    fn next_delay_returns_duration() {
        let config = AutoUpdateConfig {
            enabled: true,
            schedule: "0 0 3 * * * *".to_string(),
            timezone: "UTC".to_string(),
            max_severity: "all".to_string(),
            notify_before_secs: None,
        };
        let delay = next_delay(&config);
        assert!(delay.is_some());
        // Should be at most 24 hours
        assert!(delay.unwrap().as_secs() < 86400);
    }

    #[test]
    fn default_config_is_disabled() {
        let config = AutoUpdateConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.schedule, "0 0 3 * * * *");
        assert_eq!(config.timezone, "UTC");
        assert_eq!(config.max_severity, "all");
    }
}
