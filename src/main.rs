//! gopro-dl — bulk archive a GoPro cloud media library to local storage.

mod api;
mod auth;
mod cache;
mod download;
mod manifest;
mod model;
mod plan;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use clap::{Args, Parser, Subcommand};
use futures_util::stream::{self, StreamExt};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::api::Api;
use crate::manifest::{Entry, Manifest};
use crate::model::MediaItem;
use crate::plan::{Layout, Variant};

#[derive(Parser)]
#[command(
    name = "gopro-dl",
    version,
    about = "Download your GoPro cloud (GoPro Plus / Premium) media library to local disk",
    long_about = "Archive everything in your GoPro cloud account before the subscription lapses.\n\
                  Resumable, incremental, and safe to interrupt: re-running `sync` picks up where\n\
                  it left off using a manifest stored under <dest>/.gopro-dl/."
)]
struct Cli {
    /// Credentials file (default: <config-dir>/gopro-dl/credentials.json;
    /// `login` and `token` print the resolved path)
    #[arg(long, global = true, value_name = "FILE")]
    credentials: Option<PathBuf>,

    /// Verbose logging
    #[arg(short, long, global = true)]
    verbose: bool,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Log in with email + password (best effort; GoPro often blocks this)
    Login {
        #[arg(long)]
        email: Option<String>,
    },
    /// Store a bearer token copied from plus.gopro.com (the reliable path).
    /// Omit --access-token to be prompted, keeping it out of shell history.
    Token {
        #[arg(long, env = "GOPRO_ACCESS_TOKEN", hide_env_values = true)]
        access_token: Option<String>,
        #[arg(long, env = "GOPRO_REFRESH_TOKEN", hide_env_values = true)]
        refresh_token: Option<String>,
    },
    /// Show the account the stored token belongs to
    Whoami,
    /// Print a page of the media library without downloading
    List {
        #[arg(long, default_value_t = 25)]
        limit: usize,
        /// Emit raw JSON instead of a table
        #[arg(long)]
        json: bool,
    },
    /// Download everything not already on disk
    Sync(SyncArgs),
    /// Re-check downloaded files against the manifest
    Verify {
        #[arg(long, value_name = "DIR")]
        dest: PathBuf,
        /// Also recompute SHA-256 (slow; only checks entries that have one)
        #[arg(long)]
        hash: bool,
    },
    /// Summarise what has been archived so far, in items and bytes
    Stats {
        #[arg(long, value_name = "DIR")]
        dest: PathBuf,
        /// Re-read the cloud totals from GoPro before reporting
        #[arg(long)]
        refresh: bool,
    },
}

#[derive(Args)]
struct SyncArgs {
    /// Destination root, e.g. /Volumes/Archive/GoPro
    #[arg(long, value_name = "DIR")]
    dest: PathBuf,

    /// Parallel media items in flight
    #[arg(short, long, default_value_t = 4)]
    jobs: usize,

    /// Which renditions to keep
    #[arg(long, value_enum, default_value_t = Variant::Best)]
    variant: Variant,

    /// On-disk folder structure
    #[arg(long, value_enum, default_value_t = Layout::Date)]
    layout: Layout,

    /// Only media captured on/after this date (YYYY-MM-DD)
    #[arg(long)]
    since: Option<String>,

    /// Only media captured before this date (YYYY-MM-DD)
    #[arg(long)]
    until: Option<String>,

    /// Stop after this many media items
    #[arg(long)]
    limit: Option<usize>,

    /// Show what would be downloaded, then exit
    #[arg(long)]
    dry_run: bool,

    /// Record a SHA-256 for every downloaded file (enables `verify --hash`)
    #[arg(long)]
    hash: bool,

    /// Write a .json sidecar of the API metadata next to each item
    #[arg(long)]
    metadata: bool,

    /// Also fetch GoPro sidecar assets (LRV/THM/telemetry) when offered
    #[arg(long)]
    sidecars: bool,

    /// Process newest first instead of oldest first
    #[arg(long)]
    newest_first: bool,

    /// Attempt items GoPro has not finished processing (they normally 403)
    #[arg(long)]
    include_unprocessed: bool,

    /// Re-walk the media listing even if the cached one still looks current
    #[arg(long)]
    refresh_list: bool,

    /// How long a cached listing may be reused, in hours
    #[arg(long, default_value_t = 24)]
    cache_ttl: i64,

    /// Items per API page
    #[arg(long, default_value_t = 100)]
    per_page: u32,

    /// Attempts per network operation
    #[arg(long, default_value_t = 5)]
    retries: u32,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_logging(cli.verbose);
    let creds_path = cli.credentials.clone().unwrap_or_else(auth::default_path);

