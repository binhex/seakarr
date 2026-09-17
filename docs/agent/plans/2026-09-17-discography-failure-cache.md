<!-- markdownlint-disable MD013 -->
# Discography failure cache Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use sub-agents (recommended) to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stop discover and artist-only runs from asking MusicBrainz about artists that a previous run already proved unresolvable, by persisting those resolution failures in SQLite with their own expiry.

**Architecture:** A new `discography_failures` table keyed on the normalised artist key records only `Unresolved` failures. `discover_artist_albums_at` consults it after the success-cache check and, when a row is fresh and not bypassed, replays the failure as `DiscoveryOutcome::LegacyFallback { from_cache: true }` without touching the provider. Success and failure rows are mutually exclusive by construction, so a stale-but-usable success row is never shadowed. A pinned MBID or an explicit `--artist` bypasses the cache.

**Tech Stack:** Rust 2021, rusqlite (bundled SQLite), tokio, serde/serde_yaml, tracing. Tests are `#[tokio::test]` inside each module's `#[cfg(test)] mod tests`.

**Spec:** `docs/agent/specs/2026-09-17-discography-failure-cache-design.md`

---

## Scope check

The spec covers a single subsystem: discography resolution caching. It does not
bundle independent subsystems, so it stays one plan. Everything touches the
existing cache path in `src/discography/mod.rs` plus its storage, config
surface, and one reporting line — no new module is warranted.

## File structure

| File | Responsibility | Change |
| --- | --- | --- |
| `src/db.rs` | Persistence. Owns the schema and the row-level helpers. | Modify: new table, entry struct, three helpers, tests |
| `src/config.rs` | Configuration surface and validation. | Modify: new key, default fn, `Default` impl, overflow guard, tests, `sample_yaml` |
| `src/discography/mod.rs` | Resolution policy. Owns the ordering rules and the replay. | Modify: `FailureCacheUse`, `from_cache` field, read/replay helper, record helper, tests |
| `src/runner.rs` | Wiring and reporting. | Modify: two call sites, `cached_failures` handling, test provider counter, tests |
| `src/discover.rs` | Run summary text. | Modify: `DiscoverCounters` field, `discover_notices` line, tests |
| `README.md` | User documentation. | Modify: `discography` config table row and discover prose |

Nothing is created. No new module, no new binary, no migration file.

**Ordering constraint:** Task 3 adds a field to `DiscoveryOutcome::LegacyFallback`,
which breaks every constructor and exhaustive match until updated. Task 3
therefore does that enum change and fixes all sites in one compile unit.

---

### Task 1: Persistence for recorded failures

**Files:**

- Modify: `src/db.rs` (schema bootstrap around lines 185-193, domain structs after line 55, helpers after line 449, tests around line 511)
- Test: `src/db.rs` (`#[cfg(test)] mod tests`)

- [ ] **Step 1: Write the failing test**

Add to `src/db.rs` in the `mod tests` block, after the existing
`discography_cache_round_trips_and_replaces_atomically` test:

```rust
    #[test]
    fn discography_failure_round_trips_replaces_and_deletes() {
        let db = test_db();
        db.migrate().unwrap();

        assert!(db.get_discography_failure("artist").unwrap().is_none());

        db.upsert_discography_failure(&DiscographyFailureEntry {
            artist_key: "artist".to_string(),
            failure_kind: "unresolved".to_string(),
            reason: "no candidate matches".to_string(),
            recorded_at: 1_000,
        })
        .unwrap();

        // The key matches case-insensitively, exactly as the success cache does.
        let stored = db.get_discography_failure("ARTIST").unwrap().unwrap();
        assert_eq!(stored.artist_key, "artist");
        assert_eq!(stored.failure_kind, "unresolved");
        assert_eq!(stored.reason, "no candidate matches");
        assert_eq!(stored.recorded_at, 1_000);

        // A second write replaces the row rather than duplicating it.
        db.upsert_discography_failure(&DiscographyFailureEntry {
            artist_key: "artist".to_string(),
            failure_kind: "unresolved".to_string(),
            reason: "second reason".to_string(),
            recorded_at: 2_000,
        })
        .unwrap();
        let stored = db.get_discography_failure("artist").unwrap().unwrap();
        assert_eq!(stored.reason, "second reason");
        assert_eq!(stored.recorded_at, 2_000);

        assert!(db.delete_discography_failure("artist").unwrap());
        assert!(db.get_discography_failure("artist").unwrap().is_none());
        assert!(
            !db.delete_discography_failure("artist").unwrap(),
            "deleting an absent row reports false"
        );
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --lib db::tests::discography_failure_round_trips_replaces_and_deletes`

Expected: FAIL to compile, with `no method named get_discography_failure` and
`cannot find struct DiscographyFailureEntry`.

- [ ] **Step 3: Add the entry struct**

In `src/db.rs`, immediately after the `DiscographyCacheEntry` struct (which ends
around line 55), add:

```rust
/// A recorded artist-resolution failure, so a later run can skip the lookup
/// instead of asking MusicBrainz the same unanswerable question again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscographyFailureEntry {
    pub artist_key: String,
    /// Why the lookup failed. Only `"unresolved"` is written today; the column
    /// exists so the row is self-describing if a second kind is ever cached.
    pub failure_kind: String,
    /// The resolver's own message, replayed verbatim on a cache hit.
    pub reason: String,
    /// Unix seconds, the same clock as `DiscographyCacheEntry::fetched_at`.
    pub recorded_at: i64,
}
```

