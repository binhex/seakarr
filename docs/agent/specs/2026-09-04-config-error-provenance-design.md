# Configuration Error Provenance Design

## Goal

Make mode-conflict errors explain that the conflicting mode came from the YAML
configuration. The error should identify the absolute filepath and line
containing `search.default_mode`.

Example:

```text
seakarr: configuration error: --artist/--album conflict with
search.default_mode: auto in /absolute/path/configs/seakarr.yml:21;
use --mode manual
```

## Scope

- Add source provenance for configurations loaded from disk.
- Resolve the configured file path to an absolute path before reporting it.
- Record the line containing `search.default_mode`.
- Include provenance in conflicts caused by CLI selectors against a configured
  mode.
- Preserve existing behavior for explicit `--mode` conflicts and in-memory
  test configs without provenance.
- Add focused unit and integration coverage.

## Design

`Config` will retain optional, non-serialized provenance metadata set by
`Config::load()`. The metadata contains the absolute config filepath and the
line number of `search.default_mode`. CLI merging will not alter it.

The mode resolver will distinguish whether the effective mode came from
`--mode` or from `search.default_mode`. When the configured mode causes a
conflict with `--artist`, `--album`, or `--batch-file`, the error will append
the source location and configured value. If provenance is unavailable, the
resolver will fall back to the current concise error format so pure in-memory
callers remain supported.

The loaded filepath will be made absolute after the config file exists,
including after first-run default-file creation. The line number will be
determined from the loaded YAML source while locating the
`search.default_mode` entry. It will be associated with the current on-disk
config so migration/reconciliation cannot leave the diagnostic pointing at a
stale file representation.

## Error behavior

- Configured `auto` plus `--artist` or `--album`: identify
  `search.default_mode: auto`, filepath, and line; recommend `--mode manual`.
- Configured `auto` plus `--batch-file`: identify the same source; recommend
  `--mode batch`.
- Explicit `--mode auto` plus manual or batch selectors: retain the existing
  error style because the mode source is already visible in the CLI command.
- Invalid modes, blank selectors, and unrelated validation errors remain
  unchanged unless they directly use configured-mode provenance.

## Testing

- Unit test that a loaded YAML config records the absolute filepath and the
  correct `search.default_mode` line.
- Unit tests that configured-auto/manual-selector and configured-auto/batch-
  selector errors include the filepath, line, configured value, and
  recommendation.
- Integration test that invokes the binary with a relative `--config-path`
  and verifies the error reports the absolute config filepath and line.
- Regression coverage ensuring explicit CLI mode errors and in-memory configs
  continue to work.

## Out of scope

- Reporting line numbers for every configuration validation error.
- Changing YAML reconciliation or migration semantics.
- Changing the precedence rules between CLI and configuration values.
