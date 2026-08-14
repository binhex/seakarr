use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::Semaphore;

use futures::FutureExt;

use crate::client::SoulseekClient;
use crate::config::Config;
use crate::db::Database;
use crate::error::{Result, SeakarrError};
use crate::progress::{is_interactive, ProgressDisplay};
use crate::report::{AlbumOutcome, RunReport};
use crate::{download, filter, notifier, organizer, scanner, search};

/// Spawn a SIGINT (Ctrl+C) listener for the duration of a run.
///
/// The first press sets the shared cancellation flag, aborting in-flight
/// downloads (their staging dirs are cleaned by `download_album`). A second
/// press force-exits the process — the run may be wedged on a network call
/// that ignores the flag, and Ctrl+C must always be able to terminate
/// seakarr.
pub fn spawn_cancel_listener(cancel: Arc<AtomicBool>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match tokio::signal::ctrl_c().await {
                Ok(()) => {
                    if cancel.swap(true, Ordering::SeqCst) {
                        // The first press already requested graceful
                        // cancellation; the run may be wedged on a network
                        // call that ignores the flag, so a second press must
                        // always terminate seakarr.
                        tracing::info!("Received second SIGINT — forcing exit");
                        std::process::exit(130);
                    }
                    tracing::info!("Received SIGINT — aborting in-flight downloads...");
                }
                Err(e) => {
                    // Signal driver unavailable — cancellation via Ctrl+C
                    // will not work. Log and exit the listener loop so the
                    // caller is not left waiting for a flag that never flips.
                    tracing::warn!("Failed to register SIGINT handler: {e} — Ctrl+C will not work");
                    return;
                }
            }
        }
    })
}

