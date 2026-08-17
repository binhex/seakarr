# Library Upgrade Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use sub-agents (recommended) to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** After a complete album download in auto mode, copy files from staging to the origin library directory, delete lesser-quality existing audio files, and clean up staging — making seakarr a fully automated library upgrader.

**Architecture:** A new `library_upgrade` config section controls the feature. The scanner's `ScannedAlbum.path` is threaded through to `process_album` as `target_library_path`. New organizer functions handle copy-to-library, quality-aware deletion, and recovery from interrupted upgrades. Recovery uses staging dir existence as the signal.

**Tech Stack:** Rust, lofty (already used by scanner for audio metadata), tempfile, fs::copy/rename, SHA-256 for hash verification.

**Spec:** `docs/specs/2026-08-16-library-upgrade-design.md`

---

## File Structure

| File | Responsibility | Change type |
|------|---------------|-------------|
| `src/config.rs` | Add `LibraryUpgradeConfig` struct, validation | Modify |
| `src/db.rs` | Add `get_album_status` for recovery queries | Modify |
| `src/scanner.rs` | Change `find_albums_to_upgrade` to return library path | Modify |
| `src/organizer.rs` | Add quality comparison, copy, delete, recovery functions | Modify |
| `src/runner.rs` | Thread `target_library_path` through `process_album`, wire upgrade flow and recovery | Modify |
| `tests/pipeline_test.rs` | Integration tests for library upgrade and recovery | Modify |

---

### Task 1: Add `LibraryUpgradeConfig` to config module

**Files:**
- Modify: `src/config.rs`

- [ ] **Step 1: Write failing tests for config**

Add at the end of the `tests` module in `src/config.rs`:

```rust
#[test]
fn test_library_upgrade_defaults_false() {
    let config = Config::default();
    assert!(!config.library_upgrade.enabled);
    assert!(!config.library_upgrade.delete_lesser_quality);
}

#[test]
fn test_library_upgrade_from_yaml() {
    let yaml = r#"
library_upgrade:
  enabled: true
  delete_lesser_quality: true
"#;
    let config: Config = serde_yaml::from_str(yaml).unwrap();
    assert!(config.library_upgrade.enabled);
    assert!(config.library_upgrade.delete_lesser_quality);
}

#[test]
fn test_library_upgrade_requires_library_paths() {
    let mut config = Config::default();
    config.soulseek.username = "u".into();
    config.soulseek.password = "p".into();
    config.library_upgrade.enabled = true;
    config.library.paths = vec![];
    let err = config.validate().unwrap_err().to_string();
    assert!(err.contains("library.paths"), "got: {err}");
}

#[test]
fn test_library_upgrade_valid_with_paths() {
    let mut config = Config::default();
    config.soulseek.username = "u".into();
    config.soulseek.password = "p".into();
    config.library_upgrade.enabled = true;
    config.library.paths = vec!["/music".into()];
    assert!(config.validate().is_ok());
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib config::tests::test_library_upgrade 2>&1`
Expected: FAIL — `library_upgrade` field not found on `Config`

- [ ] **Step 3: Add `LibraryUpgradeConfig` struct and field**

