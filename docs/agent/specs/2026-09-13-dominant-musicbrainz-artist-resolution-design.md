# Dominant MusicBrainz artist resolution

## Problem

Artist-only manual mode currently accepts automatic MusicBrainz resolution only
when exactly one candidate has the requested canonical artist name. Multiple
canonical exact-name matches are always unresolved, even when MusicBrainz ranks
one candidate far above every alternative. The run then uses a compatible stale
cache or the visible legacy folder heuristic.

This affects established artists with short or reused names. A live search for
`Ils` returns four canonical exact-name candidates. The intended electronic
artist, Illian Walker, scores 100; the next exact-name candidate scores 86, and
the remaining exact-name candidates score 83. The current resolver ignores
those scores and reports:

`artist could not be resolved safely: 4 candidates match "ils"`

The legacy heuristic must remain available, but it should be the final fallback
rather than the routine outcome when MusicBrainz already supplies a clear
winner.

## Goal

Allow artist-only manual mode to select a uniquely dominant MusicBrainz
canonical exact-name candidate without weakening name matching or making extra
catalog requests.

A dominant candidate must have a MusicBrainz search score of 100 and lead the
runner-up canonical exact-name candidate by at least 10 points.

## Scope

In scope:

- retain MusicBrainz artist-search scores in the provider model;
- rank canonical exact-name candidates deterministically;
- select a unique score-100 candidate with a margin of at least 10;
- log evidence for automatic selection among duplicate exact names;
- preserve configured MBID, cache, stale-cache, and legacy-fallback behavior;
- add resolver, provider, orchestration, and logging tests;
- update user documentation for automatic artist resolution.

Out of scope:

- release-group count or catalog-size popularity probing;
- aliases, sort names, partial names, tags, country, area, lifespan, artist type,
  or disambiguation comments as ranking signals;
- configurable confidence thresholds;
- interactive candidate selection;
- database migrations or cache schema changes;
- changes to explicit artist-plus-album, album-only, batch, auto, or
  library-upgrade flows.

## Design principles

1. **Canonical name remains the eligibility boundary.** Ranking cannot widen
   the candidate set.
2. **Selection must be deterministic.** MusicBrainz response order and MBID
   ordering never resolve ambiguity.
3. **A clear winner needs both absolute and relative confidence.** Score 100
   alone is insufficient when another candidate is close.
4. **Missing or invalid evidence fails closed.** The resolver never invents or
   clamps a score.
5. **No extra network work.** Resolution uses the artist-search response that
   seakarr already requests.
6. **Fallback remains visible.** Unresolved cases continue through existing
   stale-cache and legacy-fallback precedence.

## Candidate model

`ArtistCandidate` gains an optional numeric MusicBrainz search score. The
MusicBrainz artist-search wire model decodes the response's `score` property.
The artist-by-MBID endpoint may omit a score because configured IDs do not need
ranking.

The provider accepts only integer scores in MusicBrainz's documented 0 through
100 range. A value outside that range is invalid provider data. A missing score
is represented explicitly rather than converted to zero.

No aliases or descriptive metadata are added to the domain candidate. The
provider may ignore those fields in MusicBrainz responses as it does today.

## Candidate eligibility

The resolver first applies the existing canonical exact-name rule:

- normalize the requested name and candidate canonical `name` with Unicode
  NFKC, Unicode lowercase conversion, trimming, and whitespace collapse;
- keep punctuation significant;
- ignore aliases, sort name, and all descriptive metadata;
- exclude every candidate whose canonical normalized name differs from the
  request.

Only this filtered subset participates in score comparison. A higher-scored
partial-name or alias match cannot win.

## Selection algorithm

The resolver handles the canonical exact-name subset as follows.

### Zero exact matches

Return an unresolved error stating that no canonical candidate matches the
requested artist.

### One exact match

Return that candidate using current behavior. A score is not required because
there is no ambiguity to rank.

### Multiple exact matches

Automatic selection succeeds only when all conditions hold:

1. every exact-name candidate supplies a score;
2. every score is in the inclusive range 0 through 100;
3. exactly one candidate has the highest score;
4. the highest score is exactly 100;
5. the highest score exceeds the second-highest score by at least 10 points.

If all conditions hold, return the unique top candidate with dominance evidence
containing the exact-match count, top score, runner-up score, and margin.

Otherwise return an unresolved error. A tied top score is never broken by
MusicBrainz response order, canonical name, MBID, or any unapproved metadata.

## Examples

| Exact-name scores | Result | Reason |
| --- | --- | --- |
| one candidate, score absent | select | No ranking is needed. |
| 100, 86, 83, 83 | select 100 | Unique top score and margin 14. |
| 100, 78, 77, 77 | select 100 | Unique top score and margin 22. |
| 99, 70 | unresolved | Top score is below 100. |
| 100, 91 | unresolved | Margin is 9. |
| 100, 90 | select 100 | Margin boundary is inclusive. |
| 100, 100, 70 | unresolved | Highest score is tied. |
| 100, missing | unresolved | Competing evidence is incomplete. |

The current live `Ils` result therefore selects MBID
`16b97aaa-d7c0-469f-8c97-47c705b2d02f` (Illian Walker): score 100 versus
runner-up score 86.

## Components and boundaries

### MusicBrainz provider

The provider continues to own HTTP behavior and typed wire decoding. It retains
`score` for artist-search results while leaving artist-by-ID behavior unchanged.
Existing response-size, timeout, retry, pacing, count, offset, and one-page
completeness checks remain unchanged.

