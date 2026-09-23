<!-- markdownlint-disable MD013 -->
# Remove organize and the organize pattern Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use sub-agents (recommended) to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking. If you dispatch, use one sub-agent per task with a fresh context and review its diff against the spec before moving on.

**Goal:** Delete `storage.organize` and `storage.organize_pattern` from the configuration, the code and the documentation, and replace what they did with the single placement mechanism, made able to find artist folders in a deep genre/type tree.

**Architecture:** One library writer (placement: `Place` / `StagingOnly` / `Upgrade`) with a fixed naming convention instead of a template. A new `discover::ArtistFolderIndex` walks the configured roots for directory names only, lazily on first use and once per run, and returns the artist folder's parent plus its on-disk name — the same `(parent, name)` pair the library scan already produces — so `LibraryTarget::Place` keeps its meaning. Auto and batch stop relying on the organize step and compute a placement target from that index.

**Tech Stack:** Rust 2021, `cargo`, `tracing`, `walkdir` (already a dependency), `tempfile` (dev-dependency), inline `#[cfg(test)]` modules plus `tests/pipeline_test.rs`.

**Spec:** `docs/agent/specs/2026-09-23-remove-organize-design.md` (approved, commit `1fec51c`).

**Plan location:** this repository keeps agent-generated plans in `docs/agent/plans/` (AGENTS.md rule 7), which overrides the writing-plans default of `docs/plans/`.

**All line numbers and code excerpts below are as of commit `1fec51c`.** Confirm the anchor text before deleting anything; if it has moved, search for the anchor rather than trusting the number.

## Scope check

The spec covers a single subsystem: the library-write path plus the artist-folder lookup it depends on. It is one plan, not several. No task is independently shippable except Task 1 (a self-contained config removal), which is why it comes first.

## File structure

| File | Responsibility after this plan | Change |
| --- | --- | --- |
| `src/config.rs` | Configuration schema and validation | Delete `StorageConfig::organize`, `StorageConfig::organize_pattern`, `default_organize_pattern`, the pattern validation block, the pattern fixtures/tests; add one reconciliation test |
| `src/organizer.rs` | Library writing: naming, copying, keep/replace policy, upgrade scoring | Delete the template engine, `organize_file`, `OrganizeInput`, the `pattern` plumbing and `ArtistComponent`; add `library_relative_path` and `album_dir_for`; rename `organize_name_from_stem` → `library_name_from_stem` |
| `src/discover.rs` | Library index, presence checks, artist-folder lookup, artist selection | Replace `resolve_artist_folder` with `ArtistFolderIndex` (lazy, cached, deep, deterministic) |
| `src/runner.rs` | Modes and per-album processing | Delete the organize step, `organize_allowed`, the album-only hard error's flag term; own `LibraryTarget` (no borrowed paths); compute placement targets for manual and automatic runs; explain staging outcomes |
| `src/main.rs` | CLI and batch mode | Batch builds a placement target per line through the same index |
| `tests/pipeline_test.rs` | End-to-end pipeline test | Comment wording only |
| `README.md` | Live user documentation | Rewrite the storage table, mode descriptions, run sequence, completion-line notes and log example |
| `src/filter.rs`, `src/client.rs`, `src/test_support.rs`, `src/scanner.rs`, `src/discs.rs`, `src/search.rs`, `src/discography/mod.rs`, `src/lib.rs` | Unchanged behaviour | Doc-comment wording only |

## Working conventions for every task

- Run tests as `cargo test <name-substring>` for one test, or `cargo test` for the workspace.
- Before every commit: `cargo fmt` then `cargo clippy -- -D warnings` then `cargo test`. All three must be clean; the project treats clippy warnings as failures.
- Commit messages follow the existing style: `refactor:`, `feat:`, `docs:`, `test:`.
- Never commit with `git add -A`; stage the files the task names.
- Tests go through public APIs. Do not call a private helper from a test.

---

### Task 1: Remove the two configuration keys

**Files:**

- Modify: `src/config.rs` (`StorageConfig` at `85-92`, `default_organize_pattern` at `362-364`, the validation block inside `validate` at `934-996`, fixtures at `1202`, `1296`, `1809`, tests at `2004-2060`, `2821-2960`)
- Test: `src/config.rs` (inline `mod tests`)

- [ ] **Step 1: Write the failing test**

Add to the inline test module in `src/config.rs`, next to `load_records_absolute_path_and_default_mode_line`:

```rust
    #[test]
    fn removed_storage_keys_are_dropped_by_reconciliation() {
        // Both keys are gone from the schema. An existing config that still
        // carries them must keep loading, and the reconciliation that runs on
        // load must write the file back without them — leaving them in place
        // would advertise settings that no longer do anything.
        let dir = TempDir::new().unwrap();
        let config_file = dir.path().join("seakarr.yml");
        let yaml = "\
soulseek:
  username: user
  password: pass
storage:
  staging_dir: downloads/staging
  organize: true
  organize_pattern: \"%album%/%artist%/%track% - %title%.%ext%\"
";
        fs::write(&config_file, yaml).unwrap();

        let config = Config::load(dir.path()).unwrap();

        assert_eq!(config.storage.staging_dir, "downloads/staging");
        let written = fs::read_to_string(&config_file).unwrap();
        assert!(
            !written.contains("organize"),
            "the reconciled config must not keep the removed keys:\n{written}"
        );
        assert!(
            dir.path().join("seakarr.yml.bak").exists(),
            "reconciliation must keep a backup of the original file"
        );
    }
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test removed_storage_keys_are_dropped_by_reconciliation`
Expected: FAIL on the `!written.contains("organize")` assertion — the keys still round-trip because they are struct fields, so the merged config equals the file and no rewrite happens.

- [ ] **Step 3: Delete the fields, the default and the validation**

