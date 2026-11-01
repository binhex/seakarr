<!-- markdownlint-disable MD013 -->

# Label-based discovery

## Problem

`discover` mode can only work out what to download from the library it is given: it
indexes the audio files on disk, derives an artist list from tags and folder names,
asks MusicBrainz for each artist's discography, and fills the gaps. The operator has no
way to say "I want whatever this record label has released", so a whole class of
acquisition is unsupported: a label's roster is exactly the set of artists an operator
does not yet know about, and an artist the library has never seen can never enter the
work list.

The request is to sweep a record label - concrete example: Hospital Records - and pick
up every release of a wanted type (studio albums by default) that the library does not
hold, using MusicBrainz as the authority for what the label has released.

Two facts about the provider shape this design:

- MusicBrainz exposes **no** browse of release groups by label. The only label-keyed
  path is browsing **releases** (`/ws/2/release?label=<mbid>`), whose payload carries
  each release's release group inline when `inc=release-groups` is requested. Label
  discovery therefore starts from releases rather than from release groups, and
  collapses to release groups locally.
- Label names are ambiguous in practice. A live query for
  `label:"Hospital Records"` returns five candidates - score 100 for the UK drum &
  bass label (label code 12939) and score 62 for a US noisecore label - so a label
  needs the same "resolve safely or refuse" treatment `--artist` already has.

## Goal

Let `--label` name one or more record labels on a `discover` run, resolve each label to
exactly one MusicBrainz entity or refuse, fetch every release of the wanted type on
that label, and download the ones the library does not hold into staging - without
changing anything about the existing library-derived artist sweep.

## Scope

### In scope

- A `--label` flag on `discover` mode accepting a label name or a MusicBrainz label
  MBID, comma-delimited and repeatable within one value list.
- Label resolution with the existing dominance rules, plus pinning via a new
  `discography.label_mbids` map.
- Browsing a label's releases, with pagination guards, a hard catalogue ceiling, and
  30-day caching in the existing `discography_cache` table.
- `--release-type` as a per-run override of `discography.allowed_types`.
- `--since` as an optional lower bound on a release group's first-release-date.
- Selection of missing albums by artist credit, reuse of `release_allowed`,
  `select_albums` and `album_identity_key`.
- Staging-only placement for every label-sourced download.
- Run-summary counters and notices, failure caching, and the provider circuit breaker.
- Documentation and end-to-end coverage.

### Out of scope

- Writing label-sourced albums into `library.paths`. Label runs never create an artist
  folder; the operator moves albums out of staging.
- Browsing by label series, imprint, sub-label or distributor. A label is one
  MusicBrainz label entity.
- Following a label's roster as a subscription: an artist is only ever discovered
  through a release that is on the label and of the wanted type.
- Release-level (as opposed to release-group-level) selection, format preferences,
  country, barcode and catalog-number handling.
- New `discover` configuration keys. The run-time selectors live on the execution
  plan, so nothing about a label run is persisted except its cache row and any
  recorded resolution failure.
- A date upper bound, "newest N" windows, and notification of newly released albums
  outside a run.
- Supplying `--artist` and `--label` together, or combining a label sweep with the
  library sweep in one invocation.

## Decisions

### One flag carries the label, and its shape decides

`--label` takes a label name or a MusicBrainz label MBID, detected by value shape.
This mirrors `--artist`, which already accepts a name, an IPI, an ISNI or an artist
MBID in one flag, and keeps the CLI surface to one new option. Two flags
(`--label` plus `--label-id`) and a name-only flag were both considered: the first
adds a second validation surface for no expressive gain, the second cannot express an
ambiguous label at all.

Accepted cost: a label literally named like a UUID would be read as an MBID. The
escape hatch is `discography.label_mbids`, which pins by name and wins over the input's
own shape.

### A comma-delimited list, swept in one run

`--label` is `value_delimiter = ','` and merges several labels into one work list, the
way `--library-path` already accepts a list. One invocation covers a label family
(Hospital Records plus Med School), which is what an operator actually wants when they
start curating by label, and a single shared download budget keeps the run bounded.

