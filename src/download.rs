use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::time::{timeout, Duration};

use indicatif::ProgressBar;

use crate::client::{DownloadStatus, FileInfo, SearchResult, SoulseekClient};
use crate::config::DownloadConfig;
use crate::error::{Result, SeakarrError};
use crate::filter;
use crate::formatting::format_speed;
use crate::progress::ProgressDisplay;

/// Sanitize a remote filename for local download: extract the basename
/// (the crate already strips directory components), reject path-traversal
/// patterns, and return a safe filename suitable for path construction.
pub(crate) fn safe_basename(remote_name: &str) -> Result<&str> {
    // Soulseek filenames may use either / or \\ as path separators
    // (many peers share from Windows machines).  Split on both and
    // take the last component.
    let basename = remote_name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(remote_name);
    if basename.is_empty() || basename == "." || basename.contains("..") {
        return Err(SeakarrError::Download(format!(
            "unsafe or empty remote filename: {remote_name:?}"
        )));
    }
    Ok(basename)
}

/// Drain the status channel until the transfer terminates (Completed/Failed)
/// or the channel closes. Prevents `remove_dir_all` from racing the vendor
/// library's transfer thread after cancellation.
async fn drain_transfer(rx: &mut tokio::sync::mpsc::Receiver<DownloadStatus>, timeout_secs: u64) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_secs);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
            Ok(Some(DownloadStatus::Completed))
            | Ok(Some(DownloadStatus::Failed { .. }))
            | Ok(None) => return,
            _ => continue,
        }
    }
}

/// Download a single file from a specific user, monitoring speed.
///
/// Retries the same peer up to `config.max_retries` times on failure,
/// waiting `config.retry_delay_secs` between attempts, before surfacing the
/// last error. A user-initiated cancellation (Ctrl+C) aborts at the next
/// safe point — between attempts or within the status polling loop — and
/// is never retried.
///
/// The progress bar is created lazily on the first `InProgress` status —
/// before that point no bar renders. Pass `None` for `progress` to skip
/// the bar entirely.
pub async fn download_file(
    client: &dyn SoulseekClient,
    file: &FileInfo,
    username: &str,
    dir: &Path,
    config: &DownloadConfig,
    progress: Option<&ProgressDisplay>,
    cancel: Option<&Arc<AtomicBool>>,
) -> Result<PathBuf> {
    // Validate the remote name for traversal safety, but pass the FULL
    // share-relative path to the crate: the Soulseek QueueUpload wire
    // message must quote the path exactly as the peer shared it (e.g.
    // "Music\Artist\Album\01 - Track.flac"). Sending only the basename
    // makes every peer respond UploadDenied because it cannot find a
    // basename-only entry in its share list. The crate strips the path
    // itself when writing the local file, so the local destination is
    // still dir/<basename>.
    let basename = safe_basename(&file.name)?;

    // Retry the same peer up to max_retries times. On cancellation the
    // first attempt already aborts via the cancel flag inside download_once,
    // and the loop re-checks the flag between attempts so a SIGINT during
    // the delay window is honoured as soon as the sleep completes.
    //
    // NOTE: all failure types are retried, including non-transient ones
    // (e.g. "user declined", "could not connect"). This is intentional —
    // the retry_delay_secs penalty is the cost of a failed attempt, and
    // the candidate-list fallback provides the real diversity. Permanent
    // failures waste one delay window per retry, then fall back.
    //
    // NOTE: with very low retry_delay_secs (< ~30 s), the vendor crate's
    // transfer thread may still be winding down (30 s socket read timeout)
    // when the retry opens a new connection to the same peer+file. Both
    // threads write to the same .part file (O_APPEND). The default 30 s
    // delay makes this negligible; lower values risk interleaved writes.
    let mut last_err: Option<SeakarrError> = None;
    for attempt in 0..=config.max_retries {
        if attempt > 0 {
            if cancel.is_some_and(|c| c.load(Ordering::SeqCst)) {
                return Err(SeakarrError::Download("download cancelled by user".into()));
            }
            tracing::info!(
                "Retrying download of {basename} from {username} (attempt {attempt}/{})",
                config.max_retries
            );
            tokio::time::sleep(Duration::from_secs(config.retry_delay_secs)).await;
            // Re-check after the delay — a SIGINT during sleep must not
            // fall through to queueing another download.
            if cancel.is_some_and(|c| c.load(Ordering::SeqCst)) {
                return Err(SeakarrError::Download("download cancelled by user".into()));
            }
        }
        match download_once(
            client, file, basename, username, dir, config, progress, cancel,
        )
        .await
        {
            Ok(path) => return Ok(path),
            Err(e) => {
                // If the cancel flag is set, the error is from a user-
                // initiated abort — surface it immediately, don't retry.
                if cancel.is_some_and(|c| c.load(Ordering::SeqCst)) {
                    return Err(e);
                }
                last_err = Some(e);
            }
        }
    }
    // Unreachable in practice: the loop always executes at least once
    // (max_retries >= 0), so last_err is always set on this path.
    Err(last_err.unwrap())
}

/// Update an exponential moving average (EMA) with a new sample.
///
/// Returns the updated EMA value. On the first call (`current` is `None`),
/// the new value is returned unsmoothed.
///
/// `alpha` controls responsiveness vs. smoothness:
/// - Higher alpha (e.g. 0.5) → faster response to real changes
/// - Lower alpha (e.g. 0.2) → smoother but slower to react
/// - Typical for speed display: 0.3 (responds within ~3-4 updates)
const SPEED_EMA_ALPHA: f64 = 0.3;

fn ema_update(current: Option<f64>, new_value: f64, alpha: f64) -> f64 {
    match current {
        Some(prev) => alpha * new_value + (1.0 - alpha) * prev,
        None => new_value,
    }
}

