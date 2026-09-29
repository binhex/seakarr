<!-- markdownlint-disable MD013 -->
# Rename `--mode auto` to `--mode upgrade`

## Problem

`auto` names the mode by its least informative property. The mode walks the library, matches
each album against the `filters` targets, and re-downloads the ones whose format or bitrate
falls below them. The README already describes it that way ("either upgrading what you
have", `README.md:1149`), and the config section that drives it is already called
`library_upgrade`. Every other mode names what it does (`manual`, `batch`, `discover`), so
`auto` is the one value an operator has to look up.

The rename is not a string change alone. `search.default_mode` is a field whose serde
default is serialised into every generated `seakarr.yml` (`src/config.rs:92-93`), via
`default_search_mode()` (`src/config.rs:358`). An existing installation's config therefore
almost certainly contains `default_mode: auto`. Rejecting the retired value without
migrating the file would make every existing install fail at startup on its next run.

## Goal

One spelling, `upgrade`, in every layer - the CLI flag, the config value, the error text,
the README, and the Rust identifiers - with the retired value rejected loudly on the
command line and migrated in place in the config file.

## Scope

In scope:

- `--mode auto` is rejected with a message naming `upgrade`.
- `search.default_mode: auto` is migrated to `upgrade` during config reconciliation.
- The schema default for `search.default_mode` becomes `upgrade`.
- `SearchMode::Auto` -> `SearchMode::Upgrade`, `ExecutionPlan::Auto` ->
  `ExecutionPlan::Upgrade`, `runner::run_auto_mode` -> `runner::run_upgrade_mode`.
- Every user-facing string and doc comment that names the mode, including `README.md`.
- Tests updated, plus new tests for the rejection and for the migration.

Out of scope:

- Accepting `auto` anywhere: no alias, no deprecation warning, no shim.
- Renaming anything that already says "upgrade" (`library_upgrade`, `needs_upgrade`).
- Changing what the mode does, its selectors, or the quality rules.
- Naming the YAML source line for arbitrary invalid mode values.

## Decisions

Three decisions were taken with the operator before this spec was written.

1. **The CLI rejects the retired value** rather than aliasing it, so the ambiguity the
   rename removes cannot survive, and the error teaches the new name at the point of use.
2. **The config value is migrated automatically** rather than rejected, because a scheduled
   run must not break on a file the tool itself generated.
3. **The rename reaches every layer**, so flag, config, errors, documentation and
   identifiers all say the same thing.

## Architecture

### `src/mode.rs` - the resolver

`SearchMode` (`src/mode.rs:6`) and `ExecutionPlan` (`src/mode.rs:15`) rename their `Auto`
variants to `Upgrade`, and `ExecutionPlan::mode` (`src/mode.rs:32`) follows. The mode match
(`src/mode.rs:116`) keeps four arms, with `"upgrade" => SearchMode::Upgrade` replacing
`"auto" => SearchMode::Auto`. That match is already made against `raw_mode.trim()`, so a
whitespace-padded `--mode " auto "` reaches the retired-value arm exactly as the migration's
`trim()` comparison expects.

The retired value gets its own arm ahead of the fall-through, so the message can point at
the replacement:

```text
invalid search mode 'auto'; this mode is now called 'upgrade'
```

The generic message keeps its existing shape, with `upgrade` first in the list
(`src/mode.rs:122`):

```text
invalid search mode '{value}' (must be upgrade, manual, batch, or discover)
```

Both `SearchMode::Upgrade` conflict messages (`src/mode.rs:148-156`) and the
`configured_mode_conflict` helper's `incompatible with auto mode` text (`src/mode.rs:79`)
are reworded to "upgrade mode". The helper keeps naming the YAML source and line through
`find_default_mode_line` (`src/config.rs:521`) when the mode came from the config.

`SearchMode` has no consumers outside `mode.rs`, so its rename is contained to that file
and its tests. `ExecutionPlan::Upgrade` is consumed by the dispatcher and by tests.

Sweep note: the rename must be token-aware. `src/client.rs:302`, `:501`, `:1299` and
`:1307` say "auto-retried" and mean something unrelated; a blind substring replacement
would corrupt them.

### `src/config.rs` - the migration and the default

`reconcile_config_file` (`src/config.rs:710`) already runs migrations on the parsed file
before merging it with the schema defaults:

```rust
let renamed = migrate_schedule_section(&mut file_value, config_file)?
    | migrate_rename(&mut file_value, "filters", "min_bitrate", "min_bit_rate")
    ...
```

A **value** migration joins that chain, built to the same contract as `migrate_rename`
(`src/config.rs:1070`): mutate the parsed YAML in place and report whether anything
changed.

`migrate_search_mode_value(&mut file_value) -> bool` rewrites `search.default_mode` when its
value, after `trim()`, equals `auto`. The comparison is deliberately the same one the
resolver makes (`--mode` if given, else `default_mode`, then `trim()`), so the migration can
never rewrite a value the resolver would accept or leave one it would reject.

Only that one value is touched. A blank, misspelled or otherwise unknown value is left in
place for the resolver to reject, unchanged.

The existing machinery then completes the work with no further changes:

- the `merged == file_value && !renamed` guard (`src/config.rs:738`) keeps an
  already-canonical config a true no-op: no rewrite, no backup;
- a change copies the original to `seakarr.yml.bak` (`src/config.rs:746`) and rewrites the
  file with the canonical header;
- `config reconciled with current schema (backup: ...)` records the event at INFO
  (`src/config.rs:755`).

`default_search_mode()` (`src/config.rs:358`) returns `"upgrade"` instead of `"auto"`, so a
newly generated config never contains the retired value.

### `src/main.rs` - the CLI surface

The `--mode` argument's help text (`src/main.rs:83`) becomes
`Override search mode (upgrade|manual|batch|discover)`, and the dispatcher's
`ExecutionPlan::Auto` arm (`src/main.rs:562`) calls `runner::run_upgrade_mode`.

### `src/runner.rs` and the prose that names the mode

`run_auto_mode` (`src/runner.rs:1403`) becomes `run_upgrade_mode`. Doc comments and comments
that name the mode are reworded in:

- `src/config.rs` - the `library_upgrade.enabled` text, `scan_on_startup`,
  `peer_track_count`, and the `search.default_mode` docs;
- `src/scanner.rs:411`, `:459`, `:583`, `:1324`, `:1539`, `:1604` - the "auto-mode upgrade
  destination/root/copies" wording;
- `src/filter.rs:7`, `:106`, `:165`, `:2112` - the library-track-count notes;
- `src/discover.rs`, `src/discs.rs` and `src/organizer.rs` where the word means the mode.

### Documentation and wording

`README.md`: the mode list and every `--mode` reference, the `search.default_mode` table row
and its default, the `library_upgrade.enabled` row ("auto mode only"), the `peer_track_count`
and `scan_on_startup` rows, and the FAQ line that reads "either upgrading what you have
(`--mode auto`)" (`README.md:1149`).

Historical specs under `docs/agent/specs/` are frozen records and are not edited.

## Data flow

Startup, in order:

1. `main` parses `--mode`, or leaves it unset.
2. `Config::load` reads `seakarr.yml`; `reconcile_config_file` runs its migrations, including
   `migrate_search_mode_value`. A retired value is rewritten to `upgrade` here, after the
   backup is written, so the returned `Config` already reports `upgrade`.
3. `resolve_execution_plan` reads `cli.mode` when given, otherwise
   `config.search.default_mode`. `--mode auto` fails here; `upgrade` yields
   `ExecutionPlan::Upgrade`.
4. The dispatcher calls `run_upgrade_mode`.

Because the migration runs before any resolution, a config that carried the retired value
behaves exactly like one that always said `upgrade`.

## Error handling

- **Retired CLI value:** `SeakarrError::Config` with the rename-pointing message, exit 1
  through the existing `exit_code_after_run` path. The same failure shape as any other
  invalid mode today.
- **Other invalid values:** unchanged message shape, with `upgrade` substituted for `auto`
  in the list.
- **Migration I/O failures:** the existing typed errors (`failed to back up config to ...`,
  `failed to write migrated config: ...`). A read-only config directory therefore fails the
  run, which it already does for schema drift, so this adds no new failure class.
- The migration cannot panic: it pattern-matches the parsed YAML and leaves anything
  unexpected in place.

## Backward compatibility

- `--mode auto` stops working, deliberately and loudly: any script using it fails with a
  message naming the replacement.
- `search.default_mode: auto` keeps working silently: the first run rewrites it to `upgrade`
  and preserves the previous file as `seakarr.yml.bak`.
- Rust API: `SearchMode::Auto` and `ExecutionPlan::Auto` disappear and `run_auto_mode` is
  renamed. `mode`, `runner` and `config` are public modules, so this is a breaking change for
  library consumers - which is why the release is a MINOR bump on this pre-1.0 crate,
  matching the convention of the previous release.
- Nothing else in the config schema changes.

## Deliberate limits

- No alias, no deprecation warning, no transitional period. The operator chose a hard rename;
  a shim would keep the ambiguous name alive and the flag and config would have different
  policies.
- The migration fixes one value only. It is not a general table of retired mode names, so a
  future rename adds its own step.
- An unknown value is never silently coerced. Only the exact retired spelling migrates, so
  typos stay visible.
- The mode's behaviour, selectors and quality rules are untouched.

## Testing

`src/mode.rs`:

- `--mode auto` is rejected and the message names `upgrade`.
- `upgrade` resolves to `ExecutionPlan::Upgrade`, from the CLI and from the config.
- The existing conflict tests move to `upgrade` and still identify the YAML source when the
  mode came from the config.
- An unknown value reports the new list, with `upgrade` first.

`src/config.rs`:

- A config containing `default_mode: auto` loads with `default_mode == "upgrade"`, the file
  on disk reads `upgrade`, and `seakarr.yml.bak` holds the pre-migration contents.
- A config already saying `upgrade` is untouched and creates no backup.
- A whitespace-padded `auto` migrates, matching the resolver's `trim()`.
- An unrelated invalid value is left in place and still rejected by the resolver.

Integration and regression:

- `tests/mode_resolution_test.rs` and `tests/pipeline_test.rs` are updated to the new
  vocabulary.
- The `src/runner.rs` tests that call `run_auto_mode` follow the rename.
- A regression test proves the retired value cannot reach the resolver from a migrated file:
  load the config, resolve the plan, and assert it is `Upgrade`.

Gates, as this repository requires: `cargo fmt --check`, `cargo clippy -- -D warnings`, the
full `cargo test`, coverage to the 95 percent floor on the changed files,
`markdownlint README.md`, and `pre-commit run --all-files`.

## Acceptance criteria

1. `--mode upgrade` selects the mode; `--mode auto` fails with a message naming `upgrade`.
2. A config containing `default_mode: auto` migrates to `upgrade` on first load, keeps
   `seakarr.yml.bak`, and the run proceeds normally.
3. `search.default_mode`'s default is `upgrade` in a freshly generated config.
4. No occurrence of `auto` that means this mode survives in `src/`, `tests/` or `README.md`,
   and the unrelated "auto-retried" wording in `src/client.rs` is untouched.
5. Every gate listed in Testing passes.

## Documentation

`README.md` is updated wherever it names the mode, so a reader finds `upgrade` and no stale
`auto`.

## Alternatives considered

**Translate in the resolver instead of migrating the file.** The resolver could accept `auto`
as a synonym and warn. Rejected: every existing config would keep the retired value, the
warning would repeat on every scheduled run, and the CLI and the config would disagree about
which spellings are legal.

**Keep `--mode auto` as a deprecated alias, following the `--daemon` pattern.** Rejected: the
operator asked for the name to be unambiguous, and an alias keeps the ambiguous name accepted
for at least another release.

**Migrate the value but leave the Rust identifiers alone.** Rejected: the flag would say
`upgrade` while `run_auto_mode` and `SearchMode::Auto` still said `auto`, which is the
confusion the rename exists to remove.