/// Process a single album: search → filter rank → download → organize → notify.
#[allow(clippy::too_many_arguments)]
pub async fn process_album(
    client: &dyn SoulseekClient,
    artist: &str,
    album: Option<&str>,
    config: &Config,
    db: &Database,
    staging_dir: &Path,
    progress: Option<&ProgressDisplay>,
    cancel: Option<&Arc<AtomicBool>>,
    library_track_count: Option<usize>,
) -> Result<AlbumOutcome> {
    // Skip if already processed
    if let Some(a) = album {
        if db.is_album_processed(artist, a)? {
            tracing::info!("Skipping already-processed: {artist} — {a}");
            return Ok(AlbumOutcome::Skipped);
        }
    }

    tracing::info!("Processing: {artist} — {}", album.unwrap_or("(all)"));

    // Create per-album staging subdirectory to prevent filename collisions
    // when multiple albums are processed concurrently (e.g. two albums both
    // containing "01 - Intro.flac").
    let album_slug = format!(
        "{}--{}",
        artist.replace('/', "-"),
        album.unwrap_or("unknown").replace('/', "-")
    );
    let album_staging = staging_dir.join(&album_slug);
    // Note: album_staging directory is created by download_album, only when
    // a valid peer with downloadable files is found. This prevents empty
    // staging directories from accumulating for albums with no results.

    // Search, with an album-only fallback for banned artist+album criteria.
    // Both searches are recorded in search_history; when the fallback fires
    // the primary row gets result_count 0 and the fallback row its matched
    // count, making fallback usage visible in the history table.
    let search_start = std::time::Instant::now();
    let outcome = search::search_album_with_fallback(
        client,
        artist,
        album,
        config.search.timeout_secs,
        config.search.fallback_search,
    )
    .await?;
    let duration_ms = search_start.elapsed().as_millis() as u64;
    // Store the canonical (trimmed) album in history so rows match the
    // album-only fallback query, which trims padded tag metadata.
    let history_album = album.map(str::trim);
    // When the fallback ran, the primary row gets only the primary search's
    // own duration (total minus fallback), so the two history rows don't
    // double-count the fallback time.
    let primary_duration_ms = duration_ms.saturating_sub(outcome.fallback_duration_ms.unwrap_or(0));
    if let Err(e) = search::record_search(
        artist,
        history_album,
        if outcome.used_fallback {
            0
        } else {
            outcome.results.len()
        },
        primary_duration_ms,
        db,
    ) {
        tracing::warn!(
            "{artist} — {}: failed to record primary search history: {e}",
            album.unwrap_or("(all)")
        );
    }
    if outcome.used_fallback {
        tracing::info!(
            "{artist} — {}: fallback album-only search found {} result(s) matching artist in path",
            album.unwrap_or("(all)"),
            outcome.results.len(),
        );
        if let Err(e) = search::record_search(
            artist,
            history_album,
            outcome.results.len(),
            outcome.fallback_duration_ms.unwrap_or(duration_ms),
            db,
        ) {
            tracing::warn!(
                "{artist} — {}: failed to record fallback search history: {e}",
                album.unwrap_or("(all)")
            );
        }
    }
    let results = outcome.results;
    if results.is_empty() {
        tracing::info!("No results for {artist} — {}", album.unwrap_or("(all)"));
        if let Some(a) = album {
            db.mark_album_processed(artist, a, "failed")?;
        }
        return Ok(AlbumOutcome::Failed {
            reason: "no results found".into(),
        });
    }

    // Filter + rank
    let total_results: usize = results.iter().map(|r| r.files.len()).sum();
    let total_users = results.len();
    let mut filtered = filter::filter_results(&results, &config.filters, library_track_count);
    // Second-chance fallback: when the primary search returned results but
    // every one was rejected (e.g. by the contiguity gate), the album-only
    // fallback query may find a complete share that the combined query
    // missed. Only fires when the fallback has not already been used.
    if filtered.is_empty()
        && !outcome.used_fallback
        && config.search.fallback_search
        && !artist.trim().is_empty()
    {
        if let Some(album_name) = album.map(str::trim).filter(|a| !a.is_empty()) {
            match search::search_fallback_only(
                client,
                artist,
                album_name,
                config.search.timeout_secs,
            )
            .await
            {
                Ok(fallback_results) => {
                    tracing::info!(
                        "{artist} — {}: all primary results rejected; fallback album-only search found {} result(s)",
                        album.unwrap_or("(all)"),
                        fallback_results.len(),
                    );
                    filtered = filter::filter_results(
                        &fallback_results,
                        &config.filters,
                        library_track_count,
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        "{artist} — {}: second-chance fallback search failed: {e}",
                        album.unwrap_or("(all)")
                    );
                }
            }
        }
    }
    if filtered.is_empty() {
        let contiguity_note = if config.filters.contiguous_tracks {
            ", contiguous track numbers"
        } else {
            ""
        };
        tracing::info!(
            "{artist} — {}: {total_results} files from {total_users} users, 0 passed filters (need: {:?} format, free slot{contiguity_note})",
            album.unwrap_or("(all)"),
            config.filters.allowed_extensions,
        );
        if let Some(a) = album {
            db.mark_album_processed(artist, a, "failed")?;
        }
        return Ok(AlbumOutcome::Failed {
            reason: "no results passed filters".into(),
        });
    }
    let ranked = filter::rank_candidates(&filtered, &config.filters);
    tracing::info!(
        "{artist} — {}: {total_results} files from {total_users} users, {} users passed filters, best: {} (speed={})",
        album.unwrap_or("(all)"),
        filtered.len(),
        ranked.first().map(|r| r.username.as_str()).unwrap_or("?"),
        ranked.first().map(|r| r.speed).unwrap_or(0),
    );

    // Download
    let downloaded = match download::download_album(
        client,
        &ranked,
        &album_staging,
        &config.download,
        &config.filters,
        progress,
        cancel,
    )
    .await
    {
        Ok(files) => files,
        Err(e) => {
            let reason = if e.to_string().contains("cancelled") {
                e.to_string()
            } else {
                format!("all candidates exhausted: {e}")
            };
            tracing::warn!(
                "{artist} — {}: download failed ({reason}); {} candidates exhausted",
                album.unwrap_or("(all)"),
                ranked.len(),
            );
            return Ok(AlbumOutcome::Failed { reason });
        }
    };

    // Organize (if enabled)
    let mut organize_ok = true;
    if config.storage.organize && !config.library.paths.is_empty() {
        let lib_root = Path::new(&config.library.paths[0]);
        for path in &downloaded {
            // Extract metadata from filename for pattern
            let stem = path.file_stem().unwrap_or_default().to_string_lossy();
            let ext = path.extension().unwrap_or_default().to_string_lossy();
            match organizer::organize_file(organizer::OrganizeInput {
                src: path,
                library_root: lib_root,
                pattern: &config.storage.organize_pattern,
                artist,
                album: album.unwrap_or("Unknown"),
                track: "01",
                title: &stem,
                ext: &ext,
            }) {
                Ok(_) => {}
                Err(e) => {
                    tracing::error!(
                        "Failed to organize {path:?} for {artist}/{}: {e}",
                        album.unwrap_or("Unknown")
                    );
                    organize_ok = false;
                }
            }
        }
    }

    // Mark processed — only success if organize also succeeded.
    // When album is None (manual mode without --album), mark_album_processed
    // is skipped (no DB row to update).
    if organize_ok {
        if let Some(a) = album {
            db.mark_album_processed(artist, a, "success")?;
        }
        // Remove the staging directory — files have been organized into the
        // library. Absence of the staging dir signals a completed download.
        if config.storage.organize && !config.library.paths.is_empty() {
            if let Err(e) = std::fs::remove_dir_all(&album_staging) {
                tracing::warn!("Failed to remove staging dir {album_staging:?}: {e}");
            }
        }
    } else if let Some(a) = album {
        db.mark_album_processed(artist, a, "failed")?;
    }
    if !organize_ok {
        return Ok(AlbumOutcome::Failed {
            reason: "download succeeded but file organization failed".into(),
        });
    }

    // Notify — log failure but don't propagate; the download succeeded
    // and is already marked success in the DB. Pre-change behaviour:
    // notify errors were also non-fatal to the album outcome.
    let track_count = downloaded.len();
    if let Err(e) = notifier::notify_success(
        &config.notifications.urls,
        artist,
        album.unwrap_or("Unknown"),
        track_count,
    )
    .await
    {
        tracing::warn!(
            "{artist} — {}: notification failed: {e}",
            album.unwrap_or("(all)")
        );
    }

    tracing::info!(
        "Completed: {artist} — {} ({track_count} tracks)",
        album.unwrap_or("(all)")
    );
    Ok(AlbumOutcome::Downloaded { track_count })
}

