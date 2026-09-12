use clap::Parser;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

use seakarr::client::{RealClient, SoulseekClient};
use seakarr::config::{CliOverrides, Config};
use seakarr::db::Database;
use seakarr::error::{Result, SeakarrError};
use seakarr::mode::ExecutionPlan;
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

    /// Repeat the selected operation in a foreground interval loop
    #[arg(long)]
    schedule: bool,

    /// Deprecated compatibility flag; use --schedule
    #[arg(long, hide = true)]
    daemon: bool,

    /// Reprocess an album even when it has a successful processed-album record
    #[arg(long)]
    ignore_processed: bool,
}

/// Program entry point.
///
/// The vendored soulseek crate reads a `LOG_LEVEL` environment variable from
/// the worker threads it spawns during connect/login. `std::env::set_var` is
/// undefined behaviour once other threads exist (it mutates shared process
/// state without synchronisation), and `#[tokio::main]` starts a
/// multi-threaded runtime before the first line of the async body runs. The
/// variable is therefore set here, synchronously, BEFORE the runtime is
/// created. Deriving it needs only the CLI override (config-level logging is
/// re-applied authoritatively inside `run()` via its own `Config::load`).
fn main() {
    let cli = Cli::parse();
    let log_level = cli
        .log_level
        .clone()
        .or_else(|| Config::load(&cli.config_path).ok().map(|c| c.logging.level))
        .unwrap_or_else(|| "info".to_string());
    std::env::set_var("LOG_LEVEL", &log_level);

    // Manual runtime so `LOG_LEVEL` is set before the first worker thread
    // spawns. `exit_code_after_run` bypasses the runtime drop (see its doc
    // comment), so the runtime is never waited on after the run finishes.
    let runtime = tokio::runtime::Runtime::new().expect("failed to initialise tokio runtime");
    std::process::exit(exit_code_after_run(runtime.block_on(run())));
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

    if cli.daemon {
        eprintln!(
            "warning: --daemon is deprecated; use --schedule; \
             support will be removed in the next minor release"
        );
    }

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
        schedule: cli.schedule || cli.daemon,
        test: cli.test,
        ignore_processed: cli.ignore_processed,
    };

    // Validate the selected mode before any startup side effects: logging
    // setup, the --test branch, config.validate(), database open, PID lock,
    // and Soulseek login must all be preceded by this check so manual and
    // batch CLI selectors can never silently enter an incompatible mode.
    let execution_plan = seakarr::mode::resolve_execution_plan(&config, &cli_overrides)?;
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

    // Acquire the PID lock BEFORE logging in. The Soulseek server treats a
    // second login of the same username as a session takeover (the first
    // instance goes "Displaced" and fails permanently), so a second instance
    // that only failed the lock AFTER logging in would already have knocked
    // the running instance offline. Early failures below release the lock so
    // no orphaned PID file is left behind.
    let pid_dir = PathBuf::from(&config.pid.path);
    std::fs::create_dir_all(&pid_dir)?;
    let pid_file = pid_dir.join(&config.pid.file);
    acquire_pid_lock(&pid_file)?;

    // Connect to Soulseek
    // LOG_LEVEL (read by the vendored crate's worker threads) is set in `main`
    // before the tokio runtime starts — std::env::set_var is UB once threads
    // exist, so it must not be called here.
    tracing::info!(
        "Connecting to Soulseek server {}...",
        config.soulseek.server
    );
    let client = RealClient::new();
    if let Err(e) = client
        .login(
            &config.soulseek.username,
            &config.soulseek.password,
            &config.soulseek.server,
            config.soulseek.listen_port,
        )
        .await
    {
        // Release the lock acquired above — a failed login must not leave an
        // orphaned PID file behind.
        if let Err(release_err) = release_pid_lock(&pid_file) {
            tracing::warn!("Failed to release PID file after login error: {release_err}");
        }
        return Err(e);
    }
    tracing::info!("Connected to Soulseek.");

    client.set_max_peers(config.soulseek.max_peers).await?;

    if config.schedule.enabled {
        let interval_mins = config.schedule.interval_mins;
        if interval_mins == 0 {
            tracing::warn!("schedule.interval_mins is 0; clamping to 1 to avoid busy-loop");
        }
        let interval = schedule_interval(interval_mins)?;
        // --ignore-processed + schedule was rejected during mode validation, so
        // the scheduled path always dispatches with false.
        run_schedule(&client, &config, &db, &pid_file, interval, &execution_plan).await
    } else {
        let result =
            dispatch_execution_plan(&client, &execution_plan, &config, &db, cli.ignore_processed)
                .await;
        release_pid_lock(&pid_file)?;
        result
    }
}

