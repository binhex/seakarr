# Queue-aware Download Timeouts Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use sub-agents
> (recommended) to implement this plan task-by-task. Steps use checkbox
> (`- [ ]`) syntax for tracking.

**Goal:** Let permitted Soulseek queue waits use their own queue limits without
consuming the post-start transfer inactivity timeout.

**Architecture:** Carry optional queue positions from the vendored client into
seakarr, then enforce one queue policy in `download_once`. Preserve the public
filter/search helpers as free-slot-only wrappers, while runner-only queue-aware
helpers admit zero-slot candidates for download-time position validation.
Queue-policy failures are typed, cancel and drain the active vendor transfer,
skip same-peer retries, and continue with the next ranked candidate.

**Tech Stack:** Rust 2021 application, vendored Rust 2024
`soulseek-rs-lib`, Tokio channels/timers, `thiserror`, Cargo tests, Clippy,
rustfmt, markdownlint, and pre-commit.

---

<!-- markdownlint-disable MD013 -->

## Planning decisions

### Scope check

Do **not** split this into separate implementation plans. The vendor status propagation, application bridge, candidate
eligibility, and download timer state machine are layers of one vertical behavior: none independently gives users a working
queue policy. Documentation and tests complete that same behavior. The following parsed-but-unwired settings remain out of
scope: `max_download_time_mins`, `min_filtered_users`, and `skip_retry_hours`.

### Repository-state guard

The working tree already contains an uncommitted edit to
`docs/agent/specs/2026-09-09-queue-aware-download-timeouts-design.md`. Preserve it exactly unless a later chain step explicitly
asks to revise the spec. Stage files explicitly in every commit; never use `git add .`.

The project-specific `AGENTS.md` mandates `docs/agent/plans/` for generated plans, which overrides the generic `docs/plans/`
location. This plan therefore remains at
`docs/agent/plans/2026-09-09-queue-aware-download-timeouts.md`.

### File map

| File | Planned responsibility |
| --- | --- |
| `Cargo.toml` | Enable Tokio's existing `test-util` feature for deterministic paused-time tests. |
| `src/error.rs` | Add the typed, non-retryable `QueueTimeout` error. |
| `src/client.rs` | Represent optional queue positions and paused state; map and bridge vendor statuses without inventing progress. |
| `src/filter.rs` | Add queue-aware filter/rejection-summary helpers while preserving public free-slot-only wrappers. |
| `src/search.rs` | Probe fallback tiers with the queue-aware filter when called by the runner. |
| `src/runner.rs` | Pass `download.max_queue_length` through every primary, title, artist-only, and rejection-summary path. |
| `src/download.rs` | Enforce queue position, queue deadlines, transfer inactivity, cancellation, retry classification, and fallback. |
| `src/config.rs` | Remove the three activated queue settings from the parsed-but-unwired list. |
| `vendor/soulseek-rs-lib/src/types.rs` | Change vendor `Queued` status to carry `Option<u32>`. |
| `vendor/soulseek-rs-lib/src/download_store.rs` | Persist and publish queue-position updates; add focused store tests. |
| `vendor/soulseek-rs-lib/src/client/downloads.rs` | Construct and match the new queued-status shape. |
| `vendor/soulseek-rs-lib/src/client/operations.rs` | Keep `PlaceInQueueUpdate` connected and update queued matches. |
| `vendor/soulseek-rs-lib/src/client/mod.rs` | Update protected-peer queued matches. |
| `vendor/soulseek-rs-lib/src/peer/download_peer.rs` | Update download fixtures/construction for the queued-status shape. |
| `vendor/soulseek-rs-lib/src/client/tests.rs` | Prove an operation-level queue update reaches the download receiver. |
| `vendor/soulseek-rs-lib/examples/stress.rs` | Update exhaustive queued-status matches in the diagnostic example. |
| `README.md` | Document active queue limits, timer boundaries, filtering, fallback, and FAQ behavior. |

No new production module, YAML key, or database migration is needed. `Cargo.lock` should not change because `tokio` is already
locked; only one additional feature is enabled for tests.

### Behavioral invariants

1. Positive queue positions are one-based. Wire position `0` means no current queue entry and is normalized to `None`; it is
   non-actionable and never proves a zero-slot candidate in-bounds.
2. `max_queue_length == 0` keeps free-slot filtering. A candidate admitted with an advertised free slot is not retroactively
   rejected by later positive telemetry.
3. A positive queue cap lets a zero-slot search result reach download, but that candidate must report an in-bound real position
   before `InProgress` or position-less `Completed`; otherwise it fails closed.
4. With a positive cap, any real position a free-slot candidate reports must still satisfy that cap.
5. `max_queue_time_secs` starts at enqueue. `max_start_time_secs` starts on the first observation of position `1`. Zero disables
   the corresponding deadline, and the earlier enabled queue deadline wins.
6. The first real `InProgress` ends queue timing and starts `timeout_secs`; later `InProgress` updates reset only that transfer
   deadline. Pre-start `Paused` remains queue time; post-start `Paused` remains transfer inactivity.
7. Queue-policy failure cancels and drains the vendor transfer, returns `SeakarrError::QueueTimeout`, skips same-peer retry, and
   lets `download_album` fall back to the next candidate.
8. Cancellation is checked before every timeout/policy return and remains higher priority. Queue wait is excluded from recorded
   effective throughput.

## Task 1: Propagate queue positions through the vendored client

**Files:**

- Modify: `vendor/soulseek-rs-lib/src/types.rs:160-225`
- Modify: `vendor/soulseek-rs-lib/src/download_store.rs:45-75,160-215`
- Modify: `vendor/soulseek-rs-lib/src/client/downloads.rs:80-180`
- Modify: `vendor/soulseek-rs-lib/src/client/operations.rs:365-390,450-465`
- Modify: `vendor/soulseek-rs-lib/src/client/mod.rs:585-600`
- Modify: `vendor/soulseek-rs-lib/src/peer/download_peer.rs:430-510`
- Modify: `vendor/soulseek-rs-lib/src/client/tests.rs`
- Modify: `vendor/soulseek-rs-lib/examples/stress.rs:1145-1175`

- [ ] **Step 1: Write the failing store propagation test**

Replace the existing `update_queue_position_sets_field_when_match` test with a receiver-aware test. This initially fails to
compile because `Queued` is still a unit variant, which is the RED contract checkpoint.

```rust
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
}
```

- [ ] **Step 2: Run the vendor RED test**

```bash
cargo test -p soulseek-rs-lib update_queue_position_updates_store_and_notifies_receiver -- --nocapture
```

Expected: compilation fails because `DownloadStatus::Queued` has no `queue_position` field.

- [ ] **Step 3: Change the vendor queued contract and publish updates**

In `vendor/soulseek-rs-lib/src/types.rs`, change only the queued variant:

```rust
pub enum DownloadStatus {
    Queued {
        queue_position: Option<u32>,
    },
    InProgress {
        bytes_downloaded: u64,
        total_bytes: u64,
        speed_bytes_per_sec: f64,
    },
    Paused {
        bytes_downloaded: u64,
        total_bytes: u64,
    },
    Completed,
    Failed(Option<String>),
    TimedOut,
}
```

Initialize new `Download` records in `client/downloads.rs` and test fixtures with:

```rust
status: DownloadStatus::Queued {
    queue_position: None,
},
```

Implement store propagation without adding a second position store. Select the newest matching record that is still queued
because code 44 has no attempt token, and normalize wire position `0` to `None`:

```rust
pub fn update_queue_position(
    &mut self,
    username: &str,
    filename: &str,
    position: u32,
) -> bool {
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
```

Do not synthesize an initial receiver event. The stored status begins as `Queued { queue_position: None }`; seakarr already
knows enqueue occurred, and later `PlaceInQueueUpdate` messages provide real telemetry.

