use async_trait::async_trait;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use tokio::sync::mpsc;

use crate::error::{Result, SeakarrError};
use crate::formatting::{format_bytes, format_speed};
use crate::progress::is_interactive;

// ── Domain types ──

#[derive(Debug, Clone)]
pub struct SearchResult {
    pub username: String,
    pub speed: u32, // advertised upload speed
    pub slots: u8,  // free upload slots
    pub files: Vec<FileInfo>,
}

#[derive(Debug, Clone)]
pub struct FileInfo {
    pub name: String,
    pub size: u64,
    pub attribs: HashMap<u32, u32>, // key 0 = bitrate, 1 = duration, 2 = VBR, 4 = sample rate, 5 = bit depth
}

#[derive(Debug, Clone)]
pub struct UserInfo {
    pub username: String,
    pub status: UserStatus,
}

#[derive(Debug, Clone)]
pub enum UserStatus {
    Online,
    Away,
    Offline,
}

#[derive(Debug, Clone)]
pub enum DownloadStatus {
    Queued {
        queue_position: u32,
    },
    InProgress {
        speed_bytes_per_sec: u64,
        bytes_downloaded: u64,
        total_bytes: u64,
    },
    Completed,
    Failed {
        reason: String,
    },
}

pub struct DownloadHandle {
    pub status_rx: mpsc::Receiver<DownloadStatus>,
    pub cancel_tx: mpsc::Sender<()>,
}

// ── Trait ──

#[async_trait]
pub trait SoulseekClient: Send + Sync {
    async fn login(
        &self,
        username: &str,
        password: &str,
        server: &str,
        listen_port: u16,
    ) -> Result<()>;
    async fn search(&self, query: &str, timeout_secs: u64) -> Result<Vec<SearchResult>>;
    async fn download(&self, file: &FileInfo, username: &str, dir: &Path)
        -> Result<DownloadHandle>;
    async fn user_info(&self, username: &str) -> Result<UserInfo>;
}

// ── Mock implementation for testing ──

pub struct MockClient {
    pub search_results: Mutex<Vec<SearchResult>>,
    /// Per-query override map. When a query has an entry here, `search()`
    /// returns it instead of the static `search_results`.
    pub search_results_by_query: Mutex<HashMap<String, Vec<SearchResult>>>,
    /// Every query string passed to `search()`, in call order.
    pub search_queries: Mutex<Vec<String>>,
    pub download_speed: Mutex<u64>,
    pub login_should_fail: Mutex<bool>,
    /// Records the last filename passed to `download()` so tests can assert
    /// the wire filename is the full share-relative path (regression guard
    /// for the UploadDenied-everywhere bug).
    pub last_download_filename: Mutex<Option<String>>,
    /// Every filename passed to `download()`, in call order.
    pub download_filenames: Mutex<Vec<String>>,
}

impl MockClient {
    pub fn new() -> Self {
        MockClient {
            search_results: Mutex::new(vec![]),
            search_results_by_query: Mutex::new(HashMap::new()),
            search_queries: Mutex::new(vec![]),
            download_speed: Mutex::new(1_000_000), // 1 MB/s
            login_should_fail: Mutex::new(false),
            last_download_filename: Mutex::new(None),
            download_filenames: Mutex::new(vec![]),
        }
    }

    /// Helper: create a mock SearchResult with minimal FileInfo.
    pub fn mock_search_result(
        username: &str,
        speed: u32,
        slots: u8,
        files: Vec<(&str, u64, u32)>,
    ) -> SearchResult {
        SearchResult {
            username: username.into(),
            speed,
            slots,
            files: files
                .into_iter()
                .map(|(name, size, bitrate)| {
                    let mut attribs = HashMap::new();
                    attribs.insert(0, bitrate); // bitrate
                    FileInfo {
                        name: name.into(),
                        size,
                        attribs,
                    }
                })
                .collect(),
        }
    }
}

impl Default for MockClient {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SoulseekClient for MockClient {
    async fn login(
        &self,
        _username: &str,
        _password: &str,
        _server: &str,
        _listen_port: u16,
    ) -> Result<()> {
        if *self.login_should_fail.lock().unwrap() {
            return Err(SeakarrError::Auth {
                attempts: 1,
                reason: "invalid credentials".into(),
            });
        }
        Ok(())
    }

