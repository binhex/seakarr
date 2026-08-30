# Peer reputation — design

Date: 2026-08-30
Status: approved

## Overview

Rank Soulseek search results using a persisted per-peer reputation instead of
trusting the peer's self-reported upload speed alone. Peers that served clean,
fast downloads rise; peers that error or throttle sink. The ranking is
query-agnostic: a peer's reputation applies to every search it matches, so a
proven-good peer is preferred when it happens to have the album, and otherwise
ignored.

This evolves and replaces the previous per-artist `prefer_reliable_peer` map.

## Motivation

Advertised upload speeds are unreliable: peers throttle or misreport them, and
observed download speeds vary enormously. Real, measured download speed is the
only trustworthy speed signal. Reliability (does the peer actually complete
tracks without errors/retries) is a second, independent signal the current
ranking ignores entirely. Both signals are only observable after we have
downloaded from a peer, so they must be remembered across runs to compound.

The codebase already contains a dormant `peer_reputation` SQLite table and the
`Database::update_peer_reputation` / `get_preferred_peers` methods — this design
wires them in rather than building from scratch.

## Behaviour

- On every album search, rank candidates using a blend of advertised and
  measured speed, plus a bounded reliability factor derived from the peer's
  recorded success rate.
- After each track transfer completes (or fails after retries), record the
  peer's measured speed and success/failure into the reputation store.
- Unknown peers (no record) rank by advertised speed with a neutral factor.
- Peers with few samples are pulled toward neutral so one good/bad download
  cannot dominate.
- A config toggle gates the whole feature; when off, ranking and recording both
  fall back to today's advertised-speed behaviour.

## Components

1. **DB layer (reuse)** — keep the existing `peer_reputation` table
   (`username, total_downloads, successful, avg_speed_kbps, last_seen,
   preferred`) and `update_peer_reputation(username, speed_kbps, success)`.
   Add `get_reputation_map()` returning a `HashMap<username, PeerReputation>`
   for O(1) lookups. The unused `preferred` column is left vestigial.

2. **Download plumbing** — surface each track's final smoothed speed (the
   existing `speed_ema`) out of the download path. `download_album` returns, in
   addition to the downloaded paths, per-track outcomes (success and measured
   `kbps`).

3. **Runner recording** — after `download_album`, `process_album` writes each
   track's `(username, speed_kbps, success)` to the store via
   `update_peer_reputation`. A DB write failure is logged and ignored; it never
   fails the album.

4. **Ranking** — `rank_candidates` accepts the reputation map and computes:

   - `effective_speed` = advertised `speed` blended with `avg_speed_kbps`,
     weighted toward the measured value as `total_downloads` grows.
   - `reliability` = `successful / total_downloads`, smoothed toward neutral
     for `total_downloads < 3`.
   - `reliability_factor` = bounded map of `reliability` into `[0.7, 1.3]`,
     centred on 1.0 at neutral (unknown peer or 50/50 history).
   - `score = effective_speed × slot_bonus × bitrate_bonus × album_bonus × reliability_factor`.

5. **Config** — `search.peer_reputation: bool` (default `true`) replaces
   `search.prefer_reliable_peer`. When `false`, the runner neither reads
   reputation in ranking nor writes it after downloads.

6. **Removal** — delete `ReliablePeers` (`src/reliable.rs`), `promote_peer`
   (`src/search.rs`), and the runner's per-artist record/evict logic. Repurpose
   `DownloadStats` to carry per-track success and measured speed.

## Data flow

```
search -> rank_candidates(reputation map) -> ranked candidates
       -> download_album (per-track measured speed + success)
       -> process_album records each track into peer_reputation
```

## Error handling

- Unknown peer: neutral (advertised speed, factor 1.0).
- Few samples (`total_downloads < 3`): reliability pulled toward neutral.
- Reputation read/write failure: warn and continue; downloads are never
  blocked or failed by reputation bookkeeping.
- `min_upload_speed_kbps` (existing hard floor) is unchanged; reputation governs
  the soft band above the floor.

## Testing

- Unit (`filter.rs`): ranking with reputation — a high-success peer is bumped,
  an error-prone peer is demoted, an unknown peer is neutral, the
  small-sample smoothing works, and the reliability factor is bounded.
- Unit (`db.rs`): `update_peer_reputation` already tested; add speed-averaging
  and success-rate derivation assertions.
- E2E (`runner.rs`): a peer with recorded history ranks first on a subsequent
  search; `peer_reputation: false` disables both ranking reads and recording.
- Config: `search.peer_reputation` defaults true, explicit false deserialises.

## Documentation

- README: replace the per-artist feature bullet and `search.prefer_reliable_peer`
  row with the peer-reputation feature and `search.peer_reputation` (default
  `true`), including the "measured speed + reliability, persisted in SQLite"
  behaviour.
- Generated config template (`config.rs`): add `peer_reputation: true` under
  `search:` and drop `prefer_reliable_peer`.
- Existing `configs/seakarr.yml` files keep working: the removed key is ignored
  by serde and the new key defaults to `true`.
