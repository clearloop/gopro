//! On-disk cache of the media listing.
//!
//! Walking `/media/search` costs one request per 100 items — 29 round trips for
//! a 2835-item library — and every interrupted run pays it again before it can
//! resume. The listing is cached next to the manifest and validated against the
//! account totals from `/media/user`, which is a single cheap request: if the
//! item count and byte total are unchanged, nothing was added or removed and
//! the cached listing still describes the library.

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::manifest::STATE_DIR;
use crate::model::{MediaItem, MediaUser};

const CACHE_FILE: &str = "library.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LibraryCache {
    pub fetched_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    /// Account totals as of the fetch; these are the cache validators.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_storage: Option<u64>,
    pub items: Vec<MediaItem>,
}

pub fn path_for(dest: &Path) -> PathBuf {
    dest.join(STATE_DIR).join(CACHE_FILE)
}

pub fn load(dest: &Path) -> Option<LibraryCache> {
    let raw = std::fs::read(path_for(dest)).ok()?;
    serde_json::from_slice(&raw).ok()
}

pub fn save(dest: &Path, cache: &LibraryCache) -> Result<()> {
    let p = path_for(dest);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = p.with_extension("json.tmp");
    // Compact, not pretty: this runs to ~1 MB for a few thousand items.
    std::fs::write(&tmp, serde_json::to_vec(cache)?)
        .with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &p)?;
    Ok(())
}

impl LibraryCache {
    /// Why this cache cannot be trusted, or `None` if it can.
    ///
    /// `user` is the freshly-read account summary. When it is absent (the call
    /// failed) only age is checked, so a listing outage does not force a
    /// 29-request re-walk.
    pub fn staleness(&self, user: Option<&MediaUser>, ttl: Duration) -> Option<String> {
        let age = Utc::now() - self.fetched_at;
        if age > ttl {
            return Some(format!(
                "cached listing is {} old",
                approx_duration(age)
            ));
        }

        // Without an account summary to compare against, age is all we have.
        if let Some(user) = user {
            if let (Some(now), Some(then)) = (&user.id, &self.account_id) {
                if now != then {
                    return Some("cached listing belongs to a different account".into());
                }
            }
            if let (Some(now), Some(then)) = (user.total_count, self.total_count) {
                if now != then {
                    return Some(format!("library changed: {then} -> {now} items"));
                }
            }
            if let (Some(now), Some(then)) = (user.total_storage, self.total_storage) {
                if now != then {
                    return Some("library size changed since the listing was cached".into());
                }
            }
        }
        // An item that was still uploading may have become downloadable without
        // moving either total, so do not serve it from cache indefinitely.
        if self.items.iter().any(|i| {
            i.ready_to_view
                .as_deref()
                .is_some_and(|s| !s.eq_ignore_ascii_case("ready"))
        }) && age > Duration::hours(1)
        {
            return Some("items were still processing when this listing was cached".into());
        }
        None
    }
}

fn approx_duration(d: Duration) -> String {
    let mins = d.num_minutes().max(0);
    if mins < 90 {
        format!("{mins}m")
    } else if mins < 60 * 48 {
        format!("{}h", mins / 60)
    } else {
        format!("{}d", mins / (60 * 24))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(ready: &str) -> MediaItem {
        serde_json::from_value(serde_json::json!({
            "id": "abc", "filename": "GX01.MP4", "ready_to_view": ready
        }))
        .unwrap()
    }

    fn cache(age_mins: i64, count: u64, ready: &str) -> LibraryCache {
        LibraryCache {
            fetched_at: Utc::now() - Duration::minutes(age_mins),
            account_id: Some("acct-1".into()),
            total_count: Some(count),
            total_storage: Some(1000),
            items: vec![item(ready)],
        }
    }

    fn user(id: &str, count: u64, storage: u64) -> MediaUser {
        serde_json::from_value(serde_json::json!({
            "id": id, "total_count": count, "total_storage": storage
        }))
        .unwrap()
    }

    const TTL: Duration = Duration::hours(24);

    #[test]
    fn an_unchanged_library_serves_from_cache() {
        let c = cache(30, 2838, "ready");
        assert_eq!(c.staleness(Some(&user("acct-1", 2838, 1000)), TTL), None);
    }

    #[test]
    fn a_changed_item_count_invalidates() {
        let c = cache(30, 2838, "ready");
        let why = c.staleness(Some(&user("acct-1", 2839, 1000)), TTL).unwrap();
        assert!(why.contains("2838 -> 2839"), "{why}");
    }

    #[test]
    fn a_changed_byte_total_invalidates() {
        // Same count, different bytes: an item was replaced.
        let c = cache(30, 2838, "ready");
        assert!(c.staleness(Some(&user("acct-1", 2838, 2000)), TTL).is_some());
    }

    #[test]
    fn a_different_account_invalidates() {
        let c = cache(30, 2838, "ready");
        assert!(c.staleness(Some(&user("acct-2", 2838, 1000)), TTL).is_some());
    }

    #[test]
    fn age_alone_invalidates() {
        let c = cache(60 * 30, 2838, "ready");
        assert!(c.staleness(Some(&user("acct-1", 2838, 1000)), TTL).is_some());
    }

    #[test]
    fn a_listing_outage_still_serves_a_fresh_cache() {
        let c = cache(30, 2838, "ready");
        assert_eq!(c.staleness(None, TTL), None);
    }

    #[test]
    fn items_that_were_still_uploading_expire_quickly() {
        // Totals do not move when transcoding finishes, so age is the only
        // signal that a pending item may now be downloadable.
        let fresh = cache(10, 2838, "uploading");
        assert_eq!(fresh.staleness(Some(&user("acct-1", 2838, 1000)), TTL), None);

        let older = cache(180, 2838, "uploading");
        let why = older.staleness(Some(&user("acct-1", 2838, 1000)), TTL).unwrap();
        assert!(why.contains("still processing"), "{why}");
    }
}
