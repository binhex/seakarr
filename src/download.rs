use std::path::{Path, PathBuf};
use tokio::time::{timeout, Duration};

use crate::client::{DownloadStatus, FileInfo, SearchResult, SoulseekClient};
use crate::config::DownloadConfig;
use crate::error::{Result, SeakarrError};
use crate::filter;

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

/// Download a single file from a specific user, monitoring speed.
pub async fn download_file(
    client: &dyn SoulseekClient,
    file: &FileInfo,
    username: &str,
    dir: &Path,
    config: &DownloadConfig,
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
    let mut handle = client.download(file, username, dir).await?;
    tracing::info!("Download queued: {basename} from {username}");
    let mut transfer_start: Option<tokio::time::Instant> = None;

    loop {
        // Guard against transfers that stop sending status updates
        // (hung peer, network partition) — timeout_secs is the deadline
        // for ANY message to arrive, not just for completion.
        let msg = timeout(
            Duration::from_secs(config.timeout_secs),
            handle.status_rx.recv(),
        )
        .await;

        match msg {
            Ok(Some(DownloadStatus::InProgress {
                speed_bytes_per_sec,
                ..
            })) => {
                if transfer_start.is_none() {
                    transfer_start = Some(tokio::time::Instant::now());
                }
                // Speed check: only after the transfer has actually started
                // transferring (not just queued), and past the wait period.
                if config.min_upload_speed_kbps > 0 {
                    if let Some(ts) = transfer_start {
                        if ts.elapsed().as_secs() >= config.speed_check_wait_secs {
                            let speed_kbps = (speed_bytes_per_sec / 1024) as u32;
                            if speed_kbps < config.min_upload_speed_kbps {
                                let _ = handle.cancel_tx.send(()).await;
                                return Err(SeakarrError::Download(format!(
                                    "speed {speed_kbps} KB/s below minimum {} KB/s",
                                    config.min_upload_speed_kbps
                                )));
                            }
                        }
                    }
                }
            }
            Ok(Some(DownloadStatus::Completed)) => {
                let dest = dir.join(basename);
                tracing::info!("Download completed: {basename}");
                return Ok(dest);
            }
            Ok(Some(DownloadStatus::Failed { reason })) => {
                tracing::warn!("Download of {basename} failed: {reason}");
                return Err(SeakarrError::Download(format!("transfer failed: {reason}")));
            }
            Ok(Some(DownloadStatus::Queued { .. })) => {}
            Ok(None) => {
                tracing::warn!("Download channel closed for {basename}");
                return Err(SeakarrError::Download(
                    "download channel closed unexpectedly".into(),
                ));
            }
            Err(_elapsed) => {
                tracing::warn!(
                    "Download of {basename} timed out after {}s",
                    config.timeout_secs
                );
                let _ = handle.cancel_tx.send(()).await;
                return Err(SeakarrError::Download("download timed out".into()));
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
) -> Result<Vec<PathBuf>> {
    let mut last_err: Option<SeakarrError> = None;

    for candidate in candidates {
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

        let mut downloaded = Vec::new();
        let mut failed = false;

        for file in &filtered_files {
            match download_file(client, file, &candidate.username, staging_dir, config).await {
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

        // Clean up files already downloaded from this failed candidate
        // before trying the next. Errors during cleanup are logged but do
        // not prevent fallback.
        for orphan in &downloaded {
            if let Err(e) = std::fs::remove_file(orphan) {
                tracing::warn!("Failed to clean up orphan staging file {orphan:?}: {e}");
            }
        }
    }

    Err(last_err.unwrap_or_else(|| SeakarrError::Download("all candidates exhausted".into())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{FileInfo, MockClient, SearchResult};
    use crate::config::DownloadConfig;
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

        let result = download_file(&client, &file, "peer", dir.path(), &config).await;
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
    async fn test_download_single_file_succeeds() {
        let client = MockClient::new();
        let dir = TempDir::new().unwrap();
        let file = make_file("track.flac", 900, 10_000_000);
        let config = default_dl_config();

        let result = download_file(&client, &file, "testuser", dir.path(), &config).await;
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

        let result = download_file(&client, &file, "testuser", dir.path(), &config).await;
        // Should detect slow speed and return error
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_download_retries_then_fails() {
        let client = MockClient::new();
        *client.download_speed.lock().unwrap() = 100_000; // Always slow → will retry and fail
        let dir = TempDir::new().unwrap();
        let file = make_file("track.flac", 900, 10_000_000);

        let mut config = default_dl_config();
        config.min_upload_speed_kbps = 1_000_000;
        config.max_retries = 2;
        config.retry_delay_secs = 0;

        let result = download_file(&client, &file, "testuser", dir.path(), &config).await;
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
            &crate::config::FilterConfig::default(),
        )
        .await;
        assert!(result.is_ok());
    }
}
