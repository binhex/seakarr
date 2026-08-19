use clap::Parser;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

use seakarr::client::{RealClient, SoulseekClient};
use seakarr::config::{CliOverrides, Config};
use seakarr::db::Database;
use seakarr::error::{Result, SeakarrError};
use seakarr::runner;

#[derive(Parser, Debug)]
#[command(
    name = "seakarr",
    version,
    about = "Soulseek music downloader and library upgrader"
)]
struct Cli {
    /// Directory containing seakarr.yml
    #[arg(long, default_value = "configs")]
    config_path: PathBuf,

    /// Override log directory
    #[arg(long)]
    log_path: Option<PathBuf>,

    /// Override log level (DEBUG|INFO|WARN|ERROR)
    #[arg(long)]
    log_level: Option<String>,

    /// Override database directory
    #[arg(long)]
    db_path: Option<PathBuf>,

    /// Override PID file directory
    #[arg(long)]
    pid_path: Option<PathBuf>,

    /// Comma-separated library paths, overrides config
    #[arg(long, value_delimiter = ',')]
    library_path: Option<Vec<String>>,

    /// Soulseek username (overrides config)
    #[arg(long)]
    soulseek_user: Option<String>,

    /// Soulseek password (overrides config)
    #[arg(long)]
    soulseek_password: Option<String>,

    /// Override incoming peer port (0 disables listener)
    #[arg(long)]
    listen_port: Option<u16>,

    /// Override search mode (auto|manual|batch)
    #[arg(long)]
    mode: Option<String>,

    /// Batch file path (newline-separated artist/album lines)
    #[arg(long)]
    batch_file: Option<PathBuf>,

    /// Artist for manual mode
    #[arg(long)]
    artist: Option<String>,

    /// Album for manual mode (optional)
    #[arg(long)]
    album: Option<String>,

    /// Validate configuration and exit
    #[arg(long)]
    test: bool,

    /// Run continuously as a daemon
    #[arg(long)]
    daemon: bool,
}

#[tokio::main]
async fn main() {
    std::process::exit(exit_code_after_run(run().await));
}

