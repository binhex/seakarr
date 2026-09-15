# MusicBrainz Library Gap Filling Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use sub-agents (recommended)
> to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for
> tracking.

**Goal:** Add a `discover` mode that derives artists from the music library,
asks MusicBrainz which conceptual albums each artist is missing, and downloads
only those albums through the existing search and download pipeline, bounded by
a per-run download budget.

**Architecture:** All new pure logic lives in a new `src/discover.rs` module
(library index, presence decision, artist selection, download budget, notice
formatting). `src/runner.rs` gains `run_discover_mode`, which composes the
existing cache-aware `discover_artist_albums`, the new presence filter, and the
existing `process_album` pipeline. Artist-only manual mode gains the same
presence filter. Auto mode is not touched.

**Tech Stack:** Rust (edition 2021), `tokio`, `async-trait`, `serde` /
`serde_yaml`, `rusqlite` via `src/db.rs`, `lofty` via `src/scanner.rs`,
`tempfile` and the in-crate `MockClient` / `FakeDiscographyProvider` doubles for
tests.

**Spec:** `docs/agent/specs/2026-09-15-musicbrainz-library-gap-filling-design.md`

<!-- markdownlint-disable MD013 -->

---

## Scope Check

Single subsystem. The change adds one new module and one new mode, and touches
the mode resolver, the runner, configuration, the report enum, and the README.
It does not change search, filtering, downloading, organising, scheduling, the
database schema, or auto mode's upgrade pass.

No decomposition into separate plans is needed. The three sub-parts that could
look independent — the pure selection logic, the runner orchestration, and the
artist-only presence skip — share the same `LibraryIndex` type, and splitting
them would produce plans that cannot be implemented or tested alone.

## Deviations from the spec (decided while mapping the code)

The approved spec is not literally implementable in four places. Each is
resolved here, and each is a candidate for a spec amendment:

1. **Unknown `--artist` cannot be checked during mode resolution.** Spec
   validation item 4 says the check happens "during mode resolution", but
   `mode::resolve_execution_plan` (`src/mode.rs`) has no library index and must
   not scan the filesystem. The check therefore happens in
   `run_discover_mode`, immediately after the index is built and before any
   search or download. It still returns `SeakarrError::Config`.
2. **Blank `--artist` in discover mode is an explicit error.** The spec is
   silent. A blank selector is rejected with a discover-specific message rather
   than being treated as "no filter", so a typo or an unset shell variable can
   never silently widen a run to the whole library.
3. **`DiscoveryOutcome::LegacyFallback` gains a `kind`.** The spec's reporting
   contract needs "unresolved" and "provider failed" as separate notices, and
   the circuit breaker must count only provider failures. Today both cases
   arrive as one `LegacyFallback { reason }`, so the distinction has to be
   added to the type. Without it, three unresolved artists would abort a run.
4. **Provider construction failure is a hard error.** The spec does not say
   what happens when `MusicBrainzProvider::new()` fails under discover. Artist-
   only manual mode warns and falls back to the folder heuristic; discover has
   no legacy fallback by design, so this becomes a new `SeakarrError::MusicBrainz`
   variant returned to the CLI.

## Plan location

Saved to `docs/agent/plans/2026-09-15-musicbrainz-library-gap-filling.md`,
following `AGENTS.md` rule 7 (agent-generated plans live under `docs/agent/`)
rather than the skill's `docs/plans/` default. `docs/plans/` holds the
project's older hand-written plans and is not used by agent-generated work.

## Conventions used by this plan

- Every task ends with a commit.
- While iterating: `cargo test -p seakarr --lib <module::filter> -v`.
- Before committing: `cargo test --workspace`, `cargo fmt --all`, and
  `cargo clippy --workspace --all-targets -- -D warnings`.
- The workspace has two members (`.`, `vendor/soulseek-rs-lib`), so
  `cargo test --workspace` runs the vendored crate's regression tests too. A
  `-p seakarr` filter skips them.
- Test doubles you will reuse:
  - `src/client.rs`: `MockClient` with `search_results_by_query` (a
    `Mutex<HashMap<String, Vec<SearchResult>>>`) and `search_queries` (a
    `Mutex<Vec<String>>` recording every query issued).
  - `src/runner.rs` `mod tests`: `FakeDiscographyProvider`, `release_group`,
    `album_result`, `make_file`, `make_test_config`, `artist_only_fixture`.
  - `src/db.rs`: `Database::open_in_memory()`.
- Rust test filters accept only one pattern, so each verification step names
  one module path.

## File Structure

| File | Responsibility | Change |
| --- | --- | --- |
| `src/discover.rs` | Pure gap-filling logic: library index, presence decision, artist selection, download budget, notice formatting. No I/O except the scan it is handed. | Create |
| `src/lib.rs` | Module declarations for integration tests | Modify: declare `pub mod discover;` |
| `src/discography/mod.rs` | Discovery domain types and cache-aware orchestration | Modify: add `DiscoveryFailure`, carry it on `LegacyFallback`, classify in `stale_or_legacy`, update 4 test patterns |
| `src/report.rs` | Per-album outcome type and run summary | Modify: add `AlbumOutcome::NoCandidates`, render it in the existing `Failed` section |
| `src/runner.rs` | Mode orchestration | Modify: two `NoCandidates` construction sites, `run_discover_mode` + `run_discover_mode_with_provider`, presence filter in `run_artist_only_mode_with_provider`, extended test double, new tests |
| `src/error.rs` | Error taxonomy | Modify: add `MusicBrainz(String)` variant |
| `src/config.rs` | Configuration schema and validation | Modify: add `DiscoverConfig`, wire into `Config` and its `Default` |
| `src/mode.rs` | Mode and selector validation | Modify: `SearchMode::Discover`, `ExecutionPlan::Discover`, discover rules, updated conflict hint and invalid-mode message |
| `src/main.rs` | CLI surface and plan dispatch | Modify: help text, `Discover` dispatch arm |
| `tests/mode_resolution_test.rs` | Binary-level mode validation | Modify: discover acceptance and rejection cases |
| `tests/pipeline_test.rs` | Pipeline-level outcome assertions | Modify: one assertion that now expects `NoCandidates` |
| `README.md` | User documentation | Modify: mode list, CLI table, config section, FAQ entries, scheduling limitation |

Files that change together stay together: the index, presence decision,
selection, and budget all live in `src/discover.rs`, because they share the
`LibraryIndex` type and are only useful together.

---

### Task 1: Library index construction

**Files:**

- Create: `src/discover.rs`
- Modify: `src/lib.rs`
- Test: `src/discover.rs` (`mod tests`)

- [ ] **Step 1: Declare the module**

In `src/lib.rs`, add the declaration in alphabetical order, between `pub mod
discs;` and `pub mod download;`:

```rust
pub mod discover;
pub mod discs;
```

- [ ] **Step 2: Write the failing tests**

Create `src/discover.rs` with only the test module below. The file will not
compile yet, which is the expected RED for this task.

```rust
//! Library gap-filling logic for `discover` mode.
//!
//! Pure, synchronous selection logic: an index of what the library already
//! holds, the presence decision, the artist work list, and the per-run
//! download budget. The caller owns all I/O.

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn scanned(artist: &str, album: &str) -> ScannedAlbum {
        ScannedAlbum {
            path: PathBuf::from("/library"),
            artist: artist.to_string(),
            album: album.to_string(),
            track_count: 1,
            needs_upgrade: 0,
            min_bitrate: Some(900),
            max_bitrate: Some(900),
            formats: vec!["flac".to_string()],
        }
    }

    #[test]
    fn empty_library_yields_an_empty_index() {
        let index = build_index(&[]);
        assert_eq!(index.artist_keys().count(), 0);
    }

    #[test]
    fn keys_are_normalised_but_punctuation_stays_significant() {
        let index = build_index(&[
            scanned("  The   BEATLES ", "Abbey Road"),
            scanned("the beatles", "Abbey Road!"),
        ]);
        assert_eq!(index.artist_keys().collect::<Vec<_>>(), ["the beatles"]);
        assert!(index.contains_album("THE BEATLES", "abbey road"));
        assert!(index.contains_album("The Beatles", "Abbey Road!"));
        assert!(
            !index.contains_album("The Beatles", "Abbey-Road"),
            "punctuation must remain significant"
        );
    }

    #[test]
    fn albums_are_deduplicated_per_artist() {
        let index = build_index(&[
            scanned("Artist", "Album"),
            scanned("Artist", "Album"),
            scanned("Artist", "Other"),
        ]);
        let albums: Vec<&str> = index
            .albums_for("Artist")
            .expect("artist must be indexed")
            .collect();
        assert_eq!(albums, ["album", "other"]);
    }

    #[test]
    fn artist_name_prefers_the_spelling_covering_most_albums() {
        let index = build_index(&[
            scanned("Sigur Ros", "A"),
            scanned("Sigur Ros", "B"),
            scanned("sigur ros", "C"),
        ]);
        assert_eq!(index.artist_name("sigur ros"), Some("Sigur Ros"));
    }

    #[test]
    fn artist_name_breaks_ties_alphabetically() {
        let index = build_index(&[scanned("Zed", "A"), scanned("abc", "B")]);
        assert_eq!(index.artist_name("zed"), Some("Zed"));
        assert_eq!(index.artist_name("abc"), Some("abc"));
    }

    #[test]
    fn blank_artist_or_album_is_skipped() {
        let index = build_index(&[scanned("   ", "Album"), scanned("Artist", "  ")]);
        assert_eq!(index.artist_keys().count(), 0);
    }

    #[test]
    fn unknown_artist_or_album_is_absent() {
        let index = build_index(&[scanned("Artist", "Album")]);
        assert!(index.has_artist("Artist"));
        assert!(!index.has_artist("Other"));
        assert!(!index.contains_album("Artist", "Other"));
        assert!(!index.contains_album("Other", "Album"));
    }
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p seakarr --lib discover -v`

Expected: FAIL to compile with `cannot find function build_index in this scope`,
`cannot find struct ScannedAlbum`, and `no method named artist_keys`.

- [ ] **Step 4: Write the minimal implementation**

