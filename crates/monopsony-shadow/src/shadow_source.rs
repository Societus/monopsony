//! GitHub release source polling for shadow packages.

use monopsony_common::shadow_types::ReleaseAsset;
use regex::Regex;
use std::path::PathBuf;
use tracing::debug;

#[derive(Debug)]
pub enum SourceError {
    Http(String),
    RateLimited { retry_after: Option<u64> },
    Parse(String),
    NotFound(String),
}

impl std::fmt::Display for SourceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Http(e) => write!(f, "HTTP error: {e}"),
            Self::RateLimited { retry_after } => {
                write!(f, "rate limited, retry after: {:?}", retry_after)
            }
            Self::Parse(e) => write!(f, "parse error: {e}"),
            Self::NotFound(e) => write!(f, "not found: {e}"),
        }
    }
}

/// Trait for version checking (extensible for git_build and direct_url later)
#[async_trait::async_trait]
pub trait UpdateSource: Send + Sync {
    /// Get the latest version tag matching the recipe's tag_pattern
    async fn latest_version(&self, tag_pattern: &str) -> Result<Option<String>, SourceError>;

    /// Get release assets for a specific tag
    async fn release_assets(&self, tag: &str) -> Result<Vec<ReleaseAsset>, SourceError>;

    /// Build a changelog URL between two versions
    fn changelog_url(&self, from: &str, to: &str) -> Option<String>;
}

pub struct GithubReleaseSource {
    /// "owner/repo" parsed from source_url
    repo: String,
    /// Optional GitHub API token to avoid rate limits
    token: Option<String>,
    /// HTTP client
    client: reqwest::Client,
}

impl GithubReleaseSource {
    pub fn new(source_url: &str, token: Option<String>) -> Result<Self, SourceError> {
        let repo = parse_github_repo(source_url)?;
        Ok(Self {
            repo,
            token,
            client: reqwest::Client::builder()
                .user_agent("monopsony-shadow/0.1")
                .build()
                .map_err(|e| SourceError::Http(e.to_string()))?,
        })
    }

    fn api_url(&self) -> String {
        format!("https://api.github.com/repos/{}", self.repo)
    }

    fn auth_header(&self) -> Option<String> {
        self.token.as_ref().map(|t| format!("Bearer {t}"))
    }
}

fn parse_github_repo(url: &str) -> Result<String, SourceError> {
    // Handle https://github.com/owner/repo and variants
    let url = url.trim_end_matches('/');
    let path = if let Some(path) = url.strip_prefix("https://github.com/") {
        path
    } else if let Some(path) = url.strip_prefix("http://github.com/") {
        path
    } else {
        return Err(SourceError::Parse(format!("not a GitHub URL: {url}")));
    };

    let parts: Vec<&str> = path.splitn(3, '/').collect();
    if parts.len() < 2 {
        return Err(SourceError::Parse(format!(
            "invalid GitHub repo path: {path}"
        )));
    }
    Ok(format!("{}/{}", parts[0], parts[1]))
}

