# Reliable-peer preference Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use sub-agents (recommended) to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Prefer a previously-reliable peer when searching for a second album by the same artist, without any new Soulseek protocol surface.

**Architecture:** Keep an in-memory `artist → peer` map (`ReliablePeers`) shared across the run's album futures. After a download, record the peer if it served the album cleanly (first candidate, no retries); on a later same-artist search, log "preferring reliable peer …" and reorder the ranked candidates so that peer is tried first. Everything gates behind a new config toggle `search.prefer_reliable_peer` (default true).

**Tech Stack:** Rust, tokio, serde/serde_yaml, tracing. No new dependencies.

---

## File map

- **Create** `src/reliable.rs` — the `ReliablePeers` map (record/get/evict).
- **Create** nothing else. Modify:
  - `src/lib.rs` — register `pub mod reliable;`.
  - `src/config.rs` — `SearchConfig.prefer_reliable_peer` (default true) + tests.
  - `src/download.rs` — `DownloadStats` struct; `download_file` gains a `retried` out-param; `download_album` gains a `stats` out-param; update its test call sites.
  - `src/search.rs` — `promote_peer` helper + test.
  - `src/runner.rs` — thread `&ReliablePeers` through `process_album`, add logging + promotion + record/evict; update the auto-loop entry and its `process_album` test call sites.
  - `src/main.rs` — create/thread `ReliablePeers` at the manual and batch entry points.
  - `README.md` — document the feature + config key.

---

## Task 1: Config toggle `search.prefer_reliable_peer`

**Files:**
- Modify: `src/config.rs` (`SearchConfig`, line ~61)
- Test: `src/config.rs` (add to existing `#[cfg(test)] mod tests`)

- [ ] **Step 1: Write the failing test**

Add to the `mod tests` in `src/config.rs`:

```rust
#[test]
fn search_prefers_reliable_peer_by_default() {
    let cfg: SearchConfig = serde_yaml::from_str("{}").unwrap();
    assert!(cfg.prefer_reliable_peer);
}

#[test]
fn search_prefer_reliable_peer_can_be_disabled() {
    let cfg: SearchConfig = serde_yaml::from_str("prefer_reliable_peer: false").unwrap();
    assert!(!cfg.prefer_reliable_peer);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p seakarr config::tests::search_prefers_reliable_peer_by_default`
Expected: FAIL (compile error — `no field prefer_reliable_peer on SearchConfig`).

- [ ] **Step 3: Implement the field**

In `src/config.rs`, in `pub struct SearchConfig`, insert after the `search_title_match` field (before `manual`):

```rust
    #[serde(default = "default_true")]
    pub prefer_reliable_peer: bool,
```

`default_true()` already exists in `src/config.rs` (returns `true`); no new helper.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p seakarr config::tests::search_prefer`
Expected: PASS (both tests).

- [ ] **Step 5: Commit**

```bash
git add src/config.rs
git commit -m "feat(config): add search.prefer_reliable_peer toggle (default true)"
```

---

## Task 2: `ReliablePeers` map

**Files:**
- Create: `src/reliable.rs`
- Modify: `src/lib.rs` (add `pub mod reliable;` after `pub mod progress;`)

- [ ] **Step 1: Write the failing test** (create `src/reliable.rs` with tests, before the impl)

```rust
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

