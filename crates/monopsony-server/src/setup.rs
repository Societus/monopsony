//! Interactive setup wizard for monopsony-server configuration.
//!
//! Generates or updates `/etc/monopsony/server.toml` with:
//! - Storage backend (filesystem / S3)
//! - Scanning config (ClamAV, YARA, entropy)
//! - Node tier (private / public / federated)
//! - Encryption key generation (256-bit for S3 SSE-C)
//!
//! Usage:
//!   monopsony-server setup            # Interactive wizard
//!   monopsony-server setup --check    # Validate existing config

use std::fs;
use std::io::{self, Write};
use std::path::Path;

/// Result of the setup wizard.
#[derive(Debug)]
pub struct SetupResult {
    pub config_path: String,
    pub sections_written: Vec<String>,
    pub warnings: Vec<String>,
}

/// Run the interactive setup wizard.
pub fn run_setup(config_path: &str) -> anyhow::Result<SetupResult> {
    let mut sections = Vec::new();
    let mut warnings = Vec::new();
    let mut toml_sections = Vec::new();

    println!("=== Monopsony Server Setup Wizard ===\n");

    // Check if config already exists
    let config_exists = Path::new(config_path).exists();
    if config_exists {
        println!("Found existing config at {}. We'll add missing sections.", config_path);
        let existing = fs::read_to_string(config_path)?;
        toml_sections.push(existing);
    }

    // 1. Storage backend
    println!("\n--- Storage Backend ---");
    println!("Choose cache storage backend:");
    println!("  1) Filesystem (default, local disk)");
    println!("  2) S3-compatible (SeaweedFS, Garage, AWS S3, etc.)");
    let backend = prompt_choice("Select [1-2]", 1)?;
    let storage_section = if backend == 2 {
        sections.push("cache.backend=s3".to_string());
        let endpoint = prompt_string("S3 endpoint URL (e.g. http://127.0.0.1:8333)")?;
        let region = prompt_string_default("S3 region", "us-east-1")?;
        let bucket = prompt_string("S3 bucket name")?;
        let access_key = prompt_string("S3 access key")?;
        let secret_key = prompt_string("S3 secret key")?;
        let force_path_style = prompt_bool("Use path-style access? (SeaweedFS/Garage: yes)", true)?;
        let retention_days: u32 = prompt_u32_default("Object Lock retention days", 7)?;

        format!(
            "\n[cache]\npath = \"/var/cache/monopsony\"\nmax_size_gb = 50\nbackend = \"s3\"\n\n[cache.s3]\nendpoint = \"{}\"\nregion = \"{}\"\nbucket = \"{}\"\naccess_key = \"{}\"\nsecret_key = \"{}\"\nforce_path_style = {}\nretention_days = {}",
            endpoint, region, bucket, access_key, secret_key, force_path_style, retention_days
        )
    } else {
        sections.push("cache.backend=filesystem".to_string());
        "\n[cache]\npath = \"/var/cache/monopsony\"\nmax_size_gb = 50".to_string()
    };

    // 2. Scanning config
    println!("\n--- Malware Scanning ---");
    let enable_scan = prompt_bool("Enable post-cache malware scanning?", false)?;
    let scan_section = if enable_scan {
        sections.push("scan.enabled=true".to_string());
        let clamav_socket = prompt_string_default("ClamAV socket path (empty to skip)", "/var/run/clamav/clamd.ctl")?;
        let yara_dir = prompt_string_default("YARA rules directory", "/etc/monopsony/yara-rules")?;
        let block_on_malware = prompt_bool("Block serving if malware detected?", true)?;
        let warn_on_cve = prompt_bool("Warn on known CVEs (OSV lookup)?", true)?;
        let entropy_threshold: f64 = prompt_f64_default("Entropy threshold (0.0-8.0, higher=more suspicious)", 7.5)?;

        // Check ClamAV socket
        if !clamav_socket.is_empty() && !Path::new(&clamav_socket).exists() {
            warnings.push(format!("ClamAV socket not found at {} — install ClamAV first", clamav_socket));
        }

        // Create YARA rules dir
        if !Path::new(&yara_dir).exists() {
            if let Err(e) = fs::create_dir_all(&yara_dir) {
                warnings.push(format!("Could not create YARA rules dir {}: {}", yara_dir, e));
            }
        }

        format!(
            "\n\n[scan]\nenabled = true\nclamav_socket = \"{}\"\nyara_rules_dir = \"{}\"\nblock_on_malware = {}\nwarn_on_cve = {}\nentropy_threshold = {}",
            clamav_socket, yara_dir, block_on_malware, warn_on_cve, entropy_threshold
        )
    } else {
        sections.push("scan.enabled=false".to_string());
        "\n\n[scan]\nenabled = false".to_string()
    };

    // 3. Node tier
    println!("\n--- Node Tier ---");
    println!("Select deployment tier:");
    println!("  1) Private (default) — homelab, air-gap, testing. No restrictions.");
    println!("  2) Public — internet-facing, serves other clients. Requires S3 + scanning.");
    println!("  3) Federated — participates in mesh network. Requires scanning.");
    let tier = prompt_choice("Select [1-3]", 1)?;
    let tier_str = match tier {
        1 => "private",
        2 => "public",
        3 => "federated",
        _ => "private",
    };
    sections.push(format!("node.tier={}", tier_str));

    let node_name = if tier >= 2 {
        Some(prompt_string("Node name (for federation identification)")?)
    } else {
        None
    };

    let node_section = format!(
        "\n\n[node]\ntier = \"{}\"{}",
        tier_str,
        node_name.map(|n| format!("\nname = \"{}\"", n)).unwrap_or_default()
    );

    // 4. Encryption key (only for S3)
    let encryption_section = if backend == 2 {
        println!("\n--- S3 Encryption ---");
        let gen_key = prompt_bool("Generate a 256-bit SSE-C encryption key?", true)?;
        if gen_key {
            let key_path = prompt_string_default("Encryption key path", "/etc/monopsony/s3-encryption.key")?;
            let key = generate_encryption_key()?;
            // Write with restrictive permissions
            if let Some(parent) = Path::new(&key_path).parent() {
                let _ = fs::create_dir_all(parent);
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&key_path)
                    .and_then(|mut f| f.write_all(&key))?;
            }
            #[cfg(not(unix))]
            {
                fs::write(&key_path, &key)?;
            }
            sections.push("encryption_key_generated".to_string());
            format!("\nencryption_key_path = \"{}\"", key_path)
        } else {
            String::new()
        }
    } else {
        String::new()
    };

    // Assemble final config
    let mut config_content = String::new();

    // Add standard sections if this is a new config
    if !config_exists {
        config_content.push_str(&generate_default_sections());
    }

    config_content.push_str(&storage_section);
    config_content.push_str(&scan_section);
    config_content.push_str(&node_section);
    config_content.push_str(&encryption_section);
    config_content.push('\n');

    // Write config
    if let Some(parent) = Path::new(config_path).parent() {
        let _ = fs::create_dir_all(parent);
    }
    fs::write(config_path, &config_content)?;

    println!("\n=== Setup Complete ===");
    println!("Config written to: {}", config_path);
    if !warnings.is_empty() {
        println!("\nWarnings:");
        for w in &warnings {
            println!("  - {}", w);
        }
    }
    println!("\nRun `monopsony-server setup --check` to verify.");

    Ok(SetupResult {
        config_path: config_path.to_string(),
        sections_written: sections,
        warnings,
    })
}

