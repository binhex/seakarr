use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use unicode_normalization::UnicodeNormalization;

use crate::config::DiscographyConfig;
use crate::config::DiscographyReleaseType;
use crate::db::{Database, DiscographyCacheEntry, DiscographyFailureEntry};
use crate::search::album_identity_key;

mod bundle;
pub mod identifier;
pub mod label;
mod musicbrainz;
pub use identifier::{resolve_code, ArtistInput, CodeIdentifier};
pub use musicbrainz::MusicBrainzProvider;

/// Apostrophe characters that spell the same mark across keyboards, tags and
/// cover art: the ASCII apostrophe, the typographic pair, the single high
/// quotation mark, the modifier letter apostrophe, the grave accent and the
/// acute accent.
///
/// They are removed before folding so `Guns N’ Roses` (how MusicBrainz spells
/// it), `Guns 'n' Roses` (how one library spells it) and `Guns N Roses` share a
/// single key. Leaving them in made one album look like several: a discover run
/// reported both MusicBrainz Sgt. Pepper release groups, the library's
/// apostrophe-less tag did not match either, and the album was downloaded twice
/// into two new folders beside the copy already on disk.
const APOSTROPHE_LIKE_CHARACTERS: &[char] = &[
    '\'',       // ASCII apostrophe
    '\u{2018}', // left single quotation mark
    '\u{2019}', // right single quotation mark (MusicBrainz's usual spelling)
    '\u{201A}', // single low-9 quotation mark
    '\u{201B}', // single high-reversed-9 quotation mark
    '\u{02BB}', // modifier letter turned comma (okina)
    '\u{02BC}', // modifier letter apostrophe
    '\u{02BE}', // modifier letter right half ring
    '\u{02BF}', // modifier letter left half ring
    '\u{02B9}', // modifier letter prime
    '\u{055A}', // Armenian apostrophe
    '\u{1FBD}', // Greek koronis (spacing form)
    '\u{0313}', // combining comma above (the decomposition of U+1FBD)
    '\u{0314}', // combining reversed comma above (of U+201B)
    '\u{05F3}', // Hebrew geresh
    '\u{2032}', // prime
    '\u{FF07}', // fullwidth apostrophe (NFKC folds it to U+0027; listed anyway)
    '`',        // grave accent
    '\u{00B4}', // acute accent
];

pub(crate) fn normalize_catalog_key(value: &str) -> String {
    // The filter runs on BOTH sides of the compatibility fold.
    //
    // Before it, because a listed mark can decompose into several characters
    // (U+00B4 becomes a space plus a combining acute) and removing it while it is
    // still one character is the only way to remove all of it.
    //
    // After it, as a safety net for a compatibility variant that is NOT itself
    // listed but folds INTO a listed mark - the shape U+FF07 (fullwidth
    // apostrophe) has. U+FF07 is also listed explicitly, so today the first pass
    // already removes it and this pass is redundant for every input; it exists so
    // the "both sides of the fold" invariant stays true if a future entry is
    // added without its compatibility form.
    value
        .chars()
        .filter(|character| !APOSTROPHE_LIKE_CHARACTERS.contains(character))
        .collect::<String>()
        .nfkc()
        .filter(|character| !APOSTROPHE_LIKE_CHARACTERS.contains(character))
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
    /// MusicBrainz artist-search relevance score in the documented 0-100
    /// range. `None` when the response omitted a score, which is normal for
    /// lookups by MBID and for a single unambiguous search result.
    pub score: Option<u8>,
    /// IPI codes the artist's own MusicBrainz record claims.
    pub ipis: Vec<String>,
    /// ISNI codes the artist's own MusicBrainz record claims.
    pub isnis: Vec<String>,
    /// MusicBrainz disambiguation comment, when present.
    pub disambiguation: Option<String>,
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
    /// A 5xx from the provider, carrying what the response said about it.
    ///
    /// MusicBrainz answers 503 for a global overload, a per-IP rate limit and a
    /// per-application throttle alike; `X-RateLimit-Zone` and the body are the
    /// only things that tell them apart, and the operator cannot act on an
    /// outage without them. `detail` is empty only when the response carried no
    /// zone, no `Retry-After` field and no body text that survives sanitising;
    /// the message is then
    /// exactly `MusicBrainz returned HTTP 503`. An unusable `Retry-After` field
    /// is quoted (up to eight fields) instead of being dropped, and a body that
    /// could not be read is reported as `<body omitted>`.
    #[error("MusicBrainz returned HTTP {status}{detail}")]
    ProviderError { status: u16, detail: String },
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
    /// The label could not be resolved to exactly one MusicBrainz candidate.
    /// Recorded in the failure cache under `label:name:<key>`, so a name that will
    /// not resolve is not asked about again until the expiry elapses.
    #[error("label could not be resolved safely: {0}")]
    LabelUnresolved(String),
    /// A label search answered with a page whose reported total disagrees with
    /// the candidates it carried. Classified as a resolution problem for this
    /// run, like its artist equivalent, but never cached: it describes one
    /// inconsistent response rather than a property of the name.
    #[error("label search was inconsistent: {0}")]
    LabelSearchInconsistent(String),
    /// The provider answered, but its candidate data was unusable: a search
    /// score outside the documented range, or a missing score where one is
    /// required. This is a provider defect, not an artist that genuinely cannot
    /// be resolved, so it must not be reported as an unresolved artist where it
    /// would hide a MusicBrainz outage from the circuit breaker. The message names no
    /// entity kind: the same defect is reported for an artist and for a label.
    #[error("MusicBrainz returned unusable candidate data: {0}")]
    InvalidCandidateData(String),
}

#[async_trait]
pub trait DiscographyProvider: Send + Sync {
    async fn search_artists(
        &self,
        artist: &str,
    ) -> std::result::Result<Vec<ArtistCandidate>, DiscographyError>;

    /// Search the MusicBrainz artist index for the artist claiming a code.
    ///
    /// Required rather than defaulted: a provider that quietly returned nothing
    /// would make a missing implementation look like a code no artist carries.
    async fn search_artists_by_code(
        &self,
        code: CodeIdentifier<'_>,
    ) -> std::result::Result<Vec<ArtistCandidate>, DiscographyError>;

    async fn artist_by_id(
        &self,
        artist_mbid: &str,
    ) -> std::result::Result<ArtistCandidate, DiscographyError>;

    /// Search the MusicBrainz label index for a label by name.
    async fn search_labels(
        &self,
        label: &str,
    ) -> std::result::Result<Vec<label::LabelCandidate>, DiscographyError>;

    /// Look up one label by MusicBrainz ID. Exact: no ranking, no score.
    async fn label_by_id(
        &self,
        label_mbid: &str,
    ) -> std::result::Result<label::LabelCandidate, DiscographyError>;

    /// Every official release on a label that the payload can describe, with its
    /// release group inline. A release carrying no release group or no artist
    /// credit cannot be represented and is counted in
    /// [`label::LabelCatalogue::incomplete`] rather than guessed.
    ///
    /// Release groups cannot be browsed by label, so the catalogue is built from
    /// releases and collapsed to release groups by the selector.
    async fn releases_by_label(
        &self,
        label_mbid: &str,
    ) -> std::result::Result<label::LabelCatalogue, DiscographyError>;

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
    /// Exactly one candidate carried the requested IPI or ISNI.
    Identity,
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

/// Resolve the artist that claims an IPI or an ISNI.
///
/// Exactly one carrier resolves. None and several are both
/// [`DiscographyError::ArtistUnresolved`], because the caller cannot act on
/// either: MusicBrainz scores identifier hits at or near 100 whatever artist they
/// belong to, so the name path's dominance rule cannot choose between them.
pub fn resolve_artist_by_code(
    code: CodeIdentifier<'_>,
    candidates: &[ArtistCandidate],
) -> std::result::Result<ResolvedArtist, DiscographyError> {
    let carriers: Vec<&ArtistCandidate> = candidates
        .iter()
        .filter(|candidate| code.matches(candidate))
        .collect();
    match carriers.as_slice() {
        [] => Err(DiscographyError::ArtistUnresolved(format!(
            "no MusicBrainz artist carries {} {}",
            code.kind(),
            code.value()
        ))),
        [only] => Ok(ResolvedArtist {
            candidate: (*only).clone(),
            resolution: ArtistResolution::Identity,
        }),
        many => Err(DiscographyError::ArtistUnresolved(format!(
            "{} artists carry {} {} ({}); pin one with discography.artist_mbids key \"{}:{}\", \
             or name the artist by its MusicBrainz ID instead",
            many.len(),
            code.kind(),
            code.value(),
            many.iter()
                .map(|candidate| format!("{} ({})", candidate.name, candidate.id))
                .collect::<Vec<_>>()
                .join(", "),
            // Lower-cased, this is exactly the key `ArtistInput::cache_key` produces.
            code.kind().to_ascii_lowercase(),
            code.value()
        ))),
    }
}

/// Which entity kind a resolution failure is about, so the shared dominance
/// helper reports the variant its caller can act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResolveKind {
    Artist,
    Label,
}

