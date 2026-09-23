<!-- markdownlint-disable MD013 -->
<!-- markdownlint-disable MD024 -->
# Manual library placement Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use sub-agents (recommended) to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make both manual forms (`--artist X` and `--artist X --album Y`) place each completed album into the artist's pre-existing library folder, leaving it in staging when that folder is absent or the album folder already exists.

**Architecture:** A bounded destination resolver reads the configured library roots (one listing each, never a walk) and returns the artist folder that exists on disk with its on-disk spelling. A small `organizer` helper derives the album directory placement would write into, exactly as the copy path derives it, so an existing album folder can be detected before anything is copied. Both manual forms pass `LibraryTarget::Place { skip_existing_album: true }` into the existing album pipeline, so placement, staging removal, the processed record, the notification and the summary destination are all inherited unchanged; discover mode passes the same target with the flag `false` and is untouched.

**Tech Stack:** Rust 2021 (crate `seakarr`), Tokio paused-time tests, `tempfile` fixtures, the project's existing `MockClient` / `FakeDiscographyProvider` / `LogCapture` test doubles.

**Design record:** `docs/agent/specs/2026-09-23-manual-library-placement-design.md`.

---

## Scope check

Single subsystem: where a manual run's completed albums are written. One plan, no split. The two manual forms share the resolver, the album-existence helper and the placement target, so they belong in one plan; batch mode is deliberately excluded by the spec.

## Spec refinements made during planning

1. **The matching rule is now exact.** The resolver sanitises both the candidate folder name and the artist name through `organizer::sanitize_component`, then compares `normalize_catalog_key` of each. That is what makes the tag spelling `AC/DC` find a folder stored as `AC-DC`; a plain case-insensitive comparison of raw names would not, and the spec's Error handling bullet was corrected (commit `31e3a7d`).
2. **The skip path finishes through `finish_library_write`** with a `Staging` destination rather than returning the outcome directly, so an album left in staging still gets its processed record, its notification and the summary destination it gets today. The spec's snippet and a new "Refinements made during planning" section record this.

## Amendments applied during review

The review loop changed four behaviours the tasks above specify. Where the code and this plan
disagree, the code and `docs/agent/specs/2026-09-23-manual-library-placement-design.md` are
authoritative.

- **Manual runs never organize (round 1, user decision).** Task 4 Step 8 said the resolver
  backing off would let the organize step create the artist folder "exactly as it does today".
  The user decided the opposite: a manual run never creates an artist folder. `LibraryTarget`
  gained `StagingOnly`, which `manual_place_target` returns when the artist has no folder, and
  the organize gate is suppressed for it. The multi-disc organize test was rewritten to assert
  the album stays in staging with no library folder created.
- **The album-only organizer guard was relaxed (round 2).** `process_album_internal` rejects an
  album-only run with `storage.organize: true`, which is right for a batch line but wrong for a
  manual album-only search that never organizes; the guard now ignores `StagingOnly`, and the old
  locking test was replaced by a batch-shape test plus a manual test.
- **The destination-existence rule is "strictly below the artist folder" (round 2).** A lexical
  comparison against the artist folder missed patterns whose expansion has no album component or
  writes to the library root. The check now compares path components and requires the derived
  directory to be a strict descendant. A `./`-prefixed pattern was named here as a third case and
  is not one: `Path` equality already compares components.
- **The resolver refuses names that sanitise away (round 2).** Blank names were guarded, but a
  name such as `***` also sanitises to the placeholder and could match a `_` folder; non-UTF-8
  folder names are now skipped rather than matched through a lossy spelling.

## File structure

| File | Change | Responsibility |
| --- | --- | --- |
| `src/organizer.rs` | Modify | New `placement_album_dir` (the album directory placement would write into) plus unit tests. No existing function changes. |
| `src/discover.rs` | Modify | New `resolve_artist_folder` (bounded, filesystem-based artist folder lookup) plus unit tests and the `Config` / `sanitize_component` imports it needs. |
| `src/runner.rs` | Modify | `LibraryTarget::Place` gains `skip_existing_album`; the album-existence gate in the `Place` arm; two small manual helpers; `process_artist_album_work` and its two callers thread the target; `run_manual_mode` threads it too; the discover call site passes `false`; tests inverted and added. |
| `README.md` | Modify | Manual-mode bullet, `--artist` and `--album` rows, the `--ignore-processed` paragraph, the placement section. |

No new files, no new configuration key, no CLI change.

---

## Task 1: `organizer::placement_album_dir`

**Files:**

- Modify: `src/organizer.rs` (add the function directly after `place_into_library`, tests in the existing `#[cfg(test)] mod tests`)

- [ ] **Step 1: Write the failing tests**

Add to the tests module in `src/organizer.rs`:

