//! Turning a media item + its signed URLs into concrete files on disk:
//! which assets to keep, and what to call them.

use clap::ValueEnum;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use crate::model::{Asset, DownloadResponse, MediaItem};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Variant {
    /// Camera originals only. What you want for an archive.
    Source,
    /// Originals, plus the highest-resolution cloud transcode when no original
    /// is offered (older uploads sometimes only expose variations).
    Best,
    /// Everything GoPro will hand over, including low-res proxies.
    All,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Layout {
    /// `<dest>/2023/2023-07-14/GX010123.MP4`
    Date,
    /// `<dest>/2023/GX010123.MP4`
    Year,
    /// `<dest>/GX010123.MP4`
    Flat,
}

#[derive(Debug, Clone)]
pub struct PlannedAsset {
    pub asset_key: String,
    pub url: String,
    pub rel_path: PathBuf,
}

/// Strip anything that would upset exFAT/NTFS on an external disk.
pub fn sanitize(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if (c as u32) < 0x20 => '_',
            c => c,
        })
        .collect();
    let cleaned = cleaned.trim().trim_end_matches('.').to_string();
    if cleaned.is_empty() {
        "unnamed".to_string()
    } else {
        cleaned
    }
}

fn split_name(name: &str) -> (String, String) {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && ext.len() <= 5 => {
            (stem.to_string(), ext.to_string())
        }
        _ => (name.to_string(), String::new()),
    }
}

fn join_name(stem: &str, ext: &str) -> String {
    if ext.is_empty() {
        stem.to_string()
    } else {
        format!("{stem}.{ext}")
    }
}

