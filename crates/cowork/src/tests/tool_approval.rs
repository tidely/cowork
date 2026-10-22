use agent::model::ToolCallId;

use super::*;

/// A run of the host's thread waiting for approval of a non-allowlisted call.
struct WaitingCall {
    message_id: Uuid,
    call: ToolCallId,
}

/// Starts a run on the host whose reply calls a non-allowlisted tool, and has
/// it wait for approval as its session would, until everyone sees it waiting.
fn wait_for_approval(session: &mut Collaboration<'_>) -> WaitingCall {
    wait_for_approval_of(
        session,
        "approval_test_tool",
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
    let mut host = ScriptedHost::default();
    let mut events = host.start(message_id, "1 + 2?");
    events.extend(host.turn(message_id, [streamed_tool_call("c", tool, arguments)], 10));
    let call = host.pending_calls()[0].id.clone();
    session.host.update(session.cx, |cowork, cx| {
        cowork.active_generations.insert(
            thread_id,
            ActiveGeneration::for_test(message_id, Arc::default()),
        );
        host_thread.update(cx, |thread, cx| {
            for event in events {
                thread.emit_for_test(event, cx);
            }
        });
        cowork.await_tool_approval(thread_id, message_id, call.clone(), tool, cx);
    });
    let mirror = session.collaborator_thread().expect("joined");
    session.wait_until("the mirror sees the call waiting", |this| {
        awaiting(&mirror, this.cx).as_ref() == Some(&call)
    });
    WaitingCall { message_id, call }
}

/// The call the thread's last agent message waits for approval of.
fn awaiting(thread: &Entity<Thread>, cx: &mut gpui::VisualTestContext) -> Option<ToolCallId> {
    thread.read_with(cx, |thread, _| {
        thread.timeline.iter().rev().find_map(|entry| match entry {
            TimelineMessage::Agent(message) => Some(message.record.awaiting_approval.clone()),
            TimelineMessage::User(_) => None,
        })?
    })
}

/// The decisions the host handed its run.
fn decisions(session: &mut Collaboration<'_>) -> Vec<(ToolCallId, bool)> {
    let thread_id = session
        .host_thread
        .read_with(session.cx, |thread, _| thread.instance_id);
    session.host.read_with(session.cx, |cowork, _| {
        cowork.active_generations[&thread_id]
            .decisions_for_test()
            .to_vec()
    })
}

#[gpui::test]
fn only_admins_decide_a_waiting_call(cx: &mut gpui::TestAppContext) {
    let mut session = Collaboration::start(cx);
    let mirror = session.collaborator_thread().expect("joined");
    let waiting = wait_for_approval(&mut session);
    let message_id = waiting.message_id;

    // Neither the mirror's guard nor the host lets a `Write` peer decide.
    let request = protocol::CollaboratorMessage::DecideToolCall {
        message_id: waiting.message_id.into_bytes(),
        call: waiting.call.clone(),
        allow: true,
    };
    mirror.read_with(session.cx, |thread, _| {
        assert!(!thread.can_approve_tools());
        assert!(!thread.request(request.clone()));
        assert!(request_unchecked(thread, request));
    });
    session.settle();
    assert!(decisions(&mut session).is_empty(), "the run still waits");
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
    assert_eq!(decisions(&mut session), [(waiting.call, false)]);
}

/// A command runs on the host's computer, so not even an admin may allow it.
#[gpui::test]
fn only_the_host_decides_a_command(cx: &mut gpui::TestAppContext) {
    let mut session = Collaboration::start(cx);
    set_default(&mut session, PeerMode::Admin);
    let waiting = wait_for_approval_of(
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
                call: waiting.call.clone(),
                allow: true,
            },
        ));
    });
    session.settle();
    assert!(decisions(&mut session).is_empty(), "the run still waits");
    let host_thread = session.host_thread.clone();
    assert_eq!(
        awaiting(&host_thread, session.cx),
        Some(waiting.call.clone())
    );

    // The host allows it from its card.
    let allow = session
        .cx
        .debug_bounds(selector("allow"))
        .expect("the host is offered Allow");
    session
        .cx
        .simulate_click(allow.center(), gpui::Modifiers::default());
    session.wait_until("everyone sees the call decided", |this| {
        awaiting(&host_thread, this.cx).is_none() && awaiting(&mirror, this.cx).is_none()
    });
    assert_eq!(decisions(&mut session), [(waiting.call, true)]);
}

/// Someone joining while a call waits sees it waiting, and a host cannot
/// claim a call waits that the run is not waiting on.
#[gpui::test]
fn waiting_calls_reach_joiners_and_are_checked(cx: &mut gpui::TestAppContext) {
    let mut session = Collaboration::start(cx);
    let waiting = wait_for_approval(&mut session);
    let welcome = session
        .host_thread
        .read_with(session.cx, |host, _| host.welcome_for_test());
    let joined = session
        .cx
        .update(|_, cx| cx.new(|cx| Thread::join_for_test(welcome.clone(), cx)));
    assert_eq!(awaiting(&joined, session.cx), Some(waiting.call.clone()));

    let elsewhere = ToolCallId("elsewhere".into());
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
}