/// `--test` mode: structural validation that works even on a freshly created
/// default config (credentials are not yet populated at that point).
fn validate_for_test(config: &Config) -> Result<()> {
    // Same non-credential constraints as Config::validate() so `--test` does
    // not report "valid" for a config that would fail at real startup.
    config.validate_non_credential_constraints()?;
    for path in &config.library.paths {
        if !Path::new(path).exists() {
            tracing::warn!("library path does not exist: {path}");
        }
    }
    tracing::info!("Configuration is valid.");
    Ok(())
}

/// Result of probing a PID file's referenced process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PidLiveness {
    /// The process exists (and we can signal it).
    Alive,
    /// The process no longer exists — the lock is stale.
    Stale,
    /// Liveness cannot be determined from this process's privileges or platform.
    Indeterminate,
}

/// Probe whether the process `pid` is alive using `kill -0` on Unix:
/// exit 0 → alive; "no such process" (ESRCH) → stale; "operation not
/// permitted" (EPERM, a process owned by another user) → alive; any other
/// outcome (spawn failure, unknown exit code) → indeterminate, so the caller
/// refuses to overwrite a potentially live lock.
#[cfg(unix)]
fn pid_is_alive(pid: i32) -> PidLiveness {
    use std::process::Command;
    let output = match Command::new("kill").arg("-0").arg(pid.to_string()).output() {
        Ok(o) => o,
        Err(_) => return PidLiveness::Indeterminate, // spawn failure: do not overwrite
    };
    match output.status.code() {
        Some(0) => PidLiveness::Alive,
        Some(1) => {
            // kill -0 exits 1 for BOTH ESRCH (no such process → stale) and
            // EPERM (process exists but owned by another user → alive). The
            // exit code alone cannot tell them apart, so parse stderr instead
            // of assuming code 1 always means "stale" (the old logic would
            // clobber a live process owned by another user).
            let stderr = String::from_utf8_lossy(&output.stderr).to_lowercase();
            if stderr.contains("no such process") || stderr.contains("process not found") {
                PidLiveness::Stale
            } else if stderr.contains("operation not permitted")
                || stderr.contains("permission denied")
            {
                PidLiveness::Alive
            } else {
                PidLiveness::Indeterminate
            }
        }
        _ => PidLiveness::Indeterminate,
    }
}

#[cfg(not(unix))]
fn pid_is_alive(_pid: i32) -> PidLiveness {
    // No portable liveness probe on non-Unix: an existing parsed PID file is
    // treated as potentially alive so we never clobber a live lock.
    PidLiveness::Indeterminate
}