    match cli.cmd {
        Cmd::Login { email } => cmd_login(&creds_path, email).await,
        Cmd::Token { access_token, refresh_token } => {
            let raw = match access_token {
                Some(t) => t,
                None => rpassword::prompt_password(
                    "Paste the gp_access_token cookie value (or an Authorization header): ",
                )?,
            };
            let token = auth::normalize_token(&raw)
                .context("no token found in that input")?;
            auth::save(
                &creds_path,
                &auth::Credentials { access_token: token, refresh_token, expires_at: None },
            )?;
            println!("Saved credentials to {}", creds_path.display());
            println!("Check it with:  gopro-dl whoami");
            Ok(())
        }
        Cmd::Whoami => {
            let api = open_api(&creds_path, 3)?;
            let user = api.media_user().await?;
            println!("GoPro cloud account");
            if let Some(id) = &user.id {
                println!("  account id:   {id}");
            }
            if let Some(since) = &user.created_at {
                println!("  member since: {since}");
            }
            println!(
                "  media items:  {}",
                user.total_count.map(|n| n.to_string()).unwrap_or_else(|| "?".into())
            );
            println!(
                "  cloud size:   {}",
                user.total_storage.map(human_bytes).unwrap_or_else(|| "?".into())
            );
            Ok(())
        }
        Cmd::List { limit, json } => cmd_list(&creds_path, limit, json).await,
        Cmd::Sync(args) => cmd_sync(&creds_path, args).await,
        Cmd::Verify { dest, hash } => cmd_verify(&dest, hash).await,
        Cmd::Stats { dest, refresh } => cmd_stats(&dest, refresh, &creds_path).await,
    }
}

/// The live progress display, if one is running. Log records are routed through
/// it so they cannot interleave with a bar redraw.
static PROGRESS: std::sync::OnceLock<MultiProgress> = std::sync::OnceLock::new();

/// Writes log lines to stderr, pausing the progress bars for the duration.
///
/// Without this, `tracing` and `indicatif` both own the cursor and the output
/// shreds itself — half-drawn bars spliced into warnings.
struct BarWriter;

impl std::io::Write for BarWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match PROGRESS.get() {
            Some(mp) => mp.suspend(|| std::io::stderr().write_all(buf))?,
            None => std::io::stderr().write_all(buf)?,
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        std::io::stderr().flush()
    }
}

fn init_logging(verbose: bool) {
    let default = if verbose { "gopro_dl=debug" } else { "gopro_dl=info" };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .without_time()
        .with_target(false)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .with_writer(|| BarWriter)
        .init();
}

/// Errors that will never fix themselves. Retrying one of these across 2835
/// items burns hours to arrive at the same place, so the run stops instead.
fn fatal_reason(e: &anyhow::Error) -> Option<String> {
    for cause in e.chain() {
        if let Some(io) = cause.downcast_ref::<std::io::Error>() {
            let what = match io.raw_os_error() {
                Some(1) | Some(13) => "permission denied writing to the destination",
                Some(28) => "the destination volume is full",
                Some(30) => "the destination volume is mounted read-only",
                _ => continue,
            };
            return Some(what.to_string());
        }
    }
    None
}

/// Turn a write failure into something the user can act on.
fn explain_io(path: &std::path::Path, e: &std::io::Error) -> anyhow::Error {
    let base = format!("cannot write to {}: {e}", path.display());
    match e.raw_os_error() {
        Some(1) | Some(13) => anyhow::anyhow!(
            "{base}\n\n\
             macOS blocks programs from writing to removable volumes until you allow it.\n\
             Open System Settings -> Privacy & Security -> Files and Folders, find your\n\
             terminal app, and enable \"Removable Volumes\" (Full Disk Access also works).\n\
             Then re-run the same command."
        ),
        Some(28) => anyhow::anyhow!("{base}\n\nThe volume is full."),
        Some(30) => anyhow::anyhow!("{base}\n\nThe volume is mounted read-only."),
        _ => anyhow::anyhow!("{base}"),
    }
}

/// Prove the destination is usable before spending minutes listing the library.
fn preflight(dest: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(dest).map_err(|e| explain_io(dest, &e))?;
    let parts = manifest::parts_dir(dest);
    std::fs::create_dir_all(&parts).map_err(|e| explain_io(&parts, &e))?;
    // create_dir_all succeeds on an existing directory even when unwritable,
    // so actually write something.
    let probe = parts.join(".write-probe");
    std::fs::write(&probe, b"gopro-dl").map_err(|e| explain_io(&probe, &e))?;
    let _ = std::fs::remove_file(&probe);
    Ok(())
}