    async fn search(&self, query: &str, _timeout_secs: u64) -> Result<Vec<SearchResult>> {
        self.search_queries.lock().unwrap().push(query.to_string());
        if let Some(results) = self.search_results_by_query.lock().unwrap().get(query) {
            return Ok(results.clone());
        }
        Ok(self.search_results.lock().unwrap().clone())
    }

    async fn download(
        &self,
        file: &FileInfo,
        _username: &str,
        _dir: &Path,
    ) -> Result<DownloadHandle> {
        *self.last_download_filename.lock().unwrap() = Some(file.name.clone());
        self.download_filenames
            .lock()
            .unwrap()
            .push(file.name.clone());
        let (status_tx, status_rx) = mpsc::channel(32);
        let (cancel_tx, mut cancel_rx) = mpsc::channel(1);
        let speed = *self.download_speed.lock().unwrap();
        let total = 10_000_000u64;

        // Simulate download progress in a background task
        tokio::spawn(async move {
            for i in 1..=5 {
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
                        speed_bytes_per_sec: speed,
                        bytes_downloaded: (total / 5) * i,
                        total_bytes: total,
                    })
                    .await;
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            let _ = status_tx.send(DownloadStatus::Completed).await;
        });

        Ok(DownloadHandle {
            status_rx,
            cancel_tx,
        })
    }

    async fn user_info(&self, _username: &str) -> Result<UserInfo> {
        Ok(UserInfo {
            username: _username.into(),
            status: UserStatus::Online,
        })
    }
}

// ── Real client (soulseek-rs-lib wrapper) ──
//
// Vendored soulseek-rs-lib v14.0.0 (workspace member at vendor/soulseek-rs-lib) with
// a local peer-registry cap. The crate's API is synchronous: `Client::connect()`/`login()` block on the
// server, `Client::search()` blocks for the whole timeout window, and
// `Client::download()` returns `(Download, std::sync::mpsc::Receiver<DownloadStatus>)`.
// All blocking calls are therefore wrapped in `spawn_blocking`.

// Note: the crate's lib target is named `soulseek_rs` (see its Cargo.toml
// `[lib] name`), even though the dependency key is `soulseek-rs-lib`.
use soulseek_rs::actor::server_actor::PeerAddress;
use soulseek_rs::client::{Client, ClientSettings};
use soulseek_rs::error::SoulseekRs;
use soulseek_rs::types::DownloadStatus as SsDownloadStatus;
use soulseek_rs::types::File as SsFile;
use soulseek_rs::types::SearchResult as SsSearchResult;
use soulseek_rs::types::UserStatus as SsUserStatus;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::Arc;
use std::time::{Duration as StdDuration, Instant};

/// How long to wait for the server's user-status replies before reporting the
/// user as offline.
const USER_INFO_TIMEOUT_SECS: u64 = 10;

/// `RealClient` wraps a connected `soulseek_rs_lib::Client`.
///
/// The trait methods only receive `&self`, so the connected client lives
/// behind interior mutability: a tokio mutex holding an `Arc<Client>`.
/// `login()` builds a fresh client per attempt and stores it on success;
/// search/download/user queries clone the `Arc` so their blocking calls can
/// run on `spawn_blocking` without holding the lock.
pub struct RealClient {
    /// The connected soulseek client, set by `login()`.
    inner: tokio::sync::Mutex<Option<Arc<Client>>>,
    /// Total connect+login attempts per `login()` call.
    login_retries: u32,
    /// Base delay between login attempts; doubled after every failure.
    login_retry_delay_secs: u64,
}

impl RealClient {
    /// Default retry settings mirror the config defaults in `SoulseekConfig`
    /// (`login_retries: 3`, `login_retry_delay_secs: 5`).
    pub fn new() -> Self {
        Self::with_login_retries(3, 5)
    }

