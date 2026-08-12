use crate::actor::peer_actor::{PeerActor, PeerMessage};
use crate::actor::{ActorHandle, ActorSystem};
use crate::client::ClientOperation;
use crate::message::MessageReader;
use crate::peer::Peer;
use crate::utils::lock::MutexExt;
use crate::{debug, error};

use std::collections::HashMap;
use std::net::TcpStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

/// Source of unique per-actor ids so terminal-outcome eviction can be made
/// identity-aware (a replaced actor must not evict its replacement).
static NEXT_PEER_ID: AtomicU64 = AtomicU64::new(1);

/// Registered peers keyed by username, each stored with the unique id of the
/// actor currently occupying the slot and its registration instant (used to
/// avoid evicting fresh search-responder connections before they deliver).
type PeerMap = HashMap<String, (u64, ActorHandle<PeerMessage>, std::time::Instant)>;

/// Registry state guarded by a single mutex: the peer map plus FIFO
/// registration order used for capacity eviction. Keeping both under one
/// lock keeps the capacity check, eviction, and insertion race-free.
struct RegistryState {
    peers: PeerMap,
    order: std::collections::VecDeque<String>,
}

/// A freshly registered peer is exempt from eviction for this long.
///
/// Search responders connect (server-brokered) and need a moment to deliver
/// their FileSearchResponse before the registry may reap them; evicting on
/// pure FIFO killed responders ~100 ms after connect and empty searches
/// returned no results at all.
pub const EVICTION_GRACE_PERIOD: std::time::Duration = std::time::Duration::from_secs(30);

pub struct PeerRegistry {
    state: Arc<Mutex<RegistryState>>,
    actor_system: Arc<ActorSystem>,
    client_channel: Sender<ClientOperation>,
    own_username: String,
    max_peers: usize,
}

/// Default ceiling on simultaneous peer connections.
///
/// The Soulseek server pushes a ConnectToPeer for every search-result peer
/// (hundreds per search), and each peer owns an OS thread for its lifetime;
/// an unbounded registry flooded the process with threads and killed the ops
/// loop. 16 is enough for the peers a client actually transfers with while
/// keeping the thread count sane.
pub const DEFAULT_MAX_PEERS: usize = 16;

impl PeerRegistry {
    #[must_use]
    pub fn new(
        actor_system: Arc<ActorSystem>,
        client_channel: Sender<ClientOperation>,
        own_username: String,
    ) -> Self {
        Self::with_max_peers(
            actor_system,
            client_channel,
            own_username,
            DEFAULT_MAX_PEERS,
        )
    }

    #[must_use]
    pub fn with_max_peers(
        actor_system: Arc<ActorSystem>,
        client_channel: Sender<ClientOperation>,
        own_username: String,
        max_peers: usize,
    ) -> Self {
        Self {
            state: Arc::new(Mutex::new(RegistryState {
                peers: HashMap::new(),
                order: std::collections::VecDeque::new(),
            })),
            actor_system,
            client_channel,
            own_username,
            max_peers: max_peers.max(1),
        }
    }

