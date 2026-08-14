use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

use crate::error::{Result, SeakarrError};

// ── Config structs (matching YAML schema) ──

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)] // all sections optional in YAML; missing sections fall back to Default
pub struct Config {
    pub soulseek: SoulseekConfig,
    pub library: LibraryConfig,
    pub storage: StorageConfig,
    pub search: SearchConfig,
    pub filters: FilterConfig,
    pub download: DownloadConfig,
    pub database: DatabaseConfig,
    pub logging: LoggingConfig,
    pub pid: PidConfig,
    pub notifications: NotificationConfig,
    pub daemon: DaemonConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SoulseekConfig {
    pub username: String,
    pub password: String,
    #[serde(default = "default_soulseek_server")]
    pub server: String,
    #[serde(default = "default_login_retries")]
    pub login_retries: u32,
    #[serde(default = "default_login_retry_delay")]
    pub login_retry_delay_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LibraryConfig {
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default = "default_true")]
    pub scan_on_startup: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageConfig {
    #[serde(default = "default_staging_dir")]
    pub staging_dir: String,
    #[serde(default)]
    pub organize: bool,
    #[serde(default = "default_organize_pattern")]
    pub organize_pattern: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchConfig {
    #[serde(default = "default_search_mode")]
    pub default_mode: String,
    #[serde(default = "default_search_timeout")]
    pub timeout_secs: u64,
    #[serde(default = "default_response_limit")]
    pub response_limit: u32,
    #[serde(default = "default_search_type")]
    pub r#type: String,
    #[serde(default = "default_search_delay")]
    pub delay_secs: f64,
    #[serde(default = "default_block_threshold")]
    pub block_threshold: u32,
    #[serde(default = "default_block_pause")]
    pub block_pause_secs: u64,
    #[serde(default = "default_true")]
    pub fallback_search: bool,
    #[serde(default)]
    pub manual: ManualConfig,
    #[serde(default)]
    pub batch: BatchConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ManualConfig {
    #[serde(default)]
    pub artist: String,
    #[serde(default)]
    pub album: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BatchConfig {
    #[serde(default)]
    pub file_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilterConfig {
    #[serde(default = "default_extensions")]
    pub allowed_extensions: Vec<String>,
    pub min_bitrate: Option<u32>,
    pub min_bitdepth: Option<u32>,
    #[serde(default)]
    pub exclude_words: Vec<String>,
    #[serde(default)]
    pub include_locked: bool,
    #[serde(default = "default_true")]
    pub contiguous_tracks: bool,
    #[serde(default = "default_min_tracks")]
    pub min_tracks: u32,
    #[serde(default = "default_true")]
    pub peer_track_count: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadConfig {
    #[serde(default = "default_concurrent")]
    pub concurrent: usize,
    #[serde(default)]
    pub max_queue_length: u32,
    #[serde(default = "default_max_start_time")]
    pub max_start_time_secs: u64,
    #[serde(default = "default_max_queue_time")]
    pub max_queue_time_secs: u64,
    #[serde(default = "default_min_upload_speed")]
    pub min_upload_speed_kbps: u32,
    #[serde(default = "default_speed_check_wait")]
    pub speed_check_wait_secs: u64,
    #[serde(default = "default_download_timeout")]
    pub timeout_secs: u64,
    #[serde(default = "default_max_download_time")]
    pub max_download_time_mins: u64,
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    #[serde(default = "default_retry_delay")]
    pub retry_delay_secs: u64,
    #[serde(default = "default_min_filtered_users")]
    pub min_filtered_users: usize,
    #[serde(default = "default_skip_retry_hours")]
    pub skip_retry_hours: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatabaseConfig {
    #[serde(default = "default_db_path")]
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingConfig {
    #[serde(default = "default_log_level")]
    pub level: String,
    #[serde(default = "default_log_path")]
    pub path: String,
    #[serde(default = "default_log_file")]
    pub file: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PidConfig {
    #[serde(default = "default_pid_path")]
    pub path: String,
    #[serde(default = "default_pid_file")]
    pub file: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotificationConfig {
    #[serde(default)]
    pub urls: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_rescan_interval")]
    pub rescan_interval_mins: u64,
}

// ── CLI overrides struct ──

#[derive(Debug, Default, Clone)]
pub struct CliOverrides {
    pub log_level: Option<String>,
    pub log_path: Option<String>,
    pub db_path: Option<String>,
    pub pid_path: Option<String>,
    pub library_path: Option<Vec<String>>,
    pub soulseek_user: Option<String>,
    pub soulseek_password: Option<String>,
    pub mode: Option<String>,
    pub batch_file: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub daemon: bool,
    pub test: bool,
}

// ── Default value functions ──

fn default_soulseek_server() -> String {
    "server.slsknet.org:2242".into()
}
fn default_login_retries() -> u32 {
    3
}
fn default_login_retry_delay() -> u64 {
    5
}
fn default_true() -> bool {
    true
}
fn default_min_tracks() -> u32 {
    3
}
fn default_staging_dir() -> String {
    "downloads/staging".into()
}
fn default_organize_pattern() -> String {
    "%artist%/%album%/%track% - %title%.%ext%".into()
}
fn default_search_mode() -> String {
    "auto".into()
}
fn default_search_timeout() -> u64 {
    15
}
fn default_response_limit() -> u32 {
    1000
}
fn default_search_type() -> String {
    "any".into()
}
fn default_search_delay() -> f64 {
    5.0
}
fn default_block_threshold() -> u32 {
    5
}
fn default_block_pause() -> u64 {
    300
}
fn default_extensions() -> Vec<String> {
    vec!["flac".into()]
}
fn default_concurrent() -> usize {
    // 1: auto mode searches albums concurrently, and each search makes the
    // Soulseek server push ConnectToPeer for every result peer (the crate
    // spawns a peer-actor thread per connection). The vendored crate's
    // peer-registry cap (16) bounds the thread count, but keeping seakarr's
    // own concurrency at 1 further reduces search-driven peer churn.
    1
}
fn default_max_start_time() -> u64 {
    120
}
fn default_max_queue_time() -> u64 {
    1800
}
fn default_min_upload_speed() -> u32 {
    250
}
fn default_speed_check_wait() -> u64 {
    30
}
fn default_download_timeout() -> u64 {
    180
}
fn default_max_download_time() -> u64 {
    120
}
fn default_max_retries() -> u32 {
    4
}
fn default_retry_delay() -> u64 {
    30
}
fn default_min_filtered_users() -> usize {
    10
}
fn default_skip_retry_hours() -> u32 {
    24
}
fn default_db_path() -> String {
    "db".into()
}
fn default_log_level() -> String {
    "INFO".into()
}
fn default_log_path() -> String {
    "logs".into()
}
fn default_log_file() -> String {
    "seakarr.log".into()
}
fn default_pid_path() -> String {
    "pids".into()
}
fn default_pid_file() -> String {
    "seakarr.pid".into()
}
fn default_rescan_interval() -> u64 {
    60
}

// ── Default impls for sub-structs ──
// Enable `#[serde(default)]` on Config so YAML files may omit entire sections.
// Each section's default mirrors Config::default() (single source of truth).

impl Default for SoulseekConfig {
    fn default() -> Self {
        Config::default().soulseek
    }
}
impl Default for LibraryConfig {
    fn default() -> Self {
        Config::default().library
    }
}
impl Default for StorageConfig {
    fn default() -> Self {
        Config::default().storage
    }
}
impl Default for SearchConfig {
    fn default() -> Self {
        Config::default().search
    }
}
impl Default for FilterConfig {
    fn default() -> Self {
        Config::default().filters
    }
}
impl Default for DownloadConfig {
    fn default() -> Self {
        Config::default().download
    }
}
impl Default for DatabaseConfig {
    fn default() -> Self {
        Config::default().database
    }
}
impl Default for LoggingConfig {
    fn default() -> Self {
        Config::default().logging
    }
}
impl Default for PidConfig {
    fn default() -> Self {
        Config::default().pid
    }
}
impl Default for NotificationConfig {
    fn default() -> Self {
        Config::default().notifications
    }
}
impl Default for DaemonConfig {
    fn default() -> Self {
        Config::default().daemon
    }
}

// ── Config impl ──

impl Config {
    /// Load config from a directory containing `seakarr.yml`.
    /// Creates a default file if none exists.
    pub fn load(config_dir: &Path) -> Result<Self> {
        let config_file = config_dir.join("seakarr.yml");

        if !config_file.exists() {
            let default_config = Config::default();
            let yaml = serde_yaml::to_string(&default_config).map_err(|e| {
                SeakarrError::Config(format!("failed to serialize default config: {e}"))
            })?;
            fs::create_dir_all(config_dir)
                .map_err(|e| SeakarrError::Config(format!("failed to create config dir: {e}")))?;
            fs::write(
                &config_file,
                format!(
                    "# seakarr.yml — Seakarr Configuration\n# Auto-created on first run.\n\n{yaml}"
                ),
            )
            .map_err(|e| SeakarrError::Config(format!("failed to write default config: {e}")))?;
            return Ok(default_config);
        }

        let contents = fs::read_to_string(&config_file)
            .map_err(|e| SeakarrError::Config(format!("failed to read {config_file:?}: {e}")))?;
        serde_yaml::from_str(&contents)
            .map_err(|e| SeakarrError::Config(format!("failed to parse {config_file:?}: {e}")))
    }

    /// Merge CLI overrides onto config values. CLI takes precedence.
    pub fn merge_cli(&mut self, cli: CliOverrides) {
        if let Some(ref v) = cli.log_level {
            self.logging.level = v.clone();
        }
        if let Some(ref v) = cli.log_path {
            self.logging.path = v.clone();
        }
        if let Some(ref v) = cli.db_path {
            self.database.path = v.clone();
        }
        if let Some(ref v) = cli.pid_path {
            self.pid.path = v.clone();
        }
        if let Some(ref v) = cli.library_path {
            self.library.paths = v.clone();
        }
        if let Some(ref v) = cli.soulseek_user {
            self.soulseek.username = v.clone();
        }
        if let Some(ref v) = cli.soulseek_password {
            self.soulseek.password = v.clone();
        }
        if let Some(ref v) = cli.mode {
            self.search.default_mode = v.clone();
        }
        if cli.daemon {
            self.daemon.enabled = true;
        }
    }

    /// Shared concurrent bounds used by both `validate()` (real startup) and
    /// `--test` mode, so a config that would fail at startup never reports
    /// "valid" under `--test`.
    pub fn validate_concurrent_bounds(concurrent: usize) -> Result<()> {
        if concurrent == 0 {
            return Err(SeakarrError::Config(
                "download.concurrent must be at least 1 (0 blocks all downloads)".into(),
            ));
        }
        if concurrent > 8 {
            return Err(SeakarrError::Config(format!(
                "download.concurrent must be at most 8, got {concurrent}"
            )));
        }
        Ok(())
    }

    /// Shared download-bound checks used by both `validate()` (real startup)
    /// and `validate_for_test` (`--test` mode). Keeps the two paths in sync.
    pub fn validate_download_bounds(&self) -> Result<()> {
        Self::validate_concurrent_bounds(self.download.concurrent)?;
        if self.download.max_retries > 10 {
            return Err(SeakarrError::Config(format!(
                "download.max_retries must be at most 10, got {}",
                self.download.max_retries
            )));
        }
        if self.download.retry_delay_secs > 300 {
            return Err(SeakarrError::Config(format!(
                "download.retry_delay_secs must be at most 300, got {}",
                self.download.retry_delay_secs
            )));
        }
        Ok(())
    }

    /// Validate required fields. Returns Ok(()) or the first error.
    pub fn validate(&self) -> Result<()> {
        if self.soulseek.username.is_empty() {
            return Err(SeakarrError::Config("soulseek.username is required".into()));
        }
        if self.soulseek.password.is_empty() {
            return Err(SeakarrError::Config("soulseek.password is required".into()));
        }
        let valid_levels = ["DEBUG", "INFO", "WARN", "ERROR"];
        if !valid_levels.contains(&self.logging.level.as_str()) {
            return Err(SeakarrError::Config(format!(
                "logging.level must be one of {:?}, got {:?}",
                valid_levels, self.logging.level
            )));
        }
        self.validate_download_bounds()?;
        Ok(())
    }
}

impl Default for Config {
    fn default() -> Self {
        Config {
            soulseek: SoulseekConfig {
                username: String::new(),
                password: String::new(),
                server: default_soulseek_server(),
                login_retries: default_login_retries(),
                login_retry_delay_secs: default_login_retry_delay(),
            },
            library: LibraryConfig {
                paths: vec![],
                scan_on_startup: true,
            },
            storage: StorageConfig {
                staging_dir: default_staging_dir(),
                organize: false,
                organize_pattern: default_organize_pattern(),
            },
            search: SearchConfig {
                default_mode: default_search_mode(),
                timeout_secs: default_search_timeout(),
                response_limit: default_response_limit(),
                r#type: default_search_type(),
                delay_secs: default_search_delay(),
                block_threshold: default_block_threshold(),
                block_pause_secs: default_block_pause(),
                fallback_search: default_true(),
                manual: ManualConfig::default(),
                batch: BatchConfig::default(),
            },
            filters: FilterConfig {
                allowed_extensions: default_extensions(),
                min_bitrate: None,
                min_bitdepth: None,
                exclude_words: vec![],
                include_locked: false,
                contiguous_tracks: default_true(),
                min_tracks: default_min_tracks(),
                peer_track_count: default_true(),
            },
            download: DownloadConfig {
                concurrent: default_concurrent(),
                max_queue_length: 0,
                max_start_time_secs: default_max_start_time(),
                max_queue_time_secs: default_max_queue_time(),
                min_upload_speed_kbps: default_min_upload_speed(),
                speed_check_wait_secs: default_speed_check_wait(),
                timeout_secs: default_download_timeout(),
                max_download_time_mins: default_max_download_time(),
                max_retries: default_max_retries(),
                retry_delay_secs: default_retry_delay(),
                min_filtered_users: default_min_filtered_users(),
                skip_retry_hours: default_skip_retry_hours(),
            },
            database: DatabaseConfig {
                path: default_db_path(),
            },
            logging: LoggingConfig {
                level: default_log_level(),
                path: default_log_path(),
                file: default_log_file(),
            },
            pid: PidConfig {
                path: default_pid_path(),
                file: default_pid_file(),
            },
            notifications: NotificationConfig { urls: vec![] },
            daemon: DaemonConfig {
                enabled: false,
                rescan_interval_mins: default_rescan_interval(),
            },
        }
    }
}

// ── Tests ──

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn sample_yaml() -> &'static str {
        r#"
soulseek:
  username: "testuser"
  password: "testpass"
  server: "server.slsknet.org:2242"
  login_retries: 3
  login_retry_delay_secs: 5

library:
  paths: ["/media/music"]
  scan_on_startup: true

storage:
  staging_dir: "downloads/staging"
  organize: false
  organize_pattern: "%artist%/%album%/%track% - %title%.%ext%"

search:
  default_mode: "auto"
  timeout_secs: 15
  response_limit: 1000
  type: "any"
  delay_secs: 5.0
  block_threshold: 5
  block_pause_secs: 300
  fallback_search: true

filters:
  allowed_extensions: ["flac"]
  min_bitrate: null
  min_bitdepth: null
  exclude_words: []
  include_locked: false
  contiguous_tracks: true
  min_tracks: 3
  peer_track_count: true

download:
  concurrent: 5
  max_queue_length: 0
  max_start_time_secs: 120
  max_queue_time_secs: 1800
  min_upload_speed_kbps: 250
  speed_check_wait_secs: 30
  timeout_secs: 180
  max_download_time_mins: 120
  max_retries: 4
  retry_delay_secs: 30
  min_filtered_users: 10
  skip_retry_hours: 24

database:
  path: "db"

logging:
  level: "INFO"
  path: "logs"
  file: "seakarr.log"

pid:
  path: "pids"
  file: "seakarr.pid"

notifications:
  urls: []

daemon:
  enabled: false
  rescan_interval_mins: 60
"#
    }

    #[test]
    fn test_validate_rejects_zero_and_high_concurrency() {
        // concurrent == 0: blocks all downloads forever.
        let mut config = Config::default();
        config.soulseek.username = "u".into();
        config.soulseek.password = "p".into();
        config.download.concurrent = 0;
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("at least 1"), "got: {err}");

        // concurrent > 8: multiplies search-driven peer churn.
        let mut config = Config::default();
        config.soulseek.username = "u".into();
        config.soulseek.password = "p".into();
        config.download.concurrent = 9;
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("at most 8"), "got: {err}");

        // The shared helper agrees with validate() at the boundary.
        assert!(Config::validate_concurrent_bounds(1).is_ok());
        assert!(Config::validate_concurrent_bounds(8).is_ok());
        assert!(Config::validate_concurrent_bounds(9).is_err());
        assert!(Config::validate_concurrent_bounds(0).is_err());
    }

    #[test]
    fn test_validate_rejects_high_max_retries() {
        // max_retries > 10: a misconfigured retry count wastes time.
        let mut config = Config::default();
        config.soulseek.username = "u".into();
        config.soulseek.password = "p".into();
        config.download.max_retries = 11;
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("max_retries"), "got: {err}");

        // Boundary values accepted.
        let mut config = Config::default();
        config.soulseek.username = "u".into();
        config.soulseek.password = "p".into();
        config.download.max_retries = 10;
        assert!(config.validate().is_ok());

        config.download.max_retries = 0;
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_validate_rejects_high_retry_delay() {
        let mut config = Config::default();
        config.soulseek.username = "u".into();
        config.soulseek.password = "p".into();
        config.download.retry_delay_secs = 301;
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("retry_delay_secs"), "got: {err}");

        config.download.retry_delay_secs = 300;
        assert!(config.validate().is_ok());
    }

    // Regression guard for the thread-explosion bug: with the default
    // concurrency, auto mode fires N concurrent album searches, each of which
    // makes the Soulseek server push ConnectToPeer for every result peer;
    // soulseek-rs-lib spawns a peer-actor thread per connection. At
    // concurrent=5 this blew past the container's pids limit (~1200-1600
    // threads) and downloads never progressed. The vendored crate adds a
    // peer-registry cap as a second line of defence, but the seakarr-side
    // default must stay at 1.
    #[test]
    fn test_default_concurrency_is_thread_safe() {
        let config = Config::default();
        assert_eq!(
            config.download.concurrent, 1,
            "default concurrency must stay at 1 — see the thread-explosion regression notes"
        );
    }

    #[test]
    fn test_load_config_from_yaml() {
        let dir = TempDir::new().unwrap();
        let yaml_path = dir.path().join("seakarr.yml");
        fs::write(&yaml_path, sample_yaml()).unwrap();

        let config = Config::load(dir.path()).unwrap();

        assert_eq!(config.soulseek.username, "testuser");
        assert_eq!(config.soulseek.password, "testpass");
        assert_eq!(config.download.concurrent, 5);
        assert_eq!(config.search.timeout_secs, 15);
        assert_eq!(config.filters.allowed_extensions, vec!["flac"]);
        assert!(config.filters.peer_track_count);
    }

    #[test]
    fn test_fallback_search_defaults_true() {
        let config = Config::default();
        assert!(config.search.fallback_search);
    }

    #[test]
    fn test_fallback_search_from_yaml() {
        let dir = TempDir::new().unwrap();
        let yaml_path = dir.path().join("seakarr.yml");
        fs::write(&yaml_path, sample_yaml()).unwrap();

        let config = Config::load(dir.path()).unwrap();

        assert!(config.search.fallback_search);
    }

    #[test]
    fn test_contiguous_tracks_defaults_true() {
        let config = Config::default();
        assert!(config.filters.contiguous_tracks);
    }

    #[test]
    fn test_contiguous_tracks_from_yaml() {
        let dir = TempDir::new().unwrap();
        let yaml_path = dir.path().join("seakarr.yml");
        fs::write(&yaml_path, sample_yaml()).unwrap();

        let config = Config::load(dir.path()).unwrap();

        assert!(config.filters.contiguous_tracks);
    }

    #[test]
    fn test_peer_track_count_defaults_true() {
        let config = Config::default();
        assert!(config.filters.peer_track_count);
    }

    #[test]
    fn test_peer_track_count_from_yaml() {
        let dir = TempDir::new().unwrap();
        let yaml_path = dir.path().join("seakarr.yml");
        fs::write(&yaml_path, sample_yaml()).unwrap();

        let config = Config::load(dir.path()).unwrap();

        assert!(config.filters.peer_track_count);
    }

    #[test]
    fn test_create_default_config_when_missing() {
        let dir = TempDir::new().unwrap();
        // No seakarr.yml exists

        let config = Config::load(dir.path()).unwrap();

        // Default config should have been created
        let yaml_path = dir.path().join("seakarr.yml");
        assert!(yaml_path.exists());
        // Default values
        assert_eq!(config.soulseek.server, "server.slsknet.org:2242");
        assert_eq!(config.search.timeout_secs, 15);
        assert_eq!(config.download.concurrent, 1);
        // The generated file must serialize every default key, including
        // the new filters toggle (round-trip requirement from the
        // contiguous-track-numbers spec).
        let generated = fs::read_to_string(&yaml_path).unwrap();
        assert!(
            generated.contains("contiguous_tracks: true"),
            "generated default config must contain contiguous_tracks: true"
        );
        assert!(
            generated.contains("min_tracks: 3"),
            "generated default config must contain min_tracks: 3"
        );
    }

    #[test]
    fn test_config_validation_missing_username() {
        let dir = TempDir::new().unwrap();
        // Config without soulseek credentials
        let minimal = r#"
soulseek:
  username: ""
  password: ""
"#;
        fs::write(dir.path().join("seakarr.yml"), minimal).unwrap();

        let config = Config::load(dir.path()).unwrap();
        let result = config.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("username"));
    }

    #[test]
    fn test_merge_cli_overrides() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("seakarr.yml"), sample_yaml()).unwrap();

        let mut config = Config::load(dir.path()).unwrap();

        let cli = CliOverrides {
            log_level: Some("DEBUG".into()),
            db_path: Some("/custom/db".into()),
            library_path: Some(vec!["/other/music".into()]),
            soulseek_user: Some("overrideuser".into()),
            ..Default::default()
        };

        config.merge_cli(cli);
        assert_eq!(config.logging.level, "DEBUG");
        assert_eq!(config.database.path, "/custom/db");
        assert_eq!(config.library.paths, vec!["/other/music"]);
        assert_eq!(config.soulseek.username, "overrideuser");
        // Non-overridden values stay from YAML
        assert_eq!(config.download.concurrent, 5);
    }
}
