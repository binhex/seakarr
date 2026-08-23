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
    /// Credentials captured by the last successful `login()`, used by
    /// `reconnect_if_needed()` after a session loss.
    reconnect_settings: tokio::sync::Mutex<Option<ReconnectSettings>>,
    /// Single-flight lock around reconnects: only one caller re-runs the
    /// login sequence for a lost session; concurrent callers re-check the
    /// session state once the lock is free instead of each logging in.
    reconnect_lock: tokio::sync::Mutex<()>,
    /// When the last reconnect attempt failed (`Some(timestamp)` = transient,
    /// retried after [`RECONNECT_COOLDOWN`]; `None` timestamp = permanent,
    /// e.g. credential rejection, never auto-retried) and the reason. Inside
    /// the cooldown (or permanently) operations fail fast instead of
    /// re-running the login sequence.
    reconnect_failed: tokio::sync::Mutex<Option<(Option<Instant>, String)>>,
    /// The configured peer-connection cap, re-applied to the fresh client
    /// after a reconnect (the vendored default would otherwise silently
    /// replace it). Kept as `usize` to exactly match the vendored
    /// `Client::set_max_peers` signature (no truncation on 64-bit).
    max_peers: std::sync::atomic::AtomicUsize,
    /// Total connect+login attempts per `login()` call.
    login_retries: u32,
    /// Base delay between login attempts; doubled after every failure.
    login_retry_delay_secs: u64,
}

/// How long a failed reconnect is "remembered". Inside this window,
/// operations fail fast with the recorded reason instead of re-running the
/// login sequence; once it elapses, a reconnect is attempted again.
const RECONNECT_COOLDOWN: StdDuration = StdDuration::from_secs(60);

/// Login credentials captured at login time, used to reconnect transparently
/// after the server session is lost.
#[derive(Clone)]
struct ReconnectSettings {
    username: String,
    password: String,
    server: String,
    listen_port: u16,
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
            reconnect_settings: tokio::sync::Mutex::new(None),
            reconnect_lock: tokio::sync::Mutex::new(()),
            reconnect_failed: tokio::sync::Mutex::new(None),
            max_peers: std::sync::atomic::AtomicUsize::new(0),
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

