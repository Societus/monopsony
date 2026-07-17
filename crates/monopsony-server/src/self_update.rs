//! Self-update mechanism for monopsony server via shadow package system.
//!
//! Reuses the same infrastructure as the client binary.

use anyhow::Context;
use monopsony_client::self_update::{check_for_update, apply_update};

/// Run self-update check or apply for the server binary
pub async fn run_self_update(cmd: crate::SelfUpdateMode) -> anyhow::Result<()> {
    let current_version = env!("CARGO_PKG_VERSION").to_string();
    
    match cmd {
        crate::SelfUpdateMode::Check => {
            println!("Current server: {}", current_version);
            
            let result = check_for_update(&current_version).await;
            
            match result {
                Ok(result) => {
                    if let Some(latest) = &result.latest_version {
                        println!("Update available: {} -> {}", result.current_version, latest);
                        if let Some(url) = &result.changelog_url {
                            println!("Changelog: {}", url);
                        }
                        std::process::exit(1); // Exit code 1 indicates update available
                    } else {
                        println!("You are running the latest version of monopsony server.");
                    }
                }
                Err(e) => {
                    eprintln!("Failed to check for updates: {}", e);
                    std::process::exit(2);
                }
            }
        }
        
        crate::SelfUpdateMode::Apply => {
            let result = check_for_update(&current_version).await;
            
            match result {
                Ok(result) => {
                    if result.latest_version.is_none() {
                        println!("Already at latest version. Nothing to do.");
                        return Ok(());
                    }
                    
                    // Check if we have an asset URL for this platform
                    let asset_url = match result.asset_url {
                        Some(url) => url,
                        None => {
                            eprintln!("No compatible release asset found for this platform.");
                            eprintln!("Available at: {}", result.changelog_url.as_deref().unwrap_or("GitHub releases"));
                            std::process::exit(3);
                        }
                    };
                    
                    // Determine current binary path
                    let current_exe = std::env::current_exe()
                        .context("Failed to get current executable path")?;
                    
                    let target_dir = current_exe.parent()
                        .context("Failed to get parent directory of binary")?
                        .to_path_buf();
                    
                    println!("Downloading update from: {}", asset_url);
                    println!("Installing to: {}", current_exe.display());

                    // Try parsing the default recipe for the managed pipeline
                    let recipe = toml::from_str::<monopsony_common::shadow_types::ShadowRecipe>(
                        monopsony_common::shadow_types::DEFAULT_MONOPSONY_RECIPE,
                    ).ok();

                    if recipe.is_some() {
                        let mut ctx = monopsony_client::self_update::UpdateContext::new(
                            recipe,
                            current_exe,
                            target_dir,
                            current_version,
                            result.latest_version.as_deref().unwrap_or("unknown").to_string(),
                            asset_url,
                        );

                        match monopsony_client::self_update::apply_update_managed(&mut ctx).await {
                            Ok(monopsony_client::self_update::UpdateOutcome::Success) => {
                                println!("Update applied successfully! Restart monopsony-server to use the new version.");
                            }
                            Ok(monopsony_client::self_update::UpdateOutcome::RolledBack(reason)) => {
                                eprintln!("Update rolled back: {}", reason);
                                eprintln!("Your system has been restored to the previous version.");
                                std::process::exit(5);
                            }
                            Err(e) => {
                                eprintln!("Failed to apply update: {}", e);
                                std::process::exit(4);
                            }
                        }
                    } else {
                        match apply_update(
                            &current_exe,
                            &target_dir,
                            &asset_url,
                            result.latest_version.as_deref().unwrap_or("unknown"),
                        ).await {
                            Ok(()) => {
                                println!("Update applied successfully! Restart monopsony-server to use the new version.");
                            }
                            Err(e) => {
                                eprintln!("Failed to apply update: {}", e);
                                std::process::exit(4);
                            }
                        }
                    }
                }
                Err(e) => {
                    eprintln!("Failed to check for updates: {}", e);
                    std::process::exit(2);
                }
            }
        }
    }
    
    Ok(())
}