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

    #[error("download error: {0}")]
    Download(String),

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
}