- [ ] **Step 4: Update every vendor queued match mechanically**

Use this audit, then change every status-only pattern to `DownloadStatus::Queued { .. }` and every constructor to
`DownloadStatus::Queued { queue_position: None }`:

```bash
rg -n "DownloadStatus::Queued" \
  vendor/soulseek-rs-lib/src vendor/soulseek-rs-lib/examples --glob '*.rs'
```

Keep the existing `ClientOperation::PlaceInQueueUpdate` branch in `client/operations.rs`; it must continue calling
`downloads.update_queue_position(...)` exactly once.

- [ ] **Step 5: Add an operation-level propagation test**

Add this to `vendor/soulseek-rs-lib/src/client/tests.rs` so the protocol operation, store, and receiver path are covered together:

```rust
#[test]
fn place_in_queue_update_reaches_download_receiver() {
    let client = Client::new("me", "password");
    let (download_sender, status_receiver) = mpsc::channel();
    client.context.write().unwrap().add_download(download(
        "peer",
        "song.mp3",
        7,
        DownloadStatus::Queued {
            queue_position: None,
        },
        download_sender,
    ));

    let (ops_sender, ops_receiver) = mpsc::channel();
    let shutdown = Arc::new(AtomicBool::new(false));
    Client::listen_to_client_operations(
        ops_receiver,
        client.context.clone(),
        "me".to_string(),
        shutdown.clone(),
    );
    ops_sender
        .send(ClientOperation::PlaceInQueueUpdate {
            username: "peer".to_string(),
            filename: "song.mp3".to_string(),
            place: 3,
        })
        .unwrap();

    assert!(matches!(
        status_receiver.recv_timeout(Duration::from_secs(1)),
        Ok(DownloadStatus::Queued {
            queue_position: Some(3)
        })
    ));
    shutdown.store(true, Ordering::Relaxed);
    drop(ops_sender);
}
```

- [ ] **Step 6: Run vendor tests and commit**

```bash
cargo test -p soulseek-rs-lib --lib
cargo test -p soulseek-rs-lib --test e2e
cargo check -p soulseek-rs-lib --example stress
git diff --check
git add vendor/soulseek-rs-lib/src vendor/soulseek-rs-lib/examples/stress.rs
git commit -m "feat: forward Soulseek queue positions"
```

Expected: vendor library tests, e2e tests, and stress-example compilation pass.

## Task 2: Preserve queue and paused states in the application bridge

**Files:**

- Modify: `src/client.rs:25-45,530-655,1290-1335`

- [ ] **Step 1: Write RED mapping and bridge tests**

Update `maps_download_statuses` to require optional positions and a real paused domain state:

```rust
assert!(matches!(
    ss_download_status_to_domain(SsDownloadStatus::Queued {
        queue_position: None,
    }),
    DownloadStatus::Queued {
        queue_position: None
    }
));
assert!(matches!(
    ss_download_status_to_domain(SsDownloadStatus::Queued {
        queue_position: Some(1),
    }),
    DownloadStatus::Queued {
        queue_position: Some(1)
    }
));
assert!(matches!(
    ss_download_status_to_domain(SsDownloadStatus::Paused {
        bytes_downloaded: 100,
        total_bytes: 1000,
    }),
    DownloadStatus::Paused {
        bytes_downloaded: 100,
        total_bytes: 1000,
    }
));
```

Add a bridge test proving queue updates are forwarded and do not terminate the bridge:

```rust
#[tokio::test]
async fn bridge_forwards_queue_positions_as_non_terminal_statuses() {
    let (crate_sender, crate_receiver) = std::sync::mpsc::channel();
    let (forward_sender, mut forward_receiver) = tokio::sync::mpsc::channel(4);
    let cancelled = Arc::new(AtomicBool::new(false));
    let worker_cancelled = cancelled.clone();
    let worker = tokio::task::spawn_blocking(move || {
        forward_transfer_status(
            &crate_receiver,
            &forward_sender,
            &worker_cancelled,
            false,
            "song.flac",
        );
    });

    crate_sender
        .send(SsDownloadStatus::Queued {
            queue_position: Some(2),
        })
        .unwrap();
    assert!(matches!(
        forward_receiver.recv().await,
        Some(DownloadStatus::Queued {
            queue_position: Some(2)
        })
    ));

    crate_sender.send(SsDownloadStatus::Completed).unwrap();
    assert!(matches!(
        forward_receiver.recv().await,
        Some(DownloadStatus::Completed)
    ));
    worker.await.unwrap();
}
```

- [ ] **Step 2: Run the application RED tests**

```bash
cargo test -p seakarr client::real_client_tests::maps_download_statuses -- --nocapture
cargo test -p seakarr client::real_client_tests::bridge_forwards_queue_positions_as_non_terminal_statuses -- --nocapture
```

Expected: compilation fails until the domain enum and mapping preserve the new fields/states.

- [ ] **Step 3: Add the domain status shapes and exact mapping**

Replace the domain enum variants with:

```rust
pub enum DownloadStatus {
    Queued {
        queue_position: Option<u32>,
    },
    Paused {
        bytes_downloaded: u64,
        total_bytes: u64,
    },
    InProgress {
        speed_bytes_per_sec: u64,
        bytes_downloaded: u64,
        total_bytes: u64,
    },
    Completed,
    Failed {
        reason: String,
    },
}
```

Map vendor states one-for-one:

```rust
SsDownloadStatus::Queued { queue_position } => DownloadStatus::Queued { queue_position },
SsDownloadStatus::Paused {
    bytes_downloaded,
    total_bytes,
} => DownloadStatus::Paused {
    bytes_downloaded,
    total_bytes,
},
```

Keep `Queued` and `Paused` non-terminal in `forward_transfer_status` and `drain_transfer`. Update the progress-log comment and
local match so paused logs may still show zero display speed, but the forwarded value remains `DownloadStatus::Paused`.

- [ ] **Step 4: Audit all application status matches, test, and commit**

```bash
rg -n "DownloadStatus::(Queued|Paused)" src tests --glob '*.rs'
cargo test -p seakarr client::real_client_tests -- --nocapture
cargo test -p seakarr download::tests::progress_bar_not_created_until_transfer_starts -- --nocapture
git diff --check
git add src/client.rs
git commit -m "fix: preserve queued and paused download states"
```

Expected: bridge tests pass, and neither queued nor paused status creates a progress bar or ends the bridge.

## Task 3: Make search filtering queue-aware without breaking public helpers

**Files:**

- Modify: `src/filter.rs:1-155,1365-1540`
- Modify: `src/search.rs:235-420`
- Modify: `src/runner.rs:185-390,825-850`

- [ ] **Step 1: Add RED filter and search-tier tests**

In `src/filter.rs`, retain the current default-cap assertion and add positive-cap coverage:

```rust
#[test]
fn positive_queue_cap_keeps_zero_slot_candidate_for_download_validation() {
    let cfg = default_filter_config();
    let results = vec![make_result(
        "queued-peer",
        500,
        0,
        vec![make_file("Album/01 - track.flac", 900, 30_000_000)],
    )];

    let filtered = filter_results_with_queue_limit(&results, &cfg, None, None, 3);

    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].username, "queued-peer");
}
```

Add the paired rejection-summary assertion:

```rust
let summary = summarize_rejections_with_queue_limit(&results, &cfg, None, None, 3);
assert_eq!(summary.no_free_slots, 0);
```

In `src/search.rs`, add
`positive_queue_cap_stops_search_fallback`. Its primary result has zero slots
and a valid album path. With cap `3`, the primary tier should be usable and no
lowercase or album-only query should run:

```rust
let outcome = search_album_with_fallback_with_queue_limit(
    &client,
    "Artist",
    Some("Album"),
    15,
    &test_filters(),
    None,
    3,
)
.await
.unwrap();
assert_eq!(outcome.results.len(), 1);
assert_eq!(client.search_queries.lock().unwrap().as_slice(), ["Artist Album"]);
```

