use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use lofty::file::AudioFile;
use sha2::{Digest, Sha256};

use crate::config::Config;
use crate::error::Result;

// ── Library upgrade: format classification and quality scoring ──

/// Classification of audio formats by quality tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioFormat {
    Lossless,
    Lossy,
}

/// Return the format category for a file extension, or None for non-audio
/// files (images, logs, info files, playlists, etc.).
pub fn format_from_extension(ext: &str) -> Option<AudioFormat> {
    match ext.to_lowercase().as_str() {
        "flac" | "wav" | "alac" | "ape" | "wv" | "aiff" | "aif" => Some(AudioFormat::Lossless),
        "mp3" | "ogg" | "oga" | "aac" | "m4a" | "wma" | "opus" | "spx" => Some(AudioFormat::Lossy),
        _ => None,
    }
}

/// Return true if the path points to a recognised audio file.
pub fn is_audio_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .and_then(format_from_extension)
        .is_some()
}

/// Quality score for a lossless file. Higher is better. The +1000 base keeps
/// every lossless file above every lossy bitrate; bitdepth and sample rate
/// then refine the ranking (e.g. 24-bit/96kHz FLAC beats 16-bit/44.1kHz).
pub fn quality_score_lossless(bitdepth: u32, sample_rate: u32) -> u64 {
    1000 + (bitdepth as u64) * 100 + (sample_rate as u64)
}

/// Quality score for a lossy file. Higher is better; the audio bitrate in
/// kbps ranks, e.g., 320 kbps MP3 above 128 kbps MP3.
pub fn quality_score_lossy(bitrate: u32) -> u64 {
    bitrate as u64
}

/// Strip a leading track-number token (e.g. "02 - " in "02 - Song" or "04_"
/// in "04_Cure for Me") from a file stem so it can be used as the pattern's
/// `%title%` placeholder without duplicating the track number. Returns the
/// stem unchanged when it has no numeric prefix.
fn strip_leading_track_token(stem: &str) -> &str {
    // Locate the first alphanumeric token.
    let token_start = stem
        .char_indices()
        .find(|(_, c)| c.is_alphanumeric())
        .map(|(i, _)| i)
        .unwrap_or(stem.len());
    if token_start == stem.len() {
        return stem;
    }
    let token_end = stem[token_start..]
        .find(|c: char| !c.is_alphanumeric())
        .map(|off| token_start + off)
        .unwrap_or(stem.len());
    let token = &stem[token_start..token_end];
    if token.len() > 3 || !token.chars().all(|c| c.is_ascii_digit()) {
        return stem; // e.g. "Song.flac" or "Album (1999) ..." — nothing to strip
    }
    // Skip the token plus any following separators (spaces, dashes, dots...).
    let rest = stem[token_end..]
        .find(|c: char| c.is_alphanumeric())
        .map(|off| token_end + off)
        .unwrap_or(stem.len());
    // If stripping consumes the entire stem (e.g. "01" -> ""), keep the
    // original to avoid producing an empty title placeholder.
    if rest >= stem.len() {
        return stem;
    }
    &stem[rest..]
}

/// Expand an organization pattern with metadata placeholders.
/// Placeholders: %artist%, %album%, %track%, %title%, %ext%, %user%
pub fn expand_pattern(
    pattern: &str,
    artist: &str,
    album: &str,
    track: &str,
    title: &str,
    ext: &str,
    user: &str,
) -> String {
    pattern
        .replace("%artist%", &sanitize_component(artist))
        .replace("%album%", &sanitize_component(album))
        .replace("%track%", &sanitize_component(track))
        .replace("%title%", &sanitize_component(title))
        .replace("%ext%", &sanitize_component(ext))
        .replace("%user%", &sanitize_component(user))
}

/// Remove path separators, null bytes, percent signs (to prevent cascading
/// placeholder re-substitution), and directory-traversal patterns from a
/// metadata value so it cannot inject extra path segments into the
/// destination path.
pub fn sanitize_component(value: &str) -> String {
    let mut s = value
        .replace(['/', '\\'], "-")
        .replace('\0', "")
        .replace('%', "％"); // U+FF05 FULLWIDTH PERCENT SIGN — prevents cascading replace
                             // Collapse directory-traversal sequences.
    while s.contains("..") {
        s = s.replace("..", "．"); // U+FF0E FULLWIDTH FULL STOP
    }
    s
}

