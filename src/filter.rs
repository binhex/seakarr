use crate::client::{FileInfo, SearchResult};
use crate::config::FilterConfig;
use unicode_normalization::UnicodeNormalization;

/// Filter search results by extension, bitrate, excluded words, free slots,
/// the download-completeness rule, contiguous track numbers (when
/// `contiguous_tracks` is enabled), and — in auto mode — the library track
/// count (`peer_track_count`): results whose usable track count is below the
/// library's existing track count are rejected.
///
/// Completeness is judged on the largest album group — the files
/// `download_album` actually fetches — never on the whole result: a result must
/// hold at least `min_tracks` files in that group (or at least one when
/// `min_tracks` is 0) and, when its numbering is credible, must reach track 1.
/// See [`incomplete_download`].
///
/// When `album` is `Some`, results whose file paths do not contain the album
/// name as whole words are also rejected (primary artist+album search tier).
/// The track-name fallback tier passes `None` so it is never album-gated.
///
/// This wrapper is free-slot-only (queue cap 0), so results with no free
/// upload slots are rejected. Callers that enforce
/// `download.max_queue_length` use [`filter_results_with_queue_limit`], which
/// keeps a zero-slot candidate for download-time queue validation.
pub fn filter_results(
    results: &[SearchResult],
    config: &FilterConfig,
    library_track_count: Option<usize>,
    album: Option<&str>,
) -> Vec<SearchResult> {
    filter_results_with_queue_limit(
        results,
        config,
        library_track_count,
        album,
        0,
        TrackOneAnchor::Required,
    )
}

/// Queue-aware variant of [`filter_results`]. A zero-slot result is kept only
/// when `max_queue_length` is positive; its reported queue position is
/// validated when the download is actually queued. `anchor` selects whether the
/// completeness rule's track-1 half applies (see [`TrackOneAnchor`]).
pub(crate) fn filter_results_with_queue_limit(
    results: &[SearchResult],
    config: &FilterConfig,
    library_track_count: Option<usize>,
    album: Option<&str>,
    max_queue_length: u32,
    anchor: TrackOneAnchor,
) -> Vec<SearchResult> {
    results
        .iter()
        .filter(|r| {
            // Album-name gate (primary artist+album search tier only): when
            // the caller knows the album name, reject results whose file
            // paths do not contain it as a contiguous run of word tokens.
            // This stops substring false matches like "S Club" matching
            // "Sgt. Peppers Lonely Hearts **Club** Band". The track-name
            // fallback tier passes None and is never gated here.
            if let Some(album_name) = album {
                if !album_matches_result(r, album_name) {
                    return false;
                }
            }

            // Filter: must have free upload slots unless a positive queue
            // cap permits a bounded queue wait (the peer's reported queue
            // position is validated at download time). With the default cap
            // of 0, results with no free slots (slots == 0) are always
            // rejected. The include_locked field is defined but not yet
            // enforced.
            if r.slots == 0 && max_queue_length == 0 {
                return false;
            }

            // The downloadable set: files passing extension + bitrate +
            // word filters AND the basename safety check download_album
            // applies — the contiguity check must mirror what will really
            // be downloaded, or an unsafe-named numbered track would pass
            // here and then be dropped at download time, recreating the
            // gap this feature exists to prevent.
            let safe_and_passing = |f: &FileInfo| {
                crate::download::safe_basename(&f.name).is_ok() && file_passes_filters(f, config)
            };
            if !config.contiguous_tracks {
                // Toggle off: count safe, quality-passing files (mirroring
                // download_album) and reject incomplete shares. min_tracks == 0
                // keeps the "at least one usable file" floor when the gate is
                // disabled (0).
                let passing: Vec<&FileInfo> =
                    r.files.iter().filter(|f| safe_and_passing(f)).collect();
                // Same completeness rule as the contiguous branch, over the same
                // downloadable set. min_tracks == 0 disables both halves but
                // never accepts a result with zero usable files.
                if let Some(kind) =
                    incomplete_download(&downloadable_basenames(&passing), config.min_tracks, anchor)
                {
                    tracing::debug!("result from {} rejected: {}", r.username, kind.filter_reason());
                    return false;
                }
                if passing.is_empty() {
                    return false;
                }
                // Library track count check (auto mode only).
                // Note: the count mirrors what download_album will actually
                // download — a peer's SearchResult can span multiple album
                // directories (original edition + anniversary edition both
                // matching the query), and download_album keeps only the
                // LARGEST single directory group. Comparing the total passing
                // file count against the library lets a multi-directory peer
                // through whose largest album is below the library count,
                // which then fails the post-download completeness gate.
                // The count is of files passing quality filters, not unique
                // track numbers — duplicate filenames are counted separately,
                // matching the library's own file-based count.
                if let Some(lib_count) = library_track_count {
                    if config.peer_track_count
                        && crate::download::largest_album_group(&passing).len() < lib_count
                    {
                        tracing::debug!(
                            "result from {} rejected: largest album directory has {} tracks < library track count {}",
                            r.username,
                            crate::download::largest_album_group(&passing).len(),
                            lib_count
                        );
                        return false;
                    }
                }
                return true;
            }
            let passing: Vec<&FileInfo> = r.files.iter().filter(|f| safe_and_passing(f)).collect();
            // The completeness gate judges the set that will actually be
            // downloaded — the largest single album directory — with the same
            // rule the post-download library write applies. Counting every
            // passing file in the result instead let a peer through whose album
            // group was a fragment: the whole album was downloaded, refused at
            // the library write, and left in the staging directory.
            if let Some(kind) =
                incomplete_download(&downloadable_basenames(&passing), config.min_tracks, anchor)
            {
                tracing::debug!("result from {} rejected: {}", r.username, kind.filter_reason());
                return false;
            }
            if passing.is_empty() {
                return false;
            }
            if !files_have_contiguous_tracks_per_directory(&passing) {
                // Distinguish the two rejection causes for operators.
                let any_numbered = passing
                    .iter()
                    .any(|f| crate::tracks::track_number_from_filename(&f.name).is_some());
                tracing::debug!(
                    "result from {} rejected: {}",
                    r.username,
                    if any_numbered {
                        "non-contiguous track numbers in one or more directories"
                    } else {
                        "no parseable track numbers"
                    }
                );
                return false;
            }
            // Library track count check (auto mode only).
            // The count mirrors download_album: a peer's SearchResult can
            // span multiple album directories, and only the LARGEST single
            // directory group is ever downloaded. Comparing the total
            // passing count against the library would let a multi-directory
            // peer through whose largest album is below the library count,
            // which then fails the post-download completeness gate.
            // Note: with the default min_tracks=3, albums with 1-2 tracks
            // (EPs, singles) are already rejected by the min_tracks gate
            // before this check runs. To apply the library check to EPs,
            // set min_tracks to 0 or 1.
            if let Some(lib_count) = library_track_count {
                if config.peer_track_count
                    && crate::download::largest_album_group(&passing).len() < lib_count
                {
                    tracing::debug!(
                        "result from {} rejected: largest album directory has {} tracks < library track count {}",
                        r.username,
                        crate::download::largest_album_group(&passing).len(),
                        lib_count
                    );
                    return false;
                }
            }
            // Edge case: peers sharing multiple files with the same track
            // number (e.g. two "01 - Intro.flac" with different codecs)
            // can pass count+contiguity but collapse to one file during
            // download (safe_basename deduplication). The last file in
            // name-sorted order survives; downloaded.len() may overcount
            // relative to unique tracks. Accepted by design.
            true
        })
        .cloned()
        .collect()
}

/// Whether the completeness rule's track-1 anchor applies to a search.
///
/// The anchor asks whether a downloaded set reaches track 1, which is what makes a
/// set a complete *new* album. A library-upgrade candidate is judged differently:
/// the library already holds its own track 1, and the upgrade only replaces the
/// files that failed the quality gate, so a peer delivering just those files is a
/// legitimate source. This mirrors where the post-download gate applies the anchor
/// (`library_write_refusal` on the placement and organize paths) and where it does
/// not (the upgrade path's `expected_tracks`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TrackOneAnchor {
    /// A new album: the downloaded set must reach track 1.
    Required,
    /// A library-upgrade candidate: only the files needing replacement must arrive.
    NotRequired,
}

/// Why a downloaded set is not a complete album.
///
/// The pre-download filter, the rejection summary and the post-download library
/// write all branch on this classification instead of re-deriving half of the
/// rule themselves, which is how the pre-download and post-download gates drifted
/// apart in the first place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IncompleteDownload {
    /// Fewer files in the downloadable set than `min_tracks` allows.
    TooShort { found: usize, min_tracks: u32 },
    /// The numbering is credible, has at least two distinct values, and never
    /// reaches track 1.
    MissingTrackOne { start: u32 },
}

impl IncompleteDownload {
    /// The operator-facing reason for a refusal after the download, as reported
    /// in the `Failed` outcome and its warning.
    pub(crate) fn reason(self) -> String {
        match self {
            IncompleteDownload::TooShort { found, min_tracks } => {
                format!("only {found} of at least {min_tracks} tracks were downloaded")
            }
            IncompleteDownload::MissingTrackOne { start } => {
                format!("the tracks start at track {start} instead of 1")
            }
        }
    }

    /// The same refusal as seen by the pre-download filter, where nothing has been
    /// transferred yet: it names the downloadable set rather than a download.
    pub(crate) fn filter_reason(self) -> String {
        match self {
            IncompleteDownload::TooShort { found, min_tracks } => {
                format!("only {found} of at least {min_tracks} tracks in the largest album group")
            }
            IncompleteDownload::MissingTrackOne { start } => {
                format!("the tracks start at track {start} instead of 1")
            }
        }
    }
}

