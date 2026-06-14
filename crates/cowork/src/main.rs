mod app;
mod cli;
mod config;
mod events;
mod permissions;
mod persistence;
mod runtime;
mod tui;
mod ui;

use clap::Parser;

use cli::RunOptions;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tui::run(RunOptions::parse()).await
}