    pub fn register_peer(
        &self,
        peer: Peer,
        stream: Option<TcpStream>,
        reader: Option<MessageReader>,
    ) -> Result<ActorHandle<PeerMessage>, String> {
        let username = peer.username.clone();
        let id = NEXT_PEER_ID.fetch_add(1, Ordering::Relaxed);

        let actor = PeerActor::new(
            peer,
            stream,
            reader,
            self.client_channel.clone(),
            self.own_username.clone(),
            id,
        );

        // Take the lock before the actor exists: a peer that dies instantly
        // (refused dial, immediate hangup) reports its terminal outcome to
        // the client loop, whose eviction takes this same lock. With the
        // insert racing the spawn, eviction could run first, find nothing,
        // and the entry inserted afterwards became a permanent zombie
        // claiming the username.
        //
        // Deadlock-avoidance invariant: the spawned actor must never call
        // back into this registry while this lock is held. Actors talk to the
        // client loop via channels; the loop blocks on this mutex until
        // register_peer releases it (contention, not deadlock).
        let mut state = self
            .state
            .lock_safe()
            .map_err(|e| format!("peer registry lock poisoned: {e}"))?;

        // Bound the number of simultaneous peer threads. The Soulseek server
        // pushes ConnectToPeer for every search-result peer, and each peer
        // owns an OS thread for its lifetime; without this cap a single
        // popular search (hundreds of peers) flooded the process and killed
        // the client ops loop.
        if state.peers.contains_key(&username) && state.peers.len() >= self.max_peers {
            // At capacity: skip replacement. Duplicate-username floods
            // (one user with many search results) would otherwise spawn
            // a fresh thread per duplicate while the replaced actor
            // lingers in its dial window — the cap would bound entries,
            // not threads. Keep the existing actor; it either serves or
            // evicts itself on its terminal event, freeing the slot.
            //
            // Tradeoff: a duplicate arriving in the tiny window between
            // a peer's terminal event and its processing is skipped even
            // though the slot is about to free. Self-healing: the
            // terminal event evicts the entry, and the server re-brokers
            // the connection on the next retry.
            debug!("[peer_registry] skipping replacement of {username} at capacity");
            return state
                .peers
                .get(&username)
                .map(|(_, h, _)| h.clone())
                .ok_or_else(|| format!("peer {username} vanished from registry"));
        }

        // Spawn FIRST, evict after: a failed spawn (EAGAIN under thread
        // pressure — the exact condition the cap exists for) must not stop a
        // live peer for nothing. Eviction and insertion both happen under
        // this same lock, so the capacity check remains race-free.
        let handle = self
            .actor_system
            .try_spawn_with_handle(actor, |actor, handle| {
                actor.set_self_handle(handle);
            })
            .map_err(|e| format!("failed to spawn peer actor thread: {e}"))?;

        if !state.peers.contains_key(&username) && state.peers.len() >= self.max_peers {
            // FIFO eviction with a grace period: drop the oldest
            // registration that has been around long enough to have
            // delivered its search response (or whatever it connected
            // for). Search-result flood peers register first, so they are
            // the natural eviction candidates — but a responder evicted
            // milliseconds after connecting never delivers, which made
            // searches return no results at all.
            //
            // If every peer is still inside the grace window, refuse the
            // new registration instead: slot churn would otherwise kill
            // responders faster than they can answer.
            let now = std::time::Instant::now();
            let oldest_evictable = state
                .peers
                .iter()
                .filter(|(_, (_, _, registered))| {
                    now.duration_since(*registered) >= EVICTION_GRACE_PERIOD
                })
                .map(|(name, _)| name.clone())
                .min_by_key(|name| {
                    state
                        .peers
                        .get(name)
                        .map_or(now, |(_, _, registered)| *registered)
                });

            let Some(oldest) = oldest_evictable else {
                return Err(format!(
                    "peer registry at capacity ({}) and all peers inside eviction grace period — refusing {username}",
                    self.max_peers
                ));
            };

            state.order.retain(|u| u != &oldest);
            debug_assert!(
                state.peers.contains_key(&oldest),
                "order/peers invariant violated: {oldest} in order but not in peers"
            );
            if let Some((_, old_handle, _)) = state.peers.remove(&oldest) {
                let _ = old_handle.stop();
                debug!("[peer_registry] evicted oldest peer {oldest} at capacity");
            }
        }

        // Stop any actor already registered under this username so it does not
        // become an orphan pinning a pool worker forever. Eviction on the
        // replaced actor's later shutdown is identity-aware (keyed on its id),
        // so stopping it here cannot evict this new connection.
        if let Some((_, old_handle, _)) = state.peers.insert(
            username.clone(),
            (id, handle.clone(), std::time::Instant::now()),
        ) {
            let _ = old_handle.stop();
            // A replacement is a fresh connection: refresh its FIFO position
            // so it is not evicted as "oldest" on the next capacity-triggering
            // registration (it inherited the original registration's age).
            state.order.retain(|u| u != &username);
            state.order.push_back(username.clone());
            debug!(
                "[peer_registry] Replaced existing peer actor for {}",
                username
            );
        } else {
            state.order.push_back(username);
        }

        Ok(handle)
    }

