<!-- markdownlint-disable MD013 -->
# Queue position refresh (interactive queue bar)

## Problem

The interactive queue bar exists, updates in place, and shows a position that never
changes. The operator sees a queue depth as a fixed number and cannot tell whether the
line is advancing, stalled, or dead:

```text
  ⠇ 01-04 Groovin High.flac - qiman88 queue #20
```

The render path is not at fault. `QueueWait::observe` (`src/download.rs`) calls
`ProgressDisplay::update_queue_bar` (`src/progress.rs`) for every position it is handed,
and `observe` filters only zero and absent positions. A number that does not move means no
new position ever arrived.

### The position is requested once per five minutes

The only later positions come from a `PlaceInQueueRequest` sent by the vendored peer actor.
`start_queue_position_requests` (`vendor/soulseek-rs-lib/src/actor/peer_actor.rs`) sends one
immediately when the download is queued, then arms `next_request` with
`QUEUE_POSITION_REQUEST_INTERVAL`, which is `Duration::from_mins(5)`.

The receive path works. When a peer volunteers a `PlaceInQueueResponse` on its own, the
position reaches the bar: `handle_place_in_queue_response` forwards
`ClientOperation::PlaceInQueueUpdate`, the client operations loop calls
`DownloadStore::update_queue_position`, the store publishes
`DownloadStatus::Queued { queue_position }` on the download's own channel, and seakarr's
bridge maps it to `QueueWait::observe`.

The gap is therefore purely cadence. seakarr's queue limits bound a wait on the same order
as the refresh interval, so a wait usually ends before the actor's second request.

### Evidence

Measured over the operator's own log (`logs/seakarr.log`, 2026-09-18 to 2026-09-22),
pairing each `Download queued: ... - position N` with the `Download started: ... (last
position M)` that follows it:

- 2352 paired waits, and **exactly one** showed a different position later
  (59 → 27 after 2 m 20 s — an unsolicited peer response, which is what proves the receive
  path works).
- Of the 446 waits that started at position 10 or deeper, that same single wait is the only
  one that ever changed.
- The longest deep waits were 299 s, 298 s and 290 s at positions 13, 20 and 20 — cut off by
  the deployment's `max_queue_time_secs=300`, which is one refresh interval.

A wait capped at 300 s with a 300 s re-ask receives one position report by construction.

### The queue-head deadline is collateral damage

`max_start_time_secs` (default 120 s, `src/config.rs`) arms only when a report says
position 1 (`QueueWait::observe` sets `head_at`). At a five-minute cadence that report
usually never arrives, so the head deadline is usually unarmed and the limit silently does
nothing.

## Goal

The position rendered in the existing queue bar tracks the peer's real queue closely enough
to be useful, without adding any `INFO` or `WARN` line, and without changing the label's
shape.

## Scope

In scope:

- an on-demand "ask this peer for the position now" capability in the vendored crate,
  routed through the peer actor that already owns the protocol request;
- a 30-second refresh cadence owned by seakarr, driven while an attempt is queued;
- the `DEBUG` record for each ask;
- the queue-head deadline becoming effective as a consequence of fresher positions;
- tests for the cadence, the label updates, and the stop conditions;
- README and design-doc updates.

Out of scope:

- any new `INFO` or `WARN` line, and any change to the existing queued/started/timeout
  wording;
- a new configuration key (the cadence is a named constant);
- removing or re-arming the vendored crate's five-minute timer;
- changing the bar's label: no elapsed wait, no movement arrow, no ETA;
- predicting time to the queue head, or ranking candidates by queue depth;
- changing `max_queue_length`, `max_queue_time_secs` or `max_start_time_secs` semantics.

## Decisions

1. **seakarr owns the cadence.** A vendor-constant change would make one protocol actor
   responsible for a policy the queue limits already express, and a new `ClientSettings`
   field would add public configuration surface for a value with one sensible setting. The
   cadence belongs beside the queue-wait state machine that already bounds the wait.
2. **The refresh interval is 30 seconds**, a named constant. The operator's queue cap is
   300 s, so this yields ten asks per full wait; the single observed advance moved about 14
   positions per minute, so the number typically moves several places per refresh.
