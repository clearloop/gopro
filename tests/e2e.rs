//! End-to-end: drive the real binary against a stand-in GoPro API, and assert
//! the properties that matter for a long unattended archive run —
//! interrupted transfers resume, and a second run is a no-op.

use std::collections::HashSet;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Two items sharing one camera filename — the case that made naming ambiguous.
const ITEM_A: &str = "aaaaaaaa1111";
const ITEM_B: &str = "bbbbbbbb2222";

fn body_for(id: &str) -> Vec<u8> {
    // Big enough to span several chunks, small enough to stay fast.
    id.as_bytes().iter().cycle().take(200_000).copied().collect()
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    /// Serve everything immediately.
    Normal,
    /// Drop the first attempt at each asset half-way through, as a flaky link
    /// would; the retry resumes.
    TruncateFirst,
    /// Dribble the body out so a run can be interrupted mid-file.
    Slow,
    /// Reject anything that does not carry the `gp_access_token` cookie, the
    /// way GoPro's web session actually authenticates.
    RequireCookie,
    /// Serve a large library whose every asset 500s, to exercise the
    /// give-up-early path.
    AlwaysFail,
}

struct Fake {
    /// CDN paths already served once.
    served: Mutex<HashSet<String>>,
    /// How many times the client walked the media listing.
    searches: Mutex<usize>,
    /// How many times it asked for an item's signed download links.
    downloads: Mutex<usize>,
    mode: Mode,
}

async fn spawn_api(mode: Mode) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base = format!("http://{addr}");
    let state = Arc::new(Fake {
        served: Mutex::new(HashSet::new()),
        searches: Mutex::new(0),
        downloads: Mutex::new(0),
        mode,
    });
    let base_for_handler = base.clone();

    tokio::spawn(async move {
        while let Ok((sock, _)) = listener.accept().await {
            let state = state.clone();
            let base = base_for_handler.clone();
            tokio::spawn(async move {
                let (rd, mut wr) = sock.into_split();
                let mut rd = tokio::io::BufReader::new(rd);

                let mut request_line = String::new();
                if rd.read_line(&mut request_line).await.unwrap_or(0) == 0 {
                    return;
                }
                let path = request_line.split_whitespace().nth(1).unwrap_or("/").to_string();

                let mut range_start = 0usize;
                let mut has_cookie = false;
                loop {
                    let mut line = String::new();
                    if rd.read_line(&mut line).await.unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    let lower = line.to_ascii_lowercase();
                    if let Some(rest) = lower.strip_prefix("range: bytes=") {
                        range_start = rest.trim().trim_end_matches('-').parse().unwrap_or(0);
                    }
                    if lower.starts_with("cookie:") && lower.contains("gp_access_token=test-token") {
                        has_cookie = true;
                    }
                }

                // Signed CDN links carry their own auth, so only the API is gated.
                if state.mode == Mode::RequireCookie && !has_cookie && !path.starts_with("/cdn/") {
                    let _ = wr
                        .write_all(b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n")
                        .await;
                    return;
                }

                if path == "/debug/counts" {
                    let s = *state.searches.lock().unwrap();
                    let d = *state.downloads.lock().unwrap();
                    let _ = write_json(
                        &mut wr,
                        &format!("{{\"searches\":{s},\"downloads\":{d}}}"),
                    )
                    .await;
                } else if path == "/media/user" {
                    let n = if state.mode == Mode::AlwaysFail { 30 } else { 2 };
                    let _ = write_json(
                        &mut wr,
                        &format!(
                            "{{\"id\":\"acct-test\",\"total_count\":{n},\"total_storage\":400000}}"
                        ),
                    )
                    .await;
                } else if path.starts_with("/media/search") {
                    *state.searches.lock().unwrap() += 1;
                    let page = query_value(&path, "page").unwrap_or_else(|| "1".into());
                    let n = if state.mode == Mode::AlwaysFail { 30 } else { 2 };
                    let json = if page == "1" { search_page(n) } else { empty_page() };
                    let _ = write_json(&mut wr, &json).await;
                } else if path.starts_with("/media/") && path.ends_with("/download") {
                    *state.downloads.lock().unwrap() += 1;
                    let id = path.trim_start_matches("/media/").trim_end_matches("/download");
                    let _ = write_json(&mut wr, &download_json(&base, id)).await;
                } else if state.mode == Mode::AlwaysFail && path.starts_with("/cdn/") {
                    let _ = wr
                        .write_all(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n")
                        .await;
                } else if let Some(id) = path.strip_prefix("/cdn/").map(|p| {
                    p.split('/').next().unwrap_or_default().to_string()
                }) {
                    let body = body_for(&id);
                    let total = body.len();
                    let first_time = state.served.lock().unwrap().insert(path.clone());
                    let cut = state.mode == Mode::TruncateFirst && first_time;

                    let head = if range_start > 0 {
                        format!(
                            "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\n\
                             Content-Range: bytes {range_start}-{}/{total}\r\n\
                             Connection: close\r\n\r\n",
                            total - range_start,
                            total - 1
                        )
                    } else {
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {total}\r\n\
                             Accept-Ranges: bytes\r\nConnection: close\r\n\r\n"
                        )
                    };
                    let _ = wr.write_all(head.as_bytes()).await;
                    let slice = &body[range_start..];
                    let end = if cut { slice.len() / 2 } else { slice.len() };
                    if state.mode == Mode::Slow {
                        for chunk in slice[..end].chunks(10_000) {
                            if wr.write_all(chunk).await.is_err() {
                                return;
                            }
                            tokio::time::sleep(std::time::Duration::from_millis(120)).await;
                        }
                    } else {
                        let _ = wr.write_all(&slice[..end]).await;
                    }
                    // Dropping mid-body is exactly what a flaky link does.
                } else {
                    let _ = wr.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n").await;
                }
            });
        }
    });
    base
}

