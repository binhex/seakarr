<!-- markdownlint-disable MD013 -->
# Remove `storage.organize` and `storage.organize_pattern`

## Problem

Two configuration keys have outlived their purpose. `storage.organize` (`src/config.rs:89`)
gates a per-file library write inside `process_album_internal`, and `storage.organize_pattern`
(`src/config.rs:91`) supplies the naming template every library write expands. The operator
never uses either, and manual and discover runs already place albums through the newer
placement path, so the pair now only adds a second writer, a template language, and a
validation block to maintain.

Removing the flag is not a pure deletion, because the organize step is the **only** library
writer for two whole modes:

- auto mode passes `None` as the target whenever `library_upgrade.enabled` is false
  (`src/runner.rs:1300`), which is the shipped default, and falls back to the organize step;
- batch lines pass a target too, and the organize step is what files them.

Deleting the step without replacement would quietly stop filing those downloads, leaving
`storage.staging_dir` as the end of the road. The `organize_pattern` is worse: it is not a
flag over the machinery but the machinery itself. `expand_pattern` (`src/organizer.rs:119`)
and `placement_album_dir` derive both the album folder and the file names inside it, for
placement, for the auto-mode upgrade copy-back (`src/runner.rs:284`, `:950`) and for the
organize step, and `config.validate` rejects patterns that are not a relative path inside the
library (`src/config.rs:940-996`).

A third, independent defect in the same area decides the shape of the replacement. Placement
resolves an artist's folder by listing each configured root's immediate children
(`discover::resolve_artist_folder`, `src/discover.rs:216-257`), and `place_into_library`
requires that folder name to be a single path component (`src/organizer.rs:486-493`). The
operator's library is `/media/Music/<user>/<type>/<genre>/<subgenre>/<artist>/<album>` — five
levels above the artist folder — so the shipped manual placement can never find an artist
folder there and every manual run stays in staging. The library scan does not share this
limitation: it records the directory **above** the artist folder as `ScannedAlbum::path`
(`src/scanner.rs:445-455`), which discover mode passes straight to `LibraryTarget::Place`
(`src/runner.rs:2130-2134`), so discover mode is already correct for nested layouts.

## Goal

Remove `storage.organize` and `storage.organize_pattern` from the configuration, the code and
the documentation, and replace what they did with the one placement mechanism that already
exists, without changing where a default-configured library ends up on disk. Make that
mechanism able to find artist folders in a deep genre/type tree, so automatic runs file their
albums into the library the way the operator has laid it out.

## Scope

In scope:

- delete `storage.organize` and `storage.organize_pattern` from `StorageConfig`, including
  `default_organize_pattern` and the pattern validation block in `config.validate`;
- delete the organize step inside `process_album_internal`, its duplicate incomplete-download
  refusal path, and the `organize_allowed` gate;
- delete `organizer::organize_file`, `OrganizeInput`, `expand_pattern`,
  `expand_pattern_inner`, `placement_album_dir`, and the `pattern` parameter threaded through
  `LibraryWrite`, `copy_into_library` and `place_into_library`;
- fix the naming convention to the current default, so every write produces the same paths as
  today;
- replace the direct-child artist lookup with a deep, per-run cached one, and use it for
  manual, auto and batch targets;
- turn the album-only guard into a warning, and update the README's organize passages.

Out of scope:

- `library_upgrade.enabled` and the `LibraryTarget::Upgrade` copy-back path, which keep their
  behaviour and only lose the pattern argument;
- discover mode's targets, which are already nested-correct and come from the scan;
- the library scan, the presence index, and the `min_tracks` refusal;
- every dated spec and plan in `docs/specs/`, `docs/plans/` and the earlier `docs/agent/`
  files, which stay as the record of what was designed and shipped at the time;
- creating artist folders for a new artist (see Deliberate limits).

## Decisions

1. **Auto and batch place only into an artist folder that already exists.** The operator's
   tree is deeper than any inference can handle: the user name, type, genre and subgenre
   components above the artist are not derivable from album metadata, and the old organize
   step did not try — it wrote to `<library.paths[0]>/<%artist%>/<%album%>`, flat under the
   first root. So the deep lookup finds the folder or the album stays in staging.