Consequences, made explicit rather than inferred: labels are resolved in the order
given, targets are merged and de-duplicated across labels, and a target reachable from
two labels is owned by the first label that produced it.

### A label run replaces the library sweep

When `--label` is present the run's work list is the label catalogue alone. The
library is still scanned, because "already present" cannot be answered without it, but
its artists never enter the work list. Mixing the two sources would make a run's cost
and its summary depend on two very different inputs, and the shared download budget
(`discover.max_cycle_downloads`) would let the library sweep consume the budget before
the label was reached.

`--artist` and `--label` together are refused: an explicitly named artist is a
single-artist request, a label is a catalogue request, and neither is a filter on the
other.

### The wanted type comes from config, overridable per run

Label runs honour `discography.allowed_types` (default `[studio_album]`) exactly as the
artist sweep does, and `--release-type` overrides it for one run. This keeps one
classifier - `release_allowed` - and one vocabulary - the `DiscographyReleaseType`
enum - across both sweeps, while letting "albums and EPs" be a command-line decision
rather than a config edit.

Two alternatives were rejected. Config-only would force a config edit for a one-off
run. A required-per-run `--release-type` would make label runs ignore the
`discography` section that governs every other MusicBrainz query, which is a silent
inconsistency waiting to be reported as a bug.

`album` is accepted as an alias of `studio_album`, because an operator asking for
"albums" means exactly what `StudioAlbum` classifies; `release_allowed` already treats
an `Album` primary type with no secondary types as a studio album. Both spellings map
to the same enum variant, so the alias cannot diverge from the classifier.

### "New" means missing from the library, optionally bounded by `--since`

The default is gap-fill, matching the artist sweep: every album of the wanted type on
the label that the library does not hold. A rolling recency window as the default was
rejected because it silently never fetches a label's back catalogue and because a
missed run longer than the window would lose releases permanently.

`--since YYYY[-MM[-DD]]` bounds a first run or a backfill without changing the
definition of missing. It is a lower bound on the release group's first-release-date,
validated by the same partial-date rules used for MusicBrainz dates. An **undated**
release group cannot satisfy a lower bound it cannot prove, so it is excluded and
counted separately; it is never admitted on the assumption that it is recent.

### The artist credit is the searchable artist, verbatim

A label-sourced album is searched and staged under the release's full artist credit as
MusicBrainz spells it (`Nu:Tone feat. Stamina MC`), not under a stripped lead artist.
The credit is what identifies the release, the library's own tags use the same shape,
and trimming it would make the Soulseek query differ from the album's identity on disk.

`discover.exclude_artists` still applies: a credit whose normalised key matches an
excluded name (`Various Artists`, `VA`, `Unknown Artist` by default) is dropped and
counted. This reuses an existing, documented key rather than introducing a label-only
exclusion list.

### Every label-sourced download stays in staging

Label runs pass `LibraryTarget::StagingOnly` to `process_album`. That variant already
documents itself as "the album stays in staging: nothing is written to the library,
because placement has no artist folder to write into and never creates one", so no new
placement code is needed and no label run can create a library folder.

Creating `<library.paths[0]>/<Artist>/<Album>/` was considered and rejected by the
operator: the value of the feature is discovery, not unattended writes into the
library, and the operator moves albums deliberately.

### Cache the catalogue for the configured discography TTL

The label catalogue is cached in `discography_cache` under a namespaced key with the
same `discography.cache_days` (30) lifetime and the same stale-on-refresh-failure
behaviour as an artist discography. No schema change: `artist_key` is a
`TEXT PRIMARY KEY COLLATE NOCASE`, so `label:<normalised name>` is a key, and the
label path is the only reader of that prefix.

Accepted consequence: a brand-new release can take up to 30 days to appear in a sweep.
That is the direct cost of the chosen lifetime; a shorter label-only TTL was rejected
as one more configuration surface for a modest gain, and no-cache was rejected as
re-fetching up to 12 pages every run.

