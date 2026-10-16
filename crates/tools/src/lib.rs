mod tools;

pub use tools::calculate::*;
pub use tools::comments::*;

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rig::tool::{Tool as _, ToolContext, ToolErrorKind, ToolSet};
    use serde_json::json;

    use super::*;

    /// Invented arguments reach the model as actionable errors.
    #[test]
    fn unknown_arguments_return_model_feedback() {
        let mut tools = ToolSet::default();
        tools.add_tool(Calculate);
        tools.add_tool(RespondToComment::new(Arc::new(TurnComments::new(1))));

        for (name, mut arguments, expected) in [
            (
                Calculate::NAME,
                json!({ "operation": "add", "a": 2, "b": 3 }),
                "operation",
            ),
            (
                RespondToComment::NAME,
                json!({ "comment_id": "comment_1", "response": "Reply" }),
                "comment_id",
            ),
        ] {
            arguments["timeout"] = json!("60000");
            let result = futures::executor::block_on(tools.execute(
                name,
                arguments.to_string(),
                &mut ToolContext::new(),
            ));
            assert_eq!(
                result.error().unwrap().kind(),
                ToolErrorKind::InvalidArgs,
                "{name}"
            );
            let feedback = result.output().as_text().unwrap();
            assert!(feedback.contains("unknown field `timeout`"), "{feedback}");
            assert!(feedback.contains(expected), "{feedback}");
        }
    }
}