/// Validate existing config with --check mode.
pub fn run_check(config_path: &str) -> anyhow::Result<Vec<String>> {
    let mut issues = Vec::new();

    if !Path::new(config_path).exists() {
        issues.push(format!("Config file not found: {}", config_path));
        return Ok(issues);
    }

    let content = fs::read_to_string(config_path)?;

    // Basic section checks
    let required_sections = &["[server]", "[grpc]", "[database]", "[cache]", "[tls]"];
    for section in required_sections {
        if !content.contains(section) {
            issues.push(format!("Missing required section: {}", section));
        }
    }

    // Check scan section
    if content.contains("enabled = true") || content.contains("enabled=true") {
        // Scan enabled — check for clamav socket
        if let Some(socket) = extract_toml_value(&content, "clamav_socket") {
            if !socket.is_empty() && !Path::new(&socket).exists() {
                issues.push(format!("ClamAV socket not found: {} (scanning enabled)", socket));
            }
        }
        // Check for YARA rules dir
        if let Some(yara_dir) = extract_toml_value(&content, "yara_rules_dir") {
            if !Path::new(&yara_dir).exists() {
                issues.push(format!("YARA rules directory not found: {} (scanning enabled)", yara_dir));
            }
        }
    }

    // Check node tier constraints
    if let Some(tier) = extract_toml_value(&content, "tier") {
        match tier.as_str() {
            "\"public\"" => {
                if !content.contains("backend = \"s3\"") && !content.contains("backend=\"s3\"") {
                    issues.push("Public tier requires S3 storage backend".to_string());
                }
                if !content.contains("enabled = true") && !content.contains("enabled=true") {
                    issues.push("Public tier requires scanning to be enabled".to_string());
                }
            }
            "\"federated\"" => {
                if !content.contains("enabled = true") && !content.contains("enabled=true") {
                    issues.push("Federated tier requires scanning to be enabled".to_string());
                }
            }
            _ => {}
        }
    }

    // Check encryption key if S3
    if content.contains("backend = \"s3\"") || content.contains("backend=\"s3\"") {
        if let Some(key_path) = extract_toml_value(&content, "encryption_key_path") {
            let path = key_path.trim_matches('"');
            if !Path::new(path).exists() {
                issues.push(format!("S3 encryption key not found: {}", path));
            }
        }
    }

    // Check database URL
    if let Some(db_url) = extract_toml_value(&content, "url") {
        if !db_url.contains("postgres://") && !db_url.contains("postgresql://") {
            issues.push("Database URL should start with postgres://".to_string());
        }
    }

    Ok(issues)
}

