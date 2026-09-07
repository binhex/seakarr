//! Disc-designator parsing shared by the download stager (`download.rs`) and
//! the library organizer (`organizer.rs`). Both must agree on what counts as
//! a per-disc folder so multi-disc albums are staged and copied with the
//! same structure — otherwise identical basenames across discs ("01 -
//! Track.flac" on both CD 01 and CD 02) can collide in one flat directory.

use regex::Regex;
use std::sync::OnceLock;

/// True when a directory leaf is a dedicated disc designator, e.g. "CD 01",
/// "CD1", "Disc 2", "disc 02" (case-insensitive).
pub fn is_disc_folder(leaf: &str) -> bool {
    let lower = leaf.trim().to_ascii_lowercase();
    let rest = lower
        .strip_prefix("cd")
        .or_else(|| lower.strip_prefix("disc"))
        .map(str::trim)
        .unwrap_or("");
    !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit())
}

/// Strip a trailing embedded disc marker from an album folder name, e.g.
/// "Gold (Disc 1)" -> "Gold", "Album - CD 2" -> "Album", "Gold [Disc 1]"
/// -> "Gold". Returns None when no marker is present or the marker is the
/// whole name (a dedicated disc folder, handled by [`is_disc_folder`]).
pub fn strip_embedded_disc_marker(leaf: &str) -> Option<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"(?i)(?:[\s(\[-])(?:cd|disc)\s*\d+\s*[)\]]?$").expect("valid disc-marker regex")
    });
    let m = re.find(leaf)?;
    let prefix = leaf[..m.start()].trim_end_matches([' ', '-', '–', '—', '(', '[', '\t']);
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
