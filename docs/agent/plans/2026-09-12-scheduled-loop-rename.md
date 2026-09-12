# Scheduled Loop Rename Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use sub-agents (recommended)
> to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for
> tracking.

**Goal:** Replace misleading daemon terminology with an interval-driven
schedule contract while preserving behavior, migrating existing YAML safely,
and supporting `--daemon` as a warned hidden alias for one minor release.

**Architecture:** Make `schedule` canonical from configuration through runtime
dispatch. Extend the existing YAML reconciliation pass to merge legacy
`daemon` values into `schedule` before defaults are applied, while the CLI
keeps a separate hidden legacy field so it can warn only when `--daemon` was
actually used. Continue dispatching the same validated `ExecutionPlan`
immediately and then after each interval.

**Tech Stack:** Rust 2021, Clap 4 derive, serde/serde_yaml, Tokio, tracing,
tempfile, Cargo, rustfmt, Clippy, markdownlint, and pre-commit.

---

<!-- markdownlint-disable MD013 -->

## Planning decisions

### Scope

Keep this as one implementation plan. The configuration schema, CLI compatibility, mode validation, runtime loop, and README are coupled names for one existing behavior. Splitting them would leave intermediate releases with a CLI and YAML schema that disagree or with migration support that has no canonical destination.

Do not change search, runner, download, database, Soulseek, organization, notification, PID-lock, or execution-plan behavior. Do not add background process management, cron/calendar scheduling, fixed wall-clock cadence, or a delayed-first-run option.

### Repository and commit guard

The approved specification is committed at `823c346` on `main`:

- `docs/agent/specs/2026-09-12-scheduled-loop-rename-design.md`

Project rules require code review before code commits. Therefore implementation tasks end with **unstaged checkpoints**, not commits. Keep implementation changes unstaged and uncommitted until the chain's verification, review, QA, and finalising steps authorize a commit. Never use `git add .`.

### File map

| File | Action | Responsibility |
| --- | --- | --- |
| `src/config.rs` | Modify | Canonical `ScheduleConfig`, `Config.schedule`, `CliOverrides.schedule`, YAML section/key migration, defaults, validation, and migration tests. |
| `src/main.rs` | Modify | Public `--schedule`, hidden legacy `--daemon`, warning, canonical overrides, renamed schedule loop/cycle, logs, and immediate-first-cycle tests. |
| `src/mode.rs` | Modify | Scheduled-mode incompatibility checks and unit-test terminology. |
| `src/client.rs` | Modify comments only | Replace an active-code comment that describes reconnect behavior as daemon-specific. |
| `src/runner.rs` | Modify comments only | Replace active-code comments that refer to daemon scan cycles. |
| `tests/schedule_cli_test.rs` | Create | Binary-level help, canonical flag, legacy warning, and dual-flag compatibility tests. |
| `tests/mode_resolution_test.rs` | Modify | Prove canonical and legacy scheduled invocations reject `--ignore-processed` before login using new wording. |
| `README.md` | Modify | Public schedule terminology, examples, options, YAML reference, behavior, and migration note. |
| `docs/agent/specs/2026-09-12-scheduled-loop-rename-design.md` | Read only | Approved behavior and compatibility contract. |
| `docs/specs/**`, `docs/plans/**`, older `docs/agent/**` | Preserve | Historical records retain the terminology used when written. |

No dependency, lockfile, database schema, or new runtime module is required.

## Task 1: Make the configuration schema canonical and migrate legacy YAML

**Files:**

- Modify: `src/config.rs:35-55`
- Modify: `src/config.rs:210-225`
- Modify: `src/config.rs:270-292`
- Modify: `src/config.rs:465-475`
- Modify: `src/config.rs:620-710`
- Modify: `src/config.rs:718-860`
- Modify: `src/config.rs:888-940`
- Modify: `src/config.rs:1000-1140`
- Modify: `src/config.rs:1190-2250`
- Modify mechanically for compilation: `src/main.rs`
- Modify mechanically for compilation: `src/mode.rs`

- [ ] **Step 1: Add RED tests for the canonical default schema**

In the `src/config.rs` test module, replace daemon-specific fixture fields with the future canonical names and add this test. It should not compile until `Config.schedule` and `ScheduleConfig` exist.

