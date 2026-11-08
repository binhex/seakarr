<!-- markdownlint-disable MD013 -->

# Label-Based Discovery Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use sub-agents (recommended) to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let `--label` sweep one or more record labels on a `discover` run, resolving each label on MusicBrainz, fetching every release of the wanted type, and downloading the albums the library lacks into staging.

**Architecture:** Two new modules with one job each. `src/discography/label.rs` owns label identity (name-or-MBID parse, search, safe resolution) and the cached release catalogue (browse plus pagination guards). `src/label_discover.rs` owns pure selection: merge labels, filter by type and `--since`, dedupe, order, and split present from missing against `LibraryIndex`. `src/runner.rs` gains one orchestration function that reuses `process_album` with `LibraryTarget::StagingOnly`; the library-derived artist sweep is untouched. Shared cache machinery in `src/discography/mod.rs` is widened to be reusable by both sources.

**Tech Stack:** Rust 2021, `clap` v4 (derive), `rusqlite`, `tokio`, `serde`/`serde_json`, `tracing`, `wiremock` for provider tests, `tempfile` for integration fixtures. No new dependency.

**Spec:** `docs/agent/specs/2026-11-01-label-based-discovery-design.md` (approved, commit `0ba3d99`).

**Status:** implemented and released as `v0.34.0`. The code blocks in the task list below are
working notes from before implementation; several were superseded during review (the library
scan failure aborts instead of scanning on, the circuit breaker ignores a fresh cache hit, the
cache payload carries no `label_name`, and `from_parts` re-parses so a near-miss UUID stays
malformed). `src/discography/label.rs`, `src/label_discover.rs` and the spec's
"Corrected during review" block are authoritative; do not re-derive the code from these
snippets. The task checkboxes below were used as
working notes and are left unticked; this line is the authoritative status. Released with
open review residuals recorded in
`docs/agent/reviews/2026-11-02-label-based-discovery-residuals.md`.

---

## Scope Check

Single subsystem: one crate, one new provider surface plus one selection module and one runner entry point. The parts are not separable into independent plans, because the CLI flag, the provider method, the selection rules, the cache rows and the staging placement must land together or `--label` either does nothing or does the wrong thing. **No split.**

## Corrections found during implementation

Recorded as they were found, so a later reader does not re-derive them.

1. **Task 10's tests called `config.validate()`** on `Config::default()`, which rejects an
   empty `soulseek.username`/`password` before `validate_discography()` runs, so four of
   the five tests could never pass as written. Fixed: the four validation tests call
   `config.validate_non_credential_constraints()`, which is what the sibling
   `discography.artist_mbids` tests already use and which still runs
   `validate_discography()`. The production code in Task 10 is unchanged.
2. **Task 1 asked for `ResolveKind` and a `kind` parameter**, but `LabelUnresolved` does
   not exist until Task 2, and `ResolveKind::Label` is not constructed until
   `resolve_label` lands - so the variant would be dead code and `-D warnings` would
   fail. Fixed: Task 1 keeps the helper artist-flavoured; Task 4 (same chunk as
   `resolve_label`) adds `ResolveKind` and the parameter, and the artist messages must
   stay byte-identical across that change. (The Task 1 correction note below says the
   same thing about Task 4; the earlier phrasing that named Task 3 was superseded.)
3. **Every `rustfmt --edition 2024` instruction in this plan is wrong.** The crate is
   `edition = "2021"` and HEAD is edition-2021 fmt-clean; formatting a file with the
   2024 style edition reformats it, and `cargo fmt --check` then fails. Use plain
   `cargo fmt` (which reads the edition from Cargo.toml) or `rustfmt <file>` with no
   `--edition`, and never run rustfmt on a module root such as `src/lib.rs` or
   `src/discography/mod.rs` unless you intend to format that whole subtree.
4. **`PartialDate` had to become `pub`** (fields private, `parse_partial_date` still
   `pub(crate)`): `pub fn label_targets(..., since: Option<PartialDate>, ...)` and the
   later `pub enum ExecutionPlan::Discover { since: Option<PartialDate> }` expose it in
   public signatures, which `private_interfaces` rejects under `-D warnings`. Making
   the type public once is the fix; an `#[allow(private_interfaces)]` is not.

## Plan-level clarifications against the spec

Two places where the spec's wording is refined so the types stay honest. Neither changes a behaviour the spec describes.

1. The spec sketches `releases_by_label -> Result<Vec<LabelRelease>>` and says incomplete releases are "dropped in the provider and counted by the caller". A `Vec` cannot carry that count, so the provider returns `LabelCatalogue { releases, incomplete }`. The behaviour is exactly what the spec says: counted, never guessed.
2. The spec sketches `LabelRequest { input, mbid }`. That is what is built, and `LabelInput::from_parts` reconstructs the classified input from those two fields so the runner never re-parses or re-validates a CLI string.

## File Structure

| File | Responsibility after this change |
| --- | --- |
| `src/discography/label.rs` (new) | `LabelInput`, `LabelCandidate`, `resolve_label`, `discover_label_releases` (cache + browse orchestration), `LabelRelease`, `LabelCatalogue`, `LabelCacheEntry`, `pinned_mbid`. Depends on the provider and the database; knows nothing about download or placement. |
| `src/discography/mod.rs` (modify) | Adds `pub mod label;`, the two `Label*` error variants, the shared dominance helper, the extracted failure-replay decision, `pub(crate)` cache helpers, and `PartialDate`/`parse_partial_date` visibility. |
| `src/discography/musicbrainz.rs` (modify) | `search_labels`, `label_by_id`, `releases_by_label`, their wire types and page checks. |
| `src/label_discover.rs` (new) | `LabelTarget`, `SelectionCounts`, `LabelTargets`, `label_targets`, `LabelCounters`, `label_notices`. Pure: no HTTP, no filesystem. |
| `src/config.rs` (modify) | `discography.label_mbids` plus validation. |
| `src/mode.rs` (modify) | `LabelRequest`, `parse_labels`, `parse_release_types`, `ExecutionPlan::Discover` fields, every new refusal. |
| `src/main.rs` (modify) | Three clap arguments, `CliOverrides` plumbing, dispatch to `run_label_discover_mode`. |
| `src/runner.rs` (modify) | `run_label_discover_mode` and `run_label_discover_mode_with_provider`. Artist sweep untouched. |
| `src/lib.rs` (modify) | `pub mod label_discover;` |
| `README.md` (modify) | Discover-mode feature bullet, flag table, `discography.label_mbids`. |
| `tests/pipeline_test.rs` (modify) | One end-to-end label fixture. |

Order matters. Tasks 1-2 widen shared machinery and keep the suite green. Tasks 3-6 build `label.rs` and the provider with no production caller. Tasks 7-9 build the cache path and pure selection. Tasks 10-12 wire config, validation and the CLI. Task 13 adds the runner. Task 14 documents. Task 15 proves it end to end. Task 16 runs the gate.

---

### Task 1: Widen the shared cache and dominance machinery

**Files:**

- Modify: `src/discography/mod.rs`

No behaviour change. The artist sweep's observable behaviour, including every message, must stay byte-identical: the existing suite is the guard.

- [ ] **Step 1: Run the focused baseline and record it**

```bash
cargo test --lib discography 2>&1 | tail -5
```

Expected: PASS. Note the number of passing tests; it must be unchanged (plus new ones) after the refactor.

- [ ] **Step 2: Rename the failure-cache predicate**

`DiscographyError::is_stable_artist_resolution_failure` becomes `is_stable_resolution_failure`, because a label name is now recorded in the same table. Replace the definition and every call site.

```bash
grep -rln 'is_stable_artist_resolution_failure' src/ | xargs sed -i 's/is_stable_artist_resolution_failure/is_stable_resolution_failure/g'
```

In `src/discography/mod.rs`, widen the doc sentence that names the predicate and make the match accept both cacheable variants:

```rust
    /// True when this failure means the entity could not be matched or
    /// disambiguated - by canonical name, by the IPI or ISNI the caller named, or,
    /// for a label, by name - as opposed to a transport, HTTP, decode, pagination or
    /// unusable-data failure, and is therefore worth remembering across runs.
    pub(crate) fn is_stable_resolution_failure(&self) -> bool {
        matches!(self, Self::ArtistUnresolved(_) | Self::LabelUnresolved(_))
    }
```

- [ ] **Step 3: Extract the dominance decision**

Replace the body of `resolve_dominant_artist` with a call to a shared helper. The helper returns both the winning index and the evidence, so the artist path cannot recompute a different winner.

```rust
/// The winner of the dominance rules, with the evidence that selected it.
pub(crate) struct Dominance {
    /// Index into the slice the caller passed.
    pub index: usize,
    pub evidence: DominanceEvidence,
}

/// Apply the dominance rules to two or more scored candidates.
///
/// Shared by artist and label resolution so the two cannot diverge on what
/// "resolved safely" means. The messages name only the key, so both callers
/// report identical reasons for identical provider data.
pub(crate) fn dominant_candidate_index<T>(
    key: &str,
    matches: &[&T],
    score_of: impl Fn(&T) -> Option<u8>,
    id_of: impl Fn(&T) -> &str,
) -> std::result::Result<Dominance, DiscographyError> {
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
    if matches.iter().any(|candidate| score_of(candidate).is_none()) {
        return Err(DiscographyError::InvalidCandidateData(format!(
            "{exact_matches} candidates match {key:?} and at least one has no search score"
        )));
    }
    // Both defect classes were rejected above, so every score is present and in
    // range here.
    let mut scored: Vec<(u8, usize, &str)> = matches
        .iter()
        .enumerate()
        .filter_map(|(index, candidate)| score_of(candidate).map(|score| (score, index, id_of(candidate))))
        .collect();
    debug_assert_eq!(scored.len(), exact_matches);
    // Sort by score, then MBID, so the comparison never depends on the order
    // MusicBrainz returned the candidates in.
    scored.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.2.cmp(right.2)));
    let (top_score, top_index, _) = scored[0];
    let leaders = scored.iter().filter(|(score, _, _)| *score == top_score).count();
    if leaders != 1 {
        return Err(DiscographyError::ArtistUnresolved(format!(
            "{exact_matches} candidates match {key:?} with a tied top score of {top_score}"
        )));
    }
    if top_score < REQUIRED_TOP_SCORE {
        return Err(DiscographyError::ArtistUnresolved(format!(
            "{exact_matches} candidates match {key:?} and the highest score {top_score} is below {REQUIRED_TOP_SCORE}"
        )));
    }
    let runner_up_score = scored[1].0;
    let margin = top_score - runner_up_score;
    if margin < REQUIRED_SCORE_MARGIN {
        return Err(DiscographyError::ArtistUnresolved(format!(
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
```

**Correction (found during implementation, wave 1).** Do NOT add `ResolveKind` or a
`kind` parameter here. `DiscographyError::LabelUnresolved` does not exist until Task 2,
and once it does, nothing in the crate constructs `ResolveKind::Label` until
`src/discography/label.rs` lands in Task 3 - and a `pub(crate)` variant that is never
constructed fails `cargo clippy -D warnings` with "variant `Label` is never
constructed". Suppressing that with `#[allow(dead_code)]` would leave an attribute
nobody is responsible for removing. So the helper stays artist-flavoured in this
task - its refusal arms construct `DiscographyError::ArtistUnresolved(format!(...))`
inline - and **Task 4 adds `ResolveKind` and the `kind` parameter** in the same chunk
that first constructs `ResolveKind::Label`.

Then `resolve_dominant_artist` becomes:

```rust
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
```

- [ ] **Step 4: Make the cache helpers reusable**

`src/discography/mod.rs`:

- `struct PartialDate` becomes `pub(crate) struct PartialDate`, and its fields stay private.
- `fn parse_partial_date` becomes `pub(crate) fn parse_partial_date`.
- `fn cache_is_fresh` becomes `pub(crate) fn cache_is_fresh`.
- `fn stale_age_days` becomes `pub(crate) fn stale_age_days`.
- `fn record_resolution_failure` becomes `pub(crate) fn record_resolution_failure`, and its doc gains one sentence: "The key is namespaced by the caller: `label:<key>` rows are written by the label path and replayed by it alone."
- Split `replay_recorded_failure` into a decision and an adapter:

```rust
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
        if let Err(error) = db.delete_discography_failure(key) {
            tracing::warn!("failed to clear discography failure row: {error}");
        }
        return None;
    }
    if use_cache == FailureCacheUse::Bypass || config.failure_cache_days == 0 {
        return None;
    }
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
    tracing::debug!(key, recorded_at = entry.recorded_at, "skipping a recorded resolution failure");
    Some(entry)
}
```

`replay_recorded_failure` keeps its exact current signature and return type, and becomes a thin adapter that turns the entry into the artist-shaped `Discovery` it returns today. Preserve the existing empty-reason default (`"artist could not be resolved safely"`).

- [ ] **Step 5: Add the payload size check**

Split the limit out of `serialize_cache_payload` so a label payload can be checked without being a `Vec<ReleaseGroup>`:

```rust
/// Reject a cache payload over the 16 MiB limit.
pub(crate) fn check_cache_payload_size(
    payload: &str,
) -> std::result::Result<(), DiscographyError> {
    if payload.len() > MAX_CACHE_PAYLOAD_BYTES {
        return Err(DiscographyError::Decode(format!(
            "cache payload of {} bytes exceeds the {MAX_CACHE_PAYLOAD_BYTES}-byte limit",
            payload.len()
        )));
    }
    Ok(())
}
```

`serialize_cache_payload` keeps its signature and calls `check_cache_payload_size(&payload)?` before returning.

- [ ] **Step 6: Re-run the suite and confirm nothing moved**

```bash
cargo test --lib discography 2>&1 | tail -5
cargo clippy --all-targets -- -D warnings
```

Expected: PASS, same or greater test count, no clippy warning. If any existing assertion changed, the extraction changed behaviour - revert that part rather than editing the assertion.

- [ ] **Step 7: Commit**

```bash
git add src/discography/mod.rs
git commit -m "refactor: share dominance and failure-cache machinery for label resolution"
```

---

### Task 2: Add the label error variants and classify them

**Files:**

- Modify: `src/discography/mod.rs`

- [ ] **Step 1: Write the failing tests**

Add to the `mod tests` block in `src/discography/mod.rs`:

```rust
    #[test]
    fn label_resolution_failures_are_classified_for_the_breaker_and_the_cache() {
        let unresolved = DiscographyError::LabelUnresolved("no candidate matches \"hospital\"".into());
        assert_eq!(DiscoveryFailure::from_error(&unresolved), DiscoveryFailure::Unresolved);
        assert!(unresolved.is_stable_resolution_failure());

        let inconsistent = DiscographyError::LabelSearchInconsistent("reported 2, carried 1".into());
        assert_eq!(DiscoveryFailure::from_error(&inconsistent), DiscoveryFailure::Unresolved);
        assert!(
            !inconsistent.is_stable_resolution_failure(),
            "an inconsistent search page describes one response and must not be cached"
        );

        let outage = DiscographyError::HttpStatus(503);
        assert_eq!(DiscoveryFailure::from_error(&outage), DiscoveryFailure::Provider);
        assert!(!outage.is_stable_resolution_failure());
    }
```

- [ ] **Step 2: Run it to confirm it fails**

```bash
cargo test --lib discography::tests::label_resolution_failures 2>&1 | tail -20
```

Expected: FAIL to compile with `no variant or associated item named 'LabelUnresolved'`.

- [ ] **Step 3: Add the variants**

In the `DiscographyError` enum, after `ArtistSearchInconsistent`:

```rust
    /// The label could not be resolved to exactly one MusicBrainz candidate.
    /// Recorded in the failure cache under `label:<key>`, so a name that will
    /// not resolve is not asked about again until the expiry elapses.
    #[error("label could not be resolved safely: {0}")]
    LabelUnresolved(String),
    /// A label search answered with a page whose reported total disagrees with
    /// the candidates it carried. Classified as a resolution problem for this
    /// run, like its artist equivalent, but never cached: it describes one
    /// inconsistent response rather than a property of the name.
    #[error("label search was inconsistent: {0}")]
    LabelSearchInconsistent(String),
```

Extend `DiscoveryFailure::from_error`:

```rust
            DiscographyError::ArtistUnresolved(_)
            | DiscographyError::ArtistSearchInconsistent(_)
            | DiscographyError::LabelUnresolved(_)
            | DiscographyError::LabelSearchInconsistent(_) => Self::Unresolved,
```

- [ ] **Step 4: Run it to confirm it passes**

```bash
cargo test --lib discography::tests::label_resolution_failures 2>&1 | tail -5
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/discography/mod.rs
git commit -m "feat: add label resolution error kinds"
```

---

### Task 3: `LabelInput` — classifying a `--label` value

**Files:**

- Create: `src/discography/label.rs`
- Modify: `src/discography/mod.rs` (module declaration only)

- [ ] **Step 1: Declare the module and write the failing tests**

In `src/discography/mod.rs`, beside the existing `pub mod identifier;`:

```rust
pub mod label;
```

In `src/discography/label.rs`, start with the module doc, the type, and the tests:

```rust
//! Identifying a MusicBrainz label from a `--label` value.
//!
//! A label value is a name or a MusicBrainz label UUID. The shape rule is
//! deliberately narrow: only a value that is *entirely* a UUID is treated as an
//! ID, so an ordinary label name is never reinterpreted. A label literally named
//! like a UUID is pinned with `discography.label_mbids`, which wins over the
//! input's own shape.

use std::collections::BTreeMap;

use super::identifier::{pin_key as identifier_pin_key, valid_mbid};

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
    /// Build from parts the CLI layer has already classified and validated.
    pub fn from_parts(input: String, mbid: Option<String>) -> Self {
        match mbid {
            Some(mbid) => Self::Mbid(mbid.to_ascii_lowercase()),
            None => Self::Name(input),
        }
    }
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
        assert!(input.validate().is_err(), "a short UUID must not become a name");
        assert!(
            input.malformed_reason().unwrap().contains("hyphenated like a MusicBrainz ID"),
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
            "label:hospital records"
        );
        assert_eq!(
            LabelInput::parse(HOSPITAL_LABEL_MBID).cache_key(),
            format!("label:mbid:{HOSPITAL_LABEL_MBID}")
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
        pins.insert("hospital  records".to_string(), HOSPITAL_LABEL_MBID.to_string());

        let by_name = LabelInput::parse("Hospital Records");
        assert_eq!(pinned_mbid(&pins, &by_name), Some(HOSPITAL_LABEL_MBID));

        let by_id = LabelInput::parse(HOSPITAL_LABEL_MBID);
        assert_eq!(pinned_mbid(&pins, &by_id), None);
    }
}
```

- [ ] **Step 2: Run to confirm it fails**

```bash
cargo test --lib discography::label 2>&1 | tail -20
```

Expected: FAIL to compile - `parse`, `display`, `mbid`, `validate`, `malformed_reason`, `cache_key`, `pinned_mbid` do not exist.

- [ ] **Step 3: Implement it**

Add above the `#[cfg(test)]` block:

```rust
impl LabelInput {
    /// Classify a `--label` value by its shape.
    ///
    /// Total: anything that is not a UUID stays a name, so classification never
    /// drops a label. A UUID-shaped value that is not a usable UUID is reported
    /// by [`LabelInput::validate`] instead of being searched for as a name.
    pub fn parse(raw: &str) -> Self {
        let trimmed = raw.trim();
        if valid_mbid(trimmed) {
            return Self::Mbid(trimmed.to_ascii_lowercase());
        }
        if looks_like_mbid(trimmed) {
            return Self::Malformed {
                value: raw.to_string(),
                reason: format!(
                    "label {raw:?} is hyphenated like a MusicBrainz ID but is not one \
                     (8-4-4-4-12 hexadecimal digits)"
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

    /// Reject a value that claims to be an ID but cannot be used as one.
    pub fn validate(&self) -> std::result::Result<(), String> {
        match self {
            Self::Name(value) if value.trim().is_empty() => {
                Err(format!("label {value:?} is blank"))
            }
            Self::Name(_) | Self::Mbid(_) => Ok(()),
            Self::Malformed { reason, .. } => Err(reason.clone()),
        }
    }

    /// The key for the catalogue and failure caches.
    ///
    /// Prefixed twice so a name key and an ID key cannot collide, and so the
    /// label space cannot collide with an artist key in the same table.
    pub fn cache_key(&self) -> String {
        format!("label:{}", self.pin_key())
    }

    /// The key `discography.label_mbids` is matched on.
    ///
    /// A UUID input has no name to pin, so it pins itself.
    fn pin_key(&self) -> String {
        match self {
            Self::Mbid(value) => format!("mbid:{value}"),
            Self::Name(value) | Self::Malformed { value, .. } => identifier_pin_key(value),
        }
    }
}

/// The MBID pinned for this input's name, when one is configured.
///
/// A pin is keyed by label name, so a UUID input never matches - it is already
/// exact.
pub fn pinned_mbid<'a>(
    pins: &'a BTreeMap<String, String>,
    input: &LabelInput,
) -> Option<&'a str> {
    let LabelInput::Name(name) = input else {
        return None;
    };
    let key = identifier_pin_key(name);
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
```

- [ ] **Step 4: Run to confirm it passes**

```bash
cargo test --lib discography::label 2>&1 | tail -5
```

Expected: PASS, 7 tests.

- [ ] **Step 5: Commit**

```bash
git add src/discography/label.rs src/discography/mod.rs
git commit -m "feat: classify a --label value as a name or a MusicBrainz label ID"
```

---

### Task 4: Resolve a label safely

**Files:**

- Modify: `src/discography/label.rs`

- [ ] **Step 1: Write the failing tests**

Append inside `mod tests`:

```rust
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
        assert_eq!(resolved.id, candidates[1].id, "the score decides, not the response order");
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
        let candidates = vec![candidate(1, "Hospital Records Limited", Some(88), "UK drum & bass")];
        let error = resolve_label("Hospital Records", &candidates).unwrap_err();
        let DiscographyError::LabelUnresolved(reason) = &error else {
            panic!("expected LabelUnresolved, got {error:?}");
        };
        assert!(reason.contains("no MusicBrainz label matches"), "got {reason}");
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
        assert!(!reason.contains("Label 10"), "the eleventh is not: {reason}");
    }
```

- [ ] **Step 2: Run to confirm it fails**

```bash
cargo test --lib discography::label 2>&1 | tail -20
```

Expected: FAIL to compile - `LabelCandidate` and `resolve_label` do not exist.

- [ ] **Step 3: Parameterise the shared dominance helper**

In `src/discography/mod.rs`, add the `ResolveKind` enum and give
`dominant_candidate_index` a `kind` parameter. This belongs in this task, not Task 1:
`ResolveKind::Label` is first constructed by `resolve_label` below, so a variant added
earlier would be dead code under `-D warnings`.

```rust
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
```

Then replace the three inline `DiscographyError::ArtistUnresolved(format!(...))`
constructions in `dominant_candidate_index` with `kind.unresolved(format!(...))`, and
pass `ResolveKind::Artist` from `resolve_dominant_artist`.

Run `cargo test --lib discography` here: every existing artist message must be
unchanged.

- [ ] **Step 4: Implement the label types and resolution**

Add above the `#[cfg(test)]` block:

```rust
use super::{dominant_candidate_index, normalize_catalog_key, DiscographyError, ResolveKind};

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
            )?;
            Ok((*many[dominance.index]).clone())
        }
    }
}

/// A bounded, human answer to "which label did you mean?".
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
        format!("{} ({})", candidate.name, detail.join(", "))
    }
}
```

- [ ] **Step 5: Run to confirm it passes**

```bash
cargo test --lib discography 2>&1 | tail -5
cargo clippy --all-targets -- -D warnings
```

Expected: PASS, 13 label tests plus the unchanged artist suite, and no clippy warning
(the `Label` arm is now constructed, so nothing is dead).

- [ ] **Step 6: Commit**

```bash
git add src/discography/label.rs
git commit -m "feat: resolve a label name with the shared dominance rules"
```

---

### Task 5: Provider label search and lookup

**Files:**

- Modify: `src/discography/musicbrainz.rs`
- Modify: `src/discography/mod.rs` (trait methods)
- Modify: `src/discography/label.rs` (no change expected; the wire type converts into `LabelCandidate`)
- Modify: `src/runner.rs` and `src/discography/mod.rs` test doubles

- [ ] **Step 1: Add the trait methods**

In `src/discography/mod.rs`, extend `DiscographyProvider` after `artist_by_id`:

```rust
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
```

Both are required rather than defaulted: a provider that quietly returned nothing would make an unimplemented label path look like a label with no releases.

- [ ] **Step 2: Write the failing provider tests**

Add to `mod tests` in `src/discography/musicbrainz.rs`:

```rust
    #[tokio::test]
    async fn label_search_carries_the_score_code_and_comment() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ws/2/label"))
            .and(query_param("query", "label:\"Hospital Records\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "count": 2,
                "offset": 0,
                "labels": [
                    {
                        "id": "8562598d-60d1-4bd6-8b85-4d709e951770",
                        "name": "Hospital Records",
                        "score": 100,
                        "label-code": 12939,
                        "disambiguation": "UK drum & bass"
                    },
                    {
                        "id": "c8471d13-3574-4040-be20-229656068504",
                        "name": "Hospital Records",
                        "score": 62,
                        "disambiguation": "US noisecore label"
                    }
                ]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let provider = MusicBrainzProvider::for_test(server.uri(), Duration::ZERO).unwrap();
        let labels = DiscographyProvider::search_labels(&provider, "Hospital Records")
            .await
            .unwrap();

        assert_eq!(labels.len(), 2);
        assert_eq!(labels[0].score, Some(100));
        assert_eq!(labels[0].label_code, Some(12939));
        assert_eq!(labels[0].disambiguation.as_deref(), Some("UK drum & bass"));
        assert_eq!(labels[1].label_code, None);
    }

    #[tokio::test]
    async fn label_search_escapes_quotes_in_the_name() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ws/2/label"))
            .and(query_param("query", "label:\"The \\\"A\\\" Label\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "count": 0,
                "offset": 0,
                "labels": []
            })))
            .expect(1)
            .mount(&server)
            .await;

        let provider = MusicBrainzProvider::for_test(server.uri(), Duration::ZERO).unwrap();
        let labels = DiscographyProvider::search_labels(&provider, "The \"A\" Label")
            .await
            .unwrap();
        assert!(labels.is_empty());
    }

    #[tokio::test]
    async fn a_label_page_that_disagrees_with_its_count_is_inconsistent() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ws/2/label"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "count": 2,
                "offset": 0,
                "labels": [{ "id": "8562598d-60d1-4bd6-8b85-4d709e951770", "name": "Hospital Records" }]
            })))
            .mount(&server)
            .await;

        let provider = MusicBrainzProvider::for_test(server.uri(), Duration::ZERO).unwrap();
        let error = DiscographyProvider::search_labels(&provider, "Hospital Records")
            .await
            .unwrap_err();
        assert!(
            matches!(error, DiscographyError::LabelSearchInconsistent(_)),
            "got {error:?}"
        );
    }

    #[tokio::test]
    async fn a_label_lookup_checks_the_returned_id() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ws/2/label/8562598d-60d1-4bd6-8b85-4d709e951770"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "c8471d13-3574-4040-be20-229656068504",
                "name": "Hospital Records"
            })))
            .mount(&server)
            .await;

        let provider = MusicBrainzProvider::for_test(server.uri(), Duration::ZERO).unwrap();
        let error = DiscographyProvider::label_by_id(&provider, "8562598d-60d1-4bd6-8b85-4d709e951770")
            .await
            .unwrap_err();
        assert!(matches!(error, DiscographyError::Decode(_)), "got {error:?}");
    }
```

- [ ] **Step 3: Run to confirm it fails**

```bash
cargo test --lib discography::musicbrainz::tests::label 2>&1 | tail -20
```

Expected: FAIL to compile - the trait methods are declared but not implemented.

- [ ] **Step 4: Implement the wire types and the two methods**

In `src/discography/musicbrainz.rs`, after `ReleaseGroupWire`:

```rust
#[derive(Debug, Deserialize)]
struct LabelPage {
    count: usize,
    offset: usize,
    #[serde(default)]
    labels: Vec<LabelWire>,
}

#[derive(Debug, Deserialize)]
struct LabelWire {
    id: String,
    name: String,
    #[serde(default, deserialize_with = "deserialize_optional_score")]
    score: Option<u8>,
    #[serde(rename = "label-code", default)]
    label_code: Option<u32>,
    #[serde(default)]
    disambiguation: Option<String>,
}

impl From<LabelWire> for super::label::LabelCandidate {
    fn from(wire: LabelWire) -> Self {
        Self {
            id: wire.id,
            name: wire.name,
            score: wire.score,
            label_code: wire.label_code,
            disambiguation: wire.disambiguation,
        }
    }
}

/// Reject a label search page that cannot be trusted, exactly as
/// [`check_artist_page`] does for artists.
fn check_label_page(page: &LabelPage) -> std::result::Result<(), DiscographyError> {
    if page.offset != 0 {
        return Err(DiscographyError::IncompletePagination(format!(
            "label search returned a non-zero offset {}",
            page.offset
        )));
    }
    if page.count != page.labels.len() {
        return Err(DiscographyError::LabelSearchInconsistent(format!(
            "label search returned {} candidates but reported {}",
            page.labels.len(),
            page.count
        )));
    }
    Ok(())
}
```

In `impl DiscographyProvider for MusicBrainzProvider`, after `artist_by_id`:

```rust
    async fn search_labels(
        &self,
        label: &str,
    ) -> std::result::Result<Vec<super::label::LabelCandidate>, DiscographyError> {
        let escaped = label.replace('\\', "\\\\").replace('"', "\\\"");
        let page: LabelPage = self
            .request_json(
                "/ws/2/label",
                &[
                    ("query", format!("label:\"{escaped}\"")),
                    ("fmt", "json".into()),
                    ("limit", PAGE_SIZE.to_string()),
                ],
            )
            .await?;
        check_label_page(&page)?;
        Ok(page
            .labels
            .into_iter()
            .map(super::label::LabelCandidate::from)
            .collect())
    }

    async fn label_by_id(
        &self,
        label_mbid: &str,
    ) -> std::result::Result<super::label::LabelCandidate, DiscographyError> {
        let wire: LabelWire = self
            .request_json(
                &format!("/ws/2/label/{label_mbid}"),
                &[("fmt", "json".into())],
            )
            .await?;
        if !wire.id.eq_ignore_ascii_case(label_mbid) {
            return Err(DiscographyError::Decode(format!(
                "label id {} does not match the requested MBID {label_mbid}",
                wire.id
            )));
        }
        // The lookup endpoint carries no search relevance score and needs none.
        Ok(super::label::LabelCandidate::from(wire))
    }
```

- [ ] **Step 5: Extend the two test doubles**

`src/runner.rs`, inside `impl DiscographyProvider for FakeDiscographyProvider`, add:

```rust
        async fn search_labels(
            &self,
            _label: &str,
        ) -> std::result::Result<Vec<crate::discography::label::LabelCandidate>, DiscographyError>
        {
            panic!("artist-sweep tests never query labels")
        }

        async fn label_by_id(
            &self,
            _label_mbid: &str,
        ) -> std::result::Result<crate::discography::label::LabelCandidate, DiscographyError>
        {
            panic!("artist-sweep tests never query labels")
        }
```

`src/discography/mod.rs`, inside `impl DiscographyProvider for FakeProvider`, add the same two methods with the same panics.

- [ ] **Step 6: Run to confirm it passes**

```bash
cargo test --lib discography 2>&1 | tail -5
cargo clippy --all-targets -- -D warnings
```

Expected: PASS, no warning.

- [ ] **Step 7: Commit**

```bash
git add src/discography/musicbrainz.rs src/discography/mod.rs src/runner.rs
git commit -m "feat: search and look up a MusicBrainz label"
```

---

### Task 6: Fetch a label's release catalogue

**Files:**

- Modify: `src/discography/musicbrainz.rs`
- Modify: `src/discography/mod.rs` (trait method)
- Modify: `src/discography/label.rs` (`LabelRelease`, `LabelCatalogue`)

- [ ] **Step 1: Define the types and write the failing tests**

In `src/discography/label.rs`, above the `#[cfg(test)]` block:

```rust
use super::ReleaseGroup;

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
    /// Releases whose payload carried no release group or no artist credit.
    /// Counted, never guessed: an attributed guess would download the wrong album.
    pub incomplete: usize,
}
```

`ReleaseGroup` already derives `Serialize`/`Deserialize`, so `LabelRelease` can be cached.

Add to `mod tests` in `src/discography/musicbrainz.rs`:

```rust
    fn release_payload(count: usize, offset: usize, length: usize) -> serde_json::Value {
        let releases: Vec<serde_json::Value> = (0..length)
            .map(|index| {
                json!({
                    "id": format!("release-{}", offset + index),
                    "title": format!("Album {}", offset + index),
                    "status": "Official",
                    "release-group": {
                        "id": format!("group-{}", offset + index),
                        "title": format!("Album {}", offset + index),
                        "first-release-date": "2004-05-10",
                        "primary-type": "Album",
                        "secondary-types": []
                    },
                    "artist-credit": [
                        { "name": "Nu:Tone", "joinphrase": " feat. " },
                        { "name": "Stamina MC", "joinphrase": "" }
                    ]
                })
            })
            .collect();
        json!({
            "release-count": count,
            "release-offset": offset,
            "releases": releases
        })
    }

    #[tokio::test]
    async fn releases_by_label_paginate_with_official_status() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ws/2/release"))
            .and(query_param("label", "8562598d-60d1-4bd6-8b85-4d709e951770"))
            .and(query_param("status", "official"))
            .and(query_param("inc", "release-groups+artist-credits"))
            .and(query_param("offset", "0"))
            .respond_with(ResponseTemplate::new(200).set_body_json(release_payload(101, 0, 100)))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/ws/2/release"))
            .and(query_param("offset", "100"))
            .respond_with(ResponseTemplate::new(200).set_body_json(release_payload(101, 100, 1)))
            .expect(1)
            .mount(&server)
            .await;

        let provider = MusicBrainzProvider::for_test(server.uri(), Duration::ZERO).unwrap();
        let catalogue = DiscographyProvider::releases_by_label(
            &provider,
            "8562598d-60d1-4bd6-8b85-4d709e951770",
        )
        .await
        .unwrap();

        assert_eq!(catalogue.releases.len(), 101);
        assert_eq!(catalogue.incomplete, 0);
        assert_eq!(catalogue.releases[0].artist_credit, "Nu:Tone feat. Stamina MC");
        assert_eq!(catalogue.releases[0].release_group.primary_type.as_deref(), Some("Album"));
    }

    #[tokio::test]
    async fn a_release_without_a_release_group_or_credit_is_counted_not_guessed() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ws/2/release"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "release-count": 3,
                "release-offset": 0,
                "releases": [
                    {
                        "id": "release-1",
                        "release-group": { "id": "group-1", "title": "Album 1" },
                        "artist-credit": [{ "name": "Artist", "joinphrase": "" }]
                    },
                    { "id": "release-2" },
                    {
                        "id": "release-3",
                        "release-group": { "id": "group-3", "title": "Album 3" },
                        "artist-credit": []
                    }
                ]
            })))
            .mount(&server)
            .await;

        let provider = MusicBrainzProvider::for_test(server.uri(), Duration::ZERO).unwrap();
        let catalogue = DiscographyProvider::releases_by_label(
            &provider,
            "8562598d-60d1-4bd6-8b85-4d709e951770",
        )
        .await
        .unwrap();

        assert_eq!(catalogue.releases.len(), 1);
        assert_eq!(catalogue.incomplete, 2);
    }

    #[tokio::test]
    async fn a_short_release_page_is_incomplete_pagination() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ws/2/release"))
            .respond_with(ResponseTemplate::new(200).set_body_json(release_payload(101, 0, 1)))
            .mount(&server)
            .await;

        let provider = MusicBrainzProvider::for_test(server.uri(), Duration::ZERO).unwrap();
        let error = DiscographyProvider::releases_by_label(
            &provider,
            "8562598d-60d1-4bd6-8b85-4d709e951770",
        )
        .await
        .unwrap_err();
        assert!(matches!(error, DiscographyError::IncompletePagination(_)), "got {error:?}");
    }

    #[tokio::test]
    async fn a_duplicate_release_id_is_incomplete_pagination() {
        let server = MockServer::start().await;
        let mut page = release_payload(2, 0, 1);
        page["releases"] = json!([
            {
                "id": "release-1",
                "release-group": { "id": "group-1", "title": "Album 1" },
                "artist-credit": [{ "name": "Artist", "joinphrase": "" }]
            },
            {
                "id": "release-1",
                "release-group": { "id": "group-2", "title": "Album 2" },
                "artist-credit": [{ "name": "Artist", "joinphrase": "" }]
            }
        ]);
        Mock::given(method("GET"))
            .and(path("/ws/2/release"))
            .respond_with(ResponseTemplate::new(200).set_body_json(page))
            .mount(&server)
            .await;

        let provider = MusicBrainzProvider::for_test(server.uri(), Duration::ZERO).unwrap();
        let error = DiscographyProvider::releases_by_label(
            &provider,
            "8562598d-60d1-4bd6-8b85-4d709e951770",
        )
        .await
        .unwrap_err();
        let DiscographyError::IncompletePagination(reason) = &error else {
            panic!("expected IncompletePagination, got {error:?}");
        };
        assert!(reason.contains("duplicate"), "got {reason}");
    }
```

- [ ] **Step 2: Run to confirm it fails**

```bash
cargo test --lib discography::musicbrainz::tests::release 2>&1 | tail -20
```

Expected: FAIL to compile - `releases_by_label` is not a member of the trait.

- [ ] **Step 3: Add the trait method**

In `src/discography/mod.rs`:

```rust
    /// Every official release on a label, with its release group inline.
    ///
    /// Release groups cannot be browsed by label, so the catalogue is built from
    /// releases and collapsed to release groups by the selector.
    async fn releases_by_label(
        &self,
        label_mbid: &str,
    ) -> std::result::Result<super::label::LabelCatalogue, DiscographyError>;
```

- [ ] **Step 4: Implement it**

In `src/discography/musicbrainz.rs`, add near `MAX_RELEASE_GROUPS`:

```rust
/// Releases one label sweep will page through. A label above this ceiling is
/// reported rather than silently truncated: a partial catalogue looks exactly
/// like "nothing new".
const MAX_LABEL_RELEASES: usize = PAGE_SIZE * 50;
```

Add the wire types after `ArtistCreditWire` (created in this step):

```rust
#[derive(Debug, Deserialize)]
struct ReleasePage {
    #[serde(rename = "release-count")]
    count: usize,
    #[serde(rename = "release-offset")]
    offset: usize,
    #[serde(default)]
    releases: Vec<ReleaseWire>,
}

#[derive(Debug, Deserialize)]
struct ReleaseWire {
    id: String,
    #[serde(rename = "release-group", default)]
    release_group: Option<ReleaseGroupWire>,
    #[serde(rename = "artist-credit", default)]
    artist_credit: Vec<ArtistCreditWire>,
}

#[derive(Debug, Deserialize)]
struct ArtistCreditWire {
    #[serde(default)]
    name: String,
    #[serde(default)]
    joinphrase: Option<String>,
}

/// The full credit string MusicBrainz spells for a release.
///
/// Names and joinphrases are concatenated rather than re-derived, so
/// `Nu:Tone feat. Stamina MC` is reproduced exactly.
fn credit_string(credits: &[ArtistCreditWire]) -> String {
    let mut credit = String::new();
    for part in credits {
        credit.push_str(part.name.trim());
        if let Some(join) = part.joinphrase.as_deref() {
            credit.push_str(join);
        }
    }
    credit.trim().to_string()
}

impl super::label::LabelRelease {
    /// Build from a wire release, or report that the payload cannot describe it.
    fn from_wire(wire: ReleaseWire) -> Option<Self> {
        let release_group = wire.release_group?;
        let artist_credit = credit_string(&wire.artist_credit);
        if artist_credit.is_empty() {
            return None;
        }
        Some(Self {
            release_id: wire.id,
            release_group: release_group.into(),
            artist_credit,
        })
    }
}
```

`ReleaseGroupWire` already exists in this file, so `from_wire` converts it with the existing `impl From<ReleaseGroupWire> for ReleaseGroup`.

Then in `impl DiscographyProvider`:

```rust
    async fn releases_by_label(
        &self,
        label_mbid: &str,
    ) -> std::result::Result<super::label::LabelCatalogue, DiscographyError> {
        let mut query = vec![
            ("label", label_mbid.to_string()),
            ("fmt", "json".into()),
            ("limit", PAGE_SIZE.to_string()),
            ("offset", "0".into()),
            ("status", "official".into()),
            ("inc", "release-groups+artist-credits".into()),
        ];
        let first: ReleasePage = self.request_json("/ws/2/release", &query).await?;
        if first.count > MAX_LABEL_RELEASES {
            return Err(DiscographyError::IncompletePagination(format!(
                "count {} exceeds the {MAX_LABEL_RELEASES} release limit",
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
        if first.releases.len() != expected_first_len {
            return Err(DiscographyError::IncompletePagination(format!(
                "first page returned {} of the expected {expected_first_len} releases",
                first.releases.len()
            )));
        }

        let mut releases: Vec<super::label::LabelRelease> = Vec::with_capacity(first.count);
        // Duplicate detection runs on the raw payload, before an incomplete
        // release is dropped: otherwise a dropped release could hide a duplicate.
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut incomplete = 0usize;
        let mut collect = |page: Vec<ReleaseWire>,
                           releases: &mut Vec<super::label::LabelRelease>,
                           incomplete: &mut usize|
         -> std::result::Result<(), DiscographyError> {
            for wire in page {
                if !seen.insert(wire.id.clone()) {
                    return Err(DiscographyError::IncompletePagination(
                        "release pages contain duplicate IDs".into(),
                    ));
                }
                match super::label::LabelRelease::from_wire(wire) {
                    Some(release) => releases.push(release),
                    None => *incomplete += 1,
                }
            }
            Ok(())
        };

        let count = first.count;
        collect(first.releases, &mut releases, &mut incomplete)?;
        let mut offset = PAGE_SIZE;
        while seen.len() < count {
            let expected_page_len = (count - offset).min(PAGE_SIZE);
            query[3] = ("offset", offset.to_string());
            let page: ReleasePage = self.request_json("/ws/2/release", &query).await?;
            if page.count != count {
                return Err(DiscographyError::IncompletePagination(format!(
                    "page count {} changed from {count}",
                    page.count
                )));
            }
            if page.offset != offset {
                return Err(DiscographyError::IncompletePagination(format!(
                    "page offset {} does not advance to requested offset {offset}",
                    page.offset
                )));
            }
            if page.releases.len() != expected_page_len {
                return Err(DiscographyError::IncompletePagination(format!(
                    "offset {offset} returned {} of the expected {expected_page_len} releases",
                    page.releases.len()
                )));
            }
            collect(page.releases, &mut releases, &mut incomplete)?;
            offset += PAGE_SIZE;
        }
        if seen.len() != count {
            return Err(DiscographyError::IncompletePagination(format!(
                "collected {} of {} releases",
                seen.len(),
                count
            )));
        }
        Ok(super::label::LabelCatalogue {
            releases,
            incomplete,
        })
    }
```

The closure borrows `seen` immutably through `insert` while `releases` is passed as a parameter - this compiles because `seen` is captured by mutable reference and `releases`/`incomplete` are parameters, and the closure is called sequentially. If the borrow checker objects, replace the closure with a local `fn` that takes `&mut HashSet<String>` as an explicit parameter; the logic is identical.

- [ ] **Step 5: Extend the two test doubles again**

Add to both `FakeDiscographyProvider` (runner tests) and `FakeProvider` (mod.rs tests):

```rust
        async fn releases_by_label(
            &self,
            _label_mbid: &str,
        ) -> std::result::Result<crate::discography::label::LabelCatalogue, DiscographyError>
        {
            panic!("artist-sweep tests never query a label catalogue")
        }
```

- [ ] **Step 6: Run to confirm it passes**

```bash
cargo test --lib discography 2>&1 | tail -5
cargo clippy --all-targets -- -D warnings
```

Expected: PASS, no warning.

- [ ] **Step 7: Commit**

```bash
git add src/discography/label.rs src/discography/musicbrainz.rs src/discography/mod.rs src/runner.rs
git commit -m "feat: page a label's official release catalogue with pagination guards"
```

---

### Task 7: Cache the catalogue and replay recorded failures

**Files:**

- Modify: `src/discography/label.rs`

This is where a label sweep becomes cheap on repeat runs and cannot be wedged by a stale row.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `src/discography/label.rs`:

```rust
    use super::super::{FailureCacheUse, SECONDS_PER_DAY};
    use crate::config::{DiscographyConfig, DiscographyReleaseType};
    use crate::db::Database;
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
        failure: Option<DiscographyError>,
        search_calls: AtomicUsize,
        lookup_calls: AtomicUsize,
        catalogue_calls: AtomicUsize,
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
            })
        }

        fn failing(reason: &str) -> Arc<Self> {
            Arc::new(Self {
                candidates: Vec::new(),
                catalogue: LabelCatalogue { releases: Vec::new(), incomplete: 0 },
                failure: Some(DiscographyError::Transport(reason.to_string())),
                search_calls: AtomicUsize::new(0),
                lookup_calls: AtomicUsize::new(0),
                catalogue_calls: AtomicUsize::new(0),
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
            if let Some(error) = &self.failure {
                return Err(clone_error(error));
            }
            Ok(self.candidates.clone())
        }

        async fn label_by_id(
            &self,
            _label_mbid: &str,
        ) -> std::result::Result<LabelCandidate, DiscographyError> {
            self.lookup_calls.fetch_add(1, Ordering::SeqCst);
            if let Some(error) = &self.failure {
                return Err(clone_error(error));
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
            if let Some(error) = &self.failure {
                return Err(clone_error(error));
            }
            Ok(self.catalogue.clone())
        }
    }

    /// `DiscographyError` is not `Clone`, and the fake needs to answer the same
    /// failure repeatedly, so each variant is rebuilt.
    fn clone_error(error: &DiscographyError) -> DiscographyError {
        match error {
            DiscographyError::Transport(reason) => DiscographyError::Transport(reason.clone()),
            DiscographyError::HttpStatus(status) => DiscographyError::HttpStatus(*status),
            DiscographyError::ProviderError { status, detail } => DiscographyError::ProviderError {
                status: *status,
                detail: detail.clone(),
            },
            DiscographyError::Decode(reason) => DiscographyError::Decode(reason.clone()),
            DiscographyError::IncompletePagination(reason) => {
                DiscographyError::IncompletePagination(reason.clone())
            }
            DiscographyError::LabelUnresolved(reason) => {
                DiscographyError::LabelUnresolved(reason.clone())
            }
            DiscographyError::LabelSearchInconsistent(reason) => {
                DiscographyError::LabelSearchInconsistent(reason.clone())
            }
            other => DiscographyError::Transport(format!("{other}")),
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

        let first = discover_label_releases_at(&*provider, &db, &input, &config(), NOW, FailureCacheUse::Honour).await;
        let LabelDiscoveryOutcome::Catalogue { provenance, releases, .. } = first.outcome else {
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
    async fn a_stale_row_is_refreshed() {
        let db = Database::open_in_memory().unwrap();
        let provider = FakeLabelProvider::new(vec![hospital()]);
        let input = LabelInput::parse("Hospital Records");

        discover_label_releases_at(&*provider, &db, &input, &config(), NOW, FailureCacheUse::Honour).await;
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
        discover_label_releases_at(&*provider, &db, &input, &config(), NOW, FailureCacheUse::Honour).await;

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
        let LabelDiscoveryOutcome::Catalogue { provenance, releases, .. } = stale.outcome else {
            panic!("expected the stale catalogue, got {:?}", stale.outcome);
        };
        assert_eq!(releases.len(), 1, "the stale catalogue is still usable");
        let super::super::DiscoveryProvenance::StaleCache { age_days, kind, .. } = provenance else {
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
        });
        let input = LabelInput::parse("Hospital Records");

        let first = discover_label_releases_at(&*provider, &db, &input, &config(), NOW, FailureCacheUse::Honour).await;
        assert!(
            matches!(first.outcome, LabelDiscoveryOutcome::Failed { kind: super::super::DiscoveryFailure::Unresolved, from_cache: false, .. }),
            "got {:?}",
            first.outcome
        );

        let second = discover_label_releases_at(&*provider, &db, &input, &config(), NOW + 60, FailureCacheUse::Honour).await;
        assert!(
            matches!(second.outcome, LabelDiscoveryOutcome::Failed { kind: super::super::DiscoveryFailure::Unresolved, from_cache: true, .. }),
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
        pinned.label_mbids.insert("Hospital Records".into(), LABEL_ID.into());

        let discovery = discover_label_releases_at(&*provider, &db, &input, &pinned, NOW, FailureCacheUse::Honour).await;
        assert!(matches!(discovery.outcome, LabelDiscoveryOutcome::Catalogue { .. }));
        assert_eq!(provider.lookup_calls.load(Ordering::SeqCst), 1);
        assert_eq!(provider.search_calls.load(Ordering::SeqCst), 0, "a pin never searches");
    }

    #[tokio::test]
    async fn a_changed_pin_refetches_an_incompatible_row() {
        let db = Database::open_in_memory().unwrap();
        let provider = FakeLabelProvider::new(vec![hospital()]);
        let input = LabelInput::parse("Hospital Records");

        discover_label_releases_at(&*provider, &db, &input, &config(), NOW, FailureCacheUse::Honour).await;

        let mut pinned = config();
        pinned
            .label_mbids
            .insert("Hospital Records".into(), "c8471d13-3574-4040-be20-229656068504".into());
        let discovery = discover_label_releases_at(&*provider, &db, &input, &pinned, NOW + 60, FailureCacheUse::Honour).await;
        let LabelDiscoveryOutcome::Catalogue { provenance, .. } = discovery.outcome else {
            panic!("expected a catalogue, got {:?}", discovery.outcome);
        };
        assert_eq!(provenance, super::super::DiscoveryProvenance::Refreshed);
        assert_eq!(provider.lookup_calls.load(Ordering::SeqCst), 1);
    }
```

Note: `LabelDiscoveryOutcome` is `Debug`, so the `panic!("{:?}")` messages work. `DiscographyConfig` must gain `label_mbids` first - Task 10 does that; this task's tests are written against it, so **swap the order**: do Task 10 (config) before this task if implementing strictly left to right. The plan keeps this order because Task 10 is tiny; if the tests do not compile for the missing field, add the field first as Task 10 describes.

- [ ] **Step 2: Run to confirm it fails**

```bash
cargo test --lib discography::label::tests 2>&1 | tail -20
```

Expected: FAIL to compile - `discover_label_releases_at`, `LabelDiscovery`, `LabelDiscoveryOutcome` do not exist.

- [ ] **Step 3: Implement the cache entry and the orchestrator**

Add above `#[cfg(test)]` in `src/discography/label.rs`:

```rust
use super::{
    cache_is_fresh, check_cache_payload_size, identifier, record_resolution_failure,
    replayable_failure, stale_age_days, DiscoveryFailure, DiscoveryProvenance,
    DiscographyConfig, FailureCacheUse, DiscographyProvider,
};
use crate::db::{Database, DiscographyCacheEntry};

/// Version of the cached payload. A row with another version is refetched, so a
/// future field change cannot be read as if it were this one.
pub const LABEL_CACHE_VERSION: u32 = 1;

/// What `discography_cache` holds under a `label:` key.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LabelCacheEntry {
    pub version: u32,
    pub label_name: String,
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
/// freshly recorded resolution failure unless bypassed, then MusicBrainz, then a
/// compatible stale cache, and finally a typed failure.
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
    let label_key = input.cache_key();
    let configured_mbid = pinned_mbid(&config.label_mbids, input);
    let cached = load_cached_catalogue(db, &label_key, configured_mbid);

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
                    tracing::warn!("failed to write label catalogue cache for {label_key:?}: {reason}");
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

/// Read a compatible, well-formed cache row. Incompatible, corrupt and
/// wrong-version rows are warned about (corrupt ones deleted) and treated as a
/// miss, so a cache can never block a lookup it was not written for.
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
    if identifier::pin_key(&entry.artist_key) != label_key {
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
            let _ = db.delete_discography_cache(label_key);
            None
        }
        Err(error) => {
            tracing::warn!("label catalogue cache for {label_key:?} is corrupt ({error}); deleting");
            let _ = db.delete_discography_cache(label_key);
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
        label_name: candidate.name.clone(),
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
```

`DiscoveryProvenance` and `DiscoveryFailure` already derive `Debug, Clone, PartialEq, Eq`, so `LabelDiscovery` can derive the same.

- [ ] **Step 4: Run to confirm it passes**

```bash
cargo test --lib discography::label 2>&1 | tail -5
```

Expected: PASS. The six cache tests plus the earlier ones.

- [ ] **Step 5: Commit**

```bash
git add src/discography/label.rs src/config.rs
git commit -m "feat: cache a label catalogue and replay recorded label failures"
```

---

### Task 8: Pure selection — label releases into download targets

**Files:**

- Create: `src/label_discover.rs`
- Modify: `src/lib.rs`

- [ ] **Step 1: Declare the module and write the failing tests**

`src/lib.rs`, beside the other `pub mod` lines:

```rust
pub mod label_discover;
```

`src/label_discover.rs`:

```rust
//! Pure selection for a label sweep: a label's release groups become download
//! targets.
//!
//! Nothing here performs I/O. The provider layer fetches and caches; the runner
//! sequences and downloads; this module decides what is wanted, in what order,
//! and how many of each rejection reason were seen.

use std::collections::{BTreeMap, BTreeSet};

use crate::config::DiscographyReleaseType;
use crate::discography::label::LabelRelease;
use crate::discography::{
    normalize_catalog_key, parse_partial_date, release_allowed, select_albums, AlbumTarget,
    PartialDate, ReleaseGroup,
};
use crate::search::album_identity_key;

/// One album to download, with the artist credit it is searched under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelTarget {
    pub artist: String,
    pub target: AlbumTarget,
}

/// How many releases each filter removed.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SelectionCounts {
    pub releases_examined: usize,
    pub not_wanted_type: usize,
    pub since_skipped: usize,
    pub undated_skipped: usize,
    pub excluded_credits: usize,
    pub eligible: usize,
}

/// The work list plus the counts that explain it.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct LabelTargets {
    pub targets: Vec<LabelTarget>,
    pub counts: SelectionCounts,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group(id: &str, title: &str, date: Option<&str>, primary: &str, secondary: &[&str]) -> ReleaseGroup {
        ReleaseGroup {
            id: id.to_string(),
            title: title.to_string(),
            first_release_date: date.map(str::to_string),
            primary_type: Some(primary.to_string()),
            secondary_types: secondary.iter().map(|value| value.to_string()).collect(),
        }
    }

    fn release(id: &str, credit: &str, group: ReleaseGroup) -> LabelRelease {
        LabelRelease {
            release_id: format!("release-{id}"),
            release_group: group,
            artist_credit: credit.to_string(),
        }
    }

    fn studio() -> Vec<DiscographyReleaseType> {
        vec![DiscographyReleaseType::StudioAlbum]
    }

    fn nothing_excluded() -> Vec<String> {
        Vec::new()
    }

    fn bound(value: &str) -> PartialDate {
        parse_partial_date(value).expect("test bound must parse")
    }

    #[test]
    fn a_studio_album_becomes_one_target_under_its_credit() {
        let releases = vec![release(
            "1",
            "London Elektricity",
            group("g1", "Syncopated City", Some("2008-06-02"), "Album", &[]),
        )];
        let selected = label_targets(&releases, &studio(), None, &nothing_excluded());
        assert_eq!(selected.targets.len(), 1);
        assert_eq!(selected.targets[0].artist, "London Elektricity");
        assert_eq!(selected.targets[0].target.title, "Syncopated City");
        assert_eq!(selected.counts.eligible, 1);
    }

    #[test]
    fn releases_of_another_type_are_counted_not_selected() {
        let releases = vec![
            release("1", "A", group("g1", "Album", Some("2001"), "Album", &[])),
            release("2", "A", group("g2", "Single", Some("2002"), "Single", &[])),
            release("3", "A", group("g3", "Live Thing", Some("2003"), "Album", &["Live"])),
        ];
        let selected = label_targets(&releases, &studio(), None, &nothing_excluded());
        assert_eq!(selected.targets.len(), 1);
        assert_eq!(selected.counts.not_wanted_type, 2);
    }

    #[test]
    fn an_ep_is_selected_only_when_it_is_wanted() {
        let releases = vec![release("1", "A", group("g1", "An EP", Some("2001"), "EP", &[]))];
        assert!(label_targets(&releases, &studio(), None, &nothing_excluded()).targets.is_empty());
        let ep_only = vec![DiscographyReleaseType::Ep];
        assert_eq!(label_targets(&releases, &ep_only, None, &nothing_excluded()).targets.len(), 1);
    }

    #[test]
    fn the_since_bound_is_inclusive_and_counts_what_it_drops() {
        let releases = vec![
            release("1", "A", group("g1", "Before", Some("1999-12-31"), "Album", &[])),
            release("2", "A", group("g2", "On The Bound", Some("2000-01-01"), "Album", &[])),
            release("3", "A", group("g3", "After", Some("2001"), "Album", &[])),
        ];
        let selected = label_targets(&releases, &studio(), Some(bound("2000-01-01")), &nothing_excluded());
        let titles: Vec<&str> = selected
            .targets
            .iter()
            .map(|target| target.target.title.as_str())
            .collect();
        assert_eq!(titles, vec!["On The Bound", "After"]);
        assert_eq!(selected.counts.since_skipped, 1);
    }

    #[test]
    fn an_undated_release_cannot_satisfy_a_lower_bound() {
        let releases = vec![
            release("1", "A", group("g1", "Undated", None, "Album", &[])),
            release("2", "A", group("g2", "Dated", Some("2025"), "Album", &[])),
        ];
        let selected = label_targets(&releases, &studio(), Some(bound("2024")), &nothing_excluded());
        assert_eq!(selected.targets.len(), 1);
        assert_eq!(selected.targets[0].target.title, "Dated");
        assert_eq!(selected.counts.undated_skipped, 1);
        assert_eq!(selected.counts.since_skipped, 0);
    }

    #[test]
    fn without_a_bound_an_undated_release_is_selected_last() {
        let releases = vec![
            release("1", "A", group("g1", "Undated", None, "Album", &[])),
            release("2", "A", group("g2", "Dated", Some("2025"), "Album", &[])),
        ];
        let selected = label_targets(&releases, &studio(), None, &nothing_excluded());
        let titles: Vec<&str> = selected.targets.iter().map(|t| t.target.title.as_str()).collect();
        assert_eq!(titles, vec!["Dated", "Undated"]);
        assert_eq!(selected.counts.undated_skipped, 0);
    }

    #[test]
    fn excluded_credits_are_dropped_and_counted() {
        let releases = vec![
            release("1", "Various Artists", group("g1", "Compilation", Some("2001"), "Album", &[])),
            release("2", "VA", group("g2", "Another", Some("2002"), "Album", &[])),
            release("3", "Real Artist", group("g3", "Album", Some("2003"), "Album", &[])),
        ];
        let excluded = vec!["Various Artists".to_string(), "VA".to_string()];
        let selected = label_targets(&releases, &studio(), None, &excluded);
        assert_eq!(selected.targets.len(), 1);
        assert_eq!(selected.counts.excluded_credits, 2);
    }

    #[test]
    fn a_collaboration_keeps_its_full_credit_string() {
        let releases = vec![release(
            "1",
            "Nu:Tone feat. Stamina MC",
            group("g1", "Words 2 Believe In", Some("2007"), "Album", &[]),
        )];
        let selected = label_targets(&releases, &studio(), None, &nothing_excluded());
        assert_eq!(selected.targets[0].artist, "Nu:Tone feat. Stamina MC");
    }

    #[test]
    fn the_same_album_on_two_labels_collapses_to_one_target() {
        let releases = vec![
            release("1", "A", group("g1", "Album", Some("2001"), "Album", &[])),
            release("2", "A", group("g2", "Album", Some("2001"), "Album", &[])),
        ];
        let selected = label_targets(&releases, &studio(), None, &nothing_excluded());
        assert_eq!(selected.targets.len(), 1);
    }

    #[test]
    fn ordering_is_by_artist_then_date_then_title() {
        let releases = vec![
            release("1", "Zed", group("g1", "Later", Some("2010"), "Album", &[])),
            release("2", "Abc", group("g2", "Second", Some("2005"), "Album", &[])),
            release("3", "Abc", group("g3", "First", Some("2001"), "Album", &[])),
            release("4", "Abc", group("g4", "Also First", Some("2001"), "Album", &[])),
        ];
        let selected = label_targets(&releases, &studio(), None, &nothing_excluded());
        let ordered: Vec<(&str, &str)> = selected
            .targets
            .iter()
            .map(|target| (target.artist.as_str(), target.target.title.as_str()))
            .collect();
        assert_eq!(
            ordered,
            vec![
                ("Abc", "Also First"),
                ("Abc", "First"),
                ("Abc", "Second"),
                ("Zed", "Later"),
            ]
        );
    }

    #[test]
    fn the_same_input_in_another_order_selects_the_same_work_list() {
        let releases = vec![
            release("1", "A", group("g1", "One", Some("2001"), "Album", &[])),
            release("2", "B", group("g2", "Two", Some("2002"), "Album", &[])),
            release("3", "A", group("g3", "Three", Some("2003"), "Album", &[])),
        ];
        let mut shuffled = releases.clone();
        shuffled.reverse();
        let first = label_targets(&releases, &studio(), None, &nothing_excluded());
        let second = label_targets(&shuffled, &studio(), None, &nothing_excluded());
        assert_eq!(first, second);
    }

    #[test]
    fn an_empty_catalogue_selects_nothing_and_says_so() {
        let selected = label_targets(&[], &studio(), None, &nothing_excluded());
        assert!(selected.targets.is_empty());
        assert_eq!(selected.counts.releases_examined, 0);
        assert_eq!(selected.counts.eligible, 0);
    }
}
```

