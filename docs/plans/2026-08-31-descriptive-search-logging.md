# Descriptive search-tier logging Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use sub-agents (recommended) to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the search-fallback cascade self-explanatory in the log by logging one descriptive line per search tier, replacing the vendored crate's ambiguous bare "Searching for {query}" lines.

**Architecture:** Three surgical edits, all log-only. `search_album_with_fallback` logs a tier label before each of tiers 1a/1b/2; `search_by_title` logs a label before its track-title search; the vendored `client/search.rs` demotes its `info!` to `debug!`. No behaviour, schema, config, or API changes.

**Tech Stack:** Rust, tracing. No new dependencies.

**Testing note (why no TDD here):** this change only alters log output. No test asserts log text, and capturing logs would require a test-only `tracing` subscriber (out of scope, and the approved spec explicitly says "no new tests"). Each task therefore verifies by `cargo fmt --check` + `cargo test --workspace` staying green (496 tests), not by a RED test. `src/search.rs` has no existing `tracing` import — use fully-qualified `tracing::info!` (as shown).

---

## File map

- Modify `src/search.rs` — tier labels in `search_album_with_fallback` and `search_by_title`.
- Modify `vendor/soulseek-rs-lib/src/client/search.rs` — `info!` → `debug!`.

---

## Task 1: Tier labels in `search_album_with_fallback`

**Files:**
- Modify: `src/search.rs`

- [ ] **Step 1: Add the Tier 1a label**

Just above `let results = search_album(client, artist, album, timeout_secs).await?;` in `search_album_with_fallback`, insert:

```rust
    // Tier 1a: primary "Artist Album" search (original casing)
    if let Some(a) = album.filter(|a| !a.trim().is_empty()) {
        if artist.trim().is_empty() {
            tracing::info!("Searching for Album ({a})");
        } else {
            tracing::info!("Searching for Artist + Album ({artist} {a})");
        }
    } else {
        tracing::info!("Searching for Artist ({artist})");
    }
    let results = search_album(client, artist, album, timeout_secs).await?;
```

- [ ] **Step 2: Add the Tier 1b label**

Just above `let lower_results = search_album(client, &artist_lower, Some(&album_lower), timeout_secs).await?;`, insert:

```rust
                tracing::info!(
                    "Searching for Artist + Album lowercase ({artist_lower} {album_lower})"
                );
```

- [ ] **Step 3: Add the Tier 2 label**

Just above `let album_results = search_album(client, "", Some(album_name), timeout_secs).await?;`, insert:

```rust
            tracing::info!("Searching for Album ({album_name})");
```

- [ ] **Step 4: Verify**

Run: `cargo fmt --check && cargo test -p seakarr search::`
Expected: fmt clean, all `search::` tests pass (log lines don't affect them).

- [ ] **Step 5: Commit**

```bash
git add src/search.rs
git commit -m "feat(search): log a descriptive label per search tier"
```

---

## Task 2: Track-title label in `search_by_title`

**Files:**
- Modify: `src/search.rs`

- [ ] **Step 1: Add the label**

Just above `let mut results = search_raw(client, &query, timeout_secs).await?;` in `search_by_title`, insert:

```rust
    tracing::info!("Searching by track title ({query})");
```

- [ ] **Step 2: Verify**

Run: `cargo fmt --check && cargo test -p seakarr search::`
Expected: clean + green.

- [ ] **Step 3: Commit**

```bash
git add src/search.rs
git commit -m "feat(search): log the track-title fallback query"
```

---

## Task 3: Demote the vendored "Searching for" line to debug

**Files:**
- Modify: `vendor/soulseek-rs-lib/src/client/search.rs`

- [ ] **Step 1: Change the level**

Find:

```rust
        info!("Searching for {}", query);
```

Replace with:

```rust
        debug!("Searching for {}", query);
```

(`debug!` is already available in the vendored crate's macro set.)

- [ ] **Step 2: Verify**

Run: `cargo test --workspace`
Expected: 496/496 pass (nothing asserts on the vendored log level).

- [ ] **Step 3: Commit**

```bash
git add vendor/soulseek-rs-lib/src/client/search.rs
git commit -m "refactor(soulseek-rs-lib): log outgoing searches at debug, not info"
```

---

## Final verification

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

All three must exit 0 (496 tests). Confirm `git status` shows only `src/search.rs` and `vendor/soulseek-rs-lib/src/client/search.rs`.

---

## Self-review notes

- **Spec coverage:** tiers 1a/1b/2 (Task 1), track-title (Task 2), vendored demotion (Task 3). Every spec requirement maps to a task.
- **Type consistency:** all labels use the in-scope variables (`artist`, `album`, `artist_lower`, `album_lower`, `album_name`, `query`) already bound at each site — no new names introduced.
- **Placeholders:** none — every snippet is complete.
