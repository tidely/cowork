//! An agent with its own host: tools that reach an application service
//! through their typed context, per-call access with approvals answered as
//! requests, a permission change the model is told about, and a dynamic tool
//! found with `search_tools` and run through `call_tool`. Needs a local
//! Ollama server with `gemma4:e4b`.
//!
//! cargo run -p agent --example agent

use std::sync::{Arc, Mutex};

use agent::tool::{Provides, Tool, ToolCall, ToolCapabilities, ToolDescriptor, ToolError};
use agent::{
    Agent, AgentBuilder, AgentDefinition, AgentHost, ApprovalDecision, CallAuthorization,
    ConfigurationChange, Decision, DynamicToolChange, Entry, EventKind, Origin, Request,
    RequestKind,
};
use agent::{
    Ollama,
    model::{LanguageModel, Thinking, ToolArguments},
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

/// An application service the note tools share.
#[derive(Default)]
struct Notebook {
    notes: Mutex<Vec<String>>,
}

/// The host: the session's types and policy, and the services its tools use.
struct Desk {
    notebook: Arc<Notebook>,
}

impl Provides<Notebook> for Desk {
    fn provide(self: &Arc<Self>) -> Arc<Notebook> {
        self.notebook.clone()
    }
}

/// What one call needs.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
enum Access {
    #[default]
    Read,
    Write,
}

/// Tools that declare no access of their own only read.
impl From<()> for Access {
    fn from(_: ()) -> Self {
        Self::Read
    }
}

/// The live permission state: whether writing needs the user's approval.
#[derive(Clone, Debug, Default)]
struct NotePolicy {
    auto_write: bool,
}

impl AgentHost for Desk {
    type Source = String;
    type Input = String;
    type Access = Access;
    type Permissions = NotePolicy;

    fn authorize(&self, call: &CallAuthorization<'_, Self>) -> Decision {
        let granted = call
            .grants
            .iter()
            .any(|grant| grant.access == Access::Write);
        match call.access {
            Access::Read => Decision::Allow,
            Access::Write if call.permissions.auto_write || granted => Decision::Allow,
            Access::Write => Decision::Ask,
        }
    }

    fn describe_permissions(&self, policy: &NotePolicy, _: &[ToolDescriptor]) -> Option<String> {
        Some(if policy.auto_write {
            "Reading and writing notes run without approval.".into()
        } else {
            "Reading notes runs without approval. Writing a note needs the user's approval.".into()
        })
    }
}

#[derive(Deserialize, JsonSchema)]
struct NoArgs {}

struct ReadNotes;

impl Tool for ReadNotes {
    type Args = NoArgs;
    type Output = Value;
    type Context = Notebook;
    type Access = Access;

    fn name(&self) -> &str {
        "read_notes"
    }
    fn description(&self) -> String {
        "Read every saved note".into()
    }
    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities {
            is_read_only: true,
            ..Default::default()
        }
    }
    async fn run(
        &self,
        _: ToolCall<Access>,
        notebook: Arc<Notebook>,
        _: NoArgs,
    ) -> Result<Value, ToolError> {
        Ok(json!({"notes": *notebook.notes.lock().unwrap()}))
    }
}

#[derive(Deserialize, JsonSchema)]
struct NoteArgs {
    /// The note's text.
    text: String,
}

struct WriteNote;

impl Tool for WriteNote {
    type Args = NoteArgs;
    type Output = Value;
    type Context = Notebook;
    type Access = Access;

    fn name(&self) -> &str {
        "write_note"
    }
    fn description(&self) -> String {
        "Save a note".into()
    }
    fn access(&self, _: &NoteArgs, _: &Notebook) -> Access {
        Access::Write
    }
    async fn run(
        &self,
        call: ToolCall<Access>,
        notebook: Arc<Notebook>,
        args: NoteArgs,
    ) -> Result<Value, ToolError> {
        // The call carries exactly the access that was authorized.
        assert_eq!(call.access, Access::Write);
        notebook.notes.lock().unwrap().push(args.text.clone());
        Ok(json!({"saved": args.text}))
    }
}

