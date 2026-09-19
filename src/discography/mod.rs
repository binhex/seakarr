use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use unicode_normalization::UnicodeNormalization;

use crate::config::DiscographyConfig;
use crate::config::DiscographyReleaseType;
use crate::db::{Database, DiscographyCacheEntry, DiscographyFailureEntry};

mod bundle;
mod musicbrainz;
pub use musicbrainz::MusicBrainzProvider;

pub(crate) fn normalize_catalog_key(value: &str) -> String {
    value
        .nfkc()
        .flat_map(char::to_lowercase)
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Comparison key for library album presence, built from the name the library
/// write path would store rather than from the raw title.
///
/// The album folder on disk has been through
/// [`crate::organizer::sanitize_component`], but the album tag the scanner
/// prefers, and the MusicBrainz title the presence check is made against, have
/// not. Sanitising both sides keeps a stored album equal to the title it was
/// written from; comparing raw titles would make an album whose title carried a
/// character the filesystem cannot hold permanently "missing", and discover
/// would download it again on every cycle.
///
/// Artist identity deliberately does not use this: it keeps
/// [`normalize_catalog_key`] and its significant-punctuation contract.
///
/// The compatibility fold runs BEFORE the sanitiser so that a width variant of a
/// reserved character reaches it as the reserved character itself. Folding
/// afterwards would leave a library tagged `Tronic Jazz：` (U+FF1A) keyed on
/// `tronic jazz:` while the MusicBrainz spelling keys on
/// `tronic jazz the berlin sessions`, and the album would be re-downloaded every
/// cycle.
pub(crate) fn normalize_album_key(value: &str) -> String {
    let folded: String = value.nfkc().collect();
    normalize_catalog_key(&crate::organizer::sanitize_component(&folded))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtistCandidate {
    pub id: String,
    pub name: String,
    /// MusicBrainz artist-search relevance score in the documented 0-100
    /// range. `None` when the response omitted a score, which is normal for
    /// lookups by MBID and for a single unambiguous search result.
    pub score: Option<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseGroup {
    pub id: String,
    pub title: String,
    pub first_release_date: Option<String>,
    pub primary_type: Option<String>,
    pub secondary_types: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlbumTarget {
    pub release_group_id: String,
    pub title: String,
}

#[derive(Debug, Error)]
pub enum DiscographyError {
    #[error("MusicBrainz transport error: {0}")]
    Transport(String),
    #[error("MusicBrainz returned HTTP {0}")]
    HttpStatus(u16),
    #[error("MusicBrainz returned an invalid Retry-After header")]
    InvalidRetryAfter,
    #[error("MusicBrainz response exceeded 4 MiB")]
    ResponseTooLarge,
    #[error("MusicBrainz response was invalid: {0}")]
    Decode(String),
    #[error("MusicBrainz pagination was incomplete: {0}")]
    IncompletePagination(String),
    #[error("artist could not be resolved safely: {0}")]
    ArtistUnresolved(String),
    /// The artist search answered with a page whose reported total disagrees
    /// with the candidates it carried, so the exact-name match cannot be proven
    /// from the candidates in hand. This is a property of one HTTP response
    /// rather than a fact about the artist name, which is why it stays out of
    /// the resolution-failure cache: a single inconsistent response must not
    /// suppress the artist for days. It is still classified as an artist
    /// problem for the circuit breaker and the legacy fallback, so the run
    /// behaves exactly as it did before the failure cache existed.
    #[error("artist search was inconsistent: {0}")]
    ArtistSearchInconsistent(String),
    /// The provider answered, but its candidate data was unusable: a search
    /// score outside the documented range, or a missing score where one is
    /// required. This is a provider defect, not an artist that genuinely cannot
    /// be resolved, so it must not be reported as an unresolved artist where it
    /// would hide a MusicBrainz outage from the circuit breaker.
    #[error("MusicBrainz returned unusable artist candidate data: {0}")]
    InvalidCandidateData(String),
}

#[async_trait]
pub trait DiscographyProvider: Send + Sync {
    async fn search_artists(
        &self,
        artist: &str,
    ) -> std::result::Result<Vec<ArtistCandidate>, DiscographyError>;

    async fn artist_by_id(
        &self,
        artist_mbid: &str,
    ) -> std::result::Result<ArtistCandidate, DiscographyError>;

    async fn release_groups(
        &self,
        artist_mbid: &str,
    ) -> std::result::Result<Vec<ReleaseGroup>, DiscographyError>;
}

/// MusicBrainz search score a duplicate canonical exact-name candidate must
/// reach before it may be selected automatically.
const REQUIRED_TOP_SCORE: u8 = 100;

/// Minimum lead over the runner-up canonical exact-name candidate.
const REQUIRED_SCORE_MARGIN: u8 = 10;

/// Evidence that duplicate canonical exact-name candidates were resolved by
/// MusicBrainz search-score dominance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DominanceEvidence {
    pub exact_matches: usize,
    pub top_score: u8,
    pub runner_up_score: u8,
    pub margin: u8,
}

/// Why an artist candidate was accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArtistResolution {
    /// Exactly one candidate matched the requested canonical name.
    UniqueExactName,
    /// Duplicate exact names resolved by a dominant search score.
    ScoreDominance(DominanceEvidence),
}

/// A resolved artist and the reason it was accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedArtist {
    pub candidate: ArtistCandidate,
    pub resolution: ArtistResolution,
}

/// Resolve the artist whose canonical name normalizes to `artist_key`.
///
/// One canonical exact-name match is accepted as before. Duplicate exact-name
/// matches are accepted only when every candidate carries a score, the
/// canonical names compare equal after `normalize_catalog_key`, exactly one
/// candidate holds the highest score, that score is `REQUIRED_TOP_SCORE`, and
/// it leads the runner-up by at least `REQUIRED_SCORE_MARGIN`.
///
/// Aliases, sort names, and descriptive metadata never widen eligibility, and
/// provider response order never decides the winner. A score above 100 is
/// rejected rather than clamped, because clamping would invent provider data.
pub fn resolve_artist(
    artist_key: &str,
    candidates: &[ArtistCandidate],
) -> std::result::Result<ResolvedArtist, DiscographyError> {
    let key = normalize_catalog_key(artist_key);
    let matches: Vec<&ArtistCandidate> = candidates
        .iter()
        .filter(|candidate| normalize_catalog_key(&candidate.name) == key)
        .collect();
    match matches.as_slice() {
        [] => Err(DiscographyError::ArtistUnresolved(format!(
            "no candidate matches {artist_key:?}"
        ))),
        [only] => Ok(ResolvedArtist {
            candidate: (*only).clone(),
            resolution: ArtistResolution::UniqueExactName,
        }),
        many => resolve_dominant_artist(artist_key, many),
    }
}

/// Apply the dominance rules to two or more canonical exact-name matches.
fn resolve_dominant_artist(
    artist_key: &str,
    matches: &[&ArtistCandidate],
) -> std::result::Result<ResolvedArtist, DiscographyError> {
    let exact_matches = matches.len();
    // Reject invalid provider data before absent data, and scan the whole set
    // rather than returning on the first offending candidate: the reported
    // reason, including the quoted invalid value, must not depend on the order
    // MusicBrainz returned candidates in.
    if let Some(invalid) = matches
        .iter()
        .filter_map(|candidate| candidate.score)
        .filter(|score| *score > REQUIRED_TOP_SCORE)
        .max()
    {
        return Err(DiscographyError::InvalidCandidateData(format!(
            "{exact_matches} candidates match {artist_key:?} and one reports the invalid score {invalid}"
        )));
    }
    if matches.iter().any(|candidate| candidate.score.is_none()) {
        return Err(DiscographyError::InvalidCandidateData(format!(
            "{exact_matches} candidates match {artist_key:?} and at least one has no search score"
        )));
    }
    // Both defect classes were rejected above, so every score is present and in
    // range here.
    let mut scored: Vec<(u8, &ArtistCandidate)> = matches
        .iter()
        .filter_map(|candidate| candidate.score.map(|score| (score, *candidate)))
        .collect();
    debug_assert_eq!(scored.len(), exact_matches);
    // Sort by score, then MBID, so the comparison never depends on the order
    // MusicBrainz returned the candidates in.
    scored.sort_by(|left, right| {
        right
            .0
            .cmp(&left.0)
            .then_with(|| left.1.id.cmp(&right.1.id))
    });
    let (top_score, top) = scored[0];
    let leaders = scored
        .iter()
        .filter(|(score, _)| *score == top_score)
        .count();
    if leaders != 1 {
        return Err(DiscographyError::ArtistUnresolved(format!(
            "{exact_matches} candidates match {artist_key:?} with a tied top score of {top_score}"
        )));
    }
    if top_score < REQUIRED_TOP_SCORE {
        return Err(DiscographyError::ArtistUnresolved(format!(
            "{exact_matches} candidates match {artist_key:?} and the highest score {top_score} is below {REQUIRED_TOP_SCORE}"
        )));
    }
    let runner_up_score = scored[1].0;
    let margin = top_score - runner_up_score;
    if margin < REQUIRED_SCORE_MARGIN {
        return Err(DiscographyError::ArtistUnresolved(format!(
            "{exact_matches} candidates match {artist_key:?} and the leading margin {margin} is below {REQUIRED_SCORE_MARGIN}"
        )));
    }
    Ok(ResolvedArtist {
        candidate: top.clone(),
        resolution: ArtistResolution::ScoreDominance(DominanceEvidence {
            exact_matches,
            top_score,
            runner_up_score,
            margin,
        }),
    })
}

fn secondary_category(value: &str) -> Option<DiscographyReleaseType> {
    match value.trim().to_ascii_lowercase().as_str() {
        "live" => Some(DiscographyReleaseType::LiveAlbum),
        "compilation" => Some(DiscographyReleaseType::Compilation),
        "remix" => Some(DiscographyReleaseType::Remix),
        "soundtrack" => Some(DiscographyReleaseType::Soundtrack),
        "dj-mix" => Some(DiscographyReleaseType::DjMix),
        "mixtape/street" => Some(DiscographyReleaseType::Mixtape),
        _ => None,
    }
}

/// Log why a release group was excluded, so a debug run explains a gap.
///
/// Returns nothing on purpose: the caller decides the verdict, which keeps the
/// rejection path readable instead of hiding a `false` in a boolean-returning
/// helper.
fn log_rejected_release(release: &ReleaseGroup, reason: &str) {
    tracing::debug!(
        release_group_id = %release.id,
        title = %release.title,
        reason,
        "excluding MusicBrainz release group"
    );
}

/// Classify a release group against the configured friendly categories.
/// Unknown primary or secondary types are rejected; secondary types must all
/// be allowed; `Album` without secondaries requires `StudioAlbum`.
pub fn release_allowed(release: &ReleaseGroup, allowed: &[DiscographyReleaseType]) -> bool {
    let Some(primary) = release.primary_type.as_deref() else {
        log_rejected_release(release, "missing primary type");
        return false;
    };
    let primary = primary.trim().to_ascii_lowercase();
    if primary != "album" && primary != "ep" && primary != "single" {
        log_rejected_release(release, &format!("unknown primary type {primary}"));
        return false;
    }
    let mut secondaries = Vec::new();
    for value in &release.secondary_types {
        let Some(category) = secondary_category(value) else {
            log_rejected_release(release, &format!("unknown secondary type {}", value.trim()));
            return false;
        };
        secondaries.push(category);
    }
    let all_secondaries_allowed = |secondaries: &[DiscographyReleaseType]| {
        secondaries
            .iter()
            .all(|secondary| allowed.contains(secondary))
    };
    let is_allowed = match primary.as_str() {
        "album" => {
            if secondaries.is_empty() {
                allowed.contains(&DiscographyReleaseType::StudioAlbum)
            } else {
                all_secondaries_allowed(&secondaries)
            }
        }
        "ep" => {
            allowed.contains(&DiscographyReleaseType::Ep) && all_secondaries_allowed(&secondaries)
        }
        "single" => {
            allowed.contains(&DiscographyReleaseType::Single)
                && all_secondaries_allowed(&secondaries)
        }
        _ => false,
    };
    if is_allowed {
        true
    } else {
        log_rejected_release(release, "release type is not enabled");
        false
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct PartialDate {
    year: i32,
    month: u32,
    day: u32,
}

/// Parse `YYYY`, `YYYY-MM`, or `YYYY-MM-DD`. Year must be 1..=9999, month
/// 1..=12 when present, and complete dates must be real calendar dates.
/// Missing month/day sort as zero.
fn parse_partial_date(value: &str) -> Option<PartialDate> {
    let mut parts = value.trim().split('-');
    let year: i32 = parts.next()?.parse().ok()?;
    if !(1..=9999).contains(&year) {
        return None;
    }
    let month = match parts.next() {
        None => 0,
        Some(part) => {
            let month: u32 = part.parse().ok()?;
            if !(1..=12).contains(&month) {
                return None;
            }
            month
        }
    };
    let day = match parts.next() {
        None => 0,
        Some(part) => part.parse().ok()?,
    };
    if parts.next().is_some() {
        return None;
    }
    match (month, day) {
        (0, 0) => Some(PartialDate {
            year,
            month: 0,
            day: 0,
        }),
        (month, 0) => Some(PartialDate {
            year,
            month,
            day: 0,
        }),
        (0, _) => None,
        (month, day) => {
            chrono::NaiveDate::from_ymd_opt(year, month, day)?;
            Some(PartialDate { year, month, day })
        }
    }
}

/// Filter allowed release groups with non-empty titles, drop any whose title
/// bundles other release groups of the same artist, deduplicate by the
/// normalized title key (prefer dated over undated, then earlier date, then
/// lexicographically smaller MBID), and order dated targets by partial date
/// before undated targets.
pub fn select_albums(
    groups: &[ReleaseGroup],
    allowed: &[DiscographyReleaseType],
) -> Vec<AlbumTarget> {
    #[derive(Clone)]
    struct Candidate {
        date: Option<PartialDate>,
        target: AlbumTarget,
    }

    // Every release-group title the artist has, before the release-type filter
    // runs, so a bundle part counts as known even when its own release group is
    // not selectable under the configured categories.
    let artist_titles: std::collections::BTreeSet<String> = groups
        .iter()
        .map(|group| normalize_catalog_key(&group.title))
        .filter(|key| !key.is_empty())
        .collect();

    let mut best: std::collections::BTreeMap<String, Candidate> = std::collections::BTreeMap::new();
    for release in groups {
        if !release_allowed(release, allowed) {
            continue;
        }
        let title = release.title.trim();
        if title.is_empty() {
            tracing::debug!(
                release_group_id = %release.id,
                "excluding MusicBrainz release group: empty title"
            );
            continue;
        }
        if bundle::is_bundle(title, &artist_titles) {
            log_rejected_release(release, "title bundles other release groups");
            continue;
        }
        let key = normalize_catalog_key(title);
        let candidate = Candidate {
            date: release
                .first_release_date
                .as_deref()
                .and_then(parse_partial_date),
            target: AlbumTarget {
                release_group_id: release.id.clone(),
                title: title.to_owned(),
            },
        };
        let better =
            |existing: &Candidate, candidate: &Candidate| match (existing.date, candidate.date) {
                (Some(_), None) => false,
                (None, Some(_)) => true,
                (Some(a), Some(b)) => {
                    b < a
                        || (b == a
                            && candidate.target.release_group_id < existing.target.release_group_id)
                }
                (None, None) => {
                    candidate.target.release_group_id < existing.target.release_group_id
                }
            };
        match best.get_mut(&key) {
            Some(existing) => {
                if better(existing, &candidate) {
                    *existing = candidate;
                }
            }
            None => {
                best.insert(key, candidate);
            }
        }
    }

    let mut dated: Vec<Candidate> = best
        .values()
        .filter(|candidate| candidate.date.is_some())
        .cloned()
        .collect();
    let mut undated: Vec<Candidate> = best
        .values()
        .filter(|candidate| candidate.date.is_none())
        .cloned()
        .collect();
    dated.sort_by(|a, b| {
        a.date
            .cmp(&b.date)
            .then_with(|| {
                normalize_catalog_key(&a.target.title).cmp(&normalize_catalog_key(&b.target.title))
            })
            .then_with(|| a.target.release_group_id.cmp(&b.target.release_group_id))
    });
    undated.sort_by(|a, b| {
        normalize_catalog_key(&a.target.title)
            .cmp(&normalize_catalog_key(&b.target.title))
            .then_with(|| a.target.release_group_id.cmp(&b.target.release_group_id))
    });
    dated
        .into_iter()
        .chain(undated)
        .map(|candidate| candidate.target)
        .collect()
}

pub const SECONDS_PER_DAY: u64 = 86_400;
pub const MAX_CACHE_PAYLOAD_BYTES: usize = 16 * 1024 * 1024;

/// The only failure kind written to the failure cache. Anything else is treated
/// as a miss, so a future kind cannot suppress a lookup it was not written for.
const FAILURE_KIND_UNRESOLVED: &str = "unresolved";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscoveryProvenance {
    FreshCache,
    Refreshed,
    StaleCache {
        age_days: u64,
        refresh_error: String,
        /// Why the refresh failed. Kept so callers can tell a genuine outage
        /// from a refresh that merely stopped resolving the artist: only the
        /// former is an outage for circuit-breaker purposes.
        kind: DiscoveryFailure,
    },
}

/// Why authoritative discovery could not supply albums.
///
/// The distinction matters to callers that must not treat an unresolvable
/// artist like an outage: `discover` aborts after consecutive
/// [`DiscoveryFailure::Provider`] failures but keeps going past any number of
/// [`DiscoveryFailure::Unresolved`] artists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscoveryFailure {
    /// The artist could not be resolved to exactly one MusicBrainz candidate.
    Unresolved,
    /// MusicBrainz could not be reached, or returned unusable data.
    Provider,
}

impl DiscographyError {
    /// True when this failure means the artist name could not be matched or
    /// disambiguated, as opposed to a transport, HTTP, decode, pagination or
    /// unusable-data failure, and is therefore worth remembering across runs.
    ///
    /// Only [`DiscographyError::ArtistUnresolved`] qualifies. Its sibling
    /// [`DiscographyError::ArtistSearchInconsistent`] is deliberately classified
    /// as an artist problem for the circuit breaker and the legacy fallback, but
    /// a page whose reported total disagrees with its payload describes one
    /// inconsistent response, so caching it would suppress a healthy artist
    /// until the expiry elapsed.
    ///
    /// The match is not entirely response-independent: three of the
    /// `ArtistUnresolved` producers decide from MusicBrainz's per-response search
    /// scores — a tied top score, a top score below the required threshold, and a
    /// leading margin below the required margin — so a later re-ranking could make
    /// a cached name resolvable while the row still suppresses the lookup until it
    /// expires. That is accepted deliberately: the alternative is to remember
    /// nothing and pay the per-run cost this cache exists to remove.
    pub(crate) fn is_stable_artist_resolution_failure(&self) -> bool {
        matches!(self, Self::ArtistUnresolved(_))
    }
}

impl DiscoveryFailure {
    /// Only an artist-resolution failure is an artist problem; every other
    /// provider error (transport, HTTP status, decode, pagination, or unusable
    /// candidate data) is an outage problem.
    ///
    /// An inconsistent search page counts as an artist problem even though it is
    /// not cacheable, because falling back for this run is the pre-existing
    /// behaviour and changing it here would turn a response defect into a run
    /// abort.
    fn from_error(error: &DiscographyError) -> Self {
        match error {
            DiscographyError::ArtistUnresolved(_)
            | DiscographyError::ArtistSearchInconsistent(_) => Self::Unresolved,
            _ => Self::Provider,
        }
    }
}

/// Whether a caller will accept a recorded resolution failure in place of a
/// provider call. Named rather than a bare `bool` so the two call sites read as
/// a decision, following `ArtistComponent` in `organizer.rs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureCacheUse {
    /// Replay a fresh recorded failure for this artist.
    Honour,
    /// Ignore any recorded failure: the user asked for this artist by name, or
    /// pinned its MBID, so the lookup runs.
    Bypass,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscoveryOutcome {
    Authoritative {
        albums: Vec<AlbumTarget>,
        provenance: DiscoveryProvenance,
    },
    AuthoritativeEmpty {
        provenance: DiscoveryProvenance,
    },
    LegacyFallback {
        reason: String,
        kind: DiscoveryFailure,
        /// True when this failure was replayed from the failure cache rather
        /// than observed live, so the caller can report the two separately.
        from_cache: bool,
    },
}

/// Resolve an artist's conceptual albums, preferring a fresh discography
/// cache, then a freshly recorded resolution failure unless the caller bypassed
/// it or the artist already has a usable cache entry, then authoritative
/// MusicBrainz data, then a compatible stale cache, and finally a visible legacy
/// fallback.
///
/// `failure_cache` decides whether a recorded resolution failure may stand in
/// for the provider call; see [`FailureCacheUse`].
pub async fn discover_artist_albums(
    provider: &dyn DiscographyProvider,
    db: &Database,
    artist: &str,
    config: &DiscographyConfig,
    failure_cache: FailureCacheUse,
) -> DiscoveryOutcome {
    discover_artist_albums_at(
        provider,
        db,
        artist,
        config,
        chrono::Utc::now().timestamp(),
        failure_cache,
    )
    .await
}

/// Time-injectable variant of [`discover_artist_albums`].
pub(crate) async fn discover_artist_albums_at(
    provider: &dyn DiscographyProvider,
    db: &Database,
    artist: &str,
    config: &DiscographyConfig,
    now: i64,
    failure_cache: FailureCacheUse,
) -> DiscoveryOutcome {
    let artist_key = normalize_catalog_key(artist);
    let configured_mbid = config
        .artist_mbids
        .iter()
        .find(|(name, _)| normalize_catalog_key(name) == artist_key)
        .map(|(_, mbid)| mbid.as_str());

    let cached = load_compatible_cache(db, &artist_key, configured_mbid);
    if let Some(cached) = &cached {
        if cache_is_fresh(cached.entry.fetched_at, config.cache_days, now) {
            return select_outcome(
                &cached.groups,
                &config.allowed_types,
                DiscoveryProvenance::FreshCache,
            );
        }
    }

    if let Some(replayed) = replay_recorded_failure(
        db,
        &artist_key,
        config,
        now,
        failure_cache,
        configured_mbid,
        cached.is_some(),
    ) {
        return replayed;
    }

    let refresh = async {
        let resolved = match configured_mbid {
            Some(mbid) => provider.artist_by_id(mbid).await?,
            None => {
                let candidates = provider.search_artists(artist).await?;
                let resolved = resolve_artist(&artist_key, &candidates)?;
                if let ArtistResolution::ScoreDominance(evidence) = &resolved.resolution {
                    // Debug, not info: this fires for every artist MusicBrainz
                    // returns more than one exact canonical match for, which is a
                    // routine catalogue quirk rather than an operator-actionable
                    // event. The evidence keeps the decision auditable when the
                    // level is raised, and the selection is not silent either way:
                    // the chosen name and MBID reach the caller and the cache.
                    tracing::debug!(
                        artist = %artist,
                        selected_name = %resolved.candidate.name,
                        selected_mbid = %resolved.candidate.id,
                        exact_matches = evidence.exact_matches,
                        top_score = evidence.top_score,
                        runner_up_score = evidence.runner_up_score,
                        margin = evidence.margin,
                        "resolved duplicate canonical artist name by search-score dominance"
                    );
                }
                resolved.candidate
            }
        };
        // The row records a failure to RESOLVE this name, so a successful
        // resolution makes it obsolete whatever happens to the release-group
        // fetch that follows. Clearing it here rather than on the fully
        // successful path is what stops an earlier run's row from keeping the
        // artist skipped for the rest of its expiry once the name resolves.
        if let Err(error) = db.delete_discography_failure(&artist_key) {
            tracing::warn!("failed to clear discography failure cache: {error}");
        }
        let groups = provider.release_groups(&resolved.id).await?;
        Ok::<_, DiscographyError>((resolved, groups))
    }
    .await;

    match refresh {
        Ok((resolved, groups)) => match serialize_cache_payload(&groups) {
            Ok(payload) => {
                let entry = DiscographyCacheEntry {
                    artist_key: artist_key.clone(),
                    artist_mbid: resolved.id.clone(),
                    canonical_artist: resolved.name.clone(),
                    fetched_at: now,
                    release_groups_json: payload,
                };
                if let Err(error) = db.upsert_discography_cache(&entry) {
                    tracing::warn!("failed to write discography cache for {artist:?}: {error}");
                }
                select_outcome(
                    &groups,
                    &config.allowed_types,
                    DiscoveryProvenance::Refreshed,
                )
            }
            // An oversized (or unserializable) payload is a refresh error:
            // fall back to the stale cache or legacy discovery.
            Err(error) => stale_or_legacy(cached, &config.allowed_types, now, error),
        },
        Err(error) => {
            let has_success_row = cached.is_some();
            record_resolution_failure(db, &artist_key, config, now, &error, has_success_row);
            stale_or_legacy(cached, &config.allowed_types, now, error)
        }
    }
}

struct CachedDiscography {
    entry: DiscographyCacheEntry,
    groups: Vec<ReleaseGroup>,
}

/// Read and validate a compatible cache row: the normalized artist key must
/// match and, when an MBID override exists, the row must carry the same MBID
/// ignoring ASCII case. Corrupt rows are warned about, deleted, and skipped.
fn load_compatible_cache(
    db: &Database,
    artist_key: &str,
    configured_mbid: Option<&str>,
) -> Option<CachedDiscography> {
    let entry = match db.get_discography_cache(artist_key) {
        Ok(Some(entry)) => entry,
        Ok(None) => return None,
        Err(error) => {
            tracing::warn!("failed to read discography cache: {error}");
            return None;
        }
    };
    if normalize_catalog_key(&entry.artist_key) != artist_key {
        return None;
    }
    if let Some(mbid) = configured_mbid {
        if !entry.artist_mbid.eq_ignore_ascii_case(mbid) {
            return None;
        }
    }
    match serde_json::from_str::<Vec<ReleaseGroup>>(&entry.release_groups_json) {
        Ok(groups) => Some(CachedDiscography { entry, groups }),
        Err(error) => {
            tracing::warn!("discography cache for {artist_key:?} is corrupt ({error}); deleting");
            let _ = db.delete_discography_cache(artist_key);
            None
        }
    }
}

/// A row is fresh only when `now >= fetched_at`, `age < cache_days * 86_400`,
/// and `cache_days > 0`. Equality is stale; future timestamps are stale.
fn cache_is_fresh(fetched_at: i64, cache_days: u64, now: i64) -> bool {
    if cache_days == 0 {
        return false;
    }
    let age_seconds = (now as i128) - (fetched_at as i128);
    age_seconds >= 0 && age_seconds < (cache_days as i128) * (SECONDS_PER_DAY as i128)
}

/// Non-negative saturating age in complete days, for stale-cache warnings.
fn stale_age_days(now: i64, fetched_at: i64) -> u64 {
    let age_seconds = (now as i128) - (fetched_at as i128);
    if age_seconds <= 0 {
        0
    } else {
        (age_seconds / SECONDS_PER_DAY as i128) as u64
    }
}

/// Replay a recorded resolution failure instead of calling the provider.
///
/// Returns `None` when there is nothing to replay. In order of the checks below:
/// a pinned MBID makes resolution deterministic, so the recorded row is deleted
/// and the lookup runs; the caller bypassed the cache; the configured expiry is
/// `0`; a compatible success row exists, which is the better answer even when it
/// is stale; no row exists; the row is stale; or its kind is unknown.
///
/// Every one of those is a miss rather than an error, because a cache must never
/// be able to block a lookup it was not written for.
fn replay_recorded_failure(
    db: &Database,
    artist_key: &str,
    config: &DiscographyConfig,
    now: i64,
    use_cache: FailureCacheUse,
    configured_mbid: Option<&str>,
    has_success_row: bool,
) -> Option<DiscoveryOutcome> {
    if configured_mbid.is_some() {
        // A pinned MBID makes resolution deterministic, so any recorded failure
        // is obsolete. Drop it and let the lookup run.
        if let Err(error) = db.delete_discography_failure(artist_key) {
            tracing::warn!("failed to clear discography failure row: {error}");
        }
        return None;
    }
    if use_cache == FailureCacheUse::Bypass || config.failure_cache_days == 0 {
        return None;
    }
    // The two kinds of row are mutually exclusive by construction, but a failed
    // delete or a crash between the success write and the failure delete can
    // leave both. A compatible success row is the better answer even when it is
    // stale - it is a usable fallback - so a failure row must never shadow it,
    // or the artist would be skipped instead of falling back to those albums.
    if has_success_row {
        return None;
    }
    let entry = match db.get_discography_failure(artist_key) {
        Ok(Some(entry)) => entry,
        Ok(None) => return None,
        Err(error) => {
            tracing::warn!("failed to read discography failure cache: {error}");
            return None;
        }
    };
    if !cache_is_fresh(entry.recorded_at, config.failure_cache_days, now) {
        return None;
    }
    if entry.failure_kind != FAILURE_KIND_UNRESOLVED {
        return None;
    }
    let reason = if entry.reason.trim().is_empty() {
        "artist could not be resolved safely".to_string()
    } else {
        entry.reason
    };
    tracing::debug!(
        artist_key,
        recorded_at = entry.recorded_at,
        "skipping a recorded resolution failure"
    );
    Some(DiscoveryOutcome::LegacyFallback {
        reason,
        kind: DiscoveryFailure::Unresolved,
        from_cache: true,
    })
}

/// Record an artist-resolution failure so a later run can skip the lookup.
///
/// Only a failure to match the artist name is recorded, which
/// [`DiscographyError::is_stable_artist_resolution_failure`] decides. That
/// excludes a `Provider` failure, because an outage would otherwise keep
/// suppressing a healthy MusicBrainz for the whole expiry, and it excludes
/// [`DiscographyError::ArtistSearchInconsistent`], which is classified as an
/// artist problem for this run's fallback but describes one response. Nothing is
/// recorded when the feature is off, when there is no usable artist key, or when
/// a success row already exists for this key — the two kinds of row are mutually
/// exclusive by construction, so a stale-but-usable success row is never
/// shadowed by a fresh failure.
fn record_resolution_failure(
    db: &Database,
    artist_key: &str,
    config: &DiscographyConfig,
    now: i64,
    error: &DiscographyError,
    has_success_row: bool,
) {
    if config.failure_cache_days == 0 || has_success_row || artist_key.is_empty() {
        return;
    }
    // Cacheability is decided by the error itself, not by the discovery-failure
    // classification: an inconsistent search page is classified as unresolved so
    // this run falls back, but it describes one response and must not be
    // remembered.
    if !error.is_stable_artist_resolution_failure() {
        return;
    }
    let entry = DiscographyFailureEntry {
        artist_key: artist_key.to_string(),
        failure_kind: FAILURE_KIND_UNRESOLVED.to_string(),
        reason: error.to_string(),
        recorded_at: now,
    };
    if let Err(error) = db.upsert_discography_failure(&entry) {
        tracing::warn!("failed to write discography failure cache for {artist_key:?}: {error}");
    }
}

/// Reject cache payloads over the 16 MiB limit (a refresh error that falls
/// back to a stale cache or legacy discovery when no cache exists).
fn serialize_cache_payload(
    groups: &[ReleaseGroup],
) -> std::result::Result<String, DiscographyError> {
    let payload = serde_json::to_string(groups)
        .map_err(|error| DiscographyError::Decode(format!("failed to serialize cache: {error}")))?;
    if payload.len() > MAX_CACHE_PAYLOAD_BYTES {
        return Err(DiscographyError::Decode(format!(
            "cache payload of {} bytes exceeds the {MAX_CACHE_PAYLOAD_BYTES}-byte limit",
            payload.len()
        )));
    }
    Ok(payload)
}

fn select_outcome(
    groups: &[ReleaseGroup],
    allowed: &[DiscographyReleaseType],
    provenance: DiscoveryProvenance,
) -> DiscoveryOutcome {
    let albums = select_albums(groups, allowed);
    if albums.is_empty() {
        DiscoveryOutcome::AuthoritativeEmpty { provenance }
    } else {
        DiscoveryOutcome::Authoritative { albums, provenance }
    }
}

/// On a refresh error, return the compatible stale cache when present
/// (converting a filtered-empty stale list to `AuthoritativeEmpty`), else
/// `LegacyFallback` with the exact typed error text.
fn stale_or_legacy(
    cached: Option<CachedDiscography>,
    allowed: &[DiscographyReleaseType],
    now: i64,
    refresh_error: DiscographyError,
) -> DiscoveryOutcome {
    let Some(cached) = cached else {
        return DiscoveryOutcome::LegacyFallback {
            kind: DiscoveryFailure::from_error(&refresh_error),
            reason: refresh_error.to_string(),
            from_cache: false,
        };
    };
    let provenance = DiscoveryProvenance::StaleCache {
        age_days: stale_age_days(now, cached.entry.fetched_at),
        kind: DiscoveryFailure::from_error(&refresh_error),
        refresh_error: refresh_error.to_string(),
    };
    select_outcome(&cached.groups, allowed, provenance)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group(
        id: &str,
        title: &str,
        date: Option<&str>,
        primary: Option<&str>,
        secondary: &[&str],
    ) -> ReleaseGroup {
        ReleaseGroup {
            id: id.into(),
            title: title.into(),
            first_release_date: date.map(str::to_owned),
            primary_type: primary.map(str::to_owned),
            secondary_types: secondary.iter().map(|value| (*value).to_owned()).collect(),
        }
    }

    fn scored(id: &str, name: &str, score: Option<u8>) -> ArtistCandidate {
        ArtistCandidate {
            id: id.to_string(),
            name: name.to_string(),
            score,
        }
    }

    /// The live MusicBrainz artist search for `Ils`: four canonical exact-name
    /// candidates scored 100, 86, 83, 83.
    fn ils_candidates() -> Vec<ArtistCandidate> {
        vec![
            scored("16b97aaa-d7c0-469f-8c97-47c705b2d02f", "Ils", Some(100)),
            scored("638e9183-2cde-4c07-b1d5-1f0e0361ed1c", "Ils", Some(86)),
            scored("cc54a811-a221-49f1-b93d-ed42f1affbb0", "ILS", Some(83)),
            scored("5323e64e-008f-4b5c-affc-f410b3746908", "ILS", Some(83)),
        ]
    }

    #[test]
    fn catalog_key_uses_nfkc_case_and_whitespace_without_dropping_punctuation() {
        assert_eq!(normalize_catalog_key("  ＡC/DC  "), "ac/dc");
        assert_ne!(
            normalize_catalog_key("AC/DC"),
            normalize_catalog_key("AC DC")
        );
    }

    #[test]
    fn exact_artist_resolution_uses_only_unique_canonical_name() {
        let candidates = vec![
            ArtistCandidate {
                id: "1".into(),
                name: "ＡC/DC".into(),
                score: None,
            },
            ArtistCandidate {
                id: "2".into(),
                name: "AC DC".into(),
                score: None,
            },
        ];
        let resolved = resolve_artist("ac/dc", &candidates).unwrap();
        assert_eq!(resolved.candidate.id, "1");
        assert_eq!(resolved.resolution, ArtistResolution::UniqueExactName);

        // Two exact matches stay unresolved without a dominant score.
        assert!(resolve_artist(
            "AC DC",
            &[
                ArtistCandidate {
                    id: "2".into(),
                    name: "AC DC".into(),
                    score: Some(100),
                },
                ArtistCandidate {
                    id: "3".into(),
                    name: " ac dc ".into(),
                    score: Some(100),
                },
            ]
        )
        .is_err());
        let error = resolve_artist("Missing", &candidates).unwrap_err();
        assert!(error
            .to_string()
            .contains("no candidate matches \"Missing\""));
    }

    #[test]
    fn dominance_selects_the_unique_score_100_candidate() {
        let resolved = resolve_artist("ils", &ils_candidates()).unwrap();
        assert_eq!(
            resolved.candidate.id,
            "16b97aaa-d7c0-469f-8c97-47c705b2d02f"
        );
        assert_eq!(
            resolved.resolution,
            ArtistResolution::ScoreDominance(DominanceEvidence {
                exact_matches: 4,
                top_score: 100,
                runner_up_score: 86,
                margin: 14,
            })
        );
    }

    #[test]
    fn dominance_selects_the_bonobo_shaped_winner() {
        let candidates = vec![
            scored("b1000000-0000-0000-0000-000000000001", "Bonobo", Some(100)),
            scored("b1000000-0000-0000-0000-000000000002", "Bonobo", Some(78)),
            scored("b1000000-0000-0000-0000-000000000003", "Bonobo", Some(77)),
            scored("b1000000-0000-0000-0000-000000000004", "Bonobo", Some(77)),
        ];
        let resolved = resolve_artist("bonobo", &candidates).unwrap();
        assert_eq!(
            resolved.candidate.id,
            "b1000000-0000-0000-0000-000000000001"
        );
        assert_eq!(
            resolved.resolution,
            ArtistResolution::ScoreDominance(DominanceEvidence {
                exact_matches: 4,
                top_score: 100,
                runner_up_score: 78,
                margin: 22,
            })
        );
    }

    #[test]
    fn dominance_is_independent_of_response_order() {
        fn assert_all_permutations(
            candidates: &mut [ArtistCandidate],
            index: usize,
            expected: &ResolvedArtist,
            visited: &mut usize,
        ) {
            if index == candidates.len() {
                assert_eq!(resolve_artist("ils", candidates).unwrap(), *expected);
                *visited += 1;
                return;
            }
            for swap_index in index..candidates.len() {
                candidates.swap(index, swap_index);
                assert_all_permutations(candidates, index + 1, expected, visited);
                candidates.swap(index, swap_index);
            }
        }

        let expected = resolve_artist("ils", &ils_candidates()).unwrap();
        let mut candidates = ils_candidates();
        let mut visited = 0;
        assert_all_permutations(&mut candidates, 0, &expected, &mut visited);
        assert_eq!(visited, 24, "four candidates must produce 4! permutations");
    }

    #[test]
    fn unique_exact_name_resolves_with_or_without_a_score() {
        for score in [None, Some(60)] {
            let candidates = vec![scored("only", "Nils Frahm", score)];
            let resolved = resolve_artist("nils frahm", &candidates).unwrap();
            assert_eq!(resolved.resolution, ArtistResolution::UniqueExactName);
            assert_eq!(resolved.candidate.id, "only");
        }
    }

    #[test]
    fn weak_or_incomplete_dominance_is_unresolved() {
        let cases: Vec<(&str, Vec<ArtistCandidate>, &str)> = vec![
            (
                "top score below 100",
                vec![scored("a", "Ils", Some(99)), scored("b", "Ils", Some(70))],
                "highest score 99 is below 100",
            ),
            (
                "margin below 10",
                vec![scored("a", "Ils", Some(100)), scored("b", "Ils", Some(91))],
                "leading margin 9 is below 10",
            ),
            (
                "tied top score",
                vec![
                    scored("a", "Ils", Some(100)),
                    scored("b", "Ils", Some(100)),
                    scored("c", "Ils", Some(70)),
                ],
                "tied top score of 100",
            ),
            (
                "no exact match",
                vec![scored("a", "Somebody Else", Some(100))],
                "no candidate matches \"ils\"",
            ),
        ];

        for (label, candidates, expected_reason) in cases {
            let error = resolve_artist("ils", &candidates).unwrap_err();
            assert!(
                matches!(error, DiscographyError::ArtistUnresolved(_)),
                "{label} must stay unresolved, got {error:?}"
            );
            assert!(
                error.to_string().contains(expected_reason),
                "{label} must explain the failed rule, got: {error}"
            );
        }
    }

    #[test]
    fn unusable_candidate_data_is_not_reported_as_unresolved() {
        // Invalid or missing search scores are provider data defects rather than
        // an artist that cannot be resolved. Classifying them with the
        // resolution failures would hide a broken provider from the discover
        // circuit breaker.
        let cases = vec![
            (
                "missing competing score",
                vec![scored("a", "Ils", Some(100)), scored("b", "Ils", None)],
                "at least one has no search score",
            ),
            (
                "invalid score above the range",
                vec![scored("a", "Ils", Some(255)), scored("b", "Ils", Some(70))],
                "invalid score 255",
            ),
            (
                "invalid score takes precedence over a later missing score",
                vec![scored("a", "Ils", Some(255)), scored("b", "Ils", None)],
                "invalid score 255",
            ),
            (
                "invalid score takes precedence over an earlier missing score",
                vec![scored("a", "Ils", None), scored("b", "Ils", Some(255))],
                "invalid score 255",
            ),
            (
                "highest invalid score is reported regardless of order",
                vec![scored("a", "Ils", Some(101)), scored("b", "Ils", Some(255))],
                "invalid score 255",
            ),
        ];

        for (label, candidates, expected_reason) in cases {
            let error = resolve_artist("ils", &candidates).unwrap_err();
            assert!(
                matches!(error, DiscographyError::InvalidCandidateData(_)),
                "{label} must be classed as a provider data defect, got {error:?}"
            );
            assert!(
                error.to_string().contains(expected_reason),
                "{label} must explain the failed rule, got: {error}"
            );
        }
    }

    #[test]
    fn dominance_margin_boundary_is_inclusive() {
        let candidates = vec![scored("a", "Ils", Some(100)), scored("b", "Ils", Some(90))];
        let resolved = resolve_artist("ils", &candidates).unwrap();
        assert_eq!(resolved.candidate.id, "a");
    }

    #[test]
    fn non_exact_higher_score_cannot_win() {
        let candidates = vec![
            scored("canonical", "Ils", None),
            scored("alias", "Illian Walker", Some(100)),
        ];
        let resolved = resolve_artist("ils", &candidates).unwrap();
        assert_eq!(resolved.resolution, ArtistResolution::UniqueExactName);
        assert_eq!(resolved.candidate.id, "canonical");
    }

    #[test]
    fn release_classification_is_conservative() {
        let studio = group("1", "Studio", Some("2000"), Some("Album"), &[]);
        let live = group("2", "Live", Some("2001-02"), Some("Album"), &["Live"]);
        let live_compilation = group(
            "3",
            "Live Collection",
            Some("2002-03-04"),
            Some("Album"),
            &["Live", "Compilation"],
        );
        assert!(release_allowed(
            &studio,
            &[DiscographyReleaseType::StudioAlbum]
        ));
        assert!(!release_allowed(
            &live,
            &[DiscographyReleaseType::StudioAlbum]
        ));
        assert!(release_allowed(&live, &[DiscographyReleaseType::LiveAlbum]));
        assert!(!release_allowed(
            &live_compilation,
            &[DiscographyReleaseType::LiveAlbum]
        ));
        assert!(release_allowed(
            &live_compilation,
            &[
                DiscographyReleaseType::LiveAlbum,
                DiscographyReleaseType::Compilation,
            ]
        ));
    }

    #[test]
    fn unknown_and_empty_release_metadata_is_rejected() {
        let allowed = [
            DiscographyReleaseType::StudioAlbum,
            DiscographyReleaseType::LiveAlbum,
        ];
        assert!(!release_allowed(
            &group("1", "Missing primary", None, None, &[]),
            &allowed,
        ));
        assert!(!release_allowed(
            &group("2", "Unknown primary", None, Some("Other"), &[]),
            &allowed,
        ));
        assert!(!release_allowed(
            &group(
                "3",
                "Unknown secondary",
                None,
                Some("Album"),
                &["Interview"],
            ),
            &allowed,
        ));
        assert!(
            select_albums(&[group("4", "   ", None, Some("Album"), &[])], &allowed,).is_empty()
        );
    }

    #[test]
    fn excluded_release_groups_are_logged_with_reasons() {
        let capture = crate::test_support::LogCapture::start();
        let groups = [
            group(
                "unknown-secondary",
                "Unknown Secondary",
                None,
                Some("Album"),
                &["Interview"],
            ),
            group("empty-title", "   ", None, Some("Album"), &[]),
        ];
        assert!(select_albums(
            &groups,
            &[
                DiscographyReleaseType::StudioAlbum,
                DiscographyReleaseType::LiveAlbum,
            ],
        )
        .is_empty());

        let logs = capture.text();
        assert!(logs.contains("unknown-secondary"), "got: {logs}");
        assert!(logs.contains("unknown secondary type"), "got: {logs}");
        assert!(logs.contains("empty-title"), "got: {logs}");
        assert!(logs.contains("empty title"), "got: {logs}");
    }

    /// The live MusicBrainz release groups for Archive on 2026-09-17, reduced
    /// to the three that matter: the two-album bundle and the albums it names.
    /// The last MBID is a test fixture, not a real MusicBrainz ID.
    fn archive_bundle_groups() -> Vec<ReleaseGroup> {
        vec![
            group(
                "3eecc2ca-8d9e-4159-8bee-cd84173a5fba",
                "Controlling Crowds / You All Look the Same to Me",
                Some("2014"),
                Some("Album"),
                &[],
            ),
            group(
                "532ed7d7-ad97-3771-b9e8-333f1c25adc2",
                "Controlling Crowds",
                Some("2009-03-27"),
                Some("Album"),
                &[],
            ),
            group(
                "aa11bb22-cc33-44dd-55ee-66ff77008899",
                "You All Look the Same to Me",
                Some("2002-03-12"),
                Some("Album"),
                &[],
            ),
        ]
    }

    #[test]
    fn a_bundled_release_group_is_not_selected_and_its_albums_are() {
        let albums = select_albums(
            &archive_bundle_groups(),
            &[DiscographyReleaseType::StudioAlbum],
        );
        let titles: Vec<&str> = albums.iter().map(|album| album.title.as_str()).collect();
        assert_eq!(
            titles,
            vec!["You All Look the Same to Me", "Controlling Crowds"]
        );
        let bundle_id = "3eecc2ca-8d9e-4159-8bee-cd84173a5fba";
        assert!(albums
            .iter()
            .all(|album| album.release_group_id != bundle_id));
        assert_eq!(
            albums[1].release_group_id,
            "532ed7d7-ad97-3771-b9e8-333f1c25adc2"
        );
    }

    #[test]
    fn a_venue_and_date_title_stays_a_single_target() {
        let groups = vec![
            group(
                "m1",
                concat!(
                    "Live at Wembley Stadium, London, England",
                    " / April 20th, 1992"
                ),
                Some("1992-04-20"),
                Some("Album"),
                &[],
            ),
            group("m2", "Metallica", Some("1991-08-12"), Some("Album"), &[]),
        ];
        let albums = select_albums(&groups, &[DiscographyReleaseType::StudioAlbum]);
        let titles: Vec<&str> = albums.iter().map(|album| album.title.as_str()).collect();
        assert_eq!(
            titles,
            vec![
                "Metallica",
                "Live at Wembley Stadium, London, England / April 20th, 1992"
            ]
        );
    }

    #[test]
    fn a_bundle_is_dropped_when_one_part_is_filtered_out() {
        let groups = vec![
            group(
                "b1",
                "First Album / Second Album",
                Some("2020"),
                Some("Album"),
                &[],
            ),
            group("b2", "First Album", Some("2001"), Some("EP"), &[]),
            group("b3", "Second Album", Some("2002"), Some("Album"), &[]),
        ];
        let albums = select_albums(&groups, &[DiscographyReleaseType::StudioAlbum]);
        let titles: Vec<&str> = albums.iter().map(|album| album.title.as_str()).collect();
        assert_eq!(titles, vec!["Second Album"]);
    }

    /// The same shape as `archive_bundle_groups`, with a bundle MBID that no
    /// other test emits, so a captured log record can only have come from the
    /// test that uses this fixture.
    fn bundle_log_fixture() -> Vec<ReleaseGroup> {
        let mut groups = archive_bundle_groups();
        groups[0].id = "9f8e7d6c-5b4a-4392-8170-6f5e4d3c2b1a".to_owned();
        groups
    }

    #[test]
    fn a_bundled_release_group_is_logged_with_its_reason() {
        let capture = crate::test_support::LogCapture::start();
        let albums = select_albums(
            &bundle_log_fixture(),
            &[DiscographyReleaseType::StudioAlbum],
        );
        assert_eq!(albums.len(), 2);
        let logs = capture.text();
        // Both halves of the assertion read the same record, so only this
        // test's own fixture can satisfy it - the unique MBID and the reason
        // must appear on one line.
        let record = logs
            .lines()
            .find(|line| line.contains("9f8e7d6c-5b4a-4392-8170-6f5e4d3c2b1a"))
            .unwrap_or_default();
        assert!(
            record.contains("title bundles other release groups"),
            "got: {logs}"
        );
    }

    #[test]
    fn albums_are_deduplicated_and_ordered_by_partial_date() {
        let groups = vec![
            group("b", "Same", Some("2001"), Some("Album"), &[]),
            group("a", " same ", Some("1999-12"), Some("Album"), &[]),
            group("c", "Later", Some("2005-01-02"), Some("Album"), &[]),
            group("d", "Undated", Some("not-a-date"), Some("Album"), &[]),
        ];
        let albums = select_albums(&groups, &[DiscographyReleaseType::StudioAlbum]);
        assert_eq!(
            albums
                .iter()
                .map(|album| album.title.as_str())
                .collect::<Vec<_>>(),
            vec!["same", "Later", "Undated"]
        );
        assert_eq!(albums[0].release_group_id, "a");
    }

    #[test]
    fn every_friendly_release_category_maps() {
        let cases = [
            (
                "Album",
                Vec::<&str>::new(),
                DiscographyReleaseType::StudioAlbum,
            ),
            ("Album", vec!["Live"], DiscographyReleaseType::LiveAlbum),
            ("EP", Vec::<&str>::new(), DiscographyReleaseType::Ep),
            ("Single", Vec::<&str>::new(), DiscographyReleaseType::Single),
            (
                "Album",
                vec!["Compilation"],
                DiscographyReleaseType::Compilation,
            ),
            ("Album", vec!["Remix"], DiscographyReleaseType::Remix),
            (
                "Album",
                vec!["Soundtrack"],
                DiscographyReleaseType::Soundtrack,
            ),
            ("Album", vec!["DJ-mix"], DiscographyReleaseType::DjMix),
            (
                "Album",
                vec!["Mixtape/Street"],
                DiscographyReleaseType::Mixtape,
            ),
        ];
        for (primary, secondary, allowed) in cases {
            let release = group("id", "title", None, Some(primary), &secondary);
            assert!(
                release_allowed(&release, &[allowed]),
                "failed for {allowed:?}"
            );
        }
    }

    #[test]
    fn partial_dates_reject_impossible_values() {
        assert_eq!(
            parse_partial_date("2000"),
            Some(PartialDate {
                year: 2000,
                month: 0,
                day: 0
            })
        );
        assert_eq!(
            parse_partial_date("2000-02"),
            Some(PartialDate {
                year: 2000,
                month: 2,
                day: 0
            })
        );
        assert_eq!(
            parse_partial_date("2000-02-29"),
            Some(PartialDate {
                year: 2000,
                month: 2,
                day: 29
            })
        );
        assert_eq!(parse_partial_date("2001-02-29"), None);
        assert_eq!(parse_partial_date("2000-13"), None);
        assert_eq!(parse_partial_date("2000-01-32"), None);
        assert_eq!(parse_partial_date("not-a-date"), None);
    }

    use crate::config::DiscographyConfig;
    use crate::db::{Database, DiscographyCacheEntry};
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeProvider {
        artist_responses:
            Mutex<VecDeque<std::result::Result<Vec<ArtistCandidate>, DiscographyError>>>,
        group_responses: Mutex<VecDeque<std::result::Result<Vec<ReleaseGroup>, DiscographyError>>>,
        requested_group_mbids: Mutex<Vec<String>>,
        artist_calls: AtomicUsize,
        lookup_calls: AtomicUsize,
        group_calls: AtomicUsize,
        failure: Option<String>,
        /// Answer the artist search with a page whose reported total disagrees
        /// with the candidates it carried.
        inconsistent: bool,
    }

    impl FakeProvider {
        fn artists(artists: Vec<ArtistCandidate>) -> Self {
            Self {
                artist_responses: Mutex::new(VecDeque::from([Ok(artists)])),
                ..Self::default()
            }
        }

        fn with_groups(groups: Vec<ReleaseGroup>) -> Self {
            Self {
                artist_responses: Mutex::new(VecDeque::from([Ok(vec![ArtistCandidate {
                    id: "11111111-1111-1111-1111-111111111111".to_string(),
                    name: "Artist".to_string(),
                    score: None,
                }])])),
                group_responses: Mutex::new(VecDeque::from([Ok(groups)])),
                ..Self::default()
            }
        }

        fn failing(reason: &str) -> Self {
            Self {
                failure: Some(reason.to_string()),
                ..Self::default()
            }
        }

        /// An artist search that returns only non-matching names, so resolution
        /// fails as `DiscoveryFailure::Unresolved` rather than as an outage.
        fn unresolvable_recording() -> Self {
            Self {
                artist_responses: Mutex::new(VecDeque::from([Ok(vec![ArtistCandidate {
                    id: "22222222-2222-2222-2222-222222222222".to_string(),
                    name: "Somebody Else".to_string(),
                    score: None,
                }])])),
                ..Self::default()
            }
        }

        /// A search that answers with a page whose reported total disagrees with
        /// its payload. Classified as an artist problem for this run, but it is a
        /// property of one response, so it must never be cached.
        fn inconsistent_search() -> Self {
            Self {
                inconsistent: true,
                ..Self::default()
            }
        }

        fn total_calls(&self) -> usize {
            self.artist_calls.load(Ordering::SeqCst)
                + self.lookup_calls.load(Ordering::SeqCst)
                + self.group_calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl DiscographyProvider for FakeProvider {
        async fn search_artists(
            &self,
            _artist: &str,
        ) -> std::result::Result<Vec<ArtistCandidate>, DiscographyError> {
            self.artist_calls.fetch_add(1, Ordering::SeqCst);
            if let Some(reason) = &self.failure {
                return Err(DiscographyError::Transport(reason.clone()));
            }
            if self.inconsistent {
                return Err(DiscographyError::ArtistSearchInconsistent(
                    "artist search returned 0 candidates but reported 1".to_string(),
                ));
            }
            self.artist_responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Ok(Vec::new()))
        }

        async fn artist_by_id(
            &self,
            artist_mbid: &str,
        ) -> std::result::Result<ArtistCandidate, DiscographyError> {
            self.lookup_calls.fetch_add(1, Ordering::SeqCst);
            if let Some(reason) = &self.failure {
                return Err(DiscographyError::Transport(reason.clone()));
            }
            Ok(ArtistCandidate {
                id: artist_mbid.to_string(),
                name: "Artist".to_string(),
                score: None,
            })
        }

        async fn release_groups(
            &self,
            artist_mbid: &str,
        ) -> std::result::Result<Vec<ReleaseGroup>, DiscographyError> {
            self.group_calls.fetch_add(1, Ordering::SeqCst);
            self.requested_group_mbids
                .lock()
                .unwrap()
                .push(artist_mbid.to_string());
            if let Some(reason) = &self.failure {
                return Err(DiscographyError::Transport(reason.clone()));
            }
            self.group_responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Ok(Vec::new()))
        }
    }

    fn cache_groups(
        db: &Database,
        artist_key: &str,
        mbid: &str,
        fetched_at: i64,
        groups: &[ReleaseGroup],
    ) {
        db.upsert_discography_cache(&DiscographyCacheEntry {
            artist_key: artist_key.to_string(),
            artist_mbid: mbid.to_string(),
            canonical_artist: "Artist".to_string(),
            fetched_at,
            release_groups_json: serde_json::to_string(groups).unwrap(),
        })
        .unwrap();
    }

    fn record_failure(db: &Database, artist_key: &str, reason: &str, recorded_at: i64) {
        db.upsert_discography_failure(&crate::db::DiscographyFailureEntry {
            artist_key: artist_key.to_string(),
            failure_kind: "unresolved".to_string(),
            reason: reason.to_string(),
            recorded_at,
        })
        .unwrap();
    }

    #[tokio::test]
    async fn an_unresolved_artist_is_recorded() {
        let db = Database::open_in_memory().unwrap();
        let provider = FakeProvider::unresolvable_recording();

        discover_artist_albums_at(
            &provider,
            &db,
            "Unknown",
            &DiscographyConfig::default(),
            1_000,
            FailureCacheUse::Honour,
        )
        .await;

        let stored = db.get_discography_failure("unknown").unwrap().unwrap();
        assert_eq!(stored.failure_kind, "unresolved");
        assert_eq!(stored.recorded_at, 1_000);
        assert!(!stored.reason.is_empty());
    }

    #[tokio::test]
    async fn a_provider_outage_is_not_recorded() {
        let db = Database::open_in_memory().unwrap();
        let provider = FakeProvider::failing("service unavailable");

        discover_artist_albums_at(
            &provider,
            &db,
            "Artist",
            &DiscographyConfig::default(),
            1_000,
            FailureCacheUse::Honour,
        )
        .await;

        assert!(
            db.get_discography_failure("artist").unwrap().is_none(),
            "a transient outage must never be remembered"
        );
    }

    #[tokio::test]
    async fn zero_failure_cache_days_records_nothing() {
        let db = Database::open_in_memory().unwrap();
        let config = DiscographyConfig {
            failure_cache_days: 0,
            ..DiscographyConfig::default()
        };
        let provider = FakeProvider::unresolvable_recording();

        discover_artist_albums_at(
            &provider,
            &db,
            "Unknown",
            &config,
            1_000,
            FailureCacheUse::Honour,
        )
        .await;

        assert!(db.get_discography_failure("unknown").unwrap().is_none());
    }

    #[tokio::test]
    async fn no_failure_row_is_written_when_a_success_row_exists() {
        let db = Database::open_in_memory().unwrap();
        let groups = vec![group("1", "Studio", Some("2000"), Some("Album"), &[])];
        // Stale, so the refresh still runs, but present, so the rows stay
        // mutually exclusive.
        cache_groups(
            &db,
            "artist",
            "11111111-1111-1111-1111-111111111111",
            0,
            &groups,
        );
        let provider = FakeProvider::default();

        discover_artist_albums_at(
            &provider,
            &db,
            "Artist",
            &DiscographyConfig::default(),
            31 * 86_400,
            FailureCacheUse::Honour,
        )
        .await;

        assert!(db.get_discography_failure("artist").unwrap().is_none());
    }

    #[tokio::test]
    async fn a_successful_refresh_clears_a_recorded_failure() {
        let db = Database::open_in_memory().unwrap();
        let config = DiscographyConfig::default();
        // Recorded exactly at the expiry, so the row is stale (equality is
        // stale, as it is for the success cache) and the refresh runs instead
        // of being replayed.
        record_failure(&db, "artist", "no candidate matches", 1_000);
        let provider =
            FakeProvider::with_groups(vec![group("1", "Studio", Some("2000"), Some("Album"), &[])]);

        discover_artist_albums_at(
            &provider,
            &db,
            "Artist",
            &config,
            1_000 + (config.failure_cache_days * 86_400) as i64,
            FailureCacheUse::Honour,
        )
        .await;

        assert!(
            db.get_discography_failure("artist").unwrap().is_none(),
            "a successful resolution makes the recorded failure obsolete"
        );
    }

    #[tokio::test]
    async fn fresh_recorded_failure_is_replayed_without_a_provider_call() {
        let db = Database::open_in_memory().unwrap();
        record_failure(&db, "unknown", "no candidate matches \"unknown\"", 1_000);
        let provider = FakeProvider::unresolvable_recording();

        let outcome = discover_artist_albums_at(
            &provider,
            &db,
            "Unknown",
            &DiscographyConfig::default(),
            1_100,
            FailureCacheUse::Honour,
        )
        .await;

        assert!(matches!(
            outcome,
            DiscoveryOutcome::LegacyFallback {
                ref reason,
                kind: DiscoveryFailure::Unresolved,
                from_cache: true,
            } if reason == "no candidate matches \"unknown\""
        ));
        assert_eq!(
            provider.total_calls(),
            0,
            "a replay must not call the provider"
        );
    }

    #[tokio::test]
    async fn recorded_failure_at_the_ttl_boundary_is_retried() {
        let db = Database::open_in_memory().unwrap();
        let config = DiscographyConfig::default();
        let ttl = config.failure_cache_days * 86_400;
        record_failure(&db, "unknown", "no candidate matches", 1_000);
        let provider = FakeProvider::unresolvable_recording();

        let outcome = discover_artist_albums_at(
            &provider,
            &db,
            "Unknown",
            &config,
            1_000 + ttl as i64,
            FailureCacheUse::Honour,
        )
        .await;

        assert!(matches!(
            outcome,
            DiscoveryOutcome::LegacyFallback {
                from_cache: false,
                ..
            }
        ));
        assert!(provider.total_calls() > 0, "an expired row must retry");
    }

    #[tokio::test]
    async fn recorded_failure_beyond_the_ttl_is_retried() {
        // The contract requires both boundaries to retry, not just equality: a row
        // one second past the expiry must be treated exactly like one at it.
        let db = Database::open_in_memory().unwrap();
        let config = DiscographyConfig::default();
        let ttl = config.failure_cache_days * 86_400;
        record_failure(&db, "unknown", "no candidate matches", 1_000);
        let provider = FakeProvider::unresolvable_recording();

        let outcome = discover_artist_albums_at(
            &provider,
            &db,
            "Unknown",
            &config,
            1_000 + ttl as i64 + 1,
            FailureCacheUse::Honour,
        )
        .await;

        assert!(matches!(
            outcome,
            DiscoveryOutcome::LegacyFallback {
                from_cache: false,
                ..
            }
        ));
        assert!(
            provider.total_calls() > 0,
            "a row past the expiry must retry"
        );
    }

    #[tokio::test]
    async fn zero_failure_cache_days_retries_and_records_nothing() {
        let db = Database::open_in_memory().unwrap();
        let config = DiscographyConfig {
            failure_cache_days: 0,
            ..DiscographyConfig::default()
        };
        record_failure(&db, "unknown", "no candidate matches", 1_000);
        let provider = FakeProvider::unresolvable_recording();

        let outcome = discover_artist_albums_at(
            &provider,
            &db,
            "Unknown",
            &config,
            1_100,
            FailureCacheUse::Honour,
        )
        .await;

        assert!(matches!(
            outcome,
            DiscoveryOutcome::LegacyFallback {
                from_cache: false,
                ..
            }
        ));
        assert!(provider.total_calls() > 0);
        // The seeded row is what proves nothing was honoured. It must also be
        // left exactly as it was, because a disabled cache must not rewrite what
        // is already there - the sibling test covers the case where no row exists.
        let stored = db.get_discography_failure("unknown").unwrap().unwrap();
        assert_eq!(
            stored.recorded_at, 1_000,
            "a disabled cache must not rewrite an existing row"
        );
    }

    #[tokio::test]
    async fn bypass_ignores_a_fresh_recorded_failure() {
        let db = Database::open_in_memory().unwrap();
        record_failure(&db, "unknown", "no candidate matches", 1_000);
        let provider = FakeProvider::unresolvable_recording();

        let outcome = discover_artist_albums_at(
            &provider,
            &db,
            "Unknown",
            &DiscographyConfig::default(),
            1_100,
            FailureCacheUse::Bypass,
        )
        .await;

        assert!(matches!(
            outcome,
            DiscoveryOutcome::LegacyFallback {
                from_cache: false,
                ..
            }
        ));
        assert!(provider.total_calls() > 0);
    }

    #[tokio::test]
    async fn a_pinned_mbid_bypasses_and_clears_a_recorded_failure() {
        let db = Database::open_in_memory().unwrap();
        record_failure(&db, "artist", "no candidate matches", 1_000);
        // A failing provider, so the pinned refresh produces no success row: the
        // only thing that can clear the recorded failure is the bypass itself.
        let provider = FakeProvider::failing("service unavailable");
        let config = DiscographyConfig {
            artist_mbids: std::collections::BTreeMap::from([(
                "Artist".to_string(),
                "11111111-1111-1111-1111-111111111111".to_string(),
            )]),
            ..DiscographyConfig::default()
        };

        discover_artist_albums_at(
            &provider,
            &db,
            "Artist",
            &config,
            1_100,
            FailureCacheUse::Honour,
        )
        .await;

        assert!(
            provider.total_calls() > 0,
            "a pinned MBID must reach MusicBrainz"
        );
        assert!(
            db.get_discography_failure("artist").unwrap().is_none(),
            "the recorded failure is obsolete once the MBID is pinned"
        );
    }

    #[tokio::test]
    async fn a_successful_resolution_clears_the_row_even_if_the_release_fetch_fails() {
        // The row records a failure to RESOLVE the name. Once the name resolves
        // the row is obsolete, whatever happens to the release-group fetch that
        // follows; otherwise an earlier run's row would keep the artist skipped
        // for the rest of its expiry even though it now resolves.
        let db = Database::open_in_memory().unwrap();
        record_failure(&db, "artist", "no candidate matches", 1_000);
        let provider = FakeProvider {
            artist_responses: Mutex::new(VecDeque::from([Ok(vec![ArtistCandidate {
                id: "11111111-1111-1111-1111-111111111111".to_string(),
                name: "Artist".to_string(),
                score: None,
            }])])),
            group_responses: Mutex::new(VecDeque::from([Err(DiscographyError::Transport(
                "service unavailable".to_string(),
            ))])),
            ..FakeProvider::default()
        };

        // Bypass so the fresh row cannot be replayed, forcing the resolution to
        // actually run.
        discover_artist_albums_at(
            &provider,
            &db,
            "Artist",
            &DiscographyConfig::default(),
            1_100,
            FailureCacheUse::Bypass,
        )
        .await;

        assert!(
            db.get_discography_failure("artist").unwrap().is_none(),
            "the name resolved, so the recorded failure is obsolete"
        );
    }

    #[tokio::test]
    async fn an_inconsistent_search_page_is_not_recorded() {
        // A page whose reported total disagrees with its payload is a property of
        // one response, not a fact about the artist, so it must not suppress the
        // artist for the whole expiry. It is still treated as an artist problem
        // for this run, so the caller falls back exactly as it did before the
        // failure cache existed.
        let db = Database::open_in_memory().unwrap();
        let provider = FakeProvider::inconsistent_search();

        let outcome = discover_artist_albums_at(
            &provider,
            &db,
            "Artist",
            &DiscographyConfig::default(),
            1_000,
            FailureCacheUse::Honour,
        )
        .await;

        assert!(matches!(
            outcome,
            DiscoveryOutcome::LegacyFallback {
                kind: DiscoveryFailure::Unresolved,
                from_cache: false,
                ..
            }
        ));
        assert!(
            db.get_discography_failure("artist").unwrap().is_none(),
            "a single inconsistent response must never be remembered"
        );
    }

    #[tokio::test]
    async fn a_failure_row_never_shadows_a_usable_success_row() {
        // The two kinds of row are mutually exclusive by construction, but a
        // failed delete or a crash can leave both. A stale success row is still a
        // usable fallback, so it must win over a fresh failure row rather than
        // the artist being skipped.
        let db = Database::open_in_memory().unwrap();
        let groups = vec![group("1", "Studio", Some("2000"), Some("Album"), &[])];
        cache_groups(
            &db,
            "artist",
            "11111111-1111-1111-1111-111111111111",
            0,
            &groups,
        );
        record_failure(&db, "artist", "no candidate matches", 1_000_000);

        let provider = FakeProvider::default();
        // A longer failure expiry than success expiry is what lets a fresh failure
        // row coexist with a stale success row; the shipped defaults cannot.
        let config = DiscographyConfig {
            cache_days: 1,
            failure_cache_days: 30,
            ..DiscographyConfig::default()
        };

        let outcome = discover_artist_albums_at(
            &provider,
            &db,
            "Artist",
            &config,
            1_000_100,
            FailureCacheUse::Honour,
        )
        .await;

        assert!(
            matches!(
                outcome,
                DiscoveryOutcome::Authoritative {
                    provenance: DiscoveryProvenance::StaleCache { .. },
                    ..
                }
            ),
            "the stale success row must be used instead of being shadowed, got {outcome:?}"
        );
    }

    #[tokio::test]
    async fn an_unknown_failure_kind_is_treated_as_a_miss() {
        let db = Database::open_in_memory().unwrap();
        db.upsert_discography_failure(&crate::db::DiscographyFailureEntry {
            artist_key: "unknown".to_string(),
            failure_kind: "something-new".to_string(),
            reason: "from a future version".to_string(),
            recorded_at: 1_000,
        })
        .unwrap();
        let provider = FakeProvider::unresolvable_recording();

        let outcome = discover_artist_albums_at(
            &provider,
            &db,
            "Unknown",
            &DiscographyConfig::default(),
            1_100,
            FailureCacheUse::Honour,
        )
        .await;

        assert!(matches!(
            outcome,
            DiscoveryOutcome::LegacyFallback {
                from_cache: false,
                ..
            }
        ));
        assert!(provider.total_calls() > 0);
    }

    #[tokio::test]
    async fn fresh_cache_avoids_provider_and_refilters_current_types() {
        let db = Database::open_in_memory().unwrap();
        let groups = vec![
            group("1", "Studio", Some("2000"), Some("Album"), &[]),
            group("2", "Live", Some("2001"), Some("Album"), &["Live"]),
        ];
        cache_groups(
            &db,
            "artist",
            "11111111-1111-1111-1111-111111111111",
            1_000,
            &groups,
        );
        let provider = FakeProvider::default();
        let config = DiscographyConfig {
            allowed_types: vec![DiscographyReleaseType::LiveAlbum],
            ..DiscographyConfig::default()
        };

        let outcome = discover_artist_albums_at(
            &provider,
            &db,
            "Artist",
            &config,
            1_100,
            FailureCacheUse::Honour,
        )
        .await;

        assert!(matches!(outcome, DiscoveryOutcome::Authoritative {
            provenance: DiscoveryProvenance::FreshCache,
            ref albums,
        } if albums.iter().map(|album| album.title.as_str()).collect::<Vec<_>>() == ["Live"]));
        assert_eq!(provider.total_calls(), 0);
    }

    #[tokio::test]
    async fn failed_refresh_prefers_compatible_stale_cache() {
        let db = Database::open_in_memory().unwrap();
        let groups = vec![group("1", "Studio", Some("2000"), Some("Album"), &[])];
        cache_groups(
            &db,
            "artist",
            "11111111-1111-1111-1111-111111111111",
            0,
            &groups,
        );
        let provider = FakeProvider::failing("service unavailable");
        let config = DiscographyConfig::default();

        let outcome = discover_artist_albums_at(
            &provider,
            &db,
            "Artist",
            &config,
            31 * 86_400,
            FailureCacheUse::Honour,
        )
        .await;

        assert!(matches!(
            outcome,
            DiscoveryOutcome::Authoritative {
                provenance: DiscoveryProvenance::StaleCache { .. },
                ..
            }
        ));
    }

    #[tokio::test]
    async fn unresolved_without_cache_returns_visible_legacy_reason() {
        let db = Database::open_in_memory().unwrap();
        let provider = FakeProvider::artists(Vec::new());
        let outcome = discover_artist_albums_at(
            &provider,
            &db,
            "Unknown",
            &DiscographyConfig::default(),
            1_000,
            FailureCacheUse::Honour,
        )
        .await;

        assert!(matches!(
            outcome,
            DiscoveryOutcome::LegacyFallback {
                reason,
                kind,
                from_cache: false,
            } if kind == DiscoveryFailure::Unresolved
                    && reason.contains("artist could not be resolved safely")
        ));
    }

    #[tokio::test]
    async fn zero_cache_days_always_refreshes() {
        let db = Database::open_in_memory().unwrap();
        let groups = vec![group("1", "Studio", Some("2000"), Some("Album"), &[])];
        cache_groups(
            &db,
            "artist",
            "11111111-1111-1111-1111-111111111111",
            5_000,
            &groups,
        );
        let provider = FakeProvider::with_groups(groups);
        let config = DiscographyConfig {
            cache_days: 0,
            ..DiscographyConfig::default()
        };

        let outcome = discover_artist_albums_at(
            &provider,
            &db,
            "Artist",
            &config,
            5_000,
            FailureCacheUse::Honour,
        )
        .await;

        assert!(matches!(
            outcome,
            DiscoveryOutcome::Authoritative {
                provenance: DiscoveryProvenance::Refreshed,
                ..
            }
        ));
        assert_eq!(provider.total_calls(), 2);
    }

    #[tokio::test]
    async fn cache_is_stale_at_thirty_day_boundary() {
        let db = Database::open_in_memory().unwrap();
        let groups = vec![group("1", "Studio", Some("2000"), Some("Album"), &[])];
        let now = 2_000_000_000;
        cache_groups(
            &db,
            "artist",
            "11111111-1111-1111-1111-111111111111",
            now - 30 * 86_400,
            &groups,
        );
        let provider = FakeProvider::with_groups(groups);

        let outcome = discover_artist_albums_at(
            &provider,
            &db,
            "Artist",
            &DiscographyConfig::default(),
            now,
            FailureCacheUse::Honour,
        )
        .await;

        assert!(
            matches!(
                outcome,
                DiscoveryOutcome::Authoritative {
                    provenance: DiscoveryProvenance::Refreshed,
                    ..
                }
            ),
            "a cache aged exactly cache_days must refresh, not return FreshCache"
        );
        assert_eq!(provider.total_calls(), 2);
    }

    #[tokio::test]
    async fn future_cache_timestamp_is_stale() {
        let db = Database::open_in_memory().unwrap();
        let groups = vec![group("1", "Studio", Some("2000"), Some("Album"), &[])];
        cache_groups(
            &db,
            "artist",
            "11111111-1111-1111-1111-111111111111",
            10_001,
            &groups,
        );
        let provider = FakeProvider::failing("service unavailable");

        let outcome = discover_artist_albums_at(
            &provider,
            &db,
            "Artist",
            &DiscographyConfig::default(),
            10_000,
            FailureCacheUse::Honour,
        )
        .await;

        assert!(
            matches!(
                outcome,
                DiscoveryOutcome::Authoritative {
                    provenance: DiscoveryProvenance::StaleCache { age_days: 0, .. },
                    ..
                }
            ),
            "a future timestamp must never be FreshCache; its age must saturate at zero"
        );
        assert!(!matches!(
            outcome,
            DiscoveryOutcome::Authoritative {
                provenance: DiscoveryProvenance::FreshCache,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn configured_mbid_invalidates_other_cache_identity() {
        let configured = "22222222-2222-2222-2222-222222222222";
        let db = Database::open_in_memory().unwrap();
        cache_groups(
            &db,
            "artist",
            "11111111-1111-1111-1111-111111111111",
            5_000,
            &[],
        );
        let failing = FakeProvider::failing("service unavailable");
        let mut config = DiscographyConfig::default();
        config
            .artist_mbids
            .insert("Artist".to_string(), configured.to_string());

        let outcome = discover_artist_albums_at(
            &failing,
            &db,
            "Artist",
            &config,
            6_000,
            FailureCacheUse::Honour,
        )
        .await;

        assert!(
            matches!(outcome, DiscoveryOutcome::LegacyFallback { .. }),
            "a cache row for another MBID must never serve as a stale fallback"
        );
        assert_eq!(
            failing.lookup_calls.load(Ordering::SeqCst),
            1,
            "the configured MBID must be verified via artist_by_id"
        );
        assert_eq!(
            failing.artist_calls.load(Ordering::SeqCst),
            0,
            "a configured MBID must not trigger a name search"
        );

        let groups = vec![group("1", "Studio", Some("2000"), Some("Album"), &[])];
        let provider = FakeProvider::with_groups(groups);
        let outcome = discover_artist_albums_at(
            &provider,
            &db,
            "Artist",
            &config,
            6_000,
            FailureCacheUse::Honour,
        )
        .await;
        assert!(matches!(
            outcome,
            DiscoveryOutcome::Authoritative {
                provenance: DiscoveryProvenance::Refreshed,
                ..
            }
        ));
        let entry = db.get_discography_cache("artist").unwrap().unwrap();
        assert_eq!(
            entry.artist_mbid, configured,
            "the refreshed cache must carry the configured MBID the provider received"
        );
    }

    #[tokio::test]
    async fn corrupt_cache_is_deleted_then_refreshed() {
        let db = Database::open_in_memory().unwrap();
        db.upsert_discography_cache(&DiscographyCacheEntry {
            artist_key: "artist".into(),
            artist_mbid: "11111111-1111-1111-1111-111111111111".into(),
            canonical_artist: "Artist".into(),
            fetched_at: 1_000,
            release_groups_json: "not json".into(),
        })
        .unwrap();
        let groups = vec![group("1", "Studio", Some("2000"), Some("Album"), &[])];
        let provider = FakeProvider::with_groups(groups);

        let outcome = discover_artist_albums_at(
            &provider,
            &db,
            "Artist",
            &DiscographyConfig::default(),
            5_000,
            FailureCacheUse::Honour,
        )
        .await;

        assert!(matches!(
            outcome,
            DiscoveryOutcome::Authoritative {
                provenance: DiscoveryProvenance::Refreshed,
                ..
            }
        ));
        let entry = db.get_discography_cache("artist").unwrap().unwrap();
        assert!(
            serde_json::from_str::<Vec<ReleaseGroup>>(&entry.release_groups_json).is_ok(),
            "the replacement cache row must decode"
        );
    }

    #[tokio::test]
    async fn complete_empty_provider_result_is_authoritative_empty() {
        let db = Database::open_in_memory().unwrap();
        let provider = FakeProvider::with_groups(Vec::new());

        let outcome = discover_artist_albums_at(
            &provider,
            &db,
            "Artist",
            &DiscographyConfig::default(),
            1_000,
            FailureCacheUse::Honour,
        )
        .await;

        assert!(
            matches!(
                outcome,
                DiscoveryOutcome::AuthoritativeEmpty {
                    provenance: DiscoveryProvenance::Refreshed,
                }
            ),
            "a resolved artist with zero release groups is authoritative, never legacy"
        );
    }

    #[tokio::test]
    async fn cache_write_failure_does_not_discard_refresh() {
        let db = Database::open_in_memory().unwrap();
        db.conn
            .execute_batch("DROP TABLE discography_cache")
            .unwrap();
        let groups = vec![group("1", "Studio", Some("2000"), Some("Album"), &[])];
        let provider = FakeProvider::with_groups(groups);

        let outcome = discover_artist_albums_at(
            &provider,
            &db,
            "Artist",
            &DiscographyConfig::default(),
            1_000,
            FailureCacheUse::Honour,
        )
        .await;

        assert!(matches!(
            outcome,
            DiscoveryOutcome::Authoritative {
                provenance: DiscoveryProvenance::Refreshed,
                ..
            }
        ));
        assert_eq!(provider.total_calls(), 2);
    }

    #[tokio::test]
    async fn oversized_cache_payload_uses_stale_or_legacy() {
        let oversized = vec![group(
            "1",
            &"x".repeat(17 * 1024 * 1024),
            Some("2000"),
            Some("Album"),
            &[],
        )];

        let db = Database::open_in_memory().unwrap();
        let small = vec![group("1", "Studio", Some("2000"), Some("Album"), &[])];
        cache_groups(
            &db,
            "artist",
            "11111111-1111-1111-1111-111111111111",
            0,
            &small,
        );
        let provider = FakeProvider::with_groups(oversized.clone());
        let outcome = discover_artist_albums_at(
            &provider,
            &db,
            "Artist",
            &DiscographyConfig::default(),
            31 * 86_400,
            FailureCacheUse::Honour,
        )
        .await;

        assert!(
            matches!(outcome, DiscoveryOutcome::Authoritative {
            provenance: DiscoveryProvenance::StaleCache { .. },
            ref albums,
        } if albums.iter().map(|album| album.title.as_str()).collect::<Vec<_>>() == ["Studio"]),
            "an oversized refresh payload must fall back to the compatible stale cache"
        );

        let db = Database::open_in_memory().unwrap();
        let provider = FakeProvider::with_groups(oversized);
        let outcome = discover_artist_albums_at(
            &provider,
            &db,
            "Artist",
            &DiscographyConfig::default(),
            31 * 86_400,
            FailureCacheUse::Honour,
        )
        .await;
        assert!(
            matches!(outcome, DiscoveryOutcome::LegacyFallback { .. }),
            "an oversized payload without a cache must fall back to legacy"
        );
    }

    #[test]
    fn dominant_selection_logs_its_evidence_at_debug() {
        let db = Database::open_in_memory().unwrap();
        // A distinctive artist spelling, deliberately unlike the Ils fixture the
        // parallel dominance tests share: the capture window separates capture
        // windows from each other, but a test that is not capturing still
        // contributes records while a window is open, so every asserted value
        // here has to be one only this test can produce.
        let provider = FakeProvider {
            artist_responses: Mutex::new(VecDeque::from([Ok(vec![
                scored(
                    "11111111-aaaa-4aaa-8aaa-000000000001",
                    "Dominance Probe",
                    Some(100),
                ),
                scored(
                    "11111111-aaaa-4aaa-8aaa-000000000002",
                    "DOMINANCE PROBE",
                    Some(87),
                ),
            ])])),
            group_responses: Mutex::new(VecDeque::from([Ok(vec![group(
                "1",
                "Studio",
                Some("2000"),
                Some("Album"),
                &[],
            )])])),
            ..FakeProvider::default()
        };
        let capture = crate::test_support::LogCapture::start();

        let outcome = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(discover_artist_albums_at(
                &provider,
                &db,
                "dominance probe",
                &DiscographyConfig::default(),
                1_000,
                FailureCacheUse::Honour,
            ));

        assert!(matches!(
            outcome,
            DiscoveryOutcome::Authoritative {
                provenance: DiscoveryProvenance::Refreshed,
                ..
            }
        ));

        let logs = capture.text();
        // Every assertion is made against the single record that carries the
        // selected MBID. Asserting over the whole buffer would also accept
        // another test's records, and asserting the level over the buffer would
        // accept a DEBUG copy of this message.
        let record = logs
            .lines()
            .find(|line| line.contains("selected_mbid=11111111-aaaa-4aaa-8aaa-000000000001"))
            .unwrap_or_else(|| panic!("no dominance evidence record, got:\n{logs}"));
        assert!(
            record.starts_with("DEBUG "),
            "the evidence must be logged at DEBUG, got: {record}"
        );
        for field in [
            "resolved duplicate canonical artist name by search-score dominance",
            "artist=dominance probe",
            "selected_name=Dominance Probe",
            "exact_matches=2",
            "top_score=100",
            "runner_up_score=87",
            "margin=13",
        ] {
            assert!(record.contains(field), "missing {field:?} in: {record}");
        }
    }

    #[tokio::test]
    async fn dominant_selection_persists_the_chosen_mbid() {
        let db = Database::open_in_memory().unwrap();
        let provider = FakeProvider {
            artist_responses: Mutex::new(VecDeque::from([Ok(ils_candidates())])),
            group_responses: Mutex::new(VecDeque::from([Ok(vec![group(
                "1",
                "Studio",
                Some("2000"),
                Some("Album"),
                &[],
            )])])),
            ..FakeProvider::default()
        };

        let outcome = discover_artist_albums_at(
            &provider,
            &db,
            "Ils",
            &DiscographyConfig::default(),
            1_000,
            FailureCacheUse::Honour,
        )
        .await;
        assert!(matches!(
            outcome,
            DiscoveryOutcome::Authoritative {
                provenance: DiscoveryProvenance::Refreshed,
                ..
            }
        ));

        assert_eq!(
            provider.requested_group_mbids.lock().unwrap().as_slice(),
            ["16b97aaa-d7c0-469f-8c97-47c705b2d02f"]
        );
        let entry = db.get_discography_cache("ils").unwrap().unwrap();
        assert_eq!(entry.artist_mbid, "16b97aaa-d7c0-469f-8c97-47c705b2d02f");
        assert_eq!(entry.canonical_artist, "Ils");
    }

    #[tokio::test]
    async fn unresolved_dominance_prefers_stale_cache() {
        let db = Database::open_in_memory().unwrap();
        let groups = vec![group("1", "Studio", Some("2000"), Some("Album"), &[])];
        cache_groups(
            &db,
            "ils",
            "16b97aaa-d7c0-469f-8c97-47c705b2d02f",
            0,
            &groups,
        );
        // 100 versus 100 is a tied top score, so the refresh cannot resolve.
        let provider = FakeProvider {
            artist_responses: Mutex::new(VecDeque::from([Ok(vec![
                scored("16b97aaa-d7c0-469f-8c97-47c705b2d02f", "Ils", Some(100)),
                scored("638e9183-2cde-4c07-b1d5-1f0e0361ed1c", "Ils", Some(100)),
            ])])),
            ..FakeProvider::default()
        };

        let outcome = discover_artist_albums_at(
            &provider,
            &db,
            "Ils",
            &DiscographyConfig::default(),
            31 * 86_400,
            FailureCacheUse::Honour,
        )
        .await;

        assert!(matches!(
            outcome,
            DiscoveryOutcome::Authoritative {
                provenance: DiscoveryProvenance::StaleCache { .. },
                ..
            }
        ));
    }

    #[tokio::test]
    async fn unresolved_dominance_without_cache_reports_legacy_fallback() {
        let db = Database::open_in_memory().unwrap();
        let provider = FakeProvider {
            artist_responses: Mutex::new(VecDeque::from([Ok(vec![
                scored("a", "Ils", Some(99)),
                scored("b", "Ils", Some(98)),
            ])])),
            ..FakeProvider::default()
        };

        let outcome = discover_artist_albums_at(
            &provider,
            &db,
            "Ils",
            &DiscographyConfig::default(),
            1_000,
            FailureCacheUse::Honour,
        )
        .await;

        assert!(matches!(
            outcome,
            DiscoveryOutcome::LegacyFallback {
                reason,
                kind,
                from_cache: false,
            } if kind == DiscoveryFailure::Unresolved
                    && reason.contains("artist could not be resolved safely")
                    && reason.contains("below 100")
        ));
    }

    #[tokio::test]
    async fn configured_mbid_bypasses_score_ranking() {
        let db = Database::open_in_memory().unwrap();
        let provider = FakeProvider {
            group_responses: Mutex::new(VecDeque::from([Ok(vec![group(
                "1",
                "Studio",
                Some("2000"),
                Some("Album"),
                &[],
            )])])),
            ..FakeProvider::default()
        };
        let mut config = DiscographyConfig::default();
        config.artist_mbids.insert(
            "Ils".to_string(),
            "16b97aaa-d7c0-469f-8c97-47c705b2d02f".to_string(),
        );

        let outcome = discover_artist_albums_at(
            &provider,
            &db,
            "Ils",
            &config,
            1_000,
            FailureCacheUse::Honour,
        )
        .await;

        assert!(matches!(
            outcome,
            DiscoveryOutcome::Authoritative {
                provenance: DiscoveryProvenance::Refreshed,
                ..
            }
        ));
        assert_eq!(provider.artist_calls.load(Ordering::SeqCst), 0);
        assert_eq!(provider.lookup_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn fresh_cache_skips_dominance_ranking() {
        let db = Database::open_in_memory().unwrap();
        let groups = vec![group("1", "Studio", Some("2000"), Some("Album"), &[])];
        cache_groups(
            &db,
            "ils",
            "16b97aaa-d7c0-469f-8c97-47c705b2d02f",
            1_000,
            &groups,
        );
        let provider = FakeProvider {
            artist_responses: Mutex::new(VecDeque::from([Ok(ils_candidates())])),
            ..FakeProvider::default()
        };

        let outcome = discover_artist_albums_at(
            &provider,
            &db,
            "Ils",
            &DiscographyConfig::default(),
            1_100,
            FailureCacheUse::Honour,
        )
        .await;

        assert!(matches!(
            outcome,
            DiscoveryOutcome::Authoritative {
                provenance: DiscoveryProvenance::FreshCache,
                ..
            }
        ));
        assert_eq!(provider.total_calls(), 0);
    }

    #[test]
    fn unusable_candidate_data_is_classified_as_a_provider_failure() {
        // A malformed response must count towards the circuit breaker. Calling
        // it "unresolved" would let a broken provider outrun the breaker and
        // grind through the whole library, which is what the breaker prevents.
        let invalid = DiscographyError::InvalidCandidateData("invalid score 255".into());
        assert_eq!(
            DiscoveryFailure::from_error(&invalid),
            DiscoveryFailure::Provider
        );
        // A wrong page is a provider defect too, so it must reach the breaker.
        let pagination = DiscographyError::IncompletePagination("non-zero offset".into());
        assert_eq!(
            DiscoveryFailure::from_error(&pagination),
            DiscoveryFailure::Provider
        );
        let unresolved = DiscographyError::ArtistUnresolved("no candidate matches".into());
        assert_eq!(
            DiscoveryFailure::from_error(&unresolved),
            DiscoveryFailure::Unresolved
        );
    }

    #[tokio::test]
    async fn unresolved_artist_reports_the_unresolved_kind() {
        let db = Database::open_in_memory().unwrap();
        let provider = FakeProvider::artists(Vec::new());
        let outcome = discover_artist_albums_at(
            &provider,
            &db,
            "Unknown",
            &DiscographyConfig::default(),
            1_000,
            FailureCacheUse::Honour,
        )
        .await;

        assert!(matches!(
            outcome,
            DiscoveryOutcome::LegacyFallback {
                kind: DiscoveryFailure::Unresolved,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn provider_error_reports_the_provider_kind() {
        let db = Database::open_in_memory().unwrap();
        let provider = FakeProvider::failing("connection reset");
        let outcome = discover_artist_albums_at(
            &provider,
            &db,
            "Artist",
            &DiscographyConfig::default(),
            1_000,
            FailureCacheUse::Honour,
        )
        .await;

        assert!(matches!(
            outcome,
            DiscoveryOutcome::LegacyFallback {
                kind: DiscoveryFailure::Provider,
                ..
            }
        ));
    }
}