    /// Reconnect transparently when the server session has been lost.
    ///
    /// Idempotent: returns immediately while the session is alive. After a
    /// `Disconnected` loss it re-logins with the credentials captured at
    /// login time and swaps in the fresh client. Reconnects are
    /// single-flight (concurrent callers wait for the in-flight attempt and
    /// re-check), and a failed reconnect is negative-cached for
    /// [`RECONNECT_COOLDOWN`]: within that window operations fail fast with
    /// the recorded reason instead of re-running the login sequence, and once
    /// it elapses a reconnect is attempted again (a transient outage does not
    /// permanently brick the session).
    ///
    /// A `Displaced` loss (another login took this username) is deliberately
    /// NOT reconnected: re-logging in would evict the other session and can
    /// start a reconnect ping-pong between clients. It surfaces as
    /// [`SeakarrError::Displaced`] instead, with the loss reason.
    async fn reconnect_if_needed(&self) -> Result<()> {
        let loss = self.connected_client().await?.session_loss();
        let Some(loss) = loss else {
            return Ok(());
        };

        // Another session owns this account now. Reconnecting would evict it
        // and could ping-pong the account; surface the loss reason as its own
        // error type so callers can distinguish "displaced" from a network
        // drop. Checked before the negative cache: a stale reconnect failure
        // must not mask the takeover reason.
        if loss == ::soulseek_rs::types::SessionLoss::Displaced {
            return Err(SeakarrError::Displaced {
                reason: loss.to_string(),
            });
        }

        // Negative cache: if the last reconnect failed recently (within
        // RECONNECT_COOLDOWN), fail fast with the recorded reason rather than
        // re-running the full multi-attempt login sequence per call. Once the
        // cooldown elapses, the cache is treated as expired and a reconnect is
        // attempted again below (a transient outage must not permanently
        // brick the session).
        //
        // NOTE: the guard is dropped at the end of this statement (the value
        // is cloned); never re-acquire this mutex inside an `if let` whose
        // scrutinee opened it — the guard outlives the block body and
        // self-deadlocks.
        let cached = self.reconnect_failed.lock().await.clone();
        if let Some((failed_at, reason)) = cached {
            let fresh = failed_at.is_none_or(|at| at.elapsed() < RECONNECT_COOLDOWN);
            if fresh {
                return Err(SeakarrError::Disconnected { reason });
            }
        }

        // Single-flight: only one caller reconnects; the rest re-check the
        // session state once the in-flight attempt finishes.
        let _guard = self.reconnect_lock.lock().await;
        if self.connected_client().await?.session_loss().is_none() {
            return Ok(());
        }
        // Re-check the negative cache under the lock: callers that queued
        // before the in-flight attempt failed must not each re-run the full
        // login sequence (thundering herd). Expired entries are cleared so the
        // retry below actually proceeds; permanent entries (None timestamp)
        // always fail fast.
        let cached = self.reconnect_failed.lock().await.clone();
        if let Some((failed_at, reason)) = cached {
            let fresh = failed_at.is_none_or(|at| at.elapsed() < RECONNECT_COOLDOWN);
            if fresh {
                return Err(SeakarrError::Disconnected { reason });
            }
            // Dropped the read guard above; a fresh short-lived guard here
            // clears the expired entry without re-entering a held lock.
            self.reconnect_failed.lock().await.take();
        }

        let settings = self
            .reconnect_settings
            .lock()
            .await
            .clone()
            .ok_or_else(|| SeakarrError::Disconnected {
                reason: "no stored login settings to reconnect with".into(),
            })?;
        // Stop the old client's listener BEFORE the new connect() so its
        // socket (and the configured port) is released: otherwise
        // Listen::bind races the old listener, falls back to an ephemeral
        // port, and the reconnected client silently advertises the wrong
        // port. We keep the old (dead) client in `inner` so a failed
        // reconnect still leaves a client whose session_loss() is observable,
        // keeping the negative-cache / cooldown retry path reachable.
        tracing::warn!("Soulseek session lost ({loss:?}); reconnecting...");
        if let Ok(old) = self.connected_client().await {
            // The join inside stop_listener can block up to ~10s if the
            // accept loop is parked on a full handshake semaphore; run it off
            // the async worker so it doesn't stall the runtime.
            let _ = tokio::task::spawn_blocking(move || old.stop_listener()).await;
        }
        match self
            .login(
                &settings.username,
                &settings.password,
                &settings.server,
                settings.listen_port,
            )
            .await
        {
            Ok(()) => {
                // Reconnected: clear the negative cache so future operations
                // use the fresh session, and re-apply the configured peer cap
                // (login() creates a fresh client with the vendored default).
                *self.reconnect_failed.lock().await = None;
                let cap = self.max_peers.load(std::sync::atomic::Ordering::Relaxed);
                if cap > 0 {
                    if let Ok(client) = self.connected_client().await {
                        client.set_max_peers(cap);
                    }
                }
                Ok(())
            }
            Err(e) => {
                let reason = format!("reconnect failed: {e}");
                // Credential rejection is permanent (credentials won't change
                // while the process runs): store a None timestamp so it is
                // never auto-retried. Network/other failures are transient.
                let is_auth = matches!(&e, SeakarrError::Auth { .. });
                let stamp = if is_auth { None } else { Some(Instant::now()) };
                *self.reconnect_failed.lock().await = Some((stamp, reason.clone()));
                // Preserve the credential-rejection signal instead of
                // flattening everything into Disconnected.
                match e {
                    SeakarrError::Auth { .. } => Err(e),
                    _ => Err(SeakarrError::Disconnected { reason }),
                }
            }
        }
    }

