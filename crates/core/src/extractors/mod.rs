mod eporner;
mod facebook;
mod instagram;
mod pinterest;
mod pornhub;
mod spankbang;
mod tiktok;
mod x;
mod xhamster;
mod xvideos;

use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

use reqwest::Url;

use crate::error::fail;
use crate::platform::parse_url;
use crate::{Client, Error, Format, Media, Platform, Profile, Result};

fn resolve(input: &str) -> Result<(Url, Platform)> {
    let url = parse_url(input)?;
    let platform =
        Platform::detect(&url).ok_or_else(|| Error::Unsupported(input.trim().to_owned()))?;
    Ok((url, platform))
}

/// Walks through the posts of a profile (or Pinterest board) a page at a time.
pub(crate) enum Pager {
    TikTok(tiktok::Pager),
    Pinterest(pinterest::Pager),
}

impl Pager {
    /// The next batch of posts, possibly empty; `None` once there are no more.
    pub(crate) async fn next(&mut self, client: &Client) -> Result<Option<Vec<Media>>> {
        match self {
            Self::TikTok(pager) => pager.next(client).await,
            Self::Pinterest(pager) => pager.next(client).await,
        }
    }
}

pub(crate) fn is_profile(input: &str) -> bool {
    match resolve(input) {
        Ok((url, Platform::TikTok)) => tiktok::is_profile(&url),
        Ok((url, Platform::Pinterest)) => pinterest::is_profile(&url),
        _ => false,
    }
}

pub(crate) async fn profile(client: &Client, input: &str) -> Result<Profile> {
    match resolve(input)? {
        (url, Platform::TikTok) => tiktok::profile(client, &url).await,
        (url, Platform::Pinterest) => pinterest::profile(client, &url).await,
        (_, platform) => Err(fail(
            platform,
            "whole profiles can only be downloaded from TikTok and Pinterest for now",
        )),
    }
}

pub(crate) async fn avatar(client: &Client, input: &str) -> Result<Media> {
    match resolve(input)? {
        (url, Platform::TikTok) => tiktok::avatar(client, &url).await,
        (url, Platform::Pinterest) => pinterest::avatar(client, &url).await,
        (_, platform) => Err(fail(
            platform,
            "profile pictures can only be downloaded from TikTok and Pinterest for now",
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
        Platform::Pinterest => pinterest::extract(client, &url).await?,
        Platform::Pornhub | Platform::YouPorn | Platform::RedTube => {
            pornhub::extract(client, &url, platform).await?
        }
        Platform::Eporner => eporner::extract(client, &url).await?,
        Platform::XVideos | Platform::Xnxx => xvideos::extract(client, &url, platform).await?,
        Platform::XHamster => xhamster::extract(client, &url).await?,
        Platform::SpankBang => spankbang::extract(client, &url).await?,
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

/// The `content` of a `<meta property="…">` (or `name="…"`) tag, decoded.
fn meta(html: &str, property: &str) -> Option<String> {
    let name = regex::escape(property);
    let patterns = [
        format!(r#"<meta[^>]+(?:property|name)=["']{name}["'][^>]*\bcontent=["']([^"']*)"#),
        format!(r#"<meta[^>]+\bcontent=["']([^"']*)["'][^>]*(?:property|name)=["']{name}["']"#),
    ];
    patterns.iter().find_map(|p| {
        let found = Regex::new(p)
            .ok()?
            .captures(html)?
            .get(1)?
            .as_str()
            .trim()
            .to_owned();
        Some(unescape_html(&found)).filter(|s| !s.is_empty())
    })
}

static ENTITY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"&(#x[0-9a-fA-F]+|#[0-9]+|[a-z]+);").unwrap());

fn unescape_html(s: &str) -> String {
    ENTITY
        .replace_all(s, |c: &regex::Captures| {
            let name = &c[1];
            let decoded = if let Some(hex) = name.strip_prefix("#x") {
                u32::from_str_radix(hex, 16).ok().and_then(char::from_u32)
            } else if let Some(dec) = name.strip_prefix('#') {
                dec.parse().ok().and_then(char::from_u32)
            } else {
                match name {
                    "amp" => Some('&'),
                    "quot" => Some('"'),
                    "apos" => Some('\''),
                    "lt" => Some('<'),
                    "gt" => Some('>'),
                    _ => None,
                }
            };
            decoded.map_or_else(|| c[0].to_owned(), String::from)
        })
        .into_owned()
}

/// Adds a `Referer` header, which these sites' file servers expect.
fn with_referer(mut formats: Vec<Format>, referer: &str) -> Vec<Format> {
    for f in &mut formats {
        f.headers.push(("Referer".into(), referer.into()));
    }
    formats
}

/// Downloads JSON, sending `referer`.
async fn json(client: &Client, url: &str, referer: &str) -> Result<Value> {
    Ok(client
        .http_for(url)
        .get(url)
        .header(reqwest::header::REFERER, referer)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?)
}
