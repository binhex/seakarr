use crate::client::{SearchResult, SoulseekClient};
use crate::error::Result;
use regex::Regex;
use std::sync::OnceLock;
use unicode_normalization::UnicodeNormalization;

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
/// searches by album name alone, keeps only results with at least one
/// artist-matching file, and prunes each kept result down to its
/// artist-matching files only (see [`path_matches_artist`]) so the download
/// pipeline receives only quality-filtered, artist-matching candidates.
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
    let fallback = search_fallback_only(client, artist, album_name, timeout_secs).await?;
    Ok(SearchOutcome {
        results: fallback,
        used_fallback: true,
        fallback_duration_ms: Some(fallback_start.elapsed().as_millis() as u64),
    })
}

/// Perform the album-only fallback search: query by album name alone and
/// keep only results that contain at least one artist-matching file, pruned
/// down to the artist-matching files (see [`path_matches_artist`]).
pub async fn search_fallback_only(
    client: &dyn SoulseekClient,
    artist: &str,
    album_name: &str,
    timeout_secs: u64,
) -> Result<Vec<SearchResult>> {
    let mut fallback = search_raw(client, album_name, timeout_secs).await?;
    // Keep only results that contain at least one artist-matching file, and
    // within each kept result keep ONLY the artist-matching files. Without
    // the per-file filter, download_album would take every quality-passing
    // file of the chosen result — a mixed share (one matching file + other
    // artists' tracks that match the broad album-only query) would donate
    // wrong-artist files, defeating the fallback's purpose.
    fallback.retain_mut(|r| {
        r.files.retain_mut(|f| path_matches_artist(&f.name, artist));
        !r.files.is_empty()
    });
    Ok(fallback)
}

/// Audio file extensions collected by [`get_library_track_filenames`].
const AUDIO_EXTENSIONS: &[&str] = &[
    "flac", "mp3", "m4a", "aac", "ogg", "opus", "wav", "wma", "ape",
];

/// The leading-track-number pattern (`01.`, `01 -`, `01-`, `12-`, ...).
fn track_number_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\d+[\.\-\s]+").expect("valid track-number regex"))
}

/// Bracket characters. Each is replaced with a space so bracketed sections
/// act as word separators while their contents are kept (see
/// [`clean_track_title`]).
fn bracket_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[\(\)\[\]\{\}]").expect("valid bracket regex"))
}

