//! Disc-designator parsing shared by the download stager (`download.rs`) and
//! the library organizer (`organizer.rs`). Both must agree on what counts as
//! a per-disc folder so multi-disc albums are staged and copied with the
//! same structure — otherwise identical basenames across discs ("01 -
//! Track.flac" on both CD 01 and CD 02) can collide in one flat directory.

use regex::Regex;
use std::sync::OnceLock;

/// True when a directory leaf is a dedicated disc designator, e.g. "CD 01",
/// "CD1", "Disc 2", "disc 02" (case-insensitive), including a designator
/// wrapped in brackets, braces, or parentheses such as "{cd1}".
pub fn is_disc_folder(leaf: &str) -> bool {
    let trimmed = leaf.trim();
    let trimmed = match (trimmed.as_bytes().first(), trimmed.as_bytes().last()) {
        (Some(b'{'), Some(b'}')) | (Some(b'['), Some(b']')) | (Some(b'('), Some(b')')) => {
            trimmed[1..trimmed.len() - 1].trim()
        }
        _ => trimmed,
    };
    parse_disc_label(trimmed).is_some()
}

fn parse_disc_label(label: &str) -> Option<u32> {
    let lower = label.trim().to_ascii_lowercase();
    let rest = lower
        .strip_prefix("disc")
        .or_else(|| lower.strip_prefix("disk"))
        .or_else(|| lower.strip_prefix("cd"))?
        .trim_start();
    let digit_count = rest
        .chars()
        .take_while(|character| character.is_ascii_digit())
        .count();
    let disc: u32 = rest[..digit_count].parse().ok()?;
    let remainder = rest[digit_count..].trim();
    if remainder.is_empty()
        || remainder
            .strip_prefix("of")
            .or_else(|| remainder.strip_prefix('/'))
            .is_some_and(|total| {
                total
                    .trim()
                    .chars()
                    .all(|character| character.is_ascii_digit())
            })
    {
        Some(disc)
    } else {
        None
    }
}

/// Strip a trailing embedded disc marker from an album folder name, e.g.
/// "Gold (Disc 1)" -> "Gold", "Album - CD 2" -> "Album", "Gold [Disc 1]"
/// -> "Gold", "1998 K And D Sessions {cd1}" -> "1998 K And D Sessions".
/// Returns None when no marker is present or the marker is the whole name
/// (a dedicated disc folder, handled by [`is_disc_folder`]).
pub fn strip_embedded_disc_marker(leaf: &str) -> Option<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(
            r"(?i)(?:\(\s*(?:cd|disc|disk)\s*\d+(?:\s*(?:of|/)\s*\d+)?\s*\)|\[\s*(?:cd|disc|disk)\s*\d+(?:\s*(?:of|/)\s*\d+)?\s*\]|\{\s*(?:cd|disc|disk)\s*\d+(?:\s*(?:of|/)\s*\d+)?\s*\}|(?:\s+|-\s*)(?:cd|disc|disk)\s*\d+(?:\s*(?:of|/)\s*\d+)?)\s*$",
        )
        .expect("valid disc-marker regex")
    });
    let m = re.find(leaf)?;
    let prefix = leaf[..m.start()].trim_end_matches([' ', '-', '–', '—', '(', '[', '{', '\t']);
    if prefix.is_empty() {
        return None;
    }
    Some(prefix.to_string())
}

/// True when a directory leaf designates a single disc of a multi-disc
/// album: either a dedicated disc folder ("CD 01", "Disc 2") or an album
/// folder carrying an embedded disc marker ("Gold (Disc 1)").
pub fn is_disc_designator(leaf: &str) -> bool {
    is_disc_folder(leaf) || strip_embedded_disc_marker(leaf).is_some()
}

