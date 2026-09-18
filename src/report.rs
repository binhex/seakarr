// src/report.rs

use std::path::PathBuf;

/// Where a completed album ended up.
///
/// The two variants are deliberately distinct rather than a path plus a boolean:
/// the completion log, the run summary, and the notification all phrase the
/// destination differently, and a staging path that read as a library location is
/// the confusion this type exists to prevent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownloadDestination {
    /// Written into the library at this album folder.
    Library(PathBuf),
    /// Deliberately left in staging; this is the album's final location.
    Staging(PathBuf),
}

impl DownloadDestination {
    /// The path to report, with the staging form marked so it cannot be read as
    /// a library location.
    #[must_use]
    pub fn render(&self) -> String {
        match self {
            Self::Library(path) => path.display().to_string(),
            Self::Staging(path) => format!("{} (kept in staging)", path.display()),
        }
    }
}

/// Outcome of processing a single album.
#[derive(Debug, Clone, PartialEq)]
pub enum AlbumOutcome {
    Downloaded {
        track_count: usize,
        destination: DownloadDestination,
    },
    Skipped,
    Failed {
        reason: String,
    },
    /// Search produced nothing, or nothing that survived filtering, so no
    /// download was attempted.
    ///
    /// Rendered in the summary exactly like [`AlbumOutcome::Failed`]. It exists
    /// as a distinct variant so `discover` can tell an album that never reached
    /// the download stage from one that did, and decline to charge its download
    /// budget for work that never happened.
    NoCandidates {
        reason: String,
    },
}

/// One successfully downloaded album and where it landed.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DownloadedAlbum {
    artist: String,
    album: String,
    track_count: usize,
    destination: DownloadDestination,
}

/// Collects album outcomes during a run and prints a summary.
#[derive(Debug, Default)]
pub struct RunReport {
    downloaded: Vec<DownloadedAlbum>,      // completion order
    skipped: Vec<(String, String)>,        // (artist, album)
    failed: Vec<(String, String, String)>, // (artist, album, reason)
    notices: Vec<String>,
}

impl RunReport {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record an album outcome.
    pub fn record(&mut self, artist: &str, album: &str, outcome: AlbumOutcome) {
        match outcome {
            AlbumOutcome::Downloaded {
                track_count,
                destination,
            } => {
                self.downloaded.push(DownloadedAlbum {
                    artist: artist.to_string(),
                    album: album.to_string(),
                    track_count,
                    destination,
                });
            }
            AlbumOutcome::Skipped => {
                self.skipped.push((artist.to_string(), album.to_string()));
            }
            AlbumOutcome::Failed { reason } | AlbumOutcome::NoCandidates { reason } => {
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
            lines.extend(self.downloaded.iter().map(|entry| {
                format!(
                    "  {} — {} ({} tracks) -> {}",
                    entry.artist,
                    entry.album,
                    entry.track_count,
                    entry.destination.render()
                )
            }));
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
            AlbumOutcome::Downloaded {
                track_count: 10,
                destination: DownloadDestination::Staging(PathBuf::from("/downloads/album")),
            },
        );
        assert_eq!(report.downloaded_count(), 1);
        assert_eq!(report.skipped_count(), 0);
        assert_eq!(report.failed_count(), 0);
    }

    #[test]
    fn summary_renders_the_library_destination() {
        let mut report = RunReport::new();
        report.record(
            "Aquasky",
            "Shadow Era Pt. 1",
            AlbumOutcome::Downloaded {
                track_count: 8,
                destination: DownloadDestination::Library(PathBuf::from(
                    "/media/Music/Paul/Albums/Aquasky/Shadow Era Pt. 1",
                )),
            },
        );
        assert_eq!(
            report.summary_lines(),
            vec![
                "=== Run summary ===".to_string(),
                "Downloaded (1):".to_string(),
                "  Aquasky — Shadow Era Pt. 1 (8 tracks) -> \
                 /media/Music/Paul/Albums/Aquasky/Shadow Era Pt. 1"
                    .to_string(),
            ]
        );
    }

    #[test]
    fn summary_marks_a_staging_destination_as_kept_in_staging() {
        // A staging path is a real final destination, but it must not read as a
        // library location — that confusion is the defect this work fixes.
        let mut report = RunReport::new();
        report.record(
            "Aquasky",
            "Shadow Era Pt. 2",
            AlbumOutcome::Downloaded {
                track_count: 6,
                destination: DownloadDestination::Staging(PathBuf::from(
                    "/downloads/Aquasky--Shadow Era Pt. 2",
                )),
            },
        );
        assert_eq!(
            report.summary_lines(),
            vec![
                "=== Run summary ===".to_string(),
                "Downloaded (1):".to_string(),
                "  Aquasky — Shadow Era Pt. 2 (6 tracks) -> \
                 /downloads/Aquasky--Shadow Era Pt. 2 (kept in staging)"
                    .to_string(),
            ]
        );
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
    fn no_candidates_renders_in_the_failed_section() {
        let mut report = RunReport::new();
        report.record(
            "Artist",
            "Album",
            AlbumOutcome::NoCandidates {
                reason: "no results found".into(),
            },
        );
        assert_eq!(report.failed_count(), 1);
        assert_eq!(
            report.summary_lines(),
            vec![
                "=== Run summary ===".to_string(),
                "Failed (1):".to_string(),
                // The renderer joins artist and album with U+2014.
                "  Artist \u{2014} Album (no results found)".to_string(),
            ]
        );
    }

    #[test]
    fn test_mixed_outcomes() {
        let mut report = RunReport::new();
        report.record(
            "A",
            "1",
            AlbumOutcome::Downloaded {
                track_count: 5,
                destination: DownloadDestination::Staging(PathBuf::from("/downloads/album")),
            },
        );
        report.record("B", "2", AlbumOutcome::Skipped);
        report.record(
            "C",
            "3",
            AlbumOutcome::Failed {
                reason: "timeout".into(),
            },
        );
        report.record(
            "D",
            "4",
            AlbumOutcome::Downloaded {
                track_count: 8,
                destination: DownloadDestination::Staging(PathBuf::from("/downloads/album")),
            },
        );
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
        report.record(
            "Z",
            "first",
            AlbumOutcome::Downloaded {
                track_count: 1,
                destination: DownloadDestination::Staging(PathBuf::from("/downloads/album")),
            },
        );
        report.record("A", "second", AlbumOutcome::Skipped);
        // Entries should be in the order they were recorded.
        assert_eq!(report.downloaded[0].artist, "Z");
        assert_eq!(report.downloaded[0].album, "first");
        assert_eq!(report.downloaded[0].track_count, 1);
        assert_eq!(report.skipped[0], ("A".to_string(), "second".to_string()));
    }
}
