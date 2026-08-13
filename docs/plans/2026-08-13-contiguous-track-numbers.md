# Contiguous Track-Number Check Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use sub-agents (recommended) to implement this plan
> task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Reject search results whose downloadable tracks are not numbered contiguously, so seakarr
never downloads an incomplete album (duplicate track numbers permitted, missing track numbers not).

**Architecture:** A new pure module `src/tracks.rs` parses track numbers from filenames and checks
contiguity; `filter::filter_results` consults it (when `filters.contiguous_tracks` is enabled,
default true) over each result's quality-filter-passing files. Everything downstream (rank/download)
is unchanged.

**Tech Stack:** Rust, tokio, serde/serde_yaml. No new dependencies.

---

## Spec

Design spec (source of truth): `docs/specs/2026-08-13-contiguous-track-numbers-design.md`

Decisions locked in the spec:

- Track numbers are **1–3 digit all-numeric tokens**, leading (`04_Cure for Me.flac`) or anywhere
  in the filename (`… - 11 - Cure for the Itch.flac`). 4-digit tokens are ignored (years).
- The **first** such token wins (multi-number names like `1-01` yield 1).
- The check runs over **quality-filter-passing files only** (the set `download_album` downloads).
- A result with **no** parseable numbers is **rejected**.
- Contiguity = sorted unique numbers have **no gaps**; duplicates are permitted; starting number need not be 1.
- Applies to **both primary and fallback** results (both flow through `filter_results`).
- Config toggle `filters.contiguous_tracks` default `true`.
- Multi-disc numbering is out of scope (such collections set the toggle to false).

## File Map

| File | Action | Responsibility |
| ---- | ------ | -------------- |
| `src/tracks.rs` | **Create** | Pure parsing + contiguity helpers |
| `src/lib.rs` | Modify | Register `pub mod tracks;` |
| `src/filter.rs` | Modify | `filter_results` gains the contiguity predicate |
| `src/config.rs` | Modify | `FilterConfig.contiguous_tracks` (serde default true), Default impl, sample YAML |
| `src/runner.rs` | Modify | Update the "0 passed filters" log wording; add integration test |
| `README.md` | Modify | Document the new `filters:` key |

No other files change. `download.rs` and `search.rs` are untouched.

---

### Task 1: Config — `filters.contiguous_tracks` toggle

**Files:**
- Modify: `src/config.rs` (`FilterConfig` struct, `Default` impl at ~line 494, `sample_yaml()` filters section, tests module)
- Test: `src/config.rs`

- [ ] **Step 1: Write the failing tests**

Add inside the `#[cfg(test)] mod tests` block of `src/config.rs`, next to the existing `test_fallback_search_defaults_true`:

```rust
    #[test]
    fn test_contiguous_tracks_defaults_true() {
        let config = Config::default();
        assert!(config.filters.contiguous_tracks);
    }

    #[test]
    fn test_contiguous_tracks_from_yaml() {
        let dir = TempDir::new().unwrap();
        let yaml_path = dir.path().join("seakarr.yml");
        fs::write(&yaml_path, sample_yaml()).unwrap();

        let config = Config::load(dir.path()).unwrap();

        assert!(config.filters.contiguous_tracks);
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p seakarr test_contiguous_tracks`
Expected: FAIL — compile error `no field 'contiguous_tracks' on type 'FilterConfig'`.

- [ ] **Step 3: Add the field to `FilterConfig`**

In `src/config.rs`, inside `pub struct FilterConfig`, after the `include_locked` field add:

```rust
    #[serde(default = "default_true")]
    pub contiguous_tracks: bool,
```

(`default_true()` already exists in this file — used by `fallback_search` and `scan_on_startup`.)

- [ ] **Step 4: Add the field to the manual `Default` impl**

In `impl Default for Config`, inside the `filters: FilterConfig { ... }` block, after `include_locked: false,` add:

```rust
                contiguous_tracks: default_true(),
```

- [ ] **Step 5: Add the key to `sample_yaml()`**

In the `filters:` section of the test helper `sample_yaml()`, after `include_locked: false` add:

```yaml
  contiguous_tracks: true
```

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test -p seakarr test_contiguous_tracks`
Expected: PASS (2 tests).

- [ ] **Step 7: Commit**

```bash
cargo fmt
git add src/config.rs
git commit -m "feat: add filters.contiguous_tracks config toggle (default on)"
```

---

### Task 2: `tracks.rs` — `track_number_from_filename`

**Files:**
- Create: `src/tracks.rs`
- Modify: `src/lib.rs` (register the module)
- Test: `src/tracks.rs` (tests module)

- [ ] **Step 1: Write the failing tests**

Create `src/tracks.rs` with ONLY the tests (the functions do not exist yet):

```rust
//! Track-number parsing and contiguity checks for search results.