#[async_trait::async_trait]
impl UpdateSource for GithubReleaseSource {
    async fn latest_version(&self, tag_pattern: &str) -> Result<Option<String>, SourceError> {
        let url = format!("{}/releases?per_page=30", self.api_url());
        let mut req = self.client.get(&url);
        if let Some(ref auth) = self.auth_header() {
            req = req.header("Authorization", auth);
        }

        let resp = req
            .send()
            .await
            .map_err(|e| SourceError::Http(e.to_string()))?;

        if resp.status() == reqwest::StatusCode::FORBIDDEN {
            let retry = resp
                .headers()
                .get("Retry-After")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok());
            return Err(SourceError::RateLimited { retry_after: retry });
        }

        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(SourceError::NotFound(self.repo.clone()));
        }

        if !resp.status().is_success() {
            return Err(SourceError::Http(format!(
                "GitHub API returned {}",
                resp.status()
            )));
        }

        let releases: Vec<serde_json::Value> = resp
            .json()
            .await
            .map_err(|e| SourceError::Parse(e.to_string()))?;

        let pat = Regex::new(tag_pattern).map_err(|e| SourceError::Parse(e.to_string()))?;

        for release in &releases {
            // Skip prereleases and drafts
            if release["prerelease"].as_bool() == Some(true)
                || release["draft"].as_bool() == Some(true)
            {
                continue;
            }
            if let Some(tag) = release["tag_name"].as_str() {
                if pat.is_match(tag) {
                    debug!("Latest version for {}: {}", self.repo, tag);
                    return Ok(Some(tag.to_string()));
                }
            }
        }

        Ok(None)
    }

    async fn release_assets(&self, tag: &str) -> Result<Vec<ReleaseAsset>, SourceError> {
        let url = format!("{}/releases/tags/{}", self.api_url(), tag);
        let mut req = self.client.get(&url);
        if let Some(ref auth) = self.auth_header() {
            req = req.header("Authorization", auth);
        }

        let resp = req
            .send()
            .await
            .map_err(|e| SourceError::Http(e.to_string()))?;

        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(SourceError::NotFound(format!("tag {tag} in {}", self.repo)));
        }

        if !resp.status().is_success() {
            return Err(SourceError::Http(format!(
                "GitHub API returned {} for tag {tag}",
                resp.status()
            )));
        }

        let release: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| SourceError::Parse(e.to_string()))?;

        let assets = release["assets"]
            .as_array()
            .ok_or_else(|| SourceError::Parse("no assets array".into()))?;

        let mut result = Vec::new();
        for asset in assets {
            result.push(ReleaseAsset {
                name: asset["name"].as_str().unwrap_or_default().to_string(),
                url: asset["browser_download_url"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
                size: asset["size"].as_u64().unwrap_or(0),
                sha256: None, // GitHub doesn't provide SHA-256 in the API
            });
        }

        debug!("Found {} assets for {}/{}", result.len(), self.repo, tag);
        Ok(result)
    }

    fn changelog_url(&self, from: &str, to: &str) -> Option<String> {
        Some(format!(
            "https://github.com/{}/compare/{}...{}",
            self.repo, from, to
        ))
    }
}

/// Source for git_build packages. Uses a local bare clone (mirror) to list tags and versions.
pub struct GitBuildSource {
    /// Path to the bare clone on the server
    mirror_path: PathBuf,
    /// Optional branch for HEAD tracking (default: main)
    branch: Option<String>,
    /// Source URL (for changelog URL construction if it's a GitHub repo)
    source_url: Option<String>,
}

impl GitBuildSource {
    pub fn new(mirror_path: PathBuf, branch: Option<String>, source_url: Option<String>) -> Self {
        Self {
            mirror_path,
            branch,
            source_url,
        }
    }
}

#[async_trait::async_trait]
impl UpdateSource for GitBuildSource {
    async fn latest_version(&self, tag_pattern: &str) -> Result<Option<String>, SourceError> {
        let pat = Regex::new(tag_pattern).map_err(|e| SourceError::Parse(e.to_string()))?;

        // List tags via git command
        let output = tokio::process::Command::new("git")
            .args(["tag", "-l", "--sort=-creatordate"])
            .current_dir(&self.mirror_path)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .await
            .map_err(|e| SourceError::Http(e.to_string()))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(SourceError::Http(format!("git tag list failed: {stderr}")));
        }

        let tags: Vec<String> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect();

        // Return the first tag matching the pattern (already sorted newest first)
        for tag in &tags {
            if pat.is_match(tag) {
                debug!("GitBuildSource latest version: {}", tag);
                return Ok(Some(tag.clone()));
            }
        }