### Resolution refuses rather than guesses

Label resolution reuses the constants and shape of artist resolution: exactly one
canonical exact-name match is accepted; duplicates require every candidate to carry a
score, exactly one candidate at `REQUIRED_TOP_SCORE`, and a lead of at least
`REQUIRED_SCORE_MARGIN`. Anything else is `LabelUnresolved` with the candidates in the
message, so the operator can pin the right MBID instead of discovering later that the
run downloaded from the wrong label.

## CLI surface

| Flag | Values | Validation |
| --- | --- | --- |
| `--label` | one or more label names or label MBIDs, comma-delimited | discover mode only; each element non-blank; a UUID shape must pass `valid_mbid`; duplicates after normalisation are refused; refused together with `--artist`, `--album` and `--batch-file` |
| `--release-type` | `studio_album`, `album` (alias), `live_album`, `ep`, `single`, `compilation`, `remix`, `soundtrack`, `dj_mix`, `mixtape` | requires `--label`; unknown value refuses and lists the accepted set; an empty list refuses |
| `--since` | `YYYY`, `YYYY-MM` or `YYYY-MM-DD` | requires `--label`; must parse under the partial-date rules; a partial date lower-bounds by its missing components (a `YYYY-MM` bound sorts before any day in that month) |

Every new refusal names the flag and the reason, in the existing style, for example
`--artist and --label are mutually exclusive; run one source per invocation` and
`--release-type is only valid with --label`.

The validated selectors ride on the execution plan, not on `Config`:

```rust
pub enum ExecutionPlan {
    // Existing variants are unchanged.
    Discover {
        /// The one explicitly named artist, when the operator named one.
        artist: Option<String>,
        /// Labels to sweep; empty means the library-derived artist sweep.
        labels: Vec<LabelRequest>,
        /// Per-run override of `discography.allowed_types`.
        release_types: Option<Vec<DiscographyReleaseType>>,
        /// Per-run lower bound on a release group's first-release-date.
        since: Option<PartialDate>,
    },
}
```

`LabelRequest` is the parsed `--label` element: `{ input: String, mbid: Option<String> }`,
so the runner never re-parses a CLI string and the mode layer owns shape validation.

Discover mode keeps its existing prerequisites: `discography.enabled: true` and a
non-empty `library.paths`. Neither is relaxed for a label run - the first because the
sweep is a MusicBrainz query, the second because "already present" needs a library.

## Config surface

One new key, shaped exactly like the map it sits beside:

```yaml
discography:
  label_mbids:
    "Hospital Records": 8562598d-60d1-4bd6-8b85-4d709e951770
```

Validation, shared with `artist_mbids` through the same helper: a key must not be
empty, two keys must not collide after normalisation, and every value must be a valid
MusicBrainz MBID.

## Provider layer

`src/discography/label.rs` mirrors `src/discography/identifier.rs`.

- `LabelInput::parse(&str) -> LabelInput` classifies `Mbid` or `Name`; `validate()`
  returns the malformed-shape reason for a UUID that does not pass `valid_mbid`;
  `pin_key()` normalises a configured name for map lookup; `cache_key()` returns the
  `label:` namespaced key.
- `LabelCandidate { id, name, score: Option<u8>, disambiguation: Option<String>, label_code: Option<u32> }`.
  `label_code` and `disambiguation` exist to make an ambiguity message actionable.
- `DiscographyProvider` gains three methods, each required rather than defaulted, so a
  provider that quietly returned nothing cannot look like a label with no releases:

```rust
async fn search_labels(&self, label: &str) -> Result<Vec<LabelCandidate>, DiscographyError>;
async fn label_by_id(&self, label_mbid: &str) -> Result<LabelCandidate, DiscographyError>;
async fn releases_by_label(&self, label_mbid: &str) -> Result<Vec<LabelRelease>, DiscographyError>;
```

