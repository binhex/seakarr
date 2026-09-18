use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use lofty::file::AudioFile;
use sha2::{Digest, Sha256};

use crate::config::Config;
use crate::error::{Result, SeakarrError};

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

/// Derive the `%track%` and `%title%` metadata values for a staging file
/// stem (e.g. `"02 - Track Two"` -> `("02", "Track Two")`). The track
/// number is zero-padded to two digits and the leading track token is
/// stripped from the title so a `%track% - %title%` pattern yields clean
/// names like `"02 - Track Two.flac"`. Files without a parseable track
/// number fall back to track `"01"` with the stem unchanged as the title.
///
/// This is the single source of truth for organize naming: both the
/// auto-upgrade copy path and the manual organize path must produce the
/// same destination names for the same staging files.
pub fn organize_name_from_stem(stem: &str) -> (String, String) {
    let track = crate::tracks::track_number_from_filename(stem)
        .map(|n| format!("{n:02}"))
        .unwrap_or_else(|| "01".to_string());
    let title = strip_leading_track_token(stem).to_string();
    // A DISC-TRACK stem ("1-11 - Steel Bars") keeps the track number once the
    // disc number is stripped, which would duplicate it in `%track% - %title%`.
    // Only the hyphenated form is treated this way: a title that merely starts
    // with its own track number ("01 - 1 Thing") keeps that word.
    let title = if crate::tracks::has_hyphenated_disc_prefix(stem) {
        strip_leading_track_token(&title).to_string()
    } else {
        title
    };
    (track, title)
}

/// Expand an organization pattern with metadata placeholders.
/// Placeholders: %artist%, %album%, %track%, %title%, %ext%, %user%
///
/// Every value is sanitised, so remote metadata cannot inject path segments.
/// Callers that already hold a filesystem-derived component use the placement
/// entry point instead, which substitutes it verbatim.
pub fn expand_pattern(
    pattern: &str,
    artist: &str,
    album: &str,
    track: &str,
    title: &str,
    ext: &str,
    user: &str,
) -> String {
    expand_pattern_inner(
        pattern,
        &sanitize_component(artist),
        album,
        track,
        title,
        ext,
        user,
    )
}

/// Expand a pattern whose artist value is already final, such as the on-disk
/// name of an existing artist folder.
///
/// `%artist%` is substituted last so a value that itself contains a placeholder
/// cannot cascade into another field's value. Sanitised callers are unaffected
/// because [`sanitize_component`] removes `%` before substitution.
fn expand_pattern_inner(
    pattern: &str,
    artist: &str,
    album: &str,
    track: &str,
    title: &str,
    ext: &str,
    user: &str,
) -> String {
    pattern
        .replace("%album%", &sanitize_component(album))
        .replace("%track%", &sanitize_component(track))
        .replace("%title%", &sanitize_component(title))
        .replace("%ext%", &sanitize_component(ext))
        .replace("%user%", &sanitize_component(user))
        .replace("%artist%", artist)
}

/// Characters Windows cannot store in a file or directory name, whatever the
/// filesystem serving them. Windows reserves these outright, and a Linux write
/// that keeps one leaves the folder unrenderable over SMB: Explorer falls back
/// to a mangled 8.3 short name. `/` and `\` are absent because they are
/// replaced rather than removed, so a value cannot inject a path segment.
const WINDOWS_RESERVED_CHARACTERS: [char; 7] = ['<', '>', ':', '"', '|', '?', '*'];

/// Device names Windows reserves in every directory, so they can never name a
/// folder. The superscript digits are listed by Microsoft alongside the plain
/// ones.
const WINDOWS_RESERVED_NAMES: [&str; 28] = [
    "CON",
    "PRN",
    "AUX",
    "NUL",
    "COM1",
    "COM2",
    "COM3",
    "COM4",
    "COM5",
    "COM6",
    "COM7",
    "COM8",
    "COM9",
    "LPT1",
    "LPT2",
    "LPT3",
    "LPT4",
    "LPT5",
    "LPT6",
    "LPT7",
    "LPT8",
    "LPT9",
    "COM\u{b9}",
    "COM\u{b2}",
    "COM\u{b3}",
    "LPT\u{b9}",
    "LPT\u{b2}",
    "LPT\u{b3}",
];

/// Inserted where a component would otherwise be unsafe. It makes a reserved
/// device name an ordinary one (`CON` becomes `_CON`), and replaces a value
/// that sanitises away to nothing so a path level is never empty — without it a
/// title such as `???` would collapse a level and write an album beside its
/// artist folder instead of inside it.
const SAFE_COMPONENT_PLACEHOLDER: &str = "_";

/// Remove the characters Windows cannot store and trim the trailing dot or
/// space it silently discards. Keeping either would mean the name Linux stores
/// and the name Explorer shows are different strings — the defect this guards.
///
/// The trailing trim is a single pass over the predicate, not a dot-then-space
/// sequence: removing a dot run exposes the whitespace in front of it, and
/// removing that whitespace can expose another dot, so `"Album. ."` would
/// otherwise come back still ending in the dot Windows drops.
fn strip_unsafe_name_characters(value: &str) -> String {
    value
        .chars()
        .filter(|character| {
            !WINDOWS_RESERVED_CHARACTERS.contains(character) && !character.is_control()
        })
        .collect::<String>()
        .trim()
        .trim_end_matches(|character: char| character == '.' || character.is_whitespace())
        .to_string()
}

/// Prefix a reserved device name so it can be used as an ordinary component.
/// Windows matches the stem before the first dot, so `NUL.txt` is as reserved
/// as `NUL`. Ordinary names that merely contain a device name are untouched.
fn neutralise_device_name(value: &str) -> String {
    let stem = value.split('.').next().unwrap_or_default().trim();
    if WINDOWS_RESERVED_NAMES
        .iter()
        .any(|name| name.eq_ignore_ascii_case(stem))
    {
        format!("{SAFE_COMPONENT_PLACEHOLDER}{value}")
    } else {
        value.to_string()
    }
}

/// Remove path separators, null bytes, percent signs (to prevent cascading
/// placeholder re-substitution), and directory-traversal patterns from a
/// metadata value so it cannot inject extra path segments into the
/// destination path.
///
/// The same pass also makes the value portable between Linux and Windows,
/// because the library may be served to Windows clients over SMB: characters
/// Windows reserves are removed, control characters are removed, a trailing dot
/// or space is trimmed, and a reserved device name is prefixed. A value that
/// sanitises away to nothing becomes [`SAFE_COMPONENT_PLACEHOLDER`]. The result
/// is idempotent, so re-sanitising an already-sanitised on-disk name is a no-op
/// — which is what lets the album-presence key compare a stored name with the
/// MusicBrainz title it was written from.
pub fn sanitize_component(value: &str) -> String {
    let value = value
        .replace(['/', '\\'], "-")
        .replace('\0', "")
        .replace('%', "％"); // U+FF05 FULLWIDTH PERCENT SIGN — prevents cascading replace
    let mut s = strip_unsafe_name_characters(&value);
    // Collapse directory-traversal sequences. Runs after the trailing-dot trim
    // so "Album.." becomes "Album" rather than a fullwidth stop.
    while s.contains("..") {
        s = s.replace("..", "．"); // U+FF0E FULLWIDTH FULL STOP
    }
    let s = neutralise_device_name(&s);
    if s.is_empty() {
        SAFE_COMPONENT_PLACEHOLDER.to_string()
    } else {
        s
    }
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

/// Result of a library write: the album folder targeted and the files written.
///
/// `album_dir` is reported to the operator as the album's final destination, so
/// it must be the album folder a human recognises — never a disc subdirectory,
/// and never a path recomputed from the organize pattern (which the
/// sanitisation pass and the verbatim artist component both rewrite).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryWriteOutcome {
    /// The album folder the write targeted, with any disc subdirectory removed.
    /// Present even when `written` is empty.
    pub album_dir: PathBuf,
    /// Destinations actually written. Excludes a file whose destination was
    /// kept because the library already held a better or parseable copy.
    pub written: Vec<PathBuf>,
}