- [ ] **Step 4: Add the table to the schema bootstrap**

In `src/db.rs`, inside the existing `execute_batch` in `migrate()`, immediately
after the `CREATE TABLE IF NOT EXISTS discography_cache (...)` statement, add:

```sql
            CREATE TABLE IF NOT EXISTS discography_failures (
                artist_key   TEXT PRIMARY KEY COLLATE NOCASE,
                failure_kind TEXT NOT NULL,
                reason       TEXT NOT NULL,
                recorded_at  INTEGER NOT NULL
            );
```

- [ ] **Step 5: Add the three helpers**

In `src/db.rs`, in the `// ── Discography cache ──` section, after
`delete_discography_cache` and before the closing brace of the impl block, add:

```rust
    /// Read the recorded resolution failure for `artist_key`, if there is one.
    pub fn get_discography_failure(
        &self,
        artist_key: &str,
    ) -> Result<Option<DiscographyFailureEntry>> {
        self.conn
            .query_row(
                "SELECT artist_key, failure_kind, reason, recorded_at
                 FROM discography_failures WHERE artist_key = ?1 COLLATE NOCASE",
                params![artist_key],
                |row| {
                    Ok(DiscographyFailureEntry {
                        artist_key: row.get(0)?,
                        failure_kind: row.get(1)?,
                        reason: row.get(2)?,
                        recorded_at: row.get(3)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    /// Record a resolution failure for `artist_key`, replacing any earlier one.
    pub fn upsert_discography_failure(&self, entry: &DiscographyFailureEntry) -> Result<()> {
        self.conn.execute(
            "INSERT INTO discography_failures
             (artist_key, failure_kind, reason, recorded_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(artist_key) DO UPDATE SET
               failure_kind = excluded.failure_kind,
               reason = excluded.reason,
               recorded_at = excluded.recorded_at",
            params![
                entry.artist_key,
                entry.failure_kind,
                entry.reason,
                entry.recorded_at,
            ],
        )?;
        Ok(())
    }

    /// Delete the recorded failure for `artist_key`; true when a row was removed.
    pub fn delete_discography_failure(&self, artist_key: &str) -> Result<bool> {
        Ok(self.conn.execute(
            "DELETE FROM discography_failures WHERE artist_key = ?1 COLLATE NOCASE",
            params![artist_key],
        )? > 0)
    }
```

- [ ] **Step 6: Extend the schema assertion**

In `src/db.rs`, in the test that asserts the created table list (the block
ending with `assert!(tables.contains(&"discography_cache".to_string()));` around
line 511), add after that line:

```rust
        assert!(tables.contains(&"discography_failures".to_string()));
```

- [ ] **Step 7: Run the tests to verify they pass**

Run: `cargo test --lib db::tests`

Expected: PASS, including
`discography_failure_round_trips_replaces_and_deletes` and the table-list test.

- [ ] **Step 8: Commit**

```bash
git add src/db.rs
git commit -m "feat(db): add discography_failures table and helpers"
```

---

### Task 2: Configuration key

**Files:**

- Modify: `src/config.rs` (`DiscographyConfig` around lines 243-271, validation around line 864, `sample_yaml` around line 1296, tests around line 2700)
- Modify: `README.md` (`### discography` config table)
- Test: `src/config.rs`

- [ ] **Step 1: Write the failing tests**

Add to `src/config.rs` in `mod tests`:

```rust
    #[test]
    fn discography_failure_cache_days_round_trips_through_yaml() {
        let mut config = Config::default();
        config.discography.failure_cache_days = 3;

        let yaml = serde_yaml::to_string(&config).unwrap();
        assert!(yaml.contains("failure_cache_days: 3"), "got {yaml}");

        let parsed: Config = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed.discography.failure_cache_days, 3);
    }

    #[test]
    fn discography_failure_cache_days_too_large_is_rejected() {
        let mut config = Config::default();
        config.discography.failure_cache_days = u64::MAX;
        let error = config.validate_non_credential_constraints().unwrap_err();
        assert!(
            error
                .to_string()
                .contains("discography.failure_cache_days is too large"),
            "got {error}"
        );
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib config::tests::discography_failure_cache`

Expected: FAIL to compile, with `no field failure_cache_days on DiscographyConfig`.

- [ ] **Step 3: Add the config field and default**

In `src/config.rs`, in `DiscographyConfig` (around lines 243-252), after
`pub cache_days: u64,` add:

```rust
    /// How long a recorded artist-resolution failure is honoured. `0` disables
    /// the failure cache entirely: nothing is written and nothing is replayed.
    #[serde(default = "default_discography_failure_cache_days")]
    pub failure_cache_days: u64,
```

And after `default_discography_cache_days()` add:

```rust
fn default_discography_failure_cache_days() -> u64 {
    7
}
```

- [ ] **Step 4: Add it to the `Default` impl**

In `src/config.rs`, in the manual `Default` impl for `DiscographyConfig`
(around lines 264-269), add the field alongside the others:

```rust
            failure_cache_days: default_discography_failure_cache_days(),
```

- [ ] **Step 5: Add the overflow guard**

The TTL arithmetic multiplies days by 86 400, so a value that cannot be
represented must be rejected rather than silently saturated. In
`validate_non_credential_constraints`, immediately after the existing
`discography.cache_days is too large` check (around line 868), add:

