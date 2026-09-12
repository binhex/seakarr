use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use unicode_normalization::UnicodeNormalization;

use crate::config::DiscographyConfig;
use crate::config::DiscographyReleaseType;
use crate::db::{Database, DiscographyCacheEntry};

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtistCandidate {
    pub id: String,
    pub name: String,
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

/// Resolve the artist whose canonical name normalizes to `artist_key`.
/// Returns the sole match, or `ArtistUnresolved` naming zero or the exact
/// duplicate count otherwise.
pub fn resolve_exact_artist(
    artist_key: &str,
    candidates: &[ArtistCandidate],
) -> std::result::Result<ArtistCandidate, DiscographyError> {
    let key = normalize_catalog_key(artist_key);
    let matches: Vec<&ArtistCandidate> = candidates
        .iter()
        .filter(|candidate| normalize_catalog_key(&candidate.name) == key)
        .collect();
    match matches.as_slice() {
        [only] => Ok((*only).clone()),
        [] => Err(DiscographyError::ArtistUnresolved(format!(
            "no candidate matches {artist_key:?}"
        ))),
        _ => Err(DiscographyError::ArtistUnresolved(format!(
            "{} candidates match {artist_key:?}",
            matches.len()
        ))),
    }
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

fn reject_release(release: &ReleaseGroup, reason: &str) -> bool {
    tracing::debug!(
        release_group_id = %release.id,
        title = %release.title,
        reason,
        "excluding MusicBrainz release group"
    );
    false
}

/// Classify a release group against the configured friendly categories.
/// Unknown primary or secondary types are rejected; secondary types must all
/// be allowed; `Album` without secondaries requires `StudioAlbum`.
pub fn release_allowed(release: &ReleaseGroup, allowed: &[DiscographyReleaseType]) -> bool {
    let Some(primary) = release.primary_type.as_deref() else {
        return reject_release(release, "missing primary type");
    };
    let primary = primary.trim().to_ascii_lowercase();
    if primary != "album" && primary != "ep" && primary != "single" {
        return reject_release(release, &format!("unknown primary type {primary}"));
    }
    let mut secondaries = Vec::new();
    for value in &release.secondary_types {
        let Some(category) = secondary_category(value) else {
            return reject_release(release, &format!("unknown secondary type {}", value.trim()));
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
        reject_release(release, "release type is not enabled")
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

/// Filter allowed release groups with non-empty titles, deduplicate by the
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscoveryProvenance {
    FreshCache,
    Refreshed,
    StaleCache {
        age_days: u64,
        refresh_error: String,
    },
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
    },
}

/// Resolve an artist's conceptual albums, preferring a fresh discography
/// cache, then authoritative MusicBrainz data, then a compatible stale cache,
/// and finally a visible legacy fallback.
pub async fn discover_artist_albums(
    provider: &dyn DiscographyProvider,
    db: &Database,
    artist: &str,
    config: &DiscographyConfig,
) -> DiscoveryOutcome {
    discover_artist_albums_at(provider, db, artist, config, chrono::Utc::now().timestamp()).await
}

/// Time-injectable variant of [`discover_artist_albums`].
pub(crate) async fn discover_artist_albums_at(
    provider: &dyn DiscographyProvider,
    db: &Database,
    artist: &str,
    config: &DiscographyConfig,
    now: i64,
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

    let refresh = async {
        let resolved = match configured_mbid {
            Some(mbid) => provider.artist_by_id(mbid).await?,
            None => {
                let candidates = provider.search_artists(artist).await?;
                resolve_exact_artist(&artist_key, &candidates)?
            }
        };
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
        Err(error) => stale_or_legacy(cached, &config.allowed_types, now, error),
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
            reason: refresh_error.to_string(),
        };
    };
    let provenance = DiscoveryProvenance::StaleCache {
        age_days: stale_age_days(now, cached.entry.fetched_at),
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
            },
            ArtistCandidate {
                id: "2".into(),
                name: "AC DC".into(),
            },
        ];
        assert_eq!(resolve_exact_artist("ac/dc", &candidates).unwrap().id, "1");
        assert!(resolve_exact_artist(
            "AC DC",
            &[
                ArtistCandidate {
                    id: "2".into(),
                    name: "AC DC".into()
                },
                ArtistCandidate {
                    id: "3".into(),
                    name: " ac dc ".into()
                },
            ]
        )
        .is_err());
        assert!(resolve_exact_artist("Missing", &candidates).is_err());
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
        let buffer = Arc::new(Mutex::new(String::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_writer(CapturingWriter(Arc::clone(&buffer)))
            .with_max_level(tracing::Level::DEBUG)
            .with_ansi(false)
            .without_time()
            .finish();
        tracing::subscriber::with_default(subscriber, || {
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
        });

        let logs = buffer.lock().unwrap().clone();
        assert!(logs.contains("unknown-secondary"), "got: {logs}");
        assert!(logs.contains("unknown secondary type"), "got: {logs}");
        assert!(logs.contains("empty-title"), "got: {logs}");
        assert!(logs.contains("empty title"), "got: {logs}");
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
    use std::sync::{Arc, Mutex};

    #[derive(Clone)]
    struct CapturingWriter(Arc<Mutex<String>>);

    impl std::io::Write for CapturingWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .unwrap()
                .push_str(&String::from_utf8_lossy(bytes));
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturingWriter {
        type Writer = CapturingWriter;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    #[derive(Default)]
    struct FakeProvider {
        artist_responses:
            Mutex<VecDeque<std::result::Result<Vec<ArtistCandidate>, DiscographyError>>>,
        group_responses: Mutex<VecDeque<std::result::Result<Vec<ReleaseGroup>, DiscographyError>>>,
        artist_calls: AtomicUsize,
        lookup_calls: AtomicUsize,
        group_calls: AtomicUsize,
        failure: Option<String>,
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
            })
        }

        async fn release_groups(
            &self,
            _artist_mbid: &str,
        ) -> std::result::Result<Vec<ReleaseGroup>, DiscographyError> {
            self.group_calls.fetch_add(1, Ordering::SeqCst);
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

        let outcome = discover_artist_albums_at(&provider, &db, "Artist", &config, 1_100).await;

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

        let outcome =
            discover_artist_albums_at(&provider, &db, "Artist", &config, 31 * 86_400).await;

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
        )
        .await;

        assert!(matches!(
            outcome,
            DiscoveryOutcome::LegacyFallback { reason }
                if reason.contains("artist could not be resolved safely")
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

        let outcome = discover_artist_albums_at(&provider, &db, "Artist", &config, 5_000).await;

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

        let outcome =
            discover_artist_albums_at(&provider, &db, "Artist", &DiscographyConfig::default(), now)
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

        let outcome = discover_artist_albums_at(&failing, &db, "Artist", &config, 6_000).await;

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
        let outcome = discover_artist_albums_at(&provider, &db, "Artist", &config, 6_000).await;
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
        )
        .await;
        assert!(
            matches!(outcome, DiscoveryOutcome::LegacyFallback { .. }),
            "an oversized payload without a cache must fall back to legacy"
        );
    }
}
