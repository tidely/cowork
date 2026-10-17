//! Handing the host's approval of one call to the tool that runs it.
//!
//! An approved write, command, or permission request runs with exactly what
//! the host approved ([`crate::WriteFile::context`], [`crate::RunCommand::context`],
//! [`crate::RequestPermissions::context`]). Rig gives every call of a run the
//! same tool context, so the approval travels beside it instead: the run's
//! context carries a [`CallApprovals`], the approval hook leaves the call's
//! context in it just before the call is dispatched, and the tool takes it.
//!
//! Calls must be dispatched one at a time, so that the context left is the
//! dispatched call's: only a call of the same tool takes it, and the hook
//! clears it once the call returned, so it never outlives its call.

use std::sync::{Arc, Mutex};

use rig::tool::ToolContext;

/// The context the host approved the call about to run with, if any; see
/// the module documentation.
#[derive(Default)]
pub struct CallApprovals(Mutex<Option<(String, ToolContext)>>);

impl CallApprovals {
    /// A run's tool context, carrying `approvals` to its tools.
    pub fn run_context(approvals: &Arc<Self>) -> ToolContext {
        ToolContext::new().with_scope(approvals.clone())
    }

    /// Leaves `context` for the next call of `tool`, which is about to run.
    pub fn approve(&self, tool: &str, context: ToolContext) {
        *self.0.lock().unwrap() = Some((tool.to_owned(), context));
    }

    /// Forgets an approval no call took, once the call it was for returned.
    pub fn clear(&self) {
        self.0.lock().unwrap().take();
    }

    /// The context the host approved this call of `tool` with: what the run
    /// left for it, or, without a run's approvals, `context` itself, as for a
    /// tool called directly with an approval context.
    pub(crate) fn approved(context: &ToolContext, tool: &str) -> ToolContext {
        let Some(approvals) = context.scope::<Self>() else {
            return context.clone();
        };
        let mut approved = approvals.0.lock().unwrap();
        match approved.take() {
            Some((approved_tool, approved)) if approved_tool == tool => approved,
            other => {
                *approved = other;
                ToolContext::new()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use rig::tool::Tool as _;

    use super::*;
    use crate::{Grant, RunCommand};

    /// An approval reaches the next call of its tool only, once.
    #[test]
    fn an_approval_reaches_one_call_of_its_tool() {
        let approvals = Arc::new(CallApprovals::default());
        let context = CallApprovals::run_context(&approvals);
        let granted = |context: &ToolContext| !context.is_empty();

        approvals.approve(RunCommand::NAME, RunCommand::context(Grant::default()));
        assert!(!granted(&CallApprovals::approved(&context, "write_file")));
        assert!(granted(&CallApprovals::approved(
            &context,
            RunCommand::NAME
        )));
        assert!(!granted(&CallApprovals::approved(
            &context,
            RunCommand::NAME
        )));

        approvals.approve(RunCommand::NAME, RunCommand::context(Grant::default()));
        approvals.clear();
        assert!(!granted(&CallApprovals::approved(
            &context,
            RunCommand::NAME
        )));

        // A tool called with an approval context directly keeps it.
        let direct = RunCommand::context(Grant::default());
        assert!(granted(&CallApprovals::approved(&direct, RunCommand::NAME)));
    }
}