/// Run in automatic mode: scan library, find upgrades, process each album concurrently.
pub async fn run_auto_mode(
    client: &dyn SoulseekClient,
    config: &Config,
    db: &Database,
) -> Result<()> {
    if config.library.paths.is_empty() {
        return Err(SeakarrError::Config(
            "library.paths is empty — nothing to scan".into(),
        ));
    }

    // Scan library
    tracing::info!("Scanning library...");
    let albums = scanner::scan_library(&config.library.paths)?;
    let targets_with_counts = scanner::find_albums_to_upgrade(&albums, &config.filters);
    for album in &albums {
        let fmt_str: Vec<&str> = album.formats.iter().map(|f| f.as_str()).collect();
        tracing::info!(
            "  {artist} — {album} ({tracks} tracks, formats: {formats}, bitrate: {bitrate:?})",
            artist = album.artist,
            album = album.album,
            tracks = album.track_count,
            formats = fmt_str.join(","),
            bitrate = album.min_bitrate,
        );
    }
    tracing::info!(
        "Found {} albums to upgrade out of {} total",
        targets_with_counts.len(),
        albums.len()
    );

    if targets_with_counts.is_empty() {
        tracing::info!("Nothing to upgrade.");
        return Ok(());
    }

    // Process concurrently with bounded concurrency.
    //
    // NOTE: `tokio::spawn` cannot be used here — the borrowed `&Database` is
    // !Send (rusqlite::Connection is not Sync), and spawn requires 'static
    // futures. Instead we build !Send boxed local futures that borrow
    // `client`/`config`/`db` and poll them cooperatively in this task via
    // `join_all` (FuturesUnordered under the hood), bounding the number of
    // albums in flight with a shared tokio semaphore.
    let staging_dir = Path::new(&config.storage.staging_dir);
    std::fs::create_dir_all(staging_dir)?;

    let progress = if is_interactive() {
        Some(Arc::new(ProgressDisplay::new()))
    } else {
        None
    };

    // Shared cancellation flag: SIGINT (Ctrl+C) sets it, aborting in-flight
    // downloads. Each album's staging dir is cleaned by download_album.
    let cancel = Arc::new(AtomicBool::new(false));
    let _listener = spawn_cancel_listener(Arc::clone(&cancel));

    let semaphore = Arc::new(Semaphore::new(config.download.concurrent.max(1)));

    let targets_vec: Vec<(String, String, usize)> = targets_with_counts;
    let mut futures_vec = Vec::new();

    for (artist, album, track_count) in &targets_vec {
        let semaphore = Arc::clone(&semaphore);
        let progress = progress.clone();
        let cancel = cancel.clone();
        let artist = artist.clone();
        let album = album.clone();
        let library_track_count = *track_count;
        futures_vec.push(
            async move {
                // Park until a permit is free — this is what bounds concurrency.
                let _permit = semaphore
                    .acquire_owned()
                    .await
                    .expect("semaphore is never closed");
                let result = process_album(
                    client,
                    &artist,
                    Some(&album),
                    config,
                    db,
                    staging_dir,
                    progress.as_deref(),
                    Some(&cancel),
                    Some(library_track_count),
                )
                .await;
                (artist, album, result)
            }
            .boxed_local(),
        );
    }

    let results = futures::future::join_all(futures_vec).await;

    if let Some(ref p) = progress {
        p.clear();
    }

    // Collect outcomes into the run report and print the summary once at the
    // end. Environment errors (DB write, search) from inside process_album
    // are recorded as Failed entries; staging-dir creation above also
    // propagates but runs before the report exists (no summary printed).
    let mut report = RunReport::new();
    for (artist, album, result) in results {
        match result {
            Ok(outcome) => report.record(&artist, &album, outcome),
            Err(e) => {
                tracing::error!("Album processing failed: {artist} — {album}: {e}");
                report.record(
                    &artist,
                    &album,
                    AlbumOutcome::Failed {
                        reason: e.to_string(),
                    },
                );
            }
        }
    }
    report.print_summary();

    // Abort the cancel listener so the tokio task does not accumulate across
    // daemon scan cycles (each cycle calls run_auto_mode again). Without
    // abort(), the JoinHandle drop only detaches the task — it keeps running
    // and waiting for SIGINT, leaking one task per scan.
    _listener.abort();

    Ok(())
}

