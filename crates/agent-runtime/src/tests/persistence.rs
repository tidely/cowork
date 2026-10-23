use super::*;
use crate::persistence::{JsonlLog, LogError, load, record::StoredEventKind};

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

fn model_context(directory: &Directory) -> Vec<Message> {
    std::fs::read_to_string(directory.0.join("chat_history.jsonl"))
        .unwrap()
        .lines()
        .map(|line| {
            serde_json::from_str::<language_model::Envelope>(line)
                .unwrap()
                .decode_message()
                .unwrap()
        })
        .collect()
}

#[tokio::test]
async fn transcript_and_tool_elements_survive_compaction_and_model_context_file_rebuild() {
    let directory = Directory::new();
    let Fixture {
        session,
        handle,
        mut requests,
        mut invocations,
        ..
    } = super::compaction::seeded(false);
    let log = JsonlLog::create(&directory.0, session.conversation(), 0)
        .await
        .unwrap();
    let driver = session.run(|event| log.record(&event).unwrap());
    let controller = async move {
        let send = |kind| handle.send(Request::new("human".into(), kind));
        send(input("new task", Delivery::Queue)).await.unwrap();
        let request = recv(&mut requests).await;
        let events = vec![
            Ok(StreamEvent::Delta {
                index: 0,
                delta: ContentDelta::Text("preview of a tool call".into()),
            }),
            Ok(StreamEvent::Finished(Box::new(Completion {
                message: calls(vec![call()]),
            }))),
        ];
        assert!(request.response.send(stream::iter(events).boxed()).is_ok());
        let invocation = recv(&mut invocations).await;
        let progress = ToolStreamItem::Progress(ToolProgress::Custom {
            kind: "card".into(),
            value: json!({"path":"old-element"}),
        });
        let finished = ToolStreamItem::Finished(Ok(json!({"done": true})));
        assert!(
            invocation
                .response
                .send(stream::iter([progress, finished]).boxed())
                .is_ok()
        );
        respond(recv(&mut requests).await, text("done"));
        let compact = send(RequestKind::Compact);
        let answer = async {
            respond(recv(&mut requests).await, text("summary"));
        };
        let (compacted, ()) = futures::join!(compact, answer);
        assert!(matches!(compacted, Ok(Response::Compacted(_))));
        send(input("continue after compaction", Delivery::Queue))
            .await
            .unwrap();
        let next = recv(&mut requests).await;
        assert!(matches!(
            &next.request.messages[0].content[..],
            [Content::Text { text }] if text.contains("summary")
        ));
        respond(next, text("continued"));
        drop(handle);
    };
    let (result, ()) = tokio::time::timeout(WAIT, async { futures::join!(driver, controller) })
        .await
        .unwrap();
    log.finish().await.unwrap();
    let loaded = load::<String>(&directory.0).await.unwrap();
    // Entries come back with their origins.
    assert_eq!(loaded.conversation.entries(), result.conversation.entries());
    assert_eq!(
        loaded.conversation.model_messages(),
        result.conversation.model_messages()
    );
    assert_eq!(loaded.conversation.compactions().len(), 1);
    for expected in ["preview", "tool_started", "tool_progress", "tool_finished"] {
        assert!(loaded.updates.iter().any(
            |event| matches!(&event.event, StoredEventKind::Observation { name, .. } if name == expected)
        ));
    }
    assert!(loaded.updates.iter().any(|event| matches!(
        &event.event,
        StoredEventKind::Observation { name, data }
            if name == "tool_progress" && data["progress"]["value"]["path"] == "old-element"
    )));

    // The context file is only a projection. Opening repairs it from the full log.
    tokio::fs::write(
        directory.0.join("chat_history.jsonl"),
        b"stale projection\n",
    )
    .await
    .unwrap();
    let (log, reopened) = JsonlLog::open::<String>(&directory.0).await.unwrap();
    let context = model_context(&directory);
    assert_eq!(context, result.conversation.model_messages());
    let mut replica = reopened.initial_conversation.clone();
    for event in &reopened.updates {
        event.apply(&mut replica).unwrap();
    }
    assert_eq!(replica.entries(), reopened.conversation.entries());
    assert_eq!(
        replica.model_messages(),
        reopened.conversation.model_messages()
    );

    let Fixture {
        session,
        handle,
        mut requests,
        ..
    } = fixture(Mode::Allow);
    let session = session
        .with_conversation(reopened.conversation.clone(), reopened.next_sequence)
        .unwrap();
    let driver = session.run(|event| log.record(&event).unwrap());
    let controller = async move {
        handle
            .send(Request::new(
                "human".into(),
                input("resumed", Delivery::Queue),
            ))
            .await
            .unwrap();
        let request = recv(&mut requests).await;
        // Delivered context isn't stored: the resumed session tells it again.
        assert_eq!(&request.request.messages[..context.len()], &context);
        respond(request, text("resumed answer"));
        drop(handle);
    };
    let (resumed, ()) = tokio::time::timeout(WAIT, async { futures::join!(driver, controller) })
        .await
        .unwrap();
    log.finish().await.unwrap();
    assert_eq!(
        load::<String>(&directory.0)
            .await
            .unwrap()
            .conversation
            .entries(),
        resumed.conversation.entries()
    );
}

