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
- **Authoritative artist discography** — artist-only manual runs resolve conceptual albums from MusicBrainz
  release groups (cached locally for 30 days) and then run one sequential `Artist Album` Soulseek search per
  eligible album, oldest first. Release categories and optional MusicBrainz artist IDs are configurable; the
  previous folder heuristic remains the explicit opt-out and the visible fallback.
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
  across restarts and schedule cycles.
- **Scheduled mode** — run the selected auto, manual, or batch operation immediately, then repeat it after a
  configurable interval. SIGTERM stops after the active cycle; Ctrl+C requests active-cycle cancellation or
  stops the scheduler while it is waiting between cycles.
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

# Scheduled foreground loop (run immediately, then wait 60 min after each cycle)
seakarr --schedule

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
artist-only manual search resolves MusicBrainz conceptual albums first and then
performs one sequential targeted Soulseek search per eligible album, oldest
first. Explicit artist+album, album-only, batch, auto, and library-upgrade flows
are unchanged. If authoritative discovery cannot be established, the previous
single-query folder heuristic remains available as a visible fallback; set
`discography.enabled: false` to select it deliberately. `--test` performs the same mode and selector
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
| `--artist <name>` | Manual selector; without `--album`, processes each eligible MusicBrainz conceptual album (or each identifiable folder when legacy discovery is selected). | *(from config)* |
| `--album <name>` | Manual selector; may be used without `--artist`. | *(from config)* |
| `--batch-file <path>` | Batch selector; surrounding whitespace is ignored; cannot be combined with artist or album selectors. | *(from config)* |
| `--schedule` | Run immediately, then repeat the same validated auto, manual, or batch operation after each interval. | `false` |
| `--ignore-processed` | Reprocess a successful album once. | `false` |

`--ignore-processed` applies to one-shot auto, manual, and batch modes. It is a
CLI-only override: it does not persist to configuration YAML, it leaves search
history and unrelated albums untouched, and it is intended for replacing corrupt
or otherwise invalid downloaded files. The matching successful database record is
deleted before the attempt; if the retry fails, the album remains eligible for
normal future retries. In manual and batch modes, use `storage.organize: true` if
the replacement should be moved into the library; otherwise it remains in staging.
In auto mode, only albums selected by the existing upgrade scanner are eligible.
The flag cannot be combined with `--schedule` (or configured scheduled mode), so a
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

### `discography`

Controls authoritative MusicBrainz discovery for artist-only manual runs. The
artist's conceptual release groups are resolved before any Soulseek search;
explicit artist-plus-album, album-only, batch, auto, and library-upgrade flows
are unchanged. The MusicBrainz API needs no account or API key.

| Key | Description | Default |
| --- | ----------- | ------- |
| `enabled` | Use MusicBrainz release groups before artist-only Soulseek searches; `false` selects legacy folder discovery. | `true` |
| `cache_days` | Complete 24-hour periods before refresh; `0` refreshes every run but keeps stale fallback. | `30` |
| `allowed_types` | Any of `studio_album`, `live_album`, `ep`, `single`, `compilation`, `remix`, `soundtrack`, `dj_mix`, `mixtape`. | `[studio_album]` |
| `artist_mbids` | Optional artist-name to MusicBrainz UUID map for ambiguous names. | `{}` |

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

### `schedule`

| Key | Description | Default |
| --- | ----------- | ------- |
| `enabled` | Run the selected operation in a foreground interval loop. Also enabled by `--schedule`. | `false` |
| `interval_mins` | Minutes to wait after a completed cycle before starting the next one. Values below `1` are clamped to `1`. | `60` |

#### Migrating from daemon terminology

`--daemon` remains as a hidden compatibility flag for this release. It behaves
like `--schedule`, prints a deprecation warning, and will be removed in the
next minor release. Existing `daemon.enabled` and
`daemon.rescan_interval_mins` values are migrated automatically to
`schedule.enabled` and `schedule.interval_mins`. Before rewriting the file,
seakarr saves the original as `seakarr.yml.bak`. Explicit values already under
`schedule` take precedence over legacy values.

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

Performs steps 3–7 above for an explicit album, or for every eligible album
resolved from an artist-only MusicBrainz discography lookup. Each resolved album
then runs through the normal targeted `Artist Album` search, sequentially and
oldest first; editions and remasters of the same conceptual album are
deduplicated before searching. At least one target is required; CLI values take
precedence over `search.manual.artist` and `search.manual.album`, and album-only
searches are supported.

Explicit artist-plus-album and album-only manual searches, batch mode, automatic
mode, and the library-upgrade workflow are unchanged and never consult
MusicBrainz. The legacy single-query folder heuristic is retained as the
`discography.enabled: false` opt-out and as an automatic fallback when
authoritative discovery cannot be established; automatic fallback is reported
with a WARN and a run-summary notice.

### Batch mode

Reads a newline-separated text file of `artist - album` lines and performs steps 3–7 for each line. Reports
success and failure counts on completion. Lines starting with `#` are treated as comments.

### Scheduled mode

