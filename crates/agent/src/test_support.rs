//! Helpers for tests that script a run's events by hand.

use rig::{
    operation::{AdapterOutput, Completion},
    wire::Sink,
};

use crate::AgentEvent;

/// Makes hand-built events canonical, as Rig's stream makes a provider's:
/// each turn's model events go through Rig's completion sink, so every block
/// end carries the block it finalized, and the blocks still open when the
/// turn ends are closed before its `TurnEnded`. Other events pass through.
///
/// A prefix of the events makes the same prefix of what this returns, so a
/// test can stop partway through a turn.
///
/// # Panics
///
/// If Rig rejects an event, as it does a malformed complete tool input.
pub fn canonical(events: impl IntoIterator<Item = AgentEvent>) -> Vec<AgentEvent> {
    let mut sink = AdapterOutput::new();
    let mut canonical = Vec::new();
    let drain = |sink: &mut AdapterOutput, canonical: &mut Vec<AgentEvent>| {
        canonical.extend(
            sink.drain()
                .map(|event| AgentEvent::Model(event.expect("a valid model event"))),
        );
    };
    for event in events {
        match event {
            AgentEvent::Model(event) => {
                sink.push(Ok(event));
                drain(&mut sink, &mut canonical);
            }
            AgentEvent::TurnEnded { .. } => {
                Sink::<Completion>::finish(&mut sink);
                drain(&mut sink, &mut canonical);
                sink = AdapterOutput::new();
                canonical.push(event);
            }
            AgentEvent::ToolResult { .. } => canonical.push(event),
        }
    }
    canonical
}