/// Metadata used to expand the organize pattern.
#[derive(Debug, Clone)]
pub struct OrganizeInput<'a> {
    pub src: &'a Path,
    pub library_root: &'a Path,
    pub pattern: &'a str,
    pub artist: &'a str,
    pub album: &'a str,
    pub track: &'a str,
    pub title: &'a str,
    pub ext: &'a str,
}

/// Move a file from staging to the library using the naming pattern.
/// Handles directory creation and duplicate filenames (adds (1), (2) suffix).
pub fn organize_file(input: OrganizeInput<'_>) -> Result<PathBuf> {
    let relative = expand_pattern(
        input.pattern,
        input.artist,
        input.album,
        input.track,
        input.title,
        input.ext,
        "unknown",
    );
    let dest = input.library_root.join(&relative);

    // Create parent directories
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }

    // Handle duplicates: append (1), (2), etc.
    let final_dest = if dest.exists() {
        let stem = dest.file_stem().unwrap_or_default().to_string_lossy();
        let ext_str = dest
            .extension()
            .map(|e| format!(".{}", e.to_string_lossy()))
            .unwrap_or_default();
        let parent = dest.parent().unwrap_or(Path::new("."));
        let mut counter = 1;
        loop {
            let candidate = parent.join(format!("{stem} ({counter}){ext_str}"));
            if !candidate.exists() {
                break candidate;
            }
            counter += 1;
        }
    } else {
        dest
    };

    fs::rename(input.src, &final_dest)?;
    Ok(final_dest)
}

// ── Library upgrade: copy, quality-aware deletion, and recovery ──

/// Copy downloaded files from staging into the library directory, applying
/// the organize pattern for naming. The staging files are preserved (this is
/// a copy, not a move). Track numbers are zero-padded to two digits and the
/// leading track token is stripped from the title, so `%track% - %title%`
/// produces clean names like "01 - Song.flac".
pub fn copy_to_library(
    downloaded: &[PathBuf],
    library_root: &Path,
    pattern: &str,
    artist: &str,
    album: &str,
) -> Result<Vec<PathBuf>> {
    let mut dests = Vec::with_capacity(downloaded.len());
    for src in downloaded {
        let stem = src.file_stem().unwrap_or_default().to_string_lossy();
        let ext = src.extension().unwrap_or_default().to_string_lossy();
        let track = crate::tracks::track_number_from_filename(&stem)
            .map(|n| format!("{n:02}"))
            .unwrap_or_else(|| "01".to_string());
        let title = strip_leading_track_token(&stem);
        let relative = expand_pattern(pattern, artist, album, &track, title, &ext, "unknown");
        let dest = library_root.join(&relative);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(src, &dest)?;
        dests.push(dest);
    }
    Ok(dests)
}

/// Delete audio files in the album directory that are lower quality than the
/// best quality of the newly downloaded files. Non-audio files (images, logs,
/// cuesheets, playlists) are never deleted, and files whose paths appear in
/// `new_files` (the destination paths returned by `copy_to_library`) are
/// skipped. An empty `new_files` slice is a no-op (no new files means nothing
/// to compare against). Returns the number of files deleted.
pub fn delete_lesser_quality_files(
    library_root: &Path,
    artist: &str,
    album: &str,
    new_files: &[PathBuf],
) -> Result<u32> {
    let album_dir = library_root
        .join(sanitize_component(artist))
        .join(sanitize_component(album));

    // Files that were just copied in are the new quality baseline — skip them.
    let new_filenames: HashSet<String> = new_files
        .iter()
        .filter_map(|p| p.file_name().and_then(|n| n.to_str()))
        .map(String::from)
        .collect();

    // Best score among the newly downloaded files; 0 with no new files so
    // deletion is a no-op when the feature is disabled.
    let new_best_score = new_files
        .iter()
        .filter_map(|p| file_quality_score(p))
        .max()
        .unwrap_or(0);

    let mut deleted = 0;
    for entry in fs::read_dir(&album_dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if !is_audio_file(&path) {
            continue; // cover.jpg, info.nfo, logs, cuesheets are preserved.
        }
        let name = path.file_name().and_then(|n| n.to_str());
        if name.is_some_and(|n| new_filenames.contains(n)) {
            continue;
        }
        let existing_score = file_quality_score(&path).unwrap_or(0);
        if existing_score < new_best_score {
            fs::remove_file(&path)?;
            deleted += 1;
        }
    }
    Ok(deleted)
}

