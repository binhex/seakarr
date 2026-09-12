# Authoritative Artist Discography Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use sub-agents (recommended)
> to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for
> tracking.

**Goal:** Replace noisy artist-only Soulseek discovery with cached MusicBrainz
release groups and one targeted Soulseek search per eligible conceptual album.

**Architecture:** A new discography module separates pure catalog rules and
cache orchestration from a bounded MusicBrainz HTTP provider. Artist-only
manual mode consumes explicit authoritative, empty, stale-cache, or
legacy-fallback outcomes while reusing the existing per-album pipeline
unchanged.

**Tech Stack:** Rust 2021, Tokio, reqwest 0.12, serde/serde_json, rusqlite,
async-trait, thiserror, unicode-normalization, wiremock, tracing, Cargo,
Clippy, rustfmt, markdownlint, and pre-commit.

---

<!-- markdownlint-disable MD013 -->

## Planning decisions

### Scope

Keep this as one plan. Provider retrieval, classification, SQLite cache, summary notices, and runner integration are coupled layers of one artist-only feature. Splitting them would create intermediate states that either fetch data without using it or change runner behavior without an authoritative source.

Do not change explicit artist-plus-album, album-only, batch, automatic library-upgrade, vendor Soulseek, download, filtering, ranking, organization, or notification behavior. Keep `search::group_artist_results` as the supported legacy fallback.

### Repository and commit guard

The approved specification is committed at `6544916` on `main`, which is one commit ahead of `origin/main`. Start implementation from that state and preserve the spec exactly unless a review finding requires a documented amendment.

Project rules require code review before code commits. Therefore each task ends with an **unstaged checkpoint** rather than a commit. Keep changes unstaged and uncommitted through implementation, verification, tech-debt, and review. The finalising chain step owns the eventual commit/release choice. Never use `git add .`.

### File map

| File | Responsibility |
| --- | --- |
| `Cargo.toml` | Declare `serde_json` directly; no new package should be downloaded because it is already locked transitively. |
| `Cargo.lock` | Record only dependency-edge changes produced by Cargo, if any. |
| `src/lib.rs` | Export the new `discography` module. |
| `src/discography/mod.rs` | Catalog domain types, provider trait, NFKC keys, exact artist resolution, release classification, date parsing, deduplication, ordering, cache precedence, and discovery outcomes. |
| `src/discography/musicbrainz.rs` | MusicBrainz HTTP models, User-Agent, bounded response streaming, one-request-per-second pacing, retries, artist search/by-ID lookup, and release-group pagination. |
| `src/config.rs` | `DiscographyConfig`, friendly release categories, defaults, YAML reconciliation, MBID/key validation, and config tests. |
| `src/db.rs` | `discography_cache` schema and read/upsert/delete methods. |
| `src/report.rs` | General run-summary notices and tests. |
| `src/runner.rs` | Provider injection seam, authoritative target processing, empty outcome, stale warning, explicit legacy mode, and warned automatic fallback. |
| `vendor/soulseek-rs-lib/src/actor/peer_actor.rs` | Verification-only Clippy compatibility: make the non-mutating transfer-request handler borrow `self` immutably. |
| `README.md` | User-facing setup, categories, cache semantics, ambiguity handling, and fallback visibility. |
| `docs/agent/specs/2026-09-10-authoritative-artist-discography-design.md` | Read-only implementation contract unless a review-approved correction is necessary. |

## Task 1: Add the configuration contract

**Files:**

- Create: `src/discography/mod.rs`
- Modify: `src/lib.rs`
- Modify: `src/config.rs`
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`

- [ ] **Step 1: Write RED normalization and configuration tests**

Create `src/discography/mod.rs` containing only the normalization test and add `pub mod discography;` to `src/lib.rs` so Cargo can compile that RED test. Do not add the production function yet. Add these tests to `src/config.rs` and the new module test block:

```rust
#[test]
fn discography_defaults_are_authoritative_studio_albums() {
    let config = Config::default();
    assert!(config.discography.enabled);
    assert_eq!(config.discography.cache_days, 30);
    assert_eq!(
        config.discography.allowed_types,
        vec![DiscographyReleaseType::StudioAlbum]
    );
    assert!(config.discography.artist_mbids.is_empty());
}

#[test]
fn discography_validation_rejects_invalid_values() {
    let mut config = Config::default();
    config.discography.allowed_types.clear();
    assert!(config
        .validate_non_credential_constraints()
        .unwrap_err()
        .to_string()
        .contains("discography.allowed_types"));

    let mut config = Config::default();
    config.discography.artist_mbids.insert(
        "Artist".to_string(),
        "not-a-musicbrainz-id".to_string(),
    );
    assert!(config
        .validate_non_credential_constraints()
        .unwrap_err()
        .to_string()
        .contains("discography.artist_mbids"));

    let mut config = Config::default();
    config
        .discography
        .artist_mbids
        .insert("Artist".to_string(), "11111111-1111-1111-1111-111111111111".to_string());
    config.discography.artist_mbids.insert(
        " artist ".to_string(),
        "22222222-2222-2222-2222-222222222222".to_string(),
    );
    assert!(config
        .validate_non_credential_constraints()
        .unwrap_err()
        .to_string()
        .contains("duplicate normalized artist"));
}

#[test]
fn existing_yaml_is_reconciled_with_discography_defaults() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("seakarr.yml"),
        "soulseek:\n  username: user\n  password: pass\n",
    )
    .unwrap();

    let config = Config::load(dir.path()).unwrap();
    let contents = fs::read_to_string(dir.path().join("seakarr.yml")).unwrap();

    assert!(config.discography.enabled);
    assert!(contents.contains("discography:"));
    assert!(contents.contains("cache_days: 30"));
    assert!(contents.contains("studio_album"));
}
```

```rust
#[test]
fn catalog_key_uses_nfkc_case_and_whitespace_without_dropping_punctuation() {
    assert_eq!(normalize_catalog_key("  ＡC/DC  "), "ac/dc");
    assert_ne!(normalize_catalog_key("AC/DC"), normalize_catalog_key("AC DC"));
}
```

- [ ] **Step 2: Run the RED tests**

```bash
cargo test -p seakarr discography_defaults_are_authoritative_studio_albums -- --nocapture
cargo test -p seakarr catalog_key_uses_nfkc_case_and_whitespace_without_dropping_punctuation -- --nocapture
```

Expected: compilation fails because `Config::discography`, `DiscographyReleaseType`, and `normalize_catalog_key` do not exist.

- [ ] **Step 3: Add the direct JSON dependency and module boundary**

Add to `[dependencies]` in `Cargo.toml`:

```toml
serde_json = "1"
```

Replace the test-only contents of `src/discography/mod.rs` with the shared key function plus the unchanged test:

```rust
use unicode_normalization::UnicodeNormalization;

pub(crate) fn normalize_catalog_key(value: &str) -> String {
    value
        .nfkc()
        .flat_map(char::to_lowercase)
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_key_uses_nfkc_case_and_whitespace_without_dropping_punctuation() {
        assert_eq!(normalize_catalog_key("  ＡC/DC  "), "ac/dc");
        assert_ne!(normalize_catalog_key("AC/DC"), normalize_catalog_key("AC DC"));
    }
}
```

Run `cargo check -p seakarr` once so Cargo updates only the root package's direct dependency list in `Cargo.lock`.

- [ ] **Step 4: Add exact configuration types and defaults**

Import `std::collections::{BTreeMap, HashSet}` and add:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiscographyReleaseType {
    StudioAlbum,
    LiveAlbum,
    Ep,
    Single,
    Compilation,
    Remix,
    Soundtrack,
    DjMix,
    Mixtape,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscographyConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_discography_cache_days")]
    pub cache_days: u64,
    #[serde(default = "default_discography_release_types")]
    pub allowed_types: Vec<DiscographyReleaseType>,
    #[serde(default)]
    pub artist_mbids: BTreeMap<String, String>,
}

fn default_discography_cache_days() -> u64 {
    30
}

fn default_discography_release_types() -> Vec<DiscographyReleaseType> {
    vec![DiscographyReleaseType::StudioAlbum]
}

impl Default for DiscographyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            cache_days: default_discography_cache_days(),
            allowed_types: default_discography_release_types(),
            artist_mbids: BTreeMap::new(),
        }
    }
}
```

Add `pub discography: DiscographyConfig` to `Config` and `discography: DiscographyConfig::default()` to `Config::default()` between `search` and `filters`, keeping YAML order stable.

MBID validation proves canonical UUID shape only. A well-formed nonexistent ID is deliberately not negatively cached; each later stale/uncached run attempts MusicBrainz again before following normal stale/legacy fallback precedence.

