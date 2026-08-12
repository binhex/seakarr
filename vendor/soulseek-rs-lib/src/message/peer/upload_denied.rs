use crate::{
    message::{Message, MessageHandler},
    peer::PeerMessage,
};
use std::sync::mpsc::Sender;

/// A peer refusing a queued upload with "UploadDenied" (peer code 50).
///
/// The file is no longer shared. Without this handler the refusal is silently
/// dropped and the queued download hangs until the caller's own timeout; with
/// it, the download is failed immediately so callers can fall back to the next
/// candidate.
pub struct UploadDeniedHandler;

impl MessageHandler<PeerMessage> for UploadDeniedHandler {
    fn get_code(&self) -> u8 {
        50
    }

    fn handle(&self, message: &mut Message, sender: Sender<PeerMessage>) {
        let filename = message.read_string();
        let _ = sender.send(PeerMessage::UploadDenied(filename));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::framed;

    #[test]
    fn parses_filename_and_forwards_denial() {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut message = framed(|m| {
            m.write_string("shared\\Artist\\Album\\01 - Track.flac");
        });

        UploadDeniedHandler.handle(&mut message, tx);

        match rx.try_recv() {
            Ok(PeerMessage::UploadDenied(filename)) => {
                assert_eq!(filename, "shared\\Artist\\Album\\01 - Track.flac");
            }
            other => panic!("expected UploadDenied, got {other:?}"),
        }
    }
}
