//! Eporner: the page carries a hash that, reshuffled the way its player does,
//! unlocks a JSON list of MP4s.

use std::sync::LazyLock;

use regex::Regex;
use reqwest::Url;

use super::{json, meta, segment_after, with_referer};
use crate::error::fail;
use crate::{Client, Format, Media, MediaKind, Platform, Result};

const HOME: &str = "https://www.eporner.com/";

static HASH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"hash\s*[:=]\s*["']([0-9a-f]{32})"#).unwrap());
static LABEL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(\d+)p(?:@(\d+)fps)?").unwrap());

pub(super) async fn extract(client: &Client, url: &Url) -> Result<Vec<Media>> {
    let page = client.page(url.as_str()).await?;
    let id = video_id(&page.url)
        .or_else(|| video_id(url))
        .ok_or_else(|| fail(Platform::Eporner, "couldn't find the video ID in this link"))?;
    let hash = HASH
        .captures(&page.html)
        .map(|c| player_hash(&c[1]))
        .ok_or_else(|| fail(Platform::Eporner, "couldn't find the video on the page"))?;

    let api = format!(
        "https://www.eporner.com/xhr/video/{id}?hash={hash}&device=generic&domain=www.eporner.com&fallback=false"
    );
    let video = json(client, &api, HOME).await?;
    if video["available"] == false {
        let message = video["message"]
            .as_str()
            .unwrap_or("this video isn't available");
        return Err(fail(Platform::Eporner, message));
    }

    let formats: Vec<Format> = video["sources"]["mp4"]
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(label, source)| {
            let src = source["src"].as_str().filter(|s| s.starts_with("http"))?;
            let caps = LABEL.captures(label)?;
            let height: u32 = caps[1].parse().ok()?;
            let id = match caps.get(2) {
                Some(fps) => format!("{height}p{}", fps.as_str()),
                None => format!("{height}p"),
            };
            Some(Format {
                height: Some(height),
                ..Format::mp4(id, src)
            })
        })
        .collect();
    if formats.is_empty() {
        return Err(fail(Platform::Eporner, "no video found on the page"));
    }

    Ok(vec![Media {
        platform: Platform::Eporner,
        kind: MediaKind::Video,
        id,
        title: meta(&page.html, "og:title"),
        author: None,
        thumbnail: meta(&page.html, "og:image"),
        duration: None,
        formats: with_referer(formats, HOME),
    }])
}

/// `/video-<id>/<title>/`, `/hd-porn/<id>/<title>/` or `/embed/<id>/`.
fn video_id(url: &Url) -> Option<String> {
    let id = segment_after(url, &["hd-porn", "embed"])
        .map(str::to_owned)
        .or_else(|| {
            url.path_segments()?
                .find_map(|s| s.strip_prefix("video-"))
                .map(str::to_owned)
        })?;
    Some(id).filter(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric()))
}

/// The player sends the page's 32-digit hex hash as four base-36 numbers.
fn player_hash(hex: &str) -> String {
    (0..4)
        .filter_map(|i| u32::from_str_radix(hex.get(i * 8..i * 8 + 8)?, 16).ok())
        .map(base36)
        .collect()
}

fn base36(mut n: u32) -> String {
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = Vec::new();
    loop {
        out.push(DIGITS[(n % 36) as usize]);
        n /= 36;
        if n == 0 {
            break;
        }
    }
    out.reverse();
    String::from_utf8(out).expect("ASCII digits")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_video_ids() {
        let id = |s: &str| video_id(&Url::parse(s).unwrap());
        assert_eq!(
            id("https://www.eporner.com/video-AbC123/some-title/").as_deref(),
            Some("AbC123")
        );
        assert_eq!(
            id("https://www.eporner.com/hd-porn/95008/Title/").as_deref(),
            Some("95008")
        );
        assert_eq!(id("https://www.eporner.com/"), None);
    }

    #[test]
    fn reshuffles_hash_like_the_player() {
        assert_eq!(base36(0), "0");
        assert_eq!(base36(u32::MAX), "1z141z3");
        assert_eq!(
            player_hash("0000000aFFFFFFFF0000002400000000"),
            "a1z141z3100"
        );
    }
}
