# One-Time Peer Private Message Delivery Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use sub-agents (recommended)
> to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for
> tracking.

**Goal:** Stop server-replayed Soulseek private messages from appearing on every
seakarr run, while genuinely new messages are still shown once, buffered once,
and then cleared server-side.

**Architecture:** Branch on the existing `new_message` flag inside the vendored
server code-22 handler. A new message is logged and forwarded to the client
buffer first, then acknowledged; a replay is acknowledged silently with no log
and no client delivery. All parsing and message-factory behaviour is reused.

**Tech Stack:** Rust, `std::sync::mpsc`, the crate's own `Message`,
`MessageFactory`, `ServerMessage`, and `UserMessage` types, plus the
`crate::message::framed` test helper.

**Spec:** `docs/agent/specs/2026-09-14-one-time-peer-message-delivery-design.md`

---

<!-- markdownlint-disable MD013 -->

## Scope Check

Single subsystem: one handler in one vendored file, plus one documentation
amendment. It does not touch the client API, the actor, configuration, the
database, or chat-room messages. No decomposition into separate plans is needed.

## File Structure

| File | Responsibility | Change |
| --- | --- | --- |
| `vendor/soulseek-rs-lib/src/message/server/message_user.rs` | Decode server code 22, decide delivery vs replay, emit INFO and acknowledgement | Modify: reorder to deliver-then-acknowledge, acknowledge replays, add a pure body-free failure helper, add a `#[cfg(test)] mod tests` |
| `docs/agent/specs/2026-09-14-one-time-peer-message-delivery-design.md` | Approved design | Modify: replace the log-capture test clause with behavioural and pure-helper coverage |

No other file changes. The client API (`take_private_messages`), `MessageFactory`,
`UserMessage`, and `ServerMessage` are reused unchanged.

## Codebase facts this plan depends on

Verified in the working tree; do not re-derive or guess these:

- The handler currently acknowledges only when `new_message` is true, and it
  sends that acknowledgement **before** forwarding the message.
- `crate::message::framed(|m| { ... })` is a `#[cfg(test)]` helper that builds a
  message with the 8-byte frame header and parks the read pointer at the payload.
- `MessageFactory::build_message_acked(id)` produces exactly
  `[23, 0, 0, 0, <id as 4-byte LE>]`.
- `UserMessage` exposes `id()`, `timestamp()`, `username()`, `message()`, and
  `is_new()`.
- Test log output is not assertable: `LOG_LEVEL` is a private `static mut`
  defaulting to `Warn`, and `BUFFER` has no read accessor. Tests must not depend
  on log lines. (This is why Task 1 amends the spec.)
- The default `Warn` level also means the handler's `info!` and `debug!` calls
  are silent during tests. The `error!` call for a dropped client channel is
  visible, so Task 4's dropped-receiver test prints one ERROR line to stderr;
  that is expected and is not a failure.

---

### Task 1: Amend the spec's test clause to match the codebase

The approved spec requires capturing log output, which this crate's logger
cannot support. The user chose behavioural plus pure-helper coverage instead.

**Files:**

- Modify: `docs/agent/specs/2026-09-14-one-time-peer-message-delivery-design.md`

- [ ] **Step 1: Replace the disconnected-channel test subsection**

In `docs/agent/specs/2026-09-14-one-time-peer-message-delivery-design.md`,
replace this subsection:

```markdown
### Disconnected-channel test

Drop the receiver and invoke the handler for both true and false flags. Capture
logs with the project's existing test logger, without adding a dependency, and
assert:

1. the handler returns without panic;
2. the new-message case records the existing API-forwarding ERROR;
3. both cases record a DEBUG acknowledgement failure, proving the handler still
   attempted the acknowledgement after API forwarding failed;
4. the acknowledgement failure line contains the message ID and username;
5. the acknowledgement failure line does not contain the message body.
```

with:

```markdown
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

Suppressing the INFO line on replay is proven by the replay test's event stream
(no `PrivateMessageReceived`), together with the INFO call sitting inside the
`new_message` branch.
```

