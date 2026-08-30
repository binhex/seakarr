# Effective-throughput peer reputation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use sub-agents (recommended) to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Record a peer's *effective throughput* (bytes ÷ total wall-clock time, including retries) as its measured speed, so retried-but-recovered downloads demote the peer in ranking.

**Architecture:** The change is confined to `src/download.rs`. `download_once` stops returning the instantaneous speed (reverting to `Result<PathBuf>`, keeping its `speed_ema` only for the progress bar). `download_file` captures a start `Instant` before its retry loop and, on success, returns `file_bytes / elapsed / 1024.0` (KiB/s) via a small pure helper. Downstream (`TrackRecord`, `update_peer_reputation`, `rank_candidates`) is unchanged.

**Tech Stack:** Rust, tokio. No new dependencies.

---

## File map

- Modify `src/download.rs` — add `effective_throughput_kib_s` helper, revert `download_once` return, compute throughput in `download_file`.
- Modify `README.md` — feature wording.
- Nothing else changes. `src/download.rs`'s `download_file` **call sites are unchanged** (its signature `-> Result<(PathBuf, f64)>` stays; only the `f64`'s meaning changes). `download_album`'s `(path, speed_kbps)` binding is unchanged.

---

## Task 1: `effective_throughput_kib_s` helper (pure, deterministic)

**Files:**
- Modify: `src/download.rs`

- [ ] **Step 1: Write the failing test**

Add to `src/download.rs` `#[cfg(test)] mod tests`:

```rust
#[test]
fn effective_throughput_computes_bytes_per_wall_second() {
    use std::time::Duration;
    // 1024 bytes over 1 second = 1 KiB/s
    assert!((effective_throughput_kib_s(1024, Duration::from_secs(1)) - 1.0).abs() < 1e-9);
    // 1024 bytes over 2 seconds = 0.5 KiB/s
    assert!((effective_throughput_kib_s(1024, Duration::from_secs(2)) - 0.5).abs() < 1e-9);
}

#[test]
fn effective_throughput_guards_against_zero_elapsed() {
    use std::time::Duration;
    // Zero elapsed must not divide by zero; the 1ms floor yields a finite value.
    assert!(effective_throughput_kib_s(1024, Duration::ZERO).is_finite());
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p seakarr download::tests::effective_throughput`
Expected: FAIL (compile error — `effective_throughput_kib_s` not found).

- [ ] **Step 3: Implement the helper**

Add near the other free helpers in `src/download.rs` (e.g. next to `ema_update`):

```rust
/// Effective transfer throughput: downloaded bytes over total wall-clock time,
/// in KiB/s. Elapsed is floored at 1 ms so a near-instant transfer can't
/// divide by zero.
fn effective_throughput_kib_s(bytes: u64, elapsed: std::time::Duration) -> f64 {
    let secs = elapsed.as_secs_f64().max(0.001);
    bytes as f64 / secs / 1024.0
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p seakarr download::tests::effective_throughput`
Expected: PASS (2 tests).

- [ ] **Step 5: Commit**

```bash
git add src/download.rs
git commit -m "feat(download): add effective-throughput helper"
```

---

## Task 2: `download_once` returns just the path

**Files:**
- Modify: `src/download.rs`

- [ ] **Step 1: Change the return type**

Change `download_once`'s signature return from `Result<(PathBuf, f64)>` to `Result<PathBuf>`.

- [ ] **Step 2: Change the success return**

Find the `Completed` arm's return:

```rust
                return Ok((dest, speed_ema.unwrap_or(0.0) / 1024.0)); // KiB/s
```

Replace with:

```rust
                return Ok(dest);
```

The `speed_ema` variable is still used earlier in the loop for the progress bar (`bar.set_prefix(format_speed(smoothed as u64))`), so it is not dead code.

- [ ] **Step 3: Update `download_file`'s match arm**

In `download_file`, the `Ok((path, speed_kbps)) => return Ok((path, speed_kbps))` arm no longer compiles. Replace it (this also carries out Task 3's logic in one place):

```rust
        match download_once(
            client, file, basename, username, dir, config, filters, progress, cancel,
        )
        .await
        {
            Ok(path) => {
                let bytes = std::fs::metadata(&path)
                    .map(|m| m.len())
                    .unwrap_or(file.size);
                let throughput = effective_throughput_kib_s(bytes, started.elapsed());
                return Ok((path, throughput));
            }
            Err(e) => {
                if cancel.is_some_and(|c| c.load(Ordering::SeqCst)) {
                    return Err(e);
                }
                last_err = Some(e);
            }
        }
```

- [ ] **Step 4: Compile + run download tests**

Run: `cargo test -p seakarr download::`
Expected: PASS (the `download_file` call sites still match `Result<(PathBuf, f64)>`, so no test call-site edits are needed — verify none reference `download_once` directly with `grep -n "download_once(" src/download.rs`; the only caller is `download_file`).

- [ ] **Step 5: Commit**

```bash
git add src/download.rs
git commit -m "feat(download): download_once returns path only; compute throughput in download_file"
```

---

## Task 3: Capture start time in `download_file`

**Files:**
- Modify: `src/download.rs`

- [ ] **Step 1: Add the start instant**

Just above `let mut last_err: Option<SeakarrError> = None;` (before the `for attempt in 0..=config.max_retries` loop), add:

```rust
    // Wall-clock start of the whole retry loop: the recorded throughput spans
    // every attempt and every retry_delay_secs sleep, so a peer that needs
    // retries is demoted by the time it costs.
    let started = std::time::Instant::now();
    let mut last_err: Option<SeakarrError> = None;
```

(`std::time::Instant` is used fully-qualified; no import change.)

- [ ] **Step 2: Compile + run the full suite**

Run: `cargo test --workspace`
Expected: PASS (the `started` binding is used by the `Ok(path)` arm added in Task 2).

- [ ] **Step 3: Commit**

```bash
git add src/download.rs
git commit -m "feat(download): record effective throughput over the retry loop"
```

---

## Task 4: README wording

**Files:**
- Modify: `README.md`

- [ ] **Step 1: Update the feature bullet**

Find the peer-reputation wording and change "measured download speed" to reflect throughput:

```markdown
- **Peer reputation** — remembers each peer's effective throughput (bytes ÷ wall-clock time, including
  retries) and success rate (in SQLite), and ranks search results by a blend of advertised and measured
  throughput plus a reliability factor, so fast, reliable peers are preferred and slow or error-prone peers
  are demoted — regardless of what you search for. Controlled by `search.peer_reputation` (default `true`).
```

(Adjust only the factual wording; keep the existing bullet's structure and the config-row unchanged.)

- [ ] **Step 2: Commit**

```bash
git add README.md
git commit -m "docs: describe effective-throughput reputation in README"
```

---

## Final verification

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

All three must exit 0.

---

## Self-review notes

- **Spec coverage:** helper + wall-clock capture (Tasks 1 & 3), `download_once` revert (Task 2), README (Task 4). Every spec section maps to a task. No schema/config/ranking changes — matching the spec.
- **Type consistency:** `effective_throughput_kib_s(bytes: u64, elapsed: std::time::Duration) -> f64` (Task 1) is called in Task 3 with `(bytes, started.elapsed())`; `download_file` keeps `Result<(PathBuf, f64)>` so all existing call sites and `download_album`'s `(path, speed_kbps)` binding stay unchanged.
- **Placeholders:** none — all code and commands are concrete.
