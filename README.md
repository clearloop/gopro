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

**1. Paste a bearer token from the browser (recommended)**

1. Log in at <https://plus.gopro.com>.
2. Open DevTools → Network, click anything that loads media.
3. Pick a request to `api.gopro.com`, copy the `Authorization: Bearer …` value.

```sh
gopro-dl token --access-token 'eyJhbGciOi…'
gopro-dl whoami          # confirm it works
```

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

Between runs, `stats` answers the same question without touching the network:

```
$ gopro-dl stats --dest /Volumes/Archive/GoPro
Archive:   /Volumes/Archive/GoPro
Items:     1204/5231 archived (23.0%)
Remaining: 4027
Files:     1310
Size:      842.1 GiB
Last sync: 2026-09-07 05:11:05 UTC
Partial:   3 interrupted transfer(s), 4.2 GiB already fetched — `sync` resumes them

By year:
  2021       612 files   402.8 GiB
  2022       698 files   439.3 GiB
```

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

The GoPro cloud API is undocumented and unofficial. Endpoints, field names and
the OAuth client credentials can change without notice; every response field is
parsed leniently so a shape change degrades rather than crashes, but a breaking
change on GoPro's side will need the code updated. This tool only reads media
from your own account — it never deletes anything, locally or remotely.

## Development

```sh
cargo test      # unit tests + resume/atomicity tests against a local HTTP origin
cargo clippy --all-targets
```
