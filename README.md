<!-- markdownlint-disable MD013 -->
# seakarr

Automated Soulseek music downloader with library quality upgrading.

## Features

- **Library quality scanner** — walks your music library directories, reads audio tags (FLAC, MP3, AAC,
  OGG, Opus, WAV, WMA, and more via [lofty](https://crates.io/crates/lofty)), and identifies albums
  whose tracks fall below configurable bitrate threshold or are in a lossy format.
- **Automatic mode** — for each album needing an upgrade, searches the Soulseek network, ranks
  candidates by speed × free slots × bitrate, downloads the best match, and organises the result into your
  library.
- **Manual & batch modes** — search for a specific artist/album on demand, or process a newline-separated
  text file of `artist - album` lines to download a curated wantlist.
- **Quality filtering** — filter Soulseek results by file extension, minimum
  bitrate, excluded keywords, and upload availability. With
  `download.max_queue_length: 0` a peer must offer a free upload slot; a
  positive limit also admits zero-slot peers until their reported queue
  position can be validated. Path-traversal names are rejected.
- **Peer reputation** — remembers each peer's effective throughput (bytes ÷ transfer time: retry delays plus
  the final transfer, excluding queue wait) and success rate (in SQLite), and ranks search results by a blend
  of advertised and measured throughput plus a reliability factor, so fast, reliable peers are preferred and
  slow or error-prone peers are demoted — regardless of what you search for. Controlled by
  `search.peer_reputation` (default `true`).
- **Download resilience** — speed monitoring with configurable minimums (slow
  peers cancelled mid-transfer), queue-wait limits while a transfer sits in a
  remote queue (`max_queue_time_secs`, `max_start_time_secs`), stall timeout
  with cancel, per-file retries with configurable count and delay, and
  candidate fallback (try the next ranked peer once retries are exhausted).
- **Post-download organisation** — move completed files from a staging directory into your library using a
  configurable naming pattern (`%artist%/%album%/...`), with traversal-safe sanitisation and automatic
  duplicate handling.
- **SQLite persistence** — tracks processed albums, download queue, peer reputation, and search history
  across restarts and daemon cycles.
- **Daemon mode** — run continuously, re-scanning your library on a configurable interval and upgrading
  albums as they become available on Soulseek. Graceful shutdown on SIGINT (Ctrl+C) and SIGTERM.
- **PID lock** — prevents concurrent instances from running against the same database and staging
  directory.
- **Notifications** — sends alerts via any [Apprise](https://github.com/caronc/apprise)-compatible service
  (ntfy, Discord, Telegram, email, and more) on each successful album download.
- **Config-driven** — all behaviour is controlled by a single `seakarr.yml` YAML file; a default is
  created automatically on first run. The CLI exposes only essential overrides.

## Prerequisites

- [Rust](https://www.rust-lang.org/tools/install) (stable toolchain)
- A [Soulseek](https://www.slsknet.org/) account (username and password)
- A music library organised as `Artist/Album/Track` directories (recommended)

## Quick start

### Installation

```bash
git clone https://github.com/binhex/seakarr
cd seakarr
cargo build --release
```

The binary is at `target/release/seakarr`.

### Usage

```bash
seakarr --help
```

On first run a default configuration file is created at `configs/seakarr.yml`. Edit it to set your
Soulseek username, password, and library paths, then run `seakarr --test` to validate.

```bash
# Validate configuration
seakarr --test

# One-shot automatic upgrade scan
seakarr

# Continuous daemon mode (re-scan every 60 min)
seakarr --daemon

# Manual search for every album found for an artist
seakarr --mode manual --artist "Pink Floyd"

# Manual search for one specific album
seakarr --mode manual --artist "Pink Floyd" --album "The Wall"

# Reprocess an album whose previous successful download is recorded
seakarr --mode manual --artist "Afterlife" \
  --album "The Afterlife Lounge" --ignore-processed

# Batch processing from a text file
seakarr --mode batch --batch-file wantlist.txt
```

Mode selection is explicit. `--artist` and `--album` are manual selectors, and
`--batch-file` is a batch selector; these options do not silently override the
configured `search.default_mode`. When the configured mode is `auto`, add
`--mode manual` for a manual target or `--mode batch` for a batch file. An
artist-only manual search discovers and processes each identifiable album from
one artist query. Explicit artist+album and album-only manual searches are also
supported. `--test` performs the same mode and selector
validation before structural checks, so manual mode requires a non-empty artist or
album and batch mode requires a non-empty batch file path. To clear a configured manual
fallback for one field, pass that selector explicitly as an empty value, such as
`--mode manual --artist ""` together with `--album "The Wall"`. Album-only searches
are intentionally album-name-only and are not recorded in processed-album history,
so a later run may search or download them again. If `storage.organize` is enabled
with library paths, album-only runs fail before searching because no artist
destination is available.

## Options

All options are optional overrides. When an option is omitted, the value from `seakarr.yml` is used.

### Config & paths

| Option | Description | Default |
| ------ | ----------- | ------- |
| `--config-path <dir>` | Directory containing `seakarr.yml`. | `configs` |
| `--log-path <dir>` | Override the log directory from config. The file `seakarr.log` is created inside. | *(from config)* |
| `--log-level <level>` | Override the log level. Choices: `DEBUG`, `INFO`, `WARN`, `ERROR`. | *(from config)* |
| `--db-path <dir>` | Override the database directory from config. The file `seakarr.db` is created inside. | *(from config)* |
| `--pid-path <dir>` | Override the PID file directory from config. The file `seakarr.pid` is created inside. | *(from config)* |
| `--library-path <path[,path...]>` | Comma-separated library paths, overrides `library.paths` in config. | *(from config)* |
| `--test` | Validate configuration and exit without running any tasks. | `false` |
| `--version` | Print the version and exit. | — |

### Soulseek auth

| Option | Description | Default |
| ------ | ----------- | ------- |
| `--soulseek-user <user>` | Soulseek username (overrides config). | *(from config)* |
| `--soulseek-password <pass>` | Soulseek password (overrides config). | *(from config)* |
| `--listen-port <N>` | Override incoming peer port. `0` disables the listener. | *(from config)* |

### Mode selection

| Option | Description | Default |
| ------ | ----------- | ------- |
| `--mode <mode>` | Select `auto`, `manual`, or `batch`: library scan, target search, or batch file. | *(from config)* |
| `--artist <name>` | Manual selector; without `--album`, processes each identifiable album found. | *(from config)* |
| `--album <name>` | Manual selector; may be used without `--artist`. | *(from config)* |
| `--batch-file <path>` | Batch selector; surrounding whitespace is ignored; cannot be combined with artist or album selectors. | *(from config)* |
| `--daemon` | Repeat the same validated auto, manual, or batch operation each cycle. | `false` |
| `--ignore-processed` | Reprocess a successful album once. | `false` |

`--ignore-processed` applies to one-shot auto, manual, and batch modes. It is a
CLI-only override: it does not persist to configuration YAML, it leaves search
history and unrelated albums untouched, and it is intended for replacing corrupt
or otherwise invalid downloaded files. The matching successful database record is
deleted before the attempt; if the retry fails, the album remains eligible for
normal future retries. In manual and batch modes, use `storage.organize: true` if
the replacement should be moved into the library; otherwise it remains in staging.
In auto mode, only albums selected by the existing upgrade scanner are eligible.
The flag cannot be combined with `--daemon` (or configured daemon mode), so a
forced reprocess is never repeated automatically on every cycle. If a forced
search fails before completing, the album is recorded as failed and remains
eligible for normal future retries.

## Configuration

All behaviour is controlled by a YAML file inside the config directory (`configs/seakarr.yml` by default).
A default config is created automatically on first run. The file is divided into the sections below.

### `soulseek`

| Key | Description | Default |
| --- | ----------- | ------- |
| `username` | Soulseek account username. Required. Overridden by `--soulseek-user`. | `""` |
| `password` | Soulseek account password. Required. Overridden by `--soulseek-password`. | `""` |
| `server` | Soulseek server address. | `server.slsknet.org:2242` |
| `login_retries` | Maximum login attempts with exponential backoff. | `3` |
| `login_retry_delay_secs` | Initial backoff delay in seconds (doubles each retry). | `5` |
| `listen_port` | Incoming peer port for accepting connections from other Soulseek clients. Set to `0` to disable the listener (firewalled mode). Requires port forwarding at your router for values > 0. | `2234` |
| `max_peers` | Maximum simultaneous peer connections (minimum 1). Each connection uses a 256 KB actor thread. Higher values allow more parallel search/download candidates but use more memory. | `64` |

### `library`

| Key | Description | Default |
| --- | ----------- | ------- |
| `paths` | Root directories to scan for music files. Each path should contain `Artist/Album` subdirectories. Overridden by `--library-path`. | `[]` |
| `scan_on_startup` | Rescan the library on startup (auto mode). *(Reserved for future use — not yet enforced.)* | `true` |

### `storage`

| Key | Description | Default |
| --- | ----------- | ------- |
| `staging_dir` | Directory where in-progress downloads land before organisation. Auto-created if missing. | `downloads/staging` |
| `organize` | Automatically move completed downloads into the library. | `false` |
| `organize_pattern` | Naming template for organised files. Placeholders: `%artist%`, `%album%`, `%track%`, `%title%`, `%ext%`, `%user%`. | `%artist%/%album%/%track% - %title%.%ext%` |

### `search`

The selected `default_mode` determines which mode-specific values are active.
Manual values and batch paths do not infer a mode; values belonging to an inactive
mode are ignored. CLI values take precedence over values in the selected section.

| Key | Description | Default |
| --- | ----------- | ------- |
| `default_mode` | Default search mode. Choices: `auto`, `manual`, `batch`. | `auto` |
| `timeout_secs` | How long to wait for Soulseek search responses. | `15` |
| `response_limit` | Maximum search results to collect. *(Reserved for future use — not yet enforced.)* | `1000` |
| `type` | Filter results by track count. `any` (no restriction), `album` (5+ tracks), `single` (1–4 tracks). *(Reserved for future use — not yet enforced.)* | `any` |
| `delay_secs` | Minimum gap between consecutive network searches to avoid flooding. *(Reserved for future use — not yet enforced.)* | `5.0` |
| `block_threshold` | Consecutive zero-result searches before checking for Soulseek rate-limiting. *(Reserved for future use — not yet enforced.)* | `5` |
| `block_pause_secs` | Pause duration when rate-limiting is detected, in seconds. *(Reserved for future use — not yet enforced.)* | `300` |
| `manual.artist` | Manual artist fallback, used only in `manual` mode. | `""` |
| `manual.album` | Manual album fallback, used only in `manual` mode. | `""` |
| `batch.file_path` | Batch file fallback, used only in `batch` mode. | `""` |
| `search_title_match` | Minimum percentage of the album's non-generic track titles that a title-search result must contain for the title-search fallback tier to keep it. Set `0` to disable the tier. | `70` |
| `peer_reputation` | Blend measured speed + reliability into search ranking. Set to `false` to rank by advertised speed only. | `true` |

### `filters`

Controls which Soulseek search results pass the quality gate.

| Key | Description | Default |
| --- | ----------- | ------- |
| `allowed_extensions` | Only consider files with these extensions (lowercase, no dot). | `[flac]` |
| `min_bit_rate` | Minimum bitrate in kbps. Lossy files whose actual bitrate is below this value are rejected (verified after download when the peer omits the bitrate attribute). `0` disables. | `0` |
| `min_bit_depth` | Minimum bit depth in bits (e.g. `16` or `24`). Lossless files whose actual bit depth is below this value are rejected (verified after download when the peer omits the attribute). `0` disables. | `0` |
| `exclude_words` | Reject files whose names contain any of these keywords (case-insensitive). | `[]` |
| `include_locked` | Include locked (private) files in search results. *(Reserved for future use — not yet enforced.)* | `false` |
| `contiguous_tracks` | Reject results with gaps in their track numbers; duplicates permitted. Numberless filenames (e.g. `track01.flac`, bare `Title.flac`) count as unnumbered — set `false` for unnumbered or multi-disc collections. | `true` |
| `min_tracks` | Minimum number of quality-passing tracks a share must contain for its files to be considered. Rejects incomplete shares (e.g. a single track of a 16-track album). Applies regardless of `contiguous_tracks`. Set `0` to disable. | `3` |
| `peer_track_count` | In auto mode, reject search results whose usable track count is below the library's existing track count for the same album. Prevents silent downgrades when the library already has a more complete copy. Also applies in manual mode when the album is already present in the library (a library track count is derived there); batch mode has no library track count. Note: with the default `min_tracks: 3`, albums with 1-2 tracks (EPs, singles) are rejected by `min_tracks` before this check runs — set `min_tracks: 0` or `1` to apply the library check to EPs. | `true` |

### `download`

| Key | Description | Default |
| --- | ----------- | ------- |
| `concurrent` | Maximum simultaneous album downloads. Defaults to `1` — the Soulseek server floods peer connections for every search result and the client library spawns a thread per peer, so higher values multiply thread usage. | `1` |
| `max_queue_length` | `0` requires a free upload slot during candidate selection; later telemetry does not retroactively reject an admitted free-slot peer. A positive value also permits zero-slot peers only when a reported positive queue position is at or below the limit. Unknown positions and wire position `0` do not prove a zero-slot peer is within the limit. | `0` |
| `max_start_time_secs` | Maximum seconds from first reaching queue position `1` until the first transfer progress. `0` disables this queue-head limit. | `120` |
| `max_queue_time_secs` | Maximum total seconds from enqueue until the first transfer progress. `0` disables this total queue limit. | `1800` |
| `min_upload_speed_kbps` | Cancel transfers where measured speed drops below this threshold. `0` disables the speed check. | `250` |
| `speed_check_wait_secs` | Seconds to wait after a transfer starts before measuring speed. | `30` |
| `timeout_secs` | Post-start inactivity timeout — starts at the first `InProgress` status, resets only on later `InProgress` events, and is not reset by a paused status. Cancels the transfer when no update arrives within this period. | `180` |
| `max_download_time_mins` | Hard wallclock ceiling in minutes for a single album download session. *(Reserved for future use — not yet enforced.)* | `120` |
| `max_retries` | Per-file retry attempts on the same peer before falling back to the next candidate. `0` disables retries. | `4` |
| `retry_delay_secs` | Seconds to wait between retry attempts. | `30` |
| `min_filtered_users` | Minimum number of filtered candidates required to apply the speed check. *(Reserved for future use — not yet enforced.)* | `10` |
| `skip_retry_hours` | Cooldown in hours before re-attempting a transiently-failed album on the next run. *(Reserved for future use — not yet enforced.)* | `24` |

### `library_upgrade`

Auto-mode workflow that finds library albums failing the quality gate and re-downloads them from a
better source, replacing the existing files.

| Key | Description | Default |
| --- | ----------- | ------- |
| `enabled` | Enable the library-upgrade workflow (auto mode only). When enabled, albums whose formats or bitrate fall below the `filters` targets are re-downloaded and their files copied into the library. | `false` |
| `delete_lesser_quality` | After a successful upgrade, delete existing files in the album that are lower quality than the newly downloaded copies (non-audio files are never deleted). | `false` |

### `database`

| Key | Description | Default |
| --- | ----------- | ------- |
| `path` | Directory for the SQLite database (`seakarr.db` is created inside). Overridden by `--db-path`. | `db` |

### `logging`

| Key | Description | Default |
| --- | ----------- | ------- |
| `level` | Log level for both console and file output. Choices: `DEBUG`, `INFO`, `WARN`, `ERROR`. Overridden by `--log-level`. | `INFO` |
| `path` | Directory for the log file (`seakarr.log` is created inside). Overridden by `--log-path`. | `logs` |
| `file` | Log filename. | `seakarr.log` |

### `pid`

| Key | Description | Default |
| --- | ----------- | ------- |
| `path` | Directory for the PID file (`seakarr.pid` is created inside). Overridden by `--pid-path`. | `pids` |
| `file` | PID filename. | `seakarr.pid` |

### `notifications`

| Key | Description | Default |
| --- | ----------- | ------- |
| `urls` | List of [Apprise](https://github.com/caronc/apprise) service URLs. A success notification is sent for each completed album. Leave empty to disable. | `[]` |

Apprise supports ntfy, Discord, Telegram, email, Slack, and many other services. Example:
`ntfy://my-topic`, `discord://webhook-id/webhook-token`.

### `daemon`

| Key | Description | Default |
| --- | ----------- | ------- |
| `enabled` | Run in daemon mode (continuously re-scan). Also enabled by `--daemon`. | `false` |
| `rescan_interval_mins` | Minutes between library re-scans in daemon mode. Values below `1` are clamped to `1`. | `60` |

## How it works

Seakarr has three operating modes:

### Automatic mode (default)

1. **Scan** — walks every path in `library.paths`, reads audio tags via `lofty`, and groups tracks by
   artist and album. Prefers tag metadata over directory names.
2. **Detect upgrades** — for each album, checks whether any track is in a non-allowed format or below
   `min_bit_rate`. Albums with tagged bitrate `None` are also flagged (unknown quality).
3. **Search** — queries the Soulseek network for each flagged album.
4. **Filter & rank** — filters results by extension, bitrate, excluded words,
   and upload availability: with `download.max_queue_length: 0` a peer must
   offer a free upload slot, while a positive limit also admits zero-slot peers
   whose queue position is validated during download. When
   `filters.contiguous_tracks` is enabled, results whose downloadable track
   numbers have gaps (or none at all) are discounted.
   Ranks candidates by `speed × slot_bonus × bitrate_bonus × reliability_factor` (reliability and measured-speed
   reputation adjust the advertised speed).
5. **Download** — downloads from the highest-ranked peer, monitoring transfer
   speed in real time. While a transfer waits in a remote queue, seakarr asks
   the peer for its position immediately and every five minutes.
   `max_queue_time_secs` caps the total wait from enqueue and
   `max_start_time_secs` caps the wait after reaching queue position `1`; a
   peer that exceeds either limit is abandoned and the next candidate is tried
   without retrying the same peer. After transfer progress begins,
   `timeout_secs` guards against inactivity (the per-album stall timeout for
   unresponsive peers). If the speed drops below `min_upload_speed_kbps`, the
   transfer is cancelled and the next candidate is tried. Per-file retries with
   configurable count and delay (`max_retries`, `retry_delay_secs`) re-attempt
   the same peer before falling back to the next candidate.
6. **Organise** — if `storage.organize` is enabled, completed files are moved from the staging directory
   into the library using the configured naming pattern. Duplicate filenames receive a `(1)` suffix.
7. **Persist & notify** — the album is marked as processed in SQLite and an Apprise notification is sent
   (if configured).

### Manual mode

Performs steps 3–7 above for an explicit album, or for every identifiable album
returned by an artist-only search. At least one target is required; CLI values
take precedence over `search.manual.artist` and `search.manual.album`, and
album-only searches are supported.

### Batch mode

Reads a newline-separated text file of `artist - album` lines and performs steps 3–7 for each line. Reports
success and failure counts on completion. Lines starting with `#` are treated as comments.

### Daemon mode

When `--daemon` or `daemon.enabled` is set, the same validated auto, manual, or batch
plan runs in a continuous loop. After each cycle, seakarr sleeps for
`daemon.rescan_interval_mins` before dispatching that unchanged plan again. The daemon
handles SIGINT (Ctrl+C) and SIGTERM gracefully — the PID file is removed and the current
cycle is allowed to finish.

## Development

```bash
git clone https://github.com/binhex/seakarr
cd seakarr
cargo build
```

### Running tests

```bash
cargo test
```

### Linting

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

### Pre-commit hooks

```bash
pre-commit run --all-files
```

## FAQ

**Q: Why do some albums fail with "no results passed filters"?**

`filters.contiguous_tracks` (default `true`) rejects search results whose track numbers have gaps,
and results with no parseable track numbers at all. Shares numbered like `track01.flac` (digits
fused to letters) or without numbers are treated as unnumbered. If your collection uses such
naming, set `filters.contiguous_tracks: false` in `seakarr.yml`.

These albums appear in the "Failed" section of the run summary with the reason
"no results passed filters". Albums with zero search results at all appear with the reason
"no results found". Both are retried on subsequent runs.

Two known limitations of the heuristic, also solvable with `contiguous_tracks: false`: (1) the
first number in the filename wins, so artist names containing digits (`Maroon 5`, `50 Cent`,
`Blink 182`) are parsed instead of the track number and can mask gaps; (2) disc-track numbering
(`1-01`, `2-03`) is parsed per disc — each disc's tracks are validated independently, so a
partial multi-disc share (missing a whole disc) is rejected rather than silently passing.

**Q: How should I organise my music library?**

Seakarr expects an `Artist/Album/Track` directory structure by default. For example:

```text
/media/music/
  Pink Floyd/
    The Dark Side of the Moon/
      01 - Speak to Me.flac
      02 - Breathe.flac
      ...
```

If your files have embedded tags (artist, album, bitrate), seakarr prefers tag metadata over directory
names. Files without readable tags fall back to the directory naming convention.

**Q: What formats can seakarr scan?**

All formats supported by the [lofty](https://crates.io/crates/lofty) crate: FLAC, MP3, AAC, OGG, Opus,
WAV, WMA, APE, MPC, Speex, and more. Bitrate and format information is extracted from file headers and tags.

**Q: What formats will seakarr download?**

The `filters.allowed_extensions` config key controls which formats pass the quality gate. By default only
`flac` is accepted — MP3s and other lossy formats are excluded. Set it to `[flac, mp3]` to allow both.

**Q: Can I download from queued peers instead of only free-slot peers?**

Yes. Keep `download.max_queue_length: 0` for free-slot-only behavior, or set a
positive limit to try zero-slot peers whose reported positive queue position
is within that bound. Unknown and out-of-bound positions fail closed and move
to the next candidate without retrying the same peer. A wire position of `0`
means the peer has no actionable queue entry; it is treated as unknown and
never proves a zero-slot peer eligible. A peer admitted with an advertised free
slot is not retroactively rejected by later telemetry. `max_queue_time_secs`
limits total queue wait, `max_start_time_secs` limits the wait after reaching
position `1`, and `timeout_secs` controls inactivity only after transfer
progress begins. With the defaults, a silent pre-start wait can therefore last
up to `max_queue_time_secs: 1800`, rather than the post-start
`timeout_secs: 180`.

**Q: How do I prevent seakarr from downloading files with certain words in the filename?**

Add entries to `filters.exclude_words` — for example, `[vinyl, demo, live]` will reject any file whose
name contains "vinyl", "demo", or "live" (case-insensitive).

**Q: What happens if my staging directory and library are on different filesystems?**

`fs::rename` cannot move a file across mount points, so seakarr detects the cross-device error and falls
back to copying the file into the library and then deleting the staging copy. The organized file lands in
the correct library location regardless of filesystem layout.

**Q: Can I run multiple instances of seakarr at once?**

No — the PID lock prevents concurrent runs. If a second instance starts, it detects the existing PID file,
checks whether the process is still alive, and exits with an error. Delete the PID file manually if it is
stale.

___
If you appreciate my work, then please consider buying me a beer  :D

[![PayPal donation](https://www.paypal.com/en_US/i/btn/btn_donate_SM.gif)](https://www.paypal.com/cgi-bin/webscr?cmd=_s-xclick&hosted_button_id=MM5E27UX6AUU4)
