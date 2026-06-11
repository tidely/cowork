use std::collections::HashSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolPermissionMode {
    Allow,
    Ask,
    Deny,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentProfile {
    allowed_tools: HashSet<&'static str>,
    denied_tools: HashSet<&'static str>,
    default_permission: ToolPermissionMode,
}

impl AgentProfile {
    pub(crate) fn from_env() -> Self {
        match std::env::var("COWORK_AGENT_PROFILE")
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str()
        {
            "skip-permissions" | "unrestricted" => Self::skip_permissions(),
            "read" | "read-only" => Self::read_only(),
            _ => Self::ask_for_writes(),
        }
    }

    pub(crate) fn ask_for_writes() -> Self {
        Self {
            allowed_tools: always_allowed_read_and_delegate_tools(),
            denied_tools: HashSet::new(),
            default_permission: ToolPermissionMode::Ask,
        }
    }

    pub(crate) fn skip_permissions() -> Self {
        Self {
            allowed_tools: HashSet::new(),
            denied_tools: HashSet::new(),
            default_permission: ToolPermissionMode::Allow,
        }
    }

    pub(crate) fn read_only() -> Self {
        Self {
            allowed_tools: always_allowed_read_and_delegate_tools(),
            denied_tools: tool_names(["edit_file", "write_file"]),
            default_permission: ToolPermissionMode::Ask,
        }
    }

    pub(crate) fn permission_for_tool(&self, tool_name: &str) -> ToolPermissionMode {
        if self.denied_tools.contains(tool_name) {
            ToolPermissionMode::Deny
        } else if self.allowed_tools.contains(tool_name) {
            ToolPermissionMode::Allow
        } else {
            self.default_permission
        }
    }
}

fn always_allowed_read_and_delegate_tools() -> HashSet<&'static str> {
    tool_names(["read_file", "read_pdf", "list_directory", "subagent"])
}

fn tool_names<const N: usize>(names: [&'static str; N]) -> HashSet<&'static str> {
    names.into_iter().collect()
}
