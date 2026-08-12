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
    pub track_count: usize,
    pub min_bitrate: Option<u32>,
    pub max_bitrate: Option<u32>,
    pub formats: Vec<String>,
}

/// Recognised audio file extensions that the scanner should pick up.
/// This is broader than `filters.allowed_extensions` — the scanner needs to
/// see *all* audio files so it can detect albums that contain formats outside
/// the user's quality target.
const KNOWN_AUDIO_EXTENSIONS: &[&str] = &[
    "flac", "mp3", "m4a", "aac", "ogg", "opus", "wav", "wma", "ape", "mpc", "wv", "aiff", "aif",
    "dsf", "dff", "spx",
];

/// Walk library directories, group audio files by artist/album, collect format+bitrate info.
pub fn scan_library(library_paths: &[String]) -> Result<Vec<ScannedAlbum>> {
    let mut albums: std::collections::BTreeMap<(String, String), ScannedAlbum> =
        std::collections::BTreeMap::new();
    let ext_set: HashSet<&str> = KNOWN_AUDIO_EXTENSIONS.iter().copied().collect();

    for lib_path_str in library_paths {
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

            // Infer artist/album from directory structure: <root>/Artist/Album/tracks
            let relative = path.strip_prefix(lib_path).unwrap_or(path);
            let components: Vec<&str> = relative.iter().filter_map(|c| c.to_str()).collect();

            if components.len() < 3 {
                continue; // Need at least Artist/Album/file
            }
            let artist = components[0].to_string();
            let album = components[1].to_string();

            // Read audio tags if available
            let (tag_artist, tag_album, bitrate) = read_audio_tags(path);

            // Prefer tag metadata over directory name
            let final_artist = tag_artist.unwrap_or(artist);
            let final_album = tag_album.unwrap_or(album);

            let key = (final_artist.clone(), final_album.clone());
            albums
                .entry(key)
                .and_modify(|a| {
                    a.track_count += 1;
                    if let Some(br) = bitrate {
                        a.min_bitrate = Some(a.min_bitrate.map_or(br, |m| m.min(br)));
                        a.max_bitrate = Some(a.max_bitrate.map_or(br, |m| m.max(br)));
                    }
                    if !a.formats.contains(&ext) {
                        a.formats.push(ext.clone());
                    }
                })
                .or_insert_with(|| ScannedAlbum {
                    path: lib_path.to_path_buf(),
                    artist: final_artist,
                    album: final_album,
                    track_count: 1,
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
/// An album is flagged if ANY track is below quality thresholds or in a non-allowed format.
pub fn find_albums_to_upgrade(
    albums: &[ScannedAlbum],
    config: &FilterConfig,
) -> Vec<(String, String)> {
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
            if let Some(min_br) = config.min_bitrate {
                match a.min_bitrate {
                    None => return true, // unknown quality → flags whole album
                    Some(album_min) if album_min < min_br => return true,
                    _ => {}
                }
            }

            false
        })
        .map(|a| (a.artist.clone(), a.album.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
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
        let albums = scan_library(&library_paths(dir.path())).unwrap();
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

        let albums = scan_library(&library_paths(dir.path())).unwrap();
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
                track_count: 3,
                min_bitrate: Some(128),
                max_bitrate: Some(192),
                formats: vec!["mp3".into()],
            },
            ScannedAlbum {
                path: PathBuf::new(),
                artist: "Artist2".into(),
                album: "Album2".into(),
                track_count: 5,
                min_bitrate: Some(900),
                max_bitrate: Some(1200),
                formats: vec!["flac".into()],
            },
        ];

        let config = crate::config::FilterConfig {
            allowed_extensions: vec!["flac".into()],
            min_bitrate: Some(320),
            min_bitdepth: None,
            exclude_words: vec![],
            include_locked: false,
        };

        let to_upgrade = find_albums_to_upgrade(&albums, &config);
        // Artist1: mp3 (not in allowed extensions) → upgrade
        // Artist2: flac, min bitrate 900 (above 320) → no upgrade
        assert_eq!(to_upgrade.len(), 1);
        assert_eq!(to_upgrade[0].0, "Artist1");
        assert_eq!(to_upgrade[0].1, "Album1");
    }

    #[test]
    fn test_find_albums_to_upgrade_wrong_format() {
        let albums = vec![ScannedAlbum {
            path: PathBuf::new(),
            artist: "Artist".into(),
            album: "Album".into(),
            track_count: 2,
            min_bitrate: Some(320),
            max_bitrate: Some(320),
            formats: vec!["mp3".into()],
        }];

        let config = crate::config::FilterConfig {
            allowed_extensions: vec!["flac".into()],
            min_bitrate: None,
            min_bitdepth: None,
            exclude_words: vec![],
            include_locked: false,
        };

        let to_upgrade = find_albums_to_upgrade(&albums, &config);
        assert_eq!(to_upgrade.len(), 1); // mp3 should trigger upgrade (not flac)
    }
}
