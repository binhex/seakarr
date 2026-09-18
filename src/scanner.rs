use std::collections::HashSet;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

use lofty::file::{AudioFile, TaggedFileExt};
use lofty::tag::Accessor;

use crate::config::FilterConfig;
use crate::error::{Result, SeakarrError};

#[derive(Debug, Clone)]
pub struct ScannedAlbum {
    pub path: PathBuf,
    /// Artist name as the album is keyed: the embedded tag when one is present,
    /// otherwise the on-disk artist folder name.
    pub artist: String,
    /// Album title as the album is keyed: the embedded tag when one is
    /// present, otherwise the on-disk folder name.
    pub album: String,
    /// On-disk names of every album folder this album was found in. Kept for
    /// the library-presence key: the write path names a placed folder from the
    /// MusicBrainz title, while the audio inside carries whatever the peer
    /// tagged it with, so a presence check that saw only `album` would treat
    /// the album seakarr had just placed as missing and download it again.
    ///
    /// A set, not one name: the same tagged album can sit in several folders,
    /// and presence must accept whichever of them a target title matches. A
    /// single value would be whichever file the unsorted walk happened to see
    /// first, which the walk-order-independence contract excludes.
    pub album_dirs: std::collections::BTreeSet<String>,
    /// On-disk names of every artist folder this album was found in. Like
    /// `album_dirs`, a set rather than the single recorded location: a merged
    /// album (same tag artist and album in two folders) keeps one `artist_dir`
    /// as its destination, but every folder that holds it is evidence that its
    /// artist owns that folder, which is what the discover work list gates on.
    pub artist_dirs: std::collections::BTreeSet<String>,
    /// On-disk name of the artist folder this album was found in. Path-derived
    /// (unlike `artist`, which prefers the embedded tag), so a caller that
    /// writes back into the library reuses the folder that already exists
    /// instead of creating a second spelling beside it. This is the recorded
    /// location used as a destination; `artist_dirs` is the full evidence set.
    pub artist_dir: String,
    /// Total number of audio files grouped into this album (all formats).
    pub track_count: usize,
    /// Number of files that fail the quality/format gate and therefore need
    /// replacing by the library-upgrade workflow. This — not `track_count` —
    /// is the completeness/peer-count reference: a mixed-format album (e.g.
    /// 12 FLAC + 12 MP3 where `allowed_extensions` is `[flac]`) needs only
    /// its 12 non-conforming files re-downloaded, and comparing a peer's
    /// FLAC group against the total 24 would reject every peer forever.
    pub needs_upgrade: usize,
    pub min_bitrate: Option<u32>,
    pub max_bitrate: Option<u32>,
    pub formats: Vec<String>,
}

/// Recognised audio file extensions that the scanner should pick up.
/// This is broader than `filters.allowed_extensions` — the scanner needs to
/// see *all* audio files so it can detect albums that contain formats outside
/// the user's quality target. Shared with the search-side library lookup so both
/// agree on what counts as a library track.
///
/// Not every entry here is scoreable: `organizer::format_from_extension` covers
/// the common formats plus `oga`/`alac`, but returns `None` for `dsf`, `dff` and
/// `mpc`, which are therefore counted as library tracks yet never scored or
/// deleted as lesser quality.
pub(crate) const KNOWN_AUDIO_EXTENSIONS: &[&str] = &[
    "flac", "mp3", "m4a", "aac", "ogg", "oga", "opus", "wav", "wma", "ape", "mpc", "wv", "aiff",
    "aif", "alac", "dsf", "dff", "spx",
];

/// How often the walk reports progress at debug level. Counted in audio files,
/// not seconds: a progress line per file would flood the log on a large library,
/// and a time-based cadence is untestable without waiting.
const SCAN_PROGRESS_EVERY_FILES: usize = 500;

/// How long the walk may run before it reports progress at info level.
///
/// The shipped `logging.level` is INFO, so a debug-only heartbeat would still
/// leave "is it working or hung?" unanswered for the minutes a real library
/// takes: one bounded line a minute shows the walk is advancing. It is checked
/// between entries, so a silence longer than this means the walk has not returned
/// from the entry it is on - which a single stalled file read can also cause, so
/// the line shows progress rather than proving health.
const SCAN_HEARTBEAT_SECS: u64 = 60;

/// Report the walk's progress at info level when the heartbeat is due, advancing
/// the clock it is given. Returns whether it reported.
///
/// Decision and emission are one function so a test can drive the schedule with
/// an injected `now`: testing the predicate alone would let a deleted or
/// downgraded emission leave the suite green. The walk's own call to this is
/// covered by inspection, because exercising it needs a scan lasting a minute.
fn maybe_report_heartbeat(
    last: &mut std::time::Instant,
    now: std::time::Instant,
    files_seen: usize,
    albums: usize,
    started: std::time::Instant,
    interval: std::time::Duration,
) -> bool {
    if !heartbeat_due(*last, now, interval) {
        return false;
    }
    *last = now;
    tracing::info!(
        "Library scan still running: {files_seen} audio file(s), {albums} album(s) ({:.0}s elapsed)",
        now.duration_since(started).as_secs_f64()
    );
    true
}

/// True when enough time has passed for another info-level progress line.
fn heartbeat_due(
    last: std::time::Instant,
    now: std::time::Instant,
    interval: std::time::Duration,
) -> bool {
    now.duration_since(last) >= interval
}

/// Tags read from one audio file, plus whether the file could be opened at all.
///
/// A readable file may legitimately carry no artist or album tag; an unreadable
/// one is a different condition and is reported separately, because grouping it
/// by folder name silently is how a corrupt file goes unnoticed.
struct AudioTags {
    artist: Option<String>,
    album: Option<String>,
    bitrate_kbps: Option<u32>,
    readable: bool,
    /// Why the file could not be read, when it could not. Carried so the debug
    /// line can distinguish a corrupt file from a permission or I/O failure.
    error: Option<String>,
}

