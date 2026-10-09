//! With a logged-in session, uses the media API Instagram's apps use. Without one,
//! reads the post data embedded in the post's page, falling back to the GraphQL
//! query the web app runs, which Instagram often refuses to logged-out visitors.

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use regex::Regex;
use reqwest::{StatusCode, Url};
use serde_json::{Value, json};

use super::{dimension, number, segment_after, text, title_from_caption};
use crate::error::fail;
use crate::{Client, Format, Media, MediaKind, Platform, Result};

const GRAPHQL: &str = "https://www.instagram.com/graphql/query";
const API: &str = "https://i.instagram.com/api/v1";
const APP_ID: &str = "936619743392459";
/// Instagram changes this ID from time to time; update it when posts stop loading.
const POST_QUERY_ID: &str = "8845758582119845";
const POST_QUERY_NAME: &str = "PolarisPostActionLoadPostQueryQuery";
const NEEDS_LOGIN: &str = "Instagram only shows this to logged-in visitors right now; \
     add --cookies-from-browser chrome (or the browser you use Instagram in)";

static LSD_TOKEN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#""LSD",\[\],\{"token":"([^"]+)""#).unwrap());
static PAGE_DATA: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"<script\b[^>]*\bdata-sjs>(\{.+?\})</script>"#).unwrap());
static REPRESENTATION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?s)<Representation\b([^>]*)>(.*?)</Representation>"#).unwrap());
static ATTRIBUTE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"(\w+)="([^"]*)""#).unwrap());
static BASE_URL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<BaseURL>([^<]+)</BaseURL>").unwrap());

pub(super) async fn extract(client: &Client, url: &Url) -> Result<Vec<Media>> {
    let mut url = url.clone();
    if url.path().starts_with("/share/") {
        url = client.page(url.as_str()).await?.url;
    }
    if let Some(which) = stories_in(&url) {
        return stories(client, which).await;
    }
    let code = shortcode(&url).ok_or_else(|| {
        fail(
            Platform::Instagram,
            "couldn't find the post ID in this link",
        )
    })?;

    if is_logged_in(client) {
        post_from_api(client, code).await
    } else {
        logged_out(client, code).await
    }
}

fn is_logged_in(client: &Client) -> bool {
    let site = Url::parse("https://www.instagram.com/").expect("valid URL");
    client.cookie(&site, "sessionid").is_some()
}

/// Calls the API Instagram's apps use; only works with a logged-in session.
async fn api(client: &Client, path: &str, query: &[(&str, &str)]) -> Result<Value> {
    let site = Url::parse("https://www.instagram.com/").expect("valid URL");
    let resp = client
        .http
        .get(format!("{API}{path}"))
        .query(query)
        .header("X-IG-App-ID", APP_ID)
        .header(
            "X-CSRFToken",
            client.cookie(&site, "csrftoken").unwrap_or_default(),
        )
        .header("X-IG-WWW-Claim", "0")
        .header("X-ASBD-ID", "198387")
        .header("Origin", "https://www.instagram.com")
        .header("Referer", "https://www.instagram.com/")
        .send()
        .await?;

    let status = resp.status();
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    if body["require_login"] == true || status == StatusCode::UNAUTHORIZED {
        return Err(fail(
            Platform::Instagram,
            "Instagram didn't accept the login cookies; open instagram.com in your browser to refresh the login, then try again",
        ));
    }
    if status == StatusCode::TOO_MANY_REQUESTS {
        return Err(fail(
            Platform::Instagram,
            "Instagram is limiting requests; try again in a few minutes",
        ));
    }
    if status == StatusCode::NOT_FOUND {
        return Err(fail(
            Platform::Instagram,
            "not found (it may be deleted, or from a private account you don't follow)",
        ));
    }
    Ok(body)
}

async fn post_from_api(client: &Client, code: &str) -> Result<Vec<Media>> {
    let pk = media_pk(code).ok_or_else(|| {
        fail(
            Platform::Instagram,
            "couldn't find the post ID in this link",
        )
    })?;
    let body = api(client, &format!("/media/{pk}/info/"), &[]).await?;
    if body.pointer("/items/0").is_none() {
        return Err(fail(
            Platform::Instagram,
            "post not found (it may be deleted, or from a private account you don't follow)",
        ));
    }
    Ok(parse_api_item(&body["items"][0], code))
}

#[derive(Debug, PartialEq)]
enum Stories {
    Highlight(String),
    /// A user's current stories, or just one of them.
    User {
        username: String,
        story: Option<String>,
    },
}

/// Story links: `/stories/highlights/<id>/`, `/stories/<user>/[<id>/]`, and shared
/// highlight links `/s/<base64 of "highlight:<id>">`.
fn stories_in(url: &Url) -> Option<Stories> {
    let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    let is_id = |s: &&str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    match segments.as_slice() {
        ["stories", "highlights", id, ..] if is_id(id) => {
            Some(Stories::Highlight((*id).to_owned()))
        }
        ["stories", username, rest @ ..] => Some(Stories::User {
            username: (*username).to_owned(),
            story: rest.first().copied().filter(is_id).map(str::to_owned),
        }),
        ["s", encoded, ..] => {
            let engine = base64::engine::GeneralPurpose::new(
                &base64::alphabet::STANDARD,
                base64::engine::GeneralPurposeConfig::new()
                    .with_decode_padding_mode(base64::engine::DecodePaddingMode::Indifferent),
            );
            let decoded = base64::Engine::decode(&engine, encoded).ok()?;
            let id = String::from_utf8(decoded)
                .ok()?
                .strip_prefix("highlight:")?
                .to_owned();
            is_id(&id.as_str()).then_some(Stories::Highlight(id))
        }
        _ => None,
    }
}

async fn stories(client: &Client, which: Stories) -> Result<Vec<Media>> {
    if !is_logged_in(client) {
        return Err(fail(
            Platform::Instagram,
            "Instagram only shows stories to logged-in visitors; add --cookies-from-browser chrome \
             (or the browser you use Instagram in)",
        ));
    }

    let (reel_id, only) = match which {
        Stories::Highlight(id) => (format!("highlight:{id}"), None),
        Stories::User { username, story } => {
            let profile = api(
                client,
                "/users/web_profile_info/",
                &[("username", &username)],
            )
            .await?;
            let id = text(&profile, "/data/user/id").ok_or_else(|| {
                fail(
                    Platform::Instagram,
                    format!("account @{username} not found"),
                )
            })?;
            (id, story)
        }
    };

    let body = api(client, "/feed/reels_media/", &[("reel_ids", &reel_id)]).await?;
    let reel = body["reels"]
        .get(&reel_id)
        .or_else(|| body.pointer("/reels_media/0"))
        .ok_or_else(|| {
            fail(
                Platform::Instagram,
                "no stories found (they may have expired, or the account is private)",
            )
        })?;

    let author = text(reel, "/user/username");
    let title = text(reel, "/title");
    let media: Vec<Media> = reel["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let pk = text(item, "/pk").or_else(|| number(item, "/pk").map(|n| n.to_string()))?;
            if only.as_ref().is_some_and(|want| *want != pk) {
                return None;
            }
            node_media(item, pk, title.clone(), author.clone())
        })
        .collect();
    if media.is_empty() && only.is_some() {
        return Err(fail(
            Platform::Instagram,
            "that story isn't available anymore (stories expire after 24 hours)",
        ));
    }
    Ok(media)
}

/// The post's page only includes its data for visitors who already have Instagram's
/// visitor cookies, which any earlier page hands out.
async fn logged_out(client: &Client, code: &str) -> Result<Vec<Media>> {
    let site = Url::parse("https://www.instagram.com/").expect("valid URL");
    if client.cookie(&site, "csrftoken").is_none() {
        client.page(site.as_str()).await?;
    }
    let post_url = format!("https://www.instagram.com/p/{code}/");
    let page = client.page(&post_url).await?;
    if page.url.path().starts_with("/accounts/login") {
        return Err(fail(Platform::Instagram, NEEDS_LOGIN));
    }
    if let Some(post) = embedded_post(&page.html) {
        let media = parse_api_item(&post, code);
        if !media.is_empty() {
            return Ok(media);
        }
    }
    graphql_post(client, code, &post_url, &page.html).await
}

/// Logged-out post pages carry the post in the media API's shape, deep inside one
/// of their JSON scripts.
fn embedded_post(html: &str) -> Option<Value> {
    PAGE_DATA
        .captures_iter(html)
        .filter(|c| c[1].contains("xig_polaris_media"))
        .filter_map(|c| serde_json::from_str::<Value>(&c[1]).ok())
        .find_map(|data| {
            find_key(&data, "xig_polaris_media")?
                .get("if_not_gated_logged_out")
                .filter(|post| post.is_object())
                .cloned()
        })
}

fn find_key<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    match value {
        Value::Object(map) => map
            .get(key)
            .or_else(|| map.values().find_map(|v| find_key(v, key))),
        Value::Array(items) => items.iter().find_map(|v| find_key(v, key)),
        _ => None,
    }
}

