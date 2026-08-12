# Seakarr — Rust CLI Soulseek Downloader: Design Spec

**Date:** 2026-08-12
**Status:** Approved, awaiting implementation
**Context:** Third iteration of Seakarr. Previous attempts: Python+slskd (slow, unreliable API, complexity
spiral) and Android Kotlin app (no maintained JVM Soulseek library, abandoned). This iteration uses the
pure-Rust `soulseek-rs-lib` crate for direct Soulseek protocol access — no slskd intermediary.

---

## 1. Architecture Overview

Single Cargo binary crate (`seakarr`) with 11 internal modules. tokio async runtime. Each module has one
clear responsibility and communicates through well-defined public APIs. Dependencies only flow downward.

```
src/
├── main.rs          # CLI entry point (clap derive)
├── config.rs        # YAML config loading/merging/validation
├── db.rs            # SQLite (rusqlite) — all persistence
├── scanner.rs       # Library walker (walkdir) + tag reader (lofty)
├── client.rs        # soulseek-rs-lib wrapper (trait-based for testability)
├── search.rs        # Search orchestration — execute, collect, deduplicate
├── download.rs      # Download from candidate, speed monitoring, retry, cancel
├── filter.rs        # Quality/candidate filtering and scoring
├── organizer.rs     # Post-download file organization (staging → library)
├── notifier.rs      # Apprise webhook POST calls
└── runner.rs        # Orchestrator — mode dispatch, concurrency, shutdown
```

**Key architectural decision:** The soulseek-rs-lib wrapper (`client.rs`) is behind a `SoulseekClient`
trait. This makes 90% of the pipeline testable without a live Soulseek network. See Section 7 (Testing).

---

## 2. Config Schema (`seakarr.yml`)

Sectioned YAML. Auto-created with defaults on first run if the file doesn't exist. Config file is always
named `seakarr.yml` inside the directory specified by `--config-path` (default: `configs/`).

**Resolution order (later wins):** code defaults → seakarr.yml → CLI flags.

```yaml
# seakarr.yml — Seakarr Configuration

soulseek:
  username: ""                           # Soulseek login (or via --soulseek-user)
  password: ""                           # Soulseek password (or via --soulseek-password)
  server: "server.slsknet.org:2242"      # Soulseek server address
  login_retries: 3                       # Max login attempts with backoff
  login_retry_delay_secs: 5              # Initial backoff (doubles each retry)

library:
  paths: []                              # Music library dirs (or via --library-path)
  scan_on_startup: true                  # Rescan on each run (false = use cached)

storage:
  staging_dir: "downloads/staging"       # In-progress downloads land here (auto-created)
  organize: false                        # Auto-move completed downloads to library
  organize_pattern: "%artist%/%album%/%track% - %title%.%ext%"
  # Placeholders: %artist%, %album%, %track%, %title%, %ext%, %user%

search:
  default_mode: "auto"                   # auto | manual | batch (or via --mode)
  timeout_secs: 15
  response_limit: 1000
  type: "any"                            # any | album | single
  delay_secs: 5.0                        # Min gap between consecutive searches
  block_threshold: 5                     # Consecutive zero-result searches → check blocking
  block_pause_secs: 300                  # Pause if Soulseek blocking detected
  manual:                                # Used when mode=manual (or --artist/--album)
    artist: ""
    album: ""
  batch:
    file_path: ""                        # Newline-separated artist/album list

filters:
  allowed_extensions: [flac]             # Only consider these formats
  min_bitrate: null                      # kbps (null = no minimum)
  min_bitdepth: null                     # bits (null = no minimum)
  exclude_words: []                      # Exclude filenames containing these
  include_locked: false                  # Include locked/private files

download:
  concurrent: 5                          # Max simultaneous downloads
  max_queue_length: 0                    # 0 = free-slot only; >0 tolerate queues up to N
  max_start_time_secs: 120               # Max wait at queue front
  max_queue_time_secs: 1800              # Max total queue wait (0 = disabled)
  min_upload_speed_kbps: 250             # Cancel below this speed (0 = disabled)
  speed_check_wait_secs: 30              # Seconds before measuring speed
  timeout_secs: 180                      # Inactivity timeout
  browse_timeout_secs: 60                # Max browse wait (0 = disabled)
  max_download_time_mins: 120            # Hard wallclock ceiling per album
  max_retries: 4                         # Per-file retry attempts
  retry_delay_secs: 30                   # Wait between retries
  min_filtered_users: 10                 # Min candidates for speed check
  skip_retry_hours: 24                   # Cooldown before re-attempting transient failures

database:
  path: "db"                             # Directory for SQLite DB (overridden by --db-path)
  browse_cache_ttl_days: 7               # Cache browsed directories (0 = disabled)

logging:
  level: "INFO"                          # DEBUG | INFO | WARN | ERROR
  path: "logs"                           # Directory (overridden by --log-path)
  file: "seakarr.log"                    # Filename

pid:
  path: "pids"                           # Directory (overridden by --pid-path)
  file: "seakarr.pid"                    # Filename

notifications:
  urls: []                               # Apprise webhook URLs
  # e.g. ["ntfy://mytopic", "discord://webhook_id/token"]

daemon:
  enabled: false                         # Run continuously (--daemon also enables)
  rescan_interval_mins: 60               # Re-scan interval in daemon mode
```

