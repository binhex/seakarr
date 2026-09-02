# CLI Mode Resolution Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use sub-agents (recommended) to
> implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for
> tracking.

**Goal:** Prevent artist, album, and batch CLI selectors from silently entering
an incompatible mode, and validate the selected mode before startup side effects.

**Architecture:** Add a pure resolver in a dedicated `src/mode.rs` unit. It
returns an `ExecutionPlan` for auto, manual, or batch execution and is called
before CLI overrides are merged, so the same plan can be used by one-shot runs,
daemon cycles, and `--test`.

**Tech Stack:** Rust stable, Cargo, clap derive, Tokio, `thiserror`, existing
`Config`/`CliOverrides` types, `MockClient`, `tempfile`, README Markdown, and
repository pre-commit hooks.

---

<!-- markdownlint-disable MD013 -->

## Scope check

The approved specification covers one cohesive subsystem: selecting and validating
the operation mode before dispatch. Startup, daemon cycles, manual runner input, and
documentation are coupled consumers of that one decision and must share the same plan;
splitting them into independent plans would recreate the current risk of divergent
mode behavior. This is therefore one implementation plan, not multiple independent
subsystem plans.

## File map

### Create

- `src/mode.rs` — Pure `SearchMode`, `ExecutionPlan`, and
  `resolve_execution_plan` implementation with unit tests. It owns mode precedence,
  selector conflicts, target fallback, and configuration errors.
- `tests/mode_resolution_test.rs` — Process-level regression proving the reported
  invocation fails before connection or library scanning.

### Modify

- `src/lib.rs` — Export the new `mode` module for the binary and integration tests.
- `src/main.rs` — Resolve the plan immediately after config loading, dispatch
  one-shot and daemon runs from it, and update daemon/startup tests.
- `src/runner.rs` — Let `run_manual_mode` accept an optional artist so album-only
  manual plans reach the existing album-only search support.
- `README.md` — Document explicit mode selection, selector conflicts, album-only
  manual searches, and daemon consistency.

### Do not modify

- `src/config.rs` — `Config`, `CliOverrides`, and `merge_cli` already contain all
  required fields and are retained.
- `src/search.rs` — `search_album` and `search_album_with_fallback` already support
  an empty artist for album-only searches.
- Configuration instance files such as `configs/seakarr.yml`, generated logs,
  databases, credentials, and the vendored Soulseek library.

## Shared interface contract

