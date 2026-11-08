//! Human-friendly byte/speed formatting for download progress display.

/// Format a byte count into a human-friendly string.
///
/// | Range      | Format      | Example     |
/// |------------|-------------|-------------|
/// | < 1024     | `{n} B`     | `512 B`     |
/// | < 1 MB     | `{n:.1} KB` | `256.5 KB`  |
/// | < 1 GB     | `{n:.1} MB` | `31.8 MB`   |
/// | >= 1 GB    | `{n:.1} GB` | `1.2 GB`    |
pub fn format_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;

    let b = bytes as f64;
    if b < KB {
        format!("{bytes} B")
    } else if b < MB {
        format!("{:.1} KB", b / KB)
    } else if b < GB {
        format!("{:.1} MB", b / MB)
    } else {
        format!("{:.1} GB", b / GB)
    }
}

/// Format a speed in bytes/sec into a human-friendly string.
///
/// | Range        | Format       | Example      |
/// |--------------|--------------|--------------|
/// | < 1024 B/s   | `{n} B/s`    | `512 B/s`    |
/// | < 1 MB/s     | `{n:.1} KB/s`| `256.5 KB/s` |
/// | >= 1 MB/s    | `{n:.1} MB/s`| `2.5 MB/s`   |
pub fn format_speed(bytes_per_sec: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;

    let b = bytes_per_sec as f64;
    if b < KB {
        format!("{bytes_per_sec} B/s")
    } else if b < MB {
        format!("{:.1} KB/s", b / KB)
    } else {
        format!("{:.1} MB/s", b / MB)
    }
}

/// Format an elapsed duration for queue-wait reporting.
///
/// | Range     | Format      | Example   |
/// |-----------|-------------|-----------|
/// | < 1 min   | `{s}s`      | `45s`     |
/// | < 1 hour  | `{m}m {s}s` | `21m 40s` |
/// | >= 1 hour | `{h}h {m}m` | `1h 3m`   |
pub fn format_duration(elapsed: std::time::Duration) -> String {
    let total = elapsed.as_secs();
    let hours = total / 3600;
    let minutes = (total % 3600) / 60;
    let seconds = total % 60;
    if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m {seconds}s")
    } else {
        format!("{seconds}s")
    }
}

/// Unicode format characters (category Cf) that are invisible but reorder or
/// hide the text around them: zero-width characters, the bidirectional
/// overrides and isolates, the Arabic letter mark, the prepended concatenation
/// marks, the Khmer vowel signs, the interlinear annotation anchors, the tag
/// characters (the standard ASCII-smuggling vector), and the byte-order mark.
/// Control characters are handled separately by [`single_line`].
pub(crate) fn is_format_character(character: char) -> bool {
    matches!(
        character,
        '\u{00AD}'
            | '\u{0600}'..='\u{0605}'
            | '\u{061C}'
            | '\u{06DD}'
            | '\u{070F}'
            | '\u{0890}'..='\u{0891}'
            | '\u{08E2}'
            | '\u{17B4}'..='\u{17B5}'
            | '\u{180E}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{206F}'
            | '\u{FFF9}'..='\u{FFFB}'
            | '\u{1107F}'
            | '\u{110BD}'
            | '\u{110CD}'
            | '\u{13430}'..='\u{1343F}'
            | '\u{1BCA0}'..='\u{1BCA3}'
            | '\u{1D173}'..='\u{1D17A}'
            | '\u{E0001}'
            | '\u{E0020}'..='\u{E007F}'
            | '\u{FEFF}'
    )
}

/// Collapse control characters so peer-supplied text cannot forge or corrupt a log or
/// summary line.
///
/// A Soulseek peer controls both its username and the file names it advertises, and both
/// reach the run summary through an album's reason string. A raw ESC sequence there can
/// repaint the operator's terminal or fake a summary line - the summary is the record of
/// what the run did, so it must read as data no matter what the network sent.
pub fn single_line(text: &str) -> String {
    text.chars()
        .map(|character| {
            // Control characters (ESC and friends) and Unicode format characters (the bidi
            // overrides U+202E, zero-width marks) can both repaint or reorder a line the
            // operator reads, and a peer or provider controls this text.
            if character.is_control() || is_format_character(character) {
                ' '
            } else {
                character
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn format_duration_seconds_only() {
        assert_eq!(format_duration(Duration::from_secs(0)), "0s");
        assert_eq!(format_duration(Duration::from_secs(45)), "45s");
        assert_eq!(format_duration(Duration::from_secs(59)), "59s");
    }

    #[test]
    fn format_duration_minutes_and_seconds() {
        // Matches the reported example: a 21m 40s queue wait.
        assert_eq!(format_duration(Duration::from_secs(60)), "1m 0s");
        assert_eq!(format_duration(Duration::from_secs(1_300)), "21m 40s");
    }

    #[test]
    fn format_duration_hours() {
        assert_eq!(format_duration(Duration::from_secs(3_600)), "1h 0m");
        assert_eq!(format_duration(Duration::from_secs(3_780)), "1h 3m");
    }

    #[test]
    fn test_format_bytes_zero() {
        assert_eq!(format_bytes(0), "0 B");
    }

    #[test]
    fn test_format_bytes_under_1kb() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1023), "1023 B");
    }

    #[test]
    fn test_format_bytes_kb_range() {
        assert_eq!(format_bytes(1024), "1.0 KB");
        assert_eq!(format_bytes(1536), "1.5 KB");
        assert_eq!(format_bytes(102_400), "100.0 KB");
    }

    #[test]
    fn test_format_bytes_mb_range() {
        assert_eq!(format_bytes(1_048_576), "1.0 MB");
        assert_eq!(format_bytes(10_485_760), "10.0 MB");
        assert_eq!(format_bytes(33_304_229), "31.8 MB");
    }

    #[test]
    fn test_format_bytes_gb_range() {
        assert_eq!(format_bytes(1_073_741_824), "1.0 GB");
        assert_eq!(format_bytes(2_147_483_648), "2.0 GB");
    }

    #[test]
    fn test_format_speed_zero() {
        assert_eq!(format_speed(0), "0 B/s");
    }

    #[test]
    fn test_format_speed_under_1kbs() {
        assert_eq!(format_speed(512), "512 B/s");
    }

    #[test]
    fn test_format_speed_kbs_and_mbs_range() {
        assert_eq!(format_speed(1024), "1.0 KB/s");
        assert_eq!(format_speed(2_649_609), "2.5 MB/s"); // from the user's log
    }

    #[test]
    fn test_format_speed_mbs_range() {
        assert_eq!(format_speed(1_048_576), "1.0 MB/s");
        assert_eq!(format_speed(2_850_380), "2.7 MB/s"); // from the user's log
    }
}
