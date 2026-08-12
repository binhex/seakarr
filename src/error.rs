use thiserror::Error;

#[derive(Error, Debug)]
pub enum SeakarrError {
    #[error("configuration error: {0}")]
    Config(String),

    #[error("soulseek authentication failed after {attempts} attempts: {reason}")]
    Auth { attempts: u32, reason: String },

    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),

    #[error("soulseek client error: {0}")]
    Client(String),

    #[error("scanner error: {0}")]
    Scanner(String),

    #[error("download error: {0}")]
    Download(String),

    #[error("pid lock error: {0}")]
    PidLock(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, SeakarrError>;
