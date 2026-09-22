<!-- markdownlint-disable MD013 -->
<!-- markdownlint-disable MD024 -->
# Queue position refresh Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use sub-agents (recommended) to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the interactive queue bar's position number update in place while a download waits in a peer's upload queue, instead of showing the first reported position forever.

**Architecture:** The vendored crate's peer actor owns the protocol request (`PlaceInQueueRequest`) but only sends it every five minutes, which is the same order as the queue limits that bound a wait. A new `PeerMessage::RequestQueuePosition` lets a consumer ask the actor to send that same request on demand, exposed as `Client::request_place_in_queue`. seakarr gains a 30-second `QueueWait` deadline checked at the top of its existing status poll loop, which calls that method while the attempt is queued; the peer's answer already flows back through `PlaceInQueueResponse` → `DownloadStore::update_queue_position` → `QueueWait::observe` → `ProgressDisplay::update_queue_bar`, which re-renders the bar in place. No log line is added at `INFO` or above, and the bar's label text is unchanged.

**Tech Stack:** Rust 2021 (seakarr, workspace root) and edition 2024 (vendored `soulseek-rs-lib` member), Tokio with paused-time tests, `indicatif` 0.17 progress bars, `tracing` for logs, `async-trait` for the client trait.

**Design record:** `docs/agent/specs/2026-09-22-queue-position-refresh-design.md`.

---

## Scope check

Single subsystem: the download queue-wait path and the vendored peer actor's position request. No split is needed. The two crates are separate files but one change; the vendor half is useless without the seakarr half and vice versa, so they are planned as ordered tasks in one plan rather than separate plans.

## Spec refinements made during planning

The approved spec said the new trait method would be synchronous. It cannot be: `RealClient`
reaches the crate client through `tokio::sync::Mutex<Option<Arc<Client>>>` (`src/client.rs:273-275`),
and locking that from a synchronous method would need `try_lock`, which fails while another task
holds it and would silently drop refreshes. The spec was amended (commit `6b7eccd`) so the method
is `async`; the call itself does no I/O and never fails.

## Amendments applied during implementation

This plan is the execution record, so where the code deliberately diverges from it, the
amendment is recorded here. The design of record is
`docs/agent/specs/2026-09-22-queue-position-refresh-design.md`, which was updated for each of
these.

- **Task 1** — the actor's on-demand ask skips at `DEBUG` when the actor has no control stream,
  instead of going through `send_message`, which logs an error. Otherwise a registered peer
  whose connection just went away would log one error per 30-second ask until the client-ops
  thread evicted it.
- **Task 3** — `MockClient` does **not** record asks and does **not** return `true`: the
  recording field and its self-referential test were removed by the tech-debt step as a test
  that only exercised its own fixture, and the double now answers `false` because it models no
  peer actor. The cadence assertions live on `ScriptedClient`, which records every ask. The
  plan's Task 3 Steps 1 and 4 therefore describe a shape that no longer exists.
- **Task 5** — the ask sits **after** the loop's exit checks (cancellation, transfer inactivity,
  queue-deadline expiry) and before the status poll, not immediately after the notice check.
  Placing it after the expiry checks is what makes "no ask after a queue timeout" literally
  true; the `!transfer.has_started()` gate is load-bearing because the loop keeps polling after
  the transfer starts.
- **Task 6** — the download-level bar test asserts counters (`created=1, updated=2,
  finished=1`), not rendered label text: the bar's message is not reachable from that level. The
  in-place text replacement is asserted at the display level in `src/progress.rs`, and the test
  was renamed to `reported_positions_update_the_queue_bar_in_place` because it pins the bar's
  update contract, not the ask that produces fresh positions.
