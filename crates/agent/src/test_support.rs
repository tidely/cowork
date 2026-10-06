//! Helpers for tests that script a run's events.

use futures::StreamExt as _;
use rig::{completion::CompletionRequest, streaming::Item, test_utils::MockCompletionModel};

pub use rig::test_utils::{MockStreamEvent, mock_final_with_total_tokens};

use crate::AgentEvent;

/// The events one model turn reports when the provider streams `script`:
/// each event Rig's stream yields, then the turn's end. The script runs
/// through Rig's mock model, so the events are exactly what Rig emits, each
/// part ending with the content Rig finalized. End the script with a
/// [`MockStreamEvent::FinalResponse`], whose usage the turn's end carries.
///
/// A prefix of the events is what the turn reported so far, so a test can
/// stop partway through it.
///
/// # Panics
///
/// If Rig refuses the script, as it does one that never ends the reply.
pub fn turn(script: impl IntoIterator<Item = MockStreamEvent>) -> Vec<AgentEvent> {
    let model = MockCompletionModel::from_stream_turns([script.into_iter().collect::<Vec<_>>()]);
    // The mock streams without a runtime.
    futures::executor::block_on(async {
        let mut stream = model
            .stream(CompletionRequest::new("turn"))
            .expect("the mock model streams");
        let mut events = Vec::new();
        while let Some(item) = stream.next().await {
            if let Item::Event(event) = item.expect("a scripted event") {
                events.push(AgentEvent::Model(event));
            }
        }
        let response = stream.finish().await.expect("a scripted end");
        let head = response.head();
        events.push(AgentEvent::TurnEnded {
            origin: head.origin,
            stop: head.stop,
            usage: response.usage,
        });
        events
    })
}
