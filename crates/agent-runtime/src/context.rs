//! Model-visible context gathered from the session's subsystems at each step.
//!
//! A section is a typed view of some state. At every safe point the session
//! takes each section's snapshot and compares it with what the model last
//! received; changed sections render a note. Notes are committed to the
//! conversation (never injected for one request only, which would change the
//! next request's prefix), each wrapped in its own `<section_id>` tags inside
//! one user entry with an `Origin::Context`, as Codex does with its fragments.

use std::collections::BTreeMap;

use language_model::{Content, Message, Role};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use tool_runtime::ToolDescriptor;

use crate::{
    AgentDefinition, AgentHost, Configuration, Conversation, DefinitionRevision, Entry, Origin,
};

/// Read-only session state a section can describe.
pub struct SessionView<'a, H: AgentHost> {
    pub host: &'a H,
    pub definition: &'a AgentDefinition,
    pub revision: DefinitionRevision,
    pub configuration: &'a Configuration<H::Permissions>,
    /// Base tools offered directly, without `search_tools` and `call_tool`.
    pub tools: &'a [ToolDescriptor],
    /// Tools reachable through `call_tool`.
    pub dynamic_tools: &'a [ToolDescriptor],
}

/// A typed part of the state visible to the model.
pub trait ContextSection<H: AgentHost>: Send + Sync + 'static {
    /// Tag name and delivery key; must stay stable.
    const ID: &'static str;
    /// Only the data needed to decide what the model must be told next.
    type Snapshot: Serialize + DeserializeOwned + PartialEq;

    fn snapshot(&self, view: &SessionView<'_, H>) -> Self::Snapshot;

    /// `previous` is `None` when the model has not seen this section, at the
    /// start or after a compaction summarized it: render the full state.
    /// Return `None` when there is nothing to say.
    fn render(&self, previous: Option<&Self::Snapshot>, current: &Self::Snapshot)
    -> Option<String>;
}

/// Object-safe form, so a session can hold sections with different snapshots.
pub(crate) trait ErasedSection<H: AgentHost>: Send + Sync {
    fn id(&self) -> &'static str;
    /// The new snapshot and its note, or `None` if nothing changed.
    fn update(
        &self,
        view: &SessionView<'_, H>,
        previous: Option<&Value>,
    ) -> Option<(Value, Option<String>)>;
}

impl<H: AgentHost, T: ContextSection<H>> ErasedSection<H> for T {
    fn id(&self) -> &'static str {
        T::ID
    }

    fn update(
        &self,
        view: &SessionView<'_, H>,
        previous: Option<&Value>,
    ) -> Option<(Value, Option<String>)> {
        let current = self.snapshot(view);
        // A snapshot this version can't read is treated as never delivered.
        let previous =
            previous.and_then(|value| serde_json::from_value::<T::Snapshot>(value.clone()).ok());
        if previous.as_ref() == Some(&current) {
            return None;
        }
        let note = self.render(previous.as_ref(), &current);
        let snapshot = serde_json::to_value(&current).expect("section snapshots serialize");
        Some((snapshot, note))
    }
}

/// Context to commit at a safe point: the entry (if any section has
/// something to say) and every changed snapshot.
pub(crate) struct ContextUpdate<S> {
    pub entry: Option<Entry<S>>,
    pub snapshots: BTreeMap<String, Value>,
}

impl<S> ContextUpdate<S> {
    pub fn is_empty(&self) -> bool {
        self.snapshots.is_empty()
    }
}

pub(crate) fn update<H: AgentHost>(
    sections: &[Box<dyn ErasedSection<H>>],
    view: &SessionView<'_, H>,
    conversation: &Conversation<H::Source>,
) -> ContextUpdate<H::Source> {
    let mut snapshots = BTreeMap::new();
    let mut ids = Vec::new();
    let mut content = Vec::new();
    for section in sections {
        let Some((snapshot, note)) = section.update(view, conversation.delivered(section.id()))
        else {
            continue;
        };
        snapshots.insert(section.id().to_owned(), snapshot);
        if let Some(note) = note {
            let id = section.id();
            content.push(Content::text(format!("<{id}>\n{}\n</{id}>", note.trim())));
            ids.push(id.to_owned());
        }
    }
    ContextUpdate {
        entry: (!content.is_empty()).then(|| Entry {
            message: Message::new(Role::User, content),
            origin: Origin::Context { sections: ids },
        }),
        snapshots,
    }
}

