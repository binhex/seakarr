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
/// in "04_Cure for Me") from a file stem so it can be used as the fixed layout's
/// title without duplicating the track number. Returns the
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
    // original to avoid producing an empty title.
    if rest >= stem.len() {
        return stem;
    }
    &stem[rest..]
}

/// Derive the track number and title a staging file stem contributes to the fixed
/// `NN - Title.ext` layout (e.g. `"02 - Track Two"` -> `("02", "Track Two")`). The
/// track number is zero-padded to two digits and the leading track token is
/// stripped from the title, so the layout yields clean names like
/// `"02 - Track Two.flac"`. Files without a parseable track
/// number fall back to track `"01"` with the stem unchanged as the title.
///
/// This is the single source of truth for library naming: every path that derives
/// a library destination (the upgrade copy and the placement check) must
/// produce the same names for the same staging files.
pub fn library_name_from_stem(stem: &str) -> (String, String) {
    let track = crate::tracks::track_number_from_filename(stem)
        .map(|n| format!("{n:02}"))
        .unwrap_or_else(|| "01".to_string());
    let title = strip_leading_track_token(stem).to_string();
    // A DISC-TRACK stem ("1-11 - Steel Bars") keeps the track number once the
    // disc number is stripped, which would duplicate it in `NN - Title.ext`.
    // Only the hyphenated form is treated this way: a title that merely starts
    // with its own track number ("01 - 1 Thing") keeps that word.
    let title = if crate::tracks::has_hyphenated_disc_prefix(stem) {
        strip_leading_track_token(&title).to_string()
    } else {
        title
    };
    (track, title)
}

/// The library-relative path one staged file is written to.
///
/// The layout is fixed: the artist component, then the sanitised album title,
/// then `NN - Title.ext`. This is the code form of the default pattern the
/// configuration used to carry, so every library write produces the paths a
/// default-configured library already holds. Every metadata value except a
/// filesystem-derived artist name is sanitised, so remote metadata cannot inject
/// a path segment.
fn library_relative_path(
    artist_dir: &str,
    album: &str,
    track: &str,
    title: &str,
    ext: &str,
) -> PathBuf {
    Path::new(artist_dir)
        .join(sanitize_component(album))
        .join(format!(
            "{} - {}.{}",
            sanitize_component(track),
            sanitize_component(title),
            sanitize_component(ext)
        ))
}

/// The album folder a placement writes into for this artist folder and album
/// title, or `None` when the artist folder name is not usable as one path
/// component.
///
/// The single source of truth for the album folder: the writer creates files
/// whose parent is exactly this path and the existing-album check reads it, so
/// the two cannot disagree.
pub fn album_dir_for(artist_parent: &Path, artist_dir: &str, album: &str) -> Option<PathBuf> {
    let mut components = Path::new(artist_dir).components();
    let single_normal = matches!(components.next(), Some(std::path::Component::Normal(_)))
        && components.next().is_none();
    if !single_normal {
        return None;
    }
    Some(
        artist_parent
            .join(artist_dir)
            .join(sanitize_component(album)),
    )
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

/// Remove path separators, null bytes, percent signs (kept out so a sanitised name
/// stays stable when it is stored and sanitised again), and directory-traversal
/// patterns from a metadata value so it cannot inject extra path segments into the
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
        .replace('%', "％"); // U+FF05 FULLWIDTH PERCENT SIGN — keeps sanitising idempotent
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

/// Result of a library write: the album folder targeted and the files written.
///
/// `album_dir` is reported to the operator as the album's final destination, so
/// it must be the album folder a human recognises — never a disc subdirectory,
/// and never a path recomputed from the sanitised file name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryWriteOutcome {
    /// The album folder the write targeted, with any disc subdirectory removed.
    /// Present even when `written` is empty.
    pub album_dir: PathBuf,
    /// Destinations actually written. Excludes a file whose destination was
    /// kept because the library already held a better or parseable copy.
    pub written: Vec<PathBuf>,
}

// ── Library upgrade: copy, quality-aware deletion, and recovery ──

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

