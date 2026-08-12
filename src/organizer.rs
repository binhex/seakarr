use std::fs;
use std::path::{Path, PathBuf};

use crate::error::Result;

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
fn sanitize_component(value: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;
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
}
