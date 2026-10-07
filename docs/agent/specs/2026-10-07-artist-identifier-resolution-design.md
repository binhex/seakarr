<!-- markdownlint-disable MD013 -->

# Artist identification by IPI, ISNI or MusicBrainz ID

## Problem

`--artist` accepts an artist name, and discovery resolves that name against
MusicBrainz: candidates must match the requested canonical name exactly, and
several exact-name matches are only accepted when one is uniquely dominant
(score 100 with a 10-point lead). A name MusicBrainz cannot tie to exactly one
artist therefore fails: in discover mode it is a configuration error raised
before the library walk, and in artist-only manual mode it falls back to the
legacy folder heuristic.

Some artists are identified far more reliably by their rights identifiers than by
their name. The motivating case is the drum-and-bass producer Grafix, whose
MusicBrainz record carries IPI `00581977202` and ISNI `0000 0004 6659 7022`; a
name search for `Grafix` competes with every other spelling and credit of that
name, while either code points at exactly one artist.

Today the only way to pin an identity is `discography.artist_mbids`, a
name-to-UUID map in `seakarr.yml`. That works, but it needs the hyphenated UUID
rather than the code the operator actually holds, requires editing configuration
before a one-off run, and keys on the name that failed to resolve in the first
place.

MusicBrainz supports both codes in artist search. Verified live on 2026-10-07:

- `query=isni:0000000466597022` returns exactly one artist, `Grafix`, MBID
  `c02a5966-b5c4-483a-8326-483edcf3680e`, score 100.
- `query=ipi:00581977202` returns the same single artist with score 100.
- The search response carries the candidate's own identifiers
  (`"ipis":["00581977202"]`, `"isnis":["0000000466597022"]`) and a
  `disambiguation` comment (`DnB producer Josh Jackson`), so a match can be
  confirmed from the artist's record rather than inferred from a relevance score.
- The plain entity lookup `/ws/2/artist/<mbid>` does **not** return `ipis` or
  `isnis`; only search does.

## Goal

Let `--artist` carry an exact MusicBrainz identity — an ISNI, an IPI, or an
artist UUID — and fetch that artist's discography without adding new flags,
without weakening name resolution, and without guessing when a code matches more
than one artist.

## Scope

### In scope

- Value-shape detection on the existing `--artist` flag.
- A provider search path for IPI and ISNI; the existing MBID lookup for UUIDs.
- Strict single-carrier resolution with candidate self-verification.
- Discover mode and artist-only manual mode.
- The resolved canonical name as the work item's name.
- Cache keys, the `discography.artist_mbids` interaction, observability, docs.

### Out of scope

- Reading MusicBrainz IDs from file tags. The library scanner captures artist,
  album and bitrate only, so presence, placement and the folder gate stay
  name-based; identity matching against the library is not part of this design.
- New configuration keys. `discography.artist_mbids` keeps UUID values; only its
  keys gain the ability to be a code.
- Identifiers in the automatic sweep or in `discover.exclude_artists`, which are
  library-derived and name-keyed.
- Identifiers in `--mode manual --album Y` runs and in `--batch-file` lines,
  which never ask MusicBrainz.
- Supplying more than one identifier for a single run.
- `arid:` search for UUIDs: an entity lookup already exists and is exact.

## Decisions

### The existing flag carries the identifier, and its shape decides

`--artist` stays a single `Option<String>`; no flag is added. Auto-detection was
chosen over explicit `--artist-ipi`/`--artist-isni` flags because the whole point
is to be simpler than editing configuration, and over a config-only extension
because a one-off run should not need a config edit. Accepted cost: a band
literally named like one of the code shapes would be read as a code. The escape
hatch is the existing `discography.artist_mbids` pin, which takes precedence over
the input's own shape (see Caching and pins).

### One value per run

Supplying a name *and* a code together is not supported: the code alone is
strictly more precise, so there is nothing to combine, and a precedence rule
would be one more thing to document, test and get wrong.

