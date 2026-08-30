# Peer reputation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use sub-agents (recommended) to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Rank Soulseek search results using a SQLite-persisted per-peer reputation (measured speed + reliability), replacing the per-artist `prefer_reliable_peer` map.

**Architecture:** Reuse the existing `peer_reputation` table and `Database::update_peer_reputation`/`get_preferred_peers` (already defined, never wired in). Surface each track's final smoothed speed out of the download path, record per-track success/speed after each album download, and feed a `HashMap<username, PeerReputation>` into `rank_candidates` which blends advertised speed with measured speed and applies a bounded reliability factor.

**Tech Stack:** Rust, tokio, rusqlite, serde/serde_yaml, tracing. No new dependencies.

**Unit convention (lock in):** `SearchResult.speed` is **bytes/sec** (advertised). The DB stores `avg_speed_kbps` in **kbps** (`bytes/sec / 1024.0`). All blends convert measured kbps → bytes/sec (`* 1024.0`) before mixing with advertised speed.

---

## File map

- **Create** nothing new. Modify:
  - `src/db.rs` — add `get_reputation_map()`.
  - `src/config.rs` — rename `SearchConfig.prefer_reliable_peer` → `peer_reputation` (default true) + template + tests.
  - `src/download.rs` — `TrackRecord`; `download_once`/`download_file` return `(PathBuf, speed_kbps)`; `download_album` fills per-track records into `DownloadStats`; drop the old per-album stats fields.
  - `src/filter.rs` — `rank_candidates` gains a reputation lookup and blends speed + reliability factor.
  - `src/runner.rs` — remove per-artist logic; load reputation map; record per-track after download; signature drops `&ReliablePeers`.
  - `src/main.rs` — drop `ReliablePeers` threading.
  - `tests/pipeline_test.rs` — drop `ReliablePeers` args.
  - `README.md` — feature + config row.
- **Delete** `src/reliable.rs`; delete `search::promote_peer` (+ its tests); delete `src/lib.rs` `pub mod reliable;`.

---

## Task 1: DB — `get_reputation_map`

**Files:**
- Modify: `src/db.rs`

- [ ] **Step 1: Write the failing test**

In `src/db.rs` `#[cfg(test)] mod tests`, add (next to `test_peer_reputation_upsert`):

```rust
#[test]
fn test_get_reputation_map_returns_indexed_peers() {
    let db = test_db();
    db.update_peer_reputation("alice", 500.0, true).unwrap();
    db.update_peer_reputation("alice", 700.0, true).unwrap();
    db.update_peer_reputation("bob", 300.0, false).unwrap();

    let map = db.get_reputation_map().unwrap();
    assert_eq!(map.len(), 2);
    assert_eq!(map["alice"].total_downloads, 2);
    assert_eq!(map["alice"].successful, 2);
    assert_eq!(map["alice"].avg_speed_kbps, 600.0); // (500 + 700) / 2
    assert_eq!(map["bob"].total_downloads, 1);
    assert_eq!(map["bob"].successful, 0);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p seakarr db::tests::test_get_reputation_map_returns_indexed_peers`
Expected: FAIL (compile error — `get_reputation_map` not found).

- [ ] **Step 3: Implement**

In `src/db.rs`, add after `get_preferred_peers`:

```rust
pub fn get_reputation_map(&self) -> Result<std::collections::HashMap<String, PeerReputation>> {
    let mut stmt = self.conn.prepare(
        "SELECT username, total_downloads, successful, avg_speed_kbps, preferred
         FROM peer_reputation",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            PeerReputation {
                username: row.get(0)?,
                total_downloads: row.get::<_, u32>(1)?,
                successful: row.get::<_, u32>(2)?,
                avg_speed_kbps: row.get(3)?,
                preferred: row.get::<_, bool>(4)?,
            },
        ))
    })?;
    let mut map = std::collections::HashMap::new();
    for row in rows {
        let (name, rep) = row?;
        map.insert(name, rep);
    }
    Ok(map)
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p seakarr db::tests::test_get_reputation_map_returns_indexed_peers`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/db.rs
git commit -m "feat(db): add get_reputation_map for indexed peer reputation lookups"
```

---

## Task 2: Ranking — blend speed + reliability factor

**Files:**
- Modify: `src/filter.rs` (`rank_candidates` + tests)
- Modify: `src/runner.rs` (the single `rank_candidates` call site — pass an empty map for now)

- [ ] **Step 1: Write the failing test**

In `src/filter.rs` `#[cfg(test)] mod tests`, add:

```rust
fn rep(username: &str, total: u32, successful: u32, avg_kbps: f64) -> crate::db::PeerReputation {
    crate::db::PeerReputation {
        username: username.to_string(),
        total_downloads: total,
        successful,
        avg_speed_kbps: avg_kbps,
        preferred: false,
    }
}

#[test]
fn rank_prefers_measured_fast_reliable_peer() {
    // Advertised speeds: slow A (1 MB/s), fast B (10 MB/s). A has a strong
    // measured record (50 MB/s + 100% success); B is unknown.
    let mut results = vec![
        crate::client::SearchResult { username: "A".into(), speed: 1_000_000, slots: 1, files: vec![] },
        crate::client::SearchResult { username: "B".into(), speed: 10_000_000, slots: 1, files: vec![] },
    ];
    let mut map = std::collections::HashMap::new();
    map.insert("A".to_string(), rep("A", 20, 20, 50_000.0)); // 50 MB/s, 100% success

    results = rank_candidates(&results, &FilterConfig::default(), None, &map);

    assert_eq!(results[0].username, "A", "measured-reliable peer A must outrank faster-but-unknown B");
}

#[test]
fn rank_demotes_error_prone_peer() {
    // A and B identical advertised speed; A has 100% failure history.
    let mut results = vec![
        crate::client::SearchResult { username: "A".into(), speed: 5_000_000, slots: 1, files: vec![] },
        crate::client::SearchResult { username: "B".into(), speed: 5_000_000, slots: 1, files: vec![] },
    ];
    let mut map = std::collections::HashMap::new();
    map.insert("A".to_string(), rep("A", 10, 0, 0.0)); // 100% failure
    map.insert("B".to_string(), rep("B", 10, 10, 5_000.0));

    results = rank_candidates(&results, &FilterConfig::default(), None, &map);

    assert_eq!(results[0].username, "B", "error-prone peer A must rank below clean B");
}

#[test]
fn rank_unknown_peer_is_neutral() {
    let results = vec![
        crate::client::SearchResult { username: "A".into(), speed: 5_000_000, slots: 1, files: vec![] },
    ];
    let map = std::collections::HashMap::new();
    let ranked = rank_candidates(&results, &FilterConfig::default(), None, &map);
    assert_eq!(ranked[0].username, "A", "unknown peer must still be returned");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p seakarr filter::tests::rank_`
Expected: FAIL (compile error — `rank_candidates` takes 3 args, `PeerReputation` fields missing, `FilterConfig::default()` may not exist).

- [ ] **Step 3: Implement the blend + factor**

Change `rank_candidates` signature to:

```rust
pub fn rank_candidates(
    results: &[SearchResult],
    config: &FilterConfig,
    album: Option<&str>,
    reputation: &std::collections::HashMap<String, crate::db::PeerReputation>,
) -> Vec<SearchResult> {
```

Inside the `.map(|r| { ... })`, replace the `let speed_score = r.speed as f64;` line with:

```rust
            let advertised_bps = r.speed as f64;
            // Blend toward the measured speed as the peer's history grows.
            let effective_speed = match reputation.get(&r.username) {
                Some(rep) if rep.total_downloads > 0 => {
                    let measured_bps = rep.avg_speed_kbps * 1024.0;
                    let w = rep.total_downloads as f64 / (rep.total_downloads as f64 + 3.0);
                    advertised_bps * (1.0 - w) + measured_bps * w
                }
                _ => advertised_bps,
            };
            // Laplace-smoothed success rate -> bounded factor in [0.7, 1.3].
            let reliability_factor = match reputation.get(&r.username) {
                Some(rep) => {
                    let r = (rep.successful as f64 + 1.5) / (rep.total_downloads as f64 + 3.0);
                    0.7 + 0.6 * r
                }
                None => 1.0,
            };
            let speed_score = effective_speed;
```

And change the score line to include the factor:

```rust
            let score = speed_score * slot_bonus * bitrate_bonus * album_bonus * reliability_factor;
```