Add above `mod tests` in `src/discover.rs`:

```rust
use std::collections::{BTreeMap, BTreeSet};

use crate::discography::normalize_catalog_key;
use crate::scanner::ScannedAlbum;

/// One library artist: every spelling seen, and the normalised album titles.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct IndexedArtist {
    /// Original spelling to number of albums seen under it.
    spellings: BTreeMap<String, usize>,
    /// Normalised album titles.
    albums: BTreeSet<String>,
}

/// What the library already holds, keyed exactly like MusicBrainz catalog keys.
///
/// Built from one `scanner::scan_library` walk, so a nested layout such as
/// `<root>/Genre/Artist/Album` is indexed as correctly as `<root>/Artist/Album`:
/// the scanner already resolves the artist from tags with a folder fallback.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct LibraryIndex {
    artists: BTreeMap<String, IndexedArtist>,
}

impl LibraryIndex {
    /// Every artist key, in deterministic lexicographic order.
    pub fn artist_keys(&self) -> impl Iterator<Item = &str> {
        self.artists.keys().map(String::as_str)
    }

    /// True when any album is indexed for this artist.
    pub fn has_artist(&self, artist: &str) -> bool {
        self.artists.contains_key(&normalize_catalog_key(artist))
    }

    /// True when this artist/album pair is present in the library.
    pub fn contains_album(&self, artist: &str, album: &str) -> bool {
        self.artists
            .get(&normalize_catalog_key(artist))
            .is_some_and(|entry| entry.albums.contains(&normalize_catalog_key(album)))
    }

    /// Normalised album titles for one artist key, in order.
    pub fn albums_for(&self, artist: &str) -> Option<impl Iterator<Item = &str>> {
        self.artists
            .get(&normalize_catalog_key(artist))
            .map(|entry| entry.albums.iter().map(String::as_str))
    }

    /// The spelling to send to MusicBrainz: the one covering the most albums,
    /// with ties broken alphabetically so the query never depends on walk
    /// order.
    pub fn artist_name(&self, artist_key: &str) -> Option<&str> {
        let entry = self.artists.get(artist_key)?;
        entry
            .spellings
            .iter()
            .min_by_key(|(spelling, albums)| (std::cmp::Reverse(**albums), spelling.as_str()))
            .map(|(spelling, _)| spelling.as_str())
    }
}

/// Index scanned albums by normalised artist key and album title.
pub fn build_index(albums: &[ScannedAlbum]) -> LibraryIndex {
    let mut index = LibraryIndex::default();
    for album in albums {
        let artist_key = normalize_catalog_key(&album.artist);
        let album_key = normalize_catalog_key(&album.album);
        if artist_key.is_empty() || album_key.is_empty() {
            continue;
        }
        let entry = index.artists.entry(artist_key).or_default();
        *entry.spellings.entry(album.artist.clone()).or_insert(0) += 1;
        entry.albums.insert(album_key);
    }
    index
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p seakarr --lib discover -v`

Expected: PASS, 7 tests.

- [ ] **Step 6: Commit**

```bash
cargo fmt --all
git add src/discover.rs src/lib.rs
git commit -m "feat: index library artists and albums for gap filling"
```

---

### Task 2: Presence decision

**Files:**

- Modify: `src/discover.rs`
- Test: `src/discover.rs` (`mod tests`)

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `src/discover.rs`:

```rust
    use crate::discography::AlbumTarget;

    fn target(title: &str) -> AlbumTarget {
        AlbumTarget {
            release_group_id: format!("rg-{title}"),
            title: title.to_string(),
        }
    }

    #[test]
    fn missing_albums_keeps_only_what_the_library_lacks() {
        let index = build_index(&[scanned("Discovery", "Present")]);
        let targets = vec![target("Present"), target("Absent")];
        let missing = missing_albums(&index, "Discovery", &targets);
        assert_eq!(
            missing.iter().map(|t| t.title.as_str()).collect::<Vec<_>>(),
            ["Absent"]
        );
    }

    #[test]
    fn presence_ignores_case_and_whitespace_but_not_punctuation() {
        let index = build_index(&[scanned("Discovery", "  DISCOVERY  ")]);
        assert!(missing_albums(&index, "discovery", &[target("Discovery")]).is_empty());
        assert_eq!(
            missing_albums(&index, "Discovery", &[target("Discovery!")]).len(),
            1
        );
    }

    #[test]
    fn edition_qualifiers_do_not_satisfy_the_plain_album() {
        let index = build_index(&[
            scanned("Discovery", "Discovery (Deluxe Edition)"),
            scanned("Discovery", "Discovery [Remastered]"),
        ]);
        assert_eq!(
            missing_albums(&index, "Discovery", &[target("Discovery")]).len(),
            1,
            "strict matching: an edition is not the plain album"
        );
    }

    #[test]
    fn presence_is_scoped_to_the_artist() {
        let index = build_index(&[scanned("Other Artist", "Discovery")]);
        assert_eq!(
            missing_albums(&index, "Discovery", &[target("Discovery")]).len(),
            1
        );
    }

    #[test]
    fn missing_albums_preserves_input_order_and_an_empty_index_keeps_everything() {
        let index = build_index(&[]);
        let targets = vec![target("Third"), target("First"), target("Second")];
        let missing = missing_albums(&index, "Discovery", &targets);
        assert_eq!(
            missing.iter().map(|t| t.title.as_str()).collect::<Vec<_>>(),
            ["Third", "First", "Second"]
        );
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p seakarr --lib discover::tests::missing_albums -v`

Expected: FAIL to compile with `cannot find function missing_albums in this scope`.

- [ ] **Step 3: Write the minimal implementation**

Add after `build_index` in `src/discover.rs`:

```rust
/// Remove every target the library already holds, preserving input order.
///
/// Presence is a normalised title match inside the artist's own index entry: an
/// album counts as present when any audio file was scanned under that
/// artist/album directory. Quality and track count are deliberately ignored, so
/// a partially populated album counts as present.
pub fn missing_albums(
    index: &LibraryIndex,
    artist: &str,
    targets: &[AlbumTarget],
) -> Vec<AlbumTarget> {
    targets
        .iter()
        .filter(|target| !index.contains_album(artist, &target.title))
        .cloned()
        .collect()
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p seakarr --lib discover -v`

Expected: PASS, 12 tests.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
git add src/discover.rs
git commit -m "feat: decide album presence from the library index"
```

---

### Task 3: Artist selection

**Files:**

- Modify: `src/discover.rs`
- Test: `src/discover.rs` (`mod tests`)

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `src/discover.rs`:

```rust
    use crate::error::SeakarrError;

    fn library_fixture() -> LibraryIndex {
        build_index(&[
            scanned("Beta Band", "Album One"),
            scanned("Alpha Artist", "Album Two"),
            scanned("Live", "Album Three"),
        ])
    }

    #[test]
    fn artists_come_back_in_normalised_key_order() {
        let selection = select_artists(&library_fixture(), &[], None).unwrap();
        assert_eq!(selection.artists, ["Alpha Artist", "Beta Band", "Live"]);
        assert!(selection.excluded.is_empty());
    }

    #[test]
    fn exclusions_match_the_whole_key_not_a_substring() {
        let selection = select_artists(&library_fixture(), &["live".to_string()], None).unwrap();
        assert_eq!(selection.artists, ["Alpha Artist", "Beta Band"]);
        assert_eq!(selection.excluded, ["Live"]);
    }

    #[test]
    fn exclusion_matching_ignores_case_and_whitespace() {
        let selection =
            select_artists(&library_fixture(), &["  BETA   BAND ".to_string()], None).unwrap();
        assert_eq!(selection.artists, ["Alpha Artist", "Live"]);
        assert_eq!(selection.excluded, ["Beta Band"]);
    }

    #[test]
    fn a_filter_narrows_the_run_to_one_artist() {
        let selection = select_artists(&library_fixture(), &[], Some("beta band")).unwrap();
        assert_eq!(selection.artists, ["Beta Band"]);
    }

    #[test]
    fn an_unknown_filter_artist_is_a_configuration_error() {
        let error = select_artists(&library_fixture(), &[], Some("Nobody")).unwrap_err();
        assert!(
            matches!(error, SeakarrError::Config(_)),
            "expected a configuration error, got {error:?}"
        );
        assert!(error.to_string().contains("not found in the library"));
    }

    #[test]
    fn a_blank_filter_artist_is_a_configuration_error() {
        let error = select_artists(&library_fixture(), &[], Some("   ")).unwrap_err();
        assert!(error.to_string().contains("not found in the library"));
    }

    #[test]
    fn an_exclusion_that_matches_nothing_excludes_nothing() {
        let selection =
            select_artists(&library_fixture(), &["Various Artists".to_string()], None).unwrap();
        assert_eq!(selection.artists.len(), 3);
        assert!(selection.excluded.is_empty());
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p seakarr --lib discover::tests::artists -v`

Expected: FAIL to compile with `cannot find function select_artists in this scope`.

- [ ] **Step 3: Write the minimal implementation**

Add after `missing_albums` in `src/discover.rs`, and add the two imports at the
top of the file:

```rust
use crate::error::{Result, SeakarrError};
```

```rust
/// The artists selected for gap filling, plus the exclusions that were applied.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ArtistSelection {
    /// Library spellings to query, in normalised-key order.
    pub artists: Vec<String>,
    /// Library spellings skipped because an exclusion matched their key.
    pub excluded: Vec<String>,
}