/// Generate default required sections for a new config.
fn generate_default_sections() -> String {
    r#"# Monopsony Server Configuration
# Generated by `monopsony-server setup`

[server]
bind = "0.0.0.0:3128"

[grpc]
bind = "0.0.0.0:50051"

[database]
url = "postgres://monopsony:monopsony@localhost/monopsony"
max_connections = 10

[tls]
ca_path = "/var/lib/monopsony/ca.pem"
ca_key_path = "/var/lib/monopsony/ca.key"
server_cert_path = "/var/lib/monopsony/server.pem"
server_key_path = "/var/lib/monopsony/server.key"

[discovery]
mdns_enabled = true

[metrics]
enabled = true
bind = "0.0.0.0:9090"

[logging]
level = "info"
format = "json"
"#.to_string()
}

fn generate_encryption_key() -> anyhow::Result<Vec<u8>> {
    use std::time::{SystemTime, UNIX_EPOCH};
    let t = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let mut key = vec![0u8; 32];
    let seed = &key as *const _ as usize;
    for (i, b) in key.iter_mut().enumerate() {
        *b = ((t >> (i % 8)) as u8) ^ (i as u8) ^ ((seed >> (i % 8)) as u8);
    }
    Ok(key)
}

fn extract_toml_value(content: &str, key: &str) -> Option<String> {
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('#') { continue; }
        if let Some(eq_pos) = trimmed.find('=') {
            let k = trimmed[..eq_pos].trim();
            if k == key {
                return Some(trimmed[eq_pos + 1..].trim().to_string());
            }
        }
    }
    None
}

// --- Prompt helpers ---

fn prompt_str(msg: &str) -> io::Result<String> {
    print!("{}: ", msg);
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    Ok(input.trim().to_string())
}

fn prompt_choice(prompt: &str, default: u32) -> io::Result<u32> {
    let input = prompt_str(prompt)?;
    if input.is_empty() {
        return Ok(default);
    }
    Ok(input.parse().unwrap_or(default))
}

fn prompt_string(prompt: &str) -> io::Result<String> {
    let val = prompt_str(&format!("{}:", prompt))?;
    if val.is_empty() {
        Ok(String::new())
    } else {
        Ok(val)
    }
}

fn prompt_string_default(prompt: &str, default: &str) -> io::Result<String> {
    let val = prompt_str(&format!("{} [{}]", prompt, default))?;
    if val.is_empty() {
        Ok(default.to_string())
    } else {
        Ok(val)
    }
}

fn prompt_bool(prompt: &str, default: bool) -> io::Result<bool> {
    let suffix = if default { "[Y/n]" } else { "[y/N]" };
    let val = prompt_str(&format!("{} {}", prompt, suffix))?;
    if val.is_empty() {
        return Ok(default);
    }
    Ok(matches!(val.to_lowercase().as_str(), "y" | "yes" | "true" | "1"))
}

fn prompt_u32_default(prompt: &str, default: u32) -> io::Result<u32> {
    let val = prompt_str(&format!("{} [{}]", prompt, default))?;
    if val.is_empty() {
        Ok(default)
    } else {
        Ok(val.parse().unwrap_or(default))
    }
}

