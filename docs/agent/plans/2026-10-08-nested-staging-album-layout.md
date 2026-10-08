<!-- markdownlint-disable MD013 -->

# Nested staging album layout Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use sub-agents (recommended) to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stage every download under `<storage.staging_dir>/<artist>/<album>/` instead of `<storage.staging_dir>/<artist>--<album>/`, migrating existing legacy directories on startup.

**Architecture:** A new `src/staging.rs` owns the layout rule (`artist_dir`, `album_dir`), the startup migration (`normalise_layout`, `prepare`) and the staging scan (`scan`, which resolves a sanitised directory name back to the raw `processed_albums` row). `organizer.rs` keeps the sanitiser and the library half of recovery; `runner.rs` and `main.rs` call `staging::prepare` from each mode entry point and derive every album staging path from `staging::album_dir`.

**Tech Stack:** Rust 2024, `rusqlite` (SQLite), `tokio`, `tracing`, `tempfile` for tests. No new dependency.

**Spec:** `docs/agent/specs/2026-10-08-nested-staging-album-layout-design.md` (approved, commit `542309e`).

> **Plan location note:** the invoking chain asked for `docs/plans/`. The repository convention (`AGENTS.md` rule 7, and every plan since `docs/agent/plans/2026-09-09-queue-aware-download-timeouts.md`) is `docs/agent/plans/`. This plan follows the repository convention; `docs/plans/` is a legacy directory holding pre-convention plans only.

---

## Scope Check

Single subsystem. The spec changes the staging path layout in one crate, with one new module. It is not separable into independent plans: the construction site, the decode sites, the migration and the recovery lookup must land together or staging holds two layouts at once and recovery deletes real albums as debris. **No split.**

## File Structure

| File | Responsibility after this change |
| --- | --- |
| `src/staging.rs` (new) | The staging layout rule, the legacy migration, and the staging scan with identity resolution. |
| `src/lib.rs` (modify) | Declare `pub mod staging;`. |
| `src/organizer.rs` (modify) | Keeps `sanitize_component`, `parse_album_slug` (legacy parser), `resume_library_upgrade`. `disc_subdir` gains an album-component parameter; `recover_interrupted_upgrades` iterates `staging::scan`. |
| `src/runner.rs` (modify) | `process_album_internal` builds the nested album path; `finish_library_write` and `discard_refused_staging` prune the artist level; three mode entry points call `staging::prepare`. |
| `src/main.rs` (modify) | `run_batch_mode` calls `staging::prepare`. |
| `src/download.rs` (modify) | One added test assertion only. Production code unchanged. |
| `tests/pipeline_test.rs` (modify) | The end-to-end staging fixture expects the nested tree. |

Order matters: Tasks 1-5 build `staging.rs` with no production caller, so the suite stays green. Task 6 changes `disc_subdir` (its one caller is updated in the same task). Task 7 switches recovery over. Task 8 switches the runner and sweeps the runner test fixtures. Task 9 wires the mode entry points. Task 10 finishes the end-to-end and documentation work.

---

### Task 1: `staging.rs` foundation — `artist_dir` and `album_dir`

**Files:**

- Create: `src/staging.rs`
- Modify: `src/lib.rs:5-23` (module declarations)

- [ ] **Step 1: Create the module with the API stubbed so the tests can compile**

Create `src/staging.rs`:

```rust
//! The staging album layout.
//!
//! A staged album is two real directories — `<staging_dir>/<artist>/<album>` —
//! rather than one flattened `<artist>--<album>` name. Both components are
//! produced by [`crate::organizer::sanitize_component`], the same sanitiser
//! every library write uses, so the staging tree mirrors the library tree for
//! the same album.

use std::path::{Path, PathBuf};

/// The album component used when a run names no album. Matches the value the
/// flattened layout used, so an artist-only run's staging directory keeps the
/// name an operator already recognises.
pub const UNKNOWN_ALBUM: &str = "unknown";

/// `<staging_dir>/<sanitized artist>`.
pub fn artist_dir(staging_dir: &Path, artist: &str) -> PathBuf {
    todo!("sanitize the artist component")
}

/// `<staging_dir>/<sanitized artist>/<sanitized album>`.
///
/// `album: None` keeps the [`UNKNOWN_ALBUM`] component an artist-only run used
/// under the flattened layout.
pub fn album_dir(staging_dir: &Path, artist: &str, album: Option<&str>) -> PathBuf {
    todo!("sanitize both components")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::organizer::sanitize_component;

    #[test]
    fn artist_dir_sanitises_the_artist_component() {
        assert_eq!(
            artist_dir(Path::new("/staging"), "AC/DC"),
            Path::new("/staging/AC-DC"),
            "a path separator in an artist name must not create a level"
        );
    }

    #[test]
    fn album_dir_sanitises_the_album_component() {
        assert_eq!(
            album_dir(Path::new("/staging"), "Artist", Some("Gold: Greatest Hits")),
            Path::new("/staging/Artist/Gold Greatest Hits"),
            "a Windows-reserved character is removed by the library sanitiser"
        );
    }

    #[test]
    fn album_dir_uses_the_placeholder_for_a_component_that_sanitises_away() {
        assert_eq!(
            album_dir(Path::new("/staging"), "???", Some("Album")),
            Path::new("/staging/_/Album"),
            "a component that keeps nothing becomes the sanitiser's placeholder"
        );
    }

    #[test]
    fn album_dir_neutralises_a_reserved_device_name() {
        assert_eq!(
            album_dir(Path::new("/staging"), "Artist", Some("CON")),
            Path::new("/staging/Artist/_CON"),
            "a reserved device name is prefixed by the library sanitiser"
        );
    }

    #[test]
    fn album_dir_keeps_the_unknown_component_when_no_album_is_named() {
        assert_eq!(
            album_dir(Path::new("/staging"), "Artist", None),
            Path::new("/staging/Artist/unknown"),
            "an artist-only run keeps the album component it always had"
        );
    }

    #[test]
    fn album_dir_makes_an_album_only_run_a_two_level_tree() {
        assert_eq!(
            album_dir(Path::new("/staging"), "", Some("Orphan Album")),
            Path::new("/staging/_/Orphan Album"),
            "a blank artist still gets an artist level, so the tree depth is constant"
        );
    }

    #[test]
    fn album_dir_matches_the_library_components_for_a_hostile_title() {
        let staging = Path::new("/staging");
        let artist = "Gold: Greatest Hits";
        let album = "A/B .. ???";
        assert_eq!(
            album_dir(staging, artist, Some(album)),
            staging
                .join(sanitize_component(artist))
                .join(sanitize_component(album)),
            "staging must use one sanitiser, the same one the library write uses"
        );
    }
}
```

Add the declaration to `src/lib.rs` in alphabetical position, between `search` and `tracks` is wrong — insert after `pub mod search;` is not alphabetical either. Insert it so the list stays sorted: `pub mod scan_progress;`, `pub mod scanner;`, `pub mod search;`, `pub mod staging;`, `pub mod tracks;`.

```rust
pub mod staging;
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib staging 2>&1 | tail -20`