2. **Naming stays exactly as it is today.** The album folder is the sanitised album title
   under the artist folder, files are `<NN> - <Title>.<ext>` with the leading track token
   stripped, and a disc subfolder is preserved verbatim from staging. This is byte-identical
   to what the current default pattern produces, so existing libraries and the upgrade
   copy-back keep agreeing, and only the ability to override the layout goes away.
3. **First deterministic match wins, and an ambiguous match warns.** Roots are searched in
   configuration order; within a root the walk is depth-first over alphabetically sorted
   directory names. When more than one folder matched, one WARN names the folder chosen and
   the candidates skipped, so a placement into the Singles tree instead of Albums is visible.
4. **One lazy cached walk per run.** The artist index is built on the first album that would
   place, so a run that places nothing never walks the tree, and every later album in the same
   run reuses it. The walk reads directory names only — never audio files, tags or content.
5. **Album-only auto and batch lines warn and keep the album in staging.** Placement needs an
   artist folder, so an album-only line cannot place; a warning is visible without turning a
   previously working batch file into a hard failure. Manual album-only runs keep their
   existing silent staging behaviour.
6. **The configuration keys are removed, not deprecated.** The structs carry no
   `deny_unknown_fields`, so a config file that still contains them keeps loading; the existing
   reconciliation (`Config::load`) writes the file back without them on the next load, keeping
   a `.yml.bak` backup and the existing `config reconciled with current schema` INFO line. No
   new migration code, no warning of its own.
7. **Existing keys and defaults are otherwise untouched.** No new configuration key is added
   for the lookup or the naming; the depth of the operator's tree is discovered at run time
   rather than configured.

## Architecture

### `src/config.rs` — the keys and their validation

`StorageConfig` keeps `staging_dir` and loses `organize` and `organize_pattern`, with
`default_organize_pattern` and the whole pattern validation block (relative-path check,
`..` check, trailing-separator check and its tests) deleted alongside them. Because the
generated config is produced by serialising the default (`Config::load`,
`serde_yaml::to_string`), both keys disappear from a freshly generated file, and the
reconciliation drops them from an existing one.

### `src/discover.rs` — the deep artist lookup

`resolve_artist_folder` is replaced by a per-run index of artist folders, built from the
configured roots:

```rust
/// Every artist folder under the configured roots, built on first use and reused
/// for the rest of the run. Ranking is configuration order, then the depth-first,
/// alphabetically sorted walk order.
pub struct ArtistFolderIndex {
    /// Normalised artist key to the folders that matched, best first.
    folders: BTreeMap<String, Vec<(PathBuf, String)>>,
    /// False until the first lookup walks the roots; a run that never places
    /// never builds the index.
    built: bool,
}

impl ArtistFolderIndex {
    /// Empty and unbuilt: no filesystem access happens until the first lookup.
    pub fn new(config: &Config) -> Self;
    /// The parent directory and on-disk name of the artist's folder, or `None`.
    /// Builds the index on the first call; warns once when several folders
    /// matched and names the one chosen.
    pub fn find(&mut self, artist: &str) -> Option<(PathBuf, String)>;
}
```

`find` builds the index on first call, then answers from it. Building walks each root's
directories depth-first with sorted entries, records every directory whose sanitised,
normalised name is non-empty as a candidate artist folder, and prunes nothing — no
file is opened, no tag is read, and an album folder is simply a name that only matches when an
artist really carries that name. Lookup keeps today's guards: a blank artist and a name that
sanitises to the placeholder resolve to `None`, a non-UTF-8 folder name is skipped rather than
matched lossily, and a root that cannot be listed is warned about and skipped while a missing
root is silently skipped. Ranking is configuration order, then walk order as decided above; a
lookup that finds more than one folder emits the ambiguity WARN described in Decisions 3.

The return shape is deliberately the same `(parent, name)` pair the scan produces and
discover already passes to `LibraryTarget::Place`, so the placement plumbing, the
single-component check and the existing-album gate need no change.

### `src/organizer.rs` — fixed naming, no template

