use agent::ToolDecision;
use rig::message::CallId;
use tokio::sync::oneshot;

use super::*;

/// A run of the host's thread waiting for approval of a `calculate` call.
struct WaitingCall {
    message_id: Uuid,
    call: CallId,
    decision: oneshot::Receiver<ToolDecision>,
    task: tokio::task::JoinHandle<()>,
}

/// Starts a run on the host whose reply calls `calculate`, and has it wait
/// for approval as the agent's hook would, until everyone sees it waiting.
fn wait_for_approval(session: &mut Collaboration<'_>) -> WaitingCall {
    wait_for_approval_of(
        session,
        "calculate",
        serde_json::json!({"operation": "add", "a": 1, "b": 2}),
    )
}

/// Like [`wait_for_approval`], for a call of `tool` with `arguments`.
fn wait_for_approval_of(
    session: &mut Collaboration<'_>,
    tool: &str,
    arguments: serde_json::Value,
) -> WaitingCall {
    let host_thread = session.host_thread.clone();
    let thread_id = host_thread.read_with(session.cx, |thread, _| thread.instance_id);
    let message_id = Uuid::new_v4();
    let call = CallId::from_wire("c");
    let task = session._runtime.spawn(std::future::pending::<()>());
    let (decide, decision) = oneshot::channel();
    session.host.update(session.cx, |cowork, cx| {
        cowork.active_generations.insert(
            thread_id,
            ActiveGeneration::for_test(message_id, task.abort_handle(), Arc::default()),
        );
        host_thread.update(cx, |thread, cx| {
            thread.emit_for_test(agent_started(message_id, None, "1 + 2?"), cx);
            let turn = model_turn([streamed_tool_call("c", tool, arguments)], 10);
            for event in turn {
                thread.emit_for_test(agent_event(message_id, event), cx);
            }
        });
        cowork.await_tool_approval(thread_id, message_id, call.clone(), tool, decide, cx);
    });
    let mirror = session.collaborator_thread().expect("joined");
    session.wait_until("the mirror sees the call waiting", |this| {
        awaiting(&mirror, this.cx).as_ref() == Some(&call)
    });
    WaitingCall {
        message_id,
        call,
        decision,
        task,
    }
}

/// The call the thread's last agent message waits for approval of.
fn awaiting(thread: &Entity<Thread>, cx: &mut gpui::VisualTestContext) -> Option<CallId> {
    thread.read_with(cx, |thread, _| {
        thread.timeline.iter().rev().find_map(|entry| match entry {
            TimelineMessage::Agent(message) => Some(message.awaiting_approval.clone()),
            TimelineMessage::User(_) => None,
        })?
    })
}

#[gpui::test]
fn only_admins_decide_a_waiting_call(cx: &mut gpui::TestAppContext) {
    let mut session = Collaboration::start(cx);
    let mirror = session.collaborator_thread().expect("joined");
    let mut waiting = wait_for_approval(&mut session);
    let message_id = waiting.message_id;
    let selector = move |part: &str| -> &'static str {
        Box::leak(format!("tool-approval-{message_id}-0-{part}").into_boxed_str())
    };

    // The host may decide; the collaborator, joined as `Write`, waits.
    assert!(session.cx.debug_bounds(selector("allow")).is_some());
    assert!(session.cx.debug_bounds(selector("waiting")).is_some());

    // Neither the mirror's guard nor the host lets a `Write` peer decide.
    let request = protocol::CollaboratorMessage::DecideToolCall {
        message_id: waiting.message_id.into_bytes(),
        call: protocol::Json::call_id(&waiting.call),
        allow: true,
    };
    mirror.read_with(session.cx, |thread, _| {
        assert!(!thread.can_approve_tools());
        assert!(!thread.request(request.clone()));
        assert!(request_unchecked(thread, request));
    });
    session.settle();
    assert!(waiting.decision.try_recv().is_err(), "the run still waits");
    let host_thread = session.host_thread.clone();
    assert_eq!(
        awaiting(&host_thread, session.cx),
        Some(waiting.call.clone())
    );
    assert!(
        session.collaborator_thread().is_some(),
        "permission denial is not a protocol disconnect"
    );

    set_default(&mut session, PeerMode::Admin);
    let call = waiting.call.clone();
    session.collaborator.update(session.cx, |cowork, cx| {
        cowork.decide_tool_call(message_id, call, false, cx);
    });
    session.wait_until("everyone sees the call decided", |this| {
        awaiting(&host_thread, this.cx).is_none() && awaiting(&mirror, this.cx).is_none()
    });
    assert!(matches!(
        waiting.decision.try_recv(),
        Ok(ToolDecision::Deny { .. })
    ));

    // A decision that comes too late changes nothing.
    let call = waiting.call.clone();
    session.host.update(session.cx, |cowork, cx| {
        cowork.decide_tool_call(message_id, call, true, cx);
    });
    waiting.task.abort();
}