/// Walk library directories, group audio files by artist/album, collect
/// format+bitrate info, and per-file gate status. `filters` provides the
/// allowed-extension and minimum-bitrate gate used to compute
/// [`ScannedAlbum::needs_upgrade`] for every file.
///
/// `cancel` is an optional cooperative cancellation flag, checked once per walk
/// entry; when it is set the walk stops and returns [`SeakarrError::Cancelled`],
/// so a caller can tell a user cancellation from a scan that failed. It is the
/// shared [`std::sync::atomic::AtomicBool`] rather than an `Arc` because the
/// caller owns the `Arc` and this only reads it.
///
/// The walk reports what it is doing: an info line naming the roots when it
/// starts, an info line every [`SCAN_HEARTBEAT_SECS`] while it runs, a debug line
/// every [`SCAN_PROGRESS_EVERY_FILES`] audio files, a debug line for each file
/// whose tags cannot be read, and an info line with the final counts when it
/// ends. Before this, a stalled scan was indistinguishable from a hang.
///
/// # Errors
///
/// [`SeakarrError::Cancelled`] when `cancel` is set, and
/// [`SeakarrError::Scanner`] when a configured root does not exist.
pub fn scan_library(
    library_paths: &[String],
    filters: &crate::config::FilterConfig,
    cancel: Option<&std::sync::atomic::AtomicBool>,
) -> Result<Vec<ScannedAlbum>> {
    scan_library_with_heartbeat(
        library_paths,
        filters,
        cancel,
        std::time::Duration::from_secs(SCAN_HEARTBEAT_SECS),
    )
}