/// Map a run result to a process exit code, printing the error on failure.
///
/// `main` calls `std::process::exit` with this value instead of returning
/// normally: after the run completes the tokio runtime drop would block
/// indefinitely on any still-running spawned blocking task (e.g. a download
/// status bridge), leaving the process hung and Ctrl+C unable to terminate
/// it (the SIGINT listener is aborted after the run summary, and tokio's
/// global signal handler swallows further presses). An explicit exit
/// bypasses the runtime drop entirely.
fn exit_code_after_run(result: Result<()>) -> i32 {
    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("seakarr: {e}");
            1
        }
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse();

    // Load and merge config
    let mut config = Config::load(&cli.config_path)?;

    let cli_overrides = CliOverrides {
        log_level: cli.log_level.clone(),
        log_path: cli.log_path.as_ref().map(|p| p.to_string_lossy().into()),
        db_path: cli.db_path.as_ref().map(|p| p.to_string_lossy().into()),
        pid_path: cli.pid_path.as_ref().map(|p| p.to_string_lossy().into()),
        library_path: cli.library_path.clone(),
        soulseek_user: cli.soulseek_user.clone(),
        soulseek_password: cli.soulseek_password.clone(),
        listen_port: cli.listen_port,
        mode: cli.mode.clone(),
        batch_file: cli.batch_file.as_ref().map(|p| p.to_string_lossy().into()),
        artist: cli.artist.clone(),
        album: cli.album.clone(),
        daemon: cli.daemon,
        test: cli.test,
    };
    config.merge_cli(cli_overrides);

    // Setup logging (stdout + rolling file)
    let log_dir = PathBuf::from(&config.logging.path);
    std::fs::create_dir_all(&log_dir)?;
    let file_appender = tracing_appender::rolling::never(&log_dir, &config.logging.file);
    // Suppress noisy library logs: lofty emits a WARN per MP3 file
    // about bitrate estimation, which drowns out the scanner output.
    let filter_str = format!("{},lofty=error,soulseek_rs=error", config.logging.level);
    let env_filter = EnvFilter::try_new(&filter_str).unwrap_or_else(|_| EnvFilter::new("INFO"));

    tracing_subscriber::registry()
        .with(env_filter)
        .with(fmt::Layer::new().with_writer(std::io::stdout))
        .with(
            fmt::Layer::new()
                .with_writer(file_appender)
                .with_ansi(false),
        )
        .init();

    // --test: structural validation, then exit
    if cli.test {
        validate_for_test(&config)?;
        return Ok(());
    }

    // Validate required fields
    config.validate()?;

    // Open database (before PID lock — DB errors should not leave a stale pid)
    let db_dir = PathBuf::from(&config.database.path);
    let db = Database::open(&db_dir, &config.database)?;

    // Connect to Soulseek
    // Suppress the crate's internal logger (it uses LOG_LEVEL / RUST_LOG
    // env vars, not the tracing ecosystem).  Set to INFO for debugging.
    std::env::set_var("LOG_LEVEL", &config.logging.level);
    tracing::info!(
        "Connecting to Soulseek server {}...",
        config.soulseek.server
    );
    let client = RealClient::new();
    client
        .login(
            &config.soulseek.username,
            &config.soulseek.password,
            &config.soulseek.server,
            config.soulseek.listen_port,
        )
        .await?;
    tracing::info!("Connected to Soulseek.");

    client.set_max_peers(config.soulseek.max_peers).await?;

    // Acquire PID lock only after DB + login succeed, so failures before
    // this point don't leave an orphaned PID file.
    let pid_dir = PathBuf::from(&config.pid.path);
    std::fs::create_dir_all(&pid_dir)?;
    let pid_file = pid_dir.join(&config.pid.file);
    acquire_pid_lock(&pid_file)?;

    // Validate search mode before dispatch
    let mode = config.search.default_mode.as_str();
    if !matches!(mode, "auto" | "manual" | "batch") {
        release_pid_lock(&pid_file)?;
        return Err(SeakarrError::Config(format!(
            "invalid search mode '{mode}' — must be auto, manual, or batch"
        )));
    }

    if config.daemon.enabled {
        let interval_mins = config.daemon.rescan_interval_mins.max(1);
        if config.daemon.rescan_interval_mins == 0 {
            tracing::warn!("daemon.rescan_interval_mins is 0 — clamping to 1 to avoid busy-loop");
        }
        // Use the clamped interval for the daemon loop
        let interval = tokio::time::Duration::from_secs(interval_mins * 60);
        run_daemon(&client, &config, &db, &pid_file, interval).await
    } else {
        let result = match mode {
            "manual" => {
                let artist = cli
                    .artist
                    .as_deref()
                    .filter(|a| !a.is_empty())
                    .or_else(|| non_empty(&config.search.manual.artist))
                    .ok_or_else(|| {
                        SeakarrError::Config("--artist required for manual mode".into())
                    })?;
                let album: Option<&str> = cli
                    .album
                    .as_deref()
                    .filter(|a| !a.is_empty())
                    .or_else(|| non_empty(&config.search.manual.album));
                runner::run_manual_mode(&client, artist, album, &config, &db).await
            }
            "batch" => {
                let batch_path = cli
                    .batch_file
                    .as_deref()
                    .map(|p| p.to_string_lossy().into_owned())
                    .or_else(|| non_empty(&config.search.batch.file_path).map(str::to_owned))
                    .ok_or_else(|| {
                        SeakarrError::Config("--batch-file required for batch mode".into())
                    })?;
                run_batch_mode(&client, &batch_path, &config, &db).await
            }
            _ => runner::run_auto_mode(&client, &config, &db).await,
        };

        release_pid_lock(&pid_file)?;
        result
    }
}