fn open_api(creds_path: &std::path::Path, retries: u32) -> Result<Api> {
    let creds = auth::load(creds_path)?;
    Api::new(creds, creds_path.to_path_buf(), retries)
}

async fn cmd_login(creds_path: &std::path::Path, email: Option<String>) -> Result<()> {
    let email = match email {
        Some(e) => e,
        None => {
            eprint!("GoPro email: ");
            let mut s = String::new();
            std::io::stdin().read_line(&mut s)?;
            s.trim().to_string()
        }
    };
    let password = rpassword::prompt_password("GoPro password: ")?;
    let http = reqwest::Client::new();

    let creds = match auth::password_login(&http, &email, &password, None).await {
        Ok(c) => c,
        Err(e) if e.to_string() == "TWO_FACTOR_REQUIRED" => {
            eprint!("Two-factor code: ");
            let mut code = String::new();
            std::io::stdin().read_line(&mut code)?;
            auth::password_login(&http, &email, &password, Some(code.trim())).await?
        }
        Err(e) => return Err(e),
    };
    auth::save(creds_path, &creds)?;
    println!("Saved credentials to {}", creds_path.display());
    Ok(())
}

async fn cmd_list(creds_path: &std::path::Path, limit: usize, json: bool) -> Result<()> {
    let api = open_api(creds_path, 3)?;
    let per_page = limit.clamp(1, 100) as u32;
    let page = api.search_page(1, per_page).await?;
    if let Some(p) = page.pages {
        eprintln!("{} items total across {} pages", p.total_items, p.total_pages);
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&page.embedded.media)?);
        return Ok(());
    }
    for item in page.embedded.media.iter().take(limit) {
        println!(
            "{:<22}  {:<10}  {:>10}  {}",
            item.timestamp()
                .map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string())
                .unwrap_or_else(|| "—".into()),
            item.kind.as_deref().unwrap_or("?"),
            item.size().map(human_bytes).unwrap_or_else(|| "?".into()),
            item.display_name()
        );
    }
    Ok(())
}

/// The media listing, from cache when it is still provably current.
async fn library(
    api: &Api,
    args: &SyncArgs,
    user: Option<&crate::model::MediaUser>,
) -> Result<Vec<MediaItem>> {
    let ttl = chrono::Duration::hours(args.cache_ttl.max(0));
    let cached = cache::load(&args.dest);

    let stale = match &cached {
        None => Some("no cached listing yet".to_string()),
        Some(_) if args.refresh_list => Some("--refresh-list given".to_string()),
        Some(c) => c.staleness(user, ttl),
    };

    if let (Some(c), None) = (&cached, &stale) {
        let age = chrono::Utc::now() - c.fetched_at;
        info!(
            "using cached listing: {} items, unchanged since {} ago \
             (--refresh-list to re-walk)",
            c.items.len(),
            approx(age)
        );
        return Ok(c.items.clone());
    }
    if let Some(why) = &stale {
        if cached.is_some() {
            info!("re-listing media: {why}");
        }
    }

    let items = collect_library(api, args.per_page.clamp(1, 100)).await?;
    let entry = cache::LibraryCache {
        fetched_at: chrono::Utc::now(),
        account_id: user.and_then(|u| u.id.clone()),
        total_count: user.and_then(|u| u.total_count),
        total_storage: user.and_then(|u| u.total_storage),
        items: items.clone(),
    };
    if let Err(e) = cache::save(&args.dest, &entry) {
        warn!("could not cache the listing: {e:#}");
    }
    Ok(items)
}

fn approx(d: chrono::Duration) -> String {
    let mins = d.num_minutes().max(0);
    if mins < 90 {
        format!("{mins}m")
    } else if mins < 60 * 48 {
        format!("{}h", mins / 60)
    } else {
        format!("{}d", mins / (60 * 24))
    }
}

/// Walk every page of `/media/search` and return the full library.
async fn collect_library(api: &Api, per_page: u32) -> Result<Vec<MediaItem>> {
    let spinner = ProgressBar::new_spinner();
    spinner.set_style(ProgressStyle::with_template("{spinner} {msg}").unwrap());
    spinner.enable_steady_tick(std::time::Duration::from_millis(120));
    spinner.set_message("listing media…");

    let mut all = Vec::new();
    let mut page = 1u32;
    loop {
        let resp = api.search_page(page, per_page).await?;
        let got = resp.embedded.media.len();
        all.extend(resp.embedded.media);
        spinner.set_message(format!("listing media… {} items", all.len()));

        let more = match resp.pages {
            Some(p) if p.total_pages > 0 => page < p.total_pages,
            _ => got as u32 == per_page,
        };
        if !more || got == 0 {
            break;
        }
        page += 1;
    }
    spinner.finish_with_message(format!("found {} media items", all.len()));
    Ok(all)
}

