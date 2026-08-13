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

#[cfg(test)]
mod tests {
    use super::*;

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