/// Copy downloaded files from staging into the library directory. The staging
/// files are preserved (this is a copy, not a move). Track numbers are zero-padded
/// to two digits and the leading track token is stripped from the title, so files
/// land as "01 - Song.flac" under the sanitised artist and album folders.
///
/// A destination that already holds a strictly better file is kept (see the
/// per-file guard in the shared implementation below); otherwise the copy
/// replaces it. Used by the library upgrade path, where the artist name is
/// tag-derived and every metadata value is sanitised.
pub fn copy_to_library(
    downloaded: &[PathBuf],
    library_root: &Path,
    artist: &str,
    album: &str,
) -> Result<LibraryWriteOutcome> {
    let artist_dir = sanitize_component(artist);
    copy_into_library(LibraryWrite {
        downloaded,
        library_root,
        artist_dir: &artist_dir,
        album,
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
/// `artist_dir` must be a single ordinary path component, and is used verbatim as
/// the folder name: a value carrying a separator or `..` would write outside the
/// library, so the callers pass the on-disk artist folder name the library walk
/// or [`crate::discover::ArtistFolderIndex`] found, never a raw tag.
///
/// # Errors
///
/// [`SeakarrError::Config`] when `artist_dir` is not a single ordinary path
/// component, and any filesystem error the copy or directory creation returns.
pub fn place_into_library(
    downloaded: &[PathBuf],
    artist_parent: &Path,
    artist_dir: &str,
    album: &str,
) -> Result<LibraryWriteOutcome> {
    // The artist folder name comes from the library walk, which only ever produces
    // one component, but a caller could pass anything and a second component would
    // write outside the artist folder.
    if album_dir_for(artist_parent, artist_dir, album).is_none() {
        return Err(SeakarrError::Config(format!(
            "artist folder {artist_dir:?} must be a single path component"
        )));
    }
    copy_into_library(LibraryWrite {
        downloaded,
        library_root: artist_parent,
        artist_dir,
        album,
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
    artist_dir: &'a str,
    album: &'a str,
    existing_file: ExistingFile,
}

fn copy_into_library(write: LibraryWrite<'_>) -> Result<LibraryWriteOutcome> {
    let LibraryWrite {
        downloaded,
        library_root,
        artist_dir,
        album,
        existing_file,
    } = write;
    let mut written = Vec::with_capacity(downloaded.len());
    let mut album_dir: Option<PathBuf> = None;
    for src in downloaded {
        let stem = src.file_stem().unwrap_or_default().to_string_lossy();
        let ext = src.extension().unwrap_or_default().to_string_lossy();
        let (track, title) = library_name_from_stem(&stem);
        let relative = library_relative_path(artist_dir, album, &track, &title, &ext);
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
        if let Some(disc) = disc_subdir(src, album) {
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
                    if is_the_same_file(src, &dest) {
                        // The destination *is* the source, so there is nothing to
                        // keep or replace: reporting it as kept would claim a file
                        // was preserved that this run just downloaded. Falls through
                        // to the same-file guard below.
                        false
                    } else if parses_as_audio(&dest) {
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
        if is_the_same_file(src, &dest) {
            // The staging root lies inside a library root, so the album is already
            // at its destination. Copying a file onto itself truncates it.
            written.push(dest);
            continue;
        }
        fs::copy(src, &dest)?;
        tracing::debug!("Placed: {} -> {}", src.display(), dest.display());
        written.push(dest);
    }
    Ok(LibraryWriteOutcome {
        // `downloaded` is never empty for a completed album, so the fallback is
        // defensive only.
        album_dir: album_dir.unwrap_or_else(|| library_root.to_path_buf()),
        written,
    })
}

/// Remove download residue below `path`, leaving anything else in place, and
/// report whether anything was removed.
///
/// Only the files a download can leave behind are deleted. A `cover.jpg` the
/// operator added stays, and so does any directory that still holds it: the
/// residue is the interleaving hazard, not the album directory itself.
fn clear_download_residue(path: &Path) -> bool {
    let mut removed = false;
    let Ok(entries) = fs::read_dir(path) else {
        return removed;
    };
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            if clear_download_residue(&entry.path()) {
                removed = true;
            }
            match fs::remove_dir(entry.path()) {
                Ok(()) => removed = true,
                Err(e) => {
                    tracing::debug!("Left the subdirectory {:?} in place: {e}", entry.path());
                }
            }
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
        if name.ends_with(".part") || name.ends_with(".tmp") || parses_as_audio(&entry.path()) {
            match fs::remove_file(entry.path()) {
                Ok(()) => removed = true,
                Err(e) => tracing::warn!(
                    "Failed to remove the download residue {:?}: {e}",
                    entry.path()
                ),
            }
        }
    }
    removed
}

/// True when `path` holds anything an interrupted download can leave behind: an
/// audio file it managed to fetch, or a partial file. A directory with neither is
/// not download debris, and is left alone with a warning rather than deleted —
/// the operator may have put content there, and the layout makes every child of
/// an artist level look like an album.
fn holds_download_residue(path: &Path) -> bool {
    let Ok(entries) = fs::read_dir(path) else {
        return false;
    };
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            if holds_download_residue(&entry.path()) {
                return true;
            }
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
        if name.ends_with(".part") || name.ends_with(".tmp") || parses_as_audio(&entry.path()) {
            return true;
        }
    }
    false
}

/// True when two paths name the same existing file. A copy must never be
/// performed onto itself: `fs::copy` opens the destination for writing before it
/// reads the source, so it truncates the file it was about to copy.
fn is_the_same_file(source: &Path, destination: &Path) -> bool {
    match (
        std::fs::canonicalize(source),
        std::fs::canonicalize(destination),
    ) {
        (Ok(source), Ok(destination)) => source == destination,
        _ => false,
    }
}

/// Drop an artist level once it holds nothing. A directory that still holds a
/// sibling album is the documented case and stays silent; any other failure is
/// logged, because it is the only signal the operator would get.
pub(crate) fn prune_artist_level(dir: &Path) {
    match fs::remove_dir(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            let holds_something = fs::read_dir(dir)
                .map(|mut entries| entries.next().is_some())
                .unwrap_or(false);
            if holds_something {
                tracing::debug!("Artist level {dir:?} still holds a sibling album");
            } else {
                tracing::warn!("Failed to remove the empty artist level {dir:?}: {e}");
            }
        }
    }
}

/// True when two paths name the same directory, whether or not it exists yet. The
/// staging layout mirrors the library layout, so a staging root inside a library
/// root makes an album's staging path and its library destination the same
/// directory; the comparison is canonical, so a symlinked or `.`-containing
/// spelling of either path still matches.
pub(crate) fn is_the_same_dir(left: &Path, right: &Path) -> bool {
    // Shared with the staging module, which resolves a path that does not exist
    // yet — a write destination may not.
    crate::staging::is_the_same_path(left, right)
}

/// True when lofty can open and parse the file, i.e. the library already holds
/// an audio file at this path. Unlike quality scoring this does not depend on
/// the format being scoreable, so a valid DSF or DFF file, which has no entry
/// in [`format_from_extension`], still counts.
pub(crate) fn parses_as_audio(path: &Path) -> bool {
    lofty::probe::Probe::open(path)
        .and_then(|probe| probe.read())
        .is_ok()
}

/// Return the disc subdirectory component of a staging source path, if the
/// file was downloaded into a per-disc folder — either a dedicated disc
/// folder ("CD 01", "Disc 2") or an album folder carrying an embedded disc
/// marker ("Gold (Disc 1)"). Files flattened directly into the album staging
/// root return `None` so their destination is unchanged.
///
/// `album` is the album title the write is for. The decision is made from the
/// file's immediate parent and nothing above it: the stager nests at most one
/// disc level inside the album directory, so the parent is either the album root
/// (files flattened into it, destination unchanged) or the peer's disc folder.
/// A parent whose name equals the sanitised album title is the album root even
/// when that title itself carries an embedded disc marker, and walking further
/// up would let an ancestor outside the album be taken for a disc label.
///
/// The returned label is the peer's own folder name, so it is run through
/// [`sanitize_component`] before it is handed back: it becomes a real path
/// component in the library, and a peer-supplied name must not reach the
/// filesystem raw.
///
/// Uses [`crate::discs`] so the destination grouping matches the download
/// stager exactly.
fn disc_subdir(src: &Path, album: &str) -> Option<String> {
    let parent = src.parent()?;
    let name = parent.file_name().and_then(|name| name.to_str())?;
    if name == sanitize_component(album) || !crate::discs::is_disc_designator(name) {
        return None;
    }
    // Only the classification is normalized across marker styles; the label
    // itself is the peer's own, so it goes through the sanitiser before it
    // becomes a library path component.
    Some(sanitize_component(name))
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
    fn walk(
        src_root: &Path,
        dest_root: &Path,
        album: &str,
        copied: &mut Vec<PathBuf>,
    ) -> Result<()> {
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
                // The album root is the source root itself, so a subdirectory whose
                // sanitised name equals the album component is the album root seen
                // again (an album whose own title is a disc designator). Its files
                // belong in the album folder, exactly as the forward copy places
                // them; recursing would add a level the completed write does not
                // have.
                if leaf == sanitize_component(album) {
                    walk(&src, dest_root, album, copied)?;
                    continue;
                }
                let dest_dir = dest_root.join(leaf);
                fs::create_dir_all(&dest_dir)?;
                walk(&src, &dest_dir, album, copied)?;
            } else {
                let dest = dest_root.join(sanitize_component(&entry.file_name().to_string_lossy()));
                if dest.exists() {
                    // The same-path check must come first: a staging root inside a
                    // library root makes source and destination one file, and the
                    // replace path below would delete it before it could be read.
                    if is_the_same_file(&src, &dest) || file_hash(&src)? == file_hash(&dest)? {
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
    walk(album_staging, &album_dir, album, &mut copied)?;

    // Clean up staging — absence of the staging dir signals a completed run. Not
    // when the staging directory *is* the library location, which happens when
    // the staging root sits inside a library root: removing it would delete the
    // album that was just recovered.
    if is_the_same_dir(album_staging, &album_dir) {
        tracing::warn!(
            "Not removing {album_staging:?}: it is the album's library location, not a staging copy (storage.staging_dir is inside a library.paths entry; keep it outside)"
        );
    } else {
        fs::remove_dir_all(album_staging)?;
    }

    // Delete lesser-quality files (if enabled).
    if config.library_upgrade.delete_lesser_quality {
        delete_lesser_quality_files(library_root, artist, album, &copied)?;
    }
    Ok(())
}

/// Parse a legacy album staging directory name (e.g. "Artist--Album") into
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
///
/// The identity of each album comes from [`crate::staging::scan`], which
/// matches the sanitised directory names against the raw names the database
/// stores, so a title the sanitiser rewrote is still recovered.
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

    // A staging root that is itself a configured library root makes the walk see
    // library content, which recovery must not touch at all.
    if config
        .library
        .paths
        .iter()
        .any(|root| is_the_same_dir(Path::new(root), staging_dir))
    {
        tracing::warn!(
            "Recovery skipped: {staging_dir:?} is a configured library path, so its contents are library albums rather than staged downloads (keep storage.staging_dir outside every library.paths entry)"
        );
        return Ok(());
    }

    for staged in crate::staging::scan(staging_dir, db)? {
        // A directory inside a configured library root is library content, not
        // staging: when storage.staging_dir sits inside a library path, the walk
        // sees that root's artist and album folders. Recovery must not delete
        // library albums that simply have no processed_albums row.
        if let Some(root) = config.library.paths.iter().find(|root| {
            let root = Path::new(root);
            // Either direction. The scan reports paths two levels below the
            // staging root, so a library root sitting deeper than that is reported
            // as an *ancestor* of itself: without the reverse test the cleanup arm
            // would delete the library below it.
            crate::staging::is_inside(root, &staged.album_dir_path)
                || crate::staging::is_inside(&staged.album_dir_path, root)
        }) {
            tracing::warn!(
                "Recovery skipped {:?}: it and the library root {root:?} contain one another (keep storage.staging_dir outside every library.paths entry)",
                staged.album_dir_path
            );
            continue;
        }
        let (artist, album) = match &staged.identity {
            Some(identity) => (identity.artist.clone(), identity.album.clone()),
            None => ("Unknown".to_string(), staged.dir_album.clone()),
        };
        match staged
            .identity
            .as_ref()
            .map(|identity| identity.status.as_str())
        {
            Some("success") => {
                tracing::warn!("Recovering interrupted library upgrade: {artist} - {album}");
                resume_library_upgrade(
                    config,
                    &staged.album_dir_path,
                    library_root,
                    &artist,
                    &album,
                )?;
                // This album's artist level exists only for it here, so drop it once
                // it holds nothing — the same guarantee the cleanup arm gives.
                prune_artist_level(&staged.artist_dir_path);
            }
            // "in-progress" is never written by any code path (downloads
            // record "failed" on interruption), so this arm also covers
            // crashed downloads whose run ended with a recorded failure, a
            // non-success status, or no identity at all.
            _ => {
                // Only a directory that really holds download residue is cleared,
                // and only the residue itself: the layout makes every child of an
                // artist level look like an album, so content the operator put
                // there has no row and must not be deleted.
                // An unreadable directory is not the same as an empty one: the
                // residue rule must not claim it holds nothing.
                if fs::read_dir(&staged.album_dir_path).is_err() {
                    tracing::warn!(
                        "Leaving {:?}: it could not be read, so its contents are unknown",
                        staged.album_dir_path
                    );
                    continue;
                }
                if !holds_download_residue(&staged.album_dir_path) {
                    tracing::warn!(
                        "Leaving {:?}: it holds no audio and no partial download, so it is not the debris of an interrupted download — remove it by hand if it is unwanted",
                        staged.album_dir_path
                    );
                    continue;
                }
                tracing::info!(
                    "Cleaning up leftover staging from interrupted run: {artist} - {album}"
                );
                if clear_download_residue(&staged.album_dir_path) {
                    // Remove the album directory only if the pass emptied it; a
                    // surviving `cover.jpg` keeps the directory, which is right.
                    match fs::remove_dir(&staged.album_dir_path) {
                        Ok(()) => {}
                        Err(e) => tracing::debug!(
                            "Left {:?} in place after clearing its residue: {e}",
                            staged.album_dir_path
                        ),
                    }
                }
                prune_artist_level(&staged.artist_dir_path);
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
    fn placement_writes_the_fixed_layout_without_a_pattern() {
        // The default pattern used to produce exactly this layout, so a
        // default-configured library must receive byte-identical paths after
        // the pattern is gone. The staged name exercises both naming rules:
        // the track number is zero-padded and the leading track token is
        // stripped from the title.
        let staging = tempfile::TempDir::new().unwrap();
        let library = tempfile::TempDir::new().unwrap();
        let staged = staging.path().join("2 - Track Two.flac");
        fs::write(&staged, b"fake flac data").unwrap();
        fs::create_dir_all(library.path().join("Artist")).unwrap();

        let outcome = place_into_library(
            std::slice::from_ref(&staged),
            library.path(),
            "Artist",
            "Album",
        )
        .unwrap();

        assert_eq!(
            outcome.album_dir,
            library.path().join("Artist").join("Album"),
            "the album folder is the sanitised album title under the artist folder"
        );
        assert_eq!(
            outcome.written,
            vec![library
                .path()
                .join("Artist")
                .join("Album")
                .join("02 - Track Two.flac")],
            "the track number is zero-padded and the leading track token is stripped"
        );
    }

    #[test]
    fn placement_keeps_a_staging_disc_subdirectory_under_the_album_folder() {
        // A staging layout with a disc folder must keep that folder under the album
        // and must not report it as the album folder itself.
        let staging = TempDir::new().unwrap();
        let library = TempDir::new().unwrap();
        let disc = staging
            .path()
            .join("The Artist")
            .join("The Album")
            .join("CD 01");
        fs::create_dir_all(&disc).unwrap();
        let source = disc.join("01 - One.flac");
        fs::write(&source, b"fake flac content").unwrap();
        fs::create_dir_all(library.path().join("The Artist")).unwrap();

        let outcome = place_into_library(
            std::slice::from_ref(&source),
            library.path(),
            "The Artist",
            "The Album",
        )
        .unwrap();

        assert_eq!(
            outcome.album_dir,
            library.path().join("The Artist").join("The Album"),
            "the reported album folder is never the disc folder"
        );
        assert_eq!(
            outcome.written,
            vec![library
                .path()
                .join("The Artist")
                .join("The Album")
                .join("CD 01")
                .join("01 - One.flac")],
            "the disc subdirectory is preserved under the album folder"
        );
    }

    #[test]
    fn disc_subdir_treats_an_album_named_like_a_disc_folder_as_the_root() {
        // The album root's own name can carry an embedded disc marker. The
        // decision uses the file's immediate parent, so a "Disc 1" album root is
        // never mistaken for one of its own disc subdirectories.
        let root = Path::new("/staging/Artist/Disc 1");
        let flat = root.join("01 - One.flac");
        assert_eq!(disc_subdir(&flat, "Disc 1"), None);

        let disc = root.join("CD 01").join("01 - One.flac");
        assert_eq!(disc_subdir(&disc, "Disc 1"), Some("CD 01".to_string()));
    }

    #[test]
    fn disc_subdir_does_not_take_an_ancestor_outside_the_album_for_a_disc_label() {
        // Only the immediate parent counts. A file flattened into an album whose
        // own name looks like a disc folder yields no disc label, even though the
        // artist level above it is named like one too.
        let file = Path::new("/staging/CD 01/Artist/Gold (Disc 1)/01 - One.flac");
        assert_eq!(disc_subdir(file, "Gold (Disc 1)"), None);

        // A real new-layout nested case: the peer's own disc folder inside that
        // album is still returned.
        let nested = Path::new("/staging/Artist/Gold (Disc 1)/CD 02/01.flac");
        assert_eq!(
            disc_subdir(nested, "Gold (Disc 1)"),
            Some("CD 02".to_string())
        );
    }

    #[test]
    fn disc_subdir_is_none_for_the_unknown_album_level() {
        // An artist-only run stages under the `unknown` component while the
        // library writers spell their own placeholder `Unknown`. The difference
        // cannot leak into a destination, because only the immediate parent is
        // consulted and `unknown` is not a disc designator.
        let file = Path::new("/staging/Artist/unknown/01 - One.flac");
        assert_eq!(disc_subdir(file, "unknown"), None);
    }

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
        // A folder legitimately named "%album%" must stay a literal component: the
        // on-disk artist folder name is used verbatim and is never re-expanded.
        let staging = TempDir::new().unwrap();
        let library = TempDir::new().unwrap();
        let artist_dir = library.path().join("%album%");
        fs::create_dir_all(&artist_dir).unwrap();

        let src = staging.path().join("01 - Track One.flac");
        fs::write(&src, b"fake flac data").unwrap();

        let dests = place_into_library(
            std::slice::from_ref(&src),
            library.path(),
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
        // runs them through the same sanitiser. The resume walk must apply it too,
        // or recovery creates a name the library's own write path refuses to create
        // — the SMB short-name defect itself. Only the character divergence is closed
        // here: the forward name is derived the same way (zero-padded track numbers,
        // a stripped leading track token), so a peer name such as `1 - Track.flac`
        // still differs from the forward `01 - Track.flac` and is copied again under
        // its own name.
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

        let files = vec![src1.clone(), src2.clone()];
        copy_to_library(&files, library.path(), "Test Artist", "Test Album").unwrap();

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

        // Staging layout mirrors the runner: The Artist/The Album/CD 01/...,
        // The Artist/The Album/CD 02/...
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

        let files = vec![
            cd1_t1.clone(),
            cd1_t2.clone(),
            cd2_t1.clone(),
            cd2_t2.clone(),
        ];
        copy_to_library(&files, library.path(), "Test Artist", "Test Album").unwrap();

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

        let outcome = copy_to_library(
            &[cd1_t1, cd2_t1],
            library.path(),
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

        let outcome =
            place_into_library(&[staged], library.path(), "On Disk Artist", "Test Album").unwrap();

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

        copy_to_library(
            &[d1_track.clone(), d2_track.clone()],
            library.path(),
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

        let dests = copy_to_library(
            std::slice::from_ref(&src),
            library.path(),
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

        let dests = copy_to_library(
            std::slice::from_ref(&src),
            library.path(),
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
    fn test_delete_lesser_quality_reports_an_album_folder_that_holds_no_files() {
        // A folder the album no longer occupies holds no files, so the walk finds no
        // album directory: the caller logs the error and the old files stay where
        // they are.
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
    fn is_the_same_file_recognises_one_file_reached_two_ways() {
        let root = TempDir::new().unwrap();
        let file = root.path().join("01 - One.flac");
        fs::write(&file, b"audio").unwrap();

        assert!(is_the_same_file(&file, &file));
        assert!(is_the_same_file(
            &file,
            &root.path().join(".").join("01 - One.flac")
        ));
        assert!(!is_the_same_file(&file, &root.path().join("other.flac")));
    }

    #[test]
    fn placement_does_not_copy_a_file_onto_itself() {
        // A staging root inside a library root makes source and destination one
        // file. Copying it onto itself truncates it, so the write must be skipped
        // and the album still reported as written.
        let root = TempDir::new().unwrap();
        let library = root.path().join("library");
        let album = library.join("The Artist").join("The Album");
        fs::create_dir_all(&album).unwrap();
        let track = album.join("01 - Track.flac");
        // Deliberately not valid audio, so the placement takes the replace path
        // and reaches the copy guard rather than the keep-when-valid path.
        fs::write(&track, b"not audio").unwrap();
        let before = fs::read(&track).unwrap();

        let outcome = place_into_library(
            std::slice::from_ref(&track),
            &library,
            "The Artist",
            "The Album",
        )
        .unwrap();

        assert_eq!(fs::read(&track).unwrap(), before, "the file must be intact");
        assert_eq!(outcome.written, vec![track.clone()]);
        assert_eq!(outcome.album_dir, track.parent().unwrap().to_path_buf());
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
        let staging = staging_root.path().join("Artist").join("Album");
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
        assert!(
            !staging_root.path().join("Artist").exists(),
            "a recovered album leaves no empty artist level behind"
        );
    }

    #[test]
    fn test_recover_interrupted_upgrades_resumes_an_album_with_a_sanitised_directory_name() {
        // The directory name is the sanitised title while the row holds the raw
        // title. An exact lookup would miss and the album would be deleted as
        // debris — the failure this match exists to prevent.
        use crate::db::Database;

        let staging_root = TempDir::new().unwrap();
        let staging = staging_root
            .path()
            .join("Artist")
            .join(sanitize_component("Gold: Greatest Hits"));
        fs::create_dir_all(&staging).unwrap();
        fs::write(staging.join("01 - Song.flac"), b"correct content").unwrap();

        let library = TempDir::new().unwrap();
        let mut config = Config::default();
        config.library_upgrade.enabled = true;
        config.library.paths = vec![library.path().to_string_lossy().into_owned()];

        let db = Database::open_in_memory().unwrap();
        db.mark_album_processed("Artist", "Gold: Greatest Hits", "success")
            .unwrap();

        recover_interrupted_upgrades(&config, &db, staging_root.path()).unwrap();

        assert!(
            library
                .path()
                .join("Artist")
                .join("Gold Greatest Hits")
                .join("01 - Song.flac")
                .exists(),
            "the raw row must drive the library destination, not the directory name"
        );
        assert!(
            !staging.exists(),
            "a resumed album must not be left in staging as well"
        );
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
        let staging = staging_root.path().join("Artist").join("Album");
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
        assert!(
            !staging_root.path().join("Artist").exists(),
            "the cleanup also drops the artist level it emptied"
        );
    }

    #[test]
    fn test_recover_interrupted_upgrades_leaves_a_directory_that_is_not_download_residue() {
        // The layout makes every child of an artist level look like an album, so a
        // directory the operator put there has no row and no download residue.
        // Deleting it would destroy content the migration itself refuses to touch,
        // so it is left with a warning.
        use crate::db::Database;

        let staging_root = TempDir::new().unwrap();
        let scans = staging_root.path().join("Artist").join("Scans");
        fs::create_dir_all(&scans).unwrap();
        fs::write(scans.join("cover.jpg"), b"image").unwrap();

        let library = TempDir::new().unwrap();
        let mut config = Config::default();
        config.library_upgrade.enabled = true;
        config.library.paths = vec![library.path().to_string_lossy().into_owned()];
        let db = Database::open_in_memory().unwrap();

        recover_interrupted_upgrades(&config, &db, staging_root.path()).unwrap();

        assert!(
            scans.join("cover.jpg").exists(),
            "a directory holding no audio and no partial file is not download debris"
        );
    }

    #[test]
    fn test_recover_interrupted_upgrades_cleans_untracked_leftover() {
        // The same, for a leftover whose album has NO DB record at all. An
        // album-only run stages under the sanitiser's `_` placeholder and
        // writes no row, because `mark_album_processed_if_identifiable`
        // returns early for a blank artist.
        use crate::db::Database;

        let staging_root = TempDir::new().unwrap();
        let staging = staging_root.path().join("_").join("Album");
        fs::create_dir_all(&staging).unwrap();
        // Download residue, which is what makes the directory debris: a directory
        // holding no audio and no partial file is left in place instead.
        fs::write(staging.join("01 - Song.flac.part"), b"partial bytes").unwrap();
        fs::write(staging.join("cover.jpg"), b"image").unwrap();

        let library = TempDir::new().unwrap();
        let mut config = Config::default();
        config.library_upgrade.enabled = true;
        config.library.paths = vec![library.path().to_string_lossy().into_owned()];

        let db = Database::open_in_memory().unwrap(); // no record at all

        recover_interrupted_upgrades(&config, &db, staging_root.path()).unwrap();

        assert!(
            !staging.join("01 - Song.flac.part").exists(),
            "the partial download is the interleaving hazard and must be removed"
        );
        assert!(
            staging.join("cover.jpg").exists(),
            "content that is not download residue is kept"
        );
    }

    #[test]
    fn test_recover_interrupted_upgrades_skips_a_library_root_nested_under_staging() {
        // The scan reports paths two levels below the staging root, so a library
        // root deeper than that is reported as an ancestor of itself. The guard
        // must be symmetric, or the cleanup arm deletes the library below it.
        use crate::db::Database;

        let staging = TempDir::new().unwrap();
        let library = staging.path().join("media").join("music").join("flac");
        let album = library.join("Artist").join("Album");
        fs::create_dir_all(&album).unwrap();
        write_minimal_flac(&album.join("01 - Song.flac"));

        let mut config = Config::default();
        config.library_upgrade.enabled = true;
        config.library.paths = vec![library.to_string_lossy().into_owned()];
        let db = Database::open_in_memory().unwrap();

        recover_interrupted_upgrades(&config, &db, staging.path()).unwrap();

        assert!(
            album.join("01 - Song.flac").exists(),
            "a library root below the staging root must survive a recovery pass"
        );
    }

    #[test]
    fn test_recover_interrupted_upgrades_skips_a_staging_root_that_is_a_library_root() {
        // staging_dir == library root: the walk sees the library's own artist and
        // album folders, and recovery must not delete one just because it has no
        // processed_albums row.
        use crate::db::Database;

        let library = TempDir::new().unwrap();
        let album = library.path().join("Artist").join("Album");
        fs::create_dir_all(&album).unwrap();
        write_minimal_flac(&album.join("01 - Song.flac"));

        let mut config = Config::default();
        config.library_upgrade.enabled = true;
        config.library.paths = vec![library.path().to_string_lossy().into_owned()];
        let db = Database::open_in_memory().unwrap();

        recover_interrupted_upgrades(&config, &db, library.path()).unwrap();

        assert!(
            album.join("01 - Song.flac").exists(),
            "library content must survive a recovery pass over the library root"
        );
    }
}