---

## 3. CLI Surface (clap derive)

16 flags. The original 11 from the user's spec plus 5 for mode dispatch (--mode, --batch-file, --artist,
--album, --daemon).

```
seakarr [OPTIONS]

Config & paths:
  --config-path <dir>       Dir containing seakarr.yml [default: configs]
  --log-path <dir>          Override logging.path
  --log-level <level>       Override logging.level (DEBUG|INFO|WARN|ERROR)
  --db-path <dir>           Override database.path
  --pid-path <dir>          Override pid.path

Library override:
  --library-path <paths>    Comma-separated paths, overrides library.paths

Soulseek auth (overrides config):
  --soulseek-user <user>    Soulseek username
  --soulseek-password <pw>  Soulseek password

Mode selection (overrides config search.default_mode):
  --mode <mode>             auto | manual | batch
  --batch-file <path>       Text file (newline-separated artist/album lines)
  --artist <name>           Artist for manual mode
  --album <name>            Album for manual mode (optional)

Operational:
  --test                    Validate config and exit
  --daemon                  Run continuously with periodic re-scan
  --version                 Print version and exit
  --help                    Show this message
```

**Mode resolution:**
1. CLI `--mode` takes precedence over config `search.default_mode`
2. Manual mode: requires `--artist` (or `search.manual.artist` in config)
3. Batch mode: requires `--batch-file` (or `search.batch.file_path`)
4. Auto mode: requires `library.paths` to be non-empty

**`--test` validation:** Parse + merge config, verify required fields populated, check library paths exist
(warn if missing), open/migrate DB, exit with report.

---

## 4. Data Flow

### Mode 1: Automatic (library scan → upgrade)

```
scanner::scan_library(paths, exts)
  → walkdir + lofty → [(artist, album, bitrate, format, bitdepth, track_count)]
  → filter::find_albums_to_upgrade()
    → If ANY track below min_bitrate/min_bitdepth OR not in allowed_extensions
    → flag whole album for replacement
  → db::get_processed_albums() → skip already-done albums
  → runner::process_album(artist, album) ← tokio Semaphore(concurrent=N)
      ├─ search::search_album()  → client.search() → deduplicate
      ├─ filter::rank_candidates()
      │    └─ Exclude locked, bad extensions, excluded words, slow users
      │    └─ Score: speed × slot_bonus × bitrate_bonus → sort
      ├─ download::download_album()  → client.download() → monitor status channel
      │    ├─ Speed check after speed_check_wait_secs → cancel if < min_upload_speed
      │    ├─ Queue timeout check → cancel if queue_position > acceptable
      │    ├─ Download timeout → cancel if no progress for N seconds
      │    └─ Retry up to max_retries, then try next candidate
      ├─ organizer::organize() → move from staging to library (if storage.organize)
      ├─ db::mark_album_processed(), db::update_peer_reputation()
      └─ notifier::notify() → POST to Apprise URLs
```

### Mode 2: Manual (single search)

```
login → search(artist, album?) → rank candidates → download best → organize → exit
```

### Mode 3: Batch (text file)

```
parse file → for each line:
  search → rank → download → organize
  record per-line status (success/failed)
  continue to next line regardless of failure
→ print summary: "X succeeded, Y failed, Z skipped"
```

