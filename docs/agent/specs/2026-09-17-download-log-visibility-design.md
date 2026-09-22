# Download log visibility

> **Superseded in part (2026-09-21).** Item 2 below reads "first `InProgress`":
> that signal is now the first `InProgress` that *reports bytes*. The transfer-start
> point moved from the peer's accept/offset handshake to the first byte-carrying
> progress report, so a peer that accepts and then goes silent gets its queued line
> at the 5-second grace instead. The rest of this design is unchanged.

## Problem

Two operator-reported defects in the download log, both about information the
run already knows and never prints.

### The final destination is never logged

The only path seakarr reports is the **staging** copy, and it is reported as if
it were the destination. Abridged, with `{staging}` standing in for the
per-album staging directory:

```text
INFO seakarr::download: Download completed: 08 Moondance.flac -> {staging}/...
```

The arrow, the word "completed", and the absence of any later line together
read as "this is where the album ended up". It is not. For a tracked library
write the file is copied into the library minutes later and the staging copy is
removed, and the album completion line carries no path at all:

```text
INFO seakarr::runner: Completed: Aquasky - Shadow Era Pt. 1 (8 tracks)
```

So an operator watching a run cannot answer "where did this album go?" without
walking the library. The reported run is the ordinary case; the album was placed
in the library and nothing in the log said so.

The information exists at the point of completion in every code path and is
discarded in all of them:

- `organizer::copy_to_library` (`organizer.rs:398`) and
  `organizer::place_into_library` (`organizer.rs:442`) return the written
  destinations, and `runner.rs:658` / `runner.rs:720` use them only to drive the
  lesser-quality deletion pass.
- `organizer::organize_file` (`organizer.rs:293`) returns the destination path
  and the generic organize loop throws it away — `runner.rs:789` matches
  `Ok(_) => {}` at `runner.rs:799`.
- `finish_library_write` (`runner.rs:169`) logs the counts and knows nothing.

Two further inconsistencies live in the same lines. `runner.rs:195` and
`runner.rs:850` are the same sentence with different punctuation (hyphen versus
em dash), because the completion line is duplicated rather than shared. And the
staging-only state — no library write at all, so staging **is** the final
location — is indistinguishable in the log from the staged-then-moved case.

### The queue position is never logged

A queued download says so and then goes silent:

```text
INFO seakarr::download: Download queued: 08 Moondance.flac from nottucks
```

`download.rs:431` fires immediately after `client.download()` returns a handle,
before any status is read, so no position exists yet — and no later line ever
mentions one. For a peer deep in its upload queue the operator sees one line and
then nothing until the queue limit expires or the transfer starts, with no way
to tell "waiting at position 40" from "hung".

> **Amended 2026-09-22:** the five-minute cadence below is no longer the
> only source of positions. seakarr now asks its peer every 30 seconds while
> an attempt is queued, so the interactive queue bar's number keeps up with
> the queue. See `2026-09-22-queue-position-refresh-design.md`. The logging
> decisions in this document — one notice, one started line, position
> changes at `DEBUG` — are unchanged.

The position is available. The vendored peer actor sends `PlaceInQueueRequest`
immediately on `QueueUpload` and every 300 seconds while that file remains
queued, and the status channel already carries it as
`DownloadStatus::Queued { queue_position }`. `download_once` already tracks
`observed_queue_position`, but only to write it into queue-timeout and rejection
messages.

Naively logging every position update is not acceptable: the positions are
one-based and a peer can report a queue hundreds deep, so change-driven logging
would emit one line per step and bury the log — exactly the "busy live counter"
the operator ruled out. Simply raising telemetry frequency is not the answer
either; the spam is proportional to queue depth, not to time.

## Goal

An operator reading the log must be able to answer both questions without
walking the filesystem:

- where did this album finally land, in one line per album, whether it was
  written into the library or deliberately left in staging;
- what queue position a download reached, on a bounded number of lines that does
  not grow with queue depth, with the live detail available without being forced
  on everyone.

Both must hold in a captured container log, a log file, and an interactive
terminal, because the operator reads all three.

## Scope

In scope:

- the album completion line and the staging line, reworded and made truthful;
- the destination path threaded from the three library-write paths and the
  staging-only path into the log, the run summary, and the notification payload;
- a bounded deferred queue notice and a queue-exit line, both emitted once per
  attempt;
- per-position-change detail at `DEBUG`, plus an interactive queue spinner;
- the `AlbumOutcome::Downloaded` shape change this requires;
- README notes for the new log lines.

Out of scope:

- ranking or pre-selecting peers by queue depth. Searches expose advertised
  speed and free slots only, user stats expose no queue field, and a position is
  not comparable between peers without that peer's turnover rate. Settled as
  impossible with this protocol; nothing here revisits it.
- `max_queue_length`, `max_queue_time_secs`, `max_start_time_secs` or any other
  queue policy. Semantics and fail-closed admission are unchanged.
- the wording of the existing queue timeout, rejection, quality-verification and
  failure warnings, which keep their current text and their position field.
- the transfer progress bar and its "no bar before the transfer starts"
  contract.
- new configuration keys. The queue notice grace is a constant and the
  position lines need no opt-in.
- a live counter. Explicitly rejected by the operator.
- notification payload fields other than the message string.

## Decisions

**The final destination is the album folder, reported on the album completion
line, once per album.** One line answers the question. Per-file lines multiply
by track count and duplicate the existing partial-write warnings, so per-file
destinations are emitted at `DEBUG` only.

**The completion line is one shared helper, not two duplicate literals.** The
hyphen/em-dash divergence is fixed by construction rather than by editing one of
the two copies.

**The destination is reported as actually written, never recomputed from
`organize_pattern`.** The pattern is not the truth: `sanitize_component` rewrites
every component, `ArtistComponent::Verbatim` substitutes the on-disk artist
folder, and multi-disc albums gain a disc subdirectory inserted after the
pattern is expanded (`organizer.rs:505`-`517`). Recomputing would print a path
that does not exist and would re-introduce the lossy mapping the sanitisation
work deliberately centralised. `LibraryWriteOutcome.album_dir` is derived from
the destinations the write actually produced.

**`album_dir` excludes the disc subdirectory, and is returned even when nothing
was written.** `dests[0].parent()` is wrong for a multi-disc album — it reports
the disc folder — and `dests` is empty when every destination already held a
better or parseable file, which is a legitimate successful placement. The
organizer knows whether it inserted a disc component, so it strips it when
deriving the album folder rather than guessing afterwards.

**The staging line is relabelled `Download staged`, not removed.** The path is
genuinely useful when debugging a run with organisation disabled, and removing
it would lose the per-file staging confirmation. Relabelling removes the
misleading arrow semantics: the line now says "staged here", and the completion
line says where the album ended up.

**The staging-only state is reported as the destination, marked as staging.**
When `storage.organize` is false or `library.paths` is empty, staging is where
the album stays and is removed only on a tracked write. Reporting
`(kept in staging)` is truthful; reporting nothing would leave the same gap the
report is about.

**The queue notice is deferred to the first of three events.** The line keeps
the operator's requested single-line form (`Download queued: … - position 42`)
by waiting for the position, but a bounded grace of 5 seconds prevents the
silence that motivated the report:

1. first positive position observation — the line carries the position;
2. first `InProgress` — the line carries no position, because the peer went
   straight to transferring; *(superseded 2026-09-21: the first `InProgress` that
   reports bytes — see the note above)*
3. 5-second grace elapsed — the line carries no position, and any later position
   reaches the started line and `DEBUG`.

Deferring indefinitely was rejected: a peer that never answers would emit no
queued line at all until the queue limit expired, which is the original defect.

**Five seconds is a constant, not configuration.** The vendored client requests
telemetry immediately on enqueue and the status poll window is 200 ms
(`download.rs:274`), so the position normally arrives within a few hundred
milliseconds and the deferral is invisible. Five seconds is generous enough to
absorb a slow round trip without delaying the line by a noticeable amount, and a
config knob for it would be surface area nobody would ever change.

**Position changes after the notice are `DEBUG`, plus the spinner.** The
queue-depth problem is a volume problem, so mid-queue updates are demoted rather
than throttled. Throttling by time still grows with queue duration; demoting to
`DEBUG` makes `INFO` output flat in both depth and duration while keeping the
full trace available on demand.

**The interactive queue bar is an addition, not a replacement.**
`ProgressDisplay` is constructed only when stderr is a terminal (`main.rs:577`),
and indicatif renders to stderr, so in a container log or the file appender a
spinner renders nothing at all. The bounded log lines are therefore the floor
that must stand on their own, and the bar is the interactive upgrade.

**The queue bar uses its own counter.** `bars_created` is documented as the
evidence that no bar appears before a transfer starts, and the test at
`download.rs:1527` asserts exactly that. A queue bar deliberately exists while
nothing has started transferring, so mixing the two would either weaken that
contract or make the test lie. `queue_bars_created()` is separate and the
transfer-bar contract is untouched.

