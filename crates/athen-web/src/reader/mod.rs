//! Page-reader adapters: URL → clean markdown.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use athen_core::error::Result;

pub mod cloudflare;
pub mod hybrid;
pub mod jina;
pub mod local;
pub mod wayback;

/// Cleaned page content ready for the LLM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadResult {
    pub url: String,
    pub title: Option<String>,
    /// Markdown body. May be empty for very heavy SPAs that the local reader
    /// couldn't make sense of — agents should detect that and fall back to
    /// `web_search` for snippets.
    pub content: String,
    /// Identifier of the reader that produced this content
    /// (`"local-markdown"`, `"local-html"`, `"cloudflare"`).
    pub source: String,
}

/// URL → readable markdown.
#[async_trait]
pub trait PageReader: Send + Sync {
    async fn fetch(&self, url: &str) -> Result<ReadResult>;
    fn name(&self) -> &'static str;
}
