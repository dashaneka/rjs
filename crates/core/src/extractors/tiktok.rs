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
    Ok(item_media(video_item(&scope)?))
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
    let mut page = client.page(url.as_str()).await?;
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
    PAGE_DATA
        .captures(&page.html)
        .and_then(|c| serde_json::from_str::<Value>(&c[1]).ok())
        .and_then(|mut data| data.get_mut("__DEFAULT_SCOPE__").map(Value::take))
        .ok_or_else(|| fail(Platform::TikTok, "couldn't find any data on the page"))
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
    detail
        .pointer("/itemInfo/itemStruct")
        .ok_or_else(|| fail(Platform::TikTok, "couldn't find the post on the page"))
}

fn check_status(detail: &Value, what: &str) -> Result<()> {
    match detail["statusCode"].as_i64().unwrap_or(0) {
        0 => Ok(()),
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

fn video_formats(video: &Value) -> Vec<Format> {
    let mut formats: Vec<Format> = video["bitrateInfo"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
        .filter_map(|(i, b)| {
            Some(Format {
                width: dimension(b, "/PlayAddr/Width"),
                height: dimension(b, "/PlayAddr/Height"),
                bitrate: number(b, "/Bitrate"),
                size: number(b, "/PlayAddr/DataSize"),
                codec: text(b, "/CodecType"),
                ..Format::mp4(
                    text(b, "/GearName").unwrap_or_else(|| format!("play-{i}")),
                    mirror(&b["PlayAddr"]["UrlList"])?,
                )
            })
        })
        .collect();

    // Listed first so equal qualities from `bitrateInfo`, which have mirrors, win.
    if let Some(url) = text(video, "/playAddr") {
        formats.insert(
            0,
            Format {
                width: dimension(video, "/width"),
                height: dimension(video, "/height"),
                bitrate: number(video, "/bitrate"),
                codec: text(video, "/codecType"),
                ..Format::mp4("play", url)
            },
        );
    }
    formats
}

/// Each quality comes from several mirrors. For posts from the profile list, TikTok
/// refuses the direct file-server links but serves its own `/aweme/v1/play/` link.
fn mirror(urls: &Value) -> Option<String> {
    let urls: Vec<&str> = urls.as_array()?.iter().filter_map(Value::as_str).collect();
    urls.iter()
        .find(|u| u.contains("tiktok.com/aweme/v1/play/"))
        .or(urls.first())
        .map(|u| (*u).to_owned())
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
        assert_eq!(f.len(), 2);
        assert_eq!(f[0].id, "play");
        assert_eq!(
            (f[1].id.as_str(), f[1].height, f[1].size),
            ("normal_720_0", Some(1280), Some(2048))
        );
        assert_eq!(f[1].url, "https://www.tiktok.com/aweme/v1/play/?id=720");
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
