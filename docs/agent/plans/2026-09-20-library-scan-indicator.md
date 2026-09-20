# Library Scan Indicator Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use sub-agents (recommended) to
> implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for
> tracking.

**Goal:** Give the interactive library scan a spinner that updates one stderr
line in place with the audio-file count, the album count and the elapsed
seconds, while leaving a headless run's log output exactly as it is today.

**Architecture:** The walk keeps counting as it does now and gains a
two-method port, `scanner::ScanProgress`, which it drives from the same
per-entry point as the existing heartbeat. A new `scan_progress` module
implements the port as `ScanIndicator`: it renders a spinner bar through the
existing `ProgressDisplay` and suppresses the console copy of the per-minute
heartbeat through a reloadable per-layer filter that `main` installs. The
`ProgressDisplay` is created before the scan rather than after it, so one owner
drives stderr per run.

**Tech Stack:** Rust 2021, `indicatif` 0.17 (`ProgressBar::new_spinner`),
`tracing` + `tracing-subscriber` 0.3 (`reload`, `filter::Targets`), `walkdir`,
`tempfile` fixtures.

**Spec:** `docs/agent/specs/2026-09-20-library-scan-indicator-design.md`

**Plan location:** saved under `docs/agent/plans/` because the repository's
AGENTS.md mandates that directory for agent-generated plans (it overrides the
skill's `docs/plans/` default, and every existing plan lives there).

**Note on code formatting:** snippets below are wrapped at 80 columns for the
Markdown line-length rule. `cargo fmt` (100-column default) may join some
wrapped lines; that is expected and not a deviation.

---

## Scope

One subsystem: the interactive indicator for the library scan, plus the log
filter and the two call sites it needs. No other long-running phase is touched
(the MusicBrainz discography wait is explicitly out of scope in the spec).

## File structure

- `src/scanner.rs` — owns the counts and the cadence: `scan_counts`, the
  `ScanProgress` port, `progress_due`/`maybe_report_progress`, and one extra
  parameter on `scan_library` and `scan_library_with_heartbeat`. It knows
  nothing about indicatif or log filters.
- `src/progress.rs` — owns terminal rendering: the scan-bar trio
  (`create_scan_bar`, `update_scan_bar`, `clear_scan_bar`), the `SCAN_LABEL_MAX`
  cap, and the created/finished/updated counters.
- `src/scan_progress.rs` (new) — owns the indicator: the `ConsoleFilter` trait,
  the process-wide install/lookup, and `ScanIndicator` with its `Drop` guard. It
  implements `scanner::ScanProgress`.
- `src/main.rs` — owns logging setup: the reloadable per-layer console filter,
  installed for `scan_progress`.
- `src/runner.rs` — owns run wiring: it creates the `ProgressDisplay` before the
  scan, builds the indicator in `scan_library_cancellable`, and passes the
  display on to the downloads.
- `src/lib.rs` — declares the new module.
- `README.md` — documents the interactive behaviour in "Library scan log lines".

---

### Task 1: Share one counts formatter

**Files:**

- Modify: `src/scanner.rs` (constants block near `SCAN_HEARTBEAT_SECS`, the
  heartbeat emission in `maybe_report_heartbeat`, and the test module)

- [ ] **Step 1: Write the failing test**

Add to the `tests` module in `src/scanner.rs` (next to the other
`scan_reports_*` tests):

```rust
    #[test]
    fn scan_counts_renders_the_shared_counts_text() {
        // One formatter feeds both the info heartbeat and the interactive
        // spinner, so the two can never report different numbers for the
        // same moment. Exact text: both outputs are read by an operator.
        assert_eq!(scan_counts(4948, 408), "4948 audio file(s), 408 album(s)");
        assert_eq!(scan_counts(0, 0), "0 audio file(s), 0 album(s)");
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --lib scan_counts_renders_the_shared_counts_text`
Expected: FAIL — `cannot find function 'scan_counts' in this scope`.

- [ ] **Step 3: Write the minimal implementation**

In `src/scanner.rs`, above `SCAN_HEARTBEAT_SECS`:

```rust
/// The counts both progress outputs share: `"4948 audio file(s)"` joined with
/// the album count.
///
/// The info heartbeat and the interactive spinner both report the walk's
/// progress at the same moments, and one formatter is what keeps them from
/// drifting into two different numbers for the same moment.
#[must_use]
pub fn scan_counts(files: usize, albums: usize) -> String {
    format!("{files} audio file(s), {albums} album(s)")
}
```

- [ ] **Step 4: Use it in the heartbeat, keeping the line byte-identical**

Replace the `tracing::info!` in `maybe_report_heartbeat` with:

```rust
    tracing::info!(
        "Library scan still running: {} ({:.0}s elapsed)",
        scan_counts(files_seen, albums),
        now.duration_since(started).as_secs_f64()
    );
```

The rendered text must not change: the existing tests assert the
`Library scan still running:` prefix and the counts.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --lib scanner::`
Expected: PASS, including `scan_counts_renders_the_shared_counts_text` and the
existing heartbeat tests, which prove the line is unchanged.

- [ ] **Step 6: Commit**

```bash
git add src/scanner.rs
git commit -m "refactor: share one scan counts formatter"
```

---

### Task 2: Add the ScanProgress port and drive it from the walk

**Files:**

- Modify: `src/scanner.rs` (`scan_library`, `scan_library_with_heartbeat`, the
  walk loop, the exits, and the test module)
- Modify: `docs/agent/specs/2026-09-20-library-scan-indicator-design.md` (the
  trait sketch gains `finish`)

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `src/scanner.rs`. First the fake and the
cadence test:

```rust
    /// Records what the walk reports, so the seam can be asserted directly.
    #[derive(Default)]
    struct RecordingProgress {
        updates: std::sync::Mutex<Vec<(usize, usize)>>,
        finishes: std::sync::atomic::AtomicUsize,
    }

    impl ScanProgress for RecordingProgress {
        fn update(
            &self,
            files: usize,
            albums: usize,
            _elapsed: std::time::Duration,
        ) {
            self.updates.lock().unwrap().push((files, albums));
        }

        fn finish(&self) {
            self.finishes
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    #[test]
    fn progress_due_only_fires_after_the_interval() {
        // Synthetic instants, mirroring `heartbeat_due`: a wall-clock test
        // would need a real second to elapse and would prove less.
        let start = std::time::Instant::now();
        let interval = std::time::Duration::from_secs(1);
        assert!(!progress_due(start, start, interval));
        assert!(!progress_due(
            start,
            start + std::time::Duration::from_millis(999),
            interval
        ));
        assert!(progress_due(
            start,
            start + std::time::Duration::from_secs(1),
            interval
        ));
    }

    #[test]
    fn maybe_report_progress_does_nothing_without_a_renderer() {
        // A headless run passes `None`; the check must not fire, so nothing
        // downstream can observe a bar that was never created.
        let start = std::time::Instant::now();
        let mut last = start;
        assert!(!maybe_report_progress(
            None,
            &mut last,
            start + std::time::Duration::from_secs(5),
            10,
            2,
            start,
            std::time::Duration::from_secs(1),
        ));
        assert_eq!(
            last, start,
            "the clock must not advance when nothing reported"
        );
    }

    #[test]
    fn scan_reports_its_counts_to_the_indicator_and_finishes_once() {
        // The walk owns the counting and the cadence; the renderer owns the
        // drawing. This pins the numbers the spinner receives, that they only
        // ever grow, and that the walk releases the renderer exactly once -
        // the closing info line depends on that release having happened.
        let dir = library_with_albums(1);
        let progress = RecordingProgress::default();

        let albums = scan_library_with_heartbeat(
            &library_paths(dir.path()),
            &FilterConfig::default(),
            None,
            std::time::Duration::from_secs(SCAN_HEARTBEAT_SECS),
            std::time::Duration::ZERO,
            Some(&progress),
        )
        .unwrap();

        assert_eq!(albums.len(), 1);
        let updates = progress.updates.lock().unwrap().clone();
        assert_eq!(
            updates.first(),
            Some(&(0, 0)),
            "the bar must appear with zero counts before the walk starts"
        );
        assert_eq!(
            updates.last(),
            Some(&(2, 1)),
            "the last update must carry the walk's final counts"
        );
        assert!(
            updates.windows(2).all(|pair| pair[1] >= pair[0]),
            "the counts must never go backwards, got {updates:?}"
        );
        assert_eq!(
            progress.finishes.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the walk must release the indicator exactly once"
        );
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib scanner::tests::progress_due`
Expected: FAIL — `cannot find function 'progress_due' in this scope`, and the
other two new tests fail to compile for the same reason.

- [ ] **Step 3: Write the port, the cadence helpers and the release helper**

In `src/scanner.rs`, below `scan_counts`:

```rust
/// How long the walk may run between interactive progress updates.
///
/// The counts come from the walk itself, so this is the smallest interval that
/// still reads as live without re-rendering once per file on a library of tens
/// of thousands.
const SCAN_PROGRESS_UPDATE_SECS: u64 = 1;

/// What the walk reports while it runs, for callers that render it.
///
/// The walk owns the counting and the cadence; the implementor owns the
/// rendering. `finish` exists because the closing info line must be emitted
/// after the renderer has released whatever it holds - a log filter that hides
/// the heartbeat, for instance - and `Drop` alone cannot order that.
pub trait ScanProgress {
    /// Report the counts seen so far. Called at most once a second from the
    /// same per-entry point as the heartbeat, and once with zero counts before
    /// the walk starts.
    fn update(&self, files: usize, albums: usize, elapsed: std::time::Duration);

    /// The walk is over; release anything held. Called on every return path,
    /// before the closing info line. Must be idempotent.
    fn finish(&self);
}

/// True when enough time has passed for another interactive update.
fn progress_due(
    last: std::time::Instant,
    now: std::time::Instant,
    interval: std::time::Duration,
) -> bool {
    now.duration_since(last) >= interval
}

/// Report the walk's progress to `progress` when the spinner interval is due,
/// advancing the clock it is given. Returns whether it reported.
///
/// Mirrors [`maybe_report_heartbeat`]: decision and emission are one function
/// so a test can drive the schedule with an injected `now`.
fn maybe_report_progress(
    progress: Option<&dyn ScanProgress>,
    last: &mut std::time::Instant,
    now: std::time::Instant,
    files_seen: usize,
    albums: usize,
    started: std::time::Instant,
    interval: std::time::Duration,
) -> bool {
    let Some(progress) = progress else {
        return false;
    };
    if !progress_due(*last, now, interval) {
        return false;
    }
    *last = now;
    progress.update(files_seen, albums, now.duration_since(started));
    true
}

/// Release the indicator, if one is attached. Called before every closing line
/// and before every early return, so the renderer can never outlive its walk.
fn finish_progress(progress: Option<&dyn ScanProgress>) {
    if let Some(progress) = progress {
        progress.finish();
    }
}
```

- [ ] **Step 4: Thread the port through both entry points**

Change the public entry point:

```rust
pub fn scan_library(
    library_paths: &[String],
    filters: &crate::config::FilterConfig,
    cancel: Option<&std::sync::atomic::AtomicBool>,
    progress: Option<&dyn ScanProgress>,
) -> Result<Vec<ScannedAlbum>> {
    scan_library_with_heartbeat(
        library_paths,
        filters,
        cancel,
        std::time::Duration::from_secs(SCAN_HEARTBEAT_SECS),
        std::time::Duration::from_secs(SCAN_PROGRESS_UPDATE_SECS),
        progress,
    )
}
```

and the private one:

```rust
fn scan_library_with_heartbeat(
    library_paths: &[String],
    filters: &crate::config::FilterConfig,
    cancel: Option<&std::sync::atomic::AtomicBool>,
    heartbeat_every: std::time::Duration,
    progress_every: std::time::Duration,
    progress: Option<&dyn ScanProgress>,
) -> Result<Vec<ScannedAlbum>> {
```

Extend the doc comment above `scan_library` with one sentence, so the walk's
own documentation lists the new output:

```rust
/// When `progress` is supplied, the walk also reports the same counts to it
/// once a second and releases it before the closing line, which is how the
/// interactive spinner is driven.
```

- [ ] **Step 5: Wire the walk: initial update, per-entry update, releases**

Directly after `let mut last_heartbeat = scan_started;` add:

```rust
    let mut last_progress = scan_started;
```

Directly after the `let cancelled = || ...;` line add:

```rust
    if let Some(progress) = progress {
        progress.update(0, 0, std::time::Duration::ZERO);
    }
```

In the walk loop, replace the existing heartbeat call with one shared instant
and both checks:

```rust
            let now = std::time::Instant::now();
            maybe_report_heartbeat(
                &mut last_heartbeat,
                now,
                files_seen,
                albums.len(),
                scan_started,
                heartbeat_every,
            );
            maybe_report_progress(
                progress,
                &mut last_progress,
                now,
                files_seen,
                albums.len(),
                scan_started,
                progress_every,
            );
```

Before the missing-root error return, add the release:

```rust
        if !lib_path.exists() {
            finish_progress(progress);
            return Err(SeakarrError::Scanner(format!(
                "library path does not exist: {lib_path_str}"
            )));
        }
```

Before the cancellation line, add the release so that line is visible:

```rust
            if cancelled() {
                finish_progress(progress);
                tracing::info!(
                    "Library scan cancelled by user after {files_seen} audio file(s)"
                );
                return Err(SeakarrError::Cancelled);
            }
```

Before the completion line, send the final counts and release:

```rust
    let album_count = albums.len();
    if let Some(progress) = progress {
        progress.update(files_seen, album_count, scan_started.elapsed());
        progress.finish();
    }
    // The existing completion line follows unchanged, and must stay unchanged:
    // it reports `{files_seen} audio file(s), {album_count} album(s),
    // {unreadable} unreadable file(s) in {:.1}s`, and tests assert its counts.
```

- [ ] **Step 6: Update the existing call sites**

In `src/scanner.rs` tests, the five `scan_library(...)` calls gain `None` as a
fourth argument, and the existing `scan_library_with_heartbeat(...)` call gains
`std::time::Duration::from_secs(SCAN_PROGRESS_UPDATE_SECS), None` after its
`heartbeat_every` argument. In `src/runner.rs`, the four
`scan_library_cancellable` calls and the `scanner::scan_library` call inside it
are updated in Task 6; until then the crate will not compile, so run this task's
commands after Task 6 if you are executing tasks out of order.

- [ ] **Step 7: Run the tests to verify they pass**

Run: `cargo test --lib scanner::`
Expected: PASS, including the three new tests and every existing scan-line
test.

- [ ] **Step 8: Amend the spec's trait sketch**

In `docs/agent/specs/2026-09-20-library-scan-indicator-design.md`, replace:

```rust
pub trait ScanProgress {
    fn update(&self, files: usize, albums: usize, elapsed: Duration);
}
```

with:

```rust
pub trait ScanProgress {
    fn update(&self, files: usize, albums: usize, elapsed: Duration);
    fn finish(&self);
}
```

and add directly below it:

```markdown
`finish` is the walk's release signal, and it exists because the closing info
line must be logged *after* the renderer has released the console filter;
relying on `Drop` alone would order that wrongly, since the indicator outlives
the call in the caller. It is idempotent, so `Drop` can call it too.
```

- [ ] **Step 9: Commit**

```bash
git add src/scanner.rs \
  docs/agent/specs/2026-09-20-library-scan-indicator-design.md
git commit -m "feat: report scan progress through a ScanProgress port"
```

---

### Task 3: Render the scan bar in ProgressDisplay

**Files:**

- Modify: `src/progress.rs` (the struct, its `new`, its accessors, and the
  test module)

- [ ] **Step 1: Write the failing tests**

Add to `src/progress.rs`'s `tests` module:

```rust
    #[test]
    fn scan_bar_counters_are_separate_from_the_other_bar_counters() {
        let display = ProgressDisplay::new();
        let bar = display.create_scan_bar("Scanning library: 0 audio file(s)");
        assert_eq!(display.scan_bars_created(), 1);
        assert_eq!(display.scan_bars_finished(), 0);
        assert_eq!(
            display.created_bars(),
            0,
            "a scan bar is not a transfer bar"
        );
        assert_eq!(
            display.queue_bars_created(),
            0,
            "a scan bar is not a queue bar"
        );
        display.update_scan_bar(&bar, "Scanning library: 2 audio file(s)");
        assert_eq!(display.scan_bars_updated(), 1);
        display.clear_scan_bar(bar);
        assert_eq!(display.scan_bars_finished(), 1);
    }

    #[test]
    fn scan_bar_ticks_exactly_like_the_queue_bar() {
        // "Same as the queue" is the requirement, so the tick characters are
        // compared against the queue bar's rather than hard-coded: changing one
        // bar's ticks without the other fails this test.
        let display = ProgressDisplay::new();
        let scan = display.create_scan_bar("Scanning library: 0 audio file(s)");
        let queue = display.create_queue_bar("01 - Track.flac - peer queue #1");
        let scan_style = scan.style();
        let queue_style = queue.style();
        let scan_ticks: Vec<&str> =
            (0..10).map(|i| scan_style.get_tick_str(i)).collect();
        let queue_ticks: Vec<&str> =
            (0..10).map(|i| queue_style.get_tick_str(i)).collect();
        assert_eq!(
            scan_ticks, queue_ticks,
            "the scan spinner must tick like the queue spinner"
        );
        display.clear_scan_bar(scan);
        display.clear_queue_bar(queue);
    }

    #[test]
    fn scan_label_cap_covers_the_longest_real_message() {
        // `create_scan_bar` truncates at SCAN_LABEL_MAX, and the tail of the
        // message is the elapsed time: a cap below a real message would cut it
        // off. Seven-digit file counts, six-digit album counts and five-digit
        // second counts are the bounds a real library can reach.
        let longest = format!(
            "Scanning library: {} audio file(s), {} album(s) ({}s elapsed)",
            9_999_999, 999_999, 99_999
        );
        assert!(
            longest.chars().count() <= SCAN_LABEL_MAX,
            "SCAN_LABEL_MAX ({SCAN_LABEL_MAX}) truncates a real message \
             ({} chars)",
            longest.chars().count()
        );
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib progress::tests::scan_bar`
Expected: FAIL — `no method named 'create_scan_bar'`, `scan_bars_created`,
`SCAN_LABEL_MAX` not found.

- [ ] **Step 3: Add the counters and the label cap**

In `src/progress.rs`, add to `struct ProgressDisplay` after
`queue_bars_updated`:

```rust
    /// Monotonic count of scan bars created. The scan bar is the one bar that
    /// legitimately exists before any transfer: the walk is not a download, so
    /// folding it into `bars_created` would break that contract's meaning.
    scan_bars_created: AtomicUsize,
    /// Monotonic count of scan bars released. Must equal `scan_bars_created`
    /// once a scan returns: the terminal must be free for the download bars.
    scan_bars_finished: AtomicUsize,
    /// Monotonic count of scan-bar message updates, so an implementation that
    /// created a second bar instead of updating the first is caught.
    scan_bars_updated: AtomicUsize,
```

initialise them in `new` (after `queue_bars_updated: AtomicUsize::new(0),`):

```rust
            scan_bars_created: AtomicUsize::new(0),
            scan_bars_finished: AtomicUsize::new(0),
            scan_bars_updated: AtomicUsize::new(0),
```

and add the accessors after `queue_bars_updated`:

```rust
    /// Number of scan bars created so far (monotonic).
    pub fn scan_bars_created(&self) -> usize {
        self.scan_bars_created.load(Ordering::SeqCst)
    }

    /// Number of scan bars released so far (monotonic).
    pub fn scan_bars_finished(&self) -> usize {
        self.scan_bars_finished.load(Ordering::SeqCst)
    }

    /// Number of scan-bar message updates so far (monotonic).
    pub fn scan_bars_updated(&self) -> usize {
        self.scan_bars_updated.load(Ordering::SeqCst)
    }
```

Add the cap next to `QUEUE_LABEL_MAX`:

```rust
/// Longest message [`ProgressDisplay::create_scan_bar`] can render: the fixed
/// wording, a seven-digit file count, a six-digit album count and a five-digit
/// second count. The elapsed time is the tail, so a cap below this would
/// truncate the number the operator is watching.
pub const SCAN_LABEL_MAX: usize = 96;
```

- [ ] **Step 4: Add the bar trio**

In `impl ProgressDisplay`, after `clear_queue_bar`:

```rust
    /// Create the library scan's bar.
    ///
    /// A spinner, not a bar with a length: the walk cannot know how many files
    /// or albums it will find until it has found them, so there is no total to
    /// divide by. Same template and tick set as the queue bar, because the
    /// requirement is the queue's spinner.
    pub fn create_scan_bar(&self, message: &str) -> ProgressBar {
        self.scan_bars_created.fetch_add(1, Ordering::SeqCst);
        let bar = self.multi.add(ProgressBar::new_spinner());
        let style = ProgressStyle::with_template("  {spinner} {msg}")
            .expect("valid scan bar template")
            .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏");
        bar.set_style(style);
        bar.enable_steady_tick(std::time::Duration::from_millis(120));
        bar.set_message(safe_label(message, SCAN_LABEL_MAX));
        bar
    }

    /// Update the scan bar's message in place.
    pub fn update_scan_bar(&self, bar: &ProgressBar, message: &str) {
        self.scan_bars_updated.fetch_add(1, Ordering::SeqCst);
        bar.set_message(safe_label(message, SCAN_LABEL_MAX));
    }

    /// Release the scan bar, leaving the terminal to the download bars.
    pub fn clear_scan_bar(&self, bar: ProgressBar) {
        self.scan_bars_finished.fetch_add(1, Ordering::SeqCst);
        bar.finish_and_clear();
    }
```

- [ ] **Step 5: Update the type's own doc comment**

Replace the first line of the `ProgressDisplay` doc comment
("Manages download progress bars — one per active track download.") with:

```rust
/// Manages the run's terminal bars: one per active track download, one queue
/// bar per file waiting in a peer's queue, and one for the library scan.
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test --lib progress::`
Expected: PASS, including the three new tests and every existing bar test.

- [ ] **Step 7: Commit**

```bash
git add src/progress.rs
git commit -m "feat: render a scan spinner alongside the download bars"
```

---

### Task 4: Build the ScanIndicator

**Files:**

- Create: `src/scan_progress.rs`
- Modify: `src/lib.rs` (module list)

- [ ] **Step 1: Write the failing tests**

Create `src/scan_progress.rs` with only the test module first, so the tests
fail on the missing implementation:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::progress::ProgressDisplay;
    use crate::scanner::ScanProgress;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct RecordingConsole {
        states: Arc<Mutex<Vec<bool>>>,
    }

    impl RecordingConsole {
        fn recorded(&self) -> Vec<bool> {
            self.states.lock().unwrap().clone()
        }
    }

    impl ConsoleFilter for RecordingConsole {
        fn set_console_heartbeat(&self, enabled: bool) {
            self.states.lock().unwrap().push(enabled);
        }
    }

    #[test]
    fn scan_message_matches_the_shared_counts_formatter() {
        assert_eq!(
            scan_message(4948, 408, std::time::Duration::from_secs(180)),
            "Scanning library: 4948 audio file(s), 408 album(s) (180s elapsed)"
        );
    }

    #[test]
    fn a_headless_start_creates_no_bar_and_touches_no_filter() {
        // No display means no bar, and no bar means the console heartbeat is
        // not suppressed: that absence is what keeps a headless run identical
        // to the behaviour before this feature.
        let console = RecordingConsole::default();
        let indicator =
            ScanIndicator::start(None, Some(Arc::new(console.clone())));

        indicator.update(10, 2, std::time::Duration::from_secs(1));
        indicator.finish();

        assert!(
            console.recorded().is_empty(),
            "a headless run must not touch the console filter"
        );
    }

    #[test]
    fn an_interactive_start_shows_one_bar_and_suppresses_then_restores() {
        let display = ProgressDisplay::new();
        let console = RecordingConsole::default();
        let indicator =
            ScanIndicator::start(Some(&display), Some(Arc::new(console.clone())));

        indicator.update(10, 2, std::time::Duration::from_secs(1));
        assert_eq!(display.scan_bars_created(), 1);
        assert_eq!(display.scan_bars_updated(), 1);
        assert_eq!(
            console.recorded(),
            vec![false],
            "the heartbeat must be suppressed while the spinner is live"
        );

        indicator.finish();
        assert_eq!(display.scan_bars_finished(), 1);
        assert_eq!(
            console.recorded(),
            vec![false, true],
            "the heartbeat must come back when the walk is over"
        );
    }

    #[test]
    fn finishing_twice_releases_once() {
        // The walk calls `finish` before its closing line and `Drop` calls it
        // again; a second release would break the created-equals-finished
        // contract the download bars depend on.
        let display = ProgressDisplay::new();
        let console = RecordingConsole::default();
        let indicator =
            ScanIndicator::start(Some(&display), Some(Arc::new(console.clone())));

        indicator.finish();
        indicator.finish();

        assert_eq!(display.scan_bars_finished(), 1);
        assert_eq!(console.recorded(), vec![false, true]);
    }

    #[test]
    fn dropping_releases_without_an_explicit_finish() {
        // Every early exit - a cancelled walk, an IO error, a panic - relies on
        // this rather than on the walk's own release call.
        let display = ProgressDisplay::new();
        let console = RecordingConsole::default();
        {
            let _indicator =
                ScanIndicator::start(Some(&display), Some(Arc::new(console.clone())));
            assert_eq!(display.scan_bars_created(), 1);
        }
        assert_eq!(display.scan_bars_finished(), 1);
        assert_eq!(console.recorded(), vec![false, true]);
    }

    #[test]
    fn installed_console_filter_is_absent_until_main_installs_one() {
        // The lookup is what `runner` uses; in tests and library use nothing is
        // installed, so suppression must degrade to a no-op rather than panic.
        let _ = installed_console_filter();
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib scan_progress::`
Expected: FAIL — the module does not exist yet, so first add
`pub mod scan_progress;` to `src/lib.rs` (before `pub mod scanner;`) and re-run:
FAIL with `cannot find function 'scan_message'` and
`cannot find type 'ScanIndicator'`.

- [ ] **Step 3: Write the implementation**

Above the test module in `src/scan_progress.rs`:

```rust
//! The interactive library-scan indicator.
//!
//! Holds the two things that may exist only while the scan runs: the spinner
//! bar, and the console-log filter that suppresses the per-minute heartbeat the
//! spinner replaces. The walk drives it through
//! [`crate::scanner::ScanProgress`], so the walk never learns about indicatif
//! or about log filters.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use crate::progress::ProgressDisplay;
use crate::scanner::{scan_counts, ScanProgress};

/// The console log layer's scanner filter, as the indicator drives it.
///
/// A trait rather than a concrete `tracing_subscriber::reload::Handle`, so the
/// indicator is testable without a subscriber and this module stays free of the
/// logging setup's types.
pub trait ConsoleFilter: Send + Sync {
    /// `true` lets the heartbeat reach the console; `false` suppresses it.
    fn set_console_heartbeat(&self, enabled: bool);
}

static CONSOLE_FILTER: OnceLock<Arc<dyn ConsoleFilter>> = OnceLock::new();

/// Install the console filter the indicator drives. Called once by `main`;
/// where it is never called - tests, library use - suppression is a no-op.
pub fn install_console_filter(filter: Arc<dyn ConsoleFilter>) {
    let _ = CONSOLE_FILTER.set(filter);
}

/// The installed console filter, if any.
#[must_use]
pub fn installed_console_filter() -> Option<Arc<dyn ConsoleFilter>> {
    CONSOLE_FILTER.get().cloned()
}

/// The spinner's message: the same counts and elapsed time the info heartbeat
/// reports, so the two outputs can never disagree.
#[must_use]
fn scan_message(files: usize, albums: usize, elapsed: Duration) -> String {
    format!(
        "Scanning library: {} ({:.0}s elapsed)",
        scan_counts(files, albums),
        elapsed.as_secs_f64()
    )
}

/// One interactive scan's spinner and console suppression.
///
/// Created before the walk and released by it, so the closing info line is
/// logged with the console filter already restored.
pub struct ScanIndicator<'a> {
    display: Option<&'a ProgressDisplay>,
    bar: Option<indicatif::ProgressBar>,
    console: Option<Arc<dyn ConsoleFilter>>,
    released: AtomicBool,
}

impl<'a> ScanIndicator<'a> {
    /// Start the indicator.
    ///
    /// With no display (a headless run) nothing is created and no filter is
    /// touched. With one, the spinner appears immediately with zero counts and
    /// the console heartbeat is suppressed for as long as the indicator lives.
    #[must_use]
    pub fn start(
        display: Option<&'a ProgressDisplay>,
        console: Option<Arc<dyn ConsoleFilter>>,
    ) -> Self {
        let bar = display.map(|display| {
            display.create_scan_bar(&scan_message(0, 0, Duration::ZERO))
        });
        let console = match (&bar, console) {
            (Some(_), Some(console)) => {
                console.set_console_heartbeat(false);
                Some(console)
            }
            // A headless run has no bar to justify touching the filter, and an
            // installed-but-unused filter must not be left flipped.
            _ => None,
        };
        Self {
            display,
            bar,
            console,
            released: AtomicBool::new(false),
        }
    }

    /// Release the bar and the suppression. Idempotent, because the walk calls
    /// it before its closing line and `Drop` calls it again.
    pub fn release(&self) {
        if self.released.swap(true, Ordering::SeqCst) {
            return;
        }
        if let (Some(display), Some(bar)) = (self.display, self.bar.as_ref()) {
            display.clear_scan_bar(bar.clone());
        }
        if let Some(console) = &self.console {
            console.set_console_heartbeat(true);
        }
    }
}

impl ScanProgress for ScanIndicator<'_> {
    fn update(&self, files: usize, albums: usize, elapsed: Duration) {
        if let (Some(display), Some(bar)) = (self.display, self.bar.as_ref()) {
            display.update_scan_bar(bar, &scan_message(files, albums, elapsed));
        }
    }

    fn finish(&self) {
        self.release();
    }
}

impl Drop for ScanIndicator<'_> {
    fn drop(&mut self) {
        self.release();
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib scan_progress::`
Expected: PASS — six tests.

- [ ] **Step 5: Commit**

```bash
git add src/scan_progress.rs src/lib.rs
git commit -m "feat: add the library scan indicator"
```

---

### Task 5: Install the reloadable console filter in main

**Files:**

- Modify: `src/main.rs` (the logging setup and its test module)

- [ ] **Step 1: Write the failing test**

Add to `mod tests` in `src/main.rs`:

```rust
    #[test]
    fn the_console_filter_hides_and_restores_the_scan_heartbeat() {
        use seakarr::scan_progress::ConsoleFilter;
        use std::sync::{Arc, Mutex};
        use tracing_subscriber::layer::SubscriberExt;

        // A writer we can read back, so the assertion is about what actually
        // reached the console rather than about the filter's internal state.
        #[derive(Clone, Default)]
        struct Captured(Arc<Mutex<Vec<u8>>>);

        impl std::io::Write for Captured {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let captured = Captured::default();
        let (filter, handle) =
            tracing_subscriber::reload::Layer::new(console_targets(true));
        let writer = captured.clone();
        let subscriber = tracing_subscriber::registry().with(
            tracing_subscriber::fmt::Layer::new()
                .with_writer(move || writer.clone())
                .with_ansi(false)
                .with_filter(filter),
        );
        let driver = ConsoleFilterFn(move |enabled: bool| {
            let _ =
                handle.modify(|targets| *targets = console_targets(enabled));
        });

        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(target: "seakarr::scanner", "heartbeat visible");
            driver.set_console_heartbeat(false);
            tracing::info!(target: "seakarr::scanner", "heartbeat suppressed");
            tracing::info!(
                target: "seakarr::runner",
                "other target still shown"
            );
            driver.set_console_heartbeat(true);
            tracing::info!(target: "seakarr::scanner", "heartbeat restored");
        });

        let captured_text = captured.0.lock().unwrap().clone();
        let text = String::from_utf8(captured_text).unwrap();
        assert!(text.contains("heartbeat visible"), "got:\n{text}");
        assert!(!text.contains("heartbeat suppressed"), "got:\n{text}");
        assert!(
            text.contains("other target still shown"),
            "suppression must be scoped to the scanner target, got:\n{text}"
        );
        assert!(text.contains("heartbeat restored"), "got:\n{text}");
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --bin seakarr the_console_filter_hides`
Expected: FAIL — `cannot find function 'console_targets'` / type
`ConsoleFilterFn`.

- [ ] **Step 3: Write the filter helpers**

Add near the top of `src/main.rs`, after the `use` block:

```rust
/// The console layer's own filter.
///
/// Everything the registry filter already admitted passes, minus the scan
/// heartbeat while the interactive spinner is showing it instead. Only the
/// console layer carries this filter: the file layer keeps the per-minute line
/// whether or not a terminal is attached.
fn console_targets(heartbeat: bool) -> tracing_subscriber::filter::Targets {
    use tracing_subscriber::filter::{LevelFilter, Targets};
    let base = Targets::new().with_default(LevelFilter::TRACE);
    if heartbeat {
        base
    } else {
        base.with_target("seakarr::scanner", LevelFilter::OFF)
    }
}

/// Adapts a closure to the indicator's console-filter port, so the reload
/// handle's subscriber type never has to be named here.
struct ConsoleFilterFn<F: Fn(bool) + Send + Sync>(F);

impl<F: Fn(bool) + Send + Sync> seakarr::scan_progress::ConsoleFilter
    for ConsoleFilterFn<F>
{
    fn set_console_heartbeat(&self, enabled: bool) {
        (self.0)(enabled);
    }
}
```

- [ ] **Step 4: Wire it into the logging setup**

Replace the subscriber construction in the run path with:

```rust
    let (console_filter, console_handle) =
        tracing_subscriber::reload::Layer::new(console_targets(true));
    tracing_subscriber::registry()
        .with(env_filter)
        .with(
            fmt::Layer::new()
                .with_writer(std::io::stdout)
                .with_filter(console_filter),
        )
        .with(
            fmt::Layer::new()
                .with_writer(file_appender)
                .with_ansi(false),
        )
        .init();
    seakarr::scan_progress::install_console_filter(std::sync::Arc::new(
        ConsoleFilterFn(move |enabled: bool| {
            if let Err(error) = console_handle
                .modify(|targets| *targets = console_targets(enabled))
            {
                // Losing the filter is not worth failing a scan over: the
                // heartbeat would simply keep printing to the console.
                tracing::debug!(
                    "could not update the console scan filter: {error}"
                );
            }
        }),
    ));
```

Add `use tracing_subscriber::layer::SubscriberExt;` if `with_filter` is not
already in scope (the existing `prelude::*` import provides it, so the build
will tell you).

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --bin seakarr the_console_filter_hides`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/main.rs
git commit -m "feat: reload the console filter around an interactive scan"
```

---

### Task 6: Wire the indicator into both runners

**Files:**

- Modify: `src/runner.rs` (`scan_library_cancellable`, `run_auto_mode` around
  its scan, `run_discover_mode_with_provider` around its scan, and the four
  remaining `scan_library_cancellable` call sites)

- [ ] **Step 1: Write the failing test**

Add to `mod tests` in `src/runner.rs` (the fixture pattern already exists in
`a_cancelled_scan_reports_nothing_to_do`):

```rust
    #[test]
    fn scan_shows_and_releases_exactly_one_indicator_bar() {
        // The scan's indicator is the run's first bar, and the download bars
        // come later: one created, one released, so the terminal is free when
        // the downloads start.
        let dir = TempDir::new().unwrap();
        let album_dir = dir.path().join("Artist").join("Album");
        std::fs::create_dir_all(&album_dir).unwrap();
        write_minimal_flac_with_tags(
            &album_dir.join("01 - track.flac"),
            "Artist",
            "Album",
        );
        let (mut config, _db, _staging) = artist_only_fixture();
        config.library.paths = vec![dir.path().to_string_lossy().into_owned()];
        let cancel = Arc::new(AtomicBool::new(false));
        let display = ProgressDisplay::new();

        let outcome =
            scan_library_cancellable(&config, &cancel, Some(&display)).unwrap();

        assert!(outcome.is_some(), "the scan must still return the library");
        assert_eq!(display.scan_bars_created(), 1);
        assert_eq!(display.scan_bars_finished(), 1);
        assert_eq!(
            display.created_bars(),
            0,
            "the scan must not create a transfer bar"
        );
    }

    #[test]
    fn a_headless_scan_creates_no_indicator_bar() {
        // Nothing to observe without a display, so this pins the contract that
        // matters: with no display the scan still works and returns the library
        // without touching the terminal.
        let dir = TempDir::new().unwrap();
        let album_dir = dir.path().join("Artist").join("Album");
        std::fs::create_dir_all(&album_dir).unwrap();
        write_minimal_flac_with_tags(
            &album_dir.join("01 - track.flac"),
            "Artist",
            "Album",
        );
        let (mut config, _db, _staging) = artist_only_fixture();
        config.library.paths = vec![dir.path().to_string_lossy().into_owned()];
        let cancel = Arc::new(AtomicBool::new(false));

        let outcome = scan_library_cancellable(&config, &cancel, None).unwrap();

        let albums = outcome.expect("a headless scan must return the library");
        assert_eq!(albums.len(), 1);
    }

    #[test]
    fn a_cancelled_scan_still_releases_its_indicator_bar() {
        // Cancellation is the path most likely to strand a bar, and a stranded
        // scan bar would sit on the terminal for the rest of the run. The walk
        // releases before it returns `Cancelled`, and `Drop` covers the rest.
        let dir = TempDir::new().unwrap();
        let album_dir = dir.path().join("Artist").join("Album");
        std::fs::create_dir_all(&album_dir).unwrap();
        write_minimal_flac_with_tags(
            &album_dir.join("01 - track.flac"),
            "Artist",
            "Album",
        );
        let (mut config, _db, _staging) = artist_only_fixture();
        config.library.paths = vec![dir.path().to_string_lossy().into_owned()];
        let cancel = Arc::new(AtomicBool::new(true));
        let display = ProgressDisplay::new();

        let outcome =
            scan_library_cancellable(&config, &cancel, Some(&display)).unwrap();

        assert!(outcome.is_none(), "a cancelled scan reports no albums");
        assert_eq!(display.scan_bars_created(), 1);
        assert_eq!(
            display.scan_bars_finished(),
            display.scan_bars_created(),
            "every created scan bar must be released"
        );
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib scan_shows_and_releases_exactly_one_indicator_bar`
Expected: FAIL to compile — `scan_library_cancellable` takes 2 arguments.

- [ ] **Step 3: Build the indicator in the shared scan helper**

Replace `scan_library_cancellable` in `src/runner.rs` with:

```rust
/// `Ok(None)` means the user cancelled: the caller returns without doing any
/// work, and `main` releases the PID lock as it unwinds.
///
/// The indicator is created here rather than inside the walk, so the scan and
/// the downloads share one `ProgressDisplay` and the terminal has one owner per
/// run. With `progress: None` (a headless run) no bar is created and the
/// console heartbeat is left alone.
fn scan_library_cancellable(
    config: &Config,
    cancel: &AtomicBool,
    progress: Option<&ProgressDisplay>,
) -> Result<Option<Vec<scanner::ScannedAlbum>>> {
    let indicator = scan_progress::ScanIndicator::start(
        progress,
        scan_progress::installed_console_filter(),
    );
    match scanner::scan_library(
        &config.library.paths,
        &config.filters,
        Some(cancel),
        Some(&indicator),
    ) {
        Ok(albums) => Ok(Some(albums)),
        Err(SeakarrError::Cancelled) => {
            tracing::info!(
                "Library scan cancelled — aborting before any work item"
            );
            Ok(None)
        }
        Err(error) => Err(error),
    }
}
```

Add the import next to the existing `use crate::progress::{...}` line:

```rust
use crate::scan_progress;
```

- [ ] **Step 4: Update the four call sites**

In `run_auto_mode`, replace the scan block and delete the later duplicate
display creation:

```rust
    // The display is created before the scan, not after it: the scan owns the
    // first bar and the downloads own the rest, and one owner per run keeps a
    // single MultiProgress writing to stderr.
    let progress = if is_interactive() {
        Some(Arc::new(ProgressDisplay::new()))
    } else {
        None
    };

    // Scan library
    tracing::info!("Scanning library...");
    let Some(albums) =
        scan_library_cancellable(config, &cancel, progress.as_deref())?
    else {
        return Ok(());
    };
```

then delete the now-duplicated block further down:

```rust
    let progress = if is_interactive() {
        Some(Arc::new(ProgressDisplay::new()))
    } else {
        None
    };
```

In `run_discover_mode_with_provider`, do the same with `as_ref()`:

```rust
    let progress = if is_interactive() {
        Some(ProgressDisplay::new())
    } else {
        None
    };

    let Some(scanned) =
        scan_library_cancellable(config, &cancel, progress.as_ref())?
    else {
        return Ok(());
    };
```

and delete that function's later duplicate display creation.

Finally, the two test call sites in `mod tests`
(`scan_library_cancellable(&config, &cancel)`) gain `None` as a third argument.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --lib runner::` then `cargo test`
Expected: PASS — both new runner tests, the two updated scan tests, and the
whole suite.

- [ ] **Step 6: Commit**

```bash
git add src/runner.rs
git commit -m "feat: give the scan the run's progress display"
```

---

### Task 7: Document the interactive behaviour and run the gates

**Files:**

- Modify: `README.md` (the "Library scan log lines" section)

- [ ] **Step 1: Update the README**

Append to the "#### Library scan log lines" section, after the paragraph that
ends "so the line shows progress rather than proving health":

```markdown
On an interactive terminal the per-minute line is replaced by a spinner that
updates a single line in place — `Scanning library: 4948 audio file(s), 408
album(s) (60s elapsed)` — carrying the same counts and elapsed time, so a long
scan shows movement without scrolling. The counts come from the walk itself, so
a stalled read freezes them while the spinner keeps ticking, exactly as the
per-minute line does today. The per-minute line is still written to the log file
while the spinner is live, and a headless run (no terminal attached) keeps the
log lines unchanged. The starting, complete and cancelled lines still reach both
the console and the file.
```

- [ ] **Step 2: Verify the documentation renders**

Run: `markdownlint README.md` and
`markdownlint docs/agent/specs/2026-09-20-library-scan-indicator-design.md`
Expected: both exit 0 (README disables the line-length rule; the spec and this
plan do not, so both must stay within 80 columns).

- [ ] **Step 3: Run the full gate**

```bash
cargo fmt
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
pre-commit run --all-files
```

Expected: every command exits 0, `cargo test` reports 0 failures (1097 before
this feature plus the new tests), and `pre-commit` reports all hooks passed.

- [ ] **Step 4: Commit**

```bash
git add README.md
git commit -m "docs: describe the interactive library scan indicator"
```

---

## Execution notes for sub-agents

- One sub-agent per task, in order; each task ends with its own commit and must
  leave `cargo test` green. Tasks 2 and 6 are interlocked (the extra parameter
  on `scanner::scan_library` breaks `runner`'s call until Task 6 wires it), so
  if a sub-agent runs Task 2 alone it must also update `runner`'s call sites in
  the same task; the plan lists them explicitly in Task 2 Step 6 and Task 6
  Step 4.
- Never change the text of the three existing info lines (`Library scan
  starting`, `Library scan still running`, `Library scan complete`) or the
  cancellation line: their exact wording is asserted by existing tests and
  documented in the README.
- A task is not complete until its own tests and the full suite pass; report the
  command output, not a summary of it.

## Self-review

**Spec coverage.** Every acceptance criterion maps to a task:
criterion 1 (spinner appears and refreshes) → Tasks 2, 3, 4; criterion 2
(no console heartbeat while live, still in the file) → Tasks 4, 5; criterion 3
(closing lines still reach both) → Task 2 Steps 5 and the release ordering in
Task 4; criterion 4 (headless unchanged) → Tasks 4 and 6 tests; criterion 5
(cleared and restored on every path, including panic) → Task 4's idempotent
`release` plus `Drop`, asserted by Task 6's cancelled-scan test; criterion 6
(both call sites) → Task 6 Step 4;
criterion 7 (no config keys, no scan-result change) → nothing adds config, and
Task 1 keeps the log text byte-identical; criterion 8 (tick set and template
match the queue) → Task 3 Step 1's cross-bar tick comparison.

**Placeholder scan.** No "TBD", "TODO", "add error handling" or "similar to
Task N" appears above; every code step carries the code, every command step
carries the command and its expected result.

**Type consistency.** `scan_counts`, `ScanProgress::{update, finish}`,
`SCAN_PROGRESS_UPDATE_SECS`, `progress_due`, `maybe_report_progress`,
`finish_progress`, `SCAN_LABEL_MAX`, `create_scan_bar`, `update_scan_bar`,
`clear_scan_bar`, `scan_bars_created/finished/updated`,
`ConsoleFilter::set_console_heartbeat`, `install_console_filter`,
`installed_console_filter`, `scan_message`, `ScanIndicator::{start, release}`,
`console_targets`, `ConsoleFilterFn` and
`scan_library_cancellable(config, cancel, progress)` are each defined once and
used with the same signature everywhere later in the plan.

**One deliberate spec refinement** is carried in Task 2 Step 8: the trait gains
`finish`, because the closing info line must be logged after the console filter
is restored and `Drop` cannot order that. The spec file is updated in the same
commit so the design record and the code agree.
