# CLI Mode Resolution for Target Selectors

**Date:** 2026-09-02  
**Status:** Approved design; pending written-spec review  
**Scope:** CLI mode selection, validation, and dispatch  

<!-- markdownlint-disable MD013 -->

## 1. Problem

Seakarr currently resolves the effective mode from `search.default_mode` after CLI
overrides are merged. When a configuration contains a library path and its mode
remains `auto`, invoking the binary with manual target flags can still enter the
library scanner. For example, a command containing `--artist sleeper --album
"the modern age"` produced `Scanning library...` and processed an unrelated album.

This is misleading and potentially unsafe: a user who supplied a specific target
must never silently trigger an automatic library scan. The application must either
run a compatible manual or batch operation, or stop with an actionable mode error.

## 2. Goals

- Resolve and validate mode/selector combinations before any database setup, PID
  lock, listener startup, Soulseek login, or library scan.
- Prevent manual or batch CLI selectors from reaching auto-mode dispatch.
- Make album-only manual searches valid while still requiring at least one manual target.
- Apply identical rules to one-shot, daemon, and `--test` execution.
- Produce actionable errors that identify the incompatible input and the required mode.
- Keep mode resolution pure and independently testable.
- Preserve the existing YAML schema, database schema, and search/download behavior outside the manual input boundary.

## 3. Non-goals

- Automatically changing `auto` to `manual` or `batch` based on selectors.
- Adding new CLI flags or configuration keys.
- Changing library scanning, search ranking, download, organization, or persistence behavior.
- Treating inactive configuration sections as errors.
- Adding network calls or connection recovery as part of mode resolution.

## 4. Decisions

### 4.1 Mode precedence

The CLI `--mode` value takes precedence over `search.default_mode`. If `--mode` is
absent, the configured default remains authoritative. CLI selectors do not silently
override either source of mode selection.

Consequently, when the configuration resolves to `auto`, the reported invocation
without `--mode manual` must fail rather than scan the library. An explicit
compatible mode is required.

### 4.2 Selector groups

- Manual selectors: `--artist` and `--album`.
- Batch selector: `--batch-file`.
- Manual and batch CLI selector groups are mutually exclusive.
- A manual request may contain artist only, album only, or both.
- A manual request with neither target is invalid.

Values are considered present only when non-empty after trimming. CLI values take
precedence over values in the selected mode's configuration section.

### 4.3 Inactive configuration values

Mode-specific configuration values are passive. Only the section belonging to the selected mode is used:

- `search.manual.artist` and `search.manual.album` are used only in manual mode.
- `search.batch.file_path` is used only in batch mode.
- Values in inactive sections are ignored and do not infer a mode or create a conflict.

### 4.4 Validation timing

Mode resolution occurs immediately after configuration loading and CLI input
preparation. It must complete before runtime side effects such as database opening,
PID acquisition, listener setup, or Soulseek login. `--test` invokes the same resolver
before its existing structural validation and exits without connecting.

### 4.5 Daemon consistency

Daemon mode follows the same resolved mode and criteria as one-shot execution. A
validated execution plan is passed into daemon cycles so a manual or batch request
cannot be reinterpreted as an auto scan.

## 5. Mode matrix

| Effective mode | Accepted inputs | Rejected CLI inputs | Required data |
| --- | --- | --- | --- |
| `auto` | Library configuration | `--artist`, `--album`, `--batch-file` | Existing library-path requirements |
| `manual` | CLI/config artist and/or album | `--batch-file` | At least one non-empty artist or album |
| `batch` | CLI/config batch file | `--artist`, `--album` | A non-empty batch-file path |

The following combinations are invalid:

- Any manual selector with effective `auto`; the error instructs the user to use `--mode manual`.
- `--batch-file` with effective `auto` or `manual`; the error instructs the user to
  use `--mode batch` where appropriate.
- Any manual selector combined with `--batch-file`.
- `--mode auto` combined with a manual or batch CLI selector.
- `--mode manual` combined with `--batch-file`.
- `--mode batch` combined with `--artist` or `--album`.
- Manual mode without a resolved artist or album.
- Batch mode without a resolved batch-file path.
- An unsupported mode value.

A configured value in an inactive section does not change this matrix. For example,
a configured batch path is ignored in manual mode, and configured manual criteria
are ignored in auto mode.

## 6. Architecture

### 6.1 Dedicated mode unit

Add a focused `src/mode.rs` unit containing:

- A `SearchMode` representation for `Auto`, `Manual`, and `Batch`.
- A resolved execution-plan representation containing the mode and its selected criteria.
- Pure resolution and validation logic that accepts the raw CLI override presence plus the loaded configuration values.
- Actionable configuration errors for invalid combinations and missing criteria.

The unit must not depend on networking, database state, filesystem scanning, or
runner side effects. It should be exposed through the crate library boundary as
needed by `main.rs` and tests.

### 6.2 Startup integration

`main.rs` will prepare the CLI override values, call the resolver against the loaded
configuration and raw overrides, and retain the resulting plan. Only after
resolution succeeds will the existing CLI-to-config merge run. That merge remains
responsible for runtime configuration values and for making criteria available to
daemon-compatible configuration paths, but it must not be used as a substitute for
canonical mode validation.

The startup sequence becomes:

1. Parse CLI arguments.
2. Load the configuration.
3. Prepare CLI overrides.
4. Resolve the execution plan from the loaded configuration and raw overrides.
5. Return an actionable error immediately if resolution fails.
6. Merge valid CLI overrides into the runtime configuration.
7. For `--test`, run structural validation and exit.
8. For a real run, continue with existing configuration validation, database setup,
   PID locking, login, and dispatch.
9. Dispatch using the resolved plan.

### 6.3 Runner boundary

The resolved manual plan carries optional artist and album values. The manual
runner boundary must accept an optional artist so album-only searches are represented
explicitly rather than rejected by startup validation. When artist is absent, the
manual path preserves album-only semantics rather than substituting a library target.
Existing search and download logic remains unchanged apart from accepting this
approved optional-artist input where required by the manual path.

Batch and auto plans continue to use their existing runner paths and configuration settings.

### 6.4 Daemon boundary

`run_daemon` and its cycle dispatcher consume the validated plan. Each cycle uses
the plan's mode and criteria while continuing to receive the full configuration for
operational settings. A daemon invocation with valid manual or batch CLI criteria
therefore repeats that requested operation; it never falls back to scanning the
library.

## 7. Error behavior

Errors use the existing configuration-error mechanism and are specific enough to correct the invocation. They should:

- Name the supplied selector or conflicting selector groups.
- Identify the effective mode when useful.
- State the required corrective mode or missing value.
- Occur before the first connection log and before library scanning.

Representative behavior:

- Configured `auto` plus `--artist`/`--album`: reject and state that `--mode manual` is required.
- Configured `auto` plus `--batch-file`: reject and state that `--mode batch` is required.
- `--mode manual --batch-file file.txt`: reject the batch selector as incompatible with manual mode.
- `--mode batch --artist Artist`: reject the manual selector as incompatible with batch mode.
- `--mode manual` with no effective target: reject and state that at least one of artist or album is required.
- `--mode batch` with no effective file: reject and state that a batch file is required.

The exact wording may follow existing `SeakarrError::Config` conventions, but no
invalid combination may be silently ignored.

## 8. Testing strategy

### 8.1 Resolver unit tests

Add table-driven or equivalent unit coverage for:

- Configured auto with no selectors produces an auto plan.
- Configured auto with artist, album, or batch-file CLI input fails.
- Explicit `--mode auto` with any selector fails.
- Explicit manual mode accepts artist-only, album-only, and artist-plus-album input.
- Manual mode resolves CLI values before configured manual values.
- Manual mode without both targets fails.
- Explicit batch mode resolves a CLI batch path before the configured path.
- Batch mode without a path fails.
- Manual and batch CLI selectors together fail.
- Every explicit mode/selector mismatch fails.
- Unsupported mode values fail.
- Inactive configuration values are ignored.
- Whitespace-only values do not satisfy required inputs.

### 8.2 Startup and `--test` regressions

Add coverage proving that the exact reported scenario does not enter auto dispatch.
The validation path must fail before connection and scanning. Add corresponding
`--test` coverage proving that the same mode error is reported without network
activity.

### 8.3 Daemon regressions

Preserve and extend the existing daemon tests to verify that validated manual and
batch plans remain the selected operation across cycles. Include an album-only manual
plan where the test infrastructure permits it.

### 8.4 Existing suite

Run the project's existing Rust formatting, linting, unit, integration, and
pre-commit checks after implementation. No existing search, download, scanner, or
persistence test should require a behavior change unrelated to mode input resolution.

## 9. Documentation updates

Update the README and CLI option documentation to state:

- `--artist` and `--album` are manual selectors.
- `--batch-file` is a batch selector.
- Selectors do not silently override the configured mode.
- Users must select a compatible mode when the configured mode conflicts, especially
  `--mode manual` for the reported invocation.
- Album-only manual searches are supported.
- Daemon mode applies the same mode and selector rules.

No changes are required to the YAML schema or to existing configuration keys.

## 10. Acceptance criteria

The implementation satisfies this specification when:

1. The shown command with a configuration whose effective mode is `auto` exits with
   an actionable mode error, before any Soulseek connection or library scan.
2. The same target succeeds through the explicit compatible manual invocation and processes only the requested target.
3. Auto mode still scans configured library paths when no conflicting selectors are present.
4. Batch mode accepts a compatible batch file and rejects manual selectors.
5. Manual mode accepts artist-only, album-only, and artist-plus-album requests, and rejects an empty target.
6. Invalid explicit mode/selector combinations fail without silently discarding flags.
7. `--test` applies the same mode-validation rules without network activity.
8. Daemon mode honors the same validated manual, batch, or auto plan on every cycle.
9. Existing tests and quality checks remain passing.

## 11. Implementation scope

Expected implementation files are:

- `src/mode.rs` — new pure mode model and resolver.
- `src/main.rs` — early resolution, plan-based one-shot and daemon dispatch, and startup tests.
- `src/runner.rs` — optional artist support at the manual boundary if required by the existing signatures.
- `src/lib.rs` — expose the new unit if required by the crate structure.
- `README.md` and relevant CLI documentation — clarify mode/selector semantics.

The implementation must not modify credentials, configuration instance files,
generated logs, database files, or unrelated search/download modules.