```rust
#[test]
fn default_config_serializes_only_schedule_names() {
    let value = serde_yaml::to_value(Config::default()).unwrap();

    assert!(value.get("schedule").is_some());
    assert!(value.get("daemon").is_none());
    assert_eq!(value["schedule"]["enabled"].as_bool(), Some(false));
    assert_eq!(value["schedule"]["interval_mins"].as_u64(), Some(60));
    assert!(value["schedule"].get("rescan_interval_mins").is_none());
}
```

Update `sample_yaml()` to end with the canonical section:

```yaml
schedule:
  enabled: false
  interval_mins: 60
```

- [ ] **Step 2: Run the schema test to verify RED**

Run:

```bash
cargo test --lib config::tests::default_config_serializes_only_schedule_names -- --exact
```

Expected: compilation fails because `Config` has no `schedule` field yet.

- [ ] **Step 3: Add RED migration tests for old-only, mixed, and idempotent YAML**

Add these helpers and tests to `src/config.rs`'s existing test module. Use `Config::load` so the tests cover migration, backup, re-read, and deserialization together.

```rust
fn write_config(dir: &TempDir, yaml: &str) -> PathBuf {
    let path = dir.path().join("seakarr.yml");
    fs::write(&path, yaml).unwrap();
    path
}

#[test]
fn load_migrates_daemon_section_and_interval() {
    let dir = TempDir::new().unwrap();
    let path = write_config(
        &dir,
        r#"
soulseek:
  username: test
  password: test
daemon:
  enabled: true
  rescan_interval_mins: 17
"#,
    );

    let config = Config::load(dir.path()).unwrap();
    assert!(config.schedule.enabled);
    assert_eq!(config.schedule.interval_mins, 17);

    let migrated = fs::read_to_string(&path).unwrap();
    let value: serde_yaml::Value = serde_yaml::from_str(&migrated).unwrap();
    assert!(value.get("daemon").is_none());
    assert!(value["schedule"].get("rescan_interval_mins").is_none());
    assert_eq!(value["schedule"]["enabled"].as_bool(), Some(true));
    assert_eq!(value["schedule"]["interval_mins"].as_u64(), Some(17));
    assert!(dir.path().join("seakarr.yml.bak").exists());
}

#[test]
fn load_mixed_schedule_values_win_and_inherit_missing_legacy_values() {
    let dir = TempDir::new().unwrap();
    let path = write_config(
        &dir,
        r#"
soulseek:
  username: test
  password: test
daemon:
  enabled: true
  rescan_interval_mins: 17
schedule:
  enabled: false
"#,
    );

    let config = Config::load(dir.path()).unwrap();
    assert!(!config.schedule.enabled);
    assert_eq!(config.schedule.interval_mins, 17);

    let value: serde_yaml::Value =
        serde_yaml::from_str(&fs::read_to_string(path).unwrap()).unwrap();
    assert!(value.get("daemon").is_none());
    assert_eq!(value["schedule"]["enabled"].as_bool(), Some(false));
    assert_eq!(value["schedule"]["interval_mins"].as_u64(), Some(17));
}

#[test]
fn load_current_schedule_is_idempotent() {
    let dir = TempDir::new().unwrap();
    let path = write_config(
        &dir,
        r#"
soulseek:
  username: test
  password: test
schedule:
  enabled: true
  interval_mins: 23
"#,
    );

    Config::load(dir.path()).unwrap();
    let once = fs::read_to_string(&path).unwrap();
    Config::load(dir.path()).unwrap();
    let twice = fs::read_to_string(&path).unwrap();

    assert_eq!(once, twice);
    assert_eq!(
        std::fs::read_dir(dir.path()).unwrap().count(),
        2,
        "only seakarr.yml and its single backup should exist"
    );
}
```

- [ ] **Step 4: Run migration tests to verify RED**

Run:

```bash
cargo test --lib config::tests::load_migrates_daemon_section_and_interval -- --exact
cargo test --lib config::tests::load_mixed_schedule_values_win_and_inherit_missing_legacy_values -- --exact
cargo test --lib config::tests::load_current_schedule_is_idempotent -- --exact
```

Expected: compilation fails on missing canonical schedule fields and migration behavior.

- [ ] **Step 5: Rename the configuration types and fields**

