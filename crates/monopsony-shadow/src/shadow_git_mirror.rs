//! Git mirror maintenance for shadow packages with git_build source type.

use std::path::{Path, PathBuf};

use tokio::process::Command;
use tracing::{debug, info};

#[derive(Debug)]
pub enum MirrorError {
    GitCommandFailed { exit_code: i32, stderr: String },
    Io(std::io::Error),
    CloneFailed(String),
    RefNotFound(String),
}

impl std::fmt::Display for MirrorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::GitCommandFailed { exit_code, stderr } => {
                write!(f, "git command failed (exit {exit_code}): {stderr}")
            }
            Self::Io(e) => write!(f, "IO error: {e}"),
            Self::CloneFailed(e) => write!(f, "clone failed: {e}"),
            Self::RefNotFound(e) => write!(f, "ref not found: {e}"),
        }
    }
}

impl std::error::Error for MirrorError {}

impl From<std::io::Error> for MirrorError {
    fn from(e: std::io::Error) -> Self {
        MirrorError::Io(e)
    }
}

pub struct GitMirrorManager {
    /// Base directory for git mirrors: /var/cache/monopsony/shadow/git-mirrors/
    mirror_dir: PathBuf,
}

impl GitMirrorManager {
    pub fn new(mirror_dir: PathBuf) -> Self {
        Self { mirror_dir }
    }