### Daemon mode

```
acquire PID → run first cycle (full scan) → loop:
  sleep(rescan_interval_mins)
  incremental scan: walk library dirs, compare dir mtime vs last scan time.
    Only re-scan directories modified since the previous cycle.
  process new/changed upgrade targets (skip already-processed albums)
→ on SIGTERM/SIGINT: finish current downloads, save queue, release PID
```

**Graceful shutdown:** tokio signal handler catches SIGTERM/SIGINT → sets cancellation token. Current
downloads complete; pending items saved to download_queue table. PID file removed. DB closed cleanly.

---

## 5. Database Schema (SQLite via rusqlite)

Single file in `database.path` directory. Schema versioned via `user_version` pragma for migrations.

**Processed album tracking:**
```sql
CREATE TABLE processed_albums (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    artist      TEXT NOT NULL,
    album       TEXT NOT NULL,
    status      TEXT NOT NULL DEFAULT 'pending',  -- pending|success|failed|skipped
    attempts    INTEGER NOT NULL DEFAULT 0,
    last_error  TEXT,
    first_seen  TEXT NOT NULL DEFAULT (datetime('now')),
    last_tried  TEXT,
    UNIQUE(artist, album)
);
```

**Download queue (persists across restarts):**
```sql
CREATE TABLE download_queue (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    artist      TEXT NOT NULL,
    album       TEXT,
    filename    TEXT NOT NULL,
    username    TEXT NOT NULL,
    size_bytes  INTEGER NOT NULL,
    bitrate     INTEGER,
    format      TEXT,
    status      TEXT NOT NULL DEFAULT 'queued',
    progress    REAL NOT NULL DEFAULT 0.0,
    local_path  TEXT,
    created_at  TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at  TEXT NOT NULL DEFAULT (datetime('now'))
);
```

**Peer reputation:**
```sql
CREATE TABLE peer_reputation (
    username        TEXT PRIMARY KEY,
    total_downloads INTEGER NOT NULL DEFAULT 0,
    successful      INTEGER NOT NULL DEFAULT 0,
    avg_speed_kbps  REAL NOT NULL DEFAULT 0.0,
    last_seen       TEXT NOT NULL DEFAULT (datetime('now')),
    preferred       INTEGER NOT NULL DEFAULT 0
);
```

**Stats and history:**
```sql
CREATE TABLE search_history (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    artist       TEXT NOT NULL,
    album        TEXT,
    result_count INTEGER NOT NULL DEFAULT 0,
    duration_ms  INTEGER,
    searched_at  TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE download_stats (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    artist         TEXT NOT NULL,
    album          TEXT NOT NULL,
    username       TEXT NOT NULL,
    filename       TEXT NOT NULL,
    size_bytes     INTEGER NOT NULL,
    bitrate        INTEGER,
    format         TEXT,
    speed_kbps     REAL,
    duration_secs  REAL,
    retries        INTEGER NOT NULL DEFAULT 0,
    status         TEXT NOT NULL,
    downloaded_at  TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE browse_cache (
    username   TEXT NOT NULL,
    path       TEXT NOT NULL,
    data_json  TEXT NOT NULL,
    cached_at  TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (username, path)
);
```

**Batch job tracking:**
```sql
CREATE TABLE batch_jobs (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    file_path   TEXT NOT NULL,
    total_lines INTEGER NOT NULL DEFAULT 0,
    completed   INTEGER NOT NULL DEFAULT 0,
    failed      INTEGER NOT NULL DEFAULT 0,
    status      TEXT NOT NULL DEFAULT 'running',
    created_at  TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE batch_job_lines (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    job_id       INTEGER NOT NULL REFERENCES batch_jobs(id),
    line_number  INTEGER NOT NULL,
    artist       TEXT NOT NULL,
    album        TEXT,
    status       TEXT NOT NULL DEFAULT 'pending',
    error        TEXT,
    processed_at TEXT
);
```

**Table purposes:**
- `processed_albums`: per-album lifecycle — tracks whether an album has been attempted/succeeded so auto mode skips it on subsequent runs (unless `skip_retry_hours` cooldown has expired on a failed entry).
- `download_stats`: per-transfer metrics — detailed record of every actual download for speed/reliability analysis.

