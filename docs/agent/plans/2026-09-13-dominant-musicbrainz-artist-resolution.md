# Dominant MusicBrainz Artist Resolution Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use sub-agents (recommended)
> to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for
> tracking.

**Goal:** Let artist-only manual mode select a uniquely dominant MusicBrainz
canonical exact-name candidate (a unique score 100 with a margin of at least
10) instead of falling back to the legacy folder heuristic.

**Architecture:** Retain the MusicBrainz artist-search `score` in the provider
model, then replace the single-match-only resolver with `resolve_artist`,
which filters canonical exact-name candidates, validates scores, and applies
deterministic dominance rules. Discovery keeps its existing precedence
(configured MBID, fresh cache, refresh, stale cache, legacy fallback) and logs
dominance evidence at INFO. No new config keys, no database migration, and no
extra HTTP requests.

**Tech Stack:** Rust, `serde`/`serde_json`, `async-trait`, `wiremock` and
`tokio` for provider tests, and `tracing` capture for logging tests.

**Spec:** `docs/agent/specs/2026-09-13-dominant-musicbrainz-artist-resolution-design.md`

<!-- markdownlint-disable MD013 -->

---

## Scope Check

Single subsystem. The change lives entirely inside the existing `discography` module plus one README section. It does not touch search, filtering, downloading, organising, scheduling, or configuration schema. No decomposition into separate plans is needed.

## File Structure

| File | Responsibility | Change |
| --- | --- | --- |
| `src/discography/mod.rs` | Domain candidate model, resolver policy, discovery orchestration, cache-time precedence | Modify: add `score` field, add dominance types/rules, replace `resolve_exact_artist` with `resolve_artist`, add INFO logging, add tests |
| `src/discography/musicbrainz.rs` | HTTP behaviour and wire decoding only | Modify: decode and validate `score`, add tests |
| `src/runner.rs` | Artist-only orchestration | Modify: add `score: None` to two test-only `ArtistCandidate` literals |
| `README.md` | User documentation | Modify: artist-resolution bullet under the artist-only manual-mode FAQ |

Files that change together stay together: the candidate model, its policy, and its orchestration all live in `discography/mod.rs`.

## Conventions used by this plan

- Every task ends with a commit.
- Run Rust tests with `cargo test -p seakarr --lib <filter>` while iterating; run `cargo test --workspace` before committing.
- Existing test helpers you will reuse in `src/discography/mod.rs` (`mod tests`):
  `Database::open_in_memory()`, `cache_groups(&db, artist_key, mbid, fetched_at, groups)`,
  `group(id, title, date, primary, secondary)`, `FakeProvider`, and `CapturingWriter`.
- Existing test helper in `src/discography/musicbrainz.rs` (`mod tests`):
  `MusicBrainzProvider::for_test(server.uri(), Duration::ZERO)` with `wiremock`.

---

### Task 1: Add an optional score to the domain candidate

Mechanical, behaviour-preserving step. It exists so the next task can carry real scores without mixing a model change into a policy change.

**Files:**

- Modify: `src/discography/mod.rs` (struct at ~line 24)
- Modify: `src/discography/musicbrainz.rs` (2 literals)
- Modify: `src/runner.rs` (2 literals)

- [ ] **Step 1: Add the field**

In `src/discography/mod.rs`, replace:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtistCandidate {
    pub id: String,
    pub name: String,
}
```

with:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtistCandidate {
    pub id: String,
    pub name: String,
    /// MusicBrainz artist-search relevance score in the documented 0-100
    /// range. `None` when the response omitted a score, which is normal for
    /// lookups by MBID and for a single unambiguous search result.
    pub score: Option<u8>,
}
```

- [ ] **Step 2: Compile to find every construction site**

Run: `cargo build --workspace 2>&1 | rg -n 'missing field .score.|error\[' | head -40`
Expected: errors naming each `ArtistCandidate { .. }` literal that needs the new field. There are 11 known sites; the compiler is the authority.

- [ ] **Step 3: Update every construction site**

Add `score: None` to each literal except the struct definition. The known sites are:

- `src/discography/musicbrainz.rs` — the `search_artists` wire mapping and the `artist_by_id` lookup.
- `src/discography/mod.rs` — the `exact_artist_resolution_uses_only_unique_canonical_name` test literals, `FakeProvider::with_groups`, and `FakeProvider::artist_by_id`.
- `src/runner.rs` — `FakeDiscographyProvider::search_artists` and `FakeDiscographyProvider::artist_by_id`.

Example for `src/discography/musicbrainz.rs`:

```rust
            .map(|wire| ArtistCandidate {
                id: wire.id,
                name: wire.name,
                score: None,
            })
```

Example for a test literal in `src/discography/mod.rs`:

```rust
            ArtistCandidate {
                id: "1".into(),
                name: "ＡC/DC".into(),
                score: None,
            },
```

- [ ] **Step 4: Verify no construction site was missed**

Run: `rg -n 'ArtistCandidate \{' src/ | wc -l`
Expected: 11 (the 11 literals from the spec's exploration; the count includes none of the struct definition, which does not contain `ArtistCandidate {`).

Then confirm each literal carries a score:

Run: `rg -n -A4 'ArtistCandidate \{' src/ | rg -c 'score:'`
Expected: 11

- [ ] **Step 5: Run the suite**

Run: `cargo test --workspace`
Expected: all tests pass (775+ tests, 0 failures). No behaviour changed.

- [ ] **Step 6: Commit**

```bash
git add src/discography/mod.rs src/discography/musicbrainz.rs src/runner.rs
git commit -m "refactor: carry an optional MusicBrainz score on artist candidates"
```

---

### Task 2: Retain and validate the MusicBrainz search score

**Files:**

- Modify: `src/discography/musicbrainz.rs` (`ArtistWire`, both wire mappings, new deserializer, unit tests)

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `src/discography/musicbrainz.rs`:

```rust
    #[tokio::test]
    async fn artist_search_retains_scores_and_tolerates_missing_or_string_scores() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ws/2/artist"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "count": 3,
                "offset": 0,
                "artists": [
                    { "id": "16b97aaa-d7c0-469f-8c97-47c705b2d02f", "name": "Ils", "score": 100 },
                    { "id": "638e9183-2cde-4c07-b1d5-1f0e0361ed1c", "name": "Ils", "score": "86" },
                    { "id": "cc54a811-a221-49f1-b93d-ed42f1affbb0", "name": "Ils" }
                ]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let provider = MusicBrainzProvider::for_test(server.uri(), Duration::ZERO).unwrap();
        let artists = provider.search_artists("Ils").await.unwrap();

        assert_eq!(artists[0].score, Some(100));
        assert_eq!(artists[1].score, Some(86));
        assert_eq!(artists[2].score, None);
    }

    #[tokio::test]
    async fn artist_search_rejects_out_of_range_and_non_numeric_scores() {
        for invalid in [serde_json::json!(101), serde_json::json!(-1), serde_json::json!("high")] {
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
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p seakarr --lib discography::musicbrainz::tests::artist_search_retains_scores_and_tolerates_missing_or_string_scores -v`
Expected: FAIL — `artists[0].score` is `None` because the wire model does not decode `score` yet.

- [ ] **Step 3: Extend the wire model**

In `src/discography/musicbrainz.rs`, replace:

```rust
#[derive(Debug, Deserialize)]
struct ArtistWire {
    id: String,
    name: String,
}
```

with:

```rust
#[derive(Debug, Deserialize)]
struct ArtistWire {
    id: String,
    name: String,
    #[serde(default, deserialize_with = "deserialize_optional_score")]
    score: Option<u8>,
}

/// Decode a MusicBrainz artist-search score.
///
/// The search API returns a JSON integer, while MusicBrainz's MMD JSON
/// examples show a numeric string, so both are accepted. Anything else, and
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
        serde_json::Value::Number(number) => number.as_i64(),
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
```

- [ ] **Step 4: Carry the score through both mappings**

In `search_artists`' mapping, replace `score: None,` (added in Task 1) with:

```rust
                score: wire.score,
```

Leave `artist_by_id` at `score: None`; that endpoint carries no search score and configured MBIDs need no ranking. Add this comment directly above its literal:

```rust
        // The lookup-by-MBID endpoint has no search relevance score, and a
        // configured MBID needs no ranking.
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p seakarr --lib discography::musicbrainz::tests -v`
Expected: PASS for both new tests and every pre-existing provider test.

- [ ] **Step 6: Commit**

```bash
git add src/discography/musicbrainz.rs
git commit -m "feat: decode MusicBrainz artist search scores"
```

---

### Task 3: Add deterministic dominance rules to the resolver

Replaces `resolve_exact_artist` with `resolve_artist`, which returns the chosen candidate plus the reason it was chosen. Also updates the three existing call/assertion sites so the crate keeps compiling.

**Files:**

- Modify: `src/discography/mod.rs` (resolver ~lines 76-106, its test ~lines 560-590, discovery call ~line 388)
- Modify: `src/discography/musicbrainz.rs` (one test assertion ~line 502)

- [ ] **Step 1: Write the failing resolver tests**

In `mod tests` in `src/discography/mod.rs`, add these helpers and tests:

```rust
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
    fn dominance_selects_the_unique_score_100_candidate() {
        let resolved = resolve_artist("ils", &ils_candidates()).unwrap();
        assert_eq!(resolved.candidate.id, "16b97aaa-d7c0-469f-8c97-47c705b2d02f");
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
        let mut reversed = ils_candidates();
        reversed.reverse();
        assert_eq!(
            resolve_artist("ils", &reversed).unwrap(),
            resolve_artist("ils", &ils_candidates()).unwrap()
        );
    }

    #[test]
    fn unique_exact_name_still_resolves_without_a_score() {
        let candidates = vec![scored("only", "Nils Frahm", None)];
        let resolved = resolve_artist("nils frahm", &candidates).unwrap();
        assert_eq!(resolved.resolution, ArtistResolution::UniqueExactName);
        assert_eq!(resolved.candidate.id, "only");
    }

    #[test]
    fn weak_or_incomplete_dominance_is_unresolved() {
        let cases: Vec<(&str, Vec<ArtistCandidate>)> = vec![
            (
                "top score below 100",
                vec![
                    scored("a", "Ils", Some(99)),
                    scored("b", "Ils", Some(70)),
                ],
            ),
            (
                "margin below 10",
                vec![
                    scored("a", "Ils", Some(100)),
                    scored("b", "Ils", Some(91)),
                ],
            ),
            (
                "tied top score",
                vec![
                    scored("a", "Ils", Some(100)),
                    scored("b", "Ils", Some(100)),
                    scored("c", "Ils", Some(70)),
                ],
            ),
            (
                "missing competing score",
                vec![scored("a", "Ils", Some(100)), scored("b", "Ils", None)],
            ),
            (
                "invalid score above the range",
                vec![
                    scored("a", "Ils", Some(255)),
                    scored("b", "Ils", Some(70)),
                ],
            ),
            ("no exact match", vec![scored("a", "Somebody Else", Some(100))]),
        ];

        for (label, candidates) in cases {
            let result = resolve_artist("ils", &candidates);
            assert!(
                matches!(result, Err(DiscographyError::ArtistUnresolved(_))),
                "{label} must stay unresolved, got {result:?}"
            );
        }
    }

    #[test]
    fn dominance_margin_boundary_is_inclusive() {
        let candidates = vec![
            scored("a", "Ils", Some(100)),
            scored("b", "Ils", Some(90)),
        ];
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
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p seakarr --lib discography::tests::dominance -v`
Expected: FAIL — the tests do not compile because `resolve_artist`, `ArtistResolution`, and `DominanceEvidence` do not exist.

- [ ] **Step 3: Add the dominance types and replace the resolver**

In `src/discography/mod.rs`, replace the whole existing `resolve_exact_artist` function (its doc comment plus body) with:

```rust
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
/// canonical names compare equal after [`normalize_catalog_key`], exactly one
/// candidate holds the highest score, that score is [`REQUIRED_TOP_SCORE`],
/// and it leads the runner-up by at least [`REQUIRED_SCORE_MARGIN`].
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
    let mut scored: Vec<(u8, &ArtistCandidate)> = Vec::with_capacity(exact_matches);
    for candidate in matches {
        let Some(score) = candidate.score else {
            return Err(DiscographyError::ArtistUnresolved(format!(
                "{exact_matches} candidates match {artist_key:?} and at least one has no search score"
            )));
        };
        if score > REQUIRED_TOP_SCORE {
            return Err(DiscographyError::ArtistUnresolved(format!(
                "{exact_matches} candidates match {artist_key:?} and one reports the invalid score {score}"
            )));
        }
        scored.push((score, candidate));
    }
    // Sort by score, then MBID, so the comparison never depends on the order
    // MusicBrainz returned the candidates in.
    scored.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.id.cmp(&right.1.id)));
    let (top_score, top) = scored[0];
    let leaders = scored.iter().filter(|(score, _)| *score == top_score).count();
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
```

- [ ] **Step 4: Update the existing resolver test**

Replace the existing `exact_artist_resolution_uses_only_unique_canonical_name` test in `src/discography/mod.rs` with:

```rust
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
        assert!(resolve_artist("Missing", &candidates).is_err());
    }
```

- [ ] **Step 5: Update the provider test assertion**

In `src/discography/musicbrainz.rs`, inside `artist_search_sends_identity_headers_and_encoded_query`, replace:

```rust
        assert!(
            super::super::resolve_exact_artist("AC DC", &artists).is_err(),
            "sort names, aliases, and scores must not widen canonical-name matching"
        );
```

with:

```rust
        assert!(
            super::super::resolve_artist("AC DC", &artists).is_err(),
            "sort names, aliases, and scores must not widen canonical-name matching"
        );
```

- [ ] **Step 6: Update the discovery call site**

In `src/discography/mod.rs`, inside `discover_artist_albums_at`, replace:

```rust
                let candidates = provider.search_artists(artist).await?;
                resolve_exact_artist(&artist_key, &candidates)?
```

with:

```rust
                let candidates = provider.search_artists(artist).await?;
                resolve_artist(&artist_key, &candidates)?.candidate
```

- [ ] **Step 7: Confirm no stale references remain**

Run: `rg -n 'resolve_exact_artist' src/`
Expected: no output.

- [ ] **Step 8: Run the tests to verify they pass**

Run: `cargo test -p seakarr --lib discography::tests -v`
Expected: PASS for the eight new resolver tests, the updated exact-name test, and all pre-existing discovery tests.

- [ ] **Step 9: Commit**

```bash
git add src/discography/mod.rs src/discography/musicbrainz.rs
git commit -m "feat: resolve dominant duplicate MusicBrainz artist names by score"
```

---

### Task 4: Log dominance evidence during discovery

**Files:**

- Modify: `src/discography/mod.rs` (`discover_artist_albums_at`, ~line 383)

- [ ] **Step 1: Write the failing test**

In `mod tests` in `src/discography/mod.rs`, add:

```rust
    #[tokio::test]
    async fn dominant_selection_logs_its_evidence_at_info() {
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
        let buffer = Arc::new(Mutex::new(String::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_writer(CapturingWriter(Arc::clone(&buffer)))
            .with_max_level(tracing::Level::INFO)
            .with_ansi(false)
            .without_time()
            .finish();

        let outcome = tracing::subscriber::with_default(subscriber, || {
            tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(discover_artist_albums_at(
                    &provider,
                    &db,
                    "Ils",
                    &DiscographyConfig::default(),
                    1_000,
                ))
        });

        assert!(matches!(
            outcome,
            DiscoveryOutcome::Authoritative {
                provenance: DiscoveryProvenance::Refreshed,
                ..
            }
        ));

        let logs = buffer.lock().unwrap().clone();
        assert!(
            logs.contains("16b97aaa-d7c0-469f-8c97-47c705b2d02f"),
            "log must name the selected MBID, got: {logs}"
        );
        assert!(logs.contains("100"), "log must include the top score");
        assert!(logs.contains("86"), "log must include the runner-up score");
        assert!(logs.contains("14"), "log must include the margin");
    }
```

If `mutex`/`Arc`/`VecDeque` imports or the `tracing_subscriber` dependency are not already in scope in `mod tests`, reuse exactly what the existing `excluded_release_groups_are_logged_with_reasons` test uses — do not add new dependencies.

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p seakarr --lib discography::tests::dominant_selection_logs_its_evidence_at_info -v`
Expected: FAIL on the MBID assertion — no selection log exists yet.

- [ ] **Step 3: Add the INFO log**

In `discover_artist_albums_at`, replace the branch updated in Task 3 with:

```rust
            None => {
                let candidates = provider.search_artists(artist).await?;
                let resolved = resolve_artist(&artist_key, &candidates)?;
                if let ArtistResolution::ScoreDominance(evidence) = &resolved.resolution {
                    tracing::info!(
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
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p seakarr --lib discography::tests::dominant_selection_logs_its_evidence_at_info -v`
Expected: PASS.

- [ ] **Step 5: Run the whole discography module**

Run: `cargo test -p seakarr --lib discography -v`
Expected: PASS, including all pre-existing logging tests.

- [ ] **Step 6: Commit**

```bash
git add src/discography/mod.rs
git commit -m "feat: log dominant artist selection evidence at info level"
```

---

### Task 5: Prove cache, override, and fallback behaviour end to end

**Files:**

- Modify: `src/discography/mod.rs` (`mod tests`)

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `src/discography/mod.rs`:

```rust
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
        )
        .await;
        assert!(matches!(
            outcome,
            DiscoveryOutcome::Authoritative {
                provenance: DiscoveryProvenance::Refreshed,
                ..
            }
        ));

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

        let outcome =
            discover_artist_albums_at(&provider, &db, "Ils", &DiscographyConfig::default(), 31 * 86_400)
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
        )
        .await;

        assert!(matches!(
            outcome,
            DiscoveryOutcome::LegacyFallback { reason }
                if reason.contains("artist could not be resolved safely")
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

        let outcome = discover_artist_albums_at(&provider, &db, "Ils", &config, 1_000).await;

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

        let outcome =
            discover_artist_albums_at(&provider, &db, "Ils", &DiscographyConfig::default(), 1_100)
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
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p seakarr --lib discography::tests::dominant_selection_persists_the_chosen_mbid discography::tests::unresolved_dominance -v`
Expected: FAIL on the assertions — the chosen MBID is not yet persisted by the new path, and the legacy reason wording is not yet produced.

- [ ] **Step 3: Confirm the production code already satisfies them**

No production change should be required: Tasks 3 and 4 already route the dominant candidate through the existing cache write, override branch, and `stale_or_legacy` path. If a test fails, do **not** weaken it — report the discrepancy instead of editing the assertion, because a genuine failure here means the routing is wrong.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p seakarr --lib discography::tests -v`
Expected: PASS for all five new tests.

- [ ] **Step 5: Commit**

```bash
git add src/discography/mod.rs
git commit -m "test: prove dominance routing through cache, override, and fallback"
```

---

### Task 6: Update the README artist-resolution documentation

**Files:**

- Modify: `README.md` (~lines 498-503, the artist-only FAQ bullet)

- [ ] **Step 1: Replace the stale bullet**

In `README.md`, under `**Q: How does artist-only manual mode choose which albums to download?**`, replace:

```markdown
- Automatic resolution accepts only one unique exact artist name match. Matching uses Unicode NFKC
  normalization, lowercase conversion, trimming, and whitespace collapse, but punctuation stays significant,
  so `AC/DC` and `AC DC` are distinct. Zero matches or multiple matches are unresolved: seakarr does not pick
  the highest-scored result, it falls back as described below. Use `discography.artist_mbids` to pin an
  ambiguous name to a MusicBrainz artist UUID; a configured ID always takes precedence over name search.
```

with:

```markdown
- Automatic resolution accepts only candidates whose canonical MusicBrainz name matches exactly. Matching uses
  Unicode NFKC normalization, lowercase conversion, trimming, and whitespace collapse, but punctuation stays
  significant, so `AC/DC` and `AC DC` are distinct. One exact match is used directly. When several canonical
  exact names match, seakarr selects one only when it is uniquely dominant: a MusicBrainz search score of 100
  with a lead of at least 10 points over the runner-up. Tied top scores, a missing score, a score below 100,
  and a margin below 10 stay unresolved and fall back as described below. Aliases, sort names, artist tags,
  and catalog size never participate. Use `discography.artist_mbids` to pin an ambiguous name to a MusicBrainz
  artist UUID; a configured ID always takes precedence over name search.
```

- [ ] **Step 2: Lint the documentation**

Run: `markdownlint --fix README.md && markdownlint README.md`
Expected: exit 0, no output.

- [ ] **Step 3: Confirm the claim matches the code**

Run: `rg -n 'score of 100|at least 10|Aliases, sort names' README.md`
Expected: the new bullet appears, and it names score 100, margin 10, and the excluded signals.

- [ ] **Step 4: Commit**

```bash
git add README.md
git commit -m "docs: describe dominant MusicBrainz artist resolution"
```

---

### Task 7: Final verification

**Files:** none (verification only)

- [ ] **Step 1: Format and lint**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
```

Expected: exit 0 for both.

- [ ] **Step 2: Full test suite**

```bash
cargo test --workspace
```

Expected: all suites pass with 0 failures.

- [ ] **Step 3: Pre-commit gate**

```bash
pre-commit run --all-files
```

Expected: all hooks pass.

- [ ] **Step 4: Confirm the acceptance criteria**

```bash
rg -n 'REQUIRED_TOP_SCORE|REQUIRED_SCORE_MARGIN' src/discography/mod.rs
rg -n 'fn dominance_|fn unique_exact_name_still_resolves|fn weak_or_incomplete' src/discography/mod.rs
rg -n 'fn artist_search_retains_scores|fn artist_search_rejects_out_of_range' src/discography/musicbrainz.rs
```

Expected: constants defined once, eight resolver tests present, two provider tests present.

- [ ] **Step 5: Confirm no scope creep**

```bash
git diff --stat origin/main..HEAD
```

Expected: only `src/discography/mod.rs`, `src/discography/musicbrainz.rs`, `src/runner.rs`, `README.md`, and the plan/spec documents.

- [ ] **Step 6: Report**

No commit is needed for this task unless Steps 1-3 changed files; if `cargo fmt` or `markdownlint --fix` modified anything, commit it as:

```bash
git add -A
git commit -m "chore: format after dominant artist resolution"
```

---

## Self-Review

### Spec coverage

| Spec requirement | Task |
| --- | --- |
| Retain artist-search score in the provider model | 1, 2 |
| Canonical exact-name filtering unchanged | 3 |
| Unique score-100 candidate with margin >= 10 | 3 |
| Missing/invalid scores fail closed | 2, 3 |
| Response order never decides | 3 (`dominance_is_independent_of_response_order`) |
| Aliases and descriptive metadata cannot win | 3 (`non_exact_higher_score_cannot_win`) |
| No extra MusicBrainz requests | 4, 5 (call-count assertions) |
| INFO evidence for automatic selection | 4 |
| Unresolved reasons are explicit and distinct | 3, 5 |
| Configured MBID, fresh cache, stale cache, legacy precedence preserved | 5 |
| Selected MBID drives release-group retrieval and cache write | 5 |
| README documents the behaviour | 6 |
| No config key or migration | 1-7 (no config/db files touched) |
| Full gates pass | 7 |

No spec section is left without a task.

### Placeholder scan

No `TBD`, `TODO`, or "implement later" text. Every code step contains the code to write, and every command states its expected result.

### Type consistency

- `ArtistCandidate.score: Option<u8>` is defined in Task 1 and used identically in Tasks 2-5.
- `resolve_artist(artist_key: &str, candidates: &[ArtistCandidate]) -> Result<ResolvedArtist, DiscographyError>` is defined in Task 3, called as `resolve_artist(...)?.candidate` in Task 3 Step 6, and used by tests in Tasks 3-5.
- `ArtistResolution::ScoreDominance(DominanceEvidence)` and the `exact_matches`/`top_score`/`runner_up_score`/`margin` field names are identical in the Task 3 definition, the Task 3 tests, and the Task 4 log statement.
- `REQUIRED_TOP_SCORE` and `REQUIRED_SCORE_MARGIN` are defined once in Task 3 and referenced only from `resolve_dominant_artist`.
- Test helpers `scored(...)`, `ils_candidates()`, `cache_groups(...)`, `group(...)`, `FakeProvider`, and `CapturingWriter` all already exist or are introduced exactly once.
