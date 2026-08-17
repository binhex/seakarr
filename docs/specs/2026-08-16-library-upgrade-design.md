# Library Upgrade — Design Spec

**Date**: 2026-08-16
**Status**: Approved (pending implementation)
**Scope**: Auto mode only — after a complete album download, copy files from staging to the origin library directory, delete lesser-quality existing audio files, clean up staging.

---

## 1. Overview

The primary aim of seakarr is to upgrade a music library in a fully automated fashion. Currently, downloaded albums land in the staging directory (`downloads/staging/`) and the user must manually move them into the library. This feature closes that gap: when an album download completes in auto mode, seakarr copies the files into the library directory where the album was found, replacing any lesser-quality audio files while preserving non-audio metadata (images, logs, cuesheets, etc.).

**Key design decisions**:
- **Completeness gate**: All filtered tracks from the best peer must have downloaded successfully. Partial downloads stay in staging.
- **Target path**: Match the origin library path from scanner metadata (threaded from `run_auto_mode` through `process_album`).
- **Quality rule**: Format priority — FLAC > all lossy formats. Within the same format, compare bitrate/bitdepth.
- **Deletion**: Permanent delete of lesser-quality audio files. Non-audio files (jpg, png, nfo, log, txt, cue, m3u) are never deleted.
- **Robustness**: Copy (not move) from staging to library. Old files deleted only after all copies succeed. Staging dir existence is the recovery signal.
- **Mode**: Auto mode only. Manual/batch mode falls back to existing `storage.organize` behavior.

---

## 2. Config

New top-level section in `seakarr.yml`:

```yaml
library_upgrade:
  enabled: true               # Enable library upgrade after complete download
  delete_lesser_quality: true  # Delete existing audio files with worse quality
```

**Defaults**: Both `false` — opt-in behavior.

**Validation**:
- If `library_upgrade.enabled: true` and `library.paths` is empty → config validation error at startup.
- If `delete_lesser_quality: true` but `enabled: false` → log warning, delete flag ignored.

**Relationship to existing `storage.organize`**:
- When `library_upgrade.enabled: true` in auto mode: the library upgrade supersedes the organize step. The `organize_pattern` is still used for naming files in the library.
- When `library_upgrade.enabled: false`: existing `storage.organize` behavior is unchanged.
- Manual/batch mode: always uses existing `storage.organize` behavior regardless of `library_upgrade`.

**Config structs** (`src/config.rs`):

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LibraryUpgradeConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub delete_lesser_quality: bool,
}
```

Added to `Config` as `pub library_upgrade: LibraryUpgradeConfig` with `#[serde(default)]`.

---

## 3. Data Flow — Threading Library Path

Currently `run_auto_mode` scans library paths and finds albums, but `process_album` doesn't receive the origin path. The fix:

### 3.1 `process_album` signature change

Add `target_library_path: Option<&Path>` parameter:

```rust
pub async fn process_album(
    client: &dyn SoulseekClient,
    artist: &str,
    album: Option<&str>,
    config: &Config,
    db: &Database,
    staging_dir: &Path,
    progress: Option<&ProgressDisplay>,
    cancel: Option<&Arc<AtomicBool>>,
    library_track_count: Option<usize>,
    target_library_path: Option<&Path>,  // NEW
) -> Result<AlbumOutcome>
```

### 3.2 Threading from `run_auto_mode`

The scanner returns `AlbumInfo` with artist, album, and origin path. For each album to upgrade:

```rust
process_album(
    client,
    &album_info.artist,
    Some(&album_info.album),
    config,
    db,
    staging_dir,
    progress.as_deref(),
    Some(&cancel),
    Some(album_info.track_count),
    Some(Path::new(&album_info.library_path)),  // origin path
)
```

### 3.3 Threading from manual/batch mode

Manual and batch mode pass `None` as `target_library_path`, so library upgrade is skipped and existing organize behavior applies.

---

## 4. Core Logic — Library Upgrade Flow

After `download_album` returns `downloaded: Vec<PathBuf>`:

### Step 1 — Completeness gate

```rust
if config.library_upgrade.enabled {
    if let Some(target_path) = target_library_path {
        // Check completeness: all filtered tracks must have downloaded
        let expected_count = filtered.iter().map(|r| r.files.len()).sum::<usize>();
        if downloaded.len() < expected_count {
            tracing::warn!(
                "{artist} — {}: download incomplete ({}/{} tracks), skipping library upgrade",
                album.unwrap_or("?"),
                downloaded.len(),
                expected_count,
            );
            // Keep staging dir, mark as failed
            return Ok(AlbumOutcome::Failed {
                reason: "incomplete download, library upgrade skipped".into(),
            });
        }
        // Proceed with library upgrade
        run_library_upgrade(config, artist, album, &downloaded, target_path)?;
    }
}
```

