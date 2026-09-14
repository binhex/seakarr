# One-time peer private-message delivery

## Problem

Soulseek can replay previously delivered private messages on every reconnect.
The server message includes a `new_message` flag and a server-assigned message
ID used by acknowledgement code 23.

The current server code-22 handler correctly logs and forwards every delivery,
but sends an acknowledgement only when `new_message` is true. For a
`new_message: false` offline re-delivery, the display and client buffering are
required; the defect is that it is never acknowledged after that delivery.

The same peer message consequently appears on every seakarr run. The reported
example has message ID `292923` and `new_message: false`.

## Goal

Display and expose every private message delivered by the server, then clear it
server-side so it is not delivered again on later runs. A `new_message: false`
message can be the client's first delivery after it was queued while offline,
so it must not be suppressed.

## Scope

In scope:

- server code-22 private-message handling;
- preservation of the existing `new_message` flag on `UserMessage`;
- acknowledgement of every successfully parsed message ID;
- existing INFO display and client API buffering for both flag values;
- delivery of messages marked false before acknowledging their server backlog entry;
- DEBUG-only acknowledgement failure diagnostics;
- handler-level tests for event ordering, payload, and acknowledgement bytes.

Out of scope:

- SQLite or in-memory message-ID deduplication;
- configuration keys;
- message history or retention UI;
- automatic replies or peer blocking;
- acknowledgement retries inside the message handler;
- changes to chat-room messages;
- changes to the public `UserMessage`, `MessageFactory`, or client API.

## Protocol evidence

Nicotine+'s reverse-engineered Soulseek protocol documentation defines the final
code-22 boolean as: "True if message is new, false if message is re-sent (e.g.
if recipient was offline)." It also states that code 23 confirms receipt and
that the server keeps sending the message until it is acknowledged.

Reference: <https://nicotine-plus.org/doc/SLSKPROTOCOL.html#server-code-22>

## Existing components

The design reuses existing protocol and client components:

- `MessageUser` parses server message code 22;
- `UserMessage` carries ID, timestamp, username, body, and `new_message`;
- `MessageFactory::build_message_acked(id)` builds server code 23;
- `ServerMessage::PrivateMessageReceived` forwards a private message into the
  client loop;
- `ServerMessage::SendMessage` sends the acknowledgement to the server actor;
- `Client::take_private_messages()` drains messages buffered since its previous
  call.

Only `vendor/soulseek-rs-lib/src/message/server/message_user.rs` requires a
behavior change. Tests belong in that same module because they exercise the
handler's private channel contract directly.

## Behavioral contract

### New delivery

When `new_message` is true:

1. Parse the complete message.
2. Construct `UserMessage` with the original fields.
3. Emit the existing INFO display containing the sender and body.
4. Attempt to send `ServerMessage::PrivateMessageReceived(user_message)`.
5. Attempt to send `ServerMessage::SendMessage` containing a code-23
   acknowledgement for the original ID.

The delivery event is queued before the acknowledgement. This prevents a
successfully acknowledged message from being lost before local delivery.

The user selected clear-after-log semantics: if forwarding into the client API
fails after the INFO display, the handler still attempts the acknowledgement.
The log display counts as delivery for clearing purposes.

### Resent offline delivery

When `new_message` is false:

1. Parse the complete message.
2. Construct and log the `UserMessage`, preserving `is_new() == false`.
3. Send `PrivateMessageReceived` so the first local delivery is not lost.
4. Send code-23 acknowledgement for the original ID.

Protocol documentation defines false as a re-sent message, for example one sent
while the recipient was offline. It can therefore be the first delivery this
client process has seen. The acknowledgement clears the server backlog entry so
it is not delivered again on a later run.

## Ordering

A single `std::sync::mpsc::Sender<ServerMessage>` queues handler output. The
required observable order is:

- `new_message: true`: `PrivateMessageReceived`, then acknowledgement;
- `new_message: false`: `PrivateMessageReceived`, then acknowledgement.

The implementation must not acknowledge a new message before attempting local
delivery.

## Failure handling

### Client API forwarding failure

Add an ERROR diagnostic when `PrivateMessageReceived` cannot be queued. Use a
MessageUser-specific prefix so it is distinguishable from the actor's later
forwarding diagnostic, and do not include the message body in that error.
Proceed to the acknowledgement attempt because the INFO message was already
displayed.

### Acknowledgement enqueue failure

Emit DEBUG with:

- message ID;
- username;
- channel error.