/// The download-completeness rule, shared by the pre-download filter and the
/// post-download library write. Both apply it to the same set — the files of the
/// largest album group, which is what `download_album` fetches — through the same
/// quality filter and the same grouping, so the two see identical name lists by
/// construction. The pre-download call is the decisive one; the library-write call
/// is a defensive backstop for a future change to the download layer.
///
/// `names` are the basenames of the files that will be (or were) downloaded, and
/// `min_tracks` disables both halves when it is 0, matching the documented escape
/// hatch for EPs and singles. The count half always applies; the numbering half
/// applies only when `anchor` is [`TrackOneAnchor::Required`].
///
/// A set is refused when it is shorter than `min_tracks`, or when its numbered
/// files are credible and none of them is track 1 — a gap-free run of 02..10 is
/// still a fragment of something longer. The numbering half is deliberately
/// cautious, and misses several real fragments as a result. It judges a set
/// only when **every** file parsed a number, so a mixed set — an `Intro.flac`
/// beside `02 - Two.flac` — is left alone. It also requires at least **two
/// distinct** parsed values, because `track_number_from_filename` reads the
/// *first* numeric token: every file of
/// `Blink 182 - Enema of the State - 01 - Dumpweed.flac` reports track 182, and
/// a single repeated value is indistinguishable from a set that genuinely
/// repeats one track number, which the project supports. A lone numbered file
/// is not judged either, since one value can never be distinct.
///
/// Track numbers are read as tracks, not as fragments, when they are `1` or
/// when their last two digits are `01`: a rip that fuses the disc and track
/// number (`101` for disc 1 track 1) counts as starting at track 1, just as the
/// hyphenated `1-01` form does.
pub(crate) fn incomplete_download(
    names: &[&str],
    min_tracks: u32,
    anchor: TrackOneAnchor,
) -> Option<IncompleteDownload> {
    if min_tracks == 0 {
        return None;
    }
    if names.len() < min_tracks as usize {
        return Some(IncompleteDownload::TooShort {
            found: names.len(),
            min_tracks,
        });
    }
    if anchor == TrackOneAnchor::NotRequired {
        return None;
    }
    let numbers: Vec<u32> = names
        .iter()
        .filter_map(|name| crate::tracks::track_number_from_filename(name))
        .collect();
    let mut distinct = numbers.clone();
    distinct.sort_unstable();
    distinct.dedup();
    if numbers.len() == names.len() {
        if let Some(&start) = distinct.first() {
            // A disc and track fused into one value ("101" for disc 1 track 1,
            // "205" for disc 2 track 5) carries the same signal as the
            // hyphenated "1-01" form, which `track_number_from_filename`
            // already unwraps, so the same modulo test applies to the fused
            // spelling.
            let starts_at_track_one = distinct.iter().any(|number| number % 100 == 1);
            if distinct.len() >= 2 && !starts_at_track_one {
                return Some(IncompleteDownload::MissingTrackOne { start });
            }
        }
    }
    None
}

/// Basenames of the files `download_album` would fetch for this result: the
/// largest single album group, with the discs of a multi-disc album collapsed
/// into one group exactly as the download does.
fn downloadable_basenames<'a>(passing: &[&'a FileInfo]) -> Vec<&'a str> {
    crate::download::largest_album_group(passing)
        .iter()
        .map(|file| {
            file.name
                .rsplit_once(['/', '\\'])
                .map(|(_, basename)| basename)
                .unwrap_or(file.name.as_str())
        })
        .collect()
}

/// Check that every directory group in `files` carries contiguous track
/// numbers on its own. A peer's SearchResult can span multiple album
/// directories (multi-disc albums: "CD 01" + "CD 02" subfolders, or
/// embedded markers like "Gold (Disc 1)" / "Gold (Disc 2)", or DISC-TRACK
/// filenames like "1-01 - Title.flac"). Each disc must be complete
/// independently — a gap in one disc (e.g. CD 01 missing tracks 11 and 13)
/// must not be masked by another disc carrying those numbers. Files are
/// grouped by their raw parent directory AND disc number, and each group
/// must pass [`crate::tracks::files_have_contiguous_tracks`].
fn files_have_contiguous_tracks_per_directory(files: &[&FileInfo]) -> bool {
    let mut groups: std::collections::HashMap<(String, Option<u32>), Vec<&FileInfo>> =
        Default::default();
    for f in files {
        // Raw parent directory (everything before the last separator).
        // Deliberately NOT run through album_group_key: discs must stay
        // separate so each is validated on its own.
        let dir = f
            .name
            .rsplit_once(['/', '\\'])
            .map(|(parent, _basename)| parent)
            .filter(|p| !p.is_empty())
            .unwrap_or("<root>")
            .to_string();
        // Sub-group by disc number for DISC-TRACK filenames so that a
        // flat directory holding "1-01..1-16" and "2-01..2-16" files
        // validates each disc independently — CD 02 must not mask a gap
        // in CD 01. Standard filenames have no disc number (None) and
        // group together per directory.
        let disc = crate::tracks::disc_number_from_filename(&f.name);
        groups.entry((dir, disc)).or_default().push(f);
    }
    groups
        .values()
        .all(|g| crate::tracks::files_have_contiguous_tracks(g))
}

pub(crate) fn file_passes_filters(file: &FileInfo, config: &FilterConfig) -> bool {
    // Extension check
    let ext = file.name.rsplit('.').next().unwrap_or("").to_lowercase();
    if !config
        .allowed_extensions
        .iter()
        .any(|e| e.to_lowercase() == ext)
    {
        return false;
    }
    // Bitrate check (key 0 = bitrate in kbps). When a minimum is configured
    // and the peer provides bitrate, reject files below the minimum. When the
    // peer does NOT provide bitrate, let the file pass — it will be verified
    // post-download using actual file metadata (lofty).
    if config.min_bit_rate > 0 {
        if let Some(&file_br) = file.attribs.get(&0) {
            if file_br < config.min_bit_rate {
                return false;
            }
        }
        // attribs.get(&0) == None → pass (verify post-download)
    }
    // Bitdepth check (key 5 = bit depth). Same semantics as the bitrate
    // check: reject only when the peer PROVIDES a bitdepth below the
    // minimum; a missing bitdepth passes and is verified post-download.
    if config.min_bit_depth > 0 {
        if let Some(&file_bd) = file.attribs.get(&5) {
            if file_bd < config.min_bit_depth {
                return false;
            }
        }
        // attribs.get(&5) == None → pass (verify post-download)
    }
    // Excluded words check
    let lower_name = file.name.to_lowercase();
    if config
        .exclude_words
        .iter()
        .any(|w| lower_name.contains(&w.to_lowercase()))
    {
        return false;
    }
    true
}

/// Split a name into lowercase alphanumeric word tokens, collapsing
/// separators (spaces, punctuation, path separators).
///
/// `"S Club"` → `["s", "club"]`; `"Sgt. Peppers Lonely Hearts Club Band"`
/// → `["sgt", "peppers", "lonely", "hearts", "club", "band"]`.
fn word_tokens(name: &str) -> Vec<String> {
    name.nfkd()
        .filter(|c| c.is_ascii())
        .collect::<String>()
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect()
}

/// Whether `tokens` contains `needle` as a contiguous run of words.
///
/// Word-boundary matching is essential: a naive substring check would let
/// "S Club" match "Sgt. Peppers Lonely Hearts **Club** Band" (the "s" of
/// "hearts" + "club") — the exact false match this check exists to prevent.
fn tokens_contain_contiguous(tokens: &[String], needle: &[String]) -> bool {
    if needle.is_empty() {
        return false;
    }
    tokens.windows(needle.len()).any(|w| w == needle)
}

/// Whether any of a result's file paths contains the album name as a
/// contiguous run of word tokens.
fn album_matches_result(result: &SearchResult, album: &str) -> bool {
    let album_tokens = word_tokens(album);
    // An empty token set (punctuation-only album name like "( )", or
    // whitespace-only) carries no discriminative constraint — skip the
    // gate so the album can still be processed by other filters.
    if album_tokens.is_empty() {
        return true;
    }
    result.files.iter().any(|f| {
        let path_tokens = word_tokens(&f.name);
        tokens_contain_contiguous(&path_tokens, &album_tokens)
    })
}

/// Match strength of a result against an album name, for ranking:
/// - `2`: the album name appears in the file's album folder (the parent
///   directory path components before the basename) — strongest signal.
/// - `1`: the album name appears somewhere in the file path.
/// - `0`: no match.
fn album_match_strength(result: &SearchResult, album: &str) -> u8 {
    let album_tokens = word_tokens(album);
    if album_tokens.is_empty() {
        return 1; // no discriminative content — treat as baseline match
    }
    let mut best = 0u8;
    for f in &result.files {
        let path_tokens = word_tokens(&f.name);
        if !tokens_contain_contiguous(&path_tokens, &album_tokens) {
            continue;
        }
        // Folder-level match: album name appears in the parent directory
        // (everything before the last path separator / basename).
        let parent = f
            .name
            .rsplit_once(['/', '\\'])
            .map(|(parent, _basename)| parent)
            .unwrap_or("");
        let parent_tokens = word_tokens(parent);
        if tokens_contain_contiguous(&parent_tokens, &album_tokens) {
            return 2;
        }
        best = best.max(1);
    }
    best
}

