use std::{
    collections::HashSet,
    fmt,
    sync::{Arc, Mutex},
};

use rig::tool::{Tool, ToolContext, ToolExecutionError};
use serde::{Deserialize, Serialize};

/// A stable, model-facing identifier for a comment in one agent turn.
///
/// These identifiers are deliberately scoped to a turn. The application should
/// pair them with its canonical comments when constructing the prompt instead of
/// copying comment bodies into this crate.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct CommentId(String);

impl CommentId {
    fn for_index(index: usize) -> Self {
        Self(format!("comment_{}", index + 1))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CommentId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// An agent response recorded for a comment in the current turn.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CommentResponse {
    pub comment_id: CommentId,
    pub response: String,
}

#[derive(Default)]
struct TurnCommentsState {
    responses: Vec<CommentResponse>,
    responded_to: HashSet<CommentId>,
}

/// Shared state for the comments attached to one agent turn.
///
/// Keep this in the application and pass an [`Arc`] clone to
/// [`RespondToComment`]. The tool then mutates the same response store rather
/// than owning a copy. Only generated aliases are stored here; comment bodies
/// remain in the application's canonical message model.
pub struct TurnComments {
    comment_ids: Vec<CommentId>,
    state: Mutex<TurnCommentsState>,
}

impl TurnComments {
    /// Creates aliases `comment_1` through `comment_{comment_count}`.
    pub fn new(comment_count: usize) -> Self {
        Self {
            comment_ids: (0..comment_count).map(CommentId::for_index).collect(),
            state: Mutex::new(TurnCommentsState::default()),
        }
    }

    /// Returns aliases in the same order as the comments supplied by the app.
    pub fn comment_ids(&self) -> &[CommentId] {
        &self.comment_ids
    }

    /// Returns a snapshot of responses in tool-call order.
    pub fn responses(&self) -> Result<Vec<CommentResponse>, CommentToolError> {
        Ok(self
            .state
            .lock()
            .map_err(|_| CommentToolError::StoreUnavailable)?
            .responses
            .clone())
    }

    /// Returns aliases that have not received a response, in comment order.
    pub fn unanswered_comment_ids(&self) -> Result<Vec<CommentId>, CommentToolError> {
        let state = self
            .state
            .lock()
            .map_err(|_| CommentToolError::StoreUnavailable)?;
        Ok(self
            .comment_ids
            .iter()
            .filter(|comment_id| !state.responded_to.contains(*comment_id))
            .cloned()
            .collect())
    }

    /// Whether every comment in this turn has received a response.
    pub fn is_complete(&self) -> Result<bool, CommentToolError> {
        Ok(self.unanswered_comment_ids()?.is_empty())
    }

    fn record_response(
        &self,
        comment_id: &str,
        response: String,
    ) -> Result<CommentId, CommentToolError> {
        if response.trim().is_empty() {
            return Err(CommentToolError::EmptyResponse);
        }

        let comment_id = self
            .comment_ids
            .iter()
            .find(|candidate| candidate.as_str() == comment_id)
            .cloned()
            .ok_or_else(|| CommentToolError::UnknownComment(comment_id.to_owned()))?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| CommentToolError::StoreUnavailable)?;

        if !state.responded_to.insert(comment_id.clone()) {
            return Err(CommentToolError::AlreadyResponded(comment_id));
        }

        state.responses.push(CommentResponse {
            comment_id: comment_id.clone(),
            response,
        });
        Ok(comment_id)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum CommentToolError {
    UnknownComment(String),
    AlreadyResponded(CommentId),
    EmptyResponse,
    StoreUnavailable,
}

impl fmt::Display for CommentToolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownComment(comment_id) => {
                write!(formatter, "unknown comment id `{comment_id}`")
            }
            Self::AlreadyResponded(comment_id) => {
                write!(
                    formatter,
                    "a response for `{comment_id}` was already recorded"
                )
            }
            Self::EmptyResponse => formatter.write_str("the comment response cannot be empty"),
            Self::StoreUnavailable => {
                formatter.write_str("the comment response store is unavailable")
            }
        }
    }
}

impl std::error::Error for CommentToolError {}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct RespondToCommentArgs {
    /// One of the comment_ids listed with the inline comments, for example comment_1. Never invent one.
    pub comment_id: String,
    /// Your reply to that comment, shown beside it
    pub response: String,
}

#[derive(Debug, Serialize)]
pub struct CommentResponseRecorded {
    pub comment_id: String,
    pub recorded: bool,
}

/// Records an agent's response to one of the comments in the current turn.
pub struct RespondToComment {
    comments: Arc<TurnComments>,
}

impl RespondToComment {
    pub fn new(comments: Arc<TurnComments>) -> Self {
        Self { comments }
    }

    pub fn comments(&self) -> &Arc<TurnComments> {
        &self.comments
    }
}

impl Tool for RespondToComment {
    const NAME: &'static str = "respond_to_comment";
    type Error = CommentToolError;
    type Args = RespondToCommentArgs;
    type Output = CommentResponseRecorded;

