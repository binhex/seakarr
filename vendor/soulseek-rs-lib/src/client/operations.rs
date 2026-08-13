use super::{
    Arc, BROKER_CONNECT_TIMEOUT, Client, ClientContext, ClientOperation, ConnectionType, Download,
    DownloadPeer, DownloadStatus, Peer, PeerMessage, PeerRegistry, Receiver, RwLock, RwLockExt,
    ServerMessage, build_search_response, debug, error, info, next_connect_token, sleep, thread,
    trace, warn,
};
use crate::message::server::MessageFactory;
use std::collections::HashSet;
use std::sync::{Condvar, Mutex};

/// Maximum number of ConnectToPeer handler threads that may run concurrently.
///
/// The Soulseek server pushes one ConnectToPeer per search-result peer; a
/// popular search yields hundreds. Without a bound, the ops loop spawned a
/// fresh 2 MB-stack thread per op, and a flood measured 1600+ threads before
/// the process died with "failed to set up alternative stack guard page:
/// Cannot allocate memory". Handlers acquire a permit before spawning and
/// release it on completion, so at most this many are in flight; excess ops
/// wait in the ops loop.
const MAX_CONCURRENT_PEER_CONNECTS: usize = 32;

/// Maximum number of F-type (file-transfer) ConnectToPeer handlers that may
/// run concurrently. These handlers run whole brokered downloads and are
/// bounded by the download queue in normal operation; the cap exists so a
/// hostile server emitting F-type ConnectToPeers cannot grow thread count
/// without bound.
const MAX_CONCURRENT_F_TRANSFERS: usize = 64;

/// A tiny counting semaphore (the crate is zero-dependency; std's Semaphore
/// is still unstable). Backed by a Mutex<usize> + Condvar.
struct ConnectPermitPool {
    available: Mutex<usize>,
    wake: Condvar,
}

impl ConnectPermitPool {
    const fn new(limit: usize) -> Self {
        Self {
            available: Mutex::new(limit),
            wake: Condvar::new(),
        }
    }

    fn acquire(&self) {
        let mut available = self
            .available
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while *available == 0 {
            available = self
                .wake
                .wait(available)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        *available -= 1;
    }

    fn release(&self) {
        let mut available = self
            .available
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *available += 1;
        self.wake.notify_one();
    }
}

/// Releases a pool permit on drop (RAII), so handler threads free their slot
/// when they finish, including on early returns / panics.
struct PermitGuard<'a>(&'a ConnectPermitPool);

impl Drop for PermitGuard<'_> {
    fn drop(&mut self) {
        self.0.release();
    }
}

/// Removes a download token from the in-flight set on drop (RAII), so the
/// duplicate-transfer guard cannot leak entries when a handler exits early
/// or panics.
struct ActiveDownloadGuard<'a>(&'a Mutex<HashSet<u32>>, u32);

impl Drop for ActiveDownloadGuard<'_> {
    fn drop(&mut self) {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.1);
    }
}

