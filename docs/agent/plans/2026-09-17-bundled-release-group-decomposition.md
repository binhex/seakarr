# Bundled Release-Group Decomposition Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use sub-agents (recommended) to
> implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for
> tracking.

**Goal:** Stop seakarr from searching a MusicBrainz release-group title that
names several albums joined by a spaced slash, and search each named album under
its own title instead.

**Architecture:** A new pure module `src/discography/bundle.rs` recognises a
bundled title from its structure (a slash with whitespace on both sides) plus
evidence (every part is already a release-group title for that artist).
`select_albums` skips a group the rule reports as a bundle, so its parts - which
are already separate release groups - are selected under their own titles by the
existing code path. No configuration, schema, cache or search-path change.

**Tech Stack:** Rust 2021, `unicode-normalization` (already a dependency),
`tracing` for the exclusion log, in-repo unit tests with
`crate::test_support::LogCapture`.

**Spec:** the bundled release-group decomposition design, committed as
`docs/agent/specs/2026-09-17-bundled-release-group-decomposition-design.md`

---

## Scope check

One subsystem, one plan. The design touches a single decision point
(`select_albums`) and one new pure module. Nothing in this plan is independently
shippable, so it is not split.

## File structure

- `src/discography/bundle.rs` (create) - the rule: decompose a title, and
  decide whether it bundles other release groups. Pure, no I/O, owns its unit
  tests.
- `src/discography/mod.rs` (modify) - declare `mod bundle;`, build the artist's
  title set once in `select_albums` and skip bundled groups, plus integration
  tests in `mod tests`.
- `README.md` (modify) - two user-facing notes: the `discography` configuration
  section, and the authoritative-discovery narrative under `## How it works`.

Nothing else changes. `discover.rs`, `runner.rs`, `search.rs`,
`discography/musicbrainz.rs`, `config.rs`, `db.rs` and the search path are
untouched.

## Environment preconditions

Run these before Task 1 and stop if any fails:

```bash
cd /data/seakarr
git status --short          # expected: no output (clean tree)
git log --oneline -1        # expected: the spec commit
cargo clippy --version      # expected: clippy 0.1.97 or newer
cargo test --lib --quiet    # expected: ok, 690+ tests pass
```

The baseline lib test count is above 690 and all pass. Record the number so the
final task can show the increase.

---

### Task 1: Create `bundle.rs` and the syntactic decomposition

**Files:**

- Create: `src/discography/bundle.rs`
- Modify: `src/discography/mod.rs:10` (module declaration only, in this task)

- [ ] **Step 1: Create the module with tests and a RED stub**

Create `src/discography/bundle.rs` with this exact content. The implementation
is intentionally a stub so the "split" assertions fail first.