The placeholder language goes: `expand_pattern`, `expand_pattern_inner`, the per-placeholder
substitution and the `%user%` placeholder (which already expanded to the literal `"unknown"`
in every library-write path). `LibraryWrite` loses its `pattern` field, and `copy_into_library`
composes the destination directly — artist component verbatim as today, then the sanitised
album title, then `NN - Title.ext` derived by the existing single source of truth for naming,
then the staging disc subfolder when one exists. `placement_album_dir` collapses to
`artist_dir.join(album)` with the same sanitisation, and `organize_name_from_stem` is renamed
to say what it now is (library naming, not organize naming) with its documentation updated.

`place_into_library` keeps its signature minus the pattern. `organize_file` and `OrganizeInput`
are deleted: they existed only for the organize step.

### `src/runner.rs` — one writer, computed targets

The organize block (`src/runner.rs:1121-1182`), `organize_allowed`, and the second refusal path
inside it are deleted, as is the `library_album_dir` bookkeeping that existed to carry the
organize step's result into the completion line. What remains is the `Place` arm, which already
handles the refusal, the write, the partial-write warning and `finish_library_write`.

Targets are computed per mode:

- manual: `manual_place_target` keeps `skip_existing_album: true` and now calls the deep
  lookup, so an artist five levels down resolves where it previously resolved to nothing;
- auto and batch: a new helper resolves the same lookup with `skip_existing_album: false` and
  returns `LibraryTarget::StagingOnly` when the artist has no folder. Auto's
  `library_upgrade.enabled` branch keeps `LibraryTarget::Upgrade` and takes priority;
- discover: unchanged, still built from the scan's `album_location`.

The album-only guard (`src/runner.rs:519-531`) loses its flag term and its early return:
instead of failing the run it emits the naming-neutral warning and lets the album download
into staging. The partial-write warning that told the operator to check
`storage.organize_pattern` is reworded to describe the fixed layout.

### Documentation and wording

`README.md` loses both rows from the storage table, the organize step from the mode
descriptions and the run sequence, the `storage.organize` conditions in the completion-line
section, and the `Organized:` log example; the data-flow diagram and the placement passages
that mention the flag or the pattern are rewritten around placement alone. Doc comments that
name the organize step in `filter.rs`, `client.rs`, `test_support.rs`, `scanner.rs`,
`discs.rs`, `search.rs`, `discography/mod.rs` and `lib.rs` are reworded without behavioural
change. The module name `organizer` stays: it names the library-writing module, not the
removed step.

## Data flow

An automatic run with a completed album, after this change:

1. search, candidate ranking and download are unchanged, and the album lands in
   `storage.staging_dir/<artist>--<album>/`;
2. the completeness check (`min_tracks`, numbering) runs exactly as it does for placement
   today, and a refused set keeps its staging copy;
3. the artist lookup runs (building the index on first use);
4. **found:** the album is placed into `<artist folder>/<album>`, existing readable files are
   kept, missing ones are written, and staging is removed once files were written;
5. **not found:** the album stays in staging and the run logs the explanation once, as manual
   runs already do.

## Error handling

Placement failures keep today's behaviour: an incomplete download is refused with the
per-album and peer consequences already implemented, an I/O failure during the write marks the
album failed and leaves the staging copy in place, and a partial write warns with the counts.
New and changed:

- ambiguous artist match → one WARN naming the chosen folder and the skipped candidates;
- album-only auto or batch line → WARN, album stays in staging, run continues;
- unlistable root → WARN and skip (existing behaviour, now shared by the deep walk);
- blank artist or a name sanitising to the placeholder → no placement and no misleading
  explanation line;
- stale config keys → dropped by the existing reconciliation with a `.yml.bak` backup.

## Backward compatibility

- **Configuration:** a file containing `storage.organize` or `storage.organize_pattern` keeps
  loading; the keys are ignored and removed on the next schema reconciliation. Values are not
  translated because there is nothing to translate them into — placement's layout is the
  default the patterns already expressed.
- **On-disk results:** unchanged for a default-configured library. Album folders and file names
  are identical, and the upgrade copy-back writes the same names it writes today.
- **Behaviour:** two deliberate changes. Album-only auto and batch lines warn and stay in
  staging instead of hard-failing. Automatic runs place only into an existing artist folder and
  never create one, so an artist with no folder keeps its downloads in staging.