/// Added while the conversation runs; reached through `call_tool`.
struct WordCount;

impl Tool for WordCount {
    type Args = NoteArgs;
    type Output = Value;
    type Context = ();
    type Access = ();

    fn name(&self) -> &str {
        "word_count"
    }
    fn description(&self) -> String {
        "Count the words in a text".into()
    }
    async fn run(&self, _: ToolCall<()>, _: Arc<()>, args: NoteArgs) -> Result<Value, ToolError> {
        Ok(json!({"words": args.text.split_whitespace().count()}))
    }
}

async fn ask(agent: &Agent<Desk>, prompt: &str) -> Result<(), Box<dyn std::error::Error>> {
    println!("\n> {prompt}");
    println!("{}", agent.prompt(prompt).await?.text().trim());
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let notebook = Arc::new(Notebook::default());
    let desk = Arc::new(Desk {
        notebook: notebook.clone(),
    });
    let definition = AgentDefinition::new(LanguageModel::new("gemma4:e4b"))
        .instructions(
            "You keep the user's notes with the read_notes and write_note tools. \
             Use tools exactly as the user asks, and keep replies short.",
        )
        .tool("read_notes")
        .tool("write_note")
        .thinking(Thinking::Disabled);
    let (events, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let provider = Arc::new(Ollama::new("http://localhost:11434")?);
    let agent = AgentBuilder::new(desk, provider, definition, "user".to_owned())
        .tool(ReadNotes)?
        .tool(WriteNote)?
        .on_event(move |event| {
            let _ = events.send(event);
        })
        .build()
        .await?;

    // Show progress and answer approvals, as an application's UI would.
    let handle = agent.handle();
    let ui = tokio::spawn(async move {
        while let Some(event) = receiver.recv().await {
            match event.kind {
                EventKind::ToolStarted { call, tool } => {
                    let arguments = match &call.arguments {
                        ToolArguments::Object { value } => Value::Object(value.clone()),
                        ToolArguments::Invalid { raw } => Value::String(raw.clone()),
                    };
                    println!("  [tool] {tool} (called as {}) {arguments}", call.name);
                }
                EventKind::ApprovalRequested { call, tool, access } => {
                    println!(
                        "  [approval] {tool} needs {access:?} access; allowing it for this run"
                    );
                    let decision = RequestKind::ResolveApproval {
                        call: call.id,
                        decision: ApprovalDecision::AllowForRun,
                    };
                    let _ = handle.post(Request::new("user".into(), decision));
                }
                EventKind::MessageCommitted {
                    entry:
                        Entry {
                            origin: Origin::Context { sections },
                            ..
                        },
                    ..
                } => println!(
                    "  [context] the model is told about: {}",
                    sections.join(", ")
                ),
                EventKind::DynamicToolAdded(name) => println!("  [tools] added {name}"),
                _ => {}
            }
        }
    });

    ask(
        &agent,
        "Save a note that says \"Buy oat milk\" with write_note. \
         Then call read_notes and tell me what the notes say.",
    )
    .await?;

    // Configuration doesn't change the prompt prefix; the model hears about
    // the new policy in a context note before its next request.
    agent
        .send(RequestKind::Configure(ConfigurationChange::Permissions(
            NotePolicy { auto_write: true },
        )))
        .await?;
    ask(
        &agent,
        "Save another note that says \"Call the dentist\" with write_note. Reply with just OK.",
    )
    .await?;

    // A dynamic tool never changes the request's tools.
    agent
        .send(RequestKind::DynamicTools(DynamicToolChange::add(WordCount)))
        .await?;
    ask(
        &agent,
        "Call search_tools with the query \"count words\". Then use call_tool to run the tool \
         you found on the text \"the quick brown fox jumps\". Reply with only the number of words.",
    )
    .await?;

    println!("\nNotebook: {:?}", notebook.notes.lock().unwrap());
    agent.close().await?;
    ui.await?;
    Ok(())
}