        // If no tags match and branch is set, return HEAD commit hash
        if let Some(ref branch) = self.branch {
            let output = tokio::process::Command::new("git")
                .args(["rev-parse", &format!("refs/heads/{branch}")])
                .current_dir(&self.mirror_path)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .output()
                .await
                .map_err(|e| SourceError::Http(e.to_string()))?;

            if output.status.success() {
                let hash = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !hash.is_empty() {
                    return Ok(Some(hash));
                }
            }
        }

        Ok(None)
    }

    async fn release_assets(&self, _tag: &str) -> Result<Vec<ReleaseAsset>, SourceError> {
        // For git_build, there are no release assets — the source IS the asset.
        // The client gets the source via worktree extraction.
        Ok(vec![])
    }

    fn changelog_url(&self, from: &str, to: &str) -> Option<String> {
        // If source_url is a GitHub repo, construct compare URL
        if let Some(ref url) = self.source_url {
            if let Ok(repo) = parse_github_repo(url) {
                return Some(format!(
                    "https://github.com/{}/compare/{}...{}",
                    repo, from, to
                ));
            }
        }
        None
    }
}

/// How to discover the latest version from a direct URL source
#[derive(Debug, Clone)]
pub enum VersionDiscovery {
    /// Scrape an HTML page and extract version with regex
    ScrapePage { url: String, pattern: String },
    /// Follow a known URL pattern with version embedded
    UrlPattern { asset_url_template: String },
}

/// Source for direct_url packages (e.g., Tailscale pkgs.tailscale.com)
#[allow(dead_code)]
pub struct DirectUrlSource {
    /// Base URL for version scraping / asset download
    #[allow(dead_code)]
    source_url: String,
    /// How to discover the latest version
    version_discovery: VersionDiscovery,
    client: reqwest::Client,
}

impl DirectUrlSource {
    pub fn new(
        source_url: String,
        version_discovery: VersionDiscovery,
    ) -> Result<Self, SourceError> {
        let client = reqwest::Client::builder()
            .user_agent("monopsony-shadow/0.1")
            .build()
            .map_err(|e| SourceError::Http(e.to_string()))?;
        Ok(Self {
            source_url,
            version_discovery,
            client,
        })
    }
}

#[async_trait::async_trait]
impl UpdateSource for DirectUrlSource {
    async fn latest_version(&self, tag_pattern: &str) -> Result<Option<String>, SourceError> {
        match &self.version_discovery {
            VersionDiscovery::ScrapePage { url, pattern } => {
                let resp = self
                    .client
                    .get(url)
                    .send()
                    .await
                    .map_err(|e| SourceError::Http(e.to_string()))?;

                if !resp.status().is_success() {
                    return Err(SourceError::Http(format!(
                        "scrape page returned {}",
                        resp.status()
                    )));
                }

                let html = resp
                    .text()
                    .await
                    .map_err(|e| SourceError::Http(e.to_string()))?;

                let scrape_re =
                    Regex::new(pattern).map_err(|e| SourceError::Parse(e.to_string()))?;
                let tag_re =
                    Regex::new(tag_pattern).map_err(|e| SourceError::Parse(e.to_string()))?;

                let versions: Vec<String> = scrape_re
                    .captures_iter(&html)
                    .filter_map(|c| c.get(1).map(|m| m.as_str().to_string()))
                    .filter(|v| tag_re.is_match(v))
                    .collect();

                // Return the last match (often the latest on the page)
                Ok(versions.into_iter().last())
            }
            VersionDiscovery::UrlPattern { .. } => {
                // No way to discover versions from URL pattern alone
                Ok(None)
            }
        }
    }

    async fn release_assets(&self, tag: &str) -> Result<Vec<ReleaseAsset>, SourceError> {
        match &self.version_discovery {
            VersionDiscovery::UrlPattern { asset_url_template } => {
                let url = asset_url_template
                    .replace("{version}", tag)
                    .replace("{arch}", "amd64")
                    .replace("{os}", "linux");
                Ok(vec![ReleaseAsset {
                    name: url.split('/').next_back().unwrap_or("asset").to_string(),
                    url: url.clone(),
                    size: 0, // unknown until download
                    sha256: None,
                }])
            }
            VersionDiscovery::ScrapePage { .. } => Ok(vec![]),
        }
    }

