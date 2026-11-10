//! Find and download videos and pictures from TikTok, Instagram, Facebook, X, Pinterest
//! and a handful of adult video sites.
//!
//! ```no_run
//! # async fn run() -> rjs_core::Result<()> {
//! let client = rjs_core::Client::new()?;
//! for media in rjs_core::extract(&client, "https://x.com/user/status/123").await? {
//!     let options = rjs_core::DownloadOptions::default();
//!     rjs_core::download_media(&client, &media, ".".as_ref(), &options, &rjs_core::NoProgress).await?;
//! }
//! # Ok(())
//! # }
//! ```

mod cancel;
mod client;
pub mod cookies;
mod download;
mod error;
mod extractors;
mod media;
mod platform;
mod profile;

pub use cancel::CancellationToken;
pub use client::{Client, ClientBuilder};
pub use cookies::Cookie;
pub use download::{DownloadOptions, NoProgress, Progress, Saved, download_media};
pub use error::{Error, ErrorKind, Result};
pub use media::{Format, Media, MediaKind, Protocol, Selection};
pub use platform::Platform;
pub use profile::Profile;

/// Looks up every video and picture in the post behind `url`.
pub async fn extract(client: &Client, url: &str) -> Result<Vec<Media>> {
    extractors::extract(client, url).await
}

/// Extract with cooperative cancellation. Cancelling drops pending HTTP requests.
pub async fn extract_with_cancel(
    client: &Client,
    url: &str,
    cancel: &CancellationToken,
) -> Result<Vec<Media>> {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(Error::Cancelled),
        result = extract(client, url) => result,
    }
}

/// Download with cancellation. Partial files can remain for safe resumption.
pub async fn download_media_with_cancel(
    client: &Client,
    media: &Media,
    dir: &std::path::Path,
    options: &DownloadOptions,
    progress: &dyn Progress,
    cancel: &CancellationToken,
) -> Result<Saved> {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(Error::Cancelled),
        result = download_media(client, media, dir, options, progress) => result,
    }
}

/// Whether `url` points at a whole account rather than a single post.
pub fn is_profile(url: &str) -> bool {
    extractors::is_profile(url)
}

/// Loads the account behind a profile link.
pub async fn profile(client: &Client, url: &str) -> Result<Profile> {
    extractors::profile(client, url).await
}

/// The profile picture of the account behind `url`, or of whoever posted it.
pub async fn avatar(client: &Client, url: &str) -> Result<Media> {
    extractors::avatar(client, url).await
}
