use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use std::io::IsTerminal;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Check if stderr is an interactive terminal.
/// When false, progress bars should not be created.
pub fn is_interactive() -> bool {
    std::io::stderr().is_terminal()
}

/// Manages the run's terminal bars: one per active track download, one queue
/// bar per file waiting in a peer's queue, and one for the library scan.
///
/// Wraps `indicatif::MultiProgress` so multiple concurrent album downloads
/// each get their own progress bar. Callers should check `is_interactive()`
/// before creating an instance; in non-interactive contexts, skip creation
/// entirely rather than relying on this struct to no-op.
pub struct ProgressDisplay {
    multi: MultiProgress,
    /// Monotonic count of progress bars created, so tests (and future
    /// diagnostics) can observe that bars are only created once a transfer
    /// has actually started. Never decremented.
    bars_created: AtomicUsize,
    /// Monotonic count of queue bars created. Tracked separately because a
    /// queue bar deliberately exists before any transfer starts, so folding it
    /// into `bars_created` would destroy the meaning of that contract.
    queue_bars_created: AtomicUsize,
    /// Monotonic count of queue bars released. Must equal
    /// `queue_bars_created` once an attempt finishes: no path may leave a bar
    /// on the terminal.
    queue_bars_finished: AtomicUsize,
    /// Monotonic count of queue-bar message updates. Exists so the in-place
    /// update is observable: without it, an implementation that created a second
    /// bar instead of updating the first would pass every test.
    queue_bars_updated: AtomicUsize,
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
}

impl ProgressDisplay {
    /// Create a new ProgressDisplay.
    /// Renders to stderr. Should only be called when `is_interactive()` is true.
    pub fn new() -> Self {
        let multi = MultiProgress::new();
        // indicatif renders to stderr by default — matches spec.
        Self {
            multi,
            bars_created: AtomicUsize::new(0),
            queue_bars_created: AtomicUsize::new(0),
            queue_bars_finished: AtomicUsize::new(0),
            queue_bars_updated: AtomicUsize::new(0),
            scan_bars_created: AtomicUsize::new(0),
            scan_bars_finished: AtomicUsize::new(0),
            scan_bars_updated: AtomicUsize::new(0),
        }
    }

    /// Number of progress bars created so far (monotonic). Lets tests assert
    /// that bars are only created once a transfer actually starts.
    pub fn created_bars(&self) -> usize {
        self.bars_created.load(Ordering::SeqCst)
    }

    /// Number of queue bars created so far (monotonic).
    pub fn queue_bars_created(&self) -> usize {
        self.queue_bars_created.load(Ordering::SeqCst)
    }

    /// Number of queue bars released so far (monotonic).
    pub fn queue_bars_finished(&self) -> usize {
        self.queue_bars_finished.load(Ordering::SeqCst)
    }

    /// Number of queue-bar message updates so far (monotonic).
    pub fn queue_bars_updated(&self) -> usize {
        self.queue_bars_updated.load(Ordering::SeqCst)
    }

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

    /// Update an existing queue bar in place.
    ///
    /// Going through the display keeps the update observable, the same way the
    /// created/finished counters make the create-and-release contract observable.
    pub fn update_queue_bar(&self, bar: &ProgressBar, label: &str) {
        self.queue_bars_updated.fetch_add(1, Ordering::SeqCst);
        bar.set_message(safe_label(label, QUEUE_LABEL_MAX));
    }

    /// Create a progress bar for a track download.
    ///
    /// The bar shows: filename | downloaded/total | speed | bar | percentage
    pub fn create_bar(&self, filename: &str, total_bytes: u64) -> ProgressBar {
        self.bars_created.fetch_add(1, Ordering::SeqCst);
        // Initialise with the actual total so the bar starts at 0% — using
        // length 0 renders as a full 100% bar (0/0 = complete) before any
        // transfer begins.
        let bar = self.multi.add(ProgressBar::new(total_bytes));
        let style = ProgressStyle::with_template(
            "  {spinner} {msg}  {bytes}/{total_bytes}  {prefix}  [{bar:20}]  {percent}%",
        )
        .expect("valid progress bar template");
        let style = style.progress_chars("█░");
        bar.set_style(style);
        // Show only the basename, strip control characters (terminal injection
        // defence against peer-supplied filenames), and truncate to 60 chars.
        let display_name = filename.rsplit(['/', '\\']).next().unwrap_or(filename);
        bar.set_message(safe_label(display_name, 60));
        bar
    }

