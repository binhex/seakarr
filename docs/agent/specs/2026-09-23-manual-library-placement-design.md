<!-- markdownlint-disable MD013 -->
# Manual downloads placed into the existing artist folder

## Problem

Manual runs download successfully and then leave the album in the staging directory. The
operator asked for `--artist "The Cinematic Orchestra" --mode manual` to fill gaps in their
library and had to move all five albums across by hand:

```text
INFO seakarr::runner: Completed: The Cinematic Orchestra - To Believe (8 tracks) -> downloads/staging/The Cinematic Orchestra--To Believe (kept in staging)
INFO seakarr::report: Downloaded (5):
INFO seakarr::report:   The Cinematic Orchestra — Motion (7 tracks) -> downloads/staging/The Cinematic Orchestra--Motion (kept in staging)
...
```

The cause is a literal `None`. Both manual forms pass no library target to the album pipeline:

- artist-only (`--artist X` without `--album`) → `process_artist_album_work` →`process_album_internal(..., target: None, ...)` at `src/runner.rs:1397`;
- explicit album (`--artist X --album Y`) → `run_manual_mode` → `process_album(..., None, // target: manual mode has no library write)` at `src/runner.rs:2234`.

`target: None` means the destination is staging, which `src/report.rs:26` renders as
`(kept in staging)`. Discover mode is the only mode that places new albums today, and the
behaviour is locked by a test that says so explicitly
(`artist_only_mode_leaves_the_download_in_staging`, `src/runner.rs:5826`):

> Only discover mode places files in the library. Artist-only, manual and batch runs keep
> their completed download in staging, so this locks the "only discover places" contract
> against a future reroute of artist-only runs through the placement path.

The scan that artist-only runs perform (`discover::index_from_paths`, `src/runner.rs:1477`
and `:1578`) is used **only** for the presence check (`discover::missing_albums`), not for a
destination. So the operator's reading was correct: the run reads the library to decide what
is missing and then never writes to it.

## Goal

A manual run's completed albums land in the artist's **pre-existing** library folder, so the
operator no longer moves files by hand, without changing anything else about how manual runs
behave.

## Scope

In scope:

- placing downloads of both manual forms (artist-only and explicit album) into the existing
  artist folder under a configured library path;
- deciding, before any copy, that the destination album folder is free (otherwise the
  download stays in staging);
- an `INFO` line when the artist has no library folder, and one when an album folder already
  exists, so a non-placement is never unexplained;
- inverting the locked contract test and updating the README passages that assert
  "only discover places".

Out of scope:

- `--mode batch` (`src/main.rs:659` also passes `target: None`); it is a separate loop and is
  not part of this change;
- `--mode auto` and `--mode discover`; their existing behaviour, including discover's
  unconditional placement, is unchanged;
- sweeping staging folders left by earlier runs (including the albums the operator moved by
  hand);
- the presence check, `--ignore-processed` semantics, and the search/quality filters;
- merging into or overwriting an album folder that already exists;
- any new configuration key or CLI flag;
- retiring `storage.organize`, the generic organize step, or the `organize_pattern` key. That is a
  separate change with its own spec: `organize_pattern` is not organize-specific (it names every
  library write, expanded at `src/runner.rs:882` for upgrades, `:967` for placement and `:1058`
  for organize), and the organize step remains the only library route for batch mode and for auto
  mode with `library_upgrade.enabled: false`.

## Decisions

1. **Both manual forms place.** Artist-only runs already walk the library, so the destination
   is nearly free; the explicit-album form gets it from a bounded directory listing, which
   keeps that form walk-free (its documented role is the fast escape hatch, and it is the
   only way to force a re-download of something already present).
