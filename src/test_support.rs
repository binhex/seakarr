//! Test-only helpers shared by the crate's unit tests.
//!
//! Compiled only under `cargo test`; nothing here is part of the binary. The
//! audio fixture lives in one place because three test modules need a file that
//! `lofty` can open, and a per-module copy drifts: the copies previously in
//! `organizer` and `download` had to be kept byte-identical by hand.
//!
//! The fixtures are written byte by byte rather than through `lofty`'s writer,
//! which refuses to rewrite a file that carries no audio frames.

use std::fs;
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, Once};

use lofty::file::TaggedFileExt;
use lofty::probe::Probe;
use lofty::tag::Accessor;

/// Sample rate encoded in the STREAMINFO block of every fixture.
const SAMPLE_RATE: u32 = 44_100;

/// Vorbis comment vendor string written by [`write_minimal_flac_with_tags`].
const TAG_VENDOR: &str = "seakarr test fixture";

/// FLAC metadata block type for Vorbis comments.
const VORBIS_COMMENT_BLOCK: u8 = 4;

/// A search-result file entry with the given bitrate attribute and size.
///
/// Used by every module that builds `SearchResult` fixtures; it lived in five
/// test modules before being shared here.
pub fn make_file(name: &str, bitrate: u32, size: u64) -> crate::client::FileInfo {
    let mut attribs = std::collections::HashMap::new();
    attribs.insert(0, bitrate);
    crate::client::FileInfo {
        name: name.into(),
        size,
        attribs,
    }
}

/// Write a minimal valid FLAC: the `fLaC` marker plus a STREAMINFO block
/// describing 44100 Hz stereo 16-bit audio.
///
/// The file carries no audio frames, so it is exactly the shape an interrupted
/// download leaves behind. That is deliberate: `lofty` still opens and parses
/// it, so it stands in for a library track that the scanner and the placement
/// guard accept, and it proves those checks read the header rather than the
/// whole file.
pub fn write_minimal_flac(path: &Path) {
    fs::write(path, streaminfo_only_flac(16)).unwrap();
}

/// [`write_minimal_flac`] with 24-bit audio, so quality scoring ranks it
/// strictly above a 16-bit copy.
pub fn write_minimal_flac24(path: &Path) {
    fs::write(path, streaminfo_only_flac(24)).unwrap();
}

/// [`write_minimal_flac`] carrying Vorbis comment artist and album tags.
///
/// The scanner prefers tag metadata over the directory name, while a placement
/// destination is always the folder the walk saw. Those two can disagree, and
/// this fixture is how that seam is exercised.
pub fn write_minimal_flac_with_tags(path: &Path, artist: &str, album: &str) {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"fLaC");
    // STREAMINFO stops being the last block once a comment block follows it.
    bytes.extend_from_slice(&[0x00, 0x00, 0x00, 0x22]);
    bytes.extend_from_slice(&streaminfo(16));
    write_vorbis_comment_block(
        &mut bytes,
        &[format!("ARTIST={artist}"), format!("ALBUM={album}")],
    );
    fs::write(path, &bytes).unwrap();

    let tagged = Probe::open(path)
        .expect("the tagged fixture must be openable")
        .read()
        .expect("the tagged fixture must be readable");
    let written = tagged
        .primary_tag()
        .and_then(|tag| tag.artist())
        .map(|value| value.into_owned());
    assert_eq!(
        written.as_deref(),
        Some(artist),
        "the tags must read back, otherwise the seam test proves nothing"
    );
}

/// The 42 STREAMINFO-only bytes shared by [`write_minimal_flac`] and
/// [`write_minimal_flac24`]: marker, block header, and one 34-byte block.
fn streaminfo_only_flac(bits_per_sample: u32) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(42);
    bytes.extend_from_slice(b"fLaC");
    // Last-block flag (0x80) + block type 0 (STREAMINFO), content length 34.
    bytes.extend_from_slice(&[0x80, 0x00, 0x00, 0x22]);
    bytes.extend_from_slice(&streaminfo(bits_per_sample));
    bytes
}