- [ ] **Step 2: Run the RED filter/search tests**

```bash
cargo test -p seakarr positive_queue_cap_keeps_zero_slot_candidate_for_download_validation -- --nocapture
cargo test -p seakarr positive_queue_cap_stops_search_fallback -- --nocapture
```

Expected: compilation fails because the queue-aware helper functions do not exist.

- [ ] **Step 3: Add backward-compatible filter and summary wrappers**

Rename the current `filter_results` implementation to
`filter_results_with_queue_limit`, make it `pub(crate)`, and add a final
`max_queue_length: u32` parameter. Keep the rest of that function body in
place. Insert this complete compatibility wrapper immediately above it:

```rust
pub fn filter_results(
    results: &[SearchResult],
    config: &FilterConfig,
    library_track_count: Option<usize>,
    album: Option<&str>,
) -> Vec<SearchResult> {
    filter_results_with_queue_limit(results, config, library_track_count, album, 0)
}
```

In the renamed implementation, replace the current slot gate with:

```rust
if r.slots == 0 && max_queue_length == 0 {
    return false;
}
```

Rename the current summary implementation to
`summarize_rejections_with_queue_limit`, make it `pub(crate)`, and add a final
`max_queue_length: u32` parameter. Insert this compatibility wrapper:

```rust
pub fn summarize_rejections(
    results: &[SearchResult],
    config: &FilterConfig,
    library_track_count: Option<usize>,
    album: Option<&str>,
) -> FilterRejectionSummary {
    summarize_rejections_with_queue_limit(
        results,
        config,
        library_track_count,
        album,
        0,
    )
}
```

In the renamed summary body, use:

```rust
if r.slots == 0 && max_queue_length == 0 {
    summary.no_free_slots += 1;
    continue;
}
```

Do not change extension, quality, album, minimum-track, contiguity, library-count, or ranking behavior. Existing
`rank_candidates` already gives free-slot candidates a higher slot bonus and safely scores zero-slot candidates.

- [ ] **Step 4: Add a backward-compatible queue-aware search entry point**

Preserve the current public function as a cap-zero wrapper:

```rust
pub async fn search_album_with_fallback(
    client: &dyn SoulseekClient,
    artist: &str,
    album: Option<&str>,
    timeout_secs: u64,
    filters: &FilterConfig,
    library_track_count: Option<usize>,
) -> Result<SearchOutcome> {
    search_album_with_fallback_with_queue_limit(
        client,
        artist,
        album,
        timeout_secs,
        filters,
        library_track_count,
        0,
    )
    .await
}
```

Move the current search body to `pub(crate) async fn search_album_with_fallback_with_queue_limit(...)` with a final
`max_queue_length: u32` parameter. Add that parameter to private `tier_has_usable_results`, and have it call
`filter_results_with_queue_limit` in every fallback tier.

- [ ] **Step 5: Thread the existing cap through all runner paths**

Use `search_album_with_fallback_with_queue_limit(..., config.download.max_queue_length)` in both
`process_album_internal` and `run_artist_only_mode`. Use `filter_results_with_queue_limit` for primary/presearched and title
results. Use `summarize_rejections_with_queue_limit` for both summary sites.

Make the zero-result requirement log match policy:

```rust
let availability_requirement = if config.download.max_queue_length == 0 {
    "free slot".to_string()
} else {
    format!(
        "free slot or queue position <= {}",
        config.download.max_queue_length
    )
};
```

Insert `availability_requirement` in place of the hard-coded `free slot` text.

- [ ] **Step 6: Run focused regressions and commit**

```bash
cargo test -p seakarr filter::tests -- --nocapture
cargo test -p seakarr search::tests -- --nocapture
cargo test -p seakarr runner::tests::test_results_rejected_by_filters_marks_failed -- --nocapture
cargo test -p seakarr runner::tests::test_run_manual_mode -- --nocapture
git diff --check
git add src/filter.rs src/search.rs src/runner.rs
git commit -m "feat: admit bounded queued download candidates"
```

Expected: cap zero remains free-slot-only; a positive cap keeps zero-slot candidates and prevents needless fallback-tier
searches.

## Task 4: Add deterministic queue-state RED tests

**Files:**

- Modify: `Cargo.toml`
- Modify: `src/error.rs`
- Modify: `src/download.rs:720-end` test module

- [ ] **Step 1: Enable paused Tokio time for tests**

Add the already-used Tokio package to `[dev-dependencies]` with only the extra test feature; Cargo feature unification supplies
`full` from the normal dependency and `test-util` during tests:

```toml
[dev-dependencies]
tokio = { version = "1", features = ["test-util"] }
```

Do not add `tokio-test`; `#[tokio::test(start_paused = true)]`, `sleep`, and `advance` are sufficient.

- [ ] **Step 2: Drive the typed error contract with a test, then add it**

First add this complete test module to `src/error.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::SeakarrError;

    #[test]
    fn queue_timeout_error_displays_reason() {
        let error = SeakarrError::QueueTimeout(
            "queue position 4 exceeds limit 3".into(),
        );
        assert_eq!(
            error.to_string(),
            "download queue timeout: queue position 4 exceeds limit 3"
        );
    }
}
```

Run:

```bash
cargo test -p seakarr queue_timeout_error_displays_reason -- --nocapture
```

Expected RED: `QueueTimeout` does not exist. Then add beside `QualityRejected` in `src/error.rs`:

```rust
#[error("download queue timeout: {0}")]
QueueTimeout(String),
```

Rerun the test; expected GREEN.

- [ ] **Step 3: Add one reusable scripted download client**

Add this test fixture to `src/download.rs`. It keeps a status sender alive after a script ends so a queued attempt waits until
seakarr cancels it, and records calls/cancellations for retry/fallback assertions.

```rust
#[derive(Clone)]
struct StatusStep {
    after: Duration,
    status: DownloadStatus,
}

fn status_step(after: Duration, status: DownloadStatus) -> StatusStep {
    StatusStep { after, status }
}

struct ScriptedClient {
    scripts: Mutex<std::collections::VecDeque<Vec<StatusStep>>>,
    calls: std::sync::atomic::AtomicUsize,
    cancellations: Arc<std::sync::atomic::AtomicUsize>,
    usernames: Mutex<Vec<String>>,
}

impl ScriptedClient {
    fn new(scripts: Vec<Vec<StatusStep>>) -> Self {
        Self {
            scripts: Mutex::new(scripts.into()),
            calls: std::sync::atomic::AtomicUsize::new(0),
            cancellations: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            usernames: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl SoulseekClient for ScriptedClient {
    async fn login(&self, _u: &str, _p: &str, _s: &str, _port: u16) -> Result<()> {
        Ok(())
    }

    async fn search(&self, _query: &str, _timeout_secs: u64) -> Result<Vec<SearchResult>> {
        Ok(Vec::new())
    }

    async fn download(
        &self,
        _file: &FileInfo,
        username: &str,
        _dir: &Path,
    ) -> Result<DownloadHandle> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.usernames.lock().unwrap().push(username.to_string());
        let script = self
            .scripts
            .lock()
            .unwrap()
            .pop_front()
            .expect("one status script per download call");
        let (status_tx, status_rx) = mpsc::channel(16);
        let (cancel_tx, mut cancel_rx) = mpsc::channel(1);
        let cancellations = self.cancellations.clone();
        tokio::spawn(async move {
            for step in script {
                tokio::select! {
                    _ = tokio::time::sleep(step.after) => {
                        if status_tx.send(step.status).await.is_err() {
                            return;
                        }
                    }
                    cancelled = cancel_rx.recv() => {
                        if cancelled.is_some() {
                            cancellations.fetch_add(1, Ordering::SeqCst);
                            let _ = status_tx.send(DownloadStatus::Failed {
                                reason: "cancelled".into(),
                            }).await;
                        }
                        return;
                    }
                }
            }
            if cancel_rx.recv().await.is_some() {
                cancellations.fetch_add(1, Ordering::SeqCst);
                let _ = status_tx.send(DownloadStatus::Failed {
                    reason: "cancelled".into(),
                }).await;
            }
        });
        Ok(DownloadHandle {
            status_rx,
            cancel_tx,
        })
    }
}
```

