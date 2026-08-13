# Download Progress Display — Design Spec

Date: 2026-08-13
Status: Draft — pending user review

## Overview

Replace the noisy, raw-bytes download logging with human-friendly console
progress bars using the `indicatif` crate. One progress bar per active album
shows the current track's filename, downloaded/total size in MB, transfer speed,
a visual bar, and percentage. In non-interactive contexts (daemon, piped, CI),
progress bars are disabled and the existing log lines are reformatted to use
human-friendly units instead of raw bytes.

## User Requirements

1. One progress bar per concurrent album, showing the current track's progress.
2. Sizes displayed in MB (or KB for small files), speeds in KB/s or MB/s.
3. Progress bars render to stderr; all other tracing output stays on stdout.
4. Bridge InProgress log lines (raw `SsDownloadStatus` debug dumps) are
   suppressed when progress bars are active.
5. In non-interactive contexts (TTY check fails), no progress bars are created
   and the bridge logs use human-friendly formatting instead of raw bytes.
6. No new config options — TTY auto-detection is sufficient.

## Architecture

### New Modules

| Module | Responsibility |
| --- | --- |
| `src/formatting.rs` | `format_bytes(u64) -> String` and `format_speed(u64) -> String` — shared human-friendly formatting |
| `src/progress.rs` | `ProgressDisplay` wrapping `indicatif::MultiProgress`, bar lifecycle (create/update/finish), `is_interactive()` TTY check |

### Modified Files

| File | Change |
| --- | --- |
| `Cargo.toml` | Add `indicatif = "0.17"` dependency |
| `src/lib.rs` | Add `pub mod formatting; pub mod progress;` |
| `src/client.rs` | Bridge InProgress logs use `format_bytes`/`format_speed`; suppressed when progress bars active |
| `src/download.rs` | `download_album` creates `ProgressDisplay`, passes bar to each `download_file`; `download_file` updates bar on `InProgress` |
| `src/runner.rs` | `process_album` passes progress context through to `download_album` |

### Data Flow

```
bridge (client.rs) ──status_rx──▸ download_file (download.rs) ──▸ progress bar update
                                  download_album creates/finishes bars per track
```

The bridge continues emitting `DownloadStatus::InProgress` unchanged — only the
consumer changes. `download_file` receives a progress handle, updates the bar on
each `InProgress` status, and the bar auto-finishes when the track completes.

## Visual Design

### Progress Bar (interactive mode)

```
  ▸ 03. Sometimes You Cant Make It on Your Own.flac  10.1/31.8 MB  2.5 MB/s  [████████░░░░░░░░░░░░] 32%
```

Components (left to right):
- `▸` — active indicator (spinner character from indicatif)
- Filename — basename only, truncated if terminal is narrow
- `10.1/31.8 MB` — downloaded / total in human-friendly format
- `2.5 MB/s` — current transfer speed
- `[████████░░░░░░░░░░░░]` — visual progress bar (20 chars)
- `32%` — percentage complete

When a track finishes, the bar updates to the next track's filename and resets
to 0%. When the album completes all tracks, the bar is removed.

### Fallback Log Line (non-interactive mode)

```
Downloading: 03. Sometimes You Cant Make It on Your Own.flac — 10.1 MB / 31.8 MB @ 2.5 MB/s
```

Replaces the current raw debug output:
```
Bridge progress for track.flac: InProgress { bytes_downloaded: 10552744, total_bytes: 33304229, speed_bytes_per_sec: 2649609.6 }
```

## Formatting Utilities

### `format_bytes(bytes: u64) -> String`

| Range | Format | Example |
| --- | --- | --- |
| < 1024 | `{n} B` | `512 B` |
| < 1 MB | `{n:.1} KB` | `256.5 KB` |
| < 1 GB | `{n:.1} MB` | `31.8 MB` |
| >= 1 GB | `{n:.1} GB` | `1.2 GB` |

### `format_speed(bytes_per_sec: u64) -> String`

