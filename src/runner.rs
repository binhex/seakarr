use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::Semaphore;

use futures::FutureExt;

use crate::client::SoulseekClient;
use crate::config::Config;
use crate::db::Database;
use crate::discography::{
    discover_artist_albums, DiscographyProvider, DiscoveryOutcome, DiscoveryProvenance,
    MusicBrainzProvider,
};
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

/// Record every track's measured speed + success/failure into the peer
/// reputation store. Failures are logged and ignored — reputation bookkeeping
/// never fails an album.
fn record_track_reputation(db: &Database, stats: &download::DownloadStats) {
    for track in &stats.tracks {
        if let Err(e) = db.update_peer_reputation(&track.username, track.speed_kbps, track.success)
        {
            tracing::warn!(
                "failed to record peer reputation for {}: {e}",
                track.username
            );
        }
    }
}

/// The peer that got furthest in the download: the last track recorded, which
/// belongs to the candidate that either completed the album or failed last.
fn furthest_peer(stats: &download::DownloadStats) -> Option<&str> {
    stats.tracks.last().map(|t| t.username.as_str())
}

/// Record a single album-level failure for `peer`, used to demote a peer that
/// served an incomplete album or failed the whole album.
fn record_album_failure(db: &Database, peer: &str) {
    if let Err(e) = db.update_peer_reputation(peer, 0.0, false) {
        tracing::warn!("failed to record peer reputation for {peer}: {e}");
    }
}

fn mark_album_processed_if_identifiable(
    db: &Database,
    artist: &str,
    album: Option<&str>,
    status: &str,
) -> Result<()> {
    if artist.trim().is_empty() {
        return Ok(());
    }
    if let Some(album) = album {
        db.mark_album_processed(artist, album, status)?;
    }
    Ok(())
}

