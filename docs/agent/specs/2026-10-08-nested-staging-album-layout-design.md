<!-- markdownlint-disable MD013 -->

# Nested staging album layout

## Problem

A staged album is one directory named `<artist>--<album>` directly under
`storage.staging_dir`, so a download that stays in staging reads as
`downloads/staging/Artist Name--Album Name/01 - Track.flac`. The operator wants the
two components to be real directories instead:
`downloads/staging/Artist Name/Album Name/01 - Track.flac`.

The flattened name is not only cosmetic. It is a lossy encoding of two values into one
path component, and the code that consumes it has to decode it again:

| Site | Behaviour today |
| --- | --- |
| `src/runner.rs:508-516` | Builds `format!("{}--{}", artist.replace(['/','\\'], "-"), album.unwrap_or("unknown").replace(['/','\\'], "-"))` and joins it to the staging root. |
| `src/organizer.rs:516-533` `disc_subdir` | Walks a staged file's parent directories and stops when a directory name `.contains("--")`, because that names the album staging root. An album whose own name contains `--` cannot be told from a boundary by this test. |
| `src/organizer.rs:758-763` `parse_album_slug` | Splits on the first `--` to recover `(artist, album)`; a name without a separator becomes album `"Unknown"`. |
| `src/organizer.rs:774-810` `recover_interrupted_upgrades` | Reads the top level of the staging root, treats every directory as an album slug, re-verifies a `success` album by copying it back, and deletes every other leftover. |
| `src/runner.rs:355-389` `discard_refused_staging` | Removes a refused album's staged files, then prunes directories that are now empty. Its warnings explain the `A--B` + `C` versus `A` + `B--C` slug collapse. |
| `src/runner.rs:392-413` `finish_library_write` | Removes the album staging directory after the album has been written to the library. |

## Goal

A completed download stages under `<staging_dir>/<artist>/<album>/`, for every mode, with
both components produced by the same sanitiser the library write uses, and with no
behaviour change anywhere else: same recovery semantics, same cleanup, same database keys.

## Scope

### In scope

- The staging path layout and every site that builds or decodes it.
- A startup migration for staging directories left by earlier versions.
- The recovery scan's album-identity lookup, which the new layout and the sanitiser change.
- Cleanup of the artist level when an album leaves staging.
- An audit that records, and tests that enforce, one sanitiser for every folder seakarr
  creates.

### Out of scope

- The library layout, which is already `<library root>/<artist>/<album>/` and does not change.
- Placement semantics: placement still never creates an artist folder, and still writes only
  into an artist folder the library already holds.
- Any new or changed configuration key. `storage.staging_dir` keeps its meaning and default
  (`downloads/staging`).
- A validation that `storage.staging_dir` sits outside every `library.paths` entry. The
  README documents the requirement; this change does not add a gate.
- Disc subdirectory nesting inside an album (`download.rs`), which is unchanged because it
  receives the finished album path.
- Historical design documents that record the old layout
  (`docs/agent/specs/2026-09-15-discover-library-placement-design.md:14`,
  `docs/agent/specs/2026-09-23-remove-organize-design.md:209`). They are dated records and
  are left as written.

## Decisions

### Staging albums are two path components

`<staging_dir>/<artist>/<album>/`. The artist level exists for every album, including an
album-only manual run, which names no artist. That keeps one rule, and therefore one
recovery walk, instead of a layout whose depth depends on which request produced it.

### Both components use `sanitize_component`

The artist component and the album component each pass through
`organizer::sanitize_component`, the function every library write already uses: Windows
reserved characters and control characters are removed, a trailing dot or space is trimmed,
a reserved device name is prefixed (`CON` becomes `_CON`), and a value that sanitises to
nothing becomes `_`.

Consequences, all deliberate:

- The staging tree mirrors the library tree for the same album, so a folder name that
  renders correctly in the library renders correctly in staging too.
- An album-only run (blank artist) stages under the `_` placeholder:
  `<staging_dir>/_/<album>/`. `_` is the sanitiser's existing placeholder for "this value
  keeps nothing", and `ArtistFolderIndex` already refuses to treat `_` as an artist folder
  (`src/discover.rs:319-320`), so the placeholder cannot be mistaken for a library match.
