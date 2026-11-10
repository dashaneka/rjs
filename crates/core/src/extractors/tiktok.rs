//! Reads the data TikTok embeds in its web pages, and the public video list its
//! creator pages use for walking through a profile.

use std::collections::HashSet;
use std::sync::LazyLock;
use std::time::{SystemTime, UNIX_EPOCH};

use regex::Regex;
use reqwest::Url;
use reqwest::header::REFERER;
use serde_json::Value;

use super::{dimension, number, text, title_from_caption};
use crate::error::fail;
use crate::{Client, Format, Media, MediaKind, Platform, Profile, Result};

const LIST_API: &str = "https://www.tiktok.com/api/creator/item_list/";
/// Any cursor older than TikTok itself means we've gone past the account's first post.
const TIKTOK_LAUNCH_MS: u64 = 1_472_706_000_000;
const WEEK_MS: u64 = 7 * 86_400_000;

static PAGE_DATA: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?s)<script[^>]+id="__UNIVERSAL_DATA_FOR_REHYDRATION__"[^>]*>(.*?)</script>"#)
        .unwrap()
});

pub(super) async fn extract(client: &Client, url: &Url) -> Result<Vec<Media>> {
    let scope = page_data(client, url).await?;
    let items = item_media(video_item(&scope)?);
    if items.is_empty() {
        return Err(fail(
            Platform::TikTok,
            "the post has no supported media URLs",
        ));
    }
    Ok(items)
}

/// Profile links look like `tiktok.com/@name`, without a video after it.
pub(super) fn is_profile(url: &Url) -> bool {
    let mut segments = url
        .path_segments()
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty());
    segments
        .next()
        .is_some_and(|s| s.starts_with('@') && s.len() > 1)
        && segments.next().is_none()
}

pub(super) async fn profile(client: &Client, url: &Url) -> Result<Profile> {
    let scope = page_data(client, url).await?;
    let detail = scope
        .get("webapp.user-detail")
        .ok_or_else(|| fail(Platform::TikTok, "this link isn't a TikTok profile"))?;
    check_status(detail, "profile")?;

    let user = &detail["userInfo"]["user"];
    let username = text(user, "/uniqueId")
        .ok_or_else(|| fail(Platform::TikTok, "couldn't read the profile"))?;
    let sec_uid =
        text(user, "/secUid").ok_or_else(|| fail(Platform::TikTok, "couldn't read the profile"))?;

    Ok(Profile {
        platform: Platform::TikTok,
        name: text(user, "/nickname"),
        bio: text(user, "/signature"),
        post_count: number(detail, "/userInfo/stats/videoCount"),
        follower_count: number(detail, "/userInfo/stats/followerCount"),
        private: user["privateAccount"] == true,
        avatar: avatar_media(user),
        pager: super::Pager::TikTok(Pager::new(sec_uid, username.clone())),
        username,
    })
}

/// The profile picture of the profile, or of the creator of the video, behind `url`.
pub(super) async fn avatar(client: &Client, url: &Url) -> Result<Media> {
    let scope = page_data(client, url).await?;
    let user = match scope.pointer("/webapp.user-detail/userInfo/user") {
        Some(user) => user,
        None => &video_item(&scope)?["author"],
    };
    avatar_media(user).ok_or_else(|| fail(Platform::TikTok, "couldn't find the profile picture"))
}

/// Walks through a profile's posts, newest first.
pub(crate) struct Pager {
    sec_uid: String,
    username: String,
    device_id: String,
    cursor: u64,
    seen: HashSet<String>,
    done: bool,
}

impl Pager {
    fn new(sec_uid: String, username: String) -> Self {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        Self {
            sec_uid,
            username,
            device_id: (7_250_000_000_000_000_000 + u64::from(now.subsec_nanos()) * 1_000_003)
                .to_string(),
            cursor: now.as_millis() as u64,
            seen: HashSet::new(),
            done: false,
        }
    }

