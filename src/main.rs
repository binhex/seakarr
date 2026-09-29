use clap::Parser;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

use seakarr::client::SoulseekClient;
use seakarr::config::{CliOverrides, Config};
use seakarr::db::Database;
use seakarr::error::{Result, SeakarrError};
use seakarr::mode::ExecutionPlan;
use seakarr::runner;

/// The console layer's own filter.
///
/// Everything the registry filter already admitted passes, minus the scan
/// heartbeat while the interactive spinner is showing it instead. Only the
/// console layer carries this filter: the file layer keeps the per-minute line
/// whether or not a terminal is attached.
fn console_targets(heartbeat: bool) -> tracing_subscriber::filter::Targets {
    use tracing_subscriber::filter::{LevelFilter, Targets};
    let base = Targets::new().with_default(LevelFilter::TRACE);
    if heartbeat {
        base
    } else {
        base.with_target("seakarr::scanner", LevelFilter::OFF)
    }
}

/// Adapts a closure to the indicator's console-filter port, so the reload
/// handle's subscriber type never has to be named here.
struct ConsoleFilterFn<F: Fn(bool) + Send + Sync>(F);

impl<F: Fn(bool) + Send + Sync> seakarr::scan_progress::ConsoleFilter for ConsoleFilterFn<F> {
    fn set_console_heartbeat(&self, enabled: bool) {
        (self.0)(enabled);
    }
}

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

    /// Override search mode (auto|manual|batch|discover)
    #[arg(long)]
    mode: Option<String>,

    /// Batch file path (newline-separated artist/album lines)
    #[arg(long)]
    batch_file: Option<PathBuf>,

    /// Artist for manual mode; in discover mode a narrowing filter that also
    /// overrides discover.exclude_artists and the folder-ownership gate
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
    std::process::exit(exit_code_after_run(runtime.block_on(run(cli))));
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

/// The client `run` drives during startup: the shared [`SoulseekClient`]
/// behaviour plus the peer cap, which only the real client can apply. Declared
/// here rather than added to the library's public trait so the public surface
/// stays unchanged.
///
/// [`SoulseekClient`]: seakarr::client::SoulseekClient
#[async_trait::async_trait]
trait StartupClient: seakarr::client::SoulseekClient {
    async fn apply_max_peers(&self, max_peers: usize) -> Result<()>;
}

#[async_trait::async_trait]
impl StartupClient for seakarr::client::RealClient {
    async fn apply_max_peers(&self, max_peers: usize) -> Result<()> {
        self.set_max_peers(max_peers).await
    }
}

/// Builds the client `run` logs in with.
///
/// Production always builds a [`RealClient`](seakarr::client::RealClient) from
/// the loaded config; a test substitutes a double, which is what makes the
/// post-login wiring (peer-cap re-apply, the schedule/dispatch branch, PID
/// release) reachable without a Soulseek server.
type ClientFactory = Box<dyn Fn(&Config) -> Box<dyn StartupClient>>;

fn real_client_factory(config: &Config) -> Box<dyn StartupClient> {
    Box::new(seakarr::client::RealClient::from_config(config))
}

/// Program-internal entry point: run one execution with the production client.
async fn run(cli: Cli) -> Result<()> {
    run_with(cli, Box::new(real_client_factory)).await
}

