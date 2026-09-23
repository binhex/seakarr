use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::Semaphore;

use futures::FutureExt;

use crate::client::SoulseekClient;
use crate::config::Config;
use crate::db::Database;
use crate::discography::{
    discover_artist_albums, DiscographyProvider, DiscoveryFailure, DiscoveryOutcome,
    DiscoveryProvenance, FailureCacheUse, MusicBrainzProvider,
};
use crate::error::{Result, SeakarrError};
use crate::progress::{is_interactive, ProgressDisplay};
use crate::report::{AlbumOutcome, DownloadDestination, RunReport};
use crate::scan_progress;
use crate::{discover, download, filter, notifier, organizer, scanner, search};

/// Spawn a SIGINT (Ctrl+C) listener for the duration of a run.
///
/// The first press sets the shared cancellation flag, cancelling the run in
/// flight — an album download (its staging dir is cleaned by `download_album`) or
/// the library scan. A second
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
                    tracing::info!("Received SIGINT — cancelling the run...");
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

/// Owns one run's SIGINT listener and aborts it when the run ends.
///
/// A scheduled loop calls a mode once per cycle, so the listener must not
/// outlive its run: dropping a [`tokio::task::JoinHandle`] only detaches the
/// task, which would leave one task, one signal receiver and one flag per cycle,
/// and every one of them would log on the next Ctrl+C. Aborting from `Drop`
/// makes that structural, so an early return — a cancelled scan, an empty work
/// list, a propagated error — cannot leak one.
pub struct CancellationGuard {
    handle: tokio::task::JoinHandle<()>,
}

impl Drop for CancellationGuard {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// Arm the SIGINT listener and hand back the flag it flips.
///
/// Armed *before* the library scan: the scan reads every audio file's tags and
/// is the longest silent phase of a run, so a listener installed only afterwards
/// left Ctrl+C to the default signal action, which killed the process, left the
/// PID file behind, and made the next run warn about a dead PID.
///
/// Residual window: `tokio::signal::ctrl_c` registers the OS handler when the
/// spawned listener task is first polled and the runtime has an idle worker, so
/// a SIGINT delivered between this returning and that first poll still takes the
/// default disposition. It is microseconds against a scan of minutes, and
/// closing it needs either an await on a registration handshake (making this
/// async at every call site) or a platform-specific `signal::unix` stream; the
/// window is documented instead of papered over.
pub fn arm_cancellation() -> (Arc<AtomicBool>, CancellationGuard) {
    let cancel = Arc::new(AtomicBool::new(false));
    let listener = spawn_cancel_listener(Arc::clone(&cancel));
    (cancel, CancellationGuard { handle: listener })
}

/// Scan the library for a mode that has already armed cancellation.
///
/// `Ok(None)` means the user cancelled: the caller returns without doing any
/// work, and `main` releases the PID lock as it unwinds.
///
/// The indicator is created here rather than inside the walk, so the scan and
/// the downloads share one `ProgressDisplay` and the terminal has one owner per
/// run. With `progress: None` (a headless run) no bar is created and the
/// console heartbeat is left alone.
fn scan_library_cancellable(
    config: &Config,
    cancel: &AtomicBool,
    progress: Option<&ProgressDisplay>,
) -> Result<Option<Vec<scanner::ScannedAlbum>>> {
    let outcome = with_scan_indicator(progress, |walk_progress| {
        scanner::scan_library(
            &config.library.paths,
            &config.filters,
            Some(cancel),
            walk_progress,
        )
    });
    match outcome {
        Ok(albums) => Ok(Some(albums)),
        Err(SeakarrError::Cancelled) => {
            tracing::info!("Library scan cancelled — aborting before any work item");
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

/// Run `scan` with the interactive scan indicator attached.
///
/// One helper for every library scan in the run, so the indicator is built in
/// exactly one place and each scan reports and releases it the same way. With
/// `progress: None` (a headless run) no bar is created and the console
/// heartbeat is left alone.
fn with_scan_indicator<T>(
    progress: Option<&ProgressDisplay>,
    scan: impl FnOnce(Option<&dyn scanner::ScanProgress>) -> T,
) -> T {
    let indicator =
        scan_progress::ScanIndicator::start(progress, scan_progress::installed_console_filter());
    scan(Some(&indicator))
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

fn unique_user_count(results: &[crate::client::SearchResult]) -> usize {
    results
        .iter()
        .map(|result| result.username.to_lowercase())
        .collect::<std::collections::HashSet<_>>()
        .len()
}

fn candidate_disc_numbers(result: &crate::client::SearchResult) -> std::collections::BTreeSet<u32> {
    result
        .files
        .iter()
        .filter_map(|file| {
            let parent = file.name.rsplit_once(['/', '\\'])?.0;
            let leaf = parent.rsplit(['/', '\\']).next()?;
            crate::discs::disc_number(leaf)
        })
        .collect()
}

/// Remove marked candidates that contain fewer discs than the most complete
/// candidate for this identity. Flat-folder candidates remain eligible. When
/// every marked candidate contains only one disc but several disc numbers are
/// advertised across peers, none is complete and the album fails for retry.
fn remove_incomplete_split_disc_candidates(results: &mut Vec<crate::client::SearchResult>) -> bool {
    let disc_sets: Vec<std::collections::BTreeSet<u32>> =
        results.iter().map(candidate_disc_numbers).collect();
    let advertised: std::collections::BTreeSet<u32> = disc_sets.iter().flatten().copied().collect();
    if advertised.len() < 2 {
        return false;
    }
    let largest = disc_sets
        .iter()
        .map(std::collections::BTreeSet::len)
        .max()
        .unwrap_or(0);
    if largest <= 1 {
        results.clear();
        return true;
    }
    let mut index = 0usize;
    results.retain(|_| {
        let keep = disc_sets[index].is_empty() || disc_sets[index].len() == largest;
        index += 1;
        keep
    });
    results.is_empty()
}

/// Where a completed download is written, and what its write is allowed to do.
///
/// `Upgrade` is auto mode's replacement of an album that already exists but
/// fails the quality gate: it is gated by the caller on
/// `library_upgrade.enabled`, compares the download against the library's own
/// track count, and may delete lesser-quality files. `Place` is the placement of
/// a newly downloaded album beside the artist's existing albums, used by discover
/// mode and by manual runs: it carries no completeness baseline and never deletes
/// anything. `skip_existing_album` is manual mode's rule that an album folder
/// which already exists keeps the download in staging instead of being written
/// into; discover passes `false`. `StagingOnly` is a manual run with no artist
/// folder to place into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LibraryTarget<'a> {
    Upgrade {
        root: &'a Path,
        expected_tracks: usize,
    },
    Place {
        root: &'a Path,
        artist_dir: &'a str,
        skip_existing_album: bool,
    },
    /// A manual run whose artist has no library folder: the album stays in staging
    /// and nothing is written to the library, not even by the generic organize
    /// step, because a manual run never creates an artist folder.
    StagingOnly,
}

/// The album directory placement would write into, when it already exists and the
/// caller asked for the existing-album rule.
///
/// `skip_existing_album` callers use this to tell whether the album's folder is
/// already on disk before anything is copied: placement never replaces a readable
/// file, so writing into an existing folder could report success while adding
/// nothing. `None` means the caller may place normally.
fn existing_album_destination(
    skip_existing_album: bool,
    downloaded: &[PathBuf],
    config: &Config,
    root: &Path,
    artist_dir: &str,
    album: Option<&str>,
) -> Option<PathBuf> {
    if !skip_existing_album {
        return None;
    }
    let album_dir = downloaded.first().and_then(|first| {
        organizer::placement_album_dir(
            root,
            &config.storage.organize_pattern,
            artist_dir,
            album.unwrap_or("Unknown"),
            first,
        )
    })?;
    // Only a destination strictly below the artist folder is an album folder. A
    // pattern that puts the tracks straight into the artist folder, writes to the
    // library root, or omits `%artist%` derives that folder (or something outside it)
    // instead; treating it as an existing album folder would make manual placement
    // impossible under the pattern, and placement's own keep-existing rule handles
    // files that are already there. `Path` equality compares components, so a
    // pattern spelling the artist component with a redundant prefix (`./`) still
    // matches.
    let artist_dir_path = root.join(artist_dir);
    if album_dir == artist_dir_path || !album_dir.starts_with(&artist_dir_path) {
        return None;
    }
    album_dir.is_dir().then_some(album_dir)
}

/// Reason a downloaded set must not be written into the library, or `None` when
/// it may be.
///
/// This is the post-download half of one shared rule —
/// [`crate::filter::incomplete_download`] — applied to the same set the
/// pre-download filter already judged (identical quality filter, identical
/// grouping), so no run reaches it today: the filter refuses such a set before
/// anything is fetched. It is kept as a defensive backstop for a future change to
/// the download layer, and the paths that call it (the `Place` arm, used by
/// discover and by manual runs, and generic organize) are backstops in the same
/// sense. The library-upgrade path does not
/// call it: its own gate compares the
/// download against `needs_upgrade`, the number of files that failed the quality
/// gate for that album — a different reference, not a stronger one, so the two
/// gates do not subsume each other.
///
/// `min_tracks == 0` disables the gate, exactly as it disables the pre-download
/// gate, so EPs and singles stay reachable for operators who ask for them.
fn library_write_refusal(downloaded: &[PathBuf], min_tracks: u32) -> Option<String> {
    let names: Vec<&str> = downloaded
        .iter()
        .filter_map(|path| path.file_name().and_then(|name| name.to_str()))
        .collect();
    crate::filter::incomplete_download(&names, min_tracks, crate::filter::TrackOneAnchor::Required)
        .map(|kind| kind.reason())
}

/// Remove `dir` and its now-empty subdirectories, deepest first, and report what
/// kept a directory in place. `None` means the tree is gone (a directory that never
/// existed included).
///
/// `remove_dir` refuses a non-empty directory, so the emptiness check and the
/// removal are one operation: nothing this run did not stage can be deleted, and a
/// file staged concurrently between the download and this call cannot be caught by
/// a stale scan.
fn remove_empty_staging_dirs(dir: &Path) -> Option<PathBuf> {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
                remove_empty_staging_dirs(&path);
            }
        }
    }
    match std::fs::remove_dir(dir) {
        Ok(()) => None,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        // A non-empty directory names its first entry; any other failure (an
        // unwritable parent, a racing removal) names the directory itself, so the
        // caller always warns about something that is really still on disk.
        Err(_) => first_remaining_entry(dir).or_else(|| Some(dir.to_path_buf())),
    }
}

/// The first entry left in `dir`, for a warning that names what stayed. The entry
/// may be a file or a subdirectory.
fn first_remaining_entry(dir: &Path) -> Option<PathBuf> {
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .next()
        .map(|entry| entry.path())
}

/// Drop a refused album's staged files. A refusal means the set is never
/// written to the library, so keeping it would accumulate unusable audio in
/// `storage.staging_dir` while the album is re-downloaded from scratch on the
/// next run.
///
/// Exactly the paths this run staged are removed, never anything else. The staging
/// slug is `artist--album`, which two different pairs can collapse onto (`A--B` +
/// `C` and `A` + `B--C`), and albums are processed concurrently, so the directory
/// can hold another album's file; directories are removed only once they are empty
/// and anything left behind is named in a warning. A directory that was never
/// created is not a failure.
///
/// Ownership is by path, so a foreign file that happens to land on the same path as
/// one of ours is removed with ours — the two are indistinguishable, and the slug
/// collapse that produces that state is the same one the warning below describes.
fn discard_refused_staging(album_staging: &Path, downloaded: &[PathBuf]) {
    for path in downloaded {
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!(
                "Failed to remove the staged file {path:?} of a refused download: {e}"
            ),
        }
    }
    if let Some(remaining) = remove_empty_staging_dirs(album_staging) {
        if remaining == album_staging {
            tracing::warn!(
                "Leaving {album_staging:?} in place: it could not be removed, and it may hold an entry this run did not stage (another album on the same artist--album staging name, or leftovers from an earlier run)"
            );
        } else {
            tracing::warn!(
                "Leaving {album_staging:?} in place: it still holds {remaining:?}, the first entry this run did not stage (a file or subdirectory — another album on the same artist--album staging name, or leftovers from an earlier run)"
            );
        }
    }
}

/// Shared tail for both library writes: drop the staging copy, record the
/// album as processed, notify, and report the completed album.
async fn finish_library_write(
    config: &Config,
    db: &Database,
    album_staging: &Path,
    artist: &str,
    album: Option<&str>,
    track_count: usize,
    destination: DownloadDestination,
) -> Result<AlbumOutcome> {
    // A library write has moved the album out of staging, so the staging copy is
    // dropped. A staging destination means staging *is* the album's final
    // location — removing it would delete the album.
    if matches!(destination, DownloadDestination::Library(_)) {
        if let Err(e) = std::fs::remove_dir_all(album_staging) {
            tracing::warn!("Failed to remove staging dir {album_staging:?}: {e}");
        }
    }
    mark_album_processed_if_identifiable(db, artist, album, "success")?;
    if let Err(e) = notifier::notify_success(
        &config.notifications.urls,
        artist,
        album.unwrap_or("Unknown"),
        track_count,
        destination.render().as_str(),
    )
    .await
    {
        tracing::warn!(
            "{artist} - {}: notification failed: {e}",
            album.unwrap_or("(all)")
        );
    }
    tracing::info!(
        "Completed: {artist} - {} ({track_count} tracks) -> {}",
        album.unwrap_or("(all)"),
        destination.render()
    );
    Ok(AlbumOutcome::Downloaded {
        track_count,
        destination,
    })
}

