<!-- markdownlint-disable MD013 -->
# Scheduled Loop Rename Design

## Summary

Rename seakarr's foreground recurring execution feature from "daemon" to "schedule." The current feature does
not detach or run as a background daemon: it runs the selected operation immediately, waits for a configured
interval after completion, and repeats in the foreground.

The canonical public interface becomes:

```text
seakarr --schedule
```

```yaml
schedule:
  enabled: true
  interval_mins: 60
```

The behavior of the recurring loop does not otherwise change.

## Goals

- Describe the feature accurately as interval-driven scheduled execution rather than daemonization.
- Rename the CLI flag, YAML schema, diagnostics, logs, documentation, tests, and internal identifiers coherently.
- Preserve existing users' configuration values through automatic migration and backup.
- Preserve scripts for one minor release through a deprecated `--daemon` compatibility flag.
- Preserve all current auto, manual, batch, signal-handling, and PID-cleanup behavior.

## Non-goals

- Detaching or backgrounding the process.
- Adding a service manager, process supervisor, or operating-system daemon integration.
- Adding cron expressions, calendar times, or wall-clock scheduling.
- Adding a configurable first-run policy.
- Changing runner, database, Soulseek, search, download, or organization behavior.
- Rewriting historical design and implementation documents that used the old terminology.

## Current Behavior

The current `--daemon` flag and `daemon.enabled` configuration enter a foreground loop. Seakarr resolves one
auto, manual, or batch execution plan, dispatches it immediately, waits for
`daemon.rescan_interval_mins` after that cycle completes, and dispatches the same plan again. SIGTERM received
during a cycle stops the loop after that cycle; SIGINT during a cycle requests cancellation, while SIGINT or
SIGTERM received between cycles stops the loop and removes the PID file.

The term "rescan" is also inaccurate for manual and batch plans, which repeat searches or batch processing
rather than scanning the library.

## Approaches Considered

### 1. Full terminology migration — selected

Rename the public interface and corresponding internal concepts. Automatically migrate existing YAML and retain
a temporary deprecated CLI compatibility flag.

This provides one consistent vocabulary for users and maintainers while giving existing installations a bounded,
low-risk migration path.

### 2. Public facade only

Expose `--schedule` and `schedule:` publicly while retaining daemon-named internal types and functions.

This reduces the implementation diff, but leaves misleading terminology throughout the maintenance surface and
makes future diagnostics and tests easier to implement inconsistently.

### 3. CLI alias only

Add `--schedule` while retaining the existing `daemon:` configuration schema and internal terminology.

This is the smallest change, but it does not satisfy the configuration rename and creates two public names for one
concept.

## Public Contract

The canonical CLI flag is `--schedule`. Normal help output advertises only this flag and describes it as repeating
the selected operation in a foreground interval loop.

The canonical YAML schema is:

```yaml
schedule:
  enabled: false
  interval_mins: 60
```

The interval remains a delay between completed cycles rather than a fixed wall-clock cadence. When scheduling is
enabled, the first operation runs immediately. After it completes, seakarr waits `schedule.interval_mins` before
starting the next cycle.

The same already-validated auto, manual, or batch execution plan is reused on every cycle. Scheduling remains
incompatible with `--ignore-processed` because that combination would force repeated reprocessing.

## CLI Compatibility

The first release containing `--schedule` also accepts `--daemon` as a hidden compatibility flag. Using the legacy
flag:

1. enables scheduled execution;
2. emits a warning that `--daemon` is deprecated and `--schedule` should be used; and
3. otherwise behaves identically to `--schedule`.

If both flags appear, scheduling is enabled once and the legacy-use warning is emitted. This avoids an unnecessary
error because the two flags request the same behavior.

The legacy CLI flag is removed in the following minor release. The migration note and warning must state this
bounded compatibility period without depending on a hard-coded version number in runtime logic.

## YAML Migration

Configuration loading extends the existing reconciliation and backup mechanism. Migration occurs before unknown
keys are removed or schema defaults are merged.

The migration rules are:

1. Rename the top-level `daemon` section to `schedule`.
2. Rename `rescan_interval_mins` to `interval_mins` within the effective schedule section.
3. Preserve legacy values.
4. If old and new names coexist, explicit values under `schedule` win.
5. For a field missing under `schedule`, inherit the corresponding value from `daemon` when present.
6. Remove the legacy section and key after merging.
7. Back up the original file as `seakarr.yml.bak` before writing the migrated YAML.

