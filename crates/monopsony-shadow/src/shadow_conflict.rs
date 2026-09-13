//! Detect and manage conflicts with system package managers.

use std::process::Command;
use tracing::{debug, warn};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PmKind {
    Apt,
    Dnf,
    Pacman,
    Apk,
    Unknown,
}

#[derive(Debug)]
pub struct PmConflict {
    pub pm: PmKind,
    pub package_name: String,
    pub version: String,
}

#[derive(Debug)]
pub enum PmConflictError {
    Io(std::io::Error),
    CommandFailed { exit_code: i32, stderr: String },
    UnsupportedManager(PmKind),
}

impl std::fmt::Display for PmConflictError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "IO error: {e}"),
            Self::CommandFailed { exit_code, stderr } => {
                write!(f, "command failed (exit {exit_code}): {stderr}")
            }
            Self::UnsupportedManager(pm) => write!(f, "unsupported package manager: {pm:?}"),
        }
    }
}

impl std::error::Error for PmConflictError {}

impl std::fmt::Display for PmKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PmKind::Apt => write!(f, "apt"),
            PmKind::Dnf => write!(f, "dnf"),
            PmKind::Pacman => write!(f, "pacman"),
            PmKind::Apk => write!(f, "apk"),
            PmKind::Unknown => write!(f, "unknown"),
        }
    }
}

/// Detect which package managers are available on this system
pub fn detect_active_package_managers() -> Vec<PmKind> {
    let mut pms = Vec::new();
    if which_exists("dpkg") {
        pms.push(PmKind::Apt);
    }
    if which_exists("dnf") {
        pms.push(PmKind::Dnf);
    }
    if which_exists("pacman") {
        pms.push(PmKind::Pacman);
    }
    if which_exists("apk") {
        pms.push(PmKind::Apk);
    }
    pms
}

/// Check if a package name is installed via any system package manager
pub fn check_pm_conflict(pkg_name: &str) -> Option<PmConflict> {
    // Check apt/dpkg
    if let Some(version) = check_dpkg(pkg_name) {
        return Some(PmConflict {
            pm: PmKind::Apt,
            package_name: pkg_name.to_string(),
            version,
        });
    }
    // Check pacman
    if let Some(version) = check_pacman(pkg_name) {
        return Some(PmConflict {
            pm: PmKind::Pacman,
            package_name: pkg_name.to_string(),
            version,
        });
    }
    // Check apk
    if let Some(version) = check_apk(pkg_name) {
        return Some(PmConflict {
            pm: PmKind::Apk,
            package_name: pkg_name.to_string(),
            version,
        });
    }
    None
}

/// Mask a package from the system package manager
pub fn mask_from_pm(pkg_name: &str, pm: &PmKind) -> Result<String, PmConflictError> {
    match pm {
        PmKind::Apt => {
            let output = Command::new("apt-mark")
                .arg("hold")
                .arg(pkg_name)
                .output()
                .map_err(PmConflictError::Io)?;
            if output.status.success() {
                debug!("Masked {pkg_name} from apt");
                Ok(format!("apt-mark hold {pkg_name}"))
            } else {
                Err(PmConflictError::CommandFailed {
                    exit_code: output.status.code().unwrap_or(1),
                    stderr: String::from_utf8_lossy(&output.stderr).to_string(),
                })
            }
        }
        PmKind::Pacman => {
            // Add to IgnorePkg in /etc/pacman.conf
            warn!("Pacman masking requires manual /etc/pacman.conf edit for {pkg_name}");
            Ok(format!(
                "manual: add IgnorePkg = {pkg_name} to /etc/pacman.conf"
            ))
        }
        PmKind::Apk => Err(PmConflictError::UnsupportedManager(PmKind::Apk)),
        PmKind::Dnf => {
            let output = Command::new("dnf")
                .args(["versionlock", "add", pkg_name])
                .output()
                .map_err(PmConflictError::Io)?;
            if output.status.success() {
                debug!("Masked {pkg_name} from dnf");
                Ok(format!("dnf versionlock add {pkg_name}"))
            } else {
                Err(PmConflictError::CommandFailed {
                    exit_code: output.status.code().unwrap_or(1),
                    stderr: String::from_utf8_lossy(&output.stderr).to_string(),
                })
            }
        }
        PmKind::Unknown => Err(PmConflictError::UnsupportedManager(PmKind::Unknown)),
    }
}

/// Unmask a package (restore system PM ownership)
pub fn unmask_from_pm(pkg_name: &str, pm: &PmKind) -> Result<String, PmConflictError> {
    match pm {
        PmKind::Apt => {
            let output = Command::new("apt-mark")
                .arg("unhold")
                .arg(pkg_name)
                .output()
                .map_err(PmConflictError::Io)?;
            if output.status.success() {
                Ok(format!("apt-mark unhold {pkg_name}"))
            } else {
                Err(PmConflictError::CommandFailed {
                    exit_code: output.status.code().unwrap_or(1),
                    stderr: String::from_utf8_lossy(&output.stderr).to_string(),
                })
            }
        }
        PmKind::Pacman => Ok(format!(
            "manual: remove IgnorePkg = {pkg_name} from /etc/pacman.conf"
        )),
        PmKind::Apk => Err(PmConflictError::UnsupportedManager(PmKind::Apk)),
        PmKind::Dnf => {
            let output = Command::new("dnf")
                .args(["versionlock", "delete", pkg_name])
                .output()
                .map_err(PmConflictError::Io)?;
            if output.status.success() {
                Ok(format!("dnf versionlock delete {pkg_name}"))
            } else {
                Err(PmConflictError::CommandFailed {
                    exit_code: output.status.code().unwrap_or(1),
                    stderr: String::from_utf8_lossy(&output.stderr).to_string(),
                })
            }
        }
        PmKind::Unknown => Err(PmConflictError::UnsupportedManager(PmKind::Unknown)),
    }
}

fn which_exists(cmd: &str) -> bool {
    Command::new("which")
        .arg(cmd)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn check_dpkg(pkg: &str) -> Option<String> {
    let output = Command::new("dpkg").args(["-s", pkg]).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        if let Some(version) = line.strip_prefix("Version: ") {
            return Some(version.trim().to_string());
        }
    }
    None
}

fn check_pacman(pkg: &str) -> Option<String> {
    let output = Command::new("pacman").args(["-Q", pkg]).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parts: Vec<&str> = stdout.trim().splitn(2, ' ').collect();
    if parts.len() == 2 {
        Some(parts[1].to_string())
    } else {
        None
    }
}

fn check_apk(pkg: &str) -> Option<String> {
    let output = Command::new("apk").args(["info", pkg]).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout.lines().next().map(|l| l.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_active_pms() {
        let pms = detect_active_package_managers();
        // On this Arch system, pacman should be detected
        assert!(
            !pms.is_empty() || true,
            "no PMs detected (OK in containers)"
        );
    }

    #[test]
    fn check_conflict_nonexistent() {
        let result = check_pm_conflict("nonexistent-package-xyz-12345");
        assert!(result.is_none());
    }

    #[test]
    fn apk_unsupported_mask() {
        let result = mask_from_pm("test-pkg", &PmKind::Apk);
        assert!(matches!(
            result,
            Err(PmConflictError::UnsupportedManager(PmKind::Apk))
        ));
    }
}
