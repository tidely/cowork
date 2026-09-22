use std::{
    collections::HashSet,
    fmt,
    sync::{Arc, Mutex},
};

use rig::tool::{Tool, ToolContext, ToolExecutionError};
use serde::{Deserialize, Serialize};
use serde_json::json;

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

#[derive(Debug, Deserialize)]
pub struct RespondToCommentArgs {
    pub comment_id: String,
    pub response: String,
}

#[derive(Debug, Serialize)]
pub struct CommentResponseRecorded {
    pub comment_id: CommentId,
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
        "Record a response to a user comment from the current turn. Call this once for each comment, using the comment_id shown in the prompt.".into()
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "comment_id": {
                    "type": "string",
                    "description": "The turn-local comment identifier, for example comment_1"
                },
                "response": {
                    "type": "string",
                    "description": "The response to the comment"
                }
            },
            "required": ["comment_id", "response"],
            "additionalProperties": false
        })
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
        let comment_id = self
            .comments
            .record_response(&args.comment_id, args.response)?;
        Ok(CommentResponseRecorded {
            comment_id,
            recorded: true,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
