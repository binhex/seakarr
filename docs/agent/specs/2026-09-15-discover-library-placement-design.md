# Discover-mode library placement

## Problem

`discover` mode downloads albums into the staging directory and leaves them
there. The copy-back branch inside `process_album_internal` is wrapped in
`if config.library_upgrade.enabled` and reached only when a target library path
is supplied; discover passes `None`, so a completed album never reaches the
library. The only other writer is the generic organize block, which requires
`storage.organize: true` and writes under `library.paths[0]` — the wrong root
for any nested library such as `Music/<user>/Albums/<genre>/<style>/<artist>/`.

The visible effect is that discover fills the staging directory instead of the
library: files sit in `downloads/staging/<artist>--<album>/`, the library is
never extended, and a later run reports the album as still missing. A
subsequent auto run's recovery scan can then delete those leftover directories
as debris of an interrupted upgrade.

## Goal

Every album that discover mode completes is placed into the artist's own
library folder — the directory the artist's existing albums were found in —
using the configured organize pattern, and the staging copy is removed once the
copy succeeds. Nothing else changes: auto, artist-only manual, explicit manual,
and batch behaviour, the presence index, the download budget, and the reporting
contract all stay as they are.

## Scope

In scope:

- library placement of completed discover downloads;
- the destination pair (library root and artist directory) carried by the
  library index;
- disc-folder-aware path resolution in the scanner, so a multi-disc album's
  artist folder and library location are derived correctly;
- an explicit `LibraryTarget` type replacing the loose target parameter on
  `process_album`;
- unit, scanner, and runner tests, plus README documentation.

Out of scope:

- placement for artist-only manual, explicit manual, or batch runs;
- quality-based deletion (`library_upgrade.delete_lesser_quality`) on the
  placement path;
- presence or completeness semantics, including partial-album completion;
- upgrade recovery (`recover_interrupted_upgrades`) changes;
- any new configuration key or YAML schema change;
- per-artist destination overrides, or a destination other than the artist's
  existing folder.

## Decisions

### Placement is unconditional in discover mode

Discover mode exists to fill the library, so a completed album left in staging
is never the desired outcome. Placement therefore does not depend on
`storage.organize` and does not depend on `library_upgrade.enabled`. Both flags
keep their current meaning for the modes that already honour them.

The alternative — reusing `library_upgrade.enabled` — was rejected because that
key means "re-download albums that already exist but fail the quality gate",
which is not what is happening here, and because an installation with the flag
off would keep stranding files. A new `discover.place_downloads` key was
rejected as unnecessary: no caller wants the old behaviour.

### The destination is the artist's own library folder

A placed album lands beside the artist's existing albums, under the directory
the scanner found those albums in. Genre nesting is preserved and no second
artist tree is created. `library.paths[0]` is deliberately not used as the
destination root: for a nested library it is a genre or user directory, so
albums would be written outside the artist's own tree.

### The destination pair comes from the scan, and the on-disk folder name wins

The scanner already derives, per album, the directory *above* the artist folder
(`ScannedAlbum::path`). The index additionally records the on-disk name of the
artist folder itself, so a destination is a pair of (root, artist directory).

The artist directory that is actually on disk — not the tag-derived query
spelling — is used for the `%artist%` component. Tag spellings and folder
spellings differ in real libraries (`Guns 'n' Roses` in tags, `Guns N Roses` on
disk); expanding the pattern with the tag spelling would create a second artist
directory beside the existing one. MusicBrainz queries and processed-album
records keep using the tag-preferred query spelling, which is unchanged
behaviour.

Where one artist's albums were found under more than one root, the destination
with the most albums wins, ties broken alphabetically. This mirrors the rule the
index already uses to choose a query spelling, and keeps the destination
independent of filesystem walk order.

### The scanner resolves the path positionally, peeling one disc folder

The scanner currently takes the artist from the first path component and the
album from the second, and derives the album's library location by dropping the
last two components. Those two rules disagree as soon as a library is nested or
a multi-disc album keeps its discs in subdirectories:

