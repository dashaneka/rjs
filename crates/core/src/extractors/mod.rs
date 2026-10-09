mod facebook;
mod instagram;
mod tiktok;
mod x;

use serde_json::Value;

use reqwest::Url;

pub(crate) use tiktok::Pager;

use crate::error::fail;
use crate::platform::parse_url;
use crate::{Client, Error, Media, Platform, Profile, Result};

fn resolve(input: &str) -> Result<(Url, Platform)> {
    let url = parse_url(input)?;
    let platform =
        Platform::detect(&url).ok_or_else(|| Error::Unsupported(input.trim().to_owned()))?;
    Ok((url, platform))
}

pub(crate) fn is_profile(input: &str) -> bool {
    matches!(resolve(input), Ok((url, Platform::TikTok)) if tiktok::is_profile(&url))
}

pub(crate) async fn profile(client: &Client, input: &str) -> Result<Profile> {
    match resolve(input)? {
        (url, Platform::TikTok) => tiktok::profile(client, &url).await,
        (_, platform) => Err(fail(
            platform,
            "whole profiles can only be downloaded from TikTok for now",
        )),
    }
}

pub(crate) async fn avatar(client: &Client, input: &str) -> Result<Media> {
    match resolve(input)? {
        (url, Platform::TikTok) => tiktok::avatar(client, &url).await,
        (_, platform) => Err(fail(
            platform,
            "profile pictures can only be downloaded from TikTok for now",
        )),
    }
}

pub(crate) async fn extract(client: &Client, input: &str) -> Result<Vec<Media>> {
    let (url, platform) = resolve(input)?;
    let media = match platform {
        Platform::X => x::extract(client, &url).await?,
        Platform::TikTok => tiktok::extract(client, &url).await?,
        Platform::Instagram => instagram::extract(client, &url).await?,
        Platform::Facebook => facebook::extract(client, &url).await?,
    };
    if media.is_empty() {
        return Err(fail(platform, "no videos or pictures found in this post"));
    }
    Ok(media)
}

fn text(v: &Value, pointer: &str) -> Option<String> {
    v.pointer(pointer)?
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// Reads a number that sites sometimes send as a string.
fn number(v: &Value, pointer: &str) -> Option<u64> {
    let v = v.pointer(pointer)?;
    v.as_u64()
        .or_else(|| v.as_f64().map(|f| f as u64))
        .or_else(|| v.as_str()?.parse().ok())
        .filter(|&n| n > 0)
}

fn dimension(v: &Value, pointer: &str) -> Option<u32> {
    number(v, pointer).and_then(|n| u32::try_from(n).ok())
}

/// Shortens post captions into something usable as a title.
fn title_from_caption(caption: Option<String>) -> Option<String> {
    let line = caption?
        .lines()
        .find(|l| !l.trim().is_empty())?
        .trim()
        .to_owned();
    Some(match line.char_indices().nth(100) {
        Some((cut, _)) => format!("{}…", &line[..cut]),
        None => line,
    })
}

/// The path segment right after the first segment named in `after`, e.g. the ID in `/status/<id>`.
fn segment_after<'a>(url: &'a reqwest::Url, after: &[&str]) -> Option<&'a str> {
    let mut segments = url.path_segments()?;
    segments.find(|s| after.contains(s))?;
    segments.next().filter(|s| !s.is_empty())
}
