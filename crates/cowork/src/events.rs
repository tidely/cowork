use futures::SinkExt;
use tokio_util::sync::PollSender;

use crate::{
    app::{AgentAddr, AgentEvent, PendingToolPermission, ThreadId, ToolPermissionResponse},
    tui::{RuntimeEvent, RuntimeEventSender},
};

#[derive(Clone)]
pub(crate) struct AgentEventSink {
    events: PollSender<RuntimeEvent>,
    thread_id: ThreadId,
}

impl AgentEventSink {
    pub(crate) fn new(events: RuntimeEventSender, thread_id: ThreadId) -> Self {
        Self {
            events: PollSender::new(events),
            thread_id,
        }
    }

    pub(crate) async fn send(&mut self, event: AgentEvent) {
        let _ = self
            .events
            .send(RuntimeEvent::Agent(self.thread_id, event))
            .await;
    }

    pub(crate) async fn request_tool_permission(
        &mut self,
        addr: AgentAddr,
        id: String,
        name: String,
        arguments: serde_json::Value,
    ) -> Option<ToolPermissionResponse> {
        let (respond_to, response) = tokio::sync::oneshot::channel();
        let request =
            PendingToolPermission::new(self.thread_id, addr, id, name, arguments, respond_to);

        self.events
            .send(RuntimeEvent::ToolPermissionRequest(request))
            .await
            .ok()?;

        response.await.ok()
    }
}