    #[must_use]
    pub fn get_peer(&self, username: &str) -> Option<ActorHandle<PeerMessage>> {
        match self.state.lock_safe() {
            Ok(state) => state
                .peers
                .get(username)
                .map(|(_, handle, _)| handle.clone()),
            Err(e) => {
                error!("[peer_registry] get_peer: {}", e);
                None
            }
        }
    }

    #[must_use]
    pub fn remove_peer(&self, username: &str) -> Option<ActorHandle<PeerMessage>> {
        let mut state = match self.state.lock_safe() {
            Ok(s) => s,
            Err(e) => {
                error!("[peer_registry] remove_peer: {}", e);
                return None;
            }
        };
        let removed = state.peers.remove(username);

        if removed.is_some() {
            state.order.retain(|u| u != username);
            debug!("[peer_registry] Removed peer actor for {}", username);
        }

        removed.map(|(_, handle, _)| handle)
    }

    /// Remove and return the actor for `username` only if it is still the actor
    /// with `id`. A stale (replaced) actor's terminal notification therefore
    /// cannot evict the newer actor that now occupies the slot.
    #[must_use]
    pub fn remove_peer_if(&self, username: &str, id: u64) -> Option<ActorHandle<PeerMessage>> {
        let mut state = match self.state.lock_safe() {
            Ok(s) => s,
            Err(e) => {
                error!("[peer_registry] remove_peer_if: {}", e);
                return None;
            }
        };
        if state
            .peers
            .get(username)
            .is_some_and(|(stored, _, _)| *stored == id)
        {
            let removed = state.peers.remove(username).map(|(_, handle, _)| handle);
            state.order.retain(|u| u != username);
            debug!("[peer_registry] Removed peer actor {} for {}", id, username);
            return removed;
        }
        None
    }

    #[must_use]
    pub fn contains(&self, username: &str) -> bool {
        match self.state.lock_safe() {
            Ok(state) => state.peers.contains_key(username),
            Err(e) => {
                error!("[peer_registry] contains: {}", e);
                false
            }
        }
    }

    pub fn send_to_peer(&self, username: &str, message: PeerMessage) -> Result<(), String> {
        let handle = self
            .get_peer(username)
            .ok_or_else(|| format!("Peer {username} not found in registry"))?;

        // Refresh FIFO position: an actively-used peer (e.g. one with a
        // queued download or an in-flight transfer) must not be evicted as
        // "oldest" while it is being used. Idle flood peers age out first.
        //
        // Known limitations (benign):
        // - TOCTOU: the handle was fetched under an earlier lock hold; if the
        //   peer is evicted/replaced between the two acquisitions, the refresh
        //   is a no-op (contains_key guard) and the message goes to a stopped
        //   actor, whose channel send still succeeds into the mailbox and is
        //   dropped. At-most-once delivery; callers treat it as best-effort.
        // - The refresh is triggerable by a chatty remote peer (repeated
        //   requests pin its slot), so eviction protection is not absolute.
        //   The thread cap still holds, which is the security-critical
        //   property.
        if let Ok(mut state) = self.state.lock_safe()
            && state.peers.contains_key(username)
        {
            state.order.retain(|u| u != username);
            state.order.push_back(username.to_string());
        }

        handle.send(message)
    }

    pub fn queue_upload(&self, username: &str, filename: String) -> Result<(), String> {
        self.send_to_peer(username, PeerMessage::QueueUpload(filename))
    }
}