Add this validator and call it from `validate_non_credential_constraints`:

```rust
fn valid_mbid(value: &str) -> bool {
    const GROUPS: [usize; 5] = [8, 4, 4, 4, 12];
    let parts: Vec<&str> = value.split('-').collect();
    parts.len() == GROUPS.len()
        && parts
            .iter()
            .zip(GROUPS)
            .all(|(part, length)| part.len() == length && part.chars().all(|c| c.is_ascii_hexdigit()))
}

fn validate_discography(&self) -> Result<()> {
    if self.discography.enabled && self.discography.allowed_types.is_empty() {
        return Err(SeakarrError::Config(
            "discography.allowed_types must not be empty when discography.enabled is true".into(),
        ));
    }
    if self.discography.cache_days > i64::MAX as u64 / 86_400 {
        return Err(SeakarrError::Config(
            "discography.cache_days is too large".into(),
        ));
    }
    let mut normalized = HashSet::new();
    for (artist, mbid) in &self.discography.artist_mbids {
        let key = crate::discography::normalize_catalog_key(artist);
        if key.is_empty() || !normalized.insert(key) {
            return Err(SeakarrError::Config(format!(
                "discography.artist_mbids contains an empty or duplicate normalized artist key: {artist:?}"
            )));
        }
        if !valid_mbid(mbid) {
            return Err(SeakarrError::Config(format!(
                "discography.artist_mbids contains an invalid MusicBrainz ID for {artist:?}"
            )));
        }
    }
    Ok(())
}
```

Insert the `discography` block into `sample_yaml()` so deserialization tests exercise explicit values.

- [ ] **Step 5: Run GREEN config checks**

```bash
cargo test -p seakarr discography -- --nocapture
cargo test -p seakarr config::tests -- --nocapture
cargo check -p seakarr
```

Expected: all config and normalization tests pass; `Cargo.lock` contains no new downloaded package, only `serde_json` in seakarr's dependency list if Cargo rewrites it.

- [ ] **Step 6: Record the unstaged checkpoint**

```bash
git diff --check
git status --short
```

Expected paths: `Cargo.toml`, possibly `Cargo.lock`, `src/lib.rs`, `src/config.rs`, and `src/discography/mod.rs`. Do not stage or commit.

## Task 2: Add persistent discography cache storage

**Files:**

- Modify: `src/db.rs`

- [ ] **Step 1: Write RED cache schema and round-trip tests**

Add:

```rust
#[test]
fn discography_cache_round_trips_and_replaces_atomically() {
    let db = test_db();
    let first = DiscographyCacheEntry {
        artist_key: "test artist".into(),
        artist_mbid: "11111111-1111-1111-1111-111111111111".into(),
        canonical_artist: "Test Artist".into(),
        fetched_at: 1_000,
        release_groups_json: "[]".into(),
    };
    db.upsert_discography_cache(&first).unwrap();
    assert_eq!(db.get_discography_cache("test artist").unwrap(), Some(first));

    let replacement = DiscographyCacheEntry {
        artist_key: "test artist".into(),
        artist_mbid: "22222222-2222-2222-2222-222222222222".into(),
        canonical_artist: "Test Artist Two".into(),
        fetched_at: 2_000,
        release_groups_json: "[{}]".into(),
    };
    db.upsert_discography_cache(&replacement).unwrap();
    assert_eq!(
        db.get_discography_cache("TEST ARTIST").unwrap(),
        Some(replacement)
    );
    assert!(db.delete_discography_cache("test artist").unwrap());
    assert!(db.get_discography_cache("test artist").unwrap().is_none());
}
```

Extend `test_create_tables` with:

```rust
assert!(tables.contains(&"discography_cache".to_string()));
```

- [ ] **Step 2: Run the RED database test**

```bash
cargo test -p seakarr db::tests::discography_cache_round_trips_and_replaces_atomically -- --nocapture
```

Expected: compilation fails because `DiscographyCacheEntry` and the cache methods do not exist.

- [ ] **Step 3: Add the cache schema and methods**

Add beside the other database domain structs:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscographyCacheEntry {
    pub artist_key: String,
    pub artist_mbid: String,
    pub canonical_artist: String,
    pub fetched_at: i64,
    pub release_groups_json: String,
}
```

Add this table to `Database::migrate()` without dropping existing data:

```sql
CREATE TABLE IF NOT EXISTS discography_cache (
    artist_key         TEXT PRIMARY KEY COLLATE NOCASE,
    artist_mbid        TEXT NOT NULL,
    canonical_artist   TEXT NOT NULL,
    fetched_at         INTEGER NOT NULL,
    release_groups_json TEXT NOT NULL
);
```

Add these methods:

```rust
pub fn get_discography_cache(&self, artist_key: &str) -> Result<Option<DiscographyCacheEntry>> {
    self.conn
        .query_row(
            "SELECT artist_key, artist_mbid, canonical_artist, fetched_at, release_groups_json
             FROM discography_cache WHERE artist_key = ?1 COLLATE NOCASE",
            params![artist_key],
            |row| {
                Ok(DiscographyCacheEntry {
                    artist_key: row.get(0)?,
                    artist_mbid: row.get(1)?,
                    canonical_artist: row.get(2)?,
                    fetched_at: row.get(3)?,
                    release_groups_json: row.get(4)?,
                })
            },
        )
        .optional()
        .map_err(Into::into)
}

pub fn upsert_discography_cache(&self, entry: &DiscographyCacheEntry) -> Result<()> {
    self.conn.execute(
        "INSERT INTO discography_cache
         (artist_key, artist_mbid, canonical_artist, fetched_at, release_groups_json)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(artist_key) DO UPDATE SET
           artist_mbid = excluded.artist_mbid,
           canonical_artist = excluded.canonical_artist,
           fetched_at = excluded.fetched_at,
           release_groups_json = excluded.release_groups_json",
        params![
            entry.artist_key,
            entry.artist_mbid,
            entry.canonical_artist,
            entry.fetched_at,
            entry.release_groups_json,
        ],
    )?;
    Ok(())
}

pub fn delete_discography_cache(&self, artist_key: &str) -> Result<bool> {
    Ok(self.conn.execute(
        "DELETE FROM discography_cache WHERE artist_key = ?1 COLLATE NOCASE",
        params![artist_key],
    )? > 0)
}
```

- [ ] **Step 4: Run GREEN database tests**

```bash
cargo test -p seakarr db::tests::discography_cache_round_trips_and_replaces_atomically -- --nocapture
cargo test -p seakarr db::tests::test_create_tables -- --nocapture
```

Expected: both tests pass and repeated `migrate()` calls retain the cache table.

- [ ] **Step 5: Record the unstaged checkpoint**

```bash
git diff --check
git status --short
```

Expected: `src/db.rs` joins the Task 1 paths. Do not stage or commit.

## Task 3: Implement pure catalog identity and release rules

**Files:**

- Modify: `src/discography/mod.rs`

- [ ] **Step 1: Write RED domain-rule tests**

Add table-driven tests using this helper:

```rust
fn group(
    id: &str,
    title: &str,
    date: Option<&str>,
    primary: Option<&str>,
    secondary: &[&str],
) -> ReleaseGroup {
    ReleaseGroup {
        id: id.into(),
        title: title.into(),
        first_release_date: date.map(str::to_owned),
        primary_type: primary.map(str::to_owned),
        secondary_types: secondary.iter().map(|value| (*value).to_owned()).collect(),
    }
}
```

Required tests:

```rust
#[test]
fn exact_artist_resolution_uses_only_unique_canonical_name() {
    let candidates = vec![
        ArtistCandidate { id: "1".into(), name: "ＡC/DC".into() },
        ArtistCandidate { id: "2".into(), name: "AC DC".into() },
    ];
    assert_eq!(resolve_exact_artist("ac/dc", &candidates).unwrap().id, "1");
    assert!(resolve_exact_artist("AC DC", &[
        ArtistCandidate { id: "2".into(), name: "AC DC".into() },
        ArtistCandidate { id: "3".into(), name: " ac dc ".into() },
    ]).is_err());
    assert!(resolve_exact_artist("Missing", &candidates).is_err());
}