3. **The label is unchanged** — position only. Elapsed wait time and a movement arrow were
   both considered and rejected: the request is that the number updates, and the spinner
   glyph already signals liveness between refreshes.
4. **The request is actor-mediated.** A new `PeerMessage::RequestQueuePosition` handled by
   the peer actor, plus a public `Client::request_place_in_queue`. The generic
   `queue_peer_message` path was rejected because `take_peer_messages` is drained only in
   the `PeerConnected` arm (`vendor/soulseek-rs-lib/src/client/operations.rs`), so a request
   raised during an established queue wait can sit unsent — the display would stay frozen
   in exactly the case being fixed.
5. **The vendored crate's five-minute timer is left alone.** It stays as a floor for any
   consumer that does not ask, and a redundant ask costs one ignored response. Re-arming
   `next_request` from the consumer side was rejected as coupling seakarr's cadence into the
   actor's bookkeeping for no behavioural gain.
6. **The refresh stops when the transfer starts.** Once bytes arrive the position no longer
   means anything, and the queue bar is already cleared.
7. **The queue-head deadline repair is in scope.** A fresh position-1 report arms
   `max_start_time_secs` within about 30 s instead of almost never. This changes outcomes for
   waits that previously sat at position 1 indefinitely; that is the intended repair and is
   recorded here as a behaviour change, not a display-only change.
8. **`SoulseekClient::request_queue_position` is a required method**, not a defaulted one, so
   a production client cannot silently skip the refresh. The seven implementations are
   one-liners.
9. **One `DEBUG` record per ask**, matching the existing `Queue position for X from Y: N`
   record, so a frozen number can be diagnosed as "we asked and the peer did not answer"
   instead of guessed.
10. **No configuration key.** If the cadence ever needs tuning, promoting the constant to a
    config key is a one-line change plus a README row.

## Architecture

### `vendor/soulseek-rs-lib/src/actor/peer_actor.rs`

`PeerMessage` gains a variant, a sibling of `QueueUpload` and `StopQueuePositionRequests`:

```rust
/// Ask this peer for our current position in its upload queue, on demand.
/// The periodic timer is unaffected: this is the caller's cadence, not ours.
RequestQueuePosition { filename: String },
```

The dispatch gains one arm, and the actor gains one method:

```rust
fn request_queue_position(&mut self, filename: &str) {
    self.send_message(MessageFactory::build_place_in_queue_request(filename));
}
```

No bookkeeping change. Sending does not depend on a `queue_position_requests` entry (the
entry exists to drive the timer, not to gate a send), and `send_message` already handles a
missing stream by logging and returning. A peer that is not currently polling therefore
still answers an on-demand ask.

### `vendor/soulseek-rs-lib/src/client/mod.rs`

A public method, shaped like the room-send path and documented in the `cancel_upload`
style:

```rust
/// Ask `username` where our queued copy of `filename` currently sits.
///
/// Returns whether a live peer actor accepted the request. `false` is not an
/// error: the download's own queue limits still bound the wait.
#[must_use = "returns whether the request reached a peer actor"]
pub fn request_place_in_queue(&self, username: &str, filename: &str) -> bool
```

It clones the peer registry out of the context and calls
`PeerRegistry::send_to_peer(username, PeerMessage::RequestQueuePosition { filename })`,
returning whether that succeeded.

### `src/client.rs`

`SoulseekClient` gains a synchronous method (no I/O, so no `async`):

```rust
/// Ask a peer where our queued copy of `filename` sits. Best-effort: a peer
/// with no live control connection returns false and the wait is unaffected.
fn request_queue_position(&self, username: &str, filename: &str) -> bool;
```

`RealClient` delegates to the crate. `MockClient` records the call and returns a canned
value so tests can assert the cadence. The remaining doubles
(`ScriptedClient`, `SelectiveFailClient`, `ControllableClient`, `RetryClient` in
`src/download.rs`; `CancelAfterFirstSearchClient` in `src/runner.rs`) each get a one-line
implementation.

