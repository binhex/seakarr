<!-- markdownlint-disable MD013 -->
# Rename `--mode auto` to `--mode upgrade` Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use sub-agents (recommended) to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Rename the `auto` search mode to `upgrade` in every layer - CLI value, config value, error text, README, and Rust identifiers - rejecting the retired CLI value and migrating the retired config value in place.

**Architecture:** The four mode values are parsed in one place, `resolve_execution_plan` (`src/mode.rs:99`), from `--mode` or `config.search.default_mode`. The config file is reconciled against the schema on every load by `reconcile_config_file` (`src/config.rs:710`), which already runs key migrations, writes a `seakarr.yml.bak` backup, rewrites the file and re-reads it. The new work adds a **value** migration to that same chain and swaps the vocabulary; no behaviour of the mode itself changes.

**Tech Stack:** Rust 2021, `clap` 4 (derive) for the CLI, `serde_yaml` for config parsing and migration, `tempfile` for tests, `tracing` for the reconciliation log line.

**Approved spec:** `docs/agent/specs/2026-09-29-rename-auto-mode-to-upgrade-design.md`

**Plan location:** this repository keeps agent-generated plans in `docs/agent/plans/` (AGENTS.md rule 7), which overrides the writing-plans default of `docs/plans/`.

**Ordering rationale:** every task leaves the tree compiling and every test green. That is why `upgrade` is accepted *before* `auto` is rejected: rejecting first would break the shipped default (`default_mode: auto` is what every generated config contains), taking the whole suite red mid-plan. The release is a MINOR version bump; versioning belongs to the finalising step, not to these tasks.

---

## Scope check

One plan, not several. The spec covers a single subsystem - the name of one search mode - and every surface it touches (resolver, config reconciliation, CLI help, dispatcher, doc comments, README, tests) is part of that one rename. No independent subsystem is involved, so no split is warranted.

## File structure

No file is created. Eight existing files change, plus four test locations. Each file keeps its current responsibility; the work is a vocabulary change inside them, not a restructuring.

| File | Responsibility | What changes |
| --- | --- | --- |
| `src/mode.rs` | Parses the four mode values into `SearchMode` and validates the resulting `ExecutionPlan` | The accepted-value match, the retired-value arm, the invalid-value message, the conflict and blank-selector messages, the enum variant names, and the mode-naming doc comments |
| `src/config.rs` | Loads, validates and reconciles `seakarr.yml` | `default_search_mode()`, the new `migrate_search_mode_value` value migration and its wiring into the migration chain, plus mode-naming doc comments |
| `src/main.rs` | CLI definition, startup sequence, dispatch | The `--mode` help text and the `ExecutionPlan::Upgrade` dispatch arm |
| `src/runner.rs` | Executes each mode | `run_auto_mode` -> `run_upgrade_mode` and its doc comment |
| `src/scanner.rs`, `src/filter.rs`, `src/discover.rs`, `src/discs.rs`, `src/organizer.rs` | Scan, quality filtering and placement | Mode-naming comments and doc comments only |
| `README.md` | Operator documentation | Every mode mention, the `default_mode` row and default, and the three rows that describe mode-specific behaviour |
| `src/mode.rs` (tests), `src/config.rs` (tests), `src/main.rs` (tests), `tests/mode_resolution_test.rs` | Test suites | New tests for the acceptance, the migration and the rejection; updated fixtures, assertions and test names |

`tests/pipeline_test.rs` carries one mode mention; Task 4 and Task 7 cover it through the repository-wide greps.

## Working conventions for every task

- **TDD**: write the test, run it, confirm it fails for the stated reason, then implement the minimum that makes it pass. A test that passes before the implementation is not evidence.
- **Read the output** of every command before moving on; the expected output is given for each run step.
- **Commit once per task**, with the message given in its final step, after the full suite is green.
- **ASCII only** in code, comments and commit messages.
- **Task 7 makes no commit**: it only verifies.

---

### Task 1: Accept `upgrade` and make it the default

**Files:**