/// Run in manual mode: process a single search term.
pub async fn run_manual_mode(
    client: &dyn SoulseekClient,
    artist: &str,
    album: Option<&str>,
    config: &Config,
    db: &Database,
) -> Result<()> {
    let staging_dir = Path::new(&config.storage.staging_dir);
    std::fs::create_dir_all(staging_dir)?;

    let album_display = album.unwrap_or("(all)");
    let mut report = RunReport::new();

    // Environment errors from inside process_album (DB write, search)
    // are recorded as Failed entries so they appear in the summary,
    // then still propagated so the CLI exits non-zero.
    // Staging-dir creation above also propagates, but runs before the
    // report exists so no summary is printed for that failure.
    let progress = if is_interactive() {
        Some(ProgressDisplay::new())
    } else {
        None
    };
    let progress_ref = progress.as_ref();
    // Cancellation flag: SIGINT aborts the in-flight download; download_album
    // cleans the album's staging dir.
    let cancel = Arc::new(AtomicBool::new(false));
    let _listener = spawn_cancel_listener(Arc::clone(&cancel));
    let result = process_album(
        client,
        artist,
        album,
        config,
        db,
        staging_dir,
        progress_ref,
        Some(&cancel),
        None, // library_track_count (manual mode: no scanner data)
    )
    .await;
    match &result {
        Ok(outcome) => report.record(artist, album_display, outcome.clone()),
        Err(e) => {
            tracing::error!("Manual mode: {artist} — {album_display}: {e}");
            report.record(
                artist,
                album_display,
                AlbumOutcome::Failed {
                    reason: e.to_string(),
                },
            );
        }
    }

    if let Some(ref p) = progress {
        p.clear();
    }

    report.print_summary();
    _listener.abort();
    result.map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{FileInfo, MockClient, SearchResult};
    use crate::config::Config;
    use crate::db::Database;
    use std::collections::HashMap;
    use std::sync::Arc;
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

    fn make_test_config() -> Config {
        let mut config = Config::default();
        config.soulseek.username = "test".into();
        config.soulseek.password = "test".into();
        config.download.concurrent = 2;
        config.download.min_upload_speed_kbps = 0; // disabled
        config.download.speed_check_wait_secs = 0;
        config.download.max_retries = 1;
        config.download.retry_delay_secs = 0;
        config.notifications.urls = vec![];
        // Disable the min_tracks gate for these pipeline tests — they
        // exercise process_album flow with small mock shares, not share
        // completeness.
        config.filters.min_tracks = 0;
        config
    }

    #[tokio::test]
    async fn test_run_manual_mode() {
        let client = Arc::new(MockClient::new());
        *client.search_results.lock().unwrap() = vec![SearchResult {
            username: "user1".into(),
            speed: 500,
            slots: 1,
            files: vec![make_file("01 - track.flac", 900, 10_000_000)],
        }];

        let config = make_test_config();
        let db = Database::open_in_memory().unwrap();
        let staging = TempDir::new().unwrap();

        let result = process_album(
            client.as_ref() as &dyn crate::client::SoulseekClient,
            "Test Artist",
            Some("Test Album"),
            &config,
            &db,
            staging.path(),
            None,
            None,
            None,
        )
        .await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), AlbumOutcome::Downloaded { track_count: 1 });
    }

    #[tokio::test]
    async fn test_fallback_disabled_by_config_issues_single_search() {
        let client = Arc::new(MockClient::new());
        // Even though an album-only query would match, the config disables
        // the fallback: exactly one (primary) query must be issued.
        client.search_results_by_query.lock().unwrap().insert(
            "Test Album".into(),
            vec![SearchResult {
                username: "user1".into(),
                speed: 500,
                slots: 1,
                files: vec![make_file(
                    r"Music\Test Artist\Test Album\01 - track.flac",
                    900,
                    10_000_000,
                )],
            }],
        );

        let mut config = make_test_config();
        config.search.fallback_search = false;
        let db = Database::open_in_memory().unwrap();
        let staging = TempDir::new().unwrap();

        let result = process_album(
            client.as_ref() as &dyn crate::client::SoulseekClient,
            "Test Artist",
            Some("Test Album"),
            &config,
            &db,
            staging.path(),
            None,
            None,
            None,
        )
        .await;
        assert!(result.is_ok());
        match result.unwrap() {
            AlbumOutcome::Failed { reason } => assert_eq!(reason, "no results found"),
            other => panic!("Expected AlbumOutcome::Failed, got: {other:?}"),
        }

        let queries = client.search_queries.lock().unwrap().clone();
        assert_eq!(queries, vec!["Test Artist Test Album".to_string()]);

        let rows = db.get_processed_albums().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, "failed");
    }

    #[tokio::test]
    async fn test_fallback_download_completes_album_and_records_history() {
        let client = Arc::new(MockClient::new());
        client.search_results_by_query.lock().unwrap().insert(
            "Test Album".into(),
            vec![SearchResult {
                username: "user1".into(),
                speed: 500,
                slots: 1,
                files: vec![
                    make_file(
                        r"Music\Test Artist\Test Album\01 - track.flac",
                        900,
                        10_000_000,
                    ),
                    // A mixed-share decoy: matches the album-only query and
                    // passes quality filters, but the artist is not in the
                    // path. Per-file filtering must keep it out of the
                    // download stage entirely.
                    make_file(
                        r"Music\Someone Else\Test Album\02 - decoy.flac",
                        900,
                        10_000_000,
                    ),
                ],
            }],
        );
        // Primary query "Test Artist Test Album" has no map entry -> empty.

        let config = make_test_config();
        let db = Database::open_in_memory().unwrap();
        let staging = TempDir::new().unwrap();

        let result = process_album(
            client.as_ref() as &dyn crate::client::SoulseekClient,
            "Test Artist",
            Some("Test Album"),
            &config,
            &db,
            staging.path(),
            None,
            None,
            None,
        )
        .await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), AlbumOutcome::Downloaded { track_count: 1 });

        // Fallback fired: primary query then album-only query.
        let queries = client.search_queries.lock().unwrap().clone();
        assert_eq!(
            queries,
            vec![
                "Test Artist Test Album".to_string(),
                "Test Album".to_string()
            ]
        );

        // Per-file filtering held at the download boundary: exactly the
        // artist-matching file was queued for download, never the decoy.
        let downloads = client.download_filenames.lock().unwrap().clone();
        assert_eq!(
            downloads,
            vec![r"Music\Test Artist\Test Album\01 - track.flac".to_string()],
            "only artist-matching files may be downloaded"
        );

        // Album completed successfully.
        let rows = db.get_processed_albums().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, "success");

        // Both searches recorded: primary with 0 results, fallback with 1.
        let history_count: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM search_history", [], |r| r.get(0))
            .unwrap();
        assert_eq!(history_count, 2);
        let fallback_count: i64 = db
            .conn
            .query_row(
                "SELECT result_count FROM search_history WHERE album = 'Test Album' ORDER BY id DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(fallback_count, 1);
    }

    #[tokio::test]
    async fn test_second_chance_fallback_when_primary_all_rejected() {
        let client = Arc::new(MockClient::new());
        // Primary search (static results): gappy tracks 01, 03 — rejected
        // by the contiguity gate.
        *client.search_results.lock().unwrap() = vec![SearchResult {
            username: "gappy-peer".into(),
            speed: 900,
            slots: 1,
            files: vec![
                make_file(
                    r"Music\Test Artist\Test Album\01 - track.flac",
                    900,
                    10_000_000,
                ),
                make_file(
                    r"Music\Test Artist\Test Album\03 - track.flac",
                    900,
                    10_000_000,
                ),
            ],
        }];
        // The album-only fallback finds a complete share.
        client.search_results_by_query.lock().unwrap().insert(
            "Test Album".into(),
            vec![SearchResult {
                username: "complete-peer".into(),
                speed: 500,
                slots: 1,
                files: vec![
                    make_file(
                        r"Music\Test Artist\Test Album\01 - track.flac",
                        900,
                        10_000_000,
                    ),
                    make_file(
                        r"Music\Test Artist\Test Album\02 - track.flac",
                        900,
                        10_000_000,
                    ),
                ],
            }],
        );

        let config = make_test_config();
        let db = Database::open_in_memory().unwrap();
        let staging = TempDir::new().unwrap();

        let result = process_album(
            client.as_ref() as &dyn crate::client::SoulseekClient,
            "Test Artist",
            Some("Test Album"),
            &config,
            &db,
            staging.path(),
            None,
            None,
            None,
        )
        .await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), AlbumOutcome::Downloaded { track_count: 2 });

        // Both the primary and the second-chance fallback query ran.
        let queries = client.search_queries.lock().unwrap().clone();
        assert_eq!(
            queries,
            vec![
                "Test Artist Test Album".to_string(),
                "Test Album".to_string()
            ]
        );
        // The complete fallback share was downloaded, album succeeded.
        let rows = db.get_processed_albums().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, "success");
        let downloads = client.download_filenames.lock().unwrap().clone();
        assert_eq!(downloads.len(), 2, "complete fallback share downloaded");
    }

    #[tokio::test]
    async fn test_fallback_no_matches_marks_failed() {
        let client = Arc::new(MockClient::new());
        client.search_results_by_query.lock().unwrap().insert(
            "Test Album".into(),
            vec![SearchResult {
                username: "user1".into(),
                speed: 500,
                slots: 1,
                files: vec![make_file(
                    r"Music\Someone Else\Test Album\01 - track.flac",
                    900,
                    10_000_000,
                )],
            }],
        );

        let config = make_test_config();
        let db = Database::open_in_memory().unwrap();
        let staging = TempDir::new().unwrap();

        let result = process_album(
            client.as_ref() as &dyn crate::client::SoulseekClient,
            "Test Artist",
            Some("Test Album"),
            &config,
            &db,
            staging.path(),
            None,
            None,
            None,
        )
        .await;
        assert!(result.is_ok());
        match result.unwrap() {
            AlbumOutcome::Failed { reason } => assert_eq!(reason, "no results found"),
            other => panic!("Expected AlbumOutcome::Failed, got: {other:?}"),
        }

        // The fallback fired: primary query then album-only query.
        let queries = client.search_queries.lock().unwrap().clone();
        assert_eq!(
            queries,
            vec![
                "Test Artist Test Album".to_string(),
                "Test Album".to_string()
            ]
        );

        let rows = db.get_processed_albums().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, "failed");
    }

    #[tokio::test]
    async fn test_fallback_with_gappy_tracks_marks_failed() {
        let client = Arc::new(MockClient::new());
        client.search_results_by_query.lock().unwrap().insert(
            "Test Album".into(),
            vec![SearchResult {
                username: "user1".into(),
                speed: 500,
                slots: 1,
                files: vec![
                    make_file(
                        r"Music\Test Artist\Test Album\01 - track.flac",
                        900,
                        10_000_000,
                    ),
                    make_file(
                        r"Music\Test Artist\Test Album\03 - track.flac",
                        900,
                        10_000_000,
                    ),
                ],
            }],
        );
        // Primary query "Test Artist Test Album" has no map entry -> empty.

        let config = make_test_config();
        let db = Database::open_in_memory().unwrap();
        let staging = TempDir::new().unwrap();

        let result = process_album(
            client.as_ref() as &dyn crate::client::SoulseekClient,
            "Test Artist",
            Some("Test Album"),
            &config,
            &db,
            staging.path(),
            None,
            None,
            None,
        )
        .await;
        assert!(result.is_ok());
        match result.unwrap() {
            AlbumOutcome::Failed { reason } => assert_eq!(reason, "no results passed filters"),
            other => panic!("Expected AlbumOutcome::Failed, got: {other:?}"),
        }

        // Gappy track set rejected at the filter stage -> album failed,
        // nothing downloaded.
        let rows = db.get_processed_albums().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, "failed");
        assert!(client.download_filenames.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_runner_handles_empty_targets() {
        let client = Arc::new(MockClient::new());
        let mut config = make_test_config();
        // Point at an empty directory: nothing to scan -> no upgrade targets -> Ok.
        let tmp = TempDir::new().unwrap();
        config.library.paths = vec![tmp.path().to_string_lossy().into()];

        let db = Database::open_in_memory().unwrap();

        // No targets — should not panic or error
        let result = run_auto_mode(
            client.as_ref() as &dyn crate::client::SoulseekClient,
            &config,
            &db,
        )
        .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_run_auto_mode_processes_album_and_marks_success() {
        let client = Arc::new(MockClient::new());
        *client.search_results.lock().unwrap() = vec![SearchResult {
            username: "user1".into(),
            speed: 500,
            slots: 1,
            files: vec![make_file("01 - track.flac", 900, 10_000_000)],
        }];

        let mut config = make_test_config();
        let tmp = TempDir::new().unwrap();
        // Library layout: <tmp>/Test Artist/Test Album/01 - track.mp3
        // mp3 is not in allowed_extensions (default [flac]) so the album is
        // flagged for upgrade; the mock search supplies the flac result.
        let artist_dir = tmp.path().join("Test Artist").join("Test Album");
        std::fs::create_dir_all(&artist_dir).unwrap();
        std::fs::write(artist_dir.join("01 - track.mp3"), b"fake mp3 data").unwrap();
        config.library.paths = vec![tmp.path().to_string_lossy().into()];

        let db = Database::open_in_memory().unwrap();

        let result = run_auto_mode(
            client.as_ref() as &dyn crate::client::SoulseekClient,
            &config,
            &db,
        )
        .await;
        assert!(result.is_ok());

        // Album processed successfully through the outcome-collection path.
        let rows = db.get_processed_albums().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, "success");
    }

    #[tokio::test]
    async fn test_process_album_returns_failed_when_download_exhausted() {
        let client = Arc::new(MockClient::new());
        // Slow download speed triggers speed-check failure.
        *client.download_speed.lock().unwrap() = 100_000; // 100 KB/s
        *client.search_results.lock().unwrap() = vec![SearchResult {
            username: "user1".into(),
            speed: 500,
            slots: 1,
            files: vec![make_file("01 - track.flac", 900, 10_000_000)],
        }];

        let mut config = make_test_config();
        // Require impossibly fast upload → download fails → candidates exhausted.
        config.download.min_upload_speed_kbps = 10_000_000;
        config.download.speed_check_wait_secs = 0;
        config.download.max_retries = 1;
        config.download.retry_delay_secs = 0;

        let db = Database::open_in_memory().unwrap();
        let staging = TempDir::new().unwrap();

        let result = process_album(
            client.as_ref() as &dyn crate::client::SoulseekClient,
            "Test Artist",
            Some("Test Album"),
            &config,
            &db,
            staging.path(),
            None,
            None,
            None,
        )
        .await;
        assert!(result.is_ok());
        let outcome = result.unwrap();
        match outcome {
            AlbumOutcome::Failed { reason } => {
                assert!(
                    reason.contains("all candidates exhausted"),
                    "Expected 'all candidates exhausted' in reason, got: {reason}"
                );
            }
            other => panic!("Expected AlbumOutcome::Failed, got: {other:?}"),
        }
    }

    // Regression guard: the first SIGINT must set the cancellation flag so
    // in-flight downloads abort gracefully. Runs in a child process — raising
    // SIGINT in the shared test process would also hit other tests' listeners
    // (cancelling their in-flight albums) when tests run in parallel.
    #[cfg(unix)]
    #[test]
    fn cancel_listener_sets_flag_on_first_sigint() {
        if std::env::var("SEAKARR_SIGINT_FLAG_CHILD").is_ok() {
            // Child branch: run the real listener, report when the flag is set.
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let cancel = Arc::new(AtomicBool::new(false));
                let _listener = spawn_cancel_listener(Arc::clone(&cancel));
                // Yield so the runtime polls the listener and registers the
                // SIGINT handler before we signal readiness.
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                println!("READY");
                use std::io::Write;
                std::io::stdout().flush().unwrap();
                // Wait for the first SIGINT to set the flag.
                for _ in 0..100 {
                    if cancel.load(Ordering::SeqCst) {
                        println!("FLAG_SET");
                        std::io::stdout().flush().unwrap();
                        return;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
                panic!("cancel flag was not set after SIGINT");
            });
            return;
        }

        // Parent branch: spawn the child, wait for READY, send one SIGINT,
        // expect FLAG_SET.
        let exe = std::env::current_exe().unwrap();
        let mut child = std::process::Command::new(exe)
            .arg("--exact")
            .arg("runner::tests::cancel_listener_sets_flag_on_first_sigint")
            .arg("--nocapture")
            .env("SEAKARR_SIGINT_FLAG_CHILD", "1")
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("failed to spawn child");

        let pid = child.id() as i32;
        use std::io::BufRead;
        let mut flag_line = String::new();
        {
            let mut stdout = std::io::BufReader::new(child.stdout.as_mut().unwrap());
            // Skip harness banner lines until READY.
            loop {
                let mut line = String::new();
                stdout
                    .read_line(&mut line)
                    .expect("child did not print READY");
                if line.contains("READY") {
                    break;
                }
            }

            unsafe { libc::kill(pid, libc::SIGINT) };

            // The child must observe the flag and print FLAG_SET within 10 s.
            stdout
                .read_line(&mut flag_line)
                .expect("child did not print FLAG_SET");
        }
        assert!(
            flag_line.contains("FLAG_SET"),
            "first SIGINT must set the cancel flag, got: {flag_line:?}"
        );
        let status = child.wait().expect("child did not exit");
        assert!(
            status.success(),
            "child should exit cleanly, got {status:?}"
        );
    }

    // Regression guard: Ctrl+C must always be able to terminate seakarr.
    // The first press requests graceful cancellation; a second press must
    // force-exit the process (exit code 130). With the old single-shot
    // listener the second SIGINT was swallowed by tokio's signal handler and
    // the process stayed alive forever.
    #[cfg(unix)]
    #[test]
    fn second_sigint_forces_process_exit() {
        if std::env::var("SEAKARR_SIGINT_CHILD").is_ok() {
            // Child branch: run the real listener, then sleep — only a forced
            // exit can end the process.
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let cancel = Arc::new(AtomicBool::new(false));
                let _listener = spawn_cancel_listener(Arc::clone(&cancel));
                // Yield so the runtime polls the listener task and registers
                // the SIGINT handler before we signal readiness — otherwise
                // the first SIGINT hits the default handler and kills the
                // child, masking what the test is checking.
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                println!("READY");
                use std::io::Write;
                std::io::stdout().flush().unwrap();
                tokio::time::sleep(std::time::Duration::from_secs(300)).await;
            });
            panic!("child should have been force-exited by the second SIGINT");
        }

        // Parent branch: spawn the child (this test, exactly), wait for it to
        // arm the listener, then send two SIGINTs. The second must exit 130.
        let exe = std::env::current_exe().unwrap();
        let mut child = std::process::Command::new(exe)
            .arg("--exact")
            .arg("runner::tests::second_sigint_forces_process_exit")
            .arg("--nocapture")
            .env("SEAKARR_SIGINT_CHILD", "1")
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("failed to spawn child");

        use std::io::BufRead;
        let mut ready_line = String::new();
        {
            let stdout = child.stdout.as_mut().unwrap();
            let mut reader = std::io::BufReader::new(stdout);
            // Skip harness banner lines until the child's READY marker.
            loop {
                ready_line.clear();
                reader
                    .read_line(&mut ready_line)
                    .expect("child did not print READY");
                if ready_line.contains("READY") {
                    break;
                }
            }
        }
        assert!(
            ready_line.contains("READY"),
            "child did not become ready: {ready_line:?}"
        );

        let pid = child.id() as i32;
        unsafe { libc::kill(pid, libc::SIGINT) };
        // Give the first press time to be processed before the second.
        std::thread::sleep(std::time::Duration::from_millis(500));
        unsafe { libc::kill(pid, libc::SIGINT) };

        // The second SIGINT must terminate the child with exit code 130. Poll
        // try_wait so a hang surfaces as a timeout rather than blocking.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            match child.try_wait().unwrap() {
                Some(status) => {
                    assert_eq!(
                        status.code(),
                        Some(130),
                        "second SIGINT must force-exit with code 130, got {status:?}"
                    );
                    break;
                }
                None => {
                    if std::time::Instant::now() > deadline {
                        let _ = child.kill();
                        panic!(
                            "child did not exit after two SIGINTs (second press swallowed — hang reproduced)"
                        );
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            }
        }
    }
}
