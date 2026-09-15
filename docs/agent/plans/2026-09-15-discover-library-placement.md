<!-- markdownlint-disable MD013 -->
# Discover-Mode Library Placement Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use sub-agents (recommended) to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Place every album that `discover` mode completes into the artist's own
library folder (the directory the artist's existing albums were found in) using
the configured organize pattern, and delete the staging copy once the placement
succeeds.

**Architecture:** A new `LibraryTarget` enum replaces the loose
`target_library_path` parameter on `process_album`, so one copy-back block
serves both auto mode's gated upgrade (`Upgrade { root, expected_tracks }`) and
discover's unconditional placement (`Place { root, artist_dir }`). The
destination pair is carried from the library scan through `LibraryIndex` into a
`SelectedArtist` entry, which makes "artist selected but no destination"
unrepresentable. To make the destination correct for multi-disc and nested
libraries, the scanner resolves the artist folder, album folder, and library
location positionally, peeling one dedicated disc folder via a new
`discs::album_index` helper that the peer-side path parser also uses.

**Tech Stack:** Rust (stable), `walkdir`, `lofty`, `tokio`, `rusqlite`,
`tempfile`; tests are in-module `#[cfg(test)]` modules plus a `MockClient` and
`FakeDiscographyProvider` already present in `src/runner.rs`.

**Spec:** `docs/agent/specs/2026-09-15-discover-library-placement-design.md`

**Commit note:** the working tree already contains an uncommitted `0.23.0`
version bump in `Cargo.toml` and `Cargo.lock`. It is not part of this plan.
Stage only the files named in each task's commit step; never `git add -A` and
never `git commit -a`.

---

## Scope Check

Single subsystem: one defect (discover downloads never reach the library) plus
the path-resolution correction it depends on. No split into separate plans is
warranted — the scanner change, the `LibraryTarget` refactor, and the discover
wiring all serve the same delivered behaviour and cannot ship independently
without leaving the feature half-working.

---

## File Structure

| File | Responsibility | Change |
| --- | --- | --- |
| `src/discs.rs` | Disc-designator parsing shared by stager, organizer, and path parsers | Add `album_index` helper + tests |
| `src/search.rs` | Search, ranking, peer-share grouping | Refactor `artist_album_name` onto the shared helper (behaviour-preserving) |
| `src/scanner.rs` | Library walk, per-album format/quality scan, path-derived names and locations | Add `ScannedAlbum::artist_dir`; positional disc-aware derivation; tests |
| `src/discover.rs` | Pure presence index, artist selection, download budget, notices | Record destinations; add `artist_destination`; `SelectedArtist`; tests |
| `src/runner.rs` | Mode runners and the shared per-album pipeline | Add `LibraryTarget`; rework the copy-back block; gate auto mode at its caller; place in the discover loop; tests |
| `README.md` | User documentation | Document placement, the pattern's role, and the artist-only limit |

No new files. `src/mode.rs` is untouched. `src/config.rs` gains the
`storage.organize_pattern` containment check (see Task 8 Step 6 below) plus its
tests, and `src/main.rs` changes only in a comment that named the removed
parameter.

---

## Task 1: Shared disc-aware album-component helper

**Files:**

- Modify: `src/discs.rs` (add `album_index` after `parse_disc_label`, before the
  `strip_embedded_disc_marker` doc comment; add tests inside the existing
  `#[cfg(test)] mod tests`)

- [ ] **Step 1: Write the failing tests**

Add to the `#[cfg(test)] mod tests` block in `src/discs.rs`:

```rust
    #[test]
    fn album_index_finds_the_album_component_for_flat_and_nested_paths() {
        assert_eq!(album_index(&["Artist", "Album", "01 - track.flac"]), Some(1));
        assert_eq!(
            album_index(&["Genre", "Artist", "Album", "01 - track.flac"]),
            Some(2)
        );
        assert_eq!(
            album_index(&["A", "B", "Artist", "Album", "01 - track.flac"]),
            Some(3)
        );
    }

    #[test]
    fn album_index_peels_one_dedicated_disc_folder() {
        assert_eq!(
            album_index(&["Artist", "Album", "CD 01", "01 - track.flac"]),
            Some(1)
        );
        assert_eq!(
            album_index(&["Genre", "Artist", "Album", "{cd2}", "01 - track.flac"]),
            Some(2)
        );
    }

    #[test]
    fn album_index_declines_when_no_artist_component_would_remain() {
        // The album component is itself a disc folder directly under the
        // artist folder: whether that is an album folder or unusable is the
        // caller's decision, so the helper declines instead of guessing.
        assert_eq!(album_index(&["Artist", "CD 01", "01 - track.flac"]), None);
        // Two components cannot supply an artist folder, an album folder, and
        // a file.
        assert_eq!(album_index(&["Album", "01 - track.flac"]), None);
        assert_eq!(album_index(&["01 - track.flac"]), None);
        assert_eq!(album_index(&[]), None);
    }

    #[test]
    fn album_index_does_not_mistake_an_ordinary_folder_for_a_disc_folder() {
        assert_eq!(
            album_index(&["Artist", "Album (Disc 1)", "01 - track.flac"]),
            Some(1)
        );
        assert_eq!(
            album_index(&["Artist", "Album", "01 - track.flac"]),
            Some(1)
        );
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

```bash
cargo test --lib discs::tests::album_index 2>&1 | tail -20
```

Expected: compile error, `cannot find function 'album_index' in this scope`.

- [ ] **Step 3: Write the implementation**

Add to `src/discs.rs`, immediately after `parse_disc_label`:

```rust
/// Index of the album component in a slash-split path, peeling one dedicated
/// disc folder.
///
/// The album component is the one holding the files. A dedicated disc folder
/// ("CD 01", "Disc 2") is stepped over, so the album is the folder above it.
/// Embedded markers ("Gold (Disc 1)") are not peeled here — the album component
/// is the folder holding the files, and each caller decides what to do with a
/// marker: the peer-side parser folds it with `strip_embedded_disc_marker`,
/// while the library scanner keeps the on-disk folder name verbatim.
///
/// Returns `None` when no artist component would remain above the album, which
/// leaves the caller to decide whether that shape is an album folder (a library
/// folder named like a disc) or unusable (a peer share path).
pub fn album_index(components: &[&str]) -> Option<usize> {
    let index = components.len().checked_sub(2)?;
    let index = if is_disc_folder(components[index]) {
        index.checked_sub(1)?
    } else {
        index
    };
    (index > 0).then_some(index)
}
```

- [ ] **Step 4: Run the tests to verify they pass**

```bash
cargo test --lib discs:: 2>&1 | tail -20
```

Expected: all `discs` tests pass, including the new `album_index_*` tests.

- [ ] **Step 5: Commit**

```bash
git add src/discs.rs
git commit -m "feat: add disc-aware album component helper"
```

---

## Task 2: Share the helper with the peer-side path parser

**Files:**

- Modify: `src/search.rs:672-691` (`artist_album_name`)

This is a behaviour-preserving refactor, so the order is: prove green, change,
prove still green.

- [ ] **Step 1: Run the existing peer grouping tests to establish green**

```bash
cargo test --lib search::tests::test_group_artist_results 2>&1 | tail -20
```

Expected: 2 tests pass
(`test_group_artist_results_separates_albums_and_merges_discs`,
`test_group_artist_results_collapses_one_release_across_folder_variants`).

- [ ] **Step 2: Refactor onto the shared helper**

Replace the body of `artist_album_name` in `src/search.rs` with:

```rust
/// Extract a logical album name from a share-relative file path.
fn artist_album_name(path: &str, artist: &str) -> Option<String> {
    let components: Vec<&str> = path
        .split(['/', '\\'])
        .filter(|part| !part.is_empty())
        .collect();
    let album_index = crate::discs::album_index(&components)?;
    if !artist_directory_matches(&components[album_index - 1..album_index], artist) {
        return None;
    }
    let leaf = components[album_index];
    let album = crate::discs::strip_embedded_disc_marker(leaf).unwrap_or_else(|| leaf.to_owned());
    let album = album.trim();
    (!album.is_empty()).then(|| album.to_owned())
}
```

The helper's `None` replaces the removed `checked_sub(2)?`, the
`checked_sub(1)?`, and the `if album_index == 0 { return None; }` guard, whose
three conditions produced exactly the same refusals.

- [ ] **Step 3: Run the tests to verify they still pass**

```bash
cargo test --lib search:: 2>&1 | tail -20
```

Expected: all `search` tests pass, unchanged. In particular
`test_group_artist_results_separates_albums_and_merges_discs` still reports only
`["Album One", "Album Two"]`, still excludes `Test Artist\01 - loose.flac`
(index 0 → `None`) and `Test Artist\Other Artist\Album Three\01 - nested.flac`
(artist component mismatch), and still merges `CD 01`/`CD 02` into one album.

- [ ] **Step 4: Commit**

```bash
git add src/search.rs
git commit -m "refactor: share the disc-aware album rule with the peer parser"
```

---

## Task 3: Positional, disc-aware path derivation in the scanner

**Files:**

- Modify: `src/scanner.rs:12-30` (struct), `src/scanner.rs:70-150` (derivation),
  `src/scanner.rs:270-300` (test literals), `src/scanner.rs:360-400`
  (nested-location test expectations), `src/discover.rs:315-330` (test helper
  literal)

- [ ] **Step 1: Add the `artist_dir` field and populate it from the current
  derivation**

In `src/scanner.rs`, add the field to `ScannedAlbum` after `album`:

```rust
pub struct ScannedAlbum {
    pub path: PathBuf,
    pub artist: String,
    pub album: String,
    /// On-disk name of the artist folder this album was found in. Path-derived
    /// (unlike `artist`, which prefers the embedded tag), so a caller that
    /// writes back into the library reuses the folder that already exists
    /// instead of creating a second spelling beside it.
    pub artist_dir: String,
    /// Total number of audio files grouped into this album (all formats).
    pub track_count: usize,
```

In the same function, keep this step minimal: alongside the existing `let artist
= components[0].to_string();` / `let album = components[1].to_string();` lines
add

```rust
            // Placeholder derivation, replaced in Step 4: the artist folder is
            // the component the old rule treated as the artist.
            let artist_dir = components[0].to_string();
```

and add `artist_dir: artist_dir.clone(),` to the `ScannedAlbum { .. }` literal
inside `.or_insert_with(|| ...)` (around line 140).

Then update the two `ScannedAlbum` literals in the scanner's own tests
(`src/scanner.rs:277` and `:287`) and the one in `src/discover.rs:316` with a
matching `artist_dir: "Artist".into(),` / `artist_dir: artist.to_string(),`
field so the crate compiles.

- [ ] **Step 2: Run the suite to confirm the mechanical change is green**

```bash
cargo test --lib 2>&1 | tail -20
```

Expected: PASS (no behaviour changed yet).
`test_find_albums_to_upgrade_preserves_nested_album_location` still asserts
`artist == "Pop"` and `album == "Alesha Dixon"`.

- [ ] **Step 3: Write the failing behaviour tests**

Add to the `#[cfg(test)] mod tests` block in `src/scanner.rs`:

```rust
    #[test]
    fn test_scan_resolves_nested_layout_names_positionally() {
        let dir = TempDir::new().unwrap();
        let album_dir = dir.path().join("Genre").join("Artist").join("Album");
        fs::create_dir_all(&album_dir).unwrap();
        fs::write(album_dir.join("01 - track.flac"), b"fake flac data").unwrap();

        let albums = scan_library(&library_paths(dir.path()), &FilterConfig::default()).unwrap();
        assert_eq!(albums.len(), 1);
        assert_eq!(albums[0].artist, "Artist");
        assert_eq!(albums[0].album, "Album");
        assert_eq!(albums[0].artist_dir, "Artist");
        assert_eq!(albums[0].path, dir.path().join("Genre"));
    }

    #[test]
    fn test_scan_steps_over_one_disc_folder() {
        let dir = TempDir::new().unwrap();
        for disc in ["CD 01", "CD 02"] {
            let disc_dir = dir.path().join("Artist").join("Album").join(disc);
            fs::create_dir_all(&disc_dir).unwrap();
            fs::write(disc_dir.join("01 - track.flac"), b"fake flac data").unwrap();
        }

        let albums = scan_library(&library_paths(dir.path()), &FilterConfig::default()).unwrap();
        assert_eq!(albums.len(), 1, "both discs are one album");
        assert_eq!(albums[0].artist, "Artist");
        assert_eq!(albums[0].album, "Album");
        assert_eq!(albums[0].artist_dir, "Artist");
        assert_eq!(albums[0].path, dir.path().to_path_buf());
        assert_eq!(albums[0].track_count, 2);
    }

    #[test]
    fn test_scan_steps_over_a_disc_folder_in_a_nested_layout() {
        let dir = TempDir::new().unwrap();
        let disc_dir = dir
            .path()
            .join("Genre")
            .join("Artist")
            .join("Album")
            .join("Disc 2");
        fs::create_dir_all(&disc_dir).unwrap();
        fs::write(disc_dir.join("01 - track.flac"), b"fake flac data").unwrap();

        let albums = scan_library(&library_paths(dir.path()), &FilterConfig::default()).unwrap();
        assert_eq!(albums.len(), 1);
        assert_eq!(albums[0].artist, "Artist");
        assert_eq!(albums[0].album, "Album");
        assert_eq!(albums[0].artist_dir, "Artist");
        assert_eq!(albums[0].path, dir.path().join("Genre"));
    }

    #[test]
    fn test_scan_keeps_a_disc_named_folder_directly_under_the_artist() {
        let dir = TempDir::new().unwrap();
        let album_dir = dir.path().join("Artist").join("CD 01");
        fs::create_dir_all(&album_dir).unwrap();
        fs::write(album_dir.join("01 - track.flac"), b"fake flac data").unwrap();

        let albums = scan_library(&library_paths(dir.path()), &FilterConfig::default()).unwrap();
        assert_eq!(
            albums.len(),
            1,
            "an album folder named like a disc must stay in the index"
        );
        assert_eq!(albums[0].artist, "Artist");
        assert_eq!(albums[0].album, "CD 01");
        assert_eq!(albums[0].artist_dir, "Artist");
        assert_eq!(albums[0].path, dir.path().to_path_buf());
    }

    #[test]
    fn test_scan_keeps_an_embedded_marker_in_the_album_folder_name() {
        // The album name is the identity the auto-mode upgrade copies into and
        // the root the quality-deletion pass walks, so it is kept verbatim.
        let dir = TempDir::new().unwrap();
        let album_dir = dir.path().join("Artist").join("Gold (Disc 1)");
        fs::create_dir_all(&album_dir).unwrap();
        fs::write(album_dir.join("01 - track.flac"), b"fake flac data").unwrap();

        let albums = scan_library(&library_paths(dir.path()), &FilterConfig::default()).unwrap();
        assert_eq!(albums.len(), 1);
        assert_eq!(albums[0].artist, "Artist");
        assert_eq!(albums[0].album, "Gold (Disc 1)");
        assert_eq!(albums[0].artist_dir, "Artist");
        assert_eq!(albums[0].path, dir.path().to_path_buf());
    }
```

- [ ] **Step 4: Run the new tests to verify they fail**

```bash
cargo test --lib scanner::tests 2>&1 | tail -30
```

Expected failures: `test_scan_resolves_nested_layout_names_positionally` (gets
`"Genre"`/`"Artist"`), `test_scan_steps_over_one_disc_folder` (gets `artist ==
"Artist"`, `album == "CD 01"`, `path == <root>/Artist`),
`test_scan_steps_over_a_disc_folder_in_a_nested_layout`,
`test_scan_keeps_an_embedded_marker_in_the_album_folder_name`.
`test_scan_keeps_a_disc_named_folder_directly_under_the_artist` passes both
before and after (it is the preserved shape).

- [ ] **Step 5: Replace the derivation with the positional rule**

In `src/scanner.rs`, replace the block that currently reads

```rust
            // Infer artist/album from directory structure: <root>/Artist/Album/tracks
            let relative = path.strip_prefix(lib_path).unwrap_or(path);
            let components: Vec<&str> = relative.iter().filter_map(|c| c.to_str()).collect();

            if components.len() < 3 {
                continue; // Need at least Artist/Album/file
            }
            let artist = components[0].to_string();
            let album = components[1].to_string();
```

with

```rust
            // Infer the artist folder, album folder, and library location from
            // the directory structure. The album folder is the one holding the
            // files, one dedicated disc folder is stepped over so a multi-disc
            // album resolves to its album folder, and the artist folder is the
            // one directly above it. For a nested layout such as
            // <root>/Genre/Artist/Album this keeps every component correct
            // instead of naming the genre after the artist.
            let relative = path.strip_prefix(lib_path).unwrap_or(path);
            let components: Vec<&str> = relative.iter().filter_map(|c| c.to_str()).collect();

            let album_index = match crate::discs::album_index(&components) {
                Some(index) => index,
                // A folder named like a disc directly under the artist folder
                // is an album folder here, not a disc of one, so keep the
                // unpeeled reading rather than dropping a real album from the
                // index. Anything with no album component left at all is not a
                // library album layout and is skipped.
                None => match components.len().checked_sub(2) {
                    Some(index) if index >= 1 => index,
                    _ => continue, // Need at least Artist/Album/file
                },
            };
            let artist_dir = components[album_index - 1].to_string();
            // Keep the on-disk album folder name verbatim. It is the identity
            // the auto-mode upgrade copies into and the root the
            // quality-deletion pass walks, so stripping an embedded marker here
            // would move the copy into a new folder and leave the replaced
            // files behind in the old one.
            let album = components[album_index].to_string();
```

Then replace the tag-preference and location lines

```rust
            // Prefer tag metadata over directory name
            let final_artist = tag_artist.unwrap_or(artist);
            let final_album = tag_album.unwrap_or(album);

            let key = (final_artist.clone(), final_album.clone());
```

with

```rust
            // Prefer tag metadata over directory name
            let final_artist = tag_artist.unwrap_or_else(|| artist_dir.clone());
            let final_album = tag_album.unwrap_or(album);

            let key = (final_artist.clone(), final_album.clone());
```

and replace the `album_location` computation

```rust
            let album_location = components
                .get(..components.len().saturating_sub(3))
                .map(|extra| {
                    extra
                        .iter()
                        .fold(lib_path.to_path_buf(), |acc, c| acc.join(c))
                })
                .unwrap_or_else(|| lib_path.to_path_buf());
```

with

```rust
            let album_location = components[..album_index - 1]
                .iter()
                .fold(lib_path.to_path_buf(), |acc, c| acc.join(c));
```

Finally drop the now-removed placeholder line from Step 1 (`let artist_dir =
components[0].to_string();`) and keep `artist_dir: artist_dir.clone(),` in the
`.or_insert_with` literal.

- [ ] **Step 6: Correct the pre-existing nested-location test**

The old expectations encoded the broken fallback. In
`test_find_albums_to_upgrade_preserves_nested_album_location` (`src/scanner.rs`,
the test that writes `<root>/Pop/Alesha Dixon/The Alesha Show/01 - Track.ogg`),
update the two assertions and their comments:

```rust
        assert_eq!(to_upgrade[0].0, "Alesha Dixon"); // artist folder, not the genre
        assert_eq!(to_upgrade[0].1, "The Alesha Show"); // album folder
```

and replace the stale comment above them with:

```rust
        // Artist and album now come from the two folders above the file, so a
        // nested layout no longer reports the genre as the artist.
```

- [ ] **Step 7: Run the scanner and discover tests**

```bash
cargo test --lib scanner:: 2>&1 | tail -20
cargo test --lib discover:: 2>&1 | tail -20
```

Expected: PASS. `discover::tests` keeps passing because its `scanned()` helper
supplies `artist_dir` and only the index fields it already used.

- [ ] **Step 8: Commit**

```bash
git add src/scanner.rs src/discover.rs
git commit -m "fix: resolve library artist and album folders positionally"
```

---

## Task 4: Carry the destination through the library index

**Files:**

- Modify: `src/discover.rs:14-90` (`IndexedArtist`, `LibraryIndex`,
  `build_index`), `src/discover.rs:100-185` (`ArtistSelection`,
  `select_artists`), `src/discover.rs:310-620` (tests)

- [ ] **Step 1: Write the failing tests**

Add to `src/discover.rs`'s `#[cfg(test)] mod tests`, and extend the existing
`scanned()` helper so a destination can be set:

```rust
    fn scanned(artist: &str, album: &str) -> ScannedAlbum {
        scanned_at(artist, album, "/library", artist)
    }

    fn scanned_at(artist: &str, album: &str, root: &str, artist_dir: &str) -> ScannedAlbum {
        ScannedAlbum {
            path: PathBuf::from(root),
            artist: artist.to_string(),
            album: album.to_string(),
            artist_dir: artist_dir.to_string(),
            track_count: 1,
            needs_upgrade: 0,
            min_bitrate: Some(900),
            max_bitrate: Some(900),
            formats: vec!["flac".to_string()],
        }
    }

    fn selected_names(selection: &ArtistSelection) -> Vec<&str> {
        selection
            .artists
            .iter()
            .map(|artist| artist.name.as_str())
            .collect()
    }

    #[test]
    fn destination_is_recorded_per_artist_from_the_scan() {
        let index = build_index(&[
            scanned_at("Metallica", "72 Seasons", "/library/Metal", "Metallica"),
            scanned_at("Metallica", "Reload", "/library/Metal", "Metallica"),
        ]);
        assert_eq!(
            index.artist_destination("metallica"),
            Some(("/library/Metal", "Metallica"))
        );
    }

    #[test]
    fn destination_majority_wins_and_ties_break_alphabetically() {
        let index = build_index(&[
            scanned_at("Artist", "One", "/library/Metal", "Artist"),
            scanned_at("Artist", "Two", "/library/Metal", "Artist"),
            scanned_at("Artist", "Three", "/library/Collections", "Artist"),
            scanned_at("Artist", "Four", "/library/Zoo", "Artist"),
        ]);
        assert_eq!(
            index.artist_destination("artist"),
            Some(("/library/Metal", "Artist")),
            "two albums beat one, and the single-album tie breaks alphabetically"
        );
    }

    #[test]
    fn destination_keeps_the_on_disk_artist_folder_spelling() {
        let index = build_index(&[scanned_at(
            "Guns 'n' Roses",
            "Appetite for Destruction",
            "/library/Rock",
            "Guns N Roses",
        )]);
        assert_eq!(
            index.artist_destination("guns 'n' roses"),
            Some(("/library/Rock", "Guns N Roses")),
            "the folder that exists on disk wins over the tag spelling"
        );
    }

    #[test]
    fn an_unknown_artist_has_no_destination() {
        assert_eq!(build_index(&[]).artist_destination("nobody"), None);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

```bash
cargo test --lib discover::tests 2>&1 | tail -20
```

Expected: compile errors — `ScannedAlbum` has no `artist_dir` field in the
fixture (`scanned_at` fails to compile), and `artist_destination` does not
exist.

- [ ] **Step 3: Implement destinations and the selection type**

In `src/discover.rs`, extend `IndexedArtist`:

```rust
struct IndexedArtist {
    /// Original spelling to number of albums seen under it.
    spellings: BTreeMap<String, usize>,
    /// Normalised album titles.
    albums: BTreeSet<String>,
    /// Library root and on-disk artist folder to number of albums found there.
    /// A library whose artist sits under one genre root has a single entry; an
    /// artist split across roots has several and the majority wins.
    destinations: BTreeMap<(String, String), usize>,
}
```

Add the accessor to `impl LibraryIndex`, after `artist_name`:

```rust
    /// The library root and on-disk artist folder to place this artist's
    /// downloads under: the pair covering the most albums, with ties broken
    /// alphabetically so the destination never depends on walk order.
    pub fn artist_destination(&self, artist_key: &str) -> Option<(&str, &str)> {
        let entry = self.artists.get(artist_key)?;
        entry
            .destinations
            .iter()
            .min_by_key(|((root, directory), albums)| {
                (std::cmp::Reverse(**albums), root.as_str(), directory.as_str())
            })
            .map(|((root, directory), _)| (root.as_str(), directory.as_str()))
    }
```

In `build_index`, inside the per-album loop after
`entry.albums.insert(album_key);`, record the destination:

```rust
        *entry
            .destinations
            .entry((
                album.path.to_string_lossy().into_owned(),
                album.artist_dir.clone(),
            ))
            .or_insert(0) += 1;
```

Replace `ArtistSelection` with:

```rust
/// One artist selected for gap filling: what to ask MusicBrainz, and where the
/// completed downloads belong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedArtist {
    /// Spelling to query MusicBrainz.
    pub name: String,
    /// Parent directory of the artist's existing library folder.
    pub library_root: PathBuf,
    /// On-disk name of the artist's existing library folder.
    pub artist_dir: String,
}

/// The artists selected for gap filling, plus the exclusions that were applied.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ArtistSelection {
    /// Library spellings to query, in normalised-key order, each with the
    /// destination its downloads belong under.
    pub artists: Vec<SelectedArtist>,
    /// Library spellings skipped because an exclusion matched their key.
    pub excluded: Vec<String>,
}
```

and in the `select_artists` loop, replace
`selection.artists.push(name.to_string());` with:

```rust
        // Every indexed album registers a destination, so the lookup cannot
        // fail for a key drawn from `artist_keys`. The guard is belt and
        // braces, never a silent drop of a real artist, and it warns rather
        // than asserting so an invariant breach stays visible in release
        // builds.
        let Some((library_root, artist_dir)) = index.artist_destination(key) else {
            tracing::warn!("artist {key:?} has albums but no destination; skipping");
            continue;
        };
        selection.artists.push(SelectedArtist {
            name: name.to_string(),
            library_root: PathBuf::from(library_root),
            artist_dir: artist_dir.to_string(),
        });