    /// The next batch of posts, possibly empty; `None` once there are no more.
    pub(crate) async fn next(&mut self, client: &Client) -> Result<Option<Vec<Media>>> {
        if self.done {
            return Ok(None);
        }

        let cursor = self.cursor.to_string();
        let verify = format!("verify_{:07x}", self.cursor % 0x1000_0000);
        let query = [
            ("aid", "1988"),
            ("app_language", "en"),
            ("app_name", "tiktok_web"),
            ("browser_language", "en-US"),
            ("browser_name", "Mozilla"),
            ("browser_online", "true"),
            ("browser_platform", "Win32"),
            ("browser_version", "5.0 (Windows)"),
            ("channel", "tiktok_web"),
            ("cookie_enabled", "true"),
            ("count", "15"),
            ("cursor", &cursor),
            ("device_id", &self.device_id),
            ("device_platform", "web_pc"),
            ("focus_state", "true"),
            ("from_page", "user"),
            ("history_len", "2"),
            ("is_fullscreen", "false"),
            ("is_page_visible", "true"),
            ("language", "en"),
            ("os", "windows"),
            ("priority_region", ""),
            ("referer", ""),
            ("region", "US"),
            ("screen_height", "1080"),
            ("screen_width", "1920"),
            ("secUid", &self.sec_uid),
            // 1 = newest to oldest, starting before `cursor`.
            ("type", "1"),
            ("tz_name", "UTC"),
            ("verifyFp", &verify),
            ("webcast_language", "en"),
        ];
        let body = client
            .http
            .get(LIST_API)
            .query(&query)
            .header(
                REFERER,
                format!("https://www.tiktok.com/@{}", self.username),
            )
            .send()
            .await?
            .text()
            .await?;
        let data: Value = serde_json::from_str(&body).map_err(|_| {
            fail(
                Platform::TikTok,
                "TikTok didn't send the list of posts (it may be limiting requests); try again later",
            )
        })?;

        let items = data["itemList"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default();
        let mut media = Vec::new();
        for item in items {
            if let Some(id) = text(item, "/id")
                && self.seen.insert(id)
            {
                media.extend(item_media(item));
            }
        }

        // Continue from the oldest post seen. A batch can come back empty when the
        // account posted nothing in that stretch, so then step back a week.
        let oldest = items
            .last()
            .and_then(|i| number(i, "/createTime"))
            .map(|s| s * 1000);
        self.cursor = match oldest {
            Some(ms) if ms < self.cursor => ms,
            _ => self.cursor.saturating_sub(WEEK_MS),
        };
        if data["hasMorePrevious"] != true || self.cursor < TIKTOK_LAUNCH_MS {
            self.done = true;
        }
        Ok(Some(media))
    }
}

/// Loads a TikTok page and returns the data it embeds for the app.
async fn page_data(client: &Client, url: &Url) -> Result<Value> {
    // Short links (vm.tiktok.com, /t/...) redirect to the full page.
    let clean = canonical_post_url(url);
    let mut page = client.page(clean.as_ref().unwrap_or(url).as_str()).await?;
    if page.url.path() == "/login" {
        return Err(fail(
            Platform::TikTok,
            "TikTok requires login for this post",
        ));
    }
    if !PAGE_DATA.is_match(&page.html) && page.html.contains(r#"id="cs""#) {
        let answer = challenge::solve(&page.html).ok_or_else(|| {
            fail(
                Platform::TikTok,
                "TikTok showed a check page rjs couldn't get past; try again later or from another network",
            )
        })?;
        let site = Url::parse("https://www.tiktok.com/").expect("valid URL");
        for (name, value) in &answer {
            client.set_cookie(
                &format!("{name}={value}; Domain=tiktok.com; Path=/; Max-Age=120"),
                &site,
            );
        }
        page = client.page(page.url.as_str()).await?;
        // The page's own script lets these expire right after the reload.
        for (name, _) in &answer {
            client.set_cookie(
                &format!("{name}=; Domain=tiktok.com; Path=/; Max-Age=0"),
                &site,
            );
        }
    }
    scope_from_html(&page.html, &page.url)
        .ok_or_else(|| fail(Platform::TikTok, "couldn't find any data on the page"))
}

fn canonical_post_url(url: &Url) -> Option<Url> {
    let segments: Vec<_> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    let id = match segments.as_slice() {
        [user, "video" | "photo", id] if user.starts_with('@') => *id,
        ["embed", id] | ["share", "video", id] => *id,
        _ => return None,
    };
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut canonical = url.clone();
    canonical.set_query(None);
    canonical.set_fragment(None);
    Some(canonical)
}

fn scope_from_html(html: &str, url: &Url) -> Option<Value> {
    if let Some(scope) = PAGE_DATA
        .captures(html)
        .and_then(|c| serde_json::from_str::<Value>(&c[1]).ok())
        .and_then(|mut v| v.get_mut("__DEFAULT_SCOPE__").map(Value::take))
    {
        return Some(scope);
    }
    static SIGI: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r#"(?s)<script[^>]+id=["'](?:SIGI_STATE|sigi-persisted-data)["'][^>]*>(.*?)</script>"#,
        )
        .unwrap()
    });
    let canonical = canonical_post_url(url)?;
    let id = canonical.path_segments()?.rfind(|s| !s.is_empty())?;
    let data = SIGI
        .captures(html)
        .and_then(|c| serde_json::from_str::<Value>(&c[1]).ok())?;
    let item = data.get("ItemModule")?.get(id)?;
    Some(
        serde_json::json!({"webapp.video-detail": {"statusCode": 0, "itemInfo": {"itemStruct": item}}}),
    )
}