`MusicBrainzProvider` implements them with the existing request plumbing - pacing,
`Retry-After`/503 pauses, the 4 MiB body cap, transport and decode classification - so
a label query fails exactly as an artist query does.

Wire types:

- `LabelSearchPage { count, offset, labels: Vec<LabelWire> }` with
  `#[serde(rename = "label-count")]` / `#[serde(rename = "label-offset")]` if the
  response carries them, otherwise honest `Option`s.
- `ReleasePage { count, offset, releases: Vec<ReleaseWire> }` renamed from
  `release-count` / `release-offset`.
- `ReleaseWire { id, title, date, status, release_group: Option<ReleaseGroupWire>, artist_credit: Vec<ArtistCreditWire> }`,
  where `ArtistCreditWire { name, joinphrase }` is flattened into the credit string by
  the same `credit_name()` helper the artist path uses for candidate names, so
  `Nu:Tone feat. Stamina MC` is produced by concatenation of names and joinphrases
  rather than by guesswork.

`LabelRelease`, the unit both the provider and the selector use:

```rust
pub struct LabelRelease {
    pub release_id: String,
    pub release_group: ReleaseGroup,
    /// The full credit string, as MusicBrainz spells it.
    pub artist_credit: String,
}
```

A release whose payload has no release group or no artist credit is dropped in the
provider and counted by the caller as `skipped_incomplete`; it is never reconstructed
by guessing an artist or a title.

## Catalogue retrieval

```http
GET /ws/2/release
  ?label=<mbid>
  &status=official
  &inc=release-groups
  &fmt=json
  &limit=100
  &offset=<offset>
```

- `status=official` is sent server-side: it removes bootlegs and promotional copies
  without a client-side guess.
- `type` is deliberately **not** sent. Browse type filtering cannot express secondary
  types (`live`, `compilation`, `remix`, `soundtrack`, `dj mix`, `mixtape`), so
  sending it for some type sets and not others would make the server and
  `release_allowed` two sources of truth. Measured cost on the real label: 1137
  official releases (12 pages) instead of 397 with `type=album` (4 pages), paid once
  per cache lifetime.
- Pagination guards mirror `release_groups` exactly: first page must report offset 0,
  each page's `release-count` must equal the first page's, each page's length must
  equal `min(remaining, PAGE_SIZE)`, no page may contain a duplicate release ID, and
  the collected total must equal the reported count. Any violation is
  `DiscographyError::IncompletePagination` and the cache is not written: a partial
  catalogue looks exactly like "nothing new" and must never be treated as complete.
- `MAX_LABEL_RELEASES` (5000) caps a catalogue that exceeds what one sweep can
  responsibly page through; exceeding it is `IncompletePagination` naming the count.

## Caching and pins

| `discography_cache` column | Value for a label row |
| --- | --- |
| `artist_key` | `label:<pin_key(label)>` |
| `artist_mbid` | the resolved label MBID |
| `canonical_artist` | the resolved label name |
| `release_groups_json` | `LabelCacheEntry { version, label_name, releases: Vec<LabelRelease> }` |
| `fetched_at` | unchanged |

Read path: a fresh row is served as `DiscoveryProvenance::FreshCache`; a stale row is
refreshed; a failed refresh that produced a usable stale row is served as
`DiscoveryProvenance::StaleCache` with a warning naming age and reason. A cached row
whose stored MBID differs from the pinned or requested MBID is not compatible and is
refetched, so a pin change takes effect immediately.

Write path: only after a complete, validated catalogue, and only when
`discography.cache_days > 0`; the payload is size-checked against
`MAX_CACHE_PAYLOAD_BYTES` before insert.

Failure caching: `LabelUnresolved` is recorded in `discography_failures` under
`label:<pin_key>` with `failure_kind = "unresolved"` and honoured for
`discography.failure_cache_days`. `LabelSearchInconsistent` is deliberately **not**
cached - it describes one inconsistent response, not a property of the name.

## Resolution

