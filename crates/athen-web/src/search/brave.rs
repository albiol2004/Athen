//! Brave Search API adapter.
//!
//! Free tier: 2k queries/month, 1 query/sec. Generous enough for a personal
//! assistant. Key is supplied by the user in settings.
//!
//! Endpoint: <https://api.search.brave.com/res/v1/web/search>
//! Auth: `X-Subscription-Token` header.

use async_trait::async_trait;

use athen_core::error::{AthenError, Result};

use super::{SearchResult, WebSearchProvider};

const ENDPOINT: &str = "https://api.search.brave.com/res/v1/web/search";

pub struct BraveSearch {
    api_key: String,
    client: reqwest::Client,
}

impl BraveSearch {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            client: crate::default_http_client(),
        }
    }
}

#[async_trait]
impl WebSearchProvider for BraveSearch {
    fn name(&self) -> &'static str {
        "brave"
    }

    async fn search(&self, query: &str, max_results: usize) -> Result<Vec<SearchResult>> {
        let count = max_results.clamp(1, 20);
        let resp = self
            .client
            .get(ENDPOINT)
            .header("X-Subscription-Token", &self.api_key)
            .header("Accept", "application/json")
            .query(&[("q", query), ("count", &count.to_string())])
            .send()
            .await
            .map_err(|e| AthenError::Other(format!("brave request failed: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            // Surface the status code in the error message so the multi-provider
            // wrapper can classify 429 (rate-limit) vs 402/403 (quota/auth).
            let text = resp.text().await.unwrap_or_default();
            return Err(AthenError::Other(format!("brave HTTP {status}: {text}")));
        }

        let parsed: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| AthenError::Other(format!("brave JSON decode failed: {e}")))?;

        let results = parsed
            .get("web")
            .and_then(|w| w.get("results"))
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        Ok(results
            .into_iter()
            .take(max_results)
            .map(|r| SearchResult {
                title: r
                    .get("title")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                url: r
                    .get("url")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                snippet: r
                    .get("description")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
            })
            .collect())
    }
}
