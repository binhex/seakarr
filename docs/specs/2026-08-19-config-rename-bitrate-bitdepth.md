# Config Rename: min_bitrate → min_bit_rate, min_bitdepth → min_bit_depth

**Date:** 2026-08-19
**Status:** Approved

## Problem

The current config fields `min_bitrate` and `min_bitdepth` in `FilterConfig` are `Option<u32>` with `null` defaults in the YAML config. This looks odd in the config file and the `Option<u32>` type adds complexity throughout the codebase (`if let Some(min_br) = ...` everywhere).

## Goal

Rename the config fields to `min_bit_rate` and `min_bit_depth`, change the type from `Option<u32>` to `u32` (with 0 = disabled), and migrate existing configs to preserve user values.

## Design

### Config Field Changes

**Current:**
```rust
pub min_bitrate: Option<u32>,    // default: None (null in YAML)
pub min_bitdepth: Option<u32>,   // default: None (null in YAML)
```

**New:**
```rust
#[serde(default)]
pub min_bit_rate: u32,    // default: 0 (disabled). >0 = minimum kbps for lossy files
#[serde(default)]
pub min_bit_depth: u32,   // default: 0 (disabled). >0 = minimum bit depth for lossless files
```

**YAML default:**
```yaml
min_bit_rate: 0      # Minimum bitrate (kbps) for lossy files. 0 = disabled.
min_bit_depth: 0     # Minimum bit depth for lossless files. 0 = disabled.
```

### Semantic Change

| Old | New | Meaning |
|-----|-----|---------|
| `min_bitrate: None` | `min_bit_rate: 0` | Disabled (no check) |
| `min_bitrate: Some(320)` | `min_bit_rate: 320` | Minimum 320 kbps |
| `min_bitdepth: None` | `min_bit_depth: 0` | Disabled (no check) |
| `min_bitdepth: Some(16)` | `min_bit_depth: 16` | Minimum 16-bit |

### Code Changes

**Pattern change:** All `if let Some(min_br) = config.min_bitrate { ... }` becomes `if config.min_bit_rate > 0 { ... }`.

**Files affected:**

| File | Changes |
|------|---------|
| `src/config.rs` | Rename fields, change type to `u32`, add `#[serde(default)]`, update example YAML, add migration logic |
| `src/filter.rs` | Update `file_passes_filters()`, `rank_candidates()`, `summarize_rejections()`, `FilterRejectionSummary` field name, all tests |
| `src/download.rs` | Update `verify_downloaded_quality()`, all tests |
| `src/runner.rs` | No change needed (`album.min_bitrate` is a different struct) |
| `src/organizer.rs` | No change needed (helpers don't reference config fields directly) |
| `tests/pipeline_test.rs` | Update any tests that set `min_bitrate`/`min_bitdepth` |

### Config Migration

The `reconcile_config_file()` function needs a migration step before the merge to preserve existing user values:

```rust
fn reconcile_config_file(config_file: &Path, contents: &str) -> Result<()> {
    // ... existing logic to parse file ...

    // Migration: rename min_bitrate → min_bit_rate, min_bitdepth → min_bit_depth
    // Preserves existing values (e.g., min_bitrate: 320 becomes min_bit_rate: 320)
    migrate_rename(&mut file_value, "filters", "min_bitrate", "min_bit_rate");
    migrate_rename(&mut file_value, "filters", "min_bitdepth", "min_bit_depth");

    let merged = merge_with_defaults(&default_value, &file_value);
    // ... rest of existing logic ...
}

fn migrate_rename(config: &mut serde_yaml::Value, section: &str, old_key: &str, new_key: &str) {
    // Find the section, read old value, set new key, remove old key
}
```

**Migration behavior:**
- Old config with `min_bitrate: 320` → migrated to `min_bit_rate: 320`
- Old config with `min_bitrate: null` → migrated to `min_bit_rate: 0`
- Old config without `min_bitrate` → `min_bit_rate` gets default (0)
- Old config key `min_bitrate` is removed from the output

### Non-goals

- No change to `AlbumInfo.min_bitrate` in scanner.rs (different concept — the actual minimum bitrate of the album's tracks, not the config threshold)
- No change to the download-then-verify behavior (just the config field names/types)
- No new config options

### Testing Strategy

**Unit tests (in `src/config.rs`):**
- `test_migrate_rename_preserves_value`: Old config with `min_bitrate: 320` → `min_bit_rate: 320`
- `test_migrate_rename_null_becomes_zero`: Old config with `min_bitrate: null` → `min_bit_rate: 0`
- `test_migrate_rename_missing_key`: Old config without `min_bitrate` → `min_bit_rate: 0`
- `test_migrate_rename_removes_old_key`: Old config key `min_bitrate` is removed

**Existing tests updated:**
- All tests in `src/filter.rs`, `src/download.rs` that set `min_bitrate`/`min_bitdepth` → use new names and `0` instead of `None`
