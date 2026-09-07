//! Wire types for the (unofficial) GoPro cloud media API.
//!
//! The API is not documented publicly, so every field here is optional and
//! tolerant: GoPro changes shapes without notice and a missing field must never
//! abort a multi-terabyte archive run.

use serde::{Deserialize, Serialize};

/// `file_size` and friends arrive as either a JSON number or a quoted string
/// depending on endpoint and account age.
pub fn loose_u64(v: &Option<serde_json::Value>) -> Option<u64> {
    match v.as_ref()? {
        serde_json::Value::Number(n) => n.as_u64(),
        serde_json::Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct MediaSearchResponse {
    #[serde(rename = "_embedded", default)]
    pub embedded: SearchEmbedded,
    #[serde(rename = "_pages")]
    pub pages: Option<Pages>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[allow(dead_code)] // `errors` is kept to document the wire shape
pub struct SearchEmbedded {
    #[serde(default)]
    pub media: Vec<MediaItem>,
    #[serde(default)]
    pub errors: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[allow(dead_code)] // pagination echo fields; kept for debugging
pub struct Pages {
    #[serde(default)]
    pub current_page: u32,
    #[serde(default)]
    pub per_page: u32,
    #[serde(default)]
    pub total_items: u64,
    #[serde(default)]
    pub total_pages: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaItem {
    pub id: String,
    #[serde(default)]
    pub filename: Option<String>,
    #[serde(default)]
    pub file_extension: Option<String>,
    #[serde(default)]
    pub captured_at: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub content_title: Option<String>,
    #[serde(default)]
    pub content_type: Option<String>,
    #[serde(default)]
    pub file_size: Option<serde_json::Value>,
    #[serde(default)]
    pub item_count: Option<u32>,
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    #[serde(default)]
    pub resolution: Option<String>,
    #[serde(default)]
    pub source_duration: Option<serde_json::Value>,
    #[serde(default)]
    pub ready_to_view: Option<String>,
    #[serde(default)]
    pub moments_count: Option<u32>,
}

impl MediaItem {
    pub fn size(&self) -> Option<u64> {
        loose_u64(&self.file_size)
    }

    /// Best-effort capture timestamp, falling back to upload time.
    pub fn timestamp(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        let raw = self.captured_at.as_ref().or(self.created_at.as_ref())?;
        chrono::DateTime::parse_from_rfc3339(raw)
            .ok()
            .map(|dt| dt.with_timezone(&chrono::Utc))
    }

    /// Filename as GoPro knows it, e.g. `GX010123.MP4`.
    pub fn display_name(&self) -> String {
        if let Some(f) = self.filename.as_ref().filter(|f| !f.is_empty()) {
            return f.clone();
        }
        match self.file_extension.as_deref() {
            Some(ext) if !ext.is_empty() => format!("{}.{}", self.id, ext),
            _ => self.id.clone(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct DownloadResponse {
    #[serde(default)]
    pub filename: Option<String>,
    #[serde(rename = "_embedded", default)]
    pub embedded: DownloadEmbedded,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct DownloadEmbedded {
    /// Camera-original files. Chaptered videos and burst/timelapse groups have
    /// more than one entry, distinguished by `item_number`.
    #[serde(default)]
    pub files: Vec<Asset>,
    /// Cloud-side transcodes (`mp4_low`, `concat`, ...).
    #[serde(default)]
    pub variations: Vec<Asset>,
    /// LRV / THM / GPMF companions when the account has them.
    #[serde(default)]
    pub sidecar_files: Vec<Asset>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Asset {
    pub url: String,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub quality: Option<String>,
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    #[serde(default)]
    pub item_number: Option<u32>,
    #[serde(default)]
    pub width: Option<serde_json::Value>,
    #[serde(default)]
    pub height: Option<serde_json::Value>,
    #[serde(default)]
    pub file_extension: Option<String>,
}

impl Asset {
    pub fn pixels(&self) -> u64 {
        let w = loose_u64(&self.width).unwrap_or(0);
        let h = loose_u64(&self.height).unwrap_or(0);
        w * h
    }

    /// Stable-ish tag used to key manifest entries and to disambiguate names.
    pub fn tag(&self) -> String {
        self.label
            .clone()
            .or_else(|| self.quality.clone())
            .or_else(|| self.kind.clone())
            .unwrap_or_else(|| "asset".to_string())
    }
}
