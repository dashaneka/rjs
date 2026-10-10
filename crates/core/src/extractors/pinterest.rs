//! Pinterest: pins (pictures, GIFs, videos, carousels and multi-page idea pins),
//! boards and everything a user has saved, all from the JSON API its website uses.

use std::collections::HashSet;

use futures_util::future::join_all;
use reqwest::header::{ACCEPT, RANGE};
use reqwest::{StatusCode, Url};
use serde_json::{Value, json};

use super::{dimension, number, text, title_from_caption};
use crate::download::hls;
use crate::error::fail;
use crate::{Client, Format, Media, MediaKind, Platform, Profile, Result};

const API: &str = "https://www.pinterest.com/resource/";
const PAGE_SIZE: u32 = 25;

/// First path segments that are site pages rather than usernames.
const NOT_USERS: &[&str] = &[
    "pin",
    "search",
    "ideas",
    "today",
    "explore",
    "categories",
    "topics",
    "business",
    "settings",
    "shopping",
    "videos",
    "_",
    "login",
    "signup",
    "about",
];

enum Target {
    Pin(String),
    User { username: String, created: bool },
    Board { username: String, slug: String },
}

pub(super) async fn extract(client: &Client, url: &Url) -> Result<Vec<Media>> {
    match target(client, url).await? {
        Target::Pin(id) => pin_media(client, &fetch_pin(client, &id).await?).await,
        Target::User { .. } | Target::Board { .. } => Err(fail(
            Platform::Pinterest,
            "this short link leads to a board or profile; use its full link instead",
        )),
    }
}

/// Boards (`/<user>/<board>/`) and users (`/<user>/`) are downloaded page by page.
pub(super) fn is_profile(url: &Url) -> bool {
    !is_short_link(url) && matches!(parse(url), Some(Target::User { .. } | Target::Board { .. }))
}

pub(super) async fn profile(client: &Client, url: &Url) -> Result<Profile> {
    match target(client, url).await? {
        Target::Board { username, slug } => {
            let board = call(client, "Board", json!({"username": username, "slug": slug})).await?;
            let board = &board["data"];
            let id = text(board, "/id")
                .ok_or_else(|| fail(Platform::Pinterest, "couldn't read the board"))?;
            Ok(Profile {
                platform: Platform::Pinterest,
                username: format!("{username}/{slug}"),
                name: text(board, "/name"),
                bio: text(board, "/description"),
                post_count: number(board, "/pin_count"),
                follower_count: number(board, "/follower_count"),
                private: false,
                avatar: avatar_media(&board["owner"]),
                pager: super::Pager::Pinterest(Pager::new(
                    "BoardFeed",
                    json!({"board_id": id, "page_size": PAGE_SIZE}),
                )),
            })
        }
        Target::User { username, created } => {
            let user = fetch_user(client, &username).await?;
            // Saved pins are everything on the profile; created ones only what they uploaded.
            let resource = if created {
                "UserActivityPins"
            } else {
                "UserPins"
            };
            Ok(Profile {
                platform: Platform::Pinterest,
                name: text(&user, "/full_name"),
                bio: text(&user, "/about"),
                post_count: number(&user, "/pin_count"),
                follower_count: number(&user, "/follower_count"),
                private: user["is_private_profile"] == true,
                avatar: avatar_media(&user),
                pager: super::Pager::Pinterest(Pager::new(
                    resource,
                    json!({"username": username, "page_size": PAGE_SIZE}),
                )),
                username,
            })
        }
        Target::Pin(_) => Err(fail(Platform::Pinterest, "this link is a single pin")),
    }
}

/// The profile picture of the user behind a pin, board or profile link.
pub(super) async fn avatar(client: &Client, url: &Url) -> Result<Media> {
    let username = match target(client, url).await? {
        Target::User { username, .. } | Target::Board { username, .. } => username,
        Target::Pin(id) => {
            let pin = fetch_pin(client, &id).await?;
            creator(&pin)
                .and_then(|c| text(c, "/username"))
                .ok_or_else(|| fail(Platform::Pinterest, "couldn't find who posted this pin"))?
        }
    };
    avatar_media(&fetch_user(client, &username).await?)
        .ok_or_else(|| fail(Platform::Pinterest, "couldn't find the profile picture"))
}