The **peer's** path is what goes on the wire — `file.name`, not the display `basename` — because
the response is matched against `Download.filename` in the store. A basename would never
match and the position would be dropped.

### `src/download.rs`

A named constant beside the other download constants:

```rust
/// How often a queued attempt re-asks its peer for a position.
///
/// The vendored crate's own interval is five minutes, which outlives the queue
/// cap this deployment runs (`max_queue_time_secs=300`), so a wait used to see
/// one position report and a frozen bar. Fresh positions also arm the
/// `max_start_time_secs` head deadline, which needs a position-1 report.
const QUEUE_POSITION_REFRESH: Duration = Duration::from_secs(30);
```

`QueueWait` gains a field and a due-check mirroring `notice_is_due`:

```rust
/// When the next position ask is due.
next_position_request: tokio::time::Instant,

/// Whether a position ask is due, advancing the deadline when it is.
fn position_request_is_due(&mut self, now: tokio::time::Instant) -> bool {
    if now < self.next_position_request {
        return false;
    }
    self.next_position_request = now + QUEUE_POSITION_REFRESH;
    true
}
```

`new` seeds the field with `enqueued_at + QUEUE_POSITION_REFRESH`: the vendored crate
already asks immediately at queue entry, so seakarr's first ask is a refresh, not a
duplicate.

The check goes at the **top of the poll loop**, immediately after the notice check, for the
reason that check's own comment gives: a peer answering position-0 more often than the poll
window keeps the poll alive, so a time-driven check buried in the timeout arm can be
starved. The loop already wakes at least every `STATUS_POLL_INTERVAL` (200 ms), so an ask is
never late by more than one poll window.

```rust
if queue.position_request_is_due(now) {
    tracing::debug!("Queue position request for {basename} from {username}");
    client.request_queue_position(username, &file.name);
}
```

Placement at the loop top gives the stop conditions for free: the transfer starting, a queue
deadline expiring, a rejection verdict, or a cancellation all leave the loop, so no ask can
outlive the wait it belongs to.

### `src/progress.rs`

Unchanged. `update_queue_bar`, `queue_label` and the `queue_bars_updated` counter already
express everything this design needs.

## Data flow

```text
poll loop top (every <=200 ms)
  ├─ queue.position_request_is_due(now) ─► client.request_queue_position(username, file.name)
  │                                            └─ PeerRegistry::send_to_peer
  │                                                 └─ actor: PlaceInQueueRequest (code 51)
  └─ peer answers (code 44)
        └─ actor: handle_place_in_queue_response
              └─ ClientOperation::PlaceInQueueUpdate
                    └─ DownloadStore::update_queue_position
                          └─ DownloadStatus::Queued { queue_position }
                                └─ bridge -> QueueWait::observe
                                      └─ ProgressDisplay::update_queue_bar  (label re-rendered in place)
```

## Error handling

- **No live peer actor** (dropped peer, no registry entry): the call returns `false`, nothing
  is logged as an error, and the wait continues under its queue limits.
- **Peer answers with the same position**: the bar re-renders identical text and no position
  `DEBUG` line is emitted, because `observe` only logs a change. An unchanged number is not
  evidence that the refresh stopped.
- **Peer answers position 0 or omits it**: filtered exactly as today — no bar update, no
  rejection.
- **A refreshed position exceeds `max_queue_length`**: the rejection now fires on fresher
  data, earlier than before. The message already names the last observed position.
- **A refreshed position is 1**: `head_at` is set, arming `max_start_time_secs`. This is the
  intended behaviour change described in Decision 7.
- **Cancellation**: the top-of-loop cancel check precedes the refresh, so Ctrl+C never waits
  on an ask, and a cancelled attempt sends nothing further.
- **A per-file retry**: builds a new `QueueWait`, so its 30-second window restarts. The
  vendored crate's immediate ask at queue entry still supplies the first position.

## Backward compatibility

- No log line is added, removed, reworded or re-levelled at `INFO` or above.
- No configuration key, no CLI flag, no schema change, no database change.
- The vendored crate's five-minute timer and its `queue_position_requests` bookkeeping are
  untouched, so any other consumer of the crate keeps today's behaviour exactly.