/// Before showing a page, TikTok sometimes sends a check page instead: a small
/// puzzle that browsers solve in JavaScript and answer with a cookie.
mod challenge {
    use base64::Engine;
    use base64::engine::DecodePaddingMode;
    use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig, STANDARD};
    use regex::Regex;
    use serde_json::Value;
    use sha2::{Digest, Sha256};

    const MAX_TRIES: u32 = 1_000_000;

    /// The cookies (name, value) that answer the check on `html`.
    pub(super) fn solve(html: &str) -> Option<Vec<(String, String)>> {
        let lenient = GeneralPurpose::new(
            &base64::alphabet::STANDARD,
            GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
        );
        let mut data: Value =
            serde_json::from_slice(&lenient.decode(class_of(html, "cs")?).ok()?).ok()?;
        let base = STANDARD.decode(data.pointer("/v/a")?.as_str()?).ok()?;
        let expected = STANDARD.decode(data.pointer("/v/c")?.as_str()?).ok()?;

        // Find the number that, appended to `a`, hashes to `c`.
        let hasher = Sha256::new_with_prefix(&base);
        let number = (0..=MAX_TRIES).map(|n| n.to_string()).find(|n| {
            hasher
                .clone()
                .chain_update(n.as_bytes())
                .finalize()
                .as_slice()
                == expected
        })?;
        data["d"] = STANDARD.encode(number).into();

        let mut cookies = vec![(
            class_of(html, "wci")?.to_owned(),
            STANDARD.encode(data.to_string()),
        )];
        if let (Some(name), Some(value)) = (class_of(html, "rci"), class_of(html, "rs"))
            && !name.is_empty()
            && !value.is_empty()
        {
            cookies.push((name.to_owned(), value.to_owned()));
        }
        Some(cookies)
    }

    /// The check page keeps its values in the `class` of elements with known ids.
    fn class_of<'a>(html: &'a str, id: &str) -> Option<&'a str> {
        let tag = Regex::new(&format!(r#"<[^>]*\bid="{id}"[^>]*>"#)).ok()?;
        let class = Regex::new(r#"\bclass="([^"]*)""#).ok()?;
        let found = tag.find(html)?.as_str();
        Some(class.captures(found)?.get(1)?.as_str())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn answers_check_page() {
            let base = b"some puzzle";
            let expected = Sha256::digest([base.as_slice(), b"4321"].concat());
            let data = serde_json::json!({
                "v": {"a": STANDARD.encode(base), "b": 1, "c": STANDARD.encode(expected)},
                "s": "x"
            });
            let encoded = STANDARD.encode(data.to_string());
            let html = format!(
                r#"<p id="wci" class="_wafchallengeid"></p><p id="cs" class="{}"></p><p id="rci" class="waforiginalreid"></p><p id="rs" class=""></p>"#,
                encoded.trim_end_matches('=')
            );

            let cookies = solve(&html).unwrap();
            assert_eq!(cookies.len(), 1);
            assert_eq!(cookies[0].0, "_wafchallengeid");
            let answer: Value =
                serde_json::from_slice(&STANDARD.decode(&cookies[0].1).unwrap()).unwrap();
            assert_eq!(answer["d"], STANDARD.encode("4321"));
            assert_eq!(answer["s"], "x");
            assert!(answer.to_string().starts_with(r#"{"v":"#));
        }
    }
}

fn video_item(scope: &Value) -> Result<&Value> {
    let detail = scope
        .get("webapp.video-detail")
        .ok_or_else(|| fail(Platform::TikTok, "this link isn't a TikTok post"))?;
    check_status(detail, "post")?;
    if detail.pointer("/itemInfo/itemStruct/isContentClassified") == Some(&Value::Bool(true))
        && detail.pointer("/itemInfo/itemStruct/video").is_none()
    {
        return Err(fail(
            Platform::TikTok,
            "TikTok requires authorized login for this post",
        ));
    }
    detail
        .pointer("/itemInfo/itemStruct")
        .ok_or_else(|| fail(Platform::TikTok, "couldn't find the post on the page"))
}

fn check_status(detail: &Value, what: &str) -> Result<()> {
    match detail["statusCode"].as_i64().unwrap_or(0) {
        0 => Ok(()),
        10216 | 10222 => Err(fail(
            Platform::TikTok,
            "this post or account is private; authorized login is required",
        )),
        10204 => Err(fail(
            Platform::TikTok,
            "TikTok blocked this network from accessing the post",
        )),
        status => {
            let reason = text(detail, "/statusMsg").unwrap_or_else(|| format!("status {status}"));
            Err(fail(
                Platform::TikTok,
                format!("{what} unavailable ({reason})"),
            ))
        }
    }
}

/// A post is either one video or a slideshow of pictures.
fn item_media(item: &Value) -> Vec<Media> {
    let Some(id) = text(item, "/id") else {
        return Vec::new();
    };
    let base = Media {
        platform: Platform::TikTok,
        kind: MediaKind::Video,
        id: id.clone(),
        title: title_from_caption(text(item, "/desc")),
        author: text(item, "/author/uniqueId"),
        thumbnail: text(item, "/video/cover"),
        duration: number(item, "/video/duration").map(|s| s as f64),
        formats: Vec::new(),
    };

    if let Some(images) = item.pointer("/imagePost/images").and_then(Value::as_array) {
        let many = images.len() > 1;
        return images
            .iter()
            .enumerate()
            .filter_map(|(i, image)| {
                let format = Format {
                    width: dimension(image, "/imageWidth"),
                    height: dimension(image, "/imageHeight"),
                    ..Format::image("image", text(image, "/imageURL/urlList/0")?)
                };
                Some(Media {
                    kind: MediaKind::Image,
                    id: if many {
                        format!("{id}_{}", i + 1)
                    } else {
                        id.clone()
                    },
                    duration: None,
                    formats: with_referer(vec![format]),
                    ..base.clone()
                })
            })
            .collect();
    }

    let formats = video_formats(&item["video"]);
    if formats.is_empty() {
        return Vec::new();
    }
    vec![Media {
        formats: with_referer(formats),
        ..base
    }]
}

// These public web-data variants are also handled by yt-dlp's TikTok extractor.
// Keep this implementation independent: no Python process or external service.
fn media_urls(value: &Value) -> Vec<String> {
    fn collect(value: &Value, urls: &mut Vec<String>) {
        match value {
            Value::String(raw) => {
                let normalized = if raw.starts_with("//") {
                    format!("https:{raw}")
                } else {
                    raw.clone()
                };
                if let Ok(url) = Url::parse(&normalized)
                    && matches!(url.scheme(), "https" | "http")
                    && url.host_str().is_some()
                    && !urls.contains(&normalized)
                {
                    urls.push(normalized);
                }
            }
            Value::Array(values) => {
                for v in values {
                    collect(v, urls);
                }
            }
            Value::Object(map) => {
                for key in ["src", "url", "UrlList", "urlList"] {
                    if let Some(v) = map.get(key) {
                        collect(v, urls);
                    }
                }
            }
            _ => {}
        }
    }
    let mut urls = Vec::new();
    collect(value, &mut urls);
    urls
}

fn format_metadata(
    quality: &Value,
    video: &Value,
) -> (Option<String>, Option<u32>, Option<u32>, Option<u64>) {
    static KEY: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"v[^_]+_([^_]+)_(\d+)p_(\d+)").unwrap());
    let key = text(quality, "/PlayAddr/UrlKey");
    let captures = key.as_deref().and_then(|k| KEY.captures(k));
    let codec = captures
        .as_ref()
        .map(|c| c[1].to_owned())
        .or_else(|| text(quality, "/CodecType"));
    let codec = codec.map(|c| match c.as_str() {
        "bytevc1" | "hevc" => "h265".into(),
        _ => c,
    });
    let bitrate =
        number(quality, "/Bitrate").or_else(|| captures.as_ref().and_then(|c| c[3].parse().ok()));
    let mut width = dimension(quality, "/PlayAddr/Width");
    let mut height = dimension(quality, "/PlayAddr/Height");
    if (width.is_none() || height.is_none())
        && let Some(c) = captures
    {
        let resolution = c[2]
            .parse::<u32>()
            .ok()
            .map(|n| if n == 540 { 576 } else { n });
        if let (Some(res), Some(w), Some(h)) = (
            resolution,
            dimension(video, "/width"),
            dimension(video, "/height"),
        ) && w > 0
            && h > 0
        {
            if w < h {
                width = Some(res);
                height = Some(((u64::from(res) * u64::from(h) / u64::from(w)) as u32) & !1);
            } else {
                height = Some(res);
                width = Some((((u64::from(res) * u64::from(w) / u64::from(h)) as u32) + 1) & !1);
            }
        }
    }
    (codec, width, height, bitrate)
}

fn is_video_only(url: &str) -> bool {
    Url::parse(url).is_ok_and(|u| {
        u.path()
            .trim_end_matches('/')
            .ends_with("/media-video-hvc1")
    })
}

fn video_formats(video: &Value) -> Vec<Format> {
    let mut formats = Vec::new();
    // Watermarked download URLs are a last resort, below normal playback formats.
    for (i, url) in media_urls(&video["downloadAddr"])
        .into_iter()
        .chain(media_urls(&video["download"]["url"]))
        .enumerate()
    {
        formats.push(Format::mp4(format!("download-watermarked-{i}"), url));
    }
    for (i, url) in media_urls(&video["playAddr"]).into_iter().enumerate() {
        if text(video, "/codecType").is_some_and(|codec| codec.eq_ignore_ascii_case("bytevc2"))
            || is_video_only(&url)
        {
            continue;
        }
        formats.push(Format {
            width: dimension(video, "/width"),
            height: dimension(video, "/height"),
            bitrate: number(video, "/bitrate"),
            codec: text(video, "/codecType"),
            ..Format::mp4(
                if i == 0 {
                    "play".into()
                } else {
                    format!("play-{i}")
                },
                url,
            )
        });
    }
    for (i, quality) in video["bitrateInfo"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
    {
        let (codec, width, height, bitrate) = format_metadata(quality, video);
        // bytevc2 is a proprietary codec normal media players cannot decode.
        if codec
            .as_deref()
            .is_some_and(|c| c.eq_ignore_ascii_case("bytevc2"))
            || text(quality, "/CodecType").is_some_and(|c| c.eq_ignore_ascii_case("bytevc2"))
        {
            continue;
        }
        let urls = media_urls(&quality["PlayAddr"]);
        let Some(primary) = urls.first() else {
            continue;
        };
        let id = text(quality, "/GearName").unwrap_or_else(|| format!("play-{i}"));
        let base = Format {
            width,
            height,
            bitrate,
            codec,
            size: number(quality, "/PlayAddr/DataSize"),
            ..Format::mp4(id.clone(), primary.clone())
        };
        // Prefer the first advertised media URL; retain every other URL as a mirror.
        // Later equal formats win in rjs, so insert the preferred URL last.
        for (index, url) in urls
            .iter()
            .enumerate()
            .skip(1)
            .rev()
            .chain(std::iter::once((0, primary)))
        {
            let has_audio = !is_video_only(url);
            if !has_audio {
                continue;
            }
            formats.push(Format {
                id: if index == 0 {
                    id.clone()
                } else {
                    format!("{id}-mirror-{index}")
                },
                url: url.clone(),
                has_audio,
                ..base.clone()
            });
        }
    }
    let mut seen = HashSet::new();
    formats.reverse();
    formats.retain(|f| seen.insert(f.url.clone()));
    formats.reverse();
    formats
}

fn avatar_media(user: &Value) -> Option<Media> {
    let url = ["/avatarLarger", "/avatarMedium", "/avatarThumb"]
        .iter()
        .find_map(|p| text(user, p))?;
    Some(Media {
        platform: Platform::TikTok,
        kind: MediaKind::Image,
        id: "avatar".into(),
        title: text(user, "/nickname"),
        author: text(user, "/uniqueId"),
        thumbnail: None,
        duration: None,
        formats: with_referer(vec![Format::image("avatar", url)]),
    })
}

/// TikTok's file servers reject requests that don't come from tiktok.com, and tie
/// video links to the cookies set by the page (the shared client keeps those).
fn with_referer(mut formats: Vec<Format>) -> Vec<Format> {
    for f in &mut formats {
        f.headers
            .push(("Referer".into(), "https://www.tiktok.com/".into()));
    }
    formats
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn reads_web_address_shapes_and_deduplicates() {
        let urls = media_urls(&json!([
            {"src": "//cdn.example/one"}, {"url": "https://cdn.example/two"},
            {"UrlList": ["https://cdn.example/one", "javascript:alert(1)"]}
        ]));
        assert_eq!(urls, ["https://cdn.example/one", "https://cdn.example/two"]);
        let formats = video_formats(&json!({"width": 720, "height": 1280,
            "playAddr": [{"src": "https://cdn.example/play"}],
            "download": {"url": "https://cdn.example/watermarked"}}));
        assert_eq!(formats.len(), 2);
        assert_eq!(formats.last().unwrap().id, "play");
    }

    #[test]
    fn infers_portrait_resolution_and_skips_unsupported_or_split_streams() {
        let formats = video_formats(&json!({"width": 576, "height": 1024,
        "bitrateInfo": [
            {"PlayAddr": {"UrlKey": "v123_h264_720p_1200000", "UrlList": ["https://cdn.example/hd"]}},
            {"PlayAddr": {"UrlKey": "v123_bytevc2_1080p_2000000", "UrlList": ["https://cdn.example/vvc"]}},
            {"PlayAddr": {"UrlKey": "v123_bytevc1_1080p_2000000", "UrlList": ["https://cdn.example/media-video-hvc1/"]}}
        ]}));
        assert_eq!(formats.len(), 1);
        let f = &formats[0];
        assert_eq!(
            (f.width, f.height, f.bitrate),
            (Some(720), Some(1280), Some(1200000))
        );
        assert_eq!(f.codec.as_deref(), Some("h264"));
    }

    #[test]
    fn excludes_unsupported_and_video_only_default_urls() {
        let formats = video_formats(&json!({
            "codecType": "ByteVC2",
            "playAddr": "https://cdn.example/unsupported",
            "bitrateInfo": [{"CodecType": "bytevc2", "PlayAddr": {
                "UrlKey": "v123_h264_720p_1200000",
                "UrlList": ["https://cdn.example/conflicting"]
            }}]
        }));
        assert!(formats.is_empty());
        let formats = video_formats(&json!({
            "playAddr": "https://cdn.example/media-video-hvc1?token=1"
        }));
        assert!(formats.is_empty());
    }

    #[test]
    fn reads_legacy_state_only_for_requested_post() {
        let url = Url::parse("https://www.tiktok.com/@someone/video/123?_t=tracking#x").unwrap();
        assert_eq!(
            canonical_post_url(&url).unwrap().as_str(),
            "https://www.tiktok.com/@someone/video/123"
        );
        let html = r#"<script id='SIGI_STATE'>{"ItemModule":{"123":{"id":"123","video":{"playAddr":"https://cdn.example/v"}},"456":{"id":"456"}}}</script>"#;
        let scope = scope_from_html(html, &url).unwrap();
        assert_eq!(video_item(&scope).unwrap()["id"], "123");
        assert!(
            scope_from_html(
                html,
                &Url::parse("https://www.tiktok.com/@someone/video/999").unwrap()
            )
            .is_none()
        );
    }

    #[test]
    fn reports_restricted_posts_without_inventing_formats() {
        for status in [10204, 10216, 10222] {
            assert!(check_status(&json!({"statusCode": status}), "post").is_err());
        }
    }

    #[test]
    fn recognises_profile_links() {
        let profile = |s: &str| is_profile(&Url::parse(s).unwrap());
        assert!(profile("https://www.tiktok.com/@scout2015"));
        assert!(profile("https://www.tiktok.com/@scout2015/?lang=en"));
        assert!(!profile(
            "https://www.tiktok.com/@scout2015/video/6718335390845095173"
        ));
        assert!(!profile("https://vm.tiktok.com/ZMabc/"));
        assert!(!profile("https://www.tiktok.com/@"));
    }

    #[test]
    fn reads_video_post() {
        let item = json!({
            "id": "1", "desc": "hello", "author": { "uniqueId": "someone" },
            "video": {
                "playAddr": "https://v/play", "width": 576, "height": 1024, "duration": 10,
                "bitrateInfo": [{
                    "GearName": "normal_720_0", "Bitrate": 1000000, "CodecType": "h264",
                    "PlayAddr": { "Width": 720, "Height": 1280, "DataSize": "2048", "UrlList": [
                        "https://v16-webapp-prime.tiktok.com/video/720",
                        "https://www.tiktok.com/aweme/v1/play/?id=720"
                    ] }
                }]
            }
        });
        let media = item_media(&item);
        assert_eq!(media.len(), 1);
        let f = &media[0].formats;
        assert_eq!(f.len(), 3);
        assert_eq!(f[0].id, "play");
        assert_eq!(
            (f[2].id.as_str(), f[2].height, f[2].size),
            ("normal_720_0", Some(1280), Some(2048))
        );
        assert_eq!(f[2].url, "https://v16-webapp-prime.tiktok.com/video/720");
        assert_eq!(f[1].url, "https://www.tiktok.com/aweme/v1/play/?id=720");
        assert_ne!(f[1].id, f[2].id);
        assert!(
            f.iter()
                .all(|f| f.headers.iter().any(|(k, _)| k == "Referer"))
        );
    }

    #[test]
    fn reads_slideshow_pictures() {
        let item = json!({
            "id": "9", "author": { "uniqueId": "someone" }, "video": {},
            "imagePost": { "images": [
                { "imageWidth": 1080, "imageHeight": 1440, "imageURL": { "urlList": ["https://p/1.jpeg?x"] } },
                { "imageURL": { "urlList": ["https://p/2.webp?x"] } },
            ]}
        });
        let media = item_media(&item);
        assert_eq!(media.len(), 2);
        assert_eq!(
            (media[0].id.as_str(), media[0].kind),
            ("9_1", MediaKind::Image)
        );
        assert_eq!(media[0].formats[0].ext, "jpeg");
        assert_eq!(media[1].formats[0].ext, "webp");
    }

    #[test]
    fn reads_avatar() {
        let user =
            json!({ "uniqueId": "someone", "avatarMedium": "https://p/a~c5_720x720.jpeg?x" });
        let avatar = avatar_media(&user).unwrap();
        assert_eq!(
            avatar.file_name(&avatar.formats[0].ext),
            "someone_avatar.jpeg"
        );
    }
}