async fn graphql_post(
    client: &Client,
    code: &str,
    post_url: &str,
    html: &str,
) -> Result<Vec<Media>> {
    let site = Url::parse("https://www.instagram.com/").expect("valid URL");
    let lsd = LSD_TOKEN
        .captures(html)
        .map(|c| c[1].to_owned())
        .unwrap_or_default();
    let csrf = client.cookie(&site, "csrftoken").unwrap_or_default();

    let variables = json!({
        "shortcode": code,
        "fetch_tagged_user_count": null,
        "hoisted_comment_id": null,
        "hoisted_reply_id": null,
    });
    let resp = client
        .http
        .post(GRAPHQL)
        .header("X-IG-App-ID", APP_ID)
        .header("X-FB-LSD", &lsd)
        .header("X-CSRFToken", &csrf)
        .header("X-FB-Friendly-Name", POST_QUERY_NAME)
        .header("Origin", "https://www.instagram.com")
        .header("Referer", post_url)
        .form(&[
            ("variables", variables.to_string().as_str()),
            ("doc_id", POST_QUERY_ID),
            ("lsd", &lsd),
            ("fb_api_req_friendly_name", POST_QUERY_NAME),
        ])
        .send()
        .await?;

    let status = resp.status();
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    if body["require_login"] == true
        || matches!(
            status,
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN | StatusCode::TOO_MANY_REQUESTS
        )
    {
        return Err(fail(Platform::Instagram, NEEDS_LOGIN));
    }
    let post = body
        .pointer("/data/xdt_shortcode_media")
        .filter(|p| p.is_object())
        .ok_or_else(|| {
            fail(
                Platform::Instagram,
                "post not found (it may be private or deleted)",
            )
        })?;
    Ok(parse_graphql_post(post, code))
}