/// Build the deterministic artist work list.
///
/// Exclusions are compared as whole normalised keys, never as substrings, so an
/// entry such as `live` cannot silently drop an unrelated artist whose name
/// happens to contain it. A filter may only narrow the library-derived list;
/// naming an artist the library does not hold is a configuration error rather
/// than a silent no-op.
pub fn select_artists(
    index: &LibraryIndex,
    excludes: &[String],
    filter: Option<&str>,
) -> Result<ArtistSelection> {
    let excluded_keys: BTreeSet<String> = excludes
        .iter()
        .map(|value| normalize_catalog_key(value))
        .filter(|key| !key.is_empty())
        .collect();

    let filter_key = filter.map(normalize_catalog_key).filter(|key| !key.is_empty());
    if let Some(name) = filter {
        if filter_key.is_none() || !index.has_artist(name) {
            return Err(SeakarrError::Config(format!(
                "artist {name:?} was not found in the library; discover mode can only narrow to artists already present"
            )));
        }
    }

    let mut selection = ArtistSelection::default();
    for key in index.artist_keys() {
        let Some(name) = index.artist_name(key) else {
            continue;
        };
        if excluded_keys.contains(key) {
            selection.excluded.push(name.to_string());
            continue;
        }
        if let Some(wanted) = filter_key.as_deref() {
            if key != wanted {
                continue;
            }
        }
        selection.artists.push(name.to_string());
    }
    Ok(selection)
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p seakarr --lib discover -v`

Expected: PASS, 19 tests.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
git add src/discover.rs
git commit -m "feat: select discover mode artists from the library"
```

---

### Task 4: Download budget

**Files:**

- Modify: `src/discover.rs`
- Test: `src/discover.rs` (`mod tests`)

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `src/discover.rs`:

```rust
    #[test]
    fn a_zero_limit_is_unlimited() {
        let mut budget = DownloadBudget::new(0);
        for _ in 0..100 {
            budget.charge();
        }
        assert!(budget.is_unlimited());
        assert!(!budget.exhausted());
        assert_eq!(budget.charged(), 100);
    }

    #[test]
    fn a_budget_exhausts_at_its_limit() {
        let mut budget = DownloadBudget::new(2);
        assert!(!budget.exhausted());
        budget.charge();
        assert!(!budget.exhausted());
        budget.charge();
        assert!(budget.exhausted());
    }

    #[test]
    fn charging_beyond_the_limit_saturates() {
        let mut budget = DownloadBudget::new(1);
        budget.charge();
        budget.charge();
        assert_eq!(budget.charged(), 2);
        assert!(budget.exhausted());
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p seakarr --lib discover::tests::a_budget -v`

Expected: FAIL to compile with `cannot find type DownloadBudget in this scope`.

- [ ] **Step 3: Write the minimal implementation**

Add after `ArtistSelection` in `src/discover.rs`:

```rust
/// Per-run download allowance. A limit of `0` means unlimited.
///
/// The budget is charged when a download attempt begins, never for an album
/// that was skipped as already present or that produced no admissible
/// candidate. Charging the latter would livelock the feature: failed albums do
/// not block retries, so the same cap-exhausting albums would consume every
/// run's allowance and no download would ever start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DownloadBudget {
    limit: u32,
    charged: u32,
}

impl DownloadBudget {
    /// Create a budget; `0` means unlimited.
    pub fn new(limit: u32) -> Self {
        Self { limit, charged: 0 }
    }

    /// True when the budget places no limit on this run.
    pub fn is_unlimited(&self) -> bool {
        self.limit == 0
    }

    /// Number of attempts charged so far.
    pub fn charged(&self) -> u32 {
        self.charged
    }

    /// True when no further download may start.
    pub fn exhausted(&self) -> bool {
        !self.is_unlimited() && self.charged >= self.limit
    }

    /// Record one download attempt.
    pub fn charge(&mut self) {
        self.charged = self.charged.saturating_add(1);
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p seakarr --lib discover -v`

Expected: PASS, 22 tests.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
git add src/discover.rs
git commit -m "feat: add a per-run discover download budget"
```

---

### Task 5: Classify discovery failures

Without this, an unresolved artist and an unreachable MusicBrainz look
identical, so the circuit breaker would abort a run after three artists that
simply have no MusicBrainz entry.

**Files:**

- Modify: `src/discography/mod.rs:440-470` (the `DiscoveryOutcome` enum)
- Modify: `src/discography/mod.rs:644-665` (`stale_or_legacy`)
- Modify: `src/discography/mod.rs:1320, 1446, 1615, 1772` (test patterns)
- Modify: `src/runner.rs:1181` (artist-only manual match)
- Test: `src/discography/mod.rs` (`mod tests`)

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `src/discography/mod.rs`. Both tests use the
`FakeProvider`, `Database::open_in_memory`, and `discover_artist_albums_at`
helpers that module already uses, so no new test double is needed:
`FakeProvider::artists(Vec::new())` resolves to no candidate (unresolved) and
`FakeProvider::failing("...")` fails the request (provider).

```rust
    #[tokio::test]
    async fn unresolved_artist_reports_the_unresolved_kind() {
        let db = Database::open_in_memory().unwrap();
        let provider = FakeProvider::artists(Vec::new());
        let outcome = discover_artist_albums_at(
            &provider,
            &db,
            "Unknown",
            &DiscographyConfig::default(),
            1_000,
        )
        .await;

        assert!(matches!(
            outcome,
            DiscoveryOutcome::LegacyFallback {
                kind: DiscoveryFailure::Unresolved,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn provider_error_reports_the_provider_kind() {
        let db = Database::open_in_memory().unwrap();
        let provider = FakeProvider::failing("connection reset");
        let outcome = discover_artist_albums_at(
            &provider,
            &db,
            "Artist",
            &DiscographyConfig::default(),
            1_000,
        )
        .await;

        assert!(matches!(
            outcome,
            DiscoveryOutcome::LegacyFallback {
                kind: DiscoveryFailure::Provider,
                ..
            }
        ));
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p seakarr --lib discography::tests::unresolved_artist_reports -v`

Expected: FAIL to compile with `cannot find type DiscoveryFailure in this scope`.

- [ ] **Step 3: Write the minimal implementation**

Add above `DiscoveryOutcome` in `src/discography/mod.rs`:

```rust
/// Why authoritative discovery could not supply albums.
///
/// The distinction matters to callers that must not treat an unresolvable
/// artist like an outage: `discover` aborts after consecutive
/// [`DiscoveryFailure::Provider`] failures but keeps going past any number of
/// [`DiscoveryFailure::Unresolved`] artists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscoveryFailure {
    /// The artist could not be resolved to exactly one MusicBrainz candidate.
    Unresolved,
    /// MusicBrainz could not be reached, or returned unusable data.
    Provider,
}

impl DiscoveryFailure {
    /// Only an artist-resolution failure is an artist problem; every other
    /// provider error (transport, HTTP status, decode, pagination) is an
    /// outage problem.
    fn from_error(error: &DiscographyError) -> Self {
        match error {
            DiscographyError::ArtistUnresolved(_) => Self::Unresolved,
            _ => Self::Provider,
        }
    }
}
```

Change the `LegacyFallback` variant to carry the kind:

```rust
    LegacyFallback {
        reason: String,
        kind: DiscoveryFailure,
    },
```

Change the no-cache branch of `stale_or_legacy`:

```rust
    let Some(cached) = cached else {
        return DiscoveryOutcome::LegacyFallback {
            kind: DiscoveryFailure::from_error(&refresh_error),
            reason: refresh_error.to_string(),
        };
    };
```

- [ ] **Step 4: Update the four existing patterns and the runner match**

In `src/discography/mod.rs`, the two tests that destructure `LegacyFallback`
with a `reason` guard, at lines 1320 and 1772, need the kind added. At 1320:

```rust
        assert!(matches!(
            outcome,
            DiscoveryOutcome::LegacyFallback { reason, kind }
                if kind == DiscoveryFailure::Unresolved
                    && reason.contains("artist could not be resolved safely")
        ));
```

At 1772, the same addition with the extra reason guard that test already has:

```rust
        assert!(matches!(
            outcome,
            DiscoveryOutcome::LegacyFallback { reason, kind }
                if kind == DiscoveryFailure::Unresolved
                    && reason.contains("artist could not be resolved safely")
                    && reason.contains("below 100")
        ));
```

In `src/runner.rs`, the artist-only manual match keeps ignoring the kind:

```rust
        DiscoveryOutcome::LegacyFallback { reason, .. } => {
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p seakarr --lib discography -v`

Then: `cargo test -p seakarr --lib runner::tests::artist_only -v`

Expected: PASS. Any test that asserted the old single-field pattern was updated
in Step 4, not deleted.

- [ ] **Step 6: Commit**

```bash
cargo fmt --all
git add src/discography/mod.rs src/runner.rs
git commit -m "feat: distinguish unresolved artists from provider failures"
```

---

### Task 6: Add `AlbumOutcome::NoCandidates`

**Files:**

- Modify: `src/report.rs:1-60`
- Modify: `src/runner.rs:447, 483` (construction sites)
- Modify: `src/runner.rs:1594, 2905, 2964, 3199` (test assertions)
- Modify: `tests/pipeline_test.rs:186`
- Test: `src/report.rs` (`mod tests`)

- [ ] **Step 1: Write the failing test**

Add to the `tests` module in `src/report.rs`:

```rust
    #[test]
    fn no_candidates_renders_in_the_failed_section() {
        let mut report = RunReport::new();
        report.record(
            "Artist",
            "Album",
            AlbumOutcome::NoCandidates {
                reason: "no results found".into(),
            },
        );
        assert_eq!(report.failed_count(), 1);
        assert_eq!(
            report.summary_lines(),
            vec![
                "=== Run summary ===".to_string(),
                "Failed (1):".to_string(),
                "  Artist — Album (no results found)".to_string(),
            ]
        );
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p seakarr --lib report::tests::no_candidates -v`

Expected: FAIL to compile with `no variant or associated item named NoCandidates`.

- [ ] **Step 3: Write the minimal implementation**

In `src/report.rs`, extend the enum:

```rust
/// Outcome of processing a single album.
#[derive(Debug, Clone, PartialEq)]
pub enum AlbumOutcome {
    Downloaded { track_count: usize },
    Skipped,
    Failed { reason: String },
    /// Search produced nothing, or nothing that survived filtering, so no
    /// download was attempted.
    ///
    /// Rendered in the summary exactly like [`AlbumOutcome::Failed`]. It exists
    /// as a distinct variant so `discover` can tell an album that never reached
    /// the download stage from one that did, and decline to charge its download
    /// budget for work that never happened.
    NoCandidates { reason: String },
}
```

Extend the `record` match arm:

```rust
            AlbumOutcome::Failed { reason } | AlbumOutcome::NoCandidates { reason } => {
                self.failed.push((artist.to_string(), album.to_string(), reason));
            }
```

- [ ] **Step 4: Move the two runner construction sites**

In `src/runner.rs`, at the "no results found" site (line 447) and the "no
results passed filters" site (line 483), the returned variant changes. The
reasons and all surrounding logic stay identical:

```rust
            return Ok(AlbumOutcome::NoCandidates {
                reason: "no results found".into(),
            });
```

```rust
        return Ok(AlbumOutcome::NoCandidates {
            reason: "no results passed filters".into(),
        });
```

Leave every other `AlbumOutcome::Failed` construction untouched: those paths
reached the download stage and must still consume the budget.

- [ ] **Step 5: Update the four runner test assertions and the pipeline test**

In `src/runner.rs` at lines 1594, 2905, and 3199, and in
`tests/pipeline_test.rs` at line 186, the pattern becomes:

```rust
            AlbumOutcome::NoCandidates { reason } => assert_eq!(reason, "no results found"),
```

In `src/runner.rs` at the line-2964 site, the pattern becomes:

```rust
                    AlbumOutcome::NoCandidates { reason } => {
                        assert!(reason.contains("no results passed filters"), "got: {reason}");
                        true
                    }
```

Keep the surrounding assertions and their comments; only the matched variant
and the reason text they check change.

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test -p seakarr --lib report -v`

Then: `cargo test --workspace`

Expected: PASS. If a test still matches `AlbumOutcome::Failed` for one of the
two moved reasons, it will fail with an unmatched-outcome panic naming the
reason; update that pattern the same way.

- [ ] **Step 7: Commit**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
git add src/report.rs src/runner.rs tests/pipeline_test.rs
git commit -m "refactor: distinguish no-candidate albums from failed attempts"
```

---

### Task 7: `DiscoverConfig`

**Files:**

- Modify: `src/config.rs` (after `DiscographyConfig`, its defaults, and the
  `Default for Config` body)
- Test: `src/config.rs` (`mod tests`)

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `src/config.rs`:

```rust
    #[test]
    fn discover_defaults_are_conservative() {
        let config = Config::default();
        assert_eq!(config.discover.max_cycle_downloads, 5);
        assert_eq!(
            config.discover.exclude_artists,
            vec![
                "Various Artists".to_string(),
                "VA".to_string(),
                "Unknown Artist".to_string()
            ]
        );
    }

    #[test]
    fn default_config_file_documents_the_discover_block() {
        let dir = TempDir::new().unwrap();
        let config = Config::load(dir.path()).unwrap();
        let yaml = fs::read_to_string(dir.path().join("seakarr.yml")).unwrap();
        assert!(yaml.contains("discover:"), "got:\n{yaml}");
        assert!(yaml.contains("max_cycle_downloads: 5"), "got:\n{yaml}");
        assert_eq!(config.discover.max_cycle_downloads, 5);
    }

    #[test]
    fn a_config_without_the_discover_block_still_loads() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("seakarr.yml"),
            "soulseek:\n  username: user\n  password: pass\n",
        )
        .unwrap();
        let config = Config::load(dir.path()).unwrap();
        assert_eq!(config.discover.max_cycle_downloads, 5);
        assert!(!config.discover.exclude_artists.is_empty());
    }

    #[test]
    fn discovered_exclusions_may_be_disabled_with_an_empty_list() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("seakarr.yml"),
            "discover:\n  exclude_artists: []\n",
        )
        .unwrap();
        let config = Config::load(dir.path()).unwrap();
        assert!(config.discover.exclude_artists.is_empty());
        assert_eq!(config.discover.max_cycle_downloads, 5);
    }
```

The existing module already imports `TempDir` and `fs`; reuse those imports
rather than adding new ones.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p seakarr --lib config::tests::discover -v`

Expected: FAIL to compile with `no field discover on type Config`.

- [ ] **Step 3: Write the minimal implementation**

Add after `default_discography_release_types` in `src/config.rs`:

```rust
/// Gap-filling (`discover` mode) configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoverConfig {
    /// Download attempts allowed per run. `0` means unlimited.
    #[serde(default = "default_discover_max_cycle_downloads")]
    pub max_cycle_downloads: u32,
    /// Artist keys skipped before any MusicBrainz lookup. Matched as whole
    /// normalised keys, never as substrings.
    #[serde(default = "default_discover_exclude_artists")]
    pub exclude_artists: Vec<String>,
}

fn default_discover_max_cycle_downloads() -> u32 {
    5
}

fn default_discover_exclude_artists() -> Vec<String> {
    vec![
        "Various Artists".to_string(),
        "VA".to_string(),
        "Unknown Artist".to_string(),
    ]
}

impl Default for DiscoverConfig {
    fn default() -> Self {
        Self {
            max_cycle_downloads: default_discover_max_cycle_downloads(),
            exclude_artists: default_discover_exclude_artists(),
        }
    }
}
```

Add the field to `Config`, directly after `discography`:

```rust
    pub discography: DiscographyConfig,
    pub discover: DiscoverConfig,
```

Add it to `impl Default for Config`, after the `discography:` entry:

```rust
            discover: DiscoverConfig::default(),
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p seakarr --lib config -v`

Expected: PASS, including the existing default-config and merge tests.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
git add src/config.rs
git commit -m "feat: add discover mode configuration"
```

---

### Task 8: `run_discover_mode`

**Files:**

- Modify: `src/error.rs`
- Modify: `src/runner.rs` (new functions, extended test double, new tests)
- Modify: `src/discover.rs` (notice formatting + `index_from_paths`)
- Test: `src/discover.rs`, `src/runner.rs`

- [ ] **Step 1: Add the error variant**

In `src/error.rs`, add to `SeakarrError` after `Scanner`:

```rust
    #[error("musicbrainz error: {0}")]
    MusicBrainz(String),
```

- [ ] **Step 2: Write the failing tests for the notice formatting**

Add to the `tests` module in `src/discover.rs`:

```rust
    #[test]
    fn notices_are_omitted_when_nothing_happened() {
        let counters = DiscoverCounters {
            artists_total: 3,
            artists_examined: 3,
            ..DiscoverCounters::default()
        };
        assert!(discover_notices(&counters).is_empty());
    }

    #[test]
    fn notices_summarise_every_aggregate_in_order() {
        let counters = DiscoverCounters {
            present: 12,
            excluded: 2,
            unresolved: vec!["Mystery Artist".to_string()],
            no_eligible_albums: 1,
            provider_failed: vec![("Offline Artist".to_string(), "connection reset".to_string())],
            budget_reached_at: Some("Beta Band".to_string()),
            budget_limit: 5,
            artists_total: 10,
            artists_examined: 4,
        };
        assert_eq!(
            discover_notices(&counters),
            vec![
                "discover: 12 album(s) already present; skipped".to_string(),
                "discover: excluded 2 artist(s) by discover.exclude_artists".to_string(),
                "discover: 1 artist(s) unresolved on MusicBrainz: Mystery Artist".to_string(),
                "discover: 1 artist(s) had no albums matching discography.allowed_types"
                    .to_string(),
                "discover: provider failed for Offline Artist (connection reset); artist skipped"
                    .to_string(),
                "discover: download budget of 5 reached at artist \"Beta Band\"; 4 of 10 artist(s) examined"
                    .to_string(),
            ]
        );
    }

    #[test]
    fn unresolved_names_are_capped_at_ten_with_a_remainder() {
        let counters = DiscoverCounters {
            unresolved: (1..=12).map(|n| format!("Artist {n}")).collect(),
            artists_total: 12,
            artists_examined: 12,
            ..DiscoverCounters::default()
        };
        let names = discover_notices(&counters)
            .into_iter()
            .find(|notice| notice.contains("unresolved on MusicBrainz"))
            .expect("an unresolved notice must be present");
        assert!(names.contains("Artist 1"), "got: {names}");
        assert!(names.contains("Artist 10"), "got: {names}");
        assert!(!names.contains("Artist 11"), "got: {names}");
        assert!(names.ends_with("and 2 more"), "got: {names}");
    }

    #[test]
    fn budget_notice_uses_the_configured_limit() {
        let counters = DiscoverCounters {
            budget_reached_at: Some("Artist".to_string()),
            budget_limit: 0,
            artists_total: 1,
            artists_examined: 1,
            ..DiscoverCounters::default()
        };
        assert!(
            discover_notices(&counters)
                .iter()
                .any(|notice| notice.contains("download budget of 0 reached")),
            "the notice must echo the configured limit verbatim"
        );
    }
```

- [ ] **Step 3: Implement the notice formatting and the path helper**

Add to `src/discover.rs` (above `mod tests`):

```rust
use crate::config::FilterConfig;

/// Counters accumulated across one discover run, used to build the summary.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DiscoverCounters {
    /// Albums skipped because the library already holds them.
    pub present: usize,
    /// Artists skipped by `discover.exclude_artists`.
    pub excluded: usize,
    /// Artists MusicBrainz could not resolve, in encounter order.
    pub unresolved: Vec<String>,
    /// Artists that resolved but had no eligible release groups.
    pub no_eligible_albums: usize,
    /// Artists whose provider request failed, with the reason.
    pub provider_failed: Vec<(String, String)>,
    /// The artist the run stopped at because the budget was spent.
    pub budget_reached_at: Option<String>,
    /// The configured budget limit, echoed in the notice.
    pub budget_limit: u32,
    /// Artists selected for this run.
    pub artists_total: usize,
    /// Artists actually examined before the run stopped.
    pub artists_examined: usize,
}

/// Names listed before an unresolved-artist notice is truncated.
const MAX_LISTED_UNRESOLVED: usize = 10;

/// Build the aggregate notices for a finished discover run, in spec order.
///
/// Pure: the caller records the returned strings on its `RunReport`.
pub fn discover_notices(counters: &DiscoverCounters) -> Vec<String> {
    let mut notices = Vec::new();
    if counters.present > 0 {
        notices.push(format!(
            "discover: {} album(s) already present; skipped",
            counters.present
        ));
    }
    if counters.excluded > 0 {
        notices.push(format!(
            "discover: excluded {} artist(s) by discover.exclude_artists",
            counters.excluded
        ));
    }
    if !counters.unresolved.is_empty() {
        let listed = counters
            .unresolved
            .iter()
            .take(MAX_LISTED_UNRESOLVED)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        let remainder = counters.unresolved.len().saturating_sub(MAX_LISTED_UNRESOLVED);
        let names = if remainder > 0 {
            format!("{listed} and {remainder} more")
        } else {
            listed
        };
        notices.push(format!(
            "discover: {} artist(s) unresolved on MusicBrainz: {names}",
            counters.unresolved.len()
        ));
    }
    if counters.no_eligible_albums > 0 {
        notices.push(format!(
            "discover: {} artist(s) had no albums matching discography.allowed_types",
            counters.no_eligible_albums
        ));
    }
    for (artist, reason) in &counters.provider_failed {
        notices.push(format!(
            "discover: provider failed for {artist} ({reason}); artist skipped"
        ));
    }
    if let Some(artist) = &counters.budget_reached_at {
        notices.push(format!(
            "discover: download budget of {} reached at artist \"{artist}\"; {} of {} artist(s) examined",
            counters.budget_limit, counters.artists_examined, counters.artists_total
        ));
    }
    notices
}

/// Build the presence index from configured library paths.
///
/// An empty `paths` list yields an empty index rather than an error, so callers
/// that only use the index to filter (artist-only manual mode) keep working
/// without a configured library.
pub fn index_from_paths(paths: &[String], filters: &FilterConfig) -> Result<LibraryIndex> {
    if paths.is_empty() {
        return Ok(LibraryIndex::default());
    }
    Ok(build_index(&crate::scanner::scan_library(paths, filters)?))
}
```

- [ ] **Step 4: Run the discover tests to verify they pass**

Run: `cargo test -p seakarr --lib discover -v`

Expected: PASS, 28 `discover::tests` (the filtered run reports more, because the filter also matches other modules' test names).

- [ ] **Step 5: Extend the runner test double**

In `src/runner.rs`, in the test module's `FakeDiscographyProvider`, add a
`candidate_names` field so an unresolvable artist can be simulated, and add the
constructor. Replace the struct and its impl block with:

```rust
    struct FakeDiscographyProvider {
        groups: Vec<ReleaseGroup>,
        failure: Option<String>,
        /// `None` echoes the requested artist back as the only candidate.
        candidate_names: Option<Vec<String>>,
    }

    impl FakeDiscographyProvider {
        fn with_groups(groups: Vec<ReleaseGroup>) -> Self {
            Self {
                groups,
                failure: None,
                candidate_names: None,
            }
        }

        fn failing(reason: &str) -> Self {
            Self {
                groups: Vec::new(),
                failure: Some(reason.to_string()),
                candidate_names: None,
            }
        }

        /// A provider whose artist search returns only non-matching names, so
        /// resolution fails as `DiscoveryFailure::Unresolved`.
        fn unresolvable() -> Self {
            Self {
                groups: Vec::new(),
                failure: None,
                candidate_names: Some(vec!["Somebody Else".to_string()]),
            }
        }
    }
```

In its `search_artists` implementation, replace the successful branch with:

```rust
            let names = self
                .candidate_names
                .clone()
                .unwrap_or_else(|| vec![artist.to_string()]);
            Ok(names
                .into_iter()
                .enumerate()
                .map(|(position, name)| ArtistCandidate {
                    id: format!("1111111{position}-1111-1111-1111-111111111111"),
                    name,
                    score: None,
                })
                .collect())
```

- [ ] **Step 6: Write the failing runner tests**

Add to the `mod tests` in `src/runner.rs`, after the existing artist-only
tests:

```rust
    /// A library fixture with `<root>/Artist/Album/track.flac` directories.
    fn library_with(albums: &[(&str, &str)]) -> TempDir {
        let library = TempDir::new().unwrap();
        for (artist, album) in albums {
            let dir = library.path().join(artist).join(album);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("01 - track.flac"), b"fake flac data").unwrap();
        }
        library
    }

    fn discover_fixture(albums: &[(&str, &str)]) -> (Config, Database, TempDir, TempDir) {
        let (mut config, db, staging) = artist_only_fixture();
        let library = library_with(albums);
        config.library.paths = vec![library.path().to_string_lossy().into_owned()];
        (config, db, staging, library)
    }

    fn search_index(soulseek: &MockClient, artist: &str, queries: &[(&str, &str)]) {
        let mut map = soulseek.search_results_by_query.lock().unwrap();
        for (query, album) in queries {
            map.insert(
                (*query).to_string(),
                vec![album_result(artist, album)],
            );
        }
    }

    #[tokio::test]
    async fn discover_downloads_only_albums_the_library_lacks() {
        let soulseek = MockClient::new();
        search_index(&soulseek, "Test Artist", &[("Test Artist Newer", "Newer")]);
        let provider = FakeDiscographyProvider::with_groups(vec![
            release_group("old", "Older", "1999"),
            release_group("new", "Newer", "2005"),
        ]);
        let (config, db, staging, _library) =
            discover_fixture(&[("Test Artist", "Older")]);

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
            soulseek.search_queries.lock().unwrap().as_slice(),
            ["Test Artist Newer"],
            "an album already in the library must never be searched"
        );
    }

    #[tokio::test]
    async fn discover_skips_an_album_with_no_candidates_without_charging_the_budget() {
        let soulseek = MockClient::new();
        search_index(&soulseek, "Test Artist", &[("Test Artist Second", "Second")]);
        let provider = FakeDiscographyProvider::with_groups(vec![
            release_group("first", "First", "1999"),
            release_group("second", "Second", "2005"),
        ]);
        let (mut config, db, staging, _library) = discover_fixture(&[("Test Artist", "Present")]);
        config.discover.max_cycle_downloads = 1;

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
            soulseek
                .search_queries
                .lock()
                .unwrap()
                .iter()
                .any(|query| query == "Test Artist Second"),
            "the empty first album must not consume the only budget slot; queries: {:?}",
            soulseek.search_queries.lock().unwrap()
        );
    }

    #[tokio::test]
    async fn discover_stops_at_the_budget_and_leaves_later_artists_unexamined() {
        let soulseek = MockClient::new();
        search_index(&soulseek, "Alpha Artist", &[("Alpha Artist Missing", "Missing")]);
        let (mut config, db, staging, _library) =
            discover_fixture(&[("Alpha Artist", "Present"), ("Beta Artist", "Present")]);
        config.discover.max_cycle_downloads = 1;
        let provider = FakeDiscographyProvider::with_groups(vec![release_group(
            "missing",
            "Missing",
            "1999",
        )]);

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
            soulseek.search_queries.lock().unwrap().as_slice(),
            ["Alpha Artist Missing"],
            "the budget must stop the run before the second artist"
        );
    }

    #[tokio::test]
    async fn discover_excludes_configured_artists_before_any_lookup() {
        let soulseek = MockClient::new();
        let (mut config, db, staging, _library) = discover_fixture(&[("Various Artists", "Hits")]);
        config.discover.exclude_artists = vec!["Various Artists".to_string()];
        let provider = FakeDiscographyProvider::with_groups(vec![release_group(
            "hits", "Hits", "1999",
        )]);

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

        assert!(soulseek.search_queries.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn discover_narrows_to_the_requested_artist() {
        let soulseek = MockClient::new();
        search_index(&soulseek, "Beta Artist", &[("Beta Artist Missing", "Missing")]);
        let (config, db, staging, _library) =
            discover_fixture(&[("Alpha Artist", "Present"), ("Beta Artist", "Present")]);
        let provider = FakeDiscographyProvider::with_groups(vec![release_group(
            "missing",
            "Missing",
            "1999",
        )]);

        run_discover_mode_with_provider(
            &soulseek,
            &config,
            &db,
            Some("Beta Artist"),
            false,
            staging.path(),
            &provider,
        )
        .await
        .unwrap();

        assert_eq!(
            soulseek.search_queries.lock().unwrap().as_slice(),
            ["Beta Artist Missing"]
        );
    }

    #[tokio::test]
    async fn discover_rejects_a_filter_artist_absent_from_the_library() {
        let soulseek = MockClient::new();
        let (config, db, staging, _library) = discover_fixture(&[("Alpha Artist", "Present")]);
        let provider = FakeDiscographyProvider::with_groups(vec![]);

        let error = run_discover_mode_with_provider(
            &soulseek,
            &config,
            &db,
            Some("Nobody"),
            false,
            staging.path(),
            &provider,
        )
        .await
        .unwrap_err();

        assert!(matches!(error, SeakarrError::Config(_)), "got {error:?}");
        assert!(error.to_string().contains("not found in the library"));
    }

    #[tokio::test]
    async fn discover_skips_an_unresolved_artist_without_searching() {
        let soulseek = MockClient::new();
        let (config, db, staging, _library) = discover_fixture(&[("Test Artist", "Present")]);
        let provider = FakeDiscographyProvider::unresolvable();

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
            soulseek.search_queries.lock().unwrap().is_empty(),
            "an unresolved artist must never trigger a broad artist search"
        );
    }

    #[tokio::test]
    async fn discover_aborts_after_three_consecutive_provider_failures() {
        let soulseek = MockClient::new();
        let (config, db, staging, _library) = discover_fixture(&[
            ("Alpha Artist", "Present"),
            ("Beta Artist", "Present"),
            ("Gamma Artist", "Present"),
        ]);
        let provider = FakeDiscographyProvider::failing("connection reset");

        let error = run_discover_mode_with_provider(
            &soulseek,
            &config,
            &db,
            None,
            false,
            staging.path(),
            &provider,
        )
        .await
        .unwrap_err();

        assert!(
            matches!(error, SeakarrError::MusicBrainz(_)),
            "expected a MusicBrainz error, got {error:?}"
        );
    }

    #[tokio::test]
    async fn discover_requires_an_enabled_discography() {
        let soulseek = MockClient::new();
        let (mut config, db, staging, _library) = discover_fixture(&[("Test Artist", "Present")]);
        config.discography.enabled = false;
        let provider = FakeDiscographyProvider::with_groups(vec![]);

        let error = run_discover_mode_with_provider(
            &soulseek,
            &config,
            &db,
            None,
            false,
            staging.path(),
            &provider,
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("discography.enabled"), "got {error}");
    }
```

- [ ] **Step 7: Run the tests to verify they fail**

Run: `cargo test -p seakarr --lib runner::tests::discover -v`

Expected: FAIL to compile with
`cannot find function run_discover_mode_with_provider in this scope`.

- [ ] **Step 8: Implement the discover runner**

Add to `src/runner.rs`, after `run_artist_only_mode` and before
`run_manual_mode`. Add `use crate::discover;` to the existing `use crate::{...}`
line so it reads:

```rust
use crate::{discover, download, filter, notifier, organizer, scanner, search};
```

```rust
/// Consecutive provider failures tolerated before a discover run aborts.
const DISCOVER_PROVIDER_FAILURE_LIMIT: u32 = 3;

/// Run discover mode: derive artists from the library, ask MusicBrainz what
/// each one is missing, and download only those albums.
///
/// Auto mode's upgrade pass is untouched: this mode never replaces an album it
/// considers present.
pub async fn run_discover_mode(
    client: &dyn SoulseekClient,
    config: &Config,
    db: &Database,
    artist_filter: Option<&str>,
    ignore_processed: bool,
) -> Result<()> {
    if !config.discography.enabled {
        return Err(SeakarrError::Config(
            "discover mode requires discography.enabled: true".into(),
        ));
    }
    if config.library.paths.is_empty() {
        return Err(SeakarrError::Config(
            "library.paths is empty — discover mode needs a library to derive artists from".into(),
        ));
    }
    let provider = MusicBrainzProvider::new().map_err(|error| {
        SeakarrError::MusicBrainz(format!("could not construct the provider: {error}"))
    })?;
    run_discover_mode_with_provider(
        client,
        config,
        db,
        artist_filter,
        ignore_processed,
        Path::new(&config.storage.staging_dir),
        &provider,
    )
    .await
}

/// Timeline-free core of discover mode, with the provider injected.
#[allow(clippy::too_many_arguments)]
async fn run_discover_mode_with_provider(
    client: &dyn SoulseekClient,
    config: &Config,
    db: &Database,
    artist_filter: Option<&str>,
    ignore_processed: bool,
    staging_dir: &Path,
    provider: &dyn DiscographyProvider,
) -> Result<()> {
    if !config.discography.enabled {
        return Err(SeakarrError::Config(
            "discover mode requires discography.enabled: true".into(),
        ));
    }
    if config.library.paths.is_empty() {
        return Err(SeakarrError::Config(
            "library.paths is empty — discover mode needs a library to derive artists from".into(),
        ));
    }

    let scanned = scanner::scan_library(&config.library.paths, &config.filters)?;
    let index = discover::build_index(&scanned);
    let selection =
        discover::select_artists(&index, &config.discover.exclude_artists, artist_filter)?;

    let mut counters = discover::DiscoverCounters {
        excluded: selection.excluded.len(),
        budget_limit: config.discover.max_cycle_downloads,
        artists_total: selection.artists.len(),
        ..discover::DiscoverCounters::default()
    };
    let mut budget = discover::DownloadBudget::new(config.discover.max_cycle_downloads);
    let mut report = RunReport::new();
    let mut consecutive_provider_failures: u32 = 0;

    let progress = if is_interactive() {
        Some(ProgressDisplay::new())
    } else {
        None
    };
    let cancel = Arc::new(AtomicBool::new(false));
    let _listener = spawn_cancel_listener(Arc::clone(&cancel));

    for artist in &selection.artists {
        if cancel.load(Ordering::SeqCst) || budget.exhausted() {
            // Only the first write names the artist where the budget ran out;
            // a later artist was never examined, so it must not rename it.
            counters.budget_reached_at.get_or_insert_with(|| artist.clone());
            break;
        }
        counters.artists_examined += 1;
        match discover_artist_albums(provider, db, artist, &config.discography).await {
            DiscoveryOutcome::Authoritative { albums, provenance } => {
                if let DiscoveryProvenance::StaleCache {
                    age_days,
                    refresh_error,
                } = &provenance
                {
                    tracing::warn!(
                        "{artist}: discography cache is {age_days} day(s) old and refresh failed ({refresh_error}); using stale cache"
                    );
                }
                consecutive_provider_failures = 0;
                let missing = discover::missing_albums(&index, artist, &albums);
                counters.present += albums.len() - missing.len();
                for target in missing {
                    if cancel.load(Ordering::SeqCst) || budget.exhausted() {
                        counters.budget_reached_at.get_or_insert_with(|| artist.clone());
                        break;
                    }
                    let result = process_album(
                        client,
                        artist,
                        Some(&target.title),
                        ignore_processed,
                        config,
                        db,
                        staging_dir,
                        progress.as_ref(),
                        Some(&cancel),
                        None,
                        None,
                    )
                    .await;
                    match result {
                        Ok(outcome) => {
                            if !matches!(outcome, AlbumOutcome::NoCandidates { .. }) {
                                budget.charge();
                            }
                            report.record(artist, &target.title, outcome);
                        }
                        Err(error) => {
                            // Matches auto mode: an environment error is
                            // recorded and the run continues to the next album.
                            tracing::error!(
                                "Album processing failed: {artist} — {}: {error}",
                                target.title
                            );
                            budget.charge();
                            report.record(
                                artist,
                                &target.title,
                                AlbumOutcome::Failed {
                                    reason: error.to_string(),
                                },
                            );
                        }
                    }
                    if budget.exhausted() {
                        counters.budget_reached_at.get_or_insert_with(|| artist.clone());
                        break;
                    }
                }
            }
            DiscoveryOutcome::AuthoritativeEmpty { provenance } => {
                if let DiscoveryProvenance::StaleCache {
                    age_days,
                    refresh_error,
                } = &provenance
                {
                    tracing::warn!(
                        "{artist}: discography cache is {age_days} day(s) old and refresh failed ({refresh_error}); no eligible authoritative albums"
                    );
                }
                consecutive_provider_failures = 0;
                counters.no_eligible_albums += 1;
            }
            DiscoveryOutcome::LegacyFallback { reason, kind } => match kind {
                DiscoveryFailure::Unresolved => {
                    // Not an outage: recording it as unresolved keeps the
                    // circuit breaker for provider failures only.
                    tracing::warn!("{artist}: artist could not be resolved on MusicBrainz: {reason}");
                    counters.unresolved.push(artist.clone());
                }
                DiscoveryFailure::Provider => {
                    consecutive_provider_failures += 1;
                    tracing::warn!("{artist}: MusicBrainz unavailable ({reason}); artist skipped");
                    counters.provider_failed.push((artist.clone(), reason));
                    if consecutive_provider_failures >= DISCOVER_PROVIDER_FAILURE_LIMIT {
                        for notice in discover::discover_notices(&counters) {
                            report.add_notice(notice);
                        }
                        report.print_summary();
                        _listener.abort();
                        return Err(SeakarrError::MusicBrainz(format!(
                            "{DISCOVER_PROVIDER_FAILURE_LIMIT} consecutive MusicBrainz failures; aborting discover run"
                        )));
                    }
                }
            },
        }
    }

    for notice in discover::discover_notices(&counters) {
        report.add_notice(notice);
    }
    if let Some(ref display) = progress {
        display.clear();
    }
    report.print_summary();
    _listener.abort();
    Ok(())
}
```

Add `use crate::discography::DiscoveryFailure;` to the `use
crate::discography::{...}` import list in `src/runner.rs`.

- [ ] **Step 9: Run the tests to verify they pass**

Run: `cargo test -p seakarr --lib runner::tests::discover -v`

Expected: PASS, 12 tests.

Then: `cargo test -p seakarr --lib discover -v` (28 `discover::tests`) and
`cargo test --workspace`.

- [ ] **Step 10: Commit**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
git add src/error.rs src/discover.rs src/runner.rs
git commit -m "feat: add discover mode gap-filling runner"
```

---

### Task 9: Mode wiring and CLI

**Files:**

- Modify: `src/mode.rs`
- Modify: `src/main.rs:64-70` (CLI help), `src/main.rs:426-445` (dispatch)
- Modify: `src/mode.rs` (`mod tests`)
- Modify: `tests/mode_resolution_test.rs`
- Test: `src/mode.rs` (`mod tests`)

- [ ] **Step 1: Write the failing mode tests**

Add to the `tests` module in `src/mode.rs`:

```rust
    #[test]
    fn discover_mode_resolves_without_a_selector() {
        let mut config = config_with_mode("discover");
        config.library.paths = vec!["/library".to_string()];
        let plan = resolve_execution_plan(&config, &cli(None, None, None, None)).unwrap();
        assert_eq!(plan, ExecutionPlan::Discover { artist: None });
        assert_eq!(plan.mode(), SearchMode::Discover);
    }

    #[test]
    fn discover_mode_accepts_an_optional_artist_filter() {
        let mut config = config_with_mode("discover");
        config.library.paths = vec!["/library".to_string()];
        let plan = resolve_execution_plan(&config, &cli(None, Some("Autechre"), None, None))
            .unwrap();
        assert_eq!(
            plan,
            ExecutionPlan::Discover {
                artist: Some("Autechre".into()),
            }
        );
    }

    #[test]
    fn discover_mode_rejects_an_album_selector() {
        let mut config = config_with_mode("discover");
        config.library.paths = vec!["/library".to_string()];
        assert_config_error(
            &config,
            &cli(None, None, Some("Album"), None),
            "--album is incompatible with discover mode",
        );
    }

    #[test]
    fn discover_mode_rejects_a_batch_file() {
        let mut config = config_with_mode("discover");
        config.library.paths = vec!["/library".to_string()];
        assert_config_error(
            &config,
            &cli(None, None, None, Some("wantlist.txt")),
            "--batch-file is incompatible with discover mode",
        );
    }

    #[test]
    fn discover_mode_rejects_a_blank_artist_selector() {
        let mut config = config_with_mode("discover");
        config.library.paths = vec!["/library".to_string()];
        assert_config_error(
            &config,
            &cli(None, Some("   "), None, None),
            "must not be blank in discover mode",
        );
    }

    #[test]
    fn discover_mode_requires_an_enabled_discography() {
        let mut config = config_with_mode("discover");
        config.library.paths = vec!["/library".to_string()];
        config.discography.enabled = false;
        assert_config_error(
            &config,
            &cli(None, None, None, None),
            "requires discography.enabled",
        );
    }

    #[test]
    fn discover_mode_requires_library_paths() {
        let config = config_with_mode("discover");
        assert_config_error(&config, &cli(None, None, None, None), "library.paths");
    }

    #[test]
    fn discover_mode_is_reachable_from_the_cli() {
        let mut config = config_with_mode("auto");
        config.library.paths = vec!["/library".to_string()];
        let plan = resolve_execution_plan(&config, &cli(Some("discover"), None, None, None))
            .unwrap();
        assert_eq!(plan, ExecutionPlan::Discover { artist: None });
    }
```

- [ ] **Step 2: Update the two invalid-mode assertions**

In the existing `unsupported_mode_is_rejected` and `empty_cli_mode_is_rejected`
tests, the expected substring changes to match the new mode list:

```rust
            "must be auto, manual, batch, or discover",
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p seakarr --lib mode::tests::discover -v`

Expected: FAIL to compile with `no variant or associated item named Discover`.

- [ ] **Step 4: Implement the mode changes**

In `src/mode.rs`, extend `SearchMode`:

```rust
pub enum SearchMode {
    Auto,
    Manual,
    Batch,
    Discover,
}
```

Extend `ExecutionPlan`:

```rust
pub enum ExecutionPlan {
    Auto,
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

Extend `ExecutionPlan::mode`:

```rust
            Self::Batch { .. } => SearchMode::Batch,
            Self::Discover { .. } => SearchMode::Discover,
```

Add `"discover"` to the mode parse and update the error text:

```rust
    let mode = match raw_mode.trim() {
        "auto" => SearchMode::Auto,
        "manual" => SearchMode::Manual,
        "batch" => SearchMode::Batch,
        "discover" => SearchMode::Discover,
        value => {
            return Err(SeakarrError::Config(format!(
                "invalid search mode '{value}' (must be auto, manual, batch, or discover)"
            )));
        }
    };
```

Update the auto-mode conflict hint so it names both valid targets:

```rust
                return Err(SeakarrError::Config(configured_mode_conflict(
                    config,
                    mode_from_cli,
                    "--artist/--album",
                    "use --mode manual or --mode discover",
                )));
```

Add the discover arm to the `match mode` block, after `SearchMode::Batch`. The
order of the checks matters: any `--album` is rejected before the blank-artist
check, so `--album ""` reports the incompatible-selector message rather than a
blank-artist one.

```rust
        SearchMode::Discover => {
            if !config.discography.enabled {
                return Err(SeakarrError::Config(
                    "discover mode requires discography.enabled: true".into(),
                ));
            }
            if config.library.paths.is_empty() {
                return Err(SeakarrError::Config(
                    "discover mode requires at least one library.paths entry".into(),
                ));
            }
            if cli.album.is_some() {
                return Err(SeakarrError::Config(
                    "--album is incompatible with discover mode; use --mode manual".into(),
                ));
            }
            if has_batch_cli_selector {
                return Err(SeakarrError::Config(
                    "--batch-file is incompatible with discover mode; use --mode batch".into(),
                ));
            }
            if cli
                .artist
                .as_deref()
                .is_some_and(|value| value.trim().is_empty())
            {
                return Err(SeakarrError::Config(
                    "--artist must not be blank in discover mode; omit it to process the whole library"
                        .into(),
                ));
            }
            Ok(ExecutionPlan::Discover {
                artist: cli_artist,
            })
        }
```

- [ ] **Step 5: Run the mode tests to verify they pass**

Run: `cargo test -p seakarr --lib mode -v`

Expected: PASS, all existing mode tests plus 8 new ones.

- [ ] **Step 6: Wire the CLI**

In `src/main.rs`, update the `--mode` help text:

```rust
    /// Override search mode (auto|manual|batch|discover)
    #[arg(long)]
    mode: Option<String>,
```

Add the dispatch arm after the `Batch` arm in `dispatch_execution_plan`:

```rust
        ExecutionPlan::Discover { artist } => {
            runner::run_discover_mode(client, config, db, artist.as_deref(), ignore_processed).await
        }
```

- [ ] **Step 7: Add the binary-level tests**

Add to `tests/mode_resolution_test.rs`, following the existing helper style in
that file (it writes a config file and runs the binary with `--test`):

```rust
#[test]
fn test_discover_mode_with_library_passes_validation() {
    let temp = TempDir::new().unwrap();
    let config_dir = temp.path().join("config");
    let log_dir = temp.path().join("logs");
    let library = temp.path().join("library");
    std::fs::create_dir_all(&library).unwrap();
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("seakarr.yml"),
        format!(
            "soulseek:\n  username: user\n  password: pass\nlibrary:\n  paths:\n    - {}\n",
            library.display()
        ),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_seakarr"))
        .args([
            "--config-path",
            config_dir.to_str().unwrap(),
            "--log-path",
            log_dir.to_str().unwrap(),
            "--mode",
            "discover",
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
        Some(0),
        "expected --test to accept a discover plan, got:\n{combined}"
    );
    assert!(
        combined.contains("Configuration is valid."),
        "got:\n{combined}"
    );
}

#[test]
fn test_discover_mode_rejects_album_selector() {
    let temp = TempDir::new().unwrap();
    let config_dir = temp.path().join("config");
    let log_dir = temp.path().join("logs");
    let library = temp.path().join("library");
    std::fs::create_dir_all(&library).unwrap();
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("seakarr.yml"),
        format!(
            "soulseek:\n  username: user\n  password: pass\nlibrary:\n  paths:\n    - {}\n",
            library.display()
        ),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_seakarr"))
        .args([
            "--config-path",
            config_dir.to_str().unwrap(),
            "--log-path",
            log_dir.to_str().unwrap(),
            "--mode",
            "discover",
            "--album",
            "Album",
            "--test",
        ])
        .output()
        .expect("failed to start seakarr");

    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_ne!(output.status.code(), Some(0), "got:\n{combined}");
    assert!(
        combined.contains("--album is incompatible with discover mode"),
        "got:\n{combined}"
    );
}
```

- [ ] **Step 8: Run the tests to verify they pass**

Run: `cargo test -p seakarr --test mode_resolution_test -v`

Then: `cargo test --workspace`

Expected: PASS.

- [ ] **Step 9: Commit**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
git add src/mode.rs src/main.rs tests/mode_resolution_test.rs
git commit -m "feat: wire discover mode into the CLI and scheduler"
```

---

### Task 10: Presence skip for artist-only manual runs

**Files:**

- Modify: `src/runner.rs` (`run_artist_only_mode_with_provider`)
- Test: `src/runner.rs` (`mod tests`)

- [ ] **Step 1: Write the failing tests**

Add to the `mod tests` in `src/runner.rs`:

```rust
    #[tokio::test]
    async fn artist_only_manual_skips_albums_already_in_the_library() {
        let soulseek = MockClient::new();
        search_index(&soulseek, "Test Artist", &[("Test Artist Missing", "Missing")]);
        let provider = FakeDiscographyProvider::with_groups(vec![
            release_group("present", "Present", "1999"),
            release_group("missing", "Missing", "2005"),
        ]);
        let (mut config, db, staging) = artist_only_fixture();
        let library = library_with(&[("Test Artist", "Present")]);
        config.library.paths = vec![library.path().to_string_lossy().into_owned()];

        let run = run_artist_only_mode_with_provider(
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

        assert_eq!(
            soulseek.search_queries.lock().unwrap().as_slice(),
            ["Test Artist Missing"],
            "an album present in the library must not be re-downloaded"
        );
        assert_eq!(run.outcomes.len(), 1);
        assert!(matches!(run.outcomes[0].1, AlbumOutcome::Downloaded { .. }));
        assert!(
            run.notice
                .as_deref()
                .is_some_and(|notice| notice.contains("1 album(s) already present")),
            "expected an aggregate notice, got {:?}",
            run.notice
        );
    }

    #[tokio::test]
    async fn artist_only_manual_with_everything_present_issues_no_search() {
        let soulseek = MockClient::new();
        let provider = FakeDiscographyProvider::with_groups(vec![release_group(
            "present", "Present", "1999",
        )]);
        let (mut config, db, staging) = artist_only_fixture();
        let library = library_with(&[("Test Artist", "Present")]);
        config.library.paths = vec![library.path().to_string_lossy().into_owned()];

        let run = run_artist_only_mode_with_provider(
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

        assert!(soulseek.search_queries.lock().unwrap().is_empty());
        assert!(run.outcomes.is_empty());
        assert!(
            run.notice
                .as_deref()
                .is_some_and(|notice| notice.contains("already present")),
            "expected an all-present notice, got {:?}",
            run.notice
        );
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p seakarr --lib runner::tests::artist_only_manual -v`

Expected: FAIL. Both tests fail on their assertions, because the library
presence is not consulted yet: the first searches two albums instead of one,
and the second issues one search instead of none.

- [ ] **Step 3: Implement the presence skip**

In `run_artist_only_mode_with_provider` in `src/runner.rs`, replace the
`DiscoveryOutcome::Authoritative` arm's work-list construction. The current arm
builds `let work = albums.into_iter().map(|album| (album.title, None)).collect();`.
Replace it up to (but not including) the existing `process_artist_album_work`
call with:

```rust
        DiscoveryOutcome::Authoritative { albums, provenance } => {
            if let DiscoveryProvenance::StaleCache {
                age_days,
                refresh_error,
            } = &provenance
            {
                tracing::warn!(
                    "{artist}: discography cache is {age_days} day(s) old and refresh failed ({refresh_error}); using stale cache"
                );
            }
            // A failing scan must not break artist-only manual mode, which
            // worked without a library before: warn and filter nothing.
            let index = match discover::index_from_paths(&config.library.paths, &config.filters) {
                Ok(index) => index,
                Err(error) => {
                    tracing::warn!(
                        "{artist}: library scan failed ({error}); skipping the already-present check"
                    );
                    discover::LibraryIndex::default()
                }
            };
            let missing = discover::missing_albums(&index, artist, &albums);
            let present = albums.len() - missing.len();
            if present > 0 && missing.is_empty() {
                return Ok(ArtistOnlyRun {
                    outcomes: Vec::new(),
                    notice: Some(format!(
                        "Artist-only run: all {present} eligible album(s) already present"
                    )),
                });
            }
            let notice = if present > 0 {
                Some(format!(
                    "Artist-only run: {present} album(s) already present; skipped"
                ))
            } else {
                None
            };
            let work = missing
                .into_iter()
                .map(|album| (album.title, None))
                .collect();
```

Then, after the existing `let outcomes = process_artist_album_work(...).await?;`
call, the arm's closing return becomes:

```rust
            Ok(ArtistOnlyRun { outcomes, notice })
        }
```

replacing the previous `Ok(ArtistOnlyRun { outcomes, notice: None })`. The
surrounding arms (`AuthoritativeEmpty`, `LegacyFallback`) and the
`process_artist_album_work` call itself are unchanged.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p seakarr --lib runner::tests::artist_only -v`

Then: `cargo test --workspace`

Expected: PASS. If an existing artist-only test asserts a search or outcome that
presence filtering now suppresses, that test has a library path configured and a
genuinely present album; report it rather than weakening the assertion, because
the new behaviour is the intended change.

- [ ] **Step 5: Add the regression test for a failing scan**

Add to the `mod tests` in `src/runner.rs`:

```rust
    #[tokio::test]
    async fn artist_only_manual_still_runs_when_the_library_path_is_missing() {
        let soulseek = MockClient::new();
        search_index(&soulseek, "Test Artist", &[("Test Artist Album", "Album")]);
        let provider =
            FakeDiscographyProvider::with_groups(vec![release_group("album", "Album", "1999")]);
        let (mut config, db, staging) = artist_only_fixture();
        config.library.paths = vec!["/definitely/not/here".to_string()];

        let run = run_artist_only_mode_with_provider(
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

        assert_eq!(
            soulseek.search_queries.lock().unwrap().as_slice(),
            ["Test Artist Album"],
            "an unusable library path must not stop artist-only manual mode"
        );
        assert!(matches!(run.outcomes[0].1, AlbumOutcome::Downloaded { .. }));
    }
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test -p seakarr --lib runner -v`

Expected: PASS.

- [ ] **Step 7: Commit**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
git add src/runner.rs
git commit -m "feat: skip albums already present in artist-only manual runs"
```

---

### Task 11: Documentation

**Files:**

- Modify: `README.md`

- [ ] **Step 1: Add the mode to the feature list**

In the `## Features` list, directly after the **Automatic mode** bullet, insert:

```markdown
- **Discover mode** — derives the artist list from your library (tags first,
  folder names as the fallback), asks MusicBrainz which conceptual albums each
  artist is missing, and downloads only those. Albums already present are never
  searched or re-downloaded, and each run stops after
  `discover.max_cycle_downloads` download attempts so a large library fills in
  over successive runs. Release types come from `discography.allowed_types`.
```

- [ ] **Step 2: Document the configuration block**

Add a `### discover` section immediately after the `### discography` section,
matching the surrounding table-free prose style:

```markdown
### `discover`

Controls `discover` mode, which fills gaps in your library for artists you
already have.

- `max_cycle_downloads` — download attempts allowed per run. `0` means
  unlimited. Default `5`. Albums skipped as already present, and albums whose
  search produced no admissible candidate, do not count against the budget, so
  a run always makes progress on the next attempt.
- `exclude_artists` — artist names skipped before any MusicBrainz lookup,
  matched as whole names ignoring case and spacing. Default
  `[Various Artists, VA, Unknown Artist]`, which prevents aggregator folders
  from expanding into hundreds of releases when `compilation` or `live_album` is
  enabled.

An album counts as present when a matching `artist/album` folder holds at least
one audio file. Matching is exact after case, spacing, and Unicode folding, so
punctuation is significant and an album you hold only as a deluxe or remastered
edition does not satisfy the plain album. Quality is not considered: replacing
lossy files remains `auto` mode's job.
```

- [ ] **Step 3: Document the CLI selector and the scheduling limitation**

Add these two entries to the FAQ section, in the existing question-and-answer
style:

```markdown
**Q: How do I fill in missing albums for one artist only?**

Run `--mode discover --artist "Artist Name"`. The name must already exist in
your library; discover only narrows the list it derives from your library, it
never adds an artist you do not have. To fetch a specific album instead, use
`--mode manual --artist "Artist Name" --album "Album Title"`.

**Q: Can I run auto mode and discover mode on a schedule at the same time?**

No. seakarr holds a single PID lock and a single Soulseek session, and a second
login with the same username displaces the running instance. A scheduled
instance performs one job: either upgrading what you have (`--mode auto`) or
filling gaps (`--mode discover`). Alternating between them across separate runs
is the supported approach.
```

- [ ] **Step 4: Document the artist-only behaviour change**

In the section describing artist-only manual runs, add one sentence to the
paragraph that describes authoritative discovery:

```markdown
Artist-only manual runs also skip every album that is already present in the
library, so `--artist X` fetches only what you are missing rather than
re-downloading albums you already own.
```

- [ ] **Step 5: Update the CLI table**

In the CLI options table, change the `--mode` row's description to list the
four modes:

```markdown
| `--mode <mode>` | Select `auto`, `manual`, `batch`, or `discover`. | *(from config)* |
```

- [ ] **Step 6: Lint and verify**

Run: `markdownlint README.md`

Expected: exit 0. The file already begins with
`<!-- markdownlint-disable MD013 -->`, so long table rows and code spans are
permitted.

- [ ] **Step 7: Commit**

```bash
git add README.md
git commit -m "docs: document discover mode and its configuration"
```

---

## Self-Review

**1. Spec coverage.** Every spec section maps to a task:

- Separate mode, auto untouched → Task 9 (wiring) plus Tasks 1-4, 8 (no writes
  to `run_auto_mode` anywhere in the plan).
- Tag-first artist source → Task 1 (index built from `scanner::scan_library`,
  which already prefers tags).
- Presence: folder with any audio file → Task 2.
- Strict normalised matching → Task 2 tests (case/whitespace equivalent,
  punctuation and edition qualifiers not).
- Presence index from one recursive walk, rejecting
  `get_library_track_filenames` → Task 1 plus `index_from_paths` in Task 8.
- `discography.allowed_types` as the single source of release types → no task
  introduces a new release-type key; Task 8 passes `&config.discography`
  straight through to `discover_artist_albums`.
- Budget, `0` unlimited, attempt-charged, `NoCandidates` exempt, stops
  examining further artists → Tasks 4, 6, 8.
- Unresolved artists skipped and reported, never broad-searched → Tasks 5, 8
  (test `discover_skips_an_unresolved_artist_without_searching`).
- Provider errors skip; three consecutive aborts → Tasks 5, 8.
- Deterministic ordering and spelling → Tasks 1 (spelling arbitration), 3
  (artist order), 8 (`discover_artist_albums` supplies album order).
- Exclusions exact, not substring → Task 3.
- `--artist` optional narrowing, unknown name errors → Tasks 3, 8, 9.
- Presence beats `--ignore-processed` → Task 8 passes `ignore_processed` to
  `process_album` only; the presence filter runs first and unconditionally.
- Reporting via existing notices, no per-album rows for present albums → Task 8
  (`discover_notices`, counters).
- Configuration keys and defaults → Task 7.
- Error handling list → Tasks 8, 9.
- Backward compatibility and the four deviations → documented above, plus
  Task 10 for the artist-only change.
- Acceptance criteria 1-8 → 1: Task 9; 2: Tasks 1, 3; 3: Tasks 2, 8, 10; 4:
  Tasks 7, 8; 5: Tasks 4, 6, 8; 6: Tasks 5, 8; 7: Task 8; 8: Task 11.

No spec requirement is left without a task.

**2. Placeholder scan.** No `TBD`, `TODO`, `FIXME`, "add error handling",
"write tests for the above", or "similar to Task N" appears. Every code step
carries the code, every test step carries the test, and every verification step
carries an exact command with expected output. The one step that asks the
implementer to extend an existing double (Task 5, Step 1) names the exact
constructors to add and the exact call path to wrap, because the existing
`FakeProvider`'s shape is unknown until the implementer opens that module.

**3. Type consistency.** Checked across tasks: `LibraryIndex` (Task 1) is used
by `missing_albums` (Task 2), `select_artists` (Task 3), `index_from_paths`
and `run_discover_mode_with_provider` (Task 8), and `run_artist_only_mode_with_provider`
(Task 10). `ArtistSelection` (Task 3) is consumed only in Task 8.
`DownloadBudget` (Task 4) is consumed only in Task 8. `DiscoverCounters` and
`discover_notices` (Task 8) agree on field names, including
`budget_reached_at` and `budget_limit`. `DiscoveryFailure` (Task 5) is
destructured in Task 8 as `{ reason, kind }`. `AlbumOutcome::NoCandidates`
(Task 6) is matched in Task 8 with the same field name (`reason`).
`ExecutionPlan::Discover { artist }` (Task 9) is destructured in the
`main.rs` dispatch arm added in the same task. `run_discover_mode_with_provider`
takes `staging_dir: &Path` in both its definition (Task 8) and its tests.

## Out of scope for this plan

- Amending the spec for the four deviations listed at the top. Recommended as a
  follow-up commit, not part of implementation.
- Any change to `run_auto_mode`, the upgrade pipeline, notifications, the
  database schema, or the vendored Soulseek client.
- Edition-tolerant matching, completeness thresholds, per-artist caps, and
  MBID auto-discovery, all recorded as deliberate limits in the spec.
