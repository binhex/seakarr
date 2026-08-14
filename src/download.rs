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
                // Update progress bar if present
                if let Some(bar) = &bar {
                    bar.set_position(bytes_downloaded);
                    bar.set_prefix(format_speed(speed_bytes_per_sec));
                }
            }
            Ok(Some(DownloadStatus::Completed)) => {
                if let Some(bar) = &bar {
                    // Snap to 100% before finishing — the final InProgress
                    // may have left the bar below the total. finish()
                    // keeps the final frame visible (cleared by the
                    // runner's p.clear() at end of run).
                    bar.set_position(last_total_bytes);
                    bar.finish();
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
                    // Do NOT retry the same user: an unresponsive peer
                    // (silent transfer handshake) burns timeout_secs per
                    // attempt, and retrying it N times just delays the
                    // ranked candidate fallback by N × timeout_secs. The
                    // candidate list IS the retry mechanism.
                    tracing::warn!(
                        "Download of {} from {} failed: {e}",
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