/// The 34-byte STREAMINFO payload.
///
/// Layout: minimum and maximum block size, minimum and maximum frame size
/// (unknown), the packed sample rate / channels / bit depth / total sample
/// count, and the unencoded-audio MD5.
fn streaminfo(bits_per_sample: u32) -> [u8; 34] {
    debug_assert!(
        (4..=32).contains(&bits_per_sample),
        "FLAC bit depth must be 4-32, got {bits_per_sample}"
    );
    let mut bytes = [0u8; 34];
    // Minimum and maximum block size (4096). Frame sizes stay unknown (0).
    bytes[0..4].copy_from_slice(&[0x10, 0x00, 0x10, 0x00]);
    // 20-bit sample rate, 3-bit channels-1 (stereo), 5-bit bits-per-sample-1,
    // then the top 4 bits of the 36-bit total sample count.
    let packed = (SAMPLE_RATE << 12) | (1 << 9) | ((bits_per_sample - 1) << 4);
    bytes[10..14].copy_from_slice(&packed.to_be_bytes());
    // Remaining 32 bits of the 36-bit total sample count. The trailing MD5 of
    // the unencoded audio stays unknown (zeros).
    bytes[14..18].copy_from_slice(&SAMPLE_RATE.to_be_bytes());
    bytes
}

/// Append one Vorbis comment block, which must be the last metadata block.
///
/// Written by hand because `lofty`'s FLAC writer cannot rewrite a file with no
/// audio frames. Vorbis comment lengths are little-endian; the block header
/// length is big-endian.
fn write_vorbis_comment_block(out: &mut Vec<u8>, comments: &[String]) {
    let mut payload = Vec::new();
    payload.extend_from_slice(&(TAG_VENDOR.len() as u32).to_le_bytes());
    payload.extend_from_slice(TAG_VENDOR.as_bytes());
    payload.extend_from_slice(&(comments.len() as u32).to_le_bytes());
    for comment in comments {
        payload.extend_from_slice(&(comment.len() as u32).to_le_bytes());
        payload.extend_from_slice(comment.as_bytes());
    }
    let length = u32::try_from(payload.len()).expect("payload length must fit in u32");
    assert!(length < (1 << 24), "a FLAC block length is 24 bits");
    out.push(0x80 | VORBIS_COMMENT_BLOCK);
    out.extend_from_slice(&length.to_be_bytes()[1..]);
    out.extend_from_slice(&payload);
}

/// Buffer that log records are appended to while a capture is active.
///
/// `None` outside a capture window, so unrelated tests cannot fill it.
static ACTIVE_CAPTURE: Mutex<Option<Arc<Mutex<String>>>> = Mutex::new(None);

/// Serialises capture windows so two tests never record into the same buffer.
static CAPTURE_WINDOW: Mutex<()> = Mutex::new(());

/// Thread that owns the capture window, so a nested capture can fail loudly
/// rather than deadlock on the non-reentrant window lock.
static WINDOW_OWNER: Mutex<Option<std::thread::ThreadId>> = Mutex::new(None);

/// Guards the one-time installation of the capturing subscriber.
static SUBSCRIBER_INSTALLED: Once = Once::new();

/// A window during which log records are captured.
///
/// Log assertions need a process-wide subscriber rather than a thread-local
/// one: an event can be emitted from a thread the test did not create (a
/// blocking pool, a runtime worker), and a thread-local default only sees
/// events from the thread that installed it. A process-wide subscriber plus a
/// capture window gives every thread's records to the test that opened it.
///
/// The window serialises *capturing* tests against each other, so one capture
/// never sees another's buffer. It cannot silence a test that is not capturing:
/// such a test still logs while a window is open, and those records land in the
/// open window. Assertions must therefore key on values only the capturing
/// test's own fixture can produce, or a concurrent test's identical log line
/// can satisfy the guard on its behalf.
#[must_use = "dropping the capture ends the record window"]
pub struct LogCapture {
    buffer: Arc<Mutex<String>>,
    _window: MutexGuard<'static, ()>,
}

impl LogCapture {
    /// Open a capture window. Blocks while another test holds one.
    ///
    /// # Panics
    ///
    /// When this thread already holds a window: the window lock is not
    /// reentrant, so a nested capture would hang the test binary.
    pub fn start() -> LogCapture {
        install_capturing_subscriber();
        let thread = std::thread::current().id();
        assert!(
            window_owner() != Some(thread),
            "this thread already holds a log capture window; LogCapture is not reentrant, so a nested capture would deadlock"
        );
        let window = CAPTURE_WINDOW
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let buffer = Arc::new(Mutex::new(String::new()));
        if let Ok(mut active) = ACTIVE_CAPTURE.lock() {
            *active = Some(Arc::clone(&buffer));
        }
        if let Ok(mut owner) = WINDOW_OWNER.lock() {
            *owner = Some(thread);
        }
        LogCapture {
            buffer,
            _window: window,
        }
    }