/// Process a single album: search → filter rank → download → organize → notify.
/// When `target_library_path` is provided and `library_upgrade.enabled` is on
/// (auto mode only), a completed download is copied into the origin library
/// directory instead of the generic organize step, and the album completes
/// early (the organize block below is bypassed).
///
/// When `config.search.peer_reputation` is on, the peer reputation map is
/// loaded from the DB before ranking (measured speed + reliability factor
/// blend in [`filter::rank_candidates`]) and each track's outcome is recorded
/// back to the DB after a successful download.
#[allow(clippy::too_many_arguments)]
pub async fn process_album(
    client: &dyn SoulseekClient,
    artist: &str,
    album: Option<&str>,
    ignore_processed: bool,
    config: &Config,
    db: &Database,
    staging_dir: &Path,
    progress: Option<&ProgressDisplay>,
    cancel: Option<&Arc<AtomicBool>>,
    library_track_count: Option<usize>,
    target_library_path: Option<&Path>,
) -> Result<AlbumOutcome> {
    process_album_internal(
        client,
        artist,
        album,
        ignore_processed,
        config,
        db,
        staging_dir,
        progress,
        cancel,
        library_track_count,
        target_library_path,
        None,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn process_album_internal(
    client: &dyn SoulseekClient,
    artist: &str,
    album: Option<&str>,
    ignore_processed: bool,
    config: &Config,
    db: &Database,
    staging_dir: &Path,
    progress: Option<&ProgressDisplay>,
    cancel: Option<&Arc<AtomicBool>>,
    library_track_count: Option<usize>,
    target_library_path: Option<&Path>,
    presearched_results: Option<Vec<crate::client::SearchResult>>,
) -> Result<AlbumOutcome> {
    if artist.trim().is_empty() && config.storage.organize && !config.library.paths.is_empty() {
        return Err(SeakarrError::Config(
            "cannot organize an album-only download without an artist; provide --artist or disable storage.organize"
                .into(),
        ));
    }

    // Skip if already processed — unless the user explicitly requested a
    // reprocess via --ignore-processed, in which case the matching success
    // record is deleted so this run can replace it (search history and other
    // albums are untouched).
    if !artist.trim().is_empty() {
        if let Some(album_name) = album {
            if ignore_processed {
                if db.delete_processed_album(artist, album_name)? {
                    tracing::info!("Ignoring already-processed record: {artist} — {album_name}");
                }
            } else if db.is_album_processed(artist, album_name)? {
                tracing::info!("Skipping already-processed: {artist} — {album_name}");
                return Ok(AlbumOutcome::Skipped);
            }
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

    // Search for artist + album unless artist-only mode supplied results that
    // were already discovered and grouped in one artist query.
    let presearched = presearched_results.is_some();
    let mut results = match presearched_results {
        Some(results) => results,
        None => {
            let search_start = std::time::Instant::now();
            let outcome = match search::search_album_with_fallback_with_queue_limit(
                client,
                artist,
                album,
                config.search.timeout_secs,
                &config.filters,
                library_track_count,
                config.download.max_queue_length,
            )
            .await
            {
                Ok(outcome) => outcome,
                Err(error) => {
                    // Preserve a failed status after --ignore-processed removes a
                    // prior success record, so a hard search error cannot erase all
                    // processing state for the album.
                    mark_album_processed_if_identifiable(db, artist, album, "failed")?;
                    return Err(error);
                }
            };
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
            outcome.results
        }
    };
    if album.is_some() && !artist.trim().is_empty() {
        search::retain_artist_files(&mut results, artist);
    }

    // Filter + rank
    let mut total_results: usize = results.iter().map(|r| r.files.len()).sum();
    let mut total_users = results.len();
    let mut filtered = filter::filter_results_with_queue_limit(
        &results,
        &config.filters,
        library_track_count,
        album,
        config.download.max_queue_length,
    );
    // Track which results were last filtered (for rejection summary)
    let mut last_filtered_results: Vec<crate::client::SearchResult> = results.clone();
    // Title-search fallback: when the primary search returned no usable results
    // by the cleaned title of the album's alphabetically-first library track
    // and keep only results containing the album's library track titles.
    // Only fires when the local library holds the album (enabling the title
    // list), the title search is enabled, an album is being processed, and the
    // manual target includes an artist for the library-path lookup.
    // Track whether the title-search tier actually fired
    let mut title_search_attempted = false;

    if filtered.is_empty()
        && !presearched
        && config.search.search_title_match > 0
        && !config.library.paths.is_empty()
        && !artist.trim().is_empty()
    {
        // Manual mode without --album has no album name to match — the tier
        // cannot fire and the failure falls through to the checks below.
        if let Some(album_name) = album {
            match search::get_library_track_filenames(&config.library.paths, artist, album_name) {
                Ok(lib_filenames) if !lib_filenames.is_empty() => {
                    let title_start = std::time::Instant::now();
                    // Drop meaningless track names ("CD Track N", "Track N",
                    // "01", ...) so a library of generic names doesn't build a
                    // garbage query that matches unrelated albums.
                    let non_generic: Vec<String> = lib_filenames
                        .iter()
                        .filter(|f| !search::is_generic_track_name(f))
                        .cloned()
                        .collect();
                    if non_generic.is_empty() {
                        tracing::info!(
                            "{artist} — {album_name}: all track names are generic, skipping title-search fallback"
                        );
                    } else {
                        title_search_attempted = true;
                        // the Soulseek lib otherwise gives no clue that this is a
                        // track-title fallback (and for which album). Log it up
                        // front so the user can tie the query to the album.
                        tracing::info!(
                            "{artist} — {album_name}: no usable primary results, falling back to track-title search"
                        );
                        match search::search_by_title(
                            client,
                            &non_generic,
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
                                    total_results =
                                        title_results.iter().map(|r| r.files.len()).sum();
                                    total_users = title_results.len();
                                    filtered = filter::filter_results_with_queue_limit(
                                        &title_results,
                                        &config.filters,
                                        library_track_count,
                                        // The track-name fallback tier is never
                                        // album-gated: we could not find the
                                        // album by name, so rejecting on album
                                        // would leave us with nothing.
                                        None,
                                        config.download.max_queue_length,
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
                let rejection_summary = filter::summarize_rejections_with_queue_limit(
                    &last_filtered_results,
                    &config.filters,
                    library_track_count,
                    // Title-search results are never album-gated.
                    None,
                    config.download.max_queue_length,
                );
                if rejection_summary.has_rejections() {
                    tracing::info!(
                        "  → {} (title-search results)",
                        rejection_summary.summary_line(),
                    );
                }
            }
            mark_album_processed_if_identifiable(db, artist, album, "failed")?;
            return Ok(AlbumOutcome::Failed {
                reason: "no results found".into(),
            });
        }
        let contiguity_note = if config.filters.contiguous_tracks {
            ", contiguous track numbers"
        } else {
            ""
        };
        let rejection_summary = filter::summarize_rejections_with_queue_limit(
            &last_filtered_results,
            &config.filters,
            library_track_count,
            // When the title-search fallback fired, no album gate applies.
            if title_search_attempted { None } else { album },
            config.download.max_queue_length,
        );
        let availability_requirement = if config.download.max_queue_length == 0 {
            "free slot".to_string()
        } else {
            format!(
                "free slot or queue position <= {}",
                config.download.max_queue_length
            )
        };
        tracing::info!(
            "{artist} — {}: {total_results} files from {total_users} users, 0 passed filters (need: {:?} format, {availability_requirement}{contiguity_note})\n  → {}",
            album.unwrap_or("(all)"),
            config.filters.allowed_extensions,
            rejection_summary.summary_line(),
        );
        mark_album_processed_if_identifiable(db, artist, album, "failed")?;
        return Ok(AlbumOutcome::Failed {
            reason: "no results passed filters".into(),
        });
    }
    // Rank bonus applies only to primary-tier results: when the title-search
    // fallback fired, the album name is not a meaningful discriminator (we
    // searched by track title because the album name search failed).
    let rank_album = if title_search_attempted { None } else { album };
    // Load the peer reputation map (measured speed + reliability) before
    // ranking. On a DB error we proceed with an empty map — reputation never
    // blocks a search.
    let reputation = if config.search.peer_reputation {
        db.get_reputation_map().unwrap_or_default()
    } else {
        std::collections::HashMap::new()
    };
    let ranked = filter::rank_candidates(&filtered, &config.filters, rank_album, &reputation);
    tracing::info!(
        "{artist} — {}: {total_results} files from {total_users} users, {} users passed filters, best: {} (speed={})",
        album.unwrap_or("(all)"),
        filtered.len(),
        ranked.first().map(|r| r.username.as_str()).unwrap_or("?"),
        ranked.first().map(|r| r.speed).unwrap_or(0),
    );

    // Download
    let mut stats = download::DownloadStats::default();
    let downloaded = match download::download_album(
        client,
        &ranked,
        &album_staging,
        &config.download,
        &config.filters,
        progress,
        cancel,
        &mut stats,
    )
    .await
    {
        Ok(files) => {
            // Record per-track outcomes as soon as they are known, so the
            // measured speed + reliability land regardless of what happens
            // downstream (completeness gate, library upgrade, organize).
            if config.search.peer_reputation {
                record_track_reputation(db, &stats);
            }
            files
        }
        Err(e) => {
            let is_cancelled = cancel.is_some_and(|c| c.load(std::sync::atomic::Ordering::SeqCst));
            // A user abort (Ctrl+C) is not the peer's fault and records nothing.
            // Otherwise record per-track outcomes plus one album-level failure
            // for the peer that got furthest, so a consistently-failing peer
            // accumulates negative reputation and sinks in the ranking.
            if config.search.peer_reputation && !is_cancelled {
                record_track_reputation(db, &stats);
                if let Some(peer) = furthest_peer(&stats).map(str::to_string) {
                    record_album_failure(db, &peer);
                }
            }
            let reason = if is_cancelled {
                e.to_string()
            } else {
                format!("all candidates exhausted: {e}")
            };
            tracing::warn!(
                "{artist} — {}: download failed ({reason}); {} candidates exhausted",
                album.unwrap_or("(all)"),
                ranked.len(),
            );
            mark_album_processed_if_identifiable(db, artist, album, "failed")?;
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
            //
            // A library upgrade target is only ever supplied by auto mode,
            // which always passes the scanner's replacement count, so the
            // count is always Some here. The old fallback to the peer's
            // folder size was dead code and would have been *wrong* if it had
            // fired (a peer folder spans several album directories while
            // download_album downloads only the largest group). A caller that
            // somehow passes a target without a count is a programming error
            // — surface it as a failed album rather than panicking.
            let Some(expected_count) = library_track_count else {
                return Ok(AlbumOutcome::Failed {
                    reason: "library upgrade target set without a library track count".into(),
                });
            };
            if downloaded.len() < expected_count {
                // The serving peer delivered an incomplete album — record an
                // album-level failure for it (per-track outcomes were already
                // recorded) so it sinks instead of being re-picked next cycle.
                if config.search.peer_reputation {
                    if let Some(peer) = furthest_peer(&stats).map(str::to_string) {
                        record_album_failure(db, &peer);
                    }
                }
                tracing::warn!(
                    "{artist} - {}: download incomplete ({}/{} tracks), skipping library upgrade",
                    album.unwrap_or("?"),
                    downloaded.len(),
                    expected_count,
                );
                mark_album_processed_if_identifiable(db, artist, album, "failed")?;
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
                    mark_album_processed_if_identifiable(db, artist, album, "success")?;
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
                    mark_album_processed_if_identifiable(db, artist, album, "failed")?;
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
            // Metadata is derived exactly as in the auto-upgrade copy path
            // (organizer::organize_name_from_stem): the leading track token
            // is stripped from the title and the track number is zero-padded,
            // so a staged "02 - Track Two.flac" organizes to
            // "02 - Track Two.flac" — never the duplicated, unpadded
            // "2 - 02 - Track Two.flac".
            let stem = path.file_stem().unwrap_or_default().to_string_lossy();
            let ext = path.extension().unwrap_or_default().to_string_lossy();
            let (track, title) = organizer::organize_name_from_stem(&stem);
            match organizer::organize_file(organizer::OrganizeInput {
                src: path,
                library_root: lib_root,
                pattern: &config.storage.organize_pattern,
                artist,
                album: album.unwrap_or("Unknown"),
                track: &track,
                title: &title,
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

    // Mark processed — only success if organize also succeeded. Albums without
    // an artist are not recorded because ("", album) is not an unambiguous key.
    if organize_ok {
        mark_album_processed_if_identifiable(db, artist, album, "success")?;
        // Remove the staging directory — files have been organized into the
        // library. Absence of the staging dir signals a completed download.
        if config.storage.organize && !config.library.paths.is_empty() {
            if let Err(e) = std::fs::remove_dir_all(&album_staging) {
                tracing::warn!("Failed to remove staging dir {album_staging:?}: {e}");
            }
        }
    } else {
        mark_album_processed_if_identifiable(db, artist, album, "failed")?;
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
    ignore_processed: bool,
) -> Result<()> {
    if config.library.paths.is_empty() {
        return Err(SeakarrError::Config(
            "library.paths is empty — nothing to scan".into(),
        ));
    }

    // Scan library
    tracing::info!("Scanning library...");
    let albums = scanner::scan_library(&config.library.paths, &config.filters)?;
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
                    ignore_processed,
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

/// Resolve an artist/album pair to the casing already stored in history.
fn canonical_processed_target(
    db: &Database,
    artist: &str,
    album: &str,
) -> Result<(String, String)> {
    let Some(record) = db.get_processed_albums()?.into_iter().find(|record| {
        record.artist.trim().eq_ignore_ascii_case(artist.trim())
            && record.album.eq_ignore_ascii_case(album)
    }) else {
        return Ok((artist.to_owned(), album.to_owned()));
    };
    Ok((record.artist, record.album))
}

/// Outcome of an artist-only manual run: per-album outcomes plus an optional
/// run-level notice (e.g. a visible automatic legacy fallback).
struct ArtistOnlyRun {
    outcomes: Vec<(String, AlbumOutcome)>,
    notice: Option<String>,
}

/// Process a fixed list of album targets. Each work item carries the album
/// title and optional pre-searched Soulseek results: legacy grouping supplies
/// `Some(results)` (no new query, no search-history row) while authoritative
/// targets supply `None`, causing `process_album_internal` to run the normal
/// targeted search and search-history recording.
#[allow(clippy::too_many_arguments)]
async fn process_artist_album_work(
    client: &dyn SoulseekClient,
    artist: &str,
    ignore_processed: bool,
    config: &Config,
    db: &Database,
    staging_dir: &Path,
    progress: Option<&ProgressDisplay>,
    cancel: &Arc<AtomicBool>,
    work: Vec<(String, Option<Vec<crate::client::SearchResult>>)>,
) -> Result<Vec<(String, AlbumOutcome)>> {
    let mut outcomes = Vec::with_capacity(work.len());
    for (album_title, presearched) in work {
        if cancel.load(Ordering::SeqCst) {
            outcomes.push((
                "(all)".to_string(),
                AlbumOutcome::Failed {
                    reason: "download cancelled by user".into(),
                },
            ));
            break;
        }
        let (process_artist, process_album) = canonical_processed_target(db, artist, &album_title)?;
        let library_track_count = if config.library.paths.is_empty() {
            None
        } else {
            search::get_library_track_filenames(
                &config.library.paths,
                &process_artist,
                &process_album,
            )
            .ok()
            .filter(|tracks| !tracks.is_empty())
            .map(|tracks| tracks.len())
        };
        let result = process_album_internal(
            client,
            &process_artist,
            Some(&process_album),
            ignore_processed,
            config,
            db,
            staging_dir,
            progress,
            Some(cancel),
            library_track_count,
            None,
            presearched,
        )
        .await?;
        let cancelled = matches!(
            &result,
            AlbumOutcome::Failed { reason } if reason.contains("cancelled by user")
        );
        outcomes.push((process_album, result));
        if cancelled {
            break;
        }
    }
    Ok(outcomes)
}

/// Process every logical album discovered by one artist-only Soulseek search.
/// This is the untouched legacy folder heuristic: broad artist search, a
/// single search-history row, folder grouping, presearched results handed to
/// each album, and the same cancellation behaviour as before. It remains the
/// explicit-disable path and the visible automatic fallback.
#[allow(clippy::too_many_arguments)]
async fn run_legacy_artist_only_mode(
    client: &dyn SoulseekClient,
    artist: &str,
    ignore_processed: bool,
    config: &Config,
    db: &Database,
    staging_dir: &Path,
    progress: Option<&ProgressDisplay>,
    cancel: &Arc<AtomicBool>,
) -> Result<ArtistOnlyRun> {
    let search_start = std::time::Instant::now();
    let outcome = search::search_album_with_fallback_with_queue_limit(
        client,
        artist,
        None,
        config.search.timeout_secs,
        &config.filters,
        None,
        config.download.max_queue_length,
    )
    .await?;
    let duration_ms = search_start.elapsed().as_millis() as u64;
    if let Err(e) = search::record_search(artist, None, outcome.results.len(), duration_ms, db) {
        tracing::warn!("{artist} — (all): failed to record search history: {e}");
    }

    let albums = search::group_artist_results(&outcome.results, artist);
    if albums.is_empty() {
        let reason = if cancel.load(Ordering::SeqCst) {
            "download cancelled by user"
        } else {
            "no identifiable albums found"
        };
        return Ok(ArtistOnlyRun {
            outcomes: vec![(
                "(all)".to_string(),
                AlbumOutcome::Failed {
                    reason: reason.into(),
                },
            )],
            notice: None,
        });
    }

    let work = albums
        .into_iter()
        .map(|album| (album.album, Some(album.results)))
        .collect();
    let outcomes = process_artist_album_work(
        client,
        artist,
        ignore_processed,
        config,
        db,
        staging_dir,
        progress,
        cancel,
        work,
    )
    .await?;
    Ok(ArtistOnlyRun {
        outcomes,
        notice: None,
    })
}

/// Artist-only manual mode against an injected authoritative discography
/// provider. Authoritative targets become one sequential targeted Soulseek
/// search per conceptual album; empty outcomes search nothing; stale caches
/// warn; an unavailable provider falls back to the legacy folder heuristic
/// with a visible run notice.
#[allow(clippy::too_many_arguments)]
async fn run_artist_only_mode_with_provider(
    client: &dyn SoulseekClient,
    artist: &str,
    ignore_processed: bool,
    config: &Config,
    db: &Database,
    staging_dir: &Path,
    progress: Option<&ProgressDisplay>,
    cancel: &Arc<AtomicBool>,
    provider: &dyn DiscographyProvider,
) -> Result<ArtistOnlyRun> {
    match discover_artist_albums(provider, db, artist, &config.discography).await {
        DiscoveryOutcome::Authoritative { albums, provenance } => {
            if let DiscoveryProvenance::StaleCache {
                age_days,
                refresh_error,
            } = &provenance
            {
                tracing::warn!(
                    "{artist}: discography cache is {age_days} day(s) old and refresh failed ({refresh_error}); using stale cache"
                );
            }
            let work = albums
                .into_iter()
                .map(|album| (album.title, None))
                .collect();
            let outcomes = process_artist_album_work(
                client,
                artist,
                ignore_processed,
                config,
                db,
                staging_dir,
                progress,
                cancel,
                work,
            )
            .await?;
            Ok(ArtistOnlyRun {
                outcomes,
                notice: None,
            })
        }
        DiscoveryOutcome::AuthoritativeEmpty { provenance } => {
            if let DiscoveryProvenance::StaleCache {
                age_days,
                refresh_error,
            } = &provenance
            {
                tracing::warn!(
                    "{artist}: discography cache is {age_days} day(s) old and refresh failed ({refresh_error}); no eligible authoritative albums"
                );
            }
            Ok(ArtistOnlyRun {
                outcomes: Vec::new(),
                notice: Some(format!(
                    "No eligible authoritative albums found for {artist}"
                )),
            })
        }
        DiscoveryOutcome::LegacyFallback { reason } => {
            tracing::warn!(
                "{artist}: authoritative discography unavailable ({reason}); falling back to heuristic folder-based album discovery"
            );
            let run = run_legacy_artist_only_mode(
                client,
                artist,
                ignore_processed,
                config,
                db,
                staging_dir,
                progress,
                cancel,
            )
            .await?;
            Ok(ArtistOnlyRun {
                outcomes: run.outcomes,
                notice: Some(format!(
                    "Authoritative discography unavailable: {reason}; album names were discovered heuristically from Soulseek folders"
                )),
            })
        }
    }
}

/// Complete an artist-only manual run. With `discography.enabled` (the
/// default) the authoritative MusicBrainz provider takes over; an explicit
/// disable keeps the legacy folder heuristic silently, while a provider
/// construction failure keeps it with the same visible warned fallback.
#[allow(clippy::too_many_arguments)]
async fn run_artist_only_mode(
    client: &dyn SoulseekClient,
    artist: &str,
    ignore_processed: bool,
    config: &Config,
    db: &Database,
    staging_dir: &Path,
    progress: Option<&ProgressDisplay>,
    cancel: &Arc<AtomicBool>,
) -> Result<ArtistOnlyRun> {
    if !config.discography.enabled {
        tracing::info!(
            "{artist}: authoritative discography is disabled in config — using legacy folder-based album discovery"
        );
        return run_legacy_artist_only_mode(
            client,
            artist,
            ignore_processed,
            config,
            db,
            staging_dir,
            progress,
            cancel,
        )
        .await;
    }
    let provider = match MusicBrainzProvider::new() {
        Ok(provider) => provider,
        Err(error) => {
            tracing::warn!(
                "{artist}: failed to construct the MusicBrainz provider ({error}); using legacy folder-based album discovery"
            );
            let run = run_legacy_artist_only_mode(
                client,
                artist,
                ignore_processed,
                config,
                db,
                staging_dir,
                progress,
                cancel,
            )
            .await?;
            return Ok(ArtistOnlyRun {
                outcomes: run.outcomes,
                notice: Some(format!(
                    "Authoritative discography unavailable: {error}; album names were discovered heuristically from Soulseek folders"
                )),
            });
        }
    };
    run_artist_only_mode_with_provider(
        client,
        artist,
        ignore_processed,
        config,
        db,
        staging_dir,
        progress,
        cancel,
        &provider,
    )
    .await
}

/// Run in manual mode: process a single artist and/or album search target.
pub async fn run_manual_mode(
    client: &dyn SoulseekClient,
    artist: Option<&str>,
    album: Option<&str>,
    ignore_processed: bool,
    config: &Config,
    db: &Database,
) -> Result<()> {
    let artist_name = artist.filter(|name| !name.trim().is_empty()).unwrap_or("");
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

    // Derive library track count from the configured library paths when an
    // explicit album is selected, so peer_track_count can reject peers with
    // fewer tracks than the library even in manual mode.
    let derived_library_count = album.and_then(|album_name| {
        if artist_name.is_empty() || config.library.paths.is_empty() {
            return None;
        }
        search::get_library_track_filenames(&config.library.paths, artist_name, album_name)
            .ok()
            .filter(|tracks| !tracks.is_empty())
            .map(|tracks| tracks.len())
    });
    let result = if album.is_none() && !artist_name.is_empty() {
        match run_artist_only_mode(
            client,
            artist_name,
            ignore_processed,
            config,
            db,
            staging_dir,
            progress_ref,
            &cancel,
        )
        .await
        {
            Ok(run) => {
                for (album_name, outcome) in run.outcomes {
                    report.record(artist_name, &album_name, outcome);
                }
                if let Some(notice) = run.notice {
                    report.add_notice(notice);
                }
                Ok(())
            }
            Err(e) => {
                tracing::error!("Manual mode: {artist_name} — {album_display}: {e}");
                report.record(
                    artist_name,
                    album_display,
                    AlbumOutcome::Failed {
                        reason: e.to_string(),
                    },
                );
                Err(e)
            }
        }
    } else {
        let result = process_album(
            client,
            artist_name,
            album,
            ignore_processed,
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
            Ok(outcome) => report.record(artist_name, album_display, outcome.clone()),
            Err(e) => {
                tracing::error!("Manual mode: {artist_name} — {album_display}: {e}");
                report.record(
                    artist_name,
                    album_display,
                    AlbumOutcome::Failed {
                        reason: e.to_string(),
                    },
                );
            }
        }
        result.map(|_| ())
    };

    if let Some(ref p) = progress {
        p.clear();
    }

    report.print_summary();
    _listener.abort();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{FileInfo, MockClient, SearchResult};
    use crate::config::Config;
    use crate::db::Database;
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::Mutex;
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

    // Regression guard: the organize step must derive names exactly like the
    // auto-upgrade copy path — zero-padded track number and the title
    // stripped of its leading track token (see organizer::organize_name_from_stem).
    #[test]
    fn organize_uses_shared_name_derivation() {
        // Padded to two digits, no leading-token duplication.
        assert_eq!(
            organizer::organize_name_from_stem("02 - Track One"),
            ("02".to_string(), "Track One".to_string())
        );
        assert_eq!(
            organizer::organize_name_from_stem("13 - Tender"),
            ("13".to_string(), "Tender".to_string())
        );
        assert_eq!(
            organizer::organize_name_from_stem("1 - Intro"),
            ("01".to_string(), "Intro".to_string())
        );
        // No parseable number -> previous fallback behaviour (track "01",
        // stem unchanged as the title).
        assert_eq!(
            organizer::organize_name_from_stem("Cover Art"),
            ("01".to_string(), "Cover Art".to_string())
        );
        // 4+ digit tokens (years) are ignored -> fallback track "01".
        assert_eq!(
            organizer::organize_name_from_stem("2001 - A Space Odyssey"),
            ("01".to_string(), "2001 - A Space Odyssey".to_string())
        );
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
            false,
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
    async fn ignore_processed_bypasses_success_record_and_recreates_it() {
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

        // A pre-existing successful record would normally skip this album.
        db.mark_album_processed("Test Artist", "Test Album", "success")
            .unwrap();

        // 1) Normal path: the success record must still be honoured.
        let result = process_album(
            client.as_ref() as &dyn crate::client::SoulseekClient,
            "Test Artist",
            Some("Test Album"),
            false,
            &config,
            &db,
            staging.path(),
            None,
            None,
            None,
            None,
        )
        .await;
        assert_eq!(result.unwrap(), AlbumOutcome::Skipped);
        assert!(
            db.is_album_processed("Test Artist", "Test Album").unwrap(),
            "normal path must not delete the success record"
        );

        // 2) --ignore-processed path: the record is deleted, the album is
        //    processed, and a successful retry recreates the success record.
        let result = process_album(
            client.as_ref() as &dyn crate::client::SoulseekClient,
            "Test Artist",
            Some("Test Album"),
            true,
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
        assert!(
            db.is_album_processed("Test Artist", "Test Album").unwrap(),
            "successful reprocess must recreate the success record"
        );
    }

    #[tokio::test]
    async fn ignore_processed_failed_retry_leaves_failed_status() {
        let client = Arc::new(MockClient::new());
        let config = make_test_config();
        let db = Database::open_in_memory().unwrap();
        let staging = TempDir::new().unwrap();

        db.mark_album_processed("Test Artist", "Test Album", "success")
            .unwrap();

        // Empty search results: the forced retry must run (not skip) and
        // record a failed status in place of the deleted success record.
        let result = process_album(
            client.as_ref() as &dyn crate::client::SoulseekClient,
            "Test Artist",
            Some("Test Album"),
            true,
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
        assert!(
            !db.is_album_processed("Test Artist", "Test Album").unwrap(),
            "failed retry must not leave a success record"
        );
        let status = db.get_album_status("Test Artist", "Test Album").unwrap();
        assert_eq!(status, Some("failed".to_string()));
    }

    #[tokio::test]
    async fn ignore_processed_hard_search_error_records_failure() {
        let client = Arc::new(MockClient::new());
        *client.search_should_fail.lock().unwrap() = true;
        let config = make_test_config();
        let db = Database::open_in_memory().unwrap();
        let staging = TempDir::new().unwrap();
        db.mark_album_processed("Test Artist", "Test Album", "success")
            .unwrap();

        let result = process_album(
            client.as_ref() as &dyn crate::client::SoulseekClient,
            "Test Artist",
            Some("Test Album"),
            true,
            &config,
            &db,
            staging.path(),
            None,
            None,
            None,
            None,
        )
        .await;

        assert!(result.is_err());
        assert_eq!(
            db.get_album_status("Test Artist", "Test Album").unwrap(),
            Some("failed".to_string())
        );
    }

    #[tokio::test]
    async fn ignore_processed_without_existing_record_proceeds_normally() {
        let client = Arc::new(MockClient::new());
        let config = make_test_config();
        let db = Database::open_in_memory().unwrap();
        let staging = TempDir::new().unwrap();

        let result = process_album(
            client.as_ref() as &dyn crate::client::SoulseekClient,
            "Test Artist",
            Some("Test Album"),
            true,
            &config,
            &db,
            staging.path(),
            None,
            None,
            None,
            None,
        )
        .await;

        assert!(matches!(result, Ok(AlbumOutcome::Failed { .. })));
        assert_eq!(
            db.get_album_status("Test Artist", "Test Album").unwrap(),
            Some("failed".to_string())
        );
    }

    // Album-only manual mode: an empty artist must still reach the
    // album-only search support (the query is issued album-only and the run
    // succeeds), not fail on a required artist argument.
    #[tokio::test]
    async fn test_run_manual_mode_accepts_album_only() {
        let client = MockClient::new();
        let staging = TempDir::new().unwrap();
        let mut config = make_test_config();
        config.library.paths.clear();
        config.storage.staging_dir = staging.path().to_string_lossy().into();
        let db = Database::open_in_memory().unwrap();

        run_manual_mode(&client, None, Some("Test Album"), false, &config, &db)
            .await
            .expect("album-only manual mode must run");

        let queries = client.search_queries.lock().unwrap().clone();
        assert!(
            queries.iter().any(|query| query == "Test Album"),
            "album-only mode must issue an album-only query, got {queries:?}"
        );
        assert!(
            db.get_processed_albums().unwrap().is_empty(),
            "album-only runs must not create an ambiguous processed-album key"
        );
    }

    #[tokio::test]
    async fn test_run_manual_mode_album_only_skips_title_search() {
        let client = MockClient::new();
        let staging = TempDir::new().unwrap();
        let library = TempDir::new().unwrap();
        let album_dir = library.path().join("Test Album");
        std::fs::create_dir_all(&album_dir).unwrap();
        std::fs::write(album_dir.join("01 - Distinct.flac"), b"").unwrap();

        let mut config = make_test_config();
        config.library.paths = vec![library.path().to_string_lossy().into()];
        config.storage.staging_dir = staging.path().to_string_lossy().into();
        let db = Database::open_in_memory().unwrap();

        run_manual_mode(&client, None, Some("Test Album"), false, &config, &db)
            .await
            .expect("album-only manual mode should finish without a title fallback");

        let queries = client.search_queries.lock().unwrap().clone();
        assert_eq!(queries, vec!["Test Album".to_string()]);
    }

    #[tokio::test]
    async fn test_album_only_mode_rejects_unsafe_organization() {
        let client = MockClient::new();
        let staging = TempDir::new().unwrap();
        let library = TempDir::new().unwrap();
        let mut config = make_test_config();
        config.library.paths = vec![library.path().to_string_lossy().into()];
        config.storage.organize = true;
        config.storage.staging_dir = staging.path().to_string_lossy().into();
        let db = Database::open_in_memory().unwrap();

        let error = run_manual_mode(&client, None, Some("Test Album"), false, &config, &db)
            .await
            .expect_err("album-only organization must be rejected without an artist");
        assert!(
            matches!(&error, SeakarrError::Config(message) if message.contains("cannot organize")),
            "expected an actionable organization error, got {error:?}"
        );
        assert!(
            client.search_queries.lock().unwrap().is_empty(),
            "unsafe album-only organization must be rejected before searching"
        );
    }

    #[tokio::test]
    async fn test_run_manual_mode_preserves_nonblank_artist_whitespace() {
        let client = MockClient::new();
        let staging = TempDir::new().unwrap();
        let mut config = make_test_config();
        config.library.paths.clear();
        config.storage.staging_dir = staging.path().to_string_lossy().into();
        let db = Database::open_in_memory().unwrap();

        run_manual_mode(
            &client,
            Some(" Test Artist "),
            Some("Test Album"),
            false,
            &config,
            &db,
        )
        .await
        .expect("manual mode should record a failed search without results");

        let artist: String = db
            .conn
            .query_row(
                "SELECT artist FROM search_history ORDER BY id DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(artist, " Test Artist ");
    }

    #[tokio::test]
    async fn test_peer_reputation_recorded() {
        let client = Arc::new(MockClient::new());
        let peer_a = MockClient::mock_search_result(
            "peerA",
            100_000,
            1,
            vec![(r"Test Artist\Album One\01.flac", 10_000_000, 900)],
        );
        {
            let mut by_query = client.search_results_by_query.lock().unwrap();
            by_query.insert("Test Artist Album One".to_string(), vec![peer_a]);
        }
        let config = make_test_config();
        let db = Database::open_in_memory().unwrap();
        let staging = TempDir::new().unwrap();

        // First album: downloads cleanly -> records peerA as reliable + fast.
        process_album(
            client.as_ref() as &dyn crate::client::SoulseekClient,
            "Test Artist",
            Some("Album One"),
            false,
            &config,
            &db,
            staging.path(),
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();

        let rep = db.get_reputation_map().unwrap();
        assert!(rep.contains_key("peera"));
        assert_eq!(rep["peera"].successful, 1);
    }

    #[tokio::test]
    async fn test_peer_reputation_disabled_records_nothing() {
        let client = Arc::new(MockClient::new());
        let peer_a = MockClient::mock_search_result(
            "peerA",
            100_000,
            1,
            vec![(r"Test Artist\Album One\01.flac", 10_000_000, 900)],
        );
        {
            let mut by_query = client.search_results_by_query.lock().unwrap();
            by_query.insert("Test Artist Album One".to_string(), vec![peer_a]);
        }
        let mut config = make_test_config();
        config.search.peer_reputation = false;
        let db = Database::open_in_memory().unwrap();
        let staging = TempDir::new().unwrap();

        process_album(
            client.as_ref() as &dyn crate::client::SoulseekClient,
            "Test Artist",
            Some("Album One"),
            false,
            &config,
            &db,
            staging.path(),
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();

        assert!(
            db.get_reputation_map().unwrap().is_empty(),
            "peer_reputation: false must not record anything"
        );
    }

    #[tokio::test]
    async fn test_failed_download_records_failure_reputation() {
        let client = Arc::new(MockClient::new());
        let peer_a = MockClient::mock_search_result(
            "peerA",
            100_000,
            1,
            vec![(r"Test Artist\Album One\01.flac", 10_000_000, 900)],
        );
        {
            let mut by_query = client.search_results_by_query.lock().unwrap();
            by_query.insert("Test Artist Album One".to_string(), vec![peer_a]);
        }
        *client.download_fails.lock().unwrap() = true;
        let config = make_test_config();
        let db = Database::open_in_memory().unwrap();
        let staging = TempDir::new().unwrap();

        process_album(
            client.as_ref() as &dyn crate::client::SoulseekClient,
            "Test Artist",
            Some("Album One"),
            false,
            &config,
            &db,
            staging.path(),
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();

        // A failing peer must be recorded as a failure (0 successes, >=1 total).
        let rep = db.get_reputation_map().unwrap();
        assert!(rep.contains_key("peera"));
        assert_eq!(rep["peera"].successful, 0);
        assert!(
            rep["peera"].total_downloads >= 1,
            "a failed download must record a failure"
        );
    }

    #[tokio::test]
    async fn artist_only_manual_mode_downloads_each_discovered_album() {
        let client = MockClient::new();
        *client.search_results.lock().unwrap() = vec![SearchResult {
            username: "artist-peer".into(),
            speed: 500,
            slots: 1,
            files: vec![
                make_file(r"Test Artist\Album One\01 - one.flac", 900, 10_000_000),
                make_file(r"Test Artist\Album One\02 - two.flac", 900, 10_000_000),
                make_file(r"Test Artist\Album Two\01 - one.flac", 900, 10_000_000),
                make_file(r"Test Artist\Album Two\02 - two.flac", 900, 10_000_000),
            ],
        }];

        let staging = TempDir::new().unwrap();
        let mut config = make_test_config();
        config.discography.enabled = false;
        config.storage.staging_dir = staging.path().to_string_lossy().into();
        let db = Database::open_in_memory().unwrap();

        run_manual_mode(&client, Some("Test Artist"), None, false, &config, &db)
            .await
            .expect("artist-only manual mode must process all discovered albums");

        assert_eq!(
            client.search_queries.lock().unwrap().as_slice(),
            ["Test Artist"],
            "artist-only mode must discover albums with one artist query"
        );
        let downloaded = client.download_filenames.lock().unwrap().clone();
        assert_eq!(
            downloaded.len(),
            4,
            "artist-only mode must download every track from both discovered albums"
        );
        assert!(downloaded.iter().any(|name| name.contains("Album One")));
        assert!(downloaded.iter().any(|name| name.contains("Album Two")));
        let processed = db.get_processed_albums().unwrap();
        assert_eq!(processed.len(), 2);
        assert!(processed
            .iter()
            .all(|record| record.artist == "Test Artist"));
        assert!(processed.iter().any(|record| record.album == "Album One"));
        assert!(processed.iter().any(|record| record.album == "Album Two"));
    }

    #[tokio::test]
    async fn artist_only_manual_mode_preserves_multi_disc_organization() {
        let client = MockClient::new();
        *client.write_files.lock().unwrap() = true;
        *client.search_results.lock().unwrap() = vec![SearchResult {
            username: "artist-peer".into(),
            speed: 500,
            slots: 1,
            files: vec![
                make_file(
                    r"Test Artist\Album One\CD 01\01 - one.flac",
                    900,
                    10_000_000,
                ),
                make_file(
                    r"Test Artist\Album One\CD 02\01 - one.flac",
                    900,
                    10_000_000,
                ),
            ],
        }];

        let staging = TempDir::new().unwrap();
        let library = TempDir::new().unwrap();
        let mut config = make_test_config();
        config.discography.enabled = false;
        config.storage.staging_dir = staging.path().to_string_lossy().into();
        config.storage.organize = true;
        config.library.paths = vec![library.path().to_string_lossy().into()];
        let db = Database::open_in_memory().unwrap();

        run_manual_mode(&client, Some("Test Artist"), None, false, &config, &db)
            .await
            .expect("artist-only manual mode must organize all discs");

        let album_dir = library.path().join("Test Artist").join("Album One");
        assert!(album_dir.join("CD 01/01 - one.flac").exists());
        assert!(album_dir.join("CD 02/01 - one.flac").exists());
    }

    #[tokio::test]
    async fn artist_only_manual_mode_matches_processed_album_case_insensitively() {
        let client = MockClient::new();
        *client.search_results.lock().unwrap() = vec![SearchResult {
            username: "artist-peer".into(),
            speed: 500,
            slots: 1,
            files: vec![make_file(
                r"Test Artist\ALBUM ONE\01 - one.flac",
                900,
                10_000_000,
            )],
        }];

        let staging = TempDir::new().unwrap();
        let mut config = make_test_config();
        config.discography.enabled = false;
        config.storage.staging_dir = staging.path().to_string_lossy().into();
        let db = Database::open_in_memory().unwrap();
        db.mark_album_processed("Test Artist", "Album One", "success")
            .unwrap();

        run_manual_mode(&client, Some("Test Artist"), None, false, &config, &db)
            .await
            .expect("artist-only manual mode must honor processed albums");

        assert!(
            client.download_filenames.lock().unwrap().is_empty(),
            "case-only album path differences must not trigger a re-download"
        );
    }

    // ── Authoritative discography integration ──

    use crate::client::DownloadHandle;
    use crate::discography::{
        ArtistCandidate, DiscographyError, DiscographyProvider, ReleaseGroup,
    };
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct FakeDiscographyProvider {
        groups: Vec<ReleaseGroup>,
        failure: Option<String>,
    }

    impl FakeDiscographyProvider {
        fn with_groups(groups: Vec<ReleaseGroup>) -> Self {
            Self {
                groups,
                failure: None,
            }
        }

        fn failing(reason: &str) -> Self {
            Self {
                groups: Vec::new(),
                failure: Some(reason.to_string()),
            }
        }
    }

    #[async_trait]
    impl DiscographyProvider for FakeDiscographyProvider {
        async fn search_artists(
            &self,
            artist: &str,
        ) -> std::result::Result<Vec<ArtistCandidate>, DiscographyError> {
            if let Some(reason) = &self.failure {
                return Err(DiscographyError::Transport(reason.clone()));
            }
            Ok(vec![ArtistCandidate {
                id: "11111111-1111-1111-1111-111111111111".to_string(),
                name: artist.to_string(),
            }])
        }

        async fn artist_by_id(
            &self,
            artist_mbid: &str,
        ) -> std::result::Result<ArtistCandidate, DiscographyError> {
            if let Some(reason) = &self.failure {
                return Err(DiscographyError::Transport(reason.clone()));
            }
            Ok(ArtistCandidate {
                id: artist_mbid.to_string(),
                name: "Test Artist".to_string(),
            })
        }

        async fn release_groups(
            &self,
            _artist_mbid: &str,
        ) -> std::result::Result<Vec<ReleaseGroup>, DiscographyError> {
            if let Some(reason) = &self.failure {
                return Err(DiscographyError::Transport(reason.clone()));
            }
            Ok(self.groups.clone())
        }
    }

    fn release_group(id: &str, title: &str, date: &str) -> ReleaseGroup {
        ReleaseGroup {
            id: id.to_string(),
            title: title.to_string(),
            first_release_date: Some(date.to_string()),
            primary_type: Some("Album".to_string()),
            secondary_types: Vec::new(),
        }
    }

    fn album_result(artist: &str, album: &str) -> SearchResult {
        SearchResult {
            username: "peer".to_string(),
            speed: 500,
            slots: 1,
            files: vec![make_file(
                &format!(r"{artist}\{album}\01 - track.flac"),
                900,
                10_000_000,
            )],
        }
    }

    fn artist_only_fixture() -> (Config, Database, TempDir) {
        let staging = TempDir::new().unwrap();
        let mut config = make_test_config();
        config.storage.staging_dir = staging.path().to_string_lossy().into_owned();
        config.filters.min_tracks = 1;
        config.filters.contiguous_tracks = true;
        config.download.min_upload_speed_kbps = 0;
        (config, Database::open_in_memory().unwrap(), staging)
    }

    struct CancelAfterFirstSearchClient {
        inner: MockClient,
        cancel: Arc<AtomicBool>,
        searches: AtomicUsize,
    }

    #[async_trait]
    impl SoulseekClient for CancelAfterFirstSearchClient {
        async fn login(
            &self,
            username: &str,
            password: &str,
            server: &str,
            port: u16,
        ) -> Result<()> {
            self.inner.login(username, password, server, port).await
        }

        async fn search(&self, query: &str, timeout_secs: u64) -> Result<Vec<SearchResult>> {
            let result = self.inner.search(query, timeout_secs).await;
            if self.searches.fetch_add(1, Ordering::SeqCst) == 0 {
                self.cancel.store(true, Ordering::SeqCst);
            }
            result
        }

        async fn download(
            &self,
            file: &FileInfo,
            username: &str,
            dir: &Path,
        ) -> Result<DownloadHandle> {
            self.inner.download(file, username, dir).await
        }
    }

    #[tokio::test]
    async fn artist_only_authoritative_discovery_searches_each_album_oldest_first() {
        let soulseek = MockClient::new();
        soulseek.search_results_by_query.lock().unwrap().insert(
            "Test Artist Older".into(),
            vec![album_result("Test Artist", "Older")],
        );
        soulseek.search_results_by_query.lock().unwrap().insert(
            "Test Artist Newer".into(),
            vec![album_result("Test Artist", "Newer")],
        );
        let provider = FakeDiscographyProvider::with_groups(vec![
            release_group("new", "Newer", "2005"),
            release_group("old", "Older", "1999"),
        ]);
        let (config, db, staging) = artist_only_fixture();

        let run = run_artist_only_mode_with_provider(
            &soulseek,
            "Test Artist",
            false,
            &config,
            &db,
            staging.path(),
            None,
            &Arc::new(AtomicBool::new(false)),
            &provider,
        )
        .await
        .unwrap();

        assert_eq!(
            soulseek.search_queries.lock().unwrap().as_slice(),
            ["Test Artist Older", "Test Artist Newer"]
        );
        assert_eq!(run.outcomes.len(), 2);
        assert!(run.notice.is_none());
    }

    #[tokio::test]
    async fn authoritative_processed_album_skips_before_search() {
        let soulseek = MockClient::new();
        soulseek.search_results_by_query.lock().unwrap().insert(
            "Test Artist Older".into(),
            vec![album_result("Test Artist", "Older")],
        );
        soulseek.search_results_by_query.lock().unwrap().insert(
            "Test Artist Newer".into(),
            vec![album_result("Test Artist", "Newer")],
        );
        let provider = FakeDiscographyProvider::with_groups(vec![
            release_group("new", "Newer", "2005"),
            release_group("old", "Older", "1999"),
        ]);
        let (config, db, staging) = artist_only_fixture();
        db.mark_album_processed("Test Artist", "Older", "success")
            .unwrap();

        let run = run_artist_only_mode_with_provider(
            &soulseek,
            "Test Artist",
            false,
            &config,
            &db,
            staging.path(),
            None,
            &Arc::new(AtomicBool::new(false)),
            &provider,
        )
        .await
        .unwrap();

        assert_eq!(
            soulseek.search_queries.lock().unwrap().as_slice(),
            ["Test Artist Newer"],
            "the already-processed older album must be skipped before any search"
        );
        assert_eq!(run.outcomes[0].1, AlbumOutcome::Skipped);
    }

    #[tokio::test]
    async fn authoritative_album_failure_continues() {
        let soulseek = MockClient::new();
        // Only the newer album has results; the older album's targeted search
        // comes back empty and fails, but iteration must continue.
        soulseek.search_results_by_query.lock().unwrap().insert(
            "Test Artist Newer".into(),
            vec![album_result("Test Artist", "Newer")],
        );
        let provider = FakeDiscographyProvider::with_groups(vec![
            release_group("new", "Newer", "2005"),
            release_group("old", "Older", "1999"),
        ]);
        let (config, db, staging) = artist_only_fixture();

        let run = run_artist_only_mode_with_provider(
            &soulseek,
            "Test Artist",
            false,
            &config,
            &db,
            staging.path(),
            None,
            &Arc::new(AtomicBool::new(false)),
            &provider,
        )
        .await
        .unwrap();

        let queries = soulseek.search_queries.lock().unwrap().clone();
        let older = queries
            .iter()
            .position(|query| query == "Test Artist Older")
            .expect("the older album's primary query must run first");
        let newer = queries
            .iter()
            .position(|query| query == "Test Artist Newer")
            .expect("the newer album must still be searched after a failure");
        assert!(
            older < newer,
            "album queries must stay in chronological order, got {queries:?}"
        );
        assert!(
            matches!(run.outcomes[0].1, AlbumOutcome::Failed { .. }),
            "the empty older album must fail"
        );
        assert!(
            matches!(run.outcomes[1].1, AlbumOutcome::Downloaded { .. }),
            "the newer album must still download after the older album failed"
        );
    }

    #[tokio::test]
    async fn authoritative_cancellation_stops_iteration() {
        let cancel = Arc::new(AtomicBool::new(false));
        let soulseek = MockClient::new();
        soulseek.search_results_by_query.lock().unwrap().insert(
            "Test Artist Older".into(),
            vec![album_result("Test Artist", "Older")],
        );
        soulseek.search_results_by_query.lock().unwrap().insert(
            "Test Artist Newer".into(),
            vec![album_result("Test Artist", "Newer")],
        );
        let client = CancelAfterFirstSearchClient {
            inner: soulseek,
            cancel: Arc::clone(&cancel),
            searches: AtomicUsize::new(0),
        };
        let provider = FakeDiscographyProvider::with_groups(vec![
            release_group("new", "Newer", "2005"),
            release_group("old", "Older", "1999"),
        ]);
        let (config, db, staging) = artist_only_fixture();

        run_artist_only_mode_with_provider(
            &client,
            "Test Artist",
            false,
            &config,
            &db,
            staging.path(),
            None,
            &cancel,
            &provider,
        )
        .await
        .unwrap();

        let queries = client.inner.search_queries.lock().unwrap().clone();
        assert!(
            !queries.contains(&"Test Artist Newer".to_string()),
            "cancellation after the first album's search must stop iteration, got {queries:?}"
        );
        assert!(
            cancel.load(Ordering::SeqCst),
            "the first search must have set the cancellation flag"
        );
    }

    #[tokio::test]
    async fn authoritative_empty_searches_nothing() {
        let soulseek = MockClient::new();
        let provider = FakeDiscographyProvider::with_groups(Vec::new());
        let (config, db, staging) = artist_only_fixture();

        let run = run_artist_only_mode_with_provider(
            &soulseek,
            "Test Artist",
            false,
            &config,
            &db,
            staging.path(),
            None,
            &Arc::new(AtomicBool::new(false)),
            &provider,
        )
        .await
        .unwrap();

        assert!(
            soulseek.search_queries.lock().unwrap().is_empty(),
            "an authoritative empty discovery must not search Soulseek"
        );
        assert!(
            run.outcomes.is_empty(),
            "an authoritative empty result is not a failed or skipped album"
        );
        assert_eq!(
            run.notice.as_deref(),
            Some("No eligible authoritative albums found for Test Artist")
        );
    }

    #[test]
    fn stale_cache_warns_without_legacy_notice() {
        use crate::db::DiscographyCacheEntry;

        let buf = Arc::new(Mutex::new(String::new()));
        let writer = CapturingWriter(buf.clone());
        let subscriber = tracing_subscriber::fmt()
            .with_writer(writer)
            .with_max_level(tracing::Level::INFO)
            .with_ansi(false)
            .without_time()
            .finish();

        tracing::subscriber::with_default(subscriber, || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let soulseek = MockClient::new();
                let provider = FakeDiscographyProvider::failing("service unavailable");
                let (config, db, staging) = artist_only_fixture();
                db.upsert_discography_cache(&DiscographyCacheEntry {
                    artist_key: "test artist".to_string(),
                    artist_mbid: "11111111-1111-1111-1111-111111111111".to_string(),
                    canonical_artist: "Test Artist".to_string(),
                    fetched_at: 0,
                    release_groups_json: serde_json::to_string(&[release_group(
                        "old",
                        "Old Album",
                        "1998",
                    )])
                    .unwrap(),
                })
                .unwrap();

                let run = run_artist_only_mode_with_provider(
                    &soulseek,
                    "Test Artist",
                    false,
                    &config,
                    &db,
                    staging.path(),
                    None,
                    &Arc::new(AtomicBool::new(false)),
                    &provider,
                )
                .await
                .unwrap();

                let captured = buf.lock().unwrap().clone();
                assert!(
                    captured.contains("day(s) old")
                        && captured.contains("service unavailable")
                        && captured.contains("stale"),
                    "the stale-cache WARN must name the age and refresh error, got:\n{captured}"
                );
                assert!(
                    run.notice.is_none(),
                    "a stale cache must not produce a legacy-fallback notice"
                );
            });
        });
    }

    #[tokio::test]
    async fn automatic_legacy_fallback_is_visible() {
        let soulseek = MockClient::new();
        soulseek.search_results_by_query.lock().unwrap().insert(
            "Test Artist".into(),
            vec![album_result("Test Artist", "Album One")],
        );
        let provider = FakeDiscographyProvider::failing("service unavailable");
        let (config, db, staging) = artist_only_fixture();

        let run = run_artist_only_mode_with_provider(
            &soulseek,
            "Test Artist",
            false,
            &config,
            &db,
            staging.path(),
            None,
            &Arc::new(AtomicBool::new(false)),
            &provider,
        )
        .await
        .unwrap();

        assert_eq!(
            soulseek.search_queries.lock().unwrap().as_slice(),
            ["Test Artist"],
            "the automatic legacy fallback must use the original one-artist query"
        );
        let notice = run
            .notice
            .expect("an automatic legacy fallback must attach a run notice");
        assert!(
            notice.contains("service unavailable") && notice.contains("heuristically"),
            "the notice must name the transport reason and the heuristic origin, got: {notice}"
        );
    }

    #[tokio::test]
    async fn disabled_discography_uses_legacy_without_outage_notice() {
        let soulseek = MockClient::new();
        soulseek.search_results_by_query.lock().unwrap().insert(
            "Test Artist".into(),
            vec![album_result("Test Artist", "Album One")],
        );
        let (mut config, db, staging) = artist_only_fixture();
        config.discography.enabled = false;

        let run = run_artist_only_mode(
            &soulseek,
            "Test Artist",
            false,
            &config,
            &db,
            staging.path(),
            None,
            &Arc::new(AtomicBool::new(false)),
        )
        .await
        .unwrap();

        assert_eq!(
            soulseek.search_queries.lock().unwrap().as_slice(),
            ["Test Artist"],
            "an explicit disable must keep the legacy one-artist query"
        );
        assert!(
            run.notice.is_none(),
            "an explicit disable is not an outage and must not emit a notice"
        );
    }

    #[tokio::test]
    async fn explicit_and_automatic_modes_keep_query_contracts() {
        // Explicit album fixture: an artist+album manual run must issue exactly
        // the primary "Artist Album" query, unchanged by the artist-only
        // authoritative refactor.
        let explicit = MockClient::new();
        explicit.search_results_by_query.lock().unwrap().insert(
            "Test Artist Test Album".into(),
            vec![album_result("Test Artist", "Test Album")],
        );
        let (config, db, _staging) = artist_only_fixture();
        run_manual_mode(
            &explicit,
            Some("Test Artist"),
            Some("Test Album"),
            false,
            &config,
            &db,
        )
        .await
        .expect("explicit album manual mode must run");
        assert_eq!(
            explicit.search_queries.lock().unwrap().as_slice(),
            ["Test Artist Test Album"],
            "explicit album processing must keep its single targeted query"
        );

        // Album-only fixture: an album-only manual run keeps its album-only
        // query and never issues an artist query.
        let album_only = MockClient::new();
        run_manual_mode(&album_only, None, Some("Test Album"), false, &config, &db)
            .await
            .expect("album-only manual mode must run");
        assert_eq!(
            album_only.search_queries.lock().unwrap().as_slice(),
            ["Test Album".to_string()]
        );
    }

    #[tokio::test]
    async fn test_primary_search_issues_single_query() {
        let client = Arc::new(MockClient::new());
        // With no album, only the artist-only primary search runs. The
        // album-only fallback tier requires an album, so exactly one query
        // is issued and, with no results, the album fails with no results.
        let config = make_test_config();
        let db = Database::open_in_memory().unwrap();
        let staging = TempDir::new().unwrap();

        let result = process_album(
            client.as_ref() as &dyn crate::client::SoulseekClient,
            "Test Artist",
            None,
            false,
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
        assert_eq!(queries, vec!["Test Artist".to_string()]);
    }

    // When the primary search returns results but all are rejected by filters
    // (e.g. contiguity gate), the album must be marked as failed with
    // "no results passed filters". The filter-aware search cascade continues
    // past the unusable primary tier (lowercase + album-only), but no tier
    // yields a usable share, so the first non-empty tier's results come back
    // and are rejected by process_album's own filter pass.
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
            false,
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

        // The cascade ran all tiers (primary, lowercase, album-only) because
        // no tier survived the probe; every tier's results are gappy and were
        // rejected by the filters inside search_album_with_fallback.
        let queries = client.search_queries.lock().unwrap().clone();
        assert_eq!(
            queries,
            vec![
                "Test Artist Test Album".to_string(),
                "test artist test album".to_string(),
                "Test Album".to_string()
            ]
        );

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
            false,
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
            false,
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

        // Four queries ran: primary, lowercase fallback, album-only
        // fallback, title search.
        let queries = client.search_queries.lock().unwrap().clone();
        assert_eq!(
            queries,
            vec![
                "Adele 25".to_string(),
                "adele 25".to_string(),
                "25".to_string(),
                "i miss you".to_string()
            ]
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

    // Regression: when the local library holds only generic track names
    // ("CD Track N", "Track N", ...), the title-search fallback must be
    // skipped entirely — it could never build a meaningful query. Only the
    // album searches run (primary + album-only), never a track-title query.
    #[tokio::test]
    async fn test_title_search_skips_when_all_tracks_generic() {
        let client = Arc::new(MockClient::new());
        // Primary "Prince The Very Best Of Prince" and album-only
        // "The Very Best Of Prince" return nothing (no map entries, empty
        // static results). The library has only generic names, so the
        // title-search tier must not issue a query.
        let mut config = make_test_config();
        config.search.search_title_match = 70;
        let tmp = TempDir::new().unwrap();
        let album_dir = tmp.path().join("Prince").join("The Very Best Of Prince");
        std::fs::create_dir_all(&album_dir).unwrap();
        std::fs::write(album_dir.join("CD Track 1.mp3"), b"fake mp3").unwrap();
        std::fs::write(album_dir.join("CD Track 2.mp3"), b"fake mp3").unwrap();
        config.library.paths = vec![tmp.path().to_string_lossy().into()];

        let db = Database::open_in_memory().unwrap();
        let staging = TempDir::new().unwrap();

        let result = process_album(
            client.as_ref() as &dyn crate::client::SoulseekClient,
            "Prince",
            Some("The Very Best Of Prince"),
            false,
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

        // Only the three album searches ran (primary + lowercase fallback +
        // album-only) — no track-title query was issued for the all-generic
        // library.
        let queries = client.search_queries.lock().unwrap().clone();
        assert_eq!(
            queries,
            vec![
                "Prince The Very Best Of Prince".to_string(),
                "prince the very best of prince".to_string(),
                "The Very Best Of Prince".to_string()
            ]
        );
    }

    // End-to-end fallback hierarchy: primary "Prince Musicology" is empty
    // (blocked artist), the album-only tier searches "Musicology" and finds
    // a result whose path matches "Prince", and the download succeeds.
    #[tokio::test]
    async fn test_album_only_fallback_fires_when_primary_empty() {
        let client = Arc::new(MockClient::new());
        client.search_results_by_query.lock().unwrap().insert(
            "Musicology".into(),
            vec![SearchResult {
                username: "user1".into(),
                speed: 500,
                slots: 1,
                files: vec![make_file(
                    "Prince/Musicology/01 - Musicology.flac",
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
            "Prince",
            Some("Musicology"),
            false,
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

        // Three searches were attempted: primary, lowercase fallback, then
        // album-only.
        let queries = client.search_queries.lock().unwrap().clone();
        assert_eq!(
            queries,
            vec![
                "Prince Musicology".to_string(),
                "prince musicology".to_string(),
                "Musicology".to_string()
            ]
        );
    }

    // End-to-end lowercase fallback: primary "Prince Musicology" is empty
    // (blocked artist), the lowercase fallback searches "prince musicology"
    // and finds a result whose path matches "Prince", and the download
    // succeeds without reaching the album-only tier.
    #[tokio::test]
    async fn test_lowercase_fallback_fires_when_primary_empty() {
        let client = Arc::new(MockClient::new());
        client.search_results_by_query.lock().unwrap().insert(
            "prince musicology".into(),
            vec![SearchResult {
                username: "user1".into(),
                speed: 500,
                slots: 1,
                files: vec![make_file(
                    "Prince/Musicology/01 - Musicology.flac",
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
            "Prince",
            Some("Musicology"),
            false,
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

        // Two searches were attempted: primary then lowercase fallback — the
        // album-only tier never fired because lowercase found results.
        let queries = client.search_queries.lock().unwrap().clone();
        assert_eq!(
            queries,
            vec![
                "Prince Musicology".to_string(),
                "prince musicology".to_string()
            ]
        );
    }

    // ── Log-capture harness ──

    #[derive(Clone)]
    struct CapturingWriter(Arc<Mutex<String>>);

    impl std::io::Write for CapturingWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .unwrap()
                .push_str(&String::from_utf8_lossy(buf));
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturingWriter {
        type Writer = CapturingWriter;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    // Regression: when primary and album-only searches return nothing and
    // the title-search fallback fires, the bare "Searching for X" line from
    // the soulseek lib must be preceded by a clear, contextual log line that
    // names the artist/album and says we're falling back to a track-title
    // search. Without it a user can't tell what "Searching for tomorrow
    // comes today" even refers to.
    //
    // Uses a global default subscriber (not thread-local set_default) so the
    // async process_album work is captured regardless of which runtime thread
    // runs it. A thread-local set_default only captures logs emitted on the
    // thread that called it, which is unreliable when #[tokio::test]'s async
    // body runs on a different/runtime thread.
    #[tokio::test(flavor = "current_thread")]
    async fn test_title_search_fallback_logs_contextual_message() {
        let buf = Arc::new(Mutex::new(String::new()));
        let writer = CapturingWriter(buf.clone());
        let subscriber = tracing_subscriber::fmt()
            .with_writer(writer)
            .with_max_level(tracing::Level::INFO)
            .with_ansi(false)
            .without_time()
            .finish();
        // Use a global default so all threads' tracing calls are captured.
        // This is the only test in the suite that installs a subscriber, so
        // it is safe to set the process-wide default here.
        tracing::subscriber::set_global_default(subscriber)
            .expect("global tracing subscriber must be set exactly once");

        let client = Arc::new(MockClient::new());
        // Library track "01 - I Miss You.mp3" cleans to "i miss you", the
        // alphabetically-first title and thus the search query. The mock has
        // results ONLY for this query, so the primary search is empty.
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
            false,
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

        let captured = buf.lock().unwrap().clone();
        assert!(
            captured.contains("falling back to track-title search"),
            "expected a contextual fallback log line, got:\n{captured}"
        );
        assert!(
            captured.contains("Adele"),
            "expected the fallback log line to name the artist,\n{captured}"
        );
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
            false,
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
        // The mock writes real file bytes to the staging dir, so
        // copy_to_library has actual content to copy (no pre-seeding).
        *client.write_files.lock().unwrap() = true;

        let result = process_album(
            client.as_ref() as &dyn crate::client::SoulseekClient,
            "Test Artist",
            Some("Test Album"),
            false,
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
        db.mark_album_processed("Test Artist", "Test Album", "success")
            .unwrap();

        let result = run_auto_mode(
            client.as_ref() as &dyn crate::client::SoulseekClient,
            &config,
            &db,
            true,
        )
        .await;
        assert!(result.is_ok());

        // Album processed successfully through the outcome-collection path.
        let rows = db.get_processed_albums().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, "success");
        let queries = client.search_queries.lock().unwrap();
        assert!(
            queries
                .iter()
                .any(|query| query == "Test Artist Test Album"),
            "ignore_processed must let auto mode search the pre-processed target"
        );
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

        // The mock writes real file bytes to staging during the run, so
        // copy_to_library has actual content (no pre-seeding — a pre-seeded
        // dir would be cleaned up as an "untracked leftover" by the
        // startup recovery scan).
        let staging = TempDir::new().unwrap();
        config.storage.staging_dir = staging.path().to_string_lossy().into();
        *client.write_files.lock().unwrap() = true;

        let db = Database::open_in_memory().unwrap();
        let result = run_auto_mode(
            client.as_ref() as &dyn crate::client::SoulseekClient,
            &config,
            &db,
            false,
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
        let mut found = vec![];
        fn walk(p: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            if p.is_dir() {
                if let Ok(rd) = std::fs::read_dir(p) {
                    for e in rd.flatten() {
                        let q = e.path();
                        if q.is_dir() {
                            walk(&q, out);
                        } else {
                            out.push(q);
                        }
                    }
                }
            }
        }
        walk(tmp.path(), &mut found);
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
            false,
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
