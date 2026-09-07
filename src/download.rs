//! Resumable HTTP range downloads with atomic publish.

use anyhow::{anyhow, bail, Context, Result};
use futures_util::StreamExt;
use indicatif::ProgressBar;
use sha2::{Digest, Sha256};
use std::io::SeekFrom;
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tokio::fs::{self, File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, BufWriter};
use tracing::debug;

/// Signed CDN URLs expire; the caller re-mints them when it sees this.
#[derive(Debug)]
pub struct UrlExpired(pub reqwest::StatusCode);

impl std::fmt::Display for UrlExpired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "signed CDN URL rejected ({})", self.0)
    }
}

impl std::error::Error for UrlExpired {}

/// The user asked to stop. The partial file is intact and resumable.
#[derive(Debug)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("cancelled by user")
    }
}

impl std::error::Error for Cancelled {}

/// Where a transfer reports to, and how it learns it should stop.
#[derive(Default, Clone, Copy)]
pub struct Sink<'a> {
    pub pb: Option<&'a ProgressBar>,
    /// Run-wide byte counter, incremented per chunk as bytes actually arrive.
    pub downloaded: Option<&'a AtomicU64>,
    pub cancel: Option<&'a AtomicBool>,
}

impl Sink<'_> {
    fn stopped(&self) -> bool {
        self.cancel.is_some_and(|c| c.load(Ordering::SeqCst))
    }
}

#[cfg(test)]
fn part_path(dest: &Path) -> PathBuf {
    let mut s = dest.as_os_str().to_os_string();
    s.push(".part");
    PathBuf::from(s)
}

/// Streams `url` into `dest`, staging through `part` and resuming from whatever
/// `part` already holds when the server honours `Range`. Returns the final size.
///
/// `part` is supplied by the caller rather than derived from `dest` so that a
/// partial is bound to the media item it came from, not to a filename.
pub async fn fetch(
    http: &reqwest::Client,
    url: &str,
    dest: &Path,
    part: &Path,
    sink: Sink<'_>,
) -> Result<u64> {
    for dir in [dest.parent(), part.parent()].into_iter().flatten() {
        fs::create_dir_all(dir)
            .await
            .with_context(|| format!("creating {}", dir.display()))?;
    }
    let mut have = fs::metadata(part).await.map(|m| m.len()).unwrap_or(0);
    let mut resume = have > 0;

    loop {
        let mut req = http.get(url);
        if resume {
            req = req.header(reqwest::header::RANGE, format!("bytes={have}-"));
        }
        let resp = req.send().await.with_context(|| format!("GET {}", short(url)))?;
        let status = resp.status();

        // Stale partial (file changed server-side, or we already have it all):
        // start clean rather than splicing mismatched bytes together.
        if status == reqwest::StatusCode::RANGE_NOT_SATISFIABLE && resume {
            let _ = fs::remove_file(part).await;
            have = 0;
            resume = false;
            continue;
        }
        if status == reqwest::StatusCode::FORBIDDEN || status == reqwest::StatusCode::GONE {
            return Err(anyhow!(UrlExpired(status)));
        }
        if !status.is_success() {
            bail!("GET {} -> {status}", short(url));
        }

        let partial = status == reqwest::StatusCode::PARTIAL_CONTENT;
        if !partial {
            have = 0;
        }
        let remaining = resp.content_length();
        let total = remaining.map(|r| r + have);

        if let Some(pb) = sink.pb {
            pb.set_length(total.unwrap_or(0));
            pb.set_position(have);
        }

        let mut file = if partial {
            let mut f = OpenOptions::new()
                .write(true)
                .open(part)
                .await
                .with_context(|| format!("opening {}", part.display()))?;
            f.seek(SeekFrom::Start(have)).await?;
            debug!("resuming {} at {have} bytes", dest.display());
            f
        } else {
            File::create(part)
                .await
                .with_context(|| format!("creating {}", part.display()))?
        };

        let mut writer = BufWriter::with_capacity(1 << 20, &mut file);
        let mut stream = resp.bytes_stream();
        let mut written = have;
        let mut cancelled = false;
        let mut stream_err = None;
        while let Some(chunk) = stream.next().await {
            // Flush what we already hold before propagating: a dropped
            // connection must not also throw away the last megabyte, or a flaky
            // link makes no forward progress across retries.
            let chunk = match chunk {
                Ok(c) => c,
                Err(e) => {
                    stream_err = Some(anyhow::Error::new(e).context("connection dropped mid-transfer"));
                    break;
                }
            };
            writer.write_all(&chunk).await?;
            written += chunk.len() as u64;
            if let Some(pb) = sink.pb {
                pb.set_position(written);
            }
            if let Some(c) = sink.downloaded {
                c.fetch_add(chunk.len() as u64, Ordering::Relaxed);
            }
            // Stop between chunks rather than at file boundaries: a half-written
            // 8 GB clip costs nothing, because the partial survives and resumes.
            if sink.stopped() {
                cancelled = true;
                break;
            }
        }
        writer.flush().await?;
        drop(writer);
        if let Some(e) = stream_err {
            file.sync_all().await?;
            return Err(e);
        }
        if cancelled {
            file.sync_all().await?;
            return Err(anyhow!(Cancelled));
        }
        // Durable before we claim it: an ext-disk yank mid-run must not leave a
        // manifest entry pointing at empty blocks.
        file.sync_all().await?;
        drop(file);

        if let Some(t) = total {
            if written != t {
                bail!(
                    "{}: expected {t} bytes, got {written} — leaving .part for retry",
                    dest.display()
                );
            }
        }

        fs::rename(part, dest)
            .await
            .with_context(|| format!("publishing {}", dest.display()))?;
        return Ok(written);
    }
}

