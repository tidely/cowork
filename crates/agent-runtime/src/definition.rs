use language_model::Thinking;

/// Reusable, data-only agent configuration. Providers, tool implementations,
/// approval callbacks, storage, and live conversation state are bound at build
/// time. Cloning a definition never shares a running conversation.
#[derive(Clone, Debug)]
pub struct AgentDefinition {
    pub name: String,
    pub description: String,
    pub system_prompt: String,
    /// None exposes all registered tools; Some selects exactly these names.
    /// Selection never grants execution permission. Missing names fail build.
    pub tools: Option<Vec<String>>,
    pub permissions: ToolPermissions,
    pub max_turns: usize,
    pub max_output_tokens: u64,
    pub thinking: Option<Thinking>,
}

impl Default for AgentDefinition {
    fn default() -> Self {
        Self {
            name: "agent".into(),
            description: String::new(),
            system_prompt: String::new(),
            tools: None,
            permissions: ToolPermissions::default(),
            max_turns: 8,
            max_output_tokens: 4096,
            thinking: None,
        }
    }
}

/// Runtime services and approval state do not belong in a definition.
/// Custom host approval logic is installed separately with AgentBuilder::tool_policy.
#[derive(Clone, Debug, Default)]
pub enum ToolPermissions {
    #[default]
    DenyAll,
    AllowListed(Vec<String>),
    AllowAll,
}
