use std::io::{self, Write};

use futures::StreamExt as _;
use language_models::{
    CompletionRequest, ContentDelta, LanguageModel, LanguageModelProvider, Message,
    OpenAiCompatible, StreamEvent, Thinking,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let provider = OpenAiCompatible::new("http://localhost:11434/v1")?;
    let model = LanguageModel::new("qwen3.8:27b");
    let request = CompletionRequest {
        messages: vec![Message::user("Say hello in one sentence.")],
        thinking: Some(Thinking::Effort("medium".into())),
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
            io::stdout().flush()?;
        }
    }
    println!();
    Ok(())
}
