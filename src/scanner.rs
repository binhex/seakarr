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
    pub artist: String,
    pub album: String,
    /// On-disk name of the artist folder this album was found in. Path-derived
    /// (unlike `artist`, which prefers the embedded tag), so a caller that
    /// writes back into the library reuses the folder that already exists
    /// instead of creating a second spelling beside it.
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

/// Walk library directories, group audio files by artist/album, collect
/// format+bitrate info, and per-file gate status. `filters` provides the
/// allowed-extension and minimum-bitrate gate used to compute.
/// [`ScannedAlbum::needs_upgrade`] for every file.
pub fn scan_library(
    library_paths: &[String],
    filters: &crate::config::FilterConfig,
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
            // The on-disk album folder name, kept verbatim. It is the identity
            // the upgrade path copies into and the root the quality-deletion
            // pass walks, so stripping an embedded disc marker here would move
            // the write into a new folder and leave the replaced files behind
            // in the old one — the album would then be re-upgraded on every
            // run. Merging marker-variant folders into one album is a presence
            // and identity concern, not a path concern.
            let album = components[album_index].to_string();

            // Read audio tags if available
            let (tag_artist, tag_album, bitrate) = read_audio_tags(path);

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
            let final_album = tag_album.unwrap_or(album);

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
                    artist_dir: artist_dir.clone(),
                    track_count: 1,
                    needs_upgrade: usize::from(file_needs_upgrade),
                    min_bitrate: bitrate,
                    max_bitrate: bitrate,
                    formats: vec![ext],
                });
        }
    }

    Ok(albums.into_values().collect())
}

/// Read artist, album, and bitrate from an audio file using lofty.
/// Returns (artist, album, bitrate_kbps). Falls back gracefully if tag reading fails.
fn read_audio_tags(path: &Path) -> (Option<String>, Option<String>, Option<u32>) {
    let tagged_file = match lofty::probe::Probe::open(path) {
        Ok(probe) => match probe.read() {
            Ok(file) => file,
            Err(_) => return (None, None, None),
        },
        Err(_) => return (None, None, None),
    };

    let tag = tagged_file
        .primary_tag()
        .or_else(|| tagged_file.first_tag());

    let artist = tag.and_then(|t| t.artist().map(|a| a.to_string()));
    let album = tag.and_then(|t| t.album().map(|a| a.to_string()));

    // lofty 0.21 reports the audio bitrate in kbps
    let bitrate = tagged_file.properties().audio_bitrate();

    (artist, album, bitrate)
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
    use crate::test_support::{write_minimal_flac, write_minimal_flac_with_tags};
    use std::fs;
    use tempfile::TempDir;

    /// Helper: wrap a temp dir path as the single-element library paths list
    /// expected by `scan_library`.
    fn library_paths(dir: &std::path::Path) -> Vec<String> {
        vec![dir.to_string_lossy().into_owned()]
    }

    #[test]
    fn test_scan_empty_directory() {
        let dir = TempDir::new().unwrap();
        let albums = scan_library(&library_paths(dir.path()), &FilterConfig::default()).unwrap();
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

        let albums = scan_library(&library_paths(dir.path()), &FilterConfig::default()).unwrap();
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

        let albums = scan_library(&library_paths(dir.path()), &FilterConfig::default()).unwrap();
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

        let albums = scan_library(&library_paths(dir.path()), &FilterConfig::default()).unwrap();
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

        let albums = scan_library(&library_paths(dir.path()), &FilterConfig::default()).unwrap();

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
        let albums = scan_library(&library_paths(dir.path()), &filters).unwrap();
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

        let albums = scan_library(&library_paths(dir.path()), &FilterConfig::default()).unwrap();
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

        let albums = scan_library(&library_paths(dir.path()), &FilterConfig::default()).unwrap();
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

        let albums = scan_library(&library_paths(dir.path()), &FilterConfig::default()).unwrap();
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

        let albums = scan_library(&library_paths(dir.path()), &FilterConfig::default()).unwrap();
        assert!(albums.is_empty());
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
        // Regression: the album name is the identity the auto-mode upgrade
        // copies into and the root the quality-deletion pass walks. Stripping
        // the marker here moves the copy into a new folder while the replaced
        // files stay in the old one, so the album was re-upgraded every run.
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
}