impl Client {
    pub(crate) fn listen_to_client_operations(
        reader: Receiver<ClientOperation>,
        client_context: Arc<RwLock<ClientContext>>,
        own_username: String,
    ) {
        thread::spawn(move || {
            // Bound the number of concurrently running ConnectToPeer
            // handlers. The Soulseek server pushes one ConnectToPeer per
            // search-result peer; a popular search yields hundreds, and an
            // unbounded thread-per-op flood (measured: 1600+ threads, then
            // an OOM abort "failed to set up alternative stack guard page")
            // killed the process. Handlers acquire a permit before spawning
            // and release it when done, so at most this many ops run
            // concurrently; excess ops wait here in the loop.
            let permits = Arc::new(ConnectPermitPool::new(MAX_CONCURRENT_PEER_CONNECTS));
            // A separate pool for F-type (file-transfer) handlers. They run
            // the whole brokered download inside the handler — dial,
            // transfer, pause waits — so letting them share the P-type pool
            // would let slow transfers stall the ops loop (head-of-line
            // blocking). A dedicated pool bounds them independently: in
            // normal operation the download queue is far below the limit,
            // and a hostile server emitting F-type ConnectToPeers can no
            // longer grow thread count without bound.
            //
            // Trade-off (bounded): if every F permit is held, the ops loop
            // blocks in acquire() until a transfer finishes. Handlers drain
            // within their dial/read timeouts, so the stall is temporary in
            // normal operation; seakarr itself never pauses transfers and
            // caps concurrent downloads far below the limit, so the pool
            // cannot be exhausted by our own traffic.
            let f_permits = Arc::new(ConnectPermitPool::new(MAX_CONCURRENT_F_TRANSFERS));
            // In-flight download tokens. All DownloadFromPeer ops pass
            // through this single-threaded loop, so the insert check is
            // atomic with respect to sibling ops; handlers remove their
            // token on completion via ActiveDownloadGuard. This closes the
            // duplicate-transfer window that a status check cannot: the
            // handler resets the download to Queued at entry and the status
            // only becomes InProgress after the dial, so a peer that knows
            // a token could replay TransferResponses and start concurrent
            // writers on the same .part file.
            let active_downloads: Arc<Mutex<HashSet<u32>>> = Arc::new(Mutex::new(HashSet::new()));
            for operation in reader {
                match operation {
                    ClientOperation::ConnectToPeer(peer) => {
                        let pooled = matches!(peer.connection_type, ConnectionType::P);
                        let pool = if pooled { &permits } else { &f_permits };
                        pool.acquire();
                        let pool_for_thread = pool.clone();
                        let client_context_clone = client_context.clone();
                        let own_username_clone = own_username.clone();

                        let result = thread::Builder::new()
                            .name("connect-to-peer".to_string())
                            .spawn(move || {
                                // Release the permit only after the handler
                                // has finished.
                                let _permit = PermitGuard(&pool_for_thread);
                                Self::connect_to_peer(
                                    peer,
                                    client_context_clone,
                                    own_username_clone,
                                    None,
                                );
                            });
                        if let Err(e) = result {
                            // Thread failed to start (EAGAIN under thread
                            // pressure): the permit was acquired but no
                            // PermitGuard was ever created, so release it
                            // here — otherwise the effective pool capacity
                            // drains with every failed spawn. Do not panic:
                            // a panic here would kill the ops loop.
                            pool.release();
                            error!("[client] failed to spawn ConnectToPeer handler: {e}");
                        }
                    }
                    ClientOperation::SearchResult(search_result) => {
                        trace!("[client] SearchResult {:?}", search_result);
                        let mut context = match client_context.write_safe() {
                            Ok(c) => c,
                            Err(e) => {
                                error!("[client] SearchResult write: {}", e);
                                continue;
                            }
                        };
                        let result_token = search_result.token;

                        // Find the search with matching token
                        for search in context.searches.values_mut() {
                            if search.token == result_token {
                                search.accept(search_result);
                                break;
                            }
                        }
                    }
                    ClientOperation::PeerDisconnected(id, username, error) => {
                        // Scope the read guard: process_failed_uploads
                        // below acquires a write lock on the same
                        // RwLock, which would self-deadlock the entire
                        // client ops loop if this read guard were still
                        // held on this thread. Evict only if this exact
                        // actor still occupies the slot, so a replaced
                        // actor's shutdown can't remove its successor.
                        {
                            let context = match client_context.read_safe() {
                                Ok(c) => c,
                                Err(e) => {
                                    error!("[client] PeerDisconnected read: {}", e);
                                    continue;
                                }
                            };
                            if let Some(ref registry) = context.peer_registry
                                && let Some(handle) = registry.remove_peer_if(&username, id)
                            {
                                let _ = handle.stop();
                            }
                        }
                        // Only an error is evidence the peer is gone. A clean
                        // close — our idle reaper, or a remote client tidying
                        // an idle socket while it waits in our queue — must
                        // not throw away everything that peer has queued: the
                        // connection comes back, the queue cannot. A departed
                        // peer wedging uploads shut is still covered: its
                        // erroring transfer fails within the socket deadlines
                        // and the error branch here frees its slots.
                        if let Some(error) = error {
                            warn!(
                                "[client] Peer {} disconnected with error: {:?}",
                                username, error
                            );
                            Self::process_failed_uploads(client_context.clone(), &username, None);
                            Self::release_upload_slots(&client_context, &username);
                        }
                    }
                    ClientOperation::DownloadFromPeer(token, peer, allowed) => {
                        let maybe_download = match client_context.read_safe() {
                            Ok(ctx) => ctx.get_download_by_token(token).cloned(),
                            Err(e) => {
                                error!("[client] DownloadFromPeer read: {}", e);
                                continue;
                            }
                        };
                        let own_username = own_username.clone();
                        let client_context_clone = client_context.clone();

                        trace!(
                            "[client] DownloadFromPeer token: {} peer: {:?}",
                            token, peer
                        );
                        let Some(download) = maybe_download else {
                            error!("Can't find download with token {:?}", token);
                            continue;
                        };

                        // Terminal-state guard: a peer we downloaded from
                        // knows the token (we sent it in our TransferRequest)
                        // and can replay TransferResponses on the still-open
                        // control connection. Never start a transfer for a
                        // download that already completed or failed — the
                        // handler would overwrite the finished file.
                        if matches!(
                            download.status,
                            DownloadStatus::Completed | DownloadStatus::Failed { .. }
                        ) {
                            debug!(
                                "[client] skipping DownloadFromPeer for token {token}: \
                                 download is in a terminal state"
                            );
                            continue;
                        }

                        // Duplicate-token guard: only start a transfer when
                        // the token is not already in flight. The status is
                        // unsuitable for this check — the handler resets it
                        // to Queued at entry and it only becomes InProgress
                        // after the dial — so track in-flight tokens in a
                        // set. Two concurrent download_file calls for one
                        // token both open the same .part file in append mode
                        // and interleave writes, silently corrupting the
                        // download.
                        {
                            let mut active = active_downloads
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            if !active.insert(token) {
                                debug!(
                                    "[client] skipping DownloadFromPeer for token {token}: \
                                     transfer already in flight"
                                );
                                continue;
                            }
                        }

                        // Bound the spawn like the other peer-dial handlers:
                        // a malicious peer that knows our download tokens can
                        // spam TransferResponses and grow dialing threads
                        // without bound otherwise.
                        f_permits.acquire();
                        let pool_for_thread = f_permits.clone();
                        let active_for_thread = active_downloads.clone();
                        let result = thread::Builder::new()
                            .name("download-from-peer".to_string())
                            .spawn(move || {
                                let _active = ActiveDownloadGuard(&active_for_thread, token);
                                let _permit = PermitGuard(&pool_for_thread);
                                let download_peer = DownloadPeer::new(
                                    download.username.clone(),
                                    peer.host.clone(),
                                    peer.port,
                                    token,
                                    allowed,
                                    own_username,
                                );
                                let Some(filename) =
                                    download.filename.split('\\').next_back()
                                else {
                                    error!(
                                        "Cant find filename to save download: {:?}",
                                        download.filename
                                    );
                                    return;
                                };
                                match download_peer.download_file(
                                    client_context_clone.clone(),
                                    Some(download.clone()),
                                    None,
                                ) {
                                    Ok((download, filename)) => {
                                        let _ = download.sender.send(DownloadStatus::Completed);
                                        match client_context_clone.write_safe() {
                                            Ok(mut ctx) => ctx.update_download_with_status(
                                                download.token,
                                                DownloadStatus::Completed,
                                            ),
                                            Err(e) => {
                                                error!("[client] download complete write: {}", e);
                                            }
                                        }
                                        info!(
                                            "Successfully downloaded {} bytes to {}",
                                            download.size, filename
                                        );
                                    }
                                    Err(e) => {
                                        let reason = Some(e.to_string());
                                    let _ = download
                                        .sender
                                        .send(DownloadStatus::Failed(reason.clone()));
                                    match client_context_clone.write_safe() {
                                        Ok(mut ctx) => ctx.update_download_with_status(
                                            download.token,
                                            DownloadStatus::Failed(reason),
                                        ),
                                        Err(e) => error!("[client] download failed write: {}", e),
                                    }
                                    error!(
                                        "Failed to download file '{}' from {}:{} (token: {}) - Error: {}",
                                        filename, peer.host, peer.port, download.token, e
                                    );
                                }
                            }
                            });
                        if let Err(e) = result {
                            // Same permit-leak guard as the ConnectToPeer
                            // branch: the permit was acquired but no
                            // PermitGuard was created. Also release the
                            // in-flight token — no handler will run to
                            // remove it.
                            f_permits.release();
                            active_downloads
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .remove(&token);
                            error!("[client] failed to spawn download handler: {e}");
                        }
                    }
                    ClientOperation::GetPeerAddressResponse {
                        username,
                        host,
                        port,
                        obfuscation_type,
                        obfuscated_port,
                    } => {
                        debug!(
                            "Received peer address for {}: {}:{} (obf_type: {}, obf_port: {})",
                            username, host, port, obfuscation_type, obfuscated_port
                        );

                        // Port 0 is the server saying it does not know
                        // where this user listens (no SetWaitPort yet,
                        // or firewalled). It is not an address: caching
                        // it poisons every later lookup, and an upload
                        // dialled at it dies with "Can't assign
                        // requested address" and is dropped on the
                        // floor. Leave those uploads queued for a later
                        // resolution instead.
                        //
                        // The connect attempt further down still runs:
                        // its failure is exactly what makes an
                        // unreachable peer fall back to the
                        // server-brokered path.
                        let waiting_serves = if port == 0 {
                            warn!("[client] server reports no listening port for {}", username);
                            Vec::new()
                        } else {
                            match client_context.write_safe() {
                                Ok(mut ctx) => {
                                    ctx.cache_peer_address(&username, host.clone(), port);
                                    ctx.pending_serves.remove(&username).unwrap_or_default()
                                }
                                Err(_) => Vec::new(),
                            }
                        };
                        for token in waiting_serves {
                            Self::spawn_serve(
                                &client_context,
                                &own_username,
                                token,
                                host.clone(),
                                port,
                            );
                        }

                        let peer_exists = match client_context.read_safe() {
                            Ok(ctx) => ctx
                                .peer_registry
                                .as_ref()
                                .is_some_and(|r| r.contains(&username)),
                            Err(e) => {
                                error!("[client] GetPeerAddressResponse read: {}", e);
                                continue;
                            }
                        };

                        // Existing peer: skip re-registration. Reconnect
                        // policy on conflict is intentionally undecided.
                        if !peer_exists {
                            let peer = Peer::new(
                                username,
                                ConnectionType::P,
                                host,
                                port,
                                None,
                                // The `privileged` field is dead (never
                                // consumed); pass 0 instead of conflating
                                // it with obfuscation_type.
                                0,
                                0,
                                obfuscated_port,
                            );
                            let client_context_clone = client_context.clone();
                            let own_username_clone = own_username.clone();

                            permits.acquire();
                            let permits_for_thread = permits.clone();
                            let result = thread::Builder::new()
                                .name("get-peer-address".to_string())
                                .spawn(move || {
                                    let _permit = PermitGuard(&permits_for_thread);
                                    Self::connect_to_peer(
                                        peer,
                                        client_context_clone,
                                        own_username_clone,
                                        None,
                                    );
                                });
                            if let Err(e) = result {
                                // Same permit-leak guard as the
                                // ConnectToPeer branch: the permit was
                                // acquired but no PermitGuard was created.
                                permits.release();
                                error!(
                                    "[client] failed to spawn peer-address connect handler: {e}"
                                );
                            }
                        }
                    }
                    ClientOperation::UpdateDownloadTokens(transfer, username) => {
                        let mut context = match client_context.write_safe() {
                            Ok(c) => c,
                            Err(e) => {
                                error!("[client] UpdateDownloadTokens write: {}", e);
                                continue;
                            }
                        };

                        let download_to_update = context.get_downloads().iter().find_map(|d| {
                            if d.username == username && d.filename == transfer.filename {
                                Some((d.token, d.clone()))
                            } else {
                                None
                            }
                        });

                        if let Some((old_token, download)) = download_to_update {
                            trace!(
                                "[client] UpdateDownloadTokens found {old_token}, transfer: {:?}",
                                transfer
                            );

                            context.add_download(Download {
                                token: transfer.token,
                                size: transfer.size,
                                ..download
                            });
                            context.remove_download(old_token);
                        }

                        // Only now invite the file connection: it is
                        // matched by this token, which is recorded as
                        // of the line above. Answering any earlier
                        // races the peer's connection against our own
                        // bookkeeping.
                        let registry = context.peer_registry.clone();
                        drop(context);
                        if let Some(registry) = registry {
                            let response =
                                MessageFactory::build_transfer_response_message(transfer);
                            let _ = registry
                                .send_to_peer(&username, PeerMessage::SendMessage(response));
                        }
                    }
                    ClientOperation::UploadFailed(username, filename) => {
                        Self::process_failed_uploads(
                            client_context.clone(),
                            &username,
                            Some(&filename),
                        );
                    }
                    ClientOperation::PlaceInQueueUpdate {
                        username,
                        filename,
                        place,
                    } => match client_context.write_safe() {
                        Ok(mut ctx) => {
                            let updated = ctx
                                .downloads
                                .update_queue_position(&username, &filename, place);
                            if !updated {
                                debug!(
                                    "[client] PlaceInQueueUpdate: no matching download for {}/{}",
                                    username, filename
                                );
                            }
                        }
                        Err(e) => {
                            error!("[client] PlaceInQueueUpdate write: {}", e);
                        }
                    },
                    ClientOperation::SetServerSender(sender) => match client_context.write_safe() {
                        Ok(mut ctx) => {
                            ctx.server_sender = Some(sender);
                            debug!("[client] Server sender initialized");
                        }
                        Err(e) => {
                            error!("[client] SetServerSender write: {}", e);
                        }
                    },
                    ClientOperation::PrivateMessageReceived(user_message) => {
                        match client_context.write_safe() {
                            Ok(mut ctx) => {
                                ctx.push_private_message(user_message);
                            }
                            Err(e) => error!("[client] PrivateMessageReceived write: {}", e),
                        }
                    }
                    ClientOperation::UserStatusReceived {
                        username,
                        status,
                        privileged,
                    } => match client_context.write_safe() {
                        Ok(mut ctx) => {
                            ctx.apply_user_status(username, status, privileged);
                        }
                        Err(e) => {
                            error!("[client] UserStatusReceived write: {}", e);
                        }
                    },
                    ClientOperation::UserStatsReceived {
                        username,
                        average_speed,
                        shared_files,
                        shared_folders,
                    } => match client_context.write_safe() {
                        Ok(mut ctx) => ctx.apply_user_stats(
                            username,
                            average_speed,
                            shared_files,
                            shared_folders,
                        ),
                        Err(e) => {
                            error!("[client] UserStatsReceived write: {}", e);
                        }
                    },
                    ClientOperation::RoomEvent(event) => match client_context.write_safe() {
                        Ok(mut ctx) => ctx.apply_room_event(event),
                        Err(e) => error!("[client] RoomEvent write: {}", e),
                    },
                    ClientOperation::WishlistInterval(seconds) => {
                        if let Ok(mut ctx) = client_context.write_safe() {
                            ctx.wishlist_interval = Some(seconds);
                        }
                    }
                    ClientOperation::PeerConnected(username) => {
                        // A control connection just came up — one we dialled,
                        // or an inbound one the listener registered. Flush any
                        // downloads that were queued for this peer while it
                        // was unreachable. Collect under a read guard, then
                        // act without it held.
                        let (registry, files): (Option<PeerRegistry>, Vec<String>) =
                            match client_context.read_safe() {
                                Ok(ctx) => (
                                    ctx.peer_registry.clone(),
                                    ctx.get_downloads()
                                        .iter()
                                        .filter(|d| {
                                            d.username == username
                                                && matches!(d.status, DownloadStatus::Queued)
                                        })
                                        .map(|d| d.filename.clone())
                                        .collect(),
                                ),
                                Err(e) => {
                                    error!("[client] PeerConnected read: {}", e);
                                    continue;
                                }
                            };
                        // Also flush any peer messages (e.g. search
                        // responses) queued while connecting.
                        let queued_messages = client_context
                            .write_safe()
                            .map(|mut ctx| ctx.take_peer_messages(&username))
                            .unwrap_or_default();
                        if let Some(registry) = registry {
                            for filename in files {
                                let _ = registry.queue_upload(&username, filename);
                            }
                            for message in queued_messages {
                                let _ = registry
                                    .send_to_peer(&username, PeerMessage::SendMessage(message));
                            }
                        }
                    }
                    ClientOperation::IncomingSearch {
                        username,
                        token,
                        query,
                    } => {
                        // Don't answer our own distributed search.
                        if username == own_username {
                            continue;
                        }
                        let response = match client_context.read_safe() {
                            Ok(ctx) => {
                                build_search_response(&ctx.shares, &own_username, token, &query)
                            }
                            Err(e) => {
                                error!("[client] IncomingSearch read: {}", e);
                                continue;
                            }
                        };
                        let Some(message) = response else {
                            continue; // no matching shares
                        };

                        // Deliver to the searcher: send now if we have a
                        // control connection, else open one and queue.
                        let (connected, registry, server_sender) = match client_context.read_safe()
                        {
                            Ok(ctx) => (
                                ctx.peer_registry
                                    .as_ref()
                                    .is_some_and(|r| r.contains(&username)),
                                ctx.peer_registry.clone(),
                                ctx.server_sender.clone(),
                            ),
                            Err(_) => continue,
                        };
                        if connected {
                            if let Some(registry) = registry {
                                let _ = registry
                                    .send_to_peer(&username, PeerMessage::SendMessage(message));
                            }
                        } else {
                            if let Ok(mut ctx) = client_context.write_safe() {
                                ctx.queue_peer_message(&username, message);
                            }
                            if let Some(sender) = server_sender {
                                let _ = sender.send(ServerMessage::GetPeerAddress(username));
                            }
                        }
                    }
                    ClientOperation::QueueUpload {
                        requester_key,
                        filename,
                    } => {
                        // The peer served next may not be this one.
                        match client_context.write_safe() {
                            Ok(mut ctx) => {
                                let Some(file) = ctx.shares.get(&filename) else {
                                    debug!("[client] QueueUpload for unknown file {}", filename);
                                    continue;
                                };
                                let size = file.size;
                                let real_path = file.real_path.clone();
                                ctx.enqueue_upload(&requester_key, &filename, real_path, size);
                            }
                            Err(e) => {
                                error!("[client] QueueUpload write: {}", e);
                                continue;
                            }
                        }
                        Self::pump_upload_queue(&client_context);
                    }
                    ClientOperation::PlaceInQueueRequested {
                        requester_key,
                        filename,
                    } => {
                        let (registry, place) = match client_context.read_safe() {
                            Ok(ctx) => (
                                ctx.peer_registry.clone(),
                                ctx.place_in_queue(&requester_key, &filename),
                            ),
                            Err(_) => continue,
                        };
                        // Silence would leave the peer guessing; a file
                        // no longer queued is being served, which is
                        // place 0 by the same convention Nicotine+ uses.
                        if let Some(registry) = registry {
                            let _ = registry.send_to_peer(
                                &requester_key,
                                PeerMessage::SendMessage(
                                    MessageFactory::build_place_in_queue_response(
                                        &filename,
                                        place.unwrap_or(0),
                                    ),
                                ),
                            );
                        }
                    }
                    ClientOperation::PrivilegedUsers(users) => {
                        if let Ok(mut ctx) = client_context.write_safe() {
                            debug!("[client] {} privileged users listed", users.len());
                            ctx.set_privileged_users(users);
                        }
                        // Someone already waiting may have just been
                        // outranked, but a slot that is free should
                        // still be filled.
                        Self::pump_upload_queue(&client_context);
                    }
                    ClientOperation::OwnPrivileges(seconds) => {
                        if let Ok(mut ctx) = client_context.write_safe() {
                            ctx.own_privileges = Some(seconds);
                        }
                    }
                    ClientOperation::StartUpload { token } => {
                        // The peer accepted our offer: resolve their
                        // address (from the code-9 GetPeerAddress) and
                        // stream the file, or queue until it resolves.
                        let (job_addr, downloader) = match client_context.read_safe() {
                            Ok(ctx) => {
                                let Some(job) = ctx.uploads.get(&token) else {
                                    continue;
                                };
                                (ctx.peer_address(&job.downloader), job.downloader.clone())
                            }
                            Err(_) => continue,
                        };
                        if let Some((host, port)) = job_addr {
                            Self::spawn_serve(&client_context, &own_username, token, host, port);
                        } else {
                            if let Ok(mut ctx) = client_context.write_safe() {
                                ctx.pending_serves
                                    .entry(downloader.clone())
                                    .or_default()
                                    .push(token);
                            }
                            if let Ok(ctx) = client_context.read_safe()
                                && let Some(sender) = ctx.server_sender.clone()
                            {
                                let _ = sender.send(ServerMessage::GetPeerAddress(downloader));
                            }
                        }
                    }
                    ClientOperation::ShareListRequested { requester_key } => {
                        // Reply with our full shared-file listing.
                        let (registry, message) = match client_context.read_safe() {
                            Ok(ctx) => {
                                let dirs =
                                    ctx.shares
                                        .directories()
                                        .into_iter()
                                        .map(|(name, files)| {
                                            crate::message::peer::SharedDirectory { name, files }
                                        })
                                        .collect::<Vec<_>>();
                                (
                                    ctx.peer_registry.clone(),
                                    crate::message::peer::build_shared_file_list(&dirs),
                                )
                            }
                            Err(_) => continue,
                        };
                        if let Some(registry) = registry {
                            let _ = registry
                                .send_to_peer(&requester_key, PeerMessage::SendMessage(message));
                        }
                    }
                    ClientOperation::BrowseResult {
                        username,
                        directories,
                    } => {
                        if let Ok(mut ctx) = client_context.write_safe() {
                            ctx.store_browse_result(username, directories);
                        }
                    }
                    ClientOperation::PeerConnectFailed(id, username) => {
                        // Direct connect failed: ask the server to
                        // broker it. Register a correlation token, then
                        // send ConnectToPeer so the (firewalled) peer
                        // connects back to our listener quoting it.
                        let token = next_connect_token();
                        let server_sender = match client_context.write_safe() {
                            Ok(mut ctx) => {
                                // Reap the dead outbound actor so it
                                // stops pinning a pool worker and no
                                // longer shadows the brokered reconnect
                                // (a stale registry entry would make
                                // later downloads queue into a dead,
                                // streamless actor and hang). Identity-
                                // aware so a newer namesake is untouched.
                                if let Some(handle) = ctx
                                    .peer_registry
                                    .as_ref()
                                    .and_then(|r| r.remove_peer_if(&username, id))
                                {
                                    let _ = handle.stop();
                                }
                                ctx.add_pending_connect(token, username.clone());
                                ctx.server_sender.clone()
                            }
                            Err(e) => {
                                error!("[client] PeerConnectFailed write: {}", e);
                                continue;
                            }
                        };
                        let Some(sender) = server_sender else {
                            continue;
                        };
                        let msg = crate::message::server::MessageFactory::build_connect_to_peer(
                            token,
                            &username,
                            ConnectionType::P,
                        );
                        let _ = sender.send(ServerMessage::SendMessage(msg));

                        // Bound the brokered attempt: if no PierceFirewall
                        // consumes the token, fail the peer's queued
                        // downloads (so the caller's Receiver unblocks)
                        // and reclaim the token. A successful pierce
                        // takes the token first, making this a no-op.
                        //
                        // Skip the reaper entirely when the peer has no
                        // queued downloads: the 20s-sleeping thread would
                        // be pure overhead, and fast-failing dial floods
                        // could otherwise keep many of them alive at once.
                        let has_queued_downloads = client_context.read_safe().is_ok_and(|ctx| {
                            ctx.get_downloads().iter().any(|d| {
                                d.username == username && matches!(d.status, DownloadStatus::Queued)
                            })
                        });
                        if !has_queued_downloads {
                            continue;
                        }
                        let timeout_ctx = client_context.clone();
                        let timeout_user = username.clone();
                        thread::spawn(move || {
                            sleep(BROKER_CONNECT_TIMEOUT);
                            let still_pending = timeout_ctx
                                .write_safe()
                                .is_ok_and(|mut c| c.take_pending_connect(token).is_some());
                            if still_pending {
                                Self::fail_queued_downloads(&timeout_ctx, &timeout_user);
                            }
                        });
                    }
                }
            }
        });
    }
}