pub async fn sha256_of(path: &Path) -> Result<String> {
    let mut f = File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn short(url: &str) -> &str {
    url.split('?').next().unwrap_or(url)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncBufReadExt;
    use tokio::net::TcpListener;

    /// Minimal HTTP/1.1 origin that understands open-ended `Range` requests,
    /// so we can exercise resume without reaching for a web framework.
    async fn serve(body: &'static [u8]) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((sock, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let (rd, mut wr) = sock.into_split();
                    let mut rd = tokio::io::BufReader::new(rd);
                    let mut start = 0usize;
                    loop {
                        let mut line = String::new();
                        if rd.read_line(&mut line).await.unwrap_or(0) == 0 {
                            return;
                        }
                        let lower = line.to_ascii_lowercase();
                        if let Some(rest) = lower.strip_prefix("range: bytes=") {
                            start = rest
                                .trim()
                                .trim_end_matches('-')
                                .parse()
                                .unwrap_or(0);
                        }
                        if line == "\r\n" {
                            break;
                        }
                    }
                    let total = body.len();
                    let head = if start > 0 {
                        format!(
                            "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\n\
                             Content-Range: bytes {start}-{}/{total}\r\n\r\n",
                            total - start,
                            total - 1
                        )
                    } else {
                        format!("HTTP/1.1 200 OK\r\nContent-Length: {total}\r\n\r\n")
                    };
                    let _ = wr.write_all(head.as_bytes()).await;
                    let _ = wr.write_all(&body[start..]).await;
                });
            }
        });
        format!("http://{addr}/GX010123.MP4")
    }

    const BODY: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";

    #[tokio::test]
    async fn downloads_whole_file_and_publishes_atomically() {
        let url = serve(BODY).await;
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("2023/2023-07-14/GX010123.MP4");

        let part = part_path(&dest);
        let n = fetch(&reqwest::Client::new(), &url, &dest, &part, Sink::default())
            .await
            .unwrap();

        assert_eq!(n, BODY.len() as u64);
        assert_eq!(std::fs::read(&dest).unwrap(), BODY);
        assert!(!part.exists(), ".part must be gone once published");
    }

    #[tokio::test]
    async fn resumes_from_an_existing_partial() {
        let url = serve(BODY).await;
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("GX010123.MP4");
        let part = part_path(&dest);
        std::fs::write(&part, &BODY[..10]).unwrap();

        let n = fetch(&reqwest::Client::new(), &url, &dest, &part, Sink::default())
            .await
            .unwrap();

        assert_eq!(n, BODY.len() as u64);
        assert_eq!(std::fs::read(&dest).unwrap(), BODY);
    }

    #[tokio::test]
    async fn a_partial_lives_outside_the_archive_tree() {
        // The staging path is the caller's choice, so an interrupted download of
        // one item can never be resumed into a different item's bytes.
        let url = serve(BODY).await;
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("2023/GOPR0001.JPG");
        let part = dir.path().join(".gopro-dl/parts/mediaid__source_0.part");

        fetch(&reqwest::Client::new(), &url, &dest, &part, Sink::default())
            .await
            .unwrap();

        assert_eq!(std::fs::read(&dest).unwrap(), BODY);
        assert!(!dir.path().join("2023/GOPR0001.JPG.part").exists());
    }

    #[tokio::test]
    async fn cancelling_keeps_a_resumable_partial() {
        let url = serve(BODY).await;
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("GX010123.MP4");
        let part = part_path(&dest);

        // Already set: the transfer stops after its first chunk.
        let cancel = AtomicBool::new(true);
        let downloaded = AtomicU64::new(0);
        let err = fetch(
            &reqwest::Client::new(),
            &url,
            &dest,
            &part,
            Sink { pb: None, downloaded: Some(&downloaded), cancel: Some(&cancel) },
        )
        .await
        .unwrap_err();

        assert!(err.downcast_ref::<Cancelled>().is_some(), "got: {err:#}");
        assert!(!dest.exists(), "an incomplete file must never be published");
        assert!(part.exists(), "partial must survive for the next run");

        // Resuming with a fresh flag completes the file.
        let n = fetch(&reqwest::Client::new(), &url, &dest, &part, Sink::default())
            .await
            .unwrap();
        assert_eq!(n, BODY.len() as u64);
        assert_eq!(std::fs::read(&dest).unwrap(), BODY);
    }

    #[tokio::test]
    async fn expired_url_is_reported_as_such() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let _ = sock
                    .write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n")
                    .await;
            }
        });
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("x.MP4");
        let err = fetch(
            &reqwest::Client::new(),
            &format!("http://{addr}/x.MP4"),
            &dest,
            &part_path(&dest),
            Sink::default(),
        )
        .await
        .unwrap_err();

        assert!(err.downcast_ref::<UrlExpired>().is_some(), "got: {err:#}");
    }

    #[tokio::test]
    async fn sha256_matches_known_digest() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f.bin");
        std::fs::write(&p, b"abc").unwrap();
        assert_eq!(
            sha256_of(&p).await.unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
