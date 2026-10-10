//! XVideos and XNXX, which share a player that's set up in the page with calls
//! like `html5player.setVideoUrlHigh('…')`.

use std::sync::LazyLock;

use regex::Regex;
use reqwest::Url;

use super::{meta, unescape_html, with_referer};
use crate::download::hls;
use crate::error::fail;
use crate::{Client, Format, Media, MediaKind, Platform, Result};

static SOURCE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"setVideo(UrlLow|UrlHigh|HLS)\(\s*["']([^"']+)["']"#).unwrap());
static SETTING: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"set(VideoTitle|ThumbUrl169|ThumbUrl)\(\s*["']([^"']+)["']"#).unwrap()
});
static ERROR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?s)<h1 class="inlineError">(.+?)</h1>"#).unwrap());

pub(super) async fn extract(client: &Client, url: &Url, platform: Platform) -> Result<Vec<Media>> {
    let id =
        video_id(url).ok_or_else(|| fail(platform, "couldn't find the video ID in this link"))?;
    let page = client.page(url.as_str()).await?;
    let html = &page.html;
    if let Some(c) = ERROR.captures(html) {
        let message = unescape_html(c[1].trim());
        return Err(fail(platform, message));
    }

    let referer = format!(
        "{}://{}/",
        page.url.scheme(),
        page.url.host_str().unwrap_or_default()
    );
    let headers = vec![("Referer".to_owned(), referer.clone())];
    let sources: Vec<(String, String)> = SOURCE
        .captures_iter(html)
        .map(|c| (c[1].to_owned(), c[2].to_owned()))
        .collect();
    let mut formats = Vec::new();
    for (kind, link) in sources {
        let link = page.url.join(&link).map(String::from).unwrap_or(link);
        match kind.as_str() {
            "UrlLow" => formats.insert(0, Format::mp4("low", link)),
            "UrlHigh" => formats.push(Format::mp4("high", link)),
            // HLS has the sharpest qualities; the MP4s are a fallback if it fails.
            _ => {
                if let Ok(qualities) = hls::formats(client, &link, &headers).await {
                    formats.extend(qualities);
                }
            }
        }
    }
    if formats.is_empty() {
        return Err(fail(platform, "no video found on the page"));
    }

    let setting = |name: &str| {
        SETTING
            .captures_iter(html)
            .find(|c| &c[1] == name)
            .map(|c| unescape_html(&c[2]))
    };
    Ok(vec![Media {
        platform,
        kind: MediaKind::Video,
        id,
        title: meta(html, "og:title").or_else(|| setting("VideoTitle")),
        author: None,
        thumbnail: setting("ThumbUrl169").or_else(|| setting("ThumbUrl")),
        duration: meta(html, "og:duration").and_then(|d| d.parse().ok()),
        formats: with_referer(formats, &referer),
    }])
}

/// XVideos: `/video.<id>/<title>` (or older `/video<id>/`); XNXX: `/video-<id>/<title>`.
fn video_id(url: &Url) -> Option<String> {
    let segment = url.path_segments()?.find(|s| s.starts_with("video"))?;
    let id = segment
        .trim_start_matches("video")
        .trim_start_matches(['.', '-']);
    Some(id.to_owned()).filter(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_video_ids() {
        let id = |s: &str| video_id(&Url::parse(s).unwrap());
        assert_eq!(
            id("https://www.xvideos.com/video.ucuvbkfda4e/a_title").as_deref(),
            Some("ucuvbkfda4e")
        );
        assert_eq!(
            id("https://www.xvideos.com/video4588838/biker").as_deref(),
            Some("4588838")
        );
        assert_eq!(
            id("https://www.xnxx.com/video-55awb78/skyrim").as_deref(),
            Some("55awb78")
        );
        assert_eq!(id("https://www.xnxx.com/search/x"), None);
    }

    #[test]
    fn reads_player_setup() {
        let html = r#"html5player.setVideoTitle('A title');
            html5player.setVideoUrlHigh('https://cdn/high.mp4?x');
            html5player.setVideoUrlLow('https://cdn/low.mp4?x');"#;
        let found: Vec<(String, String)> = SOURCE
            .captures_iter(html)
            .map(|c| (c[1].to_owned(), c[2].to_owned()))
            .collect();
        assert_eq!(
            found[0],
            ("UrlHigh".into(), "https://cdn/high.mp4?x".into())
        );
        assert_eq!(found[1].0, "UrlLow");
    }
}
