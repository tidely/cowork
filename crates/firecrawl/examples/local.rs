//! Start `firecrawl-local serve` in another terminal, then run:
//! `cargo run -p firecrawl --example local -- https://example.com`.

use firecrawl::{Client, Format, MapRequest, ScrapeRequest};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let url = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "https://example.com".into());
    let client = Client::new(reqwest::Client::new(), "http://127.0.0.1:3002");
    let health = client.health().await?;
    println!("{} {}: {}", health.engine, health.version, health.status);

    let document = client
        .scrape(&ScrapeRequest {
            url: url.clone(),
            formats: vec![Format::Markdown],
            only_main_content: Some(true),
        })
        .await?;
    if let Some(warning) = document.warning {
        eprintln!("Scrape warning: {warning}");
    }
    if document.blocked {
        return Err("site blocked the scrape".into());
    }
    let markdown = document
        .markdown
        .filter(|text| !text.trim().is_empty())
        .ok_or("scrape returned no Markdown")?;
    println!("\n{markdown}");

    let links = client
        .map(&MapRequest {
            url,
            search: None,
            limit: Some(5),
        })
        .await?;
    println!("\nMap ({} URLs):", links.len());
    for link in links {
        println!("{}", link.url);
    }
    Ok(())
}
