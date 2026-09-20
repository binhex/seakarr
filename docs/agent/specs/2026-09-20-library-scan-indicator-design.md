# Library scan indicator (interactive spinner)

**Date:** 2026-09-20
**Status:** approved design; implementation not started

## Goal

Keep the library scan's log contract exactly as it is for headless runs, and
give an interactive terminal a spinner that updates one line in place while the
walk runs, in the same style as the download queue spinner.

## What the scan reports today

The walk reports itself in three ways (README, "Library scan log lines"):

- `Library scan starting: N root(s): <roots>` when the walk starts;
- `Library scan still running: F audio file(s), A album(s) (Ss elapsed)` at
  INFO once a minute while it runs;
- `Library scan complete: F audio file(s), A album(s), U unreadable file(s) in
  S.Ss` when it ends, or `Library scan cancelled by user after F audio file(s)`
  when Ctrl+C interrupted it.

Two DEBUG lines add detail. The per-minute line exists because the scan is the
longest silent phase of a run, and it is a documented contract with tests pinned
to its cadence.

On a terminal the result is still sparse: one line a minute scrolls past. The
operator asked for the treatment the queue already gets from `progress.rs` - a
spinner that updates a single line in place.

## Decisions taken before implementation

- The indicator shows **files first, then albums**, because files are the walk's
  work unit and increment fastest (about 82 a second on the reported library,
  against about 7 albums a second), so a stalled read shows up sooner.
- The console shows the **spinner instead of** the per-minute line, while the
  **log file keeps** that line unchanged.
- The indicator covers **every library scan**: auto mode's startup scan, a
  discover run's scan, and an artist-only manual run's presence scan.
- The indicator is **owned by the scan and driven through a narrow callback**, so
  the renderer stays out of the walk and a run keeps one terminal owner.

## Design

### The indicator belongs to the scan, driven through a narrow seam

`scanner::scan_library` gains one optional parameter:

```rust
progress: Option<&dyn ScanProgress>
```

with

```rust
pub trait ScanProgress {
    fn update(&self, files: usize, albums: usize, elapsed: Duration);
    fn finish(&self);
}
```

`finish` is the walk's release signal, and it exists because the closing info
line must be logged *after* the renderer has released the console filter;
relying on `Drop` alone would order that wrongly, since the indicator outlives
the call in the caller. It is idempotent, so `Drop` can call it too.

The walk already carries `files_seen`, `albums` and its start instant, so the
callback adds no accounting. It is called from the same per-entry point as the
existing debug cadence, throttled to at most once a second, and the initial
message is set when the bar is created so the line is on screen from the first
moment of the scan. The scanner keeps no knowledge of indicatif, TTY detection,
or log filtering: it reports numbers, and the renderer decides what to draw.
Existing call sites pass `None`.

### `progress.rs` renders the bar, like the queue bar

`ProgressDisplay` gains a trio mirroring the queue-bar trio:

- `create_scan_bar()` - a spinner created with the queue bar's exact template
  `"  {spinner} {msg}"` and tick set, with `enable_steady_tick(120ms)`;
- `update_scan_bar(&bar, message)`;
- `clear_scan_bar(bar)` - `finish_and_clear()`.

The tick set is the queue's Braille rotation; "same as the queue" is the
requirement, so this is not literal ASCII.

Monotonic counters `scan_bars_created`, `scan_bars_finished` and
`scan_bars_updated` make the "exactly one bar, always released" contract
observable and testable without a TTY, exactly as the queue counters do.

### The console heartbeat is suppressed, not removed

An interactive run shows the spinner instead of the per-minute line; the log
file keeps the line. That needs a per-layer filter: `main.rs` adds a reloadable
filter to the stdout layer only (`tracing_subscriber::reload`), leaving the
registry `EnvFilter` and the file layer as they are. The handle is installed in
a process-wide slot; where no handle is installed (tests, library use)
suppression is a no-op.

While the spinner is live the stdout layer drops the `seakarr::scanner` target,
which is where the heartbeat is emitted. The start line is logged before the
indicator starts and the complete and cancel lines after it is released, so all
three still reach the console:

```text
INFO seakarr::scanner: Library scan starting: 1 root(s): /mnt/user/Music/Paul/Albums/
⠸ Scanning library: 15553 audio file(s), 1220 album(s) (180s elapsed)
INFO seakarr::scanner: Library scan complete: 24120 audio file(s),
1893 album(s), 0 unreadable file(s) in 291.4s
```

Trade-off: suppression is target-wide for the walk's duration, so a
scanner-level WARN raised during the walk would be console-suppressed while
still reaching the log file, and console lines from *other* modules are not
suppressed at all - they print above the spinner, which redraws on its next
tick, exactly as download bars live with INFO lines today. No scanner-level WARN
exists today; unreadable files are named at DEBUG, and the log file keeps the
record either way.

### One display per run

Both call sites create their `ProgressDisplay` after the scan today, so the scan
would otherwise need a second terminal owner. Instead the display is created
before the scan in auto mode and in discover, and passed to both the scan
indicator and the later downloads. One owner per run drives stderr, and the
indicator is cleared before any download bar exists.

