# Ignore Processed Albums Design

## Goal

Add a CLI-only `--ignore-processed` option that allows an intentional one-shot
reprocessing attempt for an artist/album already marked successful in SQLite.

## Decisions

- Flag name: `--ignore-processed`.
- Existing successful database record: delete the exact artist/album row before processing; failed records retain their attempt history.
- Daemon mode: reject `--ignore-processed` when daemon mode is enabled, before startup side effects.
- Scope: usable in one-shot auto, manual, and batch modes.
- Album-only searches remain unaffected because they have no processed-album key.

## Architecture

The flag is a runtime-only boolean carried through `CliOverrides` and the
existing execution paths into `runner::process_album`, the shared point where
the processed-album skip currently occurs.

When enabled, `process_album` deletes the exact processed-album row immediately
before the current skip check and logs the bypass. Processing then follows the
existing search, filtering, download, organization, notification, and status
recording flow. A successful retry recreates the success row; a failed retry
uses the existing failure-status behavior. Hard search errors also create a
failed record so prior processing state is not silently lost. Search history is
not deleted.

The database deletion uses a parameterized artist/album query restricted to
`status = 'success'` and does not affect other albums. No schema migration or
configuration-file setting is needed.

## CLI behavior

Example:

```bash
seakarr --mode manual --artist "Afterlife" \
  --album "The Afterlife Lounge" --ignore-processed
```

With an existing record, the application reports an informational bypass such
as:

```text
Ignoring already-processed record: Afterlife — The Afterlife Lounge
```

Without an existing record, the flag has no special effect and processing
continues normally. Without the flag, the current skip behavior is unchanged.

`--ignore-processed` with daemon mode returns a configuration error before
logging setup, database processing, PID locking, or Soulseek login. The same
applies when daemon mode is enabled in YAML. This prevents a forced download on
every daemon cycle.

## Mode propagation

- **Manual:** the flag applies to the requested artist/album.
- **Batch:** the flag applies independently to each artist/album entry.
- **One-shot auto:** the flag applies to albums selected by the existing
  library-upgrade scanner; it does not make every library album a target.
- **Daemon:** explicitly rejected.

## Files and responsibilities

- `src/main.rs`: define the Clap flag, reject the daemon combination, construct
  `CliOverrides`, and propagate the value through dispatch.
- `src/config.rs`: add the runtime-only `CliOverrides` field without changing
  YAML serialization.
- `src/db.rs`: add parameterized deletion of one processed-album row.
- `src/runner.rs`: accept the flag in `process_album`, delete the matching row
  before the skip check, and log the bypass.
- `tests/`: cover database deletion, normal skip behavior, manual/auto/batch
  propagation, successful recreation, failed retry status, and daemon
  rejection before side effects.
- `README.md`: document usage, one-shot scope, database-record deletion, and
  daemon incompatibility.

## Error handling and safety

- Database deletion errors are returned instead of ignored.
- Only the exact artist/album pair is deleted.
- Existing records for other artists or albums remain unchanged.
- The flag is not persisted and cannot silently alter later invocations.
- Normal skip and status-update behavior remains unchanged when the flag is
  absent.

## Out of scope

- Automatic corruption detection.
- File integrity validation.
- A configuration-file equivalent.
- Repeated forced processing in daemon mode.
- Deleting search history or unrelated database records.
