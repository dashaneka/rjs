use crate::Platform;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("unsupported link: {0}")]
    Unsupported(String),

    #[error("{platform}: {message}")]
    Extract { platform: Platform, message: String },

    #[error("network error: {0}")]
    Network(#[from] reqwest::Error),

    #[error("file error: {0}")]
    Io(#[from] std::io::Error),

    #[error("ffmpeg: {0}")]
    Ffmpeg(String),

    /// A problem with a video served in pieces (HLS).
    #[error("{0}")]
    Stream(String),

    #[error("cookies: {0}")]
    Cookies(String),
}

pub(crate) fn fail(platform: Platform, message: impl Into<String>) -> Error {
    Error::Extract {
        platform,
        message: message.into(),
    }
}
