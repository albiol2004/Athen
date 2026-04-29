//! Bundled, no-key page reader.
//!
//! Strategy:
//! 1. GET with `Accept: text/markdown` — sites on Cloudflare with "Markdown
//!    for Agents" enabled return clean markdown for free, no signup.
//! 2. If the response is HTML, run it through `html2md` to extract a
//!    readable markdown projection.
//! 3. JS-heavy SPAs will return mostly-empty bodies; we surface that
//!    truthfully so the agent can fall back to `web_search`.

use async_trait::async_trait;

use athen_core::error::{AthenError, Result};

use super::{PageReader, ReadResult};

const MAX_BODY_BYTES: usize = 5 * 1024 * 1024;
const MAX_OUTPUT_CHARS: usize = 40_000;

pub struct LocalReader {
    client: reqwest::Client,
}

impl LocalReader {
    pub fn new() -> Self {
        Self { client: crate::default_http_client() }
    }

    pub fn with_client(client: reqwest::Client) -> Self {
        Self { client }
    }
}

impl Default for LocalReader {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl PageReader for LocalReader {
    fn name(&self) -> &'static str {
        "local"
    }

    async fn fetch(&self, url: &str) -> Result<ReadResult> {
        let resp = self
            .client
            .get(url)
            // Cloudflare's edge content negotiation hands back markdown when
            // the origin opts in. Costs us one header on every fetch.
            .header(reqwest::header::ACCEPT, "text/markdown, text/html;q=0.9, */*;q=0.5")
            .send()
            .await
            .map_err(|e| AthenError::Other(format!("fetch_url request failed: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            return Err(AthenError::Other(format!(
                "fetch_url got HTTP {status} for {url}"
            )));
        }

        let final_url = resp.url().to_string();
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();

        // Read body with a hard cap so a hostile or huge page can't blow
        // memory. We don't stream-truncate mid-UTF8 — `text()` decodes
        // first; the cap is enforced after.
        let body = resp
            .text()
            .await
            .map_err(|e| AthenError::Other(format!("fetch_url body read failed: {e}")))?;
        let body = if body.len() > MAX_BODY_BYTES {
            body[..MAX_BODY_BYTES].to_string()
        } else {
            body
        };

        let (markdown, source, title) = if content_type.contains("text/markdown") {
            (body, "local-markdown", None)
        } else if content_type.contains("text/html") || content_type.is_empty() {
            let title = extract_title(&body);
            // html2md happily inlines `<script>` and `<style>` bodies as
            // text — strip them first so the agent isn't fed a wall of CSS.
            let cleaned = strip_noise(&body);
            let md = html2md::parse_html(&cleaned);
            (md, "local-html", title)
        } else if content_type.starts_with("text/") {
            (body, "local-text", None)
        } else {
            return Err(AthenError::Other(format!(
                "fetch_url: unsupported content-type '{content_type}' for {url}"
            )));
        };

        let trimmed = truncate_chars(&markdown, MAX_OUTPUT_CHARS);

        Ok(ReadResult {
            url: final_url,
            title,
            content: trimmed,
            source: source.to_string(),
        })
    }
}

/// Remove `<script>...</script>` and `<style>...</style>` blocks before
/// markdown conversion. Case-insensitive, tolerant of attributes on the
/// open tag. Imperfect on pathological/malformed HTML — that's fine, we
/// only need to filter out the common noise sources.
fn strip_noise(html: &str) -> String {
    fn strip_tag(input: &str, tag: &str) -> String {
        let lower = input.to_ascii_lowercase();
        let open = format!("<{tag}");
        let close = format!("</{tag}>");
        let mut out = String::with_capacity(input.len());
        let mut cursor = 0;
        while let Some(start) = lower[cursor..].find(&open) {
            let abs_start = cursor + start;
            out.push_str(&input[cursor..abs_start]);
            // Advance past the open tag's `>`.
            let after_open = match input[abs_start..].find('>') {
                Some(p) => abs_start + p + 1,
                None => break,
            };
            // Find the matching close tag.
            let after_close = match lower[after_open..].find(&close) {
                Some(p) => after_open + p + close.len(),
                None => break,
            };
            cursor = after_close;
        }
        out.push_str(&input[cursor..]);
        out
    }
    strip_tag(&strip_tag(html, "script"), "style")
}

/// Pull `<title>` out of an HTML doc with a tiny regex-free scan. Cheap and
/// good enough for the common case (a single `<title>...</title>` in head).
fn extract_title(html: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let start = lower.find("<title")?;
    let after_open = html[start..].find('>')? + start + 1;
    let close = lower[after_open..].find("</title>")? + after_open;
    let title = html[after_open..close].trim();
    if title.is_empty() { None } else { Some(title.to_string()) }
}

/// UTF-8 safe character truncation. Cuts at the char boundary closest to
/// `max_chars` so we never split a multibyte sequence.
fn truncate_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let truncated: String = s.chars().take(max_chars).collect();
    format!("{truncated}\n\n[... truncated, original was longer than {max_chars} chars ...]")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_title_basic() {
        let html = "<html><head><title>Hello World</title></head><body>x</body></html>";
        assert_eq!(extract_title(html).as_deref(), Some("Hello World"));
    }

    #[test]
    fn extracts_title_with_attrs() {
        let html = "<html><head><title id=\"t\">Tagged</title></head></html>";
        assert_eq!(extract_title(html).as_deref(), Some("Tagged"));
    }

    #[test]
    fn no_title_returns_none() {
        let html = "<html><body>no title</body></html>";
        assert_eq!(extract_title(html), None);
    }

    #[test]
    fn truncate_preserves_short() {
        assert_eq!(truncate_chars("abc", 10), "abc");
    }

    #[test]
    fn strip_noise_removes_script_and_style() {
        let html = "<html><head><style>body{color:red}</style></head>\
                    <body><script>alert(1)</script><p>hi</p></body></html>";
        let s = strip_noise(html);
        assert!(!s.contains("color:red"));
        assert!(!s.contains("alert(1)"));
        assert!(s.contains("<p>hi</p>"));
    }

    #[test]
    fn strip_noise_handles_attrs_on_open_tag() {
        let html = "<style type=\"text/css\">p{}</style><p>x</p>";
        let s = strip_noise(html);
        assert!(!s.contains("p{}"));
        assert!(s.contains("<p>x</p>"));
    }

    #[test]
    fn truncate_cuts_long() {
        let s: String = "a".repeat(100);
        let t = truncate_chars(&s, 10);
        assert!(t.starts_with("aaaaaaaaaa"));
        assert!(t.contains("truncated"));
    }
}