Expected: the crate compiles and every `staging::tests` test FAILS by panicking with `not yet implemented: sanitize the artist component` (Task 1's `todo!`). This is the RED phase: the tests exist, the behaviour does not.

- [ ] **Step 3: Implement `artist_dir` and `album_dir`**

Replace the two stubbed bodies in `src/staging.rs`:

```rust
/// `<staging_dir>/<sanitized artist>`.
pub fn artist_dir(staging_dir: &Path, artist: &str) -> PathBuf {
    staging_dir.join(crate::organizer::sanitize_component(artist))
}

/// `<staging_dir>/<sanitized artist>/<sanitized album>`.
///
/// `album: None` keeps the [`UNKNOWN_ALBUM`] component an artist-only run used
/// under the flattened layout.
pub fn album_dir(staging_dir: &Path, artist: &str, album: Option<&str>) -> PathBuf {
    artist_dir(staging_dir, artist)
        .join(crate::organizer::sanitize_component(
            album.unwrap_or(UNKNOWN_ALBUM),
        ))
}
```

Remove the now-unused `use crate::organizer::sanitize_component;`? No — it stays, the test module uses it.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib staging 2>&1 | tail -20`

Expected: `test result: ok. 7 passed; 0 failed`.

- [ ] **Step 5: Verify the whole suite and the gates are unchanged**

Run: `cargo test 2>&1 | tail -15 && cargo fmt --all && cargo clippy --all-targets -- -D warnings 2>&1 | tail -15`

Expected: the suite passes at its pre-change count, `clippy` reports no warnings.

- [ ] **Step 6: Commit**

```bash
git add src/staging.rs src/lib.rs
git commit -m "feat: add the nested staging path constructor"
```

---

### Task 2: `normalise_layout` — rename legacy directories and prune empty artist levels

**Files:**

- Modify: `src/staging.rs`
- Test: `src/staging.rs` (same file, `#[cfg(test)] mod tests`)

- [ ] **Step 1: Write the failing tests**

Append to `mod tests` in `src/staging.rs`:

```rust
    use tempfile::TempDir;

    #[test]
    fn normalise_layout_renames_a_legacy_slug_into_two_levels() {
        let root = TempDir::new().unwrap();
        let legacy = root.path().join("Artist--Album");
        std::fs::create_dir_all(legacy.join("CD 01")).unwrap();
        std::fs::write(legacy.join("CD 01").join("01 - One.flac"), b"audio").unwrap();
        std::fs::write(legacy.join("cover.jpg"), b"image").unwrap();

        let report = normalise_layout(root.path()).unwrap();

        assert_eq!(
            report.migrated,
            vec![legacy.clone()],
            "a legacy slug with no nested destination is renamed wholesale"
        );
        assert!(!legacy.exists(), "the legacy directory must be gone");
        assert_eq!(
            std::fs::read(root.path().join("Artist").join("Album").join("cover.jpg")).unwrap(),
            b"image"
        );
        assert!(
            root.path()
                .join("Artist")
                .join("Album")
                .join("CD 01")
                .join("01 - One.flac")
                .exists(),
            "a nested disc folder keeps its shape"
        );
    }

    #[test]
    fn normalise_layout_prunes_an_empty_top_level_directory() {
        let root = TempDir::new().unwrap();
        let empty = root.path().join("Empty Artist");
        std::fs::create_dir_all(&empty).unwrap();

        let report = normalise_layout(root.path()).unwrap();

        assert_eq!(report.pruned_artist_dirs, vec![empty.clone()]);
        assert!(!empty.exists(), "an empty artist level must not accumulate");
    }

    #[test]
    fn normalise_layout_leaves_a_populated_artist_directory_alone() {
        let root = TempDir::new().unwrap();
        let artist = root.path().join("Artist");
        std::fs::create_dir_all(artist.join("Album")).unwrap();
        std::fs::write(artist.join("Album").join("01 - One.flac"), b"audio").unwrap();

        let report = normalise_layout(root.path()).unwrap();

        assert!(report.pruned_artist_dirs.is_empty());
        assert!(report.migrated.is_empty());
        assert!(artist.join("Album").join("01 - One.flac").exists());
    }

    #[test]
    fn normalise_layout_creates_a_missing_staging_root() {
        let root = TempDir::new().unwrap();
        let missing = root.path().join("not-created-yet");

        let report = prepare(&missing).unwrap();

        assert!(missing.is_dir(), "prepare must create the staging root");
        assert!(report.migrated.is_empty());
        assert!(report.failed.is_empty());
    }

    #[test]
    fn prepare_over_an_already_nested_tree_changes_nothing() {
        let root = TempDir::new().unwrap();
        let album = root.path().join("Artist").join("Album");
        std::fs::create_dir_all(&album).unwrap();
        std::fs::write(album.join("01 - One.flac"), b"audio").unwrap();

        let first = prepare(root.path()).unwrap();
        let second = prepare(root.path()).unwrap();

        assert!(first.migrated.is_empty() && first.merged.is_empty());
        assert!(second.migrated.is_empty() && second.merged.is_empty());
        assert!(album.join("01 - One.flac").exists(), "prepare is idempotent");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib staging 2>&1 | tail -25`

Expected: FAIL to compile — `cannot find function normalise_layout`, `cannot find function prepare`, `cannot find type MigrationReport`. That is the RED phase for this task (the tests name behaviour that does not exist). If the crate does not compile, noting the exact unresolved names is the verification.

- [ ] **Step 3: Implement `MigrationReport`, `normalise_layout` and `prepare`**

Add above `#[cfg(test)]` in `src/staging.rs`:

```rust
/// What a [`normalise_layout`] pass changed, and what it could not.
///
/// Every field is a record for a warning, not an error: a wasted rename must
/// never abort a download run.
#[derive(Debug, Default)]
pub struct MigrationReport {
    /// Legacy directories renamed wholesale: the nested destination was absent.
    pub migrated: Vec<PathBuf>,
    /// Legacy directories merged into a destination that already existed.
    pub merged: Vec<PathBuf>,
    /// A legacy directory left in place, with the first entry that stopped it.
    pub left_in_place: Vec<(PathBuf, PathBuf)>,
    /// A top-level directory that is neither a legacy slug nor an artist directory.
    pub unrecognised: Vec<PathBuf>,
    /// Names that are not valid UTF-8; renaming them lossily could orphan an album.
    pub non_utf8: Vec<PathBuf>,
    /// A directory whose rewrite failed, with the error.
    pub failed: Vec<(PathBuf, String)>,
    /// Empty artist directories removed.
    pub pruned_artist_dirs: Vec<PathBuf>,
}

impl MigrationReport {
    /// Log every anomaly at the level an operator sees at startup.
    fn log_anomalies(&self) {
        for (path, error) in &self.failed {
            tracing::warn!("Staging migration could not rewrite {path:?}: {error}");
        }
        for path in &self.non_utf8 {
            tracing::warn!(
                "Staging migration left {path:?} untouched: its name is not valid UTF-8, and renaming it lossily could orphan an album"
            );
        }
        for (path, first) in &self.left_in_place {
            tracing::warn!(
                "Staging migration left {path:?} in place: {first:?} already exists at the destination and was not overwritten"
            );
        }
        for path in &self.unrecognised {
            tracing::warn!(
                "Staging holds {path:?}, which is neither a legacy artist--album directory nor an artist directory; leaving it untouched"
            );
        }
    }
}

/// Create the staging root if it is missing, then bring its layout up to date.
///
/// The one call a run makes before it stages anything. An anomaly is logged,
/// never returned as an error: the run's own failure surface is unchanged.
pub fn prepare(staging_dir: &Path) -> Result<MigrationReport> {
    std::fs::create_dir_all(staging_dir)?;
    let report = normalise_layout(staging_dir)?;
    report.log_anomalies();
    Ok(report)
}

/// Rename legacy `<artist>--<album>` directories into `<artist>/<album>` and
/// remove artist directories that hold nothing. A missing staging root is not
/// an error: there is nothing stale in a directory that does not exist.
pub fn normalise_layout(staging_dir: &Path) -> Result<MigrationReport> {
    let mut report = MigrationReport::default();
    let entries = match std::fs::read_dir(staging_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(report),
        Err(e) => return Err(e.into()),
    };

    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                report
                    .failed
                    .push((staging_dir.to_path_buf(), e.to_string()));
                continue;
            }
        };
        if !entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
            continue;
        }
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()).map(str::to_owned) else {
            report.non_utf8.push(path);
            continue;
        };

        if !name.contains("--") {
            prune_or_report_artist_dir(&path, &mut report);
            continue;
        }

        let (artist, album) = crate::organizer::parse_album_slug(&name);
        let destination = artist_dir(staging_dir, &artist).join(
            crate::organizer::sanitize_component(&album),
        );
        if !destination.exists() {
            let parent = destination.parent().unwrap_or(staging_dir);
            match std::fs::create_dir_all(parent).and_then(|()| std::fs::rename(&path, &destination))
            {
                Ok(()) => report.migrated.push(path),
                Err(e) => report.failed.push((path, e.to_string())),
            }
        } else {
            report.left_in_place.push((path, destination));
        }
    }

    Ok(report)
}

/// Remove a top-level directory that holds nothing; report one that holds files
/// but no subdirectory, because in a nested layout that shape is ambiguous and
/// deleting real content is worse than leaving junk.
fn prune_or_report_artist_dir(path: &Path, report: &mut MigrationReport) {
    let Ok(entries) = std::fs::read_dir(path) else {
        report.failed.push((path.to_path_buf(), "unreadable".to_string()));
        return;
    };
    let mut holds_anything = false;
    let mut holds_a_subdirectory = false;
    for entry in entries.flatten() {
        holds_anything = true;
        if entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
            holds_a_subdirectory = true;
        }
    }
    if !holds_anything {
        match std::fs::remove_dir(path) {
            Ok(()) => report.pruned_artist_dirs.push(path.to_path_buf()),
            Err(e) => report.failed.push((path.to_path_buf(), e.to_string())),
        }
    } else if !holds_a_subdirectory {
        report.unrecognised.push(path.to_path_buf());
    }
}
```

Add `use crate::error::Result;` to the imports at the top of `src/staging.rs`.

Note: Task 2 deliberately records a wholesale-rename failure under `failed`; the merge case is Task 3. The `left_in_place` push above is a placeholder for a destination that already exists — Task 3 replaces it with the merge.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib staging 2>&1 | tail -20`

Expected: `test result: ok. 12 passed; 0 failed`.

- [ ] **Step 5: Run the gates**

Run: `cargo fmt --all && cargo clippy --all-targets -- -D warnings 2>&1 | tail -15 && cargo test 2>&1 | tail -8`

Expected: no clippy warnings, suite passes.

- [ ] **Step 6: Commit**

```bash
git add src/staging.rs
git commit -m "feat: migrate legacy staging slugs into nested directories"
```

---

### Task 3: `normalise_layout` — merge without overwriting

**Files:**

- Modify: `src/staging.rs`

- [ ] **Step 1: Write the failing tests**

Append to `mod tests` in `src/staging.rs`:

```rust
    #[test]
    fn normalise_layout_merges_into_an_existing_destination_without_overwriting() {
        let root = TempDir::new().unwrap();
        let legacy = root.path().join("Artist--Album");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("collision.flac"), b"legacy copy").unwrap();
        std::fs::write(legacy.join("only-legacy.flac"), b"moves").unwrap();

        let destination = root.path().join("Artist").join("Album");
        std::fs::create_dir_all(&destination).unwrap();
        std::fs::write(destination.join("collision.flac"), b"destination copy").unwrap();

        let report = normalise_layout(root.path()).unwrap();

        assert_eq!(
            std::fs::read(destination.join("collision.flac")).unwrap(),
            b"destination copy",
            "the destination file must win"
        );
        assert_eq!(
            std::fs::read(destination.join("only-legacy.flac")).unwrap(),
            b"moves",
            "an entry absent at the destination still moves"
        );
        assert_eq!(
            report.left_in_place,
            vec![(legacy.clone(), legacy.join("collision.flac"))],
            "a directory still holding a skipped entry is reported, with the entry that stopped it"
        );
        assert!(
            legacy.join("collision.flac").exists(),
            "the skipped legacy copy is left where it is"
        );
        assert!(report.merged.is_empty(), "an incomplete merge is not a merge");
    }

    #[test]
    fn normalise_layout_removes_a_legacy_directory_the_merge_emptied() {
        let root = TempDir::new().unwrap();
        let legacy = root.path().join("Artist--Album");
        std::fs::create_dir_all(legacy.join("CD 01")).unwrap();
        std::fs::write(legacy.join("CD 01").join("01 - One.flac"), b"audio").unwrap();
        std::fs::write(legacy.join("cover.jpg"), b"image").unwrap();

        let destination = root.path().join("Artist").join("Album");
        std::fs::create_dir_all(&destination).unwrap();
        std::fs::write(destination.join("notes.txt"), b"notes").unwrap();

        let report = normalise_layout(root.path()).unwrap();

        assert_eq!(report.merged, vec![legacy.clone()]);
        assert!(!legacy.exists(), "an emptied legacy directory is removed");
        assert!(
            destination.join("CD 01").join("01 - One.flac").exists(),
            "a nested disc folder keeps its shape through the merge"
        );
        assert_eq!(std::fs::read(destination.join("cover.jpg")).unwrap(), b"image");
    }

    #[test]
    fn normalise_layout_leaves_a_byte_invalid_name_alone() {
        #[cfg(unix)]
        {
            use std::ffi::OsString;
            use std::os::unix::ffi::OsStringExt;

            let root = TempDir::new().unwrap();
            let invalid = root
                .path()
                .join(OsString::from_vec(b"Artist\xff--Album".to_vec()));
            std::fs::create_dir_all(&invalid).unwrap();

            let report = normalise_layout(root.path()).unwrap();

            assert_eq!(report.non_utf8, vec![invalid.clone()]);
            assert!(invalid.exists(), "a lossy rename could orphan an album");
        }
    }

    #[test]
    fn normalise_layout_reports_a_top_level_directory_holding_only_files() {
        let root = TempDir::new().unwrap();
        let ambiguous = root.path().join("stray-files");
        std::fs::create_dir_all(&ambiguous).unwrap();
        std::fs::write(ambiguous.join("01 - One.flac"), b"audio").unwrap();

        let report = normalise_layout(root.path()).unwrap();

        assert_eq!(report.unrecognised, vec![ambiguous.clone()]);
        assert!(ambiguous.exists(), "deleting real content is worse than leaving junk");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib staging 2>&1 | tail -30`

Expected: `normalise_layout_merges_into_an_existing_destination_without_overwriting` FAILS — the current code pushes the destination into `left_in_place` without moving `only-legacy.flac`, so `std::fs::read(destination.join("only-legacy.flac"))` panics. `normalise_layout_removes_a_legacy_directory_the_merge_emptied` FAILS on `assert_eq!(report.merged, ...)`. The byte-invalid and stray-files tests should already PASS from Task 2 — if they fail, the Task 2 implementation is wrong and must be fixed before continuing.

- [ ] **Step 3: Implement the merge**

In `src/staging.rs`, replace the `else` arm of the legacy branch in `normalise_layout`:

```rust
        } else {
            match merge_without_overwrite(&path, &destination) {
                Ok(MergeOutcome::Emptied) => {
                    match std::fs::remove_dir(&path) {
                        Ok(()) => report.merged.push(path),
                        Err(e) => report.failed.push((path, e.to_string())),
                    }
                }
                Ok(MergeOutcome::Skipped(first)) => report.left_in_place.push((path, first)),
                Err(e) => report.failed.push((path, e.to_string())),
            }
        }
