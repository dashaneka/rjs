# metadata corpus comparisons

create a local json array containing entries such as `[{"platform":"example","url":"https://example.com/post/123"}]`. urls can contain private or signed data; keep the manifest private. run:

```sh
python benchmarks/coverage.py manifest.json --rjs ./target/release/rjs --yt-dlp yt-dlp --jobs 3 --timeout 60 --output coverage.json
python -m unittest discover -s benchmarks -p test_coverage.py -v
```

`--jobs` is limited to 1–3 concurrent entries; each entry invokes the two tools sequentially. commands accept shell-style quoting but never run through a shell. the runner does not save commands, urls, titles, descriptions, or subprocess diagnostics. it retains sanitized ids, duration, format counts, dimensions, codecs, sizes and bitrates. the input urls are passed to the tools as command-line arguments. failure, timeout, malformed json and missing expected metadata remain individual results and do not count as metadata successes. a zero exit from the runner means the corpus run completed, not that every entry succeeded.

selection is only a candidate when a single rjs media item and a direct http format from each tool have the same scheme, host, port and path, ignoring query strings. the smallest known size is preferred, then known bitrate, then unknown size. this is a heuristic: matching paths can still serve different content, and requests may require different headers or credentials. unsupported means no candidate was identified, not necessarily that either tool cannot download the item. metadata success and candidate selection never establish download success. use the explicit selectors with `benchmarks/compare.py` and inspect the actual downloaded files and hashes to establish same-file equivalence. the corpus report always records zero verified downloads because it performs none.

use `--private-diagnostics-dir /tmp/coverage-debug` only for local debugging. it saves raw stdout and stderr by zero-based manifest index and tool, including sensitive urls and other private content. the public report contains only broad failure categories; they are heuristic and do not diagnose every failure. timestamps, available tool versions, and the installed `curl_cffi` version (or null) are recorded as environment context.

## reproduce the remaining fixture checks locally

the versioned reference file names upstream test classes and indices rather than publishing a list of adult-content links. the resolver reads literal urls from the installed extractor source using python's ast parser; it does not execute extractor test expressions. the fixtures can still be unavailable, deleted, or restricted. they are inputs for testing, not a guarantee of working samples.

from the repository root, in a python virtual environment:

```sh
python -m pip install 'yt-dlp==2026.10.26' curl-cffi
cargo build --release --locked
python benchmarks/fixture_corpus.py --output benchmarks/local-remaining.json
python benchmarks/coverage.py benchmarks/local-remaining.json \
  --rjs ./target/release/rjs --yt-dlp 'python -m yt_dlp' \
  --jobs 3 --timeout 60 --output benchmarks/local-coverage.json
```

the local manifests and reports are gitignored. use your own public, currently playable links in a manifest when a fixture is unavailable. this workflow does not add credentials or solve access checks. do not disable certificate verification to make a comparison pass. for private inputs, inspect the sanitized report before sharing; never share raw diagnostic files or cookies.

see [the remaining-site investigation](REMAINING.md) for the recorded results and the distinction between attempted and successfully verified coverage.