fn parse_date(s: &str) -> Result<DateTime<Utc>> {
    let d = NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .with_context(|| format!("`{s}` is not a YYYY-MM-DD date"))?;
    Ok(Utc.from_utc_datetime(&d.and_hms_opt(0, 0, 0).unwrap()))
}

async fn cmd_sync(creds_path: &std::path::Path, args: SyncArgs) -> Result<()> {
    if args.jobs == 0 {
        bail!("--jobs must be at least 1");
    }
    preflight(&args.dest)?;

    let api = Arc::new(open_api(creds_path, args.retries)?);

    // One cheap request that both validates the cached listing and supplies the
    // account totals used for progress reporting.
    let user = match api.media_user().await {
        Ok(u) => Some(u),
        Err(e) => {
            warn!("could not read account totals: {e:#}");
            None
        }
    };

    let mut items = library(&api, &args, user.as_ref()).await?;
    let library_size = items.len();

    // Filter
    let since = args.since.as_deref().map(parse_date).transpose()?;
    let until = args.until.as_deref().map(parse_date).transpose()?;
    if since.is_some() || until.is_some() {
        items.retain(|i| match i.timestamp() {
            Some(ts) => since.map_or(true, |s| ts >= s) && until.map_or(true, |u| ts < u),
            None => false,
        });
    }
    // Items still uploading or transcoding have signed URLs that 403. Leaving
    // them out keeps the run clean; they are not recorded, so a later run picks
    // them up once GoPro finishes.
    let mut unprocessed = 0usize;
    if !args.include_unprocessed {
        items.retain(|i| match i.ready_to_view.as_deref() {
            Some("ready") | None => true,
            Some(_) => {
                unprocessed += 1;
                false
            }
        });
    }

    items.sort_by_key(|i| i.timestamp());
    if args.newest_first {
        items.reverse();
    }

    let mut manifest_init = Manifest::load(&args.dest)?;
    manifest_init.library_items = Some(library_size as u64);
    // GoPro reports the account's byte total directly, which is more reliable
    // than summing per-item sizes (some items report none).
    if let Some(u) = &user {
        if let Some(n) = u.total_count {
            manifest_init.library_items = Some(n.max(library_size as u64));
        }
        manifest_init.library_bytes = u.total_storage;
    }

    // Decide what is left to do *before* touching the network. Asking GoPro for
    // download links only to find the file already on disk costs one request
    // per item, which on a resumed run is the entire library.
    let profile = profile_of(&args);
    let already_done = {
        let spinner = ProgressBar::new_spinner();
        spinner.set_style(ProgressStyle::with_template("{spinner} {msg}").unwrap());
        spinner.enable_steady_tick(std::time::Duration::from_millis(120));
        spinner.set_message("checking what is already on disk…");
        let before = items.len();
        items.retain(|i| !manifest_init.item_complete(&args.dest, &i.id, &profile));
        spinner.finish_and_clear();
        before - items.len()
    };
    if already_done > 0 {
        info!(
            "{already_done} item(s) already complete on disk; {} left to fetch",
            items.len()
        );
    }

    // Applied last, so `--limit 20` means twenty items of actual work rather
    // than twenty items that may all turn out to be done already.
    if let Some(limit) = args.limit {
        items.truncate(limit);
    }

    let manifest = Arc::new(Mutex::new(manifest_init));

    if unprocessed > 0 {
        info!(
            "skipping {unprocessed} item(s) GoPro is still processing; \
             re-run later to collect them (--include-unprocessed to try anyway)"
        );
    }

    if args.dry_run {
        let pending_bytes: u64 = items.iter().filter_map(|i| i.size()).sum();
        println!("{} items match the filters", items.len() + already_done);
        println!("  already complete: {already_done}");
        println!(
            "  to download:      {}  (~{} reported by GoPro)",
            items.len(),
            human_bytes(pending_bytes)
        );
        println!("  destination:      {}", args.dest.display());
        let partials = manifest::partials(&args.dest);
        if !partials.is_empty() {
            println!(
                "  resumable:        {} partial transfer(s), {} already fetched",
                partials.len(),
                human_bytes(partials.iter().map(|(_, n)| n).sum())
            );
        }
        return Ok(());
    }

    // First Ctrl-C stops between chunks and flushes the manifest; partials are
    // kept and resumed next run. A second one is an escape hatch, and is still
    // safe: nothing is published until it is complete.
    let cancel = Arc::new(AtomicBool::new(false));
    {
        let cancel = cancel.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                cancel.store(true, Ordering::SeqCst);
                warn!("stopping — partial files are kept and will resume next run \
                       (Ctrl-C again to quit now)");
            }
            if tokio::signal::ctrl_c().await.is_ok() {
                eprintln!("forced quit; progress up to the last completed file is saved");
                std::process::exit(130);
            }
        });
    }

    // GoPro reports a size per item in the search results, so we can show a real
    // byte total and ETA rather than a bare spinner. It is an estimate: it counts
    // one rendition per item, and items already on disk are excluded.
    // `items` already excludes everything on disk, so this is the work left.
    let pending_bytes: u64 = items.iter().filter_map(|i| i.size()).sum();

    let multi = MultiProgress::new();
    let _ = PROGRESS.set(multi.clone());
    let overall = multi.add(ProgressBar::new(items.len() as u64));
    overall.set_style(
        ProgressStyle::with_template("{bar:32.cyan/blue} {pos}/{len} items · {msg}")
            .unwrap()
            .progress_chars("=> "),
    );
    let bytes_bar = multi.add(ProgressBar::new(pending_bytes));
    bytes_bar.set_style(
        ProgressStyle::with_template(
            "{bar:32.green/blue} {bytes}/~{total_bytes} · {binary_bytes_per_sec} · ETA {eta}",
        )
        .unwrap()
        .progress_chars("=> "),
    );

    // Seeded from the manifest so a restart cannot hand a name to a different
    // item than the one that already owns it on disk.
    let claimed: Arc<Mutex<HashSet<PathBuf>>> =
        Arc::new(Mutex::new(manifest.lock().await.claimed_paths()));
    let bytes_total = Arc::new(AtomicU64::new(0));
    let files_total = Arc::new(AtomicU64::new(0));
    let skipped = Arc::new(AtomicU64::new(0));
    let short = Arc::new(AtomicU64::new(0));
    let failures: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    // Set when the run hits something no amount of retrying will fix, or when
    // enough items fail back-to-back that the problem is clearly systemic.
    let abort: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let consecutive = Arc::new(AtomicU64::new(0));
    let dest = Arc::new(args.dest.clone());
    let jobs = args.jobs;
    let run_total = items.len();
    let opts = Arc::new(args);

    let results = stream::iter(items.into_iter().map(|item| {
        let api = api.clone();
        let manifest = manifest.clone();
        let claimed = claimed.clone();
        let multi = multi.clone();
        let overall = overall.clone();
        let bytes_bar = bytes_bar.clone();
        let bytes_total = bytes_total.clone();
        let files_total = files_total.clone();
        let skipped = skipped.clone();
        let short = short.clone();
        let failures = failures.clone();
        let abort = abort.clone();
        let consecutive = consecutive.clone();
        let dest = dest.clone();
        let opts = opts.clone();
        let cancel = cancel.clone();

        async move {
            if cancel.load(Ordering::SeqCst) {
                return;
            }
            let name = item.display_name();
            overall.set_message(name.clone());

            let outcome = sync_one(
                &api, &item, &dest, &opts, &manifest, &claimed, &multi, &bytes_total,
                &files_total, &skipped, &short, &cancel, &overall, run_total,
            )
            .await;

            match outcome {
                Ok(()) => {
                    consecutive.store(0, Ordering::SeqCst);
                }
                Err(e) if e.downcast_ref::<download::Cancelled>().is_some() => {}
                Err(e) => {
                    let run = consecutive.fetch_add(1, Ordering::SeqCst) + 1;

                    if let Some(reason) = fatal_reason(&e) {
                        let mut slot = abort.lock().await;
                        if slot.is_none() {
                            *slot = Some(reason);
                        }
                        cancel.store(true, Ordering::SeqCst);
                    } else if run >= CONSECUTIVE_FAILURE_LIMIT {
                        let mut slot = abort.lock().await;
                        if slot.is_none() {
                            *slot = Some(format!(
                                "{run} items failed in a row — stopping rather than \
                                 working through the whole library"
                            ));
                        }
                        cancel.store(true, Ordering::SeqCst);
                    }

                    warn!("{name} ({}) failed: {e:#}", item.id);
                    failures.lock().await.push((item.id.clone(), format!("{e:#}")));
                }
            }
            overall.inc(1);
            bytes_bar.set_position(bytes_total.load(Ordering::SeqCst));

            // Flush periodically so a hard kill loses at most a few entries.
            if overall.position() % 25 == 0 {
                let m = manifest.lock().await;
                let _ = m.save(&dest);
            }
        }
    }))
    .buffer_unordered(jobs)
    .collect::<Vec<()>>();

    results.await;
    overall.finish_and_clear();
    bytes_bar.finish_and_clear();

    {
        let mut m = manifest.lock().await;
        m.last_sync = Some(Utc::now());
        m.save(&dest)?;
    }

    let failures = failures.lock().await;
    println!(
        "\nDownloaded {} files ({}) this run; {} item(s) were already complete.",
        files_total.load(Ordering::SeqCst),
        human_bytes(bytes_total.load(Ordering::SeqCst)),
        already_done + skipped.load(Ordering::SeqCst) as usize
    );
    {
        let m = manifest.lock().await;
        print_progress(&m);
    }
    println!("Archive:   {}", dest.display());
    let short_count = short.load(Ordering::SeqCst);
    if short_count > 0 {
        println!(
            "\n{short_count} file(s) did not match the size GoPro reports. \
             Run `gopro-dl verify --dest {}` and inspect them.",
            dest.display()
        );
    }
    if !failures.is_empty() {
        println!("\n{} item(s) failed — re-run `sync` to retry them:", failures.len());
        for (id, err) in failures.iter().take(20) {
            println!("  {id}: {}", api::truncate(err, 160));
        }
        if failures.len() > 20 {
            println!("  … and {} more", failures.len() - 20);
        }
    }
    if let Some(reason) = abort.lock().await.clone() {
        println!("\nRun stopped early: {reason}.");
        println!("Nothing already downloaded was lost; fix the cause and re-run to continue.");
        if let Some((_, first)) = failures.first() {
            println!("\nFirst error was:\n  {first}");
        }
        bail!("sync aborted: {reason}");
    }

    if cancel.load(Ordering::SeqCst) {
        let partials = manifest::partials(&dest);
        println!(
            "\nStopped on interrupt. {} partial transfer(s) held at {}; re-run the same \
             `sync` command to resume exactly where this left off.",
            partials.len(),
            human_bytes(partials.iter().map(|(_, n)| n).sum())
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn sync_one(
    api: &Api,
    item: &MediaItem,
    dest: &std::path::Path,
    opts: &SyncArgs,
    manifest: &Mutex<Manifest>,
    claimed: &Mutex<HashSet<PathBuf>>,
    multi: &MultiProgress,
    bytes_total: &AtomicU64,
    files_total: &AtomicU64,
    skipped: &AtomicU64,
    short: &AtomicU64,
    cancel: &AtomicBool,
    overall: &ProgressBar,
    run_total: usize,
) -> Result<()> {
    // Signed URLs are short-lived, so links are minted per attempt — but the
    // plan (which file goes where) is made exactly once, so a retry resumes the
    // same paths instead of claiming new ones.
    let mut last_err: Option<anyhow::Error> = None;
    let mut planned: Option<Vec<plan::PlannedAsset>> = None;

    for attempt in 1..=opts.retries.max(1) {
        if cancel.load(Ordering::SeqCst) {
            return Err(anyhow::anyhow!(download::Cancelled));
        }
        let dl = match api.download_links(&item.id).await {
            Ok(d) => d,
            Err(e) => {
                last_err = Some(e);
                tokio::time::sleep(backoff(attempt)).await;
                continue;
            }
        };

        match &mut planned {
            None => {
                let recorded = manifest.lock().await.recorded_for(&item.id);
                let mut guard = claimed.lock().await;
                planned = Some(plan::plan_item(
                    item,
                    &dl,
                    opts.variant,
                    opts.layout,
                    opts.sidecars,
                    &mut guard,
                    &recorded,
                ));
            }
            Some(existing) => {
                let fresh = plan::asset_urls(&dl, opts.variant, opts.sidecars);
                for asset in existing.iter_mut() {
                    if let Some(url) = fresh.get(&asset.asset_key) {
                        asset.url.clone_from(url);
                    }
                }
            }
        }
        let planned = planned.as_ref().expect("just populated");
        let originals = planned
            .iter()
            .filter(|a| a.asset_key.starts_with("original:"))
            .count();
        if planned.is_empty() {
            bail!(
                "GoPro offered no downloadable assets (state: {})",
                item.ready_to_view.as_deref().unwrap_or("unknown")
            );
        }

        let mut all_ok = true;
        for asset in planned {
            if manifest.lock().await.is_done(dest, &item.id, &asset.asset_key) {
                skipped.fetch_add(1, Ordering::SeqCst);
                continue;
            }
            if cancel.load(Ordering::SeqCst) {
                return Err(anyhow::anyhow!(download::Cancelled));
            }
            let target = dest.join(&asset.rel_path);
            let part = manifest::part_for(dest, &item.id, &asset.asset_key);

            let pb = multi.add(ProgressBar::new(0));
            pb.set_style(
                ProgressStyle::with_template(
                    "  {msg:36!} {bytes:>10}/{total_bytes:>10} {bytes_per_sec:>11}",
                )
                .unwrap(),
            );
            pb.set_message(
                asset
                    .rel_path
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default(),
            );

            let res = download::fetch(
                &api.http,
                &asset.url,
                &target,
                &part,
                download::Sink {
                    pb: Some(&pb),
                    downloaded: Some(bytes_total),
                    cancel: Some(cancel),
                },
            )
            .await;
            pb.finish_and_clear();
            multi.remove(&pb);

            match res {
                Ok(bytes) => {
                    // GoPro's per-item `file_size` is the size of the original.
                    // A mismatch means we fetched a different rendition than we
                    // meant to — the failure mode that silently fills an archive
                    // with 720p proxies — so surface it loudly.
                    if asset.asset_key.starts_with("original:") && originals == 1 {
                        if let Some(expected) = item.size() {
                            if bytes != expected {
                                short.fetch_add(1, Ordering::SeqCst);
                                warn!(
                                    "{}: got {} but GoPro reports {} for this item — \
                                     check the rendition",
                                    asset.rel_path.display(),
                                    human_bytes(bytes),
                                    human_bytes(expected)
                                );
                            }
                        }
                    }
                    let sha256 = if opts.hash {
                        Some(download::sha256_of(&target).await?)
                    } else {
                        None
                    };
                    let n = files_total.fetch_add(1, Ordering::SeqCst) + 1;
                    // A durable x/y trail: redirect stderr to a file and this is
                    // the record of exactly what landed, and when.
                    info!(
                        "[{}/{run_total} items · {n} files] {} ({})",
                        overall.position() + 1,
                        asset.rel_path.display(),
                        human_bytes(bytes)
                    );
                    manifest.lock().await.insert(Entry {
                        media_id: item.id.clone(),
                        asset_key: asset.asset_key.clone(),
                        rel_path: asset.rel_path.to_string_lossy().to_string(),
                        bytes,
                        sha256,
                        captured_at: item.captured_at.clone().or_else(|| item.created_at.clone()),
                        completed_at: Utc::now(),
                    });
                }
                Err(e) if e.downcast_ref::<download::Cancelled>().is_some() => return Err(e),
                // A full disk or a permission problem will fail identically on
                // every retry; surface it now instead of after five backoffs.
                Err(e) if fatal_reason(&e).is_some() => return Err(e),
                Err(e) => {
                    // Expired signature: re-mint links and try the item again.
                    if e.downcast_ref::<download::UrlExpired>().is_some() {
                        info!("signed URL expired for {}; re-minting", item.id);
                        all_ok = false;
                        break;
                    }
                    last_err = Some(e);
                    all_ok = false;
                    break;
                }
            }
        }

        if all_ok {
            if opts.metadata {
                write_metadata(dest, item, planned)?;
            }
            // Only now, with every planned asset on disk, is the item finished.
            manifest.lock().await.mark_complete(&item.id, &profile_of(opts));
            return Ok(());
        }
        tokio::time::sleep(backoff(attempt)).await;
    }

    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("gave up after {} attempts", opts.retries)))
}