**`AlbumOutcome::Downloaded` carries the destination.** The destination is
produced inside `process_album` and consumed at three places outside it — the
log line, the summary, and the notifier — so the outcome is the only carrier
that avoids a parallel channel threaded alongside it. The cost is a mechanical
test churn, recorded under Risks.

**Staging is modelled as a destination variant, not a boolean.** The wording of
the log line, the summary, and the notification all branch on it, and a
`Library(PathBuf)`/`Staging(PathBuf)` enum makes the two states impossible to
confuse while keeping the path in both.

## Configuration

No new keys. `logging.level` (default `INFO`, `--log-level` override) keeps its
meaning, and the demoted position updates are what makes `DEBUG` useful again:
`DEBUG` now shows the full queue trace that `INFO` deliberately does not.

## Architecture

### `src/organizer.rs`

```rust
/// Result of a library write: the album folder targeted and the files
/// actually written.
pub struct LibraryWriteOutcome {
    /// The album folder the write targeted, with any disc subdirectory
    /// removed. Present even when `written` is empty.
    pub album_dir: PathBuf,
    /// Destinations actually written. Excludes a file whose destination
    /// was kept because the library already held a better copy.
    pub written: Vec<PathBuf>,
}
```

`copy_to_library` and `place_into_library` return
`Result<LibraryWriteOutcome>` instead of `Result<Vec<PathBuf>>`.

`copy_into_library` derives `album_dir` while it iterates: it already computes
`dest` (`organizer.rs:505`) and already knows whether it appended a
`disc_subdir` component, so it records the first disc-stripped parent and
returns it unchanged for the remaining files. Every file of one album expands
the same album component, so the first value is deterministic.

A `tracing::debug!("Organized: {} -> {}", src.display(), dest.display())` is
added where the copy succeeds — the only place both paths are in hand.

### `src/runner.rs`

`AlbumOutcome::Downloaded` gains the destination (definition lives in
`src/report.rs`):

```rust
pub enum DownloadDestination {
    /// Written into the library at this album folder.
    Library(PathBuf),
    /// Deliberately left in staging; this is the album's final location.
    Staging(PathBuf),
}

AlbumOutcome::Downloaded {
    track_count: usize,
    destination: DownloadDestination,
}
```

The four paths map onto it:

| Path | Site | Destination |
| --- | --- | --- |
| Library upgrade | `runner.rs:658` | `Library(outcome.album_dir)` |
| Discover placement | `runner.rs:720` | `Library(outcome.album_dir)` |
| Generic organize | `runner.rs:789` | `Library(dest.parent())` |
| Organize disabled / no paths | fall-through | `Staging(album_staging)` |

`finish_library_write` takes the destination and becomes the single emitter of
the completion line, replacing both `runner.rs:195` and `runner.rs:850`. The
generic organize loop collects the `PathBuf` that `organize_file` already
returns; the album folder is its parent, and at least one success is guaranteed
before the line fires because a total failure returns `Failed` at
`runner.rs:825`.

### `src/report.rs`

`downloaded` becomes a list of entries carrying artist, album, track count and
destination, and the summary line renders the path, with `{library}` and
`{staging}` standing in for the absolute folders:

```text
INFO seakarr::report:   Aquasky — Shadow Era Pt. 1 (8 tracks) -> {library}
INFO seakarr::report:   Aquasky — Shadow Era Pt. 2 (6 tracks) -> {staging}
```

A staging destination renders with a `(kept in staging)` suffix.

### `src/notifier.rs`

`notify_success` gains the destination. `title` and `type` are unchanged; only
`message` grows the path, so a consumer parsing the payload sees a suffix rather
than a reshaped object:

```text
Downloaded "Aquasky — Shadow Era Pt. 1" (8 tracks) to {library}
```

### `src/download.rs`

`download_once` replaces the immediate log at `download.rs:431` with a deferred
notice and adds the queue-exit line:

```text
INFO Download queued: 08 Moondance.flac from nottucks - position 42
INFO Download started: 08 Moondance.flac from nottucks after 21m 40s queued
```

The started line appends `(last position {p})` when a position was ever seen. When
a transfer starts with no queue wait at all — the free-slot case, where
`peer_slots > 0` admitted the candidate and it began immediately — the same line
reads `Download started: {basename} from {username} immediately (free slot)`
instead of a duration and position, so every downloaded file still produces a
started line and the two forms stay distinguishable.

The grace deadline is checked alongside the existing queue deadlines inside the
poll loop, so no new timer or task is introduced. `observed_queue_position`
keeps its current meaning and its current consumers, and gains one more: the
last-position field of the started line.