### Step 2 — Copy to library

```rust
fn run_library_upgrade(
    config: &Config,
    artist: &str,
    album: Option<&str>,
    downloaded: &[PathBuf],
    target_library_path: &Path,
) -> Result<()> {
    let album_dir = target_library_path
        .join(sanitize_component(artist))
        .join(sanitize_component(album.unwrap_or("Unknown")));

    fs::create_dir_all(&album_dir)?;

    // Copy each downloaded file to the library directory
    for src in downloaded {
        let filename = src.file_name().unwrap();
        let dest = album_dir.join(filename);
        fs::copy(src, &dest)?;
    }

    // Delete lesser-quality audio files (if enabled)
    if config.library_upgrade.delete_lesser_quality {
        delete_lesser_quality_files(&album_dir, downloaded)?;
    }

    Ok(())
}
```

### Step 3 — Delete lesser-quality audio files

```rust
fn delete_lesser_quality_files(
    album_dir: &Path,
    new_files: &[PathBuf],
) -> Result<u32> {
    let new_filenames: HashSet<&str> = new_files
        .iter()
        .filter_map(|p| p.file_name().and_then(|n| n.to_str()))
        .collect();

    let mut deleted = 0;
    for entry in fs::read_dir(album_dir)? {
        let entry = entry?;
        let path = entry.path();

        // Skip non-audio files
        if !is_audio_file(&path) {
            continue;
        }

        // Skip files we just copied
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            if new_filenames.contains(name) {
                continue;
            }
        }

        // Compare quality: if existing file is worse than new download, delete it
        if existing_is_worse(&path, new_files)? {
            fs::remove_file(&path)?;
            deleted += 1;
        }
    }

    Ok(deleted)
}
```

**Quality comparison logic** (`is_audio_file`, `existing_is_worse`):

```rust
const AUDIO_EXTENSIONS: &[&str] = &[
    "flac", "mp3", "ogg", "oga", "aac", "m4a", "wav", "wma", "opus", "alac",
];

fn is_audio_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|ext| AUDIO_EXTENSIONS.contains(&ext.to_lowercase().as_str()))
        .unwrap_or(false)
}

/// Format priority: FLAC (and other lossless) > lossy formats.
/// Within same category, compare bitrate/bitdepth.
fn existing_is_worse(existing: &Path, new_files: &[PathBuf]) -> Result<bool> {
    let existing_meta = read_audio_metadata(existing)?;
    let new_meta = aggregate_new_quality(new_files)?;

    Ok(existing_meta.quality_score() < new_meta.quality_score())
}
```

**Quality score**:
- Lossless formats (FLAC, WAV, ALAC): `1000 + bitdepth * 100 + sample_rate`
- Lossy formats: `bitrate` (kbps)
- Unknown: `0`

This ensures FLAC always ranks above any lossy format, and within lossy formats, higher bitrate wins.

---

## 5. Recovery from Interrupted Upgrades

On startup (before any new downloads), `run_auto_mode` runs a recovery scan:

```rust
fn recover_interrupted_upgrades(
    config: &Config,
    db: &Database,
    staging_dir: &Path,
) -> Result<()> {
    if !config.library_upgrade.enabled {
        return Ok(());
    }

    for entry in fs::read_dir(staging_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }

        let album_staging = entry.path();
        let album_slug = album_staging.file_name().unwrap().to_string_lossy();

        // Parse artist--album from slug
        let (artist, album) = parse_album_slug(&album_slug);

        // Check DB status
        match db.get_album_status(&artist, &album)? {
            Some("success") => {
                // Staging dir exists + DB says success = partial move
                tracing::warn!(
                    "Recovering interrupted library upgrade: {artist} — {album}"
                );
                // Re-verify/copy files from staging to library
                // (hash check: skip files already in library, re-copy corrupt ones)
                // Then clean up staging
                resume_library_upgrade(config, &artist, &album, &album_staging)?;
            }
            Some("in-progress") => {
                // Previous run crashed mid-download
                tracing::info!(
                    "Cleaning up incomplete download: {artist} — {album}"
                );
                fs::remove_dir_all(&album_staging)?;
            }
            _ => {
                // Normal state — skip
            }
        }
    }

    Ok(())
}
```

**Resume logic** (`resume_library_upgrade`):

