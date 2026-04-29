//! Tavily Search adapter — paid/freemium API designed for AI agents.
//!
//! Free tier: ~1k requests/month, no card required. Returns answer-ready
//! context (richer than raw SERPs). User supplies the key in settings.

use async_trait::async_trait;
use serde_json::json;

use athen_core::error::{AthenError, Result};

use super::{SearchResult, WebSearchProvider};

const ENDPOINT: &str = "https://api.tavily.com/search";

pub struct TavilySearch {
    api_key: String,
    client: reqwest::Client,
}

impl TavilySearch {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            client: crate::default_http_client(),
        }
    }
}

#[async_trait]
impl WebSearchProvider for TavilySearch {
    fn name(&self) -> &'static str {
        "tavily"
    }

    async fn search(&self, query: &str, max_results: usize) -> Result<Vec<SearchResult>> {
        let body = json!({
            "api_key": self.api_key,
            "query": query,
            "max_results": max_results,
            "search_depth": "basic",
        });

        let resp = self
            .client
            .post(ENDPOINT)
            .json(&body)
            .send()
            .await
            .map_err(|e| AthenError::Other(format!("tavily request failed: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(AthenError::Other(format!("tavily HTTP {status}: {text}")));
        }

        let parsed: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| AthenError::Other(format!("tavily JSON decode failed: {e}")))?;

        let results = parsed
            .get("results")
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
                    .get("content")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
            })
            .collect())
    }
}