- An artist-only run that names no album keeps today's `"unknown"` album component:
  `<staging_dir>/<artist>/unknown/`.

### A new `src/staging.rs` owns the layout

The layout rule, the migration, and the staging scan move into one small module. This
follows the crate's existing idiom (`discs.rs`, `tracks.rs`, `report.rs`) and keeps the two
riskiest behaviours of this change — a merge that must never overwrite, and a name lookup
that must resolve a sanitised directory back to a raw album — unit-testable without a
runner, a client, or a download.

`organizer.rs` keeps the library half of recovery (`resume_library_upgrade`) and the
sanitiser. `runner.rs` keeps orchestration and calls into the module.

### Legacy `<artist>--<album>` folders are migrated at startup

`normalise_layout` runs once per run, before the recovery scan, and is **not** gated on
`library_upgrade.enabled`: the layout rule belongs to staging, not to one mode. It renames
each top-level `<artist>--<album>` directory into the nested form, so a leftover from an
earlier version is still found by the recovery scan and by the operator.

`parse_album_slug` is kept, renamed in documentation as the legacy parser, and used only by
the migration. It is removed from the recovery path.

### Migration merges without overwriting

Migrating a legacy directory whose nested destination already exists must not destroy
either copy:

- Rename each entry that is absent at the destination, recursing into subdirectories so a
  legacy `CD 01/` folder keeps its shape.
- When a name already exists at the destination, keep the destination file and leave the
  legacy copy where it is. Nothing is overwritten.
- A legacy directory that the merge emptied is removed. A legacy directory that still holds
  a skipped entry is left in place and reported, naming the first entry that stopped it —
  the same shape of warning `discard_refused_staging` already emits.

### Recovery resolves identity by a sanitised match

`recover_interrupted_upgrades` identifies an album by looking a directory name up in the
`processed_albums` table, which stores raw names (`db.get_album_status`). With sanitised
directory names, an exact lookup would miss (`Gold: Greatest Hits` stages as
`Gold_ Greatest Hits`), and a missed lookup means the leftover is deleted as untracked
debris — data loss for an interrupted upgrade that was about to be recovered.

`staging::scan` therefore loads `db.get_processed_albums()` once and matches each
`<artist>/<album>` directory against rows by comparing `sanitize_component(row.artist)` and
`sanitize_component(row.album)` with the two directory names, case-insensitively. A match
yields the row's **raw** names and status, which are what `resume_library_upgrade` needs:
it sanitises them again for the library destination (sanitising is idempotent, so the
destination is unchanged) and the status write keeps the raw key. No row yields no
identity, and the leftover is cleaned exactly as today.

When several rows match, the highest `id` wins and the ambiguity is warned about, so the
most recently tried album is the one recovered.

### `disc_subdir` stops at the album root by name, not by a separator

`disc_subdir` currently stops walking when a parent directory name contains `--`. It is
called from exactly one place, `organizer.rs:426` inside `copy_into_library`, where the
album string is already in scope, so it instead stops when the directory name equals
`sanitize_component(album)` — the album staging directory's own name. This is the same
boundary without the separator sniff, and the innermost-match-first walk still returns a
real disc folder before it reaches the album root, so a multi-disc album whose own name
carries an embedded disc marker (`Gold (Disc 1)`) behaves as it does today.

### An empty artist directory is pruned

The artist level is a directory seakarr creates, so it is removed when it holds nothing:

- Immediately, by `finish_library_write` and `discard_refused_staging`, once the album
  directory they handle has gone.
- Elsewhere, by `normalise_layout` at the start of the next run, which removes an artist
  directory that holds no entries at all. This covers a failure path that removed an album
  directory and left its parent behind.

`fs::remove_dir` refuses a non-empty directory, so an artist's second album, staged
concurrently or left from an interrupted run, can never be removed by this.

### Sanitisation is consistent across every library write

One rule, one function: every production path that turns an artist or album string into a
path component calls `sanitize_component` at the moment the name is created. There is no
second sanitiser and no per-mode variant. Staging joins that set.

The audit below records the state this change must preserve, so a later reader can tell a
deliberate exception from an oversight:

