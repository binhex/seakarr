use crate::client::{FileInfo, SearchResult};
use crate::config::FilterConfig;

/// Filter search results by extension, bitrate, excluded words, free slots,
/// minimum track count (`min_tracks`), contiguous track numbers (when
/// `contiguous_tracks` is enabled), and — in auto mode — the library track
/// count (`peer_track_count`): results whose usable track count is below the
/// library's existing track count are rejected. Returns only results with at
/// least `min_tracks` matching files (or at least one when `min_tracks` is 0).
pub fn filter_results(
    results: &[SearchResult],
    config: &FilterConfig,
    library_track_count: Option<usize>,
) -> Vec<SearchResult> {
    results
        .iter()
        .filter(|r| {
            // Filter: must have free upload slots. Results with no free
            // slots (slots == 0) are always rejected. The include_locked
            // field is defined but not yet enforced.
            if r.slots == 0 {
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
                // download_album) and reject incomplete shares below
                // min_tracks. min_tracks.max(1) keeps the "at least one
                // usable file" floor when the gate is disabled (0).
                let passing_count = r.files.iter().filter(|f| safe_and_passing(f)).count();
                // min_tracks == 0 disables the gate but never accepts a
                // result with zero usable files.
                let min = config.min_tracks.max(1) as usize;
                if passing_count < min {
                    return false;
                }
                // Library track count check (auto mode only).
                // Note: passing_count counts files passing quality filters,
                // not unique track numbers — duplicate filenames are counted
                // separately. This is intentional: the check compares the
                // library's total file count against the peer's usable file
                // count, so track-number deduplication would undercount.
                if let Some(lib_count) = library_track_count {
                    if config.peer_track_count && passing_count < lib_count {
                        tracing::debug!(
                            "result from {} rejected: {} filtered tracks < library track count {}",
                            r.username,
                            passing_count,
                            lib_count
                        );
                        return false;
                    }
                }
                return true;
            }
            let passing: Vec<&FileInfo> = r.files.iter().filter(|f| safe_and_passing(f)).collect();
            if passing.len() < config.min_tracks as usize {
                // Incomplete share: fewer tracks than the configured minimum
                // (e.g. a single track of a 16-track album).
                tracing::debug!(
                    "result from {} rejected: {} passing files below min_tracks={}",
                    r.username,
                    passing.len(),
                    config.min_tracks
                );
                return false;
            }
            if passing.is_empty() {
                return false;
            }
            if !crate::tracks::files_have_contiguous_tracks(&passing) {
                // Distinguish the two rejection causes for operators.
                let any_numbered = passing
                    .iter()
                    .any(|f| crate::tracks::track_number_from_filename(&f.name).is_some());
                tracing::debug!(
                    "result from {} rejected: {}",
                    r.username,
                    if any_numbered {
                        "non-contiguous track numbers"
                    } else {
                        "no parseable track numbers"
                    }
                );
                return false;
            }
            // Library track count check (auto mode only).
            // Note: with the default min_tracks=3, albums with 1-2 tracks
            // (EPs, singles) are already rejected by the min_tracks gate
            // before this check runs. To apply the library check to EPs,
            // set min_tracks to 0 or 1.
            if let Some(lib_count) = library_track_count {
                if config.peer_track_count && passing.len() < lib_count {
                    tracing::debug!(
                        "result from {} rejected: {} filtered tracks < library track count {}",
                        r.username,
                        passing.len(),
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
    // Bitrate check (key 0 = bitrate in kbps). An absent bitrate attribute
    // means we cannot verify the file's quality — reject it when a minimum
    // is configured, to avoid downloading files of unknown provenance.
    if let Some(min_br) = config.min_bitrate {
        match file.attribs.get(&0) {
            None => return false,
            Some(&file_br) if file_br < min_br => return false,
            _ => {}
        }
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

/// Rank candidates by score: speed × slot_bonus × bitrate_bonus.
/// Higher score = better candidate.
pub fn rank_candidates(results: &[SearchResult], config: &FilterConfig) -> Vec<SearchResult> {
    let mut scored: Vec<(f64, &SearchResult)> = results
        .iter()
        .map(|r| {
            let speed_score = r.speed as f64;
            let slot_bonus = if r.slots > 0 { 1.5 } else { 1.0 };
            let bitrate_bonus = if let Some(min_br) = config.min_bitrate {
                let max_br = r
                    .files
                    .iter()
                    .filter_map(|f| f.attribs.get(&0))
                    .max()
                    .unwrap_or(&0);
                if *max_br >= min_br {
                    1.0 + (*max_br as f64 - min_br as f64) / 1000.0
                } else {
                    0.0
                }
            } else {
                1.0
            };
            let score = speed_score * slot_bonus * bitrate_bonus;
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
    use std::collections::HashMap;

    fn make_file(name: &str, bitrate: u32, size: u64) -> FileInfo {
        let mut attribs = HashMap::new();
        attribs.insert(0, bitrate);
        FileInfo {
            name: name.into(),
            size,
            attribs,
        }
    }

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
            min_bitrate: Some(320),
            min_bitdepth: None,
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

        let filtered = filter_results(&results, &cfg, None);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].username, "user2");
    }

    #[test]
    fn test_filter_by_min_bitrate() {
        let cfg = FilterConfig {
            min_bitrate: Some(320),
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

        let filtered = filter_results(&results, &cfg, None);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].username, "user2");
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

        let filtered = filter_results(&results, &cfg, None);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].username, "user2");
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

        let ranked = rank_candidates(&results, &cfg);
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

        let filtered = filter_results(&results, &cfg, None);
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

        let filtered = filter_results(&results, &cfg, None);
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

        let filtered = filter_results(&results, &cfg, None);
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

        let filtered = filter_results(&results, &cfg, None);
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

        let filtered = filter_results(&results, &cfg, None);
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

        let filtered = filter_results(&results, &cfg, None);
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

        let filtered = filter_results(&results, &cfg, None);
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

        let filtered = filter_results(&results, &cfg, None);
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

        let filtered = filter_results(&results, &cfg, None);
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

        let filtered = filter_results(&results, &cfg, None);
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

        let filtered = filter_results(&results, &cfg, None);
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

        let filtered = filter_results(&results, &cfg, None);
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
            min_bitrate: None,
            min_bitdepth: None,
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
        let filtered = filter_results(&results, &cfg, Some(5));
        assert!(filtered.is_empty(), "3 tracks < library 5 → rejected");
    }

    #[test]
    fn test_peer_track_count_accepts_equal() {
        // Peer has 5 filtered files, library has 5 → passes.
        let cfg = FilterConfig {
            allowed_extensions: vec!["flac".into()],
            min_bitrate: None,
            min_bitdepth: None,
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
        let filtered = filter_results(&results, &cfg, Some(5));
        assert_eq!(filtered.len(), 1, "5 tracks == library 5 → accepted");
    }

    #[test]
    fn test_peer_track_count_accepts_greater() {
        // Peer has 7 filtered files, library has 5 → passes.
        let cfg = FilterConfig {
            allowed_extensions: vec!["flac".into()],
            min_bitrate: None,
            min_bitdepth: None,
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
        let filtered = filter_results(&results, &cfg, Some(5));
        assert_eq!(filtered.len(), 1, "7 tracks > library 5 → accepted");
    }

    #[test]
    fn test_peer_track_count_disabled() {
        // peer_track_count: false → peer with 3 files passes even though
        // library has 5.
        let cfg = FilterConfig {
            allowed_extensions: vec!["flac".into()],
            min_bitrate: None,
            min_bitdepth: None,
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
        let filtered = filter_results(&results, &cfg, Some(5));
        assert_eq!(filtered.len(), 1, "check disabled → accepted regardless");
    }

    #[test]
    fn test_peer_track_count_none_skips() {
        // library_track_count: None (batch/manual mode) → check skipped.
        let cfg = FilterConfig {
            allowed_extensions: vec!["flac".into()],
            min_bitrate: None,
            min_bitdepth: None,
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
        let filtered = filter_results(&results, &cfg, None);
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
            min_bitrate: None,
            min_bitdepth: None,
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
        let filtered = filter_results(&results, &cfg, Some(5));
        assert!(
            filtered.is_empty(),
            "3 tracks < library 5 with contiguous_tracks ON → rejected"
        );
    }

    #[test]
    fn test_min_tracks_preempts_peer_track_count() {
        // With default min_tracks=3, a 2-track peer is rejected by min_tracks
        // before the library track count check runs — even if the library has
        // only 1 track (so the peer would have passed the library check).
        let cfg = FilterConfig {
            allowed_extensions: vec!["flac".into()],
            min_bitrate: None,
            min_bitdepth: None,
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
        let filtered = filter_results(&results, &cfg, Some(1));
        assert!(
            filtered.is_empty(),
            "2 tracks < min_tracks=3 → rejected by min_tracks before library check"
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
    /// Results rejected because fewer tracks than library (peer_track_count)
    pub peer_track_count_rejected: usize,
    /// Files rejected by bitrate check
    pub bitrate_rejected: usize,
    /// Files rejected by excluded words
    pub words_rejected: usize,
}

impl FilterRejectionSummary {
    /// Returns true if there were any rejections.
    pub fn has_rejections(&self) -> bool {
        self.extension_rejected > 0
            || self.no_free_slots > 0
            || self.non_contiguous > 0
            || self.below_min_tracks > 0
            || self.peer_track_count_rejected > 0
            || self.bitrate_rejected > 0
            || self.words_rejected > 0
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
        if self.peer_track_count_rejected > 0 {
            parts.push(format!(
                "{} below library track count",
                self.peer_track_count_rejected
            ));
        }
        if self.bitrate_rejected > 0 {
            parts.push(format!("{} below min bitrate", self.bitrate_rejected));
        }
        if self.words_rejected > 0 {
            parts.push(format!(
                "{} excluded word{}",
                self.words_rejected,
                if self.words_rejected == 1 { "" } else { "s" }
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
pub fn summarize_rejections(
    results: &[SearchResult],
    config: &FilterConfig,
    library_track_count: Option<usize>,
) -> FilterRejectionSummary {
    let mut summary = FilterRejectionSummary::default();
    let mut ext_counts: std::collections::BTreeMap<String, usize> =
        std::collections::BTreeMap::new();

    for r in results {
        // Slot check
        if r.slots == 0 {
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
            // Bitrate check
            if let Some(min_br) = config.min_bitrate {
                match f.attribs.get(&0) {
                    None => {
                        summary.bitrate_rejected += 1;
                        continue;
                    }
                    Some(&file_br) if file_br < min_br => {
                        summary.bitrate_rejected += 1;
                        continue;
                    }
                    _ => {}
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

        // Min tracks check (mirror filter_results: min_tracks=0 still
        // enforces floor of1 for zero-passing-file rejection)
        let min = config.min_tracks.max(1) as usize;
        if passing_files.len() < min {
            summary.below_min_tracks += 1;
            continue;
        }

        // Contiguity check (only if enabled and we have files)
        // — matches filter_results check order (contiguity before library gate)
        if config.contiguous_tracks
            && !passing_files.is_empty()
            && !crate::tracks::files_have_contiguous_tracks(&passing_files)
        {
            summary.non_contiguous += 1;
            continue;
        }

        // Library track count check (auto mode only)
        if let Some(lib_count) = library_track_count {
            if config.peer_track_count && passing_files.len() < lib_count {
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
    use std::collections::HashMap;

    fn make_file(name: &str, bitrate: u32, size: u64) -> FileInfo {
        let mut attribs = HashMap::new();
        attribs.insert(0, bitrate);
        FileInfo {
            name: name.into(),
            size,
            attribs,
        }
    }

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
            min_bitrate: Some(320),
            min_bitdepth: None,
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
        let summary = summarize_rejections(&results, &cfg, None);
        assert!(summary.has_rejections());
        assert_eq!(summary.extension_rejected, 6);
        assert_eq!(summary.most_common_rejected_ext, "mp3");
        assert!(summary
            .summary_line()
            .contains("6 not in allowed formats (mostly: mp3)"));
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
        let summary = summarize_rejections(&results, &cfg, None);
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
        let summary = summarize_rejections(&results, &cfg, None);
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
        let summary = summarize_rejections(&results, &cfg, None);
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
        let summary = summarize_rejections(&results, &cfg, None);
        assert!(!summary.has_rejections());
        assert_eq!(summary.summary_line(), "no rejections");
    }

    #[test]
    fn test_summary_empty_results() {
        let cfg = default_filter_config();
        let results: Vec<SearchResult> = vec![];
        let summary = summarize_rejections(&results, &cfg, None);
        assert!(!summary.has_rejections());
    }

    #[test]
    fn test_summary_peer_track_count_rejected() {
        // Library has 5 tracks, peer has 3 → peer_track_count gate rejects.
        let cfg = FilterConfig {
            allowed_extensions: vec!["flac".into()],
            min_bitrate: None,
            min_bitdepth: None,
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
        let summary = summarize_rejections(&results, &cfg, Some(5));
        assert!(summary.has_rejections());
        assert_eq!(summary.peer_track_count_rejected, 1);
        assert!(summary.summary_line().contains("below library track count"));
    }

    #[test]
    fn test_summary_peer_track_count_disabled() {
        // Library has 5 tracks, peer has 3, but peer_track_count=false → passes.
        let cfg = FilterConfig {
            allowed_extensions: vec!["flac".into()],
            min_bitrate: None,
            min_bitdepth: None,
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
        let summary = summarize_rejections(&results, &cfg, Some(5));
        assert!(!summary.has_rejections());
    }
}
