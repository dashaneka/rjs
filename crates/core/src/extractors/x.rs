//! Uses the public embed API that powers tweets embedded on other websites.

use std::sync::LazyLock;
use std::time::{SystemTime, UNIX_EPOCH};

use regex::Regex;
use reqwest::header::USER_AGENT;
use reqwest::{StatusCode, Url};
use serde_json::Value;

use super::{dimension, number, segment_after, text, title_from_caption};
use crate::error::fail;
use crate::{Client, Format, Media, MediaKind, Platform, Result};

const API: &str = "https://cdn.syndication.twimg.com/tweet-result";

static SIZE_IN_URL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"/(\d+)x(\d+)/").unwrap());

pub(super) async fn extract(client: &Client, url: &Url) -> Result<Vec<Media>> {
    let id = segment_after(url, &["status", "statuses"])
        .filter(|s| s.bytes().all(|b| b.is_ascii_digit()))
        .ok_or_else(|| fail(Platform::X, "couldn't find the post ID in this link"))?;

    let resp = client
        .http
        .get(API)
        .query(&[("id", id), ("token", &token()), ("lang", "en")])
        .header(USER_AGENT, "Googlebot")
        .send()
        .await?;
    if resp.status() == StatusCode::NOT_FOUND {
        return Err(fail(
            Platform::X,
            "post not found (it may be deleted or the link is wrong)",
        ));
    }
    let tweet: Value = resp.error_for_status()?.json().await?;
    if tweet["__typename"] == "TweetTombstone" {
        return Err(fail(
            Platform::X,
            "this post can't be viewed without logging in (it may be age-restricted, private or deleted)",
        ));
    }

    let mut items = media_in(&tweet);
    if items.is_empty()
        && let Some(quoted) = tweet.get("quoted_tweet")
    {
        items = media_in(quoted);
    }

    let many = items.len() > 1;
    Ok(items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let is_photo = item["type"] == "photo";
            Media {
                platform: Platform::X,
                kind: if is_photo {
                    MediaKind::Image
                } else {
                    MediaKind::Video
                },
                id: if many {
                    format!("{id}_{}", i + 1)
                } else {
                    id.to_owned()
                },
                title: title_from_caption(text(&tweet, "/text")),
                author: text(&tweet, "/user/screen_name"),
                thumbnail: text(item, "/media_url_https"),
                duration: number(item, "/video_info/duration_millis").map(|ms| ms as f64 / 1000.0),
                formats: if is_photo {
                    photo_format(item).into_iter().collect()
                } else {
                    formats(item)
                },
            }
        })
        .collect())
}

/// Photos and videos attached directly, or a video inside a card (the preview box used by ads and links).
fn media_in(tweet: &Value) -> Vec<Value> {
    let wanted = |m: &&Value| {
        ["video", "animated_gif", "photo"]
            .iter()
            .any(|t| m["type"] == *t)
    };

    let direct: Vec<Value> = tweet["mediaDetails"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(wanted)
        .cloned()
        .collect();
    if !direct.is_empty() {
        return direct;
    }

    let Some(card) = tweet
        .pointer("/card/binding_values/unified_card/string_value")
        .and_then(Value::as_str)
        .and_then(|s| serde_json::from_str::<Value>(s).ok())
    else {
        return Vec::new();
    };
    card["media_entities"]
        .as_object()
        .into_iter()
        .flat_map(|entities| entities.values())
        .filter(wanted)
        .cloned()
        .collect()
}

/// Photo URLs look like `https://pbs.twimg.com/media/<name>.jpg`; `name=orig` asks for the uploaded original.
fn photo_format(photo: &Value) -> Option<Format> {
    let url = photo["media_url_https"].as_str()?;
    let (base, ext) = url.rsplit_once('.').filter(|(_, ext)| !ext.contains('/'))?;
    Some(Format {
        ext: if ext == "jpeg" {
            "jpg".into()
        } else {
            ext.into()
        },
        width: dimension(photo, "/original_info/width"),
        height: dimension(photo, "/original_info/height"),
        ..Format::image("orig", format!("{base}?format={ext}&name=orig"))
    })
}

fn formats(video: &Value) -> Vec<Format> {
    let has_audio = video["type"] != "animated_gif";
    video
        .pointer("/video_info/variants")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|v| v["content_type"] == "video/mp4")
        .filter_map(|v| {
            let url = v["url"].as_str()?;
            let size = SIZE_IN_URL.captures(url);
            let side = |i| size.as_ref()?.get(i)?.as_str().parse().ok();
            let (width, height) = (side(1), side(2));
            let bitrate = number(v, "/bitrate");
            Some(Format {
                width,
                height,
                bitrate,
                has_audio,
                ..Format::mp4(
                    match height {
                        Some(h) => format!("{h}p"),
                        None => format!("mp4-{}", bitrate.unwrap_or(0) / 1000),
                    },
                    url,
                )
            })
        })
        .map(|mut f| {
            if f.width.is_none() {
                f.width = dimension(video, "/original_info/width");
                f.height = dimension(video, "/original_info/height");
            }
            f
        })
        .collect()
}

/// The embed API wants a token but accepts any short base-36 string.
fn token() -> String {
    let mut n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(1);
    let mut out = String::new();
    while n > 0 && out.len() < 10 {
        out.push(char::from_digit((n % 36) as u32, 36).unwrap());
        n /= 36;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(s: &str) -> Option<String> {
        segment_after(&Url::parse(s).unwrap(), &["status", "statuses"]).map(str::to_owned)
    }

    #[test]
    fn finds_post_id() {
        assert_eq!(
            id("https://x.com/user/status/123?s=20").as_deref(),
            Some("123")
        );
        assert_eq!(
            id("https://twitter.com/i/web/status/456").as_deref(),
            Some("456")
        );
        assert_eq!(id("https://x.com/user").as_deref(), None);
    }

    #[test]
    fn reads_original_photo() {
        let photo = serde_json::json!({
            "type": "photo",
            "media_url_https": "https://pbs.twimg.com/media/CGMNYZvW0AIVoom.png",
            "original_info": { "width": 480, "height": 758 }
        });
        let f = photo_format(&photo).unwrap();
        assert_eq!(
            f.url,
            "https://pbs.twimg.com/media/CGMNYZvW0AIVoom?format=png&name=orig"
        );
        assert_eq!(
            (f.ext.as_str(), f.width, f.has_video),
            ("png", Some(480), false)
        );
    }

    #[test]
    fn reads_variants() {
        let video: Value = serde_json::json!({
            "type": "video",
            "video_info": { "variants": [
                { "content_type": "application/x-mpegURL", "url": "https://v/pl.m3u8" },
                { "content_type": "video/mp4", "bitrate": 832000, "url": "https://v/vid/540x540/a.mp4" },
            ]}
        });
        let f = formats(&video);
        assert_eq!(f.len(), 1);
        assert_eq!(
            (f[0].width, f[0].height, f[0].id.as_str()),
            (Some(540), Some(540), "540p")
        );
    }
}
