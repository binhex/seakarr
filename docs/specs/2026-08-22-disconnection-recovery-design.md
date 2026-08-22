# Server Disconnection Recovery

## Problem

When the Soulseek server drops the TCP connection (e.g., rate-limiting after a
burst of searches), seakarr's searches silently return zero results for all
subsequent albums. The vendored `soulseek-rs-lib` queues search messages in a
disconnected actor, and `collect_for(timeout)` burns the full 15-second window
waiting for results that will never arrive.

The `SessionWatch` mechanism exists in the vendored lib and correctly records
the loss (`SessionLoss::Disconnected` or `SessionLoss::Displaced`), but nothing
checks it. The `send_search()` method only verifies that the actor handle
exists (it does — the actor thread is still alive), not that the connection is
alive.

**Observed behaviour:** Searching for "intro" (a title-search fallback for
"DJ Krush — Strictly Turntablised") triggers a server disconnect. Every
subsequent album search returns 0 results, burning 45+ seconds per album (3
casing tiers x 15s timeout).

## Goal

Detect session loss early, reconnect transparently, and retry the failed
operation. If reconnection fails, return a clear error instead of silently
returning empty results.

**Scope:** Both `search()` and `download()` paths in `RealClient`. Changes in
both the vendored lib (defense in depth) and seakarr's client layer.

## Design

### Layer 1: Vendored Lib — Fail-Fast on Session Loss

**File:** `vendor/soulseek-rs-lib/src/client/search.rs`

Modify `send_search()` to check `session_loss()` before sending:

```rust
fn send_search(&self, query: &str, wishlist: bool) -> Result<()> {
    // Fail fast if the session is already lost
    if self.session_loss().is_some() {
        return Err(SoulseekRs::NotConnected);
    }

    let Some(handle) = &self.server_handle else {
        return Err(SoulseekRs::NotConnected);
    };
    // ... rest unchanged
}
```

This is defense in depth — even if the caller doesn't check `session_loss()`,
the search fails immediately instead of silently queuing and timing out.

**No changes to `ServerActor`.** The server actor's `tick()` does nothing when
`Disconnected` — this is intentional. Reconnection is handled at the seakarr
client level (creating a fresh `Client` with new actor threads), not by the
vendored lib's actor.

### Layer 2: Seakarr Client — Transparent Reconnect

**File:** `src/error.rs`

Add a new error variant:

```rust
#[error("server connection lost: {reason}")]
Disconnected { reason: String },
```

**File:** `src/client.rs`

Add a private method `reconnect_if_needed()` to `RealClient`:

1. Gets the connected client via `connected_client()`
2. Checks `client.session_loss()`
3. If `Some(loss)`, logs the loss reason
4. Creates a fresh `Client` using the stored credentials (see below) and
   attempts `connect()` + `login()` using the existing retry logic (3
   attempts, 5s backoff)
5. If login succeeds, replaces `self.inner` with the new client and returns
   `Ok(())`
6. If login fails, returns `Err(SeakarrError::Disconnected { reason })`

**Credential storage:** `RealClient` stores the server address, username,
password, and listen port after a successful `login()` call. These are needed
for reconnection. The vendored lib's `Client` exposes `username()` but not
`password()` or `address()`, so `RealClient` must retain them.

Wrap `search()` and `download()` with the reconnect check:

```rust
async fn search(&self, query: &str, timeout_secs: u64) -> Result<Vec<SearchResult>> {
    self.reconnect_if_needed().await?;
    // Existing search logic...
}
```

The `reconnect_if_needed()` call is idempotent — if the session is alive, it
returns `Ok(())` immediately with no overhead (just an atomic load).

**No changes to `SoulseekClient` trait.** The reconnect logic is internal to
`RealClient`. The trait stays unchanged — `MockClient` doesn't need session
loss simulation.

### Runner & Daemon Interaction

**No changes to the runner or daemon code.**

The reconnect is transparent — `RealClient::search()` and `download()` handle
it internally. The runner's existing error handling already works:

- **Auto mode:** Errors from `process_album` are caught and recorded as
  `AlbumOutcome::Failed { reason }`. If reconnect fails, the album is marked
  as failed with a clear reason like "server connection lost: the connection to
  the server dropped".

- **Manual mode:** Same pattern — errors propagate and the CLI exits non-zero
  with a clear message.

- **Daemon mode:** If a scan cycle fails, the error is logged and the daemon
  sleeps until the next cycle. The next cycle will attempt a fresh connection
  (since `reconnect_if_needed()` runs before each search).

**Key behaviour:** If reconnect succeeds mid-scan, the remaining albums in the
batch are processed normally. If reconnect fails, the current album fails fast
(no 15s timeout burn) and subsequent albums also fail fast (the
`reconnect_if_needed()` check returns the same error immediately).

## Testing

### Vendored Lib Tests

Add a test in `vendor/soulseek-rs-lib/src/client/search.rs` that verifies
`send_search()` returns `Err(NotConnected)` when `session_loss()` is set.

### Seakarr Client Tests

Add tests in `src/client.rs` for the `RealClient` reconnect behaviour:

1. **Reconnect succeeds:** Mock a session loss, verify that `search()` attempts
   re-login and retries the search successfully.
2. **Reconnect fails:** Mock a session loss where re-login also fails, verify
   that `search()` returns `Err(Disconnected)` with a clear reason.
3. **No session loss:** Verify that `search()` proceeds normally when the
   session is alive (no reconnect overhead).
4. **Download reconnect:** Same patterns for `download()`.

## Files Changed

| File | Change |
|------|--------|
| `vendor/soulseek-rs-lib/src/client/search.rs` | Add `session_loss()` check in `send_search()` |
| `src/error.rs` | Add `SeakarrError::Disconnected` variant |
| `src/client.rs` | Add credential fields to `RealClient`, add `reconnect_if_needed()`, wrap `search()` and `download()` |