A config containing only current names is unchanged. Re-loading a migrated config is idempotent and does not cause
another rewrite. Migration support for old YAML remains available after the CLI compatibility flag is removed so
users can upgrade directly from older versions.

Newly created default configurations contain only `schedule.enabled` and `schedule.interval_mins`.

## Internal Components

### CLI and scheduler entry point

`src/main.rs` owns the canonical and compatibility flags, deprecation warning, schedule activation, interval
construction, recurring loop, cycle helper, signal handling, and scheduler log messages.

Internal names should use schedule-oriented terms, including the CLI field, overrides, loop function, cycle helper,
comments, and log messages. The scheduling loop continues to call the shared execution-plan dispatcher.

### Configuration

`src/config.rs` owns `ScheduleConfig`, `Config.schedule`, `CliOverrides.schedule`, default serialization, CLI merge,
YAML migration, interval validation, and reconciliation tests.

The migration should build on the existing reconciliation mechanism rather than introduce a second config rewrite
path.

### Mode validation

`src/mode.rs` keeps execution-plan resolution unchanged. It reads the renamed scheduled-state fields and updates
user-facing validation messages from daemon terminology to scheduled terminology.

### Unchanged boundaries

Runner APIs, execution-plan variants, database schemas, network behavior, download behavior, and organization logic
remain unchanged. The rename stops at the scheduler boundary and does not alter the operation performed within a
cycle.

## Runtime and Error Behavior

- `schedule.interval_mins: 0` is clamped to one minute with a warning using the new key path.
- An interval that cannot be represented safely is rejected using the new key path.
- `--ignore-processed` combined with CLI- or config-enabled scheduling fails before startup side effects.
- A cycle failure is logged and the scheduler continues to the next interval, matching current behavior.
- SIGTERM during a cycle stops scheduling after that cycle; SIGINT during a cycle requests cancellation.
- SIGINT or SIGTERM received between cycles stops the loop and removes the PID file.
- Config migration failures retain the existing backup/write error behavior and do not silently discard values.
- User-visible logs use "Schedule," "scheduled mode," or "cycle," not "daemon."

## Documentation

Update the active README content, including:

- feature summary;
- quick-start example;
- CLI option table;
- configuration section and defaults;
- operating-mode explanation;
- `--ignore-processed` compatibility text; and
- a migration note covering the deprecated flag and automatic YAML backup/rewrite.

Historical specifications and plans remain unchanged because they record the terminology and behavior at the time
they were written. The new design document is the authoritative record for this rename.

## Testing and Acceptance Criteria

The implementation is complete when tests demonstrate all of the following:

1. `--help` advertises `--schedule` and does not advertise the hidden legacy flag.
2. `--schedule` enables recurring execution.
3. `--daemon` temporarily enables the same behavior and emits the deprecation warning.
4. Supplying both flags enables one scheduler and emits the warning without a conflict.
5. Default YAML contains only `schedule.enabled` and `schedule.interval_mins`.
6. Old-only YAML migrates both the section and interval key while preserving values and creating a backup.
7. New-only YAML is not rewritten.
8. Mixed old/new YAML gives new values precedence and inherits only missing legacy values.
9. Re-loading migrated YAML is idempotent.
10. Auto, manual, and batch schedules run immediately and reuse the same validated plan on later cycles.
11. Interval clamping, overflow rejection, cycle-failure continuation, signal handling, and PID cleanup retain their
    current behavior with schedule-oriented messages: SIGTERM stops after an active cycle, SIGINT requests active-cycle
    cancellation, and either signal stops scheduling while the loop is waiting.
12. `--ignore-processed` remains rejected for both CLI- and config-enabled scheduling before login or other startup
    side effects.
13. The full existing test suite passes without database or network contract changes.

## Release Sequence

- **First minor release:** introduce the canonical schedule names, migrate YAML automatically, and accept the hidden
  deprecated `--daemon` flag with a warning.
- **Following minor release:** remove the legacy CLI flag. Keep YAML migration support for direct upgrades from older
  releases.