#[tokio::test]
async fn torn_tail_can_be_reopened_but_unknown_schemas_are_preserved_as_errors() {
    use tokio::io::AsyncWriteExt;
    let directory = Directory::new();
    let conversation = super::compaction::conversation(&super::compaction::history());
    let log = JsonlLog::create(&directory.0, &conversation, 0)
        .await
        .unwrap();
    log.record(&AgentEvent::<Host> {
        sequence: 0,
        run: None,
        kind: EventKind::Preview {
            index: 0,
            delta: ContentDelta::Text("unfinished".into()),
        },
    })
    .unwrap();
    log.finish().await.unwrap();
    let updates = directory.0.join("updates.jsonl");
    // Flushed before returning: a tokio `File` writes on a background thread,
    // so `write_all` alone may return before the bytes reach the file, and
    // the reads below would race the append.
    let append = |bytes: Vec<u8>| {
        let updates = updates.clone();
        async move {
            let mut file = tokio::fs::OpenOptions::new()
                .append(true)
                .open(&updates)
                .await
                .unwrap();
            file.write_all(&bytes).await.unwrap();
            file.flush().await.unwrap();
        }
    };
    append(b"{\"type_id\":".to_vec()).await;
    assert!(
        load::<String>(&directory.0)
            .await
            .unwrap()
            .ignored_torn_tail
    );
    let (log, loaded) = JsonlLog::open::<String>(&directory.0).await.unwrap();
    assert_eq!(loaded.conversation.entries(), conversation.entries());
    log.record(&AgentEvent::<Host> {
        sequence: loaded.next_sequence,
        run: None,
        kind: EventKind::RunStarted,
    })
    .unwrap();
    log.finish().await.unwrap();
    assert!(
        !load::<String>(&directory.0)
            .await
            .unwrap()
            .ignored_torn_tail
    );
    let unknown = serde_json::to_string(&language_model::Envelope {
        type_id: "cowork.agent.log.v99".into(),
        payload: json!({"future":"keep me"}),
    })
    .unwrap()
        + "\n";
    append(unknown.into_bytes()).await;
    let before = tokio::fs::read(&updates).await.unwrap();
    assert!(matches!(
        JsonlLog::open::<String>(&directory.0).await,
        Err(LogError::Unsupported(_))
    ));
    assert_eq!(tokio::fs::read(updates).await.unwrap(), before);
}

#[tokio::test]
async fn background_write_failure_is_reported_on_finish() {
    let directory = Directory::new();
    let log = JsonlLog::create(&directory.0, &Conversation::<String>::new(), 0)
        .await
        .unwrap();
    let context = directory.0.join("chat_history.jsonl");
    tokio::fs::remove_file(&context).await.unwrap();
    tokio::fs::create_dir(&context).await.unwrap();
    log.record(&AgentEvent::<Host> {
        sequence: 0,
        run: None,
        kind: EventKind::MessageCommitted {
            index: 0,
            entry: Entry {
                message: Message::user("accepted in memory"),
                origin: Origin::Input {
                    id: InputId::new(),
                    source: "human".into(),
                },
            },
        },
    })
    .unwrap();
    assert!(matches!(log.finish().await, Err(LogError::Io(_))));
}