In `src/config.rs`:

1. `StorageConfig` becomes:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageConfig {
    #[serde(default = "default_staging_dir")]
    pub staging_dir: String,
}
```

1. Delete the whole `fn default_organize_pattern` (`362-364`) and the `default_organize_pattern()` reference in the `Default for StorageConfig`-style initializer at `1202`.
2. Inside `validate`, delete both blocks: the pattern-containment check that starts with the comment `// The organize pattern is joined onto the library root.` and ends with the `storage.organize_pattern must be a non-empty relative path inside the library` error, and the trailing-separator check that starts `// A trailing separator names a directory` and ends with the `must name a file, not end with a path separator` error. Delete their comments with them.
3. Remove the `organize_pattern:` lines from the YAML fixtures at `1296` and `1809`.

- [ ] **Step 4: Delete the pattern tests**

Delete these tests and the `pattern_error` helper they share (`2004-2960`): `pattern_error`, `test_organize_pattern_must_stay_inside_the_library`, `test_organize_pattern_inside_the_library_is_accepted`, `test_organize_pattern_rejects_windows_root_only_forms`, `organize_pattern_must_have_an_ordinary_component`, `organize_pattern_must_name_a_file_not_a_directory`, `organize_pattern_is_rejected_when_it_escapes_the_library`, `organize_pattern_is_rejected_when_empty`, `organize_pattern_accepts_relative_paths_with_curdir_components`.

Replace the containment coverage with a test that the sanitised album folder can never escape the artist folder, which is the property the deleted validation existed to protect:

```rust
    #[test]
    fn album_folder_stays_inside_the_artist_folder() {
        // The pattern validation used to guarantee this for every configured
        // pattern. Placement now derives the album folder itself, so the
        // guarantee has to hold for hostile metadata instead of for config.
        let album_dir = crate::organizer::album_dir_for(
            std::path::Path::new("/library"),
            "Artist",
            "../../etc/passwd",
        )
        .expect("an artist folder name of one component is usable");

        assert!(
            album_dir.starts_with("/library/Artist"),
            "an album title must not escape the artist folder, got {album_dir:?}"
        );
        assert_eq!(
            album_dir.parent(),
            Some(std::path::Path::new("/library/Artist")),
            "the album folder is exactly one level under the artist folder, so a hostile \
             title cannot add a path segment"
        );
    }
```

- [ ] **Step 5: Run the tests**

Run: `cargo test --lib config`
Expected: PASS, including `removed_storage_keys_are_dropped_by_reconciliation` and `album_folder_stays_inside_the_artist_folder` (the second compiles only after Task 2 adds `album_dir_for`; if you are doing tasks strictly in order, this test fails to compile — that is expected and Task 2 Step 3 makes it pass. If you prefer a green tree per commit, move this one test into Task 2 Step 3.)

- [ ] **Step 6: Commit**

```bash
git add src/config.rs
git commit -m "refactor: remove storage.organize and storage.organize_pattern from the config"
```

---

### Task 2: Fixed naming replaces the template engine

**Files:**

- Modify: `src/organizer.rs` (`organize_name_from_stem` at `96`, `expand_pattern` at `119`, `expand_pattern_inner` at `145`, `OrganizeInput` at `280`, `organize_file` at `309`, `ArtistComponent` at `391`, `ExistingFile` at `404`, `copy_to_library` at `434`, `copy_into_library` at `546`, `placement_album_dir` at `514`, `LibraryWrite` at `536`, tests at `999`, `1036`, `1053`, `1987`)

- [ ] **Step 1: Write the failing test**

Add to the inline test module in `src/organizer.rs`:

```rust
    #[test]
    fn placement_writes_the_fixed_layout_without_a_pattern() {
        // The default pattern used to produce exactly this layout, so a
        // default-configured library must receive byte-identical paths after
        // the pattern is gone. The staged name exercises both naming rules:
        // the track number is zero-padded and the leading track token is
        // stripped from the title.
        let staging = tempfile::TempDir::new().unwrap();
        let library = tempfile::TempDir::new().unwrap();
        let staged = staging.path().join("2 - Track Two.flac");
        fs::write(&staged, b"fake flac data").unwrap();
        fs::create_dir_all(library.path().join("Artist")).unwrap();

        let outcome = place_into_library(
            std::slice::from_ref(&staged),
            library.path(),
            "Artist",
            "Album",
        )
        .unwrap();

        assert_eq!(
            outcome.album_dir,
            library.path().join("Artist").join("Album"),
            "the album folder is the sanitised album title under the artist folder"
        );
        assert_eq!(
            outcome.written,
            vec![library.path().join("Artist").join("Album").join("02 - Track Two.flac")],
            "the track number is zero-padded and the leading track token is stripped"
        );
    }
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test placement_writes_the_fixed_layout_without_a_pattern`
Expected: FAIL to compile — `place_into_library` still takes a pattern argument. That compile failure is the RED for this refactor.

- [ ] **Step 3: Add the fixed-layout helpers and switch the writers**

In `src/organizer.rs`, add next to `organize_name_from_stem`:

```rust
/// The library-relative path one staged file is written to.
///
/// The layout is fixed: the artist component, then the sanitised album title,
/// then `NN - Title.ext`. This is the code form of the default pattern the
/// configuration used to carry, so every library write produces the paths a
/// default-configured library already holds. Every metadata value except a
/// filesystem-derived artist name is sanitised, so remote metadata cannot inject
/// a path segment.
fn library_relative_path(artist_dir: &str, album: &str, track: &str, title: &str, ext: &str) -> PathBuf {
    Path::new(artist_dir)
        .join(sanitize_component(album))
        .join(format!(
            "{} - {}.{}",
            sanitize_component(track),
            sanitize_component(title),
            sanitize_component(ext)
        ))
}

/// The album folder a placement writes into for this artist folder and album
/// title, or `None` when the artist folder name is not usable as one path
/// component.
///
/// The single source of truth for the album folder: the writer below creates
/// files whose parent is exactly this path and the existing-album check reads
/// it, so the two cannot disagree.
pub fn album_dir_for(artist_parent: &Path, artist_dir: &str, album: &str) -> Option<PathBuf> {
    let mut components = Path::new(artist_dir).components();
    let single_normal = matches!(components.next(), Some(std::path::Component::Normal(_)))
        && components.next().is_none();
    if !single_normal {
        return None;
    }
    Some(
        artist_parent
            .join(artist_dir)
            .join(sanitize_component(album)),
    )
}
```

