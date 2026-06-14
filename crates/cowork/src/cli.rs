use clap::Parser;

#[derive(Debug, Parser)]
#[command(version, about = "TUI-first AI assistant with recursive subagents")]
pub(crate) struct RunOptions {
    /// Start a fresh in-memory session and do not load or save persisted threads.
    #[arg(long)]
    pub(crate) temporary: bool,

    /// Default Ollama model to use for the main agent and subagents.
    #[arg(short, long, default_value = "gemma4:12b-it-qat")]
    pub(crate) model: String,

    /// Submit this prompt as soon as the TUI starts.
    #[arg(short = 'p', long = "prompt", value_name = "PROMPT")]
    pub(crate) initial_prompt: Option<String>,
}
