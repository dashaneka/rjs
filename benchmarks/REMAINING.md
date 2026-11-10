# remaining-site investigation — 2026-11-10

the earlier four-post benchmark is unchanged. this pass attempted the remaining nine supported platform labels, using two distinct, exact, unskipped upstream fixture urls per label. the pinned provenance is in [remaining-fixtures.json](remaining-fixtures.json). a resolver can generate the actual input manifest locally without publishing adult-content titles or urls here.

## measured outcome

| metric | result |
| --- | --- |
| platform labels with a comparison attempt, including the earlier pass | 13/13 — 100% |
| newly attempted paired metadata cases | 18 |
| new usable metadata pairs | 0/18 |
| new matched-file download comparisons | 0 |
| platform labels with successful matched-file comparisons | 4/13 — 30.8% |

attempted coverage is not successful download coverage. none of the failures is counted as a speed win, and the wider goal of outperforming other downloaders on every supported site remains unproven.

## observations and limits

- both tools failed to produce usable metadata for all eighteen exact fixture cases. the sanitized, per-tool outcomes and fixture provenance are in [the report](results/2026-11-10-remaining.json). private stdout and stderr were retained only locally for diagnosis.
- the yt-dlp baseline included its optional curl-cffi transport. some cases still reported certificate failures, which make those cases unsuitable for a performance conclusion. certificate verification was not disabled.
- three saved public homepage responses — pornhub, youporn and redtube — were identical 195-byte, http 200 pages saying “site unavailable” and “unable to access this site.” their matching body hash is recorded. those responses did not expose testable media.
- eporner, xhamster and spankbang homepage checks reported http 200, 195 bytes and no public video anchors, but did not retain the bodies. xvideos and xnxx checks also reported no matching video anchors without retaining their bodies. the exact cause of those responses cannot be established from the saved evidence; it would be incorrect to claim every site returned the identical page.
- the facebook pages inspected exposed no playable video data. no suggested video was substituted for the requested post, and no login, challenge or access gate was bypassed.
- preliminary discovery included old, modified or deactivated fixture inputs and repeated requests. those probes were not used as platform-wide evidence or successful coverage. this report uses the exact versioned corpus for its eighteen-case count and preserves its unsuccessful outcomes.

the tool defaults are not identical transports: rjs already has site-specific tls routing and tiktok check-page handling; yt-dlp may choose curl-cffi. this pass did not add or enhance those mechanisms. metadata extraction is not proof of an intact downloaded file. the next valid step is to run the corpus on an environment that exposes playable public media, then compare the same formats and verify output hashes with compare.py. [local instructions](COVERAGE.md) produce a sanitized report without sharing raw diagnostics.

## code and verification

the audit found a reproducible xhamster parsing bug: valid window.initials json was rejected when additional script statements followed it. streaming json parsing now reads the object without swallowing trailing javascript, including nested content and semicolons in strings. a synthetic regression test covers the case. this fix is unit-tested; unavailable live content prevented a live download assertion.

the new corpus runner validates nonempty media ids and usable format urls, keeps metadata and verified-download counts separate, records failure categories and optional transport versions, and can save private diagnostics explicitly. independent review caught and corrected acceptance of empty format lists and overly loose origin matching. fixture resolution is version-pinned and uses ast parsing rather than evaluating upstream source.

verification after the change: 89 core tests, 3 cli tests, 1 rust documentation test, and 9 benchmark-tool tests passed. formatting, strict clippy and the release build also passed. the fixture resolver reproduced all eighteen tested input hashes exactly.