| Library-write path | Artist component | Album and file components |
| --- | --- | --- |
| `copy_to_library` (`organizer.rs:328` → `library_relative_path:129-134`) | `sanitize_component(artist)` | `sanitize_component(album/track/title/ext)` |
| `place_into_library` (`album_dir_for:145-158`) | the existing on-disk folder name, used **verbatim** | `sanitize_component(album)` |
| `delete_lesser_quality_files:545-554` | sanitised | sanitised |
| `resume_library_upgrade:696-698`, per-entry leaf `:723`/`:728` | sanitised | sanitised |
| `disc_subdir:531` (a disc label becomes a real folder) | n/a | sanitised |
| `search.rs:1211-1256`, `discover.rs:319-320` (lookups, both sides) | sanitised | sanitised |
| staged file names (`download.rs`) | n/a | peer basename, `safe_basename`-checked; never a library folder |

The single exception is `place_into_library`, and it is deliberate: its `artist_dir` is the
name of an artist folder that already exists on disk, found by the library walk or
`ArtistFolderIndex`, and placement exists to write *into* that folder rather than create a
differently-named one beside it. Re-sanitising it would not add safety — `album_dir_for`
already rejects any value that is not a single ordinary path component — it would split one
artist across two folders. Every folder seakarr *creates* comes from a sanitised name.

## Architecture

### `src/staging.rs` (new)

```rust
/// `<staging_dir>/<sanitized artist>`.
pub fn artist_dir(staging_dir: &Path, artist: &str) -> PathBuf;

/// `<staging_dir>/<sanitized artist>/<sanitized album>`.
/// `None` keeps today's `"unknown"` album component.
pub fn album_dir(staging_dir: &Path, artist: &str, album: Option<&str>) -> PathBuf;

/// One album found under the staging root.
pub struct StagedAlbum {
    /// `<staging_dir>/<artist>` — the level to prune when the album leaves.
    pub artist_dir_path: PathBuf,
    /// `<staging_dir>/<artist>/<album>`.
    pub album_dir_path: PathBuf,
    /// The album directory name as it is on disk (the sanitised component).
    pub dir_album: String,
    /// `None` when no `processed_albums` row matches: an untracked leftover.
    pub identity: Option<StagedIdentity>,
}

/// Raw names and status, as stored in the database.
pub struct StagedIdentity {
    pub artist: String,
    pub album: String,
    pub status: String,
}

/// Walk the two levels under the staging root and resolve each album's identity.
pub fn scan(staging_dir: &Path, db: &Database) -> Result<Vec<StagedAlbum>>;

/// Rename legacy `<artist>--<album>` directories into the nested form, prune artist
/// directories that hold nothing, and report what happened. A missing staging root
/// is not an error: there is nothing stale in a directory that does not exist.
pub fn normalise_layout(staging_dir: &Path) -> Result<MigrationReport>;

/// The one call a run makes before it stages anything: create the staging root if
/// it is missing, then `normalise_layout`. Replaces the bare
/// `std::fs::create_dir_all(staging_dir)` each mode performs today.
pub fn prepare(staging_dir: &Path) -> Result<MigrationReport>;

pub struct MigrationReport {
    /// Legacy directories renamed wholesale: the nested destination was absent.
    pub migrated: Vec<PathBuf>,
    /// Legacy directories merged into a destination that already existed.
    pub merged: Vec<PathBuf>,
    /// A legacy directory left in place, with the first entry that stopped it.
    pub left_in_place: Vec<(PathBuf, PathBuf)>,
    /// A top-level directory that is neither a legacy slug nor an artist directory.
    pub unrecognised: Vec<PathBuf>,
    /// Names that are not valid UTF-8; renaming them lossily could orphan an album.
    pub non_utf8: Vec<PathBuf>,
    pub failed: Vec<(PathBuf, String)>,
    pub pruned_artist_dirs: Vec<PathBuf>,
}
```

### `src/runner.rs`

- `process_album_internal` builds `album_staging = staging::album_dir(staging_dir, artist, album)`
  in place of the `format!("{}--{}", ...)` slug. Every other use of the album staging path in
  the file (`finish_library_write`, `discard_refused_staging`, the `DownloadDestination::Staging`
  value, log lines) derives from that one value.
