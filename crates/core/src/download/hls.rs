//! HLS: a video served as a playlist (`.m3u8`) of short pieces, downloaded in
//! parallel and joined in order.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use aes::cipher::block_padding::Pkcs7;
use aes::cipher::{BlockModeDecrypt, KeyIvInit};
use futures_util::{StreamExt, TryStreamExt, stream};
use reqwest::Url;
use reqwest::header::{HeaderMap, RANGE};
use tokio::fs::{self, File};
use tokio::io::{AsyncWriteExt, BufWriter};

use super::{DownloadOptions, Progress, RETRIES, header_map, part_path, run_ffmpeg};
use crate::{Client, Error, Format, Result};

type Decryptor = cbc::Decryptor<aes::Aes128>;

/// The qualities offered by the playlist at `url`. Each format keeps `headers`
/// for downloading later.
pub(crate) async fn formats(
    client: &Client,
    url: &str,
    headers: &[(String, String)],
) -> Result<Vec<Format>> {
    let (base, text) = fetch_playlist(client, url, &header_map(headers)).await?;
    let mut formats = variants(&text, &base);
    if formats.is_empty() {
        // Not a list of qualities but the pieces of a single one.
        formats.push(Format::hls("hls", base.as_str()));
    }
    for f in &mut formats {
        f.headers = headers.to_vec();
    }
    Ok(formats)
}

/// Downloads `format` to `dest` and returns where it ended up: an MP4 when
/// ffmpeg is installed to rewrap the pieces, otherwise a `.ts` file next to it.
pub(super) async fn download(
    client: &Client,
    format: &Format,
    dest: &Path,
    label: &str,
    options: &DownloadOptions,
    progress: &dyn Progress,
) -> Result<PathBuf> {
    let headers = super::request_headers(format);
    let (mut base, mut text) = fetch_playlist(client, &format.url, &headers).await?;
    if let Some(best) = variants(&text, &base).into_iter().max_by_key(|f| {
        let pixels = u64::from(f.width.unwrap_or(0)) * u64::from(f.height.unwrap_or(0));
        (pixels, f.bitrate.unwrap_or(0))
    }) {
        (base, text) = fetch_playlist(client, &best.url, &headers).await?;
    }
    let playlist = parse_media(&text, &base)?;
    if playlist.segments.is_empty() {
        return Err(Error::Stream("the video's playlist is empty".into()));
    }

    let mut keys = HashMap::new();
    for key in playlist.segments.iter().filter_map(|s| s.key.as_ref()) {
        if !keys.contains_key(&key.url) {
            let bytes = fetch_bytes(client, &key.url, None, &headers).await?;
            let key_bytes: [u8; 16] = bytes
                .try_into()
                .map_err(|_| Error::Stream("the video's decryption key is invalid".into()))?;
            keys.insert(key.url.clone(), key_bytes);
        }
    }

    progress.start(label, None, 0);
    let part = part_path(dest, "part");
    let mut out = BufWriter::with_capacity(256 * 1024, File::create(&part).await?);
    if let Some(init) = &playlist.init {
        let data = fetch_bytes(client, &init.url, init.range, &headers).await?;
        out.write_all(&data).await?;
        progress.advance(data.len() as u64);
    }
    let mut pieces = stream::iter(
        playlist
            .segments
            .iter()
            .map(|segment| fetch_segment(client, segment, &keys, &headers)),
    )
    .buffered(options.connections.max(1));
    while let Some(data) = pieces.try_next().await? {
        out.write_all(&data).await?;
        progress.advance(data.len() as u64);
    }
    out.flush().await?;
    drop(out);

    // Pieces with an init section are already MP4 fragments.
    if playlist.init.is_some() {
        fs::rename(&part, dest).await?;
        return Ok(dest.to_path_buf());
    }
    if run_ffmpeg(
        &[&part],
        &["-bsf:a", "aac_adtstoasc"],
        dest,
        "converting the video to MP4",
    )
    .await?
    {
        fs::remove_file(&part).await?;
        return Ok(dest.to_path_buf());
    }
    let ts = dest.with_extension("ts");
    fs::rename(&part, &ts).await?;
    Ok(ts)
}