```

and add:

```rust
/// What a merge left behind.
enum MergeOutcome {
    /// Every entry moved; the legacy directory is now empty.
    Emptied,
    /// A name already existed at the destination. Nothing was overwritten, and
    /// the first colliding source entry is named for the warning.
    Skipped(PathBuf),
}

/// Move every entry of `legacy` that is absent at `destination`, recursing so a
/// nested disc folder keeps its shape. A name that already exists at the
/// destination is never overwritten: the destination entry stays and the legacy
/// copy is left where it is.
fn merge_without_overwrite(legacy: &Path, destination: &Path) -> std::io::Result<MergeOutcome> {
    let mut first_skipped: Option<PathBuf> = None;
    for entry in std::fs::read_dir(legacy)? {
        let entry = entry?;
        let source = entry.path();
        let target = destination.join(entry.file_name());
        if target.exists() {
            first_skipped.get_or_insert(source);
            continue;
        }
        if entry.file_type()?.is_dir() {
            std::fs::create_dir_all(&target)?;
            if let MergeOutcome::Skipped(first) = merge_without_overwrite(&source, &target)? {
                first_skipped.get_or_insert(first);
            } else {
                let _ = std::fs::remove_dir(&source);
            }
        } else {
            std::fs::rename(&source, &target)?;
        }
    }
    Ok(match first_skipped {
        Some(first) => MergeOutcome::Skipped(first),
        None => MergeOutcome::Emptied,
    })
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib staging 2>&1 | tail -20`

Expected: `test result: ok. 16 passed; 0 failed`.

- [ ] **Step 5: Run the gates**

Run: `cargo fmt --all && cargo clippy --all-targets -- -D warnings 2>&1 | tail -15 && cargo test 2>&1 | tail -8`

Expected: no clippy warnings, suite passes.

- [ ] **Step 6: Commit**

```bash
git add src/staging.rs
git commit -m "feat: merge a migrated staging directory without overwriting"
```

---

### Task 4: `staging::scan` — resolve a sanitised directory back to a raw album row

**Files:**

- Modify: `src/staging.rs`

- [ ] **Step 1: Write the failing tests**

Append to `mod tests` in `src/staging.rs`:

```rust
    use crate::db::Database;

    #[test]
    fn scan_resolves_a_sanitised_directory_name_to_the_raw_row() {
        let root = TempDir::new().unwrap();
        std::fs::create_dir_all(root.path().join("Artist").join("Gold Greatest Hits")).unwrap();
        let db = Database::open_in_memory().unwrap();
        db.mark_album_processed("Artist", "Gold: Greatest Hits", "success")
            .unwrap();

        let staged = scan(root.path(), &db).unwrap();

        assert_eq!(staged.len(), 1);
        assert_eq!(staged[0].dir_album, "Gold Greatest Hits");
        let identity = staged[0].identity.as_ref().expect("a matching row");
        assert_eq!(identity.artist, "Artist", "the raw name is what recovery needs");
        assert_eq!(identity.album, "Gold: Greatest Hits");
        assert_eq!(identity.status, "success");
    }

    #[test]
    fn scan_matches_case_insensitively_like_the_database_lookup() {
        let root = TempDir::new().unwrap();
        std::fs::create_dir_all(root.path().join("artist").join("album")).unwrap();
        let db = Database::open_in_memory().unwrap();
        db.mark_album_processed("ARTIST", "ALBUM", "failed").unwrap();

        let staged = scan(root.path(), &db).unwrap();

        assert!(staged[0].identity.is_some(), "SQLite COLLATE NOCASE is ASCII-insensitive");
    }

    #[test]
    fn scan_returns_no_identity_for_an_untracked_album_directory() {
        let root = TempDir::new().unwrap();
        std::fs::create_dir_all(root.path().join("_").join("Album")).unwrap();
        let db = Database::open_in_memory().unwrap();

        let staged = scan(root.path(), &db).unwrap();

        assert_eq!(staged.len(), 1, "an album-only leftover is still an album");
        assert!(staged[0].identity.is_none());
        assert_eq!(staged[0].dir_album, "Album");
    }

    #[test]
    fn scan_prefers_the_highest_id_when_several_rows_match() {
        let root = TempDir::new().unwrap();
        std::fs::create_dir_all(root.path().join("Artist").join("Album")).unwrap();
        let db = Database::open_in_memory().unwrap();
        // Two raw names that sanitise to the same two directory names.
        db.mark_album_processed("Artist", "Album", "failed").unwrap();
        db.mark_album_processed("Artist", "Album?", "success")
            .unwrap();

        let staged = scan(root.path(), &db).unwrap();

        let identity = staged[0].identity.as_ref().expect("a matching row");
        assert_eq!(
            identity.status, "success",
            "the highest id is the most recently tried album"
        );
        assert_eq!(identity.album, "Album?");
    }

    #[test]
    fn scan_reports_every_album_level_under_one_artist() {
        let root = TempDir::new().unwrap();
        std::fs::create_dir_all(root.path().join("Artist").join("One")).unwrap();
        std::fs::create_dir_all(root.path().join("Artist").join("Two")).unwrap();
        let db = Database::open_in_memory().unwrap();

        let mut names: Vec<String> = scan(root.path(), &db)
            .unwrap()
            .into_iter()
            .map(|album| album.dir_album)
            .collect();
        names.sort();

        assert_eq!(names, vec!["One".to_string(), "Two".to_string()]);
    }

    #[test]
    fn scan_tolerates_a_missing_staging_root() {
        let root = TempDir::new().unwrap();
        let db = Database::open_in_memory().unwrap();

        assert!(scan(&root.path().join("absent"), &db).unwrap().is_empty());
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib staging 2>&1 | tail -30`

Expected: FAIL to compile — `cannot find function scan`, `cannot find type StagedAlbum`, `cannot find type StagedIdentity`.

- [ ] **Step 3: Implement `StagedAlbum`, `StagedIdentity` and `scan`**

Add to `src/staging.rs`:

```rust
/// One album found under the staging root.
#[derive(Debug)]
pub struct StagedAlbum {
    /// `<staging_dir>/<artist>` — the level to prune when the album leaves.
    pub artist_dir_path: PathBuf,
    /// `<staging_dir>/<artist>/<album>`.
    pub album_dir_path: PathBuf,
    /// The album directory name as it is on disk (the sanitised component).
    pub dir_album: String,
    /// `None` when no `processed_albums` row matches: an untracked leftover.
    pub identity: Option<StagedIdentity>,
}

/// Raw names and status, as stored in the database.
#[derive(Debug)]
pub struct StagedIdentity {
    pub artist: String,
    pub album: String,
    pub status: String,
}

/// Walk the two levels under the staging root and resolve each album's identity.
///
/// The directory names are sanitised while the database rows are raw, so an
/// exact lookup would miss a title such as `Gold: Greatest Hits` and the
/// leftover would be deleted as debris. Rows are matched by sanitising both
/// sides, case-insensitively, which is what the database's own `COLLATE NOCASE`
/// lookup does.
pub fn scan(staging_dir: &Path, db: &crate::db::Database) -> Result<Vec<StagedAlbum>> {
    let rows = db.get_processed_albums()?;
    let mut albums = Vec::new();
    let artist_entries = match std::fs::read_dir(staging_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(albums),
        Err(e) => return Err(e.into()),
    };

    for artist_entry in artist_entries {
        let artist_entry = artist_entry?;
        if !artist_entry.file_type()?.is_dir() {
            continue;
        }
        let artist_dir_path = artist_entry.path();
        let Some(artist_name) = artist_dir_path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        for album_entry in std::fs::read_dir(&artist_dir_path)? {
            let album_entry = album_entry?;
            if !album_entry.file_type()?.is_dir() {
                continue;
            }
            let album_dir_path = album_entry.path();
            let Some(album_name) = album_dir_path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            albums.push(StagedAlbum {
                artist_dir_path: artist_dir_path.clone(),
                album_dir_path,
                dir_album: album_name.to_string(),
                identity: resolve_identity(&rows, artist_name, album_name),
            });
        }
    }

    Ok(albums)
}

/// Find the `processed_albums` row a staging directory belongs to. The highest
/// `id` wins when several raw names sanitise to the same two directory names;
/// the ambiguity is warned about, so a shared sanitised name still recovers the
/// most recently tried album.
fn resolve_identity(
    rows: &[crate::db::ProcessedAlbum],
    artist_dir: &str,
    album_dir: &str,
) -> Option<StagedIdentity> {
    let mut matches: Vec<&crate::db::ProcessedAlbum> = rows
        .iter()
        .filter(|row| {
            crate::organizer::sanitize_component(&row.artist).eq_ignore_ascii_case(artist_dir)
                && crate::organizer::sanitize_component(&row.album).eq_ignore_ascii_case(album_dir)
        })
        .collect();
    matches.sort_by_key(|row| row.id);
    let (newest, older) = matches.split_last()?;
    if let Some(first) = older.first() {
        tracing::warn!(
            "Staging {artist_dir:?}/{album_dir:?} matches several processed albums; using the newest {:?} - {:?} (it also matches {:?} - {:?})",
            newest.artist,
            newest.album,
            first.artist,
            first.album
        );
    }
    Some(StagedIdentity {
        artist: newest.artist.clone(),
        album: newest.album.clone(),
        status: newest.status.clone(),
    })
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib staging 2>&1 | tail -20`

Expected: `test result: ok. 22 passed; 0 failed`.

- [ ] **Step 5: Run the gates**

Run: `cargo fmt --all && cargo clippy --all-targets -- -D warnings 2>&1 | tail -15 && cargo test 2>&1 | tail -8`

Expected: no clippy warnings, suite passes.

- [ ] **Step 6: Commit**

```bash
git add src/staging.rs
git commit -m "feat: resolve staged album identity by sanitised name"
```

---

### Task 5: `disc_subdir` stops at the album root by name

**Files:**

- Modify: `src/organizer.rs:513-536` (`disc_subdir`), `src/organizer.rs:426` (its one call site)
- Test: `src/organizer.rs:866-896`, `src/organizer.rs:1382-1400`

- [ ] **Step 1: Write the failing test**

Add to `mod tests` in `src/organizer.rs`, after `placement_keeps_a_staging_disc_subdirectory_under_the_album_folder`:

```rust
    #[test]
    fn disc_subdir_treats_an_album_named_like_a_disc_folder_as_the_root() {
        // The album root's own name can carry an embedded disc marker. The walk
        // must stop at the album root by name, so a "Disc 1" album root is never
        // mistaken for one of its own disc subdirectories.
        let root = Path::new("/staging/Artist/Disc 1");
        let flat = root.join("01 - One.flac");
        assert_eq!(disc_subdir(&flat, "Disc 1"), None);

        let disc = root.join("CD 01").join("01 - One.flac");
        assert_eq!(disc_subdir(&disc, "Disc 1"), Some("CD 01".to_string()));
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --lib disc_subdir 2>&1 | tail -20`

Expected: FAIL to compile — `disc_subdir` takes 1 argument but 2 were supplied. That is the RED phase.

- [ ] **Step 3: Add the album parameter and change the boundary test**

In `src/organizer.rs`, change the signature and the boundary check:

```rust
/// Return the disc subdirectory component of a staging source path, if the
/// file was downloaded into a per-disc folder — either a dedicated disc
/// folder ("CD 01", "Disc 2") or an album folder carrying an embedded disc
/// marker ("Gold (Disc 1)"). Walks the parent components and returns the
/// deepest disc folder component. Files flattened directly into the album
/// staging root return `None` so their destination is unchanged.
///
/// `album` is the album title the write is for. The walk stops when a parent
/// directory name equals the sanitised album title, because that names the
/// album staging root: the boundary is the album's own name, not a substring
/// of a flattened slug, so an album whose name contains the separator the old
/// layout used is no longer ambiguous.
///
/// The returned label is the peer's own folder name, so it is run through
/// [`sanitize_component`] before it is handed back: it becomes a real path
/// component in the library, and a peer-supplied name must not reach the
/// filesystem raw.
///
/// Uses [`crate::discs`] so the destination grouping matches the download
/// stager exactly.
fn disc_subdir(src: &Path, album: &str) -> Option<String> {
    let album_component = sanitize_component(album);
    let mut current = src.parent();
    while let Some(dir) = current {
        if let Some(name) = dir.file_name().and_then(|n| n.to_str()) {
            // The runner's per-album staging root is <staging>/<artist>/<album>.
            // Its name is the sanitised album title, which can itself carry an
            // embedded "(Disc N)" marker — so it must be recognised before the
            // disc-designator test, never mistaken for one of its own discs.
            if name == album_component {
                break;
            }
            if crate::discs::is_disc_designator(name) {
                // Return the innermost folder, closest to the file. Only the
                // classification is normalized across marker styles; the label
                // itself is the peer's own, so it goes through the sanitiser
                // before it becomes a library path component.
                return Some(sanitize_component(name));
            }
        }
        current = dir.parent();
    }
    None
}
```

and update the one call site at `src/organizer.rs:426`:

```rust
        if let Some(disc) = disc_subdir(src, album) {
```

- [ ] **Step 4: Update the stale staging fixture in the placement test**

In `placement_keeps_a_staging_disc_subdirectory_under_the_album_folder` (`src/organizer.rs:866-896`), the fixture builds `Artist--Album/CD 01`. Change it to the nested shape and the album the test passes:

```rust
        let disc = staging.path().join("The Artist").join("The Album").join("CD 01");
```

and update the comment above the `display_layout_of_part_of_the_staging_tree`-style assertions at `src/organizer.rs:1390-1391` (the multi-disc copy test) so the comment names the nested layout:

```rust
        // Staging layout mirrors the runner: The Artist/The Album/CD 01/...,
        // The Artist/The Album/CD 02/...
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --lib disc_subdir 2>&1 | tail -15 && cargo test --lib organizer 2>&1 | tail -15`

Expected: both pass, including the new `disc_subdir_treats_an_album_named_like_a_disc_folder_as_the_root`.

- [ ] **Step 6: Run the gates**

Run: `cargo fmt --all && cargo clippy --all-targets -- -D warnings 2>&1 | tail -15 && cargo test 2>&1 | tail -8`

Expected: no clippy warnings, suite passes.

- [ ] **Step 7: Commit**

```bash
git add src/organizer.rs
git commit -m "refactor: stop the disc walk at the album root by name"
```

---

### Task 6: recovery iterates `staging::scan`

**Files:**

- Modify: `src/organizer.rs:770-812` (`recover_interrupted_upgrades`)
- Test: `src/organizer.rs:1809-1895` (the three recovery tests)

- [ ] **Step 1: Update the recovery tests to the nested fixture and add the sanitised-name case**

Replace the three recovery tests in `mod tests` in `src/organizer.rs` (`test_recover_interrupted_upgrades_resumes_success`, `..._cleans_failed_leftover`, `..._cleans_untracked_leftover`) with these four, all using nested fixtures:

```rust
    #[test]
    fn test_recover_interrupted_upgrades_resumes_success() {
        use crate::db::Database;

        let staging_root = TempDir::new().unwrap();
        let staging = staging_root.path().join("Artist").join("Album");
        fs::create_dir_all(&staging).unwrap();
        fs::write(staging.join("01 - Song.flac"), b"correct content").unwrap();

        let library = TempDir::new().unwrap();
        let mut config = Config::default();
        config.library_upgrade.enabled = true;
        config.library.paths = vec![library.path().to_string_lossy().into_owned()];

        let db = Database::open_in_memory().unwrap();
        db.mark_album_processed("Artist", "Album", "success")
            .unwrap();

        recover_interrupted_upgrades(&config, &db, staging_root.path()).unwrap();

        assert!(library.path().join("Artist/Album/01 - Song.flac").exists());
        assert!(!staging.exists());
    }

    #[test]
    fn test_recover_interrupted_upgrades_resumes_an_album_with_a_sanitised_directory_name() {
        // The directory name is the sanitised title while the row holds the raw
        // title. An exact lookup would miss and the album would be deleted as
        // debris — the failure this match exists to prevent.
        use crate::db::Database;

        let staging_root = TempDir::new().unwrap();
        let staging = staging_root
            .path()
            .join("Artist")
            .join(sanitize_component("Gold: Greatest Hits"));
        fs::create_dir_all(&staging).unwrap();
        fs::write(staging.join("01 - Song.flac"), b"correct content").unwrap();

        let library = TempDir::new().unwrap();
        let mut config = Config::default();
        config.library_upgrade.enabled = true;
        config.library.paths = vec![library.path().to_string_lossy().into_owned()];

        let db = Database::open_in_memory().unwrap();
        db.mark_album_processed("Artist", "Gold: Greatest Hits", "success")
            .unwrap();

        recover_interrupted_upgrades(&config, &db, staging_root.path()).unwrap();

        assert!(
            library
                .path()
                .join("Artist")
                .join("Gold Greatest Hits")
                .join("01 - Song.flac")
                .exists(),
            "the raw row must drive the library destination, not the directory name"
        );
    }

    #[test]
    fn test_recover_interrupted_upgrades_cleans_failed_leftover() {
        // Regression (release-review Finding 7): a per-album staging dir left
        // behind by an interrupted DOWNLOAD (album status "failed") must be
        // removed, not ignored forever — otherwise the next download for the
        // same album reuses the directory and can interleave with stale
        // .part / partial files.
        use crate::db::Database;

        let staging_root = TempDir::new().unwrap();
        let staging = staging_root.path().join("Artist").join("Album");
        fs::create_dir_all(&staging).unwrap();
        fs::write(staging.join("01 - Song.flac.part"), b"partial bytes").unwrap();

        let library = TempDir::new().unwrap();
        let mut config = Config::default();
        config.library_upgrade.enabled = true;
        config.library.paths = vec![library.path().to_string_lossy().into_owned()];

        let db = Database::open_in_memory().unwrap();
        db.mark_album_processed("Artist", "Album", "failed")
            .unwrap();

        recover_interrupted_upgrades(&config, &db, staging_root.path()).unwrap();

        assert!(
            !staging.exists(),
            "failed-status leftover staging dir must be cleaned up"
        );
    }

    #[test]
    fn test_recover_interrupted_upgrades_cleans_untracked_leftover() {
        // The same, for a leftover whose album has NO DB record at all. An
        // album-only run stages under the sanitiser's `_` placeholder and
        // writes no row, because `mark_album_processed_if_identifiable`
        // returns early for a blank artist.
        use crate::db::Database;

        let staging_root = TempDir::new().unwrap();
        let staging = staging_root.path().join("_").join("Album");
        fs::create_dir_all(&staging).unwrap();
        fs::write(staging.join("cover.jpg"), b"image").unwrap();

        let library = TempDir::new().unwrap();
        let mut config = Config::default();
        config.library_upgrade.enabled = true;
        config.library.paths = vec![library.path().to_string_lossy().into_owned()];

        let db = Database::open_in_memory().unwrap(); // no record at all

        recover_interrupted_upgrades(&config, &db, staging_root.path()).unwrap();

        assert!(
            !staging.exists(),
            "untracked leftover staging dir must be cleaned up"
        );
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib recover_interrupted 2>&1 | tail -30`

Expected: `test_recover_interrupted_upgrades_resumes_success` FAILS — `recover_interrupted_upgrades` still reads the top level and calls `parse_album_slug("Artist")`, yielding artist `Artist` / album `Unknown`; no row matches, so the whole `Artist` directory (including `Album/01 - Song.flac`) is removed and `assert!(library...exists())` fails. `..._resumes_an_album_with_a_sanitised_directory_name` FAILS the same way. This is the RED phase and it demonstrates the data loss the change prevents.

- [ ] **Step 3: Implement the switch to `staging::scan`**

Replace `recover_interrupted_upgrades` in `src/organizer.rs`:

```rust
/// Recover interrupted library upgrades by scanning the staging directory for
/// leftover album directories. Directories whose album is marked "success"
/// in the DB are re-verified and copied into the library (crash during the
/// copy step). Every other leftover — an album marked "failed", an album
/// that could not be identified, or a directory whose run never recorded any
/// status — is the debris of an interrupted *download*: the album was never
/// fully staged, so the directory is removed. This keeps a later download
/// for the same album from interleaving with stale `.part` files or partial
/// tracks from the crashed run.
///
/// The identity of each album comes from [`crate::staging::scan`], which
/// matches the sanitised directory names against the raw names the database
/// stores, so a title the sanitiser rewrote is still recovered.
pub fn recover_interrupted_upgrades(
    config: &Config,
    db: &crate::db::Database,
    staging_dir: &Path,
) -> Result<()> {
    if !config.library_upgrade.enabled {
        return Ok(());
    }
    if config.library.paths.is_empty() {
        return Ok(());
    }
    let library_root = Path::new(&config.library.paths[0]);

    for staged in crate::staging::scan(staging_dir, db)? {
        let (artist, album) = match &staged.identity {
            Some(identity) => (identity.artist.clone(), identity.album.clone()),
            None => ("Unknown".to_string(), staged.dir_album.clone()),
        };
        match staged.identity.as_ref().map(|identity| identity.status.as_str()) {
            Some("success") => {
                tracing::warn!("Recovering interrupted library upgrade: {artist} - {album}");
                resume_library_upgrade(
                    config,
                    &staged.album_dir_path,
                    library_root,
                    &artist,
                    &album,
                )?;
            }
            // "in-progress" is never written by any code path (downloads
            // record "failed" on interruption), so this arm also covers
            // crashed downloads whose run ended with a recorded failure, a
            // non-success status, or no identity at all.
            _ => {
                tracing::info!(
                    "Cleaning up leftover staging from interrupted run: {artist} - {album}"
                );
                fs::remove_dir_all(&staged.album_dir_path)?;
            }
        }
    }
    Ok(())
}
```

Remove the now-unused `parse_album_slug` call from this function; keep `parse_album_slug` itself and its test (`test_parse_album_slug`) — the migration uses it. If clippy reports `parse_album_slug` as dead code, that is because Task 3's call is in `src/staging.rs`; check that `normalise_layout` calls `crate::organizer::parse_album_slug` and that the function is `pub`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib recover_interrupted 2>&1 | tail -20 && cargo test --lib organizer 2>&1 | tail -15`

Expected: all four recovery tests pass.

- [ ] **Step 5: Run the gates**

Run: `cargo fmt --all && cargo clippy --all-targets -- -D warnings 2>&1 | tail -15 && cargo test 2>&1 | tail -8`

Expected: no clippy warnings, suite passes.

- [ ] **Step 6: Commit**

```bash
git add src/organizer.rs
git commit -m "refactor: recover staged albums through the sanitised scan"
```

---

### Task 7: the runner builds and cleans the nested album path

**Files:**

- Modify: `src/runner.rs:508-516` (path construction), `src/runner.rs:325-340` (`remove_empty_staging_dirs`), `src/runner.rs:351-390` (`discard_refused_staging`), `src/runner.rs:392-420` (`finish_library_write`)

- [ ] **Step 1: Update the runner test fixtures to the expected nested layout**

These are the tests whose assertions name a staging path. Each edit turns a flattened name into two components, including the negative assertions — a `!exists()` on the old flattened path would pass vacuously after the change and silently stop testing anything.

| Line | Change |
| --- | --- |
| `src/runner.rs:3738` | `staging.path().join("Test Artist--Test Album")` → `staging.path().join("Test Artist").join("Test Album")` |
| `src/runner.rs:3822` | same as above |
| `src/runner.rs:4865` | `staging.path().join("Test Artist--Album One")` → `staging.path().join("Test Artist").join("Album One")` |
| `src/runner.rs:5045` | `staging.path().join("Test Artist--New Album")` → `staging.path().join("Test Artist").join("New Album")` |
| `src/runner.rs:5544` | `staging.path().join("Radiohead--In Rainbows")` → `staging.path().join("Radiohead").join("In Rainbows")` |
| `src/runner.rs:5691` | `staging.path().join("--Orphan Album 6a2b")` → `staging.path().join("_").join("Orphan Album 6a2b")` |
| `src/runner.rs:6629` | `staging.path().join("Test Artist--Missing")` → `staging.path().join("Test Artist").join("Missing")` |
| `src/runner.rs:6830` | `staging.path().join("Test Artist--Old Album")` → `staging.path().join("Test Artist").join("Old Album")` |
| `src/runner.rs:6883` | `staging.path().join("Alpha Artist--Missing")` → `staging.path().join("Alpha Artist").join("Missing")` |
| `src/runner.rs:6947` | `staging.path().join("Test Artist--Missing")` → `staging.path().join("Test Artist").join("Missing")` |
| `src/runner.rs:7033` | same as above |
| `src/runner.rs:7407` | `staging.path().join("Test Artist--Album").join("01 - track.flac")` → `staging.path().join("Test Artist").join("Album").join("01 - track.flac")` |
| `src/runner.rs:7479` | same as above |
| `src/runner.rs:7540` | `staging.path().join("Test Artist--Album")` → `staging.path().join("Test Artist").join("Album")` |
| `src/runner.rs:8829` | `staging.path().join("Shy Fx--Missing")` → `staging.path().join("Shy Fx").join("Missing")` |
| `src/runner.rs:8921` | `staging.path().join("Nobody--Missing")` → `staging.path().join("Nobody").join("Missing")` |
| `src/runner.rs:9212` | `staging.path().join("Test Artist--New Album")` → `staging.path().join("Test Artist").join("New Album")` |
| `src/runner.rs:9255` | same as above |
| `src/runner.rs:9472` | `staging.path().join("Test Artist--Test Album")` → `staging.path().join("Test Artist").join("Test Album")` |

Also update the wording of the assertion at `src/runner.rs:5691`, which currently says "the staging slug has no artist component":

```rust
        // The blank artist means the artist level is the sanitiser's `_`
        // placeholder, and the download must be there rather than anywhere in
        // the library.
        assert!(
            staging.path().join("_").join("Orphan Album 6a2b").exists(),
            "a manual album-only request keeps its download in staging"
        );
```

Then add these three tests to `src/runner.rs`'s test module (after the existing `finish_library_write` / staging-cleanup tests):

```rust
    #[tokio::test]
    async fn a_library_write_leaves_no_empty_artist_staging_directory() {
        let soulseek = MockClient::new();
        search_index(&soulseek, "Test Artist", &[("Test Artist Album", "Album")]);
        *soulseek.write_files.lock().unwrap() = true;
        let provider =
            FakeDiscographyProvider::with_groups(vec![release_group("album", "Album", "1998")]);
        let (config, db, staging, library) = discover_fixture(&[("Test Artist", "Present")]);

        run_artist_only_mode_with_provider(
            &soulseek,
            "Test Artist",
            false,
            &config,
            &db,
            staging.path(),
            None,
            &Arc::new(AtomicBool::new(false)),
            &provider,
        )
        .await
        .unwrap();

        assert!(
            library
                .path()
                .join("Test Artist")
                .join("Album")
                .join("01 - track.flac")
                .exists(),
            "the album must be placed in the pre-existing artist folder"
        );
        assert!(
            !staging.path().join("Test Artist").exists(),
            "a placed album must leave neither the album directory nor an empty artist level"
        );
    }

    #[tokio::test]
    async fn a_refused_download_leaves_no_empty_artist_directory() {
        let client = MockClient::new();
        let staging = TempDir::new().unwrap();
        let target = TempDir::new().unwrap();
        let mut config = make_test_config();
        config.storage.staging_dir = staging.path().to_string_lossy().into();
        config.library.paths = vec![target.path().to_string_lossy().into()];
        config.filters.min_tracks = 2;
        let db = Database::open_in_memory().unwrap();

        // One track where the album needs two: the completeness gate refuses the
        // set after the files have been staged.
        client.add_search_result(
            "Test Artist Album",
            make_result("peer", "Test Artist\\Test Album\\01 - Only.flac", 900, 12_000_000),
        );
        *client.write_files.lock().unwrap() = true;

        let result = process_album(
            &client,
            "Test Artist",
            Some("Test Album"),
            false,
            &config,
            &db,
            staging.path(),
            None,
            None,
            Some(2),
            Some(LibraryTarget::Upgrade {
                root: target.path().to_path_buf(),
                expected_tracks: 2,
            }),
        )
        .await
        .unwrap();

        assert!(
            matches!(&result, AlbumOutcome::Failed { reason } if reason.contains("incomplete download")),
            "expected the completeness gate to reject, got {result:?}"
        );
        assert!(
            !staging.path().join("Test Artist").exists(),
            "a refused download must leave neither the album directory nor an empty artist level"
        );
    }

    #[tokio::test]
    async fn a_completed_album_keeps_a_sibling_album_under_the_same_artist() {
        let staging = TempDir::new().unwrap();
        let sibling = staging.path().join("Test Artist").join("Other Album");
        std::fs::create_dir_all(&sibling).unwrap();
        std::fs::write(sibling.join("01 - Other.flac"), b"audio").unwrap();

        let client = MockClient::new();
        let library = TempDir::new().unwrap();
        std::fs::create_dir_all(library.path().join("Test Artist")).unwrap();
        let mut config = make_test_config();
        config.storage.staging_dir = staging.path().to_string_lossy().into();
        config.library.paths = vec![library.path().to_string_lossy().into()];
        config.filters.min_tracks = 0;
        let db = Database::open_in_memory().unwrap();

        search_index(&client, "Test Artist", &[("Test Artist Album", "Album")]);
        *client.write_files.lock().unwrap() = true;

        let result = process_album(
            &client,
            "Test Artist",
            Some("Album"),
            false,
            &config,
            &db,
            staging.path(),
            None,
            None,
            None,
            Some(LibraryTarget::Upgrade {
                root: library.path().to_path_buf(),
                expected_tracks: 0,
            }),
        )
        .await
        .unwrap();

        assert!(matches!(&result, AlbumOutcome::Downloaded { .. }), "got {result:?}");
        assert!(
            sibling.join("01 - Other.flac").exists(),
            "pruning the artist level must never remove a sibling album"
        );
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib -- --test-threads=1 a_library_write_leaves_no_empty_artist_staging_directory a_refused_download_leaves_no_empty_artist_directory 2>&1 | tail -30`

Expected: `a_library_write_leaves_no_empty_artist_staging_directory` FAILS on `!staging.path().join("Test Artist").exists()` — the artist directory is left behind (the album directory is removed but its parent is not). `a_refused_download_leaves_no_empty_artist_directory` FAILS the same way. `a_completed_album_keeps_a_sibling_album_under_the_same_artist` should PASS already (nothing prunes anything yet) — it is the guard against an over-eager prune later. The swept fixture tests fail too: they assert the nested path, which production does not build yet.

- [ ] **Step 3: Implement the nested path construction and the two prunes**

In `src/runner.rs`, replace the slug construction in `process_album_internal`:

```rust
    // Create per-album staging subdirectory to prevent filename collisions
    // when multiple albums are processed concurrently (e.g. two albums both
    // containing "01 - Intro.flac"). The layout is two real levels,
    // <staging>/<artist>/<album>, and both components come from the library
    // sanitiser so the staging tree mirrors the library tree.
    let album_staging = crate::staging::album_dir(staging_dir, artist, album);
```

In `finish_library_write`, replace the removal block:

```rust
    // A library write has moved the album out of staging, so the staging copy is
    // dropped, and the artist level with it when it holds nothing else. A
    // staging destination means staging *is* the album's final location —
    // removing it would delete the album.
    if matches!(destination, DownloadDestination::Library(_)) {
        if let Err(e) = std::fs::remove_dir_all(album_staging) {
            tracing::warn!("Failed to remove staging dir {album_staging:?}: {e}");
        } else if let Some(artist_dir) = album_staging.parent() {
            prune_empty_dir(artist_dir);
        }
    }
```

In `discard_refused_staging`, replace the file-removal tail and reword the doc comment and warnings:

```rust
/// Drop a refused album's staged files. A refusal means the set is never
/// written to the library, so keeping it would accumulate unusable audio in
/// `storage.staging_dir` while the album is re-downloaded from scratch on the
/// next run.
///
/// Exactly the paths this run staged are removed, never anything else. Albums
/// are processed concurrently, so the artist directory can hold another album's
/// directory; directories are removed only once they are empty and anything left
/// behind is named in a warning. A directory that was never created is not a
/// failure.
///
/// Ownership is by path, so a foreign file that happens to land on the same path
/// as one of ours is removed with ours — the two are indistinguishable.
fn discard_refused_staging(album_staging: &Path, downloaded: &[PathBuf]) {
    for path in downloaded {
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!(
                "Failed to remove the staged file {path:?} of a refused download: {e}"
            ),
        }
    }
    match remove_empty_staging_dirs(album_staging) {
        None => {
            if let Some(artist_dir) = album_staging.parent() {
                prune_empty_dir(artist_dir);
            }
        }
        Some(remaining) if remaining == album_staging => tracing::warn!(
            "Leaving {album_staging:?} in place: it could not be removed, and it may hold an entry this run did not stage (leftovers from an earlier run)"
        ),
        Some(remaining) => tracing::warn!(
            "Leaving {album_staging:?} in place: it still holds {remaining:?}, the first entry this run did not stage (a file or subdirectory — leftovers from an earlier run)"
        ),
    }
}

/// Remove a directory once it holds nothing. `remove_dir` refuses a non-empty
/// directory, so a sibling album staged under the same artist is never touched;
/// any failure is a directory that is still in use, which is not an error.
fn prune_empty_dir(dir: &Path) {
    match std::fs::remove_dir(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::debug!("Artist staging dir {dir:?} left in place: {e}"),
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib -- --test-threads=1 2>&1 | tail -30`

Expected: `test result: ok`, 0 failed, including the three new tests and every swept fixture.

- [ ] **Step 5: Run the whole suite and the gates**

Run: `cargo fmt --all && cargo clippy --all-targets -- -D warnings 2>&1 | tail -15 && cargo test 2>&1 | tail -10`

Expected: no clippy warnings; suite passes including `tests/pipeline_test.rs` — which still asserts the old flattened path and will fail here. If it does, make the Task 9 change to `tests/pipeline_test.rs:166` now rather than leaving the suite red:

```rust
    // The staging tree was consumed by the placement, including the artist level.
    let album_staging = staging.path().join("Test Artist").join("Test Album");
    assert!(!album_staging.exists());
    assert!(!staging.path().join("Test Artist").exists());
```

- [ ] **Step 6: Commit**

```bash
git add src/runner.rs tests/pipeline_test.rs
git commit -m "feat: stage albums under an artist and album directory"
```

---

### Task 8: every mode prepares the staging layout

**Files:**

- Modify: `src/runner.rs:1465-1466` (`run_upgrade_mode`), `src/runner.rs:2975-2976` (`run_manual_mode`), `src/runner.rs:2460-2500` (`run_discover_mode`), `src/main.rs:737-738` (`run_batch_mode`)

- [ ] **Step 1: Write the failing test**

Add to `src/runner.rs`'s test module, beside the other discover tests:

```rust
    #[tokio::test]
    async fn a_discovery_run_migrates_a_legacy_staging_directory_before_staging() {
        // Discover mode had no staging setup at all before this change: it
        // passed the configured path straight through. It must now migrate, so a
        // leftover from an earlier version is still recovered.
        let staging = TempDir::new().unwrap();
        let legacy = staging.path().join("Leftover Artist--Leftover Album");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("01 - Song.flac"), b"audio").unwrap();

        let client = MockClient::new();
        let mut config = make_test_config();
        config.storage.staging_dir = staging.path().to_string_lossy().into();
        config.library.paths.clear();
        let db = Database::open_in_memory().unwrap();

        run_discover_mode(&client, &config, &db, Some("Leftover Artist"), false)
            .await
            .unwrap();

        assert!(
            !legacy.exists(),
            "the legacy directory must be renamed before the run stages anything"
        );
        assert!(
            staging
                .path()
                .join("Leftover Artist")
                .join("Leftover Album")
                .join("01 - Song.flac")
                .exists(),
            "the migrated album keeps its file at the nested path"
        );
    }
```

Adjust the `run_discover_mode` argument list to its real signature; read it at the top of `src/runner.rs:2455-2470` before running. The assertion set is what matters.

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --lib a_discovery_run_migrates_a_legacy_staging_directory_before_staging 2>&1 | tail -20`

Expected: FAIL on `!legacy.exists()` — the legacy directory is untouched because discover mode never calls the migration.

- [ ] **Step 3: Wire `staging::prepare` into the four entry points**

In `src/runner.rs`, `run_upgrade_mode` (replacing the bare `create_dir_all` at 1465-1466):

```rust
    let staging_dir = Path::new(&config.storage.staging_dir);
    crate::staging::prepare(staging_dir)?;
```

In `src/runner.rs`, `run_manual_mode` (replacing the bare `create_dir_all` at 2975-2976):

```rust
    let staging_dir = Path::new(&config.storage.staging_dir);
    crate::staging::prepare(staging_dir)?;
```

In `src/runner.rs`, `run_discover_mode`, before it calls `run_discover_mode_with_provider`, add the staging setup discover never had:

```rust
    // Discover mode stages downloads like every other mode, so it prepares the
    // staging layout too: create the root, then migrate any legacy directory so
    // a leftover from an earlier version is still found by the operator.
    let staging_dir = Path::new(&config.storage.staging_dir);
    crate::staging::prepare(staging_dir)?;
```

and pass `staging_dir` instead of `Path::new(&config.storage.staging_dir)` to `run_discover_mode_with_provider`.

In `src/main.rs`, `run_batch_mode` (replacing the bare `create_dir_all` at 737-738):

```rust
    let staging_dir = Path::new(&config.storage.staging_dir);
    seakarr::staging::prepare(staging_dir)?;
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib a_discovery_run_migrates_a_legacy_staging_directory_before_staging 2>&1 | tail -20`

Expected: PASS.

- [ ] **Step 5: Run the whole suite and the gates**

Run: `cargo fmt --all && cargo clippy --all-targets -- -D warnings 2>&1 | tail -15 && cargo test 2>&1 | tail -10`

Expected: no clippy warnings, suite passes. Expect existing mode tests to keep passing: they configure an existing `TempDir` as `staging_dir`, so `prepare` finds nothing to migrate.

- [ ] **Step 6: Commit**

```bash
git add src/runner.rs src/main.rs
git commit -m "feat: prepare the staging layout in every mode"
```

---

### Task 9: end-to-end and disc-nesting assertions

**Files:**

- Modify: `src/download.rs` (test module only)
- Test: `tests/pipeline_test.rs:160-170`

- [ ] **Step 1: Verify the end-to-end staging assertions**

`tests/pipeline_test.rs:166` was already updated in Task 7 Step 5. Confirm it reads:

```rust
    // The staging tree was consumed by the placement, including the artist level.
    let album_staging = staging.path().join("Test Artist").join("Test Album");
    assert!(!album_staging.exists());
    assert!(!staging.path().join("Test Artist").exists());
```

- [ ] **Step 2: Add the disc-nesting assertion**

Find the multi-disc staging test in `src/download.rs`'s test module — the one whose comment names `CD 01` and `CD 02` as separate disc groups (`src/download.rs:2700-2735` is the search-grouping test; the download-side disc nesting test is the one that asserts real staged paths). Add, beside its existing assertions:

```rust
        // The disc level nests inside the album directory, which is now
        // <staging>/<artist>/<album>: the album root is the album's own
        // directory, and a multi-disc album keeps each disc below it.
        assert!(
            staging
                .path()
                .join("Test Artist")
                .join("Test Album")
                .join("CD 01")
                .join("01 - One.flac")
                .exists(),
            "a disc folder must nest inside the album directory, not beside it"
        );
```

If the test in question calls `download_album` with a hand-built album staging path (not through `process_album`), assert the nesting relative to the path it passed, and state that in the assertion message. Do not add a `staging::album_dir` call to production `download.rs`.

- [ ] **Step 3: Run the tests**

Run: `cargo test --test pipeline_test 2>&1 | tail -15 && cargo test --lib download 2>&1 | tail -15`

Expected: both pass.

- [ ] **Step 4: Run the gates**

Run: `cargo fmt --all && cargo clippy --all-targets -- -D warnings 2>&1 | tail -15 && cargo test 2>&1 | tail -10`

Expected: no clippy warnings, full suite passes.

- [ ] **Step 5: Commit**

```bash
git add tests/pipeline_test.rs src/download.rs
git commit -m "test: pin the nested staging layout end to end"
```

---

### Task 10: documentation check and release note

**Files:**

- Modify: `README.md` (only if Step 1 finds a stale claim)
- Modify: `docs/agent/specs/2026-10-08-nested-staging-album-layout-design.md` (only if a `file:line` reference moved)

- [ ] **Step 1: Check whether the README names the flattened layout**

Run: `grep -n -- '--<album>\|artist--album\|Artist Name--Album' README.md`

Expected: no output. The spec records that the README describes `storage.staging_dir` without naming the layout. If the command prints a line, update that sentence to name the nested layout — do not leave a claim that contradicts the code.

- [ ] **Step 2: Check the spec's `file:line` references still resolve**

Run: `for ref in "src/runner.rs:508" "src/organizer.rs:516" "src/organizer.rs:758" "src/organizer.rs:774" "src/runner.rs:355" "src/runner.rs:392"; do file="${ref%%:*}"; line="${ref##*:}"; printf '%s -> ' "$ref"; sed -n "${line}p" "$file"; done`

Expected: each line is the construct the spec's problem table describes. Where a reference has moved, add a dated `Superseded on …` note in the spec rather than editing the table's numbers — the repository leaves historical design documents as written.

- [ ] **Step 3: Lint the markdown**

Run: `markdownlint --fix README.md docs/agent/plans/2026-10-08-nested-staging-album-layout.md docs/agent/specs/2026-10-08-nested-staging-album-layout-design.md`

Expected: exit 0. If `markdownlint` is unavailable, STOP and report before finalising.

- [ ] **Step 4: Record the release note text**

Add a note to the plan's follow-up list (this step is the record, not a file to ship): the version that ships this change must state that the staging layout is now `<staging_dir>/<artist>/<album>/` and that the migration is **one-way** — running an older binary against a migrated staging directory treats each artist directory as an album slug and deletes the album directories as untracked debris, so reverting requires an empty staging directory.

- [ ] **Step 5: Run the full gate set**

Run: `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test 2>&1 | tail -10 && pre-commit run --all-files`

Expected: all pass. Coverage floor for this repository is 95%; the new `src/staging.rs` is covered by its own unit tests, so `cargo llvm-cov` (or the repository's coverage command) must report no drop below the floor.

- [ ] **Step 6: Commit**

```bash
git add README.md docs/agent/specs/2026-10-08-nested-staging-album-layout-design.md
git commit -m "docs: record the shipped nested staging layout"
```

---

## Self-Review

### 1. Spec coverage

| Spec requirement | Task |
| --- | --- |
| `<staging>/<artist>/<album>` construction, both components sanitised | Task 1 |
| Artist level for an album-only run, `_` placeholder | Task 1 |
| `unknown` album component for an artist-only run | Task 1 |
| New `src/staging.rs` owning layout, migration, scan | Tasks 1-4 |
| `normalise_layout` runs per run, ungated | Tasks 2, 8 |
| Legacy rename, recursion preserving a disc folder | Task 2 |
| Merge without overwriting; `left_in_place` | Task 3 |
| Non-UTF-8 name left alone; `unrecognised` directory left alone | Task 3 |
| Empty artist directory pruned (migration and both cleanup paths) | Tasks 2, 7 |
| `scan` identity by sanitised, case-insensitive match; highest `id`; ambiguity warning | Task 4 |
| Recovery iterates `staging::scan`; keeps gate, position, two arms | Task 6 |
| `parse_album_slug` kept as the legacy parser, off the recovery path | Tasks 2, 6 |
| `disc_subdir` stops at the album root by name | Task 5 |
| Runner path construction from `staging::album_dir` | Task 7 |
| `finish_library_write` and `discard_refused_staging` prune the artist level; warnings reworded | Task 7 |
| `staging::prepare` replaces every bare `create_dir_all`, discover gains one | Task 8 |
| Sanitisation audit enforced by tests | Task 1 (`album_dir_matches_the_library_components_for_a_hostile_title`), Task 5 (TDD preserves the existing sanitiser tests) |
| Acceptance criterion 7 (every mode migrates) | Task 8 |
| Acceptance criterion 8 (`fmt`, `clippy`, `test`) | Every task's gate step, and Task 10 Step 5 |
| README check, release note, historical specs left as written | Task 10 |

No spec requirement is left without a task. The spec's sanitisation audit table is a record of what must not change, so it needs no code: Tasks 5 and 6 are the only edits touching those paths, and both are covered by existing tests that must keep passing.

### 2. Placeholder scan

No "TBD", "handle edge cases", or "similar to Task N". Every code step carries the code. Two steps deliberately cite a source location to be read rather than reproducing a large existing test body (Task 8 Step 1's `run_discover_mode` argument list, Task 9 Step 2's disc-nesting test name); each states exactly what to look for and what the assertion must be.

### 3. Type consistency

- `staging::artist_dir(&Path, &str) -> PathBuf`, `staging::album_dir(&Path, &str, Option<&str>) -> PathBuf`, `staging::scan(&Path, &Database) -> Result<Vec<StagedAlbum>>`, `staging::normalise_layout(&Path) -> Result<MigrationReport>`, `staging::prepare(&Path) -> Result<MigrationReport>` — consistent across Tasks 1-4 and their call sites in Tasks 7 and 8.
- `StagedAlbum { artist_dir_path, album_dir_path, dir_album, identity }` and `StagedIdentity { artist, album, status }` — the field names used in Task 4's `scan`, Task 6's recovery loop, and Task 4's tests all match.
- `MigrationReport { migrated, merged, left_in_place, unrecognised, non_utf8, failed, pruned_artist_dirs }` — Task 2 defines it, Task 2's tests and Task 3's tests use exactly those names.
- `disc_subdir(&Path, &str) -> Option<String>` — Task 5 changes both the definition and its single call site.
- `prune_empty_dir(dir: &Path)` — defined and used in Task 7 only.
- `remove_empty_staging_dirs` keeps its `Option<PathBuf>` return and is consumed by the new `match` in `discard_refused_staging`.

One inconsistency found and fixed during review: Task 2 initially pushed a pre-existing destination straight into `left_in_place`; Task 3 replaces that with the merge and moves the destination to the merge's own report path. Task 2's test `normalise_layout_leaves_a_populated_artist_directory_alone` does not depend on that arm, so the two tasks do not conflict.

### 4. Interpretation flagged for the implementer

The spec's error-handling table says an unwritable staging root "surfaces through the existing `?` on the staging setup", while its architecture section says the staging calls "keep the existing treatment: a failure is logged as a warning and never aborts the run". This plan implements `staging::prepare(staging_dir)?` — `create_dir_all` and a top-level `read_dir` failure propagate exactly as they do today (which is what `run_manual_mode`'s own comment documents), while every per-entry anomaly is recorded in `MigrationReport` and logged, so no wasted rename aborts a run. If the reviewer reads the architecture sentence as also covering `create_dir_all`, the change is one `?` → `if let Err(e) = … { tracing::warn!(…) }` at four call sites in Task 8.

---

## Execution Handoff

Plan complete and saved to `docs/agent/plans/2026-10-08-nested-staging-album-layout.md`. Proceeding with subagent-driven execution.