```rust
//! Recognition of MusicBrainz release groups whose title bundles several
//! albums.
//!
//! MusicBrainz publishes real release groups such as Archive's
//! `Controlling Crowds / You All Look the Same to Me` (Album, 2014), where
//! each part is also a release group of its own. A bundled title adds no
//! music: it only fabricates an album target that no peer share can match,
//! and it is searched again on every discover run.
//!
//! The rule is deliberately conservative. A title is a bundle only when every
//! part is already a release-group title for the same artist, so a genuine
//! title that happens to contain a spaced slash - Metallica's
//! `Live at Wembley Stadium, London, England / April 20th, 1992`, Oasis's A/B
//! single `Little by Little / She Is Love` - is left alone. Every ambiguous
//! shape resolves to "keep", which is the behaviour that existed before this
//! module.

use unicode_normalization::UnicodeNormalization;

/// Split a release-group title into the album titles it bundles.
///
/// Returns the trimmed parts only when the title has at least two parts, none
/// of them empty, and every `/` has whitespace on both sides. A slash without
/// surrounding whitespace is never a separator, so `Either/Or` and
/// `John Lennon/Plastic Ono Band` stay single titles.
///
/// The title is NFKC-folded before splitting so a fullwidth slash is
/// recognised, matching the fold [`normalize_catalog_key`] applies.
pub(crate) fn split_parts(title: &str) -> Option<Vec<String>> {
    // RED stub: replaced in step 4.
    let _ = title;
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Long titles are assembled from parts so no source line is unwrapped in
    /// a way that changes the string.
    const JAMIROQUAI_BUNDLE: &str = concat!(
        "Emergency on Planet Earth",
        " / The Return of the Space Cowboy",
        " / Travelling Without Moving"
    );
    const DYLAN_BUNDLE: &str = concat!(
        "The Freewheelin\u{2019} Bob Dylan",
        " / The Times They Are A\u{2010}Changin\u{2019}",
        " / Another Side Of Bob Dylan"
    );

    #[test]
    fn decomposition_splits_spaced_slashes_and_trims_each_part() {
        assert_eq!(
            split_parts("Controlling Crowds / You All Look the Same to Me"),
            Some(vec![
                "Controlling Crowds".to_owned(),
                "You All Look the Same to Me".to_owned(),
            ])
        );
        assert_eq!(
            split_parts("  Controlling Crowds   /   Noise  "),
            Some(vec!["Controlling Crowds".to_owned(), "Noise".to_owned()])
        );
        assert_eq!(
            split_parts(JAMIROQUAI_BUNDLE),
            Some(vec![
                "Emergency on Planet Earth".to_owned(),
                "The Return of the Space Cowboy".to_owned(),
                "Travelling Without Moving".to_owned(),
            ])
        );
        assert_eq!(
            split_parts(DYLAN_BUNDLE),
            Some(vec![
                "The Freewheelin\u{2019} Bob Dylan".to_owned(),
                "The Times They Are A\u{2010}Changin\u{2019}".to_owned(),
                "Another Side Of Bob Dylan".to_owned(),
            ])
        );
    }

    #[test]
    fn decomposition_folds_a_fullwidth_slash_before_splitting() {
        // U+FF0F FULLWIDTH SOLIDUS folds to '/' under NFKC.
        assert_eq!(
            split_parts("Controlling Crowds \u{ff0f} Noise"),
            Some(vec!["Controlling Crowds".to_owned(), "Noise".to_owned()])
        );
    }

    #[test]
    fn decomposition_keeps_titles_whose_slashes_are_not_separators() {
        assert_eq!(split_parts("Either/Or"), None);
        assert_eq!(split_parts("John Lennon/Plastic Ono Band"), None);
        assert_eq!(split_parts("Alternate Versions From either/or"), None);
        assert_eq!(
            split_parts("Controlling Crowds /You All Look the Same to Me"),
            None
        );
        assert_eq!(
            split_parts("Controlling Crowds/ You All Look the Same to Me"),
            None
        );
        assert_eq!(split_parts("Controlling Crowds"), None);
    }

    #[test]
    fn decomposition_rejects_empty_shapes() {
        assert_eq!(split_parts(""), None);
        assert_eq!(split_parts("   "), None);
        assert_eq!(split_parts(" / "), None);
        assert_eq!(split_parts("Controlling Crowds / "), None);
        assert_eq!(split_parts(" / Noise"), None);
        assert_eq!(split_parts("Controlling Crowds / / Noise"), None);
    }
}
```

- [ ] **Step 2: Declare the module**

In `src/discography/mod.rs`, replace line 10:

```rust
mod musicbrainz;
```

with:

```rust
mod bundle;
mod musicbrainz;
```

- [ ] **Step 3: Run the tests to verify the RED phase**

Run:

```bash
cargo test --lib discography::bundle -- --nocapture
```

Expected: `decomposition_keeps_titles_whose_slashes_are_not_separators` and
`decomposition_rejects_empty_shapes` PASS (the stub already returns `None`), and
`decomposition_splits_spaced_slashes_and_trims_each_part` and
`decomposition_folds_a_fullwidth_slash_before_splitting` FAIL with
`left: None, right: Some([...])`. That failure is the RED phase for this task.

