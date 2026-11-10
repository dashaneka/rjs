# rjs

A Rust downloader for **TikTok, Instagram, Facebook, X and Pinterest**, plus several adult video sites. The executable does not require Python or yt-dlp.

```bash
rjs "https://www.tiktok.com/@scout2015/video/6718335390845095173"
```

## What it can grab

- **X**: videos, GIFs, photos
- **TikTok**: videos, photo slideshows, profile pictures, whole profiles
- **Instagram**: posts, reels, carousels (stories and highlights need your login)
- **Facebook**: videos and reels
- **Pinterest**: pictures, GIFs, videos, carousels, idea pins, whole boards and profiles, `pin.it` links
- **Adult sites (18+)**: Pornhub, YouPorn, RedTube, Eporner, xHamster, XVideos, XNXX, SpankBang

It uses parallel ranges when a server provides a strong validator, safely resumes matching partial ranges, and skips completed files. When the source cannot be pinned to one version, it uses a complete response instead of combining potentially different files.

## Install

You need current stable [Rust](https://rustup.rs) and a C compiler (`gcc` or `clang`, most systems have one).

```bash
cargo install --git https://github.com/dashaneka/rjs rjs
```

Or build it yourself:

```bash
git clone https://github.com/dashaneka/rjs
cd rjs
cargo build --release
# the app is now at target/release/rjs
```

Optional: install [ffmpeg](https://ffmpeg.org). It joins Instagram's separate video and sound, and turns streamed videos (like xHamster) into a normal `.mp4`. Without it you get a `.ts` file, which most players still open.

## Use it

```bash
rjs "<link>"                          # download a post
rjs -i "<link>"                       # just show what's there
rjs -f 720 "<link>"                   # pick a quality
rjs -f best-resolution "<link>"       # prioritize resolution while retaining audio
rjs -f compatible "<link>"            # prefer broadly compatible codecs (default)
rjs -f worst "<link>"                 # lowest available video resolution
rjs -f audio "<link>"                 # an audio-only source stream, when available
rjs --dump-json "<link>"              # JSON metadata without downloading
rjs --batch-file links.txt --quiet    # download links listed one per line
rjs -o ~/Videos "<link>"              # save somewhere else
rjs -l 20 "https://www.tiktok.com/@name"   # last 20 posts of a profile
rjs -a "https://www.tiktok.com/@name"      # just the profile picture
rjs "https://www.pinterest.com/name/board/" # a whole Pinterest board
rjs -b chrome "<instagram story link>"     # use your browser login
```

Put links in quotes, so `&` in them doesn't confuse your terminal.

`best` retains the default compatibility preference. `audio` does not convert a muxed video into audio; it requires an audio-only source stream. Batch files ignore empty lines and lines beginning with `#`. Quiet mode keeps errors on stderr, and failures produce a nonzero exit status. The library's `Media::select_with_max_size` selects only formats with known sizes, including both streams for a merge; it is an estimate, not a transfer byte limit.

Logins work with Chrome, Chromium, Brave, Edge and Firefox (`-b`), or a `cookies.txt` file (`--cookies`).

## Use it in your own code

The downloader lives in `crates/core` as the `rjs-core` library. Configure a shared client and pass a cancellation token to cancel pending work:

```rust,no_run
use std::time::Duration;
use rjs_core::{Client, CancellationToken, DownloadOptions, NoProgress};

# async fn run() -> rjs_core::Result<()> {
let client = Client::builder()
    .connect_timeout(Duration::from_secs(15))
    .read_timeout(Duration::from_secs(30))
    .user_agent("my-app/1.0")
    // .proxy("http://localhost:8080")
    .build()?;
let cancel = CancellationToken::new();
for media in rjs_core::extract_with_cancel(&client, "https://x.com/user/status/123", &cancel).await? {
    rjs_core::download_media_with_cancel(
        &client, &media, ".".as_ref(), &DownloadOptions::default(), &NoProgress, &cancel
    ).await?;
}
# Ok(())
# }
```

Call `cancel.cancel()` from another task to stop pending extraction or download work. A cancelled download may leave partial files for resumption. The original `extract` and `download_media` APIs remain available. For error handling, `Error::kind()` provides an `ErrorKind`, and `Error::is_retryable()` indicates whether a bounded retry may help.

## testing and benchmarks

Run `cargo test --workspace` for local tests. Extractor tests exercise embedded sample responses and parsing for the supported site families; they are not evidence that every site currently works live. Access, login requirements, site responses, formats and availability can change. Use actual links and credentials appropriate to your environment to verify live behavior.

The optional standard-library benchmark runner compares paired runs with yt-dlp. See [benchmark instructions](benchmarks/README.md). It records failures and media details as well as timing; no general speed claim follows from a single link or test environment.

See the [measured results and coverage limits](benchmarks/VERIFICATION.md) for the current four-post comparison and the local verification details.

The [remaining-site investigation](benchmarks/REMAINING.md) records the unsuccessful cases separately, plus a local corpus runner and a tested xhamster parsing fix. Attempting a platform does not count as proving its downloads work.

## Good to know

Only download things you have the right to. Sites change often; if something breaks, open an issue with the link.

If your network blocks XVideos or XNXX, rjs connects without telling the network which site it's visiting, so it usually still works. SpankBang often sits behind a Cloudflare check that rjs can't pass.

MIT license.