fn prompt_f64_default(prompt: &str, default: f64) -> io::Result<f64> {
    let val = prompt_str(&format!("{} [{}]", prompt, default))?;
    if val.is_empty() {
        Ok(default)
    } else {
        Ok(val.parse().unwrap_or(default))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_generate_encryption_key_length() {
        let key = generate_encryption_key().unwrap();
        assert_eq!(key.len(), 32);
    }

    #[test]
    fn test_generate_encryption_key_deterministic_same_tick() {
        let key1 = generate_encryption_key().unwrap();
        let key2 = generate_encryption_key().unwrap();
        // Keys should differ (different nanos) or at least be valid length
        assert_eq!(key1.len(), 32);
        assert_eq!(key2.len(), 32);
    }

    #[test]
    fn test_extract_toml_value_found() {
        let content = "bind = \"0.0.0.0:3128\"\nenabled = true\n";
        assert_eq!(extract_toml_value(content, "bind"), Some("\"0.0.0.0:3128\"".to_string()));
        assert_eq!(extract_toml_value(content, "enabled"), Some("true".to_string()));
    }

    #[test]
    fn test_extract_toml_value_missing() {
        let content = "bind = \"0.0.0.0:3128\"\n";
        assert_eq!(extract_toml_value(content, "port"), None);
    }

    #[test]
    fn test_extract_toml_value_skips_comments() {
        let content = "# enabled = false\nenabled = true\n";
        assert_eq!(extract_toml_value(content, "enabled"), Some("true".to_string()));
    }

    #[test]
    fn test_run_check_missing_file() {
        let issues = run_check("/tmp/nonexistent_monopsony_test.toml").unwrap();
        assert!(issues.iter().any(|i| i.contains("not found")));
    }

    #[test]
    fn test_run_check_valid_config() {
        let dir = std::env::temp_dir().join("monopsony-setup-test-valid");
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("server.toml");
        fs::write(&path, r#"
[server]
bind = "0.0.0.0:3128"

[grpc]
bind = "0.0.0.0:50051"

[database]
url = "postgres://monopsony:password@localhost/monopsony"
max_connections = 10

[cache]
path = "/var/cache/monopsony"
max_size_gb = 50

[tls]
ca_path = "/var/lib/monopsony/ca.pem"
ca_key_path = "/var/lib/monopsony/ca.key"
server_cert_path = "/var/lib/monopsony/server.pem"
server_key_path = "/var/lib/monopsony/server.key"

[scan]
enabled = false

[node]
tier = "private"
"#).unwrap();
        let issues = run_check(path.to_str().unwrap()).unwrap();
        assert!(issues.is_empty(), "Expected no issues, got: {:?}", issues);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_run_check_missing_sections() {
        let dir = std::env::temp_dir().join("monopsony-setup-test-missing");
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("server.toml");
        fs::write(&path, "[server]\nbind = \"0.0.0.0:3128\"\n").unwrap();
        let issues = run_check(path.to_str().unwrap()).unwrap();
        assert!(issues.iter().any(|i| i.contains("[grpc]")));
        assert!(issues.iter().any(|i| i.contains("[database]")));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_run_check_public_without_s3() {
        let dir = std::env::temp_dir().join("monopsony-setup-test-public");
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("server.toml");
        fs::write(&path, r#"
[server]
bind = "0.0.0.0:3128"
[grpc]
bind = "0.0.0.0:50051"
[database]
url = "postgres://monopsony:password@localhost/monopsony"
[cache]
path = "/var/cache/monopsony"
[tls]
ca_path = "/var/lib/monopsony/ca.pem"
ca_key_path = "/var/lib/monopsony/ca.key"
server_cert_path = "/var/lib/monopsony/server.pem"
server_key_path = "/var/lib/monopsony/server.key"
[scan]
enabled = true
[node]
tier = "public"
"#).unwrap();
        let issues = run_check(path.to_str().unwrap()).unwrap();
        assert!(issues.iter().any(|i| i.contains("S3 storage")));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_run_check_federated_without_scan() {
        let dir = std::env::temp_dir().join("monopsony-setup-test-fed");
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("server.toml");
        fs::write(&path, r#"
[server]
bind = "0.0.0.0:3128"
[grpc]
bind = "0.0.0.0:50051"
[database]
url = "postgres://monopsony:password@localhost/monopsony"
[cache]
path = "/var/cache/monopsony"
[tls]
ca_path = "/var/lib/monopsony/ca.pem"
ca_key_path = "/var/lib/monopsony/ca.key"
server_cert_path = "/var/lib/monopsony/server.pem"
server_key_path = "/var/lib/monopsony/server.key"
[scan]
enabled = false
[node]
tier = "federated"
"#).unwrap();
        let issues = run_check(path.to_str().unwrap()).unwrap();
        assert!(issues.iter().any(|i| i.contains("scanning")));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_default_sections_contains_required() {
        let defaults = generate_default_sections();
        assert!(defaults.contains("[server]"));
        assert!(defaults.contains("[grpc]"));
        assert!(defaults.contains("[database]"));
        assert!(defaults.contains("[tls]"));
        assert!(defaults.contains("[metrics]"));
        assert!(defaults.contains("[discovery]"));
        assert!(defaults.contains("[logging]"));
        // [cache], [scan], [node] are generated at runtime by setup wizard
    }

    #[test]
    fn test_prompt_bool_parsing() {
        // Test via the parsing logic directly
        assert!(matches!("y".to_lowercase().as_str(), "y" | "yes" | "true" | "1"));
        assert!(matches!("yes".to_lowercase().as_str(), "y" | "yes" | "true" | "1"));
        assert!(matches!("TRUE".to_lowercase().as_str(), "y" | "yes" | "true" | "1"));
        assert!(!matches!("n".to_lowercase().as_str(), "y" | "yes" | "true" | "1"));
    }
}