/// Move a file from staging to the library using the naming pattern.
/// Handles directory creation and duplicate filenames (adds (1), (2) suffix).
pub fn organize_file(input: OrganizeInput<'_>) -> Result<LibraryWriteOutcome> {
    let relative = expand_pattern(
        input.pattern,
        input.artist,
        input.album,
        input.track,
        input.title,
        input.ext,
        "unknown",
    );
    let mut dest = input.library_root.join(&relative);
    // The album folder is the destination's parent *before* the disc
    // subdirectory is inserted below, so a multi-disc album reports the album
    // folder rather than the disc folder.
    let album_dir = dest
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| input.library_root.to_path_buf());
    if let Some(disc) = disc_subdir(input.src) {
        dest = match dest.parent() {
            Some(parent) => parent.join(disc).join(dest.file_name().unwrap_or_default()),
            None => PathBuf::from(disc).join(dest.file_name().unwrap_or_default()),
        };
    }

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

    match fs::rename(input.src, &final_dest) {
        Ok(()) => {}
        // The staging area and the library may live on different mount
        // points (separate disks, Docker volumes). rename(2) then fails
        // with EXDEV; fall back to copy-and-delete so the file still lands
        // in the library (mirroring the library-upgrade copy path).
        Err(e) if e.kind() == std::io::ErrorKind::CrossesDevices => {
            fs::copy(input.src, &final_dest)?;
            fs::remove_file(input.src)?;
        }
        Err(e) => return Err(e.into()),
    }
    // The generic organize path is the manual and batch route into the library
    // and writes one file at a time. Without this the README's promise of
    // per-file destinations at DEBUG holds only for the copy-based paths.
    tracing::debug!(
        "Organized: {} -> {}",
        input.src.display(),
        final_dest.display()
    );
    Ok(LibraryWriteOutcome {
        album_dir,
        written: vec![final_dest],
    })
}

// ── Library upgrade: copy, quality-aware deletion, and recovery ──

/// How the `%artist%` component of a pattern is produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArtistComponent {
    /// Tag-derived name: sanitised like every other metadata value.
    Sanitized,
    /// An existing on-disk folder name, used verbatim as a single path
    /// component. Rewriting it (percent signs, double dots, backslashes) would
    /// place the album beside the real artist folder instead of inside it.
    /// Callers only pass one component produced by the library walk, so it
    /// cannot introduce a path separator.
    Verbatim,
}

/// What to do when the destination file already exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExistingFile {
    /// Upgrade semantics: replace the existing file unless it scores strictly
    /// higher, because the destination belongs to the album being replaced.
    ReplaceUnlessBetter,
    /// Placement semantics: keep any existing file that parses as audio,
    /// because the destination folder may hold a different edition of the
    /// album. Only a file lofty cannot open at all (junk bytes, an empty file,
    /// an unrelated format) is replaced.
    ///
    /// The probe reads the header and metadata block, so a copy interrupted
    /// after that block still parses and is kept. That is deliberate: reading
    /// every audio frame to prove completeness is not affordable per file, and
    /// replacing a complete track that merely looks smaller would destroy a
    /// different edition of the album. A kept destination is therefore not
    /// written from the fresh download — the caller completes the album as
    /// placed and warns when every destination was kept (see the placement arm
    /// in `runner`).
    KeepWhenValid,
}

/// Copy downloaded files from staging into the library directory, applying
/// the organize pattern for naming. The staging files are preserved (this is
/// a copy, not a move). Track numbers are zero-padded to two digits and the
/// leading track token is stripped from the title, so `%track% - %title%`
/// produces clean names like "01 - Song.flac".
///
/// A destination that already holds a strictly better file is kept (see the
/// per-file guard in the shared implementation below); otherwise the copy
/// replaces it. Used by the library upgrade path, where the artist name is
/// tag-derived and every metadata value is sanitised.
pub fn copy_to_library(
    downloaded: &[PathBuf],
    library_root: &Path,
    pattern: &str,
    artist: &str,
    album: &str,
) -> Result<LibraryWriteOutcome> {
    copy_into_library(LibraryWrite {
        downloaded,
        library_root,
        pattern,
        artist,
        album,
        artist_component: ArtistComponent::Sanitized,
        existing_file: ExistingFile::ReplaceUnlessBetter,
    })
}

/// Place newly downloaded files beside an artist's existing albums.
///
/// Two differences from [`copy_to_library`], both required because placement
/// adds a new album rather than replacing a known one:
///
/// - `artist_dir` is an on-disk folder name used verbatim, so the album lands
///   inside the folder that already exists instead of beside a rewritten copy
///   of its name;
/// - a destination file that parses as audio is never replaced, because the
///   destination folder may hold a different edition of the album. The test is
///   header/metadata parseability, so a partially written file that still
///   parses counts as audio the library already holds, and a track whose
///   destination is kept is not written from the fresh download. The album as a
///   whole still counts as placed in that state, which is what the design
///   requires: a failure there would not converge, because the album folder
///   presence looks for is still absent.
///
/// `artist_dir` must be a single ordinary path component. It is substituted
/// into the pattern without sanitisation, so a value carrying a separator or
/// `..` would write outside the library; the discover loop only ever passes the
/// artist folder name produced by the library walk.
///
/// # Errors
///
/// [`SeakarrError::Config`] when `artist_dir` is not a single ordinary path
/// component, and any filesystem error the copy or directory creation returns.
pub fn place_into_library(
    downloaded: &[PathBuf],
    library_root: &Path,
    pattern: &str,
    artist_dir: &str,
    album: &str,
) -> Result<LibraryWriteOutcome> {
    let mut components = Path::new(artist_dir).components();
    let single_normal = matches!(components.next(), Some(std::path::Component::Normal(_)))
        && components.next().is_none();
    if !single_normal {
        return Err(SeakarrError::Config(format!(
            "artist folder {artist_dir:?} must be a single path component"
        )));
    }
    copy_into_library(LibraryWrite {
        downloaded,
        library_root,
        pattern,
        artist: artist_dir,
        album,
        artist_component: ArtistComponent::Verbatim,
        existing_file: ExistingFile::KeepWhenValid,
    })
}