/// Rank candidates by score: speed × slot_bonus × bitrate_bonus × album_bonus
/// × reliability_factor. Higher score = better candidate.
///
/// When `reputation` holds a peer with a download history, the advertised
/// speed is blended toward the peer's measured average speed (weight grows
/// with download count) and a Laplace-smoothed reliability factor in
/// [0.7, 1.3] is applied — error-prone peers are demoted. Peers without a
/// record are scored on advertised speed alone (neutral).
pub fn rank_candidates(
    results: &[SearchResult],
    config: &FilterConfig,
    album: Option<&str>,
    reputation: &std::collections::HashMap<String, crate::db::PeerReputation>,
) -> Vec<SearchResult> {
    let mut scored: Vec<(f64, &SearchResult)> = results
        .iter()
        .map(|r| {
            let advertised_bps = r.speed as f64;
            // Soulseek usernames are case-insensitive; keys are stored lowercase.
            let rep = reputation.get(&r.username.to_lowercase());
            // Blend toward the measured speed as the peer's history grows.
            // avg_speed_kbps is KiB/s (bytes/sec ÷ 1024), hence * 1024.0 back
            // to bytes/sec to match the advertised-speed unit.
            let effective_speed = match rep {
                Some(rep) if rep.total_downloads > 0 => {
                    let measured_bps = rep.avg_speed_kbps * 1024.0;
                    let w = rep.total_downloads as f64 / (rep.total_downloads as f64 + 3.0);
                    advertised_bps * (1.0 - w) + measured_bps * w
                }
                _ => advertised_bps,
            };
            // Laplace-smoothed success rate -> bounded factor, asymptotically
            // [0.7, 1.3] (centred 1.0 at a 50/50 history).
            let reliability_factor = match rep {
                Some(rep) => {
                    let r = (rep.successful as f64 + 1.5) / (rep.total_downloads as f64 + 3.0);
                    0.7 + 0.6 * r
                }
                None => 1.0,
            };
            let speed_score = effective_speed;
            let slot_bonus = if r.slots > 0 { 1.5 } else { 1.0 };
            let album_bonus = match album {
                Some(name) => match album_match_strength(r, name) {
                    2 => 1.5,
                    1 => 1.1,
                    _ => 1.0,
                },
                None => 1.0,
            };
            let bitrate_bonus = if config.min_bit_rate > 0 {
                let max_br = r
                    .files
                    .iter()
                    .filter_map(|f| f.attribs.get(&0))
                    .max()
                    .unwrap_or(&0);
                if *max_br >= config.min_bit_rate {
                    1.0 + (*max_br as f64 - config.min_bit_rate as f64) / 1000.0
                } else {
                    0.0
                }
            } else {
                1.0
            };
            let score = speed_score * slot_bonus * bitrate_bonus * album_bonus * reliability_factor;
            (score, r)
        })
        .collect();

    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    scored.into_iter().map(|(_, r)| r.clone()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{FileInfo, SearchResult};
    use crate::config::FilterConfig;
    use crate::test_support::make_file;
    use std::collections::HashMap;

    fn make_result(username: &str, speed: u32, slots: u8, files: Vec<FileInfo>) -> SearchResult {
        SearchResult {
            username: username.into(),
            speed,
            slots,
            files,
        }
    }

    fn rep(
        username: &str,
        total: u32,
        successful: u32,
        avg_kbps: f64,
    ) -> crate::db::PeerReputation {
        crate::db::PeerReputation {
            username: username.to_string(),
            total_downloads: total,
            successful,
            avg_speed_kbps: avg_kbps,
        }
    }

    #[test]
    fn rank_prefers_measured_fast_reliable_peer() {
        // Advertised speeds: slow A (1 MB/s), fast B (10 MB/s). A has a strong
        // measured record (50 MB/s + 100% success); B is unknown.
        let mut results = vec![
            crate::client::SearchResult {
                username: "A".into(),
                speed: 1_000_000,
                slots: 1,
                files: vec![],
            },
            crate::client::SearchResult {
                username: "B".into(),
                speed: 10_000_000,
                slots: 1,
                files: vec![],
            },
        ];
        let mut map = std::collections::HashMap::new();
        map.insert("a".to_string(), rep("a", 20, 20, 50_000.0)); // 50 MB/s, 100% success

        results = rank_candidates(&results, &FilterConfig::default(), None, &map);

        assert_eq!(
            results[0].username, "A",
            "measured-reliable peer A must outrank faster-but-unknown B"
        );
    }

    #[test]
    fn rank_demotes_error_prone_peer() {
        // A and B identical advertised speed; A has 100% failure history.
        let mut results = vec![
            crate::client::SearchResult {
                username: "A".into(),
                speed: 5_000_000,
                slots: 1,
                files: vec![],
            },
            crate::client::SearchResult {
                username: "B".into(),
                speed: 5_000_000,
                slots: 1,
                files: vec![],
            },
        ];
        let mut map = std::collections::HashMap::new();
        map.insert("a".to_string(), rep("a", 10, 0, 0.0)); // 100% failure
        map.insert("b".to_string(), rep("b", 10, 10, 5_000.0));

        results = rank_candidates(&results, &FilterConfig::default(), None, &map);

        assert_eq!(
            results[0].username, "B",
            "error-prone peer A must rank below clean B"
        );
    }

    #[test]
    fn rank_unknown_peer_is_neutral() {
        let results = vec![crate::client::SearchResult {
            username: "A".into(),
            speed: 5_000_000,
            slots: 1,
            files: vec![],
        }];
        let map = std::collections::HashMap::new();
        let ranked = rank_candidates(&results, &FilterConfig::default(), None, &map);
        assert_eq!(
            ranked[0].username, "A",
            "unknown peer must still be returned"
        );
    }

    fn default_filter_config() -> FilterConfig {
        FilterConfig {
            allowed_extensions: vec!["flac".into()],
            min_bit_rate: 320,
            min_bit_depth: 0,
            exclude_words: vec![],
            include_locked: false,
            contiguous_tracks: true,
            // 0 disables the min_tracks gate in these focused tests —
            // they exercise extension/bitrate/word/slot filtering, not
            // share completeness. Dedicated tests set min_tracks explicitly.
            min_tracks: 0,
            peer_track_count: true,
        }
    }

    #[test]
    fn test_filter_by_extension() {
        let cfg = default_filter_config();
        let results = vec![
            make_result(
                "user1",
                500,
                1,
                vec![make_file("01 - track.mp3", 320, 10_000_000)],
            ),
            make_result(
                "user2",
                400,
                2,
                vec![make_file("01 - track.flac", 900, 30_000_000)],
            ),
        ];

        let filtered = filter_results(&results, &cfg, None, None);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].username, "user2");
    }

    #[test]
    fn test_filter_by_min_bitrate() {
        let cfg = FilterConfig {
            min_bit_rate: 320,
            ..default_filter_config()
        };
        let results = vec![
            make_result(
                "user1",
                500,
                1,
                vec![make_file("01 - track.flac", 128, 5_000_000)],
            ),
            make_result(
                "user2",
                400,
                2,
                vec![make_file("01 - track.flac", 900, 30_000_000)],
            ),
        ];

        let filtered = filter_results(&results, &cfg, None, None);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].username, "user2");
    }

    #[test]
    fn test_filter_passes_missing_bitrate_when_min_set() {
        // A file with NO bitrate attribute (key 0) must PASS when
        // min_bitrate is set: quality is verified post-download using the
        // actual file metadata rather than rejected at search time.
        let cfg = FilterConfig {
            min_bit_rate: 320,
            min_bit_depth: 0,
            ..default_filter_config()
        };
        let file = FileInfo {
            name: "01 - Track.flac".into(),
            size: 10_000_000,
            attribs: HashMap::new(), // no bitrate attribute
        };
        assert!(
            file_passes_filters(&file, &cfg),
            "file with missing bitrate should pass when min_bitrate is set"
        );
    }

    #[test]
    fn test_filter_passes_missing_bitdepth_when_min_set() {
        // A file with NO bitdepth attribute (key 5) must PASS when
        // min_bitdepth is set: quality is verified post-download.
        let cfg = FilterConfig {
            min_bit_rate: 0,
            min_bit_depth: 16,
            ..default_filter_config()
        };
        let file = FileInfo {
            name: "01 - Track.flac".into(),
            size: 10_000_000,
            attribs: HashMap::new(), // no bitdepth attribute
        };
        assert!(
            file_passes_filters(&file, &cfg),
            "file with missing bitdepth should pass when min_bitdepth is set"
        );
    }

    #[test]
    fn test_filter_still_rejects_low_bitrate_when_provided() {
        // When the peer DOES provide a bitrate below the min, reject.
        let cfg = FilterConfig {
            min_bit_rate: 320,
            min_bit_depth: 0,
            ..default_filter_config()
        };
        let mut attribs = HashMap::new();
        attribs.insert(0, 128u32); // bitrate = 128 kbps < 320
        let file = FileInfo {
            name: "01 - Track.flac".into(),
            size: 5_000_000,
            attribs,
        };
        assert!(
            !file_passes_filters(&file, &cfg),
            "file with bitrate below min must still be rejected"
        );
    }

    #[test]
    fn test_filter_still_rejects_low_bitdepth_when_provided() {
        // When the peer DOES provide a bitdepth below the min, reject.
        let cfg = FilterConfig {
            min_bit_rate: 0,
            min_bit_depth: 24,
            ..default_filter_config()
        };
        let mut attribs = HashMap::new();
        attribs.insert(5, 16u32); // bitdepth = 16 < 24
        let file = FileInfo {
            name: "01 - Track.flac".into(),
            size: 30_000_000,
            attribs,
        };
        assert!(
            !file_passes_filters(&file, &cfg),
            "file with bitdepth below min must still be rejected"
        );
    }

    #[test]
    fn test_filter_by_queue_length() {
        let cfg = default_filter_config();
        // max_queue_length=0 means only free slots (slots > 0)
        let results = vec![
            make_result(
                "user1",
                500,
                0,
                vec![make_file("01 - track.flac", 900, 30_000_000)],
            ),
            make_result(
                "user2",
                400,
                2,
                vec![make_file("01 - track.flac", 320, 10_000_000)],
            ),
        ];

        let filtered = filter_results(&results, &cfg, None, None);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].username, "user2");
    }

    #[test]
    fn positive_queue_cap_keeps_zero_slot_candidate_for_download_validation() {
        // With a positive queue cap, a zero-slot candidate must reach the
        // download step, where its reported queue position is validated.
        let cfg = default_filter_config();
        let results = vec![make_result(
            "queued-peer",
            500,
            0,
            vec![make_file("Album/01 - track.flac", 900, 30_000_000)],
        )];

        let filtered = filter_results_with_queue_limit(
            &results,
            &cfg,
            None,
            None,
            3,
            TrackOneAnchor::Required,
        );

        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].username, "queued-peer");

        // The rejection summary must agree: a positive cap is not a
        // free-slot rejection.
        let summary = summarize_rejections_with_queue_limit(
            &results,
            &cfg,
            None,
            None,
            3,
            TrackOneAnchor::Required,
        );
        assert_eq!(summary.no_free_slots, 0);
    }

    #[test]
    fn test_rank_candidates_by_score() {
        let cfg = default_filter_config();
        let results = vec![
            make_result(
                "slow",
                100,
                1,
                vec![make_file("track.flac", 320, 10_000_000)],
            ),
            make_result(
                "fast",
                1000,
                1,
                vec![make_file("track.flac", 900, 30_000_000)],
            ),
            make_result(
                "medium",
                500,
                1,
                vec![make_file("track.flac", 500, 20_000_000)],
            ),
        ];

        let ranked = rank_candidates(&results, &cfg, None, &HashMap::new());
        assert_eq!(ranked[0].username, "fast"); // highest speed
        assert_eq!(ranked[1].username, "medium");
        assert_eq!(ranked[2].username, "slow");
    }

    #[test]
    fn test_exclude_words_filter() {
        let cfg = FilterConfig {
            exclude_words: vec!["vinyl".into(), "demo".into()],
            ..default_filter_config()
        };
        let results = vec![
            make_result(
                "user1",
                500,
                1,
                vec![make_file("01 - track (vinyl rip).flac", 900, 30_000_000)],
            ),
            make_result(
                "user2",
                400,
                2,
                vec![make_file("01 - track.flac", 900, 30_000_000)],
            ),
        ];

        let filtered = filter_results(&results, &cfg, None, None);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].username, "user2");
    }

    #[test]
    fn test_filter_rejects_gappy_tracks_when_toggle_on() {
        let cfg = FilterConfig {
            contiguous_tracks: true,
            ..default_filter_config()
        };
        let results = vec![make_result(
            "user1",
            500,
            1,
            vec![
                make_file("01 - A.flac", 900, 30_000_000),
                make_file("03 - C.flac", 900, 30_000_000),
            ],
        )];

        let filtered = filter_results(&results, &cfg, None, None);
        assert!(filtered.is_empty());
    }

    #[test]
    fn test_filter_accepts_gappy_tracks_when_toggle_off() {
        let cfg = FilterConfig {
            contiguous_tracks: false,
            ..default_filter_config()
        };
        let results = vec![make_result(
            "user1",
            500,
            1,
            vec![
                make_file("01 - A.flac", 900, 30_000_000),
                make_file("03 - C.flac", 900, 30_000_000),
            ],
        )];

        let filtered = filter_results(&results, &cfg, None, None);
        assert_eq!(filtered.len(), 1);
    }

    #[test]
    fn test_filter_toggle_off_applies_min_tracks() {
        // Toggle-off must still reject incomplete shares below the
        // configured minimum — regression: the off branch previously
        // ignored min_tracks for non-contiguous configs.
        let cfg = FilterConfig {
            contiguous_tracks: false,
            min_tracks: 3,
            ..default_filter_config()
        };
        let results = vec![make_result(
            "user1",
            500,
            1,
            vec![make_file(
                "16 - It Was a Very Good Year.flac",
                900,
                30_000_000,
            )],
        )];

        let filtered = filter_results(&results, &cfg, None, None);
        assert!(
            filtered.is_empty(),
            "single-track result must be rejected when min_tracks is 3 (toggle off)"
        );
    }

    #[test]
    fn test_filter_toggle_off_min_tracks_zero_still_needs_a_file() {
        // Regression: with min_tracks: 0 (gate disabled), a result with
        // zero files passing the quality filters must still be rejected —
        // previously 0 >= 0 was always true.
        let cfg = FilterConfig {
            contiguous_tracks: false,
            min_tracks: 0,
            ..default_filter_config()
        };
        let results = vec![make_result(
            "user1",
            500,
            1,
            // mp3 is not in allowed_extensions (flac only) — zero passing.
            vec![make_file("01 - A.mp3", 320, 10_000_000)],
        )];

        let filtered = filter_results(&results, &cfg, None, None);
        assert!(
            filtered.is_empty(),
            "zero-file result must be rejected even with min_tracks: 0"
        );
    }

    #[test]
    fn test_filter_toggle_off_accepts_at_min_tracks() {
        // Toggle-off with exactly min_tracks passing files must be accepted.
        let cfg = FilterConfig {
            contiguous_tracks: false,
            min_tracks: 3,
            ..default_filter_config()
        };
        let results = vec![make_result(
            "user1",
            500,
            1,
            vec![
                make_file("01 - A.flac", 900, 30_000_000),
                make_file("02 - B.flac", 900, 30_000_000),
                make_file("03 - C.flac", 900, 30_000_000),
            ],
        )];

        let filtered = filter_results(&results, &cfg, None, None);
        assert_eq!(
            filtered.len(),
            1,
            "3-track result must pass with min_tracks=3 (toggle off)"
        );
    }

    #[test]
    fn test_filter_min_tracks_one_boundary() {
        // min_tracks = 1: a single passing file is accepted (EP/single
        // threshold), but zero passing files is still rejected.
        let cfg = FilterConfig {
            min_tracks: 1,
            ..default_filter_config()
        };
        let results = vec![make_result(
            "user1",
            500,
            1,
            vec![make_file("01 - Single.flac", 900, 30_000_000)],
        )];

        let filtered = filter_results(&results, &cfg, None, None);
        assert_eq!(
            filtered.len(),
            1,
            "single track must pass with min_tracks=1"
        );
    }

    #[test]
    fn test_filter_rejects_unnumbered_result_when_toggle_on() {
        let cfg = FilterConfig {
            contiguous_tracks: true,
            ..default_filter_config()
        };
        let results = vec![make_result(
            "user1",
            500,
            1,
            vec![make_file("Title.flac", 900, 30_000_000)],
        )];

        let filtered = filter_results(&results, &cfg, None, None);
        assert!(filtered.is_empty());
    }

    #[test]
    fn test_filter_contiguity_runs_over_quality_passing_files_only() {
        // The full result looks contiguous (01, 02, 03), but track 02 is an
        // mp3 that fails the quality filters — the downloadable set is
        // 01, 03, which has a gap, so the result must be rejected.
        let cfg = FilterConfig {
            contiguous_tracks: true,
            ..default_filter_config()
        };
        let results = vec![make_result(
            "user1",
            500,
            1,
            vec![
                make_file("01 - A.flac", 900, 30_000_000),
                make_file("02 - B.mp3", 320, 10_000_000),
                make_file("03 - C.flac", 900, 30_000_000),
            ],
        )];

        let filtered = filter_results(&results, &cfg, None, None);
        assert!(filtered.is_empty());
    }

    #[test]
    fn test_filter_contiguity_excludes_unsafe_basenames() {
        // Tracks 01, 02, 03 where 02 has an unsafe basename (contains
        // ".."): the contiguity set mirrors what download_album would
        // really fetch, so 02 is excluded, leaving the gap {1, 3} and the
        // result must be rejected.
        let cfg = FilterConfig {
            contiguous_tracks: true,
            ..default_filter_config()
        };
        let results = vec![make_result(
            "user1",
            500,
            1,
            vec![
                make_file("01 - A.flac", 900, 30_000_000),
                make_file("02 - B..flac", 900, 30_000_000),
                make_file("03 - C.flac", 900, 30_000_000),
            ],
        )];

        let filtered = filter_results(&results, &cfg, None, None);
        assert!(filtered.is_empty());
    }

    #[test]
    fn test_filter_rejects_result_below_min_tracks() {
        // A peer sharing a single track of a multi-track album is an
        // incomplete share — reject it even though a lone track passes
        // the contiguity check (vacuous truth on a 1-element set).
        let cfg = FilterConfig {
            min_tracks: 3,
            ..default_filter_config()
        };
        let results = vec![make_result(
            "user1",
            500,
            1,
            vec![make_file(
                "16 - It Was a Very Good Year.flac",
                900,
                30_000_000,
            )],
        )];

        let filtered = filter_results(&results, &cfg, None, None);
        assert!(
            filtered.is_empty(),
            "single-track result must be rejected when min_tracks is 3"
        );
    }

    #[test]
    fn test_filter_accepts_result_at_min_tracks() {
        let cfg = FilterConfig {
            min_tracks: 3,
            ..default_filter_config()
        };
        let results = vec![make_result(
            "user1",
            500,
            1,
            vec![
                make_file("01 - A.flac", 900, 30_000_000),
                make_file("02 - B.flac", 900, 30_000_000),
                make_file("03 - C.flac", 900, 30_000_000),
            ],
        )];

        let filtered = filter_results(&results, &cfg, None, None);
        assert_eq!(
            filtered.len(),
            1,
            "3-track result must pass with min_tracks=3"
        );
    }

    #[test]
    fn test_peer_track_count_rejects_lesser() {
        // Peer has 3 filtered files, library has 5 → rejected.
        let cfg = FilterConfig {
            allowed_extensions: vec!["flac".into()],
            min_bit_rate: 0,
            min_bit_depth: 0,
            exclude_words: vec![],
            include_locked: false,
            contiguous_tracks: false,
            min_tracks: 1,
            peer_track_count: true,
        };
        let results = vec![SearchResult {
            username: "user1".into(),
            speed: 500,
            slots: 1,
            files: vec![
                make_file("01 - track.flac", 900, 10_000_000),
                make_file("02 - track.flac", 900, 10_000_000),
                make_file("03 - track.flac", 900, 10_000_000),
            ],
        }];
        let filtered = filter_results(&results, &cfg, Some(5), None);
        assert!(filtered.is_empty(), "3 tracks < library 5 → rejected");
    }

    #[test]
    fn test_peer_track_count_accepts_equal() {
        // Peer has 5 filtered files, library has 5 → passes.
        let cfg = FilterConfig {
            allowed_extensions: vec!["flac".into()],
            min_bit_rate: 0,
            min_bit_depth: 0,
            exclude_words: vec![],
            include_locked: false,
            contiguous_tracks: false,
            min_tracks: 1,
            peer_track_count: true,
        };
        let results = vec![SearchResult {
            username: "user1".into(),
            speed: 500,
            slots: 1,
            files: vec![
                make_file("01 - track.flac", 900, 10_000_000),
                make_file("02 - track.flac", 900, 10_000_000),
                make_file("03 - track.flac", 900, 10_000_000),
                make_file("04 - track.flac", 900, 10_000_000),
                make_file("05 - track.flac", 900, 10_000_000),
            ],
        }];
        let filtered = filter_results(&results, &cfg, Some(5), None);
        assert_eq!(filtered.len(), 1, "5 tracks == library 5 → accepted");
    }

    #[test]
    fn test_peer_track_count_accepts_greater() {
        // Peer has 7 filtered files, library has 5 → passes.
        let cfg = FilterConfig {
            allowed_extensions: vec!["flac".into()],
            min_bit_rate: 0,
            min_bit_depth: 0,
            exclude_words: vec![],
            include_locked: false,
            contiguous_tracks: false,
            min_tracks: 1,
            peer_track_count: true,
        };
        let results = vec![SearchResult {
            username: "user1".into(),
            speed: 500,
            slots: 1,
            files: vec![
                make_file("01 - track.flac", 900, 10_000_000),
                make_file("02 - track.flac", 900, 10_000_000),
                make_file("03 - track.flac", 900, 10_000_000),
                make_file("04 - track.flac", 900, 10_000_000),
                make_file("05 - track.flac", 900, 10_000_000),
                make_file("06 - track.flac", 900, 10_000_000),
                make_file("07 - track.flac", 900, 10_000_000),
            ],
        }];
        let filtered = filter_results(&results, &cfg, Some(5), None);
        assert_eq!(filtered.len(), 1, "7 tracks > library 5 → accepted");
    }

    #[test]
    fn test_peer_track_count_disabled() {
        // peer_track_count: false → peer with 3 files passes even though
        // library has 5.
        let cfg = FilterConfig {
            allowed_extensions: vec!["flac".into()],
            min_bit_rate: 0,
            min_bit_depth: 0,
            exclude_words: vec![],
            include_locked: false,
            contiguous_tracks: false,
            min_tracks: 1,
            peer_track_count: false,
        };
        let results = vec![SearchResult {
            username: "user1".into(),
            speed: 500,
            slots: 1,
            files: vec![
                make_file("01 - track.flac", 900, 10_000_000),
                make_file("02 - track.flac", 900, 10_000_000),
                make_file("03 - track.flac", 900, 10_000_000),
            ],
        }];
        let filtered = filter_results(&results, &cfg, Some(5), None);
        assert_eq!(filtered.len(), 1, "check disabled → accepted regardless");
    }

    #[test]
    fn test_peer_track_count_none_skips() {
        // library_track_count: None (batch/manual mode) → check skipped.
        let cfg = FilterConfig {
            allowed_extensions: vec!["flac".into()],
            min_bit_rate: 0,
            min_bit_depth: 0,
            exclude_words: vec![],
            include_locked: false,
            contiguous_tracks: false,
            min_tracks: 1,
            peer_track_count: true,
        };
        let results = vec![SearchResult {
            username: "user1".into(),
            speed: 500,
            slots: 1,
            files: vec![
                make_file("01 - track.flac", 900, 10_000_000),
                make_file("02 - track.flac", 900, 10_000_000),
                make_file("03 - track.flac", 900, 10_000_000),
            ],
        }];
        let filtered = filter_results(&results, &cfg, None, None);
        assert_eq!(
            filtered.len(),
            1,
            "library_track_count None → check skipped"
        );
    }

    #[test]
    fn test_peer_track_count_with_contiguous_tracks() {
        // ON-branch: contiguous_tracks enabled, peer has fewer tracks than
        // library → rejected. Verifies the check works in the contiguous branch
        // (all other peer_track_count tests use contiguous_tracks: false).
        let cfg = FilterConfig {
            allowed_extensions: vec!["flac".into()],
            min_bit_rate: 0,
            min_bit_depth: 0,
            exclude_words: vec![],
            include_locked: false,
            contiguous_tracks: true,
            min_tracks: 1,
            peer_track_count: true,
        };
        let results = vec![SearchResult {
            username: "user1".into(),
            speed: 500,
            slots: 1,
            files: vec![
                make_file("01 - track.flac", 900, 10_000_000),
                make_file("02 - track.flac", 900, 10_000_000),
                make_file("03 - track.flac", 900, 10_000_000),
            ],
        }];
        let filtered = filter_results(&results, &cfg, Some(5), None);
        assert!(
            filtered.is_empty(),
            "3 tracks < library 5 with contiguous_tracks ON → rejected"
        );
    }

    #[test]
    fn test_contiguity_rejects_gap_in_any_disc_of_multi_disc_album() {
        // Regression for the Michael Bolton "The Essential Michael Bolton"
        // case: a multi-disc album where CD 01 is missing tracks 11 and 13
        // but CD 02 carries them. The old contiguity check ran across ALL
        // files (both discs) and deduplicated track numbers, so the merged
        // set 01..16 had no gaps and the gapped disc was accepted. Each
        // disc must be contiguous on its own — a gap in ANY disc rejects
        // the whole result.
        let cfg = FilterConfig {
            allowed_extensions: vec!["flac".into()],
            min_bit_rate: 0,
            min_bit_depth: 0,
            exclude_words: vec![],
            include_locked: false,
            contiguous_tracks: true,
            min_tracks: 1,
            peer_track_count: false,
        };
        let files = |disc: &str, nums: &[u32]| -> Vec<FileInfo> {
            nums.iter()
                .map(|n| {
                    make_file(
                        &format!(
                            "Music\\Michael Bolton\\The Essential Michael Bolton\\{disc}\\{n:02} - track.flac"
                        ),
                        900,
                        10_000_000,
                    )
                })
                .collect()
        };
        let mut all = files("CD 01", &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 12, 14, 15, 16]);
        all.extend(files(
            "CD 02",
            &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16],
        ));

        let results = vec![SearchResult {
            username: "user1".into(),
            speed: 500,
            slots: 1,
            files: all,
        }];
        let filtered = filter_results(&results, &cfg, None, None);
        assert!(
            filtered.is_empty(),
            "CD 01 has gaps (11, 13 missing) — must be rejected even though CD 02 carries them"
        );
    }

    #[test]
    fn test_contiguity_rejects_gap_in_flat_disc_track_album() {
        // Regression: a peer shares a multi-disc album in a SINGLE flat
        // directory with DISC-TRACK filenames (e.g. "1-01 - Title.flac",
        // "2-01 - Title.flac"). CD 01 (disc "1-") is missing tracks 11
        // and 13, but CD 02 (disc "2-") is complete. The per-directory
        // grouping puts all files in one group; with CD 02 filling the
        // gaps, the combined track set appears contiguous. Each disc
        // must be validated independently — a gap in any disc rejects
        // the whole result.
        let cfg = FilterConfig {
            allowed_extensions: vec!["flac".into()],
            min_bit_rate: 0,
            min_bit_depth: 0,
            exclude_words: vec![],
            include_locked: false,
            contiguous_tracks: true,
            min_tracks: 1,
            peer_track_count: false,
        };
        // All files in ONE directory, DISC-TRACK naming:
        // "<disc>-<track> - Title.flac"
        let files = |disc: &str, nums: &[u32]| -> Vec<FileInfo> {
            nums.iter()
                .map(|n| {
                    make_file(
                        &format!(
                            "Music\\Michael Bolton - The Essential Michael Bolton\\{disc}-{n:02} - track.flac"
                        ),
                        900,
                        10_000_000,
                    )
                })
                .collect()
        };
        let mut all = files("1", &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 12, 14, 15, 16]);
        all.extend(files(
            "2",
            &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16],
        ));

        let results = vec![SearchResult {
            username: "peer1".into(),
            speed: 500,
            slots: 1,
            files: all,
        }];
        let filtered = filter_results(&results, &cfg, None, None);
        assert!(
            filtered.is_empty(),
            "CD 01 has gaps (11, 13 missing) — must be rejected even though CD 02 carries them"
        );
    }

    #[test]
    fn test_contiguity_rejects_a_run_that_starts_after_track_one() {
        // Regression for the Cyantific "Archive 1" leftovers: a gap-free run
        // that never reaches track 1 (02..10 here, 02..06 below) passed this
        // filter, was downloaded in full, and was refused only at the library
        // write — the download was wasted and the partial album stayed in
        // `storage.staging_dir`. The pre-download gate must apply the same
        // track-1 anchor the post-download gate applies.
        let cfg = FilterConfig {
            min_tracks: 3,
            ..default_filter_config()
        };
        let files = (2..=6)
            .map(|n| make_file(&format!("{n:02} - track.flac"), 900, 30_000_000))
            .collect();
        let results = vec![make_result("user1", 500, 1, files)];

        let filtered = filter_results(&results, &cfg, None, None);
        assert!(
            filtered.is_empty(),
            "a gap-free run of 02..06 has no track 1 and is a fragment, so it must not be downloaded"
        );
    }

    #[test]
    fn test_min_tracks_counts_the_largest_album_group() {
        // Regression for the Charlie Parker "The Happy \"Bird\"" leftovers:
        // the filter counted every quality-passing file in the result (5), but
        // only the largest single album directory (3) is ever downloaded, so the
        // album cleared the pre-download gate and was refused after the
        // download. The count must measure the set that will be downloaded —
        // the largest album group — exactly as `download_album` does.
        let files = vec![
            make_file(r"Music\Artist\Album\01 - A.flac", 900, 30_000_000),
            make_file(r"Music\Artist\Album\02 - B.flac", 900, 30_000_000),
            make_file(r"Music\Artist\Other\01 - C.flac", 900, 30_000_000),
            make_file(r"Music\Artist\Other\02 - D.flac", 900, 30_000_000),
            make_file(r"Music\Artist\Other\03 - E.flac", 900, 30_000_000),
        ];
        let results = vec![make_result("user1", 500, 1, files)];

        let cfg = FilterConfig {
            min_tracks: 4,
            ..default_filter_config()
        };
        assert!(
            filter_results(&results, &cfg, None, None).is_empty(),
            "the largest album group holds 3 files, below min_tracks=4, so the result must be rejected up front"
        );

        // Control: the same result is accepted once the largest group satisfies
        // the floor — the rejection above is about the group, not the total.
        let cfg = FilterConfig {
            min_tracks: 3,
            ..default_filter_config()
        };
        assert_eq!(
            filter_results(&results, &cfg, None, None).len(),
            1,
            "a 3-file largest group meets min_tracks=3"
        );
    }

    #[test]
    fn test_incomplete_download_classifies_both_halves_and_its_guards() {
        // Direct cover for the shared rule's internals: the fused disc-track
        // spelling, the two "cannot judge this set" guards, the count half, the
        // anchor switch, and the min_tracks 0 escape hatch. These were only
        // exercised indirectly through `filter_results` before.
        use IncompleteDownload::{MissingTrackOne, TooShort};
        use TrackOneAnchor::{NotRequired, Required};

        // Count half, with the found/minimum pair the log needs.
        assert_eq!(
            incomplete_download(&["01 - A.flac", "02 - B.flac"], 3, Required),
            Some(TooShort {
                found: 2,
                min_tracks: 3
            })
        );
        // Numbering half: a credible gap-free run that starts past track 1.
        assert_eq!(
            incomplete_download(&["02 - A.flac", "03 - B.flac"], 2, Required),
            Some(MissingTrackOne { start: 2 })
        );
        // Fused disc+track ("101" is disc 1 track 1) is anchored at track 1: read
        // literally it would be `MissingTrackOne` (1 and 2 are distinct, neither is
        // 1), so this assertion is what pins the `% 100 == 1` rule.
        assert_eq!(
            incomplete_download(&["101 - A.flac", "102 - B.flac"], 2, Required),
            None
        );
        // The hyphenated disc-track form is unwrapped to the track number alone, so
        // "1-01" and "1-02" parse to 1 and 2: the set has two distinct values and it
        // is the anchor (1 % 100 == 1) that accepts it.
        assert_eq!(
            incomplete_download(&["1-01 - A.flac", "1-02 - B.flac"], 2, Required),
            None
        );
        // Track 100 is not a fused track-1: 100 % 100 == 0.
        assert_eq!(
            incomplete_download(&["100 - A.flac", "02 - B.flac"], 2, Required),
            Some(MissingTrackOne { start: 2 })
        );
        // Guards: a mixed set, a lone numbered file, and one repeated value are
        // all left alone because the read is not credible. The repeated-value case is
        // the discriminating one: without the at-least-two-distinct guard the anchor
        // would refuse 7 and 7.
        assert_eq!(
            incomplete_download(&["Intro.flac", "02 - B.flac"], 2, Required),
            None
        );
        assert_eq!(incomplete_download(&["01 - A.flac"], 1, Required), None);
        assert_eq!(
            incomplete_download(&["07 - A.flac", "07 - B.flac"], 2, Required),
            None
        );
        // min_tracks 0 disables both halves.
        assert_eq!(
            incomplete_download(&["02 - A.flac", "03 - B.flac"], 0, Required),
            None
        );
        // A library-upgrade candidate skips the anchor — the library holds track 1
        // — but the count half still applies to it.
        assert_eq!(
            incomplete_download(&["02 - A.flac", "03 - B.flac"], 2, NotRequired),
            None,
            "an upgrade may be served by a peer that shares only the files it needs"
        );
        assert_eq!(
            incomplete_download(&["02 - A.flac"], 2, NotRequired),
            Some(TooShort {
                found: 1,
                min_tracks: 2
            }),
            "the count half is not an upgrade exemption"
        );
    }

    #[test]
    fn test_the_anchor_switch_changes_what_the_filter_accepts() {
        // The same result is refused as a new album and accepted as an upgrade
        // candidate: the only difference is the anchor.
        let cfg = FilterConfig {
            min_tracks: 2,
            ..default_filter_config()
        };
        let results = vec![make_result(
            "user1",
            500,
            1,
            vec![
                make_file("09 - Nine.flac", 900, 30_000_000),
                make_file("10 - Ten.flac", 900, 30_000_000),
            ],
        )];

        assert!(
            filter_results_with_queue_limit(
                &results,
                &cfg,
                None,
                None,
                0,
                TrackOneAnchor::Required
            )
            .is_empty(),
            "a run of 09..10 is not a new album"
        );
        assert_eq!(
            filter_results_with_queue_limit(
                &results,
                &cfg,
                None,
                None,
                0,
                TrackOneAnchor::NotRequired
            )
            .len(),
            1,
            "the same run is a usable upgrade source"
        );
    }

    #[test]
    fn test_toggle_off_still_refuses_a_run_without_track_one() {
        // The anchor belongs to the completeness rule, not to the gap check, so
        // turning `contiguous_tracks` off for an unnumbered collection does not
        // disable it — only `min_tracks: 0` does. Deleting the off-branch call
        // must fail this test.
        let cfg = FilterConfig {
            contiguous_tracks: false,
            min_tracks: 2,
            ..default_filter_config()
        };
        let results = vec![make_result(
            "user1",
            500,
            1,
            vec![
                make_file("09 - Nine.flac", 900, 30_000_000),
                make_file("10 - Ten.flac", 900, 30_000_000),
            ],
        )];
        assert!(
            filter_results(&results, &cfg, None, None).is_empty(),
            "a run of 09..10 is a fragment even with the gap check disabled"
        );
    }

    #[test]
    fn test_toggle_off_min_tracks_counts_the_largest_album_group() {
        // The group-aware count applies on the off branch too: the peer would
        // otherwise be downloaded for a 3-file album group while the result as a
        // whole clears the floor.
        let files = vec![
            make_file(r"Music\Artist\Album\01 - A.flac", 900, 30_000_000),
            make_file(r"Music\Artist\Album\02 - B.flac", 900, 30_000_000),
            make_file(r"Music\Artist\Other\01 - C.flac", 900, 30_000_000),
            make_file(r"Music\Artist\Other\02 - D.flac", 900, 30_000_000),
            make_file(r"Music\Artist\Other\03 - E.flac", 900, 30_000_000),
        ];
        let results = vec![make_result("user1", 500, 1, files)];

        let cfg = FilterConfig {
            contiguous_tracks: false,
            min_tracks: 4,
            ..default_filter_config()
        };
        assert!(
            filter_results(&results, &cfg, None, None).is_empty(),
            "the largest album group holds 3 files, below min_tracks=4, even with the gap check disabled"
        );

        let cfg = FilterConfig {
            contiguous_tracks: false,
            min_tracks: 3,
            ..default_filter_config()
        };
        assert_eq!(
            filter_results(&results, &cfg, None, None).len(),
            1,
            "a 3-file largest group meets min_tracks=3"
        );
    }

    #[test]
    fn test_min_tracks_preempts_peer_track_count() {
        // With default min_tracks=3, a 2-track peer is rejected by min_tracks
        // before the library track count check runs — even if the library has
        // only 1 track (so the peer would have passed the library check).
        let cfg = FilterConfig {
            allowed_extensions: vec!["flac".into()],
            min_bit_rate: 0,
            min_bit_depth: 0,
            exclude_words: vec![],
            include_locked: false,
            contiguous_tracks: false,
            min_tracks: 3,
            peer_track_count: true,
        };
        let results = vec![SearchResult {
            username: "user1".into(),
            speed: 500,
            slots: 1,
            files: vec![
                make_file("01 - track.flac", 900, 10_000_000),
                make_file("02 - track.flac", 900, 10_000_000),
            ],
        }];
        // Library has 1 track; peer has 2 (>=1). But min_tracks=3 rejects
        // because 2 < 3. The library check never runs.
        let filtered = filter_results(&results, &cfg, Some(1), None);
        assert!(
            filtered.is_empty(),
            "2 tracks < min_tracks=3 → rejected by min_tracks before library check"
        );
    }

    #[test]
    fn test_peer_track_count_counts_largest_album_directory() {
        // Regression for the Jennifer Lopez "This Is Me Then" case: a peer's
        // SearchResult can span MULTIPLE album directories (original edition
        // + anniversary edition version both matching the query). The peer
        // passed the library track count check because the TOTAL passing
        // files across all directories (6) >= library count (5) — but
        // download_album only downloads the LARGEST single directory group
        // (3 files here), which is then rejected by the post-download
        // completeness gate, throwing away the download. The filter must
        // count what will actually be downloaded: the largest directory
        // group's size — and reject the peer up-front when that is below
        // the library track count.
        let cfg = FilterConfig {
            allowed_extensions: vec!["flac".into()],
            min_bit_rate: 0,
            min_bit_depth: 0,
            exclude_words: vec![],
            include_locked: false,
            contiguous_tracks: false,
            min_tracks: 1,
            peer_track_count: true,
        };
        let results = vec![SearchResult {
            username: "user1".into(),
            speed: 500,
            slots: 1,
            // Two equal-sized album directories; neither has >= library
            // count (5), even though the sum (6) does.
            files: vec![
                make_file(r"Music\Album (Original)\01 - track.flac", 900, 10_000_000),
                make_file(r"Music\Album (Original)\02 - track.flac", 900, 10_000_000),
                make_file(r"Music\Album (Original)\03 - track.flac", 900, 10_000_000),
                make_file(
                    r"Music\Album (2022 Edition)\01 - track.flac",
                    900,
                    10_000_000,
                ),
                make_file(
                    r"Music\Album (2022 Edition)\02 - track.flac",
                    900,
                    10_000_000,
                ),
                make_file(
                    r"Music\Album (2022 Edition)\03 - track.flac",
                    900,
                    10_000_000,
                ),
            ],
        }];
        let filtered = filter_results(&results, &cfg, Some(5), None);
        assert!(
            filtered.is_empty(),
            "largest album directory has 3 tracks < library 5 → rejected up-front"
        );
    }
}

