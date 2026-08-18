//! Track-number parsing and contiguity checks for search results.

use crate::client::FileInfo;

/// Extract a track number from a filename (or share path).
///
/// Splits on non-alphanumeric boundaries and returns the first 1-3 digit
/// all-numeric token (zero-padded counts), ignoring 4+ digit tokens such as
/// years. Covers leading numbering ("04_Cure for Me.flac") and mid-filename
/// numbering ("Linkin Park - Hybrid Theory - 11 - Cure for the Itch.flac").
/// Returns `None` when no such token exists.
///
/// For DISC-TRACK filenames (e.g. "1-11 - Title.flac", "2-16 - Title.flac"),
/// the first numeric token is the disc number and the second is the track
/// number. When the first two alphanumeric tokens are both 1–3 digit numbers,
/// the first is skipped and the second (track number) is returned.
pub fn track_number_from_filename(name: &str) -> Option<u32> {
    let basename = name.rsplit(['\\', '/']).next().unwrap_or(name);
    let tokens: Vec<&str> = basename
        .split(|c: char| !c.is_alphanumeric())
        .filter(|tok| !tok.is_empty())
        .collect();

    // For DISC-TRACK filenames (e.g. "1-11 - Title.flac"), the first
    // numeric token is the disc number and the second is the track
    // number. Skip the disc number and return the track number.
    let start = if tokens.len() >= 2
        && tokens[0].len() <= 3
        && tokens[0].chars().all(|c| c.is_ascii_digit())
        && tokens[1].len() <= 3
        && tokens[1].chars().all(|c| c.is_ascii_digit())
    {
        1
    } else {
        0
    };

    tokens[start..].iter().find_map(|tok| {
        if tok.len() > 3 || !tok.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        tok.parse::<u32>().ok()
    })
}