    /// Set the maximum number of simultaneous peer connections.
    /// Delegates to the vendored library's `Client::set_max_peers` which
    /// enforces a floor of 1. Stored so a reconnected client re-applies the
    /// same cap (the vendored default would otherwise take over).
    pub async fn set_max_peers(&self, max_peers: usize) -> Result<()> {
        self.max_peers
            .store(max_peers.max(1), std::sync::atomic::Ordering::Relaxed);
        let client = self.connected_client().await?;
        client.set_max_peers(max_peers);
        Ok(())
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
                    tracing::debug!("Bridge status for {safe_filename}: {status:?}");
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
        // Whether any failure was a credential rejection (the server's login
        // verdict said no) vs a network/timeout failure. A credential
        // rejection is permanent (credentials won't change while the process
        // runs), while a network failure is transient. Sticky (OR'd), so a
        // mixed auth-then-network sequence still classifies as permanent.
        let mut last_was_auth = false;
        // The reason of the attempt that *caused* the auth classification, so
        // the returned Auth error doesn't carry a later network attempt's
        // misleading reason (e.g. "Operation timed out") in mixed sequences.
        let mut auth_reason: Option<String> = None;
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
            .map_err(|e| SeakarrError::Disconnected {
                reason: format!("login task failed: {e}"),
            })?;

            match result {
                Ok((client, true)) => {
                    // Assigning a fresh client drops the previous Arc while the
                    // `inner` guard is held on the async worker. Today that
                    // drop's `Client::drop` -> `stop_listener()` is a no-op
                    // (startup login has no prior client; the reconnect path
                    // already stopped the old listener), so it cannot stall.
                    // Keep it that way: any future direct re-login path must
                    // release the old Arc off-lock before re-assigning.
                    *self.inner.lock().await = Some(Arc::new(client));
                    *self.reconnect_settings.lock().await = Some(ReconnectSettings {
                        username: username.to_string(),
                        password: password.to_string(),
                        server: server.to_string(),
                        listen_port,
                    });
                    // A successful (re)login means the connection is healthy
                    // again: clear any stale negative cache so future session
                    // losses can reconnect.
                    *self.reconnect_failed.lock().await = None;
                    if enable_listen {
                        tracing::info!("[listener] enabled on port {listen_port}");
                    } else {
                        tracing::info!("[listener] disabled (listen_port=0)");
                    }
                    return Ok(());
                }
                Ok((_, false)) => {
                    last_reason = "the server rejected the login".to_string();
                    last_was_auth = true;
                    auth_reason = Some(last_reason.clone());
                }
                Err(e) => {
                    last_reason = e.to_string();
                    // The vendored actor reports credential rejection as
                    // `Err(SoulseekRs::AuthenticationFailed)` (never `Ok(false)`),
                    // while network/timeout failures surface as other `Err`
                    // variants (Timeout, NotConnected, NetworkError). OR (not
                    // overwrite) so a credential rejection on ANY attempt is
                    // remembered — a mixed auth-then-network sequence still
                    // classifies as permanent, not transient.
                    if matches!(&e, ::soulseek_rs::error::SoulseekRs::AuthenticationFailed) {
                        last_was_auth = true;
                        auth_reason = Some(last_reason.clone());
                    }
                    // Limitation: a ban/version rejection that the server
                    // signals by dropping the connection (rather than an
                    // explicit `AuthenticationFailed` verdict) surfaces as
                    // Timeout/ConnectionClosed and is therefore classified
                    // transient, so it retries every cooldown period. The
                    // vendored actor does not distinguish these, so this is a
                    // documented (not silent) behavior.
                }
            }

            if attempt + 1 < self.login_retries {
                let delay = self
                    .login_retry_delay_secs
                    .saturating_mul(1u64 << attempt.min(20));
                tokio::time::sleep(StdDuration::from_secs(delay)).await;
            }
        }