Note: the existing `FilterConfig::default()` may not be derivable if `FilterConfig` lacks `Default`. If so, in the tests build a `FilterConfig` via `serde_yaml::from_str("{}")` or use the existing test helper in `filter.rs` tests (there is already a `make_file` helper). Adjust the test construction to match what `filter.rs` tests already use.

- [ ] **Step 4: Update the runner caller (compile only)**

In `src/runner.rs`, the `rank_candidates` call is:

```rust
    let ranked = filter::rank_candidates(&filtered, &config.filters, rank_album);
```

Change to (temporary — real map lands in Task 4):

```rust
    let ranked = filter::rank_candidates(&filtered, &config.filters, rank_album, &std::collections::HashMap::new());
```

Also update the other `filter::rank_candidates` call sites in `src/runner.rs` tests (grep `rank_candidates`) the same way.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p seakarr filter::tests::rank_ && cargo test -p seakarr runner::`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/filter.rs src/runner.rs
git commit -m "feat(filter): blend measured speed and reliability into search ranking"
```

---

## Task 3: Download — surface per-track measured speed

**Files:**
- Modify: `src/download.rs` (+ update `src/runner.rs` caller)

- [ ] **Step 1: Add the `TrackRecord` type and repurpose `DownloadStats`**

In `src/download.rs`, replace the current `DownloadStats` struct with:

```rust
/// A single track's outcome, for peer-reputation recording.
#[derive(Debug, Default, Clone)]
pub struct TrackRecord {
    pub username: String,
    /// Measured transfer speed in kbps (0 when the track failed).
    pub speed_kbps: f64,
    pub success: bool,
}

/// Per-track outcomes collected during an album download, consumed by the
/// runner to update peer reputation.
#[derive(Debug, Default, Clone)]
pub struct DownloadStats {
    pub tracks: Vec<TrackRecord>,
}
```

- [ ] **Step 2: `download_once` returns the final measured speed**

Change `download_once`'s signature return type from `Result<PathBuf>` to `Result<(PathBuf, f64)>`, and at its success return (`Ok(dest)` inside the `DownloadStatus::Completed` arm) return the final speed:

```rust
                return Ok((dest, speed_ema.unwrap_or(0.0) / 1024.0)); // kbps
```