- `<root>/Artist/Album/track.flac` resolves the location to `<root>` and the
  artist folder to `Artist` (correct);
- `<root>/Genre/Artist/Album/track.flac` resolves the location correctly to
  `<root>/Genre` but names the artist `Genre` and the album `Artist` when tags
  are unreadable;
- `<root>/Artist/Album/CD 01/track.flac` resolves the location to
  `<root>/Artist`, so placement would create an album folder inside an existing
  album folder and auto mode's upgrade copy-back lands one level too deep.

All three become one positional rule: the album component is the one holding the
files, peeling a dedicated disc folder when that still leaves an artist
component above it; the artist component is the one directly above the album
component; the library location is everything above that.

The disc designator recognition (dedicated disc folders and embedded markers)
is the one the download stager, the organizer, and the peer-side path parser
already share, so library-side and peer-side grouping cannot disagree about
which component is the album. Only one disc level is peeled, matching the
peer-side rule.

Two shapes are called out because the positional rule handles them in ways
that are easy to misread:

- a folder named like a disc *directly* above the files is read as a disc
  whenever a component above it can serve as the album, matching the peer-side
  rule: `<root>/Genre/Artist/CD 01/track.flac` resolves to artist `Genre`,
  album `Artist`, album location `<root>`. Only at the minimum depth,
  `<root>/Artist/CD 01/track.flac`, is that folder itself the album: peeling
  there would leave `Artist` as the album with nothing above it to be the
  artist. The peer-side parser declines that shape outright, and keeping it in
  the index is deliberate, because dropping it would remove a real album and
  invite a re-download;
- extra directory levels above the artist folder (`Genre/Style/Artist/Album/`)
  need no special handling, because the rule is positional.

Consequences, all deliberate:

- a disc-nested album's library location becomes correct, which also fixes the
  auto-mode upgrade destination for those albums;
- for untagged files the folder fallback now names the real artist and album
  folders at any nesting depth, instead of the first two components, and the
  album folder name is kept verbatim — including an embedded marker such as
  `Gold (Disc 1)`, which is the identity the upgrade copy and the
  quality-deletion root use.

### `LibraryTarget` replaces the loose target parameter

`process_album` currently accepts `library_track_count: Option<usize>` and
`target_library_path: Option<&Path>` as independent optional arguments, which
admits the state "target set, no track count" that the implementation has to
catch at runtime by returning `Failed("library upgrade target set without a
library track count")`. The target becomes one enum:

```rust
pub enum LibraryTarget<'a> {
    Upgrade { root: &'a Path, expected_tracks: usize },
    Place { root: &'a Path, artist_dir: &'a str },
}
```

`Upgrade` is auto mode's gated copy-back: the completeness gate, the copy, and
the optional quality-deletion pass. `Place` is discover mode's placement: the
same copy, no completeness gate, no quality deletion. The invalid state
disappears from the type, and the defensive failure arm is deleted rather than
kept.

`library_track_count` stays a separate parameter because it also feeds the
peer-track-count search filter, which applies with or without a target.

### Gating moves to the auto-mode caller

The `if config.library_upgrade.enabled` wrapper is removed from
`process_album_internal`; auto mode constructs its own target only when the flag
is on:

```rust
let target = config.library_upgrade.enabled.then_some(LibraryTarget::Upgrade {
    root: library_path.as_path(),
    expected_tracks: library_track_count,
});
```

`then_some` rather than `then(|| ...)`: the construction has no side effects, and
`clippy -D warnings` rejects the lazy-closure form as
`unnecessary_lazy_evaluations`.

With the flag off, auto mode passes no target and takes the generic organize
path exactly as it does today, so auto-mode behaviour is unchanged in both flag
states.

### Selection carries the destination

`select_artists` returns placements, not bare names:

```rust
pub struct SelectedArtist {
    /// Spelling to query MusicBrainz.
    pub name: String,
    /// Parent directory of the artist's existing library folder.
    pub library_root: PathBuf,
    /// On-disk name of the artist's existing library folder.
    pub artist_dir: String,
}
```

