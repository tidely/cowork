use super::*;

type TestInbox = Inbox<&'static str, &'static str>;

fn input(source: &'static str, payload: &'static str) -> Input<&'static str, &'static str> {
    Input {
        id: InputId::new(),
        source,
        payload,
        policy: Policy {
            access: QueueAccess::Editable,
            fallback: Fallback::Queue,
            lifetime: Lifetime::Retain,
        },
    }
}

fn admit(inbox: &mut TestInbox, payload: &'static str, delivery: Delivery) -> InputId {
    let input = input("human", payload);
    let id = input.id;
    inbox.admit(input, delivery).unwrap();
    id
}

fn commit(inbox: &mut TestInbox, id: DeliveryId) -> Vec<&'static str> {
    inbox
        .commit(id, |_| Ok::<_, ()>(()))
        .unwrap()
        .unwrap()
        .inputs
        .into_iter()
        .map(|input| input.payload)
        .collect()
}

fn start(inbox: &mut TestInbox) -> RunId {
    let id = inbox.begin_next().unwrap().unwrap();
    commit(inbox, id);
    inbox.run().unwrap()
}

#[test]
fn sources_share_lanes_without_losing_provenance_or_priority() {
    let mut inbox = TestInbox::default();
    admit(&mut inbox, "initial", Delivery::Queue);
    let run = start(&mut inbox);
    inbox
        .admit(input("parent", "steer"), Delivery::Steer)
        .unwrap();
    inbox
        .admit(input("timer", "wake"), Delivery::Interject)
        .unwrap();
    assert_eq!(inbox.signal(), Some(Signal::YieldWait(run)));
    let batch = inbox.begin_safe_point(run).unwrap().unwrap();
    assert_eq!(
        inbox
            .batch(batch)
            .unwrap()
            .inputs
            .iter()
            .map(|input| input.source)
            .collect::<Vec<_>>(),
        ["timer", "parent"]
    );
    // Arrivals during preparation belong to the next safe point.
    admit(&mut inbox, "late", Delivery::Steer);
    assert_eq!(commit(&mut inbox, batch), ["wake", "steer"]);
    assert_eq!(
        inbox.finish_run(run, RunOutcome::Completed),
        Err(StateError::Undelivered)
    );
    let next = inbox.begin_safe_point(run).unwrap().unwrap();
    assert_eq!(commit(&mut inbox, next), ["late"]);
    inbox.finish_run(run, RunOutcome::Completed).unwrap();
}

#[test]
fn interrupt_cancels_and_preserves_fifo_ahead_of_ordinary_work() {
    let mut inbox = TestInbox::default();
    admit(&mut inbox, "initial", Delivery::Queue);
    let run = start(&mut inbox);
    admit(&mut inbox, "ordinary", Delivery::Queue);
    admit(&mut inbox, "urgent one", Delivery::Interrupt);
    admit(&mut inbox, "urgent two", Delivery::Interrupt);
    assert_eq!(inbox.signal(), Some(Signal::Cancel(run)));
    assert_eq!(inbox.begin_safe_point(run).unwrap(), None);
    inbox.finish_run(run, RunOutcome::Stopped).unwrap();
    for expected in ["urgent one", "urgent two", "ordinary"] {
        let id = inbox.begin_next().unwrap().unwrap();
        assert_eq!(commit(&mut inbox, id), [expected]);
        inbox
            .finish_run(inbox.run().unwrap(), RunOutcome::Completed)
            .unwrap();
    }
}