        // Credential rejection and network failure are fundamentally
        // different: the former will not change without new credentials, the
        // latter is transient. Return distinct variants so the reconnect
        // path can cache them differently.
        if last_was_auth {
            Err(SeakarrError::Auth {
                attempts: self.login_retries,
                reason: auth_reason.unwrap_or(last_reason),
            })
        } else {
            Err(SeakarrError::Disconnected {
                reason: last_reason,
            })
        }
    }

    async fn search(&self, query: &str, timeout_secs: u64) -> Result<Vec<SearchResult>> {
        self.reconnect_if_needed().await?;
        let client = self.connected_client().await?;
        let query_owned = query.to_string();
        let query_for_task = query_owned.clone();
        let results = tokio::task::spawn_blocking(move || {
            // Blocks the calling thread for the whole timeout window.
            client.search(&query_for_task, StdDuration::from_secs(timeout_secs))
        })
        .await
        .map_err(|e| SeakarrError::Client(format!("search task panicked: {e}")))?
        .map_err(|e| match e {
            // A session loss between the reconnect check and the send (e.g.
            // displaced concurrently) surfaces as NotConnected; map it to the
            // caller-facing Disconnected so retry/abort logic sees one shape.
            ::soulseek_rs::error::SoulseekRs::NotConnected => SeakarrError::Disconnected {
                reason: format!("server connection lost while searching '{query_owned}'"),
            },
            e => SeakarrError::Client(format!("search '{query_owned}' failed: {e}")),
        })?;

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
        self.reconnect_if_needed().await?;
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
        .map_err(|e| match e {
            // Symmetric with search(): a session loss between the reconnect
            // check and the queue surfaces as NotConnected; map it to the
            // caller-facing Disconnected so retry/abort logic sees one shape.
            ::soulseek_rs::error::SoulseekRs::NotConnected => SeakarrError::Disconnected {
                reason: format!(
                    "server connection lost while queueing download of '{filename}' from '{username_owned}'"
                ),
            },
            e => {
                SeakarrError::Download(format!(
                    "failed to queue download of '{filename}' from '{username_owned}': {e}"
                ))
            }
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
            tracing::debug!("Bridge started for {bridge_filename} from {bridge_username}");
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
            tracing::debug!(
                "Bridge finished for {bridge_filename} from {bridge_username}, clearing download state"
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
            // Do NOT remove_download here — the F-connection transfer is
            // still running and wait_while_paused holds a store reference.
            // Removing the download races with the transfer thread and
            // causes TokenNotFound. The bridge's own remove_download call
            // after drain_transfer handles cleanup.
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
        self.reconnect_if_needed().await?;
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
            .map_err(|e| match e {
                // Symmetric with search(): a session loss between the
                // reconnect check and the request surfaces as NotConnected;
                // map it to the caller-facing Disconnected.
                ::soulseek_rs::error::SoulseekRs::NotConnected => SeakarrError::Disconnected {
                    reason: format!(
                        "server connection lost while requesting user info for '{username_owned}'"
                    ),
                },
                e => SeakarrError::Client(format!(
                    "user info request for '{username_owned}' failed: {e}"
                )),
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
    use ::soulseek_rs::types::SessionLoss;
    use std::collections::HashMap;

    // Compile-time proof that the trait's `Send + Sync` supertrait holds.
    #[test]
    fn real_client_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<RealClient>();
    }

    // The Disconnected error (surfaced when a reconnect after session loss
    // fails) carries a clear, caller-visible reason.
    #[test]
    fn disconnected_error_displays_reason() {
        let err = SeakarrError::Disconnected {
            reason: "the connection to the server dropped".into(),
        };
        assert_eq!(
            err.to_string(),
            "server connection lost: the connection to the server dropped"
        );
    }

    // Characterisation guard: reconnection must not mask the normal
    // not-logged-in error path for a fresh client.
    #[tokio::test]
    async fn search_without_login_still_errors_not_connected() {
        let client = RealClient::new();
        let err = client.search("query", 1).await.unwrap_err();
        assert!(err
            .to_string()
            .contains("not connected: call login() first"));
    }

    // Build a RealClient whose inner client has the given session loss
    // recorded, with no stored reconnect settings. Uses the vendored crate's
    // test-only `record_session_loss` hook so reconnect paths can be exercised
    // without a live Soulseek connection.
    async fn real_client_with_loss(loss: SessionLoss) -> RealClient {
        let client = Client::with_settings(ClientSettings::new("test-user", "test-pass"));
        client.record_session_loss(loss);
        let rc = RealClient::new();
        *rc.inner.lock().await = Some(Arc::new(client));
        rc
    }

    // A live session must not trigger any reconnect work.
    #[tokio::test]
    async fn reconnect_if_needed_is_noop_when_session_alive() {
        let client = Client::with_settings(ClientSettings::new("test-user", "test-pass"));
        let rc = RealClient::new();
        *rc.inner.lock().await = Some(Arc::new(client));

        assert!(rc.reconnect_if_needed().await.is_ok());
        // No reconnect was attempted, so no failure is cached.
        assert!(rc.reconnect_failed.lock().await.is_none());
    }

    // A displaced session (another login owns the account) must fail fast with
    // the Displaced variant, never reconnecting (which would ping-pong the
    // account between clients).
    #[tokio::test]
    async fn displaced_session_returns_displaced_error() {
        let rc = real_client_with_loss(SessionLoss::Displaced).await;

        let err = rc.reconnect_if_needed().await.unwrap_err();
        assert!(matches!(err, SeakarrError::Displaced { .. }));
        // No reconnect was attempted: the negative cache stays clear.
        assert!(rc.reconnect_failed.lock().await.is_none());
    }

    // A previously failed reconnect must be negative-cached: the next
    // operation fails fast with the recorded reason instead of re-running the
    // full multi-attempt login sequence — as long as the cooldown has not
    // elapsed.
    #[tokio::test]
    async fn failed_reconnect_is_negative_cached() {
        let rc = real_client_with_loss(SessionLoss::Disconnected).await;
        *rc.reconnect_failed.lock().await = Some((
            Some(Instant::now()),
            "reconnect failed: server unreachable".into(),
        ));

        let err = rc.reconnect_if_needed().await.unwrap_err();
        assert!(matches!(err, SeakarrError::Disconnected { .. }));
        assert_eq!(
            err.to_string(),
            "server connection lost: reconnect failed: server unreachable"
        );
    }

    // A failed reconnect inside the cooldown window keeps failing fast with
    // the recorded reason — the storm-prevention behaviour from earlier
    // rounds.
    #[tokio::test]
    async fn fresh_reconnect_failure_fails_fast_during_cooldown() {
        let rc = real_client_with_loss(SessionLoss::Disconnected).await;
        *rc.reconnect_failed.lock().await =
            Some((Some(Instant::now()), "reconnect failed: server down".into()));

        let err = rc.reconnect_if_needed().await.unwrap_err();
        assert!(matches!(err, SeakarrError::Disconnected { .. }));
        assert_eq!(
            err.to_string(),
            "server connection lost: reconnect failed: server down"
        );
        // The cache must survive the fast-fail: a later call inside the same
        // window still fails fast.
        assert!(rc.reconnect_failed.lock().await.is_some());
    }

    // After the cooldown elapses, a reconnect must be attempted again (the
    // daemon must recover from a transient outage without a restart). The
    // stale cached reason must not short-circuit the attempt.
    #[tokio::test]
    async fn expired_reconnect_failure_allows_retry_after_cooldown() {
        let rc = real_client_with_loss(SessionLoss::Disconnected).await;
        // A failure recorded well beyond the (60s) cooldown window.
        *rc.reconnect_failed.lock().await = Some((
            Some(Instant::now() - StdDuration::from_secs(2 * 60)),
            "reconnect failed: long ago".into(),
        ));

        // The cache is expired, so the code must proceed into the reconnect
        // branch. With no stored settings it fails with the "no stored login
        // settings" error — proving the stale reason was NOT returned and the
        // retry path was entered.
        let err = rc.reconnect_if_needed().await.unwrap_err();
        assert!(matches!(err, SeakarrError::Disconnected { .. }));
        assert!(err
            .to_string()
            .contains("no stored login settings to reconnect with"));
    }

    // A credential-rejection (Auth) failure must never auto-retry: the
    // credentials cannot change while the process runs, so re-running the full
    // login sequence every cooldown is pure waste (and, per finding, leaks
    // threads). A Permanent failure fails fast forever, until an explicit
    // successful login clears it.
    #[tokio::test]
    async fn auth_failure_is_permanent_and_never_retried() {
        let rc = real_client_with_loss(SessionLoss::Disconnected).await;
        // None timestamp => permanent (Auth) failure: never auto-retried.
        *rc.reconnect_failed.lock().await = Some((
            None,
            "reconnect failed: soulseek authentication failed".into(),
        ));

        let err = rc.reconnect_if_needed().await.unwrap_err();
        assert!(matches!(err, SeakarrError::Disconnected { .. }));
        assert!(err.to_string().contains("soulseek authentication failed"));
        // The permanent entry survives: no retry, no clear.
        assert!(rc.reconnect_failed.lock().await.is_some());
    }

    // set_max_peers must preserve the full usize cap across reconnects: a
    // value above u32::MAX must not truncate in the stored atomic.
    #[tokio::test]
    async fn set_max_peers_preserves_large_cap_without_truncation() {
        let client = Client::with_settings(ClientSettings::new("test-user", "test-pass"));
        let rc = RealClient::new();
        *rc.inner.lock().await = Some(Arc::new(client));

        rc.set_max_peers(usize::MAX).await.unwrap();

        assert_eq!(
            rc.max_peers.load(std::sync::atomic::Ordering::Relaxed),
            usize::MAX
        );
    }

    // A disconnected session with no stored credentials reaches the reconnect
    // branch and fails with the "no stored login settings" error — proving the
    // code enters the reconnect path rather than silently succeeding.
    #[tokio::test]
    async fn disconnected_without_settings_fails_disconnected() {
        let rc = real_client_with_loss(SessionLoss::Disconnected).await;

        let err = rc.reconnect_if_needed().await.unwrap_err();
        assert!(matches!(err, SeakarrError::Disconnected { .. }));
        assert!(err
            .to_string()
            .contains("no stored login settings to reconnect with"));
    }

    // download() must run the reconnect guard before touching the network:
    // on a displaced session it fails with Displaced, not a download error.
    #[tokio::test]
    async fn download_on_displaced_session_returns_displaced() {
        let rc = real_client_with_loss(SessionLoss::Displaced).await;
        let file = FileInfo {
            name: "01 - track.flac".into(),
            size: 10_000,
            attribs: HashMap::new(),
        };

        let err = rc
            .download(&file, "peer", std::path::Path::new("/tmp"))
            .await
            .err()
            .expect("displaced session must fail download");
        assert!(matches!(err, SeakarrError::Displaced { .. }));
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