/// Summary of why results were rejected by the filter.
/// Used for concise log messages showing the primary rejection reason(s).
#[derive(Debug, Default, Clone)]
pub struct FilterRejectionSummary {
    /// Files rejected because extension not in allowed_extensions
    pub extension_rejected: usize,
    /// Most common extension among extension-rejected files
    pub most_common_rejected_ext: String,
    /// Results rejected because no free upload slots
    pub no_free_slots: usize,
    /// Results rejected because track numbers aren't contiguous
    pub non_contiguous: usize,
    /// Results rejected because below min_tracks
    pub below_min_tracks: usize,
    /// Results rejected because the set that would be downloaded is a fragment
    /// of something longer: a credible numbering run that never reaches track 1.
    /// A set that is merely too short is counted in `below_min_tracks` instead.
    pub incomplete_download: usize,
    /// Results rejected because none of their files passed the quality gate, so
    /// the track-count floor never applied (reported separately so the log names
    /// the gate that actually rejected them, e.g. `min_tracks: 0`)
    pub no_usable_files: usize,
    /// Results rejected because fewer tracks than library (peer_track_count)
    pub peer_track_count_rejected: usize,
    /// Files rejected by bitrate check
    pub bitrate_rejected: usize,
    /// Files rejected by bitdepth check
    pub bitdepth_rejected: usize,
    /// Files rejected by excluded words
    pub words_rejected: usize,
    /// Results rejected because no file path contains the album name
    pub album_mismatch: usize,
}

