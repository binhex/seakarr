# Missing Bitrate/Bitdepth Download-Then-Verify Fallback

**Date:** 2026-08-19
**Status:** Approved
**Author:** seakarr development

## Problem

When `min_bitrate` is set in the config and a Soulseek peer doesn't provide
bitrate metadata for their shared files, seakarr currently rejects those files
at the search level — even if the files are actually high quality. This means
users miss out on potentially good audio from peers whose clients don't report
file attributes.

The same issue applies to `min_bitdepth` for lossless files, which is currently
parsed but never enforced.

## Goal

Change the behavior so that files with missing bitrate/bitdepth metadata are
downloaded and verified post-download, rather than rejected at search time.
This maximizes the pool of available audio while still enforcing quality
requirements.

## Design

### Scope

Both `min_bitrate` and `min_bitdepth` get the download-then-verify fallback.
No new config options are added — the behavior is always enabled when the
corresponding config is set.

### Format-specific metrics

- **`min_bitrate`** applies only to **lossy** files (MP3, OGG, AAC, WMA, Opus, etc.)
- **`min_bitdepth`** applies only to **lossless** files (FLAC, ALAC, WAV, etc.)

Each format type has its own quality metric. A lossy file is never checked
against `min_bitdepth`, and a lossless file is never checked against
`min_bitrate`.

### Filter changes (`src/filter.rs`)

**Current behavior:** `file_passes_filters()` rejects files with missing
bitrate when `min_bitrate` is set (`attribs.get(&0)` returns `None` → reject).

**New behavior:** When `min_bitrate` is set and `file.attribs.get(&0)` returns
`None`, the file is NOT rejected — it passes the filter. Same for `min_bitdepth`
and `file.attribs.get(&5)`.

The filter still rejects files where the peer DOES provide bitrate/bitdepth and
it's below the minimum (existing behavior, unchanged).

**Ranking:** No change needed. `rank_candidates()` already gives no bitrate
bonus to files with missing bitrate (`filter.rs:323-338`). Peers with
known-good bitrate are preferred, and missing-bitrate peers are tried as
fallbacks.

### Download changes (`src/download.rs`)

**Current behavior:** `download_file()` downloads a file and returns
success/failure. No post-download quality verification.

**New behavior:** After each file completes, `download_file()` verifies
bitrate/bitdepth using the existing `file_quality_score()` function
(`organizer.rs:331-345`) which reads actual file metadata via the `lofty` crate.

**Verification logic (per-track, after download completes):**

```
if min_bitrate is set:
    if file is lossy:
        if peer didn't provide bitrate (attribs.get(&0) is None):
            read actual bitrate via lofty
            if actual_bitrate < min_bitrate:
                delete downloaded file
                return error "bitrate below minimum"
        else:
            # already checked pre-download, no action
    else:
        # lossless file, min_bitrate doesn't apply

if min_bitdepth is set:
    if file is lossless:
        if peer didn't provide bitdepth (attribs.get(&5) is None):
            read actual bitdepth via lofty
            if actual_bitdepth < min_bitdepth:
                delete downloaded file
                return error "bitdepth below minimum"
        else:
            # already checked pre-download, no action
    else:
        # lossy file, min_bitdepth doesn't apply
```

**Fail-fast behavior:** Verification happens after EACH track download. If any
track fails, the download stops immediately, the staging directory is cleaned
up (existing cleanup logic in `download_album()`), and the next candidate peer
is tried. This minimizes wasted bandwidth.

### Fallback integration

The error from `download_file()` triggers the existing fallback logic in
`download_album()` (`download.rs:530-545`):
1. Current candidate is marked as failed
2. Staging directory is cleaned up
3. Next ranked candidate is tried

No changes needed to the fallback logic — the verification error is treated
the same as any other download failure.

### Config changes (`src/config.rs`)

- `min_bitdepth` is now enforced via the download-then-verify fallback. It was
  already parsed and defaulted (`config.rs:103`), but never enforced
  (`filter.rs:36-37` says "defined but not yet enforced"). No code change
  needed — the enforcement comes from the download verification.

### Verification function (`src/organizer.rs`)

Reuse the existing `file_quality_score()` function (`organizer.rs:331-345`)
which reads actual file metadata using `lofty`:
- For lossless files: reads `props.bit_depth()` and `props.sample_rate()`
- For lossy files: reads `props.audio_bitrate()`

A helper function extracts the raw bitrate/bitdepth from the score for
comparison against the config minimum.

### Edge cases

| Scenario | Handling |
|----------|----------|
| Peer provides bitrate, it's below min | Rejected at search level (existing behavior, unchanged) |
| Peer provides bitrate, it meets min | Accepted at search level (existing behavior, unchanged) |
| Peer doesn't provide bitrate, min_bitrate set | Passed at search level, downloaded, verified post-download |
| Peer doesn't provide bitrate, min_bitrate NOT set | Passed at search level, no verification (existing behavior) |
| Peer doesn't provide bitdepth, min_bitdepth set | Passed at search level, downloaded, verified post-download |
| Peer doesn't provide bitdepth, min_bitdepth NOT set | Passed at search level, no verification (existing behavior) |
| File format unknown (can't parse) | Skip verification (pass through — can't verify what we can't parse) |
| Both min_bitrate and min_bitdepth set | Check both: lossy files check bitrate, lossless check bitdepth |
| File is lossless, min_bitrate set | Don't check min_bitrate for lossless files (separate metrics) |
| File is lossy, min_bitdepth set | Don't check min_bitdepth for lossy files (separate metrics) |

### Testing strategy

**Unit tests (in `src/download.rs`):**
- `test_download_verifies_bitrate_for_missing_metadata`: Mock peer that
  doesn't provide bitrate. Download file with low actual bitrate (<
  min_bitrate). Verify download fails with appropriate error.
- `test_download_verifies_bitdepth_for_missing_metadata`: Same for bitdepth
  with lossless file.
- `test_download_skips_verification_when_peer_provides_metadata`: Mock peer
  that provides bitrate. Verify no post-download verification (pre-download
  filter already checked).
- `test_download_skips_verification_when_min_not_set`: No min_bitrate
  configured. Verify no verification happens.
- `test_download_fails_fast_on_bitrate_failure`: Download album with multiple
  tracks. First track fails verification. Verify download stops immediately
  (remaining tracks not downloaded).

**Integration tests (in `tests/pipeline_test.rs`):**
- `test_pipeline_rejects_low_bitrate_after_download`: End-to-end test with
  mock client. Peer doesn't provide bitrate. Download completes, verification
  fails, album discarded, next peer tried.

## Non-goals

- No new config options (behavior always enabled when min_bitrate/min_bitdepth
  is set)
- No changes to the search/ranking logic (missing bitrate = lower rank,
  existing behavior)
- No changes to the completeness gate (verification happens before completeness
  check)
- No changes to the library upgrade flow (verification happens during download,
  before library copy)
