# Bundled release-group decomposition

## Problem

MusicBrainz publishes release groups whose title is several albums joined by a
spaced slash. They are real entities, not malformed data. Queried live on
2026-09-17, the artist Archive (MBID `bd513de0-e42f-425e-ae46-817d7bc5fb1c`)
returns a release group titled `Controlling Crowds / You All Look the Same to
Me`, primary type Album, first release 2014, MBID
`3eecc2ca-8d9e-4159-8bee-cd84173a5fba`. Each part is also a release group in its
own right: `Controlling Crowds` (Album, 2009-03-27) and `You All Look the Same
to Me` (Album, 2002, and an EP in 2001).

`select_albums` has no concept of a bundle, so the concatenated title becomes an
`AlbumTarget`, the runner searches it as-is, every search tier returns nothing,
and the same cost is paid on every run:

Abridged from the reported run, where `{title}` is the concatenated string:

```text
INFO seakarr::runner: Processing: Archive — {title}
INFO seakarr::search: Searching for Artist + Album (Archive {title})
INFO seakarr::search: Searching for Artist + Album lowercase (...)
INFO seakarr::search: Searching for Artist + Album punctuation-normalised (...)
INFO seakarr::search: Searching for Album ({title})
INFO seakarr::runner: No results for Archive — {title}
```

Nothing is faulty in the search path. A peer lists one album per folder or
filename, so no share ever matches a title that names two albums, and the
punctuation-normalised tier only collapses punctuation, it does not remove the
second album name.

The symptom is not rare or Archive-specific. Sampling every release group of
five artists (Archive, Jamiroquai, Bob Dylan, Metallica, Oasis) on 2026-09-17
found 16 titles containing a slash, 13 of them with a spaced slash, and six of
those 13 were bundles: two each for Archive, Jamiroquai and Bob Dylan.
Jamiroquai even has a three-album bundle. Every bundle target is an unsearchable
title that is re-searched on every discover run.

The real defect is narrower than "MusicBrainz returns strange results": the
discography resolver treats a release group that bundles other release groups as
if it were an album, and so fabricates a target that duplicates albums it has
already selected and can never be downloaded.

## Goal

A discover run or an artist-only run must not produce an album target whose
title names more than one album, when MusicBrainz already lists each named album
as its own release group for that artist. The constituent albums keep being
selected, searched and skipped-when-present exactly as they are today.

Nothing else changes: the same release-type filter, the same dedupe and
ordering, the same cache, the same counters, the same reporting, and the same
search path.

## Scope

In scope:

- a pure rule that recognises a bundled release-group title;
- one call site in `select_albums`, which serves discover mode and artist-only
  runs;
- unit tests over real MusicBrainz titles plus a syntactic edge table;
- integration tests proving target selection and logging;
- README notes in the `discography` configuration section and in the
  authoritative-discovery narrative of `## How it works`.

Out of scope:

- separators other than a slash with whitespace on both sides. `+`, `&`, `;`,
  ` vs `, ` and ` and unspaced slashes are not separators.
- fuzzy part matching. No disc/format-prefix stripping (`2cd:`), no
  punctuation-insensitive comparison, no `&`/`and` equivalence.
- rewriting titles inside the MusicBrainz provider before they are cached.
- the discography cache payload, its TTL, and any cache migration.
- explicit album input: `--artist X --album Y` and batch wantlist lines keep
  searching exactly the text the user supplied.
- repairing or renaming bundled albums already on disk.
- the search path, `search::album_identity_key`, `discover::contains_album` and
  every naming or sanitisation concern.

## Decisions

**A bundle is recognised by structure plus evidence, never by structure alone.**
A title is a bundle only when every part, after the existing
`normalize_catalog_key` (NFKC, lowercase, whitespace collapse), equals the title
of another release group in the same response for that artist, never the
bundled title itself. Structural splitting alone would destroy genuine titles:
Metallica publishes an Album whose title is a venue and a date joined by a
spaced slash,

`Live at Wembley Stadium, London, England / April 20th, 1992`

and Oasis publishes an A/B single, `Little by Little / She Is Love`. Both are
preserved by the evidence requirement, because none of their parts is a
release-group title.

Evidence from the 2026-09-17 sample (long titles wrapped for display):

```text
result  type    artist         title
SPLIT   Album   Archive        Controlling Crowds / You All Look the Same to Me
SPLIT   Album   Archive        You All Look the Same to Me / Noise
SPLIT   Album   Jamiroquai     Emergency on Planet Earth /
                               The Return of the Space Cowboy /
                               Travelling Without Moving
SPLIT   Album   Bob Dylan      The Freewheelin' Bob Dylan /
                               The Times They Are A-Changin' /
                               Another Side Of Bob Dylan
KEEP    Album   Metallica      Live at Wembley Stadium, London, England /
                               April 20th, 1992
KEEP    Album   Bob Dylan      2cd: Highway 61 Revisited / Blonde on Blonde
KEEP    Album   Bob Dylan      Time Out of Mind / Love and Theft
KEEP    Album   Elliott Smith  Either/Or
KEEP    Album   Elliott Smith  Alternate Versions From either/or
KEEP    Album   John Lennon    John Lennon/Plastic Ono Band
KEEP    EP      John Lennon    Elton John / John Lennon
KEEP    Single  Archive        Wiped Out / Violently
KEEP    Single  Oasis          Little by Little / She Is Love
```