impl Clone for PeerRegistry {
    fn clone(&self) -> Self {
        Self {
            state: self.state.clone(),
            actor_system: self.actor_system.clone(),
            client_channel: self.client_channel.clone(),
            own_username: self.own_username.clone(),
            max_peers: self.max_peers,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::PeerRegistry;
    use crate::actor::ActorSystem;
    use crate::peer::{ConnectionType, Peer};
    use crate::utils::lock::MutexExt;
    use std::net::{TcpListener, TcpStream};
    use std::sync::Arc;

    /// Test registry with zero eviction grace so FIFO behavior can be
    /// exercised without waiting out the 30 s production grace period.
    #[allow(dead_code)]
    fn zero_grace_registry(max_peers: usize) -> PeerRegistry {
        let system = Arc::new(ActorSystem::new());
        let (tx, _rx) = std::sync::mpsc::channel();
        let registry = PeerRegistry::with_max_peers(system, tx, "me".to_string(), max_peers);
        // Collapse the grace period by backdating every registered entry.
        // register_peer stores Instant::now() internally, so instead we
        // simply re-register after a tiny sleep in the eviction test below.
        registry
    }

    /// Backdate every entry in the registry past the eviction grace period
    /// so the next capacity-triggering registration evicts the oldest.
    fn backdate_all(registry: &PeerRegistry) {
        let mut state = registry.state.lock_safe().unwrap();
        let old = std::time::Instant::now()
            .checked_sub(super::EVICTION_GRACE_PERIOD * 2)
            .unwrap();
        for (_, _, registered) in state.peers.values_mut() {
            *registered = old;
        }
    }

    #[test]
    fn remove_peer_if_respects_actor_identity() {
        let system = Arc::new(ActorSystem::new());
        let (tx, _rx) = std::sync::mpsc::channel();
        let registry = PeerRegistry::new(system, tx, "me".to_string());

        // A real loopback connection makes the actor inbound (no dial-out);
        // non-blocking so it can process Stop promptly on teardown.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let stream = TcpStream::connect(addr).unwrap();
        stream.set_nonblocking(true).unwrap();
        let _server_side = listener.accept().unwrap().0;

        let peer = Peer::new(
            "bob".to_string(),
            ConnectionType::P,
            "127.0.0.1".to_string(),
            u32::from(addr.port()),
            None,
            0,
            0,
            0,
        );
        registry.register_peer(peer, Some(stream), None).unwrap();
        assert!(registry.contains("bob"));

        // A stale / wrong id must not evict the live actor.
        assert!(registry.remove_peer_if("bob", u64::MAX).is_none());
        assert!(registry.contains("bob"));

        // Unconditional removal still works (and stops the actor).
        let handle = registry.remove_peer("bob");
        assert!(handle.is_some());
        let _ = handle.unwrap().stop();
        assert!(!registry.contains("bob"));
    }

    // The thread-per-peer design floods the process when the Soulseek server
    // pushes a ConnectToPeer for every search result (measured: 487 peers
    // from one album search). Each peer owns an OS thread for its lifetime,
    // so an unbounded registry ballooned to 1600+ threads and the crate's
    // ops loop died (thread::spawn EAGAIN), killing all downloads. The cap
    // bounds the registry so only a sane number of peer threads exist.
    #[test]
    fn register_peer_evicts_oldest_beyond_cap() {
        let system = Arc::new(ActorSystem::new());
        let (tx, _rx) = std::sync::mpsc::channel();
        let registry = PeerRegistry::with_max_peers(system, tx, "me".to_string(), 2);

        let peers: Vec<Peer> = (0..4)
            .map(|i| {
                let listener = TcpListener::bind("127.0.0.1:0").unwrap();
                let addr = listener.local_addr().unwrap();
                let stream = TcpStream::connect(addr).unwrap();
                stream.set_nonblocking(true).unwrap();
                let _server = listener.accept().unwrap().0;
                Peer::new(
                    format!("peer{i}"),
                    ConnectionType::P,
                    "127.0.0.1".to_string(),
                    u32::from(addr.port()),
                    None,
                    0,
                    0,
                    0,
                )
            })
            .collect();

        assert!(
            registry
                .register_peer(peers[0].clone(), Some(peer_stream("p0")), None)
                .is_ok()
        );
        assert!(
            registry
                .register_peer(peers[1].clone(), Some(peer_stream("p1")), None)
                .is_ok()
        );
        // Backdate the entries past the eviction grace period so the FIFO
        // policy (rather than the grace refusal) decides eviction.
        backdate_all(&registry);
        // At capacity: peer2 evicts peer0 (FIFO), peer3 evicts peer1.
        assert!(
            registry
                .register_peer(peers[2].clone(), Some(peer_stream("p2")), None)
                .is_ok()
        );
        assert!(
            registry
                .register_peer(peers[3].clone(), Some(peer_stream("p3")), None)
                .is_ok()
        );

        assert!(!registry.contains("peer0"));
        assert!(!registry.contains("peer1"));
        assert!(registry.contains("peer2"));
        assert!(registry.contains("peer3"));
    }

    // Duplicate-username floods must not bypass the cap: at capacity,
    // re-registering a known username is skipped (no fresh thread spawned).
    #[test]
    fn register_peer_skips_replacement_at_capacity() {
        let system = Arc::new(ActorSystem::new());
        let (tx, _rx) = std::sync::mpsc::channel();
        let registry = PeerRegistry::with_max_peers(system, tx, "me".to_string(), 1);

        let make_peer = |name: &str| {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let stream = TcpStream::connect(addr).unwrap();
            stream.set_nonblocking(true).unwrap();
            let _server = listener.accept().unwrap().0;
            Peer::new(
                name.to_string(),
                ConnectionType::P,
                "127.0.0.1".to_string(),
                u32::from(addr.port()),
                None,
                0,
                0,
                0,
            )
        };

        let first = registry
            .register_peer(make_peer("bob"), Some(peer_stream("bob")), None)
            .unwrap();
        // Same username again at capacity: replacement skipped, still one entry.
        let second = registry
            .register_peer(make_peer("bob"), Some(peer_stream("bob")), None)
            .unwrap();
        assert!(registry.contains("bob"));
        // Settle past the actor's Stop-drain window (the loop ticks every
        // ~100 ms): only then does a send into a replaced-and-stopped actor
        // actually fail, which is what distinguishes "skip" from "replace".
        std::thread::sleep(std::time::Duration::from_millis(300));
        // The ORIGINAL actor must still be alive: if the skip guard were
        // removed and a replacement actor spawned, the first handle's channel
        // would be closed (old actor stopped) and this send would fail.
        first
            .send(crate::actor::peer_actor::PeerMessage::QueueUpload(
                "probe.flac".to_string(),
            ))
            .expect("original actor must survive the skipped replacement");
        // And the returned handle for the skipped registration is the same
        // live actor, not a fresh one.
        second
            .send(crate::actor::peer_actor::PeerMessage::QueueUpload(
                "probe2.flac".to_string(),
            ))
            .expect("skipped registration must return a live handle");
    }

    fn peer_stream(_name: &str) -> TcpStream {
        // Reuse a fresh loopback stream; the registry accepts any stream.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let stream = TcpStream::connect(addr).unwrap();
        stream.set_nonblocking(true).unwrap();
        let _server = listener.accept().unwrap().0;
        stream
    }

    // A dial that is refused reports its terminal outcome almost instantly —
    // possibly while register_peer is still between spawn and insert. The
    // registry holds its lock across both, so the eviction that follows must
    // always find the entry; a miss left a permanent zombie claiming the
    // username.
    #[test]
    fn refused_dial_does_not_leave_a_zombie_entry() {
        use crate::client::ClientOperation;

        // A bind-then-drop port can be re-used by another process before the
        // dial, in which case the dial succeeds and no PeerConnectFailed
        // arrives. Retry with a fresh port when that happens (rare flake).
        for attempt in 0..3u8 {
            let system = Arc::new(ActorSystem::new());
            let (tx, rx) = std::sync::mpsc::channel();
            let registry = PeerRegistry::new(system, tx, "me".to_string());

            let port = {
                let probe = TcpListener::bind("127.0.0.1:0").unwrap();
                probe.local_addr().unwrap().port()
            };

            let peer = Peer::new(
                format!("ghost{attempt}"),
                ConnectionType::P,
                "127.0.0.1".to_string(),
                u32::from(port),
                None,
                0,
                0,
                0,
            );
            let username = peer.username.clone();
            registry.register_peer(peer, None, None).unwrap();

            match rx.recv_timeout(std::time::Duration::from_secs(10)) {
                Ok(ClientOperation::PeerConnectFailed(id, name)) => {
                    assert_eq!(name, username);
                    if let Some(handle) = registry.remove_peer_if(&username, id) {
                        let _ = handle.stop();
                    }
                    assert!(
                        !registry.contains(&username),
                        "a refused dial must not leave a registry entry"
                    );
                    return;
                }
                // Port was re-used and the dial succeeded — try a fresh one.
                Ok(_) => {}
                Err(err) => {
                    panic!("timed out waiting for PeerConnectFailed: {err}")
                }
            }
        }
        panic!("could not produce a refused dial after 3 attempts");
    }
}
