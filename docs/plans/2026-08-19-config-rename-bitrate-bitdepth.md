# Config Rename: min_bitrate/min_bitdepth → min_bit_rate/min_bit_depth — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use sub-agents (recommended) to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Rename `min_bitrate`/`min_bitdepth` to `min_bit_rate`/`min_bit_depth`, change type from `Option<u32>` to `u32` (0 = disabled), and migrate existing configs.

**Architecture:** Single-pass refactor touching `config.rs` (struct + migration), `filter.rs` (filter logic + tests), and `download.rs` (verification + tests). Config migration preserves existing user values.

**Tech Stack:** Rust, serde/serde_yaml, existing seakarr config/filter/download modules

**Spec:** `docs/specs/2026-08-19-config-rename-bitrate-bitdepth.md`

---

## File Structure

| File | Responsibility | Change Type |
|------|---------------|-------------|
| `src/config.rs` | `FilterConfig` struct definition, defaults, example YAML, migration logic | Modify |
| `src/filter.rs` | `file_passes_filters()`, `rank_candidates()`, `summarize_rejections()`, `FilterRejectionSummary` field name, all tests | Modify |
| `src/download.rs` | `verify_downloaded_quality()`, all tests | Modify |
| `tests/pipeline_test.rs` | Any tests that set `min_bitrate`/`min_bitdepth` | Modify (if needed) |

---

### Task 1: Add migrate_rename helper and tests to config.rs

**Files:**
- Modify: `src/config.rs`

The `reconcile_config_file()` function needs a migration step to rename old keys to new keys while preserving values. Add a `migrate_rename()` helper and tests.

- [ ] **Step 1: Write the failing tests**

Add these tests to `src/config.rs mod tests`:

```rust
#[test]
fn test_migrate_rename_preserves_value() {
    let mut config = serde_yaml::from_str(indoc! {"
        filters:
          min_bitrate: 320
    "}).unwrap();
    migrate_rename(&mut config, "filters", "min_bitrate", "min_bit_rate");
    let filters = config.get("filters").unwrap();
    assert_eq!(filters["min_bit_rate"].as_u64().unwrap(), 320);
}

#[test]
fn test_migrate_rename_null_becomes_zero() {
    let mut config = serde_yaml::from_str(indoc! {"
        filters:
          min_bitrate: null
    "}).unwrap();
    migrate_rename(&mut config, "filters", "min_bitrate", "min_bit_rate");
    let filters = config.get("filters").unwrap();
    // null → 0 (disabled)
    assert_eq!(filters["min_bit_rate"].as_u64().unwrap_or(0), 0);
}

#[test]
fn test_migrate_rename_missing_key() {
    let mut config = serde_yaml::from_str(indoc! {"
        filters:
          allowed_extensions: [flac]
    "}).unwrap();
    migrate_rename(&mut config, "filters", "min_bitrate", "min_bit_rate");
    let filters = config.get("filters").unwrap();
    // No old key → new key not added (merge_with_defaults will add it)
    assert!(filters.get("min_bit_rate").is_none());
}

#[test]
fn test_migrate_rename_removes_old_key() {
    let mut config = serde_yaml::from_str(indoc! {"
        filters:
          min_bitrate: 320
    "}).unwrap();
    migrate_rename(&mut config, "filters", "min_bitrate", "min_bit_rate");
    let filters = config.get("filters").unwrap();
    assert!(filters.get("min_bitrate").is_none(), "old key must be removed");
    assert!(filters.get("min_bit_rate").is_some(), "new key must exist");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test test_migrate_rename -- --nocapture 2>&1 | tail -20`
Expected: FAIL with "cannot find function `migrate_rename`"

- [ ] **Step 3: Implement the migrate_rename helper**

Add this function to `src/config.rs`, near `merge_with_defaults()`:

```rust
/// Rename a key within a YAML section, preserving the value.
/// If the old key exists and is not null, its value is copied to the new key.
/// If the old key exists and is null, the new key is set to 0 (disabled).
/// The old key is always removed if present.
/// If the old key is missing, nothing happens (merge_with_defaults will add the new key with its default).
fn migrate_rename(config: &mut serde_yaml::Value, section: &str, old_key: &str, new_key: &str) {
    let serde_yaml::Value::Mapping(root) = config else { return; };
    let Some(serde_yaml::Value::Mapping(sec)) = root.get_mut(&serde_yaml::Value::String(section.into())) else { return; };

    let old_key_yaml = serde_yaml::Value::String(old_key.into());
    let new_key_yaml = serde_yaml::Value::String(new_key.into());

    // Remove the old key and get its value
    let old_val = sec.remove(&old_key_yaml);

    match old_val {
        Some(serde_yaml::Value::Null) => {
            // null → 0 (disabled)
            sec.insert(new_key_yaml, serde_yaml::Value::Number(0.into()));
        }
        Some(val) => {
            // Preserve the value under the new key
            sec.insert(new_key_yaml, val);
        }
        None => {
            // Old key not present — nothing to migrate.
            // merge_with_defaults will add the new key with its default (0).
        }
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test test_migrate_rename -- --nocapture 2>&1 | tail -20`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add src/config.rs
git commit -m "feat: add migrate_rename helper for config key renames"
```

---

### Task 2: Rename FilterConfig fields and update defaults

**Files:**
- Modify: `src/config.rs`

Rename the struct fields, change the type from `Option<u32>` to `u32`, add `#[serde(default)]`, update the default implementation and example YAML.

- [ ] **Step 1: Write the failing tests**

Add these tests to `src/config.rs mod tests`:

```rust
#[test]
fn test_filter_config_min_bit_rate_default_is_zero() {
    let config = Config::default();
    assert_eq!(config.filters.min_bit_rate, 0, "min_bit_rate default should be 0 (disabled)");
}

#[test]
fn test_filter_config_min_bit_depth_default_is_zero() {
    let config = Config::default();
    assert_eq!(config.filters.min_bit_depth, 0, "min_bit_depth default should be 0 (disabled)");
}

#[test]
fn test_filter_config_from_yaml_parses_min_bit_rate() {
    let yaml = indoc! {"
        soulseek:
          username: test
          password: test
        filters:
          min_bit_rate: 320
    "};
    let config: Config = serde_yaml::from_str(yaml).unwrap();
    assert_eq!(config.filters.min_bit_rate, 320);
}

#[test]
fn test_filter_config_from_yaml_parses_min_bit_depth() {
    let yaml = indoc! {"
        soulseek:
          username: test
          password: test
        filters:
          min_bit_depth: 24
    "};
    let config: Config = serde_yaml::from_str(yaml).unwrap();
    assert_eq!(config.filters.min_bit_depth, 24);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test test_filter_config_min_bit -- --nocapture 2>&1 | tail -20`
Expected: FAIL with "no field `min_bit_rate` on type `FilterConfig`"

- [ ] **Step 3: Rename the struct fields**

In `src/config.rs`, change the `FilterConfig` struct:

```rust
// Current (around line 102-103):
pub min_bitrate: Option<u32>,
pub min_bitdepth: Option<u32>,

// New:
#[serde(default)]
pub min_bit_rate: u32,
#[serde(default)]
pub min_bit_depth: u32,
```

- [ ] **Step 4: Update the Default implementation**

In the `Default for Config` impl (around line 637-638):

```rust
// Current:
min_bitrate: None,
min_bitdepth: None,

// New:
min_bit_rate: 0,
min_bit_depth: 0,
```

- [ ] **Step 5: Update the example YAML**

In the config generation code (around line 724-725):