/// Return the numeric disc identifier from a recognized dedicated or embedded
/// disc marker.
pub fn disc_number(leaf: &str) -> Option<u32> {
    if !is_disc_designator(leaf) {
        return None;
    }
    let lower = leaf.to_ascii_lowercase();
    let marker = ["disc", "disk", "cd"]
        .into_iter()
        .filter_map(|name| lower.rfind(name).map(|index| (index, name.len())))
        .max_by_key(|(index, _)| *index)?;
    let rest = lower[marker.0 + marker.1..].trim_start();
    let digits: String = rest
        .chars()
        .take_while(|character| character.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_disc_folder_accepts_dedicated_disc_leaves() {
        for leaf in [
            "CD 01",
            "CD1",
            "Disc 2",
            "disc 02",
            " cd3 ",
            "{cd1}",
            "[CD2]",
            "(Disc 3)",
            "Disc 1 of 2",
            "Disc 2/2",
            "Disk 3",
        ] {
            assert!(is_disc_folder(leaf), "{leaf:?} must be a disc folder");
        }
        for leaf in ["Album", "CD", "Disc", "CD1 bonus", "Album [Disc 1]"] {
            assert!(!is_disc_folder(leaf), "{leaf:?} must not be a disc folder");
        }
    }

    #[test]
    fn strip_embedded_disc_marker_removes_parenthesised_and_bracketed_markers() {
        assert_eq!(
            strip_embedded_disc_marker("Gold (Disc 1)").as_deref(),
            Some("Gold")
        );
        assert_eq!(
            strip_embedded_disc_marker("Album - CD 2").as_deref(),
            Some("Album")
        );
        assert_eq!(
            strip_embedded_disc_marker("Gold [Disc 1]").as_deref(),
            Some("Gold")
        );
    }

    // Regression: the reported Kruder & Dorfmeister artist-only run staged one
    // album as "1998 K And D Sessions {cd1}" and "{cd2}". The brace form was
    // not recognised as a disc marker, so one album became two separate
    // downloads instead of two discs of the same download.
    #[test]
    fn strip_embedded_disc_marker_removes_brace_markers() {
        assert_eq!(
            strip_embedded_disc_marker("1998 K And D Sessions {cd1}").as_deref(),
            Some("1998 K And D Sessions")
        );
        assert_eq!(
            strip_embedded_disc_marker("1998 K And D Sessions {cd2}").as_deref(),
            Some("1998 K And D Sessions")
        );
        assert_eq!(
            strip_embedded_disc_marker("Album {CD 2}").as_deref(),
            Some("Album")
        );
        assert_eq!(
            strip_embedded_disc_marker("Album {disc3}").as_deref(),
            Some("Album")
        );
        assert_eq!(
            strip_embedded_disc_marker("Album { cd4 }").as_deref(),
            Some("Album")
        );
        assert_eq!(
            strip_embedded_disc_marker("Album - CD 2 ").as_deref(),
            Some("Album")
        );
        assert_eq!(
            strip_embedded_disc_marker("Album (Disc 1 of 2)").as_deref(),
            Some("Album")
        );
        assert_eq!(
            strip_embedded_disc_marker("Album (Disc 2/2)").as_deref(),
            Some("Album")
        );
    }

    #[test]
    fn mismatched_or_unclosed_wrappers_are_not_disc_markers() {
        for leaf in ["{cd1)", "(CD 1]", "[Disc 2}", "CD 01)"] {
            assert!(!is_disc_folder(leaf), "{leaf:?} must not be a disc folder");
            assert!(
                strip_embedded_disc_marker(&format!("Album {leaf}")).is_none(),
                "{leaf:?} must not be an embedded disc marker"
            );
        }
    }

    #[test]
    fn disc_number_parses_dedicated_and_embedded_markers() {
        assert_eq!(disc_number("CD 01"), Some(1));
        assert_eq!(disc_number("Album { cd2 }"), Some(2));
        assert_eq!(disc_number("Gold (Disc 3)"), Some(3));
        assert_eq!(disc_number("Discovery CD1"), Some(1));
        assert_eq!(disc_number("Discovery {cd2}"), Some(2));
        assert_eq!(disc_number("Album (Disc 1 of 2)"), Some(1));
        assert_eq!(disc_number("Album (Disc 2/2)"), Some(2));
        assert_eq!(disc_number("Album Disk 3"), Some(3));
        assert_eq!(disc_number("Album"), None);
    }

    #[test]
    fn is_disc_designator_accepts_brace_markers() {
        assert!(is_disc_designator("1998 K And D Sessions {cd1}"));
        assert!(is_disc_designator("Album {CD 2}"));
        assert!(is_disc_designator("CD 02"));
        assert!(!is_disc_designator("1998 K And D Sessions"));
    }
}
