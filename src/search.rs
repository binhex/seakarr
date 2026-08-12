use crate::client::{SearchResult, SoulseekClient};
use crate::error::Result;

/// Search Soulseek with a raw query, returning deduplicated results.
async fn search_raw(
    client: &dyn SoulseekClient,
    query: &str,
    timeout_secs: u64,
) -> Result<Vec<SearchResult>> {
    let mut results = client.search(query, timeout_secs).await?;
    dedup_results(&mut results);
    Ok(results)
}

/// Deduplicate by filename+size within each result's files.
fn dedup_results(results: &mut [SearchResult]) {
    for result in results {
        result.files.sort_by(|a, b| a.name.cmp(&b.name));
        result
            .files
            .dedup_by(|a, b| a.name == b.name && a.size == b.size);
    }
}

/// Search Soulseek for an album, returning deduplicated results.
pub async fn search_album(
    client: &dyn SoulseekClient,
    artist: &str,
    album: Option<&str>,
    timeout_secs: u64,
) -> Result<Vec<SearchResult>> {
    let query = match album {
        Some(a) if !a.is_empty() => format!("{artist} {a}"),
        _ => artist.to_string(),
    };
    search_raw(client, &query, timeout_secs).await
}

/// Outcome of an album search, including whether the fallback ran.
#[derive(Debug, Clone)]
pub struct SearchOutcome {
    pub results: Vec<SearchResult>,
    pub used_fallback: bool,
    /// Duration of the fallback search only (`None` when no fallback ran),
    /// so history rows can record each search's own duration.
    pub fallback_duration_ms: Option<u64>,
}

/// Search for an album, falling back to an album-only query when the
/// combined "Artist Album" search returns zero results.
///
/// Soulseek sometimes bans specific artist+album criteria. The fallback
/// searches by album name alone and keeps only results where at least one
/// file's share-relative path matches the artist (see
/// [`path_matches_artist`]), so the download pipeline receives the same
/// quality-filtered candidates as a normal search.
pub async fn search_album_with_fallback(
    client: &dyn SoulseekClient,
    artist: &str,
    album: Option<&str>,
    timeout_secs: u64,
    fallback_enabled: bool,
) -> Result<SearchOutcome> {
    let primary = search_album(client, artist, album, timeout_secs).await?;
    // Trim the album for the fallback query: padded album names from tag
    // metadata (e.g. "History ") would otherwise defeat the album-only search.
    let Some(album_name) = album.map(str::trim).filter(|a| !a.is_empty()) else {
        return Ok(SearchOutcome {
            results: primary,
            used_fallback: false,
            fallback_duration_ms: None,
        });
    };
    // An empty artist cannot be matched meaningfully — never fall back for
    // it (e.g. a malformed batch line " - Album").
    if !primary.is_empty() || !fallback_enabled || artist.trim().is_empty() {
        return Ok(SearchOutcome {
            results: primary,
            used_fallback: false,
            fallback_duration_ms: None,
        });
    }

    let fallback_start = std::time::Instant::now();
    let mut fallback = search_raw(client, album_name, timeout_secs).await?;
    fallback.retain(|r| r.files.iter().any(|f| path_matches_artist(&f.name, artist)));
    Ok(SearchOutcome {
        results: fallback,
        used_fallback: true,
        fallback_duration_ms: Some(fallback_start.elapsed().as_millis() as u64),
    })
}

/// Record a search in history (used by runner for stats).
pub fn record_search(
    artist: &str,
    album: Option<&str>,
    result_count: usize,
    duration_ms: u64,
    db: &crate::db::Database,
) -> Result<()> {
    db.conn.execute(
        "INSERT INTO search_history (artist, album, result_count, duration_ms) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![artist, album, result_count as i64, duration_ms as i64],
    )?;
    Ok(())
}

/// Common articles that carry no discriminating power when matching an
/// artist against a file path ("The Beatles" must match "Beatles").
const ARTIST_STOP_WORDS: &[&str] = &["the", "a", "an"];

