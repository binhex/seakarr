# Queue-aware download timeouts

## Problem

`DownloadConfig` already exposes `max_queue_length`,
`max_start_time_secs`, and `max_queue_time_secs`, but these settings are
currently parsed and unused. `download_once` starts the generic
`timeout_secs` deadline immediately after enqueueing a file. A peer that
keeps the file queued longer than that deadline is therefore abandoned before
reaching the queue head.

## Goal

Implement queue-aware download handling so queued files can wait according to
the configured queue policy, while transfer inactivity remains separately
controlled after the file actually starts transferring.

## Scope

In scope:

- queue-position propagation through the vendored Soulseek client and the
  `SoulseekClient` bridge;
- `max_queue_length` filtering and enforcement;
- `max_queue_time_secs` and `max_start_time_secs` enforcement;
- separation of queue waiting from post-start transfer inactivity;
- queue-specific fallback and regression tests;
- README/config documentation for the now-active settings.

Out of scope:

- `max_download_time_mins`, `min_filtered_users`, and `skip_retry_hours`;
- unrelated changes to peer ranking or album selection;
- changing the existing per-file retry policy for failures after transfer
  start.

## Decisions

### Queue length

- `max_queue_length: 0` preserves free-slot-only candidate filtering. Once a
  candidate is admitted because the search advertised a free slot, later
  positive telemetry does not retroactively reject it; queue timers still
  apply.
- A positive `max_queue_length` permits a zero-slot peer only when a real
  queue position is observed and is within the configured bound.
- An unknown queue position is never treated as within the bound. If a
  positive queue cap is active and no valid position arrives before the
  queued attempt must proceed or expire, the candidate fails closed.
- Positive queue positions are one-based; position `1` represents the queue
  head. Wire position `0` means the peer has no current queue entry (including
  after handing the file over), so it is normalized to an unknown,
  non-actionable position and never proves a zero-slot candidate is in-bound.

### Timers

Each file has an enqueue timestamp and, once applicable, a queue-head
timestamp and first-transfer timestamp:

- `max_queue_time_secs` starts at enqueue and limits the total time until the
  first real `InProgress` status. `0` disables this limit.
- `max_start_time_secs` starts when queue position `1` is observed and limits
  the time from queue head to the first real `InProgress` status. `0` disables
  this limit.
- The earlier enabled queue deadline wins.
- `timeout_secs` starts only at the first real `InProgress` status and is reset
  only by later `InProgress` updates.
- A `Queued` or pre-start `Paused` status consumes queue time and never starts
  or resets the transfer inactivity timer.
- A post-start `Paused` status counts as inactivity and does not reset the
  transfer deadline.

### Failure and retry behavior

Queue expiry and failure to obtain a valid queue position produce a distinct
queue-timeout error. The error is treated as peer-specific and is not retried
against the same peer. `download_album` proceeds to the next ranked candidate.
Cancellation remains higher priority than timeout and continues to clean up
staging. Transfer failures, quality rejections, and post-start inactivity
retain their existing retry/fallback behavior.

## Design

### Vendor status propagation

The vendored client already stores queue positions when it receives
`PlaceInQueueUpdate`, but the download status receiver does not receive that
information. Extend the vendor status path so a queue-position update is
forwarded to the download's status channel, while preserving the internal
queue-position store.

The application domain status must distinguish an actionable positive queue
position from no position. The vendor normalizes wire position `0` to `None`,
the same non-actionable state used before a position is known. The bridge must
preserve queued status as queued and must not map queued or paused states to
transfer progress before the first real `InProgress` event.

The downloader must actively request queue-position telemetry rather than rely
on unsolicited peer messages. After sending `QueueUpload` (peer code 43), the
vendor peer actor sends `PlaceInQueueRequest` (peer code 51) immediately and
every 300 seconds while that file remains queued. This telemetry is
policy-independent because the protocol actor does not own application config,
advertised free-slot snapshots can become stale, and position `1` drives the
queue-head deadline for any wait. Polling stops when transfer starts, the
upload fails, the download is removed or cancelled, or the peer actor
disconnects. Transfer-request handling defers its stop until the client
operation selects the newest matching queued attempt, migrates the wire token,
and sends an attempt-scoped stop before replying. Repeated
`PlaceInQueueResponse` messages (peer code 44) may update a still-queued
download, but a late response must not regress an `InProgress`, `Paused`,
completed, or failed vendor status back to queued.

