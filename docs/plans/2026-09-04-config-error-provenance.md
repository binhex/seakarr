# Configuration Error Provenance Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use sub-agents
> (recommended) to implement this plan task-by-task. Steps use checkbox
> (`- [ ]`) syntax for tracking.

**Goal:** Report the absolute configuration filepath and
`search.default_mode` line when CLI selectors conflict with a mode loaded from
YAML.

**Architecture:** Keep optional, non-serialized source metadata on `Config`.
`Config::load()` records the canonical config path and the line for
`search.default_mode` after reconciliation, while
`mode::resolve_execution_plan()` adds that metadata only when the effective
mode came from configuration. Explicit CLI mode errors and in-memory configs
retain their current fallback messages.

**Tech Stack:** Rust 2021, `serde_yaml`, `clap`, `thiserror`, Cargo unit tests,
and binary integration tests under `tests/`.

---

## Scope check and file map

This is one cohesive subsystem: configuration loading and mode-conflict
diagnostics. It does not require separate plans because no independent
service, schema, or runtime subsystem is being changed.

Files to modify:

- `src/config.rs` — define optional config-source metadata, resolve the absolute
  filepath, locate the mode key line, and preserve metadata through loading.
- `src/mode.rs` — distinguish configured mode from explicit CLI mode and format
  provenance-aware conflict messages.
- `tests/mode_resolution_test.rs` — verify the user-visible binary error with
  an absolute path and line number.

Documentation:

- No README behavior section is required; the existing mode usage documentation
  remains accurate. The error output itself becomes the user guidance.

## Task 1: Add config source metadata and line detection

**Files:**

- Modify: `src/config.rs` near `Config` and `Config::load()`.
- Test: `src/config.rs` unit-test module.

- [ ] **Step 1: Write the failing metadata test**

Add a test using `TempDir` and a YAML file whose `search.default_mode` is on a
known line. Load it with `Config::load()` and assert that the config exposes the
absolute filepath and one-based line number. Also test a generated default file
so first-run configs have provenance.

```rust
#[test]
fn load_records_absolute_path_and_default_mode_line() {
    let dir = TempDir::new().unwrap();
    let config_file = dir.path().join("seakarr.yml");
    let yaml = concat!(
        "soulseek:\n  username: user\n  password: pass\n",
        "search:\n  default_mode: auto\n",
    );
    fs::write(&config_file, yaml).unwrap();

    let config = Config::load(dir.path()).unwrap();
    let source = config.source().expect("loaded config has source");

    assert_eq!(source.path, config_file.canonicalize().unwrap());
    assert_eq!(source.default_mode_line, 5);
}
```

The test should use a public or `pub(crate)` accessor rather than reading a
private field directly from outside the `Config` implementation.

- [ ] **Step 2: Run the focused test and confirm it fails**

Run:

```bash
cargo test load_records_absolute_path_and_default_mode_line -- --exact
```

Expected: compilation failure because `Config::source()` and the source metadata
type do not exist yet.

- [ ] **Step 3: Implement the minimal metadata type and loader wiring**

Add a cloneable source type and an optional skipped field to `Config`:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigSource {
    pub path: PathBuf,
    pub default_mode_line: usize,
}

// Inside Config:
#[serde(skip)]
source: Option<ConfigSource>,
```

Initialize `source` to `None` in `Config::default()`. In `Config::load()`, after
reconciliation and the final read, canonicalize `config_file`, find the
one-based `search.default_mode` line in the final contents, deserialize the
config, and set its source metadata before returning. Return a configuration
error if canonicalization fails. Expose a read-only accessor:

```rust
pub fn source(&self) -> Option<&ConfigSource> {
    self.source.as_ref()
}
```

Implement a focused helper that scans only the `search` YAML section and returns
the line whose trimmed content begins with `default_mode:`. Reset section state
when a non-indented top-level YAML key begins; return `None` if the key cannot
be found. The reconciled file is the source of truth, so line numbers refer to
the file users will inspect.

- [ ] **Step 4: Run config tests and verify the metadata behavior**

Run:

```bash
cargo test config::tests::load_records_absolute_path_and_default_mode_line -- --exact
cargo test config::tests::test_create_default_config_when_missing -- --exact
```

Expected: both tests pass, with the generated config also containing a source
path and a positive mode line. Existing serialization tests must remain passing
because the metadata field is skipped by Serde.

- [ ] **Step 5: Commit the loader change**

```bash
git add src/config.rs
git commit -m "feat: retain config source provenance"
```

## Task 2: Add provenance-aware mode conflict formatting

**Files:**

- Modify: `src/mode.rs` in `resolve_execution_plan()` and its tests.

- [ ] **Step 1: Write failing resolver tests**

Add tests that load a temporary YAML config, pass CLI artist or batch selectors,
and assert the error includes the configured key/value, canonical path, line,
and suggested mode:

```rust
#[test]
fn configured_auto_manual_selector_error_identifies_yaml_source() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("seakarr.yml"),
        "search:\n  default_mode: auto\n",
    )
    .unwrap();
    let config = Config::load(dir.path()).unwrap();
    let error = resolve_execution_plan(
        &config,
        &cli(None, Some("Artist"), None, None),
    )
    .unwrap_err()
    .to_string();

    assert!(error.contains("search.default_mode: auto"));
    assert!(error.contains(
        &config.source().unwrap().path.to_string_lossy(),
    ));
    assert!(error.contains(&format!(
        ":{}",
        config.source().unwrap().default_mode_line
    )));
    assert!(error.contains("use --mode manual"));
}
```

Add the equivalent batch-selector assertion and retain an in-memory config test
that confirms the old concise fallback still contains `--mode manual`.

- [ ] **Step 2: Run the mode tests and confirm the new assertions fail**

Run:

```bash
cargo test configured_auto_manual_selector_error_identifies_yaml_source \
  -- --exact