- `finish_library_write` prunes the artist level after removing the album directory.
- `discard_refused_staging` prunes the artist level once `remove_empty_staging_dirs` reports
  the album directory gone, and its warnings describe the nested name instead of the
  `artist--album` collapse. The collapse cannot occur in the new layout, because the two
  values are separate path components.
- Each mode's existing `let staging_dir = Path::new(&config.storage.staging_dir);` /
  `std::fs::create_dir_all(staging_dir)?` pair becomes `staging::prepare(staging_dir)`:
  `run_upgrade_mode` (`runner.rs:1465`), `run_manual_mode` (`runner.rs:2975`), and
  `run_batch_mode` (`main.rs:737`). `run_discover_mode` (`runner.rs:2465`) has no staging
  setup today — it passes the configured path straight to `process_album` — so it gains one
  `staging::prepare` call, which also creates a staging root that `download_album` currently
  creates lazily.
- `run_upgrade_mode` then calls `organizer::recover_interrupted_upgrades`, unchanged in
  gate, position, and effect. Recovery stays where it is: manual, discover, and batch runs
  do not scan staging for interrupted upgrades today, and this change does not add that.
  Both staging calls keep the existing treatment: a failure is logged as a warning and never
  aborts the run.

### `src/organizer.rs`

- `sanitize_component` stays here; `staging.rs` calls it. It is not duplicated or moved.
- `parse_album_slug` stays as the legacy parser, used only by `staging::normalise_layout`,
  and leaves the recovery path.
- `disc_subdir` takes the album component and stops on a name match instead of `contains("--")`.
- `recover_interrupted_upgrades` keeps its signature, its gate (`library_upgrade.enabled`
  and a configured library path), and its two arms, but iterates `staging::scan(...)` instead
  of reading directory names: `Some(identity)` with status `success` resumes the copy with
  the identity's raw names, and everything else — a failed status, a non-success status, no
  identity at all — removes the album directory as debris.

### `src/download.rs`

Unchanged. It receives the album path, creates it on demand, and nests disc folders inside
it.

## Data flow

1. A run starts and calls `staging::prepare(staging_dir)`, from each mode's entry point:
   `run_upgrade_mode`, `run_discover_mode`, `run_manual_mode`, and `run_batch_mode` in
   `main.rs`. It creates the staging root if missing, then renames each legacy
   `<artist>--<album>` directory into `<artist>/<album>` (merging when the destination
   exists), removes empty artist directories, and logs and records every anomaly.
2. `organizer::recover_interrupted_upgrades` runs in upgrade mode under its existing gate:
   it scans the two levels, resolves each album's raw identity by the sanitised match, copies
   a `success` album into the library, and deletes anything else. The other modes stage no
   recovery scan, as today.
3. `process_album_internal` computes `<staging>/<sanitize(artist)>/<sanitize(album)>`.
   `download_album` creates the album directory only when a peer will really deliver files,
   and keeps its existing per-disc nesting inside it.
4. A library write copies the album into the library, then `finish_library_write` removes the
   album staging directory and prunes the artist directory if it is now empty. A download
   that stays in staging keeps both levels, and its reported destination names the nested path.
5. A refused album's staged files are removed, the album directory is pruned when empty, and
   the artist directory is pruned the same way.

## Error handling

| Situation | Outcome |
| --- | --- |
| Migration cannot rename or merge an entry | The entry is recorded in `failed` with its error, the run continues, and the startup call logs a warning. A wasted rename never aborts a download run. |
| A legacy directory still holds a skipped entry after the merge | Left in place, recorded in `left_in_place` with the first entry that stopped it, and warned about. No file is overwritten. |
| A top-level directory name is not valid UTF-8 | Left untouched and recorded in `non_utf8`. Renaming it through the lossy conversion would invent characters and could orphan a recoverable album. |
| A top-level directory with no `--` holds no subdirectory | Left in place and recorded in `unrecognised`. In a nested layout the meaning of files directly in a top-level directory is ambiguous, and deleting real content is worse than leaving junk. |
| A legacy directory was left in place | Re-examined on the next run: the migration is idempotent, so a directory the operator resolved migrates then, and one that is still blocked warns again rather than being deleted. |
| Several `processed_albums` rows match the sanitised names | The highest `id` wins; the ambiguity is warned about with both raw names. |
| No `processed_albums` row matches | `identity: None`, and the album directory is removed as untracked debris, exactly as an unmatched `artist--album` directory is today. |
| A blank artist (album-only run) | Stages under `_`; it writes no `processed_albums` row (`mark_album_processed_if_identifiable` returns early for a blank artist), so recovery cleans it as it cleans `--Album` today. |
| Pruning an artist directory that holds a sibling album | `fs::remove_dir` fails on a non-empty directory; the failure is ignored, and the sibling is untouched. |
| An unwritable staging root | Surfaces through the existing `?` on the staging setup, unchanged from today. |