use crate::client::FileInfo;

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn make_file(name: &str) -> FileInfo {
        FileInfo {
            name: name.into(),
            size: 0,
            attribs: HashMap::new(),
        }
    }

    #[test]
    fn test_leading_token() {
        assert_eq!(
            track_number_from_filename("04_Cure for Me.flac"),
            Some(4)
        );
    }

    #[test]
    fn test_leading_token_dash_separated() {
        assert_eq!(
            track_number_from_filename("08 - the cure.flac"),
            Some(8)
        );
    }

    #[test]
    fn test_mid_filename_token() {
        assert_eq!(
            track_number_from_filename(
                "Linkin Park - Hybrid Theory - 11 - Cure for the Itch.flac"
            ),
            Some(11)
        );
    }

    #[test]
    fn test_four_digit_year_ignored() {
        assert_eq!(
            track_number_from_filename("Hybrid Theory (2000) - 01 - Papercut.flac"),
            Some(1)
        );
    }

    #[test]
    fn test_no_number_returns_none() {
        assert_eq!(
            track_number_from_filename("Cure for the Itch.flac"),
            None
        );
    }

    #[test]
    fn test_token_with_letters_returns_none() {
        assert_eq!(track_number_from_filename("Track 4a.flac"), None);
    }

    #[test]
    fn test_path_prefix_stripped() {
        assert_eq!(
            track_number_from_filename(r"shared\Linkin Park\Hybrid Theory\01 - Papercut.flac"),
            Some(1)
        );
    }

    #[test]
    fn test_first_numeric_token_wins() {
        // Multi-disc style names yield the first 1-3 digit token.
        assert_eq!(track_number_from_filename("1-01 - Title.flac"), Some(1));
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p seakarr track_number_from_filename`
Expected: FAIL — compile error `cannot find function 'track_number_from_filename'` (plus `unused import FileInfo`). Note: `make_file` in the tests module returns `FileInfo`, so the top-level `use crate::client::FileInfo;` must exist — that part compiles once added in Step 3.

- [ ] **Step 3: Write the implementation**

Create the production half of `src/tracks.rs` (above the tests module) and register the module:

`src/tracks.rs`:

```rust
//! Track-number parsing and contiguity checks for search results.

use crate::client::FileInfo;

/// Extract a track number from a filename (or share path).
///
/// Splits on non-alphanumeric boundaries and returns the first 1-3 digit
/// all-numeric token (zero-padded counts), ignoring 4+ digit tokens such as
/// years. Covers leading numbering ("04_Cure for Me.flac") and mid-filename
/// numbering ("Linkin Park - Hybrid Theory - 11 - Cure for the Itch.flac").
/// Returns `None` when no such token exists.
pub fn track_number_from_filename(name: &str) -> Option<u32> {
    let basename = name.rsplit(['\\', '/']).next().unwrap_or(name);
    basename
        .split(|c: char| !c.is_alphanumeric())
        .filter(|tok| !tok.is_empty())
        .find_map(|tok| {
            if tok.len() > 3 || !tok.chars().all(|c| c.is_ascii_digit()) {
                return None;
            }
            tok.parse::<u32>().ok()
        })
}
```

`src/lib.rs` — after `pub mod search;` add:

```rust
pub mod tracks;
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p seakarr track_number_from_filename`
Expected: PASS (8 tests).

- [ ] **Step 5: Commit**

```bash
cargo fmt
git add src/tracks.rs src/lib.rs
git commit -m "feat: add track-number parsing helper (tracks module)"
```

---

### Task 3: `tracks.rs` — `files_have_contiguous_tracks`

**Files:**
- Modify: `src/tracks.rs`
- Test: `src/tracks.rs`

- [ ] **Step 1: Write the failing tests**

Append to the tests module of `src/tracks.rs` (reusing `make_file` from Task 2):

```rust
    #[test]
    fn test_contiguous_passes() {
        let files = vec![
            make_file("01 - A.flac"),
            make_file("02 - B.flac"),
            make_file("03 - C.flac"),
        ];
        let refs: Vec<&FileInfo> = files.iter().collect();
        assert!(files_have_contiguous_tracks(&refs));
    }

    #[test]
    fn test_duplicates_pass() {
        let files = vec![
            make_file("01 - A.flac"),
            make_file("02 - B.flac"),
            make_file("02 - B (alt).flac"),
            make_file("03 - C.flac"),
        ];
        let refs: Vec<&FileInfo> = files.iter().collect();
        assert!(files_have_contiguous_tracks(&refs));
    }

    #[test]
    fn test_gap_fails() {
        let files = vec![
            make_file("01 - A.flac"),
            make_file("03 - C.flac"),
        ];
        let refs: Vec<&FileInfo> = files.iter().collect();
        assert!(!files_have_contiguous_tracks(&refs));
    }

    #[test]
    fn test_production_gap_fails() {
        // The reported incident: The Cure album with tracks 04, 08, 16.
        let files = vec![
            make_file("04_Cure for Me.flac"),
            make_file("08_the cure.flac"),
            make_file("16_Cure for Me (acoustic).flac"),
        ];
        let refs: Vec<&FileInfo> = files.iter().collect();
        assert!(!files_have_contiguous_tracks(&refs));
    }

    #[test]
    fn test_single_track_passes() {
        let files = vec![make_file("07 - A.flac")];
        let refs: Vec<&FileInfo> = files.iter().collect();
        assert!(files_have_contiguous_tracks(&refs));
    }

    #[test]
    fn test_empty_or_unnumbered_fails() {
        let empty: Vec<&FileInfo> = vec![];
        assert!(!files_have_contiguous_tracks(&empty));

        let files = vec![make_file("Title.flac"), make_file("Another.flac")];
        let refs: Vec<&FileInfo> = files.iter().collect();
        assert!(!files_have_contiguous_tracks(&refs));
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p seakarr files_have_contiguous_tracks`
Expected: FAIL — compile error `cannot find function 'files_have_contiguous_tracks'`.

- [ ] **Step 3: Write the implementation**

Add to `src/tracks.rs` (below `track_number_from_filename`):

```rust
/// Check that a set of files carries contiguous track numbers.
///
/// Collects the track number of every file, requires at least one, then
/// verifies the sorted unique numbers have no gaps. Duplicate track numbers
/// are permitted; missing numbers are not.
pub fn files_have_contiguous_tracks(files: &[&FileInfo]) -> bool {
    let mut numbers: Vec<u32> = files
        .iter()
        .filter_map(|f| track_number_from_filename(&f.name))
        .collect();
    if numbers.is_empty() {
        return false;
    }
    numbers.sort_unstable();
    numbers.dedup();
    numbers.windows(2).all(|w| w[1] == w[0] + 1)
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p seakarr files_have_contiguous_tracks`
Expected: PASS (6 tests).

- [ ] **Step 5: Commit**

```bash
cargo fmt
git add src/tracks.rs
git commit -m "feat: add contiguous-track check helper"
```

---

### Task 4: Wire the contiguity predicate into `filter_results`

**Files:**
- Modify: `src/filter.rs` (`filter_results`)
- Test: `src/filter.rs`

- [ ] **Step 1: Write the failing tests**

Append to the tests module of `src/filter.rs` (reusing the existing `make_file`, `make_result`, `default_filter_config` helpers):

```rust
    #[test]
    fn test_filter_rejects_gappy_tracks_when_toggle_on() {
        let cfg = FilterConfig {
            contiguous_tracks: true,
            ..default_filter_config()
        };
        let results = vec![make_result(
            "user1",
            500,
            1,
            vec![
                make_file("01 - A.flac", 900, 30_000_000),
                make_file("03 - C.flac", 900, 30_000_000),
            ],
        )];

        let filtered = filter_results(&results, &cfg);
        assert!(filtered.is_empty());
    }

    #[test]
    fn test_filter_accepts_gappy_tracks_when_toggle_off() {
        let cfg = FilterConfig {
            contiguous_tracks: false,
            ..default_filter_config()
        };
        let results = vec![make_result(
            "user1",
            500,
            1,
            vec![
                make_file("01 - A.flac", 900, 30_000_000),
                make_file("03 - C.flac", 900, 30_000_000),
            ],
        )];

        let filtered = filter_results(&results, &cfg);
        assert_eq!(filtered.len(), 1);
    }

    #[test]
    fn test_filter_rejects_unnumbered_result_when_toggle_on() {
        let cfg = FilterConfig {
            contiguous_tracks: true,
            ..default_filter_config()
        };
        let results = vec![make_result(
            "user1",
            500,
            1,
            vec![make_file("Title.flac", 900, 30_000_000)],
        )];

        let filtered = filter_results(&results, &cfg);
        assert!(filtered.is_empty());
    }

    #[test]
    fn test_filter_contiguity_runs_over_quality_passing_files_only() {
        // The full result looks contiguous (01, 02, 03), but track 02 is an
        // mp3 that fails the quality filters — the downloadable set is
        // 01, 03, which has a gap, so the result must be rejected.
        let cfg = FilterConfig {
            contiguous_tracks: true,
            ..default_filter_config()
        };
        let results = vec![make_result(
            "user1",
            500,
            1,
            vec![
                make_file("01 - A.flac", 900, 30_000_000),
                make_file("02 - B.mp3", 320, 10_000_000),
                make_file("03 - C.flac", 900, 30_000_000),
            ],
        )];

        let filtered = filter_results(&results, &cfg);
        assert!(filtered.is_empty());
    }
```

Note: `default_filter_config()` (used by these tests) must gain the new field — add `contiguous_tracks: true,` to it in the same step (it currently lists all `FilterConfig` fields explicitly):

```rust
    fn default_filter_config() -> FilterConfig {
        FilterConfig {
            allowed_extensions: vec!["flac".into()],
            min_bitrate: Some(320),
            min_bitdepth: None,
            exclude_words: vec![],
            include_locked: false,
            contiguous_tracks: true,
        }
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p seakarr test_filter_rejects_gappy_tracks_when_toggle_on`
Expected: FAIL — compile error `missing field 'contiguous_tracks' in initializer of 'FilterConfig'` (the `default_filter_config` helper does not yet initialise the new field; the production predicate is also missing, so the behaviour is absent either way). Both halves are fixed in Step 3.

- [ ] **Step 3: Write the implementation**

Add `contiguous_tracks: true,` to `default_filter_config()` in the tests module (exact block shown above), then replace the body of `filter_results` in `src/filter.rs` with:

```rust
pub fn filter_results(results: &[SearchResult], config: &FilterConfig) -> Vec<SearchResult> {
    results
        .iter()
        .filter(|r| {
            // Filter: must have free slots (if max_queue_length == 0)
            if r.slots == 0 {
                return false;
            }

            // The downloadable set: files passing extension + bitrate +
            // word filters. The contiguity check runs over THIS set — a
            // result whose quality-passing tracks have gaps would download
            // an incomplete album, so it is discounted here at the search
            // result stage.
            let passing: Vec<&FileInfo> = r
                .files
                .iter()
                .filter(|f| file_passes_filters(f, config))
                .collect();
            if passing.is_empty() {
                return false;
            }
            if config.contiguous_tracks
                && !crate::tracks::files_have_contiguous_tracks(&passing)
            {
                return false;
            }
            true
        })
        .cloned()
        .collect()
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p seakarr filter::tests`
Expected: PASS — the 4 new tests plus all pre-existing filter tests (9 total).

- [ ] **Step 5: Commit**

```bash
cargo fmt
git add src/filter.rs
git commit -m "feat: discount search results with non-contiguous track numbers"
```

---

### Task 5: Runner — log wording and integration test

**Files:**
- Modify: `src/runner.rs` (log wording, tests module)
- Test: `src/runner.rs`

- [ ] **Step 1: Write the failing test**

Append to the tests module of `src/runner.rs` (reusing the existing `make_file` and `make_test_config` helpers):

```rust
    #[tokio::test]
    async fn test_fallback_with_gappy_tracks_marks_skipped() {
        let client = Arc::new(MockClient::new());
        client.search_results_by_query.lock().unwrap().insert(
            "Test Album".into(),
            vec![SearchResult {
                username: "user1".into(),
                speed: 500,
                slots: 1,
                files: vec![
                    make_file(
                        r"Music\Test Artist\Test Album\01 - track.flac",
                        900,
                        10_000_000,
                    ),
                    make_file(
                        r"Music\Test Artist\Test Album\03 - track.flac",
                        900,
                        10_000_000,
                    ),
                ],
            }],
        );
        // Primary query "Test Artist Test Album" has no map entry -> empty.

        let config = make_test_config();
        let db = Database::open_in_memory().unwrap();
        let staging = TempDir::new().unwrap();

        let result = process_album(
            client.as_ref() as &dyn crate::client::SoulseekClient,
            "Test Artist",
            Some("Test Album"),
            &config,
            &db,
            staging.path(),
        )
        .await;
        assert!(result.is_ok());

        // Gappy track set rejected at the filter stage -> album skipped,
        // nothing downloaded.
        let rows = db.get_processed_albums().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, "skipped");
        assert!(client.download_filenames.lock().unwrap().is_empty());
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p seakarr test_fallback_with_gappy_tracks_marks_skipped`
Expected: FAIL — the album is NOT skipped (tracks 01+03 pass quality and would download), so `rows[0].status` is `"success"`.

- [ ] **Step 3: Update the "0 passed filters" log wording**

In `src/runner.rs`, replace the `filtered.is_empty()` log block:

```rust
    if filtered.is_empty() {
        tracing::info!(
            "{artist} — {}: {total_results} files from {total_users} users, 0 passed filters (need: {:?} format, free slot)",
            album.unwrap_or("(all)"),
            config.filters.allowed_extensions,
        );
```

with:

```rust
    if filtered.is_empty() {
        let contiguity_note = if config.filters.contiguous_tracks {
            ", contiguous track numbers"
        } else {
            ""
        };
        tracing::info!(
            "{artist} — {}: {total_results} files from {total_users} users, 0 passed filters (need: {:?} format, free slot{contiguity_note})",
            album.unwrap_or("(all)"),
            config.filters.allowed_extensions,
        );
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p seakarr test_fallback_with_gappy_tracks_marks_skipped`
Expected: PASS. Then the whole runner module: `cargo test -p seakarr runner::tests` — all pass, including `test_fallback_download_completes_album_and_records_history` (its single track 01 is trivially contiguous, so it still downloads).

- [ ] **Step 5: Commit**

```bash
cargo fmt
git add src/runner.rs
git commit -m "feat: skip albums whose fallback results have non-contiguous tracks"
```

---

### Task 6: README + full verification + final commit

**Files:**
- Modify: `README.md` (filters table)

- [ ] **Step 1: Add the README row**

In `README.md`, in the `### filters` table, after the `min_bitdepth` row add:

```markdown
| `contiguous_tracks` | Require downloaded track numbers to be contiguous — results with gaps (incomplete albums) are discounted at the search stage. Duplicate track numbers are permitted; set `false` for unnumbered or multi-disc collections. | `true` |
```

- [ ] **Step 2: Verify the README row renders as a table row (pipe-delimited, correct column count)**

Run: `markdownlint README.md`
Expected: exit 0.

- [ ] **Step 3: Format**

Run: `cargo fmt --check`
Expected: no output, exit 0. If not, run `cargo fmt` and re-check.

- [ ] **Step 4: Lint**

Run: `cargo clippy -p seakarr --all-targets -- -D warnings`
Expected: exit 0, no warnings.

- [ ] **Step 5: Full test suite (workspace includes the vendored soulseek-rs-lib)**

Run: `cargo test`
Expected: all tests PASS, including the vendored crate's regression tests.

- [ ] **Step 6: Commit**

```bash
git add README.md
git commit -m "docs: document filters.contiguous_tracks"
```

(If Steps 3-5 required changes, include those files in the same commit.)

---

## Self-Review Notes

- **Spec coverage:** §1 behaviour (toggle semantics, duplicates OK, gaps reject, unnumbered reject, filter-passing-only scope) → Tasks 3-4; §2 parsing rule (1-3 digit, first token, 4-digit skip) → Task 2; §2 module (`tracks.rs`, `lib.rs`) → Task 2; §2 filter wiring → Task 4; §2 config → Task 1; §2 README → Task 6; §3 data flow → Task 4 (predicate) + Task 5 (integration); §4 error handling (total parsing, no new errors) → Task 2 implementation + Task 5 (skipped path); §5 config surface → Task 1; §6 testing matrix → Tasks 1-5.
- **Type consistency:** `track_number_from_filename(&str) -> Option<u32>` and `files_have_contiguous_tracks(&[&FileInfo]) -> bool` are used identically in Task 2/3 definitions, Task 3/4 tests, and the Task 4 `filter_results` implementation. `FilterConfig.contiguous_tracks: bool` is consistent across Tasks 1 and 4. `MockClient.download_filenames` (existing, v0.3.2) is used in Task 5.
- **Existing tests preserved:** `filter_results` still returns whole results (`.cloned()`), so `download_album` re-filters by quality as before; pre-existing filter tests use contiguous or single-track sets and pass. The runner's existing fallback tests use track `01` (trivially contiguous).