/// `--test` mode: structural validation that works even on a freshly created
/// default config (credentials are not yet populated at that point).
fn validate_for_test(config: &Config) -> Result<()> {
    let valid_levels = ["DEBUG", "INFO", "WARN", "ERROR"];
    if !valid_levels.contains(&config.logging.level.as_str()) {
        return Err(SeakarrError::Config(format!(
            "logging.level must be one of {valid_levels:?}, got {:?}",
            config.logging.level
        )));
    }
    // Same numeric bounds as Config::validate() so `--test` does not report
    // "valid" for a config that would fail at real startup.
    config.validate_download_bounds()?;
    if config.soulseek.max_peers == 0 {
        return Err(SeakarrError::Config(
            "soulseek.max_peers must be at least 1".into(),
        ));
    }
    for path in &config.library.paths {
        if !Path::new(path).exists() {
            tracing::warn!("library path does not exist: {path}");
        }
    }
    tracing::info!("Configuration is valid.");
    Ok(())
}

/// Returns `Some(&str)` when the value is non-empty after trimming, else `None`.
fn non_empty(value: &str) -> Option<&str> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

/// Write PID to file. Returns error if another instance is already running.
fn acquire_pid_lock(pid_file: &Path) -> Result<()> {
    if pid_file.exists() {
        let contents = std::fs::read_to_string(pid_file)?;
        // A corrupt or empty PID file is treated as stale.
        let pid: i32 = match contents.trim().parse() {
            Ok(p) if p > 0 => p,
            _ => {
                tracing::warn!("PID file {pid_file:?} is corrupt — removing and continuing");
                std::fs::remove_file(pid_file)?;
                0 // fall through to creation
            }
        };
        if pid > 0 {
            #[cfg(unix)]
            {
                use std::process::Command;
                let output = Command::new("kill").arg("-0").arg(pid.to_string()).output();
                // Exit status 0: process exists and we can signal it → alive.
                // Exit status 1: no such process → stale. Anything else
                // (e.g. signal=EPERM) means the process exists but
                // belongs to another user — treat as alive.
                if let Ok(output) = &output {
                    // kill -0 exit codes: 0 = process exists + we can signal → alive.
                    // 1 = no such process (ESRCH) → stale. Anything else (e.g. EPERM,
                    // another user's process) → treat as alive.
                    if output.status.code() != Some(1) {
                        return Err(SeakarrError::PidLock(format!(
                            "Another instance is running with PID {pid}. If this is stale, delete {pid_file:?}"
                        )));
                    }
                }
                // On non-Unix we skip the liveness check; stale PID files
                // must be removed manually.
            }
        }
    }
    let my_pid = std::process::id();
    std::fs::write(pid_file, my_pid.to_string())?;
    Ok(())
}

fn release_pid_lock(pid_file: &Path) -> Result<()> {
    if pid_file.exists() {
        std::fs::remove_file(pid_file)?;
    }
    Ok(())
}

/// Daemon loop: run a scan cycle (dispatching on the configured search
/// mode), then sleep until the next cycle or shut down gracefully on
/// SIGINT/SIGTERM.
async fn run_daemon(
    client: &dyn SoulseekClient,
    config: &Config,
    db: &Database,
    pid_file: &Path,
    interval: tokio::time::Duration,
) -> Result<()> {
    let mut sigterm = signal_terminate();

    loop {
        tracing::info!("Daemon: starting scan cycle...");
        if let Err(e) = run_daemon_cycle(client, config, db).await {
            tracing::error!("Scan cycle failed: {e}");
        }

        // Wait for the next cycle time, Ctrl+C, or SIGTERM.
        // The SIGTERM branch only exists when the signal listener was
        // successfully registered (Unix); on non-Unix we fall through
        // to a plain sleep.
        if let Some(ref mut s) = sigterm {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {
                    tracing::info!("Daemon: received SIGINT, shutting down...");
                    release_pid_lock(pid_file)?;
                    return Ok(());
                }
                _ = s.recv() => {
                    tracing::info!("Daemon: received SIGTERM, shutting down...");
                    release_pid_lock(pid_file)?;
                    return Ok(());
                }
                _ = tokio::time::sleep(interval) => {}
            }
        } else {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {
                    tracing::info!("Daemon: received SIGINT, shutting down...");
                    release_pid_lock(pid_file)?;
                    return Ok(());
                }
                _ = tokio::time::sleep(interval) => {}
            }
        }
    }
}