- [ ] **Step 2: Run to confirm it fails**

```bash
cargo test --lib label_discover 2>&1 | tail -20
```

Expected: FAIL to compile - `label_targets` does not exist.

- [ ] **Step 3: Implement it**

Add above the `#[cfg(test)]` block:

```rust
/// Turn a label's release groups into the albums this run should download.
///
/// Rejections are counted per release, so the summary can explain a catalogue
/// that produced no work. Ordering is total and deterministic - artist key, then
/// the existing partial-date order (dated before undated, earliest first), then
/// normalised title, then release-group id - which is what makes a
/// budget-limited sweep converge instead of oscillating.
pub fn label_targets(
    releases: &[LabelRelease],
    allowed: &[DiscographyReleaseType],
    since: Option<PartialDate>,
    exclude_artists: &[String],
) -> LabelTargets {
    let mut counts = SelectionCounts {
        releases_examined: releases.len(),
        ..SelectionCounts::default()
    };
    let excluded: BTreeSet<String> = exclude_artists
        .iter()
        .map(|name| normalize_catalog_key(name))
        .filter(|key| !key.is_empty())
        .collect();

    // Group by the credit a release was issued under, and remember each release
    // group's earliest date: `select_albums` returns targets without dates, and
    // the date is part of the ordering contract.
    let mut groups: BTreeMap<String, (String, Vec<ReleaseGroup>)> = BTreeMap::new();
    let mut dates: BTreeMap<String, Option<PartialDate>> = BTreeMap::new();

    for release in releases {
        let credit_key = normalize_catalog_key(&release.artist_credit);
        if credit_key.is_empty() || excluded.contains(&credit_key) {
            counts.excluded_credits += 1;
            continue;
        }
        if !release_allowed(&release.release_group, allowed) {
            counts.not_wanted_type += 1;
            continue;
        }
        let date = release
            .release_group
            .first_release_date
            .as_deref()
            .and_then(parse_partial_date);
        if let Some(bound) = since {
            match date {
                Some(date) if date >= bound => {}
                Some(_) => {
                    counts.since_skipped += 1;
                    continue;
                }
                // An undated release group cannot prove it satisfies a lower
                // bound, so it is excluded rather than assumed recent.
                None => {
                    counts.undated_skipped += 1;
                    continue;
                }
            }
        }
        dates
            .entry(release.release_group.id.clone())
            .and_modify(|slot| {
                if let Some(candidate) = date {
                    *slot = Some(match *slot {
                        Some(existing) => existing.min(candidate),
                        None => candidate,
                    });
                }
            })
            .or_insert(date);
        groups
            .entry(credit_key)
            .or_insert_with(|| (release.artist_credit.clone(), Vec::new()))
            .1
            .push(release.release_group.clone());
    }

    // `select_albums` is reused per credit group rather than reimplemented, so
    // bundle decomposition, artist-prefix stripping and the album-identity
    // collapse behave exactly as they do in the artist sweep.
    let mut merged: Vec<(String, Option<PartialDate>, LabelTarget)> = Vec::new();
    let mut seen: BTreeSet<(String, String)> = BTreeSet::new();
    for (credit_key, (credit, groups)) in groups {
        for target in select_albums(&groups, allowed, &credit) {
            let identity = album_identity_key(&target.title, &credit);
            if !seen.insert((credit_key.clone(), identity)) {
                continue;
            }
            let date = dates.get(&target.release_group_id).copied().flatten();
            merged.push((
                credit_key.clone(),
                date,
                LabelTarget {
                    artist: credit.clone(),
                    target,
                },
            ));
        }
    }

    merged.sort_by(|left, right| {
        left.0
            .cmp(&right.0)
            .then_with(|| match (left.1, right.1) {
                (Some(a), Some(b)) => a.cmp(&b),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            })
            .then_with(|| {
                normalize_catalog_key(&left.2.target.title)
                    .cmp(&normalize_catalog_key(&right.2.target.title))
            })
            .then_with(|| {
                left.2
                    .target
                    .release_group_id
                    .cmp(&right.2.target.release_group_id)
            })
    });

    counts.eligible = merged.len();
    LabelTargets {
        targets: merged.into_iter().map(|(_, _, target)| target).collect(),
        counts,
    }
}
```

- [ ] **Step 4: Run to confirm it passes**

```bash
cargo test --lib label_discover 2>&1 | tail -5
```

Expected: PASS, 12 tests. If `ordering_is_by_artist_then_date_then_title` fails on `"Also First"` vs `"First"`, the tie-break is normalised title order (`also first` < `first`), which is the expected outcome - do not change the assertion.

- [ ] **Step 5: Commit**

```bash
git add src/label_discover.rs src/lib.rs
git commit -m "feat: select label releases into download targets"
```

---

### Task 9: Label run counters and notices

**Files:**

- Modify: `src/label_discover.rs`

- [ ] **Step 1: Write the failing tests**

Append inside `mod tests`:

```rust
    fn counters() -> LabelCounters {
        LabelCounters {
            requested: vec!["Hospital Records".into()],
            budget_limit: 5,
            ..LabelCounters::default()
        }
    }

    #[test]
    fn a_quiet_run_says_what_it_resolved_and_where_the_albums_went() {
        let mut run = counters();
        run.resolved.push(("Hospital Records".into(), "8562598d-60d1-4bd6-8b85-4d709e951770".into()));
        run.selection.releases_examined = 12;
        run.selection.eligible = 4;
        run.present = 1;
        run.staged_albums = 3;

        let notices = label_notices(&run, "downloads/staging");
        assert!(notices[0].contains("resolved 1 label"), "{notices:?}");
        assert!(notices[0].contains("Hospital Records"), "{notices:?}");
        assert!(notices.iter().any(|notice| notice.contains("12 release(s) examined")), "{notices:?}");
        assert!(notices.iter().any(|notice| notice.contains("1 album(s) already present")), "{notices:?}");
        assert!(notices.iter().any(|notice| notice.contains("3 album(s) staged")), "{notices:?}");
        assert!(
            notices.iter().any(|notice| notice.contains("downloads/staging")),
            "the closing line must name the staging root: {notices:?}"
        );
    }

    #[test]
    fn unresolved_and_failed_labels_are_reported_separately() {
        let mut run = counters();
        run.unresolved.push("Hospital Records".into());
        run.cached_failures.push("Med School".into());
        run.provider_failed.push(("Spearhead".into(), "MusicBrainz returned HTTP 503".into()));

        let notices = label_notices(&run, "downloads/staging");
        assert!(notices.iter().any(|notice| notice.contains("1 label(s) unresolved")), "{notices:?}");
        assert!(notices.iter().any(|notice| notice.contains("Med School")), "{notices:?}");
        assert!(notices.iter().any(|notice| notice.contains("provider failed for Spearhead")), "{notices:?}");
    }

    #[test]
    fn the_since_and_type_counts_are_reported_when_they_explain_nothing_eligible() {
        let mut run = counters();
        run.selection.releases_examined = 30;
        run.selection.not_wanted_type = 20;
        run.selection.since_skipped = 5;
        run.selection.undated_skipped = 3;
        run.selection.excluded_credits = 2;

        let notices = label_notices(&run, "downloads/staging");
        assert!(notices.iter().any(|notice| notice.contains("20 not of the wanted type")), "{notices:?}");
        assert!(notices.iter().any(|notice| notice.contains("5 release(s) excluded by --since")), "{notices:?}");
        assert!(notices.iter().any(|notice| notice.contains("3 undated")), "{notices:?}");
        assert!(notices.iter().any(|notice| notice.contains("2 release(s) skipped: excluded artist credit")), "{notices:?}");
    }

    #[test]
    fn the_budget_stop_names_the_artist_it_stopped_on() {
        let mut run = counters();
        run.budget_reached_at = Some("London Elektricity".into());
        let notices = label_notices(&run, "downloads/staging");
        assert!(
            notices.iter().any(|notice| notice.contains("5 download(s)") && notice.contains("London Elektricity")),
            "{notices:?}"
        );
    }

    #[test]
    fn incomplete_releases_are_named() {
        let mut run = counters();
        run.incomplete = 2;
        let notices = label_notices(&run, "downloads/staging");
        assert!(
            notices.iter().any(|notice| notice.contains("2 release(s) skipped: incomplete MusicBrainz data")),
            "{notices:?}"
        );
    }
```

- [ ] **Step 2: Run to confirm it fails**

```bash
cargo test --lib label_discover 2>&1 | tail -20
```

Expected: FAIL to compile - `LabelCounters` and `label_notices` do not exist.

- [ ] **Step 3: Implement them**

Add above the `#[cfg(test)]` block:

```rust
/// Counters accumulated across one label run, used to build the summary.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct LabelCounters {
    /// The `--label` values as given, in order.
    pub requested: Vec<String>,
    /// Labels resolved this run, as `(name, mbid)` in encounter order.
    pub resolved: Vec<(String, String)>,
    /// Labels MusicBrainz could not resolve, in encounter order.
    pub unresolved: Vec<String>,
    /// Labels skipped because a previous run recorded a resolution failure.
    pub cached_failures: Vec<String>,
    /// Labels whose request failed, with the reason.
    pub provider_failed: Vec<(String, String)>,
    /// Labels actually examined before the run stopped.
    pub labels_examined: usize,
    /// What the selection filters removed and kept.
    pub selection: SelectionCounts,
    /// Releases whose payload carried no release group or artist credit.
    pub incomplete: usize,
    /// Albums the library already holds.
    pub present: usize,
    /// Albums downloaded and left in staging, counted from the outcome.
    pub staged_albums: usize,
    /// Albums that failed, with the reason recorded on the report.
    pub failed_albums: usize,
    /// The artist the run stopped at because the budget was spent.
    pub budget_reached_at: Option<String>,
    /// The configured budget limit, echoed in the notice.
    pub budget_limit: u32,
}

/// Build the aggregate notices for a finished label run, in spec order.
///
/// Pure: the caller records the returned strings on its `RunReport`.
pub fn label_notices(counters: &LabelCounters, staging_dir: &str) -> Vec<String> {
    let mut notices = Vec::new();

    if !counters.resolved.is_empty() {
        let listed: Vec<String> = counters
            .resolved
            .iter()
            .map(|(name, mbid)| format!("{name} ({mbid})"))
            .collect();
        notices.push(format!(
            "label: resolved {} label(s): {}",
            counters.resolved.len(),
            listed.join(", ")
        ));
    }
    if !counters.unresolved.is_empty() {
        notices.push(format!(
            "label: {} label(s) unresolved: {}",
            counters.unresolved.len(),
            counters.unresolved.join(", ")
        ));
    }
    if !counters.cached_failures.is_empty() {
        notices.push(format!(
            "label: {} label(s) skipped from a recorded resolution failure: {}",
            counters.cached_failures.len(),
            counters.cached_failures.join(", ")
        ));
    }
    for (label, reason) in &counters.provider_failed {
        notices.push(format!("label: provider failed for {label} ({reason}); label skipped"));
    }
    if counters.selection.releases_examined > 0 {
        notices.push(format!(
            "label: {} release(s) examined; {} not of the wanted type",
            counters.selection.releases_examined, counters.selection.not_wanted_type
        ));
    }
    if counters.incomplete > 0 {
        notices.push(format!(
            "label: {} release(s) skipped: incomplete MusicBrainz data",
            counters.incomplete
        ));
    }
    if counters.selection.since_skipped > 0 || counters.selection.undated_skipped > 0 {
        notices.push(format!(
            "label: {} release(s) excluded by --since ({} undated)",
            counters.selection.since_skipped, counters.selection.undated_skipped
        ));
    }
    if counters.selection.excluded_credits > 0 {
        notices.push(format!(
            "label: {} release(s) skipped: excluded artist credit",
            counters.selection.excluded_credits
        ));
    }
    if counters.present > 0 {
        notices.push(format!(
            "label: {} album(s) already present; skipped",
            counters.present
        ));
    }
    notices.push(format!(
        "label: {} album(s) eligible",
        counters.selection.eligible
    ));
    notices.push(format!(
        "label: {} album(s) staged; {} failed",
        counters.staged_albums, counters.failed_albums
    ));
    if let Some(artist) = &counters.budget_reached_at {
        notices.push(format!(
            "label: budget of {} download(s) reached at {artist}; the rest is left for a later run",
            counters.budget_limit
        ));
    }
    notices.push(format!(
        "label: staged albums were not written to the library; move them from {staging_dir}"
    ));
    notices
}
```

- [ ] **Step 4: Run to confirm it passes**

```bash
cargo test --lib label_discover 2>&1 | tail -5
```

Expected: PASS, 17 tests.

- [ ] **Step 5: Commit**

```bash
git add src/label_discover.rs
git commit -m "feat: report a label run's counters as run notices"
```

---

### Task 10: `discography.label_mbids`

**Files:**

- Modify: `src/config.rs`

- [ ] **Step 1: Write the failing tests**

In `mod tests` in `src/config.rs`:

```rust
    #[test]
    fn label_mbids_defaults_to_empty() {
        let config = Config::default();
        assert!(config.discography.label_mbids.is_empty());
    }

    #[test]
    fn label_mbids_accepts_a_valid_pin() {
        let mut config = Config::default();
        config.discography.label_mbids.insert(
            "Hospital Records".into(),
            "8562598d-60d1-4bd6-8b85-4d709e951770".into(),
        );
        assert!(config.validate().is_ok());
    }

    #[test]
    fn label_mbids_rejects_an_invalid_mbid() {
        let mut config = Config::default();
        config
            .discography
            .label_mbids
            .insert("Hospital Records".into(), "not-a-uuid".into());
        let error = config.validate().expect_err("an invalid MBID must be refused");
        assert!(
            error.to_string().contains("discography.label_mbids contains an invalid MusicBrainz ID"),
            "got {error}"
        );
    }

    #[test]
    fn label_mbids_rejects_an_empty_key() {
        let mut config = Config::default();
        config.discography.label_mbids.insert(
            "   ".into(),
            "8562598d-60d1-4bd6-8b85-4d709e951770".into(),
        );
        let error = config.validate().expect_err("a blank key must be refused");
        assert!(
            error.to_string().contains("discography.label_mbids contains an empty label key"),
            "got {error}"
        );
    }

    #[test]
    fn label_mbids_rejects_two_keys_that_normalise_alike() {
        let mut config = Config::default();
        config.discography.label_mbids.insert(
            "Hospital Records".into(),
            "8562598d-60d1-4bd6-8b85-4d709e951770".into(),
        );
        config.discography.label_mbids.insert(
            "hospital  records".into(),
            "c8471d13-3574-4040-be20-229656068504".into(),
        );
        let error = config.validate().expect_err("a duplicate key must be refused");
        assert!(
            error.to_string().contains("empty or duplicate normalized label key"),
            "got {error}"
        );
    }
```

- [ ] **Step 2: Run to confirm it fails**

```bash
cargo test --lib config::tests::label_mbids 2>&1 | tail -20
```

Expected: FAIL to compile - `label_mbids` does not exist on `DiscographyConfig`.

- [ ] **Step 3: Implement it**

In the `DiscographyConfig` struct, after `artist_mbids`:

```rust
    /// Pinned label name to MusicBrainz label ID. A pin wins over the input's own
    /// shape and over search, so an ambiguous label name resolves deterministically.
    #[serde(default)]
    pub label_mbids: BTreeMap<String, String>,
```

In `impl Default for DiscographyConfig`, add `label_mbids: BTreeMap::new(),`.

In `fn validate_discography(&self)` (after the `artist_mbids` loop):

```rust
        let mut seen_labels: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        for (label, mbid) in &self.discography.label_mbids {
            let key = crate::discography::identifier::pin_key(label);
            if key.is_empty() || !seen_labels.insert(key) {
                return Err(SeakarrError::Config(format!(
                    "discography.label_mbids contains an empty or duplicate normalized label key: {label:?}"
                )));
            }
            if !crate::discography::identifier::valid_mbid(mbid) {
                return Err(SeakarrError::Config(format!(
                    "discography.label_mbids contains an invalid MusicBrainz ID for {label:?}"
                )));
            }
        }
```

- [ ] **Step 4: Run to confirm it passes**

```bash
cargo test --lib config 2>&1 | tail -5
```

Expected: PASS, including the existing `discography_defaults_are_authoritative_studio_albums` and the `mirrors!` test (both sides default to an empty map).

- [ ] **Step 5: Re-run the label cache tests that depend on this field**

```bash
cargo test --lib discography::label 2>&1 | tail -5
```

Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/config.rs
git commit -m "feat: pin a label name to a MusicBrainz label ID"
```

---

### Task 11: Execution plan — `--label`, `--release-type`, `--since`

**Files:**

- Modify: `src/mode.rs`

- [ ] **Step 1: Write the failing tests**

In `mod tests` in `src/mode.rs`, add the two parsing helpers to the existing `use super::{...}` list:

```rust
    use super::{
        configured_mode_conflict, parse_labels, parse_release_types, resolve_execution_plan,
        ExecutionPlan, SearchMode,
    };
```

Then extend the test module with a second constructor so the existing 30-odd call sites stay untouched:

```rust
    fn label_cli(labels: &[&str], release_types: &[&str], since: Option<&str>) -> CliOverrides {
        CliOverrides {
            mode: Some("discover".into()),
            label: Some(labels.iter().map(|value| value.to_string()).collect()),
            release_type: (!release_types.is_empty())
                .then(|| release_types.iter().map(|value| value.to_string()).collect()),
            since: since.map(str::to_owned),
            ..CliOverrides::default()
        }
    }
```

Then add:

```rust
    #[test]
    fn a_label_plan_carries_its_labels_types_and_bound() {
        let config = config_with_mode("discover");
        let plan = resolve_execution_plan(&config, &label_cli(&["Hospital Records,Med School"], &[], None))
            .expect("a label sweep is a valid discover plan");
        let ExecutionPlan::Discover { labels, release_types, since, artist } = plan else {
            panic!("expected a discover plan");
        };
        assert_eq!(labels.len(), 1, "the value delimiter is clap's job; mode sees one element here");
        assert_eq!(labels[0].input, "Hospital Records,Med School");
        assert!(release_types.is_none());
        assert!(since.is_none());
        assert!(artist.is_none());
    }

    #[test]
    fn several_labels_keep_their_order() {
        let config = config_with_mode("discover");
        let plan = resolve_execution_plan(
            &config,
            &label_cli(&["Hospital Records", "Med School"], &[], None),
        )
        .unwrap();
        let ExecutionPlan::Discover { labels, .. } = plan else {
            panic!("expected a discover plan");
        };
        assert_eq!(
            labels.iter().map(|label| label.input.as_str()).collect::<Vec<_>>(),
            vec!["Hospital Records", "Med School"]
        );
    }

    #[test]
    fn an_mbid_label_is_recognised_as_an_id() {
        let config = config_with_mode("discover");
        let plan = resolve_execution_plan(
            &config,
            &label_cli(&["8562598d-60d1-4bd6-8b85-4d709e951770"], &[], None),
        )
        .unwrap();
        let ExecutionPlan::Discover { labels, .. } = plan else {
            panic!("expected a discover plan");
        };
        assert_eq!(
            labels[0].mbid.as_deref(),
            Some("8562598d-60d1-4bd6-8b85-4d709e951770")
        );
    }

    #[test]
    fn a_uuid_shaped_typo_in_label_is_a_config_error() {
        let config = config_with_mode("discover");
        assert_config_error(
            &config,
            &label_cli(&["8562598d-60d1-4bd6-8b85-4d709e95177"], &[], None),
            "hyphenated like a MusicBrainz ID",
        );
    }

    #[test]
    fn a_blank_label_is_refused() {
        let config = config_with_mode("discover");
        assert_config_error(&config, &label_cli(&["   "], &[], None), "is blank");
    }

    #[test]
    fn the_same_label_twice_is_refused() {
        let config = config_with_mode("discover");
        assert_config_error(
            &config,
            &label_cli(&["Hospital Records", "hospital  records"], &[], None),
            "is given more than once",
        );
    }

    #[test]
    fn label_and_artist_are_mutually_exclusive() {
        let config = config_with_mode("discover");
        let overrides = CliOverrides {
            mode: Some("discover".into()),
            artist: Some("London Elektricity".into()),
            label: Some(vec!["Hospital Records".into()]),
            ..CliOverrides::default()
        };
        assert_config_error(&config, &overrides, "mutually exclusive");
    }

    #[test]
    fn label_outside_discover_mode_is_refused() {
        for mode in ["upgrade", "manual", "batch"] {
            let config = config_with_mode(mode);
            let overrides = CliOverrides {
                label: Some(vec!["Hospital Records".into()]),
                ..CliOverrides::default()
            };
            assert_config_error(&config, &overrides, "only valid in discover mode");
        }
    }

    #[test]
    fn release_type_without_label_is_refused() {
        let config = config_with_mode("discover");
        assert_config_error(
            &config,
            &label_cli(&[], &["album"], None),
            "--release-type is only valid with --label",
        );
    }

    #[test]
    fn since_without_label_is_refused() {
        let config = config_with_mode("discover");
        assert_config_error(
            &config,
            &label_cli(&[], &[], Some("2025-01-01")),
            "--since is only valid with --label",
        );
    }

    #[test]
    fn the_album_alias_maps_to_studio_album() {
        let config = config_with_mode("discover");
        let plan = resolve_execution_plan(
            &config,
            &label_cli(&["Hospital Records"], &["album,ep"], None),
        )
        .unwrap();
        let ExecutionPlan::Discover { release_types, .. } = plan else {
            panic!("expected a discover plan");
        };
        assert_eq!(
            release_types,
            Some(vec![
                crate::config::DiscographyReleaseType::StudioAlbum,
                crate::config::DiscographyReleaseType::Ep,
            ])
        );
    }

    #[test]
    fn release_type_parsing_rejects_an_empty_list() {
        let error = parse_release_types(&[]).expect_err("an empty list must be refused");
        assert!(
            error.to_string().contains("needs at least one value"),
            "got {error}"
        );
    }

    #[test]
    fn an_unknown_release_type_lists_the_accepted_set() {
        let config = config_with_mode("discover");
        assert_config_error(
            &config,
            &label_cli(&["Hospital Records"], &["bogus"], None),
            "accepted: album (studio_album), live_album, ep, single, compilation, remix, soundtrack, dj_mix, mixtape",
        );
    }

    #[test]
    fn since_accepts_a_year_month_and_day_and_rejects_anything_else() {
        let config = config_with_mode("discover");
        for value in ["2025", "2025-01", "2025-01-31"] {
            let plan = resolve_execution_plan(&config, &label_cli(&["Hospital Records"], &[], Some(value)))
                .expect("a valid bound must be accepted");
            let ExecutionPlan::Discover { since, .. } = plan else {
                panic!("expected a discover plan");
            };
            assert!(since.is_some(), "{value} must parse");
        }
        assert_config_error(
            &config,
            &label_cli(&["Hospital Records"], &[], Some("31-01-2025")),
            "--since",
        );
        assert_config_error(
            &config,
            &label_cli(&["Hospital Records"], &[], Some("2025-02-30")),
            "--since",
        );
    }
```

Also update the three existing literals that build a discover plan, in `src/mode.rs` and one in `src/main.rs`, to the new shape:

```rust
        assert_eq!(
            plan,
            ExecutionPlan::Discover {
                artist: None,
                labels: Vec::new(),
                release_types: None,
                since: None,
            }
        );
```

`src/main.rs:1166` similarly becomes:

```rust
        let plan = ExecutionPlan::Discover {
            artist: None,
            labels: Vec::new(),
            release_types: None,
            since: None,
        };
```

- [ ] **Step 2: Run to confirm it fails**

```bash
cargo test --lib mode 2>&1 | tail -20
```

Expected: FAIL to compile - `CliOverrides` has no `label`, `release_type` or `since`; `ExecutionPlan::Discover` has no `labels`.

- [ ] **Step 3: Add the override fields**

`src/config.rs`, in `struct CliOverrides`, after `album`:

```rust
    /// `--label` values as given, for a discover label sweep. Runtime only: not
    /// merged into `Config`, because a label run is a per-invocation choice.
    pub label: Option<Vec<String>>,
    /// `--release-type` values as given. Runtime only, like `label`.
    pub release_type: Option<Vec<String>>,
    /// `--since` as given. Runtime only, like `label`.
    pub since: Option<String>,
```

`merge_cli` deliberately ignores all three; add a comment where the other runtime-only fields are noted.

- [ ] **Step 4: Add the plan fields and the parsing helpers**

`src/mode.rs`, replace the `Discover` variant:

```rust
    /// Gap filling: artists come from the library, optionally narrowed to one, or
    /// from the labels named on the command line.
    Discover {
        /// The one explicitly named artist, when the operator named one.
        artist: Option<String>,
        /// Labels to sweep. Empty means the library-derived artist sweep.
        labels: Vec<LabelRequest>,
        /// Per-run override of `discography.allowed_types`.
        release_types: Option<Vec<DiscographyReleaseType>>,
        /// Per-run lower bound on a release group's first-release-date.
        since: Option<PartialDate>,
    },
```

Add the imports at the top of `src/mode.rs`:

```rust
use crate::config::{CliOverrides, Config, DiscographyReleaseType};
use crate::discography::label::LabelInput;
use crate::discography::PartialDate;
use crate::error::{Result, SeakarrError};
```

Add the type and helpers:

```rust
/// One `--label` element, already classified and validated.
///
/// The runner rebuilds the classified input from these two fields with
/// [`LabelInput::from_parts`], so it never re-parses or re-validates a CLI string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelRequest {
    /// The value as given, trimmed.
    pub input: String,
    /// The MusicBrainz ID, when the value was one.
    pub mbid: Option<String>,
}

/// Parse, validate and de-duplicate the `--label` values.
pub fn parse_labels(values: &[String]) -> Result<Vec<LabelRequest>> {
    let mut requests = Vec::with_capacity(values.len());
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for raw in values {
        let parsed = LabelInput::parse(raw);
        parsed.validate().map_err(SeakarrError::Config)?;
        if !seen.insert(parsed.cache_key()) {
            return Err(SeakarrError::Config(format!(
                "--label {raw:?} is given more than once"
            )));
        }
        requests.push(LabelRequest {
            input: raw.trim().to_string(),
            mbid: parsed.mbid().map(str::to_string),
        });
    }
    Ok(requests)
}

/// Parse the `--release-type` values into the enum the classifier uses.
pub fn parse_release_types(values: &[String]) -> Result<Vec<DiscographyReleaseType>> {
    let mut parsed = Vec::with_capacity(values.len());
    for value in values {
        let kind = match value.trim().to_ascii_lowercase().as_str() {
            // `album` is the operator's word for what the classifier calls a
            // studio album: an `Album` primary type with no secondary types.
            "album" | "studio_album" => DiscographyReleaseType::StudioAlbum,
            "live_album" => DiscographyReleaseType::LiveAlbum,
            "ep" => DiscographyReleaseType::Ep,
            "single" => DiscographyReleaseType::Single,
            "compilation" => DiscographyReleaseType::Compilation,
            "remix" => DiscographyReleaseType::Remix,
            "soundtrack" => DiscographyReleaseType::Soundtrack,
            "dj_mix" => DiscographyReleaseType::DjMix,
            "mixtape" => DiscographyReleaseType::Mixtape,
            other => {
                return Err(SeakarrError::Config(format!(
                    "--release-type {other:?} is not a known release type; accepted: album \
                     (studio_album), live_album, ep, single, compilation, remix, soundtrack, \
                     dj_mix, mixtape"
                )));
            }
        };
        parsed.push(kind);
    }
    if parsed.is_empty() {
        return Err(SeakarrError::Config(
            "--release-type needs at least one value".into(),
        ));
    }
    Ok(parsed)
}
```

- [ ] **Step 5: Validate in the discover arm**

In `SearchMode::Discover`, after the existing `--artist` checks and before `Ok(ExecutionPlan::Discover ...)`:

```rust
            let labels = match cli.label.as_deref() {
                Some(values) if !values.is_empty() => parse_labels(values)?,
                _ => Vec::new(),
            };
            let release_types = match cli.release_type.as_deref() {
                Some(values) if !values.is_empty() => Some(parse_release_types(values)?),
                _ => None,
            };
            let since = match cli.since.as_deref() {
                Some(value) => {
                    let bound = crate::discography::parse_partial_date(value).ok_or_else(|| {
                        SeakarrError::Config(format!(
                            "--since {value:?} is not a date; use YYYY, YYYY-MM or YYYY-MM-DD"
                        ))
                    })?;
                    Some(bound)
                }
                None => None,
            };
            if !labels.is_empty() && cli_artist.is_some() {
                return Err(SeakarrError::Config(
                    "--artist and --label are mutually exclusive; run one source per invocation"
                        .into(),
                ));
            }
            if labels.is_empty() {
                if release_types.is_some() {
                    return Err(SeakarrError::Config(
                        "--release-type is only valid with --label".into(),
                    ));
                }
                if since.is_some() {
                    return Err(SeakarrError::Config(
                        "--since is only valid with --label".into(),
                    ));
                }
            }
            Ok(ExecutionPlan::Discover {
                artist: cli_artist,
                labels,
                release_types,
                since,
            })
```

- [ ] **Step 6: Refuse the label selectors in the other three modes**

Next to the existing `has_batch_cli_selector` binding:

```rust
    let has_label_selector = cli.label.as_deref().is_some_and(|values| !values.is_empty())
        || cli.release_type.as_deref().is_some_and(|values| !values.is_empty())
        || cli.since.is_some();
```

In `SearchMode::Upgrade`, `SearchMode::Manual` and `SearchMode::Batch`, add as the first statement of each arm:

```rust
            if has_label_selector {
                return Err(SeakarrError::Config(
                    "--label/--release-type/--since are only valid in discover mode".into(),
                ));
            }
```

- [ ] **Step 7: Run to confirm it passes**

```bash
cargo test --lib mode 2>&1 | tail -5
cargo test --lib config 2>&1 | tail -5
```

Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add src/mode.rs src/config.rs src/main.rs
git commit -m "feat: validate --label, --release-type and --since into the discover plan"
```

---

### Task 12: CLI arguments

**Files:**

- Modify: `src/main.rs`

This task adds the arguments and the overrides only. The dispatch arm is changed in Task 13, together with the function it calls, so the tree compiles at every commit.

- [ ] **Step 1: Add the clap arguments**

In `struct Cli`, after `album`:

```rust
    /// Record labels to sweep in discover mode: a name or a MusicBrainz label ID,
    /// comma-delimited for several.
    #[arg(long, value_delimiter = ',')]
    label: Option<Vec<String>>,

    /// Release types wanted in a label sweep, comma-delimited.
    /// Defaults to discography.allowed_types.
    #[arg(long, value_delimiter = ',')]
    release_type: Option<Vec<String>>,

    /// Lower bound on a release group's first-release date:
    /// YYYY, YYYY-MM or YYYY-MM-DD.
    #[arg(long)]
    since: Option<String>,
```

- [ ] **Step 2: Pass them through the overrides**

In `run_with`, inside the `CliOverrides { ... }` literal:

```rust
        label: cli.label.clone(),
        release_type: cli.release_type.clone(),
        since: cli.since.clone(),
```

- [ ] **Step 3: Write the failing test, then make it pass**

Add to `mod tests` in `src/main.rs`:

```rust
    #[test]
    fn the_label_flags_are_parsed_by_clap() {
        use clap::Parser;

        let cli = Cli::try_parse_from([
            "seakarr",
            "--mode",
            "discover",
            "--label",
            "Hospital Records,Med School",
            "--release-type",
            "album,ep",
            "--since",
            "2025-01-01",
        ])
        .expect("the label flags must parse");

        assert_eq!(
            cli.label,
            Some(vec!["Hospital Records".to_string(), "Med School".to_string()])
        );
        assert_eq!(
            cli.release_type,
            Some(vec!["album".to_string(), "ep".to_string()])
        );
        assert_eq!(cli.since.as_deref(), Some("2025-01-01"));
    }

    #[test]
    fn a_single_label_needs_no_delimiter() {
        use clap::Parser;

        let cli = Cli::try_parse_from(["seakarr", "--label", "Hospital Records"])
            .expect("one label must parse");
        assert_eq!(cli.label, Some(vec!["Hospital Records".to_string()]));
        assert!(cli.release_type.is_none());
        assert!(cli.since.is_none());
    }
```

Run:

```bash
cargo test --lib main::tests 2>&1 | tail -20
```

Expected: PASS after Steps 1-3. Before them it fails to compile, which is the RED phase for this task.

- [ ] **Step 4: Run the schedule CLI tests**

```bash
cargo test --test schedule_cli_test 2>&1 | tail -5
cargo test --lib 2>&1 | tail -5
```

Expected: PASS. Nothing calls `run_label_discover_mode` yet - `dispatch_execution_plan` still matches the old `Discover { artist }` shape, so this `cargo test --lib` run fails to compile until Task 13 lands. If that is the state you are in, run only the two clap tests after Step 1 (`cargo test --lib main::tests::the_label_flags main::tests::a_single_label`) and re-run the whole suite at the end of Task 13.

- [ ] **Step 5: Commit**

```bash
git add src/main.rs
git commit -m "feat: accept --label, --release-type and --since on the command line"
```

---

### Task 13: The label runner

**Files:**

- Modify: `src/runner.rs`

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `src/runner.rs`, after the existing `discover_fixture` helper:

```rust
    // ── Label sweep ──

    use crate::discography::label::{LabelCandidate, LabelCatalogue, LabelRelease};
    use crate::mode::LabelRequest;

    /// A provider that answers one label and one catalogue, and counts calls.
    struct FakeLabelProvider {
        candidate: Option<LabelCandidate>,
        catalogue: LabelCatalogue,
        failure: Option<String>,
        catalogue_calls: Arc<AtomicUsize>,
    }

    impl FakeLabelProvider {
        fn with_catalogue(releases: Vec<ReleaseGroup>) -> Self {
            Self {
                candidate: Some(LabelCandidate {
                    id: "8562598d-60d1-4bd6-8b85-4d709e951770".into(),
                    name: "Hospital Records".into(),
                    score: Some(100),
                    label_code: Some(12939),
                    disambiguation: Some("UK drum & bass".into()),
                }),
                catalogue: LabelCatalogue {
                    releases: releases
                        .into_iter()
                        .enumerate()
                        .map(|(index, release_group)| LabelRelease {
                            release_id: format!("release-{index}"),
                            release_group,
                            artist_credit: "London Elektricity".into(),
                        })
                        .collect(),
                    incomplete: 0,
                },
                failure: None,
                catalogue_calls: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn unresolvable() -> Self {
            Self {
                candidate: Some(LabelCandidate {
                    id: "8562598d-60d1-4bd6-8b85-4d709e951770".into(),
                    name: "Some Other Label".into(),
                    score: Some(100),
                    label_code: None,
                    disambiguation: None,
                }),
                catalogue: LabelCatalogue { releases: Vec::new(), incomplete: 0 },
                failure: None,
                catalogue_calls: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn failing(reason: &str) -> Self {
            Self {
                candidate: None,
                catalogue: LabelCatalogue { releases: Vec::new(), incomplete: 0 },
                failure: Some(reason.to_string()),
                catalogue_calls: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    #[async_trait]
    impl DiscographyProvider for FakeLabelProvider {
        async fn search_artists(
            &self,
            _artist: &str,
        ) -> std::result::Result<Vec<ArtistCandidate>, DiscographyError> {
            panic!("label tests never search artists")
        }

        async fn search_artists_by_code(
            &self,
            _code: CodeIdentifier<'_>,
        ) -> std::result::Result<Vec<ArtistCandidate>, DiscographyError> {
            panic!("label tests never search artist codes")
        }

        async fn artist_by_id(
            &self,
            _artist_mbid: &str,
        ) -> std::result::Result<ArtistCandidate, DiscographyError> {
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
            if let Some(reason) = &self.failure {
                return Err(DiscographyError::Transport(reason.clone()));
            }
            Ok(self.candidate.iter().cloned().collect())
        }

        async fn label_by_id(
            &self,
            _label_mbid: &str,
        ) -> std::result::Result<LabelCandidate, DiscographyError> {
            self.catalogue_calls.fetch_add(1, Ordering::SeqCst);
            if let Some(reason) = &self.failure {
                return Err(DiscographyError::Transport(reason.clone()));
            }
            self.candidate
                .clone()
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

    fn label_request(value: &str) -> LabelRequest {
        LabelRequest {
            input: value.to_string(),
            mbid: None,
        }
    }

    #[tokio::test]
    async fn a_label_run_stages_every_missing_album_and_never_writes_the_library() {
        let (mut config, db, staging, library) = discover_fixture(&[]);
        config.filters.min_tracks = 0;
        config.discover.max_cycle_downloads = 10;
        let client = MockClient::new();
        *client.search_results.lock().unwrap() = vec![crate::client::SearchResult {
            username: "peer".into(),
            speed: 1000,
            slots: 2,
            files: vec![
                FileInfo {
                    name: r"London Elektricity\Syncopated City\01 - Bare Liars.flac".into(),
                    size: 20_000_000,
                    attribs: std::collections::HashMap::from([(0u32, 900u32)]),
                },
                FileInfo {
                    name: r"London Elektricity\Syncopated City\02 - Guess Who.flac".into(),
                    size: 20_000_000,
                    attribs: std::collections::HashMap::from([(0u32, 900u32)]),
                },
            ],
        }];
        let provider = FakeLabelProvider::with_catalogue(vec![ReleaseGroup {
            id: "group-1".into(),
            title: "Syncopated City".into(),
            first_release_date: Some("2008-06-02".into()),
            primary_type: Some("Album".into()),
            secondary_types: Vec::new(),
        }]);

        run_label_discover_mode_with_provider(
            &client,
            &config,
            &db,
            &[label_request("Hospital Records")],
            None,
            None,
            false,
            staging.path(),
            &provider,
        )
        .await
        .unwrap();

        assert!(
            staging.path().join("London Elektricity").join("Syncopated City").is_dir(),
            "the album must be staged"
        );
        let library_entries: Vec<_> = std::fs::read_dir(library.path()).unwrap().collect();
        assert!(
            library_entries.is_empty(),
            "a label run must never create a library folder, found {library_entries:?}"
        );
    }

    #[tokio::test]
    async fn a_label_album_the_library_holds_is_not_searched() {
        let (config, db, staging, _library) = discover_fixture(&[("London Elektricity", "Syncopated City")]);
        let client = MockClient::new();
        let provider = FakeLabelProvider::with_catalogue(vec![ReleaseGroup {
            id: "group-1".into(),
            title: "Syncopated City".into(),
            first_release_date: Some("2008-06-02".into()),
            primary_type: Some("Album".into()),
            secondary_types: Vec::new(),
        }]);

        run_label_discover_mode_with_provider(
            &client,
            &config,
            &db,
            &[label_request("Hospital Records")],
            None,
            None,
            false,
            staging.path(),
            &provider,
        )
        .await
        .unwrap();

        assert!(
            client.search_queries.lock().unwrap().is_empty(),
            "a present album must not be searched"
        );
    }

    #[tokio::test]
    async fn an_unresolved_label_does_not_stop_the_run() {
        let (mut config, db, staging, _library) = discover_fixture(&[]);
        config.discover.max_cycle_downloads = 10;
        let client = MockClient::new();
        let provider = FakeLabelProvider::unresolvable();

        run_label_discover_mode_with_provider(
            &client,
            &config,
            &db,
            &[label_request("Hospital Records")],
            None,
            None,
            false,
            staging.path(),
            &provider,
        )
        .await
        .expect("an unresolvable label is reported, not fatal");
    }

    #[tokio::test]
    async fn three_consecutive_provider_failures_abort_the_label_run() {
        let (mut config, db, staging, _library) = discover_fixture(&[]);
        config.discover.max_cycle_downloads = 10;
        let client = MockClient::new();
        let provider = FakeLabelProvider::failing("MusicBrainz returned HTTP 503");

        let error = run_label_discover_mode_with_provider(
            &client,
            &config,
            &db,
            &[
                label_request("Hospital Records"),
                label_request("Med School"),
                label_request("Spearhead"),
            ],
            None,
            None,
            false,
            staging.path(),
            &provider,
        )
        .await
        .expect_err("three consecutive provider failures must abort");

        assert!(
            error.to_string().contains("3 consecutive MusicBrainz failures"),
            "got {error}"
        );
    }

    #[tokio::test]
    async fn the_budget_stops_a_label_run_at_the_configured_number() {
        let (mut config, db, staging, _library) = discover_fixture(&[]);
        config.filters.min_tracks = 0;
        config.discover.max_cycle_downloads = 1;
        let client = MockClient::new();
        *client.search_results.lock().unwrap() = vec![crate::client::SearchResult {
            username: "peer".into(),
            speed: 1000,
            slots: 2,
            files: vec![
                FileInfo {
                    name: r"London Elektricity\Album One\01 - A.flac".into(),
                    size: 20_000_000,
                    attribs: std::collections::HashMap::from([(0u32, 900u32)]),
                },
                FileInfo {
                    name: r"London Elektricity\Album One\02 - B.flac".into(),
                    size: 20_000_000,
                    attribs: std::collections::HashMap::from([(0u32, 900u32)]),
                },
            ],
        }];
        let provider = FakeLabelProvider::with_catalogue(vec![
            ReleaseGroup {
                id: "group-1".into(),
                title: "Album One".into(),
                first_release_date: Some("2001".into()),
                primary_type: Some("Album".into()),
                secondary_types: Vec::new(),
            },
            ReleaseGroup {
                id: "group-2".into(),
                title: "Album Two".into(),
                first_release_date: Some("2002".into()),
                primary_type: Some("Album".into()),
                secondary_types: Vec::new(),
            },
        ]);

        run_label_discover_mode_with_provider(
            &client,
            &config,
            &db,
            &[label_request("Hospital Records")],
            None,
            None,
            false,
            staging.path(),
            &provider,
        )
        .await
        .unwrap();

        assert_eq!(
            client.search_queries.lock().unwrap().len(),
            1,
            "the budget must stop the sweep after one download"
        );
    }
```

`discover_fixture` returns `(Config, Database, TempDir, TempDir)` per its definition; the tests above bind the fourth value as `library`. Confirm the fixture's library TempDir is empty at start; if it seeds albums, adjust the assertions to compare against its initial contents rather than emptiness.

- [ ] **Step 2: Run to confirm it fails**

```bash
cargo test --lib runner::tests::a_label_run 2>&1 | tail -20
```

Expected: FAIL to compile - `run_label_discover_mode_with_provider` does not exist.

- [ ] **Step 3: Implement the runner**

In `src/runner.rs`, after `run_discover_mode`:

```rust
/// Sweep one or more record labels, staging every album of the wanted type that
/// the library does not hold.
///
/// Label discovery replaces the library-derived artist sweep for this run: the
/// library is still scanned, because "already present" cannot be answered
/// without it, but its artists never enter the work list.
pub async fn run_label_discover_mode(
    client: &dyn SoulseekClient,
    config: &Config,
    db: &Database,
    labels: &[crate::mode::LabelRequest],
    release_types: Option<&[crate::config::DiscographyReleaseType]>,
    since: Option<crate::discography::PartialDate>,
    ignore_processed: bool,
) -> Result<()> {
    if !config.discography.enabled {
        return Err(SeakarrError::Config(
            "discover mode requires discography.enabled: true".into(),
        ));
    }
    if config.library.paths.is_empty() {
        return Err(SeakarrError::Config(
            "library.paths is empty - a label sweep needs a library to check what is present".into(),
        ));
    }
    let provider = MusicBrainzProvider::new().map_err(|error| {
        SeakarrError::MusicBrainz(format!("could not construct the provider: {error}"))
    })?;
    let staging_dir = Path::new(&config.storage.staging_dir);
    crate::staging::prepare(staging_dir, &config.library.paths)?;
    run_label_discover_mode_with_provider(
        client,
        config,
        db,
        labels,
        release_types,
        since,
        ignore_processed,
        staging_dir,
        &provider,
    )
    .await
}

/// Timeline-free core of a label sweep, with the provider injected.
#[allow(clippy::too_many_arguments)] // the run's plan, report, counters and
                                     // staging root travel together
async fn run_label_discover_mode_with_provider(
    client: &dyn SoulseekClient,
    config: &Config,
    db: &Database,
    labels: &[crate::mode::LabelRequest],
    release_types: Option<&[crate::config::DiscographyReleaseType]>,
    since: Option<crate::discography::PartialDate>,
    ignore_processed: bool,
    staging_dir: &Path,
    provider: &dyn DiscographyProvider,
) -> Result<()> {
    use crate::discography::label::{discover_label_releases, LabelDiscoveryOutcome, LabelInput};
    use crate::label_discover;

    let allowed: Vec<crate::config::DiscographyReleaseType> = release_types
        .map(<[crate::config::DiscographyReleaseType]>::to_vec)
        .unwrap_or_else(|| config.discography.allowed_types.clone());
    let (cancel, _guard) = arm_cancellation();
    let progress = if is_interactive() {
        Some(ProgressDisplay::new())
    } else {
        None
    };

    let mut counters = label_discover::LabelCounters {
        requested: labels.iter().map(|label| label.input.clone()).collect(),
        budget_limit: config.discover.max_cycle_downloads,
        ..label_discover::LabelCounters::default()
    };
    let mut report = RunReport::new();
    let mut consecutive_provider_failures: u32 = 0;
    let mut collected: Vec<crate::discography::label::LabelRelease> = Vec::new();

    for request in labels {
        counters.labels_examined += 1;
        let input = LabelInput::from_parts(request.input.clone(), request.mbid.clone());
        let discovery = discover_label_releases(
            provider,
            db,
            &input,
            &config.discography,
            FailureCacheUse::Honour,
        )
        .await;
        match discovery.outcome {
            LabelDiscoveryOutcome::Catalogue {
                releases,
                incomplete,
                provenance,
            } => {
                counters.resolved.push((
                    discovery.label_name.unwrap_or_else(|| request.input.clone()),
                    discovery.label_mbid.unwrap_or_default(),
                ));
                counters.incomplete += incomplete;
                match &provenance {
                    DiscoveryProvenance::StaleCache {
                        age_days,
                        refresh_error,
                        kind,
                    } => {
                        tracing::warn!(
                            "{}: label catalogue cache is {age_days} day(s) old and refresh failed ({refresh_error}); using stale cache",
                            request.input
                        );
                        if *kind == DiscoveryFailure::Provider {
                            consecutive_provider_failures += 1;
                            if consecutive_provider_failures >= DISCOVER_PROVIDER_FAILURE_LIMIT {
                                return Err(abort_label_run(
                                    &mut report,
                                    &counters,
                                    progress.as_ref(),
                                    &request.input,
                                ));
                            }
                        } else {
                            consecutive_provider_failures = 0;
                        }
                    }
                    _ => consecutive_provider_failures = 0,
                }
                collected.extend(releases);
            }
            LabelDiscoveryOutcome::Failed {
                reason,
                kind,
                from_cache,
            } => {
                if from_cache {
                    counters.cached_failures.push(request.input.clone());
                    continue;
                }
                match kind {
                    DiscoveryFailure::Unresolved => {
                        tracing::warn!(
                            "{}: label could not be resolved on MusicBrainz: {reason}",
                            request.input
                        );
                        counters.unresolved.push(request.input.clone());
                        consecutive_provider_failures = 0;
                    }
                    DiscoveryFailure::Provider => {
                        tracing::warn!(
                            "{}: MusicBrainz unavailable ({reason}); label skipped",
                            request.input
                        );
                        counters
                            .provider_failed
                            .push((request.input.clone(), reason));
                        consecutive_provider_failures += 1;
                        if consecutive_provider_failures >= DISCOVER_PROVIDER_FAILURE_LIMIT {
                            return Err(abort_label_run(
                                &mut report,
                                &counters,
                                progress.as_ref(),
                                &request.input,
                            ));
                        }
                    }
                }
            }
        }
    }

    let selection =
        label_discover::label_targets(&collected, &allowed, since, &config.discover.exclude_artists);
    counters.selection = selection.counts;

    // One library walk answers "already present" for every label, exactly as the
    // artist sweep pays for it once.
    let index = match with_scan_indicator(progress.as_ref(), |walk_progress| {
        discover::index_from_paths(
            &config.library.paths,
            &config.filters,
            Some(&cancel),
            walk_progress,
        )
    }) {
        Ok(index) => index,
        Err(SeakarrError::Cancelled) => {
            tracing::info!("label sweep: library scan cancelled by user - stopping before any album");
            finish_label_run(&mut report, &counters, progress.as_ref(), staging_dir);
            return Ok(());
        }
        Err(error) => {
            // As in the artist sweep, a failing scan must not break the run: warn
            // and treat every album as missing.
            tracing::warn!(
                "label sweep: library scan failed ({error}); treating every album as missing"
            );
            discover::LibraryIndex::default()
        }
    };

    let mut budget = discover::DownloadBudget::new(config.discover.max_cycle_downloads);
    for target in &selection.targets {
        if budget.exhausted() {
            counters.budget_reached_at = Some(target.artist.clone());
            break;
        }
        if cancel.load(Ordering::SeqCst) {
            break;
        }
        if index.contains_album(&target.artist, &target.target.title) {
            counters.present += 1;
            continue;
        }
        let result = process_album(
            client,
            &target.artist,
            Some(&target.target.title),
            ignore_processed,
            config,
            db,
            staging_dir,
            progress.as_ref(),
            Some(&cancel),
            None,
            Some(LibraryTarget::StagingOnly),
        )
        .await;
        match result {
            Ok(outcome) => {
                if charges_download_budget(&outcome) {
                    budget.charge();
                }
                if matches!(
                    &outcome,
                    AlbumOutcome::Downloaded {
                        destination: DownloadDestination::Staging(_),
                        ..
                    }
                ) {
                    counters.staged_albums += 1;
                }
                report.record(&target.artist, &target.target.title, outcome);
            }
            Err(error) => {
                tracing::error!(
                    "Album processing failed: {} - {}: {error}",
                    target.artist,
                    target.target.title
                );
                if charges_after_error(&error) {
                    budget.charge();
                }
                counters.failed_albums += 1;
                report.record(
                    &target.artist,
                    &target.target.title,
                    AlbumOutcome::Failed {
                        reason: error.to_string(),
                    },
                );
            }
        }
    }

    finish_label_run(&mut report, &counters, progress.as_ref(), staging_dir);
    Ok(())
}

/// Abort a label run on the provider circuit breaker, after reporting what was
/// done so far.
fn abort_label_run(
    report: &mut RunReport,
    counters: &crate::label_discover::LabelCounters,
    progress: Option<&ProgressDisplay>,
    staging_dir: &Path,
    label: &str,
) -> SeakarrError {
    finish_label_run(report, counters, progress, staging_dir);
    SeakarrError::MusicBrainz(format!(
        "{label}: {DISCOVER_PROVIDER_FAILURE_LIMIT} consecutive MusicBrainz failures; aborting discover run"
    ))
}

/// Add a label run's notices, clear the scan indicator and print the summary.
///
/// One owner for the run's exit reporting, mirroring [`finish_discover_run`], so a
/// run that stops early (scan cancelled, breaker tripped) reports through the
/// same path as one that ran to the end.
fn finish_label_run(
    report: &mut RunReport,
    counters: &crate::label_discover::LabelCounters,
    progress: Option<&ProgressDisplay>,
    staging_dir: &Path,
) {
    for notice in crate::label_discover::label_notices(counters, &staging_dir.to_string_lossy()) {
        report.add_notice(notice);
    }
    if let Some(display) = progress {
        display.clear();
    }
    report.print_summary();
}
```

The two `abort_label_run` call sites inside the label loop pass `staging_dir`
as their fourth argument, and the cancelled-scan early return passes it too:

```rust
                                return Err(abort_label_run(
                                    &mut report,
                                    &counters,
                                    progress.as_ref(),
                                    staging_dir,
                                    &request.input,
                                ));
```

```rust
                            finish_label_run(&mut report, &counters, progress.as_ref(), staging_dir);
```

- [ ] **Step 4: Dispatch the plan from `main.rs`**

Replace the `ExecutionPlan::Discover` arm in `dispatch_execution_plan` in
`src/main.rs`:

```rust
        ExecutionPlan::Discover {
            artist,
            labels,
            release_types,
            since,
        } => {
            if labels.is_empty() {
                runner::run_discover_mode(client, config, db, artist.as_deref(), ignore_processed)
                    .await
            } else {
                runner::run_label_discover_mode(
                    client,
                    config,
                    db,
                    labels,
                    release_types.as_deref(),
                    *since,
                    ignore_processed,
                )
                .await
            }
        }
```

- [ ] **Step 5: Run to confirm it passes**

```bash
cargo test --lib runner::tests 2>&1 | tail -10
```

Expected: PASS, including the five new tests.

- [ ] **Step 6: Run the whole suite**

```bash
cargo test 2>&1 | tail -10
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

Expected: PASS on all three. Fix any clippy complaint properly rather than silencing it, except where the existing file already uses an `#[allow]` for the same reason.

- [ ] **Step 7: Commit**

```bash
git add src/runner.rs src/main.rs
git commit -m "feat: sweep a record label and stage the albums the library lacks"
```

- [ ] **Step 8: Re-run the CLI and schedule tests, which were blocked in Task 12**

```bash
cargo test --lib main::tests 2>&1 | tail -5
cargo test --test schedule_cli_test 2>&1 | tail -5
```

Expected: PASS. The dispatch arm now compiles, so the whole binary does.

---

### Task 14: Documentation

**Files:**

- Modify: `README.md`

- [ ] **Step 1: Extend the discover feature bullet**

After the existing discover paragraph's final sentence (`Release types come from discography.allowed_types.`), add:

```markdown
- **Label discovery** — `--label` sweeps a record label instead of your library:
  the value is a label name or a MusicBrainz label ID, and several labels are
  comma-delimited (`--label "Hospital Records,Med School"`). The label is resolved
  on MusicBrainz with the same exact-match and score-dominance rules used for
  artists, so an ambiguous name is refused with its candidates listed rather than
  guessed; `discography.label_mbids` pins a name to an ID when the same label is
  swept repeatedly. `--release-type` overrides `discography.allowed_types` for that
  run (`album` is accepted as another word for `studio_album`), and `--since
  YYYY[-MM[-DD]]` bounds the sweep by first-release-date; an undated release group
  cannot satisfy a lower bound and is excluded. Albums already in the library are
  skipped and every other eligible album is downloaded into `storage.staging_dir`
  under `<artist>/<album>/` — a label sweep never writes into your library.
  A label's catalogue is cached for `discography.cache_days`, so a brand-new
  release can take up to that long to appear.
```

- [ ] **Step 2: Document the config key**

In the configuration reference, beside `discography.artist_mbids`, add:

```markdown
| `discography.label_mbids` | Label name to MusicBrainz label ID, for a name that resolves ambiguously. Keys are unique after normalisation and values must be valid MBIDs. | `{}` |
```

- [ ] **Step 3: Add the flags to the flag table**

Add `--label`, `--release-type` and `--since` with their conflicts (`--label` only in discover, refused with `--artist`, `--album`, `--batch-file`; `--release-type` and `--since` require `--label`).

- [ ] **Step 4: Lint the README**

```bash
markdownlint --fix README.md && markdownlint README.md
```

Expected: exit 0. If lines exceed 120 characters outside tables, reflow them.

- [ ] **Step 5: Commit**

```bash
git add README.md
git commit -m "docs: describe label discovery"
```

---

### Task 15: End-to-end pipeline test

**Files:**

- Modify: `tests/pipeline_test.rs`

The integration test proves the public surface, not just the private wiring: a `--label` run through `mode::resolve_execution_plan` plus the runner.

- [ ] **Step 1: Write the failing test**

Append to `tests/pipeline_test.rs`:

```rust
/// A label sweep stages the album and leaves the library untouched.
///
/// Uses the manual-mode-shaped mock client through `process_album`, which is the
/// same call the label runner makes, so this covers the staged tree and the
/// staging-only target without needing a MusicBrainz provider.
#[tokio::test]
async fn test_label_sweep_stages_without_writing_the_library() {
    use seakarr::runner::{process_album, LibraryTarget};

    let client = MockClient::new();
    *client.search_results.lock().unwrap() = vec![SearchResult {
        username: "fastuser".into(),
        speed: 1000,
        slots: 2,
        files: vec![
            make_file(
                r"London Elektricity\Syncopated City\01 - Bare Liars.flac",
                900,
                15_000_000,
            ),
            make_file(
                r"London Elektricity\Syncopated City\02 - Guess Who.flac",
                850,
                12_000_000,
            ),
        ],
    }];

    let staging = TempDir::new().unwrap();
    let library = TempDir::new().unwrap();
    let mut config = Config::default();
    config.soulseek.username = "test".into();
    config.soulseek.password = "test".into();
    config.storage.staging_dir = staging.path().to_string_lossy().into();
    config.library.paths = vec![library.path().to_string_lossy().into()];
    config.download.min_upload_speed_kbps = 0;
    config.download.max_retries = 1;
    config.notifications.urls = vec![];
    config.filters.min_tracks = 0;

    let db = Database::open_in_memory().unwrap();

    process_album(
        &client,
        "London Elektricity",
        Some("Syncopated City"),
        false,
        &config,
        &db,
        staging.path(),
        None,
        None,
        None,
        Some(LibraryTarget::StagingOnly),
    )
    .await
    .expect("the staged download must succeed");

    assert!(
        staging
            .path()
            .join("London Elektricity")
            .join("Syncopated City")
            .is_dir(),
        "the album must be staged under <artist>/<album>"
    );
    assert!(
        std::fs::read_dir(library.path()).unwrap().next().is_none(),
        "a staging-only target must not create a library folder"
    );
}
```

- [ ] **Step 2: Run to confirm it passes**

```bash
cargo test --test pipeline_test 2>&1 | tail -10
```

Expected: PASS. If `process_album`'s staging path is `<staging>/<artist>/<album>` (the nested layout) the assertion holds; if the mock's file names place it differently, read the outcome's `DownloadDestination::Staging(path)` and assert against that path instead of recomputing it.

- [ ] **Step 3: Commit**

```bash
git add tests/pipeline_test.rs
git commit -m "test: prove a label-sourced album is staged and not placed"
```

---

### Task 16: Full gate

**Files:** none.

- [ ] **Step 1: Format and lint**

```bash
cargo fmt --all
cargo clippy --all-targets -- -D warnings
```

Expected: both clean.

- [ ] **Step 2: Full test suite**

```bash
cargo test --all 2>&1 | tail -15
```

Expected: 0 failures. Count the tests and compare against the baseline from Task 1 plus the new ones.

- [ ] **Step 3: Coverage**

```bash
cargo llvm-cov --all-targets --fail-under-lines 95 2>&1 | tail -15
```

Expected: at or above 95% lines. If `cargo llvm-cov` is not installed, report the missing tool rather than declaring the gate passed.

- [ ] **Step 4: Documentation and dependency gates**

```bash
markdownlint README.md docs/agent/specs/2026-11-01-label-based-discovery-design.md
cargo audit
cargo deny check
pre-commit run --all-files
```

Expected: all clean.

- [ ] **Step 5: Commit any gate fixes**

```bash
git add -A
git commit -m "chore: satisfy the label discovery quality gates"
```

Only if the gate produced changes; an empty commit is not needed.

---

## Spec coverage

Every section of `2026-11-01-label-based-discovery-design.md` maps to a task:

| Spec section | Tasks |
| --- | --- |
| Goal, Scope in, Decisions (one flag, list, replaces sweep) | 3, 11, 13 |
| Scope out (no library writes, no new discover keys, no series) | 12, 13, 14 |
| CLI surface (flags, refusals) | 11, 12 |
| Config surface (`discography.label_mbids`) | 7, 10 |
| Provider layer (trait, wire types, credit string) | 5, 6 |
| Catalogue retrieval (`status=official`, no `type`, guards, ceiling) | 6 |
| Caching and pins (TTL, stale, failure cache, MBID mismatch, version, size) | 1, 7, 10 |
| Resolution (pin, by ID, search, dominance, refusal listing) | 2, 3, 4, 7 |
| Data flow | 13 |
| Selection detail (type, `--since`, undated, exclusion, dedupe, ordering) | 8 |
| Download and placement (staging only, budget, processed records) | 13 |
| Error handling (nine rows) | 2, 6, 7, 11, 13 |
| Observability (eleven notices) | 9 |
| Components and boundaries | 1-13 (file table to task map) |
| Testing (unit, validation, integration) | 3-11, 13 |
| Documentation | 14 |
| Accepted consequences | 14 (README wording), 16 (gate records them) |
| Acceptance criteria 1-10 | 4, 7, 8, 11, 13, 15 (criterion 10 guarded by Task 1 Step 6) |

## Verification checklist for the implementer

- Every new refusal names the flag it is about, in the style of the existing messages.
- No label path writes under `library.paths`: the only placement target used is `LibraryTarget::StagingOnly`.
- `label.<key>` rows and artist rows never collide: the label cache key is prefixed twice for an ID (`label:mbid:<uuid>`) and once for a name (`label:<normalised name>`).
- An unresolvable label never counts towards the circuit breaker; a provider failure always does.
- No counter claims staging unless the outcome was `Downloaded { destination: Staging(_), .. }`.
- The artist sweep's behaviour and messages are unchanged: `git diff src/runner.rs` shows additions only outside `run_discover_mode_with_provider`.