- [ ] **Step 4: Add the queue/timer RED matrix**

Each test uses `#[tokio::test(start_paused = true)]`, `max_retries = 0`, and the private
`download_file_for_candidate(..., peer_slots, ...)` entry point introduced in Task 5. Add these exact cases:

| Test | Script/config | Required assertion |
| --- | --- | --- |
| `queued_wait_does_not_consume_transfer_timeout` | slots `0`, cap `3`; `Queued(Some(2))`, wait 2s, `InProgress`, `Completed`; transfer timeout 1s, queue time 10s | success |
| `total_queue_timeout_expires_queued_attempt` | slots `0`, cap `3`; `Queued(Some(2))`, then wait; queue time 2s | `QueueTimeout` mentioning `max_queue_time_secs=2` |
| `queue_head_timeout_starts_at_position_one` | slots `0`, cap `3`; position 2 for 2s, then position 1 for 2s; start limit 1s, total 10s | `QueueTimeout` mentioning `max_start_time_secs=1` |
| `time_at_position_two_does_not_consume_start_limit` | slots `0`, cap `3`; position 2 for 2s, position 1 for 500ms, then progress/completion | success |
| `pre_start_pause_does_not_start_transfer_timeout` | slots `1`; `Paused`, wait 2s, progress/completion; transfer timeout 1s, queue time 10s | success |
| `post_start_pause_does_not_reset_transfer_timeout` | progress, wait 800ms, `Paused`, wait 400ms, progress/completion; transfer timeout 1s | ordinary download timeout before second progress |
| `zero_queue_timers_allow_delayed_start` | slots `1`; wait 2s, progress/completion; both queue timers zero, transfer timeout 1s | success |
| `zero_queue_cap_preserves_admitted_free_slot_candidate` | slots `1`, cap `0`; positive queue telemetry, then progress/completion | success |
| `positive_cap_rejects_out_of_bound_position` | slots `0`, cap `3`; `Queued(Some(4))` | `QueueTimeout` mentioning positions 4 and 3 |
| `wire_zero_position_is_ignored_for_free_slot_candidate` | slots `1`, cap `3`; `Queued(Some(0))`, then progress/completion | success |
| `wire_zero_position_does_not_prove_zero_slot_candidate` | slots `0`, cap `3`; `Queued(Some(0))`, then progress | `QueueTimeout` mentioning unknown position |
| `zero_slot_candidate_fails_closed_without_position` | slots `0`, cap `3`; immediate `InProgress` | `QueueTimeout` mentioning unknown position |
| `free_slot_candidate_can_start_without_position` | slots `1`, cap `3`; immediate progress/completion | success |

Use this status construction consistently:

```rust
DownloadStatus::Queued {
    queue_position: Some(2),
}
```

Use `Duration::from_millis(...)` for sub-second ordering. Do not use wall-clock sleeps or values larger than two simulated
seconds.

- [ ] **Step 5: Run the RED matrix**

```bash
cargo test -p seakarr download::tests::queued_wait_does_not_consume_transfer_timeout -- --nocapture
cargo test -p seakarr download::tests::total_queue_timeout_expires_queued_attempt -- --nocapture
cargo test -p seakarr download::tests::post_start_pause_does_not_reset_transfer_timeout -- --nocapture
```

Expected: compilation fails because `download_file_for_candidate` and the queue state machine do not exist. Do not weaken the
assertions to make current behavior pass.

## Task 5: Implement the queue-aware download state machine

**Files:**

- Modify: `src/download.rs:1-430,528-675`

- [ ] **Step 1: Preserve the public API and add candidate slot context privately**

Keep public `download_file` source-compatible by delegating with one assumed free slot:

```rust
pub async fn download_file(
    client: &dyn SoulseekClient,
    file: &FileInfo,
    username: &str,
    dir: &Path,
    config: &DownloadConfig,
    filters: &crate::config::FilterConfig,
    progress: Option<&ProgressDisplay>,
    cancel: Option<&Arc<AtomicBool>>,
) -> Result<(PathBuf, f64)> {
    download_file_for_candidate(
        client,
        file,
        username,
        1,
        dir,
        config,
        filters,
        progress,
        cancel,
    )
    .await
}
```

Rename the current implementation to `download_file_for_candidate`, make it
private, and add `peer_slots: u8` between `username` and `dir`. Keep its
validation, retry, and throughput body in place. Pass `peer_slots` to
`download_once`. In `download_album`, replace the direct call with
`download_file_for_candidate(..., candidate.slots, ...)`. Do not add queue state to `FileInfo` or duplicate the config value.

- [ ] **Step 2: Add small deadline helpers**

Import `DownloadHandle`, use a 200ms cancellation poll, and add:

```rust
const STATUS_POLL_INTERVAL: Duration = Duration::from_millis(200);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QueueDeadlineKind {
    TotalQueue,
    QueueHead,
}

fn enabled_deadline(start: tokio::time::Instant, seconds: u64) -> Option<tokio::time::Instant> {
    (seconds > 0)
        .then(|| start.checked_add(Duration::from_secs(seconds)))
        .flatten()
}

fn earliest_queue_deadline(
    total: Option<tokio::time::Instant>,
    head: Option<tokio::time::Instant>,
) -> Option<(tokio::time::Instant, QueueDeadlineKind)> {
    match (total, head) {
        (Some(total), Some(head)) if head < total => {
            Some((head, QueueDeadlineKind::QueueHead))
        }
        (Some(total), _) => Some((total, QueueDeadlineKind::TotalQueue)),
        (None, Some(head)) => Some((head, QueueDeadlineKind::QueueHead)),
        (None, None) => None,
    }
}

async fn cancel_and_drain(handle: &mut DownloadHandle) {
    let _ = handle.cancel_tx.send(()).await;
    drain_transfer(&mut handle.status_rx, 5).await;
}

fn cancellation_requested(cancel: Option<&Arc<AtomicBool>>) -> bool {
    cancel.is_some_and(|flag| flag.load(Ordering::SeqCst))
}

async fn stop_with_error(
    handle: &mut DownloadHandle,
    bar: &Option<ProgressBar>,
    error: SeakarrError,
) -> SeakarrError {
    if let Some(bar) = bar {
        bar.finish_and_clear();
    }
    cancel_and_drain(handle).await;
    error
}
```

Use `stop_with_error` from every cancellation, queue-policy, speed, and
transfer-timeout exit that already cancels and drains. Before constructing a
queue error, select the error with cancellation first:

```rust
let error = if cancellation_requested(cancel) {
    SeakarrError::Download("download cancelled by user".into())
} else {
    SeakarrError::QueueTimeout(reason)
};
return Err(stop_with_error(&mut handle, &bar, error).await);
```

- [ ] **Step 3: Initialize separate queue and transfer state**

Replace the enqueue-time generic deadline in `download_once` with:

```rust
let enqueued_at = tokio::time::Instant::now();
let total_queue_deadline = enabled_deadline(enqueued_at, config.max_queue_time_secs);
let mut queue_head_at = None;
let mut observed_queue_position = None;
let mut transfer_start = None;
let mut transfer_deadline = None;
let requires_queue_position = config.max_queue_length > 0 && peer_slots == 0;
```