    fn changelog_url(&self, _from: &str, _to: &str) -> Option<String> {
        None // No standard changelog for direct URLs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_github_https() {
        assert_eq!(
            parse_github_repo("https://github.com/Sonarr/Sonarr").unwrap(),
            "Sonarr/Sonarr"
        );
    }

    #[test]
    fn parse_github_trailing_slash() {
        assert_eq!(
            parse_github_repo("https://github.com/Sonarr/Sonarr/").unwrap(),
            "Sonarr/Sonarr"
        );
    }

    #[test]
    fn parse_github_extra_path() {
        assert_eq!(
            parse_github_repo("https://github.com/Sonarr/Sonarr/releases").unwrap(),
            "Sonarr/Sonarr"
        );
    }

    #[test]
    fn parse_github_invalid() {
        assert!(parse_github_repo("https://gitlab.com/foo/bar").is_err());
        assert!(parse_github_repo("not-a-url").is_err());
    }

    #[test]
    fn changelog_url_format() {
        let source = GithubReleaseSource::new("https://github.com/Sonarr/Sonarr", None).unwrap();
        assert_eq!(
            source.changelog_url("4.0.6", "4.0.7"),
            Some("https://github.com/Sonarr/Sonarr/compare/4.0.6...4.0.7".into())
        );
    }

    #[test]
    fn git_build_source_changelog_github() {
        let source = GitBuildSource::new(
            PathBuf::from("/tmp/mirror"),
            None,
            Some("https://github.com/example/tool".into()),
        );
        assert_eq!(
            source.changelog_url("v1.0", "v2.0"),
            Some("https://github.com/example/tool/compare/v1.0...v2.0".into())
        );
    }

    #[test]
    fn git_build_source_changelog_non_github() {
        let source = GitBuildSource::new(
            PathBuf::from("/tmp/mirror"),
            None,
            Some("https://gitlab.com/example/tool".into()),
        );
        assert_eq!(source.changelog_url("v1.0", "v2.0"), None);
    }

    #[test]
    fn direct_url_source_new() {
        let source = DirectUrlSource::new(
            "https://pkgs.example.com/stable".into(),
            VersionDiscovery::UrlPattern {
                asset_url_template: "https://pkgs.example.com/stable/tool_{version}_{arch}.tgz"
                    .into(),
            },
        );
        assert!(source.is_ok());
    }

    #[tokio::test]
    async fn direct_url_url_pattern_assets() {
        let source = DirectUrlSource::new(
            "https://pkgs.example.com/stable".into(),
            VersionDiscovery::UrlPattern {
                asset_url_template: "https://pkgs.example.com/stable/tool_{version}_{arch}.tgz"
                    .into(),
            },
        )
        .unwrap();

        let assets = source.release_assets("1.5.0").await.unwrap();
        assert_eq!(assets.len(), 1);
        assert!(assets[0].url.contains("1.5.0"));
        assert!(assets[0].url.contains("amd64"));
        assert_eq!(assets[0].name, "tool_1.5.0_amd64.tgz");
    }

    #[tokio::test]
    async fn direct_url_scrape_page_no_assets() {
        let source = DirectUrlSource::new(
            "https://example.com/versions".into(),
            VersionDiscovery::ScrapePage {
                url: "https://example.com/versions".into(),
                pattern: r#"(\d+\.\d+)"#.into(),
            },
        )
        .unwrap();

        // ScrapePage variant returns no assets (assets must be discovered alongside the version)
        let assets = source.release_assets("1.0").await.unwrap();
        assert!(assets.is_empty());
    }

    #[test]
    fn git_build_no_changelog_without_source_url() {
        let source = GitBuildSource::new(PathBuf::from("/tmp/mirror"), None, None);
        assert_eq!(source.changelog_url("1.0", "2.0"), None);
    }
}