impl FilterRejectionSummary {
    /// Returns true if there were any rejections.
    pub fn has_rejections(&self) -> bool {
        self.extension_rejected > 0
            || self.no_free_slots > 0
            || self.non_contiguous > 0
            || self.below_min_tracks > 0
            || self.incomplete_download > 0
            || self.no_usable_files > 0
            || self.peer_track_count_rejected > 0
            || self.bitrate_rejected > 0
            || self.bitdepth_rejected > 0
            || self.words_rejected > 0
            || self.album_mismatch > 0
    }

    /// Returns a concise one-line summary for logging.
    /// Example: "rejected: 93 not in [flac] (mostly: mp3), 5 no free slots"
    pub fn summary_line(&self) -> String {
        let mut parts = Vec::new();
        if self.extension_rejected > 0 {
            let ext_info = if !self.most_common_rejected_ext.is_empty() {
                // Sanitize extension for safe logging (strip control chars)
                let safe_ext: String = self
                    .most_common_rejected_ext
                    .chars()
                    .filter(|c| !c.is_control())
                    .collect();
                format!(" (mostly: {})", safe_ext)
            } else {
                String::new()
            };
            parts.push(format!(
                "{} not in allowed formats{}",
                self.extension_rejected, ext_info
            ));
        }
        if self.no_free_slots > 0 {
            parts.push(format!(
                "{} no free slot{}",
                self.no_free_slots,
                if self.no_free_slots == 1 { "" } else { "s" }
            ));
        }
        if self.non_contiguous > 0 {
            parts.push(format!(
                "{} non-contiguous track{}",
                self.non_contiguous,
                if self.non_contiguous == 1 { "" } else { "s" }
            ));
        }
        if self.below_min_tracks > 0 {
            parts.push(format!(
                "{} below min track{}",
                self.below_min_tracks,
                if self.below_min_tracks == 1 { "" } else { "s" }
            ));
        }
        if self.incomplete_download > 0 {
            parts.push(format!("{} missing track 1", self.incomplete_download));
        }
        if self.no_usable_files > 0 {
            parts.push(format!("{} with no usable files", self.no_usable_files));
        }
        if self.peer_track_count_rejected > 0 {
            parts.push(format!(
                "{} below library track count",
                self.peer_track_count_rejected
            ));
        }
        if self.bitrate_rejected > 0 {
            parts.push(format!("{} below min bitrate", self.bitrate_rejected));
        }
        if self.bitdepth_rejected > 0 {
            parts.push(format!("{} below min bitdepth", self.bitdepth_rejected));
        }
        if self.words_rejected > 0 {
            parts.push(format!(
                "{} excluded word{}",
                self.words_rejected,
                if self.words_rejected == 1 { "" } else { "s" }
            ));
        }
        if self.album_mismatch > 0 {
            parts.push(format!(
                "{} album mismatch{}",
                self.album_mismatch,
                if self.album_mismatch == 1 { "" } else { "es" }
            ));
        }
        if parts.is_empty() {
            "no rejections".to_string()
        } else {
            format!("rejected: {}", parts.join(", "))
        }
    }
}

