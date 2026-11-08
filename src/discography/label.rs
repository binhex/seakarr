//! Identifying a MusicBrainz label from a `--label` value.
//!
//! A label value is a name or a MusicBrainz label UUID. The shape rule is
//! deliberately narrow: only a value that is *entirely* a UUID is treated as an ID,
//! so an ordinary label name is never reinterpreted. A label literally named like a
//! UUID is pinned with `discography.label_mbids`, keyed by that same text, and the pin
//! wins over the input's own shape. A hyphenated *near miss* is different: it is
//! refused outright as a probable typo, and no pin can rescue it, so a label whose
//! name is not a real UUID cannot be swept by name.

use std::collections::BTreeMap;

use crate::db::{Database, DiscographyCacheEntry};

use super::identifier::{pin_key as identifier_pin_key, valid_mbid};
use super::{
    cache_is_fresh, check_cache_payload_size, dominant_candidate_index, normalize_catalog_key,
    record_resolution_failure, replayable_failure, stale_age_days, DiscographyConfig,
    DiscographyError, DiscographyProvider, DiscoveryFailure, DiscoveryProvenance, FailureCacheUse,
    ReleaseGroup, ResolveKind,
};

/// What one `--label` element means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LabelInput {
    /// A label name, kept as typed.
    Name(String),
    /// A hyphenated MusicBrainz label UUID, lower-cased.
    Mbid(String),
    /// A value shaped like a UUID but not a usable one. Every caller rejects it
    /// before any request, so a typo is never searched for as a name.
    Malformed { value: String, reason: String },
}

impl LabelInput {
    /// Build from `LabelRequest`'s two fields, classifying as [`LabelInput::parse`]
    /// would.
    ///
    /// A caller may hand over a value the CLI never saw, so this must not assume the
    /// value was already classified: a UUID-shaped near miss stays `Malformed` and is
    /// refused before any request, instead of becoming a name to search for.
    pub fn from_parts(input: String, mbid: Option<String>) -> Self {
        match mbid {
            Some(mbid) => Self::Mbid(mbid.to_ascii_lowercase()),
            // Classified exactly as `parse` would, so a near miss stays `Malformed`, a
            // UUID-shaped value stays an ID, and a plain name stays a name.
            None => Self::parse(&input),
        }
    }

    /// Classify a `--label` value by its shape.
    ///
    /// Total: anything that is not UUID-shaped stays a name, so classification
    /// never drops a label. A UUID-shaped value that is not a usable UUID is
    /// reported by [`LabelInput::validate`] instead of being searched for as a
    /// name.
    pub fn parse(raw: &str) -> Self {
        let trimmed = raw.trim();
        if valid_mbid(trimmed) {
            return Self::Mbid(trimmed.to_ascii_lowercase());
        }
        if looks_like_mbid(trimmed) {
            return Self::Malformed {
                value: raw.to_string(),
                reason: format!(
                    "--label {raw:?} is hyphenated like a MusicBrainz ID but is not one \
                     (four hyphens separating five hexadecimal groups)"
                ),
            };
        }
        Self::Name(raw.to_string())
    }

    /// The value to show and to search for: a name as typed, an ID normalised.
    pub fn display(&self) -> &str {
        match self {
            Self::Name(value) | Self::Mbid(value) => value,
            Self::Malformed { value, .. } => value,
        }
    }

    /// The MusicBrainz ID to look up, when this input is one.
    pub fn mbid(&self) -> Option<&str> {
        match self {
            Self::Mbid(value) => Some(value),
            _ => None,
        }
    }

    /// Why this value cannot be used, when it is a near-miss UUID.
    pub fn malformed_reason(&self) -> Option<&str> {
        match self {
            Self::Malformed { reason, .. } => Some(reason),
            _ => None,
        }
    }

    /// Reject a value that claims to be an ID but cannot be used as one, and a
    /// blank name.
    pub fn validate(&self) -> std::result::Result<(), String> {
        match self {
            Self::Name(value) if value.trim().is_empty() => {
                Err(format!("--label {value:?} is blank"))
            }
            Self::Name(value) if normalize_catalog_key(value).is_empty() => Err(format!(
                "--label {value:?} carries no name once apostrophe marks and spacing are folded"
            )),
            Self::Name(_) => Ok(()),
            Self::Mbid(value) if !valid_mbid(value) => {
                Err(format!("--label ID {value:?} is not a MusicBrainz ID"))
            }
            Self::Mbid(_) => Ok(()),
            Self::Malformed { reason, .. } => Err(reason.clone()),
        }
    }

    /// The key for the catalogue and failure caches.
    ///
    /// `label:name:<key>` for a name and `label:id:<uuid>` for an ID. The name/ID
    /// marker is structural rather than folded, so a label literally named like an ID
    /// cannot share a row with that ID. The leading `label:` keeps the whole space
    /// apart from the artist keys that share the table (`name`, `isni:`, `ipi:`,
    /// `mbid:`).
    ///
    /// An artist literally named `label:name:vault` still folds onto a label key. Each
    /// cache survives that differently: a catalogue row's payload decodes only as its
    /// own type, so a mismatched row fails to parse and is refetched, while a failure
    /// row stores only a kind and a reason, so such an artist and a label of the same
    /// name would replay each other's unresolved failure for
    /// `discography.failure_cache_days`. The prefix makes that require a deliberately
    /// adversarial name and costs one suppressed lookup, not a wrong write.
    pub fn cache_key(&self) -> String {
        match self {
            Self::Mbid(value) => format!("label:id:{value}"),
            Self::Name(value) | Self::Malformed { value, .. } => {
                format!("label:name:{}", identifier_pin_key(value))
            }
        }
    }
}

/// The MBID pinned for this input, when one is configured.
///
/// The lookup is by the value the operator gave, whatever its shape, so a pin wins
/// over the input's own shape: a label literally named like a UUID is read as an ID
/// by [`LabelInput::parse`], and the pin keyed by that same text is what rescues it.
/// An unrelated pin never matches an ID input, because the key would be a different
/// string.
pub fn pinned_mbid<'a>(pins: &'a BTreeMap<String, String>, input: &LabelInput) -> Option<&'a str> {
    let key = identifier_pin_key(input.display());
    pins.iter()
        .find(|(pinned, _)| identifier_pin_key(pinned) == key)
        .map(|(_, mbid)| mbid.as_str())
}

/// True when `value` is hyphenated like a UUID but is not one.
fn looks_like_mbid(value: &str) -> bool {
    let stripped: String = value
        .chars()
        .filter(|character| !character.is_whitespace() && *character != '-')
        .collect();
    value.matches('-').count() == 4
        && (30..=34).contains(&stripped.len())
        && value
            .split('-')
            .all(|group| !group.is_empty() && group.chars().all(|c| c.is_ascii_hexdigit()))
}

/// One label as MusicBrainz describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelCandidate {
    pub id: String,
    pub name: String,
    /// MusicBrainz search relevance score. `None` for a lookup by ID, which
    /// carries no score and needs none.
    pub score: Option<u8>,
    /// MusicBrainz label code, carried so an ambiguity message is actionable.
    pub label_code: Option<u32>,
    /// Disambiguation comment, e.g. "UK drum & bass".
    pub disambiguation: Option<String>,
}

/// Candidates listed in one refusal message.
pub const MAX_LISTED_LABELS: usize = 10;

