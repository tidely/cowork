mod tools;

pub use tools::calculate::*;
pub use tools::comments::*;

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use agent::tool::{ToolErrorKind, ToolRegistry};
    use serde_json::json;

    use super::*;

    struct Host(RunComments);

    impl AsRef<RunComments> for Host {
        fn as_ref(&self) -> &RunComments {
            &self.0
        }
    }

    impl AsRef<()> for Host {
        fn as_ref(&self) -> &() {
            &()
        }
    }

    /// Invented arguments reach the model as actionable errors.
    #[test]
    fn unknown_arguments_return_model_feedback() {
        let mut tools = ToolRegistry::<Host, ()>::default();
        tools.register(Calculate).unwrap();
        tools.register(RespondToComment).unwrap();
        let host = Arc::new(Host(RunComments::default()));

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
            let Err(error) = tools.prepare(name, arguments, &host) else {
                panic!("{name} accepted an unknown argument");
            };
            assert_eq!(error.kind, ToolErrorKind::InvalidArguments, "{name}");
            assert!(error.message.contains("unknown field `timeout`"), "{error}");
            assert!(error.message.contains(expected), "{error}");
        }
    }
}