    /// Configure the login retry/backoff policy. `retries` is the total number
    /// of connect+login attempts; `delay_secs` is the first inter-attempt
    /// delay, doubled after every failure.
    pub fn with_login_retries(retries: u32, delay_secs: u64) -> Self {
        RealClient {
            inner: tokio::sync::Mutex::new(None),
            login_retries: retries.max(1),
            login_retry_delay_secs: delay_secs,
        }
    }

    /// Clone of the connected client, or an error if not logged in.
    async fn connected_client(&self) -> Result<Arc<Client>> {
        self.inner
            .lock()
            .await
            .as_ref()
            .cloned()
            .ok_or_else(|| SeakarrError::Client("not connected: call login() first".into()))
    }
}

impl Default for RealClient {
    fn default() -> Self {
        Self::new()
    }
}

/// Split a `host:port` server string into a `PeerAddress`.
fn parse_server_address(server: &str) -> Result<PeerAddress> {
    let (host, port) = server.rsplit_once(':').ok_or_else(|| {
        SeakarrError::Client(format!(
            "invalid server address '{server}': expected host:port"
        ))
    })?;
    if host.is_empty() {
        return Err(SeakarrError::Client(format!(
            "invalid server address '{server}': missing host"
        )));
    }
    let port: u16 = port.parse().map_err(|_| {
        SeakarrError::Client(format!(
            "invalid server address '{server}': bad port '{port}'"
        ))
    })?;
    Ok(PeerAddress::new(host.to_string(), port))
}

/// Map a crate search result to the domain type.
fn ss_search_result_to_domain(result: SsSearchResult) -> SearchResult {
    SearchResult {
        username: result.username,
        speed: result.speed,
        slots: result.slots,
        files: result.files.into_iter().map(ss_file_to_domain).collect(),
    }
}

/// Map a crate file entry to the domain type.
fn ss_file_to_domain(file: SsFile) -> FileInfo {
    FileInfo {
        name: file.name,
        size: file.size,
        attribs: file.attribs,
    }
}

/// Map a crate download status to the domain type.
fn ss_download_status_to_domain(status: SsDownloadStatus) -> DownloadStatus {
    match status {
        // The crate sends Queued without a position (it tracks queue position
        // internally and only updates its own store); the domain type wants a
        // number, so report 0.
        SsDownloadStatus::Queued => DownloadStatus::Queued { queue_position: 0 },
        SsDownloadStatus::InProgress {
            bytes_downloaded,
            total_bytes,
            speed_bytes_per_sec,
        } => DownloadStatus::InProgress {
            speed_bytes_per_sec: speed_bytes_per_sec.max(0.0).round() as u64,
            bytes_downloaded,
            total_bytes,
        },
        // The domain type has no Paused variant; keep reporting progress so
        // the consumer's speed/timeout checks keep running.
        SsDownloadStatus::Paused {
            bytes_downloaded,
            total_bytes,
        } => DownloadStatus::InProgress {
            speed_bytes_per_sec: 0,
            bytes_downloaded,
            total_bytes,
        },
        SsDownloadStatus::Completed => DownloadStatus::Completed,
        SsDownloadStatus::Failed(reason) => DownloadStatus::Failed {
            reason: reason.unwrap_or_else(|| "transfer failed".to_string()),
        },
        SsDownloadStatus::TimedOut => DownloadStatus::Failed {
            reason: "transfer timed out".to_string(),
        },
    }
}

/// Map a crate user status to the domain type.
fn ss_user_status_to_domain(status: SsUserStatus) -> UserStatus {
    match status {
        SsUserStatus::Online => UserStatus::Online,
        SsUserStatus::Away => UserStatus::Away,
        SsUserStatus::Offline => UserStatus::Offline,
    }
}

