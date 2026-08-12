use crate::{
    message::{Message, MessageHandler},
    peer::PeerMessage,
};
use std::sync::mpsc::Sender;

pub struct UploadFailedHandler;
impl MessageHandler<PeerMessage> for UploadFailedHandler {
    fn get_code(&self) -> u8 {
        46
    }
    fn handle(&self, message: &mut Message, sender: Sender<PeerMessage>) {
        let filename = message.read_string();
        // Forward to the actor so the queued download is failed and the
        // caller's status channel unblocks (previously this only logged,
        // leaving the download to hang until the caller's own timeout).
        let _ = sender.send(PeerMessage::UploadDenied(filename));
    }
}
