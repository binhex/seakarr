use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{Result, SeakarrError};

// ── Config structs (matching YAML schema) ──

/// Where a loaded `Config` came from (absolute file path and the on-disk line
/// of its `search.default_mode` entry), used to point users at the exact YAML
/// line that caused a mode conflict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigSource {
    pub path: PathBuf,
    pub default_mode_line: usize,
}

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
    pub library_upgrade: LibraryUpgradeConfig,
    pub daemon: DaemonConfig,
    // Populated by Config::load for on-disk configs; in-memory configs (e.g.
    // Config::default()) have no source. Skipped by (de)serialization so it
    // never appears in YAML output.
    #[serde(skip)]
    pub(crate) source: Option<ConfigSource>,
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
    #[serde(default = "default_listen_port")]
    pub listen_port: u16,
    #[serde(default = "default_max_peers")]
    pub max_peers: usize,
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
    #[serde(default = "default_search_title_match")]
    pub search_title_match: u32,
    #[serde(default = "default_true", alias = "prefer_reliable_peer")]
    pub peer_reputation: bool,
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
    #[serde(default)]
    pub min_bit_rate: u32, // default: 0 (disabled). >0 = minimum kbps for lossy files
    #[serde(default)]
    pub min_bit_depth: u32, // default: 0 (disabled). >0 = minimum bit depth for lossless files
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
pub struct LibraryUpgradeConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub delete_lesser_quality: bool,
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
    pub listen_port: Option<u16>,
    pub mode: Option<String>,
    pub batch_file: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub daemon: bool,
    pub test: bool,
    /// Runtime-only: bypass the processed-album success check for this run.
    pub ignore_processed: bool,
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
fn default_listen_port() -> u16 {
    2234
}
fn default_max_peers() -> usize {
    64
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
fn default_search_title_match() -> u32 {
    // Match threshold (0-100) for title-word matching; 0 disables the feature.
    70
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
impl Default for LibraryUpgradeConfig {
    fn default() -> Self {
        Config::default().library_upgrade
    }
}
impl Default for DaemonConfig {
    fn default() -> Self {
        Config::default().daemon
    }
}

// ── Config impl ──

/// Find the one-based line number of the `search.default_mode` entry within
/// the on-disk YAML contents. Only the `search:` top-level section is
/// inspected so a same-named key elsewhere cannot be misreported. Both block
/// style (`search:` followed by indented keys) and flow style
/// (`search: {default_mode: auto, ...}`) are handled. Quoted keys, spacing
/// before colons, and inline comments are also accepted. Returns `None` when
/// the entry is absent.
fn find_default_mode_line(contents: &str) -> Option<usize> {
    let document: serde_yaml::Value = serde_yaml::from_str(contents).ok()?;
    let search = document.get("search")?.as_mapping()?;
    if !search.contains_key(serde_yaml::Value::String("default_mode".into())) {
        return None;
    }

    let mut in_search_section = false;
    let mut in_flow_search = false;
    let mut in_root_flow = false;
    let mut search_child_indent = None;
    for (idx, line) in contents.lines().enumerate() {
        let line_number = idx + 1;
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let indent = line.len() - line.trim_start_matches([' ', '\t']).len();
        if indent == 0 {
            if let Some(search_value) = yaml_key_value_start(trimmed, "search") {
                let search_value = search_value.trim();
                if search_value.is_empty() || search_value.starts_with('#') {
                    in_search_section = true;
                    in_flow_search = false;
                    search_child_indent = None;
                } else if search_value.starts_with('{') {
                    in_search_section = true;
                    in_flow_search = true;
                    search_child_indent = None;
                    if flow_value_contains_key(search_value, "default_mode") {
                        return Some(line_number);
                    }
                } else {
                    in_search_section = false;
                    in_flow_search = false;
                    search_child_indent = None;
                }
            } else if trimmed.starts_with('{') {
                in_root_flow = true;
                if flow_value_contains_key(trimmed, "search")
                    && flow_value_contains_key(trimmed, "default_mode")
                {
                    return Some(line_number);
                }
            } else {
                in_search_section = false;
                in_flow_search = false;
                in_root_flow = false;
                search_child_indent = None;
            }
            continue;
        }
        if in_root_flow
            && flow_value_contains_key(trimmed, "search")
            && flow_value_contains_key(trimmed, "default_mode")
        {
            return Some(line_number);
        }
        if in_search_section {
            let flow_key = trimmed.trim_start_matches(',').trim();
            if in_flow_search
                && (yaml_key_value_start(flow_key, "default_mode").is_some()
                    || flow_key.starts_with('{'))
                && flow_value_contains_key(flow_key, "default_mode")
            {
                return Some(line_number);
            }
            let child_indent = search_child_indent.get_or_insert(indent);
            if !in_flow_search
                && *child_indent == indent
                && yaml_key_value_start(trimmed, "default_mode").is_some()
            {
                return Some(line_number);
            }
        }
    }
    None
}

/// Return the value portion of a YAML key when the key is followed by a colon.
/// This intentionally handles the simple key spellings supported by the
/// configuration schema, including quoted keys and whitespace before `:`.
fn yaml_key_value_start<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let line = line.trim_start();
    let unquoted = line.strip_prefix(key);
    let double_quoted_key = format!("\"{key}\"");
    let single_quoted_key = format!("'{key}'");
    let remainder = unquoted
        .or_else(|| line.strip_prefix(&double_quoted_key))
        .or_else(|| line.strip_prefix(&single_quoted_key))?;
    let remainder = remainder.trim_start();
    remainder.strip_prefix(':')
}

/// Check for an exact key in a YAML flow-style mapping.
fn flow_value_contains_key(value: &str, key: &str) -> bool {
    let mut segment_start = 0;
    let mut quote = None;
    for (index, character) in value.char_indices() {
        match (quote, character) {
            (Some('\''), '\'') | (Some('"'), '"') => quote = None,
            (None, '\'') | (None, '"') => quote = Some(character),
            (None, '{' | '}' | ',') => {
                if yaml_key_value_start(value[segment_start..index].trim(), key).is_some() {
                    return true;
                }
                segment_start = index + character.len_utf8();
            }
            _ => {}
        }
    }
    yaml_key_value_start(value[segment_start..].trim(), key).is_some()
}

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
            let written_contents = format!(
                "# seakarr.yml — Seakarr Configuration\n# Auto-created on first run.\n\n{yaml}"
            );
            fs::write(&config_file, &written_contents).map_err(|e| {
                SeakarrError::Config(format!("failed to write default config: {e}"))
            })?;
            return default_config.with_source(&config_file, &written_contents);
        }

        let contents = fs::read_to_string(&config_file)
            .map_err(|e| SeakarrError::Config(format!("failed to read {config_file:?}: {e}")))?;
        Self::reconcile_config_file(&config_file, &contents)?;
        // Re-read after migration: reconcile_config_file may have migrated
        // old keys (e.g. min_bitrate → min_bit_rate) on disk. Parsing the
        // original contents would silently drop the old keys (unknown to
        // serde) and default the new keys to 0, disabling quality filters
        // for the entire first run. Re-reading the migrated file ensures
        // the returned config reflects the current on-disk state.
        let migrated = fs::read_to_string(&config_file).map_err(|e| {
            SeakarrError::Config(format!(
                "failed to re-read {config_file:?} after migration: {e}"
            ))
        })?;
        let config: Config = serde_yaml::from_str(&migrated)
            .map_err(|e| SeakarrError::Config(format!("failed to parse {config_file:?}: {e}")))?;
        config.with_source(&config_file, &migrated)
    }

    /// Attach provenance (absolute path + `search.default_mode` line) taken
    /// from the on-disk contents that were actually parsed, so mode-conflict
    /// diagnostics can name the exact file and line that set the mode.
    fn with_source(mut self, config_file: &Path, contents: &str) -> Result<Self> {
        let path = config_file
            .canonicalize()
            .map_err(|e| SeakarrError::Config(format!("failed to resolve {config_file:?}: {e}")))?;
        let default_mode_line = find_default_mode_line(contents).unwrap_or(0);
        self.source = Some(ConfigSource {
            path,
            default_mode_line,
        });
        Ok(self)
    }

    /// Where this config was loaded from, if it came from disk.
    pub fn source(&self) -> Option<&ConfigSource> {
        self.source.as_ref()
    }

    /// Reconcile the on-disk config with the current schema: add any missing
    /// entries (top-level sections and nested keys) from defaults, and remove
    /// entries that no longer exist in the schema. The original file is
    /// preserved as `seakarr.yml.bak`. No-op when the file already matches the
    /// schema.
    fn reconcile_config_file(config_file: &Path, contents: &str) -> Result<()> {
        let default_value: serde_yaml::Value =
            serde_yaml::to_value(Config::default()).map_err(|e| {
                SeakarrError::Config(format!("failed to serialize default config: {e}"))
            })?;
        let mut file_value: serde_yaml::Value = serde_yaml::from_str(contents)
            .map_err(|e| SeakarrError::Config(format!("failed to parse {config_file:?}: {e}")))?;

        // Migration: rename config keys that changed between versions.
        // Preserves existing values (e.g., min_bitrate: 320 becomes
        // min_bit_rate: 320). Null legacy values are dropped so
        // merge_with_defaults restores the schema default (0 for the u32
        // quality keys, true for peer_reputation).
        let renamed = migrate_rename(&mut file_value, "filters", "min_bitrate", "min_bit_rate")
            | migrate_rename(&mut file_value, "filters", "min_bitdepth", "min_bit_depth")
            | migrate_rename(
                &mut file_value,
                "search",
                "prefer_reliable_peer",
                "peer_reputation",
            );

        let merged = merge_with_defaults(&default_value, &file_value);
        if merged == file_value && !renamed {
            return Ok(());
        }

        let yaml = serde_yaml::to_string(&merged).map_err(|e| {
            SeakarrError::Config(format!("failed to serialize migrated config: {e}"))
        })?;

        let backup_path = config_file.with_extension("yml.bak");
        fs::copy(config_file, &backup_path).map_err(|e| {
            SeakarrError::Config(format!("failed to back up config to {backup_path:?}: {e}"))
        })?;
        fs::write(
            config_file,
            format!("# seakarr.yml — Seakarr Configuration\n\n{yaml}"),
        )
        .map_err(|e| SeakarrError::Config(format!("failed to write migrated config: {e}")))?;
        tracing::info!("config reconciled with current schema (backup: {backup_path:?})");
        Ok(())
    }

    /// Merge CLI overrides onto config values. CLI takes precedence.
    pub fn merge_cli(&mut self, cli: CliOverrides) {
        if let Some(ref v) = cli.log_level {
            self.logging.level = v.to_uppercase();
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
        if let Some(port) = cli.listen_port {
            self.soulseek.listen_port = port;
        }
        if let Some(ref v) = cli.mode {
            self.search.default_mode = v.clone();
        }
        // Persist manual/batch criteria into config so the daemon loop
        // (which only sees Config, never the CLI) can honour them.
        if let Some(ref v) = cli.artist {
            self.search.manual.artist = v.clone();
        }
        if let Some(ref v) = cli.album {
            self.search.manual.album = v.clone();
        }
        if let Some(ref v) = cli.batch_file {
            self.search.batch.file_path = v.clone();
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

    /// Validate configuration constraints that do not require credentials.
    /// Shared by normal startup and `--test` mode to keep their checks in sync.
    pub fn validate_non_credential_constraints(&self) -> Result<()> {
        let valid_levels = ["DEBUG", "INFO", "WARN", "ERROR"];
        if !valid_levels.contains(&self.logging.level.as_str()) {
            return Err(SeakarrError::Config(format!(
                "logging.level must be one of {:?}, got {:?}",
                valid_levels, self.logging.level
            )));
        }
        self.validate_download_bounds()?;
        if self.search.search_title_match > 100 {
            return Err(SeakarrError::Config(format!(
                "search.search_title_match must be 0-100 (0 = disabled), got {}",
                self.search.search_title_match
            )));
        }
        if self.soulseek.max_peers == 0 {
            return Err(SeakarrError::Config(
                "soulseek.max_peers must be at least 1".into(),
            ));
        }
        if self.library_upgrade.enabled && self.library.paths.is_empty() {
            return Err(SeakarrError::Config(
                "library_upgrade.enabled requires at least one library.paths entry".into(),
            ));
        }
        if self.daemon.rescan_interval_mins > u64::MAX / 60 {
            return Err(SeakarrError::Config(
                "daemon.rescan_interval_mins is too large; maximum is 307445734561271883".into(),
            ));
        }
        Ok(())
    }

    /// Validate required fields and all runtime configuration constraints.
    /// Returns `Ok(())` or the first error.
    pub fn validate(&self) -> Result<()> {
        if self.soulseek.username.is_empty() {
            return Err(SeakarrError::Config("soulseek.username is required".into()));
        }
        if self.soulseek.password.is_empty() {
            return Err(SeakarrError::Config("soulseek.password is required".into()));
        }
        self.validate_non_credential_constraints()
    }
}

/// Rename a key within a YAML section, preserving the value.
/// If the old key exists and is not null, its value is copied to the new key
/// (unless the new key is already present with a non-null value — the explicit
/// new-format value wins over the legacy key).
/// If the old key is null, it is dropped and the new key is left absent so
/// merge_with_defaults restores the schema default. Inserting 0 here would
/// produce `prefer_reliable_peer: null` → `peer_reputation: 0`, which
/// serde_yaml cannot parse into bool, permanently breaking Config::load.
/// The old key is always removed if present.
/// If the old key is missing, nothing happens (merge_with_defaults will add
/// the new key with its default).
///
/// Returns `true` if a rename occurred (old key was present), `false` otherwise.
fn migrate_rename(
    config: &mut serde_yaml::Value,
    section: &str,
    old_key: &str,
    new_key: &str,
) -> bool {
    let serde_yaml::Value::Mapping(root) = config else {
        return false;
    };
    let Some(serde_yaml::Value::Mapping(sec)) =
        root.get_mut(serde_yaml::Value::String(section.into()))
    else {
        return false;
    };

    let old_key_yaml = serde_yaml::Value::String(old_key.into());
    let new_key_yaml = serde_yaml::Value::String(new_key.into());

    // Remove the old key and get its value
    let Some(old_val) = sec.remove(&old_key_yaml) else {
        // Old key not present — nothing to migrate.
        // merge_with_defaults will add the new key with its default.
        return false;
    };

    if matches!(old_val, serde_yaml::Value::Null) {
        // null → drop the key; merge_with_defaults restores the schema
        // default. See the doc comment above.
        return true;
    }

    // Preserve the value under the new key — unless the user already wrote
    // an explicit new-format value. A null new value is treated as absent so
    // the meaningful legacy value is not silently discarded.
    let new_value_is_meaningful = sec.get(&new_key_yaml).is_some_and(|value| !value.is_null());
    if !new_value_is_meaningful {
        sec.insert(new_key_yaml, old_val);
    }
    true
}

/// Recursively merge a parsed config file value over the current schema
/// defaults. Every key present in the defaults is kept (file value wins when
/// present, default added when missing); keys present in the file but absent
/// from the schema are dropped. User values are never overwritten.
fn merge_with_defaults(default: &serde_yaml::Value, file: &serde_yaml::Value) -> serde_yaml::Value {
    match (default, file) {
        (serde_yaml::Value::Mapping(default_map), serde_yaml::Value::Mapping(file_map)) => {
            let mut merged = serde_yaml::Mapping::new();
            for (key, default_val) in default_map {
                let value = match file_map.get(key) {
                    Some(file_val) => merge_with_defaults(default_val, file_val),
                    None => default_val.clone(),
                };
                merged.insert(key.clone(), value);
            }
            serde_yaml::Value::Mapping(merged)
        }
        // Scalars, sequences: file value wins when present; otherwise the
        // default (covers missing-but-defaulted values and empty lists).
        // A present-but-null file value (e.g. a bare `filters:` section) must
        // fall back to the default: keeping Null would rewrite the user's
        // file into one that fails the struct parse (null mapping).
        (_, serde_yaml::Value::Null) => default.clone(),
        (_, file_val) => file_val.clone(),
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
                listen_port: default_listen_port(),
                max_peers: default_max_peers(),
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
                search_title_match: default_search_title_match(),
                peer_reputation: default_true(),
                manual: ManualConfig::default(),
                batch: BatchConfig::default(),
            },
            filters: FilterConfig {
                allowed_extensions: default_extensions(),
                min_bit_rate: 0,
                min_bit_depth: 0,
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
            library_upgrade: LibraryUpgradeConfig {
                enabled: false,
                delete_lesser_quality: false,
            },
            daemon: DaemonConfig {
                enabled: false,
                rescan_interval_mins: default_rescan_interval(),
            },
            source: None,
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
  listen_port: 2234
  max_peers: 64

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
  search_title_match: 70
  peer_reputation: true

filters:
  allowed_extensions: ["flac"]
  min_bit_rate: 0
  min_bit_depth: 0
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

    #[test]
    fn test_validate_rejects_zero_max_peers() {
        let mut config = Config::default();
        config.soulseek.username = "u".into();
        config.soulseek.password = "p".into();
        config.soulseek.max_peers = 0;
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("max_peers"), "got: {err}");
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
        assert_eq!(config.soulseek.listen_port, 2234);
        assert_eq!(config.soulseek.max_peers, 64);
        assert_eq!(config.download.concurrent, 5);
        assert_eq!(config.search.timeout_secs, 15);
        assert_eq!(config.filters.allowed_extensions, vec!["flac"]);
        assert!(config.filters.peer_track_count);
    }

    #[test]
    fn load_records_absolute_path_and_default_mode_line() {
        let dir = TempDir::new().unwrap();
        let config_file = dir.path().join("seakarr.yml");
        let yaml = "soulseek:\n  username: user\n  password: pass\nsearch:\n  default_mode: auto\n";
        fs::write(&config_file, yaml).unwrap();

        let config = Config::load(dir.path()).unwrap();
        let source = config.source().expect("loaded config has source");

        assert_eq!(source.path, config_file.canonicalize().unwrap());
        // The reported line must point at search.default_mode in the FINAL
        // on-disk config: reconciliation may rewrite the file, so the line is
        // read back from disk, not from the original fixture.
        let final_contents = fs::read_to_string(&config_file).unwrap();
        let expected_line = find_default_mode_line(&final_contents).expect("default_mode present");
        assert_eq!(source.default_mode_line, expected_line);
        assert!(expected_line > 0);
    }

    #[test]
    fn test_search_title_match_defaults_70() {
        let config = Config::default();
        assert_eq!(config.search.search_title_match, 70);
    }

    #[test]
    fn test_search_title_match_from_yaml() {
        let yaml = r#"
search:
  search_title_match: 50
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.search.search_title_match, 50);
    }

    #[test]
    fn test_search_title_match_zero_disables() {
        // 0 means the title-match feature is disabled; it must parse as 0
        // (not be treated as "unset" and defaulted to 70).
        let yaml = r#"
search:
  search_title_match: 0
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.search.search_title_match, 0);
    }

    #[test]
    fn search_peer_reputation_enabled_by_default() {
        let cfg: SearchConfig = serde_yaml::from_str("{}").unwrap();
        assert!(cfg.peer_reputation);
    }

    #[test]
    fn search_peer_reputation_can_be_disabled() {
        let cfg: SearchConfig = serde_yaml::from_str("peer_reputation: false").unwrap();
        assert!(!cfg.peer_reputation);
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
        assert_eq!(config.soulseek.listen_port, 2234);
        assert_eq!(config.soulseek.max_peers, 64);
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
        let source = config
            .source()
            .expect("auto-created config must retain source provenance");
        assert!(source.default_mode_line > 0);
        assert_eq!(source.path, yaml_path.canonicalize().unwrap());
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
            listen_port: Some(8080),
            ..Default::default()
        };

        config.merge_cli(cli);
        assert_eq!(config.logging.level, "DEBUG");
        assert_eq!(config.database.path, "/custom/db");
        assert_eq!(config.library.paths, vec!["/other/music"]);
        assert_eq!(config.soulseek.username, "overrideuser");
        assert_eq!(config.soulseek.listen_port, 8080);
        // Non-overridden values stay from YAML
        assert_eq!(config.download.concurrent, 5);
    }

    // Regression: --log-level must be case-insensitive so "debug" works
    // the same as "DEBUG".
    #[test]
    fn test_merge_cli_log_level_case_insensitive() {
        let mut config = Config::default();
        config.merge_cli(CliOverrides {
            log_level: Some("debug".into()),
            ..Default::default()
        });
        assert_eq!(config.logging.level, "DEBUG");

        let mut config = Config::default();
        config.merge_cli(CliOverrides {
            log_level: Some("info".into()),
            ..Default::default()
        });
        assert_eq!(config.logging.level, "INFO");

        let mut config = Config::default();
        config.merge_cli(CliOverrides {
            log_level: Some("Debug".into()),
            ..Default::default()
        });
        assert_eq!(config.logging.level, "DEBUG");
    }

    #[test]
    fn test_load_migrates_missing_sections_with_backup() {
        let dir = TempDir::new().unwrap();
        // Existing config WITHOUT the library_upgrade section (pre-v0.12.0 file)
        let minimal = r#"
soulseek:
  username: "testuser"
  password: "testpass"

library:
  paths: ["/media/music"]
"#;
        fs::write(dir.path().join("seakarr.yml"), minimal).unwrap();

        let config = Config::load(dir.path()).unwrap();

        // The loaded config must have the new section populated with defaults
        assert!(!config.library_upgrade.enabled);
        assert!(!config.library_upgrade.delete_lesser_quality);
        // User's existing values preserved
        assert_eq!(config.library.paths, vec!["/media/music"]);
        assert_eq!(config.soulseek.username, "testuser");

        // The file itself must be migrated: library_upgrade section written back
        let migrated = fs::read_to_string(dir.path().join("seakarr.yml")).unwrap();
        assert!(
            migrated.contains("library_upgrade:"),
            "migrated config must contain library_upgrade section, got:\n{migrated}"
        );
        assert!(
            migrated.contains("enabled: false"),
            "migrated config must contain library_upgrade.enabled: false, got:\n{migrated}"
        );
        // User values must survive the migration rewrite
        assert!(
            migrated.contains("/media/music"),
            "migrated config must preserve user library paths, got:\n{migrated}"
        );
        assert!(
            migrated.starts_with("# seakarr.yml — Seakarr Configuration\n\n"),
            "migrated config must use the migrated-file header, got:\n{migrated}"
        );
        assert!(
            !migrated.contains("Auto-created on first run"),
            "migrated user config must not claim it was auto-created"
        );

        // A backup of the original must exist
        let backup = dir.path().join("seakarr.yml.bak");
        assert!(backup.exists(), "backup seakarr.yml.bak must be created");
        let backup_contents = fs::read_to_string(&backup).unwrap();
        assert!(
            !backup_contents.contains("library_upgrade"),
            "backup must contain the ORIGINAL unmigrated config"
        );
        assert!(
            backup_contents.contains("/media/music"),
            "backup must preserve original content"
        );
    }

    #[test]
    fn test_load_migrates_only_once_no_duplicate_backup() {
        let dir = TempDir::new().unwrap();
        let minimal = r#"
soulseek:
  username: "testuser"
  password: "testpass"
"#;
        fs::write(dir.path().join("seakarr.yml"), minimal).unwrap();

        // First load migrates
        Config::load(dir.path()).unwrap();
        let migrated = fs::read_to_string(dir.path().join("seakarr.yml")).unwrap();
        assert!(migrated.contains("library_upgrade:"));

        // Second load must NOT rewrite or create a second backup
        Config::load(dir.path()).unwrap();
        let after = fs::read_to_string(dir.path().join("seakarr.yml")).unwrap();
        assert_eq!(
            migrated, after,
            "already-migrated config must not be rewritten"
        );

        let bak_entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.starts_with("seakarr.yml"))
            .collect();
        assert_eq!(
            bak_entries.len(),
            2,
            "expected only seakarr.yml + seakarr.yml.bak, got: {bak_entries:?}"
        );
    }

    #[test]
    fn test_load_migrates_old_filter_keys_to_new_names() {
        // Regression: Config::load() must return migrated values (not 0)
        // when the config file contains old key names (min_bitrate/min_bitdepth)
        // that were renamed to min_bit_rate/min_bit_depth. The migration
        // rewrites the on-disk file and Config::load() re-reads it.
        let dir = TempDir::new().unwrap();
        let yaml = r#"
soulseek:
  username: test
  password: test
filters:
  allowed_extensions: [flac]
  min_bitrate: 320
  min_bitdepth: 24
  exclude_words: []
  include_locked: false
  contiguous_tracks: true
  min_tracks: 3
  peer_track_count: true
search:
  default_mode: auto
  timeout_secs: 10
  response_limit: 25
  type: global
  delay_secs: 1.0
  block_threshold: 50
  block_pause_secs: 300
  search_title_match: 70
  manual:
    artist: ""
    album: ""
  batch:
    file_path: ""
download:
  concurrent: 3
  max_queue_length: 0
  max_start_time_secs: 120
  max_queue_time_secs: 1800
  min_upload_speed_kbps: 0
  speed_check_wait_secs: 0
  timeout_secs: 300
  max_download_time_mins: 120
  max_retries: 3
  retry_delay_secs: 30
storage:
  staging_dir: /tmp/staging
  organize: true
  organize_pattern: "%artist%/%album%/%track% - %title%.%ext%"
logging:
  level: INFO
  path: ""
daemon:
  enabled: false
  interval_secs: 3600
library:
  paths: []
  scan_on_startup: true
library_upgrade:
  enabled: false
  delete_lesser_quality: false
notifications:
  urls: []
"#;
        fs::write(dir.path().join("seakarr.yml"), yaml).unwrap();

        let config = Config::load(dir.path()).unwrap();

        assert_eq!(
            config.filters.min_bit_rate, 320,
            "Config::load must return migrated min_bit_rate from old min_bitrate key"
        );
        assert_eq!(
            config.filters.min_bit_depth, 24,
            "Config::load must return migrated min_bit_depth from old min_bitdepth key"
        );

        // Verify the on-disk file was migrated
        let contents = fs::read_to_string(dir.path().join("seakarr.yml")).unwrap();
        assert!(
            contents.contains("min_bit_rate: 320"),
            "migrated file must contain min_bit_rate: 320"
        );
        assert!(
            contents.contains("min_bit_depth: 24"),
            "migrated file must contain min_bit_depth: 24"
        );
        assert!(
            !contents.contains("min_bitrate:"),
            "old key min_bitrate must be removed from migrated file"
        );
        assert!(
            !contents.contains("min_bitdepth:"),
            "old key min_bitdepth must be removed from migrated file"
        );
    }

    #[test]
    fn test_load_migrates_nested_missing_entries() {
        let dir = TempDir::new().unwrap();
        // Section present but missing some NESTED keys (simulates a schema change
        // that added new keys inside an existing section)
        let yaml = r#"
soulseek:
  username: "testuser"
  password: "testpass"

filters:
  allowed_extensions: ["flac"]
  contiguous_tracks: true

library:
  paths: ["/media/music"]
"#;
        fs::write(dir.path().join("seakarr.yml"), yaml).unwrap();

        Config::load(dir.path()).unwrap();

        let migrated = fs::read_to_string(dir.path().join("seakarr.yml")).unwrap();
        // Nested keys missing from the filters section must be added with defaults
        assert!(
            migrated.contains("min_tracks: 3"),
            "nested missing entry min_tracks must be added, got:\n{migrated}"
        );
        assert!(
            migrated.contains("peer_track_count: true"),
            "nested missing entry peer_track_count must be added, got:\n{migrated}"
        );
        // User's existing nested values preserved
        assert!(
            migrated.contains("allowed_extensions:\n- \"flac\"")
                || migrated.contains("allowed_extensions:")
        );
        assert!(migrated.contains("contiguous_tracks: true"));
    }

    #[test]
    fn test_load_removes_entries_no_longer_in_schema() {
        let dir = TempDir::new().unwrap();
        // Config containing a section that no longer exists in the schema
        // (simulates a removed config entry from an old version)
        let yaml = r#"
soulseek:
  username: "testuser"
  password: "testpass"

library:
  paths: ["/media/music"]

obsolete_section:
  some_old_option: 42
"#;
        fs::write(dir.path().join("seakarr.yml"), yaml).unwrap();

        Config::load(dir.path()).unwrap();

        let migrated = fs::read_to_string(dir.path().join("seakarr.yml")).unwrap();
        assert!(
            !migrated.contains("obsolete_section"),
            "removed config entry must be dropped, got:\n{migrated}"
        );
        assert!(
            !migrated.contains("some_old_option"),
            "removed nested entry must be dropped, got:\n{migrated}"
        );
        // User's valid values still present
        assert!(migrated.contains("/media/music"));
        assert!(migrated.contains("testuser"));
    }

    #[test]
    fn test_load_preserves_existing_values_never_overwrites() {
        let dir = TempDir::new().unwrap();
        // User's custom values must survive reconciliation unchanged
        let yaml = r#"
soulseek:
  username: "myuser"
  password: "mypass"

search:
  timeout_secs: 99
  response_limit: 1234

library:
  paths: ["/media/music"]
"#;
        fs::write(dir.path().join("seakarr.yml"), yaml).unwrap();

        let config = Config::load(dir.path()).unwrap();

        assert_eq!(config.soulseek.username, "myuser");
        assert_eq!(config.search.timeout_secs, 99);
        assert_eq!(config.search.response_limit, 1234);

        let migrated = fs::read_to_string(dir.path().join("seakarr.yml")).unwrap();
        assert!(
            migrated.contains("timeout_secs: 99"),
            "user value must not be overwritten: {migrated}"
        );
        assert!(migrated.contains("response_limit: 1234"));
        assert!(migrated.contains("myuser"));
    }

    #[test]
    fn test_library_upgrade_defaults_false() {
        let config = Config::default();
        assert!(!config.library_upgrade.enabled);
        assert!(!config.library_upgrade.delete_lesser_quality);
    }

    #[test]
    fn test_library_upgrade_from_yaml() {
        let yaml = r#"
library_upgrade:
  enabled: true
  delete_lesser_quality: true
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert!(config.library_upgrade.enabled);
        assert!(config.library_upgrade.delete_lesser_quality);
    }

    #[test]
    fn test_library_upgrade_requires_library_paths() {
        let mut config = Config::default();
        config.soulseek.username = "u".into();
        config.soulseek.password = "p".into();
        config.library_upgrade.enabled = true;
        config.library.paths = vec![];
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("library.paths"), "got: {err}");
    }

    #[test]
    fn test_library_upgrade_valid_with_paths() {
        let mut config = Config::default();
        config.soulseek.username = "u".into();
        config.soulseek.password = "p".into();
        config.library_upgrade.enabled = true;
        config.library.paths = vec!["/music".into()];
        assert!(config.validate().is_ok());
    }

    // Regression: `--daemon --mode manual --artist X --album Y` must keep
    // the manual criteria reachable from the daemon loop. merge_cli stores
    // mode/daemon but must ALSO persist artist/album/batch_file into the
    // config sections so run_daemon (which only sees Config, never the CLI)
    // can honour the requested search criteria.
    #[test]
    fn merge_cli_persists_manual_and_batch_criteria() {
        let mut config = Config::default();
        config.merge_cli(CliOverrides {
            log_level: None,
            log_path: None,
            db_path: None,
            pid_path: None,
            library_path: None,
            soulseek_user: None,
            soulseek_password: None,
            listen_port: None,
            mode: Some("manual".into()),
            batch_file: None,
            artist: Some("Michael Bolton".into()),
            album: Some("The Essential Michael Bolton".into()),
            daemon: true,
            test: false,
            ignore_processed: false,
        });

        assert_eq!(config.search.default_mode, "manual");
        assert_eq!(config.search.manual.artist, "Michael Bolton");
        assert_eq!(config.search.manual.album, "The Essential Michael Bolton");
        assert!(config.daemon.enabled);

        // Batch criteria must survive as well.
        let mut config = Config::default();
        config.merge_cli(CliOverrides {
            mode: Some("batch".into()),
            batch_file: Some("/tmp/albums.txt".into()),
            artist: None,
            album: None,
            daemon: true,
            ..Default::default()
        });
        assert_eq!(config.search.default_mode, "batch");
        assert_eq!(config.search.batch.file_path, "/tmp/albums.txt");
    }

    // ── migrate_rename tests ──

    #[test]
    fn test_filter_config_min_bit_rate_default_is_zero() {
        let config = Config::default();
        assert_eq!(
            config.filters.min_bit_rate, 0,
            "min_bit_rate default should be 0 (disabled)"
        );
    }

    #[test]
    fn test_filter_config_min_bit_depth_default_is_zero() {
        let config = Config::default();
        assert_eq!(
            config.filters.min_bit_depth, 0,
            "min_bit_depth default should be 0 (disabled)"
        );
    }

    #[test]
    fn test_filter_config_from_yaml_parses_min_bit_rate() {
        let yaml = r#"
soulseek:
  username: test
  password: test
filters:
  min_bit_rate: 320
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.filters.min_bit_rate, 320);
    }

    #[test]
    fn test_filter_config_from_yaml_parses_min_bit_depth() {
        let yaml = r#"
soulseek:
  username: test
  password: test
filters:
  min_bit_depth: 24
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.filters.min_bit_depth, 24);
    }

    #[test]
    fn test_reconcile_migrates_min_bitrate_to_min_bit_rate() {
        let dir = TempDir::new().unwrap();
        let config_file = dir.path().join("seakarr.yml");
        fs::write(
            &config_file,
            r#"
soulseek:
  username: test
  password: test
filters:
  min_bitrate: 320
  min_bitdepth: 16
"#,
        )
        .unwrap();

        Config::reconcile_config_file(&config_file, &fs::read_to_string(&config_file).unwrap())
            .unwrap();

        let contents = fs::read_to_string(&config_file).unwrap();
        let config: serde_yaml::Value = serde_yaml::from_str(&contents).unwrap();
        let filters = config.get("filters").unwrap();

        assert_eq!(
            filters["min_bit_rate"].as_u64().unwrap(),
            320,
            "min_bitrate: 320 must become min_bit_rate: 320"
        );
        assert_eq!(
            filters["min_bit_depth"].as_u64().unwrap(),
            16,
            "min_bitdepth: 16 must become min_bit_depth: 16"
        );
        assert!(
            filters.get("min_bitrate").is_none(),
            "old key min_bitrate must be removed"
        );
        assert!(
            filters.get("min_bitdepth").is_none(),
            "old key min_bitdepth must be removed"
        );
    }

    #[test]
    fn test_migrate_rename_preserves_value() {
        let mut config: serde_yaml::Value = serde_yaml::from_str(
            r#"
filters:
  min_bitrate: 320
"#,
        )
        .unwrap();
        migrate_rename(&mut config, "filters", "min_bitrate", "min_bit_rate");
        let filters = config.get("filters").unwrap();
        assert_eq!(filters["min_bit_rate"].as_u64().unwrap(), 320);
    }

    #[test]
    fn test_migrate_rename_null_drops_key_for_schema_default() {
        let mut config: serde_yaml::Value = serde_yaml::from_str(
            r#"
filters:
  min_bitrate: null
"#,
        )
        .unwrap();
        migrate_rename(&mut config, "filters", "min_bitrate", "min_bit_rate");
        let filters = config.get("filters").unwrap();
        // null → the key is dropped; merge_with_defaults restores the schema
        // default (0 here, true for the bool peer_reputation key) upstream.
        assert!(
            filters.get("min_bitrate").is_none(),
            "legacy key must be removed"
        );
        assert!(
            filters.get("min_bit_rate").is_none(),
            "null legacy value must not be eagerly materialised; the merge restores the default"
        );
    }

    #[test]
    fn test_migrate_rename_null_search_key_does_not_emit_number() {
        // Regression: `prefer_reliable_peer: null` must NOT become
        // `peer_reputation: 0` (an integer). serde_yaml cannot deserialize an
        // integer into bool, so the migrated file would permanently break
        // Config::load. The null value is dropped and merge_with_defaults
        // restores the bool default (true).
        let mut config: serde_yaml::Value = serde_yaml::from_str(
            r#"
search:
  prefer_reliable_peer: null
"#,
        )
        .unwrap();
        migrate_rename(
            &mut config,
            "search",
            "prefer_reliable_peer",
            "peer_reputation",
        );
        let search = config.get("search").unwrap();
        assert!(
            search.get("prefer_reliable_peer").is_none(),
            "legacy key must be removed"
        );
        assert!(
            search.get("peer_reputation").is_none(),
            "peer_reputation must stay absent (bool default restored by merge), got: {:?}",
            search.get("peer_reputation")
        );
    }

    #[test]
    fn test_migrate_rename_new_key_wins_when_both_present() {
        // A file with both the legacy and the new-format key must keep the
        // user's explicit new-format value — the legacy key must not clobber it.
        let mut config: serde_yaml::Value = serde_yaml::from_str(
            r#"
filters:
  min_bit_rate: 128
  min_bitrate: 320
"#,
        )
        .unwrap();
        migrate_rename(&mut config, "filters", "min_bitrate", "min_bit_rate");
        let filters = config.get("filters").unwrap();
        assert_eq!(
            filters["min_bit_rate"].as_u64().unwrap(),
            128,
            "explicit new-format value must win over the legacy key"
        );
        assert!(
            filters.get("min_bitrate").is_none(),
            "legacy key must be removed"
        );
    }

    #[test]
    fn test_reconcile_null_section_falls_back_to_defaults() {
        // A bare `filters:` section (null value) must be repaired to the
        // schema defaults instead of being rewritten as `filters: null`,
        // which would fail the subsequent struct parse in Config::load.
        let dir = TempDir::new().unwrap();
        let config_file = dir.path().join("seakarr.yml");
        fs::write(
            &config_file,
            r#"
soulseek:
  username: test
  password: test
filters:
"#,
        )
        .unwrap();

        let config = Config::load(dir.path()).unwrap();
        assert_eq!(
            config.filters.allowed_extensions,
            vec!["flac"],
            "null section must fall back to schema defaults"
        );

        let contents = fs::read_to_string(&config_file).unwrap();
        let written: serde_yaml::Value = serde_yaml::from_str(&contents).unwrap();
        assert!(written.get("filters").is_some());
        assert!(
            !written["filters"].is_null(),
            "rewritten config must not keep a null filters section"
        );
    }

    #[test]
    fn test_load_null_legacy_bool_uses_bool_default() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("seakarr.yml"),
            r#"
