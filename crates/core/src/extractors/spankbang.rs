//! SpankBang: the page sets `stream_url_<quality>` variables, or gives a stream
//! key that its API swaps for the same list.

use std::sync::LazyLock;

use regex::Regex;
use reqwest::Url;
use reqwest::header::REFERER;
use serde_json::Value;

use super::{meta, with_referer};
use crate::download::hls;
use crate::error::fail;
use crate::{Client, Format, Media, MediaKind, Platform, Result};

const HOME: &str = "https://spankbang.com/";

static STREAM: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"stream_url_([^\s=]+)\s*=\s*["']([^"']+)["']"#).unwrap());
static STREAM_KEY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"data-streamkey\s*=\s*["']([^"']+)["']"#).unwrap());
static TITLE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"<h1[^>]+\btitle=["']([^"']+)["']"#).unwrap());
static HEIGHT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(\d+)p$").unwrap());

pub(super) async fn extract(client: &Client, url: &Url) -> Result<Vec<Media>> {
    let id = video_id(url).ok_or_else(|| {
        fail(
            Platform::SpankBang,
            "couldn't find the video ID in this link",
        )
    })?;
    let home = Url::parse(HOME).expect("valid URL");
    client.set_cookie("country=US; Domain=spankbang.com; Path=/", &home);

    let page_url = format!("https://spankbang.com/{id}/video/");
    let page = client.page(&page_url).await?;
    let html = &page.html;
    if html.contains("<title>Just a moment") || html.contains("challenge-platform") {
        return Err(fail(
            Platform::SpankBang,
            "SpankBang's Cloudflare check blocked the request; try again later or from another network",
        ));
    }
    if html.contains("video_removed") {
        return Err(fail(Platform::SpankBang, "this video was removed"));
    }

    let mut streams: Vec<(String, String)> = STREAM
        .captures_iter(html)
        .map(|c| (c[1].to_owned(), c[2].to_owned()))
        .collect();
    if streams.is_empty()
        && let Some(key) = STREAM_KEY.captures(html).map(|c| c[1].to_owned())
    {
        let list: Value = client
            .http
            .post("https://spankbang.com/api/videos/stream")
            .header(REFERER, &page_url)
            .header("X-Requested-With", "XMLHttpRequest")
            .form(&[("id", key.as_str()), ("data", "0")])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        for (name, value) in list.as_object().into_iter().flatten() {
            let link = value.as_str().or_else(|| value.get(0)?.as_str());
            if let Some(link) = link {
                streams.push((name.clone(), link.to_owned()));
            }
        }
    }

    let headers = vec![("Referer".to_owned(), page_url.clone())];
    let mut formats = Vec::new();
    for (name, link) in streams {
        if !link.starts_with("http") {
            continue;
        }
        if name.starts_with("m3u8") || link.contains(".m3u8") {
            if let Ok(found) = hls::formats(client, &link, &headers).await {
                formats.extend(found);
            }
        } else if let Some(height) = HEIGHT.captures(&name).and_then(|c| c[1].parse().ok()) {
            formats.push(Format {
                height: Some(height),
                ..Format::mp4(name, link)
            });
        }
    }
    if formats.is_empty() {
        return Err(fail(Platform::SpankBang, "no video found on the page"));
    }

    Ok(vec![Media {
        platform: Platform::SpankBang,
        kind: MediaKind::Video,
        id,
        title: TITLE
            .captures(html)
            .map(|c| c[1].to_owned())
            .or_else(|| meta(html, "og:title")),
        author: None,
        thumbnail: meta(html, "og:image"),
        duration: None,
        formats: with_referer(formats, &page_url),
    }])
}

/// `/<id>/video/<title>`, `/<id>/play/…` or `/<id>/embed/`.
fn video_id(url: &Url) -> Option<String> {
    let mut segments = url.path_segments()?.filter(|s| !s.is_empty());
    let id = segments.next()?;
    let kind = segments.next()?;
    let known = ["video", "play", "embed"].contains(&kind);
    (known && id.bytes().all(|b| b.is_ascii_alphanumeric())).then(|| id.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_video_ids() {
        let id = |s: &str| video_id(&Url::parse(s).unwrap());
        assert_eq!(
            id("https://spankbang.com/56b3d/video/a+title").as_deref(),
            Some("56b3d")
        );
        assert_eq!(
            id("https://spankbang.com/56b3d/embed/").as_deref(),
            Some("56b3d")
        );
        assert_eq!(id("https://spankbang.com/s/abc/"), None);
    }

    #[test]
    fn reads_stream_variables() {
        let html = r#"var stream_url_480p = 'https://cdn/480.mp4'; var stream_url_m3u8 = "https://cdn/m.m3u8";"#;
        let found: Vec<(String, String)> = STREAM
            .captures_iter(html)
            .map(|c| (c[1].to_owned(), c[2].to_owned()))
            .collect();
        assert_eq!(found[0], ("480p".into(), "https://cdn/480.mp4".into()));
        assert_eq!(found[1].0, "m3u8");
    }
}
