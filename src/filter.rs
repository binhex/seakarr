use crate::client::{FileInfo, SearchResult};
use crate::config::FilterConfig;

/// Filter search results by extension, bitrate, excluded words, and slots.
/// Returns only results with at least one matching file.
pub fn filter_results(results: &[SearchResult], config: &FilterConfig) -> Vec<SearchResult> {
    results
        .iter()
        .filter(|r| {
            // Filter: must have free slots (if max_queue_length == 0)
            if r.slots == 0 {
                return false;
            }

            // Filter: at least one file must pass extension + bitrate + word filters
            r.files.iter().any(|f| file_passes_filters(f, config))
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
                vec![make_file("track.mp3", 320, 10_000_000)],
            ),
            make_result(
                "user2",
                400,
                2,
                vec![make_file("track.flac", 900, 30_000_000)],
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
                vec![make_file("track.flac", 128, 5_000_000)],
            ),
            make_result(
                "user2",
                400,
                2,
                vec![make_file("track.flac", 900, 30_000_000)],
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
                vec![make_file("track.flac", 900, 30_000_000)],
            ),
            make_result(
                "user2",
                400,
                2,
                vec![make_file("track.flac", 320, 10_000_000)],
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
                vec![make_file("track (vinyl rip).flac", 900, 30_000_000)],
            ),
            make_result(
                "user2",
                400,
                2,
                vec![make_file("track.flac", 900, 30_000_000)],
            ),
        ];

        let filtered = filter_results(&results, &cfg);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].username, "user2");
    }
}
