# gopro-dl

Bulk-archive a GoPro cloud media library (GoPro Plus / Premium) to local storage
before the subscription lapses.

Built for one job: get **everything** off GoPro's servers and onto an external
disk, resumably, without babysitting it.

- **Incremental** — a manifest under `<dest>/.gopro-dl/` records every completed
  file, so re-running `sync` only fetches what is missing.
- **Resumable** — downloads use HTTP `Range` against a `.part` file and are only
  renamed into place after `fsync`, so a yanked USB cable or `Ctrl-C` costs you
  at most one partial file.
- **Honest about failure** — failed items are listed at the end and retried on
  the next run rather than silently skipped.
- **Handles the awkward cases** — chaptered videos and burst groups (multiple
  files per media item), duplicate camera filenames across years, media that
  only exists as a cloud transcode.

## Install

```sh
cargo build --release
# binary at ./target/release/gopro-dl
```

## Authenticate

GoPro has no public API. Two ways in — the first is the one that reliably works:

**1. Paste a token from the browser (recommended)**

GoPro's web app authenticates with a **`gp_access_token` cookie**, not an
`Authorization` header, so that cookie's value is what you want:

1. Log in at <https://plus.gopro.com>.
2. Open DevTools → **Application** (Chrome) or **Storage** (Firefox/Safari) →
   **Cookies** → `https://plus.gopro.com`.
3. Copy the **Value** of `gp_access_token`.

The client sends the token as both a `Bearer` header and a `gp_access_token`
cookie, so it works whichever way an endpoint expects it. If you happen to find
an `Authorization: Bearer …` header in the Network tab instead, that works too.

```sh
gopro-dl token           # prompts, so the token stays out of shell history
gopro-dl whoami          # confirm it works — prints your account
```

Paste any of these; the token is extracted from all of them:

- the bare value, `eyJhbGciOi…`
- a cookie pair, `gp_access_token=eyJhbGciOi…`
- a whole cookie jar, `_ga=…; gp_access_token=eyJ…; gp_user=…`
- a header line, `Authorization: Bearer eyJhbGciOi…`

`--access-token` and `GOPRO_ACCESS_TOKEN` also work if you are scripting it.

Tokens are short-lived (hours). If a long run dies with a 401, paste a fresh one
and re-run `sync` — it resumes.

**2. Password grant (best effort)**

```sh
gopro-dl login --email you@example.com
```

This uses the GoPro web client's OAuth credentials and yields a refresh token,
so long runs survive token expiry. GoPro rotates those credentials and gates the
endpoint behind captchas and 2FA, so it may fail with `invalid_client` — fall
back to method 1, or override with `GOPRO_CLIENT_ID` / `GOPRO_CLIENT_SECRET`.

Credentials are stored `0600` at `~/.config/gopro-dl/credentials.json`.
`GOPRO_ACCESS_TOKEN` in the environment overrides the file.

## What actually gets downloaded

Verified against a live account, because this part is counter-intuitive:

`GET /media/{id}/download` returns two lists, and **`_embedded.files` is not the
original**. For a processed video it points at the 720p `edit_proxy` — same
pixel dimensions in the metadata, ~20x smaller on the wire. The camera original
is in `_embedded.variations` under the label `source` (or `baked_source` for a
MultiClipEdit). Preferring `files` yields an archive that looks complete and is
useless.

`gopro-dl` resolves originals from the labelled variations, falls back to
`files` only for items GoPro has not transcoded yet, and cross-checks the
downloaded size against the `file_size` GoPro reports for the item — so this
class of mistake shows up as a warning rather than a silent 20x shortfall.

Items still uploading or transcoding are skipped (their signed URLs 403); they
are not recorded, so a later run collects them. `--include-unprocessed` overrides.

## Archive

```sh
# See what's there and how big it is, without downloading
gopro-dl sync --dest /Volumes/Archive/GoPro --dry-run

# Do it
gopro-dl sync --dest /Volumes/Archive/GoPro --jobs 4

# Later, or after an interrupt — picks up where it left off
gopro-dl sync --dest /Volumes/Archive/GoPro

# Confirm the archive is intact
gopro-dl verify --dest /Volumes/Archive/GoPro
gopro-dl stats  --dest /Volumes/Archive/GoPro
```