Then rework the writers:

1. Rename `organize_name_from_stem` to `library_name_from_stem` and update its callers (`src/organizer.rs` itself and `src/runner.rs`, where the organize step and the upgrade copy-back call it). Keep its body and change the doc comment's phrase `single source of truth for organize naming` to `single source of truth for library naming`.
2. `LibraryWrite` drops `pattern` and `artist_component`; `artist_dir` becomes the final artist component:

```rust
struct LibraryWrite<'a> {
    downloaded: &'a [PathBuf],
    library_root: &'a Path,
    artist_dir: &'a str,
    album: &'a str,
    existing_file: ExistingFile,
}
```

1. In `copy_into_library`, replace the `relative` computation with:

```rust
        let relative = library_relative_path(artist_dir, album, &track, &title, &ext);
```

and replace the destructuring and every later `artist` use with `artist_dir`.
4. `copy_to_library` sanitises its tag-derived artist once and passes it as the final component:

```rust
pub fn copy_to_library(
    downloaded: &[PathBuf],
    library_root: &Path,
    artist: &str,
    album: &str,
) -> Result<LibraryWriteOutcome> {
    let artist_dir = sanitize_component(artist);
    copy_into_library(LibraryWrite {
        downloaded,
        library_root,
        artist_dir: &artist_dir,
        album,
        existing_file: ExistingFile::ReplaceUnlessBetter,
    })
}
```

1. `place_into_library` loses the pattern parameter and validates the single component itself, then delegates:

```rust
pub fn place_into_library(
    downloaded: &[PathBuf],
    artist_parent: &Path,
    artist_dir: &str,
    album: &str,
) -> Result<LibraryWriteOutcome> {
    // The artist folder name comes from the library walk, which only ever produces
    // one component, but a caller could pass anything and a second component would
    // write outside the artist folder.
    if album_dir_for(artist_parent, artist_dir, album).is_none() {
        return Err(SeakarrError::Config(format!(
            "artist folder {artist_dir:?} must be a single path component"
        )));
    }
    copy_into_library(LibraryWrite {
        downloaded,
        library_root: artist_parent,
        artist_dir,
        album,
        existing_file: ExistingFile::KeepWhenValid,
    })
}
```

1. Delete `ArtistComponent` and `OrganizeInput` and `organize_file`: nothing uses them once `copy_into_library` calls `library_relative_path`. Delete the `pattern` parameter from `copy_to_library` and `place_into_library` signatures and update every caller (Task 4 handles the runner; until then the runner will not compile — that is why Task 2 and Task 4 must land together if you want a green tree, or keep the pattern parameter one commit longer and remove it in Task 4).

- [ ] **Step 4: Replace `placement_album_dir` with the fixed derivation**

Delete `placement_album_dir` (`514-530`). Its only caller is `existing_album_destination` in `src/runner.rs`; Task 4 Step 3 rewrites that caller to use `album_dir_for`, at which point the existing-album check needs no "strictly below the artist folder" special case, because the fixed derivation is always exactly `<artist folder>/<album>`.

- [ ] **Step 5: Adapt the pattern tests**

- Delete `test_expand_pattern`, `test_expand_pattern_with_spaces`, and `placement_album_dir_follows_a_pattern_that_shapes_the_album_folder`.
- Rename `test_delete_lesser_quality_reports_a_pattern_that_misses_the_album_folder` to `test_delete_lesser_quality_reports_an_album_folder_that_holds_no_files` and keep its assertions (the album folder it names now comes from the fixed layout).
- Keep every test that asserts sanitisation, disc-subfolder preservation, keep/replace policy and upgrade scoring exactly as they are; they are the proof that the fixed layout is byte-identical for the paths that matter.

- [ ] **Step 6: Run the tests**

Run: `cargo test --lib organizer`
Expected: PASS, including `placement_writes_the_fixed_layout_without_a_pattern`.

Run: `cargo clippy -- -D warnings`
Expected: no warnings (unused imports from the deleted engine must be removed).

- [ ] **Step 7: Commit**

```bash
git add src/organizer.rs
git commit -m "refactor: fix the library naming layout and drop the pattern engine"
```

---

### Task 3: A deep, cached artist-folder lookup

**Files:**

- Modify: `src/discover.rs` (`resolve_artist_folder` at `216-257`, its 12 tests at `646-810`, imports at `1-20`)
- Test: `src/discover.rs` (inline `mod tests`)

- [ ] **Step 1: Write the failing tests**

Add to the inline test module in `src/discover.rs`. The first one fails today, because `resolve_artist_folder` only lists a root's immediate children:

```rust
    #[test]
    fn an_artist_folder_five_levels_below_a_root_is_found() {
        // The operator's layout: /media/Music/<user>/<type>/<genre>/<subgenre>/<artist>.
        // The direct-child lookup cannot see it, which is why manual and automatic
        // placement never fired on a real library.
        let library = tempfile::TempDir::new().unwrap();
        let artist = library
            .path()
            .join("Paul")
            .join("Albums")
            .join("Rock")
            .join("Indie")
            .join("Radiohead");
        fs::create_dir_all(&artist).unwrap();
        let mut config = Config::default();
        config.library.paths = vec![library.path().to_string_lossy().into_owned()];

        let index = ArtistFolderIndex::new(&config);

        assert_eq!(
            index.find("Radiohead"),
            Some((
                library
                    .path()
                    .join("Paul")
                    .join("Albums")
                    .join("Rock")
                    .join("Indie"),
                "Radiohead".to_string()
            )),
            "the lookup must descend past a root's immediate children"
        );
    }

    #[test]
    fn an_ambiguous_artist_folder_picks_the_first_and_warns() {
        // Albums and Singles both holding the artist is legitimate, so the choice
        // must be deterministic and visible rather than silent.
        let library = tempfile::TempDir::new().unwrap();
        let albums = library.path().join("Albums").join("Radiohead");
        let singles = library.path().join("Singles").join("Radiohead");
        fs::create_dir_all(&albums).unwrap();
        fs::create_dir_all(&singles).unwrap();
        let mut config = Config::default();
        config.library.paths = vec![library.path().to_string_lossy().into_owned()];
        let capture = crate::test_support::LogCapture::start();

        let found = ArtistFolderIndex::new(&config).find("Radiohead");

        assert_eq!(found, Some((library.path().join("Albums"), "Radiohead".to_string())));
        assert!(
            capture.text().contains("more than one library folder matches this artist"),
            "an ambiguous match must warn:\n{}",
            capture.text()
        );
    }

    #[test]
    fn the_index_is_not_built_when_nothing_is_looked_up() {
        // A run that places nothing must not walk the library tree at all.
        let library = tempfile::TempDir::new().unwrap();
        let mut config = Config::default();
        config.library.paths = vec![library.path().join("missing").to_string_lossy().into_owned()];

        let index = ArtistFolderIndex::new(&config);

        assert!(!index.is_built(), "constructing the index must not walk anything");
    }
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test --lib discover`
Expected: FAIL to compile — `ArtistFolderIndex` does not exist. To see the behavioural RED as well, point the first test at the old API for one run: `resolve_artist_folder(&config, "Radiohead")` returns `None` for the nested fixture, which is the defect this task fixes.

- [ ] **Step 3: Implement `ArtistFolderIndex` and delete `resolve_artist_folder`**

At the top of `src/discover.rs` add `use std::sync::OnceLock;` and `use walkdir::WalkDir;` (keep the existing imports). Replace `resolve_artist_folder` (`216-257`) with:

```rust
/// Every artist folder under the configured library roots, built on first use and
/// reused for the rest of the run.
///
/// The walk reads **directory names only**: no file is opened and no tag is read,
/// which is what makes a lookup affordable in a deep
/// `<user>/<type>/<genre>/<subgenre>/<artist>` tree. Building is lazy, so a run
/// that never places anything never touches the library tree, and it happens at
/// most once per run.
pub struct ArtistFolderIndex {
    roots: Vec<PathBuf>,
    folders: OnceLock<BTreeMap<String, Vec<(PathBuf, String)>>>,
}

impl ArtistFolderIndex {
    /// An unbuilt index over the configured library roots, in configuration order.
    pub fn new(config: &Config) -> Self {
        Self {
            roots: config.library.paths.iter().map(PathBuf::from).collect(),
            folders: OnceLock::new(),
        }
    }

    /// Whether the walk has run. A run that places nothing never builds the index.
    pub fn is_built(&self) -> bool {
        self.folders.get().is_some()
    }

    /// The parent directory and on-disk name of this artist's folder, or `None`
    /// when no configured root holds one.
    ///
    /// Roots are searched in configuration order, and each root is walked
    /// depth-first with directory names in alphabetical order, so the choice never
    /// depends on filesystem order. When more than one folder matched, the first in
    /// that order wins and a WARN names it alongside the folders it skipped.
    pub fn find(&self, artist: &str) -> Option<(PathBuf, String)> {
        let key = artist_folder_key(artist)?;
        let folders = self.folders.get_or_init(|| walk_artist_folders(&self.roots));
        let candidates = folders.get(&key)?;
        let (parent, name) = candidates.first()?.clone();
        if let Some(skipped) = candidates.get(1..).filter(|rest| !rest.is_empty()) {
            let skipped = skipped
                .iter()
                .map(|(parent, name)| parent.join(name).display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            tracing::warn!(
                "{name}: more than one library folder matches this artist; placing into {} and skipping {skipped}",
                parent.join(&name).display()
            );
        }
        Some((parent, name))
    }
}

/// The comparison key for an artist folder name, or `None` when the name carries
/// nothing a filesystem can keep: a blank name, or one that sanitises away to the
/// placeholder (`***`), which must not match a folder whose own name did the same.
fn artist_folder_key(artist: &str) -> Option<String> {
    if artist.trim().is_empty() {
        return None;
    }
    let sanitized = sanitize_component(artist);
    if sanitized == sanitize_component("") {
        return None;
    }
    let key = normalize_catalog_key(&sanitized);
    (!key.is_empty()).then_some(key)
}

/// Walk every root's directories depth-first with sorted names, recording each
/// directory under the key its own name produces. Album folders land in the map
/// too and simply never match an artist unless one carries that name.
fn walk_artist_folders(roots: &[PathBuf]) -> BTreeMap<String, Vec<(PathBuf, String)>> {
    let mut folders: BTreeMap<String, Vec<(PathBuf, String)>> = BTreeMap::new();
    for root in roots {
        // A root that does not exist is not configured yet; any other failure is
        // worth naming, because otherwise the caller reports the artist as having
        // no folder rather than the root as unreadable.
        if !root.exists() {
            continue;
        }
        if let Err(error) = std::fs::read_dir(root) {
            tracing::warn!(
                "library root {} cannot be listed ({error}); skipping it",
                root.display()
            );
            continue;
        }
        for entry in WalkDir::new(root)
            .follow_links(false)
            .sort_by_file_name()
            .into_iter()
            .filter_map(Result::ok)
        {
            if entry.depth() == 0 || !entry.file_type().is_dir() {
                continue;
            }
            // A non-UTF-8 folder name cannot be compared with a UTF-8 artist, so it
            // is skipped rather than matched lossily.
            let Some(name) = entry.file_name().to_str() else {
                continue;
            };
            let Some(key) = artist_folder_key(name) else {
                continue;
            };
            let Some(parent) = entry.path().parent() else {
                continue;
            };
            folders
                .entry(key)
                .or_default()
                .push((parent.to_path_buf(), name.to_string()));
        }
    }
    folders
}
```