- **Tasks 5 and 7** — the 95 s cap in the cadence test (rather than the plan's 90 s) and the
  extra regression test for a wait that ends at the cap were added while fixing review findings.

## File structure

| File | Change | Responsibility |
| --- | --- | --- |
| `vendor/soulseek-rs-lib/src/actor/peer_actor.rs` | Modify | `PeerMessage::RequestQueuePosition` variant, its dispatch arm, and `PeerActor::request_queue_position` which sends the on-demand protocol request |
| `vendor/soulseek-rs-lib/src/client/tests.rs` | Modify | Vendor test that the on-demand ask reaches a peer actor only when one exists |
| `vendor/soulseek-rs-lib/src/client/mod.rs` | Modify | `Client::request_place_in_queue`, the public entry point that routes to the peer's actor |
| `src/client.rs` | Modify | `SoulseekClient::request_queue_position` (required, `async`), `MockClient` recording, `RealClient` delegation |
| `src/download.rs` | Modify | `QUEUE_POSITION_REFRESH` constant, `QueueWait::position_request_is_due`, the poll-loop ask, `ScriptedClient` recording |
| `src/progress.rs` | Modify | Test only: an update replaces the number in place |
| `src/runner.rs` | Modify | One-line trait implementation for `CancelAfterFirstSearchClient` |
| `README.md` | Modify | Position refresh cadence, and `max_start_time_secs` arming from a fresh report |
| `docs/agent/specs/2026-09-17-download-log-visibility-design.md` | Modify | Amendment note: the position no longer arrives only on a five-minute cadence |

No new files. No new configuration key. No database or schema change.

---

## Task 1: Vendor — the actor can be asked for a position on demand

**Files:**

- Modify: `vendor/soulseek-rs-lib/src/actor/peer_actor.rs` (enum at `:24-60`, dispatch at `:263-275`, method beside `start_queue_position_requests` at `:572`, tests at `:1027-1050`)

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` block in `peer_actor.rs`, directly after `queue_upload_requests_position_immediately_and_periodically`:

```rust
    #[test]
    fn an_on_demand_position_request_is_sent_and_leaves_the_timer_alone() {
        let (mut actor, _rx, mut far_end) = connected_actor();
        let filename = "song.mp3".to_string();

        actor.handle_message(PeerMessage::RequestQueuePosition {
            filename: filename.clone(),
        });

        let expected = MessageFactory::build_place_in_queue_request(&filename).get_buffer();
        let mut actual = vec![0; expected.len()];
        far_end.read_exact(&mut actual).unwrap();
        assert_eq!(actual, expected);
        assert!(
            !actor.queue_position_requests.contains_key(&filename),
            "an on-demand ask must not register the periodic timer"
        );
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p soulseek-rs-lib --lib an_on_demand_position_request_is_sent_and_leaves_the_timer_alone`
Expected: FAIL to compile with `error[E0599]: no variant or associated item named 'RequestQueuePosition' found for enum 'PeerMessage'`

- [ ] **Step 3: Add the enum variant**

In `pub enum PeerMessage`, after `StopQueuePositionRequests`:

```rust
    /// Ask this peer for our current position in its upload queue, on demand.
    /// The periodic timer is unaffected: this is the caller's cadence, not ours.
    RequestQueuePosition { filename: String },
```

- [ ] **Step 4: Add the dispatch arm**

In `handle_message`, after the `PeerMessage::StopQueuePositionRequests` arm:

```rust
            PeerMessage::RequestQueuePosition { filename } => {
                self.request_queue_position(&filename);
            }
```

- [ ] **Step 5: Add the actor method**

Directly after `request_due_queue_positions`:

```rust
    /// Send one position request without touching the periodic schedule.
    ///
    /// Deliberately independent of `queue_position_requests`: that entry exists to
    /// drive the timer, not to gate a send, so a consumer asking on its own
    /// cadence does not have to register first. A peer with no live stream is
    /// handled inside `send_message`, which logs and returns.
    fn request_queue_position(&mut self, filename: &str) {
        self.send_message(MessageFactory::build_place_in_queue_request(filename));
    }
```

- [ ] **Step 6: Run the test to verify it passes**

Run: `cargo test -p soulseek-rs-lib --lib an_on_demand_position_request_is_sent_and_leaves_the_timer_alone`
Expected: PASS, `1 passed`

- [ ] **Step 7: Run the vendor suite to check nothing regressed**

Run: `cargo test -p soulseek-rs-lib --lib`
Expected: PASS, `184 passed` (183 before this change, plus the new test)

- [ ] **Step 8: Commit**

```bash
git add vendor/soulseek-rs-lib/src/actor/peer_actor.rs
git commit -m "feat: ask a queued peer for its position on demand"
```

---

## Task 2: Vendor — `Client::request_place_in_queue`

**Files:**

- Modify: `vendor/soulseek-rs-lib/src/client/mod.rs` (add beside `cancel_upload` at `:790`)
- Test: `vendor/soulseek-rs-lib/src/client/tests.rs` (module declared at `mod.rs:875`)

- [ ] **Step 1: Write the failing test**

Add to `vendor/soulseek-rs-lib/src/client/tests.rs`:

```rust
#[test]
fn request_place_in_queue_is_false_with_no_peer_actor() {
    // No login, so no peer registry exists: the call must report that the ask
    // went nowhere instead of panicking or erroring. seakarr's queue limits
    // still bound the wait, so this is not a failure condition.
    let client = Client::with_settings(ClientSettings::new("test-user", "test-pass"));

    assert!(!client.request_place_in_queue("bob", "song.mp3"));
}
```

Match the file's existing imports: if it uses `use super::*;`, no import change is needed; if it imports names individually, add `Client` and `ClientSettings`.

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p soulseek-rs-lib --lib request_place_in_queue_is_false_with_no_peer_actor`
Expected: FAIL to compile with `error[E0599]: no method named 'request_place_in_queue' found for struct 'Client'`

- [ ] **Step 3: Add the method**

In `impl Client`, directly after `cancel_upload`:

```rust
    /// Ask `username` where our queued copy of `filename` currently sits.
    ///
    /// Returns whether a live peer actor accepted the request. `false` is not an
    /// error: it means the peer has no control connection right now, and the
    /// download's own queue limits still bound the wait.
    #[must_use = "returns whether the request reached a peer actor"]
    pub fn request_place_in_queue(&self, username: &str, filename: &str) -> bool {
        let registry = self
            .context
            .read_safe()
            .ok()
            .and_then(|ctx| ctx.peer_registry.clone());
        registry.is_some_and(|registry| {
            registry
                .send_to_peer(
                    username,
                    PeerMessage::RequestQueuePosition {
                        filename: filename.to_string(),
                    },
                )
                .is_ok()
        })
    }
```

Check that `PeerMessage` and `RwLockExt` (for `read_safe`) are already in scope in `client/mod.rs` — `browse_user` uses `self.context.write_safe()`, and `cancel_upload` uses `self.context.read_safe()`, so both are available.

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p soulseek-rs-lib --lib request_place_in_queue_is_false_with_no_peer_actor`
Expected: PASS, `1 passed`

- [ ] **Step 5: Run the vendor suite**

Run: `cargo test -p soulseek-rs-lib`
Expected: PASS, `185 passed` in the lib suite plus the existing e2e suite

- [ ] **Step 6: Commit**

```bash
git add vendor/soulseek-rs-lib/src/client/mod.rs vendor/soulseek-rs-lib/src/client/tests.rs
git commit -m "feat: expose an on-demand queue position request to consumers"
```

---

## Task 3: seakarr — the client trait can ask a peer for a position

**Files:**

- Modify: `src/client.rs` (trait at `:63-75`, `MockClient` fields at `:86-104`, `MockClient::new` at `:106-121`, `impl SoulseekClient for MockClient` at `:156`, `impl SoulseekClient for RealClient` at `:729`)
- Modify: `src/download.rs` (`ControllableClient` at `:1754`, `RetryClient` at `:1810`, `SelectiveFailClient` at `:1880`, `ScriptedClient` at `:3963` — plus the `ScriptedClient` struct at `:3941-3946` and its `new` at `:3948`)
- Modify: `src/runner.rs` (`CancelAfterFirstSearchClient` at `:4518`)

- [ ] **Step 1: Write the failing tests**

In `src/client.rs`, inside the existing `mod real_client_tests` (after `maps_download_statuses`), add:

```rust
    #[tokio::test]
    async fn mock_client_records_position_asks() {
        let client = MockClient::new();

        assert!(client.request_queue_position("bob", "Music\\a\\b.flac").await);
        assert_eq!(
            client.position_asks.lock().unwrap().clone(),
            vec![("bob".to_string(), "Music\\a\\b.flac".to_string())]
        );
    }

    #[tokio::test]
    async fn request_queue_position_is_false_without_a_connection() {
        // No login, so no crate client to ask. The call must report "no peer
        // actor" rather than erroring, because the queue limits still bound the
        // wait.
        let client = RealClient::new();

        assert!(!client.request_queue_position("bob", "song.mp3").await);
    }
```

In `src/download.rs`, add this accessor to the test-only `impl ScriptedClient` block (after `fn new`). It reads the field added in Step 6, so it will not compile until then — that is part of the RED phase:

```rust
        /// Every position ask this client received, in call order.
        fn asks(&self) -> Vec<(String, String)> {
            self.position_asks.lock().unwrap().clone()
        }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p seakarr --lib mock_client_records_position_asks request_queue_position_is_false_without_a_connection`
Expected: FAIL to compile with `error[E0599]: no method named 'request_queue_position' found for struct 'MockClient'` and the same for `RealClient`

- [ ] **Step 3: Add the trait method**

In `pub trait SoulseekClient`, after `download`:

> **Amended during implementation:** `MockClient` answers `false` instead of recording asks, and
> the shipped doc wording reports "whether a peer actor was found to receive the ask". See the
> amendments section above.

```rust
    /// Ask a peer where our queued copy of `filename` sits. Best-effort: a peer
    /// with no live control connection returns false and the wait is unaffected.
    ///
    /// `filename` is the peer's own share-relative path, the same value passed to
    /// `download()`: the position response is matched against the download record
    /// by that name, so a display basename would never match.
    async fn request_queue_position(&self, username: &str, filename: &str) -> bool;
```

- [ ] **Step 4: Record asks in `MockClient`**

Add the field to `pub struct MockClient` after `write_files`:

```rust
    /// Every `(username, filename)` passed to `request_queue_position()`, in
    /// call order, so tests can assert the refresh cadence.
    pub position_asks: Mutex<Vec<(String, String)>>,
```

Initialise it in `MockClient::new`, after `write_files`:

```rust
            position_asks: Mutex::new(vec![]),
```

Add the implementation in `impl SoulseekClient for MockClient`, after `download`:

```rust
    async fn request_queue_position(&self, username: &str, filename: &str) -> bool {
        self.position_asks
            .lock()
            .unwrap()
            .push((username.to_string(), filename.to_string()));
        true
    }
```

- [ ] **Step 5: Delegate in `RealClient`**

Add to `impl SoulseekClient for RealClient`, after `download`:

```rust
    async fn request_queue_position(&self, username: &str, filename: &str) -> bool {
        // Deliberately no `reconnect_if_needed`: a session that is down has no
        // peer actor to ask, and the queue limits still bound the wait, so a
        // refresh must never trigger a login.
        let Ok(client) = self.connected_client().await else {
            return false;
        };
        client.request_place_in_queue(username, filename)
    }
```

- [ ] **Step 6: Add the recording field to `ScriptedClient`**

In the test-only `struct ScriptedClient` in `src/download.rs`, add a field after `usernames`:

```rust
        position_asks: Mutex<Vec<(String, String)>>,
```

Initialise it in `ScriptedClient::new`, after `usernames`:

```rust
                position_asks: Mutex::new(Vec::new()),
```

Add the implementation inside `#[async_trait] impl SoulseekClient for ScriptedClient`, after `download`:

```rust
        async fn request_queue_position(&self, username: &str, filename: &str) -> bool {
            self.position_asks
                .lock()
                .unwrap()
                .push((username.to_string(), filename.to_string()));
            true
        }
```

- [ ] **Step 7: Add the one-line implementations to the remaining doubles**

For each of these, add the method at the end of the `impl SoulseekClient for ...` block. The return value is informational — production ignores it — so a double that models no peer actor answers `false`:

`ControllableClient`, `RetryClient`, `SelectiveFailClient` (all in `src/download.rs`) and `CancelAfterFirstSearchClient` (`src/runner.rs`):

```rust
        async fn request_queue_position(&self, _username: &str, _filename: &str) -> bool {
            false
        }
```

- [ ] **Step 8: Run the tests to verify they pass**

Run: `cargo test -p seakarr --lib mock_client_records_position_asks request_queue_position_is_false_without_a_connection`
Expected: PASS, `2 passed`

- [ ] **Step 9: Run the whole suite to catch a missed implementation**

Run: `cargo test -p seakarr`
Expected: PASS. A missed trait implementation is a compile error naming the type, so this step is the completeness check.

- [ ] **Step 10: Commit**

```bash
git add src/client.rs src/download.rs src/runner.rs
git commit -m "feat: add a best-effort queue position ask to the client trait"
```

---

## Task 4: seakarr — a 30-second ask deadline on `QueueWait`

**Files:**

- Modify: `src/download.rs` (constant beside `QUEUE_NOTICE_GRACE` at `:291-299`, `QueueWait` struct at `:550-569`, `QueueWait::new` at `:572-584`, new method after `notice_is_due` at `:585-587`)

- [ ] **Step 1: Write the failing test**

Add to the tests module in `src/download.rs`, near the other `QueueWait` tests:

```rust
    #[test]
    fn the_position_ask_is_due_every_thirty_seconds() {
        // The vendored crate asks once at enqueue, so the first ask from seakarr
        // is a refresh, never a duplicate.
        let config = default_dl_config();
        let start = tokio::time::Instant::now();
        let mut queue = QueueWait::new(start, &config);

        assert!(
            !queue.position_request_is_due(start),
            "the crate already asked at enqueue"
        );
        assert!(!queue.position_request_is_due(start + Duration::from_secs(29)));
        assert!(queue.position_request_is_due(start + Duration::from_secs(30)));
        assert!(
            !queue.position_request_is_due(start + Duration::from_secs(59)),
            "the deadline advances by one interval per ask"
        );
        assert!(queue.position_request_is_due(start + Duration::from_secs(60)));
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p seakarr --lib the_position_ask_is_due_every_thirty_seconds`
Expected: FAIL to compile with `error[E0599]: no method named 'position_request_is_due' found for struct 'QueueWait'`

- [ ] **Step 3: Add the constant**

In `src/download.rs`, directly after `QUEUE_NOTICE_GRACE`:

```rust
/// How often a queued attempt re-asks its peer for a position.
///
/// The vendored crate's own interval is five minutes, which outlives the queue
/// cap this deployment runs (`max_queue_time_secs=300`), so a wait used to see
/// one position report and a bar frozen at it. Fresh positions also arm the
/// `max_start_time_secs` head deadline, which needs a position-1 report.
const QUEUE_POSITION_REFRESH: Duration = Duration::from_secs(30);
```

- [ ] **Step 4: Add the field and its initialisation**

In `struct QueueWait`, after `observed_position`:

```rust
    /// When the next on-demand position ask is due.
    next_position_request: tokio::time::Instant,
```

In `QueueWait::new`, after `observed_position: None,`:

```rust
            next_position_request: enqueued_at + QUEUE_POSITION_REFRESH,
```

- [ ] **Step 5: Add the due check**

Directly after `notice_is_due`:

```rust
    /// Whether a position ask is due, advancing the deadline when it is.
    ///
    /// The same shape as `notice_is_due`: both are time-driven, idempotent, and
    /// evaluated at the top of the poll loop so a busy status channel cannot
    /// starve them.
    fn position_request_is_due(&mut self, now: tokio::time::Instant) -> bool {
        if now < self.next_position_request {
            return false;
        }
        self.next_position_request = now + QUEUE_POSITION_REFRESH;
        true
    }
```

- [ ] **Step 6: Run the test to verify it passes**

Run: `cargo test -p seakarr --lib the_position_ask_is_due_every_thirty_seconds`
Expected: PASS, `1 passed`

- [ ] **Step 7: Commit**

```bash
git add src/download.rs
git commit -m "feat: add a 30 second queue position refresh deadline"
```

---

## Task 5: seakarr — the poll loop asks its peer

**Files:**

- Modify: `src/download.rs` (loop top at `:935-939`, after the notice check)

- [ ] **Step 1: Write the failing test**

Add to the tests module in `src/download.rs`:

```rust
    #[tokio::test(start_paused = true)]
    async fn a_queued_wait_re_asks_its_peer_every_thirty_seconds() {
        // A 90 s cap with a 30 s cadence gives three asks: at 30 s, 60 s and 90 s.
        // The ask is evaluated before the deadline check, so the ask that lands on
        // the expiry instant still goes out.
        let client = ScriptedClient::new(vec![vec![status_step(
            Duration::from_secs(1),
            queue_position(20),
        )]]);
        let mut config = default_dl_config();
        config.max_retries = 0;
        config.max_queue_length = 50;
        config.max_queue_time_secs = 90;

        let (_dir, result) =
            download_with_named_script(&client, 0, "Music\\A\\B\\ask.flac", "ask-peer", &config)
                .await;

        assert!(result.is_err(), "the 90 s cap must end the wait");
        assert_eq!(
            client.asks(),
            vec![
                ("ask-peer".to_string(), "Music\\A\\B\\ask.flac".to_string());
                3
            ],
            "one ask per 30 s of waiting, carrying the peer's own path"
        );
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p seakarr --lib a_queued_wait_re_asks_its_peer_every_thirty_seconds`
Expected: FAIL with `assertion left == right failed` — `left: []`, `right: [("ask-peer", "Music\\A\\B\\ask.flac"), ...]`

- [ ] **Step 3: Wire the ask into the poll loop**

In `download_once`, after the notice check and the loop's exit checks, before the status poll:

> **Amended during implementation:** the ask sits after the loop's exit checks (not immediately
> after the notice check), is gated on `!transfer.has_started()`, and uses
> `let _ = client.request_queue_position(username, &file.name).await;`. See the amendments
> section above.

```rust
        if queue.notice_is_due(now) {
            queue.notice(basename, username);
        }
        // A time-driven check like the notice above, and here for the same
        // reason: a peer answering position-0 more often than the poll window
        // keeps the loop alive, so a check inside the timeout arm can be starved.
        // The vendored crate asks once at enqueue, so this is a refresh.
        if queue.position_request_is_due(now) {
            tracing::debug!("Queue position request for {basename} from {username}");
            // Best-effort: a peer with no live control connection answers false,
            // and the queue limits still bound the wait.
            let _ = client.request_queue_position(username, &file.name).await;
        }
```

`file.name` is the peer's share-relative path, which is what `download()` was given and what the response is matched against.

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p seakarr --lib a_queued_wait_re_asks_its_peer_every_thirty_seconds`
Expected: PASS, `1 passed`

- [ ] **Step 5: Run the queue-wait tests to check the loop still behaves**

Run: `cargo test -p seakarr --lib download::tests`
Expected: PASS, every download test passes

- [ ] **Step 6: Commit**

```bash
git add src/download.rs
git commit -m "feat: re-ask a queued peer for its position every 30 seconds"
```

---

## Task 6: seakarr — the number actually changes on the bar

**Files:**

- Modify: `src/progress.rs` (tests module at `:246`)
- Modify: `src/download.rs` (tests module)

- [ ] **Step 1: Write the failing test for the render half**

Add to the tests module in `src/progress.rs`:

```rust
    #[test]
    fn update_queue_bar_replaces_the_number_in_place() {
        // The bar exists so a deep queue costs one terminal line: an update must
        // replace the message, not append to it.
        let display = ProgressDisplay::new();
        let bar = display.create_queue_bar(&queue_label("01 - Track.flac", "peer", Some(20)));
        assert_eq!(bar.message(), "01 - Track.flac - peer queue #20");

        display.update_queue_bar(&bar, &queue_label("01 - Track.flac", "peer", Some(12)));

        assert_eq!(
            bar.message(),
            "01 - Track.flac - peer queue #12",
            "the number must be replaced in place"
        );
        assert_eq!(display.queue_bars_updated(), 1);
        display.clear_queue_bar(bar);
    }
```

- [ ] **Step 2: Write the failing test for the state-machine half**

Add to the tests module in `src/download.rs`:

```rust
    #[tokio::test(start_paused = true)]
    async fn refreshed_positions_update_the_queue_bar() {
        // Three fresh positions must reach the bar as three updates. The peer's
        // answers arrive as `Queued` statuses, exactly as the real bridge maps
        // them, so this covers the observe -> update_queue_bar path.
        let client = ScriptedClient::new(vec![vec![
            status_step(Duration::from_secs(1), queue_position(20)),
            status_step(Duration::from_secs(30), queue_position(12)),
            status_step(Duration::from_secs(30), queue_position(3)),
            status_step(Duration::from_secs(1), in_progress(10_000_000)),
            status_step(Duration::from_secs(1), DownloadStatus::Completed),
        ]]);
        let display = ProgressDisplay::new();
        let mut config = default_dl_config();
        config.max_retries = 0;
        config.max_queue_length = 50;

        let (_dir, result) =
            download_with_script_and_progress(&client, 0, &display, &config, None).await;

        assert!(result.is_ok(), "the transfer must complete, got {result:?}");
        assert_eq!(
            display.queue_bars_updated(),
            3,
            "each reported position must update the bar"
        );
        assert_eq!(display.queue_bars_finished(), 1, "the bar is released once");
    }
```

- [ ] **Step 3: Run both tests**

Run: `cargo test -p seakarr --lib update_queue_bar_replaces_the_number_in_place refreshed_positions_update_the_queue_bar`
Expected: both PASS — the render path and the update path already exist; these tests pin them, and the second is the end-to-end evidence that a refreshed number reaches the bar. If either fails, the poll-loop wiring from Task 5 is wrong: check that `observe` is reached for each `Queued` status.

- [ ] **Step 4: Run the download and progress suites**

Run: `cargo test -p seakarr --lib download::tests progress::tests`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add src/progress.rs src/download.rs
git commit -m "test: pin that refreshed positions update the queue bar in place"
```

---

## Task 7: seakarr — the ask stops when the wait ends

**Files:**

- Modify: `src/download.rs` (tests module)

- [ ] **Step 1: Write the failing tests**

Add to the tests module in `src/download.rs`:

```rust
    #[tokio::test(start_paused = true)]
    async fn no_position_ask_is_sent_before_the_refresh_interval() {
        // A 20 s cap ends the wait before the first 30 s ask is due.
        let client = ScriptedClient::new(vec![vec![status_step(
            Duration::from_secs(1),
            queue_position(20),
        )]]);
        let mut config = default_dl_config();
        config.max_retries = 0;
        config.max_queue_length = 50;
        config.max_queue_time_secs = 20;

        let (_dir, result) =
            download_with_script(&client, 0, 10_000_000, &config, None).await;

        assert!(result.is_err(), "the 20 s cap must end the wait");
        assert!(
            client.asks().is_empty(),
            "the crate already asked at enqueue; seakarr must not ask before 30 s, got {:?}",
            client.asks()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn position_asks_stop_once_the_transfer_starts() {
        // The transfer starts at 25 s, before the first ask is due, and the
        // attempt then runs on for a minute: a refresh that outlived the queue
        // would appear as asks at 30 s and 60 s.
        let client = ScriptedClient::new(vec![vec![
            status_step(Duration::from_secs(1), queue_position(20)),
            status_step(Duration::from_secs(24), in_progress(10_000_000)),
            status_step(Duration::from_secs(60), DownloadStatus::Completed),
        ]]);
        let mut config = default_dl_config();
        config.max_retries = 0;
        config.max_queue_length = 50;
        config.max_queue_time_secs = 600;

        let (_dir, result) =
            download_with_script(&client, 0, 10_000_000, &config, None).await;

        assert!(result.is_ok(), "the transfer must complete, got {result:?}");
        assert!(
            client.asks().is_empty(),
            "a running transfer needs no position, got {:?}",
            client.asks()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn position_asks_stop_on_cancellation() {
        let cancel = Arc::new(AtomicBool::new(false));
        let trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(25)).await;
            trigger.store(true, Ordering::SeqCst);
        });
        let client = ScriptedClient::new(vec![vec![status_step(
            Duration::from_secs(1),
            queue_position(20),
        )]]);
        let mut config = default_dl_config();
        config.max_retries = 0;
        config.max_queue_length = 50;
        config.max_queue_time_secs = 600;

        let (_dir, result) =
            download_with_script(&client, 0, 10_000_000, &config, Some(&cancel)).await;

        assert!(
            result.unwrap_err().to_string().contains("download cancelled by user"),
            "cancellation must win"
        );
        assert!(
            client.asks().is_empty(),
            "a cancelled attempt must send nothing further, got {:?}",
            client.asks()
        );
    }
```

- [ ] **Step 2: Run the tests**

Run: `cargo test -p seakarr --lib no_position_ask_is_sent_before_the_refresh_interval position_asks_stop_once_the_transfer_starts position_asks_stop_on_cancellation`
Expected: all PASS. They pass because the ask lives in a loop that is left on transfer start, expiry and cancellation; if any fails, the ask was placed outside the loop or after the exit checks.

- [ ] **Step 3: Run the download suite**

Run: `cargo test -p seakarr --lib download::tests`
Expected: PASS

- [ ] **Step 4: Commit**

```bash
git add src/download.rs
git commit -m "test: pin when the queue position refresh stops"
```

---

## Task 8: Documentation

**Files:**

- Modify: `README.md` (queue-bar paragraph at `:489-492`, `max_start_time_secs` row at `:370`)
- Modify: `docs/agent/specs/2026-09-17-download-log-visibility-design.md` (position paragraph at `:69-71`)

- [ ] **Step 1: Update the queue-bar paragraph**

Current text at `README.md:489-492`:

```markdown
Positions are deliberately **not** logged as a live counter: a queue 100 deep would produce
100 lines. Every position change is instead available at `DEBUG`, and in an interactive
terminal a queue bar shows the current position in place, so a long wait costs one terminal
line:
```

Add one sentence after it:

```markdown
While the file waits, seakarr re-asks the peer for its position every 30 seconds, so that
number keeps up with the queue instead of freezing at the first report. The refresh stops as
soon as the transfer starts.
```

- [ ] **Step 2: Update the `max_start_time_secs` row**

Current row at `README.md:370`:

```markdown
| `max_start_time_secs` | Maximum seconds from first reaching queue position `1` until the first transfer progress. `0` disables this queue-head limit. | `120` |
```

Replace with:

```markdown
| `max_start_time_secs` | Maximum seconds from first reaching queue position `1` until the first transfer progress. `0` disables this queue-head limit. Position reports are refreshed every 30 s while queued, so this limit arms from a fresh position-1 report. | `120` |
```

- [ ] **Step 3: Amend the 2026-09-17 spec**

Current text at `docs/agent/specs/2026-09-17-download-log-visibility-design.md:69-71`:

```markdown
The position is available. The vendored peer actor sends `PlaceInQueueRequest`
immediately on `QueueUpload` and every 300 seconds while that file remains
queued, and the status channel already carries it as
```

Add an amendment note directly before that paragraph:

```markdown
> **Amended 2026-09-22:** the five-minute cadence below is no longer the only source of
> positions. seakarr now asks its peer every 30 seconds while an attempt is queued, so the
> interactive queue bar's number keeps up with the queue. See
> `2026-09-22-queue-position-refresh-design.md`. The logging decisions in this document —
> one notice, one started line, position changes at `DEBUG` — are unchanged.
```

- [ ] **Step 4: Lint the Markdown**

Run: `markdownlint --fix README.md docs/agent/specs/2026-09-17-download-log-visibility-design.md`
Expected: exit 0, no output

- [ ] **Step 5: Commit**

```bash
git add README.md docs/agent/specs/2026-09-17-download-log-visibility-design.md
git commit -m "docs: record the 30 second queue position refresh"
```

---

## Task 9: Full verification

**Files:** none (verification only)

- [ ] **Step 1: Format and lint**

Run: `cargo fmt --check && cargo clippy -- -D warnings`
Expected: both exit 0, no warnings

- [ ] **Step 2: Full test suite**

Run: `cargo test`
Expected: exit 0; the seakarr lib suite grows by the tests added here, and the vendor lib suite reports `185 passed` with no failures

- [ ] **Step 3: Coverage**

Run: `cargo llvm-cov -p seakarr --summary-only`
Expected: TOTAL lines at or above 95 % (96.43 % before this change). The new code paths are exercised by the tests in Tasks 4-7; a drop means one of them is not reaching the ask or the update.

- [ ] **Step 4: Pre-commit gate**

Run: `pre-commit run --all-files`
Expected: all hooks Passed

- [ ] **Step 5: Confirm the acceptance criteria**

Check each box in the spec's acceptance criteria section against the work done:

- the bar's number reflects a position reported after the first one, with no new `INFO`/`WARN` line;
- the label text is unchanged apart from the number;
- a queued attempt with no further statuses issues an ask roughly every 30 s;
- no ask after transfer start, after a queue timeout, or after cancellation;
- the ask carries the peer's share path, not the display basename;
- `max_start_time_secs` can expire on a fresh position-1 report (the `head_at` assignment already exists in `observe`; the refreshed position is what makes it reachable);
- fmt, clippy, tests, coverage and pre-commit all pass.

- [ ] **Step 6: Report**

Summarise for the user: files changed, test counts, coverage, and the two behaviour notes — the queue bar's number now refreshes every 30 s, and `max_start_time_secs` (default 120 s) can now expire on a wait that reaches position 1, which previously almost never happened.

---

## Spec coverage map

| Spec requirement | Task |
| --- | --- |
| `PeerMessage::RequestQueuePosition` + actor send, timer untouched | 1 |
| `Client::request_place_in_queue` public entry point | 2 |
| Required `SoulseekClient::request_queue_position` (async) + 7 impls | 3 |
| `QUEUE_POSITION_REFRESH` constant and `QueueWait::position_request_is_due` | 4 |
| Poll-loop placement after the notice check, `DEBUG` record per ask | 5 |
| Peer's share path on the wire, not the basename | 5, 7 |
| Bar label unchanged, number replaced in place | 6 |
| Refresh stops at transfer start, on timeout, on cancellation | 7 |
| Queue-head deadline arms from a fresh position-1 report | 7 (verification), spec acceptance criteria |
| README cadence + `max_start_time_secs` note | 8 |
| Amendment note on the 2026-09-17 spec | 8 |
| fmt, clippy, tests, coverage, pre-commit | 9 |

## Self-review

- **Placeholder scan:** none found — every code step shows the code, every command shows its expected result, and no step defers work to a later task.
- **Type consistency:** `request_queue_position(&self, username, filename) -> bool` is identical in the trait (Task 3), the `MockClient` and `ScriptedClient` recordings (`position_asks: Mutex<Vec<(String, String)>>`, read in tests as `client.asks()` for `ScriptedClient` and `client.position_asks.lock()` for `MockClient`), the `RealClient` delegation and the Task 5 call site; the vendor method is `request_place_in_queue` (Task 2) and is called only from `RealClient` (Task 3); `position_request_is_due(&mut self, now)` matches its definition (Task 4) and use (Task 5); the vendor's `queue_position_requests` (unchanged) is never confused with seakarr's `position_asks` test recording.
- **Ordering:** the vendor tasks come first because `RealClient` cannot delegate before `Client::request_place_in_queue` exists; the trait task precedes the loop wiring; tests precede each implementation.