/// Write the current PID to `pid_file`. Returns an error if another instance
/// is already running.
///
/// The PID file is created atomically with `O_EXCL`/`create_new`, closing the
/// time-of-check-to-time-of-use window of the old exists-then-write sequence:
/// two instances started together cannot both see an absent file and both
/// write. On collision the loser probes the existing PID's liveness — a
/// stale (dead) PID is removed and the atomic create is retried; a live or
/// indeterminate PID causes an error.
fn acquire_pid_lock(pid_file: &Path) -> Result<()> {
    loop {
        let result = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(pid_file);
        match result {
            Ok(mut file) => {
                use std::io::Write;
                file.write_all(std::process::id().to_string().as_bytes())?;
                return Ok(());
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let contents = match std::fs::read_to_string(pid_file) {
                    Ok(c) => c,
                    Err(_) => {
                        // Unreadable PID file (e.g. a directory or a
                        // permission error): cannot verify liveness, so refuse
                        // to overwrite a potentially live lock.
                        return Err(SeakarrError::PidLock(format!(
                            "cannot read PID file {pid_file:?}; another instance may be running. If this is stale, delete it"
                        )));
                    }
                };
                let pid: i32 = match contents.trim().parse() {
                    Ok(p) if p > 0 => p,
                    _ => {
                        // Corrupt/empty PID file is stale — remove and retry.
                        tracing::warn!(
                            "PID file {pid_file:?} is corrupt — removing and continuing"
                        );
                        std::fs::remove_file(pid_file)?;
                        continue;
                    }
                };
                match pid_is_alive(pid) {
                    PidLiveness::Stale => {
                        tracing::warn!(
                            "PID file {pid_file:?} references dead PID {pid} — removing and continuing"
                        );
                        std::fs::remove_file(pid_file)?;
                        continue; // retry the atomic create
                    }
                    PidLiveness::Alive | PidLiveness::Indeterminate => {
                        return Err(SeakarrError::PidLock(format!(
                            "Another instance is running with PID {pid}. If this is stale, delete {pid_file:?}"
                        )));
                    }
                }
            }
            Err(e) => return Err(e.into()),
        }
    }
}

fn release_pid_lock(pid_file: &Path) -> Result<()> {
    if pid_file.exists() {
        std::fs::remove_file(pid_file)?;
    }
    Ok(())
}

fn schedule_interval(interval_mins: u64) -> Result<tokio::time::Duration> {
    let interval_mins = interval_mins.max(1);
    let seconds = interval_mins.checked_mul(60).ok_or_else(|| {
        SeakarrError::Config(format!(
            "schedule.interval_mins is too large; maximum is {}",
            u64::MAX / 60
        ))
    })?;
    Ok(tokio::time::Duration::from_secs(seconds))
}

fn finish_scheduled_shutdown(pid_file: &Path, signal: &str) -> Result<()> {
    tracing::info!("Schedule: received {signal}, shutting down...");
    release_pid_lock(pid_file)
}

/// Dispatch a validated execution plan to the matching runner. This is the
/// only mode-to-runner match in the binary - both one-shot runs and scheduled
/// cycles execute the same already-validated plan with the same criteria.
async fn dispatch_execution_plan(
    client: &dyn SoulseekClient,
    plan: &ExecutionPlan,
    config: &Config,
    db: &Database,
    ignore_processed: bool,
) -> Result<()> {
    match plan {
        ExecutionPlan::Auto => runner::run_auto_mode(client, config, db, ignore_processed).await,
        ExecutionPlan::Manual { artist, album } => {
            runner::run_manual_mode(
                client,
                artist.as_deref(),
                album.as_deref(),
                ignore_processed,
                config,
                db,
            )
            .await
        }
        ExecutionPlan::Batch { file_path } => {
            run_batch_mode(client, file_path, config, db, ignore_processed).await
        }
    }
}

/// Scheduled loop: run immediately using the validated execution plan, then
/// wait until the next cycle or shut down gracefully on SIGINT/SIGTERM.
async fn run_schedule(
    client: &dyn SoulseekClient,
    config: &Config,
    db: &Database,
    pid_file: &Path,
    interval: tokio::time::Duration,
    plan: &ExecutionPlan,
) -> Result<()> {
    let mut sigterm = signal_terminate();

    loop {
        tracing::info!("Schedule: starting cycle...");
        if let Err(error) = run_schedule_cycle(client, config, db, plan).await {
            tracing::error!("Scheduled cycle failed: {error}");
        }

        // Wait for the next cycle time, Ctrl+C, or SIGTERM.
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                finish_scheduled_shutdown(pid_file, "SIGINT")?;
                return Ok(());
            }
            _ = wait_for_sigterm(&mut sigterm) => {
                finish_scheduled_shutdown(pid_file, "SIGTERM")?;
                return Ok(());
            }
            _ = tokio::time::sleep(interval) => {}
        }
    }
}