/// Analyze why results were rejected without re-running the full filter.
/// Returns a summary of rejection reasons across all results.
///
/// This wrapper is free-slot-only (queue cap 0). Callers that enforce
/// `download.max_queue_length` use [`summarize_rejections_with_queue_limit`]
/// so an admitted zero-slot candidate is not counted as a free-slot rejection.
pub fn summarize_rejections(
    results: &[SearchResult],
    config: &FilterConfig,
    library_track_count: Option<usize>,
    album: Option<&str>,
) -> FilterRejectionSummary {
    summarize_rejections_with_queue_limit(
        results,
        config,
        library_track_count,
        album,
        0,
        TrackOneAnchor::Required,
    )
}

/// Queue-aware variant of [`summarize_rejections`]. A zero-slot result is
/// counted as a free-slot rejection only when `max_queue_length` is 0. `anchor`
/// must match the value the results were filtered with, or the buckets describe a
/// different gate than the one that ran.
pub(crate) fn summarize_rejections_with_queue_limit(
    results: &[SearchResult],
    config: &FilterConfig,
    library_track_count: Option<usize>,
    album: Option<&str>,
    max_queue_length: u32,
    anchor: TrackOneAnchor,
) -> FilterRejectionSummary {
    let mut summary = FilterRejectionSummary::default();
    let mut ext_counts: std::collections::BTreeMap<String, usize> =
        std::collections::BTreeMap::new();

    for r in results {
        // Album-name gate check
        if let Some(album_name) = album {
            if !album_matches_result(r, album_name) {
                summary.album_mismatch += 1;
                continue;
            }
        }

        // Slot check: mirrors filter_results — a positive queue cap keeps a
        // zero-slot result eligible for download-time queue validation, so
        // it is not a free-slot rejection.
        if r.slots == 0 && max_queue_length == 0 {
            summary.no_free_slots += 1;
            continue;
        }

        // Count files passing extension check
        let mut passing_files = Vec::new();
        for f in &r.files {
            // Unsafe basename check (matches filter_results logic)
            if crate::download::safe_basename(&f.name).is_err() {
                continue;
            }
            let ext = f.name.rsplit('.').next().unwrap_or("").to_lowercase();
            if !config
                .allowed_extensions
                .iter()
                .any(|e| e.to_lowercase() == ext)
            {
                summary.extension_rejected += 1;
                *ext_counts.entry(ext.clone()).or_insert(0) += 1;
                continue;
            }
            // Bitrate check (key 0 = bitrate in kbps). Mirrors
            // file_passes_filters: only count as a bitrate rejection when
            // the peer PROVIDES a bitrate below the minimum. Missing
            // bitrate metadata passes the filter and is verified
            // post-download — it is not a rejection.
            if config.min_bit_rate > 0 {
                if let Some(&file_br) = f.attribs.get(&0) {
                    if file_br < config.min_bit_rate {
                        summary.bitrate_rejected += 1;
                        continue;
                    }
                }
            }
            // Bitdepth check (key 5 = bit depth). Same semantics as the
            // bitrate check: reject only when the peer PROVIDES a bitdepth
            // below the minimum. Missing bitdepth passes the filter and is
            // verified post-download — it is not a rejection.
            if config.min_bit_depth > 0 {
                if let Some(&file_bd) = f.attribs.get(&5) {
                    if file_bd < config.min_bit_depth {
                        summary.bitdepth_rejected += 1;
                        continue;
                    }
                }
            }
            // Excluded words
            let lower_name = f.name.to_lowercase();
            if config
                .exclude_words
                .iter()
                .any(|w| lower_name.contains(&w.to_lowercase()))
            {
                summary.words_rejected += 1;
                continue;
            }
            passing_files.push(f);
        }

        // A result with no quality-passing file at all is bucketed separately:
        // with `min_tracks > 0` the shared rule classifies it as `TooShort`
        // (`0 < min_tracks`), but naming the gate that actually emptied the result
        // is more useful here than reporting a track-count shortfall.
        if passing_files.is_empty() {
            summary.no_usable_files += 1;
            continue;
        }
        // Completeness rule, shared with `filter_results` and measured on the
        // same set it judges: the largest album group, not every passing file in
        // the result. The classification decides the bucket, so this summary
        // cannot disagree with the gate about which half refused a set — with the
        // one deliberate exception above, where a result with no quality-passing
        // file is reported as `no_usable_files` rather than as `TooShort`.
        match incomplete_download(
            &downloadable_basenames(&passing_files),
            config.min_tracks,
            anchor,
        ) {
            Some(IncompleteDownload::TooShort { .. }) => {
                summary.below_min_tracks += 1;
                continue;
            }
            Some(IncompleteDownload::MissingTrackOne { .. }) => {
                summary.incomplete_download += 1;
                continue;
            }
            None => {}
        }

        // Contiguity check (only if enabled and we have files)
        // — matches filter_results check order (contiguity before library gate)
        // Must use per-directory contiguity (same as filter_results) so
        // multi-disc albums with per-disc track numbering are validated
        // correctly.
        if config.contiguous_tracks
            && !passing_files.is_empty()
            && !files_have_contiguous_tracks_per_directory(&passing_files)
        {
            summary.non_contiguous += 1;
            continue;
        }

        // Library track count check (auto mode only)
        // Must mirror filter_results: use the largest album group's
        // length, not the flat file count, so multi-folder shares and
        // multi-disc albums are scored consistently.
        if let Some(lib_count) = library_track_count {
            if config.peer_track_count
                && crate::download::largest_album_group(&passing_files).len() < lib_count
            {
                summary.peer_track_count_rejected += 1;
                continue;
            }
        }
    }

    // Find most common rejected extension
    if let Some((ext, _)) = ext_counts.iter().max_by_key(|(_, &count)| count) {
        summary.most_common_rejected_ext = ext.clone();
    }

    summary
}