Because every indexed album contributes a destination, a selected artist always
has one, and "artist selected but no destination known" is not representable —
the same reasoning that removes the target/count failure arm above.

### Placement writes through its own entry point

Placement calls `organizer::place_into_library`, which shares one implementation
with the upgrade path's `organizer::copy_to_library` and differs in exactly two
ways:

- the `%artist%` component is the on-disk artist folder name used *verbatim*,
  not passed through `sanitize_component`. Rewriting it (a folder named
  `100% Hits..` would become `100％ Hits．．`) places the album beside the real
  artist folder instead of inside it — the failure this design exists to
  prevent. The value is a single path component produced by the library walk, so
  it cannot introduce a separator, and `%artist%` is substituted last so a name
  containing a placeholder cannot cascade into another field;
- an existing destination file that parses as audio is never replaced, because
  the destination folder may hold a different edition of the album (an existing
  folder named after the MusicBrainz title whose files carry different tags).
  Only a file that does not parse — a truncated copy from an interrupted run —
  is replaced, so a retry can finish an album whose copy was interrupted before
  any complete file landed. The gap: placement is not transactional, so a crash
  after at least one file lands leaves an album the presence index already
  treats as present (see Deliberate limits). The upgrade path keeps its existing
  rule: replace unless the destination scores strictly higher.

Everything else is inherited: `sanitize_component` on the album, track, title,
and extension values, parent-directory creation, multi-disc subdirectory
preservation, cross-filesystem safety, and the commit order.

There is no completeness gate. A new album has no library track-count baseline,
and presence semantics already treat any audio file under the album folder as
present, so a gate here would only reject albums for a reason the rest of the
design does not share. There is no `delete_lesser_quality` pass either: it is an
upgrade action, comparing a download against the file it replaces, and nothing
is being replaced here.

### The album folder takes the MusicBrainz title

The placed album directory is named from the MusicBrainz album title, not from a
peer's folder name, matching the string already used for the search, the
processed-album record, and the notification. Folder identity stays consistent
with the record that will later mark the album as present.

### Staging is removed after a successful copy

A successful placement removes the album's staging directory, exactly as the
library-upgrade copy path does. This keeps one authoritative copy, avoids
double disk usage for large FLAC albums, and prevents the auto-mode recovery
scan from treating the leftover as an interrupted upgrade and re-copying it to
`library.paths[0]`. A failure to remove the directory is logged as a warning and
does not change the outcome, matching current behaviour.

### A placement failure is a charged `Failed` outcome

A copy or directory-creation failure returns
`Failed { reason: "library placement failed: <cause>" }` with the staging
directory retained, so no downloaded data is lost. The album is recorded
`failed`, the run continues, and the download budget is charged — consistent
with the existing rule that only `Downloaded` and `Failed` outcomes reach the
download stage and therefore charge. The album is retried on a later run
because only successes become processed records.

### Notifications, reporting, budget, and recovery are unchanged

Placement reuses the shared success tail: mark processed as `success`, remove
staging, notify once. No new notice, counter, or report section is introduced; a
placement failure appears in the existing `Failed` section with its reason. The
budget rule is untouched. Cancellation is untouched: Ctrl+C stops before the
next album and a cancelled download cleans its own staging. Discover does not
participate in upgrade recovery, and after this change it leaves no staging
directory behind on success.

## Configuration

No new keys and no schema change. Discover placement is governed by the
existing `library.paths` (the index and the destination pair come from the scan
of those paths) and `storage.organize_pattern`, which defines the destination
shape: `%artist%` expands to the on-disk artist directory and `%album%` to the
MusicBrainz title, relative to the scanned root.

One semantic is called out for documentation: discover honours
`storage.organize_pattern` as a naming preference even when
`storage.organize` is `false`. For every other mode the pattern only applies
when that flag is on; for discover it also shapes placement. The pattern is
honoured verbatim, so a pattern without `%artist%` places the album directly
under the scanned root — the same as any other organize write.

