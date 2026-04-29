//! No-key DuckDuckGo search via the HTML SERP endpoint.
//!
//! POSTs to `https://html.duckduckgo.com/html/`, parses the result list with
//! CSS selectors. No API key, no signup. Bundled default so Athen ships with
//! working web search out of the box.
//!
//! Tradeoffs vs an API: title/snippet only (no answer synthesis), occasional
//! HTTP 202 rate limits on bursts, and DDG owns the markup — if they restyle
//! the SERP we ship a parser fix. Acceptable for low-volume agent use.

use async_trait::async_trait;
use scraper::{Html, Selector};
use std::sync::OnceLock;

use athen_core::error::{AthenError, Result};

use super::{SearchResult, WebSearchProvider};

const ENDPOINT: &str = "https://html.duckduckgo.com/html/";

pub struct DuckDuckGoSearch {
    client: reqwest::Client,
}

impl DuckDuckGoSearch {
    pub fn new() -> Self {
        Self { client: crate::default_http_client() }
    }

    pub fn with_client(client: reqwest::Client) -> Self {
        Self { client }
    }
}

impl Default for DuckDuckGoSearch {
    fn default() -> Self {
        Self::new()
    }
}

// CSS selectors compiled once. scraper's Selector isn't Send, hence the
// thread-local-style `OnceLock` of the raw selector strings rather than the
// parsed selectors themselves — we rebuild them per call (cheap).
fn result_selector() -> &'static str {
    static S: OnceLock<&'static str> = OnceLock::new();
    S.get_or_init(|| "div.result, div.web-result")
}

#[async_trait]
impl WebSearchProvider for DuckDuckGoSearch {
    fn name(&self) -> &'static str {
        "duckduckgo"
    }

    async fn search(&self, query: &str, max_results: usize) -> Result<Vec<SearchResult>> {
        let resp = self
            .client
            .post(ENDPOINT)
            .form(&[("q", query)])
            .send()
            .await
            .map_err(|e| AthenError::Other(format!("duckduckgo request failed: {e}")))?;

        let status = resp.status();
        if status == reqwest::StatusCode::ACCEPTED {
            // DDG returns 202 when it wants you to slow down.
            return Err(AthenError::Other(
                "duckduckgo rate-limited (HTTP 202); back off and retry".into(),
            ));
        }
        if !status.is_success() {
            return Err(AthenError::Other(format!(
                "duckduckgo returned HTTP {status}"
            )));
        }

        let body = resp
            .text()
            .await
            .map_err(|e| AthenError::Other(format!("duckduckgo body read failed: {e}")))?;

        Ok(parse_serp(&body, max_results))
    }
}

/// Parse DDG's HTML SERP into [`SearchResult`]s. Tolerant of layout drift:
/// missing fields fall back to empty strings rather than failing the whole
/// query.
fn parse_serp(html: &str, max_results: usize) -> Vec<SearchResult> {
    let doc = Html::parse_document(html);
    let result_sel =
        Selector::parse(result_selector()).expect("static selector should parse");
    let title_sel = Selector::parse("a.result__a").expect("static selector should parse");
    let snippet_sel =
        Selector::parse("a.result__snippet, .result__snippet").expect("static selector should parse");

    let mut out = Vec::with_capacity(max_results.min(20));
    for el in doc.select(&result_sel) {
        if out.len() >= max_results {
            break;
        }
        let title_el = match el.select(&title_sel).next() {
            Some(t) => t,
            None => continue,
        };
        let title = title_el.text().collect::<String>().trim().to_string();
        let raw_url = title_el
            .value()
            .attr("href")
            .unwrap_or_default()
            .to_string();
        // DDG wraps every result link as /l/?uddg=<encoded>&...; unwrap it
        // so the agent gets the real destination.
        let url = unwrap_ddg_redirect(&raw_url);
        let snippet = el
            .select(&snippet_sel)
            .next()
            .map(|s| s.text().collect::<String>().trim().to_string())
            .unwrap_or_default();

        if title.is_empty() && url.is_empty() {
            continue;
        }
        out.push(SearchResult { title, url, snippet });
    }
    out
}

/// DDG's HTML SERP wraps every result `href` as `/l/?uddg=<urlencoded>`.
/// Unwrap to the real destination URL. If parsing fails we return the input
/// unchanged so the caller still gets *something* clickable.
fn unwrap_ddg_redirect(href: &str) -> String {
    // Accept the absolute and protocol-relative forms DDG uses.
    let stripped = href
        .strip_prefix("//duckduckgo.com")
        .or_else(|| href.strip_prefix("https://duckduckgo.com"))
        .or_else(|| href.strip_prefix("http://duckduckgo.com"))
        .unwrap_or(href);

    if let Some(rest) = stripped.strip_prefix("/l/?") {
        for pair in rest.split('&') {
            if let Some(v) = pair.strip_prefix("uddg=") {
                if let Ok(decoded) = urlencoding_decode(v) {
                    return decoded;
                }
            }
        }
    }
    href.to_string()
}

/// Minimal percent-decoder. Avoids pulling in the `urlencoding` crate just
/// for one helper. Returns `Err` only on invalid UTF-8 in the decoded bytes.
fn urlencoding_decode(s: &str) -> std::result::Result<String, std::str::Utf8Error> {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                if let (Some(h), Some(l)) = (hi, lo) {
                    out.push(((h << 4) | l) as u8);
                    i += 3;
                    continue;
                }
                out.push(b'%');
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    std::str::from_utf8(&out).map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unwraps_ddg_redirect() {
        let href = "//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fa%3Fb%3D1&rut=abc";
        assert_eq!(unwrap_ddg_redirect(href), "https://example.com/a?b=1");
    }

    #[test]
    fn passes_through_non_redirect() {
        let href = "https://example.com/direct";
        assert_eq!(unwrap_ddg_redirect(href), href);
    }
}
