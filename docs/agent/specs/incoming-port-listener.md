# Spec: Incoming Port (Listener) Support

## Problem

Seakarr currently hardcodes `enable_listen: false` when creating the soulseek_rs `ClientSettings` (`src/client.rs:492`). This means:

- Seakarr can only reach peers who have their own incoming port open
- Firewalled peers (who don't have a port forward) are unreachable
- Search results are reduced because we miss peers behind NAT/firewall
- The Soulseek server sees seakarr as a firewalled client, which limits its role in the network

The soulseek_rs library already has full listener support: `Listen::bind(port)` binds `0.0.0.0:port`, `Listen::serve()` accepts PierceFirewall and PeerInit connections, and `ServerActor` advertises the bound port to the server on login. Seakarr just needs to expose this via config and CLI.

## Solution

Expose the existing listener functionality through a single config field and CLI flag.

### Config Changes

Add `listen_port` to `SoulseekConfig` in `src/config.rs`:

```yaml
soulseek:
  server: "server.slsknet.org:2242"
  listen_port: 2234    # incoming peer port; 0 = disabled (firewalled mode)
```

- Default: **2234** (Soulseek standard port, matches lib's `DEFAULT_LISTEN_PORT`)
- Value `0` disables the listener (firewalled mode, current behaviour)
- Any value > 0 enables the listener on that port

### CLI Changes

Add `--listen-port <N>` to the CLI in `src/main.rs`:

- Overrides the config value (CLI takes precedence)
- `--listen-port 0` disables the listener regardless of config
- `--listen-port 8080` overrides config's `listen_port`

### Code Changes

#### `src/config.rs`

1. Add `listen_port: u16` field to `SoulseekConfig` struct with `#[serde(default = "default_listen_port")]`
2. Add `default_listen_port() -> u16` function returning `2234`
3. Add `listen_port: Option<u16>` to `CliOverrides` struct
4. In `merge_cli()`: if `cli.listen_port` is `Some`, override `self.soulseek.listen_port`

#### `src/main.rs`

1. Add `--listen-port <N>` argument to the `Cli` struct with clap
2. Pass it through to `CliOverrides`

#### `src/client.rs`

1. In `RealClient::login()` at line ~490:
   - Replace `enable_listen: false` with `enable_listen: config.soulseek.listen_port > 0`
   - Set `listen_port: config.soulseek.listen_port`
2. After successful connect, log the bound port:
   - If listener enabled: `info!("[listener] listening on 0.0.0.0:{bound_port}")`
   - If listener disabled: `info!("[listener] disabled (listen_port=0)")`
3. The lib handles: port-in-use fallback to ephemeral, server advertisement, connection accept loop

#### `README.md`

Add to the soulseek config table:

| Key | Description | Default |
|-----|-------------|---------|
| `listen_port` | Incoming peer port. Set to 0 to disable the listener (firewalled mode). Requires port forwarding at router level for values > 0. | `2234` |

Add to CLI flags:

| Flag | Description |
|------|-------------|
| `--listen-port <N>` | Override `listen_port` config. 0 disables the listener. |

### Behaviour

| Config | CLI | Result |
|--------|-----|--------|
| `listen_port: 2234` | (none) | Binds `0.0.0.0:2234`, advertises to server |
| `listen_port: 0` | (none) | No listener, firewalled mode |
| `listen_port: 2234` | `--listen-port 0` | No listener (CLI override) |
| `listen_port: 2234` | `--listen-port 8080` | Binds `0.0.0.0:8080` |
| `listen_port: 2234` | `--listen-port 2234` | Binds `0.0.0.0:2234` (same as default) |

### Edge Cases

- **Port already in use**: The lib's `Listen::bind()` falls back to an ephemeral port and logs a warning. The actually-bound port is what gets advertised to the server. No seakarr-level handling needed.
- **Port 0 in config + no CLI override**: Seakarr sets `enable_listen: false` — no listener starts, firewalled mode (current behaviour).
- **Multiple seakarr instances on same machine**: Each will race for port 2234; losers get ephemeral ports. This is the lib's designed behaviour.

### Testing

1. **Unit test**: Verify `SoulseekConfig` default has `listen_port: 2234`
2. **Unit test**: Verify CLI override merges correctly
3. **Unit test**: Verify `listen_port: 0` sets `enable_listen: false`
4. **Integration test** (manual): User sets up port forward, runs seakarr, verifies log shows bound port and search results increase

### Out of Scope

- UPnP/NAT-PMP automatic port forwarding (future enhancement)
- Shared directories / upload serving (seakarr is download-only)
- Multiple listen ports
- IPv6 listener binding