/// Check that a set of files carries contiguous track numbers.
///
/// Collects the track number of every file, requires at least one, then
/// verifies the sorted unique numbers have no gaps. Duplicate track numbers
/// are permitted; missing numbers are not.
pub fn files_have_contiguous_tracks(files: &[&FileInfo]) -> bool {
    let mut numbers: Vec<u32> = files
        .iter()
        .filter_map(|f| track_number_from_filename(&f.name))
        .collect();
    if numbers.is_empty() {
        return false;
    }
    numbers.sort_unstable();
    numbers.dedup();
    numbers.windows(2).all(|w| w[1] == w[0] + 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn make_file(name: &str) -> FileInfo {
        FileInfo {
            name: name.into(),
            size: 0,
            attribs: HashMap::new(),
        }
    }

    #[test]
    fn test_leading_token() {
        assert_eq!(track_number_from_filename("04_Cure for Me.flac"), Some(4));
    }

    #[test]
    fn test_leading_token_dash_separated() {
        assert_eq!(track_number_from_filename("08 - the cure.flac"), Some(8));
    }

    #[test]
    fn test_mid_filename_token() {
        assert_eq!(
            track_number_from_filename("Linkin Park - Hybrid Theory - 11 - Cure for the Itch.flac"),
            Some(11)
        );
    }

    #[test]
    fn test_four_digit_year_ignored() {
        assert_eq!(
            track_number_from_filename("Hybrid Theory (2000) - 01 - Papercut.flac"),
            Some(1)
        );
    }

    #[test]
    fn test_no_number_returns_none() {
        assert_eq!(track_number_from_filename("Cure for the Itch.flac"), None);
    }

    #[test]
    fn test_token_with_letters_returns_none() {
        assert_eq!(track_number_from_filename("Track 4a.flac"), None);
    }

    #[test]
    fn test_path_prefix_stripped() {
        assert_eq!(
            track_number_from_filename(r"shared\Linkin Park\Hybrid Theory\01 - Papercut.flac"),
            Some(1)
        );
    }

    #[test]
    fn test_first_numeric_token_wins() {
        // Multi-disc style names yield the first 1-3 digit token.
        assert_eq!(track_number_from_filename("1-01 - Title.flac"), Some(1));
    }

    #[test]
    fn test_contiguous_passes() {
        let files = [
            make_file("01 - A.flac"),
            make_file("02 - B.flac"),
            make_file("03 - C.flac"),
        ];
        let refs: Vec<&FileInfo> = files.iter().collect();
        assert!(files_have_contiguous_tracks(&refs));
    }

    #[test]
    fn test_duplicates_pass() {
        let files = [
            make_file("01 - A.flac"),
            make_file("02 - B.flac"),
            make_file("02 - B (alt).flac"),
            make_file("03 - C.flac"),
        ];
        let refs: Vec<&FileInfo> = files.iter().collect();
        assert!(files_have_contiguous_tracks(&refs));
    }

    #[test]
    fn test_gap_fails() {
        let files = [make_file("01 - A.flac"), make_file("03 - C.flac")];
        let refs: Vec<&FileInfo> = files.iter().collect();
        assert!(!files_have_contiguous_tracks(&refs));
    }

    #[test]
    fn test_production_gap_fails() {
        // The reported incident: The Cure album with tracks 04, 08, 16.
        let files = [
            make_file("04_Cure for Me.flac"),
            make_file("08_the cure.flac"),
            make_file("16_Cure for Me (acoustic).flac"),
        ];
        let refs: Vec<&FileInfo> = files.iter().collect();
        assert!(!files_have_contiguous_tracks(&refs));
    }

    #[test]
    fn test_single_track_passes() {
        let files = [make_file("07 - A.flac")];
        let refs: Vec<&FileInfo> = files.iter().collect();
        assert!(files_have_contiguous_tracks(&refs));
    }

    #[test]
    fn test_empty_or_unnumbered_fails() {
        let empty: Vec<&FileInfo> = vec![];
        assert!(!files_have_contiguous_tracks(&empty));

        let files = [make_file("Title.flac"), make_file("Another.flac")];
        let refs: Vec<&FileInfo> = files.iter().collect();
        assert!(!files_have_contiguous_tracks(&refs));
    }

    #[test]
    fn test_boundary_token_lengths() {
        // 3 digits parse; 4-digit year tokens are skipped.
        assert_eq!(track_number_from_filename("999 - Title.flac"), Some(999));
        assert_eq!(track_number_from_filename("1000 - Title.flac"), None);
        assert_eq!(
            track_number_from_filename("Album (1999) - 01 - Title.flac"),
            Some(1)
        );
    }

    #[test]
    fn test_zero_valued_tokens() {
        // Zero-padded and zero-valued tokens parse to their numeric value.
        assert_eq!(track_number_from_filename("00 - Intro.flac"), Some(0));
        assert_eq!(track_number_from_filename("000 - Intro.flac"), Some(0));
    }

    #[test]
    fn test_first_token_rule_documented_phantom_number() {
        // Documented first-token limitation: an artist name containing
        // digits ("Maroon 5") is parsed before the real track number, so
        // every file yields the phantom number and gaps are masked. This
        // behaviour is the spec-locked first-token rule.
        assert_eq!(
            track_number_from_filename("Maroon 5 - 03 - Maps.flac"),
            Some(5)
        );
    }

    #[test]
    fn test_extract_track_from_disc_track_format() {
        // DISC-TRACK filenames: "1-01 - Title.flac", "1-11 - Title.flac",
        // "2-16 - Title.flac". The first numeric token is the disc number,
        // the second is the track number. track_number_from_filename should
        // return the TRACK number, not the disc number.
        assert_eq!(
            track_number_from_filename("1-01 - Soul Provider.flac"),
            Some(1)
        );
        assert_eq!(
            track_number_from_filename("1-11 - Steel Bars.flac"),
            Some(11)
        );
        assert_eq!(
            track_number_from_filename("2-16 - Thats What Love Is All About.flac"),
            Some(16)
        );
        // Standard format still works: "01 - Song.flac" → 1
        assert_eq!(
            track_number_from_filename("01 - Soul Provider.flac"),
            Some(1)
        );
        assert_eq!(track_number_from_filename("11 - Steel Bars.flac"), Some(11));
    }
}