The pattern is validated: it must be non-empty, relative, and free of `..`
components, because `Path::join` discards the library root for an absolute
pattern and either form can write outside the library.

## Architecture

### `src/discs.rs`

One new helper, sharing the module's existing disc recognition:

```rust
/// Index of the album component in a slash-split path, peeling one dedicated
/// disc folder. `None` when no artist component would remain above the album.
pub fn album_index(components: &[&str]) -> Option<usize>;
```

It implements the rule the peer-side parser already applies: start from the
component holding the file, step up one when that component is a dedicated disc
folder, and decline when no component remains above the album. Embedded-marker
stripping stays with the existing `strip_embedded_disc_marker`.

`src/search.rs`'s `artist_album_name` is refactored onto the helper with no
behaviour change — its current `album_index == 0` refusal becomes the helper's
`None` — and its existing tests must pass untouched.

### `src/scanner.rs`

- `ScannedAlbum` gains `artist_dir: String`, the on-disk name of the artist
  folder the album was found in.
- The path-derived branch becomes positional: the album component is
  `discs::album_index`, the artist component is the one directly above it, the
  library location is everything above that, and the album name is the album
  component kept verbatim. The artist and album names remain tag-preferred, with
  these values as the fallback.
- When the helper declines because the album component itself looks like a disc
  folder with the artist directly above it, the unpeeled reading is kept. A file
  that cannot supply an artist component, an album component, and a file is
  still skipped, exactly as the current three-component guard does.

### `src/discover.rs`

- `IndexedArtist` additionally records the destination pairs seen for that
  artist and their album counts; `build_index` fills them from
  `ScannedAlbum::path` and `ScannedAlbum::artist_dir`.
- New accessor:

  ```rust
  /// (root, on-disk artist directory) for this artist, majority first,
  /// ties broken alphabetically.
  pub fn artist_destination(&self, artist_key: &str) -> Option<(&str, &str)>;
  ```

- `ArtistSelection::artists` becomes `Vec<SelectedArtist>`, carrying the query
  spelling and the destination. Existence, presence, and budget logic are
  otherwise unchanged.

### `src/runner.rs`

- New public `LibraryTarget` enum, as above.
- `process_album` and `process_album_internal` take
  `target: Option<LibraryTarget<'_>>` in place of `target_library_path`.
- The copy-back block becomes a match on the target with one shared success
  tail; the `Failed("library upgrade target set without a library track count")`
  arm and the `library_upgrade.enabled` wrapper are removed from it.
- `run_auto_mode` constructs `LibraryTarget::Upgrade` only when
  `library_upgrade.enabled` is true.
- The discover loop builds `LibraryTarget::Place { root, artist_dir }` from the
  selected artist's `library_root` and `artist_dir` for every missing album.

### `src/config.rs`, `src/main.rs`, `src/mode.rs`

`src/config.rs` gains the `storage.organize_pattern` containment check
described under Configuration. `src/mode.rs` is unchanged. `src/main.rs`
changes only in a comment that named the removed parameter alongside a `None`
argument.

## Data flow

1. `scan_library(library.paths, filters)` walks the library once and resolves
   each album's artist folder, album folder, and library location positionally,
   peeling one disc folder.
2. `build_index` records, per artist, the query spellings, the normalised album
   titles, and the destination pairs with their album counts.
3. `select_artists` applies exclusions and the optional filter, orders the
   result, and attaches each selected artist's destination.
4. For each artist, while the budget is not exhausted:
   1. `discover_artist_albums` resolves the artist's conceptual albums.
   2. `missing_albums` removes albums already present.
   3. For each remaining album, oldest first, `process_album` runs with
      `library_track_count: None` and the artist's `LibraryTarget::Place`.
5. Inside `process_album_internal`, after a successful download:
   `place_into_library(root, organize_pattern, on-disk artist directory, album
   title)` → remove the staging album directory → record `success` → notify →
   `Downloaded { track_count }`, which charges the budget as it already does.

