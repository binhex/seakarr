# Ignore Processed Albums Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use sub-agents (recommended) to
implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for
tracking.

**Goal:** Add a CLI-only `--ignore-processed` flag that deletes the matching
processed-album record and permits one intentional retry in one-shot auto,
manual, and batch modes.

**Architecture:** Carry one runtime boolean from Clap through `CliOverrides`,
mode validation, dispatch, and the shared `runner::process_album` function.
Delete only the exact artist/album row immediately before the existing success
check; leave search history and unrelated rows intact. Reject the flag when
daemon mode is enabled so it cannot force a repeat download on every cycle.

**Tech Stack:** Rust 2021, Clap derive, SQLite via `rusqlite`, Tokio, existing
unit tests, and binary integration tests.

---

## Scope and file map

This is one cohesive CLI-to-runner behavior change. The database method, CLI
plumbing,
and shared runner check are tightly coupled and should be implemented as one
source
chunk. Tests and README documentation are separate follow-up chunks.

Files to modify:

- `src/main.rs` — define `--ignore-processed`, reject it with effective daemon
mode,
  construct overrides, and pass the value through dispatch and batch execution.
- `src/config.rs` — add the runtime-only `ignore_processed: bool` field to
  `CliOverrides`; do not serialize it to YAML.
- `src/mode.rs` — reject the flag when `cli.daemon` or configured daemon mode
is
  enabled, before any startup side effects.
- `src/db.rs` — add a parameterized exact artist/album deletion method.
- `src/runner.rs` — accept the boolean in the shared processing path, delete
the
  matching row before the normal skip check, and propagate it through auto/manual
  processing.
- `tests/` — add database, runner, mode, and CLI integration coverage; update
  existing `process_album`, `run_auto_mode`, and dispatch call sites with the
  default `false` option.
- `README.md` — document usage, one-shot scope, record deletion, and daemon
  incompatibility.

No new files, database migration, configuration key, or integrity checker is
needed.

## Task 1: Add the CLI flag and pre-startup validation

**Files:**

- Modify: `src/main.rs` near `Cli`, `run`, and dispatch functions.
- Modify: `src/config.rs` in `CliOverrides`.
- Modify: `src/mode.rs` in `resolve_execution_plan` and its tests.
- Test: `tests/mode_resolution_test.rs`.

- [ ] **Step 1: Write failing CLI and mode tests**

Add a Clap integration test that runs `--help` and asserts it contains
`--ignore-processed`. Add a mode-resolution test that passes `ignore_processed:
true`
with either `cli.daemon: true` or `config.daemon.enabled = true` and asserts a
configuration error containing `cannot be used with daemon mode`.

```rust
#[test]
fn ignore_processed_is_rejected_for_daemon_mode() {
    let config = config_with_mode("auto");
    let mut overrides = cli(None, None, None, None);
    overrides.daemon = true;
    overrides.ignore_processed = true;

    assert_config_error(
        &config,
        &overrides,
        "cannot be used with daemon mode",
    );
}
```

Add `ignore_processed: bool` to the test helper's `CliOverrides` literal or
rely
on `..CliOverrides::default()` after the production field exists.

- [ ] **Step 2: Run the focused tests and confirm RED**

Run:

```bash
cargo test ignore_processed_is_rejected_for_daemon_mode -- --exact
cargo test --test mode_resolution_test -- --nocapture
```

Expected: compilation fails because the override field and CLI flag do not
exist.

- [ ] **Step 3: Implement the CLI plumbing and validation**

Add this field to `Cli`:

```rust
/// Reprocess an album even when it has a successful processed-album record
#[arg(long)]
ignore_processed: bool,
```

Add this field to `CliOverrides`:

```rust
pub ignore_processed: bool,
```

Populate it in `run()` alongside `daemon` and `test`. At the beginning of
`resolve_execution_plan`, before mode resolution or any side effects, reject
the
flag when either the CLI or effective configuration enables daemon mode:

```rust
if cli.ignore_processed && (cli.daemon || config.daemon.enabled) {
    return Err(SeakarrError::Config(
        "--ignore-processed cannot be used with daemon mode".into(),
    ));
}
```