/// Resolve a label name to exactly one MusicBrainz candidate.
///
/// The rules are the artist rules: one canonical exact-name match is accepted;
/// duplicates are accepted only under the shared dominance rule; zero matches is
/// a refusal that lists what MusicBrainz did return.
pub fn resolve_label(
    label_key: &str,
    candidates: &[LabelCandidate],
) -> std::result::Result<LabelCandidate, DiscographyError> {
    let key = normalize_catalog_key(label_key);
    let matches: Vec<&LabelCandidate> = candidates
        .iter()
        .filter(|candidate| normalize_catalog_key(&candidate.name) == key)
        .collect();
    match matches.as_slice() {
        [] => Err(DiscographyError::LabelUnresolved(format!(
            "no MusicBrainz label matches {label_key:?}{}",
            nearest_labels(candidates)
        ))),
        [only] => Ok((*only).clone()),
        many => {
            let dominance = dominant_candidate_index(
                ResolveKind::Label,
                label_key,
                many,
                |candidate| candidate.score,
                |candidate| candidate.id.as_str(),
            )
            // The dominance rules answer in scores; the operator needs names. Every
            // ambiguity is therefore answered with the same bounded candidate list the
            // zero-match branch uses, so the refusal can be acted on by pinning one of the
            // listed IDs.
            .map_err(|error| match error {
                DiscographyError::LabelUnresolved(reason) => DiscographyError::LabelUnresolved(
                    format!("{reason}{}", nearest_labels(candidates)),
                ),
                other => other,
            })?;
            Ok((*many[dominance.index]).clone())
        }
    }
}

/// A bounded, human answer to "which label did you mean?".
///
/// Each entry carries the MBID: the refusal's whole purpose is to let the operator pin the
/// right one with `discography.label_mbids`, and a name alone cannot do that.
fn nearest_labels(candidates: &[LabelCandidate]) -> String {
    if candidates.is_empty() {
        return String::new();
    }
    let listed: Vec<String> = candidates
        .iter()
        .take(MAX_LISTED_LABELS)
        .map(describe_label)
        .collect();
    format!("; nearest: {}", listed.join(", "))
}

fn describe_label(candidate: &LabelCandidate) -> String {
    let mut detail: Vec<String> = Vec::new();
    if let Some(score) = candidate.score {
        detail.push(format!("score {score}"));
    }
    if let Some(code) = candidate.label_code {
        detail.push(format!("label code {code}"));
    }
    if let Some(comment) = candidate
        .disambiguation
        .as_deref()
        .map(str::trim)
        .filter(|comment| !comment.is_empty())
    {
        detail.push(comment.to_string());
    }
    if detail.is_empty() {
        format!("{} ({})", candidate.name, candidate.id)
    } else {
        format!(
            "{} ({}, id {})",
            candidate.name,
            detail.join(", "),
            candidate.id
        )
    }
}

/// One release on a label, with the release group it belongs to.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LabelRelease {
    pub release_id: String,
    pub release_group: ReleaseGroup,
    /// The full artist credit, as MusicBrainz spells it.
    pub artist_credit: String,
}

/// A label's catalogue, plus what the payload could not describe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelCatalogue {
    pub releases: Vec<LabelRelease>,
    /// Releases whose payload could not describe them: no release group, or an artist
    /// credit with a part that names nothing (no credit name and no artist name).
    /// Counted, never guessed: an attributed guess would download the wrong album.
    pub incomplete: usize,
}

/// Version of the cached payload. A row with another version is refetched, so a
/// future field change cannot be read as if it were this one.
pub const LABEL_CACHE_VERSION: u32 = 1;

/// What `discography_cache` holds under a `label:` key.
///
/// The resolved label's name and ID live in the row's own `canonical_artist` and
/// `artist_mbid` columns, so they are not duplicated here.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LabelCacheEntry {
    pub version: u32,
    pub releases: Vec<LabelRelease>,
    pub incomplete: usize,
}

/// What a label lookup produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LabelDiscoveryOutcome {
    Catalogue {
        releases: Vec<LabelRelease>,
        incomplete: usize,
        provenance: DiscoveryProvenance,
    },
    Failed {
        reason: String,
        kind: DiscoveryFailure,
        /// True when this failure was replayed from the failure cache rather
        /// than observed live.
        from_cache: bool,
    },
}

/// A label lookup's result, with the identity it resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelDiscovery {
    pub outcome: LabelDiscoveryOutcome,
    pub label_name: Option<String>,
    pub label_mbid: Option<String>,
}

/// Resolve a label and return its catalogue, preferring a fresh cache, then a
/// freshly recorded resolution failure unless bypassed or the label already has
/// a usable cache entry, then MusicBrainz, then a compatible stale cache, and
/// finally a typed failure.
pub async fn discover_label_releases(
    provider: &dyn DiscographyProvider,
    db: &Database,
    input: &LabelInput,
    config: &DiscographyConfig,
    failure_cache: FailureCacheUse,
) -> LabelDiscovery {
    discover_label_releases_at(
        provider,
        db,
        input,
        config,
        chrono::Utc::now().timestamp(),
        failure_cache,
    )
    .await
}

/// Time-injectable variant of [`discover_label_releases`].
pub(crate) async fn discover_label_releases_at(
    provider: &dyn DiscographyProvider,
    db: &Database,
    input: &LabelInput,
    config: &DiscographyConfig,
    now: i64,
    failure_cache: FailureCacheUse,
) -> LabelDiscovery {
    // Every caller reaches this function with the same full validation the CLI applies:
    // a near-miss UUID is never searched for as a name, a blank name is never searched
    // for at all, and an `Mbid` whose value is not a UUID never reaches the URL it would
    // be interpolated into. `malformed_reason` covers only the near-miss shape, so
    // validating here is what keeps the promise for a library caller that builds a
    // `LabelInput` variant directly.
    if let Err(reason) = input.validate() {
        return LabelDiscovery {
            outcome: LabelDiscoveryOutcome::Failed {
                reason,
                kind: DiscoveryFailure::Unresolved,
                from_cache: false,
            },
            label_name: None,
            label_mbid: None,
        };
    }
    let label_key = input.cache_key();
    let configured_mbid = pinned_mbid(&config.label_mbids, input);
    // A row must describe the label this run asked for. When the input names an ID, that ID
    // is the expectation even without a pin: a row written while a pin pointed elsewhere was
    // stored under this same key with the other label's MBID, and serving it after the pin
    // was removed would stage a completely different label's albums.
    let expected_mbid = configured_mbid.or_else(|| input.mbid());
    let cached = load_cached_catalogue(db, &label_key, expected_mbid);

    if let Some(cached) = &cached {
        if cache_is_fresh(cached.entry.fetched_at, config.cache_days, now) {
            return LabelDiscovery {
                outcome: LabelDiscoveryOutcome::Catalogue {
                    releases: cached.releases.clone(),
                    incomplete: cached.incomplete,
                    provenance: DiscoveryProvenance::FreshCache,
                },
                label_name: Some(cached.entry.canonical_artist.clone()),
                label_mbid: Some(cached.entry.artist_mbid.clone()),
            };
        }
    }

    if let Some(replayed) = replayable_failure(
        db,
        &label_key,
        config,
        now,
        failure_cache,
        configured_mbid,
        cached.is_some(),
    ) {
        let reason = if replayed.reason.trim().is_empty() {
            "label could not be resolved safely".to_string()
        } else {
            replayed.reason
        };
        return LabelDiscovery {
            outcome: LabelDiscoveryOutcome::Failed {
                reason,
                kind: DiscoveryFailure::Unresolved,
                from_cache: true,
            },
            label_name: None,
            label_mbid: None,
        };
    }

    let refresh = async {
        let candidate = match (input.mbid(), configured_mbid) {
            // A pinned MBID wins over every shape, exactly as it does for a name.
            (_, Some(mbid)) => provider.label_by_id(mbid).await?,
            (Some(mbid), None) => provider.label_by_id(mbid).await?,
            (None, None) => {
                let candidates = provider.search_labels(input.display()).await?;
                resolve_label(input.display(), &candidates)?
            }
        };
        let catalogue = provider.releases_by_label(&candidate.id).await?;
        Ok::<_, DiscographyError>((candidate, catalogue))
    };

    match refresh.await {
        Ok((candidate, catalogue)) => {
            if config.cache_days > 0 {
                if let Err(reason) = store_catalogue(db, &label_key, &candidate, &catalogue, now) {
                    tracing::warn!(
                        "failed to write label catalogue cache for {label_key:?}: {reason}"
                    );
                }
            }
            if let Err(error) = db.delete_discography_failure(&label_key) {
                tracing::warn!("failed to clear label failure row: {error}");
            }
            LabelDiscovery {
                outcome: LabelDiscoveryOutcome::Catalogue {
                    releases: catalogue.releases,
                    incomplete: catalogue.incomplete,
                    provenance: DiscoveryProvenance::Refreshed,
                },
                label_name: Some(candidate.name),
                label_mbid: Some(candidate.id),
            }
        }
        Err(error) => {
            // Only a name is worth remembering: a UUID or a pin always resolves
            // deterministically, so a row for one could never be replayed.
            if input.mbid().is_none() && configured_mbid.is_none() {
                record_resolution_failure(db, &label_key, config, now, &error, cached.is_some());
            }
            let kind = DiscoveryFailure::from_error(&error);
            match cached {
                Some(cached) => LabelDiscovery {
                    outcome: LabelDiscoveryOutcome::Catalogue {
                        releases: cached.releases,
                        incomplete: cached.incomplete,
                        provenance: DiscoveryProvenance::StaleCache {
                            age_days: stale_age_days(now, cached.entry.fetched_at),
                            kind,
                            refresh_error: error.to_string(),
                        },
                    },
                    label_name: Some(cached.entry.canonical_artist),
                    label_mbid: Some(cached.entry.artist_mbid),
                },
                None => LabelDiscovery {
                    outcome: LabelDiscoveryOutcome::Failed {
                        reason: error.to_string(),
                        kind,
                        from_cache: false,
                    },
                    label_name: None,
                    label_mbid: None,
                },
            }
        }
    }
}