At the top of every loop, check cancellation first. If no transfer has started, calculate the head deadline from
`queue_head_at` and choose the earlier queue deadline. If transfer has started, use only `transfer_deadline`. Bound the receive
poll by the active deadline:

```rust
let now = tokio::time::Instant::now();
let head_deadline = queue_head_at
    .and_then(|head| enabled_deadline(head, config.max_start_time_secs));
let active_queue_deadline = earliest_queue_deadline(total_queue_deadline, head_deadline);
let active_deadline = if transfer_start.is_some() {
    transfer_deadline
} else {
    active_queue_deadline.map(|(deadline, _)| deadline)
};
let poll_timeout = active_deadline
    .map(|deadline| deadline.saturating_duration_since(now))
    .unwrap_or(STATUS_POLL_INTERVAL)
    .min(STATUS_POLL_INTERVAL);
let message = timeout(poll_timeout, handle.status_rx.recv()).await;
```

Before receiving and after a poll timeout, compare `now` with the active deadline. Queue expiry must identify its exact source:

```rust
let reason = match kind {
    QueueDeadlineKind::TotalQueue => format!(
        "{basename} from {username} exceeded max_queue_time_secs={} (last queue position: {})",
        config.max_queue_time_secs,
        observed_queue_position
            .map(|position| position.to_string())
            .unwrap_or_else(|| "unknown".to_string()),
    ),
    QueueDeadlineKind::QueueHead => format!(
        "{basename} from {username} exceeded max_start_time_secs={} at queue position 1",
        config.max_start_time_secs,
    ),
};
```

Check the cancellation flag again before returning this error, finish any progress bar, call `cancel_and_drain`, log the reason,
and return `SeakarrError::QueueTimeout(reason)`.

- [ ] **Step 4: Enforce queue-position policy on status events**

Add this complete validator:

```rust
fn queue_position_rejection(position: u32, max_queue_length: u32) -> Option<String> {
    if max_queue_length > 0 && position > max_queue_length {
        Some(format!(
            "reported queue position {position}, exceeding max_queue_length={max_queue_length}"
        ))
    } else {
        None
    }
}
```

Before transfer start, handle `Queued { queue_position }` as follows. Prefix a
rejection detail with `{basename} from {username}`, log that full reason, then
use the cancellation-first `stop_with_error` path from Step 2.

```rust
if let Some(position) = queue_position.filter(|position| *position > 0) {
    if let Some(detail) = queue_position_rejection(position, config.max_queue_length) {
        let reason = format!("{basename} from {username} {detail}");
        tracing::warn!("Download queue rejected: {reason}");
        let error = if cancellation_requested(cancel) {
            SeakarrError::Download("download cancelled by user".into())
        } else {
            SeakarrError::QueueTimeout(reason)
        };
        return Err(stop_with_error(&mut handle, &bar, error).await);
    }
    observed_queue_position = Some(position);
    if position == 1 && queue_head_at.is_none() {
        queue_head_at = Some(tokio::time::Instant::now());
    }
}
```

Ignore `Queued` events arriving after transfer start; they must not reset transfer inactivity or retroactively re-enter queue
state. Leave `Paused` as an explicit no-op in both phases so it resets neither queue nor transfer deadlines.

- [ ] **Step 5: Start/reset transfer timing only on real progress**

At the beginning of the `InProgress` arm, fail closed before accepting the first progress event from a zero-slot candidate that
has not reported a valid position:

```rust
let now = tokio::time::Instant::now();
if transfer_start.is_none() && requires_queue_position && observed_queue_position.is_none() {
    let reason = format!(
        "{basename} from {username} started with an unknown queue position while max_queue_length={}",
        config.max_queue_length
    );
    tracing::warn!("Download queue rejected: {reason}");
    let error = if cancellation_requested(cancel) {
        SeakarrError::Download("download cancelled by user".into())
    } else {
        SeakarrError::QueueTimeout(reason)
    };
    return Err(stop_with_error(&mut handle, &bar, error).await);
}
if transfer_start.is_none() {
    transfer_start = Some(now);
}
transfer_deadline = now.checked_add(Duration::from_secs(config.timeout_secs));
```

Apply the same missing-position guard before accepting `Completed` with no prior `InProgress`. Preserve the existing free-slot
`Completed`-without-progress behavior and its `0.0` throughput result. Keep speed checks, EMA display, quality verification, and
normal completion logic unchanged.

On post-start deadline expiry, retain the existing log wording and ordinary `SeakarrError::Download("download timed out")`.
That error remains retryable against the same peer.

- [ ] **Step 6: Make queue-policy errors non-retryable**

Change only the classification predicate and its documentation:

```rust
fn is_retryable(error: &SeakarrError) -> bool {
    !matches!(
        error,
        SeakarrError::QualityRejected(_) | SeakarrError::QueueTimeout(_)
    )
}
```

- [ ] **Step 7: Run the complete state matrix and commit**

```bash
cargo test -p seakarr download::tests::queued_wait_does_not_consume_transfer_timeout -- --nocapture
cargo test -p seakarr download::tests::total_queue_timeout_expires_queued_attempt -- --nocapture
cargo test -p seakarr download::tests::queue_head_timeout_starts_at_position_one -- --nocapture
cargo test -p seakarr download::tests::time_at_position_two_does_not_consume_start_limit -- --nocapture
cargo test -p seakarr download::tests::pre_start_pause_does_not_start_transfer_timeout -- --nocapture
cargo test -p seakarr download::tests::post_start_pause_does_not_reset_transfer_timeout -- --nocapture
cargo test -p seakarr download::tests::zero_queue_timers_allow_delayed_start -- --nocapture
cargo test -p seakarr download::tests::zero_queue_cap_preserves_admitted_free_slot_candidate -- --nocapture
cargo test -p seakarr download::tests::positive_cap_rejects_out_of_bound_position -- --nocapture
cargo test -p seakarr download::tests::wire_zero_position_is_ignored_for_free_slot_candidate -- --nocapture
cargo test -p seakarr download::tests::wire_zero_position_does_not_prove_zero_slot_candidate -- --nocapture
cargo test -p seakarr download::tests::zero_slot_candidate_fails_closed_without_position -- --nocapture
cargo test -p seakarr download::tests::free_slot_candidate_can_start_without_position -- --nocapture
git diff --check
git add Cargo.toml src/error.rs src/download.rs
git commit -m "feat: separate queue waits from transfer inactivity"
```

Expected: all tests complete using simulated time, so this gate should take seconds rather than waiting for configured wall-clock
limits.

## Task 6: Prove retry, fallback, cancellation, and throughput integration

**Files:**

- Modify: `src/download.rs` test module
- Test: existing `tests/pipeline_test.rs` without modification

- [ ] **Step 1: Test queue timeout skips same-peer retries and falls back**

Use `ScriptedClient` with two scripts: the queued peer reports position 1 and waits past a one-second head limit; the free-slot
peer immediately reports progress and completion. Build two one-file candidates in that order, set `max_retries = 3`, and assert:

```rust
assert!(result.is_ok());
assert_eq!(client.calls.load(Ordering::SeqCst), 2);
assert_eq!(
    client.usernames.lock().unwrap().as_slice(),
    ["queued-peer", "free-peer"]
);
assert_eq!(client.cancellations.load(Ordering::SeqCst), 1);
```

The call count is the regression guard: a retryable error would call the queued peer four times before fallback.

- [ ] **Step 2: Test queue wait stays out of effective throughput**

Use a 1,024-byte file, wait five simulated seconds at an in-bound queue position, emit first progress, wait one simulated second,
then complete. Assert the reported throughput is approximately one KiB/s rather than approximately one-sixth KiB/s:

```rust
assert!((speed_kbps - 1.0).abs() < 0.05, "unexpected speed: {speed_kbps}");
```

- [ ] **Step 3: Test cancellation priority over queue expiry**

Start a queued attempt with a cancellation flag already set. Preserve the
returned error for both assertions:

```rust
let error = result.unwrap_err();
assert!(error.to_string().contains("download cancelled by user"));
assert!(!matches!(error, SeakarrError::QueueTimeout(_)));
assert_eq!(client.cancellations.load(Ordering::SeqCst), 1);
```

This preserves Ctrl+C priority and the existing staging cleanup path.

- [ ] **Step 4: Run integration and legacy regressions**

```bash
cargo test -p seakarr queue_timeout_falls_back_without_same_peer_retry -- --nocapture
cargo test -p seakarr queue_wait_is_excluded_from_effective_throughput -- --nocapture
cargo test -p seakarr cancellation_wins_over_queue_expiry -- --nocapture
cargo test -p seakarr download::tests::download_retries_same_peer_after_failure -- --nocapture
cargo test -p seakarr download::tests::test_cancellation_flag_aborts_download_and_cleans_up -- --nocapture
cargo test -p seakarr download::tests::test_failed_candidate_cleans_up_part_files -- --nocapture
cargo test -p seakarr --test pipeline_test test_full_pipeline_manual_mode -- --nocapture
```

Expected: queue timeout makes one attempt per queued peer, transfer failures still retry, candidate fallback succeeds, cancellation
cleans staging, and the existing manual pipeline remains green.

- [ ] **Step 5: Commit integration coverage**

```bash
git diff --check
git add src/download.rs
git commit -m "test: cover queued peer fallback and accounting"
```

## Task 7: Document the activated queue policy

**Files:**

- Modify: `src/config.rs:6-23`
- Modify: `README.md:1-25,220-240,300-325,400-410`

- [ ] **Step 1: Remove only the activated keys from the reserved comment**

The `DownloadConfig` reserved list must become:

```rust
//   DownloadConfig::max_download_time_mins, min_filtered_users,
//                skip_retry_hours
```

Keep all other parsed-but-unwired keys and the `DownloadConfig` fields/defaults unchanged.

- [ ] **Step 2: Replace the three README config descriptions**

Use these semantics in the `download` table:

```markdown
| `max_queue_length` | `0` requires a free upload slot during candidate selection and does not retroactively reject an admitted free-slot peer. A positive value also permits zero-slot peers only with an in-bound positive position; unknown and wire-zero positions fail closed for those peers. | `0` |
| `max_start_time_secs` | Maximum seconds from first reaching queue position `1` until the first transfer progress. `0` disables this queue-head limit. | `120` |
| `max_queue_time_secs` | Maximum total seconds from enqueue until the first transfer progress. `0` disables this total queue limit. | `1800` |
```

Clarify `timeout_secs` as applying from first `InProgress` and resetting only on later `InProgress` events; paused status does not
reset it.

- [ ] **Step 3: Update feature, workflow, and FAQ wording**

Update the quality-filtering feature and automatic-mode filter step to say that candidates need a free slot when
`max_queue_length` is zero, while positive caps admit zero-slot candidates for observed-position validation. Update the download
step to distinguish total queue time, queue-head start time, and post-start transfer inactivity.

Replace the queued-peer FAQ answer with:

```markdown
Yes. Keep `download.max_queue_length: 0` for free-slot-only candidate selection, or set a positive limit to try zero-slot peers
whose reported positive queue position is within that bound. Unknown and out-of-bound positions fail closed and move to the next
candidate without retrying the same peer. Wire position `0` is non-actionable and cannot prove a zero-slot candidate eligible; a
candidate admitted with a free slot is not retroactively rejected by later telemetry. `max_queue_time_secs` limits total queue
wait, `max_start_time_secs` limits the wait after reaching position `1`, and `timeout_secs` controls inactivity only after
transfer progress begins.
```

- [ ] **Step 4: Lint documentation and commit**

```bash
markdownlint --fix README.md
markdownlint README.md
git diff --check
git add README.md src/config.rs
git commit -m "docs: explain queued download limits"
```

Expected: markdownlint and diff checks pass. Do not stage or rewrite the pre-existing spec edit in this commit.

## Task 8: Run full verification and review handoff

**Files:**

- Verify all files changed in Tasks 1-7; create no new production file.

- [ ] **Step 1: Audit status variants and queue config use**

```bash
rg -n "DownloadStatus::Queued(?!\\s*\\{)" \
  src vendor/soulseek-rs-lib --glob '*.rs' --pcre2
rg -n "max_queue_length|max_start_time_secs|max_queue_time_secs" src README.md
```

Expected: the first command finds no stale unit-variant construction or match. The second shows production reads in filter,
search/runner plumbing, and download timing in addition to config definitions/tests/docs.