/// Aggressively normalize a track filename into a searchable title.
///
/// Steps: strip the file extension, drop a leading track number
/// (`\d+[.\-\s]+`), turn bracket characters into spaces (keeping their
/// contents), normalize unicode to ASCII (NFKD), lowercase, drop every
/// non-alphanumeric/non-whitespace character, and collapse whitespace.
///
/// Note: the bracket step replaces each bracket *character* with a space
/// and keeps the contents — the plan's literal greedy pattern
/// `[()\[\]{}][^)]*` would swallow the contents ("(Live) [Remix]" →
/// "hello"), contradicting its own documented examples ("hello live
/// remix"). The examples are the contract and the tests assert them.
///
/// Examples:
/// - `"03. I Miss You.mp3"` → `"i miss you"`
/// - `"01 - Hello (Live) [Remix].flac"` → `"hello live remix"`
/// - `"Café.mp3"` → `"cafe"`
/// - `"12- Bye.mp3"` → `"bye"`
pub fn clean_track_title(filename: &str) -> String {
    let stem = filename.rsplit_once('.').map_or(filename, |(stem, _)| stem);
    let no_track_number = track_number_re().replace(stem, "");
    let no_brackets = bracket_re().replace_all(&no_track_number, " ");
    let normalized: String = no_brackets
        .nfkd()
        .filter(|c| c.is_ascii())
        .collect::<String>()
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect();
    normalized.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// List the audio filenames (not full paths) inside
/// `<path>/<artist>/<album>/` for each configured library path.
///
/// Non-audio files, sub-directories, and unreadable/missing album
/// directories are skipped; a missing album directory yields an empty list,
/// never an error. The result is sorted alphabetically and deduplicated
/// (the same album may exist under several library roots).
pub fn get_library_track_filenames(
    library_paths: &[String],
    artist: &str,
    album: &str,
) -> Result<Vec<String>> {
    // Reject path traversal and separator injection from tag-derived names.
    if artist.contains("..")
        || album.contains("..")
        || artist.contains('/')
        || artist.contains('\\')
        || album.contains('/')
        || album.contains('\\')
    {
        return Ok(Vec::new());
    }
    let mut filenames = Vec::new();
    for library_path in library_paths {
        // Try the exact tag-derived path first.
        let album_dir = std::path::Path::new(library_path).join(artist).join(album);
        let found = collect_audio_filenames(&album_dir);
        if !found.is_empty() {
            filenames.extend(found);
            continue;
        }
        // Fallback: scan for case-insensitive directory matches. The
        // scanner uses tag metadata (e.g. "25") but folders on disk may
        // differ (e.g. "25 (Deluxe)"). Walk one level and match.
        let lib = std::path::Path::new(library_path);
        let Ok(lib_entries) = std::fs::read_dir(lib) else {
            continue;
        };
        for artist_entry in lib_entries.flatten() {
            if !artist_entry.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let artist_name = artist_entry.file_name().to_string_lossy().into_owned();
            if !artist_name.eq_ignore_ascii_case(artist) {
                continue;
            }
            let artist_dir = artist_entry.path();
            let Ok(album_entries) = std::fs::read_dir(&artist_dir) else {
                continue;
            };
            for album_entry in album_entries.flatten() {
                if !album_entry.file_type().is_ok_and(|t| t.is_dir()) {
                    continue;
                }
                let album_name = album_entry.file_name().to_string_lossy().into_owned();
                if album_name.eq_ignore_ascii_case(album) {
                    let found = collect_audio_filenames(&album_entry.path());
                    if !found.is_empty() {
                        filenames.extend(found);
                    }
                }
            }
        }
    }
    filenames.sort();
    filenames.dedup();
    Ok(filenames)
}

/// Collect audio filenames from a directory, returning an empty Vec if
/// the directory doesn't exist or contains no audio files.
fn collect_audio_filenames(dir: &std::path::Path) -> Vec<String> {
    let mut filenames = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return filenames;
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_file() {
            continue;
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_audio = std::path::Path::new(&name)
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| AUDIO_EXTENSIONS.contains(&ext.to_lowercase().as_str()))
            .unwrap_or(false);
        if is_audio {
            filenames.push(name);
        }
    }
    filenames
}

