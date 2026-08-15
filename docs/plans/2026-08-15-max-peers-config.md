# `max_peers` Config Entry — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use sub-agents (recommended) to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a `max_peers` config field (default 64) to control the maximum simultaneous peer connections, replacing the hardcoded vendored library constant.

**Architecture:** Single config field threaded through `SoulseekConfig` → `main.rs` → `RealClient.set_max_peers()` → vendored library's `Client.set_max_peers(n)`. The vendored `DEFAULT_MAX_PEERS` is restored to the upstream's 512 (the config overrides it after login).

**Tech Stack:** Rust, serde, tokio

---

## File Structure

| File | Change |
|---|---|
| `src/config.rs` | Add `max_peers: usize` to `SoulseekConfig` + default fn + `Config::default()` + `sample_yaml()` + tests |
| `src/client.rs` | Add `set_max_peers` method to `RealClient` |
| `src/main.rs` | Call `client.set_max_peers(config.soulseek.max_peers)` after login |
| `vendor/soulseek-rs-lib/src/client/mod.rs` | Restore `DEFAULT_MAX_PEERS` from 32 to 512 |
| `README.md` | Document `max_peers` in the soulseek config table |

---

### Task 1: Add `max_peers` to `SoulseekConfig`

**Files:**
- Modify: `src/config.rs`

- [ ] **Step 1: Add the field to `SoulseekConfig`**

In `src/config.rs`, in the `SoulseekConfig` struct (line ~36), add after `listen_port`:

```rust
    #[serde(default = "default_max_peers")]
    pub max_peers: usize,
```

- [ ] **Step 2: Add the default function**

In `src/config.rs`, near the other default functions (after `default_listen_port` at line ~212), add:

```rust
fn default_max_peers() -> usize {
    64
}
```

- [ ] **Step 3: Add to `Config::default()`**

In `src/config.rs`, in the `Default for Config` impl's soulseek block (line ~505), add after `listen_port: default_listen_port(),`:

```rust
                max_peers: default_max_peers(),
```

- [ ] **Step 4: Add to `sample_yaml()`**

In `src/config.rs`, in the `sample_yaml()` function's soulseek section (after `listen_port: 2234` at line ~593), add:

```yaml
  max_peers: 64
```

- [ ] **Step 5: Update existing config tests**

In `src/config.rs` test module, in `test_load_config_from_yaml` (line ~745), add after the `listen_port` assertion:

```rust
        assert_eq!(config.soulseek.max_peers, 64);
```

And in the defaults test (line ~801), add:

```rust
        assert_eq!(config.soulseek.max_peers, 64);
```

- [ ] **Step 6: Verify compilation**

Run: `cargo build 2>&1 | tail -5`
Expected: `Finished dev profile`

- [ ] **Step 7: Commit**

```bash
git add src/config.rs
git commit -m "feat: add max_peers field to SoulseekConfig (default 64)"
```

---

### Task 2: Add `set_max_peers` to `RealClient` and call after login

**Files:**
- Modify: `src/client.rs` (add method to `RealClient`)
- Modify: `src/main.rs` (call after login)

- [ ] **Step 1: Add `set_max_peers` method to `RealClient`**

In `src/client.rs`, after the `connected_client` method (line ~290), add:

```rust
    /// Set the maximum number of simultaneous peer connections.
    /// Delegates to the vendored library's `Client::set_max_peers` which
    /// enforces a floor of 1.
    pub async fn set_max_peers(&self, max_peers: usize) -> Result<()> {
        let client = self.connected_client().await?;
        client.set_max_peers(max_peers);
        Ok(())
    }
```

- [ ] **Step 2: Call after login in `main.rs`**

In `src/main.rs`, after the login succeeds (after `tracing::info!("Connected to Soulseek.");` at line ~182), add:

```rust
    client
        .set_max_peers(config.soulseek.max_peers)
        .await?;
```

- [ ] **Step 3: Verify compilation**

Run: `cargo build 2>&1 | tail -5`
Expected: `Finished dev profile`

- [ ] **Step 4: Commit**

```bash
git add src/client.rs src/main.rs
git commit -m "feat: apply max_peers config after login via set_max_peers"
```

---

### Task 3: Restore vendored `DEFAULT_MAX_PEERS` to upstream 512

**Files:**
- Modify: `vendor/soulseek-rs-lib/src/client/mod.rs`

- [ ] **Step 1: Change the constant**

In `vendor/soulseek-rs-lib/src/client/mod.rs`, line ~59:

From:
```rust
const DEFAULT_MAX_PEERS: usize = 32;
```

To:
```rust
const DEFAULT_MAX_PEERS: usize = 512;
```

- [ ] **Step 2: Verify all tests pass**

Run: `cargo test 2>&1 | grep "test result"`
Expected: all pass (the config overrides the constant after login)

- [ ] **Step 3: Commit**

```bash
git add vendor/soulseek-rs-lib/src/client/mod.rs
git commit -m "chore: restore vendored DEFAULT_MAX_PEERS to upstream 512"
```

---

### Task 4: Update README

**Files:**
- Modify: `README.md`

- [ ] **Step 1: Add to soulseek config table**

In `README.md`, in the soulseek config table (after `listen_port` row, line ~125), add:

```markdown
| `max_peers` | Maximum simultaneous peer connections. Each connection uses a 256 KB actor thread. Higher values allow more parallel search/download candidates but use more memory. | `64` |
```

- [ ] **Step 2: Commit**

```bash
git add README.md
git commit -m "docs: document max_peers config field"
```

---

### Task 5: Full verification

- [ ] **Step 1: Run the full test suite**

Run: `cargo test 2>&1 | grep "test result"`
Expected: all pass across all crates

- [ ] **Step 2: Run clippy**

Run: `cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -3`
Expected: `Finished dev profile` — zero warnings

- [ ] **Step 3: Run fmt check**

Run: `cargo fmt --check 2>&1`
Expected: clean (no diff)

---

## Non-Goals (from spec)

- No CLI flag (`--max-peers`) — config file only
- No dynamic runtime changes — `set_max_peers` called once after login
- No per-search or per-album peer limits
- No upper bound validation — the OS enforces it via thread/memory limits