### Which renditions you get

| `--variant` | Behaviour |
| --- | --- |
| `source` | Camera originals only. |
| `best` *(default)* | Originals, falling back to the highest-resolution cloud transcode for items that expose no original. |
| `all` | Everything, including low-res proxies. |

Add `--sidecars` to also pull LRV/THM/telemetry companions when GoPro offers
them, and `--metadata` to drop a `.gopro.json` of the API metadata next to each
item.

### Layout

`--layout date` (default) gives `2023/2023-07-14/GX010123.MP4`; `year` gives
`2023/GX010123.MP4`; `flat` puts everything in one directory. Filenames are
sanitised for exFAT/NTFS, and two items that share a camera filename are
disambiguated with a short media id rather than overwriting each other.

## Progress and status

While a sync runs you get a live two-line display, plus one log line per
completed file:

```
[==============>                 ] 812/5231 items · GX010423.MP4
[============>                   ] 214.7 GiB/~842.1 GiB · 38.2 MiB/s · ETA 4h31m
 INFO [813/5231 items · 951 files] 2021/2021-03-02/GX010423.MP4 (2.1 GiB)
```

The byte total is an estimate from the sizes GoPro reports in the library
listing, so the ETA is a guide, not a promise. Redirecting stderr (`2> sync.log`)
gives you a durable x/y record of exactly what landed and when.

Between runs, `stats` answers the same question offline — in items **and**
bytes, which is what matters when clips range from 50 MB to 2.3 GB:

```
$ gopro-dl stats --dest /Volumes/Archive/GoPro
Archive:   /Volumes/Archive/GoPro
Items:     1204/2838 archived (42.4%)
Size:      412.3 GiB of ~948.9 GiB (43.4%)
Remaining: 1634 items, ~536.6 GiB to fetch
Files:     1310 on disk
Last sync: 2026-09-07 05:11:05 UTC
Partial:   3 interrupted transfer(s) holding 4.2 GiB — `sync` resumes them

By year:
  2021       612 files    402.8 GiB
  2022       698 files    439.3 GiB
```

The cloud totals come from `GET /media/user` and are cached in the manifest, so
`stats` needs no network. `stats --refresh` re-reads them. `whoami` shows the
same totals straight from the account:

```
$ gopro-dl whoami
GoPro cloud account
  account id:   2892bb04-…
  member since: 2023-11-10T09:36:19Z
  media items:  2838
  cloud size:   948.9 GiB
```

## The listing cache

Walking `/media/search` costs one request per 100 items — 29 round trips for a
2835-item library, about 30 seconds — and an interrupted run would otherwise pay
that again before it could resume anything.

The listing is cached in `<dest>/.gopro-dl/library.json` and validated on each
run against `GET /media/user`, a single request returning the account's item
count and byte total. If both are unchanged, nothing was added or removed and
the cached listing still describes the library:

```
$ gopro-dl sync --dest /Volumes/Archive/GoPro --dry-run
 INFO using cached listing: 2835 items, unchanged since 0m ago (--refresh-list to re-walk)
```

That is a real validator, not just a timer, so the cache is dropped as soon as
the library actually changes. It is also discarded when the totals move, when the
credentials point at a different account, after `--cache-ttl` hours (default 24),
and within an hour when the listing contained items that were still uploading —
those become downloadable without moving either total, so age is the only signal.
`--refresh-list` forces a re-walk.

Measured on a 2835-item library: **28.1s → 0.86s**, same result.

## When things go wrong

The tool distinguishes three kinds of failure, because they need different
responses over a run that lasts hours.

**Fatal — stop immediately.** A permission error, a full volume, or a read-only
mount will fail identically on every one of 2835 items. These abort the run at
once with a non-zero exit and an explanation, rather than retrying each item
five times with backoff (which would burn hours arriving at the same place).

Destination problems are caught *before* the library listing, so you find out in
about a second:

```
$ gopro-dl sync --dest /Volumes/MyDisk/GoPro
Error: cannot write to /Volumes/MyDisk/GoPro/.gopro-dl/parts: Operation not permitted

macOS blocks programs from writing to removable volumes until you allow it.
Open System Settings -> Privacy & Security -> Files and Folders, find your
terminal app, and enable "Removable Volumes" (Full Disk Access also works).
Then re-run the same command.
```

**Systemic — give up early.** If 12 items fail back to back, the problem is not
per-item. The run stops and says so instead of working through the whole
library to produce 2835 identical errors.

**Per-item — retry, then carry on.** Network blips, 5xx, 429 and expired signed
URLs are retried with exponential backoff (`--retries`, default 5); expired CDN
links are re-minted from the API first. An item that still fails is logged, the
run continues, and it is listed at the end:

```
3 item(s) failed — re-run `sync` to retry them:
  XNNdogNowlX2r: GET https://…/source/default/1.mp4 -> 503 Service Unavailable
```

Failed items are never recorded in the manifest, so simply re-running `sync`
retries exactly those and skips everything already done.

Log lines are routed through the progress display, so warnings appear whole on
their own line instead of being spliced into a half-drawn bar.

## Stopping and restarting

Press `Ctrl-C` whenever you like. The run stops at the next chunk boundary —
not at the next file — flushes the manifest, and tells you what it held onto:

```
Stopped on interrupt. 2 partial transfer(s) held at 3.1 GiB; re-run the same
`sync` command to resume exactly where this left off.
```

A second `Ctrl-C` quits immediately, and that is also safe.

Re-running the same command resumes. Concretely:

- **A file that finished** is skipped — but only after checking it is still on
  disk at the recorded size, so a disk that got wiped or half-restored is
  re-fetched rather than assumed good.
- **A file that was mid-transfer** resumes by byte offset via HTTP `Range`.
  Partials are staged in `<dest>/.gopro-dl/parts/` keyed by media id, never
  beside the final file, so an interrupted `GOPR0001.JPG` can never be resumed
  into a *different* item that happens to share that camera filename.
- **A file that was never started** is simply downloaded.
- **Names stay put.** Collision suffixes are seeded from the manifest and the
  plan for an item is made once, so a restart — or a retry after a network blip
  — writes to the same paths as the first attempt, never a second copy under a
  different name.
- **Nothing partial is ever published.** Files are only renamed into the archive
  after the full length arrives and `fsync` returns, so anything you can see in
  the archive tree is complete.

Killing the process outright (`kill -9`, power loss, yanked disk) is the same
story, minus the tidy summary: the staged partial holds whatever reached the
disk, and the next run resumes from there.

### Useful flags

```
--since 2019-01-01 --until 2021-01-01   # date window (capture time)
--limit 50                              # first N items, for a trial run
--hash                                  # record SHA-256, enables `verify --hash`
--newest-first                          # default is oldest first
--include-unprocessed                   # try items GoPro is still processing
--jobs N                                # concurrent items (default 4)
--retries N                             # attempts per operation (default 5)
```

## Doing a large archive well

1. `--dry-run` first to see the item count and rough size, and check the disk
   has room.
2. Start with `--limit 20` to confirm the layout is what you want — the manifest
   makes the real run skip whatever the trial already fetched.
3. Run the real sync. Interrupt it whenever; re-running resumes.
4. `verify` at the end. Anything reported as `MISSING` / `SIZE` can be deleted
   and refetched by another `sync`.
5. Keep `<dest>/.gopro-dl/manifest.json` with the archive — it is the record of
   what came from where.

## Caveats

The GoPro cloud API is undocumented and unofficial. The endpoints and field
semantics here were established by probing a real account (note `GET /media/user`
for the account summary — `/v1/user` 404s), but GoPro can change any of it
without notice. Every response field is parsed leniently so a shape change
degrades rather than crashes.

The size cross-check is the safety net worth knowing about: if GoPro reorganises
which list holds the original, you get a warning per file rather than a quietly
worthless archive.

This tool only reads media from your own account — it never deletes anything,
locally or remotely.

## Development

```sh
cargo test      # unit tests + resume/atomicity tests against a local HTTP origin
cargo clippy --all-targets
```