```rust
        if self.discography.failure_cache_days > i64::MAX as u64 / 86_400 {
            return Err(SeakarrError::Config(
                "discography.failure_cache_days is too large".into(),
            ));
        }
```

- [ ] **Step 6: Extend the defaults test and the sample YAML**

In the existing test `discography_defaults_are_authoritative_studio_albums`
(around line 2701), after `assert_eq!(config.discography.cache_days, 30);` add:

```rust
        assert_eq!(config.discography.failure_cache_days, 7);
```

In `sample_yaml()` (around line 1296), under the `discography:` block, after
`cache_days: 30` add:

```yaml
  failure_cache_days: 7
```

- [ ] **Step 7: Run the tests to verify they pass**

Run: `cargo test --lib config::tests`

Expected: PASS. If any round-trip or merge test fails on the new key, update
that fixture the same way — the key has a serde default, so a file that omits it
must still load.

- [ ] **Step 8: Document the key in the README config table**

In `README.md`, in the `### discography` section's table, add a row after the
`cache_days` row:

```markdown
| `failure_cache_days` | How long an artist that MusicBrainz could not resolve is remembered, so a later run skips the lookup instead of asking again. `0` disables the failure cache: nothing is recorded and nothing is skipped. Only resolution failures are remembered — a MusicBrainz outage is always retried. | `7` |
```

- [ ] **Step 9: Commit**

```bash
git add src/config.rs README.md
git commit -m "feat(config): add discography.failure_cache_days"
```

---

### Task 3: Replay a recorded failure instead of calling the provider

**Files:**

- Modify: `src/discography/mod.rs` (`DiscoveryOutcome` around line 519, `discover_artist_albums`/`_at` around lines 536-560, helpers near `cache_is_fresh` around line 666, tests)
- Modify: `src/runner.rs` (call sites at 1280 and 1634, match at 1356)
- Test: `src/discography/mod.rs`

- [ ] **Step 1: Add the bypass enum and the outcome field**

In `src/discography/mod.rs`, above `DiscoveryOutcome`, add:

```rust
/// Whether a caller will accept a recorded resolution failure in place of a
/// provider call. Named rather than a bare `bool` so the two call sites read as
/// a decision, following `ArtistComponent` in `organizer.rs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureCacheUse {
    /// Replay a fresh recorded failure for this artist.
    Honour,
    /// Ignore any recorded failure: the user asked for this artist by name, or
    /// pinned its MBID, so the lookup runs.
    Bypass,
}
```

In `DiscoveryOutcome::LegacyFallback`, add the field:

```rust
    LegacyFallback {
        reason: String,
        kind: DiscoveryFailure,
        /// True when this failure was replayed from the failure cache rather
        /// than observed live, so the caller can report the two separately.
        from_cache: bool,
    },
```

- [ ] **Step 2: Thread the parameter through the entry points**

Change the two signatures and the delegating call so the bypass choice reaches
the resolution body:

```rust
pub async fn discover_artist_albums(
    provider: &dyn DiscographyProvider,
    db: &Database,
    artist: &str,
    config: &DiscographyConfig,
    failure_cache: FailureCacheUse,
) -> DiscoveryOutcome {
    discover_artist_albums_at(
        provider,
        db,
        artist,
        config,
        chrono::Utc::now().timestamp(),
        failure_cache,
    )
    .await
}
```

and `discover_artist_albums_at` gains, after its `now: i64` parameter:

```rust
    failure_cache: FailureCacheUse,