#[test]
fn release_classification_is_conservative() {
    let studio = group("1", "Studio", Some("2000"), Some("Album"), &[]);
    let live = group("2", "Live", Some("2001-02"), Some("Album"), &["Live"]);
    let live_compilation = group(
        "3",
        "Live Collection",
        Some("2002-03-04"),
        Some("Album"),
        &["Live", "Compilation"],
    );
    assert!(release_allowed(&studio, &[DiscographyReleaseType::StudioAlbum]));
    assert!(!release_allowed(&live, &[DiscographyReleaseType::StudioAlbum]));
    assert!(release_allowed(&live, &[DiscographyReleaseType::LiveAlbum]));
    assert!(!release_allowed(
        &live_compilation,
        &[DiscographyReleaseType::LiveAlbum]
    ));
    assert!(release_allowed(
        &live_compilation,
        &[
            DiscographyReleaseType::LiveAlbum,
            DiscographyReleaseType::Compilation,
        ]
    ));
}

#[test]
fn albums_are_deduplicated_and_ordered_by_partial_date() {
    let groups = vec![
        group("b", "Same", Some("2001"), Some("Album"), &[]),
        group("a", " same ", Some("1999-12"), Some("Album"), &[]),
        group("c", "Later", Some("2005-01-02"), Some("Album"), &[]),
        group("d", "Undated", Some("not-a-date"), Some("Album"), &[]),
    ];
    let albums = select_albums(&groups, &[DiscographyReleaseType::StudioAlbum]);
    assert_eq!(
        albums.iter().map(|album| album.title.as_str()).collect::<Vec<_>>(),
        vec!["same", "Later", "Undated"]
    );
    assert_eq!(albums[0].release_group_id, "a");
}
```

Add these complete coverage tests:

```rust
#[test]
fn every_friendly_release_category_maps() {
    let cases = [
        ("Album", Vec::<&str>::new(), DiscographyReleaseType::StudioAlbum),
        ("Album", vec!["Live"], DiscographyReleaseType::LiveAlbum),
        ("EP", Vec::<&str>::new(), DiscographyReleaseType::Ep),
        ("Single", Vec::<&str>::new(), DiscographyReleaseType::Single),
        ("Album", vec!["Compilation"], DiscographyReleaseType::Compilation),
        ("Album", vec!["Remix"], DiscographyReleaseType::Remix),
        ("Album", vec!["Soundtrack"], DiscographyReleaseType::Soundtrack),
        ("Album", vec!["DJ-mix"], DiscographyReleaseType::DjMix),
        ("Album", vec!["Mixtape/Street"], DiscographyReleaseType::Mixtape),
    ];
    for (primary, secondary, allowed) in cases {
        let release = group("id", "title", None, Some(primary), &secondary);
        assert!(release_allowed(&release, &[allowed]), "failed for {allowed:?}");
    }
}