fn query_value(path: &str, key: &str) -> Option<String> {
    path.split(['?', '&'])
        .filter_map(|kv| kv.split_once('='))
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v.to_string())
}

async fn write_json(wr: &mut tokio::net::tcp::OwnedWriteHalf, json: &str) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n",
        json.len()
    );
    wr.write_all(head.as_bytes()).await?;
    wr.write_all(json.as_bytes()).await
}

fn search_page(n: usize) -> String {
    // Two items sharing a filename by design; the rest are filler used by the
    // give-up-early test.
    let mut media = vec![
        format!(
            r#"{{"id":"{ITEM_A}","filename":"GOPR0001.JPG","file_extension":"JPG",
                "captured_at":"2023-07-14T09:30:00Z","file_size":200000,"type":"Photo",
                "ready_to_view":"ready"}}"#
        ),
        format!(
            r#"{{"id":"{ITEM_B}","filename":"GOPR0001.JPG","file_extension":"JPG",
                "captured_at":"2023-07-14T11:00:00Z","file_size":"200000","type":"Photo",
                "ready_to_view":"ready"}}"#
        ),
    ];
    for i in 2..n {
        media.push(format!(
            r#"{{"id":"id{i:04}","filename":"GOPR{i:04}.JPG","file_extension":"JPG",
                "captured_at":"2023-07-15T09:00:00Z","file_size":200000,"type":"Photo",
                "ready_to_view":"ready"}}"#
        ));
    }
    format!(
        r#"{{"_embedded":{{"media":[{}]}},"_pages":{{"current_page":1,"per_page":100,
           "total_items":{},"total_pages":1}}}}"#,
        media.join(","),
        media.len()
    )
}

fn empty_page() -> String {
    r#"{"_embedded":{"media":[]},"_pages":{"current_page":2,"per_page":100,"total_items":2,"total_pages":1}}"#.to_string()
}

fn download_json(base: &str, id: &str) -> String {
    format!(
        r#"{{"filename":"GOPR0001.JPG","_embedded":{{"files":[
            {{"url":"{base}/cdn/{id}/GOPR0001.JPG?sig=abc"}}
        ]}}}}"#
    )
}

fn run_sync(base: &str, dest: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_gopro-dl"))
        .args(["sync", "--dest", dest.to_str().unwrap(), "--jobs", "1", "--retries", "5"])
        .env("GOPRO_API_BASE", base)
        .env("GOPRO_ACCESS_TOKEN", "test-token")
        .output()
        .expect("failed to run gopro-dl")
}