/// Media API posts: `media_type` 1 is a photo, 2 a video, 8 a carousel of either.
fn parse_api_item(item: &Value, code: &str) -> Vec<Media> {
    let nodes: Vec<&Value> = match item["carousel_media"].as_array() {
        Some(children) => children.iter().collect(),
        None => vec![item],
    };
    let title = title_from_caption(text(item, "/caption/text"));
    let author = text(item, "/user/username");

    let many = nodes.len() > 1;
    nodes
        .iter()
        .enumerate()
        .filter_map(|(i, node)| {
            let id = if many {
                format!("{code}_{}", i + 1)
            } else {
                code.to_owned()
            };
            node_media(node, id, title.clone(), author.clone())
        })
        .collect()
}

/// One photo or video from the media API (a post, a carousel entry or a story).
fn node_media(
    node: &Value,
    id: String,
    title: Option<String>,
    author: Option<String>,
) -> Option<Media> {
    let videos = node["video_versions"].as_array().filter(|v| !v.is_empty());
    let mut seen = HashSet::new();
    let (kind, formats): (MediaKind, Vec<Format>) = match videos {
        Some(versions) => (
            MediaKind::Video,
            versions
                .iter()
                // Logged-out pages list the same file several times.
                .filter(|v| seen.insert(text(v, "/url").map(|u| without_query(&u).to_owned())))
                .filter_map(|v| {
                    let height = dimension(v, "/height");
                    Some(Format {
                        width: dimension(v, "/width"),
                        height,
                        ..Format::mp4(
                            height.map_or_else(|| "mp4".into(), |h| format!("{h}p")),
                            text(v, "/url")?,
                        )
                    })
                })
                .chain(dash_formats(node))
                .collect(),
        ),
        None => (MediaKind::Image, best_image(node).into_iter().collect()),
    };
    if formats.is_empty() {
        return None;
    }
    Some(Media {
        platform: Platform::Instagram,
        kind,
        id,
        title,
        author,
        thumbnail: text(node, "/image_versions2/candidates/0/url"),
        duration: node["video_duration"].as_f64(),
        formats,
    })
}

