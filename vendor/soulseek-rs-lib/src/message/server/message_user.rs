use crate::actor::server_actor::{ServerMessage, UserMessage};
use crate::message::server::MessageFactory;
use crate::message::{Message, MessageHandler};
use crate::{debug, error, info};

use std::sync::mpsc::Sender;

pub struct MessageUser;

/// Body-free diagnostic for an acknowledgement that could not be queued.
///
/// Kept as a pure function so the privacy guarantee is directly testable: the
/// crate logger exposes no readable capture, so a test asserts this text names
/// the message and sender without repeating the message body.
fn acknowledgement_failure(id: u32, username: &str) -> String {
    format!("[MessageUser] could not acknowledge message {id} from {username}")
}

impl MessageHandler<ServerMessage> for MessageUser {
    fn get_code(&self) -> u8 {
        22
    }

    fn handle(&self, message: &mut Message, sender: Sender<ServerMessage>) {
        let id = message.read_int32();
        let timestamp = message.read_int32();
        let username = message.read_string();
        let message_content = message.read_string();
        let new_message = message.read_bool();

        // Display and forward every server delivery before clearing it. A false
        // `new_message` flag can be the first local delivery of a message queued
        // while this client was offline, so suppressing it would lose the body.
        let user_message = UserMessage::new(
            id,
            timestamp,
            username.clone(),
            message_content,
            new_message,
        );

        info!("[MessageUser] User message received:{:?}", user_message);

        // Surface the message to the client so it can be read via the API. The
        // INFO display counts as delivery, so still clear it if forwarding fails.
        if let Err(error) = sender.send(ServerMessage::PrivateMessageReceived(user_message)) {
            error!(
                "[MessageUser] could not forward private message to client: {}",
                error
            );
        }

        // Acknowledge every delivery. The server keeps re-sending a message it
        // still holds, which is why an unacknowledged private message used to
        // reappear on every run.
        if let Err(error) = sender.send(ServerMessage::SendMessage(
            MessageFactory::build_message_acked(id),
        )) {
            debug!("{}: {}", acknowledgement_failure(id, &username), error);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::framed;

    const MESSAGE_ID: u32 = 292_923;
    const TIMESTAMP: u32 = 1_789_162_359;
    const SENDER: &str = "bossanovakid";
    const BODY: &str = "Please share your music.";

    fn message_payload(new_message: bool) -> Message {
        framed(|message| {
            message
                .write_int32(MESSAGE_ID)
                .write_int32(TIMESTAMP)
                .write_string(SENDER)
                .write_string(BODY)
                .write_bool(new_message);
        })
    }

    /// An acknowledgement is server code 23 followed by the original id.
    fn assert_acknowledges(acknowledgement: &Message) {
        let data = acknowledgement.get_data();
        assert_eq!(data.len(), 8, "acknowledgement is code plus id");
        assert_eq!(
            &data[0..4],
            23u32.to_le_bytes().as_slice(),
            "acknowledgement uses server code 23"
        );
        assert_eq!(
            &data[4..8],
            MESSAGE_ID.to_le_bytes().as_slice(),
            "acknowledgement carries the original id"
        );
    }

    #[test]
    fn a_new_message_is_delivered_before_it_is_acknowledged() {
        let (sender, receiver) = std::sync::mpsc::channel();
        let mut message = message_payload(true);

        MessageUser.handle(&mut message, sender);

        match receiver.try_recv() {
            Ok(ServerMessage::PrivateMessageReceived(delivered)) => {
                assert_eq!(delivered.id(), MESSAGE_ID);
                assert_eq!(delivered.timestamp(), TIMESTAMP);
                assert_eq!(delivered.username(), SENDER);
                assert_eq!(delivered.message(), BODY);
                assert!(delivered.is_new());
            }
            other => panic!("first event must be the delivery, got {other:?}"),
        }

        match receiver.try_recv() {
            Ok(ServerMessage::SendMessage(acknowledgement)) => {
                assert_acknowledges(&acknowledgement);
            }
            other => panic!("second event must be the acknowledgement, got {other:?}"),
        }

        assert!(
            receiver.try_recv().is_err(),
            "a new message produces exactly two events"
        );
    }

    #[test]
    fn a_resent_offline_message_is_delivered_then_acknowledged() {
        let (sender, receiver) = std::sync::mpsc::channel();
        let mut message = message_payload(false);

        MessageUser.handle(&mut message, sender);

        match receiver.try_recv() {
            Ok(ServerMessage::PrivateMessageReceived(delivered)) => {
                assert_eq!(delivered.id(), MESSAGE_ID);
                assert_eq!(delivered.timestamp(), TIMESTAMP);
                assert_eq!(delivered.username(), SENDER);
                assert_eq!(delivered.message(), BODY);
                assert!(!delivered.is_new());
            }
            other => panic!("first event must deliver the offline message, got {other:?}"),
        }

        match receiver.try_recv() {
            Ok(ServerMessage::SendMessage(acknowledgement)) => {
                assert_acknowledges(&acknowledgement);
            }
            other => panic!("second event must acknowledge the offline message, got {other:?}"),
        }

        assert!(
            receiver.try_recv().is_err(),
            "an offline message produces exactly two events"
        );
    }

    #[test]
    fn a_disconnected_channel_is_non_fatal_and_the_diagnostic_omits_the_body() {
        // Ack enqueue failure must not panic for either flag value, including
        // an offline re-delivery.
        for new_message in [true, false] {
            let (sender, receiver) = std::sync::mpsc::channel();
            drop(receiver);
            let mut message = message_payload(new_message);

            MessageUser.handle(&mut message, sender);
        }

        // The failure text names the message and sender, never the body.
        let failure = acknowledgement_failure(MESSAGE_ID, SENDER);
        assert!(failure.contains("292923"), "got: {failure}");
        assert!(failure.contains(SENDER), "got: {failure}");
        assert!(
            !failure.contains(BODY),
            "acknowledgement diagnostics must not repeat the body, got: {failure}"
        );
    }
}
