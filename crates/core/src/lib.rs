//! Find and download videos and pictures from TikTok, Instagram, Facebook, X
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

mod client;
pub mod cookies;
mod download;
mod error;
mod extractors;
mod media;
mod platform;
mod profile;

pub use client::Client;
pub use cookies::Cookie;
pub use download::{DownloadOptions, NoProgress, Progress, Saved, download_media};
pub use error::{Error, Result};
pub use media::{Format, Media, MediaKind, Protocol, Selection};
pub use platform::Platform;
pub use profile::Profile;

/// Looks up every video and picture in the post behind `url`.
pub async fn extract(client: &Client, url: &str) -> Result<Vec<Media>> {
    extractors::extract(client, url).await
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
