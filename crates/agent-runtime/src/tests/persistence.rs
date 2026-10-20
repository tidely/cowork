use super::*;
use crate::persistence::{JsonlLog, load, record::StoredEventKind};

pub(super) struct Directory(pub(super) std::path::PathBuf);
impl Directory {
    pub(super) fn new() -> Self {
        Self(std::env::temp_dir().join(format!(
            "cowork-agent-log-{}",
            language_model::MessageId::new().0
        )))
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::test]
async fn transcript_and_tool_elements_survive_compaction_and_model_context_file_rebuild() {
    let directory = Directory::new();
    let (provider, mut requests) = test_provider();
    let (executor, mut invocations, _) = test_executor(Arc::new(AllowAll));
    let (session, handle) = compaction::seeded(provider, executor, false);
    let log = JsonlLog::create(&directory.0, session.conversation(), 0)
        .await
        .unwrap();
    let driver = session.run(|event| log.record(&event).unwrap());
    let controller = async move {
        handle
            .submit(input("human", "new task"), Delivery::Queue)
            .await
            .unwrap();
        let request = recv(&mut requests).await;
        let events = vec![
            Ok(StreamEvent::Delta {
                index: 0,
                delta: ContentDelta::Text("preview of a tool call".into()),
            }),
            Ok(StreamEvent::Finished(Box::new(Completion {
                message: reply(vec![Content::ToolCall(call())], "tool_calls"),
            }))),
        ];
        assert!(request.response.send(stream::iter(events).boxed()).is_ok());
        let invocation = recv(&mut invocations).await;
        assert!(
            invocation
                .response
                .send(
                    stream::iter([
                        ToolStreamItem::Progress(ToolProgress::Custom {
                            kind: "card".into(),
                            value: serde_json::json!({"path":"old-element"})
                        }),
                        ToolStreamItem::Finished(Ok(output())),
                    ])
                    .boxed()
                )
                .is_ok()
        );
        respond(
            recv(&mut requests).await,
            reply(vec![Content::text("done")], "stop"),
        );
        let commands = handle.clone();
        let compact = tokio::spawn(async move { commands.compact().await });
        respond(
            recv(&mut requests).await,
            reply(vec![Content::text("summary")], "stop"),
        );
        compact.await.unwrap().unwrap();
        handle
            .submit(input("human", "continue after compaction"), Delivery::Queue)
            .await
            .unwrap();
        let next = recv(&mut requests).await;
        assert!(matches!(
            &next.request.messages[1].content[..],
            [Content::Text { text }] if text.contains("summary")
        ));
        respond(next, reply(vec![Content::text("continued")], "stop"));
        drop(handle);
    };
    let (result, ()) = tokio::time::timeout(WAIT, async { futures::join!(driver, controller) })
        .await
        .unwrap();
    log.finish().await.unwrap();
    let loaded = load(&directory.0).await.unwrap();
    assert_eq!(
        loaded.conversation.messages(),
        result.conversation.messages()
    );
    assert_eq!(
        loaded.conversation.model_messages(),
        result.conversation.model_messages()
    );
    assert_eq!(loaded.conversation.compactions().len(), 1);
    assert!(loaded.conversation.messages().len() > loaded.conversation.model_messages().len());
    for expected in ["preview", "tool_started", "tool_progress", "tool_finished"] {
        assert!(loaded.updates.iter().any(|event| matches!(&event.event, StoredEventKind::Observation { name, .. } if name == expected)));
    }
    assert!(loaded.updates.iter().any(
        |event| matches!(&event.event, StoredEventKind::Observation { name, data }
        if name == "tool_progress" && data["progress"]["value"]["path"] == "old-element")
    ));
    let old_card = loaded.updates.iter().position(|event| matches!(&event.event, StoredEventKind::Observation { name, .. } if name == "tool_finished")).unwrap();
    let compacted = loaded
        .updates
        .iter()
        .position(|event| matches!(event.event, StoredEventKind::Compacted { .. }))
        .unwrap();
    assert!(old_card < compacted);
    // The context file is only a projection. Opening repairs it from the full log.
    tokio::fs::write(
        directory.0.join("chat_history.jsonl"),
        b"stale projection\n",
    )
    .await
    .unwrap();
    let (log, reopened) = JsonlLog::open(&directory.0).await.unwrap();
    let bytes = tokio::fs::read_to_string(directory.0.join("chat_history.jsonl"))
        .await
        .unwrap();
    let context = bytes
        .lines()
        .map(|line| {
            serde_json::from_str::<language_model::Envelope>(line)
                .unwrap()
                .decode_message()
                .unwrap()
        })
        .collect::<Vec<_>>();
    assert_eq!(context, result.conversation.model_messages());
    assert_eq!(
        reopened.conversation.messages(),
        result.conversation.messages()
    );
    let mut replica = reopened.initial_conversation.clone();
    for event in &reopened.updates {
        event.apply(&mut replica).unwrap();
    }
    assert_eq!(
        replica.model_messages(),
        reopened.conversation.model_messages()
    );
    let (provider, mut requests) = test_provider();
    let (executor, _, _) = test_executor(Arc::new(AllowAll));
    let (session, handle) = test_session(provider, executor);
    let session = session
        .with_conversation(reopened.conversation.clone(), reopened.next_sequence)
        .unwrap();
    let driver = session.run(|event| log.record(&event).unwrap());
    let controller = async move {
        handle
            .submit(input("human", "resumed"), Delivery::Queue)
            .await
            .unwrap();
        let request = recv(&mut requests).await;
        assert_eq!(&request.request.messages[..context.len()], &context);
        respond(
            request,
            reply(vec![Content::text("resumed answer")], "stop"),
        );
        drop(handle);
    };
    let (resumed, ()) = tokio::time::timeout(WAIT, async { futures::join!(driver, controller) })
        .await
        .unwrap();
    log.finish().await.unwrap();
    assert_eq!(
        load(&directory.0).await.unwrap().conversation.messages(),
        resumed.conversation.messages()
    );
}

#[tokio::test]
async fn torn_tail_can_be_reopened_but_unknown_schemas_are_preserved_as_errors() {
    let directory = Directory::new();
    let conversation = Conversation::new(compaction::history()).unwrap();
    let log = JsonlLog::create(&directory.0, &conversation, 0)
        .await
        .unwrap();
    log.record(&AgentEvent::<&str> {
        sequence: 0,
        run: None,
        kind: EventKind::Preview {
            index: 0,
            delta: ContentDelta::Text("unfinished".into()),
        },
    })
    .unwrap();
    log.finish().await.unwrap();
    use tokio::io::AsyncWriteExt;
    let updates = directory.0.join("updates.jsonl");
    tokio::fs::OpenOptions::new()
        .append(true)
        .open(&updates)
        .await
        .unwrap()
        .write_all(b"{\"type_id\":")
        .await
        .unwrap();
    assert!(load(&directory.0).await.unwrap().ignored_torn_tail);
    let (log, loaded) = JsonlLog::open(&directory.0).await.unwrap();
    assert_eq!(loaded.conversation.messages(), conversation.messages());
    log.record(&AgentEvent::<&str> {
        sequence: loaded.next_sequence,
        run: None,
        kind: EventKind::RunStarted,
    })
    .unwrap();
    log.finish().await.unwrap();
    assert!(!load(&directory.0).await.unwrap().ignored_torn_tail);
    let unknown = serde_json::to_string(&language_model::Envelope {
        type_id: "cowork.agent.log.v99".into(),
        payload: serde_json::json!({"future":"keep me"}),
    })
    .unwrap()
        + "\n";
    tokio::fs::OpenOptions::new()
        .append(true)
        .open(&updates)
        .await
        .unwrap()
        .write_all(unknown.as_bytes())
        .await
        .unwrap();
    let before = tokio::fs::read(&updates).await.unwrap();
    assert!(matches!(
        JsonlLog::open(&directory.0).await,
        Err(crate::persistence::LogError::Unsupported(_))
    ));
    assert_eq!(tokio::fs::read(updates).await.unwrap(), before);
}

#[tokio::test]
async fn background_write_failure_is_reported_on_finish() {
    let directory = Directory::new();
    let conversation = Conversation::default();
    let log = JsonlLog::create(&directory.0, &conversation, 0)
        .await
        .unwrap();
    let context = directory.0.join("chat_history.jsonl");
    tokio::fs::remove_file(&context).await.unwrap();
    tokio::fs::create_dir(&context).await.unwrap();
    log.record(&AgentEvent::<&str> {
        sequence: 0,
        run: None,
        kind: EventKind::MessageCommitted {
            index: 0,
            message: Message::user("accepted in memory"),
            input: None,
        },
    })
    .unwrap();
    assert!(matches!(
        log.finish().await,
        Err(crate::persistence::LogError::Io(_))
    ));
}
