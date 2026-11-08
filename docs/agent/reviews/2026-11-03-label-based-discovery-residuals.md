<!-- markdownlint-disable MD013 -->

# Label-Based Discovery — Code Review Residuals (round 10)

Triage: PLAY (branch main, 2026-11-03)

| | |
| --- | --- |
| Branch | `main` |
| Base commit | `528155d` (`v0.34.0`); all review work is uncommitted on top |
| Rounds run this pass | 8 (rounds 3-10 of the resumed review; rounds 1-2 were lost to a context budget) |
| Circuit breaker | `CIRCUIT_BREAKER_ROUNDS = 10` — reached |
| Reviewer A | `opencode-go/deepseek-v4.1-flash:max` |
| Reviewer B | `opencode-go/glm-5.3-flash:max` |

Round 10 reported one HIGH, four MEDIUM and five LOW findings. Every fix below was applied after the
round-10 reviewers read the code, so **none of it has been independently re-reviewed** — the cap ends
the loop at round 10 by design and forbids an 11th round.

This is the second cap ledger for this change. The first
(`2026-11-02-label-based-discovery-residuals.md`) records an earlier 10-round pass; its findings were
re-checked and closed by every reviewer in this pass, and its own round-10 fixes were verified as
applied by rounds 3-9 here. Findings from rounds 3-9 of this pass were each re-read by the following
round's reviewers and are recorded in the session transcript rather than duplicated here.

## Findings

| # | Severity | Location | Finding | Status |
| --- | --- | --- | --- | --- |
| 1 | HIGH | `src/runner.rs` `outcome_is_cancellation` | The cancellation predicate matched the reason's **tail**, but a peer controls the crate's `Failed` reason, which is forwarded verbatim inside `transfer failed: {reason}`. A reason ending with the cancellation text therefore faked an operator interrupt: the label sweep marked itself cancelled, skipped the remaining albums, charged nothing and printed `label: cancelled by user`. | `applied, not independently re-reviewed` — the predicate now requires exact equality with the two shapes the download stage really produces (`download error: …` and `all candidates exhausted: download error: …`). `an_interrupted_album_is_a_cancellation_not_a_failure` gained the spoof case; reverting to a tail test fails it. |
| 2 | MEDIUM | `src/label_discover.rs` `--since` notice | `since_skipped` (dated, below the bound) and `undated_skipped` are disjoint, but the notice printed the first as a total with the second as a parenthetical, so "0 excluded" could appear beside three undated exclusions. | `applied, not independently re-reviewed` — the first number is now the sum, with "N below the bound, M undated or unparseable". The notice test asserts the whole line. |
| 3 | MEDIUM | `src/filter.rs` `summarize_rejections_with_queue_limit` | A result the album gate dropped because no name-matching group satisfied the per-album rules was counted as **no rejection at all**, so `explain_no_candidates` printed "no rejections" for a result the gate had refused. | `applied, not independently re-reviewed` — the summary now checks `group_satisfies_album_rules` over the candidate groups and attributes the first group's failure (too short, missing track 1, non-contiguous, or peer count). `a_gate_rejected_result_is_counted_in_the_summary` fails without it. |
| 4 | MEDIUM | `src/label_discover.rs` `label_targets` | Under a collaboration credit, `select_albums` cannot fold a lead-artist-prefixed title, so one album listed as both `Half Light` and `Grafix - Half Light` yields two targets and is downloaded twice. | `applied, not independently re-reviewed` — recorded as an accepted consequence in the spec (guessing which part of a credit is the lead artist is rejected for the same reason the artist sweep rejects it) and pinned by `a_full_credit_cannot_fold_a_lead_prefixed_title`. No behaviour change. |
| 5 | MEDIUM | `src/client.rs` `safe_filename` | The download bridge stripped control characters from a peer-supplied file name but not Unicode format characters, so a bidi override could still reorder a `Downloading:` line. | `applied, not independently re-reviewed` — the filter now also rejects `formatting::is_format_character`, matching `single_line` and `progress::safe_label`. |
| 6 | LOW | `src/download.rs`, `src/filter.rs`, `src/runner.rs` | Peer- and provider-supplied text reached `tracing` records unsanitised at several sites: the transfer-failure reason, the rejection-summary usernames, and the label resolution reason. | `applied, not independently re-reviewed` — each site now formats through `formatting::single_line`. The stored username is untouched (it keys peer reputation). |
| 7 | LOW | `src/discography/label.rs` | The catalogue-cache write/size failure branch had no test, unlike the artist-path equivalent. | `applied, not independently re-reviewed` — `a_catalogue_too_large_to_cache_is_still_returned` proves an oversized payload is still returned and not written. |
| 8 | LOW | `src/label_discover.rs` | `release_allowed` was evaluated twice per release, so a rejected release produced two identical debug records. | `applied, not independently re-reviewed` — evaluated once and reused. |
| 9 | LOW | `src/runner.rs`, `src/main.rs` | Upgrade and batch recorded an operator interrupt only as an INFO line, while discover, label and manual add a summary notice. | `applied, not independently re-reviewed` — both now add `<mode>: cancelled by user; the run stopped before it finished`, matching the other modes. |
| 10 | LOW | `README.md` | The `--since` documentation omitted that a date which exists but cannot be parsed is also excluded. | `applied, not independently re-reviewed` — the label bullet now says so and matches the notice's wording. |

## Verification gap

Round 10's fixes were applied after both reviewers finished reading the code. Nothing in this ledger
has been independently re-reviewed. The full suite passes locally (`cargo test`: 1602 tests, 0
failures), `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check` and `markdownlint` are
clean, and every fix that has a test was mutation-checked by hand (reverting the guard fails the
test). Those are the implementer's own runs, not a reviewer's.

The specific risks a later review should re-check, in priority order:

1. `outcome_is_cancellation` against a genuinely interrupted download **and** against the spoof case:
   does the sweep stop, stay uncharged, leave `failed_albums` at zero, and print
   `label: cancelled by user` — while a peer-supplied `Failed` reason ending with the cancellation
   text is still counted as a failure?
2. The album gate's rejection attribution: a result whose name-matching group is too short beside a
   larger group that does not match must appear in the summary, and one whose name-matching group
   passes must not be counted as rejected.
3. `recover_interrupted_upgrades` with a seeded library: a staged success the library does not hold
   must be left in place, and an interrupted upgrade's album (which the library does hold) must still
   be adopted.
4. The staging-copy policy: a forced reprocess must leave the previous copy in place and warn, and a
   colliding album's files must survive both a failed and a successful attempt.
5. The label summary's `--since` line, and the new `stale catalogue was used` / `already processed`
   lines against the spec's observability order.

## Lifecycle

A later fix run may take these findings on. `review-triage` names the ledger it accepts in
`REVIEW_TRIAGE_STATUS: PLAY <path>` and stamps it with a `Triage: PLAY` line; `finalising` deletes the
ledger once every finding in it is verified resolved, or shrinks it to the rows still open.