Each `KEEP` row is a title the rule refuses to decompose, for a recorded
reason: neither part is a release-group title (`Wiped Out / Violently`,
`Little by Little / She Is Love`), the parts are a venue and a date, a disc
prefix blocks a part (`2cd:`), a part is spelled differently from the canonical
release-group title (`Love and Theft`), the slash has no whitespace around it
(`Either/Or`, `John Lennon/Plastic Ono Band`, `Alternate Versions From
either/or`), or the parts are two artists (`Elton John / John Lennon`).

The rule is therefore conservative by construction: a title that is not clearly
a bundle of known albums is kept, which is exactly today's behaviour.

**Whitespace on both sides of every slash is required.** Without it the rule
would split `Either/Or`, `John Lennon/Plastic Ono Band` and `Alternate Versions
From either/or`, all of which are single titles. Requiring whitespace also keeps
the rule independent of any separator list, since an unspaced slash is never a
separator.

**Parts are matched exactly, after `normalize_catalog_key`.** That function
already folds NFKC, lowercases and collapses whitespace, so `G N' R Lies` and
`G n'r Lies` compare equal without new machinery. Looser matching was rejected:
discarding punctuation makes `Love & Theft` and `Love and Theft` compare equal,
which can split a title that is genuinely one album.

**The bundle is replaced by its parts, and the parts need no new targets.**
Under the rule, every part is already a separate release group of the same
artist, so it is selected on its own, keeps its own MBID and release date, and
its search uses its own canonical title. Both alternatives were rejected:

- keeping the bundle as an additional target retains the unsearchable search and
  its `No results` line, which is the cost being removed;
- synthesising targets for the parts invents release groups MusicBrainz did not
  offer and would search a part that `discography.allowed_types` excludes,
  contradicting the configured release-type filter.

**Only MusicBrainz-driven runs are affected.** The rule lives in
`select_albums`, the single place that converts release groups into targets, and
its only caller is `discover_artist_albums`. That covers discover mode and an
artist-only run, and it also covers the fresh-cache and stale-cache paths,
because those re-run selection over the cached raw release-group list. Explicit
album input and batch lines are user intent and have no release-group list in
hand to validate against, so they are left alone.

**The rule lives in a new pure module.** `discography/mod.rs` is 2,786 lines, of
which roughly 1,840 are tests, and the repository already has a precedent for a
small dedicated rule module in `src/discs.rs` (292 lines). A post-pass over the
selected target list was rejected because it would re-derive what
`select_albums` has just filtered and deduped; rewriting titles in the provider
was rejected because it would write invented titles into the 30-day discography
cache, so a later correction to the rule would not reach rows cached before it.

## Configuration

No new configuration keys, no schema change and no new CLI flags. The rule is
unconditional and applies wherever `select_albums` runs. `discography.enabled`,
`discography.allowed_types`, `discography.cache_days` and
`discography.failure_cache_days` keep their current meaning.

## Architecture

### `src/discography/bundle.rs` (new)

Declared as `mod bundle;` next to the existing `mod musicbrainz;`. Two total,
pure functions, no I/O and no state:

- the syntactic decomposition, which NFKC-folds the title, splits it on `/`,
  and returns the trimmed parts only when there are at least two of them, none
  is empty, and every slash has whitespace on both sides, otherwise nothing;
- the semantic check, which takes a title and the artist's normalised title set
  and reports true only when the decomposition succeeds and every part is
  present in that set.

Splitting works on `/`-separated segments and inspects neighbouring characters,
so it never slices at a byte offset and cannot panic on a multi-byte title. The
module owns its own unit tests.

### `src/discography/mod.rs`

`select_albums` gains a single pre-pass and one early skip:

- build the normalised title set once, from the unfiltered group list, so a part
  counts as known even when its own release group is later excluded by
  `release_allowed`;
- inside the existing loop, after the empty-title check, skip any group the
  semantic check reports as a bundle, logging the exclusion with the existing
  `log_rejected_release` helper at debug level and a distinct reason.

Everything else in the loop - `release_allowed`, the title-key dedupe, the
dated-before-undated ordering, the MBID tie-break - is untouched.

No other production file changes. `discover.rs`, `runner.rs`, `search.rs`,
`discography/musicbrainz.rs` and the database layer are unaffected.

## Data flow

```text
discover mode / artist-only run
        |
        v
discover_artist_albums  ──▶  release groups
        |                   (fresh fetch, fresh cache or stale cache)
        |                              |
        |                              v
        |                    select_albums(groups, allowed_types)
        |                              |
        |            build normalised title set from all groups
        |                              |
        |            per group: release_allowed -> empty title
        |                       -> bundle? -> dedupe
        |                              |
        v                              v
DiscoveryOutcome::Authoritative { albums }   no bundle title among the targets
        |
        v
discover::missing_albums(index, artist, targets)
        |
        v
runner: one "Artist Album" search per remaining target, under each part's own
        canonical title, skipped when the library already holds that album
```