/// Everything one library write needs: the files to copy, where they go, and
/// which keep/replace policy applies. Grouped so the shared implementation takes
/// one argument instead of seven, and so the two entry points read as a list of
/// named choices rather than a positional run of strings.
struct LibraryWrite<'a> {
    downloaded: &'a [PathBuf],
    library_root: &'a Path,
    pattern: &'a str,
    artist: &'a str,
    album: &'a str,
    artist_component: ArtistComponent,
    existing_file: ExistingFile,
}

fn copy_into_library(write: LibraryWrite<'_>) -> Result<LibraryWriteOutcome> {
    let LibraryWrite {
        downloaded,
        library_root,
        pattern,
        artist,
        album,
        artist_component,
        existing_file,
    } = write;
    let mut written = Vec::with_capacity(downloaded.len());
    let mut album_dir: Option<PathBuf> = None;
    for src in downloaded {
        let stem = src.file_stem().unwrap_or_default().to_string_lossy();
        let ext = src.extension().unwrap_or_default().to_string_lossy();
        let (track, title) = organize_name_from_stem(&stem);
        let relative = match artist_component {
            ArtistComponent::Sanitized => {
                expand_pattern(pattern, artist, album, &track, &title, &ext, "unknown")
            }
            ArtistComponent::Verbatim => {
                expand_pattern_inner(pattern, artist, album, &track, &title, &ext, "unknown")
            }
        };
        let mut dest = library_root.join(&relative);
        // Recorded before the disc subdirectory is inserted below, and before
        // the keep/ replace decision, so the album folder is known even when
        // every destination is kept.
        if album_dir.is_none() {
            album_dir = dest.parent().map(Path::to_path_buf);
        }
        // Preserve the per-disc structure for multi-disc albums: when a
        // source file lives in a disc subdirectory under staging (a dedicated
        // "CD 01" folder or an embedded-marker "Gold (Disc 1)" folder),
        // keep that disc subdirectory under the album so same-named tracks
        // from different discs do not collide.
        if let Some(disc) = disc_subdir(src) {
            dest = match dest.parent() {
                Some(parent) => parent.join(disc).join(dest.file_name().unwrap_or_default()),
                None => PathBuf::from(disc).join(dest.file_name().unwrap_or_default()),
            };
        }
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        if dest.exists() {
            let keep_existing = match existing_file {
                // Never downgrade an existing library file. When the
                // destination already holds a copy that is strictly better than
                // the new download (e.g. the library has a 24-bit FLAC and the
                // flagged album's peer copy is 16-bit), keep the existing file
                // and do not record the destination as newly written — the
                // quality deletion pass must not treat it as a protected new
                // file while its own score already exceeds the new baseline.
                // Unparseable files score 0, so a parseable new download still
                // replaces a corrupt or mislabelled stale file. Equal scores
                // copy (the new download is in the same quality tier and
                // becomes the new baseline).
                ExistingFile::ReplaceUnlessBetter => {
                    let existing_score = file_quality_score(&dest).unwrap_or(0);
                    let new_score = file_quality_score(src).unwrap_or(0);
                    if existing_score > new_score {
                        tracing::info!(
                            "Keeping higher-quality existing file {} (score {existing_score} > {new_score})",
                            dest.display()
                        );
                        true
                    } else {
                        false
                    }
                }
                // The probe is deliberate rather than a quality score: a valid
                // file in a format with no score (DSF, for example) must still
                // count as audio the library already holds.
                ExistingFile::KeepWhenValid => {
                    if parses_as_audio(&dest) {
                        tracing::info!(
                            "Keeping existing file {} (placement never replaces a file that parses as audio)",
                            dest.display()
                        );
                        true
                    } else {
                        false
                    }
                }
            };
            if keep_existing {
                continue;
            }
        }
        fs::copy(src, &dest)?;
        tracing::debug!("Organized: {} -> {}", src.display(), dest.display());
        written.push(dest);
    }
    Ok(LibraryWriteOutcome {
        // `downloaded` is never empty for a completed album, so the fallback is
        // defensive only.
        album_dir: album_dir.unwrap_or_else(|| library_root.to_path_buf()),
        written,
    })
}

/// True when lofty can open and parse the file, i.e. the library already holds
/// an audio file at this path. Unlike quality scoring this does not depend on
/// the format being scoreable, so a valid DSF or DFF file, which has no entry
/// in [`format_from_extension`], still counts.
fn parses_as_audio(path: &Path) -> bool {
    lofty::probe::Probe::open(path)
        .and_then(|probe| probe.read())
        .is_ok()
}

/// Return the disc subdirectory component of a staging source path, if the
/// file was downloaded into a per-disc folder — either a dedicated disc
/// folder ("CD 01", "Disc 2") or an album folder carrying an embedded disc
/// marker ("Gold (Disc 1)"). Walks the parent components and returns the
/// deepest disc folder component. Files flattened directly into the album
/// staging root return `None` so their destination is unchanged.
///
/// The returned label is the peer's own folder name, so it is run through
/// [`sanitize_component`] before it is handed back: it becomes a real path
/// component in the library, and a peer-supplied name must not reach the
/// filesystem raw.
///
/// Uses [`crate::discs`] so the destination grouping matches the download
/// stager exactly.
fn disc_subdir(src: &Path) -> Option<String> {
    let mut current = src.parent();
    while let Some(dir) = current {
        if let Some(name) = dir.file_name().and_then(|n| n.to_str()) {
            // The runner's per-album staging root uses Artist--Album. It can
            // contain an embedded "(Disc N)" album name but is not itself a
            // disc directory.
            if name.contains("--") {
                break;
            }
            if crate::discs::is_disc_designator(name) {
                // Return the innermost folder, closest to the file. Only the
                // classification is normalized across marker styles; the label
                // itself is the peer's own, so it goes through the sanitiser
                // before it becomes a library path component.
                return Some(sanitize_component(name));
            }
        }
        current = dir.parent();
    }
    None
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
    // Use relative paths (from album_dir) so files in disc subdirectories
    // (e.g. "CD 01/01 - Track.flac") are distinguished from flat files
    // with the same basename (e.g. "01 - Track.flac").
    let new_relpaths: HashSet<String> = new_files
        .iter()
        .filter_map(|p| p.strip_prefix(&album_dir).ok())
        .map(|p| p.to_string_lossy().into_owned())
        .collect();

    // Best score among the newly downloaded files; 0 with no new files so
    // deletion is a no-op when the feature is disabled.
    let new_best_score = new_files
        .iter()
        .filter_map(|p| file_quality_score(p))
        .max()
        .unwrap_or(0);

    // Recursively walk the album directory so files inside disc
    // subdirectories (CD 01/, CD 02/) are considered for deletion too.
    fn walk_delete(
        dir: &Path,
        album_dir: &Path,
        new_relpaths: &HashSet<String>,
        new_best_score: u64,
    ) -> Result<u32> {
        let mut count: u32 = 0;
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if entry.file_type()?.is_dir() {
                count += walk_delete(&path, album_dir, new_relpaths, new_best_score)?;
                continue;
            }
            if !is_audio_file(&path) {
                continue; // cover.jpg, info.nfo, logs, cuesheets are preserved.
            }
            let rel = path
                .strip_prefix(album_dir)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();
            if new_relpaths.contains(&rel) {
                continue;
            }
            let existing_score = file_quality_score(&path).unwrap_or(0);
            if existing_score < new_best_score {
                fs::remove_file(&path)?;
                count += 1;
            }
        }
        Ok(count)
    }
    let deleted = walk_delete(&album_dir, &album_dir, &new_relpaths, new_best_score)?;
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