    fn description(&self) -> String {
        "Reply to an inline comment: a note a participant attached to an excerpt of an earlier assistant message. \
         Inline comments only exist when the current user message begins with a list of them, each with a comment_id such as comment_1. \
         Call this once for each listed comment_id, and never otherwise. \
         Ordinary messages from participants, including questions and requests, are not comments: answer them in your normal reply."
            .into()
    }

    fn parameters(&self) -> serde_json::Value {
        schemars::schema_for!(RespondToCommentArgs).into()
    }

    fn map_error(&self, error: Self::Error) -> ToolExecutionError {
        match error {
            CommentToolError::UnknownComment(_) => ToolExecutionError::not_found(error.to_string()),
            CommentToolError::AlreadyResponded(_) | CommentToolError::EmptyResponse => {
                ToolExecutionError::invalid_args(error.to_string())
            }
            CommentToolError::StoreUnavailable => ToolExecutionError::other(error.to_string()),
        }
    }

    async fn call(
        &self,
        _context: &mut ToolContext,
        args: Self::Args,
    ) -> Result<Self::Output, Self::Error> {
        if self.comments.comment_ids().is_empty() {
            return Ok(CommentResponseRecorded {
                comment_id: args.comment_id,
                recorded: false,
            });
        }

        let comment_id = self
            .comments
            .record_response(&args.comment_id, args.response)?;
        Ok(CommentResponseRecorded {
            comment_id: comment_id.to_string(),
            recorded: true,
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn schema_and_argument_names_match() {
        let tool = RespondToComment::new(Arc::new(TurnComments::new(1)));
        let args: RespondToCommentArgs = serde_json::from_value(json!({
            "comment_id": "comment_1", "response": "Reply"
        }))
        .unwrap();
        assert_eq!(args.comment_id, "comment_1");
        assert_eq!(args.response, "Reply");
        assert!(
            serde_json::from_value::<RespondToCommentArgs>(json!({
                "comment_id": "comment_1"
            }))
            .is_err()
        );
        let parameters = tool.parameters();
        assert_eq!(parameters["type"], "object");
        assert_eq!(parameters["required"], json!(["comment_id", "response"]));
        assert_eq!(parameters["additionalProperties"], false);
        let properties = &parameters["properties"];
        assert_eq!(properties["comment_id"]["type"], "string");
        assert_eq!(
            properties["comment_id"]["description"],
            "One of the comment_ids listed with the inline comments, for example comment_1. Never invent one."
        );
        assert_eq!(properties["response"]["type"], "string");
        assert_eq!(
            properties["response"]["description"],
            "Your reply to that comment, shown beside it"
        );
    }

    #[test]
    fn stale_calls_are_ignored_when_the_turn_has_no_comments() {
        let comments = Arc::new(TurnComments::new(0));
        let tool = RespondToComment::new(comments.clone());
        let result = futures::executor::block_on(tool.call(
            &mut ToolContext::new(),
            RespondToCommentArgs {
                comment_id: "comment_1".into(),
                response: "Stale response".into(),
            },
        ))
        .unwrap();

        assert!(!result.recorded);
        assert_eq!(result.comment_id, "comment_1");
        assert!(comments.responses().unwrap().is_empty());
    }

    #[test]
    fn creates_predictable_turn_local_ids() {
        let comments = TurnComments::new(3);

        assert_eq!(
            comments
                .comment_ids()
                .iter()
                .map(CommentId::as_str)
                .collect::<Vec<_>>(),
            ["comment_1", "comment_2", "comment_3"]
        );
    }

    #[test]
    fn records_responses_in_call_order() {
        let comments = TurnComments::new(2);

        comments
            .record_response("comment_2", "Second response".into())
            .unwrap();
        comments
            .record_response("comment_1", "First response".into())
            .unwrap();

        assert_eq!(
            comments.responses().unwrap(),
            vec![
                CommentResponse {
                    comment_id: CommentId::for_index(1),
                    response: "Second response".into(),
                },
                CommentResponse {
                    comment_id: CommentId::for_index(0),
                    response: "First response".into(),
                },
            ]
        );
    }

    #[test]
    fn reports_unanswered_comments_in_comment_order() {
        let comments = TurnComments::new(3);
        comments
            .record_response("comment_2", "Second response".into())
            .unwrap();

        assert!(!comments.is_complete().unwrap());
        assert_eq!(
            comments.unanswered_comment_ids().unwrap(),
            vec![CommentId::for_index(0), CommentId::for_index(2)]
        );

        comments
            .record_response("comment_1", "First response".into())
            .unwrap();
        comments
            .record_response("comment_3", "Third response".into())
            .unwrap();
        assert!(comments.is_complete().unwrap());
    }

    #[test]
    fn rejects_unknown_duplicate_and_empty_responses() {
        let comments = TurnComments::new(1);

        assert_eq!(
            comments.record_response("comment_2", "Response".into()),
            Err(CommentToolError::UnknownComment("comment_2".into()))
        );
        assert_eq!(
            comments.record_response("comment_1", "  ".into()),
            Err(CommentToolError::EmptyResponse)
        );

        comments
            .record_response("comment_1", "Response".into())
            .unwrap();
        assert_eq!(
            comments.record_response("comment_1", "Another response".into()),
            Err(CommentToolError::AlreadyResponded(CommentId::for_index(0)))
        );
    }
}