Do not include the private-message body. No retry is attempted inside the
handler because a disconnected server-actor channel indicates session teardown;
reconnect logic owns later recovery.

### Parsing behavior

Malformed or truncated packet handling remains unchanged. Existing readers
substitute zero or empty values when fields are unavailable, so a severely
truncated code-22 frame can still reach acknowledgement with message ID 0. This
low-risk server-input edge is accepted in this change rather than adding a new
packet validator. The feature does not introduce persistence or retry semantics.

## Data handling and privacy

The private-message body remains visible only in the existing INFO message and
in the `UserMessage` delivered to API consumers. This applies to both flag
values because a false flag can represent the first local delivery after the
client was offline.

Acknowledgement diagnostics identify the message by ID and username only. They
must not repeat the body at DEBUG, WARN, or ERROR.

No private-message data is written to SQLite or a new file.

Message visibility in seakarr logs requires `logging.level: INFO` or a more
verbose level. At `WARN` or `ERROR`, the message is still delivered to the
client API buffer and acknowledged, but seakarr itself does not currently drain
that buffer; users selecting those log levels therefore accept that private
messages are cleared without appearing in the process log.

## Test design

Tests construct actual code-22 payloads with `Message` write helpers, call
`MessageUser::handle`, and inspect a `std::sync::mpsc` receiver.

### New-message test

For `new_message: true`, assert:

1. the first received event is `PrivateMessageReceived`;
2. its `UserMessage` preserves ID, timestamp, username, body, and true flag;
3. the second event is `SendMessage`;
4. the sent message bytes identify server code 23 and carry the original ID;
5. no third event exists.

### Resent-offline-message test

For `new_message: false`, assert:

1. the first received event is `PrivateMessageReceived`;
2. its `UserMessage` preserves every field and `is_new() == false`;
3. the second event is `SendMessage`;
4. its bytes identify code 23 and the original ID;
5. no third event exists.

The acknowledgement assertion is the direct regression for the reported
repeating message. The delivery assertion prevents the fix from discarding a
message that the client is seeing for the first time after being offline.

### Disconnected-channel and diagnostic-privacy test

The crate logger cannot be read back in tests: `LOG_LEVEL` is a private
`static mut` that defaults to `Warn`, and the buffered output has no read
accessor. The diagnostic guarantee is therefore pinned two ways.

First, behaviourally: drop the receiver and invoke the handler for both true and
false flags, then assert the handler returns without panic, proving
acknowledgement enqueue failure is non-fatal.

Second, structurally: the acknowledgement failure text is built by a pure
`acknowledgement_failure(id, username)` helper. The test asserts that text
contains the message ID and the sender name and does **not** contain the message
body. Because the handler has exactly one acknowledgement-failure `debug!` call
site, which logs that helper's output plus the channel error, the body-free
guarantee follows from the helper assertion.

Both flag values follow the same local-delivery-then-acknowledgement order. The
flag is preserved on `UserMessage` for API consumers; it no longer decides
whether the message is displayed.

### Existing tests

Retain and run:

- `MessageFactory::build_message_acked` byte-shape test;
- client private-message buffering and draining tests;
- end-to-end private-message delivery test;
- complete workspace test suite.

No live external Soulseek service is required for the new unit tests.

## Documentation

No README update is required. This fixes protocol cleanup behavior without
adding user-facing configuration, commands, API methods, or workflow changes.

In-code comments in `message_user.rs` must state:

- new messages are delivered before acknowledgement;
- new messages and offline re-deliveries are both acknowledged after display;
- acknowledgement failures use a stable body-free DEBUG message naming only
  the message ID, username, and channel error.

## Acceptance criteria

- A new private message is logged once at INFO.
- A new private message is buffered once for `take_private_messages()`.
- The new-message delivery event precedes its acknowledgement event.
- Every parsed new message is acknowledged after the local delivery attempt.
- A `new_message: false` offline delivery is logged and buffered once.
- Its `UserMessage` preserves `is_new() == false`.
- It is acknowledged with code 23 and the original message ID, preventing later
  redelivery.
- API-forwarding failure does not prevent an acknowledgement attempt.
- Acknowledgement failure is DEBUG-only and does not include the body.
- No local message-ID store, configuration key, schema change, or public API
  change is introduced.
- Existing private-message and workspace tests remain green.

## Residual risk

If the server session is lost after local display but before the queued
acknowledgement reaches the server, reconnect can deliver and display the same
message again within one seakarr run. Local ID deduplication is deliberately out
of scope; successful code-23 acknowledgement remains the mechanism that stops
later redelivery.