1. A `discography.label_mbids` pin wins over the input's shape and is fetched by ID.
2. An MBID-shaped input is fetched by ID.
3. A name is searched with `label:"<escaped name>"`; the query is built in the HTTP
   layer, so Lucene syntax never leaves it.
4. The candidates are classified by canonical exact-name match under
   `normalize_catalog_key`; one match is accepted, several are accepted only under the
   dominance rule, zero is refused.
5. A refusal names up to `MAX_LISTED_LABELS` (10) candidates with score, label code
   and disambiguation, so pinning the intended one is a copy-paste away.

## Data flow

```text
--label "Hospital Records,Med School"
  -> mode layer: parse, validate, build ExecutionPlan::Discover
  -> runner: prepare staging, open provider
  -> for each label (in given order)
       resolve label            (pin | by-id | search + dominance)   [cached failure honoured]
       load catalogue           (fresh cache | browse + guards | stale cache) [30-day TTL]
       collect LabelRelease list
  -> label_targets(releases, allowed_types, since, exclude_artists)
       drop excluded credits
       release_allowed
       since bound (undated excluded and counted)
       group by normalised artist credit
       select_albums per credit group  (bundle split, identity dedupe, ordering)
       merge across labels, dedupe by (artist key, album identity key)
       deterministic ordering
  -> library scan (index_from_paths) -> LibraryIndex
  -> split present / missing
  -> for each missing target, oldest first
       process_album(..., LibraryTarget::StagingOnly)
       charge DownloadBudget on a download charge
  -> label_notices -> RunReport
```

## Selection detail

`src/label_discover.rs` is pure: it takes `Vec<LabelRelease>`, the allowed types, the
optional `since` bound and the exclusion list, and returns targets plus counters. It
performs no I/O, so every filter and ordering rule is unit-testable without HTTP or a
filesystem.

`select_albums(credit, groups, allowed)` is reused per artist-credit group rather than
reimplemented for labels. That gives label mode the existing bundle decomposition,
artist-prefix stripping and `album_identity_key` collapsing for free - the label path
cannot drift from the artist path on what counts as one album.

Ordering is total and deterministic: artist key, then the existing partial-date order
(dated before undated, earlier first), then normalised title, then release-group ID.
Two runs over the same catalogue therefore download in the same order, which is what
makes a budget-limited sweep converge instead of oscillating.

## Download and placement

- Every target is processed as `process_album(client, artist, Some(&album), ignore_processed, ..., Some(LibraryTarget::StagingOnly))`.
  The album lands at `<storage.staging_dir>/<artist>/<album>/` via the nested staging
  layout, and nothing is written under `library.paths`.
- `discover.max_cycle_downloads` bounds the run; the loop stops at the same points as
  the artist sweep (budget exhausted, cancellation), recording the artist it stopped on.
- `discover.ignore_processed` and `--ignore-processed` behave as they do for an
  artist-only run: a recorded success is skipped unless the flag bypasses it.
- Every album is charged exactly as an artist-sweep album is (`charges_download_budget`
  on success, `charges_after_error` on failure), so the budget cannot be gamed by
  failed attempts.

## Error handling

| Situation | Behaviour | Reported as |
| --- | --- | --- |
| Malformed UUID shape in `--label` or `label_mbids` | refuse before any request | configuration error naming the flag |
| Label name resolves to no exact match, or ambiguous candidates | skip that label, continue with the remaining labels | `unresolved` list plus a `discography_failures` row |
| Label search response internally inconsistent | skip that label, do not cache the failure | `unresolved` list, uncached |
| Transport, 5xx, decode, oversized body, pagination guard | skip that label | `provider_failed`, counted by the circuit breaker |
| Catalogue exceeds `MAX_LABEL_RELEASES` | skip that label, do not cache a partial catalogue | `provider_failed` with the limit named |
| Three consecutive provider failures | abort the run through the existing `abort_discover_run` | `SeakarrError::MusicBrainz`, as today |
| Library scan fails | warn, continue with an empty index, so every target is treated as missing | warning plus the existing notices |
| Library scan cancelled | stop before any download | cancelled notice, as today |
| No label resolves at all | finish the run with an explanatory notice | notice, exit success |