#[test]
fn failed_preparation_keeps_ownership_and_stop_recovers_in_order() {
    let mut inbox = TestInbox::default();
    admit(&mut inbox, "initial", Delivery::Queue);
    let run = start(&mut inbox);
    admit(&mut inbox, "first", Delivery::Steer);
    let batch = inbox.begin_safe_point(run).unwrap().unwrap();
    assert_eq!(
        inbox
            .commit(batch, |_| Err::<(), _>("journal unavailable"))
            .unwrap(),
        Err("journal unavailable")
    );
    admit(&mut inbox, "second", Delivery::Steer);
    admit(&mut inbox, "queued", Delivery::Queue);
    inbox.stop(run).unwrap();
    let mut called = false;
    assert_eq!(
        inbox
            .commit(batch, |_| {
                called = true;
                Ok::<_, ()>(())
            })
            .unwrap_err(),
        StateError::StaleDelivery
    );
    assert!(!called);
    inbox.finish_run(run, RunOutcome::Stopped).unwrap();
    assert!(inbox.begin_next().unwrap().is_none());
    assert_eq!(
        inbox
            .pending()
            .map(|input| input.payload)
            .collect::<Vec<_>>(),
        ["first", "second", "queued"]
    );
    inbox.resume();
    let next = start(&mut inbox);
    assert_eq!(inbox.stop(run), Err(StateError::StaleRun));
    assert_eq!(
        inbox.finish_run(run, RunOutcome::Stopped).unwrap_err(),
        StateError::StaleRun
    );
    assert_eq!(inbox.run(), Some(next));
}

#[test]
fn idle_and_stopping_inputs_follow_their_explicit_fallback() {
    let mut inbox = TestInbox::default();
    let mut notice = input("monitor", "notice");
    notice.policy.fallback = Fallback::Discard;
    let discarded = inbox.admit(notice.clone(), Delivery::Interject).unwrap();
    assert_eq!(discarded.discarded, Some(notice.clone()));
    admit(&mut inbox, "initial", Delivery::Steer);
    let run = start(&mut inbox);
    inbox.admit(notice.clone(), Delivery::Steer).unwrap();
    inbox.stop(run).unwrap();
    admit(&mut inbox, "after stop", Delivery::Steer);
    assert_eq!(
        inbox.finish_run(run, RunOutcome::Stopped).unwrap(),
        [notice]
    );
    assert_eq!(inbox.pending().next().unwrap().payload, "after stop");
}

#[test]
fn reserved_initial_prompt_survives_interrupt_before_commit() {
    let mut inbox = TestInbox::default();
    admit(&mut inbox, "initial", Delivery::Queue);
    let initial = inbox.begin_next().unwrap().unwrap();
    let run = inbox.run().unwrap();
    admit(&mut inbox, "urgent", Delivery::Interrupt);
    inbox.finish_run(run, RunOutcome::Stopped).unwrap();
    assert_eq!(inbox.batch(initial).unwrap_err(), StateError::StaleDelivery);
    let next = inbox.begin_next().unwrap().unwrap();
    assert_eq!(commit(&mut inbox, next), ["urgent"]);
    inbox
        .finish_run(inbox.run().unwrap(), RunOutcome::Completed)
        .unwrap();
    let next = inbox.begin_next().unwrap().unwrap();
    assert_eq!(commit(&mut inbox, next), ["initial"]);
}

#[test]
fn producer_close_invalidates_preparation_without_dropping_other_sources() {
    let mut inbox = TestInbox::default();
    admit(&mut inbox, "initial", Delivery::Queue);
    let run = start(&mut inbox);
    let mut wake = input("task", "completed");
    wake.policy.lifetime = Lifetime::ProducerBound;
    inbox.admit(wake, Delivery::Interject).unwrap();
    admit(&mut inbox, "human", Delivery::Steer);
    let old = inbox.begin_safe_point(run).unwrap().unwrap();
    let withdrawn = inbox.producer_closed(&"task");
    assert_eq!(withdrawn.inputs.len(), 1);
    assert_eq!(withdrawn.invalidated, Some(old));
    assert_eq!(
        inbox.commit(old, |_| Ok::<_, ()>(())).unwrap_err(),
        StateError::StaleDelivery
    );
    assert_eq!(
        commit(&mut inbox, withdrawn.replacement.unwrap()),
        ["human"]
    );
    inbox.finish_run(run, RunOutcome::Completed).unwrap();
}

