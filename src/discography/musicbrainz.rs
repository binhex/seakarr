use std::num::IntErrorKind;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde::Deserialize;

use super::{ArtistCandidate, CodeIdentifier, DiscographyError, DiscographyProvider, ReleaseGroup};

const MUSICBRAINZ_BASE_URL: &str = "https://musicbrainz.org";
const REQUEST_INTERVAL: Duration = Duration::from_secs(1);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_RETRY_AFTER: u64 = 30;
/// Characters of a 5xx body kept for the error message. MusicBrainz sends a
/// short explanation when it denies a request; a proxy error page can be
/// arbitrarily long, so the excerpt is bounded rather than trusted.
const MAX_ERROR_BODY_CHARS: usize = 256;
/// Bytes of a 5xx body kept before it is decoded. Truncating first bounds the
/// work a hostile error page can cost: normalising a multi-megabyte body before
/// the excerpt cut would allocate one slice per word.
const MAX_ERROR_BODY_BYTES: usize = 4096;
/// Characters of the `X-RateLimit-Zone` header kept in the message. The header
/// is attacker-reachable through the same 5xx path as the body, so it is
/// bounded and sanitised the same way.
const MAX_ERROR_ZONE_CHARS: usize = 64;
/// Bytes of one header value kept before it is decoded. Same reasoning as the
/// body bound: a hostile multi-megabyte header must not be normalised in full.
const MAX_ERROR_HEADER_BYTES: usize = 512;
/// A reset-derived pause larger than this is treated as an unusable value (a
/// wrong unit or a clock far out of step) rather than a real wait: MusicBrainz
/// rate-limit windows are seconds to minutes, not days.
const MAX_PLAUSIBLE_PAUSE: Duration = Duration::from_secs(86_400);
/// Unusable `Retry-After` fields quoted in one error message. A hostile response
/// can repeat the header many times; the per-field length is bounded, so the
/// count must be bounded too.
const MAX_QUOTED_RETRY_AFTER: usize = 8;
const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const PAGE_SIZE: usize = 100;
const MAX_PAGES: usize = 100;
/// Reject counts that would need more than `MAX_PAGES` pages (and thus more
/// than 10,000 release groups) before allocating any collection.
const MAX_RELEASE_GROUPS: usize = PAGE_SIZE * MAX_PAGES;

#[derive(Debug, Deserialize)]
struct ArtistPage {
    count: usize,
    offset: usize,
    artists: Vec<ArtistWire>,
}

#[derive(Debug, Deserialize)]
struct ArtistWire {
    id: String,
    name: String,
    #[serde(default, deserialize_with = "deserialize_optional_score")]
    score: Option<u8>,
    /// The artist's own IPI codes. Absent on the entity-lookup endpoint.
    #[serde(default)]
    ipis: Vec<String>,
    /// The artist's own ISNI codes. Absent on the entity-lookup endpoint.
    #[serde(default)]
    isnis: Vec<String>,
    /// MusicBrainz disambiguation comment, e.g. "DnB producer Josh Jackson".
    #[serde(default)]
    disambiguation: Option<String>,
}

impl From<ArtistWire> for ArtistCandidate {
    fn from(wire: ArtistWire) -> Self {
        Self {
            id: wire.id,
            name: wire.name,
            score: wire.score,
            ipis: wire.ipis,
            isnis: wire.isnis,
            disambiguation: wire.disambiguation,
        }
    }
}

/// Reject an artist search page that cannot be trusted.
///
/// A non-zero offset means the provider answered the wrong page, and a payload
/// that disagrees with the reported count means one inconsistent response. The
/// two are classified apart because only the first is a provider defect.
fn check_artist_page(page: &ArtistPage) -> std::result::Result<(), DiscographyError> {
    if page.offset != 0 {
        return Err(DiscographyError::IncompletePagination(format!(
            "artist search returned a non-zero offset {}",
            page.offset
        )));
    }
    if page.count != page.artists.len() {
        return Err(DiscographyError::ArtistSearchInconsistent(format!(
            "artist search returned {} candidates but reported {}",
            page.artists.len(),
            page.count
        )));
    }
    Ok(())
}

/// Decode a MusicBrainz artist-search score.
///
/// The search API returns a JSON integer, while MusicBrainz's MMD JSON
/// examples show a numeric string; JSON numbers with an integral value are
/// equivalent, so all three encodings are accepted. Anything else, and
/// any value outside the documented 0-100 range, is a decoding error: a score
/// must never be clamped, defaulted, or silently discarded, because the
/// resolver treats a missing score as "cannot rank" and an invented score
/// would change which artist is selected.
fn deserialize_optional_score<'de, D>(deserializer: D) -> std::result::Result<Option<u8>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize as _;

    let value = serde_json::Value::deserialize(deserializer)?;
    let score = match value {
        serde_json::Value::Null => return Ok(None),
        serde_json::Value::Number(number) => number.as_i64().or_else(|| {
            number
                .as_f64()
                .filter(|value| value.fract() == 0.0)
                .map(|value| value as i64)
        }),
        serde_json::Value::String(text) => text.trim().parse::<i64>().ok(),
        _ => None,
    };
    match score {
        Some(score) if (0..=100).contains(&score) => Ok(Some(score as u8)),
        Some(invalid) => Err(serde::de::Error::custom(format!(
            "artist search score {invalid} is outside the 0-100 range"
        ))),
        None => Err(serde::de::Error::custom(
            "artist search score is not an integer",
        )),
    }
}

#[derive(Debug, Deserialize)]
struct ReleaseGroupPage {
    #[serde(rename = "release-group-count")]
    count: usize,
    #[serde(rename = "release-group-offset")]
    offset: usize,
    #[serde(rename = "release-groups")]
    release_groups: Vec<ReleaseGroupWire>,
}

#[derive(Debug, Deserialize)]
struct ReleaseGroupWire {
    id: String,
    title: String,
    #[serde(rename = "first-release-date")]
    first_release_date: Option<String>,
    #[serde(rename = "primary-type")]
    primary_type: Option<String>,
    #[serde(rename = "secondary-types", default)]
    secondary_types: Vec<String>,
}

impl From<ReleaseGroupWire> for ReleaseGroup {
    fn from(wire: ReleaseGroupWire) -> Self {
        Self {
            id: wire.id,
            title: wire.title,
            first_release_date: wire.first_release_date,
            primary_type: wire.primary_type,
            secondary_types: wire.secondary_types,
        }
    }
}

pub struct MusicBrainzProvider {
    client: reqwest::Client,
    base_url: String,
    request_interval: Duration,
    next_request: Arc<tokio::sync::Mutex<tokio::time::Instant>>,
}

fn production_request_gate() -> Arc<tokio::sync::Mutex<tokio::time::Instant>> {
    static GATE: OnceLock<Arc<tokio::sync::Mutex<tokio::time::Instant>>> = OnceLock::new();
    Arc::clone(GATE.get_or_init(|| Arc::new(tokio::sync::Mutex::new(tokio::time::Instant::now()))))
}

impl MusicBrainzProvider {
    pub fn new() -> std::result::Result<Self, DiscographyError> {
        Self::build(
            MUSICBRAINZ_BASE_URL.to_string(),
            REQUEST_INTERVAL,
            production_request_gate(),
        )
    }

    #[cfg(test)]
    pub(crate) fn for_test(
        base_url: String,
        request_interval: Duration,
    ) -> std::result::Result<Self, DiscographyError> {
        Self::build(
            base_url,
            request_interval,
            Arc::new(tokio::sync::Mutex::new(tokio::time::Instant::now())),
        )
    }

    fn build(
        base_url: String,
        request_interval: Duration,
        next_request: Arc<tokio::sync::Mutex<tokio::time::Instant>>,
    ) -> std::result::Result<Self, DiscographyError> {
        let user_agent = format!(
            "seakarr/{} (https://github.com/binhex/seakarr)",
            env!("CARGO_PKG_VERSION")
        );
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .user_agent(user_agent)
            .build()
            .map_err(|error| DiscographyError::Transport(error.to_string()))?;
        Ok(Self {
            client,
            base_url,
            request_interval,
            next_request,
        })
    }

    /// Hold the shared gate: sleep until the next allowed slot when
    /// necessary, then claim the slot that starts one `request_interval`
    /// from now. All provider instances share the gate, so production
    /// requests are spaced one second apart process-wide.
    async fn pace(&self) {
        let mut next_request = self.next_request.lock().await;
        let now = tokio::time::Instant::now();
        if *next_request > now {
            tokio::time::sleep_until(*next_request).await;
        }
        *next_request = tokio::time::Instant::now() + self.request_interval;
    }

    async fn read_limited(
        response: &mut reqwest::Response,
    ) -> std::result::Result<Vec<u8>, DiscographyError> {
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| DiscographyError::Transport(error.to_string()))?
        {
            let total = body
                .len()
                .checked_add(chunk.len())
                .ok_or(DiscographyError::ResponseTooLarge)?;
            if total > MAX_RESPONSE_BYTES {
                return Err(DiscographyError::ResponseTooLarge);
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }

    /// Execute one bounded request with pacing and up to two retries.
    ///
    /// Transport and plain 5xx failures back off one then two seconds; a 429
    /// waits its integer `Retry-After` (rejecting malformed values or values
    /// above 30 seconds); other 4xx statuses and decode failures return at once.
    /// A 5xx that names a pause waits that long instead, because MusicBrainz
    /// documents that retrying before its rate-limit window reopens is very
    /// likely to fail. The pause is the later of `Retry-After` - delta seconds
    /// or an HTTP-date - and the `X-RateLimit-Reset` epoch second, both measured
    /// against the response's own `Date` when it carries a parseable one, so
    /// client clock skew cannot change the wait. A malformed or non-UTF-8 field
    /// is ignored while another hint still applies, and the local backoff is
    /// used only when no hint is usable - including a hint that has already
    /// elapsed, and a reset more than a day away, which is treated as a wrong
    /// unit rather than a real pause. A pause beyond the cap is surfaced at once
    /// rather than slept on, so the operator learns the wait instead of the
    /// client retrying into a closed window. Each attempt also carries the 10 s
    /// request timeout, so one call spends at most two capped pauses plus three
    /// timeouts, never an unbounded wait.
    async fn request_json<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, String)],
    ) -> std::result::Result<T, DiscographyError> {
        let url = format!("{}{}", self.base_url, path);
        let mut attempt = 0usize;
        loop {
            self.pace().await;
            let response = self.client.get(&url).query(query).send().await;
            match response {
                Ok(mut response) => {
                    let status = response.status();
                    if status.is_success() {
                        let bytes = Self::read_limited(&mut response).await?;
                        return serde_json::from_slice(&bytes)
                            .map_err(|error| DiscographyError::Decode(error.to_string()));
                    }
                    if status.as_u16() == 429 {
                        let retry_after = parse_retry_after(&response)?;
                        if attempt == 2 {
                            return Err(DiscographyError::HttpStatus(429));
                        }
                        tokio::time::sleep(retry_after).await;
                    } else if status.is_server_error() {
                        // MusicBrainz documents that retrying before its own
                        // rate-limit reset is very likely to fail, so the pause
                        // it asks for outranks the local backoff. A hint longer
                        // than the retry cap is surfaced instead of slept on:
                        // retrying inside a long window only adds load, and the
                        // operator needs the wait in the message.
                        let hint = server_retry_hint(&response);
                        let within_cap =
                            hint.is_none_or(|wait| wait <= Duration::from_secs(MAX_RETRY_AFTER));
                        if attempt == 2 || !within_cap {
                            return Err(provider_error(response, status.as_u16(), hint).await);
                        }
                        let wait = hint.unwrap_or_else(|| backoff(attempt));
                        tokio::time::sleep(wait).await;
                    } else {
                        return Err(DiscographyError::HttpStatus(status.as_u16()));
                    }
                }
                Err(error) => {
                    if attempt == 2 {
                        return Err(DiscographyError::Transport(error.to_string()));
                    }
                    tokio::time::sleep(backoff(attempt)).await;
                }
            }
            attempt += 1;
        }
    }

    async fn search_artists(
        &self,
        artist: &str,
    ) -> std::result::Result<Vec<ArtistCandidate>, DiscographyError> {
        let escaped = artist.replace('\\', "\\\\").replace('"', "\\\"");
        let query_value = format!("artist:\"{escaped}\"");
        let page: ArtistPage = self
            .request_json(
                "/ws/2/artist",
                &[
                    ("query", query_value),
                    ("fmt", "json".into()),
                    ("limit", PAGE_SIZE.to_string()),
                ],
            )
            .await?;
        check_artist_page(&page)?;
        Ok(page
            .artists
            .into_iter()
            .map(ArtistCandidate::from)
            .collect())
    }

    /// Search the artist index for the artist claiming a code.
    async fn search_artists_by_code(
        &self,
        code: CodeIdentifier<'_>,
    ) -> std::result::Result<Vec<ArtistCandidate>, DiscographyError> {
        let page: ArtistPage = self
            .request_json(
                "/ws/2/artist",
                &[
                    ("query", code.query()),
                    ("fmt", "json".into()),
                    ("limit", PAGE_SIZE.to_string()),
                ],
            )
            .await?;
        check_artist_page(&page)?;
        Ok(page
            .artists
            .into_iter()
            .map(ArtistCandidate::from)
            .collect())
    }

    async fn artist_by_id(
        &self,
        artist_mbid: &str,
    ) -> std::result::Result<ArtistCandidate, DiscographyError> {
        let wire: ArtistWire = self
            .request_json(
                &format!("/ws/2/artist/{artist_mbid}"),
                &[("fmt", "json".into())],
            )
            .await?;
        if !wire.id.eq_ignore_ascii_case(artist_mbid) {
            return Err(DiscographyError::Decode(format!(
                "artist id {} does not match the requested MBID {artist_mbid}",
                wire.id
            )));
        }
        // The lookup-by-MBID endpoint carries no search relevance score and no
        // search-index identifiers, and a configured MBID needs no ranking.
        Ok(ArtistCandidate::from(wire))
    }

    async fn release_groups(
        &self,
        artist_mbid: &str,
    ) -> std::result::Result<Vec<ReleaseGroup>, DiscographyError> {
        let mut query = vec![
            ("artist", artist_mbid.to_string()),
            ("fmt", "json".into()),
            ("limit", PAGE_SIZE.to_string()),
            ("offset", "0".into()),
            ("release-group-status", "website-default".into()),
        ];
        let first: ReleaseGroupPage = self.request_json("/ws/2/release-group", &query).await?;
        if first.count > MAX_RELEASE_GROUPS {
            return Err(DiscographyError::IncompletePagination(format!(
                "count {} exceeds the {MAX_RELEASE_GROUPS} release-group limit",
                first.count
            )));
        }
        if first.offset != 0 {
            return Err(DiscographyError::IncompletePagination(format!(
                "first page offset {} is not zero",
                first.offset
            )));
        }
        let expected_first_len = first.count.min(PAGE_SIZE);
        if first.release_groups.len() != expected_first_len {
            return Err(DiscographyError::IncompletePagination(format!(
                "first page returned {} of the expected {expected_first_len} release groups",
                first.release_groups.len()
            )));
        }
        let mut groups: Vec<ReleaseGroup> = Vec::with_capacity(first.count);
        groups.extend(first.release_groups.into_iter().map(Into::into));
        let mut offset = PAGE_SIZE;
        while groups.len() < first.count {
            let expected_page_len = (first.count - offset).min(PAGE_SIZE);
            query[3] = ("offset", offset.to_string());
            let page: ReleaseGroupPage = self.request_json("/ws/2/release-group", &query).await?;
            if page.count != first.count {
                return Err(DiscographyError::IncompletePagination(format!(
                    "page count {} changed from {}",
                    page.count, first.count
                )));
            }
            if page.offset != offset {
                return Err(DiscographyError::IncompletePagination(format!(
                    "page offset {} does not advance to requested offset {offset}",
                    page.offset
                )));
            }
            if page.release_groups.len() != expected_page_len {
                return Err(DiscographyError::IncompletePagination(format!(
                    "offset {offset} returned {} of the expected {expected_page_len} release groups",
                    page.release_groups.len()
                )));
            }
            groups.extend(page.release_groups.into_iter().map(Into::into));
            offset += PAGE_SIZE;
        }
        if groups.len() != first.count {
            return Err(DiscographyError::IncompletePagination(format!(
                "collected {} of {} release groups",
                groups.len(),
                first.count
            )));
        }
        let unique_ids: std::collections::HashSet<&str> =
            groups.iter().map(|group| group.id.as_str()).collect();
        if unique_ids.len() != groups.len() {
            return Err(DiscographyError::IncompletePagination(
                "release-group pages contain duplicate IDs".into(),
            ));
        }
        Ok(groups)
    }
}