/// CDN URLs usually carry the true filename, either as the last path segment or
/// in a `response-content-disposition` override.
pub fn filename_from_url(raw: &str) -> Option<String> {
    let parsed = url::Url::parse(raw).ok()?;
    for (k, v) in parsed.query_pairs() {
        if k.eq_ignore_ascii_case("response-content-disposition") {
            if let Some(idx) = v.to_lowercase().find("filename=") {
                let tail = v[idx + "filename=".len()..].trim_matches('"');
                let name = tail.split(';').next().unwrap_or(tail).trim_matches('"');
                if name.contains('.') {
                    return Some(sanitize(name));
                }
            }
        }
    }
    let seg = parsed.path_segments()?.next_back()?;
    let decoded = percent_decode(seg);
    if decoded.contains('.') && decoded.len() < 200 {
        Some(sanitize(&decoded))
    } else {
        None
    }
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn dir_for(item: &MediaItem, layout: Layout) -> PathBuf {
    if layout == Layout::Flat {
        return PathBuf::new();
    }
    match item.timestamp() {
        Some(ts) if layout == Layout::Date => PathBuf::from(ts.format("%Y").to_string())
            .join(ts.format("%Y-%m-%d").to_string()),
        Some(ts) => PathBuf::from(ts.format("%Y").to_string()),
        None => PathBuf::from("undated"),
    }
}

/// Pick the assets to fetch for one item, in a stable order.
fn select(dl: &DownloadResponse, variant: Variant, sidecars: bool) -> Vec<(&Asset, bool)> {
    let mut out: Vec<(&Asset, bool)> = Vec::new();
    for f in &dl.embedded.files {
        out.push((f, true));
    }

    match variant {
        Variant::Source => {}
        Variant::All => {
            for v in &dl.embedded.variations {
                out.push((v, false));
            }
        }
        Variant::Best => {
            if out.is_empty() {
                if let Some(best) = dl
                    .embedded
                    .variations
                    .iter()
                    .max_by_key(|v| (v.pixels(), v.tag()))
                {
                    out.push((best, false));
                }
            }
        }
    }

    if sidecars {
        for s in &dl.embedded.sidecar_files {
            out.push((s, false));
        }
    }
    out
}

/// Stable identity for one downloadable asset within a media item.
fn asset_key(asset: &Asset, is_source: bool) -> String {
    if is_source {
        format!("source:{}", asset.item_number.unwrap_or(0))
    } else {
        format!("{}:{}", asset.tag(), asset.item_number.unwrap_or(0))
    }
}

/// Freshly signed URLs for an item, keyed the same way `plan_item` keys them.
///
/// Signed URLs expire, so a retry re-mints them — but it must *not* re-plan the
/// filenames, or an item that failed once would claim a second name and land on
/// disk twice under different names.
pub fn asset_urls(
    dl: &DownloadResponse,
    variant: Variant,
    sidecars: bool,
) -> HashMap<String, String> {
    select(dl, variant, sidecars)
        .into_iter()
        .map(|(a, is_source)| (asset_key(a, is_source), a.url.clone()))
        .collect()
}

/// `claimed` carries across items within a run — and is seeded from the
/// manifest at startup — so two media items that share a filename (very common:
/// cameras reset their counters) never overwrite each other, and the item that
/// won a bare name in an earlier run keeps it.
///
/// `recorded` maps asset key to the path a previous run already committed to
/// for *this* item; those win outright, so a restart never renames a file it
/// has already written.
pub fn plan_item(
    item: &MediaItem,
    dl: &DownloadResponse,
    variant: Variant,
    layout: Layout,
    sidecars: bool,
    claimed: &mut HashSet<PathBuf>,
    recorded: &HashMap<String, PathBuf>,
) -> Vec<PlannedAsset> {
    let base = sanitize(dl.filename.as_deref().unwrap_or(&item.display_name()));
    let (base_stem, base_ext) = split_name(&base);
    let dir = dir_for(item, layout);

    let chosen = select(dl, variant, sidecars);
    let multi_source = chosen.iter().filter(|(_, is_source)| *is_source).count() > 1;

    let mut planned = Vec::new();
    for (asset, is_source) in chosen {
        let from_url = filename_from_url(&asset.url);

        let name = if is_source {
            match (multi_source, from_url.as_deref()) {
                // Chaptered video / burst: trust the CDN's per-part filename.
                (true, Some(n)) => n.to_string(),
                (true, None) => {
                    let n = asset.item_number.unwrap_or(0);
                    join_name(&format!("{base_stem}-{n:03}"), &base_ext)
                }
                (false, _) => base.clone(),
            }
        } else {
            let ext = asset
                .file_extension
                .clone()
                .filter(|e| !e.is_empty())
                .or_else(|| from_url.as_deref().map(|n| split_name(n).1))
                .filter(|e| !e.is_empty())
                .unwrap_or_else(|| base_ext.clone());
            let suffix = sanitize(&asset.tag());
            let part = asset
                .item_number
                .filter(|n| *n > 1)
                .map(|n| format!("-{n:03}"))
                .unwrap_or_default();
            join_name(&format!("{base_stem}-{suffix}{part}"), &ext)
        };

        let asset_key = asset_key(asset, is_source);

        if let Some(known) = recorded.get(&asset_key) {
            claimed.insert(known.clone());
            planned.push(PlannedAsset {
                asset_key,
                url: asset.url.clone(),
                rel_path: known.clone(),
            });
            continue;
        }

        let mut rel = dir.join(&name);
        if !claimed.insert(rel.clone()) {
            // Same name, different media item — disambiguate with the id.
            let (stem, ext) = split_name(&name);
            let short_id: String = item.id.chars().take(8).collect();
            rel = dir.join(join_name(&format!("{stem}-{short_id}"), &ext));
            claimed.insert(rel.clone());
        }

        planned.push(PlannedAsset { asset_key, url: asset.url.clone(), rel_path: rel });
    }
    planned
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DownloadResponse, MediaItem};

    fn item(json: serde_json::Value) -> MediaItem {
        serde_json::from_value(json).unwrap()
    }

    fn dl(json: serde_json::Value) -> DownloadResponse {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn sanitize_strips_hostile_characters() {
        assert_eq!(sanitize("a/b:c*d?.MP4"), "a_b_c_d?.MP4".replace('?', "_"));
        assert_eq!(sanitize("   "), "unnamed");
        assert_eq!(sanitize("trailing."), "trailing");
    }

    #[test]
    fn filename_comes_from_path_segment() {
        let u = "https://cdn.gopro.com/derivate/1/2/GX010123.MP4?X-Amz-Signature=abc";
        assert_eq!(filename_from_url(u).as_deref(), Some("GX010123.MP4"));
    }

    #[test]
    fn filename_prefers_content_disposition() {
        let u = "https://cdn.gopro.com/x/opaque-blob\
                 ?response-content-disposition=attachment%3B%20filename%3D%22GOPR9999.JPG%22";
        assert_eq!(filename_from_url(u).as_deref(), Some("GOPR9999.JPG"));
    }

    #[test]
    fn single_source_lands_in_date_folders() {
        let it = item(serde_json::json!({
            "id": "abc123def456", "filename": "GX010123.MP4", "captured_at": "2023-07-14T09:30:00Z"
        }));
        let d = dl(serde_json::json!({
            "filename": "GX010123.MP4",
            "_embedded": { "files": [{ "url": "https://cdn/x/GX010123.MP4?sig=1" }] }
        }));
        let mut claimed = HashSet::new();
        let p = plan_item(&it, &d, Variant::Source, Layout::Date, false, &mut claimed, &HashMap::new());
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].rel_path, PathBuf::from("2023/2023-07-14/GX010123.MP4"));
        assert_eq!(p[0].asset_key, "source:0");
    }

    #[test]
    fn chaptered_video_keeps_every_part() {
        let it = item(serde_json::json!({
            "id": "abc", "filename": "GX010123.MP4", "captured_at": "2023-07-14T09:30:00Z"
        }));
        let d = dl(serde_json::json!({
            "filename": "GX010123.MP4",
            "_embedded": { "files": [
                { "url": "https://cdn/x/GX010123.MP4?s=1", "item_number": 1 },
                { "url": "https://cdn/x/GX020123.MP4?s=2", "item_number": 2 }
            ]}
        }));
        let mut claimed = HashSet::new();
        let p = plan_item(&it, &d, Variant::Source, Layout::Flat, false, &mut claimed, &HashMap::new());
        let names: Vec<_> = p.iter().map(|a| a.rel_path.to_string_lossy().to_string()).collect();
        assert_eq!(names, vec!["GX010123.MP4", "GX020123.MP4"]);
        assert_eq!(p[1].asset_key, "source:2");
    }

    #[test]
    fn colliding_names_across_items_are_disambiguated() {
        let d = dl(serde_json::json!({
            "filename": "GOPR0001.JPG",
            "_embedded": { "files": [{ "url": "https://cdn/x/GOPR0001.JPG?s=1" }] }
        }));
        let a = item(serde_json::json!({
            "id": "aaaaaaaa1111", "filename": "GOPR0001.JPG", "captured_at": "2023-07-14T09:30:00Z"
        }));
        let b = item(serde_json::json!({
            "id": "bbbbbbbb2222", "filename": "GOPR0001.JPG", "captured_at": "2023-07-14T11:00:00Z"
        }));
        let mut claimed = HashSet::new();
        let pa = plan_item(&a, &d, Variant::Source, Layout::Date, false, &mut claimed, &HashMap::new());
        let pb = plan_item(&b, &d, Variant::Source, Layout::Date, false, &mut claimed, &HashMap::new());
        assert_eq!(pa[0].rel_path, PathBuf::from("2023/2023-07-14/GOPR0001.JPG"));
        assert_eq!(pb[0].rel_path, PathBuf::from("2023/2023-07-14/GOPR0001-bbbbbbbb.JPG"));
    }

    #[test]
    fn best_falls_back_to_largest_variation_when_no_original() {
        let it = item(serde_json::json!({ "id": "abc", "filename": "GX010123.MP4" }));
        let d = dl(serde_json::json!({
            "filename": "GX010123.MP4",
            "_embedded": { "files": [], "variations": [
                { "url": "https://cdn/low.mp4?s=1", "label": "mp4_low", "width": 848, "height": 480 },
                { "url": "https://cdn/high.mp4?s=2", "label": "high_res_proxy_mp4",
                  "width": "1920", "height": "1080" }
            ]}
        }));
        let mut claimed = HashSet::new();
        let p = plan_item(&it, &d, Variant::Best, Layout::Flat, false, &mut claimed, &HashMap::new());
        assert_eq!(p.len(), 1);
        assert!(p[0].url.contains("high.mp4"));
        assert_eq!(p[0].rel_path, PathBuf::from("GX010123-high_res_proxy_mp4.mp4"));
    }

    #[test]
    fn source_variant_ignores_variations() {
        let it = item(serde_json::json!({ "id": "abc", "filename": "GX010123.MP4" }));
        let d = dl(serde_json::json!({
            "filename": "GX010123.MP4",
            "_embedded": {
                "files": [{ "url": "https://cdn/x/GX010123.MP4?s=1" }],
                "variations": [{ "url": "https://cdn/low.mp4?s=2", "label": "mp4_low" }]
            }
        }));
        let mut claimed = HashSet::new();
        let p = plan_item(&it, &d, Variant::Source, Layout::Flat, false, &mut claimed, &HashMap::new());
        assert_eq!(p.len(), 1);
        assert!(p[0].url.contains("GX010123.MP4"));
    }

    #[test]
    fn undated_media_gets_its_own_folder() {
        let it = item(serde_json::json!({ "id": "abc", "filename": "GOPR0002.JPG" }));
        let d = dl(serde_json::json!({
            "_embedded": { "files": [{ "url": "https://cdn/x/GOPR0002.JPG?s=1" }] }
        }));
        let mut claimed = HashSet::new();
        let p = plan_item(&it, &d, Variant::Source, Layout::Date, false, &mut claimed, &HashMap::new());
        assert_eq!(p[0].rel_path, PathBuf::from("undated/GOPR0002.JPG"));
    }

    #[test]
    fn restart_reuses_the_path_a_previous_run_recorded() {
        let it = item(serde_json::json!({
            "id": "abc", "filename": "GX010123.MP4", "captured_at": "2023-07-14T09:30:00Z"
        }));
        let d = dl(serde_json::json!({
            "filename": "GX010123.MP4",
            "_embedded": { "files": [{ "url": "https://cdn/x/GX010123.MP4?s=1" }] }
        }));
        let recorded = HashMap::from([(
            "source:0".to_string(),
            PathBuf::from("2023/2023-07-14/GX010123-abc.MP4"),
        )]);
        let mut claimed = HashSet::new();
        let p = plan_item(&it, &d, Variant::Source, Layout::Date, false, &mut claimed, &recorded);
        assert_eq!(p[0].rel_path, PathBuf::from("2023/2023-07-14/GX010123-abc.MP4"));
        // Fresh URL, old path: a restart re-downloads to where it already was.
        assert!(p[0].url.contains("s=1"));
    }

    #[test]
    fn collision_suffixes_survive_a_restart_in_any_order() {
        let d = dl(serde_json::json!({
            "filename": "GOPR0001.JPG",
            "_embedded": { "files": [{ "url": "https://cdn/x/GOPR0001.JPG?s=1" }] }
        }));
        let a = item(serde_json::json!({
            "id": "aaaaaaaa1111", "filename": "GOPR0001.JPG", "captured_at": "2023-07-14T09:30:00Z"
        }));
        let b = item(serde_json::json!({
            "id": "bbbbbbbb2222", "filename": "GOPR0001.JPG", "captured_at": "2023-07-14T11:00:00Z"
        }));

        // Run 1 finished A only; the manifest now claims the bare name for A.
        let mut claimed = HashSet::from([PathBuf::from("2023/2023-07-14/GOPR0001.JPG")]);
        let a_recorded = HashMap::from([(
            "source:0".to_string(),
            PathBuf::from("2023/2023-07-14/GOPR0001.JPG"),
        )]);

        // Run 2 happens to reach B first — it must still yield to A.
        let pb = plan_item(&b, &d, Variant::Source, Layout::Date, false, &mut claimed, &HashMap::new());
        let pa = plan_item(&a, &d, Variant::Source, Layout::Date, false, &mut claimed, &a_recorded);
        assert_eq!(pa[0].rel_path, PathBuf::from("2023/2023-07-14/GOPR0001.JPG"));
        assert_eq!(pb[0].rel_path, PathBuf::from("2023/2023-07-14/GOPR0001-bbbbbbbb.JPG"));
    }

    #[test]
    fn refreshed_urls_key_exactly_like_the_plan() {
        // A retry maps new signed URLs onto the existing plan by key, so the two
        // must agree on keys or a retry would silently download nothing.
        let it = item(serde_json::json!({ "id": "abc", "filename": "GX010123.MP4" }));
        let d = dl(serde_json::json!({
            "filename": "GX010123.MP4",
            "_embedded": {
                "files": [
                    { "url": "https://cdn/a.MP4?s=1", "item_number": 1 },
                    { "url": "https://cdn/b.MP4?s=1", "item_number": 2 }
                ],
                "variations": [{ "url": "https://cdn/low.mp4?s=1", "label": "mp4_low" }],
                "sidecar_files": [{ "url": "https://cdn/t.THM?s=1", "label": "thm" }]
            }
        }));
        let mut claimed = HashSet::new();
        let planned = plan_item(&it, &d, Variant::All, Layout::Flat, true, &mut claimed, &HashMap::new());
        let urls = asset_urls(&d, Variant::All, true);

        assert_eq!(planned.len(), urls.len());
        for a in &planned {
            assert_eq!(urls.get(&a.asset_key).map(String::as_str), Some(a.url.as_str()));
        }
    }
}