`Download completed: {basename} -> {dest}` (`download.rs:606`) becomes
`Download staged: {basename} -> {dest}`.

### `src/progress.rs`

```rust
/// Create a bar for a file waiting in a peer's queue. Unlike a transfer
/// bar this deliberately exists before any transfer starts.
pub fn create_queue_bar(&self, label: &str) -> ProgressBar

/// Number of queue bars created so far (monotonic), tracked separately
/// from `created_bars` so the transfer-bar contract keeps its meaning.
pub fn queue_bars_created(&self) -> usize
```

Template `{spinner} {msg}` with a steady tick and message
`08 Moondance.flac - nottucks queue #42`, added to the same `MultiProgress` as
the transfer bars so concurrent queued albums each get their own line. The
label is filtered for control characters the same way `create_bar` already
filters filenames, because it carries peer-supplied text.

## Data flow

```text
enqueue ─► poll loop (200 ms)
  ├─ first position ──► queue bar (TTY) + queued line w/ position
  ├─ first InProgress ► queued line, no position
  └─ 5 s grace ──────► queued line, no position

later position updates ─► queue bar update + DEBUG line only

transfer start ─► queue bar cleared, transfer bar created
               └► started line with wait and last position

library write ─► LibraryWriteOutcome { album_dir, written }
   │
   ├─► completion line (INFO, one per album)
   ├─► summary entry (INFO)
   └─► notification message (webhook payload)
```

## Error handling

Queue policy is untouched. Fail-closed admission on an unproven position, the
`max_queue_length` rejection, and both queue deadlines keep their behaviour and
their current warning text, including the position field. The started line never
substitutes for a failure line: an attempt that is rejected or expires emits its
existing warning and falls back to the next candidate.

A grace expiry followed by a late position is not an error path: the position
reaches the started line and `DEBUG`, and no late `INFO` line is emitted, so the
"two lines per file" bound holds even for a peer that answers after the grace.

The queue bar is released on every path the attempt can take out of the queue —
transfer start, `expire_queue_wait`, `reject_queued_attempt`, cancellation, and
channel close — mirroring the existing transfer-bar lifecycle so no bar outlives
its attempt.

Progress display remains optional. Every new bar interaction is inside the
existing `Option<&ProgressDisplay>` guard, so a non-interactive run and a failed
bar render cannot fail a transfer.

A destination that cannot be derived is not an error: the staging-only path
always has `album_staging`, and a library write always has `album_dir` or, in
the generic path, at least one successful destination. Organisation failure
continues to produce `Failed` and therefore no completion line.

## Backward compatibility

No new YAML keys, no database migration, no schema change. `logging.level`
keeps its default and its override.

Log output changes shape, which is the point of the change and is worth stating
plainly: `Download completed` becomes `Download staged`, `Download queued` may
be delayed by up to five seconds and may gain a position suffix, one new
`Download started` line appears per downloaded file, and the completion line and
summary entries gain a path. Anyone grepping for `Download completed` must
update their pattern.

`AlbumOutcome::Downloaded` is a public enum in the library target and gains a
required field, so it is a breaking change for any external consumer. The crate
ships as a binary and the field is additive in meaning, so this is accepted
rather than versioned around.

The notification payload keeps its `{title, message, type}` shape, so an Apprise
API server and any consumer that renders `message` keep working; only the
message text changes.

The transfer progress bar, `bars_created` semantics, and the
`download.rs:1527` test are unchanged.

## Deliberate limits

- **The album folder, not the file path.** Per-file destinations are `DEBUG`
  only.
- **No `INFO` line per position change.** Only the first observation, the start
  line, and the timeout/rejection warnings mention a position at `INFO`.
- **The grace is fixed at 5 seconds.** A peer that answers later gets no queued
  position line.
- **A queue bar is invisible without a TTY.** Stated above deliberately: the log
  floor is designed to stand alone, and no attempt is made to render bars into
  captured logs.
- **No queue-depth ranking.** Unchanged from the earlier finding.
- **No notification field for the destination.** The path rides in the message
  string rather than becoming a structured field.
- **`organize_pattern` is never printed.** The reported path is what was
  written.

## Testing

RED first, per the project standard.

`organizer.rs`:

1. `copy_to_library` returns `album_dir` without the disc component for a
   multi-disc staging layout.
2. `album_dir` is returned, and `written` is empty, when every destination was
   kept because the library already held a better or parseable copy.
3. `place_into_library` returns the on-disk artist folder's album directory when
   `ArtistComponent::Verbatim` is in play.

