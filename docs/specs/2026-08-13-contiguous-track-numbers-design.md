# Seakarr — Contiguous Track-Number Check at the Search-Result Stage: Design Spec

**Date:** 2026-08-13
**Status:** Approved
**Context:** Seakarr currently downloads whatever files pass its quality filters, with no completeness
check. Production observations: an album for `The Cure — The Cure` was marked `Completed` with only 3
tracks (numbers 04, 08, 16 — clearly incomplete), and a `Linkin Park — Hybrid Theory` transfer failed
mid-album without a completeness gate. This spec adds a contiguity check at the search-result stage:
any result whose downloadable track set has gaps is discounted before ranking. Duplicate track
numbers are permitted; missing track numbers are not.

---

## 1. Behaviour

When `filters.contiguous_tracks` is enabled (default `true`), a search result passes
`filter_results` only if **both**:

1. At least one file passes the existing quality filters (extension, bitrate, exclude-words, free
   slots) — unchanged behaviour, and
2. The **set of track numbers** parsed from the **filter-passing files** is **contiguous**: the
   sorted unique numbers have no gaps. Duplicates are permitted (tracks 1, 2, 2, 3 pass); gaps are
   not (1, 3, 4 fails; 04, 08, 16 fails).

Locked-in rules:

- Track numbers are 1–3 digit all-numeric tokens, either leading (`04_Cure for Me.flac`) or anywhere
  in the filename (`Linkin Park - Hybrid Theory - 11 - Cure for the Itch.flac`). Four-digit tokens
  are ignored (years, e.g. `2000`).
- A result where **no** filter-passing file yields a number is **rejected** — contiguity cannot be
  verified, so the result is not trusted.
- The check runs over **filter-passing files only** — the same set `download_album` would actually
  download, so a full-looking listing with non-downloadable tracks (e.g. mp3 tracks among flacs)
  cannot pass on the strength of files that will never be fetched.
- The check applies to **both primary and fallback** results, since both flow through
  `filter_results`. Opt-out via config.

Scope notes:

- Contiguity means "no gaps" — a starting number other than 1 is accepted (`{5, 6, 7, 8}` passes).
- Multi-disc numbering (`1-01`, `2-03`) is out of scope and may cause rejection; such collections
  should set `contiguous_tracks: false`.
- Transfer-time failures (the Hybrid Theory case) are already handled by `download_album`'s
  per-candidate all-or-nothing behaviour with cleanup and candidate fallback; this spec does not
  change that.

## 2. Components

### New module `src/tracks.rs` (pure, no I/O, no async)

- `pub fn track_number_from_filename(name: &str) -> Option<u32>` — strip any directory prefix, then
  split the basename on non-alphanumeric boundaries; return the number of the **first** 1–3 digit
  all-numeric token, `None` if there is none. The first-token rule keeps multi-number names such as
  `1-01` deterministic (returns 1).
- `pub fn files_have_contiguous_tracks(files: &[&FileInfo]) -> bool` — collect the parsed numbers;
  return `false` if none; sort unique values and verify no gaps between consecutive values.

### `src/filter.rs` (changes)

- `filter_results` gains a second condition in its predicate: when `config.contiguous_tracks` is
  true, `tracks::files_have_contiguous_tracks` must hold over the filter-passing files of that
  result. `file_passes_filters` and `rank_candidates` are unchanged.

### `src/config.rs` (changes)

- `FilterConfig` gains `contiguous_tracks: bool` with `#[serde(default = "default_true")]` (existing
  configs keep the feature enabled).
- Added to the manual `Default` impl and to the test helper `sample_yaml()`. The auto-generated
  default config file picks the key up automatically via serialisation of `Config::default()`.

### `src/lib.rs` (changes)

- Register `pub mod tracks;`.

### `README.md` (changes)

- Document the new key in the `filters:` table.

No changes to `download.rs`, `runner.rs`, or the search/fallback flow.

## 3. Data flow

```text
runner.process_album
  → filter::filter_results(results, config)
      per result:
        passing = files where file_passes_filters(f, config)
        if passing.is_empty() → reject
        if config.contiguous_tracks && !tracks::files_have_contiguous_tracks(&passing) → reject
  → rank_candidates → download_album   [unchanged]
```

The reported failure is fixed at this stage: `weirdpossum`'s The-Cure result (tracks 04, 08, 16 —
gaps) is rejected before ranking, so no incomplete album is marked completed. A Hybrid-Theory
result listing all 12 tracks still passes; transfer failures during download remain handled by the
existing all-or-nothing candidate fallback.

## 4. Error handling

- Parsing is total: malformed filenames return `None` (never panic). A file with no number
  contributes nothing to the set; a result whose entire filter-passing set is unnumbered is
  rejected by the no-numbers rule.
- No new error variants. Rejection simply removes the result from the filtered list; the existing
  "0 passed filters" logging covers it (wording updated to mention the contiguity requirement when
  enabled).

## 5. Config surface

One new key, no CLI changes:

```yaml
filters:
  contiguous_tracks: true   # default; set false to disable the contiguity check
```

## 6. Testing

- **Unit (`tracks.rs`):** `track_number_from_filename` — leading token (`04_Cure for Me.flac` → 4);
  mid-filename token (`Linkin Park - Hybrid Theory - 11 - Cure for the Itch.flac` → 11);
  zero-padded leading token (`08_the cure.flac` → 8); four-digit year ignored
  (`Hybrid Theory (2000) - 01 - Papercut.flac` → 1, not 2000); no number → `None`; token containing
  letters (`Track 4a`) → `None`; directory prefix stripped.
- **Unit (`tracks.rs`):** `files_have_contiguous_tracks` — contiguous 1..N passes; duplicates pass
  (1, 2, 2, 3); a gap fails (1, 3, 4; and the production case 04, 08, 16); a single track passes;
  empty/unnumbered set fails.
- **Unit (`filter.rs`):** `filter_results` — a result whose filter-passing files are non-contiguous
  is rejected when the toggle is on; accepted when the toggle is off; unnumbered results rejected
  when on, accepted when off.
- **Config:** `contiguous_tracks` defaults to `true` and round-trips YAML.
- **Integration (`runner.rs`):** a fallback result with track gaps leads to `skipped`; a contiguous
  fallback result proceeds to download. Extends the existing fallback integration tests.

## 7. Out of scope

- Multi-disc numbering (`1-01`, `2-03`) and disc-number parsing.
- Requiring the track set to start at 1.
- Completeness checks at download time (beyond the existing all-or-nothing candidate fallback).
- Album-duration or track-count heuristics from tag metadata.