impl ReliablePeers {
    #[must_use]
    pub fn new() -> Self {
        Self {
            peers: Mutex::new(HashMap::new()),
        }
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
}
```

- [ ] **Step 2: Register the module**

In `src/lib.rs`, add after `pub mod progress;`:

```rust
pub mod reliable;
```

- [ ] **Step 3: Run tests to verify they pass**

Run: `cargo test -p seakarr reliable::`
Expected: PASS (3 tests). (The module compiles because the impl is present; the tests are included in the same file, so they pass immediately. If you want a true RED, stub `record/get/evict` to no-ops first, watch the 3 tests fail, then fill in the impl — then run again.)

- [ ] **Step 4: Commit**

```bash
git add src/reliable.rs src/lib.rs
git commit -m "feat: add ReliablePeers in-memory cache for known-good peers"
```

---

## Task 3: Report download reliability (`DownloadStats`)

**Files:**
- Modify: `src/download.rs`

- [ ] **Step 1: Add `DownloadStats` near the top of `src/download.rs`** (after the imports/consts, before `fn download_file`)

```rust
/// How an album download completed, so the runner can tell a clean first-
/// candidate success (a "reliable" peer) from one that needed fallback or
/// retries.
#[derive(Debug, Default, Clone, Copy)]
pub struct DownloadStats {
    /// 1-based count of candidates tried before success (equals the candidate
    /// list length when every candidate failed).
    pub candidates_tried: usize,
    /// True when at least one file was retried after a failed attempt.
    pub retried: bool,
}
```

- [ ] **Step 2: Thread a `retried` flag through `download_file`**

Change the signature (currently ends `cancel: Option<&Arc<AtomicBool>>`) to add a trailing param:

```rust
    cancel: Option<&Arc<AtomicBool>>,
    retried: &mut bool,
```

In the retry loop, at the start of the `if attempt > 0 {` block (before the `tracing::info!("Retrying download …")`), add:

```rust
        if attempt > 0 {
            *retried = true;
            tracing::info!(
                "Retrying download of {basename} from {username} (attempt {attempt}/{})",
                config.max_retries
            );
            tokio::time::sleep(Duration::from_secs(config.retry_delay_secs)).await;
        }
```

(Keep the existing `tracing::info!` line intact; just add `*retried = true;` above it.)

- [ ] **Step 3: Thread `stats` through `download_album`**

Change the signature (currently ends `cancel: Option<&Arc<AtomicBool>>`) to add a trailing param:

```rust
    cancel: Option<&Arc<AtomicBool>>,
    stats: &mut DownloadStats,
```

At the top of the function, before `let mut last_err`, add:

```rust
    let mut candidates_tried = 0usize;
    let mut retried = false;
```

Inside the `for candidate in candidates {` loop, as its first statement, add:

```rust
        candidates_tried += 1;
```

Change the `download_file(...)` call to pass the flag:

```rust
            match download_file(
                client,
                file,
                &candidate.username,
                &disc_dir,
                config,
                filters,
                progress,
                cancel,
                &mut retried,
            )
            .await
```

At the success return (`if !failed { return Ok(downloaded); }`), set stats first:

```rust
        if !failed {
            stats.candidates_tried = candidates_tried;
            stats.retried = retried;
            return Ok(downloaded);
        }
```

At the final failure return (`Err(last_err.unwrap_or_else(…))`), set stats first:

```rust
    stats.candidates_tried = candidates_tried;
    stats.retried = retried;
    Err(last_err.unwrap_or_else(|| SeakarrError::Download("all candidates exhausted".into())))
```

- [ ] **Step 4: Update every `download_album` and `download_file` test call site**

`download_album(...)` test call sites are at `src/download.rs` lines 1052, 1314, 1379, 1464, 1546, 1638, 1727, 1805, 1901, 1941, 1985, 2037. Add a trailing `&mut DownloadStats::default()` to each.

`download_file(...)` test call sites are at `src/download.rs` lines 977, 1011, 1131, 1169, 1209, 1243, 1271, 1846, 1873, 2110, 2142. Add a trailing `&mut false` to each.

- [ ] **Step 5: Compile + run download tests**

Run: `cargo test -p seakarr download::`
Expected: PASS (all download tests, with the mechanical arg additions).

- [ ] **Step 6: Commit**

```bash
git add src/download.rs
git commit -m "feat(download): report download reliability (candidates tried, retries)"
```

---

## Task 4: `promote_peer` helper

**Files:**
- Modify: `src/search.rs`

- [ ] **Step 1: Write the failing test**

Add to `src/search.rs` `#[cfg(test)] mod tests`:

```rust
#[test]
fn promote_peer_moves_matches_to_the_front_preserving_order() {
    let mk = |u: &str| crate::client::SearchResult {
        username: u.to_string(),
        speed: 0,
        slots: 0,
        files: Vec::new(),
    };
    let results = vec![mk("a"), mk("b"), mk("c"), mk("b")];
    let promoted = promote_peer(results, "b");
    let names: Vec<String> = promoted.iter().map(|r| r.username.clone()).collect();
    assert_eq!(names, vec!["b", "b", "a", "c"]);
}

#[test]
fn promote_peer_returns_unchanged_when_peer_absent() {
    let mk = |u: &str| crate::client::SearchResult {
        username: u.to_string(),
        speed: 0,
        slots: 0,
        files: Vec::new(),
    };
    let results = vec![mk("a"), mk("c")];
    let promoted = promote_peer(results, "zzz");
    let names: Vec<String> = promoted.iter().map(|r| r.username.clone()).collect();
    assert_eq!(names, vec!["a", "c"]);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p seakarr search::tests::promote_peer`
Expected: FAIL (compile error — `promote_peer` not defined).

- [ ] **Step 3: Implement**

Add to `src/search.rs` (next to the other pub helpers):

```rust
/// Move results from `peer` to the front of the ranked list, preserving the
/// relative order of the rest. Used to prefer a previously-reliable peer so
/// the downloader tries it first. A no-op when `peer` produced no results.
pub fn promote_peer(results: Vec<SearchResult>, peer: &str) -> Vec<SearchResult> {
    let mut front = Vec::with_capacity(results.len());
    let mut rest = Vec::with_capacity(results.len());
    for r in results {
        if r.username == peer {
            front.push(r);
        } else {
            rest.push(r);
        }
    }
    front.extend(rest);
    front
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p seakarr search::tests::promote_peer`
Expected: PASS (2 tests).

- [ ] **Step 5: Commit**

```bash
git add src/search.rs
git commit -m "feat(search): add promote_peer helper"
```

---

## Task 5: Wire the feature into `process_album` + entry points

**Files:**
- Modify: `src/runner.rs`
- Modify: `src/main.rs`

- [ ] **Step 1: Add the `reliable_peers` parameter to `process_album`**

In `src/runner.rs`, change the signature to add `reliable_peers: &crate::reliable::ReliablePeers` after `config: &Config`:

```rust
pub async fn process_album(
    client: &dyn SoulseekClient,
    artist: &str,
    album: Option<&str>,
    config: &Config,
    reliable_peers: &crate::reliable::ReliablePeers,
    db: &Database,
    staging_dir: &Path,
    progress: Option<&ProgressDisplay>,
    cancel: Option<&Arc<AtomicBool>>,
    library_track_count: Option<usize>,
    target_library_path: Option<&Path>,
) -> Result<AlbumOutcome> {
```

- [ ] **Step 2: Log "preferring" before the search**

In `process_album`, replace the `// Search for artist + album.` block:

```rust
    // Prefer a previously-reliable peer for this artist, if known and enabled.
    let preferred_peer = if config.search.prefer_reliable_peer {
        reliable_peers.get(artist)
    } else {
        None
    };
    if let Some(peer) = &preferred_peer {
        tracing::info!(
            "Searching {artist} — {}, preferring reliable peer {peer}...",
            album.unwrap_or("(all)")
        );
    }

    // Search for artist + album.
    let search_start = std::time::Instant::now();
```

- [ ] **Step 3: Promote after ranking**

Find (after the title-search tier):

```rust
    let ranked = filter::rank_candidates(&filtered, &config.filters, rank_album);
```

Keep the existing `tracing::info!(... "best: ...")` line that follows it, then insert the promotion immediately after that log line:

```rust
    let ranked = if let Some(peer) = preferred_peer.as_deref() {
        search::promote_peer(ranked, peer)
    } else {
        ranked
    };
```

- [ ] **Step 4: Record / evict from the download outcome**

Change the download call to pass stats:

```rust
    let mut stats = download::DownloadStats::default();
    let downloaded = match download::download_album(
        client,
        &ranked,
        &album_staging,
        &config.download,
        &config.filters,
        progress,
        cancel,
        &mut stats,
    )
    .await
    {
        Ok(files) => files,
        Err(e) => {
            if config.search.prefer_reliable_peer {
                reliable_peers.evict(artist);
            }
            let reason = if e.to_string().contains("cancelled") {
                e.to_string()
            } else {
                format!("all candidates exhausted: {e}")
            };
            tracing::warn!(
                "{artist} — {}: download failed ({reason}); {} candidates exhausted",
                album.unwrap_or("(all)"),
                ranked.len(),
            );
            return Ok(AlbumOutcome::Failed { reason });
        }
    };
```

Immediately after the `match` (before the `// Library upgrade …` block), add:

```rust
    // Remember a peer that served the album cleanly; drop a stale entry when
    // it didn't (fallback candidate or a retry means it wasn't reliable).
    if config.search.prefer_reliable_peer {
        if stats.candidates_tried == 1 && !stats.retried {
            if let Some(peer) = ranked.first().map(|r| r.username.as_str()) {
                reliable_peers.record(artist, peer);
            }
        } else {
            reliable_peers.evict(artist);
        }
    }
```

- [ ] **Step 5: Create and thread `ReliablePeers` at the entry points**

In `src/runner.rs` auto loop (around the `let semaphore = …` / `let targets_vec …` area, before building `futures_vec`), add:

```rust
    let reliable_peers = crate::reliable::ReliablePeers::new();
```

and inside the async closure pass `&reliable_peers` after `config`:

```rust
                let result = process_album(
                    client,
                    &artist,
                    Some(&album),
                    config,
                    &reliable_peers,
                    db,
                    staging_dir,
                    progress.as_deref(),
                    Some(&cancel),
                    Some(library_track_count),
                    Some(library_path),
                )
                .await;
```

In `src/main.rs` (manual-mode and batch-mode `process_album` call sites, both around the `seakarr::runner::process_album(` occurrences), create the map once before the loop and pass `&reliable_peers`:

```rust
    let reliable_peers = seakarr::reliable::ReliablePeers::new();
```

and add `&reliable_peers,` after `config,` in each call.

- [ ] **Step 6: Update every `process_album` test call site in `src/runner.rs`**

There are 14 `process_album(` call sites (mostly in `#[cfg(test)]`). Add `&crate::reliable::ReliablePeers::new(),` immediately after the `config,` argument in each. (Do not bind it to a variable unless the test needs to inspect it — a fresh instance per call is fine.)

- [ ] **Step 7: Compile + run the full suite**

Run: `cargo test --workspace`
Expected: PASS (all tests, including existing process_album tests now passing `&ReliablePeers::new()`).

- [ ] **Step 8: Commit**

```bash
git add src/runner.rs src/main.rs
git commit -m "feat(runner): prefer reliable peer per artist and record/evict from download outcome"
```

---

## Task 6: README documentation

**Files:**
- Modify: `README.md`

- [ ] **Step 1: Add a feature bullet**

In the "Features" section, add a bullet near the quality-filtering / search items:

```markdown
- **Reliable-peer preference** — when an album downloads cleanly (first candidate, no retries), the
  winning peer is remembered and preferred for the next album by the same artist, biasing downloads
  toward proven-good sources. Controlled by `search.prefer_reliable_peer` (default `true`).
```

- [ ] **Step 2: Add a config note**

In the "Config-driven" paragraph or a config section, note:

```markdown
`search.prefer_reliable_peer` (default `true`) reuses a peer that served a clean download for the
same artist on the next search. Set it to `false` to always use the plain ranked search.
```

- [ ] **Step 3: Commit**

```bash
git add README.md
git commit -m "docs: document search.prefer_reliable_peer"
```

---

## Final verification

Run the full gates fresh:

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

All three must exit 0 before the change is considered done.

---

## Self-review notes

- **Spec coverage:** toggle (Task 1), map (Task 2), reliability signal (Task 3), promotion (Task 4), record/evict + logging + threading (Task 5), docs (Task 6). All spec sections map to a task.
- **Type consistency:** `ReliablePeers::record/get/evict` (Task 2) match the calls in Task 5; `DownloadStats { candidates_tried, retried }` (Task 3) matches Task 5's `stats.candidates_tried` / `stats.retried`; `promote_peer(results: Vec<SearchResult>, peer: &str)` (Task 4) matches Task 5's `search::promote_peer(ranked, peer)`.
- **Reliability rule (explicit):** record on `candidates_tried == 1 && !retried` (first candidate served cleanly); otherwise evict. This realizes "no retries and no failures → reliable" and handles "peer absent → someone else served cleanly → record the new peer".