- [ ] **Step 2: Lint the documentation**

Run: `markdownlint --fix docs/agent/specs/2026-09-14-one-time-peer-message-delivery-design.md && markdownlint docs/agent/specs/2026-09-14-one-time-peer-message-delivery-design.md`
Expected: exit 0, no output.

- [ ] **Step 3: Confirm no stale log-capture wording remains**

Run: `rg -n 'Capture logs|records a DEBUG|log-capture' docs/agent/specs/2026-09-14-one-time-peer-message-delivery-design.md`
Expected: no output.

- [ ] **Step 4: Commit**

```bash
git add docs/agent/specs/2026-09-14-one-time-peer-message-delivery-design.md
git commit -m "docs: pin peer-message diagnostics with behavioural coverage"
```

---

### Task 2: Write the failing regression tests

The second test in this task is the direct regression for the reported bug: a
`new_message: false` replay must produce an acknowledgement and nothing else.

**Files:**

- Modify: `vendor/soulseek-rs-lib/src/message/server/message_user.rs`

- [ ] **Step 1: Append the test module**

Add this module to the end of
`vendor/soulseek-rs-lib/src/message/server/message_user.rs`. Do **not** add the
`acknowledgement_failure` helper yet; Task 3 introduces it.

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

    /// An acknowledgement is server code 23 followed by the original id.
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
            Ok(ServerMessage::SendMessage(acknowledgement)) => {
                assert_acknowledges(&acknowledgement);
            }
            other => panic!("second event must be the acknowledgement, got {other:?}"),
        }

        assert!(
            receiver.try_recv().is_err(),
            "a new message produces exactly two events"
        );
    }

    #[test]
    fn a_replayed_message_is_acknowledged_without_being_delivered_again() {
        let (sender, receiver) = std::sync::mpsc::channel();
        let mut message = message_payload(false);

        MessageUser.handle(&mut message, sender);

        match receiver.try_recv() {
            Ok(ServerMessage::SendMessage(acknowledgement)) => {
                assert_acknowledges(&acknowledgement);
            }
            other => panic!("a replay must only be acknowledged, got {other:?}"),
        }

        assert!(
            receiver.try_recv().is_err(),
            "a replay must not be delivered to the client again"
        );
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail for the right reasons**

Run: `cargo test -p soulseek-rs-lib --lib message_user -v`
Expected: FAIL, both tests, with these messages:

- `a_replayed_message_is_acknowledged_without_being_delivered_again` fails with
  `a replay must only be acknowledged, got PrivateMessageReceived(...)` because
  the current handler forwards a replay and never acknowledges it.
- `a_new_message_is_delivered_before_it_is_acknowledged` fails with
  `first event must be the delivery, got SendMessage(...)` because the current
  handler acknowledges before forwarding.

Do not proceed while either test passes, and do not weaken an assertion to make
it pass.

- [ ] **Step 3: Do not commit yet**

These tests are intentionally red. They are committed together with the
implementation in Task 3 Step 4 so every commit stays green.

---

### Task 3: Deliver new messages first and acknowledge every delivery

**Files:**

- Modify: `vendor/soulseek-rs-lib/src/message/server/message_user.rs`

- [ ] **Step 1: Replace the handler body**

Replace the entire current contents of
`vendor/soulseek-rs-lib/src/message/server/message_user.rs` with:

```rust
use crate::actor::server_actor::{ServerMessage, UserMessage};
use crate::message::server::MessageFactory;
use crate::message::{Message, MessageHandler};
use crate::{debug, error, info};

use std::sync::mpsc::Sender;

pub struct MessageUser;

/// Body-free diagnostic for an acknowledgement that could not be queued.
///
/// Kept as a pure function so the privacy guarantee is directly testable: the
/// crate logger exposes no readable capture, so a test asserts this text names
/// the message and sender without repeating the message body.
fn acknowledgement_failure(id: u32, username: &str) -> String {
    format!("[MessageUser] could not acknowledge message {id} from {username}")
}

impl MessageHandler<ServerMessage> for MessageUser {
    fn get_code(&self) -> u8 {
        22
    }

    fn handle(&self, message: &mut Message, sender: Sender<ServerMessage>) {
        let id = message.read_int32();
        let timestamp = message.read_int32();
        let username = message.read_string();
        let message_content = message.read_string();
        let new_message = message.read_bool();

        // Deliver before acknowledging. The INFO line is the user-visible
        // delivery, so clearing the message server-side first would risk losing
        // it if local forwarding then failed.
        if new_message {
            let user_message =
                UserMessage::new(id, timestamp, username.clone(), message_content, new_message);

            info!("[MessageUser] User message received:{:?}", user_message);

            // Surface the message to the client so it can be read via the API.
            if let Err(error) = sender.send(ServerMessage::PrivateMessageReceived(user_message)) {
                error!(
                    "[server] Error forwarding private message to client: {}",
                    error
                );
            }
        }

        // Acknowledge every delivery, including replays. The server keeps
        // re-sending a message it still holds, which is why a replayed private
        // message used to reappear on every run.
        if let Err(error) = sender.send(ServerMessage::SendMessage(
            MessageFactory::build_message_acked(id),
        )) {
            debug!("{}: {}", acknowledgement_failure(id, &username), error);
        }
    }
}
```

- [ ] **Step 2: Run the focused tests to verify they pass**

Run: `cargo test -p soulseek-rs-lib --lib message_user -v`
Expected: PASS for both tests, plus the pre-existing
`message::server::message_factory::tests::test_build_message_acked` when the
wider filter is used.

- [ ] **Step 3: Run the whole vendored library suite**

Run: `cargo test -p soulseek-rs-lib --lib`
Expected: all tests pass with 0 failures.

- [ ] **Step 4: Commit the tests and the implementation together**

```bash
git add vendor/soulseek-rs-lib/src/message/server/message_user.rs
git commit -m "fix: acknowledge replayed peer messages so they stop reappearing"
```

---

### Task 4: Prove acknowledgement failure is non-fatal and body-free

**Files:**

- Modify: `vendor/soulseek-rs-lib/src/message/server/message_user.rs` (`mod tests`)

- [ ] **Step 1: Add the disconnected-channel and privacy test**

Append this test inside the existing `mod tests` block in
`vendor/soulseek-rs-lib/src/message/server/message_user.rs`, after
`a_replayed_message_is_acknowledged_without_being_delivered_again`:

```rust
    #[test]
    fn a_disconnected_channel_is_non_fatal_and_the_diagnostic_omits_the_body() {
        // Ack enqueue failure must not panic, for a new message or a replay.
        for new_message in [true, false] {
            let (sender, receiver) = std::sync::mpsc::channel();
            drop(receiver);
            let mut message = message_payload(new_message);

            MessageUser.handle(&mut message, sender);
        }

        // The failure text names the message and sender, never the body.
        let failure = acknowledgement_failure(MESSAGE_ID, SENDER);
        assert!(failure.contains("292923"), "got: {failure}");
        assert!(failure.contains(SENDER), "got: {failure}");
        assert!(
            !failure.contains(BODY),
            "acknowledgement diagnostics must not repeat the body, got: {failure}"
        );
    }
```

- [ ] **Step 2: Run the test**

Run: `cargo test -p soulseek-rs-lib --lib message_user::tests::a_disconnected_channel_is_non_fatal_and_the_diagnostic_omits_the_body -- --exact`
Expected: PASS. One `ERROR` line about the dropped client channel is printed to
stderr because the default log level is `Warn`; that output is expected.

This test cannot start red: it exercises a helper and a no-panic property that
Task 3 introduced. It is a regression pin for the privacy and non-fatal
guarantees, not a TDD step. Say so plainly rather than claiming a red phase.

- [ ] **Step 3: Run the full message module**

Run: `cargo test -p soulseek-rs-lib --lib message:: -- -v`
Expected: PASS for all message handler, factory, and reader tests.

- [ ] **Step 4: Commit**

```bash
git add vendor/soulseek-rs-lib/src/message/server/message_user.rs
git commit -m "test: cover peer-message acknowledgement failure and diagnostic privacy"
```

---

### Task 5: Final verification

**Files:** none (verification only)

- [ ] **Step 1: Format and lint**

```bash
cargo fmt --all
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
```

Expected: exit 0 for all three, zero warnings.

- [ ] **Step 2: Full workspace test suite**

```bash
cargo test --workspace
```

Expected: every suite passes with 0 failures, including the vendored library
tests and the end-to-end `a_private_message_is_delivered_between_users` test.

- [ ] **Step 3: Pre-commit gate**

```bash
pre-commit run --all-files
```

Expected: all hooks pass.

- [ ] **Step 4: Confirm the acceptance criteria by inspection**

```bash
rg -n 'acknowledgement_failure|MessageFactory::build_message_acked|PrivateMessageReceived|if new_message' vendor/soulseek-rs-lib/src/message/server/message_user.rs
```

Expected: exactly one `acknowledgement_failure` definition, one code-23
acknowledgement call site, one `PrivateMessageReceived` call site inside the
`if new_message` branch, and no second acknowledgement site.

- [ ] **Step 5: Confirm nothing outside scope changed**

```bash
git diff --stat origin/main..HEAD
```

Expected: only `vendor/soulseek-rs-lib/src/message/server/message_user.rs` and
the amended spec document, plus this plan.

- [ ] **Step 6: Report**

If Steps 1-3 changed any file, commit it:

```bash
git add -A
git commit -m "chore: format after peer-message acknowledgement fix"
```

---

## Self-Review

### Spec coverage

| Spec requirement | Task |
| --- | --- |
| Branch on the existing `new_message` flag | 3 |
| New message: log once, buffer once, then acknowledge | 3 |
| Delivery event precedes acknowledgement | 3 (ordering assertions in Task 2) |
| Delivery happens before the acknowledgement attempt | 3 |
| Replay: no INFO, no client forwarding, acknowledgement only | 2, 3 |
| Acknowledgement uses code 23 and the original id | 2 (`assert_acknowledges`), 3 |
| API-forwarding failure does not block acknowledgement | 3 (acknowledgement is outside the branch) |
| Acknowledgement failure is DEBUG-only and body-free | 3 (single `debug!` call site plus pure helper), 4 |
| Acknowledgement failure is non-fatal | 4 |
| No message-ID store, config key, schema change, or public API change | 1-5 (no such file touched) |
| Existing private-message and workspace tests stay green | 3, 5 |
| In-code comments explain delivery order, replay suppression, and DEBUG-only failures | 3 |
| Spec's log-capture clause reconciled with the codebase | 1 |

No spec section is left without a task.

### Placeholder scan

No `TBD`, `TODO`, or "implement later" text. Every code step contains the code
to write, every command shows its expected result, and the two intentionally
red tests in Task 2 have their exact expected failure messages recorded.

### Type consistency

- `acknowledgement_failure(id: u32, username: &str) -> String` is defined in
  Task 3 and called identically in Task 3's handler and Task 4's test.
- `message_payload(new_message: bool) -> Message` and
  `assert_acknowledges(acknowledgement: &Message)` are defined once in Task 2 and
  reused in Task 4.
- Constants `MESSAGE_ID`, `TIMESTAMP`, `SENDER`, `BODY` are defined once in
  Task 2 and referenced in Tasks 2 and 4 with the same names.
- Accessors used line up with the real API: `id()`, `timestamp()`,
  `username()`, `message()`, `is_new()`.
- `ServerMessage::SendMessage(Message)` and
  `ServerMessage::PrivateMessageReceived(UserMessage)` are used exactly as the
  enum defines them.
- The acknowledgement byte layout `[23, 0, 0, 0, id LE]` matches
  `MessageFactory::build_message_acked`, which the existing factory test pins.
