//! Submitted comment headers in both disclosure states.

use gpui::AppContext as _;
use uuid::Uuid;

use super::SceneContext;
use crate::{
    participant::ParticipantId,
    thread::ThreadStore,
    thread_draft::ThreadDraft,
    timeline::{CommentReference, TimelineMessage, UserComment, UserCommentBody, UserMessageGroup},
};

pub(super) fn scene(context: &SceneContext) -> anyhow::Result<()> {
    let id = Uuid::new_v4();
    let author = ParticipantId::new();
    let mut stage = context.open(|_, cx| {
        let timeline = [true, false]
            .into_iter()
            .map(|comments_folded| {
                TimelineMessage::User(
                    UserMessageGroup {
                        id: Uuid::new_v4(),
                        comments: vec![UserComment {
                            id: Uuid::new_v4(),
                            author,
                            presence: Default::default(),
                            reference: CommentReference {
                                message_id: Uuid::new_v4(),
                                range: 0..5,
                                quote: "Notes".into(),
                            },
                            body: UserCommentBody::Submitted("Could you clarify this?".into()),
                        }],
                        blocks: Vec::new(),
                        comments_folded,
                    }
                    .into(),
                )
            })
            .collect();
        let thread = cx.new(|_| super::super::test_thread(id, timeline, ThreadDraft::new(author)));
        let store = cx.new(|cx| {
            let mut store = ThreadStore::default();
            store.push_front_active(thread, cx);
            store
        });
        (store, Some(id))
    })?;
    stage.capture("1-folded-and-expanded")?;
    Ok(())
}