async fn fetch_playlist(client: &Client, url: &str, headers: &HeaderMap) -> Result<(Url, String)> {
    let resp = client
        .http
        .get(url)
        .headers(headers.clone())
        .send()
        .await?
        .error_for_status()?;
    let base = resp.url().clone();
    let text = resp.text().await?;
    if !text.trim_start().starts_with("#EXTM3U") {
        return Err(Error::Stream(
            "the video's playlist couldn't be read".into(),
        ));
    }
    Ok((base, text))
}

async fn fetch_segment(
    client: &Client,
    segment: &Segment,
    keys: &HashMap<Url, [u8; 16]>,
    headers: &HeaderMap,
) -> Result<Vec<u8>> {
    let mut data = fetch_bytes(client, &segment.url, segment.range, headers).await?;
    if let Some(key) = &segment.key {
        // Without an explicit IV, the piece's sequence number is used.
        let iv = key
            .iv
            .unwrap_or_else(|| u128::from(segment.sequence).to_be_bytes());
        let len = Decryptor::new(&keys[&key.url].into(), &iv.into())
            .decrypt_padded::<Pkcs7>(&mut data)
            .map_err(|_| Error::Stream("a piece of the video couldn't be decrypted".into()))?
            .len();
        data.truncate(len);
    }
    Ok(data)
}

/// Fetches `url` (or bytes `start..=end` of it), retrying after network errors.
async fn fetch_bytes(
    client: &Client,
    url: &Url,
    range: Option<(u64, u64)>,
    headers: &HeaderMap,
) -> Result<Vec<u8>> {
    let mut failures = 0;
    loop {
        let mut request = client.http.get(url.clone()).headers(headers.clone());
        if let Some((start, end)) = range {
            request = request.header(RANGE, format!("bytes={start}-{end}"));
        }
        let result: Result<Vec<u8>> = async {
            let resp = request.send().await?.error_for_status()?;
            Ok(resp.bytes().await?.to_vec())
        }
        .await;
        match result {
            Ok(data) => return Ok(data),
            Err(e) if failures >= RETRIES => return Err(e),
            Err(_) => {
                failures += 1;
                tokio::time::sleep(Duration::from_millis(500 * u64::from(failures))).await;
            }
        }
    }
}

/// The qualities listed by a master playlist; empty for any other playlist.
fn variants(text: &str, base: &Url) -> Vec<Format> {
    let mut formats = Vec::new();
    let mut ids = HashSet::new();
    let mut lines = text.lines().map(str::trim);
    while let Some(line) = lines.next() {
        let Some(attrs) = line.strip_prefix("#EXT-X-STREAM-INF:") else {
            continue;
        };
        let attrs = attributes(attrs);
        let Some(uri) = lines
            .by_ref()
            .find(|l| !l.is_empty() && !l.starts_with('#'))
        else {
            break;
        };
        let Ok(url) = base.join(uri) else {
            continue;
        };
        let size = attrs.get("RESOLUTION").and_then(|r| r.split_once('x'));
        let width = size.and_then(|(w, _)| w.parse().ok());
        let height = size.and_then(|(_, h)| h.parse().ok());
        let bitrate: Option<u64> = attrs.get("BANDWIDTH").and_then(|b| b.parse().ok());
        let kbps = bitrate.unwrap_or(0) / 1000;
        let mut id = match width.zip(height) {
            Some((w, h)) => format!("hls-{}p", u32::min(w, h)),
            None => format!("hls-{kbps}k"),
        };
        if !ids.insert(id.clone()) {
            id = format!("{id}-{kbps}k");
            ids.insert(id.clone());
        }
        formats.push(Format {
            width,
            height,
            bitrate,
            codec: attrs.get("CODECS").cloned(),
            ..Format::hls(id, url.as_str())
        });
    }
    formats
}