- Modify: `src/mode.rs:114-126` (the mode match and its error message)
- Modify: `src/config.rs:358-360` (`default_search_mode`)
- Modify: `src/main.rs:83` (the `--mode` help text)
- Test: `src/mode.rs` (tests module, after `configured_auto_without_selectors_returns_auto`)
- Test: `src/config.rs` (tests module)
- Test: `src/main.rs` (tests module)
- Test: `src/mode.rs:686-694` (`unsupported_mode_is_rejected` assertion)

- [ ] **Step 1: Write the failing test for config-driven `upgrade`**

Add to the tests module in `src/mode.rs`, directly after `configured_auto_without_selectors_returns_auto` (line 303):

```rust
    #[test]
    fn configured_upgrade_without_selectors_returns_auto_plan() {
        // `upgrade` is the new spelling of the mode the `Auto` variants still
        // represent; the variant rename is a separate task.
        let config = config_with_mode("upgrade");
        let plan = resolve_execution_plan(&config, &cli(None, None, None, None)).unwrap();
        assert_eq!(plan, ExecutionPlan::Auto);
        assert_eq!(plan.mode(), SearchMode::Auto);
    }
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test --lib configured_upgrade_without_selectors_returns_auto_plan`
Expected: FAIL - `expected "invalid search mode 'upgrade' (must be auto, manual, batch, or discover)"`

- [ ] **Step 3: Write the failing test for the default value**

Add to the tests module in `src/config.rs`:

```rust
    #[test]
    fn default_search_mode_is_upgrade() {
        assert_eq!(Config::default().search.default_mode, "upgrade");
    }
```

- [ ] **Step 4: Run it to verify it fails**

Run: `cargo test --lib default_search_mode_is_upgrade`
Expected: FAIL - left: `"auto"`, right: `"upgrade"`

- [ ] **Step 5: Write the failing test for the CLI help text**

Add to the tests module in `src/main.rs`:

```rust
    #[test]
    fn the_mode_flag_help_advertises_upgrade() {
        use clap::CommandFactory;
        let help = Cli::command().render_help().to_string();
        assert!(
            help.contains("upgrade|manual|batch|discover"),
            "the --mode help must advertise upgrade, got:\n{help}"
        );
    }
```

- [ ] **Step 6: Run it to verify it fails**

Run: `cargo test --bin seakarr the_mode_flag_help_advertises_upgrade`
Expected: FAIL - the rendered help still says `(auto|manual|batch|discover)`

- [ ] **Step 7: Implement - accept `upgrade` in the resolver**

In `src/mode.rs`, change the mode match (lines 114-126) to add the new value and reorder the message list so `upgrade` leads. Keep the `"auto"` arm: rejecting it is Task 3, and removing it now would break every generated config before the migration exists.

```rust
    let mode = match raw_mode.trim() {
        "upgrade" => SearchMode::Auto,
        "auto" => SearchMode::Auto,
        "manual" => SearchMode::Manual,
        "batch" => SearchMode::Batch,
        "discover" => SearchMode::Discover,
        value => {
            return Err(SeakarrError::Config(format!(
                "invalid search mode '{value}' (must be upgrade, manual, batch, or discover)"
            )));
        }
    };
```

- [ ] **Step 8: Implement - make `upgrade` the default**

In `src/config.rs`, replace `default_search_mode` (lines 358-360):

```rust
fn default_search_mode() -> String {
    "upgrade".into()
}
```

- [ ] **Step 9: Implement - update the CLI help text**

In `src/main.rs`, replace the `--mode` doc comment (line 83):

```rust
    /// Override search mode (upgrade|manual|batch|discover)
    #[arg(long)]
    mode: Option<String>,
```

- [ ] **Step 10: Update the two assertions that pin the old message**

In `src/mode.rs`, the tests `unsupported_mode_is_rejected` (line ~686) and `empty_cli_mode_is_rejected` (line ~698) both assert `"must be auto, manual, batch, or discover"`. Change both expected strings to:

```rust
            "must be upgrade, manual, batch, or discover",
```

- [ ] **Step 11: Run the affected tests and the full suite**

Run: `cargo test`
Expected: PASS - all tests green, including `configured_upgrade_without_selectors_returns_auto_plan`, `default_search_mode_is_upgrade`, `the_mode_flag_help_advertises_upgrade`, and the two updated assertions.

- [ ] **Step 12: Commit**

