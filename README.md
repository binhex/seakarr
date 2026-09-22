<!-- markdownlint-disable MD013 -->
# seakarr

Automated Soulseek music downloader with library quality upgrading.

## Features

- **Library quality scanner** — walks your music library directories, reads audio tags (FLAC, MP3, AAC,
  OGG, Opus, WAV, WMA, and more via [lofty](https://crates.io/crates/lofty)), and identifies albums
  whose tracks are in a format outside `filters.allowed_extensions` or fall below the
  configurable bitrate threshold.
- **Automatic mode** — for each album needing an upgrade, searches the Soulseek network, ranks
  candidates by advertised speed (adjusted by measured-throughput reputation), free-slot bonus, bitrate bonus,
  album-name match and peer reliability, and downloads the best match. The result is written into your
  library when `storage.organize` or `library_upgrade.enabled` is on; at the shipped defaults of `false` it
  stays in `storage.staging_dir`. An upgrade whose every destination already holds a strictly better file
  keeps those files and still completes the album, so the staging copy is removed without a new file being
  written. A candidate whose files omit the bitrate attribute scores zero on the bitrate bonus once
  `filters.min_bit_rate` is set, so a peer that reports one is preferred; it is still admitted, because its
  real quality is verified after download.
- **Discover mode** — derives the artist list from your library (tags first,
  folder names as the fallback), keeps only the artists that own a folder of
  their own, asks MusicBrainz which conceptual albums each of those is missing,
  and downloads only those. An artist the library only knows as a guest inside
  another artist's folder is skipped, and the run summary reports how many were
  skipped that way; name one explicitly with `--artist` to process it anyway.
  Albums already present are never
  searched or re-downloaded, and each run stops after
  `discover.max_cycle_downloads` download attempts so a large library fills in
  over successive runs. Release types come from `discography.allowed_types`.
- **Manual & batch modes** — search for a specific artist/album on demand, or process a newline-separated
  text file of `artist - album` lines to download a curated wantlist.
- **Authoritative artist discography** — artist-only manual runs resolve conceptual albums from MusicBrainz
  release groups (cached locally for 30 days) and then run one sequential `Artist Album` Soulseek search per
  eligible album, oldest first. The same authoritative resolution powers `discover` mode for every artist
  it sweeps — those that own a folder named after them. Release categories and optional MusicBrainz artist IDs are configurable; the
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
- **Download resilience** — speed monitoring with configurable minimums (a peer
  whose smoothed speed is below `min_upload_speed_kbps` past
  `speed_check_wait_secs` is cancelled and treated as a failed candidate rather
  than re-queued, because the same peer cannot get faster), queue-wait limits while a transfer sits in a
  remote queue (`max_queue_time_secs`, `max_start_time_secs`), stall timeout
  with cancel, per-file retries with configurable count and delay, and
  candidate fallback (try the next ranked peer once retries are exhausted).
- **Post-download organisation** — move completed files from a staging directory into your library using a
  configurable naming pattern (`%artist%/%album%/...`), with automatic duplicate handling. Every path
  component is sanitised before the folder is created: characters Windows reserves (`< > : " | ? *`),
  control characters, a trailing dot or space, and reserved device names are all removed or neutralised, so
  a library served to Windows clients over SMB renders every folder name instead of a mangled one. Path
  separators are replaced and directory-traversal sequences are rewritten, so remote metadata cannot escape
  the library. The mapping is deliberately lossy: two names that differ only in a removed character collapse
  to one component, so a peer offering both `Gold: Disc 1` and `Gold Disc 1` writes them into the same
  folder.
- **SQLite persistence** — tracks processed albums, peer reputation, and search history
  across restarts and schedule cycles.
- **Scheduled mode** — run the selected auto, manual, batch, or discover operation immediately, then repeat
  it after a configurable interval. SIGTERM stops after the active cycle; Ctrl+C requests active-cycle
  cancellation or stops the scheduler while it is waiting between cycles. A Ctrl+C received during the
  library scan aborts the scan and the run before any work item. While a run is active, a second Ctrl+C
  forces immediate exit with code 130 and leaves the PID file behind, which the next run treats as a
  stale lock and removes; a press while the scheduler waits between cycles is a graceful shutdown that
  removes the PID file and exits 0.
- **PID lock** — prevents concurrent instances from running against the same database and staging
  directory.
- **Notifications** — sends alerts for each successful album download by POSTing a small JSON body
  (`title`, `message`, `type`) to every configured `notifications.urls` entry. Only `http(s)` endpoints are
  delivered: an Apprise-style scheme URL such as `ntfy://my-topic` is accepted by configuration but never
  delivered, and the failure is logged as a warning rather than failing the run.
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
| `--mode <mode>` | Select `auto`, `manual`, `batch`, or `discover`. | *(from config)* |
| `--artist <name>` | Manual selector; without `--album`, processes each eligible MusicBrainz conceptual album (or each identifiable folder when legacy discovery is selected). In `discover` mode, an optional narrowing filter that must name an artist already in the library, and that overrides both `discover.exclude_artists` and the folder-ownership gate for that one artist. | *(from config)* |
| `--album <name>` | Manual selector; may be used without `--artist`. | *(from config)* |
| `--batch-file <path>` | Batch selector; surrounding whitespace is ignored; cannot be combined with artist or album selectors. | *(from config)* |
| `--schedule` | Run immediately, then repeat the same validated auto, manual, batch, or discover operation after each interval. | `false` |
| `--ignore-processed` | Reprocess a successful album once. | `false` |

`--ignore-processed` applies to one-shot auto, manual, batch, and discover modes.
In `discover` mode, and in artist-only manual runs (`--artist` without
`--album`), it is honoured for processed-record checks but it never bypasses the
library-presence check, so it cannot re-download an album you already own. To
force a replacement of a file you believe is corrupt, name the album explicitly
(`--artist X --album Y`), which is never filtered by library presence. It is a
CLI-only override: it does not persist to configuration
YAML, it leaves search history and unrelated albums untouched, and it is
intended for replacing corrupt or otherwise invalid downloaded files. The
matching successful database record is deleted before the attempt; if the retry
fails, the album remains eligible for normal future retries. In manual and
batch modes, use `storage.organize: true` if the replacement should be moved
into the library; otherwise it remains in staging until a later auto run with
`library_upgrade.enabled: true` runs its recovery scan, which adopts a staging
leftover with a matching successful record into `library.paths[0]` and deletes a
leftover with no record. In auto mode, only albums
selected by the existing upgrade scanner are eligible. The flag cannot be
combined with `--schedule` (or configured scheduled mode), so a forced reprocess
is never repeated automatically on every cycle. If a forced search fails before
completing, the album is recorded as failed and remains eligible for normal
future retries.

## Configuration

All behaviour is controlled by a YAML file inside the config directory (`configs/seakarr.yml` by default).
A default config is created automatically on first run. The file is divided into the sections below.

### `soulseek`

| Key | Description | Default |
| --- | ----------- | ------- |
| `username` | Soulseek account username. Required. Overridden by `--soulseek-user`. | `""` |
| `password` | Soulseek account password. Required. Overridden by `--soulseek-password`. | `""` |
| `server` | Soulseek server address. | `server.slsknet.org:2242` |
| `login_retries` | Maximum login attempts with exponential backoff. At most `10`. | `3` |
| `login_retry_delay_secs` | Initial backoff delay in seconds (doubles each retry, capped at 10 minutes per wait). At most `300`. | `5` |
| `listen_port` | Incoming peer port for accepting connections from other Soulseek clients. Set to `0` to disable the listener (firewalled mode). Requires port forwarding at your router for values > 0. | `2234` |
| `max_peers` | Maximum simultaneous peer connections (minimum 1). Each connection uses a 256 KB actor thread. Higher values allow more parallel search/download candidates but use more memory. | `64` |

### `library`

| Key | Description | Default |
| --- | ----------- | ------- |
| `paths` | Root directories to scan for music files, in priority order. Each path should contain `Artist/Album` subdirectories. When the same album is found under more than one root, the earliest listed entry supplies the location an upgrade writes back to, while the track counts from all copies are summed (so keep the entries non-overlapping: overlapping roots inflate the count the peer-completeness gate compares against). Discover placement is artist-level instead: it uses the root holding most of that artist's albums, with ties broken alphabetically. Overridden by `--library-path`. | `[]` |
| `scan_on_startup` | Rescan the library on startup (auto mode). *(Reserved for future use — not yet enforced.)* | `true` |

### `storage`

| Key | Description | Default |
| --- | ----------- | ------- |
| `staging_dir` | Directory where in-progress downloads land before organisation. Auto-created if missing. Keep it outside every `library.paths` entry: a staging folder inside a library root is scanned as if it were albums of its own artist. | `downloads/staging` |
| `organize` | Automatically move completed downloads into the library. | `false` |
| `organize_pattern` | Naming template for organised files. Placeholders: `%artist%`, `%album%`, `%track%`, `%title%`, `%ext%`, `%user%` (always expands to `unknown` today). Must be non-empty, relative, contain at least one ordinary path component, be free of `..` components, and must not end with a path separator. It must also expand to a distinct path per track: unlike the organize step, the library copy paths resolve a collision with the keep/overwrite rules instead of a `(1)` suffix, so a pattern that does not distinguish the tracks (for example one with neither `%track%` nor `%title%`) lets one track stand in for the rest. A pattern whose expansion names a path that already exists as a directory — `%artist%` alone, or `%artist%/%album%` when those match the on-disk folders — cannot be copied to at all, so every track fails with `Is a directory`. Discover placement warns when only some of the downloaded files were written; the upgrade path logs each kept file at INFO instead. Discover mode also uses it to shape placement even when `organize` is `false`. | `%artist%/%album%/%track% - %title%.%ext%` |

### `search`

The selected `default_mode` determines which mode-specific values are active.
Manual values and batch paths do not infer a mode; values belonging to an inactive
mode are ignored. CLI values take precedence over values in the selected section.

| Key | Description | Default |
| --- | ----------- | ------- |
| `default_mode` | Default search mode. Choices: `auto`, `manual`, `batch`, `discover`. | `auto` |
| `timeout_secs` | How long to wait for Soulseek search responses. | `15` |
| `response_limit` | Maximum search results to collect. *(Reserved for future use — not yet enforced.)* | `1000` |
| `type` | Filter results by track count. `any` (no restriction), `album` (5+ tracks), `single` (1–4 tracks). *(Reserved for future use — not yet enforced.)* | `any` |
| `delay_secs` | Minimum gap between consecutive network searches to avoid flooding. *(Reserved for future use — not yet enforced.)* | `5.0` |
| `block_threshold` | Consecutive zero-result searches before checking for Soulseek rate-limiting. *(Reserved for future use — not yet enforced.)* | `5` |
| `block_pause_secs` | Pause duration when rate-limiting is detected, in seconds. *(Reserved for future use — not yet enforced.)* | `300` |
| `manual.artist` | Manual artist fallback, used only in `manual` mode. | `""` |
| `manual.album` | Manual album fallback, used only in `manual` mode. | `""` |
| `batch.file_path` | Batch file fallback, used only in `batch` mode. | `""` |
| `search_title_match` | Minimum percentage of the album's non-generic track titles that a title-search result must contain for the title-search fallback tier to keep it. Set `0` to disable the tier. At most `100`. | `70` |
| `peer_reputation` | Blend measured speed + reliability into search ranking. Set to `false` to rank by advertised speed only. | `true` |

### `discography`

Controls authoritative MusicBrainz discovery. Artist-only manual runs resolve
the artist's conceptual release groups before any Soulseek search, and
`discover` mode applies the same resolution to every eligible artist derived
from your library. Explicit artist-plus-album, album-only, batch, auto, and
library-upgrade flows are unchanged. The MusicBrainz API needs no account or API
key.

A release group whose title names several other release groups joined by a spaced slash, such as
Archive's `Controlling Crowds / You All Look the Same to Me`, is not used as an album name. Each part
is already a release group of its own, so it is searched under its own title instead of the
concatenation, which returned no results. A part is only searched when it is also selected by
`allowed_types`; a part whose own release group the release-type filter excludes is not searched at
all. A title keeps being searched as MusicBrainz spells it unless every part of it matches one of
that artist's release-group titles exactly, after case, width and whitespace folding, so `Either/Or`
is unaffected, and so is a venue-and-date title such as
`Live at Wembley Stadium, London, England / April 20th, 1992`.

| Key | Description | Default |
| --- | ----------- | ------- |
| `enabled` | Use MusicBrainz release groups before artist-only Soulseek searches; `false` selects legacy folder discovery. | `true` |
| `cache_days` | Complete 24-hour periods before refresh; `0` refreshes every run but keeps stale fallback. | `30` |
| `failure_cache_days` | How long an artist that MusicBrainz could not resolve is remembered, so a later run skips the lookup instead of asking again. `0` disables the failure cache: nothing is recorded and nothing is skipped. Only resolution failures are remembered — a MusicBrainz outage is always retried. | `7` |
| `allowed_types` | Any of `studio_album`, `live_album`, `ep`, `single`, `compilation`, `remix`, `soundtrack`, `dj_mix`, `mixtape`. Must not be empty while `enabled` is true. | `[studio_album]` |
| `artist_mbids` | Optional artist-name to MusicBrainz UUID map for ambiguous names. Keys must be unique after normalization, so `Nirvana` and ` nirvana ` cannot both appear. | `{}` |

### `discover`

Controls `discover` mode, which fills gaps in your library for artists you
already have.

Gap filling follows folders, not tags. An artist is only swept when at least one
of its albums was found in a folder named after the artist itself, so an album
tagged `ARTIST=Apashe` that sits inside the `Bassnectar` folder does not make
Apashe an artist discover fills: it is skipped, counted in the run summary as
`discover: N artist(s) skipped: no folder of their own`, and named in the debug
log. Name such an artist with `--artist` to process it anyway. The accepted cost
is that an artist whose albums live in a folder with a different name — beyond
case, spacing, or width folding, which the gate tolerates — or in a
folder the organiser sanitised, is no longer swept automatically.

| Key | Description | Default |
| --- | ----------- | ------- |
| `max_cycle_downloads` | Download attempts allowed per run. `0` means unlimited. Only work that reached the download stage is charged: albums skipped because they are already present, albums already recorded as processed, and albums whose search produced no admissible candidate all cost nothing. A transfer that fails is charged and retried on a later run, so a cluster of permanently unavailable albums can consume successive runs before later artists are reached. | `5` |
| `exclude_artists` | Artist names skipped before any MusicBrainz lookup, matched as whole names ignoring case and spacing, which prevents aggregator folders from expanding into hundreds of releases when `compilation` or `live_album` is enabled. An explicit `--artist` overrides this list for that one artist. | `[Various Artists, VA, Unknown Artist]` |

An album counts as present when a matching `artist/album` folder holds at least
one audio file. Both halves are matched under two spellings, because the two
sides are written by different owners: the album matches on its embedded album
tag **or** on any album folder the scan found it in, and the artist matches on
its tag spelling **or** on the artist folder the albums were found in. That is
what lets a placed album stay present when a peer's tag spells the title
differently from the MusicBrainz title the folder was named from. Matching is
exact after case, spacing, and Unicode folding, and
after the name sanitiser has run on both sides: the folder seakarr writes has
been through it, so a title such as `Tronic Jazz: The Berlin Sessions` is stored
as `Tronic Jazz The Berlin Sessions` and still satisfies the MusicBrainz title it
came from. Punctuation the sanitiser leaves alone stays significant, so an album
held only under a spelling that differs in both the tag and the folder name — a
deluxe or remastered edition, for example — does not satisfy the plain album;
the one exception is a folder that itself carries the plain title, which counts
even when the files inside are tagged as an edition, because that folder name is
what the write path took from the MusicBrainz title.
The sanitised key is lossy, so the reverse can also happen: two release titles
that differ only by a character it removes share one key, and holding one of them
makes the other look present, so discover skips it.
Quality is not considered: replacing lossy files remains `auto` mode's job. The
match is a whole title, so a folder that also carries a release year, the artist
name, or a format label (`2006 - Days to Come`, `Days to Come - Bonobo`,
`Days to Come [FLAC]`) is a different key from the plain title MusicBrainz
reports, and discover can place one extra copy beside it; keeping folder names
close to the MusicBrainz title avoids that.

An artist that MusicBrainz cannot resolve to exactly one candidate is recorded
for `discography.failure_cache_days`, and a later run skips the lookup for it
instead of asking again; the run summary reports those skips separately from
failures encountered in that run. An artist whose discography is already cached
is not recorded, because that cache is the better answer even once it has
expired. Running an artist explicitly with `--artist`, or pinning it in
`discography.artist_mbids`, always bypasses the recorded failure and queries
MusicBrainz; the row is cleared as soon as the artist name resolves, and pinning
an MBID clears it immediately. Only a failure to match the artist name is remembered —
an outage, and a search whose reported result count disagrees with the page it
returned, are retried every run rather than cached.

### `filters`

Controls which Soulseek search results pass the quality gate.

| Key | Description | Default |
| --- | ----------- | ------- |
| `allowed_extensions` | Only consider files with these extensions. Entries must be bare extensions of ASCII letters and digits (so `flac`, `mp3`, `m4a`); an empty list, or an entry such as `.flac` or `flac, mp3` that could never match, aborts startup. | `[flac]` |
| `min_bit_rate` | Minimum bitrate in kbps. At candidate selection, any file whose advertised bitrate is below this value is rejected, lossless included; the post-download verification is what applies to lossy files only, and it also runs when the peer omitted the bitrate attribute. `0` disables. | `0` |
| `min_bit_depth` | Minimum bit depth in bits (e.g. `16` or `24`). Lossless files whose actual bit depth is below this value are rejected (verified after download when the peer omits the attribute). `0` disables. Auto mode's upgrade scan does not read bit depth, so a library of 16-bit files is never flagged for upgrade because of this setting; it only rejects candidates whose advertised or measured depth is lower. | `0` |
| `exclude_words` | Reject files whose names contain any of these keywords (case-insensitive). | `[]` |
| `include_locked` | Include locked (private) files in search results. *(Reserved for future use — not yet enforced.)* | `false` |
| `contiguous_tracks` | Reject results with gaps in their track numbers; duplicates permitted. Numberless filenames (e.g. `track01.flac`, bare `Title.flac`) count as unnumbered — set `false` for unnumbered collections. Each disc of a multi-disc album is validated independently, so multi-disc collections keep this on. This toggle governs the gap check only; the track-1 half of the completeness rule is part of `min_tracks` and is disabled only by `min_tracks: 0`. | `true` |
| `min_tracks` | Minimum number of downloadable tracks a share must contain for its files to be considered. Rejects incomplete shares (e.g. a single track of a 16-track album). The rule has two halves and is measured on the largest album group — the set that will actually be downloaded — so a result cannot pass on files from directories that will not be fetched: the group must reach `min_tracks`, and (for a new album) its numbered files, when every name parses with at least two distinct values, must include track 1. A library-upgrade candidate is exempt from that second half, because the library holds its own track 1 and the upgrade only replaces the files that failed the quality gate. Set `0` to disable both halves. The same rule is enforced **after** download on the discover-placement and organize paths as a backstop, where a refused set has its staged files removed and the album is recorded failed rather than written into the library. See [Incomplete downloads are not written to the library](#incomplete-downloads-are-not-written-to-the-library). | `3` |
| `peer_track_count` | In auto mode, reject search results whose usable track count is below the number of library files that fail the quality gate for the same album — the album's `needs_upgrade` count, not its total track count, so a mixed-format album is compared only against the files that actually need replacing, and a fully conforming album is never flagged. Prevents silent downgrades when the library already has a more complete copy. In manual mode, when the album is already present in the library, the compared count is the number of audio files held directly by the album folder, including files that already conform, so a hand-run upgrade of a mixed-format album can be rejected by a peer that auto mode would accept; the gate is skipped entirely when that count is zero, which is the case for an album whose tracks live in per-disc sub-folders such as `CD 01/`, and for an artist folder that is not directly under a library path (a nested layout such as `<root>/Genre/Artist/Album`, where the lookup finds no tracks). Batch and discover runs have no library track count at all. Note: with the default `min_tracks: 3`, albums with 1-2 tracks (EPs, singles) are rejected by `min_tracks` before this check runs — set `min_tracks: 0` or `1` to apply the library check to EPs. | `true` |

### `download`

| Key | Description | Default |
| --- | ----------- | ------- |
| `concurrent` | Maximum simultaneous album downloads, between `1` and `8`. Defaults to `1` — the Soulseek server floods peer connections for every search result and the client library spawns a thread per peer, so higher values multiply thread usage. | `1` |
| `max_queue_length` | `0` requires a free upload slot during candidate selection; later telemetry does not retroactively reject an admitted free-slot peer. A positive value also permits zero-slot peers only when a reported positive queue position is at or below the limit. Unknown positions and wire position `0` do not prove a zero-slot peer is within the limit. | `0` |
| `max_start_time_secs` | Maximum seconds from first reaching queue position `1` until the first transfer progress. `0` disables this queue-head limit. Position reports are refreshed every 30 s while queued, so this limit arms from a fresh position-1 report. | `120` |
| `max_queue_time_secs` | Maximum total seconds from enqueue until the first transfer progress. `0` disables this total queue limit. | `1800` |
| `min_upload_speed_kbps` | Cancel a transfer whose smoothed speed (the average the progress bar displays over the transfer's own samples) is below this threshold once `speed_check_wait_secs` has passed since the transfer started. The candidate is then abandoned without re-asking the same peer — the same peer cannot get faster, and a new request only puts the file back in its queue — so the next ranked candidate is tried. `0` disables the speed check. | `250` |
| `speed_check_wait_secs` | Seconds of real transfer progress before the smoothed speed may cancel a transfer. A resumed transfer counts its offset handshake (the surviving `.part` size) as the start. | `30` |
| `timeout_secs` | Inactivity timeout — ends a transfer when no `InProgress` status arrives within this period. The clock is armed by every `InProgress` status, including the zero-byte offset handshake (so a peer that accepts and then goes silent is bounded even when both queue limits are disabled), and reset by later ones; it is not reset by a paused status. `0` disables the inactivity timeout, like the queue limits. | `180` |
| `max_download_time_mins` | Hard wallclock ceiling in minutes for a single album download session. *(Reserved for future use — not yet enforced.)* | `120` |
| `max_retries` | Per-file retry attempts on the same peer before falling back to the next candidate. `0` disables retries. At most `10`. | `4` |
| `retry_delay_secs` | Seconds to wait between retry attempts. At most `300`. | `30` |
| `min_filtered_users` | Minimum number of filtered candidates required to apply the speed check. *(Reserved for future use — not yet enforced.)* | `10` |
| `skip_retry_hours` | Cooldown in hours before re-attempting a transiently-failed album on the next run. *(Reserved for future use — not yet enforced.)* | `24` |

### `library_upgrade`

Auto-mode workflow that finds library albums failing the quality gate and re-downloads them from a
better source, replacing the existing files.

The destination is derived from the album's tags, not from the folder the album was found in, so a tag
spelling that differs from the folder name (`Guns 'n' Roses` versus `Guns N Roses`) writes into a second
artist folder and leaves the original files where they are. A name rewritten by the portable-name
sanitiser behaves the same way: an album stored by an earlier release as `Tronic Jazz: The Berlin
Sessions` is upgraded into a sibling `Tronic Jazz The Berlin Sessions` folder, because that is the name the
current sanitiser writes. `delete_lesser_quality` then walks only the folder the upgrade wrote to, so the
original lower-quality files stay behind and the album keeps its stale copy. Seakarr never renames an
existing folder for you: rename it to the sanitised name to converge. Discover mode is not affected: it
places under the on-disk artist folder.

| Key | Description | Default |
| --- | ----------- | ------- |
| `enabled` | Enable the library-upgrade workflow (auto mode only). When enabled, albums whose formats or bitrate fall below the `filters` targets are re-downloaded and their files copied into the library. Requires at least one `library.paths` entry. | `false` |
| `delete_lesser_quality` | After a successful upgrade, delete existing files in the album that are lower quality than the newly written copies (non-audio files are never deleted). The pass walks `<artist>/<album>` under the directory the album was found in (the library root itself only for a flat `<root>/Artist/Album` layout) and compares against the best written file, so two cases behave differently from the name: a pattern that writes outside that album folder leaves the old files judged against a replacement stored elsewhere, and a file kept because it scored higher than its own incoming copy can still be deleted when another track of the album scored higher. | `false` |

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

#### Library scan log lines

The library scan is the longest silent phase of a run, so it reports what it is doing. One info line
names the roots when the walk starts, an info line every minute while it runs carries the file and album
counts so far (a longer silence means the walk has not returned from the entry it is on, which a single
stalled read can also cause, so the line shows progress rather than proving health), an info line with the
final counts and elapsed
time ends it, and the count of files whose tags could not be read is included there too — those files are
also named individually at debug level, because grouping them by folder name silently is how a corrupt
file goes unnoticed. Two debug lines add detail without filling an info-level log: one every 500 audio
files, and one per unreadable file.

On an interactive terminal the per-minute line is replaced by a spinner that
updates a single line in place — `Scanning library: 4948 audio file(s), 408
album(s) (60s elapsed)` — carrying the same counts and elapsed time, so a long
scan shows movement without scrolling. The counts come from the walk itself, so
a stalled read freezes them while the spinner keeps ticking, exactly as the
per-minute line does today. The per-minute line is still written to the log file
while the spinner is live, and a headless run (no terminal attached) keeps the
log lines unchanged. The starting, complete and cancelled lines still reach both
the console and the file.

#### Download log lines

Every completed album produces one completion line naming its final destination:

```text
INFO seakarr::runner: Completed: Aquasky - Shadow Era Pt. 1 (8 tracks) -> /media/Music/Paul/Albums/Aquasky/Shadow Era Pt. 1
```

The path is the album folder the write actually produced, after name sanitisation and disc
subdirectory handling — not the raw `storage.organize_pattern`. When `storage.organize` is
`false`, or `library.paths` is empty, the album stays where it was downloaded and the line
says so:

```text
INFO seakarr::runner: Completed: Aquasky - Shadow Era Pt. 2 (6 tracks) -> /downloads/Aquasky--Shadow Era Pt. 2 (kept in staging)
```

The per-file line that appears as each track finishes reports the **staging** location, and is
labelled accordingly so it cannot be mistaken for the final destination:

```text
INFO seakarr::download: Download staged: 08 Moondance.flac -> /downloads/Aquasky--Shadow Era Pt. 1/08 Moondance.flac
```

The same destination appears in the end-of-run summary and in the `message` of each success
notification. Per-file library destinations are available at `DEBUG`:

```text
DEBUG seakarr::organizer: Organized: /downloads/Aquasky--Shadow Era Pt. 1/08 Moondance.flac -> /media/Music/Paul/Albums/Aquasky/Shadow Era Pt. 1/08 - Moondance.flac
```

A download that waits in a peer's upload queue reports its position in the same line that
announces it, and reports how long it waited when it finally starts:

```text
INFO seakarr::download: Download queued: 08 Moondance.flac from nottucks - position 42
INFO seakarr::download: Download started: 08 Moondance.flac from nottucks after 21m 40s queued (last position 3)
```

The queue line is emitted once the peer reports a position — normally within a fraction of a
second — or at the latest five seconds after the request, or when a queue limit ends the wait.
A peer that never answers still produces the line, without a position, even when
`max_queue_time_secs` is shorter than five seconds and the queue limit expires first — unless the
attempt ends first, in which case its own warning (cancellation, a closed status channel, or a
peer-reported failure) is what the log shows. A candidate admitted on an advertised free slot that
starts transferring at once — within one poll window — reads:

```text
INFO seakarr::download: Download queued: 08 Moondance.flac from nottucks
INFO seakarr::download: Download started: 08 Moondance.flac from nottucks immediately (free slot)
```

Positions are deliberately **not** logged as a live counter: a queue 100 deep would produce
100 lines. Every position change is instead available at `DEBUG`, and in an interactive
terminal a queue bar shows the current position in place, so a long wait costs one terminal
line:

```text
DEBUG seakarr::download: Queue position for 08 Moondance.flac from nottucks: 9
```

While the file waits, seakarr re-asks the peer for its position every 30 seconds, so that
number keeps up with the queue instead of freezing at the first report. The refresh stops as
soon as the transfer starts.

A peer that never reports a position and does not start promptly is reported with the wait it
actually served (`... after 12s queued`), so `immediately` is never printed for a transfer that
demonstrably waited.

Queue timeouts and rejections keep reporting the position in their existing warnings.

#### Incomplete downloads are not written to the library

The same completeness rule runs twice, on one shared implementation
(`filter::incomplete_download`, which classifies the refusal so neither site re-derives half of it
and drifts). Before the download, the filter judges the set that will actually be fetched — the
largest album group, the directory (or collapsed multi-disc set) that `download_album` really
downloads. For a **new** album it rejects a result that could never be placed, so the transfer is
not started at all; a **library-upgrade** candidate is judged by the count half alone, because the
library already holds its own track 1 and the upgrade only replaces the files that failed the quality
gate. The library write then applies the rule again as a defensive backstop. Both sites see the same
set by construction: the identical quality filter and the identical grouping feed both, so the
backstop cannot fire for a set the filter approved, and no operator should expect its warning.

What the rule catches is a result that would otherwise be downloaded for nothing. Counting the whole
result instead of the group is the routine case: an album whose title is also one of its track titles
makes peers' copies of the *track* match the query by filename, so the result counts plenty of files
while the largest album group is that one track. Contiguous numbering alone does not make an album
either, so a set of tracks nine and ten is a fragment of something longer however tight its numbering
looks.

Such a result is rejected before the download, and the rejection appears in the run's rejection
summary:

```text
INFO seakarr::runner: Cyantific — Archive 1: 167 files from 18 users, 0 passed filters (need: ["flac"] format, free slot or queue position <= 100, contiguous track numbers)
  → rejected: 1 missing track 1
```

A group that is too short reports the count half instead (`→ rejected: 1 below min track`). The
per-result reason (`only 3 of at least 5 tracks in the largest album group`) is logged at DEBUG for
the rejected peer, so it takes `--log-level DEBUG` to see it.

Should a refusal ever happen after the download instead — the backstop above, or the library-upgrade
path's own `expected_tracks` gate — the album is recorded failed and its staged files are removed with
it, so a refusal leaves nothing of its own in `storage.staging_dir`. A directory that still
holds an entry this run did not stage (another album on the same `artist--album` staging name, or
leftovers from an earlier run) is left in place, with a warning naming the first such entry — a file
or a subdirectory. Ownership is by path, so a foreign file that lands on the same path as one of
ours goes with ours; the two cannot be told apart. The serving peer is
recorded as an album failure too. The demotion is real but light: the tracks that peer did deliver are
already credited as successes for the same download, so the album-level failure only tips the balance
against a peer that has a positive history.

The library-upgrade path's refusal logs its own wording:

```text
WARN seakarr::runner: Aquasky - Shadow Era Pt. 1: download incomplete (3/6 tracks), skipping library upgrade
```

Two conditions make up the rule. The anchor is part of it rather than part of the
`contiguous_tracks` toggle — turning the gap check off for an unnumbered collection does not
disable the anchor — and only `min_tracks: 0` disables both halves. The anchor is skipped for
library-upgrade candidates, which only have to deliver the files the library needs replaced.

- **the set is shorter than `min_tracks`** — measured on the largest album group, the set that will
  actually be downloaded, and applied in every mode; and
- **its numbered files, if any, do not include track 1** — a set of tracks nine and ten is a
  fragment however contiguous it looks. This half applies to new albums; an upgrade candidate is
  exempt, because the library keeps its own track 1.

Both conditions are checked in the filter before the download, and again by the library write after
it, on the discover-placement and organize paths. Because the anchor reads the *first* numeric token
of each name, a compilation whose files lead with a varying number (`2 Unlimited - 01 - ...`,
`3 Doors Down - 02 - ...`) is judged on those phantom numbers, and can be refused by the anchor
when no name parses to a track 1. That class is not new to the library write, which always applied
this rule, but it is now also refused before the download in every mode.

The numbering half is deliberately cautious, and misses several real fragments as a result. It
judges a set only when **every** downloaded file parsed a number, so a mixed set — an `Intro.flac`
beside `02 - Two.flac` — is left alone. It also requires at least **two distinct** parsed values,
because `track_number_from_filename` reads the *first* numeric token: every file of
`Blink 182 - Enema of the State - 01 - Dumpweed.flac` reports track 182, and a single repeated
value is indistinguishable from a set that genuinely repeats one track number, which the project
supports. A lone numbered file is not judged either, since one value can never be distinct.

So a fragment that includes track 1, or that repeats one number, or that mixes numbered and
unnumbered names, can still be written when it is at least `min_tracks` long — the gate has no
notion of the album's true length. Raise `min_tracks` to narrow that; the count half is the one
that catches every short set regardless of its names.

Track numbers are read as tracks, not as fragments, when they are `1` or when their last two
digits are `01`: a rip that fuses the disc and track number (`101` for disc 1 track 1) counts as
starting at track 1, just as the hyphenated `1-01` form does.

Note that the count half also applies to genuine EPs and singles: an album shorter than
`min_tracks` is refused in the filter, before any download, in every mode — the target is not
consulted first. Auto mode with `library_upgrade.enabled: false` (the shipped default) needs
`min_tracks: 0` or `1` to replace a short album rather than refusing it every cycle. With
`library_upgrade.enabled: true` and `min_tracks` low enough to admit the album, the library-upgrade
path's own `needs_upgrade` reference decides the copy after the download, and that path may be
served by a peer sharing only the non-conforming files.

### `pid`

| Key | Description | Default |
| --- | ----------- | ------- |
| `path` | Directory for the PID file (`seakarr.pid` is created inside). Overridden by `--pid-path`. | `pids` |
| `file` | PID filename. | `seakarr.pid` |

### `notifications`

| Key | Description | Default |
| --- | ----------- | ------- |
| `urls` | List of webhook endpoints. A success notification is POSTed to each one for every completed album, as JSON `{title, message, type}`. Only `http`/`https` URLs are delivered; Apprise-style scheme URLs (`ntfy://`, `discord://`) are accepted but never delivered, and the failure is logged as a warning. Leave empty to disable. | `[]` |

The endpoint must be `http` or `https` and must accept a JSON body. An Apprise API server works: point the
URL at its `/notify` endpoint. Bare Apprise scheme URLs such as `ntfy://my-topic` or
`discord://webhook-id/webhook-token` are parsed as configuration but cannot be delivered, because the
notification is a plain HTTP POST; use each service's HTTP webhook URL instead.

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

Seakarr has four operating modes:

### Automatic mode (default)

1. **Scan** — walks every path in `library.paths`, reads audio tags via `lofty`, and groups tracks by
   artist and album. Prefers tag metadata over directory names. Folder-derived artist and album names
   assume UTF-8 path components: a non-UTF-8 component is skipped, which shifts the derived names for
   that album, so keep library folder names in UTF-8. The scan reports its progress (see the library scan
   log lines above) and can be stopped with Ctrl+C, which aborts the run before any album is processed.
2. **Detect upgrades** — for each album, checks whether any track is in a non-allowed format or below
   `min_bit_rate`. When `min_bit_rate` is set, an album whose files all report no bitrate is flagged too,
   because its quality cannot be verified.
3. **Search** — queries the Soulseek network for each flagged album.
4. **Filter & rank** — filters results by extension, bitrate, excluded words,
   and upload availability: with `download.max_queue_length: 0` a peer must
   offer a free upload slot, while a positive limit also admits zero-slot peers
   whose queue position is validated during download. When
   `filters.contiguous_tracks` is enabled, results whose downloadable track
   numbers have gaps (or none at all) are rejected before ranking. The
   completeness rule (`filters.min_tracks`) also runs here, in every mode: it is
   measured on the largest album group — the set that would actually be
   downloaded — and it rejects a group that is too short or whose credible
   numbering never reaches track 1, so a result that could never be placed is
   rejected before the transfer rather than after it.
   Ranks candidates by `speed × slot_bonus × bitrate_bonus × album_bonus × reliability_factor` (reliability and
   measured-speed reputation adjust the advertised speed; `album_bonus` is 1.5 when the peer's folder matches the
   album name, 1.1 when the name appears elsewhere in the path, 1.0 otherwise).
5. **Download** — downloads from the highest-ranked peer, monitoring transfer
   speed in real time. While a transfer waits in a remote queue, seakarr asks
   the peer for its position immediately, then every 30 seconds until the
   transfer starts.
   `max_queue_time_secs` caps the total wait from enqueue and
   `max_start_time_secs` caps the wait after reaching queue position `1`; a
   peer that exceeds either limit is abandoned and the next candidate is tried
   without retrying the same peer. Once the peer accepts the transfer,
   `timeout_secs` guards against inactivity (the per-album stall timeout for
   unresponsive peers). If the smoothed speed at least `speed_check_wait_secs`
   after the transfer started is below `min_upload_speed_kbps`, the transfer is
   cancelled and the candidate is abandoned — re-asking the same peer only puts
   the file back in its queue — so the next candidate is tried without spending
   the per-file retries. Per-file retries with configurable count and delay
   (`max_retries`, `retry_delay_secs`) re-attempt the same peer for transient
   failures before falling back to the next candidate.
6. **Organise** — if `storage.organize` is enabled, completed files are moved from the staging directory
   into the library using the configured naming pattern. Duplicate filenames receive a `(1)` suffix.
7. **Persist & notify** — the album is marked as processed in SQLite and the success payload is POSTed to each
   configured `notifications.urls` webhook (if any).

### Manual mode

Performs steps 3–7 above for an explicit album, or for every eligible album
resolved from an artist-only MusicBrainz discography lookup. Each resolved album
then runs through the normal targeted `Artist Album` search, sequentially and
oldest first; editions and remasters of the same conceptual album are
deduplicated before searching. At least one target is required; CLI values take
precedence over `search.manual.artist` and `search.manual.album`, and album-only
searches are supported. Artist-only manual runs also skip every album that is
already present in the library, so `--artist X` fetches only what you are
missing rather than re-downloading albums you already own.

Explicit artist-plus-album and album-only manual searches, batch mode, automatic
mode, and the library-upgrade workflow are unchanged and never consult
MusicBrainz. The legacy single-query folder heuristic is retained as the
`discography.enabled: false` opt-out and as an automatic fallback when
authoritative discovery cannot be established; automatic fallback is reported
with a WARN and a run-summary notice.

### Batch mode

Reads a newline-separated text file of `artist - album` lines and performs steps 3–7 for each line. Reports
success and failure counts on completion. Lines starting with `#` are treated as comments.

### Discover mode

Fills in what an existing library is missing. Seakarr walks `library.paths` once and derives
the artist list from tags where present, falling back to folder names, keeping only the artists
that own a folder named after them. Each swept artist is resolved
on MusicBrainz using the same authoritative resolution as artist-only manual mode, and the
release groups in `discography.allowed_types` are the only candidates. Albums already present
in the library are skipped, and the remainder runs through the same search, ranking,
download, organisation, and notification pipeline as the other modes, oldest album first.
`discover.exclude_artists` skips aggregator names before any lookup, the
folder-ownership gate keeps an artist the library knows only as a guest inside
another artist's folder out of the sweep, `--artist` optionally
names one artist already in the library to process on its own (overriding both
the exclusion list and the gate), and the run stops after
`discover.max_cycle_downloads` download attempts so a large library fills in over successive
runs.

Each completed album is placed in the artist's own library folder — the directory that artist's
existing albums were scanned from — using `storage.organize_pattern`, with `%artist%` expanded to the
artist folder name that is actually on disk and `%album%` to the MusicBrainz title. Placement is
unconditional in discover mode: it does not depend on `storage.organize` or on
`library_upgrade.enabled`, and `library_upgrade.delete_lesser_quality` never applies to it. Once the
copy succeeds the staging directory for that album is removed, so discover leaves nothing behind.
The scanner resolves the artist folder, album folder, and library location positionally and steps
over one dedicated disc folder (`CD 01`, `Disc 2`), so nested layouts such as
`<root>/Genre/Artist/Album` and albums whose discs sit in dedicated disc folders place correctly. The
album folder must hold the files itself: for an untagged `<root>/Artist/Album/FLAC/01.flac` the
sub-folder is read as the album (`Album` becomes the artist and `FLAC` the album), so keep format or
extra sub-folders outside the album folder.
An album split across marker-shaped folders (`Gold (Disc 1)/`, `Gold (Disc 2)/` under one album
folder) reads as two albums and is documented as a limit rather than corrected; the derived artist
folder is then the marker folder's parent (`Gold/`), which also feeds the discover destination pair
when those are the artist's only albums. Such an artist owns no folder named after the tag
spelling, so the folder gate keeps it out of an automatic sweep; `--artist` processes it. A placed
album that keeps a marker folder is therefore
indexed under the marker name, not the album title, so a later run with a cleared database or
`--ignore-processed` can download it again. On the upgrade path the same shape can also nest one
level too deep, giving `<artist>/Gold (Disc 1)/Gold (Disc 1)/...`, because the disc folder is
preserved under an album whose own name is that marker. Only discover mode
places albums beside the artist's folder: an artist-only manual run (`--mode manual --artist "Name"`) does not, so with
`storage.organize: true` it organizes its downloads under `library.paths[0]` and with `organize` off they stay in
`staging_dir`.

### Scheduled mode

When `--schedule` or `schedule.enabled` is set, the same validated auto, manual, batch, or discover
plan runs immediately in a foreground loop. After each cycle completes, seakarr waits for
`schedule.interval_mins` before dispatching that unchanged plan again. SIGTERM received at
any time stops the scheduler after the active cycle and removes the PID file. Ctrl+C during
the library scan aborts the scan and the run before any work item; Ctrl+C during
an active cycle requests cancellation; Ctrl+C while waiting between cycles stops the
scheduler and removes the PID file. A second Ctrl+C during an active run forces immediate exit with
code 130 and leaves the PID file behind, which the next run treats as a stale lock and removes; while
the scheduler is waiting between cycles a press is that graceful shutdown, removes the PID file and
exits 0.

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

**Q: What happens when the Soulseek session drops?**

A dropped connection is reconnected transparently, using `soulseek.login_retries` and
`soulseek.login_retry_delay_secs` for the login attempts. A reconnect that fails for a transient reason
is retried after a short cooldown; one rejected because the credentials are no longer accepted is not
retried at all, so fixing the credentials and restarting is required. A session displaced by another
login with the same username is different again: seakarr reports the takeover and stops using that
session rather than reconnecting, because logging in again would evict the other instance.

**Q: Why do some albums fail with "no results passed filters"?**

`filters.contiguous_tracks` (default `true`) rejects search results whose track numbers have gaps,
and results with no parseable track numbers at all. Shares numbered like `track01.flac` (digits
fused to letters) or without numbers are treated as unnumbered. If your collection uses such
naming, set `filters.contiguous_tracks: false` in `seakarr.yml`.

These albums appear in the "Failed" section of the run summary with the reason
"no results passed filters". Albums where every search tier came back empty appear with the reason
"no results found" instead, and that includes an album whose only results came from the track-title fallback
and were all rejected (the rejection summary is written to the log). Both are retried on subsequent runs.

Two known limitations of the heuristic. (1) The
first number in the filename wins, so artist names containing digits (`Maroon 5`, `50 Cent`,
`Blink 182`) can be read as the track number instead — every file in such a share then yields the same
number, which the duplicate-tolerant check accepts, so a share with real gaps can pass. Disabling
`contiguous_tracks` does not help there: the verdict is already a pass. The exception is a hyphenated
disc-track name such as `1-11 - Title.flac`, where the second number is the track and the disc number is
dropped from the written name; a share that writes the disc and track numbers apart (`2 - 05 - Title.flac`)
is read by the first-number rule instead. (2) A gap inside any one disc
is rejected, because each disc's tracks are validated independently, but a share that omits an entire
disc is not detected by this heuristic — a peer advertising only `CD 01` forms one contiguous group,
so a multi-disc album can be completed from it and recorded as present. Turning the gate off does not
help there: it is a false acceptance, so disabling the check only accepts more shares. In auto mode,
and in manual mode when the album is already in the library, the peer track-count gate is what rejects
a peer that supplies fewer tracks than the album needs; discover and batch runs have no such baseline,
because neither derives a library track count for the album.

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
names. Files without readable tags fall back to the directory naming convention; a file that cannot be
read at all is counted in the scan's completion line and named at debug level, so it does not disappear
from the report while still being grouped by folder name.

**Q: What formats can seakarr scan?**

The extensions the scanner recognises, all readable by the [lofty](https://crates.io/crates/lofty)
crate: FLAC, MP3, M4A/AAC, OGG/OGA, Opus, WAV, WMA, APE, MPC, WavPack, AIFF, ALAC, DSF/DFF and Speex. A
file whose extension is not in that list is skipped even if lofty could read it, so an unusual container
(say `.mp4` or `.m4b`) is invisible to the scanner. Bitrate and format information is extracted from file
headers and tags.

**Q: What formats will seakarr download?**

The `filters.allowed_extensions` config key controls which formats pass the quality gate. By default only
`flac` is accepted — MP3s and other lossy formats are excluded. Set it to `[flac, mp3]` to allow both.

**Q: Can I download from queued peers instead of only free-slot peers?**

Yes. Keep `download.max_queue_length: 0` for free-slot-only behavior, or set a
positive limit to try zero-slot peers whose reported positive queue position
is within that bound. Unknown and out-of-bound positions fail closed and move
to the next candidate without retrying the same peer. A wire position of `0`
means the peer has no actionable queue entry; it is treated as unknown and
never proves a zero-slot peer eligible. With `max_queue_length: 0`, a peer
admitted with an advertised free slot is not retroactively rejected by later
telemetry; a positive limit applies the position check to every candidate.
`max_queue_time_secs`
limits total queue wait, `max_start_time_secs` limits the wait after reaching
position `1`, and `timeout_secs` controls inactivity from the peer's accept (its
first `InProgress`, including the zero-byte offset handshake) onwards. With the
defaults, a peer that never accepts is bounded only by
`max_queue_time_secs: 1800`, while one that accepts and then goes silent is
bounded by `timeout_secs: 180`.

**Q: How do I prevent seakarr from downloading files with certain words in the filename?**

Add entries to `filters.exclude_words` — for example, `[vinyl, demo, live]` will reject any file whose
name contains "vinyl", "demo", or "live" (case-insensitive).

**Q: What happens if my staging directory and library are on different filesystems?**

`fs::rename` cannot move a file across mount points, so seakarr detects the cross-device error and falls
back to copying the file into the library and then deleting the staging copy. The organized file lands in
the correct library location regardless of filesystem layout.

**Q: Can I run multiple instances of seakarr at once?**

No — the PID lock prevents concurrent runs. If a second instance starts, it detects the existing PID file and
checks whether that process is still alive. A PID that is running aborts with an error; a PID whose liveness can
be established and that is no longer running is treated as a stale lock and replaced automatically, so the run
continues. Delete the PID file by hand when its contents cannot be read, or when liveness cannot be determined —
that happens when the liveness probe itself fails or its message is not recognised, and the error names the file
to delete.

**Q: How does artist-only manual mode choose which albums to download?**

Artist-only manual runs resolve the artist on MusicBrainz and process conceptual release groups rather than
arbitrary Soulseek folders:

- Automatic resolution accepts only candidates whose canonical MusicBrainz name matches exactly. Matching uses
  Unicode NFKC normalization, lowercase conversion, trimming, and whitespace collapse, but punctuation stays
  significant, so `AC/DC` and `AC DC` are distinct. One exact match is used directly. When several canonical
  exact names match, seakarr selects one only when it is uniquely dominant: a MusicBrainz search score of 100
  with a lead of at least 10 points over the runner-up. Tied top scores, a score below 100, and a margin below
  10 stay unresolved and fall back as described below. A missing score, and invalid, out-of-range or fractional
  score data, are treated as unusable provider data instead: the refresh is rejected and the same
  stale-cache/legacy fallback follows, and in discover mode the artist counts towards the consecutive
  provider-failure breaker. Aliases, sort names, artist
  tags, and catalog size never participate. Use `discography.artist_mbids` to pin an ambiguous name to a MusicBrainz
  artist UUID; a configured ID always takes precedence over name search.
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
- Each eligible album runs through the existing targeted search cascade sequentially; an album whose run ends in a
  failed download or no usable candidate does not block the remaining albums, and cancellation stops scheduling later
  ones. A search-stage error (a lost session, for example) does abort the rest of the artist's albums, because the
  client state it fails on would affect every later search too.

**Q: What happens when MusicBrainz cannot be reached or returns nothing?**

Fallback precedence is fresh cache, a freshly recorded resolution failure (unless `--artist` or a pinned MBID
bypasses it), successful refresh, stale cache, then the legacy heuristic. The last
step applies to artist-only manual mode; discover mode instead reports the artist as skipped and aborts the
run after repeated provider failures (see the discover answer below), and `discography.enabled: false` is a
configuration error for discover.

- A cached discography is fresh for `discography.cache_days` complete 24-hour periods (30 by default). `0`
  attempts a refresh on every run but still keeps the cached copy as a stale fallback. Boundary equality is
  stale, and a clock rollback or future timestamp is stale too.
- A successful refresh replaces the cached copy atomically. The raw release groups are re-filtered against the
  current `allowed_types` on every run, so changing categories applies immediately without re-downloading.
- A release group whose title names several other release groups joined by a spaced slash is not
  selected when every part matches one of that artist's release-group titles exactly, after case,
  width and whitespace folding. Those parts are searched under their own titles instead of the
  concatenation, which returned no results, and a part is only searched when `allowed_types` also
  selects its own release group. A title whose parts are not spelled exactly like the artist's
  release-group titles is searched as MusicBrainz spells it, so a disc-prefixed box set such as
  `2cd: Highway 61 Revisited / Blonde on Blonde` is unchanged: the prefix means the first part
  matches nothing, even though the artist does have `Highway 61 Revisited`.
- If a refresh fails, seakarr warns with the cache age and the failure reason and processes the compatible
  stale cache instead.
- If no compatible cache exists, seakarr logs a prominent WARN, records a run-summary notice naming the exact
  reason, and uses the legacy single-query folder heuristic; the notice states that album names were discovered
  heuristically from Soulseek folders. This is the artist-only manual behaviour: discover never falls back to the
  heuristic, it warns, records the artist as skipped or provider-failed, and stops the run after
  `DISCOVER_PROVIDER_FAILURE_LIMIT` consecutive provider failures.
- A valid authoritative empty result — zero release groups, or zero albums left after `allowed_types`
  filtering — is not an error: no Soulseek search runs, there is no heuristic fallback, and the run summary
  shows a neutral notice instead of a failed or skipped album.
- Set `discography.enabled: false` to select the legacy heuristic deliberately in artist-only manual mode. That
  choice is logged as an explicit configuration choice without an outage warning. Discover mode requires a
  configured discography and refuses to start without it.

The legacy heuristic normalizes common folder-name variations so one release is
processed once. It strips explicit artist prefixes (including an abbreviated
`K+D` prefix before a bare-year title), leading, separator-enclosed, and full
ISO release years (`2006 - Days To Come`, `Album - 2006 - Title`,
`2006-10-02 - Days To Come`, `Days to Come (2006)`), the structural word
`Album`, a trailing artist name (`Days to Come - Bonobo`), punctuation, common
Latin variants and non-decomposable Latin letters (`ø`, `đ`, `þ`), trademark
marks, audio-format suffixes, and folder disc markers. Release years, format
labels, `TM` marks, and a trailing artist name are stripped repeatedly, so
their order in the folder name does not matter, and a self-titled folder keeps
the artist identity (`Kruder & Dorfmeister 1998` and
`Kruder & Dorfmeister FLAC` both reduce to the artist).

Bracketed annotations are stripped only when they carry no edition meaning:
catalog codes, source labels, and disc notes (`[ZENCD119, flac]`,
`(bonus disc)`) collapse into the base release, while annotations naming an
edition (`(Deluxe Edition)`, `(Remastered)`, `(Live)`, `(Instrumental)`,
`(Demo)`, `(Remixes)`, `(Limited Edition)`, `(Part 2)`) stay distinct releases.

Two conservatisms remain: a separator-less prefix for a single-word artist
stays significant (`Nirvana Nevermind` differs from `Nevermind`) to avoid
collapsing titles such as `Doors Open` into `Open`, and a trailing bare year
stays significant to avoid merging `Blade Runner` with `Blade Runner 2049`.
This is a conservative text heuristic, not edition metadata: genuinely distinct
releases whose names differ only by a stripped marker can still collapse into
one candidate.

For legacy artist-only discovery, folder-marked single-disc peers are rejected
when another candidate advertises a larger, self-consistent disc set. If every
marked candidate contains only one of several observed discs, the album fails
with `multi-disc album is split across peers; no complete candidate available`
and is retried later rather than recorded as a partial success. Flat folders,
filename-only disc numbering, explicit artist-plus-album runs, and batch runs
are outside this split-disc check. Success rows created by older versions under
a single-disc album label are ignored by normalized history matching; rerun the
album to replace that partial result.

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

**Q: What happens when MusicBrainz cannot resolve an artist from my library?**

In `discover` mode the artist is skipped for that run and reported in the run
summary: no broad Soulseek artist search is issued and no folder-derived
discovery is attempted, so albums are never downloaded from an unverified
identity. `discography.artist_mbids` is the supported fix for a name MusicBrainz
cannot resolve on its own. Three consecutive MusicBrainz request failures abort
the run instead, so a provider outage cannot grind through the rest of the
library.

**Q: How do I fill in missing albums for one artist only?**

Run `--mode discover --artist "Artist Name"`. The name must already exist in
your library; discover derives the artist list from your library and never adds
an artist you do not have. Naming one explicitly also bypasses the
folder-ownership gate, which is how you reach an artist whose albums sit in a
folder named after somebody else. To fetch a specific album instead, use
`--mode manual --artist "Artist Name" --album "Album Title"`.

**Q: Where do discover downloads end up?**

In the artist's own library folder, beside the albums that artist already has. Discover derives the
destination from the same scan that produces its artist list, so a nested library such as
`Music/<user>/Albums/<genre>/<style>/<artist>/` keeps its layout instead of writing a second artist
tree under `library.paths[0]`. The folder itself comes from `%artist%` in `storage.organize_pattern`, expanded to the on-disk artist folder name, so a pattern that omits `%artist%` writes beside that folder — under its parent directory, which is the library root only in a flat `<root>/Artist/Album` layout — instead of inside it. Likewise, a pattern that keeps `%artist%` but omits `%album%` (for example `%artist%/%track% - %title%.%ext%`) writes every album directly into the artist's folder, so tracks from different albums can sit side by side there; keep `%album%` in the pattern for that reason. The staging copy is deleted once the album is placed. Placement never replaces a file
that already parses as audio, because the album folder may belong to a different edition of the album — only a file
that cannot be opened at all (junk bytes, an empty file, an unrelated format) is replaced. The check reads the metadata
header, so a copy interrupted after that header still parses: it is kept as it stands and the incoming file for that
track is not written, while the tracks with no destination are copied normally. Whenever fewer files were written than
downloaded — whether every destination was kept or only some were — the run warns with both counts and still completes
the album as placed, so a later run does not download it again.
A placement failure (a read-only or otherwise blocked
destination) keeps the staging copy, records the album as failed, and counts against
`discover.max_cycle_downloads`. The album is retried when no audio file reached the folder; once any file landed the
album counts as present, because the presence check accepts the album folder name the placement wrote as well as the
embedded album tag, and accepts the artist folder name as well as the tag spelling. A peer whose album tag or artist
tag is spelled differently from the MusicBrainz title therefore cannot make a later run re-download an album seakarr
placed itself, and the `--ignore-processed` override cannot reach it either: both sides of the lookup cover both
spellings. This holds for a pattern that keeps `%album%`; a pattern that omits it writes every album straight into the
artist folder (see the pattern note above), where no album folder exists for the scan to match. Those albums are
invisible to the scan instead of merely re-downloaded: an artist whose albums are all written flat has no index entry
and drops out of the work list altogether, while an artist that also has albums in folders keeps its entry and its flat
albums are searched and kept again on every run, whatever their tags say. The processed-album record still suppresses
the album on later runs. Note that
`--artist` narrowing still keys on the tag spelling, and the artist work list now also requires the
artist to own a folder of its own, so a name spelled
differently on disk than in its tags is skipped by the sweep and has to be named with `--artist`.
Case, spacing and width differences fold, so only a genuinely different spelling is skipped. Note that a later auto run with
`library_upgrade.enabled` removes every leftover staging
directory whose album is not recorded as successful, so a retained copy is a short-lived safeguard rather than a
permanent one.

**Q: Can I run auto mode and discover mode on a schedule at the same time?**

No. seakarr holds a single PID lock and a single Soulseek session, and a second
login with the same username displaces the running instance. A scheduled
instance performs one job: either upgrading what you have (`--mode auto`) or
filling gaps (`--mode discover`). Alternating between them across separate runs
is the supported approach.

___
If you appreciate my work, then please consider buying me a beer  :D

[![PayPal donation](https://www.paypal.com/en_US/i/btn/btn_donate_SM.gif)](https://www.paypal.com/cgi-bin/webscr?cmd=_s-xclick&hosted_button_id=MM5E27UX6AUU4)