struct Playlist {
    /// The MP4 header that fragmented-MP4 pieces need in front of them.
    init: Option<Segment>,
    segments: Vec<Segment>,
}

struct Segment {
    url: Url,
    /// Bytes `start..=end` of `url`, when pieces are parts of one file.
    range: Option<(u64, u64)>,
    key: Option<Key>,
    sequence: u64,
}

#[derive(Clone)]
struct Key {
    url: Url,
    iv: Option<[u8; 16]>,
}

fn parse_media(text: &str, base: &Url) -> Result<Playlist> {
    let bad = |what: &str| Error::Stream(format!("the video's playlist has {what}"));
    let mut playlist = Playlist {
        init: None,
        segments: Vec::new(),
    };
    let mut sequence = 0;
    let mut key = None;
    let mut pending_range = None;
    // Where the previous byte range ended, for ranges that continue from it.
    let mut last_end: Option<(Url, u64)> = None;

    for line in text.lines().map(str::trim) {
        if let Some(value) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
            sequence = value.parse().unwrap_or(0);
        } else if let Some(value) = line.strip_prefix("#EXT-X-KEY:") {
            let attrs = attributes(value);
            key = match attrs.get("METHOD").map(String::as_str) {
                None | Some("NONE") => None,
                Some("AES-128") => {
                    let uri = attrs
                        .get("URI")
                        .ok_or_else(|| bad("a key without a link"))?;
                    Some(Key {
                        url: base.join(uri).map_err(|_| bad("a bad key link"))?,
                        iv: attrs.get("IV").and_then(|iv| parse_iv(iv)),
                    })
                }
                Some(other) => {
                    return Err(Error::Stream(format!(
                        "the video is protected with {other}, which rjs can't decrypt"
                    )));
                }
            };
        } else if let Some(value) = line.strip_prefix("#EXT-X-MAP:") {
            let attrs = attributes(value);
            let uri = attrs
                .get("URI")
                .ok_or_else(|| bad("a header without a link"))?;
            let url = base.join(uri).map_err(|_| bad("a bad header link"))?;
            let range = attrs
                .get("BYTERANGE")
                .and_then(|r| parse_range(r))
                .map(|(len, offset)| {
                    let start = offset.unwrap_or(0);
                    (start, start + len - 1)
                });
            playlist.init = Some(Segment {
                url,
                range,
                key: None,
                sequence: 0,
            });
        } else if let Some(value) = line.strip_prefix("#EXT-X-BYTERANGE:") {
            pending_range = parse_range(value);
        } else if !line.is_empty() && !line.starts_with('#') {
            let url = base.join(line).map_err(|_| bad("a bad link"))?;
            let range = pending_range.take().map(|(len, offset)| {
                let start = offset.unwrap_or_else(|| match &last_end {
                    Some((prev, end)) if *prev == url => end + 1,
                    _ => 0,
                });
                (start, start + len - 1)
            });
            if let Some((_, end)) = range {
                last_end = Some((url.clone(), end));
            }
            playlist.segments.push(Segment {
                url,
                range,
                key: key.clone(),
                sequence,
            });
            sequence += 1;
        }
    }
    Ok(playlist)
}

/// `length[@offset]`, with a non-zero length.
fn parse_range(value: &str) -> Option<(u64, Option<u64>)> {
    let (len, offset) = match value.split_once('@') {
        Some((len, offset)) => (len, Some(offset.trim().parse().ok()?)),
        None => (value, None),
    };
    let len: u64 = len.trim().parse().ok().filter(|&n| n > 0)?;
    Some((len, offset))
}

fn parse_iv(value: &str) -> Option<[u8; 16]> {
    let hex = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))?;
    u128::from_str_radix(hex, 16).ok().map(u128::to_be_bytes)
}