/// [`scan_library`] with the heartbeat interval injected.
///
/// The interval is a parameter only so the walk's own heartbeat call can be
/// tested: exercising it through `scan_library` would need a scan lasting a
/// minute, which leaves the operator-visible line unpinned.
fn scan_library_with_heartbeat(
    library_paths: &[String],
    filters: &crate::config::FilterConfig,
    cancel: Option<&std::sync::atomic::AtomicBool>,
    heartbeat_every: std::time::Duration,
) -> Result<Vec<ScannedAlbum>> {
    let mut albums: std::collections::BTreeMap<(String, String), ScannedAlbum> =
        std::collections::BTreeMap::new();
    let ext_set: HashSet<&str> = KNOWN_AUDIO_EXTENSIONS.iter().copied().collect();
    let allowed_set: HashSet<String> = filters
        .allowed_extensions
        .iter()
        .map(|e| e.to_lowercase())
        .collect();

    // Which root supplied each album's recorded location. `library.paths` order
    // decides, so the first listed entry wins when the same album exists under
    // several roots (a backup listed afterwards must not take over).
    let mut location_root: std::collections::BTreeMap<(String, String), usize> =
        std::collections::BTreeMap::new();

    // The scan reads every audio file's tags, which is minutes of I/O on a real
    // library. Without these lines a slow scan is indistinguishable from a
    // hang: nothing else in this phase logs anything.
    tracing::info!(
        "Library scan starting: {} root(s): {}",
        library_paths.len(),
        library_paths.join(", ")
    );
    let scan_started = std::time::Instant::now();
    let mut last_heartbeat = scan_started;
    let mut files_seen: usize = 0;
    let mut unreadable: usize = 0;
    // Acquire, which is sufficient alongside every other reader of this flag
    // (the listener stores with SeqCst and downloads load with SeqCst). Relaxed
    // would also do for a cooperative check, but a lone weaker ordering invites
    // the question of whether it was a mistake.
    let cancelled = || cancel.is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Acquire));

    for (root_index, lib_path_str) in library_paths.iter().enumerate() {
        let lib_path = Path::new(lib_path_str);
        if !lib_path.exists() {
            return Err(SeakarrError::Scanner(format!(
                "library path does not exist: {lib_path_str}"
            )));
        }
        for entry in WalkDir::new(lib_path)
            .follow_links(false)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            // Checked before the entry is classified, so a tree dominated by
            // non-audio files is still interruptible, and so the count in the
            // cancellation message is what has actually been handled rather
            // than one file more.
            if cancelled() {
                tracing::info!("Library scan cancelled by user after {files_seen} audio file(s)");
                return Err(SeakarrError::Cancelled);
            }
            // Time-based and therefore bounded, and evaluated per walk entry
            // rather than per audio file: a tree dominated by non-audio entries
            // walks for minutes without touching a single tag, and must still
            // show movement at the level operators actually run with.
            maybe_report_heartbeat(
                &mut last_heartbeat,
                std::time::Instant::now(),
                files_seen,
                albums.len(),
                scan_started,
                heartbeat_every,
            );
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_lowercase();

            if !ext_set.contains(ext.as_str()) {
                continue;
            }

            files_seen += 1;
            if files_seen.is_multiple_of(SCAN_PROGRESS_EVERY_FILES) {
                tracing::debug!(
                    "Library scan: {files_seen} audio file(s), {} album(s) so far ({:.0}s)",
                    albums.len(),
                    scan_started.elapsed().as_secs_f64()
                );
            }

            // Infer the artist folder, album folder, and library location from
            // the directory structure. The album folder is the one holding the
            // files, one dedicated disc folder is stepped over so a multi-disc
            // album resolves to its album folder, and the artist folder is the
            // one directly above it. For a nested layout such as
            // <root>/Genre/Artist/Album this keeps every component correct
            // instead of naming the genre after the artist.
            let relative = path.strip_prefix(lib_path).unwrap_or(path);
            let components: Vec<&str> = relative.iter().filter_map(|c| c.to_str()).collect();

            // A folder named like a disc is stepped over so a multi-disc album
            // resolves to its album folder, matching the peer-side rule. The
            // folder holding the files is read as a disc whenever a component
            // above it can serve as the album, so
            // <root>/Genre/Artist/CD 01/track.flac resolves to the artist
            // "Genre" with the album "Artist"; only at the minimum depth,
            // <root>/Artist/CD 01/track.flac, is the disc-named folder itself
            // read as the album, because peeling there would leave the artist
            // component with nothing above it to be the artist (the peer-side
            // parser refuses that shape outright). Anything with no album
            // component left at all is not a library album layout and is
            // skipped.
            let album_index = match crate::discs::album_index(&components) {
                Some(index) => index,
                _ => match components.len().checked_sub(2) {
                    Some(index) if index >= 1 => index,
                    _ => continue, // Need at least Artist/Album/file
                },
            };
            let artist_dir = components[album_index - 1].to_string();
            // The on-disk album folder name, kept verbatim. Stripping an
            // embedded disc marker here would move the write into a new folder
            // and leave the replaced files behind in the old one — the album
            // would then be re-upgraded on every run. Merging marker-variant
            // folders into one album is a presence and identity concern, not a
            // path concern.
            //
            // Note that the auto-mode upgrade destination comes from the album
            // TAG (`final_album` below), not from this name, so a tag that
            // differs from the folder already sent that write to a sibling
            // folder before the portable-name sanitiser existed. The current
            // sanitiser is a second cause of the same divergence; the
            // `library_upgrade` section of the README documents the consequence.
            let album_dir = components[album_index].to_string();

            // Read audio tags if available
            let tags = read_audio_tags(path);
            if !tags.readable {
                unreadable += 1;
                tracing::debug!(
                    "Library scan: cannot read tags from {} ({}); grouping it by folder name",
                    path.display(),
                    tags.error.as_deref().unwrap_or("unknown reason")
                );
            }
            let (tag_artist, tag_album, bitrate) = (tags.artist, tags.album, tags.bitrate_kbps);

            // Whether THIS file fails the quality gate: a non-allowed format,
            // or a bitrate below the configured minimum (unknown bitrate is
            // treated as failing — the album is flagged for upgrade because
            // its quality cannot be verified). Files that already conform are
            // NOT part of the upgrade — they must not inflate the baseline
            // that peer-track-count and the completeness gate compare against.
            let file_needs_upgrade = !allowed_set.contains(ext.as_str())
                || (filters.min_bit_rate > 0
                    && (bitrate.is_none() || bitrate.unwrap() < filters.min_bit_rate));

            // Prefer tag metadata over directory name
            let final_artist = tag_artist.unwrap_or_else(|| artist_dir.clone());
            let final_album = tag_album.unwrap_or_else(|| album_dir.clone());

            let key = (final_artist.clone(), final_album.clone());
            // The album's real location inside the library: the directory ABOVE
            // the artist/album folders. For a standard <root>/Artist/Album
            // layout this is the library root itself; for nested layouts (e.g.
            // <root>/Genre/Artist/Album) it is <root>/Genre. Threaded to the
            // runner so the library upgrade copies new files back into the
            // exact directory the album was found in — not the root of the
            // library path.
            let album_location = components[..album_index - 1]
                .iter()
                .fold(lib_path.to_path_buf(), |acc, c| acc.join(c));
            // The same artist/album key can appear under several roots, and under
            // one root in several folders. The earliest listed root wins, and
            // inside one root the lexicographically smallest folder wins, so the
            // recorded pair (used by placement and by the auto-mode upgrade root)
            // never depends on filesystem walk order.
            let take_location = match albums.get(&key) {
                None => true,
                Some(existing) => match location_root.get(&key) {
                    // An earlier root already supplied the location.
                    Some(taken) if *taken < root_index => false,
                    Some(taken) if *taken == root_index => {
                        (&album_location, &artist_dir) < (&existing.path, &existing.artist_dir)
                    }
                    _ => true,
                },
            };
            if take_location {
                location_root.insert(key.clone(), root_index);
            }

            albums
                .entry(key)
                .and_modify(|a| {
                    if take_location {
                        a.path = album_location.clone();
                        a.artist_dir = artist_dir.clone();
                    }
                    // Every folder holding this album is recorded, not just the
                    // recorded location's: presence accepts any of them, and a
                    // set does not depend on the order the walk visited them.
                    a.album_dirs.insert(album_dir.clone());
                    a.artist_dirs.insert(artist_dir.clone());
                    a.track_count += 1;
                    if file_needs_upgrade {
                        a.needs_upgrade += 1;
                    }
                    if let Some(br) = bitrate {
                        a.min_bitrate = Some(a.min_bitrate.map_or(br, |m| m.min(br)));
                        a.max_bitrate = Some(a.max_bitrate.map_or(br, |m| m.max(br)));
                    }
                    if !a.formats.contains(&ext) {
                        a.formats.push(ext.clone());
                    }
                })
                .or_insert_with(|| ScannedAlbum {
                    path: album_location,
                    artist: final_artist,
                    album: final_album,
                    album_dirs: std::collections::BTreeSet::from([album_dir.clone()]),
                    artist_dirs: std::collections::BTreeSet::from([artist_dir.clone()]),
                    artist_dir: artist_dir.clone(),
                    track_count: 1,
                    needs_upgrade: usize::from(file_needs_upgrade),
                    min_bitrate: bitrate,
                    max_bitrate: bitrate,
                    formats: vec![ext],
                });
        }
    }

    let album_count = albums.len();
    tracing::info!(
        "Library scan complete: {files_seen} audio file(s), {album_count} album(s), {unreadable} unreadable file(s) in {:.1}s",
        scan_started.elapsed().as_secs_f64()
    );
    Ok(albums.into_values().collect())
}

/// Read artist, album and bitrate from an audio file using lofty, and report
/// whether the file could be opened at all. A readable file may carry no tags;
/// that is not the same as a file that cannot be read, which the scan counts and
/// names at debug level instead of silently grouping it by folder name.
fn read_audio_tags(path: &Path) -> AudioTags {
    let tagged_file = match lofty::probe::Probe::open(path) {
        Ok(probe) => match probe.read() {
            Ok(file) => file,
            Err(error) => return unreadable(error),
        },
        Err(error) => return unreadable(error),
    };

    let tag = tagged_file
        .primary_tag()
        .or_else(|| tagged_file.first_tag());

    let artist = tag.and_then(|t| t.artist().map(|a| a.to_string()));
    let album = tag.and_then(|t| t.album().map(|a| a.to_string()));

    // lofty 0.21 reports the audio bitrate in kbps
    let bitrate = tagged_file.properties().audio_bitrate();

    AudioTags {
        artist,
        album,
        bitrate_kbps: bitrate,
        readable: true,
        error: None,
    }
}

