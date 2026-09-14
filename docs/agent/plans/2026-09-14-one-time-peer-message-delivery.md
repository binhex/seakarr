# One-Time Peer Private Message Delivery Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use sub-agents (recommended)
> to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for
> tracking.

**Goal:** Display every Soulseek private message delivered to the client, then
acknowledge its server ID so it does not reappear on later seakarr runs.

**Architecture:** Server code 22 always logs and forwards the parsed
`UserMessage`, preserving the protocol's `new_message` flag, then sends code 23
to acknowledge the original ID. The flag does not suppress delivery because
protocol documentation says false can mean a message re-sent after the recipient
was offline, which may be this client's first visible delivery.

**Tech Stack:** Rust, `std::sync::mpsc`, the crate's `Message`,
`MessageFactory`, `ServerMessage`, `UserMessage`, and `crate::message::framed`
test helper.

**Spec:** `docs/agent/specs/2026-09-14-one-time-peer-message-delivery-design.md`

---

<!-- markdownlint-disable MD013 -->

## Scope Check

Single subsystem: one vendored server-message handler plus the approved design
document. No client API, actor, configuration, database, schema, chat-room, or
persistence changes.

## File Structure

| File | Responsibility | Change |
| --- | --- | --- |
| `vendor/soulseek-rs-lib/src/message/server/message_user.rs` | Parse server code 22, display/forward it, then acknowledge code 23 | Modify handler and add local unit tests |
| `docs/agent/specs/2026-09-14-one-time-peer-message-delivery-design.md` | Approved behavioral contract | Amend false-flag semantics using protocol evidence and document behavioural diagnostic testing |

## Protocol premise

Nicotine+'s reverse-engineered Soulseek protocol documentation defines the last
code-22 boolean as: "True if message is new, false if message is re-sent (e.g.
if recipient was offline)." Code 23 confirms receipt; without it the server
keeps sending the message.

Therefore `new_message: false` must still be displayed and buffered. It can be
the first delivery this client process sees after being offline. The fix for
repetition is acknowledgement after delivery, not suppression.

## Task 1: Amend the design contract

**Files:**

- Modify: `docs/agent/specs/2026-09-14-one-time-peer-message-delivery-design.md`

- [ ] **Step 1: Correct false-flag semantics**

Document that both flag values are logged and forwarded before acknowledgement.
A false flag is preserved as `UserMessage::is_new() == false` but does not
suppress display, because it can represent a first local delivery after the
client was offline.

- [ ] **Step 2: Reconcile diagnostic testing**

State that the crate logger has no readable test capture. Pin acknowledgement
failure through a no-panic channel test and a pure
`acknowledgement_failure(id, username)` helper whose output includes ID and
username but excludes the body.

- [ ] **Step 3: Lint the amended spec**

Run:

```bash
markdownlint --fix docs/agent/specs/2026-09-14-one-time-peer-message-delivery-design.md
markdownlint docs/agent/specs/2026-09-14-one-time-peer-message-delivery-design.md
```

Expected: exit 0, no output.

- [ ] **Step 4: Commit**

```bash
git add docs/agent/specs/2026-09-14-one-time-peer-message-delivery-design.md
git commit -m "docs: correct offline peer-message delivery semantics"
```

## Task 2: Write the failing protocol regression tests

**Files:**

- Modify: `vendor/soulseek-rs-lib/src/message/server/message_user.rs`

- [ ] **Step 1: Add framed-payload test helpers**

Append a `#[cfg(test)] mod tests` using the crate's existing `framed` helper:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::framed;

    const MESSAGE_ID: u32 = 292_923;
    const TIMESTAMP: u32 = 1_789_162_359;
    const SENDER: &str = "bossanovakid";
    const BODY: &str = "Please share your music.";

    fn message_payload(new_message: bool) -> Message {
        framed(|message| {
            message
                .write_int32(MESSAGE_ID)
                .write_int32(TIMESTAMP)
                .write_string(SENDER)
                .write_string(BODY)
                .write_bool(new_message);
        })
    }

    fn assert_acknowledges(acknowledgement: &Message) {
        let data = acknowledgement.get_data();
        assert_eq!(data.len(), 8, "acknowledgement is code plus id");
        assert_eq!(
            &data[0..4],
            23u32.to_le_bytes().as_slice(),
            "acknowledgement uses server code 23"
        );
        assert_eq!(
            &data[4..8],
            MESSAGE_ID.to_le_bytes().as_slice(),
            "acknowledgement carries the original id"
        );
    }