/// How many back-to-back item failures before we conclude the problem is not
/// per-item and stop.
const CONSECUTIVE_FAILURE_LIMIT: u64 = 12;

/// Identifies the set of assets a run asks for, so a later run can tell
/// "nothing left to do" from "you changed the flags, look again".
fn profile_of(args: &SyncArgs) -> String {
    let mut p = args.variant.as_str().to_string();
    if args.sidecars {
        p.push_str("+sidecars");
    }
    p
}

fn backoff(attempt: u32) -> std::time::Duration {
    std::time::Duration::from_millis((500u64 << attempt.min(6)).min(30_000))
}

fn write_metadata(
    dest: &std::path::Path,
    item: &MediaItem,
    planned: &[plan::PlannedAsset],
) -> Result<()> {
    let Some(first) = planned.first() else { return Ok(()) };
    let mut side = dest.join(&first.rel_path);
    side.set_extension("gopro.json");
    let body = serde_json::json!({
        "media": item,
        "files": planned.iter().map(|p| p.rel_path.to_string_lossy()).collect::<Vec<_>>(),
    });
    std::fs::write(side, serde_json::to_vec_pretty(&body)?)?;
    Ok(())
}

async fn cmd_verify(dest: &std::path::Path, hash: bool) -> Result<()> {
    let m = Manifest::load(dest)?;
    let pb = ProgressBar::new(m.entries.len() as u64);
    pb.set_style(
        ProgressStyle::with_template("{bar:32.cyan/blue} {pos}/{len} {msg}")
            .unwrap()
            .progress_chars("=> "),
    );

    let (mut ok, mut missing, mut wrong_size, mut bad_hash) = (0u64, 0u64, 0u64, 0u64);
    for entry in m.entries.values() {
        pb.inc(1);
        let path = dest.join(&entry.rel_path);
        match std::fs::metadata(&path) {
            Err(_) => {
                missing += 1;
                println!("MISSING   {}", entry.rel_path);
                continue;
            }
            Ok(meta) if meta.len() != entry.bytes => {
                wrong_size += 1;
                println!("SIZE      {} ({} on disk, {} expected)", entry.rel_path, meta.len(), entry.bytes);
                continue;
            }
            Ok(_) => {}
        }
        if hash {
            if let Some(expected) = &entry.sha256 {
                pb.set_message(entry.rel_path.clone());
                let actual = download::sha256_of(&path).await?;
                if &actual != expected {
                    bad_hash += 1;
                    println!("CHECKSUM  {}", entry.rel_path);
                    continue;
                }
            }
        }
        ok += 1;
    }
    pb.finish_and_clear();
    println!("\n{ok} ok, {missing} missing, {wrong_size} wrong size, {bad_hash} checksum mismatch");
    if missing + wrong_size + bad_hash > 0 {
        println!("Delete the bad files and re-run `sync` to refetch them.");
        std::process::exit(1);
    }
    Ok(())
}