```rust
    #[test]
    fn placement_album_dir_matches_the_copy_destination() {
        let album_dir = placement_album_dir(
            Path::new("/library"),
            "%artist%/%album%/%track% - %title%.%ext%",
            "The Artist",
            "The Album",
            Path::new("/staging/Artist--Album/01 - One.flac"),
        );

        assert_eq!(
            album_dir,
            Some(PathBuf::from("/library/The Artist/The Album"))
        );
    }

    #[test]
    fn placement_album_dir_follows_a_pattern_that_shapes_the_album_folder() {
        // The album component carries another placeholder, so the destination is
        // not simply `<root>/<artist>/<album>`: the check has to follow the pattern.
        let album_dir = placement_album_dir(
            Path::new("/library"),
            "%artist%/%album% - %user%/%track% - %title%.%ext%",
            "The Artist",
            "The Album",
            Path::new("/staging/Artist--Album/01 - One.flac"),
        );

        assert_eq!(
            album_dir,
            Some(PathBuf::from("/library/The Artist/The Album - unknown"))
        );
    }

    #[test]
    fn placement_album_dir_ignores_a_disc_subdirectory_in_staging() {
        // `copy_into_library` records the album directory from the pattern, before
        // it inserts the disc subdirectory, so a staging layout with a disc folder
        // must not move the album directory down into it.
        let album_dir = placement_album_dir(
            Path::new("/library"),
            "%artist%/%album%/%track% - %title%.%ext%",
            "The Artist",
            "The Album",
            Path::new("/staging/Artist--Album/CD 01/01 - One.flac"),
        );

        assert_eq!(
            album_dir,
            Some(PathBuf::from("/library/The Artist/The Album"))
        );
    }
```