/// Single download attempt for `download_file` (no retry loop). Queues the
/// transfer and polls status until success, failure, timeout, or cancel.
/// The `basename` parameter must be pre-validated by the caller.
#[allow(clippy::too_many_arguments)]
async fn download_once(
    client: &dyn SoulseekClient,
    file: &FileInfo,
    basename: &str,
    username: &str,
    dir: &Path,
    config: &DownloadConfig,
    progress: Option<&ProgressDisplay>,
    cancel: Option<&Arc<AtomicBool>>,
) -> Result<PathBuf> {
    let mut handle = match client.download(file, username, dir).await {
        Ok(h) => h,
        Err(e) => return Err(e),
    };
    tracing::info!("Download queued: {basename} from {username}");
    let mut transfer_start: Option<tokio::time::Instant> = None;
    // Progress bar for this transfer, created lazily on the first InProgress
    // status. Before the transfer actually starts no bar may render — the
    // user should only see progress once a download is underway (the bar was
    // previously created eagerly in download_album from search metadata,
    // so a 0 B/[total] [0%] bar appeared before the bridge even started).
    let mut bar: Option<ProgressBar> = None;
    // Track the peer's reported total so the bar can be snapped to 100%
    // on completion (the final InProgress may lag the actual end).
    let mut last_total_bytes: u64 = 0;
    // Exponential moving average of the transfer speed. Smooths out
    // the raw instantaneous speed_bytes_per_sec from each InProgress
    // status so the displayed speed doesn't jump around.
    let mut speed_ema: Option<f64> = None;
    // Wall-clock deadline for the entire transfer — reset on every status
    // message. With 1-second polling, Err(_elapsed) fires every second;
    // only trigger timeout when the deadline is truly exceeded.
    let mut deadline = tokio::time::Instant::now() + Duration::from_secs(config.timeout_secs);

    loop {
        // Honour cancellation (Ctrl+C / SIGINT): abort the transfer and
        // clean up. The caller (download_album) removes the staging dir.
        if cancel.is_some_and(|c| c.load(Ordering::SeqCst)) {
            let _ = handle.cancel_tx.send(()).await;
            drain_transfer(&mut handle.status_rx, 5).await;
            if let Some(bar) = &bar {
                bar.finish_and_clear();
            }
            return Err(SeakarrError::Download("download cancelled by user".into()));
        }
        // Poll status with a short timeout so the cancel flag is checked
        // frequently (at least once per second). The wall-clock deadline
        // for the whole transfer is still config.timeout_secs.
        let poll_timeout = Duration::from_secs(1);
        let msg = timeout(poll_timeout, handle.status_rx.recv()).await;

        // Any message from the peer resets the deadline — the transfer
        // is alive. Only the Err (poll timeout) arm checks the deadline.
        if msg.is_ok() {
            deadline = tokio::time::Instant::now() + Duration::from_secs(config.timeout_secs);
        }

        match msg {
            Ok(Some(DownloadStatus::InProgress {
                speed_bytes_per_sec,
                bytes_downloaded,
                total_bytes,
            })) => {
                if transfer_start.is_none() {
                    transfer_start = Some(tokio::time::Instant::now());
                    // Create the progress bar only once the transfer has
                    // actually started (first InProgress). Skip for
                    // zero-length transfers — indicatif renders len==0 as
                    // 100% (state.rs:282), which reproduces the bug.
                    if total_bytes > 0 {
                        if let Some(p) = progress {
                            bar = Some(p.create_bar(basename, total_bytes));
                        }
                    }
                }
                last_total_bytes = total_bytes;
                // Speed check: only after the transfer has actually started
                // transferring (not just queued), and past the wait period.
                if config.min_upload_speed_kbps > 0 {
                    if let Some(ts) = transfer_start {
                        if ts.elapsed().as_secs() >= config.speed_check_wait_secs {
                            let speed_kbps = (speed_bytes_per_sec / 1024) as u32;
                            if speed_kbps < config.min_upload_speed_kbps {
                                if let Some(bar) = &bar {
                                    bar.finish_and_clear();
                                }
                                let _ = handle.cancel_tx.send(()).await;
                                // Wait for the transfer to terminate before
                                // returning — download_album calls remove_dir_all.
                                drain_transfer(&mut handle.status_rx, 5).await;
                                return Err(SeakarrError::Download(format!(
                                    "speed {speed_kbps} KB/s below minimum {} KB/s",
                                    config.min_upload_speed_kbps
                                )));
                            }
                        }
                    }
                }
                // Update progress bar if present — use EMA-smoothed
                // speed for display so it doesn't jump around.
                let smoothed = ema_update(speed_ema, speed_bytes_per_sec as f64, SPEED_EMA_ALPHA);
                speed_ema = Some(smoothed);
                if let Some(bar) = &bar {
                    bar.set_position(bytes_downloaded);
                    bar.set_prefix(format_speed(smoothed as u64));
                }
            }
            Ok(Some(DownloadStatus::Completed)) => {
                if let Some(bar) = &bar {
                    // Snap to 100% before clearing — the final InProgress
                    // may have left the bar below the total.
                    // finish_and_clear() removes the bar from the terminal
                    // so the next track starts with a single bar (no
                    // "double progress bar" effect).
                    bar.set_position(last_total_bytes);
                    bar.finish_and_clear();
                }
                let dest = dir.join(basename);
                tracing::info!("Download completed: {basename}");
                return Ok(dest);
            }
            Ok(Some(DownloadStatus::Failed { reason })) => {
                if let Some(bar) = &bar {
                    bar.finish_and_clear();
                }
                tracing::warn!("Download of {basename} failed: {reason}");
                return Err(SeakarrError::Download(format!("transfer failed: {reason}")));
            }
            Ok(Some(DownloadStatus::Queued { .. })) => {}
            Ok(None) => {
                if let Some(bar) = &bar {
                    bar.finish_and_clear();
                }
                tracing::warn!("Download channel closed for {basename}");
                return Err(SeakarrError::Download(
                    "download channel closed unexpectedly".into(),
                ));
            }
            Err(_elapsed) => {
                // The 1-second poll timed out — check the wall-clock
                // deadline before declaring the transfer dead.
                if tokio::time::Instant::now() >= deadline {
                    if let Some(bar) = &bar {
                        bar.finish_and_clear();
                    }
                    tracing::warn!(
                        "Download of {basename} timed out after {}s",
                        config.timeout_secs
                    );
                    let _ = handle.cancel_tx.send(()).await;
                    drain_transfer(&mut handle.status_rx, 5).await;
                    return Err(SeakarrError::Download("download timed out".into()));
                }
                // Still within deadline — continue polling.
            }
        }
    }
}