#[async_trait]
impl DiscographyProvider for MusicBrainzProvider {
    async fn search_artists(
        &self,
        artist: &str,
    ) -> std::result::Result<Vec<ArtistCandidate>, DiscographyError> {
        self.search_artists(artist).await
    }

    async fn search_artists_by_code(
        &self,
        code: CodeIdentifier<'_>,
    ) -> std::result::Result<Vec<ArtistCandidate>, DiscographyError> {
        self.search_artists_by_code(code).await
    }

    async fn artist_by_id(
        &self,
        artist_mbid: &str,
    ) -> std::result::Result<ArtistCandidate, DiscographyError> {
        self.artist_by_id(artist_mbid).await
    }

    async fn release_groups(
        &self,
        artist_mbid: &str,
    ) -> std::result::Result<Vec<ReleaseGroup>, DiscographyError> {
        self.release_groups(artist_mbid).await
    }
}

fn backoff(attempt: usize) -> Duration {
    match attempt {
        0 => Duration::from_secs(1),
        _ => Duration::from_secs(2),
    }
}

/// The pause a 5xx response asked for: `Retry-After` (delta seconds or an
/// HTTP-date) or `X-RateLimit-Reset` (the epoch second the window reopens),
/// whichever is later.
///
/// The reset unit is epoch seconds, not delta seconds and not milliseconds:
/// MetaBrainz documents `X-RateLimit-Reset` as "the remaining window before the
/// rate limit resets, in UTC epoch seconds"
/// (<https://community.metabrainz.org/t/what-do-the-x-ratelimit-headers-mean-and-how-should-they-be-used/259737>),
/// and a live `musicbrainz.org` 200 response carried `x-ratelimit-reset` three
/// seconds before its own `Date` epoch, which only an epoch value can be. A
/// delta-seconds reading collapses to a filtered zero; a millisecond reading
/// exceeds the plausibility bound. Both are treated as unusable, so the failure
/// stays inside the bounded backoff instead of retrying in a closed window or
/// sleeping for years.
///
/// Both values are advisory. A malformed or absent value is skipped, and
/// `None` means neither was usable, so the caller falls back to its own
/// backoff, because a 503 is an outage signal even when the response says
/// nothing useful. A zero or already-elapsed value is also treated as no hint:
/// a replayed or stale reset header must not license an immediate retry, and
/// the backoff is the only safe pause left. MusicBrainz documents that retrying
/// before the reset is very likely to fail, which is why the later of the two
/// wins over a shorter `Retry-After`.
fn server_retry_hint(response: &reqwest::Response) -> Option<Duration> {
    let now = server_now_epoch(response).unwrap_or_else(local_now_epoch);
    let retry_after = retry_after_pause(response, now);
    let until_reset = reset_pause(response, now);
    match (retry_after, until_reset) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (hint, None) | (None, hint) => hint,
    }
}

/// The pause named by `X-RateLimit-Reset`, or `None` when it is unusable.
///
/// An epoch beyond `i64::MAX` is rejected explicitly rather than wrapping to a
/// negative number: the value is nonsense, so it must not masquerade as an
/// elapsed window. A reset beyond a day is a wrong unit or a clock far out of
/// step, not a pause a rate limiter would ask for.
fn reset_pause(response: &reqwest::Response, now: i64) -> Option<Duration> {
    let epoch = i64::try_from(header_integer_max(response.headers(), "x-ratelimit-reset")?).ok()?;
    let wait = Duration::from_secs(epoch.saturating_sub(now).max(0) as u64);
    if wait.is_zero() || wait > MAX_PLAUSIBLE_PAUSE {
        None
    } else {
        Some(wait)
    }
}

/// The longest pause named by any `Retry-After` field.
fn retry_after_pause(response: &reqwest::Response, now: i64) -> Option<Duration> {
    response
        .headers()
        .get_all("retry-after")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .filter_map(|text| retry_after_field(text.trim(), now))
        .max()
        .filter(|wait| !wait.is_zero())
}

/// One `Retry-After` field: delta-seconds, an HTTP-date, or a comma-merged list.
///
/// Both single forms are legal, and a date means "do not come back before this
/// instant", so it is converted rather than ignored. The list form appears when
/// an intermediary merges duplicate fields; the whole field is tried first,
/// because an HTTP-date itself contains a comma.
fn retry_after_field(text: &str, now: i64) -> Option<Duration> {
    if let Some(wait) = retry_after_single(text, now) {
        return Some(wait);
    }
    // Every comma-separated suffix is tried, not every piece: an HTTP-date
    // carries commas of its own, so a list like `0, Wed, 21 Oct 2035 …` only
    // parses from the comma that starts the date.
    text.match_indices(',')
        .filter_map(|(index, _)| retry_after_single(text[index + 1..].trim(), now))
        .max()
}

/// One value of a `Retry-After` field.
fn retry_after_single(text: &str, now: i64) -> Option<Duration> {
    if let Some(seconds) = parse_delay_field(text) {
        return Some(Duration::from_secs(seconds));
    }
    parse_http_date(text).map(|epoch| Duration::from_secs(epoch.saturating_sub(now).max(0) as u64))
}

/// A delay in seconds. An all-digit value too large for `u64` saturates: the
/// server named a pause, and skipping it would retry inside a window the cap
/// exists to refuse. A non-numeric value is unusable.
fn parse_delay_field(text: &str) -> Option<u64> {
    match text.parse::<u64>() {
        Ok(number) => Some(number),
        Err(error)
            if error.kind() == &IntErrorKind::PosOverflow
                && !text.is_empty()
                && text.bytes().all(|byte| byte.is_ascii_digit()) =>
        {
            Some(u64::MAX)
        }
        Err(_) => None,
    }
}

/// Parse the three HTTP-date forms a `Retry-After` may carry (RFC 9110).
fn parse_http_date(text: &str) -> Option<i64> {
    const FORMATS: [&str; 3] = [
        "%a, %d %b %Y %H:%M:%S GMT", // IMF-fixdate
        "%A, %d-%b-%y %H:%M:%S GMT", // RFC 850
        "%a %b %e %H:%M:%S %Y",      // asctime
    ];
    FORMATS.iter().find_map(|format| {
        chrono::NaiveDateTime::parse_from_str(text, format)
            .ok()
            .map(|naive| naive.and_utc().timestamp())
    })
}

/// The response's own clock, from its `Date` header.
///
/// `X-RateLimit-Reset` is an absolute epoch, so measuring it against the client
/// clock turns skew into either a no-retry failure (client behind) or an
/// immediate retry inside the closed window (client ahead). The response's
/// `Date` comes from the same server, which makes the difference skew-free
/// whenever the header parses.
fn server_now_epoch(response: &reqwest::Response) -> Option<i64> {
    let value = response.headers().get("date")?.to_str().ok()?;
    chrono::NaiveDateTime::parse_from_str(value.trim(), "%a, %d %b %Y %H:%M:%S GMT")
        .ok()
        .map(|naive| naive.and_utc().timestamp())
}

/// The local clock, used only when the response carried no parseable `Date`.
fn local_now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs() as i64)
}

/// The largest integer carried by every field of one header.
///
/// A repeated header must not hide a longer pause behind a shorter first value:
/// `Retry-After: 0` followed by `Retry-After: 60` would otherwise retry inside
/// the closed window. Duplicates are invalid per RFC 9110, so the conservative
/// reading is the maximum rather than the first. The same rule guards
/// `X-RateLimit-Reset`, which is why this takes the header map rather than the
/// response.
fn header_integer_max(headers: &reqwest::header::HeaderMap, name: &str) -> Option<u64> {
    headers
        .get_all(name)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .filter_map(|text| parse_delay_field(text.trim()))
        .max()
}

/// Whether a `Retry-After` field names a pause the client can honour.
///
/// Usability is decided from the raw field, not from its sanitised rendering: a
/// field that only parses after invisible characters are stripped was never
/// honoured, so it must be quoted as ignored rather than silently dropped.
fn is_usable_retry_after(value: &reqwest::header::HeaderValue, now: i64) -> bool {
    let Ok(text) = value.to_str() else {
        return false;
    };
    let text = text.trim();
    // A legal `0` is usable but says "now"; it is superseded by any reset rather
    // than quoted as a defect. An elapsed *date* is different: it names a
    // window that has passed, which is a defect worth quoting.
    if parse_delay_field(text).is_some() {
        return true;
    }
    retry_after_field(text, now).is_some_and(|wait| !wait.is_zero())
}

/// Render one header field as bounded, printable, single-line text.
fn header_text(value: &reqwest::header::HeaderValue) -> String {
    let bytes = value.as_bytes();
    let head = &bytes[..bytes.len().min(MAX_ERROR_HEADER_BYTES)];
    single_line(&String::from_utf8_lossy(head), MAX_ERROR_ZONE_CHARS)
}

/// Render one unusable header field so its defect stays visible.
///
/// Unlike [`header_text`], invisible characters are escaped rather than
/// replaced: `Retry-After: 5<ZWSP>` must not render as a perfectly good `5` the
/// client refused for no visible reason. The raw value is clipped first and the
/// escaped form is never re-clipped, so the closing quote always survives and a
/// clipped escape cannot expose the following part of the message.
fn header_evidence(value: &reqwest::header::HeaderValue) -> String {
    let bytes = value.as_bytes();
    let head = &bytes[..bytes.len().min(MAX_ERROR_HEADER_BYTES)];
    let text = String::from_utf8_lossy(head);
    let clipped: String = text.chars().take(MAX_ERROR_ZONE_CHARS).collect();
    let mut rendered = format!("{clipped:?}");
    if text.chars().count() > MAX_ERROR_ZONE_CHARS {
        rendered.push_str("...");
    }
    rendered
}

/// Turn a 5xx response into the typed error the caller classifies and the
/// operator reads: the status, the rate-limit zone when present, the pause the
/// server asked for, and a bounded excerpt of the body.
async fn provider_error(
    mut response: reqwest::Response,
    status: u16,
    hint: Option<Duration>,
) -> DiscographyError {
    let zone = response
        .headers()
        .get("x-ratelimit-zone")
        .map(header_text)
        .filter(|zone| !zone.is_empty());
    let now = server_now_epoch(&response).unwrap_or_else(local_now_epoch);
    let ignored_retry_after: Vec<String> = response
        .headers()
        .get_all("retry-after")
        .iter()
        .filter(|value| !is_usable_retry_after(value, now))
        .take(MAX_QUOTED_RETRY_AFTER)
        .map(header_evidence)
        .collect();
    let now = server_now_epoch(&response).unwrap_or_else(local_now_epoch);
    let unusable_reset = response
        .headers()
        .get("x-ratelimit-reset")
        .filter(|_| reset_pause(&response, now).is_none())
        .map(header_evidence);
    let body = match MusicBrainzProvider::read_limited(&mut response).await {
        Ok(bytes) => {
            let head = &bytes[..bytes.len().min(MAX_ERROR_BODY_BYTES)];
            single_line(&String::from_utf8_lossy(head), MAX_ERROR_BODY_CHARS)
        }
        Err(_) => "<body omitted>".to_string(),
    };
    let mut parts = Vec::new();
    if let Some(zone) = zone {
        // The zone is stripped of quotes and semicolons by `single_line`, and
        // the evidence fields are Debug-quoted, so no value can forge a
        // `;`-separated part of this message.
        parts.push(format!("rate-limit zone {zone:?}"));
    }
    for ignored in &ignored_retry_after {
        // A field that could not form the honoured hint is still quoted: the
        // operator needs to see what the server said instead of losing it.
        parts.push(format!("retry-after {ignored} not honoured"));
    }
    if let Some(reset) = unusable_reset {
        // The reset is not used, so the operator must still see the window the
        // server named instead of a silent fallback.
        parts.push(format!("x-ratelimit-reset {reset} unusable"));
    }
    if let Some(wait) = hint {
        parts.push(format!("server asked to wait {}s", wait.as_secs()));
        if wait > Duration::from_secs(MAX_RETRY_AFTER) {
            parts.push(format!(
                "not retried: the hint exceeds the {MAX_RETRY_AFTER}s retry cap"
            ));
        } else {
            // The error is built on the return path, so no retry follows it.
            parts.push("retries exhausted".to_string());
        }
    }
    if !body.is_empty() {
        parts.push(format!("body: {body:?}"));
    }
    let detail = if parts.is_empty() {
        String::new()
    } else {
        format!(" ({})", parts.join("; "))
    };
    DiscographyError::ProviderError { status, detail }
}

/// Collapse text to a single printable line, truncated to `limit` characters.
///
/// Control and format characters are replaced with spaces before the whitespace
/// collapse: this text reaches the operator's terminal and log file, and a
/// hostile proxy can answer a 5xx with an ANSI escape sequence or a bidi
/// override that visually rewrites the surrounding log line. The quote and
/// semicolon are removed as well, so a value cannot forge one of the quoted,
/// semicolon-separated parts of the error message.
fn single_line(text: &str, limit: usize) -> String {
    let printable: String = text
        .chars()
        .map(|character| {
            if character.is_control()
                || is_format_character(character)
                || matches!(character, ';' | '"')
            {
                ' '
            } else {
                character
            }
        })
        .collect();
    let collapsed = printable.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() > limit {
        let kept: String = collapsed.chars().take(limit).collect();
        format!("{kept}...")
    } else {
        collapsed
    }
}

/// Unicode format characters (category Cf) that are invisible but reorder or
/// hide the text around them: zero-width characters, the bidirectional
/// overrides and isolates, the Arabic letter mark, the prepended concatenation
/// marks, the Khmer vowel signs, the interlinear annotation anchors, the tag
/// characters (the standard ASCII-smuggling vector), and the byte-order mark.
/// Control characters are handled separately by [`single_line`].
fn is_format_character(character: char) -> bool {
    matches!(
        character,
        '\u{00AD}'
            | '\u{0600}'..='\u{0605}'
            | '\u{061C}'
            | '\u{06DD}'
            | '\u{070F}'
            | '\u{0890}'..='\u{0891}'
            | '\u{08E2}'
            | '\u{17B4}'..='\u{17B5}'
            | '\u{180E}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{206F}'
            | '\u{FFF9}'..='\u{FFFB}'
            | '\u{1107F}'
            | '\u{110BD}'
            | '\u{110CD}'
            | '\u{13430}'..='\u{1343F}'
            | '\u{1BCA0}'..='\u{1BCA3}'
            | '\u{1D173}'..='\u{1D17A}'
            | '\u{E0001}'
            | '\u{E0020}'..='\u{E007F}'
            | '\u{FEFF}'
    )
}