- [ ] **Step 2: Format and lint Rust**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
```

Expected: both commands exit zero with no warnings.

- [ ] **Step 3: Run the full workspace suite once**

```bash
cargo test --workspace
```

Expected: application unit/integration tests, vendor unit/e2e tests, and doctests all pass. Do not repeat the full suite between
individual implementation tasks; use focused tests there to avoid the excessive runtime seen in the previous attempt.

- [ ] **Step 4: Run repository gates**

```bash
pre-commit run --all-files
git diff --check
git status --short --branch
```

Expected: all hooks pass, the diff has no whitespace errors, and status lists only intentional implementation/doc artifacts plus
the previously present spec/plan state.

- [ ] **Step 5: Hand off to the required two-reviewer loop**

Review the full implementation diff with both configured reviewer models. Require explicit coverage of:

- optional queue-position propagation from `PlaceInQueueUpdate` through the application bridge;
- cap-zero rejection, positive-cap bounds, invalid zero, and unknown-position fail-closed behavior;
- total versus queue-head deadline precedence and zero-disabled semantics;
- pre/post-start paused behavior and transfer deadline resets;
- cancellation/drain ordering, non-retryable queue errors, staging cleanup, and candidate fallback;
- queue-wait exclusion from peer throughput;
- backward-compatible public filter/search/download helper behavior;
- README/config consistency and absence of out-of-scope setting activation.

Fix concrete findings only within the review step, rerun affected focused tests after each fix, and run the full verification gate
once more after the review reaches a terminal clean state. Do not claim completion from an agent report alone.

## Task 9: Request queue positions and reject late status regression

**Files:**

- Modify: `vendor/soulseek-rs-lib/src/message/server/message_factory.rs`
- Modify: `vendor/soulseek-rs-lib/src/actor/peer_actor.rs`
- Modify: `vendor/soulseek-rs-lib/src/actor/peer_registry.rs`
- Modify: `vendor/soulseek-rs-lib/src/client/downloads.rs`
- Modify: `vendor/soulseek-rs-lib/src/download_store.rs`
- Modify: `README.md`

This task resolves the first review round's concrete findings. Keep the work unstaged and uncommitted.

- [x] **Step 1: Add RED wire and actor lifecycle tests**

Add an exact peer-code-51 factory test beside the existing code-44 test:

```rust
#[test]
fn test_build_place_in_queue_request() {
    let message = MessageFactory::build_place_in_queue_request("song.mp3");
    let expect: Vec<u8> = [
        51, 0, 0, 0,
        8, 0, 0, 0, 115, 111, 110, 103, 46, 109, 112, 51,
    ]
    .to_vec();
    assert_eq!(expect, message.get_data());
}
```

In `peer_actor.rs`, use the existing real-loopback `connected_actor` fixture to assert that
`PeerMessage::QueueUpload("song.mp3")` writes code 43 followed immediately by code 51. Force that file's next-request instant
into the past and call `tick()` to assert a second code-51 frame. Add focused assertions that `UploadFailed`,
`TransferRequest`, and `StopQueuePositionRequests` remove the matching filename from the schedule.

- [x] **Step 2: Run the protocol tests to prove RED**

```bash
cargo test -p soulseek-rs-lib message::server::message_factory::test_build_place_in_queue_request -- --nocapture
cargo test -p soulseek-rs-lib actor::peer_actor::tests::queue_upload_requests_position_immediately_and_periodically -- --nocapture
```

Expected: compilation fails because `build_place_in_queue_request`, the request schedule, and the stop message do not exist.
Capture this failure before adding production behavior.

- [x] **Step 3: Add code-51 construction and actor-owned scheduling**

Add the message builder:

```rust
pub fn build_place_in_queue_request(filename: &str) -> Message {
    let mut message = Message::new();
    message.write_int32(51).write_string(filename);
    message
}
```

Add `PeerMessage::StopQueuePositionRequests(String)`, a
`queue_position_requests: HashMap<String, Instant>` actor field, and this cadence:

```rust
const QUEUE_POSITION_REQUEST_INTERVAL: Duration = Duration::from_mins(5);
```

When handling `QueueUpload(filename)`, send `build_queue_upload(&filename)`, then
`build_place_in_queue_request(&filename)`, then schedule the next request for `Instant::now() +
QUEUE_POSITION_REQUEST_INTERVAL`. During each connected `tick`, collect due filenames before mutating the map, send code 51 for
each, and advance only still-present entries by five minutes. Sending a request must refresh normal socket activity before the
idle-disconnect check.

Remove the schedule entry before forwarding a matching `TransferRequest` or `UploadFailed`; handle
`StopQueuePositionRequests(filename)` idempotently. Drop of the actor naturally clears all remaining schedules.

- [x] **Step 4: Stop polling when callers remove queued state**

Add this registry boundary:

```rust
pub fn stop_queue_position_requests(
    &self,
    username: &str,
    filename: String,
) -> Result<(), String> {
    self.send_to_peer(username, PeerMessage::StopQueuePositionRequests(filename))
}
```

After `Client::remove_queued_download` or `Client::remove_download` successfully removes a record, clone the optional peer
registry outside the context write-lock and call `stop_queue_position_requests`. Ignore a missing/disconnected actor because the
state is already gone with that actor. Do not change pause/resume behavior: polling has already stopped once an active transfer
request is received.

- [x] **Step 5: Add RED/GREEN protection against late queue responses**

First add a store test that creates an `InProgress` download with a live receiver, calls
`update_queue_position("peer", "song.mp3", 7)`, and asserts `false`, no channel message, unchanged status, and unchanged
`queue_position`. Run it before the guard:

```bash
cargo test -p soulseek-rs-lib download_store::tests::late_queue_position_does_not_regress_in_progress_download -- --nocapture
```

Expected RED: the current store returns true, publishes `Queued`, and overwrites `InProgress`. Then select only matching
records that are still queued. Search newest-first so a terminal old attempt cannot consume a tokenless response intended for a
newer queued retry.

Keep repeated updates for still-queued records valid. Rerun the new store test and the existing
`update_queue_position_updates_store_and_notifies_receiver` test; both must pass.

- [x] **Step 6: Document active telemetry and run focused GREEN checks**

In README automatic-mode step 5, state that seakarr requests queue position immediately and every five minutes while queued.
Then run:

```bash
cargo fmt --all
cargo test -p soulseek-rs-lib message::server::message_factory::test_build_place_in_queue_request -- --nocapture
cargo test -p soulseek-rs-lib actor::peer_actor::tests::queue_upload_requests_position_immediately_and_periodically -- --nocapture
cargo test -p soulseek-rs-lib actor::peer_actor::tests::queue_position_polling_stops_when_queue_lifecycle_ends -- --nocapture
cargo test -p soulseek-rs-lib download_store::tests::late_queue_position_does_not_regress_in_progress_download -- --nocapture
cargo test -p soulseek-rs-lib download_store::tests::update_queue_position_updates_store_and_notifies_receiver -- --nocapture
markdownlint --fix README.md docs/agent/specs/2026-09-09-queue-aware-download-timeouts-design.md \
  docs/agent/plans/2026-09-09-queue-aware-download-timeouts.md
markdownlint README.md docs/agent/specs/2026-09-09-queue-aware-download-timeouts-design.md \
  docs/agent/plans/2026-09-09-queue-aware-download-timeouts.md
git diff --check
```

Expected: all focused tests and documentation gates pass.

- [ ] **Step 7: Re-run full gates and the same two-reviewer loop**

```bash
cargo fmt --all -- --check
cargo check --workspace
cargo check -p soulseek-rs-lib --example stress
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
pre-commit run --all-files
git diff --check
git status --short --branch
```

Expected: every command exits zero and the workspace test summary has no failures. Keep all files unstaged and uncommitted. Then
repeat review with the same two distinct reviewer models; require zero issues from both in the same round before final
verification.

## Task 10: Isolate delayed cleanup by download attempt

**Files:**

- Modify: `vendor/soulseek-rs-lib/src/types.rs`
- Modify: `vendor/soulseek-rs-lib/src/actor/peer_actor.rs`
- Modify: `vendor/soulseek-rs-lib/src/actor/peer_registry.rs`
- Modify: `vendor/soulseek-rs-lib/src/client/downloads.rs`
- Modify: `vendor/soulseek-rs-lib/src/client/operations.rs`
- Modify: `vendor/soulseek-rs-lib/src/client/tests.rs`
- Modify: `vendor/soulseek-rs-lib/src/download_store.rs`
- Modify: `vendor/soulseek-rs-lib/src/peer/download_peer.rs`
- Modify: `src/client.rs`

This task resolves review round 2's zero-delay retry race. Keep the work unstaged and uncommitted.

- [x] **Step 1: Add and run the RED attempt-isolation tests**

Add `stale_stop_preserves_requeued_file_position_polling` to queue attempt 1, re-queue attempt 2, deliver attempt 1's delayed
stop, and require attempt 2's schedule to survive. Add `remove_download_by_attempt_id_preserves_newer_same_file` with two
same-user/same-file records and require removal of only attempt 1. Run both exact filters before production changes; expected RED
is missing attempt-aware variants/fields and `remove_download_by_attempt_id`.

- [x] **Step 2: Add a stable attempt identity**

Add `attempt_id: u32` to `Download`, initialized to the first local download token and copied unchanged when
`ClientOperation::UpdateDownloadTokens` replaces the mutable wire `token`. Update every `Download` literal. The attempt ID is
internal lifecycle identity and is never serialized on the Soulseek wire.

- [x] **Step 3: Make polling schedules and stops attempt-aware**

Represent each actor schedule as:

```rust
struct QueuePositionRequest {
    attempt_id: u32,
    next_request: Instant,
}
```

Change `PeerMessage::QueueUpload` to carry `filename` and `attempt_id`, and change `StopQueuePositionRequests` to carry
`filename` plus `Option<u32>`. A token-scoped stop removes only a schedule whose stored attempt ID matches; `None` retains the
existing explicit remove-by-filename semantics. Upload failure remains filename-wide because the wire message carries no local
attempt ID. Transfer start defers cleanup to `UpdateDownloadTokens`, which selects the newest queued same-file attempt, preserves
its stable ID during wire-token migration, and sends an attempt-scoped stop before the transfer response.

Thread `attempt_id` through both queue-upload call sites: immediate `download_with_metadata` and the `PeerConnected` flush.

- [x] **Step 4: Remove only the bridge's own attempt**

Add `DownloadStore::remove_by_attempt_id` returning the removed `Download`, then add:

```rust
#[must_use]
pub fn remove_download_by_attempt_id(&self, attempt_id: u32) -> bool
```

The client method removes only that record and sends a token-scoped stop with the removed record's username, filename, and
attempt ID. Capture `download_handle.attempt_id` before moving the handle into seakarr's bridge and replace its filename-based
cleanup call with `remove_download_by_attempt_id`. Preserve the existing public filename-removal APIs for their current callers.

- [ ] **Step 5: Run GREEN, full gates, and another two-reviewer round**

```bash
cargo fmt --all
cargo test -p soulseek-rs-lib actor::peer_actor::tests::stale_stop_preserves_requeued_file_position_polling -- --nocapture
cargo test -p soulseek-rs-lib client::tests::remove_download_by_attempt_id_preserves_newer_same_file -- --nocapture
cargo test -p soulseek-rs-lib actor::peer_actor::tests::queue_upload_requests_position_immediately_and_periodically -- --nocapture
cargo test -p soulseek-rs-lib actor::peer_actor::tests::queue_position_polling_stops_when_queue_lifecycle_ends -- --nocapture
cargo test -p soulseek-rs-lib
cargo fmt --all -- --check
cargo check --workspace
cargo check -p soulseek-rs-lib --example stress
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
markdownlint README.md docs/agent/specs/2026-09-09-queue-aware-download-timeouts-design.md \
  docs/agent/plans/2026-09-09-queue-aware-download-timeouts.md