- `Client::request_place_in_queue` and `PeerMessage::RequestQueuePosition` are additive.
- Behavioural change: waits that reach position 1 can now be cut off by
  `max_start_time_secs`, where previously that limit was usually unarmed. Operators running
  the default 120 s will see queue-head timeouts that previously did not happen.

## Deliberate limits

- A peer that keeps answering with an unchanged position is asked ten times over a
  five-minute wait; there is no backoff, because an unchanged position is a legitimate answer
  and the refresh is cheap.
- The design cannot distinguish "the peer ignored the ask" from "the queue is not moving".
  The `DEBUG` record shows that an ask was made; only the peer knows the rest.
- No time-to-head estimate. Positions are not comparable between peers without that peer's
  turnover rate, and this protocol does not expose one.

## Testing

Vendor (`cargo test -p soulseek-rs-lib`):

1. `RequestQueuePosition` puts exactly one `PlaceInQueueRequest` on the wire, following the
   existing actor tests that inspect the sent buffer.
2. It does not add a `queue_position_requests` entry and does not move an existing
   `next_request`, so the five-minute timer keeps its own schedule.
3. `Client::request_place_in_queue` returns `false` without panicking when no peer actor
   exists.

seakarr (`cargo test -p seakarr`), using the paused-time scripted client:

1. A wait that reports position 20 and then nothing, bounded by a queue limit longer than
   90 s, records asks at about 30 s, 60 s and 90 s — and none before 30 s.
2. Positions 20 → 12 → 3 produce three bar updates, asserting the rendered label text
   (`queue #20`, `queue #12`, `queue #3`), not merely the update counter.
3. The ask counter stops growing after `InProgress`, after a queue-timeout expiry, and after
   cancellation; the queue bar is still released on each of those paths.
4. The recorded filename is the peer's full path (`Music\Artist\Album\01.flac`), not the
   basename.
5. The per-ask `DEBUG` record is emitted at `DEBUG` level only (captured-level assertion in
   the style of the existing log-level tests).

## Acceptance criteria

- [ ] While an attempt is queued, the queue bar's number reflects a position reported after
      the first one, without any new `INFO`/`WARN` line.
- [ ] The bar's label text is unchanged apart from the number.
- [ ] A queued attempt with no further status reports issues an ask roughly every 30 s.
- [ ] No ask is issued after the transfer starts, after a queue timeout, or after a
      cancellation.
- [ ] The ask carries the peer's share path, not the display basename.
- [ ] `max_start_time_secs` can expire on a fresh position-1 report.
- [ ] `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test` and
      `pre-commit run --all-files` all pass; coverage stays at or above 95 % lines.

## Documentation

- README: state that a queued download's position is re-checked every 30 s while it waits,
  and that `max_start_time_secs` arms from a fresh position-1 report.
- This spec records the design.
- `docs/agent/specs/2026-09-17-download-log-visibility-design.md` states that the position
  arrives on a five-minute cadence; it gains an amendment note pointing here, because that
  is no longer the whole truth.

## Alternatives considered

- **Shorten the vendored crate's constant** (`QUEUE_POSITION_REQUEST_INTERVAL` 5 min → 30 s):
  a one-line change, but it puts a consumer policy inside a protocol actor, cannot be tuned
  per consumer, and needs a new seam to test.
- **Expose the interval as a `ClientSettings` field** with a new config key: explicit and
  testable, at the cost of public configuration surface for a value with one sensible
  setting.
- **Route the ask through `queue_peer_message`**: smallest vendor diff, but the drain happens
  only on a `PeerConnected` event, so the ask can sit unsent during a live queue wait.
- **Re-arm the actor's `next_request` on each consumer ask**: tidier timer state, but it
  couples seakarr's cadence into the actor's bookkeeping and the redundant ask is harmless.
- **Display-side only** (show elapsed wait or a movement arrow instead of chasing fresher
  numbers): rejected — the request is that the number updates.