#[test]
fn partial_dates_reject_impossible_values() {
    assert_eq!(parse_partial_date("2000"), Some(PartialDate { year: 2000, month: 0, day: 0 }));
    assert_eq!(parse_partial_date("2000-02"), Some(PartialDate { year: 2000, month: 2, day: 0 }));
    assert_eq!(parse_partial_date("2000-02-29"), Some(PartialDate { year: 2000, month: 2, day: 29 }));
    assert_eq!(parse_partial_date("2001-02-29"), None);
    assert_eq!(parse_partial_date("2000-13"), None);
    assert_eq!(parse_partial_date("2000-01-32"), None);
    assert_eq!(parse_partial_date("not-a-date"), None);
}
```

- [ ] **Step 2: Run the RED domain tests**

```bash
cargo test -p seakarr discography::tests::exact_artist_resolution_uses_only_unique_canonical_name -- --nocapture
cargo test -p seakarr discography::tests::release_classification_is_conservative -- --nocapture
cargo test -p seakarr discography::tests::albums_are_deduplicated_and_ordered_by_partial_date -- --nocapture
```

Expected: compilation fails because the domain types and selection functions do not exist.

- [ ] **Step 3: Add domain types, errors, and provider trait**

Add imports and exact public contracts:

```rust
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::config::DiscographyReleaseType;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtistCandidate {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseGroup {
    pub id: String,
    pub title: String,
    pub first_release_date: Option<String>,
    pub primary_type: Option<String>,
    pub secondary_types: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlbumTarget {
    pub release_group_id: String,
    pub title: String,
}

#[derive(Debug, Error)]
pub enum DiscographyError {
    #[error("MusicBrainz transport error: {0}")]
    Transport(String),
    #[error("MusicBrainz returned HTTP {0}")]
    HttpStatus(u16),
    #[error("MusicBrainz returned an invalid Retry-After header")]
    InvalidRetryAfter,
    #[error("MusicBrainz response exceeded 4 MiB")]
    ResponseTooLarge,
    #[error("MusicBrainz response was invalid: {0}")]
    Decode(String),
    #[error("MusicBrainz pagination was incomplete: {0}")]
    IncompletePagination(String),
    #[error("artist could not be resolved safely: {0}")]
    ArtistUnresolved(String),
}

#[async_trait]
pub trait DiscographyProvider: Send + Sync {
    async fn search_artists(
        &self,
        artist: &str,
    ) -> std::result::Result<Vec<ArtistCandidate>, DiscographyError>;

    async fn artist_by_id(
        &self,
        artist_mbid: &str,
    ) -> std::result::Result<ArtistCandidate, DiscographyError>;

    async fn release_groups(
        &self,
        artist_mbid: &str,
    ) -> std::result::Result<Vec<ReleaseGroup>, DiscographyError>;
}
```

- [ ] **Step 4: Add exact matching, type mapping, dates, and selection**

Implement `resolve_exact_artist` by filtering candidates whose canonical `name` key equals the requested key. Return the sole match; return `ArtistUnresolved` naming zero or the exact duplicate count otherwise.

Use this mapping contract:

```rust
fn secondary_category(value: &str) -> Option<DiscographyReleaseType> {
    match value.trim().to_ascii_lowercase().as_str() {
        "live" => Some(DiscographyReleaseType::LiveAlbum),
        "compilation" => Some(DiscographyReleaseType::Compilation),
        "remix" => Some(DiscographyReleaseType::Remix),
        "soundtrack" => Some(DiscographyReleaseType::Soundtrack),
        "dj-mix" => Some(DiscographyReleaseType::DjMix),
        "mixtape/street" => Some(DiscographyReleaseType::Mixtape),
        _ => None,
    }
}
```

`release_allowed` must apply these rules in order and emit a debug log naming the release-group ID/title and rejection reason for every rejected group. `select_albums` emits the same contextual debug log for an empty title:

1. reject an absent/unknown primary type;
2. reject every absent/unknown secondary mapping;
3. primary `Album` with no secondary types requires `StudioAlbum`;
4. primary `Album` with secondary types requires every mapped secondary type and does not require `StudioAlbum`;
5. primary `EP` requires `Ep` plus every mapped secondary type;
6. primary `Single` requires `Single` plus every mapped secondary type;
7. every other primary type is rejected.

Parse dates into this sortable private type:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct PartialDate {
    year: i32,
    month: u32,
    day: u32,
}
```

Accept `YYYY`, `YYYY-MM`, and `YYYY-MM-DD`. Require year `1..=9999`, month `1..=12` when present, and validate complete dates with `chrono::NaiveDate::from_ymd_opt`. Missing month/day sort as zero. Any other shape is `None`.

`select_albums` must filter allowed non-empty titles, deduplicate by `normalize_catalog_key(title)`, prefer dated over undated duplicates, then earlier date, then lexicographically smaller MBID. Sort dated targets by `PartialDate`, normalized title, MBID; append undated targets sorted by normalized title, MBID. Preserve the trimmed canonical title of the selected group.

- [ ] **Step 5: Run GREEN pure tests**

```bash
cargo test -p seakarr discography::tests -- --nocapture
```

Expected: all exact matching, category, malformed-date, deduplication, and ordering tests pass without network access.

- [ ] **Step 6: Record the unstaged checkpoint**

```bash
cargo fmt --all
git diff --check
git status --short
```

Do not stage or commit.

## Task 4: Implement the bounded MusicBrainz provider

**Files:**

- Create: `src/discography/musicbrainz.rs`
- Modify: `src/discography/mod.rs`

- [ ] **Step 1: Resolve current reqwest documentation**

Use Context7 to resolve `reqwest` and confirm `Client::builder().timeout`, `RequestBuilder::query`, response headers, `Response::chunk`, and JSON deserialization behavior for version 0.12. Do not add another HTTP or rate-limit dependency.

- [ ] **Step 2: Write RED wiremock tests**

Add these imports and the complete page fixture before the tests:

```rust
use serde_json::json;
use wiremock::matchers::{header_regex, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn release_page(count: usize, offset: usize, length: usize) -> serde_json::Value {
    let groups: Vec<serde_json::Value> = (0..length)
        .map(|index| {
            json!({
                "id": format!("group-{}", offset + index),
                "title": format!("Album {}", offset + index),
                "first-release-date": "2000",
                "primary-type": "Album",
                "secondary-types": []
            })
        })
        .collect();
    json!({
        "release-group-count": count,
        "release-group-offset": offset,
        "release-groups": groups
    })
}
```

Then add:

```rust
#[tokio::test]
async fn artist_search_sends_identity_headers_and_encoded_query() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/ws/2/artist"))
        .and(query_param("query", "artist:\"AC/DC\""))
        .and(query_param("fmt", "json"))
        .and(query_param("limit", "100"))
        .and(header_regex("user-agent", r"^seakarr/[0-9]+\.[0-9]+\.[0-9]+ \(https://github.com/binhex/seakarr\)$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "count": 1,
            "offset": 0,
            "artists": [{"id": "11111111-1111-1111-1111-111111111111", "name": "AC/DC"}]
        })))
        .mount(&server)
        .await;

    let provider = MusicBrainzProvider::for_test(server.uri(), Duration::ZERO).unwrap();
    let artists = provider.search_artists("AC/DC").await.unwrap();
    assert_eq!(artists[0].name, "AC/DC");
}

#[tokio::test]
async fn release_groups_paginate_with_website_default_status() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/ws/2/release-group"))
        .and(query_param("artist", "11111111-1111-1111-1111-111111111111"))
        .and(query_param("release-group-status", "website-default"))
        .and(query_param("offset", "0"))
        .respond_with(ResponseTemplate::new(200).set_body_json(release_page(101, 0, 100)))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/ws/2/release-group"))
        .and(query_param("offset", "100"))
        .respond_with(ResponseTemplate::new(200).set_body_json(release_page(101, 100, 1)))
        .expect(1)
        .mount(&server)
        .await;

    let provider = MusicBrainzProvider::for_test(server.uri(), Duration::ZERO).unwrap();
    let groups = provider
        .release_groups("11111111-1111-1111-1111-111111111111")
        .await
        .unwrap();
    assert_eq!(groups.len(), 101);
}
```

Add this exact provider test matrix. Tests involving time use `#[tokio::test(start_paused = true)]` and `tokio::time::advance`; all other tests pass `Duration::ZERO` to `for_test`.

| Test | Fixture | Required assertion |
| --- | --- | --- |
| `requests_are_spaced_one_second_apart` | Two successful endpoints with `REQUEST_INTERVAL` | The second mock has zero hits before a one-second advance and one hit afterwards. |
| `provider_instances_share_a_request_gate` | Two providers built with one shared gate against wiremock | A request from provider two waits for provider one's one-second slot. |
| `retry_after_is_honored` | 429 with `Retry-After: 1`, then 200 | Two requests; no second hit before advancing one second. |
| `invalid_retry_after_values_are_rejected` | Missing, nonnumeric, HTTP-date, and value 31 | `InvalidRetryAfter` after one request each. |
| `exhausted_retry_after_returns_429` | Three 429 responses with delay zero | `HttpStatus(429)` after exactly three requests. |
| `server_error_retries_then_succeeds` | 500, then 200 | Two requests and successful decoded value. |
| `exhausted_server_errors_return_last_status` | Three 503 responses | `HttpStatus(503)` after exactly three attempts. |
| `artist_search_over_one_page_is_unresolved` | Count 101 with 100 payload entries | `ArtistUnresolved`, locking the documented one-page bound. |
| `configured_id_fetches_canonical_artist` | `/ws/2/artist/{mbid}?fmt=json` returns one artist | Returned ID/name populate the canonical cache identity without name search. |
| `not_found_is_not_retried` | Persistent 404 | `HttpStatus(404)` and exactly one request. |
| `malformed_json_is_rejected` | 200 with invalid JSON | `Decode` and no retry. |
| `oversized_chunked_body_is_rejected` | Chunked body of `MAX_RESPONSE_BYTES + 1` | `ResponseTooLarge`. |
| `changing_page_count_is_incomplete` | Page counts 101 then 102 | `IncompletePagination`. |
| `duplicate_release_group_ids_are_incomplete` | Final page repeats an ID from page one | `IncompletePagination`. |
| `wrong_page_offset_is_incomplete` | Second page reports offset 0 | `IncompletePagination`. |
| `empty_intermediate_page_is_incomplete` | Count 101, first page empty | `IncompletePagination`. |
| `short_release_group_page_is_incomplete` | Count 101 with one item on the first page | `IncompletePagination` after exactly one request. |
| `artist_search_count_must_match_payload` | Artist count differs from payload length | `ArtistUnresolved`. |
| `excessive_release_count_is_incomplete` | Count 10,001 | `IncompletePagination` before page allocation. |
| `transport_exhaustion_is_bounded` | Unused loopback port | `Transport` after exactly three attempts and advances of one then two seconds. |

- [ ] **Step 3: Run the provider RED tests**

```bash
cargo test -p seakarr discography::musicbrainz::tests::artist_search_sends_identity_headers_and_encoded_query -- --nocapture
```

Expected: compilation fails because `MusicBrainzProvider` and wire models do not exist.

- [ ] **Step 4: Add provider constants and wire models**

Create `musicbrainz.rs` with:

```rust
const MUSICBRAINZ_BASE_URL: &str = "https://musicbrainz.org";
const REQUEST_INTERVAL: Duration = Duration::from_secs(1);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_RETRY_AFTER: u64 = 30;
const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const PAGE_SIZE: usize = 100;
const MAX_PAGES: usize = 100;
const MAX_RELEASE_GROUPS: usize = PAGE_SIZE * MAX_PAGES;

#[derive(Debug, Deserialize)]
struct ArtistPage {
    count: usize,
    offset: usize,
    artists: Vec<ArtistWire>,
}

#[derive(Debug, Deserialize)]
struct ArtistWire {
    id: String,
    name: String,
}

#[derive(Debug, Deserialize)]
struct ReleaseGroupPage {
    #[serde(rename = "release-group-count")]
    count: usize,
    #[serde(rename = "release-group-offset")]
    offset: usize,
    #[serde(rename = "release-groups")]
    release_groups: Vec<ReleaseGroupWire>,
}

#[derive(Debug, Deserialize)]
struct ReleaseGroupWire {
    id: String,
    title: String,
    #[serde(rename = "first-release-date")]
    first_release_date: Option<String>,
    #[serde(rename = "primary-type")]
    primary_type: Option<String>,
    #[serde(rename = "secondary-types", default)]
    secondary_types: Vec<String>,
}
```

- [ ] **Step 5: Implement bounded request execution**

Define:

```rust
pub struct MusicBrainzProvider {
    client: reqwest::Client,
    base_url: String,
    request_interval: Duration,
    next_request: Arc<tokio::sync::Mutex<tokio::time::Instant>>,
}
```

Use these constructor signatures:

```rust
impl MusicBrainzProvider {
    pub fn new() -> std::result::Result<Self, DiscographyError> {
        Self::build(
            MUSICBRAINZ_BASE_URL.to_string(),
            REQUEST_INTERVAL,
            production_request_gate(),
        )
    }

    pub(crate) fn for_test(
        base_url: String,
        request_interval: Duration,
    ) -> std::result::Result<Self, DiscographyError> {
        Self::build(
            base_url,
            request_interval,
            Arc::new(tokio::sync::Mutex::new(tokio::time::Instant::now())),
        )
    }

    fn build(
        base_url: String,
        request_interval: Duration,
        next_request: Arc<tokio::sync::Mutex<tokio::time::Instant>>,
    ) -> std::result::Result<Self, DiscographyError> {
        let user_agent = format!(
            "seakarr/{} (https://github.com/binhex/seakarr)",
            env!("CARGO_PKG_VERSION")
        );
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .user_agent(user_agent)
            .build()
            .map_err(|error| DiscographyError::Transport(error.to_string()))?;
        Ok(Self {
            client,
            base_url,
            request_interval,
            next_request,
        })
    }
}
```

Import `std::sync::{Arc, OnceLock}` and add a process-wide production gate:

```rust
fn production_request_gate() -> Arc<tokio::sync::Mutex<tokio::time::Instant>> {
    static GATE: OnceLock<Arc<tokio::sync::Mutex<tokio::time::Instant>>> = OnceLock::new();
    Arc::clone(GATE.get_or_init(|| {
        Arc::new(tokio::sync::Mutex::new(tokio::time::Instant::now()))
    }))
}
```

Implement `pace()` by holding `next_request`, sleeping until it when necessary, then assigning `Instant::now() + self.request_interval`. Ordinary wiremock tests pass `Duration::ZERO`; paused-time pacing tests call private `build` with one shared `Arc<Mutex<Instant>>`, advance one second, and prove requests from either instance cannot start early. Implement `read_limited` with repeated `response.chunk().await`, checking `total.checked_add(chunk.len())` before extending a `Vec<u8>` and returning `ResponseTooLarge` above 4 MiB.

Use this request boundary:

```rust
async fn request_json<T: serde::de::DeserializeOwned>(
    &self,
    path: &str,
    query: &[(&str, String)],
) -> std::result::Result<T, DiscographyError>
```

It performs three attempts. Each attempt calls `pace` before `send`. Transport and 5xx failures sleep 1 second after attempt one and 2 seconds after attempt two. A 429 parses integer delay-seconds in `Retry-After`, accepts values from 0 through 30, rejects HTTP-date/malformed/larger values, and sleeps the accepted value before retry. Rejection is deliberate provider unavailability, preventing an external response from imposing an unbounded wait. Other 4xx statuses return `HttpStatus` immediately. Successful bodies pass through `read_limited` and `serde_json::from_slice`.

- [ ] **Step 6: Implement artist search and release pagination**

Artist search requests `/ws/2/artist` with a `query` value assembled as `artist:"` plus the escaped artist plus `"`, together with `fmt=json` and `limit=100`. Escape backslash as `\\` and quote as `\"` before building the Lucene phrase. Require response offset zero and require `count == artists.len()`; either truncation or excess payload data returns `ArtistUnresolved`. A reported count above 100 therefore favors safe visible fallback over guessing. Map only `id` and canonical `name`; do not model score, sort name, or aliases.

`artist_by_id` requests `/ws/2/artist/{mbid}` with `fmt=json`, decodes one `ArtistWire`, and requires the returned ID to equal the requested MBID ignoring ASCII case. A mismatch is `Decode` rather than a silently changed identity.

Release-group requests use `/ws/2/release-group` with `artist`, `fmt=json`, `limit=100`, current `offset`, and `release-group-status=website-default`. Require every page count to equal the first count, every returned offset to equal the requested offset, every payload length to equal `min(100, count - offset)`, and every release-group ID to be unique across the completed response. Reject counts over 10,000, short/empty pages, duplicate IDs, non-advancing offsets, or more than 100 pages. Return only after collected length equals count exactly.

Export the provider from `mod.rs`:

```rust
mod musicbrainz;
pub use musicbrainz::MusicBrainzProvider;
```

- [ ] **Step 7: Run GREEN provider tests**

```bash
cargo test -p seakarr discography::musicbrainz::tests -- --nocapture
```

Expected: all wiremock tests pass with simulated time and no request reaches the live MusicBrainz service.

- [ ] **Step 8: Record the unstaged checkpoint**

```bash
cargo fmt --all
git diff --check
git status --short
```

Do not stage or commit.

## Task 5: Implement cache precedence and discovery outcomes

**Files:**

- Modify: `src/discography/mod.rs`
- Modify: `src/db.rs`

- [ ] **Step 1: Write RED service tests with a fake provider**

Create this test fake and cache helper in the existing `discography` test module:

```rust
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

#[derive(Default)]
struct FakeProvider {
    artist_responses: Mutex<VecDeque<std::result::Result<Vec<ArtistCandidate>, DiscographyError>>>,
    group_responses: Mutex<VecDeque<std::result::Result<Vec<ReleaseGroup>, DiscographyError>>>,
    artist_calls: AtomicUsize,
    lookup_calls: AtomicUsize,
    group_calls: AtomicUsize,
    failure: Option<String>,
}

impl FakeProvider {
    fn artists(artists: Vec<ArtistCandidate>) -> Self {
        Self {
            artist_responses: Mutex::new(VecDeque::from([Ok(artists)])),
            ..Self::default()
        }
    }

    fn with_groups(groups: Vec<ReleaseGroup>) -> Self {
        Self {
            artist_responses: Mutex::new(VecDeque::from([Ok(vec![ArtistCandidate {
                id: "11111111-1111-1111-1111-111111111111".to_string(),
                name: "Artist".to_string(),
            }])])),
            group_responses: Mutex::new(VecDeque::from([Ok(groups)])),
            ..Self::default()
        }
    }

    fn failing(reason: &str) -> Self {
        Self {
            failure: Some(reason.to_string()),
            ..Self::default()
        }
    }

    fn total_calls(&self) -> usize {
        self.artist_calls.load(Ordering::SeqCst)
            + self.lookup_calls.load(Ordering::SeqCst)
            + self.group_calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl DiscographyProvider for FakeProvider {
    async fn search_artists(
        &self,
        _artist: &str,
    ) -> std::result::Result<Vec<ArtistCandidate>, DiscographyError> {
        self.artist_calls.fetch_add(1, Ordering::SeqCst);
        if let Some(reason) = &self.failure {
            return Err(DiscographyError::Transport(reason.clone()));
        }
        self.artist_responses
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Ok(Vec::new()))
    }

    async fn artist_by_id(
        &self,
        artist_mbid: &str,
    ) -> std::result::Result<ArtistCandidate, DiscographyError> {
        self.lookup_calls.fetch_add(1, Ordering::SeqCst);
        if let Some(reason) = &self.failure {
            return Err(DiscographyError::Transport(reason.clone()));
        }
        Ok(ArtistCandidate {
            id: artist_mbid.to_string(),
            name: "Artist".to_string(),
        })
    }

    async fn release_groups(
        &self,
        _artist_mbid: &str,
    ) -> std::result::Result<Vec<ReleaseGroup>, DiscographyError> {
        self.group_calls.fetch_add(1, Ordering::SeqCst);
        if let Some(reason) = &self.failure {
            return Err(DiscographyError::Transport(reason.clone()));
        }
        self.group_responses
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Ok(Vec::new()))
    }
}

fn cache_groups(
    db: &Database,
    artist_key: &str,
    mbid: &str,
    fetched_at: i64,
    groups: &[ReleaseGroup],
) {
    db.upsert_discography_cache(&DiscographyCacheEntry {
        artist_key: artist_key.to_string(),
        artist_mbid: mbid.to_string(),
        canonical_artist: "Artist".to_string(),
        fetched_at,
        release_groups_json: serde_json::to_string(groups).unwrap(),
    })
    .unwrap();
}
```

Then add focused tests including:

```rust
#[tokio::test]
async fn fresh_cache_avoids_provider_and_refilters_current_types() {
    let db = Database::open_in_memory().unwrap();
    let groups = vec![
        group("1", "Studio", Some("2000"), Some("Album"), &[]),
        group("2", "Live", Some("2001"), Some("Album"), &["Live"]),
    ];
    cache_groups(&db, "artist", "11111111-1111-1111-1111-111111111111", 1_000, &groups);
    let provider = FakeProvider::default();
    let mut config = DiscographyConfig::default();
    config.allowed_types = vec![DiscographyReleaseType::LiveAlbum];

    let outcome = discover_artist_albums_at(&provider, &db, "Artist", &config, 1_100).await;

    assert!(matches!(outcome, DiscoveryOutcome::Authoritative {
        provenance: DiscoveryProvenance::FreshCache,
        ref albums,
    } if albums.iter().map(|album| album.title.as_str()).collect::<Vec<_>>() == ["Live"]));
    assert_eq!(provider.total_calls(), 0);
}

#[tokio::test]
async fn failed_refresh_prefers_compatible_stale_cache() {
    let db = Database::open_in_memory().unwrap();
    let groups = vec![group("1", "Studio", Some("2000"), Some("Album"), &[])];
    cache_groups(&db, "artist", "11111111-1111-1111-1111-111111111111", 0, &groups);
    let provider = FakeProvider::failing("service unavailable");
    let config = DiscographyConfig::default();

    let outcome = discover_artist_albums_at(&provider, &db, "Artist", &config, 31 * 86_400).await;

    assert!(matches!(outcome, DiscoveryOutcome::Authoritative {
        provenance: DiscoveryProvenance::StaleCache { .. },
        ..
    }));
}

#[tokio::test]
async fn unresolved_without_cache_returns_visible_legacy_reason() {
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
        DiscoveryOutcome::LegacyFallback { reason }
            if reason.contains("artist could not be resolved safely")
    ));
}
```

Add this exact service test matrix:

| Test | Setup | Required assertion |
| --- | --- | --- |
| `zero_cache_days_always_refreshes` | Compatible row at `now`, `cache_days = 0`, successful fake refresh | Provider called and provenance is `Refreshed`. |
| `cache_is_stale_at_thirty_day_boundary` | Row age exactly `30 * 86_400` | Provider called rather than returning `FreshCache`. |
| `future_cache_timestamp_is_stale` | `fetched_at = now + 1` and provider failure | `StaleCache` with age zero, never `FreshCache`. |
| `configured_mbid_invalidates_other_cache_identity` | Cache MBID A, configured MBID B | Provider receives B; cache A is not stale fallback. |
| `corrupt_cache_is_deleted_then_refreshed` | Invalid JSON row and successful provider | `Refreshed` and replacement row decodes. |
| `complete_empty_provider_result_is_authoritative_empty` | Unique artist and zero release groups | `AuthoritativeEmpty { provenance: Refreshed }`, never legacy. |
| `cache_write_failure_does_not_discard_refresh` | Drop cache table before successful provider calls | `Authoritative` with `Refreshed` provenance. |
| `oversized_cache_payload_uses_stale_or_legacy` | Generated serialized groups exceed 16 MiB | Compatible stale cache wins; without stale cache result is `LegacyFallback`. |

- [ ] **Step 2: Run the service RED tests**

```bash
cargo test -p seakarr discography::tests::fresh_cache_avoids_provider_and_refilters_current_types -- --nocapture
```

Expected: compilation fails because outcomes and the discovery service do not exist.

- [ ] **Step 3: Add outcome contracts**

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscoveryProvenance {
    FreshCache,
    Refreshed,
    StaleCache { age_days: u64, refresh_error: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscoveryOutcome {
    Authoritative {
        albums: Vec<AlbumTarget>,
        provenance: DiscoveryProvenance,
    },
    AuthoritativeEmpty {
        provenance: DiscoveryProvenance,
    },
    LegacyFallback { reason: String },
}
```

Add constants `SECONDS_PER_DAY: u64 = 86_400` and `MAX_CACHE_PAYLOAD_BYTES: usize = 16 * 1024 * 1024`.

- [ ] **Step 4: Implement cache parsing and freshness**

A compatible cache row has the normalized requested key and, when an override exists, the same MBID ignoring ASCII case. Deserialize `release_groups_json` into `Vec<ReleaseGroup>`. On decode failure, warn, call `delete_discography_cache`, and continue without cache. Retention is deliberately append-only by normalized artist (one bounded row each); refresh replaces that artist's row and no global eviction task is added.

Freshness uses checked arithmetic. A row is fresh only when `now >= fetched_at`, `age_seconds < cache_days * 86_400`, and `cache_days > 0`. Equality is stale. Future timestamps are stale. Compute stale `age_days` with non-negative saturating conversion for warning output.

- [ ] **Step 5: Implement refresh and fallback precedence**

Add:

```rust
pub async fn discover_artist_albums(
    provider: &dyn DiscographyProvider,
    db: &Database,
    artist: &str,
    config: &DiscographyConfig,
) -> DiscoveryOutcome {
    discover_artist_albums_at(provider, db, artist, config, chrono::Utc::now().timestamp()).await
}
```

The `pub(crate)` `_at` variant executes:

1. normalize the requested artist;
2. read and validate compatible cache;
3. return a fresh cache after applying `select_albums`;
4. for a configured MBID, call `artist_by_id` to verify it and obtain the canonical name; otherwise call `search_artists` plus `resolve_exact_artist`;
5. call `release_groups` with the resolved ID;
6. serialize the complete unfiltered vector and reject payloads over 16 MiB;
7. attempt atomic cache upsert, warning but continuing on write failure;
8. return `AuthoritativeEmpty { provenance }` when `select_albums` is empty, preserving fresh, refreshed, or stale provenance;
9. on any refresh error, return compatible stale cache when present, again converting a filtered-empty stale list to `AuthoritativeEmpty { provenance: StaleCache { age_days, refresh_error } }`;
10. otherwise return `LegacyFallback` with the exact typed error text.

A fresh cache filtered to zero albums returns `AuthoritativeEmpty { provenance: FreshCache }`; it never refreshes or falls back merely because current type configuration excludes every group.

- [ ] **Step 6: Run GREEN service and database tests**

```bash
cargo test -p seakarr discography::tests -- --nocapture
cargo test -p seakarr db::tests -- --nocapture
```

Expected: every cache precedence, corruption, invalidation, authoritative-empty, and raw re-filtering test passes.

- [ ] **Step 7: Record the unstaged checkpoint**

```bash
cargo fmt --all
git diff --check
git status --short
```

Do not stage or commit.

## Task 6: Add run-summary notices

**Files:**

- Modify: `src/report.rs`

- [ ] **Step 1: Write RED notice tests**

```rust
#[test]
fn notices_are_retained_even_without_album_outcomes() {
    let mut report = RunReport::new();
    report.add_notice("Authoritative discovery unavailable; used legacy album discovery");
    assert_eq!(report.notice_count(), 1);
    assert_eq!(
        report.notices(),
        ["Authoritative discovery unavailable; used legacy album discovery"]
    );
}
```

Add a second test against a private rendered-lines seam:

```rust
#[test]
fn notice_only_summary_is_rendered() {
    let mut report = RunReport::new();
    report.add_notice("Legacy discovery was used");
    assert_eq!(
        report.summary_lines(),
        vec![
            "=== Run summary ===".to_string(),
            "Notices (1):".to_string(),
            "  Legacy discovery was used".to_string(),
        ]
    );
}
```

- [ ] **Step 2: Run the RED report test**

```bash
cargo test -p seakarr report::tests::notices_are_retained_even_without_album_outcomes -- --nocapture
```

Expected: compilation fails because notice methods do not exist.

- [ ] **Step 3: Add notice storage and output**

Add `notices: Vec<String>` to `RunReport`, then:

```rust
pub fn add_notice(&mut self, notice: impl Into<String>) {
    self.notices.push(notice.into());
}

pub fn notice_count(&self) -> usize {
    self.notices.len()
}

pub fn notices(&self) -> &[String] {
    &self.notices
}
```

Move summary formatting into this private deterministic helper and keep `print_summary` as the tracing boundary:

```rust
fn summary_lines(&self) -> Vec<String> {
    if self.downloaded.is_empty()
        && self.skipped.is_empty()
        && self.failed.is_empty()
        && self.notices.is_empty()
    {
        return Vec::new();
    }
    let mut lines = vec!["=== Run summary ===".to_string()];
    if !self.notices.is_empty() {
        lines.push(format!("Notices ({}):", self.notices.len()));
        lines.extend(self.notices.iter().map(|notice| format!("  {notice}")));
    }
    if !self.downloaded.is_empty() {
        lines.push(format!("Downloaded ({}):", self.downloaded.len()));
        lines.extend(self.downloaded.iter().map(|(artist, album, count)| {
            format!("  {artist} — {album} ({count} tracks)")
        }));
    }
    if !self.skipped.is_empty() {
        lines.push(format!("Skipped ({}):", self.skipped.len()));
        lines.extend(
            self.skipped
                .iter()
                .map(|(artist, album)| format!("  {artist} — {album}")),
        );
    }
    if !self.failed.is_empty() {
        lines.push(format!("Failed ({}):", self.failed.len()));
        lines.extend(self.failed.iter().map(|(artist, album, reason)| {
            format!("  {artist} — {album} ({reason})")
        }));
    }
    lines
}

pub fn print_summary(&self) {
    for line in self.summary_lines() {
        tracing::info!("{line}");
    }
}
```

- [ ] **Step 4: Run GREEN report tests**

```bash
cargo test -p seakarr report::tests -- --nocapture
```

Expected: notices print even when no album outcome exists, and all existing outcome counts remain unchanged.

- [ ] **Step 5: Record the unstaged checkpoint**

```bash
git diff --check
git status --short
```

Do not stage or commit.

## Task 7: Replace normal artist-only discovery in the runner

**Files:**

- Modify: `src/runner.rs`

- [ ] **Step 1: Write RED authoritative runner tests**

Add these helpers to the existing runner test module:

```rust
use async_trait::async_trait;
use crate::client::DownloadHandle;
use crate::discography::{ArtistCandidate, DiscographyError, DiscographyProvider, ReleaseGroup};
use std::sync::atomic::{AtomicUsize, Ordering};

struct FakeDiscographyProvider {
    groups: Vec<ReleaseGroup>,
    failure: Option<String>,
}

impl FakeDiscographyProvider {
    fn with_groups(groups: Vec<ReleaseGroup>) -> Self {
        Self {
            groups,
            failure: None,
        }
    }

    fn failing(reason: &str) -> Self {
        Self {
            groups: Vec::new(),
            failure: Some(reason.to_string()),
        }
    }
}

#[async_trait]
impl DiscographyProvider for FakeDiscographyProvider {
    async fn search_artists(
        &self,
        artist: &str,
    ) -> std::result::Result<Vec<ArtistCandidate>, DiscographyError> {
        if let Some(reason) = &self.failure {
            return Err(DiscographyError::Transport(reason.clone()));
        }
        Ok(vec![ArtistCandidate {
            id: "11111111-1111-1111-1111-111111111111".to_string(),
            name: artist.to_string(),
        }])
    }

    async fn artist_by_id(
        &self,
        artist_mbid: &str,
    ) -> std::result::Result<ArtistCandidate, DiscographyError> {
        if let Some(reason) = &self.failure {
            return Err(DiscographyError::Transport(reason.clone()));
        }
        Ok(ArtistCandidate {
            id: artist_mbid.to_string(),
            name: "Test Artist".to_string(),
        })
    }

    async fn release_groups(
        &self,
        _artist_mbid: &str,
    ) -> std::result::Result<Vec<ReleaseGroup>, DiscographyError> {
        if let Some(reason) = &self.failure {
            return Err(DiscographyError::Transport(reason.clone()));
        }
        Ok(self.groups.clone())
    }
}

fn release_group(id: &str, title: &str, date: &str) -> ReleaseGroup {
    ReleaseGroup {
        id: id.to_string(),
        title: title.to_string(),
        first_release_date: Some(date.to_string()),
        primary_type: Some("Album".to_string()),
        secondary_types: Vec::new(),
    }
}

fn album_result(artist: &str, album: &str) -> SearchResult {
    SearchResult {
        username: "peer".to_string(),
        speed: 500,
        slots: 1,
        files: vec![make_file(
            &format!(r"{artist}\{album}\01 - track.flac"),
            900,
            10_000_000,
        )],
    }
}

fn artist_only_fixture() -> (Config, Database, TempDir) {
    let staging = TempDir::new().unwrap();
    let mut config = make_test_config();
    config.storage.staging_dir = staging.path().to_string_lossy().into_owned();
    config.filters.min_tracks = 1;
    config.filters.contiguous_tracks = true;
    config.download.min_upload_speed_kbps = 0;
    (config, Database::open_in_memory().unwrap(), staging)
}

struct CancelAfterFirstSearchClient {
    inner: MockClient,
    cancel: Arc<AtomicBool>,
    searches: AtomicUsize,
}

#[async_trait]
impl SoulseekClient for CancelAfterFirstSearchClient {
    async fn login(
        &self,
        username: &str,
        password: &str,
        server: &str,
        port: u16,
    ) -> Result<()> {
        self.inner.login(username, password, server, port).await
    }

    async fn search(&self, query: &str, timeout_secs: u64) -> Result<Vec<SearchResult>> {
        let result = self.inner.search(query, timeout_secs).await;
        if self.searches.fetch_add(1, Ordering::SeqCst) == 0 {
            self.cancel.store(true, Ordering::SeqCst);
        }
        result
    }

    async fn download(
        &self,
        file: &FileInfo,
        username: &str,
        dir: &Path,
    ) -> Result<DownloadHandle> {
        self.inner.download(file, username, dir).await
    }
}
```

Then add:

```rust
#[tokio::test]
async fn artist_only_authoritative_discovery_searches_each_album_oldest_first() {
    let soulseek = MockClient::new();
    soulseek.search_results_by_query.lock().unwrap().insert(
        "Test Artist Older".into(),
        vec![album_result("Test Artist", "Older")],
    );
    soulseek.search_results_by_query.lock().unwrap().insert(
        "Test Artist Newer".into(),
        vec![album_result("Test Artist", "Newer")],
    );
    let provider = FakeDiscographyProvider::with_groups(vec![
        release_group("new", "Newer", "2005"),
        release_group("old", "Older", "1999"),
    ]);
    let (config, db, staging) = artist_only_fixture();

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
        ["Test Artist Older", "Test Artist Newer"]
    );
    assert_eq!(run.outcomes.len(), 2);
    assert!(run.notice.is_none());
}
```

Add this exact runner test matrix:

| Test | Setup | Required assertion |
| --- | --- | --- |
| `authoritative_processed_album_skips_before_search` | Mark older album successful before a two-album run | Query list contains only the newer album; first outcome is `Skipped`. |
| `authoritative_album_failure_continues` | Older album returns no results, newer album returns one valid result | Both primary album queries occur in chronological order. |
| `authoritative_cancellation_stops_iteration` | First album's controlled client sets cancellation | Newer album query is absent. |
| `authoritative_empty_searches_nothing` | Provider returns no release groups | Empty query/outcome lists and neutral notice `No eligible authoritative albums found for Test Artist`; refreshed provenance emits no stale warning. |
| `stale_cache_warns_without_legacy_notice` | Stale cache plus provider failure and existing log capture | WARN includes age and refresh error; `run.notice` is `None`. |
| `automatic_legacy_fallback_is_visible` | Provider transport failure and one broad-search result | Query list is `["Test Artist"]`; notice contains the transport reason and `heuristically`. |
| `disabled_discography_uses_legacy_without_outage_notice` | `config.discography.enabled = false` | Query list is `["Test Artist"]`; notice is `None`. |
| `explicit_and_automatic_modes_keep_query_contracts` | Existing explicit album, album-only, batch, and auto fixtures | Their pre-existing expected query lists remain unchanged. |

- [ ] **Step 2: Run the runner RED test**

```bash
cargo test -p seakarr runner::tests::artist_only_authoritative_discovery_searches_each_album_oldest_first -- --nocapture
```

Expected: compilation fails because the provider-aware runner seam and result type do not exist.

- [ ] **Step 3: Isolate the existing legacy path**

Rename current `run_artist_only_mode` to `run_legacy_artist_only_mode` without changing its broad search, single search-history row, grouping, presearched result handling, or cancellation behavior. Run the existing three artist-only legacy tests with `config.discography.enabled = false` and preserve their current assertions.

Define:

```rust
struct ArtistOnlyRun {
    outcomes: Vec<(String, AlbumOutcome)>,
    notice: Option<String>,
}
```

Extract the common per-album loop into `process_artist_album_work`, where each item contains an album title and `Option<Vec<SearchResult>>`. Legacy groups provide `Some(results)`; authoritative targets provide `None`, causing `process_album_internal` to run the normal targeted search and search-history recording.

- [ ] **Step 4: Add authoritative orchestration and production provider creation**

Add a private provider-aware function with this exact boundary:

```rust
#[allow(clippy::too_many_arguments)]
async fn run_artist_only_mode_with_provider(
    client: &dyn SoulseekClient,
    artist: &str,
    ignore_processed: bool,
    config: &Config,
    db: &Database,
    staging_dir: &Path,
    progress: Option<&ProgressDisplay>,
    cancel: &Arc<AtomicBool>,
    provider: &dyn DiscographyProvider,
) -> Result<ArtistOnlyRun>
```

Match `discover_artist_albums`:

- `Authoritative { albums, FreshCache | Refreshed }`: convert titles to work with no presearched results;
- `Authoritative { albums, StaleCache { age_days, refresh_error } }`: emit one WARN naming age and refresh error, then process targets;
- `AuthoritativeEmpty { provenance }`: emit the stale warning when that provenance is `StaleCache`, then return no album outcomes and the neutral notice `No eligible authoritative albums found for {artist}`;
- `LegacyFallback { reason }`: emit one WARN before searching, call `run_legacy_artist_only_mode`, and attach `Authoritative discography unavailable: {reason}; album names were discovered heuristically from Soulseek folders`.

The normal `run_artist_only_mode` wrapper must:

1. call legacy directly with an informational log when `config.discography.enabled` is false;
2. otherwise construct `MusicBrainzProvider::new()`;
3. turn provider-construction failure into the same warned legacy fallback rather than returning a hard error;
4. call the provider-aware function on success.

- [ ] **Step 5: Attach outcomes and notices to `RunReport`**

In `run_manual_mode`, consume `ArtistOnlyRun`: record every outcome exactly as today, then call `report.add_notice` when `notice` is `Some`. Leave the explicit album branch untouched. `report.print_summary()` remains after progress cleanup and before cancellation-listener abort.

- [ ] **Step 6: Run GREEN runner tests**

```bash
cargo test -p seakarr runner::tests::artist_only_authoritative -- --nocapture
cargo test -p seakarr runner::tests::artist_only_manual_mode -- --nocapture
cargo test -p seakarr runner::tests::test_run_manual_mode -- --nocapture
cargo test -p seakarr runner::tests::test_run_auto_mode_processes_album_and_marks_success -- --nocapture
```

Expected: authoritative tests show only sequential `Artist Album` queries; legacy tests show the original one artist query; unrelated modes preserve their existing queries.

- [ ] **Step 7: Record the unstaged checkpoint**

```bash
cargo fmt --all
git diff --check
git status --short
```

Do not stage or commit.

## Task 8: Document authoritative artist discovery

**Files:**

- Modify: `README.md`

- [ ] **Step 1: Update feature and manual-mode documentation**

Document that artist-only mode normally resolves MusicBrainz conceptual albums first and then performs one sequential targeted Soulseek search per album. State that explicit artist-plus-album, album-only, batch, auto, and library-upgrade flows are unchanged.

Replace wording that says artist-only mode discovers albums from one Soulseek query. Keep a separate paragraph explaining that the old folder heuristic remains the configured/failure fallback.

- [ ] **Step 2: Add the complete configuration table**

Add a `discography` subsection containing:

| Key | Description | Default |
| --- | --- | --- |
| `enabled` | Use MusicBrainz release groups before artist-only Soulseek searches; `false` selects legacy folder discovery. | `true` |
| `cache_days` | Complete 24-hour periods before refresh; `0` refreshes every run but keeps stale fallback. | `30` |
| `allowed_types` | Any of `studio_album`, `live_album`, `ep`, `single`, `compilation`, `remix`, `soundtrack`, `dj_mix`, `mixtape`. | `[studio_album]` |
| `artist_mbids` | Optional artist-name to MusicBrainz UUID map for ambiguous names. | `{}` |

- [ ] **Step 3: Add FAQ behavior and examples**

Explain:

- unique exact canonical-name matching and optional MBID overrides;
- oldest-first conceptual albums with editions/remasters deduplicated;
- official/default MusicBrainz release-group status filtering;
- fresh, refresh, stale, and legacy precedence;
- WARN plus run-summary notice on automatic legacy fallback;
- authoritative empty means no downloads and no heuristic fallback;
- one request per second and no MusicBrainz API key.

Include one override example:

```yaml
discography:
  enabled: true
  cache_days: 30
  allowed_types: [studio_album, live_album]
  artist_mbids:
    "Nirvana": "5b11f4ce-a62d-471e-81fc-a69a8278c7da"
```

- [ ] **Step 4: Lint documentation**

```bash
markdownlint --fix README.md
markdownlint README.md
git diff --check
```

Expected: all commands exit zero. Do not stage or commit.

## Task 9: Run full verification and hand off to review

**Files:**

- Verify all modified and created files; do not add implementation changes in this task.

- [ ] **Step 1: Audit spec coverage and stale behavior**

```bash
rg -n "discography|MusicBrainz|studio_album|LegacyFallback|StaleCache" \
  src README.md docs/agent/specs/2026-09-10-authoritative-artist-discography-design.md
rg -n "group_artist_results|run_legacy_artist_only_mode" src/search.rs src/runner.rs
rg -n "search_album_with_fallback_with_queue_limit" src/runner.rs
```

Expected: authoritative and legacy paths both remain reachable; explicit album processing still uses the established queue-aware search cascade.

- [ ] **Step 2: Run formatting and compilation gates**

```bash
cargo fmt --all -- --check
cargo check --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

Expected: every command exits zero with no warnings.

- [ ] **Step 3: Run focused provider, cache, report, and runner tests**

```bash
cargo test -p seakarr discography:: -- --nocapture
cargo test -p seakarr db::tests::discography -- --nocapture
cargo test -p seakarr report::tests -- --nocapture
cargo test -p seakarr runner::tests::artist_only -- --nocapture
```

Expected: all tests pass without live MusicBrainz calls or real multi-second sleeps.

- [ ] **Step 4: Run the complete workspace suite**

```bash
cargo test --workspace
```

Expected: application, binary, integration, vendor, e2e, and doctests all pass.

- [ ] **Step 5: Run repository gates**

```bash
markdownlint README.md \
  docs/agent/specs/2026-09-10-authoritative-artist-discography-design.md \
  docs/agent/plans/2026-09-11-authoritative-artist-discography.md
pre-commit run --all-files
git diff --check
git status --short --branch
```

Expected: lint, hooks, and diff checks exit zero. The working tree contains only the planned unstaged files and the plan itself; no ignored file is staged or committed.

- [ ] **Step 6: Record verification gaps without committing**

If every gate passes, hand off to the required tech-debt, adversarial two-model code-review, QA, and finalising steps. If any gate or specified test coverage is missing, keep the gate closed, record the gaps, obtain user approval, and complete Task 10 before handoff. Do not stage or commit.

## Task 10: Remediate verification gaps

**Files:**

- Modify: `src/config.rs`
- Modify: `src/discography/mod.rs`
- Modify: `src/discography/musicbrainz.rs`
- Modify: `vendor/soulseek-rs-lib/src/actor/peer_actor.rs`

- [ ] **Step 1: Add the missing specification regressions**

Add focused tests proving unknown configured release values fail deserialization, an empty release allowlist remains valid when discovery is disabled, unknown/absent release classifications and empty titles fail closed with contextual debug logs, HTTP response delay beyond the ten-second client timeout is bounded across three attempts, MusicBrainz score/sort-name/aliases cannot widen canonical-name matching, short release-group pages fail immediately, and artist-search count equals payload length.

- [ ] **Step 2: Run the remediation tests**

```bash
cargo test -p seakarr config::tests::discography_unknown_release_type_is_rejected -- --exact --nocapture
cargo test -p seakarr discography::tests::unknown_and_empty_release_metadata_is_rejected -- --exact --nocapture
cargo test -p seakarr discography::musicbrainz::tests::artist_search_sends_identity_headers_and_encoded_query -- --exact --nocapture
cargo test -p seakarr discography::musicbrainz::tests::request_timeout_is_bounded -- --exact --nocapture
cargo test -p seakarr config::tests::disabled_discography_allows_an_empty_release_allowlist -- --exact --nocapture
cargo test -p seakarr discography::tests::excluded_release_groups_are_logged_with_reasons -- --exact --nocapture
cargo test -p seakarr discography::musicbrainz::tests::short_release_group_page_is_incomplete -- --exact --nocapture
cargo test -p seakarr discography::musicbrainz::tests::artist_search_count_must_match_payload -- --exact --nocapture
cargo test -p seakarr runner::tests::authoritative_empty_searches_nothing -- --exact --nocapture
```

Expected: all nine tests pass without live network access or real multi-second waits.

- [ ] **Step 3: Resolve the current Clippy gate**

Change only the private non-mutating receiver:

```rust
fn handle_transfer_request(&self, transfer: Transfer)
```

Run:

```bash
cargo clippy --workspace --all-targets -- -D warnings
```

Expected: exit zero; no behavior or call sites change.

- [ ] **Step 4: Rerun every verification gate**

```bash
cargo fmt --all -- --check
cargo check --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
markdownlint README.md \
  docs/agent/specs/2026-09-10-authoritative-artist-discography-design.md \
  docs/agent/plans/2026-09-11-authoritative-artist-discography.md
pre-commit run --all-files
git diff --check
git status --short --branch
```

Expected: all commands exit zero, all tests pass, and nothing is staged.

- [ ] **Step 5: Continue the chain without committing**

Hand off to tech-debt, adversarial two-model code review, QA, and finalising in that order. Any later behavioral fix must follow TDD and update the approved design if its behavior changes. The finalising step owns the eventual commit choice.

## Spec coverage self-check

| Specification requirement | Planned coverage |
| --- | --- |
| MusicBrainz as sole initial provider | Tasks 3-5 |
| No credentials; identifying User-Agent | Task 4 |
| One request per second and bounded retries | Task 4 |
| Body, page, and partial-pagination safety | Task 4 |
| Unique exact canonical-name match | Tasks 3-5 |
| Optional MBID overrides | Tasks 1 and 5 |
| Friendly configurable categories; studio default | Tasks 1 and 3 |
| Website-default status and conceptual release groups | Tasks 4 and 8 |
| NFKC deduplication and partial-date order | Task 3 |
| SQLite raw cache and 30-day/zero-day semantics | Tasks 2 and 5 |
| Fresh, refresh, stale, legacy precedence | Task 5 |
| Authoritative empty never falls back | Tasks 5 and 7 |
| Sequential targeted Soulseek searches | Task 7 |
| Processed skip, continuation, cancellation | Task 7 |
| Explicit disable and visible fallback | Tasks 6-8 |
| Legacy grouping retained | Task 7 |
| Unrelated modes unchanged | Tasks 7 and 9 |
| Deterministic no-live-service tests | Tasks 4, 5, 7, and 9 |
| README/config documentation | Task 8 |
| Full project gates | Tasks 9-10 |

No specification requirement is intentionally deferred.
