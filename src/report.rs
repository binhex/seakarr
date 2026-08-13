// src/report.rs

/// Outcome of processing a single album.
#[derive(Debug, Clone, PartialEq)]
pub enum AlbumOutcome {
    Downloaded { track_count: usize },
    Skipped,
    Failed { reason: String },
}

/// Collects album outcomes during a run and prints a summary.
#[derive(Debug, Default)]
pub struct RunReport {
    downloaded: Vec<(String, String, usize)>, // (artist, album, track_count)
    skipped: Vec<(String, String)>,           // (artist, album)
    failed: Vec<(String, String, String)>,    // (artist, album, reason)
}

impl RunReport {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record an album outcome.
    pub fn record(&mut self, artist: &str, album: &str, outcome: AlbumOutcome) {
        match outcome {
            AlbumOutcome::Downloaded { track_count } => {
                self.downloaded
                    .push((artist.to_string(), album.to_string(), track_count));
            }
            AlbumOutcome::Skipped => {
                self.skipped.push((artist.to_string(), album.to_string()));
            }
            AlbumOutcome::Failed { reason } => {
                self.failed
                    .push((artist.to_string(), album.to_string(), reason));
            }
        }
    }

    /// Print summary via tracing::info!. Omits empty sections. Prints nothing
    /// if no outcomes were recorded.
    pub fn print_summary(&self) {
        let total = self.downloaded.len() + self.skipped.len() + self.failed.len();
        if total == 0 {
            return;
        }

        tracing::info!("=== Run summary ===");

        if !self.downloaded.is_empty() {
            tracing::info!("Downloaded ({}):", self.downloaded.len());
            for (artist, album, track_count) in &self.downloaded {
                tracing::info!("  {artist} — {album} ({track_count} tracks)");
            }
        }

        if !self.skipped.is_empty() {
            tracing::info!("Skipped ({}):", self.skipped.len());
            for (artist, album) in &self.skipped {
                tracing::info!("  {artist} — {album}");
            }
        }

        if !self.failed.is_empty() {
            tracing::info!("Failed ({}):", self.failed.len());
            for (artist, album, reason) in &self.failed {
                tracing::info!("  {artist} — {album} ({reason})");
            }
        }
    }

    // Accessors for testing
    pub fn downloaded_count(&self) -> usize {
        self.downloaded.len()
    }

    pub fn skipped_count(&self) -> usize {
        self.skipped.len()
    }

    pub fn failed_count(&self) -> usize {
        self.failed.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_report_has_no_outcomes() {
        let report = RunReport::new();
        assert_eq!(report.downloaded_count(), 0);
        assert_eq!(report.skipped_count(), 0);
        assert_eq!(report.failed_count(), 0);
    }

    #[test]
    fn test_record_downloaded() {
        let mut report = RunReport::new();
        report.record(
            "Artist A",
            "Album 1",
            AlbumOutcome::Downloaded { track_count: 10 },
        );
        assert_eq!(report.downloaded_count(), 1);
        assert_eq!(report.skipped_count(), 0);
        assert_eq!(report.failed_count(), 0);
    }

    #[test]
    fn test_record_skipped() {
        let mut report = RunReport::new();
        report.record("Artist B", "Album 2", AlbumOutcome::Skipped);
        assert_eq!(report.downloaded_count(), 0);
        assert_eq!(report.skipped_count(), 1);
        assert_eq!(report.failed_count(), 0);
    }

    #[test]
    fn test_record_failed() {
        let mut report = RunReport::new();
        report.record(
            "Artist C",
            "Album 3",
            AlbumOutcome::Failed {
                reason: "all candidates exhausted".into(),
            },
        );
        assert_eq!(report.downloaded_count(), 0);
        assert_eq!(report.skipped_count(), 0);
        assert_eq!(report.failed_count(), 1);
    }

    #[test]
    fn test_mixed_outcomes() {
        let mut report = RunReport::new();
        report.record("A", "1", AlbumOutcome::Downloaded { track_count: 5 });
        report.record("B", "2", AlbumOutcome::Skipped);
        report.record(
            "C",
            "3",
            AlbumOutcome::Failed {
                reason: "timeout".into(),
            },
        );
        report.record("D", "4", AlbumOutcome::Downloaded { track_count: 8 });
        assert_eq!(report.downloaded_count(), 2);
        assert_eq!(report.skipped_count(), 1);
        assert_eq!(report.failed_count(), 1);
    }

    #[test]
    fn test_ordering_preserved() {
        let mut report = RunReport::new();
        report.record("Z", "first", AlbumOutcome::Downloaded { track_count: 1 });
        report.record("A", "second", AlbumOutcome::Skipped);
        // Entries should be in the order they were recorded.
        assert_eq!(
            report.downloaded[0],
            ("Z".to_string(), "first".to_string(), 1)
        );
        assert_eq!(report.skipped[0], ("A".to_string(), "second".to_string()));
    }
}
