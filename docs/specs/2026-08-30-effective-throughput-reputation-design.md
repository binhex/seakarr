# Effective-throughput peer reputation — design

Date: 2026-08-30
Status: approved

## Overview

Replace the per-track "measured speed" recorded into `peer_reputation` with
**effective throughput**: downloaded bytes divided by the total wall-clock time
the track took, including every retry attempt and the `retry_delay_secs` waits
between them. Peers that recover from retries are demoted by the very time they
cost, without needing to classify why the retry happened.

This is an evolution of the existing peer-reputation feature (v0.18.0), not a
new subsystem.

## Motivation

A track that retries and then succeeds is currently recorded as a pure success
with the *final attempt's* instantaneous speed. Retries are invisible to the
ranking, so a peer that needs two 30-second retries on every file ranks as well
as one that downloads cleanly — even though it burns real wall-clock time
(e.g. the benign "Token not found" race observed on a peer that costs ~30-50s
per retry). The time cost is real regardless of whose fault the retry is, so
it should be reflected in the peer's ranking.

## Behaviour

- Record `effective_throughput = downloaded_bytes / total_wall_time` (KiB/s) for
  each successfully downloaded track, instead of the EMA-smoothed instantaneous
  speed.
- `total_wall_time` spans the whole `download_file` retry loop: every attempt
  and every `retry_delay_secs` sleep.
- A clean first-attempt download is essentially unchanged (elapsed ≈ transfer
  time), so its throughput ≈ its instantaneous speed.
- A retried download gets a proportionally lower throughput, which the existing
  ranking blend turns into a demotion.
- **Reliability is untouched**: a retried-but-successful track still records
  `success = true`; only the speed axis moves. (User chose "speed only".)

## Components

1. **`download_file`** (`src/download.rs`) — capture `std::time::Instant`
   before the retry loop; on success return
   `(path, file_bytes / elapsed.as_secs_f64() / 1024.0)` as the throughput in
   KiB/s. Byte count is the completed file's actual size
   (`std::fs::metadata(dest).len()`). Guard against negligible elapsed time
   with `elapsed.max(Duration::from_millis(1))`.
2. **`download_once`** — revert its return type to `Result<PathBuf>`. Its
   internal `speed_ema` remains, used only for the progress-bar display.
3. **Unchanged downstream** — `TrackRecord.speed_kbps`, `update_peer_reputation`,
   the `avg_speed_kbps` / `speed_samples` columns, and the `rank_candidates`
   blend all stay as-is; only the meaning of the recorded value changes.

## Data flow

```
download_file (per track)
  -> success: (path, bytes / total_elapsed / 1024.0 KiB/s)
  -> download_album pushes TrackRecord { username, speed_kbps: throughput, success }
  -> runner -> db.update_peer_reputation(username, throughput, success)
  -> rank_candidates blends avg_speed_kbps (now throughput) with advertised speed
```

## Error handling

- Failed tracks (retries exhausted) are unchanged: `success = false, speed = 0`.
- Negligible-elapsed guard prevents a divide-by-zero or an absurdly high spike.

## Testing

- Unit (`download.rs`): a clean transfer returns throughput ≈ bytes/elapsed; a
  fail-then-succeed transfer (mock with `download_fails` toggling, or a mock
  that fails once then succeeds, with `retry_delay_secs = 0`) returns a much
  lower throughput than the same bytes delivered cleanly.
- Existing `db.rs`/`filter.rs`/`runner.rs` tests seed `avg_speed_kbps` directly
  and remain green (no signature/schema change).

## Documentation

- README: update the peer-reputation wording from "measured download speed" to
  "effective throughput (bytes ÷ wall-clock time, including retries)".

## Notes

- Existing `avg_speed_kbps` rows recorded before this change hold
  instantaneous-speed values; they blend into the running average and dilute as
  new throughput data accumulates. No data reset is required.
- No config key, schema column, or ranking formula changes.