    /// Create a bar for a file waiting in a peer's queue.
    ///
    /// Unlike [`Self::create_bar`] this deliberately exists before any transfer
    /// starts: a queue position is real information while the file waits, and
    /// the bar updates in place, so a deep queue costs one terminal line rather
    /// than one line per position change.
    pub fn create_queue_bar(&self, label: &str) -> ProgressBar {
        self.queue_bars_created.fetch_add(1, Ordering::SeqCst);
        let bar = self.multi.add(ProgressBar::new_spinner());
        let style = ProgressStyle::with_template("  {spinner} {msg}")
            .expect("valid queue bar template")
            .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏");
        bar.set_style(style);
        bar.enable_steady_tick(std::time::Duration::from_millis(120));
        bar.set_message(safe_label(label, QUEUE_LABEL_MAX));
        bar
    }

    /// Release a queue bar. Going through the display keeps the "every attempt
    /// releases its queue bar" contract observable, the same way
    /// `bars_created` makes "no bar before the transfer starts" observable.
    pub fn clear_queue_bar(&self, bar: ProgressBar) {
        self.queue_bars_finished.fetch_add(1, Ordering::SeqCst);
        bar.finish_and_clear();
    }

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

    /// Remove all bars (call when download session ends).
    pub fn clear(&self) {
        let _ = self.multi.clear();
    }
}

/// Strip control characters from peer-supplied text and truncate it. Shared by
/// the transfer bar and the queue bar: both render text a peer chose, and a raw
/// control sequence would be interpreted by the terminal.
fn safe_label(text: &str, max_chars: usize) -> String {
    text.chars()
        .filter(|c| !c.is_control())
        .take(max_chars)
        .collect()
}

/// Longest label [`queue_label`] can produce: a 48-character basename, the
/// `" - "` separator, a 24-character peer name, and the longer of the two
/// suffixes — `" queue position unknown"` (23) exceeds `" queue #"` plus the
/// ten digits of a `u32` (18).
///
/// [`ProgressDisplay::create_queue_bar`] uses this as its cap so it can never
/// truncate the position off a label `queue_label` just built — the position is
/// the whole reason the bar exists, and truncating the tail is exactly what
/// would drop it.
pub const QUEUE_LABEL_MAX: usize = 48 + 3 + 24 + " queue position unknown".len();

/// Longest message [`ProgressDisplay::create_scan_bar`] can render: the fixed
/// wording, a seven-digit file count, a six-digit album count and a five-digit
/// second count. The elapsed time is the tail, so a cap below this would
/// truncate the number the operator is watching.
pub const SCAN_LABEL_MAX: usize = 96;

/// Message for a queued file's bar: `08 Moondance.flac - peer queue #42`.
///
/// Both the filename and the peer name are peer-supplied, so both are
/// sanitised; the position is a number from the wire. The result is never
/// longer than [`QUEUE_LABEL_MAX`], so passing it to `set_message` needs no
/// further truncation and both paths that set a queue-bar message agree.
#[must_use]
pub fn queue_label(basename: &str, username: &str, position: Option<u32>) -> String {
    let base = safe_label(basename, 48);
    let peer = safe_label(username, 24);
    match position {
        Some(position) => format!("{base} - {peer} queue #{position}"),
        None => format!("{base} - {peer} queue position unknown"),
    }
}

impl Default for ProgressDisplay {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_interactive_returns_false_in_tests() {
        // In test context (cargo test), stderr is typically not a TTY.
        // This test may pass or fail depending on how tests are run,
        // but documents the expected behaviour.
        // We just verify the function doesn't panic.
        let _ = is_interactive();
    }

    #[test]
    fn test_progress_display_creation() {
        let display = ProgressDisplay::new();
        let bar = display.create_bar("01 - Track.flac", 33_304_229);
        // Bar should be created and usable
        bar.set_position(10_552_744);
        assert_eq!(bar.position(), 10_552_744);
        bar.finish();
        display.clear();
    }

    // Regression guard: when a download completes, the progress bar must
    // be finished and snapped to 100%. The visual clearing behavior
    // (finish_and_clear vs finish) is enforced by code review — indicatif
    // exposes no public API to distinguish DoneVisible from DoneHidden.
    #[test]
    fn bar_finishes_at_100_percent_after_completion() {
        let display = ProgressDisplay::new();
        let bar = display.create_bar("01 - Track.flac", 10_000_000);
        bar.set_position(10_000_000);
        bar.finish();
        assert!(
            bar.is_finished(),
            "progress bar must be finished after download completes"
        );
        assert_eq!(
            bar.position(),
            10_000_000,
            "bar must show 100% after completion"
        );
        display.clear();
    }