/// The "how far along am I" block — in items *and* bytes, because 1200 of 2838
/// items says little when clip sizes range from 50 MB to 2.3 GB.
fn print_progress(m: &Manifest) {
    let have_bytes = m.total_bytes();
    let archived = m.media_count() as u64;

    match m.library_items {
        Some(total) if total > 0 => println!(
            "Items:     {archived}/{total} archived ({:.1}%)",
            archived as f64 / total as f64 * 100.0
        ),
        _ => println!("Items:     {archived} archived"),
    }
    match m.library_bytes {
        Some(total) if total > 0 => println!(
            "Size:      {} of ~{} ({:.1}%)",
            human_bytes(have_bytes),
            human_bytes(total),
            (have_bytes as f64 / total as f64 * 100.0).min(100.0)
        ),
        _ => println!("Size:      {}", human_bytes(have_bytes)),
    }

    let items_left = m.library_items.map(|t| t.saturating_sub(archived));
    let bytes_left = m.library_bytes.map(|t| t.saturating_sub(have_bytes));
    if items_left.is_some() || bytes_left.is_some() {
        let items = items_left.map(|n| format!("{n} items")).unwrap_or_default();
        let bytes = bytes_left
            .map(|n| format!("~{} to fetch", human_bytes(n)))
            .unwrap_or_default();
        let parts: Vec<&str> = [items.as_str(), bytes.as_str()]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect();
        if !parts.is_empty() {
            println!("Remaining: {}", parts.join(", "));
        }
    }
    println!("Files:     {} on disk", m.entries.len());
}