pre-commit run --all-files
git diff --check
git status --short --branch
```

Expected: all commands exit zero; then both configured reviewer models report zero concrete issues in the same review round.

## Task 11: Reconcile wire-zero and overlapping-retry review findings

**Files:**

- Modify: `src/download.rs`
- Modify: `src/client.rs`
- Modify: `vendor/soulseek-rs-lib/src/download_store.rs`
- Modify: `vendor/soulseek-rs-lib/src/client/downloads.rs`
- Modify: `vendor/soulseek-rs-lib/src/client/operations.rs`
- Modify: `vendor/soulseek-rs-lib/src/actor/peer_actor.rs`
- Modify: `vendor/soulseek-rs-lib/src/types.rs`
- Modify: `README.md`
- Modify: `docs/agent/specs/2026-09-09-queue-aware-download-timeouts-design.md`

This task resolves review round 3's eight de-duplicated findings. Keep the work unstaged and uncommitted.

- [x] **Step 1: Drive wire-zero and cap-zero compatibility with RED tests**

Replace the superseded zero-position rejection case with deterministic tests proving that wire `0` is non-actionable, cannot
prove a zero-slot candidate eligible, and does not abort a free-slot candidate. Replace cap-zero's post-admission rejection test
with a regression proving that a candidate admitted with an advertised free slot is not retroactively rejected. Run every test
before production changes and capture its policy-specific `QueueTimeout` failure.

- [x] **Step 2: Normalize and route tokenless queue responses safely**

Add RED vendor tests proving wire `0` publishes `Queued { queue_position: None }` and an overlapping old terminal record cannot
consume a response while a newer same-file attempt remains queued. Normalize zero and select the newest still-queued match.

- [x] **Step 3: Lock the application bridge cleanup seam**

Add an application-level RED regression that creates two same-user/same-file vendor attempts, forwards the old attempt to a
terminal status through the real bridge helper, and requires cleanup to preserve the new attempt. Centralize forwarding plus
`remove_download_by_attempt_id` in the helper used by production.

- [x] **Step 4: Resolve structural, logging, and documentation findings**

Clone the peer registry during the same context-lock acquisition used for attempt removal, clarify lifecycle cleanup boundaries,
downgrade expected cleaned-token transfer noise to debug, correct the stale bridge comment, and align the spec, plan, README, and
status-field documentation with wire semantics.

- [ ] **Step 5: Run focused checks and continue the bounded two-reviewer loop**

Run the changed state-machine, store, actor, client, and bridge regressions. Then dispatch both configured reviewer models in
parallel against the complete current diff. Continue until both return zero issues in one round or the five-round cap is reached.

## Task 12: Resolve round-4 lifecycle and failure findings

**Files:**

- Modify: `src/download.rs`
- Modify: `src/client.rs`
- Modify: `vendor/soulseek-rs-lib/src/download_store.rs`
- Modify: `vendor/soulseek-rs-lib/src/client/downloads.rs`
- Modify: `vendor/soulseek-rs-lib/src/client/operations.rs`
- Modify: `vendor/soulseek-rs-lib/src/client/tests.rs`
- Modify: `vendor/soulseek-rs-lib/src/actor/peer_actor.rs`
- Modify: `vendor/soulseek-rs-lib/src/types.rs`
- Modify: `README.md`
- Modify: `docs/agent/specs/2026-09-09-queue-aware-download-timeouts-design.md`

This task resolves review round 4. Keep the work unstaged and uncommitted.

- [x] **Step 1: Remove redundant terminal cancellation and make direct candidate validation live**

Add RED tests proving a terminal vendor failure returns without a second cancel/drain and a direct cap-zero/zero-slot call is
rejected before contacting the peer. Expose `download_file_for_candidate` as the queue-aware public entry while retaining the
source-compatible free-slot wrapper, then remove the unreachable event-time cap-zero branch.

- [x] **Step 2: Pause cancellation by stable attempt identity**

Add a RED overlapping same-file regression for `pause_download_by_attempt_id`, implement it through `DownloadStore`, and switch
the application cancellation task from filename lookup to the captured attempt ID. Rename the vendor `Download` binding so it
cannot be confused with the application `DownloadHandle`.

- [x] **Step 3: Correlate transfer-start cleanup before stopping telemetry**

Add RED actor and operation tests. The actor must defer polling cleanup; `UpdateDownloadTokens` must select the newest matching
queued attempt, preserve its stable ID while replacing the wire token, and enqueue an attempt-scoped stop before replying to the
peer.

- [x] **Step 4: Record accepted policy boundaries**

Keep code-51 telemetry policy-independent because slot snapshots can become stale and position `1` drives queue-head timing.
Keep the documented default 1800-second total queue window. Per the user's decision, retain the u32 attempt ID and document its
practically unreachable 2^31-attempt single-session wrap limit instead of introducing a second wider identity model.

- [x] **Step 5: Run focused checks and the fifth review round**

Format the workspace, run the changed application/vendor regressions and Markdown lint, then dispatch both configured reviewer
models in parallel against the complete current diff. This is the fifth and final bounded review round.

The final round reported two P2 notes. Replace the weak pre-set cancellation test with deterministic cancellation-during-poll
coverage for both queue expiry and position rejection. Add a one-second idle-reaper grace when a queue-position poll is due soon,
so the five-minute idle threshold cannot win the sub-tick boundary before the five-minute poll. Focused regressions pass; the
round cap prevents another reviewer verification pass, so the review gate completes rather than claiming a clean pass.

## Spec coverage self-check

| Spec requirement | Planned coverage |
| --- | --- |
| Queue-position propagation | Tasks 1-2 and 9 |
| Active immediate and five-minute queue-position requests | Task 9 |
| Late queue responses cannot regress transfer state | Task 9 |
| Delayed cleanup cannot affect a newer same-file retry | Tasks 10-12 |
| Tokenless telemetry targets the newest still-queued overlap | Tasks 11-12 |
| Attempt-scoped bridge cancellation and terminal failure handling | Task 12 |
| Unknown/wire-zero versus actionable positive position | Tasks 1, 2, 4, 5, 9, and 11 |
| Cap-zero free-slot behavior | Tasks 3-5 and 11-12 |
| Positive-cap eligibility and enforcement | Tasks 3-6 |
| Total and queue-head limits | Tasks 4-5 |
| Transfer inactivity starts/resets only on progress | Tasks 4-5 |
| Pre/post-start paused semantics | Tasks 2, 4, and 5 |
| Typed queue failure, no same-peer retry, candidate fallback | Tasks 4-6 |
| Cancellation, draining, staging cleanup, and throughput accounting | Tasks 5-6 |
| README/config documentation | Task 7 |
| Existing tests and quality gates | Tasks 6 and 8 |
| Out-of-scope settings remain unused | Scope check, Task 7, and Task 8 review |

No spec requirement is intentionally deferred.