- [ ] **Step 4: Point the existing resolver tests at the index**

The 12 tests at `646-810` keep their fixtures, and each `resolve_artist_folder(&config, "X")` call becomes `ArtistFolderIndex::new(&config).find("X")`. Their expectations change only where they asserted a *configured root* as the first tuple element: the index returns the artist folder's **parent**, which for a direct-child fixture is the root itself, so those assertions stay valid unchanged. Do not delete any of them; they are the guards for blank names, placeholder names, non-UTF-8 names, unreadable roots, first-root preference, smallest-spelling preference and files named like the artist.

- [ ] **Step 5: Run the tests**

Run: `cargo test --lib discover`
Expected: PASS, including the three new tests. If `resolve_artist_folder` is still referenced anywhere, the compile error names the caller; every caller is updated in Task 4, so either land Tasks 3 and 4 together or keep a thin `resolve_artist_folder` wrapper for one commit and delete it in the next.

- [ ] **Step 6: Commit**

```bash
git add src/discover.rs
git commit -m "feat: find artist folders anywhere under the library roots"
```

---

### Task 4: One writer for every mode

**Files:**

- Modify: `src/runner.rs` (album-only guard `519-531`, `Place` arm `1001-1118`, organize block `1121-1182`, upgrade copy-back `~950`, auto target `1298-1310`, `resolve_manual_target` `1430-1436`, `manual_place_target` `1465-1477`, manual call sites `1688`, `1800`, `2406`, results loop `1350-1366`, tests mentioning `storage.organize`)
- Modify: `src/main.rs` (`run_batch_mode` `597-680`, comment at `626`, batch call site `652-660`)
- Test: `src/runner.rs`, `src/main.rs`, `tests/pipeline_test.rs`

- [ ] **Step 1: Write the failing tests**

Add to the inline test module in `src/runner.rs`. Use a unique artist name in each test that asserts on log output: `test_support::LogCapture` is process-wide, so an assertion keyed only on the message text can be satisfied by a concurrent test.

```rust
    #[tokio::test]
    async fn a_manual_run_places_into_a_nested_existing_artist_folder() {
        // The artist folder lives where the operator's tree puts it, five levels
        // below the configured root. The lookup must find it and place the album
        // inside it, creating nothing else.
        let client = MockClient::new();
        *client.search_results.lock().unwrap() = vec![album_result("Radiohead", "In Rainbows")];
        *client.write_files.lock().unwrap() = true;
        let (mut config, db, staging) = artist_only_fixture();
        let library = TempDir::new().unwrap();
        let artist_parent = library.path().join("Paul/Albums/Rock/Indie");
        std::fs::create_dir_all(artist_parent.join("Radiohead")).unwrap();
        config.library.paths = vec![library.path().to_string_lossy().into_owned()];
        config.discography.enabled = false;

        run_manual_mode(
            &client,
            Some("Radiohead"),
            Some("In Rainbows"),
            false,
            &config,
            &db,
        )
        .await
        .expect("a manual run completes");

        assert!(
            artist_parent
                .join("Radiohead")
                .join("In Rainbows")
                .join("01 - track.flac")
                .exists(),
            "the album must land in the nested artist folder"
        );
        assert!(
            !staging.path().join("Radiohead--In Rainbows").exists(),
            "a placed album leaves no staging copy"
        );
    }

    #[tokio::test]
    async fn the_automatic_target_places_into_an_existing_folder_and_stages_without_one() {
        // Auto mode and batch mode both route through this helper, so this is the
        // placement contract for both of them.
        let (mut config, _db, _staging) = artist_only_fixture();
        let library = TempDir::new().unwrap();
        let artist_parent = library.path().join("Paul/Albums/Rock/Indie");
        std::fs::create_dir_all(artist_parent.join("Radiohead")).unwrap();
        config.library.paths = vec![library.path().to_string_lossy().into_owned()];

        let index = discover::ArtistFolderIndex::new(&config);
        let found = automatic_place_target(&index, "Radiohead");

        assert!(
            matches!(
                found,
                LibraryTarget::Place {
                    skip_existing_album: false,
                    ..
                }
            ),
            "an existing artist folder is placed into, merging with what is already there: {found:?}"
        );
        assert!(
            matches!(
                automatic_place_target(&index, "No Such Artist 9c4e"),
                LibraryTarget::StagingOnly
            ),
            "an artist with no folder keeps its download in staging"
        );
    }

    #[tokio::test]
    async fn an_artist_with_no_folder_is_explained_once() {
        let client = MockClient::new();
        *client.search_results.lock().unwrap() =
            vec![album_result("Nowhere Artist 7f3a", "Nowhere Album")];
        *client.write_files.lock().unwrap() = true;
        let (mut config, db, staging) = artist_only_fixture();
        let library = TempDir::new().unwrap();
        std::fs::create_dir_all(library.path().join("Other Artist")).unwrap();
        config.library.paths = vec![library.path().to_string_lossy().into_owned()];
        config.discography.enabled = false;
        let capture = crate::test_support::LogCapture::start();

        run_manual_mode(
            &client,
            Some("Nowhere Artist 7f3a"),
            Some("Nowhere Album"),
            false,
            &config,
            &db,
        )
        .await
        .expect("a run with no artist folder completes");

        let logs = capture.text();
        let explained: Vec<&str> = logs
            .lines()
            .filter(|line| {
                line.contains("no library folder found under the configured library paths")
                    && line.contains("Nowhere Artist 7f3a")
            })
            .collect();
        assert_eq!(
            explained.len(),
            1,
            "the explanation is printed once per run, and only for this run's artist:\n{logs}"
        );
        assert!(
            staging
                .path()
                .join("Nowhere Artist 7f3a--Nowhere Album")
                .exists(),
            "the download stays in staging"
        );
    }

    #[tokio::test]
    async fn an_album_only_request_warns_instead_of_failing() {
        // The old guard returned a Config error before searching, and only when
        // storage.organize was on. Placement needs an artist folder, so an
        // album-only request cannot be filed - but it is still a valid download and
        // must warn, not abort.
        let client = MockClient::new();
        *client.search_results.lock().unwrap() = vec![album_result("Test Artist", "Album")];
        *client.write_files.lock().unwrap() = true;
        let (mut config, db, staging) = artist_only_fixture();
        let library = TempDir::new().unwrap();
        config.library.paths = vec![library.path().to_string_lossy().into_owned()];

        let result = process_album(
            &client,
            "",
            Some("Album"),
            false,
            &config,
            &db,
            staging.path(),
            None,
            None,
            None,
            None,
        )
        .await;

        assert!(
            matches!(result, Ok(AlbumOutcome::Downloaded { .. })),
            "an album-only request must download rather than fail: {result:?}"
        );
    }
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test --lib runner`
Expected: FAIL. `an_album_only_request_warns_instead_of_failing` fails because the guard still returns `Err(Config(..))` before the search, and the other three fail to compile until `automatic_place_target` and the owned `LibraryTarget` exist. Treat the guard failure as the behavioural RED and the compile failures as the structural RED for this task.

