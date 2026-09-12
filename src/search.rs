use crate::client::{FileInfo, SearchResult, SoulseekClient};
use crate::config::FilterConfig;
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
    // Trim the assembled query: when artist is empty (album-only search) the
    // artist-album join would otherwise produce a leading space
    // ("<artist> <album>" -> " Musicology"), which would be sent verbatim to
    // Soulseek. Trimming keeps the album-only query clean ("Musicology").
    let query = match album {
        Some(a) if !a.is_empty() => format!("{artist} {a}").trim().to_string(),
        _ => artist.to_string(),
    };
    search_raw(client, &query, timeout_secs).await
}

/// Outcome of an album search.
#[derive(Debug, Clone)]
pub struct SearchOutcome {
    pub results: Vec<SearchResult>,
}

/// A distinct album discovered from one artist-only search.
#[derive(Debug, Clone)]
pub(crate) struct ArtistAlbumResults {
    pub album: String,
    pub results: Vec<SearchResult>,
}

/// Common articles that carry no discriminating power when matching names.
const ARTIST_STOP_WORDS: &[&str] = &["the", "a", "an"];

/// Words allowed between the significant words of a repeated artist prefix.
const ARTIST_PREFIX_NOISE_WORDS: &[&str] = &["the", "a", "an", "and"];

/// Explicit audio/source labels that do not distinguish an album title.
const ALBUM_FORMAT_SUFFIXES: &[&str] = &[
    "flac", "mp3", "wav", "ape", "alac", "aiff", "m4a", "ogg", "lossless", "320", "320kbps",
    "24bit",
];

/// Stable artist identity used by processed-history reconciliation.
pub(crate) fn artist_identity_key(artist: &str) -> String {
    let normalized = if contains_non_ascii_alphanumeric(artist) {
        unicode_identity_key(artist)
    } else {
        normalize_search_term(artist).to_lowercase()
    };
    let fallback = normalized.trim().to_string();
    let mut words: Vec<String> = normalized
        .split_whitespace()
        .filter(|word| !ARTIST_STOP_WORDS.contains(word))
        .map(str::to_string)
        .collect();
    words.sort();
    if words.is_empty() {
        fallback
    } else {
        words.join(" ")
    }
}

/// Return the title after an explicit `Artist - Title` or `Artist: Title`
/// prefix. Single-word artists require this boundary to avoid treating titles
/// such as `Doors Open` as artist-prefixed.
fn strip_explicit_artist_prefix<'a>(album: &'a str, artist: &str) -> Option<&'a str> {
    [" - ", ": ", " – ", " — "]
        .into_iter()
        .find_map(|separator| {
            let (prefix, title) = album.split_once(separator)?;
            (artist_identity_key(prefix) == artist_identity_key(artist)).then_some(title)
        })
}

fn contains_non_ascii_alphanumeric(value: &str) -> bool {
    value
        .nfkd()
        .any(|character| character.is_alphanumeric() && !character.is_ascii())
}