All implementation tasks use these exact public names and shapes:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchMode {
    Auto,
    Manual,
    Batch,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecutionPlan {
    Auto,
    Manual {
        artist: Option<String>,
        album: Option<String>,
    },
    Batch {
        file_path: String,
    },
}

impl ExecutionPlan {
    pub fn mode(&self) -> SearchMode;
}

pub fn resolve_execution_plan(
    config: &Config,
    cli: &CliOverrides,
) -> Result<ExecutionPlan>;
```

The resolver treats a supplied CLI `--mode` as the mode source; otherwise it uses
`config.search.default_mode`. CLI `--artist` and `--album` form the manual selector
group, while CLI `--batch-file` forms the batch selector. Manual and batch CLI
selectors conflict. CLI target values override selected-mode config values;
whitespace-only values are absent. Inactive config sections are ignored.

## Task 1: Specify the resolver with failing unit tests

**Files:**

- Create: `src/mode.rs`
- Modify: `src/lib.rs:3-18`

- [ ] **Step 1: Add the module declaration and write the failing resolver tests**

Add the module declaration to `src/lib.rs`:

```rust
pub mod mode;
```

Create `src/mode.rs` with this test module. It deliberately references the
not-yet-defined resolver types so the first run is the RED phase.

```rust
#[cfg(test)]
mod tests {
    use super::{resolve_execution_plan, ExecutionPlan, SearchMode};
    use crate::config::{CliOverrides, Config};
    use crate::error::SeakarrError;

    fn config_with_mode(mode: &str) -> Config {
        let mut config = Config::default();
        config.search.default_mode = mode.to_owned();
        config
    }

    fn cli(
        mode: Option<&str>,
        artist: Option<&str>,
        album: Option<&str>,
        batch_file: Option<&str>,
    ) -> CliOverrides {
        CliOverrides {
            mode: mode.map(str::to_owned),
            artist: artist.map(str::to_owned),
            album: album.map(str::to_owned),
            batch_file: batch_file.map(str::to_owned),
            ..CliOverrides::default()
        }
    }

    fn assert_config_error(config: &Config, overrides: &CliOverrides, text: &str) {
        let error = resolve_execution_plan(config, overrides)
            .expect_err("expected a config error");
        assert!(
            matches!(&error, &SeakarrError::Config(_)),
            "expected SeakarrError::Config, got {error:?}"
        );
        assert!(
            error.to_string().contains(text),
            "expected {text:?} in {error:?}"
        );
    }

    #[test]
    fn configured_auto_without_selectors_returns_auto() {
        let config = config_with_mode("auto");
        let plan = resolve_execution_plan(&config, &cli(None, None, None, None)).unwrap();
        assert_eq!(plan, ExecutionPlan::Auto);
        assert_eq!(plan.mode(), SearchMode::Auto);
    }

    #[test]
    fn configured_auto_rejects_manual_cli_selectors() {
        let config = config_with_mode("auto");
        assert_config_error(
            &config,
            &cli(None, None, Some("Album"), None),
            "--mode manual",
        );
    }

    #[test]
    fn configured_auto_rejects_batch_cli_selector() {
        let config = config_with_mode("auto");
        assert_config_error(
            &config,
            &cli(None, None, None, Some("wantlist.txt")),
            "--mode batch",
        );
    }

    #[test]
    fn explicit_manual_mode_overrides_configured_auto() {
        let config = config_with_mode("auto");
        let plan = resolve_execution_plan(
            &config,
            &cli(Some("manual"), Some("Artist"), Some("Album"), None),
        )
        .unwrap();
        assert_eq!(
            plan,
            ExecutionPlan::Manual {
                artist: Some("Artist".into()),
                album: Some("Album".into()),
            }
        );
    }

    #[test]
    fn manual_mode_accepts_artist_only() {
        let config = config_with_mode("manual");
        let plan = resolve_execution_plan(&config, &cli(None, Some("Artist"), None, None)).unwrap();
        assert_eq!(
            plan,
            ExecutionPlan::Manual {
                artist: Some("Artist".into()),
                album: None,
            }
        );
    }

    #[test]
    fn manual_mode_accepts_album_only() {
        let config = config_with_mode("manual");
        let plan = resolve_execution_plan(&config, &cli(None, None, Some("Album"), None)).unwrap();
        assert_eq!(
            plan,
            ExecutionPlan::Manual {
                artist: None,
                album: Some("Album".into()),
            }
        );
    }

    #[test]
    fn manual_mode_uses_cli_values_before_config_values() {
        let mut config = config_with_mode("manual");
        config.search.manual.artist = "Configured Artist".into();
        config.search.manual.album = "Configured Album".into();
        let plan = resolve_execution_plan(
            &config,
            &cli(None, Some("CLI Artist"), None, None),
        )
        .unwrap();
        assert_eq!(
            plan,
            ExecutionPlan::Manual {
                artist: Some("CLI Artist".into()),
                album: Some("Configured Album".into()),
            }
        );
    }

    #[test]
    fn manual_mode_requires_at_least_one_target() {
        let config = config_with_mode("manual");
        assert_config_error(
            &config,
            &cli(None, None, None, None),
            "at least one non-empty target",
        );
    }

    #[test]
    fn manual_mode_rejects_batch_file() {
        let config = config_with_mode("manual");
        assert_config_error(
            &config,
            &cli(None, None, None, Some("wantlist.txt")),
            "incompatible with manual mode",
        );
    }

    #[test]
    fn explicit_auto_rejects_manual_selector() {
        let config = config_with_mode("manual");
        assert_config_error(
            &config,
            &cli(Some("auto"), Some("Artist"), None, None),
            "--mode manual",
        );
    }

    #[test]
    fn explicit_auto_rejects_batch_selector() {
        let config = config_with_mode("manual");
        assert_config_error(
            &config,
            &cli(Some("auto"), None, None, Some("wantlist.txt")),
            "--mode batch",
        );
    }

    #[test]
    fn explicit_manual_rejects_batch_selector() {
        let config = config_with_mode("auto");
        assert_config_error(
            &config,
            &cli(Some("manual"), None, None, Some("wantlist.txt")),
            "incompatible with manual mode",
        );
    }

    #[test]
    fn explicit_batch_rejects_manual_selector() {
        let config = config_with_mode("auto");
        assert_config_error(
            &config,
            &cli(Some("batch"), Some("Artist"), None, None),
            "incompatible with batch mode",
        );
    }

    #[test]
    fn batch_mode_uses_cli_path_before_config_path() {
        let mut config = config_with_mode("batch");
        config.search.batch.file_path = "configured.txt".into();
        let plan = resolve_execution_plan(
            &config,
            &cli(None, None, None, Some("cli.txt")),
        )
        .unwrap();
        assert_eq!(
            plan,
            ExecutionPlan::Batch {
                file_path: "cli.txt".into(),
            }
        );
    }

    #[test]
    fn batch_mode_uses_config_path_when_cli_path_is_absent() {
        let mut config = config_with_mode("batch");
        config.search.batch.file_path = "configured.txt".into();
        let plan = resolve_execution_plan(&config, &cli(None, None, None, None)).unwrap();
        assert_eq!(
            plan,
            ExecutionPlan::Batch {
                file_path: "configured.txt".into(),
            }
        );
    }

    #[test]
    fn batch_mode_requires_a_file() {
        let config = config_with_mode("batch");
        assert_config_error(
            &config,
            &cli(None, None, None, None),
            "batch mode requires",
        );
    }

    #[test]
    fn batch_mode_rejects_manual_selectors() {
        let config = config_with_mode("batch");
        assert_config_error(
            &config,
            &cli(None, Some("Artist"), None, None),
            "incompatible with batch mode",
        );
    }

    #[test]
    fn manual_and_batch_cli_selectors_conflict() {
        let config = config_with_mode("auto");
        assert_config_error(
            &config,
            &cli(None, Some("Artist"), None, Some("wantlist.txt")),
            "cannot be combined",
        );
    }

    #[test]
    fn inactive_config_values_do_not_infer_mode() {
        let mut auto = config_with_mode("auto");
        auto.search.manual.artist = "Stale Artist".into();
        auto.search.batch.file_path = "stale.txt".into();
        let plan = resolve_execution_plan(&auto, &cli(None, None, None, None)).unwrap();
        assert_eq!(plan, ExecutionPlan::Auto);

        let mut manual = config_with_mode("manual");
        manual.search.manual.artist = "Artist".into();
        manual.search.batch.file_path = "stale.txt".into();
        let plan = resolve_execution_plan(&manual, &cli(None, None, None, None)).unwrap();
        assert_eq!(
            plan,
            ExecutionPlan::Manual {
                artist: Some("Artist".into()),
                album: None,
            }
        );

        let mut batch = config_with_mode("batch");
        batch.search.manual.artist = "Stale Artist".into();
        batch.search.batch.file_path = "wantlist.txt".into();
        let plan = resolve_execution_plan(&batch, &cli(None, None, None, None)).unwrap();
        assert_eq!(
            plan,
            ExecutionPlan::Batch {
                file_path: "wantlist.txt".into(),
            }
        );
    }

    #[test]
    fn whitespace_values_do_not_satisfy_manual_or_batch_requirements() {
        let mut manual = config_with_mode("manual");
        manual.search.manual.artist = "   ".into();
        manual.search.manual.album = "\t".into();
        assert_config_error(&manual, &cli(None, Some("  "), None, None), "at least one");

        let mut batch = config_with_mode("batch");
        batch.search.batch.file_path = "  ".into();
        assert_config_error(&batch, &cli(None, None, None, Some("\t")), "batch mode requires");
    }

    #[test]
    fn unsupported_mode_is_rejected() {
        let config = config_with_mode("sideways");
        assert_config_error(&config, &cli(None, None, None, None), "must be auto, manual, or batch");
    }
}
```

- [ ] **Step 2: Run the resolver tests to verify the RED phase**

Run:

```bash
cargo test --lib mode::tests --no-run
```

Expected: compilation fails because `SearchMode`, `ExecutionPlan`, and
`resolve_execution_plan` are not defined yet. Do not weaken the tests to make this
phase compile.

## Task 2: Implement the pure resolver

**Files:**

- Modify: `src/mode.rs`

- [ ] **Step 1: Add the production resolver above the tests**

Keep the test module from Task 1 unchanged and add this production code before it:

```rust
use crate::config::{CliOverrides, Config};
use crate::error::{Result, SeakarrError};

/// The three operations that seakarr can execute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchMode {
    Auto,
    Manual,
    Batch,
}