#[test]
fn withdrawal_of_initial_input_cancels_only_its_unstarted_run() {
    let mut inbox = TestInbox::default();
    let mut wake = input("task", "completed");
    wake.policy.lifetime = Lifetime::ProducerBound;
    inbox.admit(wake, Delivery::Queue).unwrap();
    let batch = inbox.begin_next().unwrap().unwrap();
    let run = inbox.run().unwrap();
    let withdrawn = inbox.producer_closed(&"task");
    assert_eq!(withdrawn.invalidated, Some(batch));
    assert_eq!(withdrawn.replacement, None);
    assert_eq!(inbox.signal(), Some(Signal::Cancel(run)));
    inbox.finish_run(run, RunOutcome::Stopped).unwrap();
    assert_eq!(inbox.pending().count(), 0);
}

#[test]
fn redirect_is_atomic_and_protected_or_reserved_rows_cannot_be_edited() {
    let mut inbox = TestInbox::default();
    let first = admit(&mut inbox, "initial", Delivery::Queue);
    let batch = inbox.begin_next().unwrap().unwrap();
    assert_eq!(inbox.remove(first).unwrap_err(), StateError::NotEditable);
    commit(&mut inbox, batch);
    let run = inbox.run().unwrap();
    let queued = admit(&mut inbox, "follow up", Delivery::Queue);
    inbox.edit(queued, "edited").unwrap();
    assert_eq!(
        inbox.redirect(queued, Delivery::Steer).unwrap().location,
        Admission::Active(run)
    );
    assert_eq!(
        inbox.pending().filter(|input| input.id == queued).count(),
        1
    );
    assert_eq!(inbox.edit(queued, "too late"), Err(StateError::NotEditable));
    let mut protected = input("parent", "protected");
    protected.policy.access = QueueAccess::Protected;
    let id = protected.id;
    inbox.admit(protected, Delivery::Queue).unwrap();
    assert_eq!(inbox.remove(id).unwrap_err(), StateError::NotEditable);
    let batch = inbox.begin_safe_point(run).unwrap().unwrap();
    assert_eq!(commit(&mut inbox, batch), ["edited"]);
}

#[test]
fn duplicate_rejection_returns_payload_and_queue_reordering_preserves_other_rows() {
    let mut inbox = TestInbox::default();
    let a = input("human", "a");
    let a_id = a.id;
    inbox.admit(a.clone(), Delivery::Queue).unwrap();
    let rejection = inbox.admit(a.clone(), Delivery::Interrupt).unwrap_err();
    assert_eq!(rejection.reason, StateError::Duplicate);
    assert_eq!(rejection.input, a);
    let b = admit(&mut inbox, "b", Delivery::Queue);
    inbox.move_before(b, Some(a_id)).unwrap();
    assert_eq!(
        inbox
            .pending()
            .map(|input| input.payload)
            .collect::<Vec<_>>(),
        ["b", "a"]
    );
}

#[test]
fn failure_and_shutdown_pause_work_and_return_discarded_inputs() {
    let mut inbox = TestInbox::default();
    admit(&mut inbox, "retained", Delivery::Queue);
    let run = start(&mut inbox);
    admit(&mut inbox, "follow up", Delivery::Steer);
    inbox.finish_run(run, RunOutcome::Failed).unwrap();
    assert!(inbox.is_paused());
    let mut ephemeral = input("timer", "tick");
    ephemeral.policy.lifetime = Lifetime::Ephemeral;
    inbox.admit(ephemeral.clone(), Delivery::Queue).unwrap();
    assert_eq!(inbox.shutdown().inputs, [ephemeral]);
    assert_eq!(inbox.pending().next().unwrap().payload, "follow up");
    assert!(inbox.begin_next().unwrap().is_none());
}