pub(crate) struct Pager {
    resource: &'static str,
    options: Value,
    bookmark: Option<String>,
    done: bool,
}

impl Pager {
    fn new(resource: &'static str, options: Value) -> Self {
        Self {
            resource,
            options,
            bookmark: None,
            done: false,
        }
    }

    /// The next batch of pins, possibly empty; `None` once there are no more.
    pub(crate) async fn next(&mut self, client: &Client) -> Result<Option<Vec<Media>>> {
        if self.done {
            return Ok(None);
        }
        let mut options = self.options.clone();
        if let Some(bookmark) = &self.bookmark {
            options["bookmarks"] = json!([bookmark]);
        }
        let page = call(client, self.resource, options).await?;
        self.bookmark = text(&page, "/bookmark").filter(|b| b != "-end-");
        self.done = self.bookmark.is_none();

        let mut media = Vec::new();
        for item in page["data"].as_array().into_iter().flatten() {
            if item["type"] != "pin" {
                continue;
            }
            // Lists only carry previews of videos and leave out the pages of idea
            // pins, so those are looked up one by one.
            let full;
            let pin = match text(item, "/id") {
                Some(id) if item["videos"].is_object() || lacks_pages(item) => {
                    match fetch_pin(client, &id).await {
                        Ok(pin) => {
                            full = pin;
                            &full
                        }
                        Err(_) => item,
                    }
                }
                _ => item,
            };
            // A pin that can't be read shouldn't stop the rest of the board.
            if let Ok(found) = pin_media(client, pin).await {
                media.extend(found);
            }
        }
        Ok(Some(media))
    }
}

fn lacks_pages(pin: &Value) -> bool {
    pin.pointer("/story_pin_data/pages").is_some_and(|pages| {
        pages.as_array().is_none_or(|pages| {
            pages
                .iter()
                .all(|p| p["blocks"].as_array().is_none_or(Vec::is_empty))
        })
    })
}

async fn call(client: &Client, resource: &str, options: Value) -> Result<Value> {
    let data = json!({ "options": options }).to_string();
    let resp = client
        .http
        .get(format!("{API}{resource}Resource/get/"))
        .query(&[("data", data)])
        .header(ACCEPT, "application/json")
        .header("X-Pinterest-PWS-Handler", "www/[username].js")
        .send()
        .await?;
    if resp.status() == StatusCode::NOT_FOUND {
        return Err(fail(
            Platform::Pinterest,
            "couldn't find this on Pinterest; it may be private or deleted",
        ));
    }
    let mut body: Value = resp.error_for_status()?.json().await?;
    Ok(body["resource_response"].take())
}

async fn fetch_pin(client: &Client, id: &str) -> Result<Value> {
    let options = json!({"field_set_key": "unauth_react_main_pin", "id": id});
    Ok(call(client, "Pin", options).await?["data"].take())
}

async fn fetch_user(client: &Client, username: &str) -> Result<Value> {
    Ok(call(client, "User", json!({ "username": username })).await?["data"].take())
}

fn is_short_link(url: &Url) -> bool {
    url.host_str()
        .is_some_and(|h| h == "pin.it" || h.ends_with(".pin.it"))
}

/// What a link points at, following `pin.it` short links first.
async fn target(client: &Client, url: &Url) -> Result<Target> {
    let resolved;
    let url = if is_short_link(url) {
        resolved = client.http.get(url.as_str()).send().await?.url().clone();
        &resolved
    } else {
        url
    };
    parse(url).ok_or_else(|| {
        fail(
            Platform::Pinterest,
            "this link isn't a pin, board or profile",
        )
    })
}

/// `/pin/<id>/` (or `/pin/<title>--<id>/`), `/<user>/<board>/` or `/<user>/`.
fn parse(url: &Url) -> Option<Target> {
    let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    match segments.as_slice() {
        ["pin", pin, ..] => {
            let id = pin.rsplit("--").next()?;
            id.bytes()
                .all(|b| b.is_ascii_digit())
                .then(|| Target::Pin(id.to_owned()))
        }
        [user, ..] if NOT_USERS.contains(user) => None,
        [user] | [user, "_saved" | "pins"] => Some(Target::User {
            username: (*user).to_owned(),
            created: false,
        }),
        [user, "_created"] => Some(Target::User {
            username: (*user).to_owned(),
            created: true,
        }),
        [user, slug] | [user, slug, ..] if !slug.starts_with('_') => Some(Target::Board {
            username: (*user).to_owned(),
            slug: (*slug).to_owned(),
        }),
        _ => None,
    }
}