/// Validated mode and the criteria needed by that mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecutionPlan {
    Auto,
    Manual {
        artist: Option<String>,
        album: Option<String>,
    },
    Batch {
        file_path: String,
    },
}

impl ExecutionPlan {
    /// Return the mode represented by this validated plan.
    pub fn mode(&self) -> SearchMode {
        match self {
            Self::Auto => SearchMode::Auto,
            Self::Manual { .. } => SearchMode::Manual,
            Self::Batch { .. } => SearchMode::Batch,
        }
    }
}

fn non_empty(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

/// Resolve and validate the operation selected by config and CLI overrides.
pub fn resolve_execution_plan(
    config: &Config,
    cli: &CliOverrides,
) -> Result<ExecutionPlan> {
    let raw_mode = cli
        .mode
        .as_deref()
        .unwrap_or(config.search.default_mode.as_str());
    let mode = match raw_mode.trim() {
        "auto" => SearchMode::Auto,
        "manual" => SearchMode::Manual,
        "batch" => SearchMode::Batch,
        value => {
            return Err(SeakarrError::Config(format!(
                "invalid search mode '{value}' (must be auto, manual, or batch)"
            )));
        }
    };

    let cli_artist = non_empty(cli.artist.as_deref());
    let cli_album = non_empty(cli.album.as_deref());
    let cli_batch_file = non_empty(cli.batch_file.as_deref());
    let has_manual_cli_selector = cli_artist.is_some() || cli_album.is_some();

    if has_manual_cli_selector && cli_batch_file.is_some() {
        return Err(SeakarrError::Config(
            "manual selectors --artist/--album cannot be combined with --batch-file".into(),
        ));
    }

    match mode {
        SearchMode::Auto => {
            if has_manual_cli_selector {
                return Err(SeakarrError::Config(
                    "--artist/--album are incompatible with auto mode; use --mode manual".into(),
                ));
            }
            if cli_batch_file.is_some() {
                return Err(SeakarrError::Config(
                    "--batch-file is incompatible with auto mode; use --mode batch".into(),
                ));
            }
            Ok(ExecutionPlan::Auto)
        }
        SearchMode::Manual => {
            if cli_batch_file.is_some() {
                return Err(SeakarrError::Config(
                    "--batch-file is incompatible with manual mode; use --mode batch".into(),
                ));
            }
            let artist = cli_artist
                .or_else(|| non_empty(Some(config.search.manual.artist.as_str())));
            let album = cli_album
                .or_else(|| non_empty(Some(config.search.manual.album.as_str())));
            if artist.is_none() && album.is_none() {
                return Err(SeakarrError::Config(
                    "manual mode requires at least one non-empty target: --artist or "
                        .to_owned()
                        + "--album (or search.manual values)",
                ));
            }
            Ok(ExecutionPlan::Manual { artist, album })
        }
        SearchMode::Batch => {
            if has_manual_cli_selector {
                return Err(SeakarrError::Config(
                    "--artist/--album are incompatible with batch mode; use --mode manual".into(),
                ));
            }
            let file_path = cli_batch_file
                .or_else(|| non_empty(Some(config.search.batch.file_path.as_str())));
            let Some(file_path) = file_path else {
                return Err(SeakarrError::Config(
                    "batch mode requires --batch-file or search.batch.file_path".into(),
                ));
            };
            Ok(ExecutionPlan::Batch { file_path })
        }
    }
}
```

The owned `Option<String>` values make CLI precedence explicit and avoid borrowing
temporary values from the fallback closures.

- [ ] **Step 2: Run all resolver tests to verify GREEN**

Run:

```bash
cargo fmt --all
cargo test --lib mode::tests -- --nocapture
```

Expected: all resolver tests pass, including the auto/manual conflict, batch conflict,
album-only manual, config fallback, inactive-config, whitespace, and invalid-mode cases.

- [ ] **Step 3: Commit the isolated resolver**

```bash
git add src/lib.rs src/mode.rs
git commit -m "feat: add pure CLI mode resolver"
```

## Task 3: Validate the plan before startup side effects

**Files:**

- Create: `tests/mode_resolution_test.rs`
- Modify: `src/main.rs:1-130`

- [ ] **Step 1: Add the process-level failing regression test**

Create `tests/mode_resolution_test.rs`:

```rust
use std::process::Command;
use tempfile::TempDir;

#[test]
fn artist_and_album_do_not_enter_configured_auto_mode() {
    let temp = TempDir::new().unwrap();
    let config_dir = temp.path().join("config");
    let log_dir = temp.path().join("logs");

    let output = Command::new(env!("CARGO_BIN_EXE_seakarr"))
        .args([
            "--config-path",
            config_dir.to_str().unwrap(),
            "--log-path",
            log_dir.to_str().unwrap(),
            "--listen-port",
            "2234",
            "--artist",
            "sleeper",
            "--album",
            "the modern age",
            "--test",
        ])
        .output()
        .expect("failed to start seakarr");

    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        output.status.code(),
        Some(1),
        "expected a mode-validation failure, got:\n{combined}"
    );
    assert!(
        combined.contains("--mode manual"),
        "error must tell the user how to select manual mode:\n{combined}"
    );
    assert!(
        !combined.contains("Connecting to Soulseek"),
        "mode errors must occur before login:\n{combined}"
    );
    assert!(
        !combined.contains("Scanning library"),
        "manual selectors must never enter the auto scanner:\n{combined}"
    );
}
```

The temporary config directory starts empty, so `Config::load` supplies the default
`auto` mode. `--test` avoids credentials and network access while exercising the real
binary startup path.

- [ ] **Step 2: Run the regression to verify the RED phase**

Run:

```bash
cargo test --test mode_resolution_test artist_and_album_do_not_enter_configured_auto_mode -- --nocapture
```

Expected: FAIL because the current binary merges the selectors but still reports the
configuration as valid under auto mode.

- [ ] **Step 3: Call the resolver before merging overrides or setting up logging**

In `src/main.rs`, retain the existing `CliOverrides` construction, then replace the current direct merge:

```rust
config.merge_cli(cli_overrides);
```

with:

```rust
let _execution_plan = seakarr::mode::resolve_execution_plan(&config, &cli_overrides)?;
config.merge_cli(cli_overrides);
```

This call must remain immediately after `CliOverrides` construction. It must precede
logging setup, the `--test` branch, `config.validate()`, database opening, PID locking,
and Soulseek login. Leave the old dispatch block in place temporarily; Task 5 replaces
it with plan dispatch.

- [ ] **Step 4: Run the regression to verify GREEN**

Run:

```bash
cargo test --test mode_resolution_test artist_and_album_do_not_enter_configured_auto_mode -- --nocapture
```

Expected: PASS with exit code `1`, an error containing `--mode manual`, and no connection or scan log.

- [ ] **Step 5: Commit the early validation change**

```bash
git add src/main.rs tests/mode_resolution_test.rs
git commit -m "fix: validate CLI mode selectors before startup"
```

## Task 4: Support album-only manual runner input

**Files:**

- Modify: `src/runner.rs:719-789` and `src/runner.rs` test module
- Modify: `src/main.rs:222,397` for the existing manual call sites

- [ ] **Step 1: Add the failing album-only runner test**

Add this test to `src/runner.rs`'s existing `tests` module, after `test_run_manual_mode`:

```rust
#[tokio::test]
async fn test_run_manual_mode_accepts_album_only() {
    let client = MockClient::new();
    let staging = TempDir::new().unwrap();
    let mut config = make_test_config();
    config.library.paths.clear();
    config.storage.staging_dir = staging.path().to_string_lossy().into();
    let db = Database::open_in_memory().unwrap();

    run_manual_mode(&client, None, Some("Test Album"), &config, &db)
        .await
        .expect("album-only manual mode must run");

    let queries = client.search_queries.lock().unwrap().clone();
    assert!(
        queries.iter().any(|query| query == "Test Album"),
        "album-only mode must issue an album-only query, got {queries:?}"
    );
}
```

- [ ] **Step 2: Run the album-only test to verify the RED phase**

Run:

```bash
cargo test --lib runner::tests::test_run_manual_mode_accepts_album_only -- --nocapture
```

Expected: compilation fails because the current `run_manual_mode` signature requires
`artist: &str` rather than `artist: Option<&str>`.

- [ ] **Step 3: Change `run_manual_mode` to normalize an optional artist**

Replace the current `run_manual_mode` function with this implementation. It keeps
`process_album`'s existing `&str` API, passing an empty artist only for the approved
album-only case, and skips library track-count derivation when there is no artist:

```rust
/// Run in manual mode: process a single artist and/or album search target.
pub async fn run_manual_mode(
    client: &dyn SoulseekClient,
    artist: Option<&str>,
    album: Option<&str>,
    config: &Config,
    db: &Database,
) -> Result<()> {
    let artist_name = artist
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or("");
    let staging_dir = Path::new(&config.storage.staging_dir);
    std::fs::create_dir_all(staging_dir)?;

    let album_display = album.unwrap_or("(all)");
    let mut report = RunReport::new();

    let progress = if is_interactive() {
        Some(ProgressDisplay::new())
    } else {
        None
    };
    let progress_ref = progress.as_ref();
    let cancel = Arc::new(AtomicBool::new(false));
    let _listener = spawn_cancel_listener(Arc::clone(&cancel));

    let derived_library_count = album.and_then(|album_name| {
        if artist_name.is_empty() || config.library.paths.is_empty() {
            return None;
        }
        search::get_library_track_filenames(&config.library.paths, artist_name, album_name)
            .ok()
            .filter(|tracks| !tracks.is_empty())
            .map(|tracks| tracks.len())
    });

    let result = process_album(
        client,
        artist_name,
        album,
        config,
        db,
        staging_dir,
        progress_ref,
        Some(&cancel),
        derived_library_count,
        None,
    )
    .await;
    match &result {
        Ok(outcome) => report.record(artist_name, album_display, outcome.clone()),
        Err(error) => {
            tracing::error!("Manual mode: {artist_name} — {album_display}: {error}");
            report.record(
                artist_name,
                album_display,
                AlbumOutcome::Failed {
                    reason: error.to_string(),
                },
            );
        }
    }

    if let Some(ref display) = progress {
        display.clear();
    }

    report.print_summary();
    _listener.abort();
    result.map(|_| ())
}
```

Update the two existing call sites in `src/main.rs` while the old dispatch block still exists:

```rust
runner::run_manual_mode(&client, Some(artist), album, &config, &db).await
```

and:

```rust
runner::run_manual_mode(client, Some(artist), album, config, db).await
```

- [ ] **Step 4: Run the runner and existing pipeline tests**

Run:

```bash
cargo fmt --all
cargo test --lib runner::tests::test_run_manual_mode_accepts_album_only -- --nocapture
cargo test --test pipeline_test -- --nocapture
```

Expected: all selected tests pass. The album-only test must observe `Test Album` as
a query, while the existing `process_album` public API tests remain unchanged.

- [ ] **Step 5: Commit optional-artist support**

```bash
git add src/main.rs src/runner.rs
git commit -m "feat: support album-only manual searches"
```

## Task 5: Dispatch the validated plan for one-shot and daemon runs

**Files:**

- Modify: `src/main.rs:190-236,330-418,501-550`

- [ ] **Step 1: Add a failing plan-dispatch test**

Add this test to `src/main.rs`'s existing `tests` module. First add the production
import at the top of `src/main.rs` so the test and the forthcoming dispatcher use the
same type:

```rust
use seakarr::mode::ExecutionPlan;
```

Then add:

```rust
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

    dispatch_execution_plan(&client, &plan, &config, &db)
        .await
        .expect("manual plan must dispatch without a library");

    let queries = client.search_queries.lock().unwrap();
    assert!(
        queries.iter().any(|query| query.contains("Michael Bolton")),
        "manual plan must use its artist, got queries: {queries:?}"
    );
}
```

Add a second test immediately after it for the batch branch:

```rust
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
    std::fs::write(&batch_path, "Artist - Album\n").unwrap();
    let db = Database::open_in_memory().unwrap();
    let plan = ExecutionPlan::Batch {
        file_path: batch_path.to_string_lossy().into_owned(),
    };

    dispatch_execution_plan(&client, &plan, &config, &db)
        .await
        .expect("batch plan must dispatch without a library");

    let queries = client.search_queries.lock().unwrap();
    assert!(
        queries.iter().any(|query| query.contains("Artist")),
        "batch plan must process its file, got queries: {queries:?}"
    );
}
```

- [ ] **Step 2: Run the new tests to verify the RED phase**

Run:

```bash
cargo test --bin seakarr dispatches_ -- --nocapture
```

Expected: compilation fails because `dispatch_execution_plan` does not exist.

- [ ] **Step 3: Add the single plan dispatcher**

Retain the `ExecutionPlan` import added in Step 1. Add this function immediately before
`run_daemon`:

```rust
async fn dispatch_execution_plan(
    client: &dyn SoulseekClient,
    plan: &ExecutionPlan,
    config: &Config,
    db: &Database,
) -> Result<()> {
    match plan {
        ExecutionPlan::Auto => runner::run_auto_mode(client, config, db).await,
        ExecutionPlan::Manual { artist, album } => {
            runner::run_manual_mode(client, artist.as_deref(), album.as_deref(), config, db).await
        }
        ExecutionPlan::Batch { file_path } => {
            run_batch_mode(client, file_path, config, db).await
        }
    }
}
```

This is the only mode-to-runner match in `main.rs`. It passes the plan's owned
criteria directly to both one-shot and daemon execution.

- [ ] **Step 4: Replace one-shot mode matching with plan dispatch**

Rename `_execution_plan` to `execution_plan`, then replace the existing dispatch block
beginning at `// Validate search mode before dispatch` and ending after the daemon
`if`/`else` result with:

```rust
    if config.daemon.enabled {
        let interval_mins = config.daemon.rescan_interval_mins.max(1);
        if config.daemon.rescan_interval_mins == 0 {
            tracing::warn!("daemon.rescan_interval_mins is 0 — clamping to 1 to avoid busy-loop");
        }
        let interval = tokio::time::Duration::from_secs(interval_mins * 60);
        run_daemon(
            &client,
            &config,
            &db,
            &pid_file,
            interval,
            &execution_plan,
        )
        .await
    } else {
        let result = dispatch_execution_plan(&client, &execution_plan, &config, &db).await;
        release_pid_lock(&pid_file)?;
        result
    }
```

This removes the late invalid-mode check and the duplicated manual/batch argument
resolution. Invalid plans have already returned before login, while valid plans use
the same data for every dispatch path.

- [ ] **Step 5: Pass the plan through the daemon loop**

Change the `run_daemon` signature to include the plan:

```rust
async fn run_daemon(
    client: &dyn SoulseekClient,
    config: &Config,
    db: &Database,
    pid_file: &Path,
    interval: tokio::time::Duration,
    plan: &ExecutionPlan,
) -> Result<()> {
```

Inside its loop, replace:

```rust
if let Err(e) = run_daemon_cycle(client, config, db).await {
```

