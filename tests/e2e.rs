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
}

struct Fake {
    /// CDN paths already served once.
    served: Mutex<HashSet<String>>,
    mode: Mode,
}

async fn spawn_api(mode: Mode) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base = format!("http://{addr}");
    let state = Arc::new(Fake { served: Mutex::new(HashSet::new()), mode });
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
                loop {
                    let mut line = String::new();
                    if rd.read_line(&mut line).await.unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    let lower = line.to_ascii_lowercase();
                    if let Some(rest) = lower.strip_prefix("range: bytes=") {
                        range_start = rest.trim().trim_end_matches('-').parse().unwrap_or(0);
                    }
                }

                if path.starts_with("/media/search") {
                    let page = query_value(&path, "page").unwrap_or_else(|| "1".into());
                    let json = if page == "1" { search_page() } else { empty_page() };
                    let _ = write_json(&mut wr, &json).await;
                } else if path.starts_with("/media/") && path.ends_with("/download") {
                    let id = path.trim_start_matches("/media/").trim_end_matches("/download");
                    let _ = write_json(&mut wr, &download_json(&base, id)).await;
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

fn search_page() -> String {
    format!(
        r#"{{"_embedded":{{"media":[
            {{"id":"{ITEM_A}","filename":"GOPR0001.JPG","file_extension":"JPG",
              "captured_at":"2023-07-14T09:30:00Z","file_size":200000,"type":"Photo"}},
            {{"id":"{ITEM_B}","filename":"GOPR0001.JPG","file_extension":"JPG",
              "captured_at":"2023-07-14T11:00:00Z","file_size":"200000","type":"Photo"}}
        ]}},"_pages":{{"current_page":1,"per_page":100,"total_items":2,"total_pages":1}}}}"#
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
        stdout2.contains("Archived 2/2 items"),
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
    let staged = parts.join(format!("{ITEM_A}__source_0.part"));
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
    assert!(stdout3.contains("Archived 2/2 items"), "got:\n{stdout3}");
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