/// Splits `KEY=value,KEY="quoted, value"` lists.
fn attributes(s: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let mut rest = s;
    while let Some((key, after)) = rest.split_once('=') {
        let (value, next) = match after.strip_prefix('"') {
            Some(quoted) => {
                let end = quoted.find('"').unwrap_or(quoted.len());
                (&quoted[..end], quoted.get(end + 1..).unwrap_or_default())
            }
            None => {
                let end = after.find(',').unwrap_or(after.len());
                (&after[..end], &after[end..])
            }
        };
        out.insert(key.trim().to_owned(), value.to_owned());
        rest = next.trim_start_matches(',');
    }
    out
}

#[cfg(test)]
mod tests {
    use aes::cipher::BlockModeEncrypt;

    use super::*;

    fn base() -> Url {
        Url::parse("https://cdn.example/v/master.m3u8?t=1").unwrap()
    }

    #[test]
    fn reads_qualities() {
        let text = "#EXTM3U\n\
            #EXT-X-STREAM-INF:BANDWIDTH=1280000,RESOLUTION=854x480,CODECS=\"avc1.4d401f,mp4a.40.2\"\n\
            480p/index.m3u8\n\
            #EXT-X-STREAM-INF:BANDWIDTH=4000000,RESOLUTION=1920x1080\n\
            https://other.example/1080.m3u8\n";
        let formats = variants(text, &base());
        assert_eq!(formats.len(), 2);
        assert_eq!(formats[0].id, "hls-480p");
        assert_eq!(formats[0].url, "https://cdn.example/v/480p/index.m3u8");
        assert_eq!(formats[0].codec.as_deref(), Some("avc1.4d401f,mp4a.40.2"));
        assert_eq!(
            (formats[1].width, formats[1].height),
            (Some(1920), Some(1080))
        );
        assert!(variants("#EXTM3U\n#EXTINF:4,\na.ts\n", &base()).is_empty());
    }

    #[test]
    fn reads_pieces_keys_and_ranges() {
        let text = "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:7\n\
            #EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\"\n\
            #EXTINF:4,\na.ts\n\
            #EXT-X-KEY:METHOD=NONE\n\
            #EXT-X-BYTERANGE:100@0\n#EXTINF:4,\nall.ts\n\
            #EXT-X-BYTERANGE:50\n#EXTINF:4,\nall.ts\n";
        let playlist = parse_media(text, &base()).unwrap();
        let s = &playlist.segments;
        assert_eq!(s.len(), 3);
        assert_eq!(s[0].sequence, 7);
        assert_eq!(
            s[0].key.as_ref().unwrap().url.as_str(),
            "https://cdn.example/v/key.bin"
        );
        assert!(s[1].key.is_none());
        assert_eq!(s[1].range, Some((0, 99)));
        assert_eq!(s[2].range, Some((100, 149)));
        assert!(parse_media("#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"k\"\n", &base()).is_err());
    }

    #[test]
    fn decrypts_with_sequence_number_iv() {
        let key = [7u8; 16];
        let iv = u128::from(3u64).to_be_bytes();
        let mut buf = b"piece of video".to_vec();
        let len = buf.len();
        buf.resize(len + 16, 0);
        let encrypted = cbc::Encryptor::<aes::Aes128>::new(&key.into(), &iv.into())
            .encrypt_padded::<Pkcs7>(&mut buf, len)
            .unwrap()
            .to_vec();

        let segment = Segment {
            url: base(),
            range: None,
            key: Some(Key {
                url: base(),
                iv: None,
            }),
            sequence: 3,
        };
        let mut data = encrypted;
        let k = segment.key.as_ref().unwrap();
        let iv =
            k.iv.unwrap_or_else(|| u128::from(segment.sequence).to_be_bytes());
        let plain = Decryptor::new(&key.into(), &iv.into())
            .decrypt_padded::<Pkcs7>(&mut data)
            .unwrap();
        assert_eq!(plain, b"piece of video");
    }

    #[test]
    fn splits_attributes() {
        let a = attributes(
            r#"METHOD=AES-128,URI="https://k/a,b",IV=0x00000000000000000000000000000001"#,
        );
        assert_eq!(a["URI"], "https://k/a,b");
        assert_eq!(parse_iv(&a["IV"]).unwrap()[15], 1);
    }
}
