<!-- markdownlint-disable MD013 -->

# Artist identification by IPI, ISNI or MusicBrainz ID — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use sub-agents (recommended) to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let `--artist` carry an exact MusicBrainz identity — an ISNI, an IPI or an artist UUID — detected from the value's shape, so an artist whose name cannot be resolved is still fetched exactly.

**Architecture:** A new pure `src/discography/identifier.rs` classifies and validates the CLI value and holds the identity resolver. The provider trait gains one search method for IPI/ISNI (query syntax stays inside `musicbrainz.rs`); UUIDs reuse the existing `artist_by_id` entity lookup. `discover_artist_albums_at` branches on the classified input for its cache key, pin lookup and provider call, and reports the resolved canonical name on its authoritative outcomes. The runner uses that canonical name as the work item's name for an identifier input, and refuses a code that matches no single carrier instead of falling back to the Soulseek heuristic.

**Tech Stack:** Rust 2021, `async-trait`, `serde`/`serde_json`, `reqwest` (via `MusicBrainzProvider`), `thiserror`, `wiremock` + `tokio::test` for provider tests, `cargo-llvm-cov` for coverage.

**Spec:** `docs/agent/specs/2026-10-07-artist-identifier-resolution-design.md`

**Verified external facts this plan relies on** (probed live on 2026-10-07, see the spec): MusicBrainz artist search accepts `isni:"…"` and `ipi:"…"` query terms; the response carries `ipis`, `isnis` and `disambiguation`; `/ws/2/artist/<mbid>` does not carry `ipis`/`isnis`. The motivating codes are IPI `00581977202` and ISNI `0000 0004 6659 7022`, both resolving to `Grafix`, MBID `c02a5966-b5c4-483a-8326-483edcf3680e`.

---

## File structure

| File | Responsibility | Change |
| --- | --- | --- |
| `src/discography/identifier.rs` | Classify a `--artist` value, validate a code, build the cache key, resolve a code against candidates | **Create** |
| `src/discography/mod.rs` | Declare the new module; carry identifiers on candidates; branch discovery on the classified input; report the canonical name | Modify |
| `src/discography/musicbrainz.rs` | Build `isni:`/`ipi:` queries; parse `ipis`/`isnis`/`disambiguation` | Modify |
| `src/runner.rs` | Use the canonical name for an identifier input; refuse a failed identifier in manual mode | Modify |
| `src/mode.rs` | Reject an identifier for an album-only manual run | Modify |
| `src/main.rs` | Reject an identifier in a batch line; update `--artist` help | Modify |
| `src/config.rs` | Validate `artist_mbids` keys with the shared UUID check | Modify |
| `README.md` | Document the four accepted shapes and the resolution log line | Modify |
| `docs/agent/specs/2026-09-15-musicbrainz-library-gap-filling-design.md`, `docs/agent/specs/2026-09-13-dominant-musicbrainz-artist-resolution-design.md` | Dated supersession notes | Modify |

No new configuration keys, no scanner change, no sweep support (identifiers are classified only where an explicit `--artist` is read).

---

## Task 1: The identifier module — classification, validation, cache key

**Files:**

- Create: `src/discography/identifier.rs`
- Modify: `src/discography/mod.rs:11-13` (declare `mod identifier;` and re-export its types)

- [ ] **Step 1: Write the failing tests**

Append to the new `src/discography/identifier.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn code(value: &str) -> Option<CodeIdentifier<'_>> {
        ArtistInput::parse(value).code()
    }

    #[test]
    fn an_eleven_digit_value_is_an_ipi() {
        assert_eq!(code("00581977202"), Some(CodeIdentifier::Ipi("00581977202")));
    }

    #[test]
    fn a_sixteen_digit_value_is_an_isni_with_spaces_and_hyphens_removed() {
        assert_eq!(
            code("0000 0004 6659 7022"),
            Some(CodeIdentifier::Isni("0000000466597022"))
        );
        assert_eq!(
            code("0000-0004-6659-7022"),
            Some(CodeIdentifier::Isni("0000000466597022"))
        );
    }

    #[test]
    fn a_lower_case_isni_check_character_is_upper_cased() {
        assert_eq!(
            code("000000012345678x"),
            Some(CodeIdentifier::Isni("000000012345678X"))
        );
    }

    #[test]
    fn a_hyphenated_uuid_is_a_musicbrainz_id() {
        let input = ArtistInput::parse("c02a5966-b5c4-483a-8326-483edcf3680e");
        assert_eq!(
            input,
            ArtistInput::Mbid("c02a5966-b5c4-483a-8326-483edcf3680e".to_string())
        );
        assert!(input.code().is_none(), "an MBID is looked up, not searched");
    }

    #[test]
    fn a_name_stays_a_name() {
        assert_eq!(
            ArtistInput::parse("Grafix"),
            ArtistInput::Name("Grafix".to_string())
        );
        assert_eq!(
            ArtistInput::parse("  Grafix  "),
            ArtistInput::Name("  Grafix  ".to_string()),
            "names keep their spelling; trimming stays with select_artists"
        );
    }

    #[test]
    fn a_band_named_like_a_code_shape_is_read_as_that_code() {
        // Documented accepted consequence: the shape wins over the name.
        assert_eq!(code("12345678901"), Some(CodeIdentifier::Ipi("12345678901")));
    }

    #[test]
    fn a_wrong_length_digit_string_is_a_name() {
        assert_eq!(code("1234567890"), None);
        assert_eq!(code("123456789012"), None);
        assert_eq!(code("123456789012345"), None);
        assert_eq!(code("12345678901234567"), None);
    }

    #[test]
    fn a_sixteen_character_value_with_a_letter_inside_is_a_name() {
        assert_eq!(code("0000000a66597022"), None);
    }

    #[test]
    fn a_bad_isni_check_digit_is_rejected_by_validation() {
        let input = ArtistInput::parse("0000000466597023");
        assert_eq!(
            input.validate(),
            Err("ISNI \"0000000466597023\" has an invalid check character".to_string())
        );
    }

    #[test]
    fn the_motivating_isni_and_ipi_validate() {
        assert_eq!(ArtistInput::parse("0000 0004 6659 7022").validate(), Ok(()));
        assert_eq!(ArtistInput::parse("00581977202").validate(), Ok(()));
    }

    #[test]
    fn a_uuid_with_a_letter_outside_hexadecimal_is_a_name_not_a_rejected_code() {
        // Only a shape that claims to be a code can fail validation.
        assert_eq!(
            ArtistInput::parse("c02a5966-b5c4-483a-8326-483edcf3680g"),
            ArtistInput::Name("c02a5966-b5c4-483a-8326-483edcf3680g".to_string())
        );
    }

    #[test]
    fn a_blank_name_fails_validation() {
        assert_eq!(
            ArtistInput::parse("   ").validate(),
            Err("artist \"   \" is blank".to_string())
        );
    }

    #[test]
    fn cache_keys_separate_names_codes_and_ids() {
        assert_eq!(ArtistInput::parse("Grafix").cache_key(), "grafix");
        assert_eq!(
            ArtistInput::parse("0000 0004 6659 7022").cache_key(),
            "isni:0000000466597022"
        );
        assert_eq!(
            ArtistInput::parse("00581977202").cache_key(),
            "ipi:00581977202"
        );
        assert_eq!(
            ArtistInput::parse("C02A5966-B5C4-483A-8326-483EDCF3680E").cache_key(),
            "mbid:c02a5966-b5c4-483a-8326-483edcf3680e"
        );
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib discography::identifier 2>&1 | tail -20`
Expected: FAIL — `error[E0433]: failed to resolve: use of undeclared crate or module` (the module does not exist yet).

- [ ] **Step 3: Write the module**

Create `src/discography/identifier.rs` with this content above the test module:

```rust
//! Identifying a MusicBrainz artist from a `--artist` value.
//!
//! An explicit `--artist` may be a name, an ISNI, an IPI or a MusicBrainz UUID.
//! The value's shape decides, and the shape rule is deliberately narrow: only a
//! value that is *entirely* one of the code shapes is treated as a code, so an
//! ordinary name is never reinterpreted unless it is spelled exactly like a code.
//! A band named `12345678901` is therefore read as an IPI; `discography.artist_mbids`
//! keyed by the same string is the way to pin it to an artist instead.

use serde::{Deserialize, Serialize};

use super::{normalize_catalog_key, resolve_artist_by_code, ArtistCandidate, DiscographyError, ResolvedArtist};

/// What an explicit `--artist` value means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArtistInput {
    /// An artist name, kept as typed.
    Name(String),
    /// A 16-character ISNI, normalised: digits with an optional trailing `X`.
    Isni(String),
    /// An 11-digit IPI.
    Ipi(String),
    /// A hyphenated MusicBrainz artist UUID, lower-cased.
    Mbid(String),
}

/// A code that has to be searched for in the MusicBrainz artist index.
///
/// The MBID is deliberately absent: an ID is fetched with the entity lookup
/// instead, so a search for one cannot be expressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodeIdentifier<'a> {
    Isni(&'a str),
    Ipi(&'a str),
}

impl CodeIdentifier<'_> {
    /// The query term MusicBrainz expects for this code.
    pub(crate) fn query(&self) -> String {
        match self {
            Self::Isni(value) => format!("isni:\"{value}\""),
            Self::Ipi(value) => format!("ipi:\"{value}\""),
        }
    }

    /// The human name of the code, for messages.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Isni(_) => "ISNI",
            Self::Ipi(_) => "IPI",
        }
    }

    /// The normalised code value.
    pub fn value(&self) -> &str {
        match self {
            Self::Isni(value) | Self::Ipi(value) => value,
        }
    }

    /// True when `candidate` claims this code in its own identifier list.
    ///
    /// Comparison is on the normalised forms, so spacing and an ISNI's letter
    /// case cannot make MusicBrainz's own record look like a different code.
    pub fn matches(&self, candidate: &ArtistCandidate) -> bool {
        let claimed = match self {
            Self::Isni(_) => &candidate.isnis,
            Self::Ipi(_) => &candidate.ipis,
        };
        claimed.iter().any(|code| {
            let normalised = normalise_code(code);
            normalised.eq_ignore_ascii_case(self.value())
        })
    }
}

impl ArtistInput {
    /// Classify an explicit `--artist` value by its shape.
    ///
    /// Total: a value that is not a code shape stays a name, so classification
    /// never silently drops an artist. Shape mistakes are reported by
    /// [`ArtistInput::validate`].
    pub fn parse(raw: &str) -> Self {
        let stripped: String = raw
            .chars()
            .filter(|character| !character.is_whitespace() && *character != '-')
            .collect();
        if valid_mbid(raw) {
            return Self::Mbid(raw.to_ascii_lowercase());
        }
        if is_isni_shape(&stripped) {
            return Self::Isni(stripped.to_ascii_uppercase());
        }
        if stripped.len() == IPI_LENGTH && stripped.chars().all(|c| c.is_ascii_digit()) {
            return Self::Ipi(stripped);
        }
        Self::Name(raw.to_string())
    }

    /// Wrap a library-derived artist name. The sweep never classifies: a name it
    /// read from a tag or folder is a name even when it is spelled like a code.
    pub fn name(value: impl Into<String>) -> Self {
        Self::Name(value.into())
    }

    /// The code to search for, when this input is one.
    pub fn code(&self) -> Option<CodeIdentifier<'_>> {
        match self {
            Self::Isni(value) => Some(CodeIdentifier::Isni(value)),
            Self::Ipi(value) => Some(CodeIdentifier::Ipi(value)),
            Self::Name(_) | Self::Mbid(_) => None,
        }
    }

    /// The MusicBrainz ID to look up, when this input is one.
    pub fn mbid(&self) -> Option<&str> {
        match self {
            Self::Mbid(value) => Some(value),
            _ => None,
        }
    }

    /// Reject a value that claims to be a code but cannot be used as one.
    ///
    /// A name is only checked for emptiness: a name that carries nothing names
    /// nothing, and `select_artists` applies the same rule.
    pub fn validate(&self) -> std::result::Result<(), String> {
        match self {
            Self::Name(value) if value.trim().is_empty() => {
                Err(format!("artist {value:?} is blank"))
            }
            Self::Name(_) => Ok(()),
            Self::Ipi(_) => Ok(()),
            Self::Mbid(_) => Ok(()),
            Self::Isni(value) => {
                if isni_check_character_is_valid(value) {
                    Ok(())
                } else {
                    Err(format!("ISNI {value:?} has an invalid check character"))
                }
            }
        }
    }

    /// The key for the discography and failure caches.
    ///
    /// Prefixed for a code or an ID so the three spaces cannot collide, and so a
    /// configured `discography.artist_mbids` key can be written as `isni:<digits>`
    /// to pin a code that several artists share.
    pub fn cache_key(&self) -> String {
        match self {
            Self::Name(value) => normalize_catalog_key(value),
            Self::Isni(value) => format!("isni:{value}"),
            Self::Ipi(value) => format!("ipi:{value}"),
            Self::Mbid(value) => format!("mbid:{value}"),
        }
    }

    /// True when this input names an exact MusicBrainz identity.
    pub fn is_identity(&self) -> bool {
        !matches!(self, Self::Name(_))
    }

    /// A short label for messages: the code shape, or `name`.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Name(_) => "artist",
            Self::Isni(_) => "ISNI",
            Self::Ipi(_) => "IPI",
            Self::Mbid(_) => "MusicBrainz ID",
        }
    }
}

/// ISNI length, including its check character.
const ISNI_LENGTH: usize = 16;
/// IPI length. IPIs carry no check digit.
const IPI_LENGTH: usize = 11;

/// Strip the separators MusicBrainz and printed ISNIs use.
fn normalise_code(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_whitespace() && *character != '-')
        .collect()
}

/// True when `value` is 16 characters of digits with an optional trailing `X`.
fn is_isni_shape(value: &str) -> bool {
    if value.len() != ISNI_LENGTH {
        return false;
    }
    let (body, last) = value.split_at(ISNI_LENGTH - 1);
    body.chars().all(|c| c.is_ascii_digit()) && (last.eq_ignore_ascii_case("x") || last.chars().all(|c| c.is_ascii_digit()))
}

/// ISO 7064 MOD 11-2, the ISNI check-character algorithm.
///
/// The body is processed from the left with a running remainder of 0; each
/// character is worth its digit, or 10 for a leading `X`. The check character is
/// valid when the final remainder is 1.
fn isni_check_character_is_valid(value: &str) -> bool {
    let mut characters = value.chars();
    let mut remainder: u32 = 0;
    for character in characters.by_ref().take(ISNI_LENGTH - 1) {
        let Some(digit) = character.to_digit(10) else {
            return false;
        };
        remainder = (remainder + digit) * 2 % 11;
    }
    let Some(check) = characters.next() else {
        return false;
    };
    let expected = 12 - remainder;
    let expected = if expected == 11 { 0 } else { expected };
    match check {
        'X' | 'x' => expected == 10,
        other => other.to_digit(10) == Some(expected),
    }
}

/// True when `value` is a hyphenated MusicBrainz artist UUID.
///
/// The same shape `discography.artist_mbids` has always accepted.
pub(crate) fn valid_mbid(value: &str) -> bool {
    const GROUPS: [usize; 5] = [8, 4, 4, 4, 12];
    let parts: Vec<&str> = value.split('-').collect();
    parts.len() == GROUPS.len()
        && parts
            .iter()
            .zip(GROUPS)
            .all(|(part, length)| part.len() == length && part.chars().all(|c| c.is_ascii_hexdigit()))
}

/// Resolve a code against the artists MusicBrainz returned for it.
///
/// Only a candidate whose own identifier list contains the code counts, so a
/// fuzzy search hit that does not carry it is discarded rather than trusted on
/// its relevance score.
pub fn resolve_code(
    code: CodeIdentifier<'_>,
    candidates: &[ArtistCandidate],
) -> std::result::Result<ResolvedArtist, DiscographyError> {
    resolve_artist_by_code(code, candidates)
}
```

Then declare the module and re-export in `src/discography/mod.rs`, replacing lines 11-13:

```rust
mod bundle;
pub mod identifier;
mod musicbrainz;
pub use identifier::{ArtistInput, CodeIdentifier};
pub use musicbrainz::MusicBrainzProvider;
```

And in `src/discography/mod.rs`, add the resolver next to the existing `resolve_artist` (after it, around line 215):

```rust
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
            "{} artists carry {} {} ({}); pin the MusicBrainz ID with \
             discography.artist_mbids or the UUID form of --artist",
            many.len(),
            code.kind(),
            code.value(),
            many.iter()
                .map(|candidate| format!("{} ({})", candidate.name, candidate.id))
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib discography::identifier 2>&1 | tail -20`
Expected: PASS — 12 tests, plus two compile errors to fix first in the same step: `ArtistCandidate` has no `ipis`/`isnis` field yet, so add them now (Task 2 does the parsing; this step only adds the fields):