2. **The destination comes from the filesystem, not the scan index.** The index knows
   `(library_root, artist_dir)` per artist (`discover::LibraryIndex::artist_destination`,
   `src/discover.rs:167`), but it only lists artists the walk found audio for, and it follows
   a most-albums rule. Reading the directory directly makes the condition literal ("the artist
   folder must exist"), keeps working when a scan fails or is stale, and costs one listing per
   configured root instead of a walk.
3. **An existing album folder means the download stays in staging.** Only a destination
   strictly below the artist folder counts as an album folder: the artist folder itself (a
   pattern without `%album%`) and the library root (a pattern without `%artist%`) are not album
   folders, so those patterns still place. For a real album folder, placement never replaces a
   readable file, so writing into an existing one could report success while adding nothing — the
   exact confusion the operator wanted to avoid. Staying in staging keeps the download visible
   and untouched for a manual decision.
4. **The gate is implemented once, in the `Place` arm.** `LibraryTarget::Place` gains a
   `skip_existing_album` flag; discover and auto pass `false`, so their paths are byte-for-byte
   unaffected and only the album pipeline learns the rule.
5. **Two `INFO` lines explain non-placement** (no artist folder; album folder already exists).
   Neither is a failure: the album is downloaded and reported as Downloaded → staging, exactly
   as today.
6. **Multi-root resolution is config order.** The first configured library path containing a
   matching artist folder wins. This deliberately differs from discover's most-albums rule,
   which needs album counts that a manual placement does not have; config order is
   deterministic and easy to explain.
7. **The operator-visible contract reversal is recorded.** The old contract is quoted above and
   is replaced by: *a manual run places into the artist folder that already exists; it never
   creates one*. The README and the locked test are updated together.
8. **No new configuration.** The two conditions (artist folder exists, a library path is
   configured) are the whole rule, and they are derived from state, not settings.
9. **Batch mode is left alone** and recorded here as a deliberate exclusion, so a later change
   is a decision rather than an oversight.
10. **Placement runs regardless of `storage.organize`, and a manual run never organizes.** An
    album with a placement target is placed, or stays in staging when a directory a placement
    would write into already exists. An artist with no library folder passes
    `LibraryTarget::StagingOnly`: the album stays in staging and the generic organize step is
    skipped for it, so `storage.organize: true` does not create an artist folder for a manual
    run. That was settled during review (the implementation originally let organize create the
    folder, which contradicted "a manual run never creates one"); the organize step keeps its
    old behaviour for batch mode and for auto mode with `library_upgrade.enabled: false`, and
    retiring it outright remains a follow-up change (see Out of scope).
11. **The organize step and `storage.organize` stay in place** for the modes that still rely on
    them (batch, and auto mode with `library_upgrade.enabled: false`), and `organize_pattern`
    remains the naming template for placement, upgrades and organize alike.

## Refinements made during planning

1. **The matching rule is specified exactly.** The resolver sanitises both the candidate folder
   name and the artist through `organizer::sanitize_component`, then compares
   `normalize_catalog_key` of each. This is what makes `AC/DC` (tag) find `AC-DC` (stored); a
   bare case-insensitive comparison of raw names would not, and an earlier draft of the Error
   handling section overstated it.
2. **The skip path finishes through `finish_library_write`** rather than returning the outcome
   directly, so an album left in staging still gets its processed record, its notification and
   the summary destination it gets today.

## Architecture

### `src/discover.rs` — destination resolver

```rust
/// The library root and on-disk artist folder a manual run should place this
/// artist's downloads into, or `None` when no configured library path holds a
/// folder for the artist.
///
/// Reads the filesystem rather than the scan index: the index only lists artists
/// the walk found audio for, and a failed scan must not change where a manual
/// download lands. Matching is on the sanitised artist name, case-insensitively,
/// and the returned folder name is the spelling that exists on disk, so a
/// placement lands inside the existing folder instead of beside a rewritten copy
/// of it.
pub fn resolve_artist_folder(config: &Config, artist: &str) -> Option<(PathBuf, String)>;
```

Each configured library path is listed once, in configuration order. Entries that are not
directories, and directories whose sanitised name does not match the sanitised artist name
case-insensitively, are skipped. The first match wins and is returned with its on-disk
spelling. No recursion, no tag reads, no album counting.

### `src/organizer.rs` — destination album folder

```rust
/// The album directory placement would write into, derived exactly as
/// `copy_into_library` derives it: expand the pattern for the first downloaded
/// file (artist value verbatim), then take that path's parent, recorded before
/// any disc subdirectory is inserted.
pub fn placement_album_dir(
    library_root: &Path,
    pattern: &str,
    artist_dir: &str,
    album: &str,
    first_downloaded: &Path,
) -> Option<PathBuf>;
```

This mirrors `copy_into_library` (`src/organizer.rs:527-545`) rather than guessing from the
album title, so a pattern such as `%artist%/%album% (%year%)/%track% - %title%.%ext%` is
honoured: the check tests the very directory the copy would use.

### `src/runner.rs` — the gate and the call sites

`LibraryTarget::Place` (`src/runner.rs:248-251`) gains `skip_existing_album: bool`. In the
`Place` arm (`:933`), before `organizer::place_into_library` is called:

```rust
let existing_album_dir = if skip_existing_album {
    downloaded.first().and_then(|first| {
        organizer::placement_album_dir(
            root,
            &config.storage.organize_pattern,
            artist_dir,
            album.unwrap_or("Unknown"),
            first,
        )
    })
} else {
    None
};

// Only a destination strictly below the artist folder is an album folder: a pattern
// that omits `%album%` derives the artist folder, one that omits `%artist%` derives
// the library root, and neither is an album folder to collide with.
let existing_album_dir = existing_album_dir.filter(|dir| {
    let artist_dir_path = root.join(artist_dir);
    dir != &artist_dir_path && dir.starts_with(&artist_dir_path)
});

if let Some(existing) = existing_album_dir.filter(|dir| dir.is_dir()) {
    tracing::info!(
        "{artist} - {}: album folder already exists at {}; leaving the download in staging",
        album.unwrap_or("?"),
        existing.display()
    );
    // The same finish a staging album gets at the tail of this function, so the
    // processed record, the notification and the summary destination stay
    // exactly as they are today.
    return finish_library_write(
        config,
        db,
        &album_staging,
        artist,
        album,
        downloaded.len(),
        DownloadDestination::Staging(album_staging.clone()),
    )
    .await;
}
```

Nothing else in the arm changes: the completeness backstop, the placement call, the
`finish_library_write` success path (staging removal, processed record, notification, library
destination in the report) and the failure path all stay as they are.

Call sites:

- `run_artist_only_mode_with_provider` and `run_legacy_artist_only_mode` call
  `resolve_artist_folder` once per run and pass `Some(manual_place_target(&destination))` to
  `process_artist_album_work`, which forwards it to its `process_album_internal` call.
- `run_manual_mode` builds the same target for its `process_album` call.
- `manual_place_target` returns `LibraryTarget::Place { skip_existing_album: true }` when the
  artist folder was found and `LibraryTarget::StagingOnly` when it was not.
- Discover passes `skip_existing_album: false`; auto mode's `LibraryTarget::Upgrade` is
  untouched.

Once per run, when library paths are configured and an album really was downloaded into staging
because the artist has no folder, the run explains it:

```text
{artist}: no library folder found under the configured library paths; downloads stay in staging
```

A run that placed, failed or skipped every album logs nothing, and an album-only run is silent
because it never looked for an artist folder. With no library path configured the resolver returns
`None` without a line — that configuration means staging is the destination, which is today's
documented default.

## Data flow

```text
manual run (artist-only or explicit album)
  ├─ resolve_artist_folder(config, artist)
  │     └─ None ─────────────────────► target = StagingOnly   (stays in staging; organize suppressed)
  │     └─ Some((root, artist_dir)) ─► target = Place { skip_existing_album: true }
  │
  ├─ album downloaded into staging  (unchanged)
  │
  └─ Place arm
        ├─ derived destination strictly below the artist folder, and existing
        │     └──────────────────────────────► INFO + Downloaded -> Staging (kept in staging)
        ├─ place_into_library(...) ok ────────► finish_library_write -> Library
        │                                          (staging removed, processed=success, notify)
        └─ place_into_library(...) err ───────► error + Failed (staging kept)

        every branch returns, so the generic organize step is not reached for this album
```

## Error handling

- **No library path configured**: resolver returns `None`; staging. Silent.
- **Artist folder absent**: resolver returns `None`; staging, with the single `INFO` line.
  Matching sanitises both sides and compares through `normalize_catalog_key` (`src/discography/mod.rs:14`:
  NFKC, lowercased, whitespace collapsed), so the tag spelling finds the stored folder — `AC/DC`
  matches a folder stored as `AC-DC`, because the sanitiser writes the separator as a hyphen — and
  the on-disk spelling is what gets written to.
- **Artist folder found**: always a single path component, because the resolver only returns a
  direct child of a configured root — exactly what `place_into_library` requires
  (`src/organizer.rs:483-490`). The verbatim pattern expansion substitutes `%artist%` last, so
  an on-disk name containing a `%` cannot cascade into another field.
- **Album folder already exists**: staging kept, `INFO` naming that folder, nothing written or
  removed; the album is reported as Downloaded → staging.
- **Placement fails** (permissions, disk full, destination named as a directory): error logged,
  album Failed, staging kept — inherited from the existing arm (`:1006-1018`).
- **Completeness backstop** (`library_write_refusal`): the identical rule already runs
  before the download (`src/filter.rs:98` and `:141`), so manual runs reach it no more often
  than discover does. If it fires, the album is Failed and the staging copy is discarded, as in
  discover; `filters.min_tracks: 0` remains the escape hatch for singles and EPs. One corner does
  not reach it: the existing-album gate returns before the backstop, so a manual album whose
  folder already exists keeps its staged files and is recorded as a success rather than being
  refused — deliberate, because nothing is written into the library in that case. That staging
  leftover is a normal `success` record, so a later auto run with `library_upgrade.enabled: true`
  may adopt it into `library.paths[0]` through `recover_interrupted_upgrades`; keeping it out of
  that recovery is a separate change.
- **`--ignore-processed`**: unchanged. It cannot bypass the presence check and has no effect on
  placement.
- **Scan failed or stale**: the presence check is blind (existing warn-and-continue behaviour),
  placement still resolves from disk, and the album-existence gate keeps those downloads in
  staging rather than writing over an existing album folder.
- **Multi-root**: the first configured root with a matching artist folder wins. Documented
  above; deterministic.
- **`storage.organize` interaction**: placement is attempted regardless of the flag and the
  `Place` arm returns, so the generic organize step never runs for an album that has a placement
  target. With `storage.organize: true` and no artist folder for that artist, the download stays
  in staging where organize would previously have copied it into a newly created folder. That is
  the accepted cost of making "the artist folder must exist" a hard condition; the organize step
  itself is untouched for batch mode and for auto mode with `library_upgrade.enabled: false`.

## Backward compatibility

| Behaviour | Before | After |
| --- | --- | --- |
| `--artist X`, artist folder exists | staging | **placed in the artist folder** |
| `--artist X`, no artist folder | staging | staging (unchanged, plus an INFO line) |
| `--artist X --album Y`, artist folder exists | staging | **placed in the artist folder** |
| `--artist X --album Y`, no artist folder | staging | staging (unchanged, plus an INFO line) |
| Either form, album folder already exists | staging | staging (nothing written, INFO naming the folder) |
| Either form, no library path configured | staging | staging (unchanged) |
| Either form, artist folder exists, `storage.organize: true` | organised into the library (creating the artist folder when absent, keeping existing files) | **placed into the existing artist folder**; staging when the album folder already exists |
| Either form, no artist folder, `storage.organize: true` | organised into the library (artist folder created) | staging (the artist folder must exist) |
| `--mode batch`, `--mode auto`, `--mode discover` | — | unchanged |

Unchanged: the presence check, `--ignore-processed`, the quality filters, the download
pipeline, the processed-album records, notifications, and the run summary's shape (its
destination now names the library when the album was placed).