fn without_query(url: &str) -> &str {
    url.split('?').next().unwrap_or(url)
}

/// The sharpest version of a video is often only in its DASH manifest, as separate
/// video and audio streams.
fn dash_formats(node: &Value) -> Vec<Format> {
    let Some(manifest) = node["video_dash_manifest"].as_str() else {
        return Vec::new();
    };
    REPRESENTATION
        .captures_iter(manifest)
        .filter_map(|c| {
            let attrs: HashMap<&str, &str> = ATTRIBUTE
                .captures_iter(c.get(1)?.as_str())
                .filter_map(|a| Some((a.get(1)?.as_str(), a.get(2)?.as_str())))
                .collect();
            let number = |name: &str| attrs.get(name)?.parse::<u64>().ok();
            let url = BASE_URL.captures(c.get(2)?.as_str())?[1].replace("&amp;", "&");
            let bitrate = number("bandwidth");
            let kbps = bitrate.unwrap_or(0) / 1000;
            let stream = Format {
                bitrate,
                size: number("FBContentLength"),
                codec: attrs.get("codecs").map(|c| (*c).to_owned()),
                ..Format::mp4("", url)
            };
            let mime = attrs.get("mimeType")?;
            if mime.starts_with("video/") {
                let width = number("width").and_then(|w| u32::try_from(w).ok());
                let height = number("height").and_then(|h| u32::try_from(h).ok());
                let quality = width.zip(height).map_or(0, |(w, h)| w.min(h));
                Some(Format {
                    id: format!("{quality}p-{kbps}k"),
                    width,
                    height,
                    has_audio: false,
                    ..stream
                })
            } else if mime.starts_with("audio/") {
                Some(Format {
                    id: format!("audio-{kbps}k"),
                    has_video: false,
                    ..stream
                })
            } else {
                None
            }
        })
        .collect()
}

/// Instagram lists the full-size picture first; logged-out pages leave out the sizes,
/// so ties go to the earliest candidate.
fn best_image(node: &Value) -> Option<Format> {
    let (index, best) = node
        .pointer("/image_versions2/candidates")?
        .as_array()?
        .iter()
        .enumerate()
        .max_by_key(|(i, c)| {
            let pixels = number(c, "/width").unwrap_or(0) * number(c, "/height").unwrap_or(0);
            (pixels, Reverse(*i))
        })?;
    let size = |key: &str, original: &str| {
        dimension(best, key).or_else(|| (index == 0).then(|| dimension(node, original)).flatten())
    };
    Some(Format {
        width: size("/width", "/original_width"),
        height: size("/height", "/original_height"),
        ..Format::image("image", text(best, "/url")?)
    })
}