- **New capability:** runs on a nested library (genre/type/subgenre above the artist) place
  where they previously could not, because the lookup now descends past a root's immediate
  children.

## Deliberate limits

- A brand-new artist is never created in the library: the run cannot know the user, type,
  genre or subgenre components, so those albums stay in staging until the operator creates the
  artist folder.
- The lookup matches folder names, not tags: a library whose artist folders are named
  differently from the artist will not match, exactly as today's resolver behaves.
- One walk per run reads directory names across all configured roots; a very large tree makes
  the first placement of a run slower than a single directory listing. Nothing is read beyond
  directory entries, and a run that places nothing pays nothing.
- The match ranking is first-in-walk-order, not shallowest-first: within one root, a depth-first
  walk can prefer a deeper folder. The ambiguity warning exists so a surprising choice is
  visible rather than silent.
- Custom naming is gone. A pattern such as `%album%/%artist%/...` is no longer honoured.

## Testing

Unit tests, all through the public API:

- the deep lookup: an artist folder nested several levels below a root is found and returned as
  `(parent, name)`; roots are searched in configuration order; sorted depth-first order decides
  between two matches in one root; more than one match warns and still places; a blank artist,
  a placeholder-sanitising name and a non-UTF-8 folder are skipped; a missing root is skipped
  while an unlistable root warns; the index is not built when nothing looks up;
- manual runs: a nested artist folder receives the album with `skip_existing_album` semantics
  unchanged, and an absent artist folder keeps the download in staging with the explanation;
- auto and batch runs: an album places into an existing nested artist folder; an artist with no
  folder stays in staging; an album-only line warns and stays in staging;
- naming: the fixed convention reproduces today's results (track zero-padding, leading track
  token stripped, disc subfolder preserved) and the upgrade copy-back writes the same paths;
- config: a file carrying both removed keys loads, the keys are absent after reconciliation,
  a backup is written, and the default config no longer serialises them.

Named red-green mutations to prove the tests bite: remove the lookup's nested descent (nested
placement must fail), remove the ambiguity warning (it must be observed), invert the
`skip_existing_album` flag for automatic runs (the existing-album rule must break), and restore
the album-only error (the warning test must fail).

## Acceptance criteria

1. `storage.organize` and `storage.organize_pattern` do not appear in `src/`, `tests/` or
   `README.md`, and a freshly generated config contains neither key.
2. No code path writes to the library except placement: `organize_file`, `OrganizeInput`, the
   pattern engine and the organize step are gone, and no behaviour depends on a removed flag.
3. An automatic run on a nested library places a completed album into the existing artist
   folder and creates nothing new; with no artist folder the album stays in staging with one
   explanation line.
4. A default-configured library receives byte-identical paths and file names to those produced
   before the change, verified by the naming tests.
5. `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo check --all-targets`,
   `cargo test`, coverage at or above the project floor, `cargo audit`, `cargo deny check`,
   markdownlint and pre-commit all pass.

## Documentation

`README.md` is the live reference and is rewritten for both removals. The new spec and the
existing `docs/agent/specs/2026-09-23-manual-library-placement-design.md` remain in place as
the record of what was decided; the latter's organize references describe the state at the time
it shipped and are deliberately not rewritten.

## Alternatives considered

- **Keep the organize step and only delete the flag** (always write when a library path exists):
  the smallest diff, but it keeps two writers, the template engine and the per-file naming path
  alive forever, which is the complexity the operator asked to remove.
- **Reuse the discover scan index for the lookup:** the index is built from a full library scan
  that reads tags for every audio file, and it only lists artists the walk found audio for, so a
  stale or failed scan would silently change where a download lands. The directory-names-only
  walk is cheaper and independent of the scan.
- **Depth-limited walk (for example six levels):** bounds the cost of an artist with no folder,
  but the operator's artist folders already sit five levels down, so the limit would be one
  level from silently failing and would break as the tree deepens.
- **Keep the template engine with one hardcoded pattern:** preserves the expansion code and its
  tests while removing the configuration, leaving a template language with a single constant
  caller — pure maintenance cost for a behaviour nobody can change.
- **Per-album walk with early exit:** cheaper for one shallow match, but a batch run with many
  artists pays one walk per album, and every miss walks the whole tree; the cached index pays at
  most one walk per run.