/// Check whether a share-relative file path matches the artist, word-level.
///
/// The path is lowercased and `\` separators normalised to `/`. The artist
/// is split into alphanumeric words; common articles are dropped and every
/// remaining word must appear as a case-insensitive substring of the path.
/// If no words remain (artist is all stop-words), the full lowercased
/// artist name is matched as a substring instead.
///
/// Known accepted risk: substring-per-word means "Prince" also matches
/// "Princess". Degenerate artists widen the window further (single-letter
/// tokens like "U2" match any path containing `u` and `2`; a lone stop-word
/// artist like "The" matches nearly every path). Empty or blank artist
/// names never match. Downstream quality filters still apply.
pub fn path_matches_artist(path: &str, artist: &str) -> bool {
    if artist.trim().is_empty() {
        return false;
    }
    let normalised = path.to_lowercase().replace('\\', "/");
    let words: Vec<String> = artist
        .split(|c: char| !c.is_alphanumeric())
        .map(|w| w.to_lowercase())
        .filter(|w| !w.is_empty())
        .collect();
    let distinctive: Vec<&String> = words
        .iter()
        .filter(|w| !ARTIST_STOP_WORDS.contains(&w.as_str()))
        .collect();
    if distinctive.is_empty() {
        return normalised.contains(&artist.to_lowercase());
    }
    distinctive.iter().all(|w| normalised.contains(w.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{FileInfo, MockClient, SearchResult};
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

    #[tokio::test]
    async fn test_search_returns_results() {
        let client = MockClient::new();
        *client.search_results.lock().unwrap() = vec![SearchResult {
            username: "user1".into(),
            speed: 500,
            slots: 2,
            files: vec![make_file("track.flac", 900, 30_000_000)],
        }];

        let results = search_album(&client, "Test Artist", Some("Test Album"), 15)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].username, "user1");
    }

    #[tokio::test]
    async fn test_search_deduplicates_by_filename() {
        let client = MockClient::new();
        *client.search_results.lock().unwrap() = vec![
            SearchResult {
                username: "user1".into(),
                speed: 500,
                slots: 1,
                files: vec![make_file("track.flac", 900, 30_000_000)],
            },
            SearchResult {
                username: "user2".into(),
                speed: 400,
                slots: 2,
                files: vec![make_file("track.flac", 900, 30_000_000)], // same filename
            },
        ];

        let results = search_album(&client, "Artist", Some("Album"), 15)
            .await
            .unwrap();
        // Both users returned (dedup is by filename+size within each result, not across users — both have the same file but from different users)
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn test_path_matches_artist_basic_backslash_path() {
        assert!(path_matches_artist(
            r"@@rldqn\complete\Michael Jackson\History\01 - Billie Jean.flac",
            "Michael Jackson"
        ));
    }

    #[test]
    fn test_path_matches_artist_case_insensitive_and_forward_slashes() {
        assert!(path_matches_artist(
            "music/michael jackson/history/01 - billie jean.flac",
            "MICHAEL JACKSON"
        ));
    }

    #[test]
    fn test_path_matches_artist_reordered_words() {
        assert!(path_matches_artist(
            "Jackson, Michael - History - 01 - Billie Jean.flac",
            "Michael Jackson"
        ));
    }

    #[test]
    fn test_path_matches_artist_dropped_article() {
        // Artist "The Beatles" must match a path shared as just "Beatles".
        assert!(path_matches_artist(
            "Beatles - Abbey Road - 01 - Come Together.flac",
            "The Beatles"
        ));
    }

    #[test]
    fn test_path_matches_artist_punctuation() {
        assert!(path_matches_artist(
            r"AC-DC\Back in Black\01 - Hells Bells.flac",
            "AC/DC"
        ));
    }

    #[test]
    fn test_path_matches_artist_all_stop_words_falls_back_to_full_name() {
        assert!(path_matches_artist(
            "The The - Infected - 01.flac",
            "The The"
        ));
        assert!(!path_matches_artist(
            "Some Other Artist - 01.flac",
            "The The"
        ));
    }

    #[test]
    fn test_path_matches_artist_no_match() {
        assert!(!path_matches_artist(
            r"Music\Other Artist\History\01 - track.flac",
            "Michael Jackson"
        ));
    }

    #[test]
    fn test_path_matches_artist_empty_artist_returns_false() {
        assert!(!path_matches_artist("anything.flac", ""));
        assert!(!path_matches_artist(r"Music\Whatever\01.flac", "   "));
    }

    #[tokio::test]
    async fn test_fallback_skipped_when_artist_empty() {
        let client = MockClient::new();
        let outcome = search_album_with_fallback(&client, "", Some("Album"), 15, true)
            .await
            .unwrap();
        assert!(!outcome.used_fallback);
        assert_eq!(client.search_queries.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn test_fallback_skipped_when_album_blank() {
        let client = MockClient::new();
        let outcome = search_album_with_fallback(&client, "Artist", Some(" "), 15, true)
            .await
            .unwrap();
        assert!(!outcome.used_fallback);
        assert_eq!(client.search_queries.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn test_fallback_query_uses_trimmed_album() {
        let client = MockClient::new();
        client.search_results_by_query.lock().unwrap().insert(
            "History".into(),
            vec![SearchResult {
                username: "peer1".into(),
                speed: 500,
                slots: 1,
                files: vec![make_file(
                    r"Music\Michael Jackson\History\01 - Billie Jean.flac",
                    900,
                    30_000_000,
                )],
            }],
        );

        let outcome =
            search_album_with_fallback(&client, "Michael Jackson", Some(" History "), 15, true)
                .await
                .unwrap();
        assert!(outcome.used_fallback);
        assert_eq!(outcome.results.len(), 1);
        let queries = client.search_queries.lock().unwrap().clone();
        assert_eq!(queries.len(), 2);
        // The fallback query must be the trimmed album name, not " History ".
        assert_eq!(queries.last().unwrap(), "History");
    }

    #[tokio::test]
    async fn test_fallback_used_when_primary_empty() {
        let client = MockClient::new();
        client.search_results_by_query.lock().unwrap().insert(
            "History".into(),
            vec![SearchResult {
                username: "peer1".into(),
                speed: 500,
                slots: 1,
                files: vec![make_file(
                    r"Music\Michael Jackson\History\01 - Billie Jean.flac",
                    900,
                    30_000_000,
                )],
            }],
        );
        // The primary query has no map entry, so it returns the empty
        // static search_results — the fallback trigger.

        let outcome =
            search_album_with_fallback(&client, "Michael Jackson", Some("History"), 15, true)
                .await
                .unwrap();
        assert!(outcome.used_fallback);
        assert!(outcome.fallback_duration_ms.is_some());
        assert_eq!(outcome.results.len(), 1);
        assert_eq!(outcome.results[0].username, "peer1");
        let queries = client.search_queries.lock().unwrap().clone();
        assert_eq!(
            queries,
            vec!["Michael Jackson History".to_string(), "History".to_string()]
        );
    }

    #[tokio::test]
    async fn test_no_fallback_when_primary_non_empty() {
        let client = MockClient::new();
        *client.search_results.lock().unwrap() = vec![SearchResult {
            username: "user1".into(),
            speed: 500,
            slots: 1,
            files: vec![make_file("track.flac", 900, 30_000_000)],
        }];

        let outcome = search_album_with_fallback(&client, "Artist", Some("Album"), 15, true)
            .await
            .unwrap();
        assert!(!outcome.used_fallback);
        assert!(outcome.fallback_duration_ms.is_none());
        assert_eq!(outcome.results.len(), 1);
        let queries = client.search_queries.lock().unwrap().clone();
        assert_eq!(queries, vec!["Artist Album".to_string()]);
    }

    #[tokio::test]
    async fn test_fallback_skipped_when_disabled() {
        let client = MockClient::new();
        let outcome = search_album_with_fallback(&client, "Artist", Some("Album"), 15, false)
            .await
            .unwrap();
        assert!(!outcome.used_fallback);
        assert_eq!(client.search_queries.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn test_fallback_skipped_when_no_album() {
        let client = MockClient::new();
        let outcome = search_album_with_fallback(&client, "Artist", None, 15, true)
            .await
            .unwrap();
        assert!(!outcome.used_fallback);
        assert_eq!(client.search_queries.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn test_fallback_filters_results_by_artist_in_path() {
        let client = MockClient::new();
        client.search_results_by_query.lock().unwrap().insert(
            "Album".into(),
            vec![
                SearchResult {
                    username: "right".into(),
                    speed: 500,
                    slots: 1,
                    files: vec![make_file(
                        r"Music\Artist\Album\01 - track.flac",
                        900,
                        30_000_000,
                    )],
                },
                SearchResult {
                    username: "wrong".into(),
                    speed: 999,
                    slots: 1,
                    files: vec![make_file(
                        r"Music\Someone Else\Album\01 - track.flac",
                        900,
                        30_000_000,
                    )],
                },
            ],
        );

        let outcome = search_album_with_fallback(&client, "Artist", Some("Album"), 15, true)
            .await
            .unwrap();
        assert!(outcome.used_fallback);
        assert_eq!(outcome.results.len(), 1);
        assert_eq!(outcome.results[0].username, "right");
    }
}