Every other `Err(...)` path stays an `Err`. (The `speed_ema` is already computed in this function's status loop.)

- [ ] **Step 3: `download_file` propagates speed**

Change `download_file` return type to `Result<(PathBuf, f64)>`. In its loop, the `Ok(path) => return Ok(path)` success arm becomes:

```rust
            Ok((path, speed_kbps)) => return Ok((path, speed_kbps)),
```

- [ ] **Step 4: `download_album` records per-track outcomes**

Change `download_album`'s file loop so that:

- on success, push a `TrackRecord { username: candidate.username.clone(), speed_kbps, success: true }` to `stats.tracks`;
- on failure (the `Err(e)` arm), push `TrackRecord { username: candidate.username.clone(), speed_kbps: 0.0, success: false }` then `failed = true; break;` as today.

Remove the now-unused `candidates_tried`, `retried`, and `first_candidate_transfer_failed` locals and their `stats.*` assignments (both the `return Ok(downloaded)` success path and the final `Err` path now only need to return `downloaded`; the per-track records already live in `stats.tracks`).

- [ ] **Step 5: Update every `download_file` / `download_album` test call site**

`download_file(...)` test call sites are inside `src/download.rs` tests (grep `download_file(`). Each previously `.await.unwrap()` on a `Result<PathBuf>` now yields `(PathBuf, f64)`; adjust assertions to use `result.0` (the path) and ignore `result.1`. `download_album(...)` test call sites (grep `download_album(`) still take `&mut stats` — no signature change there, only the `stats` struct shape changed; update any test that reads `stats.candidates_tried`/`retried`/`first_candidate_transfer_failed` to read `stats.tracks` instead.

- [ ] **Step 6: Update the runner caller**

In `src/runner.rs`, the download call currently binds `let downloaded = match download::download_album(..., &mut stats).await { Ok(files) => files, ... }`. This still works (`download_album` returns `Result<Vec<PathBuf>>`; only `stats`'s fields changed). No change needed here yet — Task 4 consumes `stats.tracks`.

- [ ] **Step 7: Compile + run download tests**

Run: `cargo test -p seakarr download::`
Expected: PASS after fixing each call site.

- [ ] **Step 8: Commit**

```bash
git add src/download.rs
git commit -m "feat(download): surface per-track measured speed for reputation"
```

---

## Task 4: Runner — wire reputation (read + write), remove per-artist logic

**Files:**
- Modify: `src/runner.rs`, `src/main.rs`, `tests/pipeline_test.rs`

- [ ] **Step 1: Remove the per-artist plumbing**

In `src/runner.rs`:
- Delete the `preferred_peer` lookup block (the `// Prefer a previously-reliable peer ...` + `if let Some(peer) = &preferred_peer { tracing::info!(...) }` before the search), the `promote_peer` reorder (`let ranked = if let Some(peer) = preferred_peer.as_deref() { search::promote_peer(...) } else { ranked };`), and the `winner`/`preferred_was_first` block.
- Delete every `reliable_peers.record/evict/evict_if` block (the download `Err` arm guard, the incomplete gate, and both success-path record/evict blocks).
- Remove the `reliable_peers: &crate::reliable::ReliablePeers` parameter from `process_album`'s signature.

- [ ] **Step 2: Read reputation before ranking**

In `process_album`, replace the `let ranked = filter::rank_candidates(...)` line and its surrounding removal with:

```rust
    let reputation = if config.search.peer_reputation {
        db.get_reputation_map().unwrap_or_default()
    } else {
        std::collections::HashMap::new()
    };
    let ranked = filter::rank_candidates(&filtered, &config.filters, rank_album, &reputation);
```

(The `db.get_reputation_map()` returns `Result<HashMap<...>>`; `.unwrap_or_default()` needs `HashMap::default()` which exists. On error we proceed with an empty map — reputation never blocks a search.)

- [ ] **Step 3: Record per-track after download**

Immediately after the `let downloaded = match download::download_album(...) { ... };` succeeds (in the `Ok` path), add:

```rust
    if config.search.peer_reputation {
        for track in &stats.tracks {
            if let Err(e) = db.update_peer_reputation(&track.username, track.speed_kbps, track.success) {
                tracing::warn!("failed to record peer reputation for {}: {e}", track.username);
            }
        }
    }
```

(Record even when the album later fails at the organize/upgrade gate — the track outcomes are still valid reputation signal. Place this right after the `Ok(files) => files` arm resolves, before the library-upgrade block.)

- [ ] **Step 4: Update entry points and tests to drop `&ReliablePeers`**

- `src/runner.rs` auto loop: remove `let reliable_peers = crate::reliable::ReliablePeers::new();` and the `&reliable_peers,` argument.
- `src/main.rs` (manual + batch): remove `let reliable_peers = seakarr::reliable::ReliablePeers::new();` and the `&reliable_peers,` argument.
- `tests/pipeline_test.rs`: remove the `&seakarr::reliable::ReliablePeers::new(),` argument from both `process_album` calls.
- Every `process_album(` test call site in `src/runner.rs` (grep `process_album(`): remove the `&crate::reliable::ReliablePeers::new(),` argument. Also remove the now-dead `test_reliable_peer_is_preferred_for_second_album_of_same_artist`, `test_failed_preferred_peer_is_evicted`, and `test_cancellation_does_not_evict_preferred_peer` tests (they exercise the removed per-artist feature).

- [ ] **Step 5: Add an end-to-end reputation test**

In `src/runner.rs` tests, add:

```rust
#[tokio::test]
async fn test_peer_reputation_recorded_and_used_in_ranking() {
    let client = Arc::new(MockClient::new());
    let peer_a = MockClient::mock_search_result(
        "peerA", 100_000, 1,
        vec![(r"Test Artist\Album One\01.flac", 10_000_000, 900)],
    );
    {
        let mut by_query = client.search_results_by_query.lock().unwrap();
        by_query.insert("Test Artist Album One".to_string(), vec![peer_a]);
    }
    let config = make_test_config();
    let db = Database::open_in_memory().unwrap();
    let staging = TempDir::new().unwrap();

    // First album: downloads cleanly -> records peerA as reliable + fast.
    process_album(
        client.as_ref() as &dyn crate::client::SoulseekClient,
        "Test Artist", Some("Album One"), &config, &db, staging.path(),
        None, None, None, None,
    ).await.unwrap();

    let rep = db.get_reputation_map().unwrap();
    assert!(rep.contains_key("peerA"));
    assert_eq!(rep["peerA"].successful, 1);
}
```

(This asserts recording works end-to-end. It uses the post-refactor `process_album` signature — no `reliable_peers` argument.)

- [ ] **Step 6: Compile + full suite**

Run: `cargo test --workspace`
Expected: PASS after all call-site cleanups.

- [ ] **Step 7: Commit**

```bash
git add src/runner.rs src/main.rs tests/pipeline_test.rs
git commit -m "feat(runner): wire peer reputation recording and ranking"
```

---

## Task 5: Delete the per-artist map + `promote_peer`

**Files:**
- Delete: `src/reliable.rs`
- Modify: `src/lib.rs` (remove `pub mod reliable;`), `src/search.rs` (remove `promote_peer` + its tests), `src/runner.rs` (remove the `use` if any)

- [ ] **Step 1: Delete the files/symbols**

```bash
git rm src/reliable.rs
```

In `src/lib.rs`, remove the line `pub mod reliable;`.

In `src/search.rs`, delete the `promote_peer` function and its two `promote_peer_*` tests (they are now dead — the runner no longer calls it).

- [ ] **Step 2: Compile + full suite**

Run: `cargo test --workspace`
Expected: PASS (nothing references the deleted module).

- [ ] **Step 3: Commit**

```bash
git add -A
git commit -m "refactor: remove per-artist ReliablePeers map and promote_peer"
```

---

## Task 6: Config + docs

**Files:**
- Modify: `src/config.rs` (rename field + template + tests), `README.md`

- [ ] **Step 1: Rename the config field**

In `src/config.rs` `SearchConfig`, rename:

```rust
    #[serde(default = "default_true")]
    pub prefer_reliable_peer: bool,
```
to:
```rust
    #[serde(default = "default_true")]
    pub peer_reputation: bool,
```

Update the `Config::default()` literal and the config test(s) that reference `prefer_reliable_peer` (grep it) to `peer_reputation`. Update the YAML template line `prefer_reliable_peer: true` → `peer_reputation: true`.

- [ ] **Step 2: Verify config tests**

Run: `cargo test -p seakarr config::`
Expected: PASS.

- [ ] **Step 3: Update README**

In `README.md`, replace the "Reliable-peer preference" feature bullet and the `prefer_reliable_peer` config row with:

```markdown
- **Peer reputation** — remembers each peer's measured download speed and success rate (in SQLite), and ranks
  search results by a blend of advertised and measured speed plus a reliability factor, so fast, reliable peers
  are preferred and error-prone or throttling peers are demoted — regardless of what you search for. Controlled
  by `search.peer_reputation` (default `true`).
```

and the config-table row:

```markdown
| `peer_reputation` | Blend measured speed + reliability into search ranking. Set to `false` to rank by advertised speed only. | `true` |
```

- [ ] **Step 4: Commit**

```bash
git add src/config.rs README.md
git commit -m "feat(config): add search.peer_reputation (replaces prefer_reliable_peer)"
```

---

## Final verification

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

All three must exit 0. Confirm `git status` shows no stray files (no `src/reliable.rs`, no coverage artifacts).

---

## Self-review notes

- **Spec coverage:** reuse DB layer (Task 1), ranking blend+factor (Task 2), per-track speed surfacing (Task 3), runner read+write + removal (Task 4), delete per-artist map + promote_peer (Task 5), config rename + docs (Task 6). Every spec section maps to a task.
- **Type consistency:** `PeerReputation` fields (`total_downloads`, `successful`, `avg_speed_kbps`, `preferred`) as defined in `db.rs` are used verbatim in `filter.rs` tests and logic; `TrackRecord { username, speed_kbps, success }` (Task 3) is consumed by `db.update_peer_reputation(username, speed_kbps, success)` (Task 4); `get_reputation_map()` (Task 1) is consumed as the `&HashMap<String, PeerReputation>` in `rank_candidates` (Task 2) and `process_album` (Task 4); config key `peer_reputation` (Task 6) is read in Task 4.
- **Units:** advertised `speed` (bytes/sec) vs stored `avg_speed_kbps` (kbps) — converted consistently via `* 1024.0` in the blend and `/ 1024.0` on record.
- **Placeholders:** none — all formulas, signatures, and commands are concrete.
