use crate::Platform;

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Stable categories for callers; inspect the error itself for details.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ErrorKind {
    Unsupported,
    Extraction,
    Network,
    Http,
    Timeout,
    Tls,
    Filesystem,
    InvalidMedia,
    Processing,
    Cookies,
    Cancelled,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("operation cancelled")]
    Cancelled,
    #[error("unsupported link: {0}")]
    Unsupported(String),

    #[error("{platform}: {message}")]
    Extract { platform: Platform, message: String },

    #[error("network error: {0}")]
    Network(#[from] reqwest::Error),

    #[error("secure connection setup: {0}")]
    Tls(#[from] rustls::Error),

    #[error("file error: {0}")]
    Io(#[from] std::io::Error),

    #[error("invalid media response: {0}")]
    InvalidMedia(&'static str),

    #[error("ffmpeg: {0}")]
    Ffmpeg(String),

    /// A problem with a video served in pieces (HLS).
    #[error("{0}")]
    Stream(String),

    #[error("cookies: {0}")]
    Cookies(String),
}

impl Error {
    pub fn kind(&self) -> ErrorKind {
        match self {
            Self::Unsupported(_) => ErrorKind::Unsupported,
            Self::Extract { .. } => ErrorKind::Extraction,
            Self::Network(error) if error.is_timeout() => ErrorKind::Timeout,
            Self::Network(error) if error.status().is_some() => ErrorKind::Http,
            Self::Network(_) => ErrorKind::Network,
            Self::Tls(_) => ErrorKind::Tls,
            Self::Io(_) => ErrorKind::Filesystem,
            Self::InvalidMedia(_) => ErrorKind::InvalidMedia,
            Self::Ffmpeg(_) | Self::Stream(_) => ErrorKind::Processing,
            Self::Cookies(_) => ErrorKind::Cookies,
            Self::Cancelled => ErrorKind::Cancelled,
        }
    }

    /// Whether repeating a request may help. Callers must still bound retries.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Network(error) => match error.status() {
                Some(status) => status.is_server_error() || matches!(status.as_u16(), 408 | 429),
                None => error.is_timeout() || error.is_connect() || error.is_body(),
            },
            _ => false,
        }
    }
}

pub(crate) fn fail(platform: Platform, message: impl Into<String>) -> Error {
    Error::Extract {
        platform,
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn deterministic_errors_are_not_retried() {
        for error in [
            Error::Cancelled,
            Error::InvalidMedia("invalid"),
            Error::Cookies("missing".into()),
            Error::Unsupported("url".into()),
            Error::Io(std::io::Error::other("disk")),
        ] {
            assert!(!error.is_retryable());
        }
        assert_eq!(Error::Cancelled.kind(), ErrorKind::Cancelled);
        assert_eq!(Error::InvalidMedia("html").kind(), ErrorKind::InvalidMedia);
    }
}
