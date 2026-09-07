# gopro-dl

Bulk-download a GoPro cloud (GoPro Plus / Premium) media library to local disk.
Resumable, incremental, safe to interrupt.

```sh
cargo build --release
```

## Authenticate

GoPro's web app authenticates with a `gp_access_token` cookie, not an
`Authorization` header. Log in at <https://plus.gopro.com>, then DevTools →
Application/Storage → Cookies → copy the value of `gp_access_token`.

```sh
gopro-dl token      # prompts; accepts the raw value, a cookie pair,
                    # a whole cookie jar, or an Authorization header
gopro-dl whoami     # confirms it works
```

Tokens last a few hours. If a long run dies with a 401, paste a fresh one and
re-run — it resumes. `gopro-dl login --email you@example.com` tries the OAuth
password grant instead, which yields a refresh token but is often blocked by
GoPro.

## Use

```sh
gopro-dl sync --dest /Volumes/Disk/GoPro --dry-run   # what and how big
gopro-dl sync --dest /Volumes/Disk/GoPro --jobs 4    # download
gopro-dl sync --dest /Volumes/Disk/GoPro             # resumes; skips what it has
gopro-dl stats  --dest /Volumes/Disk/GoPro           # progress, in items and bytes
gopro-dl verify --dest /Volumes/Disk/GoPro           # check files against the manifest
```

Useful flags:

```
--variant source|best|all   originals only / originals with fallback (default) / everything
--layout date|year|flat     2023/2023-07-14/GX010123.MP4 (default) | 2023/… | flat
--since / --until           YYYY-MM-DD window on capture time
--limit N                   N items of outstanding work, for a trial run
--jobs N                    concurrent items (default 4)
--sidecars / --metadata     LRV/THM/telemetry companions / a .json of the API metadata
--include-unprocessed       try items GoPro is still transcoding (they normally 403)
--refresh-list              re-walk the listing instead of using the cache
--hash                      record SHA-256, enabling `verify --hash`
```

## Notes

**Originals, not proxies.** `_embedded.files` is not the camera original for a
processed item — it is the 720p `edit_proxy`, carrying the original's dimensions
in its metadata at ~1/20th the size. The original is in `_embedded.variations`
under `source` (`baked_source` for a MultiClipEdit). Every original is
cross-checked against the `file_size` GoPro reports, so a mismatch warns instead
of silently filling the archive with proxies.

**Resuming.** State lives in `<dest>/.gopro-dl/`. Completed items are skipped
without any API call, after confirming the files are still on disk at their
recorded size. Partial transfers are staged under `.gopro-dl/parts/`, keyed by
media id, and resume by byte offset; nothing is published into the archive until
it is complete and `fsync`ed. `Ctrl-C` stops at the next chunk and keeps the
partial; a second one quits immediately.

**Listing cache.** The library listing is cached and validated against the
account totals from `/media/user`; unchanged totals mean the library did not
change. Dropped when the totals move, on a different account, after
`--cache-ttl` hours (default 24), and within an hour when the listing held items
that were still processing.

**Failures.** Permission denied, a full volume and a read-only mount abort
immediately rather than retrying across the whole library; destination
writability is checked before the listing. Twelve consecutive item failures stop
the run. Everything else retries with backoff, is reported at the end, and is
retried by the next `sync`.

**Unofficial API.** Endpoints and field semantics were established by probing a
live account and can change without notice. Responses are parsed leniently. This
tool only reads; it never deletes anything, locally or remotely.

## Development

```sh
cargo test                 # unit tests, plus end-to-end tests against a fake API
cargo clippy --all-targets
```
