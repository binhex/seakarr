//! In-memory map of the last peer that served each artist reliably.

use std::collections::HashMap;
use std::sync::Mutex;

/// Artist → the peer username that most recently downloaded an album by that
/// artist cleanly (first candidate, first attempt, no retries, no failures).
///
/// One instance is shared across the concurrently-processed album futures for
/// the lifetime of a run. The lock is never held across `.await`.
pub struct ReliablePeers {
    peers: Mutex<HashMap<String, String>>,
}

impl Default for ReliablePeers {
    fn default() -> Self {
        Self {
            peers: Mutex::new(HashMap::new()),
        }
    }
}

impl ReliablePeers {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&self, artist: &str, peer: &str) {
        let mut peers = self.peers.lock().unwrap_or_else(|p| p.into_inner());
        peers.insert(artist.to_string(), peer.to_string());
    }

    #[must_use]
    pub fn get(&self, artist: &str) -> Option<String> {
        let peers = self.peers.lock().unwrap_or_else(|p| p.into_inner());
        peers.get(artist).cloned()
    }

    pub fn evict(&self, artist: &str) {
        let mut peers = self.peers.lock().unwrap_or_else(|p| p.into_inner());
        peers.remove(artist);
    }

    /// Remove the artist's entry only if it still names `peer`. A stale
    /// eviction (from an older album that read the map before a newer one
    /// recorded a different peer) must not wipe the fresher record.
    pub fn evict_if(&self, artist: &str, peer: &str) {
        let mut peers = self.peers.lock().unwrap_or_else(|p| p.into_inner());
        if peers
            .get(artist)
            .is_some_and(|p| p.eq_ignore_ascii_case(peer))
        {
            peers.remove(artist);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_then_get_returns_the_peer() {
        let peers = ReliablePeers::new();
        assert!(peers.get("artist").is_none());
        peers.record("artist", "peer");
        assert_eq!(peers.get("artist").as_deref(), Some("peer"));
    }

    #[test]
    fn record_overwrites_the_previous_peer() {
        let peers = ReliablePeers::new();
        peers.record("artist", "first");
        peers.record("artist", "second");
        assert_eq!(peers.get("artist").as_deref(), Some("second"));
    }

    #[test]
    fn evict_removes_the_entry() {
        let peers = ReliablePeers::new();
        peers.record("artist", "peer");
        peers.evict("artist");
        assert!(peers.get("artist").is_none());
    }

    #[test]
    fn evict_if_only_removes_when_the_peer_matches() {
        let peers = ReliablePeers::new();
        peers.record("artist", "peerA");
        // A stale eviction for a peer that no longer holds the entry is a no-op.
        peers.evict_if("artist", "peerB");
        assert_eq!(peers.get("artist").as_deref(), Some("peerA"));
        // A matching (case-insensitive) eviction removes it.
        peers.evict_if("artist", "peera");
        assert!(peers.get("artist").is_none());
    }
}
