// tests/pipeline_test.rs — End-to-end pipeline tests using the mock client.
// These exercise the runner's public API through the `seakarr::` crate root.
use seakarr::client::{FileInfo, MockClient, SearchResult};
use seakarr::config::Config;
use seakarr::db::Database;
use seakarr::report::AlbumOutcome;
use std::collections::HashMap;
use tempfile::TempDir;

fn make_file(name: &str, bitrate: u32, size: u64) -> FileInfo {
    let mut attribs = HashMap::new();
    attribs.insert(0, bitrate);
    FileInfo {
        name: name.into(),
        size,
        attribs,
    }
}

#[tokio::test]
async fn test_full_pipeline_manual_mode() {
    let client = MockClient::new();
    *client.search_results.lock().unwrap() = vec![SearchResult {
        username: "fastuser".into(),
        speed: 1000,
        slots: 2,
        files: vec![
            make_file(
                r"Test Artist\Test Album\01 - Track One.flac",
                900,
                15_000_000,
            ),
            make_file(
                r"Test Artist\Test Album\02 - Track Two.flac",
                850,
                12_000_000,
            ),
        ],
    }];

    let staging = TempDir::new().unwrap();
    let mut config = Config::default();
    config.soulseek.username = "test".into();
    config.soulseek.password = "test".into();
    config.storage.staging_dir = staging.path().to_string_lossy().into();
    config.download.min_upload_speed_kbps = 0;
    config.download.max_retries = 1;
    config.notifications.urls = vec![];
    // Disable min_tracks gate — this test uses a 2-file mock share.
    config.filters.min_tracks = 0;

    let db = Database::open_in_memory().unwrap();

    let result = seakarr::runner::process_album(
        &client,
        "Test Artist",
        Some("Test Album"),
        &config,
        &db,
        staging.path(),
        None,
        None,
        None, // library_track_count (not applicable in manual mode)
    )
    .await;

    assert!(result.is_ok());
    assert_eq!(result.unwrap(), AlbumOutcome::Downloaded { track_count: 2 });

    // Album should be marked as processed
    assert!(db.is_album_processed("Test Artist", "Test Album").unwrap());
}

#[tokio::test]
async fn test_full_pipeline_auto_mode_no_results() {
    let client = MockClient::new();
    // No search results added — should handle gracefully

    let staging = TempDir::new().unwrap();
    let mut config = Config::default();
    config.soulseek.username = "test".into();
    config.soulseek.password = "test".into();
    config.storage.staging_dir = staging.path().to_string_lossy().into();

    let db = Database::open_in_memory().unwrap();

    let result = seakarr::runner::process_album(
        &client,
        "Obscure Artist",
        Some("Nonexistent Album"),
        &config,
        &db,
        staging.path(),
        None,
        None,
        None, // library_track_count (not applicable in manual mode)
    )
    .await;

    // Should succeed even with no results (marked as failed, not skipped)
    assert!(result.is_ok());
    match result.unwrap() {
        AlbumOutcome::Failed { reason } => assert_eq!(reason, "no results found"),
        other => panic!("Expected AlbumOutcome::Failed, got: {other:?}"),
    }

    // Only the primary search fires (no fallback).
    let history_count: i64 = db
        .conn
        .query_row("SELECT COUNT(*) FROM search_history", [], |r| r.get(0))
        .unwrap();
    assert_eq!(history_count, 1);
}