The provider still makes one artist search with `limit=100`. It does not fetch
release-group counts for multiple candidates.

### Resolver

The resolver remains independent of HTTP and accepts the requested name plus a
candidate slice. It owns exact-name filtering, score validation, sorting, tie
detection, threshold checks, and construction of selection evidence or an
unresolved reason.

Selection evidence is returned to the discovery orchestration rather than
requiring the HTTP provider to log policy decisions.

### Discovery orchestration

The existing discovery service keeps its precedence:

1. configured `discography.artist_mbids` override;
2. compatible fresh cache;
3. MusicBrainz refresh and automatic artist resolution;
4. compatible stale cache when refresh or resolution fails;
5. visible legacy folder discovery.

A configured MBID remains authoritative and bypasses name ranking. A successful
automatic selection uses the selected MBID for the existing release-group
browse, filtering, cache write, and targeted Soulseek searches.

## Caching

No database or serialized release-group shape changes.

The cache already stores the requested normalized artist key, resolved canonical
name, selected artist MBID, fetch time, and release groups. A successful
dominant selection writes the same cache shape with the chosen MBID. A fresh
compatible cache continues to avoid MusicBrainz requests. A later refresh may
select a different artist only if the current MusicBrainz evidence satisfies
the same deterministic rule.

A score is transient resolution evidence and is not stored in the discography
cache.

## Observability

A selection among multiple canonical exact-name candidates emits one INFO event
with structured fields for:

- requested artist;
- selected canonical artist name;
- selected MBID;
- canonical exact-match count;
- top score;
- runner-up score;
- score margin.

A unique exact-name selection may retain current routine logging because no
ambiguity was resolved.

Unresolved errors identify the failed rule without dumping the full response:

- no canonical exact-name candidate;
- multiple exact-name candidates with missing score evidence;
- invalid score outside 0 through 100;
- tied top score;
- highest score below 100;
- score margin below 10.

WARN behavior for stale-cache use and legacy fallback remains unchanged. No
candidate aliases, tags, or unrelated metadata are written to logs.

## Error handling

Artist ranking errors remain non-fatal to artist-only manual mode. They flow
through existing stale-cache and legacy-fallback handling.

Transport errors, non-success statuses, rate limiting, retries, timeout,
response-size limits, JSON decoding, artist-search completeness checks, and
release-group pagination retain current behavior.

A unique canonical exact match remains valid when its score is missing. Missing
scores become an error only when multiple exact-name candidates require
comparison.

## Testing

### Pure resolver tests

Tests must prove:

1. zero canonical exact matches are unresolved;
2. one canonical exact match resolves with or without a score;
3. `100, 86, 83, 83` selects the score-100 candidate;
4. `100, 78, 77, 77` selects the score-100 candidate;
5. top score 99 is unresolved;
6. margin 9 is unresolved;
7. margin 10 resolves;
8. tied top scores are unresolved;
9. missing score on any competing exact-name candidate is unresolved;
10. invalid scores are rejected;
11. a higher-scored alias or partial-name result cannot enter the exact-name
    candidate set;
12. every candidate permutation produces the same result.

### Provider tests

Wiremock tests must prove:

- artist-search scores are decoded and retained;
- a missing score remains absent;
- invalid numeric values fail decoding or score validation;
- search query, identity headers, limit, count, offset, response-size, retry,
  and timeout contracts remain unchanged;
- sort name, aliases, and descriptive metadata cannot widen canonical matching.

### Integration tests

Discovery tests must prove:

- the selected MBID drives release-group retrieval;
- the chosen MBID and release groups are written to the existing cache shape;
- a fresh cache avoids ranking requests;
- configured MBID overrides bypass score ranking;
- an unresolved score decision uses compatible stale cache when available;
- unresolved selection without compatible cache retains the visible legacy
  fallback;
- INFO logging includes chosen MBID and score evidence;
- WARN and run-summary behavior for fallback is unchanged.

All tests use mocked providers or wiremock. No test calls live MusicBrainz or
waits for the production one-second request interval.

## Documentation

README artist-resolution documentation must state:

- unique canonical exact-name matches still resolve automatically;
- duplicate canonical exact names resolve only for a unique score-100 candidate
  with a margin of at least 10;
- aliases and catalog popularity do not participate;
- configured MBIDs remain the deterministic override;
- unresolved cases still prefer stale cache before visible legacy fallback.

## Acceptance criteria

- `Ils` candidates scored 100, 86, 83, and 83 resolve to MBID
  `16b97aaa-d7c0-469f-8c97-47c705b2d02f`.
- Bonobo's observed 100, 78, 77, and 77 pattern resolves to its score-100
  candidate.
- Unique exact matches retain current behavior.
- Ties, score 99, margin 9, missing competing scores, invalid scores, and
  non-exact high-scored candidates remain unresolved.
- Candidate response order cannot change selection.
- Selected MBID enters existing release-group retrieval and cache persistence.
- Automatic ambiguous-name selection logs its evidence at INFO.
- Existing stale-cache and legacy-fallback outcomes remain visible and
  unchanged.
- No extra MusicBrainz request, configuration key, or database migration is
  introduced.

## References

- [MusicBrainz API search](https://musicbrainz.org/doc/MusicBrainz_API/Search)
- [MusicBrainz artist model](https://musicbrainz.org/doc/Artist)
- Live artist search evidence captured during design:
  `https://musicbrainz.org/ws/2/artist/?query=artist%3A%22ils%22&fmt=json&limit=10`
