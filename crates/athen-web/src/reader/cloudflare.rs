//! Cloudflare Browser Rendering `/markdown` endpoint adapter.
//!
//! Universal (any URL, full JS render, returns markdown). Requires a
//! Cloudflare account ID + API token with the `Browser Rendering - Edit`
//! permission. Paid: $0.09 per browser-hour at the time of writing.
//!
//! Best fallback when the local reader returns near-empty markdown for
//! JS-heavy SPAs.

use async_trait::async_trait;
use serde_json::json;

use athen_core::error::{AthenError, Result};

use super::{PageReader, ReadResult};

pub struct CloudflareReader {
    account_id: String,
    api_token: String,
    client: reqwest::Client,
}

impl CloudflareReader {
    pub fn new(account_id: impl Into<String>, api_token: impl Into<String>) -> Self {
        Self {
            account_id: account_id.into(),
            api_token: api_token.into(),
            client: crate::default_http_client(),
        }
    }
}

#[async_trait]
impl PageReader for CloudflareReader {
    fn name(&self) -> &'static str {
        "cloudflare"
    }

    async fn fetch(&self, url: &str) -> Result<ReadResult> {
        let endpoint = format!(
            "https://api.cloudflare.com/client/v4/accounts/{}/browser-rendering/markdown",
            self.account_id
        );

        let resp = self
            .client
            .post(&endpoint)
            .bearer_auth(&self.api_token)
            .json(&json!({ "url": url }))
            .send()
            .await
            .map_err(|e| AthenError::Other(format!("cloudflare request failed: {e}")))?;

        let status = resp.status();
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| AthenError::Other(format!("cloudflare JSON decode failed: {e}")))?;

        if !status.is_success() {
            return Err(AthenError::Other(format!(
                "cloudflare HTTP {status}: {body}"
            )));
        }

        // Standard Cloudflare API envelope: { success, errors, result }.
        // The /markdown endpoint puts the markdown string in `result`.
        let success = body.get("success").and_then(|v| v.as_bool()).unwrap_or(false);
        if !success {
            return Err(AthenError::Other(format!(
                "cloudflare reader reported failure: {body}"
            )));
        }
        let markdown = body
            .get("result")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                AthenError::Other(format!(
                    "cloudflare response missing 'result' string: {body}"
                ))
            })?
            .to_string();

        Ok(ReadResult {
            url: url.to_string(),
            title: None,
            content: markdown,
            source: "cloudflare".to_string(),
        })
    }
}
