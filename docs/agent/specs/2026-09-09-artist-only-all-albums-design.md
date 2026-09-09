# Artist-only manual searches download all discovered albums

## Problem

A manual artist-only search currently runs one artist query and passes all
returned files to a single album-processing pipeline. The downloader
intentionally selects one largest logical album group per candidate to prevent
cross-album mixing, so an artist search can download only one album even when
the search response contains several albums.

## Goal

When manual mode receives `--artist <artist>` without `--album`, discover
distinct albums from the single artist search and process one best downloadable
copy of each album. Preserve the existing behavior for explicit artist+album
searches, album-only searches, batch mode, and automatic library upgrades.

## Non-goals

- Do not change Soulseek query semantics or add a new network search per
  discovered album.
- Do not make the lower-level downloader process multiple albums in one
  staging directory.
- Do not change album selection for explicit `--album` searches.
- Do not infer album names from unstructured/root-level filenames when no album
  directory is present.

## Design

### Artist-only discovery

`run_manual_mode` will use the existing artist-only search once. The search
result set will be grouped by logical album directory before invoking the album
pipeline:

- `Artist/Album/track` files use `Album` as the logical album.
- Dedicated disc directories such as `Album/CD 01/track` are folded into
  `Album`.
- Embedded disc markers in an album directory remain one logical album.
- Each peer remains a separate candidate, with only that candidate's files for
  the current album group retained.

Files without an identifiable artist/album directory are excluded from
artist-all discovery. If no identifiable groups remain, the run records a
failed `(all)` target instead of choosing an arbitrary largest folder.

### Pipeline reuse

The existing post-search portion of `process_album` will be extracted into a
private helper that accepts already-searched candidates and an explicit album
name. `process_album` will continue to perform its normal search, then call
that helper. Artist-only mode will call the helper once per discovered album.

Each invocation retains the existing single-album invariants:

- processed-album checks and `--ignore-processed` apply to the concrete
  artist/album pair;
- filtering, ranking, and candidate fallback operate only on that album's
  files;
- each album receives an independent staging directory;
- organization, notifications, and run-report entries use the concrete album
  name;
- incomplete or failed albums do not prevent later discovered albums from
  being attempted;
- cancellation stops further work and cleans active staging.

The artist-only search is recorded once with no album. Processing
already-searched groups does not issue additional Soulseek searches or
duplicate search-history rows.

## Error handling

- A search error remains a hard error and propagates through the existing
  manual-mode result path.
- An album-level failure is recorded in `RunReport` and processing continues
  to other groups.
- Setup and database errors retain the existing propagation behavior.
- No identifiable album groups produce a failed `(all)` report entry with an
  actionable reason.

## Testing

1. Add a grouping test with two albums, multiple peers, and multi-disc paths.
   Verify that files are grouped by logical album and never mixed.
2. Add an artist-only end-to-end test using `MockClient` that verifies one
   artist query, both albums downloaded, independent staging/processing, and
   concrete processed-album records.
3. Preserve and run the existing explicit album, album-only, batch, auto,
   filtering, download, and organization tests.
4. Run formatting, clippy, the full Rust test suite, pre-commit, and
   `git diff --check` before completion.