/// Everything a pin holds, one item per picture or video.
async fn pin_media(client: &Client, pin: &Value) -> Result<Vec<Media>> {
    let id = text(pin, "/id").ok_or_else(|| fail(Platform::Pinterest, "couldn't read the pin"))?;
    let mut items: Vec<(MediaKind, Vec<Format>, Option<f64>)> = Vec::new();

    if let Some(list) = pin.pointer("/videos/video_list") {
        items.push(video_item(client, list).await);
    } else if let Some(pages) = pin
        .pointer("/story_pin_data/pages")
        .and_then(Value::as_array)
    {
        for page in pages {
            let before = items.len();
            for block in page["blocks"].as_array().into_iter().flatten() {
                if let Some(list) = block.pointer("/video/video_list") {
                    items.push(video_item(client, list).await);
                } else if let Some(images) = block.pointer("/image/images") {
                    items.extend(image_item(client, images).await);
                }
            }
            if items.len() == before {
                items.extend(image_item(client, &page["image"]["images"]).await);
            }
        }
    } else if let Some(slots) = pin
        .pointer("/carousel_data/carousel_slots")
        .and_then(Value::as_array)
    {
        for slot in slots {
            items.extend(image_item(client, &slot["images"]).await);
        }
    }

    if items.is_empty() {
        let uploaded =
            text(pin, "/domain").is_none_or(|d| d.eq_ignore_ascii_case("uploaded by user"));
        if !uploaded && pin["embed"]["type"] != "gif" && text(pin, "/embed/src").is_some() {
            let site = text(pin, "/domain").unwrap_or_default();
            return Err(fail(
                Platform::Pinterest,
                format!("this pin plays a video from {site}, which rjs can't download"),
            ));
        }
        items.extend(image_item(client, &pin["images"]).await);
    }
    items.retain(|(_, formats, _)| !formats.is_empty());

    let count = items.len();
    let title = ["/title", "/grid_title", "/closeup_unified_title"]
        .iter()
        .find_map(|p| {
            text(pin, p)
                .map(|t| t.trim().to_owned())
                .filter(|t| !t.is_empty())
        })
        .or_else(|| title_from_caption(text(pin, "/description")));
    let author = creator(pin).and_then(|c| text(c, "/username"));
    let thumbnail = text(pin, "/images/orig/url");
    Ok(items
        .into_iter()
        .enumerate()
        .map(|(i, (kind, formats, duration))| Media {
            platform: Platform::Pinterest,
            kind,
            id: if count > 1 {
                format!("{id}_{}", i + 1)
            } else {
                id.clone()
            },
            title: title.clone(),
            author: author.clone(),
            thumbnail: thumbnail.clone(),
            duration,
            formats,
        })
        .collect())
}

/// The person who made the pin, or failing that whoever saved it.
fn creator(pin: &Value) -> Option<&Value> {
    ["native_creator", "pinner"]
        .iter()
        .map(|k| &pin[*k])
        .find(|c| c["username"].is_string())
}