fn unicode_identity_key(value: &str) -> String {
    let mut folded = String::new();
    for character in value.nfkc().flat_map(char::to_lowercase) {
        match character {
            'æ' => folded.push_str("ae"),
            'œ' => folded.push_str("oe"),
            'ß' => folded.push_str("ss"),
            'ø' => folded.push('o'),
            'ł' => folded.push('l'),
            _ => folded.push(character),
        }
    }
    folded
        .split(|character: char| !character.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// True for a bare four-digit year token such as "1998".
fn is_year_token(token: &str) -> bool {
    token.len() == 4
        && token.chars().all(|c| c.is_ascii_digit())
        && (token.starts_with("19") || token.starts_with("20"))
}

/// Remove an explicit trailing trademark marker without altering ordinary
/// words ending in the letters `tm` (for example, `ATM`).
fn strip_ascii_trademark_suffix(value: &str) -> &str {
    let Some(prefix) = value.strip_suffix("TM") else {
        return value;
    };
    if prefix.chars().last().is_some_and(char::is_lowercase) {
        prefix
    } else {
        value
    }
}

/// Drop a leading run of tokens that repeats the requested artist name, so
/// "Kruder & Dorfmeister - The K&D Sessions" and "Kruder Dorfmeister - ..."
/// both reduce to the album title. Nothing is dropped unless at least one
/// artist token was matched.
fn strip_artist_prefix(tokens: &mut Vec<String>, artist: &str) {
    let artist_words = canonical_artist_words(artist);
    let significant: Vec<&str> = artist_words
        .iter()
        .map(String::as_str)
        .filter(|word| !ARTIST_PREFIX_NOISE_WORDS.contains(word))
        .collect();
    // A single shared word is too weak: artist "The Doors" has an album
    // "Doors Open", which must not collapse into a separate "Open" album.
    if significant.len() < 2 {
        return;
    }
    let mut index = 0usize;
    for expected in significant {
        while tokens
            .get(index)
            .is_some_and(|token| ARTIST_PREFIX_NOISE_WORDS.contains(&token.as_str()))
        {
            index += 1;
        }
        if tokens.get(index).map(String::as_str) != Some(expected) {
            return;
        }
        index += 1;
    }
    while tokens
        .get(index)
        .is_some_and(|token| ARTIST_PREFIX_NOISE_WORDS.contains(&token.as_str()))
    {
        index += 1;
    }
    tokens.drain(..index);
}

/// Strip an abbreviated multi-word artist prefix only when it directly
/// precedes a bare-year album title (`K+D 1995` for Kruder & Dorfmeister).
fn strip_artist_initials_before_year_title(tokens: &mut Vec<String>, artist: &str) {
    let initials: Vec<String> = canonical_artist_words(artist)
        .into_iter()
        .filter(|word| !ARTIST_PREFIX_NOISE_WORDS.contains(&word.as_str()))
        .filter_map(|word| word.chars().next().map(|initial| initial.to_string()))
        .collect();
    if initials.len() < 2 {
        return;
    }
    let mut index = 0usize;
    for expected in &initials {
        if tokens.get(index) != Some(expected) {
            return;
        }
        index += 1;
        if tokens.get(index).is_some_and(|token| token == "and") {
            index += 1;
        }
    }
    if tokens.len() == index + 1 && is_year_token(&tokens[index]) {
        tokens.drain(..index);
    }
}

/// Drop leading bare release-year tokens, as in "1998 - The K&D Sessions".
///
/// Only a leading year is release metadata. A year inside a title stays part of
/// the title, so "Blade Runner 2049" never collapses into "Blade Runner", and
/// an all-year title ("2020 - 1995") reduces to its trailing year.
fn strip_release_years(tokens: &mut Vec<String>) {
    // Preserve titles such as "2001: A Space Odyssey"; an initial year before
    // the articles "a" or "an" is treated as title text, not release metadata.
    if tokens.len() > 2 && matches!(tokens[1].as_str(), "a" | "an") {
        return;
    }
    let leading_years = tokens
        .iter()
        .take_while(|token| is_year_token(token))
        .count()
        .min(tokens.len().saturating_sub(1));
    tokens.drain(..leading_years);
}

/// Drop trailing audio-format labels that do not distinguish a release.
fn strip_format_suffixes(tokens: &mut Vec<String>) {
    while tokens.len() > 1
        && tokens
            .last()
            .is_some_and(|token| ALBUM_FORMAT_SUFFIXES.contains(&token.as_str()))
    {
        tokens.pop();
    }
}

/// Drop leading articles so "The K&D Sessions" equals "K&D Sessions".
fn strip_leading_article(tokens: &mut Vec<String>) {
    let articles = tokens
        .iter()
        .take_while(|token| ARTIST_STOP_WORDS.contains(&token.as_str()))
        .count();
    tokens.drain(..articles);
}

fn strip_bracketed_years(value: &str) -> std::borrow::Cow<'_, str> {
    static BRACKETED_YEAR: OnceLock<Regex> = OnceLock::new();
    let bracketed_year = BRACKETED_YEAR.get_or_init(|| {
        Regex::new(r"[\[({]\s*(?:19|20)\d{2}\s*[\])}]").expect("valid bracketed-year regex")
    });
    bracketed_year.replace_all(value, " ")
}

/// Tokenize an album folder name for identity comparison: fold case, drop
/// publishing marks, treat `&`/`+` as the word "and", and remove bracketed
/// release years.
fn album_identity_tokens(album: &str) -> Vec<String> {
    // Trademark and registered marks are publishing noise, not title text.
    let without_marks: String = album
        .chars()
        .filter(|c| !matches!(c, '\u{2122}' | '\u{00ae}' | '\u{00a9}'))
        .collect();
    let without_marks = strip_ascii_trademark_suffix(&without_marks);
    // A bracketed bare year is release metadata rather than title text.
    let without_years = strip_bracketed_years(without_marks);
    let normalized = normalize_search_term(&without_years).to_lowercase();
    let mut tokens: Vec<String> = normalized
        .split(|character: char| !character.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(str::to_string)
        .collect();
    if tokens.len() > 1 && tokens.last().is_some_and(|token| token == "tm") {
        tokens.pop();
    }
    tokens
}

/// Identity key that collapses the different textual representations of one
/// release folder into a single album.
///
/// Sharers name the same release in many ways — an artist prefix, a leading
/// release year, bracketed years, trademark marks, `&` instead of `and`, and
/// inconsistent punctuation. Keying on the raw lowercase folder
/// name treated each spelling as its own album, so one artist-only run
/// downloaded the same album several times.
///
/// A non-empty folder name always yields a non-empty key: when ASCII folding
/// removes all title text, the lowercased Unicode folder name is retained.
pub(crate) fn album_identity_key(album: &str, artist: &str) -> String {
    let logical_album =
        crate::discs::strip_embedded_disc_marker(album).unwrap_or_else(|| album.trim().to_string());
    let title = strip_explicit_artist_prefix(&logical_album, artist).unwrap_or(&logical_album);
    let title = strip_ascii_trademark_suffix(title);
    let mut tokens = if contains_non_ascii_alphanumeric(title) {
        unicode_identity_key(&strip_bracketed_years(title))
            .split_whitespace()
            .map(str::to_string)
            .collect()
    } else {
        album_identity_tokens(title)
    };
    let normalized_fallback = tokens.join(" ");
    strip_release_years(&mut tokens);
    strip_artist_prefix(&mut tokens, artist);
    strip_release_years(&mut tokens);
    strip_artist_initials_before_year_title(&mut tokens, artist);
    strip_format_suffixes(&mut tokens);
    strip_leading_article(&mut tokens);

    if tokens.is_empty() {
        if normalized_fallback.is_empty() {
            return logical_album.to_lowercase();
        }
        return normalized_fallback;
    }
    tokens.join(" ")
}

/// Group one artist search response into independent album candidates.
///
/// Files are first checked against the requested artist path, then grouped by
/// the album directory immediately above the file. A dedicated disc directory
/// is folded into its parent album, so all discs of one album remain together.
/// Results without an identifiable album directory are omitted because their
/// album cannot be safely named or organized.
pub(crate) fn group_artist_results(
    results: &[SearchResult],
    artist: &str,
) -> Vec<ArtistAlbumResults> {
    let mut groups: std::collections::HashMap<String, (String, Vec<SearchResult>)> =
        std::collections::HashMap::new();
    let mut identity_cache = std::collections::HashMap::new();

    for result in results {
        let mut result_albums: std::collections::HashMap<String, (String, Vec<FileInfo>)> =
            std::collections::HashMap::new();
        for file in &result.files {
            if !path_matches_artist_directory(&file.name, artist) {
                continue;
            }
            let Some(album) = artist_album_name(&file.name, artist) else {
                continue;
            };
            // Keep each spelling variant as its own candidate for one peer.
            // True disc markers already share the same stripped `album` and
            // remain together; unrelated raw folders must not combine to pass
            // `min_tracks` when only one folder will be downloaded.
            let raw_key = album.to_lowercase();
            let entry = result_albums
                .entry(raw_key)
                .or_insert_with(|| (album.clone(), Vec::new()));
            entry.1.push(file.clone());
        }

        for (_raw_key, (album, files)) in result_albums {
            let key = identity_cache
                .entry(album.clone())
                .or_insert_with(|| album_identity_key(&album, artist))
                .clone();
            let entry = groups
                .entry(key)
                .or_insert_with(|| (album.clone(), Vec::new()));
            // Keep the lexicographically smallest spelling as the album name.
            // `result_albums` is a HashMap, so iteration order varies between
            // runs; an unstable name would change the staging directory,
            // report label, and processed-album record, making one release
            // look like a new album and download it again.
            if album < entry.0 {
                entry.0 = album;
            }
            entry.1.push(SearchResult {
                username: result.username.clone(),
                speed: result.speed,
                slots: result.slots,
                files,
            });
        }
    }

    let mut albums: Vec<ArtistAlbumResults> = groups
        .into_values()
        .map(|(album, results)| ArtistAlbumResults { album, results })
        .collect();
    albums.sort_by_cached_key(|group| group.album.to_lowercase());
    albums
}

/// Extract a logical album name from a share-relative file path.
fn artist_album_name(path: &str, artist: &str) -> Option<String> {
    let components: Vec<&str> = path
        .split(['/', '\\'])
        .filter(|part| !part.is_empty())
        .collect();
    let mut album_index = components.len().checked_sub(2)?;
    if crate::discs::is_disc_folder(components[album_index]) {
        album_index = album_index.checked_sub(1)?;
    }
    if album_index == 0 {
        return None;
    }
    if !artist_directory_matches(&components[album_index - 1..album_index], artist) {
        return None;
    }
    let leaf = components[album_index];
    let album = crate::discs::strip_embedded_disc_marker(leaf).unwrap_or_else(|| leaf.to_owned());
    let album = album.trim();
    (!album.is_empty()).then(|| album.to_owned())
}

/// Match the requested artist against one directory component, ignoring
/// common articles but rejecting extra artist words such as `Other Artist`
/// when the target is only `Artist`.
fn canonical_artist_words(value: &str) -> Vec<String> {
    normalize_search_term(value)
        .split_whitespace()
        .map(|word| {
            if word.eq_ignore_ascii_case("n") {
                "and".to_string()
            } else {
                word.to_lowercase()
            }
        })
        .collect()
}

fn artist_directory_matches(components: &[&str], artist: &str) -> bool {
    let artist_words = canonical_artist_words(artist);
    if artist_words.is_empty() {
        return false;
    }
    let artist_distinctive: Vec<String> = artist_words
        .iter()
        .filter(|word| !ARTIST_STOP_WORDS.contains(&word.as_str()))
        .cloned()
        .collect();
    let ignore_stop_words = !artist_distinctive.is_empty();
    let expected = if ignore_stop_words {
        artist_distinctive
    } else {
        artist_words
    };
    components.iter().any(|component| {
        let component_words: Vec<String> = canonical_artist_words(component)
            .into_iter()
            .filter(|word| !ignore_stop_words || !ARTIST_STOP_WORDS.contains(&word.as_str()))
            .collect();
        let mut actual = component_words;
        let mut expected = expected.clone();
        actual.sort_unstable();
        expected.sort_unstable();
        actual == expected
    })
}

/// Keep only files whose directory path contains an exact artist directory.
pub(crate) fn retain_artist_files(results: &mut Vec<SearchResult>, artist: &str) {
    for result in &mut *results {
        result
            .files
            .retain(|file| path_matches_artist_directory(&file.name, artist));
    }
    results.retain(|result| !result.files.is_empty());
}

/// Check whether a file path contains an exact artist directory component.
fn path_matches_artist_directory(path: &str, artist: &str) -> bool {
    let components: Vec<&str> = path
        .split(['/', '\\'])
        .filter(|part| !part.is_empty())
        .collect();
    components.len() >= 2 && artist_directory_matches(&components[..components.len() - 1], artist)
}

/// Normalize an artist/album name for the punctuation-tolerant search tier.
///
/// Case is preserved (the lowercase tier covers that axis); accented
/// characters are folded to ASCII via NFKD ("Tiësto" -> "Tiesto").
/// Characters with no ASCII NFKD decomposition (e.g. dotless i, CJK,
/// Cyrillic) are dropped, so a name made entirely of them normalises to
/// the empty string (callers must guard on that). The ampersand and plus
/// become the word "and"; tight punctuation (period, comma, apostrophe,
/// double-quote, backtick) is removed so letters join ("S.P.Y." -> "SPY",
/// "D'Angelo" -> "DAngelo"); separators (hyphen, underscore, slash,
/// backslash, and brackets) become spaces ("In-The-Skys" -> "In The Skys",
/// "AC/DC" -> "AC DC"). Whitespace is collapsed and trimmed.
pub fn normalize_search_term(input: &str) -> String {
    let folded: String = input
        .nfkd()
        .map(|c| {
            if c.is_whitespace() || matches!(c, '\u{2010}'..='\u{2015}' | '\u{00B7}') {
                ' '
            } else {
                c
            }
        })
        .filter(|c| c.is_ascii())
        .collect();
    let mut out = String::with_capacity(folded.len() + 8);
    for c in folded.chars() {
        match c {
            '&' | '+' => out.push_str(" and "),
            '.' | ',' | '\'' | '"' | '`' => {}
            '-' | '_' | '/' | '\\' | '(' | ')' | '[' | ']' | '{' | '}' => out.push(' '),
            _ => out.push(c),
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Returns true when `results` contain at least one result that passes the
/// full (queue-aware) filter pipeline — i.e. when the tier produced a
/// downloadable result set. The cascade continues to the next tier when a
/// tier returns non-empty but unusable results.
fn tier_has_usable_results(
    results: &[SearchResult],
    filters: &FilterConfig,
    library_track_count: Option<usize>,
    album: Option<&str>,
    max_queue_length: u32,
) -> bool {
    !crate::filter::filter_results_with_queue_limit(
        results,
        filters,
        library_track_count,
        album,
        max_queue_length,
    )
    .is_empty()
}

/// Run a fallback-tier search, treating errors as "no results" so a
/// transient timeout/disconnect on a lower tier does not abort the whole
/// album. The primary tier still propagates errors via `?`.
async fn search_fallback_tier(
    client: &dyn SoulseekClient,
    query_artist: &str,
    query_album: Option<&str>,
    timeout_secs: u64,
) -> Vec<SearchResult> {
    match search_album(client, query_artist, query_album, timeout_secs).await {
        Ok(results) => results,
        Err(e) => {
            tracing::warn!("fallback search failed, skipping tier: {e}");
            Vec::new()
        }
    }
}

/// Search for an album by artist + album name, stopping at the first tier
/// whose results survive the full filter pipeline
/// ([`crate::filter::filter_results`]).
///
/// Tier 1a runs the primary "Artist Album" search (original casing). Tier 1b
/// retries lowercased (Soulseek returns different result sets per casing, and
/// lowercase tends to be lower quality, so original casing is preferred);
/// skipped when the artist is empty or the query is already lowercase.
/// Tier 1c retries with punctuation normalised via [`normalize_search_term`]
/// (case preserved; skipped when normalisation changes nothing). Tier 2 falls
/// back to an album-name-only search for artist+album queries (artist `""`),
/// keeping only results whose file paths match the artist via
/// [`path_matches_artist`]. It is skipped when the artist is already empty
/// because Tier 1 is already an album-only search.
///
/// A tier is accepted only when it yields at least one result that passes the
/// filters (extension, bitrate, slots, min tracks, contiguity, album gate).
/// When a tier returns non-empty but unusable results, the cascade continues
/// to the next tier. If no tier yields usable results, the first non-empty
/// tier's results are returned so the caller's rejection summary and
/// title-search fallback still have data. An empty outcome is returned when
/// no tier produces any results (including the album-only tier matching no
/// artist).
///
/// This wrapper is free-slot-only (queue cap 0): a tier whose results all
/// have zero free slots is not considered usable. Callers that enforce
/// `download.max_queue_length` use
/// [`search_album_with_fallback_with_queue_limit`], so a zero-slot peer
/// admitted by a positive queue cap is not discarded before download.
pub async fn search_album_with_fallback(
    client: &dyn SoulseekClient,
    artist: &str,
    album: Option<&str>,
    timeout_secs: u64,
    filters: &FilterConfig,
    library_track_count: Option<usize>,
) -> Result<SearchOutcome> {
    search_album_with_fallback_with_queue_limit(
        client,
        artist,
        album,
        timeout_secs,
        filters,
        library_track_count,
        0,
    )
    .await
}

/// Queue-aware variant of [`search_album_with_fallback`]. A tier is usable
/// when it yields at least one result that passes the queue-aware filter
/// pipeline, so a zero-slot candidate admitted by a positive queue cap does
/// not trigger unnecessary fallback searches.
pub(crate) async fn search_album_with_fallback_with_queue_limit(
    client: &dyn SoulseekClient,
    artist: &str,
    album: Option<&str>,
    timeout_secs: u64,
    filters: &FilterConfig,
    library_track_count: Option<usize>,
    max_queue_length: u32,
) -> Result<SearchOutcome> {
    // The first tier whose raw results were non-empty but did not survive
    // filtering. Returned when no tier yields a usable (filter-passing)
    // result set, so the caller's rejection summary and title-search
    // fallback still have data to work with.
    let mut fallback: Option<Vec<SearchResult>> = None;

    // Tier 1a: primary "Artist Album" search (original casing)
    if let Some(a) = album.filter(|a| !a.trim().is_empty()) {
        if artist.trim().is_empty() {
            tracing::info!("Searching for Album ({})", a.trim());
        } else {
            tracing::info!(
                "Searching for Artist + Album ({} {})",
                artist.trim(),
                a.trim()
            );
        }
    } else {
        tracing::info!("Searching for Artist ({artist})");
    }
    let mut results = search_album(client, artist, album, timeout_secs).await?;
    if album.is_some() && !artist.trim().is_empty() {
        retain_artist_files(&mut results, artist);
    }
    if !results.is_empty() {
        if tier_has_usable_results(
            &results,
            filters,
            library_track_count,
            album,
            max_queue_length,
        ) {
            return Ok(SearchOutcome { results });
        }
        fallback = Some(results);
    }

    // Tier 1b: lowercase fallback (when original casing was not usable)
    if let Some(album_name) = album {
        if !album_name.trim().is_empty() && !artist.trim().is_empty() {
            let artist_lower = artist.to_lowercase();
            let album_lower = album_name.to_lowercase();
            if artist_lower != artist || album_lower != album_name {
                tracing::info!(
                    "Searching for Artist + Album lowercase ({} {})",
                    artist_lower.trim(),
                    album_lower.trim()
                );
                let mut lower_results =
                    search_fallback_tier(client, &artist_lower, Some(&album_lower), timeout_secs)
                        .await;
                retain_artist_files(&mut lower_results, artist);
                if !lower_results.is_empty() {
                    if tier_has_usable_results(
                        &lower_results,
                        filters,
                        library_track_count,
                        album,
                        max_queue_length,
                    ) {
                        return Ok(SearchOutcome {
                            results: lower_results,
                        });
                    }
                    if fallback.is_none() {
                        fallback = Some(lower_results);
                    }
                }
            }
        }
    }

    // Tier 1c: punctuation-normalised fallback (when the casing variants
    // were not usable). Peers often list names with different punctuation.
    if let Some(album_name) = album {
        if !album_name.trim().is_empty() && !artist.trim().is_empty() {
            let artist_norm = normalize_search_term(artist);
            let album_norm = normalize_search_term(album_name);
            let artist_collapsed = artist.split_whitespace().collect::<Vec<_>>().join(" ");
            let album_collapsed = album_name.split_whitespace().collect::<Vec<_>>().join(" ");
            if !artist_norm.is_empty()
                && !album_norm.is_empty()
                && (artist_norm != artist_collapsed || album_norm != album_collapsed)
            {
                tracing::info!(
                    "Searching for Artist + Album punctuation-normalised ({} {})",
                    artist_norm,
                    album_norm
                );
                let mut norm_results =
                    search_fallback_tier(client, &artist_norm, Some(&album_norm), timeout_secs)
                        .await;
                retain_artist_files(&mut norm_results, artist);
                if !norm_results.is_empty() {
                    if tier_has_usable_results(
                        &norm_results,
                        filters,
                        library_track_count,
                        album,
                        max_queue_length,
                    ) {
                        return Ok(SearchOutcome {
                            results: norm_results,
                        });
                    }
                    if fallback.is_none() {
                        fallback = Some(norm_results);
                    }
                }
            }
        }
    }

    // Tier 2: album-only search for artist+album queries (when the casing and
    // punctuation variants were not usable). Album-only input already ran this
    // query as Tier 1, so repeating it would only duplicate network traffic.
    if !artist.trim().is_empty() {
        if let Some(album_name) = album {
            if !album_name.trim().is_empty() {
                tracing::info!("Searching for Album ({})", album_name.trim());
                let mut artist_matches =
                    search_fallback_tier(client, "", Some(album_name), timeout_secs).await;
                retain_artist_files(&mut artist_matches, artist);
                if !artist_matches.is_empty() {
                    if tier_has_usable_results(
                        &artist_matches,
                        filters,
                        library_track_count,
                        album,
                        max_queue_length,
                    ) {
                        return Ok(SearchOutcome {
                            results: artist_matches,
                        });
                    }
                    if fallback.is_none() {
                        fallback = Some(artist_matches);
                    }
                }
            }
        }
    }

    // No tier yielded a usable result set: return the first non-empty tier's
    // results (today's behaviour) or empty if every tier was empty.
    Ok(SearchOutcome {
        results: fallback.unwrap_or_default(),
    })
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
    // Treat common separators (underscore, hyphen, plus) as word boundaries
    // so tokens stay separate ("Prince_-_Musicology" → "prince musicology",
    // not the smeared "princemusicology"). Without this the artist name
    // cannot be removed and the fallback query stays blocked.
    let separated = no_brackets.replace(['_', '-', '+'], " ");
    let normalized: String = separated
        .nfkd()
        .filter(|c| c.is_ascii())
        .collect::<String>()
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect();
    normalized.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Returns true if the track name is generic/meaningless for search purposes.
/// Generic names produce garbage queries that match unrelated albums.
pub fn is_generic_track_name(name: &str) -> bool {
    let cleaned = clean_track_title(name);
    if cleaned.is_empty() {
        return true;
    }
    // Patterns that indicate generic/meaningless track names. Each keyword
    // (optionally followed by a track number) must span the whole cleaned
    // name, so multi-word real titles are not misclassified: "CD Track 1",
    // "Track1", "Audio 2", "Recording 5", "Untitled", "Unknown" and bare
    // numbers ("01", "42") are generic, while "hello", "musicology",
    // "i miss" are not. Edge case: tracks literally titled "Untitled 2"
    // or "Unknown 1" will be filtered — accepted trade-off.
    static GENERIC_PATTERNS: OnceLock<Regex> = OnceLock::new();
    let re = GENERIC_PATTERNS.get_or_init(|| {
        Regex::new(r"(?i)^((untitled|unknown|cd\s*track|track|audio|recording)(\s*\d+)?|\d+)$")
            .expect("valid generic pattern regex")
    });
    re.is_match(&cleaned)
}

/// Build the Soulseek query for the fallback track search.
///
/// Cleans the library track filename (see [`clean_track_title`]) and strips
/// the artist name tokens so the resulting query searches by track name only.
/// This is essential because Soulseek blocks certain artists from being
/// found: including the artist in the query reproduces the same block that
/// the artist+album search hit, so the whole fallback would fail. Returns an
/// empty string when nothing remains after removing the artist.
///
/// The artist is split into alphanumeric words and each is removed from the
/// cleaned title (which is already lowercased/ascii-normalized). If the
/// cleaned title equals the artist (e.g. a title that is only the artist
/// name), the track number/album tokens are not present so it collapses to
/// empty — the caller treats that as "cannot build a track query".
pub fn fallback_track_query(filename: &str, artist: &str) -> String {
    let cleaned = clean_track_title(filename);
    if cleaned.is_empty() || artist.trim().is_empty() {
        return cleaned;
    }
    // Artist alphanumeric words, normalized the same way as clean_track_title
    // (NFKD + ASCII filter + lowercase + separator→space + split_whitespace)
    // so tokens match regardless of accented characters, separator style, or
    // embedded punctuation (e.g. "D'Angelo" → "dangelo", matching the
    // cleaned title's single token).
    let artist_tokens: std::collections::HashSet<String> = artist
        .replace(['_', '-', '+'], " ")
        .nfkd()
        .filter(|c| c.is_ascii())
        .collect::<String>()
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect::<String>()
        .split_whitespace()
        .map(String::from)
        .collect();
    let words: Vec<&str> = cleaned
        .split_whitespace()
        .filter(|w| !artist_tokens.contains(*w))
        .collect();
    words.join(" ")
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

/// Decide whether a cleaned library title `lib` matches a cleaned peer title
/// `peer`, guarding against false positives on short titles.
///
/// Multi-word titles match as a substring — real peer filenames embed extra
/// metadata, so "tomorrow comes today" appears inside "gorillaz tomorrow
/// comes today", and exact equality would wrongly reject it. A single-word
/// title, by contrast, is matched only at word boundaries so a short library
/// title like "one" cannot match inside an unrelated word such as "someone",
/// and a single word shorter than 4 characters is rejected outright as too
/// ambiguous to identify a track reliably (e.g. "in", "the").
fn lib_title_matches(peer: &str, lib: &str) -> bool {
    if lib.split_whitespace().count() >= 2 {
        return peer.contains(lib);
    }
    if lib.len() < 4 {
        return false; // too short to be discriminating
    }
    peer.split(|c: char| !c.is_alphanumeric())
        .any(|word| word == lib)
}

/// Search Soulseek by the cleaned library track titles, keeping only
/// results that contain at least `match_threshold_pct`% of the album's
/// tracks.
///
/// Every library filename is normalized with [`clean_track_title`]; the
/// alphabetically-first title (the library list is sorted by
/// [`get_library_track_filenames`]) becomes the search query, stripped of
/// the artist name via [`fallback_track_query`] so the query isn't blocked
/// by Soulseek's artist filter. Each result's files are pruned to those
/// whose cleaned basename (last path component) contains at least one
/// library title as a substring, and a result survives only when the
/// number of distinct matched titles is at least
/// `ceil(len(titles) * threshold / 100)`. An empty library short-circuits
/// to an empty result set without touching the network.
pub async fn search_by_title(
    client: &dyn SoulseekClient,
    library_filenames: &[String],
    artist: &str,
    timeout_secs: u64,
    match_threshold_pct: u32,
) -> Result<Vec<SearchResult>> {
    // Filter out generic track names before building the query
    let non_generic: Vec<String> = library_filenames
        .iter()
        .filter(|f| !is_generic_track_name(f))
        .cloned()
        .collect();
    if non_generic.is_empty() {
        // All tracks have generic names — can't build a meaningful query
        return Ok(Vec::new());
    }
    // Clean titles — preserves order from sorted filenames for the query.
    let clean_titles: Vec<String> = non_generic
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
    // Search by the alphabetically-first cleaned title, stripped of the
    // artist name so the query isn't blocked by Soulseek's artist filter.
    // Use clean_titles[0] (already cleaned/filtered) rather than the raw
    // library_filenames[0], so a file whose title cleans to empty (e.g.
    // "01.mp3") doesn't short-circuit the whole fallback.
    let query = fallback_track_query(&clean_titles[0], artist);
    if query.is_empty() {
        return Ok(Vec::new());
    }
    let required = library_titles
        .len()
        .saturating_mul(match_threshold_pct as usize)
        .div_ceil(100);
    tracing::info!("Searching by track title ({query})");
    let mut results = search_raw(client, &query, timeout_secs).await?;
    // Keep only files whose cleaned basename contains at least one distinct
    // library title as a substring, and keep the result only if enough
    // distinct titles matched. Substring containment (rather than exact
    // equality) is required because real peer filenames embed extra
    // metadata — artist/album names — so "Gorillaz - Tomorrow Comes
    // Today.mp3" cleans to "gorillaz tomorrow comes today", which is a
    // superset of the library title "tomorrow comes today". Exact equality
    // rejected these and made the title-search fallback return 0 results
    // despite Soulseek returning plenty of matches.
    //
    // Matching iterates library titles (not files) so that (a) every title
    // that appears as a substring in any file is counted (deterministic,
    // no HashSet-iteration-order dependence) and (b) a single file can
    // satisfy multiple titles when titles overlap as substrings.
    results.retain_mut(|result| {
        let cleaned_titles: Vec<String> = result
            .files
            .iter()
            .map(|f| {
                let basename = f.name.rsplit(['/', '\\']).next().unwrap_or_default();
                clean_track_title(basename)
            })
            .collect();
        let matched = library_titles
            .iter()
            .filter(|lib| cleaned_titles.iter().any(|t| lib_title_matches(t, lib)))
            .count();
        result.files.retain(|f| {
            let basename = f.name.rsplit(['/', '\\']).next().unwrap_or_default();
            let title = clean_track_title(basename);
            library_titles
                .iter()
                .any(|lib| lib_title_matches(&title, lib))
        });
        matched >= required
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

/// Check whether a share-relative file path matches the artist by exact
/// alphanumeric word tokens.
///
/// Common articles are dropped when other artist words exist, so "The
/// Beatles" matches a path containing "Beatles". Matching uses whole tokens,
/// not substrings, so "Prince" does not match "Princess". If an artist is
/// made entirely of stop-words, every stop-word token must be present.
///
/// Used by the album-only fallback tier and artist-only album discovery to
/// verify that a search result belongs to the target artist. Empty or blank
/// artist names never match.
pub fn path_matches_artist(path: &str, artist: &str) -> bool {
    if artist.trim().is_empty() {
        return false;
    }
    let path_words: Vec<String> = path
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect();
    let words: Vec<String> = artist
        .split(|c: char| !c.is_alphanumeric())
        .map(str::to_lowercase)
        .filter(|word| !word.is_empty())
        .collect();
    if words.is_empty() {
        return false;
    }
    let distinctive: Vec<&String> = words
        .iter()
        .filter(|word| !ARTIST_STOP_WORDS.contains(&word.as_str()))
        .collect();
    let required = if distinctive.is_empty() {
        words.iter().collect()
    } else {
        distinctive
    };
    required
        .iter()
        .all(|word| path_words.iter().any(|path_word| path_word == *word))
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

    fn test_filters() -> FilterConfig {
        FilterConfig {
            allowed_extensions: vec!["flac".into()],
            min_bit_rate: 0,
            min_bit_depth: 0,
            exclude_words: vec![],
            include_locked: false,
            contiguous_tracks: false,
            min_tracks: 0,
            peer_track_count: false,
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

    #[test]
    fn test_group_artist_results_separates_albums_and_merges_discs() {
        let results = vec![SearchResult {
            username: "peer1".into(),
            speed: 500,
            slots: 1,
            files: vec![
                make_file(
                    r"Test Artist\Album One\CD 01\01 - one.flac",
                    900,
                    30_000_000,
                ),
                make_file(
                    r"Test Artist\Album One\CD 02\01 - two.flac",
                    900,
                    30_000_000,
                ),
                make_file(r"Test Artist\Album Two\01 - one.flac", 900, 30_000_000),
                make_file(
                    r"Other Performer\Album Three\01 - one.flac",
                    900,
                    30_000_000,
                ),
                make_file(r"Test Artist\01 - loose.flac", 900, 30_000_000),
                make_file(
                    r"Test Artist\Other Artist\Album Three\01 - nested.flac",
                    900,
                    30_000_000,
                ),
            ],
        }];

        let albums = group_artist_results(&results, "Test Artist");
        assert_eq!(
            albums
                .iter()
                .map(|album| album.album.as_str())
                .collect::<Vec<_>>(),
            vec!["Album One", "Album Two"]
        );
        assert_eq!(albums[0].results.len(), 1);
        assert_eq!(albums[0].results[0].files.len(), 2);
        assert_eq!(albums[1].results[0].files.len(), 1);
    }

    /// Build one peer result per path so each path is an independent album
    /// candidate, mirroring separate sharers of the same release.
    fn album_variant_results(paths: &[&str]) -> Vec<SearchResult> {
        paths
            .iter()
            .enumerate()
            .map(|(index, path)| SearchResult {
                username: format!("peer-{index}"),
                speed: 500,
                slots: 1,
                files: vec![make_file(path, 900, 30_000_000)],
            })
            .collect()
    }

    /// Regression: the reported artist-only run for "kruder & dorfmeister"
    /// downloaded the same album several times because every textual
    /// representation of one release folder became its own work item.
    #[test]
    fn test_group_artist_results_collapses_one_release_across_folder_variants() {
        let variants = [
            r"MUSIC\Kruder & Dorfmeister\[2014] The K&D Sessions\01 - Heroes.flac",
            r"MUSIC\Kruder & Dorfmeister\Kruder and Dorfmeister - The K&D Sessions™\01 - Heroes.flac",
            r"MUSIC\Kruder & Dorfmeister\The K&D SessionsTM\01 - Heroes.flac",
            r"MUSIC\Kruder & Dorfmeister\The K&D Sessions TM\01 - Heroes.flac",
            r"MUSIC\Kruder & Dorfmeister\Kruder & Dorfmeister - 1998 - The K&D Sessions\01 - Heroes.flac",
            r"MUSIC\Kruder & Dorfmeister\Kruder & Dorfmeister - The K&D Sessions\01 - Heroes.flac",
            r"MUSIC\Kruder & Dorfmeister\Kruder Dorfmeister - The K&D Sessions\01 - Heroes.flac",
            r"MUSIC\Kruder & Dorfmeister\The K & D Sessions (1998)\01 - Heroes.flac",
            r"MUSIC\Kruder & Dorfmeister\Kruder Dorfmeister - The K&D Sessions\CD1\01 - Heroes.flac",
            r"MUSIC\Kruder & Dorfmeister\1998 K And D Sessions {cd1}\01 - Heroes.flac",
            r"MUSIC\Kruder & Dorfmeister\1998 K And D Sessions {cd2}\01 - Heroes.flac",
        ];
        let results = album_variant_results(&variants);

        let albums = group_artist_results(&results, "Kruder & Dorfmeister");

        assert_eq!(
            albums.len(),
            1,
            "every textual variant of one release must group into a single album, got {:?}",
            albums
                .iter()
                .map(|album| album.album.as_str())
                .collect::<Vec<_>>()
        );
        assert_eq!(albums[0].results.len(), variants.len());
    }

    /// Guard against fixing the duplicate bug by over-merging: genuinely
    /// different releases of the same artist must stay separate albums.
    #[test]
    fn test_group_artist_results_keeps_distinct_releases_apart() {
        let distinct = [
            r"MUSIC\Kruder & Dorfmeister\DJ-Kicks Kruder & Dorfmeister\01 - track.flac",
            r"MUSIC\Kruder & Dorfmeister\Conversions - A K&D Selection\01 - track.flac",
            r"MUSIC\Kruder & Dorfmeister\1995\01 - track.flac",
            r"MUSIC\Kruder & Dorfmeister\The G-Stone Book\01 - track.flac",
            r"MUSIC\Kruder & Dorfmeister\Shakatakadoodub\01 - track.flac",
        ];
        let results = album_variant_results(&distinct);

        let albums = group_artist_results(&results, "Kruder & Dorfmeister");

        assert_eq!(
            albums.len(),
            distinct.len(),
            "different releases must not be merged, got {:?}",
            albums
                .iter()
                .map(|album| album.album.as_str())
                .collect::<Vec<_>>()
        );
    }

    /// A release-year suffix or prefix must not create a second copy of an
    /// album whose title is itself a bare year ("1995" released 2020).
    #[test]
    fn test_group_artist_results_ignores_release_year_around_year_title() {
        let variants = [
            r"MUSIC\Kruder & Dorfmeister\1995\01 - Johnson.flac",
            r"MUSIC\Kruder & Dorfmeister\2020 - 1995\01 - Johnson.flac",
            r"MUSIC\Kruder & Dorfmeister\1995 (2020)\01 - Johnson.flac",
            r"MUSIC\Kruder & Dorfmeister\Kruder & Dorfmeister - 2020 - 1995\01 - Johnson.flac",
        ];
        let results = album_variant_results(&variants);

        let albums = group_artist_results(&results, "Kruder & Dorfmeister");

        assert_eq!(
            albums.len(),
            1,
            "release years around a bare-year title must not split the album, got {:?}",
            albums
                .iter()
                .map(|album| album.album.as_str())
                .collect::<Vec<_>>()
        );
    }

    /// A year that is part of the album title must not be treated as a release
    /// year and stripped, or distinct albums collapse into one download.
    #[test]
    fn test_group_artist_results_keeps_a_year_in_the_title_apart() {
        let distinct = [
            r"MUSIC\Test Artist\Blade Runner 2049\01 - track.flac",
            r"MUSIC\Test Artist\Blade Runner\01 - track.flac",
        ];
        let results = album_variant_results(&distinct);

        let albums = group_artist_results(&results, "Test Artist");

        assert_eq!(
            albums.len(),
            2,
            "a year inside a title must not merge it with the shorter title, got {:?}",
            albums
                .iter()
                .map(|album| album.album.as_str())
                .collect::<Vec<_>>()
        );
    }

    /// Regression built from the exact folder names of the reported run
    /// (`downloads/staging/Kruder & Dorfmeister/`): the seven ways that run
    /// spelled one release must collapse to a single album, while the artist's
    /// other releases stay separate.
    ///
    /// The share name for DJ-Kicks uses a Unicode hyphen (U+2010); it is
    /// written here with an ASCII hyphen because both normalize identically.
    #[test]
    fn test_group_artist_results_collapses_the_reported_staging_folder_names() {
        let folders = [
            "1995",
            "1996 - Conversions Flac",
            "1998 K And D Sessions {cd1}",
            "1998 K And D Sessions {cd2}",
            "2000 - The G-Stone Book",
            "2001 - Dub Sessions USA 2001",
            "2020 - K+D 1995",
            "DJ-Kicks Kruder & Dorfmeister",
            "Kruder & Dorfmeister - 1998 - The K&D Sessions",
            "Kruder & Dorfmeister - The K&D Sessions",
            "Kruder and Dorfmeister - The K&D Sessions™",
            "The K & D Sessions (1998)",
            "[2014] The K&D Sessions",
        ];
        let paths: Vec<String> = folders
            .iter()
            .map(|folder| format!(r"MUSIC\Kruder & Dorfmeister\{folder}\01 - track.flac"))
            .collect();
        let borrowed: Vec<&str> = paths.iter().map(String::as_str).collect();
        let results = album_variant_results(&borrowed);

        let albums = group_artist_results(&results, "Kruder & Dorfmeister");

        let mut grouped: Vec<(String, usize)> = albums
            .iter()
            .map(|album| (album.album.clone(), album.results.len()))
            .collect();
        grouped.sort();

        assert_eq!(
            grouped,
            vec![
                ("1995".to_string(), 2),
                ("1996 - Conversions Flac".to_string(), 1),
                ("1998 K And D Sessions".to_string(), 7),
                ("2000 - The G-Stone Book".to_string(), 1),
                ("2001 - Dub Sessions USA 2001".to_string(), 1),
                ("DJ-Kicks Kruder & Dorfmeister".to_string(), 1),
            ],
            "seven spellings of one release must become a single album of seven results"
        );
    }

    #[test]
    fn test_group_artist_results_collapses_self_titled_variants() {
        let variants = [
            r"MUSIC\Kruder & Dorfmeister\Kruder & Dorfmeister\01.flac",
            r"MUSIC\Kruder & Dorfmeister\Kruder and Dorfmeister\01.flac",
            r"MUSIC\Kruder & Dorfmeister\[1998] Kruder & Dorfmeister\01.flac",
        ];
        let albums =
            group_artist_results(&album_variant_results(&variants), "Kruder & Dorfmeister");
        assert_eq!(albums.len(), 1, "got {albums:?}");
    }

    #[test]
    fn test_group_artist_results_strips_year_before_artist_prefix() {
        let variants = [
            r"MUSIC\Kruder & Dorfmeister\1998 Kruder & Dorfmeister - The K&D Sessions\01.flac",
            r"MUSIC\Kruder & Dorfmeister\The K&D Sessions\01.flac",
        ];
        let albums =
            group_artist_results(&album_variant_results(&variants), "Kruder & Dorfmeister");
        assert_eq!(albums.len(), 1, "got {albums:?}");
    }

    #[test]
    fn test_group_artist_results_recognises_artist_initials_before_year_title() {
        let variants = [
            r"MUSIC\Kruder & Dorfmeister\1995\01.flac",
            r"MUSIC\Kruder & Dorfmeister\2020 - K+D 1995\01.flac",
        ];
        let albums =
            group_artist_results(&album_variant_results(&variants), "Kruder & Dorfmeister");
        assert_eq!(albums.len(), 1, "got {albums:?}");
    }

    #[test]
    fn test_group_artist_results_folds_accents_consistently() {
        let variants = [
            r"MUSIC\Test Artist\Café del Mar\01.flac",
            r"MUSIC\Test Artist\Cafe del Mar\01.flac",
        ];
        let albums = group_artist_results(&album_variant_results(&variants), "Test Artist");
        assert_eq!(albums.len(), 1, "got {albums:?}");
    }

    #[test]
    fn test_group_artist_results_does_not_strip_letters_tm() {
        let distinct = [
            r"MUSIC\Test Artist\Greatest ATM\01.flac",
            r"MUSIC\Test Artist\Greatest A\01.flac",
        ];
        let albums = group_artist_results(&album_variant_results(&distinct), "Test Artist");
        assert_eq!(albums.len(), 2, "got {albums:?}");
    }

    #[test]
    fn test_group_artist_results_strips_single_word_artist_with_explicit_separator() {
        let variants = [
            r"MUSIC\Nirvana\Nirvana - Nevermind\01.flac",
            r"MUSIC\Nirvana\Nevermind\01.flac",
        ];
        let albums = group_artist_results(&album_variant_results(&variants), "Nirvana");
        assert_eq!(albums.len(), 1, "got {albums:?}");
    }

    #[test]
    fn test_group_artist_results_does_not_strip_partial_single_word_artist() {
        let distinct = [
            r"MUSIC\The Doors\Doors Open\01.flac",
            r"MUSIC\The Doors\Open\01.flac",
        ];
        let albums = group_artist_results(&album_variant_results(&distinct), "The Doors");
        assert_eq!(albums.len(), 2, "got {albums:?}");
    }

    #[test]
    fn test_album_identity_never_merges_distinct_non_ascii_titles() {
        assert_ne!(
            album_identity_key("Группа крови", "Kino"),
            album_identity_key("Чёрный альбом", "Kino")
        );
    }

    #[test]
    fn test_album_identity_keeps_non_ascii_titles_with_shared_suffix_distinct() {
        assert_ne!(
            album_identity_key("Группа крови (Live)", "Kino"),
            album_identity_key("Чёрный альбом (Live)", "Kino")
        );
    }

    #[test]
    fn test_album_identity_normalizes_years_around_non_ascii_titles() {
        assert_eq!(
            album_identity_key("1998 Чёрный альбом", "Кино"),
            album_identity_key("Чёрный альбом (1998)", "Кино")
        );
    }

    #[test]
    fn test_album_identity_strips_explicit_format_suffixes() {
        assert_eq!(
            album_identity_key("The K&D Sessions [FLAC]", "Kruder & Dorfmeister"),
            album_identity_key("The K&D Sessions", "Kruder & Dorfmeister")
        );
    }

    #[test]
    fn test_album_identity_normalizes_punctuation_and_special_latin_letters() {
        assert_eq!(
            album_identity_key("Sessions: Remixed", "Artist"),
            album_identity_key("Sessions - Remixed", "Artist")
        );
        assert_eq!(
            album_identity_key("Ænima", "Artist"),
            album_identity_key("Aenima", "Artist")
        );
        assert_eq!(
            album_identity_key("Weiße", "Artist"),
            album_identity_key("Weisse", "Artist")
        );
    }

    #[test]
    fn test_artist_identity_is_never_empty_for_article_only_names() {
        assert_ne!(artist_identity_key("A"), artist_identity_key("The The"));
    }

    #[test]
    fn test_album_identity_preserves_year_as_part_of_title() {
        assert_ne!(
            album_identity_key("2001 - A Space Odyssey", "Artist"),
            album_identity_key("Space Odyssey", "Artist")
        );
    }

    #[test]
    fn test_album_identity_strips_historical_brace_disc_markers() {
        assert_eq!(
            album_identity_key("1998 K And D Sessions {cd1}", "Kruder & Dorfmeister"),
            album_identity_key("The K&D Sessions", "Kruder & Dorfmeister")
        );
    }

    #[test]
    fn test_album_identity_keeps_narrow_year_boundaries() {
        assert_ne!(
            album_identity_key("Album [Remastered 1998]", "Artist"),
            album_identity_key("Album", "Artist")
        );
        assert_ne!(
            album_identity_key("1812 Overture", "Artist"),
            album_identity_key("Overture", "Artist")
        );
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
        assert!(!path_matches_artist(
            r"Music\Princess\Album\01 - track.flac",
            "Prince"
        ));
    }

    #[test]
    fn test_path_matches_artist_empty_artist_returns_false() {
        assert!(!path_matches_artist("anything.flac", ""));
        assert!(!path_matches_artist(r"Music\Whatever\01.flac", "   "));
        assert!(!path_matches_artist(r"Music\Whatever\01.flac", "!!!"));
    }

    #[tokio::test]
    async fn test_album_only_fallback_never_matches_when_artist_empty() {
        // An empty artist can never pass path_matches_artist. The primary
        // album-only query is sufficient, so the redundant album-only fallback
        // tier must be skipped when the artist is empty.
        let client = MockClient::new();
        let outcome =
            search_album_with_fallback(&client, "", Some("Album"), 15, &test_filters(), None)
                .await
                .unwrap();
        assert!(outcome.results.is_empty());
        assert_eq!(
            client.search_queries.lock().unwrap().clone(),
            vec!["Album".to_string()]
        );
    }

    #[tokio::test]
    async fn test_no_search_when_album_blank() {
        let client = MockClient::new();
        let _outcome =
            search_album_with_fallback(&client, "Artist", Some(" "), 15, &test_filters(), None)
                .await
                .unwrap();
        assert_eq!(client.search_queries.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn test_primary_empty_album_only_fallback_returns_match() {
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
        // The primary query "Michael Jackson History" has no map entry, so
        // it returns empty, as does the lowercase fallback
        // "michael jackson history". The album-only tier then searches
        // "History", whose result path matches "Michael Jackson", so it is
        // returned.

        let outcome = search_album_with_fallback(
            &client,
            "Michael Jackson",
            Some("History"),
            15,
            &test_filters(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(outcome.results.len(), 1);
        let queries = client.search_queries.lock().unwrap().clone();
        assert_eq!(
            queries,
            vec![
                "Michael Jackson History".to_string(),
                "michael jackson history".to_string(),
                "History".to_string()
            ]
        );
    }

    #[tokio::test]
    async fn test_no_fallback_when_primary_non_empty() {
        let client = MockClient::new();
        // The primary tier's result must survive the full filter pipeline
        // (including the album-name gate), so the cascade returns it without
        // issuing any fallback queries.
        *client.search_results.lock().unwrap() = vec![SearchResult {
            username: "user1".into(),
            speed: 500,
            slots: 1,
            files: vec![make_file("Artist/Album/01 - Track.flac", 900, 30_000_000)],
        }];

        let outcome =
            search_album_with_fallback(&client, "Artist", Some("Album"), 15, &test_filters(), None)
                .await
                .unwrap();
        assert_eq!(outcome.results.len(), 1);
        let queries = client.search_queries.lock().unwrap().clone();
        assert_eq!(queries, vec!["Artist Album".to_string()]);
    }

    #[tokio::test]
    async fn positive_queue_cap_stops_search_fallback() {
        let client = MockClient::new();
        // A zero-slot primary result is usable when a positive queue cap is
        // configured (the download step validates the real queue position),
        // so the cascade must stop at the primary tier instead of running
        // the lowercase/album-only fallbacks.
        *client.search_results.lock().unwrap() = vec![SearchResult {
            username: "queued-peer".into(),
            speed: 500,
            slots: 0,
            files: vec![make_file("Artist/Album/01 - Track.flac", 900, 30_000_000)],
        }];

        let outcome = search_album_with_fallback_with_queue_limit(
            &client,
            "Artist",
            Some("Album"),
            15,
            &test_filters(),
            None,
            3,
        )
        .await
        .unwrap();
        assert_eq!(outcome.results.len(), 1);
        assert_eq!(
            client.search_queries.lock().unwrap().as_slice(),
            ["Artist Album"]
        );
    }

    #[tokio::test]
    async fn test_artist_album_search_prunes_wrong_artist_files() {
        let client = MockClient::new();
        *client.search_results.lock().unwrap() = vec![SearchResult {
            username: "peer1".into(),
            speed: 500,
            slots: 1,
            files: vec![
                make_file(r"Artist\Album\01 - right.flac", 900, 30_000_000),
                make_file(r"Other Artist\Album\01 - wrong.flac", 900, 30_000_000),
            ],
        }];

        let outcome =
            search_album_with_fallback(&client, "Artist", Some("Album"), 15, &test_filters(), None)
                .await
                .unwrap();
        assert_eq!(outcome.results.len(), 1);
        assert_eq!(outcome.results[0].files.len(), 1);
        assert!(outcome.results[0].files[0].name.contains("right"));
    }

    #[tokio::test]
    async fn test_all_tiers_run_when_primary_empty_and_album_present() {
        // All tiers run when the primary search is empty and an album is
        // present: the primary "Artist Album", the lowercase fallback
        // "artist album", and the album-only "Album" query. With no results
        // for any of them, the outcome is empty.
        let client = MockClient::new();
        let outcome =
            search_album_with_fallback(&client, "Artist", Some("Album"), 15, &test_filters(), None)
                .await
                .unwrap();
        assert!(outcome.results.is_empty());
        assert_eq!(client.search_queries.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn test_single_query_when_no_album() {
        let client = MockClient::new();
        let _outcome =
            search_album_with_fallback(&client, "Artist", None, 15, &test_filters(), None)
                .await
                .unwrap();
        assert_eq!(client.search_queries.lock().unwrap().len(), 1);
    }

    // ── clean_track_title ──

    #[test]
    fn test_clean_track_title_strips_extension_and_leading_track_number() {
        assert_eq!(clean_track_title("03. I Miss You.mp3"), "i miss you");
        assert_eq!(clean_track_title("01 - Hello.flac"), "hello");
        assert_eq!(clean_track_title("12- Bye.mp3"), "bye");
        assert_eq!(clean_track_title("7.On The Floor.mp3"), "on the floor");
    }

    // ── normalize_search_term ──

    #[test]
    fn test_normalize_search_term_joins_periods() {
        assert_eq!(normalize_search_term("S.P.Y."), "SPY");
    }

    #[test]
    fn test_normalize_search_term_full_album() {
        assert_eq!(
            normalize_search_term("S.P.Y. - In The Skys"),
            "SPY In The Skys"
        );
    }

    #[test]
    fn test_normalize_search_term_ampersand_to_and() {
        assert_eq!(normalize_search_term("Guns & Roses"), "Guns and Roses");
    }

    #[test]
    fn test_normalize_search_term_plus_to_and() {
        assert_eq!(normalize_search_term("A+B"), "A and B");
    }

    #[test]
    fn test_normalize_search_term_empty_output() {
        // Pure punctuation and pure non-ASCII inputs normalise to nothing.
        assert_eq!(normalize_search_term("..."), "");
        assert_eq!(normalize_search_term("   "), "");
        assert_eq!(normalize_search_term("周杰倫"), "");
    }

    #[test]
    fn test_normalize_search_term_non_ascii_whitespace_and_dash() {
        // Non-ASCII whitespace (NEL U+0085, NBSP U+00A0) must separate tokens.
        assert_eq!(normalize_search_term("A\u{0085}B"), "A B");
        assert_eq!(
            normalize_search_term("Guns\u{00A0}N\u{00A0}Roses"),
            "Guns N Roses"
        );
        // Smart dashes and middot must separate, not fuse.
        assert_eq!(normalize_search_term("Skys\u{2014}Vol 2"), "Skys Vol 2");
        assert_eq!(normalize_search_term("A\u{00B7}B"), "A B");
    }

    #[test]
    fn test_normalize_search_term_separators() {
        assert_eq!(normalize_search_term("AC-DC"), "AC DC");
        assert_eq!(normalize_search_term("AC/DC"), "AC DC");
    }

    #[test]
    fn test_normalize_search_term_brackets_and_underscore() {
        assert_eq!(
            normalize_search_term("In_The-Skys (Deluxe)"),
            "In The Skys Deluxe"
        );
    }

    #[test]
    fn test_normalize_search_term_apostrophe_joins() {
        assert_eq!(normalize_search_term("D'Angelo"), "DAngelo");
    }

    #[test]
    fn test_normalize_search_term_accent_fold() {
        assert_eq!(normalize_search_term("Tiësto"), "Tiesto");
        assert_eq!(normalize_search_term("Café"), "Cafe");
    }

    #[test]
    fn test_normalize_search_term_preserves_case() {
        assert_eq!(normalize_search_term("Spy In The Skys"), "Spy In The Skys");
    }

    #[test]
    fn test_normalize_search_term_no_change_passthrough() {
        assert_eq!(normalize_search_term("Musicology"), "Musicology");
    }

    #[test]
    fn test_normalize_search_term_collapses_whitespace() {
        assert_eq!(
            normalize_search_term("  Guns   &   Roses  "),
            "Guns and Roses"
        );
    }

    // ── fallback_track_query ──

    #[test]
    fn test_fallback_track_query_excludes_artist_name() {
        // Regression: the fallback track search must NOT include the artist
        // name, because Soulseek blocks certain artists from being found.
        // Searching "prince musicology..." is just as blocked as searching
        // "prince musicology". The query must be the track name only.
        let q = fallback_track_query("Prince_-_Musicology_2004_01_Musicology.mp3", "Prince");
        assert!(
            !q.to_lowercase().contains("prince"),
            "fallback query must not contain the artist name, got: {q}"
        );
        assert!(!q.is_empty(), "fallback query must not be empty");
        // The known track title should survive as the searchable token.
        assert!(q.to_lowercase().contains("musicology"), "got: {q}");
    }

    #[test]
    fn test_fallback_track_query_uses_clean_track_title() {
        // Simple filename where the artist is not embedded: just the track
        // title survives the clean, and the query equals that title.
        let q = fallback_track_query("01 - Musicology.mp3", "Prince");
        assert_eq!(q, "musicology");
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
        let results = search_by_title(&client, &[], "", 15, 100).await.unwrap();
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
        let results = search_by_title(&client, &library, "", 15, 100)
            .await
            .unwrap();
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
        let results = search_by_title(&client, &library, "", 15, 50)
            .await
            .unwrap();
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

        let results = search_by_title(&client, &library, "", 15, 100)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].files.len(), 1);
        assert_eq!(
            results[0].files[0].name,
            r"Music\Artist\Album\01 - Cafés.mp3"
        );
    }

    // Regression: real Soulseek peer filenames usually embed the artist
    // name (e.g. "Gorillaz - Tomorrow Comes Today.mp3"), which after
    // clean_track_title becomes "gorillaz tomorrow comes today" — a
    // SUPERSET of the library title "tomorrow comes today". Exact-equality
    // matching (library_titles.contains(&title)) rejects these, so the
    // title-search fallback returned 0 results even though the underlying
    // Soulseek search finds plenty of matches. Matching must use substring
    // containment: a library title matches when it appears within the
    // cleaned peer filename.
    #[tokio::test]
    async fn test_search_by_title_matches_peer_filename_with_artist_prefix() {
        let client = MockClient::new();
        let library = vec!["01 - Tomorrow Comes Today.mp3".to_string()];
        client.search_results_by_query.lock().unwrap().insert(
            "tomorrow comes today".into(),
            vec![SearchResult {
                username: "user1".into(),
                speed: 500,
                slots: 1,
                files: vec![
                    // Artist-embedded peer filename — the common real case.
                    make_file("Gorillaz - Tomorrow Comes Today.mp3", 900, 10_000_000),
                ],
            }],
        );

        let results = search_by_title(&client, &library, "Gorillaz", 15, 100)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].username, "user1");
        assert_eq!(results[0].files.len(), 1);
        assert_eq!(
            results[0].files[0].name,
            "Gorillaz - Tomorrow Comes Today.mp3"
        );
    }

    // ── lib_title_matches (short-title false-positive guard) ──

    #[test]
    fn test_lib_title_matches_guards_against_short_title_false_positives() {
        // Multi-word titles match as a substring (peer files embed extra
        // artist/album metadata).
        assert!(lib_title_matches(
            "gorillaz tomorrow comes today",
            "tomorrow comes today"
        ));
        // Single long-enough word: matches at a word boundary...
        assert!(lib_title_matches("man on fire", "fire"));
        assert!(lib_title_matches("fire", "fire"));
        // ...but NOT inside a longer word ("fire" in "firefly").
        assert!(!lib_title_matches("firefly", "fire"));
        assert!(!lib_title_matches("someone", "one"));
        // Single words shorter than 4 chars are rejected outright as too
        // ambiguous, even at a word boundary ("in" as a track title).
        assert!(!lib_title_matches("love is in the air", "in"));
        assert!(!lib_title_matches("in", "in"));
    }

    // ── is_generic_track_name ──

    #[test]
    fn test_is_generic_track_name_cd_track() {
        // "CD Track N" / "CD TrackN" (with or without space) are generic.
        assert!(is_generic_track_name("CD Track 1.mp3"));
        assert!(is_generic_track_name("CD Track 01.flac"));
        assert!(is_generic_track_name("CD Track1.mp3"));
        assert!(is_generic_track_name("cd track 5.mp3"));
    }

    #[test]
    fn test_is_generic_track_name_track() {
        // Bare "Track N" / "TrackN" are generic.
        assert!(is_generic_track_name("Track 1.mp3"));
        assert!(is_generic_track_name("Track1.mp3"));
        assert!(is_generic_track_name("track 5.mp3"));
    }

    #[test]
    fn test_is_generic_track_name_other_patterns() {
        // Other meaningless names: untitled, unknown, audio/recording N, and
        // bare numbers.
        assert!(is_generic_track_name("Untitled.mp3"));
        assert!(is_generic_track_name("Unknown.flac"));
        assert!(is_generic_track_name("Audio 1.mp3"));
        assert!(is_generic_track_name("Recording 5.flac"));
        assert!(is_generic_track_name("01.mp3"));
        assert!(is_generic_track_name("42.flac"));
    }

    #[test]
    fn test_is_generic_track_name_non_generic() {
        // Real titles must never be flagged as generic.
        assert!(!is_generic_track_name("Tomorrow Comes Today.mp3"));
        assert!(!is_generic_track_name("Musicology.flac"));
        assert!(!is_generic_track_name("01 - Hello.mp3"));
        assert!(!is_generic_track_name("Cafés.flac"));
        assert!(!is_generic_track_name("I Miss You.mp3"));
    }

    // ── album-only fallback ──

    #[tokio::test]
    async fn test_album_only_fallback_when_primary_empty() {
        let client = MockClient::new();
        // Primary "Prince Musicology" has no map entry -> empty (blocked
        // artist), as does the lowercase fallback "prince musicology".
        // Album-only "Musicology" returns a result whose file path contains
        // "Prince".
        client.search_results_by_query.lock().unwrap().insert(
            "Musicology".into(),
            vec![SearchResult {
                username: "peer1".into(),
                speed: 500,
                slots: 1,
                files: vec![make_file(
                    "Prince/Musicology/01 - Musicology.flac",
                    900,
                    30_000_000,
                )],
            }],
        );

        let outcome = search_album_with_fallback(
            &client,
            "Prince",
            Some("Musicology"),
            15,
            &test_filters(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(outcome.results.len(), 1);
        // All three tiers ran in order. The album-only query is "Musicology"
        // — not " Musicology" (leading space must be trimmed).
        let queries = client.search_queries.lock().unwrap().clone();
        assert_eq!(
            queries,
            vec![
                "Prince Musicology".to_string(),
                "prince musicology".to_string(),
                "Musicology".to_string()
            ]
        );
    }

    #[tokio::test]
    async fn test_album_only_fallback_filters_by_artist() {
        let client = MockClient::new();
        // Album-only "Musicology" returns a result whose path has the WRONG
        // artist — it must be filtered out by the artist check.
        client.search_results_by_query.lock().unwrap().insert(
            "Musicology".into(),
            vec![SearchResult {
                username: "peer1".into(),
                speed: 500,
                slots: 1,
                files: vec![make_file(
                    "Some Other Artist/Musicology/01 - Track.flac",
                    900,
                    30_000_000,
                )],
            }],
        );

        let outcome = search_album_with_fallback(
            &client,
            "Prince",
            Some("Musicology"),
            15,
            &test_filters(),
            None,
        )
        .await
        .unwrap();
        assert!(outcome.results.is_empty());
    }

    #[tokio::test]
    async fn test_album_only_fallback_skips_when_album_empty() {
        let client = MockClient::new();
        // No search results -> primary comes up empty. Album is None, so the
        // album-only tier is skipped -> still empty and only 1 query issued.
        let outcome =
            search_album_with_fallback(&client, "Prince", None, 15, &test_filters(), None)
                .await
                .unwrap();
        assert!(outcome.results.is_empty());
        let queries = client.search_queries.lock().unwrap().clone();
        assert_eq!(queries, vec!["Prince".to_string()]);
    }

    // ── lowercase casing fallback ──

    #[tokio::test]
    async fn test_lowercase_fallback_when_primary_empty() {
        let client = MockClient::new();
        // Primary "Prince Musicology" has no map entry -> empty (blocked
        // artist). Lowercase "prince musicology" returns results.
        client.search_results_by_query.lock().unwrap().insert(
            "prince musicology".into(),
            vec![SearchResult {
                username: "peer1".into(),
                speed: 500,
                slots: 1,
                files: vec![make_file(
                    "Prince/Musicology/01 - Musicology.flac",
                    900,
                    30_000_000,
                )],
            }],
        );

        let outcome = search_album_with_fallback(
            &client,
            "Prince",
            Some("Musicology"),
            15,
            &test_filters(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(outcome.results.len(), 1);
        // Both tiers ran: original casing + lowercase fallback.
        let queries = client.search_queries.lock().unwrap().clone();
        assert_eq!(
            queries,
            vec![
                "Prince Musicology".to_string(),
                "prince musicology".to_string()
            ]
        );
    }

    #[tokio::test]
    async fn test_lowercase_fallback_skipped_when_primary_has_results() {
        let client = MockClient::new();
        // Primary "Prince Musicology" returns results. Lowercase fallback
        // must NOT be attempted.
        client.search_results_by_query.lock().unwrap().insert(
            "Prince Musicology".into(),
            vec![SearchResult {
                username: "peer1".into(),
                speed: 500,
                slots: 1,
                files: vec![make_file(
                    "Prince/Musicology/01 - Musicology.flac",
                    900,
                    30_000_000,
                )],
            }],
        );

        let outcome = search_album_with_fallback(
            &client,
            "Prince",
            Some("Musicology"),
            15,
            &test_filters(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(outcome.results.len(), 1);
        // Only the original casing query was issued — no lowercase fallback.
        let queries = client.search_queries.lock().unwrap().clone();
        assert_eq!(queries, vec!["Prince Musicology".to_string()]);
    }

    #[tokio::test]
    async fn test_album_only_tier_after_both_casings_fail() {
        let client = MockClient::new();
        // Both casing variants return nothing. Album-only "Musicology"
        // returns a result whose path matches "Prince".
        client.search_results_by_query.lock().unwrap().insert(
            "Musicology".into(),
            vec![SearchResult {
                username: "peer1".into(),
                speed: 500,
                slots: 1,
                files: vec![make_file(
                    "Prince/Musicology/01 - Musicology.flac",
                    900,
                    30_000_000,
                )],
            }],
        );

        let outcome = search_album_with_fallback(
            &client,
            "Prince",
            Some("Musicology"),
            15,
            &test_filters(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(outcome.results.len(), 1);
        // Three queries ran: original casing, lowercase, album-only.
        let queries = client.search_queries.lock().unwrap().clone();
        assert_eq!(
            queries,
            vec![
                "Prince Musicology".to_string(),
                "prince musicology".to_string(),
                "Musicology".to_string()
            ]
        );
    }

    #[tokio::test]
    async fn test_lowercase_fallback_skipped_when_already_lowercase() {
        let client = MockClient::new();
        // Both artist and album are already lowercase. Tier 1a
        // ("prince musicology") returns empty. Tier 1b must be skipped
        // because the lowercased query would be byte-identical to the
        // Tier 1a query (no new information). Tier 2 (album-only
        // "musicology") returns a result whose path matches "Prince".
        // Without the skip, Tier 1b would issue a duplicate "prince
        // musicology" query (3 total). With the skip, only 2 queries run.
        client.search_results_by_query.lock().unwrap().insert(
            "musicology".into(),
            vec![SearchResult {
                username: "peer1".into(),
                speed: 500,
                slots: 1,
                files: vec![make_file(
                    "Prince/Musicology/01 - Musicology.flac",
                    900,
                    30_000_000,
                )],
            }],
        );

        let outcome = search_album_with_fallback(
            &client,
            "prince",
            Some("musicology"),
            15,
            &test_filters(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(outcome.results.len(), 1);
        // Two queries ran: Tier 1a ("prince musicology") and Tier 2
        // ("musicology"). Tier 1b was skipped because lowercasing
        // would produce the same query as Tier 1a.
        let queries = client.search_queries.lock().unwrap().clone();
        assert_eq!(
            queries,
            vec!["prince musicology".to_string(), "musicology".to_string()]
        );
    }

    // ── punctuation fallback ──

    #[tokio::test]
    async fn test_punctuation_fallback_fires_when_normalisation_changes() {
        let client = MockClient::new();
        // Tier 1a "S.P.Y. In The Skys" and Tier 1b "s.p.y. in the skys" have
        // no map entries -> empty. Tier 1c normalises to "SPY In The Skys".
        client.search_results_by_query.lock().unwrap().insert(
            "SPY In The Skys".into(),
            vec![SearchResult {
                username: "peer1".into(),
                speed: 500,
                slots: 1,
                files: vec![make_file(
                    "SPY/In The Skys/01 - Track.flac",
                    900,
                    30_000_000,
                )],
            }],
        );

        let outcome = search_album_with_fallback(
            &client,
            "S.P.Y.",
            Some("In The Skys"),
            15,
            &test_filters(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(outcome.results.len(), 1);
        assert_eq!(outcome.results[0].username, "peer1");
        let queries = client.search_queries.lock().unwrap().clone();
        assert_eq!(
            queries,
            vec![
                "S.P.Y. In The Skys".to_string(),
                "s.p.y. in the skys".to_string(),
                "SPY In The Skys".to_string()
            ]
        );
    }

    #[tokio::test]
    async fn test_punctuation_fallback_skipped_when_normalisation_empty() {
        let client = MockClient::new();
        // A punctuation-only artist normalises to "". Tier 1c must be
        // skipped (an empty term would duplicate Tier 2's album-only query
        // without artist verification), so only 3 queries run: 1a, 1b, 2.
        let outcome = search_album_with_fallback(
            &client,
            "...",
            Some("Musicology"),
            15,
            &test_filters(),
            None,
        )
        .await
        .unwrap();
        assert!(outcome.results.is_empty());
        let queries = client.search_queries.lock().unwrap().clone();
        assert_eq!(
            queries,
            vec![
                "... Musicology".to_string(),
                "... musicology".to_string(),
                "Musicology".to_string()
            ]
        );
    }

    #[tokio::test]
    async fn test_punctuation_fallback_skipped_when_only_whitespace_differs() {
        let client = MockClient::new();
        // Artist has a double space; normalisation only collapses whitespace,
        // so the query would be token-identical to Tier 1a. Tier 1c must skip
        // (3 queries: 1a, 1b, 2).
        let outcome = search_album_with_fallback(
            &client,
            "Guns  N  Roses",
            Some("Spaghetti"),
            15,
            &test_filters(),
            None,
        )
        .await
        .unwrap();
        assert!(outcome.results.is_empty());
        let queries = client.search_queries.lock().unwrap().clone();
        assert_eq!(
            queries,
            vec![
                "Guns  N  Roses Spaghetti".to_string(),
                "guns  n  roses spaghetti".to_string(),
                "Spaghetti".to_string()
            ]
        );
    }

    #[tokio::test]
    async fn test_punctuation_fallback_album_only_punctuation() {
        let client = MockClient::new();
        // Artist is clean, album has a hyphen. Tier 1c fires with the
        // normalised album after 1a/1b come up empty.
        client.search_results_by_query.lock().unwrap().insert(
            "Prince Music ology".into(),
            vec![SearchResult {
                username: "peer1".into(),
                speed: 500,
                slots: 1,
                files: vec![make_file("Prince/Music ology/01.flac", 900, 30_000_000)],
            }],
        );
        let outcome = search_album_with_fallback(
            &client,
            "Prince",
            Some("Music-ology"),
            15,
            &test_filters(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(outcome.results.len(), 1);
        let queries = client.search_queries.lock().unwrap().clone();
        assert_eq!(
            queries,
            vec![
                "Prince Music-ology".to_string(),
                "prince music-ology".to_string(),
                "Prince Music ology".to_string()
            ]
        );
    }

    #[tokio::test]
    async fn test_punctuation_fallback_skipped_when_album_normalises_empty() {
        let client = MockClient::new();
        // Album is pure non-ASCII, normalising to "". Tier 1c must skip (an
        // empty album term would duplicate Tier 2's album-only query).
        let outcome = search_album_with_fallback(
            &client,
            "Prince",
            Some("周杰倫"),
            15,
            &test_filters(),
            None,
        )
        .await
        .unwrap();
        assert!(outcome.results.is_empty());
        let queries = client.search_queries.lock().unwrap().clone();
        assert_eq!(
            queries,
            vec![
                "Prince 周杰倫".to_string(),
                "prince 周杰倫".to_string(),
                "周杰倫".to_string()
            ]
        );
    }

    #[tokio::test]
    async fn test_punctuation_fallback_falls_through_to_album_only() {
        let client = MockClient::new();
        // Tier 1c ("SPY In The Skys") returns nothing; Tier 2 ("In The Skys")
        // finds an artist-verified result. Verify 1c fires first, then 2.
        client.search_results_by_query.lock().unwrap().insert(
            "In The Skys".into(),
            vec![SearchResult {
                username: "peer1".into(),
                speed: 500,
                slots: 1,
                files: vec![make_file(
                    "SPY/In The Skys/01 - Track.flac",
                    900,
                    30_000_000,
                )],
            }],
        );
        let outcome = search_album_with_fallback(
            &client,
            "S.P.Y.",
            Some("In The Skys"),
            15,
            &test_filters(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(outcome.results.len(), 1);
        let queries = client.search_queries.lock().unwrap().clone();
        assert_eq!(
            queries,
            vec![
                "S.P.Y. In The Skys".to_string(),
                "s.p.y. in the skys".to_string(),
                "SPY In The Skys".to_string(),
                "In The Skys".to_string()
            ]
        );
    }

    #[tokio::test]
    async fn test_punctuation_fallback_skipped_when_no_punctuation() {
        let client = MockClient::new();
        // "Prince" + "Musicology" has no punctuation: Tier 1c normalisation
        // is a no-op and must be skipped. Tier 1a, 1b, and album-only run.
        let outcome = search_album_with_fallback(
            &client,
            "Prince",
            Some("Musicology"),
            15,
            &test_filters(),
            None,
        )
        .await
        .unwrap();
        assert!(outcome.results.is_empty());
        let queries = client.search_queries.lock().unwrap().clone();
        assert_eq!(
            queries,
            vec![
                "Prince Musicology".to_string(),
                "prince musicology".to_string(),
                "Musicology".to_string()
            ]
        );
    }

    #[tokio::test]
    async fn test_punctuation_fallback_ampersand() {
        let client = MockClient::new();
        client.search_results_by_query.lock().unwrap().insert(
            "Guns and Roses Appetite for Destruction".into(),
            vec![SearchResult {
                username: "peer1".into(),
                speed: 500,
                slots: 1,
                files: vec![make_file(
                    "Guns N Roses/Appetite for Destruction/01.flac",
                    900,
                    30_000_000,
                )],
            }],
        );

        let outcome = search_album_with_fallback(
            &client,
            "Guns & Roses",
            Some("Appetite for Destruction"),
            15,
            &test_filters(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(outcome.results.len(), 1);
        let queries = client.search_queries.lock().unwrap().clone();
        assert_eq!(
            queries,
            vec![
                "Guns & Roses Appetite for Destruction".to_string(),
                "guns & roses appetite for destruction".to_string(),
                "Guns and Roses Appetite for Destruction".to_string()
            ]
        );
    }

    // ── filter-aware cascade continuation ──

    #[tokio::test]
    async fn test_cascade_continues_when_primary_only_mp3() {
        let client = MockClient::new();
        // Tier 1a returns only mp3 (rejected by the flac filter) -> cascade
        // continues. Tier 1c returns flac and wins.
        client.search_results_by_query.lock().unwrap().insert(
            "S.P.Y. In The Skys".into(),
            vec![SearchResult {
                username: "mp3peer".into(),
                speed: 500,
                slots: 1,
                files: vec![make_file("S.P.Y./In The Skys/01.mp3", 320, 8_000_000)],
            }],
        );
        client.search_results_by_query.lock().unwrap().insert(
            "SPY In The Skys".into(),
            vec![SearchResult {
                username: "flacpeer".into(),
                speed: 500,
                slots: 1,
                files: vec![make_file(
                    "SPY/In The Skys/01 - Track.flac",
                    900,
                    30_000_000,
                )],
            }],
        );

        let outcome = search_album_with_fallback(
            &client,
            "S.P.Y.",
            Some("In The Skys"),
            15,
            &test_filters(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(outcome.results.len(), 1);
        assert_eq!(outcome.results[0].username, "flacpeer");
        let queries = client.search_queries.lock().unwrap().clone();
        assert_eq!(
            queries,
            vec![
                "S.P.Y. In The Skys".to_string(),
                "s.p.y. in the skys".to_string(),
                "SPY In The Skys".to_string()
            ]
        );
    }

    #[tokio::test]
    async fn test_cascade_fallback_from_album_only_tier() {
        let client = MockClient::new();
        // 1a/1b empty; 1c skipped (no punctuation). Tier 2 returns an artist-
        // matching but all-mp3 result -> rejected by the flac probe. The
        // cascade returns Tier 2's artist-pruned result as the fallback.
        client.search_results_by_query.lock().unwrap().insert(
            "Musicology".into(),
            vec![SearchResult {
                username: "mp3peer".into(),
                speed: 500,
                slots: 1,
                files: vec![
                    make_file("Prince/Musicology/01.mp3", 320, 8_000_000),
                    // Wrong-artist file that path_matches_artist prunes.
                    make_file("Other/Musicology/02.mp3", 320, 8_000_000),
                ],
            }],
        );

        let outcome = search_album_with_fallback(
            &client,
            "Prince",
            Some("Musicology"),
            15,
            &test_filters(),
            None,
        )
        .await
        .unwrap();
        // Fallback = Tier 2's artist-pruned results (1 result, 1 file).
        assert_eq!(outcome.results.len(), 1);
        assert_eq!(outcome.results[0].files.len(), 1);
        assert!(outcome.results[0].files[0].name.contains("Prince"));
        let queries = client.search_queries.lock().unwrap().clone();
        assert_eq!(
            queries,
            vec![
                "Prince Musicology".to_string(),
                "prince musicology".to_string(),
                "Musicology".to_string()
            ]
        );
    }

    #[tokio::test]
    async fn test_cascade_returns_first_tier_when_all_tiers_junk() {
        let client = MockClient::new();
        // Every tier returns only mp3 -> no tier passes the flac filter. The
        // cascade must return the FIRST tier's results (today's behaviour)
        // and still run all tiers.
        let mp3 = |path: &str| {
            vec![SearchResult {
                username: "mp3peer".into(),
                speed: 500,
                slots: 1,
                files: vec![make_file(path, 320, 8_000_000)],
            }]
        };
        client
            .search_results_by_query
            .lock()
            .unwrap()
            .insert("Prince Musicology".into(), mp3("Prince/Musicology/01.mp3"));
        client
            .search_results_by_query
            .lock()
            .unwrap()
            .insert("prince musicology".into(), mp3("Prince/Musicology/01.mp3"));
        client
            .search_results_by_query
            .lock()
            .unwrap()
            .insert("Musicology".into(), mp3("Prince/Musicology/01.mp3"));

        let outcome = search_album_with_fallback(
            &client,
            "Prince",
            Some("Musicology"),
            15,
            &test_filters(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(outcome.results.len(), 1);
        assert_eq!(outcome.results[0].username, "mp3peer");
        let queries = client.search_queries.lock().unwrap().clone();
        assert_eq!(
            queries,
            vec![
                "Prince Musicology".to_string(),
                "prince musicology".to_string(),
                "Musicology".to_string()
            ]
        );
    }

    // ── generic-name filtering in search_by_title ──

    #[tokio::test]
    async fn test_search_by_title_skips_generic_names() {
        let client = MockClient::new();
        let library = vec!["CD Track 1.mp3".to_string(), "CD Track 2.mp3".to_string()];
        let results = search_by_title(&client, &library, "", 15, 100)
            .await
            .unwrap();
        assert!(results.is_empty());
        // No query may be issued for all-generic libraries.
        assert!(client.search_queries.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_search_by_title_uses_non_generic_names() {
        let client = MockClient::new();
        // Mixed library: a generic "CD Track 1" plus a real "01 - Musicology".
        // The real track must drive the query ("musicology"), not a generic.
        let library = vec![
            "CD Track 1.mp3".to_string(),
            "01 - Musicology.mp3".to_string(),
        ];
        client.search_results_by_query.lock().unwrap().insert(
            "musicology".into(),
            vec![SearchResult {
                username: "peer1".into(),
                speed: 500,
                slots: 1,
                files: vec![make_file(
                    "Musicology/01 - Musicology.flac",
                    900,
                    30_000_000,
                )],
            }],
        );

        let results = search_by_title(&client, &library, "", 15, 100)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        let queries = client.search_queries.lock().unwrap().clone();
        assert_eq!(queries, vec!["musicology".to_string()]);
    }

    // (SearchOutcome tests removed with fallback)
}