```rust
fn resume_library_upgrade(
    config: &Config,
    artist: &str,
    album: &str,
    album_staging: &Path,
) -> Result<()> {
    let target_path = /* resolve from config.library_paths */;
    let album_dir = target_path.join(artist).join(album);

    for entry in fs::read_dir(album_staging)? {
        let entry = entry?;
        let src = entry.path();
        let dest = album_dir.join(src.file_name().unwrap());

        if dest.exists() {
            // Hash check: skip if identical, re-copy if corrupt
            if file_hash(&src)? == file_hash(&dest)? {
                continue; // Already copied correctly
            }
            // Corrupt/truncated copy — delete and re-copy
            fs::remove_file(&dest)?;
        }

        fs::copy(&src, &dest)?;
    }

    // Clean up staging
    fs::remove_dir_all(album_staging)?;

    // Delete lesser-quality files
    if config.library_upgrade.delete_lesser_quality {
        let new_files: Vec<PathBuf> = fs::read_dir(&album_dir)?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| is_audio_file(p))
            .collect();
        delete_lesser_quality_files(&album_dir, &new_files)?;
    }

    Ok(())
}
```

---

## 6. Edge Cases

| Scenario | Handling |
|----------|----------|
| Album directory doesn't exist in library | Create it with `create_dir_all`, copy files, no deletion needed |
| Album directory has only non-audio files (cues, logs, images) | Copy new files in, non-audio files preserved, no audio to delete |
| Download is partial (10/13 tracks) | Completeness gate fails, staging preserved, album marked "failed" |
| Target library path doesn't exist | Error: log warning, skip upgrade, keep staging |
| Copy fails mid-way (disk full, permission) | Stop, log error, staging preserved (some files may be in library) |
| Staging dir exists on startup, DB says "success" | Recovery: re-verify/re-copy files, clean up staging, delete old |
| Album already upgraded (staging empty, DB "success") | Normal skip — nothing to do |
| Config changes between runs (upgrade disabled then re-enabled) | Recovery only runs if upgrade is enabled; staging dirs left by disabled runs are not cleaned |
| `library_upgrade.enabled: true` but `library.paths` empty | Config validation error at startup |
| Multiple library paths, album in different path each run | Scanner returns the actual path; threading ensures correct target |
| Existing library has FLAC, new download is MP3 | FLAC preserved (not worse), MP3 added alongside — no deletion |
| Existing library has MP3 320kbps, new download is FLAC | MP3 deleted (FLAC is better quality) |
| Existing library has FLAC 16-bit, new download is FLAC 24-bit | 16-bit deleted (24-bit is better quality) |

---

## 7. Error Handling

| Error | Action |
|-------|--------|
| Copy fails (I/O error, disk full) | Log ERROR, stop upgrade, keep staging, mark "failed" |
| Delete fails (permission) | Log ERROR, continue (non-fatal — old files remain alongside new) |
| Hash mismatch during recovery | Delete corrupt file, re-copy from staging |
| Recovery scan fails | Log WARNING, skip recovery for that album, continue startup |
| Config validation fails | Hard error at startup — refuse to run |
| Album directory creation fails | Log ERROR, skip upgrade, keep staging |

---

## 8. Testing Strategy

### Unit tests (in `src/organizer.rs`)

| Test | What it verifies |
|------|-----------------|
| `test_format_priority_comparison` | FLAC > MP3 > OGG, same-format bitrate/bitdepth comparison |
| `test_is_audio_file` | Correct classification: flac/mp3/ogg/aac/wav/wma/opus/alac = audio; jpg/nfo/log/cue/txt = not audio |
| `test_copy_to_library` | Files copied from staging to library, staging preserved |
| `test_delete_lesser_quality` | Old MP3 deleted when new FLAC lands; old FLAC preserved; images/nfo untouched |
| `test_delete_lesser_quality_disabled` | With `delete_lesser_quality: false`, old files preserved |
| `test_completeness_gate` | Partial download (10/13) → upgrade skipped |
| `test_recovery_resumes_partial_copy` | Staging dir with files + DB "success" → files re-verified/copied, staging cleaned |
| `test_recovery_skips_corrupt_file` | Hash mismatch → corrupt file deleted, re-copied from staging |

### Integration tests (in `tests/pipeline_test.rs`)

| Test | What it verifies |
|------|-----------------|
| `test_full_pipeline_with_library_upgrade` | End-to-end: download → complete → copy → delete old → success |
| `test_library_upgrade_recovery` | Simulate interrupted move, verify recovery on next run |

---

## 9. Files Changed

| File | Change |
|------|--------|
| `src/config.rs` | Add `LibraryUpgradeConfig` struct, add to `Config`, add validation |
| `src/organizer.rs` | Add `copy_to_library`, `delete_lesser_quality_files`, `is_audio_file`, `existing_is_worse`, quality score functions, recovery functions |
| `src/runner.rs` | Add `target_library_path` param to `process_album`, add library upgrade flow, add recovery scan in `run_auto_mode`, thread path from scanner |
| `tests/pipeline_test.rs` | Add integration tests for library upgrade |