| Range | Format | Example |
| --- | --- | --- |
| < 1024 B/s | `{n} B/s` | `512 B/s` |
| < 1 MB/s | `{n:.1} KB/s` | `256.5 KB/s` |
| >= 1 MB/s | `{n:.1} MB/s` | `2.5 MB/s` |

Used by: progress bar template, bridge log messages, and potentially the run
summary in future.

## TTY Detection and Fallback

```rust
pub fn is_interactive() -> bool {
    std::io::IsTerminal::is_terminal(&std::io::stderr())
}
```

**When interactive (stderr is a TTY):**
- `ProgressDisplay` creates `indicatif::MultiProgress` rendering to stderr
- Bridge InProgress logs are suppressed (state transitions like started/completed/failed are kept)
- Album start/completion logged to stdout via tracing as before

**When non-interactive (daemon, piped, CI):**
- `ProgressDisplay` is a no-op (no bars created)
- Bridge logs use `format_bytes`/`format_speed` for human-friendly output
- All other behaviour unchanged

No config option — the TTY check is automatic and sufficient.

## Integration Points

### `download_album` Signature Change

```rust
pub async fn download_album(
    client: &dyn SoulseekClient,
    candidates: &[SearchResult],
    staging_dir: &Path,
    config: &DownloadConfig,
    filters: &crate::config::FilterConfig,
    progress: Option<&ProgressDisplay>,  // NEW parameter
) -> Result<Vec<PathBuf>>
```

When `progress` is Some, `download_album` creates a bar for each track via
`progress.create_bar(track_name, total_bytes)` and passes it to `download_file`.
When None, no bars are created.

### `download_file` Signature Change

```rust
pub async fn download_file(
    client: &dyn SoulseekClient,
    file: &FileInfo,
    username: &str,
    dir: &Path,
    config: &DownloadConfig,
    bar: Option<&ProgressBar>,  // NEW parameter
) -> Result<PathBuf>
```

On each `InProgress` status: if `bar` is Some, call `bar.set_position(bytes_downloaded)`
and `bar.set_prefix(format_speed(speed_bytes_per_sec))`. If None, no update.

### `process_album` Passes Progress Context

`process_album` receives `Option<&ProgressDisplay>` from the mode runner
(`run_auto_mode`, `run_manual_mode`) and forwards it to `download_album`.

The mode runner creates `ProgressDisplay` based on `is_interactive()`:
- `run_auto_mode`: one `ProgressDisplay` shared across all concurrent albums
- `run_manual_mode`: one `ProgressDisplay` for the single album
- `run_batch_mode`: one `ProgressDisplay` for the entire batch (bars recycle)

### Bridge Log Suppression

In `src/client.rs`, the bridge's InProgress logging block:

```rust
// Before:
tracing::info!("Bridge progress for {bridge_filename}: {status:?}");

// After (when progress bars active):
// Suppressed — progress bar handles display

// After (when non-interactive):
tracing::info!(
    "Downloading: {bridge_filename} — {} / {} @ {}",
    format_bytes(bytes_downloaded),
    format_bytes(total_bytes),
    format_speed(speed_bytes_per_sec),
);
```

The bridge needs access to the `is_interactive()` flag. This can be passed as a
boolean at client construction time or checked once at startup.

## Testing

- `formatting.rs`: unit tests for `format_bytes` and `format_speed` covering
  all range boundaries (0, 1023, 1024, 999999, 1000000, 1GB+).
- `progress.rs`: test `is_interactive()` returns false in test context. Test
  bar creation/update/finish lifecycle.
- `download.rs`: existing tests pass unchanged (progress parameter is `None`
  in tests). Add test verifying human-friendly format appears in bridge logs.
- `client.rs`: test that bridge log format uses format_bytes/format_speed.

## Out of Scope

- ETA display (estimated time remaining) — could be added later as indicatif
  supports it natively.
- Album-level aggregate progress (e.g., "3/10 tracks done") — the run summary
  already covers this post-download.
- Colour themes — indicatif's defaults are fine; custom themes can be added later.
- Config options for progress bar style — YAGNI until users request it.