Pass the boolean through the existing one-shot dispatch functions. Do not merge
it
into `Config`; it is invocation-only. Preserve all existing mode precedence and
validation behavior.

- [ ] **Step 4: Run the focused tests and verify GREEN**

Run:

```bash
cargo test ignore_processed_is_rejected_for_daemon_mode -- --exact
cargo test --test mode_resolution_test -- --nocapture
cargo fmt -- --check
```

Expected: the new daemon rejection and existing mode tests pass, and formatting
is
clean.

- [ ] **Step 5: Commit the CLI validation chunk**

```bash
git add src/main.rs src/config.rs src/mode.rs tests/mode_resolution_test.rs
git commit -m "feat: add ignore processed CLI option"
```

## Task 2: Add exact processed-record deletion and runner behavior

**Files:**

- Modify: `src/db.rs` in the processed-album methods.
- Modify: `src/runner.rs` in `process_album`, `run_auto_mode`, and
  `run_manual_mode`.
- Modify: `src/main.rs` in `dispatch_execution_plan` and `run_batch_mode`.
- Test: `src/db.rs` and `src/runner.rs` test modules.

- [ ] **Step 1: Write failing database and runner tests**

Add a database test proving deletion is exact:

```rust
#[test]
fn delete_processed_album_removes_only_the_requested_pair() {
    let db = Database::open_in_memory().unwrap();
    db.mark_album_processed("Artist", "Album", "success").unwrap();
    db.mark_album_processed("Artist", "Other", "success").unwrap();

    assert!(db.delete_processed_album("Artist", "Album").unwrap());
    assert!(!db.is_album_processed("Artist", "Album").unwrap());
    assert!(db.is_album_processed("Artist", "Other").unwrap());
    assert!(!db.delete_processed_album("Artist", "Missing").unwrap());
}
```

Add runner coverage that first creates a successful processed record, then
calls
`process_album` with `ignore_processed = false` and confirms `Skipped`,
followed by
a call with `ignore_processed = true` and a mock result that can complete.
Assert the
second call reaches processing and recreates the success record. Keep the
existing
failure-path test and assert a forced failed retry leaves status `failed`.

- [ ] **Step 2: Run the focused tests and confirm RED**

Run:

```bash
cargo test delete_processed_album_removes_only_the_requested_pair -- --exact
cargo test ignore_processed -- --nocapture
```

Expected: compilation fails because `delete_processed_album` and the new runner
parameter do not exist.

- [ ] **Step 3: Implement the database deletion method**

Add this method to `Database`:

```rust
pub fn delete_processed_album(
    &self,
    artist: &str,
    album: &str,
) -> Result<bool> {
    let deleted = self.conn.execute(
        "DELETE FROM processed_albums WHERE artist = ?1 AND album = ?2 AND status = 'success'",
        params![artist, album],
    )?;
    Ok(deleted > 0)
}
```

Use parameter binding exactly as shown. Do not delete search-history rows or
rows for
other artist/album pairs.

- [ ] **Step 4: Implement the shared runner bypass**

Add `ignore_processed: bool` to `process_album` immediately after `album` or in
the
existing options position, then update every in-repository call site. Replace
the
current skip block with this behavior:

```rust
if !artist.trim().is_empty() {
    if let Some(album_name) = album {
        if ignore_processed {
            if db.delete_processed_album(artist, album_name)? {
                tracing::info!(
                    "Ignoring already-processed record: {artist} — {album_name}"
                );
            }
        } else if db.is_album_processed(artist, album_name)? {
            tracing::info!(
                "Skipping already-processed: {artist} — {album_name}"
            );
            return Ok(AlbumOutcome::Skipped);
        }
    }
}
```

Add the same boolean to `run_auto_mode`, `run_manual_mode`, and the batch
dispatcher.
Pass it to every `process_album` invocation. One-shot auto passes the value for
each
scanner-selected target; manual passes it for its one target; batch passes it
for
each parsed line. The daemon path must never reach processing with this value
because
Task 1 rejects the combination.

Do not delete a record for album-only searches, because those searches
intentionally
have no processed-album key.

- [ ] **Step 5: Run runner and database tests**

Run:

```bash
cargo test delete_processed_album -- --nocapture
cargo test ignore_processed -- --nocapture
cargo test --lib
cargo fmt -- --check
```

Expected: all focused and library tests pass, including normal skip behavior
and
forced reprocessing behavior.

- [ ] **Step 6: Commit the processing chunk**

```bash
git add src/db.rs src/runner.rs src/main.rs
 git commit -m "feat: bypass processed album records"
```

## Task 3: Add end-to-end coverage for all supported modes

**Files:**

- Modify: `tests/mode_resolution_test.rs` or add a focused integration test
file
  under `tests/`.
- Modify: existing test call sites in `src/runner.rs`, `src/main.rs`, and
  `tests/pipeline_test.rs` to pass `false` where normal behavior is intended.

- [ ] **Step 1: Add CLI regression coverage**

Add an integration test that uses isolated config, database, log, and PID
paths,
pre-populates the database with a successful artist/album record, and invokes
the
binary with `--mode manual --artist Artist --album Album --ignore-processed
--test`.
Because `--test` exits before database processing, use the runner/database unit
test
for actual deletion and use the CLI test to verify flag parsing and daemon
rejection.
For daemon rejection, assert exit code 1 and no `Connecting to Soulseek`
output.

Add a process-level help assertion:

```rust
let output = Command::new(env!("CARGO_BIN_EXE_seakarr"))
    .arg("--help")
    .output()
    .unwrap();
let help = String::from_utf8_lossy(&output.stdout);
assert!(help.contains("--ignore-processed"));
```

- [ ] **Step 2: Verify mode propagation through tests**

Update auto, manual, and batch test calls to pass `false` and retain their
existing
assertions. Add a forced manual runner test with a pre-existing success record
and a
mock downloadable result. Add a batch test with one pre-existing record and one
new
entry; assert only the matching record is bypassed.

- [ ] **Step 3: Run all integration tests**

Run:

```bash
cargo test --test mode_resolution_test -- --nocapture
cargo test --test pipeline_test -- --nocapture
cargo test
```

Expected: every existing and new test passes with zero failures.

- [ ] **Step 4: Commit integration coverage**

```bash
git add src/runner.rs src/main.rs tests/
git commit -m "test: cover forced album reprocessing"
```

## Task 4: Document the public CLI behavior

**Files:**

- Modify: `README.md` in the usage examples, options table, and mode guidance.

- [ ] **Step 1: Update usage documentation**

Add this example beside manual mode:

```bash
# Reprocess an album whose previous successful download is recorded
seakarr --mode manual --artist "Afterlife" \
  --album "The Afterlife Lounge" --ignore-processed
```

Add this options-table entry with the project’s existing columns:

```text
| `--ignore-processed` | Delete the matching processed-album record and |
|                      | reprocess it for this invocation.              |
```

Add the daemon restriction immediately below the table as prose: the option cannot
be used with `--daemon`.

Explain that the flag is available for one-shot auto, manual, and batch modes,
does
not persist to YAML, leaves search history and unrelated albums untouched, and
is
intended for replacing corrupt or otherwise invalid downloaded files. State
that it
is rejected with daemon mode to prevent repeated forced downloads.

- [ ] **Step 2: Check documentation consistency**

Verify the README command examples use the exact flag spelling and that no
section
claims the flag is configurable in YAML or available in daemon mode.

- [ ] **Step 3: Run Markdown validation**

Run:

```bash
markdownlint --fix README.md
```

Expected: Markdown validation passes without changing unrelated README content.

## Final verification checklist

- [ ] `--ignore-processed` is parsed as a boolean CLI-only option.
- [ ] Daemon combinations fail before login and database processing.
- [ ] Normal invocations still skip successful processed records.
- [ ] Forced invocations delete only the exact artist/album record.
- [ ] Successful retries recreate the success record.
- [ ] Failed retries use existing failure status behavior.
- [ ] Auto, manual, and batch one-shot paths propagate the flag.
- [ ] Album-only requests remain unaffected.
- [ ] Search history and unrelated processed rows remain intact.
- [ ] README documents the exact behavior and safety warning.
- [ ] Full `cargo test` passes before finalising.
