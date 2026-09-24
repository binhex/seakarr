//! Library gap-filling logic for `discover` mode.
//!
//! Selection logic and presence data: an index of what the library already
//! holds, the presence decision, the artist work list, and the per-run download
//! budget. Two entry points read the filesystem on the caller's behalf -
//! [`index_from_paths`] walks the library and [`ArtistFolderIndex`] walks the
//! configured roots for artist folders; the rest is pure.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use walkdir::WalkDir;

use crate::config::Config;
use crate::discography::{normalize_album_key, normalize_catalog_key, AlbumTarget};
use crate::error::{Result, SeakarrError};
use crate::organizer::sanitize_component;
use crate::scanner::ScannedAlbum;

/// One library artist: every spelling seen, and the normalised album titles.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct IndexedArtist {
    /// Original spelling to number of albums seen under it.
    spellings: BTreeMap<String, usize>,
    /// Normalised album titles, from the embedded tag when one is present.
    albums: BTreeSet<String>,
    /// Normalised **on-disk album folder names**, the tag spelling included. A
    /// folder this program placed is named from the MusicBrainz title while the
    /// audio inside carries the peer's own tag, so presence has to accept
    /// either spelling or it keeps treating its own placement as missing.
    album_folders: BTreeSet<String>,
    /// Normalised on-disk artist folder names this artist's albums were found
    /// under. Presence follows them, scoped to what each folder holds, so an
    /// album indexed under another artist spelling still counts for this artist
    /// when it lives in one of these folders.
    artist_folders: BTreeSet<String>,
    /// Library root and on-disk artist folder to number of albums found there.
    /// A library whose artist sits under one genre root has a single entry; an
    /// artist split across roots has several and the majority wins.
    ///
    /// The root is kept as a `PathBuf`, never a `String`: a lossy round trip
    /// through UTF-8 would turn a non-UTF-8 library path into a replacement
    /// character, and placement would create a second tree beside the real one.
    destinations: BTreeMap<(PathBuf, String), usize>,
}

/// What the library already holds, keyed exactly like MusicBrainz catalog keys.
///
/// Built from one `scanner::scan_library` walk, so a nested layout such as
/// `<root>/Genre/Artist/Album` is indexed as correctly as `<root>/Artist/Album`:
/// the scanner already resolves the artist from tags with a folder fallback.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct LibraryIndex {
    artists: BTreeMap<String, IndexedArtist>,
    /// The presence keys recorded **inside** each on-disk artist folder, keyed
    /// by the folder's own spelling. Scoping the follow by folder is what keeps
    /// an album that another artist keeps in a different folder from satisfying
    /// this artist's lookup.
    folders: BTreeMap<String, BTreeSet<String>>,
}

/// True when the album is one of the titles this artist entry holds, under
/// either the embedded tag or a folder the album was found in.
fn holds_album(entry: &IndexedArtist, album_key: &str) -> bool {
    entry.albums.contains(album_key) || entry.album_folders.contains(album_key)
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

    /// True when the library holds an album of this artist in a folder named
    /// after the artist itself — the on-disk evidence that the operator
    /// collects this artist under that name rather than meeting it as a guest
    /// inside somebody else's folder.
    pub fn owns_folder(&self, artist_key: &str) -> bool {
        self.artists
            .get(artist_key)
            .is_some_and(|entry| entry.artist_folders.contains(artist_key))
    }

    /// True when this artist/album pair is present in the library.
    ///
    /// Matching is a whole normalised title match, not an identity match: a
    /// library folder carrying a release year, the artist name, or a format
    /// label is a different key, so `2006 - Days to Come` does not satisfy the
    /// target `Days to Come`. Punctuation stays significant by design (see the
    /// README), and the search-side `album_identity_key` heuristic is not
    /// applied here. The README documents the consequence: such a release can be
    /// downloaded again and placed beside the folder that already holds it.
    ///
    /// Two spellings of each half are accepted, because the two sides of the
    /// comparison are written by different owners:
    ///
    /// - the album matches on its embedded tag **or** on any album folder name
    ///   it was found under, which the write path takes from the MusicBrainz
    ///   title;
    /// - the artist matches on its tag spelling **or**, when neither side of
    ///   that comparison is found, on the artist folders its albums live in: an
    ///   album the artist's folders hold counts, whatever artist it is indexed
    ///   under. Any name may be passed, not only an indexed artist, because an
    ///   on-disk folder spelling is itself a valid lookup key.
    pub fn contains_album(&self, artist: &str, album: &str) -> bool {
        let artist_key = normalize_catalog_key(artist);
        let album_key = normalize_album_key(album);
        if self
            .artists
            .get(&artist_key)
            .is_some_and(|entry| holds_album(entry, &album_key))
        {
            return true;
        }
        // Follow the artist folders. An album whose files carry another artist
        // spelling — a third spelling, or none at all, in which case the folder
        // name becomes the key — is indexed under that spelling while still
        // living in a folder this artist's albums were found in. Only the albums
        // recorded **in those folders** count, so an album of another artist
        // that merely shares a folder name, or that lives in a folder this
        // artist does not use, cannot satisfy the lookup.
        //
        // The queried spelling is itself checked as a folder name, which covers
        // both a folder-only artist (no entry under that spelling) and an artist
        // whose albums live in a folder named after the spelling being queried.
        let in_own_folders = self.artists.get(&artist_key).is_some_and(|entry| {
            entry
                .artist_folders
                .iter()
                .any(|folder| self.folder_holds(folder, &album_key))
        });
        in_own_folders || self.folder_holds(&artist_key, &album_key)
    }

    /// True when the folder recorded this album title inside it.
    fn folder_holds(&self, folder: &str, album_key: &str) -> bool {
        self.folders
            .get(folder)
            .is_some_and(|albums| albums.contains(album_key))
    }

    /// Normalised tag-derived album titles for one artist key, in order.
    ///
    /// Only the tags are listed: the titles an album is also reachable under
    /// because of the folder it sits in are presence keys, not catalog titles,
    /// and presence accepts them through [`Self::contains_album`].
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

    /// The library root and on-disk artist folder to place this artist's
    /// downloads under: the pair covering the most albums, with ties broken
    /// alphabetically so the destination never depends on walk order.
    pub fn artist_destination(&self, artist_key: &str) -> Option<(&Path, &str)> {
        let entry = self.artists.get(artist_key)?;
        entry
            .destinations
            .iter()
            .min_by_key(|((root, directory), albums)| {
                (
                    std::cmp::Reverse(**albums),
                    root.as_os_str(),
                    directory.as_str(),
                )
            })
            .map(|((root, directory), _)| (root.as_path(), directory.as_str()))
    }
}

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
    /// Artist keys whose ambiguity has already been reported. Auto mode resolves a
    /// target once per album, so without this an artist with three albums would
    /// repeat the same warning three times in one run.
    warned: Mutex<BTreeSet<String>>,
}