```rust
// Current:
min_bitrate: null
min_bitdepth: null

// New:
min_bit_rate: 0      # Minimum bitrate (kbps) for lossy files. 0 = disabled.
min_bit_depth: 0     # Minimum bit depth for lossless files. 0 = disabled.
```

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test test_filter_config_min_bit -- --nocapture 2>&1 | tail -20`
Expected: PASS

- [ ] **Step 7: Commit**

```bash
git add src/config.rs
git commit -m "feat: rename FilterConfig fields to min_bit_rate/min_bit_depth (u32, 0=disabled)"
```

---

### Task 3: Update reconcile_config_file to call migrate_rename

**Files:**
- Modify: `src/config.rs`

The `reconcile_config_file()` function needs to call `migrate_rename()` before the merge to preserve existing user values.

- [ ] **Step 1: Write the failing test**

Add this test to `src/config.rs mod tests`:

```rust
#[test]
fn test_reconcile_migrates_min_bitrate_to_min_bit_rate() {
    let dir = tempfile::TempDir::new().unwrap();
    let config_file = dir.path().join("seakarr.yml");
    fs::write(&config_file, indoc! {"
        soulseek:
          username: test
          password: test
        filters:
          min_bitrate: 320
          min_bitdepth: 16
    "}).unwrap();

    Config::reconcile_config_file(&config_file, &fs::read_to_string(&config_file).unwrap()).unwrap();

    let contents = fs::read_to_string(&config_file).unwrap();
    let config: serde_yaml::Value = serde_yaml::from_str(&contents).unwrap();
    let filters = config.get("filters").unwrap();

    assert_eq!(filters["min_bit_rate"].as_u64().unwrap(), 320, "min_bitrate: 320 must become min_bit_rate: 320");
    assert_eq!(filters["min_bit_depth"].as_u64().unwrap(), 16, "min_bitdepth: 16 must become min_bit_depth: 16");
    assert!(filters.get("min_bitrate").is_none(), "old key min_bitrate must be removed");
    assert!(filters.get("min_bitdepth").is_none(), "old key min_bitdepth must be removed");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test test_reconcile_migrates_min_bitrate -- --nocapture 2>&1 | tail -20`
Expected: FAIL (old keys still present or new keys not set)

- [ ] **Step 3: Add migration calls to reconcile_config_file**

In `src/config.rs`, in `reconcile_config_file()` (around line 436), add migration calls before the merge:

```rust
fn reconcile_config_file(config_file: &Path, contents: &str) -> Result<()> {
    let default_value: serde_yaml::Value = serde_yaml::to_value(Config::default()).map_err(|e| {
        SeakarrError::Config(format!("failed to serialize default config: {e}"))
    })?;
    let mut file_value: serde_yaml::Value = serde_yaml::from_str(contents)
        .map_err(|e| SeakarrError::Config(format!("failed to parse {config_file:?}: {e}")))?;

    // Migration: rename config keys that changed between versions.
    // Preserves existing values (e.g., min_bitrate: 320 becomes min_bit_rate: 320).
    migrate_rename(&mut file_value, "filters", "min_bitrate", "min_bit_rate");
    migrate_rename(&mut file_value, "filters", "min_bitdepth", "min_bit_depth");

    let merged = merge_with_defaults(&default_value, &file_value);
    // ... rest of existing logic unchanged ...
```

Note: `file_value` must be `mut` now (it wasn't before). Change `let file_value` to `let mut file_value`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test test_reconcile_migrates_min_bitrate -- --nocapture 2>&1 | tail -20`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add src/config.rs
git commit -m "feat: migrate old min_bitrate/min_bitdepth keys to new names on config load"
```

---

### Task 4: Update filter.rs to use new field names

**Files:**
- Modify: `src/filter.rs`

Update all references to `min_bitrate`/`min_bitdepth` in filter logic and tests. Change `if let Some(min_br) = config.min_bitrate` to `if config.min_bit_rate > 0`.

- [ ] **Step 1: Write the failing tests**

Update existing tests to use new field names. In `src/filter.rs mod tests`, change all occurrences:

```rust
// Old pattern:
min_bitrate: Some(320),
min_bitdepth: None,

// New pattern:
min_bit_rate: 320,
min_bit_depth: 0,
```

And:
```rust
// Old pattern:
min_bitrate: None,
min_bitdepth: Some(16),

// New pattern:
min_bit_rate: 0,
min_bit_depth: 16,
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test filter:: -- --nocapture 2>&1 | grep "error" | head -10`
Expected: FAIL with "no field `min_bitrate` on type `&FilterConfig`"

- [ ] **Step 3: Update file_passes_filters**

In `src/filter.rs`, change `file_passes_filters()` (around line 209-227):

```rust
// Current:
if let Some(min_br) = config.min_bitrate {
    if let Some(&file_br) = file.attribs.get(&0) {
        if file_br < min_br {
            return false;
        }
    }
}
if let Some(min_bd) = config.min_bitdepth {
    if let Some(&file_bd) = file.attribs.get(&5) {
        if file_bd < min_bd {
            return false;
        }
    }
}

// New:
if config.min_bit_rate > 0 {
    if let Some(&file_br) = file.attribs.get(&0) {
        if file_br < config.min_bit_rate {
            return false;
        }
    }
}
if config.min_bit_depth > 0 {
    if let Some(&file_bd) = file.attribs.get(&5) {
        if file_bd < config.min_bit_depth {
            return false;
        }
    }
}
```

- [ ] **Step 4: Update rank_candidates**

In `src/filter.rs`, change `rank_candidates()` (around line 336):

```rust
// Current:
let bitrate_bonus = if let Some(min_br) = config.min_bitrate {

// New:
let bitrate_bonus = if config.min_bit_rate > 0 {
```

And update the comparison inside to use `config.min_bit_rate` instead of `min_br`.

- [ ] **Step 5: Update summarize_rejections**

In `src/filter.rs`, change `summarize_rejections()` (around line 1405-1433):

```rust
// Current:
if let Some(min_br) = config.min_bitrate {
    if let Some(&file_br) = f.attribs.get(&0) {
        if file_br < min_br {
            summary.bitrate_rejected += 1;
            continue;
        }
    }
}
if let Some(min_bd) = config.min_bitdepth {
    if let Some(&file_bd) = f.attribs.get(&5) {
        if file_bd < min_bd {
            summary.bitdepth_rejected += 1;
            continue;
        }
    }
}

// New:
if config.min_bit_rate > 0 {
    if let Some(&file_br) = f.attribs.get(&0) {
        if file_br < config.min_bit_rate {
            summary.bitrate_rejected += 1;
            continue;
        }
    }
}
if config.min_bit_depth > 0 {
    if let Some(&file_bd) = f.attribs.get(&5) {
        if file_bd < config.min_bit_depth {
            summary.bitdepth_rejected += 1;
            continue;
        }
    }
}
```

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test filter:: -- --nocapture 2>&1 | tail -10`
Expected: PASS

- [ ] **Step 7: Commit**

```bash
git add src/filter.rs
git commit -m "refactor: update filter.rs to use min_bit_rate/min_bit_depth (u32)"
```

---

### Task 5: Update download.rs to use new field names

**Files:**
- Modify: `src/download.rs`

Update all references to `min_bitrate`/`min_bitdepth` in download verification logic and tests.

- [ ] **Step 1: Write the failing tests**

Update existing tests to use new field names. In `src/download.rs mod tests`, change all occurrences:

```rust
// Old pattern:
min_bitrate: Some(320),
min_bitdepth: None,

// New pattern:
min_bit_rate: 320,
min_bit_depth: 0,
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test download:: -- --nocapture 2>&1 | grep "error" | head -10`
Expected: FAIL with "no field `min_bitrate` on type `&FilterConfig`"

- [ ] **Step 3: Update verify_downloaded_quality**

In `src/download.rs`, change `verify_downloaded_quality()` (around line 350-373):

```rust
// Current:
if let Some(min_br) = filters.min_bitrate {
    if !file.attribs.contains_key(&0) {
        if let Some(actual_br) = crate::organizer::extract_bitrate(path) {
            if actual_br < min_br {
                return Err(SeakarrError::Download(format!(
                    "bitrate {actual_br} kbps below minimum {min_br} kbps"
                )));
            }
        }
    }
}
if let Some(min_bd) = filters.min_bitdepth {
    if !file.attribs.contains_key(&5) {
        if let Some(actual_bd) = crate::organizer::extract_bitdepth(path) {
            if actual_bd < min_bd {
                return Err(SeakarrError::Download(format!(
                    "bitdepth {actual_bd} below minimum {min_bd}"
                )));
            }
        }
    }
}

// New:
if filters.min_bit_rate > 0 {
    if !file.attribs.contains_key(&0) {
        if let Some(actual_br) = crate::organizer::extract_bitrate(path) {
            if actual_br < filters.min_bit_rate {
                return Err(SeakarrError::Download(format!(
                    "bitrate {actual_br} kbps below minimum {} kbps",
                    filters.min_bit_rate
                )));
            }
        }
    }
}
if filters.min_bit_depth > 0 {
    if !file.attribs.contains_key(&5) {
        if let Some(actual_bd) = crate::organizer::extract_bitdepth(path) {
            if actual_bd < filters.min_bit_depth {
                return Err(SeakarrError::Download(format!(
                    "bitdepth {actual_bd} below minimum {}",
                    filters.min_bit_depth
                )));
            }
        }
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test download:: -- --nocapture 2>&1 | tail -10`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add src/download.rs
git commit -m "refactor: update download.rs to use min_bit_rate/min_bit_depth (u32)"
```

---

### Task 6: Final verification — full test suite

- [ ] **Step 1: Run full test suite**

Run: `cargo test 2>&1 | tail -20`
Expected: All tests pass (431+ tests, 0 failures)

- [ ] **Step 2: Run clippy**

Run: `cargo clippy --all-targets -- -D warnings 2>&1 | tail -10`
Expected: Clean (no warnings)

- [ ] **Step 3: Run format check**

Run: `cargo fmt --check 2>&1`
Expected: Clean (no diff)

- [ ] **Step 4: Final commit if needed**

```bash
git add -A
git commit -m "feat: complete config rename min_bitrate/min_bitdepth to min_bit_rate/min_bit_depth"
```