/// Pinterest lists several MP4s and HLS playlists. The sizes it gives for the
/// MP4s are often wrong (a "720P" file can be 240×240), so each is measured.
async fn video_item(client: &Client, list: &Value) -> (MediaKind, Vec<Format>, Option<f64>) {
    let mut candidates = Vec::new();
    let mut playlists = Vec::new();
    let mut duration = None;
    for (key, entry) in list.as_object().into_iter().flatten() {
        let Some(url) = text(entry, "/url") else {
            continue;
        };
        duration = duration.or_else(|| number(entry, "/duration").map(|ms| ms as f64 / 1000.0));
        if key.contains("HLS") || url.contains(".m3u8") {
            playlists.push((key.contains("MOBILE"), url));
        } else if !key.contains("HEVC") {
            // HEVC copies are the same picture in a format fewer players handle.
            candidates.push((key.trim_start_matches("V_").to_ascii_lowercase(), url));
        }
    }

    let sizes = join_all(candidates.iter().map(|(_, url)| mp4_size(client, url))).await;
    let mut seen = HashSet::new();
    let mut formats = Vec::new();
    for ((id, url), size) in candidates.into_iter().zip(sizes) {
        if seen.insert(size) {
            formats.push(Format {
                width: size.map(|s| s.0),
                height: size.map(|s| s.1),
                ..Format::mp4(id, url)
            });
        }
    }
    playlists.sort();
    if let Some((_, playlist)) = playlists.into_iter().next() {
        match hls::formats(client, &playlist, &[]).await {
            Ok(found) => formats.extend(found),
            Err(_) if formats.is_empty() => formats.push(Format::hls("hls", playlist)),
            Err(_) => {}
        }
    }
    (MediaKind::Video, formats, duration)
}

/// Width and height read from the start of an MP4 file.
async fn mp4_size(client: &Client, url: &str) -> Option<(u32, u32)> {
    let resp = client
        .http
        .get(url)
        .header(RANGE, "bytes=0-65535")
        .send()
        .await
        .ok()?;
    video_size(&resp.bytes().await.ok()?)
}

/// Finds the first track with a picture size in `moov/trak/tkhd`. Works when the
/// file's index comes first, as it does for files meant for streaming.
fn video_size(data: &[u8]) -> Option<(u32, u32)> {
    let moov = find_box(data, b"moov")?;
    let mut rest = moov;
    while let Some((kind, body, next)) = next_box(rest) {
        rest = next;
        if &kind != b"trak" {
            continue;
        }
        let tkhd = find_box(body, b"tkhd")?;
        // Width and height are 16.16 fixed point numbers at the end of the header.
        let at = if tkhd.first()? == &1 { 88 } else { 76 };
        let read = |i: usize| Some(u32::from_be_bytes(tkhd.get(i..i + 4)?.try_into().ok()?) >> 16);
        let (width, height) = (read(at)?, read(at + 4)?);
        if width > 0 && height > 0 {
            return Some((width, height));
        }
    }
    None
}

fn find_box<'a>(mut data: &'a [u8], wanted: &[u8; 4]) -> Option<&'a [u8]> {
    while let Some((kind, body, rest)) = next_box(data) {
        if &kind == wanted {
            return Some(body);
        }
        data = rest;
    }
    None
}

/// Splits off the first box: its type, its contents (cut short if the data
/// ends early) and what follows it.
fn next_box(data: &[u8]) -> Option<([u8; 4], &[u8], &[u8])> {
    let size = u32::from_be_bytes(data.get(..4)?.try_into().ok()?) as usize;
    let kind: [u8; 4] = data.get(4..8)?.try_into().ok()?;
    let (header, size) = match size {
        0 => (8, data.len()),
        1 => (
            16,
            usize::try_from(u64::from_be_bytes(data.get(8..16)?.try_into().ok()?)).ok()?,
        ),
        n => (8, n),
    };
    if size < header {
        return None;
    }
    let end = size.min(data.len());
    Some((kind, data.get(header..end)?, &data[end..]))
}

/// The largest version of a picture (GIFs stay GIFs).
async fn image_item(
    client: &Client,
    images: &Value,
) -> Option<(MediaKind, Vec<Format>, Option<f64>)> {
    let images = images.as_object()?;
    let original = ["orig", "originals"].iter().find_map(|k| images.get(*k));
    let entry = original.or_else(|| images.values().max_by_key(|v| dimension(v, "/width")))?;
    let mut url = text(entry, "/url")?;
    let (mut width, mut height) = (dimension(entry, "/width"), dimension(entry, "/height"));

    // Without an original listed, the full-size file usually still sits under /originals/.
    if original.is_none()
        && let Some(full) = originals_url(&url)
        && client
            .http
            .head(&full)
            .send()
            .await
            .is_ok_and(|r| r.status().is_success())
    {
        url = full;
        (width, height) = (None, None);
    }
    let format = Format {
        width,
        height,
        ..Format::image("orig", url)
    };
    Some((MediaKind::Image, vec![format], None))
}

