# Spec: `max_peers` Config Entry

## Problem

The maximum number of simultaneous peer connections is hardcoded in the vendored soulseek-rs library (`DEFAULT_MAX_PEERS`). Seakarr overrides it to 32, but changing it requires editing the vendored source code. Users should be able to tune this value via the config file.

## Solution

Expose `max_peers` as a config field in the `soulseek` section. After login, call the vendored library's `set_max_peers` API to apply the configured value.

### Config Changes

Add `max_peers` to `SoulseekConfig` in `src/config.rs`:

```yaml
soulseek:
  max_peers: 64    # max simultaneous peer connections (each uses ~256 KB stack)
```

- Default: **64**
- Floor: 1 (enforced by the vendored library's `set_max_peers`)
- No upper bound (the OS enforces it via thread/memory limits)

### Code Changes

#### `src/config.rs`

1. Add `max_peers: usize` field to `SoulseekConfig` with `#[serde(default = "default_max_peers")]`
2. Add `default_max_peers() -> usize` function returning `64`
3. Add `max_peers: default_max_peers()` to `Config::default()` soulseek block
4. Add `max_peers: 64` to `sample_yaml()` soulseek section
5. Update existing tests to assert `config.soulseek.max_peers == 64`

#### `src/main.rs`

After login succeeds (after `client.login(...)` call), add:

```rust
client.set_max_peers(config.soulseek.max_peers);
```

This calls the vendored library's `Client::set_max_peers` which stores the value in an `AtomicUsize` read by the peer registry.

#### `vendor/soulseek-rs-lib/src/client/mod.rs`

Change `DEFAULT_MAX_PEERS` from `32` back to `512` (upstream default). The config overrides it immediately after login, so the constant only matters if `set_max_peers` is never called — which doesn't happen in seakarr.

#### `README.md`

Add to the soulseek config table:

| Key | Description | Default |
|-----|-------------|---------|
| `max_peers` | Maximum simultaneous peer connections. Each connection uses a 256 KB actor thread. Higher values allow more parallel search/download candidates but use more memory. | `64` |

### Behaviour

| Config | Result |
|--------|--------|
| `max_peers: 64` (default) | 64 simultaneous connections allowed |
| `max_peers: 32` | 32 connections (conservative) |
| `max_peers: 128` | 128 connections (aggressive) |
| `max_peers: 0` | Floor enforced: 1 connection |
| (not set) | Default 64 |

### Testing

1. **Unit test**: `SoulseekConfig` default has `max_peers: 64`
2. **Unit test**: YAML loading sets `max_peers` correctly
3. **Existing tests**: all pass (no logic change, just config plumbing)

### Out of Scope

- CLI flag (`--max-peers`) — config file only, per user request
- Dynamic runtime changes — `set_max_peers` is called once after login
- Per-search or per-album peer limits