with:

```rust
if let Err(e) = run_daemon_cycle(client, config, db, plan).await {
```

Replace `run_daemon_cycle` with:

```rust
/// Run one daemon cycle using the already validated execution plan.
async fn run_daemon_cycle(
    client: &dyn SoulseekClient,
    config: &Config,
    db: &Database,
    plan: &ExecutionPlan,
) -> Result<()> {
    dispatch_execution_plan(client, plan, config, db).await
}
```

Remove the now-unused `non_empty` helper from `src/main.rs`. Confirm it has no remaining callers with:

```bash
rg -n '\bnon_empty\(' src/main.rs
```

Expected: no matches. The resolver owns its own private trimming helper.

- [ ] **Step 6: Update the existing daemon test to pass an explicit plan**

In `daemon_cycle_honours_manual_mode_artist_album`, retain the existing test setup and
add this plan before the cycle call:

```rust
let plan = ExecutionPlan::Manual {
    artist: Some("Michael Bolton".into()),
    album: Some("The Essential Michael Bolton".into()),
};
```

Change the call from:

```rust
run_daemon_cycle(&client, &config, &db)
```

to:

```rust
run_daemon_cycle(&client, &config, &db, &plan)
```

The empty library paths continue to prove that the manual plan, not auto mode, was dispatched.