```

- [ ] **Step 2: Add the new-message ordering test**

```rust
    #[test]
    fn a_new_message_is_delivered_before_it_is_acknowledged() {
        let (sender, receiver) = std::sync::mpsc::channel();
        let mut message = message_payload(true);
        MessageUser.handle(&mut message, sender);

        match receiver.try_recv() {
            Ok(ServerMessage::PrivateMessageReceived(delivered)) => {
                assert_eq!(delivered.id(), MESSAGE_ID);
                assert_eq!(delivered.timestamp(), TIMESTAMP);
                assert_eq!(delivered.username(), SENDER);
                assert_eq!(delivered.message(), BODY);
                assert!(delivered.is_new());
            }
            other => panic!("first event must be the delivery, got {other:?}"),
        }
        match receiver.try_recv() {
            Ok(ServerMessage::SendMessage(ack)) => assert_acknowledges(&ack),
            other => panic!("second event must be the acknowledgement, got {other:?}"),
        }
        assert!(receiver.try_recv().is_err());
    }
```

- [ ] **Step 3: Add the resent-offline-message test**

```rust
    #[test]
    fn a_resent_offline_message_is_delivered_then_acknowledged() {
        let (sender, receiver) = std::sync::mpsc::channel();
        let mut message = message_payload(false);
        MessageUser.handle(&mut message, sender);

        match receiver.try_recv() {
            Ok(ServerMessage::PrivateMessageReceived(delivered)) => {
                assert_eq!(delivered.id(), MESSAGE_ID);
                assert_eq!(delivered.timestamp(), TIMESTAMP);
                assert_eq!(delivered.username(), SENDER);
                assert_eq!(delivered.message(), BODY);
                assert!(!delivered.is_new());
            }
            other => panic!("first event must deliver the offline message, got {other:?}"),
        }
        match receiver.try_recv() {
            Ok(ServerMessage::SendMessage(ack)) => assert_acknowledges(&ack),
            other => panic!("second event must acknowledge the offline message, got {other:?}"),
        }
        assert!(receiver.try_recv().is_err());
    }
}
```

- [ ] **Step 4: Run RED**

Run:

```bash
cargo test -p soulseek-rs-lib --lib message::server::message_user::tests -- --nocapture
```

Expected: two failures against the old handler:

- new message: `first event must be the delivery, got Ok(SendMessage(...))`;
- false/offline message: its first delivery succeeds, but the second event is
  absent because the old handler never sends code 23.

Do not weaken either ordering or acknowledgement assertion.

## Task 3: Deliver then acknowledge every code-22 message

**Files:**

- Modify: `vendor/soulseek-rs-lib/src/message/server/message_user.rs`

- [ ] **Step 1: Add the body-free failure helper**

```rust
fn acknowledgement_failure(id: u32, username: &str) -> String {
    format!("[MessageUser] could not acknowledge message {id} from {username}")
}
```

- [ ] **Step 2: Replace `MessageUser::handle`**

After parsing the existing fields, use this sequence:

```rust
        let user_message = UserMessage::new(
            id,
            timestamp,
            username.clone(),
            message_content,
            new_message,
        );

        info!("[MessageUser] User message received:{:?}", user_message);

        if let Err(error) = sender.send(ServerMessage::PrivateMessageReceived(user_message)) {
            error!(
                "[MessageUser] could not forward private message to client: {}",
                error
            );
        }

        if let Err(error) = sender.send(ServerMessage::SendMessage(
            MessageFactory::build_message_acked(id),
        )) {
            debug!("{}: {}", acknowledgement_failure(id, &username), error);
        }
```

Import macros with `use crate::{debug, error, info};`. Add comments explaining:

- both flag values are delivered because false can mean queued while offline;
- INFO display occurs before server-side clearing;
- acknowledgement is attempted even when client forwarding fails;
- the server repeats unacknowledged messages on later runs.

- [ ] **Step 3: Run GREEN**

Run:

```bash
cargo test -p soulseek-rs-lib --lib message::server::message_user::tests -- --nocapture
```

Expected: both tests pass.

- [ ] **Step 4: Run the vendored library suite**

Run: `cargo test -p soulseek-rs-lib --lib`
Expected: all tests pass with 0 failures.

- [ ] **Step 5: Commit tests and implementation together**

```bash
git add vendor/soulseek-rs-lib/src/message/server/message_user.rs
git commit -m "fix: acknowledge delivered peer messages so they stop reappearing"
```

## Task 4: Pin failure privacy and non-fatal behavior

**Files:**

- Modify: `vendor/soulseek-rs-lib/src/message/server/message_user.rs`

- [ ] **Step 1: Add the disconnected-channel test**

```rust
    #[test]
    fn a_disconnected_channel_is_non_fatal_and_the_diagnostic_omits_the_body() {
        for new_message in [true, false] {
            let (sender, receiver) = std::sync::mpsc::channel();
            drop(receiver);
            let mut message = message_payload(new_message);
            MessageUser.handle(&mut message, sender);
        }

        let failure = acknowledgement_failure(MESSAGE_ID, SENDER);
        assert!(failure.contains("292923"), "got: {failure}");
        assert!(failure.contains(SENDER), "got: {failure}");
        assert!(!failure.contains(BODY), "got: {failure}");
    }