### Exactly one carrier, or refuse

A candidate counts only if its own `ipis`/`isnis` array contains the requested
code. Zero carriers and several carriers are both refusals, reported distinctly.
An IPI or ISNI is not unique by construction — a person and a group, or two
MusicBrainz entities for one act, can share one — and the existing dominance
rules cannot help because MusicBrainz scores identifier hits at or near 100
regardless of which artist they are. Refusing is also consistent with the hard
error an unresolvable name already raises before the walk.

### Codes are honoured where a discography is fetched

Discover mode and artist-only manual mode both accept a code, matching how
`discography.artist_mbids` already applies to both. Paths that never ask
MusicBrainz — `--mode manual --artist X --album Y` and batch lines — reject a
recognisable identifier with a configuration error, because a code cannot name a
library folder and searching Soulseek for a digit string fails late and
confusingly.

### A code-only run names the work item after the resolved artist

The work item carries the canonical MusicBrainz name of the artist that owns the
code. Library folder lookup, the `Artist Album` search, placement and the
processed records then follow today's rules unchanged: the on-disk folder
spelling wins when the library already holds a folder for that artist, otherwise
the existing staging fallback and warning apply. A code run behaves exactly like a
run naming that canonical name, except the identity came from the code.

### IPI and ISNI get a provider search; UUIDs reuse the entity lookup

A new `search_artists_by_identifier` covers IPI and ISNI, keeping MusicBrainz
query syntax inside the HTTP layer where it lives today. A UUID needs no search:
`artist_by_id` is an exact entity lookup and is more reliable than an `arid:`
query. The existing heavily tested `search_artists(name)` path is untouched.

## CLI surface

| `--artist` value, after stripping spaces and hyphens | Treated as | Validation |
| --- | --- | --- |
| `8-4-4-4-12` hexadecimal UUID | MusicBrainz artist ID | the existing `valid_mbid` rules |
| 16 characters, digits with an optional trailing `X` | ISNI | ISO 7064 MOD 11-2 check digit |
| 11 digits | IPI | digits only; IPI has no check digit |
| anything else | artist name | today's behaviour, unchanged |

A value that looks like a code but fails validation — a bad ISNI check digit, 15
or 12 digits, a UUID with the wrong group lengths — is a configuration error
naming the problem. It is not silently searched for as a name, and it does not
claim the artist is unknown. This is the only change in behaviour for a
name-shaped input.

IPIs carry no check digit, so a mistyped but well-formed IPI surfaces as "no
MusicBrainz artist carries this IPI" rather than as a validation error.

## Provider layer

- `ArtistIdentifier { Isni(String), Ipi(String) }`, held normalised: digits only
  for both, with an ISNI check character upper-cased. Parsing and normalisation
  are pure functions shared by the CLI validation, the resolver and the cache key.
- `DiscographyProvider` gains
  `search_artists_by_identifier(&self, &ArtistIdentifier) -> Result<Vec<ArtistCandidate>, DiscographyError>`.
  The implementation builds `isni:"…"` or `ipi:"…"` and reuses the existing
  request, pacing, retry, `offset != 0` and count-versus-payload consistency
  checks verbatim, so an identifier search classifies its failures exactly as a
  name search does.
- `ArtistWire` gains `#[serde(default)] ipis`, `isnis` and `disambiguation`.
  `ArtistCandidate` gains the two identifier lists so the resolver can verify a
  candidate; `disambiguation` is carried for reporting.
- Test fixtures that construct `ArtistCandidate` gain two empty vectors. This is
  mechanical and is the accepted cost of keeping one candidate type rather than a
  parallel identifier-only type.

## Resolution

`resolve_artist_by_identifier(identifier, candidates)`:

1. Keep the candidates whose own identifier list contains the requested code,
   compared after normalisation.
2. Exactly one carrier resolves, recorded as a new `ArtistResolution::Identifier`
   variant so the log evidence distinguishes a code match from a name match.
