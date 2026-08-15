# Incoming Port (Listener) Support — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use sub-agents (recommended) to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Expose the soulseek_rs library's existing listener support through a `listen_port` config field and `--listen-port` CLI flag, replacing the hardcoded `enable_listen: false`.

**Architecture:** Single config field (`listen_port: u16`, default 2234, 0 = disabled) threaded through CLI overrides → `SoulseekClient::login` trait parameter → `ClientSettings` in the real client. The library handles binding, port-in-use fallback, and server advertisement.

**Tech Stack:** Rust, tokio, serde, clap, tracing

---

## File Structure

| File | Change |
|---|---|
| `src/config.rs` | Add `listen_port: u16` to `SoulseekConfig` + default fn; add `listen_port: Option<u16>` to `CliOverrides` + merge logic; update sample_yaml + 2 existing tests |
| `src/main.rs` | Add `--listen-port <N>` to `Cli`; pass through `CliOverrides` construction |
| `src/client.rs` | Add `listen_port: u16` param to `SoulseekClient::login` trait, `RealClient::login`, `MockClient::login`; replace `enable_listen: false` with config-driven values + log |
| `README.md` | Document the field in the soulseek config table + the CLI flag |

---

### Task 1: Add `listen_port` to `SoulseekConfig`

**Files:**
- Modify: `src/config.rs`
- Test: `src/config.rs` (existing tests module)

- [ ] **Step 1: Add the field to `SoulseekConfig`**

In `src/config.rs`, in the `SoulseekConfig` struct (line ~26), add after `login_retry_delay_secs`:

```rust
#[serde(default)]
pub listen_port: u16,
```