Use these canonical definitions in `src/config.rs`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduleConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_schedule_interval")]
    pub interval_mins: u64,
}

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
    pub schedule: bool,
    pub test: bool,
    /// Runtime-only: bypass the processed-album success check for this run.
    pub ignore_processed: bool,
}
```

Rename `Config.daemon` to `Config.schedule`, `DaemonConfig` to `ScheduleConfig`, and its `Default` implementation accordingly:

```rust
impl Default for ScheduleConfig {
    fn default() -> Self {
        Config::default().schedule
    }
}
```

Use the canonical default value:

```rust
schedule: ScheduleConfig {
    enabled: false,
    interval_mins: default_schedule_interval(),
},
```

Rename `default_rescan_interval` to `default_schedule_interval` and use that name in both the serde attribute and `Config::default` so active identifiers contain no stale rescan terminology:

```rust
fn default_schedule_interval() -> u64 {
    60
}
```

- [ ] **Step 6: Add a focused section migration helper**

Place this helper beside `migrate_rename`. It canonicalizes the interval key in both old and new sections, lets meaningful new values win, inherits missing/null new values from the legacy section, and removes `daemon`.

```rust
fn rename_mapping_key(
    mapping: &mut serde_yaml::Mapping,
    old_key: &str,
    new_key: &str,
) -> bool {
    let old_key = serde_yaml::Value::String(old_key.into());
    let new_key = serde_yaml::Value::String(new_key.into());
    let Some(old_value) = mapping.remove(&old_key) else {
        return false;
    };

    if !old_value.is_null()
        && mapping
            .get(&new_key)
            .map_or(true, serde_yaml::Value::is_null)
    {
        mapping.insert(new_key, old_value);
    }
    true
}

fn migrate_schedule_section(config: &mut serde_yaml::Value) -> bool {
    let serde_yaml::Value::Mapping(root) = config else {
        return false;
    };
    let daemon_key = serde_yaml::Value::String("daemon".into());
    let schedule_key = serde_yaml::Value::String("schedule".into());
    let mut legacy = root.remove(&daemon_key);
    let had_legacy = legacy.is_some();

    if let Some(serde_yaml::Value::Mapping(mapping)) = legacy.as_mut() {
        rename_mapping_key(mapping, "rescan_interval_mins", "interval_mins");
    }

    match (root.get_mut(&schedule_key), legacy) {
        (Some(serde_yaml::Value::Mapping(current)), Some(serde_yaml::Value::Mapping(legacy))) => {
            let renamed_interval =
                rename_mapping_key(current, "rescan_interval_mins", "interval_mins");
            for (key, value) in legacy {
                if current
                    .get(&key)
                    .map_or(true, serde_yaml::Value::is_null)
                {
                    current.insert(key, value);
                }
            }
            had_legacy || renamed_interval
        }
        (Some(serde_yaml::Value::Mapping(current)), _) => {
            rename_mapping_key(current, "rescan_interval_mins", "interval_mins") || had_legacy
        }
        (Some(current), Some(legacy)) if current.is_null() && !legacy.is_null() => {
            *current = legacy;
            true
        }
        (Some(_), _) => had_legacy,
        (None, Some(legacy)) if !legacy.is_null() => {
            root.insert(schedule_key, legacy);
            true
        }
        (None, Some(_)) => true,
        (None, None) => false,
    }
}
```

Call it first in `Config::reconcile_config_file`, using non-short-circuit OR so all migrations run:

```rust
let renamed = migrate_schedule_section(&mut file_value)
    | migrate_rename(&mut file_value, "filters", "min_bitrate", "min_bit_rate")
    | migrate_rename(&mut file_value, "filters", "min_bitdepth", "min_bit_depth")
    | migrate_rename(
        &mut file_value,
        "search",
        "prefer_reliable_peer",
        "peer_reputation",
    );