3. No carrier is `DiscographyError::ArtistUnresolved` with a reason naming the
   code.
4. Several carriers is `DiscographyError::ArtistUnresolved` with a reason naming
   the count and the carriers, and pointing at the UUID form.

No new error variant is introduced: "cannot resolve to exactly one artist" is
what `ArtistUnresolved` already means, so the failure cache records it, the
circuit breaker does not count it, and discover raises its existing hard
configuration error before the library walk.

## Data flow

`discover_artist_albums_at` keeps its current signature and parses the artist
string once through the shared parser, so both callers and their test doubles are
unchanged at the call boundary.

For a code input:

1. The cache key is `isni:<digits>` or `ipi:<digits>` — prefixed so the two kinds
   cannot collide and so a code key cannot collide with a name key.
2. A `discography.artist_mbids` entry keyed by that same string wins and skips the
   search, exactly as a name-keyed pin does today.
3. Otherwise the provider is asked, and the resolver applies the single-carrier
   rule.
4. `discover_artist_albums` returns the resolved canonical name alongside the
   albums. It reaches the cache row and the debug log today and is discarded; the
   `Authoritative` and `AuthoritativeEmpty` variants gain
   `canonical_name: Option<String>` so the runner can use it. Match arms that do
   not need it take `..`, so the change is a field addition rather than a return
   type rewrite.
5. Discover's pre-walk verification (`verify_named_artist`) uses the canonical
   name for the selection and for `ArtistFolderIndex`, so placement follows the
   existing folder-spelling rules.
6. The resolution is logged and summarised: the code, the canonical name, the
   MBID and the disambiguation comment when MusicBrainz supplies one.

## Caching and pins

- The discography cache row keeps its existing shape; only the key can now be an
  identifier key. Rows still record `artist_mbid` and `canonical_artist`, and the
  compatibility check against a configured pin is unchanged.
- The failure cache records an identifier-keyed resolution failure under the same
  rules as a name-keyed one, so a repeated code run replays the recorded failure
  cheaply. An explicit `--artist` run bypasses the recorded failure, so a code run
  always asks once, matching the rule established for names.
- Consequence: a name run and a code run for the same artist keep separate cache
  rows and cost one extra release-group fetch the first time. This is accepted
  rather than solved, because keying by MBID would require a second cache lookup
  path and a schema change for a bounded, one-off cost.

## Error handling

| Situation | Discover | Artist-only manual | Album-only manual / batch |
| --- | --- | --- | --- |
| Malformed code-shaped value | Configuration error at mode resolution, before any request | Same | Same |
| Code with exactly one carrier | Resolved before the walk; library scanned with the canonical name | Resolved; albums fetched | Rejected as a configuration error |
| Code with no carrier | Hard configuration error before the walk | **Hard configuration error**, not the legacy heuristic fallback | Rejected as a configuration error |
| Code with several carriers | Hard configuration error naming the count and the carriers | Same | Rejected as a configuration error |
| MusicBrainz unreachable | Existing breaker and skip behaviour | **Hard MusicBrainz error** for a code input, not the legacy heuristic fallback | Not applicable |

The manual-mode refusals are deliberate: the legacy heuristic exists to guess album
names for an artist that was named, and running it for a code would search
Soulseek for a digit string, whether the code failed to resolve or the provider
could not be reached at all. A search-stage error raised after resolution (a lost
session, for example) keeps its existing behaviour.

## Observability

One log line and one summary notice per identifier resolution, carrying the input
code, the resolved canonical name, the MBID and the disambiguation comment when
present. This is what lets an operator confirm that a shared IPI or ISNI picked
the artist they meant, and it is the only place the canonical-name decision
becomes visible.

## Components and boundaries

- `src/discography/identifier.rs` (new): `ArtistIdentifier`, `parse_artist_input`,
  normalisation, ISNI check digit, `resolve_artist_by_identifier`. Pure and
  free of I/O, so it is testable without a provider.
