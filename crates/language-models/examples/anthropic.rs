//! ANTHROPIC_API_KEY and ANTHROPIC_MODEL select credentials and a model; no
//! network request is made until the provider starts the completion.
use futures::StreamExt as _;
use language_models::{
    Anthropic, CompletionRequest, ContentDelta, LanguageModel, LanguageModelProvider, Message,
    StreamEvent,
};
use std::io::Write as _;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let key = std::env::var("ANTHROPIC_API_KEY")?;
    let model = LanguageModel::new(std::env::var("ANTHROPIC_MODEL")?);
    let provider = Anthropic::new("https://api.anthropic.com/v1")?.with_api_key(&key)?;
    let request = CompletionRequest {
        messages: vec![Message::user("Explain Rust's ownership model briefly.")],
        max_output_tokens: Some(4096),
        ..Default::default()
    };
    let mut stream = provider.stream(&model, request).await?;
    while let Some(event) = stream.next().await {
        if let StreamEvent::Delta {
            delta: ContentDelta::Text(text),
            ..
        } = event?
        {
            print!("{text}");
            std::io::stdout().flush()?;
        }
    }
    println!();
    Ok(())
}