```

- [ ] **Step 7: Update canonical merge and validation paths**

Change CLI merging and interval validation to:

```rust
if cli.schedule {
    self.schedule.enabled = true;
}
```

```rust
if self.schedule.interval_mins > u64::MAX / 60 {
    return Err(SeakarrError::Config(
        "schedule.interval_mins is too large; maximum is 307445734561271883".into(),
    ));
}
```

Rename the existing overflow test to `test_validate_rejects_unrepresentable_schedule_interval` and assert the canonical key path.

- [ ] **Step 8: Make mechanical consumer renames required for compilation**

In `src/main.rs` and `src/mode.rs`, rename only the data-model references required by Task 1:

```rust
// src/main.rs: preserve the old public flag until Task 2.
schedule: cli.daemon,
```

```rust
// src/main.rs
if config.schedule.enabled {
    let interval_mins = config.schedule.interval_mins.max(1);
}
```

```rust
// src/mode.rs
if cli.ignore_processed && (cli.schedule || config.schedule.enabled) {
    return Err(SeakarrError::Config(
        "--ignore-processed cannot be used with scheduled mode".into(),
    ));
}
```

Update all `CliOverrides` test literals to use `schedule`, and all config assertions to use `config.schedule`. Do not rename the CLI flag or runtime functions in this task.

- [ ] **Step 9: Run the configuration and mode tests to verify GREEN**

Run:

```bash
cargo test --lib config::tests -- --nocapture
cargo test --lib mode::tests -- --nocapture
```

Expected: all configuration and mode unit tests pass, including the new migration cases.

- [ ] **Step 10: Record an unstaged checkpoint**

Run:

```bash
cargo fmt --all
cargo fmt --all -- --check
git diff --check
git status --short
```

Expected: only the planned Rust files and this plan are modified/untracked; no staged changes exist.

## Task 2: Add the canonical CLI and bounded legacy compatibility

**Files:**

- Create: `tests/schedule_cli_test.rs`
- Modify: `src/main.rs:70-85`
- Modify: `src/main.rs:130-160`

- [ ] **Step 1: Write RED binary-level CLI tests**

Create `tests/schedule_cli_test.rs` with complete process helpers and the four compatibility cases:

```rust
use std::process::{Command, Output};
use tempfile::TempDir;

fn run_test_mode(flags: &[&str]) -> Output {
    let temp = TempDir::new().unwrap();
    let config_dir = temp.path().join("config");
    let log_dir = temp.path().join("logs");
    Command::new(env!("CARGO_BIN_EXE_seakarr"))
        .args([
            "--config-path",
            config_dir.to_str().unwrap(),
            "--log-path",
            log_dir.to_str().unwrap(),
        ])
        .args(flags)
        .arg("--test")
        .output()
        .expect("failed to start seakarr")
}