soulseek:
  username: test
  password: test
search:
  prefer_reliable_peer: null
"#,
        )
        .unwrap();

        let config = Config::load(dir.path()).unwrap();
        assert!(config.search.peer_reputation);
        let contents = fs::read_to_string(dir.path().join("seakarr.yml")).unwrap();
        assert!(contents.contains("peer_reputation: true"));
        assert!(!contents.contains("peer_reputation: 0"));
    }

    #[test]
    fn test_load_null_new_key_preserves_legacy_value() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("seakarr.yml"),
            r#"
soulseek:
  username: test
  password: test
search:
  prefer_reliable_peer: false
  peer_reputation: null
"#,
        )
        .unwrap();

        let config = Config::load(dir.path()).unwrap();
        assert!(!config.search.peer_reputation);
        let contents = fs::read_to_string(dir.path().join("seakarr.yml")).unwrap();
        assert!(contents.contains("peer_reputation: false"));
    }

    #[test]
    fn test_find_default_mode_line_block_style() {
        let contents = concat!(
            "# seakarr.yml\n",
            "soulseek:\n",
            "  username: user\n",
            "search:\n",
            "  default_mode: auto\n",
            "  timeout_secs: 15\n",
        );
        assert_eq!(find_default_mode_line(contents), Some(5));
    }

    #[test]
    fn test_find_default_mode_line_crlf_and_comments() {
        // CRLF line endings, comments, and a bracketed value must not confuse
        // the section scanner.
        let contents = "search:\r\n  # comment\r\n  default_mode: manual\r\n";
        assert_eq!(find_default_mode_line(contents), Some(3));
    }

    #[test]
    fn test_find_default_mode_line_flow_style() {
        // A schema-complete flow-style `search:` maps the key onto one line.
        let contents = concat!(
            "soulseek:\n",
            "  username: user\n",
            "search: {default_mode: auto, timeout_secs: 15}\n",
        );
        assert_eq!(find_default_mode_line(contents), Some(3));
    }

    #[test]
    fn test_find_default_mode_line_absent_returns_none() {
        let contents = concat!("soulseek:\n", "  username: user\n");
        assert_eq!(find_default_mode_line(contents), None);
    }

    #[test]
    fn test_find_default_mode_line_ignores_other_sections() {
        // A `default_mode:` key under a different top-level section must not
        // be reported.
        let contents = concat!(
            "search:\n",
            "  timeout_secs: 15\n",
            "other:\n",
            "  default_mode: 1\n",
        );
        assert_eq!(find_default_mode_line(contents), None);
    }

    #[test]
    fn test_find_default_mode_line_accepts_valid_yaml_spacing_and_comments() {
        let contents = concat!("search : # configure search\n", "  default_mode : auto\n",);
        assert_eq!(find_default_mode_line(contents), Some(2));

        let quoted = "\"search\": {default_mode : auto}\n";
        assert_eq!(find_default_mode_line(quoted), Some(1));

        let single_quoted = "'search': {default_mode: auto}\n";
        assert_eq!(find_default_mode_line(single_quoted), Some(1));
    }

    #[test]
    fn test_validate_rejects_unrepresentable_daemon_interval() {
        let mut config = Config::default();
        config.daemon.rescan_interval_mins = u64::MAX;
        let error = config
            .validate_non_credential_constraints()
            .unwrap_err()
            .to_string();
        assert!(error.contains("daemon.rescan_interval_mins"));
    }

    #[test]
    fn test_find_default_mode_line_ignores_nested_values_and_handles_root_flow() {
        let nested = concat!(
            "search:\n",
            "  batch:\n",
            "    file_path: |\n",
            "      default_mode: not_the_setting\n",
        );
        assert_eq!(find_default_mode_line(nested), None);

        let root_flow = "{soulseek: {username: user}, search: {default_mode: auto}}\n";
        assert_eq!(find_default_mode_line(root_flow), Some(1));

        let multiline_flow = concat!(
            "search: {\n",
            "  batch: {file_path: \"\"},\n",
            "    default_mode: auto\n",
            "}\n",
        );
        assert_eq!(find_default_mode_line(multiline_flow), Some(3));

        let quoted_fake = "search: {batch: {file_path: \"{default_mode: auto}\"}}\n";
        assert_eq!(find_default_mode_line(quoted_fake), None);
    }

    #[test]
    fn test_migrate_rename_missing_key() {
        let mut config: serde_yaml::Value = serde_yaml::from_str(
            r#"
filters:
  allowed_extensions: [flac]
"#,
        )
        .unwrap();
        migrate_rename(&mut config, "filters", "min_bitrate", "min_bit_rate");
        let filters = config.get("filters").unwrap();
        // No old key → new key not added (merge_with_defaults will add it)
        assert!(filters.get("min_bit_rate").is_none());
    }

    #[test]
    fn test_migrate_rename_removes_old_key() {
        let mut config: serde_yaml::Value = serde_yaml::from_str(
            r#"
filters:
  min_bitrate: 320
"#,
        )
        .unwrap();
        migrate_rename(&mut config, "filters", "min_bitrate", "min_bit_rate");
        let filters = config.get("filters").unwrap();
        assert!(
            filters.get("min_bitrate").is_none(),
            "old key must be removed"
        );
        assert!(filters.get("min_bit_rate").is_some(), "new key must exist");
    }

    #[test]
    fn test_migrate_rename_prefer_reliable_peer_preserves_value() {
        let mut config: serde_yaml::Value = serde_yaml::from_str(
            r#"
search:
  prefer_reliable_peer: false
"#,
        )
        .unwrap();
        migrate_rename(
            &mut config,
            "search",
            "prefer_reliable_peer",
            "peer_reputation",
        );
        let search = config.get("search").unwrap();
        assert!(
            search.get("prefer_reliable_peer").is_none(),
            "old key must be removed"
        );
        assert!(
            !search["peer_reputation"].as_bool().unwrap(),
            "the explicit opt-out must be preserved, not flipped to the default true"
        );
    }
}