/// Run ONE daemon scan cycle, dispatching on the configured search mode.
///
/// The daemon loop must respect the same mode + criteria as a one-shot run:
/// manual mode searches the configured artist/album, batch mode processes
/// the configured batch file, auto mode scans the library. Previously the
/// daemon hardcoded auto mode, silently ignoring `--mode manual --artist X
/// --album Y` (or `--mode batch --batch-file F`).
async fn run_daemon_cycle(
    client: &dyn SoulseekClient,
    config: &Config,
    db: &Database,
) -> Result<()> {
    let mode = config.search.default_mode.as_str();
    match mode {
        "manual" => {
            let artist = non_empty(&config.search.manual.artist).ok_or_else(|| {
                SeakarrError::Config("--artist required for manual mode (daemon)".into())
            })?;
            let album = non_empty(&config.search.manual.album);
            runner::run_manual_mode(client, artist, album, config, db).await
        }
        "batch" => {
            let batch_path = non_empty(&config.search.batch.file_path).ok_or_else(|| {
                SeakarrError::Config("--batch-file required for batch mode (daemon)".into())
            })?;
            run_batch_mode(client, batch_path, config, db).await
        }
        _ => runner::run_auto_mode(client, config, db).await,
    }
}

/// Returns a SIGTERM listener on Unix, or `None` on other platforms.
#[cfg(unix)]
fn signal_terminate() -> Option<tokio::signal::unix::Signal> {
    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok()
}

#[cfg(not(unix))]
fn signal_terminate() -> Option<tokio::signal::unix::Signal> {
    None
}

