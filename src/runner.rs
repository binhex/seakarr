use std::path::Path;
use std::sync::Arc;
use tokio::sync::Semaphore;

use futures::FutureExt;

use crate::client::SoulseekClient;
use crate::config::Config;
use crate::db::Database;
use crate::error::{Result, SeakarrError};
use crate::{download, filter, notifier, organizer, scanner, search};

/// Process a single album: search → filter rank → download → organize → notify.
pub async fn process_album(
    client: &dyn SoulseekClient,
    artist: &str,
    album: Option<&str>,
    config: &Config,
    db: &Database,
    staging_dir: &Path,
) -> Result<()> {
    // Skip if already processed
    if let Some(a) = album {
        if db.is_album_processed(artist, a)? {
            tracing::info!("Skipping already-processed: {artist} — {a}");
            return Ok(());
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
    std::fs::create_dir_all(&album_staging)?;

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
            db.mark_album_processed(artist, a, "skipped")?;
        }
        return Ok(());
    }

    // Filter + rank
    let total_results: usize = results.iter().map(|r| r.files.len()).sum();
    let total_users = results.len();
    let mut filtered = filter::filter_results(&results, &config.filters);
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
                    filtered = filter::filter_results(&fallback_results, &config.filters);
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
            db.mark_album_processed(artist, a, "skipped")?;
        }
        return Ok(());
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
    )
    .await
    {
        Ok(files) => files,
        Err(e) => {
            tracing::warn!(
                "{artist} — {}: download failed ({e}); {} candidates exhausted",
                album.unwrap_or("(all)"),
                ranked.len(),
            );
            return Err(e);
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
    // When album is None (manual mode without --album), treat organize
    // failure the same way so the CLI exits with an error.
    if organize_ok {
        if let Some(a) = album {
            db.mark_album_processed(artist, a, "success")?;
        }
    } else if let Some(a) = album {
        db.mark_album_processed(artist, a, "failed")?;
    }
    if !organize_ok {
        return Err(SeakarrError::Download(
            "download succeeded but file organization failed".into(),
        ));
    }

    // Notify
    let track_count = downloaded.len();
    notifier::notify_success(
        &config.notifications.urls,
        artist,
        album.unwrap_or("Unknown"),
        track_count,
    )
    .await?;

    tracing::info!(
        "Completed: {artist} — {} ({track_count} tracks)",
        album.unwrap_or("(all)")
    );
    Ok(())
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
    let targets = scanner::find_albums_to_upgrade(&albums, &config.filters);
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
        targets.len(),
        albums.len()
    );

    if targets.is_empty() {
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

    let semaphore = Arc::new(Semaphore::new(config.download.concurrent.max(1)));

    let futures = targets.into_iter().map(|(artist, album)| {
        let semaphore = Arc::clone(&semaphore);
        async move {
            // Park until a permit is free — this is what bounds concurrency.
            let _permit = semaphore
                .acquire_owned()
                .await
                .expect("semaphore is never closed");
            process_album(client, &artist, Some(&album), config, db, staging_dir).await
        }
        .boxed_local()
    });

    let results = futures::future::join_all(futures).await;

    for result in results {
        if let Err(e) = result {
            tracing::error!("Album processing failed: {e}");
        }
    }

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
    process_album(client, artist, album, config, db, staging_dir).await
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
        )
        .await;
        assert!(result.is_ok());
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
        )
        .await;
        assert!(result.is_ok());

        let queries = client.search_queries.lock().unwrap().clone();
        assert_eq!(queries, vec!["Test Artist Test Album".to_string()]);

        let rows = db.get_processed_albums().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, "skipped");
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

        process_album(
            client.as_ref() as &dyn crate::client::SoulseekClient,
            "Test Artist",
            Some("Test Album"),
            &config,
            &db,
            staging.path(),
        )
        .await
        .unwrap();

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

        process_album(
            client.as_ref() as &dyn crate::client::SoulseekClient,
            "Test Artist",
            Some("Test Album"),
            &config,
            &db,
            staging.path(),
        )
        .await
        .unwrap();

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
    async fn test_fallback_no_matches_marks_skipped() {
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
        )
        .await;
        assert!(result.is_ok());

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
        assert_eq!(rows[0].status, "skipped");
    }

    #[tokio::test]
    async fn test_fallback_with_gappy_tracks_marks_skipped() {
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
        )
        .await;
        assert!(result.is_ok());

        // Gappy track set rejected at the filter stage -> album skipped,
        // nothing downloaded.
        let rows = db.get_processed_albums().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, "skipped");
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
}