/// Run one execution: load the config, resolve the mode, install logging, and
/// dispatch. Takes the parsed `Cli` rather than parsing argv itself, so a test
/// can drive the startup path without inheriting the test harness's arguments.
async fn run_with(cli: Cli, client_factory: ClientFactory) -> Result<()> {
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

    let (console_filter, console_handle) =
        tracing_subscriber::reload::Layer::new(console_targets(true));
    // `try_init` rather than `init`: a global subscriber can be installed only
    // once per process, and `init` panics on a second call. Production installs
    // it exactly once, but a test that drives `run` directly shares its process
    // with every other test.
    //
    // A failure is reported rather than swallowed: if the subscriber could not
    // be installed the process would otherwise run with no console output and no
    // file log, and nothing at all would say so.
    if let Err(error) = tracing_subscriber::registry()
        .with(env_filter)
        .with(
            fmt::Layer::new()
                .with_writer(std::io::stdout)
                .with_filter(console_filter),
        )
        .with(
            fmt::Layer::new()
                .with_writer(file_appender)
                .with_ansi(false),
        )
        .try_init()
    {
        eprintln!("seakarr: logging subscriber was not installed: {error}");
    }
    seakarr::scan_progress::install_console_filter(std::sync::Arc::new(ConsoleFilterFn(
        move |enabled: bool| {
            if let Err(error) = console_handle.modify(|targets| *targets = console_targets(enabled))
            {
                // Losing the filter is not worth failing a scan over: the
                // heartbeat would simply keep printing to the console.
                tracing::debug!("could not update the console scan filter: {error}");
            }
        },
    )));

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
    let client = client_factory(&config);
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

    if let Err(e) = client.apply_max_peers(config.soulseek.max_peers).await {
        // The lock was acquired above; a failure here must not leave it behind.
        if let Err(release_err) = release_pid_lock(&pid_file) {
            tracing::warn!("Failed to release PID file after client setup error: {release_err}");
        }
        return Err(e);
    }

    // The shared trait surface is what scheduling and dispatch need; the peer
    // cap above is the only startup-specific call.
    let dispatch_client: &dyn seakarr::client::SoulseekClient = &*client;

    if config.schedule.enabled {
        let interval_mins = config.schedule.interval_mins;
        if interval_mins == 0 {
            tracing::warn!("schedule.interval_mins is 0; clamping to 1 to avoid busy-loop");
        }
        let interval = schedule_interval(interval_mins)?;
        // --ignore-processed + schedule was rejected during mode validation, so
        // the scheduled path always dispatches with false.
        run_schedule(
            dispatch_client,
            &config,
            &db,
            &pid_file,
            interval,
            &execution_plan,
        )
        .await
    } else {
        let result = dispatch_execution_plan(
            dispatch_client,
            &execution_plan,
            &config,
            &db,
            cli.ignore_processed,
        )
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

/// The error to report when a parsed PID file cannot be taken over.
///
/// `Stale` is not an error — the caller removes the file and retries — so this
/// returns `None` for it. Split out from [`acquire_pid_lock`] so both messages
/// (a live PID, and one whose liveness could not be determined) are asserted
/// without needing a real process to exist.
fn pid_lock_conflict(liveness: PidLiveness, pid: i32, pid_file: &Path) -> Option<SeakarrError> {
    match liveness {
        PidLiveness::Stale => None,
        PidLiveness::Alive => Some(SeakarrError::PidLock(format!(
            "Another instance is running with PID {pid}. If this is stale, delete {pid_file:?}"
        ))),
        PidLiveness::Indeterminate => Some(SeakarrError::PidLock(format!(
            "Cannot tell whether PID {pid} is still running, so {pid_file:?} is treated as a live lock. If no seakarr process is running, delete {pid_file:?}"
        ))),
    }
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
    let stderr = String::from_utf8_lossy(&output.stderr);
    classify_kill_failure(output.status.code(), &stderr)
}

/// Classify a `kill -0` failure from its exit code and stderr.
///
/// Split out from [`pid_is_alive`] so the locale-fragile text matching can be
/// unit-tested without spawning a process. `kill -0` exits 1 for BOTH ESRCH (no
/// such process → stale) and EPERM (owned by another user → alive), so the exit
/// code alone cannot tell them apart; anything unrecognised is indeterminate, so
/// a potentially live lock is never overwritten.
#[cfg(unix)]
fn classify_kill_failure(code: Option<i32>, stderr: &str) -> PidLiveness {
    match code {
        Some(0) => PidLiveness::Alive,
        Some(1) => {
            let message = stderr.to_lowercase();
            if message.contains("no such process") || message.contains("process not found") {
                PidLiveness::Stale
            } else if message.contains("operation not permitted")
                || message.contains("permission denied")
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
                if let Some(error) = pid_lock_conflict(pid_is_alive(pid), pid, pid_file) {
                    return Err(error);
                }
                // No conflict, so the recorded process is provably gone.
                tracing::warn!(
                    "PID file {pid_file:?} references dead PID {pid} — removing and continuing"
                );
                std::fs::remove_file(pid_file)?;
                continue; // retry the atomic create
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
        ExecutionPlan::Discover { artist } => {
            runner::run_discover_mode(client, config, db, artist.as_deref(), ignore_processed).await
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

    // Shared cancellation flag: SIGINT aborts the in-flight album download (its
    // staging dir is cleaned by download_album). Batch mode performs no library
    // scan of its own; the flag is armed through the same guard as the other
    // modes so the listener cannot outlive the cycle.
    let (cancel, _guard) = seakarr::runner::arm_cancellation();

    // One artist-folder index per run: the lookup walks the configured roots for
    // directory names only, at most once, and only when a line needs it.
    let artist_folders = seakarr::discover::ArtistFolderIndex::new(config);
    let mut explained: BTreeSet<String> = BTreeSet::new();

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

        // Batch files its own downloads: an artist the library already holds is
        // placed into, and one it does not hold keeps the download in staging.
        // An artist-only line names no album, so there is nothing to file into the
        // artist folder; only a line naming both is placed.
        let target = match album {
            Some(_) => seakarr::runner::automatic_place_target(&artist_folders, artist),
            None => seakarr::runner::LibraryTarget::StagingOnly,
        };
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
            Some(target),
        )
        .await
        {
            Ok(outcome) => {
                // An artist-only line stages by rule rather than because a folder is
                // missing, so the "no library folder found" explanation would be false.
                if album.is_some() {
                    seakarr::runner::explain_staging_outcome(
                        artist,
                        config,
                        &outcome,
                        &mut explained,
                    );
                }
                report.record(artist, album_display, outcome)
            }
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
    // `_guard` aborts the listener as it drops.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use seakarr::client::MockClient;
    use seakarr::db::Database;
    use tempfile::TempDir;

    #[test]
    fn the_console_filter_hides_and_restores_the_scan_heartbeat() {
        use seakarr::scan_progress::ConsoleFilter;
        use std::sync::{Arc, Mutex};
        use tracing_subscriber::layer::SubscriberExt;

        // A writer we can read back, so the assertion is about what actually
        // reached the console rather than about the filter's internal state.
        #[derive(Clone, Default)]
        struct Captured(Arc<Mutex<Vec<u8>>>);

        impl std::io::Write for Captured {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let captured = Captured::default();
        let (filter, handle) = tracing_subscriber::reload::Layer::new(console_targets(true));
        let writer = captured.clone();
        let subscriber = tracing_subscriber::registry().with(
            tracing_subscriber::fmt::Layer::new()
                .with_writer(move || writer.clone())
                .with_ansi(false)
                .with_filter(filter),
        );
        let driver = ConsoleFilterFn(move |enabled: bool| {
            let _ = handle.modify(|targets| *targets = console_targets(enabled));
        });

        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(target: "seakarr::scanner", "heartbeat visible");
            driver.set_console_heartbeat(false);
            tracing::info!(target: "seakarr::scanner", "heartbeat suppressed");
            tracing::info!(
                target: "seakarr::runner",
                "other target still shown"
            );
            driver.set_console_heartbeat(true);
            tracing::info!(target: "seakarr::scanner", "heartbeat restored");
        });

        let captured_text = captured.0.lock().unwrap().clone();
        let text = String::from_utf8(captured_text).unwrap();
        assert!(text.contains("heartbeat visible"), "got:\n{text}");
        assert!(!text.contains("heartbeat suppressed"), "got:\n{text}");
        assert!(
            text.contains("other target still shown"),
            "suppression must be scoped to the scanner target, got:\n{text}"
        );
        assert!(text.contains("heartbeat restored"), "got:\n{text}");
    }

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
    fn schedule_interval_rejects_a_span_that_cannot_be_expressed_in_seconds() {
        // u64::MAX minutes is ~3.5e17 years: the multiply overflows, and the run
        // must report a named configuration error rather than wrap to a tiny
        // interval and busy-loop.
        let error = schedule_interval(u64::MAX).expect_err("an overflowing interval must fail");
        assert!(
            error
                .to_string()
                .contains("schedule.interval_mins is too large"),
            "the error must name the setting, got {error}"
        );
    }

    #[test]
    fn finish_scheduled_shutdown_removes_pid_file() {
        let temp = TempDir::new().unwrap();
        let pid_file = temp.path().join("seakarr.pid");
        std::fs::write(&pid_file, "123").unwrap();

        finish_scheduled_shutdown(&pid_file, "test signal").unwrap();

        assert!(!pid_file.exists());
    }

    #[test]
    fn pid_lock_refuses_a_pid_path_that_cannot_be_read() {
        // A directory at the PID path: POSIX checks O_CREAT|O_EXCL before the
        // open flags that would otherwise reject a directory, so `create_new`
        // reports AlreadyExists (measured: EEXIST on Linux), and reading the path
        // as a PID then fails. Liveness cannot be verified, so the lock must
        // refuse rather than overwrite a possibly-live lock.
        let temp = TempDir::new().unwrap();
        let pid_file = temp.path().join("seakarr.pid");
        std::fs::create_dir(&pid_file).unwrap();

        let error =
            acquire_pid_lock(&pid_file).expect_err("an unreadable PID path must be refused");
        assert!(
            matches!(error, SeakarrError::PidLock(_)),
            "expected a PidLock error, got {error:?}"
        );
        assert!(
            pid_file.is_dir(),
            "refusing the lock must leave the path untouched"
        );
    }

    #[tokio::test]
    async fn a_discover_plan_dispatches_to_the_discover_runner() {
        // Discover refuses a plan it cannot serve. This fixture keeps the
        // default (enabled) discography and clears the library paths, so the
        // refusal must be the library one - the discography check runs first and
        // would otherwise mask it.
        let client = MockClient::new();
        let mut config = Config::default();
        config.notifications.urls = vec![];
        config.library.paths.clear();
        assert!(
            config.discography.enabled,
            "the fixture must exercise the library refusal, not the discography one"
        );
        let temp = TempDir::new().unwrap();
        config.storage.staging_dir = temp.path().to_string_lossy().into();
        let db = Database::open_in_memory().unwrap();
        let plan = ExecutionPlan::Discover { artist: None };

        let error = dispatch_execution_plan(&client, &plan, &config, &db, false)
            .await
            .expect_err("discover without a library must be refused");
        assert!(
            error.to_string().contains("library.paths is empty"),
            "the refusal must name the missing library, got {error}"
        );
        assert!(
            client.search_queries.lock().unwrap().is_empty(),
            "a refused plan must not search before it refuses"
        );
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

    // A batch line naming only an artist ("Artist") has no album to file, so the run
    // must keep the download in staging rather than inventing an album folder (it
    // used to write a folder literally named "Unknown"). A batch line can never be
    // album-only: `run_batch_mode` trims each line, so " - Album" becomes "- Album"
    // and parses as an artist-only line.
    // Runs on one thread so the thread-local subscriber below sees every event.
    #[tokio::test(flavor = "current_thread")]
    async fn an_artist_only_batch_line_keeps_its_download_in_staging() {
        let client = MockClient::new();
        *client.search_results.lock().unwrap() = vec![seakarr::client::SearchResult {
            username: "peer".to_string(),
            speed: 500,
            slots: 1,
            files: vec![seakarr::client::FileInfo {
                name: "Music\\peer\\Artist\\01 - Track.flac".to_string(),
                size: 10_000_000,
                attribs: std::collections::HashMap::new(),
            }],
        }];
        *client.write_files.lock().unwrap() = true;
        let mut config = Config::default();
        config.download.min_upload_speed_kbps = 0;
        config.download.speed_check_wait_secs = 0;
        config.download.max_retries = 1;
        config.download.retry_delay_secs = 0;
        config.notifications.urls = vec![];
        config.filters.min_tracks = 0;
        let temp = TempDir::new().unwrap();
        config.storage.staging_dir = temp.path().to_string_lossy().into();
        let library = temp.path().join("library");
        config.library.paths = vec![library.to_string_lossy().into_owned()];
        // The artist folder exists, so a placement target would be found: without the
        // album-is-required rule the run would write <library>/Artist/Unknown.
        std::fs::create_dir_all(library.join("Artist")).unwrap();
        let batch_path = temp.path().join("wantlist.txt");
        std::fs::write(&batch_path, "Artist\n").unwrap();
        let db = Database::open_in_memory().unwrap();
        let plan = ExecutionPlan::Batch {
            file_path: batch_path.to_string_lossy().into_owned(),
        };

        // A thread-local capture, because the library's LogCapture is not reachable
        // from this crate. An artist-only line stages by rule, so claiming no folder
        // was found would be false whenever the artist folder exists.
        #[derive(Clone, Default)]
        struct Captured(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
        impl std::io::Write for Captured {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let captured = Captured::default();
        let writer = captured.clone();
        let subscriber = tracing_subscriber::registry().with(
            tracing_subscriber::fmt::Layer::new()
                .with_writer(move || writer.clone())
                .with_ansi(false),
        );
        let guard = tracing::subscriber::set_default(subscriber);

        dispatch_execution_plan(&client, &plan, &config, &db, false)
            .await
            .expect("an artist-only batch line must complete");
        drop(guard);

        let logs = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
        assert!(
            !logs
                .lines()
                .any(|line| line.contains("no library folder found") && line.contains("Artist")),
            "an artist-only line stages by rule, so it must not claim no folder was found:\n{logs}"
        );

        assert!(
            temp.path()
                .join("Artist--unknown")
                .join("01 - Track.flac")
                .exists(),
            "an artist-only line has no album to file, so its download stays in staging"
        );
        let albums: Vec<String> = std::fs::read_dir(library.join("Artist"))
            .expect("the artist folder is readable")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            albums.is_empty(),
            "an artist-only line must not invent an album folder, found {albums:?}"
        );
    }

    // A batch line naming an artist and an album places into that artist's existing
    // library folder, which is the behaviour the batch target exists for.
    #[tokio::test]
    async fn a_batch_line_places_into_the_existing_artist_folder() {
        let client = MockClient::new();
        *client.search_results.lock().unwrap() = vec![seakarr::client::SearchResult {
            username: "peer".to_string(),
            speed: 500,
            slots: 1,
            files: vec![seakarr::client::FileInfo {
                name: "Music\\peer\\Artist\\Album\\01 - Track.flac".to_string(),
                size: 10_000_000,
                attribs: std::collections::HashMap::new(),
            }],
        }];
        *client.write_files.lock().unwrap() = true;
        let mut config = Config::default();
        config.download.min_upload_speed_kbps = 0;
        config.download.speed_check_wait_secs = 0;
        config.download.max_retries = 1;
        config.download.retry_delay_secs = 0;
        config.notifications.urls = vec![];
        config.filters.min_tracks = 0;
        let temp = TempDir::new().unwrap();
        config.storage.staging_dir = temp.path().to_string_lossy().into();
        let library = temp.path().join("library");
        config.library.paths = vec![library.to_string_lossy().into_owned()];
        std::fs::create_dir_all(library.join("Artist")).unwrap();
        let batch_path = temp.path().join("wantlist.txt");
        std::fs::write(&batch_path, "Artist - Album\n").unwrap();
        let db = Database::open_in_memory().unwrap();
        let plan = ExecutionPlan::Batch {
            file_path: batch_path.to_string_lossy().into_owned(),
        };

        dispatch_execution_plan(&client, &plan, &config, &db, false)
            .await
            .expect("a batch line must complete");

        assert!(
            library
                .join("Artist")
                .join("Album")
                .join("01 - Track.flac")
                .exists(),
            "the album must be placed into the artist folder the library already holds"
        );
        assert!(
            !temp.path().join("Artist--Album").exists(),
            "a placed album leaves no staging copy"
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

    /// A config that starts a real run but can never reach a Soulseek server:
    /// the server address cannot be parsed, so `login` fails before any connect
    /// attempt and the test costs no wall-clock time. Every path the run writes
    /// to — database, pid, logs and staging — lives under `dir`, so the run
    /// touches nothing outside it.
    fn run_config_fixture(dir: &std::path::Path, library_paths: &[std::path::PathBuf]) -> Cli {
        let library = if library_paths.is_empty() {
            String::new()
        } else {
            let entries: Vec<String> = library_paths
                .iter()
                .map(|path| format!("\n    - {}", path.display()))
                .collect();
            format!("library:\n  paths:{}", entries.concat())
        };
        let yaml = format!(
            "soulseek:\n  username: test-user\n  password: test-pass\n  server: \"no-port\"\n  \
             listen_port: 0\n  max_peers: 1\n  login_retries: 1\n  login_retry_delay_secs: 0\n\
             database:\n  path: {dir}/db\n\
             pid:\n  path: {dir}/pid\n  file: seakarr.pid\n\
             storage:\n  staging_dir: {dir}/staging\n\
             logging:\n  level: INFO\n  path: {dir}/logs\n  file: seakarr.log\n{library}\n",
            dir = dir.display()
        );
        std::fs::write(dir.join("seakarr.yml"), yaml).expect("fixture config must be writable");
        Cli {
            config_path: dir.to_path_buf(),
            log_path: None,
            log_level: None,
            db_path: None,
            pid_path: None,
            library_path: None,
            soulseek_user: None,
            soulseek_password: None,
            listen_port: None,
            mode: None,
            batch_file: None,
            artist: None,
            album: None,
            test: false,
            schedule: false,
            daemon: false,
            ignore_processed: false,
        }
    }

    // `main` cannot be driven from a test (it calls `std::process::exit`), and
    // `run` used to parse argv itself, so the real startup sequence below config
    // loading was never executed by any test. It takes the parsed CLI now, which
    // makes that sequence — logging setup, mode resolution, validation, database
    // open, PID lock, login, lock release — reachable.
    #[tokio::test]
    async fn run_in_test_mode_validates_the_configuration_and_exits() {
        let dir = tempfile::TempDir::new().unwrap();
        // A library path that does not exist: `--test` warns rather than fails.
        let cli = run_config_fixture(dir.path(), &[dir.path().join("absent-library")]);

        run(Cli { test: true, ..cli })
            .await
            .expect("--test must return Ok for a structurally valid configuration");
    }

    /// A startup client double: a [`MockClient`] that records the peer cap the
    /// run applied and can be told to reject it, so the post-login wiring is
    /// reachable without a Soulseek server.
    struct StartupDouble {
        inner: MockClient,
        applied: std::sync::Arc<std::sync::Mutex<Vec<usize>>>,
        searches: std::sync::Arc<std::sync::Mutex<usize>>,
        reject_cap: bool,
        fail_search: bool,
    }

    #[async_trait::async_trait]
    impl SoulseekClient for StartupDouble {
        async fn login(
            &self,
            username: &str,
            password: &str,
            server: &str,
            listen_port: u16,
        ) -> Result<()> {
            self.inner
                .login(username, password, server, listen_port)
                .await
        }

        async fn search(
            &self,
            query: &str,
            timeout_secs: u64,
        ) -> Result<Vec<seakarr::client::SearchResult>> {
            *self.searches.lock().unwrap() += 1;
            if self.fail_search {
                return Err(SeakarrError::Disconnected {
                    reason: "injected search failure".into(),
                });
            }
            self.inner.search(query, timeout_secs).await
        }

        async fn download(
            &self,
            file: &seakarr::client::FileInfo,
            username: &str,
            dir: &Path,
        ) -> Result<seakarr::client::DownloadHandle> {
            self.inner.download(file, username, dir).await
        }

        async fn request_queue_position(&self, username: &str, filename: &str) -> bool {
            self.inner.request_queue_position(username, filename).await
        }
    }

    #[async_trait::async_trait]
    impl StartupClient for StartupDouble {
        async fn apply_max_peers(&self, max_peers: usize) -> Result<()> {
            self.applied.lock().unwrap().push(max_peers);
            if self.reject_cap {
                return Err(SeakarrError::Client("peer cap rejected".into()));
            }
            Ok(())
        }
    }

    /// A batch plan that dispatches without touching the network or a library.
    fn batch_fixture(dir: &std::path::Path) -> Cli {
        let batch_path = dir.join("wantlist.txt");
        std::fs::write(&batch_path, "Artist - Album\n").expect("batch fixture must be writable");
        let mut cli = run_config_fixture(dir, &[]);
        cli.mode = Some("batch".into());
        cli.batch_file = Some(batch_path);
        cli
    }

    fn startup_factory(
        applied: std::sync::Arc<std::sync::Mutex<Vec<usize>>>,
        reject_cap: bool,
        fail_search: bool,
    ) -> ClientFactory {
        startup_factory_watching(
            applied,
            std::sync::Arc::new(std::sync::Mutex::new(0usize)),
            reject_cap,
            fail_search,
        )
    }

    /// As [`startup_factory`], but also sharing a counter of the searches the
    /// double has been asked to run, so a caller can tell when a scheduled cycle
    /// has started.
    fn startup_factory_watching(
        applied: std::sync::Arc<std::sync::Mutex<Vec<usize>>>,
        searches: std::sync::Arc<std::sync::Mutex<usize>>,
        reject_cap: bool,
        fail_search: bool,
    ) -> ClientFactory {
        Box::new(move |_config| {
            Box::new(StartupDouble {
                inner: MockClient::new(),
                applied: std::sync::Arc::clone(&applied),
                searches: std::sync::Arc::clone(&searches),
                reject_cap,
                fail_search,
            })
        })
    }

    /// A scheduled run's config: a manual plan over an empty library, a staging
    /// dir inside `dir`, and a one-minute interval.
    fn schedule_run_config_fixture(dir: &std::path::Path) -> Cli {
        let cli = run_config_fixture(dir, &[]);
        let mut yaml = std::fs::read_to_string(dir.join("seakarr.yml"))
            .expect("the base fixture config must exist");
        yaml.push_str(
            "schedule:\n  enabled: true\n  interval_mins: 1\n\
             download:\n  concurrent: 2\n  min_upload_speed_kbps: 0\n  \
             speed_check_wait_secs: 0\n  max_retries: 1\n  retry_delay_secs: 0\n\
             notifications:\n  urls: []\n\
             filters:\n  min_tracks: 0\n",
        );
        std::fs::write(dir.join("seakarr.yml"), yaml)
            .expect("the schedule config must be writable");
        Cli {
            mode: Some("manual".into()),
            artist: Some("Michael Bolton".into()),
            album: Some("The Essential Michael Bolton".into()),
            ..cli
        }
    }

    // The scheduled branch of the startup path — interval construction and the
    // handover to `run_schedule` — returns only when a signal arrives, so it can
    // only be reached from a real signalled process. The child signals ITSELF
    // once the first cycle has started searching, which is deterministic: the
    // scheduler installs its SIGTERM listener before the first cycle, so the
    // signal cannot be lost to the default disposition.
    // Unix-only: it raises a real SIGTERM and uses the `#[cfg(unix)]`
    // `receive_child_marker` helper and `libc::kill`.
    #[cfg(unix)]
    #[test]
    fn run_with_a_scheduled_plan_releases_the_pid_lock_on_a_real_signal() {
        if let Ok(marker) = std::env::var("SEAKARR_RUN_SCHEDULE_CHILD") {
            let dir = std::path::PathBuf::from(
                std::env::var("SEAKARR_RUN_SCHEDULE_DIR").expect("child needs its directory"),
            );
            let cli = schedule_run_config_fixture(&dir);
            let applied = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let searches = std::sync::Arc::new(std::sync::Mutex::new(0usize));
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();

            runtime.block_on(async {
                let factory = startup_factory_watching(
                    std::sync::Arc::clone(&applied),
                    std::sync::Arc::clone(&searches),
                    false,
                    false,
                );
                let scheduled = run_with(cli, factory);
                tokio::pin!(scheduled);
                loop {
                    tokio::select! {
                        result = scheduled.as_mut() => {
                            panic!("the schedule ended before it was signalled: {result:?}")
                        }
                        _ = tokio::time::sleep(std::time::Duration::from_millis(25)) => {
                            if *searches.lock().unwrap() > 0 {
                                break;
                            }
                        }
                    }
                }

                unsafe { libc::kill(libc::getpid(), libc::SIGTERM) };

                scheduled
                    .await
                    .expect("a signalled scheduled run must finish cleanly");
            });

            assert!(
                !dir.join("pid").join("seakarr.pid").exists(),
                "{marker} must remove the PID file"
            );
            println!("PID_REMOVED");
            use std::io::Write;
            std::io::stdout().flush().unwrap();
            return;
        }

        let dir = TempDir::new().unwrap();
        let executable = std::env::current_exe().unwrap();
        let mut child = std::process::Command::new(executable)
            .arg("--exact")
            .arg("tests::run_with_a_scheduled_plan_releases_the_pid_lock_on_a_real_signal")
            .arg("--nocapture")
            .env("SEAKARR_RUN_SCHEDULE_CHILD", "SIGTERM")
            .env("SEAKARR_RUN_SCHEDULE_DIR", dir.path())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("failed to spawn the scheduled child");
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

        let stopped =
            receive_child_marker(&receiver, "PID_REMOVED", std::time::Duration::from_secs(30));
        if !stopped {
            // The child loops until it has issued a search, so a run that stalls
            // would block this test - and with it the whole bin test binary -
            // forever. Kill it before reporting, as the sibling signal test does.
            let _ = child.kill();
        }
        let status = child.wait().unwrap();
        reader.join().unwrap();

        assert!(
            stopped,
            "the signalled scheduled run must release the PID file"
        );
        assert_eq!(
            status.code(),
            Some(0),
            "a signalled scheduled run must exit cleanly"
        );
    }

    // With this seam the post-login wiring is reachable: the run logs in,
    // re-applies the configured peer cap, dispatches, and releases the PID lock.
    #[tokio::test]
    async fn run_applies_the_peer_cap_and_dispatches_after_a_successful_login() {
        let dir = tempfile::TempDir::new().unwrap();
        let cli = batch_fixture(dir.path());
        let applied = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));

        run_with(
            cli,
            startup_factory(std::sync::Arc::clone(&applied), false, false),
        )
        .await
        .expect("a batch plan over an empty library must dispatch");

        assert_eq!(
            applied.lock().unwrap().as_slice(),
            [1],
            "the configured soulseek.max_peers must be applied to the fresh client"
        );
        assert!(
            !dir.path().join("pid").join("seakarr.pid").exists(),
            "a completed run must release the PID lock"
        );
    }

    // A peer cap the client refuses must abort the run and still release the PID
    // lock, or a later run would refuse to start against a stale PID file.
    #[tokio::test]
    async fn run_releases_the_pid_lock_when_the_peer_cap_is_rejected() {
        let dir = tempfile::TempDir::new().unwrap();
        let cli = batch_fixture(dir.path());
        let applied = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));

        let error = run_with(
            cli,
            startup_factory(std::sync::Arc::clone(&applied), true, false),
        )
        .await
        .expect_err("a rejected peer cap must fail the run");

        assert!(
            matches!(error, SeakarrError::Client(_)),
            "the rejection reason must surface, got {error:?}"
        );
        assert!(
            !dir.path().join("pid").join("seakarr.pid").exists(),
            "a failed client setup must not leave an orphaned PID file behind"
        );
    }

    // A batch line whose album cannot be processed must be reported and must not
    // abort the run; the PID lock is still released.
    #[tokio::test]
    async fn a_failed_batch_line_does_not_abort_the_run() {
        let dir = tempfile::TempDir::new().unwrap();
        let cli = batch_fixture(dir.path());
        let applied = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));

        run_with(cli, startup_factory(applied, false, true))
            .await
            .expect("a failing album must be reported rather than abort the run");

        assert!(
            !dir.path().join("pid").join("seakarr.pid").exists(),
            "a run with a failed album must still release the PID lock"
        );
    }

    #[tokio::test]
    async fn the_real_startup_client_applies_the_cap_through_the_production_impl() {
        // The production `StartupClient` impl delegates to `RealClient`. With no
        // logged-in session it must report that, not silently succeed, so a
        // mis-wired cap can never look applied.
        let client = seakarr::client::RealClient::new();

        let error = StartupClient::apply_max_peers(&client, 4)
            .await
            .expect_err("applying a cap without a session must fail");

        assert!(
            error.to_string().contains("not connected"),
            "the error must say the session is missing, got {error}"
        );
    }

    // `run` installs a console filter whose toggle closure reloads the tracing
    // layer. That filter is process-global and set-once, so which installation
    // wins depends on test order and the reload handle may already be detached
    // from the active subscriber. This pins only what is observable either way:
    // the filter is retrievable after a run and toggling it is panic-free. It is
    // NOT evidence that the reload is live, and no assertion here would catch a
    // broken reload.
    #[tokio::test]
    async fn the_installed_console_filter_can_be_toggled_after_a_run() {
        let dir = tempfile::TempDir::new().unwrap();
        let cli = run_config_fixture(dir.path(), &[]);
        run(Cli { test: true, ..cli })
            .await
            .expect("--test must install the console filter and exit");

        let filter = seakarr::scan_progress::installed_console_filter()
            .expect("a run installs the console filter");
        filter.set_console_heartbeat(false);
        filter.set_console_heartbeat(true);
    }

    #[tokio::test]
    async fn run_releases_the_pid_lock_when_the_login_fails() {
        let dir = tempfile::TempDir::new().unwrap();
        let cli = run_config_fixture(dir.path(), &[]);

        let error = run(cli)
            .await
            .expect_err("an unusable server must fail the run");

        assert!(
            matches!(
                error,
                SeakarrError::Client(_)
                    | SeakarrError::Disconnected { .. }
                    | SeakarrError::Auth { .. }
            ),
            "a login failure must surface as a client, disconnected or auth error, got {error:?}"
        );
        assert!(
            dir.path().join("db").join("seakarr.db").is_file(),
            "the run reached login, which happens after the database is opened"
        );
        assert!(
            !dir.path().join("pid").join("seakarr.pid").exists(),
            "a failed login must not leave an orphaned PID file behind"
        );
    }

    // Ordering evidence the assertion above cannot give: a live PID lock conflict
    // must be refused, and the database must already exist when it is, proving
    // the database is opened BEFORE the lock is taken. `run_with` promises that
    // order so "database errors should not leave a stale pid".
    #[tokio::test]
    async fn the_database_is_opened_before_the_pid_lock_is_taken() {
        let dir = tempfile::TempDir::new().unwrap();
        let cli = run_config_fixture(dir.path(), &[]);
        // A PID file naming this live process: the lock must refuse it.
        std::fs::create_dir_all(dir.path().join("pid")).unwrap();
        std::fs::write(
            dir.path().join("pid").join("seakarr.pid"),
            std::process::id().to_string(),
        )
        .unwrap();

        let error = run_with(
            cli,
            startup_factory(
                std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
                false,
                false,
            ),
        )
        .await
        .expect_err("a live PID lock must be refused");

        assert!(
            matches!(error, SeakarrError::PidLock(_)),
            "expected a PidLock error, got {error:?}"
        );
        assert!(
            dir.path().join("db").join("seakarr.db").is_file(),
            "the database must be open before the lock is refused"
        );
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

    #[cfg(unix)]
    #[test]
    fn kill_failure_classification_covers_every_outcome() {
        // The text of `kill -0`'s error is locale-dependent and the exit code
        // alone cannot separate ESRCH from EPERM, so pin every branch: a change
        // that collapsed the unrecognised case into Stale would clobber a live
        // lock owned by another user.
        assert_eq!(classify_kill_failure(Some(0), ""), PidLiveness::Alive);
        assert_eq!(
            classify_kill_failure(Some(1), "kill: no such process"),
            PidLiveness::Stale
        );
        assert_eq!(
            classify_kill_failure(Some(1), "kill: process not found"),
            PidLiveness::Stale
        );
        assert_eq!(
            classify_kill_failure(Some(1), "kill: Operation not permitted"),
            PidLiveness::Alive,
            "EPERM means the process exists but is owned by another user"
        );
        assert_eq!(
            classify_kill_failure(Some(1), "kill: permission denied"),
            PidLiveness::Alive
        );
        assert_eq!(
            classify_kill_failure(Some(1), "kill: Kein solcher Prozess"),
            PidLiveness::Indeterminate,
            "an unrecognised (localised) message must not be read as stale"
        );
        assert_eq!(
            classify_kill_failure(Some(3), ""),
            PidLiveness::Indeterminate
        );
        assert_eq!(
            classify_kill_failure(None, ""),
            PidLiveness::Indeterminate,
            "a signal-killed probe is not evidence of a stale lock"
        );
    }

    #[test]
    fn pid_lock_conflict_messages_name_the_pid_and_the_file() {
        let file = Path::new("/var/lib/seakarr/seakarr.pid");
        assert!(
            pid_lock_conflict(PidLiveness::Stale, 42, file).is_none(),
            "a stale lock is not a conflict: the caller removes it"
        );

        let alive = pid_lock_conflict(PidLiveness::Alive, 42, file)
            .expect("a live PID is a conflict")
            .to_string();
        assert!(
            alive.contains("Another instance is running with PID 42"),
            "got: {alive}"
        );
        assert!(
            alive.contains("seakarr.pid"),
            "the message must name the file to delete, got: {alive}"
        );

        let unknown = pid_lock_conflict(PidLiveness::Indeterminate, 42, file)
            .expect("an undecidable liveness is a conflict")
            .to_string();
        assert!(
            unknown.contains("Cannot tell whether PID 42"),
            "the indeterminate message must not claim a running instance, got: {unknown}"
        );
        assert!(
            unknown.contains("seakarr.pid"),
            "the message must name the file to delete, got: {unknown}"
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