/// What the host's permissions mean for the offered tools, as the host's
/// [`AgentHost::describe_permissions`] puts it.
pub struct ToolPolicySection;

impl<H: AgentHost> ContextSection<H> for ToolPolicySection {
    const ID: &'static str = "tool_policy";
    type Snapshot = Option<String>;

    fn snapshot(&self, view: &SessionView<'_, H>) -> Option<String> {
        let tools: Vec<_> = view
            .tools
            .iter()
            .chain(view.dynamic_tools)
            .cloned()
            .collect();
        view.host
            .describe_permissions(&view.configuration.permissions, &tools)
    }

    fn render(
        &self,
        previous: Option<&Option<String>>,
        current: &Option<String>,
    ) -> Option<String> {
        match (previous, current) {
            (_, Some(text)) => Some(text.clone()),
            (Some(Some(_)), None) => Some("The earlier tool policy no longer applies.".into()),
            _ => None,
        }
    }
}

/// The dynamic tools reachable through `call_tool`, by name and description.
pub struct DynamicToolsSection;

impl<H: AgentHost> ContextSection<H> for DynamicToolsSection {
    const ID: &'static str = "dynamic_tools";
    type Snapshot = BTreeMap<String, String>;

    fn snapshot(&self, view: &SessionView<'_, H>) -> Self::Snapshot {
        view.dynamic_tools
            .iter()
            .map(|tool| {
                let description = tool.definition.description.clone().unwrap_or_default();
                (tool.name().to_owned(), description)
            })
            .collect()
    }

    fn render(
        &self,
        previous: Option<&Self::Snapshot>,
        current: &Self::Snapshot,
    ) -> Option<String> {
        let list = |tools: Vec<(&String, &String)>| {
            tools
                .into_iter()
                .map(|(name, description)| format!("- {name}: {description}"))
                .collect::<Vec<_>>()
                .join("\n")
        };
        let usage = "Get a tool's parameters with search_tools, then run it with call_tool.";
        let Some(previous) = previous else {
            return (!current.is_empty()).then(|| {
                format!(
                    "Tools available through call_tool:\n{}\n{usage}",
                    list(current.iter().collect())
                )
            });
        };
        let added: Vec<_> = current
            .iter()
            .filter(|(name, description)| previous.get(*name) != Some(description))
            .collect();
        let removed: Vec<_> = previous
            .keys()
            .filter(|name| !current.contains_key(*name))
            .map(String::as_str)
            .collect();
        let mut parts = Vec::new();
        if !added.is_empty() {
            parts.push(format!(
                "New tools available through call_tool:\n{}\n{usage}",
                list(added)
            ));
        }
        if !removed.is_empty() {
            parts.push(format!(
                "These tools are no longer available: {}.",
                removed.join(", ")
            ));
        }
        (!parts.is_empty()).then(|| parts.join("\n"))
    }
}

/// Tells the model when a replaced definition switched the model or its
/// thinking. The first definition needs no note.
pub struct ModelSection;

impl<H: AgentHost> ContextSection<H> for ModelSection {
    const ID: &'static str = "model";
    type Snapshot = (String, Option<String>);

    fn snapshot(&self, view: &SessionView<'_, H>) -> Self::Snapshot {
        let thinking = view
            .definition
            .thinking
            .as_ref()
            .map(|thinking| format!("{thinking:?}"));
        (view.definition.model.id.clone(), thinking)
    }

    fn render(
        &self,
        previous: Option<&Self::Snapshot>,
        current: &Self::Snapshot,
    ) -> Option<String> {
        let previous = previous?;
        (previous.0 != current.0).then(|| {
            format!(
                "The conversation continues with a different model, {}.",
                current.0
            )
        })
    }
}