fn combined(output: &Output) -> String {
    format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn help_advertises_schedule_and_hides_daemon() {
    let output = Command::new(env!("CARGO_BIN_EXE_seakarr"))
        .arg("--help")
        .output()
        .expect("failed to start seakarr");
    let text = combined(&output);

    assert!(output.status.success(), "got:\n{text}");
    assert!(text.contains("--schedule"), "got:\n{text}");
    assert!(!text.contains("--daemon"), "got:\n{text}");
}

#[test]
fn schedule_is_accepted_without_deprecation_warning() {
    let output = run_test_mode(&["--schedule"]);
    let text = combined(&output);

    assert!(output.status.success(), "got:\n{text}");
    assert!(text.contains("Configuration is valid."), "got:\n{text}");
    assert!(!text.contains("deprecated"), "got:\n{text}");
}

#[test]
fn daemon_is_hidden_but_accepted_with_warning() {
    let output = run_test_mode(&["--daemon"]);
    let text = combined(&output);

    assert!(output.status.success(), "got:\n{text}");
    assert!(text.contains("Configuration is valid."), "got:\n{text}");
    assert!(
        text.contains("--daemon is deprecated; use --schedule"),
        "got:\n{text}"
    );
    assert!(text.contains("next minor release"), "got:\n{text}");
}

#[test]
fn both_schedule_flags_enable_once_and_warn_once() {
    let output = run_test_mode(&["--schedule", "--daemon"]);
    let text = combined(&output);

    assert!(output.status.success(), "got:\n{text}");
    assert_eq!(text.matches("--daemon is deprecated").count(), 1, "got:\n{text}");
}
```

- [ ] **Step 2: Run CLI tests to verify RED**

Run:

```bash
cargo test --test schedule_cli_test -- --nocapture
```

Expected: FAIL because `--schedule` is unknown and `--daemon` is still shown without a warning.

- [ ] **Step 3: Implement canonical and hidden Clap fields**

Replace the existing daemon field in `Cli` with distinct canonical and legacy fields so runtime code can detect which spelling was used:

```rust
/// Repeat the selected operation in a foreground interval loop
#[arg(long)]
schedule: bool,

/// Deprecated compatibility flag; use --schedule
#[arg(long, hide = true)]
daemon: bool,
```

Do not use a Clap alias on `schedule`: an alias would lose the information needed to warn only for legacy use.

- [ ] **Step 4: Emit one early legacy warning and construct the effective override**

Immediately after the second `Cli::parse()` in `run()`, before configuration loading can fail, add:

```rust
if cli.daemon {
    eprintln!(
        "warning: --daemon is deprecated; use --schedule; \
         support will be removed in the next minor release"
    );
}
```

Construct the canonical override once:

```rust
schedule: cli.schedule || cli.daemon,
```

The first parse in `main()` remains warning-free; this guarantees one warning rather than two.

- [ ] **Step 5: Run CLI compatibility tests to verify GREEN**

Run:

```bash
cargo test --test schedule_cli_test -- --nocapture
```

Expected: all four tests pass; normal help contains `--schedule`, legacy help is hidden, and legacy use emits exactly one warning.

- [ ] **Step 6: Record an unstaged checkpoint**

Run:

```bash
cargo fmt --all
cargo fmt --all -- --check
git diff --check
git status --short
```

Expected: `src/main.rs` and `tests/schedule_cli_test.rs` are included in the unstaged change set; nothing is staged.

## Task 3: Rename runtime scheduling and preserve execution behavior

**Files:**

- Modify: `src/main.rs:225-255`
- Modify: `src/main.rs:395-485`
- Modify: `src/main.rs:590-650`
- Modify: `src/mode.rs:90-105`
- Modify: `src/mode.rs:710-745`
- Modify: `tests/mode_resolution_test.rs:297-340`

- [ ] **Step 1: Rename and tighten scheduled-mode unit tests before production identifiers**

Rename the two mode tests and use canonical fields/messages:

```rust
#[test]
fn ignore_processed_is_rejected_for_scheduled_cli_mode() {
    let config = config_with_mode("auto");
    let mut overrides = cli(None, None, None, None);
    overrides.schedule = true;
    overrides.ignore_processed = true;

    assert_config_error(&config, &overrides, "cannot be used with scheduled mode");
}

#[test]
fn ignore_processed_is_rejected_for_configured_schedule() {
    let mut config = config_with_mode("auto");
    config.schedule.enabled = true;
    let mut overrides = cli(None, None, None, None);
    overrides.ignore_processed = true;

    assert_config_error(&config, &overrides, "cannot be used with scheduled mode");
}
```

Rename the main-module regression test to `schedule_cycle_honours_manual_mode_artist_album` and change its call to the not-yet-defined `run_schedule_cycle`.

- [ ] **Step 2: Add a RED immediate-first-cycle test**

In `src/main.rs`'s test module, add a paused-time test using the same mock/config setup as `schedule_cycle_honours_manual_mode_artist_album`. The key assertion is that a search occurs without advancing virtual time:

```rust
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
    let started = tokio::time::Instant::now();
    let schedule = run_schedule(
        &client,
        &config,
        &db,
        &pid_file,
        tokio::time::Duration::from_secs(3_600),
        &plan,
    );
    tokio::pin!(schedule);

    for _ in 0..100 {
        tokio::select! {
            result = &mut schedule => panic!("schedule ended unexpectedly: {result:?}"),
            _ = tokio::task::yield_now() => {}
        }
        if !client.search_queries.lock().unwrap().is_empty() {
            break;
        }
    }

    assert!(
        !client.search_queries.lock().unwrap().is_empty(),
        "first cycle must dispatch before the interval elapses"
    );
    assert_eq!(tokio::time::Instant::now(), started);
}
```

- [ ] **Step 3: Run focused tests to verify RED**

Run:

```bash
cargo test --bin seakarr tests::schedule_cycle_honours_manual_mode_artist_album -- --exact
cargo test --bin seakarr tests::schedule_starts_first_cycle_before_waiting -- --exact
cargo test --lib mode::tests::ignore_processed_is_rejected_for_scheduled_cli_mode -- --exact
```

Expected: compilation fails because the runtime still uses `run_daemon`/`run_daemon_cycle` and old test names.

- [ ] **Step 4: Rename scheduler functions, fields, comments, and messages**

Use canonical interval handling in `run()`:

```rust
if config.schedule.enabled {
    let interval_mins = config.schedule.interval_mins.max(1);
    if config.schedule.interval_mins == 0 {
        tracing::warn!(
            "schedule.interval_mins is 0 - clamping to 1 to avoid busy-loop"
        );
    }
    let interval =
        tokio::time::Duration::from_secs(interval_mins.checked_mul(60).ok_or_else(|| {
            SeakarrError::Config(
                "schedule.interval_mins is too large; maximum is 307445734561271883".into(),
            )
        })?);
    run_schedule(
        &client,
        &config,
        &db,
        &pid_file,
        interval,
        &execution_plan,
    )
    .await
} else {
    let result =
        dispatch_execution_plan(&client, &execution_plan, &config, &db, cli.ignore_processed)
            .await;
    release_pid_lock(&pid_file)?;
    result
}
```

Rename and update the loop while preserving dispatch-before-wait ordering:

```rust
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

        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("Schedule: received SIGINT, shutting down...");
                release_pid_lock(pid_file)?;
                return Ok(());
            }
            _ = wait_for_sigterm(&mut sigterm) => {
                tracing::info!("Schedule: received SIGTERM, shutting down...");
                release_pid_lock(pid_file)?;
                return Ok(());
            }
            _ = tokio::time::sleep(interval) => {}
        }
    }
}