## Error handling

There is nothing to fail. Both functions are total and pure, so no error variant
is added, no `Result` is introduced and no I/O is performed. Every ambiguous or
unrecognised shape resolves to *keep the release group as a target*, which is
today's behaviour, so the worst outcome of a misjudgement is the status quo
rather than a new failure mode.

## Backward compatibility

- No config, schema, cache-format or CLI change, so an existing install needs no
  migration.
- Existing cached discographies are fixed without expiry or refetch, because the
  cache stores raw release groups and selection runs on every load.
- The set of targets can only lose bundled titles. Every part of a bundle is
  already selected under its own name, so no album that was reachable before
  becomes unreachable, with one documented exception below.
- `processed_albums` and `search_history` rows for a bundled title are left in
  place. They simply stop being consulted, since the title is no longer a
  target, and they expire with the existing retention rules.

## Deliberate limits

- **A part excluded by `allowed_types` becomes no target at all.** If a bundle
  is an Album but one part exists only as an EP, and `allowed_types` is
  `[studio_album]`, that part is not selected. The bundle search it replaced
  never returned results, so no reachable coverage is lost, and honouring the
  type filter is the intended behaviour.
- **A bundle whose parts are spelled differently from the canonical titles is
  kept.** Bob Dylan's `2cd: Highway 61 Revisited / Blonde on Blonde` and
  `Time Out of Mind / Love and Theft` keep failing their search, exactly as
  today. Tolerating a disc prefix or `&`/`and` variation is a separate decision
  and a separate change.
- **Only a spaced slash is a separator.** Titles joined any other way are out of
  scope.
- **Three or more parts are supported, but not depth.** A part that is itself a
  bundle is not decomposed further. Its own parts are still selected separately,
  so the content remains covered.
- **Albums already on disk under a bundled folder name are not touched.** The
  library-presence check compares whole normalised titles and is unchanged, so a
  folder named after a bundle neither matches nor blocks the parts.

## Testing

Unit tests in `bundle.rs`:

- the list of real MusicBrainz titles above, split and kept as sampled, so the
  rule is pinned to the data that motivated it;
- a syntactic edge table: empty and whitespace-only titles, a lone ` / `, a
  trailing slash, a leading slash, consecutive slashes producing an empty part,
  unspaced slashes, extra surrounding whitespace, a fullwidth slash folded by
  NFKC, and multi-byte parts;
- the evidence requirement itself: a spaced-slash title whose parts are not
  release-group titles is kept, and a title is never a bundle of itself.

Integration tests in `discography/mod.rs`, using the existing `group` test
helper and `test_support::LogCapture`:

- a group list modelled on Archive: the bundle is absent from the selected
  titles while both constituent albums are present, with each part keeping its
  own MBID;
- a group list modelled on Metallica: the venue/date title stays a target;
- a bundle whose parts are EPs is dropped under `[studio_album]`, pinning the
  documented limit;
- the exclusion is logged with its distinct reason;
- the existing `select_albums` tests, including dedupe, ordering and
  `excluded_release_groups_are_logged_with_reasons`, keep passing unchanged.

Repository gates: `cargo test`, `cargo clippy`, `cargo fmt --check` and the
configured coverage floor.

## Acceptance criteria

- A discover run or artist-only run for Archive logs no `Processing: Archive —
  Controlling Crowds / You All Look the Same to Me`, and its MusicBrainz targets
  no longer include that title.
- `Controlling Crowds` and `You All Look the Same to Me` remain targets, each
  searched under its own title, and each skipped when the library already holds
  it.
- Metallica's `Live at Wembley Stadium, London, England / April 20th, 1992`,
  `Either/Or` and every A/B single in the sample remain single targets.
- Selection for an artist with no bundled titles is byte-for-byte identical to
  today.
- `--artist X --album Y` and batch lines are unaffected.

## External contracts

None. No HTTP request changes, no MusicBrainz query changes, no database schema
change, no configuration surface change and no output format change beyond the
removal of an unsearchable target and its `No results` line.

## References

- MusicBrainz release-group browse response for Archive, 2026-09-17:
  `GET /ws/2/release-group?artist=bd513de0-e42f-425e-ae46-817d7bc5fb1c&limit=100&fmt=json`
  (98 release groups, two of them bundles).
- MusicBrainz release-group search for `Controlling Crowds` under Archive,
  2026-09-17: five matches, including the Album
  `Controlling Crowds / You All Look the Same to Me` (2014).
- `src/discography/mod.rs` - `select_albums`, `normalize_catalog_key`,
  `normalize_album_key`, `release_allowed`, `select_outcome`.
- `src/discs.rs` - precedent for a small, dedicated, pure rule module.
- `docs/agent/specs/2026-09-17-discography-failure-cache-design.md` - the
  preceding change to the same subsystem, and the source of the "the cached
  payload is the full release-group list and filtering runs locally" contract.