## Error handling

- **Placement copy or directory creation fails** — `Failed` with
  `library placement failed: <cause>`; staging retained; album recorded
  `failed`; budget charged. The album is retried on a later run only when no
  audio file reached the album folder: a failure after the first file landed
  leaves a folder the presence index treats as present, so that album is
  reported as already present from then on. The retained copy is also not
  guaranteed to survive: a later auto run whose recovery scan is enabled
  (`library_upgrade.enabled`) removes every leftover staging directory whose
  album is not recorded `success`, so the album is re-downloaded from scratch.
- **Staging removal fails after a successful copy** — warning only; the album
  stays a success. The recovery scan then treats that leftover as an
  interrupted upgrade and re-copies it into `library.paths[0]` on the next auto
  run, which is the outcome removal was meant to prevent; because the album is
  already recorded successful it is not re-downloaded.
- **Destination file already present and parseable** — kept; the incoming file
  is not copied, and the album still counts as placed.
- **Artist destination unavailable** — not representable for a selected artist;
  every indexed album contributes a destination pair, and selection is drawn
  from the index.
- **`library.paths` empty or `--artist` naming an unknown artist** — unchanged
  configuration errors raised before this code runs.
- **Album already present, unresolved artist, provider failure, budget
  exhaustion, cancellation** — unchanged behaviour.

## Backward compatibility

- Auto mode's control flow is unchanged in both flag states: same targets, same
  completeness gate, same quality deletion, same organize fallback.
- One auto-mode *destination* changes, as a fix: an album whose files live in a
  disc subdirectory now upgrades into `<root>/<artist>/<album>/` instead of one
  level deeper. This is the same disc rule the peer-side parser already applies.
- Untagged libraries: flat layouts keep their current keys. Nested layouts now
  derive the real artist and album folder names instead of the first two path
  components. Both are corrections to path-derived names that only apply when
  tags are missing or unreadable; tagged libraries are unaffected. An album
  folder carrying an embedded disc marker keeps that marker in its name, because
  the name is the destination the upgrade copy and the quality-deletion root
  use; folding marker variants into one album identity is the peer-side parser's
  job and a separate concern.
- Artist-only manual, explicit manual, and batch modes are untouched; only the
  `process_album` signature they call changes.
- Existing configuration files remain valid, with one exception: a
  `storage.organize_pattern` that was empty, absolute, or carried `..` used to
  run and is now rejected at startup, because `Path::join` discards part or all
  of the library root for those forms. Every other key keeps its meaning and
  default.
- The database schema is unchanged. Discover still writes the same
  processed-album records.
- The reporting contract is unchanged: no new notices, and placement failures
  render in the existing `Failed` section.

## Deliberate limits

- **Artist-only manual runs do not place albums in the artist's folder.** Only
  discover mode places albums. An artist-only manual run organizes into
  `library.paths[0]` when `storage.organize` is on and leaves the download in
  staging when it is off — the pre-existing behaviour. Giving artist-only
  manual the same placement is a candidate follow-up.
- **A destination album folder that already exists under a different spelling
  is not reused.** The never-downgrade guard protects files, not folder
  identity, so a differently named edition folder beside it is possible.
- **Placement is not transactional.** A crash mid-copy leaves a partial album
  folder; the album is recorded `failed`, and a retry keeps whatever it finds
  (placement never replaces a file that parses as audio). The retry only happens
  when no audio file landed: presence treats any audio file under the album
  folder as present, so a crash after the first file leaves an album that later
  runs report as already present rather than completing it.
- **No quality deletion on the placement path.** `delete_lesser_quality`
  remains an upgrade-only action.
- **Presence semantics are untouched.** A partially placed album counts as
  present, so it is never completed by a later run.
