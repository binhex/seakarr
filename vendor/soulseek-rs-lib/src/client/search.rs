use super::{
    Arc, AtomicBool, Client, DEFAULT_WISHLIST_INTERVAL, Duration, HashMap, Instant, Ordering,
    Result, RwLockExt, Search, SearchResult, ServerMessage, SoulseekRs, debug, info, md5, sleep,
};

impl Client {
    pub fn search(&self, query: &str, timeout: Duration) -> Result<Vec<SearchResult>> {
        self.search_with_cancel(query, timeout, None)
    }

    /// Send `query` as a wishlist search (server code 103) and return at once.
    ///
    /// Results accumulate under `query`, so the caller reads them back with
    /// [`Client::get_search_results`] after waiting however long it wants to.
    /// Starting several wishes and then waiting once is the whole point: waiting
    /// per wish would cost one full search window each.
    ///
    /// The server rate-limits these to the interval it announced — see
    /// [`Client::wishlist_interval`] — so a wish re-sent sooner than that comes
    /// back empty.
    ///
    /// # Errors
    /// [`SoulseekRs::NotConnected`] when there is no server connection.
    pub fn start_wishlist_search(&self, query: &str) -> Result<()> {
        self.send_search(query, true)
    }

    /// How long to wait between wishlist searches: what the server announced in
    /// code 104, or [`DEFAULT_WISHLIST_INTERVAL`] until it has.
    #[must_use]
    pub fn wishlist_interval(&self) -> Duration {
        self.context
            .read_safe()
            .ok()
            .and_then(|ctx| ctx.wishlist_interval)
            .map_or(DEFAULT_WISHLIST_INTERVAL, |seconds| {
                Duration::from_secs(u64::from(seconds))
            })
    }

    pub fn search_with_cancel(
        &self,
        query: &str,
        timeout: Duration,
        cancel_flag: Option<Arc<AtomicBool>>,
    ) -> Result<Vec<SearchResult>> {
        self.send_search(query, false)?;
        Self::collect_for(timeout, cancel_flag);
        let results = self.get_search_results(query);
        // A session lost during the collect window must not surface as
        // silently-empty results — indistinguishable from "no matches" — but
        // it must also not discard results that DID arrive before the drop.
        // Only fail fast when the window ended with nothing collected.
        if results.is_empty() && self.session_loss().is_some() {
            return Err(SoulseekRs::NotConnected);
        }
        Ok(results)
    }

    /// Register `query` and put its search on the wire. Returns as soon as the
    /// message is queued; nothing has answered yet.
    fn send_search(&self, query: &str, wishlist: bool) -> Result<()> {
        // Fail fast if the session is already lost: the actor stays alive after
        // a disconnect and would otherwise queue the search forever.
        if self.session_loss().is_some() {
            return Err(SoulseekRs::NotConnected);
        }
        debug!("Searching for {}", query);

        let Some(handle) = &self.server_handle else {
            return Err(SoulseekRs::NotConnected);
        };
        let hash = md5::md5(query);
        let token = u32::from_str_radix(&hash[0..5], 16)?;

        self.context.write_safe()?.searches.insert(
            query.to_string(),
            Search {
                token,
                results: Vec::new(),
            },
        );

        let query = query.to_string();
        let _ = handle.send(if wishlist {
            ServerMessage::WishlistSearch { token, query }
        } else {
            ServerMessage::FileSearch { token, query }
        });
        Ok(())
    }

    /// Let responses accumulate for `timeout`, or until cancelled.
    ///
    /// Peers answer a search over their own connections whenever they get round
    /// to it, so there is nothing to await — the window is the whole protocol.
    /// Late responders are often the better sources, which is why this runs the
    /// window out rather than returning on the first hit.
    pub fn collect_for(timeout: Duration, cancel_flag: Option<Arc<AtomicBool>>) {
        let start = Instant::now();
        while start.elapsed() < timeout {
            sleep(Duration::from_millis(100));
            if let Some(flag) = &cancel_flag
                && flag.load(Ordering::Relaxed)
            {
                info!("Search cancelled by user");
                return;
            }
        }
    }

    #[must_use]
    pub fn get_search_results_count(&self, search_key: &str) -> usize {
        self.context
            .read_safe()
            .ok()
            .and_then(|ctx| ctx.searches.get(search_key).map(|s| s.results.len()))
            .unwrap_or(0)
    }

    #[must_use]
    pub fn get_search_results(&self, search_key: &str) -> Vec<SearchResult> {
        self.context
            .read_safe()
            .ok()
            .and_then(|ctx| ctx.searches.get(search_key).map(|s| s.results.clone()))
            .unwrap_or_default()
    }

