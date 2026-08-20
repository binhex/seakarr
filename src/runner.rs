use std::path::{Path, PathBuf};
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

/// Extract the track number from a downloaded file's stem for the organize
/// pattern's `%track%` placeholder. Falls back to `"01"` when the filename
/// carries no parseable track number (matching the historical hardcoded
/// value). Returns the raw number (e.g. `"02 - Track"` -> `"2"`).
fn track_number_for_organize(stem: &str) -> String {
    crate::tracks::track_number_from_filename(stem)
        .map(|n| n.to_string())
        .unwrap_or_else(|| "01".to_string())
}

/// Process a single album: search → filter rank → download → organize → notify.
/// When `target_library_path` is provided and `library_upgrade.enabled` is on
/// (auto mode only), a completed download is copied into the origin library
/// directory instead of the generic organize step, and the album completes
/// early (the organize block below is bypassed).
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
    target_library_path: Option<&Path>,
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
        artist.replace(['/', '\\'], "-"),
        album.unwrap_or("unknown").replace(['/', '\\'], "-")
    );
    let album_staging = staging_dir.join(&album_slug);
    // Note: album_staging directory is created by download_album, only when
    // a valid peer with downloadable files is found. This prevents empty
    // staging directories from accumulating for albums with no results.

    // Search for artist + album.
    let search_start = std::time::Instant::now();
    let outcome =
        search::search_album_with_fallback(client, artist, album, config.search.timeout_secs)
            .await?;
    let duration_ms = search_start.elapsed().as_millis() as u64;
    let history_album = album.map(str::trim);
    if let Err(e) = search::record_search(
        artist,
        history_album,
        outcome.results.len(),
        duration_ms,
        db,
    ) {
        tracing::warn!(
            "{artist} — {}: failed to record search history: {e}",
            album.unwrap_or("(all)")
        );
    }
    let results = outcome.results;

    // Filter + rank
    let mut total_results: usize = results.iter().map(|r| r.files.len()).sum();
    let mut total_users = results.len();
    let mut filtered =
        filter::filter_results(&results, &config.filters, library_track_count, album);
    // Track which results were last filtered (for rejection summary)
    let mut last_filtered_results: Vec<crate::client::SearchResult> = results.clone();
    // Title-search fallback: when the primary search returned no usable results
    // by the cleaned title of the album's alphabetically-first library track
    // and keep only results containing the album's library track titles.
    // Only fires when the local library holds the album (enabling the title
    // list), the title search is enabled, and an album is being processed.
    // Track whether the title-search tier actually fired
    let mut title_search_attempted = false;

    if filtered.is_empty()
        && config.search.search_title_match > 0
        && !config.library.paths.is_empty()
    {
        // Manual mode without --album has no album name to match — the tier
        // cannot fire and the failure falls through to the checks below.
        if let Some(album_name) = album {
            match search::get_library_track_filenames(&config.library.paths, artist, album_name) {
                Ok(lib_filenames) if !lib_filenames.is_empty() => {
                    title_search_attempted = true;
                    let title_start = std::time::Instant::now();
                    match search::search_by_title(
                        client,
                        &lib_filenames,
                        artist,
                        config.search.timeout_secs,
                        config.search.search_title_match,
                    )
                    .await
                    {
                        Ok(title_results) => {
                            tracing::info!(
                                "{artist} — {album_name}: title-search fallback found {} result(s)",
                                title_results.len(),
                            );
                            if !title_results.is_empty() {
                                total_results = title_results.iter().map(|r| r.files.len()).sum();
                                total_users = title_results.len();
                                filtered = filter::filter_results(
                                    &title_results,
                                    &config.filters,
                                    library_track_count,
                                    // The track-name fallback tier is never
                                    // album-gated: we could not find the
                                    // album by name, so rejecting on album
                                    // would leave us with nothing.
                                    None,
                                );
                                last_filtered_results = title_results.clone();
                            }
                            let title_duration_ms = title_start.elapsed().as_millis() as u64;
                            if let Err(e) = search::record_search(
                                artist,
                                Some(album_name),
                                title_results.len(),
                                title_duration_ms,
                                db,
                            ) {
                                tracing::warn!(
                                    "{artist} — {album_name}: failed to record title-search history: {e}"
                                );
                            }
                        }
                        Err(e) => {
                            tracing::warn!(
                                "{artist} — {album_name}: title-search fallback failed: {e}"
                            );
                        }
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(
                        "{artist} — {album_name}: failed to read library track filenames: {e}"
                    );
                }
            }
        }
    }
    if filtered.is_empty() {
        if results.is_empty() {
            // Every search tier came up empty.
            let tried_suffix = if title_search_attempted {
                " (tried: primary, title-search)"
            } else {
                ""
            };
            tracing::info!(
                "No results for {artist} — {}{tried_suffix}",
                album.unwrap_or("(all)")
            );
            // If the title tier ran and found results that were rejected
            // by filters, print a rejection summary so the user knows WHY.
            if title_search_attempted && !last_filtered_results.is_empty() {
                let rejection_summary = filter::summarize_rejections(
                    &last_filtered_results,
                    &config.filters,
                    library_track_count,
                    // Title-search results are never album-gated.
                    None,
                );
                if rejection_summary.has_rejections() {
                    tracing::info!(
                        "  → {} (title-search results)",
                        rejection_summary.summary_line(),
                    );
                }
            }
            if let Some(a) = album {
                db.mark_album_processed(artist, a, "failed")?;
            }
            return Ok(AlbumOutcome::Failed {
                reason: "no results found".into(),
            });
        }
        let contiguity_note = if config.filters.contiguous_tracks {
            ", contiguous track numbers"
        } else {
            ""
        };
        let rejection_summary = filter::summarize_rejections(
            &last_filtered_results,
            &config.filters,
            library_track_count,
            // When the title-search fallback fired, no album gate applies.
            if title_search_attempted { None } else { album },
        );
        tracing::info!(
            "{artist} — {}: {total_results} files from {total_users} users, 0 passed filters (need: {:?} format, free slot{contiguity_note})\n  → {}",
            album.unwrap_or("(all)"),
            config.filters.allowed_extensions,
            rejection_summary.summary_line(),
        );
        if let Some(a) = album {
            db.mark_album_processed(artist, a, "failed")?;
        }
        return Ok(AlbumOutcome::Failed {
            reason: "no results passed filters".into(),
        });
    }
    // Rank bonus applies only to primary-tier results: when the title-search
    // fallback fired, the album name is not a meaningful discriminator (we
    // searched by track title because the album name search failed).
    let rank_album = if title_search_attempted { None } else { album };
    let ranked = filter::rank_candidates(&filtered, &config.filters, rank_album);
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

    // Library upgrade (auto mode only, when enabled)
    if config.library_upgrade.enabled {
        if let Some(target_path) = target_library_path {
            // Completeness gate: the library album's own track count is the
            // reference — NOT the best peer's folder size. Peers share
            // different editions (box sets, anniversary editions) whose folder
            // can contain far more files than the album being upgraded has
            // (e.g. a 121-file peer folder for a 19-track library album).
            let expected_count = library_track_count
                .unwrap_or_else(|| ranked.first().map(|r| r.files.len()).unwrap_or(0));
            if downloaded.len() < expected_count {
                tracing::warn!(
                    "{artist} - {}: download incomplete ({}/{} tracks), skipping library upgrade",
                    album.unwrap_or("?"),
                    downloaded.len(),
                    expected_count,
                );
                return Ok(AlbumOutcome::Failed {
                    reason: "incomplete download, library upgrade skipped".into(),
                });
            }
            match organizer::copy_to_library(
                &downloaded,
                target_path,
                &config.storage.organize_pattern,
                artist,
                album.unwrap_or("Unknown"),
            ) {
                Ok(dests) => {
                    if config.library_upgrade.delete_lesser_quality {
                        match organizer::delete_lesser_quality_files(
                            target_path,
                            artist,
                            album.unwrap_or("Unknown"),
                            &dests,
                        ) {
                            Ok(count) if count > 0 => {
                                tracing::info!(
                                    "{artist} - {}: deleted {count} lesser-quality file(s)",
                                    album.unwrap_or("?")
                                );
                            }
                            Ok(_) => {}
                            Err(e) => {
                                tracing::error!(
                                    "{artist} - {}: failed to delete lesser-quality files: {e}",
                                    album.unwrap_or("?")
                                );
                            }
                        }
                    }
                    if let Err(e) = std::fs::remove_dir_all(&album_staging) {
                        tracing::warn!("Failed to remove staging dir {album_staging:?}: {e}");
                    }
                    if let Some(a) = album {
                        db.mark_album_processed(artist, a, "success")?;
                    }
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
                            "{artist} - {}: notification failed: {e}",
                            album.unwrap_or("(all)")
                        );
                    }
                    tracing::info!(
                        "Completed: {artist} - {} ({track_count} tracks)",
                        album.unwrap_or("(all)")
                    );
                    return Ok(AlbumOutcome::Downloaded { track_count });
                }
                Err(e) => {
                    tracing::error!(
                        "{artist} - {}: library upgrade failed: {e}",
                        album.unwrap_or("?")
                    );
                    return Ok(AlbumOutcome::Failed {
                        reason: format!("library upgrade failed: {e}"),
                    });
                }
            }
        }
    }

    // Organize (if enabled)
    let mut organize_ok = true;
    if config.storage.organize && !config.library.paths.is_empty() {
        let lib_root = Path::new(&config.library.paths[0]);
        for path in &downloaded {
            // Extract metadata from filename for pattern
            let stem = path.file_stem().unwrap_or_default().to_string_lossy();
            let ext = path.extension().unwrap_or_default().to_string_lossy();
            // Real track number from the filename (e.g. "02 - Track" -> 2),
            // falling back to "01" for unnumbered files — previously every
            // file was organized onto track "01".
            let track = track_number_for_organize(&stem);
            match organizer::organize_file(organizer::OrganizeInput {
                src: path,
                library_root: lib_root,
                pattern: &config.storage.organize_pattern,
                artist,
                album: album.unwrap_or("Unknown"),
                track: &track,
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
        tracing::debug!(
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

    // Recover any interrupted library upgrades from previous runs
    if let Err(e) = organizer::recover_interrupted_upgrades(config, db, staging_dir) {
        tracing::warn!("Library upgrade recovery scan failed: {e}");
    }

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

    let targets_vec: Vec<(String, String, usize, PathBuf)> = targets_with_counts;
    let mut futures_vec = Vec::new();

    for (artist, album, track_count, library_path) in &targets_vec {
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
                    Some(library_path),
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
    // Derive library track count from the configured library paths
    // when available, so the peer_track_count filter can reject peers
    // with fewer tracks than the library even in manual mode.
    let derived_library_count = album.and_then(|a| {
        if config.library.paths.is_empty() {
            return None;
        }
        search::get_library_track_filenames(&config.library.paths, artist, a)
            .ok()
            .filter(|tracks| !tracks.is_empty())
            .map(|tracks| tracks.len())
    });
    let result = process_album(
        client,
        artist,
        album,
        config,
        db,
        staging_dir,
        progress_ref,
        Some(&cancel),
        derived_library_count,
        None, // target_library_path (manual mode: no library upgrade)
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

    // Regression guard: the organize step must use the REAL track number
    // from each downloaded filename ("02 - Track.flac" -> 2), not the
    // hardcoded "01" that made every organized file land on track 1.
    #[test]
    fn organize_uses_real_track_number_from_filename() {
        assert_eq!(track_number_for_organize("02 - Track One"), "2");
        assert_eq!(track_number_for_organize("13 - Tender"), "13");
        assert_eq!(track_number_for_organize("01 - Intro"), "1");
        // No parseable number -> previous fallback behaviour ("01").
        assert_eq!(track_number_for_organize("Cover Art"), "01");
        // 4+ digit tokens (years) are ignored -> fallback "01".
        assert_eq!(track_number_for_organize("2001 - A Space Odyssey"), "01");
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
            files: vec![make_file(
                r"Test Artist\Test Album\01 - track.flac",
                900,
                10_000_000,
            )],
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
            None,
        )
        .await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), AlbumOutcome::Downloaded { track_count: 1 });
    }

    #[tokio::test]
    async fn test_primary_search_issues_single_query() {
        let client = Arc::new(MockClient::new());
        // The primary search for "Test Artist Test Album" returns 0 results.
        // No fallback should be triggered — exactly one query must be issued.
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

    // When the primary search returns results but all are rejected by filters
    // (e.g. contiguity gate), the album must be marked as failed with
    // "no results passed filters" — no fallback fires.
    #[tokio::test]
    async fn test_results_rejected_by_filters_marks_failed() {
        let client = Arc::new(MockClient::new());
        // Primary search returns gappy tracks 01, 03 — rejected by
        // the contiguity gate.
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
            None,
        )
        .await;
        assert!(result.is_ok());
        match result.unwrap() {
            AlbumOutcome::Failed { reason } => {
                assert!(
                    reason.contains("no results passed filters"),
                    "Expected 'no results passed filters', got: {reason}"
                );
            }
            other => panic!("Expected AlbumOutcome::Failed, got: {other:?}"),
        }

        // Only one search issued — no fallback.
        let queries = client.search_queries.lock().unwrap().clone();
        assert_eq!(queries, vec!["Test Artist Test Album".to_string()]);

        let rows = db.get_processed_albums().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, "failed");
    }

    #[tokio::test]
    async fn test_primary_download_completes_album_and_records_history() {
        let client = Arc::new(MockClient::new());
        client.search_results_by_query.lock().unwrap().insert(
            "Test Artist Test Album".into(),
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
                        r"Music\Test Artist\Test Album\02 - track.flac",
                        900,
                        10_000_000,
                    ),
                    // A mixed-share decoy: passes quality filters but the
                    // artist is not in the path. download_album must
                    // reject it.
                    make_file(
                        r"Music\Someone Else\Test Album\02 - decoy.flac",
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
            None,
        )
        .await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), AlbumOutcome::Downloaded { track_count: 2 });

        // Only the primary search fired.
        let queries = client.search_queries.lock().unwrap().clone();
        assert_eq!(queries, vec!["Test Artist Test Album".to_string()]);

        // Per-file filtering held at the download boundary: exactly the
        // artist-matching file was queued for download, never the decoy.
        let downloads = client.download_filenames.lock().unwrap().clone();
        assert_eq!(
            downloads.len(),
            2,
            "both artist-matching files must be downloaded"
        );

        // Album completed successfully.
        let rows = db.get_processed_albums().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, "success");

        // One search recorded.
        let history_count: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM search_history", [], |r| r.get(0))
            .unwrap();
        assert_eq!(history_count, 1);
    }

    // Third-tier title-search fallback: when the primary "Artist Album" and
    // primary search returns nothing, seakarr searches Soulseek by
    // the cleaned title of the library's alphabetically-first track and keeps
    // results whose files match the album's local track titles.
    #[tokio::test]
    async fn test_title_search_fallback_when_primary_empty() {
        let client = Arc::new(MockClient::new());
        // Library track "01 - I Miss You.mp3" cleans to "i miss you", the
        // alphabetically-first title and therefore the search query. The mock
        // has results ONLY for this query — the primary "Adele 25" and
        // "25" queries fall through to the (empty) static results.
        client.search_results_by_query.lock().unwrap().insert(
            "i miss you".into(),
            vec![SearchResult {
                username: "user1".into(),
                speed: 500,
                slots: 1,
                files: vec![
                    make_file(r"Music\user1\25\01 - I Miss You.flac", 900, 10_000_000),
                    make_file(r"Music\user1\25\02 - Hello.flac", 900, 10_000_000),
                ],
            }],
        );

        let mut config = make_test_config();
        config.search.search_title_match = 70;
        // Fake library: <tmp>/Adele/25/{01 - I Miss You.mp3, 02 - Hello.mp3}
        let tmp = TempDir::new().unwrap();
        let album_dir = tmp.path().join("Adele").join("25");
        std::fs::create_dir_all(&album_dir).unwrap();
        std::fs::write(album_dir.join("01 - I Miss You.mp3"), b"fake mp3").unwrap();
        std::fs::write(album_dir.join("02 - Hello.mp3"), b"fake mp3").unwrap();
        config.library.paths = vec![tmp.path().to_string_lossy().into()];

        let db = Database::open_in_memory().unwrap();
        let staging = TempDir::new().unwrap();

        let result = process_album(
            client.as_ref() as &dyn crate::client::SoulseekClient,
            "Adele",
            Some("25"),
            &config,
            &db,
            staging.path(),
            None,
            None,
            None,
            None,
        )
        .await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), AlbumOutcome::Downloaded { track_count: 2 });

        // Two tiers ran in order: primary, title search.
        let queries = client.search_queries.lock().unwrap().clone();
        assert_eq!(
            queries,
            vec!["Adele 25".to_string(), "i miss you".to_string()]
        );

        // Both title-matching library tracks were downloaded.
        let downloads = client.download_filenames.lock().unwrap().clone();
        assert_eq!(downloads.len(), 2);

        // Title search recorded in history alongside primary row.
        let history_count: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM search_history", [], |r| r.get(0))
            .unwrap();
        assert_eq!(history_count, 2);
        let title_count: i64 = db
            .conn
            .query_row(
                "SELECT result_count FROM search_history WHERE album = '25' ORDER BY id DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(title_count, 1);

        let rows = db.get_processed_albums().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, "success");
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
    async fn test_library_upgrade_completeness_uses_library_track_count_not_peer_folder_size() {
        // Regression: a peer folder containing a different (larger) edition of
        // the album — e.g. the real-world case where Abba Gold's best peer
        // folder had 121 files but the library album only has 19 tracks.
        // The completeness gate must compare against the LIBRARY track count,
        // not the best peer's folder file count.
        let client = Arc::new(MockClient::new());
        client.search_results_by_query.lock().unwrap().insert(
            "Test Artist Test Album".into(),
            vec![SearchResult {
                username: "user1".into(),
                speed: 500,
                slots: 1,
                // Peer folder has 5 files: 2 that match the album + 3 decoys
                // (different artist/other releases in the same share).
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
                    make_file(
                        r"Music\Other Artist\Other Album\01 - decoy.flac",
                        900,
                        10_000_000,
                    ),
                    make_file(
                        r"Music\Test Artist\Another Album\01 - decoy.flac",
                        900,
                        10_000_000,
                    ),
                ],
            }],
        );

        let mut config = make_test_config();
        config.library_upgrade.enabled = true;
        config.library_upgrade.delete_lesser_quality = false;
        let db = Database::open_in_memory().unwrap();
        let staging = TempDir::new().unwrap();
        let target = TempDir::new().unwrap();
        // Pre-seed the album staging dir (staging/<artist>--<album>) with the
        // 2 files the mock download will "write" (the mock returns
        // dir/<basename> paths without creating file content, so
        // copy_to_library needs them to already exist).
        let album_staging = staging.path().join("Test Artist--Test Album");
        std::fs::create_dir_all(&album_staging).unwrap();
        std::fs::write(album_staging.join("01 - track.flac"), b"fake flac").unwrap();
        std::fs::write(album_staging.join("02 - track.flac"), b"fake flac").unwrap();

        let result = process_album(
            client.as_ref() as &dyn crate::client::SoulseekClient,
            "Test Artist",
            Some("Test Album"),
            &config,
            &db,
            staging.path(),
            None,
            None,
            Some(2),             // library_track_count: library album has 2 tracks
            Some(target.path()), // target_library_path
        )
        .await;
        assert!(result.is_ok());
        assert_eq!(
            result.unwrap(),
            AlbumOutcome::Downloaded { track_count: 2 },
            "album must complete: 2 matching tracks downloaded, peer folder size (5) is irrelevant"
        );
    }

    #[tokio::test]
    async fn test_run_auto_mode_processes_album_and_marks_success() {
        let client = Arc::new(MockClient::new());
        *client.search_results.lock().unwrap() = vec![SearchResult {
            username: "user1".into(),
            speed: 500,
            slots: 1,
            files: vec![make_file(
                r"Test Artist\Test Album\01 - track.flac",
                900,
                10_000_000,
            )],
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

    // Regression guard: a library album nested inside a genre subdirectory
    // (e.g. <root>/Pop/Alesha Dixon/The Alesha Show/) must be upgraded IN
    // PLACE — the copied files land at the album's real location below the
    // library root, not at the root of the library path. Real-world case:
    // the Alesha Dixon album was copied to .../Albums/Pop/Alesha Dixon/...
    // instead of .../Albums/Pop/Pop/Alesha Dixon/...
    #[tokio::test]
    async fn test_auto_mode_upgrade_preserves_nested_library_location() {
        let client = Arc::new(MockClient::new());
        *client.search_results.lock().unwrap() = vec![SearchResult {
            username: "user1".into(),
            speed: 500,
            slots: 1,
            files: vec![make_file(
                r"Music\Pop\Alesha Dixon\01 - track.flac",
                900,
                10_000_000,
            )],
        }];

        let mut config = make_test_config();
        config.library_upgrade.enabled = true;
        config.library_upgrade.delete_lesser_quality = false;
        let tmp = TempDir::new().unwrap();
        // Library layout: <tmp>/Pop/Alesha Dixon/The Alesha Show/01 - track.mp3
        // (mp3 is not in the allowed [flac] list, so the album is flagged for
        // upgrade). The album lives inside the "Pop" genre subdirectory.
        let album_dir = tmp
            .path()
            .join("Pop")
            .join("Alesha Dixon")
            .join("The Alesha Show");
        std::fs::create_dir_all(&album_dir).unwrap();
        std::fs::write(album_dir.join("01 - track.mp3"), b"fake mp3 data").unwrap();
        config.library.paths = vec![tmp.path().to_string_lossy().into()];

        // Pre-seed the per-album staging dir: the mock client reports the
        // download as complete without creating file content, so the staging
        // files must already exist for copy_to_library to succeed.
        let staging = TempDir::new().unwrap();
        config.storage.staging_dir = staging.path().to_string_lossy().into();
        let album_staging = staging.path().join("Pop--Alesha Dixon");
        std::fs::create_dir_all(&album_staging).unwrap();
        std::fs::write(album_staging.join("01 - track.flac"), b"fake flac").unwrap();

        let db = Database::open_in_memory().unwrap();
        let result = run_auto_mode(
            client.as_ref() as &dyn crate::client::SoulseekClient,
            &config,
            &db,
        )
        .await;
        assert!(result.is_ok());

        // The upgraded FLAC must land at the album's REAL location inside the
        // genre subdirectory — <tmp>/Pop/Pop/Alesha Dixon/01 - track.flac —
        // mirroring the user's `.../Albums/Pop/Pop/Alesha Dixon/The Alesha Show/`
        // case. It must NOT land at the library root (<tmp>/Pop/Alesha Dixon/...).
        let expected = tmp
            .path()
            .join("Pop")
            .join("Pop")
            .join("Alesha Dixon")
            .join("01 - track.flac");
        assert!(
            expected.exists(),
            "upgrade must copy into the album's real location inside the library: {expected:?}"
        );
        let wrong = tmp
            .path()
            .join("Pop")
            .join("Alesha Dixon")
            .join("01 - track.flac");
        assert!(
            !wrong.exists(),
            "upgrade must NOT copy to the root of the library path: {wrong:?}"
        );
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
            files: vec![make_file(
                r"Test Artist\Test Album\01 - track.flac",
                900,
                10_000_000,
            )],
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
