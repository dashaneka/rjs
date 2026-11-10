# paired benchmark

`compare.py` uses python 3's standard library. build rjs in release mode and install yt-dlp separately. inspect both tools' format listings first and choose selectors that resolve to the same media (including audio, codec, resolution and protocol). ids are tool-specific; identical names do not establish equivalence. use a single post, not a profile or playlist.

```sh
cargo build --release
./target/release/rjs --info 'URL'
yt-dlp --no-config --list-formats 'URL'
python benchmarks/compare.py 'URL' --rjs ./target/release/rjs \
  --rjs-format 'RJS_ID' --yt-format 'YTDLP_ID' \
  --mode download --connections 1 --repeats 5 --output results.json
python -m unittest discover -s benchmarks -v
```

`--yt-dlp 'python -m yt_dlp'` selects a module invocation. `--mode metadata` times metadata extraction without downloads. every measured invocation uses a fresh process and an empty output directory; order alternates between tools. `--cache-mode cold` means no runner warmup. `--cache-mode warm` runs one unmeasured invocation of each tool first. neither mode clears os, dns, server or network caches, and the warmup does not preserve process state. record the machine, network, date, link and any authentication or proxy settings when publishing results. the runner records both tool versions.

The report includes preflight metadata and format lists, per-run commands, elapsed wall time, exit status, stderr, file sizes, sha256 hashes and medians of successful runs. when ffprobe is available, it also records stream dimensions and codecs and requires each output to be readable as media. without ffprobe, nonempty output alone does not prove a valid media file. the report warns about detectable differences in selected metadata and downloaded video streams, but missing metadata and compatible-looking fields do not prove equivalence. a download must exit successfully and produce nonempty files without `.part` or `.ytdl` files to count as successful. metadata mode requires valid json. failures remain visible and make the runner exit nonzero; do not compare medians without also considering success rates. equal byte counts alone do not prove content equivalence. url-named metadata fields are redacted and query strings are stripped from urls elsewhere in the report. diagnostics can still contain other sensitive data, so review the report before sharing.

The runner passes the same numerical connection setting, but the flags have different meanings: rjs uses parallel ranges where applicable, while yt-dlp's `--concurrent-fragments` applies to fragmented streams. use one connection for ordinary http comparisons and inspect the selected protocol. tools may still use different request patterns, extractors, remuxing, or caching. the benchmark does not guarantee equal work or establish a general performance ranking. a metadata run extracts each tool's available information; the rjs selector affects downloads, while the yt-dlp selector also affects its metadata output.