Peer code 44 identifies a download only by filename and carries no transfer
token. When old and new same-user/same-filename records overlap, the vendor
applies a response to the newest matching record that is still queued, instead
of letting a terminal record consume it. A response delayed across retry
generations still cannot be identified perfectly and may update the newer
attempt; this protocol limitation remains fail-closed at the application
boundary.

Internal lifecycle cleanup is stricter: every queued download carries a stable
attempt ID that does not change when its Soulseek wire transfer token changes.
Queue polling schedules and explicit stop/removal operations use that attempt
ID, so delayed cleanup from one attempt cannot delete, pause, or stop a newer
retry of the same user and filename, including when `retry_delay_secs` is `0`.
The ID retains the local u32 token counter's 2^31 wrap interval. That practical
single-session limit is accepted; eliminating it would require a second wider
identity model for a collision that cannot occur in realistic operation.

### Filtering and download flow

The queue policy is passed into candidate filtering without duplicating the
configuration key. With a positive queue cap, zero-slot candidates remain
eligible for ranking; their queue position is validated when the download is
actually queued. With a zero cap, the existing filter rejects zero-slot
candidates before download.

`download_once` becomes a small state machine:

1. Queue the file and record enqueue time.
2. Consume queued position updates, rejecting an out-of-bound position and
   recording when position `1` is reached.
3. Enforce the total queue and queue-head deadlines while no transfer has
   started.
4. On the first real `InProgress`, start transfer timing and the existing
   inactivity deadline.
5. Reset only the transfer inactivity deadline on subsequent `InProgress`
   updates.
6. On queue expiry, cancel/drain the vendor transfer, return the queue-specific
   error, and let the candidate loop select another peer.

### Observability

Queue expiry logs identify the file, peer, queue position when known, and the
specific queue limit that expired. Transfer timeout logs retain the existing
post-start wording. This lets operators distinguish a long remote queue from a
stalled transfer.

## Backward compatibility

No new YAML keys or database migrations are required. Existing configurations
continue to parse because the three queue settings already exist. The default
`max_queue_length: 0` behavior is unchanged at candidate admission: search
still requires an advertised free slot. A candidate already
admitted on that basis is not retroactively rejected solely because later
telemetry reports a positive queue position. Existing default queue timers
become active as documented, which intentionally changes queued downloads from
being governed by the generic transfer timeout to being governed by the queue
limits first.

Existing free-slot candidates that begin transferring immediately continue to
use `timeout_secs` exactly as before. Queue expiry is a new, explicit failure
reason and follows candidate fallback rather than same-peer retry.

## Testing

Add focused tests for:

1. A queued file that reaches queue head and starts before both queue deadlines.
2. Expiry of `max_queue_time_secs` while the file remains queued.
3. Expiry of `max_start_time_secs` after queue position `1` is observed.
4. A queued/pre-start paused status not starting the transfer timer.
5. A post-start paused status counting toward transfer inactivity.
6. A queue-expired candidate falling back to the next peer without retrying the
   same peer.
7. `max_queue_length: 0` rejecting zero-slot candidates.
8. A positive queue cap accepting an observed in-bound position, rejecting an
   out-of-bound position, and failing closed when the position remains unknown.
9. Wire position `0` becoming non-actionable without proving a zero-slot
   candidate eligible, and cap-zero preserving a candidate admitted with a
   free slot despite later positive telemetry.
10. Vendor queue-position updates reaching the application status channel.
11. The vendor sending an immediate and periodic queue-position request, and
    stopping those requests when the queued download leaves that lifecycle.
12. Late queue-position responses not regressing a non-queued vendor status.
13. Delayed cleanup from an old attempt preserving a newer same-file retry's
    download record and queue-position polling schedule.
14. Overlapping same-file records routing tokenless queue telemetry to the
    newest record that is still queued.
15. Existing transfer timeout, cancellation, quality verification, retry,
    free-slot, and integration tests remaining green.

## Acceptance criteria

- A peer can wait in a permitted queue longer than `timeout_secs` without being
  abandoned before reaching the queue head.
- A peer that exceeds either configured queue deadline is abandoned promptly.
- Once transfer starts, the existing inactivity timeout remains effective.
- Queue limits and errors are visible in logs and candidate fallback behavior.
- Permitted queued downloads actively request position telemetry immediately
  and every five minutes while queued.
- All existing Rust, integration, vendor, formatting, lint, and pre-commit
  checks pass.