/// Run one scheduled cycle with the same validated plan used by one-shot execution.
///
/// Keeping dispatch centralized ensures CLI criteria are not reinterpreted and
/// an explicit manual or batch plan cannot fall through to auto mode.
async fn run_schedule_cycle(
    client: &dyn SoulseekClient,
    config: &Config,
    db: &Database,
    plan: &ExecutionPlan,
) -> Result<()> {
    // Scheduled cycles never carry --ignore-processed: the combination is
    // rejected during mode validation before any dispatch.
    dispatch_execution_plan(client, plan, config, db, false).await
}

/// Returns a SIGTERM listener on Unix, or `None` on other platforms.
#[cfg(unix)]
type TerminateSignal = tokio::signal::unix::Signal;

#[cfg(not(unix))]
type TerminateSignal = ();

#[cfg(unix)]
fn signal_terminate() -> Option<TerminateSignal> {
    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok()
}

#[cfg(not(unix))]
fn signal_terminate() -> Option<TerminateSignal> {
    None
}

#[cfg(unix)]
async fn wait_for_sigterm(signal: &mut Option<TerminateSignal>) {
    if let Some(signal) = signal {
        let _ = signal.recv().await;
    } else {
        std::future::pending::<()>().await;
    }
}

#[cfg(not(unix))]
async fn wait_for_sigterm(_signal: &mut Option<TerminateSignal>) {
    std::future::pending::<()>().await;
}

