//! Jina Reader adapter — `https://r.jina.ai/<url>`.
//!
//! Server-side JS rendering, returns clean markdown. Free 500 req/min with
//! no signup; pass an API key for higher quotas. Excellent fallback when
//! the local reader returns empty/short content for SPAs.

use async_trait::async_trait;

use athen_core::error::{AthenError, Result};

use super::{PageReader, ReadResult};

const ENDPOINT: &str = "https://r.jina.ai/";

pub struct JinaReader {
    api_key: Option<String>,
    client: reqwest::Client,
}

impl JinaReader {
    /// Anonymous reader. Free tier (~500 req/min, IP-rate-limited).
    pub fn new() -> Self {
        Self {
            api_key: None,
            client: crate::default_http_client(),
        }
    }

    /// Authenticated reader. Higher rate limit, paid plans available.
    pub fn with_api_key(api_key: impl Into<String>) -> Self {
        Self {
            api_key: Some(api_key.into()),
            client: crate::default_http_client(),
        }
    }
}

impl Default for JinaReader {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl PageReader for JinaReader {
    fn name(&self) -> &'static str {
        "jina"
    }

    async fn fetch(&self, url: &str) -> Result<ReadResult> {
        // Jina expects the target URL appended directly to r.jina.ai/.
        // Don't pre-encode — they accept the raw scheme + host.
        let endpoint = format!("{ENDPOINT}{url}");

        let mut req = self
            .client
            .get(&endpoint)
            // Ask for markdown explicitly; Jina also supports JSON / HTML.
            .header("Accept", "text/markdown")
            // Request fewer engine artefacts in the body. Per Jina docs.
            .header("X-Return-Format", "markdown");
        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }

        let resp = req
            .send()
            .await
            .map_err(|e| AthenError::Other(format!("jina request failed: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(AthenError::Other(format!("jina HTTP {status}: {text}")));
        }

        let body = resp
            .text()
            .await
            .map_err(|e| AthenError::Other(format!("jina body read failed: {e}")))?;

        // Jina prepends a small header block (Title:, URL Source:, ...) ahead
        // of the markdown body. Pull the title out of it if present, then
        // strip the block so the agent doesn't see boilerplate.
        let (title, content) = split_jina_header(&body);

        Ok(ReadResult {
            url: url.to_string(),
            title,
            content,
            source: "jina".to_string(),
        })
    }
}

/// Jina's response begins with a header like:
///
/// ```text
/// Title: Page title here
/// URL Source: https://example.com/
/// Markdown Content:
/// <actual markdown>
/// ```
///
/// Pull the title and return the markdown body. If we don't see the marker
/// we fall back to returning the body unchanged so we never silently drop
/// content.
fn split_jina_header(body: &str) -> (Option<String>, String) {
    let mut title: Option<String> = None;
    if let Some(idx) = body.find("Markdown Content:") {
        // Scan the prelude for the Title: line.
        for line in body[..idx].lines() {
            if let Some(t) = line.strip_prefix("Title:") {
                title = Some(t.trim().to_string());
                break;
            }
        }
        let after = &body[idx + "Markdown Content:".len()..];
        return (title, after.trim_start_matches('\n').to_string());
    }
    (None, body.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_jina_header() {
        let body = "Title: Hello World\nURL Source: https://x\nMarkdown Content:\n# Hello\n\nbody text";
        let (title, content) = split_jina_header(body);
        assert_eq!(title.as_deref(), Some("Hello World"));
        assert!(content.starts_with("# Hello"));
        assert!(!content.contains("URL Source"));
    }

    #[test]
    fn passes_through_unmarked_body() {
        let body = "just markdown here, no header";
        let (title, content) = split_jina_header(body);
        assert!(title.is_none());
        assert_eq!(content, body);
    }
}