impl ResolveKind {
    fn unresolved(&self, reason: String) -> DiscographyError {
        match self {
            Self::Artist => DiscographyError::ArtistUnresolved(reason),
            Self::Label => DiscographyError::LabelUnresolved(reason),
        }
    }
}

/// The winner of the dominance rules, with the evidence that selected it.
pub(crate) struct Dominance {
    /// Index into the slice the caller passed.
    pub index: usize,
    pub evidence: DominanceEvidence,
}

/// Apply the dominance rules to two or more scored candidates.
///
/// The two-candidate minimum is a precondition, not a runtime check: a caller with one
/// match has already picked it and must not pay for the comparison. `debug_assert`
/// keeps a future caller honest in tests without charging production for the test.
pub(crate) fn dominant_candidate_index<T>(
    kind: ResolveKind,
    key: &str,
    matches: &[&T],
    score_of: impl Fn(&T) -> Option<u8>,
    id_of: impl Fn(&T) -> &str,
) -> std::result::Result<Dominance, DiscographyError> {
    debug_assert!(
        matches.len() >= 2,
        "the dominance rules compare two or more candidates"
    );
    let exact_matches = matches.len();
    // Reject invalid provider data before absent data, and scan the whole set
    // rather than returning on the first offending candidate: the reported
    // reason, including the quoted invalid value, must not depend on the order
    // MusicBrainz returned candidates in.
    if let Some(invalid) = matches
        .iter()
        .filter_map(|candidate| score_of(candidate))
        .filter(|score| *score > REQUIRED_TOP_SCORE)
        .max()
    {
        return Err(DiscographyError::InvalidCandidateData(format!(
            "{exact_matches} candidates match {key:?} and one reports the invalid score {invalid}"
        )));
    }
    if matches
        .iter()
        .any(|candidate| score_of(candidate).is_none())
    {
        return Err(DiscographyError::InvalidCandidateData(format!(
            "{exact_matches} candidates match {key:?} and at least one has no search score"
        )));
    }
    // Both defect classes were rejected above, so every score is present and in
    // range here.
    let mut scored: Vec<(u8, usize, &str)> = matches
        .iter()
        .enumerate()
        .filter_map(|(index, candidate)| {
            score_of(candidate).map(|score| (score, index, id_of(candidate)))
        })
        .collect();
    debug_assert_eq!(scored.len(), exact_matches);
    // Sort by score, then MBID, so the comparison never depends on the order
    // MusicBrainz returned the candidates in.
    scored.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.2.cmp(right.2)));
    let (top_score, top_index, _) = scored[0];
    let leaders = scored
        .iter()
        .filter(|(score, _, _)| *score == top_score)
        .count();
    if leaders != 1 {
        return Err(kind.unresolved(format!(
            "{exact_matches} candidates match {key:?} with a tied top score of {top_score}"
        )));
    }
    if top_score < REQUIRED_TOP_SCORE {
        return Err(kind.unresolved(format!(
            "{exact_matches} candidates match {key:?} and the highest score {top_score} is below {REQUIRED_TOP_SCORE}"
        )));
    }
    let runner_up_score = scored[1].0;
    let margin = top_score - runner_up_score;
    if margin < REQUIRED_SCORE_MARGIN {
        return Err(kind.unresolved(format!(
            "{exact_matches} candidates match {key:?} and the leading margin {margin} is below {REQUIRED_SCORE_MARGIN}"
        )));
    }
    Ok(Dominance {
        index: top_index,
        evidence: DominanceEvidence {
            exact_matches,
            top_score,
            runner_up_score,
            margin,
        },
    })
}

