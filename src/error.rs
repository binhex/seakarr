use thiserror::Error;

#[derive(Error, Debug)]
pub enum SeakarrError {
    #[error("configuration error: {0}")]
    Config(String),

    #[error("soulseek authentication failed after {attempts} attempts: {reason}")]
    Auth { attempts: u32, reason: String },

    #[error("server connection lost: {reason}")]
    Disconnected { reason: String },

    #[error("another login took over this username: {reason}")]
    Displaced { reason: String },

    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),

    #[error("soulseek client error: {0}")]
    Client(String),

    #[error("scanner error: {0}")]
    Scanner(String),

    #[error("cancelled by user")]
    Cancelled,

    #[error("musicbrainz error: {0}")]
    MusicBrainz(String),

    #[error("download error: {0}")]
    Download(String),

    /// A transfer whose smoothed speed was below `min_upload_speed_kbps` when
    /// checked at least `speed_check_wait_secs` after the transfer started. The
    /// rate is smoothed over the transfer's own samples, so a jittery sample
    /// window cannot trigger it once the average is seeded; the first sample
    /// judged after the wait may blend with the samples that preceded it. The
    /// same peer and file cannot get faster by being asked again — re-requesting
    /// only puts the file back in that peer's queue — so this error is never
    /// retried in place and the candidate fallback moves on, exactly as for
    /// `QualityRejected`. The display text matches `Download` so run summaries
    /// read unchanged.
    #[error("download error: {0}")]
    SlowDownload(String),

    #[error("quality verification rejected download: {0}")]
    QualityRejected(String),

    /// A queued download exceeded `max_queue_time_secs` or
    /// `max_start_time_secs`, or the peer reported a queue position outside
    /// `max_queue_length`. Waiting longer on the same peer cannot help, so
    /// this error is never retried in place — the candidate fallback moves on.
    #[error("download queue timeout: {0}")]
    QueueTimeout(String),

    #[error("pid lock error: {0}")]
    PidLock(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, SeakarrError>;

#[cfg(test)]
mod tests {
    use super::SeakarrError;

    #[test]
    fn queue_timeout_error_displays_reason() {
        let error = SeakarrError::QueueTimeout("queue position 4 exceeds limit 3".into());
        assert_eq!(
            error.to_string(),
            "download queue timeout: queue position 4 exceeds limit 3"
        );
    }

    #[test]
    fn slow_download_error_reads_like_a_download_error() {
        // Run summaries print this prefix, so the variant that carries the
        // permanent below-floor verdict must not change how the run reads.
        let error = SeakarrError::SlowDownload("speed 310 KB/s below minimum 400 KB/s".into());
        assert_eq!(
            error.to_string(),
            "download error: speed 310 KB/s below minimum 400 KB/s"
        );
    }
}
