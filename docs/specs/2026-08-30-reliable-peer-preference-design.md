# Reliable-peer preference — design

Date: 2026-08-30
Status: approved

## Overview

When an album downloads cleanly from a peer, remember that peer as the
"reliable peer" for the album's artist. On later searches for the same artist,
prefer that peer's results so the downloader tries the proven-good source
first. Falls back transparently to the normal ranked results when the peer is
offline or doesn't have the album.

The feature is toggleable via config and **on by default**.

## Motivation

A peer that served a clean download is likely to serve other albums by the
same artist reliably. Preferring it first biases ranking toward a known-good
source without any new protocol surface: the existing network-wide search
already returns that peer's results when it is online and matches; we just
move those results to the front of the ranked candidate list.

## Behaviour

- Maintain an in-memory, per-run map `reliable_peers: artist → peer username`.
- Record `(artist, peer)` only after an album by that artist downloads
  **cleanly**: first candidate, first attempt, zero retries, zero failures.
- On each album search, when the feature is enabled and the artist has a
  recorded peer:
  - log `Searching <artist> — <album>, preferring reliable peer <username>...`
    before the search runs;
  - after `filter::filter_results` ranks the candidates, stable-partition the
    list so that peer's entries come first, and the downloader tries them
    first.
- If a preferred peer later fails or needs a retry, evict the artist's entry
  so a now-unreliable peer is no longer preferred.
- When the feature is disabled, none of this happens: no recording, no
  promotion, no special logging.

## Scope and non-goals

- The map is **per-run** and not persisted to SQLite. Peer reliability changes
  quickly; persistence is out of scope.
- "Reliable" is download-level only. Search strategies (e.g. the title-search
  fallback) are normal search behaviour, not "failures", and do not prevent
  recording.
- No new Soulseek protocol capability is added. The network-wide search is
  unchanged; this feature only reorders its results.

## Components

1. **`ReliablePeers`** — a small type wrapping `Mutex<HashMap<String, String>>`
   with `record(artist, peer)`, `get(artist)`, and `evict(artist)`. One
   instance is created per scan cycle and shared by all concurrently-processed
   album futures. The lock is never held across `.await`.
2. **Config** — `SearchConfig.prefer_reliable_peer: bool` with
   `#[serde(default)]` set to `true`. Existing `configs/seakarr.yml` files keep
   working unchanged: serde applies the default when the key is absent.
3. **Download signal** — `download_album` additionally reports whether the
   download succeeded with zero retries and a single candidate. `process_album`
   uses that to decide `record` vs `evict`.
4. **Promotion** — in `process_album`, after ranking, stable-partition the
   ranked results so the reliable peer's entries come first.
5. **Logging** — one `info!` line for "preferring" at search time, one for
   eviction on a failed/retried preferred download.

## Data flow

1. `process_album(artist, album)` begins; it looks up `reliable_peers.get(artist)`.
2. If present and enabled, it logs the "preferring" line, then searches as
   normal.
3. `filter::filter_results` ranks candidates; the promotion step moves the
   reliable peer's entries to the front.
4. `download_album` tries candidates in order and returns success plus the
   reliability facts (retries used, candidates tried).
5. On clean success → `record(artist, peer)`. On retry/failure involving the
   preferred peer → `evict(artist)`.

## Error handling

- Lock poisoning on the `Mutex` is recovered via `into_inner()` (consistent
  with the existing codebase).
- A missing/expired peer entry is a no-op: the search simply proceeds normally.
- Eviction only fires when a preferred peer actually failed or retried; other
  download failures leave the map untouched.

## Testing

- Unit: `ReliablePeers` record/get/evict; stable promotion reorder (peer-first,
  relative order of the rest preserved); config default is `true` and an
  explicit `false` deserializes.
- `process_album` (existing mock-client harness): a clean success records the
  peer and a subsequent same-artist search promotes it; a retried or failed
  download does not record and evicts a previously-recorded peer; a disabled
  toggle performs no recording/promotion/logging.

## Documentation

- README: a bullet describing the feature and the `search.prefer_reliable_peer`
  config key (default `true`).
- No changes to `configs/seakarr.yml` (credentials file; the serde default
  covers the new key).