#[tokio::test(flavor = "multi_thread")]
async fn interrupted_transfers_resume_and_a_second_run_is_a_noop() {
    let base = spawn_api(Mode::TruncateFirst).await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path();

    // --- Run 1: every transfer is cut in half on its first attempt. ---
    let out = run_sync(&base, dest);
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(out.status.success(), "run 1 failed:\n{stderr}");

    let a = dest.join("2023/2023-07-14/GOPR0001.JPG");
    let b = dest.join("2023/2023-07-14/GOPR0001-bbbbbbbb.JPG");
    assert!(a.exists(), "missing {}\n{stderr}", a.display());
    assert!(b.exists(), "missing {}\n{stderr}", b.display());

    // Resumed bytes must splice correctly, not double up or truncate.
    assert_eq!(std::fs::read(&a).unwrap(), body_for(ITEM_A));
    assert_eq!(std::fs::read(&b).unwrap(), body_for(ITEM_B));

    // Nothing left staged, and no .part litter in the archive tree.
    assert!(
        gopro_dl_partials(dest).is_empty(),
        "staging dir should be empty after a clean run"
    );
    assert!(!dest.join("2023/2023-07-14/GOPR0001.JPG.part").exists());

    // --- Run 2: same command, nothing left to do. ---
    let out2 = run_sync(&base, dest);
    let stdout2 = String::from_utf8_lossy(&out2.stdout).to_string();
    assert!(out2.status.success(), "run 2 failed");
    assert!(
        stdout2.contains("Downloaded 0 files"),
        "second run should download nothing, got:\n{stdout2}"
    );
    assert!(
        stdout2.contains("Items:     2/2 archived"),
        "second run should report full coverage, got:\n{stdout2}"
    );

    // Filenames must not shuffle between runs.
    assert_eq!(std::fs::read(&a).unwrap(), body_for(ITEM_A));
    assert_eq!(std::fs::read(&b).unwrap(), body_for(ITEM_B));

    // And the archive holds exactly two media files.
    let files: Vec<_> = walk(dest)
        .into_iter()
        .filter(|p| !p.starts_with(dest.join(".gopro-dl")))
        .collect();
    assert_eq!(files.len(), 2, "unexpected files: {files:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stale_partial_from_a_killed_run_is_resumed_not_restarted() {
    let base = spawn_api(Mode::Normal).await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path();

    // Simulate a hard kill: half of item A already staged under its own key.
    let parts = dest.join(".gopro-dl/parts");
    std::fs::create_dir_all(&parts).unwrap();
    let staged = parts.join(format!("{ITEM_A}__original_0.part"));
    let full = body_for(ITEM_A);
    std::fs::write(&staged, &full[..full.len() / 2]).unwrap();

    let out = run_sync(&base, dest);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

    let a = dest.join("2023/2023-07-14/GOPR0001.JPG");
    assert_eq!(std::fs::read(&a).unwrap(), full, "resumed file must match byte for byte");
    assert!(!staged.exists(), "staged partial should be consumed");
}

#[tokio::test(flavor = "multi_thread")]
async fn ctrl_c_stops_cleanly_and_the_next_run_finishes_the_job() {
    let slow = spawn_api(Mode::Slow).await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path();

    let child = Command::new(env!("CARGO_BIN_EXE_gopro-dl"))
        .args(["sync", "--dest", dest.to_str().unwrap(), "--jobs", "1"])
        .env("GOPRO_API_BASE", &slow)
        .env("GOPRO_ACCESS_TOKEN", "test-token")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn gopro-dl");

    // Long enough to be mid-transfer on the first item, short enough to be well
    // before it completes (200 KB at 10 KB / 120 ms is ~2.4 s per file).
    tokio::time::sleep(std::time::Duration::from_millis(1400)).await;
    Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .expect("send SIGINT");

    let out = child.wait_with_output().expect("wait for child");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(out.status.success(), "interrupt should be a clean exit, got {:?}", out.status);
    assert!(
        stdout.contains("Stopped on interrupt"),
        "expected an interrupt summary, got:\n{stdout}"
    );

    // Nothing half-written is ever published into the archive tree...
    let published: Vec<_> = walk(dest)
        .into_iter()
        .filter(|p| !p.starts_with(dest.join(".gopro-dl")))
        .collect();
    assert!(published.len() < 2, "should not have finished both items: {published:?}");
    for p in &published {
        let id = if p.to_string_lossy().contains("bbbbbbbb") { ITEM_B } else { ITEM_A };
        assert_eq!(std::fs::read(p).unwrap(), body_for(id), "published file must be complete");
    }

    // ...but the work in flight is preserved for next time.
    let staged = gopro_dl_partials(dest);
    assert!(!staged.is_empty(), "interrupt should leave a resumable partial");
    let staged_bytes: u64 = staged.iter().map(|p| std::fs::metadata(p).unwrap().len()).sum();
    assert!(staged_bytes > 0, "partial should hold the bytes already fetched");
    assert!(staged_bytes < 200_000, "partial should be incomplete, got {staged_bytes}");

    // --- Restart against a responsive server: it picks up and finishes. ---
    let fast = spawn_api(Mode::Normal).await;
    let out2 = run_sync(&fast, dest);
    assert!(out2.status.success(), "{}", String::from_utf8_lossy(&out2.stderr));

    let a = dest.join("2023/2023-07-14/GOPR0001.JPG");
    let b = dest.join("2023/2023-07-14/GOPR0001-bbbbbbbb.JPG");
    assert_eq!(std::fs::read(&a).unwrap(), body_for(ITEM_A));
    assert_eq!(std::fs::read(&b).unwrap(), body_for(ITEM_B));
    assert!(gopro_dl_partials(dest).is_empty(), "staging should be drained");

    // A third run has nothing left to do.
    let out3 = run_sync(&fast, dest);
    let stdout3 = String::from_utf8_lossy(&out3.stdout).to_string();
    assert!(stdout3.contains("Downloaded 0 files"), "got:\n{stdout3}");
    assert!(stdout3.contains("Items:     2/2 archived"), "got:\n{stdout3}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cookie_only_session_authenticates() {
    // GoPro's web app sends no Authorization header — the token lives in a
    // gp_access_token cookie — so the client must present it that way too.
    let base = spawn_api(Mode::RequireCookie).await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path();

    let out = run_sync(&base, dest);
    assert!(
        out.status.success(),
        "cookie auth failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let a = dest.join("2023/2023-07-14/GOPR0001.JPG");
    assert!(a.exists(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(std::fs::read(&a).unwrap(), body_for(ITEM_A));
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unwritable_destination_fails_before_listing_anything() {
    // Exactly what an external disk does when macOS has not granted the
    // terminal access to removable volumes.
    let base = spawn_api(Mode::Normal).await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("archive");
    std::fs::create_dir_all(&dest).unwrap();
    let mut perms = std::fs::metadata(&dest).unwrap().permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        perms.set_mode(0o555);
    }
    std::fs::set_permissions(&dest, perms).unwrap();

    let started = std::time::Instant::now();
    let out = run_sync(&base, &dest);
    let elapsed = started.elapsed();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();

    assert!(!out.status.success(), "should exit non-zero, got:\n{stderr}");
    assert!(
        stderr.contains("cannot write to"),
        "should name the write failure, got:\n{stderr}"
    );
    assert!(
        stderr.contains("Removable Volumes"),
        "should say how to fix it, got:\n{stderr}"
    );
    assert!(
        !stderr.contains("found") && !stderr.contains("listing media"),
        "must fail before listing the library, got:\n{stderr}"
    );
    assert!(elapsed.as_secs() < 10, "should fail fast, took {elapsed:?}");

    // Leave the dir removable by tempfile's cleanup.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut p = std::fs::metadata(&dest).unwrap().permissions();
        p.set_mode(0o755);
        std::fs::set_permissions(&dest, p).unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_run_that_fails_every_item_gives_up_instead_of_grinding_on() {
    let base = spawn_api(Mode::AlwaysFail).await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path();

    let out = Command::new(env!("CARGO_BIN_EXE_gopro-dl"))
        .args(["sync", "--dest", dest.to_str().unwrap(), "--jobs", "1", "--retries", "1"])
        .env("GOPRO_API_BASE", &base)
        .env("GOPRO_ACCESS_TOKEN", "test-token")
        .output()
        .expect("run gopro-dl");

    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(!out.status.success(), "should exit non-zero");
    assert!(
        stdout.contains("failed in a row") || stderr.contains("failed in a row"),
        "should explain why it stopped, got:\n{stdout}\n{stderr}"
    );
    // 30 items were offered; it must not have attempted anywhere near all of them.
    let attempted = stderr.matches("failed:").count();
    assert!(attempted < 25, "gave up too late: {attempted} attempts");
}

async fn counts(base: &str) -> (usize, usize) {
    let body = reqwest_get(&format!("{base}/debug/counts")).await;
    let pick = |key: &str| -> usize {
        let tail = body
            .split(&format!("\"{key}\":"))
            .nth(1)
            .unwrap_or_else(|| panic!("no `{key}` in debug body: {body:?}"));
        let digits: String = tail.chars().take_while(|c| c.is_ascii_digit()).collect();
        digits
            .parse()
            .unwrap_or_else(|_| panic!("bad `{key}` value in debug body: {body:?}"))
    };
    (pick("searches"), pick("downloads"))
}

async fn search_count(base: &str) -> usize {
    counts(base).await.0
}

/// Minimal GET so the test does not need an HTTP client dependency.
async fn reqwest_get(url: &str) -> String {
    let rest = url.trim_start_matches("http://");
    let (host, path) = rest.split_once('/').unwrap();
    let mut sock = tokio::net::TcpStream::connect(host).await.unwrap();
    let req = format!("GET /{path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    sock.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    tokio::io::AsyncReadExt::read_to_end(&mut sock, &mut buf).await.unwrap();
    let text = String::from_utf8_lossy(&buf).to_string();
    text.split("\r\n\r\n").nth(1).unwrap_or("").to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_completed_run_asks_gopro_for_nothing() {
    // Resuming used to cost one /media/{id}/download call per item just to
    // learn the file was already on disk — the whole library, every time.
    let base = spawn_api(Mode::Normal).await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path();

    let out = run_sync(&base, dest);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let (searches, downloads) = counts(&base).await;
    assert!(downloads >= 2, "first run must fetch links for both items");

    let out2 = run_sync(&base, dest);
    let stderr2 = String::from_utf8_lossy(&out2.stderr).to_string();
    let stdout2 = String::from_utf8_lossy(&out2.stdout).to_string();
    assert!(out2.status.success(), "{stderr2}");

    assert_eq!(
        counts(&base).await,
        (searches, downloads),
        "a completed re-run must make no further API calls; stderr:\n{stderr2}"
    );
    assert!(
        stderr2.contains("already complete on disk"),
        "should say why there was nothing to do, got:\n{stderr2}"
    );
    assert!(stdout2.contains("Downloaded 0 files"), "got:\n{stdout2}");

    // Deleting a file must reopen exactly that item.
    std::fs::remove_file(dest.join("2023/2023-07-14/GOPR0001.JPG")).unwrap();
    let out3 = run_sync(&base, dest);
    assert!(out3.status.success());
    let (_, after) = counts(&base).await;
    assert_eq!(
        after,
        downloads + 1,
        "only the missing item should be re-fetched"
    );
    assert_eq!(
        std::fs::read(dest.join("2023/2023-07-14/GOPR0001.JPG")).unwrap(),
        body_for(ITEM_A)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_run_reuses_the_cached_listing() {
    let base = spawn_api(Mode::Normal).await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path();

    let out = run_sync(&base, dest);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let after_first = search_count(&base).await;
    assert!(after_first >= 1, "first run must walk the listing");
    assert!(dest.join(".gopro-dl/library.json").exists(), "cache should be written");

    // Nothing about the library changed, so the listing must not be walked again.
    let out2 = run_sync(&base, dest);
    let stderr2 = String::from_utf8_lossy(&out2.stderr).to_string();
    assert!(out2.status.success(), "{stderr2}");
    assert_eq!(
        search_count(&base).await,
        after_first,
        "second run re-walked the listing; stderr:\n{stderr2}"
    );
    assert!(
        stderr2.contains("using cached listing"),
        "should say it used the cache, got:\n{stderr2}"
    );

    // --refresh-list overrides it.
    let out3 = Command::new(env!("CARGO_BIN_EXE_gopro-dl"))
        .args([
            "sync", "--dest", dest.to_str().unwrap(), "--jobs", "1", "--refresh-list",
        ])
        .env("GOPRO_API_BASE", &base)
        .env("GOPRO_ACCESS_TOKEN", "test-token")
        .output()
        .unwrap();
    assert!(out3.status.success());
    assert!(
        search_count(&base).await > after_first,
        "--refresh-list should force a re-walk"
    );
}

fn gopro_dl_partials(dest: &Path) -> Vec<std::path::PathBuf> {
    match std::fs::read_dir(dest.join(".gopro-dl/parts")) {
        Ok(rd) => rd.flatten().map(|e| e.path()).collect(),
        Err(_) => Vec::new(),
    }
}

fn walk(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else { return out };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(walk(&p));
        } else {
            out.push(p);
        }
    }
    out
}