The run also prints one unused-import warning for `UnicodeNormalization`, which
Step 4 removes by using it. Do not silence it with `#[allow]`.

- [ ] **Step 4: Implement `split_parts`**

Replace the stub body in `src/discography/bundle.rs` with:

```rust
pub(crate) fn split_parts(title: &str) -> Option<Vec<String>> {
    let folded: String = title.nfkc().collect();
    let characters: Vec<char> = folded.chars().collect();
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut separators = 0usize;
    for (index, character) in characters.iter().enumerate() {
        if *character != '/' {
            current.push(*character);
            continue;
        }
        let spaced_before = index > 0 && characters[index - 1].is_whitespace();
        let spaced_after = characters
            .get(index + 1)
            .is_some_and(|next| next.is_whitespace());
        if !spaced_before || !spaced_after {
            return None;
        }
        separators += 1;
        parts.push(current.trim().to_owned());
        current = String::new();
    }
    if separators == 0 {
        return None;
    }
    parts.push(current.trim().to_owned());
    if parts.len() < 2 || parts.iter().any(String::is_empty) {
        return None;
    }
    Some(parts)
}
```

Note the loop indexes `characters`, never a byte slice, so a multi-byte title
cannot panic.

- [ ] **Step 5: Run the tests to verify GREEN**

Run:

```bash
cargo test --lib discography::bundle -- --nocapture
```

Expected: 4 passed, 0 failed.

- [ ] **Step 6: Format, lint and commit**

```bash
cargo fmt
cargo clippy --all-targets -- -D warnings
git add src/discography/bundle.rs src/discography/mod.rs
git commit -m "feat: decompose bundled MusicBrainz release-group titles"
```

Expected: clippy exits 0 with no warnings; the commit records the new module and
the one-line declaration.

---

### Task 2: Add the semantic check `is_bundle`

**Files:**

- Modify: `src/discography/bundle.rs`

- [ ] **Step 1: Write the failing tests**

Append these helpers and tests inside the existing `mod tests` block of
`src/discography/bundle.rs`, after the `decomposition_rejects_empty_shapes`
test. Also add these two imports to the top of `mod tests`, before
`use super::*;`, so the helpers do not depend on the parent module's private
imports:

```rust
    use std::collections::BTreeSet;

    use crate::discography::normalize_catalog_key;
```

Then add this constant beside `JAMIROQUAI_BUNDLE` and `DYLAN_BUNDLE`, which the
new tests need:

```rust
    const METALLICA_VENUE: &str = concat!(
        "Live at Wembley Stadium, London, England",
        " / April 20th, 1992"
    );
```

Then append these helpers and tests at the end of `mod tests`:

```rust
    /// Release-group titles each artist also has, normalised the way the
    /// production code compares them. Taken from the live MusicBrainz sample
    /// of 2026-09-17.
    fn titles(values: &[&str]) -> BTreeSet<String> {
        values
            .iter()
            .map(|value| normalize_catalog_key(value))
            .collect()
    }

    fn archive_titles() -> BTreeSet<String> {
        titles(&[
            "Controlling Crowds",
            "You All Look the Same to Me",
            "Noise",
            "Londinium",
        ])
    }

    fn jamiroquai_titles() -> BTreeSet<String> {
        titles(&[
            "Emergency on Planet Earth",
            "The Return of the Space Cowboy",
            "Travelling Without Moving",
        ])
    }

    fn dylan_titles() -> BTreeSet<String> {
        titles(&[
            "The Freewheelin\u{2019} Bob Dylan",
            "The Times They Are A\u{2010}Changin\u{2019}",
            "Another Side Of Bob Dylan",
            "Oh Mercy",
            "Time Out of Mind",
            "Blonde on Blonde",
        ])
    }

    fn metallica_titles() -> BTreeSet<String> {
        titles(&["Metallica", "Master of Puppets", "Load"])
    }

    fn oasis_titles() -> BTreeSet<String> {
        titles(&["Definitely Maybe", "(What's the Story) Morning Glory?"])
    }

    fn elliott_smith_titles() -> BTreeSet<String> {
        titles(&["Roman Candle", "Elliott Smith", "Either/Or"])
    }

    fn john_lennon_titles() -> BTreeSet<String> {
        titles(&["John Lennon/Plastic Ono Band", "Double Fantasy", "Imagine"])
    }

    #[test]
    fn bundled_titles_are_recognised_from_their_known_albums() {
        assert!(is_bundle(
            "Controlling Crowds / You All Look the Same to Me",
            &archive_titles()
        ));
        assert!(is_bundle(
            "You All Look the Same to Me / Noise",
            &archive_titles()
        ));
        assert!(is_bundle(JAMIROQUAI_BUNDLE, &jamiroquai_titles()));
        assert!(is_bundle(DYLAN_BUNDLE, &dylan_titles()));
    }

    #[test]
    fn titles_that_name_albums_the_artist_does_not_have_are_kept() {
        assert!(!is_bundle("Wiped Out / Violently", &archive_titles()));
        assert!(!is_bundle("Little by Little / She Is Love", &oasis_titles()));
        assert!(!is_bundle(METALLICA_VENUE, &metallica_titles()));
        assert!(!is_bundle(
            "2cd: Highway 61 Revisited / Blonde on Blonde",
            &dylan_titles()
        ));
        assert!(!is_bundle(
            "Time Out of Mind / Love and Theft",
            &dylan_titles()
        ));
        assert!(!is_bundle(
            "Roman Candle / Elliott Smith / Either/Or",
            &elliott_smith_titles()
        ));
        assert!(!is_bundle("Elton John / John Lennon", &john_lennon_titles()));
        assert!(!is_bundle("Either/Or", &elliott_smith_titles()));
    }

    #[test]
    fn a_title_is_never_a_bundle_of_itself() {
        let only_itself =
            titles(&["Controlling Crowds / You All Look the Same to Me"]);
        assert!(!is_bundle(
            "Controlling Crowds / You All Look the Same to Me",
            &only_itself
        ));
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run:

```bash
cargo test --lib discography::bundle
```

Expected: compile error naming the missing function, for example
`cannot find function 'is_bundle' in this scope`. A compile failure is this
task's RED phase: the behaviour does not exist yet.

- [ ] **Step 3: Implement `is_bundle`**

Add this function to `src/discography/bundle.rs`, immediately after
`split_parts`. It needs two imports that Task 1 did not add, because nothing
used them until now: put `use std::collections::BTreeSet;` first at the top of
the file, then a blank line, then `use super::normalize_catalog_key;`, so the
file begins:

```rust
use std::collections::BTreeSet;

use unicode_normalization::UnicodeNormalization;