/// Process a single album: search → filter rank → download → organize → notify.
///
/// When a [`LibraryTarget`] is supplied, a completed download is written into
/// the library instead of the generic organize step and the album completes
/// early (the organize block below is bypassed): `Upgrade` copies back into an
/// album's existing library directory behind the completeness gate, and
/// `Place` writes a newly downloaded album beside the artist's existing albums.
/// A `Place` target whose `skip_existing_album` is set keeps the download in
/// staging instead, when a directory a placement would write into already
/// exists — an album folder that already exists, whether or not it holds audio:
/// placement never replaces a readable file, so the album completes as
/// downloaded rather than being written into a folder that may already hold the
/// album. `StagingOnly` also keeps the download in staging and additionally
/// suppresses the generic organize step, because a manual run never creates an
/// artist folder.
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
    target: Option<LibraryTarget<'_>>,
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
        target,
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
    target: Option<LibraryTarget<'_>>,
    presearched_results: Option<Vec<crate::client::SearchResult>>,
) -> Result<AlbumOutcome> {
    // Only a run that reaches the generic organize step needs an artist: that step
    // builds `<root>/<artist>/<album>`, so an album-only batch line with organizing
    // enabled cannot be organized. A manual album-only run passes `StagingOnly` and
    // never organizes, so it proceeds and keeps its download in staging.
    if artist.trim().is_empty()
        && config.storage.organize
        && !config.library.paths.is_empty()
        && !matches!(target, Some(LibraryTarget::StagingOnly))
    {
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

    // The completeness rule's track-1 anchor applies to a new album, not to a
    // library-upgrade candidate: the library already holds its own track 1 and the
    // upgrade only replaces the files that failed the quality gate, so a peer
    // delivering just those files is a legitimate source. This mirrors where the
    // post-download gate applies the anchor — `library_write_refusal` on the
    // placement and organize paths, not the upgrade path's `expected_tracks`.
    let anchor = match &target {
        Some(LibraryTarget::Upgrade { .. }) => filter::TrackOneAnchor::NotRequired,
        _ => filter::TrackOneAnchor::Required,
    };

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
                anchor,
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
    if presearched && remove_incomplete_split_disc_candidates(&mut results) {
        mark_album_processed_if_identifiable(db, artist, album, "failed")?;
        // No download was attempted, so this is a no-candidate outcome rather
        // than a failed attempt. It keeps the discover budget honest if this
        // path is ever reached from a presearched work list.
        return Ok(AlbumOutcome::NoCandidates {
            reason: "multi-disc album is split across peers; no complete candidate available"
                .into(),
        });
    }

    // Filter + rank
    let mut total_results: usize = results.iter().map(|r| r.files.len()).sum();
    let mut total_users = unique_user_count(&results);
    // Artist-only legacy results were already assigned to one normalized album
    // group. Reapplying one chosen display spelling as an album-name gate would
    // discard peers whose folder uses another spelling of that same release.
    let filter_album = if presearched { None } else { album };
    let mut filtered = filter::filter_results_with_queue_limit(
        &results,
        &config.filters,
        library_track_count,
        filter_album,
        config.download.max_queue_length,
        anchor,
    );
    // Track which results were last filtered (for rejection summary)
    let mut last_filtered_results: Vec<crate::client::SearchResult> = results.clone();
    // Title-search fallback: when the primary search returned no usable results,
    // search again by the cleaned title of the album's alphabetically-first
    // library track and keep only the results that contain the album's library
    // track titles. Only fires when the local library holds the album (enabling
    // the title list), the title search is enabled, an album is being processed,
    // and the manual target includes an artist for the library-path lookup.
    // Track whether the title-search tier actually fired
    let mut title_search_attempted = false;
    // True once the title-search results are what `last_filtered_results` holds.
    // A tier that fired but returned nothing leaves the primary set in place, and
    // the album gate still explains those rejections.
    let mut summarising_title_results = false;

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
                        // Without this line the Soulseek lib gives no clue that
                        // this is a track-title fallback, nor for which album, so
                        // log it up front and let the user tie the query to the
                        // album.
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
                                    total_users = unique_user_count(&title_results);
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
                                        anchor,
                                    );
                                    last_filtered_results = title_results.clone();
                                    summarising_title_results = true;
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
            // If the title tier produced results that were rejected by filters,
            // print a rejection summary so the user knows WHY.
            if summarising_title_results && !last_filtered_results.is_empty() {
                let rejection_summary = filter::summarize_rejections_with_queue_limit(
                    &last_filtered_results,
                    &config.filters,
                    library_track_count,
                    // Title-search results are never album-gated.
                    None,
                    config.download.max_queue_length,
                    anchor,
                );
                if rejection_summary.has_rejections() {
                    tracing::info!(
                        "  → {} (title-search results)",
                        rejection_summary.summary_line(),
                    );
                }
            }
            mark_album_processed_if_identifiable(db, artist, album, "failed")?;
            return Ok(AlbumOutcome::NoCandidates {
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
            // The album gate applies only while the summarised set is the
            // primary one.
            if summarising_title_results {
                None
            } else {
                filter_album
            },
            config.download.max_queue_length,
            anchor,
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
        return Ok(AlbumOutcome::NoCandidates {
            reason: "no results passed filters".into(),
        });
    }
    // Rank bonus applies only to primary-tier results: when the title-search
    // fallback fired, the album name is not a meaningful discriminator (we
    // searched by track title because the album name search failed).
    let rank_album = if title_search_attempted {
        None
    } else {
        filter_album
    };
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
        unique_user_count(&filtered),
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
            // Debug, not warn: the outcome is already recorded in the run
            // report (printed at INFO as `Failed (n):` with the same reason)
            // and in the processed-albums table, so an inline warning would
            // repeat the line the summary carries.
            tracing::debug!(
                "{artist} — {}: download failed ({reason}); {} candidates exhausted",
                album.unwrap_or("(all)"),
                ranked.len(),
            );
            mark_album_processed_if_identifiable(db, artist, album, "failed")?;
            return Ok(AlbumOutcome::Failed { reason });
        }
    };

    // Library write: auto mode's gated upgrade, or discover mode's placement.
    match target {
        Some(LibraryTarget::Upgrade {
            root,
            expected_tracks,
        }) => {
            // Completeness gate: the album's own count of files that failed the
            // quality gate (`needs_upgrade`) is the reference — NOT the best
            // peer's folder size. Peers share different editions (box sets,
            // anniversary editions) whose folder can contain far more files than
            // the album being upgraded needs replacing (e.g. a 121-file peer
            // folder for an album with 2 non-conforming tracks).
            if downloaded.len() < expected_tracks {
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
                    expected_tracks,
                );
                discard_refused_staging(&album_staging, &downloaded);
                mark_album_processed_if_identifiable(db, artist, album, "failed")?;
                return Ok(AlbumOutcome::Failed {
                    reason: "incomplete download, library upgrade skipped".into(),
                });
            }
            match organizer::copy_to_library(
                &downloaded,
                root,
                &config.storage.organize_pattern,
                artist,
                album.unwrap_or("Unknown"),
            ) {
                Ok(outcome) => {
                    if config.library_upgrade.delete_lesser_quality {
                        match organizer::delete_lesser_quality_files(
                            root,
                            artist,
                            album.unwrap_or("Unknown"),
                            &outcome.written,
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
                    let track_count = downloaded.len();
                    return finish_library_write(
                        config,
                        db,
                        &album_staging,
                        artist,
                        album,
                        track_count,
                        DownloadDestination::Library(outcome.album_dir),
                    )
                    .await;
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
        Some(LibraryTarget::Place {
            root,
            artist_dir,
            skip_existing_album,
        }) => {
            // Manual mode's rule: an album folder that already exists wins, because
            // placement never replaces a readable file and could otherwise report
            // success while adding nothing. Checked before the completeness backstop
            // below, so a refused set keeps its staging copy rather than being
            // discarded.
            if let Some(existing) = existing_album_destination(
                skip_existing_album,
                &downloaded,
                config,
                root,
                artist_dir,
                album,
            ) {
                tracing::info!(
                    "{artist} - {}: album folder already exists at {}; leaving the download in staging",
                    album.unwrap_or("?"),
                    existing.display()
                );
                return finish_library_write(
                    config,
                    db,
                    &album_staging,
                    artist,
                    album,
                    downloaded.len(),
                    DownloadDestination::Staging(album_staging.clone()),
                )
                .await;
            }
            // A new album has no library track count to compare against, so the
            // completeness test here is the configured `min_tracks` plus the
            // numbering check instead (see `library_write_refusal`). Presence
            // already treats any audio file under the album folder as present,
            // and there is no quality deletion either — nothing is being
            // replaced. `place_into_library` uses the folder name that
            // already exists on disk verbatim, so the album lands inside it
            // instead of beside a rewritten copy of it, and it never replaces a
            // readable existing file because the destination folder may belong
            // to a different edition of the album.
            if let Some(reason) = library_write_refusal(&downloaded, config.filters.min_tracks) {
                // The serving peer also takes an album-level failure, as it does for
                // an incomplete library-upgrade download. The demotion is light: the
                // tracks it did deliver are already credited as successes, so it will
                // not necessarily fall below a peer with no history at all.
                if config.search.peer_reputation {
                    if let Some(peer) = furthest_peer(&stats).map(str::to_string) {
                        record_album_failure(db, &peer);
                    }
                }
                tracing::warn!(
                    "{artist} - {}: incomplete download ({reason}), skipping library placement",
                    album.unwrap_or("?")
                );
                discard_refused_staging(&album_staging, &downloaded);
                mark_album_processed_if_identifiable(db, artist, album, "failed")?;
                return Ok(AlbumOutcome::Failed {
                    reason: format!("incomplete download, library placement skipped: {reason}"),
                });
            }
            match organizer::place_into_library(
                &downloaded,
                root,
                &config.storage.organize_pattern,
                artist_dir,
                album.unwrap_or("Unknown"),
            ) {
                Ok(outcome) => {
                    // `place_into_library` reports only the files it wrote, so an
                    // empty list means every destination already held a file that
                    // parses as audio. The design contract for that state is that
                    // the album still counts as placed, because the library holds
                    // audio at every path placement would have written. Failing
                    // instead does not converge: the album folder the presence
                    // check looks for is still absent, so the next run downloads
                    // the album again and keeps the same files again, charging the
                    // download budget every time.
                    //
                    // Warn rather than stay silent, because any track whose
                    // incoming copy was not written is only reported per file at
                    // INFO while the staging copy is removed below.
                    if outcome.written.len() < downloaded.len() {
                        tracing::warn!(
                            "{artist} - {}: only {} of {} downloaded file(s) were written; the rest were kept because the destination already holds audio for them, from an earlier run or because another track or album of this artist maps onto the same path - check storage.organize_pattern distinguishes the tracks. The staging copy is removed",
                            album.unwrap_or("?"),
                            outcome.written.len(),
                            downloaded.len()
                        );
                    }
                    let track_count = downloaded.len();
                    return finish_library_write(
                        config,
                        db,
                        &album_staging,
                        artist,
                        album,
                        track_count,
                        DownloadDestination::Library(outcome.album_dir),
                    )
                    .await;
                }
                Err(e) => {
                    tracing::error!(
                        "{artist} - {}: library placement failed: {e}",
                        album.unwrap_or("?")
                    );
                    mark_album_processed_if_identifiable(db, artist, album, "failed")?;
                    return Ok(AlbumOutcome::Failed {
                        reason: format!("library placement failed: {e}"),
                    });
                }
            }
        }
        None | Some(LibraryTarget::StagingOnly) => {}
    }

    // Organize (if enabled)
    // Album folder the generic organize step wrote into. `None` means the step
    // did not run (organisation disabled, no configured library path, or a manual
    // run with nowhere to place), so the album is staying in staging.
    let mut library_album_dir: Option<PathBuf> = None;
    let mut organize_ok = true;
    // A manual run whose artist has no folder keeps its download in staging rather
    // than letting the organize step create the folder: manual runs place only into
    // an artist folder that already exists.
    let organize_allowed = !matches!(target, Some(LibraryTarget::StagingOnly));
    if organize_allowed && config.storage.organize && !config.library.paths.is_empty() {
        if let Some(reason) = library_write_refusal(&downloaded, config.filters.min_tracks) {
            // Same light peer demotion as the placement refusal above.
            if config.search.peer_reputation {
                if let Some(peer) = furthest_peer(&stats).map(str::to_string) {
                    record_album_failure(db, &peer);
                }
            }
            tracing::warn!(
                "{artist} - {}: incomplete download ({reason}), skipping organize",
                album.unwrap_or("?")
            );
            discard_refused_staging(&album_staging, &downloaded);
            mark_album_processed_if_identifiable(db, artist, album, "failed")?;
            return Ok(AlbumOutcome::Failed {
                reason: format!("incomplete download, organize skipped: {reason}"),
            });
        }
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
                Ok(outcome) => {
                    if library_album_dir.is_none() {
                        library_album_dir = Some(outcome.album_dir);
                    }
                }
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
    // Staging removal and the completion line now live in
    // `finish_library_write`, which removes staging only for a library
    // destination: when organisation did not run, staging *is* the album and
    // deleting it would destroy the download.
    if !organize_ok {
        mark_album_processed_if_identifiable(db, artist, album, "failed")?;
        return Ok(AlbumOutcome::Failed {
            reason: "download succeeded but file organization failed".into(),
        });
    }

    let destination = match library_album_dir {
        Some(album_dir) => DownloadDestination::Library(album_dir),
        None => DownloadDestination::Staging(album_staging.clone()),
    };
    let track_count = downloaded.len();
    finish_library_write(
        config,
        db,
        &album_staging,
        artist,
        album,
        track_count,
        destination,
    )
    .await
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

    // Arm cancellation before the scan: Ctrl+C must stop a slow scan, and a
    // first press during downloads still aborts them. The guard aborts the
    // listener on every return path, so a no-op cycle cannot leak one.
    let (cancel, _guard) = arm_cancellation();

    // The display is created before the scan, not after it: the scan owns the
    // first bar and the downloads own the rest, and one owner per run keeps a
    // single MultiProgress writing to stderr.
    let progress = if is_interactive() {
        Some(Arc::new(ProgressDisplay::new()))
    } else {
        None
    };

    // Scan library
    tracing::info!("Scanning library...");
    let Some(albums) = scan_library_cancellable(config, &cancel, progress.as_deref())? else {
        return Ok(());
    };
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

    // Shared cancellation flag, created by `arm_cancellation` before the scan.
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
        // Auto mode's copy-back is the upgrade path: it replaces an album that
        // exists but fails the quality gate, so it is gated on
        // `library_upgrade.enabled` and carries the library's own track count
        // as the completeness reference. With the flag off there is no target
        // and the generic organize path runs.
        let target = config
            .library_upgrade
            .enabled
            .then_some(LibraryTarget::Upgrade {
                root: library_path.as_path(),
                expected_tracks: library_track_count,
            });
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
                    target,
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

    // `_guard` aborts the listener as it drops. Each cycle of a scheduled run
    // calls this mode again, so the task must not outlive its run.
    Ok(())
}

fn processed_target_identity(artist: &str, album: &str) -> (String, String) {
    (
        search::artist_identity_key(artist),
        search::album_identity_key(album, artist),
    )
}

fn successful_processed_target(
    records: &[crate::db::ProcessedAlbum],
    artist: &str,
    album: &str,
) -> Option<(String, String)> {
    let (artist_identity, album_identity) = processed_target_identity(artist, album);
    records
        .iter()
        .find(|record| {
            record.status == "success"
                && !crate::discs::is_disc_designator(&record.album)
                && search::artist_identity_key(&record.artist) == artist_identity
                && search::album_identity_key(&record.album, artist) == album_identity
        })
        .map(|record| (record.artist.clone(), record.album.clone()))
}

/// Resolve an artist/album pair to an identity-equivalent spelling already
/// stored in history. Prefer a successful row so an older failed spelling
/// cannot hide proof that the release was already downloaded.
fn canonical_processed_target(
    records: &[crate::db::ProcessedAlbum],
    artist: &str,
    album: &str,
) -> (String, String) {
    if let Some(target) = successful_processed_target(records, artist, album) {
        return target;
    }
    let (artist_identity, album_identity) = processed_target_identity(artist, album);
    records
        .iter()
        .find(|record| {
            !(record.status == "success" && crate::discs::is_disc_designator(&record.album))
                && search::artist_identity_key(&record.artist) == artist_identity
                && search::album_identity_key(&record.album, artist) == album_identity
        })
        .map(|record| (record.artist.clone(), record.album.clone()))
        .unwrap_or_else(|| (artist.to_owned(), album.to_owned()))
}

/// Outcome of an artist-only manual run: per-album outcomes plus an optional
/// run-level notice (e.g. a visible automatic legacy fallback).
struct ArtistOnlyRun {
    outcomes: Vec<(String, AlbumOutcome)>,
    notice: Option<String>,
}

/// The artist folder a manual run should place into, or `None` when the artist
/// has none under a configured library path. Silent: the explanation for a
/// download that stayed in staging is logged where the outcome is known, so a run
/// with nothing to do does not claim its albums are in staging.
fn resolve_manual_target(config: &Config, artist: &str) -> Option<(PathBuf, String)> {
    // An album-only run names no artist: there is no folder to look for.
    if artist.trim().is_empty() {
        return None;
    }
    discover::resolve_artist_folder(config, artist)
}

/// Explain once per run that downloads stayed in staging because the artist has no
/// library folder. Called only when an album really was downloaded and left in
/// staging, so a run that skipped or failed every album stays quiet.
fn log_no_artist_folder(artist: &str, config: &Config, destination: &Option<(PathBuf, String)>) {
    // An album-only run never looked for an artist folder, so a line naming an empty
    // artist would explain nothing.
    if artist.trim().is_empty() {
        return;
    }
    if destination.is_none() && !config.library.paths.is_empty() {
        tracing::info!(
            "{artist}: no library folder found under the configured library paths; downloads stay in staging"
        );
    }
}

/// Whether an outcome is a completed download that stayed where it was staged.
fn stayed_in_staging(outcome: &AlbumOutcome) -> bool {
    matches!(
        outcome,
        AlbumOutcome::Downloaded {
            destination: DownloadDestination::Staging(_),
            ..
        }
    )
}

/// Manual placement's target: the resolved artist folder with the existing-album
/// rule, or [`LibraryTarget::StagingOnly`] when the artist has no library folder.
/// A manual run never creates an artist folder, so that case keeps the download
/// in staging and suppresses the generic organize step.
fn manual_place_target(destination: &Option<(PathBuf, String)>) -> LibraryTarget<'_> {
    match destination {
        Some((root, artist_dir)) => LibraryTarget::Place {
            root: root.as_path(),
            artist_dir: artist_dir.as_str(),
            skip_existing_album: true,
        },
        None => LibraryTarget::StagingOnly,
    }
}

/// Process a fixed list of album targets. Each work item carries the album
/// title and optional pre-searched Soulseek results: legacy grouping supplies
/// `Some(results)` (no new query, no search-history row) while authoritative
/// targets supply `None`, causing `process_album_internal` to run the normal
/// targeted search and search-history recording.
///
/// `target` is the library write every album uses: manual callers pass the
/// artist's existing folder with the existing-album rule, and `None` leaves the
/// completed album to the generic organize step.
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
    target: Option<LibraryTarget<'_>>,
) -> Result<Vec<(String, AlbumOutcome)>> {
    let mut outcomes = Vec::with_capacity(work.len());
    let processed_records = db.get_processed_albums()?;
    let mut successful_identities: std::collections::HashSet<(String, String)> = processed_records
        .iter()
        .filter(|record| {
            record.status == "success" && !crate::discs::is_disc_designator(&record.album)
        })
        .map(|record| processed_target_identity(&record.artist, &record.album))
        .collect();
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
        // An authoritative target keeps its MusicBrainz title for searching,
        // but an identity-equivalent legacy success still proves it is already
        // processed.
        if presearched.is_none()
            && !ignore_processed
            && successful_identities.contains(&processed_target_identity(artist, &album_title))
        {
            outcomes.push((album_title, AlbumOutcome::Skipped));
            continue;
        }
        // Historical spellings are safe only for grouped legacy results. An
        // authoritative MusicBrainz title must remain the search/gate title.
        let (process_artist, process_album) = if presearched.is_some() {
            canonical_processed_target(&processed_records, artist, &album_title)
        } else {
            (artist.to_owned(), album_title.clone())
        };
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
            target,
            presearched,
        )
        .await?;
        let cancelled = matches!(
            &result,
            AlbumOutcome::Failed { reason } if reason.contains("cancelled by user")
        );
        if matches!(result, AlbumOutcome::Downloaded { .. }) {
            successful_identities.insert(processed_target_identity(artist, &album_title));
        }
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
        // Artist-only results are handed to each album, and every one of those is a
        // new album (there is no upgrade target in this path).
        filter::TrackOneAnchor::Required,
    )
    .await?;
    let duration_ms = search_start.elapsed().as_millis() as u64;
    if let Err(e) = search::record_search(artist, None, outcome.results.len(), duration_ms, db) {
        tracing::warn!("{artist} — (all): failed to record search history: {e}");
    }

    // Skip albums the library already holds, exactly as the authoritative path
    // does. The documented promise is that --artist X fetches only what is
    // missing, and this heuristic path is both the explicit opt-out and the
    // automatic fallback during a MusicBrainz outage.
    let index = match with_scan_indicator(progress, |walk_progress| {
        discover::index_from_paths(
            &config.library.paths,
            &config.filters,
            Some(cancel),
            walk_progress,
        )
    }) {
        Ok(index) => index,
        // A user cancellation is not a scan failure: stop the run rather than
        // warn about a broken library and drop the presence check.
        Err(SeakarrError::Cancelled) => {
            tracing::info!("{artist}: library scan cancelled by user — stopping before any album");
            return Ok(ArtistOnlyRun {
                outcomes: Vec::new(),
                notice: Some(format!("{artist}: library scan cancelled by user")),
            });
        }
        Err(error) => {
            tracing::warn!(
                "{artist}: library scan failed ({error}); skipping the already-present check"
            );
            discover::LibraryIndex::default()
        }
    };

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

    let present = albums
        .iter()
        .filter(|album| index.contains_album(artist, &album.album))
        .count();
    if present > 0 && present == albums.len() {
        return Ok(ArtistOnlyRun {
            outcomes: Vec::new(),
            notice: Some(format!(
                "Artist-only run: all {present} eligible album(s) already present"
            )),
        });
    }
    let notice = if present > 0 {
        Some(format!(
            "Artist-only run: {present} album(s) already present; skipped"
        ))
    } else {
        None
    };

    let work = albums
        .into_iter()
        .filter(|album| !index.contains_album(artist, &album.album))
        .map(|album| (album.album, Some(album.results)))
        .collect();
    let destination = resolve_manual_target(config, artist);
    let target = Some(manual_place_target(&destination));
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
        target,
    )
    .await?;
    if outcomes
        .iter()
        .any(|(_, outcome)| stayed_in_staging(outcome))
    {
        log_no_artist_folder(artist, config, &destination);
    }
    Ok(ArtistOnlyRun { outcomes, notice })
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
    match discover_artist_albums(
        provider,
        db,
        artist,
        &config.discography,
        FailureCacheUse::Bypass,
    )
    .await
    {
        DiscoveryOutcome::Authoritative { albums, provenance } => {
            if let DiscoveryProvenance::StaleCache {
                age_days,
                refresh_error,
                ..
            } = &provenance
            {
                tracing::warn!(
                    "{artist}: discography cache is {age_days} day(s) old and refresh failed ({refresh_error}); using stale cache"
                );
            }
            // A failing scan must not break artist-only manual mode, which
            // worked without a library before: warn and filter nothing.
            let index = match with_scan_indicator(progress, |walk_progress| {
                discover::index_from_paths(
                    &config.library.paths,
                    &config.filters,
                    Some(cancel),
                    walk_progress,
                )
            }) {
                Ok(index) => index,
                // As on the legacy path: a cancelled scan stops the run instead
                // of warning about a failure and continuing without a presence
                // check (which would treat every album as missing).
                Err(SeakarrError::Cancelled) => {
                    tracing::info!(
                        "{artist}: library scan cancelled by user — stopping before any album"
                    );
                    return Ok(ArtistOnlyRun {
                        outcomes: Vec::new(),
                        notice: Some(format!("{artist}: library scan cancelled by user")),
                    });
                }
                Err(error) => {
                    tracing::warn!(
                        "{artist}: library scan failed ({error}); skipping the already-present check"
                    );
                    discover::LibraryIndex::default()
                }
            };
            let missing = discover::missing_albums(&index, artist, &albums);
            let present = albums.len() - missing.len();
            if present > 0 && missing.is_empty() {
                return Ok(ArtistOnlyRun {
                    outcomes: Vec::new(),
                    notice: Some(format!(
                        "Artist-only run: all {present} eligible album(s) already present"
                    )),
                });
            }
            let notice = if present > 0 {
                Some(format!(
                    "Artist-only run: {present} album(s) already present; skipped"
                ))
            } else {
                None
            };
            let work = missing
                .into_iter()
                .map(|album| (album.title, None))
                .collect();
            let destination = resolve_manual_target(config, artist);
            let target = Some(manual_place_target(&destination));
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
                target,
            )
            .await?;
            if outcomes
                .iter()
                .any(|(_, outcome)| stayed_in_staging(outcome))
            {
                log_no_artist_folder(artist, config, &destination);
            }
            Ok(ArtistOnlyRun { outcomes, notice })
        }
        DiscoveryOutcome::AuthoritativeEmpty { provenance } => {
            if let DiscoveryProvenance::StaleCache {
                age_days,
                refresh_error,
                ..
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
        DiscoveryOutcome::LegacyFallback { reason, .. } => {
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
                notice: Some(match run.notice {
                    // Keep the legacy run's own notice, for example "all albums
                    // already present": replacing it would hide why the run did
                    // no work.
                    Some(notice) => format!(
                        "Authoritative discography unavailable: {reason}; album names were discovered heuristically from Soulseek folders; {notice}"
                    ),
                    None => format!(
                        "Authoritative discography unavailable: {reason}; album names were discovered heuristically from Soulseek folders"
                    ),
                }),
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
                notice: Some(match run.notice {
                    // Keep the legacy run's own notice, as in the LegacyFallback
                    // arm: dropping it would hide why the run did no work.
                    Some(notice) => format!(
                        "Authoritative discography unavailable: {error}; album names were discovered heuristically from Soulseek folders; {notice}"
                    ),
                    None => format!(
                        "Authoritative discography unavailable: {error}; album names were discovered heuristically from Soulseek folders"
                    ),
                }),
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

/// Whether a completed album attempt consumes one unit of the discover
/// download budget.
///
/// Only work that reached the download stage is charged: a completed transfer,
/// or a transfer that was attempted and failed. Everything else is free, and
/// deliberately so. An album skipped because a success record already exists,
/// or whose search produced no admissible candidate, performed no download; if
/// such albums charged the budget, a fixed prefix of the work list would spend
/// every run's allowance and no download would ever start.
/// This helper covers the `Ok` outcomes only; `charges_after_error` covers the
/// `Err` case.
fn charges_download_budget(outcome: &AlbumOutcome) -> bool {
    matches!(
        outcome,
        AlbumOutcome::Downloaded { .. } | AlbumOutcome::Failed { .. }
    )
}

/// Whether an error returned by `process_album` means the download stage was
/// reached and must therefore be charged.
///
/// Only database failures qualify. Inside `process_album` the database writes sit
/// around a transfer — recording its success or its failure — so a transfer that
/// happened surfaces here as a database error, including when the post-download
/// write is what failed. Every other error class comes from the search stage,
/// raised before any transfer was attempted; charging those would let a run whose
/// searches keep failing spend its whole allowance without attempting a download.
///
/// The converse does not hold: a database failure can also be raised before any
/// transfer, by the processed-record check or the `--ignore-processed` deletion
/// when the database itself is locked or corrupt. That case is charged too. The
/// two cannot be told apart without stage-marking every pipeline error, and the
/// direction of the resulting error is the safe one: a broken database stops the
/// run at its cap instead of letting it exceed the cap.
fn charges_after_error(error: &SeakarrError) -> bool {
    matches!(error, SeakarrError::Database(_))
}

/// Report a discover run that the provider circuit breaker aborted, and build
/// the error to return.
fn abort_discover_run(
    report: &mut RunReport,
    counters: &discover::DiscoverCounters,
    progress: Option<&ProgressDisplay>,
) -> SeakarrError {
    for notice in discover::discover_notices(counters) {
        report.add_notice(notice);
    }
    if let Some(display) = progress {
        display.clear();
    }
    report.print_summary();
    SeakarrError::MusicBrainz(format!(
        "{DISCOVER_PROVIDER_FAILURE_LIMIT} consecutive MusicBrainz failures; aborting discover run"
    ))
}

/// Consecutive provider failures tolerated before a discover run aborts.
const DISCOVER_PROVIDER_FAILURE_LIMIT: u32 = 3;

/// Run discover mode: derive artists from the library, ask MusicBrainz what
/// each one is missing, and download only those albums.
///
/// Auto mode's upgrade pass is untouched: this mode never replaces an album it
/// considers present.
pub async fn run_discover_mode(
    client: &dyn SoulseekClient,
    config: &Config,
    db: &Database,
    artist_filter: Option<&str>,
    ignore_processed: bool,
) -> Result<()> {
    if !config.discography.enabled {
        return Err(SeakarrError::Config(
            "discover mode requires discography.enabled: true".into(),
        ));
    }
    if config.library.paths.is_empty() {
        return Err(SeakarrError::Config(
            "library.paths is empty - discover mode needs a library to derive artists from".into(),
        ));
    }
    let provider = MusicBrainzProvider::new().map_err(|error| {
        SeakarrError::MusicBrainz(format!("could not construct the provider: {error}"))
    })?;
    run_discover_mode_with_provider(
        client,
        config,
        db,
        artist_filter,
        ignore_processed,
        Path::new(&config.storage.staging_dir),
        &provider,
    )
    .await
}

/// Timeline-free core of discover mode, with the provider injected.
#[allow(clippy::too_many_arguments)]
async fn run_discover_mode_with_provider(
    client: &dyn SoulseekClient,
    config: &Config,
    db: &Database,
    artist_filter: Option<&str>,
    ignore_processed: bool,
    staging_dir: &Path,
    provider: &dyn DiscographyProvider,
) -> Result<()> {
    if !config.discography.enabled {
        return Err(SeakarrError::Config(
            "discover mode requires discography.enabled: true".into(),
        ));
    }
    if config.library.paths.is_empty() {
        return Err(SeakarrError::Config(
            "library.paths is empty - discover mode needs a library to derive artists from".into(),
        ));
    }

    // Discover reads the whole library before it can pick a single work item, so
    // cancellation is armed first: interrupting a slow scan must be possible
    // without killing the process and stranding the PID lock. The guard aborts
    // the listener on every return path.
    let (cancel, _guard) = arm_cancellation();

    // Created before the scan so the scan's indicator and the later downloads
    // share one display, and so a headless run creates none at all.
    let progress = if is_interactive() {
        Some(ProgressDisplay::new())
    } else {
        None
    };

    let Some(scanned) = scan_library_cancellable(config, &cancel, progress.as_ref())? else {
        return Ok(());
    };
    let index = discover::build_index(&scanned);
    let selection =
        discover::select_artists(&index, &config.discover.exclude_artists, artist_filter)?;

    // An explicit --artist is a deliberate one-off request, so it bypasses a
    // recorded failure; the scheduled sweep honours it. This matches the
    // existing rule that --artist overrides discover.exclude_artists.
    let failure_cache = if artist_filter.is_some() {
        FailureCacheUse::Bypass
    } else {
        FailureCacheUse::Honour
    };

    let mut counters = discover::DiscoverCounters {
        excluded: selection.excluded.len(),
        no_folder: selection.no_folder.len(),
        budget_limit: config.discover.max_cycle_downloads,
        artists_total: selection.artists.len(),
        ..discover::DiscoverCounters::default()
    };
    let mut budget = discover::DownloadBudget::new(config.discover.max_cycle_downloads);
    let mut report = RunReport::new();
    let mut consecutive_provider_failures: u32 = 0;

    for artist in &selection.artists {
        if budget.exhausted() {
            // Only the first write names the artist where the budget ran out;
            // a later artist was never examined, so it must not rename it.
            counters
                .budget_reached_at
                .get_or_insert_with(|| artist.name.clone());
            break;
        }
        if cancel.load(Ordering::SeqCst) {
            // Cancellation is not a budget stop. Conflating the two would print
            // a false "download budget reached" notice for a run the user
            // interrupted.
            break;
        }
        // Where this artist's completed albums belong: the directory the
        // artist's existing albums were scanned from, with the folder name that
        // is actually on disk. Placement is unconditional in discover mode, so
        // it does not consult `storage.organize` or `library_upgrade.enabled`.
        let placement = LibraryTarget::Place {
            root: artist.library_root.as_path(),
            artist_dir: artist.artist_dir.as_str(),
            skip_existing_album: false,
        };
        counters.artists_examined += 1;
        match discover_artist_albums(
            provider,
            db,
            &artist.name,
            &config.discography,
            failure_cache,
        )
        .await
        {
            DiscoveryOutcome::Authoritative { albums, provenance } => {
                if let DiscoveryProvenance::StaleCache {
                    age_days,
                    refresh_error,
                    kind,
                } = &provenance
                {
                    tracing::warn!(
                        "{}: discography cache is {age_days} day(s) old and refresh failed ({refresh_error}); using stale cache",
                        artist.name
                    );
                    // Only a genuine outage counts. A refresh that failed because
                    // the artist no longer resolves means MusicBrainz answered and
                    // the cached work list below is still usable, so the counter
                    // resets rather than aborting a run over a name problem.
                    if *kind == DiscoveryFailure::Provider {
                        consecutive_provider_failures += 1;
                        if consecutive_provider_failures >= DISCOVER_PROVIDER_FAILURE_LIMIT {
                            return Err(abort_discover_run(
                                &mut report,
                                &counters,
                                progress.as_ref(),
                            ));
                        }
                    } else {
                        consecutive_provider_failures = 0;
                    }
                } else {
                    consecutive_provider_failures = 0;
                }
                let missing = discover::missing_albums(&index, &artist.name, &albums);
                counters.present += albums.len() - missing.len();
                for target in missing {
                    if budget.exhausted() {
                        counters
                            .budget_reached_at
                            .get_or_insert_with(|| artist.name.clone());
                        break;
                    }
                    if cancel.load(Ordering::SeqCst) {
                        break;
                    }
                    let result = process_album(
                        client,
                        &artist.name,
                        Some(&target.title),
                        ignore_processed,
                        config,
                        db,
                        staging_dir,
                        progress.as_ref(),
                        Some(&cancel),
                        None,
                        Some(placement),
                    )
                    .await;
                    match result {
                        Ok(outcome) => {
                            if charges_download_budget(&outcome) {
                                budget.charge();
                            }
                            report.record(&artist.name, &target.title, outcome);
                        }
                        Err(error) => {
                            // Matches auto mode: an environment error is
                            // recorded and the run continues to the next album.
                            tracing::error!(
                                "Album processing failed: {} - {}: {error}",
                                artist.name,
                                target.title
                            );
                            // An error can arrive after a completed download,
                            // because the post-download bookkeeping writes can
                            // fail. `charges_after_error` draws that line from
                            // the error class rather than from an index the run
                            // has already mutated: the snapshot taken before the
                            // run cannot see albums organised during it.
                            if charges_after_error(&error) {
                                budget.charge();
                            }
                            report.record(
                                &artist.name,
                                &target.title,
                                AlbumOutcome::Failed {
                                    reason: error.to_string(),
                                },
                            );
                        }
                    }
                    if budget.exhausted() {
                        counters
                            .budget_reached_at
                            .get_or_insert_with(|| artist.name.clone());
                        break;
                    }
                }
            }
            DiscoveryOutcome::AuthoritativeEmpty { provenance } => {
                if let DiscoveryProvenance::StaleCache {
                    age_days,
                    refresh_error,
                    kind,
                } = &provenance
                {
                    tracing::warn!(
                        "{}: discography cache is {age_days} day(s) old and refresh failed ({refresh_error}); no eligible authoritative albums",
                        artist.name
                    );
                    if *kind == DiscoveryFailure::Provider {
                        consecutive_provider_failures += 1;
                        if consecutive_provider_failures >= DISCOVER_PROVIDER_FAILURE_LIMIT {
                            return Err(abort_discover_run(
                                &mut report,
                                &counters,
                                progress.as_ref(),
                            ));
                        }
                    } else {
                        consecutive_provider_failures = 0;
                    }
                } else {
                    consecutive_provider_failures = 0;
                }
                counters.no_eligible_albums += 1;
            }
            DiscoveryOutcome::LegacyFallback {
                reason,
                kind,
                from_cache,
            } => match kind {
                DiscoveryFailure::Unresolved => {
                    if from_cache {
                        // No request was made, so this is no evidence about
                        // provider health: neither increment nor reset the
                        // consecutive-failure counter.
                        tracing::debug!(
                            "{}: skipped, resolution failure recorded earlier ({reason})",
                            artist.name
                        );
                        counters.cached_failures.push(artist.name.clone());
                    } else {
                        // MusicBrainz answered for this artist, so the provider
                        // is reachable. Reset the consecutive-failure count and
                        // keep the circuit breaker for genuine outages only.
                        consecutive_provider_failures = 0;
                        tracing::warn!(
                            "{}: artist could not be resolved on MusicBrainz: {reason}",
                            artist.name
                        );
                        counters.unresolved.push(artist.name.clone());
                    }
                }
                DiscoveryFailure::Provider => {
                    consecutive_provider_failures += 1;
                    tracing::warn!(
                        "{}: MusicBrainz unavailable ({reason}); artist skipped",
                        artist.name
                    );
                    counters.provider_failed.push((artist.name.clone(), reason));
                    if consecutive_provider_failures >= DISCOVER_PROVIDER_FAILURE_LIMIT {
                        return Err(abort_discover_run(
                            &mut report,
                            &counters,
                            progress.as_ref(),
                        ));
                    }
                }
            },
        }
    }

    for notice in discover::discover_notices(&counters) {
        report.add_notice(notice);
    }
    if let Some(ref display) = progress {
        display.clear();
    }
    report.print_summary();
    // `_guard` aborts the listener as it drops; a scheduled cycle must not leak.
    Ok(())
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
    // Cancellation flag: SIGINT aborts the in-flight download (download_album
    // cleans the album's staging dir) or the library scan a sub-mode runs. The
    // guard aborts the listener on every return path.
    let (cancel, _guard) = arm_cancellation();

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
        let destination = resolve_manual_target(config, artist_name);
        let target = Some(manual_place_target(&destination));
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
            target,
        )
        .await;
        if result.as_ref().is_ok_and(stayed_in_staging) {
            log_no_artist_folder(artist_name, config, &destination);
        }
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
    // `_guard` aborts the listener as it drops.
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{FileInfo, MockClient, SearchResult};
    use crate::config::Config;
    use crate::db::Database;
    use crate::test_support::{make_file, write_minimal_flac, write_minimal_flac_with_tags};
    use std::sync::Arc;
    use tempfile::TempDir;

    #[tokio::test]
    async fn discover_mode_requires_its_flag_and_a_library() {
        // Two configuration guards that must fail before any provider call: the
        // mode cannot derive artists without a library, and it needs the
        // discography setting that gates release-group resolution.
        let client = MockClient::new();
        let db = Database::open_in_memory().unwrap();

        let mut disabled = make_test_config();
        disabled.discography.enabled = false;
        disabled.library.paths = vec!["/media/Music".into()];
        let error = run_discover_mode(&client, &disabled, &db, None, false)
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("discography.enabled"),
            "got {error:?}"
        );

        let mut empty_library = make_test_config();
        empty_library.discography.enabled = true;
        empty_library.library.paths.clear();
        let error = run_discover_mode(&client, &empty_library, &db, None, false)
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("library.paths is empty"),
            "got {error:?}"
        );
    }

    #[test]
    fn scan_shows_and_releases_exactly_one_indicator_bar() {
        // The scan's indicator is the run's first bar, and the download bars
        // come later: one created, one released, so the terminal is free when
        // the downloads start.
        let dir = TempDir::new().unwrap();
        let album_dir = dir.path().join("Artist").join("Album");
        std::fs::create_dir_all(&album_dir).unwrap();
        write_minimal_flac_with_tags(&album_dir.join("01 - track.flac"), "Artist", "Album");
        let (mut config, _db, _staging) = artist_only_fixture();
        config.library.paths = vec![dir.path().to_string_lossy().into_owned()];
        let cancel = Arc::new(AtomicBool::new(false));
        let display = ProgressDisplay::new();

        let outcome = scan_library_cancellable(&config, &cancel, Some(&display)).unwrap();

        assert!(outcome.is_some(), "the scan must still return the library");
        assert_eq!(display.scan_bars_created(), 1);
        assert_eq!(display.scan_bars_finished(), 1);
        assert_eq!(
            display.created_bars(),
            0,
            "the scan must not create a transfer bar"
        );
        // Created and released alone would also hold if the bar were merely
        // built and dropped without the walk ever driving it, so assert the
        // walk reported through the port: the start update and the final one.
        assert!(
            display.scan_bars_updated() >= 2,
            "the walk must drive the indicator, not merely have one created"
        );
    }

    #[test]
    fn a_headless_scan_creates_no_indicator_bar() {
        // Nothing to observe without a display, so this pins the contract that
        // matters: with no display the scan still works and returns the library
        // without touching the terminal.
        let dir = TempDir::new().unwrap();
        let album_dir = dir.path().join("Artist").join("Album");
        std::fs::create_dir_all(&album_dir).unwrap();
        write_minimal_flac_with_tags(&album_dir.join("01 - track.flac"), "Artist", "Album");
        let (mut config, _db, _staging) = artist_only_fixture();
        config.library.paths = vec![dir.path().to_string_lossy().into_owned()];
        let cancel = Arc::new(AtomicBool::new(false));

        let outcome = scan_library_cancellable(&config, &cancel, None).unwrap();

        let albums = outcome.expect("a headless scan must return the library");
        assert_eq!(albums.len(), 1);
    }

    #[test]
    fn a_cancelled_scan_still_releases_its_indicator_bar() {
        // Cancellation is the path most likely to strand a bar, and a stranded
        // scan bar would sit on the terminal for the rest of the run. The walk
        // releases before it returns `Cancelled`, and `Drop` covers the rest.
        let dir = TempDir::new().unwrap();
        let album_dir = dir.path().join("Artist").join("Album");
        std::fs::create_dir_all(&album_dir).unwrap();
        write_minimal_flac_with_tags(&album_dir.join("01 - track.flac"), "Artist", "Album");
        let (mut config, _db, _staging) = artist_only_fixture();
        config.library.paths = vec![dir.path().to_string_lossy().into_owned()];
        let cancel = Arc::new(AtomicBool::new(true));
        let display = ProgressDisplay::new();

        let outcome = scan_library_cancellable(&config, &cancel, Some(&display)).unwrap();

        assert!(outcome.is_none(), "a cancelled scan reports no albums");
        assert_eq!(display.scan_bars_created(), 1);
        assert_eq!(
            display.scan_bars_finished(),
            display.scan_bars_created(),
            "every created scan bar must be released"
        );
    }

    #[test]
    fn a_cancelled_scan_reports_nothing_to_do() {
        // The seam both runners use. A cancelled scan must come back as "no work"
        // rather than as a completed scan: an empty scan would look like a library
        // with nothing in it, and discover would then treat every album as a gap.
        let dir = TempDir::new().unwrap();
        let album_dir = dir.path().join("Artist").join("Album");
        std::fs::create_dir_all(&album_dir).unwrap();
        write_minimal_flac_with_tags(&album_dir.join("01 - track.flac"), "Artist", "Album");
        let (mut config, _db, _staging) = artist_only_fixture();
        config.library.paths = vec![dir.path().to_string_lossy().into_owned()];
        let cancel = Arc::new(AtomicBool::new(true));
        let capture = crate::test_support::LogCapture::start();

        let outcome = scan_library_cancellable(&config, &cancel, None).unwrap();

        assert!(
            outcome.is_none(),
            "a cancelled scan must report no albums, got {outcome:?}"
        );
        // The distinctive tail of the runner's own mapping message: the scanner
        // logs "Library scan cancelled by user ..." too, so asserting on the
        // shared prefix would leave the mapping arm untested.
        assert!(
            capture.text().contains("aborting before any work item"),
            "the runner must report the abort it performs, got:\n{}",
            capture.text()
        );
    }

    #[test]
    fn an_uncancelled_scan_still_returns_the_library() {
        let dir = TempDir::new().unwrap();
        let album_dir = dir.path().join("Artist").join("Album");
        std::fs::create_dir_all(&album_dir).unwrap();
        write_minimal_flac_with_tags(&album_dir.join("01 - track.flac"), "Artist", "Album");
        let (mut config, _db, _staging) = artist_only_fixture();
        config.library.paths = vec![dir.path().to_string_lossy().into_owned()];
        let cancel = Arc::new(AtomicBool::new(false));

        let outcome = scan_library_cancellable(&config, &cancel, None).unwrap();

        let albums = outcome.expect("an uncancelled scan must return the library");
        assert_eq!(albums.len(), 1);
        assert_eq!(albums[0].artist, "Artist");
    }

    #[tokio::test]
    async fn dropping_the_cancellation_guard_aborts_its_listener() {
        // A scheduled loop calls a mode once per cycle. Dropping a JoinHandle
        // only detaches the task, so a guard that forgot to abort would leave one
        // live signal listener per cycle; the marker proves the task was dropped.
        struct AbortMarker(Arc<AtomicBool>);
        impl Drop for AbortMarker {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let aborted = Arc::new(AtomicBool::new(false));
        let handle = tokio::spawn({
            let marker = AbortMarker(Arc::clone(&aborted));
            async move {
                let _marker = marker;
                std::future::pending::<()>().await;
            }
        });
        tokio::task::yield_now().await;
        assert!(
            !aborted.load(Ordering::SeqCst),
            "the task must be running first"
        );

        let guard = CancellationGuard { handle };
        drop(guard);
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        assert!(
            aborted.load(Ordering::SeqCst),
            "dropping the guard must abort the listener, not detach it"
        );
    }

    #[tokio::test]
    async fn a_cancelled_scan_stops_a_legacy_artist_only_run() {
        // The legacy path (discography disabled, or the provider outage
        // fallback) scans the library for the same presence check, so its
        // Cancelled arm needs the same coverage: without it a cancellation is
        // logged as a scan *failure* and every album is treated as missing.
        let soulseek = MockClient::new();
        search_index(
            &soulseek,
            "Test Artist",
            &[("Test Artist Old", "Old"), ("Test Artist New", "New")],
        );
        let (mut config, db, staging) = artist_only_fixture();
        let library = TempDir::new().unwrap();
        let present = library.path().join("Test Artist").join("Old");
        std::fs::create_dir_all(&present).unwrap();
        write_minimal_flac_with_tags(&present.join("01 - track.flac"), "Test Artist", "Old");
        config.library.paths = vec![library.path().to_string_lossy().into_owned()];
        let cancel = Arc::new(AtomicBool::new(true));
        let _capture = crate::test_support::LogCapture::start();

        let run = run_legacy_artist_only_mode(
            &soulseek,
            "Test Artist",
            false,
            &config,
            &db,
            staging.path(),
            None,
            &cancel,
        )
        .await
        .unwrap();

        assert!(
            run.outcomes.is_empty(),
            "a cancelled scan must not process albums on the legacy path: {:?}",
            run.outcomes
        );
        assert!(
            run.notice
                .as_deref()
                .is_some_and(|notice| notice.contains("cancelled")),
            "the cancellation must be reported, got {:?}",
            run.notice
        );
    }

    #[tokio::test]
    async fn an_artist_only_run_shows_and_releases_one_indicator_bar() {
        // The legacy artist-only path scans the library for its presence check
        // through `discover::index_from_paths`, which is the third library scan
        // in the program. It must use the same indicator as the other two, or
        // the same long walk feels silent on `--artist` runs.
        let soulseek = MockClient::new();
        search_index(
            &soulseek,
            "Test Artist",
            &[("Test Artist Old", "Old"), ("Test Artist New", "New")],
        );
        let (mut config, db, staging) = artist_only_fixture();
        let library = TempDir::new().unwrap();
        let present = library.path().join("Test Artist").join("Old");
        std::fs::create_dir_all(&present).unwrap();
        write_minimal_flac_with_tags(&present.join("01 - track.flac"), "Test Artist", "Old");
        config.library.paths = vec![library.path().to_string_lossy().into_owned()];
        // Cancelled so the run stops at the scan: this test is about the
        // indicator's lifecycle, and the cancelled walk is the deterministic
        // exit that most easily strands a bar.
        let cancel = Arc::new(AtomicBool::new(true));
        let display = ProgressDisplay::new();
        let _capture = crate::test_support::LogCapture::start();

        let run = run_legacy_artist_only_mode(
            &soulseek,
            "Test Artist",
            false,
            &config,
            &db,
            staging.path(),
            Some(&display),
            &cancel,
        )
        .await
        .unwrap();

        assert!(run.outcomes.is_empty());
        assert_eq!(
            display.scan_bars_created(),
            1,
            "the artist-only library scan must show the scan indicator"
        );
        assert_eq!(
            display.scan_bars_finished(),
            1,
            "the indicator must be released when the scan stops"
        );
        assert_eq!(
            display.created_bars(),
            0,
            "a library scan must not create a transfer bar"
        );
    }

    #[tokio::test]
    async fn a_cancelled_scan_stops_an_artist_only_run_without_processing_an_album() {
        // Ctrl+C during the artist-only presence scan used to be reported as a
        // scan *failure* and then silently dropped the presence check, so every
        // album looked missing. A cancellation must stop the run instead.
        let soulseek = MockClient::new();
        search_index(
            &soulseek,
            "Test Artist",
            &[("Test Artist Old", "Old"), ("Test Artist New", "New")],
        );
        let provider = FakeDiscographyProvider::with_groups(vec![
            release_group("old", "Old", "1999"),
            release_group("new", "New", "2005"),
        ]);
        let (mut config, db, staging) = artist_only_fixture();
        let library = TempDir::new().unwrap();
        let present = library.path().join("Test Artist").join("Old");
        std::fs::create_dir_all(&present).unwrap();
        write_minimal_flac_with_tags(&present.join("01 - track.flac"), "Test Artist", "Old");
        config.library.paths = vec![library.path().to_string_lossy().into_owned()];
        let cancel = Arc::new(AtomicBool::new(true));
        // Hold the capture window so this test's own cancelled-walk line cannot
        // land in another test's window and satisfy its assertion.
        let _capture = crate::test_support::LogCapture::start();

        let run = run_artist_only_mode_with_provider(
            &soulseek,
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

        assert!(
            run.outcomes.is_empty(),
            "a cancelled scan must not process albums: {:?}",
            run.outcomes
        );
        assert!(
            run.notice
                .as_deref()
                .is_some_and(|notice| notice.contains("cancelled")),
            "the cancellation must be reported, got {:?}",
            run.notice
        );
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
        // A DISC-TRACK stem: the track number is the second token, and it must
        // not survive into the title, or the name would read "11 - 11 - ...".
        assert_eq!(
            organizer::organize_name_from_stem("1-11 - Steel Bars"),
            ("11".to_string(), "Steel Bars".to_string())
        );
        assert_eq!(
            organizer::organize_name_from_stem("2-03 - Title"),
            ("03".to_string(), "Title".to_string())
        );
        // A title that merely begins with its own track number keeps that word:
        // the DISC-TRACK form is recognised by the hyphen, not by the value.
        assert_eq!(
            organizer::organize_name_from_stem("01 - 1 Thing"),
            ("01".to_string(), "1 Thing".to_string())
        );
        assert_eq!(
            organizer::organize_name_from_stem("07 - 7 Seconds"),
            ("07".to_string(), "7 Seconds".to_string())
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
        assert!(
            matches!(
                result.unwrap(),
                AlbumOutcome::Downloaded { track_count: 1, .. }
            ),
            "the album must complete with its downloaded track count"
        );
    }

    #[tokio::test]
    async fn completion_line_names_the_library_album_folder() {
        // Distinctive artist and album names: `LogCapture` keeps one process-wide
        // window, so a neighbouring test's completion line can land in this
        // buffer. Naming this fixture uniquely lets the assertion select its own
        // line instead of whichever line happens to come first.
        let client = Arc::new(MockClient::new());
        *client.search_results.lock().unwrap() = vec![SearchResult {
            username: "peer".into(),
            speed: 1000,
            slots: 1,
            files: vec![make_file(
                r"Completion Fixture Artist\Completion Fixture Album\01 - track.flac",
                900,
                10_000_000,
            )],
        }];
        // Real bytes so the organizer has something to move.
        *client.write_files.lock().unwrap() = true;
        let db = Database::open_in_memory().unwrap();
        let staging = TempDir::new().unwrap();
        let library = TempDir::new().unwrap();

        let mut config = make_test_config();
        config.storage.organize = true;
        config.storage.organize_pattern = "%artist%/%album%/%track% - %title%.%ext%".into();
        config.library.paths = vec![library.path().to_string_lossy().to_string()];

        let capture = crate::test_support::LogCapture::start();
        let result = process_album(
            client.as_ref() as &dyn crate::client::SoulseekClient,
            "Completion Fixture Artist",
            Some("Completion Fixture Album"),
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

        let expected = library
            .path()
            .join("Completion Fixture Artist/Completion Fixture Album")
            .display()
            .to_string();
        assert_eq!(
            result,
            AlbumOutcome::Downloaded {
                track_count: 1,
                destination: DownloadDestination::Library(PathBuf::from(expected.clone())),
            }
        );

        let logs = capture.text();
        let line = logs
            .lines()
            .find(|line| line.contains("Completed: Completion Fixture Artist"))
            .unwrap_or_else(|| panic!("no completion line for this fixture, got:\n{logs}"));
        assert!(
            line.contains(&expected),
            "the completion line must name the album folder, got: {line}"
        );
        assert!(
            !line.contains("(kept in staging)"),
            "a library write must not be reported as staging, got: {line}"
        );
        // Key on this fixture's own destination: the organizer unit tests emit the
        // same bare `Organized:` prefix concurrently under `LogCapture`'s single
        // process-wide window, so a bare-prefix assertion could pass without the
        // generic organize path emitting anything at all.
        assert!(
            logs.lines()
                .any(|line| line.contains("Organized:") && line.contains(&expected)),
            "the organize path must report this album's per-file destination at DEBUG, got:\n{logs}"
        );
    }

    #[tokio::test]
    async fn the_success_notification_names_the_destination() {
        // `notify_success` is reached only from `finish_library_write`, with
        // `destination.render()`. Every other runner test leaves
        // `notifications.urls` empty, so a wrong argument at that one call site
        // would leave the whole suite green.
        let mock_server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .mount(&mock_server)
            .await;

        let client = Arc::new(MockClient::new());
        *client.search_results.lock().unwrap() = vec![SearchResult {
            username: "peer".into(),
            speed: 1000,
            slots: 1,
            files: vec![make_file(
                r"Notify Fixture Artist\Notify Fixture Album\01 - track.flac",
                900,
                10_000_000,
            )],
        }];
        *client.write_files.lock().unwrap() = true;
        let db = Database::open_in_memory().unwrap();
        let staging = TempDir::new().unwrap();
        let library = TempDir::new().unwrap();

        let mut config = make_test_config();
        config.storage.organize = true;
        config.storage.organize_pattern = "%artist%/%album%/%track% - %title%.%ext%".into();
        config.library.paths = vec![library.path().to_string_lossy().to_string()];
        config.notifications.urls = vec![format!("{}/notify", mock_server.uri())];

        process_album(
            client.as_ref() as &dyn crate::client::SoulseekClient,
            "Notify Fixture Artist",
            Some("Notify Fixture Album"),
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

        let requests = mock_server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1, "exactly one success notification");
        let body: serde_json::Value =
            serde_json::from_slice(&requests[0].body).expect("the payload is JSON");
        let message = body["message"].as_str().expect("a message string");
        let expected = library
            .path()
            .join("Notify Fixture Artist/Notify Fixture Album")
            .display()
            .to_string();
        assert!(
            message.ends_with(&expected),
            "the notification must name the destination, got: {message}"
        );
    }

    #[tokio::test]
    async fn a_short_album_group_is_refused_before_the_download() {
        // The shape that produced the reported staging leftovers: a peer's result
        // spans several album directories, so it counts enough files overall, but
        // the album group that would actually be downloaded holds a single track.
        // Counting the whole result let it through, the album was downloaded in
        // full and refused at the library write, and the refused files stayed in
        // `storage.staging_dir`. The filter counts the largest album group now, so
        // the rejection lands before anything is fetched.
        let client = Arc::new(MockClient::new());
        *client.search_results.lock().unwrap() = vec![SearchResult {
            username: "peer".into(),
            speed: 1000,
            slots: 1,
            files: vec![
                make_file(
                    r"Music\Test Artist\Test Album\09 - Nine.flac",
                    900,
                    1_000_000,
                ),
                make_file(
                    r"Music\Test Artist\Other Album\10 - Ten.flac",
                    900,
                    1_000_000,
                ),
                make_file(
                    r"Music\Test Artist\Third Album\11 - Eleven.flac",
                    900,
                    1_000_000,
                ),
            ],
        }];
        *client.write_files.lock().unwrap() = true;
        let db = Database::open_in_memory().unwrap();
        let staging = TempDir::new().unwrap();
        let library = TempDir::new().unwrap();

        let mut config = make_test_config();
        // The largest album group holds one file; the whole result holds three.
        config.filters.min_tracks = 3;

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
            Some(LibraryTarget::Place {
                root: library.path(),
                artist_dir: "Test Artist",
                skip_existing_album: false,
            }),
        )
        .await
        .unwrap();

        assert!(
            matches!(&result, AlbumOutcome::NoCandidates { reason } if reason.contains("no results passed filters")),
            "a one-track album group is not an album and must be refused before the download, got {result:?}"
        );
        assert!(
            client.download_filenames.lock().unwrap().is_empty(),
            "no transfer may be started for a set that can never be placed"
        );
        assert!(
            !staging.path().join("Test Artist--Test Album").exists(),
            "a refused set must not be staged"
        );
        assert!(
            !library.path().join("Test Artist/Test Album").exists(),
            "nothing may be written into the library for an incomplete set"
        );
    }

    #[tokio::test]
    async fn a_set_without_track_one_is_refused_before_the_download() {
        // The reported shape: two contiguous tracks from the middle of an album
        // (nine and ten). Contiguous numbering alone does not make a complete
        // album. The anchor half of the completeness rule now refuses this in the
        // filter, before anything is fetched, rather than after the download.
        // The two post-download refusal arms that remain (Place and organize) are
        // backstops a run can no longer reach, because `download_album` returns
        // Err rather than a short set when any file fails; their decision is
        // covered by the `library_write_refusal` unit tests and their cleanup by
        // `discard_refused_staging_removes_our_tree_and_tolerates_a_missing_directory`,
        // while `refused_download_leaves_no_staging_copy` covers the Upgrade arm
        // end to end.
        let client = Arc::new(MockClient::new());
        *client.search_results.lock().unwrap() = vec![SearchResult {
            username: "peer".into(),
            speed: 1000,
            slots: 1,
            files: vec![
                make_file(
                    r"Music\Test Artist\Test Album\09 - Nine.flac",
                    900,
                    1_000_000,
                ),
                make_file(
                    r"Music\Test Artist\Test Album\10 - Ten.flac",
                    900,
                    1_000_000,
                ),
            ],
        }];
        *client.write_files.lock().unwrap() = true;
        let db = Database::open_in_memory().unwrap();
        let staging = TempDir::new().unwrap();
        let library = TempDir::new().unwrap();

        let mut config = make_test_config();
        config.filters.min_tracks = 2; // the count gate alone would let this through

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
            Some(LibraryTarget::Place {
                root: library.path(),
                artist_dir: "Test Artist",
                skip_existing_album: false,
            }),
        )
        .await
        .unwrap();

        assert!(
            matches!(&result, AlbumOutcome::NoCandidates { reason } if reason.contains("no results passed filters")),
            "tracks nine and ten are not a complete album and must be refused before the download, got {result:?}"
        );
        // The refusal must be recorded, or the same fragment is re-selected from
        // the same peer on every cycle.
        assert_eq!(
            db.get_album_status("Test Artist", "Test Album").unwrap(),
            Some("failed".to_string()),
            "the refused album must be recorded as failed"
        );
        assert!(
            client.download_filenames.lock().unwrap().is_empty(),
            "no transfer may be started for a set that can never be placed"
        );
        assert!(
            !staging.path().join("Test Artist--Test Album").exists(),
            "a refused set must not be staged"
        );
        assert!(
            !library.path().join("Test Artist/Test Album").exists(),
            "nothing may be written into the library for a set that starts at track 9"
        );
    }

    #[tokio::test]
    async fn complete_download_is_still_written_to_the_library() {
        // The gate must not refuse a complete album: numbered from one, and at
        // least min_tracks long.
        let client = Arc::new(MockClient::new());
        *client.search_results.lock().unwrap() = vec![SearchResult {
            username: "peer".into(),
            speed: 1000,
            slots: 1,
            files: vec![
                make_file(
                    r"Music\Test Artist\Test Album\01 - One.flac",
                    900,
                    1_000_000,
                ),
                make_file(
                    r"Music\Test Artist\Test Album\02 - Two.flac",
                    900,
                    1_000_000,
                ),
                make_file(
                    r"Music\Test Artist\Test Album\03 - Three.flac",
                    900,
                    1_000_000,
                ),
            ],
        }];
        *client.write_files.lock().unwrap() = true;
        let db = Database::open_in_memory().unwrap();
        let staging = TempDir::new().unwrap();
        let library = TempDir::new().unwrap();

        let mut config = make_test_config();
        config.filters.min_tracks = 3;

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
            Some(LibraryTarget::Place {
                root: library.path(),
                artist_dir: "Test Artist",
                skip_existing_album: false,
            }),
        )
        .await
        .unwrap();

        assert_eq!(
            result,
            AlbumOutcome::Downloaded {
                track_count: 3,
                destination: DownloadDestination::Library(
                    library.path().join("Test Artist/Test Album")
                ),
            },
            "a complete album must still be placed"
        );
        assert!(library.path().join("Test Artist/Test Album").exists());
    }

    #[tokio::test]
    async fn a_short_album_group_is_not_organized_into_the_library() {
        // The generic organize path (manual and batch runs, target: None) writes
        // straight into library.paths[0]. Its completeness refusal is now a
        // backstop: a result whose largest album group is a one-track fragment is
        // refused by the filter before the download, so nothing is fetched, staged
        // or organized.
        let client = Arc::new(MockClient::new());
        *client.search_results.lock().unwrap() = vec![SearchResult {
            username: "peer".into(),
            speed: 1000,
            slots: 1,
            files: vec![
                make_file(
                    r"Music\Test Artist\Test Album\09 - Nine.flac",
                    900,
                    1_000_000,
                ),
                make_file(
                    r"Music\Test Artist\Other Album\10 - Ten.flac",
                    900,
                    1_000_000,
                ),
                make_file(
                    r"Music\Test Artist\Third Album\11 - Eleven.flac",
                    900,
                    1_000_000,
                ),
            ],
        }];
        *client.write_files.lock().unwrap() = true;
        let db = Database::open_in_memory().unwrap();
        let staging = TempDir::new().unwrap();
        let library = TempDir::new().unwrap();

        let mut config = make_test_config();
        config.filters.min_tracks = 3;
        config.storage.organize = true;
        config.storage.organize_pattern = "%artist%/%album%/%track% - %title%.%ext%".into();
        config.library.paths = vec![library.path().to_string_lossy().to_string()];

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
            None, // no library target: the generic organize path
        )
        .await
        .unwrap();

        assert!(
            matches!(&result, AlbumOutcome::NoCandidates { reason } if reason.contains("no results passed filters")),
            "the organize path must not receive a one-track album group, got {result:?}"
        );
        assert_eq!(
            db.get_album_status("Test Artist", "Test Album").unwrap(),
            Some("failed".to_string()),
            "the filter refusal must also record the album as failed"
        );
        assert!(
            client.download_filenames.lock().unwrap().is_empty(),
            "no transfer may be started for a set that can never be organized"
        );
        assert!(
            !staging.path().join("Test Artist--Test Album").exists(),
            "a refused set must not be staged"
        );
        assert!(
            !library.path().join("Test Artist/Test Album").exists(),
            "nothing may be organized into the library for an incomplete set"
        );
    }

    #[test]
    fn library_write_refusal_is_disabled_by_min_tracks_zero() {
        // The escape hatch for EPs and singles, matching the pre-download gate.
        // The set is one the numbering half *would* refuse, so removing the early
        // return fails this test: a single unnumbered file could not, because the
        // numbering guard skips it regardless.
        let files = vec![
            PathBuf::from("staging/09 - Nine.flac"),
            PathBuf::from("staging/10 - Ten.flac"),
        ];
        assert_eq!(library_write_refusal(&files, 0), None);
    }

    #[test]
    fn library_write_refusal_keeps_the_numbering_half_at_min_tracks_one() {
        // `min_tracks: 1` opens the count half for EPs, but the numbering half still
        // refuses a fragment that starts past track 1.
        let files = vec![
            PathBuf::from("staging/09 - Nine.flac"),
            PathBuf::from("staging/10 - Ten.flac"),
        ];
        assert!(
            library_write_refusal(&files, 1).is_some(),
            "a fragment starting at track 9 must still be refused at min_tracks: 1"
        );
    }

    #[test]
    fn library_write_refusal_accepts_unnumbered_files_it_cannot_judge() {
        // An album whose names carry no parseable number cannot be judged by
        // numbering, and demanding a number would refuse valid albums.
        let files = vec![
            PathBuf::from("staging/Intro.flac"),
            PathBuf::from("staging/Outro.flac"),
        ];
        assert_eq!(library_write_refusal(&files, 2), None);
    }

    #[test]
    fn library_write_refusal_reports_the_shortfall_before_the_numbering() {
        let short = vec![PathBuf::from("staging/01 - One.flac")];
        let reason = library_write_refusal(&short, 5).expect("a short set must be refused");
        assert!(
            reason.contains("only 1 of at least 5 tracks"),
            "got {reason}"
        );

        let mid = vec![
            PathBuf::from("staging/09 - Nine.flac"),
            PathBuf::from("staging/10 - Ten.flac"),
        ];
        let reason = library_write_refusal(&mid, 2).expect("a set starting at 9 must be refused");
        assert!(reason.contains("start at track 9"), "got {reason}");
    }

    #[tokio::test]
    async fn a_number_earlier_in_the_filename_does_not_refuse_a_complete_album() {
        // `track_number_from_filename` takes the first numeric token, so every file
        // of "Blink 182 - Enema of the State - NN - Title.flac" parses as track 182.
        // A complete album must still be written: a run of identical values is a
        // phantom parse, not a mid-album fragment.
        let client = Arc::new(MockClient::new());
        *client.search_results.lock().unwrap() = vec![SearchResult {
            username: "peer".into(),
            speed: 1000,
            slots: 1,
            files: vec![
                make_file(
                    r"Music\Blink 182\Enema of the State\Blink 182 - Enema of the State - 01 - Dumpweed.flac",
                    900,
                    1_000_000,
                ),
                make_file(
                    r"Music\Blink 182\Enema of the State\Blink 182 - Enema of the State - 02 - One Step Closer.flac",
                    900,
                    1_000_000,
                ),
                make_file(
                    r"Music\Blink 182\Enema of the State\Blink 182 - Enema of the State - 03 - Aliens Exist.flac",
                    900,
                    1_000_000,
                ),
            ],
        }];
        *client.write_files.lock().unwrap() = true;
        let db = Database::open_in_memory().unwrap();
        let staging = TempDir::new().unwrap();
        let library = TempDir::new().unwrap();

        let mut config = make_test_config();
        config.filters.min_tracks = 3;

        let result = process_album(
            client.as_ref() as &dyn crate::client::SoulseekClient,
            "Blink 182",
            Some("Enema of the State"),
            false,
            &config,
            &db,
            staging.path(),
            None,
            None,
            None,
            Some(LibraryTarget::Place {
                root: library.path(),
                artist_dir: "Blink 182",
                skip_existing_album: false,
            }),
        )
        .await
        .unwrap();

        assert_eq!(
            result,
            AlbumOutcome::Downloaded {
                track_count: 3,
                destination: DownloadDestination::Library(
                    library.path().join("Blink 182/Enema of the State")
                ),
            },
            "a complete album must not be refused because a number precedes the track number"
        );
    }

    #[tokio::test]
    async fn a_mixed_unnumbered_set_is_not_refused_by_the_numbering_half() {
        // A complete album can lead with an unnumbered track ("Intro.flac") and
        // number the rest. The numbering half must not judge a mixed set.
        let client = Arc::new(MockClient::new());
        *client.search_results.lock().unwrap() = vec![SearchResult {
            username: "peer".into(),
            speed: 1000,
            slots: 1,
            files: vec![
                make_file(r"Music\Test Artist\Test Album\Intro.flac", 900, 1_000_000),
                make_file(
                    r"Music\Test Artist\Test Album\02 - Two.flac",
                    900,
                    1_000_000,
                ),
                make_file(
                    r"Music\Test Artist\Test Album\03 - Three.flac",
                    900,
                    1_000_000,
                ),
            ],
        }];
        *client.write_files.lock().unwrap() = true;
        let db = Database::open_in_memory().unwrap();
        let staging = TempDir::new().unwrap();
        let library = TempDir::new().unwrap();

        let mut config = make_test_config();
        config.filters.min_tracks = 3;

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
            Some(LibraryTarget::Place {
                root: library.path(),
                artist_dir: "Test Artist",
                skip_existing_album: false,
            }),
        )
        .await
        .unwrap();

        assert_eq!(
            result,
            AlbumOutcome::Downloaded {
                track_count: 3,
                destination: DownloadDestination::Library(
                    library.path().join("Test Artist/Test Album")
                ),
            },
            "a mixed numbered/unnumbered set must not be refused by the numbering half"
        );
    }

    #[tokio::test]
    async fn a_fused_disc_and_track_number_is_not_a_fragment() {
        // Rips that fuse disc and track ("101" for disc 1 track 1) name every file
        // that way. The album is complete and must be placed: reading 101 literally
        // would refuse it and demote the peer that served it.
        let client = Arc::new(MockClient::new());
        *client.search_results.lock().unwrap() = vec![SearchResult {
            username: "peer".into(),
            speed: 1000,
            slots: 1,
            files: vec![
                make_file(
                    r"Music\Test Artist\Test Album\101 - One.flac",
                    900,
                    1_000_000,
                ),
                make_file(
                    r"Music\Test Artist\Test Album\102 - Two.flac",
                    900,
                    1_000_000,
                ),
                make_file(
                    r"Music\Test Artist\Test Album\103 - Three.flac",
                    900,
                    1_000_000,
                ),
            ],
        }];
        *client.write_files.lock().unwrap() = true;
        let db = Database::open_in_memory().unwrap();
        let staging = TempDir::new().unwrap();
        let library = TempDir::new().unwrap();

        let mut config = make_test_config();
        config.filters.min_tracks = 3;

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
            Some(LibraryTarget::Place {
                root: library.path(),
                artist_dir: "Test Artist",
                skip_existing_album: false,
            }),
        )
        .await
        .unwrap();

        assert_eq!(
            result,
            AlbumOutcome::Downloaded {
                track_count: 3,
                destination: DownloadDestination::Library(
                    library.path().join("Test Artist/Test Album")
                ),
            },
            "a fused disc+track album is complete and must not be refused"
        );
    }

    #[tokio::test]
    async fn the_organize_path_never_sees_a_set_without_track_one() {
        // The organizer's own numbering refusal is now a backstop: the filter
        // refuses a set that starts past track 1 before the download, so the
        // organize path is never handed one.
        let client = Arc::new(MockClient::new());
        *client.search_results.lock().unwrap() = vec![SearchResult {
            username: "peer".into(),
            speed: 1000,
            slots: 1,
            files: vec![
                make_file(
                    r"Music\Test Artist\Test Album\09 - Nine.flac",
                    900,
                    1_000_000,
                ),
                make_file(
                    r"Music\Test Artist\Test Album\10 - Ten.flac",
                    900,
                    1_000_000,
                ),
            ],
        }];
        *client.write_files.lock().unwrap() = true;
        let db = Database::open_in_memory().unwrap();
        let staging = TempDir::new().unwrap();
        let library = TempDir::new().unwrap();

        let mut config = make_test_config();
        config.filters.min_tracks = 2;
        config.storage.organize = true;
        config.storage.organize_pattern = "%artist%/%album%/%track% - %title%.%ext%".into();
        config.library.paths = vec![library.path().to_string_lossy().to_string()];

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
        .await
        .unwrap();

        assert!(
            matches!(&result, AlbumOutcome::NoCandidates { reason } if reason.contains("no results passed filters")),
            "a set starting at track 9 must be refused before the download, got {result:?}"
        );
        assert!(
            client.download_filenames.lock().unwrap().is_empty(),
            "no transfer may be started for a set that can never be organized"
        );
        assert!(
            !staging.path().join("Test Artist--Test Album").exists(),
            "a refused set must not be staged"
        );
        assert!(
            !library.path().join("Test Artist").exists(),
            "nothing may be organized into the library for a fragment"
        );
    }

    #[tokio::test]
    async fn completion_line_marks_a_staging_only_album() {
        // Staging-only state: organisation is off and no library path is
        // configured, so staging is where the album stays. The completion line
        // must say so rather than staying silent, which is the defect this
        // behaviour exists to fix.
        //
        // Distinctive artist and album names for the same reason as the library
        // fixture above: `LogCapture` keeps one process-wide window, so a
        // neighbouring test's completion line can land in this buffer.
        let client = Arc::new(MockClient::new());
        *client.search_results.lock().unwrap() = vec![SearchResult {
            username: "peer".into(),
            speed: 1000,
            slots: 1,
            files: vec![make_file(
                r"Staging Fixture Artist\Staging Fixture Album\01 - track.flac",
                900,
                10_000_000,
            )],
        }];
        // Real bytes so the staged album exists on disk, as it does in a real
        // staging-only run.
        *client.write_files.lock().unwrap() = true;
        let db = Database::open_in_memory().unwrap();
        let staging = TempDir::new().unwrap();

        let mut config = make_test_config();
        config.storage.organize = false;
        config.library.paths.clear();

        let capture = crate::test_support::LogCapture::start();
        let result = process_album(
            client.as_ref() as &dyn crate::client::SoulseekClient,
            "Staging Fixture Artist",
            Some("Staging Fixture Album"),
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
            matches!(
                result,
                AlbumOutcome::Downloaded {
                    destination: DownloadDestination::Staging(_),
                    ..
                }
            ),
            "an album with no library write must be reported as staging, got {result:?}"
        );

        let logs = capture.text();
        let line = logs
            .lines()
            .find(|line| line.contains("Completed: Staging Fixture Artist"))
            .unwrap_or_else(|| panic!("no completion line for this fixture, got:\n{logs}"));
        assert!(
            line.contains("(kept in staging)"),
            "a staging destination must be marked, got: {line}"
        );
        // The album is still in staging - reporting it must not have deleted it.
        // `staging.path()` is the TempDir root and always exists, so the assertion
        // must name the album's own staging directory: that is the object a
        // regressed `remove_dir_all` would delete.
        assert!(
            staging
                .path()
                .join("Staging Fixture Artist--Staging Fixture Album")
                .exists(),
            "a staging destination must not remove the album's staging directory"
        );
    }

    #[tokio::test]
    async fn placement_reports_the_album_folder_as_the_destination() {
        // The place path passes `Library(outcome.album_dir)` from
        // `place_into_library`. The organizer unit tests cover `album_dir` itself
        // and the generic-organize test covers the other wiring, so this closes
        // the seam between them for placement, where the artist folder is used
        // verbatim from disk.
        let client = Arc::new(MockClient::new());
        *client.search_results.lock().unwrap() = vec![SearchResult {
            username: "peer".into(),
            speed: 1000,
            slots: 1,
            files: vec![make_file(
                r"Place Fixture Artist\Place Fixture Album\01 - track.flac",
                900,
                10_000_000,
            )],
        }];
        *client.write_files.lock().unwrap() = true;
        let db = Database::open_in_memory().unwrap();
        let staging = TempDir::new().unwrap();
        let library = TempDir::new().unwrap();

        let mut config = make_test_config();
        config.storage.organize_pattern = "%artist%/%album%/%track% - %title%.%ext%".into();

        let result = process_album(
            client.as_ref() as &dyn crate::client::SoulseekClient,
            "Place Fixture Artist",
            Some("Place Fixture Album"),
            false,
            &config,
            &db,
            staging.path(),
            None,
            None,
            None,
            Some(LibraryTarget::Place {
                root: library.path(),
                artist_dir: "Place Fixture Artist",
                skip_existing_album: false,
            }),
        )
        .await
        .unwrap();

        assert_eq!(
            result,
            AlbumOutcome::Downloaded {
                track_count: 1,
                destination: DownloadDestination::Library(
                    library
                        .path()
                        .join("Place Fixture Artist/Place Fixture Album")
                ),
            }
        );
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
        assert!(
            matches!(
                result.unwrap(),
                AlbumOutcome::Downloaded { track_count: 1, .. }
            ),
            "the album must complete with its downloaded track count"
        );
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
            AlbumOutcome::NoCandidates { reason } => assert_eq!(reason, "no results found"),
            other => panic!("Expected AlbumOutcome::NoCandidates, got: {other:?}"),
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

        assert!(matches!(result, Ok(AlbumOutcome::NoCandidates { .. })));
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
    async fn album_only_batch_organization_is_rejected_without_an_artist() {
        // The generic organize step builds `<root>/<artist>/<album>`, so a batch line
        // with no artist cannot be organized. Batch passes no target, which is the
        // shape this guard protects.
        let client = MockClient::new();
        let staging = TempDir::new().unwrap();
        let library = TempDir::new().unwrap();
        let mut config = make_test_config();
        config.library.paths = vec![library.path().to_string_lossy().into()];
        config.storage.organize = true;
        config.storage.staging_dir = staging.path().to_string_lossy().into();
        let db = Database::open_in_memory().unwrap();

        let error = process_album(
            &client,
            "",
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
    async fn manual_album_only_with_organize_on_keeps_staging() {
        // A manual album-only run never organizes, so the guard above must not reject
        // it: the search proceeds and nothing is created in the library.
        let client = MockClient::new();
        *client.search_results.lock().unwrap() = vec![album_result("Real Artist", "Test Album")];
        *client.write_files.lock().unwrap() = true;
        let staging = TempDir::new().unwrap();
        let library = TempDir::new().unwrap();
        let mut config = make_test_config();
        config.library.paths = vec![library.path().to_string_lossy().into()];
        config.storage.organize = true;
        config.storage.staging_dir = staging.path().to_string_lossy().into();
        let db = Database::open_in_memory().unwrap();

        run_manual_mode(&client, None, Some("Test Album"), false, &config, &db)
            .await
            .expect("a manual album-only run must not be rejected for organizing");

        assert!(
            !client.search_queries.lock().unwrap().is_empty(),
            "the run must search rather than aborting"
        );
        assert!(
            staging
                .path()
                .join("--Test Album")
                .join("01 - track.flac")
                .exists(),
            "the album-only download must stay in staging"
        );
        assert!(
            library.path().read_dir().unwrap().next().is_none(),
            "nothing may be organized into the library"
        );
    }

    #[tokio::test]
    async fn a_manual_run_that_downloads_nothing_does_not_explain_staging() {
        // The explanation belongs to an album that really stayed in staging, and the
        // artist name in the line keeps the assertion to this test's own records.
        let client = MockClient::new();
        let (mut config, db, _staging) = artist_only_fixture();
        config.library.paths = vec!["/definitely/not/here".to_string()];
        config.discography.enabled = false;
        let capture = crate::test_support::LogCapture::start();

        run_manual_mode(
            &client,
            Some("Quiet Artist 4d2e"),
            Some("Quiet Album"),
            false,
            &config,
            &db,
        )
        .await
        .expect("a run with no search results completes");

        let logs = capture.text();
        assert!(
            !logs.lines().any(|line| {
                line.contains("no library folder found") && line.contains("Quiet Artist 4d2e")
            }),
            "a run that downloaded nothing must not explain staging:\n{logs}"
        );
    }

    #[tokio::test]
    async fn manual_mode_without_library_paths_keeps_staging() {
        // With no configured library path there is nothing to place into, so the
        // explicit-album form keeps its download in staging.
        let client = MockClient::new();
        *client.search_results.lock().unwrap() = vec![album_result("Test Artist", "New Album")];
        *client.write_files.lock().unwrap() = true;
        let (mut config, db, staging) = artist_only_fixture();
        config.library.paths.clear();
        config.storage.organize = true;
        config.discography.enabled = false;

        run_manual_mode(
            &client,
            Some("Test Artist"),
            Some("New Album"),
            false,
            &config,
            &db,
        )
        .await
        .expect("manual album mode must complete");

        assert!(
            staging
                .path()
                .join("Test Artist--New Album")
                .join("01 - track.flac")
                .exists(),
            "with no library path the download stays in staging"
        );
    }

    #[tokio::test]
    async fn manual_mode_reports_a_placement_failure_and_keeps_staging() {
        // A destination that exists as a file cannot be created as a directory, so
        // placement fails. The album is reported failed with its staging copy kept,
        // exactly as the shared arm does for discover.
        let client = MockClient::new();
        *client.search_results.lock().unwrap() = vec![album_result("Test Artist", "New Album")];
        *client.write_files.lock().unwrap() = true;
        let (mut config, db, staging) = artist_only_fixture();
        let library = library_with(&[("Test Artist", "Present")]);
        std::fs::write(
            library.path().join("Test Artist").join("New Album"),
            b"not a folder",
        )
        .unwrap();
        config.library.paths = vec![library.path().to_string_lossy().into_owned()];
        config.discography.enabled = false;

        run_manual_mode(
            &client,
            Some("Test Artist"),
            Some("New Album"),
            false,
            &config,
            &db,
        )
        .await
        .expect("the run completes; the album is reported failed");

        assert!(
            staging
                .path()
                .join("Test Artist--New Album")
                .join("01 - track.flac")
                .exists(),
            "a failed placement must keep the staging copy"
        );
        assert_eq!(
            std::fs::read(library.path().join("Test Artist").join("New Album")).unwrap(),
            b"not a folder",
            "the blocking file is untouched"
        );
        assert!(
            db.get_processed_albums()
                .unwrap()
                .iter()
                .any(|record| record.album == "New Album" && record.status == "failed"),
            "the album must be recorded as failed"
        );
    }

    #[tokio::test]
    async fn manual_mode_places_with_a_pattern_that_has_no_artist_component() {
        // `%track% - %title%.%ext%` derives the library root, which always exists, so a
        // gate that only exempted the artist folder itself would treat the root as an
        // existing album folder and never place anything.
        let client = MockClient::new();
        *client.search_results.lock().unwrap() = vec![album_result("Test Artist", "New Album")];
        *client.write_files.lock().unwrap() = true;
        let (mut config, db, staging) = artist_only_fixture();
        let library = library_with(&[("Test Artist", "Present")]);
        config.library.paths = vec![library.path().to_string_lossy().into_owned()];
        config.storage.organize_pattern = "%track% - %title%.%ext%".into();
        config.discography.enabled = false;

        run_manual_mode(
            &client,
            Some("Test Artist"),
            Some("New Album"),
            false,
            &config,
            &db,
        )
        .await
        .expect("manual album mode must complete");

        assert!(
            library.path().join("01 - track.flac").exists(),
            "the pattern's destination is the library root, and the album must be placed there"
        );
        assert!(
            !staging.path().join("Test Artist--New Album").exists(),
            "a placed album leaves no staging copy"
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
    async fn artist_only_manual_mode_keeps_a_multi_disc_album_in_staging_without_an_artist_folder()
    {
        // Supersedes the old "organize creates the artist folder" expectation: a
        // manual run never creates an artist folder, so with storage.organize on and
        // no folder for the artist the album stays in staging, disc folders and all.
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
            .expect("artist-only manual mode must still download every disc");

        let staged = staging.path().join("Test Artist--Album One");
        assert!(staged.join("CD 01/01 - one.flac").exists());
        assert!(staged.join("CD 02/01 - one.flac").exists());
        assert!(
            !library.path().join("Test Artist").exists(),
            "a manual run never creates an artist folder, even with storage.organize on"
        );
    }

    #[tokio::test]
    async fn manual_mode_does_not_organize_a_folder_the_artist_does_not_have() {
        // The explicit-album form takes the same route: with storage.organize on and
        // no artist folder, the album stays in staging rather than being organized
        // into a folder the run created itself.
        let client = MockClient::new();
        *client.search_results.lock().unwrap() = vec![album_result("Test Artist", "New Album")];
        *client.write_files.lock().unwrap() = true;
        let (mut config, db, staging) = artist_only_fixture();
        let library = TempDir::new().unwrap();
        config.library.paths = vec![library.path().to_string_lossy().into_owned()];
        config.storage.organize = true;
        config.discography.enabled = false;

        run_manual_mode(
            &client,
            Some("Test Artist"),
            Some("New Album"),
            false,
            &config,
            &db,
        )
        .await
        .expect("manual album mode must complete");

        assert!(
            staging
                .path()
                .join("Test Artist--New Album")
                .join("01 - track.flac")
                .exists(),
            "the album must stay in staging when the artist folder does not exist"
        );
        assert!(
            !library.path().join("Test Artist").exists(),
            "the organize step must not run for a manual run"
        );
    }

    #[tokio::test]
    async fn manual_mode_album_only_never_places_into_a_placeholder_folder() {
        // An album-only run names no artist, so the resolver must not match a folder
        // whose own name sanitises away to the placeholder - before the guard this
        // filed the album under <library>/_/.
        let client = MockClient::new();
        *client.search_results.lock().unwrap() = vec![album_result("Real Artist", "Any Album")];
        *client.write_files.lock().unwrap() = true;
        let (mut config, db, _staging) = artist_only_fixture();
        let library = TempDir::new().unwrap();
        std::fs::create_dir_all(library.path().join("_")).unwrap();
        config.library.paths = vec![library.path().to_string_lossy().into_owned()];
        config.discography.enabled = false;
        let capture = crate::test_support::LogCapture::start();

        run_manual_mode(&client, None, Some("Any Album"), false, &config, &db)
            .await
            .expect("an album-only manual run must complete");

        assert!(
            !client.download_filenames.lock().unwrap().is_empty(),
            "the fixture must actually download something, or the assertion below is vacuous"
        );
        assert!(
            !library.path().join("_").join("Any Album").exists(),
            "an album-only run must not be filed under the placeholder folder"
        );
        // The blank name must not be reported as a missing artist folder either: a
        // line for it renders as the target followed by an empty message, which only
        // this test can produce, so the shared log window stays trustworthy.
        let logs = capture.text();
        assert!(
            !logs.contains("seakarr::runner: : no library folder found"),
            "an album-only run must not report a missing folder for an empty artist:\n{logs}"
        );
    }

    #[tokio::test]
    async fn manual_mode_places_with_a_pattern_that_omits_the_album_folder() {
        // `%artist%/%track% - %title%.%ext%` writes tracks straight into the artist
        // folder, so the derived directory is the artist folder itself. Treating that
        // as an existing album folder would make manual placement impossible under
        // the pattern; the files are placed and placement's keep rule handles
        // collisions instead.
        let client = MockClient::new();
        *client.search_results.lock().unwrap() = vec![album_result("Test Artist", "New Album")];
        *client.write_files.lock().unwrap() = true;
        let (mut config, db, staging) = artist_only_fixture();
        let library = library_with(&[("Test Artist", "Present")]);
        config.library.paths = vec![library.path().to_string_lossy().into_owned()];
        config.storage.organize_pattern = "%artist%/%track% - %title%.%ext%".into();
        config.discography.enabled = false;

        run_manual_mode(
            &client,
            Some("Test Artist"),
            Some("New Album"),
            false,
            &config,
            &db,
        )
        .await
        .expect("manual album mode must complete");

        assert!(
            library
                .path()
                .join("Test Artist")
                .join("01 - track.flac")
                .exists(),
            "the pattern's destination is the artist folder, and the album must be placed there"
        );
        assert!(
            !staging.path().join("Test Artist--New Album").exists(),
            "a placed album leaves no staging copy"
        );
    }

    #[tokio::test]
    async fn manual_mode_reports_an_album_folder_that_already_exists() {
        // The reason a download stayed in staging must be visible: a unique artist
        // name keeps the LogCapture assertion to this test's own line.
        let client = MockClient::new();
        *client.search_results.lock().unwrap() =
            vec![album_result("Report Folder Artist 9f2b", "Report Album")];
        *client.write_files.lock().unwrap() = true;
        let (mut config, db, staging) = artist_only_fixture();
        let library = library_with(&[("Report Folder Artist 9f2b", "Report Album")]);
        config.library.paths = vec![library.path().to_string_lossy().into_owned()];
        config.discography.enabled = false;
        let capture = crate::test_support::LogCapture::start();

        run_manual_mode(
            &client,
            Some("Report Folder Artist 9f2b"),
            Some("Report Album"),
            false,
            &config,
            &db,
        )
        .await
        .expect("manual album mode must complete");

        let logs = capture.text();
        let line = logs
            .lines()
            .find(|line| {
                line.contains("album folder already exists at")
                    && line.contains("Report Album")
                    && line.contains("Report Folder Artist 9f2b")
            })
            .unwrap_or_else(|| panic!("no existing-folder line for this fixture, got:\n{logs}"));
        assert!(
            line.contains("Report Folder Artist 9f2b"),
            "the line must name the artist: {line}"
        );
        assert!(
            staging
                .path()
                .join("Report Folder Artist 9f2b--Report Album")
                .exists(),
            "the download stays in staging"
        );
    }

    #[tokio::test]
    async fn manual_mode_places_a_multi_disc_album_into_the_existing_artist_folder() {
        // The shared copy path keeps per-disc subfolders; a manual placement must
        // reach the library with the same shape staging has.
        let client = MockClient::new();
        *client.write_files.lock().unwrap() = true;
        *client.search_results.lock().unwrap() = vec![SearchResult {
            username: "artist-peer".into(),
            speed: 500,
            slots: 1,
            files: vec![
                make_file(
                    r"Test Artist\New Album\CD 01\01 - one.flac",
                    900,
                    10_000_000,
                ),
                make_file(
                    r"Test Artist\New Album\CD 02\01 - two.flac",
                    900,
                    10_000_000,
                ),
            ],
        }];
        let (mut config, db, staging) = artist_only_fixture();
        let library = library_with(&[("Test Artist", "Present")]);
        config.library.paths = vec![library.path().to_string_lossy().into_owned()];
        config.discography.enabled = false;

        run_manual_mode(
            &client,
            Some("Test Artist"),
            Some("New Album"),
            false,
            &config,
            &db,
        )
        .await
        .expect("manual album mode must complete");

        let album_dir = library.path().join("Test Artist").join("New Album");
        assert!(
            album_dir.join("CD 01/01 - one.flac").exists(),
            "disc 1 must be placed under the album folder"
        );
        assert!(
            album_dir.join("CD 02/01 - two.flac").exists(),
            "disc 2 must be placed under the album folder"
        );
        assert!(
            !staging.path().join("Test Artist--New Album").exists(),
            "a placed album leaves no staging copy"
        );
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

    #[tokio::test]
    async fn legacy_merged_variants_keep_every_peer_eligible() {
        let client = MockClient::new();
        *client.search_results.lock().unwrap() = vec![
            SearchResult {
                username: "unavailable-peer".into(),
                speed: 900,
                slots: 0,
                files: vec![make_file(
                    r"Test Artist\1998 Album Name\01.flac",
                    900,
                    10_000_000,
                )],
            },
            SearchResult {
                username: "usable-peer".into(),
                speed: 500,
                slots: 1,
                files: vec![
                    make_file(r"Test Artist\Album Name (1998)\01.flac", 900, 10_000_000),
                    make_file(r"Test Artist\Album Name (1998)\02.flac", 900, 10_000_000),
                ],
            },
        ];
        let staging = TempDir::new().unwrap();
        let mut config = make_test_config();
        config.discography.enabled = false;
        config.storage.staging_dir = staging.path().to_string_lossy().into();
        config.filters.min_tracks = 2;
        let db = Database::open_in_memory().unwrap();

        run_manual_mode(&client, Some("Test Artist"), None, false, &config, &db)
            .await
            .unwrap();

        let downloaded = client.download_filenames.lock().unwrap();
        assert_eq!(
            downloaded.len(),
            2,
            "merged usable peer was filtered: {downloaded:?}"
        );
        assert!(downloaded
            .iter()
            .all(|name| name.contains("Album Name (1998)")));
    }

    #[test]
    fn canonical_processed_target_matches_normalized_album_identity() {
        let db = Database::open_in_memory().unwrap();
        db.mark_album_processed(
            "Kruder & Dorfmeister",
            "Kruder & Dorfmeister - The K&D Sessions",
            "success",
        )
        .unwrap();

        let records = db.get_processed_albums().unwrap();
        let target =
            canonical_processed_target(&records, "Kruder & Dorfmeister", "1998 K And D Sessions");

        assert_eq!(
            target,
            (
                "Kruder & Dorfmeister".to_string(),
                "Kruder & Dorfmeister - The K&D Sessions".to_string()
            )
        );
    }

    #[tokio::test]
    async fn legacy_split_discs_across_peers_fail_instead_of_marking_partial_success() {
        let client = MockClient::new();
        *client.search_results.lock().unwrap() = vec![
            SearchResult {
                username: "disc-one-peer".into(),
                speed: 600,
                slots: 1,
                files: vec![make_file(
                    r"Test Artist\Album {cd1}\01.flac",
                    900,
                    10_000_000,
                )],
            },
            SearchResult {
                username: "disc-two-peer".into(),
                speed: 500,
                slots: 1,
                files: vec![make_file(
                    r"Test Artist\Album {cd2}\01.flac",
                    900,
                    10_000_000,
                )],
            },
        ];
        let staging = TempDir::new().unwrap();
        let mut config = make_test_config();
        config.discography.enabled = false;
        config.storage.staging_dir = staging.path().to_string_lossy().into();
        config.filters.min_tracks = 1;
        let db = Database::open_in_memory().unwrap();

        let run = run_legacy_artist_only_mode(
            &client,
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

        assert!(matches!(
            &run.outcomes[0].1,
            AlbumOutcome::NoCandidates { reason }
                if reason.contains("multi-disc album is split across peers")
        ));
        assert!(client.download_filenames.lock().unwrap().is_empty());
        assert_eq!(
            db.get_album_status("Test Artist", "Album")
                .unwrap()
                .as_deref(),
            Some("failed")
        );
    }

    #[test]
    fn split_disc_filter_keeps_largest_self_consistent_edition() {
        let mut results = vec![
            SearchResult {
                username: "two-discs".into(),
                speed: 500,
                slots: 1,
                files: vec![
                    make_file(r"Artist\Album {cd1}\01.flac", 900, 10_000_000),
                    make_file(r"Artist\Album {cd2}\01.flac", 900, 10_000_000),
                ],
            },
            SearchResult {
                username: "third-disc".into(),
                speed: 400,
                slots: 1,
                files: vec![make_file(r"Artist\Album {cd3}\01.flac", 900, 10_000_000)],
            },
        ];

        assert!(!remove_incomplete_split_disc_candidates(&mut results));
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].username, "two-discs");
    }

    #[test]
    fn split_disc_filter_keeps_flat_and_fully_advertised_candidates() {
        let mut results = vec![
            SearchResult {
                username: "complete".into(),
                speed: 500,
                slots: 1,
                files: vec![
                    make_file(r"Artist\Album {cd1}\01.flac", 900, 10_000_000),
                    make_file(r"Artist\Album {cd2}\01.flac", 900, 10_000_000),
                ],
            },
            SearchResult {
                username: "split".into(),
                speed: 400,
                slots: 1,
                files: vec![make_file(r"Artist\Album {cd1}\01.flac", 900, 10_000_000)],
            },
            SearchResult {
                username: "flat".into(),
                speed: 300,
                slots: 1,
                files: vec![make_file(r"Artist\Album\01.flac", 900, 10_000_000)],
            },
        ];

        assert!(!remove_incomplete_split_disc_candidates(&mut results));
        assert_eq!(
            results
                .iter()
                .map(|result| result.username.as_str())
                .collect::<Vec<_>>(),
            ["complete", "flat"]
        );
    }

    #[tokio::test]
    async fn legacy_same_peer_variants_do_not_combine_incomplete_track_counts() {
        let client = MockClient::new();
        *client.search_results.lock().unwrap() = vec![SearchResult {
            username: "peer".into(),
            speed: 500,
            slots: 1,
            files: vec![
                make_file(r"Test Artist\1998 Album\01.flac", 900, 10_000_000),
                make_file(r"Test Artist\1998 Album\02.flac", 900, 10_000_000),
                make_file(r"Test Artist\Album (1998)\01.flac", 900, 10_000_000),
                make_file(r"Test Artist\Album (1998)\02.flac", 900, 10_000_000),
            ],
        }];
        let staging = TempDir::new().unwrap();
        let mut config = make_test_config();
        config.discography.enabled = false;
        config.storage.staging_dir = staging.path().to_string_lossy().into();
        config.filters.min_tracks = 3;
        let db = Database::open_in_memory().unwrap();

        run_manual_mode(&client, Some("Test Artist"), None, false, &config, &db)
            .await
            .unwrap();

        assert!(client.download_filenames.lock().unwrap().is_empty());
    }

    #[test]
    fn canonical_processed_target_ignores_historical_single_disc_success() {
        let db = Database::open_in_memory().unwrap();
        db.mark_album_processed("Test Artist", "Album {cd1}", "success")
            .unwrap();
        let records = db.get_processed_albums().unwrap();

        let target = canonical_processed_target(&records, "Test Artist", "Album");

        assert_eq!(target, ("Test Artist".to_string(), "Album".to_string()));
    }

    #[test]
    fn canonical_processed_target_prefers_successful_identity_match() {
        let db = Database::open_in_memory().unwrap();
        db.mark_album_processed("Test Artist", "1998 Album", "failed")
            .unwrap();
        db.mark_album_processed("Test Artist", "Test Artist - Album (1998)", "success")
            .unwrap();

        let records = db.get_processed_albums().unwrap();
        let target = canonical_processed_target(&records, "Test Artist", "Album");

        assert_eq!(target.1, "Test Artist - Album (1998)");
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
        /// `None` echoes the requested artist back as the only candidate.
        candidate_names: Option<Vec<String>>,
        artist_calls: Arc<AtomicUsize>,
    }

    impl FakeDiscographyProvider {
        fn with_groups(groups: Vec<ReleaseGroup>) -> Self {
            Self {
                groups,
                failure: None,
                candidate_names: None,
                artist_calls: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn failing(reason: &str) -> Self {
            Self {
                groups: Vec::new(),
                failure: Some(reason.to_string()),
                candidate_names: None,
                artist_calls: Arc::new(AtomicUsize::new(0)),
            }
        }

        /// A provider whose artist search returns only non-matching names, so
        /// resolution fails as `DiscoveryFailure::Unresolved`.
        fn unresolvable() -> Self {
            Self {
                groups: Vec::new(),
                failure: None,
                candidate_names: Some(vec!["Somebody Else".to_string()]),
                artist_calls: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn calls(&self) -> usize {
            self.artist_calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl DiscographyProvider for FakeDiscographyProvider {
        async fn search_artists(
            &self,
            artist: &str,
        ) -> std::result::Result<Vec<ArtistCandidate>, DiscographyError> {
            self.artist_calls.fetch_add(1, Ordering::SeqCst);
            if let Some(reason) = &self.failure {
                return Err(DiscographyError::Transport(reason.clone()));
            }
            let names = self
                .candidate_names
                .clone()
                .unwrap_or_else(|| vec![artist.to_string()]);
            Ok(names
                .into_iter()
                .enumerate()
                .map(|(position, name)| ArtistCandidate {
                    id: format!("1111111{position}-1111-1111-1111-111111111111"),
                    name,
                    score: None,
                })
                .collect())
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
                score: None,
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

        async fn request_queue_position(&self, _username: &str, _filename: &str) -> bool {
            false
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
    async fn authoritative_search_keeps_musicbrainz_title_despite_failed_legacy_spelling() {
        let soulseek = MockClient::new();
        soulseek.search_results_by_query.lock().unwrap().insert(
            "Test Artist Album".into(),
            vec![album_result("Test Artist", "Album")],
        );
        let provider =
            FakeDiscographyProvider::with_groups(vec![release_group("album-id", "Album", "1998")]);
        let (config, db, staging) = artist_only_fixture();
        db.mark_album_processed("Test Artist", "Test Artist - Album (1998)", "failed")
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
            soulseek.search_queries.lock().unwrap()[0],
            "Test Artist Album"
        );
        assert!(matches!(run.outcomes[0].1, AlbumOutcome::Downloaded { .. }));
    }

    #[tokio::test]
    async fn authoritative_discovery_skips_identity_equivalent_legacy_success() {
        let soulseek = MockClient::new();
        let provider = FakeDiscographyProvider::with_groups(vec![release_group(
            "album-id",
            "The K&D Sessions",
            "1998",
        )]);
        let (config, db, staging) = artist_only_fixture();
        db.mark_album_processed("Test Artist", "1998 The K&D Sessions", "success")
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

        assert!(soulseek.search_queries.lock().unwrap().is_empty());
        assert_eq!(run.outcomes[0].1, AlbumOutcome::Skipped);
    }

    #[tokio::test]
    async fn authoritative_run_skips_identity_equivalent_legacy_success() {
        let soulseek = MockClient::new();
        let provider = FakeDiscographyProvider::with_groups(vec![release_group(
            "album-id",
            "The K&D Sessions",
            "1998",
        )]);
        let (config, db, staging) = artist_only_fixture();
        db.mark_album_processed("Kruder and Dorfmeister", "1998 K And D Sessions", "success")
            .unwrap();

        let run = run_artist_only_mode_with_provider(
            &soulseek,
            "Kruder & Dorfmeister",
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

        assert!(soulseek.search_queries.lock().unwrap().is_empty());
        assert_eq!(run.outcomes[0].1, AlbumOutcome::Skipped);
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
            matches!(run.outcomes[0].1, AlbumOutcome::NoCandidates { .. }),
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

        let capture = crate::test_support::LogCapture::start();
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

            let captured = capture.text();
                // One record, so neither half of this warning can be supplied by
                // the other stale-cache tests' records, and the level is asserted
                // on that record too: the harness captures at DEBUG, so without
                // this a downgrade to debug! would pass while the outage warning
                // disappeared from the operator's log.
                let record = captured
                    .lines()
                    .find(|line| {
                        line.contains("day(s) old and refresh failed (MusicBrainz transport error: service unavailable); using stale cache")
                    })
                    .unwrap_or_else(|| panic!("no stale-cache WARN record, got:\n{captured}"));
                assert!(
                    record.contains(" WARN "),
                    "the stale cache must be reported at WARN, got: {record}"
                );
            assert!(
                run.notice.is_none(),
                "a stale cache must not produce a legacy-fallback notice"
            );
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

    /// A library fixture with `<root>/Artist/Album/track.flac` directories.
    fn library_with(albums: &[(&str, &str)]) -> TempDir {
        let library = TempDir::new().unwrap();
        for (artist, album) in albums {
            let dir = library.path().join(artist).join(album);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("01 - track.flac"), b"fake flac data").unwrap();
        }
        library
    }

    fn discover_fixture(albums: &[(&str, &str)]) -> (Config, Database, TempDir, TempDir) {
        let (mut config, db, staging) = artist_only_fixture();
        let library = library_with(albums);
        config.library.paths = vec![library.path().to_string_lossy().into_owned()];
        (config, db, staging, library)
    }

    fn search_index(soulseek: &MockClient, artist: &str, queries: &[(&str, &str)]) {
        let mut map = soulseek.search_results_by_query.lock().unwrap();
        for (query, album) in queries {
            map.insert((*query).to_string(), vec![album_result(artist, album)]);
        }
    }

    #[tokio::test]
    async fn discover_does_not_gap_fill_an_artist_filed_only_inside_another_artists_folder() {
        // End-to-end shape of the report: Apashe-tagged albums filed in the
        // Bassnectar folder. Discover must examine the folder's owner only, so
        // no Apashe query is ever issued.
        // Uniquely named fixtures: `LogCapture` keeps one process-wide window,
        // so a spelling shared with another test could satisfy the log
        // assertion below without this run emitting anything.
        let soulseek = MockClient::new();
        let (mut config, db, staging) = artist_only_fixture();
        let library = TempDir::new().unwrap();
        let folder = library.path().join("GatedFolderOwner");
        let guest = folder.join("GuestAlbum");
        let own = folder.join("OwnerAlbum");
        std::fs::create_dir_all(&guest).unwrap();
        std::fs::create_dir_all(&own).unwrap();
        write_minimal_flac_with_tags(
            &guest.join("01 - track.flac"),
            "GatedGuestArtist",
            "GuestAlbum",
        );
        write_minimal_flac_with_tags(
            &own.join("01 - track.flac"),
            "GatedFolderOwner",
            "OwnerAlbum",
        );
        config.library.paths = vec![library.path().to_string_lossy().into_owned()];
        let provider =
            FakeDiscographyProvider::with_groups(vec![release_group("missing", "Missing", "1999")]);
        let capture = crate::test_support::LogCapture::start();

        run_discover_mode_with_provider(
            &soulseek,
            &config,
            &db,
            None,
            false,
            staging.path(),
            &provider,
        )
        .await
        .unwrap();

        let queries = soulseek.search_queries.lock().unwrap().clone();
        assert!(
            queries
                .iter()
                .all(|query| !query.contains("GatedGuestArtist")),
            "an artist filed only inside another artist's folder must not be gap-filled: {queries:?}"
        );
        assert!(
            queries
                .iter()
                .any(|query| query.contains("GatedFolderOwner")),
            "the folder's own artist is still a work item: {queries:?}"
        );
        // The gate must be attributable in the run summary, not just counted: the
        // operator's workaround for an artist they want but the gate skipped is
        // to name it with `--artist`, which needs the spelling to be visible.
        let logs = capture.text();
        assert!(
            logs.contains("skipping GatedGuestArtist") && logs.contains("no folder of its own"),
            "the gated artist must be named in the log, got:\n{logs}"
        );
        // Two separate obligations: the per-artist debug line above, and the
        // operator-facing summary line the README documents. Only asserting the
        // first would stay green if the counter wiring were dropped.
        assert!(
            logs.contains("discover: 1 artist(s) skipped: no folder of their own"),
            "the run summary must carry the gate's count, got:\n{logs}"
        );
    }

    #[tokio::test]
    async fn discover_places_a_download_in_the_artist_library_folder() {
        let soulseek = MockClient::new();
        // The peer's folder is named differently from the MusicBrainz title, so
        // this also pins which of the two names the placed folder takes.
        search_index(
            &soulseek,
            "Test Artist",
            &[("Test Artist Missing", "2006 - Missing")],
        );
        // Real bytes so the placement copy has content.
        *soulseek.write_files.lock().unwrap() = true;
        let (config, db, staging, library) = discover_fixture(&[("Test Artist", "Present")]);
        let provider =
            FakeDiscographyProvider::with_groups(vec![release_group("missing", "Missing", "1999")]);
        // Placement must not depend on either of these flags.
        assert!(!config.storage.organize);
        assert!(!config.library_upgrade.enabled);

        run_discover_mode_with_provider(
            &soulseek,
            &config,
            &db,
            None,
            false,
            staging.path(),
            &provider,
        )
        .await
        .unwrap();

        assert!(
            library
                .path()
                .join("Test Artist")
                .join("Missing")
                .join("01 - track.flac")
                .exists(),
            "the album must be placed beside the artist's existing albums, in a folder named from the MusicBrainz title"
        );
        assert!(
            !library
                .path()
                .join("Test Artist")
                .join("2006 - Missing")
                .exists(),
            "the peer's folder spelling must not name the placed folder"
        );
        assert!(
            !staging.path().join("Test Artist--Missing").exists(),
            "the staging album directory is removed after a successful placement"
        );
        assert_eq!(
            db.get_album_status("Test Artist", "Missing")
                .unwrap()
                .as_deref(),
            Some("success")
        );
    }

    #[tokio::test]
    async fn discover_places_into_a_nested_library_layout() {
        let soulseek = MockClient::new();
        search_index(
            &soulseek,
            "Test Artist",
            &[("Test Artist Missing", "Missing")],
        );
        *soulseek.write_files.lock().unwrap() = true;

        let (mut config, db, staging) = artist_only_fixture();
        let library = TempDir::new().unwrap();
        let existing = library
            .path()
            .join("Metal")
            .join("Test Artist")
            .join("Present");
        std::fs::create_dir_all(&existing).unwrap();
        std::fs::write(existing.join("01 - track.flac"), b"fake flac data").unwrap();
        config.library.paths = vec![library.path().to_string_lossy().into_owned()];
        let provider =
            FakeDiscographyProvider::with_groups(vec![release_group("missing", "Missing", "1999")]);

        run_discover_mode_with_provider(
            &soulseek,
            &config,
            &db,
            None,
            false,
            staging.path(),
            &provider,
        )
        .await
        .unwrap();

        assert!(
            library
                .path()
                .join("Metal")
                .join("Test Artist")
                .join("Missing")
                .join("01 - track.flac")
                .exists(),
            "the album lands inside the artist's existing genre folder"
        );
    }

    #[tokio::test]
    async fn discover_placement_without_the_artist_placeholder_writes_beside_the_folder() {
        // README: the folder comes from `%artist%`, so a pattern that omits it
        // writes beside the artist's folder — under that folder's parent, which
        // in a nested library is the genre directory, not the library root.
        let soulseek = MockClient::new();
        search_index(
            &soulseek,
            "Test Artist",
            &[("Test Artist Missing", "Missing")],
        );
        *soulseek.write_files.lock().unwrap() = true;
        let (mut config, db, staging) = artist_only_fixture();
        let library = TempDir::new().unwrap();
        let existing = library
            .path()
            .join("Metal")
            .join("Test Artist")
            .join("Present");
        std::fs::create_dir_all(&existing).unwrap();
        std::fs::write(existing.join("01 - track.flac"), b"fake flac data").unwrap();
        config.library.paths = vec![library.path().to_string_lossy().into_owned()];
        config.storage.organize_pattern = "%album%/%track% - %title%.%ext%".to_string();
        let provider =
            FakeDiscographyProvider::with_groups(vec![release_group("missing", "Missing", "1999")]);

        run_discover_mode_with_provider(
            &soulseek,
            &config,
            &db,
            None,
            false,
            staging.path(),
            &provider,
        )
        .await
        .unwrap();

        assert!(
            library
                .path()
                .join("Metal")
                .join("Missing")
                .join("01 - track.flac")
                .exists(),
            "without %artist% the album lands beside the artist folder, under its parent"
        );
        assert!(
            !library
                .path()
                .join("Metal")
                .join("Test Artist")
                .join("Missing")
                .exists(),
            "the artist folder must not be used when the pattern omits %artist%"
        );
        assert!(
            !library.path().join("Missing").exists(),
            "the library root is not the placement base for a nested layout"
        );
    }

    #[tokio::test]
    async fn discover_placement_ignores_the_organize_flag() {
        // README: placement is unconditional in discover mode and does not
        // depend on `storage.organize`. With the flag on, the album must still be
        // placed once by the placement path rather than organized a second time
        // under the library root, which would leave a duplicate-suffixed copy.
        let soulseek = MockClient::new();
        search_index(
            &soulseek,
            "Test Artist",
            &[("Test Artist Missing", "Missing")],
        );
        *soulseek.write_files.lock().unwrap() = true;
        let (mut config, db, staging, library) = discover_fixture(&[("Test Artist", "Present")]);
        config.storage.organize = true;
        let provider =
            FakeDiscographyProvider::with_groups(vec![release_group("missing", "Missing", "1999")]);

        run_discover_mode_with_provider(
            &soulseek,
            &config,
            &db,
            None,
            false,
            staging.path(),
            &provider,
        )
        .await
        .unwrap();

        let album_dir = library.path().join("Test Artist").join("Missing");
        assert!(
            album_dir.join("01 - track.flac").exists(),
            "the album must be placed in the artist's folder"
        );
        assert!(
            !album_dir.join("01 - track (1).flac").exists(),
            "the organize block must not run after an early placement return"
        );
    }

    #[tokio::test]
    async fn placement_skips_when_the_album_folder_already_exists() {
        let client = MockClient::new();
        *client.search_results.lock().unwrap() = vec![album_result("Test Artist", "Old Album")];
        *client.write_files.lock().unwrap() = true;
        let (mut config, db, staging) = artist_only_fixture();
        let library = library_with(&[("Test Artist", "Old Album")]);
        config.library.paths = vec![library.path().to_string_lossy().into_owned()];
        config.storage.organize = false;

        let outcome = process_album(
            &client,
            "Test Artist",
            Some("Old Album"),
            false,
            &config,
            &db,
            staging.path(),
            None,
            None,
            None,
            Some(LibraryTarget::Place {
                root: library.path(),
                artist_dir: "Test Artist",
                skip_existing_album: true,
            }),
        )
        .await
        .unwrap();

        assert!(
            matches!(
                outcome,
                AlbumOutcome::Downloaded {
                    destination: DownloadDestination::Staging(_),
                    ..
                }
            ),
            "an existing album folder must keep the download in staging, got {outcome:?}"
        );
        assert!(
            staging
                .path()
                .join("Test Artist--Old Album")
                .join("01 - track.flac")
                .exists(),
            "the download stays in staging"
        );
        assert_eq!(
            std::fs::read(
                library
                    .path()
                    .join("Test Artist")
                    .join("Old Album")
                    .join("01 - track.flac")
            )
            .unwrap(),
            b"fake flac data",
            "the existing album folder is left untouched"
        );
    }

    #[tokio::test]
    async fn placement_still_writes_when_the_flag_is_off() {
        // Discover's shape: same target, flag off, and the existing folder is
        // written to (keeping files it already holds) exactly as before.
        let client = MockClient::new();
        *client.search_results.lock().unwrap() = vec![album_result("Test Artist", "Old Album")];
        *client.write_files.lock().unwrap() = true;
        let (mut config, db, staging) = artist_only_fixture();
        let library = library_with(&[("Test Artist", "Old Album")]);
        config.library.paths = vec![library.path().to_string_lossy().into_owned()];
        config.storage.organize = false;

        let outcome = process_album(
            &client,
            "Test Artist",
            Some("Old Album"),
            false,
            &config,
            &db,
            staging.path(),
            None,
            None,
            None,
            Some(LibraryTarget::Place {
                root: library.path(),
                artist_dir: "Test Artist",
                skip_existing_album: false,
            }),
        )
        .await
        .unwrap();

        assert!(
            matches!(
                outcome,
                AlbumOutcome::Downloaded {
                    destination: DownloadDestination::Library(_),
                    ..
                }
            ),
            "placement with the flag off must report the library destination, got {outcome:?}"
        );
        assert!(
            !staging.path().join("Test Artist--Old Album").exists(),
            "a placed album leaves no staging copy"
        );
    }

    #[tokio::test]
    async fn discover_placement_failure_retains_staging_and_charges_the_budget() {
        let soulseek = MockClient::new();
        search_index(
            &soulseek,
            "Alpha Artist",
            &[("Alpha Artist Missing", "Missing")],
        );
        search_index(
            &soulseek,
            "Beta Artist",
            &[("Beta Artist Missing", "Missing")],
        );
        *soulseek.write_files.lock().unwrap() = true;
        let (mut config, db, staging, library) =
            discover_fixture(&[("Alpha Artist", "Present"), ("Beta Artist", "Present")]);
        // One attempt, so a charged failure must stop the run before Beta.
        config.discover.max_cycle_downloads = 1;
        // A file where the album directory must be created makes the placement
        // copy fail with "not a directory".
        std::fs::write(
            library.path().join("Alpha Artist").join("Missing"),
            b"blocked",
        )
        .unwrap();
        let provider =
            FakeDiscographyProvider::with_groups(vec![release_group("missing", "Missing", "1999")]);

        run_discover_mode_with_provider(
            &soulseek,
            &config,
            &db,
            None,
            false,
            staging.path(),
            &provider,
        )
        .await
        .unwrap();

        assert_eq!(
            db.get_album_status("Alpha Artist", "Missing")
                .unwrap()
                .as_deref(),
            Some("failed"),
            "a placement failure records the album as failed"
        );
        assert!(
            staging.path().join("Alpha Artist--Missing").exists(),
            "staging is retained when placement fails so nothing is lost"
        );
        assert_eq!(
            soulseek.search_queries.lock().unwrap().as_slice(),
            ["Alpha Artist Missing"],
            "the failed placement charges the budget and stops the run there"
        );
    }

    #[tokio::test]
    async fn discover_placement_that_writes_nothing_still_counts_as_placed() {
        // Design contract: "Destination file already present and parseable —
        // kept; the incoming file is not copied, and the album still counts as
        // placed." A pattern without `%album%` can map every track onto a file
        // the library already holds, so the kept list comes back empty. That
        // state must complete as placed: it is what presence itself means, and
        // failing it would re-download the album on every run because no album
        // folder satisfying presence is ever created.
        let capture = crate::test_support::LogCapture::start();
        let soulseek = MockClient::new();
        search_index(
            &soulseek,
            "Test Artist",
            &[("Test Artist Missing", "Missing")],
        );
        *soulseek.write_files.lock().unwrap() = true;
        let (mut config, db, staging, library) = discover_fixture(&[("Test Artist", "Present")]);
        // Dropping %album% hides the album identity from the destination path.
        config.storage.organize_pattern = "%artist%/%track% - %title%.%ext%".to_string();
        // The staged file is "01 - track.flac", so this is the destination the
        // pattern produces: a real audio file that placement must not replace.
        write_minimal_flac(&library.path().join("Test Artist").join("01 - track.flac"));
        let provider =
            FakeDiscographyProvider::with_groups(vec![release_group("missing", "Missing", "1999")]);

        run_discover_mode_with_provider(
            &soulseek,
            &config,
            &db,
            None,
            false,
            staging.path(),
            &provider,
        )
        .await
        .unwrap();

        assert_eq!(
            db.get_album_status("Test Artist", "Missing")
                .unwrap()
                .as_deref(),
            Some("success"),
            "a placement whose destinations already hold audio still counts as placed"
        );
        assert!(
            !staging.path().join("Test Artist--Missing").exists(),
            "a placed album removes its staging copy"
        );
        // The warning is the only signal that no incoming file was written while
        // the staging copy was removed, so assert it rather than trusting the
        // branch to stay loud.
        let logs = capture.text();
        let record = logs
            .lines()
            .find(|line| line.contains("downloaded file(s) were written"))
            .unwrap_or_else(|| panic!("the all-kept placement was not reported, got:\n{logs}"));
        assert!(
            record.contains(" WARN ")
                && record.contains("Test Artist - Missing")
                && record.contains("only 0 of 1"),
            "the all-kept placement must be reported at WARN, naming the album and both counts, got: {record}"
        );

        // Convergence. No album folder that presence can see was created, so the
        // recorded success is what stops the album being downloaded again; a
        // failure record would leave it missing and re-download it every run,
        // charging one unit of the discover budget each time.
        run_discover_mode_with_provider(
            &soulseek,
            &config,
            &db,
            None,
            false,
            staging.path(),
            &provider,
        )
        .await
        .unwrap();
        assert_eq!(
            soulseek.search_queries.lock().unwrap().as_slice(),
            ["Test Artist Missing"],
            "the placed album must not be downloaded again on the next run"
        );
    }

    #[tokio::test]
    async fn discover_placement_keeps_existing_tracks_and_writes_the_rest() {
        // Partial keep: the destination already holds one track that parses as
        // audio, so that track is left alone while the missing track is written.
        // The album completes as placed and the kept destination stays
        // byte-identical.
        let soulseek = MockClient::new();
        soulseek.search_results_by_query.lock().unwrap().insert(
            "Test Artist Missing".into(),
            vec![SearchResult {
                username: "peer".into(),
                speed: 500,
                slots: 1,
                files: vec![
                    make_file(r"Test Artist\Missing\01 - track.flac", 900, 10_000_000),
                    make_file(r"Test Artist\Missing\02 - other.flac", 900, 10_000_000),
                ],
            }],
        );
        *soulseek.write_files.lock().unwrap() = true;
        let capture = crate::test_support::LogCapture::start();
        let (mut config, db, staging, library) = discover_fixture(&[("Test Artist", "Present")]);
        // A pattern without %album% is what makes a per-track collision possible:
        // with %album% the destination folder would itself satisfy presence and
        // the album would never be searched for in the first place.
        config.storage.organize_pattern = "%artist%/%track% - %title%.%ext%".to_string();
        // Quality deletion is an upgrade-mode concern; placement must ignore it
        // and leave the destination that already holds audio alone.
        config.library_upgrade.delete_lesser_quality = true;
        let artist_dir = library.path().join("Test Artist");
        let kept = artist_dir.join("01 - track.flac");
        write_minimal_flac(&kept);
        let kept_bytes = std::fs::read(&kept).unwrap();
        let provider =
            FakeDiscographyProvider::with_groups(vec![release_group("missing", "Missing", "1999")]);

        run_discover_mode_with_provider(
            &soulseek,
            &config,
            &db,
            None,
            false,
            staging.path(),
            &provider,
        )
        .await
        .unwrap();

        assert_eq!(
            db.get_album_status("Test Artist", "Missing")
                .unwrap()
                .as_deref(),
            Some("success"),
            "one written track is enough for the album to count as placed"
        );
        assert_eq!(
            std::fs::read(&kept).unwrap(),
            kept_bytes,
            "the destination that already parsed as audio must be untouched"
        );
        assert!(
            artist_dir.join("02 - other.flac").exists(),
            "the track with no existing destination must be written"
        );
        assert!(
            !staging.path().join("Test Artist--Missing").exists(),
            "a placed album removes its staging copy"
        );
        // A partial keep must be reported as well as the all-kept case: the kept
        // track's incoming copy is discarded while the album completes.
        let logs = capture.text();
        let record = logs
            .lines()
            .find(|line| line.contains("downloaded file(s) were written"))
            .unwrap_or_else(|| panic!("the partial placement was not reported, got:\n{logs}"));
        assert!(
            record.contains(" WARN ") && record.contains("only 1 of 2"),
            "a partial placement must warn with both counts, got: {record}"
        );
    }

    #[tokio::test]
    async fn a_placed_album_counts_as_present_on_the_next_discover_run() {
        // The convergence property placement exists for: once the album sits in
        // the artist's folder, the next run must treat it as present. A fresh
        // database is used for the second run, so only the library scan can
        // suppress the search — the processed-album record must not be what
        // makes this pass.
        //
        // The placed file is then rewritten with an album tag that differs from
        // the title placement used for the folder, which is the shape the real
        // path produces (the peer's tag is copied verbatim into a folder named
        // from the MusicBrainz title). Presence must survive that, or discover
        // re-downloads the album it just placed — the reported bug.
        let soulseek = MockClient::new();
        search_index(
            &soulseek,
            "Test Artist",
            &[("Test Artist Missing", "Missing")],
        );
        *soulseek.write_files.lock().unwrap() = true;
        let (config, db, staging, library) = discover_fixture(&[("Test Artist", "Present")]);
        let provider =
            FakeDiscographyProvider::with_groups(vec![release_group("missing", "Missing", "1999")]);

        run_discover_mode_with_provider(
            &soulseek,
            &config,
            &db,
            None,
            false,
            staging.path(),
            &provider,
        )
        .await
        .unwrap();
        let placed = library
            .path()
            .join("Test Artist")
            .join("Missing")
            .join("01 - track.flac");
        assert!(placed.exists(), "the first run must place the album");
        let searches_after_placing = soulseek.search_queries.lock().unwrap().len();

        // Same bytes, peer's own tag spelling inside the MusicBrainz-named folder.
        write_minimal_flac_with_tags(&placed, "Test Artist", "Missing (Remastered)");

        let fresh = Database::open_in_memory().unwrap();
        run_discover_mode_with_provider(
            &soulseek,
            &config,
            &fresh,
            None,
            false,
            staging.path(),
            &provider,
        )
        .await
        .unwrap();

        assert_eq!(
            soulseek.search_queries.lock().unwrap().len(),
            searches_after_placing,
            "the placed album must count as present however its tag is spelled, so the second run searches nothing"
        );
    }

    #[tokio::test]
    async fn ignore_processed_does_not_bypass_presence_for_a_differently_spelled_tag() {
        // `--ignore-processed` is documented as unable to re-download an album
        // the library already holds. It deletes the processed record, so the
        // library scan is the only gate left: with a differently spelled tag it
        // has to answer present, or the override re-downloads the album.
        let soulseek = MockClient::new();
        search_index(
            &soulseek,
            "Test Artist",
            &[("Test Artist Missing", "Missing")],
        );
        let (config, db, staging) = artist_only_fixture();
        let library = TempDir::new().unwrap();
        let album_dir = library.path().join("Test Artist").join("Missing");
        std::fs::create_dir_all(&album_dir).unwrap();
        write_minimal_flac_with_tags(
            &album_dir.join("01 - track.flac"),
            "Test Artist",
            "Missing (Remastered)",
        );
        let mut config = config;
        config.library.paths = vec![library.path().to_string_lossy().into_owned()];
        db.mark_album_processed("Test Artist", "Missing", "success")
            .unwrap();
        let provider =
            FakeDiscographyProvider::with_groups(vec![release_group("missing", "Missing", "1999")]);

        run_discover_mode_with_provider(
            &soulseek,
            &config,
            &db,
            None,
            true,
            staging.path(),
            &provider,
        )
        .await
        .unwrap();

        assert!(
            soulseek.search_queries.lock().unwrap().is_empty(),
            "the library scan must still suppress an album the library holds"
        );
    }

    #[tokio::test]
    async fn discover_does_not_search_an_album_placed_under_a_third_artist_spelling() {
        // The library's artist folder is spelled differently from its tags, and
        // one of its albums carries a third artist spelling in the files. That
        // album is keyed on the third spelling while the work item for the tag
        // spelling looks for it, so presence has to follow the artist folder.
        // Otherwise the album is searched and downloaded again whenever the
        // processed record is gone.
        //
        // The artist is named explicitly because the folder gate now removes it
        // from an unfiltered sweep: neither spelling owns the folder they live
        // in. Without the filter this test would assert an empty run and would
        // stop guarding the presence rule it exists for.
        let soulseek = MockClient::new();
        let (mut config, db, staging) = artist_only_fixture();
        let library = TempDir::new().unwrap();
        let first = library.path().join("Blockhead").join("First");
        let second = library.path().join("Blockhead").join("Second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        write_minimal_flac_with_tags(&first.join("01 - track.flac"), "Aesop Rock", "First");
        write_minimal_flac_with_tags(
            &second.join("01 - track.flac"),
            "Aesop Rock x Blockhead",
            "Second",
        );
        config.library.paths = vec![library.path().to_string_lossy().into_owned()];
        let provider = FakeDiscographyProvider::with_groups(vec![
            release_group("first", "First", "1999"),
            release_group("second", "Second", "2005"),
        ]);

        run_discover_mode_with_provider(
            &soulseek,
            &config,
            &db,
            Some("Aesop Rock"),
            false,
            staging.path(),
            &provider,
        )
        .await
        .unwrap();

        assert!(
            soulseek.search_queries.lock().unwrap().is_empty(),
            "both albums sit in the artist's folder and must not be searched: {:?}",
            soulseek.search_queries.lock().unwrap()
        );
    }

    #[tokio::test]
    async fn placement_uses_the_on_disk_artist_folder_not_the_tag_spelling() {
        // The scanner prefers tag metadata over the directory name, so the
        // MusicBrainz query can use a spelling the artist folder does not have.
        // The destination must still be the folder the walk saw: writing the tag
        // spelling would create a second artist tree beside the real one.
        //
        // Discovery's own sweep skips such an artist (it owns no folder under
        // the tag spelling — see `discover::select_artists`), so this rule is
        // exercised the way the operator reaches it: by naming the artist
        // explicitly, which overrides the folder gate as it overrides
        // `discover.exclude_artists`.
        let soulseek = MockClient::new();
        search_index(
            &soulseek,
            "Guns 'n' Roses",
            &[("Guns 'n' Roses Missing", "Missing")],
        );
        *soulseek.write_files.lock().unwrap() = true;
        let (mut config, db, staging) = artist_only_fixture();
        let library = TempDir::new().unwrap();
        let existing = library
            .path()
            .join("Rock")
            .join("Guns N Roses")
            .join("Present");
        std::fs::create_dir_all(&existing).unwrap();
        write_minimal_flac_with_tags(
            &existing.join("01 - track.flac"),
            "Guns 'n' Roses",
            "Present",
        );
        config.library.paths = vec![library.path().to_string_lossy().into_owned()];
        let provider =
            FakeDiscographyProvider::with_groups(vec![release_group("missing", "Missing", "1999")]);

        run_discover_mode_with_provider(
            &soulseek,
            &config,
            &db,
            Some("Guns 'n' Roses"),
            false,
            staging.path(),
            &provider,
        )
        .await
        .unwrap();

        assert!(
            library
                .path()
                .join("Rock")
                .join("Guns N Roses")
                .join("Missing")
                .join("01 - track.flac")
                .exists(),
            "the album must land inside the on-disk artist folder"
        );
        assert!(
            !library.path().join("Rock").join("Guns 'n' Roses").exists(),
            "the tag spelling must never create a second artist folder"
        );
    }

    #[tokio::test]
    async fn artist_only_mode_places_into_the_existing_artist_folder() {
        // Supersedes the old "only discover places" lock: a manual run now writes
        // into the artist folder that already exists, and never creates one.
        let soulseek = MockClient::new();
        search_index(&soulseek, "Test Artist", &[("Test Artist Album", "Album")]);
        *soulseek.write_files.lock().unwrap() = true;
        let provider =
            FakeDiscographyProvider::with_groups(vec![release_group("album", "Album", "1998")]);
        let (config, db, staging, library) = discover_fixture(&[("Test Artist", "Present")]);
        assert!(!config.storage.organize, "the organize step must stay off");

        run_artist_only_mode_with_provider(
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
            library
                .path()
                .join("Test Artist")
                .join("Album")
                .join("01 - track.flac")
                .exists(),
            "the album must be placed in the pre-existing artist folder"
        );
        assert!(
            !staging.path().join("Test Artist--Album").exists(),
            "a placed album leaves no staging copy"
        );
    }

    #[tokio::test]
    async fn artist_only_mode_without_an_artist_folder_keeps_staging() {
        let soulseek = MockClient::new();
        search_index(
            &soulseek,
            "Folderless Artist 8c1d",
            &[("Folderless Artist 8c1d Ghost", "Ghost")],
        );
        *soulseek.write_files.lock().unwrap() = true;
        let provider =
            FakeDiscographyProvider::with_groups(vec![release_group("ghost", "Ghost", "2001")]);
        let (mut config, db, staging) = artist_only_fixture();
        let library = TempDir::new().unwrap();
        config.library.paths = vec![library.path().to_string_lossy().into_owned()];
        let capture = crate::test_support::LogCapture::start();

        run_artist_only_mode_with_provider(
            &soulseek,
            "Folderless Artist 8c1d",
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
            staging
                .path()
                .join("Folderless Artist 8c1d--Ghost")
                .join("01 - track.flac")
                .exists(),
            "without an artist folder the album stays in staging"
        );
        assert!(
            !library.path().join("Folderless Artist 8c1d").exists(),
            "a manual run never creates an artist folder"
        );
        let logs = capture.text();
        assert!(
            logs.lines().any(|line| line.contains("no library folder")
                && line.contains("Folderless Artist 8c1d")),
            "the run must explain why the album stayed in staging:\n{logs}"
        );
    }

    #[tokio::test]
    async fn artist_only_mode_keeps_staging_when_the_album_folder_exists() {
        // An album folder that holds no audio is not "present" to the scan, so the
        // album is downloaded and then the existence gate keeps it in staging.
        let soulseek = MockClient::new();
        search_index(&soulseek, "Test Artist", &[("Test Artist Album", "Album")]);
        *soulseek.write_files.lock().unwrap() = true;
        let provider =
            FakeDiscographyProvider::with_groups(vec![release_group("album", "Album", "1998")]);
        let (config, db, staging, library) = discover_fixture(&[("Test Artist", "Present")]);
        let library_str = library.path().to_string_lossy().into_owned();
        std::fs::create_dir_all(library.path().join("Test Artist").join("Album")).unwrap();
        let capture = crate::test_support::LogCapture::start();

        run_artist_only_mode_with_provider(
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
            staging
                .path()
                .join("Test Artist--Album")
                .join("01 - track.flac")
                .exists(),
            "an existing album folder keeps the download in staging"
        );
        let logs = capture.text();
        assert!(
            logs.lines().any(|line| {
                line.contains("album folder already exists at")
                    && line.contains(library_str.as_str())
            }),
            "the run must name the existing album folder:\n{logs}"
        );
        assert!(
            library
                .path()
                .join("Test Artist")
                .join("Album")
                .read_dir()
                .unwrap()
                .next()
                .is_none(),
            "the existing album folder is left untouched"
        );
    }

    #[tokio::test]
    async fn legacy_artist_only_mode_skips_albums_already_present() {
        let soulseek = MockClient::new();
        soulseek.search_results_by_query.lock().unwrap().insert(
            "Test Artist".into(),
            vec![album_result("Test Artist", "Present")],
        );
        // A failing provider forces the legacy folder heuristic, which is the
        // documented opt-out and the automatic outage fallback.
        let provider = FakeDiscographyProvider::failing("offline");
        let (mut config, db, staging) = artist_only_fixture();
        let library = library_with(&[("Test Artist", "Present")]);
        config.library.paths = vec![library.path().to_string_lossy().into_owned()];

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
            run.outcomes.is_empty(),
            "an album already in the library must not be reprocessed on the legacy path: {:?}",
            run.outcomes
        );
        assert!(
            run.notice
                .as_deref()
                .is_some_and(|notice| notice.contains("already present")),
            "expected an all-present notice, got {:?}",
            run.notice
        );
        let queries = soulseek.search_queries.lock().unwrap().clone();
        assert!(
            !queries.iter().any(|query| query == "Test Artist Present"),
            "no per-album search may follow the broad artist query; queries: {queries:?}"
        );
    }

    #[tokio::test]
    async fn artist_only_manual_skips_albums_already_in_the_library() {
        let soulseek = MockClient::new();
        search_index(
            &soulseek,
            "Test Artist",
            &[("Test Artist Missing", "Missing")],
        );
        *soulseek.write_files.lock().unwrap() = true;
        let provider = FakeDiscographyProvider::with_groups(vec![
            release_group("present", "Present", "1999"),
            release_group("missing", "Missing", "2005"),
        ]);
        let (mut config, db, staging) = artist_only_fixture();
        let library = library_with(&[("Test Artist", "Present")]);
        config.library.paths = vec![library.path().to_string_lossy().into_owned()];

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
            ["Test Artist Missing"],
            "an album present in the library must not be re-downloaded"
        );
        assert_eq!(run.outcomes.len(), 1);
        assert!(matches!(run.outcomes[0].1, AlbumOutcome::Downloaded { .. }));
        assert!(
            run.notice
                .as_deref()
                .is_some_and(|notice| notice.contains("1 album(s) already present")),
            "expected an aggregate notice, got {:?}",
            run.notice
        );
    }

    #[tokio::test]
    async fn artist_only_manual_with_everything_present_issues_no_search() {
        let soulseek = MockClient::new();
        let provider =
            FakeDiscographyProvider::with_groups(vec![release_group("present", "Present", "1999")]);
        let (mut config, db, staging) = artist_only_fixture();
        let library = library_with(&[("Test Artist", "Present")]);
        config.library.paths = vec![library.path().to_string_lossy().into_owned()];

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

        assert!(soulseek.search_queries.lock().unwrap().is_empty());
        assert!(run.outcomes.is_empty());
        assert!(
            run.notice
                .as_deref()
                .is_some_and(|notice| notice.contains("already present")),
            "expected an all-present notice, got {:?}",
            run.notice
        );
    }

    #[tokio::test]
    async fn artist_only_manual_still_runs_when_the_library_path_is_missing() {
        let soulseek = MockClient::new();
        search_index(&soulseek, "Test Artist", &[("Test Artist Album", "Album")]);
        *soulseek.write_files.lock().unwrap() = true;
        let provider =
            FakeDiscographyProvider::with_groups(vec![release_group("album", "Album", "1999")]);
        let (mut config, db, staging) = artist_only_fixture();
        config.library.paths = vec!["/definitely/not/here".to_string()];

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
            ["Test Artist Album"],
            "an unusable library path must not stop artist-only manual mode"
        );
        assert!(matches!(run.outcomes[0].1, AlbumOutcome::Downloaded { .. }));
        assert!(
            staging
                .path()
                .join("Test Artist--Album")
                .join("01 - track.flac")
                .exists(),
            "an unusable library path must leave the download in staging"
        );
    }

    #[tokio::test]
    async fn manual_mode_places_an_explicit_album_into_the_existing_artist_folder() {
        let client = MockClient::new();
        *client.search_results.lock().unwrap() = vec![album_result("Test Artist", "New Album")];
        *client.write_files.lock().unwrap() = true;
        let (mut config, db, staging) = artist_only_fixture();
        let library = library_with(&[("Test Artist", "Present")]);
        config.library.paths = vec![library.path().to_string_lossy().into_owned()];
        config.discography.enabled = false;

        run_manual_mode(
            &client,
            Some("Test Artist"),
            Some("New Album"),
            false,
            &config,
            &db,
        )
        .await
        .expect("manual album mode must complete");

        assert!(
            library
                .path()
                .join("Test Artist")
                .join("New Album")
                .join("01 - track.flac")
                .exists(),
            "the explicit album must be placed in the existing artist folder"
        );
        assert!(
            !staging.path().join("Test Artist--New Album").exists(),
            "a placed album leaves no staging copy"
        );
    }

    #[tokio::test]
    async fn manual_mode_with_an_explicit_album_does_not_walk_the_library() {
        let client = MockClient::new();
        *client.search_results.lock().unwrap() = vec![album_result("Test Artist", "New Album")];
        *client.write_files.lock().unwrap() = true;
        let (mut config, db, _staging) = artist_only_fixture();
        let library = library_with(&[("Test Artist", "Present")]);
        config.library.paths = vec![library.path().to_string_lossy().into_owned()];
        config.discography.enabled = false;
        let capture = crate::test_support::LogCapture::start();

        run_manual_mode(
            &client,
            Some("Test Artist"),
            Some("New Album"),
            false,
            &config,
            &db,
        )
        .await
        .expect("manual album mode must complete");

        // The scan announces its roots, and this fixture's library root is unique
        // to this test, so the assertion cannot be satisfied or broken by another
        // test's scan.
        let root = library.path().to_str().unwrap();
        let logs = capture.text();
        assert!(
            !logs
                .lines()
                .any(|line| line.contains("Library scan starting") && line.contains(root)),
            "the explicit-album form must resolve the artist folder without walking:\n{logs}"
        );
    }

    #[tokio::test]
    async fn manual_mode_with_an_explicit_album_keeps_staging_when_the_album_folder_exists() {
        let client = MockClient::new();
        *client.search_results.lock().unwrap() = vec![album_result("Test Artist", "Old Album")];
        *client.write_files.lock().unwrap() = true;
        let (mut config, db, staging) = artist_only_fixture();
        let library = library_with(&[("Test Artist", "Old Album")]);
        config.library.paths = vec![library.path().to_string_lossy().into_owned()];
        config.discography.enabled = false;

        run_manual_mode(
            &client,
            Some("Test Artist"),
            Some("Old Album"),
            false,
            &config,
            &db,
        )
        .await
        .expect("manual album mode must complete");

        assert!(
            staging
                .path()
                .join("Test Artist--Old Album")
                .join("01 - track.flac")
                .exists(),
            "an existing album folder keeps the download in staging"
        );
        assert_eq!(
            std::fs::read(
                library
                    .path()
                    .join("Test Artist")
                    .join("Old Album")
                    .join("01 - track.flac")
            )
            .unwrap(),
            b"fake flac data",
            "the existing album folder is left untouched"
        );
    }

    #[tokio::test]
    async fn manual_mode_places_once_when_organize_is_on() {
        // Spec decision 10: placement runs regardless of `storage.organize` and
        // returns before the organize step. The library folder is spelled in lower
        // case, so only placement - which writes the on-disk spelling verbatim - can
        // put the files there; organize would write `Test Artist` instead.
        let client = MockClient::new();
        *client.search_results.lock().unwrap() = vec![album_result("Test Artist", "New Album")];
        *client.write_files.lock().unwrap() = true;
        let (mut config, db, staging) = artist_only_fixture();
        let library = library_with(&[("test artist", "Present")]);
        // Placement writes the on-disk spelling, organize would write the tag spelling;
        // on a case-insensitive filesystem the two are the same directory, so the
        // negative assertion below describes the Linux target.
        config.library.paths = vec![library.path().to_string_lossy().into_owned()];
        config.storage.organize = true;
        config.discography.enabled = false;

        run_manual_mode(
            &client,
            Some("Test Artist"),
            Some("New Album"),
            false,
            &config,
            &db,
        )
        .await
        .expect("manual album mode must complete");

        let album_dir = library.path().join("test artist").join("New Album");
        assert!(
            album_dir.join("01 - track.flac").exists(),
            "placement must write the artist folder that exists, spelling and all"
        );
        assert!(
            !library.path().join("Test Artist").exists(),
            "the organize step must not run after an early placement return"
        );
        assert!(
            !staging.path().join("Test Artist--New Album").exists(),
            "a placed album leaves no staging copy"
        );
    }

    #[tokio::test]
    async fn discover_downloads_only_albums_the_library_lacks() {
        let soulseek = MockClient::new();
        search_index(&soulseek, "Test Artist", &[("Test Artist Newer", "Newer")]);
        let provider = FakeDiscographyProvider::with_groups(vec![
            release_group("old", "Older", "1999"),
            release_group("new", "Newer", "2005"),
        ]);
        let (config, db, staging, _library) = discover_fixture(&[("Test Artist", "Older")]);

        run_discover_mode_with_provider(
            &soulseek,
            &config,
            &db,
            None,
            false,
            staging.path(),
            &provider,
        )
        .await
        .unwrap();

        assert_eq!(
            soulseek.search_queries.lock().unwrap().as_slice(),
            ["Test Artist Newer"],
            "an album already in the library must never be searched"
        );
    }

    #[tokio::test]
    async fn discover_does_not_search_an_album_placed_under_a_differently_spelled_tag() {
        // The reported bug, end to end. The album already sits in the artist's
        // library folder, which the write path named from the MusicBrainz
        // title, while the files inside carry the peer's own album tag. The
        // presence check must still see it: no search is issued, so no
        // bandwidth is spent on an album the library already holds.
        let soulseek = MockClient::new();
        let (mut config, db, staging) = artist_only_fixture();
        let library = TempDir::new().unwrap();
        let album_dir = library
            .path()
            .join("Test Artist")
            .join("I Heard It\u{2019}s a Mess There Too");
        std::fs::create_dir_all(&album_dir).unwrap();
        write_minimal_flac_with_tags(
            &album_dir.join("01 - track.flac"),
            "Test Artist",
            "I Heard It's A Mess There Too",
        );
        config.library.paths = vec![library.path().to_string_lossy().into_owned()];
        let provider = FakeDiscographyProvider::with_groups(vec![release_group(
            "mess",
            "I Heard It\u{2019}s a Mess There Too",
            "2025",
        )]);

        run_discover_mode_with_provider(
            &soulseek,
            &config,
            &db,
            None,
            false,
            staging.path(),
            &provider,
        )
        .await
        .unwrap();

        assert!(
            soulseek.search_queries.lock().unwrap().is_empty(),
            "an album the library already holds must never be searched"
        );
    }

    #[tokio::test]
    async fn discover_skips_an_album_with_no_candidates_without_charging_the_budget() {
        let soulseek = MockClient::new();
        search_index(
            &soulseek,
            "Test Artist",
            &[("Test Artist Second", "Second")],
        );
        let provider = FakeDiscographyProvider::with_groups(vec![
            release_group("first", "First", "1999"),
            release_group("second", "Second", "2005"),
        ]);
        let (mut config, db, staging, _library) = discover_fixture(&[("Test Artist", "Present")]);
        config.discover.max_cycle_downloads = 1;

        run_discover_mode_with_provider(
            &soulseek,
            &config,
            &db,
            None,
            false,
            staging.path(),
            &provider,
        )
        .await
        .unwrap();

        assert!(
            soulseek
                .search_queries
                .lock()
                .unwrap()
                .iter()
                .any(|query| query == "Test Artist Second"),
            "the empty first album must not consume the only budget slot; queries: {:?}",
            soulseek.search_queries.lock().unwrap()
        );
    }

    #[tokio::test]
    async fn discover_stops_at_the_budget_and_leaves_later_artists_unexamined() {
        let soulseek = MockClient::new();
        search_index(
            &soulseek,
            "Alpha Artist",
            &[("Alpha Artist Missing", "Missing")],
        );
        let (mut config, db, staging, _library) =
            discover_fixture(&[("Alpha Artist", "Present"), ("Beta Artist", "Present")]);
        config.discover.max_cycle_downloads = 1;
        let provider =
            FakeDiscographyProvider::with_groups(vec![release_group("missing", "Missing", "1999")]);

        run_discover_mode_with_provider(
            &soulseek,
            &config,
            &db,
            None,
            false,
            staging.path(),
            &provider,
        )
        .await
        .unwrap();

        assert_eq!(
            soulseek.search_queries.lock().unwrap().as_slice(),
            ["Alpha Artist Missing"],
            "the budget must stop the run before the second artist"
        );
    }

    #[tokio::test]
    async fn discover_excludes_configured_artists_before_any_lookup() {
        let soulseek = MockClient::new();
        // The album is deliberately absent from the library, so if the artist
        // were not excluded a search for it would be issued. The earlier fixture
        // had the album present, which made the test pass with or without the
        // exclusion logic.
        let (mut config, db, staging, _library) =
            discover_fixture(&[("Various Artists", "Present")]);
        config.discover.exclude_artists = vec!["Various Artists".to_string()];
        let provider =
            FakeDiscographyProvider::with_groups(vec![release_group("missing", "Missing", "1999")]);

        run_discover_mode_with_provider(
            &soulseek,
            &config,
            &db,
            None,
            false,
            staging.path(),
            &provider,
        )
        .await
        .unwrap();

        assert!(
            soulseek.search_queries.lock().unwrap().is_empty(),
            "an excluded artist must not be searched even when it has a missing album"
        );
    }

    #[tokio::test]
    async fn discover_does_not_charge_the_budget_for_already_processed_albums() {
        let soulseek = MockClient::new();
        search_index(
            &soulseek,
            "Test Artist",
            &[("Test Artist Second", "Second")],
        );
        let provider = FakeDiscographyProvider::with_groups(vec![
            release_group("first", "First", "1999"),
            release_group("second", "Second", "2005"),
        ]);
        let (mut config, db, staging, _library) = discover_fixture(&[("Test Artist", "Present")]);
        config.discover.max_cycle_downloads = 1;
        // The first album never reaches the download stage: a success record
        // already exists, so process_album returns Skipped. It must not spend
        // the only budget slot, or the run would stop before the second album
        // and never advance on any later run either.
        db.mark_album_processed("Test Artist", "First", "success")
            .unwrap();

        run_discover_mode_with_provider(
            &soulseek,
            &config,
            &db,
            None,
            false,
            staging.path(),
            &provider,
        )
        .await
        .unwrap();

        assert!(
            soulseek
                .search_queries
                .lock()
                .unwrap()
                .iter()
                .any(|query| query == "Test Artist Second"),
            "an already-processed album must not consume the budget; queries: {:?}",
            soulseek.search_queries.lock().unwrap()
        );
    }

    #[tokio::test]
    async fn discover_narrows_to_the_requested_artist() {
        let soulseek = MockClient::new();
        search_index(
            &soulseek,
            "Beta Artist",
            &[("Beta Artist Missing", "Missing")],
        );
        let (config, db, staging, _library) =
            discover_fixture(&[("Alpha Artist", "Present"), ("Beta Artist", "Present")]);
        let provider =
            FakeDiscographyProvider::with_groups(vec![release_group("missing", "Missing", "1999")]);

        run_discover_mode_with_provider(
            &soulseek,
            &config,
            &db,
            Some("Beta Artist"),
            false,
            staging.path(),
            &provider,
        )
        .await
        .unwrap();

        assert_eq!(
            soulseek.search_queries.lock().unwrap().as_slice(),
            ["Beta Artist Missing"]
        );
    }

    #[tokio::test]
    async fn discover_rejects_a_filter_artist_absent_from_the_library() {
        let soulseek = MockClient::new();
        let (config, db, staging, _library) = discover_fixture(&[("Alpha Artist", "Present")]);
        let provider = FakeDiscographyProvider::with_groups(vec![]);

        let error = run_discover_mode_with_provider(
            &soulseek,
            &config,
            &db,
            Some("Nobody"),
            false,
            staging.path(),
            &provider,
        )
        .await
        .unwrap_err();

        assert!(matches!(error, SeakarrError::Config(_)), "got {error:?}");
        assert!(error.to_string().contains("not found in the library"));
    }

    #[tokio::test]
    async fn discover_skips_an_unresolved_artist_without_searching() {
        let soulseek = MockClient::new();
        let (config, db, staging, _library) = discover_fixture(&[("Test Artist", "Present")]);
        let provider = FakeDiscographyProvider::unresolvable();

        run_discover_mode_with_provider(
            &soulseek,
            &config,
            &db,
            None,
            false,
            staging.path(),
            &provider,
        )
        .await
        .unwrap();

        assert!(
            soulseek.search_queries.lock().unwrap().is_empty(),
            "an unresolved artist must never trigger a broad artist search"
        );
    }

    #[tokio::test]
    async fn discover_skips_a_recorded_failure_without_calling_the_provider() {
        let soulseek = MockClient::new();
        let (config, db, staging, _library) = discover_fixture(&[("Test Artist", "Present")]);
        db.upsert_discography_failure(&crate::db::DiscographyFailureEntry {
            artist_key: "test artist".to_string(),
            failure_kind: "unresolved".to_string(),
            reason: "no candidate matches".to_string(),
            recorded_at: chrono::Utc::now().timestamp(),
        })
        .unwrap();
        let provider = FakeDiscographyProvider::unresolvable();

        run_discover_mode_with_provider(
            &soulseek,
            &config,
            &db,
            None,
            false,
            staging.path(),
            &provider,
        )
        .await
        .unwrap();

        assert_eq!(
            provider.calls(),
            0,
            "a recorded failure must spare the MusicBrainz lookup"
        );
    }

    #[tokio::test]
    async fn an_explicit_artist_bypasses_a_recorded_failure() {
        let soulseek = MockClient::new();
        let (config, db, staging, _library) = discover_fixture(&[("Test Artist", "Present")]);
        db.upsert_discography_failure(&crate::db::DiscographyFailureEntry {
            artist_key: "test artist".to_string(),
            failure_kind: "unresolved".to_string(),
            reason: "no candidate matches".to_string(),
            recorded_at: chrono::Utc::now().timestamp(),
        })
        .unwrap();
        let provider = FakeDiscographyProvider::unresolvable();

        run_discover_mode_with_provider(
            &soulseek,
            &config,
            &db,
            Some("Test Artist"),
            false,
            staging.path(),
            &provider,
        )
        .await
        .unwrap();

        assert!(
            provider.calls() > 0,
            "asking for an artist by name must reach MusicBrainz"
        );
    }

    #[tokio::test]
    async fn a_cached_skip_neither_increments_nor_resets_the_provider_failure_breaker() {
        // A cached skip makes no request, so it is no evidence about provider
        // health. Interleaving it between failures distinguishes the two halves:
        // if it reset the count the run would stop short of the limit and not
        // abort. Artists are examined alphabetically, so the recorded failure sits
        // between two failures.
        let soulseek = MockClient::new();
        let (config, db, staging, _library) = discover_fixture(&[
            ("Alpha Artist", "Present"),
            ("Beta Artist", "Present"),
            ("Delta Artist", "Present"),
            ("Gamma Artist", "Present"),
        ]);
        db.upsert_discography_failure(&crate::db::DiscographyFailureEntry {
            artist_key: "beta artist".to_string(),
            failure_kind: "unresolved".to_string(),
            reason: "no candidate matches".to_string(),
            recorded_at: chrono::Utc::now().timestamp(),
        })
        .unwrap();
        let provider = FakeDiscographyProvider::failing("connection reset");

        let error = run_discover_mode_with_provider(
            &soulseek,
            &config,
            &db,
            None,
            false,
            staging.path(),
            &provider,
        )
        .await
        .unwrap_err();

        assert!(
            matches!(error, SeakarrError::MusicBrainz(_)),
            "a cached skip must not reset the breaker, got {error:?}"
        );
        assert_eq!(
            provider.calls(),
            3,
            "three artists must be asked about; the recorded one must be skipped"
        );
    }

    #[tokio::test]
    async fn a_cached_skip_is_reported_in_the_summary() {
        // The counter and its notice are the only way to tell a cached skip apart
        // from a failure this run observed, so nothing else would catch a missing
        // push in the discover loop.
        //
        // `LogCapture` is process-wide and records every event emitted by any
        // concurrently running test, so both the positive and the negative
        // assertion have to key on values only this test can produce. Two
        // distinctively named artists give the notice a count of 2 that no other
        // test produces, and the names make the loop's own log lines and the
        // fresh-failure notice attributable to this run.
        let capture = crate::test_support::LogCapture::start();
        let soulseek = MockClient::new();
        let (config, db, staging, _library) = discover_fixture(&[
            ("Cached Skip Alpha", "Present"),
            ("Cached Skip Beta", "Present"),
        ]);
        for artist_key in ["cached skip alpha", "cached skip beta"] {
            db.upsert_discography_failure(&crate::db::DiscographyFailureEntry {
                artist_key: artist_key.to_string(),
                failure_kind: "unresolved".to_string(),
                reason: "no candidate matches".to_string(),
                recorded_at: chrono::Utc::now().timestamp(),
            })
            .unwrap();
        }
        let provider = FakeDiscographyProvider::unresolvable();

        run_discover_mode_with_provider(
            &soulseek,
            &config,
            &db,
            None,
            false,
            staging.path(),
            &provider,
        )
        .await
        .unwrap();

        let logs = capture.text();
        for name in ["Cached Skip Alpha", "Cached Skip Beta"] {
            assert!(
                logs.contains(&format!(
                    "{name}: skipped, resolution failure recorded earlier"
                )),
                "the loop must take the cached-skip branch for {name}, got:\n{logs}"
            );
        }
        assert!(
            logs.contains("discover: 2 artist(s) skipped from cached resolution failures"),
            "the summary must report both cached skips, got:\n{logs}"
        );
        for name in ["Cached Skip Alpha", "Cached Skip Beta"] {
            assert!(
                !logs.contains(&format!("unresolved on MusicBrainz: {name}")),
                "a cached skip must not be counted as a failure observed this run, got:\n{logs}"
            );
        }
    }

    #[tokio::test]
    async fn discover_aborts_after_three_consecutive_provider_failures() {
        let soulseek = MockClient::new();
        let (config, db, staging, _library) = discover_fixture(&[
            ("Alpha Artist", "Present"),
            ("Beta Artist", "Present"),
            ("Gamma Artist", "Present"),
        ]);
        let provider = FakeDiscographyProvider::failing("connection reset");

        let error = run_discover_mode_with_provider(
            &soulseek,
            &config,
            &db,
            None,
            false,
            staging.path(),
            &provider,
        )
        .await
        .unwrap_err();

        assert!(
            matches!(error, SeakarrError::MusicBrainz(_)),
            "expected a MusicBrainz error, got {error:?}"
        );
    }

    #[test]
    fn only_database_failures_charge_the_budget_after_an_error() {
        // A post-download bookkeeping failure is a database error and means the
        // transfer happened; a search-stage failure is not and means it did not.
        assert!(charges_after_error(&SeakarrError::Database(
            rusqlite::Error::InvalidQuery
        )));
        assert!(!charges_after_error(&SeakarrError::Client(
            "search failed".into()
        )));
        assert!(!charges_after_error(&SeakarrError::Download(
            "cancelled by user".into()
        )));
    }

    #[tokio::test]
    async fn discover_breaker_counts_stale_cache_fallbacks() {
        let soulseek = MockClient::new();
        let (config, db, staging, _library) = discover_fixture(&[
            ("Alpha Artist", "Present"),
            ("Beta Artist", "Present"),
            ("Gamma Artist", "Present"),
        ]);
        // Every artist has a warm but stale cache, and the provider always
        // fails, so each artist falls back to its cache. Those failures are
        // still failures: without counting them the run would walk the entire
        // library issuing one failing request per artist.
        for artist in ["Alpha Artist", "Beta Artist", "Gamma Artist"] {
            db.upsert_discography_cache(&crate::db::DiscographyCacheEntry {
                artist_key: crate::discography::normalize_catalog_key(artist),
                artist_mbid: "11111111-1111-1111-1111-111111111111".to_string(),
                canonical_artist: artist.to_string(),
                fetched_at: 1,
                release_groups_json: serde_json::to_string(&vec![release_group(
                    "present", "Present", "1999",
                )])
                .unwrap(),
            })
            .unwrap();
        }
        let provider = FakeDiscographyProvider::failing("offline");

        let error = run_discover_mode_with_provider(
            &soulseek,
            &config,
            &db,
            None,
            false,
            staging.path(),
            &provider,
        )
        .await
        .unwrap_err();

        assert!(
            matches!(error, SeakarrError::MusicBrainz(_)),
            "an outage hidden behind a stale cache must still trip the breaker, got {error:?}"
        );
    }

    #[tokio::test]
    async fn discover_breaker_ignores_a_stale_cache_whose_refresh_stopped_resolving() {
        // MusicBrainz answered the request; it simply no longer resolves this
        // artist. That is not an outage, so the breaker must not abort, and the
        // cached work list must still be used.
        let soulseek = MockClient::new();
        let (config, db, staging, _library) = discover_fixture(&[
            ("Alpha Artist", "Present"),
            ("Beta Artist", "Present"),
            ("Gamma Artist", "Present"),
        ]);
        for artist in ["Alpha Artist", "Beta Artist", "Gamma Artist"] {
            db.upsert_discography_cache(&crate::db::DiscographyCacheEntry {
                artist_key: crate::discography::normalize_catalog_key(artist),
                artist_mbid: "11111111-1111-1111-1111-111111111111".to_string(),
                canonical_artist: artist.to_string(),
                fetched_at: 1,
                release_groups_json: serde_json::to_string(&vec![release_group(
                    "present", "Present", "1999",
                )])
                .unwrap(),
            })
            .unwrap();
        }
        let provider = FakeDiscographyProvider::unresolvable();

        let result = run_discover_mode_with_provider(
            &soulseek,
            &config,
            &db,
            None,
            false,
            staging.path(),
            &provider,
        )
        .await;

        assert!(
            result.is_ok(),
            "an unresolvable artist is not an outage and must not trip the breaker: {result:?}"
        );
    }

    #[tokio::test]
    async fn discover_requires_an_enabled_discography() {
        let soulseek = MockClient::new();
        let (mut config, db, staging, _library) = discover_fixture(&[("Test Artist", "Present")]);
        config.discography.enabled = false;
        let provider = FakeDiscographyProvider::with_groups(vec![]);

        let error = run_discover_mode_with_provider(
            &soulseek,
            &config,
            &db,
            None,
            false,
            staging.path(),
            &provider,
        )
        .await
        .unwrap_err();

        assert!(
            error.to_string().contains("discography.enabled"),
            "got {error}"
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
            AlbumOutcome::NoCandidates { reason } => assert_eq!(reason, "no results found"),
            other => panic!("Expected AlbumOutcome::NoCandidates, got: {other:?}"),
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
            AlbumOutcome::NoCandidates { reason } => {
                assert!(
                    reason.contains("no results passed filters"),
                    "Expected 'no results passed filters', got: {reason}"
                );
            }
            other => panic!("Expected AlbumOutcome::NoCandidates, got: {other:?}"),
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
        assert!(
            matches!(
                result.unwrap(),
                AlbumOutcome::Downloaded { track_count: 2, .. }
            ),
            "the album must complete with its downloaded track count"
        );

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
        assert!(
            matches!(
                result.unwrap(),
                AlbumOutcome::Downloaded { track_count: 2, .. }
            ),
            "the album must complete with its downloaded track count"
        );

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
            AlbumOutcome::NoCandidates { reason } => assert_eq!(reason, "no results found"),
            other => panic!("Expected AlbumOutcome::NoCandidates, got: {other:?}"),
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
        assert!(
            matches!(
                result.unwrap(),
                AlbumOutcome::Downloaded { track_count: 1, .. }
            ),
            "the album must complete with its downloaded track count"
        );

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
        assert!(
            matches!(
                result.unwrap(),
                AlbumOutcome::Downloaded { track_count: 1, .. }
            ),
            "the album must complete with its downloaded track count"
        );

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
        let capture = crate::test_support::LogCapture::start();

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
        // A distinctive artist and album spelling: the sibling test
        // `test_title_search_fallback_when_primary_empty` drives the same code
        // path with the same fixture under the plain "Adele"/"25" names, and a
        // test that is not capturing still contributes records to an open
        // capture window. These values keep the guard out of its reach.
        let tmp = TempDir::new().unwrap();
        let album_dir = tmp.path().join("Adele Probe").join("25 Probe");
        std::fs::create_dir_all(&album_dir).unwrap();
        std::fs::write(album_dir.join("01 - I Miss You.mp3"), b"fake mp3").unwrap();
        std::fs::write(album_dir.join("02 - Hello.mp3"), b"fake mp3").unwrap();
        config.library.paths = vec![tmp.path().to_string_lossy().into()];

        let db = Database::open_in_memory().unwrap();
        let staging = TempDir::new().unwrap();

        let result = process_album(
            client.as_ref() as &dyn crate::client::SoulseekClient,
            "Adele Probe",
            Some("25 Probe"),
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
        assert!(
            matches!(
                result.unwrap(),
                AlbumOutcome::Downloaded { track_count: 2, .. }
            ),
            "the album must complete with its downloaded track count"
        );

        // Assert on the record itself, not on the buffer: another test's record
        // could hold either half of this, while a record naming this artist and
        // album can only come from this run. The lookup also requires this test's
        // artist, so a sibling's line for the plain "Adele" album can never be
        // selected and then reported as a mismatch.
        let captured = capture.text();
        let record = captured
            .lines()
            .find(|line| {
                line.contains("falling back to track-title search") && line.contains("Adele Probe")
            })
            .unwrap_or_else(|| panic!("expected a contextual fallback log line, got:\n{captured}"));
        assert!(
            record.contains("Adele Probe — 25 Probe"),
            "the fallback record must name the artist and album it refers to, got:\n{record}"
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
            Some(2), // library_track_count: the library album has 2 tracks
            Some(LibraryTarget::Upgrade {
                root: target.path(),
                expected_tracks: 2,
            }),
        )
        .await;
        assert!(result.is_ok());
        // The upgrade path passes `Library(outcome.album_dir)`. Asserting only the
        // track count would leave this arm free to report `(kept in staging)` and
        // skip staging removal for an album that was in fact copied in.
        assert_eq!(
            result.unwrap(),
            AlbumOutcome::Downloaded {
                track_count: 2,
                destination: DownloadDestination::Library(
                    target.path().join("Test Artist/Test Album")
                ),
            },
            "album must complete: 2 matching tracks downloaded, peer folder size (5) is irrelevant"
        );
    }

    #[tokio::test]
    async fn test_library_upgrade_rejects_an_incomplete_download() {
        // The rejecting half of the completeness gate: the library album has
        // two files needing replacement and the peer delivers one, so nothing
        // may be copied and the album is recorded failed.
        let client = Arc::new(MockClient::new());
        *client.search_results.lock().unwrap() = vec![SearchResult {
            username: "peer".into(),
            speed: 500,
            slots: 1,
            files: vec![make_file(
                r"Test Artist\Test Album\01 - track.flac",
                900,
                10_000_000,
            )],
        }];
        *client.write_files.lock().unwrap() = true;

        let mut config = make_test_config();
        config.library_upgrade.enabled = true;
        config.library_upgrade.delete_lesser_quality = false;
        // The peer-track-count filter is a separate mechanism that would reject
        // this single-file peer before any download; disable it so the
        // completeness gate is what decides.
        config.filters.peer_track_count = false;
        let db = Database::open_in_memory().unwrap();
        let staging = TempDir::new().unwrap();
        let target = TempDir::new().unwrap();

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
            Some(2), // the library album has 2 files needing upgrade
            Some(LibraryTarget::Upgrade {
                root: target.path(),
                expected_tracks: 2,
            }),
        )
        .await
        .unwrap();

        match result {
            AlbumOutcome::Failed { reason } => assert!(
                reason.contains("incomplete download"),
                "expected the completeness gate to reject, got: {reason}"
            ),
            other => panic!("expected Failed, got: {other:?}"),
        }
        assert_eq!(
            db.get_album_status("Test Artist", "Test Album")
                .unwrap()
                .as_deref(),
            Some("failed")
        );
        assert!(
            !target
                .path()
                .join("Test Artist")
                .join("Test Album")
                .join("01 - track.flac")
                .exists(),
            "nothing may be copied when the completeness gate rejects"
        );
    }

    #[tokio::test]
    async fn an_upgrade_may_deliver_only_the_files_that_need_replacing() {
        // The library already holds its own track 1, so a peer sharing only the
        // non-conforming files is a legitimate upgrade source: the anchor half of
        // the completeness rule belongs to the library-write gate, which the
        // upgrade path does not use (it compares against `expected_tracks`).
        // Applying the anchor here would refuse the set before the download and
        // cost auto mode its partial-repair source.
        let client = Arc::new(MockClient::new());
        *client.search_results.lock().unwrap() = vec![SearchResult {
            username: "peer".into(),
            speed: 500,
            slots: 1,
            files: vec![
                make_file(r"Test Artist\Test Album\03 - three.flac", 900, 10_000_000),
                make_file(r"Test Artist\Test Album\04 - four.flac", 900, 10_000_000),
                make_file(r"Test Artist\Test Album\05 - five.flac", 900, 10_000_000),
            ],
        }];
        *client.write_files.lock().unwrap() = true;

        let mut config = make_test_config();
        config.filters.min_tracks = 3; // the count half must still apply
        config.library_upgrade.enabled = true;
        config.library_upgrade.delete_lesser_quality = false;
        config.filters.peer_track_count = false;
        let db = Database::open_in_memory().unwrap();
        let staging = TempDir::new().unwrap();
        let target = TempDir::new().unwrap();

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
            Some(3), // three library files need replacing
            Some(LibraryTarget::Upgrade {
                root: target.path(),
                expected_tracks: 3,
            }),
        )
        .await
        .unwrap();

        assert!(
            matches!(&result, AlbumOutcome::Downloaded { .. }),
            "the upgrade must proceed from a peer that shares only the files the library needs, got {result:?}"
        );
        assert!(
            target
                .path()
                .join("Test Artist")
                .join("Test Album")
                .join("04 - four.flac")
                .exists(),
            "the delivered replacements must be copied into the library"
        );
    }

    #[tokio::test]
    async fn refused_download_leaves_no_staging_copy() {
        // A completeness refusal means the set is never written to the library,
        // so its staged files must go with it. Leaving them behind is how the
        // refused albums accumulated in `storage.staging_dir` (the reported 2 GiB
        // of "incomplete download, library placement skipped" albums such as
        // Cyantific "Archive 1" and Danny Byrd "Atomic Funk").
        let client = Arc::new(MockClient::new());
        *client.search_results.lock().unwrap() = vec![SearchResult {
            username: "peer".into(),
            speed: 500,
            slots: 1,
            files: vec![make_file(
                r"Test Artist\Test Album\01 - track.flac",
                900,
                10_000_000,
            )],
        }];
        *client.write_files.lock().unwrap() = true;

        let mut config = make_test_config();
        config.library_upgrade.enabled = true;
        config.library_upgrade.delete_lesser_quality = false;
        // The peer-track-count filter is a separate mechanism that would reject
        // this single-file peer before any download; disable it so the
        // completeness gate is what decides.
        config.filters.peer_track_count = false;
        let db = Database::open_in_memory().unwrap();
        let staging = TempDir::new().unwrap();
        let target = TempDir::new().unwrap();

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
            Some(2), // the library album has 2 files needing upgrade
            Some(LibraryTarget::Upgrade {
                root: target.path(),
                expected_tracks: 2,
            }),
        )
        .await
        .unwrap();

        assert!(
            matches!(&result, AlbumOutcome::Failed { reason } if reason.contains("incomplete download")),
            "expected the completeness gate to reject, got {result:?}"
        );
        assert!(
            !staging.path().join("Test Artist--Test Album").exists(),
            "a refused download must not be left in the staging directory"
        );
        // The serving peer is demoted at the album level as well as credited for
        // the track it did deliver, so a peer that serves fragments sinks in the
        // ranking instead of being re-picked every cycle.
        let reputation = db.get_reputation_map().unwrap();
        let peer = reputation
            .get("peer")
            .expect("the serving peer must be recorded");
        assert_eq!(
            peer.total_downloads, 2,
            "one delivered track plus the album-level failure"
        );
        assert_eq!(
            peer.successful, 1,
            "only the delivered track was usable audio"
        );
    }

    #[test]
    fn discard_refused_staging_removes_our_tree_and_tolerates_a_missing_directory() {
        // Supplementary guard for the helper the three refusal arms call: it must
        // clear a staged tree (including disc subdirectories) and must stay quiet
        // when a refusal happens with nothing staged, which is not a failure.
        let root = TempDir::new().unwrap();
        discard_refused_staging(&root.path().join("never-created"), &[]);

        let album = root.path().join("Test Artist--Test Album");
        std::fs::create_dir_all(album.join("disc 1")).unwrap();
        let ours = album.join("disc 1/01 - Track.flac");
        std::fs::write(&ours, b"not audio").unwrap();
        discard_refused_staging(&album, std::slice::from_ref(&ours));
        assert!(
            !album.exists(),
            "a refused album's staged tree must be removed, disc subdirectories included"
        );
    }

    #[test]
    fn discard_refused_staging_keeps_a_foreign_file_that_repeats_our_basename() {
        // Ownership must be by staged path, not by basename: another album on the
        // same staging name routinely holds the same track names, one flat and one
        // under a disc folder. A basename test accepts `CD 01/01 - Track.flac` as
        // ours and deletes it with the tree.
        let root = TempDir::new().unwrap();
        let album = root.path().join("Test Artist--Test Album");
        std::fs::create_dir_all(album.join("CD 01")).unwrap();
        let ours = album.join("01 - Track.flac");
        let foreign = album.join("CD 01/01 - Track.flac");
        std::fs::write(&ours, b"not audio").unwrap();
        std::fs::write(&foreign, b"not audio").unwrap();

        discard_refused_staging(&album, std::slice::from_ref(&ours));

        assert!(
            foreign.exists(),
            "a file this run did not stage must survive even when its basename matches one of ours"
        );
        assert!(!ours.exists(), "our own file must be removed");
        assert!(
            album.exists(),
            "the directory holding a foreign file must be left in place"
        );
    }

    #[test]
    fn discard_refused_staging_reports_a_path_that_cannot_be_removed() {
        // A staging path that is not a removable directory (here a plain file) must
        // be reported, not silently ignored: the warning names the path itself when
        // no entry can be read from it.
        let root = TempDir::new().unwrap();
        let not_a_dir = root.path().join("Test Artist--Test Album");
        std::fs::write(&not_a_dir, b"not audio").unwrap();

        discard_refused_staging(&not_a_dir, &[]);

        assert!(
            not_a_dir.exists(),
            "a path this run did not stage must survive, even when it is not a directory"
        );
    }

    #[test]
    fn discard_refused_staging_keeps_a_directory_holding_another_albums_file() {
        // The staging name is artist--album, so two different pairs can collapse
        // onto one directory (A--B + C and A + B--C) and albums run concurrently.
        // Deleting the tree there would delete a download this run did not stage,
        // so the directory is left alone with a warning instead.
        let root = TempDir::new().unwrap();
        let album = root.path().join("Test Artist--Test Album");
        std::fs::create_dir_all(&album).unwrap();
        let ours = album.join("01 - Ours.flac");
        let foreign = album.join("02 - Theirs.flac");
        std::fs::write(&ours, b"not audio").unwrap();
        std::fs::write(&foreign, b"not audio").unwrap();

        discard_refused_staging(&album, std::slice::from_ref(&ours));

        assert!(
            foreign.exists(),
            "another album's staged file must survive a refusal in this one"
        );
        assert!(
            !ours.exists(),
            "the refused album's own staged file must still be removed"
        );
        assert!(album.exists(), "the shared directory must be left in place");
    }

    #[tokio::test]
    async fn auto_mode_with_library_upgrade_disabled_uses_the_organize_path() {
        let client = Arc::new(MockClient::new());
        *client.search_results.lock().unwrap() = vec![SearchResult {
            username: "peer".into(),
            speed: 500,
            slots: 1,
            files: vec![make_file(
                r"Test Artist\Test Album\01 - track.flac",
                900,
                10_000_000,
            )],
        }];
        // Real bytes so the organizer has something to move.
        *client.write_files.lock().unwrap() = true;

        let mut config = make_test_config();
        config.library_upgrade.enabled = false;
        config.storage.organize = true;
        let library = TempDir::new().unwrap();
        config.library.paths = vec![library.path().to_string_lossy().into_owned()];
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
        .await
        .unwrap();

        assert!(
            matches!(result, AlbumOutcome::Downloaded { track_count: 1, .. }),
            "the album must complete with its downloaded track count"
        );
        assert!(
            library
                .path()
                .join("Test Artist")
                .join("Test Album")
                .join("01 - track.flac")
                .exists(),
            "with the upgrade flag off the organizer writes under library.paths[0]"
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
                r"Music\Alesha Dixon\The Alesha Show\01 - track.flac",
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
        // upgrade). The artist folder sits inside the "Pop" genre subdirectory.
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
        // genre subdirectory — <tmp>/Pop/Alesha Dixon/The Alesha Show/01 - track.flac.
        // It must NOT land one level deeper, at the doubled
        // <tmp>/Pop/Pop/Alesha Dixon/01 - track.flac path that the old
        // path-derivation produced for this library layout.
        let expected = tmp
            .path()
            .join("Pop")
            .join("Alesha Dixon")
            .join("The Alesha Show")
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
            .join("Pop")
            .join("Alesha Dixon")
            .join("01 - track.flac");
        assert!(
            !wrong.exists(),
            "upgrade must NOT copy into the doubled genre/artist path: {wrong:?}"
        );
    }

    #[tokio::test]
    async fn test_auto_mode_upgrade_of_a_disc_nested_album_lands_in_the_album_folder() {
        let client = Arc::new(MockClient::new());
        *client.search_results.lock().unwrap() = vec![SearchResult {
            username: "user1".into(),
            speed: 500,
            slots: 1,
            files: vec![make_file(
                r"Music\Alesha Dixon\The Alesha Show\01 - track.flac",
                900,
                10_000_000,
            )],
        }];

        let mut config = make_test_config();
        config.library_upgrade.enabled = true;
        config.library_upgrade.delete_lesser_quality = false;
        let tmp = TempDir::new().unwrap();
        // Library layout: <tmp>/Pop/Alesha Dixon/The Alesha Show/CD 01/01 - track.mp3.
        // The disc folder must be stepped over, so the album's location is
        // <tmp>/Pop and the artist folder is "Alesha Dixon".
        let disc_dir = tmp
            .path()
            .join("Pop")
            .join("Alesha Dixon")
            .join("The Alesha Show")
            .join("CD 01");
        std::fs::create_dir_all(&disc_dir).unwrap();
        std::fs::write(disc_dir.join("01 - track.mp3"), b"fake mp3 data").unwrap();
        config.library.paths = vec![tmp.path().to_string_lossy().into()];

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

        let expected = tmp
            .path()
            .join("Pop")
            .join("Alesha Dixon")
            .join("The Alesha Show")
            .join("01 - track.flac");
        assert!(
            expected.exists(),
            "the upgrade must land in the album folder: {expected:?}"
        );
        assert!(
            !disc_dir.join("01 - track.flac").exists(),
            "the copy must not land inside the disc subdirectory"
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

    // An exhausted album is recorded in the run report (printed at INFO as
    // `Failed (n):`) and in the processed-albums table, so the inline line must
    // not repeat the same outcome as a warning. Distinctive names: `LogCapture`
    // keeps one process-wide window, so a concurrent test's identical line must
    // not be able to satisfy the guard.
    #[tokio::test]
    async fn an_exhausted_album_is_a_debug_record() {
        let client = Arc::new(MockClient::new());
        // Below the impossible floor below, so the candidate fails on speed.
        *client.download_speed.lock().unwrap() = 100_000;
        *client.search_results.lock().unwrap() = vec![SearchResult {
            username: "quiet-capture-peer".into(),
            speed: 500,
            slots: 1,
            files: vec![make_file(
                r"Quiet Capture Artist\Quiet Capture Album\01 - track.flac",
                900,
                10_000_000,
            )],
        }];

        let mut config = make_test_config();
        config.download.min_upload_speed_kbps = 10_000_000;
        config.download.speed_check_wait_secs = 0;
        config.download.max_retries = 1;
        config.download.retry_delay_secs = 0;

        let db = Database::open_in_memory().unwrap();
        let staging = TempDir::new().unwrap();
        let capture = crate::test_support::LogCapture::start();

        let result = process_album(
            client.as_ref() as &dyn crate::client::SoulseekClient,
            "Quiet Capture Artist",
            Some("Quiet Capture Album"),
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
        assert!(
            matches!(result.unwrap(), AlbumOutcome::Failed { .. }),
            "the album must report failure"
        );

        let logs = capture.text();
        let line = logs
            .lines()
            .find(|line| {
                line.contains("Quiet Capture Artist") && line.contains("candidates exhausted")
            })
            .unwrap_or_else(|| panic!("no album failure line for this fixture, got:\n{logs}"));
        assert_eq!(
            line.split_whitespace().next(),
            Some("DEBUG"),
            "an exhausted album is not an inline warning: {line}"
        );
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