    /// Get or create a bare clone of a git repo.
    /// If the mirror already exists, fetch latest.
    /// Returns the path to the bare clone directory.
    pub async fn get_or_clone(&self, url: &str, name: &str) -> Result<PathBuf, MirrorError> {
        let repo_path = self.mirror_dir.join(name);

        if repo_path.join("HEAD").exists() {
            debug!("Mirror exists for {}, fetching", name);
            self.fetch(&repo_path).await?;
            Ok(repo_path)
        } else {
            tokio::fs::create_dir_all(&self.mirror_dir).await?;

            info!("Cloning mirror for {} from {}", name, url);
            let output = Command::new("git")
                .args(["clone", "--bare", url, &repo_path.to_string_lossy()])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .output()
                .await?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                // Clean up failed clone
                let _ = tokio::fs::remove_dir_all(&repo_path).await;
                return Err(MirrorError::CloneFailed(stderr.to_string()));
            }

            info!("Mirror cloned: {}", repo_path.display());
            Ok(repo_path)
        }
    }

    /// Fetch latest changes for an existing mirror
    pub async fn fetch(&self, repo_path: &Path) -> Result<(), MirrorError> {
        let output = Command::new("git")
            .args(["fetch", "--prune"])
            .current_dir(repo_path)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(MirrorError::GitCommandFailed {
                exit_code: output.status.code().unwrap_or(-1),
                stderr: stderr.to_string(),
            });
        }

        debug!("Fetched mirror at {}", repo_path.display());
        Ok(())
    }

    /// Get the latest commit hash for a branch or tag
    pub async fn latest_commit(
        &self,
        repo_path: &Path,
        refspec: Option<&str>,
    ) -> Result<String, MirrorError> {
        let ref_arg = refspec.unwrap_or("HEAD");
        let output = Command::new("git")
            .args(["rev-parse", ref_arg])
            .current_dir(repo_path)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(MirrorError::RefNotFound(format!("{}: {}", ref_arg, stderr)));
        }

        let hash = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Ok(hash)
    }

    /// List all tags in a mirror, sorted by creation date (newest first)
    pub async fn list_tags(&self, repo_path: &Path) -> Result<Vec<String>, MirrorError> {
        let output = Command::new("git")
            .args(["tag", "-l", "--sort=-creatordate"])
            .current_dir(repo_path)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(MirrorError::GitCommandFailed {
                exit_code: output.status.code().unwrap_or(-1),
                stderr: stderr.to_string(),
            });
        }

        let tags: Vec<String> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect();

        Ok(tags)
    }

    /// Get the commit hash for a specific tag
    pub async fn tag_commit(&self, repo_path: &Path, tag: &str) -> Result<String, MirrorError> {
        let output = Command::new("git")
            .args(["rev-list", "-1", tag])
            .current_dir(repo_path)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(MirrorError::RefNotFound(format!("tag {tag}: {stderr}")));
        }

        let hash = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Ok(hash)
    }

    /// Create a working tree from a bare clone at a specific ref.
    /// This creates a copy of the source files for the client to build from.
    pub async fn create_worktree(
        &self,
        repo_path: &Path,
        refspec: &str,
        dest: &Path,
    ) -> Result<(), MirrorError> {
        // Clean dest if it exists
        if dest.exists() {
            tokio::fs::remove_dir_all(dest).await?;
        }

        // Use git archive to extract files at the given ref
        let output = Command::new("git")
            .args(["archive", refspec])
            .current_dir(repo_path)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(MirrorError::GitCommandFailed {
                exit_code: output.status.code().unwrap_or(-1),
                stderr: stderr.to_string(),
            });
        }

        // Extract the archive to dest
        tokio::fs::create_dir_all(dest).await?;

        let extract = Command::new("tar")
            .args(["x"])
            .current_dir(dest)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;

        if let Some(mut stdin) = extract.stdin {
            use tokio::io::AsyncWriteExt;
            stdin.write_all(&output.stdout).await?;
        }

        debug!("Created worktree at {} for ref {}", dest.display(), refspec);
        Ok(())
    }

    /// Garbage collect mirrors not referenced by any recipe.
    /// Returns the number of mirrors removed.
    pub async fn gc(&self, active_repos: &[String]) -> Result<usize, MirrorError> {
        let mut removed = 0;
        let mut entries = tokio::fs::read_dir(&self.mirror_dir).await?;

        while let Some(entry) = entries.next_entry().await? {
            let name = entry.file_name().to_string_lossy().to_string();
            if entry.file_type().await?.is_dir() && !active_repos.contains(&name) {
                info!("GC: removing unreferenced mirror: {}", name);
                tokio::fs::remove_dir_all(entry.path()).await?;
                removed += 1;
            }
        }

        if removed > 0 {
            info!("GC: removed {} unreferenced mirrors", removed);
        }
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn gc_removes_unreferenced() {
        let tmp = tempfile::tempdir().unwrap();
        let mirror_dir = tmp.path().join("mirrors");
        tokio::fs::create_dir_all(&mirror_dir).await.unwrap();

        // Create two fake mirror dirs
        tokio::fs::create_dir_all(mirror_dir.join("active-repo"))
            .await
            .unwrap();
        tokio::fs::create_dir_all(mirror_dir.join("stale-repo"))
            .await
            .unwrap();

        let mgr = GitMirrorManager::new(mirror_dir);
        let removed = mgr.gc(&["active-repo".to_string()]).await.unwrap();
        assert_eq!(removed, 1);

        // active-repo still exists, stale-repo removed
        assert!(mgr.mirror_dir.join("active-repo").exists());
        assert!(!mgr.mirror_dir.join("stale-repo").exists());
    }

    #[tokio::test]
    async fn gc_keeps_all_when_active() {
        let tmp = tempfile::tempdir().unwrap();
        let mirror_dir = tmp.path().join("mirrors");
        tokio::fs::create_dir_all(&mirror_dir).await.unwrap();

        tokio::fs::create_dir_all(mirror_dir.join("repo-a"))
            .await
            .unwrap();
        tokio::fs::create_dir_all(mirror_dir.join("repo-b"))
            .await
            .unwrap();

        let mgr = GitMirrorManager::new(mirror_dir);
        let removed = mgr
            .gc(&["repo-a".to_string(), "repo-b".to_string()])
            .await
            .unwrap();
        assert_eq!(removed, 0);
    }

    #[tokio::test]
    async fn mirror_error_display() {
        let e = MirrorError::RefNotFound("v1.0".into());
        assert!(e.to_string().contains("v1.0"));

        let e = MirrorError::GitCommandFailed {
            exit_code: 128,
            stderr: "fatal: not a git repo".into(),
        };
        assert!(e.to_string().contains("128"));
    }

    #[test]
    fn new_manager() {
        let mgr = GitMirrorManager::new(PathBuf::from("/tmp/test-mirrors"));
        assert_eq!(mgr.mirror_dir, PathBuf::from("/tmp/test-mirrors"));
    }
}