/// Search Soulseek by the cleaned library track titles, keeping only
/// results that contain at least `match_threshold_pct`% of the album's
/// tracks.
///
/// Every library filename is normalized with [`clean_track_title`]; the
/// alphabetically-first title (the library list is sorted by
/// [`get_library_track_filenames`]) becomes the search query. Each result's
/// files are pruned to those whose cleaned basename (last path component)
/// matches one of the library titles exactly, and a result survives only
/// when the number of matching files is at least
/// `ceil(len(titles) * threshold / 100)`. An empty library short-circuits
/// to an empty result set without touching the network.
pub async fn search_by_title(
    client: &dyn SoulseekClient,
    library_filenames: &[String],
    timeout_secs: u64,
    match_threshold_pct: u32,
) -> Result<Vec<SearchResult>> {
    // Clean titles — preserves order from sorted filenames for the query.
    let clean_titles: Vec<String> = library_filenames
        .iter()
        .map(|filename| clean_track_title(filename))
        .filter(|t| !t.is_empty())
        .collect();
    if clean_titles.is_empty() {
        return Ok(Vec::new());
    }
    // Dedup for counting threshold — a mixed-format library (flac + mp3)
    // would otherwise inflate the denominator and double-count matches.
    let library_titles: std::collections::HashSet<String> = clean_titles.iter().cloned().collect();
    // Search by the alphabetically-first cleaned title.
    let query = &clean_titles[0];
    if query.is_empty() {
        return Ok(Vec::new());
    }
    let required = library_titles
        .len()
        .saturating_mul(match_threshold_pct as usize)
        .div_ceil(100);
    let mut results = search_raw(client, query, timeout_secs).await?;
    // Keep only files whose cleaned basename matches a distinct library
    // title, and keep the result only if enough distinct titles matched.
    results.retain_mut(|result| {
        let mut matched_titles = std::collections::HashSet::new();
        result.files.retain_mut(|file| {
            let basename = file.name.rsplit(['/', '\\']).next().unwrap_or_default();
            let title = clean_track_title(basename);
            if library_titles.contains(&title) {
                matched_titles.insert(title);
                true
            } else {
                false
            }
        });
        matched_titles.len() >= required
    });
    Ok(results)
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
/// "Princess". Degenerate artists widen the window further, e.g.:
/// single-letter tokens ("U2" matches any path containing `u` and `2`),
/// a lone stop-word artist ("The" matches nearly every path via the
/// full-name fallback), contraction words ("Guns N' Roses" requires only
/// `guns`, `n`, and `roses` anywhere), and a single-letter artist ("A"
/// matches nearly every path). Empty or blank artist names never match.
/// Accented characters are compared literally ("Tiësto" ≠ "Tiesto") — a
/// false negative, not a false positive. Downstream quality filters still
/// apply.
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
        // All words were stop-words (e.g. artist "The The"): fall back to
        // the full lowercased name as a substring match. Trimmed so a
        // padded batch line (" The The ") still matches paths carrying the
        // unpadded name.
        return normalised.contains(artist.trim().to_lowercase().as_str());
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
        // Padded artist from a sloppy batch line must still match.
        assert!(path_matches_artist(
            "The The - Infected - 01.flac",
            " The The "
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

    // Regression: a fallback result that mixes artist-matching and
    // non-matching files must not leak the non-matching files into the
    // download set. The result-level retain kept the whole result when ANY
    // file matched; download_album then downloaded every quality-passing
    // file, so a peer with a mixed share donated wrong-artist tracks (e.g.
    // an album-only "History" fallback downloaded a Tiësto track from a
    // result that also contained a Michael Jackson file).
    #[tokio::test]
    async fn test_fallback_keeps_only_artist_matching_files_within_result() {
        let client = MockClient::new();
        client.search_results_by_query.lock().unwrap().insert(
            "History".into(),
            vec![SearchResult {
                username: "mixed-share".into(),
                speed: 862_220,
                slots: 1,
                files: vec![
                    make_file(
                        r"Music\Michael Jackson\History\01 - Billie Jean.flac",
                        900,
                        30_000_000,
                    ),
                    make_file(
                        r"Music\Michael Jackson\History\02 - The Way You Make Me Feel.flac",
                        900,
                        30_000_000,
                    ),
                    // The wrong-artist track from the production report:
                    // matches the album-only query, passes quality filters,
                    // but the artist is not in the path.
                    make_file(
                        r"@@sedlr\FLACS\CD RIPS\Tiësto - Parade Of The Athletes (CD RIP) [FLAC]\03 Ancient History.flac",
                        900,
                        45_284_928,
                    ),
                ],
            }],
        );

        let outcome =
            search_album_with_fallback(&client, "Michael Jackson", Some("History"), 15, true)
                .await
                .unwrap();
        assert!(outcome.used_fallback);
        assert_eq!(outcome.results.len(), 1);
        let files = &outcome.results[0].files;
        assert_eq!(
            files.len(),
            2,
            "mixed result must keep all artist-matching files and drop non-matching ones, got {}",
            files.len()
        );
        assert!(
            files
                .iter()
                .all(|f| f.name.to_lowercase().contains("michael jackson")),
            "only artist-matching files may survive, got: {:?}",
            files.iter().map(|f| f.name.as_str()).collect::<Vec<_>>()
        );
    }

    // ── clean_track_title ──

    #[test]
    fn test_clean_track_title_strips_extension_and_leading_track_number() {
        assert_eq!(clean_track_title("03. I Miss You.mp3"), "i miss you");
        assert_eq!(clean_track_title("01 - Hello.flac"), "hello");
        assert_eq!(clean_track_title("12- Bye.mp3"), "bye");
        assert_eq!(clean_track_title("7.On The Floor.mp3"), "on the floor");
    }

    #[test]
    fn test_clean_track_title_removes_brackets_keeps_contents() {
        assert_eq!(
            clean_track_title("01 - Hello (Live) [Remix].flac"),
            "hello live remix"
        );
        assert_eq!(
            clean_track_title("Song {Bonus} [Single].mp3"),
            "song bonus single"
        );
    }

    #[test]
    fn test_clean_track_title_normalizes_unicode() {
        assert_eq!(clean_track_title("Café.mp3"), "cafe");
        assert_eq!(clean_track_title("München 2024.flac"), "munchen 2024");
    }

    #[test]
    fn test_clean_track_title_drops_punctuation_and_collapses_whitespace() {
        assert_eq!(
            clean_track_title("Hello, World! - Final.mp3"),
            "hello world final"
        );
        assert_eq!(clean_track_title("  I'm  Fine  .mp3"), "im fine");
    }

    #[test]
    fn test_clean_track_title_complex_filename() {
        assert_eq!(
            clean_track_title("12 - Hello (feat. Someone) [Bonus] {Live}.flac"),
            "hello feat someone bonus live"
        );
    }

    #[test]
    fn test_clean_track_title_removes_only_leading_track_number() {
        // A second "nn." after the first is just text, not a track number.
        assert_eq!(
            clean_track_title("12 - 04. Song Title.flac"),
            "04 song title"
        );
    }

    // ── get_library_track_filenames ──

    #[test]
    fn test_get_library_track_filenames_collects_sorted_audio_files() {
        let dir = tempfile::TempDir::new().unwrap();
        let artist_album = dir.path().join("Artist").join("Album");
        std::fs::create_dir_all(&artist_album).unwrap();
        for name in ["b.flac", "a.mp3", "c.ogg", "d.OPUS"] {
            std::fs::write(artist_album.join(name), b"x").unwrap();
        }
        // Non-audio files and sub-directories are ignored.
        std::fs::write(artist_album.join("cover.jpg"), b"x").unwrap();
        std::fs::write(artist_album.join("notes.txt"), b"x").unwrap();
        std::fs::create_dir(artist_album.join("subdir")).unwrap();

        let filenames = get_library_track_filenames(
            &[dir.path().to_string_lossy().into_owned()],
            "Artist",
            "Album",
        )
        .unwrap();
        assert_eq!(filenames, vec!["a.mp3", "b.flac", "c.ogg", "d.OPUS"]);
    }

    #[test]
    fn test_get_library_track_filenames_missing_album_dir_returns_empty() {
        let dir = tempfile::TempDir::new().unwrap();
        let filenames = get_library_track_filenames(
            &[dir.path().to_string_lossy().into_owned()],
            "Artist",
            "No Such Album",
        )
        .unwrap();
        assert!(filenames.is_empty());
    }

    #[test]
    fn test_get_library_track_filenames_multiple_paths_are_deduplicated() {
        let dir1 = tempfile::TempDir::new().unwrap();
        let dir2 = tempfile::TempDir::new().unwrap();
        for dir in [&dir1, &dir2] {
            let artist_album = dir.path().join("Artist").join("Album");
            std::fs::create_dir_all(&artist_album).unwrap();
            std::fs::write(artist_album.join("01 - Track.mp3"), b"x").unwrap();
        }
        let filenames = get_library_track_filenames(
            &[
                dir1.path().to_string_lossy().into_owned(),
                dir2.path().to_string_lossy().into_owned(),
            ],
            "Artist",
            "Album",
        )
        .unwrap();
        assert_eq!(filenames, vec!["01 - Track.mp3"]);
    }

    // ── search_by_title ──

    #[tokio::test]
    async fn test_search_by_title_empty_library_returns_empty_without_searching() {
        let client = MockClient::new();
        let results = search_by_title(&client, &[], 15, 100).await.unwrap();
        assert!(results.is_empty());
        assert!(client.search_queries.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_search_by_title_keeps_only_matching_files_and_applies_threshold() {
        let client = MockClient::new();
        let library = vec![
            "01 - Track One.mp3".to_string(),
            "02 - Track Two.flac".to_string(),
        ];
        client.search_results_by_query.lock().unwrap().insert(
            // search_raw queries with the cleaned first title.
            "track one".into(),
            vec![
                SearchResult {
                    username: "full".into(),
                    speed: 500,
                    slots: 2,
                    files: vec![
                        make_file("Album/Track One.mp3", 900, 10_000_000),
                        make_file("Album/Track Two.flac", 900, 11_000_000),
                    ],
                },
                SearchResult {
                    username: "partial".into(),
                    speed: 400,
                    slots: 1,
                    files: vec![
                        make_file("Track One.mp3", 900, 10_000_000),
                        make_file("Someone Else.mp3", 900, 12_000_000),
                    ],
                },
            ],
        );

        // 100% of 2 titles = 2 matching files required: only "full" passes.
        let results = search_by_title(&client, &library, 15, 100).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].username, "full");
        let names: Vec<&str> = results[0]
            .files
            .iter()
            .map(|f| f.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["Album/Track One.mp3", "Album/Track Two.flac"]);
    }

    #[tokio::test]
    async fn test_search_by_title_lower_threshold_keeps_partial_results() {
        let client = MockClient::new();
        let library = vec![
            "01 - Track One.mp3".to_string(),
            "02 - Track Two.flac".to_string(),
        ];
        client.search_results_by_query.lock().unwrap().insert(
            "track one".into(),
            vec![SearchResult {
                username: "partial".into(),
                speed: 400,
                slots: 1,
                files: vec![
                    make_file("Track One.mp3", 900, 10_000_000),
                    make_file("Someone Else.mp3", 900, 12_000_000),
                ],
            }],
        );

        // 50% of 2 titles = 1 matching file required; non-matching files are pruned.
        let results = search_by_title(&client, &library, 15, 50).await.unwrap();
        assert_eq!(results.len(), 1);
        let names: Vec<&str> = results[0]
            .files
            .iter()
            .map(|f| f.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["Track One.mp3"]);
    }

    #[tokio::test]
    async fn test_search_by_title_cleaned_library_matches_via_basename_and_unicode() {
        let client = MockClient::new();
        let library = vec!["01 - Cafés.mp3".to_string()];
        client.search_results_by_query.lock().unwrap().insert(
            "cafes".into(),
            vec![SearchResult {
                username: "peer".into(),
                speed: 500,
                slots: 1,
                files: vec![
                    // Windows-style path separator; basename cleaned to "cafes".
                    make_file(r"Music\Artist\Album\01 - Cafés.mp3", 900, 5_000_000),
                    make_file("Wrong Track.flac", 900, 6_000_000),
                ],
            }],
        );

        let results = search_by_title(&client, &library, 15, 100).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].files.len(), 1);
        assert_eq!(
            results[0].files[0].name,
            r"Music\Artist\Album\01 - Cafés.mp3"
        );
    }

    // ── SearchOutcome ──
}