/// Forward download status updates from the crate's std mpsc channel to the
/// consumer's tokio channel, until a terminal status, a send failure, a
/// channel disconnect, or a cancellation request.
///
/// Runs on a blocking-pool thread for the whole transfer. It must terminate
/// promptly once cancellation is requested — even when the transfer stalls
/// and no status updates arrive — otherwise the task outlives the run it
/// belongs to and the tokio runtime drop blocks forever on it (a runtime
/// waits for spawned blocking tasks to finish), leaving the process unable
/// to exit.
fn forward_transfer_status(
    crate_rx: &std::sync::mpsc::Receiver<SsDownloadStatus>,
    forward_tx: &tokio::sync::mpsc::Sender<DownloadStatus>,
    forward_cancelled: &AtomicBool,
    interactive: bool,
    filename: &str,
) {
    // Sanitize peer-supplied filename for logging: strip control characters
    // to prevent terminal injection.
    let safe_filename: String = filename.chars().filter(|c| !c.is_control()).collect();
    // Throttle per-status progress logs: the crate emits InProgress
    // updates several times a second, which floods the console. Log
    // state transitions immediately, but progress at most once per
    // 5 seconds.
    let mut last_progress_log = std::time::Instant::now()
        .checked_sub(std::time::Duration::from_secs(6))
        .unwrap();
    loop {
        match crate_rx.recv_timeout(StdDuration::from_millis(200)) {
            Ok(status) => {
                if matches!(
                    &status,
                    SsDownloadStatus::InProgress { .. } | SsDownloadStatus::Paused { .. }
                ) {
                    if !interactive {
                        // Non-interactive: human-friendly fallback log,
                        // still throttled to once per 5 seconds. When
                        // interactive the line is fully suppressed — the
                        // progress bar handles display.
                        if last_progress_log.elapsed() >= std::time::Duration::from_secs(5) {
                            // Log both InProgress and Paused (Paused
                            // maps to InProgress with speed=0 in the
                            // domain type, so display it consistently).
                            let (bd, tb, sp) = match &status {
                                SsDownloadStatus::InProgress {
                                    bytes_downloaded,
                                    total_bytes,
                                    speed_bytes_per_sec,
                                } => (*bytes_downloaded, *total_bytes, *speed_bytes_per_sec),
                                SsDownloadStatus::Paused {
                                    bytes_downloaded,
                                    total_bytes,
                                } => (*bytes_downloaded, *total_bytes, 0.0),
                                // The outer `matches!` guard ensures only
                                // InProgress or Paused reach here.
                                _ => unreachable!("outer matches! guard should have filtered this"),
                            };
                            tracing::info!(
                                "Downloading: {safe_filename} — {} / {} @ {}",
                                format_bytes(bd),
                                format_bytes(tb),
                                format_speed(sp.round() as u64),
                            );
                            last_progress_log = std::time::Instant::now();
                        }
                    }
                } else {
                    tracing::info!("Bridge status for {safe_filename}: {status:?}");
                }
                if forward_cancelled.load(Ordering::SeqCst) {
                    break;
                }
                let mapped = ss_download_status_to_domain(status);
                let terminal = matches!(
                    mapped,
                    DownloadStatus::Completed | DownloadStatus::Failed { .. }
                );
                if forward_tx.blocking_send(mapped).is_err() {
                    break;
                }
                if terminal {
                    break;
                }
            }
            // No update yet (e.g. a long queue wait) — keep polling. The
            // consumer enforces its own overall timeout and cancels via
            // `cancel_tx`.
            Err(RecvTimeoutError::Timeout) => {
                // Honour cancellation even when the transfer is stalled and no
                // status updates arrive. Without this the blocking task spins
                // forever, and the tokio runtime drop blocks on it after the
                // run — the process never exits, and Ctrl+C cannot terminate
                // it (the tokio signal handler swallows SIGINT).
                if forward_cancelled.load(Ordering::SeqCst) {
                    break;
                }
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => {
                tracing::info!("Bridge disconnected for {safe_filename}");
                break;
            }
        }
    }
}

#[async_trait]
impl SoulseekClient for RealClient {
    async fn login(
        &self,
        username: &str,
        password: &str,
        server: &str,
        listen_port: u16,
    ) -> Result<()> {
        let address = parse_server_address(server)?;
        let mut last_reason = "login failed".to_string();
        let enable_listen = listen_port > 0;

        for attempt in 0..self.login_retries {
            // A fresh client per attempt: a failed connect/login leaves the
            // crate's actor threads in an unknown state, so never reuse it.
            let settings = ClientSettings {
                username: username.to_string(),
                password: password.to_string(),
                server_address: address.clone(),
                enable_listen,
                listen_port,
                ..ClientSettings::default()
            };
            let mut client = Client::with_settings(settings);

            // connect() spawns actor threads; login() blocks up to ~45 s for
            // the server verdict — both must run off the async executor. On
            // success the client is moved back out of the task for storage.
            let result = tokio::task::spawn_blocking(move || {
                client.connect()?;
                client.login().map(|logged_in| (client, logged_in))
            })
            .await
            .map_err(|e| SeakarrError::Auth {
                attempts: attempt + 1,
                reason: format!("login task panicked: {e}"),
            })?;

            match result {
                Ok((client, true)) => {
                    *self.inner.lock().await = Some(Arc::new(client));
                    if enable_listen {
                        tracing::info!("[listener] enabled on port {listen_port}");
                    } else {
                        tracing::info!("[listener] disabled (listen_port=0)");
                    }
                    return Ok(());
                }
                Ok((_, false)) => {
                    last_reason = "the server rejected the login".to_string();
                }
                Err(e) => last_reason = e.to_string(),
            }

            if attempt + 1 < self.login_retries {
                let delay = self
                    .login_retry_delay_secs
                    .saturating_mul(1u64 << attempt.min(20));
                tokio::time::sleep(StdDuration::from_secs(delay)).await;
            }
        }

        Err(SeakarrError::Auth {
            attempts: self.login_retries,
            reason: last_reason,
        })
    }

    async fn search(&self, query: &str, timeout_secs: u64) -> Result<Vec<SearchResult>> {
        let client = self.connected_client().await?;
        let query_owned = query.to_string();
        let query_for_task = query_owned.clone();
        let results = tokio::task::spawn_blocking(move || {
            // Blocks the calling thread for the whole timeout window.
            client.search(&query_for_task, StdDuration::from_secs(timeout_secs))
        })
        .await
        .map_err(|e| SeakarrError::Client(format!("search task panicked: {e}")))?
        .map_err(|e| SeakarrError::Client(format!("search '{query_owned}' failed: {e}")))?;

        Ok(results
            .into_iter()
            .map(ss_search_result_to_domain)
            .collect())
    }

    async fn download(
        &self,
        file: &FileInfo,
        username: &str,
        dir: &Path,
    ) -> Result<DownloadHandle> {
        let client = self.connected_client().await?;
        let filename = file.name.clone();
        let username_owned = username.to_string();
        let size = file.size;
        let download_dir = dir.to_string_lossy().into_owned();

        // Queue the transfer with the soulseek client. Returns quickly; the
        // transfer itself runs on the crate's background threads.
        let queue_client = client.clone();
        let queue_filename = filename.clone();
        let queue_username = username_owned.clone();
        let (download_handle, crate_rx) = tokio::task::spawn_blocking(move || {
            queue_client.download(queue_filename, queue_username, size, download_dir)
        })
        .await
        .map_err(|e| SeakarrError::Download(format!("download task panicked: {e}")))?
        .map_err(|e| {
            SeakarrError::Download(format!(
                "failed to queue download of '{filename}' from '{username_owned}': {e}"
            ))
        })?;

        // Bridge the crate's std mpsc status channel onto a tokio channel, and
        // watch for cancellation, in detached tasks.
        let (status_tx, status_rx) = mpsc::channel(32);
        let (cancel_tx, mut cancel_rx) = mpsc::channel(1);
        let cancelled = Arc::new(AtomicBool::new(false));

        // Forward statuses until a terminal status, a cancellation, or the
        // crate closes the channel. Afterwards drop any stale download record
        // so a retry of the same file is not shadowed (same md5 token).
        let forward_tx = status_tx.clone();
        let forward_cancelled = cancelled.clone();
        let bridge_client = client.clone();
        let bridge_username = username_owned.clone();
        let bridge_filename = filename.clone();
        // Keep the Download handle alive — it holds the internal Sender that
        // the crate uses to push status updates. Dropping it closes the channel
        // and silently kills the transfer.
        let _download_handle = download_handle;
        tokio::task::spawn_blocking(move || {
            // Keep _download_handle alive for the entire bridge lifetime.
            let _keep = &_download_handle;
            tracing::info!("Bridge started for {bridge_filename} from {bridge_username}");
            // Evaluate once: isatty is a syscall per call, and the
            // interactive/non-interactive decision does not change mid-transfer.
            let interactive = is_interactive();
            forward_transfer_status(
                &crate_rx,
                &forward_tx,
                &forward_cancelled,
                interactive,
                &bridge_filename,
            );
            let _ = bridge_client.remove_download(&bridge_username, &bridge_filename);
        });

        // Cancellation: stop the wire transfer, drop the record, report.
        let cancel_client = client;
        let cancel_username = username_owned;
        let cancel_filename = filename;
        tokio::spawn(async move {
            if cancel_rx.recv().await.is_none() {
                return;
            }
            cancelled.store(true, Ordering::SeqCst);
            let _ = cancel_client.pause_download(&cancel_username, &cancel_filename);
            let _ = cancel_client.remove_download(&cancel_username, &cancel_filename);
            let _ = status_tx
                .send(DownloadStatus::Failed {
                    reason: "cancelled".into(),
                })
                .await;
        });

        Ok(DownloadHandle {
            status_rx,
            cancel_tx,
        })
    }

    async fn user_info(&self, username: &str) -> Result<UserInfo> {
        let client = self.connected_client().await?;
        let username_owned = username.to_string();
        let username_for_task = username_owned.clone();
        let status =
            tokio::task::spawn_blocking(move || -> std::result::Result<UserStatus, SoulseekRs> {
                // Ask the server, then poll for the asynchronous replies.
                client.request_user_info(&username_for_task)?;
                let deadline = Instant::now() + StdDuration::from_secs(USER_INFO_TIMEOUT_SECS);
                loop {
                    if let Some(info) = client.user_info(&username_for_task) {
                        if let Some(presence) = info.presence {
                            return Ok(ss_user_status_to_domain(presence.status));
                        }
                    }
                    if Instant::now() >= deadline {
                        // The server did not answer; report the user as offline.
                        return Ok(UserStatus::Offline);
                    }
                    std::thread::sleep(StdDuration::from_millis(100));
                }
            })
            .await
            .map_err(|e| SeakarrError::Client(format!("user info task panicked: {e}")))?
            .map_err(|e| {
                SeakarrError::Client(format!(
                    "user info request for '{username_owned}' failed: {e}"
                ))
            })?;

        Ok(UserInfo {
            username: username_owned,
            status,
        })
    }
}

#[cfg(test)]
mod real_client_tests {
    use super::*;
    use std::collections::HashMap;

    // Compile-time proof that the trait's `Send + Sync` supertrait holds.
    #[test]
    fn real_client_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<RealClient>();
    }

    // Regression guard: a stalled transfer (no status updates arriving) must
    // still terminate the status bridge once cancellation is requested.
    // Previously the recv_timeout Timeout arm never checked the cancel flag,
    // so the blocking task spun forever and the tokio runtime drop blocked on
    // it after the run — the process never exited, and Ctrl+C could not kill
    // it (the tokio signal handler swallowed SIGINT).
    #[test]
    fn forward_transfer_status_exits_on_cancellation_during_stalled_transfer() {
        // A channel that never receives a status, and stays open (the sender
        // is held until after the assertion — a dropped sender would close
        // the channel and exit the bridge via the Disconnected arm, masking
        // the bug).
        let (tx, rx) = std::sync::mpsc::channel::<SsDownloadStatus>();
        let (fwd_tx, fwd_rx) = tokio::sync::mpsc::channel(32);
        // Cancellation is already requested before the bridge starts.
        let cancelled = AtomicBool::new(true);
        let (done_tx, done_rx) = std::sync::mpsc::channel();

        let worker = std::thread::spawn(move || {
            forward_transfer_status(&rx, &fwd_tx, &cancelled, false, "01 - track.flac");
            done_tx.send(()).unwrap();
        });

        // The bridge polls every 200 ms, so with the cancel flag set it must
        // return almost immediately. Give it 5 s — with the bug it never
        // returns and the test times out (RED).
        done_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("status bridge did not exit on cancellation (hang reproduced)");
        worker.join().unwrap();
        drop(tx);
        drop(fwd_rx);
    }

    // Regression guard for the peer-connection bug: soulseek-rs-lib v8.0.0
    // never processes GetPeerAddress responses, so downloads hang forever.
    // Verified live that v14.0.0 works (official CLI resolved a peer and
    // requested a file). The fix for "concurrent invocations silently seeing
    // nothing" landed in v11.0.0. This test pins the dependency to a version
    // with the fix, AND asserts the vendored peer cap survives — upstream
    // 14.0.0 alone would satisfy the version check while reintroducing the
    // thread explosion.
    #[test]
    fn soulseek_lib_version_has_peer_connection_fix() {
        let manifest = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"))
            .expect("Cargo.toml readable");

        // 1. The dependency must be the vendored copy, declared as a path
        // dependency (workspace member) so `cargo test` exercises its tests.
        assert!(
            manifest
                .lines()
                .any(|l| l.trim() == "soulseek-rs-lib = { path = \"vendor/soulseek-rs-lib\" }"),
            "soulseek-rs-lib must be a path dependency on the vendored copy"
        );

        // 1b. The vendored crate must be a workspace member in BOTH lists,
        // or plain `cargo test` at the root silently stops running its
        // regression tests.
        let members_line = manifest
            .lines()
            .find(|l| l.trim_start().starts_with("members"))
            .expect("[workspace] members present");
        let default_members_line = manifest
            .lines()
            .find(|l| l.trim_start().starts_with("default-members"))
            .expect("[workspace] default-members present");
        assert!(
            members_line.contains("vendor/soulseek-rs-lib")
                && default_members_line.contains("vendor/soulseek-rs-lib"),
            "vendored crate must be in both workspace members and default-members"
        );

        // 2. The vendored crate's own version must be >= 11 (the release
        // with the GetPeerAddress / concurrent-invocations fix). Parse only
        // inside the [package] section so a stray dependency `version` line
        // can never confuse the parse.
        let vendor_manifest = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/vendor/soulseek-rs-lib/Cargo.toml"
        ))
        .expect("vendored Cargo.toml readable");
        let mut in_package = false;
        let mut package_version: Option<String> = None;
        for line in vendor_manifest.lines() {
            let t = line.trim();
            if t == "[package]" {
                in_package = true;
                continue;
            }
            if t.starts_with('[') {
                in_package = false;
            }
            if in_package && t.starts_with("version") {
                package_version = t.split('"').nth(1).map(str::to_string);
                break;
            }
        }
        let version = package_version.expect("vendored [package] version present");
        let major: u32 = version
            .split('.')
            .next()
            .and_then(|m| m.parse().ok())
            .unwrap_or(0);
        assert!(
            major >= 11,
            "vendored soulseek-rs-lib {version} lacks the GetPeerAddress fix — upgrade to >=11 (v14 verified working)"
        );

        // 3. The vendored client must carry the peer cap. In v14.1.0+
        // the constant lives in client/mod.rs (the registry is capped by
        // the client's max_peers field). Deleting the vendor directory or
        // reverting the cap would otherwise build and pass while
        // reintroducing the thread explosion.
        let client_src = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/vendor/soulseek-rs-lib/src/client/mod.rs"
        ))
        .expect("vendored client/mod.rs readable");
        assert!(
            client_src.contains("DEFAULT_MAX_PEERS"),
            "vendored client must contain the peer cap constant"
        );
    }

    #[test]
    fn maps_search_result_and_files() {
        let mut attribs = HashMap::new();
        attribs.insert(0, 320); // bitrate
        attribs.insert(4, 44100); // sample rate
        let ss = SsSearchResult {
            token: 1,
            username: "alice".to_string(),
            speed: 250_000,
            slots: 3,
            files: vec![SsFile {
                username: "alice".to_string(),
                name: "song.flac".to_string(),
                size: 42_000_000,
                attribs,
            }],
        };

        let mapped = ss_search_result_to_domain(ss);

        assert_eq!(mapped.username, "alice");
        assert_eq!(mapped.speed, 250_000);
        assert_eq!(mapped.slots, 3);
        assert_eq!(mapped.files.len(), 1);
        assert_eq!(mapped.files[0].name, "song.flac");
        assert_eq!(mapped.files[0].size, 42_000_000);
        assert_eq!(mapped.files[0].attribs.get(&0), Some(&320));
        assert_eq!(mapped.files[0].attribs.get(&4), Some(&44100));
    }

    #[test]
    fn maps_download_statuses() {
        assert!(matches!(
            ss_download_status_to_domain(SsDownloadStatus::Queued),
            DownloadStatus::Queued { queue_position: 0 }
        ));
        assert!(matches!(
            ss_download_status_to_domain(SsDownloadStatus::InProgress {
                bytes_downloaded: 100,
                total_bytes: 1000,
                speed_bytes_per_sec: 123.6,
            }),
            DownloadStatus::InProgress {
                speed_bytes_per_sec: 124,
                bytes_downloaded: 100,
                total_bytes: 1000,
            }
        ));
        assert!(matches!(
            ss_download_status_to_domain(SsDownloadStatus::Paused {
                bytes_downloaded: 100,
                total_bytes: 1000,
            }),
            DownloadStatus::InProgress {
                speed_bytes_per_sec: 0,
                bytes_downloaded: 100,
                total_bytes: 1000,
            }
        ));
        assert!(matches!(
            ss_download_status_to_domain(SsDownloadStatus::Completed),
            DownloadStatus::Completed
        ));
        assert!(matches!(
            ss_download_status_to_domain(SsDownloadStatus::Failed(Some("nope".to_string()))),
            DownloadStatus::Failed { reason } if reason == "nope"
        ));
        assert!(matches!(
            ss_download_status_to_domain(SsDownloadStatus::Failed(None)),
            DownloadStatus::Failed { reason } if !reason.is_empty()
        ));
        assert!(matches!(
            ss_download_status_to_domain(SsDownloadStatus::TimedOut),
            DownloadStatus::Failed { reason } if reason.contains("timed out")
        ));
    }

    #[test]
    fn maps_user_statuses() {
        assert!(matches!(
            ss_user_status_to_domain(SsUserStatus::Online),
            UserStatus::Online
        ));
        assert!(matches!(
            ss_user_status_to_domain(SsUserStatus::Away),
            UserStatus::Away
        ));
        assert!(matches!(
            ss_user_status_to_domain(SsUserStatus::Offline),
            UserStatus::Offline
        ));
    }

    #[test]
    fn parses_server_addresses() {
        let addr = parse_server_address("server.slsknet.org:2242").unwrap();
        assert_eq!(addr.get_host(), "server.slsknet.org");
        assert_eq!(addr.get_port(), 2242);
        assert!(parse_server_address("no-port").is_err());
        assert!(parse_server_address("host:notaport").is_err());
        assert!(parse_server_address(":2242").is_err());
        assert!(parse_server_address("host:99999").is_err());
    }
}

#[cfg(test)]
mod mock_client_tests {
    use super::*;

    #[tokio::test]
    async fn test_mock_search_records_queries_and_per_query_results() {
        let client = MockClient::new();
        client.search_results_by_query.lock().unwrap().insert(
            "history".into(),
            vec![MockClient::mock_search_result(
                "peer1",
                500,
                1,
                vec![("01 - track.flac", 10_000_000, 900)],
            )],
        );

        // Per-query override applies.
        let by_query = client.search("history", 15).await.unwrap();
        assert_eq!(by_query.len(), 1);
        assert_eq!(by_query[0].username, "peer1");

        // No override -> falls back to the static search_results set.
        *client.search_results.lock().unwrap() = vec![MockClient::mock_search_result(
            "static_peer",
            500,
            1,
            vec![("02 - track.flac", 10_000_000, 900)],
        )];
        let by_fallback = client.search("no override", 15).await.unwrap();
        assert_eq!(by_fallback[0].username, "static_peer");

        // Every query is recorded, in order.
        let queries = client.search_queries.lock().unwrap().clone();
        assert_eq!(
            queries,
            vec!["history".to_string(), "no override".to_string()]
        );
    }
}
