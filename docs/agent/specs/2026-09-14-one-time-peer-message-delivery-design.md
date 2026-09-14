# One-time peer private-message delivery

## Problem

Soulseek can replay previously delivered private messages on every reconnect.
The server message includes a `new_message` flag and a server-assigned message
ID used by acknowledgement code 23.

The current server code-22 handler logs and forwards every delivery, but sends
an acknowledgement only when `new_message` is true. A replay therefore follows
the wrong combination:

- `new_message: false`;
- logged again at INFO;
- forwarded into the client private-message buffer again;
- never acknowledged.

The same peer message consequently appears on every seakarr run. The reported
example has message ID `292923` and `new_message: false`.

## Goal

Display and expose a genuinely new peer private message once, then clear it
server-side. Silently clear replayed private messages without logging or
buffering them again.

## Scope

In scope:

- server code-22 private-message handling;
- branching on the existing `new_message` flag;
- acknowledgement of every successfully parsed message ID;
- existing INFO display and client API buffering for new messages;
- silent suppression of replayed messages;
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

### Replayed delivery

When `new_message` is false:

1. Parse the complete message.
2. Do not emit the INFO message.
3. Do not send `PrivateMessageReceived`.
4. Send only the code-23 acknowledgement for the original ID.

This silently clears the server backlog entry. It does not require local state
or recognition of a previously seen ID.

## Ordering

A single `std::sync::mpsc::Sender<ServerMessage>` queues handler output. The
required observable order is:

- new message: `PrivateMessageReceived`, then acknowledgement;
- replayed message: acknowledgement only.

The implementation must not acknowledge a new message before attempting local
delivery.

## Failure handling

### Client API forwarding failure

Retain the existing ERROR diagnostic when `PrivateMessageReceived` cannot be
queued. Do not include any new duplicate copy of the message body in that error.
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

Malformed or truncated packet handling remains unchanged. This feature does not
introduce message validation, persistence, or retry semantics.

## Data handling and privacy

The private-message body remains visible only in the existing INFO message and
in the new-message `UserMessage` delivered to API consumers. A replay produces
neither.

Acknowledgement diagnostics identify the message by ID and username only. They
must not repeat the body at DEBUG, WARN, or ERROR.

No private-message data is written to SQLite or a new file.

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

### Replay test

For `new_message: false`, assert:

1. the only received event is `SendMessage`;
2. its bytes identify code 23 and the original ID;
3. no `PrivateMessageReceived` event exists;
4. no second event exists.

The replay test is the direct regression for the reported repeating message.
With the current implementation it fails because the replay is forwarded and
no acknowledgement is sent.

### Disconnected-channel test

Drop the receiver, invoke the handler for both true and false flags, and assert
that the handler returns without panic. Logging capture may additionally prove
that acknowledgement failure is DEBUG-only and omits the body if the existing
logger test utilities support deterministic capture without adding a new
dependency.

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
- replayed messages are acknowledged without redisplay;
- acknowledgement failures are DEBUG-only and body-free.

## Acceptance criteria

- A new private message is logged once at INFO.
- A new private message is buffered once for `take_private_messages()`.
- The new-message delivery event precedes its acknowledgement event.
- Every parsed new message is acknowledged after the local delivery attempt.
- A replay (`new_message: false`) is not logged at INFO.
- A replay is not forwarded into the client private-message buffer.
- A replay is acknowledged with code 23 and the original message ID.
- API-forwarding failure does not prevent an acknowledgement attempt.
- Acknowledgement failure is DEBUG-only and does not include the body.
- No local message-ID store, configuration key, schema change, or public API
  change is introduced.
- Existing private-message and workspace tests remain green.
