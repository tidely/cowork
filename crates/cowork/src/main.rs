use rig_core::memory::InMemoryConversationMemory;

mod agent;
mod app;

mod tools;
mod tui;
mod ui;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let memory = InMemoryConversationMemory::new();
    tui::run(memory).await
}