```bash
git add src/mode.rs src/config.rs src/main.rs
git commit -m "feat: accept 'upgrade' as the mode name and make it the default"
```

---

### Task 2: Migrate the retired config value to `upgrade`

**Files:**

- Modify: `src/config.rs:730-737` (the migration chain in `reconcile_config_file`)
- Modify: `src/config.rs` (add `migrate_search_mode_value` next to `migrate_rename`, which starts at line 1070)
- Test: `src/config.rs` (tests module, next to `test_load_migrates_missing_sections_with_backup` at line 1729)
- Test: `tests/mode_resolution_test.rs` (`configured_auto_conflict_reports_absolute_config_path_and_line`)

- [ ] **Step 1: Write the failing test for the migration**

Add to the tests module in `src/config.rs`:

```rust
    #[test]
    fn test_load_migrates_retired_auto_mode_to_upgrade() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("seakarr.yml");
        fs::write(&file, "search:\n  default_mode: auto\n").unwrap();

        let config = Config::load(dir.path()).unwrap();

        // The in-memory config reflects the migrated file: Config::load
        // re-reads after reconciliation.
        assert_eq!(config.search.default_mode, "upgrade");
        let written = fs::read_to_string(&file).unwrap();
        assert!(written.contains("default_mode: upgrade"), "got:\n{written}");
        assert!(!written.contains("default_mode: auto"), "got:\n{written}");
        assert!(
            dir.path().join("seakarr.yml.bak").exists(),
            "the pre-migration file must be preserved as seakarr.yml.bak"
        );
    }

    #[test]
    fn test_load_does_not_back_up_an_already_canonical_config() {
        let dir = TempDir::new().unwrap();
        // The first load creates the default file; no reconciliation is needed.
        assert_eq!(
            Config::load(dir.path()).unwrap().search.default_mode,
            "upgrade"
        );
        let file = dir.path().join("seakarr.yml");
        let before = fs::read_to_string(&file).unwrap();

        // The second load must be a no-op: no rewrite, no backup.
        Config::load(dir.path()).unwrap();

        assert_eq!(fs::read_to_string(&file).unwrap(), before);
        assert!(!dir.path().join("seakarr.yml.bak").exists());
    }

    #[test]
    fn test_migrate_search_mode_value_matches_the_resolver_trim() {
        let mut value: serde_yaml::Value =
            serde_yaml::from_str("search:\n  default_mode: \" auto \"\n").unwrap();
        assert!(migrate_search_mode_value(&mut value));
        assert_eq!(value["search"]["default_mode"], "upgrade");
    }

    #[test]
    fn test_migrate_search_mode_value_leaves_an_unknown_value_alone() {
        let mut value: serde_yaml::Value =
            serde_yaml::from_str("search:\n  default_mode: upgrde\n").unwrap();
        assert!(!migrate_search_mode_value(&mut value));
        assert_eq!(value["search"]["default_mode"], "upgrde");
    }
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test --lib migrate_search_mode_value`
Expected: FAIL to compile - `cannot find function 'migrate_search_mode_value'`
Run: `cargo test --lib test_load_migrates_retired_auto_mode_to_upgrade`
Expected: compile error for the same reason (the whole suite fails to build until Step 4 adds the function)

- [ ] **Step 3: Write the failing integration assertion change**

In `tests/mode_resolution_test.rs`, the test `configured_auto_conflict_reports_absolute_config_path_and_line` writes a fixture containing `default_mode: auto` and asserts the conflict message quotes `search.default_mode: auto`. With the migration in place the file is rewritten before the conflict is reported, and `configured_mode_conflict` quotes the in-memory value (`src/mode.rs:93`), so rename the test and change that assertion:

Rename the function to:

```rust
fn configured_upgrade_conflict_reports_absolute_config_path_and_line() {
```

Keep the fixture writing `default_mode: auto` (that is what makes this a migration regression too) and add a comment recording why the reported value is `upgrade`:

```rust
    // The fixture carries the retired value on purpose: reconciliation rewrites
    // it to `upgrade` before mode resolution, so the conflict below is reported
    // against the migrated value and the migrated line. This doubles as the
    // regression test that the retired value never reaches the resolver.
```

Change the assertion to:

```rust
    assert!(
        combined.contains("search.default_mode: upgrade"),
        "got:\n{combined}"
    );
```

- [ ] **Step 4: Implement - add the value migration**

In `src/config.rs`, add this function directly above `migrate_rename` (line 1070):

```rust
/// Rewrite the retired `search.default_mode: auto` to `upgrade`.
///
/// The comparison is deliberately the same one the resolver makes - the raw
/// value, `trim()`ed - so the migration can never rewrite a value the resolver
/// would accept, or leave one it would reject. Any other value is left in place
/// for the resolver to reject, which keeps typos visible instead of silently
/// coercing them into a valid mode.
fn migrate_search_mode_value(config: &mut serde_yaml::Value) -> bool {
    let serde_yaml::Value::Mapping(root) = config else {
        return false;
    };
    let Some(serde_yaml::Value::Mapping(search)) =
        root.get_mut(serde_yaml::Value::String("search".into()))
    else {
        return false;
    };
    let mode_key = serde_yaml::Value::String("default_mode".into());
    let Some(serde_yaml::Value::String(value)) = search.get_mut(&mode_key) else {
        return false;
    };
    if value.trim() != "auto" {
        return false;
    }
    *value = "upgrade".to_string();
    true
}
```

- [ ] **Step 5: Implement - wire it into the reconciliation chain**

In `src/config.rs`, `reconcile_config_file`, extend the migration chain (the block ending at line 737) with the new step. The bitwise OR is existing behaviour: every migration must run even when an earlier one returned true.

```rust
        let renamed = migrate_schedule_section(&mut file_value, config_file)?
            | migrate_rename(&mut file_value, "filters", "min_bitrate", "min_bit_rate")
            | migrate_rename(&mut file_value, "filters", "min_bitdepth", "min_bit_depth")
            | migrate_rename(
                &mut file_value,
                "search",
                "prefer_reliable_peer",
                "peer_reputation",
            )
            | migrate_search_mode_value(&mut file_value);
```

- [ ] **Step 6: Run the new tests**

Run: `cargo test --lib migrate_search_mode_value && cargo test --lib test_load_migrates_retired_auto_mode_to_upgrade && cargo test --lib test_load_does_not_back_up_an_already_canonical_config`
Expected: PASS for all three

- [ ] **Step 7: Run the integration test**

Run: `cargo test --test mode_resolution_test configured_upgrade_conflict_reports_absolute_config_path_and_line -- --nocapture`
Expected: PASS - the message quotes `search.default_mode: upgrade` plus the absolute path and line

- [ ] **Step 8: Run the full suite**

Run: `cargo test`
Expected: PASS

- [ ] **Step 9: Commit**

```bash
git add src/config.rs tests/mode_resolution_test.rs
git commit -m "feat: migrate the retired 'auto' mode value to 'upgrade'"
```

---

### Task 3: Reject the retired CLI value with a pointer

**Files:**

- Modify: `src/mode.rs:114-127` (add the retired-value arm ahead of the fall-through)
- Test: `src/mode.rs` (tests module)
- Test: `tests/mode_resolution_test.rs` (new end-to-end test)
- Modify: `src/mode.rs` tests that use the retired spelling as a fixture

- [ ] **Step 1: Write the failing unit test**

Add to the tests module in `src/mode.rs`:

```rust
    #[test]
    fn the_retired_auto_value_is_rejected_with_a_pointer_to_upgrade() {
        let config = config_with_mode("upgrade");
        assert_config_error(
            &config,
            &cli(Some("auto"), None, None, None),
            "invalid search mode 'auto'; this mode is now called 'upgrade'",
        );
    }
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test --lib the_retired_auto_value_is_rejected_with_a_pointer_to_upgrade`
Expected: FAIL - the current message is `invalid search mode 'auto' (must be upgrade, manual, batch, or discover)`, because Task 1 left the `"auto"` arm in place

- [ ] **Step 3: Write the failing end-to-end test**

Add to `tests/mode_resolution_test.rs`:

```rust
#[test]
fn test_retired_auto_mode_is_rejected_before_login() {
    let temp = TempDir::new().unwrap();
    let config_dir = temp.path().join("config");
    let log_dir = temp.path().join("logs");

    let output = Command::new(env!("CARGO_BIN_EXE_seakarr"))
        .args([
            "--config-path",
            config_dir.to_str().unwrap(),
            "--log-path",
            log_dir.to_str().unwrap(),
            "--mode",
            "auto",
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
        "--mode auto must fail, got:\n{combined}"
    );
    assert!(
        combined.contains("invalid search mode 'auto'; this mode is now called 'upgrade'"),
        "the error must name the replacement, got:\n{combined}"
    );
    assert!(
        !combined.contains("Connecting to Soulseek"),
        "the mode error must occur before login:\n{combined}"
    );
}
```

- [ ] **Step 4: Run it to verify it fails**

Run: `cargo test --test mode_resolution_test test_retired_auto_mode_is_rejected_before_login`
Expected: FAIL - exit code 0 today, because `auto` is still accepted

- [ ] **Step 5: Implement - swap the accepted arm for the retired-value arm**

In `src/mode.rs`, replace the two arms from Task 1 so the retired spelling is caught by name ahead of the fall-through:

```rust
    let mode = match raw_mode.trim() {
        "upgrade" => SearchMode::Auto,
        "manual" => SearchMode::Manual,
        "batch" => SearchMode::Batch,
        "discover" => SearchMode::Discover,
        "auto" => {
            return Err(SeakarrError::Config(
                "invalid search mode 'auto'; this mode is now called 'upgrade'".into(),
            ));
        }
        value => {
            return Err(SeakarrError::Config(format!(
                "invalid search mode '{value}' (must be upgrade, manual, batch, or discover)"
            )));
        }
    };
```

- [ ] **Step 6: Update the test fixtures that still spell the mode `auto`**

In `src/mode.rs`, replace every fixture that passes the retired value with `upgrade`. They are not testing the retirement; they only need a valid default. First list them:

```bash
rg -n '"auto"' src/mode.rs
```

Then change exactly these, and only these:

- `empty_cli_mode_is_rejected` (line ~398): `config_with_mode("auto")` -> `config_with_mode("upgrade")`
- `explicit_manual_mode_overrides_configured_auto` (line ~399): `config_with_mode("auto")` -> `config_with_mode("upgrade")`
- `explicit_auto_rejects_manual_selector` (line ~547): `config_with_mode("auto")` -> `config_with_mode("upgrade")`
- `explicit_auto_rejects_batch_selector` (line ~557): `config_with_mode("auto")` -> `config_with_mode("upgrade")`
- `auto_mode_both_blank_selectors_report_blank_specific_message` (line ~710): `config_with_mode("auto")` -> `config_with_mode("upgrade")`
- `ignore_processed_is_allowed_for_one_shot_auto` (line ~790): `config_with_mode("auto")` -> `config_with_mode("upgrade")`

Leave the two fixtures inside `the_retired_auto_value_is_rejected_with_a_pointer_to_upgrade` untouched: that test must keep passing `Some("auto")`, or it stops testing the retirement.

- [ ] **Step 7: Run the mode unit tests**

Run: `cargo test --lib mode::`
Expected: PASS - the new rejection test plus every migrated fixture

- [ ] **Step 8: Run the full suite and the integration tests**

Run: `cargo test`
Expected: PASS - includes `test_retired_auto_mode_is_rejected_before_login`

- [ ] **Step 9: Commit**

```bash
git add src/mode.rs tests/mode_resolution_test.rs
git commit -m "feat: reject the retired 'auto' mode value"
```

---

### Task 4: Rename the identifiers to `Upgrade`

**Files:**