/// Batch mode: process a newline-separated list of `artist - album` lines.
async fn run_batch_mode(
    client: &dyn SoulseekClient,
    batch_path: &str,
    config: &Config,
    db: &Database,
    ignore_processed: bool,
) -> Result<()> {
    let contents = std::fs::read_to_string(batch_path)?;
    let lines: Vec<&str> = contents
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();

    let line_word = if lines.len() == 1 { "line" } else { "lines" };
    tracing::info!("Batch mode: {} {line_word} to process", lines.len());
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
        if artist.is_empty() && album.is_none() {
            tracing::warn!("Batch: skipping line with no artist or album");
            continue;
        }
        let album_display = album.unwrap_or("(all)");

        match seakarr::runner::process_album(
            client,
            artist,
            album,
            ignore_processed,
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

    // Regression: a validated manual plan must reach the manual runner with
    // its artist and album criteria instead of being reinterpreted by the cycle.
    #[tokio::test]
    async fn schedule_cycle_honours_manual_mode_artist_album() {
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
        config.schedule.enabled = true;
        // Empty library paths: auto mode would fail with
        // "library.paths is empty", proving manual mode ran instead.
        config.library.paths = vec![];
        let staging = TempDir::new().unwrap();
        config.storage.staging_dir = staging.path().to_string_lossy().into();
        let db = Database::open_in_memory().unwrap();
        let plan = ExecutionPlan::Manual {
            artist: Some("Michael Bolton".into()),
            album: Some("The Essential Michael Bolton".into()),
        };

        run_schedule_cycle(&client, &config, &db, &plan)
            .await
            .expect("scheduled cycle must succeed in manual mode");

        let queries = client.search_queries.lock().unwrap();
        assert!(
            queries.iter().any(|q| q.contains("Michael Bolton")),
            "manual-mode scheduled cycle must search for the requested artist, got queries: {queries:?}"
        );
    }

    // The first query must be observable while virtual time remains paused;
    // sleeping before the first cycle would make this bounded driver fail.
    #[tokio::test(start_paused = true)]
    async fn schedule_starts_first_cycle_before_waiting() {
        let client = MockClient::new();
        let mut config = Config::default();
        config.download.concurrent = 2;
        config.download.min_upload_speed_kbps = 0;
        config.download.speed_check_wait_secs = 0;
        config.download.max_retries = 1;
        config.download.retry_delay_secs = 0;
        config.notifications.urls.clear();
        config.filters.min_tracks = 0;
        config.library.paths.clear();
        let staging = TempDir::new().unwrap();
        config.storage.staging_dir = staging.path().to_string_lossy().into();
        let db = Database::open_in_memory().unwrap();
        let pid_file = staging.path().join("seakarr.pid");
        let plan = ExecutionPlan::Manual {
            artist: Some("Michael Bolton".into()),
            album: Some("The Essential Michael Bolton".into()),
        };
        let schedule = run_schedule(
            &client,
            &config,
            &db,
            &pid_file,
            tokio::time::Duration::from_secs(3_600),
            &plan,
        );
        tokio::pin!(schedule);

        drive_schedule_until_queries(schedule.as_mut(), &client, 1).await;
    }

    async fn drive_schedule_until_queries<F>(
        mut schedule: std::pin::Pin<&mut F>,
        client: &MockClient,
        expected: usize,
    ) where
        F: std::future::Future<Output = Result<()>>,
    {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            if client.search_queries.lock().unwrap().len() >= expected {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "schedule did not issue {expected} query or queries"
            );
            tokio::select! {
                result = schedule.as_mut() => panic!("schedule ended unexpectedly: {result:?}"),
                _ = tokio::task::yield_now() => {}
            }
        }
    }

    #[cfg(unix)]
    fn receive_child_marker(
        receiver: &std::sync::mpsc::Receiver<String>,
        marker: &str,
        timeout: std::time::Duration,
    ) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return false;
            }
            match receiver.recv_timeout(remaining) {
                Ok(line) if line.contains(marker) => return true,
                Ok(_) => {}
                Err(_) => return false,
            }
        }
    }

    fn scheduled_manual_fixture() -> (MockClient, Config, Database, TempDir, ExecutionPlan) {
        let client = MockClient::new();
        let mut config = Config::default();
        config.download.concurrent = 2;
        config.download.min_upload_speed_kbps = 0;
        config.download.speed_check_wait_secs = 0;
        config.download.max_retries = 1;
        config.download.retry_delay_secs = 0;
        config.notifications.urls.clear();
        config.filters.min_tracks = 0;
        config.library.paths.clear();
        let staging = TempDir::new().unwrap();
        config.storage.staging_dir = staging.path().to_string_lossy().into();
        let database = Database::open_in_memory().unwrap();
        let plan = ExecutionPlan::Manual {
            artist: Some("Michael Bolton".into()),
            album: Some("The Essential Michael Bolton".into()),
        };
        (client, config, database, staging, plan)
    }

    #[test]
    fn schedule_interval_clamps_zero_to_one_minute() {
        assert_eq!(schedule_interval(0).unwrap().as_secs(), 60);
    }

    #[test]
    fn finish_scheduled_shutdown_removes_pid_file() {
        let temp = TempDir::new().unwrap();
        let pid_file = temp.path().join("seakarr.pid");
        std::fs::write(&pid_file, "123").unwrap();

        finish_scheduled_shutdown(&pid_file, "test signal").unwrap();

        assert!(!pid_file.exists());
    }

    #[cfg(unix)]
    #[test]
    fn schedule_handles_real_signals_and_removes_pid_file() {
        if let Ok(signal) = std::env::var("SEAKARR_SCHEDULE_SIGNAL_CHILD") {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                let (client, config, database, staging, plan) = scheduled_manual_fixture();
                let pid_file = staging.path().join("seakarr.pid");
                std::fs::write(&pid_file, "123").unwrap();
                let schedule = run_schedule(
                    &client,
                    &config,
                    &database,
                    &pid_file,
                    tokio::time::Duration::from_secs(3_600),
                    &plan,
                );
                tokio::pin!(schedule);
                drive_schedule_until_queries(schedule.as_mut(), &client, 3).await;
                tokio::select! {
                    result = schedule.as_mut() => {
                        panic!("schedule ended before {signal}: {result:?}")
                    }
                    _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {}
                }
                println!("READY");
                use std::io::Write;
                std::io::stdout().flush().unwrap();

                schedule.await.unwrap();
                assert!(!pid_file.exists(), "{signal} must remove the PID file");
                println!("PID_REMOVED");
                std::io::stdout().flush().unwrap();
            });
            return;
        }

        for (signal_name, signal_number) in [("SIGINT", libc::SIGINT), ("SIGTERM", libc::SIGTERM)] {
            let executable = std::env::current_exe().unwrap();
            let mut child = std::process::Command::new(executable)
                .arg("--exact")
                .arg("tests::schedule_handles_real_signals_and_removes_pid_file")
                .arg("--nocapture")
                .env("SEAKARR_SCHEDULE_SIGNAL_CHILD", signal_name)
                .stdout(std::process::Stdio::piped())
                .spawn()
                .expect("failed to spawn schedule signal child");
            let pid = child.id() as i32;
            let stdout = child.stdout.take().unwrap();
            let (sender, receiver) = std::sync::mpsc::channel();
            let reader = std::thread::spawn(move || {
                use std::io::BufRead;
                for line in std::io::BufReader::new(stdout).lines() {
                    if sender.send(line.unwrap()).is_err() {
                        break;
                    }
                }
            });
            if !receive_child_marker(&receiver, "READY", std::time::Duration::from_secs(10)) {
                let _ = child.kill();
                let _ = child.wait();
                reader.join().unwrap();
                panic!("{signal_name} child did not become ready within 10 seconds");
            }

            unsafe { libc::kill(pid, signal_number) };

            let mut stopped =
                receive_child_marker(&receiver, "PID_REMOVED", std::time::Duration::from_secs(1));
            if !stopped && signal_name == "SIGINT" {
                // A first SIGINT may have reached the active cycle's cancel
                // listener just before the scheduler entered its wait.
                unsafe { libc::kill(pid, signal_number) };
                stopped = receive_child_marker(
                    &receiver,
                    "PID_REMOVED",
                    std::time::Duration::from_secs(9),
                );
            }
            if !stopped {
                let _ = child.kill();
                let _ = child.wait();
                reader.join().unwrap();
                panic!("{signal_name} child did not stop within 10 seconds");
            }
            let status = child.wait().expect("schedule signal child did not exit");
            reader.join().unwrap();
            assert!(status.success(), "{signal_name} child failed: {status:?}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn schedule_reuses_the_same_plan_after_each_interval() {
        let (client, config, database, staging, plan) = scheduled_manual_fixture();
        let pid_file = staging.path().join("seakarr.pid");
        let interval = tokio::time::Duration::from_secs(60);
        let schedule = run_schedule(&client, &config, &database, &pid_file, interval, &plan);
        tokio::pin!(schedule);

        drive_schedule_until_queries(schedule.as_mut(), &client, 3).await;
        tokio::time::advance(interval).await;
        drive_schedule_until_queries(schedule.as_mut(), &client, 6).await;

        let queries = client.search_queries.lock().unwrap();
        assert!(
            queries.len() >= 6,
            "expected two three-tier search cycles: {queries:?}"
        );
        let first_cycle = &queries[..3];
        assert!(
            queries.chunks_exact(3).all(|cycle| cycle == first_cycle),
            "every cycle must reuse the same manual plan: {queries:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn schedule_reuses_an_auto_plan_after_each_interval() {
        let client = MockClient::new();
        let mut config = Config::default();
        config.download.min_upload_speed_kbps = 0;
        config.download.speed_check_wait_secs = 0;
        config.download.max_retries = 1;
        config.download.retry_delay_secs = 0;
        config.notifications.urls.clear();
        config.filters.min_tracks = 0;
        let temp = TempDir::new().unwrap();
        let album_dir = temp.path().join("Artist").join("Album");
        std::fs::create_dir_all(&album_dir).unwrap();
        std::fs::write(album_dir.join("01 - Track.ogg"), b"fake ogg data").unwrap();
        config.library.paths = vec![temp.path().to_string_lossy().into_owned()];
        config.storage.staging_dir = temp.path().join("staging").to_string_lossy().into_owned();
        let database = Database::open_in_memory().unwrap();
        let pid_file = temp.path().join("seakarr.pid");
        let interval = tokio::time::Duration::from_secs(60);
        let plan = ExecutionPlan::Auto;
        let schedule = run_schedule(&client, &config, &database, &pid_file, interval, &plan);
        tokio::pin!(schedule);

        drive_schedule_until_queries(schedule.as_mut(), &client, 3).await;
        tokio::time::advance(interval).await;
        drive_schedule_until_queries(schedule.as_mut(), &client, 6).await;

        let queries = client.search_queries.lock().unwrap();
        assert!(
            queries.len() >= 6,
            "expected two three-tier auto cycles: {queries:?}"
        );
        let first_cycle = &queries[..3];
        assert!(
            queries.chunks_exact(3).all(|cycle| cycle == first_cycle),
            "every cycle must reuse the auto plan: {queries:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn schedule_continues_after_a_failed_cycle() {
        let client = MockClient::new();
        let mut config = Config::default();
        config.library.paths.clear();
        let temp = TempDir::new().unwrap();
        config.storage.staging_dir = temp.path().to_string_lossy().into();
        let database = Database::open_in_memory().unwrap();
        let batch_file = temp.path().join("missing.txt");
        let pid_file = temp.path().join("seakarr.pid");
        let interval = tokio::time::Duration::from_secs(60);
        let plan = ExecutionPlan::Batch {
            file_path: batch_file.to_string_lossy().into_owned(),
        };
        let schedule = run_schedule(&client, &config, &database, &pid_file, interval, &plan);
        tokio::pin!(schedule);

        tokio::select! {
            result = schedule.as_mut() => panic!("failed cycle stopped schedule: {result:?}"),
            _ = tokio::task::yield_now() => {}
        }
        std::fs::write(&batch_file, "Artist - Album\n").unwrap();
        tokio::time::advance(interval).await;
        drive_schedule_until_queries(schedule.as_mut(), &client, 3).await;
    }

    // A validated manual plan must dispatch to the manual runner even with
    // an empty library — the plan's own criteria drive the run, never the
    // auto scanner.
    #[tokio::test]
    async fn dispatches_manual_plan_without_scanning_library() {
        let client = MockClient::new();
        let mut config = Config::default();
        config.soulseek.username = "test".into();
        config.soulseek.password = "test".into();
        config.download.min_upload_speed_kbps = 0;
        config.download.speed_check_wait_secs = 0;
        config.download.max_retries = 1;
        config.download.retry_delay_secs = 0;
        config.notifications.urls = vec![];
        config.filters.min_tracks = 0;
        config.library.paths.clear();
        let staging = TempDir::new().unwrap();
        config.storage.staging_dir = staging.path().to_string_lossy().into();
        let db = Database::open_in_memory().unwrap();
        let plan = ExecutionPlan::Manual {
            artist: Some("Michael Bolton".into()),
            album: Some("The Essential Michael Bolton".into()),
        };

        dispatch_execution_plan(&client, &plan, &config, &db, true)
            .await
            .expect("manual plan must dispatch without a library");

        let queries = client.search_queries.lock().unwrap();
        assert!(
            queries.iter().any(|query| query.contains("Michael Bolton")),
            "manual plan must use its artist, got queries: {queries:?}"
        );
    }

    // A validated batch plan must dispatch to the batch runner with an empty
    // library and process its file line.
    #[tokio::test]
    async fn dispatches_batch_plan_without_scanning_library() {
        let client = MockClient::new();
        let mut config = Config::default();
        config.download.min_upload_speed_kbps = 0;
        config.download.speed_check_wait_secs = 0;
        config.download.max_retries = 1;
        config.download.retry_delay_secs = 0;
        config.notifications.urls = vec![];
        config.filters.min_tracks = 0;
        config.library.paths.clear();
        let temp = TempDir::new().unwrap();
        config.storage.staging_dir = temp.path().to_string_lossy().into();
        let batch_path = temp.path().join("wantlist.txt");
        std::fs::write(&batch_path, " - \nArtist - Album\nArtist - New Album\n").unwrap();
        let db = Database::open_in_memory().unwrap();
        db.mark_album_processed("Artist", "Album", "success")
            .unwrap();
        let plan = ExecutionPlan::Batch {
            file_path: batch_path.to_string_lossy().into_owned(),
        };

        dispatch_execution_plan(&client, &plan, &config, &db, false)
            .await
            .expect("normal batch plan must dispatch without a library");
        let normal_queries = client.search_queries.lock().unwrap().clone();
        assert!(
            !normal_queries.iter().any(|query| query == "Artist Album"),
            "normal batch processing must skip the pre-existing success record"
        );

        dispatch_execution_plan(&client, &plan, &config, &db, true)
            .await
            .expect("forced batch plan must dispatch without a library");

        let queries = client.search_queries.lock().unwrap();
        assert!(
            queries.len() > normal_queries.len(),
            "forced batch processing must issue additional searches"
        );
        assert!(
            queries.iter().any(|query| query == "Artist Album"),
            "forced batch processing must reprocess the pre-existing record, got queries: {queries:?}"
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

    #[test]
    fn test_validation_catches_search_title_match_overflow() {
        let mut config = Config::default();
        config.search.search_title_match = 101;
        let error = validate_for_test(&config).unwrap_err().to_string();
        assert!(error.contains("search.search_title_match"));
    }

    #[test]
    fn test_validation_catches_library_upgrade_without_paths() {
        let mut config = Config::default();
        config.library_upgrade.enabled = true;
        let error = validate_for_test(&config).unwrap_err().to_string();
        assert!(error.contains("library_upgrade.enabled"));
    }

    // ── PID lock (atomic create + liveness classification) ──

    #[test]
    fn pid_lock_writes_current_pid() {
        let dir = TempDir::new().unwrap();
        let pid_file = dir.path().join("seakarr.pid");
        acquire_pid_lock(&pid_file).unwrap();
        assert_eq!(
            std::fs::read_to_string(&pid_file).unwrap(),
            std::process::id().to_string(),
            "lock must contain our own PID"
        );
    }

    #[test]
    fn pid_lock_rejects_live_pid() {
        let dir = TempDir::new().unwrap();
        let pid_file = dir.path().join("seakarr.pid");
        acquire_pid_lock(&pid_file).unwrap(); // holds the lock with our own live PID
        let err = acquire_pid_lock(&pid_file).unwrap_err();
        assert!(
            err.to_string().contains("Another instance is running"),
            "a live PID must be reported as another instance, got: {err}"
        );
    }

    #[test]
    fn pid_lock_replaces_stale_pid_file() {
        let dir = TempDir::new().unwrap();
        let pid_file = dir.path().join("seakarr.pid");
        // A PID far above any real kernel pid_max → kill -0 reports ESRCH.
        std::fs::write(&pid_file, "999999999").unwrap();
        acquire_pid_lock(&pid_file).unwrap();
        assert_eq!(
            std::fs::read_to_string(&pid_file).unwrap(),
            std::process::id().to_string(),
            "stale PID must be replaced by our own"
        );
    }

    #[test]
    fn pid_lock_removes_corrupt_pid_file() {
        let dir = TempDir::new().unwrap();
        let pid_file = dir.path().join("seakarr.pid");
        std::fs::write(&pid_file, "not-a-pid").unwrap();
        acquire_pid_lock(&pid_file).unwrap();
        assert_eq!(
            std::fs::read_to_string(&pid_file).unwrap(),
            std::process::id().to_string(),
            "corrupt PID file must be treated as stale and replaced"
        );
    }
}
