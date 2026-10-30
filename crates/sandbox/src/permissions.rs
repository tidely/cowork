//! The tool the model asks for more permissions with, for the rest of its
//! run, as Codex's `request_permissions`: more access to the project, and
//! folders outside it.
//!
//! A call needs what it asks for ([`Needs::Permissions`]), resolved before
//! anyone is asked. The host lets it run when the run has that already or
//! someone allows it; running, it adds what it asked for to the run's grants
//! (see [`ThreadSandbox::run_grant`]), which the run's later commands get,
//! and reports everything granted back to the model. Like `run_command`'s,
//! the definition never changes and names no host paths.

use agent::tool::{Tool, ToolCall, ToolError, ToolErrorKind};
use serde::Deserialize;

use crate::{Access, Folders, Grant, Needs, ThreadSandbox};

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
    pub const NAME: &'static str = "request_permissions";

    /// What a call with `args` asks for, resolved on this computer, or why
    /// it can't be granted. A call must ask for something beyond reading
    /// the project, which every command may do.
    pub fn requested(args: &RequestPermissionsArgs) -> Result<Grant, String> {
        let grant = Grant {
            access: args.access.unwrap_or_default(),
            folders: Folders::resolve(&args.read_folders, &args.write_folders)?,
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
}

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
    type Args = RequestPermissionsArgs;
    type Output = String;
    type Context = ThreadSandbox;
    type Access = Needs;

    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> String {
        DESCRIPTION.to_owned()
    }

    fn access(&self, args: &RequestPermissionsArgs, _: &ThreadSandbox) -> Needs {
        match Self::requested(args) {
            Ok(grant) => Needs::Permissions(grant),
            Err(reason) => Needs::Refused(reason),
        }
    }

    async fn run(
        &self,
        call: ToolCall<Needs>,
        sandbox: &Self::Context,
        _: Self::Args,
    ) -> Result<Self::Output, ToolError> {
        let Needs::Permissions(grant) = call.access else {
            return Err(ToolError::new(
                ToolErrorKind::PermissionDenied,
                "Nothing was granted: only the user can grant permissions.",
            ));
        };
        sandbox.sandboxes().grant_for_run(sandbox.thread(), &grant);
        Ok(format!(
            "The user allowed it. For the rest of this turn, `run_command` calls run with these \
             permissions unless they ask for other access, and without asking the user when they \
             need nothing more:\n{}",
            sandbox.run_grant().describe()
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;

    use super::*;
    use crate::{TestRoot, call, prepare, test_sandboxes};

    fn needs(arguments: serde_json::Value) -> Needs {
        let (sandboxes, _root) = test_sandboxes();
        let sandbox = Arc::new(ThreadSandbox::new(sandboxes, "thread"));
        prepare(RequestPermissions, &sandbox, arguments)
            .unwrap()
            .access()
            .clone()
    }

    #[test]
    fn calls_must_ask_for_something_that_can_be_granted() {
        let folder = TestRoot::new();
        let path = folder.0.to_string_lossy().into_owned();
        let Needs::Permissions(requested) = needs(json!({
            "reason": "Fetch dependencies.",
            "access": "network",
            "write_folders": [path],
        })) else {
            panic!("a grant");
        };
        assert_eq!(requested.access, Access::Network);
        assert_eq!(
            requested.folders.write,
            [crate::resolved(&folder.0).unwrap()]
        );

        let Needs::Permissions(only_folders) = needs(json!({
            "reason": "Read the docs.",
            "read_folders": [path],
        })) else {
            panic!("a grant");
        };
        assert_eq!(only_folders.access, Access::Read);

        for nothing in [
            json!({"reason": "Nothing."}),
            json!({"reason": "Nothing.", "access": "read"}),
        ] {
            assert!(matches!(
                needs(nothing),
                Needs::Refused(reason) if reason.contains("asks for nothing")
            ));
        }
    }

    /// Running, the tool grants the run what it asked for and reports
    /// everything granted.
    #[tokio::test]
    async fn the_tool_grants_the_run_what_it_asked_for() {
        let (sandboxes, _root) = test_sandboxes();
        let sandbox = Arc::new(ThreadSandbox::new(sandboxes, "thread"));
        let reported = call(
            RequestPermissions,
            &sandbox,
            json!({"reason": "Fetch.", "access": "network"}),
        )
        .await
        .unwrap();
        assert!(reported.contains("project access: network"), "{reported}");
        assert_eq!(sandbox.run_grant().access, Access::Network);
        sandbox.end_run();
        assert_eq!(sandbox.run_grant(), Grant::default());
    }
}
