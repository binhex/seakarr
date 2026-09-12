use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;

use super::{ArtistCandidate, DiscographyError, DiscographyProvider, ReleaseGroup};

const MUSICBRAINZ_BASE_URL: &str = "https://musicbrainz.org";
const REQUEST_INTERVAL: Duration = Duration::from_secs(1);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_RETRY_AFTER: u64 = 30;
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
    /// Transport and 5xx failures back off one then two seconds; a 429 waits
    /// its integer `Retry-After` (rejecting malformed values or values above
    /// 30 seconds); other 4xx statuses and decode failures return at once.
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
                        if attempt == 2 {
                            return Err(DiscographyError::HttpStatus(status.as_u16()));
                        }
                        tokio::time::sleep(backoff(attempt)).await;
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
        if page.offset != 0 {
            return Err(DiscographyError::ArtistUnresolved(format!(
                "artist search returned a non-zero offset {}",
                page.offset
            )));
        }
        if page.count != page.artists.len() {
            return Err(DiscographyError::ArtistUnresolved(format!(
                "artist search returned {} candidates but reported {}",
                page.artists.len(),
                page.count
            )));
        }
        Ok(page
            .artists
            .into_iter()
            .map(|wire| ArtistCandidate {
                id: wire.id,
                name: wire.name,
            })
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
        Ok(ArtistCandidate {
            id: wire.id,
            name: wire.name,
        })
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
            super::super::resolve_exact_artist("AC DC", &artists).is_err(),
            "sort names, aliases, and scores must not widen canonical-name matching"
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
    async fn exhausted_server_errors_return_last_status() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ws/2/artist"))
            .respond_with(ResponseTemplate::new(503))
            .expect(3)
            .mount(&server)
            .await;

        let provider = MusicBrainzProvider::for_test(server.uri(), Duration::ZERO).unwrap();
        let task = tokio::spawn(async move { provider.search_artists("Nobody").await });
        for _ in 0..6 {
            if task.is_finished() {
                break;
            }
            tokio::time::advance(Duration::from_secs(1)).await;
            for _ in 0..10_000 {
                tokio::task::yield_now().await;
            }
        }

        assert!(matches!(
            task.await.unwrap(),
            Err(DiscographyError::HttpStatus(503))
        ));
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
        assert!(matches!(
            provider.search_artists("Artist").await,
            Err(DiscographyError::ArtistUnresolved(_))
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

        assert!(matches!(result, Err(DiscographyError::ArtistUnresolved(_))));
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
}