```rust
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
```

and add the new variant to `ArtistResolution`:

```rust
pub enum ArtistResolution {
    /// Exactly one candidate matched the requested canonical name.
    UniqueExactName,
    /// Duplicate exact names resolved by a dominant search score.
    ScoreDominance(DominanceEvidence),
    /// Exactly one candidate carried the requested IPI or ISNI.
    Identity,
}
```

Fix every `ArtistCandidate { … }` literal the compiler reports by adding the three new fields — the sites are `src/discography/musicbrainz.rs:285` and `:311`, `src/discography/mod.rs:1034, 1139, 1144, 1158, 1163, 1752, 1773, 1829, 2202`, and `src/runner.rs:5242` and `:5257`. For the name-search site in `musicbrainz.rs:285` use the parsed wire fields; everywhere else use `Vec::new()`, `Vec::new()`, `None`.

- [ ] **Step 5: Commit**

```bash
git add src/discography/identifier.rs src/discography/mod.rs src/discography/musicbrainz.rs src/runner.rs
git commit -m "feat: classify an explicit artist value as a name, code or MusicBrainz ID"
```

---

## Task 2: Parse the identifiers MusicBrainz returns

**Files:**

- Modify: `src/discography/musicbrainz.rs:20-33` (wire struct), `:282-290` (candidate mapping)
- Test: `src/discography/musicbrainz.rs` test module

- [ ] **Step 1: Write the failing test**

Add to the `musicbrainz.rs` test module, next to `artist_search_sends_identity_headers_and_encoded_query`:

```rust
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

        let provider = MusicBrainzProvider::for_test(&server.uri());
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

        let provider = MusicBrainzProvider::for_test(&server.uri());
        let candidates = provider.search_artists("AC/DC").await.unwrap();

        assert!(candidates[0].ipis.is_empty());
        assert!(candidates[0].isnis.is_empty());
        assert_eq!(candidates[0].disambiguation, None);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib musicbrainz::tests::artist_search_parses_the_identifiers_and_disambiguation 2>&1 | tail -20`
Expected: FAIL — assertion failure showing `ipis` is empty, because the wire struct does not parse it yet.

- [ ] **Step 3: Extend the wire struct and the mapping**

Replace `ArtistWire` in `src/discography/musicbrainz.rs`:

```rust
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
```

and the candidate mapping in `search_artists`:

```rust
            .map(|wire| ArtistCandidate {
                id: wire.id,
                name: wire.name,
                score: wire.score,
                ipis: wire.ipis,
                isnis: wire.isnis,
                disambiguation: wire.disambiguation,
            })
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib musicbrainz 2>&1 | tail -20`
Expected: PASS — the two new tests plus every existing provider test.

- [ ] **Step 5: Commit**

```bash
git add src/discography/musicbrainz.rs
git commit -m "feat: carry MusicBrainz IPI, ISNI and disambiguation on artist candidates"
```

---

## Task 3: Search MusicBrainz by code

**Files:**

