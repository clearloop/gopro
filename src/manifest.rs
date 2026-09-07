//! Append-only record of what has landed on disk, so `sync` is resumable and
//! idempotent across runs — the whole point when the library is bigger than the
//! time you have before the subscription lapses.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

pub const STATE_DIR: &str = ".gopro-dl";
const MANIFEST_FILE: &str = "manifest.json";
const PARTS_DIR: &str = "parts";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub media_id: String,
    pub asset_key: String,
    pub rel_path: String,
    pub bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub captured_at: Option<String>,
    pub completed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_sync: Option<DateTime<Utc>>,
    /// What the cloud library held at the last successful listing, so `stats`
    /// can report progress — in items *and* bytes — without hitting the network.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub library_items: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub library_bytes: Option<u64>,
    /// Media id -> the `--variant`/`--sidecars` profile under which *every*
    /// asset of that item was successfully fetched.
    ///
    /// Recorded at the item level, and only on full success. Per-file records
    /// cannot express this: an item whose second asset failed would have a
    /// first file that looks complete, and would be skipped forever.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub completed: BTreeMap<String, String>,
    /// Keyed by `{media_id}::{asset_key}` — one media item can yield several
    /// files (chapters, bursts, sidecars).
    pub entries: BTreeMap<String, Entry>,
}

impl Default for Manifest {
    fn default() -> Self {
        Self {
            version: 1,
            last_sync: None,
            library_items: None,
            library_bytes: None,
            completed: BTreeMap::new(),
            entries: BTreeMap::new(),
        }
    }
}

pub fn key(media_id: &str, asset_key: &str) -> String {
    format!("{media_id}::{asset_key}")
}

pub fn path_for(dest: &Path) -> PathBuf {
    dest.join(STATE_DIR).join(MANIFEST_FILE)
}

pub fn parts_dir(dest: &Path) -> PathBuf {
    dest.join(STATE_DIR).join(PARTS_DIR)
}

/// Partials live in a private directory keyed by media id + asset, never beside
/// the final file. Two media items that share a camera filename (cameras reset
/// their counters, so `GOPR0001.JPG` recurs across years) would otherwise
/// resume into each other's bytes after an interrupt.
pub fn part_for(dest: &Path, media_id: &str, asset_key: &str) -> PathBuf {
    let safe: String = asset_key
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' })
        .collect();
    parts_dir(dest).join(format!("{media_id}__{safe}.part"))
}

/// Partial downloads left behind by an interrupted run, as (path, bytes).
pub fn partials(dest: &Path) -> Vec<(PathBuf, u64)> {
    let Ok(rd) = std::fs::read_dir(parts_dir(dest)) else {
        return Vec::new();
    };
    rd.flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "part"))
        .filter_map(|e| e.metadata().ok().map(|m| (e.path(), m.len())))
        .collect()
}

impl Manifest {
    pub fn load(dest: &Path) -> Result<Self> {
        let p = path_for(dest);
        match std::fs::read_to_string(&p) {
            Ok(raw) => serde_json::from_str(&raw)
                .with_context(|| format!("{} is corrupt; move it aside to rebuild", p.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", p.display())),
        }
    }

    pub fn save(&self, dest: &Path) -> Result<()> {
        let p = path_for(dest);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = p.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)
            .with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &p)?;
        Ok(())
    }

    /// True when the entry exists *and* the file is still on disk at the right
    /// size — catches an external disk that was wiped or partially restored.
    pub fn is_done(&self, dest: &Path, media_id: &str, asset_key: &str) -> bool {
        let Some(entry) = self.entries.get(&key(media_id, asset_key)) else {
            return false;
        };
        match std::fs::metadata(dest.join(&entry.rel_path)) {
            Ok(m) => m.len() == entry.bytes,
            Err(_) => false,
        }
    }

    /// Every path this archive has already committed to. Seeding a run with
    /// this keeps filename-collision suffixes stable across restarts.
    pub fn claimed_paths(&self) -> HashSet<PathBuf> {
        self.entries.values().map(|e| PathBuf::from(&e.rel_path)).collect()
    }

