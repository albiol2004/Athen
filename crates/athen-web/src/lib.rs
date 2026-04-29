//! Web search and page-reader providers for Athen agents.
//!
//! Two ports:
//! - [`WebSearchProvider`]: query the web, return ranked results.
//! - [`PageReader`]: turn a URL into clean readable markdown.
//!
//! Bundled defaults ([`DuckDuckGoSearch`], [`LocalReader`]) need no API keys.
//! Optional adapters ([`TavilySearch`], [`CloudflareReader`]) upgrade quality
//! when the user supplies credentials.

pub mod reader;
pub mod search;

pub use reader::{
    cloudflare::CloudflareReader, local::LocalReader, PageReader, ReadResult,
};
pub use search::{
    duckduckgo::DuckDuckGoSearch, tavily::TavilySearch, SearchResult, WebSearchProvider,
};

/// Reusable [`reqwest::Client`] with a sane default timeout. Adapters that
/// don't bring their own client should use this one.
pub fn default_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent("Athen/0.1 (+https://github.com/albiol2004/Athen)")
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .expect("reqwest client should build with default config")
}
