//! Typed actor requests over executor-agnostic channels.
//!
//! Each operation is a command struct implementing [`Action`], sent in an
//! [`Envelope`] with a oneshot reply. The actor's mailbox enum implements
//! `From<Envelope<T>>` for each command. There is no response enum: the reply
//! type lives on the action.
//!
//! Channels come from [`futures`], so this crate does not depend on an async
//! runtime and a handle can be driven by whichever executor the caller is on.
//!
//! [`actor_action`] builds the mailbox for concrete commands. A generic
//! command needs a hand-written [`From`]:
//!
//! ```
//! use actor_handle::{Action, Envelope};
//!
//! struct Cmd<H> {
//!     host: H,
//! }
//!
//! impl<H> Action for Cmd<H> {
//!     type Reply = H;
//! }
//!
//! enum Mailbox<H> {
//!     Cmd(Envelope<Cmd<H>>),
//! }
//!
//! impl<H> From<Envelope<Cmd<H>>> for Mailbox<H> {
//!     fn from(envelope: Envelope<Cmd<H>>) -> Self {
//!         Mailbox::Cmd(envelope)
//!     }
//! }
//!
//! let (reply, response) = futures::channel::oneshot::channel();
//! let Mailbox::Cmd(envelope) = Mailbox::from(Envelope {
//!     message: Cmd { host: 1u8 },
//!     reply,
//! });
//! envelope.reply.send(envelope.message.host).unwrap();
//! assert_eq!(futures::executor::block_on(response).unwrap(), 1);
//! ```

use futures::channel::{mpsc, oneshot};

/// Why [`ActorHandle::request`] or [`ActorHandle::post`] did not complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum DeliveryError {
    /// The actor took the command and dropped the reply sender.
    #[error("actor dropped the reply")]
    ReplyDropped,
    /// The mailbox receiver is gone, so the command was not queued.
    #[error("actor is closed")]
    ActorClosed,
}

/// One operation an actor can perform.
pub trait Action {
    /// Value sent back on the envelope's oneshot.
    type Reply;
}

/// A command plus the oneshot that carries its reply.
#[derive(Debug)]
pub struct Envelope<T: Action> {
    pub message: T,
    /// `send` returns `Err` when the caller dropped the request, including [`ActorHandle::post`].
    pub reply: oneshot::Sender<T::Reply>,
}

/// Cloneable sender into an actor mailbox.
///
/// Cloning copies the sender only. `M` does not need to be [`Clone`].
pub struct ActorHandle<M> {
    sender: mpsc::UnboundedSender<M>,
}

impl<M> Clone for ActorHandle<M> {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
        }
    }
}

impl<M> std::fmt::Debug for ActorHandle<M> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ActorHandle")
            .field("sender", &self.sender)
            .finish()
    }
}

impl<M> ActorHandle<M> {
    /// Opens a mailbox. The receiver is the actor's inbox.
    pub fn new() -> (Self, mpsc::UnboundedReceiver<M>) {
        let (sender, receiver) = mpsc::unbounded();
        (Self { sender }, receiver)
    }

    /// Queues `message` and waits for the actor's reply.
    pub async fn request<T>(&self, message: T) -> Result<T::Reply, DeliveryError>
    where
        T: Action,
        M: From<Envelope<T>>,
    {
        self.enqueue(message)?
            .await
            .map_err(|_| DeliveryError::ReplyDropped)
    }

    /// Queues `message` without waiting for a reply.
    ///
    /// The reply receiver is dropped here, so the actor's `reply.send` returns
    /// `Err`. That is expected: `Drop` impls and other synchronous callbacks
    /// cannot await. The only failure is [`DeliveryError::ActorClosed`].
    pub fn post<T>(&self, message: T) -> Result<(), DeliveryError>
    where
        T: Action,
        M: From<Envelope<T>>,
    {
        drop(self.enqueue(message)?);
        Ok(())
    }

    fn enqueue<T>(&self, message: T) -> Result<oneshot::Receiver<T::Reply>, DeliveryError>
    where
        T: Action,
        M: From<Envelope<T>>,
    {
        let (reply, response) = oneshot::channel();
        self.sender
            .unbounded_send(M::from(Envelope { message, reply }))
            .map_err(|_| DeliveryError::ActorClosed)?;
        Ok(response)
    }
}

/// Mailbox enum and `From<Envelope<Command>>` for each concrete command.
///
/// Generic commands are not accepted; write the [`From`] impl by hand.
///
/// ```
/// use actor_handle::{Action, Envelope, actor_action};
///
/// struct Ping;
///
/// impl Action for Ping {
///     type Reply = ();
/// }
///
/// actor_action! {
///     enum Mailbox { Ping }
/// }
///
/// let (reply, _response) = futures::channel::oneshot::channel();
/// let _mailbox = Mailbox::from(Envelope { message: Ping, reply });
/// ```
#[macro_export]
macro_rules! actor_action {
    ($vis:vis enum $name:ident { $($command:ident),+ $(,)? }) => {
        $vis enum $name {
            $( $command($crate::Envelope<$command>), )+
        }

        $(
            impl ::core::convert::From<$crate::Envelope<$command>> for $name {
                fn from(envelope: $crate::Envelope<$command>) -> Self {
                    Self::$command(envelope)
                }
            }
        )+
    };
}

#[cfg(test)]
mod tests {
    use futures::{StreamExt as _, executor::block_on};

    use crate::{Action, ActorHandle, DeliveryError, Envelope};

    struct Ping(u8);

    impl Action for Ping {
        type Reply = u8;
    }

    crate::actor_action! {
        enum Mailbox { Ping }
    }

    #[test]
    fn request_returns_the_reply() {
        let (handle, mut inbox) = ActorHandle::new();
        let (result, ()) = block_on(async {
            let request = handle.request(Ping(1));
            let answer = async {
                let Mailbox::Ping(envelope) = inbox.next().await.expect("command queued");
                envelope.reply.send(7).expect("caller is waiting");
            };
            futures::join!(request, answer)
        });
        assert_eq!(result.expect("reply"), 7);
    }

    #[test]
    fn dropped_receiver_is_actor_closed() {
        let (handle, inbox) = ActorHandle::<Mailbox>::new();
        drop(inbox);
        let requested = block_on(handle.request(Ping(1)));
        assert!(matches!(requested, Err(DeliveryError::ActorClosed)));
        assert!(matches!(
            handle.post(Ping(1)),
            Err(DeliveryError::ActorClosed)
        ));
    }

    #[test]
    fn dropped_reply_sender_is_reply_dropped() {
        let (handle, mut inbox) = ActorHandle::new();
        let (result, ()) = block_on(async {
            let request = handle.request(Ping(1));
            let drop_reply = async {
                let Mailbox::Ping(envelope) = inbox.next().await.expect("command queued");
                drop(envelope.reply);
            };
            futures::join!(request, drop_reply)
        });
        assert!(matches!(result, Err(DeliveryError::ReplyDropped)));
    }

    #[test]
    fn post_delivers_the_message() {
        let (handle, mut inbox) = ActorHandle::new();
        handle.post(Ping(4)).expect("actor is alive");
        let Mailbox::Ping(Envelope { message, reply }) =
            block_on(inbox.next()).expect("message queued");
        assert_eq!(message.0, 4);
        assert!(reply.send(message.0).is_err());
    }

    #[test]
    fn handle_is_clone_without_clone_messages() {
        struct NoClone;
        let (handle, _inbox) = ActorHandle::<NoClone>::new();
        let _cloned = handle.clone();
        // `NoClone` has no `Clone` impl. Cloning the handle must still compile.
        let _message = NoClone;
    }
}
