# End-of-Run Summary Reporting — Design Spec

Date: 2026-08-13
Status: Approved

## Overview

Add a summary report printed to the console at the end of every run. The report
lists, by artist and album name (no track-level detail), which albums were
downloaded, which were skipped, and which failed to download. It applies to all
modes (auto, manual, batch) and to each daemon cycle.

## User Requirements (agreed)

1. Output goes to the console log only (existing `tracing` output). No report
   files, no Apprise notification changes.
2. All modes print the summary: auto, manual, batch, and daemon (per cycle).
3. Three flat sections: Downloaded, Skipped, Failed. Each entry is
   `Artist — Album`. No sub-categorisation of skips by reason.
4. Failed entries include a short reason alongside the name. No track-level
   detail anywhere.

## Data Source: In-Memory Run Report

The report is built in memory during the run. Rationale: the
`processed_albums` table mixes runs together (it is history), `last_error` is
never written today, and the "already processed" early return never touches the
table, so a DB query cannot reconstruct the per-run truth without schema and
write-path changes. An in-memory report is precise, requires no DB changes, and
the formatting is trivially unit-testable.

## New Module: `src/report.rs`

```rust
pub enum AlbumOutcome {
    Downloaded { track_count: usize },
    Skipped,
    Failed { reason: String },
}

pub struct RunReport {
    downloaded: Vec<(String, String)>, // (artist, album)
    skipped: Vec<(String, String)>,    // (artist, album)
    failed: Vec<(String, String, String)>, // (artist, album, reason)
}
```

- `RunReport::new()`, plus `record(&mut self, artist, album, outcome)` helper
  that appends to the right section.
- `print_summary(&self)` logs via `tracing::info!`. Header plus non-empty
  sections only, entries in processing order:

```text
=== Run summary ===
Downloaded (3):
  Artist A — Album 1
Skipped (2):
  Artist B — Album 2
Failed (1):
  Artist C — Album 3 (all candidates exhausted)
```

- Empty sections are omitted. If no outcomes were recorded at all, no summary
  is printed.
- Ordering is deterministic: entries are appended in completion order; auto
  mode's `join_all` preserves target order.

## `process_album` Contract Change

Return type changes from `Result<()>` to `Result<AlbumOutcome>`.

| Situation | Current behaviour | New behaviour |
| --- | --- | --- |
| Already processed | log + `Ok(())` | `Ok(Skipped)` |
| No search results | mark DB `skipped` + `Ok(())` | mark DB `skipped` + `Ok(Skipped)` |
| All results rejected by filters (incl. contiguity gate, failed second-chance fallback) | mark DB `skipped` + `Ok(())` | mark DB `skipped` + `Ok(Skipped)` |
| Download failed (all candidates exhausted) | `Err` | `Ok(Failed { reason })` |
| Download ok, organize failed | mark DB `failed` + `Err` | mark DB `failed` + `Ok(Failed { reason })` |
| Success | mark DB `success`, notify, `Ok(())` | mark DB `success`, notify, `Ok(Downloaded { track_count })` |

- `Err` is reserved for environment errors (staging-directory creation
  failure, DB write failures) plus the unchanged notify-failure propagation.
- Notify-failure behaviour is unchanged: the error still propagates, and the
  mode runner records it as a `Failed` entry (so the album appears in the
  summary's Failed section).
- Reasons are short strings derived from the existing error messages
  (e.g. `all candidates exhausted`, `download succeeded but file organization
  failed`).

## Mode Runner Changes

### Auto (`runner::run_auto_mode`)

- Collect each result of the `join_all` into a `RunReport`: `Ok(outcome)` is
  recorded directly; `Err(e)` is recorded as `Failed` with the error string.
- Replace the existing error-only loop with report collection, then call
  `print_summary` once at the end.
- Keep the existing "Found X albums to upgrade out of Y total" line. Albums
  that do not need upgrading are not part of the run and do not appear in the
  report.

### Daemon

No change needed: each cycle calls `run_auto_mode`, which prints the summary.

### Batch (`main::run_batch_mode`)

- Replace the `succeeded`/`failed` counters with a `RunReport`.
- The existing numeric line mis-counts skips as successes and is replaced by
  the summary.

### Manual (`runner::run_manual_mode`)

- Single album: print the summary after `process_album` returns.
- Artist-only runs (no `--album`) display the album as `(all)`, matching the
  existing log convention.

## Out of Scope (YAGNI)

- No DB schema or write-path changes (`processed_albums` marking stays as is).
- No report files, no notification changes, no config options.
- No skip sub-reasons, no track-level detail.
- No change to per-album notification behaviour.

## Testing

- `report.rs` unit tests: section rendering, counts, ordering, omission of
  empty sections, and the no-outcomes case.
- Runner tests updated for the new return type: assert
  `Downloaded`/`Skipped`/`Failed` variants for each existing scenario
  (fallback success, fallback no-match, gappy tracks, already processed).
- Batch-mode path tested via the existing integration coverage; assert the
  summary contents where practical.