/// Run one scheduled cycle with the same validated plan used by one-shot execution.
async fn run_schedule_cycle(
    client: &dyn SoulseekClient,
    config: &Config,
    db: &Database,
    plan: &ExecutionPlan,
) -> Result<()> {
    dispatch_execution_plan(client, plan, config, db, false).await
}
```

Use ASCII hyphens in newly edited messages and comments. Update surrounding comments from daemon to schedule terminology.

- [ ] **Step 5: Update binary-level scheduled-mode validation tests**

Rename the existing integration test to `ignore_processed_with_schedule_fails_before_login` and invoke `--schedule`. Assert:

```rust
assert_eq!(output.status.code(), Some(1), "got:\n{combined}");
assert!(
    combined.contains("cannot be used with scheduled mode"),
    "error must explain the schedule conflict, got:\n{combined}"
);
assert!(
    !combined.contains("Connecting to Soulseek"),
    "scheduled-mode validation must fail before login:\n{combined}"
);
```

Add a second test using `--daemon` to prove the compatibility path reaches the same pre-login validation and also warns:

```rust
#[test]
fn ignore_processed_with_legacy_daemon_warns_and_fails_before_login() {
    let temp = TempDir::new().unwrap();
    let config_dir = temp.path().join("config");
    let log_dir = temp.path().join("logs");
    let output = Command::new(env!("CARGO_BIN_EXE_seakarr"))
        .args([
            "--config-path",
            config_dir.to_str().unwrap(),
            "--log-path",
            log_dir.to_str().unwrap(),
            "--ignore-processed",
            "--daemon",
            "--test",
        ])
        .output()
        .expect("failed to start seakarr");
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert_eq!(output.status.code(), Some(1), "got:\n{combined}");
    assert!(combined.contains("--daemon is deprecated; use --schedule"));
    assert!(combined.contains("cannot be used with scheduled mode"));
    assert!(!combined.contains("Connecting to Soulseek"));
}
```

- [ ] **Step 6: Run runtime and validation tests to verify GREEN**

Run:

```bash
cargo test --bin seakarr tests::schedule_cycle_honours_manual_mode_artist_album -- --exact
cargo test --bin seakarr tests::schedule_starts_first_cycle_before_waiting -- --exact
cargo test --lib mode::tests::ignore_processed_is_rejected_for_scheduled_cli_mode -- --exact
cargo test --lib mode::tests::ignore_processed_is_rejected_for_configured_schedule -- --exact
cargo test --test mode_resolution_test -- --nocapture
```

Expected: all focused tests pass; the immediate-first-cycle test does not advance virtual time.

- [ ] **Step 7: Record an unstaged checkpoint**

Run:

```bash
cargo fmt --all
cargo fmt --all -- --check
git diff --check
git status --short
```

Expected: only planned files are modified/untracked and nothing is staged.

## Task 4: Update active terminology and user documentation

**Files:**

- Modify comments only: `src/client.rs:1080`
- Modify comments only: `src/runner.rs:818`
- Modify: `README.md:1-420`

- [ ] **Step 1: Update remaining active-code comments**

Use schedule-oriented wording without changing behavior:

```rust
// Scheduled execution must recover from a transient outage without a restart.
```

```rust
// Repeated schedule cycles call run_auto_mode again.
```

Do not edit historical specification or plan files merely to remove old terminology.

- [ ] **Step 2: Replace the README public examples and option contract**

Use this quick-start example:

```bash
# Scheduled foreground loop (run immediately, then repeat every 60 min)
seakarr --schedule
```

Use this option-table row:

```markdown
| `--schedule` | Run immediately, then repeat the same validated auto, manual, or batch operation after each interval. | `false` |
```

Rename the configuration heading and table:

```markdown
### `schedule`

