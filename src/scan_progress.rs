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
///
/// The filter is process-wide and the indicator flips it for as long as a scan
/// runs, so this assumes one scan at a time. A second concurrent scan would
/// have its suppression released by whichever indicator finished first; the
/// runner scans once per run, before any download starts, so that cannot happen
/// today.
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
        let bar =
            display.map(|display| display.create_scan_bar(&scan_message(0, 0, Duration::ZERO)));
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
        let indicator = ScanIndicator::start(None, Some(Arc::new(console.clone())));

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
        let indicator = ScanIndicator::start(Some(&display), Some(Arc::new(console.clone())));

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
        let indicator = ScanIndicator::start(Some(&display), Some(Arc::new(console.clone())));

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
            let _indicator = ScanIndicator::start(Some(&display), Some(Arc::new(console.clone())));
            assert_eq!(display.scan_bars_created(), 1);
        }
        assert_eq!(display.scan_bars_finished(), 1);
        assert_eq!(console.recorded(), vec![false, true]);
    }

    #[test]
    fn installed_console_filter_is_absent_until_main_installs_one() {
        // The lookup is what `runner` uses; in tests and library use nothing is
        // installed, so suppression must degrade to a no-op rather than panic.
        // Asserted rather than merely called: a test that only calls it cannot
        // fail, and would pass just as happily if a filter were installed here
        // by accident. No test in this crate installs one, so absence is the
        // deterministic state.
        assert!(
            installed_console_filter().is_none(),
            "no test may install the process-wide console filter"
        );
    }
}
