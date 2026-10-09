//! Reads video links from the JSON Facebook embeds in its video and reel pages.

use std::sync::LazyLock;

use regex::Regex;
use reqwest::Url;
use serde_json::Value;

use super::segment_after;
use crate::error::fail;
use crate::{Client, Format, Media, MediaKind, Platform, Result};

/// Keys that mark a video object, in the page layouts Facebook currently uses.
const URL_KEYS: [(&str, &str); 2] = [
    ("browser_native_sd_url", "browser_native_hd_url"),
    ("playable_url", "playable_url_quality_hd"),
];

static OG_TITLE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"<meta property="og:title" content="([^"]*)""#).unwrap());

pub(super) async fn extract(client: &Client, url: &Url) -> Result<Vec<Media>> {
    // Share links (fb.watch, /share/v/...) redirect to the real video page.
    let page = client.page(url.as_str()).await?;
    if page.url.path().starts_with("/login") {
        return Err(fail(
            Platform::Facebook,
            "this video can only be viewed after logging in",
        ));
    }

    let wanted = video_id(&page.url);
    let videos = video_objects(&page.html);
    let video = videos
        .iter()
        .find(|v| wanted.is_some() && v["id"].as_str() == wanted.as_deref())
        .or_else(|| videos.first())
        .ok_or_else(|| {
            fail(
                Platform::Facebook,
                "no video found (it may be private or need a login)",
            )
        })?;

    let mut formats = Vec::new();
    for (sd, hd) in URL_KEYS {
        for (name, key) in [("sd", sd), ("hd", hd)] {
            if let Some(url) = video[key].as_str().filter(|u| !u.is_empty()) {
                formats.push(Format::mp4(name, url));
            }
        }
        if !formats.is_empty() {
            break;
        }
    }

    let (title, author) = OG_TITLE
        .captures(&page.html)
        .map(|c| split_title(&unescape_html(&c[1])))
        .unwrap_or_default();
    Ok(vec![Media {
        platform: Platform::Facebook,
        kind: MediaKind::Video,
        id: video["id"]
            .as_str()
            .map(str::to_owned)
            .or(wanted)
            .unwrap_or_else(|| "video".into()),
        title,
        author,
        thumbnail: None,
        duration: None,
        formats,
    }])
}

fn video_id(url: &Url) -> Option<String> {
    let from_query = url
        .query_pairs()
        .find(|(k, _)| k == "v")
        .map(|(_, v)| v.into_owned());
    from_query
        .or_else(|| segment_after(url, &["videos", "reel"]).map(str::to_owned))
        .filter(|id| id.bytes().all(|b| b.is_ascii_digit()))
}

/// Finds every JSON object in the page that holds video links. The page also embeds
/// other suggested videos, so callers pick the one matching the requested ID.
fn video_objects(html: &str) -> Vec<Value> {
    let mut found: Vec<Value> = Vec::new();
    for (sd_key, _) in URL_KEYS {
        let needle = format!("\"{sd_key}\":");
        for (pos, _) in html.match_indices(&needle) {
            if let Some(obj) = enclosing_object(html, pos, sd_key)
                && !found.iter().any(|f| f["id"] == obj["id"])
            {
                found.push(obj);
            }
        }
    }
    found
}

/// Walks back from `pos` to the `{` that opens the object containing `key`.
fn enclosing_object(html: &str, pos: usize, key: &str) -> Option<Value> {
    let mut end = pos;
    for _ in 0..64 {
        let start = html[..end].rfind('{')?;
        let parsed = serde_json::Deserializer::from_str(&html[start..])
            .into_iter::<Value>()
            .next();
        if let Some(Ok(obj)) = parsed
            && obj.get(key).is_some()
        {
            return Some(obj);
        }
        end = start;
    }
    None
}

/// Page titles look like `2.8M views · 1.2K reactions | <title> | <page name>`.
fn split_title(full: &str) -> (Option<String>, Option<String>) {
    let parts: Vec<&str> = full.split(" | ").map(str::trim).collect();
    let (title, author) = match parts.as_slice() {
        [_stats, middle @ .., page] if !middle.is_empty() => (middle.join(" | "), Some(*page)),
        [only] => (only.to_string(), None),
        [title, page] => (title.to_string(), Some(*page)),
        _ => (String::new(), None),
    };
    let author = author.filter(|a| *a != "Facebook").map(str::to_owned);
    let title = Some(title).filter(|t| !t.is_empty() && t != "Facebook");
    (title, author)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_video_id() {
        let id = |s: &str| video_id(&Url::parse(s).unwrap());
        assert_eq!(
            id("https://www.facebook.com/watch/?v=101").as_deref(),
            Some("101")
        );
        assert_eq!(
            id("https://www.facebook.com/page/videos/202/").as_deref(),
            Some("202")
        );
        assert_eq!(
            id("https://www.facebook.com/reel/303").as_deref(),
            Some("303")
        );
        assert_eq!(id("https://www.facebook.com/page").as_deref(), None);
    }

    #[test]
    fn cleans_page_title() {
        let raw = "9.8K views &#xb7; 342 reactions | Let the &#x201c;Slapathon&#x201d; begin &amp; go | Beast Camp";
        let (title, author) = split_title(&unescape_html(raw));
        assert_eq!(
            title.as_deref(),
            Some("Let the \u{201c}Slapathon\u{201d} begin & go")
        );
        assert_eq!(author.as_deref(), Some("Beast Camp"));

        let (title, author) = split_title("1K views · 5 reactions | How to share | Facebook");
        assert_eq!((title.as_deref(), author), (Some("How to share"), None));
    }

    #[test]
    fn finds_objects_in_page() {
        let html = r#"<script>{"a":{"x":{"y":1},"browser_native_sd_url":"https:\/\/v\/sd1","browser_native_hd_url":"https:\/\/v\/hd1","id":"1"}}</script>
            <script>{"b":[{"browser_native_sd_url":"https:\/\/v\/sd2","browser_native_hd_url":null,"id":"2"}]}</script>"#;
        let objs = video_objects(html);
        assert_eq!(objs.len(), 2);
        assert_eq!(objs[0]["browser_native_hd_url"], "https://v/hd1");
        assert_eq!(objs[1]["id"], "2");
    }
}