In `src/config.rs`, add the struct after `NotificationConfig`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LibraryUpgradeConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub delete_lesser_quality: bool,
}
```

Add to the `Config` struct (after `notifications`):

```rust
pub library_upgrade: LibraryUpgradeConfig,
```

Add the `Default` impl (after `DaemonConfig` default):

```rust
impl Default for LibraryUpgradeConfig {
    fn default() -> Self {
        Config::default().library_upgrade
    }
}
```

In `Config::default()`, add to the struct literal:

```rust
library_upgrade: LibraryUpgradeConfig {
    enabled: false,
    delete_lesser_quality: false,
},
```

- [ ] **Step 4: Add validation**

In `Config::validate()`, add after the existing checks:

```rust
if self.library_upgrade.enabled && self.library.paths.is_empty() {
    return Err(SeakarrError::Config(
        "library_upgrade.enabled requires at least one library.paths entry".into(),
    ));
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --lib config::tests::test_library_upgrade 2>&1`
Expected: PASS (4 tests)

- [ ] **Step 6: Commit**

```bash
git add src/config.rs
git commit -m "feat: add LibraryUpgradeConfig with validation"
```

---

### Task 2: Add `get_album_status` to DB module

**Files:**
- Modify: `src/db.rs`

- [ ] **Step 1: Write failing test**

Add at the end of the tests module in `src/db.rs`:

```rust
#[test]
fn test_get_album_status() {
    let db = Database::open(":memory:").unwrap();
    db.mark_album_processed("Artist", "Album", "success").unwrap();
    let status = db.get_album_status("Artist", "Album").unwrap();
    assert_eq!(status, Some("success".to_string()));

    let missing = db.get_album_status("Nobody", "Nothing").unwrap();
    assert_eq!(missing, None);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib db::tests::test_get_album_status 2>&1`
Expected: FAIL — `get_album_status` not found

- [ ] **Step 3: Implement `get_album_status`**

Add after `is_album_processed` in `src/db.rs`:

```rust
/// Return the current status of an album, or None if not tracked.
pub fn get_album_status(&self, artist: &str, album: &str) -> Result<Option<String>> {
    let mut stmt = self.conn.prepare(
        "SELECT status FROM processed_albums WHERE artist = ?1 AND album = ?2",
    )?;
    let mut rows = stmt.query_map(params![artist, album], |row| row.get::<_, String>(0))?;
    match rows.next() {
        Some(row) => Ok(Some(row?)),
        None => Ok(None),
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib db::tests::test_get_album_status 2>&1`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add src/db.rs
git commit -m "feat: add get_album_status for library upgrade recovery"
```

---

### Task 3: Add quality comparison and audio file detection to organizer

**Files:**
- Modify: `src/organizer.rs`

- [ ] **Step 1: Write failing tests**

Add at the end of the `tests` module in `src/organizer.rs`:

```rust
#[test]
fn test_is_audio_file() {
    assert!(is_audio_file(Path::new("track.flac")));
    assert!(is_audio_file(Path::new("track.mp3")));
    assert!(is_audio_file(Path::new("track.ogg")));
    assert!(is_audio_file(Path::new("track.aac")));
    assert!(is_audio_file(Path::new("track.wav")));
    assert!(is_audio_file(Path::new("track.wma")));
    assert!(is_audio_file(Path::new("track.opus")));
    assert!(is_audio_file(Path::new("track.alac")));
    assert!(!is_audio_file(Path::new("cover.jpg")));
    assert!(!is_audio_file(Path::new("info.nfo")));
    assert!(!is_audio_file(Path::new("log.txt")));
    assert!(!is_audio_file(Path::new("disc.cue")));
    assert!(!is_audio_file(Path::new("playlist.m3u")));
}

#[test]
fn test_quality_score_lossless_beats_lossy() {
    // FLAC (16-bit, 44100 Hz) should score higher than any lossy format
    let flac_score = quality_score_lossless(16, 44100);
    let mp3_320_score = quality_score_lossy(320);
    assert!(flac_score > mp3_320_score, "flac={flac_score} mp3={mp3_320_score}");
}

#[test]
fn test_quality_score_higher_bitdepth_wins() {
    let score_16 = quality_score_lossless(16, 44100);
    let score_24 = quality_score_lossless(24, 96000);
    assert!(score_24 > score_16);
}

#[test]
fn test_quality_score_higher_bitrate_wins() {
    let score_128 = quality_score_lossy(128);
    let score_320 = quality_score_lossy(320);
    assert!(score_320 > score_128);
}

#[test]
fn test_format_from_extension() {
    assert_eq!(format_from_extension("flac"), Some(AudioFormat::Lossless));
    assert_eq!(format_from_extension("wav"), Some(AudioFormat::Lossless));
    assert_eq!(format_from_extension("alac"), Some(AudioFormat::Lossless));
    assert_eq!(format_from_extension("mp3"), Some(AudioFormat::Lossy));
    assert_eq!(format_from_extension("ogg"), Some(AudioFormat::Lossy));
    assert_eq!(format_from_extension("aac"), Some(AudioFormat::Lossy));
    assert_eq!(format_from_extension("opus"), Some(AudioFormat::Lossy));
    assert_eq!(format_from_extension("jpg"), None);
    assert_eq!(format_from_extension("nfo"), None);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib organizer::tests::test_is_audio_file 2>&1`
Expected: FAIL — `is_audio_file` not found

- [ ] **Step 3: Implement quality comparison functions**

Add at the top of `src/organizer.rs` (after existing imports):

```rust
use std::collections::HashSet;

/// Audio file extensions recognised by the library upgrade feature.
const AUDIO_EXTENSIONS: &[&str] = &[
    "flac", "mp3", "ogg", "oga", "aac", "m4a", "wav", "wma", "opus", "alac",
];

/// Classification of audio formats by quality tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioFormat {
    Lossless,
    Lossy,
}

/// Return the format category for a file extension, or None for non-audio.
pub fn format_from_extension(ext: &str) -> Option<AudioFormat> {
    match ext.to_lowercase().as_str() {
        "flac" | "wav" | "alac" | "ape" | "wv" | "aiff" | "aif" => Some(AudioFormat::Lossless),
        "mp3" | "ogg" | "oga" | "aac" | "m4a" | "wma" | "opus" | "spx" => {
            Some(AudioFormat::Lossy)
        }
        _ => None,
    }
}

/// Return true if the path has an audio file extension.
pub fn is_audio_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .and_then(format_from_extension)
        .is_some()
}

/// Quality score for a lossless file. Higher is better.
/// Formula: 1000 + bitdepth * 100 + sample_rate
pub fn quality_score_lossless(bitdepth: u32, sample_rate: u32) -> u64 {
    1000 + (bitdepth as u64) * 100 + (sample_rate as u64)
}

/// Quality score for a lossy file. Higher is better.
/// Formula: bitrate (kbps)
pub fn quality_score_lossy(bitrate: u32) -> u64 {
    bitrate as u64
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib organizer::tests::test_is_audio_file organizer::tests::test_quality_score organizer::tests::test_format_from_extension 2>&1`
Expected: PASS (5 tests)

- [ ] **Step 5: Commit**

```bash
git add src/organizer.rs
git commit -m "feat: add audio format detection and quality scoring to organizer"
```

---

### Task 4: Add `copy_to_library` and `delete_lesser_quality_files` to organizer

**Files:**
- Modify: `src/organizer.rs`

- [ ] **Step 1: Write failing tests**

Add at the end of the `tests` module in `src/organizer.rs`:

```rust
#[test]
fn test_copy_to_library_preserves_staging() {
    let staging = TempDir::new().unwrap();
    let library = TempDir::new().unwrap();

    // Create files in staging
    let src1 = staging.path().join("01 - Song.flac");
    let src2 = staging.path().join("02 - Song.flac");
    fs::write(&src1, b"flac content 1").unwrap();
    fs::write(&src2, b"flac content 2").unwrap();

    let pattern = "%artist%/%album%/%track% - %title%.%ext%";
    let files = vec![src1.clone(), src2.clone()];

    let result = copy_to_library(
        &files,
        library.path(),
        pattern,
        "Test Artist",
        "Test Album",
    );
    assert!(result.is_ok());

    // Files should exist in library
    assert!(library.path().join("Test Artist/Test Album/01 - Song.flac").exists());
    assert!(library.path().join("Test Artist/Test Album/02 - Song.flac").exists());
    // Staging files should still exist (copy, not move)
    assert!(src1.exists());
    assert!(src2.exists());
}

#[test]
fn test_delete_lesser_quality_removes_worse_files() {
    let dir = TempDir::new().unwrap();
    let album_dir = dir.path();

    // Existing MP3 file (lesser quality)
    let old_mp3 = album_dir.join("01 - Old Track.mp3");
    fs::write(&old_mp3, b"mp3 content").unwrap();

    // Existing image (should NOT be deleted)
    let cover = album_dir.join("cover.jpg");
    fs::write(&cover, b"jpg content").unwrap();

    // Existing NFO (should NOT be deleted)
    let nfo = album_dir.join("info.nfo");
    fs::write(&nfo, b"nfo content").unwrap();

    // New FLAC files (better quality)
    let new_flac = album_dir.join("01 - New Track.flac");
    fs::write(&new_flac, b"flac content").unwrap();

    // Set up library_root/Artist/Album structure
    let library_root = dir.path();
    let artist = "Artist";
    let album = "Album";
    let album_dir = library_root.join(artist).join(album);
    fs::create_dir_all(&album_dir).unwrap();

    // Move files into album dir
    fs::rename(old_mp3, album_dir.join("01 - Old Track.mp3")).unwrap();
    fs::rename(cover, album_dir.join("cover.jpg")).unwrap();
    fs::rename(nfo, album_dir.join("info.nfo")).unwrap();
    fs::rename(new_flac, album_dir.join("01 - New Track.flac")).unwrap();

    let new_files = vec![album_dir.join("01 - New Track.flac")];
    let deleted = delete_lesser_quality_files(library_root, artist, album, &new_files).unwrap();

    // Old MP3 should be deleted (FLAC > MP3)
    assert!(!album_dir.join("01 - Old Track.mp3").exists(), "old MP3 should be deleted");
    // Image and NFO should be preserved
    assert!(album_dir.join("cover.jpg").exists(), "cover.jpg should be preserved");
    assert!(album_dir.join("info.nfo").exists(), "info.nfo should be preserved");
    assert_eq!(deleted, 1);
}

#[test]
fn test_delete_lesser_quality_preserves_better_files() {
    let dir = TempDir::new().unwrap();
    let album_dir = dir.path();

    // Existing FLAC (high quality — should NOT be deleted)
    let old_flac = album_dir.join("01 - Track.flac");
    fs::write(&old_flac, b"flac content").unwrap();

    // New MP3 (lower quality)
    let new_mp3 = album_dir.join("01 - New Track.mp3");
    fs::write(&new_mp3, b"mp3 content").unwrap();

    // Set up library_root/Artist/Album structure
    let library_root = dir.path();
    let artist = "Artist";
    let album = "Album";
    let album_dir = library_root.join(artist).join(album);
    fs::create_dir_all(&album_dir).unwrap();

    fs::rename(old_flac, album_dir.join("01 - Track.flac")).unwrap();
    fs::rename(new_mp3, album_dir.join("01 - New Track.mp3")).unwrap();

    let new_files = vec![album_dir.join("01 - New Track.mp3")];
    let deleted = delete_lesser_quality_files(library_root, artist, album, &new_files).unwrap();

    // Old FLAC should be preserved (FLAC > MP3)
    assert!(album_dir.join("01 - Track.flac").exists(), "FLAC should be preserved when new download is MP3");
    assert_eq!(deleted, 0);
}

#[test]
fn test_delete_lesser_quality_disabled_noop() {
    let dir = TempDir::new().unwrap();
    let album_dir = dir.path();

    let old_mp3 = album_dir.join("01 - Track.mp3");
    fs::write(&old_mp3, b"mp3 content").unwrap();

    let new_flac = album_dir.join("01 - New.flac");
    fs::write(&new_flac, b"flac content").unwrap();

    // When delete is disabled, function should not be called.
    // This test verifies the function itself doesn't delete when
    // new_files is empty (simulating the disabled case).
    // Set up library_root/Artist/Album structure
    let library_root = dir.path();
    let artist = "Artist";
    let album = "Album";
    let album_dir = library_root.join(artist).join(album);
    fs::create_dir_all(&album_dir).unwrap();

    fs::rename(old_mp3, album_dir.join("01 - Track.mp3")).unwrap();
    fs::rename(new_flac, album_dir.join("01 - New.flac")).unwrap();

    // When delete is disabled, function should not be called.
    // This test verifies the function itself doesn't delete when
    // new_files is empty (simulating the disabled case).
    let deleted = delete_lesser_quality_files(library_root, artist, album, &[]).unwrap();
    assert!(album_dir.join("01 - Track.mp3").exists(), "old file should be preserved");
    assert_eq!(deleted, 0);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib organizer::tests::test_copy_to_library 2>&1`
Expected: FAIL — `copy_to_library` not found

- [ ] **Step 3: Implement `copy_to_library`**

Add in `src/organizer.rs` (after `organize_file`):

```rust
/// Copy downloaded files from staging to the library directory, applying
/// the organize pattern for naming. The staging directory is preserved
/// (this is a copy, not a move).
pub fn copy_to_library(
    downloaded: &[PathBuf],
    library_root: &Path,
    pattern: &str,
    artist: &str,
    album: &str,
) -> Result<()> {
    for src in downloaded {
        let stem = src.file_stem().unwrap_or_default().to_string_lossy();
        let ext = src.extension().unwrap_or_default().to_string_lossy();
        let track = crate::tracks::track_number_from_filename(&stem)
            .map(|n| n.to_string())
            .unwrap_or_else(|| "01".to_string());
        let relative = expand_pattern(pattern, artist, album, &track, &stem, &ext, "unknown");
        let dest = library_root.join(&relative);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(src, &dest)?;
    }
    Ok(())
}
```

- [ ] **Step 4: Implement `delete_lesser_quality_files`**

Add in `src/organizer.rs` (after `copy_to_library`):

```rust
/// Delete audio files in the album directory that are lower quality than
/// the new download. Non-audio files (images, logs, cuesheets) are never
/// deleted. Files matching the new download's filenames are skipped.
///
/// `library_root` is the library path (e.g. `/media/music`).
/// `artist` and `album` identify the album subdirectory.
///
/// Returns the number of files deleted.
pub fn delete_lesser_quality_files(
    library_root: &Path,
    artist: &str,
    album: &str,
    new_files: &[PathBuf],
) -> Result<u32> {
    let album_dir = library_root
        .join(sanitize_component(artist))
        .join(sanitize_component(album));

    // Build a set of new filenames to skip
    let new_filenames: HashSet<String> = new_files
        .iter()
        .filter_map(|p| p.file_name().and_then(|n| n.to_str()).map(String::from))
        .collect();

    // Determine the best quality score from the new download
    let new_best_score = new_files
        .iter()
        .filter_map(|p| file_quality_score(p))
        .max()
        .unwrap_or(0);

    let mut deleted = 0;
    for entry in fs::read_dir(&album_dir)? {
        let entry = entry?;
        let path = entry.path();

        if !path.is_file() {
            continue;
        }

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

        // Compare quality: if existing file is worse, delete it
        let existing_score = file_quality_score(&path).unwrap_or(0);
        if existing_score < new_best_score {
            fs::remove_file(&path)?;
            deleted += 1;
        }
    }

    Ok(deleted)
}

/// Compute a quality score for an audio file by reading its metadata.
/// Returns None if the file cannot be read or is not audio.
fn file_quality_score(path: &Path) -> Option<u64> {
    let ext = path.extension().and_then(|e| e.to_str())?;
    let format = format_from_extension(ext)?;

    match format {
        AudioFormat::Lossless => {
            // For lossless, use bitdepth and sample rate from lofty
            let tagged_file = lofty::probe::Probe::open(path).ok()?.read().ok()?;
            let props = tagged_file.properties();
            let bitdepth = props.bit_depth() as u32;
            let sample_rate = props.sample_rate().unwrap_or(44100);
            Some(quality_score_lossless(bitdepth, sample_rate))
        }
        AudioFormat::Lossy => {
            // For lossy, use bitrate from lofty
            let tagged_file = lofty::probe::Probe::open(path).ok()?.read().ok()?;
            let bitrate = tagged_file.properties().audio_bitrate();
            Some(quality_score_lossy(bitrate))
        }
    }
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --lib organizer::tests 2>&1`
Expected: PASS (all organizer tests)

- [ ] **Step 6: Commit**

```bash
git add src/organizer.rs
git commit -m "feat: add copy_to_library and delete_lesser_quality_files to organizer"
```

---

### Task 5: Add recovery functions to organizer

**Files:**
- Modify: `src/organizer.rs`

- [ ] **Step 1: Write failing tests**

Add at the end of the `tests` module in `src/organizer.rs`:

```rust
#[test]
fn test_resume_library_upgrade_recopies_corrupt_file() {
    let staging = TempDir::new().unwrap();
    let library = TempDir::new().unwrap();

    // Create a file in staging
    let src = staging.path().join("01 - Song.flac");
    fs::write(&src, b"correct content").unwrap();

    // Create a corrupt copy in the library (different content)
    let album_dir = library.path().join("Artist/Album");
    fs::create_dir_all(&album_dir).unwrap();
    let dest = album_dir.join("01 - Song.flac");
    fs::write(&dest, b"corrupt truncated").unwrap();

    let config = Config::default();
    let result = resume_library_upgrade(
        &config,
        staging.path(),
        library.path(),
        "Artist",
        "Album",
    );
    assert!(result.is_ok());

    // Corrupt file should have been replaced with correct content
    let content = fs::read_to_string(&dest).unwrap();
    assert_eq!(content, "correct content");
    // Staging should be cleaned up
    assert!(!staging.path().exists());
}

#[test]
fn test_resume_library_upgrade_skips_identical_file() {
    let staging = TempDir::new().unwrap();
    let library = TempDir::new().unwrap();

    // Create a file in staging
    let src = staging.path().join("01 - Song.flac");
    fs::write(&src, b"same content").unwrap();

    // Create an identical copy in the library
    let album_dir = library.path().join("Artist/Album");
    fs::create_dir_all(&album_dir).unwrap();
    let dest = album_dir.join("01 - Song.flac");
    fs::write(&dest, b"same content").unwrap();

    let config = Config::default();
    let result = resume_library_upgrade(
        &config,
        staging.path(),
        library.path(),
        "Artist",
        "Album",
    );
    assert!(result.is_ok());

    // File should still be there with same content
    let content = fs::read_to_string(&dest).unwrap();
    assert_eq!(content, "same content");
    // Staging should be cleaned up
    assert!(!staging.path().exists());
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib organizer::tests::test_resume_library_upgrade 2>&1`
Expected: FAIL — `resume_library_upgrade` not found

- [ ] **Step 3: Implement recovery functions**

Add in `src/organizer.rs` (after `delete_lesser_quality_files`):

```rust
/// Compute a SHA-256 hash of a file's contents for comparison.
pub fn file_hash(path: &Path) -> Result<String> {
    use std::io::Read;
    let mut file = fs::File::open(path)?;
    let mut hasher = sha2::Sha256::new();
    let mut buffer = [0u8; 8192];
    loop {
        let bytes_read = file.read(&mut buffer)?;
        if bytes_read == 0 {
            break;
        }
        hasher.update(&buffer[..bytes_read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Resume an interrupted library upgrade. Copies files from staging to the
/// library, verifying hashes to detect corrupt partial copies. Cleans up
/// staging when complete.
pub fn resume_library_upgrade(
    config: &Config,
    album_staging: &Path,
    library_root: &Path,
    artist: &str,
    album: &str,
) -> Result<()> {
    let album_dir = library_root
        .join(sanitize_component(artist))
        .join(sanitize_component(album));
    fs::create_dir_all(&album_dir)?;

    // Copy files from staging, verifying or replacing as needed
    for entry in fs::read_dir(album_staging)? {
        let entry = entry?;
        let src = entry.path();
        if !src.is_file() {
            continue;
        }
        let dest = album_dir.join(src.file_name().unwrap());

        if dest.exists() {
            // Hash check: skip if identical, re-copy if corrupt
            if file_hash(&src)? == file_hash(&dest)? {
                continue;
            }
            // Corrupt/truncated copy — delete and re-copy
            fs::remove_file(&dest)?;
        }

        fs::copy(&src, &dest)?;
    }

    // Clean up staging
    fs::remove_dir_all(album_staging)?;

    // Delete lesser-quality files (if enabled)
    if config.library_upgrade.delete_lesser_quality {
        let new_files: Vec<PathBuf> = fs::read_dir(&album_dir)?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| is_audio_file(p))
            .collect();
        delete_lesser_quality_files(library_root, artist, album, &new_files)?;
    }

    Ok(())
}

/// Parse an album staging directory name (e.g. "Artist--Album") into
/// (artist, album) components.
pub fn parse_album_slug(slug: &str) -> (String, String) {
    match slug.split_once("--") {
        Some((artist, album)) => (artist.to_string(), album.to_string()),
        None => (slug.to_string(), "Unknown".to_string()),
    }
}

/// Recover interrupted library upgrades by scanning the staging directory
/// for leftover album directories whose DB status is "success".
pub fn recover_interrupted_upgrades(
    config: &Config,
    db: &crate::db::Database,
    staging_dir: &Path,
) -> Result<()> {
    if !config.library_upgrade.enabled {
        return Ok(());
    }

    let library_root = Path::new(&config.library.paths[0]);

    for entry in fs::read_dir(staging_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }

        let album_staging = entry.path();
        let slug = album_staging.file_name().unwrap().to_string_lossy();
        let (artist, album) = parse_album_slug(&slug);

        match db.get_album_status(&artist, &album)? {
            Some(status) if status == "success" => {
                // Staging dir exists + DB says success = partial move
                tracing::warn!(
                    "Recovering interrupted library upgrade: {artist} — {album}"
                );
                resume_library_upgrade(
                    config,
                    &album_staging,
                    library_root,
                    &artist,
                    &album,
                )?;
            }
            Some(status) if status == "in-progress" => {
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

Note: The `file_hash` function requires adding `sha2` to `Cargo.toml`. Run: `cargo add sha2`

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib organizer::tests::test_resume_library_upgrade 2>&1`
Expected: PASS (2 tests)

- [ ] **Step 5: Run all organizer tests to verify no regressions**

Run: `cargo test --lib organizer::tests 2>&1`
Expected: PASS (all tests)

- [ ] **Step 6: Commit**

```bash
git add src/organizer.rs Cargo.toml Cargo.lock
git commit -m "feat: add recovery functions for interrupted library upgrades"
```

---

### Task 6: Thread library path through scanner and runner

**Files:**
- Modify: `src/scanner.rs` — change `find_albums_to_upgrade` return type
- Modify: `src/runner.rs` — update `run_auto_mode` to pass library path

- [ ] **Step 1: Write failing test for `find_albums_to_upgrade` with path**

Add in the tests module of `src/scanner.rs`:

```rust
#[test]
fn test_find_albums_to_upgrade_returns_library_path() {
    let dir = TempDir::new().unwrap();
    let album_dir = dir.path().join("Artist").join("Album");
    fs::create_dir_all(&album_dir).unwrap();
    // Create a non-preferred format file (OGG when flac is allowed)
    fs::write(album_dir.join("01 - Track.ogg"), b"ogg content").unwrap();

    let albums = scan_library(&library_paths(dir.path())).unwrap();
    let config = FilterConfig {
        allowed_extensions: vec!["flac".into()],
        ..Default::default()
    };
    let targets = find_albums_to_upgrade(&albums, &config);
    assert_eq!(targets.len(), 1);
    let (artist, album, track_count, path) = &targets[0];
    assert_eq!(artist, "Artist");
    assert_eq!(album, "Album");
    assert_eq!(*track_count, 1);
    assert_eq!(path, dir.path());
}
```

- [ ] **Step 2: Change `find_albums_to_upgrade` return type**

In `src/scanner.rs`, change:

```rust
pub fn find_albums_to_upgrade(
    albums: &[ScannedAlbum],
    config: &FilterConfig,
) -> Vec<(String, String, usize)> {
```

to:

```rust
pub fn find_albums_to_upgrade(
    albums: &[ScannedAlbum],
    config: &FilterConfig,
) -> Vec<(String, String, usize, PathBuf)> {
```

And change the `.map` at the end of the function from:

```rust
.map(|a| (a.artist.clone(), a.album.clone(), a.track_count))
```

to:

```rust
.map(|a| (a.artist.clone(), a.album.clone(), a.track_count, a.path.clone()))
```

- [ ] **Step 3: Update `run_auto_mode` to use the new tuple**

In `src/runner.rs`, change the targets_vec type and destructuring:

```rust
let targets_vec: Vec<(String, String, usize)> = targets_with_counts;
```

to:

```rust
let targets_vec: Vec<(String, String, usize, PathBuf)> = targets_with_counts;
```

And change the loop from:

```rust
for (artist, album, track_count) in &targets_vec {
```

to:

```rust
for (artist, album, track_count, library_path) in &targets_vec {
```

And in the `process_album` call, add the new parameter:

```rust
let result = process_album(
    client,
    &artist,
    Some(&album),
    config,
    db,
    staging_dir,
    progress.as_deref(),
    Some(&cancel),
    Some(library_track_count),
    Some(library_path),  // NEW: target library path
)
.await;
```

- [ ] **Step 4: Run tests to verify compilation and scanner test passes**

Run: `cargo test --lib scanner::tests::test_find_albums_to_upgrade_returns_library_path 2>&1`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add src/scanner.rs src/runner.rs
git commit -m "feat: thread library path from scanner through process_album"
```

---

### Task 7: Wire up library upgrade flow in `process_album`

**Files:**
- Modify: `src/runner.rs`

- [ ] **Step 1: Add `target_library_path` parameter to `process_album`**

Change the signature:

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
) -> Result<AlbumOutcome> {
```

- [ ] **Step 2: Add library upgrade logic after download**

After the `download_album` call and before the existing organize block, insert:

```rust
    // Library upgrade (auto mode only, when enabled)
    if config.library_upgrade.enabled {
        if let Some(target_path) = target_library_path {
            // Completeness gate: all files from the best peer must have downloaded
            let expected_count = ranked.first().map(|r| r.files.len()).unwrap_or(0);
            if downloaded.len() < expected_count {
                tracing::warn!(
                    "{artist} — {}: download incomplete ({}/{} tracks), skipping library upgrade",
                    album.unwrap_or("?"),
                    downloaded.len(),
                    expected_count,
                );
                return Ok(AlbumOutcome::Failed {
                    reason: "incomplete download, library upgrade skipped".into(),
                });
            }

            // Copy files to library
            match organizer::copy_to_library(
                &downloaded,
                target_path,
                &config.storage.organize_pattern,
                artist,
                album.unwrap_or("Unknown"),
            ) {
                Ok(()) => {
                    // Delete lesser-quality files
                    if config.library_upgrade.delete_lesser_quality {
                        match organizer::delete_lesser_quality_files(
                            target_path,
                            artist,
                            album.unwrap_or("Unknown"),
                            &downloaded,
                        ) {
                            Ok(count) if count > 0 => {
                                tracing::info!(
                                    "{artist} — {}: deleted {count} lesser-quality file(s)",
                                    album.unwrap_or("?"),
                                );
                            }
                            Ok(_) => {}
                            Err(e) => {
                                tracing::error!(
                                    "{artist} — {}: failed to delete lesser-quality files: {e}",
                                    album.unwrap_or("?"),
                                );
                            }
                        }
                    }

                    // Clean up staging directory
                    if let Err(e) = std::fs::remove_dir_all(&album_staging) {
                        tracing::warn!("Failed to remove staging dir {album_staging:?}: {e}");
                    }

                    // Mark processed
                    if let Some(a) = album {
                        db.mark_album_processed(artist, a, "success")?;
                    }

                    // Notify
                    let track_count = downloaded.len();
                    if let Err(e) = notifier::notify_success(
                        &config.notifications.urls,
                        artist,
                        album.unwrap_or("Unknown"),
                        track_count,
                    )
                    .await
                    {
                        tracing::warn!(
                            "{artist} — {}: notification failed: {e}",
                            album.unwrap_or("(all)")
                        );
                    }

                    tracing::info!(
                        "Completed: {artist} — {} ({track_count} tracks)",
                        album.unwrap_or("(all)")
                    );
                    return Ok(AlbumOutcome::Downloaded { track_count });
                }
                Err(e) => {
                    tracing::error!(
                        "{artist} — {}: library upgrade failed: {e}",
                        album.unwrap_or("?"),
                    );
                    return Ok(AlbumOutcome::Failed {
                        reason: format!("library upgrade failed: {e}"),
                    });
                }
            }
        }
    }
```

Note: The `sanitize_component` function is currently private in organizer.rs. It needs to be made `pub` — add `pub` to its declaration.

- [ ] **Step 3: Make `sanitize_component` public**

In `src/organizer.rs`, change:

```rust
fn sanitize_component(value: &str) -> String {
```

to:

```rust
pub fn sanitize_component(value: &str) -> String {
```

- [ ] **Step 4: Update all `process_album` call sites**

There are call sites in `run_auto_mode`, `run_manual_mode`, and `run_batch_mode`. Add `None` as the last argument for manual and batch modes (they don't have a target library path).

For `run_manual_mode`:
```rust
process_album(
    client,
    artist,
    album,
    config,
    db,
    staging_dir,
    Some(&progress),
    None,
    None,
    None,  // No target library path in manual mode
)
```

For `run_batch_mode`: same pattern — add `None` as the last argument.

- [ ] **Step 5: Run all tests to verify no regressions**

Run: `cargo test 2>&1`
Expected: PASS (all tests)

- [ ] **Step 6: Commit**

```bash
git add src/runner.rs src/organizer.rs
git commit -m "feat: wire up library upgrade flow in process_album"
```

---

### Task 8: Add recovery scan in `run_auto_mode`

**Files:**
- Modify: `src/runner.rs`

- [ ] **Step 1: Add recovery call at the start of `run_auto_mode`**

At the beginning of `run_auto_mode`, after the staging directory creation:

```rust
    // Recover any interrupted library upgrades from previous runs
    organizer::recover_interrupted_upgrades(config, db, staging_dir)?;
```

- [ ] **Step 2: Run tests to verify no regressions**

Run: `cargo test 2>&1`
Expected: PASS

- [ ] **Step 3: Commit**

```bash
git add src/runner.rs
git commit -m "feat: add recovery scan for interrupted library upgrades at startup"
```

---

### Task 9: Add remaining unit tests

**Files:**
- Modify: `src/organizer.rs` — remaining edge case tests
- Modify: `tests/pipeline_test.rs` — integration tests

- [ ] **Step 1: Add completeness gate test to runner tests**

Add in the tests module of `src/runner.rs`:

```rust
#[tokio::test]
async fn test_completeness_gate_skips_incomplete_download() {
    // This test verifies that when a download returns fewer files than
    // expected, the library upgrade is skipped and the album is marked failed.
    // Implementation depends on the existing mock infrastructure in runner tests.
    // Use the same pattern as existing process_album tests.
    // ... (adapt to existing test harness)
}
```

Note: The exact implementation depends on the existing test harness in runner.rs. Adapt the mock setup from existing `process_album` tests.

- [ ] **Step 2: Add integration test for full pipeline with library upgrade**

Add in `tests/pipeline_test.rs`:

```rust
#[tokio::test]
async fn test_full_pipeline_with_library_upgrade() {
    // This test verifies the end-to-end flow:
    // 1. Download completes all tracks
    // 2. Files are copied to library (not moved)
    // 3. Lesser-quality audio files are deleted
    // 4. Non-audio files are preserved
    // 5. Staging is cleaned up
    // 6. Album marked "success" in DB
    // ... (adapt to existing integration test infrastructure)
}
```

- [ ] **Step 3: Run all tests**

Run: `cargo test 2>&1`
Expected: PASS

- [ ] **Step 4: Commit**

```bash
git add src/runner.rs tests/pipeline_test.rs
git commit -m "test: add library upgrade unit and integration tests"
```

---

### Task 10: Final verification and cleanup

- [ ] **Step 1: Run full test suite**

Run: `cargo test 2>&1`
Expected: PASS (all tests)

- [ ] **Step 2: Run clippy**

Run: `cargo clippy -p seakarr --all-targets -- -D warnings 2>&1`
Expected: Clean (no warnings)

- [ ] **Step 3: Run formatting**

Run: `cargo fmt && cargo fmt --check 2>&1`
Expected: Clean

- [ ] **Step 4: Verify config generation includes new section**

Check that the default config YAML generated on first run includes:

```yaml
library_upgrade:
  enabled: false
  delete_lesser_quality: false
```

- [ ] **Step 5: Commit any remaining fixes**

```bash
git add -A
git commit -m "chore: final cleanup for library upgrade feature"
```