## Deliberate limits

- No merge and no overwrite. An existing album folder always wins, and the download stays in
  staging; replacing a corrupt file remains a manual operation.
- No sweep of earlier runs. Staging folders that predate this change (including the albums the
  operator moved by hand) are untouched.
- The destination is the artist's existing folder, so a manual run never creates an artist
  folder. An artist absent from the library keeps everything in staging, by design.
- Multi-root libraries resolve by configuration order, not by album counts.
- Placement does not consider album quality or upgrade semantics; it copies what was
  downloaded, exactly as discover mode does.
- Placement pre-empts the generic organize step for manual runs, and a manual run with no
  existing artist folder passes `LibraryTarget::StagingOnly`, which suppresses organize as well:
  a manual run never creates an artist folder, whatever `storage.organize` says. The organize
  step keeps its old behaviour for batch mode and for auto mode with `library_upgrade.enabled:
  false`; retiring it is the follow-up change that would remove this asymmetry.

## Testing

Updates to existing tests:

1. `artist_only_mode_leaves_the_download_in_staging` (`src/runner.rs:5826`) becomes
   `artist_only_mode_places_into_the_existing_artist_folder`: the track lands under
   `<library>/Test Artist/Album/…` and the staging copy is gone. Its "only discover places"
   comment is replaced by the new rule.