/// Download all files for an album from the best candidate, with fallback.
/// Tries each candidate in ranked order until one succeeds (or all fail).
/// Only downloads files that are safe (no path traversal) and pass the
/// configured extension filters.
/// Group files by their share-relative parent directory path and return
/// the largest group (most files). A peer's search result can span
/// multiple album directories when the query matches several of their
/// albums; downloading all of them would mix tracks from different albums
/// into one staging folder. Files at the share root (no parent component)
/// all belong to a single "<root>" group.
///
/// The key is the full parent path (not just the immediate directory name),
/// so `Abba\Greatest Hits\...` and `Bee Gees\Greatest Hits\...` form
/// separate groups despite sharing the leaf directory name "Greatest Hits".
///
/// Ties are broken by lexicographic key order for determinism.
fn largest_album_group<'a>(files: &[&'a FileInfo]) -> Vec<&'a FileInfo> {
    let mut groups: std::collections::HashMap<&str, Vec<&'a FileInfo>> = Default::default();
    for f in files {
        // rsplit_once gives the full parent path (everything before the
        // last separator), not just the immediate directory name.
        let dir = f
            .name
            .rsplit_once(['/', '\\'])
            .map(|(parent, _basename)| parent)
            .filter(|p| !p.is_empty())
            .unwrap_or("<root>");
        groups.entry(dir).or_default().push(f);
    }
    // Deterministic tie-breaking: prefer the larger group; on equal size
    // prefer lexicographically earlier key so the same album wins on
    // every run.
    groups
        .into_iter()
        .max_by(|(ak, av), (bk, bv)| av.len().cmp(&bv.len()).then_with(|| bk.cmp(ak)))
        .map(|(_, v)| v)
        .unwrap_or_default()
}