/// `https://i.pinimg.com/736x/ab/…` becomes `https://i.pinimg.com/originals/ab/…`.
fn originals_url(url: &str) -> Option<String> {
    let mut parsed = Url::parse(url).ok()?;
    let rest: Vec<String> = parsed.path_segments()?.skip(1).map(str::to_owned).collect();
    let size = parsed.path_segments()?.next()?.to_owned();
    if !size.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    parsed.set_path(&format!("/originals/{}", rest.join("/")));
    Some(parsed.into())
}

fn avatar_media(user: &Value) -> Option<Media> {
    let url = ["/image_xlarge_url", "/image_large_url", "/image_medium_url"]
        .iter()
        .find_map(|p| text(user, p))?;
    Some(Media {
        platform: Platform::Pinterest,
        kind: MediaKind::Image,
        id: "avatar".into(),
        title: text(user, "/full_name"),
        author: text(user, "/username"),
        thumbnail: None,
        duration: None,
        formats: vec![Format::image("avatar", url)],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target_of(s: &str) -> Option<String> {
        Some(match parse(&Url::parse(s).unwrap())? {
            Target::Pin(id) => format!("pin {id}"),
            Target::User { username, created } => format!("user {username} {created}"),
            Target::Board { username, slug } => format!("board {username}/{slug}"),
        })
    }

    #[test]
    fn reads_links() {
        assert_eq!(
            target_of("https://www.pinterest.com/pin/664281013778109217/").as_deref(),
            Some("pin 664281013778109217")
        );
        assert_eq!(
            target_of("https://pinterest.com/pin/blue-lagoon-nails--2885187256207927").as_deref(),
            Some("pin 2885187256207927")
        );
        assert_eq!(
            target_of("https://www.pinterest.ca/mashal0407/cool-diys/").as_deref(),
            Some("board mashal0407/cool-diys")
        );
        assert_eq!(
            target_of("https://www.pinterest.com/someone/").as_deref(),
            Some("user someone false")
        );
        assert_eq!(
            target_of("https://www.pinterest.com/someone/_created/").as_deref(),
            Some("user someone true")
        );
        assert_eq!(
            target_of("https://www.pinterest.com/search/pins/?q=x"),
            None
        );
        assert_eq!(target_of("https://www.pinterest.com/pin/abc/"), None);
    }

    #[test]
    fn finds_originals() {
        assert_eq!(
            originals_url("https://i.pinimg.com/736x/60/d2/35/abc.jpg").as_deref(),
            Some("https://i.pinimg.com/originals/60/d2/35/abc.jpg")
        );
        assert_eq!(
            originals_url("https://i.pinimg.com/originals/60/abc.jpg"),
            None
        );
    }

    #[test]
    fn measures_mp4_pictures() {
        fn mp4_box(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
            let mut b = ((body.len() + 8) as u32).to_be_bytes().to_vec();
            b.extend_from_slice(kind);
            b.extend_from_slice(body);
            b
        }
        let tkhd = |w: u32, h: u32| {
            let mut body = vec![0; 76];
            body.extend((w << 16).to_be_bytes());
            body.extend((h << 16).to_be_bytes());
            mp4_box(b"tkhd", &body)
        };
        let audio = mp4_box(b"trak", &tkhd(0, 0));
        let video = mp4_box(b"trak", &tkhd(720, 1280));
        let mut file = mp4_box(b"ftyp", b"isom");
        file.extend(mp4_box(b"moov", &[audio, video].concat()));
        file.extend(mp4_box(b"mdat", &[0; 32]));
        assert_eq!(video_size(&file), Some((720, 1280)));
        assert_eq!(video_size(&file[..20]), None);
    }

    #[test]
    fn spots_idea_pins_without_pages() {
        assert!(lacks_pages(
            &json!({"story_pin_data": {"pages": [{"type": "page"}]}})
        ));
        assert!(!lacks_pages(
            &json!({"story_pin_data": {"pages": [{"blocks": [{}]}]}})
        ));
        assert!(!lacks_pages(&json!({"videos": {}})));
    }
}
