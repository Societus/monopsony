//! GitHub releases-feed source adapter (Sprint 21 slice 21-3b).
//!
//! URL parsing and release-JSON parsing for
//! `{api_base}/repos/{owner}/{repo}/releases/latest`.
//! Offline-testable: no network in parsing.

use crate::sourcedb::sources::ResolvedMapping;

/// Adapter for the GitHub releases API.
pub struct GithubSource {
    api_base: String,
    token: Option<String>,
}

impl GithubSource {
    pub fn new() -> Self {
        Self {
            api_base: "https://api.github.com".into(),
            token: None,
        }
    }

    /// Set (or clear) the bearer token used for API requests.
    pub fn set_token(&mut self, t: Option<String>) {
        self.token = t;
    }

    /// Extract `(owner, repo)` from a GitHub URL.
    ///
    /// Accepts `github.com/OWNER/REPO[.git][/...]` (http/https) and
    /// `git@github.com:OWNER/REPO.git` (SSH). Returns `None` otherwise.
    pub fn parse_owner_repo(&self, url: &str) -> Option<(String, String)> {
        let rest = if let Some(idx) = url.find("github.com/") {
            &url[idx + "github.com/".len()..]
        } else if let Some(idx) = url.find("github.com:") {
            &url[idx + "github.com:".len()..]
        } else {
            return None;
        };

        let mut segs = rest.split('/').filter(|s| !s.is_empty());
        let owner = segs.next()?;
        let repo = segs.next()?;
        if owner.is_empty() || repo.is_empty() {
            return None;
        }
        let repo = repo.strip_suffix(".git").unwrap_or(repo);
        Some((owner.to_string(), repo.to_string()))
    }

    /// Build the latest-releases API URL for a repo.
    pub fn build_releases_url(&self, owner: &str, repo: &str) -> String {
        format!("{}/repos/{}/{}/releases/latest", self.api_base, owner, repo)
    }

    /// Parse the `releases/latest` JSON body: `tag_name` becomes `origin`.
    /// The homepage is not carried by this response (`None`); identify the
    /// mapping with [`GithubSource::mapping_source`].
    pub fn parse_release(&self, body: &str) -> Result<ResolvedMapping, String> {
        let v: serde_json::Value =
            serde_json::from_str(body).map_err(|e| format!("invalid GitHub release JSON: {e}"))?;
        let tag_name = v
            .get("tag_name")
            .and_then(|t| t.as_str())
            .ok_or_else(|| "missing tag_name in GitHub release JSON".to_string())?;
        Ok(ResolvedMapping {
            source: "github".into(),
            origin: Some(tag_name.to_string()),
            homepage: None,
        })
    }

    /// Observation source id for a resolved `owner/repo`.
    pub fn mapping_source(owner: &str, repo: &str) -> String {
        format!("github:{owner}/{repo}")
    }

    /// Case-insensitive lookup of an `ETag` header value.
    pub fn etag_from_headers(&self, headers: &[(String, String)]) -> Option<String> {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("etag"))
            .map(|(_, v)| v.clone())
    }
}

impl Default for GithubSource {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_owner_repo_https_git_suffix() {
        assert_eq!(
            GithubSource::new()
                .parse_owner_repo("https://github.com/BurntSushi/ripgrep.git")
                .unwrap(),
            ("BurntSushi".to_string(), "ripgrep".to_string())
        );
    }

    #[test]
    fn parse_owner_repo_ssh_form() {
        assert_eq!(
            GithubSource::new()
                .parse_owner_repo("git@github.com:foo/bar.git")
                .unwrap(),
            ("foo".to_string(), "bar".to_string())
        );
    }

    #[test]
    fn parse_owner_repo_non_github_is_none() {
        assert_eq!(
            GithubSource::new().parse_owner_repo("https://gitlab.com/x/y"),
            None
        );
    }

    #[test]
    fn parse_owner_repo_trailing_path_ignored() {
        assert_eq!(
            GithubSource::new()
                .parse_owner_repo("https://github.com/foo/bar/releases/tag/v1.2.3")
                .unwrap(),
            ("foo".to_string(), "bar".to_string())
        );
    }

    #[test]
    fn build_releases_url_golden() {
        assert_eq!(
            GithubSource::new().build_releases_url("BurntSushi", "ripgrep"),
            "https://api.github.com/repos/BurntSushi/ripgrep/releases/latest"
        );
    }

    #[test]
    fn parse_release_golden_tag_name() {
        let m = GithubSource::new()
            .parse_release(r#"{"tag_name":"v14.1.1"}"#)
            .unwrap();
        assert_eq!(m.origin.as_deref(), Some("v14.1.1"));
        assert_eq!(m.homepage, None);
    }

    #[test]
    fn parse_release_missing_tag_is_err() {
        assert!(GithubSource::new()
            .parse_release(r#"{"message":"Not Found"}"#)
            .is_err());
    }

    #[test]
    fn mapping_source_golden() {
        assert_eq!(
            GithubSource::mapping_source("BurntSushi", "ripgrep"),
            "github:BurntSushi/ripgrep"
        );
    }

    #[test]
    fn etag_found_case_insensitive() {
        let headers = vec![
            ("Content-Type".to_string(), "application/json".to_string()),
            ("ETag".to_string(), "\"abc\"".to_string()),
        ];
        assert_eq!(
            GithubSource::new().etag_from_headers(&headers),
            Some("\"abc\"".to_string())
        );
    }

    #[test]
    fn etag_missing_is_none() {
        let headers = vec![("X-Cache".to_string(), "HIT".to_string())];
        assert_eq!(GithubSource::new().etag_from_headers(&headers), None);
    }
}