2. `artist_only_manual_mode_downloads_each_discovered_album` keeps its intent. The multi-disc
   test became
   `artist_only_manual_mode_keeps_a_multi_disc_album_in_staging_without_an_artist_folder`:
   with `storage.organize: true` and no artist folder the album stays in staging, disc folders
   and all, and no library folder is created. A new
   `manual_mode_places_a_multi_disc_album_into_the_existing_artist_folder` covers the placed
   disc layout.
3. `artist_only_manual_still_runs_when_the_library_path_is_missing` (`:5997`) additionally
   asserts the album stayed in staging.

New unit tests:

1. `resolve_artist_folder`: case-insensitive match returning the on-disk spelling; `None` when
   absent; two roots resolve to the first configured; empty `library.paths` returns `None`;
   a file (not a directory) named like the artist is ignored.
2. `placement_album_dir`: matches `copy_into_library`'s `album_dir` for the default pattern,
   for a pattern whose album component carries another placeholder, and for a multi-disc staging
   layout (the disc subdirectory is not part of the album folder).

New integration tests:

1. Artist-only, artist folder exists → placed into it; staging removed; album recorded as
   processed; report names the library path.
2. Artist-only, artist folder absent → staging, and the INFO line is emitted (LogCapture).
3. Artist-only, album folder already exists → staging kept, INFO names the folder, and the
   existing files are untouched.