/// A command runs on the host's computer, so not even an admin may allow it.
#[gpui::test]
fn only_the_host_decides_a_command(cx: &mut gpui::TestAppContext) {
    let mut session = Collaboration::start(cx);
    set_default(&mut session, PeerMode::Admin);
    let mut waiting = wait_for_approval_of(
        &mut session,
        "run_command",
        serde_json::json!({"command": "ls"}),
    );
    let message_id = waiting.message_id;
    let selector = move |part: &str| -> &'static str {
        Box::leak(format!("tool-approval-{message_id}-0-{part}").into_boxed_str())
    };

    // The host is offered Allow; the admin waits. Both see the command.
    assert!(session.cx.debug_bounds(selector("allow")).is_some());
    assert!(session.cx.debug_bounds(selector("waiting")).is_some());
    assert!(session.cx.debug_bounds(selector("command")).is_some());

    // An admin who asks anyway is ignored by the host.
    let mirror = session.collaborator_thread().expect("joined");
    mirror.read_with(session.cx, |thread, _| {
        assert!(thread.can_approve_tools(), "an admin approves other tools");
        assert!(request_unchecked(
            thread,
            protocol::CollaboratorMessage::DecideToolCall {
                message_id: message_id.into_bytes(),
                call: protocol::Json::call_id(&waiting.call),
                allow: true,
            },
        ));
    });
    session.settle();
    assert!(waiting.decision.try_recv().is_err(), "the run still waits");
    let host_thread = session.host_thread.clone();
    assert_eq!(
        awaiting(&host_thread, session.cx),
        Some(waiting.call.clone())
    );

    let call = waiting.call.clone();
    session.host.update(session.cx, |cowork, cx| {
        cowork.decide_tool_call(message_id, call, true, cx);
    });
    session.wait_until("everyone sees the call decided", |this| {
        awaiting(&host_thread, this.cx).is_none() && awaiting(&mirror, this.cx).is_none()
    });
    assert_eq!(waiting.decision.try_recv(), Ok(ToolDecision::Allow));
    waiting.task.abort();
}

#[gpui::test]
fn the_host_allows_a_call_from_its_card(cx: &mut gpui::TestAppContext) {
    let mut session = Collaboration::start(cx);
    let mut waiting = wait_for_approval(&mut session);
    let allow = session
        .cx
        .debug_bounds(Box::leak(
            format!("tool-approval-{}-0-allow", waiting.message_id).into_boxed_str(),
        ))
        .expect("the host is offered Allow");
    session
        .cx
        .simulate_click(allow.center(), gpui::Modifiers::default());
    let mirror = session.collaborator_thread().expect("joined");
    session.wait_until("the mirror sees the call decided", |this| {
        awaiting(&mirror, this.cx).is_none()
    });
    assert_eq!(waiting.decision.try_recv(), Ok(ToolDecision::Allow));
    waiting.task.abort();
}

/// Someone joining while a call waits sees it waiting, and a host cannot
/// claim a call waits that the run is not waiting on.
#[gpui::test]
fn waiting_calls_reach_joiners_and_are_checked(cx: &mut gpui::TestAppContext) {
    let mut session = Collaboration::start(cx);
    let waiting = wait_for_approval(&mut session);
    let welcome = session
        .host_thread
        .read_with(session.cx, |host, _| protocol::Welcome {
            draft_generation: 0,
            participant_id: ParticipantId::new().into_bytes(),
            thread: host.to_protocol(),
            draft: host.draft().encode_state(),
            presence: Vec::new(),
            stored_attachments: Vec::new(),
        });
    let joined = session.cx.update(|_, cx| {
        cx.new(|cx| {
            Thread::from_welcome(
                welcome.clone(),
                ThreadDraft::new(ParticipantId::new()),
                ThreadSharing::NotShared,
                cx,
            )
        })
    });
    assert_eq!(awaiting(&joined, session.cx), Some(waiting.call.clone()));

    let elsewhere = protocol::Json::call_id(&CallId::from_wire("elsewhere"));
    let mut claimed = welcome.clone();
    for message in &mut claimed.thread.messages {
        if let protocol::TimelineMessage::Agent(message) = message {
            message.awaiting_approval = Some(elsewhere.clone());
        }
    }
    assert!(Thread::validate_welcome(&claimed).is_err());

    let mirror = session.collaborator_thread().expect("joined");
    mirror.update(session.cx, |thread, cx| {
        assert!(
            thread
                .try_apply_for_test(
                    protocol::HostMessage::ToolApprovalRequested {
                        id: waiting.message_id.into_bytes(),
                        call: elsewhere,
                    },
                    cx,
                )
                .is_err()
        );
    });
    waiting.task.abort();
}

/// A run that ends while a call waits, as when someone stops it, no longer
/// shows the call waiting.
#[gpui::test]
fn an_ended_run_waits_for_nothing(cx: &mut gpui::TestAppContext) {
    let mut session = Collaboration::start(cx);
    let waiting = wait_for_approval(&mut session);
    let host_thread = session.host_thread.clone();
    host_thread.update(session.cx, |thread, cx| {
        thread.emit_for_test(
            protocol::HostMessage::AgentEnded {
                id: waiting.message_id.into_bytes(),
                outcome: protocol::RunOutcome::Stopped,
                duration: Duration::ZERO,
            },
            cx,
        );
    });
    let mirror = session.collaborator_thread().expect("joined");
    session.wait_until("the mirror sees the run end", |this| {
        awaiting(&mirror, this.cx).is_none()
    });
    assert_eq!(awaiting(&host_thread, session.cx), None);
    waiting.task.abort();
}
