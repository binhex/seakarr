# Library Track Count Check — Design Spec

## Problem

When seakarr searches for an album upgrade in auto mode, it accepts any peer whose files pass the quality filters — even if the peer shares fewer tracks than the library already has. This means a library with a complete 12-track album could be "upgraded" to a peer's 3-track subset. The download completes, files are organized, and the album is marked as processed — effectively a downgrade.

## Solution

Add a new `peer_track_count` filter that compares the peer's filtered file count against the library's track count for the same album. If the peer has fewer usable tracks, the result is rejected and seakarr moves to the next peer.

## Scope

- Auto mode only (the scanner runs and track counts are known)
- Batch and manual mode are unaffected — the check is skipped when the library track count is unknown
- Applies to both primary search and fallback album-only search results
- Works alongside existing `min_tracks` and `contiguity` gates (additive, not replacing)

## Design

### Config

New boolean field in `FilterConfig`:

```yaml
# seakarr.yml
filters:
  peer_track_count: true   # default: true (enabled)
```

- `true` (default): reject results where the peer's filtered file count < library track count
- `false`: disable the check

### Data Flow

The library track count flows from the scanner to the filter:

1. `scanner::find_albums_to_upgrade` — currently returns `Vec<(String, String)>` (artist, album). Change to `Vec<(String, String, usize)>` — third element is `ScannedAlbum.track_count`.

2. `run_auto_mode` — receives the triple, passes `track_count` to `process_album` as a new parameter.

3. `process_album` — new parameter `library_track_count: Option<usize>`. Passes it through to `filter_results`.

4. `filter_results` — new parameter `library_track_count: Option<usize>`. When `Some(n)` and `config.peer_track_count` is `true`, applies the check. When `None` (batch/manual mode), the check is skipped.

The `Option<usize>` design means batch/manual callers pass `None` and the check is automatically skipped — no mode-conditional logic needed.

### Filter Logic

In `filter_results`, after the existing `min_tracks` and `contiguity` checks, add:

```rust
// Library track count check (auto mode only).
// Rejects results where the peer has fewer usable tracks than the library
// already has — accepting such a result would be a downgrade.
if let Some(lib_count) = library_track_count {
    if config.peer_track_count && passing_count < lib_count {
        tracing::debug!(
            "result from {} rejected: {} filtered tracks < library track count {}",
            r.username, passing_count, lib_count
        );
        return false;
    }
}
```

`passing_count` is the count of files passing existing filters (already computed for the `min_tracks` check). The check is a simple `<` comparison — peer count must be >= library count.

### Interaction with Existing Gates

Both `min_tracks` and `peer_track_count` apply:

| `min_tracks` | `peer_track_count` | Library has 10 tracks | Peer has 7 filtered files | Result |
|---|---|---|---|---|
| 5 | true | 10 | 7 | REJECT (7 < 10) |
| 5 | false | 10 | 7 | ACCEPT (7 >= 5) |
| 5 | true | 10 | 12 | ACCEPT (12 >= 10, 12 >= 5) |
| 15 | true | 10 | 12 | REJECT (12 < 15, min_tracks) |
| 5 | true | 10 | 5 | REJECT (5 < 10) |
| 0 | true | 10 | 5 | REJECT (5 < 10) |

### Edge Cases

- **Library has 0 tracks (new album):** `0 <= peer_count` always true — check passes. Correct: no downgrade possible.
- **Album has no filtered files:** The `min_tracks` check (with `min_tracks.max(1)`) already rejects these. The `peer_track_count` check is never reached.
- **Fallback search results:** The check applies to fallback results too — they go through `filter_results` with the same `library_track_count`.
- **Batch/manual mode:** `library_track_count` is `None` — check skipped automatically.

## Files Changed

| File | Change |
|---|---|
| `src/config.rs` | Add `peer_track_count: bool` to `FilterConfig` with serde default `true` |
| `src/scanner.rs` | `find_albums_to_upgrade` returns `Vec<(String, String, usize)>` |
| `src/runner.rs` | `process_album` takes `library_track_count: Option<usize>`; passes to `filter_results`; `run_auto_mode` unpacks the new triple |
| `src/filter.rs` | `filter_results` takes `library_track_count: Option<usize>`; adds the check after existing gates |
| `tests/pipeline_test.rs` | Update `process_album` call sites (pass `None` for manual-mode tests) |

## Tests

| Test | What it verifies |
|---|---|
| `test_peer_track_count_rejects_lesser` | Peer has 3 filtered files, library has 5 → result rejected |
| `test_peer_track_count_accepts_equal` | Peer has 5 filtered files, library has 5 → result passes |
| `test_peer_track_count_accepts_greater` | Peer has 7 filtered files, library has 5 → result passes |
| `test_peer_track_count_disabled` | `peer_track_count: false` → result with 3 files passes even though library has 5 |
| `test_peer_track_count_none_skips` | `library_track_count: None` (batch/manual) → check skipped, result passes |

## Non-Goals

- No changes to daemon mode behavior (daemon uses run_auto_mode, which will get the check automatically)
- No changes to the search or download logic — this is purely a filter-stage check
- No DB schema changes — the library track count is transient (from the scanner, not persisted)
- No config migration needed — serde default `true` means existing configs get the new behavior automatically