- **A marker-shaped subfolder under an album folder reads as the artist folder.**
  For `<root>/Artist/Gold/Gold (Disc 1)/track.flac` the album component is the
  marker folder, so the artist folder resolves to `Gold` and the destination
  pair is (`<root>/Artist`, `Gold`). A placement for that artist would therefore
  write an album inside the existing `Gold` folder rather than beside it. The
  shape is structurally identical to
  `<root>/Genre/Artist/Gold (Disc 1)/track.flac`, where the marker folder *is*
  the album folder and the current reading (artist `Artist`) is the correct one,
  so no positional rule can separate them. The index prefers the majority
  destination, so the bogus pair only wins when marker-shaped albums are the
  artist's only source. Folding marker variants into one album identity is work
  for a future presence-and-identity change, not a path change.
- **Only one disc level is peeled, and only when a component above the file
  can serve as the album.** `Album/CD 01/CD 02/track.flac` keeps resolving one
  level short, and `<root>/Genre/Artist/CD 01/track.flac` reads `CD 01` as a
  disc of the album `Artist` — the same reading the peer-side parser makes. The
  two readings are structurally ambiguous; the rule follows the peer side.
- **The album component must hold the files.** A non-disc subdirectory between
  the album folder and the file (`<root>/Artist/Album/Extras/track.flac`)
  resolves the album component one level deep, so the derived names become
  artist `Album`, album `Extras`; the previous rule derived artist `Artist`,
  album `Album`. The new reading keeps the library location correct, but in auto
  mode the Soulseek query uses the wrong artist, and in discover the presence
  index gains a key named after the album folder while the real album is
  counted missing. Both readings are wrong for that out-of-convention layout,
  and neither is silently corrected.

## Testing

### `discs.rs`

- The helper returns the album component index for flat, nested, and
  deep-nested paths.
- It peels one dedicated disc folder (`CD 01`, `Disc 2`, `{cd1}`) and declines
  when nothing remains above the album.
- It declines a path whose album component is a disc folder with no artist
  component above it; the scanner's fallback reading for that shape is covered
  by the scanner tests below.
- `search::artist_album_name` tests pass unchanged after the refactor,
  including the existing multi-disc grouping test.

### `scanner.rs`

- A flat `<root>/Artist/Album/track.flac` library resolves the location to the
  library root, the artist directory to `Artist`, and the names to
  `Artist`/`Album` (regression, unchanged).
- A nested `<root>/Genre/Artist/Album/track.flac` library resolves the location
  to `<root>/Genre`, the artist directory to `Artist`, and the untagged
  fallback names to `Artist`/`Album`.
- A deep-nested `<root>/Genre/Style/Artist/Album/track.flac` library resolves
  the location to `<root>/Genre/Style` and the names to `Artist`/`Album`.
- A disc-nested `<root>/Artist/Album/CD 01/track.flac` library resolves the
  location to the library root, the artist directory to `Artist`, and groups
  both discs into one album.
- A nested disc-nested variant resolves the location to `<root>/Genre`.
- An album folder carrying an embedded marker (`Gold (Disc 1)`) keeps the
  on-disk folder name, so the upgrade copy and the quality-deletion root stay in
  the folder the album was found in.
- `<root>/Artist/CD 01/track.flac` keeps its current reading: the album is
  `CD 01` under artist `Artist`, and it remains indexed.
- Files that cannot supply an artist folder, an album folder, and a file are
  still skipped.

### `discover.rs`

- `build_index` records the destination pair per album, in flat and nested
  layouts.
- `artist_destination` picks the majority pair and breaks ties alphabetically,
  independent of insert order.
- `select_artists` returns `SelectedArtist` entries carrying the query spelling
  and the destination, with exclusions and the optional filter unchanged.
- Presence, ordering, and budget tests are adapted for the selection type and
  otherwise unchanged.

### `runner.rs`

- A completed discover album is copied to
  `<root>/<on-disk artist dir>/<MusicBrainz title>/`, the staging directory is
  gone, and `success` is recorded.
- Placement lands beside the artist's existing albums in a nested library.
- Placement happens with `storage.organize: false` and
  `library_upgrade.enabled: false`.