#[cfg(test)]
mod rejection_summary_tests {
    use super::*;
    use crate::client::{FileInfo, SearchResult};
    use crate::config::FilterConfig;
    use crate::test_support::make_file;
    use std::collections::HashMap;

    fn make_result(username: &str, speed: u32, slots: u8, files: Vec<FileInfo>) -> SearchResult {
        SearchResult {
            username: username.into(),
            speed,
            slots,
            files,
        }
    }

    fn default_filter_config() -> FilterConfig {
        FilterConfig {
            allowed_extensions: vec!["flac".into()],
            min_bit_rate: 320,
            min_bit_depth: 0,
            exclude_words: vec![],
            include_locked: false,
            contiguous_tracks: true,
            min_tracks: 3,
            peer_track_count: false,
        }
    }

    #[test]
    fn test_summary_extension_rejected() {
        let cfg = default_filter_config();
        let results = vec![
            make_result(
                "user1",
                500,
                1,
                vec![
                    make_file("01 - track.mp3", 320, 10_000_000),
                    make_file("02 - track.mp3", 320, 10_000_000),
                    make_file("03 - track.mp3", 320, 10_000_000),
                ],
            ),
            make_result(
                "user2",
                400,
                1,
                vec![
                    make_file("01 - track.mp3", 320, 10_000_000),
                    make_file("02 - track.mp3", 320, 10_000_000),
                    make_file("03 - track.mp3", 320, 10_000_000),
                ],
            ),
        ];
        let summary = summarize_rejections(&results, &cfg, None, None);
        assert!(summary.has_rejections());
        assert_eq!(summary.extension_rejected, 6);
        assert_eq!(summary.most_common_rejected_ext, "mp3");
        assert!(summary
            .summary_line()
            .contains("6 not in allowed formats (mostly: mp3)"));
    }

    #[test]
    fn test_summary_counts_a_set_without_track_one_as_a_rejection() {
        // The summary must agree with `filter_results`: a gap-free run that never
        // reaches track 1 is refused there, so it cannot be silently counted as a
        // passing result here. Regression for the Cyantific "Archive 1" shape,
        // whose rejection was invisible in the "0 passed filters" breakdown.
        let cfg = default_filter_config();
        let results = vec![make_result(
            "user1",
            500,
            1,
            vec![
                make_file("02 - Two.flac", 900, 10_000_000),
                make_file("03 - Three.flac", 900, 10_000_000),
                make_file("04 - Four.flac", 900, 10_000_000),
            ],
        )];
        let summary = summarize_rejections(&results, &cfg, None, None);
        assert!(
            summary.has_rejections(),
            "a run of 02..04 is refused by the filter, so the summary must report a rejection"
        );
        assert_eq!(
            summary.incomplete_download, 1,
            "the fragment must be attributed to the completeness rule"
        );
        assert!(summary.summary_line().contains("1 missing track 1"));
    }

    #[test]
    fn test_summary_no_free_slots() {
        let cfg = default_filter_config();
        let results = vec![
            make_result(
                "user1",
                500,
                0,
                vec![make_file("01 - track.flac", 900, 30_000_000)],
            ),
            make_result(
                "user2",
                400,
                0,
                vec![make_file("01 - track.flac", 900, 30_000_000)],
            ),
        ];
        let summary = summarize_rejections(&results, &cfg, None, None);
        assert!(summary.has_rejections());
        assert_eq!(summary.no_free_slots, 2);
        assert!(summary.summary_line().contains("2 no free slots"));
    }

    #[test]
    fn test_summary_non_contiguous() {
        let cfg = FilterConfig {
            contiguous_tracks: true,
            min_tracks: 1,
            ..default_filter_config()
        };
        let results = vec![make_result(
            "user1",
            500,
            1,
            vec![
                make_file("01 - A.flac", 900, 30_000_000),
                make_file("03 - C.flac", 900, 30_000_000),
            ],
        )];
        let summary = summarize_rejections(&results, &cfg, None, None);
        assert!(summary.has_rejections());
        assert_eq!(summary.non_contiguous, 1);
        assert!(summary.summary_line().contains("1 non-contiguous track"));
    }

    #[test]
    fn test_summary_mixed_rejections() {
        let cfg = default_filter_config();
        let results = vec![
            make_result(
                "mp3-user",
                500,
                1,
                vec![
                    make_file("01 - track.mp3", 320, 10_000_000),
                    make_file("02 - track.mp3", 320, 10_000_000),
                    make_file("03 - track.mp3", 320, 10_000_000),
                ],
            ),
            make_result(
                "no-slots",
                400,
                0,
                vec![make_file("01 - track.flac", 900, 30_000_000)],
            ),
        ];
        let summary = summarize_rejections(&results, &cfg, None, None);
        assert!(summary.has_rejections());
        assert_eq!(summary.extension_rejected, 3);
        assert_eq!(summary.no_free_slots, 1);
        let line = summary.summary_line();
        assert!(line.contains("3 not in allowed formats"));
        assert!(line.contains("1 no free slot"));
    }

    #[test]
    fn test_summary_no_rejections() {
        let cfg = default_filter_config();
        let results = vec![make_result(
            "user1",
            500,
            1,
            vec![
                make_file("01 - A.flac", 900, 30_000_000),
                make_file("02 - B.flac", 900, 30_000_000),
                make_file("03 - C.flac", 900, 30_000_000),
            ],
        )];
        let summary = summarize_rejections(&results, &cfg, None, None);
        assert!(!summary.has_rejections());
        assert_eq!(summary.summary_line(), "no rejections");
    }

    #[test]
    fn test_summary_does_not_count_missing_bitrate_as_rejection() {
        // A file with NO bitrate attribute must not be counted as a bitrate
        // rejection: missing metadata passes pre-download filtering and is
        // verified post-download, mirroring file_passes_filters semantics.
        let cfg = FilterConfig {
            min_bit_rate: 320,
            min_bit_depth: 0,
            ..default_filter_config()
        };
        let results = vec![make_result(
            "user1",
            500,
            1,
            vec![FileInfo {
                name: "01 - Track.flac".into(),
                size: 10_000_000,
                attribs: HashMap::new(), // missing bitrate attribute
            }],
        )];
        let summary = summarize_rejections(&results, &cfg, None, None);
        assert_eq!(
            summary.bitrate_rejected, 0,
            "missing bitrate must not count as a bitrate rejection"
        );
    }

