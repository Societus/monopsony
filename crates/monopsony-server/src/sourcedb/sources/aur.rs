//! AUR RPC source adapter (Sprint 21 slice 21-3a).
//!
//! Talks to the AUR RPC v5 interface (`/rpc/v5/search?arg=...&by=name`)
//! and maps responses into [`ResolvedMapping`] rows for the sourcedb
//! resolution pipeline.

/// AUR RPC v5 source adapter.
#[derive(Debug, Clone)]
pub struct AurSource {
    base_url: String,
}

impl Default for AurSource {
    fn default() -> Self {
        Self::new()
    }
}

impl AurSource {
    pub fn new() -> Self {
        Self {
            base_url: "https://aur.archlinux.org/rpc/v5".into(),
        }
    }

    /// Override the RPC endpoint (tests point this at a local stub).
    pub fn set_base_url(&mut self, url: String) {
        self.base_url = url;
    }

    /// Percent-encode a query component: unreserved chars pass through,
    /// everything else becomes `%XX` (space as `%20`, not `+`).
    fn encode_component(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        for byte in s.bytes() {
            match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    out.push(byte as char)
                }
                _ => out.push_str(&format!("%{:02X}", byte)),
            }
        }
        out
    }

    /// Build the search URL for one package name.
    pub fn build_query_url(&self, name: &str) -> String {
        format!(
            "{}/search?arg={}&by=name",
            self.base_url,
            Self::encode_component(name)
        )
    }

    /// Parse an AUR RPC v5 search response body into resolved mappings.
    ///
    /// Malformed JSON is an Err; `resultcount: 0` (or an empty results
    /// array) yields an empty Vec — no results is a state, not an error.
    pub fn parse_rpc_response(
        &self,
        body: &str,
    ) -> Result<Vec<crate::sourcedb::sources::ResolvedMapping>, String> {
        let value: serde_json::Value = serde_json::from_str(body).map_err(|e| e.to_string())?;
        let results = match value.get("results") {
            Some(r) => r,
            None => return Err("missing 'results' field".to_string()),
        };
        let rows = results.as_array().ok_or("'results' is not an array")?;
        Ok(rows
            .iter()
            .filter_map(|row| {
                let name = row.get("Name").and_then(|v| v.as_str())?;
                let url = row
                    .get("URL")
                    .and_then(|v| v.as_str())
                    .filter(|u| !u.is_empty());
                Some(crate::sourcedb::sources::ResolvedMapping {
                    source: "aur".into(),
                    origin: Some(name.to_string()),
                    homepage: url.map(|u| u.to_string()),
                })
            })
            .collect())
    }

    /// Resolve one AUR package name via the RPC search endpoint.
    pub async fn resolve(
        &self,
        client: &reqwest::Client,
        name: &str,
    ) -> Result<Vec<crate::sourcedb::sources::ResolvedMapping>, String> {
        let url = self.build_query_url(name);
        let body = client
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("aur request failed: {}", e))?
            .text()
            .await
            .map_err(|e| format!("aur body read failed: {}", e))?;
        self.parse_rpc_response(&body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Golden parse: one well-formed result maps to a ResolvedMapping.
    #[test]
    fn test_parse_golden_single_result() {
        let src = AurSource::new();
        let body = "{\"resultcount\":1,\"results\":[{\"Name\":\"ripgrep\",\"Version\":\"14.1.0\",\"URL\":\"https://github.com/BurntSushi/ripgrep\"}]}";
        let mappings = src.parse_rpc_response(body).unwrap();
        assert_eq!(mappings.len(), 1);
        assert_eq!(mappings[0].source, "aur");
        assert_eq!(mappings[0].origin, Some("ripgrep".to_string()));
        assert_eq!(
            mappings[0].homepage,
            Some("https://github.com/BurntSushi/ripgrep".to_string())
        );
    }

    // Empty URL string maps to homepage None.
    #[test]
    fn test_parse_empty_url_is_none() {
        let src = AurSource::new();
        let body = "{\"resultcount\":1,\"results\":[{\"Name\":\"foo\",\"Version\":\"1.0\",\"URL\":\"\"}]}";
        let mappings = src.parse_rpc_response(body).unwrap();
        assert_eq!(mappings.len(), 1);
        assert_eq!(mappings[0].homepage, None);
        assert_eq!(mappings[0].origin, Some("foo".to_string()));
    }

    // Malformed JSON is an Err, never a panic.
    #[test]
    fn test_parse_malformed_json_is_err() {
        let src = AurSource::new();
        assert!(src.parse_rpc_response("{not json").is_err());
        assert!(src.parse_rpc_response("").is_err());
    }

    // resultcount 0 -> empty Vec (no results is a state).
    #[test]
    fn test_parse_zero_results_is_empty_vec() {
        let src = AurSource::new();
        let body = "{\"resultcount\":0,\"results\":[]}";
        let mappings = src.parse_rpc_response(body).unwrap();
        assert!(mappings.is_empty());
    }

    // Query URL golden: default base, plain name, by=name.
    #[test]
    fn test_query_url_golden() {
        let src = AurSource::new();
        assert_eq!(
            src.build_query_url("ripgrep"),
            "https://aur.archlinux.org/rpc/v5/search?arg=ripgrep&by=name"
        );
    }

    // set_base_url swaps the endpoint; special chars are percent-encoded.
    #[test]
    fn test_set_base_url_and_encoding() {
        let mut src = AurSource::new();
        src.set_base_url("http://127.0.0.1:1/rpc/v5".to_string());
        assert_eq!(
            src.build_query_url("ripgrep"),
            "http://127.0.0.1:1/rpc/v5/search?arg=ripgrep&by=name"
        );
        assert_eq!(
            src.build_query_url("python foo/bar+baz"),
            format!(
                "{}/search?arg=python%20foo%2Fbar%2Bbaz&by=name",
                src.base_url
            )
        );
    }
}