`runner.rs`:

1. The completion line carries the library album folder for the upgrade path.
2. The completion line carries the library album folder for the placement path.
3. The completion line carries the library album folder for the generic organize
   path.
4. The completion line carries the staging directory marked as staging when
   organisation is disabled and no library target applies.
5. The two former completion literals produce identical text for the same
   outcome (the punctuation divergence is gone).

`download.rs`:

1. Position observed before any transfer: exactly one queued line, carrying the
   position, then the started line with the wait and last position.
2. Free-slot attempt: exactly one queued line with no position, then a started
   line reading `immediately (free slot)`.
3. Silent peer: the queued line is still emitted at the grace deadline, with no
   position, and the attempt is not otherwise altered.
4. A position change after the notice emits no `INFO` line and one `DEBUG`
   line, asserted through a capturing subscriber.
5. A late position after a grace expiry reaches the started line's
   last-position field.
6. The queue bar is created on first position, and cleared on transfer start,
   queue expiry, rejection, cancellation, and channel close.
7. Existing queue policy tests (`max_queue_length`, both deadlines) still pass
   with their current warning text.

`progress.rs`:

1. `queue_bars_created()` increments without moving `created_bars()`, and the
   reverse.
2. Queue-bar labels strip control characters.

`report.rs` and `notifier.rs`:

1. The summary renders the library destination and the `(kept in staging)`
   form.
2. The notification message carries the destination; `title` and `type` are
   unchanged. Mock HTTP, as the existing notifier tests do.

## Acceptance criteria

1. Every completed album produces exactly one `INFO` completion line naming its
   final album folder.
2. An album left in staging is reported as the destination, marked as staging.
3. No line implies that a staging path is the final library location.
4. A queued download produces at most two `INFO` lines for its queue phase, at
   any queue depth, and never goes silent: the queued line is always emitted
   within the grace.
5. The queue position appears on the queued line when the peer reports it within
   the grace, and on the started line otherwise.
6. Every position change is visible at `DEBUG` and in the interactive spinner,
   and produces no `INFO` line.
7. The destination reaches the run summary and the notification message.
8. No queue policy, admission rule, or timeout semantic changes.
9. `created_bars()` and the "no transfer bar before the transfer starts" test
   are unaffected.

## External contracts

- **Log lines** (new):
  - `Download queued: {basename} from {username}`, with a position suffix
    appended when the peer reported one inside the grace;
  - `Download started: {basename} from {username} after {wait} queued`, with
    `(last position {p})` appended when a position was seen, or
    `Download started: {basename} from {username} immediately (free slot)` when
    the transfer never queued;
  - `Download staged: {basename} -> {path}`;
  - `Completed: {artist} - {album} ({n} tracks) -> {path}`.
- **Log lines** (`DEBUG` only): `Organized: {src} -> {dest}` and one line per
  position change.
- **Notification payload** — shape unchanged; the message gains the destination
  appended after the track count.
- **`Library` API** — `AlbumOutcome::Downloaded` gains a required
  `destination: DownloadDestination` field;
  `organizer::copy_to_library` and `organizer::place_into_library` return
  `LibraryWriteOutcome`; `notifier::notify_success` gains a destination
  parameter.
- **Vendored client** — unchanged. The queue-position telemetry already exists
  and is only consumed.

## Risks

- Adding a field to `AlbumOutcome::Downloaded` is a public-API change on the
  library target and touches roughly twenty existing test assertions — all
  mechanically, via a test helper.
- A process killed inside the 5-second grace loses the queued line. Accepted:
  the vendored client already prints its own `[INFO] [client] Downloading …`
  line at request time.

## References

- `src/download.rs` — `download_once`, the 200 ms poll loop, and the queue
  deadline handling; `src/progress.rs` — `ProgressDisplay`.
- `src/organizer.rs` — `organize_file`, `copy_to_library`, `place_into_library`,
  `copy_into_library`, `sanitize_component`, `disc_subdir`.
- `src/runner.rs` — `finish_library_write`, the four destination paths,
  `LibraryTarget`.
- `src/report.rs`, `src/notifier.rs` — outcome shape, summary, payload.
- `docs/agent/specs/2026-09-09-queue-aware-download-timeouts-design.md` — queue
  state machine, telemetry cadence, and the observability precedent for putting
  the position in a queue message.
- `docs/agent/specs/2026-09-15-discover-library-placement-design.md` — the
  placement path, `ArtistComponent::Verbatim`, and the kept-file semantics this
  design must report accurately.
