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
    notices: Vec<String>,
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

    /// Record a run-level notice, independent of any album outcome.
    pub fn add_notice(&mut self, notice: impl Into<String>) {
        self.notices.push(notice.into());
    }

    pub fn notice_count(&self) -> usize {
        self.notices.len()
    }

    pub fn notices(&self) -> &[String] {
        &self.notices
    }

    fn summary_lines(&self) -> Vec<String> {
        if self.downloaded.is_empty()
            && self.skipped.is_empty()
            && self.failed.is_empty()
            && self.notices.is_empty()
        {
            return Vec::new();
        }
        let mut lines = vec!["=== Run summary ===".to_string()];
        if !self.notices.is_empty() {
            lines.push(format!("Notices ({}):", self.notices.len()));
            lines.extend(self.notices.iter().map(|notice| format!("  {notice}")));
        }
        if !self.downloaded.is_empty() {
            lines.push(format!("Downloaded ({}):", self.downloaded.len()));
            lines.extend(
                self.downloaded
                    .iter()
                    .map(|(artist, album, count)| format!("  {artist} — {album} ({count} tracks)")),
            );
        }
        if !self.skipped.is_empty() {
            lines.push(format!("Skipped ({}):", self.skipped.len()));
            lines.extend(
                self.skipped
                    .iter()
                    .map(|(artist, album)| format!("  {artist} — {album}")),
            );
        }
        if !self.failed.is_empty() {
            lines.push(format!("Failed ({}):", self.failed.len()));
            lines.extend(
                self.failed
                    .iter()
                    .map(|(artist, album, reason)| format!("  {artist} — {album} ({reason})")),
            );
        }
        lines
    }

    /// Print summary via tracing::info!. Omits empty sections. Prints nothing
    /// if no outcomes or notices were recorded.
    pub fn print_summary(&self) {
        for line in self.summary_lines() {
            tracing::info!("{line}");
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
    fn notices_are_retained_even_without_album_outcomes() {
        let mut report = RunReport::new();
        report.add_notice("Authoritative discovery unavailable; used legacy album discovery");
        assert_eq!(report.notice_count(), 1);
        assert_eq!(
            report.notices(),
            ["Authoritative discovery unavailable; used legacy album discovery"]
        );
    }

    #[test]
    fn notice_only_summary_is_rendered() {
        let mut report = RunReport::new();
        report.add_notice("Legacy discovery was used");
        assert_eq!(
            report.summary_lines(),
            vec![
                "=== Run summary ===".to_string(),
                "Notices (1):".to_string(),
                "  Legacy discovery was used".to_string(),
            ]
        );
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