An unresolved label never counts towards the circuit breaker, and a provider failure
never writes a failure-cache row: the two failure classes stay separate, exactly as
`DiscoveryFailure` already separates them for artists.

## Observability

`LabelCounters` and `label_notices()` mirror `DiscoverCounters`/`discover_notices`,
in this order:

1. Labels resolved, with the resolved names and MBIDs.
2. Labels unresolved, listed; pin advice when more than one is unresolved.
3. Labels whose provider request failed, with reasons.
4. Releases examined, how many were dropped as not of the wanted type, and how many
   were dropped as incomplete (no release group or no artist credit).
5. Releases dropped by `--since`, and releases dropped as undated under `--since`.
6. Releases dropped because their artist credit is excluded.
7. Albums already present in the library.
8. Albums eligible this run.
9. Albums downloaded and staged; albums failed, with reasons.
10. The budget stop point, when the budget ended the run.
11. A closing line stating that staged albums were not written to the library, naming
    `storage.staging_dir`, so the placement rule is never a surprise.

No counter claims staging unless the outcome was
`AlbumOutcome::Downloaded { destination: DownloadDestination::Staging(_) }`, matching
the artist sweep's honesty rule.

## Components and boundaries

| File | Responsibility after this change |
| --- | --- |
| `src/discography/label.rs` (new) | Label identity (`LabelInput`, `LabelCandidate`), resolution, catalogue retrieval with guards, cache read/write, `LabelRelease` and `LabelCacheEntry`. Depends on the provider and the database; knows nothing about download or placement. |
| `src/discography/mod.rs` | Declares `pub mod label;`. Adds `LabelUnresolved` and `LabelSearchInconsistent`; extends `DiscoveryFailure::from_error` and the failure-cache predicate; makes `PartialDate` and `parse_partial_date` `pub(crate)` so `mode.rs` can validate `--since` with the same rules. |
| `src/discography/musicbrainz.rs` | `search_labels`, `label_by_id`, `releases_by_label`, their wire types, and the label/release response parsers. |
| `src/label_discover.rs` (new) | Pure selection (`label_targets`), `LabelTargets`, `LabelCounters`, `label_notices`. Depends only on `discography` types and `LibraryIndex`. |
| `src/config.rs` | `discography.label_mbids` with validation shared with `artist_mbids`. |
| `src/mode.rs` | `--label`/`--release-type`/`--since` parsing and validation; `ExecutionPlan::Discover` gains `labels`, `release_types`, `since`; `LabelRequest`. |
| `src/main.rs` | clap arguments (both `value_delimiter = ','`), override plumbing, dispatch of a label plan to `run_label_discover_mode`. |
| `src/runner.rs` | `run_label_discover_mode` (staging preparation, provider construction) and `run_label_discover_mode_with_provider` (resolve, fetch, select, download, report). The library sweep path is untouched. |
| `src/lib.rs` | `pub mod label_discover;` |

Boundary rules: the provider module owns HTTP and cache; `label_discover` owns filters
and ordering; `runner` owns sequencing and side effects; `mode` owns validation. Each
can be tested without the other, and none of the four grows a second responsibility.

## Testing

Unit - `LabelInput`: name, MBID, malformed UUID, blank, comma-splitting, duplicate
detection after normalisation, pin-key normalisation.

Unit - resolution: single exact match; ambiguous with dominance; ambiguous with tied
top score; top score below `REQUIRED_TOP_SCORE`; margin below `REQUIRED_SCORE_MARGIN`;
zero matches; a pinned MBID winning over a name; `label_by_id` skipping search
entirely. The five-candidate `Hospital Records` shape is a fixture.