impl ArtistFolderIndex {
    /// An unbuilt index over the configured library roots, in configuration order.
    pub fn new(config: &Config) -> Self {
        Self {
            roots: config.library.paths.iter().map(PathBuf::from).collect(),
            folders: OnceLock::new(),
            warned: Mutex::new(BTreeSet::new()),
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
    ///
    /// A candidate nested inside another candidate is not a second artist folder:
    /// that shape is a self-titled album (`<artist>/<artist>`), so only the
    /// shallowest match counts and no warning is emitted for it. The accepted cost is
    /// that when an artist's name also matches an ancestor folder (`<root>/Rock/Rock`
    /// with the artist "Rock") the ancestor wins silently, which is what the old
    /// direct-child lookup chose as well.
    pub fn find(&self, artist: &str) -> Option<(PathBuf, String)> {
        let key = artist_folder_key(artist)?;
        let folders = self
            .folders
            .get_or_init(|| walk_artist_folders(&self.roots));
        let candidates = folders.get(&key)?;
        // A self-titled album folder carries the artist's own name, so the artist
        // folder and its album folder share this key. Only the shallowest match is
        // the artist folder: the deeper one is the album, and reporting it as a
        // competing artist folder would warn on an ordinary layout.
        let candidates: Vec<&(PathBuf, String)> = candidates
            .iter()
            .filter(|(parent, _)| {
                !candidates
                    .iter()
                    .any(|(other_parent, other_name)| other_parent.join(other_name) == *parent)
            })
            .collect();
        let (parent, name) = (*candidates.first()?).clone();
        if candidates.len() > 1 {
            // Once per artist per run: auto mode resolves a target per album, so an
            // artist with several albums would otherwise repeat this warning.
            let first_report = self
                .warned
                .lock()
                .map(|mut warned| warned.insert(key.clone()))
                .unwrap_or(true);
            if first_report {
                let skipped = candidates
                    .get(1..)
                    .unwrap_or_default()
                    .iter()
                    .map(|candidate| candidate.0.join(&candidate.1).display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                tracing::warn!(
                    "{name}: more than one library folder matches this artist; placing into {} and skipping {skipped}",
                    parent.join(&name).display()
                );
            }
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
        // no folder rather than the root as unreadable. `Path::exists` would fold
        // every metadata error (permissions, I/O) into "not configured yet".
        match std::fs::read_dir(root) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                tracing::warn!(
                    "library root {} cannot be listed ({error}); skipping it",
                    root.display()
                );
                continue;
            }
        }
        for entry in WalkDir::new(root).follow_links(false).sort_by_file_name() {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    // An unreadable subdirectory yields no artist folder, and the
                    // caller would otherwise report the artist as absent rather than
                    // the tree as unreadable.
                    tracing::warn!(
                        "library walk skipped an entry under {}: {error}",
                        root.display()
                    );
                    continue;
                }
            };
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

/// Index scanned albums by normalised artist key and album title.
pub fn build_index(albums: &[ScannedAlbum]) -> LibraryIndex {
    let mut index = LibraryIndex::default();
    for album in albums {
        let artist_key = normalize_catalog_key(&album.artist);
        // A blank title carries no album identity, so it is skipped. The check
        // is made on the title itself rather than on the album key: the
        // sanitiser's non-empty placeholder exists so a path component is never
        // empty, and must not invent an album here.
        if artist_key.is_empty() || normalize_catalog_key(&album.album).is_empty() {
            continue;
        }
        let album_key = normalize_album_key(&album.album);
        let artist_dir_keys: BTreeSet<String> = album
            .artist_dirs
            .iter()
            .chain(std::iter::once(&album.artist_dir))
            .map(|dir| normalize_catalog_key(dir))
            .filter(|key| !key.is_empty())
            .collect();
        let album_dir_keys: Vec<String> = album
            .album_dirs
            .iter()
            .map(|dir| normalize_album_key(dir))
            .collect();
        {
            let entry = index.artists.entry(artist_key.clone()).or_default();
            *entry.spellings.entry(album.artist.clone()).or_insert(0) += 1;
            entry.albums.insert(album_key.clone());
            entry.album_folders.extend(album_dir_keys.iter().cloned());
            entry.artist_folders.extend(artist_dir_keys.iter().cloned());
            *entry
                .destinations
                .entry((album.path.clone(), album.artist_dir.clone()))
                .or_insert(0) += 1;
        }
        // What each folder holds, so presence can be scoped to it: the tag
        // spelling and every folder spelling the album is reachable under. A
        // merged album is recorded under every folder it was found in, not only
        // the recorded location, because it really does sit in each of them.
        for artist_dir_key in artist_dir_keys {
            let folder = index.folders.entry(artist_dir_key).or_default();
            folder.extend(album_dir_keys.iter().cloned());
            folder.insert(album_key.clone());
        }
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
    /// Library spellings skipped because the artist has no folder of its own,
    /// so the library only knows it as a guest inside another artist's folder.
    pub no_folder: Vec<String>,
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
        // Gap filling follows folders, not tags. An artist whose library presence
        // exists only inside another artist's folder — a guest on a compilation,
        // a box-set name, a collaboration spelling — is not an artist the
        // operator collects under that name, so it is skipped instead of having
        // its whole discography fetched. An explicit `--artist` overrides the
        // gate, as it already overrides the exclusion list.
        if !selected_by_filter && !index.owns_folder(key) {
            // Debug, not info: a large library gates hundreds of spellings, and
            // the summary carries the count. The names still have to be
            // recoverable, because the workaround for an artist that is wanted
            // but gated (its albums live in a differently named folder) is to
            // name it with `--artist`.
            tracing::debug!(
                "discover: skipping {name} ({key}): no folder of its own; name it with --artist to process it"
            );
            selection.no_folder.push(name.to_string());
            continue;
        }
        // Every indexed album registers a destination, so the lookup cannot
        // fail for a key drawn from `artist_keys`. The guard is belt and
        // braces, never a silent drop of a real artist — and it warns rather
        // than asserting, so an invariant breach stays visible in release
        // builds instead of silently shrinking the work list.
        let Some((library_root, artist_dir)) = index.artist_destination(key) else {
            tracing::warn!("artist {key:?} has albums but no destination; skipping");
            continue;
        };
        selection.artists.push(SelectedArtist {
            name: name.to_string(),
            library_root: library_root.to_path_buf(),
            artist_dir: artist_dir.to_string(),
        });
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
    /// Artists skipped because they have no folder of their own, so the library
    /// only knows them as guests inside another artist's folder.
    pub no_folder: usize,
    /// Artists MusicBrainz could not resolve, in encounter order.
    pub unresolved: Vec<String>,
    /// Artists skipped because a previous run recorded a resolution failure,
    /// in encounter order. Kept apart from `unresolved` so the summary shows
    /// what was avoided rather than what was attempted.
    pub cached_failures: Vec<String>,
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
    if counters.no_folder > 0 {
        notices.push(format!(
            "discover: {} artist(s) skipped: no folder of their own",
            counters.no_folder
        ));
    }
    if !counters.cached_failures.is_empty() {
        notices.push(format!(
            "discover: {} artist(s) skipped from cached resolution failures",
            counters.cached_failures.len()
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

/// Build a presence index from the configured library roots.
///
/// An empty `paths` list yields an empty index rather than an error, so callers
/// that only use the index to filter (artist-only manual mode) keep working
/// without a configured library.
///
/// `cancel` is forwarded to the walk, so a caller that has armed cancellation
/// can hand it over: a user cancellation then surfaces as
/// [`SeakarrError::Cancelled`] rather than as an empty index, which would make
/// every album look missing.
///
/// # Errors
///
/// [`SeakarrError::Cancelled`] when the walk is cancelled, and
/// [`SeakarrError::Scanner`] when a configured root does not exist.
///
/// `progress` is forwarded to the walk unchanged, so a caller that has an
/// interactive scan indicator gets the same reporting as any other library
/// scan.
pub fn index_from_paths(
    paths: &[String],
    filters: &FilterConfig,
    cancel: Option<&std::sync::atomic::AtomicBool>,
    progress: Option<&dyn crate::scanner::ScanProgress>,
) -> Result<LibraryIndex> {
    if paths.is_empty() {
        return Ok(LibraryIndex::default());
    }
    Ok(build_index(&crate::scanner::scan_library(
        paths, filters, cancel, progress,
    )?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::FilterConfig;
    use crate::discography::AlbumTarget;
    use crate::error::SeakarrError;
    use crate::scanner::scan_library;
    use crate::test_support::write_minimal_flac_with_tags;
    use std::path::PathBuf;
    use tempfile::TempDir;

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
    fn an_artist_folder_five_levels_below_a_root_is_found() {
        // The operator's layout: /media/Music/<user>/<type>/<genre>/<subgenre>/<artist>.
        // The direct-child lookup cannot see it, which is why manual and automatic
        // placement never fired on a real library.
        let library = TempDir::new().unwrap();
        let artist = library
            .path()
            .join("Paul")
            .join("Albums")
            .join("Rock")
            .join("Indie")
            .join("Radiohead");
        std::fs::create_dir_all(&artist).unwrap();
        let config = config_with_roots(&[&library]);

        let found = ArtistFolderIndex::new(&config).find("Radiohead");

        assert_eq!(
            found,
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
        let library = TempDir::new().unwrap();
        let albums = library.path().join("Albums").join("Radiohead");
        let singles = library.path().join("Singles").join("Radiohead");
        std::fs::create_dir_all(&albums).unwrap();
        std::fs::create_dir_all(&singles).unwrap();
        let config = config_with_roots(&[&library]);
        let capture = crate::test_support::LogCapture::start();

        let index = ArtistFolderIndex::new(&config);

        let found = index.find("Radiohead");

        assert_eq!(
            found,
            Some((library.path().join("Albums"), "Radiohead".to_string()))
        );
        // A second lookup for the same artist must not repeat the warning: auto mode
        // resolves a target once per album, and one artist normally has several.
        let _ = index.find("Radiohead");
        let logs = capture.text();
        let warnings = logs
            .lines()
            .filter(|line| {
                line.contains("more than one library folder matches this artist")
                    && line.contains("Radiohead")
            })
            .count();
        assert_eq!(
            warnings, 1,
            "the ambiguity warning is reported once per artist per run:\n{logs}"
        );
    }

    #[test]
    fn a_self_titled_album_folder_is_not_a_competing_artist_folder() {
        // Music/ABBA/ABBA is the ordinary self-titled layout: the album folder shares
        // the artist's name and must not be read as a second artist folder, which
        // would warn on every run for that artist.
        let library = TempDir::new().unwrap();
        let artist = library.path().join("ABBA");
        std::fs::create_dir_all(artist.join("ABBA")).unwrap();
        let config = config_with_roots(&[&library]);
        let capture = crate::test_support::LogCapture::start();

        let found = ArtistFolderIndex::new(&config).find("ABBA");

        assert_eq!(
            found,
            Some((library.path().to_path_buf(), "ABBA".to_string()))
        );
        let logs = capture.text();
        let warnings = logs
            .lines()
            .filter(|line| {
                line.contains("more than one library folder matches this artist")
                    && line.contains("ABBA")
            })
            .count();
        assert_eq!(
            warnings, 0,
            "a self-titled album folder is not an ambiguity:\n{logs}"
        );
    }

    #[test]
    fn the_index_is_not_built_when_nothing_is_looked_up() {
        // A run that places nothing must not walk the library tree at all.
        let library = TempDir::new().unwrap();
        let mut config = Config::default();
        config.library.paths = vec![library
            .path()
            .join("missing")
            .to_string_lossy()
            .into_owned()];

        let index = ArtistFolderIndex::new(&config);

        assert!(
            !index.is_built(),
            "constructing the index must not walk anything"
        );
    }

    #[test]
    fn artist_folder_index_returns_the_on_disk_spelling() {
        let library = library_with_artist_folders(&["the cinematic orchestra"]);
        let config = config_with_roots(&[&library]);

        let found = ArtistFolderIndex::new(&config).find("The Cinematic Orchestra");

        assert_eq!(
            found,
            Some((
                library.path().to_path_buf(),
                "the cinematic orchestra".to_string()
            ))
        );
    }

    #[test]
    fn artist_folder_index_folds_the_tag_spelling_onto_the_stored_folder() {
        // The sanitiser stores "AC/DC" as "AC-DC"; the tag spelling must find it.
        let library = library_with_artist_folders(&["AC-DC"]);
        let config = config_with_roots(&[&library]);

        let found = ArtistFolderIndex::new(&config).find("AC/DC");

        assert_eq!(
            found,
            Some((library.path().to_path_buf(), "AC-DC".to_string()))
        );
    }

    #[test]
    fn artist_folder_index_is_none_when_the_artist_has_no_folder() {
        let library = library_with_artist_folders(&["Someone Else"]);
        let config = config_with_roots(&[&library]);

        assert_eq!(
            ArtistFolderIndex::new(&config).find("The Cinematic Orchestra"),
            None
        );
    }

    #[test]
    fn artist_folder_index_prefers_the_first_configured_root() {
        let first = library_with_artist_folders(&["The Artist"]);
        let second = library_with_artist_folders(&["The Artist"]);
        let config = config_with_roots(&[&first, &second]);

        let found = ArtistFolderIndex::new(&config).find("The Artist");

        assert_eq!(
            found,
            Some((first.path().to_path_buf(), "The Artist".to_string())),
            "configuration order decides, deterministically"
        );
    }

    #[test]
    fn artist_folder_index_ignores_a_file_named_like_the_artist() {
        let library = TempDir::new().unwrap();
        std::fs::write(library.path().join("The Artist"), b"not a folder").unwrap();
        let config = config_with_roots(&[&library]);

        assert_eq!(ArtistFolderIndex::new(&config).find("The Artist"), None);
    }

    #[test]
    fn artist_folder_index_is_none_without_library_paths() {
        let config = Config::default();

        assert!(config.library.paths.is_empty());
        assert_eq!(ArtistFolderIndex::new(&config).find("The Artist"), None);
    }

    #[test]
    fn artist_folder_index_is_none_for_a_blank_artist() {
        // An album-only manual run passes an empty artist. Sanitising that yields
        // the non-empty "_" placeholder, which must not match a folder whose own
        // name sanitises away: such a folder is not an artist folder.
        let library = library_with_artist_folders(&["_", "***", "The Artist"]);
        let config = config_with_roots(&[&library]);

        assert_eq!(ArtistFolderIndex::new(&config).find(""), None);
        assert_eq!(ArtistFolderIndex::new(&config).find("   "), None);
        // A name that carries nothing the filesystem can keep is refused too, so it
        // cannot match the placeholder folder either.
        assert_eq!(ArtistFolderIndex::new(&config).find("***"), None);
        assert_eq!(ArtistFolderIndex::new(&config).find("???"), None);
    }

    #[test]
    fn artist_folder_index_skips_a_root_that_is_not_a_directory() {
        // A file where a root belongs cannot be listed: the resolver must name it and try
        // the next configured root rather than reporting the artist as missing.
        let holder = TempDir::new().unwrap();
        let file_root = holder.path().join("not-a-directory");
        std::fs::write(&file_root, b"file").unwrap();
        let library = library_with_artist_folders(&["The Artist"]);
        let mut config = Config::default();
        config.library.paths = vec![
            file_root.to_string_lossy().into_owned(),
            library.path().to_string_lossy().into_owned(),
        ];
        let capture = crate::test_support::LogCapture::start();

        assert_eq!(
            ArtistFolderIndex::new(&config).find("The Artist"),
            Some((library.path().to_path_buf(), "The Artist".to_string()))
        );
        let logs = capture.text();
        assert!(
            logs.lines()
                .any(|line| line.contains("cannot be listed") && line.contains("not-a-directory")),
            "an unlistable root must be reported as such:\n{logs}"
        );
    }

    // Linux filesystems accept arbitrary bytes in a name; macOS rejects them at the
    // syscall, so this fixture only exists here.
    #[cfg(target_os = "linux")]
    #[test]
    fn artist_folder_index_skips_a_non_utf8_folder() {
        use std::os::unix::ffi::OsStrExt;

        let library = TempDir::new().unwrap();
        let name = std::ffi::OsStr::from_bytes(b"The Art\xffist");
        std::fs::create_dir_all(library.path().join(name)).unwrap();
        let config = config_with_roots(&[&library]);

        // The folder exists on disk, but its name is not valid UTF-8: a lossy match
        // could make placement create a second folder beside the real one.
        assert_eq!(
            ArtistFolderIndex::new(&config).find("The Art\u{fffd}ist"),
            None
        );
    }

    #[test]
    fn artist_folder_index_skips_an_unusable_root() {
        // A root that no longer exists (or is unreadable) must not stop the search:
        // the next configured root still supplies the artist folder.
        let missing = TempDir::new().unwrap();
        let missing_path = missing.path().to_string_lossy().into_owned();
        drop(missing);
        let library = library_with_artist_folders(&["The Artist"]);
        let mut config = Config::default();
        config.library.paths = vec![missing_path, library.path().to_string_lossy().into_owned()];

        assert_eq!(
            ArtistFolderIndex::new(&config).find("The Artist"),
            Some((library.path().to_path_buf(), "The Artist".to_string()))
        );
    }

    #[test]
    fn artist_folder_index_prefers_the_smallest_spelling_within_a_root() {
        // Created in reverse order. On a filesystem whose read_dir returns insertion
        // order (tmpfs, a small ext4 directory) an unsorted resolver returns
        // "the artist" and fails here; a hash-ordered directory may still return the
        // sorted winner by chance, so this asserts the destination rather than proving
        // the sort.
        let library = library_with_artist_folders(&["the artist", "The Artist"]);
        let config = config_with_roots(&[&library]);

        assert_eq!(
            ArtistFolderIndex::new(&config).find("The Artist"),
            Some((library.path().to_path_buf(), "The Artist".to_string())),
            "read_dir order must not decide the destination"
        );
    }

    fn scanned(artist: &str, album: &str) -> ScannedAlbum {
        scanned_at(artist, album, "/library", artist)
    }

    fn scanned_at(artist: &str, album: &str, root: &str, artist_dir: &str) -> ScannedAlbum {
        scanned_in(artist, album, album, root, artist_dir)
    }

    /// As [`scanned_at`], but with the on-disk album folder named separately
    /// from the album tag — the seam a discovery placement creates.
    fn scanned_in(
        artist: &str,
        album: &str,
        album_dir: &str,
        root: &str,
        artist_dir: &str,
    ) -> ScannedAlbum {
        ScannedAlbum {
            path: PathBuf::from(root),
            artist: artist.to_string(),
            album: album.to_string(),
            album_dirs: [album_dir.to_string()].into_iter().collect(),
            artist_dirs: [artist_dir.to_string()].into_iter().collect(),
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

    fn target(title: &str) -> AlbumTarget {
        AlbumTarget {
            release_group_id: format!("rg-{title}"),
            title: title.to_string(),
        }
    }

    // ── Presence keys survive the portable-name sanitiser ──

    #[test]
    fn an_album_written_through_the_sanitiser_still_matches_its_musicbrainz_title() {
        // The folder seakarr wrote to disk went through the path sanitiser, so
        // the library holds "Tronic Jazz The Berlin Sessions" while MusicBrainz
        // still reports "Tronic Jazz: The Berlin Sessions". The presence check
        // must compare the sanitised form on both sides, or the album is
        // downloaded again on every discover cycle.
        let index = build_index(&[scanned(
            "A Guy Called Gerald",
            "Tronic Jazz The Berlin Sessions",
        )]);
        assert!(
            index.contains_album("A Guy Called Gerald", "Tronic Jazz: The Berlin Sessions"),
            "the sanitised on-disk title must satisfy the MusicBrainz title"
        );
    }

    #[test]
    fn the_presence_key_is_symmetric_for_the_stored_and_musicbrainz_spellings() {
        // Coupling guard: whichever function the presence check uses, both the
        // on-disk (sanitised) spelling and the MusicBrainz spelling must resolve
        // to one key, from either direction of the lookup.
        let stored = build_index(&[scanned(
            "A Guy Called Gerald",
            "Tronic Jazz The Berlin Sessions",
        )]);
        let tagged = build_index(&[scanned(
            "A Guy Called Gerald",
            "Tronic Jazz: The Berlin Sessions",
        )]);
        for spelling in [
            "Tronic Jazz The Berlin Sessions",
            "Tronic Jazz: The Berlin Sessions",
        ] {
            assert!(
                stored.contains_album("A Guy Called Gerald", spelling),
                "stored album must match {spelling:?}"
            );
            assert!(
                tagged.contains_album("A Guy Called Gerald", spelling),
                "tagged album must match {spelling:?}"
            );
        }
    }

    #[test]
    fn a_fullwidth_spelling_of_a_stripped_character_still_matches() {
        // NFKC folds a compatibility variant (U+FF1A) to the character the
        // sanitiser removes. Both sides must be folded BEFORE the sanitiser runs,
        // or a library tagged with the fullwidth form never matches the
        // MusicBrainz spelling and discover re-downloads the album every cycle.
        let index = build_index(&[scanned(
            "A Guy Called Gerald",
            "Tronic Jazz\u{ff1a} The Berlin Sessions",
        )]);
        assert!(
            index.contains_album("A Guy Called Gerald", "Tronic Jazz: The Berlin Sessions"),
            "a fullwidth spelling must resolve to the same presence key"
        );
    }

    #[test]
    fn missing_albums_does_not_reselect_an_album_that_was_sanitised_on_write() {
        // The user-visible consequence: a stored album must not be selected for
        // download again just because its title carried a stripped character.
        let index = build_index(&[scanned(
            "A Guy Called Gerald",
            "Tronic Jazz The Berlin Sessions",
        )]);
        let missing = missing_albums(
            &index,
            "A Guy Called Gerald",
            &[target("Tronic Jazz: The Berlin Sessions")],
        );
        assert!(
            missing.is_empty(),
            "a stored album must not be downloaded again: {missing:?}"
        );
    }

    #[test]
    fn an_album_placed_under_its_musicbrainz_title_counts_as_present_when_its_tag_differs() {
        // The reported bug. Discover names the folder it writes from the
        // MusicBrainz title, but the audio inside carries whatever the peer
        // tagged it with, and the scanner keys an album on that tag. When the
        // two spellings differ the album seakarr had just placed was invisible
        // to the presence check, so the next run searched the network and
        // downloaded the whole album again.
        //
        // The pair here differs by a curly (U+2019) versus ASCII apostrophe,
        // which the sanitiser leaves alone (it removes only `< > : " | ? *` and
        // control characters), so the two really are different keys; the
        // It-Is/It's test below pins the same seam with a difference no
        // normalisation can unify.
        let library = TempDir::new().unwrap();
        let album_dir = library
            .path()
            .join("Aesop Rock")
            .join("I Heard It\u{2019}s a Mess There Too");
        std::fs::create_dir_all(&album_dir).unwrap();
        write_minimal_flac_with_tags(
            &album_dir.join("01 - track.flac"),
            "Aesop Rock",
            "I Heard It's A Mess There Too",
        );

        let scanned = scan_library(
            &[library.path().to_string_lossy().into_owned()],
            &FilterConfig::default(),
            None,
            None,
        )
        .unwrap();
        let index = build_index(&scanned);

        assert!(
            index.contains_album("Aesop Rock", "I Heard It\u{2019}s a Mess There Too"),
            "the folder the write path created must satisfy the MusicBrainz title"
        );
        let missing = missing_albums(
            &index,
            "Aesop Rock",
            &[target("I Heard It\u{2019}s a Mess There Too")],
        );
        assert!(
            missing.is_empty(),
            "an album already placed must not be selected again: {missing:?}"
        );
    }

    #[test]
    fn the_on_disk_album_folder_name_satisfies_the_title_its_tag_misspells() {
        // The seam a placement creates: the folder takes the MusicBrainz title,
        // the tag keeps the peer's spelling. A fast pin for the same behaviour
        // the end-to-end discover run covers, including the boundary that the
        // folder name must not make punctuation insignificant.
        let index = build_index(&[scanned_in(
            "Aesop Rock",
            "I Heard It's A Mess There Too",
            "I Heard It\u{2019}s a Mess There Too",
            "/library",
            "Aesop Rock",
        )]);

        assert!(
            index.contains_album("Aesop Rock", "I Heard It\u{2019}s a Mess There Too"),
            "the folder name must satisfy the title it was written from"
        );
        assert!(
            index.contains_album("Aesop Rock", "I Heard It's A Mess There Too"),
            "the tag spelling must keep working"
        );
        assert!(
            !index.contains_album("Aesop Rock", "I Heard Its a Mess There Too"),
            "a third spelling must stay absent: punctuation remains significant"
        );
    }

    #[test]
    fn a_folder_named_after_the_plain_title_satisfies_it_even_when_its_files_are_tagged_as_an_edition(
    ) {
        // Accepted consequence of accepting either spelling. The folder name is
        // what the write path took from the MusicBrainz title, while the tag
        // inside can still name the edition the peer served, so the plain album
        // counts as present once its folder carries the plain name. Pinned so
        // the trade-off is explicit and cannot change silently.
        let index = build_index(&[scanned_in(
            "Artist",
            "Greatest Hits (Deluxe Edition)",
            "Greatest Hits",
            "/library",
            "Artist",
        )]);

        assert!(
            index.contains_album("Artist", "Greatest Hits"),
            "the folder naming the plain title satisfies it"
        );
        assert!(
            index.contains_album("Artist", "Greatest Hits (Deluxe Edition)"),
            "the tag spelling keeps working"
        );
        assert!(
            !index.contains_album("Artist", "Greatest Hits (Remastered)"),
            "an unrelated edition stays absent"
        );
    }

    #[test]
    fn a_folder_and_tag_that_no_normalisation_can_unify_still_resolve_by_folder() {
        // The reported case in a form Unicode normalisation cannot fold: "It Is"
        // and "It's" are different strings under NFKC and every other form, so
        // the folder alias is load-bearing here by construction rather than by
        // how a curly apostrophe happens to normalise.
        let index = build_index(&[scanned_in(
            "Aesop Rock",
            "I Heard It Is a Mess There Too",
            "I Heard It's a Mess There Too",
            "/library",
            "Aesop Rock",
        )]);

        assert!(
            index.contains_album("Aesop Rock", "I Heard It's a Mess There Too"),
            "the folder name must satisfy the title it was written from"
        );
        assert!(
            !index.contains_album("Aesop Rock", "I Heard It Was a Mess There Too"),
            "a title no folder or tag carries stays absent"
        );
    }

    #[test]
    fn a_folder_keyed_album_counts_for_the_artist_spelled_in_its_tags() {
        // The direction discover actually uses. The work item for a tag-keyed
        // artist carries the tag spelling, while an album whose files are
        // untagged or carry a third artist spelling is keyed on that spelling
        // instead. Both albums live in the same artist folder, so presence is
        // scoped to the folder and neither album is downloaded again.
        let index = build_index(&[
            scanned_at("Aesop Rock", "Appetite", "/library", "Blockhead"),
            scanned_at(
                "Aesop Rock x Blockhead",
                "Garbology",
                "/library",
                "Blockhead",
            ),
        ]);

        assert!(index.contains_album("Aesop Rock", "Appetite"));
        assert!(
            index.contains_album("Aesop Rock", "Garbology"),
            "an album keyed on another artist spelling still counts for the artist whose folder holds it"
        );
        assert!(
            !index.contains_album("Aesop Rock", "Unreleased"),
            "an album no folder or tag carries stays missing"
        );
    }

    #[test]
    fn an_album_of_another_artist_in_a_different_folder_does_not_count() {
        // The folder follow is scoped to what the artist's own folders hold. A
        // second artist sharing a folder name must not leak the albums it keeps
        // elsewhere, or a genuinely missing album looks present and discover
        // silently never fetches it.
        let index = build_index(&[
            scanned_at("Alpha", "Anthology", "/library", "Shared"),
            scanned_in("Beta", "Beta Song", "Beta Song", "/library", "Shared"),
            scanned_in("Beta", "Far Away", "Weird Folder", "/library", "Elsewhere"),
        ]);

        assert!(index.contains_album("Alpha", "Anthology"));
        assert!(
            index.contains_album("Alpha", "Beta Song"),
            "an album in the folder Alpha's own albums live in counts"
        );
        assert!(
            !index.contains_album("Alpha", "Far Away"),
            "an album filed in a folder Alpha does not use must not count"
        );
    }

    #[test]
    fn an_album_in_a_folder_named_after_the_queried_spelling_counts() {
        // The queried spelling is itself a folder name, so it is followed even
        // when the artist also has an entry under that key. Without it, an
        // album kept in a folder named after the spelling being queried is
        // downloaded again.
        let index = build_index(&[
            scanned_in("Aesop Rock", "Skelethon", "Skelethon", "/lib", "Blockhead"),
            scanned_in(
                "Aesop Rock x Blockhead",
                "Garbology",
                "Garbology",
                "/lib",
                "Aesop Rock",
            ),
        ]);

        assert!(index.contains_album("Aesop Rock", "Skelethon"));
        assert!(
            index.contains_album("Aesop Rock", "Garbology"),
            "the folder named after the queried spelling holds this album"
        );
    }

    #[test]
    fn a_folder_only_spelling_cannot_be_selected_with_a_filter() {
        // `--artist` narrows the work list, which is keyed on the artist tag
        // spelling; a name that exists only as an on-disk folder spelling must
        // still be rejected rather than silently selecting nothing or the wrong
        // artist.
        let index = build_index(&[scanned_at(
            "Aesop Rock",
            "Appetite",
            "/library",
            "Blockhead",
        )]);

        let error = select_artists(&index, &[], Some("Blockhead"))
            .expect_err("a folder-only spelling is not an artist the library holds");
        assert!(
            error.to_string().contains("Blockhead"),
            "the error must name the rejected spelling: {error}"
        );
        assert_eq!(
            selected_names(&select_artists(&index, &[], Some("Aesop Rock")).unwrap()),
            ["Aesop Rock"],
            "the tag spelling still selects the artist"
        );
    }

    #[test]
    fn an_artist_folder_spelling_finds_albums_indexed_under_a_different_tag_spelling() {
        // The other documented loop: an artist's albums can be indexed under a
        // tag spelling while the folder holding them is spelled differently
        // (with the tag absent, the folder name becomes the artist key). A
        // lookup for the folder spelling must still find those albums.
        let index = build_index(&[scanned_at(
            "Aesop Rock",
            "Garbology",
            "/library",
            "Blockhead",
        )]);

        assert!(
            index.contains_album("Blockhead", "Garbology"),
            "the artist folder spelling must find albums stored in that folder"
        );
        assert_eq!(
            index.artist_keys().collect::<Vec<_>>(),
            ["aesop rock"],
            "the folder spelling must not add an artist to the work list"
        );
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
    fn a_year_prefixed_folder_does_not_satisfy_the_plain_title() {
        // Documented limitation (README): presence is a whole normalised title
        // match, so a folder written as "2006 - Days to Come" is a different
        // album from the target "Days to Come" and discover may place a second
        // copy beside it. Folding the search-side identity heuristic in here
        // would also make punctuation insignificant, which the README rules out.
        let index = build_index(&[scanned("Bonobo", "2006 - Days To Come")]);
        assert!(index.contains_album("Bonobo", "2006 - Days To Come"));
        assert!(
            !index.contains_album("Bonobo", "Days to Come"),
            "the plain MusicBrainz title is not satisfied by the year-prefixed folder"
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

    /// The destination pair as owned strings, so an assertion reads the same way
    /// regardless of the `PathBuf` the index keeps internally.
    fn destination(index: &LibraryIndex, artist_key: &str) -> Option<(String, String)> {
        index
            .artist_destination(artist_key)
            .map(|(root, directory)| (root.to_string_lossy().into_owned(), directory.to_string()))
    }

    #[test]
    fn destination_is_recorded_per_artist_from_the_scan() {
        let index = build_index(&[
            scanned_at("Metallica", "72 Seasons", "/library/Metal", "Metallica"),
            scanned_at("Metallica", "Reload", "/library/Metal", "Metallica"),
        ]);
        assert_eq!(
            destination(&index, "metallica"),
            Some(("/library/Metal".to_string(), "Metallica".to_string()))
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
            destination(&index, "artist"),
            Some(("/library/Metal".to_string(), "Artist".to_string())),
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
            destination(&index, "guns 'n' roses"),
            Some(("/library/Rock".to_string(), "Guns N Roses".to_string())),
            "the folder that exists on disk wins over the tag spelling"
        );
    }

    #[test]
    fn destination_tie_breaks_alphabetically_when_counts_are_equal() {
        // Equal album counts are the case the walk-order-independence claim
        // rests on, and the only case where the tie-break itself decides.
        let index = build_index(&[
            scanned_at("Artist", "One", "/library/Zoo", "Artist"),
            scanned_at("Artist", "Two", "/library/Metal", "Artist"),
        ]);
        assert_eq!(
            destination(&index, "artist"),
            Some(("/library/Metal".to_string(), "Artist".to_string())),
            "the alphabetically first root wins a tie regardless of insert order"
        );
    }

    #[test]
    fn an_unknown_artist_has_no_destination() {
        assert_eq!(build_index(&[]).artist_destination("nobody"), None);
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
            selected_names(&selection),
            ["Alpha Artist", "Beta Band", "Live", "Live Band"]
        );
        assert!(selection.excluded.is_empty());
    }

    #[test]
    fn exclusions_match_the_whole_key_not_a_substring() {
        let selection = select_artists(&library_fixture(), &["live".to_string()], None).unwrap();
        // "Live Band" survives: only the whole key "live" is excluded.
        assert_eq!(
            selected_names(&selection),
            ["Alpha Artist", "Beta Band", "Live Band"]
        );
        assert_eq!(selection.excluded, ["Live"]);
    }

    #[test]
    fn exclusion_matching_ignores_case_and_whitespace() {
        let selection =
            select_artists(&library_fixture(), &["  BETA   BAND ".to_string()], None).unwrap();
        assert_eq!(
            selected_names(&selection),
            ["Alpha Artist", "Live", "Live Band"]
        );
        assert_eq!(selection.excluded, ["Beta Band"]);
    }

    #[test]
    fn a_filter_narrows_the_run_to_one_artist() {
        let selection = select_artists(&library_fixture(), &[], Some("beta band")).unwrap();
        assert_eq!(selected_names(&selection), ["Beta Band"]);
    }

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

    #[test]
    fn an_artist_without_a_folder_of_its_own_is_not_gap_filled() {
        // The reported bug. Albums tagged ARTIST=Apashe filed inside the
        // Bassnectar folder made discover treat Apashe as an artist to fill and
        // fetch its whole discography. An artist whose library presence exists
        // only inside another artist's folder is not an artist the operator
        // collects under that name, so it is not gap-filled.
        let index = build_index(&[scanned_at(
            "Apashe",
            "Machines Should Work",
            "/library",
            "Bassnectar",
        )]);

        let selection = select_artists(&index, &[], None).unwrap();

        assert!(
            selection.artists.is_empty(),
            "an artist with no folder of its own must not be selected: {:?}",
            selected_names(&selection)
        );
        assert_eq!(
            selection.no_folder,
            ["Apashe"],
            "the skip must be reported, not silent"
        );
    }

    #[test]
    fn a_folder_owner_keeps_being_gap_filled_while_its_guest_artist_does_not() {
        // One folder, two tag spellings: the artist whose folder it is stays a
        // work item, the guest tagged inside it does not.
        let index = build_index(&[
            scanned_at("Apashe", "Antagonist", "/library", "Bassnectar"),
            scanned_at("Bassnectar", "All Colors", "/library", "Bassnectar"),
        ]);

        let selection = select_artists(&index, &[], None).unwrap();

        assert_eq!(selected_names(&selection), ["Bassnectar"]);
        assert_eq!(selection.no_folder, ["Apashe"]);
    }

    #[test]
    fn an_explicit_filter_still_selects_an_artist_without_its_own_folder() {
        // `--artist` is a deliberate one-off request, so it overrides the folder
        // gate exactly as it already overrides discover.exclude_artists.
        let index = build_index(&[scanned_at(
            "Apashe",
            "Machines Should Work",
            "/library",
            "Bassnectar",
        )]);

        let selection = select_artists(&index, &[], Some("Apashe")).unwrap();

        assert_eq!(selected_names(&selection), ["Apashe"]);
        assert!(
            selection.no_folder.is_empty(),
            "an explicitly named artist is not a gated skip"
        );
    }

    #[test]
    fn the_folder_gate_is_reported_in_the_run_summary() {
        let counters = DiscoverCounters {
            no_folder: 3,
            artists_total: 1,
            ..DiscoverCounters::default()
        };
        // The whole line, not a substring: "13 artist(s) …" also contains
        // "3 artist(s) …", so a count regression would slip past a partial match.
        assert!(
            discover_notices(&counters)
                .contains(&"discover: 3 artist(s) skipped: no folder of their own".to_string()),
            "the gate must be visible in the run summary, not silent"
        );
    }

    #[test]
    fn an_artist_owns_a_folder_however_the_merged_album_chose_its_location() {
        // A merged album (same tag artist and album in two folders) keeps one
        // folder as its recorded location, but the artist still owns the other
        // one. The gate asks the folder set, not the recorded location, so the
        // artist is swept whichever way the location contest went.
        let mut merged = scanned_in("Zed", "Album", "Album", "/root", "Compilations");
        merged.artist_dirs = ["Compilations".to_string(), "Zed".to_string()]
            .into_iter()
            .collect();
        let index = build_index(&[merged]);

        let selection = select_artists(&index, &[], None).unwrap();

        assert_eq!(selected_names(&selection), ["Zed"]);
        assert!(selection.no_folder.is_empty());
    }

    #[test]
    fn an_album_with_no_usable_artist_folder_fails_closed() {
        // Belt and braces: the scanner never produces an empty artist folder
        // (path components are non-empty), but if one reached the index it must
        // not satisfy the gate for the artist whose entry it belongs to.
        let index = build_index(&[scanned_in("Ghost", "Album", "Album", "/library", "")]);

        let selection = select_artists(&index, &[], None).unwrap();

        assert!(
            selection.artists.is_empty(),
            "an unusable folder is no evidence of ownership"
        );
        assert_eq!(selection.no_folder, ["Ghost"]);
    }

    #[test]
    fn the_gate_normalises_both_sides_before_comparing() {
        // Gate fairness rests on folding both sides: a folder whose casing (or
        // spacing, or width) differs from the tag spelling still names the
        // artist, so a raw-string comparison would gate out real artists.
        let index = build_index(&[scanned_at(
            "SIGUR ROS",
            "Agaetis Byrjun",
            "/library",
            "Sigur Ros",
        )]);

        let selection = select_artists(&index, &[], None).unwrap();

        assert_eq!(selected_names(&selection), ["SIGUR ROS"]);
        assert!(selection.no_folder.is_empty());

        // Spacing and width fold too, not only casing: the gate compares
        // normalised keys, so a doubled space or a fullwidth space on either
        // side still names the same artist.
        for (tag, folder) in [
            ("SIGUR  ROS", "Sigur Ros"),
            ("SIGUR ROS", "Sigur\u{3000}Ros"),
        ] {
            let index = build_index(&[scanned_at(tag, "Agaetis Byrjun", "/library", folder)]);
            let selection = select_artists(&index, &[], None).unwrap();
            assert_eq!(
                selected_names(&selection),
                [tag],
                "tag {tag:?} in folder {folder:?} must still be swept"
            );
        }
    }

    #[test]
    fn an_artist_with_albums_in_several_folders_is_selected_when_one_matches() {
        // The gate asks whether the artist owns *a* folder, not whether every
        // album sits in one: one matching folder is enough to keep the artist
        // in the work list, however its other albums are filed.
        let index = build_index(&[
            scanned_at("Apashe", "Antagonist", "/library", "Bassnectar"),
            scanned_at("Apashe", "Renaissance", "/library", "Apashe"),
        ]);

        let selection = select_artists(&index, &[], None).unwrap();

        assert_eq!(selected_names(&selection), ["Apashe"]);
        assert!(selection.no_folder.is_empty());
    }

    #[test]
    fn an_excluded_artist_is_reported_as_excluded_not_as_a_gated_skip() {
        // Exclusions are decided first, so an artist that is both excluded and
        // folder-less is attributed to the list the operator wrote, and the
        // gate count stays meaningful.
        let index = build_index(&[
            scanned_at("Apashe", "Antagonist", "/library", "Bassnectar"),
            scanned_at("Bassnectar", "All Colors", "/library", "Bassnectar"),
            scanned_at("Burial", "Untrue", "/library", "Nobody"),
        ]);

        let selection =
            select_artists(&index, &["Apashe".to_string(), "Burial".to_string()], None).unwrap();

        assert_eq!(selected_names(&selection), ["Bassnectar"]);
        assert_eq!(selection.excluded, ["Apashe", "Burial"]);
        assert!(
            selection.no_folder.is_empty(),
            "an excluded artist must not also be counted as gated"
        );
    }

    #[test]
    fn a_folder_name_the_write_path_sanitised_does_not_satisfy_the_gate() {
        // Accepted cost of the gate, pinned so it cannot change silently. The
        // library write sanitises the artist component (a slash becomes a hyphen), so
        // an artist tagged `AC/DC` can own a folder spelled `AC-DC` and still be
        // gated out. Naming it explicitly is the workaround.
        let index = build_index(&[scanned_at("AC/DC", "Back in Black", "/library", "AC-DC")]);

        let selection = select_artists(&index, &[], None).unwrap();
        assert!(selection.artists.is_empty());
        assert_eq!(selection.no_folder, ["AC/DC"]);

        let named = select_artists(&index, &[], Some("AC/DC")).unwrap();
        assert_eq!(selected_names(&named), ["AC/DC"]);
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
        assert_eq!(selected_names(&selection), ["Live"]);
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
    fn cached_failures_get_their_own_notice() {
        let counters = DiscoverCounters {
            cached_failures: vec!["40 Licks".to_string(), "30Hz".to_string()],
            unresolved: vec!["Fresh Failure".to_string()],
            ..DiscoverCounters::default()
        };

        let notices = discover_notices(&counters);

        assert!(
            notices.contains(
                &"discover: 2 artist(s) skipped from cached resolution failures".to_string()
            ),
            "got {notices:?}"
        );
        assert!(
            notices.contains(
                &"discover: 1 artist(s) unresolved on MusicBrainz: Fresh Failure".to_string()
            ),
            "the cached and fresh notices must stay distinguishable: {notices:?}"
        );
    }

    #[test]
    fn notices_summarise_every_aggregate_in_order() {
        let counters = DiscoverCounters {
            present: 12,
            excluded: 2,
            no_folder: 3,
            unresolved: vec!["Mystery Artist".to_string()],
            cached_failures: vec!["Cached Artist".to_string()],
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
                "discover: 3 artist(s) skipped: no folder of their own".to_string(),
                "discover: 1 artist(s) skipped from cached resolution failures".to_string(),
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