- [ ] **Step 7: Run one-shot, daemon, and integration tests**

Run:

```bash
cargo fmt --all
cargo test --bin seakarr dispatches_ -- --nocapture
cargo test --bin seakarr daemon_cycle_honours_manual_mode_artist_album -- --nocapture
cargo test --test mode_resolution_test -- --nocapture
cargo test --all-targets
```

Expected: all tests pass. The process-level test must fail before any connection log;
the daemon test must record the configured manual artist in its mock search queries;
and the existing suite must not regress.

- [ ] **Step 8: Commit plan-based dispatch**

```bash
git add src/main.rs
git commit -m "fix: dispatch validated CLI mode plans"
```

## Task 6: Document explicit mode selection

**Files:**

- Modify: `README.md:55-100` and the `### search` configuration section

- [ ] **Step 1: Add the user-facing mode rule and update option descriptions**

After the existing manual and batch examples in the Quick start section, add:

```markdown
Mode selection is explicit. `--artist` and `--album` are manual selectors, and
`--batch-file` is a batch selector; these options do not silently override the
configured `search.default_mode`. When the configured mode is `auto`, add
`--mode manual` for a manual target or `--mode batch` for a batch file. Album-only
manual searches are supported.
```

Replace the five mode-selection rows in the Options table with:

```markdown
| `--mode <mode>` | Select `auto`, `manual`, or `batch`: library scan, target search, or batch file. | *(from config)* |
| `--artist <name>` | Manual selector; may be used without `--album`. | *(from config)* |
| `--album <name>` | Manual selector; may be used without `--artist`. | *(from config)* |
| `--batch-file <path>` | Batch selector; cannot be combined with artist or album selectors. | *(from config)* |
| `--daemon` | Repeat the same validated auto, manual, or batch operation each cycle. | `false` |
```

In the `### search` configuration documentation, add this paragraph immediately before the search table:

```markdown
The selected `default_mode` determines which mode-specific values are active.
Manual values and batch paths do not infer a mode; values belonging to an inactive
mode are ignored. CLI values take precedence over values in the selected section.
```

Replace the three mode-specific config rows with:

```markdown
| `manual.artist` | Manual artist fallback, used only in `manual` mode. | `""` |
| `manual.album` | Manual album fallback, used only in `manual` mode. | `""` |
| `batch.file_path` | Batch file fallback, used only in `batch` mode. | `""` |
```

In the `## How it works` section, replace the Manual mode and Daemon mode descriptions
with:

```markdown
### Manual mode

Performs steps 3–7 above for a single artist and/or album. At least one target is
required; CLI values take precedence over `search.manual.artist` and
`search.manual.album`, and album-only searches are supported.

### Daemon mode

When `--daemon` or `daemon.enabled` is set, the same validated auto, manual, or batch
plan runs in a continuous loop. After each cycle, seakarr sleeps for
`daemon.rescan_interval_mins` before dispatching that unchanged plan again. The daemon
handles SIGINT (Ctrl+C) and SIGTERM gracefully — the PID file is removed and the current
cycle is allowed to finish.
```