use super::normalize_catalog_key;
```

Then append:

```rust
/// True when `title` names several albums that the same artist already has as
/// separate release groups.
///
/// `artist_titles` holds [`normalize_catalog_key`] of every release-group title
/// returned for the artist, including groups the configured release-type filter
/// later rejects, so a part counts as known even when its own release group is
/// not selectable.
pub(crate) fn is_bundle(title: &str, artist_titles: &BTreeSet<String>) -> bool {
    let Some(parts) = split_parts(title) else {
        return false;
    };
    let own_key = normalize_catalog_key(title);
    parts.iter().all(|part| {
        let part_key = normalize_catalog_key(part);
        !part_key.is_empty()
            && part_key != own_key
            && artist_titles.contains(&part_key)
    })
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run:

```bash
cargo test --lib discography::bundle -- --nocapture
```

Expected: 7 passed, 0 failed.

- [ ] **Step 5: Format, lint and commit**

```bash
cargo fmt
cargo clippy --all-targets -- -D warnings
git add src/discography/bundle.rs
git commit -m "feat: recognise bundled release-group titles from known albums"
```

Expected: clippy exits 0; one commit.

---

### Task 3: Wire the rule into `select_albums`

**Files:**

- Modify: `src/discography/mod.rs` (`select_albums`, currently lines 397-421,
  and the `mod tests` block)

- [ ] **Step 1: Write the failing integration tests**

Insert these tests and the fixture into `mod tests` in
`src/discography/mod.rs`, immediately after the existing
`excluded_release_groups_are_logged_with_reasons` test:

```rust
    /// The live MusicBrainz release groups for Archive on 2026-09-17, reduced
    /// to the three that matter: the two-album bundle and the albums it names.
    /// The last MBID is a test fixture, not a real MusicBrainz ID.
    fn archive_bundle_groups() -> Vec<ReleaseGroup> {
        vec![
            group(
                "3eecc2ca-8d9e-4159-8bee-cd84173a5fba",
                "Controlling Crowds / You All Look the Same to Me",
                Some("2014"),
                Some("Album"),
                &[],
            ),
            group(
                "532ed7d7-ad97-3771-b9e8-333f1c25adc2",
                "Controlling Crowds",
                Some("2009-03-27"),
                Some("Album"),
                &[],
            ),
            group(
                "aa11bb22-cc33-44dd-55ee-66ff77008899",
                "You All Look the Same to Me",
                Some("2002-03-12"),
                Some("Album"),
                &[],
            ),
        ]
    }

    #[test]
    fn a_bundled_release_group_is_not_selected_and_its_albums_are() {
        let albums = select_albums(
            &archive_bundle_groups(),
            &[DiscographyReleaseType::StudioAlbum],
        );
        let titles: Vec<&str> = albums
            .iter()
            .map(|album| album.title.as_str())
            .collect();
        assert_eq!(
            titles,
            vec!["You All Look the Same to Me", "Controlling Crowds"]
        );
        let bundle_id = "3eecc2ca-8d9e-4159-8bee-cd84173a5fba";
        assert!(albums
            .iter()
            .all(|album| album.release_group_id != bundle_id));
        assert_eq!(
            albums[1].release_group_id,
            "532ed7d7-ad97-3771-b9e8-333f1c25adc2"
        );
    }

    #[test]
    fn a_venue_and_date_title_stays_a_single_target() {
        let groups = vec![
            group(
                "m1",
                concat!(
                    "Live at Wembley Stadium, London, England",
                    " / April 20th, 1992"
                ),
                Some("1992-04-20"),
                Some("Album"),
                &[],
            ),
            group("m2", "Metallica", Some("1991-08-12"), Some("Album"), &[]),
        ];
        let albums =
            select_albums(&groups, &[DiscographyReleaseType::StudioAlbum]);
        let titles: Vec<&str> = albums
            .iter()
            .map(|album| album.title.as_str())
            .collect();
        assert_eq!(
            titles,
            vec![
                "Metallica",
                "Live at Wembley Stadium, London, England / April 20th, 1992"
            ]
        );
    }

    #[test]
    fn a_bundle_is_dropped_when_one_part_is_filtered_out() {
        let groups = vec![
            group(
                "b1",
                "First Album / Second Album",
                Some("2020"),
                Some("Album"),
                &[],
            ),
            group("b2", "First Album", Some("2001"), Some("EP"), &[]),
            group(
                "b3",
                "Second Album",
                Some("2002"),
                Some("Album"),
                &[],
            ),
        ];
        let albums =
            select_albums(&groups, &[DiscographyReleaseType::StudioAlbum]);
        let titles: Vec<&str> = albums
            .iter()
            .map(|album| album.title.as_str())
            .collect();
        assert_eq!(titles, vec!["Second Album"]);
    }

    #[test]
    fn a_bundled_release_group_is_logged_with_its_reason() {
        let capture = crate::test_support::LogCapture::start();
        let albums = select_albums(
            &archive_bundle_groups(),
            &[DiscographyReleaseType::StudioAlbum],
        );
        assert_eq!(albums.len(), 2);
        let logs = capture.text();
        assert!(
            logs.contains("3eecc2ca-8d9e-4159-8bee-cd84173a5fba"),
            "got: {logs}"
        );
        assert!(
            logs.contains("title bundles other release groups"),
            "got: {logs}"
        );
    }
```

- [ ] **Step 2: Run the tests to verify the RED phase**

Run:

```bash
cargo test --lib discography::tests::a_ -- --nocapture
```

Expected: three failures, and one pass. (This filter selects the four new
tests plus any pre-existing discography test whose name begins `a_`.)

- `a_bundled_release_group_is_not_selected_and_its_albums_are` FAILS: the
  selected titles are `["You All Look the Same to Me", "Controlling Crowds",
  "Controlling Crowds / You All Look the Same to Me"]`, not the two expected.
- `a_bundle_is_dropped_when_one_part_is_filtered_out` FAILS: the selected titles
  are `["Second Album", "First Album / Second Album"]`.
- `a_bundled_release_group_is_logged_with_its_reason` FAILS on the
  `title bundles other release groups` assertion.
- `a_venue_and_date_title_stays_a_single_target` PASSES, before and after the
  change. It is a guard against over-splitting, not part of the RED phase.

This is the RED phase against real production code, not a stub.

- [ ] **Step 3: Build the artist title set in `select_albums`**

In `src/discography/mod.rs`, inside `select_albums`, immediately after the
`Candidate` struct definition and before `let mut best:`, insert:

```rust
    // Every release-group title the artist has, before the release-type filter
    // runs, so a bundle part counts as known even when its own release group is
    // not selectable under the configured categories.
    let artist_titles: std::collections::BTreeSet<String> = groups
        .iter()
        .map(|group| normalize_catalog_key(&group.title))
        .filter(|key| !key.is_empty())
        .collect();
```

- [ ] **Step 4: Skip bundled groups**

In the same function, immediately after the existing empty-title `continue`
block, insert:

```rust
        if bundle::is_bundle(title, &artist_titles) {
            log_rejected_release(release, "title bundles other release groups");
            continue;
        }
```

The surrounding code must then read, in this order: `release_allowed`, the
empty-title check, the bundle check, then `normalize_catalog_key(title)` and the
dedupe. Change nothing else in the function.

Do not touch any other file. `select_albums` is the only place that turns
release groups into targets, so this single call site is what confines the new
behaviour to discover mode and artist-only runs. Explicit `--artist --album`
input and batch wantlist lines reach `search_album_with_fallback*` directly, so
they keep searching exactly the text the user supplied without any further
guard.

- [ ] **Step 5: Run the tests to verify GREEN**

Run:

```bash
cargo test --lib discography -- --nocapture
```

Expected: all discography tests pass, including the four new ones and the
pre-existing `excluded_release_groups_are_logged_with_reasons`,
`select_albums` dedupe and ordering tests.

- [ ] **Step 6: Run the whole suite**

Run:

```bash
cargo test --quiet
```

Expected: all tests pass, with the lib count at the baseline from the
preconditions plus eleven new tests: seven in `bundle.rs` (four from Task 1,
three from Task 2) and four integration tests in `discography::tests`. The run
reports the exact number, and no test may fail.

- [ ] **Step 7: Format, lint and commit**

```bash
cargo fmt
cargo clippy --all-targets -- -D warnings
git add src/discography/mod.rs
git commit -m "feat: skip bundled release groups when selecting albums"
```

Expected: clippy exits 0; one commit.

---

### Task 4: Document the behaviour

**Files:**

- Modify: `README.md` (the `### discography` section, and the
  authoritative-discovery bullet list under `## How it works`)

- [ ] **Step 1: Add the configuration-section paragraph**

In `README.md`, in the `### discography` section, insert this paragraph between
the existing intro paragraph (which ends `The MusicBrainz API needs no account
or API` / `key.`) and the `| Key | Description | Default |` table:

```markdown
A release group whose title names several albums joined by a spaced slash, such
as Archive's `Controlling Crowds / You All Look the Same to Me`, is not used as
an album name. Each named album is already a release group of its own, so it is
searched under its own title instead of the concatenation no Soulseek share can
match. A title keeps being searched as MusicBrainz spells it unless every part
of it is already one of that artist's release groups, so `Either/Or` is
unaffected, and so is a venue-and-date title such as
`Live at Wembley Stadium, London, England / April 20th, 1992`.
```

Wrap the inserted text to match the surrounding README style (about 100
columns). It is wrapped narrower here only to keep this plan lint-clean; the
rendered paragraph is identical either way.

- [ ] **Step 2: Add the `How it works` bullet**

In `README.md`, under `## How it works`, in the release-group bullet list that
contains the bullet beginning `- A successful refresh replaces the cached copy
atomically.`, insert this bullet immediately after that one:

```markdown
- A release group whose title names several albums joined by a spaced slash is
  not selected when every named album is also a release group for that artist.
  Those albums are searched under their own titles, so nothing is searched under
  a name no peer can match. A title that names an album the artist does not have
  as a separate release group is searched exactly as MusicBrainz spells it,
  which leaves disc-prefixed box sets such as
  `2cd: Highway 61 Revisited / Blonde on Blonde` unchanged.
```

- [ ] **Step 3: Lint the README**

Run:

```bash
markdownlint README.md
```

Expected: exit 0, no output. (The file disables the line-length rule, so the
wrapping above is a style match, not a requirement.)

- [ ] **Step 4: Commit**

```bash
git add README.md
git commit -m "docs: describe bundled release-group title handling"
```

Expected: one commit.

---

### Task 5: Full verification gates

**Files:** none. This task proves the change is safe; it must not add code.

- [ ] **Step 1: Run every gate**

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --quiet
pre-commit run --all-files
```

Expected: every command exits 0, all tests pass, and the pre-commit hooks report
`Passed` for each hook.

- [ ] **Step 2: Check coverage did not regress**

```bash
cargo llvm-cov --summary-only 2>&1 | tail -3
```

Expected: a `TOTAL` line at or above the 95.46% baseline recorded before this
change. Every line added by Tasks 1 to 3 is exercised by tests in this plan, so
a drop means a test in Task 1, 2 or 3 was not actually run.

- [ ] **Step 3: Confirm the change is confined to the planned files**

```bash
git diff --stat HEAD~4
```

Expected: exactly `README.md`, `src/discography/bundle.rs` and
`src/discography/mod.rs`, with no other file touched. Adjust `HEAD~4` if a task
needed an extra commit.

- [ ] **Step 4: Report**

Report the four gate results, the coverage `TOTAL` line, and the
`git diff --stat` output. Do not claim the work is complete without all four
gates having passed in this task.

---

## Acceptance criteria

Copied from the spec, to be checked against the finished change:

- A discover run or artist-only run for Archive logs no `Processing: Archive —
  Controlling Crowds / You All Look the Same to Me`, and its MusicBrainz targets
  no longer include that title.
- `Controlling Crowds` and `You All Look the Same to Me` remain targets,
  searched under their own titles, and skipped when the library already holds
  them.
- Metallica's venue-and-date title, `Either/Or` and every A/B single in the
  sample remain single targets.
- Selection for an artist with no bundled titles is byte-for-byte identical to
  before.
- `--artist X --album Y` and batch lines are unaffected.

## Out of scope

Do not implement any of the following, even if it looks like a small win:

- separators other than a slash with whitespace on both sides;
- disc or format prefix stripping (`2cd:`);
- punctuation-insensitive or `&`/`and` part matching;
- synthesising targets for a bundle's parts;
- rewriting titles inside the MusicBrainz provider or changing the cache
  payload;
- changing explicit `--artist --album`, batch, auto, or library-upgrade paths;
- repairing bundled album folders already on disk.