```

- [ ] **Step 2: Run the exact test**

```bash
cargo test -p soulseek-rs-lib --lib message::server::message_user::tests::a_disconnected_channel_is_non_fatal_and_the_diagnostic_omits_the_body -- --exact
```

Expected: PASS. Add `--nocapture` to see two forwarding ERROR lines, one for
each flag value; acknowledgement DEBUG lines remain suppressed at the default
Warn level.

This test is a regression pin, not a RED step: it tests a helper and no-panic
property introduced in Task 3.

- [ ] **Step 3: Run the message module**

Run: `cargo test -p soulseek-rs-lib --lib message::`
Expected: all message handler, factory, and reader tests pass. Do not pass `-v`;
libtest rejects it.

- [ ] **Step 4: Commit**

```bash
git add vendor/soulseek-rs-lib/src/message/server/message_user.rs
git commit -m "test: cover peer-message acknowledgement failure and privacy"
```

## Task 5: Final verification

**Files:** none (verification only)

- [ ] **Step 1: Format and lint**

```bash
cargo fmt --all
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
```

Expected: exit 0, zero warnings.

- [ ] **Step 2: Run all tests**

Run: `cargo test --workspace`
Expected: all suites pass. Record whether
`a_private_message_is_delivered_between_users` actually ran: it self-skips when
neither `SOULSEEK_TEST_SERVER` nor a soulfind binary is available, and a skip
still reports as a passing test process.

- [ ] **Step 3: Run pre-commit**

Run: `pre-commit run --all-files`
Expected: all hooks pass.

- [ ] **Step 4: Inspect acceptance structure**

Run:

```bash
awk '/#\[cfg\(test\)\]/{exit} {print}' vendor/soulseek-rs-lib/src/message/server/message_user.rs \
  | rg -n 'acknowledgement_failure|PrivateMessageReceived|build_message_acked|User message received'
```

Expected: five production matches: one helper definition, one INFO display, one
client-forwarding call, one code-23 acknowledgement call, and one DEBUG failure
call that invokes the helper.

- [ ] **Step 5: Confirm scope**

With the chain's deferred-commit policy, run:

```bash
git diff --name-only HEAD
```

Expected: only the handler, amended spec, and this plan. If the plan's commit
steps were executed instead, use `git diff --name-only origin/main..HEAD` and
expect the same three files.

- [ ] **Step 6: Report**

Do not add a format commit unless Step 1 changes a file. The chain's finalising
step owns integration commits.

## Self-Review

### Spec coverage

| Requirement | Task |
| --- | --- |
| Show and buffer every delivered code-22 message | 2, 3 |
| Preserve true/false flag on `UserMessage` | 2, 3 |
| Deliver before acknowledgement | 2, 3 |
| Acknowledge every original message ID | 2, 3 |
| Stop later server redelivery | 2, 3 |
| Forward failure does not block acknowledgement | 3 (call-site structure), 5 (Step 4 inspection) |
| Ack failure DEBUG-only and body-free | 3, 4 |
| Ack failure non-fatal | 4 |
| No persistence/config/API change | 1-5 |
| Existing tests stay green | 3-5 |
| Protocol premise documented | 1 |
| In-code comments explain both flag values, ordering, and failure behavior | 3 |

### Placeholder scan

No placeholders or deferred implementation. Every code change is shown and
every command has an expected result.

### Type consistency

- `UserMessage::new` receives `(u32, u32, String, String, bool)`.
- Tests use real accessors: `id()`, `timestamp()`, `username()`, `message()`,
  `is_new()`.
- `ServerMessage::PrivateMessageReceived(UserMessage)` precedes
  `ServerMessage::SendMessage(Message)` for both flag values.
- Code 23 and original ID byte layout remain pinned by `assert_acknowledges`.