Note: `u16` defaults to `0` when missing from YAML (serde's `default` attribute uses `Default::default()` for the type, which is `0` for integers). Port `0` = disabled (firewalled mode).

- [ ] **Step 2: Add `listen_port` to `Config::default()`**

In `src/config.rs`, in the `Default for Config` impl's `soulseek: SoulseekConfig { ... }` block (line ~494), add after `login_retry_delay_secs: default_login_retry_delay(),`:

```rust
listen_port: default_listen_port(),
```

And add the default function near the other default functions (line ~201 area):

```rust
fn default_listen_port() -> u16 {
    2234
}
```

- [ ] **Step 3: Update sample_yaml**

In `src/config.rs`, in the `sample_yaml()` function's soulseek section (line ~577), add after `login_retry_delay_secs: 5`:

```yaml
  listen_port: 2234
```

- [ ] **Step 4: Update existing config tests**

In `src/config.rs` test module, the test `test_load_config_from_yaml` (around line ~670) already asserts on `config.filters.peer_track_count`. Add after that assertion:

```rust
assert_eq!(config.soulseek.listen_port, 2234);
```

Also verify the default test (around line ~708, `test_peer_track_count_defaults_true` area or wherever `Config::default()` is tested):

```rust
assert_eq!(config.soulseek.listen_port, 2234);
```

- [ ] **Step 5: Verify compilation**

Run: `cargo build 2>&1 | tail -5`
Expected: `Finished dev profile`

- [ ] **Step 6: Commit**

```bash
git add src/config.rs
git commit -m "feat: add listen_port field to SoulseekConfig (default 2234, 0=disabled)"
```

---

### Task 2: Thread `listen_port` through the `SoulseekClient` login trait

**Files:**
- Modify: `src/client.rs` (trait definition, RealClient, MockClient)
- Modify: `src/main.rs` (login call site)

This task changes the `login` trait signature from 3 parameters to 4. Every implementation and call site must update or the workspace won't compile.

- [ ] **Step 1: Update the trait definition**

In `src/client.rs`, change the `SoulseekClient` trait's `login` signature (line ~66):

From:
```rust
async fn login(&self, username: &str, password: &str, server: &str) -> Result<()>;
```

To:
```rust
async fn login(&self, username: &str, password: &str, server: &str, listen_port: u16) -> Result<()>;
```

- [ ] **Step 2: Update `MockClient::login`**

In `src/client.rs`, change the MockClient's `login` implementation (line ~140):

From:
```rust
async fn login(&self, _username: &str, _password: &str, _server: &str) -> Result<()> {
```

To:
```rust
async fn login(&self, _username: &str, _password: &str, _server: &str, _listen_port: u16) -> Result<()> {
```

(The parameter is unused in the mock — it just checks `login_should_fail`.)

- [ ] **Step 3: Update `RealClient::login`**

In `src/client.rs`, change the RealClient's `login` signature (line ~480):

From:
```rust
async fn login(&self, username: &str, password: &str, server: &str) -> Result<()> {
```

To:
```rust
async fn login(&self, username: &str, password: &str, server: &str, listen_port: u16) -> Result<()> {
```

And inside the method, replace the hardcoded `enable_listen: false` in the `ClientSettings` construction (line ~492):

From:
```rust
let settings = ClientSettings {
    username: username.to_string(),
    password: password.to_string(),
    server_address: address.clone(),
    // Headless CLI: never accept inbound transfers.
    enable_listen: false,
    ..ClientSettings::default()
};
```

To:
```rust
let enable_listen = listen_port > 0;
let settings = ClientSettings {
    username: username.to_string(),
    password: password.to_string(),
    server_address: address.clone(),
    enable_listen,
    listen_port,
    ..ClientSettings::default()
};
```

- [ ] **Step 4: Add listener status logging after successful login**

In `src/client.rs`, in `RealClient::login`, after the successful login match arm (where the client is stored), add logging. Find the spot where `tracing::info!("Connected to Soulseek.")` is called — actually this is in main.rs. Let me check where the client is stored after login.

After the successful login result is matched and the client is stored in `self.inner`, add logging. In the RealClient login method, after the `Ok(true)` match arm stores the client, add:

```rust
if enable_listen {
    tracing::info!("[listener] enabled on port {listen_port}");
} else {
    tracing::info!("[listener] disabled (listen_port=0)");
}
```

Place this after the `self.inner.lock().await = Some(Arc::new(client));` line but before the retry loop continues. The exact placement: after the successful login block (where the client is moved out of the blocking task and stored).

- [ ] **Step 5: Update the login call in `main.rs`**

In `src/main.rs`, find the `client.login(...)` call (line ~166):

From:
```rust
client
    .login(
        &config.soulseek.username,
        &config.soulseek.password,
        &config.soulseek.server,
    )
    .await?;
```

To:
```rust
client
    .login(
        &config.soulseek.username,
        &config.soulseek.password,
        &config.soulseek.server,
        config.soulseek.listen_port,
    )
    .await?;
```

- [ ] **Step 6: Verify compilation and all tests pass**

Run: `cargo test 2>&1 | grep "test result"`
Expected: all pass (no logic change, just signature threading)

Run: `cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -3`
Expected: clean

- [ ] **Step 7: Commit**

```bash
git add src/client.rs src/main.rs
git commit -m "refactor: add listen_port to SoulseekClient login trait"
```

---

### Task 3: Add `--listen-port` CLI flag and override plumbing

**Files:**
- Modify: `src/main.rs` (Cli struct, CliOverrides construction)
- Modify: `src/config.rs` (CliOverrides struct, merge_cli method)
- Test: `src/config.rs` (existing merge_cli test)

- [ ] **Step 1: Add the CLI argument to the `Cli` struct**

In `src/main.rs`, in the `Cli` struct, add after `soulseek_password`:

```rust
/// Override incoming peer port (0 disables listener)
#[arg(long)]
listen_port: Option<u16>,
```

- [ ] **Step 2: Pass it through `CliOverrides` construction**

In `src/main.rs`, in the `cli_overrides = CliOverrides { ... }` block (line ~113), add after `soulseek_password: cli.soulseek_password.clone(),`:

```rust
listen_port: cli.listen_port,
```

- [ ] **Step 3: Add `listen_port` to `CliOverrides` struct**

In `src/config.rs`, in the `CliOverrides` struct (line ~180), add after `pub soulseek_password: Option<String>,`:

```rust
pub listen_port: Option<u16>,
```

- [ ] **Step 4: Add merge logic**

In `src/config.rs`, in `merge_cli()` (line ~415), add after the `soulseek_password` override block:

```rust
if let Some(port) = cli.listen_port {
    self.soulseek.listen_port = port;
}
```

- [ ] **Step 5: Update the existing CLI override test**

In `src/config.rs` test module, in the test that checks CLI overrides (around line ~855), update the `CliOverrides` literal to include `listen_port`:

```rust
listen_port: Some(8080),
```

And add an assertion:

```rust
assert_eq!(config.soulseek.listen_port, 8080);
```

- [ ] **Step 6: Verify compilation and all tests pass**

Run: `cargo test 2>&1 | grep "test result"`
Expected: all pass

Run: `cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -3`
Expected: clean

- [ ] **Step 7: Commit**

```bash
git add src/main.rs src/config.rs
git commit -m "feat: add --listen-port CLI flag and override plumbing"
```

---

### Task 4: Document in README

**Files:**
- Modify: `README.md`

- [ ] **Step 1: Add to soulseek config table**

In `README.md`, in the soulseek config table (after `login_retry_delay_secs`, line ~126), add a new row:

```markdown
| `listen_port` | Incoming peer port for accepting connections from other Soulseek clients. Set to `0` to disable the listener (firewalled mode). Requires port forwarding at your router for values > 0. | `2234` |
```

- [ ] **Step 2: Add to CLI flags table**

In `README.md`, in the "Soulseek auth" CLI flags table (after `--soulseek-password`, line ~100), add:

```markdown
| `--listen-port <N>` | Override incoming peer port. `0` disables the listener. | *(from config)* |
```

- [ ] **Step 3: Commit**

```bash
git add README.md
git commit -m "docs: document listen_port config field and --listen-port flag"
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

- [ ] **Step 4: Commit any formatting fixes**

If `cargo fmt` made changes:
```bash
cargo fmt
git add -A
git commit -m "style: cargo fmt"
```

---

## Non-Goals (from spec)

- UPnP/NAT-PMP automatic port forwarding (future enhancement)
- Shared directories / upload serving (seakarr is download-only)
- Multiple listen ports
- IPv6 listener binding
- Port-in-use handling (the lib falls back to ephemeral port automatically)

## Behaviour Reference

| Config | CLI | Result |
|--------|-----|--------|
| `listen_port: 2234` | (none) | Binds `0.0.0.0:2234`, advertises to server |
| `listen_port: 0` | (none) | No listener, firewalled mode |
| `listen_port: 2234` | `--listen-port 0` | No listener (CLI override) |
| `listen_port: 2234` | `--listen-port 8080` | Binds `0.0.0.0:8080` |
