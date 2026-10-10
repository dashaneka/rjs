//! Pornhub and its sister sites YouPorn and RedTube, which share a player. The
//! page lists the video's sources; a link in that list returns an MP4 per quality.

use std::sync::LazyLock;

use regex::Regex;
use reqwest::Url;
use serde_json::Value;

use super::{dimension, json, meta, number, segment_after, text, with_referer};
use crate::error::fail;
use crate::{Client, Format, Media, MediaKind, Platform, Result};

static FLASHVARS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"var\s+flashvars_\d+\s*=\s*(\{.+?\});").unwrap());
static DEFINITIONS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"mediaDefinitions?["']?\s*:\s*(\[.+?\}\s*\])"#).unwrap());

pub(super) async fn extract(client: &Client, url: &Url, platform: Platform) -> Result<Vec<Media>> {
    let id = video_id(url, platform)
        .ok_or_else(|| fail(platform, "couldn't find the video ID in this link"))?;
    let (site, page_url) = match platform {
        Platform::Pornhub => (
            "pornhub.com",
            format!("https://www.pornhub.com/view_video.php?viewkey={id}"),
        ),
        Platform::YouPorn => (
            "youporn.com",
            format!("https://www.youporn.com/watch/{id}/"),
        ),
        _ => ("redtube.com", format!("https://www.redtube.com/{id}")),
    };
    let home = format!("https://www.{site}/");
    let home_url = Url::parse(&home).expect("valid URL");
    // What the site's age check sets once a visitor confirms they're an adult.
    for cookie in [
        "age_verified",
        "accessAgeDisclaimerPH",
        "accessAgeDisclaimerUK",
        "accessPH",
    ] {
        client.set_cookie(&format!("{cookie}=1; Domain={site}; Path=/"), &home_url);
    }
    client.set_cookie(&format!("platform=pc; Domain={site}; Path=/"), &home_url);

    let page = client.page(&page_url).await?;
    let html = &page.html;
    if html.contains(r#"class="geoBlocked""#) || html.contains("unavailable in your country") {
        return Err(fail(platform, "this video isn't available in your country"));
    }

    let flashvars: Value = FLASHVARS
        .captures(html)
        .and_then(|c| serde_json::from_str(&c[1]).ok())
        .unwrap_or(Value::Null);
    let definitions = match flashvars["mediaDefinitions"].as_array() {
        Some(list) => list.clone(),
        None => DEFINITIONS
            .captures(html)
            .and_then(|c| serde_json::from_str::<Vec<Value>>(&c[1]).ok())
            .unwrap_or_default(),
    };

    let mut formats = Vec::new();
    for kind in ["mp4", "hls"] {
        for definition in definitions.iter().filter(|d| d["format"] == kind) {
            let Some(link) = text(definition, "/videoUrl").and_then(|u| home_url.join(&u).ok())
            else {
                continue;
            };
            // Without a quality, the link returns the list of qualities.
            let sources = if number(definition, "/quality").is_some() {
                vec![definition.clone()]
            } else {
                match json(client, link.as_str(), &home).await {
                    Ok(Value::Array(list)) => list,
                    _ => Vec::new(),
                }
            };
            formats.extend(sources.iter().filter_map(|s| source_format(s, &home_url)));
        }
        // MP4s cover the same qualities as the HLS streams and resume better.
        if !formats.is_empty() {
            break;
        }
    }
    if formats.is_empty() {
        return Err(fail(
            platform,
            "no video found on the page (it may be removed, private or need a login)",
        ));
    }

    let duration = number(&flashvars, "/video_duration")
        .or_else(|| meta(html, "video:duration")?.parse().ok())
        .or_else(|| meta(html, "og:video:duration")?.parse().ok());
    Ok(vec![Media {
        platform,
        kind: MediaKind::Video,
        id,
        title: meta(html, "og:title").or_else(|| meta(html, "twitter:title")),
        author: None,
        thumbnail: meta(html, "og:image"),
        duration: duration.map(|s| s as f64),
        formats: with_referer(formats, &home),
    }])
}

fn source_format(source: &Value, home: &Url) -> Option<Format> {
    let url = home.join(&text(source, "/videoUrl")?).ok()?;
    let height = dimension(source, "/quality");
    let id = height.map_or_else(|| "mp4".into(), |h| format!("{h}p"));
    let format = if source["format"] == "hls" || url.path().ends_with(".m3u8") {
        Format::hls(format!("hls-{id}"), url.as_str())
    } else {
        Format::mp4(id, url.as_str())
    };
    Some(Format {
        width: dimension(source, "/width"),
        height: dimension(source, "/height").or(height),
        ..format
    })
}

/// Pornhub: `view_video.php?viewkey=<id>` or `/embed/<id>`; YouPorn: `/watch/<id>/`;
/// RedTube: `/<id>` or `embed.redtube.com/?id=<id>`.
fn video_id(url: &Url, platform: Platform) -> Option<String> {
    let query = |name: &str| {
        url.query_pairs()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.into_owned())
    };
    let segment = |after: &[&str]| segment_after(url, after).map(str::to_owned);
    let found = match platform {
        Platform::Pornhub => query("viewkey").or_else(|| segment(&["embed"])),
        Platform::YouPorn => segment(&["watch", "embed"]),
        _ => query("id").or_else(|| {
            url.path_segments()?
                .find(|s| !s.is_empty())
                .map(str::to_owned)
        }),
    };
    found.filter(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric()))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn finds_video_ids() {
        let id = |s: &str, p| video_id(&Url::parse(s).unwrap(), p);
        assert_eq!(
            id(
                "https://www.pornhub.com/view_video.php?viewkey=6a9c7dfdccea2&t=1",
                Platform::Pornhub
            )
            .as_deref(),
            Some("6a9c7dfdccea2")
        );
        assert_eq!(
            id("https://www.pornhub.com/embed/ph5a1", Platform::Pornhub).as_deref(),
            Some("ph5a1")
        );
        assert_eq!(
            id(
                "https://www.youporn.com/watch/191017841/some-title/",
                Platform::YouPorn
            )
            .as_deref(),
            Some("191017841")
        );
        assert_eq!(
            id("https://www.redtube.com/191397851", Platform::RedTube).as_deref(),
            Some("191397851")
        );
        assert_eq!(
            id(
                "https://embed.redtube.com/?id=42&bgcolor=000",
                Platform::RedTube
            )
            .as_deref(),
            Some("42")
        );
    }

    #[test]
    fn reads_sources() {
        let home = Url::parse("https://www.redtube.com/").unwrap();
        let mp4 = source_format(
            &json!({"format": "mp4", "quality": "720", "videoUrl": "https://cdn/720P_4000K.mp4?x"}),
            &home,
        )
        .unwrap();
        assert_eq!((mp4.id.as_str(), mp4.height), ("720p", Some(720)));
        let hls = source_format(
            &json!({"format": "hls", "quality": "1080", "videoUrl": "/media/1080/master.m3u8"}),
            &home,
        )
        .unwrap();
        assert_eq!(hls.id, "hls-1080p");
        assert_eq!(hls.url, "https://www.redtube.com/media/1080/master.m3u8");
    }
}
