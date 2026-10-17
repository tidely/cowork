//! The tool the model asks for more permissions with, for the rest of its
//! run, as Codex's `request_permissions`: more access to the project, and
//! folders outside it.
//!
//! The tool itself grants nothing. The host's approval hook resolves what a
//! call asks for (see [`RequestPermissions::requested`]), asks the host, and
//! keeps what was allowed for the run's later commands; it then lets the
//! call through with everything the run has been granted, which the tool
//! reports back to the model. Like `run_command`'s, the definition never
//! changes and names no host paths.

use rig::tool::{ContextValue, Tool, ToolContext, ToolExecutionError};
use serde::{Deserialize, Serialize};

use crate::{Access, CallApprovals, Folders, Grant, RunCommand, grant::named_folders};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RequestPermissionsArgs {
    /// Why the permissions are needed, in one short sentence the user reads
    /// when deciding, such as "Download the project's dependencies."
    pub reason: String,
    /// Access to the project later commands need: `write` to change it,
    /// `network` to also reach the network. Leave it out to ask only for
    /// folders.
    #[serde(default)]
    pub access: Option<Access>,
    /// Folders outside the project later commands need to read, as absolute
    /// paths on the user's computer, or starting with `~/` for the user's
    /// home folder. Optional: reads are already allowed except credentials.
    #[serde(default)]
    pub read_folders: Vec<String>,
    /// Folders outside the project later commands need to change, such as
    /// `~/.cargo` for Cargo's downloads, written like `read_folders`.
    #[serde(default)]
    pub write_folders: Vec<String>,
}

/// Asks the user for permissions the run's later commands get without
/// asking; see the module documentation.
pub struct RequestPermissions;

impl RequestPermissions {
    /// What a call with `arguments` asks for, resolved on this computer, or
    /// why it can't be granted. A call must ask for something beyond reading
    /// the project, which every command may do.
    pub fn requested(arguments: &serde_json::Value) -> Result<Grant, String> {
        let (read, write) = named_folders(arguments);
        let grant = Grant {
            access: RunCommand::requested_access(arguments).unwrap_or_default(),
            folders: Folders::resolve(&read, &write)?,
        };
        if grant == Grant::default() {
            return Err(
                "This asks for nothing: commands can always read the project. Name the `access` \
                 or the folders later commands need."
                    .into(),
            );
        }
        Ok(grant)
    }

    /// The context for an allowed call, with everything the run has been
    /// granted, `granted`, for the tool to report.
    pub fn context(granted: Grant) -> ToolContext {
        let mut context = ToolContext::new();
        context
            .insert(Granted(granted))
            .expect("a grant always encodes");
        context
    }
}

/// Everything the run was granted once the call was allowed; see
/// [`RequestPermissions::context`].
#[derive(Serialize, Deserialize)]
struct Granted(Grant);

impl ContextValue for Granted {
    const KEY: &'static str = "sandbox.request_permissions.granted";
}

/// Why a call reached the tool without the host allowing it, as from an
/// agent without an approval hook.
#[derive(Debug)]
pub struct NotGranted;

impl std::fmt::Display for NotGranted {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Nothing was granted: only the user can grant permissions.")
    }
}

impl std::error::Error for NotGranted {}

const DESCRIPTION: &str = "Ask the user for more permissions for the commands you run next, \
when several commands will need them: `access` to the project (`write` to change it, `network` to \
also reach the network), and folders outside the project to read or to change, such as a package \
manager's cache. Permissions covered by the project's auto-approval level or this run are \
approved automatically. Extra permissions wait for the user to allow them, except in a scheduled \
run, where the first call needing approval is refused with an explanation. Once allowed, later `run_command` calls get these \
permissions automatically and run without asking until this turn ends; the result lists \
everything granted. They don't apply to the file tools. For a single command, ask in its own \
`run_command` arguments instead. If the user denies it, carry on without it.";

impl Tool for RequestPermissions {
    const NAME: &'static str = "request_permissions";
    type Error = NotGranted;
    type Args = RequestPermissionsArgs;
    type Output = String;

    fn description(&self) -> String {
        DESCRIPTION.to_owned()
    }

    fn parameters(&self) -> serde_json::Value {
        schemars::schema_for!(RequestPermissionsArgs).into()
    }

    fn map_error(&self, error: Self::Error) -> ToolExecutionError {
        ToolExecutionError::permission_denied(error.to_string())
    }

    async fn call(
        &self,
        context: &mut ToolContext,
        _: Self::Args,
    ) -> Result<Self::Output, Self::Error> {
        let Granted(granted) = CallApprovals::approved(context, Self::NAME)
            .get::<Granted>()
            .ok()
            .flatten()
            .ok_or(NotGranted)?;
        Ok(format!(
            "The user allowed it. For the rest of this turn, `run_command` calls run with these \
             permissions unless they ask for other access, and without asking the user when they \
             need nothing more:\n{}",
            granted.describe()
        ))
    }
}

#[cfg(test)]
mod tests {
    use rig::tool::ToolSet;
    use serde_json::json;

    use super::*;
    use crate::TestRoot;

    #[test]
    fn calls_must_ask_for_something_that_can_be_granted() {
        let folder = TestRoot::new();
        let path = folder.0.to_string_lossy().into_owned();
        let requested = RequestPermissions::requested(&json!({
            "reason": "Fetch dependencies.",
            "access": "network",
            "write_folders": [path],
        }))
        .unwrap();
        assert_eq!(requested.access, Access::Network);
        assert_eq!(
            requested.folders.write,
            [crate::resolved(&folder.0).unwrap()]
        );

        let only_folders = RequestPermissions::requested(&json!({
            "reason": "Read the docs.",
            "read_folders": [path],
        }))
        .unwrap();
        assert_eq!(only_folders.access, Access::Read);

        for nothing in [
            json!({"reason": "Nothing."}),
            json!({"reason": "Nothing.", "access": "read"}),
        ] {
            assert!(
                RequestPermissions::requested(&nothing)
                    .unwrap_err()
                    .contains("asks for nothing")
            );
        }
    }

    /// The tool reports what the hook allowed, and nothing without it.
    #[tokio::test]
    async fn the_tool_reports_what_was_granted() {
        let mut tools = ToolSet::default();
        tools.add_tool(RequestPermissions);
        let arguments = json!({"reason": "Fetch.", "access": "network"}).to_string();

        let refused = tools
            .execute(
                RequestPermissions::NAME,
                arguments.clone(),
                &mut ToolContext::new(),
            )
            .await;
        let refused = refused.error().expect("refused").message().to_owned();
        assert!(refused.contains("only the user"), "{refused}");

        let granted = Grant {
            access: Access::Network,
            ..Grant::default()
        };
        let reported = tools
            .execute(
                RequestPermissions::NAME,
                arguments,
                &mut RequestPermissions::context(granted),
            )
            .await;
        assert!(reported.is_success());
        let reported = reported.output().render();
        assert!(reported.contains("project access: network"), "{reported}");
    }
}
