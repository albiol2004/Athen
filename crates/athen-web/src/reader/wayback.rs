//! Wayback Machine adapter — fetch the latest archive.org snapshot.
//!
//! Uses `https://web.archive.org/web/2id_/<url>`. The `id_` modifier strips
//! Wayback's wrapper UI so we get the raw archived HTML. Best last-resort
//! fallback for paywalled, blocked, or dead pages.
//!
//! Internally delegates to [`LocalReader`] for the actual HTTP + HTML →
//! markdown work — Wayback's static archive is well-behaved enough that
//! we don't need JS rendering on top of it.

use async_trait::async_trait;

use athen_core::error::{AthenError, Result};

use super::{local::LocalReader, PageReader, ReadResult};

pub struct WaybackReader {
    inner: LocalReader,
}

impl WaybackReader {
    pub fn new() -> Self {
        Self { inner: LocalReader::new() }
    }
}

impl Default for WaybackReader {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl PageReader for WaybackReader {
    fn name(&self) -> &'static str {
        "wayback"
    }

    async fn fetch(&self, url: &str) -> Result<ReadResult> {
        // The `2id_` modifier asks Wayback for the latest snapshot, raw,
        // without the navigation banner. If no snapshot exists for the URL
        // Wayback returns a 404 page that LocalReader will surface as a
        // non-success HTTP status — bubbled up as Err to the chain.
        let archive_url = format!("https://web.archive.org/web/2id_/{url}");

        let mut result = self.inner.fetch(&archive_url).await.map_err(|e| {
            AthenError::Other(format!("wayback fetch failed for {url}: {e}"))
        })?;

        // Tell the caller this came from the archive — the local reader
        // doesn't know it was wrapped in a Wayback URL.
        result.source = "wayback".to_string();
        result.url = url.to_string();
        Ok(result)
    }
}
