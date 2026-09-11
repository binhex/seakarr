use crate::types::{Download, DownloadStatus};

#[derive(Default)]
pub struct DownloadStore {
    downloads: Vec<Download>,
}

impl DownloadStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, download: Download) {
        self.downloads.push(download);
    }

    pub fn remove(&mut self, token: u32) {
        self.downloads.retain(|d| d.token != token);
    }

    pub fn remove_by_attempt_id(&mut self, attempt_id: u32) -> Option<Download> {
        let index = self
            .downloads
            .iter()
            .position(|download| download.attempt_id == attempt_id)?;
        Some(self.downloads.remove(index))
    }

    #[must_use]
    pub fn get_by_token(&self, token: u32) -> Option<&Download> {
        self.downloads.iter().find(|d| d.token == token)
    }

    pub fn get_by_token_mut(&mut self, token: u32) -> Option<&mut Download> {
        self.downloads.iter_mut().find(|d| d.token == token)
    }

    pub fn get_by_file_mut(&mut self, username: &str, filename: &str) -> Option<&mut Download> {
        self.downloads
            .iter_mut()
            .find(|d| d.username == username && d.filename == filename)
    }

    #[must_use]
    pub fn tokens(&self) -> Vec<u32> {
        self.downloads.iter().map(|d| d.token).collect()
    }

    #[must_use]
    pub const fn list(&self) -> &Vec<Download> {
        &self.downloads
    }

    pub fn update_status(&mut self, token: u32, status: DownloadStatus) {
        if let Some(download) = self.get_by_token_mut(token) {
            download.status = status;
        }
    }

    /// Publish a tokenless queue-position response to the newest matching
    /// attempt that is still queued.
    ///
    /// Soulseek encodes "not currently queued" as zero; expose that as an
    /// unknown position rather than as a one-based queue position. Because a
    /// code-44 response has no attempt token, newest-queued selection is the
    /// safest attribution when an old terminal record awaits cleanup.
    pub fn update_queue_position(&mut self, username: &str, filename: &str, position: u32) -> bool {
        let Some(download) = self.downloads.iter_mut().rev().find(|download| {
            download.username == username
                && download.filename == filename
                && matches!(download.status, DownloadStatus::Queued { .. })
        }) else {
            return false;
        };
        let queue_position = (position > 0).then_some(position);
        let status = DownloadStatus::Queued { queue_position };
        download.queue_position = queue_position;
        download.status = status.clone();
        let sender = download.sender.clone();
        let _ = sender.send(status);
        true
    }

    pub fn remove_queued_by_file(&mut self, username: &str, filename: &str) -> bool {
        let Some(index) = self.downloads.iter().position(|download| {
            download.username == username
                && download.filename == filename
                && matches!(download.status, DownloadStatus::Queued { .. })
        }) else {
            return false;
        };

        self.downloads.remove(index);
        true
    }

    /// Remove every download matching `username`/`filename` regardless of
    /// status. Used before retrying a failed download so the stale entry (whose
    /// md5-derived token collides with the retry's) can't shadow the fresh one.
    /// Returns whether anything was removed.
    pub fn remove_by_file(&mut self, username: &str, filename: &str) -> bool {
        let before = self.downloads.len();
        self.downloads
            .retain(|d| !(d.username == username && d.filename == filename));
        self.downloads.len() != before
    }

    pub fn pause_by_file(&mut self, username: &str, filename: &str) -> bool {
        let Some(download) = self.get_by_file_mut(username, filename) else {
            return false;
        };
        Self::pause(download)
    }

    pub fn pause_by_attempt_id(&mut self, attempt_id: u32) -> bool {
        let Some(download) = self
            .downloads
            .iter_mut()
            .find(|download| download.attempt_id == attempt_id)
        else {
            return false;
        };
        Self::pause(download)
    }

    fn pause(download: &mut Download) -> bool {
        let paused_status = match &download.status {
            DownloadStatus::InProgress {
                bytes_downloaded,
                total_bytes,
                ..
            } => DownloadStatus::Paused {
                bytes_downloaded: *bytes_downloaded,
                total_bytes: *total_bytes,
            },
            DownloadStatus::Paused { .. } => return true,
            _ => return false,
        };

        download.status = paused_status.clone();
        let _ = download.sender.send(paused_status);
        true
    }

    pub fn resume_by_file(&mut self, username: &str, filename: &str) -> bool {
        let Some(download) = self.get_by_file_mut(username, filename) else {
            return false;
        };

        let resumed_status = match &download.status {
            DownloadStatus::Paused {
                bytes_downloaded,
                total_bytes,
            } => DownloadStatus::InProgress {
                bytes_downloaded: *bytes_downloaded,
                total_bytes: *total_bytes,
                speed_bytes_per_sec: 0.0,
            },
            DownloadStatus::InProgress { .. } => return true,
            _ => return false,
        };

        download.status = resumed_status.clone();
        let _ = download.sender.send(resumed_status);
        true
    }
}