    /// Every log record written since the window opened, from any thread.
    pub fn text(&self) -> String {
        self.buffer
            .lock()
            .map(|text| text.clone())
            .unwrap_or_default()
    }
}

impl Drop for LogCapture {
    fn drop(&mut self) {
        if let Ok(mut active) = ACTIVE_CAPTURE.lock() {
            *active = None;
        }
        if let Ok(mut owner) = WINDOW_OWNER.lock() {
            *owner = None;
        }
    }
}

/// The thread holding the capture window, if any.
fn window_owner() -> Option<std::thread::ThreadId> {
    WINDOW_OWNER.lock().ok().and_then(|owner| *owner)
}

/// Install the capturing subscriber exactly once for the whole test process.
fn install_capturing_subscriber() {
    SUBSCRIBER_INSTALLED.call_once(|| {
        let subscriber = tracing_subscriber::fmt()
            .with_writer(ActiveCaptureWriter)
            // DEBUG so a test asserting on a DEBUG record sees it; the window,
            // not the level, is what keeps records apart.
            .with_max_level(tracing::Level::DEBUG)
            .with_ansi(false)
            .without_time()
            .finish();
        // Another crate in the process may have installed a default first. The
        // capture then records nothing and the asserting test fails loudly,
        // which is better than a silent empty-buffer pass.
        let _ = tracing::subscriber::set_global_default(subscriber);
    });
}

/// [`MakeWriter`](tracing_subscriber::fmt::MakeWriter) that appends to the
/// active capture buffer and discards records outside a capture window.
#[derive(Clone, Copy, Default)]
struct ActiveCaptureWriter;

impl Write for ActiveCaptureWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let active = ACTIVE_CAPTURE.lock().ok().and_then(|active| active.clone());
        if let Some(buffer) = active {
            if let Ok(mut text) = buffer.lock() {
                text.push_str(&String::from_utf8_lossy(bytes));
            }
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for ActiveCaptureWriter {
    type Writer = ActiveCaptureWriter;

    fn make_writer(&'a self) -> Self::Writer {
        *self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lofty::file::AudioFile;
    use tempfile::TempDir;

    #[test]
    fn fixtures_parse_as_audio_with_the_expected_bit_depth() {
        let dir = TempDir::new().unwrap();
        let sixteen = dir.path().join("16-bit.flac");
        let twenty_four = dir.path().join("24-bit.flac");
        write_minimal_flac(&sixteen);
        write_minimal_flac24(&twenty_four);

        assert_eq!(fs::metadata(&sixteen).unwrap().len(), 42);
        let sixteen_probe = Probe::open(&sixteen).unwrap().read().unwrap();
        assert_eq!(
            sixteen_probe.properties().bit_depth(),
            Some(16),
            "the fixture must read back as 16-bit for the quality tests"
        );
        let twenty_four_probe = Probe::open(&twenty_four).unwrap().read().unwrap();
        assert_eq!(
            twenty_four_probe.properties().bit_depth(),
            Some(24),
            "the 24-bit fixture must outrank the 16-bit one"
        );
    }

    #[test]
    fn tagged_fixture_exposes_both_artist_and_album() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("tagged.flac");
        write_minimal_flac_with_tags(&path, "Guns 'n' Roses", "Appetite for Destruction");

        let tagged = Probe::open(&path).unwrap().read().unwrap();
        let tag = tagged.primary_tag().expect("a tag must be present");
        assert_eq!(tag.artist().as_deref(), Some("Guns 'n' Roses"));
        assert_eq!(
            tag.album().as_deref(),
            Some("Appetite for Destruction"),
            "the album tag is what the scanner prefers over the folder name"
        );
        assert_eq!(
            tagged.properties().bit_depth(),
            Some(16),
            "adding tags must not disturb the audio properties"
        );
    }

    #[test]
    fn capture_records_are_attributed_to_the_open_window() {
        let capture = LogCapture::start();
        tracing::info!(target: "test_support", "inside the window");
        assert!(
            capture.text().contains("inside the window"),
            "a record written inside the window must be captured"
        );
        drop(capture);

        let second = LogCapture::start();
        assert!(
            !second.text().contains("inside the window"),
            "a closed window must not leak records into the next one"
        );
    }

    #[test]
    #[should_panic(expected = "not reentrant")]
    fn a_nested_capture_on_the_same_thread_fails_loudly() {
        let _outer = LogCapture::start();
        // The window lock is not reentrant, so without the guard this would hang
        // the test binary instead of reporting the mistake.
        let _inner = LogCapture::start();
    }
}