    /// Non-blocking variant that returns None if the lock is unavailable
    #[must_use]
    pub fn try_get_search_results(&self, search_key: &str) -> Option<Vec<SearchResult>> {
        self.context
            .try_read()
            .ok()
            .and_then(|ctx| ctx.searches.get(search_key).map(|s| s.results.clone()))
    }

    /// Drop a search and everything it collected.
    ///
    /// Returns whether there was one to drop. A client that stays up for days
    /// would otherwise hold every result set it has ever seen, and a caller
    /// that has dismissed a search has no other way to say so.
    #[must_use]
    pub fn forget_search(&self, search_key: &str) -> bool {
        self.context
            .write_safe()
            .is_ok_and(|mut ctx| ctx.searches.remove(search_key).is_some())
    }

    #[must_use]
    pub fn get_all_searches(&self) -> HashMap<String, Search> {
        self.context
            .read_safe()
            .map(|ctx| ctx.searches.clone())
            .unwrap_or_default()
    }

    /// Every registered search and how many files it has collected, without
    /// copying the result sets.
    ///
    /// This is the accessor to poll. [`Client::get_all_searches`] clones every
    /// result of every search, which for a popular query is megabytes per
    /// call — a caller that only wants to know what exists and whether
    /// anything new arrived reads the counts instead.
    #[must_use]
    pub fn search_file_counts(&self) -> Vec<(String, usize)> {
        self.context
            .read_safe()
            .map(|ctx| {
                ctx.searches
                    .iter()
                    .map(|(query, search)| {
                        (
                            query.clone(),
                            search.results.iter().map(|result| result.files.len()).sum(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actor::ActorHandle;
    use crate::client::ClientSettings;
    use crate::types::SessionLoss;

    // Regression guard: after a session loss the client must fail fast on
    // search instead of queueing it in the still-alive server actor (which
    // silently burns the full search timeout and returns nothing).
    #[test]
    fn send_search_returns_not_connected_after_session_loss() {
        let mut client = Client::with_settings(ClientSettings::new("test-user", "test-pass"));
        // Simulate a connection that was established and then lost: the actor
        // handle exists (as after connect()), but the session is recorded lost.
        let (tx, _rx) = std::sync::mpsc::channel();
        client.server_handle = Some(ActorHandle { sender: tx });
        client.session.record(SessionLoss::Disconnected);

        assert!(matches!(
            client.send_search("some query", false),
            Err(SoulseekRs::NotConnected)
        ));
    }

    // A session lost DURING the collect window (after send_search succeeds)
    // must surface as an error, not as silently-empty results. Without the
    // post-collect check, search_with_cancel burns the full timeout and
    // returns Ok(vec![]) — indistinguishable from "no matches".
    #[test]
    fn search_with_cancel_surfaces_loss_during_collect_window() {
        let mut client = Client::with_settings(ClientSettings::new("test-user", "test-pass"));
        let (tx, _rx) = std::sync::mpsc::channel();
        client.server_handle = Some(ActorHandle { sender: tx });
        let client = Arc::new(client);

        // Record the loss shortly after send_search has queued the search,
        // while collect_for is still sleeping in its window.
        let loss_client = Arc::clone(&client);
        let loss_thread = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(10));
            loss_client.session.record(SessionLoss::Disconnected);
        });

        let result =
            client.search_with_cancel("some query", std::time::Duration::from_millis(200), None);
        loss_thread.join().unwrap();

        assert!(matches!(result, Err(SoulseekRs::NotConnected)));
    }

    // A session lost DURING the window but AFTER results arrived must still
    // return the collected results, not discard them. Discarding already-
    // collected data is a correctness regression; the loss check only exists
    // to distinguish "no matches" from "session lost".
    #[test]
    fn search_with_cancel_preserves_partial_results_on_mid_window_loss() {
        let mut client = Client::with_settings(ClientSettings::new("test-user", "test-pass"));
        let (tx, _rx) = std::sync::mpsc::channel();
        client.server_handle = Some(ActorHandle { sender: tx });
        let client = Arc::new(client);

        let loss_client = Arc::clone(&client);
        let loss_thread = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(10));
            // One peer's answer arrived before the drop.
            if let Ok(mut ctx) = loss_client.context.write_safe()
                && let Some(search) = ctx.searches.get_mut("some query")
            {
                search.results.push(SearchResult {
                    token: 0,
                    files: Vec::new(),
                    slots: 1,
                    speed: 100,
                    username: "peer".into(),
                });
            }
            loss_client.session.record(SessionLoss::Disconnected);
        });

        let result =
            client.search_with_cancel("some query", std::time::Duration::from_millis(200), None);
        loss_thread.join().unwrap();

        // The partial result must survive the mid-window loss.
        assert!(matches!(&result, Ok(results) if results.len() == 1));
    }
}