fn parse_graphql_post(post: &Value, code: &str) -> Vec<Media> {
    let nodes: Vec<&Value> = match post["edge_sidecar_to_children"]["edges"].as_array() {
        Some(edges) => edges.iter().map(|e| &e["node"]).collect(),
        None => vec![post],
    };

    let many = nodes.len() > 1;
    nodes
        .iter()
        .enumerate()
        .filter_map(|(i, node)| {
            let is_video = node["is_video"] == true;
            let url = text(
                node,
                if is_video {
                    "/video_url"
                } else {
                    "/display_url"
                },
            )?;
            let format = Format {
                width: dimension(node, "/dimensions/width"),
                height: dimension(node, "/dimensions/height"),
                ..if is_video {
                    Format::mp4("mp4", url)
                } else {
                    Format::image("image", url)
                }
            };
            Some(Media {
                platform: Platform::Instagram,
                kind: if is_video {
                    MediaKind::Video
                } else {
                    MediaKind::Image
                },
                id: if many {
                    format!("{code}_{}", i + 1)
                } else {
                    code.to_owned()
                },
                title: title_from_caption(text(post, "/edge_media_to_caption/edges/0/node/text")),
                author: text(post, "/owner/username"),
                thumbnail: text(node, "/display_url"),
                duration: node["video_duration"].as_f64(),
                formats: vec![format],
            })
        })
        .collect()
}

fn shortcode(url: &Url) -> Option<&str> {
    segment_after(url, &["p", "reel", "reels", "tv"])
}

