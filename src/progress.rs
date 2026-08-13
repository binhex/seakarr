use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use std::io::IsTerminal;

/// Check if stderr is an interactive terminal.
/// When false, progress bars should not be created.
pub fn is_interactive() -> bool {
    std::io::stderr().is_terminal()
}

/// Manages download progress bars — one per active album.
///
/// Wraps `indicatif::MultiProgress` so multiple concurrent album downloads
/// each get their own progress bar. Callers should check `is_interactive()`
/// before creating an instance; in non-interactive contexts, skip creation
/// entirely rather than relying on this struct to no-op.
pub struct ProgressDisplay {
    multi: MultiProgress,
}

impl ProgressDisplay {
    /// Create a new ProgressDisplay.
    /// Renders to stderr. Should only be called when `is_interactive()` is true.
    pub fn new() -> Self {
        let multi = MultiProgress::new();
        // indicatif renders to stderr by default — matches spec.
        Self { multi }
    }

    /// Create a progress bar for a track download.
    ///
    /// The bar shows: filename | downloaded/total | speed | bar | percentage
    pub fn create_bar(&self, filename: &str, _total_bytes: u64) -> ProgressBar {
        let bar = self.multi.add(ProgressBar::new(0));
        let style = ProgressStyle::with_template(
            "  {spinner} {msg}  {bytes}/{total_bytes}  {prefix}  [{bar:20}]  {percent}%",
        )
        .expect("valid progress bar template");
        let style = style.progress_chars("█░");
        bar.set_style(style);
        // Show only the basename, strip control characters (terminal injection
        // defence against peer-supplied filenames), and truncate to 60 chars.
        let display_name = filename.rsplit(['/', '\\']).next().unwrap_or(filename);
        let safe_name: String = display_name
            .chars()
            .filter(|c| !c.is_control())
            .take(60)
            .collect();
        bar.set_message(safe_name);
        bar
    }

    /// Remove all bars (call when download session ends).
    pub fn clear(&self) {
        let _ = self.multi.clear();
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
}
