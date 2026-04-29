//! Web search adapters.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use athen_core::error::Result;

pub mod duckduckgo;
pub mod tavily;

/// One ranked search hit. Adapters normalize provider-specific shapes into
/// this common form so the agent sees consistent fields regardless of who
/// answered the query.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

/// Web search backend.
#[async_trait]
pub trait WebSearchProvider: Send + Sync {
    /// Run a query and return up to `max_results` hits.
    async fn search(&self, query: &str, max_results: usize) -> Result<Vec<SearchResult>>;

    /// Stable identifier for logs and tool output (`"duckduckgo"`, `"tavily"`).
    fn name(&self) -> &'static str;
}