4. Explicit album (`run_manual_mode`), artist folder exists → placed, and the run performs no
   library walk (no scan indicator/heartbeat in the captured log), proving the bounded listing
   replaced the walk.
5. Explicit album, album folder already exists → staging kept.
6. Placement failure (a destination that cannot be created, for example a file where the
   album folder belongs) → album Failed, staging copy kept, record marked failed.
7. No library paths → staging for both forms: `artist_only_manual_still_runs_when_the_library_path_is_missing`
   asserts it for the artist-only form and `manual_mode_without_library_paths_keeps_staging` for the
   explicit-album form.

Behaviour guards: the discover placement tests and the auto-mode upgrade tests must pass
unchanged.

## Acceptance criteria

- [ ] `--artist X` with a pre-existing artist folder places every downloaded album into it.
- [ ] `--artist X --album Y` does the same, without any library walk.
- [ ] An artist folder that does not exist leaves the download in staging, with an INFO line
      explaining why.
- [ ] No configured library path leaves the download in staging.
- [ ] An existing album folder is never merged into or overwritten: the download stays in
      staging and the reason is logged.
- [ ] A placement failure reports the album as Failed and keeps the staging copy.
- [ ] `--mode batch`, `--mode auto` and `--mode discover` behaviour is unchanged.
- [ ] With `storage.organize: true`, a manual run with an existing artist folder places into it,
      and an existing album folder keeps the download in staging; the organize step is not reached
      for those albums.
- [ ] `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test` and
      `pre-commit run --all-files` all pass; coverage stays at or above the 95 % project target.

## Documentation

- README: the `--artist` and `--album` rows, the manual-mode description, and the placement
  section must state the new rule (place into an existing artist folder; an existing album
  folder keeps the download in staging; the explicit-album form needs no scan), replacing the
  passages that imply only discover writes into the library. The `--ignore-processed` note
  keeps its "never bypasses the presence check" wording, which is still true.
- This spec records the superseded "only discover places" contract.
- The plan file records the test inversion and any deviation found during implementation.

## Alternatives considered

- **Resolve the destination from the scan index** (`LibraryIndex::artist_destination`, the
  most-albums rule): rejected because placement would then depend on a walk that can fail or be
  stale, and the explicit-album form would gain a walk it deliberately does not have.
- **Resolve inside `process_album_internal`** by passing a "manual" token: rejected because
  filesystem lookups would land inside the album pipeline (cognitive complexity 74) instead of
  a pure, unit-testable helper.
- **Post-run sweep of staging folders**: rejected because staging folder names are display
  slugs, so mapping back to artist and album is fragile; it would also lose per-album
  reporting, notifications and processed records, and would move historical leftovers.
- **Match the album folder by name** (sanitised album title, case-insensitive) instead of the
  pattern-exact directory: rejected because a name-shaping pattern would make the check miss,
  reintroducing the silent-merge case the gate exists to prevent.
- **Gate placement on `storage.organize` being off**, leaving organize-enabled runs untouched:
  rejected because the operator wants one rule for manual runs and intends to retire organize
  entirely; the consequence is recorded above instead of hidden behind a flag.
- **Fall through to the organize step when the album folder already exists**: rejected because it
  would merge into an existing album folder, the exact outcome the gate exists to prevent.
- **Remove `storage.organize` and `organize_pattern` as part of this change**: rejected as a
  second subsystem. `organize_pattern` names every library write (upgrades and placement
  included, `src/runner.rs:882` and `:967`) and the organize step is still the only library route
  for batch mode and for auto mode with `library_upgrade.enabled: false`; both were agreed as a
  separate change with its own spec.