- Modify: `src/discography/mod.rs:135-150` (trait), `src/discography/musicbrainz.rs` (implementation), `src/runner.rs:5230-5260` (the runner's fake provider)
- Test: `src/discography/musicbrainz.rs` test module

- [ ] **Step 1: Write the failing test**

Add to the `musicbrainz.rs` test module:

```rust
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

        let provider = MusicBrainzProvider::for_test(&server.uri());
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

        let provider = MusicBrainzProvider::for_test(&server.uri());
        let error = provider
            .search_artists_by_code(CodeIdentifier::Ipi("00581977202"))
            .await
            .unwrap_err();

        assert!(
            matches!(error, DiscographyError::IncompletePagination(_)),
            "an identifier search classifies a wrong page exactly as a name search does, got {error:?}"
        );
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib musicbrainz::tests::code_search 2>&1 | tail -20`
Expected: FAIL — `error[E0599]: no method named search_artists_by_code found`.

- [ ] **Step 3: Add the trait method and the implementation**

In `src/discography/mod.rs`, add to `DiscographyProvider` after `search_artists`:

```rust
    /// Search the MusicBrainz artist index for the artist claiming a code.
    ///
    /// Returns the raw candidates, in the provider's order; the caller decides
    /// whether exactly one of them really carries the code.
    async fn search_artists_by_code(
        &self,
        code: CodeIdentifier<'_>,
    ) -> std::result::Result<Vec<ArtistCandidate>, DiscographyError>;
```

In `src/discography/musicbrainz.rs`, add to `impl MusicBrainzProvider` next to `search_artists`:

```rust
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
        Ok(page.artists.into_iter().map(ArtistCandidate::from).collect())
    }
```

Extract the shared page checks from `search_artists` into one helper so both searches cannot drift:

```rust
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
```

Replace the body of `search_artists` with a call to the same helper and a shared conversion:

```rust
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
        Ok(page.artists.into_iter().map(ArtistCandidate::from).collect())
    }
```

and add the conversion once, next to `ArtistWire`:

```rust
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
```

The forwarding implementation at `musicbrainz.rs:397-402` gains the sibling:

```rust
    async fn search_artists_by_code(
        &self,
        code: CodeIdentifier<'_>,
    ) -> std::result::Result<Vec<ArtistCandidate>, DiscographyError> {
        self.search_artists_by_code(code).await
    }
```

Finally, teach the runner's `FakeDiscographyProvider` the new method so the suite compiles. Add a field and an implementation in `src/runner.rs`:

```rust
        /// Candidates returned for any code search, and the codes asked about.
        code_candidates: Option<Vec<ArtistCandidate>>,
        requested_codes: Arc<std::sync::Mutex<Vec<String>>>,
```

```rust
        async fn search_artists_by_code(
            &self,
            code: CodeIdentifier<'_>,
        ) -> std::result::Result<Vec<ArtistCandidate>, DiscographyError> {
            self.artist_calls.fetch_add(1, Ordering::SeqCst);
            if let Ok(mut requested) = self.requested_codes.lock() {
                requested.push(format!("{}:{}", code.kind(), code.value()));
            }
            if let Some(reason) = &self.failure {
                return Err(DiscographyError::Transport(reason.clone()));
            }
            Ok(self.code_candidates.clone().unwrap_or_default())
        }
```

and initialise both in the three existing constructors (`with_groups`, `failing`, `unresolvable`) as `code_candidates: None` and `requested_codes: Arc::new(std::sync::Mutex::new(Vec::new()))`, plus a builder used by Task 10's tests:

```rust
        fn with_code_candidates(mut self, candidates: Vec<ArtistCandidate>) -> Self {
            self.code_candidates = Some(candidates);
            self
        }

        fn requested_codes(&self) -> Vec<String> {
            self.requested_codes
                .lock()
                .map(|codes| codes.clone())
                .unwrap_or_default()
        }
```

Do the same for the discography test fake at `src/discography/mod.rs:1728` (`FakeProvider`): add `code_responses: VecDeque<Result<Vec<ArtistCandidate>, DiscographyError>>` next to `artist_responses`, initialise it empty in its constructors, and implement `search_artists_by_code` by popping from it exactly as `search_artists` pops from `artist_responses`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib musicbrainz 2>&1 | tail -20`
Expected: PASS for the two new tests. Then run `cargo test --lib 2>&1 | tail -5` and fix any fake-provider compile errors it reports until the whole lib target compiles.

- [ ] **Step 5: Commit**

```bash
git add src/discography/mod.rs src/discography/musicbrainz.rs src/runner.rs
git commit -m "feat: search the MusicBrainz artist index by IPI or ISNI"
```

---

## Task 4: Resolve a code to exactly one carrier

**Files:**

- Modify: `src/discography/mod.rs` (tests only; `resolve_code` landed in Task 1)
- Test: `src/discography/mod.rs` test module

- [ ] **Step 1: Write the failing tests**

Add to the `mod tests` block in `src/discography/mod.rs` (the file's `scored` helper at line 1033 builds candidates; give it the identifiers this task needs by assigning the new fields in each test):

```rust
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
            carrier("11111111-1111-1111-1111-111111111111", "Grafix", &[], &["0000000466597022"]),
            carrier("22222222-2222-2222-2222-222222222222", "Other", &[], &[]),
        ];

        let resolved =
            resolve_code(CodeIdentifier::Isni("0000000466597022"), &candidates).unwrap();

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
            error.to_string().contains("no MusicBrainz artist carries ISNI 0000000466597022"),
            "got {error}"
        );
    }

    #[test]
    fn a_code_with_no_carrier_names_the_code() {
        let error = resolve_code(CodeIdentifier::Ipi("00581977202"), &[]).unwrap_err();
        assert!(
            error.to_string().contains("no MusicBrainz artist carries IPI 00581977202"),
            "got {error}"
        );
    }

    #[test]
    fn a_shared_code_names_the_count_and_every_carrier() {
        let candidates = vec![
            carrier("11111111-1111-1111-1111-111111111111", "Grafix", &["00581977202"], &[]),
            carrier("22222222-2222-2222-2222-222222222222", "Grafix Band", &["00581977202"], &[]),
        ];

        let error = resolve_code(CodeIdentifier::Ipi("00581977202"), &candidates).unwrap_err();
        let message = error.to_string();

        assert!(message.contains("2 artists carry IPI 00581977202"), "got {message}");
        assert!(message.contains("Grafix (11111111-1111-1111-1111-111111111111)"), "got {message}");
        assert!(message.contains("Grafix Band (22222222-2222-2222-2222-222222222222)"), "got {message}");
        assert!(message.contains("discography.artist_mbids"), "the message must point at the fix: {message}");
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
        // The failure cache remembers this and the circuit breaker ignores it.
        let error = resolve_code(CodeIdentifier::Isni("0000000466597022"), &[]).unwrap_err();
        assert!(matches!(error, DiscographyError::ArtistUnresolved(_)), "got {error:?}");
        assert_eq!(DiscoveryFailure::from_error(&error), DiscoveryFailure::Unresolved);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib resolve_code 2>&1 | tail -20`
Expected: FAIL — `cannot find function resolve_code in this scope` until the test module imports it (`use super::*;` already covers the re-export), otherwise the first failing assertion.

- [ ] **Step 3: Make them pass**

No new production code should be needed: `resolve_code` and `resolve_artist_by_code` were written in Task 1. If a test fails, fix `resolve_artist_by_code` in `src/discography/mod.rs` — not the test.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib resolve_code 2>&1 | tail -10`
Expected: PASS — 6 tests.

- [ ] **Step 5: Commit**

```bash
git add src/discography/mod.rs
git commit -m "test: pin exactly one code carrier and the refusal reasons"
```

---

## Task 5: Branch discovery on the classified input

**Files:**

- Modify: `src/discography/mod.rs:659-790` (`discover_artist_albums`, `discover_artist_albums_at`, `select_outcome`, `stale_or_legacy`), `:634-649` (`DiscoveryOutcome`)
- Test: `src/discography/mod.rs` test module

- [ ] **Step 1: Write the failing tests**

Add to the `mod tests` block in `src/discography/mod.rs`:

```rust
    #[tokio::test]
    async fn an_isni_input_is_searched_by_code_and_reports_the_canonical_name() {
        let provider = FakeProvider::with_code(
            vec![carrier("c02a5966-b5c4-483a-8326-483edcf3680e", "Grafix", &[], &["0000000466597022"])],
            vec![group("rg-1", "Half Light", Some("2019"), Some("Album"), &[])],
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

        assert_eq!(provider.requested_codes(), vec!["ISNI:0000000466597022".to_string()]);
        assert_eq!(
            discovery.canonical_name.as_deref(),
            Some("Grafix"),
            "the runner needs the canonical name to search and place"
        );
        assert!(matches!(discovery.outcome, DiscoveryOutcome::Authoritative { .. }));
        assert_eq!(
            db.get_discography_cache("isni:0000000466597022").unwrap().map(|row| row.canonical_artist),
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

        assert_eq!(provider.search_calls(), 0, "an ID must not go through the search index");
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
        let provider = FakeProvider::with_search(vec![scored("11111111-1111-1111-1111-111111111111", "Grafix", Some(100))]);
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
            db.get_discography_cache("grafix").unwrap().map(|row| row.canonical_artist),
            Some("Grafix".to_string())
        );
        assert_eq!(discovery.canonical_name.as_deref(), Some("Grafix"));
    }
```

`FakeProvider` in the discography test module needs the three helpers used above (`with_code`, `with_id`, `with_search`), a `search_calls()` counter, a `requested_codes()` log and a `requested_artists()` log; add them next to the existing `group`/`scored` helpers in that module so the tests read cleanly. The module's own release-group builder is `group(id, title, date, primary, secondary)` — no new builder is needed for the tests below.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib discography::tests::an_isni_input_is_searched_by_code_and_reports_the_canonical_name 2>&1 | tail -20`
Expected: FAIL — `error[E0308]`/`E0061`: `discover_artist_albums_at` takes a `&str`, not an `&ArtistInput`, and `Discovery` has no `canonical_name`.

- [ ] **Step 3: Branch on the input and report the canonical name**

In `src/discography/mod.rs`:

- Change the outcome variants to carry the canonical spelling:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscoveryOutcome {
    Authoritative {
        albums: Vec<AlbumTarget>,
        provenance: DiscoveryProvenance,
        /// The canonical MusicBrainz spelling of the resolved artist, for callers
        /// that must name the work item after a code-only request.
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
```

- Add the wrapper the callers will use:

```rust
/// The outcome of resolving one artist, with the identity it resolved to.
///
/// The canonical name is separate from the outcome because only some variants
/// have one: a legacy fallback resolved nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discovery {
    pub outcome: DiscoveryOutcome,
    pub canonical_name: Option<String>,
}
```

- Change `discover_artist_albums` / `discover_artist_albums_at` to take `input: &ArtistInput` and to return `Discovery`. The provider call inside becomes a three-way branch:

```rust
    let refresh = async {
        let resolved = match (input.mbid(), input.code(), configured_mbid) {
            // A pinned UUID wins over every shape, exactly as it does for a name.
            (_, _, Some(mbid)) => provider.artist_by_id(mbid).await?,
            (Some(mbid), _, None) => provider.artist_by_id(mbid).await?,
            (None, Some(code), None) => {
                let candidates = provider.search_artists_by_code(code).await?;
                resolve_artist_by_code(code, &candidates)?
            }
            (None, None, None) => {
                let name = match input {
                    ArtistInput::Name(name) => name.as_str(),
                    _ => unreachable!("every non-name input is an MBID or a code"),
                };
                let candidates = provider.search_artists(name).await?;
                resolve_artist(&artist_key, &candidates)?
            }
        };
        // ... the existing delete_discography_failure / release_groups tail ...
        Ok::<_, DiscographyError>((resolved, groups))
    }
    .await;
```

The `artist_key` for the caches becomes `input.cache_key()`, and the name handed to `select_albums` stays the input as typed:

```rust
    let artist = input.display();
    let artist_key = input.cache_key();
```

- Thread the canonical name through the outcome builders:

```rust
fn select_outcome(
    groups: &[ReleaseGroup],
    allowed: &[DiscographyReleaseType],
    artist: &str,
    provenance: DiscoveryProvenance,
    canonical_name: Option<&str>,
) -> DiscoveryOutcome {
    let album_list = select_albums(groups, allowed, artist);
    let canonical_name = canonical_name.map(str::to_string);
    if album_list.is_empty() {
        DiscoveryOutcome::AuthoritativeEmpty { provenance, canonical_name }
    } else {
        DiscoveryOutcome::Authoritative { albums: album_list, provenance, canonical_name }
    }
}
```

and pass it at each call site: the fresh-cache path uses `cached.entry.canonical_artist`, the refresh path uses `resolved.name`, the stale path uses `cached.entry.canonical_artist`. `stale_or_legacy` gains the parameter and forwards it.

- Add `display()` to `ArtistInput` in `identifier.rs`:

```rust
    /// The value as the operator wrote it, for messages and for the album
    /// selection helpers that match on a spelling.
    pub fn display(&self) -> &str {
        match self {
            Self::Name(value)
            | Self::Isni(value)
            | Self::Ipi(value)
            | Self::Mbid(value) => value,
        }
    }
```

Then update every existing caller: `src/runner.rs` (`verify_named_artist`, the discover loop, and `run_artist_only_mode_with_provider`) and the existing tests in `src/discography/mod.rs` that call `discover_artist_albums_at(…, "Name", …)` → `…, &ArtistInput::name("Name"), …`, and read `.outcome` / `.canonical_name` where they previously used the outcome directly. The sweep in `src/runner.rs` must use `ArtistInput::name(artist.name.clone())` so a library spelling that happens to look like a code is never searched for by code.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib 2>&1 | tail -5`
Expected: PASS — the four new tests plus the whole existing library suite, with `cargo build` reporting no unused-variant warnings for the new enum.

- [ ] **Step 5: Commit**

```bash
git add src/discography/mod.rs src/discography/identifier.rs src/runner.rs
git commit -m "feat: resolve a code or MusicBrainz ID through the discovery path"
```

---

## Task 6: Make an identifier drive the discover work item

**Files:**

- Modify: `src/runner.rs:2540-2620` (pre-walk block and selection), `:2660-2690` (loop), `:2282-2320` (`verify_named_artist`)
- Test: `src/runner.rs` test module

- [ ] **Step 1: Write the failing tests**

Add to the runner test module:

```rust
    #[tokio::test]
    async fn discover_verifies_an_isni_before_scanning_the_library() {
        // The non-existent library root is the ordering proof: a run that walked
        // first would fail with Scanner instead of reaching the walk's failure
        // after the code was verified.
        let soulseek = MockClient::new();
        let (mut config, db, staging, _library) = discover_fixture(&[("Present Artist", "Present")]);
        undiscoverable_library(&mut config, &staging);
        let provider = FakeDiscographyProvider::with_groups(vec![release_group("missing", "Missing", "1999")])
            .with_code_candidates(vec![ArtistCandidate {
                id: "c02a5966-b5c4-483a-8326-483edcf3680e".to_string(),
                name: "Grafix".to_string(),
                score: Some(100),
                ipis: Vec::new(),
                isnis: vec!["0000000466597022".to_string()],
                disambiguation: None,
            }]);

        let error = run_discover_mode_with_provider(
            &soulseek,
            &config,
            &db,
            Some("0000 0004 6659 7022"),
            false,
            staging.path(),
            &provider,
        )
        .await
        .unwrap_err();

        assert_eq!(
            provider.requested_codes(),
            vec!["ISNI:0000000466597022".to_string()],
            "the code must be what MusicBrainz was asked about"
        );
        assert!(
            matches!(error, SeakarrError::Scanner(_)),
            "the code was verified first, so the walk is what fails, got {error:?}"
        );
    }

    #[tokio::test]
    async fn discover_hard_errors_when_no_artist_carries_the_code() {
        let soulseek = MockClient::new();
        let (mut config, db, staging, _library) = discover_fixture(&[("Present Artist", "Present")]);
        undiscoverable_library(&mut config, &staging);
        let provider = FakeDiscographyProvider::with_groups(Vec::new()).with_code_candidates(Vec::new());

        let error = run_discover_mode_with_provider(
            &soulseek,
            &config,
            &db,
            Some("00581977202"),
            false,
            staging.path(),
            &provider,
        )
        .await
        .unwrap_err();

        let message = error.to_string();
        assert!(matches!(error, SeakarrError::Config(_)), "got {error:?}");
        assert!(message.contains("no MusicBrainz artist carries IPI 00581977202"), "got {message}");
        assert!(soulseek.search_queries.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn discover_rejects_a_malformed_isni_before_any_request() {
        let soulseek = MockClient::new();
        let (mut config, db, staging, _library) = discover_fixture(&[("Present Artist", "Present")]);
        undiscoverable_library(&mut config, &staging);
        let provider = FakeDiscographyProvider::with_groups(Vec::new());

        let error = run_discover_mode_with_provider(
            &soulseek,
            &config,
            &db,
            Some("0000000466597023"),
            false,
            staging.path(),
            &provider,
        )
        .await
        .unwrap_err();

        assert_eq!(
            provider.calls(),
            0,
            "a bad check character must not reach MusicBrainz"
        );
        assert!(error.to_string().contains("invalid check character"), "got {error}");
    }

    #[tokio::test]
    async fn discover_places_a_code_run_under_the_canonical_name() {
        // The library folder is spelled the way the operator spells the artist;
        // the code resolves to the canonical MusicBrainz spelling, which is what
        // the search and the placement use.
        let soulseek = MockClient::new();
        search_index(&soulseek, "Grafix", &[("Grafix Missing", "Missing")]);
        *soulseek.write_files.lock().unwrap() = true;
        let (config, db, staging, library) = discover_fixture(&[("Grafix", "Present")]);
        let provider = FakeDiscographyProvider::with_groups(vec![release_group("missing", "Missing", "1999")])
            .with_code_candidates(vec![ArtistCandidate {
                id: "c02a5966-b5c4-483a-8326-483edcf3680e".to_string(),
                name: "Grafix".to_string(),
                score: Some(100),
                ipis: Vec::new(),
                isnis: vec!["0000000466597022".to_string()],
                disambiguation: None,
            }]);

        run_discover_mode_with_provider(
            &soulseek,
            &config,
            &db,
            Some("0000 0004 6659 7022"),
            false,
            staging.path(),
            &provider,
        )
        .await
        .unwrap();

        assert_eq!(
            soulseek.search_queries.lock().unwrap().as_slice(),
            ["Grafix Missing"],
            "the Soulseek search uses the resolved canonical name"
        );
        assert!(
            library
                .path()
                .join("Grafix")
                .join("Missing")
                .join("01 - track.flac")
                .exists(),
            "the album must be placed in the artist folder the canonical name resolved to"
        );
    }

    #[tokio::test]
    async fn discover_keeps_the_sweep_name_only() {
        // A library artist spelled like an IPI is still a name for the sweep: it
        // must not be searched for by code.
        let soulseek = MockClient::new();
        let (config, db, staging, _library) = discover_fixture(&[("12345678901", "Present")]);
        let provider = FakeDiscographyProvider::with_groups(Vec::new());

        run_discover_mode_with_provider(&soulseek, &config, &db, None, false, staging.path(), &provider)
            .await
            .unwrap();

        assert!(
            provider.requested_codes().is_empty(),
            "the sweep must never search by code, got {:?}",
            provider.requested_codes()
        );
        assert_eq!(provider.requested(), vec!["12345678901".to_string()]);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib discover_verifies_an_isni_before_scanning_the_library 2>&1 | tail -20`
Expected: FAIL — the code is passed to `search_artists` as a name, so `requested_codes()` is empty and the assertion fails.

- [ ] **Step 3: Classify at the boundary and use the canonical name**

In `src/runner.rs`, replace the pre-walk head of `run_discover_mode_with_provider`:

```rust
    // An explicit target is classified once: a name, or an exact MusicBrainz
    // identity (ISNI, IPI, MBID). A value that claims a code shape but cannot be
    // one is rejected here, before any request or walk.
    let explicit: Option<discography::ArtistInput> = artist_filter.map(discography::ArtistInput::parse);
    if let Some(input) = &explicit {
        input.validate().map_err(SeakarrError::Config)?;
    }
    // Names keep the name-shaped rule `select_artists` also applies; an identity
    // is already exact, so it neither needs nor can use that rule.
    let named_filter = match &explicit {
        Some(input @ discography::ArtistInput::Name(name)) => {
            let _ = input;
            Some(discover::validate_artist_filter(name)?)
        }
        _ => None,
    };
```

then, where the pre-walk verification runs, keep the resolved filter name:

```rust
    let mut filter_name: Option<String> = named_filter.map(str::to_string);
    let mut pre_resolved = match &explicit {
        Some(input) => match verify_named_artist(
            provider,
            db,
            config,
            input,
            progress.as_ref(),
            &mut report,
            &mut counters,
            &mut consecutive_provider_failures,
        )
        .await?
        {
            NamedArtistVerification::WorkList { outcome, filter_name: resolved } => {
                filter_name = Some(resolved);
                Some(outcome)
            }
            NamedArtistVerification::Stop => return Ok(()),
        },
        None => None,
    };
```

`NamedArtistVerification` becomes:

```rust
enum NamedArtistVerification {
    /// Work to do: the artist's authoritative outcome, and the name the selection
    /// and the folder lookup must use — the typed name for a name input, the
    /// resolved canonical name for an identity.
    WorkList {
        outcome: DiscoveryOutcome,
        filter_name: String,
    },
    /// No work list, so the run stops before the walk; the outage was reported.
    Stop,
}
```

and `verify_named_artist` takes the parsed input and returns that name:

```rust
async fn verify_named_artist(
    provider: &dyn DiscographyProvider,
    db: &Database,
    config: &Config,
    input: &discography::ArtistInput,
    progress: Option<&ProgressDisplay>,
    report: &mut RunReport,
    counters: &mut discover::DiscoverCounters,
    consecutive_provider_failures: &mut u32,
) -> Result<NamedArtistVerification> {
    let discovery = discover_artist_albums(
        provider,
        db,
        input,
        &config.discography,
        FailureCacheUse::Bypass,
    )
    .await;
    let label = input.display();
    let DiscoveryOutcome::LegacyFallback { reason, kind, .. } = &discovery.outcome else {
        let filter_name = discovery
            .canonical_name
            .clone()
            .unwrap_or_else(|| label.to_string());
        return Ok(NamedArtistVerification::WorkList {
            outcome: discovery.outcome,
            filter_name,
        });
    };
    match kind {
        DiscoveryFailure::Unresolved => Err(SeakarrError::Config(format!(
            "artist {label:?} could not be resolved on MusicBrainz ({reason}); \
             discover mode needs a verified artist to build its work list"
        ))),
        DiscoveryFailure::Provider => {
            if note_provider_failure(label, reason, counters, consecutive_provider_failures) {
                return Err(abort_discover_run(report, counters, progress, label));
            }
            finish_discover_run(report, counters, progress);
            Ok(NamedArtistVerification::Stop)
        }
    }
}
```

Then use `filter_name` where `artist_filter`/`validated_filter` were used for the selection and the abort label:

```rust
    let filter_folder = match filter_name.as_deref() {
        Some(name) if !index.has_artist(name) => discover::ArtistFolderIndex::new(config).find(name),
        _ => None,
    };
    let selection = discover::select_artists(
        &index,
        &config.discover.exclude_artists,
        filter_name.as_deref(),
        filter_folder.as_ref().map(|(root, artist_dir)| (root.as_path(), artist_dir.as_str())),
    )?;
```

and in the loop:

```rust
        let abort_artist = filter_name.as_deref().unwrap_or(artist.name.as_str());
```

The loop's sweep call becomes explicit about names:

```rust
                discover_artist_albums(
                    provider,
                    db,
                    &discography::ArtistInput::name(artist.name.clone()),
                    &config.discography,
                    failure_cache,
                )
                .await
                .outcome
```

and the two `DiscoveryOutcome::Authoritative`/`AuthoritativeEmpty` arms destructure the new field with `..`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib discover_ 2>&1 | tail -10`
Expected: PASS — the five new tests plus every existing discover test. Then `cargo test --lib 2>&1 | tail -5` for the rest.

- [ ] **Step 5: Commit**

```bash
git add src/runner.rs
git commit -m "feat: drive a discover work item from a code's canonical name"
```

---

## Task 7: Refuse a failed identifier in artist-only manual mode

**Files:**

- Modify: `src/runner.rs:1990-2110` (`run_artist_only_mode_with_provider`)
- Test: `src/runner.rs` test module

- [ ] **Step 1: Write the failing tests**

Add to the runner test module:

```rust
    #[tokio::test]
    async fn manual_artist_only_refuses_an_unresolved_code_instead_of_falling_back() {
        // The heuristic would search Soulseek for "00581977202". A code is an
        // exact request, so a miss is reported.
        let soulseek = MockClient::new();
        let provider = FakeDiscographyProvider::unresolvable();
        let (config, db, staging) = artist_only_fixture();

        let error = run_artist_only_mode_with_provider(
            &soulseek,
            "00581977202",
            false,
            &config,
            &db,
            staging.path(),
            None,
            &Arc::new(AtomicBool::new(false)),
            &provider,
        )
        .await
        .unwrap_err();

        assert!(matches!(error, SeakarrError::Config(_)), "got {error:?}");
        assert!(
            soulseek.search_queries.lock().unwrap().is_empty(),
            "no Soulseek search may be issued for a code that resolved to nothing"
        );
    }

    #[tokio::test]
    async fn manual_artist_only_refuses_a_code_when_the_provider_is_offline() {
        let soulseek = MockClient::new();
        let provider = FakeDiscographyProvider::failing("offline");
        let (config, db, staging) = artist_only_fixture();

        let error = run_artist_only_mode_with_provider(
            &soulseek,
            "0000 0004 6659 7022",
            false,
            &config,
            &db,
            staging.path(),
            None,
            &Arc::new(AtomicBool::new(false)),
            &provider,
        )
        .await
        .unwrap_err();

        assert!(
            matches!(error, SeakarrError::MusicBrainz(_)),
            "an outage for a code is a MusicBrainz error, not a heuristic fallback, got {error:?}"
        );
        assert!(soulseek.search_queries.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn manual_artist_only_still_falls_back_for_a_name() {
        // The existing behaviour, pinned so the code path above cannot leak into it.
        let soulseek = MockClient::new();
        soulseek
            .search_results_by_query
            .lock()
            .unwrap()
            .insert("Grafix".into(), vec![album_result("Grafix", "Half Light")]);
        let provider = FakeDiscographyProvider::unresolvable();
        let (config, db, staging) = artist_only_fixture();

        let run = run_artist_only_mode_with_provider(
            &soulseek,
            "Grafix",
            false,
            &config,
            &db,
            staging.path(),
            None,
            &Arc::new(AtomicBool::new(false)),
            &provider,
        )
        .await
        .unwrap();

        assert_eq!(soulseek.search_queries.lock().unwrap().as_slice(), ["Grafix"]);
        assert!(run.notice.unwrap().contains("heuristically"));
    }

    #[tokio::test]
    async fn manual_artist_only_searches_under_the_canonical_name_for_a_code() {
        let soulseek = MockClient::new();
        search_index(&soulseek, "Grafix", &[("Grafix Half Light", "Half Light")]);
        let provider = FakeDiscographyProvider::with_groups(vec![release_group("rg-1", "Half Light", "2019")])
            .with_code_candidates(vec![ArtistCandidate {
                id: "c02a5966-b5c4-483a-8326-483edcf3680e".to_string(),
                name: "Grafix".to_string(),
                score: Some(100),
                ipis: Vec::new(),
                isnis: vec!["0000000466597022".to_string()],
                disambiguation: None,
            }]);
        let (config, db, staging) = artist_only_fixture();

        let run = run_artist_only_mode_with_provider(
            &soulseek,
            "0000 0004 6659 7022",
            false,
            &config,
            &db,
            staging.path(),
            None,
            &Arc::new(AtomicBool::new(false)),
            &provider,
        )
        .await
        .unwrap();

        assert_eq!(
            soulseek.search_queries.lock().unwrap().as_slice(),
            ["Grafix Half Light"],
            "the album search uses the canonical name, not the code"
        );
        assert_eq!(run.outcomes.len(), 1, "the album was attempted");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib manual_artist_only_refuses_an_unresolved_code_instead_of_falling_back 2>&1 | tail -20`
Expected: FAIL — the run returns `Ok` after the heuristic searched Soulseek for the code, so both assertions fail.

- [ ] **Step 3: Classify the input, name the work item and refuse the fallback**

In `run_artist_only_mode_with_provider`, classify once at the top:

```rust
    let input = discography::ArtistInput::parse(artist);
```

pass `&input` to `discover_artist_albums`, take the outcome with `discovery.outcome`, and in the two authoritative arms use the canonical name:

```rust
        DiscoveryOutcome::Authoritative { albums, provenance, canonical_name } => {
            let artist = canonical_name.as_deref().unwrap_or(artist.as_str());
            // ... the existing body, unchanged, now bound to the canonical name ...
        }
        DiscoveryOutcome::AuthoritativeEmpty { provenance, .. } => {
            // ... unchanged ...
        }
```

and replace the fallback arm:

```rust
        DiscoveryOutcome::LegacyFallback { reason, kind, .. } => {
            if input.is_identity() {
                // The heuristic's input would be the raw code, and a Soulseek
                // search for "00581977202" is never what the operator meant.
                return Err(match kind {
                    DiscoveryFailure::Unresolved => SeakarrError::Config(format!(
                        "{} {artist:?} could not be resolved on MusicBrainz ({reason}); \
                         manual mode cannot guess album names for an identifier",
                        input.label()
                    )),
                    DiscoveryFailure::Provider => SeakarrError::MusicBrainz(format!(
                        "{} {artist:?} could not be looked up ({reason})",
                        input.label()
                    )),
                });
            }
            // ... the existing heuristic fallback, unchanged ...
        }
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib manual_artist_only 2>&1 | tail -10`
Expected: PASS — four new tests plus the existing manual-mode suite.

- [ ] **Step 5: Commit**

```bash
git add src/runner.rs
git commit -m "feat: refuse a failed identifier in artist-only manual mode"
```

---

## Task 8: Reject an identifier where no lookup happens

**Files:**

- Modify: `src/mode.rs:180-222` (manual plan), `src/main.rs:668-780` (batch lines)
- Test: `src/mode.rs` test module, `src/main.rs` test module

- [ ] **Step 1: Write the failing tests**

Add to the `mode.rs` test module:

```rust
    #[test]
    fn an_identifier_with_an_album_is_a_configuration_error() {
        let config = minimal_config();
        let cli = Cli {
            mode: Some("manual".into()),
            artist: Some("0000 0004 6659 7022".into()),
            album: Some("Half Light".into()),
            ..Cli::default()
        };

        let error = resolve_execution_plan(&config, &cli).unwrap_err();

        let message = error.to_string();
        assert!(message.contains("ISNI"), "got {message}");
        assert!(
            message.contains("names no library folder"),
            "the reason must be stated: {message}"
        );
    }

    #[test]
    fn an_identifier_without_an_album_is_accepted_for_manual_mode() {
        let config = minimal_config();
        let cli = Cli {
            mode: Some("manual".into()),
            artist: Some("00581977202".into()),
            ..Cli::default()
        };

        assert!(matches!(
            resolve_execution_plan(&config, &cli).unwrap(),
            ExecutionPlan::Manual { artist: Some(_), album: None }
        ));
    }
```

Use whatever the module's existing config/CLI fixture helpers are called; if it has no `minimal_config()`, reuse the helper the neighbouring manual-mode tests already build their config with.

Add to the `main.rs` test module:

```rust
    #[test]
    fn a_batch_line_naming_an_identifier_is_a_configuration_error() {
        let error = parse_batch_line("0000 0004 6659 7022 - Half Light").unwrap_err();
        assert!(error.contains("ISNI"), "got {error}");
        assert!(error.contains("names no library folder"), "got {error}");
    }

    #[test]
    fn a_batch_line_with_a_name_is_still_accepted() {
        assert_eq!(
            parse_batch_line("Grafix - Half Light").unwrap(),
            ("Grafix".to_string(), Some("Half Light".to_string()))
        );
    }
```

These tests assume Step 3 extracts the batch line parsing into a testable `parse_batch_line`; if the existing parser is inline in `run_batch_mode`, extract it as part of Step 3 and keep its current trimming and comment rules identical.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib mode::tests::an_identifier_with_an_album_is_a_configuration_error 2>&1 | tail -20`
Expected: FAIL — the plan resolves successfully, so `unwrap_err` panics.

- [ ] **Step 3: Add the two rejections**

In `src/mode.rs`, in the `SearchMode::Manual` branch, after the existing non-empty-target check:

```rust
            // A code identifies an artist for MusicBrainz, not a folder on disk:
            // an album-only run never asks MusicBrainz, so it cannot use one.
            if let Some(artist) = artist.as_deref() {
                let input = crate::discography::ArtistInput::parse(artist);
                if input.is_identity() && album.is_some() {
                    return Err(SeakarrError::Config(format!(
                        "{} {artist:?} names a MusicBrainz artist and names no library folder; \
                         an album-only manual run needs an artist name",
                        input.label()
                    )));
                }
            }
```

In `src/main.rs`, extract the line parsing and add the same rule:

```rust
/// Parse one batch line into an artist and an optional album.
///
/// An identifier cannot be used here: a batch line never asks MusicBrainz, and a
/// code names no library folder.
fn parse_batch_line(line: &str) -> std::result::Result<(String, Option<String>), String> {
    let (artist, album) = match line.split_once(" - ") {
        Some((artist, album)) => {
            let album = album.trim();
            (
                artist.trim().to_string(),
                if album.is_empty() { None } else { Some(album.to_string()) },
            )
        }
        None => (line.trim().to_string(), None),
    };
    let input = seakarr::discography::ArtistInput::parse(&artist);
    if input.is_identity() {
        return Err(format!(
            "{} {artist:?} names a MusicBrainz artist and names no library folder; \
             a batch line needs an artist name",
            input.label()
        ));
    }
    Ok((artist, album))
}
```

and call it wherever `run_batch_mode` currently splits a line, propagating the error as `SeakarrError::Config`:

```rust
    let (artist, album) = parse_batch_line(line).map_err(SeakarrError::Config)?;
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib mode:: 2>&1 | tail -5; cargo test --bin seakarr 2>&1 | tail -5`
Expected: PASS for both new suites.

- [ ] **Step 5: Commit**

```bash
git add src/mode.rs src/main.rs
git commit -m "feat: reject an artist identifier where no MusicBrainz lookup happens"
```

---

## Task 9: One UUID validator, and the CLI help

**Files:**

- Modify: `src/config.rs:635-642` (drop the private copy), `src/config.rs:879-891` (call the shared one), `src/main.rs:91-97` (help text)
- Test: `src/config.rs` test module, `src/main.rs` test module

- [ ] **Step 1: Write the failing tests**

Add to the `config.rs` test module, beside the existing `artist_mbids` validation tests:

```rust
    #[test]
    fn an_artist_mbids_key_may_be_a_code() {
        let mut config = Config::default();
        config.discography.artist_mbids.insert(
            "isni:0000000466597022".to_string(),
            "c02a5966-b5c4-483a-8326-483edcf3680e".to_string(),
        );

        assert!(config.validate().is_ok(), "a code-keyed pin must be accepted");
    }

    #[test]
    fn an_artist_mbids_value_still_has_to_be_a_uuid() {
        let mut config = Config::default();
        config
            .discography
            .artist_mbids
            .insert("Grafix".to_string(), "not-a-uuid".to_string());

        let error = config.validate().unwrap_err();
        assert!(error.to_string().contains("invalid MusicBrainz ID"), "got {error}");
    }
```

Add to the `main.rs` test module:

```rust
    #[test]
    fn the_artist_help_lists_every_accepted_shape() {
        let help = Cli::command().render_long_help().to_string();

        for expected in ["ISNI", "IPI", "MusicBrainz ID"] {
            assert!(help.contains(expected), "help must mention {expected}:\n{help}");
        }
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib an_artist_mbids_key_may_be_a_code 2>&1 | tail -10; cargo test --bin seakarr the_artist_help 2>&1 | tail -10`
Expected: the first test FAILS only if the config validator rejects a code key (it should already pass — it validates the key as a normalized non-empty string), and the help test FAILS because the help text does not mention ISNI/IPI yet. Both are worth having; if the first passes immediately, keep it as the regression pin for code-keyed pins.

- [ ] **Step 3: Share the validator and update the help**

In `src/config.rs`, delete the private `valid_mbid` and call the shared one:

```rust
            if !crate::discography::identifier::valid_mbid(mbid) {
                return Err(SeakarrError::Config(format!(
                    "discography.artist_mbids contains an invalid MusicBrainz ID for {artist:?}"
                )));
            }
```

In `src/main.rs`, replace the `--artist` doc comment and add a long help line so `--help` documents the shapes:

```rust
    /// Artist for manual mode; in discover mode a narrowing filter that also
    /// overrides discover.exclude_artists and the folder-ownership gate. Accepts
    /// an artist name, an ISNI (0000 0004 6659 7022), an IPI (00581977202) or a
    /// MusicBrainz artist ID; a code or ID identifies the artist exactly, with no
    /// name search. The name need not be an artist the library holds an album
    /// for: a library folder named after it becomes the destination, and with
    /// neither an album nor a folder the downloads stay in staging
    #[arg(long)]
    artist: Option<String>,
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib config:: 2>&1 | tail -5; cargo test --bin seakarr 2>&1 | tail -5`
Expected: PASS for both suites.

- [ ] **Step 5: Commit**

```bash
git add src/config.rs src/main.rs
git commit -m "refactor: share one MusicBrainz ID validator and document the artist shapes"
```

---

## Task 10: End-to-end pins with the real identifiers

**Files:**

- Test: `src/runner.rs` test module, `src/discography/identifier.rs` test module

- [ ] **Step 1: Write the failing tests**

Add to the `identifier.rs` test module the two real values end to end through the resolver:

```rust
    #[test]
    fn the_motivating_ipi_resolves_the_artist_that_claims_it() {
        let candidates = vec![ArtistCandidate {
            id: "c02a5966-b5c4-483a-8326-483edcf3680e".to_string(),
            name: "Grafix".to_string(),
            score: Some(100),
            ipis: vec!["00581977202".to_string()],
            isnis: vec!["0000000466597022".to_string()],
            disambiguation: Some("DnB producer Josh Jackson".to_string()),
        }];

        let by_ipi = resolve_code(CodeIdentifier::Ipi("00581977202"), &candidates).unwrap();
        let by_isni = resolve_code(CodeIdentifier::Isni("0000000466597022"), &candidates).unwrap();

        assert_eq!(by_ipi.candidate.id, by_isni.candidate.id);
        assert_eq!(by_ipi.resolution, ArtistResolution::Identity);
    }
```

Add to the runner test module a summary-level pin that a code run reports what it resolved:

```rust
    #[tokio::test]
    async fn a_code_run_announces_the_identity_it_resolved() {
        let soulseek = MockClient::new();
        search_index(&soulseek, "Grafix", &[("Grafix Missing", "Missing")]);
        let (config, db, staging, _library) = discover_fixture(&[("Present Artist", "Present")]);
        let provider = FakeDiscographyProvider::with_groups(vec![release_group("missing", "Missing", "1999")])
            .with_code_candidates(vec![ArtistCandidate {
                id: "c02a5966-b5c4-483a-8326-483edcf3680e".to_string(),
                name: "Grafix".to_string(),
                score: Some(100),
                ipis: vec!["00581977202".to_string()],
                isnis: Vec::new(),
                disambiguation: Some("DnB producer Josh Jackson".to_string()),
            }]);
        let capture = crate::test_support::LogCapture::start();

        run_discover_mode_with_provider(
            &soulseek,
            &config,
            &db,
            Some("00581977202"),
            false,
            staging.path(),
            &provider,
        )
        .await
        .unwrap();

        let logs = capture.text();
        assert!(
            logs.contains("IPI 00581977202")
                && logs.contains("Grafix")
                && logs.contains("c02a5966-b5c4-483a-8326-483edcf3680e"),
            "the run must say which artist the code resolved to, got:\n{logs}"
        );
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib a_code_run_announces_the_identity_it_resolved 2>&1 | tail -20`
Expected: FAIL — nothing logs the resolution yet.

- [ ] **Step 3: Log the resolution**

In `discover_artist_albums_at` in `src/discography/mod.rs`, after `resolved` is produced on the refresh path:

```rust
            if let Some(code) = input.code() {
                tracing::info!(
                    "{} {} resolved to {} ({}){}",
                    code.kind(),
                    code.value(),
                    resolved.name,
                    resolved.id,
                    resolved
                        .candidate
                        .disambiguation
                        .as_deref()
                        .map(|comment| format!(", {comment:?}"))
                        .unwrap_or_default()
                );
            }
```

and for the MBID path log the same line without a code prefix:

```rust
            if let Some(mbid) = input.mbid() {
                tracing::info!(
                    "MusicBrainz ID {mbid} resolved to {} ({})",
                    resolved.name,
                    resolved.id
                );
            }
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib 2>&1 | tail -5`
Expected: PASS — the full library suite, including both new tests.

- [ ] **Step 5: Commit**

```bash
git add src/discography/mod.rs src/discography/identifier.rs src/runner.rs
git commit -m "feat: announce the artist an identifier resolved to"
```

---

## Task 11: Documentation

**Files:**

- Modify: `README.md` (the `--artist` row, the discover FAQ, the ambiguity FAQ, the `artist_mbids` row, the scan/log line section)
- Modify: `docs/agent/specs/2026-09-15-musicbrainz-library-gap-filling-design.md`, `docs/agent/specs/2026-09-13-dominant-musicbrainz-artist-resolution-design.md`

- [ ] **Step 1: Update the README**

Add to the `--artist` row in the flags table: the four accepted shapes, that a code or ID is verified before the library walk and never falls back to the heuristic, and that a code with no single carrier is a configuration error naming the fix.

Extend the `discography.artist_mbids` row: keys may be an artist name, `isni:<digits>` or `ipi:<digits>`, and the value is always a UUID.

Add to the discover-mode section: a code run's work item is named after the resolved canonical MusicBrainz name, so placement follows the existing folder-spelling rules.

Add a paragraph to the "What happens when MusicBrainz cannot resolve an artist" FAQ: the code forms are the first fix, `artist_mbids` the precise one, and the resolution is announced in the log with the canonical name, MBID and disambiguation.

- [ ] **Step 2: Verify the README claims against the code**

Run: `rg -n 'artist_mbids|could not be resolved|ISNI|IPI' README.md | head -30`
Expected: every claim you just wrote is visible and matches the implemented behaviour; fix any sentence that the implementation contradicts.

- [ ] **Step 3: Add the dated supersession notes**

In `2026-09-15-musicbrainz-library-gap-filling-design.md`, add a `*(Superseded on 2026-10-07: ...)*` note where the artist source and the `--artist` narrowing are described, stating that an explicit value may be an ISNI, an IPI or a UUID, that its cache key is prefixed, and that the work item is named after the resolved canonical name.

In `2026-09-13-dominant-musicbrainz-artist-resolution-design.md`, add a note at the resolution-contract section stating that name resolution is unchanged and that an identity-based alternative now exists.

- [ ] **Step 4: Lint the markdown and commit**

Run: `markdownlint README.md docs/agent/specs/2026-09-15-musicbrainz-library-gap-filling-design.md docs/agent/specs/2026-09-13-dominant-musicbrainz-artist-resolution-design.md`
Expected: exit 0. If the specs need `<!-- markdownlint-disable MD013 -->`, they already carry it or the lines fit.

```bash
git add README.md docs/agent/specs/2026-09-15-musicbrainz-library-gap-filling-design.md docs/agent/specs/2026-09-13-dominant-musicbrainz-artist-resolution-design.md
git commit -m "docs: document identifying an artist by IPI, ISNI or MusicBrainz ID"
```

---

## Task 12: Full verification

**Files:** none (verification only)

- [ ] **Step 1: Run the whole gate set**

```bash
cargo fmt --all && cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo llvm-cov -p seakarr --summary-only --fail-under-lines 95
markdownlint README.md docs/agent/specs/2026-09-15-musicbrainz-library-gap-filling-design.md docs/agent/specs/2026-09-13-dominant-musicbrainz-artist-resolution-design.md
pre-commit run --all-files
```

Expected: every command exits 0, `cargo test` reports zero failures, and coverage stays at or above 95% lines (it was 97.45% at v0.31.0).

- [ ] **Step 2: Confirm the acceptance criteria from the spec by hand**

Run each against a throwaway staging directory and a `seakarr.yml` pointing at a scratch library, with `--mode discover --artist …`:

```bash
cargo run -- --mode discover --artist "0000 0004 6659 7022"   # resolves by ISNI before the walk
cargo run -- --mode discover --artist "00581977202"           # resolves the same artist by IPI
cargo run -- --mode discover --artist "c02a5966-b5c4-483a-8326-483edcf3680e"  # entity lookup, no search
cargo run -- --mode manual --artist "0000 0004 6659 7022" --album "Half Light"  # configuration error
```

Expected: the first three resolve `Grafix` and log the canonical name, MBID and disambiguation; the fourth exits non-zero with a message naming the ISNI and saying it names no library folder.

- [ ] **Step 3: Report anything that did not pass**

No commit in this task. If a gate fails, report the exact command, exit code and output; do not weaken a test, threshold or lint rule to pass it.

---

## Self-review

**Spec coverage:** CLI surface → Tasks 1, 9 (help) and 8 (rejections); provider layer → Tasks 2, 3; resolution → Tasks 1, 4; data flow and cache keys → Task 5; work-item naming → Tasks 6, 7; error handling → Tasks 6, 7, 8; observability → Task 10; testing and documentation → Tasks 1-12; acceptance criteria → Task 12 Step 2. No spec section is left without a task.

**Placeholder scan:** every code step carries the code to write; every run step carries the exact command and the expected result. Two places intentionally say "the existing body, unchanged" (Task 7's authoritative arms) and "the existing heuristic fallback, unchanged" — those are instructions to leave code alone, not omissions; the surrounding code is quoted verbatim in the file and unchanged by this plan.

**Type consistency:** `ArtistInput` (`Name`/`Isni`/`Ipi`/`Mbid`), `CodeIdentifier`, `ArtistCandidate.ipis`/`.isnis`/`.disambiguation`, `ArtistResolution::Identity`, `Discovery { outcome, canonical_name }`, `DiscoveryOutcome`'s `canonical_name`, `NamedArtistVerification::WorkList { outcome, filter_name }`, `resolve_code`, `parse_batch_line`, `valid_mbid` and `ArtistInput::display()` are spelled the same way in every task that uses them. `discover_artist_albums`/`_at` take `&ArtistInput` and return `Discovery` from Task 5 onward; Tasks 6, 7 and 10 use that signature.

**Known ordering constraint:** Task 1 adds the candidate fields and the `Identity` variant because the identifier tests cannot compile without them; Task 2 adds the parsing; Task 3 the search. Implementing Task 1 alone leaves `cargo test --lib` red until Task 2's wire fields are in — commit Task 1 only after the whole library compiles, which Step 4 of Task 1 instructs.