/// Extract the actual bitrate (kbps) of an audio file using lofty. Returns
/// None when the file cannot be parsed (e.g. junk bytes), the extension is
/// not recognised, or the format is lossless (bitrate is not a meaningful
/// quality metric for lossless formats — use [`extract_bitdepth`] instead).
pub fn extract_bitrate(path: &Path) -> Option<u32> {
    let ext = path.extension().and_then(|e| e.to_str())?;
    let format = format_from_extension(ext)?;
    match format {
        AudioFormat::Lossy => {
            let tagged_file = lofty::probe::Probe::open(path).ok()?.read().ok()?;
            let props = tagged_file.properties();
            props.audio_bitrate().filter(|&br| br > 0)
        }
        AudioFormat::Lossless => None,
    }
}

/// Extract the actual bit depth of an audio file using lofty. Returns None
/// when the file cannot be parsed (e.g. junk bytes), the extension is not
/// recognised, or the format is lossy (bit depth is not applicable to lossy
/// formats — use [`extract_bitrate`] instead).
pub fn extract_bitdepth(path: &Path) -> Option<u32> {
    let ext = path.extension().and_then(|e| e.to_str())?;
    let format = format_from_extension(ext)?;
    match format {
        AudioFormat::Lossless => {
            let tagged_file = lofty::probe::Probe::open(path).ok()?.read().ok()?;
            let props = tagged_file.properties();
            props.bit_depth().map(|b| b as u32)
        }
        AudioFormat::Lossy => None,
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

    // Walk the staging directory recursively so files inside disc
    // subdirectories (CD 01/, CD 02/) are copied as well, preserving the
    // CD XX structure under the album directory. A recursive walk is
    // required because multi-disc albums keep each disc in its own folder.
    // Returns the destination paths of all files copied or verified identical,
    // so the caller can pass them as the protection set to
    // delete_lesser_quality_files.
    fn walk(src_root: &Path, dest_root: &Path, copied: &mut Vec<PathBuf>) -> Result<()> {
        for entry in fs::read_dir(src_root)? {
            let entry = entry?;
            let src = entry.path();
            // Use entry.file_type() instead of src.is_dir() so we do not
            // follow symlinks — a symlink cycle inside staging would cause
            // unbounded recursion (stack overflow).
            if entry.file_type()?.is_dir() {
                // Recurse into the subdirectory, mapping it 1:1 onto the
                // destination so CD 01/ stays CD 01/. Sanitising the leaf keeps
                // a disc designator in the same directory the forward copy would
                // have used. Create the matching destination directory so the
                // copy has a parent to land in. The lossy conversion cannot
                // normally fire because the stager derives these names from UTF-8
                // peer metadata.
                let leaf = sanitize_component(&entry.file_name().to_string_lossy());
                let dest_dir = dest_root.join(leaf);
                fs::create_dir_all(&dest_dir)?;
                walk(&src, &dest_dir, copied)?;
            } else {
                let dest = dest_root.join(sanitize_component(&entry.file_name().to_string_lossy()));
                if dest.exists() {
                    if file_hash(&src)? == file_hash(&dest)? {
                        copied.push(dest);
                        continue; // already copied correctly
                    }
                    // Corrupt/truncated partial copy: replace it.
                    fs::remove_file(&dest)?;
                }
                fs::copy(&src, &dest)?;
                copied.push(dest);
            }
        }
        Ok(())
    }
    let mut copied = Vec::new();
    walk(album_staging, &album_dir, &mut copied)?;

    // Clean up staging — absence of the staging dir signals a completed run.
    fs::remove_dir_all(album_staging)?;

    // Delete lesser-quality files (if enabled).
    if config.library_upgrade.delete_lesser_quality {
        delete_lesser_quality_files(library_root, artist, album, &copied)?;
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
/// in the DB are re-verified and copied into the library (crash during the
/// copy step). Every other leftover — an album marked "failed", an album
/// that could not be identified, or a directory whose run never recorded any
/// status — is the debris of an interrupted *download*: the album was never
/// fully staged, so the directory is removed. This keeps a later download
/// for the same album from interleaving with stale `.part` files or partial
/// tracks from the crashed run.
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
            // "in-progress" is never written by any code path (downloads
            // record "failed" on interruption), so this arm also covers
            // crashed downloads whose run ended with a recorded failure or
            // no status at all.
            _ => {
                tracing::info!(
                    "Cleaning up leftover staging from interrupted run: {artist} - {album}"
                );
                fs::remove_dir_all(&album_staging)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::test_support::{write_minimal_flac, write_minimal_flac24};
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

    #[test]
    fn organize_preserves_brace_marker_disc_subdirectory() {
        let staging = TempDir::new().unwrap();
        let library = TempDir::new().unwrap();
        let disc = staging.path().join("Album {cd1}");
        fs::create_dir_all(&disc).unwrap();
        let source = disc.join("01 - Song.flac");
        fs::write(&source, b"content").unwrap();

        let destination = organize_file(OrganizeInput {
            src: &source,
            library_root: library.path(),
            pattern: "%artist%/%album%/%track% - %title%.%ext%",
            artist: "Artist",
            album: "Album",
            track: "01",
            title: "Song",
            ext: "flac",
        })
        .unwrap()
        .written[0]
            .clone();

        assert_eq!(
            destination,
            library
                .path()
                .join("Artist/Album/Album {cd1}/01 - Song.flac")
        );
        assert!(destination.is_file());
    }

    // ── Library upgrade tests ──

    #[test]
    fn test_place_into_library_uses_the_artist_folder_name_verbatim() {
        // Placement must reuse the folder that already exists on disk. The
        // upgrade path sanitises the artist because it is tag-derived; a folder
        // name such as "100% Hits.." must not be rewritten, because the
        // rewritten name would be a second artist folder beside the real one.
        let staging = TempDir::new().unwrap();
        let library = TempDir::new().unwrap();
        let artist_dir = library.path().join("100% Hits..");
        fs::create_dir_all(&artist_dir).unwrap();

        let src = staging.path().join("01 - Track One.flac");
        fs::write(&src, b"fake flac data").unwrap();

        let dests = place_into_library(
            std::slice::from_ref(&src),
            library.path(),
            "%artist%/%album%/%track% - %title%.%ext%",
            "100% Hits..",
            "Test Album",
        )
        .unwrap()
        .written;

        assert_eq!(dests.len(), 1);
        assert_eq!(
            dests[0],
            artist_dir.join("Test Album").join("01 - Track One.flac"),
            "the on-disk artist folder name must be used verbatim"
        );
        assert!(dests[0].exists());
    }

    #[test]
    fn test_place_into_library_never_replaces_a_readable_existing_file() {
        // The destination folder may hold a different edition of the album, so
        // placement keeps any file it finds that parses as audio. The upgrade
        // path replaces an equal-quality file; placement must not.
        //
        // The fixture is a 42-byte FLAC header with no audio frames, i.e. an
        // interrupted copy. It still parses, so it is kept: the guard is "lofty
        // can open it", never "the file is complete".
        let staging = TempDir::new().unwrap();
        let library = TempDir::new().unwrap();
        let album_dir = library.path().join("Test Artist").join("Test Album");
        fs::create_dir_all(&album_dir).unwrap();

        let existing = album_dir.join("01 - Track One.flac");
        write_minimal_flac(&existing);
        let existing_bytes = fs::read(&existing).unwrap();
        assert_eq!(
            existing_bytes.len(),
            42,
            "fixture must be header-only so the test proves completeness is not checked"
        );

        let src = staging.path().join("01 - Track One.flac");
        write_minimal_flac(&src);

        let dests = place_into_library(
            std::slice::from_ref(&src),
            library.path(),
            "%artist%/%album%/%track% - %title%.%ext%",
            "Test Artist",
            "Test Album",
        )
        .unwrap()
        .written;

        assert!(
            dests.is_empty(),
            "an existing readable file must not be reported as newly written"
        );
        assert_eq!(
            fs::read(&existing).unwrap(),
            existing_bytes,
            "the existing file must be byte-identical after placement"
        );
        assert!(src.exists(), "staging file preserved (copy, not move)");
    }

    #[test]
    fn test_place_into_library_replaces_a_file_that_does_not_parse_as_audio() {
        // Only a file lofty cannot open is replaceable. Junk bytes are used
        // rather than a truncated audio file: a truncated copy whose header
        // survived still parses and is deliberately kept.
        let staging = TempDir::new().unwrap();
        let library = TempDir::new().unwrap();
        let album_dir = library.path().join("Test Artist").join("Test Album");
        fs::create_dir_all(&album_dir).unwrap();

        let existing = album_dir.join("01 - Track One.flac");
        fs::write(&existing, b"truncated").unwrap();

        let src = staging.path().join("01 - Track One.flac");
        write_minimal_flac(&src);
        let src_bytes = fs::read(&src).unwrap();

        let dests = place_into_library(
            std::slice::from_ref(&src),
            library.path(),
            "%artist%/%album%/%track% - %title%.%ext%",
            "Test Artist",
            "Test Album",
        )
        .unwrap()
        .written;

        assert_eq!(dests.len(), 1, "an unparseable file is replaced");
        assert_eq!(fs::read(&existing).unwrap(), src_bytes);
    }

    #[test]
    fn test_place_into_library_does_not_cascade_placeholders_from_the_folder_name() {
        // A folder legitimately named "%album%" must stay a literal component.
        // %artist% is substituted last for exactly this reason: substituting it
        // first would let the folder name expand into the album field.
        let staging = TempDir::new().unwrap();
        let library = TempDir::new().unwrap();
        let artist_dir = library.path().join("%album%");
        fs::create_dir_all(&artist_dir).unwrap();

        let src = staging.path().join("01 - Track One.flac");
        fs::write(&src, b"fake flac data").unwrap();

        let dests = place_into_library(
            std::slice::from_ref(&src),
            library.path(),
            "%artist%/%album%/%track% - %title%.%ext%",
            "%album%",
            "Test Album",
        )
        .unwrap()
        .written;

        assert_eq!(
            dests[0],
            artist_dir.join("Test Album").join("01 - Track One.flac"),
            "the literal folder name must not expand into the album component"
        );
    }

    #[test]
    fn test_place_into_library_rejects_an_artist_value_that_is_not_one_component() {
        // `artist_dir` is substituted verbatim, so a value carrying a separator
        // or a parent component would write outside the library.
        let staging = TempDir::new().unwrap();
        let library = TempDir::new().unwrap();
        let src = staging.path().join("01 - Track One.flac");
        fs::write(&src, b"fake flac data").unwrap();

        for artist_dir in ["../outside", "a/b", "/absolute", "", ".", ".."] {
            let error = place_into_library(
                std::slice::from_ref(&src),
                library.path(),
                "%artist%/%album%/%track% - %title%.%ext%",
                artist_dir,
                "Test Album",
            )
            .expect_err("a non-component artist folder must be rejected");
            assert!(
                error.to_string().contains("single path component"),
                "{artist_dir:?} produced: {error}"
            );
        }
        assert!(
            !library.path().join("..").join("outside").exists(),
            "nothing may be created outside the library"
        );
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

    // ── Portable component sanitisation (Linux write, Windows read) ──

    #[test]
    fn test_sanitize_component_strips_windows_reserved_characters() {
        // A name carrying any of < > : " | ? * cannot be written on NTFS, and a
        // Linux-written folder holding one is shown by Windows Explorer as a
        // mangled 8.3 short name. The processor removes them outright so the
        // Linux name and the Windows-visible name are the same string.
        assert_eq!(sanitize_component("A<B>C:D\"E|F?G*H"), "ABCDEFGH");
    }

    #[test]
    fn test_sanitize_component_strips_control_characters() {
        // Characters 0x01-0x1F are reserved on Windows and render as garbage.
        assert_eq!(
            sanitize_component("Line\tOne\nTwo\u{1}Three\u{1f}Four"),
            "LineOneTwoThreeFour"
        );
    }

    #[test]
    fn test_sanitize_component_strips_trailing_dots_and_spaces() {
        // Windows silently drops a trailing dot or space, so a Linux folder
        // named "Album." and the "Album" Explorer shows would be two names for
        // one folder. Trimming at write time keeps both sides identical.
        assert_eq!(sanitize_component("Album. "), "Album");
        assert_eq!(sanitize_component("Album.."), "Album");
        assert_eq!(sanitize_component("  Album  "), "Album");
        // Removing a dot run can expose the whitespace before it, and removing
        // that whitespace can in turn expose another dot. Trimming must run to a
        // fixpoint, or the component still ends in the dot Windows drops.
        assert_eq!(sanitize_component("Album. ."), "Album");
        assert_eq!(sanitize_component("Album. . ."), "Album");
        // Interior dots carry meaning and must survive.
        assert_eq!(sanitize_component("Album.Vol.2"), "Album.Vol.2");
    }

    #[test]
    fn test_sanitize_component_neutralises_reserved_device_names() {
        // CON, PRN, AUX, NUL, COM1-9 and LPT1-9 name devices on Windows
        // wherever they appear, so they can never be folders. Only an exact
        // match is affected: "CONCERT" and "Comedy" are ordinary names.
        for name in [
            "CON", "con", "PRN", "AUX", "NUL", "Nul.txt", "COM1", "com9", "LPT1", "lpt9.mp3",
        ] {
            let sanitized = sanitize_component(name);
            assert_ne!(sanitized, name, "{name:?} is a reserved device name");
            assert!(!sanitized.is_empty());
        }
        assert_eq!(sanitize_component("CONCERT"), "CONCERT");
        assert_eq!(sanitize_component("Con Todo"), "Con Todo");
        assert_eq!(sanitize_component("Comedy"), "Comedy");
    }

    #[test]
    fn test_sanitize_component_never_yields_an_empty_component() {
        // An all-illegal title would otherwise collapse a path level and write
        // the album beside the artist folder instead of inside it.
        assert_eq!(sanitize_component("???"), "_");
        assert_eq!(sanitize_component("   "), "_");
        assert!(!sanitize_component("..").is_empty());
    }

    #[test]
    fn test_sanitize_component_is_idempotent() {
        // The album-presence check runs the sanitiser over a name that was
        // itself sanitised on write, so a second pass must be a no-op.
        for value in [
            "Tronic Jazz: The Berlin Sessions",
            "NUL.txt",
            "???",
            "Album..",
            "Album. .",
            "a..b",
            "100% Pure",
        ] {
            let once = sanitize_component(value);
            assert_eq!(
                sanitize_component(&once),
                once,
                "not idempotent for {value:?}"
            );
        }
    }

    #[test]
    fn test_place_into_library_strips_unsafe_characters_from_the_album_folder() {
        // The discover path: a MusicBrainz title carrying a colon must not reach
        // the filesystem, or Windows cannot render the album folder.
        let staging = TempDir::new().unwrap();
        let library = TempDir::new().unwrap();
        let src = staging.path().join("01 - Track One.flac");
        fs::write(&src, b"fake flac data").unwrap();

        let dests = place_into_library(
            std::slice::from_ref(&src),
            library.path(),
            "%artist%/%album%/%track% - %title%.%ext%",
            "A Guy Called Gerald",
            "Tronic Jazz: The Berlin Sessions",
        )
        .unwrap()
        .written;

        assert_eq!(dests.len(), 1);
        assert_eq!(
            dests[0],
            library
                .path()
                .join("A Guy Called Gerald/Tronic Jazz The Berlin Sessions/01 - Track One.flac")
        );
    }

    #[test]
    fn test_organize_file_strips_unsafe_characters_from_album_and_title() {
        // The auto path: album and title both come from remote metadata, and
        // the default pattern puts both into the destination path.
        let staging = TempDir::new().unwrap();
        let library = TempDir::new().unwrap();
        let src = staging.path().join("01 - Song.flac");
        fs::write(&src, b"fake flac content").unwrap();

        let destination = organize_file(OrganizeInput {
            src: &src,
            library_root: library.path(),
            pattern: "%artist%/%album%/%track% - %title%.%ext%",
            artist: "Aerosmith",
            album: "Tough Love: Best of the Ballads",
            track: "01",
            title: "Pink: The Song?",
            ext: "flac",
        })
        .unwrap()
        .written[0]
            .clone();

        assert_eq!(
            destination,
            library
                .path()
                .join("Aerosmith/Tough Love Best of the Ballads/01 - Pink The Song.flac")
        );
    }

    #[test]
    fn test_copy_to_library_sanitises_a_peer_supplied_disc_folder_name() {
        // A peer supplies the disc folder leaf, and an embedded marker such as
        // "Gold: Disc 1" is used as a real path component. It must be sanitised
        // like every other component, or the peer controls the folder name.
        let staging = TempDir::new().unwrap();
        let library = TempDir::new().unwrap();
        let disc = staging.path().join("Gold: Disc 1");
        fs::create_dir_all(&disc).unwrap();
        let src = disc.join("01 - Track One.flac");
        fs::write(&src, b"fake flac data").unwrap();

        let dests = copy_to_library(
            std::slice::from_ref(&src),
            library.path(),
            "%artist%/%album%/%track% - %title%.%ext%",
            "Test Artist",
            "Test Album",
        )
        .unwrap()
        .written;

        assert_eq!(dests.len(), 1);
        assert_eq!(
            dests[0],
            library
                .path()
                .join("Test Artist/Test Album/Gold Disc 1/01 - Track One.flac")
        );
    }

    #[test]
    fn test_resume_library_upgrade_sanitises_staging_file_names() {
        // Staging file names are the peer's own basenames, and the forward copy
        // runs them through the pattern's sanitiser. The resume walk must apply
        // the same sanitiser, or recovery creates a name the library's own write
        // path refuses to create — the SMB short-name defect itself. Only the
        // character divergence is closed here: the forward name is also
        // pattern-derived (zero-padded track numbers, a stripped leading track
        // token), so a peer name such as `1 - Track.flac` still differs from the
        // forward `01 - Track.flac` and is copied again under its own name.
        let staging = TempDir::new().unwrap();
        let library = TempDir::new().unwrap();
        fs::write(
            staging.path().join("01 - Cryin': The Blues.flac"),
            b"content",
        )
        .unwrap();

        let config = Config::default();
        resume_library_upgrade(&config, staging.path(), library.path(), "Artist", "Album").unwrap();

        assert!(library
            .path()
            .join("Artist/Album/01 - Cryin' The Blues.flac")
            .exists());
        assert!(
            !library
                .path()
                .join("Artist/Album/01 - Cryin': The Blues.flac")
                .exists(),
            "the peer's raw file name must not reach the library"
        );
    }

    #[test]
    fn test_resume_library_upgrade_sanitises_staging_subdirectory_names() {
        // Resume re-copies staging 1:1 into the library, so it must apply the
        // same sanitiser as the forward copy or the two disagree on the path.
        let staging = TempDir::new().unwrap();
        let library = TempDir::new().unwrap();
        let disc = staging.path().join("Gold: Disc 1");
        fs::create_dir_all(&disc).unwrap();
        fs::write(disc.join("01 - Track.flac"), b"content").unwrap();

        let config = Config::default();
        resume_library_upgrade(&config, staging.path(), library.path(), "Artist", "Album").unwrap();

        assert!(library
            .path()
            .join("Artist/Album/Gold Disc 1/01 - Track.flac")
            .exists());
    }

    // ── Post-download verification helpers ──

    #[test]
    fn test_extract_bitdepth_from_lossless_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.flac");
        write_minimal_flac(&path);
        assert_eq!(extract_bitdepth(&path), Some(16));
    }

    #[test]
    fn test_extract_bitrate_from_lossless_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.flac");
        write_minimal_flac(&path);
        assert_eq!(extract_bitrate(&path), None);
    }

    #[test]
    fn test_extract_bitdepth_returns_none_for_unparseable() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("junk.flac");
        fs::write(&path, b"not a real flac").unwrap();
        assert_eq!(extract_bitdepth(&path), None);
    }

    #[test]
    fn test_extract_bitrate_returns_none_for_unparseable() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("junk.flac");
        fs::write(&path, b"not a real flac").unwrap();
        assert_eq!(extract_bitrate(&path), None);
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
    fn test_copy_to_library_preserves_multi_disc_subdirectories() {
        // Regression: when a multi-CD album is downloaded into staging, each
        // disc lands in its own CD XX subdirectory. Copying to the library
        // must preserve that structure so same-named tracks from different
        // discs (e.g. "01 - Track.flac" on both CD 01 and CD 02) do not
        // collide in a single flat album directory.
        let staging = TempDir::new().unwrap();
        let library = TempDir::new().unwrap();

        // Staging layout mirrors download.rs: Artist--Album/CD 01/...,
        // Artist--Album/CD 02/...
        let cd1_dir = staging.path().join("CD 01");
        let cd2_dir = staging.path().join("CD 02");
        fs::create_dir_all(&cd1_dir).unwrap();
        fs::create_dir_all(&cd2_dir).unwrap();

        let cd1_t1 = cd1_dir.join("01 - Track.flac");
        let cd1_t2 = cd1_dir.join("02 - Track.flac");
        let cd2_t1 = cd2_dir.join("01 - Track.flac");
        let cd2_t2 = cd2_dir.join("02 - Track.flac");
        fs::write(&cd1_t1, b"cd1 track1").unwrap();
        fs::write(&cd1_t2, b"cd1 track2").unwrap();
        fs::write(&cd2_t1, b"cd2 track1").unwrap();
        fs::write(&cd2_t2, b"cd2 track2").unwrap();

        let pattern = "%artist%/%album%/%track% - %title%.%ext%";
        let files = vec![
            cd1_t1.clone(),
            cd1_t2.clone(),
            cd2_t1.clone(),
            cd2_t2.clone(),
        ];
        copy_to_library(&files, library.path(), pattern, "Test Artist", "Test Album").unwrap();

        // Each disc's tracks must land in its own subdirectory under the album.
        assert!(library
            .path()
            .join("Test Artist/Test Album/CD 01/01 - Track.flac")
            .exists());
        assert!(library
            .path()
            .join("Test Artist/Test Album/CD 01/02 - Track.flac")
            .exists());
        assert!(library
            .path()
            .join("Test Artist/Test Album/CD 02/01 - Track.flac")
            .exists());
        assert!(library
            .path()
            .join("Test Artist/Test Album/CD 02/02 - Track.flac")
            .exists());
    }

    #[test]
    fn copy_to_library_reports_the_album_folder_not_the_disc_folder() {
        // The completion log names the album folder. For a multi-disc album the
        // destination of the first written file is the *disc* folder, so a
        // caller deriving the album folder from `written[0].parent()` would
        // report ".../Test Album/CD 01" instead of ".../Test Album".
        let staging = TempDir::new().unwrap();
        let library = TempDir::new().unwrap();

        let cd1_dir = staging.path().join("CD 01");
        let cd2_dir = staging.path().join("CD 02");
        fs::create_dir_all(&cd1_dir).unwrap();
        fs::create_dir_all(&cd2_dir).unwrap();
        let cd1_t1 = cd1_dir.join("01 - Track.flac");
        let cd2_t1 = cd2_dir.join("01 - Track.flac");
        fs::write(&cd1_t1, b"cd1 track1").unwrap();
        fs::write(&cd2_t1, b"cd2 track1").unwrap();

        let pattern = "%artist%/%album%/%track% - %title%.%ext%";
        let outcome = copy_to_library(
            &[cd1_t1, cd2_t1],
            library.path(),
            pattern,
            "Test Artist",
            "Test Album",
        )
        .unwrap();

        assert_eq!(
            outcome.album_dir,
            library.path().join("Test Artist/Test Album"),
            "the album folder must exclude the disc subdirectory"
        );
        assert_eq!(outcome.written.len(), 2);
    }

    #[test]
    fn place_into_library_reports_the_album_folder_when_nothing_is_written() {
        // `written` is empty when every destination already holds audio (a
        // deliberate keep policy), yet the album is still placed. The album
        // folder must therefore be derivable without inspecting `written`.
        let staging = TempDir::new().unwrap();
        let library = TempDir::new().unwrap();

        let staged = staging.path().join("01 - Track.flac");
        write_minimal_flac(&staged);

        // Pre-seed the destination with a file that parses as audio, so the
        // placement keeps it instead of copying.
        let existing_dir = library.path().join("On Disk Artist/Test Album");
        fs::create_dir_all(&existing_dir).unwrap();
        write_minimal_flac(&existing_dir.join("01 - Track.flac"));

        let pattern = "%artist%/%album%/%track% - %title%.%ext%";
        let outcome = place_into_library(
            &[staged],
            library.path(),
            pattern,
            "On Disk Artist",
            "Test Album",
        )
        .unwrap();

        assert!(
            outcome.written.is_empty(),
            "the readable destination must be kept, not overwritten"
        );
        assert_eq!(
            outcome.album_dir,
            library.path().join("On Disk Artist/Test Album"),
            "the album folder must be reported even when nothing was written"
        );
    }

    #[test]
    fn test_copy_to_library_preserves_embedded_marker_disc_subdirectories() {
        // Regression (release-review Finding 2, library side): staging files
        // under an embedded-marker disc folder ("Gold (Disc 1)") must keep
        // that folder under the album when copied — mirrored from the
        // dedicated-folder (CD 01) case.
        let staging = TempDir::new().unwrap();
        let library = TempDir::new().unwrap();

        let d1 = staging.path().join("Gold (Disc 1)");
        let d2 = staging.path().join("Gold (Disc 2)");
        fs::create_dir_all(&d1).unwrap();
        fs::create_dir_all(&d2).unwrap();
        let d1_track = d1.join("01 - Four.flac");
        let d2_track = d2.join("01 - Four.flac");
        fs::write(&d1_track, b"disc1").unwrap();
        fs::write(&d2_track, b"disc2").unwrap();

        let pattern = "%artist%/%album%/%track% - %title%.%ext%";
        copy_to_library(
            &[d1_track.clone(), d2_track.clone()],
            library.path(),
            pattern,
            "Test Artist",
            "Test Album",
        )
        .unwrap();

        assert!(library
            .path()
            .join("Test Artist/Test Album/Gold (Disc 1)/01 - Four.flac")
            .exists());
        assert!(library
            .path()
            .join("Test Artist/Test Album/Gold (Disc 2)/01 - Four.flac")
            .exists());
    }

    #[test]
    fn test_copy_to_library_keeps_higher_quality_existing_file() {
        // Regression (release-review Finding 4): a library upgrade must never
        // downgrade an existing file. When the destination already holds a
        // strictly better copy (24-bit vs the new 16-bit download), the copy
        // is skipped and the destination is NOT reported as newly written
        // (so the quality-deletion pass cannot target it).
        let staging = TempDir::new().unwrap();
        let library = TempDir::new().unwrap();
        let album_dir = library.path().join("Test Artist").join("Test Album");
        fs::create_dir_all(&album_dir).unwrap();

        let existing = album_dir.join("01 - Track One.flac");
        write_minimal_flac24(&existing);
        let existing_bytes = fs::read(&existing).unwrap();

        let src = staging.path().join("01 - Track One.flac");
        write_minimal_flac(&src); // 16-bit — strictly worse

        let pattern = "%artist%/%album%/%track% - %title%.%ext%";
        let dests = copy_to_library(
            std::slice::from_ref(&src),
            library.path(),
            pattern,
            "Test Artist",
            "Test Album",
        )
        .unwrap()
        .written;

        assert!(
            dests.is_empty(),
            "a better existing file must not be reported as newly written"
        );
        assert_eq!(
            fs::read(&existing).unwrap(),
            existing_bytes,
            "existing 24-bit file must be untouched"
        );
        assert!(src.exists(), "staging file preserved (copy, not move)");
    }

    #[test]
    fn test_copy_to_library_replaces_lower_quality_existing_file() {
        // The reverse direction: an unparseable/corrupt existing file (score
        // 0) is replaced by a parseable new download.
        let staging = TempDir::new().unwrap();
        let library = TempDir::new().unwrap();
        let album_dir = library.path().join("Test Artist").join("Test Album");
        fs::create_dir_all(&album_dir).unwrap();

        let existing = album_dir.join("01 - Track One.flac");
        fs::write(&existing, b"corrupt junk bytes").unwrap();

        let src = staging.path().join("01 - Track One.flac");
        write_minimal_flac(&src);

        let pattern = "%artist%/%album%/%track% - %title%.%ext%";
        let dests = copy_to_library(
            std::slice::from_ref(&src),
            library.path(),
            pattern,
            "Test Artist",
            "Test Album",
        )
        .unwrap()
        .written;

        assert_eq!(dests.len(), 1);
        assert_eq!(
            fs::read(&existing).unwrap(),
            fs::read(&src).unwrap(),
            "corrupt existing file must be overwritten by the new download"
        );
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
        write_minimal_flac(&new_flac);

        let new_files = vec![new_flac];
        let deleted =
            delete_lesser_quality_files(library_root, "Artist", "Album", &new_files).unwrap();

        assert!(!old_mp3.exists(), "old MP3 should be deleted");
        assert!(cover.exists(), "cover.jpg should be preserved");
        assert!(nfo.exists(), "info.nfo should be preserved");
        assert_eq!(deleted, 1);
    }

    #[test]
    fn test_delete_lesser_quality_uses_the_album_location_as_its_root() {
        // The walk is rooted at the directory the album was found in, which for a
        // nested library is the genre directory rather than a library.paths entry.
        let dir = TempDir::new().unwrap();
        let library_root = dir.path().join("Metal");
        let album_dir = library_root.join("Artist").join("Album");
        fs::create_dir_all(&album_dir).unwrap();

        let old_mp3 = album_dir.join("01 - Old Track.mp3");
        fs::write(&old_mp3, b"mp3 content").unwrap();
        let new_flac = album_dir.join("01 - New Track.flac");
        write_minimal_flac(&new_flac);

        let deleted =
            delete_lesser_quality_files(&library_root, "Artist", "Album", &[new_flac]).unwrap();

        assert_eq!(deleted, 1, "the nested album location must be walked");
        assert!(!old_mp3.exists());
    }

    #[test]
    fn test_delete_lesser_quality_reports_a_pattern_that_misses_the_album_folder() {
        // A pattern that does not keep <artist>/<album> copies elsewhere, so the
        // walk finds no album directory: the caller logs the error and the old
        // files stay where they are.
        let dir = TempDir::new().unwrap();
        let library_root = dir.path();
        let misplaced = library_root.join("Artist - Album");
        fs::create_dir_all(&misplaced).unwrap();
        let new_flac = misplaced.join("01 - New Track.flac");
        write_minimal_flac(&new_flac);

        let result = delete_lesser_quality_files(library_root, "Artist", "Album", &[new_flac]);

        assert!(
            result.is_err(),
            "a missing album directory must be reported rather than silently ignored"
        );
    }

    #[test]
    fn test_delete_lesser_quality_preserves_better_files() {
        let dir = TempDir::new().unwrap();
        let library_root = dir.path();
        let album_dir = library_root.join("Artist").join("Album");
        fs::create_dir_all(&album_dir).unwrap();

        // Existing high-quality FLAC (real, parseable metadata).
        let old_flac = album_dir.join("01 - Track.flac");
        write_minimal_flac(&old_flac);
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
    fn test_resume_library_upgrade_preserves_multi_disc_subdirectories() {
        // Regression: an interrupted library upgrade of a multi-CD album
        // must restore the CD XX subdirectory structure, not flatten all
        // discs into a single album directory.
        let staging = TempDir::new().unwrap();
        let library = TempDir::new().unwrap();

        // Staging layout mirrors download.rs: CD 01/... and CD 02/...
        let cd1_dir = staging.path().join("CD 01");
        let cd2_dir = staging.path().join("CD 02");
        fs::create_dir_all(&cd1_dir).unwrap();
        fs::create_dir_all(&cd2_dir).unwrap();

        let cd1_t1 = cd1_dir.join("01 - Track.flac");
        let cd2_t1 = cd2_dir.join("01 - Track.flac");
        fs::write(&cd1_t1, b"cd1 content").unwrap();
        fs::write(&cd2_t1, b"cd2 content").unwrap();

        let config = Config::default();
        resume_library_upgrade(&config, staging.path(), library.path(), "Artist", "Album").unwrap();

        assert!(library
            .path()
            .join("Artist/Album/CD 01/01 - Track.flac")
            .exists());
        assert!(library
            .path()
            .join("Artist/Album/CD 02/01 - Track.flac")
            .exists());
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

    #[test]
    fn test_recover_interrupted_upgrades_cleans_failed_leftover() {
        // Regression (release-review Finding 7): a per-album staging dir left
        // behind by an interrupted DOWNLOAD (album status "failed") must be
        // removed, not ignored forever — otherwise the next download for the
        // same album reuses the directory and can interleave with stale
        // .part / partial files.
        use crate::db::Database;

        let staging_root = TempDir::new().unwrap();
        let staging = staging_root.path().join("Artist--Album");
        fs::create_dir_all(&staging).unwrap();
        fs::write(staging.join("01 - Song.flac.part"), b"partial bytes").unwrap();

        let library = TempDir::new().unwrap();
        let mut config = Config::default();
        config.library_upgrade.enabled = true;
        config.library.paths = vec![library.path().to_string_lossy().into_owned()];

        let db = Database::open_in_memory().unwrap();
        db.mark_album_processed("Artist", "Album", "failed")
            .unwrap();

        recover_interrupted_upgrades(&config, &db, staging_root.path()).unwrap();

        assert!(
            !staging.exists(),
            "failed-status leftover staging dir must be cleaned up"
        );
    }

    #[test]
    fn test_recover_interrupted_upgrades_cleans_untracked_leftover() {
        // The same, for a leftover whose album has NO DB record at all (an
        // album whose download crashed before any status could be recorded,
        // or an album-only slug).
        use crate::db::Database;

        let staging_root = TempDir::new().unwrap();
        let staging = staging_root.path().join("Unknown--Album");
        fs::create_dir_all(&staging).unwrap();
        fs::write(staging.join("cover.jpg"), b"image").unwrap();

        let library = TempDir::new().unwrap();
        let mut config = Config::default();
        config.library_upgrade.enabled = true;
        config.library.paths = vec![library.path().to_string_lossy().into_owned()];

        let db = Database::open_in_memory().unwrap(); // no record at all

        recover_interrupted_upgrades(&config, &db, staging_root.path()).unwrap();

        assert!(
            !staging.exists(),
            "untracked leftover staging dir must be cleaned up"
        );
    }
}