- `src/discography/mod.rs`: `discover_artist_albums_at` branches on the parsed
  input, keys the caches, consults the pin, and reports the canonical name;
  `ArtistResolution` gains `Identifier`; `DiscoveryOutcome`'s two authoritative
  variants gain `canonical_name`; `ArtistCandidate` and the provider trait gain
  their fields and method.
- `src/discography/musicbrainz.rs`: query construction for `ipi:`/`isni:`, wire
  fields, and the identifier search implementation.
- `src/runner.rs`: `verify_named_artist` uses the canonical name for a code input
  when building the selection; the manual path refuses a code that resolves to
  nothing instead of falling back.
- `src/mode.rs`: rejects a recognisable identifier for album-only manual and batch
  inputs, using the same parser so the rule cannot drift.
- `src/main.rs`: `--artist` help text lists the accepted shapes.

## Testing

- **Parser**: each shape; spaced and hyphenated ISNI and IPI; lower-case ISNI
  check character; bad ISNI check digit; 15- and 17-character digit strings; UUID
  with wrong group lengths; a plain name containing digits; a name that is exactly
  11 digits is an IPI by design and is asserted as such.
- **Resolver**: one carrier resolves; a search hit that does not carry the code is
  discarded; no carrier and several carriers each produce `ArtistUnresolved` with
  their distinct reasons; comparison ignores spacing and ISNI case.
- **Provider**: the identifier query is escaped and encoded like the name query;
  `ipis`/`isnis`/`disambiguation` parse when present and default when absent.
- **Runner, discover**: a code is verified before the library walk, proven with the
  non-existent-library-root technique that pins ordering today; a code with no
  carrier is a hard error before the walk; the resolved canonical name drives the
  search and the placement folder.
- **Runner, artist-only manual**: a code fetches albums; a code with no carrier
  fails instead of using the heuristic.
- **Mode**: an identifier with `--album` and in a batch line is a configuration
  error.
- **Regression**: the whole existing name-path suite passes unchanged, and a
  name-shaped input behaves exactly as it does today.

## Documentation

- `README.md`: the `--artist` row and help text (four accepted shapes and their
  precedence), the discover and ambiguity FAQ entries, the `discography.artist_mbids`
  row (keys may be a code), and the resolution log line.
- Dated supersession notes in `docs/agent/specs/2026-09-15-musicbrainz-library-gap-filling-design.md`
  (artist source and `--artist` narrowing) and
  `docs/agent/specs/2026-09-13-dominant-musicbrainz-artist-resolution-design.md`
  (the name-only resolution contract now has an identity-based alternative).

## Accepted consequences

- A band named exactly like a code shape is read as a code; the config pin is the
  way out, and the parser is the single place to revisit that trade.
- Name runs and code runs for one artist keep separate cache rows.
- An IPI typo is not detected by validation, only by the empty result.
- Placement for a code run depends on the canonical name matching a library
  folder after normalisation; a differently spelled folder still falls back to
  staging with a warning, as it does today.
- No library-side identity matching: the presence check remains name-based.

## Acceptance criteria

1. `--artist 0000 0004 6659 7022` in discover mode fetches the discography of the
   single artist carrying that ISNI, before the library is walked.
2. `--artist 00581977202` resolves the same artist by IPI.
3. `--artist c02a5966-b5c4-483a-8326-483edcf3680e` resolves it by UUID without a
   search request.
4. A code with no carrier, and a code with several carriers, each fail with a
   configuration error in discover and in artist-only manual mode, before any
   library scan in discover, and never fall back to the legacy heuristic.
5. A code in `--mode manual --artist X --album Y` or in a batch line is a
   configuration error.
6. Every existing name-based test passes unchanged, and `cargo clippy`,
   `cargo fmt --check`, `cargo test`, the coverage floor and the markdown lint all
   pass.
