//! Live smoke test for the bundled web stack:
//! DuckDuckGo search + HybridReader (Local → Jina → Wayback).
//! Run with: `cargo run -p athen-web --example smoke`

use athen_web::{DuckDuckGoSearch, HybridReader, PageReader, WebSearchProvider};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== web_search (DuckDuckGo) ===");
    let search = DuckDuckGoSearch::new();
    let results = search.search("rust async trait object", 3).await?;
    for (i, r) in results.iter().enumerate() {
        println!("{}. {}", i + 1, r.title);
        println!("   {}", r.url);
        let snippet = r.snippet.chars().take(120).collect::<String>();
        println!("   {snippet}");
    }
    println!();

    let reader = HybridReader::new();
    let urls = [
        ("static (expect: local-html)", "https://example.com/"),
        (
            "personal blog (expect: local-html)",
            "https://alejandrogarcia.blog/",
        ),
        (
            "JS-heavy SPA (expect: jina or wayback)",
            "https://x.com/elonmusk",
        ),
    ];

    for (label, url) in urls {
        println!("=== web_fetch — {label} ===");
        println!("url: {url}");
        match reader.fetch(url).await {
            Ok(page) => {
                println!("source: {}", page.source);
                println!("title: {:?}", page.title);
                println!("content_chars: {}", page.content.chars().count());
                let preview = page.content.chars().take(300).collect::<String>();
                println!("--- preview ---\n{preview}");
            }
            Err(e) => println!("ERROR: {e}"),
        }
        println!();
    }

    Ok(())
}
