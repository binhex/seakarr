<!-- markdownlint-disable MD013 -->

# Label-Based Discovery — Code Review Residuals

| | |
| --- | --- |
| Branch | `main` |
| Base commit | `3e2d2ac` (all review work is uncommitted on top of it) |
| Rounds run | 10 |
| Circuit breaker | `CIRCUIT_BREAKER_ROUNDS = 10` — reached |
| Reviewer A | `opencode-go/deepseek-v4.1-flash:max` |
| Reviewer B | `opencode-go/glm-5.3-flash:max` |

Round 10 reported 1 HIGH, 3 MEDIUM and 2 LOW findings. Every fix below was applied after the
round-10 reviewers read the code, so **none of them has been independently re-reviewed** — the cap
ends the loop at round 10 by design and forbids an 11th round.

## Findings

| # | Severity | Location | Finding | Status |
| --- | --- | --- | --- | --- |
| 1 | HIGH | `src/runner.rs` `outcome_is_cancellation` / `CANCELLED_REASON` | Round 9 changed the cancellation test to exact equality against `download cancelled by user`, but the download stage stores `SeakarrError::Download`'s **`Display`**, which is `download error: download cancelled by user` (optionally prefixed `all candidates exhausted:`). Every genuine Ctrl+C was therefore counted, charged and reported as a failed album, and a run interrupted on its last album reported as a normal completion. | `applied, not independently re-reviewed` — the predicate now matches the reason's tail against `download error: download cancelled by user`, which still cannot be spoofed by a peer filename (the queue-timeout path appends text after the basename, so the reason cannot end with it). The unit test is bound to the real text in both shapes. |
| 2 | MEDIUM | `src/label_discover.rs` `identities_in_bound` | The `--since` identity rescue was populated from any qualifying release, before the release-type filter, so an EP at or after the bound could rescue an album whose own studio-album group was below it — admitting an album the run's `--release-type` excludes. | `applied, not independently re-reviewed` — insertion is now gated on `release_allowed`, and `an_unwanted_type_cannot_rescue_an_album_through_the_bound` pins the negative. |
| 3 | MEDIUM | `src/label_discover.rs` `SelectionCounts` doc | The struct doc still described the pre-round-9 behaviour ("the album drops out of the work list without being counted as dropped. That is the artist sweep's collapse rule"), contradicting the rescue the code now performs. A reader trusting it could restore the old rule. | `applied, not independently re-reviewed` — the paragraph now describes the rescue and names the only unconditional drop. |
| 4 | MEDIUM | `src/runner.rs` label test block | No test bound `outcome_is_cancellation` to the reason text the download stage actually produces; the unit test pinned a bare string the pipeline never emits, and the integration test asserted only "no staging" and "one search", both true with the bug. This is the gap that let finding 1 through. | `applied, not independently re-reviewed` — the unit test now exercises both real shapes (`download error: …` and `all candidates exhausted: download error: …`) and still rejects a peer filename carrying the phrase. |
| 5 | LOW | `src/runner.rs` label loop | The label loop checks cancellation before the budget, the artist sweep the other way round, while the label comment claims parity. At the boundary where both trip together the two sweeps attribute the stop differently. | `not fixed` — deliberate: reporting an interrupt as "cancelled" rather than "budget reached" is the behaviour the round-7 finding asked for. The comment states the reason; the divergence in attribution is accepted. |
| 6 | LOW | `src/runner.rs` label loop comment | Same item as 5, reported by both reviewers as a wording rather than a behaviour issue. | `not fixed` — see 5. |

## Fixes applied during rounds 1-9 that were re-reviewed

Recorded for context only; these were verified by a later round's reviewers.

- Round 7 HIGH (interrupted album charged, counted and reported as a failure because
  `SeakarrError::Cancelled` is only produced by the library walk) — fixed, and re-verified in rounds
  8 and 9 before round 9 introduced regression 1 above.
- Round 1-6 findings (label search truncation, cache namespacing, bundle visibility, cross-credit
  dedupe, scan-failure handling, breaker sharing, notice truthfulness, README/spec drift) were each
  re-read by the following round's reviewers.

## Verification gap

Round 10's fixes were applied after both reviewers finished reading the code. Nothing in this
ledger has been independently re-reviewed. The full suite passes locally (`cargo test`: 1535 tests,
0 failures), `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check` and `markdownlint` are
clean, but those are the implementer's own runs, not a reviewer's.

The specific risks a later review should re-check, in priority order:

1. `outcome_is_cancellation` against a real interrupted download: does the sweep stop, stay
   uncharged, leave `failed_albums` at zero, and print the `label: cancelled by user` notice?
2. The `--since` rescue: an album with only an out-of-type qualifying group must still drop.
3. The rewritten `SelectionCounts` doc against the code, and the notice wording it feeds.

## Lifecycle

A later fix run may take these findings on. `review-triage` names the ledger it accepts in
`REVIEW_TRIAGE_STATUS: PLAY <path>` and stamps it with a `Triage: PLAY` line; `finalising` deletes
the ledger once every finding in it is verified resolved, or shrinks it to the rows still open.
Findings 5 and 6 are recorded as `not fixed` deliberately, so a triage run should either accept
that divergence in writing or change the label loop's ordering.