```

- [ ] **Step 3: Fix the fanned-out breakage from the new field**

Run: `cargo check --all-targets`

Expected: errors listing every `LegacyFallback` constructor and match that needs
the new field. Fix each:

- `stale_or_legacy` (the only production constructor, around line 723) passes
  `from_cache: false` — a live refresh failure is never a replay.
- `src/discography/mod.rs` test matches at 1382, 1851, 1958 and 1980: add
  `from_cache: false` where the pattern names fields, or keep `..`.
- `src/runner.rs:1356` uses `LegacyFallback { reason, .. }`, which still
  compiles; leave it.
- Both `src/runner.rs` call sites now need a fifth argument. Use `Bypass` at
  `run_artist_only_mode_with_provider` (line 1280) — that path is always a
  named artist — and `Honour` at the discover loop (line 1634) for now; Task 5
  refines that to `Bypass` when `--artist` was given.

Re-run `cargo check --all-targets` until it is clean.

- [ ] **Step 4: Write the failing tests**

Add to `src/discography/mod.rs` in `mod tests`, after the `cache_groups` helper:

```rust
    fn record_failure(db: &Database, artist_key: &str, reason: &str, recorded_at: i64) {
        db.upsert_discography_failure(&crate::db::DiscographyFailureEntry {
            artist_key: artist_key.to_string(),
            failure_kind: "unresolved".to_string(),
            reason: reason.to_string(),
            recorded_at,
        })
        .unwrap();
    }

    #[tokio::test]
    async fn fresh_recorded_failure_is_replayed_without_a_provider_call() {
        let db = Database::open_in_memory().unwrap();
        record_failure(&db, "unknown", "no candidate matches \"unknown\"", 1_000);
        let provider = FakeProvider::unresolvable_recording();

        let outcome = discover_artist_albums_at(
            &provider,
            &db,
            "Unknown",
            &DiscographyConfig::default(),
            1_100,
            FailureCacheUse::Honour,
        )
        .await;

        assert!(matches!(
            outcome,
            DiscoveryOutcome::LegacyFallback {
                ref reason,
                kind: DiscoveryFailure::Unresolved,
                from_cache: true,
            } if reason == "no candidate matches \"unknown\""
        ));
        assert_eq!(provider.total_calls(), 0, "a replay must not call the provider");
    }

    #[tokio::test]
    async fn recorded_failure_at_the_ttl_boundary_is_retried() {
        let db = Database::open_in_memory().unwrap();
        let config = DiscographyConfig::default();
        let ttl = config.failure_cache_days * 86_400;
        record_failure(&db, "unknown", "no candidate matches", 1_000);
        let provider = FakeProvider::unresolvable_recording();

        let outcome = discover_artist_albums_at(
            &provider,
            &db,
            "Unknown",
            &config,
            1_000 + ttl,
            FailureCacheUse::Honour,
        )
        .await;

        assert!(matches!(
            outcome,
            DiscoveryOutcome::LegacyFallback {
                from_cache: false,
                ..
            }
        ));
        assert!(provider.total_calls() > 0, "an expired row must retry");
    }

    #[tokio::test]
    async fn zero_failure_cache_days_retries_and_records_nothing() {
        let db = Database::open_in_memory().unwrap();
        let config = DiscographyConfig {
            failure_cache_days: 0,
            ..DiscographyConfig::default()
        };
        record_failure(&db, "unknown", "no candidate matches", 1_000);
        let provider = FakeProvider::unresolvable_recording();

        let outcome =
            discover_artist_albums_at(&provider, &db, "Unknown", &config, 1_100, FailureCacheUse::Honour)
                .await;

        assert!(matches!(
            outcome,
            DiscoveryOutcome::LegacyFallback {
                from_cache: false,
                ..
            }
        ));
        assert!(provider.total_calls() > 0);
    }

    #[tokio::test]
    async fn bypass_ignores_a_fresh_recorded_failure() {
        let db = Database::open_in_memory().unwrap();
        record_failure(&db, "unknown", "no candidate matches", 1_000);
        let provider = FakeProvider::unresolvable_recording();

        let outcome = discover_artist_albums_at(
            &provider,
            &db,
            "Unknown",
            &DiscographyConfig::default(),
            1_100,
            FailureCacheUse::Bypass,
        )
        .await;

        assert!(matches!(
            outcome,
            DiscoveryOutcome::LegacyFallback {
                from_cache: false,
                ..
            }
        ));
        assert!(provider.total_calls() > 0);
    }

    #[tokio::test]
    async fn a_pinned_mbid_bypasses_and_clears_a_recorded_failure() {
        let db = Database::open_in_memory().unwrap();
        record_failure(&db, "artist", "no candidate matches", 1_000);
        let provider = FakeProvider::unresolvable_recording();
        let config = DiscographyConfig {
            artist_mbids: std::collections::BTreeMap::from([(
                "Artist".to_string(),
                "11111111-1111-1111-1111-111111111111".to_string(),
            )]),
            ..DiscographyConfig::default()
        };

        discover_artist_albums_at(
            &provider,
            &db,
            "Artist",
            &config,
            1_100,
            FailureCacheUse::Honour,
        )
        .await;

        assert!(
            provider.total_calls() > 0,
            "a pinned MBID must reach MusicBrainz"
        );
        assert!(
            db.get_discography_failure("artist").unwrap().is_none(),
            "the recorded failure is obsolete once the MBID is pinned"
        );
    }

    #[tokio::test]
    async fn an_unknown_failure_kind_is_treated_as_a_miss() {
        let db = Database::open_in_memory().unwrap();
        db.upsert_discography_failure(&crate::db::DiscographyFailureEntry {
            artist_key: "unknown".to_string(),
            failure_kind: "something-new".to_string(),
            reason: "from a future version".to_string(),
            recorded_at: 1_000,
        })
        .unwrap();
        let provider = FakeProvider::unresolvable_recording();

        let outcome = discover_artist_albums_at(
            &provider,
            &db,
            "Unknown",
            &DiscographyConfig::default(),
            1_100,
            FailureCacheUse::Honour,
        )
        .await;

        assert!(matches!(
            outcome,
            DiscoveryOutcome::LegacyFallback {
                from_cache: false,
                ..
            }
        ));
        assert!(provider.total_calls() > 0);
    }
```

The tests need an unresolvable provider that counts calls. Add to the test
helper impl on `FakeProvider`, next to `failing`:

```rust
        /// An artist search that returns only non-matching names, so resolution
        /// fails as `DiscoveryFailure::Unresolved` rather than as an outage.
        fn unresolvable_recording() -> Self {
            Self {
                artist_responses: Mutex::new(VecDeque::from([Ok(vec![ArtistCandidate {
                    id: "22222222-2222-2222-2222-222222222222".to_string(),
                    name: "Somebody Else".to_string(),
                    score: None,
                }])])),
                ..Self::default()
            }
        }
