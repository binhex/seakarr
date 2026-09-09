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

- `max_queue_length: 0` preserves the current free-slot-only behavior.
- A positive `max_queue_length` permits a zero-slot peer only when a real
  queue position is observed and is within the configured bound.
- An unknown queue position is never treated as within the bound. If a
  positive queue cap is active and no valid position arrives before the
  queued attempt must proceed or expire, the candidate fails closed.
- Queue position is one-based; position `1` represents the queue head.

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

The application domain status must distinguish an unknown queue position from
a real position. The bridge must preserve queued status as queued and must not
map queued or paused states to transfer progress before the first real
`InProgress` event.

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
`max_queue_length: 0` behavior is unchanged. Existing default queue timers
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
9. Vendor queue-position updates reaching the application status channel.
10. Existing transfer timeout, cancellation, quality verification, retry,
    free-slot, and integration tests remaining green.

## Acceptance criteria

- A peer can wait in a permitted queue longer than `timeout_secs` without being
  abandoned before reaching the queue head.
- A peer that exceeds either configured queue deadline is abandoned promptly.
- Once transfer starts, the existing inactivity timeout remains effective.
- Queue limits and errors are visible in logs and candidate fallback behavior.
- All existing Rust, integration, vendor, formatting, lint, and pre-commit
  checks pass.