Unit - provider: page parsing, `release-count`/`release-offset` handling, missing
release group dropped, multi-part artist credit joined (`names` plus `joinphrase`),
first-page offset not zero, count drift between pages, short page, duplicate release
IDs, `MAX_LABEL_RELEASES` exceeded, 4 MiB body cap, 503 with `Retry-After`.

Unit - cache: fresh hit, stale refresh, stale served on refresh failure, failure
replay, failure bypass for a pinned MBID, MBID mismatch refetched, oversized payload
rejected.

Unit - selection: type filter per variant; `album` alias equals `studio_album`;
`--since` inclusive boundary at year, month and day precision; undated excluded under
`--since` and counted; undated admitted without `--since`; excluded credits dropped;
two labels producing one album collapse to one target; cross-artist identity
collapsing; deterministic ordering across shuffled input.

Validation - every refusal in the CLI table, one test each, asserting the message names
the flag.

Integration (mock provider and mock Soulseek client): a label run stages every eligible
album and leaves `library.paths` byte-identical; a present album is skipped; the budget
stops the run at the configured number; an unresolved label does not stop the run; three
consecutive provider failures abort with the existing error; the summary carries the
staging notice.

End-to-end (`tests/pipeline_test.rs`): one label fixture through `--mode discover
--label ... --release-type album`, asserting the staged tree and that no library folder
was created.

Gate: TDD with a genuine RED phase, cognitive complexity ceiling 25, `cargo clippy
--all-targets -- -D warnings`, `cargo fmt --check`, the full test suite, the repository's
95% line-coverage floor, `cargo audit`, `cargo deny`, and the pre-commit hooks.

## Documentation

- `README.md`: the discover bullet gains the label sweep - `--label`, the name-or-MBID
  rule, `--release-type`, `--since`, staging-only placement, the 30-day cache, and the
  fact that a label run replaces the library sweep for that invocation.
- `README.md` configuration reference: `discography.label_mbids`.
- The flag table and the mode/selector compatibility table list the three new flags and
  the `--label` conflicts.

## Accepted consequences

1. A newly released album can take up to `discography.cache_days` (30) to appear in a
   sweep. A run that finds the cache older than that refreshes it; when the refresh
   fails, the stale catalogue is used with a warning and the age named.
2. The full official catalogue is paged on a cold or stale cache (12 pages for the
   example label), because type filtering stays client-side.
3. A release group MusicBrainz has no artist credit for is skipped and counted, never
   attributed by guesswork.
4. `--since` is a lower bound only; there is no upper bound and no "newest N" selector.
5. A label whose catalogue exceeds `MAX_LABEL_RELEASES` cannot be swept until the
   ceiling is raised deliberately in code.
6. Multi-artist credits produce multi-artist staging folders, which the operator may
   need to merge by hand.
7. Label discovery cannot see an artist that has no release of the wanted type on the
   label, even if the label has signed them.

## Acceptance criteria

1. `seakarr --mode discover --label "Hospital Records"` resolves that label to the
   MusicBrainz entity for the UK drum & bass label, or refuses with candidates listed;
   it never silently picks a lower-scored namesake.
2. `--label` with an MBID performs no label search request.
3. `--release-type ep` downloads EPs only; omitting the flag downloads
   `discography.allowed_types`.
4. `--since 2025-01-01` excludes release groups first released before that date and
   excludes undated ones, both counted in the summary.
5. Every label-sourced download is present under
   `<storage.staging_dir>/<artist>/<album>/` and nothing is written under
   `library.paths`.
6. An album already in the library is not searched, not downloaded, and is counted as
   present.
7. `--label` and `--artist` together refuse with a message naming both flags; so do
   `--release-type` and `--since` without `--label`.
8. Three consecutive MusicBrainz provider failures abort the run through the existing
   breaker; an unresolvable label does not.
9. A second run inside `discography.cache_days` makes no catalogue request and reports
   the fresh-cache provenance.
10. The library-derived artist sweep behaves exactly as before when `--label` is
    absent: same selection, same notices, same placement.