| Key | Description | Default |
| --- | ----------- | ------- |
| `enabled` | Run the selected operation in a foreground interval loop. Also enabled by `--schedule`. | `false` |
| `interval_mins` | Minutes to wait after a completed cycle before starting the next one. Values below `1` are clamped to `1`. | `60` |
```

Rename the behavior section to `### Scheduled mode` and state the actual cadence:

```markdown
### Scheduled mode

When `--schedule` or `schedule.enabled` is set, the same validated auto,
manual, or batch plan runs immediately in a foreground loop. After each cycle
completes, seakarr waits for `schedule.interval_mins` before dispatching that
unchanged plan again. SIGINT (Ctrl+C) and SIGTERM stop the loop gracefully and
remove the PID file after the current cycle finishes.
```

- [ ] **Step 3: Add the bounded migration note**

Place this note next to the CLI/config schedule documentation:

```markdown
#### Migrating from daemon terminology

`--daemon` remains as a hidden compatibility flag for this release. It behaves
like `--schedule`, prints a deprecation warning, and will be removed in the
next minor release. Existing `daemon.enabled` and
`daemon.rescan_interval_mins` values are migrated automatically to
`schedule.enabled` and `schedule.interval_mins`. Before rewriting the file,
seakarr saves the original as `seakarr.yml.bak`. Explicit values already under
`schedule` take precedence over legacy values.
```

Update the `--ignore-processed` documentation to say it cannot be combined with `--schedule` or configured scheduled mode. Do not claim that the legacy alias has already been removed.

- [ ] **Step 4: Check active terminology intentionally**

Run:

```bash
rg -n '\bdaemon\b|daemon_|Daemon' src tests README.md \
  --glob '!docs/**'
```

Expected: remaining hits are limited to the hidden `Cli.daemon` compatibility field, its warning, YAML migration literals/tests, and tests that deliberately invoke the legacy flag. There should be no daemon terminology in canonical types, runtime function names, normal logs, help text, or README headings.

- [ ] **Step 5: Lint documentation and record an unstaged checkpoint**

Run:

```bash
markdownlint --fix README.md docs/agent/plans/2026-09-12-scheduled-loop-rename.md
markdownlint README.md docs/agent/plans/2026-09-12-scheduled-loop-rename.md
git diff --check
git status --short
```

Expected: markdownlint and whitespace checks pass; no files are staged.

## Task 5: Run full regression and quality verification

**Files:**

- Verify all modified files only; do not add implementation in this task.

- [ ] **Step 1: Format and verify formatting**

Run:

```bash
cargo fmt --all
cargo fmt --all -- --check
```

Expected: both commands exit 0 with no formatting diff after the first command.

- [ ] **Step 2: Run the complete workspace test suite**

Run:

```bash
cargo test --workspace
```

Expected: all seakarr and vendored `soulseek-rs-lib` tests pass.

- [ ] **Step 3: Run Clippy across every target**

Run:

```bash
cargo clippy --workspace --all-targets -- -D warnings
```

Expected: exit 0 with no warnings. Do not make unrelated cleanup changes; report any pre-existing blocker to the chain.

- [ ] **Step 4: Run documentation and repository hooks**

Run:

```bash
markdownlint README.md \
  docs/agent/specs/2026-09-12-scheduled-loop-rename-design.md \
  docs/agent/plans/2026-09-12-scheduled-loop-rename.md
pre-commit run --all-files
git diff --check
```

Expected: all commands exit 0.

- [ ] **Step 5: Verify spec coverage and bounded legacy references**

Run:

```bash
rg -n '\bdaemon\b|daemon_|Daemon' src tests README.md --glob '!docs/**'
git diff --stat
git status --short
```

Expected:

- all 13 acceptance criteria in the approved spec are covered by focused or full-suite tests;
- remaining daemon hits are compatibility/migration-only;
- no dependency, lockfile, database, runner behavior, or vendor source changes appear;
- only files in the plan's file map are modified/untracked; and
- nothing is staged or committed.

- [ ] **Step 6: Hand off to review without committing**

Report the verification commands and outcomes to the chain. The next chain steps own technical-debt review, two-reviewer code review, QA, and finalising. Do not commit, push, release, or start another implementation task from this plan.