cargo test configured_auto_batch_selector_error_identifies_yaml_source \
  -- --exact
```

Expected: the tests fail because current errors do not include provenance.

- [ ] **Step 3: Implement source-sensitive error formatting**

Capture whether `cli.mode` is present before selecting `raw_mode`:

```rust
let mode_from_cli = cli.mode.is_some();
let raw_mode = cli.mode.as_deref().unwrap_or(config.search.default_mode.as_str());
```

Add a small formatter in `mode.rs` that returns the current message when
`mode_from_cli` is true or `config.source()` is absent. Otherwise append:

```text
configured by search.default_mode: <value> in <absolute-path>:<line>
```

Use it only for configured-mode conflicts in the auto branch: manual selectors
recommend `--mode manual`, and batch selectors recommend `--mode batch`. Leave
blank-selector, explicit-mode, invalid-mode, and unrelated validation messages
unchanged unless they use the same configured auto conflict path.

- [ ] **Step 4: Run focused and complete mode tests**

Run:

```bash
cargo test mode::tests -- --nocapture
```

Expected: all mode resolver tests pass, including existing explicit-mode and
in-memory fallback tests.

- [ ] **Step 5: Commit the resolver change**

```bash
git add src/mode.rs
git commit -m "feat: explain configured mode conflicts"
```

## Task 3: Verify the end-to-end CLI diagnostic

**Files:**

- Modify: `tests/mode_resolution_test.rs`.

- [ ] **Step 1: Add an integration test with a relative config path**

Create a temporary config beneath the repository working directory (for
example, a unique directory under `target/`), write `search.default_mode: auto`
and valid credentials, invoke the binary with that directory expressed as a
relative path, and remove the directory after the command. Pass `--artist` and
`--album` without `--mode`. Assert exit code 1 and verify the combined output
contains:

```rust
assert!(combined.contains("search.default_mode: auto"));
assert!(combined.contains(
    &config_file.canonicalize().unwrap().to_string_lossy(),
));
assert!(combined.contains("use --mode manual"));
```

Use the actual line number calculated from the written file rather than a
hard-coded number so the test remains clear if the fixture gains comments.
Also assert that the output does not contain a Soulseek connection message.

- [ ] **Step 2: Run the integration test**

Run:

```bash
cargo test --test mode_resolution_test \
  artist_and_album_do_not_enter_configured_auto_mode -- --nocapture
```

Expected: PASS with exit code 1 from the child process and the diagnostic
showing the canonical config path and mode line.

- [ ] **Step 3: Run the complete verification suite**

Run:

```bash
cargo test
cargo fmt -- --check
cargo clippy --all-targets --all-features -- -D warnings
```

Expected: all tests pass, formatting is clean, and Clippy reports no warnings.

- [ ] **Step 4: Commit integration coverage**

```bash
git add tests/mode_resolution_test.rs
git commit -m "test: verify config conflict provenance"
```

## Final review checklist

- [ ] The spec requirements are covered: absolute path, mode line, configured
  versus explicit mode distinction, fallback behavior, and tests.
- [ ] No serialization or config migration behavior changed.
- [ ] Error output identifies both the source setting and the CLI conflict.
- [ ] No credentials, logs, generated binaries, or ignored files are added.