/// Compute a quality score for an audio file by reading its metadata with
/// lofty. Returns None if the file cannot be parsed (e.g. junk bytes), which
/// callers treat as score 0.
fn file_quality_score(path: &Path) -> Option<u64> {
    let ext = path.extension().and_then(|e| e.to_str())?;
    let format = format_from_extension(ext)?;
    let tagged_file = lofty::probe::Probe::open(path).ok()?.read().ok()?;
    let props = tagged_file.properties();
    match format {
        AudioFormat::Lossless => {
            let bitdepth = props.bit_depth().unwrap_or(0) as u32;
            let sample_rate = props.sample_rate().unwrap_or(44100);
            Some(quality_score_lossless(bitdepth, sample_rate))
        }
        AudioFormat::Lossy => {
            let bitrate = props.audio_bitrate().unwrap_or(0);
            Some(quality_score_lossy(bitrate))
        }
    }
}

/// Compute the SHA-256 hash of a file's contents, used to detect corrupt or
/// truncated partial copies during recovery.
pub fn file_hash(path: &Path) -> Result<String> {
    use std::io::Read;
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 8192];
    loop {
        let bytes_read = file.read(&mut buffer)?;
        if bytes_read == 0 {
            break;
        }
        hasher.update(&buffer[..bytes_read]);
    }
    // sha2 0.11's digest output is a hybrid-array `Array` without LowerHex,
    // so hex-encode byte by byte.
    let digest = hasher.finalize();
    Ok(digest.iter().map(|b| format!("{b:02x}")).collect())
}

/// Resume an interrupted library upgrade. Files already present in the library
/// with matching hashes are skipped; corrupt or truncated copies are replaced;
/// missing files are copied in. The staging directory is removed afterwards.
pub fn resume_library_upgrade(
    config: &Config,
    album_staging: &Path,
    library_root: &Path,
    artist: &str,
    album: &str,
) -> Result<()> {
    let album_dir = library_root
        .join(sanitize_component(artist))
        .join(sanitize_component(album));
    fs::create_dir_all(&album_dir)?;

    for entry in fs::read_dir(album_staging)? {
        let entry = entry?;
        let src = entry.path();
        if !src.is_file() {
            continue;
        }
        let dest = album_dir.join(src.file_name().unwrap());
        if dest.exists() {
            if file_hash(&src)? == file_hash(&dest)? {
                continue; // already copied correctly
            }
            // Corrupt/truncated partial copy: replace it.
            fs::remove_file(&dest)?;
        }
        fs::copy(&src, &dest)?;
    }

    // Clean up staging — absence of the staging dir signals a completed run.
    fs::remove_dir_all(album_staging)?;

    // Delete lesser-quality files (if enabled).
    if config.library_upgrade.delete_lesser_quality {
        let new_files: Vec<PathBuf> = fs::read_dir(&album_dir)?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| is_audio_file(p))
            .collect();
        delete_lesser_quality_files(library_root, artist, album, &new_files)?;
    }
    Ok(())
}

/// Parse an album staging directory name (e.g. "Artist--Album") into
/// (artist, album) components. Slugs without a separator map to "Unknown".
pub fn parse_album_slug(slug: &str) -> (String, String) {
    match slug.split_once("--") {
        Some((artist, album)) => (artist.to_string(), album.to_string()),
        None => (slug.to_string(), "Unknown".to_string()),
    }
}