`%user%` expands to `unknown` (`expand_pattern_inner`, `src/organizer.rs:145-160`); an unknown
placeholder such as `%year%` would stay literal, which is why the pattern-shaping test uses
`%user%`.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p seakarr --lib placement_album_dir`
Expected: FAIL to compile with `error[E0425]: cannot find function placement_album_dir in this scope`

- [ ] **Step 3: Implement the helper**

In `src/organizer.rs`, directly after `place_into_library`:

```rust
/// The album directory placement would write into for `first_downloaded`.
///
/// Derived exactly as [`copy_into_library`] derives it: expand the pattern with
/// the artist value already final, then take the parent of the file path, which
/// is recorded before any disc subdirectory is inserted. Used to tell whether the
/// album's destination already exists before anything is copied.
pub fn placement_album_dir(
    library_root: &Path,
    pattern: &str,
    artist_dir: &str,
    album: &str,
    first_downloaded: &Path,
) -> Option<PathBuf> {
    let stem = first_downloaded.file_stem()?.to_string_lossy();
    let ext = first_downloaded.extension().unwrap_or_default().to_string_lossy();
    let (track, title) = organize_name_from_stem(&stem);
    let relative = expand_pattern_inner(pattern, artist_dir, album, &track, &title, &ext, "unknown");
    library_root
        .join(relative)
        .parent()
        .map(Path::to_path_buf)
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p seakarr --lib placement_album_dir`
Expected: PASS, `3 passed`

- [ ] **Step 5: Run the organizer suite**

Run: `cargo test -p seakarr --lib organizer::tests`
Expected: PASS, no failures

- [ ] **Step 6: Commit**

```bash
git add src/organizer.rs
git commit -m "feat: derive the album directory placement would write into"
```

---

## Task 2: `discover::resolve_artist_folder`

**Files:**

- Modify: `src/discover.rs` (imports at the top, the function near `LibraryIndex`, tests in the existing `#[cfg(test)] mod tests`)

- [ ] **Step 1: Write the failing tests**

Add to the tests module in `src/discover.rs`:

```rust
    fn library_with_artist_folders(folders: &[&str]) -> TempDir {
        let library = TempDir::new().unwrap();
        for folder in folders {
            std::fs::create_dir_all(library.path().join(folder)).unwrap();
        }
        library
    }

    fn config_with_roots(roots: &[&TempDir]) -> Config {
        let mut config = Config::default();
        config.library.paths = roots
            .iter()
            .map(|root| root.path().to_string_lossy().into_owned())
            .collect();
        config
    }

    #[test]
    fn resolve_artist_folder_returns_the_on_disk_spelling() {
        let library = library_with_artist_folders(&["the cinematic orchestra"]);
        let config = config_with_roots(&[&library]);

        let found = resolve_artist_folder(&config, "The Cinematic Orchestra");

        assert_eq!(
            found,
            Some((
                library.path().to_path_buf(),
                "the cinematic orchestra".to_string()
            ))
        );
    }

    #[test]
    fn resolve_artist_folder_folds_the_tag_spelling_onto_the_stored_folder() {
        // The sanitiser stores "AC/DC" as "AC-DC"; the tag spelling must find it.
        let library = library_with_artist_folders(&["AC-DC"]);
        let config = config_with_roots(&[&library]);

        let found = resolve_artist_folder(&config, "AC/DC");

        assert_eq!(found, Some((library.path().to_path_buf(), "AC-DC".to_string())));
    }

    #[test]
    fn resolve_artist_folder_is_none_when_the_artist_has_no_folder() {
        let library = library_with_artist_folders(&["Someone Else"]);
        let config = config_with_roots(&[&library]);

        assert_eq!(resolve_artist_folder(&config, "The Cinematic Orchestra"), None);
    }

    #[test]
    fn resolve_artist_folder_prefers_the_first_configured_root() {
        let first = library_with_artist_folders(&["The Artist"]);
        let second = library_with_artist_folders(&["The Artist"]);
        let config = config_with_roots(&[&first, &second]);

        let found = resolve_artist_folder(&config, "The Artist");

        assert_eq!(
            found,
            Some((first.path().to_path_buf(), "The Artist".to_string())),
            "configuration order decides, deterministically"
        );
    }

    #[test]
    fn resolve_artist_folder_ignores_a_file_named_like_the_artist() {
        let library = TempDir::new().unwrap();
        std::fs::write(library.path().join("The Artist"), b"not a folder").unwrap();
        let config = config_with_roots(&[&library]);

        assert_eq!(resolve_artist_folder(&config, "The Artist"), None);
    }

    #[test]
    fn resolve_artist_folder_is_none_without_library_paths() {
        let config = Config::default();

        assert!(config.library.paths.is_empty());
        assert_eq!(resolve_artist_folder(&config, "The Artist"), None);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p seakarr --lib resolve_artist_folder`
Expected: FAIL to compile with `error[E0425]: cannot find function resolve_artist_folder in this scope`

- [ ] **Step 3: Add the imports**

In `src/discover.rs`, next to the existing `use crate::discography::{...}` line, add:

```rust
use crate::config::Config;
use crate::organizer::sanitize_component;
```

- [ ] **Step 4: Implement the resolver**

In `src/discover.rs`, directly after the `impl LibraryIndex` block:

```rust
/// The library root and on-disk artist folder a manual run should place this
/// artist's downloads into, or `None` when no configured library path holds a
/// folder for the artist.
///
/// Reads the filesystem rather than [`LibraryIndex`]: the index only lists
/// artists the walk found audio for, so a failed or stale scan would silently
/// change where a manual download lands. Both sides are sanitised and compared
/// through [`normalize_catalog_key`], so the tag spelling agrees with the stored
/// spelling (`AC/DC` with a folder written as `AC-DC`) and the comparison is
/// case-insensitive. The returned folder name is the spelling that exists on
/// disk, so placement lands inside the existing folder instead of beside a
/// rewritten copy of it.
pub fn resolve_artist_folder(config: &Config, artist: &str) -> Option<(PathBuf, String)> {
    let wanted = normalize_catalog_key(&sanitize_component(artist));
    if wanted.is_empty() {
        return None;
    }
    for root in &config.library.paths {
        let Ok(entries) = std::fs::read_dir(root) else {
            continue;
        };
        // Sorted so a library holding two spellings of one artist resolves
        // deterministically rather than following read_dir order.
        let mut names: Vec<String> = entries
            .flatten()
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        if let Some(name) = names
            .into_iter()
            .find(|name| normalize_catalog_key(&sanitize_component(name)) == wanted)
        {
            return Some((PathBuf::from(root), name));
        }
    }
    None
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p seakarr --lib resolve_artist_folder`
Expected: PASS, `6 passed`

- [ ] **Step 6: Run the discover suite**

Run: `cargo test -p seakarr --lib discover::tests`
Expected: PASS, no failures

- [ ] **Step 7: Commit**

```bash
git add src/discover.rs
git commit -m "feat: resolve a manual run's existing artist folder from disk"
```

---

## Task 3: the `skip_existing_album` flag and the album-existence gate

**Files:**

- Modify: `src/runner.rs` (`LibraryTarget` at `:243-252`, the `Place` arm from `:933`, the discover call site at `:1948`)

- [ ] **Step 1: Write the failing tests**

Add to the tests module in `src/runner.rs`, next to the other placement tests:

```rust
    #[tokio::test]
    async fn placement_skips_when_the_album_folder_already_exists() {
        let client = MockClient::new();
        *client.search_results.lock().unwrap() = vec![album_result("Test Artist", "Old Album")];
        *client.write_files.lock().unwrap() = true;
        let (mut config, db, staging) = artist_only_fixture();
        let library = library_with(&[("Test Artist", "Old Album")]);
        config.library.paths = vec![library.path().to_string_lossy().into_owned()];
        config.storage.organize = false;

        let outcome = process_album(
            &client,
            "Test Artist",
            Some("Old Album"),
            false,
            &config,
            &db,
            staging.path(),
            None,
            None,
            None,
            Some(LibraryTarget::Place {
                root: library.path(),
                artist_dir: "Test Artist",
                skip_existing_album: true,
            }),
        )
        .await
        .unwrap();

        assert!(
            matches!(
                outcome,
                AlbumOutcome::Downloaded {
                    destination: DownloadDestination::Staging(_),
                    ..
                }
            ),
            "an existing album folder must keep the download in staging, got {outcome:?}"
        );
        assert!(
            staging
                .path()
                .join("Test Artist--Old Album")
                .join("01 - track.flac")
                .exists(),
            "the download stays in staging"
        );
        assert_eq!(
            std::fs::read(
                library
                    .path()
                    .join("Test Artist")
                    .join("Old Album")
                    .join("01 - track.flac")
            )
            .unwrap(),
            b"fake flac data",
            "the existing album folder is left untouched"
        );
    }

    #[tokio::test]
    async fn placement_still_writes_when_the_flag_is_off() {
        // Discover's shape: same target, flag off, and the existing folder is
        // written to (keeping files it already holds) exactly as before.
        let client = MockClient::new();
        *client.search_results.lock().unwrap() = vec![album_result("Test Artist", "Old Album")];
        *client.write_files.lock().unwrap() = true;
        let (mut config, db, staging) = artist_only_fixture();
        let library = library_with(&[("Test Artist", "Old Album")]);
        config.library.paths = vec![library.path().to_string_lossy().into_owned()];
        config.storage.organize = false;

        let outcome = process_album(
            &client,
            "Test Artist",
            Some("Old Album"),
            false,
            &config,
            &db,
            staging.path(),
            None,
            None,
            None,
            Some(LibraryTarget::Place {
                root: library.path(),
                artist_dir: "Test Artist",
                skip_existing_album: false,
            }),
        )
        .await
        .unwrap();

        assert!(
            matches!(
                outcome,
                AlbumOutcome::Downloaded {
                    destination: DownloadDestination::Library(_),
                    ..
                }
            ),
            "placement with the flag off must report the library destination, got {outcome:?}"
        );
        assert!(
            !staging.path().join("Test Artist--Old Album").exists(),
            "a placed album leaves no staging copy"
        );
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p seakarr --lib placement_skips_when_the_album_folder_already_exists`
Expected: FAIL to compile with `error[E0560]: struct LibraryTarget::Place has no field named skip_existing_album`

- [ ] **Step 3: Add the flag to the enum**

In `src/runner.rs`, update the enum and its doc comment:

```rust
/// `Upgrade` is auto mode's replacement of an album that already exists but
/// fails the quality gate: it is gated by the caller on
/// `library_upgrade.enabled`, compares the download against the library's own
/// track count, and may delete lesser-quality files. `Place` is the placement of
/// a newly downloaded album beside the artist's existing albums, used by discover
/// mode and by manual runs: it carries no completeness baseline and never deletes
/// anything. `skip_existing_album` is manual mode's rule that an album folder
/// which already exists keeps the download in staging instead of being written
/// into; discover passes `false`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LibraryTarget<'a> {
    Upgrade {
        root: &'a Path,
        expected_tracks: usize,
    },
    Place {
        root: &'a Path,
        artist_dir: &'a str,
        skip_existing_album: bool,
    },
}
```

- [ ] **Step 4: Gate the `Place` arm**

Replace the arm's opening in `process_album_internal`:

```rust
        Some(LibraryTarget::Place {
            root,
            artist_dir,
            skip_existing_album,
        }) => {
            // Manual mode's rule: an album folder that already exists wins, because
            // placement never replaces a readable file and could otherwise report
            // success while adding nothing. Checked before the completeness backstop
            // below, so a refused set keeps its staging copy rather than being
            // discarded.
            let existing_album_dir = if skip_existing_album {
                downloaded.first().and_then(|first| {
                    organizer::placement_album_dir(
                        root,
                        &config.storage.organize_pattern,
                        artist_dir,
                        album.unwrap_or("Unknown"),
                        first,
                    )
                })
            } else {
                None
            };
            if let Some(existing) = existing_album_dir.filter(|dir| dir.is_dir()) {
                tracing::info!(
                    "{artist} - {}: album folder already exists at {}; leaving the download in staging",
                    album.unwrap_or("?"),
                    existing.display()
                );
                return finish_library_write(
                    config,
                    db,
                    &album_staging,
                    artist,
                    album,
                    downloaded.len(),
                    DownloadDestination::Staging(album_staging.clone()),
                )
                .await;
            }
            // A new album has no library track count to compare against, so the
            // completeness test here is the configured `min_tracks` plus the
            // numbering check instead (see `library_write_refusal`). Presence
            // already treats any audio file under the album folder as present,
            // and there is no quality deletion either — nothing is being
            // replaced. `place_into_library` uses the folder name that
            // already exists on disk verbatim, so the album lands inside it
            // instead of beside a rewritten copy of it, and it never replaces a
            // readable existing file because the destination folder may belong
            // to a different edition of the album.
            if let Some(reason) = library_write_refusal(&downloaded, config.filters.min_tracks) {
```

Keep the rest of the arm exactly as it is.

- [ ] **Step 5: Update the discover call site**

In `run_discover_mode_with_provider`, the placement target becomes:

```rust
        let placement = LibraryTarget::Place {
            root: artist.library_root.as_path(),
            artist_dir: artist.artist_dir.as_str(),
            skip_existing_album: false,
        };
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test -p seakarr --lib placement_skips_when_the_album_folder_already_exists`
Expected: PASS, `1 passed`
Run: `cargo test -p seakarr --lib placement_still_writes_when_the_flag_is_off`
Expected: PASS, `1 passed`

- [ ] **Step 7: Run the discover suite to prove discover is unchanged**

Run: `cargo test -p seakarr --lib discover`
Expected: PASS, no failures

- [ ] **Step 8: Commit**

```bash
git add src/runner.rs
git commit -m "feat: keep a manual download in staging when its album folder exists"
```

---

## Task 4: artist-only runs thread the placement target

**Files:**

- Modify: `src/runner.rs` (`process_artist_album_work` at `:1327`, its callers in `run_legacy_artist_only_mode` at `:1524` and `run_artist_only_mode_with_provider` at `:1627`, new helpers near `process_artist_album_work`)

- [ ] **Step 1: Write the failing tests**

Invert the locked test and add two, in the tests module of `src/runner.rs`:

```rust
    #[tokio::test]
    async fn artist_only_mode_places_into_the_existing_artist_folder() {
        // Supersedes the old "only discover places" lock: a manual run now writes
        // into the artist folder that already exists, and never creates one.
        let soulseek = MockClient::new();
        search_index(&soulseek, "Test Artist", &[("Test Artist Album", "Album")]);
        *soulseek.write_files.lock().unwrap() = true;
        let provider =
            FakeDiscographyProvider::with_groups(vec![release_group("album", "Album", "1998")]);
        let (mut config, db, staging, library) = discover_fixture(&[("Test Artist", "Present")]);
        assert!(!config.storage.organize, "the organize step must stay off");

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
            !staging.path().join("Test Artist--Album").exists(),
            "a placed album leaves no staging copy"
        );
    }

    #[tokio::test]
    async fn artist_only_mode_without_an_artist_folder_keeps_staging() {
        let soulseek = MockClient::new();
        search_index(
            &soulseek,
            "Folderless Artist 8c1d",
            &[("Folderless Artist 8c1d Ghost", "Ghost")],
        );
        *soulseek.write_files.lock().unwrap() = true;
        let provider =
            FakeDiscographyProvider::with_groups(vec![release_group("ghost", "Ghost", "2001")]);
        let (mut config, db, staging) = artist_only_fixture();
        let library = TempDir::new().unwrap();
        config.library.paths = vec![library.path().to_string_lossy().into_owned()];
        let capture = crate::test_support::LogCapture::start();

        run_artist_only_mode_with_provider(
            &soulseek,
            "Folderless Artist 8c1d",
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
            staging
                .path()
                .join("Folderless Artist 8c1d--Ghost")
                .join("01 - track.flac")
                .exists(),
            "without an artist folder the album stays in staging"
        );
        assert!(
            !library.path().join("Folderless Artist 8c1d").exists(),
            "a manual run never creates an artist folder"
        );
        let logs = capture.text();
        assert!(
            logs.lines().any(|line| line.contains("no library folder")
                && line.contains("Folderless Artist 8c1d")),
            "the run must explain why the album stayed in staging:\n{logs}"
        );
    }

    #[tokio::test]
    async fn artist_only_mode_keeps_staging_when_the_album_folder_exists() {
        // An album folder that holds no audio is not "present" to the scan, so the
        // album is downloaded and then the existence gate keeps it in staging.
        let soulseek = MockClient::new();
        search_index(&soulseek, "Test Artist", &[("Test Artist Album", "Album")]);
        *soulseek.write_files.lock().unwrap() = true;
        let provider =
            FakeDiscographyProvider::with_groups(vec![release_group("album", "Album", "1998")]);
        let (mut config, db, staging, library) = discover_fixture(&[("Test Artist", "Present")]);
        std::fs::create_dir_all(library.path().join("Test Artist").join("Album")).unwrap();

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
            staging
                .path()
                .join("Test Artist--Album")
                .join("01 - track.flac")
                .exists(),
            "an existing album folder keeps the download in staging"
        );
    }
```

Then delete the old test `artist_only_mode_leaves_the_download_in_staging` in full, including its comment — it asserted the contract this change replaces.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p seakarr --lib artist_only_mode_places_into_the_existing_artist_folder`
Expected: FAIL — the album is still in staging, with `assertion failed: the album must be placed in the pre-existing artist folder`
Run: `cargo test -p seakarr --lib artist_only_mode_without_an_artist_folder_keeps_staging`
Expected: FAIL — no "no library folder" line in the captured logs

- [ ] **Step 3: Add the two manual helpers**

In `src/runner.rs`, directly above `process_artist_album_work`:

```rust
/// The artist folder a manual run should place into, or `None` when the artist
/// has none under a configured library path. Silent: the explanation for a
/// download that stayed in staging is logged where the outcome is known.
fn resolve_manual_target(config: &Config, artist: &str) -> Option<(PathBuf, String)> {
    if artist.trim().is_empty() {
        return None;
    }
    discover::resolve_artist_folder(config, artist)
}

/// Explain once per run that downloads stayed in staging because the artist has no
/// library folder. Called only when an album really stayed in staging, and never for
/// a blank artist, so a run with nothing to show for itself stays quiet.
fn log_no_artist_folder(artist: &str, config: &Config, destination: &Option<(PathBuf, String)>) {
    if artist.trim().is_empty() || destination.is_some() || config.library.paths.is_empty() {
        return;
    }
    tracing::info!(
        "{artist}: no library folder found under the configured library paths; downloads stay in staging"
    );
}

/// Manual placement's target: the resolved artist folder, with the
/// existing-album rule the other placement callers do not use.
fn manual_place_target(destination: &Option<(PathBuf, String)>) -> Option<LibraryTarget<'_>> {
    destination
        .as_ref()
        .map(|(root, artist_dir)| LibraryTarget::Place {
            root: root.as_path(),
            artist_dir: artist_dir.as_str(),
            skip_existing_album: true,
        })
}
```

- [ ] **Step 4: Thread the target through `process_artist_album_work`**

Add the parameter and pass it on. The signature becomes:

```rust
async fn process_artist_album_work(
    client: &dyn SoulseekClient,
    artist: &str,
    ignore_processed: bool,
    config: &Config,
    db: &Database,
    staging_dir: &Path,
    progress: Option<&ProgressDisplay>,
    cancel: &Arc<AtomicBool>,
    work: Vec<(String, Option<Vec<crate::client::SearchResult>>)>,
    target: Option<LibraryTarget<'_>>,
) -> Result<Vec<(String, AlbumOutcome)>> {
```

and inside it, the `process_album_internal` call passes `target` where it passes `None` today (`:1397`):

```rust
        let result = process_album_internal(
            client,
            &process_artist,
            Some(&process_album),
            ignore_processed,
            config,
            db,
            staging_dir,
            progress,
            Some(cancel),
            library_track_count,
            target,
            presearched,
        )
        .await?;
```

`LibraryTarget` is `Copy`, so passing it inside the loop is fine.

- [ ] **Step 5: Wire the legacy artist-only caller**

In `run_legacy_artist_only_mode`, after `let work = albums ... .collect();` and before the `process_artist_album_work` call, add:

```rust
    let destination = resolve_manual_target(config, artist);
    let target = manual_place_target(&destination);
```

and pass `target` as the final argument of the call:

```rust
    let outcomes = process_artist_album_work(
        client,
        artist,
        ignore_processed,
        config,
        db,
        staging_dir,
        progress,
        cancel,
        work,
        target,
    )
    .await?;
```

- [ ] **Step 6: Wire the authoritative artist-only caller**

In `run_artist_only_mode_with_provider`, after `let work = missing.into_iter().map(...).collect();` and before the `process_artist_album_work` call, add the same two lines:

```rust
            let destination = resolve_manual_target(config, artist);
            let target = manual_place_target(&destination);
```

and pass `target` as the final argument of that call (same shape as Step 5).

- [ ] **Step 7: Run the three tests**

Run: `cargo test -p seakarr --lib artist_only_mode_places_into_the_existing_artist_folder`
Expected: PASS, `1 passed`
Run: `cargo test -p seakarr --lib artist_only_mode_without_an_artist_folder_keeps_staging`
Expected: PASS, `1 passed`
Run: `cargo test -p seakarr --lib artist_only_mode_keeps_staging_when_the_album_folder_exists`
Expected: PASS, `1 passed`

- [ ] **Step 8: Run the whole artist-only family**

Run: `cargo test -p seakarr --lib artist_only`
Expected: PASS, no failures. Three of these are the behavioural confirmation that nothing else moved:

- `artist_only_manual_mode_preserves_multi_disc_organization` (`:4093`) sets
  `storage.organize = true` with an **empty** library, so the resolver finds no artist folder, the
  target stays `None`, and the organize step creates `Test Artist/Album One/CD 01|CD 02` exactly as
  it does today. If this test fails here, the resolver has started inventing destinations it must not.
- `artist_only_manual_mode_matches_processed_album_case_insensitively` (`:4133`) configures no
  library paths at all, so placement is never attempted and its processed-record assertion is
  untouched.
- `artist_only_manual_still_runs_when_the_library_path_is_missing` (`:5997`) passes
  `/definitely/not/here`: the resolver cannot list it, returns `None`, and the run proceeds. Add to
  that test the assertion that the album stayed in staging:

```rust
        assert!(
            staging
                .path()
                .join("Test Artist--Album")
                .join("01 - track.flac")
                .exists(),
            "an unusable library path must leave the download in staging"
        );
```

- [ ] **Step 9: Commit**

```bash
git add src/runner.rs
git commit -m "feat: place artist-only manual downloads into the existing artist folder"
```

---

## Task 5: the explicit-album form threads the placement target

**Files:**

- Modify: `src/runner.rs` (`run_manual_mode`'s `process_album` call at `:2223`)

- [ ] **Step 1: Write the failing tests**

Add to the tests module in `src/runner.rs`:

```rust
    #[tokio::test]
    async fn manual_mode_places_an_explicit_album_into_the_existing_artist_folder() {
        let client = MockClient::new();
        *client.search_results.lock().unwrap() = vec![album_result("Test Artist", "New Album")];
        *client.write_files.lock().unwrap() = true;
        let (mut config, db, staging) = artist_only_fixture();
        let library = library_with(&[("Test Artist", "Present")]);
        config.library.paths = vec![library.path().to_string_lossy().into_owned()];
        config.discography.enabled = false;

        run_manual_mode(&client, Some("Test Artist"), Some("New Album"), false, &config, &db)
            .await
            .expect("manual album mode must complete");

        assert!(
            library
                .path()
                .join("Test Artist")
                .join("New Album")
                .join("01 - track.flac")
                .exists(),
            "the explicit album must be placed in the existing artist folder"
        );
        assert!(
            !staging.path().join("Test Artist--New Album").exists(),
            "a placed album leaves no staging copy"
        );
    }

    #[tokio::test]
    async fn manual_mode_with_an_explicit_album_does_not_walk_the_library() {
        let client = MockClient::new();
        *client.search_results.lock().unwrap() = vec![album_result("Test Artist", "New Album")];
        *client.write_files.lock().unwrap() = true;
        let (mut config, db, staging) = artist_only_fixture();
        let library = library_with(&[("Test Artist", "Present")]);
        config.library.paths = vec![library.path().to_string_lossy().into_owned()];
        config.discography.enabled = false;
        let capture = crate::test_support::LogCapture::start();

        run_manual_mode(&client, Some("Test Artist"), Some("New Album"), false, &config, &db)
            .await
            .expect("manual album mode must complete");

        // The scan announces its roots, and this fixture's library root is unique
        // to this test, so the assertion cannot be satisfied or broken by another
        // test's scan.
        let root = library.path().to_str().unwrap();
        let logs = capture.text();
        assert!(
            !logs
                .lines()
                .any(|line| line.contains("Library scan starting") && line.contains(root)),
            "the explicit-album form must resolve the artist folder without walking:\n{logs}"
        );
    }

    #[tokio::test]
    async fn manual_mode_with_an_explicit_album_keeps_staging_when_the_album_folder_exists() {
        let client = MockClient::new();
        *client.search_results.lock().unwrap() = vec![album_result("Test Artist", "Old Album")];
        *client.write_files.lock().unwrap() = true;
        let (mut config, db, staging) = artist_only_fixture();
        let library = library_with(&[("Test Artist", "Old Album")]);
        config.library.paths = vec![library.path().to_string_lossy().into_owned()];
        config.discography.enabled = false;

        run_manual_mode(&client, Some("Test Artist"), Some("Old Album"), false, &config, &db)
            .await
            .expect("manual album mode must complete");

        assert!(
            staging
                .path()
                .join("Test Artist--Old Album")
                .join("01 - track.flac")
                .exists(),
            "an existing album folder keeps the download in staging"
        );
        assert_eq!(
            std::fs::read(
                library
                    .path()
                    .join("Test Artist")
                    .join("Old Album")
                    .join("01 - track.flac")
            )
            .unwrap(),
            b"fake flac data",
            "the existing album folder is left untouched"
        );
    }

    #[tokio::test]
    async fn manual_mode_places_once_when_organize_is_on() {
        // Spec decision 10: placement runs regardless of `storage.organize` and
        // returns before the organize step, so the album is written exactly once
        // into the existing artist folder.
        let client = MockClient::new();
        *client.search_results.lock().unwrap() = vec![album_result("Test Artist", "New Album")];
        *client.write_files.lock().unwrap() = true;
        let (mut config, db, staging) = artist_only_fixture();
        let library = library_with(&[("Test Artist", "Present")]);
        config.library.paths = vec![library.path().to_string_lossy().into_owned()];
        config.storage.organize = true;
        config.discography.enabled = false;

        run_manual_mode(&client, Some("Test Artist"), Some("New Album"), false, &config, &db)
            .await
            .expect("manual album mode must complete");

        let album_dir = library.path().join("Test Artist").join("New Album");
        assert!(
            album_dir.join("01 - track.flac").exists(),
            "the album must be placed in the artist's folder"
        );
        assert!(
            !album_dir.join("01 - track (1).flac").exists(),
            "the organize block must not run after an early placement return"
        );
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p seakarr --lib manual_mode_places_an_explicit_album_into_the_existing_artist_folder`
Expected: FAIL — the album is still under `staging/Test Artist--New Album`

- [ ] **Step 3: Build and pass the target**

In `run_manual_mode`, in the `else` branch that handles an explicit album, immediately before the `process_album` call:

```rust
        let destination = resolve_manual_target(config, artist_name);
        let target = manual_place_target(&destination);
        let result = process_album(
            client,
            artist_name,
            album,
            ignore_processed,
            config,
            db,
            staging_dir,
            progress_ref,
            Some(&cancel),
            derived_library_count,
            target,
        )
        .await;
```

Delete the old `None, // target: manual mode has no library write` argument and its comment.

- [ ] **Step 4: Run the four tests**

Run: `cargo test -p seakarr --lib manual_mode_places_an_explicit_album_into_the_existing_artist_folder`
Expected: PASS, `1 passed`
Run: `cargo test -p seakarr --lib manual_mode_with_an_explicit_album_does_not_walk_the_library`
Expected: PASS, `1 passed`
Run: `cargo test -p seakarr --lib manual_mode_with_an_explicit_album_keeps_staging_when_the_album_folder_exists`
Expected: PASS, `1 passed`
Run: `cargo test -p seakarr --lib manual_mode_places_once_when_organize_is_on`
Expected: PASS, `1 passed`

- [ ] **Step 5: Run the manual-mode suite**

Run: `cargo test -p seakarr --lib manual_mode`
Expected: PASS, no failures

- [ ] **Step 6: Commit**

```bash
git add src/runner.rs
git commit -m "feat: place explicit-album manual downloads into the existing artist folder"
```

---

## Task 6: README

**Files:**

- Modify: `README.md` (the manual-mode bullet near `:31`, the `--artist` and `--album` rows near `:182`, the `--ignore-processed` paragraph near `:191`, the placement section near `:1061`)

- [ ] **Step 1: Extend the manual-mode bullet**

After the existing "Manual & batch modes" bullet's first sentence, add:

```markdown
  A manual run places each completed album into the artist's existing library folder when one
  exists under a configured `library.paths` entry; when the artist has no folder there, or the
  album's folder already exists, the download stays in `storage.staging_dir` and the run says
  why.
```

- [ ] **Step 2: Update the selector rows**

Append to the `--artist` row: `Manual downloads are placed into the artist's existing library folder when one exists; see [Placement](#placement).`
Append to the `--album` row: `The explicit form places into that folder too, without scanning the library; an album folder that already exists keeps the download in staging.`

- [ ] **Step 3: Correct the `--ignore-processed` paragraph**

Replace the sentence that begins "In manual and batch modes, use `storage.organize: true` if the replacement should be moved into the library;" with:

```markdown
In manual modes, a replacement is placed into the artist's existing library folder unless the
album folder already exists, in which case it stays in staging (there may be a different edition
there, and placement never replaces a readable file). In batch mode, use `storage.organize: true`
if the replacement should be moved into the library;
```

Keep the rest of the paragraph (the staging-recovery sentence for auto mode) unchanged.

- [ ] **Step 4: Note manual placement in the placement section**

In the placement section that begins "In the artist's own library folder, beside the albums that
artist already has.", add after the first sentence:

```markdown
Manual runs (`--artist` with or without `--album`) place the same way, except that they require the
artist folder to exist already: no manual run creates one, and an album folder that already exists
keeps the download in staging. The destination comes from a directory listing of the configured
roots, not from the scan, so the explicit-album form never walks the library.
```

- [ ] **Step 5: Lint the Markdown**

Run: `markdownlint --fix README.md`
Expected: exit 0, no output

- [ ] **Step 6: Commit**

```bash
git add README.md
git commit -m "docs: record manual placement into the existing artist folder"
```

---

## Task 7: Full verification

**Files:** none (verification only)

- [ ] **Step 1: Format and lint**

Run: `cargo fmt --check && cargo clippy -- -D warnings`
Expected: both exit 0, no warnings

- [ ] **Step 2: Full test suite**

Run: `cargo test`
Expected: exit 0; the seakarr lib suite grows by the tests added here (three in `organizer`, six in `discover`, two in the placement arm, three artist-only, four manual-mode), and no existing test fails — in particular the discover placement tests and the auto-mode upgrade tests.

- [ ] **Step 3: Coverage**

Run: `cargo llvm-cov -p seakarr --summary-only`
Expected: TOTAL lines at or above the 95 % project target. Note the pre-existing named debt in `client.rs` and `download.rs`; `runner.rs` and `organizer.rs` must not drop below their current figures.

- [ ] **Step 4: Pre-commit gate**

Run: `pre-commit run --all-files`
Expected: all hooks Passed

- [ ] **Step 5: Confirm the acceptance criteria from the spec**

Check each box against the work done: artist-only with a folder places; artist-only without one stages with the INFO line; the explicit-album form places without a walk; an existing album folder keeps the download in staging (both forms); no library path stages; a placement failure reports Failed and keeps staging (covered by the pre-existing discover test plus the unchanged arm); batch, auto and discover behaviour unchanged; all gates pass.

- [ ] **Step 6: Report**

Summarise: files changed, test counts, coverage, and the two behaviour notes — manual runs now write into a pre-existing artist folder, and an album folder that already exists is never merged into.

---

## Spec coverage map

| Spec requirement | Task |
| --- | --- |
| `resolve_artist_folder` (filesystem, sanitised + normalised match, config order, on-disk spelling) | 2 |
| `placement_album_dir` (pattern-exact album directory) | 1 |
| `LibraryTarget::Place` gains `skip_existing_album`; discover passes `false` | 3 |
| Existing album folder keeps the download in staging, with INFO | 3, 4, 5 |
| Placement runs regardless of `storage.organize` (decision 10) | 5 |
| Artist-only runs resolve once per run, thread the target, INFO when absent | 4 |
| Explicit-album form places without a walk | 5 |
| Placement failure keeps staging and reports Failed (unchanged arm) | 3 (untouched), 7 |
| Discover and auto unchanged | 3 Step 7, 7 Step 2 |
| README updates (manual bullet, selector rows, `--ignore-processed`, placement section) | 6 |
| fmt, clippy, tests, coverage, pre-commit | 7 |

## Self-review

- **Placeholder scan:** no deferrals; every code step shows the code, every command shows its expected result, and each test's fixture is spelled out.
- **Type consistency:** `placement_album_dir(library_root, pattern, artist_dir, album, first_downloaded) -> Option<PathBuf>` is identical in Tasks 1 and 3; `resolve_artist_folder(config, artist) -> Option<(PathBuf, String)>` is identical in Tasks 2, 4 and 5; `resolve_manual_target` / `manual_place_target` are defined once in Task 4 and reused in Task 5; `LibraryTarget::Place { root, artist_dir, skip_existing_album }` matches at all four construction sites (discover in Task 3, both artist-only callers via the helper in Task 4, `run_manual_mode` in Task 5); the gate calls `finish_library_write(config, db, &album_staging, artist, album, downloaded.len(), DownloadDestination::Staging(album_staging.clone()))`, matching the signature at `src/runner.rs:357`.
- **Ordering:** each task's tests are written before its implementation; Task 3 precedes Tasks 4 and 5 because the flag must exist before the manual targets can set it; Task 4 defines the helpers Task 5 uses.
- **Known fixture behaviour the plan relies on:** `MockClient` with `write_files` true writes `b"mock audio content"` to `dir/<basename>` (`src/client.rs:243`), while `library_with` writes `b"fake flac data"` — the byte assertions in Tasks 3 and 5 distinguish the two. `artist_only_fixture` sets `min_tracks = 1` (`src/runner.rs:4536`), so the completeness backstop cannot fire for the one-track fixtures.
