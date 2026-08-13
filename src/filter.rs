use crate::client::{FileInfo, SearchResult};
use crate::config::FilterConfig;

/// Filter search results by extension, bitrate, excluded words, free slots,
/// minimum track count (`min_tracks`), and — when `contiguous_tracks` is
/// enabled — contiguous track numbers over the downloadable set. Returns
/// only results with at least `min_tracks` matching files (or at least one
/// when `min_tracks` is 0).
pub fn filter_results(results: &[SearchResult], config: &FilterConfig) -> Vec<SearchResult> {
    results
        .iter()
        .filter(|r| {
            // Filter: must have free slots (if max_queue_length == 0)
            if r.slots == 0 {
                return false;
            }

            // The downloadable set: files passing extension + bitrate +
            // word filters AND the basename safety check download_album
            // applies — the contiguity check must mirror what will really
            // be downloaded, or an unsafe-named numbered track would pass
            // here and then be dropped at download time, recreating the
            // gap this feature exists to prevent.
            if !config.contiguous_tracks {
                // Toggle off: count safe, quality-passing files (mirroring
                // download_album) and reject incomplete shares below
                // min_tracks. min_tracks.max(1) keeps the "at least one
                // usable file" floor when the gate is disabled (0).
                let safe_and_passing = |f: &FileInfo| {
                    crate::download::safe_basename(&f.name).is_ok()
                        && file_passes_filters(f, config)
                };
                let passing_count = r.files.iter().filter(|f| safe_and_passing(f)).count();
                // min_tracks == 0 disables the gate but never accepts a
                // result with zero usable files.
                let min = config.min_tracks.max(1) as usize;
                return passing_count >= min;
            }
            let safe_and_passing = |f: &FileInfo| {
                crate::download::safe_basename(&f.name).is_ok() && file_passes_filters(f, config)
            };
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

        let filtered = filter_results(&results, &cfg);
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

        let filtered = filter_results(&results, &cfg);
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

        let filtered = filter_results(&results, &cfg);
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

        let filtered = filter_results(&results, &cfg);
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

        let filtered = filter_results(&results, &cfg);
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

        let filtered = filter_results(&results, &cfg);
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

        let filtered = filter_results(&results, &cfg);
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

        let filtered = filter_results(&results, &cfg);
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

        let filtered = filter_results(&results, &cfg);
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

        let filtered = filter_results(&results, &cfg);
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

        let filtered = filter_results(&results, &cfg);
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

        let filtered = filter_results(&results, &cfg);
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

        let filtered = filter_results(&results, &cfg);
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

        let filtered = filter_results(&results, &cfg);
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

        let filtered = filter_results(&results, &cfg);
        assert_eq!(
            filtered.len(),
            1,
            "3-track result must pass with min_tracks=3"
        );
    }
}