- [ ] **Step 2: Run Markdown validation on the changed documentation**

Run:

```bash
markdownlint --fix README.md
git diff --check -- README.md
```

Expected: the changed Markdown has no new structural errors and the diff has no
whitespace errors. Preserve the repository's existing line-length convention; do not
modify unrelated README sections.

- [ ] **Step 3: Commit the documentation update**

```bash
git add README.md
git commit -m "docs: clarify CLI mode selection"
```

## Task 7: Run the complete verification gate

**Files:**

- None expected; this task verifies the implementation from Tasks 1-6.

- [ ] **Step 1: Run the focused resolver and startup tests**

```bash
cargo test --lib mode::tests -- --nocapture
cargo test --test mode_resolution_test -- --nocapture
cargo test --lib runner::tests::test_run_manual_mode_accepts_album_only -- --nocapture
```

Expected: all focused tests pass.

- [ ] **Step 2: Verify the exact reported invocation without network access**

Use a temporary config directory so no repository configuration or credentials are touched:

```bash
temp_dir=$(mktemp -d)
trap 'rm -rf "$temp_dir"' EXIT
set +e
output=$(cargo run --quiet -- \
  --config-path "$temp_dir/config" \
  --log-path "$temp_dir/logs" \
  --listen-port 2234 \
  --artist sleeper \
  --album "the modern age" \
  --test 2>&1)
status=$?
set -e
printf '%s\n' "$output"
test "$status" -eq 1
grep -F -- '--mode manual' <<<"$output"
! grep -F 'Connecting to Soulseek' <<<"$output"
! grep -F 'Scanning library' <<<"$output"
```

Expected: exit status `1`, an actionable `--mode manual` message, and no connection
or scan output. The trap removes the temporary directory after the check.

- [ ] **Step 3: Run formatting, linting, and the complete Rust suite**

Run in this order:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
```

Expected: each command exits `0` with no formatting, Clippy, or test failures.

- [ ] **Step 4: Run repository hooks and inspect the final diff**

```bash
pre-commit run --all-files
git diff --check
git status --short
git log -6 --oneline --decorate
```

Expected: pre-commit succeeds; `git diff --check` is clean; the working tree contains
only intentional implementation/documentation files; and no configuration, credential,
generated log, database, or ignored file is staged.

## Spec coverage self-check

- **Mode precedence and conflicts:** Tasks 1-3 implement and test CLI/config mode
  precedence, manual/batch selector conflicts, unsupported modes, and early errors.
- **Album-only manual input:** Task 1 specifies the plan shape and Task 4 updates
  `run_manual_mode` with a public API regression test.
- **Inactive config values:** Task 1 tests that stale manual/batch config values do
  not infer auto submodes; Task 6 documents the rule.
- **One-shot and daemon consistency:** Task 5 routes both through
  `dispatch_execution_plan` and updates the daemon regression test.
- **`--test` behavior and no connection:** Task 3 adds a process-level test; Task 7 repeats the exact CLI verification.
- **No schema or unrelated subsystem changes:** The file map explicitly excludes
  `src/config.rs`, `src/search.rs`, the database, credentials, logs, and vendor code.
- **Documentation:** Task 6 updates Quick start, option descriptions, and the search configuration section.
- **Quality gates:** Task 7 runs formatting, Clippy, tests, pre-commit, and diff checks.
