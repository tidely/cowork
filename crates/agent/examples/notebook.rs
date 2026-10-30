use std::sync::Arc;

use agent::{
    AgentBuilder, AgentDefinition, AgentHost, Delivery, Fallback, Lifetime, Ollama, Policy,
    QueueAccess,
};
use language_model::LanguageModel;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::sync::Mutex;
use tool_runtime::{Tool, ToolCapabilities};

#[derive(Debug, Clone, Default, PartialEq, PartialOrd)]
enum Permission {
    #[default]
    ReadOnly,
    Write,
}

#[derive(Default, Debug)]
struct Environment {
    todo: Mutex<String>,
}

impl AsRef<Mutex<String>> for Environment {
    fn as_ref(&self) -> &Mutex<String> {
        &self.todo
    }
}

impl AgentHost for Environment {
    type Source = &'static str;
    type Input = String;
    type Access = Permission;
    type Permissions = Permission;

    fn authorize(&self, call: &agent::CallAuthorization<'_, Self>) -> agent::Decision {
        if call.permissions >= call.access {
            agent::Decision::Allow
        } else {
            agent::Decision::Deny(String::from("Insufficient permissions"))
        }
    }
}

struct ReadTodo;

#[derive(Deserialize, JsonSchema)]
struct ReadTodoArgs {}

impl Tool for ReadTodo {
    type Args = ReadTodoArgs;
    type Output = String;
    type Access = Permission;
    type Context = Mutex<String>;

    fn name(&self) -> &str {
        "read_todo"
    }

    fn description(&self) -> String {
        String::from("Read the TODO list")
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities {
            is_read_only: true,
            ..Default::default()
        }
    }

    async fn run(
        &self,
        _call: tool_runtime::ToolCall<Self::Access>,
        context: &Self::Context,
        _args: Self::Args,
    ) -> Result<Self::Output, tool_runtime::ToolError> {
        Ok(context.lock().await.to_string())
    }
}

struct WriteTodo;

#[derive(Deserialize, JsonSchema)]
struct WriteTodoArgs {
    text: String,
}

impl Tool for WriteTodo {
    type Args = WriteTodoArgs;
    type Output = String;
    type Access = Permission;
    type Context = Mutex<String>;

    fn name(&self) -> &str {
        "write_todo"
    }

    fn description(&self) -> String {
        String::from("Write the TODO list")
    }

    fn access(&self, _args: &Self::Args, _context: &Self::Context) -> Self::Access {
        Permission::Write
    }

    async fn run(
        &self,
        _call: tool_runtime::ToolCall<Self::Access>,
        context: &Self::Context,
        args: Self::Args,
    ) -> Result<Self::Output, tool_runtime::ToolError> {
        *context.lock().await = args.text;
        Ok(String::from("Wrote TODO"))
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let provider = Arc::new(Ollama::new("http://127.0.0.1:11434")?);
    let model = LanguageModel::new("qwen3.8:27b");

    let definition = AgentDefinition::new(model).instructions("Be helpful to the user.");
    let host = Arc::new(Environment::default());

    let agent = AgentBuilder::new(host.clone(), provider, definition, "User")
        .tool(ReadTodo)?
        .tool(WriteTodo)?
        .permissions(Permission::Write)
        .build()
        .await?;

    agent
        .prompt("Write work I receive from Slack in my TODO list")
        .await?;

    let reply = agent
        .handle()
        .run(
            "Slack",
            String::from("Make the powerpoint about climate change before friday"),
            Delivery::Steer,
            Policy {
                access: QueueAccess::Protected,
                fallback: Fallback::Queue,
                lifetime: Lifetime::Retain,
            },
        )
        .await?;

    println!("{}", reply.text());
    println!("TODO: {}", host.todo.lock().await);

    Ok(())
}