```

- [ ] **Step 5: Run the tests to verify they fail**

Run: `cargo test --lib discography::tests::fresh_recorded_failure_is_replayed_without_a_provider_call discography::tests::recorded_failure_at_the_ttl_boundary_is_retried`

Expected: FAIL, because the replay does not exist yet — the recorded row is
ignored and the provider is called, so `total_calls()` is greater than 0 and the
outcome has `from_cache: false`.

- [ ] **Step 6: Implement the replay**

In `src/discography/mod.rs`, add the constant near the other cache constants:

```rust
/// The only failure kind written to the failure cache. Anything else is treated
/// as a miss, so a future kind cannot suppress a lookup it was not written for.
const FAILURE_KIND_UNRESOLVED: &str = "unresolved";
```

In `discover_artist_albums_at`, after the fresh-success check and before the
`let refresh = async { ... }` block, insert:

```rust
    if let Some(replayed) = replay_recorded_failure(
        db,
        &artist_key,
        config,
        now,
        failure_cache,
        configured_mbid,
    ) {
        return replayed;
    }
```

And add the helper next to `cache_is_fresh`:

```rust
/// Replay a recorded resolution failure instead of calling the provider.
///
/// Returns `None` when there is nothing to replay: the caller bypassed the
/// cache, the configured expiry is `0`, no row exists, the row is stale, or its
/// kind is unknown. Every one of those is a miss rather than an error, because a
/// cache must never be able to block a lookup it was not written for.
fn replay_recorded_failure(
    db: &Database,
    artist_key: &str,
    config: &DiscographyConfig,
    now: i64,
    use_cache: FailureCacheUse,
    configured_mbid: Option<&str>,
) -> Option<DiscoveryOutcome> {
    if configured_mbid.is_some() {
        // A pinned MBID makes resolution deterministic, so any recorded failure
        // is obsolete. Drop it and let the lookup run.
        if let Err(error) = db.delete_discography_failure(artist_key) {
            tracing::warn!("failed to clear discography failure row: {error}");
        }
        return None;
    }
    if use_cache == FailureCacheUse::Bypass || config.failure_cache_days == 0 {
        return None;
    }
    let entry = match db.get_discography_failure(artist_key) {
        Ok(Some(entry)) => entry,
        Ok(None) => return None,
        Err(error) => {
            tracing::warn!("failed to read discography failure cache: {error}");
            return None;
        }
    };
    if !cache_is_fresh(entry.recorded_at, config.failure_cache_days, now) {
        return None;
    }
    if entry.failure_kind != FAILURE_KIND_UNRESOLVED {
        return None;
    }
    let reason = if entry.reason.trim().is_empty() {
        "artist could not be resolved safely".to_string()
    } else {
        entry.reason
    };
    tracing::debug!(
        artist_key,
        recorded_at = entry.recorded_at,
        "skipping a recorded resolution failure"
    );
    Some(DiscoveryOutcome::LegacyFallback {
        reason,
        kind: DiscoveryFailure::Unresolved,
        from_cache: true,
    })
}
```

Extend the module's db import (line 8) to include the new type:

```rust
use crate::db::{Database, DiscographyCacheEntry, DiscographyFailureEntry};
```

- [ ] **Step 7: Run the tests to verify they pass**

Run: `cargo test --lib discography::tests`

Expected: PASS, including the five new tests.

- [ ] **Step 8: Commit**

```bash
git add src/discography/mod.rs src/runner.rs
git commit -m "feat(discography): replay recorded resolution failures"
```

---

### Task 4: Record failures, and clear them on success

**Files:**

- Modify: `src/discography/mod.rs` (the refresh `match` in `discover_artist_albums_at` around lines 600-622, helper next to `replay_recorded_failure`)
- Test: `src/discography/mod.rs`

- [ ] **Step 1: Write the failing tests**

Add to `src/discography/mod.rs` in `mod tests`:

```rust
    #[tokio::test]
    async fn an_unresolved_artist_is_recorded() {
        let db = Database::open_in_memory().unwrap();
        let provider = FakeProvider::unresolvable_recording();

        discover_artist_albums_at(
            &provider,
            &db,
            "Unknown",
            &DiscographyConfig::default(),
            1_000,
            FailureCacheUse::Honour,
        )
        .await;

        let stored = db.get_discography_failure("unknown").unwrap().unwrap();
        assert_eq!(stored.failure_kind, "unresolved");
        assert_eq!(stored.recorded_at, 1_000);
        assert!(!stored.reason.is_empty());
    }

    #[tokio::test]
    async fn a_provider_outage_is_not_recorded() {
        let db = Database::open_in_memory().unwrap();
        let provider = FakeProvider::failing("service unavailable");

        discover_artist_albums_at(
            &provider,
            &db,
            "Artist",
            &DiscographyConfig::default(),
            1_000,
            FailureCacheUse::Honour,
        )
        .await;

        assert!(
            db.get_discography_failure("artist").unwrap().is_none(),
            "a transient outage must never be remembered"
        );
    }

    #[tokio::test]
    async fn zero_failure_cache_days_records_nothing() {
        let db = Database::open_in_memory().unwrap();
        let config = DiscographyConfig {
            failure_cache_days: 0,
            ..DiscographyConfig::default()
        };
        let provider = FakeProvider::unresolvable_recording();

        discover_artist_albums_at(&provider, &db, "Unknown", &config, 1_000, FailureCacheUse::Honour)
            .await;

        assert!(db.get_discography_failure("unknown").unwrap().is_none());
    }

    #[tokio::test]
    async fn no_failure_row_is_written_when_a_success_row_exists() {
        let db = Database::open_in_memory().unwrap();
        let groups = vec![group("1", "Studio", Some("2000"), Some("Album"), &[])];
        // Stale, so the refresh still runs, but present, so the rows stay
        // mutually exclusive.
        cache_groups(&db, "artist", "11111111-1111-1111-1111-111111111111", 0, &groups);
        let provider = FakeProvider::default();

        discover_artist_albums_at(
            &provider,
            &db,
            "Artist",
            &DiscographyConfig::default(),
            31 * 86_400,
            FailureCacheUse::Honour,
        )
        .await;

        assert!(db.get_discography_failure("artist").unwrap().is_none());
    }

    #[tokio::test]
    async fn a_successful_refresh_clears_a_recorded_failure() {
        let db = Database::open_in_memory().unwrap();
        let config = DiscographyConfig::default();
        // Recorded exactly at the expiry, so the row is stale (equality is
        // stale, as it is for the success cache) and the refresh runs instead
        // of being replayed.
        record_failure(&db, "artist", "no candidate matches", 1_000);
        let provider = FakeProvider::with_groups(vec![group(
            "1",
            "Studio",
            Some("2000"),
            Some("Album"),
            &[],
        )]);

        discover_artist_albums_at(
            &provider,
            &db,
            "Artist",
            &config,
            1_000 + config.failure_cache_days * 86_400,
            FailureCacheUse::Honour,
        )
        .await;

        assert!(
            db.get_discography_failure("artist").unwrap().is_none(),
            "a successful resolution makes the recorded failure obsolete"
        );
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib discography::tests::an_unresolved_artist_is_recorded discography::tests::a_provider_outage_is_not_recorded`

Expected: `an_unresolved_artist_is_recorded` FAILS with a panic on `unwrap()`
(the row is absent). `a_provider_outage_is_not_recorded` PASSES already, which is
the correct starting point for a guard test — keep it.

- [ ] **Step 3: Implement the recording and clearing**

In `src/discography/mod.rs`, in the `Ok((resolved, groups))` arm of the refresh
match, before `select_outcome(...)`, add:

```rust
                if let Err(error) = db.delete_discography_failure(&artist_key) {
                    tracing::warn!("failed to clear discography failure cache for {artist:?}: {error}");
                }
```

In the `Err(error)` arm, replace the body with:

```rust
        Err(error) => {
            let has_success_row = cached.is_some();
            record_resolution_failure(db, &artist_key, config, now, &error, has_success_row);
            stale_or_legacy(cached, &config.allowed_types, now, error)
        }
```

And add the helper:

```rust
/// Record an artist-resolution failure so a later run can skip the lookup.
///
/// Only `Unresolved` is recorded: a `Provider` failure is an outage, and
/// remembering it would keep suppressing a healthy MusicBrainz for the whole
/// expiry. Nothing is recorded when the feature is off, when there is no usable
/// artist key, or when a success row already exists for this key — the two kinds
/// of row are mutually exclusive by construction, so a stale-but-usable success
/// row is never shadowed by a fresh failure.
fn record_resolution_failure(
    db: &Database,
    artist_key: &str,
    config: &DiscographyConfig,
    now: i64,
    error: &DiscographyError,
    has_success_row: bool,
) {
    if config.failure_cache_days == 0 || has_success_row || artist_key.is_empty() {
        return;
    }
    if DiscoveryFailure::from_error(error) != DiscoveryFailure::Unresolved {
        return;
    }
    let entry = DiscographyFailureEntry {
        artist_key: artist_key.to_string(),
        failure_kind: FAILURE_KIND_UNRESOLVED.to_string(),
        reason: error.to_string(),
        recorded_at: now,
    };
    if let Err(error) = db.upsert_discography_failure(&entry) {
        tracing::warn!("failed to write discography failure cache for {artist_key:?}: {error}");
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib discography::tests`

Expected: PASS, including all five new tests.

- [ ] **Step 5: Commit**

```bash
git add src/discography/mod.rs
git commit -m "feat(discography): record unresolved artists and clear on success"
```

---

### Task 5: Runner wiring, counter, and notice

**Files:**

- Modify: `src/discover.rs` (`DiscoverCounters` around line 276, `discover_notices` around line 340)
- Modify: `src/runner.rs` (import at line 12, discover loop setup around line 1590, `LegacyFallback` arm at line 1762, test provider at line 2802)
- Test: `src/discover.rs`, `src/runner.rs`

- [ ] **Step 1: Write the failing notice test**

Add to `src/discover.rs` in `mod tests`:

```rust
    #[test]
    fn cached_failures_get_their_own_notice() {
        let counters = DiscoverCounters {
            cached_failures: vec!["40 Licks".to_string(), "30Hz".to_string()],
            unresolved: vec!["Fresh Failure".to_string()],
            ..DiscoverCounters::default()
        };

        let notices = discover_notices(&counters);

        assert!(
            notices.contains(
                &"discover: 2 artist(s) skipped from cached resolution failures".to_string()
            ),
            "got {notices:?}"
        );
        assert!(
            notices.contains(
                &"discover: 1 artist(s) unresolved on MusicBrainz: Fresh Failure".to_string()
            ),
            "the cached and fresh notices must stay distinguishable: {notices:?}"
        );
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --lib discover::tests::cached_failures_get_their_own_notice`

Expected: FAIL to compile, with `no field cached_failures on DiscoverCounters`.

- [ ] **Step 3: Add the counter field and the notice**

In `src/discover.rs`, in `DiscoverCounters`, after `pub unresolved: Vec<String>,` add:

```rust
    /// Artists skipped because a previous run recorded a resolution failure,
    /// in encounter order. Kept apart from `unresolved` so the summary shows
    /// what was avoided rather than what was attempted.
    pub cached_failures: Vec<String>,
```

In `discover_notices`, immediately before the `if !counters.unresolved.is_empty()`
block, add:

```rust
    if !counters.cached_failures.is_empty() {
        notices.push(format!(
            "discover: {} artist(s) skipped from cached resolution failures",
            counters.cached_failures.len()
        ));
    }
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test --lib discover::tests::cached_failures_get_their_own_notice`

Expected: PASS.

- [ ] **Step 5: Write the failing runner test**

In `src/runner.rs` in `mod tests`, add a call counter to the fake provider so a
test can prove the provider was never consulted. Change the struct at
line 2802 to:

```rust
    struct FakeDiscographyProvider {
        groups: Vec<ReleaseGroup>,
        failure: Option<String>,
        /// `None` echoes the requested artist back as the only candidate.
        candidate_names: Option<Vec<String>>,
        artist_calls: Arc<AtomicUsize>,
    }
```

Add `use std::sync::Arc;` to the test module's imports — only
`std::sync::atomic::{AtomicUsize, Ordering}` is imported there today. Then add
`artist_calls: Arc::new(AtomicUsize::new(0)),` to the struct literals in
`with_groups`, `failing` and `unresolvable`, and add an accessor next to
`unresolvable`:

```rust
        fn calls(&self) -> usize {
            self.artist_calls.load(Ordering::SeqCst)
        }
```

and in `search_artists`, add as the first statement:

```rust
            self.artist_calls.fetch_add(1, Ordering::SeqCst);
```

Then add the test:

```rust
    #[tokio::test]
    async fn discover_skips_a_recorded_failure_without_calling_the_provider() {
        let soulseek = MockClient::new();
        let (config, db, staging, _library) = discover_fixture(&[("Test Artist", "Present")]);
        db.upsert_discography_failure(&crate::db::DiscographyFailureEntry {
            artist_key: "test artist".to_string(),
            failure_kind: "unresolved".to_string(),
            reason: "no candidate matches".to_string(),
            recorded_at: chrono::Utc::now().timestamp(),
        })
        .unwrap();
        let provider = FakeDiscographyProvider::unresolvable();

        run_discover_mode_with_provider(
            &soulseek,
            &config,
            &db,
            None,
            false,
            staging.path(),
            &provider,
        )
        .await
        .unwrap();

        assert_eq!(
            provider.calls(),
            0,
            "a recorded failure must spare the MusicBrainz lookup"
        );
    }

    #[tokio::test]
    async fn an_explicit_artist_bypasses_a_recorded_failure() {
        let soulseek = MockClient::new();
        let (config, db, staging, _library) = discover_fixture(&[("Test Artist", "Present")]);
        db.upsert_discography_failure(&crate::db::DiscographyFailureEntry {
            artist_key: "test artist".to_string(),
            failure_kind: "unresolved".to_string(),
            reason: "no candidate matches".to_string(),
            recorded_at: chrono::Utc::now().timestamp(),
        })
        .unwrap();
        let provider = FakeDiscographyProvider::unresolvable();

        run_discover_mode_with_provider(
            &soulseek,
            &config,
            &db,
            Some("Test Artist"),
            false,
            staging.path(),
            &provider,
        )
        .await
        .unwrap();

        assert!(
            provider.calls() > 0,
            "asking for an artist by name must reach MusicBrainz"
        );
    }
```

- [ ] **Step 6: Run the runner tests to verify they fail**

Run: `cargo test --lib runner::tests::discover_skips_a_recorded_failure_without_calling_the_provider runner::tests::an_explicit_artist_bypasses_a_recorded_failure`

Expected: the first test PASSES already, because Task 3 made the replay work and
with no `--artist` filter the loop passes `Honour`; it stands as a guard rather
than a RED. The second test FAILS with `provider.calls()` equal to 0, because
the discover loop still passes `Honour` regardless of the filter. Step 7 fixes
that.

- [ ] **Step 7: Wire the bypass choice and the reporting**

In `src/runner.rs`, extend the discography import (line 12) with
`FailureCacheUse`.

In `run_discover_mode_with_provider`, after
`let selection = discover::select_artists(...)` and before the `let mut counters`
line, add:

```rust
    // An explicit --artist is a deliberate one-off request, so it bypasses a
    // recorded failure; the scheduled sweep honours it. This matches the
    // existing rule that --artist overrides discover.exclude_artists.
    let failure_cache = if artist_filter.is_some() {
        FailureCacheUse::Bypass
    } else {
        FailureCacheUse::Honour
    };
```

Pass it at the loop's call site (line 1634):

```rust
        match discover_artist_albums(provider, db, &artist.name, &config.discography, failure_cache)
            .await
        {
```

In `run_artist_only_mode_with_provider`, pass `FailureCacheUse::Bypass` at
line 1280, because that path is always a named artist:

```rust
    match discover_artist_albums(
        provider,
        db,
        artist,
        &config.discography,
        FailureCacheUse::Bypass,
    )
    .await
    {
```

In the discover loop's `LegacyFallback` arm (line 1762), split the `Unresolved`
case on `from_cache`:

```rust
            DiscoveryOutcome::LegacyFallback {
                reason,
                kind,
                from_cache,
            } => match kind {
                DiscoveryFailure::Unresolved => {
                    if from_cache {
                        // No request was made, so this is no evidence about
                        // provider health: neither increment nor reset the
                        // consecutive-failure counter.
                        tracing::debug!(
                            "{}: skipped, resolution failure recorded earlier ({reason})",
                            artist.name
                        );
                        counters.cached_failures.push(artist.name.clone());
                    } else {
                        // MusicBrainz answered for this artist, so the provider
                        // is reachable. Reset the consecutive-failure count and
                        // keep the circuit breaker for genuine outages only.
                        consecutive_provider_failures = 0;
                        tracing::warn!(
                            "{}: artist could not be resolved on MusicBrainz: {reason}",
                            artist.name
                        );
                        counters.unresolved.push(artist.name.clone());
                    }
                }
                DiscoveryFailure::Provider => {
                    consecutive_provider_failures += 1;
                    tracing::warn!(
                        "{}: MusicBrainz unavailable ({reason}); artist skipped",
                        artist.name
                    );
                    counters.provider_failed.push((artist.name.clone(), reason));
                    if consecutive_provider_failures >= DISCOVER_PROVIDER_FAILURE_LIMIT {
                        return Err(abort_discover_run(
                            &mut report,
                            &counters,
                            progress.as_ref(),
                            &_listener,
                        ));
                    }
                }
            },
```

- [ ] **Step 8: Run the tests to verify they pass**

Run: `cargo test --lib runner::tests`

Expected: PASS, including both new tests and the existing
`discover_skips_an_unresolved_artist_without_searching` and
`discover_aborts_after_three_consecutive_provider_failures` guards.

- [ ] **Step 9: Commit**

```bash
git add src/discover.rs src/runner.rs
git commit -m "feat(runner): report cached resolution skips separately"
```

---

### Task 6: Documentation and full verification

**Files:**

- Modify: `README.md` (`### discover` prose)
- Test: whole suite

- [ ] **Step 1: Document the behaviour in the discover prose**

In `README.md`, in the `### discover` section, immediately after the paragraph
beginning "An album counts as present when a matching `artist/album` folder…",
add:

```markdown
An artist that MusicBrainz cannot resolve to exactly one candidate is recorded
for `discography.failure_cache_days`, and a later run skips the lookup for it
instead of asking again; the run summary reports those skips separately from
failures encountered in that run. Running an artist explicitly with `--artist`,
or pinning it in `discography.artist_mbids`, always queries MusicBrainz and
clears the recorded failure. Only resolution failures are remembered — an
outage is retried every run rather than cached.
```

- [ ] **Step 2: Run the full test suite**

Run: `cargo test`

Expected: PASS. Every suite green, with the new tests counted in the lib total.

- [ ] **Step 3: Run the lint and format gates**

Run: `cargo clippy --all-targets --all-features -- -D warnings && cargo fmt --check`

Expected: both exit 0 with no output.

- [ ] **Step 4: Run the dependency policy gate**

Run: `cargo deny check`

Expected: `advisories ok, bans ok, licenses ok, sources ok`.

- [ ] **Step 5: Markdown lint the README**

Run: `markdownlint --fix README.md && markdownlint README.md`

Expected: clean.

- [ ] **Step 6: Commit**

```bash
git add README.md
git commit -m "docs: describe the discography failure cache"
```

---

## Acceptance checks mapped to the spec

| Spec criterion | Where it is proven |
| --- | --- |
| 1. A second run makes zero `search_artists` calls for recorded failures | Task 3 `fresh_recorded_failure_is_replayed_without_a_provider_call`, Task 5 `discover_skips_a_recorded_failure_without_calling_the_provider` |
| 2. Retried after the expiry, not before | Task 3 `recorded_failure_at_the_ttl_boundary_is_retried`, plus `cache_is_fresh` semantics reused |
| 3. `failure_cache_days: 0` restores today's behaviour and writes nothing | Task 3 `zero_failure_cache_days_retries_and_records_nothing`, Task 4 `zero_failure_cache_days_records_nothing` |
| 4. `--artist` always queries | Task 5 `an_explicit_artist_bypasses_a_recorded_failure` |
| 5. A pinned MBID always queries and clears the row | Task 3 `a_pinned_mbid_bypasses_and_clears_a_recorded_failure` |
| 6. A `Provider` failure never creates a row | Task 4 `a_provider_outage_is_not_recorded` |
| 7. The summary distinguishes cached skips | Task 5 `cached_failures_get_their_own_notice` |
| 8. Existing tests, clippy, fmt, deny all green | Task 6 steps 2-4 |

## Notes for the implementer

- `cache_is_fresh` already implements exactly the expiry semantics needed
  (`days == 0` is never fresh, a future timestamp is stale, equality is stale).
  Use it; do not write a second TTL comparison.
- The failure row is written only when no success row exists, so the two tables
  never disagree about an artist. If a test seems to need both, the test is
  wrong — that combination is excluded by design, and Task 4's
  `no_failure_row_is_written_when_a_success_row_exists` pins it.
- `DiscoveryFailure::from_error` is the single classifier. Do not add a second
  match on error types.
- Commit after every task. Each commit must leave `cargo test` green.