- The destination uses the on-disk artist directory, including when it contains
  characters that `sanitize_component` would rewrite or a placeholder such as
  `%album%`: covered by the `organizer` placement tests and by
  `destination_keeps_the_on_disk_artist_folder_spelling` in `discover`.
- A single-track peer group places successfully, so placement applies no
  completeness gate against a library track count.
- A destination file that parses as audio is kept rather than overwritten, and
  one that does not parse is replaced: covered by the `organizer` placement
  tests.
- Multi-disc staging keeps its `CD 01`/`CD 02` structure under the placed
  album: inherited from `copy_into_library` and covered by its `organizer`
  tests.
- A blocked destination yields `Failed`, retains staging, charges the budget,
  and records `failed`.
- Auto mode regression: `Upgrade` still enforces both halves of the
  completeness gate — `test_library_upgrade_completeness_uses_library_track_count_not_peer_folder_size`
  for the accepted case and `test_library_upgrade_rejects_an_incomplete_download`
  for the rejected one — and still takes the generic organize path when
  `library_upgrade.enabled` is false.
- Auto mode regression: a disc-nested album's upgrade copy lands at
  `<root>/<artist>/<album>/`
  (`test_auto_mode_upgrade_of_a_disc_nested_album_lands_in_the_album_folder`).
- Auto mode regression: a marker-named album folder (`Gold (Disc 1)`) keeps its
  on-disk folder name, so the upgrade copy and the quality-deletion root stay in
  the folder the album was found in
  (`test_scan_keeps_an_embedded_marker_in_the_album_folder_name` in `scanner`,
  which is the derivation that name comes from).

The `Upgrade` arm's call into `delete_lesser_quality_files` is unchanged by this
design and its mechanics are covered by the three `organizer` tests for that
function (`test_delete_lesser_quality_removes_worse_files`,
`test_delete_lesser_quality_preserves_better_files`,
`test_delete_lesser_quality_disabled_noop`). A runner-level assertion is not
possible with the current mock: it writes non-audio bytes, so the new files
score 0 and nothing is ever deletable.

### Mode and CLI tests

Unchanged: discover resolution, `--artist` narrowing, and the `--album` and
`--batch-file` rejections are untouched by this design.

## Acceptance criteria

1. A completed discover download is placed in the artist's existing library
   folder using `storage.organize_pattern`, with the album directory named from
   the MusicBrainz title.
2. Placement works for flat and nested libraries, always resolving the artist's
   real location, and uses the on-disk artist directory name.
3. Placement is unconditional in discover mode: it does not depend on
   `storage.organize` or `library_upgrade.enabled`.
4. The placement path applies no completeness gate and no quality deletion.
5. The staging album directory is removed after a successful copy; a removal
   failure is a warning only.
6. A placement failure is a `Failed` outcome with staging retained and the
   budget charged.
7. Disc-nested albums resolve their artist directory and library location
   correctly, and library-side and peer-side path parsing share one disc rule.
8. Auto mode's behaviour is unchanged except for the corrected disc-nested
   upgrade destination, and its existing tests pass.
9. Artist-only manual, explicit manual, and batch modes are unchanged.
10. No new configuration keys, no schema change, and no new report notices.

## External contracts

- Soulseek: unchanged.
- MusicBrainz: unchanged.
- Filesystem: discover mode writes into the artist's existing library directory
  and removes its own staging directory; no other mode's write locations change
  apart from the disc-nested upgrade correction.
- Database: unchanged schema; discover writes the same processed-album records.

## References

- `docs/agent/specs/2026-09-15-musicbrainz-library-gap-filling-design.md`
- `docs/specs/2026-08-16-library-upgrade-design.md`
- `docs/specs/2026-08-20-musicbrainz-integration-design.md`
- `src/discover.rs`, `src/scanner.rs`, `src/organizer.rs`, `src/discs.rs`,
  `src/search.rs`, `src/runner.rs`