/// The values a caller gets for a file whose tags could not be read, with the
/// reason kept for the debug line.
fn unreadable(error: impl std::fmt::Display) -> AudioTags {
    AudioTags {
        artist: None,
        album: None,
        bitrate_kbps: None,
        readable: false,
        error: Some(error.to_string()),
    }
}

/// Determine which albums need upgrading based on filter config.
/// An album is flagged if any track is in a non-allowed format, or when
/// `min_bit_rate` is set and the album's lowest reported bitrate is missing or
/// below it. A file with no reported bitrate counts towards `needs_upgrade`
/// per file, but only flags the album when no file reports one.
/// Returns (artist, album, replacement_count, library_location) where
/// `replacement_count` is the number of files failing the quality gate
/// (computed per-file during the scan, see [`ScannedAlbum::needs_upgrade`]) —
/// NOT the album's total track count. The library location (the directory the
/// album was found in; for a standard <root>/Artist/Album layout this is the
/// library root, for nested layouts it is the directory above the artist
/// folder) is threaded to the runner so auto-mode copies upgrades back into
/// the album's real location.
pub fn find_albums_to_upgrade(
    albums: &[ScannedAlbum],
    config: &FilterConfig,
) -> Vec<(String, String, usize, PathBuf)> {
    let allowed_set: HashSet<String> = config
        .allowed_extensions
        .iter()
        .map(|e| e.to_lowercase())
        .collect();

    albums
        .iter()
        .filter(|a| {
            // Check: any format not in allowed list?
            let wrong_format = a.formats.iter().any(|f| !allowed_set.contains(f));
            if wrong_format {
                return true;
            }

            // Check: bitrate below minimum? Treat unknown bitrate (None)
            // as needing upgrade — we can't verify quality without tags.
            if config.min_bit_rate > 0 {
                match a.min_bitrate {
                    None => return true, // unknown quality → flags whole album
                    Some(album_min) if album_min < config.min_bit_rate => return true,
                    _ => {}
                }
            }

            false
        })
        .map(|a| {
            (
                a.artist.clone(),
                a.album.clone(),
                a.needs_upgrade,
                a.path.clone(),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{write_minimal_flac, write_minimal_flac_with_tags, LogCapture};
    use std::fs;
    use tempfile::TempDir;

    /// Helper: wrap a temp dir path as the single-element library paths list
    /// expected by `scan_library`.
    fn library_paths(dir: &std::path::Path) -> Vec<String> {
        vec![dir.to_string_lossy().into_owned()]
    }

    /// A library with `albums` albums of two files each, all named so the walk
    /// sees them as `<root>/Artist/Album/0N - track.flac`.
    fn library_with_albums(albums: usize) -> TempDir {
        let dir = TempDir::new().unwrap();
        for index in 0..albums {
            let album_dir = dir.path().join("Artist").join(format!("Album {index}"));
            fs::create_dir_all(&album_dir).unwrap();
            write_minimal_flac_with_tags(
                &album_dir.join("01 - track.flac"),
                "Artist",
                &format!("Album {index}"),
            );
            write_minimal_flac_with_tags(
                &album_dir.join("02 - track.flac"),
                "Artist",
                &format!("Album {index}"),
            );
        }
        dir
    }

    // ── The scan must say what it is doing ──
    //
    // A library scan reads every audio file's tags on an Unraid user share and
    // can take minutes; when it stalled, nothing at all was logged at any level
    // so a slow scan was indistinguishable from a hang. These tests pin the
    // output that makes the phase observable.

    #[test]
    fn scan_reports_a_start_line_naming_its_roots() {
        let dir = library_with_albums(1);
        let capture = LogCapture::start();

        let albums =
            scan_library(&library_paths(dir.path()), &FilterConfig::default(), None).unwrap();

        assert_eq!(albums.len(), 1);
        let logs = capture.text();
        let root = dir.path().to_string_lossy();
        assert!(
            logs.lines()
                .any(|line| line.contains("Library scan starting") && line.contains(root.as_ref())),
            "the scan must name the roots it is about to walk, got:\n{logs}"
        );
    }

    #[test]
    fn scan_reports_a_finish_line_with_file_and_album_counts() {
        let dir = library_with_albums(7);
        let capture = LogCapture::start();

        let albums =
            scan_library(&library_paths(dir.path()), &FilterConfig::default(), None).unwrap();

        assert_eq!(albums.len(), 7);
        let logs = capture.text();
        // The whole line, and this fixture's own counts: a bare `14 audio file(s)`
        // substring would also be satisfied by `114 audio file(s)`, and finding
        // the first matching line in the shared process-wide capture window could
        // return a neighbouring test's scan.
        assert!(
            logs.lines().any(|line| line
                .contains("Library scan complete: 14 audio file(s), 7 album(s), 0 unreadable")),
            "the completion line must carry this scan's own counts, got:\n{logs}"
        );
    }

    #[test]
    fn scan_counts_and_names_files_whose_tags_cannot_be_read() {
        let dir = library_with_albums(1);
        let broken = dir
            .path()
            .join("Artist")
            .join("Broken Album")
            .join("01 - track.flac");
        fs::create_dir_all(broken.parent().unwrap()).unwrap();
        fs::write(&broken, b"this is not a flac stream").unwrap();
        let capture = LogCapture::start();

        let albums =
            scan_library(&library_paths(dir.path()), &FilterConfig::default(), None).unwrap();

        assert_eq!(
            albums.len(),
            2,
            "an unreadable file is still grouped by folder"
        );
        let logs = capture.text();
        assert!(
            logs.lines().any(|line| line
                .contains("Library scan complete: 3 audio file(s), 2 album(s), 1 unreadable")),
            "an unreadable file must be counted in this scan's own completion line, got:\n{logs}"
        );
        // Keyed to this fixture's own path: "01 - track.flac" is shared by a
        // dozen other fixtures in this module.
        assert!(
            logs.contains(&broken.display().to_string()) && logs.contains("cannot read tags from"),
            "the unreadable file must be named individually at debug level, got:\n{logs}"
        );
    }

    #[test]
    fn scan_reports_progress_every_five_hundred_files() {
        // 501 single-file albums trip the first debug progress line at the
        // documented cadence, and the line must carry that exact count.
        let dir = TempDir::new().unwrap();
        for index in 0..=500 {
            let album_dir = dir.path().join("Artist").join(format!("Album {index}"));
            fs::create_dir_all(&album_dir).unwrap();
            fs::write(album_dir.join("01 - track.flac"), b"fake flac data").unwrap();
        }
        let capture = LogCapture::start();

        scan_library(&library_paths(dir.path()), &FilterConfig::default(), None).unwrap();

        let logs = capture.text();
        assert!(
            logs.lines()
                .any(|line| line.contains("Library scan: 500 audio file(s),")),
            "the progress line must report the first cadence point exactly, got:\n{logs}"
        );
    }

    #[test]
    fn the_walk_itself_emits_the_heartbeat() {
        // Pins the walk's own call, not just the helper: with a zero interval the
        // first entry that reaches the check reports, so a deleted or relocated
        // call fails here. This is the line the incident needed to see.
        let dir = library_with_albums(1);
        let capture = LogCapture::start();

        scan_library_with_heartbeat(
            &library_paths(dir.path()),
            &FilterConfig::default(),
            None,
            std::time::Duration::ZERO,
        )
        .unwrap();

        let logs = capture.text();
        // With a zero interval every walk entry reports, and the check runs
        // before the entry is counted, so the highest count reached by a two-file
        // library is one file and its one album. No other test can emit a
        // heartbeat line at all: they run with the real 60-second interval.
        assert!(
            logs.lines().any(|line| line.contains("INFO")
                && line.contains("Library scan still running: 1 audio file(s), 1 album(s)")),
            "the walk must emit the heartbeat itself, got:\n{logs}"
        );
    }

    #[test]
    fn the_heartbeat_reports_once_a_minute_and_only_when_due() {
        // Drives the schedule with an injected clock, so both halves are pinned:
        // a heartbeat emitted too early, and an emission deleted or downgraded
        // below info, both fail here.
        let capture = LogCapture::start();
        // `elapsed` is measured from `started`, so the fixture backdates it: the
        // scan began 59 s before the first probe and 61 s before the second.
        let now = std::time::Instant::now();
        let started = now - std::time::Duration::from_secs(SCAN_HEARTBEAT_SECS - 1);
        let mut last = started;
        let due = now + std::time::Duration::from_secs(2);

        assert!(
            !maybe_report_heartbeat(
                &mut last,
                now,
                2,
                1,
                started,
                std::time::Duration::from_secs(SCAN_HEARTBEAT_SECS)
            ),
            "a heartbeat before the interval must not report"
        );
        assert!(
            maybe_report_heartbeat(
                &mut last,
                due,
                3,
                2,
                started,
                std::time::Duration::from_secs(SCAN_HEARTBEAT_SECS)
            ),
            "a heartbeat once the interval has passed must report"
        );
        assert_eq!(last, due, "the clock must advance to the reporting instant");
        assert!(
            !maybe_report_heartbeat(
                &mut last,
                due,
                4,
                2,
                started,
                std::time::Duration::from_secs(SCAN_HEARTBEAT_SECS)
            ),
            "an already-reported clock must not report again"
        );

        let logs = capture.text();
        assert_eq!(
            logs.matches("Library scan still running").count(),
            1,
            "exactly one heartbeat line must be emitted, got:\n{logs}"
        );
        assert!(
            logs.lines().any(|line| line.contains("INFO")
                && line.contains(
                    "Library scan still running: 3 audio file(s), 2 album(s) (61s elapsed)"
                )),
            "the heartbeat must be emitted at INFO with its counts, got:\n{logs}"
        );
    }

    #[test]
    fn a_scan_heartbeats_once_a_minute_at_info_level() {
        // The predicate is exercised through the schedule test above; this keeps
        // the interval itself pinned to the documented constant.
        let start = std::time::Instant::now();
        let interval = std::time::Duration::from_secs(SCAN_HEARTBEAT_SECS);
        assert!(!heartbeat_due(start, start, interval));
        assert!(!heartbeat_due(
            start,
            start + std::time::Duration::from_secs(SCAN_HEARTBEAT_SECS - 1),
            interval
        ));
        assert!(heartbeat_due(
            start,
            start + std::time::Duration::from_secs(SCAN_HEARTBEAT_SECS),
            interval
        ));
    }

    #[test]
    fn scan_stops_when_the_run_is_cancelled() {
        // Ctrl+C during a slow scan must stop it. Before this change the flag was
        // never consulted and the walk ran to completion, because the scan ran
        // before any signal listener existed.
        let dir = library_with_albums(3);
        let cancel = std::sync::atomic::AtomicBool::new(true);
        let capture = LogCapture::start();

        let outcome = scan_library(
            &library_paths(dir.path()),
            &FilterConfig::default(),
            Some(&cancel),
        );

        assert!(
            matches!(outcome, Err(SeakarrError::Cancelled)),
            "a cancelled scan must stop and say so, got: {outcome:?}"
        );
        // Exact count: the flag is checked before the first entry is classified,
        // so nothing has been handled yet.
        assert!(
            capture
                .text()
                .contains("Library scan cancelled by user after 0 audio file(s)"),
            "the cancellation must name the progress it reached, got:\n{}",
            capture.text()
        );
    }

    #[test]
    fn test_scan_empty_directory() {
        let dir = TempDir::new().unwrap();
        let albums =
            scan_library(&library_paths(dir.path()), &FilterConfig::default(), None).unwrap();
        assert!(albums.is_empty());
    }

    #[test]
    fn test_scan_directory_structure() {
        let dir = TempDir::new().unwrap();
        // Create artist/album/track structure
        let album_dir = dir.path().join("Test Artist").join("Test Album");
        fs::create_dir_all(&album_dir).unwrap();
        fs::write(album_dir.join("01 - Song One.flac"), b"fake flac data").unwrap();
        fs::write(album_dir.join("02 - Song Two.flac"), b"fake flac data").unwrap();

        // Another artist with MP3 — now discovered because the scanner picks
        // up all known audio formats (upgrade detection handles the filter).
        let mp3_dir = dir.path().join("Other Artist").join("Other Album");
        fs::create_dir_all(&mp3_dir).unwrap();
        fs::write(mp3_dir.join("track.mp3"), b"fake mp3 data").unwrap();

        let albums =
            scan_library(&library_paths(dir.path()), &FilterConfig::default(), None).unwrap();
        assert_eq!(albums.len(), 2);
        // Both albums should be present
        let artists: Vec<&str> = albums.iter().map(|a| a.artist.as_str()).collect();
        assert!(artists.contains(&"Test Artist"));
        assert!(artists.contains(&"Other Artist"));
    }

    #[test]
    fn test_find_albums_to_upgrade_below_bitrate() {
        let albums = vec![
            ScannedAlbum {
                path: PathBuf::new(),
                artist: "Artist1".into(),
                album: "Album1".into(),
                album_dirs: ["Album1".to_string()].into_iter().collect(),
                artist_dirs: ["Artist1".to_string()].into_iter().collect(),
                artist_dir: "Artist1".into(),
                track_count: 3,
                needs_upgrade: 3,
                min_bitrate: Some(128),
                max_bitrate: Some(192),
                formats: vec!["mp3".into()],
            },
            ScannedAlbum {
                path: PathBuf::new(),
                artist: "Artist2".into(),
                album: "Album2".into(),
                album_dirs: ["Album2".to_string()].into_iter().collect(),
                artist_dirs: ["Artist2".to_string()].into_iter().collect(),
                artist_dir: "Artist2".into(),
                track_count: 5,
                needs_upgrade: 0,
                min_bitrate: Some(900),
                max_bitrate: Some(1200),
                formats: vec!["flac".into()],
            },
        ];

        let config = crate::config::FilterConfig {
            allowed_extensions: vec!["flac".into()],
            min_bit_rate: 320,
            min_bit_depth: 0,
            exclude_words: vec![],
            include_locked: false,
            contiguous_tracks: true,
            min_tracks: 3,
            peer_track_count: true,
        };

        let to_upgrade = find_albums_to_upgrade(&albums, &config);
        // Artist1: mp3 (not in allowed extensions) → upgrade
        // Artist2: flac, min bitrate 900 (above 320) → no upgrade
        assert_eq!(to_upgrade.len(), 1);
        assert_eq!(to_upgrade[0].0, "Artist1");
        assert_eq!(to_upgrade[0].1, "Album1");
        assert_eq!(to_upgrade[0].2, 3); // needs_upgrade: all 3 mp3 files fail the gate
    }

    #[test]
    fn test_find_albums_to_upgrade_returns_library_path() {
        let dir = TempDir::new().unwrap();
        // Create Artist/Album with an ogg track — ogg is not in the allowed
        // [flac] list, so the album is flagged for upgrade and must carry its
        // origin library root path for the runner to copy upgrades back to.
        let album_dir = dir.path().join("Artist").join("Album");
        fs::create_dir_all(&album_dir).unwrap();
        fs::write(album_dir.join("01 - Track.ogg"), b"fake ogg data").unwrap();

        let albums =
            scan_library(&library_paths(dir.path()), &FilterConfig::default(), None).unwrap();
        let config = crate::config::FilterConfig {
            allowed_extensions: vec!["flac".into()],
            min_bit_rate: 0,
            min_bit_depth: 0,
            exclude_words: vec![],
            include_locked: false,
            contiguous_tracks: true,
            min_tracks: 3,
            peer_track_count: true,
        };

        let to_upgrade = find_albums_to_upgrade(&albums, &config);
        assert_eq!(to_upgrade.len(), 1);
        assert_eq!(to_upgrade[0].0, "Artist");
        assert_eq!(to_upgrade[0].1, "Album");
        assert_eq!(to_upgrade[0].2, 1); // needs_upgrade: single ogg file fails the gate
        assert_eq!(to_upgrade[0].3, dir.path().to_path_buf()); // library root
    }

    #[test]
    fn test_find_albums_to_upgrade_preserves_nested_album_location() {
        let dir = TempDir::new().unwrap();
        // Nested library layout: <root>/Pop/Alesha Dixon/The Alesha Show/01 - Track.ogg
        // The album's real location is one level below the library root (inside
        // the "Pop" genre subdirectory). The path returned for the upgrade must
        // be the directory ABOVE the artist folder (<root>/Pop), NOT the library
        // root — otherwise the upgrade copy lands at the root of the library
        // path instead of where the album actually lives.
        let album_dir = dir
            .path()
            .join("Pop")
            .join("Alesha Dixon")
            .join("The Alesha Show");
        fs::create_dir_all(&album_dir).unwrap();
        fs::write(album_dir.join("01 - Track.ogg"), b"fake ogg data").unwrap();

        let albums =
            scan_library(&library_paths(dir.path()), &FilterConfig::default(), None).unwrap();
        let config = crate::config::FilterConfig {
            allowed_extensions: vec!["flac".into()],
            min_bit_rate: 0,
            min_bit_depth: 0,
            exclude_words: vec![],
            include_locked: false,
            contiguous_tracks: true,
            min_tracks: 3,
            peer_track_count: true,
        };

        let to_upgrade = find_albums_to_upgrade(&albums, &config);
        assert_eq!(to_upgrade.len(), 1);
        // Artist and album now come from the two folders above the file, so a
        // nested layout no longer reports the genre as the artist.
        assert_eq!(to_upgrade[0].0, "Alesha Dixon"); // artist folder, not the genre
        assert_eq!(to_upgrade[0].1, "The Alesha Show"); // album folder
        assert_eq!(to_upgrade[0].2, 1); // needs_upgrade: single ogg file fails the gate
                                        // The upgrade target must be the directory above the artist folder
                                        // (<root>/Pop), not the library root — this is what preserves the
                                        // album's real location inside the library.
        assert_eq!(to_upgrade[0].3, dir.path().join("Pop"));
    }

    #[test]
    fn test_one_root_holding_two_folders_for_the_same_album_keeps_the_smallest() {
        // Inside one root the same artist/album key (here via tags) can come from
        // two folders. The lexicographically smaller location wins, so the
        // recorded pair never depends on walk order.
        let dir = TempDir::new().unwrap();
        for genre in ["B", "A"] {
            // The genre folder is the album location, and the tag fixes the key,
            // so both copies are one album with two candidate locations.
            let album_dir = dir.path().join(genre).join("Tagged").join("Album");
            fs::create_dir_all(&album_dir).unwrap();
            write_minimal_flac_with_tags(&album_dir.join("01 - track.flac"), "Tagged", "Album");
        }

        let albums =
            scan_library(&library_paths(dir.path()), &FilterConfig::default(), None).unwrap();

        assert_eq!(albums.len(), 1, "the tagged copies are one album");
        assert_eq!(
            albums[0].path,
            dir.path().join("A"),
            "the smaller location wins"
        );
        assert_eq!(albums[0].artist_dir, "Tagged");
        assert_eq!(albums[0].track_count, 2);
    }

    #[test]
    fn test_a_merged_album_records_every_folder_holding_it() {
        // The same tagged album in two differently named folders is one album
        // with one recorded location, but presence has to accept either folder
        // name: a single recorded name would be whichever the unsorted walk saw
        // first, so a target titled after the other folder would look missing and
        // the album would be downloaded again.
        let dir = TempDir::new().unwrap();
        for folder in ["Second Folder", "First Folder"] {
            let album_dir = dir.path().join("Artist").join(folder);
            fs::create_dir_all(&album_dir).unwrap();
            write_minimal_flac_with_tags(&album_dir.join("01 - track.flac"), "Artist", "Album");
        }

        let albums =
            scan_library(&library_paths(dir.path()), &FilterConfig::default(), None).unwrap();

        assert_eq!(albums.len(), 1, "the tagged copies are one album");
        assert_eq!(albums[0].track_count, 2);
        assert_eq!(
            albums[0].album_dirs.iter().cloned().collect::<Vec<_>>(),
            ["First Folder", "Second Folder"],
            "both folder names are recorded, independent of walk order"
        );
    }

    #[test]
    fn test_the_first_listed_root_wins_over_a_lower_sorting_one() {
        // `library.paths` order is the primary rule, so a backup directory listed
        // second must not take over just because its path sorts first.
        let first = TempDir::new().unwrap();
        let second = TempDir::new().unwrap();
        let (smaller, larger) = if first.path() < second.path() {
            (first.path(), second.path())
        } else {
            (second.path(), first.path())
        };
        for root in [smaller, larger] {
            let album_dir = root.join("Artist").join("Album");
            fs::create_dir_all(&album_dir).unwrap();
            fs::write(album_dir.join("01 - track.flac"), b"fake flac data").unwrap();
        }

        let albums = scan_library(
            &[
                larger.to_string_lossy().into_owned(),
                smaller.to_string_lossy().into_owned(),
            ],
            &FilterConfig::default(),
            None,
        )
        .unwrap();

        assert_eq!(albums.len(), 1);
        assert_eq!(
            albums[0].path, larger,
            "the first listed root wins, regardless of path ordering"
        );
    }

    #[test]
    fn test_find_albums_to_upgrade_ignores_bit_depth() {
        // Auto mode's upgrade scan reads formats and bitrate only, so a library of
        // 16-bit FLAC must not be flagged by `min_bit_depth: 24`; the setting only
        // rejects candidates at filter/verify time.
        let dir = TempDir::new().unwrap();
        let album_dir = dir.path().join("Artist").join("Album");
        fs::create_dir_all(&album_dir).unwrap();
        write_minimal_flac(&album_dir.join("01 - track.flac"));

        let filters = FilterConfig {
            min_bit_depth: 24,
            ..FilterConfig::default()
        };
        let albums = scan_library(&library_paths(dir.path()), &filters, None).unwrap();
        assert!(
            find_albums_to_upgrade(&albums, &filters).is_empty(),
            "bit depth must not flag an album for upgrade"
        );
    }

    #[test]
    fn test_find_albums_to_upgrade_wrong_format() {
        let albums = vec![ScannedAlbum {
            path: PathBuf::new(),
            artist: "Artist".into(),
            album: "Album".into(),
            album_dirs: ["Album".to_string()].into_iter().collect(),
            artist_dirs: ["Artist".to_string()].into_iter().collect(),
            artist_dir: "Artist".into(),
            track_count: 2,
            needs_upgrade: 2,
            min_bitrate: Some(320),
            max_bitrate: Some(320),
            formats: vec!["mp3".into()],
        }];

        let config = crate::config::FilterConfig {
            allowed_extensions: vec!["flac".into()],
            min_bit_rate: 0,
            min_bit_depth: 0,
            exclude_words: vec![],
            include_locked: false,
            contiguous_tracks: true,
            min_tracks: 3,
            peer_track_count: true,
        };

        let to_upgrade = find_albums_to_upgrade(&albums, &config);
        assert_eq!(to_upgrade.len(), 1); // mp3 should trigger upgrade (not flac)
        assert_eq!(to_upgrade[0].0, "Artist");
        assert_eq!(to_upgrade[0].2, 2); // needs_upgrade: both mp3 files fail the gate
    }

    /// Regression (release-review round 2, P1): the upgrade baseline must be
    /// the number of files that FAIL the quality gate, not the album's total
    /// audio-file count. A mixed-format album (12 FLAC + 12 MP3 with
    /// `allowed_extensions: [flac]`) needs only its 12 MP3s re-downloaded;
    /// comparing a peer's FLAC group against the total 24 would reject every
    /// peer forever and block the upgrade.
    #[test]
    fn test_mixed_format_album_baseline_counts_only_files_needing_upgrade() {
        let dir = TempDir::new().unwrap();
        let album_dir = dir.path().join("Test Artist").join("Test Album");
        fs::create_dir_all(&album_dir).unwrap();
        for n in 1..=12 {
            fs::write(
                album_dir.join(format!("{n:02} - track.flac")),
                b"fake flac data",
            )
            .unwrap();
            fs::write(
                album_dir.join(format!("{n:02} - old.mp3")),
                b"fake mp3 data",
            )
            .unwrap();
        }

        let albums =
            scan_library(&library_paths(dir.path()), &FilterConfig::default(), None).unwrap();
        assert_eq!(albums.len(), 1);
        let album = &albums[0];
        assert_eq!(album.track_count, 24);
        assert_eq!(
            album.needs_upgrade, 12,
            "only the 12 non-allowed MP3 files need replacing"
        );

        let config = crate::config::FilterConfig {
            allowed_extensions: vec!["flac".into()],
            min_bit_rate: 0,
            min_bit_depth: 0,
            exclude_words: vec![],
            include_locked: false,
            contiguous_tracks: true,
            min_tracks: 0,
            peer_track_count: true,
        };
        let to_upgrade = find_albums_to_upgrade(&albums, &config);
        assert_eq!(to_upgrade.len(), 1);
        // The runner's peer-track-count / completeness gates use this count:
        // a peer offering the 12 replacement FLACs is accepted.
        assert_eq!(to_upgrade[0].2, 12);
    }

    #[test]
    fn test_scan_resolves_nested_layout_names_positionally() {
        let dir = TempDir::new().unwrap();
        let album_dir = dir.path().join("Genre").join("Artist").join("Album");
        fs::create_dir_all(&album_dir).unwrap();
        fs::write(album_dir.join("01 - track.flac"), b"fake flac data").unwrap();

        let albums =
            scan_library(&library_paths(dir.path()), &FilterConfig::default(), None).unwrap();
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

        let albums =
            scan_library(&library_paths(dir.path()), &FilterConfig::default(), None).unwrap();
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

        let albums =
            scan_library(&library_paths(dir.path()), &FilterConfig::default(), None).unwrap();
        assert_eq!(albums.len(), 1);
        assert_eq!(albums[0].artist, "Artist");
        assert_eq!(albums[0].album, "Album");
        assert_eq!(albums[0].artist_dir, "Artist");
        assert_eq!(
            albums[0].album_dirs.iter().collect::<Vec<_>>(),
            ["Album"],
            "the album folder, never the stepped-over disc folder, is the presence key"
        );
        assert_eq!(albums[0].path, dir.path().join("Genre"));
    }

    #[test]
    fn test_scan_resolves_a_deeply_nested_layout() {
        let dir = TempDir::new().unwrap();
        let album_dir = dir
            .path()
            .join("Genre")
            .join("Style")
            .join("Artist")
            .join("Album");
        fs::create_dir_all(&album_dir).unwrap();
        fs::write(album_dir.join("01 - track.flac"), b"fake flac data").unwrap();

        let albums =
            scan_library(&library_paths(dir.path()), &FilterConfig::default(), None).unwrap();
        assert_eq!(albums.len(), 1);
        assert_eq!(albums[0].artist, "Artist");
        assert_eq!(albums[0].album, "Album");
        assert_eq!(albums[0].artist_dir, "Artist");
        assert_eq!(albums[0].path, dir.path().join("Genre").join("Style"));
    }

    #[test]
    fn test_scan_reads_a_non_disc_subfolder_as_the_album() {
        // The album component is the folder that holds the file (one dedicated
        // disc folder is stepped over), so a format or extra sub-folder inside
        // the album folder shifts the reading: the sub-folder becomes the album
        // and its parent becomes the artist. Pinned because untagged auto-mode
        // queries and the discover destination pair both derive from this.
        let dir = TempDir::new().unwrap();
        let album_dir = dir.path().join("Artist").join("Album").join("FLAC");
        fs::create_dir_all(&album_dir).unwrap();
        fs::write(album_dir.join("01 - track.flac"), b"fake flac data").unwrap();

        let albums =
            scan_library(&library_paths(dir.path()), &FilterConfig::default(), None).unwrap();
        assert_eq!(albums.len(), 1);
        assert_eq!(albums[0].artist, "Album");
        assert_eq!(albums[0].album, "FLAC");
        assert_eq!(albums[0].artist_dir, "Album");
        assert_eq!(albums[0].path, dir.path().join("Artist"));
    }

    #[test]
    fn test_scan_skips_a_file_without_an_artist_and_album_folder() {
        // A file at the library root cannot supply an artist folder, an album
        // folder, and a file, so it must not enter the index.
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("01 - track.flac"), b"fake flac data").unwrap();

        let albums =
            scan_library(&library_paths(dir.path()), &FilterConfig::default(), None).unwrap();
        assert!(albums.is_empty());
    }

    #[test]
    fn test_scan_keeps_a_disc_named_folder_directly_under_the_artist() {
        let dir = TempDir::new().unwrap();
        let album_dir = dir.path().join("Artist").join("CD 01");
        fs::create_dir_all(&album_dir).unwrap();
        fs::write(album_dir.join("01 - track.flac"), b"fake flac data").unwrap();

        let albums =
            scan_library(&library_paths(dir.path()), &FilterConfig::default(), None).unwrap();
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
        // Regression: the album name is the identity the auto-mode upgrade
        // copies into and the root the quality-deletion pass walks. Stripping
        // the marker here moves the copy into a new folder while the replaced
        // files stay in the old one, so the album was re-upgraded every run.
        let dir = TempDir::new().unwrap();
        let album_dir = dir.path().join("Artist").join("Gold (Disc 1)");
        fs::create_dir_all(&album_dir).unwrap();
        fs::write(album_dir.join("01 - track.flac"), b"fake flac data").unwrap();

        let albums =
            scan_library(&library_paths(dir.path()), &FilterConfig::default(), None).unwrap();
        assert_eq!(albums.len(), 1);
        assert_eq!(albums[0].artist, "Artist");
        assert_eq!(albums[0].album, "Gold (Disc 1)");
        assert_eq!(albums[0].artist_dir, "Artist");
        assert_eq!(albums[0].path, dir.path().to_path_buf());
    }
}