## Backward compatibility

- **Configuration:** no key is added, removed, or reinterpreted. `storage.staging_dir`
  keeps its default and meaning.
- **Database:** `processed_albums` keeps raw `artist`/`album` values and case-insensitive
  lookups. Only the recovery lookup changes, from an exact key to a sanitised match, which
  makes it resolve names it previously missed rather than miss names it previously resolved.
- **Existing staging content:** migrated on the next run. The migration is one-way. An older
  binary run against a migrated staging directory would treat each artist directory as an
  album slug (`Artist` → album `Unknown`), find no matching row, and delete the album
  directories below it as untracked debris. Reverting to an older version therefore requires
  an empty staging directory; the release note must say so.
- **Log and report wording:** paths shown to the operator change shape, because the
  destination really changed. No wording that names a mode, an outcome, or a reason changes.

## Deliberate limits

- The legacy decode inherits the ambiguity the old format always had: a legacy directory
  name containing `--` splits at the first occurrence, so an artist whose own name contains
  `--` is attributed as today. The migration is the last place this ambiguity exists.
- Migration uses `parse_album_slug`'s first-occurrence split and nothing else: it does not
  attempt to reinterpret a legacy name written by a version whose slug rules differed.
- A staging directory that is not writable is not repaired; the run reports the failure and
  continues, as it does today.
- Pruning is deliberately non-recursive beyond the album directory: an artist directory is
  removed only when it is empty, never when it holds anything seakarr cannot attribute.

## Testing

### Staging module tests (`staging.rs`)

- `album_dir` sanitises both components; a hostile title (`Gold: Greatest Hits`, `???`,
  `CON`, `A/B`) and one ending in a trailing space yield the same components the library
  write would use.
- `album_dir` with a blank artist yields the `_` level; with `album: None` yields the
  `unknown` level.
- `prepare` creates a missing staging root and reports an empty migration; a second call
  over an already-nested tree reports nothing and changes nothing.
- `normalise_layout` renames `Artist--Album` to `Artist/Album`, recursion included, so a
  legacy `CD 01/track.flac` keeps its shape.
- `normalise_layout` merges into an existing nested album: the destination file wins, the
  other file moves, and a directory still holding a skipped entry is reported in
  `left_in_place` and not deleted.
- `normalise_layout` prunes an empty artist directory and leaves one that holds anything.
- `normalise_layout` leaves a byte-invalid name alone (`#[cfg(unix)]`).
- `scan` resolves a sanitised directory name back to the raw names in the database,
  case-insensitively; returns no identity for an unknown album directory; and picks the
  highest `id` when two rows match.
- A consistency test: for one hostile album title, the two staging components equal the two
  library destination components.

### Recovery tests (`organizer.rs`)

- The recovery tests move to nested fixtures (`<root>/Artist/Album`) in place of
  `Unknown--Album`, keeping their three cases: a `success` album is copied back, a `failed`
  album is cleaned, an untracked album is cleaned.
- `disc_subdir` still excludes the embedded-marker album root and still returns an inner
  disc folder, for both a dedicated (`CD 01`) and an embedded (`Gold (Disc 1)`) marker.
- `parse_album_slug` keeps its cases as the legacy parser.

### Runner cleanup tests (`runner.rs`)

- A library write leaves neither the album directory nor an empty artist directory behind.
- A refused download leaves no empty artist directory behind, and a sibling album staged
  under the same artist is never removed.
- An album-only run stages under `_/<album>` and still reports a staging destination.
- A discovery run migrates a legacy staging directory before it stages an album, proving
  the mode that had no staging setup now has one.
- A failure path that removes an album directory leaves the artist directory for the next
  run's `normalise_layout` to prune.