    #[test]
    fn test_create_bar_extracts_basename() {
        let display = ProgressDisplay::new();
        let bar = display.create_bar(r"Music\Artist\Album\01 - Track.flac", 1000);
        // Message should be just the basename
        // indicatif stores the message — we can't easily read it back,
        // but we can verify no panic occurred.
        bar.finish();
        display.clear();
    }

    #[test]
    fn test_create_bar_uses_provided_total_length() {
        // Regression: the bar was created with length 0, which renders as a
        // full 100% bar immediately (0/0 = complete). The provided total
        // must be used so the bar starts at 0% with the correct total.
        let display = ProgressDisplay::new();
        let bar = display.create_bar("01 - Track.flac", 33_304_229);
        assert_eq!(
            bar.length(),
            Some(33_304_229),
            "bar must be initialised with the provided total bytes"
        );
        assert_eq!(
            bar.position(),
            0,
            "bar must start at position 0 (not complete)"
        );
        bar.finish();
        display.clear();
    }

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
        let scan_ticks: Vec<&str> = (0..10).map(|i| scan_style.get_tick_str(i)).collect();
        let queue_ticks: Vec<&str> = (0..10).map(|i| queue_style.get_tick_str(i)).collect();
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
            "SCAN_LABEL_MAX ({SCAN_LABEL_MAX}) truncates a real message ({} chars)",
            longest.chars().count()
        );
    }

    #[test]
    fn queue_bar_counter_is_separate_from_the_transfer_bar_counter() {
        // The transfer-bar counter is the evidence for the documented
        // "no bar before the transfer starts" contract. A queue bar exists
        // before any transfer, so it must not move that counter.
        let display = ProgressDisplay::new();
        let bar = display.create_queue_bar("01 - Track.flac - peer queue #3");
        assert_eq!(display.queue_bars_created(), 1);
        assert_eq!(
            display.created_bars(),
            0,
            "creating a queue bar must not count as creating a transfer bar"
        );
        display.clear_queue_bar(bar);
        assert_eq!(display.queue_bars_finished(), 1);
    }

    #[test]
    fn transfer_bar_counter_is_separate_from_the_queue_bar_counter() {
        let display = ProgressDisplay::new();
        let bar = display.create_bar("01 - Track.flac", 1_000);
        assert_eq!(display.created_bars(), 1);
        assert_eq!(display.queue_bars_created(), 0);
        bar.finish_and_clear();
    }

    #[test]
    fn queue_label_strips_control_characters_and_names_the_position() {
        // The filename and the peer name are both peer-supplied; a raw escape
        // sequence would be interpreted by the terminal.
        let label = queue_label("01 - Track\u{1b}[31m.flac", "peer\u{7}", Some(42));
        assert!(
            !label.chars().any(char::is_control),
            "control characters must be stripped, got {label:?}"
        );
        assert!(
            label.contains("queue #42"),
            "the position must appear, got {label:?}"
        );
    }

    #[test]
    fn queue_label_never_exceeds_the_bar_cap() {
        // `create_queue_bar` caps the message at `QUEUE_LABEL_MAX`. If
        // `queue_label` could exceed that, the tail — the position, the only
        // reason the bar exists — would be truncated off.
        let base = "b".repeat(80);
        let peer = "p".repeat(80);
        for position in [Some(42u32), None] {
            let label = queue_label(&base, &peer, position);
            assert!(
                label.len() <= QUEUE_LABEL_MAX,
                "label is {} chars, cap is {}: {label}",
                label.len(),
                QUEUE_LABEL_MAX
            );
        }
        let label = queue_label(&base, &peer, Some(42));
        assert!(
            label.ends_with("queue #42"),
            "the position must survive truncation, got: {label}"
        );
    }

    #[test]
    fn queue_label_reports_an_unknown_position() {
        let label = queue_label("01 - Track.flac", "peer", None);
        assert_eq!(label, "01 - Track.flac - peer queue position unknown");
    }

    #[test]
    fn update_queue_bar_replaces_the_number_in_place() {
        // The bar exists so a deep queue costs one terminal line: an update must
        // replace the message, not append to it.
        let display = ProgressDisplay::new();
        let bar = display.create_queue_bar(&queue_label("01 - Track.flac", "peer", Some(20)));
        assert_eq!(bar.message(), "01 - Track.flac - peer queue #20");

        display.update_queue_bar(&bar, &queue_label("01 - Track.flac", "peer", Some(12)));

        assert_eq!(
            bar.message(),
            "01 - Track.flac - peer queue #12",
            "the number must be replaced in place"
        );
        assert_eq!(display.queue_bars_updated(), 1);
        display.clear_queue_bar(bar);
    }
}