/// Shortcodes are the post's numeric ID in a URL-safe base 64. Private posts add
/// extra characters after the first 11, which aren't part of the ID.
fn media_pk(code: &str) -> Option<u128> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    code.bytes().take(11).try_fold(0u128, |pk, c| {
        let digit = ALPHABET.iter().position(|&a| a == c)?;
        Some(pk * 64 + digit as u128)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Selection;

    #[test]
    fn finds_shortcode() {
        let code = |s: &str| shortcode(&Url::parse(s).unwrap()).map(str::to_owned);
        assert_eq!(
            code("https://www.instagram.com/p/aye83DjauH/").as_deref(),
            Some("aye83DjauH")
        );
        assert_eq!(
            code("https://www.instagram.com/reel/C1a2b3/?igsh=x").as_deref(),
            Some("C1a2b3")
        );
        assert_eq!(code("https://www.instagram.com/someone/").as_deref(), None);
    }

    #[test]
    fn recognises_story_links() {
        let stories = |s: &str| stories_in(&Url::parse(s).unwrap());
        assert_eq!(
            stories("https://www.instagram.com/stories/highlights/18214689214337451/"),
            Some(Stories::Highlight("18214689214337451".into()))
        );
        assert_eq!(
            stories("https://www.instagram.com/stories/natgeo/3456789012345678901/?igsh=x"),
            Some(Stories::User {
                username: "natgeo".into(),
                story: Some("3456789012345678901".into())
            })
        );
        assert_eq!(
            stories("https://www.instagram.com/stories/natgeo/"),
            Some(Stories::User {
                username: "natgeo".into(),
                story: None
            })
        );
        // base64 of "highlight:17983407089364361", without padding.
        assert_eq!(
            stories(
                "https://www.instagram.com/s/aGlnaGxpZ2h0OjE3OTgzNDA3MDg5MzY0MzYx?story_media_id=1"
            ),
            Some(Stories::Highlight("17983407089364361".into()))
        );
        assert_eq!(stories("https://www.instagram.com/p/aye83DjauH/"), None);
    }

    #[test]
    fn converts_shortcode_to_media_id() {
        assert_eq!(media_pk("aye83DjauH"), Some(482_584_233_761_418_119));
        assert_eq!(media_pk("not valid!"), None);
    }

    #[test]
    fn reads_api_carousel() {
        let item = json!({
            "user": { "username": "someone" },
            "caption": { "text": "hello\nworld" },
            "carousel_media": [
                { "media_type": 1, "image_versions2": { "candidates": [
                    { "width": 640, "height": 640, "url": "https://i/small.jpg" },
                    { "width": 1440, "height": 1440, "url": "https://i/big.jpg?x=1" },
                ]}},
                { "media_type": 2, "video_duration": 9.5, "video_versions": [
                    { "width": 720, "height": 1280, "url": "https://v/720.mp4" },
                ]},
            ]
        });
        let media = parse_api_item(&item, "abc");
        assert_eq!(media.len(), 2);
        assert_eq!(
            (media[0].kind, media[0].id.as_str()),
            (MediaKind::Image, "abc_1")
        );
        assert_eq!(media[0].formats[0].url, "https://i/big.jpg?x=1");
        assert_eq!(
            (media[1].kind, media[1].formats[0].id.as_str()),
            (MediaKind::Video, "1280p")
        );
        assert_eq!(media[1].title.as_deref(), Some("hello"));
        assert_eq!(media[1].author.as_deref(), Some("someone"));
    }

    #[test]
    fn reads_post_embedded_in_page() {
        let data = json!({"require": [["ScheduledServerJS", "handle", null, [{"__bbox": {"result": {"data": {
            "xig_polaris_media": {"if_not_gated_logged_out": {
                "media_type": 2,
                "user": {"username": "someone"},
                "caption": {"text": "Hello"},
                "video_versions": [
                    {"type": 101, "url": "https://cdn/v.mp4?a=1"},
                    {"type": 102, "url": "https://cdn/v.mp4?a=2"}
                ],
                "video_dash_manifest": "<MPD><AdaptationSet><Representation id=\"1v\" bandwidth=\"828327\" codecs=\"vp09\" mimeType=\"video/mp4\" FBContentLength=\"1499272\" width=\"1440\" height=\"1080\"><BaseURL>https://cdn/hd.mp4?x=1&amp;y=2</BaseURL></Representation></AdaptationSet><AdaptationSet><Representation id=\"2a\" bandwidth=\"49474\" codecs=\"mp4a.40.5\" mimeType=\"audio/mp4\"><BaseURL>https://cdn/a.mp4</BaseURL></Representation></AdaptationSet></MPD>"
            }}
        }}}}]]]});
        let html = format!(
            r#"<script type="application/json" data-sjs>{{"other":1}}</script><script type="application/json" data-sjs>{data}</script>"#
        );
        let post = embedded_post(&html).unwrap();
        let media = parse_api_item(&post, "abc");
        assert_eq!(media.len(), 1);
        assert_eq!(media[0].author.as_deref(), Some("someone"));

        let ids: Vec<&str> = media[0].formats.iter().map(|f| f.id.as_str()).collect();
        assert_eq!(ids, ["mp4", "1080p-828k", "audio-49k"]);
        let hd = &media[0].formats[1];
        assert_eq!(hd.url, "https://cdn/hd.mp4?x=1&y=2");
        assert_eq!(hd.size, Some(1_499_272));
        assert!(
            matches!(media[0].best(), Some(Selection::Merge { video, audio }) if video.id == "1080p-828k" && audio.id == "audio-49k")
        );
    }

    #[test]
    fn picks_first_image_when_sizes_are_missing() {
        let node = json!({
            "original_width": 1504,
            "original_height": 1984,
            "image_versions2": {"candidates": [
                {"url": "https://cdn/full.jpg"},
                {"url": "https://cdn/p1080x1080.jpg"},
                {"url": "https://cdn/s150x150.jpg"}
            ]}
        });
        let image = best_image(&node).unwrap();
        assert_eq!(image.url, "https://cdn/full.jpg");
        assert_eq!((image.width, image.height), (Some(1504), Some(1984)));
    }

    #[test]
    fn reads_graphql_carousel() {
        let post = json!({
            "owner": { "username": "someone" },
            "edge_sidecar_to_children": { "edges": [
                { "node": { "is_video": false, "display_url": "https://i/1.jpg" } },
                { "node": { "is_video": true, "video_url": "https://v/2.mp4", "dimensions": { "width": 1080, "height": 1920 } } },
            ]}
        });
        let media = parse_graphql_post(&post, "abc");
        assert_eq!(media.len(), 2);
        assert_eq!(media[0].kind, MediaKind::Image);
        assert_eq!(media[1].formats[0].height, Some(1920));
    }
}