### End-to-end placement test (`tests/pipeline_test.rs`)

- The end-to-end staging fixture expects `<staging>/Test Artist/Test Album` and asserts the
  nested tree is gone after a successful placement.

### Download tests (`download.rs`)

- Unchanged tests must pass. One added assertion: a multi-disc album produces
  `Artist/Album/CD 01/...` inside the album directory, proving the disc nesting is still
  relative to the album root.

## Acceptance criteria

1. A completed download for artist `A` and album `B` stages at `<staging_dir>/<san(A)>/<san(B)>/`,
   in every mode: `upgrade`, `discover`, manual, and batch.
2. Every folder name seakarr creates, in staging and in the library, comes from
   `sanitize_component`; the only name used unsanitised is the existing on-disk artist folder
   in `place_into_library`, with its guard unchanged.
3. A legacy `<artist>--<album>` directory is renamed into the nested layout on the next run,
   and a nested destination that already exists is merged without overwriting a single file.
4. An interrupted library upgrade is still recovered after the change, including for an album
   title whose sanitised directory name differs from the stored raw name.
5. The `processed_albums` keys, the SQLite schema, the configuration schema, and the search,
   ranking, and placement behaviours are unchanged.
6. No empty artist directory is left in staging after a completed, refused, or failed album,
   and no sibling album directory is ever removed by cleanup.
7. Every mode that can stage a download performs the migration: `upgrade`, `discover`,
   manual, and batch.
8. `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and `cargo test` pass.

## Documentation

- This spec, and its plan in `docs/agent/plans/`, follow the repository convention for
  non-trivial work.
- The README describes `storage.staging_dir` without naming the layout, so it needs no
  change; the implementer confirms that no table or paragraph in it names
  `<artist>--<album>` before concluding that.
- A dated design decision recording the superseded layout already exists
  (`docs/agent/specs/2026-09-15-discover-library-placement-design.md`,
  `docs/agent/specs/2026-09-23-remove-organize-design.md`); those files are left as written.
- The one-way migration is called out in the release note for the version that ships it.

## Alternatives considered

**Keep the flat name, change the separator.** Replacing `--` with a character an artist name
cannot contain would fix the boundary test but not the request, and no character is both
safe on every filesystem and excluded from every metadata value.

**Sanitise the album component only.** Leaves the artist level as the one staging folder
whose name is not portable, which is the defect the library sanitiser exists to prevent, and
puts staging and library artist folders out of step.

**Keep the layout rule in `organizer.rs`.** Smallest diff, but the layout rule would stay
split between `runner.rs` (build) and `organizer.rs` (decode, recovery), which is what made
this change touch so many sites.

**Thread a typed `StagingAlbum` value through every call.** Stronger typing, but it changes
`download.rs` signatures and many call sites in a 10,700-line `runner.rs` for a layout change,
and the same guarantee is available from a single constructor plus a path comparison.

**A sidecar manifest recording the raw artist and album.** Lossless even when two raw names
sanitise to one directory name, but it puts a non-audio file inside every staged album, which
the placement and resume walks would then have to exclude from the library copy.

**Sanitise the existing artist folder name in `place_into_library`.** Rejected: the folder
already exists under that name, so sanitising it would write the album into a second artist
folder beside the real one.

## References

- `src/runner.rs` — `process_album_internal` (staging path), `discard_refused_staging`,
  `remove_empty_staging_dirs`, `finish_library_write`, `mark_album_processed_if_identifiable`,
  staging setup and the recovery call.
- `src/organizer.rs` — `sanitize_component`, `library_relative_path`, `album_dir_for`,
  `album_dir_for`'s guard, `disc_subdir`, `parse_album_slug`, `resume_library_upgrade`,
  `recover_interrupted_upgrades`, `delete_lesser_quality_files`.
- `src/db.rs` — `get_processed_albums`, `get_album_status`, `mark_album_processed`.
- `src/discover.rs` — `ArtistFolderIndex` and its refusal of the `_` placeholder.
- `src/search.rs` — the sanitised library lookups.
- `docs/agent/specs/2026-09-23-remove-organize-design.md` — the fixed library layout.
- `docs/agent/specs/2026-09-15-discover-library-placement-design.md` — placement never
  creates an artist folder.
