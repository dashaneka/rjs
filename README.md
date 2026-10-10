# rjs

A small, fast downloader for **TikTok, Instagram, Facebook and X**. One file, about 4 MB, no Python, no yt-dlp.

```bash
rjs "https://www.tiktok.com/@scout2015/video/6718335390845095173"
```

## What it can grab

- **X**: videos, GIFs, photos
- **TikTok**: videos, photo slideshows, profile pictures, whole profiles
- **Instagram**: posts, reels, carousels (stories and highlights need your login)
- **Facebook**: videos and reels
- **Adult sites (18+)**: Pornhub, YouPorn, RedTube, Eporner, xHamster, XVideos, XNXX, SpankBang

It downloads in parallel, picks up where it left off if it stops, and skips files you already have.

## Install

You need [Rust](https://rustup.rs) (1.85 or newer) and a C compiler (`gcc` or `clang`, most systems have one).

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
rjs -o ~/Videos "<link>"              # save somewhere else
rjs -l 20 "https://www.tiktok.com/@name"   # last 20 posts of a profile
rjs -a "https://www.tiktok.com/@name"      # just the profile picture
rjs -b chrome "<instagram story link>"     # use your browser login
```

Put links in quotes, so `&` in them doesn't confuse your terminal.

Logins work with Chrome, Chromium, Brave, Edge and Firefox (`-b`), or a `cookies.txt` file (`--cookies`).

## Use it in your own code

The downloader lives in `crates/core` as the `rjs-core` library, so other Rust programs can use it too.

## Good to know

Only download things you have the right to. Sites change often; if something breaks, open an issue with the link.

SpankBang often sits behind a Cloudflare check that rjs can't pass, and some countries block certain adult sites outright.

MIT license.
