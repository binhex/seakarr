//! Library gap-filling logic for `discover` mode.
//!
//! Pure, synchronous selection logic: an index of what the library already
//! holds, the presence decision, the artist work list, and the per-run
//! download budget. The caller owns all I/O.

use std::collections::{BTreeMap, BTreeSet};

use crate::discography::{normalize_catalog_key, AlbumTarget};
use crate::error::{Result, SeakarrError};
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

    let filter_key = filter
        .map(normalize_catalog_key)
        .filter(|key| !key.is_empty());
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
        // A filter is an explicit request for one artist, so it narrows the
        // list before exclusions are considered and overrides them: naming an
        // excluded artist on the command line is deliberate.
        let selected_by_filter = filter_key.as_deref() == Some(key);
        if filter_key.is_some() && !selected_by_filter {
            continue;
        }
        if excluded_keys.contains(key) && !selected_by_filter {
            selection.excluded.push(name.to_string());
            continue;
        }
        selection.artists.push(name.to_string());
    }
    Ok(selection)
}

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
    if counters.artists_total == 0 {
        // Without this a run over an unusable library exits silently: an empty
        // report prints nothing at all, which is indistinguishable from a
        // crash-free no-op.
        notices.push("discover: no eligible library artists found; nothing to do".to_string());
    }
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
        let remainder = counters
            .unresolved
            .len()
            .saturating_sub(MAX_LISTED_UNRESOLVED);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discography::AlbumTarget;
    use crate::error::SeakarrError;
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

    fn target(title: &str) -> AlbumTarget {
        AlbumTarget {
            release_group_id: format!("rg-{title}"),
            title: title.to_string(),
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
        assert!(
            index.contains_album("The Beatles", "Abbey Road!"),
            "a punctuated title is a distinct index entry"
        );
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
        // One artist key with two spellings, each covering one album, so the
        // count tie-break decides: the lexicographically smaller spelling wins.
        let index = build_index(&[scanned("Zed", "A"), scanned("ZED", "B")]);
        assert_eq!(index.artist_keys().collect::<Vec<_>>(), ["zed"]);
        assert_eq!(index.artist_name("zed"), Some("ZED"));
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

    fn library_fixture() -> LibraryIndex {
        // "Live Band" is deliberate: its key contains "live" as a proper
        // substring, so a substring-based exclusion would wrongly drop it.
        build_index(&[
            scanned("Beta Band", "Album One"),
            scanned("Alpha Artist", "Album Two"),
            scanned("Live", "Album Three"),
            scanned("Live Band", "Album Four"),
        ])
    }

    #[test]
    fn artists_come_back_in_normalised_key_order() {
        let selection = select_artists(&library_fixture(), &[], None).unwrap();
        assert_eq!(
            selection.artists,
            ["Alpha Artist", "Beta Band", "Live", "Live Band"]
        );
        assert!(selection.excluded.is_empty());
    }

    #[test]
    fn exclusions_match_the_whole_key_not_a_substring() {
        let selection = select_artists(&library_fixture(), &["live".to_string()], None).unwrap();
        // "Live Band" survives: only the whole key "live" is excluded.
        assert_eq!(
            selection.artists,
            ["Alpha Artist", "Beta Band", "Live Band"]
        );
        assert_eq!(selection.excluded, ["Live"]);
    }

    #[test]
    fn exclusion_matching_ignores_case_and_whitespace() {
        let selection =
            select_artists(&library_fixture(), &["  BETA   BAND ".to_string()], None).unwrap();
        assert_eq!(selection.artists, ["Alpha Artist", "Live", "Live Band"]);
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
        assert_eq!(selection.artists.len(), 4);
        assert!(selection.excluded.is_empty());
    }

    #[test]
    fn an_explicit_filter_overrides_the_exclusion_list() {
        let selection =
            select_artists(&library_fixture(), &["live".to_string()], Some("Live")).unwrap();
        assert_eq!(selection.artists, ["Live"]);
        assert!(
            selection.excluded.is_empty(),
            "an explicitly requested artist must not also be reported as excluded"
        );
    }

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

    #[test]
    fn an_empty_selection_announces_there_is_nothing_to_do() {
        let counters = DiscoverCounters::default();
        assert_eq!(
            discover_notices(&counters),
            vec!["discover: no eligible library artists found; nothing to do".to_string()]
        );
    }

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
        // A reached budget requires a non-zero limit: with `0` the budget is
        // unlimited and budget_reached_at is never written, so a zero-limit
        // fixture would assert a state the run cannot produce.
        let counters = DiscoverCounters {
            budget_reached_at: Some("Artist".to_string()),
            budget_limit: 7,
            artists_total: 1,
            artists_examined: 1,
            ..DiscoverCounters::default()
        };
        assert!(
            discover_notices(&counters)
                .iter()
                .any(|notice| notice.contains("download budget of 7 reached")),
            "the notice must echo the configured limit verbatim"
        );
    }
}