/// Recover interrupted library upgrades by scanning the staging directory for
/// leftover album directories. Directories whose album is marked "success"
/// in the DB are re-verified and copied into the library; "in-progress"
/// directories from crashed downloads are cleaned up.
pub fn recover_interrupted_upgrades(
    config: &Config,
    db: &crate::db::Database,
    staging_dir: &Path,
) -> Result<()> {
    if !config.library_upgrade.enabled {
        return Ok(());
    }
    if config.library.paths.is_empty() {
        return Ok(());
    }
    let library_root = Path::new(&config.library.paths[0]);

    for entry in fs::read_dir(staging_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let album_staging = entry.path();
        let slug = album_staging
            .file_name()
            .unwrap_or_default()
            .to_string_lossy();
        let (artist, album) = parse_album_slug(&slug);
        match db.get_album_status(&artist, &album)? {
            Some(status) if status == "success" => {
                tracing::warn!("Recovering interrupted library upgrade: {artist} - {album}");
                resume_library_upgrade(config, &album_staging, library_root, &artist, &album)?;
            }
            Some(status) if status == "in-progress" => {
                tracing::info!("Cleaning up incomplete download: {artist} - {album}");
                fs::remove_dir_all(&album_staging)?;
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_expand_pattern() {
        let result = expand_pattern(
            "%artist%/%album%/%track% - %title%.%ext%",
            "Pink Floyd",
            "Dark Side of the Moon",
            "01",
            "Speak to Me",
            "flac",
            "fastuser",
        );
        assert_eq!(
            result,
            "Pink Floyd/Dark Side of the Moon/01 - Speak to Me.flac"
        );
    }

    #[test]
    fn test_expand_pattern_with_spaces() {
        let result = expand_pattern(
            "%artist% - %album%/%track% %title%.%ext%",
            "Radiohead",
            "OK Computer",
            "03",
            "Subterranean Homesick Alien",
            "flac",
            "someuser",
        );
        assert_eq!(
            result,
            "Radiohead - OK Computer/03 Subterranean Homesick Alien.flac"
        );
    }

    #[test]
    fn test_organize_moves_files() {
        let staging = TempDir::new().unwrap();
        let library = TempDir::new().unwrap();

        // Create a file in staging
        let src = staging.path().join("01 - Song.flac");
        fs::write(&src, b"fake flac content").unwrap();

        let pattern = "%artist%/%album%/%track% - %title%.%ext%";
        organize_file(OrganizeInput {
            src: &src,
            library_root: library.path(),
            pattern,
            artist: "Test Artist",
            album: "Test Album",
            track: "01",
            title: "Song",
            ext: "flac",
        })
        .unwrap();

        // File should have been moved to library
        let expected = library.path().join("Test Artist/Test Album/01 - Song.flac");
        assert!(expected.exists());
        // Source should be gone
        assert!(!src.exists());
    }

    #[test]
    fn test_organize_handles_duplicates() {
        let staging = TempDir::new().unwrap();
        let library = TempDir::new().unwrap();

        let src = staging.path().join("track.flac");
        fs::write(&src, b"content").unwrap();

        // First organize
        organize_file(OrganizeInput {
            src: &src,
            library_root: library.path(),
            pattern: "%artist%/%title%.%ext%",
            artist: "Artist",
            album: "Album",
            track: "01",
            title: "Title",
            ext: "flac",
        })
        .unwrap();
        assert!(library.path().join("Artist/Title.flac").exists());

        // Second file with same name
        let src2 = staging.path().join("track2.flac");
        fs::write(&src2, b"other content").unwrap();

        organize_file(OrganizeInput {
            src: &src2,
            library_root: library.path(),
            pattern: "%artist%/%title%.%ext%",
            artist: "Artist",
            album: "Album",
            track: "01",
            title: "Title",
            ext: "flac",
        })
        .unwrap();
        // Duplicate should get (1) suffix
        assert!(library.path().join("Artist/Title (1).flac").exists());
    }

    // ── Library upgrade tests ──

    /// Write a minimal but valid FLAC file ("fLaC" marker + STREAMINFO block:
    /// 44100 Hz, stereo, 16-bit) that lofty can actually parse, so quality
    /// scoring sees real metadata instead of degrading to 0 for junk bytes.
    fn write_real_flac(path: &Path) {
        let mut bytes = Vec::with_capacity(42);
        bytes.extend_from_slice(b"fLaC");
        // STREAMINFO metadata block header: last-block flag (0x80) + type 0,
        // content length 34 (0x22).
        bytes.extend_from_slice(&[0x80, 0x00, 0x00, 0x22]);
        // min/max block size (4096).
        bytes.extend_from_slice(&[0x10, 0x00, 0x10, 0x00]);
        // min/max frame size (unknown = 0).
        bytes.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
        // 20-bit sample rate (44100) | channels-1 (1) | bits-per-sample-1 (15)
        // | top 4 bits of total samples (0).
        bytes.extend_from_slice(&0x0AC4_42F0u32.to_be_bytes());
        // Remaining 32 bits of the 36-bit total sample count (44100).
        bytes.extend_from_slice(&0x0000_AC44u32.to_be_bytes());
        // MD5 signature of unencoded audio (unknown = zeros).
        bytes.extend_from_slice(&[0u8; 16]);
        assert_eq!(bytes.len(), 42);
        fs::write(path, bytes).unwrap();
    }

    #[test]
    fn test_is_audio_file() {
        for ext in ["flac", "mp3", "ogg", "aac", "wav", "wma", "opus", "alac"] {
            assert!(
                is_audio_file(Path::new(&format!("track.{ext}"))),
                "{ext} should be audio"
            );
        }
        for ext in ["jpg", "nfo", "txt", "cue", "m3u"] {
            assert!(
                !is_audio_file(Path::new(&format!("file.{ext}"))),
                "{ext} should not be audio"
            );
        }
    }

    #[test]
    fn test_quality_score_lossless_beats_lossy() {
        let flac_score = quality_score_lossless(16, 44100);
        let mp3_320_score = quality_score_lossy(320);
        assert!(
            flac_score > mp3_320_score,
            "flac={flac_score} mp3={mp3_320_score}"
        );
    }

    #[test]
    fn test_quality_score_higher_bitdepth_wins() {
        let score_16 = quality_score_lossless(16, 44100);
        let score_24 = quality_score_lossless(24, 96000);
        assert!(score_24 > score_16);
    }

    #[test]
    fn test_quality_score_higher_bitrate_wins() {
        let score_128 = quality_score_lossy(128);
        let score_320 = quality_score_lossy(320);
        assert!(score_320 > score_128);
    }

    #[test]
    fn test_format_from_extension() {
        assert_eq!(format_from_extension("flac"), Some(AudioFormat::Lossless));
        assert_eq!(format_from_extension("wav"), Some(AudioFormat::Lossless));
        assert_eq!(format_from_extension("alac"), Some(AudioFormat::Lossless));
        assert_eq!(format_from_extension("mp3"), Some(AudioFormat::Lossy));
        assert_eq!(format_from_extension("ogg"), Some(AudioFormat::Lossy));
        assert_eq!(format_from_extension("aac"), Some(AudioFormat::Lossy));
        assert_eq!(format_from_extension("opus"), Some(AudioFormat::Lossy));
        assert_eq!(format_from_extension("jpg"), None);
        assert_eq!(format_from_extension("nfo"), None);
    }

    #[test]
    fn test_sanitize_component_is_public() {
        assert_eq!(sanitize_component("A/B"), "A-B");
    }

    #[test]
    fn test_copy_to_library_preserves_staging() {
        let staging = TempDir::new().unwrap();
        let library = TempDir::new().unwrap();

        let src1 = staging.path().join("01 - Song.flac");
        let src2 = staging.path().join("02 - Song.flac");
        fs::write(&src1, b"flac content 1").unwrap();
        fs::write(&src2, b"flac content 2").unwrap();

        let pattern = "%artist%/%album%/%track% - %title%.%ext%";
        let files = vec![src1.clone(), src2.clone()];
        copy_to_library(&files, library.path(), pattern, "Test Artist", "Test Album").unwrap();

        // Files are copied (not moved) into the library with clean names:
        // zero-padded track number plus the title stripped of its track prefix.
        assert!(library
            .path()
            .join("Test Artist/Test Album/01 - Song.flac")
            .exists());
        assert!(library
            .path()
            .join("Test Artist/Test Album/02 - Song.flac")
            .exists());
        // Staging files still exist (copy, not move).
        assert!(src1.exists());
        assert!(src2.exists());
    }

    #[test]
    fn test_delete_lesser_quality_removes_worse_files() {
        let dir = TempDir::new().unwrap();
        let library_root = dir.path();
        let album_dir = library_root.join("Artist").join("Album");
        fs::create_dir_all(&album_dir).unwrap();

        // Existing lower-quality audio and metadata files.
        let old_mp3 = album_dir.join("01 - Old Track.mp3");
        let cover = album_dir.join("cover.jpg");
        let nfo = album_dir.join("info.nfo");
        fs::write(&old_mp3, b"mp3 content").unwrap();
        fs::write(&cover, b"jpg content").unwrap();
        fs::write(&nfo, b"nfo content").unwrap();

        // The newly downloaded file must be a real, parseable FLAC so its
        // quality score is meaningful (junk bytes score 0 and nothing deletes).
        let new_flac = album_dir.join("01 - New Track.flac");
        write_real_flac(&new_flac);

        let new_files = vec![new_flac];
        let deleted =
            delete_lesser_quality_files(library_root, "Artist", "Album", &new_files).unwrap();

        assert!(!old_mp3.exists(), "old MP3 should be deleted");
        assert!(cover.exists(), "cover.jpg should be preserved");
        assert!(nfo.exists(), "info.nfo should be preserved");
        assert_eq!(deleted, 1);
    }

    #[test]
    fn test_delete_lesser_quality_preserves_better_files() {
        let dir = TempDir::new().unwrap();
        let library_root = dir.path();
        let album_dir = library_root.join("Artist").join("Album");
        fs::create_dir_all(&album_dir).unwrap();

        // Existing high-quality FLAC (real, parseable metadata).
        let old_flac = album_dir.join("01 - Track.flac");
        write_real_flac(&old_flac);
        // New download is a lower-quality MP3.
        let new_mp3 = album_dir.join("01 - New Track.mp3");
        fs::write(&new_mp3, b"mp3 content").unwrap();

        let new_files = vec![new_mp3];
        let deleted =
            delete_lesser_quality_files(library_root, "Artist", "Album", &new_files).unwrap();

        assert!(
            old_flac.exists(),
            "FLAC should be preserved when new download is MP3"
        );
        assert_eq!(deleted, 0);
    }

    #[test]
    fn test_delete_lesser_quality_disabled_noop() {
        let dir = TempDir::new().unwrap();
        let library_root = dir.path();
        let album_dir = library_root.join("Artist").join("Album");
        fs::create_dir_all(&album_dir).unwrap();

        let old_mp3 = album_dir.join("01 - Track.mp3");
        fs::write(&old_mp3, b"mp3 content").unwrap();
        let new_flac = album_dir.join("01 - New.flac");
        fs::write(&new_flac, b"flac content").unwrap();

        // Empty new_files simulates `delete_lesser_quality: false`: nothing
        // may be removed.
        let deleted = delete_lesser_quality_files(library_root, "Artist", "Album", &[]).unwrap();
        assert!(old_mp3.exists(), "old file should be preserved");
        assert!(new_flac.exists());
        assert_eq!(deleted, 0);
    }

    #[test]
    fn test_resume_library_upgrade_recopies_corrupt_file() {
        let staging = TempDir::new().unwrap();
        let library = TempDir::new().unwrap();

        let src = staging.path().join("01 - Song.flac");
        fs::write(&src, b"correct content").unwrap();

        let album_dir = library.path().join("Artist/Album");
        fs::create_dir_all(&album_dir).unwrap();
        let dest = album_dir.join("01 - Song.flac");
        fs::write(&dest, b"corrupt truncated").unwrap();

        let config = Config::default();
        resume_library_upgrade(&config, staging.path(), library.path(), "Artist", "Album").unwrap();

        let content = fs::read_to_string(&dest).unwrap();
        assert_eq!(content, "correct content");
        assert!(!staging.path().exists());
    }

    #[test]
    fn test_resume_library_upgrade_skips_identical_file() {
        let staging = TempDir::new().unwrap();
        let library = TempDir::new().unwrap();

        let src = staging.path().join("01 - Song.flac");
        fs::write(&src, b"same content").unwrap();

        let album_dir = library.path().join("Artist/Album");
        fs::create_dir_all(&album_dir).unwrap();
        let dest = album_dir.join("01 - Song.flac");
        fs::write(&dest, b"same content").unwrap();

        let config = Config::default();
        resume_library_upgrade(&config, staging.path(), library.path(), "Artist", "Album").unwrap();

        let content = fs::read_to_string(&dest).unwrap();
        assert_eq!(content, "same content");
        assert!(!staging.path().exists());
    }

    #[test]
    fn test_parse_album_slug() {
        assert_eq!(
            parse_album_slug("Artist--Album"),
            ("Artist".to_string(), "Album".to_string())
        );
        assert_eq!(
            parse_album_slug("NoSeparator"),
            ("NoSeparator".to_string(), "Unknown".to_string())
        );
    }

    #[test]
    fn test_recover_interrupted_upgrades_resumes_success() {
        use crate::db::Database;

        let staging_root = TempDir::new().unwrap();
        let staging = staging_root.path().join("Artist--Album");
        fs::create_dir_all(&staging).unwrap();
        fs::write(staging.join("01 - Song.flac"), b"correct content").unwrap();

        let library = TempDir::new().unwrap();
        let mut config = Config::default();
        config.library_upgrade.enabled = true;
        config.library.paths = vec![library.path().to_string_lossy().into_owned()];

        let db = Database::open_in_memory().unwrap();
        db.mark_album_processed("Artist", "Album", "success")
            .unwrap();

        recover_interrupted_upgrades(&config, &db, staging_root.path()).unwrap();

        assert!(library.path().join("Artist/Album/01 - Song.flac").exists());
        assert!(!staging.exists());
    }
}
