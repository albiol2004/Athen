//! Layered page reader with auto-fallback for hard-to-scrape sites.
//!
//! Chain (in order):
//! 1. **Local** — fast, no network round-trip beyond the page itself, works
//!    for the static/SSR majority.
//! 2. **Jina** — server-side JS rendering, free no-key tier. Fires when
//!    LocalReader returns suspiciously little content (likely SPA) or when
//!    the original fetch errored.
//! 3. **Wayback** — last resort for paywalls, blocked pages, and dead URLs.
//!    Tried only when the live fetches both produced no usable content.
//!
//! Each link in the chain is optional. The default chain (Local → Jina →
//! Wayback) covers the common "hard" cases without any user configuration.

use async_trait::async_trait;
use std::sync::Arc;

use athen_core::error::{AthenError, Result};

use super::{jina::JinaReader, local::LocalReader, wayback::WaybackReader, PageReader, ReadResult};

/// Below this many characters we treat the result as "definitely broken"
/// (an empty SPA root, a JS-required stub, etc.) and trigger fallback.
/// Genuinely tiny static pages like `example.com` (~190 chars of body)
/// come in above this floor.
const HARD_EMPTY_FLOOR: usize = 150;
/// Between [`HARD_EMPTY_FLOOR`] and this we only fall back when the body
/// also carries an explicit JS-required marker. Avoids retrying small but
/// legitimate pages.
const SOFT_EMPTY_CEILING: usize = 800;

pub struct HybridReader {
    primary: Arc<dyn PageReader>,
    js_fallback: Option<Arc<dyn PageReader>>,
    archive_fallback: Option<Arc<dyn PageReader>>,
}

impl HybridReader {
    /// Default chain: Local → Jina → Wayback. No keys required, no signup.
    pub fn new() -> Self {
        Self {
            primary: Arc::new(LocalReader::new()),
            js_fallback: Some(Arc::new(JinaReader::new())),
            archive_fallback: Some(Arc::new(WaybackReader::new())),
        }
    }

    /// Replace the primary (default `LocalReader`).
    pub fn with_primary(mut self, primary: Arc<dyn PageReader>) -> Self {
        self.primary = primary;
        self
    }

    /// Replace the JS-rendering fallback. Pass `None` to disable.
    pub fn with_js_fallback(mut self, fallback: Option<Arc<dyn PageReader>>) -> Self {
        self.js_fallback = fallback;
        self
    }

    /// Replace the archive fallback. Pass `None` to disable.
    pub fn with_archive_fallback(mut self, fallback: Option<Arc<dyn PageReader>>) -> Self {
        self.archive_fallback = fallback;
        self
    }
}

impl Default for HybridReader {
    fn default() -> Self {
        Self::new()
    }
}

/// Decide whether a result is thin enough to retry. Hard floor catches
/// definitely-broken outputs; the soft band only triggers when we also
/// see explicit JS-required markers. Pages that are simply small but
/// substantive (landing pages, error pages, `example.com`) pass through.
fn looks_empty(result: &ReadResult) -> bool {
    let len = result.content.chars().count();
    if len < HARD_EMPTY_FLOOR {
        return true;
    }
    let lower = result.content.to_ascii_lowercase();
    let has_js_marker = lower.contains("please enable javascript")
        || lower.contains("javascript is required")
        || lower.contains("you need to enable javascript")
        || lower.contains("enable javascript to run");
    if len < SOFT_EMPTY_CEILING && has_js_marker {
        return true;
    }
    has_js_marker && len < 2000
}

#[async_trait]
impl PageReader for HybridReader {
    fn name(&self) -> &'static str {
        "hybrid"
    }

    async fn fetch(&self, url: &str) -> Result<ReadResult> {
        // Primary: cheap local fetch. Both Ok-with-thin-content and Err
        // are signals to try the next tier.
        let primary_result = self.primary.fetch(url).await;
        match &primary_result {
            Ok(r) if !looks_empty(r) => return primary_result,
            Ok(r) => tracing::debug!(
                target: "athen_web::hybrid",
                url,
                primary = self.primary.name(),
                content_chars = r.content.chars().count(),
                "primary returned thin content; trying JS fallback"
            ),
            Err(e) => tracing::debug!(
                target: "athen_web::hybrid",
                url,
                primary = self.primary.name(),
                error = %e,
                "primary failed; trying JS fallback"
            ),
        }

        // JS fallback: SPA-friendly renderer.
        if let Some(js) = &self.js_fallback {
            match js.fetch(url).await {
                Ok(r) if !looks_empty(&r) => return Ok(r),
                Ok(r) => tracing::debug!(
                    target: "athen_web::hybrid",
                    url,
                    js = js.name(),
                    content_chars = r.content.chars().count(),
                    "JS fallback also thin; trying archive"
                ),
                Err(e) => tracing::debug!(
                    target: "athen_web::hybrid",
                    url,
                    js = js.name(),
                    error = %e,
                    "JS fallback failed; trying archive"
                ),
            }
        }

        // Archive fallback: wayback for paywalls / blocked / dead pages.
        if let Some(arch) = &self.archive_fallback {
            match arch.fetch(url).await {
                Ok(r) => return Ok(r),
                Err(e) => tracing::debug!(
                    target: "athen_web::hybrid",
                    url,
                    archive = arch.name(),
                    error = %e,
                    "archive fallback failed"
                ),
            }
        }

        // Nothing worked. Surface the primary's outcome — even if it was
        // thin content, that's still our best signal of what's there.
        primary_result.map_err(|_| {
            AthenError::Other(format!("hybrid reader: all tiers failed for {url}"))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(source: &str, content: &str) -> ReadResult {
        ReadResult {
            url: "https://x".into(),
            title: None,
            content: content.into(),
            source: source.into(),
        }
    }

    #[test]
    fn looks_empty_short_content() {
        assert!(looks_empty(&r("local-html", "tiny")));
    }

    #[test]
    fn looks_empty_js_marker_short() {
        let body = "x".repeat(500);
        let mixed = format!("{body}\nPlease enable JavaScript to view this site.");
        assert!(looks_empty(&r("local-html", &mixed)));
    }

    #[test]
    fn looks_empty_substantive_passes() {
        let body = "x".repeat(2500);
        assert!(!looks_empty(&r("local-html", &body)));
    }

    #[test]
    fn looks_empty_small_static_page_passes() {
        // example.com-sized: ~200 chars of legitimate static content.
        let body = "x".repeat(200);
        assert!(!looks_empty(&r("local-html", &body)));
    }
}