**Cleanup:** `browse_cache` rows past TTL deleted on startup. `search_history` older than 90 days pruned.

---

## 6. Error Handling

| Layer | Failure | Behavior |
|-------|---------|----------|
| Config | Missing file, invalid YAML, bad values | Missing → create default, exit. Parse error → show line+message, code 1. |
| Soulseek login | Auth failure, server unreachable | Backoff retry up to login_retries. Exhausted → exit code 2. |
| Soulseek disconnect | Mid-session connection drop | Auto-reconnect with backoff. Active downloads re-queued. |
| Search | No results, timeout | No results → log, mark skipped, continue. Timeout → use partial results. |
| Download | Transfer failed, slow peer, queue timeout | Try next ranked candidate. All exhausted → mark failed, continue to next album. |
| Scanner | Unreadable file, corrupt tags | Skip file, log warning with path. Never abort full scan for one bad file. |
| DB | Locked, corrupt, disk full | Open failure → exit code 3. Mid-session: log, continue without persistence. |
| Disk full | Staging or organize destination | Pre-flight: check space. Mid-download: cancel, clean partial file, mark failed. |
| PID | Existing PID, stale lock | Alive PID → exit 4. Stale → remove and continue. |
| Daemon | Worker task panic | tokio JoinHandle — catch, log, restart. Never crash the process. |

**Global invariants:**
- No `unwrap()`/`expect()` in production code — all Results handled explicitly
- All errors logged with context (artist, album, username, filename)
- Exit codes: 0 = success, 1 = config, 2 = auth, 3 = DB, 4 = PID lock, 5 = runtime

---

## 7. Testing Strategy

| Layer | Tool | Scope |
|-------|------|-------|
| Unit | `#[cfg(test)]` + rstest | Filter ranking/scoring, config parsing/merging, organizer pattern expansion, scanner album grouping logic. All pure-logic modules tested in isolation with mocked deps. |
| Integration | `#[cfg(test)]` + tempfile | DB CRUD with in-memory SQLite, config loading from real YAML files, scanner with small test audio fixtures, end-to-end pipeline with mocked SoulseekClient. |
| Mock Soulseek | `SoulseekClient` trait | All network calls go through this trait. Production: real soulseek-rs-lib wrapper. Tests: mock returning predefined search results and download status streams. |
| CLI smoke | assert_cmd / trycmd | `--help`, `--version`, `--test` validation, default config creation. |
| Manual | (Not automated) | Live Soulseek search/download, real daemon mode, actual Apprise notifications. |

**Trait for testability:**
```rust
#[async_trait]
pub trait SoulseekClient: Send + Sync {
    async fn login(&self, user: &str, pass: &str) -> Result<(), ClientError>;
    async fn search(&self, query: &str, timeout: Duration) -> Result<Vec<SearchResult>, ClientError>;
    async fn download(&self, file: &FileInfo, dir: &Path) -> Result<DownloadHandle, ClientError>;
    async fn browse_user(&self, username: &str) -> Result<Vec<DirectoryListing>, ClientError>;
    async fn user_info(&self, username: &str) -> Result<UserInfo, ClientError>;
}
```

**Coverage target:** 80%+ line coverage (`cargo-llvm-cov`). Primary focus: filter, config, organizer,
scanner — the pure-logic modules.

---

## 8. Dependencies (Cargo.toml)

| Crate | Purpose |
|-------|---------|
| `clap` (derive) | CLI argument parsing |
| `serde` + `serde_yaml` | YAML config serialization |
| `config` | Config file loading, layering, merging |
| `soulseek-rs-lib` | Soulseek protocol client |
| `tokio` (full) | Async runtime, semaphore, signals |
| `rusqlite` (bundled) | SQLite database |
| `lofty` | Audio tag reading (FLAC, MP3, AAC, OGG, Opus, WAV, WMA, APE, MPC, Speex) |
| `walkdir` | Recursive directory traversal |
| `reqwest` | HTTP client for Apprise notifications |
| `tracing` + `tracing-subscriber` | Structured logging |
| `rstest` (dev) | Parameterized test fixtures |
| `tempfile` (dev) | Temporary directories/files for tests |
| `assert_cmd` (dev) | CLI integration testing |
| `async-trait` | Async trait support for SoulseekClient |
