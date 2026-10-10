//! xHamster: the page's `window.initials` JSON lists HLS playlists whose links
//! are scrambled with one of a few simple number generators.

use std::sync::LazyLock;

use regex::Regex;
use reqwest::Url;
use serde_json::Value;

use super::{number, segment_after, text, unescape_html, with_referer};
use crate::download::hls;
use crate::error::fail;
use crate::{Client, Media, MediaKind, Platform, Result};

static INITIALS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)window\.initials\s*=\s*(\{.+?\})\s*;\s*</script>").unwrap());
static CLOSED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?s)<div[^>]+id=["']videoClosed["'][^>]*>(.+?)</div>"#).unwrap()
});
static TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<[^>]+>").unwrap());

pub(super) async fn extract(client: &Client, url: &Url) -> Result<Vec<Media>> {
    let id = video_id(url).ok_or_else(|| {
        fail(
            Platform::XHamster,
            "couldn't find the video ID in this link",
        )
    })?;
    let mut page_url = url.clone();
    if let Some(desktop) = url.host_str().and_then(|h| h.strip_prefix("m.")) {
        let desktop = desktop.to_owned();
        let _ = page_url.set_host(Some(&desktop));
    }
    let page = client.page(page_url.as_str()).await?;
    if let Some(c) = CLOSED.captures(&page.html) {
        let message = unescape_html(TAG.replace_all(&c[1], "").trim());
        return Err(fail(Platform::XHamster, message));
    }
    let initials: Value = INITIALS
        .captures(&page.html)
        .and_then(|c| serde_json::from_str(&c[1]).ok())
        .ok_or_else(|| fail(Platform::XHamster, "couldn't find the video on the page"))?;

    // The site's single-file MP4 links are refused by its servers, so only HLS is used.
    let referer = page.url.to_string();
    let headers = vec![("Referer".to_owned(), referer.clone())];
    let mut formats = Vec::new();
    for link in playlist_links(&initials["xplayerSettings"]["sources"]) {
        if let Ok(found) = hls::formats(client, &link, &headers).await
            && !found.is_empty()
        {
            formats = found;
            break;
        }
    }
    if formats.is_empty() {
        return Err(fail(Platform::XHamster, "no video found on the page"));
    }

    let video = &initials["videoModel"];
    Ok(vec![Media {
        platform: Platform::XHamster,
        kind: MediaKind::Video,
        id,
        title: text(video, "/title"),
        author: text(video, "/author/name"),
        thumbnail: text(video, "/thumbURL"),
        duration: number(video, "/duration").map(|s| s as f64),
        formats: with_referer(formats, &referer),
    }])
}

/// Playlist links in the order worth trying: H.264 before other codecs, which
/// not every player handles.
fn playlist_links(sources: &Value) -> Vec<String> {
    let mut codecs: Vec<&String> = sources["hls"]
        .as_object()
        .into_iter()
        .chain(sources["standard"].as_object())
        .flat_map(|m| m.keys())
        .collect();
    codecs.sort_by_key(|c| (c.as_str() != "h264", c.as_str()));
    codecs.dedup();

    let mut links = Vec::new();
    for codec in codecs {
        let mut candidates = vec![&sources["hls"][codec.as_str()]];
        if let Some(list) = sources["standard"][codec.as_str()].as_array() {
            candidates.extend(
                list.iter()
                    .filter(|f| f["quality"] == "auto" || f["label"] == "auto"),
            );
        }
        for candidate in candidates {
            for key in ["url", "fallback"] {
                if let Some(link) = candidate[key].as_str().and_then(decipher)
                    && link.contains(".m3u8")
                    && !links.contains(&link)
                {
                    links.push(link);
                }
            }
        }
    }
    links
}

/// `/videos/<title>-<id>` or `/movies/<id>/<title>.html`.
fn video_id(url: &Url) -> Option<String> {
    let id = match segment_after(url, &["videos"]) {
        Some(slug) => slug.rsplit('-').next()?,
        None => segment_after(url, &["movies"])?,
    };
    Some(id.to_owned()).filter(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric()))
}

/// Links come as hex ciphertext, or as a URL whose first path segment is one.
fn decipher(value: &str) -> Option<String> {
    let is_hex = |s: &str| s.len() >= 12 && s.bytes().all(|b| b.is_ascii_hexdigit());
    if is_hex(value) {
        return decipher_hex(value);
    }
    let mut url = Url::parse(value).ok()?;
    let path = url.path().strip_prefix('/')?.to_owned();
    let Some(end) = path.find(['/', ',']) else {
        return Some(value.to_owned());
    };
    let (hex, rest) = path.split_at(end);
    if !is_hex(hex) {
        return Some(value.to_owned());
    }
    url.set_path(&format!("/{}{rest}", decipher_hex(hex)?));
    Some(url.into())
}

/// Byte 0 picks the generator, bytes 1–4 seed it, and the rest is XORed with its output.
fn decipher_hex(hex: &str) -> Option<String> {
    let bytes: Vec<u8> = (0..hex.len() / 2)
        .map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16))
        .collect::<std::result::Result<_, _>>()
        .ok()?;
    let seed = u32::from_le_bytes(bytes.get(1..5)?.try_into().ok()?);
    let mut generator = Generator::new(*bytes.first()?, seed)?;
    // The plain text is Latin-1, so each byte is one character.
    Some(
        bytes[5..]
            .iter()
            .map(|b| char::from(b ^ generator.next_byte()))
            .collect(),
    )
}