async fn cmd_stats(dest: &std::path::Path, refresh: bool, creds: &std::path::Path) -> Result<()> {
    let mut m = Manifest::load(dest)?;

    if refresh {
        let api = open_api(creds, 3)?;
        let user = api.media_user().await.context("refreshing account totals")?;
        if let Some(n) = user.total_count {
            m.library_items = Some(n);
        }
        if let Some(b) = user.total_storage {
            m.library_bytes = Some(b);
        }
        m.save(dest)?;
    }

    println!("Archive:   {}", dest.display());
    print_progress(&m);
    if let Some(t) = m.last_sync {
        println!("Last sync: {}", t.format("%Y-%m-%d %H:%M:%S UTC"));
    }
    if m.library_bytes.is_none() {
        println!("(run `gopro-dl stats --refresh` to pull the cloud totals)");
    }

    let partials = manifest::partials(dest);
    if !partials.is_empty() {
        let bytes: u64 = partials.iter().map(|(_, n)| n).sum();
        println!(
            "Partial:   {} interrupted transfer(s) holding {} — `sync` resumes them",
            partials.len(),
            human_bytes(bytes)
        );
    }

    let mut by_year: BTreeMap<String, (u64, u64)> = BTreeMap::new();
    for e in m.entries.values() {
        let year = e
            .captured_at
            .as_deref()
            .and_then(|s| s.get(..4))
            .unwrap_or("????")
            .to_string();
        let slot = by_year.entry(year).or_default();
        slot.0 += 1;
        slot.1 += e.bytes;
    }
    if !by_year.is_empty() {
        println!("\nBy year:");
        for (year, (count, bytes)) in by_year {
            println!("  {year}  {count:>6} files  {:>12}", human_bytes(bytes));
        }
    }
    Ok(())
}

fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}