- Modify: `src/mode.rs` (`SearchMode` at line 6, `ExecutionPlan` at line 15, `ExecutionPlan::mode` at line 32, and the resolver's construction sites at lines 116 and 175)
- Modify: `src/main.rs:562` (the dispatch arm)
- Modify: `src/runner.rs:1403` (`run_auto_mode` and its doc comment)
- Test: `src/mode.rs` (test names and assertions mentioning `Auto`)
- Test: `src/runner.rs` (the tests calling `run_auto_mode`)
- Test: `tests/mode_resolution_test.rs` (test name and comment mentioning auto mode)

- [ ] **Step 1: Rename the enum variants**

In `src/mode.rs`, rename the two variants and their constructor sites. Nothing else in the crate consumes `SearchMode` outside this file, and `ExecutionPlan::Auto` is consumed only by the dispatcher.

```rust
/// The search modes that seakarr can execute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchMode {
    Upgrade,
    Manual,
    Batch,
    Discover,
}

/// Validated mode and the criteria needed by that mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecutionPlan {
    Upgrade,
    Manual {
        artist: Option<String>,
        album: Option<String>,
    },
    Batch {
        file_path: String,
    },
    /// Gap filling: artists come from the library, optionally narrowed to one.
    Discover {
        artist: Option<String>,
    },
}
```

In `ExecutionPlan::mode` (line 32):

```rust
    pub fn mode(&self) -> SearchMode {
        match self {
            Self::Upgrade => SearchMode::Upgrade,
            Self::Manual { .. } => SearchMode::Manual,
            Self::Batch { .. } => SearchMode::Batch,
            Self::Discover { .. } => SearchMode::Discover,
        }
    }
```

In the resolver, the accepted value now constructs the renamed variant, and the plan it returns is renamed too:

```rust
        "upgrade" => SearchMode::Upgrade,
```

```rust
        SearchMode::Upgrade => {
```

and, at the end of that arm:

```rust
            Ok(ExecutionPlan::Upgrade)
```

- [ ] **Step 2: Rename the runner entry point**

In `src/runner.rs`, rename the function and reword its doc comment (line 1402-1403):

```rust
/// Run in upgrade mode: scan library, find upgrades, process each album concurrently.
pub async fn run_upgrade_mode(
```

Then update its callers in `src/runner.rs` tests and the dispatcher in `src/main.rs:562`:

```rust
        ExecutionPlan::Upgrade => runner::run_upgrade_mode(client, config, db, ignore_processed).await,
```

- [ ] **Step 3: Rename the test names and remaining mentions**

Apply these renames so no test claims the mode is called auto:

```bash
# Verify what still says Auto/auto for this mode before editing.
rg -n 'Auto|run_auto_mode' src/mode.rs src/runner.rs src/main.rs tests/mode_resolution_test.rs
```

- `src/mode.rs`, each renamed exactly:
  - `configured_auto_without_selectors_returns_auto` -> `configured_upgrade_without_selectors_returns_upgrade`
  - `configured_auto_manual_selector_error_identifies_yaml_source` -> `configured_upgrade_manual_selector_error_identifies_yaml_source`
  - `configured_auto_batch_selector_error_identifies_yaml_source` -> `configured_upgrade_batch_selector_error_identifies_yaml_source`
  - `configured_auto_rejects_manual_cli_selectors` -> `configured_upgrade_rejects_manual_cli_selectors`
  - `configured_auto_rejects_batch_cli_selector` -> `configured_upgrade_rejects_batch_cli_selector`
  - `auto_mode_rejects_blank_cli_selectors` -> `upgrade_mode_rejects_blank_cli_selectors`
  - `auto_mode_reports_blank_batch_file_specifically` -> `upgrade_mode_reports_blank_batch_file_specifically`
  - `explicit_auto_rejects_manual_selector` -> `explicit_upgrade_rejects_manual_selector`
  - `explicit_auto_rejects_batch_selector` -> `explicit_upgrade_rejects_batch_selector`
  - `auto_mode_both_blank_selectors_report_blank_specific_message` -> `upgrade_mode_both_blank_selectors_report_blank_specific_message`
  - `ignore_processed_is_allowed_for_one_shot_auto` -> `ignore_processed_is_allowed_for_one_shot_upgrade`
  - `configured_upgrade_without_selectors_returns_auto_plan` (added by Task 1) -> `configured_upgrade_without_selectors_returns_upgrade_plan`
- `src/runner.rs`: every `run_auto_mode(` call in tests becomes `run_upgrade_mode(`, and `test_run_auto_mode_processes_album_and_marks_success` becomes `test_run_upgrade_mode_processes_album_and_marks_success`.
- `src/main.rs`: the test fixture at line ~1203 constructs `ExecutionPlan::Auto`; it becomes `ExecutionPlan::Upgrade`.
- `tests/mode_resolution_test.rs`: `artist_and_album_do_not_enter_configured_auto_mode` becomes `artist_and_album_do_not_enter_configured_upgrade_mode`, and its assertion message `"manual selectors must never enter the auto scanner"` becomes `"manual selectors must never enter the upgrade scanner"`.

- [ ] **Step 4: Verify no identifier survives**

Run: `rg -n 'SearchMode::Auto|ExecutionPlan::Auto|run_auto_mode|Self::Auto' src/ tests/`
Expected: no output

- [ ] **Step 5: Run the full suite**

Run: `cargo test`
Expected: PASS - the compiler proves every rename site, and every renamed test still asserts the same behaviour

- [ ] **Step 6: Commit**

```bash
git add src/mode.rs src/main.rs src/runner.rs tests/mode_resolution_test.rs
git commit -m "refactor: rename the auto mode identifiers to upgrade"
```

---

### Task 5: Reword the mode-naming prose in code comments

**Files:**

- Modify: `src/mode.rs:64-67` (the `configured_mode_conflict` doc comment) and `src/mode.rs:79` (its `incompatible with auto mode` text)
- Modify: `src/mode.rs:148-156` (the two blank-selector messages)
- Modify: `src/config.rs` (mode-naming doc comments: `library_upgrade.enabled`, `scan_on_startup`, `peer_track_count`, the `search.default_mode` docs)
- Modify: `src/scanner.rs:411`, `:459`, `:583`, `:1324`, `:1539`, `:1604`
- Modify: `src/filter.rs:7`, `:106`, `:165`, `:2112`
- Modify: `src/discover.rs`, `src/discs.rs`, `src/organizer.rs` where the word means this mode
- Test: `src/mode.rs` (the conflict assertions that pin the message)

- [ ] **Step 1: Write the failing assertion for the conflict text**

In `src/mode.rs`, add to the tests module:

```rust
    #[test]
    fn the_upgrade_conflict_message_names_upgrade_mode() {
        let config = config_with_mode("upgrade");
        assert_config_error(
            &config,
            &cli(None, Some("Artist"), None, None),
            "are incompatible with upgrade mode",
        );
    }
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test --lib the_upgrade_conflict_message_names_upgrade_mode`
Expected: FAIL - the message still reads `are incompatible with auto mode`

- [ ] **Step 3: Fix the user-facing conflict and blank-selector text**

In `src/mode.rs`, in `configured_mode_conflict` (line 79):

```rust
    let base = format!("{selectors} {verb} incompatible with upgrade mode");
```

and its doc comment (lines 64-67), which says "Format an auto-mode conflict error":

```rust
/// Format an upgrade-mode conflict error. When the mode came from the YAML
/// config (not `--mode`) and that config knows its on-disk source, point the
/// user at the exact `search.default_mode` line that caused the conflict.
/// Otherwise keep the concise legacy message (explicit `--mode` or in-memory
/// config).
```

In the `SearchMode::Upgrade` arm (lines 148-156), reword the two messages:

```rust
                    "--batch-file must not be blank in upgrade mode; use --mode batch".into(),
```

```rust
                    "blank CLI selector is incompatible with upgrade mode; use --mode manual".into(),
```

- [ ] **Step 4: Update the conflict assertions that pin the old wording**

Run: `rg -n 'incompatible with auto mode' src/mode.rs tests/`
Expected: the conflict helper plus the test expectations. Update every test expectation to `incompatible with upgrade mode`.

- [ ] **Step 5: Reword the mode-naming comments**

Run this to list every remaining prose mention of the mode:

```bash
rg -n 'auto mode|auto-mode|Auto mode|automatic mode' src/
```

Change each one that means this mode to `upgrade mode` / `upgrade-mode`. Leave untouched anything where "auto" means something else - in particular `src/client.rs:302`, `:501`, `:1299`, `:1307` ("never auto-retried"), which must not change.

- [ ] **Step 6: Verify only unrelated `auto` wording remains**

Run: `rg -n 'auto mode|auto-mode|Auto mode|automatic mode' src/`
Expected: no output (the `auto-retried` hits do not match this pattern; confirm with `rg -n 'auto' src/client.rs` that they are intact)

- [ ] **Step 7: Run the full suite**

Run: `cargo test`
Expected: PASS

- [ ] **Step 8: Commit**

```bash
git add src/mode.rs src/config.rs src/scanner.rs src/filter.rs src/discover.rs src/discs.rs src/organizer.rs
git commit -m "docs: name the upgrade mode in code comments and conflict messages"
```

---

### Task 6: Update the README

**Files:**

- Modify: `README.md:40`, `:146`, `:224`, `:253`, `:264`, `:270`, `:406`, `:411`, `:447`, `:639`, `:874`, `:1145`, `:1149`

- [ ] **Step 1: List every README mention of the mode**

Run: `rg -n -- '--mode auto|auto mode|Auto mode|default_mode' README.md`
Expected: the lines listed above, including the FAQ question `Can I run auto mode and discover mode on a schedule at the same time?` (line 1145) and the mode list at line 270.

- [ ] **Step 2: Update each mention**

Rewrite them to the new vocabulary. The four that must change value as well as wording:

```markdown
| `default_mode` | Default search mode. Choices: `upgrade`, `manual`, `batch`, `discover`. | `upgrade` |
```

```markdown
| `scan_on_startup` | Rescan the library on startup (upgrade mode). *(Reserved for future use — not yet enforced.)* | `true` |
```

```markdown
| `enabled` | Enable the library-upgrade workflow (upgrade mode only). ...
```

```markdown
instance performs one job: either upgrading what you have (`--mode upgrade`) or
```

Prose mentions ("In auto mode", "Auto mode's upgrade scan", the FAQ question, and the `--album` note at line 40) become "upgrade mode" and "Upgrade mode's upgrade scan" respectively. Do not alter line 447's meaning: the section it documents still only takes effect in this mode.

- [ ] **Step 3: Verify no stale mention remains**

Run: `rg -n -- '--mode auto|auto mode|Auto mode' README.md`
Expected: no output

- [ ] **Step 4: Lint the README**

Run: `markdownlint README.md`
Expected: exit 0, no output

- [ ] **Step 5: Commit**

```bash
git add README.md
git commit -m "docs: name the upgrade mode in the README"
```

---

### Task 7: Verify the acceptance criteria and run every gate

**Files:** none modified - verification only, so this task makes no commit.

- [ ] **Step 1: Acceptance criterion 1 - the CLI contract**

Run: `cargo test --test mode_resolution_test`
Expected: PASS, including `test_retired_auto_mode_is_rejected_before_login` (exit 1, message names `upgrade`) and the updated conflict test

- [ ] **Step 2: Acceptance criteria 2 and 3 - migration and the default**

Run: `cargo test --lib test_load_migrates_retired_auto_mode_to_upgrade test_load_does_not_back_up_an_already_canonical_config default_search_mode_is_upgrade`
Expected: PASS for all three

- [ ] **Step 3: Acceptance criterion 4 - no stale vocabulary**

```bash
rg -n 'SearchMode::Auto|ExecutionPlan::Auto|run_auto_mode|--mode auto|auto mode|auto-mode|Auto mode' src/ tests/ README.md; echo "exit=$?"
```

Expected: no matches (`exit=1` from ripgrep means no matches found). Then confirm the untouched wording is intact:

```bash
rg -c 'auto-retr' src/client.rs
```

Expected: `4` (the wording appears as `auto-retried` on three lines and `auto-retry` on one, so a count of the exact token `auto-retried` would read 3)

- [ ] **Step 4: Acceptance criterion 5 - formatter and linter**

```bash
cargo fmt --check
cargo clippy -- -D warnings
```

Expected: both exit 0

- [ ] **Step 5: Full test suite**

Run: `cargo test`
Expected: `0 failed` across every target

- [ ] **Step 6: Coverage**

Run: `cargo llvm-cov -p seakarr --summary-only --fail-under-lines 95`
Expected: exit 0, and `mode.rs` and `config.rs` at or above 95 percent lines

- [ ] **Step 7: Markdown and pre-commit gates**

```bash
markdownlint README.md
pre-commit run --all-files
```

Expected: markdownlint exit 0; pre-commit all hooks pass. Do not commit during this step.

- [ ] **Step 8: Report**

Summarise for the chain: the commands run, their exit codes, and any acceptance criterion that did not hold. No commit is made by this task.
