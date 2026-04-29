//! Live smoke test for the bundled DuckDuckGo + LocalReader stack.
//! Run with: `cargo run -p athen-web --example smoke`

use athen_web::{DuckDuckGoSearch, LocalReader, PageReader, WebSearchProvider};

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

    println!("=== fetch_url (LocalReader) ===");
    let reader = LocalReader::new();
    let page = reader.fetch("https://example.com/").await?;
    println!("source: {}", page.source);
    println!("title: {:?}", page.title);
    println!("url: {}", page.url);
    println!("--- content ({} chars) ---", page.content.chars().count());
    let preview = page.content.chars().take(400).collect::<String>();
    println!("{preview}");

    Ok(())
}