pub async fn download_album(
    client: &dyn SoulseekClient,
    candidates: &[SearchResult],
    staging_dir: &Path,
    config: &DownloadConfig,
    filters: &crate::config::FilterConfig,
    progress: Option<&ProgressDisplay>,
    cancel: Option<&Arc<AtomicBool>>,
) -> Result<Vec<PathBuf>> {
    let mut last_err: Option<SeakarrError> = None;

    // Try each candidate in ranked order; staging dir is created on demand
    // only when a candidate has valid files to download.
    for candidate in candidates {
        // Check cancellation between candidates — avoid queuing network
        // requests to the next peer after the user pressed Ctrl+C.
        // Clean the staging dir before returning so no partial downloads
        // are left behind (same cleanup as the post-loop failure path).
        if cancel.is_some_and(|c| c.load(Ordering::SeqCst)) {
            // Remove staging dir if it was created; suppress ENOENT
            // (dir may not exist yet if cancel fires before first download).
            match std::fs::remove_dir_all(staging_dir) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    tracing::warn!("Failed to clean staging dir on cancel {staging_dir:?}: {e}")
                }
            }
            return Err(SeakarrError::Download("download cancelled by user".into()));
        }

        let all_passed = candidate
            .files
            .iter()
            .filter(|f| safe_basename(&f.name).is_ok() && filter::file_passes_filters(f, filters))
            .count();
        if all_passed == 0 {
            // Show a sample of what we're rejecting to help debug
            for f in candidate.files.iter().take(3) {
                let ext = f.name.rsplit('.').next().unwrap_or("<none>");
                let safe = safe_basename(&f.name).is_ok();
                let passes = filter::file_passes_filters(f, filters);
                tracing::warn!(
                    "reject: {:?} ext={ext} safe={safe} passes={passes} bitrate={bitrate:?}",
                    f.name,
                    bitrate = f.attribs.get(&0),
                );
            }
        }
        let filtered_files: Vec<&FileInfo> = candidate
            .files
            .iter()
            .filter(|f| safe_basename(&f.name).is_ok() && filter::file_passes_filters(f, filters))
            .collect();

        if filtered_files.is_empty() {
            last_err = Some(SeakarrError::Download(
                "candidate had no safe files to download".into(),
            ));
            continue;
        }

        // A peer's search result may span multiple album directories (e.g.
        // "Abba\\[1992] Gold_ Greatest Hits\\..." and
        // "Abba\\[1993] More ABBA Gold\\..." when both match the query).
        // Downloading every matching file would mix tracks from different
        // albums into the single staging folder. Group by parent directory
        // and keep only the largest group — the album the query most likely
        // targeted — so one run downloads a single album per peer.
        let filtered_files = largest_album_group(&filtered_files);

        // Create the staging directory only when we have valid files to
        // download — prevents empty dirs for albums where every candidate
        // fails the safe_basename / file_passes_filters checks.
        std::fs::create_dir_all(staging_dir)?;

        let mut downloaded = Vec::new();
        let mut failed = false;

        for file in &filtered_files {
            match download_file(
                client,
                file,
                &candidate.username,
                staging_dir,
                config,
                progress,
                cancel,
            )
            .await
            {
                Ok(path) => {
                    downloaded.push(path);
                }
                Err(e) => {
                    // download_file already retried this file on the same
                    // peer up to max_retries times (with retry_delay_secs
                    // between attempts). A failure here means those retries
                    // were exhausted, so fall back to the next ranked
                    // candidate — the candidate list is the outer fallback.
                    tracing::warn!(
                        "Download of {} from {} failed after retries: {e}",
                        file.name,
                        candidate.username
                    );
                    last_err = Some(e);
                    failed = true;
                    break; // Move to next candidate
                }
            }
        }

        if !failed {
            return Ok(downloaded);
        }

        // The album's staging directory is per-album (created by the runner).
        // A failed candidate must leave no staging directory at all —
        // the album is either fully downloaded (directory exists with files)
        // or absent (failed/never attempted). This removes completed tracks,
        // `.part` files, and the directory itself. Retry once after a brief
        // pause to handle transient locks from a just-cancelled transfer.
        for attempt in 0..2 {
            match std::fs::remove_dir_all(staging_dir) {
                Ok(()) => break,
                Err(e) if attempt == 0 => {
                    tracing::warn!(
                        "Failed to clean up staging directory {staging_dir:?} (retrying): {e}"
                    );
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
                Err(e) => {
                    tracing::warn!("Failed to clean up staging directory {staging_dir:?}: {e}");
                }
            }
        }
    }

    Err(last_err.unwrap_or_else(|| SeakarrError::Download("all candidates exhausted".into())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{DownloadHandle, FileInfo, MockClient, SearchResult, UserInfo, UserStatus};
    use crate::config::{DownloadConfig, FilterConfig};
    use async_trait::async_trait;
    use std::collections::HashMap;
    use std::sync::Mutex;
    use tempfile::TempDir;
    use tokio::sync::mpsc;

    fn make_file(name: &str, bitrate: u32, size: u64) -> FileInfo {
        let mut attribs = HashMap::new();
        attribs.insert(0, bitrate);
        FileInfo {
            name: name.into(),
            size,
            attribs,
        }
    }

    fn default_dl_config() -> DownloadConfig {
        DownloadConfig {
            concurrent: 5,
            max_queue_length: 0,
            max_start_time_secs: 120,
            max_queue_time_secs: 1800,
            min_upload_speed_kbps: 0, // disabled for test
            speed_check_wait_secs: 0, // immediate for test
            timeout_secs: 180,
            max_download_time_mins: 120,
            max_retries: 2,
            retry_delay_secs: 0,
            min_filtered_users: 1,
            skip_retry_hours: 24,
        }
    }

    /// Filter config with the min_tracks gate disabled — these focused
    /// download tests use small mock shares and exercise transfer/cleanup
    /// logic, not share completeness (covered in filter.rs).
    fn default_filter_config_test() -> FilterConfig {
        FilterConfig {
            min_tracks: 0,
            ..FilterConfig::default()
        }
    }

    // EMA (exponential moving average) tests — verifies the smoothing
    // function used for speed display.

    #[test]
    fn ema_first_sample_returns_raw_value() {
        // First call: no smoothing, return the raw value.
        let result = ema_update(None, 1_000_000.0, 0.3);
        assert_eq!(result, 1_000_000.0);
    }

    #[test]
    fn ema_smooths_second_sample() {
        // Second call: EMA = alpha * new + (1 - alpha) * prev
        // 0.3 * 2_000_000 + 0.7 * 1_000_000 = 600_000 + 700_000 = 1_300_000
        let result = ema_update(Some(1_000_000.0), 2_000_000.0, 0.3);
        assert!(
            (result - 1_300_000.0).abs() < 0.01,
            "expected ~1_300_000, got {result}"
        );
    }

    #[test]
    fn ema_converges_to_steady_state() {
        // Seed from 0 and feed the target repeatedly — EMA should converge.
        let alpha = 0.3;
        let target = 1_000_000.0;
        let mut ema = ema_update(None, 0.0, alpha); // start from 0
        for _ in 0..50 {
            ema = ema_update(Some(ema), target, alpha);
        }
        // After 50 iterations with alpha=0.3, the residual from the
        // initial 0 is (0.7)^50 ≈ 1.8e-8 — negligible.
        assert!((ema - target).abs() < 1.0, "expected ~{target}, got {ema}");
    }

    #[test]
    fn ema_smooths_out_spikes() {
        // A single spike should be smoothed significantly.
        let alpha = 0.3;
        let mut ema = ema_update(None, 1_000_000.0, alpha);
        // Spike: 10x normal speed
        ema = ema_update(Some(ema), 10_000_000.0, alpha);
        // EMA should be much less than the spike:
        // 0.3 * 10_000_000 + 0.7 * 1_000_000 = 3_000_000 + 700_000 = 3_700_000
        assert!(
            (ema - 3_700_000.0).abs() < 0.01,
            "expected ~3_700_000, got {ema}"
        );
        // Next reading back to normal:
        ema = ema_update(Some(ema), 1_000_000.0, alpha);
        // 0.3 * 1_000_000 + 0.7 * 3_700_000 = 300_000 + 2_590_000 = 2_890_000
        assert!(
            (ema - 2_890_000.0).abs() < 0.01,
            "expected ~2_890_000, got {ema}"
        );
    }

    /// A client whose download status channel the test drives manually, so
    /// the moment a transfer "starts" (first InProgress) is under the test's
    /// control rather than raced against a background task.
    struct ControllableClient {
        /// Set when `download()` is called — signals the transfer was queued.
        download_called: Arc<std::sync::atomic::AtomicBool>,
        /// Sender captured by `download()`; the test pushes statuses through it.
        status_tx: Mutex<Option<mpsc::Sender<DownloadStatus>>>,
    }

    impl ControllableClient {
        fn new() -> Self {
            ControllableClient {
                download_called: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                status_tx: Mutex::new(None),
            }
        }
    }

    #[async_trait]
    impl SoulseekClient for ControllableClient {
        async fn login(&self, _username: &str, _password: &str, _server: &str) -> Result<()> {
            Ok(())
        }

        async fn search(&self, _query: &str, _timeout_secs: u64) -> Result<Vec<SearchResult>> {
            Ok(vec![])
        }

        async fn download(
            &self,
            _file: &FileInfo,
            _username: &str,
            _dir: &Path,
        ) -> Result<DownloadHandle> {
            let (status_tx, status_rx) = mpsc::channel(32);
            let (cancel_tx, _cancel_rx) = mpsc::channel(1);
            *self.status_tx.lock().unwrap() = Some(status_tx);
            self.download_called.store(true, Ordering::SeqCst);
            Ok(DownloadHandle {
                status_rx,
                cancel_tx,
            })
        }

        async fn user_info(&self, username: &str) -> Result<UserInfo> {
            Ok(UserInfo {
                username: username.into(),
                status: UserStatus::Online,
            })
        }
    }

    /// A client whose `download()` fails the first `failures` calls (e.g.
    /// with a timeout), then succeeds like MockClient. Records every call so
    /// tests can assert the retry count.
    struct RetryClient {
        /// How many initial `download()` calls should fail.
        failures: std::sync::atomic::AtomicUsize,
        /// Total `download()` calls made.
        calls: std::sync::atomic::AtomicUsize,
    }

    impl RetryClient {
        fn new(failures: usize) -> Self {
            RetryClient {
                failures: std::sync::atomic::AtomicUsize::new(failures),
                calls: std::sync::atomic::AtomicUsize::new(0),
            }
        }

        fn call_count(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl SoulseekClient for RetryClient {
        async fn login(&self, _username: &str, _password: &str, _server: &str) -> Result<()> {
            Ok(())
        }

        async fn search(&self, _query: &str, _timeout_secs: u64) -> Result<Vec<SearchResult>> {
            Ok(vec![])
        }

        async fn download(
            &self,
            _file: &FileInfo,
            _username: &str,
            _dir: &Path,
        ) -> Result<DownloadHandle> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            if n < self.failures.load(Ordering::SeqCst) {
                return Err(SeakarrError::Download("download timed out".into()));
            }
            let (status_tx, status_rx) = mpsc::channel(32);
            let (cancel_tx, mut cancel_rx) = mpsc::channel(1);
            let total = 10_000_000u64;
            // Success: emit InProgress then Completed (mirrors MockClient).
            tokio::spawn(async move {
                if cancel_rx.try_recv().is_ok() {
                    let _ = status_tx
                        .send(DownloadStatus::Failed {
                            reason: "cancelled".into(),
                        })
                        .await;
                    return;
                }
                let _ = status_tx
                    .send(DownloadStatus::InProgress {
                        speed_bytes_per_sec: 1_000_000,
                        bytes_downloaded: total,
                        total_bytes: total,
                    })
                    .await;
                let _ = status_tx.send(DownloadStatus::Completed).await;
            });
            Ok(DownloadHandle {
                status_rx,
                cancel_tx,
            })
        }

        async fn user_info(&self, username: &str) -> Result<UserInfo> {
            Ok(UserInfo {
                username: username.into(),
                status: UserStatus::Online,
            })
        }
    }

    /// A client that fails downloads for specific filenames, simulating
    /// partial downloads where some files in a candidate fail.
    struct SelectiveFailClient {
        /// Filenames that should return an error from `download()`.
        fail_files: std::collections::HashSet<String>,
    }

    impl SelectiveFailClient {
        fn new(fail_files: Vec<&str>) -> Self {
            SelectiveFailClient {
                fail_files: fail_files.into_iter().map(String::from).collect(),
            }
        }
    }

    #[async_trait]
    impl SoulseekClient for SelectiveFailClient {
        async fn login(&self, _u: &str, _p: &str, _s: &str) -> Result<()> {
            Ok(())
        }

        async fn search(&self, _q: &str, _t: u64) -> Result<Vec<SearchResult>> {
            Ok(vec![])
        }

        async fn download(
            &self,
            file: &FileInfo,
            _username: &str,
            _dir: &Path,
        ) -> Result<DownloadHandle> {
            if self.fail_files.contains(&file.name) {
                return Err(SeakarrError::Download("simulated failure".into()));
            }
            let (status_tx, status_rx) = mpsc::channel(32);
            let (cancel_tx, mut cancel_rx) = mpsc::channel(1);
            tokio::spawn(async move {
                if cancel_rx.try_recv().is_ok() {
                    let _ = status_tx
                        .send(DownloadStatus::Failed {
                            reason: "cancelled".into(),
                        })
                        .await;
                    return;
                }
                let _ = status_tx
                    .send(DownloadStatus::InProgress {
                        speed_bytes_per_sec: 1_000_000,
                        bytes_downloaded: 10_000_000,
                        total_bytes: 10_000_000,
                    })
                    .await;
                let _ = status_tx.send(DownloadStatus::Completed).await;
            });
            Ok(DownloadHandle {
                status_rx,
                cancel_tx,
            })
        }

        async fn user_info(&self, username: &str) -> Result<UserInfo> {
            Ok(UserInfo {
                username: username.into(),
                status: UserStatus::Online,
            })
        }
    }

    // Regression guard for the UploadDenied-everywhere bug: the Soulseek
    // QueueUpload wire message must carry the FULL share-relative path
    // exactly as the peer shared it ("Music\\Artist\\Album\\01 - Track.flac"),
    // not a basename. Sending only the basename made every peer respond
    // UploadDenied because it could not match the request against its share
    // list.
    #[tokio::test]
    async fn test_download_passes_full_share_path_to_client() {
        let client = MockClient::new();
        let dir = TempDir::new().unwrap();
        let file = make_file(
            "Music\\Amy Winehouse\\Back to Black (2006)\\1-01 - Rehab.flac",
            900,
            10_000_000,
        );
        let config = default_dl_config();

        let result = download_file(&client, &file, "peer", dir.path(), &config, None, None).await;
        assert!(result.is_ok());

        let wire_name = client
            .last_download_filename
            .lock()
            .unwrap()
            .clone()
            .expect("download() recorded a filename");
        assert_eq!(
            wire_name, "Music\\Amy Winehouse\\Back to Black (2006)\\1-01 - Rehab.flac",
            "the wire filename must be the full share-relative path"
        );
    }

    #[tokio::test]
    async fn test_download_file_with_progress_bar() {
        let client = MockClient::new();
        let dir = TempDir::new().unwrap();
        let file = make_file("01 - track.flac", 900, 10_000_000);

        let config = default_dl_config();
        let display = ProgressDisplay::new();

        let result = download_file(
            &client,
            &file,
            "testuser",
            dir.path(),
            &config,
            Some(&display),
            None,
        )
        .await;
        assert!(result.is_ok());
        // The mock client emits InProgress immediately, so a bar must have
        // been created once the transfer started.
        assert_eq!(display.created_bars(), 1);
    }

    // Regression guard: the progress bar must not be created until a
    // transfer has actually started (first InProgress status). Previously
    // download_album created the bar eagerly from search metadata BEFORE
    // calling download_file, so indicatif rendered a 0 B/29.77 MiB [0%] bar
    // on the runner's log line while the bridge was still starting.
    #[tokio::test]
    async fn progress_bar_not_created_until_transfer_starts() {
        let client = Arc::new(ControllableClient::new());
        let display = Arc::new(ProgressDisplay::new());
        let dir = TempDir::new().unwrap();

        let candidates = vec![SearchResult {
            username: "user1".into(),
            speed: 500,
            slots: 1,
            files: vec![make_file("01 - track.flac", 900, 10_000_000)],
        }];
        let config = default_dl_config();

        let task = tokio::spawn({
            let client = client.clone();
            let display = display.clone();
            let dir_path = dir.path().to_path_buf();
            async move {
                download_album(
                    client.as_ref() as &dyn SoulseekClient,
                    &candidates,
                    &dir_path,
                    &config,
                    &default_filter_config_test(),
                    Some(display.as_ref()),
                    None,
                )
                .await
            }
        });

        // Wait until the transfer has been queued with the client but no
        // status has been pushed yet.
        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(5);
        while !client.download_called.load(Ordering::SeqCst) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "download() was never called"
            );
            tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        }

        // RED: while the transfer is only queued, no progress bar may exist.
        assert_eq!(
            display.created_bars(),
            0,
            "progress bar must not appear before the transfer starts"
        );

        // The transfer starts — push the first InProgress status.
        let tx = client
            .status_tx
            .lock()
            .unwrap()
            .clone()
            .expect("status sender captured by download()");
        tx.send(DownloadStatus::InProgress {
            speed_bytes_per_sec: 1_000_000,
            bytes_downloaded: 1_000_000,
            total_bytes: 10_000_000,
        })
        .await
        .unwrap();

        // The bar must now exist.
        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(5);
        while display.created_bars() == 0 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "bar was never created after the transfer started"
            );
            tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        }
        assert_eq!(display.created_bars(), 1);

        // Complete the transfer so the task can finish.
        tx.send(DownloadStatus::Completed).await.unwrap();
        let result = task.await.unwrap();
        assert!(result.is_ok());
    }

    // Regression guard: downloads must retry the SAME peer up to
    // max_retries times (with retry_delay_secs between attempts) before
    // giving up. Previously download_file made a single attempt — the
    // max_retries/retry_delay_secs config values were dead code and a
    // failing peer moved straight to the next candidate.
    #[tokio::test]
    async fn download_retries_same_peer_after_failure() {
        // Fail the first 2 download() calls (timeout), succeed on the 3rd.
        let client = Arc::new(RetryClient::new(2));
        let dir = TempDir::new().unwrap();
        let file = make_file("track.flac", 900, 10_000_000);

        let mut config = default_dl_config();
        config.max_retries = 2;
        config.retry_delay_secs = 0; // instant retries for the test

        let result = download_file(
            client.as_ref() as &dyn SoulseekClient,
            &file,
            "testuser",
            dir.path(),
            &config,
            None,
            None,
        )
        .await;

        // Succeeded after retries.
        assert!(
            result.is_ok(),
            "download should succeed after retries: {result:?}"
        );
        // 1 initial attempt + 2 retries = 3 calls.
        assert_eq!(
            client.call_count(),
            3,
            "expected initial attempt + 2 retries (max_retries=2), got {} calls",
            client.call_count()
        );
    }

    // When all retries are exhausted, download_file must surface the last
    // failure (not silently succeed or spin forever).
    #[tokio::test]
    async fn download_returns_error_after_all_retries_exhausted() {
        let client = Arc::new(RetryClient::new(5)); // always fails
        let dir = TempDir::new().unwrap();
        let file = make_file("track.flac", 900, 10_000_000);

        let mut config = default_dl_config();
        config.max_retries = 2;
        config.retry_delay_secs = 0;

        let result = download_file(
            client.as_ref() as &dyn SoulseekClient,
            &file,
            "testuser",
            dir.path(),
            &config,
            None,
            None,
        )
        .await;

        assert!(
            result.is_err(),
            "download must fail after all retries exhausted"
        );
        assert_eq!(
            client.call_count(),
            3,
            "expected 1 initial + 2 retries = 3 attempts total"
        );
    }

    // When the cancel flag is set between attempts, the retry loop must
    // abort without retrying — the user pressed Ctrl+C during the delay
    // window or between attempt 1's failure and attempt 2's start.
    #[tokio::test]
    async fn download_does_not_retry_when_cancelled_between_attempts() {
        let client = Arc::new(RetryClient::new(5)); // always fails
        let dir = TempDir::new().unwrap();
        let file = make_file("track.flac", 900, 10_000_000);

        let mut config = default_dl_config();
        config.max_retries = 2;
        config.retry_delay_secs = 0;

        // Cancel flag is already set — download_file must abort without
        // retrying (the retry loop checks the flag before each attempt).
        let cancel = Arc::new(AtomicBool::new(true));

        let result = download_file(
            client.as_ref() as &dyn SoulseekClient,
            &file,
            "testuser",
            dir.path(),
            &config,
            None,
            Some(&cancel),
        )
        .await;

        assert!(result.is_err(), "download must fail when cancelled");
        // Only the initial attempt should have been made — the cancel
        // flag was set before the retry loop entered, so no retry.
        assert_eq!(
            client.call_count(),
            1,
            "expected exactly 1 attempt (cancel before any retry), got {}",
            client.call_count()
        );
    }

    // max_retries=0 disables retries entirely — a single attempt, then give up.
    #[tokio::test]
    async fn download_max_retries_zero_makes_single_attempt() {
        let client = Arc::new(RetryClient::new(5)); // always fails
        let dir = TempDir::new().unwrap();
        let file = make_file("track.flac", 900, 10_000_000);

        let mut config = default_dl_config();
        config.max_retries = 0; // disabled
        config.retry_delay_secs = 0;

        let result = download_file(
            client.as_ref() as &dyn SoulseekClient,
            &file,
            "testuser",
            dir.path(),
            &config,
            None,
            None,
        )
        .await;

        assert!(result.is_err(), "download must fail after single attempt");
        assert_eq!(
            client.call_count(),
            1,
            "expected exactly 1 attempt (max_retries=0), got {}",
            client.call_count()
        );
    }

    #[tokio::test]
    async fn test_download_single_file_succeeds() {
        let client = MockClient::new();
        let dir = TempDir::new().unwrap();
        let file = make_file("track.flac", 900, 10_000_000);
        let config = default_dl_config();

        let result =
            download_file(&client, &file, "testuser", dir.path(), &config, None, None).await;
        assert!(result.is_ok());
    }

    // Regression guard: when a candidate has multiple files but some
    // fail to download, download_album must NOT return Ok — it should
    // fall back to the next candidate (or return an error if exhausted).
    // The failure is detected by download_file returning Err, which sets
    // failed=true and breaks the loop — the candidate is abandoned and
    // the staging dir is cleaned up.
    #[tokio::test]
    async fn download_album_fails_when_not_all_files_downloaded() {
        // Candidate has 3 files, but "02 - track.flac" always fails.
        // With max_retries=0 (no retries), the 1st file succeeds, the
        // 2nd fails → failed=true, loop breaks, candidate abandoned.
        let client = Arc::new(SelectiveFailClient::new(vec!["02 - track.flac"]));
        let dir = TempDir::new().unwrap();

        let candidates = vec![SearchResult {
            username: "user1".into(),
            speed: 500,
            slots: 1,
            files: vec![
                make_file("01 - track.flac", 900, 10_000_000),
                make_file("02 - track.flac", 900, 10_000_000),
                make_file("03 - track.flac", 900, 10_000_000),
            ],
        }];

        let mut config = default_dl_config();
        config.max_retries = 0; // no retries — fail fast
        config.retry_delay_secs = 0;

        let result = download_album(
            client.as_ref() as &dyn SoulseekClient,
            &candidates,
            dir.path(),
            &config,
            &default_filter_config_test(),
            None,
            None,
        )
        .await;

        // Must fail: file "02" returned Err → failed=true → candidate
        // abandoned → no more candidates → all candidates exhausted.
        assert!(
            result.is_err(),
            "download_album must fail when a file download fails, got: {result:?}"
        );
    }

    // Bug regression: a peer sharing multiple albums under the same artist
    // (e.g. "Abba\\[1992] Gold_ Greatest Hits\\..." and
    // "Abba\\[1993] More ABBA Gold\\...") returns files from ALL matching
    // album directories in one search result. download_album must only
    // download files from a single album directory, or the staging folder
    // becomes a jumbled mix of tracks from different albums.
    #[tokio::test]
    async fn download_album_downloads_only_one_album_directory_per_candidate() {
        let client = Arc::new(MockClient::new());
        let dir = TempDir::new().unwrap();

        let candidates = vec![SearchResult {
            username: "cassland".into(),
            speed: 900,
            slots: 1,
            // Two different album directories in the same peer's share:
            // [1992] Gold_ Greatest Hits (tracks 16-19) and
            // [1993] More ABBA Gold (tracks 01-08).
            files: vec![
                make_file(
                    "Musikk\\Abba\\[1992] Gold_ Greatest Hits\\01-16- One of Us.flac",
                    900,
                    10_000_000,
                ),
                make_file(
                    "Musikk\\Abba\\[1992] Gold_ Greatest Hits\\01-17- The Name of the Game (edit).flac",
                    900,
                    10_000_000,
                ),
                make_file(
                    "Musikk\\Abba\\[1993] More ABBA Gold\\01. summer night city.flac",
                    900,
                    10_000_000,
                ),
                make_file(
                    "Musikk\\Abba\\[1993] More ABBA Gold\\02. angeleyes.flac",
                    900,
                    10_000_000,
                ),
            ],
        }];

        let mut config = default_dl_config();
        config.max_retries = 0;
        config.retry_delay_secs = 0;

        let result = download_album(
            client.as_ref() as &dyn SoulseekClient,
            &candidates,
            dir.path(),
            &config,
            &default_filter_config_test(),
            None,
            None,
        )
        .await;

        // Must succeed (the single album's files are all downloadable).
        assert!(result.is_ok(), "download_album should succeed: {result:?}");

        // Only files from ONE album directory may be downloaded — never a
        // mix of two albums into the same staging folder.
        let downloaded = client
            .download_filenames
            .lock()
            .unwrap()
            .iter()
            .cloned()
            .collect::<Vec<_>>();
        let dirs: std::collections::HashSet<String> = downloaded
            .iter()
            .map(|n| n.rsplit(['/', '\\']).nth(1).unwrap_or("<root>").to_string())
            .collect();
        assert!(
            dirs.len() == 1,
            "files from multiple album directories were downloaded into the same staging folder: {downloaded:?}"
        );
    }

    // Same as above but with a fallback candidate that succeeds completely.
    // download_album should skip the failed candidate and succeed with
    // the fallback.
    #[tokio::test]
    async fn download_album_falls_back_to_next_candidate_on_failure() {
        let client = Arc::new(MockClient::new());
        let dir = TempDir::new().unwrap();

        let candidates = vec![SearchResult {
            username: "peer".into(),
            speed: 900,
            slots: 1,
            files: vec![
                // Small album: 2 files
                make_file(
                    "Music\\Abba\\Gold (Disc 1)\\01 - track.flac",
                    900,
                    10_000_000,
                ),
                make_file(
                    "Music\\Abba\\Gold (Disc 1)\\02 - track.flac",
                    900,
                    10_000_000,
                ),
                // Large album: 4 files — should be selected
                make_file(
                    "Music\\Abba\\Gold (Disc 2)\\01 - track.flac",
                    900,
                    10_000_000,
                ),
                make_file(
                    "Music\\Abba\\Gold (Disc 2)\\02 - track.flac",
                    900,
                    10_000_000,
                ),
                make_file(
                    "Music\\Abba\\Gold (Disc 2)\\03 - track.flac",
                    900,
                    10_000_000,
                ),
                make_file(
                    "Music\\Abba\\Gold (Disc 2)\\04 - track.flac",
                    900,
                    10_000_000,
                ),
            ],
        }];

        let mut config = default_dl_config();
        config.max_retries = 0;
        config.retry_delay_secs = 0;

        let result = download_album(
            client.as_ref() as &dyn SoulseekClient,
            &candidates,
            dir.path(),
            &config,
            &default_filter_config_test(),
            None,
            None,
        )
        .await;

        assert!(result.is_ok(), "download_album should succeed: {result:?}");
        let downloaded = client
            .download_filenames
            .lock()
            .unwrap()
            .iter()
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(
            downloaded.len(),
            4,
            "should download the larger group (Disc 2), got {downloaded:?}"
        );
        // All downloaded files must be from Disc 2
        assert!(
            downloaded.iter().all(|n| n.contains("Disc 2")),
            "expected all files from Disc 2, got {downloaded:?}"
        );
    }

    // When a peer shares two albums with different sizes, the LARGER album
    // group should be selected (not the first, not the last, not random).
    #[tokio::test]
    async fn download_album_prefers_largest_album_group() {
        let client = Arc::new(MockClient::new());
        let dir = TempDir::new().unwrap();

        let candidates = vec![SearchResult {
            username: "peer".into(),
            speed: 900,
            slots: 1,
            files: vec![
                // Small album: 2 files
                make_file(
                    "Music\\Abba\\Gold (Disc 1)\\01 - track.flac",
                    900,
                    10_000_000,
                ),
                make_file(
                    "Music\\Abba\\Gold (Disc 1)\\02 - track.flac",
                    900,
                    10_000_000,
                ),
                // Large album: 4 files — should be selected
                make_file(
                    "Music\\Abba\\Gold (Disc 2)\\01 - track.flac",
                    900,
                    10_000_000,
                ),
                make_file(
                    "Music\\Abba\\Gold (Disc 2)\\02 - track.flac",
                    900,
                    10_000_000,
                ),
                make_file(
                    "Music\\Abba\\Gold (Disc 2)\\03 - track.flac",
                    900,
                    10_000_000,
                ),
                make_file(
                    "Music\\Abba\\Gold (Disc 2)\\04 - track.flac",
                    900,
                    10_000_000,
                ),
            ],
        }];

        let mut config = default_dl_config();
        config.max_retries = 0;
        config.retry_delay_secs = 0;

        let result = download_album(
            client.as_ref() as &dyn SoulseekClient,
            &candidates,
            dir.path(),
            &config,
            &default_filter_config_test(),
            None,
            None,
        )
        .await;

        assert!(result.is_ok(), "download_album should succeed: {result:?}");
        let downloaded = client
            .download_filenames
            .lock()
            .unwrap()
            .iter()
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(
            downloaded.len(),
            4,
            "should download the larger group (Disc 2), got {downloaded:?}"
        );
        // All downloaded files must be from Disc 2
        assert!(
            downloaded.iter().all(|n| n.contains("Disc 2")),
            "expected all files from Disc 2, got {downloaded:?}"
        );
    }

    // Two albums with the same leaf directory name under different parents
    // must be treated as separate groups (e.g. "Abba\\Greatest Hits" vs
    // "Bee Gees\\Greatest Hits"). The grouping key is the full parent
    // path, not just the immediate directory name.
    #[tokio::test]
    async fn download_album_distinguishes_same_named_dirs_under_different_parents() {
        let client = Arc::new(MockClient::new());
        let dir = TempDir::new().unwrap();

        let candidates = vec![SearchResult {
            username: "peer".into(),
            speed: 900,
            slots: 1,
            files: vec![
                // Abba\Greatest Hits — 2 files
                make_file(
                    "Music\\Abba\\Greatest Hits\\01 - track.flac",
                    900,
                    10_000_000,
                ),
                make_file(
                    "Music\\Abba\\Greatest Hits\\02 - track.flac",
                    900,
                    10_000_000,
                ),
                // Bee Gees\Greatest Hits — 3 files (larger, should win)
                make_file(
                    "Music\\Bee Gees\\Greatest Hits\\01 - track.flac",
                    900,
                    10_000_000,
                ),
                make_file(
                    "Music\\Bee Gees\\Greatest Hits\\02 - track.flac",
                    900,
                    10_000_000,
                ),
                make_file(
                    "Music\\Bee Gees\\Greatest Hits\\03 - track.flac",
                    900,
                    10_000_000,
                ),
            ],
        }];

        let mut config = default_dl_config();
        config.max_retries = 0;
        config.retry_delay_secs = 0;

        let result = download_album(
            client.as_ref() as &dyn SoulseekClient,
            &candidates,
            dir.path(),
            &config,
            &default_filter_config_test(),
            None,
            None,
        )
        .await;

        assert!(result.is_ok(), "download_album should succeed: {result:?}");
        let downloaded = client
            .download_filenames
            .lock()
            .unwrap()
            .iter()
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(
            downloaded.len(),
            3,
            "should download Bee Gees (larger group), got {downloaded:?}"
        );
        assert!(
            downloaded.iter().all(|n| n.contains("Bee Gees")),
            "expected all files from Bee Gees, got {downloaded:?}"
        );
    }

    #[tokio::test]
    async fn test_download_monitors_speed() {
        let client = MockClient::new();
        *client.download_speed.lock().unwrap() = 100_000; // 100 KB/s — slow
        let dir = TempDir::new().unwrap();
        let file = make_file("track.flac", 900, 10_000_000);

        let mut config = default_dl_config();
        config.min_upload_speed_kbps = 200; // require >200 KB/s
        config.speed_check_wait_secs = 0;

        let result =
            download_file(&client, &file, "testuser", dir.path(), &config, None, None).await;
        // Should detect slow speed and return error
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_download_slow_speed_fails() {
        let client = MockClient::new();
        *client.download_speed.lock().unwrap() = 100_000; // Always slow → will retry and fail
        let dir = TempDir::new().unwrap();
        let file = make_file("track.flac", 900, 10_000_000);

        let mut config = default_dl_config();
        config.min_upload_speed_kbps = 1_000_000;
        config.max_retries = 2;
        config.retry_delay_secs = 0;

        let result =
            download_file(&client, &file, "testuser", dir.path(), &config, None, None).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_download_with_candidate_fallback() {
        let client = MockClient::new();
        *client.download_speed.lock().unwrap() = 1_000_000; // Fast enough
        let dir = TempDir::new().unwrap();

        let candidates = vec![SearchResult {
            username: "user1".into(),
            speed: 300,
            slots: 1,
            files: vec![make_file("track.flac", 900, 10_000_000)],
        }];
        let config = default_dl_config();

        let result = download_album(
            &client,
            &candidates,
            dir.path(),
            &config,
            &default_filter_config_test(),
            None,
            None,
        )
        .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_failed_candidate_cleans_up_part_files() {
        // A candidate whose download fails partway must leave the staging
        // directory clean — no completed tracks, no stale `.part` files.
        // Regression: the orphan loop only removed files returned as `Ok`,
        // leaving `.part` files behind after a failed transfer.
        let client = MockClient::new();
        // Slow speed forces the speed-check failure on the first file.
        *client.download_speed.lock().unwrap() = 100_000; // 100 KB/s
        let dir = TempDir::new().unwrap();

        // Simulate a leftover `.part` file (as the vendor lib leaves behind
        // on an interrupted transfer) plus a completed track.
        std::fs::write(dir.path().join("01 - track.flac.part"), b"partial").unwrap();
        std::fs::write(dir.path().join("02 - track.flac"), b"complete").unwrap();

        let candidates = vec![SearchResult {
            username: "user1".into(),
            speed: 300,
            slots: 1,
            files: vec![make_file("01 - track.flac", 900, 10_000_000)],
        }];
        let mut config = default_dl_config();
        // Require impossibly fast upload so the speed check fails.
        config.min_upload_speed_kbps = 10_000_000;
        config.speed_check_wait_secs = 0;

        let result = download_album(
            &client,
            &candidates,
            dir.path(),
            &config,
            &default_filter_config_test(),
            None,
            None,
        )
        .await;
        assert!(result.is_err());

        // The staging directory must be gone — no partial downloads at all.
        assert!(
            !dir.path().exists(),
            "staging dir must be removed after failed candidate"
        );
    }

    #[tokio::test]
    async fn test_cancellation_flag_aborts_download_and_cleans_up() {
        // When the cancellation flag is set (Ctrl+C / SIGINT), download_album
        // must abort and clean the staging directory — including any `.part`
        // files the vendor library would leave behind.

        let client = Arc::new(MockClient::new());
        *client.search_results.lock().unwrap() = vec![SearchResult {
            username: "user1".into(),
            speed: 500,
            slots: 1,
            files: vec![make_file("01 - track.flac", 900, 10_000_000)],
        }];

        let dir = TempDir::new().unwrap();
        // Simulate a leftover .part file from a previous run.
        std::fs::write(dir.path().join("01 - track.flac.part"), b"stale partial").unwrap();

        let config = default_dl_config();
        let filter_config = default_filter_config_test();

        // Cancellation is already requested — download_album should abort
        // before queuing the download to the peer.
        let cancelled = Arc::new(AtomicBool::new(true));
        let results = client.search_results.lock().unwrap().clone();
        let result = download_album(
            client.as_ref(),
            &results,
            dir.path(),
            &config,
            &filter_config,
            None,
            Some(&cancelled),
        )
        .await;
        assert!(result.is_err(), "download_album must abort when cancelled");
        assert!(
            result.unwrap_err().to_string().contains("cancelled"),
            "error must indicate cancellation"
        );

        // The staging directory must be gone — no partial downloads at all.
        assert!(
            !dir.path().exists(),
            "staging dir must be removed after cancellation"
        );
    }
}