/// Returns the tokens of downloads matching `username` (and optionally a
/// `filename`) after notifying their senders of `Failed`.
///
/// Caller is responsible for then calling `update_status` and `remove` for
/// each token, typically under a write lock.
#[must_use]
pub fn collect_failed_tokens(
    store: &DownloadStore,
    username: &str,
    filename: Option<&str>,
) -> Vec<u32> {
    store
        .list()
        .iter()
        .filter(|d| d.username == username && filename.is_none_or(|f| d.filename == *f))
        .filter(|d| {
            !matches!(
                d.status,
                DownloadStatus::InProgress { .. } | DownloadStatus::Paused { .. }
            )
        })
        .map(|d| {
            let _ = d.sender.send(DownloadStatus::Failed(Some(
                "The upload failed on the other side".to_string(),
            )));
            d.token
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::DownloadMetadata;
    use std::sync::mpsc;

    fn make_download(token: u32, status: DownloadStatus) -> Download {
        Download {
            username: "peer".to_string(),
            filename: format!("file-{token}.mp3"),
            attempt_id: token,
            token,
            size: 100,
            download_directory: "test".to_string(),
            status,
            sender: mpsc::channel().0,
            queue_position: None,
            metadata: DownloadMetadata::default(),
        }
    }

    #[test]
    fn add_get_remove_roundtrip() {
        let mut store = DownloadStore::new();
        store.add(make_download(
            123,
            DownloadStatus::Queued {
                queue_position: None,
            },
        ));

        assert!(store.get_by_token(123).is_some());
        assert_eq!(store.tokens(), vec![123]);
        assert_eq!(store.list().len(), 1);

        store.remove(123);
        assert!(store.get_by_token(123).is_none());
        assert!(store.list().is_empty());
    }

    #[test]
    fn update_queue_position_updates_store_and_notifies_receiver() {
        let mut store = DownloadStore::new();
        let (sender, receiver) = mpsc::channel();
        let mut download = make_download(
            1,
            DownloadStatus::Queued {
                queue_position: None,
            },
        );
        download.username = "peer".to_string();
        download.filename = "song.mp3".to_string();
        download.sender = sender;
        store.add(download);

        assert!(store.update_queue_position("peer", "song.mp3", 7));
        assert!(matches!(
            receiver.recv_timeout(std::time::Duration::from_secs(1)),
            Ok(DownloadStatus::Queued {
                queue_position: Some(7)
            })
        ));
        let stored = store.get_by_token(1).unwrap();
        assert_eq!(stored.queue_position, Some(7));
        assert!(matches!(
            stored.status,
            DownloadStatus::Queued {
                queue_position: Some(7)
            }
        ));

        assert!(!store.update_queue_position("peer", "missing.mp3", 1));
        assert!(!store.update_queue_position("other", "song.mp3", 1));
    }

    #[test]
    fn zero_queue_position_is_published_as_unknown() {
        let mut store = DownloadStore::new();
        let (sender, receiver) = mpsc::channel();
        let mut download = make_download(
            1,
            DownloadStatus::Queued {
                queue_position: Some(2),
            },
        );
        download.username = "peer".to_string();
        download.filename = "song.mp3".to_string();
        download.sender = sender;
        download.queue_position = Some(2);
        store.add(download);

        assert!(store.update_queue_position("peer", "song.mp3", 0));
        assert!(matches!(
            receiver.recv_timeout(std::time::Duration::from_secs(1)),
            Ok(DownloadStatus::Queued {
                queue_position: None
            })
        ));
        let stored = store.get_by_token(1).unwrap();
        assert_eq!(stored.queue_position, None);
        assert!(matches!(
            stored.status,
            DownloadStatus::Queued {
                queue_position: None
            }
        ));
    }

    #[test]
    fn queue_position_update_targets_newest_queued_same_file_attempt() {
        let mut store = DownloadStore::new();
        let (old_sender, old_receiver) = mpsc::channel();
        let (new_sender, new_receiver) = mpsc::channel();
        let mut old_attempt = make_download(1, DownloadStatus::Failed(None));
        old_attempt.filename = "song.mp3".to_string();
        old_attempt.sender = old_sender;
        store.add(old_attempt);
        let mut new_attempt = make_download(
            2,
            DownloadStatus::Queued {
                queue_position: None,
            },
        );
        new_attempt.filename = "song.mp3".to_string();
        new_attempt.sender = new_sender;
        store.add(new_attempt);

        assert!(store.update_queue_position("peer", "song.mp3", 4));
        assert!(matches!(
            old_receiver.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        assert!(matches!(
            new_receiver.recv_timeout(std::time::Duration::from_secs(1)),
            Ok(DownloadStatus::Queued {
                queue_position: Some(4)
            })
        ));
        assert_eq!(store.get_by_token(1).unwrap().queue_position, None);
        assert_eq!(store.get_by_token(2).unwrap().queue_position, Some(4));
    }

    #[test]
    fn late_queue_position_does_not_regress_in_progress_download() {
        let mut store = DownloadStore::new();
        let (sender, receiver) = mpsc::channel();
        let mut download = make_download(
            1,
            DownloadStatus::InProgress {
                bytes_downloaded: 25,
                total_bytes: 100,
                speed_bytes_per_sec: 10.0,
            },
        );
        download.filename = "song.mp3".to_string();
        download.sender = sender;
        store.add(download);

        assert!(!store.update_queue_position("peer", "song.mp3", 7));
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        let stored = store.get_by_token(1).unwrap();
        assert_eq!(stored.queue_position, None);
        assert!(matches!(
            stored.status,
            DownloadStatus::InProgress {
                bytes_downloaded: 25,
                total_bytes: 100,
                speed_bytes_per_sec: 10.0
            }
        ));
    }

    #[test]
    fn late_queue_position_does_not_regress_paused_download() {
        let mut store = DownloadStore::new();
        let (sender, receiver) = mpsc::channel();
        let mut download = make_download(
            1,
            DownloadStatus::Paused {
                bytes_downloaded: 25,
                total_bytes: 100,
            },
        );
        download.filename = "song.mp3".to_string();
        download.sender = sender;
        store.add(download);

        assert!(!store.update_queue_position("peer", "song.mp3", 7));
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        let stored = store.get_by_token(1).unwrap();
        assert_eq!(stored.queue_position, None);
        assert!(matches!(
            stored.status,
            DownloadStatus::Paused {
                bytes_downloaded: 25,
                total_bytes: 100
            }
        ));
    }

    #[test]
    fn pause_then_resume_in_progress_download() {
        let mut store = DownloadStore::new();
        let (tx, rx) = mpsc::channel();
        let mut download = make_download(
            1,
            DownloadStatus::InProgress {
                bytes_downloaded: 25,
                total_bytes: 100,
                speed_bytes_per_sec: 10.0,
            },
        );
        download.sender = tx;
        store.add(download);

        assert!(store.pause_by_file("peer", "file-1.mp3"));
        assert!(matches!(
            store.get_by_token(1).unwrap().status,
            DownloadStatus::Paused {
                bytes_downloaded: 25,
                total_bytes: 100
            }
        ));
        assert!(matches!(
            rx.try_recv().unwrap(),
            DownloadStatus::Paused {
                bytes_downloaded: 25,
                total_bytes: 100
            }
        ));

        assert!(store.resume_by_file("peer", "file-1.mp3"));
        assert!(matches!(
            store.get_by_token(1).unwrap().status,
            DownloadStatus::InProgress {
                bytes_downloaded: 25,
                total_bytes: 100,
                speed_bytes_per_sec: 0.0
            }
        ));
    }

    #[test]
    fn remove_queued_skips_active_downloads() {
        let mut store = DownloadStore::new();
        store.add(make_download(
            123,
            DownloadStatus::Queued {
                queue_position: None,
            },
        ));
        store.add(make_download(
            456,
            DownloadStatus::InProgress {
                bytes_downloaded: 25,
                total_bytes: 100,
                speed_bytes_per_sec: 10.0,
            },
        ));
        // Override second download's filename so they don't collide
        store.get_by_token_mut(456).unwrap().filename = "active.mp3".to_string();
        store.get_by_token_mut(123).unwrap().filename = "queued.mp3".to_string();

        assert!(store.remove_queued_by_file("peer", "queued.mp3"));
        assert!(!store.remove_queued_by_file("peer", "active.mp3"));
        assert!(store.get_by_token(123).is_none());
        assert!(store.get_by_token(456).is_some());
    }

    #[test]
    fn remove_by_file_removes_regardless_of_status() {
        let mut store = DownloadStore::new();
        // A failed download (the retry case) plus a same-name duplicate that a
        // token-migration could have left behind — both must go.
        let mut failed = make_download(1, DownloadStatus::Failed(None));
        failed.filename = "song.mp3".to_string();
        store.add(failed);
        let mut dup = make_download(
            2,
            DownloadStatus::Queued {
                queue_position: None,
            },
        );
        dup.filename = "song.mp3".to_string();
        store.add(dup);
        let mut other = make_download(
            3,
            DownloadStatus::Queued {
                queue_position: None,
            },
        );
        other.filename = "other.mp3".to_string();
        store.add(other);

        assert!(store.remove_by_file("peer", "song.mp3"));
        assert!(store.get_by_token(1).is_none());
        assert!(store.get_by_token(2).is_none());
        assert!(store.get_by_token(3).is_some(), "other file untouched");
        assert!(!store.remove_by_file("peer", "song.mp3"), "idempotent");
    }

    #[test]
    fn collect_failed_tokens_notifies_and_lists_matching() {
        let mut store = DownloadStore::new();
        let (tx_match, rx_match) = mpsc::channel();
        let (tx_other_user, _rx_other_user) = mpsc::channel();
        let (tx_other_file, _rx_other_file) = mpsc::channel();

        let mut a = make_download(
            1,
            DownloadStatus::Queued {
                queue_position: None,
            },
        );
        a.sender = tx_match;
        a.username = "peer".to_string();
        a.filename = "song.mp3".to_string();

        let mut b = make_download(
            2,
            DownloadStatus::Queued {
                queue_position: None,
            },
        );
        b.sender = tx_other_user;
        b.username = "other".to_string();
        b.filename = "song.mp3".to_string();

        let mut c = make_download(
            3,
            DownloadStatus::Queued {
                queue_position: None,
            },
        );
        c.sender = tx_other_file;
        c.username = "peer".to_string();
        c.filename = "different.mp3".to_string();

        store.add(a);
        store.add(b);
        store.add(c);

        let tokens = collect_failed_tokens(&store, "peer", Some("song.mp3"));

        assert_eq!(tokens, vec![1]);
        assert!(matches!(
            rx_match.try_recv().unwrap(),
            DownloadStatus::Failed(_)
        ));
    }

    #[test]
    fn collect_failed_tokens_skips_in_progress_and_paused_downloads() {
        let mut store = DownloadStore::new();
        let (tx_active, _rx_active) = mpsc::channel();
        let (tx_paused, _rx_paused) = mpsc::channel();
        let (tx_queued, rx_queued) = mpsc::channel();

        // An active F-connection transfer: the P-connection may have dropped,
        // but bytes are still flowing — removing it would kill the transfer
        // and make wait_while_paused fail with TokenNotFound.
        let mut active = make_download(
            1,
            DownloadStatus::InProgress {
                bytes_downloaded: 500,
                total_bytes: 1000,
                speed_bytes_per_sec: 10.0,
            },
        );
        active.sender = tx_active;
        active.username = "peer".to_string();
        active.filename = "active.mp3".to_string();

        // A paused download can resume; dropping it would lose the part file
        // position and the user's pause state.
        let mut paused = make_download(
            2,
            DownloadStatus::Paused {
                bytes_downloaded: 300,
                total_bytes: 1000,
            },
        );
        paused.sender = tx_paused;
        paused.username = "peer".to_string();
        paused.filename = "paused.mp3".to_string();

        // A queued download has no active transfer and will never recover
        // once the peer is gone — it should be collected as failed.
        let mut queued = make_download(
            3,
            DownloadStatus::Queued {
                queue_position: None,
            },
        );
        queued.sender = tx_queued;
        queued.username = "peer".to_string();
        queued.filename = "queued.mp3".to_string();

        store.add(active);
        store.add(paused);
        store.add(queued);

        // Unscoped (filename: None) — the PeerDisconnected path.
        let tokens = collect_failed_tokens(&store, "peer", None);

        assert_eq!(tokens, vec![3], "only the queued download is collected");
        assert!(matches!(
            rx_queued.try_recv().unwrap(),
            DownloadStatus::Failed(_)
        ));
    }
}