/// Batch mode: process a newline-separated list of `artist - album` lines.
async fn run_batch_mode(
    client: &dyn SoulseekClient,
    batch_path: &str,
    config: &Config,
    db: &Database,
) -> Result<()> {
    let contents = std::fs::read_to_string(batch_path)?;
    let lines: Vec<&str> = contents
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();

    tracing::info!("Batch mode: {} lines to process", lines.len());
    let staging_dir = Path::new(&config.storage.staging_dir);
    std::fs::create_dir_all(staging_dir)?;

    let mut report = seakarr::report::RunReport::new();

    let progress = if seakarr::progress::is_interactive() {
        Some(seakarr::progress::ProgressDisplay::new())
    } else {
        None
    };

    // Shared cancellation flag: SIGINT aborts the in-flight album download;
    // its staging dir is cleaned by download_album.
    let cancel = Arc::new(AtomicBool::new(false));
    let _listener = seakarr::runner::spawn_cancel_listener(Arc::clone(&cancel));

    for line in &lines {
        // Check cancellation between batch lines — stop processing
        // remaining albums after Ctrl+C.
        if cancel.load(Ordering::SeqCst) {
            tracing::info!("Batch mode: cancelled");
            break;
        }

        let parts: Vec<&str> = line.splitn(2, " - ").collect();
        let artist = parts[0].trim();
        let album = parts.get(1).map(|a| a.trim()).filter(|a| !a.is_empty());
        let album_display = album.unwrap_or("(all)");

        match seakarr::runner::process_album(
            client,
            artist,
            album,
            config,
            db,
            staging_dir,
            progress.as_ref(),
            Some(&cancel),
            None, // library_track_count (batch mode: no scanner data)
            None, // target_library_path (batch mode: no library upgrade)
        )
        .await
        {
            Ok(outcome) => report.record(artist, album_display, outcome),
            Err(e) => {
                tracing::error!("Batch: failed {artist} — {album_display}: {e}");
                report.record(
                    artist,
                    album_display,
                    seakarr::report::AlbumOutcome::Failed {
                        reason: e.to_string(),
                    },
                );
            }
        }
    }

    if let Some(ref p) = progress {
        p.clear();
    }

    report.print_summary();
    _listener.abort();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use seakarr::client::MockClient;
    use seakarr::db::Database;
    use tempfile::TempDir;

    // Regression: `--daemon --mode manual --artist X --album Y` must honour
    // the manual criteria instead of running an auto-mode library scan.
    // Previously run_daemon hardcoded run_auto_mode every cycle, so the
    // artist/album were silently ignored and arbitrary albums downloaded.
    #[tokio::test]
    async fn daemon_cycle_honours_manual_mode_artist_album() {
        let client = MockClient::new();
        let mut config = Config::default();
        config.soulseek.username = "test".into();
        config.soulseek.password = "test".into();
        config.download.concurrent = 2;
        config.download.min_upload_speed_kbps = 0; // disabled
        config.download.speed_check_wait_secs = 0;
        config.download.max_retries = 1;
        config.download.retry_delay_secs = 0;
        config.notifications.urls = vec![];
        config.filters.min_tracks = 0;
        config.search.default_mode = "manual".into();
        config.search.manual.artist = "Michael Bolton".into();
        config.search.manual.album = "The Essential Michael Bolton".into();
        config.daemon.enabled = true;
        // Empty library paths: auto mode would fail with
        // "library.paths is empty", proving manual mode ran instead.
        config.library.paths = vec![];
        let staging = TempDir::new().unwrap();
        config.storage.staging_dir = staging.path().to_string_lossy().into();
        let db = Database::open_in_memory().unwrap();

        run_daemon_cycle(&client, &config, &db)
            .await
            .expect("daemon cycle must succeed in manual mode");

        let queries = client.search_queries.lock().unwrap();
        assert!(
            queries.iter().any(|q| q.contains("Michael Bolton")),
            "manual-mode daemon cycle must search for the requested artist, got queries: {queries:?}"
        );
    }

    // Regression: seakarr must exit after a run completes even when a
    // blocking task (e.g. the download status bridge) is still running.
    // Previously main() returned normally on success, and the tokio runtime
    // drop blocked indefinitely on the stuck spawn_blocking task — the
    // process hung and Ctrl+C could not kill it (the SIGINT listener is
    // aborted right after the run summary prints, and tokio's global signal
    // handler swallows further presses).
    #[test]
    fn process_exits_after_run_despite_stuck_blocking_task() {
        if std::env::var("SEAKARR_EXIT_CHILD").is_ok() {
            // Child branch: reproduce main()'s runtime structure — a
            // multi-thread runtime with a stuck blocking task (mimicking the
            // download bridge that never terminates). The exit path must
            // terminate the process without waiting for the runtime drop.
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap();
            // Enter the runtime so the free-function spawn_blocking has a
            // context (mirroring #[tokio::main]'s block_on wrapper).
            let _guard = rt.enter();
            // A blocking task that never returns — the runtime drop would
            // wait for this forever, hanging the process.
            tokio::task::spawn_blocking(|| loop {
                std::thread::sleep(std::time::Duration::from_secs(3600));
            });
            // main()'s exit path after a successful run:
            std::process::exit(exit_code_after_run(Ok(())));
        }

        // Parent branch: spawn the child and assert it exits promptly with
        // code 0. Without the explicit exit, the runtime drop blocks on the
        // stuck blocking task and the child never exits.
        let exe = std::env::current_exe().unwrap();
        let mut child = std::process::Command::new(exe)
            .arg("--exact")
            .arg("tests::process_exits_after_run_despite_stuck_blocking_task")
            .arg("--nocapture")
            .env("SEAKARR_EXIT_CHILD", "1")
            .spawn()
            .expect("failed to spawn child");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        loop {
            match child.try_wait().unwrap() {
                Some(status) => {
                    assert_eq!(
                        status.code(),
                        Some(0),
                        "process must exit cleanly after the run, got {status:?}"
                    );
                    break;
                }
                None => {
                    if std::time::Instant::now() > deadline {
                        let _ = child.kill();
                        panic!(
                            "process did not exit after the run (runtime drop hung on a stuck blocking task — hang reproduced)"
                        );
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            }
        }
    }

    #[test]
    fn exit_code_after_run_maps_result() {
        assert_eq!(exit_code_after_run(Ok(())), 0);
        let err: Result<()> = Err(SeakarrError::Config("bad".into()));
        assert_eq!(exit_code_after_run(err), 1);
    }
}