struct CachedCatalogue {
    entry: DiscographyCacheEntry,
    releases: Vec<LabelRelease>,
    incomplete: usize,
}

/// Read a compatible, well-formed cache row. A row whose key or pinned MBID
/// does not match is a silent miss; corrupt and wrong-version rows are warned
/// about, deleted, and treated as a miss, so a cache can never block a lookup it
/// was not written for.
fn load_cached_catalogue(
    db: &Database,
    label_key: &str,
    configured_mbid: Option<&str>,
) -> Option<CachedCatalogue> {
    let entry = match db.get_discography_cache(label_key) {
        Ok(Some(entry)) => entry,
        Ok(None) => return None,
        Err(error) => {
            tracing::warn!("failed to read label catalogue cache: {error}");
            return None;
        }
    };
    // The column is `COLLATE NOCASE`, so the lookup may return the row under an
    // equivalently-cased key. Compare case-insensitively rather than re-folding: the
    // stored key is already folded, and re-folding a `label:`-prefixed key with the
    // artist rule lower-cases the ISNI check character the name form keeps upper-case,
    // which would miss the row and re-fetch the catalogue on every run.
    if !entry.artist_key.eq_ignore_ascii_case(label_key) {
        return None;
    }
    if let Some(mbid) = configured_mbid {
        if !entry.artist_mbid.eq_ignore_ascii_case(mbid) {
            return None;
        }
    }
    match serde_json::from_str::<LabelCacheEntry>(&entry.release_groups_json) {
        Ok(parsed) if parsed.version == LABEL_CACHE_VERSION => Some(CachedCatalogue {
            entry,
            releases: parsed.releases,
            incomplete: parsed.incomplete,
        }),
        Ok(parsed) => {
            tracing::warn!(
                "label catalogue cache for {label_key:?} has version {} (expected {LABEL_CACHE_VERSION}); deleting",
                parsed.version
            );
            if let Err(error) = db.delete_discography_cache(label_key) {
                tracing::warn!("failed to delete a stale label cache row: {error}");
            }
            None
        }
        Err(error) => {
            tracing::warn!(
                "label catalogue cache for {label_key:?} is corrupt ({error}); deleting"
            );
            if let Err(error) = db.delete_discography_cache(label_key) {
                tracing::warn!("failed to delete a corrupt label cache row: {error}");
            }
            None
        }
    }
}