- [ ] **Step 3: Delete the organize step and the second writer**

In `src/runner.rs`:

1. Replace the album-only guard (`519-531`) with a warning that does not return early:

```rust
    // Placement writes into an artist folder that already exists, and an album-only
    // request names no artist, so it cannot be placed. Warn and continue: the
    // download is still valid, it just cannot be filed. The manual album-only form
    // takes the same route and is silent about it because its callers say so once
    // per run.
    if artist.trim().is_empty() && album.is_some() && !config.library.paths.is_empty() {
        tracing::warn!(
            "{}: an album-only request names no artist, so placement is impossible; the download stays in staging",
            album.unwrap_or("?")
        );
    }
```

1. Delete the whole organize block: from the comment `// Organize (if enabled)` through the closing brace of `if organize_allowed && config.storage.organize && !config.library.paths.is_empty() {`, including `library_album_dir`, `organize_ok`, `organize_allowed`, the second `library_write_refusal` branch and the `organize_file` loop. Then fix the `// Mark processed — only success if organize also succeeded.` comment: success now depends on the target arm alone.
2. In `existing_album_destination`, replace the `organizer::placement_album_dir(...)` call with the fixed derivation and delete the "strictly below the artist folder" special case, which the fixed layout makes unnecessary:

```rust
    let album_dir = organizer::album_dir_for(root, artist_dir, album.unwrap_or("Unknown"))?;
    album_dir.is_dir().then_some(album_dir)
```

1. In the upgrade arm, `organizer::copy_to_library(&downloaded, root, artist, album.unwrap_or("Unknown"))` loses its pattern argument.
2. Reword the partial-write warning that says `check storage.organize_pattern distinguishes the tracks` to `check the album title distinguishes the tracks`.

- [ ] **Step 4: Make `LibraryTarget` own its paths**

Replace the enum (`247-261`) and its `#[derive(Debug, Clone, Copy)]` with:

```rust
/// How the album pipeline should treat the library for this album.
///
/// Paths are owned: automatic runs compute a target before entering the future
/// that processes the album, so a borrowed target cannot outlive the lookup that
/// produced it.
#[derive(Debug, Clone)]
pub enum LibraryTarget {
    Upgrade {
        root: PathBuf,
        expected_tracks: usize,
    },
    Place {
        root: PathBuf,
        artist_dir: String,
        skip_existing_album: bool,
    },
    /// The album stays in staging: nothing is written to the library, because
    /// placement has no artist folder to write into.
    StagingOnly,
}
```

Then update: `process_album`, `process_album_internal` and `process_artist_album_work` keep taking `Option<LibraryTarget>`; the `Upgrade` arm's `root` is a `PathBuf`; `existing_album_destination` takes `&Path` (call `root.as_path()` at the call site); `manual_place_target` returns an owned target:

```rust
fn manual_place_target(destination: &Option<(PathBuf, String)>) -> LibraryTarget {
    match destination {
        Some((parent, artist_dir)) => LibraryTarget::Place {
            root: parent.clone(),
            artist_dir: artist_dir.clone(),
            skip_existing_album: true,
        },
        None => LibraryTarget::StagingOnly,
    }
}
```

Every construction site changes to owned values: `LibraryTarget::Place { root: some_path.as_path(), artist_dir: "X", .. }` becomes `root: some_path.to_path_buf(), artist_dir: "X".to_string()`. That includes the production sites — discover mode's per-artist target (the `LibraryTarget::Place` built from `artist.library_root` / `artist.artist_dir` just above `counters.artists_examined += 1`) and the upgrade branch — plus every test. Run `cargo build --all-targets` and fix each error it names; there are no others.

- [ ] **Step 5: Resolve targets through the index**

1. `resolve_manual_target` takes the index instead of the config:

```rust
/// The artist folder a manual run should place into, or `None` when the artist has
/// none under a configured library path. Silent: the explanation for a download
/// that stayed in staging is logged where the outcome is known.
fn resolve_manual_target(index: &discover::ArtistFolderIndex, artist: &str) -> Option<(PathBuf, String)> {
    // An album-only run names no artist: there is no folder to look for.
    if artist.trim().is_empty() {
        return None;
    }
    index.find(artist)
}
```

Each of the three call sites (`1688`, `1800`, `2406`) creates the index once in its enclosing mode function (`let artist_folders = discover::ArtistFolderIndex::new(config);`) and passes `&artist_folders`.

1. Add the shared automatic-run target helper next to `manual_place_target`:

```rust
/// The target for a mode that files its own downloads: place into the artist's
/// existing library folder, or keep the download in staging when the artist has
/// none. Placement never creates an artist folder, because the genre, type and
/// subgenre components above it are not derivable from album metadata.
pub fn automatic_place_target(index: &discover::ArtistFolderIndex, artist: &str) -> LibraryTarget {
    match index.find(artist) {
        Some((parent, artist_dir)) => LibraryTarget::Place {
            root: parent,
            artist_dir,
            skip_existing_album: false,
        },
        None => LibraryTarget::StagingOnly,
    }
}
```

1. Add the outcome-based explanation used by auto and batch:

```rust
/// Explain, at most once per artist per run, that a completed download stayed in
/// staging because the artist has no library folder. Outcome-based on purpose: a
/// run whose albums all failed must not claim its downloads are in staging.
pub fn explain_staging_outcome(
    artist: &str,
    config: &Config,
    outcome: &AlbumOutcome,
    explained: &mut BTreeSet<String>,
) {
    if !stayed_in_staging(outcome) {
        return;
    }
    if explained.insert(search::artist_identity_key(artist)) {
        log_no_artist_folder(artist, config, &None);
    }
}
```

1. In `run_auto_mode`, build the index before the album loop, and compute the target there instead of the bare upgrade branch:

```rust
    let artist_folders = discover::ArtistFolderIndex::new(config);
```

```rust
        let target = if config.library_upgrade.enabled {
            LibraryTarget::Upgrade {
                root: library_path.clone(),
                expected_tracks: library_track_count,
            }
        } else {
            automatic_place_target(&artist_folders, &artist)
        };
```

1. In the results loop (`1350-1366`), explain staging outcomes before recording them:

```rust
    let mut explained: BTreeSet<String> = BTreeSet::new();
    for (artist, album, result) in results {
        match result {
            Ok(outcome) => {
                explain_staging_outcome(&artist, config, &outcome, &mut explained);
                report.record(&artist, &album, outcome)
            }
```

- [ ] **Step 6: Give batch mode the same target**

In `src/main.rs`, in `run_batch_mode`: update the comment at `626` (it says batch performs no library scan; keep that, and note the lookup walks directory names only), then before the loop:

```rust
    // One artist-folder index per run: the lookup walks the configured roots for
    // directory names only, at most once, and only when a line needs it.
    let artist_folders = seakarr::discover::ArtistFolderIndex::new(config);
    let mut explained: BTreeSet<String> = BTreeSet::new();
```

```rust
        let target = seakarr::runner::automatic_place_target(&artist_folders, artist);
        match seakarr::runner::process_album(
            client,
            artist,
            album,
            ignore_processed,
            config,
            db,
            staging_dir,
            progress.as_ref(),
            Some(&cancel),
            None, // library_track_count (batch mode: no scanner data)
            Some(target),
        )
        .await
        {
            Ok(outcome) => {
                seakarr::runner::explain_staging_outcome(artist, config, &outcome, &mut explained);
                report.record(artist, album_display, outcome)
            }
```

Add `use std::collections::BTreeSet;` if `main.rs` does not already import it.

- [ ] **Step 7: Delete the organize-only tests and adapt the rest**

Delete the tests whose only subject is the removed writer: `organize_uses_shared_name_derivation`, `album_only_batch_organization_is_rejected_without_an_artist`, and any test that sets `config.storage.organize` purely to exercise the organize step. Then re-run the suite and read every remaining failure: a test that set `config.storage.organize = true` to prove a manual run does *not* organize has lost its subject, so assert the placement behaviour instead (manual runs place only into an existing artist folder, and creates nothing).

- [ ] **Step 8: Run the tests**

Run: `cargo test`
Expected: PASS, including the three new tests. Keep `tests/pipeline_test.rs` compiling: its two `None, // target:` comments become a placement target only if the pipeline test exercises placement — if it passes `None`, reword the comment to say the run writes nothing to the library.

- [ ] **Step 9: Commit**

```bash
git add src/runner.rs src/main.rs tests/pipeline_test.rs
git commit -m "feat: place automatic and batch downloads through the artist folder lookup"
```

---

### Task 5: Documentation and wording

**Files:**

- Modify: `README.md` (lines `15`, `158-159`, `214`, `254-255`, `463-466`, `483`, `587`, `699`, `751-753`, `772-773`, `911`, `1092`)
- Modify: doc comments in `src/filter.rs`, `src/client.rs`, `src/test_support.rs`, `src/scanner.rs`, `src/discs.rs`, `src/search.rs`, `src/discography/mod.rs`, `src/lib.rs`

- [ ] **Step 1: Delete the configuration rows and rewrite the mode text**

In `README.md`:

- Delete the `| organize | ... |` and `| organize_pattern | ... |` rows from the storage table.
- Delete the sentence at `158-159` claiming an album-only line fails before searching when `storage.organize` is enabled; the line now warns and keeps its download in staging.
- At `214`, drop the instruction to use `storage.organize: true`; batch and auto place automatically into an artist folder that already exists.
- At `463-466`, the completion-line section describes three cases: rewrite it as two — the album was placed into the artist's library folder, or it stayed in staging because the artist has no folder there (`library.paths` empty means staging is the destination, as before).
- At `483`, the `Organized:` DEBUG example is deleted; placement logs `Keeping existing file ...` and the placement INFO line already used by manual runs.
- In the run sequence at `699`, delete step 6 ("Organise — if `storage.organize` is enabled ...").
- At `751-753` and `772-773`, replace `whatever storage.organize says` with the placement rule: manual and automatic runs place into an artist folder that already exists and never create one.
- At `911` and `1092`, replace the organize-pattern references with the fixed layout: `<artist folder>/<album title>/NN - Title.ext`.
- Add to the mode section: the artist folder is found anywhere under a configured `library.paths` root, so a nested `genre/subgenre/artist` layout works, and a folder that appears more than once is reported.

- [ ] **Step 2: Reword the doc comments that name the removed step**

Each of these is comment text only; no behaviour changes. `filter.rs:208` ("on the placement and organize paths" → "on the placement path"), `client.rs:110` and `:241`, `test_support.rs:6`, `scanner.rs:63`, `discs.rs:2`, `search.rs:608`, `discography/mod.rs:580`, `lib.rs:16`. Leave `organizer` as the module name: it names the library-writing module, not the removed step.

- [ ] **Step 3: Check the docs**

Run: `rg -n -i 'organize' README.md`
Expected: no hits mentioning `storage.organize`, `organize_pattern`, the organize step or an `Organized:` log line. Historical files under `docs/specs/`, `docs/plans/` and the earlier `docs/agent/` specs are deliberately left as the record of what shipped.

Run: `markdownlint --fix README.md`
Expected: exit 0.

- [ ] **Step 4: Commit**

```bash
git add README.md src/filter.rs src/client.rs src/test_support.rs src/scanner.rs src/discs.rs src/search.rs src/discography/mod.rs src/lib.rs
git commit -m "docs: describe placement as the only library writer"
```

---

### Task 6: Full verification

**Files:** none modified; this task proves the tree.

- [ ] **Step 1: Run the project's QC order**

```bash
cargo fmt --check
cargo clippy -- -D warnings
cargo check --all-targets
cargo test
```

Expected: all clean. `cargo check --all-targets` must print **no warnings**; two pre-existing `chunks_exact` warnings in `src/main.rs` are allowed only as the known debt already recorded for the project — nothing new may appear.

- [ ] **Step 2: Run the remaining gates**

```bash
cargo llvm-cov -p seakarr --summary-only --fail-under-lines 95
cargo audit
cargo deny check
markdownlint README.md docs/agent/specs/2026-09-23-remove-organize-design.md
pre-commit run --all-files
```

Expected: coverage at or above 95 % lines, `cargo audit` exit 0 (two allowlisted advisories), `cargo deny check` reporting `advisories ok, bans ok, licenses ok, sources ok`, markdownlint exit 0, pre-commit exit 0.

- [ ] **Step 3: Prove the new tests bite (red-green)**

Each mutation is applied, the named test is run to see it fail, then the mutation is reverted and the test is run again to see it pass. Verify the revert with `git diff --stat` (empty) before moving on.

| Mutation | Test that must fail |
| --- | --- |
| Make `walk_artist_folders` skip entry depth > 1 | `an_artist_folder_five_levels_below_a_root_is_found` |
| Delete the ambiguity WARN from `find` | `an_ambiguous_artist_folder_picks_the_first_and_warns` |
| Flip `skip_existing_album` to `true` in `automatic_place_target` | `the_automatic_target_places_into_an_existing_folder_and_stages_without_one` |
| Restore the album-only early return | `an_album_only_request_warns_instead_of_failing` |
| Replace the dedupe in the manual explanation with an unconditional call | `an_artist_with_no_folder_is_explained_once` |

- [ ] **Step 4: Confirm the acceptance criteria**

```bash
rg -n 'storage\.organize|organize_pattern' src/ tests/ README.md
rg -n 'organize_file|OrganizeInput|expand_pattern|placement_album_dir|ArtistComponent' src/
```

Expected: no hits from either command.

- [ ] **Step 5: Commit anything the verification changed**

If the gates required fixes, stage only the files this plan owns and commit with `test:` or `refactor:` as appropriate. Then report the final numbers: test count, coverage percentage, and the reverted mutations with the evidence that each failed first.

---

## Notes for the implementer

- **Green-tree per commit:** Tasks 2, 3 and 4 change shared signatures, so a strict task-by-task commit sequence will not compile between tasks. Either land Tasks 2-4 as one commit with three reviewed diffs, or keep a temporary compatibility shim (the old `pattern: &str` parameter, or a `resolve_artist_folder` wrapper) for one commit and delete it in the next. Say which you chose in the commit body.
- **`ArtistFolderIndex::find` takes `&self`, not `&mut self`.** The spec sketched a mutable lookup; the implementation uses `OnceLock` so the index can be shared as an immutable reference, which is what lets auto mode resolve a target for every album before its future is spawned. Building stays lazy and at most once per run, exactly as the spec requires. Keep the spec's semantics and this signature.
- **Keep the upgrade tests untouched.** `copy_to_library`, `delete_lesser_quality_*` and the interrupted-upgrade recovery tests are the proof that the fixed layout writes the same names the pattern used to write.
- **Do not rename the `organizer` module.** The module still owns library writing, sanitisation and upgrade scoring; only the removed *step* disappears.
- **Do not touch historical documents.** `docs/specs/*`, `docs/plans/*` and the earlier `docs/agent/` specs stay as shipped.
- **Keep the README honest about staging.** With no artist folder, or with `library.paths` empty, the download stays in `storage.staging_dir` and says so once per run.