When `--schedule` or `schedule.enabled` is set, the same validated auto, manual, or batch
plan runs immediately in a foreground loop. After each cycle completes, seakarr waits for
`schedule.interval_mins` before dispatching that unchanged plan again. SIGTERM received at
any time stops the scheduler after the active cycle and removes the PID file. Ctrl+C during
an active cycle requests cancellation; Ctrl+C while waiting between cycles stops the
scheduler and removes the PID file.

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

**Q: How does artist-only manual mode choose which albums to download?**

Artist-only manual runs resolve the artist on MusicBrainz and process conceptual release groups rather than
arbitrary Soulseek folders:

- Automatic resolution accepts only one unique exact artist name match. Matching uses Unicode NFKC
  normalization, lowercase conversion, trimming, and whitespace collapse, but punctuation stays significant,
  so `AC/DC` and `AC DC` are distinct. Zero matches or multiple matches are unresolved: seakarr does not pick
  the highest-scored result, it falls back as described below. Use `discography.artist_mbids` to pin an
  ambiguous name to a MusicBrainz artist UUID; a configured ID always takes precedence over name search.
- Only release groups matching `discography.allowed_types` are eligible, and a group carrying several
  recognised classifications must match all of them. The default `[studio_album]` accepts MusicBrainz primary
  type `Album` with no secondary type, so live albums, compilations, remixes, soundtracks, DJ mixes,
  mixtapes, EPs, and singles are excluded until you opt in.
- Release-group browse requests use MusicBrainz `release-group-status=website-default`, which excludes
  promotional, bootleg, and pseudo-release-only groups while keeping a conceptual album that also has an
  official release.
- Conceptual albums are deduplicated by normalized title and processed oldest first, so different editions or
  remasters of the same album produce one targeted `Artist Album` search. Undated albums are processed after
  dated ones, sorted by title.
- Each eligible album runs through the existing targeted search cascade sequentially; one album's failure does
  not block the remaining albums, and cancellation stops scheduling later ones.

**Q: What happens when MusicBrainz cannot be reached or returns nothing?**

Fallback precedence is fresh cache, successful refresh, stale cache, then the legacy heuristic:

- A cached discography is fresh for `discography.cache_days` complete 24-hour periods (30 by default). `0`
  attempts a refresh on every run but still keeps the cached copy as a stale fallback. Boundary equality is
  stale, and a clock rollback or future timestamp is stale too.
- A successful refresh replaces the cached copy atomically. The raw release groups are re-filtered against the
  current `allowed_types` on every run, so changing categories applies immediately without re-downloading.
- If a refresh fails, seakarr warns with the cache age and the failure reason and processes the compatible
  stale cache instead.
- If no compatible cache exists, seakarr logs a prominent WARN, records a run-summary notice naming the exact
  reason, and uses the legacy single-query folder heuristic; the notice states that album names were discovered
  heuristically from Soulseek folders.
- A valid authoritative empty result — zero release groups, or zero albums left after `allowed_types`
  filtering — is not an error: no Soulseek search runs, there is no heuristic fallback, and the run summary
  shows a neutral notice instead of a failed or skipped album.
- Set `discography.enabled: false` to select the legacy heuristic deliberately. That choice is logged as an
  explicit configuration choice without an outage warning.

**Q: Does MusicBrainz require an account or API key?**

No. Seakarr sends an identifying User-Agent and paces requests to at most one per second, following the
[MusicBrainz rate-limiting guidance](https://musicbrainz.org/doc/MusicBrainz_API/Rate_Limiting). Normal reads
need no credential, and no new secret is added to `seakarr.yml`.

**Q: How do I pin an ambiguous artist name to a MusicBrainz ID?**

Add the artist name and its canonical hyphenated UUID to `discography.artist_mbids`. Keys are matched after
the same normalization used for automatic artist matching:

```yaml
discography:
  enabled: true
  cache_days: 30
  allowed_types: [studio_album, live_album]
  artist_mbids:
    "Nirvana": "5b11f4ce-a62d-471e-81fc-a69a8278c7da"
```

A configured ID takes precedence over name search. If it differs from the cached identity, the cache row is
ignored and refreshed.

**Q: What are the current MusicBrainz discovery limits?**

The integration deliberately favors safe fallback over guessing:

- Artist search reads at most 100 candidates. If MusicBrainz reports more, seakarr cannot prove the exact-name
  match is unique, so it uses compatible stale cache or the clearly warned legacy fallback.
- Configuration validates an MBID's UUID shape, not its existence. A well-formed ID that MusicBrainz does not
  recognize is retried on a later stale/uncached run and follows the normal visible fallback path; failed IDs
  are not negatively cached.
- The SQLite cache keeps one bounded row per normalized artist queried and has no automatic eviction. A later
  refresh replaces that artist's row atomically.
- `Retry-After` accepts integer delay-seconds from 0 through 30. HTTP-date, malformed, or larger values are
  treated as provider unavailability so an external response cannot cause an unbounded wait.

___
If you appreciate my work, then please consider buying me a beer  :D

[![PayPal donation](https://www.paypal.com/en_US/i/btn/btn_donate_SM.gif)](https://www.paypal.com/cgi-bin/webscr?cmd=_s-xclick&hosted_button_id=MM5E27UX6AUU4)
