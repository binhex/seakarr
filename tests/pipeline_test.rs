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
        false,
        &config,
        &db,
        staging.path(),
        None,
        None,
        None, // library_track_count (not applicable in manual mode)
        None, // target: this direct call writes nothing to the library
    )
    .await;

    assert!(result.is_ok());
    assert!(
        matches!(
            result.unwrap(),
            AlbumOutcome::Downloaded { track_count: 2, .. }
        ),
        "the album must complete with its downloaded track count"
    );

    // Album should be marked as processed
    assert!(db.is_album_processed("Test Artist", "Test Album").unwrap());
}

/// End-to-end regression for library placement (Finding 1 in the release review):
/// with a placement target and real numbered staging files, `process_album` must
/// place each track into the library with a CLEAN name — zero-padded track number
/// and the leading track token stripped from the title. It must never produce the
/// duplicated, unpadded "2 - 02 - Track Two.flac" that shipped when the placement
/// and auto-upgrade paths derived metadata differently.
#[tokio::test]
async fn test_full_pipeline_placement_uses_clean_names() {
    let client = MockClient::new();
    *client.write_files.lock().unwrap() = true; // write real files to staging
    *client.search_results.lock().unwrap() = vec![SearchResult {
        username: "fastuser".into(),
        speed: 1000,
        slots: 2,
        files: vec![
            make_file(
                r"Test Artist\Test Album\02 - Track Two.flac",
                850,
                12_000_000,
            ),
            make_file(
                r"Test Artist\Test Album\01 - Track One.flac",
                900,
                15_000_000,
            ),
        ],
    }];

    let staging = TempDir::new().unwrap();
    let library = TempDir::new().unwrap();
    let mut config = Config::default();
    config.soulseek.username = "test".into();
    config.soulseek.password = "test".into();
    config.storage.staging_dir = staging.path().to_string_lossy().into();
    config.library.paths = vec![library.path().to_string_lossy().into()];
    config.download.min_upload_speed_kbps = 0;
    config.download.max_retries = 1;
    config.notifications.urls = vec![];
    config.filters.min_tracks = 0;

    // Placement writes into an artist folder that already exists; it never
    // creates one, so the fixture provides it.
    std::fs::create_dir_all(library.path().join("Test Artist")).unwrap();
    let target = seakarr::runner::automatic_place_target(
        &seakarr::discover::ArtistFolderIndex::new(&config),
        "Test Artist",
    );

    let db = Database::open_in_memory().unwrap();

    let result = seakarr::runner::process_album(
        &client,
        "Test Artist",
        Some("Test Album"),
        false,
        &config,
        &db,
        staging.path(),
        None,
        None,
        None,
        Some(target),
    )
    .await;

    assert!(result.is_ok());
    assert!(
        matches!(
            result.unwrap(),
            AlbumOutcome::Downloaded { track_count: 2, .. }
        ),
        "the album must complete with its downloaded track count"
    );

    // Placed files carry clean, zero-padded names with the leading track
    // token stripped from the title — never "1 - 01 - Track One.flac".
    let lib_album = library.path().join("Test Artist").join("Test Album");
    assert!(lib_album.join("01 - Track One.flac").exists());
    assert!(lib_album.join("02 - Track Two.flac").exists());
    // No duplicated-number artifacts from a raw-stem title.
    assert!(!lib_album.join("2 - 02 - Track Two.flac").exists());
    assert!(!lib_album.join("1 - 01 - Track One.flac").exists());

    // The staging directory was consumed by the placement.
    let album_staging = staging.path().join("Test Artist--Test Album");
    assert!(!album_staging.exists());

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
        false,
        &config,
        &db,
        staging.path(),
        None,
        None,
        None, // library_track_count (not applicable in manual mode)
        None, // target: this direct call writes nothing to the library
    )
    .await;

    // Should succeed even with no results (marked as failed, not skipped)
    assert!(result.is_ok());
    match result.unwrap() {
        AlbumOutcome::NoCandidates { reason } => assert_eq!(reason, "no results found"),
        other => panic!("Expected AlbumOutcome::NoCandidates, got: {other:?}"),
    }

    // Only the primary search fires (no fallback).
    let history_count: i64 = db
        .conn
        .query_row("SELECT COUNT(*) FROM search_history", [], |r| r.get(0))
        .unwrap();
    assert_eq!(history_count, 1);
}