### Wording

One shared formatter produces the counts for both outputs, so they cannot drift:

```rust
fn scan_counts(files: usize, albums: usize) -> String
// "15553 audio file(s), 1220 album(s)"
```

- heartbeat: `Library scan still running: <counts> (180s elapsed)`
- spinner: `Scanning library: <counts> (180s elapsed)`

Neither can show a percentage: the total is unknown until the walk ends, so the
indicator shows counts and elapsed time, not a bar.

A consequence worth stating: because the counts and the elapsed text both come
from the walk, a stalled read freezes both while the spinner glyph keeps
ticking. That is the same signal the heartbeat line documents today - silence
means the walk has not returned from the entry it is on - and it is more visible
on one line than in a gap between log lines.

### Headless is unchanged

`is_interactive()` gates the display, as it already does for downloads. With no
display, `ScanIndicator::start(None)` creates no bar and touches no filter, so a
headless or scheduled run logs exactly what it logs today. This is asserted by a
test rather than assumed.

## Error handling and cancellation

Every exit path - completion, `Err(SeakarrError::Cancelled)`, an IO error from
the walk, and a panic - drops the indicator, which clears the bar and restores
the console filter. The order is fixed: clear the bar, restore the filter, then
log the closing line, so `Library scan complete` and the cancellation line are
never hidden by the suppression and never overwrite the spinner. A clear that
fails is ignored, as `clear_queue_bar` does. Cancellation itself is unchanged:
the flag is still checked once per walk entry and still surfaces as
`Err(Cancelled)`.

## Testing

1. `scan_counts` is covered by an exact-text test, so the heartbeat and the
   spinner message cannot drift apart.
2. The `ProgressDisplay` counters: created equals finished after a clear, and
   updated is monotonic - the contract the queue bars already have.
3. Headless: `start(None)` leaves `scan_bars_created` at zero and `update` is a
   no-op.
4. Interactive: with a display, exactly one bar is created, its message matches
   the formatter, and it is cleared exactly once on drop.
5. Suppression: a reloadable test subscriber sees a `seakarr::scanner` INFO
   event filtered while the guard is held and delivered again after release,
   including when the guard is dropped rather than explicitly finished.
6. The walk seam: a recording fake asserts monotonic counts, a final update
   carrying the end totals, and no calls at all when `None`. This is asserted by
   shape, not by call count, so the test cannot be timing-dependent.
7. Regression: the existing start, complete, heartbeat and 500-file cadence
   tests stay green, which is what proves the file log and the non-interactive
   console are unchanged.
8. Cancellation: the existing cancellation tests stay green, plus one asserting
   created equals finished when the walk returns `Cancelled`.

## Acceptance criteria

1. In an interactive run a single spinner line appears as the scan starts, with
   the spinner glyph ticking continuously and the counts and elapsed seconds
   refreshing at least once a second while the walk is making progress. A
   stalled read freezes the counts and the elapsed text while the glyph keeps
   ticking.
2. No `Library scan still running` line reaches the console while the spinner is
   live, and the same line still reaches the log file every 60s.
3. `Library scan starting`, `Library scan complete` and the cancellation line
   still reach both console and log file.
4. A headless run (stderr not a terminal) is unchanged: no spinner, heartbeat
   lines as today.
5. The spinner is cleared exactly once and the console filter restored on every
   exit path, including cancellation and panic.
6. The indicator covers every library scan: auto mode's startup scan, a discover
   run's scan, and an artist-only manual run's presence scan (which goes through
   `discover::index_from_paths`). Every one of them attaches it through the single
   helper `runner::with_scan_indicator`, so no scan can drift from the others.
   (Extended on 2026-09-20 after implementation found the third site: the
   original criterion named only `scan_library_cancellable`, which left
   `--artist` runs silent.)
7. No new configuration keys, no schema change, and no change to the scan's
   results: counts, grouping and unreadable-file handling are untouched.
8. The tick set and template match the queue spinner.

## Out of scope

The MusicBrainz discography resolution that discover mode performs before it
starts downloading is another long wait, and it is not covered here: this design
changes the library scan only.

## Configuration

None. Behaviour is gated on `is_interactive()`, as the download bars already
are. `logging.level` and `--log-level` keep their meaning: the heartbeat is
still emitted at INFO, and it is the console layer's filter that changes while
the spinner is live.

## Documentation

README's "Library scan log lines" section gains the interactive behaviour: a
spinner on stderr carrying the same counts and elapsed time, one line updating
in place, the per-minute heartbeat still written to the log file, and headless
runs unchanged.

## Alternatives considered

- **The scanner renders its own spinner.** Rejected: it puts rendering and TTY
  decisions inside the walk and gives a run two independent terminal owners.
- **No spinner; a shorter heartbeat or a `--progress` flag.** Rejected: it does
  not deliver one line updating in place.
- **Suppress the heartbeat by event rather than by target.** Rejected because it
  changes the line's logging target, and so its prefix in the log file, which
  the operator asked to keep as it is.
- **Literal ASCII tick characters.** Rejected: the requirement is the queue
  spinner, which uses the Braille rotation.
