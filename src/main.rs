use clap::Parser;
use std::path::{Path, PathBuf};
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
    if let Err(e) = run().await {
        eprintln!("seakarr: {e}");
        std::process::exit(1);
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
    std::env::set_var("LOG_LEVEL", "ERROR");
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
        )
        .await?;
    tracing::info!("Connected to Soulseek.");

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
    Config::validate_concurrent_bounds(config.download.concurrent)?;
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

/// Daemon loop: run an auto-mode scan cycle, then sleep until the next cycle
/// or shut down gracefully on SIGINT/SIGTERM.
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
        if let Err(e) = runner::run_auto_mode(client, config, db).await {
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

    let mut succeeded = 0;
    let mut failed = 0;

    for line in &lines {
        let parts: Vec<&str> = line.splitn(2, " - ").collect();
        let artist = parts[0].trim();
        let album = parts.get(1).map(|a| a.trim()).filter(|a| !a.is_empty());

        match runner::process_album(client, artist, album, config, db, staging_dir).await {
            Ok(()) => succeeded += 1,
            Err(e) => {
                tracing::error!("Batch: failed {artist} — {album:?}: {e}");
                failed += 1;
            }
        }
    }

    tracing::info!(
        "Batch complete: {succeeded} succeeded, {failed} failed, {} total",
        lines.len()
    );
    Ok(())
}