```

Add `use std::path::PathBuf;` to the module imports at the top of
`src/discover.rs` (next to `use std::collections::{BTreeMap, BTreeSet};`).

- [ ] **Step 4: Update the existing selection assertions for the new type**

In `src/discover.rs`'s tests, replace every direct comparison of
`selection.artists` against a string slice with `selected_names(&selection)`:

```rust
        assert_eq!(
            selected_names(&selection),
            ["Alpha Artist", "Beta Band", "Live", "Live Band"]
        );
```

Apply the same change to `exclusions_match_the_whole_key_not_a_substring`,
`exclusion_matching_ignores_case_and_whitespace`,
`a_filter_narrows_the_run_to_one_artist`,
`an_exclusion_that_matches_nothing_excludes_nothing`, and
`an_explicit_filter_overrides_the_exclusion_list`. Then add:

```rust
    #[test]
    fn a_selected_artist_carries_its_destination() {
        let index = build_index(&[scanned_at(
            "Beta Band",
            "Album One",
            "/library/Indie",
            "Beta Band",
        )]);
        let selection = select_artists(&index, &[], None).unwrap();
        assert_eq!(
            selection.artists,
            vec![SelectedArtist {
                name: "Beta Band".to_string(),
                library_root: PathBuf::from("/library/Indie"),
                artist_dir: "Beta Band".to_string(),
            }]
        );
    }
