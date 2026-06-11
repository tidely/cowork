mod app;
mod config;
mod events;
mod permissions;
mod runtime;
mod tui;
mod ui;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tui::run().await
}
