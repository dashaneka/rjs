# verification and measured results — 2026-11-10

this is evidence of specific improvements, not proof that rjs beats every downloader on every supported link.

the paired tests used yt-dlp 2026.10.26, checked against its [latest stable release](https://github.com/yt-dlp/yt-dlp/releases/tag/2026.10.26), and an optimized rjs 0.1.0 working-tree candidate. the tested executable was built during this change. later fixes to range-probe retries and default small-file selection were covered by local tests; these comparisons explicitly used the same single-connection transfer path. both tools ran in the same shared linux x86_64 environment, with alternating order, fresh processes and output folders, one connection, and three repetitions. timings include startup, extraction and downloading, rather than isolated network transfer speed. there was no runner warmup; os, dns, cdn and network caches were not cleared. the tools can fetch different amounts of metadata internally.

| site | actual video size | rjs median | yt-dlp median | lower median wall time | successes |
| --- | --- | --- | --- | --- | --- |
| tiktok | 720 × 1280 | 14.84 s | 19.34 s | 23.3% | 3/3 each |
| x | 1280 × 720 | 16.34 s | 29.95 s | 45.4% | 3/3 each |
| pinterest | 1080 × 1920 | 13.24 s | 19.55 s | 32.3% | 3/3 each |
| instagram | 480 × 854 | 14.35 s | 26.73 s | 46.3% | 3/3 each |

every measured download succeeded. for each post, all six output files had exactly the same sha-256 hash and byte count. ffprobe confirmed matching stream properties. the instagram source is a silent video; the comparison does not establish audio-merge performance. metadata warnings about x's post-versus-video id and missing codec/dimension fields are retained in the report; identical downloaded hashes resolve content equivalence for these samples. x used a common 720p format, not its largest available format. pinterest's source calls the selected format `720p`/`V_720P`, but the actual file is 1080 × 1920.

these are small samples in one network environment, not a statistically representative site-wide ranking. the [machine-readable report](results/2026-11-10.json) contains timings, selected formats, hashes and probe results without signed media urls or post captions. rerun the [benchmark instructions](README.md) on your own corpus and network.

## implemented improvements

- strict byte-range, content length and representation checks; bounded transient retries; fallback for servers that ignore ranges.
- strong-etag resume manifests bind partial files to the source url, version, total size and partition count. files without a trustworthy validator use one complete response. fresh single-connection and known small files avoid an extra probe request.
- ffmpeg conversion stages its output and only exposes the final filename after success. cancellation stops pending operations and kills ffmpeg children; partial work may remain, without being marked complete.
- configurable shared clients, cancellation tokens, error categories and retryability inspection for rust applications.
- explicit resolution/compatibility/audio selectors and a known-size selection helper that preserves video and accounts for merged audio sizes.
- json metadata, quiet mode, batch input, clear failure exit status, and tests for command-line automation.
- facebook avoids returning a suggested video for a missing requested id; x photo urls with query strings preserve the correct extension; tiktok excludes known unsupported/video-only playback variants.

## verification and limits

local verification: 88 core unit tests, 3 cli unit tests, 1 rust doc test, and 3 benchmark-runner tests passed. `cargo fmt --all --check`, strict clippy across all targets, and the release build passed. ci runs the same local checks.

parser fixtures cover the ten extractor modules used by the thirteen platform labels. this is not live-site coverage. live downloads were compared for the four samples above. a separate metadata smoke test succeeded for those four public platforms; both rjs and yt-dlp failed to extract the facebook sample `https://www.facebook.com/radiokicksfm/videos/3676516585958356/`. the cause was not established, so it is neither counted as a success nor described as a proven login requirement. adult-site families were not live-benchmarked in this pass. authenticated/private content was not tested.

rjs does not match yt-dlp's entire feature set. audio selectors currently require audio-only source formats; they do not convert muxed videos. subtitle extraction and broader metadata/postprocessing remain gaps. size selection is an estimate, not a strict transfer byte limit. hls does not yet provide a general external-audio-group selection interface. retry policy is bounded; the range probe supports numeric retry-after with a cap, while other retries use fixed backoff. any claim of universal superiority would require a much broader maintained corpus, more environments, and comparisons with other tools.
