//! Tracker client — publishes recipes and annotations to the tracker.

use anyhow::Result;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublishRecipeRequest {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub author_key_id: String,
    pub operator_key_id: Option<String>,
    pub recipe_toml: String,
    pub author_sig: String,
    pub operator_sig: Option<String>,
    pub source_type: String,
    pub source_url: String,
    #[serde(default)]
    pub architectures: Vec<String>,
    pub install_dir: Option<String>,
    #[serde(default)]
    pub config_tracked: bool,
    pub version: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecipeSearchResult {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub author_key_id: String,
    pub operator_key_id: Option<String>,
    pub source_type: String,
    pub source_url: String,
    pub architectures: Vec<String>,
    pub version: Option<String>,
    pub score: f64,
    pub install_count: i64,
    pub report_count: i64,
    pub quarantined: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecipeDetail {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub author_key_id: String,
    pub operator_key_id: Option<String>,
    pub recipe_toml: String,
    pub author_sig: String,
    pub operator_sig: Option<String>,
    pub source_type: String,
    pub source_url: String,
    pub architectures: Vec<String>,
    pub install_dir: Option<String>,
    pub version: Option<String>,
    pub score: f64,
    pub install_count: i64,
    pub report_count: i64,
    pub quarantined: bool,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateAnnotation {
    pub package: String,
    pub from_version: String,
    pub to_version: String,
    pub severity: String,
    pub message: String,
    #[serde(default)]
    pub url: Option<String>,
    pub author: String,
    pub timestamp: i64,
    pub signature: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoteRequest {
    pub direction: String,
    pub voter_key_id: String,
    pub install_count: Option<i64>,
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// HTTP client for communicating with a Monopsony tracker.
#[derive(Debug, Clone)]
pub struct TrackerClient {
    base_url: String,
    client: Client,
}

impl TrackerClient {
    pub fn new(base_url: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            client: Client::new(),
        }
    }

    /// Publish a signed recipe to the tracker.
    pub async fn publish_recipe(&self, req: &PublishRecipeRequest) -> Result<()> {
        let url = format!("{}/recipes/publish", self.base_url);
        debug!("Publishing recipe {} to {}", req.name, url);

        let resp = self.client.post(&url).json(req).send().await;

        match resp {
            Ok(r) if r.status().is_success() => {
                debug!("Recipe {} published successfully", req.name);
                Ok(())
            }
            Ok(r) => {
                let status = r.status();
                let body = r.text().await.unwrap_or_default();
                warn!("Failed to publish recipe {}: {} {}", req.name, status, body);
                Err(anyhow::anyhow!(
                    "tracker publish failed: {} {}",
                    status,
                    body
                ))
            }
            Err(e) => {
                warn!("Failed to publish recipe {}: {}", req.name, e);
                Err(anyhow::anyhow!("tracker unreachable: {}", e))
            }
        }
    }

    /// Search for recipes by name on the tracker.
    pub async fn search_recipes(&self, query: &str) -> Result<Vec<RecipeSearchResult>> {
        let url = format!("{}/recipes/search?q={}&limit=20", self.base_url, query);
        debug!("Searching recipes: {}", query);

        let resp = self.client.get(&url).send().await;

        match resp {
            Ok(r) if r.status().is_success() => {
                let results: Vec<RecipeSearchResult> = r.json().await?;
                Ok(results)
            }
            Ok(r) => {
                let status = r.status();
                Err(anyhow::anyhow!("search failed: {}", status))
            }
            Err(e) => {
                warn!("Search failed: {}", e);
                Err(anyhow::anyhow!("tracker unreachable: {}", e))
            }
        }
    }

    /// Get recipe details including score and reports.
    pub async fn get_recipe(&self, recipe_id: &str) -> Result<RecipeDetail> {
        let url = format!("{}/recipes/{}", self.base_url, recipe_id);
        debug!("Getting recipe: {}", recipe_id);

        let resp = self.client.get(&url).send().await;

        match resp {
            Ok(r) if r.status().is_success() => Ok(r.json().await?),
            Ok(r) => Err(anyhow::anyhow!("get recipe failed: {}", r.status())),
            Err(e) => Err(anyhow::anyhow!("tracker unreachable: {}", e)),
        }
    }

    /// Submit a vote for a recipe.
    pub async fn vote_recipe(
        &self,
        recipe_id: &str,
        direction: &str,
        voter_key_id: &str,
        install_count: i64,
    ) -> Result<()> {
        let url = format!("{}/recipes/{}/vote", self.base_url, recipe_id);
        let req = VoteRequest {
            direction: direction.to_string(),
            voter_key_id: voter_key_id.to_string(),
            install_count: Some(install_count),
        };

        let resp = self.client.post(&url).json(&req).send().await;

        match resp {
            Ok(r) if r.status().is_success() => Ok(()),
            Ok(r) => Err(anyhow::anyhow!("vote failed: {}", r.status())),
            Err(e) => Err(anyhow::anyhow!("tracker unreachable: {}", e)),
        }
    }

    /// Publish an update annotation to the tracker.
    pub async fn publish_annotation(&self, annotation: &UpdateAnnotation) -> Result<()> {
        // Annotations will be stored on the tracker via a dedicated endpoint
        // For now, we store them as part of the recipe metadata
        let url = format!("{}/annotations/publish", self.base_url);
        debug!(
            "Publishing annotation: {} {} -> {}",
            annotation.package, annotation.from_version, annotation.to_version
        );

        let resp = self.client.post(&url).json(annotation).send().await;

        match resp {
            Ok(r) if r.status().is_success() => Ok(()),
            Ok(r) => {
                let status = r.status();
                let body = r.text().await.unwrap_or_default();
                warn!("Annotation publish failed: {} {}", status, body);
                Err(anyhow::anyhow!(
                    "annotation publish failed: {} {}",
                    status,
                    body
                ))
            }
            Err(e) => {
                warn!("Annotation publish failed: {}", e);
                Err(anyhow::anyhow!("tracker unreachable: {}", e))
            }
        }
    }

    /// Fetch annotations for a package update from the tracker.
    pub async fn fetch_annotations(
        &self,
        package: &str,
        _from: &str,
        _to: &str,
    ) -> Result<Vec<UpdateAnnotation>> {
        let url = format!("{}/annotations/fetch?package={}", self.base_url, package);

        let resp = self.client.get(&url).send().await;

        match resp {
            Ok(r) if r.status().is_success() => {
                let annotations: Vec<UpdateAnnotation> = r.json().await?;
                Ok(annotations)
            }
            Ok(r) => Err(anyhow::anyhow!("fetch annotations failed: {}", r.status())),
            Err(e) => Err(anyhow::anyhow!("tracker unreachable: {}", e)),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tracker_client_construction() {
        let client = TrackerClient::new("http://localhost:3130/");
        assert_eq!(client.base_url, "http://localhost:3130");
    }

    #[test]
    fn test_tracker_client_trailing_slash() {
        let client = TrackerClient::new("http://localhost:3130");
        assert_eq!(client.base_url, "http://localhost:3130");
    }

    #[test]
    fn test_publish_request_serialization() {
        let req = PublishRecipeRequest {
            id: "hash123".into(),
            name: "test-pkg".into(),
            description: Some("A test".into()),
            author_key_id: "key123".into(),
            operator_key_id: Some("op456".into()),
            recipe_toml: "[package]".into(),
            author_sig: "sig123".into(),
            operator_sig: Some("opsig456".into()),
            source_type: "github_release".into(),
            source_url: "https://github.com/example/tool".into(),
            architectures: vec!["linux/x86_64".into()],
            install_dir: Some("/usr/local/bin".into()),
            config_tracked: false,
            version: Some("1.0.0".into()),
        };

        let json = serde_json::to_string(&req).unwrap();
        let parsed: PublishRecipeRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, "hash123");
        assert_eq!(parsed.name, "test-pkg");
        assert_eq!(parsed.architectures.len(), 1);
    }

    #[tokio::test]
    async fn test_search_recipes_unreachable() {
        let client = TrackerClient::new("http://127.0.0.1:1");
        let result = client.search_recipes("test").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_publish_recipe_unreachable() {
        let client = TrackerClient::new("http://127.0.0.1:1");
        let req = PublishRecipeRequest {
            id: "test".into(),
            name: "test".into(),
            description: None,
            author_key_id: "key".into(),
            operator_key_id: None,
            recipe_toml: "".into(),
            author_sig: "sig".into(),
            operator_sig: None,
            source_type: "github_release".into(),
            source_url: "".into(),
            architectures: vec![],
            install_dir: None,
            config_tracked: false,
            version: None,
        };
        let result = client.publish_recipe(&req).await;
        assert!(result.is_err());
    }
}