struct Generator {
    algorithm: u8,
    state: u32,
}

impl Generator {
    fn new(algorithm: u8, seed: u32) -> Option<Self> {
        (1..=7).contains(&algorithm).then_some(Self {
            algorithm,
            state: seed,
        })
    }

    fn next_byte(&mut self) -> u8 {
        let s = self.state;
        let out = match self.algorithm {
            // Linear congruential generator.
            1 => {
                self.state = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                self.state
            }
            // xorshift32.
            2 => {
                let mut s = s ^ (s << 13);
                s ^= s >> 17;
                self.state = s ^ (s << 5);
                self.state
            }
            // Weyl sequence and MurmurHash3's finaliser.
            3 => {
                self.state = s.wrapping_add(0x9e37_79b9);
                let mut s = self.state;
                s ^= s >> 16;
                s = s.wrapping_mul(0x85eb_ca77);
                s ^= s >> 13;
                s = s.wrapping_mul(0xc2b2_ae3d);
                s ^ (s >> 16)
            }
            4 => {
                self.state = s.wrapping_add(0x6d2b_79f5);
                let mut s = self.state.rotate_left(7).wrapping_add(0x9e37_79b9);
                s ^= s >> 11;
                s.wrapping_mul(0x27d4_eb2d)
            }
            5 => {
                let mut s = s ^ (s << 7);
                s ^= s >> 9;
                s ^= s << 8;
                self.state = s.wrapping_add(0xa5a5_a5a5);
                self.state
            }
            6 => {
                self.state = s.wrapping_mul(0x2c92_77b5).wrapping_add(0xac56_4b05);
                let s = self.state;
                (s ^ (s >> 18)) >> ((s >> 27) & 31)
            }
            _ => {
                self.state = s.wrapping_add(0x9e37_79b9);
                let s = self.state;
                let mut e = (s ^ (s << 5)).wrapping_mul(0x7feb_352d);
                e ^= e >> 15;
                e.wrapping_mul(0x846c_a68b)
            }
        };
        out as u8
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// Made with yt-dlp's decoder from "https://video.example/abc/1080p.m3u8".
    const SAMPLES: [&str; 7] = [
        "01eb32a4f82621c43f1163cbdcc0f43cb2250f69837f88703357c65561e50299d722812c2583462557",
        "02b168de3a9dd9427b1c4d58d9f011e25d125092ac4af91b244a9c55c8d51138cd12c7cf549f9a087d",
        "03eb32a4f85a66bbc162f56a4b5bdcc72c4b4713dd20c376999fda2459ac7826023353ed7a2a84e944",
        "04b168de3aec6b0e0bae3502fc07c6a8eea6144068f940e6d29a138a7b997a82853f760e27c1178302",
        "05eb32a4f8054657d6f735eaa46587850f6fc1370097f2eaf9a56ec7b09d8784fbc851193d4700c842",
        "06b168de3a0fa8caf9ac22a5eb4c72518e68f2ed7b5b58acf578eb55b4549c3290630315f79e7b7639",
        "07eb32a4f83ecd0dbb38576fc113c66542ae239814af14544eb66e5542e8eff1fa3c3d9976cdd4ba80",
    ];

    #[test]
    fn deciphers_every_generator() {
        for sample in SAMPLES {
            assert_eq!(
                decipher(sample).as_deref(),
                Some("https://video.example/abc/1080p.m3u8"),
                "generator {}",
                &sample[..2]
            );
        }
        assert_eq!(decipher_hex("08b168de3a00"), None);
    }

    #[test]
    fn deciphers_link_paths() {
        let plain = decipher(SAMPLES[0]).unwrap();
        let hex_path: String = SAMPLES[0].to_owned();
        let wrapped = format!("https://cdn.example/{hex_path}/rest.m3u8");
        assert_eq!(
            decipher(&wrapped).as_deref(),
            Some(format!("https://cdn.example/{plain}/rest.m3u8").as_str())
        );
        assert_eq!(
            decipher("https://cdn.example/hls/a.m3u8").as_deref(),
            Some("https://cdn.example/hls/a.m3u8")
        );
    }

    #[test]
    fn prefers_h264_playlists() {
        let sources = json!({
            "hls": {"av1": {"url": "https://cdn/av1.m3u8"}, "h264": {"url": "https://cdn/h264.m3u8"}},
            "standard": {"h264": [
                {"quality": "auto", "url": "https://cdn/auto.m3u8"},
                {"quality": "720p", "url": "https://cdn/720p.mp4"}
            ]}
        });
        assert_eq!(
            playlist_links(&sources),
            [
                "https://cdn/h264.m3u8",
                "https://cdn/auto.m3u8",
                "https://cdn/av1.m3u8"
            ]
        );
    }

    #[test]
    fn finds_video_ids() {
        let id = |s: &str| video_id(&Url::parse(s).unwrap());
        assert_eq!(
            id("https://xhamster.com/videos/some-title-xhm3rQT").as_deref(),
            Some("xhm3rQT")
        );
        assert_eq!(
            id("https://xhamster.com/movies/2221348/title.html").as_deref(),
            Some("2221348")
        );
    }
}