/// Write the catalogue. Failure to cache is a warning at the call site, never a
/// reason to lose the catalogue that was just fetched.
fn store_catalogue(
    db: &Database,
    label_key: &str,
    candidate: &LabelCandidate,
    catalogue: &LabelCatalogue,
    now: i64,
) -> std::result::Result<(), String> {
    let payload = serde_json::to_string(&LabelCacheEntry {
        version: LABEL_CACHE_VERSION,
        releases: catalogue.releases.clone(),
        incomplete: catalogue.incomplete,
    })
    .map_err(|error| format!("failed to serialize label cache: {error}"))?;
    check_cache_payload_size(&payload).map_err(|error| error.to_string())?;
    db.upsert_discography_cache(&DiscographyCacheEntry {
        artist_key: label_key.to_string(),
        artist_mbid: candidate.id.clone(),
        canonical_artist: candidate.name.clone(),
        fetched_at: now,
        release_groups_json: payload,
    })
    .map_err(|error| format!("failed to write label cache row: {error}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOSPITAL_LABEL_MBID: &str = "8562598d-60d1-4bd6-8b85-4d709e951770";

    #[test]
    fn a_name_stays_a_name_and_keeps_its_spelling() {
        let input = LabelInput::parse("  Hospital Records ");
        assert_eq!(input, LabelInput::Name("  Hospital Records ".to_string()));
        assert_eq!(input.display(), "  Hospital Records ");
        assert_eq!(input.mbid(), None);
        assert!(input.validate().is_ok());
    }

    #[test]
    fn a_uuid_is_an_id_and_is_lower_cased() {
        let input = LabelInput::parse(&HOSPITAL_LABEL_MBID.to_ascii_uppercase());
        assert_eq!(input, LabelInput::Mbid(HOSPITAL_LABEL_MBID.to_string()));
        assert_eq!(input.mbid(), Some(HOSPITAL_LABEL_MBID));
        assert!(input.validate().is_ok());
    }

    #[test]
    fn a_uuid_shaped_typo_is_reported_rather_than_searched_for() {
        let input = LabelInput::parse("8562598d-60d1-4bd6-8b85-4d709e95177");
        assert!(
            input.validate().is_err(),
            "a short UUID must not become a name"
        );
        assert!(
            input
                .malformed_reason()
                .unwrap()
                .contains("hyphenated like a MusicBrainz ID"),
            "got {:?}",
            input.malformed_reason()
        );
    }

    #[test]
    fn a_blank_value_is_rejected() {
        assert!(LabelInput::parse("   ").validate().is_err());
    }

    #[test]
    fn names_and_ids_get_distinct_cache_keys() {
        assert_eq!(
            LabelInput::parse("Hospital Records").cache_key(),
            "label:name:hospital records"
        );
        assert_eq!(
            LabelInput::parse(HOSPITAL_LABEL_MBID).cache_key(),
            format!("label:id:{HOSPITAL_LABEL_MBID}")
        );
    }

    #[test]
    fn a_label_named_like_an_id_cannot_share_that_ids_cache_row() {
        // The name/ID marker is structural, so the two readings of the same text are
        // different rows rather than the same row read two ways.
        assert_ne!(
            LabelInput::parse(HOSPITAL_LABEL_MBID).cache_key(),
            LabelInput::Name(HOSPITAL_LABEL_MBID.to_string()).cache_key()
        );
    }

    #[test]
    fn from_parts_classifies_the_same_way_parse_does() {
        // A UUID-shaped value with no recorded mbid is still an ID, not a name to
        // search for: the two readings have different cache keys.
        assert_eq!(
            LabelInput::from_parts(HOSPITAL_LABEL_MBID.to_string(), None),
            LabelInput::Mbid(HOSPITAL_LABEL_MBID.to_string())
        );
        assert_eq!(
            LabelInput::from_parts("  Hospital Records ".into(), None),
            LabelInput::Name("  Hospital Records ".to_string())
        );
    }

    #[test]
    fn validate_rejects_an_unusable_id_and_a_name_that_folds_away() {
        assert!(
            LabelInput::Mbid("not-a-uuid".to_string())
                .validate()
                .is_err(),
            "an Mbid must carry a real MusicBrainz ID"
        );
        // A typographic apostrophe folds away entirely, so the name carries nothing.
        assert!(
            LabelInput::Name(String::from("\u{2019}"))
                .validate()
                .is_err(),
            "a name made only of folded-away punctuation carries nothing"
        );
        assert!(LabelInput::parse(HOSPITAL_LABEL_MBID).validate().is_ok());
        assert!(LabelInput::parse("Hospital Records").validate().is_ok());
    }

    #[test]
    fn from_parts_keeps_a_malformed_value_malformed() {
        // A library caller can build a request without the CLI's validation, so the
        // near-miss shape must survive construction rather than become a name.
        let built = LabelInput::from_parts("8562598d-60d1-4bd6-8b85-4d709e95177".into(), None);
        assert!(built.malformed_reason().is_some());
        assert!(
            built.validate().is_err(),
            "a malformed value must still be refused"
        );

        // A plain name is unaffected.
        assert_eq!(
            LabelInput::from_parts("Hospital Records".into(), None),
            LabelInput::Name("Hospital Records".to_string())
        );
    }

    #[test]
    fn from_parts_does_not_reclassify() {
        assert_eq!(
            LabelInput::from_parts("Hospital Records".into(), Some(HOSPITAL_LABEL_MBID.into())),
            LabelInput::Mbid(HOSPITAL_LABEL_MBID.to_string())
        );
        assert_eq!(
            LabelInput::from_parts("Hospital Records".into(), None),
            LabelInput::Name("Hospital Records".to_string())
        );
    }

    #[test]
    fn a_pinned_name_is_found_through_normalisation_and_a_uuid_is_not_looked_up() {
        let mut pins = BTreeMap::new();
        pins.insert(
            "hospital  records".to_string(),
            HOSPITAL_LABEL_MBID.to_string(),
        );

        let by_name = LabelInput::parse("Hospital Records");
        assert_eq!(pinned_mbid(&pins, &by_name), Some(HOSPITAL_LABEL_MBID));

        let by_id = LabelInput::parse(HOSPITAL_LABEL_MBID);
        assert_eq!(pinned_mbid(&pins, &by_id), None);
    }

    #[test]
    fn a_pin_wins_over_an_id_shaped_input() {
        let other = "c8471d13-3574-4040-be20-229656068504";
        let mut pins = BTreeMap::new();
        // A label literally named like a UUID is read as an ID, so the pin keyed by
        // that same text is the escape hatch the module doc promises.
        pins.insert(HOSPITAL_LABEL_MBID.to_string(), other.to_string());

        let input = LabelInput::parse(HOSPITAL_LABEL_MBID);
        assert_eq!(input, LabelInput::Mbid(HOSPITAL_LABEL_MBID.to_string()));
        assert_eq!(pinned_mbid(&pins, &input), Some(other));
    }

    fn candidate(id_suffix: u8, name: &str, score: Option<u8>, comment: &str) -> LabelCandidate {
        LabelCandidate {
            id: format!("8562598{d}-60d1-4bd6-8b85-4d709e951770", d = id_suffix),
            name: name.to_string(),
            score,
            label_code: None,
            disambiguation: (!comment.is_empty()).then(|| comment.to_string()),
        }
    }

    #[test]
    fn one_exact_match_resolves() {
        let candidates = vec![
            candidate(1, "Hospital Records", Some(100), "UK drum & bass"),
            candidate(2, "Hospital Records (US)", Some(62), "US noisecore label"),
        ];
        let resolved = resolve_label("Hospital Records", &candidates).unwrap();
        assert_eq!(resolved.id, candidates[0].id);
    }

    #[test]
    fn a_scored_namesake_loses_to_the_dominant_match() {
        let candidates = vec![
            candidate(1, "Hospital Records", Some(62), "US noisecore label"),
            candidate(2, "Hospital Records", Some(100), "UK drum & bass"),
        ];
        let resolved = resolve_label("hospital  records", &candidates).unwrap();
        assert_eq!(
            resolved.id, candidates[1].id,
            "the score decides, not the response order"
        );
    }

    #[test]
    fn a_narrow_margin_refuses_and_lists_the_evidence() {
        let candidates = vec![
            candidate(1, "Hospital Records", Some(100), "UK drum & bass"),
            candidate(2, "Hospital Records", Some(95), "US noisecore label"),
        ];
        let error = resolve_label("Hospital Records", &candidates).unwrap_err();
        assert!(
            matches!(&error, DiscographyError::LabelUnresolved(reason) if reason.contains("margin 5")),
            "got {error:?}"
        );
        // The README promises the refusal lists the candidates so one can be pinned;
        // a score margin alone is not actionable.
        let DiscographyError::LabelUnresolved(reason) = &error else {
            unreachable!("checked above");
        };
        assert!(
            reason.contains("nearest:") && reason.contains("UK drum & bass"),
            "an ambiguous refusal must name its candidates, got: {reason}"
        );
        assert!(
            reason.contains(&candidates[0].id) && reason.contains(&candidates[1].id),
            "both candidate IDs are needed to pin one, got: {reason}"
        );
    }

    #[test]
    fn a_missing_score_refuses_rather_than_guessing() {
        let candidates = vec![
            candidate(1, "Hospital Records", None, ""),
            candidate(2, "Hospital Records", Some(100), ""),
        ];
        let error = resolve_label("Hospital Records", &candidates).unwrap_err();
        assert!(
            matches!(&error, DiscographyError::InvalidCandidateData(reason) if reason.contains("no search score")),
            "got {error:?}"
        );
    }

    #[test]
    fn no_exact_match_lists_the_nearest_candidates() {
        let candidates = vec![candidate(
            1,
            "Hospital Records Limited",
            Some(88),
            "UK drum & bass",
        )];
        let error = resolve_label("Hospital Records", &candidates).unwrap_err();
        let DiscographyError::LabelUnresolved(reason) = &error else {
            panic!("expected LabelUnresolved, got {error:?}");
        };
        assert!(
            reason.contains("no MusicBrainz label matches"),
            "got {reason}"
        );
        assert!(reason.contains("Hospital Records Limited"), "got {reason}");
        assert!(reason.contains("score 88"), "got {reason}");
        assert!(reason.contains("UK drum & bass"), "got {reason}");
    }

    #[test]
    fn the_nearest_list_is_bounded() {
        let candidates: Vec<LabelCandidate> = (0..25)
            .map(|index| candidate(index, &format!("Label {index}"), Some(50), ""))
            .collect();
        let error = resolve_label("Nothing Like It", &candidates).unwrap_err();
        let DiscographyError::LabelUnresolved(reason) = &error else {
            panic!("expected LabelUnresolved, got {error:?}");
        };
        assert!(reason.contains("Label 9"), "the first ten are listed");
        assert!(
            !reason.contains("Label 10"),
            "the eleventh is not: {reason}"
        );
    }

    #[test]
    fn an_empty_candidate_list_is_reported_without_a_nearest_list() {
        let error = resolve_label("Hospital Records", &[]).unwrap_err();
        let DiscographyError::LabelUnresolved(reason) = &error else {
            panic!("expected LabelUnresolved, got {error:?}");
        };
        assert!(
            reason.contains("no MusicBrainz label matches"),
            "got {reason}"
        );
        assert!(
            !reason.contains("nearest"),
            "an empty response has nothing to list: {reason}"
        );
    }

    #[test]
    fn the_nearest_list_describes_code_bearing_and_bare_candidates() {
        let mut coded = candidate(1, "Hospital Records", Some(88), "UK drum & bass");
        coded.label_code = Some(12939);
        let bare = LabelCandidate {
            id: "c8471d13-3574-4040-be20-229656068504".to_string(),
            name: "Hospital Records US".to_string(),
            score: None,
            label_code: None,
            disambiguation: Some("   ".to_string()),
        };

        let error = resolve_label("Nothing Like It", &[coded, bare]).unwrap_err();
        let DiscographyError::LabelUnresolved(reason) = &error else {
            panic!("expected LabelUnresolved, got {error:?}");
        };
        assert!(reason.contains("label code 12939"), "got {reason}");
        assert!(reason.contains("score 88"), "got {reason}");
        assert!(reason.contains("UK drum & bass"), "got {reason}");
        assert!(
            reason.contains("Hospital Records US (c8471d13-3574-4040-be20-229656068504)"),
            "a candidate with no usable detail falls back to name and id: {reason}"
        );
    }

    #[test]
    fn a_malformed_value_displays_itself_and_has_no_id() {
        let input = LabelInput::parse("8562598d-60d1-4bd6-8b85-4d709e95177");
        assert_eq!(input.display(), "8562598d-60d1-4bd6-8b85-4d709e95177");
        assert_eq!(input.mbid(), None, "a malformed value is not an ID");
        assert!(input.malformed_reason().is_some());
    }

    use super::super::{FailureCacheUse, SECONDS_PER_DAY};
    use crate::config::DiscographyConfig;
    use crate::db::{Database, DiscographyCacheEntry, DiscographyFailureEntry};
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    const LABEL_ID: &str = "8562598d-60d1-4bd6-8b85-4d709e951770";

    fn group(id: &str, title: &str) -> ReleaseGroup {
        ReleaseGroup {
            id: id.to_string(),
            title: title.to_string(),
            first_release_date: Some("2004-05-10".to_string()),
            primary_type: Some("Album".to_string()),
            secondary_types: Vec::new(),
        }
    }

    fn catalogue() -> LabelCatalogue {
        LabelCatalogue {
            releases: vec![LabelRelease {
                release_id: "release-1".to_string(),
                release_group: group("group-1", "Syncopated City"),
                artist_credit: "London Elektricity".to_string(),
            }],
            incomplete: 0,
        }
    }

    /// A provider that answers labels from a fixed table and counts calls.
    struct FakeLabelProvider {
        candidates: Vec<LabelCandidate>,
        catalogue: LabelCatalogue,
        failure: Option<String>,
        search_calls: AtomicUsize,
        lookup_calls: AtomicUsize,
        catalogue_calls: AtomicUsize,
        /// Every ID asked for by `label_by_id`, in order: a pin test must prove *which*
        /// label was looked up, not merely that a lookup happened.
        requested_ids: std::sync::Mutex<Vec<String>>,
    }

    impl FakeLabelProvider {
        fn new(candidates: Vec<LabelCandidate>) -> Arc<Self> {
            Arc::new(Self {
                candidates,
                catalogue: catalogue(),
                failure: None,
                search_calls: AtomicUsize::new(0),
                lookup_calls: AtomicUsize::new(0),
                catalogue_calls: AtomicUsize::new(0),
                requested_ids: std::sync::Mutex::new(Vec::new()),
            })
        }

        fn failing(reason: &str) -> Arc<Self> {
            Arc::new(Self {
                candidates: Vec::new(),
                catalogue: LabelCatalogue {
                    releases: Vec::new(),
                    incomplete: 0,
                },
                failure: Some(reason.to_string()),
                search_calls: AtomicUsize::new(0),
                lookup_calls: AtomicUsize::new(0),
                catalogue_calls: AtomicUsize::new(0),
                requested_ids: std::sync::Mutex::new(Vec::new()),
            })
        }
    }

    #[async_trait]
    impl DiscographyProvider for FakeLabelProvider {
        async fn search_artists(
            &self,
            _artist: &str,
        ) -> std::result::Result<Vec<super::super::ArtistCandidate>, DiscographyError> {
            panic!("label tests never search artists")
        }

        async fn search_artists_by_code(
            &self,
            _code: super::super::CodeIdentifier<'_>,
        ) -> std::result::Result<Vec<super::super::ArtistCandidate>, DiscographyError> {
            panic!("label tests never search artist codes")
        }

        async fn artist_by_id(
            &self,
            _artist_mbid: &str,
        ) -> std::result::Result<super::super::ArtistCandidate, DiscographyError> {
            panic!("label tests never look up artists")
        }

        async fn release_groups(
            &self,
            _artist_mbid: &str,
        ) -> std::result::Result<Vec<ReleaseGroup>, DiscographyError> {
            panic!("label tests never fetch an artist discography")
        }

        async fn search_labels(
            &self,
            _label: &str,
        ) -> std::result::Result<Vec<LabelCandidate>, DiscographyError> {
            self.search_calls.fetch_add(1, Ordering::SeqCst);
            if let Some(reason) = &self.failure {
                return Err(DiscographyError::Transport(reason.clone()));
            }
            Ok(self.candidates.clone())
        }

        async fn label_by_id(
            &self,
            label_mbid: &str,
        ) -> std::result::Result<LabelCandidate, DiscographyError> {
            self.lookup_calls.fetch_add(1, Ordering::SeqCst);
            self.requested_ids
                .lock()
                .unwrap()
                .push(label_mbid.to_string());
            if let Some(reason) = &self.failure {
                return Err(DiscographyError::Transport(reason.clone()));
            }
            self.candidates
                .first()
                .cloned()
                .ok_or_else(|| DiscographyError::LabelUnresolved("no candidate".into()))
        }

        async fn releases_by_label(
            &self,
            _label_mbid: &str,
        ) -> std::result::Result<LabelCatalogue, DiscographyError> {
            self.catalogue_calls.fetch_add(1, Ordering::SeqCst);
            if let Some(reason) = &self.failure {
                return Err(DiscographyError::Transport(reason.clone()));
            }
            Ok(self.catalogue.clone())
        }
    }

    fn hospital() -> LabelCandidate {
        LabelCandidate {
            id: LABEL_ID.to_string(),
            name: "Hospital Records".to_string(),
            score: Some(100),
            label_code: Some(12939),
            disambiguation: Some("UK drum & bass".to_string()),
        }
    }

    fn config() -> DiscographyConfig {
        DiscographyConfig::default()
    }

    const NOW: i64 = 1_800_000_000;

    #[tokio::test]
    async fn a_fresh_cache_row_is_served_without_a_request() {
        let db = Database::open_in_memory().unwrap();
        let provider = FakeLabelProvider::new(vec![hospital()]);
        let input = LabelInput::parse("Hospital Records");

        let first = discover_label_releases_at(
            &*provider,
            &db,
            &input,
            &config(),
            NOW,
            FailureCacheUse::Honour,
        )
        .await;
        let LabelDiscoveryOutcome::Catalogue {
            provenance,
            releases,
            ..
        } = first.outcome
        else {
            panic!("expected a catalogue, got {:?}", first.outcome);
        };
        assert_eq!(provenance, super::super::DiscoveryProvenance::Refreshed);
        assert_eq!(releases.len(), 1);

        let second = discover_label_releases_at(
            &*provider,
            &db,
            &input,
            &config(),
            NOW + SECONDS_PER_DAY as i64,
            FailureCacheUse::Honour,
        )
        .await;
        let LabelDiscoveryOutcome::Catalogue { provenance, .. } = second.outcome else {
            panic!("expected a catalogue, got {:?}", second.outcome);
        };
        assert_eq!(provenance, super::super::DiscoveryProvenance::FreshCache);
        assert_eq!(provider.search_calls.load(Ordering::SeqCst), 1);
        assert_eq!(provider.catalogue_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn removing_a_pin_from_an_id_input_refetches_the_requested_label() {
        // While the pin was in place the catalogue was stored under `label:id:<A>` with the
        // *pinned* label's MBID. With the pin gone, the run asks for A: serving that row
        // would stage a completely different label's albums under a "resolved" line naming
        // the wrong label.
        const ASKED: &str = "11111111-1111-1111-1111-111111111111";
        const PINNED: &str = "22222222-2222-2222-2222-222222222222";
        let db = Database::open_in_memory().unwrap();
        let provider = FakeLabelProvider::new(vec![LabelCandidate {
            id: PINNED.to_string(),
            name: "Pinned Label".to_string(),
            score: Some(100),
            label_code: None,
            disambiguation: None,
        }]);
        let input = LabelInput::parse(ASKED);
        let mut pinned = config();
        pinned
            .label_mbids
            .insert(ASKED.to_string(), PINNED.to_string());

        discover_label_releases_at(
            &*provider,
            &db,
            &input,
            &pinned,
            NOW,
            FailureCacheUse::Honour,
        )
        .await;
        assert_eq!(
            provider.requested_ids.lock().unwrap().as_slice(),
            [PINNED],
            "the pin must be the ID that is fetched"
        );

        let unpinned = config();
        let discovery = discover_label_releases_at(
            &*provider,
            &db,
            &input,
            &unpinned,
            NOW + 60,
            FailureCacheUse::Honour,
        )
        .await;

        assert!(
            matches!(
                discovery.outcome,
                LabelDiscoveryOutcome::Catalogue {
                    provenance: super::super::DiscoveryProvenance::Refreshed,
                    ..
                }
            ),
            "a row describing another label must not be served, got {:?}",
            discovery.outcome
        );
        assert_eq!(
            provider.requested_ids.lock().unwrap().as_slice(),
            [PINNED, ASKED],
            "the requested ID must be fetched once the pin is gone"
        );
    }

    #[tokio::test]
    async fn cache_days_zero_fetches_every_time_and_writes_nothing() {
        // `discography.cache_days: 0` means "never cache". The label path has its own
        // store/read branch, so it needs its own proof.
        let db = Database::open_in_memory().unwrap();
        let provider = FakeLabelProvider::new(vec![hospital()]);
        let input = LabelInput::parse("Hospital Records");
        let mut without_cache = config();
        without_cache.cache_days = 0;

        discover_label_releases_at(
            &*provider,
            &db,
            &input,
            &without_cache,
            NOW,
            FailureCacheUse::Honour,
        )
        .await;
        // An hour later: still well inside a non-zero `cache_days`, so a cache that was
        // not actually disabled would serve this second call from the row.
        discover_label_releases_at(
            &*provider,
            &db,
            &input,
            &without_cache,
            NOW + 3_600,
            FailureCacheUse::Honour,
        )
        .await;

        assert_eq!(
            provider.catalogue_calls.load(Ordering::SeqCst),
            2,
            "nothing may be reused when the cache is disabled"
        );
        assert!(
            db.get_discography_cache(&input.cache_key())
                .unwrap()
                .is_none(),
            "nothing may be written when the cache is disabled"
        );
    }

    #[tokio::test]
    async fn failure_cache_days_zero_neither_replays_nor_records() {
        // The label failure cache is a separate read and write from the artist one, so both
        // halves need their own proof: a row written while caching was on must not be
        // replayed when it is off, and nothing may be written while it is off.
        let db = Database::open_in_memory().unwrap();
        let provider = FakeLabelProvider::new(vec![LabelCandidate {
            name: "Some Other Label".to_string(),
            ..hospital()
        }]);
        let input = LabelInput::parse("Hospital Records");

        // With caching on, the failed resolution is recorded.
        discover_label_releases_at(
            &*provider,
            &db,
            &input,
            &config(),
            NOW,
            FailureCacheUse::Honour,
        )
        .await;
        assert!(
            db.get_discography_failure(&input.cache_key())
                .unwrap()
                .is_some(),
            "sanity: the failure must be recorded while the cache is enabled"
        );

        let mut without_failures = config();
        without_failures.failure_cache_days = 0;
        discover_label_releases_at(
            &*provider,
            &db,
            &input,
            &without_failures,
            NOW + 60,
            FailureCacheUse::Honour,
        )
        .await;

        assert_eq!(
            provider.search_calls.load(Ordering::SeqCst),
            2,
            "a disabled failure cache must not replay the recorded failure"
        );
    }

    #[test]
    fn failure_cache_days_zero_writes_no_row_for_a_label() {
        // The write half of the same setting: a zero expiry must suppress the row itself,
        // not merely keep it from being replayed.
        let db = Database::open_in_memory().unwrap();
        let input = LabelInput::parse("Hospital Records");
        let mut without_failures = config();
        without_failures.failure_cache_days = 0;
        let error = DiscographyError::LabelUnresolved("no such label".into());

        record_resolution_failure(
            &db,
            &input.cache_key(),
            &without_failures,
            NOW,
            &error,
            false,
        );

        assert!(
            db.get_discography_failure(&input.cache_key())
                .unwrap()
                .is_none(),
            "a disabled failure cache must not be written"
        );
    }

    #[test]
    fn an_inconsistent_search_page_leaves_no_failure_row() {
        // Only a resolution failure is worth remembering; one inconsistent response is not,
        // and remembering it would suppress a healthy MusicBrainz for the whole expiry.
        let db = Database::open_in_memory().unwrap();
        let input = LabelInput::parse("Hospital Records");
        let error = DiscographyError::LabelSearchInconsistent("short page".into());
        record_resolution_failure(&db, &input.cache_key(), &config(), NOW, &error, false);

        assert!(
            db.get_discography_failure(&input.cache_key())
                .unwrap()
                .is_none(),
            "an inconsistent page must not be recorded as a resolution failure"
        );
    }

    #[tokio::test]
    async fn a_catalogue_too_large_to_cache_is_still_returned() {
        // The cache write can fail on serialisation or the payload ceiling. The catalogue was
        // fetched successfully, so the run must use it and only warn - a cache failure is not a
        // resolution failure.
        let db = Database::open_in_memory().unwrap();
        // The *payload* is oversized, not the label name: the label must still resolve.
        let mut huge = group("group-1", "Syncopated City");
        huge.title = "x".repeat(super::super::MAX_CACHE_PAYLOAD_BYTES + 1);
        let provider = Arc::new(FakeLabelProvider {
            candidates: vec![hospital()],
            catalogue: LabelCatalogue {
                releases: vec![LabelRelease {
                    release_id: "release-1".to_string(),
                    release_group: huge,
                    artist_credit: "London Elektricity".to_string(),
                }],
                incomplete: 0,
            },
            failure: None,
            search_calls: AtomicUsize::new(0),
            lookup_calls: AtomicUsize::new(0),
            catalogue_calls: AtomicUsize::new(0),
            requested_ids: std::sync::Mutex::new(Vec::new()),
        });
        let input = LabelInput::parse("Hospital Records");

        let discovery = discover_label_releases_at(
            &*provider,
            &db,
            &input,
            &config(),
            NOW,
            FailureCacheUse::Honour,
        )
        .await;

        assert!(
            matches!(discovery.outcome, LabelDiscoveryOutcome::Catalogue { .. }),
            "an uncacheable catalogue is still a catalogue, got {:?}",
            discovery.outcome
        );
        assert!(
            db.get_discography_cache(&input.cache_key())
                .unwrap()
                .is_none(),
            "an oversized payload must not be written"
        );
    }

    #[tokio::test]
    async fn an_invalid_input_is_refused_before_any_request() {
        // The public variants can be built directly, so the guarantee has to live here
        // too: a value that is not a UUID must never reach the URL it is interpolated
        // into, and must not be searched for as a name either.
        let db = Database::open_in_memory().unwrap();
        let provider = FakeLabelProvider::new(vec![hospital()]);

        for input in [
            LabelInput::Mbid("../../evil".to_string()),
            LabelInput::Name("   ".to_string()),
        ] {
            let discovery = discover_label_releases_at(
                &*provider,
                &db,
                &input,
                &config(),
                NOW,
                FailureCacheUse::Honour,
            )
            .await;
            assert!(
                matches!(
                    discovery.outcome,
                    LabelDiscoveryOutcome::Failed {
                        kind: DiscoveryFailure::Unresolved,
                        ..
                    }
                ),
                "an invalid input must be refused, got {:?}",
                discovery.outcome
            );
        }
        assert_eq!(provider.search_calls.load(Ordering::SeqCst), 0);
        assert_eq!(provider.lookup_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_name_whose_key_keeps_an_upper_case_character_still_hits_the_cache() {
        // An ISNI-shaped name folds to a key with an upper-case check character. The row
        // is read back through a NOCASE column, so the comparison must not re-fold the
        // stored key with the artist rule: doing so lower-cases that character and the
        // catalogue is re-fetched on every run.
        const ISNI_NAME: &str = "isni:0000 0004 6659 702X";
        let db = Database::open_in_memory().unwrap();
        let provider = FakeLabelProvider::new(vec![LabelCandidate {
            name: ISNI_NAME.to_string(),
            ..hospital()
        }]);
        let input = LabelInput::parse(ISNI_NAME);

        let first = discover_label_releases_at(
            &*provider,
            &db,
            &input,
            &config(),
            NOW,
            FailureCacheUse::Honour,
        )
        .await;
        assert!(
            matches!(first.outcome, LabelDiscoveryOutcome::Catalogue { .. }),
            "the first run must resolve, got {:?}",
            first.outcome
        );

        let second = discover_label_releases_at(
            &*provider,
            &db,
            &input,
            &config(),
            NOW + SECONDS_PER_DAY as i64,
            FailureCacheUse::Honour,
        )
        .await;
        let LabelDiscoveryOutcome::Catalogue { provenance, .. } = second.outcome else {
            panic!("expected a catalogue, got {:?}", second.outcome);
        };
        assert_eq!(provenance, super::super::DiscoveryProvenance::FreshCache);
        assert_eq!(
            provider.catalogue_calls.load(Ordering::SeqCst),
            1,
            "the cached catalogue must be reused rather than fetched again"
        );
    }

    #[tokio::test]
    async fn a_stale_row_is_refreshed() {
        let db = Database::open_in_memory().unwrap();
        let provider = FakeLabelProvider::new(vec![hospital()]);
        let input = LabelInput::parse("Hospital Records");

        discover_label_releases_at(
            &*provider,
            &db,
            &input,
            &config(),
            NOW,
            FailureCacheUse::Honour,
        )
        .await;
        let refreshed = discover_label_releases_at(
            &*provider,
            &db,
            &input,
            &config(),
            NOW + 31 * SECONDS_PER_DAY as i64,
            FailureCacheUse::Honour,
        )
        .await;
        let LabelDiscoveryOutcome::Catalogue { provenance, .. } = refreshed.outcome else {
            panic!("expected a catalogue, got {:?}", refreshed.outcome);
        };
        assert_eq!(provenance, super::super::DiscoveryProvenance::Refreshed);
        assert_eq!(provider.catalogue_calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_failed_refresh_serves_the_stale_catalogue_with_its_age() {
        let db = Database::open_in_memory().unwrap();
        let provider = FakeLabelProvider::new(vec![hospital()]);
        let input = LabelInput::parse("Hospital Records");
        discover_label_releases_at(
            &*provider,
            &db,
            &input,
            &config(),
            NOW,
            FailureCacheUse::Honour,
        )
        .await;

        let failing = FakeLabelProvider::failing("MusicBrainz returned HTTP 503");
        let stale = discover_label_releases_at(
            &*failing,
            &db,
            &input,
            &config(),
            NOW + 40 * SECONDS_PER_DAY as i64,
            FailureCacheUse::Honour,
        )
        .await;
        let LabelDiscoveryOutcome::Catalogue {
            provenance,
            releases,
            ..
        } = stale.outcome
        else {
            panic!("expected the stale catalogue, got {:?}", stale.outcome);
        };
        assert_eq!(releases.len(), 1, "the stale catalogue is still usable");
        let super::super::DiscoveryProvenance::StaleCache { age_days, kind, .. } = provenance
        else {
            panic!("expected StaleCache, got {provenance:?}");
        };
        assert_eq!(age_days, 40);
        assert_eq!(kind, super::super::DiscoveryFailure::Provider);
    }

    #[tokio::test]
    async fn an_unresolvable_name_is_recorded_and_replayed() {
        let db = Database::open_in_memory().unwrap();
        let provider = Arc::new(FakeLabelProvider {
            candidates: vec![LabelCandidate {
                id: LABEL_ID.to_string(),
                name: "Hospital Records Limited".to_string(),
                score: Some(80),
                label_code: None,
                disambiguation: None,
            }],
            catalogue: catalogue(),
            failure: None,
            search_calls: AtomicUsize::new(0),
            lookup_calls: AtomicUsize::new(0),
            catalogue_calls: AtomicUsize::new(0),
            requested_ids: std::sync::Mutex::new(Vec::new()),
        });
        let input = LabelInput::parse("Hospital Records");

        let first = discover_label_releases_at(
            &*provider,
            &db,
            &input,
            &config(),
            NOW,
            FailureCacheUse::Honour,
        )
        .await;
        assert!(
            matches!(
                first.outcome,
                LabelDiscoveryOutcome::Failed {
                    kind: super::super::DiscoveryFailure::Unresolved,
                    from_cache: false,
                    ..
                }
            ),
            "got {:?}",
            first.outcome
        );

        let second = discover_label_releases_at(
            &*provider,
            &db,
            &input,
            &config(),
            NOW + 60,
            FailureCacheUse::Honour,
        )
        .await;
        assert!(
            matches!(
                second.outcome,
                LabelDiscoveryOutcome::Failed {
                    kind: super::super::DiscoveryFailure::Unresolved,
                    from_cache: true,
                    ..
                }
            ),
            "the recorded failure is replayed, got {:?}",
            second.outcome
        );
        assert_eq!(provider.search_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_pinned_mbid_looks_the_label_up_and_clears_a_recorded_failure() {
        let db = Database::open_in_memory().unwrap();
        let provider = FakeLabelProvider::new(vec![hospital()]);
        let input = LabelInput::parse("Hospital Records");
        let mut pinned = config();
        pinned
            .label_mbids
            .insert("Hospital Records".into(), LABEL_ID.into());
        db.upsert_discography_failure(&DiscographyFailureEntry {
            artist_key: input.cache_key(),
            failure_kind: "unresolved".to_string(),
            reason: "no candidate matches".to_string(),
            recorded_at: NOW,
        })
        .unwrap();

        let discovery = discover_label_releases_at(
            &*provider,
            &db,
            &input,
            &pinned,
            NOW,
            FailureCacheUse::Honour,
        )
        .await;
        assert!(matches!(
            discovery.outcome,
            LabelDiscoveryOutcome::Catalogue { .. }
        ));
        assert_eq!(provider.lookup_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            provider.search_calls.load(Ordering::SeqCst),
            0,
            "a pin never searches"
        );
        assert!(
            db.get_discography_failure(&input.cache_key())
                .unwrap()
                .is_none(),
            "a pin makes the recorded failure obsolete, so it is cleared"
        );
    }

    #[tokio::test]
    async fn a_malformed_input_is_refused_without_asking_musicbrainz() {
        let db = Database::open_in_memory().unwrap();
        let provider = FakeLabelProvider::new(vec![hospital()]);
        // UUID-shaped but not a UUID: the CLI validates, and the public entry point
        // must not spend a request searching for it as if it were a name.
        let input = LabelInput::parse("8562598d-60d1-4bd6-8b85-4d709e95177");

        let discovery =
            discover_label_releases(&*provider, &db, &input, &config(), FailureCacheUse::Honour)
                .await;

        let LabelDiscoveryOutcome::Failed {
            reason,
            from_cache,
            kind,
        } = discovery.outcome
        else {
            panic!("expected a refusal, got {:?}", discovery.outcome);
        };
        assert_eq!(kind, super::super::DiscoveryFailure::Unresolved);
        assert!(!from_cache);
        assert!(
            reason.contains("hyphenated like a MusicBrainz ID"),
            "got {reason}"
        );
        assert_eq!(
            provider.search_calls.load(Ordering::SeqCst),
            0,
            "a malformed value must not be searched for"
        );
        assert!(
            db.get_discography_failure(&input.cache_key())
                .unwrap()
                .is_none(),
            "a refusal that never reached MusicBrainz must not be recorded as one"
        );

        // The property that matters: a corrected value still resolves against the
        // same database, so the refusal cannot suppress a later run.
        let corrected = LabelInput::parse(LABEL_ID);
        let later = discover_label_releases(
            &*provider,
            &db,
            &corrected,
            &config(),
            FailureCacheUse::Honour,
        )
        .await;
        assert!(
            matches!(later.outcome, LabelDiscoveryOutcome::Catalogue { .. }),
            "a corrected value must still resolve, got {:?}",
            later.outcome
        );
    }

    #[tokio::test]
    async fn an_mbid_looks_the_label_up_without_a_pin() {
        let db = Database::open_in_memory().unwrap();
        let provider = FakeLabelProvider::new(vec![hospital()]);
        let input = LabelInput::parse(LABEL_ID);

        let discovery = discover_label_releases_at(
            &*provider,
            &db,
            &input,
            &config(),
            NOW,
            FailureCacheUse::Honour,
        )
        .await;

        assert!(matches!(
            discovery.outcome,
            LabelDiscoveryOutcome::Catalogue { .. }
        ));
        assert_eq!(provider.lookup_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            provider.search_calls.load(Ordering::SeqCst),
            0,
            "an exact ID needs no search"
        );
    }

    #[tokio::test]
    async fn an_id_keyed_catalogue_is_served_from_the_cache_on_the_second_run() {
        // The `label:id:<uuid>` key space is read back through the same fold-compare
        // as a name key; this pins that round trip so a change to `pin_key` or
        // `normalize_catalog_key` cannot silently make every ID-keyed row a miss.
        let db = Database::open_in_memory().unwrap();
        let provider = FakeLabelProvider::new(vec![hospital()]);
        let input = LabelInput::parse(LABEL_ID);

        let first = discover_label_releases_at(
            &*provider,
            &db,
            &input,
            &config(),
            NOW,
            FailureCacheUse::Honour,
        )
        .await;
        let LabelDiscoveryOutcome::Catalogue { provenance, .. } = first.outcome else {
            panic!("expected a catalogue, got {:?}", first.outcome);
        };
        assert_eq!(provenance, super::super::DiscoveryProvenance::Refreshed);

        let second = discover_label_releases_at(
            &*provider,
            &db,
            &input,
            &config(),
            NOW + 60,
            FailureCacheUse::Honour,
        )
        .await;
        let LabelDiscoveryOutcome::Catalogue { provenance, .. } = second.outcome else {
            panic!("expected a cached catalogue, got {:?}", second.outcome);
        };
        assert_eq!(provenance, super::super::DiscoveryProvenance::FreshCache);
        assert_eq!(
            provider.lookup_calls.load(Ordering::SeqCst),
            1,
            "the second run must be served from the row written by the first"
        );
    }

    #[tokio::test]
    async fn a_wrong_version_cache_row_is_refetched_and_replaced() {
        let db = Database::open_in_memory().unwrap();
        let provider = FakeLabelProvider::new(vec![hospital()]);
        let input = LabelInput::parse("Hospital Records");
        let future = LabelCacheEntry {
            version: LABEL_CACHE_VERSION + 1,
            releases: catalogue().releases,
            incomplete: 0,
        };
        db.upsert_discography_cache(&DiscographyCacheEntry {
            artist_key: input.cache_key(),
            artist_mbid: LABEL_ID.to_string(),
            canonical_artist: "Hospital Records".to_string(),
            fetched_at: NOW,
            release_groups_json: serde_json::to_string(&future).unwrap(),
        })
        .unwrap();

        let discovery = discover_label_releases_at(
            &*provider,
            &db,
            &input,
            &config(),
            NOW,
            FailureCacheUse::Honour,
        )
        .await;
        let LabelDiscoveryOutcome::Catalogue { provenance, .. } = discovery.outcome else {
            panic!("expected a catalogue, got {:?}", discovery.outcome);
        };
        assert_eq!(provenance, super::super::DiscoveryProvenance::Refreshed);
        assert_eq!(provider.catalogue_calls.load(Ordering::SeqCst), 1);

        let row = db
            .get_discography_cache(&input.cache_key())
            .unwrap()
            .expect("the refreshed catalogue is written back");
        let parsed: LabelCacheEntry = serde_json::from_str(&row.release_groups_json).unwrap();
        assert_eq!(
            parsed.version, LABEL_CACHE_VERSION,
            "a row from another version is replaced, not read"
        );
    }

    #[tokio::test]
    async fn a_corrupt_cache_row_is_refetched_and_replaced() {
        let db = Database::open_in_memory().unwrap();
        let provider = FakeLabelProvider::new(vec![hospital()]);
        let input = LabelInput::parse("Hospital Records");
        db.upsert_discography_cache(&DiscographyCacheEntry {
            artist_key: input.cache_key(),
            artist_mbid: LABEL_ID.to_string(),
            canonical_artist: "Hospital Records".to_string(),
            fetched_at: NOW,
            release_groups_json: "{ this is not a cache payload".to_string(),
        })
        .unwrap();

        let discovery = discover_label_releases_at(
            &*provider,
            &db,
            &input,
            &config(),
            NOW,
            FailureCacheUse::Honour,
        )
        .await;
        let LabelDiscoveryOutcome::Catalogue { provenance, .. } = discovery.outcome else {
            panic!("expected a catalogue, got {:?}", discovery.outcome);
        };
        assert_eq!(provenance, super::super::DiscoveryProvenance::Refreshed);
        assert_eq!(provider.catalogue_calls.load(Ordering::SeqCst), 1);

        let row = db
            .get_discography_cache(&input.cache_key())
            .unwrap()
            .expect("the refreshed catalogue is written back");
        let parsed: LabelCacheEntry = serde_json::from_str(&row.release_groups_json)
            .expect("a corrupt row is replaced by a well-formed one");
        assert_eq!(parsed.releases.len(), 1);
    }

    #[tokio::test]
    async fn a_replayed_failure_without_a_reason_uses_the_default_text() {
        let db = Database::open_in_memory().unwrap();
        let provider = FakeLabelProvider::new(vec![hospital()]);
        let input = LabelInput::parse("Hospital Records");
        db.upsert_discography_failure(&DiscographyFailureEntry {
            artist_key: input.cache_key(),
            failure_kind: "unresolved".to_string(),
            reason: "   ".to_string(),
            recorded_at: NOW,
        })
        .unwrap();

        let discovery = discover_label_releases_at(
            &*provider,
            &db,
            &input,
            &config(),
            NOW,
            FailureCacheUse::Honour,
        )
        .await;
        let LabelDiscoveryOutcome::Failed {
            reason,
            from_cache,
            kind,
        } = discovery.outcome
        else {
            panic!("expected a replayed failure, got {:?}", discovery.outcome);
        };
        assert!(from_cache, "the row is replayed, not re-asked");
        assert_eq!(kind, super::super::DiscoveryFailure::Unresolved);
        assert_eq!(reason, "label could not be resolved safely");
        assert_eq!(
            provider.search_calls.load(Ordering::SeqCst),
            0,
            "a replayed failure asks MusicBrainz nothing"
        );
    }

    #[tokio::test]
    async fn a_changed_pin_refetches_an_incompatible_row() {
        let db = Database::open_in_memory().unwrap();
        let provider = FakeLabelProvider::new(vec![hospital()]);
        let input = LabelInput::parse("Hospital Records");

        discover_label_releases_at(
            &*provider,
            &db,
            &input,
            &config(),
            NOW,
            FailureCacheUse::Honour,
        )
        .await;

        let mut pinned = config();
        pinned.label_mbids.insert(
            "Hospital Records".into(),
            "c8471d13-3574-4040-be20-229656068504".into(),
        );
        let discovery = discover_label_releases_at(
            &*provider,
            &db,
            &input,
            &pinned,
            NOW + 60,
            FailureCacheUse::Honour,
        )
        .await;
        let LabelDiscoveryOutcome::Catalogue { provenance, .. } = discovery.outcome else {
            panic!("expected a catalogue, got {:?}", discovery.outcome);
        };
        assert_eq!(provenance, super::super::DiscoveryProvenance::Refreshed);
        assert_eq!(provider.lookup_calls.load(Ordering::SeqCst), 1);
    }
}