    #[test]
    fn test_summary_counts_provided_low_bitdepth_as_rejection() {
        // When min_bitdepth is set and the peer PROVIDES a bitdepth below
        // the minimum, it must be counted as a bitdepth rejection.
        let cfg = FilterConfig {
            min_bit_rate: 0,
            min_bit_depth: 24,
            ..default_filter_config()
        };
        let mut attribs = HashMap::new();
        attribs.insert(5, 16u32); // bitdepth = 16 < 24
        let results = vec![make_result(
            "user1",
            500,
            1,
            vec![FileInfo {
                name: "01 - Track.flac".into(),
                size: 30_000_000,
                attribs,
            }],
        )];
        let summary = summarize_rejections(&results, &cfg, None, None);
        assert_eq!(
            summary.bitdepth_rejected, 1,
            "peer-provided bitdepth below min must count as a bitdepth rejection"
        );
    }

    #[test]
    fn test_summary_empty_results() {
        let cfg = default_filter_config();
        let results: Vec<SearchResult> = vec![];
        let summary = summarize_rejections(&results, &cfg, None, None);
        assert!(!summary.has_rejections());
    }

    #[test]
    fn test_summary_peer_track_count_rejected() {
        // Library has 5 tracks, peer has 3 → peer_track_count gate rejects.
        let cfg = FilterConfig {
            allowed_extensions: vec!["flac".into()],
            min_bit_rate: 0,
            min_bit_depth: 0,
            exclude_words: vec![],
            include_locked: false,
            contiguous_tracks: false,
            min_tracks: 1,
            peer_track_count: true,
        };
        let results = vec![make_result(
            "user1",
            500,
            1,
            vec![
                make_file("01 - track.flac", 900, 10_000_000),
                make_file("02 - track.flac", 900, 10_000_000),
                make_file("03 - track.flac", 900, 10_000_000),
            ],
        )];
        let summary = summarize_rejections(&results, &cfg, Some(5), None);
        assert!(summary.has_rejections());
        assert_eq!(summary.peer_track_count_rejected, 1);
        assert!(summary.summary_line().contains("below library track count"));
    }

    #[test]
    fn test_summary_peer_track_count_disabled() {
        // Library has 5 tracks, peer has 3, but peer_track_count=false → passes.
        let cfg = FilterConfig {
            allowed_extensions: vec!["flac".into()],
            min_bit_rate: 0,
            min_bit_depth: 0,
            exclude_words: vec![],
            include_locked: false,
            contiguous_tracks: false,
            min_tracks: 1,
            peer_track_count: false,
        };
        let results = vec![make_result(
            "user1",
            500,
            1,
            vec![
                make_file("01 - track.flac", 900, 10_000_000),
                make_file("02 - track.flac", 900, 10_000_000),
                make_file("03 - track.flac", 900, 10_000_000),
            ],
        )];
        let summary = summarize_rejections(&results, &cfg, Some(5), None);
        assert!(!summary.has_rejections());
    }

    #[test]
    fn test_filter_rejects_wrong_album_with_word_boundary_match() {
        // Regression: searching album "S Club" matched The Beatles' "Sgt.
        // Peppers Lonely Hearts Club Band" because "club" appears in the
        // path. A whole-word match for "s club" must reject it: the words
        // "s" and "club" do not appear adjacent as album tokens.
        let cfg = default_filter_config();
        let results = vec![make_result(
            "chris",
            11_015_233,
            3,
            vec![
                make_file(
                    "@@mwefg\\FLAC\\The Beatles\\The Beatles - 1967 - Sgt. Peppers Lonely Hearts Club Band \\The Beatles - (1967) Sgt. Peppers Lonely Hearts Club Band - 1 - Sgt. Peppers Lonely Hearts Club Band .flac",
                    900,
                    14_000_000,
                ),
                make_file(
                    "@@mwefg\\FLAC\\The Beatles\\The Beatles - 1967 - Sgt. Peppers Lonely Hearts Club Band \\The Beatles - (1967) Sgt. Peppers Lonely Hearts Club Band - 2 - With A Little Help From My Friends.flac",
                    900,
                    19_000_000,
                ),
                make_file(
                    "@@mwefg\\FLAC\\The Beatles\\The Beatles - 1967 - Sgt. Peppers Lonely Hearts Club Band \\The Beatles - (1967) Sgt. Peppers Lonely Hearts Club Band - 3 - Lucy In The Sky With Diamonds.flac",
                    900,
                    25_000_000,
                ),
            ],
        )];

        let filtered = filter_results(&results, &cfg, None, Some("S Club"));
        assert!(
            filtered.is_empty(),
            "wrong album (Beatles Sgt. Pepper) must be rejected"
        );
    }

    #[test]
    fn test_filter_accepts_correct_album() {
        // The genuine S Club 7 "S Club" album path contains "s club" as
        // whole words → accepted.
        let cfg = default_filter_config();
        let results = vec![make_result(
            "user1",
            500,
            2,
            vec![
                make_file(
                    "Music\\S Club 7\\S Club\\01 - Bring It All Back.flac",
                    900,
                    25_000_000,
                ),
                make_file(
                    "Music\\S Club 7\\S Club\\02 - S Club Party.flac",
                    900,
                    25_000_000,
                ),
                make_file(
                    "Music\\S Club 7\\S Club\\03 - Two in a Million.flac",
                    900,
                    25_000_000,
                ),
            ],
        )];

        let filtered = filter_results(&results, &cfg, None, Some("S Club"));
        assert_eq!(filtered.len(), 1, "correct album must pass");
        assert_eq!(filtered[0].username, "user1");
    }

    #[test]
    fn test_filter_no_album_param_gates_nothing() {
        // Album=None (track-name fallback tier) must not gate results — a
        // Beatles result passes through unchanged when the caller cannot
        // offer an album name to match.
        let cfg = default_filter_config();
        let results = vec![make_result(
            "chris",
            11_015_233,
            3,
            vec![
                make_file(
                    "@@mwefg\\FLAC\\The Beatles\\The Beatles - 1967 - Sgt. Peppers Lonely Hearts Club Band \\01 - Sgt. Peppers Lonely Hearts Club Band.flac",
                    900,
                    14_000_000,
                ),
                make_file(
                    "@@mwefg\\FLAC\\The Beatles\\The Beatles - 1967 - Sgt. Peppers Lonely Hearts Club Band \\02 - With A Little Help From My Friends.flac",
                    900,
                    19_000_000,
                ),
                make_file(
                    "@@mwefg\\FLAC\\The Beatles\\The Beatles - 1967 - Sgt. Peppers Lonely Hearts Club Band \\03 - Lucy In The Sky With Diamonds.flac",
                    900,
                    25_000_000,
                ),
            ],
        )];

        let filtered = filter_results(&results, &cfg, None, None);
        assert_eq!(filtered.len(), 1, "no album name → no album gating");
    }

    #[test]
    fn test_rank_prefers_folder_level_album_match() {
        // Two candidates at the same speed; the one whose album folder
        // (parent directory) matches "S Club" must outrank the one where
        // "s club" appears only in the file basename, not in any folder.
        let cfg = default_filter_config();
        let folder_match = make_result(
            "foldermatch",
            1000,
            1,
            vec![make_file(
                "Music\\S Club 7\\S Club\\01 - Bring It All Back.flac",
                900,
                25_000_000,
            )],
        );
        let deep_match = make_result(
            "deepmatch",
            1000,
            1,
            vec![make_file(
                "Music\\Various\\Compilations\\01 - S Club - Bring It All Back.flac",
                900,
                25_000_000,
            )],
        );

        let ranked = rank_candidates(
            &[deep_match, folder_match],
            &cfg,
            Some("S Club"),
            &HashMap::new(),
        );
        assert_eq!(
            ranked[0].username, "foldermatch",
            "folder-level album match must outrank deep path match"
        );
    }

    #[test]
    fn test_rank_no_album_no_bonus() {
        // With album=None the ranking is unchanged — equal speed keeps
        // input order.
        let cfg = default_filter_config();
        let a = make_result(
            "a",
            1000,
            1,
            vec![make_file(
                "Music\\S Club 7\\S Club\\01 - Bring It All Back.flac",
                900,
                25_000_000,
            )],
        );
        let b = make_result(
            "b",
            1000,
            1,
            vec![make_file(
                "Music\\S Club 7\\Greatest Hits Collection\\01 - Bring It All Back.flac",
                900,
                25_000_000,
            )],
        );

        let ranked = rank_candidates(&[a, b], &cfg, None, &HashMap::new());
        assert_eq!(ranked[0].username, "a", "no album → input order kept");
    }

    #[test]
    fn test_filter_punctuation_only_album_passes_through() {
        // Regression: punctuation-only album "( )" must not reject all
        // results. word_tokens returns [] → empty-token guard skips gate.
        let cfg = FilterConfig {
            min_tracks: 1,
            ..default_filter_config()
        };
        let results = vec![make_result(
            "user1",
            500,
            1,
            vec![make_file("01 - track.flac", 900, 10_000_000)],
        )];
        let filtered = filter_results(&results, &cfg, None, Some("( )"));
        assert_eq!(filtered.len(), 1, "punctuation-only album must not gate");
    }

    #[test]
    fn test_filter_accented_album_matches_ascii_path() {
        // Regression: "Café" must match a peer path containing "Cafe".
        // NFKD decomposition folds é → e + combining accent, ASCII filter
        // strips the accent.
        let cfg = FilterConfig {
            min_tracks: 1,
            ..default_filter_config()
        };
        let results = vec![make_result(
            "user1",
            500,
            1,
            vec![make_file(
                r"Music\Artist\Cafe\01 - Track.flac",
                900,
                10_000_000,
            )],
        )];
        let filtered = filter_results(&results, &cfg, None, Some("Café"));
        assert_eq!(filtered.len(), 1, "accented album must match ASCII path");
    }

    #[test]
    fn test_summarize_rejections_names_the_gate_that_rejected_a_result() {
        // With `min_tracks: 0` a result whose files are all rejected has no usable
        // files; the floor never applied, so the summary must say so instead of
        // reporting "below min track".
        let cfg = FilterConfig {
            min_tracks: 0,
            allowed_extensions: vec!["flac".into()],
            ..FilterConfig::default()
        };
        let results = vec![make_result(
            "peer",
            500,
            1,
            vec![make_file(
                r"Music\Artist\Album\01 - Track.mp3",
                320,
                10_000_000,
            )],
        )];

        let summary = summarize_rejections(&results, &cfg, None, None);

        assert_eq!(summary.no_usable_files, 1);
        assert_eq!(summary.below_min_tracks, 0);
        assert!(
            summary.summary_line().contains("no usable files"),
            "got: {}",
            summary.summary_line()
        );
    }

    #[test]
    fn test_summarize_rejections_counts_album_mismatch() {
        // Regression: summarize_rejections must report album_mismatch when
        // the album gate is the sole rejection reason.
        let cfg = default_filter_config();
        let results = vec![make_result(
            "chris",
            1000,
            3,
            vec![make_file(
                "@@mwefg\\FLAC\\The Beatles\\Sgt. Pepper\\01 - Track.flac",
                900,
                14_000_000,
            )],
        )];
        let summary = summarize_rejections(&results, &cfg, None, Some("S Club"));
        assert_eq!(summary.album_mismatch, 1);
        assert!(summary.has_rejections());
        assert!(summary.summary_line().contains("album mismatch"));
    }
}