    /// Every entry belonging to one media item, without scanning the whole map.
    fn entries_for<'a>(&'a self, media_id: &str) -> impl Iterator<Item = &'a Entry> + 'a {
        let prefix = format!("{media_id}::");
        self.entries
            .range(prefix.clone()..)
            .take_while(move |(k, _)| k.starts_with(&prefix))
            .map(|(_, e)| e)
    }

    /// Paths already recorded for one media item, keyed by asset. A restart
    /// reuses these instead of re-deriving a name that may differ.
    pub fn recorded_for(&self, media_id: &str) -> HashMap<String, PathBuf> {
        self.entries_for(media_id)
            .map(|e| (e.asset_key.clone(), PathBuf::from(&e.rel_path)))
            .collect()
    }

    /// True when this item needs no work at all, so the run can skip it without
    /// asking GoPro for download links — the difference between 2835 pointless
    /// API calls on a resumed run and none.
    ///
    /// Requires the recorded files to still be present at their recorded sizes,
    /// which is cheap (one `stat` each) and catches a wiped or half-restored
    /// disk that the manifest alone would happily lie about.
    pub fn item_complete(&self, dest: &Path, media_id: &str, profile: &str) -> bool {
        if self.completed.get(media_id).map(String::as_str) != Some(profile) {
            return false;
        }
        let mut saw_any = false;
        for entry in self.entries_for(media_id) {
            saw_any = true;
            match std::fs::metadata(dest.join(&entry.rel_path)) {
                Ok(m) if m.len() == entry.bytes => {}
                _ => return false,
            }
        }
        saw_any
    }

    /// Call only when every asset planned for this item is on disk.
    pub fn mark_complete(&mut self, media_id: &str, profile: &str) {
        self.completed.insert(media_id.to_string(), profile.to_string());
    }

    /// Distinct media items with at least one file on record.
    pub fn media_count(&self) -> usize {
        self.entries
            .values()
            .map(|e| e.media_id.as_str())
            .collect::<HashSet<_>>()
            .len()
    }

    pub fn insert(&mut self, entry: Entry) {
        self.entries.insert(key(&entry.media_id, &entry.asset_key), entry);
    }

    pub fn total_bytes(&self) -> u64 {
        self.entries.values().map(|e| e.bytes).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(rel: &str, bytes: u64) -> Entry {
        Entry {
            media_id: "abc".into(),
            asset_key: "source:0".into(),
            rel_path: rel.into(),
            bytes,
            sha256: None,
            captured_at: Some("2023-07-14T09:30:00Z".into()),
            completed_at: Utc::now(),
        }
    }

    #[test]
    fn round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let mut m = Manifest::default();
        m.insert(entry("2023/2023-07-14/GX010123.MP4", 42));
        m.save(dir.path()).unwrap();

        let loaded = Manifest::load(dir.path()).unwrap();
        assert_eq!(loaded.entries.len(), 1);
        assert_eq!(loaded.total_bytes(), 42);
    }

    #[test]
    fn missing_manifest_is_an_empty_one() {
        let dir = tempfile::tempdir().unwrap();
        assert!(Manifest::load(dir.path()).unwrap().entries.is_empty());
    }

    #[test]
    fn done_requires_the_file_to_still_be_there_at_the_right_size() {
        let dir = tempfile::tempdir().unwrap();
        let mut m = Manifest::default();
        m.insert(entry("a/GX010123.MP4", 3));
        assert!(!m.is_done(dir.path(), "abc", "source:0"), "no file on disk yet");

        std::fs::create_dir_all(dir.path().join("a")).unwrap();
        std::fs::write(dir.path().join("a/GX010123.MP4"), b"abc").unwrap();
        assert!(m.is_done(dir.path(), "abc", "source:0"));

        // Truncated by a bad unmount: must be re-downloaded, not skipped.
        std::fs::write(dir.path().join("a/GX010123.MP4"), b"a").unwrap();
        assert!(!m.is_done(dir.path(), "abc", "source:0"));
    }

    #[test]
    fn a_complete_item_needs_no_network_call() {
        let dir = tempfile::tempdir().unwrap();
        let mut m = Manifest::default();
        m.insert(entry("a/GX010123.MP4", 3));
        std::fs::create_dir_all(dir.path().join("a")).unwrap();
        std::fs::write(dir.path().join("a/GX010123.MP4"), b"abc").unwrap();

        assert!(!m.item_complete(dir.path(), "abc", "source"), "not marked complete yet");
        m.mark_complete("abc", "source");
        assert!(m.item_complete(dir.path(), "abc", "source"));
        assert!(!m.item_complete(dir.path(), "never-seen", "source"));
    }

    #[test]
    fn changing_the_variant_reopens_the_item() {
        // Recorded under `source`; asking for `all` may need more assets, so the
        // item must not be skipped on the strength of the original alone.
        let dir = tempfile::tempdir().unwrap();
        let mut m = Manifest::default();
        m.insert(entry("a/GX010123.MP4", 3));
        std::fs::create_dir_all(dir.path().join("a")).unwrap();
        std::fs::write(dir.path().join("a/GX010123.MP4"), b"abc").unwrap();
        m.mark_complete("abc", "source");

        assert!(m.item_complete(dir.path(), "abc", "source"));
        assert!(!m.item_complete(dir.path(), "abc", "all"));
        assert!(!m.item_complete(dir.path(), "abc", "source+sidecars"));
    }

    #[test]
    fn a_vanished_file_reopens_the_item() {
        let dir = tempfile::tempdir().unwrap();
        let mut m = Manifest::default();
        m.insert(entry("a/GX010123.MP4", 3));
        m.mark_complete("abc", "source");
        assert!(!m.item_complete(dir.path(), "abc", "source"), "file was never written");
    }

    #[test]
    fn one_missing_asset_reopens_the_whole_item() {
        let dir = tempfile::tempdir().unwrap();
        let mut m = Manifest::default();
        m.insert(entry("a/GX010123.MP4", 3));
        let mut second = entry("a/GX010123-proxy.mp4", 3);
        second.asset_key = "edit_proxy:0".into();
        m.insert(second);
        std::fs::create_dir_all(dir.path().join("a")).unwrap();
        std::fs::write(dir.path().join("a/GX010123.MP4"), b"abc").unwrap();
        m.mark_complete("abc", "source");
        // The proxy is absent.
        assert!(!m.item_complete(dir.path(), "abc", "source"));
    }

    #[test]
    fn entries_for_does_not_leak_across_media_ids() {
        let mut m = Manifest::default();
        let mut a = entry("a.MP4", 1);
        a.media_id = "abc".into();
        let mut b = entry("b.MP4", 1);
        b.media_id = "abcdef".into();
        m.insert(a);
        m.insert(b);
        assert_eq!(m.recorded_for("abc").len(), 1, "prefix must not match abcdef");
        assert_eq!(m.recorded_for("abcdef").len(), 1);
    }

    #[test]
    fn a_partly_failed_item_is_never_marked_complete() {
        // The regression this design exists for: one asset lands, a second
        // fails, so the item must stay open rather than looking finished.
        let dir = tempfile::tempdir().unwrap();
        let mut m = Manifest::default();
        m.insert(entry("a/GX010123.MP4", 3));
        std::fs::create_dir_all(dir.path().join("a")).unwrap();
        std::fs::write(dir.path().join("a/GX010123.MP4"), b"abc").unwrap();

        // mark_complete is not called, because the run did not finish the item.
        assert!(!m.item_complete(dir.path(), "abc", "all"));
        assert!(!m.item_complete(dir.path(), "abc", "source"));
    }

    #[test]
    fn an_old_manifest_without_completion_records_is_rechecked_once() {
        let dir = tempfile::tempdir().unwrap();
        let raw = r#"{"version":1,"entries":{"abc::original:0":{
            "media_id":"abc","asset_key":"original:0","rel_path":"a/GX010123.MP4",
            "bytes":3,"completed_at":"2026-01-01T00:00:00Z"}}}"#;
        std::fs::create_dir_all(dir.path().join(STATE_DIR)).unwrap();
        std::fs::write(path_for(dir.path()), raw).unwrap();

        let m = Manifest::load(dir.path()).unwrap();
        assert_eq!(m.entries.len(), 1, "old entries still load");
        assert!(m.completed.is_empty());
        assert!(!m.item_complete(dir.path(), "abc", "source"), "must re-check once");
    }
}