```

- [ ] **Step 5: Run the discover tests**

```bash
cargo test --lib discover:: 2>&1 | tail -20
```

Expected: PASS, including the four new destination tests and the selection
tests.

- [ ] **Step 6: Commit**

```bash
git add src/discover.rs
git commit -m "feat: carry the library destination through artist selection"
```

---

## Task 5: Introduce `LibraryTarget` and split upgrade from placement

**Files:**

- Modify: `src/runner.rs:145-230` (public `process_album` and
  `process_album_internal` signatures), `src/runner.rs:560-665` (the copy-back
  block), `src/runner.rs:834-865` (auto-mode caller), `src/runner.rs:4510-4545`
  (the library-upgrade test call site), `src/runner.rs:1800-1900` (test module
  imports)

- [ ] **Step 1: Add a characterisation test for auto mode with the flag off**

Add to `src/runner.rs`'s test module (near the other library-upgrade tests):

```rust
    #[tokio::test]
    async fn auto_mode_with_library_upgrade_disabled_uses_the_organize_path() {
        let client = Arc::new(MockClient::new());
        *client.search_results.lock().unwrap() = vec![SearchResult {
            username: "peer".into(),
            speed: 500,
            slots: 1,
            files: vec![make_file(
                r"Test Artist\Test Album\01 - track.flac",
                900,
                10_000_000,
            )],
        }];
        // Real bytes so the organizer has something to move.
        *client.write_files.lock().unwrap() = true;

        let mut config = make_test_config();
        config.library_upgrade.enabled = false;
        config.storage.organize = true;
        let library = TempDir::new().unwrap();
        config.library.paths = vec![library.path().to_string_lossy().into_owned()];
        let db = Database::open_in_memory().unwrap();
        let staging = TempDir::new().unwrap();

        let result = process_album(
            client.as_ref() as &dyn crate::client::SoulseekClient,
            "Test Artist",
            Some("Test Album"),
            false,
            &config,
            &db,
            staging.path(),
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();

        assert_eq!(result, AlbumOutcome::Downloaded { track_count: 1 });
        assert!(
            library
                .path()
                .join("Test Artist")
                .join("Test Album")
                .join("01 - track.flac")
                .exists(),
            "with the upgrade flag off the organizer writes under library.paths[0]"
        );
    }
```

- [ ] **Step 2: Run it to verify it passes before the refactor**

```bash
cargo test --lib runner::tests::auto_mode_with_library_upgrade_disabled 2>&1 | tail -20
```

Expected: PASS. This is the behaviour the refactor must preserve, so it is green
on both sides.

- [ ] **Step 3: Add the enum and the shared tail**

Add above `process_album` in `src/runner.rs`:

```rust
/// Where a completed download is written, and what its write is allowed to do.
///
/// `Upgrade` is auto mode's replacement of an album that already exists but
/// fails the quality gate: it is gated by the caller on
/// `library_upgrade.enabled`, compares the download against the library's own
/// track count, and may delete lesser-quality files. `Place` is discover mode's
/// placement of a newly downloaded album beside the artist's existing albums:
/// it carries no completeness baseline and never deletes anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LibraryTarget<'a> {
    Upgrade { root: &'a Path, expected_tracks: usize },
    Place { root: &'a Path, artist_dir: &'a str },
}

/// Shared tail for both library writes: drop the staging copy, record the
/// album as processed, notify, and report the completed album.
async fn finish_library_write(
    config: &Config,
    db: &Database,
    album_staging: &Path,
    artist: &str,
    album: Option<&str>,
    track_count: usize,
) -> Result<AlbumOutcome> {
    if let Err(e) = std::fs::remove_dir_all(album_staging) {
        tracing::warn!("Failed to remove staging dir {album_staging:?}: {e}");
    }
    mark_album_processed_if_identifiable(db, artist, album, "success")?;
    if let Err(e) = notifier::notify_success(
        &config.notifications.urls,
        artist,
        album.unwrap_or("Unknown"),
        track_count,
    )
    .await
    {
        tracing::warn!(
            "{artist} - {}: notification failed: {e}",
            album.unwrap_or("(all)")
        );
    }
    tracing::info!(
        "Completed: {artist} - {} ({track_count} tracks)",
        album.unwrap_or("(all)")
    );
    Ok(AlbumOutcome::Downloaded { track_count })
}
```

- [ ] **Step 4: Change the signatures**

In `process_album`, replace the `target_library_path: Option<&Path>` parameter
with `target: Option<LibraryTarget<'_>>` and forward it in the
`process_album_internal` call. Update the doc comment above `process_album` to:

```rust
/// Process a single album: search → filter rank → download → organize → notify.
///
/// When a [`LibraryTarget`] is supplied, a completed download is written into
/// the library instead of the generic organize step and the album completes
/// early (the organize block below is bypassed): `Upgrade` copies back into an
/// album's existing library directory behind the completeness gate, and
/// `Place` writes a newly downloaded album beside the artist's existing albums.
```

Do the same in `process_album_internal`: replace its `target_library_path:
Option<&Path>` parameter with `target: Option<LibraryTarget<'_>>`.

- [ ] **Step 5: Rewrite the copy-back block**

Replace the whole block in `process_album_internal` that starts with

```rust
    // Library upgrade (auto mode only, when enabled)
    if config.library_upgrade.enabled {
        if let Some(target_path) = target_library_path {
```

and ends with the `Failed { reason: format!("library upgrade failed: {e}") }`
arm followed by its closing braces, with:

```rust
    // Library write: auto mode's gated upgrade, or discover mode's placement.
    match target {
        Some(LibraryTarget::Upgrade {
            root,
            expected_tracks,
        }) => {
            // Completeness gate: the library album's own track count is the
            // reference — NOT the best peer's folder size. Peers share
            // different editions (box sets, anniversary editions) whose folder
            // can contain far more files than the album being upgraded has
            // (e.g. a 121-file peer folder for a 19-track library album).
            if downloaded.len() < expected_tracks {
                // The serving peer delivered an incomplete album — record an
                // album-level failure for it (per-track outcomes were already
                // recorded) so it sinks instead of being re-picked next cycle.
                if config.search.peer_reputation {
                    if let Some(peer) = furthest_peer(&stats).map(str::to_string) {
                        record_album_failure(db, &peer);
                    }
                }
                tracing::warn!(
                    "{artist} - {}: download incomplete ({}/{} tracks), skipping library upgrade",
                    album.unwrap_or("?"),
                    downloaded.len(),
                    expected_tracks,
                );
                mark_album_processed_if_identifiable(db, artist, album, "failed")?;
                return Ok(AlbumOutcome::Failed {
                    reason: "incomplete download, library upgrade skipped".into(),
                });
            }
            match organizer::copy_to_library(
                &downloaded,
                root,
                &config.storage.organize_pattern,
                artist,
                album.unwrap_or("Unknown"),
            ) {
                Ok(dests) => {
                    if config.library_upgrade.delete_lesser_quality {
                        match organizer::delete_lesser_quality_files(
                            root,
                            artist,
                            album.unwrap_or("Unknown"),
                            &dests,
                        ) {
                            Ok(count) if count > 0 => {
                                tracing::info!(
                                    "{artist} - {}: deleted {count} lesser-quality file(s)",
                                    album.unwrap_or("?")
                                );
                            }
                            Ok(_) => {}
                            Err(e) => {
                                tracing::error!(
                                    "{artist} - {}: failed to delete lesser-quality files: {e}",
                                    album.unwrap_or("?")
                                );
                            }
                        }
                    }
                    let track_count = downloaded.len();
                    return finish_library_write(
                        config,
                        db,
                        &album_staging,
                        artist,
                        album,
                        track_count,
                    )
                    .await;
                }
                Err(e) => {
                    tracing::error!(
                        "{artist} - {}: library upgrade failed: {e}",
                        album.unwrap_or("?")
                    );
                    mark_album_processed_if_identifiable(db, artist, album, "failed")?;
                    return Ok(AlbumOutcome::Failed {
                        reason: format!("library upgrade failed: {e}"),
                    });
                }
            }
        }
        Some(LibraryTarget::Place { root, artist_dir }) => {
            // No completeness gate: a new album has no library track count to
            // compare against, and presence already treats any audio file under
            // the album folder as present. No quality deletion either — nothing
            // is being replaced. `place_into_library` uses the folder name that
            // already exists on disk verbatim, so the album lands inside it
            // instead of beside a rewritten copy of it, and it never replaces a
            // destination file that parses as audio.
            match organizer::place_into_library(
                &downloaded,
                root,
                &config.storage.organize_pattern,
                artist_dir,
                album.unwrap_or("Unknown"),
            ) {
                Ok(_) => {
                    let track_count = downloaded.len();
                    return finish_library_write(
                        config,
                        db,
                        &album_staging,
                        artist,
                        album,
                        track_count,
                    )
                    .await;
                }
                Err(e) => {
                    tracing::error!(
                        "{artist} - {}: library placement failed: {e}",
                        album.unwrap_or("?")
                    );
                    mark_album_processed_if_identifiable(db, artist, album, "failed")?;
                    return Ok(AlbumOutcome::Failed {
                        reason: format!("library placement failed: {e}"),
                    });
                }
            }
        }
        None => {}
    }
```

This deletes the runtime `Failed("library upgrade target set without a library
track count")` arm, because `Upgrade` carries `expected_tracks` and the invalid
state can no longer be built.

- [ ] **Step 6: Gate auto mode at its caller**

In `run_auto_mode`'s per-album closure, replace

```rust
                let library_track_count = *track_count;
```

with

```rust
                let library_track_count = *track_count;
                // Auto mode's copy-back is the upgrade path: it replaces an
                // album that exists but fails the quality gate, so it is gated
                // on `library_upgrade.enabled` and carries the library's own
                // track count as the completeness reference. With the flag off
                // there is no target and the generic organize path runs.
                //
                // `then_some` rather than `then(|| ...)`: the construction has
                // no side effects and `clippy -D warnings` rejects the lazy
                // closure as `unnecessary_lazy_evaluations`.
                let target = config
                    .library_upgrade
                    .enabled
                    .then_some(LibraryTarget::Upgrade {
                        root: library_path.as_path(),
                        expected_tracks: library_track_count,
                    });
```

and replace the last two arguments of that `process_album` call
(`Some(library_track_count), Some(library_path),`) with:

```rust
                    Some(library_track_count),
                    target,
```

- [ ] **Step 7: Update the remaining target call site**

In the library-upgrade runner test whose `process_album` call passes
`Some(target.path()), // target_library_path` (the completeness test around
`src/runner.rs:4530`), replace the last two arguments with:

```rust
            Some(2), // library_track_count: the library album has 2 tracks
            Some(LibraryTarget::Upgrade {
                root: target.path(),
                expected_tracks: 2,
            }),
```

Every other call site passes `None` for both of those parameters and compiles
unchanged.

- [ ] **Step 8: Run the full test suite**

```bash
cargo test 2>&1 | tail -30
```

Expected: PASS, including
`auto_mode_with_library_upgrade_disabled_uses_the_organize_path`, the
library-upgrade completeness tests, the peer-reputation tests, and the discover
tests. Also confirm the removed guard is gone: `grep -n "target set without a
library track count" src/runner.rs` returns nothing.

- [ ] **Step 9: Commit**

```bash
git add src/runner.rs
git commit -m "refactor: split library upgrade from library placement behind LibraryTarget"
```

---

## Task 6: Place discover downloads in the artist's library folder

**Files:**

- Modify: `src/runner.rs:1454-1580` (`run_discover_mode_with_provider`),
  `src/runner.rs:3300-3330` (test fixtures), plus new tests next to the other
  discover tests

- [ ] **Step 1: Write the failing integration tests**

Add to `src/runner.rs`'s test module, after `discover_fixture`/`search_index`
and alongside the other `run_discover_mode_with_provider` tests:

```rust
    #[tokio::test]
    async fn discover_places_a_download_in_the_artist_library_folder() {
        let soulseek = MockClient::new();
        search_index(&soulseek, "Test Artist", &[("Test Artist Missing", "Missing")]);
        // Real bytes so the placement copy has content.
        *soulseek.write_files.lock().unwrap() = true;
        let (config, db, staging, library) = discover_fixture(&[("Test Artist", "Present")]);
        let provider =
            FakeDiscographyProvider::with_groups(vec![release_group("missing", "Missing", "1999")]);
        // Placement must not depend on either of these flags.
        assert!(!config.storage.organize);
        assert!(!config.library_upgrade.enabled);

        run_discover_mode_with_provider(
            &soulseek,
            &config,
            &db,
            None,
            false,
            staging.path(),
            &provider,
        )
        .await
        .unwrap();

        assert!(
            library
                .path()
                .join("Test Artist")
                .join("Missing")
                .join("01 - track.flac")
                .exists(),
            "the album must be placed beside the artist's existing albums"
        );
        assert!(
            !staging.path().join("Test Artist--Missing").exists(),
            "the staging album directory is removed after a successful placement"
        );
        assert_eq!(
            db.get_album_status("Test Artist", "Missing").unwrap().as_deref(),
            Some("success")
        );
    }

    #[tokio::test]
    async fn discover_places_into_a_nested_library_layout() {
        let soulseek = MockClient::new();
        search_index(&soulseek, "Test Artist", &[("Test Artist Missing", "Missing")]);
        *soulseek.write_files.lock().unwrap() = true;

        let (mut config, db, staging) = artist_only_fixture();
        let library = TempDir::new().unwrap();
        let existing = library
            .path()
            .join("Metal")
            .join("Test Artist")
            .join("Present");
        std::fs::create_dir_all(&existing).unwrap();
        std::fs::write(existing.join("01 - track.flac"), b"fake flac data").unwrap();
        config.library.paths = vec![library.path().to_string_lossy().into_owned()];
        let provider =
            FakeDiscographyProvider::with_groups(vec![release_group("missing", "Missing", "1999")]);

        run_discover_mode_with_provider(
            &soulseek,
            &config,
            &db,
            None,
            false,
            staging.path(),
            &provider,
        )
        .await
        .unwrap();

        assert!(
            library
                .path()
                .join("Metal")
                .join("Test Artist")
                .join("Missing")
                .join("01 - track.flac")
                .exists(),
            "the album lands inside the artist's existing genre folder"
        );
    }

    #[tokio::test]
    async fn discover_placement_failure_retains_staging_and_charges_the_budget() {
        let soulseek = MockClient::new();
        search_index(
            &soulseek,
            "Alpha Artist",
            &[("Alpha Artist Missing", "Missing")],
        );
        search_index(
            &soulseek,
            "Beta Artist",
            &[("Beta Artist Missing", "Missing")],
        );
        *soulseek.write_files.lock().unwrap() = true;
        let (mut config, db, staging, library) =
            discover_fixture(&[("Alpha Artist", "Present"), ("Beta Artist", "Present")]);
        // One attempt, so a charged failure must stop the run before Beta.
        config.discover.max_cycle_downloads = 1;
        // A file where the album directory must be created makes the placement
        // copy fail with "not a directory".
        std::fs::write(library.path().join("Alpha Artist").join("Missing"), b"blocked").unwrap();
        let provider =
            FakeDiscographyProvider::with_groups(vec![release_group("missing", "Missing", "1999")]);

        run_discover_mode_with_provider(
            &soulseek,
            &config,
            &db,
            None,
            false,
            staging.path(),
            &provider,
        )
        .await
        .unwrap();

        assert_eq!(
            db.get_album_status("Alpha Artist", "Missing").unwrap().as_deref(),
            Some("failed"),
            "a placement failure records the album as failed"
        );
        assert!(
            staging.path().join("Alpha Artist--Missing").exists(),
            "staging is retained when placement fails so nothing is lost"
        );
        assert_eq!(
            soulseek.search_queries.lock().unwrap().as_slice(),
            ["Alpha Artist Missing"],
            "the failed placement charges the budget and stops the run there"
        );
    }
```

Note on inherited coverage: `organizer`'s tests cover the mechanics that
placement reuses — staging preserved (`test_copy_to_library_preserves_staging`)
and multi-disc subdirectories
(`test_copy_to_library_preserves_multi_disc_subdirectories`,
`test_copy_to_library_preserves_embedded_marker_disc_subdirectories`). The
placement-specific rules are **not** inherited and need their own coverage:
placement uses a verbatim artist component and never replaces a destination file
that parses as audio, which is deliberately not the upgrade path's
never-downgrade guard, so add
`test_place_into_library_uses_the_artist_folder_name_verbatim`,
`test_place_into_library_never_replaces_a_readable_existing_file`,
`test_place_into_library_replaces_an_unreadable_existing_file`,
`test_place_into_library_does_not_cascade_placeholders_from_the_folder_name` and
`test_place_into_library_rejects_an_artist_value_that_is_not_one_component`. The
tag/folder spelling mismatch is covered at the unit level by
`destination_keeps_the_on_disk_artist_folder_spelling` in Task 4, because
`MockClient` writes untagged bytes.

- [ ] **Step 2: Run the new tests to verify they fail**

```bash
cargo test --lib runner::tests::discover_places 2>&1 | tail -30
cargo test --lib runner::tests::discover_placement_failure 2>&1 | tail -20
```

Expected: `discover_places_a_download_in_the_artist_library_folder` and
`discover_places_into_a_nested_library_layout` FAIL on the destination assertion
(files are still in `staging/Test Artist--Missing`), and
`discover_placement_failure_retains_staging_and_charges_the_budget` FAILS
because both artists get searched (no placement is attempted, so nothing fails
and nothing is charged).

- [ ] **Step 3: Place albums in the discover loop**

In `run_discover_mode_with_provider`, the loop currently binds a plain `String`.
Change the setup line above the loop:

```rust
    for artist in &selection.artists {
```

stays as it is, but every use of `artist` as a `&str` becomes `&artist.name`,
and the placement target is built once per artist. Immediately after the
cancellation check at the top of the loop body (the `if
cancel.load(Ordering::SeqCst) { break; }` block) add:

```rust
        // Where this artist's completed albums belong: the directory the
        // artist's existing albums were scanned from, with the folder name that
        // is actually on disk. Placement is unconditional in discover mode, so
        // it does not consult `storage.organize` or `library_upgrade.enabled`.
        let placement = LibraryTarget::Place {
            root: artist.library_root.as_path(),
            artist_dir: artist.artist_dir.as_str(),
        };
```

Then update the call and the counter writes in that loop body:

```rust
        counters.artists_examined += 1;
        match discover_artist_albums(provider, db, &artist.name, &config.discography).await {
```

```rust
                let missing = discover::missing_albums(&index, &artist.name, &albums);
```

```rust
                        counters
                            .budget_reached_at
                            .get_or_insert_with(|| artist.name.clone());
```

and inside the missing-album loop, replace the `process_album` call's last
argument (`None`, the `target_library_path`) with `Some(placement)`:

```rust
                    let result = process_album(
                        client,
                        &artist.name,
                        Some(&target.title),
                        ignore_processed,
                        config,
                        db,
                        staging_dir,
                        progress.as_ref(),
                        Some(&cancel),
                        None,
                        Some(placement),
                    )
                    .await;
```

The two `report.record(artist, ...)` calls in that loop become
`report.record(&artist.name, ...)`, and the `tracing::warn!("{artist}: ...")`
lines for stale caches, unresolved artists, and provider failures become `{}`
formatting with `&artist.name`:

```rust
                    tracing::warn!(
                        "{}: discography cache is {age_days} day(s) old and refresh failed ({refresh_error}); using stale cache",
                        artist.name
                    );
```

```rust
                    tracing::warn!(
                        "{}: artist could not be resolved on MusicBrainz: {reason}",
                        artist.name
                    );
```

```rust
                    tracing::warn!(
                        "{}: MusicBrainz unavailable ({reason}); artist skipped",
                        artist.name
                    );
```

`counters.unresolved.push(artist.name.clone());` and
`counters.provider_failed.push((artist.name.clone(), reason));` keep the same
shape.

- [ ] **Step 4: Run the new tests to verify they pass**

```bash
cargo test --lib runner::tests::discover_places 2>&1 | tail -20
cargo test --lib runner::tests::discover_placement_failure 2>&1 | tail -20
```

Expected: PASS.

- [ ] **Step 5: Run the discover, artist-only, and mode suites**

```bash
cargo test --lib runner::tests::discover 2>&1 | tail -30
cargo test --lib runner::tests::artist_only 2>&1 | tail -20
cargo test --test mode_resolution_test 2>&1 | tail -20
```

Expected: PASS. Artist-only manual and explicit manual runs still pass no
target, so they keep their current staging behaviour; `--artist` narrowing still
resolves to exactly one artist; the `--album`/`--batch-file` rejections are
untouched.

- [ ] **Step 6: Commit**

```bash
git add src/runner.rs
git commit -m "feat: place discover downloads in the artist's library folder"
```

---

## Task 7: Document placement

**Files:**

- Modify: `README.md` (the `storage` table around line 208, the "Discover mode"
  section around line 417, the FAQ around line 676)

- [ ] **Step 1: Update the `organize_pattern` row**

In the `### storage` table, replace the `organize_pattern` row with:

```markdown
| `organize_pattern` | Naming template for organised files. Placeholders: `%artist%`, `%album%`, `%track%`, `%title%`, `%ext%`, `%user%`. Must be relative and free of `..` components. Discover mode also uses it to shape placement even when `organize` is `false`. | `%artist%/%album%/%track% - %title%.%ext%` |
```

- [ ] **Step 2: Document placement in the Discover mode section**

At the end of the "### Discover mode" paragraphs (before "### Scheduled mode"),
append:

```markdown
Each completed album is placed in the artist's own library folder — the directory that artist's
existing albums were scanned from — using `storage.organize_pattern`, with `%artist%` expanded to the
artist folder name that is actually on disk and `%album%` to the MusicBrainz title. Placement is
unconditional in discover mode: it does not depend on `storage.organize` or on
`library_upgrade.enabled`, and `library_upgrade.delete_lesser_quality` never applies to it. Once the
copy succeeds the staging directory for that album is removed, so discover leaves nothing behind.
The scanner resolves the artist folder, album folder, and library location positionally and steps
over one dedicated disc folder (`CD 01`, `Disc 2`), so nested layouts such as
`<root>/Genre/Artist/Album` and albums whose discs sit in dedicated disc folders place correctly.
An album split across marker-shaped folders (`Gold (Disc 1)/`, `Gold (Disc 2)/` under one album
folder) reads as two albums and is documented as a limit rather than corrected. Only discover mode
places albums beside the artist's folder: an artist-only manual run (`--mode manual --artist "Name"`) does not, so with
`storage.organize: true` it organizes its downloads under `library.paths[0]` and with `organize` off they stay in
`staging_dir`.
```

- [ ] **Step 3: Add an FAQ entry**

After the "**Q: How do I fill in missing albums for one artist only?**" answer,
add:

```markdown
**Q: Where do discover downloads end up?**

In the artist's own library folder, beside the albums that artist already has. Discover derives the
destination from the same scan that produces its artist list, so a nested library such as
`Music/<user>/Albums/<genre>/<style>/<artist>/` keeps its layout instead of writing a second artist
tree under `library.paths[0]`. The staging copy is deleted once the album is placed. Placement never
replaces a file that already parses as audio, because the album folder may belong to a different
edition of the album — only a truncated leftover from an interrupted run is replaced. A placement
failure (a read-only or otherwise blocked destination) keeps the staging copy, records the album as
failed, and counts against `discover.max_cycle_downloads`. The album is retried when no audio file
reached the folder; once any file landed the album counts as present when its tags match the folder
the placement wrote to, so a mismatch between an embedded album tag and the MusicBrainz title can
still cause one more download. Note that a later auto run with `library_upgrade.enabled` removes
every leftover staging directory whose album is not recorded as successful, so a retained copy is a
short-lived safeguard rather than a permanent one.
```

- [ ] **Step 4: Lint the documentation**

```bash
markdownlint README.md 2>&1 | tail -20
```

Expected: no output (exit 0). If MD013 fires on a new line, wrap that line to 80
characters.

- [ ] **Step 5: Commit**

```bash
git add README.md
git commit -m "docs: describe discover-mode library placement"
```

---

## Task 8: Full verification

**Files:** none modified (verification only)

- [ ] **Step 1: Format check**

```bash
cargo fmt --check 2>&1 | tail -20
```

Expected: no output. If it lists files, run `cargo fmt` and re-run the check.

- [ ] **Step 2: Lint**

```bash
cargo clippy --all-targets -- -D warnings 2>&1 | tail -30
```

Expected: no warnings. Pay particular attention to `clippy::too_many_arguments`
on `process_album` (11 parameters, already `#[allow]`-annotated) and any
dead-code warning if a call-site update was missed.

- [ ] **Step 3: Full test suite, including the vendored crate**

```bash
cargo test 2>&1 | tail -40
```

Expected: PASS. The workspace includes `vendor/soulseek-rs-lib`, so its tests
run here too.

- [ ] **Step 4: Repository hooks**

```bash
pre-commit run --all-files 2>&1 | tail -20
```

Expected: all hooks pass (trailing whitespace, end-of-file, YAML, large files).

- [ ] **Step 5: Report**

Report to the chain: the exact commands run, their observed results, any test
that had to be changed and why (only
`test_find_albums_to_upgrade_preserves_nested_album_location`'s artist/album
expectations should have changed, as the intended correction), and the residual
risk that `cargo test` was the only integration evidence — no live Soulseek or
MusicBrainz run was performed.

---

## Self-Review

**Spec coverage:**

| Spec requirement | Task |
| --- | --- |
| Placement unconditional in discover mode | 6 (test asserts `organize`/`library_upgrade` off) |
| Destination = artist's own library folder | 4 (index destination), 6 (placement + nested test) |
| On-disk artist folder wins over tag spelling | 4 (`destination_keeps_the_on_disk_artist_folder_spelling`) |
| Majority destination with alphabetical tie-break | 4 (`destination_majority_wins_and_ties_break_alphabetically`) |
| Disc-folder peeling, shared with the peer parser | 1 (helper), 2 (peer parser), 3 (scanner) |
| `LibraryTarget` replaces the loose parameter | 5 |
| Gating moves to `run_auto_mode` | 5 (Steps 6–7 + characterisation test) |
| `SelectedArtist` carries the destination | 4 |
| Reuse `copy_into_library` behind `place_into_library`, no gate, no deletion | 5 (both arms), 6 (integration tests) |
| Album folder named from the MusicBrainz title | 6 (`Missing` from `release_group`) |
| Staging removed after a successful copy | 6 (first test), 5 (`finish_library_write`) |
| Placement failure = charged `Failed`, staging kept | 6 (third test) |
| Auto mode unchanged in both flag states | 5 (Steps 1–2, 8) |
| No new config keys, no schema change | `src/config.rs` gains the `organize_pattern` containment check (Task 8 Step 6) |
| README documentation | 7 |
| Deliberate limits (artist-only manual still stages) | 6 (Step 5 asserts unchanged artist-only tests), 7 (documented) |

**Placeholder scan:** no "TBD", "TODO", "handle edge cases", or "similar to Task
N" — every code step carries the code to write, every test step carries the
test, and every run step carries the command and expected result.

**Type consistency:** `LibraryTarget::{Upgrade { root, expected_tracks }, Place
{ root, artist_dir }}` is declared once in Task 5 and used with exactly those
field names in Task 5's arms and Task 6's loop. `SelectedArtist { name,
library_root, artist_dir }` is declared in Task 4 and read in Task 6 via
`&artist.name`, `artist.library_root.as_path()`, and
`artist.artist_dir.as_str()`. `discs::album_index(&[&str]) -> Option<usize>` is
declared in Task 1, consumed in Task 2 through `components[album_index -
1..album_index]` and in Task 3 through the `artist_dir`/`album` derivation.
`LibraryIndex::artist_destination(&str) -> Option<(&str, &str)>` from Task 4 is
the only destination accessor used by `select_artists`. `finish_library_write`
from Task 5 is called with the same six arguments in both arms.