/// Apply the dominance rules to two or more canonical exact-name matches.
fn resolve_dominant_artist(
    artist_key: &str,
    matches: &[&ArtistCandidate],
) -> std::result::Result<ResolvedArtist, DiscographyError> {
    let dominance = dominant_candidate_index(
        ResolveKind::Artist,
        artist_key,
        matches,
        |candidate| candidate.score,
        |candidate| candidate.id.as_str(),
    )?;
    Ok(ResolvedArtist {
        candidate: (*matches[dominance.index]).clone(),
        resolution: ArtistResolution::ScoreDominance(dominance.evidence),
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
/// A `YYYY[-MM[-DD]]` date, missing components treated as zero.
///
/// Public only because it appears in public signatures (`ExecutionPlan::Discover::since`
/// and `label_targets`). It is not constructible outside the crate: the fields are
/// private and `parse_partial_date` is crate-private, so an external caller can pass
/// `None` or a value it received from this crate, and nothing else. Bounds reach the
/// library through `--since` and `discography`, never through the API.
pub struct PartialDate {
    year: i32,
    month: u32,
    day: u32,
}

/// Parse `YYYY`, `YYYY-MM`, or `YYYY-MM-DD`. Year must be 1..=9999, month
/// 1..=12 when present, and complete dates must be real calendar dates.
/// Missing month/day sort as zero.
pub(crate) fn parse_partial_date(value: &str) -> Option<PartialDate> {
    let mut parts = value.trim().split('-');
    let year: i32 = parts.next()?.parse().ok()?;
    if !(1..=9999).contains(&year) {
        return None;
    }
    let month_part = parts.next();
    let day_part = parts.next();
    if parts.next().is_some() {
        return None;
    }
    let Some(month_part) = month_part else {
        // A bare year: no month and no day.
        return Some(PartialDate {
            year,
            month: 0,
            day: 0,
        });
    };
    let month: u32 = month_part.parse().ok()?;
    if !(1..=12).contains(&month) {
        return None;
    }
    let Some(day_part) = day_part else {
        // Year and month only.
        return Some(PartialDate {
            year,
            month,
            day: 0,
        });
    };
    // A day is present, so it must be a real one. `2025-01-00` is not "all of January"
    // spelled differently: silently widening it to the month would break the documented
    // rule that a date which does not exist is a configuration error.
    let day: u32 = day_part.parse().ok()?;
    chrono::NaiveDate::from_ymd_opt(year, month, day)?;
    Some(PartialDate { year, month, day })
}

/// Filter allowed release groups with non-empty titles, drop any whose title
/// bundles other release groups of the same artist, deduplicate by album
/// identity (prefer dated over undated, then earlier date, then
/// lexicographically smaller MBID), and order dated targets by partial date
/// before undated targets.
///
/// Identity, not the raw title, is what collapses MusicBrainz's several release
/// groups for one album into a single target — see `album_identity_key`.
/// `artist` is what the run is working from - the library's artist name in a
/// sweep, the operator's value for a name, and the resolved canonical name for an
/// ISNI, an IPI or an MBID request, which is what lets the identity strip an
/// `<artist> - ` prefix from a release group title.
pub fn select_albums(
    groups: &[ReleaseGroup],
    allowed: &[DiscographyReleaseType],
    artist: &str,
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
        let key = album_identity_key(title, artist);
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
                } else {
                    // The identity key collapsed two release groups into one
                    // target. Only one can be searched for, so the loser is
                    // dropped; say which one, because otherwise it vanishes from
                    // the run output with nothing to explain why.
                    tracing::debug!(
                        release_group_id = %candidate.target.release_group_id,
                        title = %candidate.target.title,
                        kept_release_group_id = %existing.target.release_group_id,
                        "excluding MusicBrainz release group: shares its album identity with another"
                    );
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
    /// The artist or label could not be resolved to exactly one MusicBrainz
    /// candidate.
    Unresolved,
    /// MusicBrainz could not be reached, or returned unusable data.
    Provider,
}

impl DiscographyError {
    /// True when this failure means the entity could not be matched or
    /// disambiguated - by canonical name, by the IPI or ISNI the caller named, or,
    /// for a label, by name - as opposed to a transport, HTTP, decode, pagination or
    /// unusable-data failure, and is therefore worth remembering across runs.
    ///
    /// Only [`DiscographyError::ArtistUnresolved`] and
    /// [`DiscographyError::LabelUnresolved`] qualify. Their siblings
    /// [`DiscographyError::ArtistSearchInconsistent`] and
    /// [`DiscographyError::LabelSearchInconsistent`] are deliberately classified as
    /// resolution problems for the circuit breaker and the legacy fallback, but a
    /// page whose reported total disagrees with its payload describes one
    /// inconsistent response, so caching it would suppress a healthy entity until
    /// the expiry elapsed.
    ///
    /// The match is not entirely response-independent: three of the
    /// `ArtistUnresolved` producers decide from MusicBrainz's per-response search
    /// scores — a tied top score, a top score below the required threshold, and a
    /// leading margin below the required margin — so a later re-ranking could make
    /// a cached name resolvable while the row still suppresses the lookup until it
    /// expires. The predicate is shared with the name path, but only a name failure
    /// is recorded: the two producers an identifier request adds - no carrier and
    /// several carriers - are facts about the code itself, and an explicit code
    /// always asks again, so remembering one would only accumulate dead rows.
    pub(crate) fn is_stable_resolution_failure(&self) -> bool {
        matches!(self, Self::ArtistUnresolved(_) | Self::LabelUnresolved(_))
    }
}

impl DiscoveryFailure {
    /// A resolution failure - for an artist or a label - is an entity problem;
    /// every other provider error (transport, decode, pagination, a rate-limited
    /// 5xx, or unusable candidate data) is an outage problem.
    ///
    /// An inconsistent search page counts as a resolution problem even though it
    /// is not cacheable, because falling back for this run is the pre-existing
    /// behaviour and changing it here would turn a response defect into a run
    /// abort.
    fn from_error(error: &DiscographyError) -> Self {
        match error {
            DiscographyError::ArtistUnresolved(_)
            | DiscographyError::ArtistSearchInconsistent(_)
            | DiscographyError::LabelUnresolved(_)
            | DiscographyError::LabelSearchInconsistent(_) => Self::Unresolved,
            _ => Self::Provider,
        }
    }
}

/// Whether a caller will accept a recorded resolution failure in place of a
/// provider call. Named rather than a bare `bool` so the two call sites read as
/// a decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureCacheUse {
    /// Replay a fresh recorded failure for this artist.
    Honour,
    /// Ignore any recorded failure: the user named this artist explicitly - a
    /// name, an ISNI or an IPI code, or an MBID - so the lookup runs.
    Bypass,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscoveryOutcome {
    Authoritative {
        albums: Vec<AlbumTarget>,
        provenance: DiscoveryProvenance,
        /// The canonical MusicBrainz spelling of the resolved artist, carried so the
        /// variant is self-describing. Callers read `Discovery::canonical_name`.
        canonical_name: Option<String>,
    },
    AuthoritativeEmpty {
        provenance: DiscoveryProvenance,
        canonical_name: Option<String>,
    },
    LegacyFallback {
        reason: String,
        kind: DiscoveryFailure,
        /// True when this failure was replayed from the failure cache rather
        /// than observed live, so the caller can report the two separately.
        from_cache: bool,
    },
}

/// The outcome of resolving one artist, with the identity it resolved to.
///
/// The canonical name is repeated at the top level so a caller can read it
/// without matching the variant; both authoritative variants carry it as well,
/// and a legacy fallback resolved nothing so it has none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discovery {
    pub outcome: DiscoveryOutcome,
    pub canonical_name: Option<String>,
    /// The resolved MusicBrainz artist ID, when one was resolved. An explicit code
    /// or ID run reports it in the run summary, so the operator can see which
    /// artist was chosen.
    pub mbid: Option<String>,
    /// MusicBrainz's disambiguation comment for the resolved artist, when the live
    /// response carried one. Absent for a cache-served answer, which stores the
    /// canonical name and the ID only.
    pub disambiguation: Option<String>,
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
    input: &ArtistInput,
    config: &DiscographyConfig,
    failure_cache: FailureCacheUse,
) -> Discovery {
    discover_artist_albums_at(
        provider,
        db,
        input,
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
    input: &ArtistInput,
    config: &DiscographyConfig,
    now: i64,
    failure_cache: FailureCacheUse,
) -> Discovery {
    let artist = input.display();
    let artist_key = input.cache_key();
    let configured_mbid = config
        .artist_mbids
        .iter()
        .find(|(name, _)| identifier::pin_key(name) == artist_key)
        .map(|(_, mbid)| mbid.as_str());

    let cached = load_compatible_cache(db, &artist_key, configured_mbid);
    if let Some(cached) = &cached {
        if cache_is_fresh(cached.entry.fetched_at, config.cache_days, now) {
            let canonical_name = cached.entry.canonical_artist.clone();
            return Discovery {
                outcome: select_outcome(
                    &cached.groups,
                    &config.allowed_types,
                    strip_artist(input, artist, &canonical_name),
                    DiscoveryProvenance::FreshCache,
                    Some(&canonical_name),
                ),
                canonical_name: Some(canonical_name),
                mbid: Some(cached.entry.artist_mbid.clone()),
                disambiguation: None,
            };
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
        let resolved = match (input.mbid(), input.code(), configured_mbid) {
            // A pinned UUID wins over every shape, exactly as it does for a name.
            (_, _, Some(mbid)) => provider.artist_by_id(mbid).await?,
            (Some(mbid), _, None) => provider.artist_by_id(mbid).await?,
            (None, Some(code), None) => {
                let candidates = provider.search_artists_by_code(code).await?;
                resolve_artist_by_code(code, &candidates)?.candidate
            }
            (None, None, None) => {
                let name = match input {
                    ArtistInput::Name(name) => name.as_str(),
                    // Defensive: every caller validates first, so this is unreachable
                    // from the binary. A public entry point must still not panic, and
                    // a malformed value could never resolve.
                    ArtistInput::Malformed { reason, .. } => {
                        return Err(DiscographyError::ArtistUnresolved(reason.clone()));
                    }
                    _ => unreachable!("every other non-name input is an MBID or a code"),
                };
                let candidates = provider.search_artists(name).await?;
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
        if let Some(code) = input.code() {
            tracing::info!(
                "{} {} resolved to {} ({}){}",
                code.kind(),
                code.value(),
                resolved.name,
                resolved.id,
                resolved
                    .disambiguation
                    .as_deref()
                    .map(|comment| format!(", {comment:?}"))
                    .unwrap_or_default()
            );
        }
        if let Some(mbid) = input.mbid() {
            tracing::info!(
                "MusicBrainz ID {mbid} resolved to {} ({})",
                resolved.name,
                resolved.id
            );
        }
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
                let canonical_name = resolved.name;
                Discovery {
                    outcome: select_outcome(
                        &groups,
                        &config.allowed_types,
                        strip_artist(input, artist, &canonical_name),
                        DiscoveryProvenance::Refreshed,
                        Some(&canonical_name),
                    ),
                    canonical_name: Some(canonical_name),
                    mbid: Some(resolved.id),
                    disambiguation: resolved.disambiguation,
                }
            }
            // An oversized (or unserializable) payload is a refresh error:
            // fall back to the stale cache or legacy discovery.
            Err(error) => stale_or_legacy(cached, &config.allowed_types, input, artist, now, error),
        },
        Err(error) => {
            let has_success_row = cached.is_some();
            // Only a name is worth remembering: an explicit code or ID always
            // bypasses the recorded failure, so a row for one could never be
            // replayed and would only accumulate.
            if matches!(input, ArtistInput::Name(_)) {
                record_resolution_failure(db, &artist_key, config, now, &error, has_success_row);
            }
            stale_or_legacy(cached, &config.allowed_types, input, artist, now, error)
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
    // `pin_key` folds a stored key exactly as the run built it, so an ISNI whose
    // check character is `X` still matches; `normalize_catalog_key` would lower-case
    // that `X` and never match.
    if identifier::pin_key(&entry.artist_key) != artist_key {
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
pub(crate) fn cache_is_fresh(fetched_at: i64, cache_days: u64, now: i64) -> bool {
    if cache_days == 0 {
        return false;
    }
    let age_seconds = (now as i128) - (fetched_at as i128);
    age_seconds >= 0 && age_seconds < (cache_days as i128) * (SECONDS_PER_DAY as i128)
}

/// Non-negative saturating age in complete days, for stale-cache warnings.
pub(crate) fn stale_age_days(now: i64, fetched_at: i64) -> u64 {
    let age_seconds = (now as i128) - (fetched_at as i128);
    if age_seconds <= 0 {
        0
    } else {
        (age_seconds / SECONDS_PER_DAY as i128) as u64
    }
}

/// The adapter half of the failure replay: the artist-shaped [`Discovery`] a
/// replayed entry stands for.
///
/// See [`replayable_failure`] for when an entry may be replayed at all.
fn replay_recorded_failure(
    db: &Database,
    artist_key: &str,
    config: &DiscographyConfig,
    now: i64,
    use_cache: FailureCacheUse,
    configured_mbid: Option<&str>,
    has_success_row: bool,
) -> Option<Discovery> {
    let entry = replayable_failure(
        db,
        artist_key,
        config,
        now,
        use_cache,
        configured_mbid,
        has_success_row,
    )?;
    let reason = if entry.reason.trim().is_empty() {
        "artist could not be resolved safely".to_string()
    } else {
        entry.reason
    };
    Some(Discovery {
        outcome: DiscoveryOutcome::LegacyFallback {
            reason,
            kind: DiscoveryFailure::Unresolved,
            from_cache: true,
        },
        canonical_name: None,
        mbid: None,
        disambiguation: None,
    })
}

/// The decision half of [`replay_recorded_failure`]: the recorded entry when it
/// may stand in for the provider call, `None` when the lookup must run.
///
/// Returns `None` when there is nothing to replay. In order of the checks below:
/// a pinned MBID makes resolution deterministic, so the recorded row is deleted
/// and the lookup runs; the caller bypassed the cache; the configured expiry is
/// `0`; a compatible success row exists, which is the better answer even when it
/// is stale; no row exists; the row is stale; or its kind is unknown.
///
/// Every one of those is a miss rather than an error, because a cache must never
/// be able to block a lookup it was not written for.
pub(crate) fn replayable_failure(
    db: &Database,
    key: &str,
    config: &DiscographyConfig,
    now: i64,
    use_cache: FailureCacheUse,
    configured_mbid: Option<&str>,
    has_success_row: bool,
) -> Option<DiscographyFailureEntry> {
    if configured_mbid.is_some() {
        // A pinned MBID makes resolution deterministic, so any recorded failure
        // is obsolete. Drop it and let the lookup run.
        if let Err(error) = db.delete_discography_failure(key) {
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
    // or the entity would be skipped instead of falling back to those albums.
    if has_success_row {
        return None;
    }
    let entry = match db.get_discography_failure(key) {
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
    tracing::debug!(
        key,
        recorded_at = entry.recorded_at,
        "skipping a recorded resolution failure"
    );
    Some(entry)
}

/// Record a resolution failure so a later run can skip the lookup.
///
/// Only a failure to match a name is recorded: an explicit code or ID always
/// bypasses the recorded failure, so a row for one could never be replayed. Which
/// errors qualify is [`DiscographyError::is_stable_resolution_failure`]'s
/// decision. That
/// excludes a `Provider` failure, because an outage would otherwise keep
/// suppressing a healthy MusicBrainz for the whole expiry, and it excludes
/// [`DiscographyError::ArtistSearchInconsistent`] and
/// [`DiscographyError::LabelSearchInconsistent`], which are classified as
/// resolution problems for this run's fallback but describe one response. Nothing is
/// recorded when the feature is off, when there is no usable artist key, or when
/// a success row already exists for this key — the two kinds of row are mutually
/// exclusive by construction, so a stale-but-usable success row is never
/// shadowed by a fresh failure.
///
/// The key is namespaced by the caller - `label:name:<key>` for the label path,
/// `name`/`isni:`/`ipi:`/`mbid:` for artists. The namespace is a prefix, not a column
/// of its own, so the two paths are separated exactly as far as their keys are; see
/// [`label::LabelInput::cache_key`] for what that means for a deliberately confusing
/// name.
pub(crate) fn record_resolution_failure(
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
    if !error.is_stable_resolution_failure() {
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

/// Reject a cache payload over the 16 MiB limit (a refresh error that falls
/// back to a stale cache or legacy discovery when no cache exists).
pub(crate) fn check_cache_payload_size(payload: &str) -> std::result::Result<(), DiscographyError> {
    if payload.len() > MAX_CACHE_PAYLOAD_BYTES {
        return Err(DiscographyError::Decode(format!(
            "cache payload of {} bytes exceeds the {MAX_CACHE_PAYLOAD_BYTES}-byte limit",
            payload.len()
        )));
    }
    Ok(())
}

/// Serialize a release-group list for the cache, rejecting a payload over the
/// 16 MiB limit.
fn serialize_cache_payload(
    groups: &[ReleaseGroup],
) -> std::result::Result<String, DiscographyError> {
    let payload = serde_json::to_string(groups)
        .map_err(|error| DiscographyError::Decode(format!("failed to serialize cache: {error}")))?;
    check_cache_payload_size(&payload)?;
    Ok(payload)
}

fn select_outcome(
    groups: &[ReleaseGroup],
    allowed: &[DiscographyReleaseType],
    artist: &str,
    provenance: DiscoveryProvenance,
    canonical_name: Option<&str>,
) -> DiscoveryOutcome {
    let albums = select_albums(groups, allowed, artist);
    let canonical_name = canonical_name.map(str::to_string);
    if albums.is_empty() {
        DiscoveryOutcome::AuthoritativeEmpty {
            provenance,
            canonical_name,
        }
    } else {
        DiscoveryOutcome::Authoritative {
            albums,
            provenance,
            canonical_name,
        }
    }
}

/// On a refresh error, return the compatible stale cache when present
/// (converting a filtered-empty stale list to `AuthoritativeEmpty`), else
/// `LegacyFallback` with the exact typed error text.
fn stale_or_legacy(
    cached: Option<CachedDiscography>,
    allowed: &[DiscographyReleaseType],
    input: &ArtistInput,
    artist: &str,
    now: i64,
    refresh_error: DiscographyError,
) -> Discovery {
    let Some(cached) = cached else {
        return Discovery {
            outcome: DiscoveryOutcome::LegacyFallback {
                kind: DiscoveryFailure::from_error(&refresh_error),
                reason: refresh_error.to_string(),
                from_cache: false,
            },
            canonical_name: None,
            mbid: None,
            disambiguation: None,
        };
    };
    let provenance = DiscoveryProvenance::StaleCache {
        age_days: stale_age_days(now, cached.entry.fetched_at),
        kind: DiscoveryFailure::from_error(&refresh_error),
        refresh_error: refresh_error.to_string(),
    };
    let canonical_name = cached.entry.canonical_artist.clone();
    Discovery {
        outcome: select_outcome(
            &cached.groups,
            allowed,
            strip_artist(input, artist, &canonical_name),
            provenance,
            Some(&canonical_name),
        ),
        canonical_name: Some(canonical_name),
        mbid: Some(cached.entry.artist_mbid.clone()),
        disambiguation: None,
    }
}

/// The spelling MusicBrainz release-group titles are matched against.
///
/// An identity request has no spelling of the operator's own, so it uses the
/// resolved canonical name: that is what lets `album_identity_key` strip an
/// `<artist> - ` prefix from a release-group title, so the work item carries the
/// album title alone. A name request keeps the value the operator gave.
fn strip_artist<'a>(input: &ArtistInput, artist: &'a str, canonical_name: &'a str) -> &'a str {
    if input.is_identity() {
        canonical_name
    } else {
        artist
    }
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
            ipis: Vec::new(),
            isnis: Vec::new(),
            disambiguation: None,
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

    fn carrier(id: &str, name: &str, ipis: &[&str], isnis: &[&str]) -> ArtistCandidate {
        ArtistCandidate {
            id: id.to_string(),
            name: name.to_string(),
            score: Some(100),
            ipis: ipis.iter().map(|value| value.to_string()).collect(),
            isnis: isnis.iter().map(|value| value.to_string()).collect(),
            disambiguation: None,
        }
    }

    #[test]
    fn a_code_with_exactly_one_carrier_resolves() {
        let candidates = vec![
            carrier(
                "11111111-1111-1111-1111-111111111111",
                "Grafix",
                &[],
                &["0000000466597022"],
            ),
            carrier("22222222-2222-2222-2222-222222222222", "Other", &[], &[]),
        ];

        let resolved = resolve_code(CodeIdentifier::Isni("0000000466597022"), &candidates).unwrap();

        assert_eq!(resolved.candidate.name, "Grafix");
        assert_eq!(resolved.resolution, ArtistResolution::Identity);
    }

    #[test]
    fn a_search_hit_that_does_not_claim_the_code_is_discarded() {
        // The search index is fuzzy: a hit that does not carry the code is not a
        // carrier, however it scored.
        let candidates = vec![carrier(
            "11111111-1111-1111-1111-111111111111",
            "Grafix",
            &["99999999999"],
            &["0000000000000000"],
        )];

        let error =
            resolve_code(CodeIdentifier::Isni("0000000466597022"), &candidates).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("no MusicBrainz artist carries ISNI 0000000466597022"),
            "got {error}"
        );
    }

    #[test]
    fn a_code_with_no_carrier_names_the_code() {
        let error = resolve_code(CodeIdentifier::Ipi("00581977202"), &[]).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("no MusicBrainz artist carries IPI 00581977202"),
            "got {error}"
        );
    }

    #[test]
    fn a_shared_code_names_the_count_and_every_carrier() {
        let candidates = vec![
            carrier(
                "11111111-1111-1111-1111-111111111111",
                "Grafix",
                &["00581977202"],
                &[],
            ),
            carrier(
                "22222222-2222-2222-2222-222222222222",
                "Grafix Band",
                &["00581977202"],
                &[],
            ),
        ];

        let error = resolve_code(CodeIdentifier::Ipi("00581977202"), &candidates).unwrap_err();
        let message = error.to_string();

        assert!(
            message.contains("2 artists carry IPI 00581977202"),
            "got {message}"
        );
        assert!(
            message.contains("Grafix (11111111-1111-1111-1111-111111111111)"),
            "got {message}"
        );
        assert!(
            message.contains("Grafix Band (22222222-2222-2222-2222-222222222222)"),
            "got {message}"
        );
        assert!(
            message.contains("discography.artist_mbids"),
            "the message must point at the fix: {message}"
        );
    }

    #[test]
    fn carrier_comparison_ignores_spacing_and_isni_letter_case() {
        let candidates = vec![carrier(
            "11111111-1111-1111-1111-111111111111",
            "Grafix",
            &[],
            &["0000 0004 6659 7022"],
        )];

        assert!(resolve_code(CodeIdentifier::Isni("0000000466597022"), &candidates).is_ok());
    }

    #[test]
    fn no_carrier_is_an_artist_problem_not_an_outage() {
        // The circuit breaker ignores it, and nothing is recorded for a code: an
        // explicit code always asks again.
        let error = resolve_code(CodeIdentifier::Isni("0000000466597022"), &[]).unwrap_err();
        assert!(
            matches!(error, DiscographyError::ArtistUnresolved(_)),
            "got {error:?}"
        );
        assert_eq!(
            DiscoveryFailure::from_error(&error),
            DiscoveryFailure::Unresolved
        );
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
    fn catalog_keys_fold_apostrophe_variants() {
        // MusicBrainz stores the typographic apostrophe ("Sgt. Pepper’s",
        // "Guns N’ Roses") while tags and folder names carry the ASCII one, or
        // none at all. All three spellings name the same artist and the same
        // album, so they must share one key.
        assert_eq!(
            normalize_catalog_key("Guns N\u{2019} Roses"),
            normalize_catalog_key("Guns 'n' Roses")
        );
        assert_eq!(
            normalize_catalog_key("Sgt. Pepper\u{2019}s Lonely Hearts Club Band"),
            normalize_catalog_key("Sgt. Pepper's Lonely Hearts Club Band")
        );
        assert_eq!(
            normalize_catalog_key("Sgt. Pepper's Lonely Hearts Club Band"),
            normalize_catalog_key("Sgt. Peppers Lonely Hearts Club Band")
        );
    }

    #[test]
    fn catalog_keys_fold_a_compatibility_variant_of_the_apostrophe() {
        // U+FF07 is the fullwidth apostrophe. NFKC folds it to the ASCII one, so
        // the filter has to run after the fold as well as before it, or a
        // fullwidth keyboard tag keeps its own key and one artist or album is
        // treated as two - the very duplication this fold exists to remove.
        assert_eq!(
            normalize_catalog_key("Guns N\u{ff07} Roses"),
            normalize_catalog_key("Guns N' Roses")
        );
        assert_eq!(
            normalize_catalog_key("Guns N\u{ff07} Roses"),
            normalize_catalog_key("Guns N\u{2019} Roses")
        );
        // Lookalikes with NO compatibility decomposition survive NFKC entirely,
        // so they have to be listed: U+201A is what a CP1252/SmartQuotes round
        // trip can leave in place of an apostrophe.
        assert_eq!(
            normalize_catalog_key("Guns N\u{201a} Roses"),
            normalize_catalog_key("Guns N' Roses")
        );
        assert_eq!(
            normalize_catalog_key("Guns N\u{02bb} Roses"),
            normalize_catalog_key("Guns N' Roses")
        );
        // The COMBINING forms are never produced by NFKC - they ARE the
        // decomposition - so a tag carrying one directly has to be listed too.
        assert_eq!(
            normalize_catalog_key("Guns N\u{0313} Roses"),
            normalize_catalog_key("Guns N' Roses")
        );
        assert_eq!(
            normalize_catalog_key("Guns N\u{05f3} Roses"),
            normalize_catalog_key("Guns N' Roses")
        );
    }

    #[test]
    fn artist_resolution_folds_apostrophe_variants_of_the_canonical_name() {
        // The library spells it "Guns 'n' Roses" while MusicBrainz spells it
        // "Guns N’ Roses" with U+2019. Resolution accepts a candidate only when
        // it compares equal to the query after folding, so without the fold the
        // two never meet and the artist is skipped as unresolved on every
        // discover cycle, even though its folder is sitting in the library.
        let candidates = vec![scored(
            "eeb1195b-f213-4ce1-b28c-8565211f8e43",
            "Guns N\u{2019} Roses",
            Some(100),
        )];
        let resolved = resolve_artist("Guns 'n' Roses", &candidates)
            .expect("the library spelling must resolve against the canonical name");
        assert_eq!(resolved.candidate.name, "Guns N\u{2019} Roses");
        assert_eq!(resolved.resolution, ArtistResolution::UniqueExactName);
    }

    #[test]
    fn exact_artist_resolution_uses_only_unique_canonical_name() {
        let candidates = vec![
            ArtistCandidate {
                id: "1".into(),
                name: "ＡC/DC".into(),
                score: None,
                ipis: Vec::new(),
                isnis: Vec::new(),
                disambiguation: None,
            },
            ArtistCandidate {
                id: "2".into(),
                name: "AC DC".into(),
                score: None,
                ipis: Vec::new(),
                isnis: Vec::new(),
                disambiguation: None,
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
                    ipis: Vec::new(),
                    isnis: Vec::new(),
                    disambiguation: None,
                },
                ArtistCandidate {
                    id: "3".into(),
                    name: " ac dc ".into(),
                    score: Some(100),
                    ipis: Vec::new(),
                    isnis: Vec::new(),
                    disambiguation: None,
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
        assert!(select_albums(
            &[group("4", "   ", None, Some("Album"), &[])],
            &allowed,
            "Artist"
        )
        .is_empty());
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
            "Artist",
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
            "Archive",
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
        let albums = select_albums(&groups, &[DiscographyReleaseType::StudioAlbum], "Metallica");
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
        let albums = select_albums(&groups, &[DiscographyReleaseType::StudioAlbum], "Artist");
        let titles: Vec<&str> = albums.iter().map(|album| album.title.as_str()).collect();
        assert_eq!(titles, vec!["Second Album"]);
    }

    #[test]
    fn release_groups_of_one_album_are_deduplicated_by_identity() {
        // MusicBrainz carries more than one release group for the same album.
        // This Sgt. Pepper pair is real: `9f7a4c28-…` is the 1967 album spelled
        // with U+2019, `e2efefb2-…` the 2009 album spelled with ASCII. Keying the
        // dedupe on the raw title made each a separate target, so a discover run
        // searched and downloaded the same album once per spelling, two minutes
        // apart, into two new folders beside the copy already on disk.
        let groups = vec![
            group(
                "9f7a4c28-8fa2-3113-929c-c47a9f7982c3",
                "Sgt. Pepper\u{2019}s Lonely Hearts Club Band",
                Some("1967-05-26"),
                Some("Album"),
                &[],
            ),
            group(
                "e2efefb2-0e9d-47c1-9e62-bb7e093d401b",
                "Sgt. Pepper's Lonely Hearts Club Band",
                Some("2009"),
                Some("Album"),
                &[],
            ),
            group(
                "aaaabbbb-0000-0000-0000-000000000001",
                "1998 Sgt. Peppers Lonely Hearts Club Band",
                None,
                Some("Album"),
                &[],
            ),
        ];
        let albums = select_albums(
            &groups,
            &[DiscographyReleaseType::StudioAlbum],
            "The Beatles",
        );
        assert_eq!(
            albums.len(),
            1,
            "one album must yield one target: {albums:?}"
        );
        assert_eq!(
            albums[0].release_group_id, "9f7a4c28-8fa2-3113-929c-c47a9f7982c3",
            "the dated original wins over the undated re-spelling"
        );
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
            "Archive",
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
        let albums = select_albums(&groups, &[DiscographyReleaseType::StudioAlbum], "Artist");
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
        code_responses:
            Mutex<VecDeque<std::result::Result<Vec<ArtistCandidate>, DiscographyError>>>,
        group_responses: Mutex<VecDeque<std::result::Result<Vec<ReleaseGroup>, DiscographyError>>>,
        requested_group_mbids: Mutex<Vec<String>>,
        artist_calls: AtomicUsize,
        code_calls: AtomicUsize,
        lookup_calls: AtomicUsize,
        group_calls: AtomicUsize,
        failure: Option<String>,
        /// Answer the artist search with a page whose reported total disagrees
        /// with the candidates it carried.
        inconsistent: bool,
        /// The candidate a lookup-by-MBID answers with; `None` keeps the default
        /// `Artist` naming the existing tests rely on.
        id_response: Option<ArtistCandidate>,
        requested_codes: Mutex<Vec<String>>,
        requested_artists: Mutex<Vec<String>>,
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
                    ipis: Vec::new(),
                    isnis: Vec::new(),
                    disambiguation: None,
                }])])),
                group_responses: Mutex::new(VecDeque::from([Ok(groups)])),
                ..Self::default()
            }
        }

        /// A code search that answers with `candidates`, then the release groups.
        fn with_code(candidates: Vec<ArtistCandidate>, groups: Vec<ReleaseGroup>) -> Self {
            Self {
                code_responses: Mutex::new(VecDeque::from([Ok(candidates)])),
                group_responses: Mutex::new(VecDeque::from([Ok(groups)])),
                ..Self::default()
            }
        }

        /// An entity lookup that answers with the named artist.
        fn with_id(mbid: &str, name: &str) -> Self {
            Self {
                id_response: Some(ArtistCandidate {
                    id: mbid.to_string(),
                    name: name.to_string(),
                    score: None,
                    ipis: Vec::new(),
                    isnis: Vec::new(),
                    disambiguation: None,
                }),
                ..Self::default()
            }
        }

        /// A name search that answers with `candidates`.
        fn with_search(candidates: Vec<ArtistCandidate>) -> Self {
            Self {
                artist_responses: Mutex::new(VecDeque::from([Ok(candidates)])),
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
                    ipis: Vec::new(),
                    isnis: Vec::new(),
                    disambiguation: None,
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

        /// Every artist-index search, by name or by code.
        fn search_calls(&self) -> usize {
            self.artist_calls.load(Ordering::SeqCst) + self.code_calls.load(Ordering::SeqCst)
        }

        /// The codes asked about, as `KIND:value`.
        fn requested_codes(&self) -> Vec<String> {
            self.requested_codes
                .lock()
                .map(|codes| codes.clone())
                .unwrap_or_default()
        }

        /// The names asked about.
        fn requested_artists(&self) -> Vec<String> {
            self.requested_artists
                .lock()
                .map(|names| names.clone())
                .unwrap_or_default()
        }
    }

    #[async_trait]
    impl DiscographyProvider for FakeProvider {
        async fn search_artists(
            &self,
            artist: &str,
        ) -> std::result::Result<Vec<ArtistCandidate>, DiscographyError> {
            self.artist_calls.fetch_add(1, Ordering::SeqCst);
            if let Ok(mut requested) = self.requested_artists.lock() {
                requested.push(artist.to_string());
            }
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

        async fn search_artists_by_code(
            &self,
            code: CodeIdentifier<'_>,
        ) -> std::result::Result<Vec<ArtistCandidate>, DiscographyError> {
            self.code_calls.fetch_add(1, Ordering::SeqCst);
            if let Ok(mut requested) = self.requested_codes.lock() {
                requested.push(format!("{}:{}", code.kind(), code.value()));
            }
            if let Some(reason) = &self.failure {
                return Err(DiscographyError::Transport(reason.clone()));
            }
            self.code_responses
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
            if let Some(candidate) = &self.id_response {
                return Ok(candidate.clone());
            }
            Ok(ArtistCandidate {
                id: artist_mbid.to_string(),
                name: "Artist".to_string(),
                score: None,
                ipis: Vec::new(),
                isnis: Vec::new(),
                disambiguation: None,
            })
        }

        async fn search_labels(
            &self,
            _label: &str,
        ) -> std::result::Result<Vec<label::LabelCandidate>, DiscographyError> {
            panic!("artist-sweep tests never query labels")
        }

        async fn label_by_id(
            &self,
            _label_mbid: &str,
        ) -> std::result::Result<label::LabelCandidate, DiscographyError> {
            panic!("artist-sweep tests never query labels")
        }

        async fn releases_by_label(
            &self,
            _label_mbid: &str,
        ) -> std::result::Result<label::LabelCatalogue, DiscographyError> {
            panic!("artist-sweep tests never query a label catalogue")
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
    async fn an_isni_input_is_searched_by_code_and_reports_the_canonical_name() {
        let provider = FakeProvider::with_code(
            vec![carrier(
                "c02a5966-b5c4-483a-8326-483edcf3680e",
                "Grafix",
                &[],
                &["0000000466597022"],
            )],
            vec![group(
                "rg-1",
                "Half Light",
                Some("2019"),
                Some("Album"),
                &[],
            )],
        );
        let db = Database::open_in_memory().unwrap();
        let config = DiscographyConfig::default();

        let discovery = discover_artist_albums_at(
            &provider,
            &db,
            &ArtistInput::parse("0000 0004 6659 7022"),
            &config,
            1_700_000_000,
            FailureCacheUse::Bypass,
        )
        .await;

        assert_eq!(
            provider.requested_codes(),
            vec!["ISNI:0000000466597022".to_string()]
        );
        assert_eq!(
            discovery.canonical_name.as_deref(),
            Some("Grafix"),
            "the runner needs the canonical name to search and place"
        );
        assert!(matches!(
            discovery.outcome,
            DiscoveryOutcome::Authoritative { .. }
        ));
        assert_eq!(
            db.get_discography_cache("isni:0000000466597022")
                .unwrap()
                .map(|row| row.canonical_artist),
            Some("Grafix".to_string()),
            "the cache row is keyed by the code"
        );
    }

    #[tokio::test]
    async fn an_mbid_input_is_looked_up_by_id_without_a_search() {
        let provider = FakeProvider::with_id("c02a5966-b5c4-483a-8326-483edcf3680e", "Grafix");
        let db = Database::open_in_memory().unwrap();
        let config = DiscographyConfig::default();

        let discovery = discover_artist_albums_at(
            &provider,
            &db,
            &ArtistInput::parse("c02a5966-b5c4-483a-8326-483edcf3680e"),
            &config,
            1_700_000_000,
            FailureCacheUse::Bypass,
        )
        .await;

        assert_eq!(
            provider.search_calls(),
            0,
            "an ID must not go through the search index"
        );
        assert_eq!(discovery.canonical_name.as_deref(), Some("Grafix"));
    }

    #[tokio::test]
    async fn a_code_pinned_in_config_is_looked_up_by_id_instead() {
        let provider = FakeProvider::with_id("c02a5966-b5c4-483a-8326-483edcf3680e", "Grafix");
        let db = Database::open_in_memory().unwrap();
        let mut config = DiscographyConfig::default();
        config.artist_mbids.insert(
            "isni:0000000466597022".to_string(),
            "c02a5966-b5c4-483a-8326-483edcf3680e".to_string(),
        );

        discover_artist_albums_at(
            &provider,
            &db,
            &ArtistInput::parse("0000 0004 6659 7022"),
            &config,
            1_700_000_000,
            FailureCacheUse::Bypass,
        )
        .await;

        assert_eq!(provider.search_calls(), 0, "a pinned code skips the search");
    }

    #[tokio::test]
    async fn a_name_input_keeps_the_name_path_and_cache_key() {
        let provider = FakeProvider::with_search(vec![scored(
            "11111111-1111-1111-1111-111111111111",
            "Grafix",
            Some(100),
        )]);
        let db = Database::open_in_memory().unwrap();
        let config = DiscographyConfig::default();

        let discovery = discover_artist_albums_at(
            &provider,
            &db,
            &ArtistInput::parse("Grafix"),
            &config,
            1_700_000_000,
            FailureCacheUse::Bypass,
        )
        .await;

        assert_eq!(provider.requested_artists(), vec!["Grafix".to_string()]);
        assert_eq!(
            db.get_discography_cache("grafix")
                .unwrap()
                .map(|row| row.canonical_artist),
            Some("Grafix".to_string())
        );
        assert_eq!(discovery.canonical_name.as_deref(), Some("Grafix"));
    }

    #[tokio::test]
    async fn an_unresolved_artist_is_recorded() {
        let db = Database::open_in_memory().unwrap();
        let provider = FakeProvider::unresolvable_recording();

        discover_artist_albums_at(
            &provider,
            &db,
            &ArtistInput::name("Unknown"),
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
            &ArtistInput::name("Artist"),
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
            &ArtistInput::name("Unknown"),
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
            &ArtistInput::name("Artist"),
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
            &ArtistInput::name("Artist"),
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
            &ArtistInput::name("Unknown"),
            &DiscographyConfig::default(),
            1_100,
            FailureCacheUse::Honour,
        )
        .await;

        assert!(matches!(
            outcome.outcome,
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
            &ArtistInput::name("Unknown"),
            &config,
            1_000 + ttl as i64,
            FailureCacheUse::Honour,
        )
        .await;

        assert!(matches!(
            outcome.outcome,
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
            &ArtistInput::name("Unknown"),
            &config,
            1_000 + ttl as i64 + 1,
            FailureCacheUse::Honour,
        )
        .await;

        assert!(matches!(
            outcome.outcome,
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
            &ArtistInput::name("Unknown"),
            &config,
            1_100,
            FailureCacheUse::Honour,
        )
        .await;

        assert!(matches!(
            outcome.outcome,
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
            &ArtistInput::name("Unknown"),
            &DiscographyConfig::default(),
            1_100,
            FailureCacheUse::Bypass,
        )
        .await;

        assert!(matches!(
            outcome.outcome,
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
            &ArtistInput::name("Artist"),
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
                ipis: Vec::new(),
                isnis: Vec::new(),
                disambiguation: None,
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
            &ArtistInput::name("Artist"),
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
            &ArtistInput::name("Artist"),
            &DiscographyConfig::default(),
            1_000,
            FailureCacheUse::Honour,
        )
        .await;

        assert!(matches!(
            outcome.outcome,
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
            &ArtistInput::name("Artist"),
            &config,
            1_000_100,
            FailureCacheUse::Honour,
        )
        .await;

        assert!(
            matches!(
                outcome.outcome,
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
            &ArtistInput::name("Unknown"),
            &DiscographyConfig::default(),
            1_100,
            FailureCacheUse::Honour,
        )
        .await;

        assert!(matches!(
            outcome.outcome,
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
            &ArtistInput::name("Artist"),
            &config,
            1_100,
            FailureCacheUse::Honour,
        )
        .await;

        assert!(matches!(outcome.outcome, DiscoveryOutcome::Authoritative {
            provenance: DiscoveryProvenance::FreshCache,
            ref albums,
            ..
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
            &ArtistInput::name("Artist"),
            &config,
            31 * 86_400,
            FailureCacheUse::Honour,
        )
        .await;

        assert!(matches!(
            outcome.outcome,
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
            &ArtistInput::name("Unknown"),
            &DiscographyConfig::default(),
            1_000,
            FailureCacheUse::Honour,
        )
        .await;

        assert!(matches!(
            outcome.outcome,
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
            &ArtistInput::name("Artist"),
            &config,
            5_000,
            FailureCacheUse::Honour,
        )
        .await;

        assert!(matches!(
            outcome.outcome,
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
            &ArtistInput::name("Artist"),
            &DiscographyConfig::default(),
            now,
            FailureCacheUse::Honour,
        )
        .await;

        assert!(
            matches!(
                outcome.outcome,
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
            &ArtistInput::name("Artist"),
            &DiscographyConfig::default(),
            10_000,
            FailureCacheUse::Honour,
        )
        .await;

        assert!(
            matches!(
                outcome.outcome,
                DiscoveryOutcome::Authoritative {
                    provenance: DiscoveryProvenance::StaleCache { age_days: 0, .. },
                    ..
                }
            ),
            "a future timestamp must never be FreshCache; its age must saturate at zero"
        );
        assert!(!matches!(
            outcome.outcome,
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
            &ArtistInput::name("Artist"),
            &config,
            6_000,
            FailureCacheUse::Honour,
        )
        .await;

        assert!(
            matches!(outcome.outcome, DiscoveryOutcome::LegacyFallback { .. }),
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
            &ArtistInput::name("Artist"),
            &config,
            6_000,
            FailureCacheUse::Honour,
        )
        .await;
        assert!(matches!(
            outcome.outcome,
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
            &ArtistInput::name("Artist"),
            &DiscographyConfig::default(),
            5_000,
            FailureCacheUse::Honour,
        )
        .await;

        assert!(matches!(
            outcome.outcome,
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
            &ArtistInput::name("Artist"),
            &DiscographyConfig::default(),
            1_000,
            FailureCacheUse::Honour,
        )
        .await;

        assert!(
            matches!(
                outcome.outcome,
                DiscoveryOutcome::AuthoritativeEmpty {
                    provenance: DiscoveryProvenance::Refreshed,
                    ..
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
            &ArtistInput::name("Artist"),
            &DiscographyConfig::default(),
            1_000,
            FailureCacheUse::Honour,
        )
        .await;

        assert!(matches!(
            outcome.outcome,
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
            &ArtistInput::name("Artist"),
            &DiscographyConfig::default(),
            31 * 86_400,
            FailureCacheUse::Honour,
        )
        .await;

        assert!(
            matches!(outcome.outcome, DiscoveryOutcome::Authoritative {
            provenance: DiscoveryProvenance::StaleCache { .. },
            ref albums,
            ..
        } if albums.iter().map(|album| album.title.as_str()).collect::<Vec<_>>() == ["Studio"]),
            "an oversized refresh payload must fall back to the compatible stale cache"
        );

        let db = Database::open_in_memory().unwrap();
        let provider = FakeProvider::with_groups(oversized);
        let outcome = discover_artist_albums_at(
            &provider,
            &db,
            &ArtistInput::name("Artist"),
            &DiscographyConfig::default(),
            31 * 86_400,
            FailureCacheUse::Honour,
        )
        .await;
        assert!(
            matches!(outcome.outcome, DiscoveryOutcome::LegacyFallback { .. }),
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
                &ArtistInput::name("dominance probe"),
                &DiscographyConfig::default(),
                1_000,
                FailureCacheUse::Honour,
            ));

        assert!(matches!(
            outcome.outcome,
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
            &ArtistInput::name("Ils"),
            &DiscographyConfig::default(),
            1_000,
            FailureCacheUse::Honour,
        )
        .await;
        assert!(matches!(
            outcome.outcome,
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
            &ArtistInput::name("Ils"),
            &DiscographyConfig::default(),
            31 * 86_400,
            FailureCacheUse::Honour,
        )
        .await;

        assert!(matches!(
            outcome.outcome,
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
            &ArtistInput::name("Ils"),
            &DiscographyConfig::default(),
            1_000,
            FailureCacheUse::Honour,
        )
        .await;

        assert!(matches!(
            outcome.outcome,
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
            &ArtistInput::name("Ils"),
            &config,
            1_000,
            FailureCacheUse::Honour,
        )
        .await;

        assert!(matches!(
            outcome.outcome,
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
            &ArtistInput::name("Ils"),
            &DiscographyConfig::default(),
            1_100,
            FailureCacheUse::Honour,
        )
        .await;

        assert!(matches!(
            outcome.outcome,
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
            &ArtistInput::name("Unknown"),
            &DiscographyConfig::default(),
            1_000,
            FailureCacheUse::Honour,
        )
        .await;

        assert!(matches!(
            outcome.outcome,
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
            &ArtistInput::name("Artist"),
            &DiscographyConfig::default(),
            1_000,
            FailureCacheUse::Honour,
        )
        .await;

        assert!(matches!(
            outcome.outcome,
            DiscoveryOutcome::LegacyFallback {
                kind: DiscoveryFailure::Provider,
                ..
            }
        ));
    }

    #[test]
    fn a_rate_limited_5xx_is_a_provider_outage_not_an_artist_failure() {
        // The catch-all arm is what makes the new variant an outage. Pinning it
        // stops a later edit from moving it into the resolution arm, which
        // would change the breaker; cacheability is protected separately by
        // `is_stable_resolution_failure`, which only accepts
        // `ArtistUnresolved` and `LabelUnresolved`.
        let error = DiscographyError::ProviderError {
            status: 503,
            detail: String::new(),
        };
        assert_eq!(
            DiscoveryFailure::from_error(&error),
            DiscoveryFailure::Provider
        );
        assert!(!error.is_stable_resolution_failure());
    }

    #[test]
    fn label_resolution_failures_are_classified_for_the_breaker_and_the_cache() {
        let unresolved =
            DiscographyError::LabelUnresolved("no candidate matches \"hospital\"".into());
        assert_eq!(
            DiscoveryFailure::from_error(&unresolved),
            DiscoveryFailure::Unresolved
        );
        assert!(unresolved.is_stable_resolution_failure());

        let inconsistent =
            DiscographyError::LabelSearchInconsistent("reported 2, carried 1".into());
        assert_eq!(
            DiscoveryFailure::from_error(&inconsistent),
            DiscoveryFailure::Unresolved
        );
        assert!(
            !inconsistent.is_stable_resolution_failure(),
            "an inconsistent search page describes one response and must not be cached"
        );

        let outage = DiscographyError::HttpStatus(503);
        assert_eq!(
            DiscoveryFailure::from_error(&outage),
            DiscoveryFailure::Provider
        );
        assert!(!outage.is_stable_resolution_failure());
    }

    #[test]
    fn an_album_identity_is_folded_with_the_resolved_name_not_the_code() {
        // `album_identity_key` strips an `<artist> - ` prefix, so the spelling passed
        // as the artist decides whether a prefixed release group and a bare one are
        // one album. The identifier text folds nothing; the canonical name folds both.
        let groups = vec![
            group(
                "rg-prefixed",
                "Grafix - Half Light",
                Some("2018"),
                Some("Album"),
                &[],
            ),
            group("rg-bare", "Half Light", Some("2019"), Some("Album"), &[]),
        ];
        let allowed = [DiscographyReleaseType::StudioAlbum];

        assert_eq!(
            select_albums(&groups, &allowed, "00581977202").len(),
            2,
            "a code is not a spelling, so nothing folds"
        );
        assert_eq!(
            select_albums(&groups, &allowed, "Grafix").len(),
            1,
            "the canonical name folds the prefixed title onto the bare one"
        );
    }

    #[tokio::test]
    async fn a_code_run_folds_album_identities_with_the_resolved_name() {
        // End to end: the run must fold the two release groups into one work item,
        // and report the identity it resolved so the summary can name it.
        let provider = FakeProvider::with_code(
            vec![carrier(
                "c02a5966-b5c4-483a-8326-483edcf3680e",
                "Grafix",
                &[],
                &["0000000466597022"],
            )],
            vec![
                group(
                    "rg-prefixed",
                    "Grafix - Half Light",
                    Some("2018"),
                    Some("Album"),
                    &[],
                ),
                group("rg-bare", "Half Light", Some("2019"), Some("Album"), &[]),
            ],
        );
        let db = Database::open_in_memory().unwrap();
        let config = DiscographyConfig::default();

        let discovery = discover_artist_albums_at(
            &provider,
            &db,
            &ArtistInput::parse("0000 0004 6659 7022"),
            &config,
            1_700_000_000,
            FailureCacheUse::Bypass,
        )
        .await;

        match &discovery.outcome {
            DiscoveryOutcome::Authoritative { albums, .. } => {
                assert_eq!(
                    albums.len(),
                    1,
                    "the prefixed and the bare title are one album under the canonical name"
                );
            }
            other => panic!("expected authoritative albums, got {other:?}"),
        }
        assert_eq!(discovery.canonical_name.as_deref(), Some("Grafix"));
        assert_eq!(
            discovery.mbid.as_deref(),
            Some("c02a5966-b5c4-483a-8326-483edcf3680e"),
            "the run summary needs the resolved ID"
        );
    }

    #[tokio::test]
    async fn a_code_failure_is_not_recorded_because_it_could_never_be_replayed() {
        let provider = FakeProvider::with_code(Vec::new(), Vec::new());
        let db = Database::open_in_memory().unwrap();
        let config = DiscographyConfig::default();

        let discovery = discover_artist_albums_at(
            &provider,
            &db,
            &ArtistInput::parse("0000 0004 6659 7022"),
            &config,
            1_700_000_000,
            FailureCacheUse::Bypass,
        )
        .await;

        assert!(matches!(
            discovery.outcome,
            DiscoveryOutcome::LegacyFallback { .. }
        ));
        assert!(
            db.get_discography_failure("isni:0000000466597022")
                .unwrap()
                .is_none(),
            "an explicit code always asks again, so a failure row for one is dead weight"
        );
    }

    #[tokio::test]
    async fn a_code_run_bypasses_a_recorded_failure() {
        let provider = FakeProvider::with_code(
            vec![carrier(
                "c02a5966-b5c4-483a-8326-483edcf3680e",
                "Grafix",
                &[],
                &["0000000466597022"],
            )],
            vec![group(
                "rg-1",
                "Half Light",
                Some("2019"),
                Some("Album"),
                &[],
            )],
        );
        let db = Database::open_in_memory().unwrap();
        db.upsert_discography_failure(&crate::db::DiscographyFailureEntry {
            artist_key: "isni:0000000466597022".to_string(),
            failure_kind: "unresolved".to_string(),
            reason: "no candidate matches".to_string(),
            // Fresh relative to the injected `now` below: a row recorded at the real
            // clock is already stale there, so the test would pass without the bypass.
            recorded_at: 1_700_000_000 - 60,
        })
        .unwrap();
        let config = DiscographyConfig::default();

        let discovery = discover_artist_albums_at(
            &provider,
            &db,
            &ArtistInput::parse("0000 0004 6659 7022"),
            &config,
            1_700_000_000,
            FailureCacheUse::Bypass,
        )
        .await;

        assert_eq!(
            provider.requested_codes().len(),
            1,
            "a recorded failure must not stop an explicitly requested code"
        );
        assert!(matches!(
            discovery.outcome,
            DiscoveryOutcome::Authoritative { .. }
        ));
    }

    #[test]
    fn carrier_comparison_is_case_insensitive_for_an_isni_check_character() {
        let candidates = vec![carrier(
            "11111111-1111-1111-1111-111111111111",
            "X Carrier",
            &[],
            &["0000 0002 1694 233x"],
        )];

        assert!(
            resolve_code(CodeIdentifier::Isni("000000021694233X"), &candidates).is_ok(),
            "a lower-case x in the candidate's own record must still match"
        );
    }

    #[tokio::test]
    async fn a_malformed_input_is_reported_rather_than_panicking() {
        // The binary rejects a malformed shape before any request; a direct library
        // call must not crash either.
        let provider = FakeProvider::with_code(Vec::new(), Vec::new());
        let db = Database::open_in_memory().unwrap();
        let config = DiscographyConfig::default();

        let discovery = discover_artist_albums_at(
            &provider,
            &db,
            &ArtistInput::parse("000000046659702"),
            &config,
            1_700_000_000,
            FailureCacheUse::Bypass,
        )
        .await;

        match discovery.outcome {
            DiscoveryOutcome::LegacyFallback { kind, reason, .. } => {
                assert_eq!(kind, DiscoveryFailure::Unresolved);
                assert!(
                    reason.contains("15 digits"),
                    "a malformed shape must be reported as such, never searched for: {reason}"
                );
            }
            other => panic!("expected an unresolved fallback, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_identity_run_from_a_fresh_cache_keeps_the_canonical_name() {
        // A cache-served identity run must name the work item and carry the ID
        // exactly as the refresh path does.
        let provider = FakeProvider::with_code(
            vec![carrier(
                "c02a5966-b5c4-483a-8326-483edcf3680e",
                "Grafix",
                &[],
                &["0000000466597022"],
            )],
            vec![
                group(
                    "rg-prefixed",
                    "Grafix - Half Light",
                    Some("2018"),
                    Some("Album"),
                    &[],
                ),
                group("rg-bare", "Half Light", Some("2019"), Some("Album"), &[]),
            ],
        );
        let db = Database::open_in_memory().unwrap();
        let config = DiscographyConfig::default();
        let input = ArtistInput::parse("0000 0004 6659 7022");

        discover_artist_albums_at(
            &provider,
            &db,
            &input,
            &config,
            1_700_000_000,
            FailureCacheUse::Bypass,
        )
        .await;

        let cold = FakeProvider::with_code(Vec::new(), Vec::new());
        let discovery = discover_artist_albums_at(
            &cold,
            &db,
            &input,
            &config,
            1_700_000_010,
            FailureCacheUse::Bypass,
        )
        .await;

        assert!(
            cold.requested_codes().is_empty(),
            "the fresh cache must answer without a provider request"
        );
        assert_eq!(discovery.canonical_name.as_deref(), Some("Grafix"));
        assert_eq!(
            discovery.mbid.as_deref(),
            Some("c02a5966-b5c4-483a-8326-483edcf3680e")
        );
        match &discovery.outcome {
            DiscoveryOutcome::Authoritative { albums, .. } => assert_eq!(
                albums.len(),
                1,
                "a cache-served identity run must fold identities with the canonical name"
            ),
            other => panic!("expected authoritative albums, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_isni_ending_in_x_round_trips_through_its_cache_row() {
        // The cache key keeps the check character upper-cased, so a row written for
        // an `X` ISNI must still be found on the next run.
        let candidates = vec![carrier(
            "c02a5966-b5c4-483a-8326-483edcf3680e",
            "X Artist",
            &[],
            &["000000021694233X"],
        )];
        let provider = FakeProvider::with_code(
            candidates.clone(),
            vec![group(
                "rg-1",
                "Half Light",
                Some("2019"),
                Some("Album"),
                &[],
            )],
        );
        let db = Database::open_in_memory().unwrap();
        let config = DiscographyConfig::default();
        let input = ArtistInput::parse("0000 0002 1694 233X");

        discover_artist_albums_at(
            &provider,
            &db,
            &input,
            &config,
            1_700_000_000,
            FailureCacheUse::Bypass,
        )
        .await;

        let cold = FakeProvider::with_code(candidates, Vec::new());
        let discovery = discover_artist_albums_at(
            &cold,
            &db,
            &input,
            &config,
            1_700_000_010,
            FailureCacheUse::Bypass,
        )
        .await;

        assert!(
            cold.requested_codes().is_empty(),
            "the row written for an X ISNI must be found again"
        );
        assert_eq!(discovery.canonical_name.as_deref(), Some("X Artist"));
    }

    #[tokio::test]
    async fn an_identity_run_from_a_stale_cache_keeps_the_canonical_name() {
        // A failed refresh falls back to the stale row, which must still name the work
        // item and carry the ID rather than the code.
        let provider = FakeProvider::with_code(
            vec![carrier(
                "c02a5966-b5c4-483a-8326-483edcf3680e",
                "Grafix",
                &[],
                &["0000000466597022"],
            )],
            vec![group(
                "rg-bare",
                "Half Light",
                Some("2019"),
                Some("Album"),
                &[],
            )],
        );
        let db = Database::open_in_memory().unwrap();
        let config = DiscographyConfig::default();
        let input = ArtistInput::parse("0000 0004 6659 7022");
        discover_artist_albums_at(
            &provider,
            &db,
            &input,
            &config,
            1_700_000_000,
            FailureCacheUse::Bypass,
        )
        .await;

        // Re-seed the same row as stale, then let the refresh fail.
        db.upsert_discography_cache(&crate::db::DiscographyCacheEntry {
            artist_key: input.cache_key(),
            artist_mbid: "c02a5966-b5c4-483a-8326-483edcf3680e".to_string(),
            canonical_artist: "Grafix".to_string(),
            fetched_at: 1,
            release_groups_json: serde_json::to_string(&vec![group(
                "rg-bare",
                "Half Light",
                Some("2019"),
                Some("Album"),
                &[],
            )])
            .unwrap(),
        })
        .unwrap();
        let offline = FakeProvider::with_code(Vec::new(), Vec::new());
        let discovery = discover_artist_albums_at(
            &offline,
            &db,
            &input,
            &config,
            1_700_000_000,
            FailureCacheUse::Bypass,
        )
        .await;

        assert!(
            matches!(discovery.outcome, DiscoveryOutcome::Authoritative { .. }),
            "the stale cache must answer"
        );
        assert_eq!(discovery.canonical_name.as_deref(), Some("Grafix"));
        assert_eq!(
            discovery.mbid.as_deref(),
            Some("c02a5966-b5c4-483a-8326-483edcf3680e")
        );
    }
}