fn parse_retry_after(
    response: &reqwest::Response,
) -> std::result::Result<Duration, DiscographyError> {
    let value = response
        .headers()
        .get("retry-after")
        .and_then(|header| header.to_str().ok())
        .ok_or(DiscographyError::InvalidRetryAfter)?;
    let seconds: u64 = value
        .parse()
        .map_err(|_| DiscographyError::InvalidRetryAfter)?;
    if seconds > MAX_RETRY_AFTER {
        return Err(DiscographyError::InvalidRetryAfter);
    }
    Ok(Duration::from_secs(seconds))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;
    use wiremock::matchers::{header_regex, method, path, query_param};
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};
    fn release_page(count: usize, offset: usize, length: usize) -> serde_json::Value {
        let groups: Vec<serde_json::Value> = (0..length)
            .map(|index| {
                json!({
                    "id": format!("group-{}", offset + index),
                    "title": format!("Album {}", offset + index),
                    "first-release-date": "2000",
                    "primary-type": "Album",
                    "secondary-types": []
                })
            })
            .collect();
        json!({
            "release-group-count": count,
            "release-group-offset": offset,
            "release-groups": groups
        })
    }

    /// Responder that counts every request it answers. Hit counts are the only
    /// observable that timing tests read while the tokio clock is paused; reading
    /// them never parks the test task, so the virtual clock only moves via
    /// explicit `tokio::time::advance` calls.
    #[derive(Clone)]
    struct CountingResponder {
        response: ResponseTemplate,
        hits: Arc<AtomicUsize>,
    }

    impl CountingResponder {
        fn new(response: ResponseTemplate, hits: Arc<AtomicUsize>) -> Self {
            Self { response, hits }
        }
    }

    impl Respond for CountingResponder {
        fn respond(&self, _request: &Request) -> ResponseTemplate {
            self.hits.fetch_add(1, Ordering::SeqCst);
            self.response.clone()
        }
    }

    /// Answer with a 503 whose `X-RateLimit-Reset` is computed when the response
    /// is produced, not when the mock is built. The hint is then measured over
    /// microseconds instead of the mock-building and client-construction time,
    /// so a busy test runner cannot shrink it out from under the assertion.
    #[derive(Clone)]
    struct OffsetResetResponder {
        offset_secs: i64,
        additional_headers: Vec<(&'static str, &'static str)>,
        hits: Arc<AtomicUsize>,
    }

    impl Respond for OffsetResetResponder {
        fn respond(&self, _request: &Request) -> ResponseTemplate {
            self.hits.fetch_add(1, Ordering::SeqCst);
            let now = local_now_epoch();
            let reset = (now + self.offset_secs).max(0).to_string();
            // The `Date` is the server's own clock. It makes the reset delta
            // exact, so the test does not depend on when the mock was built.
            let mut response = ResponseTemplate::new(503)
                .insert_header("date", http_date(now))
                .insert_header("x-ratelimit-reset", reset);
            for (name, value) in &self.additional_headers {
                response = response.insert_header(*name, *value);
            }
            response
        }
    }

    /// Answer a 503 from a server whose clock is wrong by a known offset.
    /// Both `Date` and `X-RateLimit-Reset` come from that skewed clock.
    #[derive(Clone)]
    struct SkewedClockResponder {
        skew_secs: i64,
        pause_secs: i64,
        /// Emit the pause as a `Retry-After` date instead of a reset.
        pause_as_date: bool,
        hits: Arc<AtomicUsize>,
    }

    impl Respond for SkewedClockResponder {
        fn respond(&self, _request: &Request) -> ResponseTemplate {
            self.hits.fetch_add(1, Ordering::SeqCst);
            let server_now = local_now_epoch() + self.skew_secs;
            let response = ResponseTemplate::new(503).insert_header("date", http_date(server_now));
            if self.pause_as_date {
                response.insert_header("retry-after", http_date(server_now + self.pause_secs))
            } else {
                response.insert_header(
                    "x-ratelimit-reset",
                    (server_now + self.pause_secs).to_string(),
                )
            }
        }
    }

    /// Answer 503 with fixed headers, so a test can compute time-sensitive
    /// values (`Date`, a date-form `Retry-After`) at respond time.
    #[derive(Clone)]
    struct HeaderResponder {
        headers: Vec<(&'static str, String)>,
        /// A reset computed at respond time, so the test never depends on when
        /// the mock was built.
        reset_offset_secs: Option<i64>,
        hits: Arc<AtomicUsize>,
    }

    impl Respond for HeaderResponder {
        fn respond(&self, _request: &Request) -> ResponseTemplate {
            self.hits.fetch_add(1, Ordering::SeqCst);
            let mut response = ResponseTemplate::new(503);
            if let Some(offset) = self.reset_offset_secs {
                let reset = (local_now_epoch() + offset).max(0).to_string();
                response = response.insert_header("x-ratelimit-reset", reset);
            }
            for (name, value) in &self.headers {
                response = response.insert_header(*name, value.as_str());
            }
            response
        }
    }

    /// Format an epoch the way an HTTP `Date` field does.
    fn http_date(epoch: i64) -> String {
        chrono::DateTime::from_timestamp(epoch, 0)
            .map(|moment| moment.format("%a, %d %b %Y %H:%M:%S GMT").to_string())
            .unwrap_or_default()
    }

    #[derive(Clone)]
    struct ShortPageResponder {
        hits: Arc<AtomicUsize>,
    }

    impl Respond for ShortPageResponder {
        fn respond(&self, request: &Request) -> ResponseTemplate {
            self.hits.fetch_add(1, Ordering::SeqCst);
            let offset = request
                .url
                .query_pairs()
                .find(|(key, _)| key == "offset")
                .and_then(|(_, value)| value.parse::<usize>().ok())
                .unwrap_or(0);
            ResponseTemplate::new(200).set_body_json(release_page(101, offset, 1))
        }
    }

    /// Yield until `condition` holds, never parking the current task. Parking
    /// under a paused clock can auto-advance time to the earliest timer, so all
    /// paused-time tests spin instead.
    async fn spin_until(mut condition: impl FnMut() -> bool, what: &str) {
        for _ in 0..1_000_000 {
            if condition() {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("spin_until timed out waiting for: {what}");
    }

    #[tokio::test]
    async fn artist_search_sends_identity_headers_and_encoded_query() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ws/2/artist"))
            .and(query_param("query", "artist:\"AC/DC\""))
            .and(query_param("fmt", "json"))
            .and(query_param("limit", "100"))
            .and(header_regex(
                "user-agent",
                r"^seakarr/[0-9]+\.[0-9]+\.[0-9]+ \(https://github.com/binhex/seakarr\)$",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "count": 1,
                "offset": 0,
                "artists": [{
                    "id": "11111111-1111-1111-1111-111111111111",
                    "name": "AC/DC",
                    "score": 100,
                    "sort-name": "AC/DC",
                    "aliases": [{"name": "AC DC"}]
                }]
            })))
            .mount(&server)
            .await;

        let provider = MusicBrainzProvider::for_test(server.uri(), Duration::ZERO).unwrap();
        let artists = provider.search_artists("AC/DC").await.unwrap();
        assert_eq!(artists[0].name, "AC/DC");
        assert!(
            super::super::resolve_artist("AC DC", &artists).is_err(),
            "sort names, aliases, and scores must not widen canonical-name matching"
        );
    }

    #[tokio::test]
    async fn artist_search_parses_the_identifiers_and_disambiguation() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ws/2/artist"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "count": 1,
                "offset": 0,
                "artists": [{
                    "id": "c02a5966-b5c4-483a-8326-483edcf3680e",
                    "name": "Grafix",
                    "score": 100,
                    "disambiguation": "DnB producer Josh Jackson",
                    "ipis": ["00581977202"],
                    "isnis": ["0000000466597022"]
                }]
            })))
            .mount(&server)
            .await;

        let provider = MusicBrainzProvider::for_test(server.uri(), Duration::ZERO).unwrap();
        let candidates = provider.search_artists("Grafix").await.unwrap();

        assert_eq!(
            candidates[0].ipis,
            vec!["00581977202".to_string()],
            "the artist's own IPI must be carried, so a code match can be verified"
        );
        assert_eq!(candidates[0].isnis, vec!["0000000466597022".to_string()]);
        assert_eq!(
            candidates[0].disambiguation.as_deref(),
            Some("DnB producer Josh Jackson")
        );
    }

    #[tokio::test]
    async fn artist_search_defaults_missing_identifiers_to_empty() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ws/2/artist"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "count": 1,
                "offset": 0,
                "artists": [{ "id": "11111111-1111-1111-1111-111111111111", "name": "AC/DC" }]
            })))
            .mount(&server)
            .await;

        let provider = MusicBrainzProvider::for_test(server.uri(), Duration::ZERO).unwrap();
        let candidates = provider.search_artists("AC/DC").await.unwrap();

        assert!(candidates[0].ipis.is_empty());
        assert!(candidates[0].isnis.is_empty());
        assert_eq!(candidates[0].disambiguation, None);
    }

    #[tokio::test]
    async fn code_search_sends_the_identifier_query_term() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ws/2/artist"))
            .and(query_param("query", "isni:\"0000000466597022\""))
            .and(query_param("fmt", "json"))
            .and(query_param("limit", "100"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "count": 1,
                "offset": 0,
                "artists": [{
                    "id": "c02a5966-b5c4-483a-8326-483edcf3680e",
                    "name": "Grafix",
                    "score": 100,
                    "isnis": ["0000000466597022"]
                }]
            })))
            .mount(&server)
            .await;

        let provider = MusicBrainzProvider::for_test(server.uri(), Duration::ZERO).unwrap();
        let candidates = provider
            .search_artists_by_code(CodeIdentifier::Isni("0000000466597022"))
            .await
            .unwrap();

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].name, "Grafix");
    }

    #[tokio::test]
    async fn code_search_rejects_a_non_zero_offset_like_the_name_search() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ws/2/artist"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "count": 1,
                "offset": 25,
                "artists": [{ "id": "11111111-1111-1111-1111-111111111111", "name": "X" }]
            })))
            .mount(&server)
            .await;

        let provider = MusicBrainzProvider::for_test(server.uri(), Duration::ZERO).unwrap();
        let error = provider
            .search_artists_by_code(CodeIdentifier::Ipi("00581977202"))
            .await
            .unwrap_err();

        assert!(
            matches!(error, DiscographyError::IncompletePagination(_)),
            "an identifier search classifies a wrong page exactly as a name search does, got {error:?}"
        );
    }

    #[tokio::test]
    async fn release_groups_paginate_with_website_default_status() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ws/2/release-group"))
            .and(query_param(
                "artist",
                "11111111-1111-1111-1111-111111111111",
            ))
            .and(query_param("release-group-status", "website-default"))
            .and(query_param("offset", "0"))
            .respond_with(ResponseTemplate::new(200).set_body_json(release_page(101, 0, 100)))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/ws/2/release-group"))
            .and(query_param("offset", "100"))
            .respond_with(ResponseTemplate::new(200).set_body_json(release_page(101, 100, 1)))
            .expect(1)
            .mount(&server)
            .await;

        let provider = MusicBrainzProvider::for_test(server.uri(), Duration::ZERO).unwrap();
        let groups = provider
            .release_groups("11111111-1111-1111-1111-111111111111")
            .await
            .unwrap();
        assert_eq!(groups.len(), 101);
    }

    #[tokio::test(start_paused = true)]
    async fn requests_are_spaced_one_second_apart() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/first"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
            .expect(1)
            .mount(&server)
            .await;
        let second_hits = Arc::new(AtomicUsize::new(0));
        Mock::given(method("GET"))
            .and(path("/second"))
            .respond_with(CountingResponder::new(
                ResponseTemplate::new(200).set_body_json(json!({"ok": true})),
                Arc::clone(&second_hits),
            ))
            .expect(1)
            .mount(&server)
            .await;

        let provider =
            Arc::new(MusicBrainzProvider::for_test(server.uri(), REQUEST_INTERVAL).unwrap());
        let first = Arc::clone(&provider);
        let first_task =
            tokio::spawn(async move { first.request_json::<Value>("/first", &[]).await });
        spin_until(|| first_task.is_finished(), "first request completion").await;
        first_task.await.unwrap().unwrap();

        let second = Arc::clone(&provider);
        let second_task =
            tokio::spawn(async move { second.request_json::<Value>("/second", &[]).await });
        for _ in 0..256 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            second_hits.load(Ordering::SeqCst),
            0,
            "the second request must not start before its one-second slot"
        );
        assert!(!second_task.is_finished());

        tokio::time::advance(REQUEST_INTERVAL).await;
        spin_until(
            || second_hits.load(Ordering::SeqCst) == 1,
            "second request after its slot",
        )
        .await;
        spin_until(|| second_task.is_finished(), "second request completion").await;
        second_task.await.unwrap().unwrap();
        assert_eq!(second_hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn provider_instances_share_a_request_gate() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/first"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
            .expect(1)
            .mount(&server)
            .await;
        let second_hits = Arc::new(AtomicUsize::new(0));
        Mock::given(method("GET"))
            .and(path("/second"))
            .respond_with(CountingResponder::new(
                ResponseTemplate::new(200).set_body_json(json!({"ok": true})),
                Arc::clone(&second_hits),
            ))
            .expect(1)
            .mount(&server)
            .await;

        let gate = Arc::new(tokio::sync::Mutex::new(tokio::time::Instant::now()));
        let first =
            MusicBrainzProvider::build(server.uri(), REQUEST_INTERVAL, Arc::clone(&gate)).unwrap();
        let first_task =
            tokio::spawn(async move { first.request_json::<Value>("/first", &[]).await });
        spin_until(|| first_task.is_finished(), "first request completion").await;
        first_task.await.unwrap().unwrap();

        let second = MusicBrainzProvider::build(server.uri(), REQUEST_INTERVAL, gate).unwrap();
        let second_task =
            tokio::spawn(async move { second.request_json::<Value>("/second", &[]).await });
        for _ in 0..256 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            second_hits.load(Ordering::SeqCst),
            0,
            "provider two must wait for provider one's one-second slot"
        );
        assert!(!second_task.is_finished());

        tokio::time::advance(REQUEST_INTERVAL).await;
        spin_until(
            || second_hits.load(Ordering::SeqCst) == 1,
            "second request after the shared slot",
        )
        .await;
        spin_until(|| second_task.is_finished(), "second request completion").await;
        second_task.await.unwrap().unwrap();
        assert_eq!(second_hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn retry_after_is_honored() {
        let server = MockServer::start().await;
        let first_hits = Arc::new(AtomicUsize::new(0));
        Mock::given(method("GET"))
            .and(path("/slow"))
            .respond_with(CountingResponder::new(
                ResponseTemplate::new(429).insert_header("retry-after", "1"),
                Arc::clone(&first_hits),
            ))
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;
        let second_hits = Arc::new(AtomicUsize::new(0));
        Mock::given(method("GET"))
            .and(path("/slow"))
            .respond_with(CountingResponder::new(
                ResponseTemplate::new(200).set_body_json(json!({"ok": true})),
                Arc::clone(&second_hits),
            ))
            .expect(1)
            .mount(&server)
            .await;

        let provider = MusicBrainzProvider::for_test(server.uri(), Duration::ZERO).unwrap();
        let task = tokio::spawn(async move { provider.request_json::<Value>("/slow", &[]).await });
        spin_until(
            || first_hits.load(Ordering::SeqCst) == 1,
            "first request answered with 429",
        )
        .await;
        assert_eq!(
            first_hits.load(Ordering::SeqCst),
            1,
            "the first request must be answered exactly once with 429"
        );
        assert_eq!(
            second_hits.load(Ordering::SeqCst),
            0,
            "the retry must not start before advancing one second"
        );

        // The client arms its Retry-After sleep at the (frozen) virtual
        // instant it finishes processing the 429, so drive the clock in whole
        // seconds with long spin windows: the sleep fires on the first advance
        // past its deadline, whichever window the arming lands in.
        let mut advanced = Duration::ZERO;
        while second_hits.load(Ordering::SeqCst) == 0 && !task.is_finished() {
            advanced += Duration::from_secs(1);
            assert!(
                advanced <= Duration::from_secs(6),
                "the Retry-After retry never landed after {advanced:?}"
            );
            tokio::time::advance(Duration::from_secs(1)).await;
            for _ in 0..10_000 {
                tokio::task::yield_now().await;
            }
        }
        assert_eq!(second_hits.load(Ordering::SeqCst), 1);
        assert!(
            advanced >= Duration::from_secs(1),
            "the retry must wait at least the one-second Retry-After, waited {advanced:?}"
        );
        spin_until(|| task.is_finished(), "retried request completion").await;
        let value = task.await.unwrap().unwrap();
        assert_eq!(value["ok"], json!(true));
    }

    #[tokio::test]
    async fn invalid_retry_after_values_are_rejected() {
        for retry_after in [
            None,
            Some("not-a-number"),
            Some("Wed, 21 Oct 2015 07:28:00 GMT"),
            Some("31"),
        ] {
            let server = MockServer::start().await;
            let mut response = ResponseTemplate::new(429);
            if let Some(value) = retry_after {
                response = response.insert_header("retry-after", value);
            }
            Mock::given(method("GET"))
                .and(path("/ws/2/artist"))
                .respond_with(response)
                .expect(1)
                .mount(&server)
                .await;

            let provider = MusicBrainzProvider::for_test(server.uri(), Duration::ZERO).unwrap();
            assert!(matches!(
                provider.search_artists("Nobody").await,
                Err(DiscographyError::InvalidRetryAfter)
            ));
        }
    }

    #[tokio::test]
    async fn exhausted_retry_after_returns_429() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ws/2/artist"))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "0"))
            .expect(3)
            .mount(&server)
            .await;

        let provider = MusicBrainzProvider::for_test(server.uri(), Duration::ZERO).unwrap();
        assert!(matches!(
            provider.search_artists("Nobody").await,
            Err(DiscographyError::HttpStatus(429))
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn exhausted_server_errors_return_a_provider_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ws/2/artist"))
            .respond_with(ResponseTemplate::new(503))
            .expect(3)
            .mount(&server)
            .await;

        let provider = MusicBrainzProvider::for_test(server.uri(), Duration::ZERO).unwrap();
        let task = tokio::spawn(async move { provider.search_artists("Nobody").await });
        advance_until(|| task.is_finished(), Duration::from_secs(12)).await;

        let result = task.await.unwrap();
        let error = match result {
            Err(error) => error,
            Ok(artists) => panic!("an exhausted 503 must be a provider error, got {artists:?}"),
        };
        assert!(
            matches!(&error, DiscographyError::ProviderError { status: 503, .. }),
            "an exhausted 503 must be a provider error, got {error:?}"
        );
        assert_eq!(
            error.to_string(),
            "MusicBrainz returned HTTP 503",
            "a 5xx that said nothing must keep the bare status message"
        );
    }

    /// Advance the paused clock in one-second steps until `done` reports true,
    /// returning how much virtual time passed. A retry sleep arms at the frozen
    /// instant its response is processed, so whole-second advances plus yield
    /// windows land it deterministically.
    async fn advance_until(mut done: impl FnMut() -> bool, limit: Duration) -> Duration {
        let mut advanced = Duration::ZERO;
        while !done() {
            assert!(
                advanced < limit,
                "condition never became true within {limit:?}"
            );
            advanced += Duration::from_secs(1);
            tokio::time::advance(Duration::from_secs(1)).await;
            for _ in 0..10_000 {
                tokio::task::yield_now().await;
            }
        }
        advanced
    }

    /// Advance in sub-second steps. Whole-second steps cannot tell a one-second
    /// pause from a two-second one, because both land on a whole-second tick;
    /// quarter-second steps make the two outcomes disjoint.
    async fn advance_until_by(
        mut done: impl FnMut() -> bool,
        limit: Duration,
        step: Duration,
    ) -> Duration {
        let mut advanced = Duration::ZERO;
        while !done() {
            assert!(
                advanced < limit,
                "condition never became true within {limit:?}"
            );
            advanced += step;
            tokio::time::advance(step).await;
            for _ in 0..10_000 {
                tokio::task::yield_now().await;
            }
        }
        advanced
    }

    /// Mount a 5xx on `/throttled` that answers `failures` times, then a 200.
    async fn mount_flaky_server_error(
        server: &MockServer,
        failures: usize,
        response: ResponseTemplate,
        error_hits: Arc<AtomicUsize>,
        success_hits: Arc<AtomicUsize>,
    ) {
        Mock::given(method("GET"))
            .and(path("/throttled"))
            .respond_with(CountingResponder::new(response, Arc::clone(&error_hits)))
            .up_to_n_times(failures as u64)
            .expect(failures as u64)
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path("/throttled"))
            .respond_with(CountingResponder::new(
                ResponseTemplate::new(200).set_body_json(json!({"ok": true})),
                Arc::clone(&success_hits),
            ))
            .expect(1)
            .mount(server)
            .await;
    }

    /// Mount a 503 with a respond-time reset on `/throttled` once, then a 200.
    async fn mount_reset_then_success(
        server: &MockServer,
        responder: OffsetResetResponder,
        success_hits: Arc<AtomicUsize>,
    ) {
        Mock::given(method("GET"))
            .and(path("/throttled"))
            .respond_with(responder)
            .up_to_n_times(1)
            .expect(1)
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path("/throttled"))
            .respond_with(CountingResponder::new(
                ResponseTemplate::new(200).set_body_json(json!({"ok": true})),
                success_hits,
            ))
            .expect(1)
            .mount(server)
            .await;
    }

    #[tokio::test(start_paused = true)]
    async fn server_error_honors_retry_after() {
        // MusicBrainz answers 503 while a rate-limit window is open, and its own
        // guidance is to pause until the window closes. The plain 5xx backoff is
        // one second, so only a client that reads Retry-After on a 5xx waits the
        // three seconds the server asked for.
        let server = MockServer::start().await;
        let error_hits = Arc::new(AtomicUsize::new(0));
        let success_hits = Arc::new(AtomicUsize::new(0));
        mount_flaky_server_error(
            &server,
            1,
            ResponseTemplate::new(503).insert_header("retry-after", "3"),
            Arc::clone(&error_hits),
            Arc::clone(&success_hits),
        )
        .await;

        let provider = MusicBrainzProvider::for_test(server.uri(), Duration::ZERO).unwrap();
        let task =
            tokio::spawn(async move { provider.request_json::<Value>("/throttled", &[]).await });
        spin_until(
            || error_hits.load(Ordering::SeqCst) == 1,
            "first request answered with 503",
        )
        .await;
        assert_eq!(success_hits.load(Ordering::SeqCst), 0);

        let advanced = advance_until(
            || success_hits.load(Ordering::SeqCst) == 1 || task.is_finished(),
            Duration::from_secs(8),
        )
        .await;
        assert!(
            advanced >= Duration::from_secs(3),
            "the 503 retry must wait the three-second Retry-After, waited {advanced:?}"
        );
        assert!(
            advanced <= Duration::from_secs(5),
            "the 503 retry must not wait longer than the Retry-After, waited {advanced:?}"
        );
        assert_eq!(success_hits.load(Ordering::SeqCst), 1);
        assert_eq!(task.await.unwrap().unwrap()["ok"], json!(true));
    }

    #[tokio::test(start_paused = true)]
    async fn server_error_honors_rate_limit_reset() {
        // `X-RateLimit-Reset` is the epoch second the window reopens. Retrying
        // before it is the failure mode MusicBrainz documents, so the client must
        // wait it out rather than use its one-second backoff. The header is
        // computed when the response is produced, so the wait cannot shrink
        // while the test process is busy building the mock and the client.
        let server = MockServer::start().await;
        let error_hits = Arc::new(AtomicUsize::new(0));
        let success_hits = Arc::new(AtomicUsize::new(0));
        mount_reset_then_success(
            &server,
            OffsetResetResponder {
                offset_secs: 10,
                additional_headers: Vec::new(),
                hits: Arc::clone(&error_hits),
            },
            Arc::clone(&success_hits),
        )
        .await;

        let provider = MusicBrainzProvider::for_test(server.uri(), Duration::ZERO).unwrap();
        let task =
            tokio::spawn(async move { provider.request_json::<Value>("/throttled", &[]).await });
        spin_until(
            || error_hits.load(Ordering::SeqCst) == 1,
            "first request answered with 503",
        )
        .await;

        let advanced = advance_until(
            || success_hits.load(Ordering::SeqCst) == 1 || task.is_finished(),
            Duration::from_secs(14),
        )
        .await;
        assert!(
            advanced >= Duration::from_secs(9),
            "the 503 retry must wait for the rate-limit reset, waited {advanced:?}"
        );
        assert!(
            advanced <= Duration::from_secs(13),
            "the 503 retry must not wait longer than the reset, waited {advanced:?}"
        );
        assert_eq!(success_hits.load(Ordering::SeqCst), 1);
        assert_eq!(task.await.unwrap().unwrap()["ok"], json!(true));
    }

    #[tokio::test(start_paused = true)]
    async fn server_error_prefers_the_longer_rate_limit_hint() {
        // A short Retry-After does not override a later reset: the later of the
        // two is the only pause that is safe to retry after. The reset is
        // computed when the response is produced, so client construction time
        // cannot shrink the wait below the assertion.
        let server = MockServer::start().await;
        let error_hits = Arc::new(AtomicUsize::new(0));
        let success_hits = Arc::new(AtomicUsize::new(0));
        mount_reset_then_success(
            &server,
            OffsetResetResponder {
                offset_secs: 10,
                additional_headers: vec![("retry-after", "1")],
                hits: Arc::clone(&error_hits),
            },
            Arc::clone(&success_hits),
        )
        .await;

        let provider = MusicBrainzProvider::for_test(server.uri(), Duration::ZERO).unwrap();
        let task =
            tokio::spawn(async move { provider.request_json::<Value>("/throttled", &[]).await });
        spin_until(
            || error_hits.load(Ordering::SeqCst) == 1,
            "first request answered with 503",
        )
        .await;

        let advanced = advance_until(
            || success_hits.load(Ordering::SeqCst) == 1 || task.is_finished(),
            Duration::from_secs(14),
        )
        .await;
        assert!(
            advanced >= Duration::from_secs(9),
            "the reset must outrank a shorter Retry-After, waited {advanced:?}"
        );
        assert!(
            advanced <= Duration::from_secs(13),
            "the reset must not be waited out twice, waited {advanced:?}"
        );
        assert_eq!(task.await.unwrap().unwrap()["ok"], json!(true));
    }

    #[tokio::test(start_paused = true)]
    async fn server_error_hint_beyond_the_cap_is_not_retried() {
        // Retrying before MusicBrainz's reset is very likely to fail, so a hint
        // longer than the retry cap must be surfaced instead of retried: one
        // attempt, no sleep, and the error names the wait the server asked for.
        let server = MockServer::start().await;
        let hits = Arc::new(AtomicUsize::new(0));
        Mock::given(method("GET"))
            .and(path("/ws/2/artist"))
            .respond_with(CountingResponder::new(
                ResponseTemplate::new(503).insert_header("retry-after", "31"),
                Arc::clone(&hits),
            ))
            .expect(1)
            .mount(&server)
            .await;

        let provider = MusicBrainzProvider::for_test(server.uri(), Duration::ZERO).unwrap();
        let task = tokio::spawn(async move { provider.search_artists("Nobody").await });
        // Let the first response land before the runtime goes idle: with a
        // paused clock an idle runtime auto-advances, and reqwest's ten-second
        // timeout would fire instead of the branch under test.
        spin_until(
            || hits.load(Ordering::SeqCst) == 1,
            "first request answered with 503",
        )
        .await;
        for _ in 0..10_000 {
            if task.is_finished() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(
            task.is_finished(),
            "a hint past the cap must be returned instead of slept on"
        );

        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "a retry hint past the cap must not be retried inside the window"
        );
        let error = task.await.unwrap().unwrap_err();
        let message = error.to_string();
        assert!(message.contains("503"), "got {message}");
        assert!(
            message.contains("31"),
            "the error must name the wait the server asked for, got {message}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn server_error_detail_names_the_rate_limit_zone_and_body() {
        // MusicBrainz answers 503 for a global overload and for a per-IP or
        // per-application throttle. Only the zone and body tell them apart, and
        // only the error carries them back to the operator.
        let server = MockServer::start().await;
        let hits = Arc::new(AtomicUsize::new(0));
        Mock::given(method("GET"))
            .and(path("/ws/2/artist"))
            .respond_with(CountingResponder::new(
                ResponseTemplate::new(503)
                    .insert_header("x-ratelimit-zone", "global")
                    .set_body_string("rate limit exceeded"),
                Arc::clone(&hits),
            ))
            .expect(3)
            .mount(&server)
            .await;

        let provider = MusicBrainzProvider::for_test(server.uri(), Duration::ZERO).unwrap();
        let task = tokio::spawn(async move { provider.search_artists("Nobody").await });
        spin_until(
            || hits.load(Ordering::SeqCst) == 1,
            "first request answered with 503",
        )
        .await;
        advance_until(|| task.is_finished(), Duration::from_secs(8)).await;
        assert_eq!(hits.load(Ordering::SeqCst), 3);

        let message = task.await.unwrap().unwrap_err().to_string();
        assert!(message.contains("HTTP 503"), "got {message}");
        assert!(
            message.contains("global"),
            "the 503 must say which rate-limit zone denied it, got {message}"
        );
        assert!(
            message.contains("rate limit exceeded"),
            "the 503 must carry the server's body, got {message}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn server_error_body_capture_is_bounded() {
        let server = MockServer::start().await;
        let hits = Arc::new(AtomicUsize::new(0));
        let body = "a".repeat(4096);
        Mock::given(method("GET"))
            .and(path("/ws/2/artist"))
            .respond_with(CountingResponder::new(
                ResponseTemplate::new(503).set_body_string(body),
                Arc::clone(&hits),
            ))
            .expect(3)
            .mount(&server)
            .await;

        let provider = MusicBrainzProvider::for_test(server.uri(), Duration::ZERO).unwrap();
        let task = tokio::spawn(async move { provider.search_artists("Nobody").await });
        spin_until(
            || hits.load(Ordering::SeqCst) == 1,
            "first request answered with 503",
        )
        .await;
        advance_until(|| task.is_finished(), Duration::from_secs(8)).await;

        let message = task.await.unwrap().unwrap_err().to_string();
        assert!(
            message.contains(&"a".repeat(MAX_ERROR_BODY_CHARS)),
            "the start of the 503 body must be kept, got {message}"
        );
        assert!(
            !message.contains(&"a".repeat(MAX_ERROR_BODY_CHARS + 1)),
            "the 503 body excerpt must be bounded, got {} characters",
            message.matches('a').count()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn server_error_with_an_elapsed_reset_waits_the_backoff() {
        // A replayed or stale 503 can carry a reset that has already passed.
        // Zero is not a licence to retry immediately: the local backoff is the
        // only safe pause left, and three back-to-back requests are exactly the
        // behaviour this change exists to remove.
        let server = MockServer::start().await;
        let error_hits = Arc::new(AtomicUsize::new(0));
        let success_hits = Arc::new(AtomicUsize::new(0));
        mount_reset_then_success(
            &server,
            OffsetResetResponder {
                offset_secs: -60,
                additional_headers: Vec::new(),
                hits: Arc::clone(&error_hits),
            },
            Arc::clone(&success_hits),
        )
        .await;

        let provider = MusicBrainzProvider::for_test(server.uri(), Duration::ZERO).unwrap();
        let task =
            tokio::spawn(async move { provider.request_json::<Value>("/throttled", &[]).await });
        spin_until(
            || error_hits.load(Ordering::SeqCst) == 1,
            "first request answered with 503",
        )
        .await;

        // The retry must not have happened yet: an elapsed reset must fall back
        // to the one-second backoff, not to an immediate retry. Without this
        // guard the assertion below still passes when the zero-hint filter is
        // removed, because the immediate retry lands inside the first advance.
        for _ in 0..10_000 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            success_hits.load(Ordering::SeqCst),
            0,
            "an elapsed reset must not license an immediate retry"
        );

        let advanced = advance_until_by(
            || success_hits.load(Ordering::SeqCst) == 1 || task.is_finished(),
            Duration::from_secs(5),
            Duration::from_millis(250),
        )
        .await;
        assert!(
            advanced >= Duration::from_secs(1),
            "an elapsed reset must fall back to the one-second backoff, waited {advanced:?}"
        );
        assert!(
            advanced <= Duration::from_millis(1500),
            "an elapsed reset must use the one-second backoff, waited {advanced:?}"
        );
        assert_eq!(success_hits.load(Ordering::SeqCst), 1);
        assert_eq!(task.await.unwrap().unwrap()["ok"], json!(true));
    }

    #[tokio::test(start_paused = true)]
    async fn server_error_hint_at_the_cap_is_retried() {
        // The cap is inclusive: a pause of exactly MAX_RETRY_AFTER is slept and
        // retried, so the boundary between sleeping and surfacing is 30 then 31.
        let server = MockServer::start().await;
        let error_hits = Arc::new(AtomicUsize::new(0));
        let success_hits = Arc::new(AtomicUsize::new(0));
        mount_flaky_server_error(
            &server,
            1,
            ResponseTemplate::new(503).insert_header("retry-after", "30"),
            Arc::clone(&error_hits),
            Arc::clone(&success_hits),
        )
        .await;

        let provider = MusicBrainzProvider::for_test(server.uri(), Duration::ZERO).unwrap();
        let task =
            tokio::spawn(async move { provider.request_json::<Value>("/throttled", &[]).await });
        spin_until(
            || error_hits.load(Ordering::SeqCst) == 1,
            "first request answered with 503",
        )
        .await;
        assert_eq!(success_hits.load(Ordering::SeqCst), 0);

        let advanced = advance_until(
            || success_hits.load(Ordering::SeqCst) == 1 || task.is_finished(),
            Duration::from_secs(40),
        )
        .await;
        assert!(
            advanced >= Duration::from_secs(30),
            "a hint of exactly the 30s cap must be slept, waited {advanced:?}"
        );
        assert!(
            advanced <= Duration::from_secs(32),
            "a capped hint must not be slept twice, waited {advanced:?}"
        );
        assert_eq!(task.await.unwrap().unwrap()["ok"], json!(true));
    }

    #[tokio::test(start_paused = true)]
    async fn server_error_prefers_retry_after_when_it_is_later() {
        // The later of the two hints wins in both directions: here Retry-After
        // is the longer one, and a nearer reset must not shorten it. Both
        // headers are produced at respond time, so neither can shrink between
        // the mock being built and the client reading them.
        let server = MockServer::start().await;
        let error_hits = Arc::new(AtomicUsize::new(0));
        let success_hits = Arc::new(AtomicUsize::new(0));
        mount_reset_then_success(
            &server,
            OffsetResetResponder {
                offset_secs: 3,
                additional_headers: vec![("retry-after", "5")],
                hits: Arc::clone(&error_hits),
            },
            Arc::clone(&success_hits),
        )
        .await;

        let provider = MusicBrainzProvider::for_test(server.uri(), Duration::ZERO).unwrap();
        let task =
            tokio::spawn(async move { provider.request_json::<Value>("/throttled", &[]).await });
        spin_until(
            || error_hits.load(Ordering::SeqCst) == 1,
            "first request answered with 503",
        )
        .await;

        let advanced = advance_until(
            || success_hits.load(Ordering::SeqCst) == 1 || task.is_finished(),
            Duration::from_secs(12),
        )
        .await;
        assert!(
            advanced >= Duration::from_secs(5),
            "a longer Retry-After must outrank a nearer reset, waited {advanced:?}"
        );
        assert!(
            advanced <= Duration::from_secs(7),
            "a longer Retry-After must not be exceeded, waited {advanced:?}"
        );
        assert_eq!(task.await.unwrap().unwrap()["ok"], json!(true));
    }

    #[tokio::test(start_paused = true)]
    async fn server_error_strips_control_characters_from_the_body() {
        // A hostile or broken proxy can answer with ANSI escapes or NUL bytes.
        // The excerpt reaches the operator's terminal and log file, so only
        // printable text may survive it.
        let server = MockServer::start().await;
        let hits = Arc::new(AtomicUsize::new(0));
        Mock::given(method("GET"))
            .and(path("/ws/2/artist"))
            .respond_with(CountingResponder::new(
                ResponseTemplate::new(503).set_body_string("\u{1b}[2Jrate\u{0} limited\u{7}"),
                Arc::clone(&hits),
            ))
            .expect(3)
            .mount(&server)
            .await;

        let provider = MusicBrainzProvider::for_test(server.uri(), Duration::ZERO).unwrap();
        let task = tokio::spawn(async move { provider.search_artists("Nobody").await });
        spin_until(
            || hits.load(Ordering::SeqCst) == 1,
            "first request answered with 503",
        )
        .await;
        advance_until(|| task.is_finished(), Duration::from_secs(8)).await;

        let message = task.await.unwrap().unwrap_err().to_string();
        assert!(
            message.chars().all(|character| !character.is_control()),
            "the 503 excerpt must not carry control characters, got {message:?}"
        );
        assert!(
            message.contains("rate limited"),
            "the printable text must survive, got {message:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn server_error_retry_after_zero_waits_the_backoff() {
        // Retry-After: 0 is a legal value, but honouring it literally collapses
        // the retries into a burst. The backoff is the pause that keeps the
        // client inside MusicBrainz's one-per-second budget.
        let server = MockServer::start().await;
        let error_hits = Arc::new(AtomicUsize::new(0));
        let success_hits = Arc::new(AtomicUsize::new(0));
        mount_flaky_server_error(
            &server,
            1,
            ResponseTemplate::new(503).insert_header("retry-after", "0"),
            Arc::clone(&error_hits),
            Arc::clone(&success_hits),
        )
        .await;

        let provider = MusicBrainzProvider::for_test(server.uri(), Duration::ZERO).unwrap();
        let task =
            tokio::spawn(async move { provider.request_json::<Value>("/throttled", &[]).await });
        spin_until(
            || error_hits.load(Ordering::SeqCst) == 1,
            "first request answered with 503",
        )
        .await;
        for _ in 0..10_000 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            success_hits.load(Ordering::SeqCst),
            0,
            "a zero Retry-After must not license an immediate retry"
        );

        let advanced = advance_until_by(
            || success_hits.load(Ordering::SeqCst) == 1 || task.is_finished(),
            Duration::from_secs(5),
            Duration::from_millis(250),
        )
        .await;
        assert!(
            advanced >= Duration::from_secs(1),
            "a zero Retry-After must fall back to the one-second backoff, waited {advanced:?}"
        );
        assert!(
            advanced <= Duration::from_millis(1500),
            "a zero Retry-After must use the one-second backoff, waited {advanced:?}"
        );
        assert_eq!(success_hits.load(Ordering::SeqCst), 1);
        assert_eq!(task.await.unwrap().unwrap()["ok"], json!(true));
    }

    #[tokio::test(start_paused = true)]
    async fn server_error_treats_an_elapsed_http_date_as_unusable() {
        // The 429 path rejects an HTTP-date with InvalidRetryAfter. A 5xx must
        // tolerate the same value instead, and an elapsed date names no future
        // pause, so it falls back to the bounded backoff.
        let server = MockServer::start().await;
        let error_hits = Arc::new(AtomicUsize::new(0));
        let success_hits = Arc::new(AtomicUsize::new(0));
        mount_flaky_server_error(
            &server,
            1,
            ResponseTemplate::new(503)
                .insert_header("retry-after", "Wed, 21 Oct 2015 07:28:00 GMT"),
            Arc::clone(&error_hits),
            Arc::clone(&success_hits),
        )
        .await;

        let provider = MusicBrainzProvider::for_test(server.uri(), Duration::ZERO).unwrap();
        let task =
            tokio::spawn(async move { provider.request_json::<Value>("/throttled", &[]).await });
        spin_until(
            || error_hits.load(Ordering::SeqCst) == 1,
            "first request answered with 503",
        )
        .await;
        for _ in 0..10_000 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            success_hits.load(Ordering::SeqCst),
            0,
            "an HTTP-date Retry-After must wait the backoff, not retry at once"
        );

        let advanced = advance_until_by(
            || success_hits.load(Ordering::SeqCst) == 1 || task.is_finished(),
            Duration::from_secs(5),
            Duration::from_millis(250),
        )
        .await;
        assert!(
            advanced >= Duration::from_secs(1),
            "an HTTP-date Retry-After must fall back to the one-second backoff, waited {advanced:?}"
        );
        assert!(
            advanced <= Duration::from_millis(1500),
            "an HTTP-date Retry-After must use the one-second backoff, waited {advanced:?}"
        );
        assert_eq!(task.await.unwrap().unwrap()["ok"], json!(true));
    }

    #[tokio::test(start_paused = true)]
    async fn server_error_names_an_unusable_retry_after() {
        // Losing the hint silently is what makes an operator unable to tell a
        // throttle from an outage: the value the server sent is quoted back.
        let server = MockServer::start().await;
        let hits = Arc::new(AtomicUsize::new(0));
        Mock::given(method("GET"))
            .and(path("/ws/2/artist"))
            .respond_with(CountingResponder::new(
                ResponseTemplate::new(503)
                    .insert_header("retry-after", "Wed, 21 Oct 2015 07:28:00 GMT"),
                Arc::clone(&hits),
            ))
            .expect(3)
            .mount(&server)
            .await;

        let provider = MusicBrainzProvider::for_test(server.uri(), Duration::ZERO).unwrap();
        let task = tokio::spawn(async move { provider.search_artists("Nobody").await });
        spin_until(
            || hits.load(Ordering::SeqCst) == 1,
            "first request answered with 503",
        )
        .await;
        advance_until(|| task.is_finished(), Duration::from_secs(8)).await;

        let message = task.await.unwrap().unwrap_err().to_string();
        assert!(message.contains("HTTP 503"), "got {message}");
        assert!(
            message.contains("Wed, 21 Oct 2015 07:28:00 GMT"),
            "the unusable value must be quoted, got {message}"
        );
        assert!(
            message.contains("not honoured"),
            "the message must say the hint was not used, got {message}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn server_error_exhausted_hint_reports_no_further_retry() {
        // A terminal error that still reads "retry after 3s" invites the
        // operator to wait for a retry that will never come.
        let server = MockServer::start().await;
        let hits = Arc::new(AtomicUsize::new(0));
        Mock::given(method("GET"))
            .and(path("/ws/2/artist"))
            .respond_with(CountingResponder::new(
                ResponseTemplate::new(503).insert_header("retry-after", "3"),
                Arc::clone(&hits),
            ))
            .expect(3)
            .mount(&server)
            .await;

        let provider = MusicBrainzProvider::for_test(server.uri(), Duration::ZERO).unwrap();
        let task = tokio::spawn(async move { provider.search_artists("Nobody").await });
        spin_until(
            || hits.load(Ordering::SeqCst) == 1,
            "first request answered with 503",
        )
        .await;
        let advanced = advance_until(|| task.is_finished(), Duration::from_secs(12)).await;
        assert!(
            advanced >= Duration::from_secs(6),
            "the hint must be honoured on both attempts, waited {advanced:?}"
        );
        assert!(
            advanced <= Duration::from_secs(8),
            "two three-second pauses must not be exceeded, waited {advanced:?}"
        );

        let message = task.await.unwrap().unwrap_err().to_string();
        assert!(
            message.contains("server asked to wait 3s"),
            "the terminal error must name the pause the server asked for, got {message}"
        );
        assert!(
            message.contains("retries exhausted"),
            "the terminal error must say no retry follows, got {message}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn server_error_sanitizes_the_rate_limit_zone() {
        // The zone travels in the same operator-facing string as the body, so it
        // gets the same treatment: single line, control characters stripped,
        // bounded length.
        let server = MockServer::start().await;
        let zone = format!("global\tzone{}", "z".repeat(200));
        Mock::given(method("GET"))
            .and(path("/ws/2/artist"))
            .respond_with(
                ResponseTemplate::new(503).insert_header("x-ratelimit-zone", zone.as_str()),
            )
            .expect(3)
            .mount(&server)
            .await;

        let provider = MusicBrainzProvider::for_test(server.uri(), Duration::ZERO).unwrap();
        let task = tokio::spawn(async move { provider.search_artists("Nobody").await });
        advance_until(|| task.is_finished(), Duration::from_secs(8)).await;

        let message = task.await.unwrap().unwrap_err().to_string();
        assert!(
            message.chars().all(|character| !character.is_control()),
            "the zone must not carry control characters, got {message:?}"
        );
        assert!(
            message.contains("global zone"),
            "the zone must be collapsed to one line, got {message}"
        );
        assert!(
            !message.contains(&"z".repeat(70)),
            "the zone must be bounded, got {} z characters",
            message.matches('z').count()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn server_error_reset_beyond_the_cap_is_not_retried() {
        // The reset branch must feed the same over-cap decision as Retry-After:
        // one attempt, no sleep, and the requested wait named in the error.
        let server = MockServer::start().await;
        let hits = Arc::new(AtomicUsize::new(0));
        Mock::given(method("GET"))
            .and(path("/ws/2/artist"))
            .respond_with(OffsetResetResponder {
                offset_secs: 120,
                additional_headers: Vec::new(),
                hits: Arc::clone(&hits),
            })
            .expect(1)
            .mount(&server)
            .await;

        let provider = MusicBrainzProvider::for_test(server.uri(), Duration::ZERO).unwrap();
        let task = tokio::spawn(async move { provider.search_artists("Nobody").await });
        spin_until(
            || hits.load(Ordering::SeqCst) == 1,
            "first request answered with 503",
        )
        .await;
        spin_until(|| task.is_finished(), "the over-cap error to be returned").await;

        assert_eq!(hits.load(Ordering::SeqCst), 1);
        let message = task.await.unwrap().unwrap_err().to_string();
        assert!(
            message.contains("server asked to wait 120s"),
            "the requested wait must be named exactly, got {message}"
        );
        assert!(
            message.contains("not retried"),
            "a reset past the cap must not be retried, got {message}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn server_error_reports_an_unreadable_body() {
        // A body over the response cap cannot be read at all; the status must
        // survive even though the excerpt is unavailable.
        let server = MockServer::start().await;
        let hits = Arc::new(AtomicUsize::new(0));
        Mock::given(method("GET"))
            .and(path("/ws/2/artist"))
            .respond_with(CountingResponder::new(
                ResponseTemplate::new(503).set_body_bytes(vec![b'a'; MAX_RESPONSE_BYTES + 1]),
                Arc::clone(&hits),
            ))
            .expect(3)
            .mount(&server)
            .await;

        let provider = MusicBrainzProvider::for_test(server.uri(), Duration::ZERO).unwrap();
        let task = tokio::spawn(async move { provider.search_artists("Nobody").await });
        spin_until(
            || hits.load(Ordering::SeqCst) == 1,
            "first request answered with 503",
        )
        .await;
        advance_until(|| task.is_finished(), Duration::from_secs(8)).await;

        let message = task.await.unwrap().unwrap_err().to_string();
        assert!(message.contains("HTTP 503"), "got {message}");
        assert!(
            message.contains("<body omitted>"),
            "an unreadable body must be reported, got {message}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn server_error_truncates_the_body_before_normalising_it() {
        // The excerpt is cut from the raw bytes first. If normalisation ran over
        // the whole response, text beyond the bound would still reach the
        // message; this body puts the only printable word past the cut.
        let server = MockServer::start().await;
        let hits = Arc::new(AtomicUsize::new(0));
        let body = format!("{}{}", " ".repeat(MAX_ERROR_BODY_BYTES), "late text");
        Mock::given(method("GET"))
            .and(path("/ws/2/artist"))
            .respond_with(CountingResponder::new(
                ResponseTemplate::new(503).set_body_string(body),
                Arc::clone(&hits),
            ))
            .expect(3)
            .mount(&server)
            .await;

        let provider = MusicBrainzProvider::for_test(server.uri(), Duration::ZERO).unwrap();
        let task = tokio::spawn(async move { provider.search_artists("Nobody").await });
        spin_until(
            || hits.load(Ordering::SeqCst) == 1,
            "first request answered with 503",
        )
        .await;
        advance_until(|| task.is_finished(), Duration::from_secs(8)).await;

        let message = task.await.unwrap().unwrap_err().to_string();
        assert!(
            !message.contains("late text"),
            "text beyond the byte cap must not reach the message, got {message}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn server_error_takes_the_longest_repeated_retry_after() {
        // Duplicate fields are invalid, but the conservative reading is the
        // maximum: a short first value must not hide a longer pause.
        let server = MockServer::start().await;
        let error_hits = Arc::new(AtomicUsize::new(0));
        let success_hits = Arc::new(AtomicUsize::new(0));
        mount_flaky_server_error(
            &server,
            1,
            ResponseTemplate::new(503)
                .insert_header("retry-after", "0")
                .append_header("retry-after", "2"),
            Arc::clone(&error_hits),
            Arc::clone(&success_hits),
        )
        .await;

        let provider = MusicBrainzProvider::for_test(server.uri(), Duration::ZERO).unwrap();
        let task =
            tokio::spawn(async move { provider.request_json::<Value>("/throttled", &[]).await });
        spin_until(
            || error_hits.load(Ordering::SeqCst) == 1,
            "first request answered with 503",
        )
        .await;
        assert_eq!(success_hits.load(Ordering::SeqCst), 0);

        let advanced = advance_until(
            || success_hits.load(Ordering::SeqCst) == 1 || task.is_finished(),
            Duration::from_secs(8),
        )
        .await;
        assert!(
            advanced >= Duration::from_secs(2),
            "the longest repeated Retry-After must be honoured, waited {advanced:?}"
        );
        assert!(
            advanced <= Duration::from_secs(4),
            "the longest repeated Retry-After must not be exceeded, waited {advanced:?}"
        );
        assert_eq!(task.await.unwrap().unwrap()["ok"], json!(true));
    }

    #[tokio::test(start_paused = true)]
    async fn server_error_strips_format_characters_from_the_body() {
        // Invisible format characters can reorder or hide the rest of a log
        // line; the body excerpt must not carry them.
        let server = MockServer::start().await;
        let hits = Arc::new(AtomicUsize::new(0));
        Mock::given(method("GET"))
            .and(path("/ws/2/artist"))
            .respond_with(CountingResponder::new(
                ResponseTemplate::new(503).set_body_string("rate\u{202E}limited\u{E0041}now"),
                Arc::clone(&hits),
            ))
            .expect(3)
            .mount(&server)
            .await;

        let provider = MusicBrainzProvider::for_test(server.uri(), Duration::ZERO).unwrap();
        let task = tokio::spawn(async move { provider.search_artists("Nobody").await });
        spin_until(
            || hits.load(Ordering::SeqCst) == 1,
            "first request answered with 503",
        )
        .await;
        advance_until(|| task.is_finished(), Duration::from_secs(8)).await;

        let message = task.await.unwrap().unwrap_err().to_string();
        assert!(
            !message.contains('\u{202E}') && !message.contains('\u{E0041}'),
            "format characters must be stripped, got {message:?}"
        );
        assert!(
            message.contains("rate"),
            "printable text must survive, got {message:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn server_error_quotes_a_non_utf8_header_value() {
        // Header values may legally carry obs-text bytes. Dropping them silently
        // would make a throttling zone indistinguishable from an absent one, so
        // the lossy text is quoted instead.
        let server = MockServer::start().await;
        let zone = reqwest::header::HeaderValue::from_bytes(b"zone\x80limited").unwrap();
        Mock::given(method("GET"))
            .and(path("/ws/2/artist"))
            .respond_with(ResponseTemplate::new(503).insert_header("x-ratelimit-zone", zone))
            .expect(3)
            .mount(&server)
            .await;

        let provider = MusicBrainzProvider::for_test(server.uri(), Duration::ZERO).unwrap();
        let task = tokio::spawn(async move { provider.search_artists("Nobody").await });
        advance_until(|| task.is_finished(), Duration::from_secs(8)).await;

        let message = task.await.unwrap().unwrap_err().to_string();
        assert!(
            message.contains("zone") && message.contains("limited"),
            "a non-UTF-8 zone must still be named, got {message:?}"
        );
        assert!(
            message.chars().all(|character| !character.is_control()),
            "the lossy text must stay printable, got {message:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn server_error_zero_retry_after_still_honours_the_reset() {
        // A zero Retry-After must not discard a usable reset: the two hints are
        // independent, and only the unusable one is dropped.
        let server = MockServer::start().await;
        let error_hits = Arc::new(AtomicUsize::new(0));
        let success_hits = Arc::new(AtomicUsize::new(0));
        mount_reset_then_success(
            &server,
            OffsetResetResponder {
                offset_secs: 10,
                additional_headers: vec![("retry-after", "0")],
                hits: Arc::clone(&error_hits),
            },
            Arc::clone(&success_hits),
        )
        .await;

        let provider = MusicBrainzProvider::for_test(server.uri(), Duration::ZERO).unwrap();
        let task =
            tokio::spawn(async move { provider.request_json::<Value>("/throttled", &[]).await });
        spin_until(
            || error_hits.load(Ordering::SeqCst) == 1,
            "first request answered with 503",
        )
        .await;

        let advanced = advance_until(
            || success_hits.load(Ordering::SeqCst) == 1 || task.is_finished(),
            Duration::from_secs(14),
        )
        .await;
        assert!(
            advanced >= Duration::from_secs(9),
            "a usable reset must be honoured despite a zero Retry-After, waited {advanced:?}"
        );
        assert!(
            advanced <= Duration::from_secs(13),
            "the reset must not be waited out twice, waited {advanced:?}"
        );
        assert_eq!(task.await.unwrap().unwrap()["ok"], json!(true));
    }

    #[tokio::test(start_paused = true)]
    async fn server_error_quotes_a_non_utf8_retry_after() {
        // The unusable-value quote is the only place a non-UTF-8 Retry-After
        // would surface; reading it strictly would silently drop it.
        let server = MockServer::start().await;
        let value = reqwest::header::HeaderValue::from_bytes(b"5\x80").unwrap();
        Mock::given(method("GET"))
            .and(path("/ws/2/artist"))
            .respond_with(ResponseTemplate::new(503).insert_header("retry-after", value))
            .expect(3)
            .mount(&server)
            .await;

        let provider = MusicBrainzProvider::for_test(server.uri(), Duration::ZERO).unwrap();
        let task = tokio::spawn(async move { provider.search_artists("Nobody").await });
        advance_until(|| task.is_finished(), Duration::from_secs(8)).await;

        let message = task.await.unwrap().unwrap_err().to_string();
        assert!(
            message.contains("retry-after") && message.contains("not honoured"),
            "a non-UTF-8 Retry-After must still be quoted, got {message:?}"
        );
    }

    #[test]
    fn header_integer_max_reads_every_field() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.append(
            "retry-after",
            "0".parse::<reqwest::header::HeaderValue>().unwrap(),
        );
        headers.append(
            "retry-after",
            "7".parse::<reqwest::header::HeaderValue>().unwrap(),
        );
        headers.append(
            "x-ratelimit-reset",
            "1793".parse::<reqwest::header::HeaderValue>().unwrap(),
        );
        assert_eq!(header_integer_max(&headers, "retry-after"), Some(7));
        assert_eq!(
            header_integer_max(&headers, "x-ratelimit-reset"),
            Some(1793)
        );
        assert_eq!(header_integer_max(&headers, "x-ratelimit-zone"), None);

        // A non-numeric field is skipped, and an all-digit field too large for
        // u64 saturates instead of being skipped: the server named a pause.
        headers.append(
            "retry-after",
            "abc".parse::<reqwest::header::HeaderValue>().unwrap(),
        );
        assert_eq!(header_integer_max(&headers, "retry-after"), Some(7));
        let mut overflow = reqwest::header::HeaderMap::new();
        overflow.append(
            "retry-after",
            "99999999999999999999"
                .parse::<reqwest::header::HeaderValue>()
                .unwrap(),
        );
        assert_eq!(header_integer_max(&overflow, "retry-after"), Some(u64::MAX));
        let mut dated = reqwest::header::HeaderMap::new();
        dated.append(
            "retry-after",
            "Wed, 21 Oct 2015 07:28:00 GMT"
                .parse::<reqwest::header::HeaderValue>()
                .unwrap(),
        );
        assert_eq!(header_integer_max(&dated, "retry-after"), None);
    }

    #[test]
    fn single_line_truncates_exactly_at_the_limit() {
        let exactly = "a".repeat(MAX_ERROR_BODY_CHARS);
        assert_eq!(single_line(&exactly, MAX_ERROR_BODY_CHARS), exactly);
        let over = "a".repeat(MAX_ERROR_BODY_CHARS + 1);
        assert_eq!(
            single_line(&over, MAX_ERROR_BODY_CHARS),
            format!("{}...", "a".repeat(MAX_ERROR_BODY_CHARS))
        );
        let zone = "z".repeat(MAX_ERROR_ZONE_CHARS);
        assert_eq!(single_line(&zone, MAX_ERROR_ZONE_CHARS), zone);
        // The part separator and the quoting character are removed, so a value
        // cannot forge a `;`-separated part of the message.
        assert_eq!(single_line("a\";b", 10), "a b");
        // Invisible format characters are removed, including the Brahmi number
        // joiner (U+1107F) and the Khmer vowel signs (U+17B4-U+17B5).
        assert_eq!(single_line("a\u{1107F}b", 10), "a b");
        assert_eq!(single_line("a\u{17B4}\u{17B5}b", 10), "a b");
        // A legal zero is usable, not a defect: it is never quoted.
        assert!(is_usable_retry_after(
            &reqwest::header::HeaderValue::from_static("0"),
            local_now_epoch()
        ));
        // An elapsed date names a window that has passed, so it is a defect.
        assert!(!is_usable_retry_after(
            &reqwest::header::HeaderValue::from_static("Wed, 21 Oct 2015 07:28:00 GMT"),
            local_now_epoch()
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn server_error_treats_an_overflowing_retry_after_as_a_pause() {
        // A 21-digit delay overflows u64. Ignoring it would retry inside a window
        // the server says is enormous; saturating makes it an over-cap pause,
        // which is surfaced instead of slept on.
        let server = MockServer::start().await;
        let hits = Arc::new(AtomicUsize::new(0));
        Mock::given(method("GET"))
            .and(path("/ws/2/artist"))
            .respond_with(CountingResponder::new(
                ResponseTemplate::new(503).insert_header("retry-after", "99999999999999999999"),
                Arc::clone(&hits),
            ))
            .expect(1)
            .mount(&server)
            .await;

        let provider = MusicBrainzProvider::for_test(server.uri(), Duration::ZERO).unwrap();
        let task = tokio::spawn(async move { provider.search_artists("Nobody").await });
        spin_until(
            || hits.load(Ordering::SeqCst) == 1,
            "first request answered with 503",
        )
        .await;
        spin_until(|| task.is_finished(), "the over-cap error to be returned").await;

        assert_eq!(hits.load(Ordering::SeqCst), 1);
        let message = task.await.unwrap().unwrap_err().to_string();
        assert!(
            message.contains("not retried"),
            "an overflowing delay must be an over-cap pause, got {message}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn server_error_measures_the_reset_against_the_server_clock() {
        // The server's clock is an hour ahead of the client's. Measured locally
        // the reset would look like a 3610 s pause and become a no-retry provider
        // failure; measured against the response's own Date it is the ten-second
        // pause the server meant.
        let server = MockServer::start().await;
        let error_hits = Arc::new(AtomicUsize::new(0));
        let success_hits = Arc::new(AtomicUsize::new(0));
        Mock::given(method("GET"))
            .and(path("/throttled"))
            .respond_with(SkewedClockResponder {
                skew_secs: 3600,
                pause_secs: 10,
                pause_as_date: false,
                hits: Arc::clone(&error_hits),
            })
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/throttled"))
            .respond_with(CountingResponder::new(
                ResponseTemplate::new(200).set_body_json(json!({"ok": true})),
                Arc::clone(&success_hits),
            ))
            .expect(1)
            .mount(&server)
            .await;

        let provider = MusicBrainzProvider::for_test(server.uri(), Duration::ZERO).unwrap();
        let task =
            tokio::spawn(async move { provider.request_json::<Value>("/throttled", &[]).await });
        spin_until(
            || error_hits.load(Ordering::SeqCst) == 1,
            "first request answered with 503",
        )
        .await;

        let advanced = advance_until(
            || success_hits.load(Ordering::SeqCst) == 1 || task.is_finished(),
            Duration::from_secs(14),
        )
        .await;
        assert!(
            advanced >= Duration::from_secs(9),
            "the reset must be measured against the server clock, waited {advanced:?}"
        );
        assert!(
            advanced <= Duration::from_secs(13),
            "the skewed clock must not lengthen the wait, waited {advanced:?}"
        );
        assert_eq!(task.await.unwrap().unwrap()["ok"], json!(true));
    }

    #[tokio::test(start_paused = true)]
    async fn server_error_decodes_a_non_utf8_body_lossily() {
        // The body is decoded with replacement characters rather than dropped:
        // a proxy error page is still worth reading in the log.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ws/2/artist"))
            .respond_with(ResponseTemplate::new(503).set_body_bytes(b"rate\x80limited".to_vec()))
            .expect(3)
            .mount(&server)
            .await;

        let provider = MusicBrainzProvider::for_test(server.uri(), Duration::ZERO).unwrap();
        let task = tokio::spawn(async move { provider.search_artists("Nobody").await });
        advance_until(|| task.is_finished(), Duration::from_secs(8)).await;

        let message = task.await.unwrap().unwrap_err().to_string();
        assert!(
            message.contains("rate") && message.contains("limited"),
            "a non-UTF-8 body must be decoded lossily, got {message:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn server_error_honors_an_http_date_retry_after() {
        // A date names the instant the client may return. Ignoring it would
        // retry inside the window the server named.
        let server = MockServer::start().await;
        let now = local_now_epoch();
        let error_hits = Arc::new(AtomicUsize::new(0));
        let success_hits = Arc::new(AtomicUsize::new(0));
        Mock::given(method("GET"))
            .and(path("/throttled"))
            .respond_with(HeaderResponder {
                headers: vec![
                    ("date", http_date(now)),
                    ("retry-after", http_date(now + 10)),
                ],
                reset_offset_secs: None,
                hits: Arc::clone(&error_hits),
            })
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/throttled"))
            .respond_with(CountingResponder::new(
                ResponseTemplate::new(200).set_body_json(json!({"ok": true})),
                Arc::clone(&success_hits),
            ))
            .expect(1)
            .mount(&server)
            .await;

        let provider = MusicBrainzProvider::for_test(server.uri(), Duration::ZERO).unwrap();
        let task =
            tokio::spawn(async move { provider.request_json::<Value>("/throttled", &[]).await });
        spin_until(
            || error_hits.load(Ordering::SeqCst) == 1,
            "first request answered with 503",
        )
        .await;

        let advanced = advance_until(
            || success_hits.load(Ordering::SeqCst) == 1 || task.is_finished(),
            Duration::from_secs(14),
        )
        .await;
        assert!(
            advanced >= Duration::from_secs(9),
            "an HTTP-date Retry-After must be waited out, waited {advanced:?}"
        );
        assert!(
            advanced <= Duration::from_secs(13),
            "the date must not be waited out twice, waited {advanced:?}"
        );
        assert_eq!(task.await.unwrap().unwrap()["ok"], json!(true));
    }

    #[tokio::test(start_paused = true)]
    async fn server_error_treats_a_reset_beyond_a_day_as_unusable() {
        // A reset days away is a wrong unit or a broken clock, not a pause a
        // rate limiter asks for. Treating it as usable would turn it into either
        // a day-long wait or a no-retry failure; it must fall back to the
        // bounded backoff instead.
        let server = MockServer::start().await;
        let error_hits = Arc::new(AtomicUsize::new(0));
        let success_hits = Arc::new(AtomicUsize::new(0));
        mount_reset_then_success(
            &server,
            OffsetResetResponder {
                offset_secs: 86_401,
                additional_headers: Vec::new(),
                hits: Arc::clone(&error_hits),
            },
            Arc::clone(&success_hits),
        )
        .await;

        let provider = MusicBrainzProvider::for_test(server.uri(), Duration::ZERO).unwrap();
        let task =
            tokio::spawn(async move { provider.request_json::<Value>("/throttled", &[]).await });
        spin_until(
            || error_hits.load(Ordering::SeqCst) == 1,
            "first request answered with 503",
        )
        .await;

        let advanced = advance_until_by(
            || success_hits.load(Ordering::SeqCst) == 1 || task.is_finished(),
            Duration::from_secs(5),
            Duration::from_millis(250),
        )
        .await;
        assert!(
            advanced >= Duration::from_secs(1),
            "an implausible reset must fall back to the backoff, waited {advanced:?}"
        );
        assert!(
            advanced <= Duration::from_millis(1500),
            "an implausible reset must use the one-second backoff, waited {advanced:?}"
        );
        assert_eq!(task.await.unwrap().unwrap()["ok"], json!(true));
    }

    #[tokio::test(start_paused = true)]
    async fn server_error_falls_back_when_the_date_is_unparseable() {
        // A CDN or proxy can strip or mangle Date. The reset must then still be
        // measured against something rather than discarded.
        let server = MockServer::start().await;
        let error_hits = Arc::new(AtomicUsize::new(0));
        let success_hits = Arc::new(AtomicUsize::new(0));
        Mock::given(method("GET"))
            .and(path("/throttled"))
            .respond_with(HeaderResponder {
                headers: vec![("date", "not-a-date".to_string())],
                reset_offset_secs: Some(20),
                hits: Arc::clone(&error_hits),
            })
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/throttled"))
            .respond_with(CountingResponder::new(
                ResponseTemplate::new(200).set_body_json(json!({"ok": true})),
                Arc::clone(&success_hits),
            ))
            .expect(1)
            .mount(&server)
            .await;

        let provider = MusicBrainzProvider::for_test(server.uri(), Duration::ZERO).unwrap();
        let task =
            tokio::spawn(async move { provider.request_json::<Value>("/throttled", &[]).await });
        spin_until(
            || error_hits.load(Ordering::SeqCst) == 1,
            "first request answered with 503",
        )
        .await;

        let advanced = advance_until(
            || success_hits.load(Ordering::SeqCst) == 1 || task.is_finished(),
            Duration::from_secs(30),
        )
        .await;
        assert!(
            advanced >= Duration::from_secs(18),
            "an unparseable Date must fall back to the local clock, waited {advanced:?}"
        );
        assert!(
            advanced <= Duration::from_secs(22),
            "the reset must not be waited out twice, waited {advanced:?}"
        );
        assert_eq!(task.await.unwrap().unwrap()["ok"], json!(true));
    }

    #[tokio::test(start_paused = true)]
    async fn server_error_honors_a_comma_joined_retry_after() {
        // An intermediary can merge duplicate fields into one comma-list. The
        // longer piece is the pause; treating the field as unparseable would
        // retry inside it.
        let server = MockServer::start().await;
        let error_hits = Arc::new(AtomicUsize::new(0));
        let success_hits = Arc::new(AtomicUsize::new(0));
        mount_flaky_server_error(
            &server,
            1,
            ResponseTemplate::new(503).insert_header("retry-after", "0, 5"),
            Arc::clone(&error_hits),
            Arc::clone(&success_hits),
        )
        .await;

        let provider = MusicBrainzProvider::for_test(server.uri(), Duration::ZERO).unwrap();
        let task =
            tokio::spawn(async move { provider.request_json::<Value>("/throttled", &[]).await });
        spin_until(
            || error_hits.load(Ordering::SeqCst) == 1,
            "first request answered with 503",
        )
        .await;
        assert_eq!(success_hits.load(Ordering::SeqCst), 0);

        let advanced = advance_until(
            || success_hits.load(Ordering::SeqCst) == 1 || task.is_finished(),
            Duration::from_secs(10),
        )
        .await;
        assert!(
            advanced >= Duration::from_secs(5),
            "a comma-joined field must use its longest piece, waited {advanced:?}"
        );
        assert!(
            advanced <= Duration::from_secs(7),
            "the longest piece must not be exceeded, waited {advanced:?}"
        );
        assert_eq!(task.await.unwrap().unwrap()["ok"], json!(true));
    }

    #[test]
    fn parse_http_date_reads_all_three_http_date_forms() {
        // RFC 9110 requires a recipient to accept all three forms.
        assert_eq!(
            parse_http_date("Sun, 06 Nov 1994 08:49:37 GMT"),
            Some(784_111_777)
        );
        assert_eq!(
            parse_http_date("Sunday, 06-Nov-94 08:49:37 GMT"),
            Some(784_111_777)
        );
        assert_eq!(
            parse_http_date("Sun Nov  6 08:49:37 1994"),
            Some(784_111_777)
        );
        assert_eq!(parse_http_date("not-a-date"), None);
    }

    #[tokio::test(start_paused = true)]
    async fn server_error_escapes_invisible_characters_in_unusable_fields() {
        // Collapsing an invisible character to a space would print a value that
        // looks correct; the escaped form keeps the defect visible.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ws/2/artist"))
            .respond_with(ResponseTemplate::new(503).insert_header("retry-after", "5\u{200B}"))
            .expect(3)
            .mount(&server)
            .await;

        let provider = MusicBrainzProvider::for_test(server.uri(), Duration::ZERO).unwrap();
        let task = tokio::spawn(async move { provider.search_artists("Nobody").await });
        advance_until(|| task.is_finished(), Duration::from_secs(8)).await;

        let message = task.await.unwrap().unwrap_err().to_string();
        assert!(
            message.contains("\\u{200b}"),
            "an invisible character must be escaped, got {message:?}"
        );
        assert!(
            message.contains("not honoured"),
            "the field must still be marked ignored, got {message:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn server_error_quotes_at_most_eight_unusable_fields() {
        // A hostile response can repeat the field; the per-field bound alone
        // would let the message grow without limit.
        let server = MockServer::start().await;
        let mut response = ResponseTemplate::new(503);
        for _ in 0..9 {
            response = response.append_header("retry-after", "garbage");
        }
        Mock::given(method("GET"))
            .and(path("/ws/2/artist"))
            .respond_with(response)
            .expect(3)
            .mount(&server)
            .await;

        let provider = MusicBrainzProvider::for_test(server.uri(), Duration::ZERO).unwrap();
        let task = tokio::spawn(async move { provider.search_artists("Nobody").await });
        advance_until(|| task.is_finished(), Duration::from_secs(8)).await;

        let message = task.await.unwrap().unwrap_err().to_string();
        assert_eq!(
            message.matches("not honoured").count(),
            MAX_QUOTED_RETRY_AFTER,
            "only the capped number of fields may be quoted, got {message}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn server_error_treats_a_mixed_date_list_as_the_named_pause() {
        // A merged field can mix a delta and a date. The date is the pause that
        // matters: taking the delta would retry inside the window it names.
        let server = MockServer::start().await;
        let now = local_now_epoch();
        let value = format!("0, {}", http_date(now + 120));
        let hits = Arc::new(AtomicUsize::new(0));
        Mock::given(method("GET"))
            .and(path("/ws/2/artist"))
            .respond_with(HeaderResponder {
                headers: vec![("date", http_date(now)), ("retry-after", value)],
                reset_offset_secs: None,
                hits: Arc::clone(&hits),
            })
            .expect(1)
            .mount(&server)
            .await;

        let provider = MusicBrainzProvider::for_test(server.uri(), Duration::ZERO).unwrap();
        let task = tokio::spawn(async move { provider.search_artists("Nobody").await });
        spin_until(
            || hits.load(Ordering::SeqCst) == 1,
            "first request answered with 503",
        )
        .await;
        spin_until(|| task.is_finished(), "the over-cap error to be returned").await;

        assert_eq!(hits.load(Ordering::SeqCst), 1);
        let message = task.await.unwrap().unwrap_err().to_string();
        assert!(
            message.contains("not retried"),
            "the date in a mixed list must be the named pause, got {message}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn server_error_measures_a_retry_after_date_against_the_server_clock() {
        // The same skew as the reset test: read locally the date would look like
        // a 3610 s pause and become a no-retry failure.
        let server = MockServer::start().await;
        let error_hits = Arc::new(AtomicUsize::new(0));
        let success_hits = Arc::new(AtomicUsize::new(0));
        Mock::given(method("GET"))
            .and(path("/throttled"))
            .respond_with(SkewedClockResponder {
                skew_secs: 3600,
                pause_secs: 10,
                pause_as_date: true,
                hits: Arc::clone(&error_hits),
            })
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/throttled"))
            .respond_with(CountingResponder::new(
                ResponseTemplate::new(200).set_body_json(json!({"ok": true})),
                Arc::clone(&success_hits),
            ))
            .expect(1)
            .mount(&server)
            .await;

        let provider = MusicBrainzProvider::for_test(server.uri(), Duration::ZERO).unwrap();
        let task =
            tokio::spawn(async move { provider.request_json::<Value>("/throttled", &[]).await });
        spin_until(
            || error_hits.load(Ordering::SeqCst) == 1,
            "first request answered with 503",
        )
        .await;

        let advanced = advance_until(
            || success_hits.load(Ordering::SeqCst) == 1 || task.is_finished(),
            Duration::from_secs(14),
        )
        .await;
        assert!(
            advanced >= Duration::from_secs(9),
            "a date-form Retry-After must be measured against the server clock, waited {advanced:?}"
        );
        assert!(
            advanced <= Duration::from_secs(13),
            "the skewed clock must not lengthen the wait, waited {advanced:?}"
        );
        assert_eq!(task.await.unwrap().unwrap()["ok"], json!(true));
    }

    #[tokio::test(start_paused = true)]
    async fn server_error_treats_an_out_of_range_reset_as_unusable() {
        // u64::MAX would wrap to -1 as i64 and look elapsed; it must be rejected
        // explicitly and fall back to the bounded backoff.
        let server = MockServer::start().await;
        let error_hits = Arc::new(AtomicUsize::new(0));
        let success_hits = Arc::new(AtomicUsize::new(0));
        mount_flaky_server_error(
            &server,
            1,
            ResponseTemplate::new(503).insert_header("x-ratelimit-reset", u64::MAX.to_string()),
            Arc::clone(&error_hits),
            Arc::clone(&success_hits),
        )
        .await;

        let provider = MusicBrainzProvider::for_test(server.uri(), Duration::ZERO).unwrap();
        let task =
            tokio::spawn(async move { provider.request_json::<Value>("/throttled", &[]).await });
        spin_until(
            || error_hits.load(Ordering::SeqCst) == 1,
            "first request answered with 503",
        )
        .await;

        let advanced = advance_until_by(
            || success_hits.load(Ordering::SeqCst) == 1 || task.is_finished(),
            Duration::from_secs(5),
            Duration::from_millis(250),
        )
        .await;
        assert!(
            advanced >= Duration::from_secs(1),
            "an out-of-range reset must fall back to the backoff, waited {advanced:?}"
        );
        assert!(
            advanced <= Duration::from_millis(1500),
            "an out-of-range reset must use the one-second backoff, waited {advanced:?}"
        );
        assert_eq!(task.await.unwrap().unwrap()["ok"], json!(true));
    }

    #[tokio::test(start_paused = true)]
    async fn server_error_names_an_unusable_reset() {
        // The reset is dropped as unusable, so the operator must still see the
        // window the server named instead of a silent fallback.
        let server = MockServer::start().await;
        let hits = Arc::new(AtomicUsize::new(0));
        Mock::given(method("GET"))
            .and(path("/ws/2/artist"))
            .respond_with(OffsetResetResponder {
                offset_secs: 90_000,
                additional_headers: Vec::new(),
                hits: Arc::clone(&hits),
            })
            .expect(3)
            .mount(&server)
            .await;

        let provider = MusicBrainzProvider::for_test(server.uri(), Duration::ZERO).unwrap();
        let task = tokio::spawn(async move { provider.search_artists("Nobody").await });
        advance_until(|| task.is_finished(), Duration::from_secs(12)).await;

        let message = task.await.unwrap().unwrap_err().to_string();
        assert!(
            message.contains("x-ratelimit-reset") && message.contains("unusable"),
            "an unusable reset must be named, got {message}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn server_error_retries_then_succeeds() {
        let server = MockServer::start().await;
        let error_hits = Arc::new(AtomicUsize::new(0));
        Mock::given(method("GET"))
            .and(path("/flaky"))
            .respond_with(CountingResponder::new(
                ResponseTemplate::new(500),
                Arc::clone(&error_hits),
            ))
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;
        let success_hits = Arc::new(AtomicUsize::new(0));
        Mock::given(method("GET"))
            .and(path("/flaky"))
            .respond_with(CountingResponder::new(
                ResponseTemplate::new(200).set_body_json(json!({"ok": true})),
                Arc::clone(&success_hits),
            ))
            .expect(1)
            .mount(&server)
            .await;

        let provider = MusicBrainzProvider::for_test(server.uri(), Duration::ZERO).unwrap();
        let task = tokio::spawn(async move { provider.request_json::<Value>("/flaky", &[]).await });
        spin_until(
            || error_hits.load(Ordering::SeqCst) == 1,
            "first request answered with 500",
        )
        .await;
        assert_eq!(
            success_hits.load(Ordering::SeqCst),
            0,
            "the retry must not start before the one-second backoff"
        );

        // Same whole-second advance loop as the Retry-After test: the 500
        // backoff sleep fires on the first advance past its deadline, so the
        // retry lands within a bounded number of advances.
        let mut advanced = Duration::ZERO;
        while success_hits.load(Ordering::SeqCst) == 0 && !task.is_finished() {
            advanced += Duration::from_secs(1);
            assert!(
                advanced <= Duration::from_secs(6),
                "the 500 retry never landed after {advanced:?}"
            );
            tokio::time::advance(Duration::from_secs(1)).await;
            for _ in 0..10_000 {
                tokio::task::yield_now().await;
            }
        }
        assert_eq!(success_hits.load(Ordering::SeqCst), 1);
        assert!(
            advanced >= Duration::from_secs(1),
            "the 500 retry must wait at least one second, waited {advanced:?}"
        );
        spin_until(|| task.is_finished(), "retried request completion").await;
        let value = task.await.unwrap().unwrap();
        assert_eq!(value["ok"], json!(true));
    }

    #[tokio::test]
    async fn artist_search_over_one_page_is_unresolved() {
        let server = MockServer::start().await;
        let artists: Vec<Value> = (0..100)
            .map(|index| {
                json!({
                    "id": format!("artist-{index}"),
                    "name": format!("Artist {index}")
                })
            })
            .collect();
        Mock::given(method("GET"))
            .and(path("/ws/2/artist"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "count": 101,
                "offset": 0,
                "artists": artists
            })))
            .expect(1)
            .mount(&server)
            .await;

        let provider = MusicBrainzProvider::for_test(server.uri(), Duration::ZERO).unwrap();
        // An artist whose search matches exceed one page is answered with a page
        // whose reported total is larger than the candidates it carries. That has
        // to stay an artist problem rather than an outage, because falling back is
        // the pre-existing behaviour, but it describes one response and must never
        // be remembered by the resolution-failure cache.
        let error = provider.search_artists("Artist").await.unwrap_err();
        assert!(matches!(
            error,
            DiscographyError::ArtistSearchInconsistent(_)
        ));
        assert!(
            !error.is_stable_artist_resolution_failure(),
            "a page-length mismatch must not be cacheable"
        );
    }

    #[tokio::test]
    async fn artist_search_with_a_non_zero_offset_is_a_provider_failure() {
        // The request never sends an offset, so a non-zero one means the
        // provider answered with the wrong page. It must be classed as a
        // provider failure, or the discover circuit breaker could never trip on
        // a misbehaving provider and the run would grind on.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ws/2/artist"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "count": 1,
                "offset": 5,
                "artists": [{
                    "id": "artist-0",
                    "name": "Artist"
                }]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let provider = MusicBrainzProvider::for_test(server.uri(), Duration::ZERO).unwrap();
        assert!(matches!(
            provider.search_artists("Artist").await,
            Err(DiscographyError::IncompletePagination(_))
        ));
    }

    #[tokio::test]
    async fn configured_id_fetches_canonical_artist() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ws/2/artist/11111111-1111-1111-1111-111111111111"))
            .and(query_param("fmt", "json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "11111111-1111-1111-1111-111111111111",
                "name": "Canonical Artist"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let provider = MusicBrainzProvider::for_test(server.uri(), Duration::ZERO).unwrap();
        let artist = provider
            .artist_by_id("11111111-1111-1111-1111-111111111111")
            .await
            .unwrap();
        assert_eq!(artist.name, "Canonical Artist");
        assert_eq!(artist.id, "11111111-1111-1111-1111-111111111111");
    }

    #[tokio::test]
    async fn not_found_is_not_retried() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ws/2/artist"))
            .respond_with(ResponseTemplate::new(404))
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;

        let provider = MusicBrainzProvider::for_test(server.uri(), Duration::ZERO).unwrap();
        let result = provider.search_artists("Nobody").await;
        assert!(matches!(result, Err(DiscographyError::HttpStatus(404))));
    }

    #[tokio::test]
    async fn malformed_json_is_rejected() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ws/2/artist"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not json {{"))
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;

        let provider = MusicBrainzProvider::for_test(server.uri(), Duration::ZERO).unwrap();
        let result = provider.search_artists("Nobody").await;
        assert!(matches!(result, Err(DiscographyError::Decode(_))));
    }

    #[tokio::test(start_paused = true)]
    async fn request_timeout_is_bounded() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ws/2/artist"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_secs(11))
                    .set_body_json(json!({"count": 0, "offset": 0, "artists": []})),
            )
            .expect(3)
            .mount(&server)
            .await;

        let provider = MusicBrainzProvider::for_test(server.uri(), Duration::ZERO).unwrap();
        let task = tokio::spawn(async move { provider.search_artists("Nobody").await });
        let mut advanced = Duration::ZERO;
        while !task.is_finished() {
            advanced += Duration::from_secs(1);
            assert!(
                advanced <= Duration::from_secs(40),
                "request timeout retries exceeded the 40-second virtual bound"
            );
            tokio::time::advance(Duration::from_secs(1)).await;
            for _ in 0..10_000 {
                tokio::task::yield_now().await;
            }
        }

        assert!(matches!(
            task.await.unwrap(),
            Err(DiscographyError::Transport(_))
        ));
        assert!(
            advanced >= Duration::from_secs(30),
            "three ten-second attempts must consume at least 30 virtual seconds"
        );
    }

    #[tokio::test]
    async fn oversized_chunked_body_is_rejected() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ws/2/artist"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string("x".repeat(MAX_RESPONSE_BYTES + 1)),
            )
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;

        let provider = MusicBrainzProvider::for_test(server.uri(), Duration::ZERO).unwrap();
        let result = provider.search_artists("Nobody").await;
        assert!(matches!(result, Err(DiscographyError::ResponseTooLarge)));
    }

    #[tokio::test]
    async fn changing_page_count_is_incomplete() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ws/2/release-group"))
            .and(query_param("offset", "0"))
            .respond_with(ResponseTemplate::new(200).set_body_json(release_page(101, 0, 100)))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/ws/2/release-group"))
            .and(query_param("offset", "100"))
            .respond_with(ResponseTemplate::new(200).set_body_json(release_page(102, 100, 1)))
            .expect(1)
            .mount(&server)
            .await;

        let provider = MusicBrainzProvider::for_test(server.uri(), Duration::ZERO).unwrap();
        let result = provider
            .release_groups("11111111-1111-1111-1111-111111111111")
            .await;
        assert!(matches!(
            result,
            Err(DiscographyError::IncompletePagination(_))
        ));
    }

    #[tokio::test]
    async fn duplicate_release_group_ids_are_incomplete() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ws/2/release-group"))
            .and(query_param("offset", "0"))
            .respond_with(ResponseTemplate::new(200).set_body_json(release_page(101, 0, 100)))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/ws/2/release-group"))
            .and(query_param("offset", "100"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "release-group-count": 101,
                "release-group-offset": 100,
                "release-groups": [{
                    "id": "group-0",
                    "title": "Duplicate",
                    "first-release-date": "2001",
                    "primary-type": "Album",
                    "secondary-types": []
                }]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let provider = MusicBrainzProvider::for_test(server.uri(), Duration::ZERO).unwrap();
        assert!(matches!(
            provider
                .release_groups("11111111-1111-1111-1111-111111111111")
                .await,
            Err(DiscographyError::IncompletePagination(_))
        ));
    }

    #[tokio::test]
    async fn wrong_page_offset_is_incomplete() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ws/2/release-group"))
            .and(query_param("offset", "0"))
            .respond_with(ResponseTemplate::new(200).set_body_json(release_page(101, 0, 100)))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/ws/2/release-group"))
            .and(query_param("offset", "100"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "release-group-count": 101,
                "release-group-offset": 0,
                "release-groups": [{
                    "id": "group-100",
                    "title": "Album 100",
                    "first-release-date": "2000",
                    "primary-type": "Album",
                    "secondary-types": []
                }]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let provider = MusicBrainzProvider::for_test(server.uri(), Duration::ZERO).unwrap();
        let result = provider
            .release_groups("11111111-1111-1111-1111-111111111111")
            .await;
        assert!(matches!(
            result,
            Err(DiscographyError::IncompletePagination(_))
        ));
    }

    #[tokio::test]
    async fn empty_intermediate_page_is_incomplete() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ws/2/release-group"))
            .and(query_param("offset", "0"))
            .respond_with(ResponseTemplate::new(200).set_body_json(release_page(101, 0, 0)))
            .expect(1)
            .mount(&server)
            .await;

        let provider = MusicBrainzProvider::for_test(server.uri(), Duration::ZERO).unwrap();
        let result = provider
            .release_groups("11111111-1111-1111-1111-111111111111")
            .await;
        assert!(matches!(
            result,
            Err(DiscographyError::IncompletePagination(_))
        ));
    }

    #[tokio::test]
    async fn short_release_group_page_is_incomplete() {
        let server = MockServer::start().await;
        let hits = Arc::new(AtomicUsize::new(0));
        Mock::given(method("GET"))
            .and(path("/ws/2/release-group"))
            .respond_with(ShortPageResponder {
                hits: Arc::clone(&hits),
            })
            .mount(&server)
            .await;

        let provider = MusicBrainzProvider::for_test(server.uri(), Duration::ZERO).unwrap();
        let result = provider
            .release_groups("11111111-1111-1111-1111-111111111111")
            .await;

        assert!(matches!(
            result,
            Err(DiscographyError::IncompletePagination(_))
        ));
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "a short first page must fail before requesting another offset"
        );
    }

    #[tokio::test]
    async fn artist_search_count_must_match_payload() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ws/2/artist"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "count": 0,
                "offset": 0,
                "artists": [{
                    "id": "11111111-1111-1111-1111-111111111111",
                    "name": "Unexpected Artist"
                }]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let provider = MusicBrainzProvider::for_test(server.uri(), Duration::ZERO).unwrap();
        let result = provider.search_artists("Unexpected Artist").await;

        assert!(matches!(
            result,
            Err(DiscographyError::ArtistSearchInconsistent(_))
        ));
    }

    #[tokio::test]
    async fn excessive_release_count_is_incomplete() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ws/2/release-group"))
            .and(query_param("offset", "0"))
            .respond_with(ResponseTemplate::new(200).set_body_json(release_page(10_001, 0, 100)))
            .expect(1)
            .mount(&server)
            .await;

        let provider = MusicBrainzProvider::for_test(server.uri(), Duration::ZERO).unwrap();
        let result = provider
            .release_groups("11111111-1111-1111-1111-111111111111")
            .await;
        assert!(matches!(
            result,
            Err(DiscographyError::IncompletePagination(_))
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn transport_exhaustion_is_bounded() {
        // Point the provider at a loopback port with no listener, so every
        // attempt fails fast and never reaches the live MusicBrainz service.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let provider =
            MusicBrainzProvider::for_test(format!("http://127.0.0.1:{port}"), Duration::ZERO)
                .unwrap();
        let start = tokio::time::Instant::now();
        let task =
            tokio::spawn(async move { provider.request_json::<Value>("/ws/2/artist", &[]).await });
        // Drive the clock in whole seconds with long spin windows: the
        // transport backoff sleeps after attempts one (1s) and two (2s) only
        // fire through explicit advances, and every error+arm step completes
        // inside a window, so the third attempt finishes within 5s.
        let mut advanced = Duration::ZERO;
        loop {
            if task.is_finished() {
                break;
            }
            advanced += Duration::from_secs(1);
            assert!(
                advanced < Duration::from_secs(5),
                "transport exhaustion never completed after {advanced:?}"
            );
            tokio::time::advance(Duration::from_secs(1)).await;
            for _ in 0..10_000 {
                tokio::task::yield_now().await;
            }
        }
        let result = task.await.unwrap();
        assert!(matches!(result, Err(DiscographyError::Transport(_))));
        // Backoff advances of one then two seconds: the two sleeps alone total
        // 3s and each error+arm step can land on a whole-second advance, so
        // three attempts finish in [3s, 5s); two attempts would finish under
        // 2s and a fourth attempt would add another two seconds.
        assert!(
            advanced >= Duration::from_secs(3) && advanced < Duration::from_secs(5),
            "expected three attempts with 1s then 2s backoffs, finished after {advanced:?}"
        );
        assert_eq!(start.elapsed(), advanced);
    }

    #[tokio::test]
    async fn artist_search_retains_scores_and_tolerates_missing_or_string_scores() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ws/2/artist"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "count": 5,
                "offset": 0,
                "artists": [
                    { "id": "16b97aaa-d7c0-469f-8c97-47c705b2d02f", "name": "Ils", "score": 100 },
                    { "id": "638e9183-2cde-4c07-b1d5-1f0e0361ed1c", "name": "Ils", "score": "86" },
                    { "id": "cc54a811-a221-49f1-b93d-ed42f1affbb0", "name": "Ils", "score": 83.0 },
                    { "id": "5323e64e-008f-4b5c-affc-f410b3746908", "name": "Ils", "score": null },
                    { "id": "0dd756ed-e253-45e6-b95f-dc08ac87018d", "name": "Ils" }
                ]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let provider = MusicBrainzProvider::for_test(server.uri(), Duration::ZERO).unwrap();
        let artists = provider.search_artists("Ils").await.unwrap();

        assert_eq!(artists[0].score, Some(100));
        assert_eq!(artists[1].score, Some(86));
        assert_eq!(artists[2].score, Some(83));
        assert_eq!(artists[3].score, None);
        assert_eq!(artists[4].score, None);
    }

    #[tokio::test]
    async fn artist_search_rejects_out_of_range_and_non_numeric_scores() {
        for invalid in [
            serde_json::json!(101),
            serde_json::json!(-1),
            serde_json::json!(86.5),
            serde_json::json!("high"),
        ] {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/ws/2/artist"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "count": 1,
                    "offset": 0,
                    "artists": [
                        { "id": "16b97aaa-d7c0-469f-8c97-47c705b2d02f", "name": "Ils", "score": invalid }
                    ]
                })))
                .expect(1)
                .mount(&server)
                .await;

            let provider = MusicBrainzProvider::for_test(server.uri(), Duration::ZERO).unwrap();
            assert!(
                matches!(
                    provider.search_artists("Ils").await,
                    Err(DiscographyError::Decode(_))
                ),
                "score {invalid} must fail decoding rather than be clamped or ignored"
            );
        }
    }

    #[tokio::test]
    async fn artist_search_rejects_a_mixed_page_with_one_invalid_score() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ws/2/artist"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "count": 2,
                "offset": 0,
                "artists": [
                    { "id": "16b97aaa-d7c0-469f-8c97-47c705b2d02f", "name": "Ils", "score": 100 },
                    { "id": "not-an-exact-match", "name": "Somebody Else", "score": "high" }
                ]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let provider = MusicBrainzProvider::for_test(server.uri(), Duration::ZERO).unwrap();
        assert!(matches!(
            provider.search_artists("Ils").await,
            Err(DiscographyError::Decode(_))
        ));
    }

    #[tokio::test]
    async fn code_search_rejects_a_count_that_disagrees_with_the_payload() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ws/2/artist"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "count": 3,
                "offset": 0,
                "artists": [{ "id": "11111111-1111-1111-1111-111111111111", "name": "Grafix" }]
            })))
            .mount(&server)
            .await;

        let provider = MusicBrainzProvider::for_test(server.uri(), Duration::ZERO).unwrap();
        let error = provider
            .search_artists_by_code(CodeIdentifier::Ipi("00581977202"))
            .await
            .unwrap_err();

        assert!(
            matches!(error, DiscographyError::ArtistSearchInconsistent(_)),
            "a count that disagrees with the payload describes one response, got {error:?}"
        );
    }
}
