use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::Path;

pub fn read_export<T: serde::de::DeserializeOwned>(dir: &Path, filename: &str) -> Result<T> {
    let path = dir.join(filename);
    let content =
        std::fs::read_to_string(&path).with_context(|| format!("Reading {}", path.display()))?;
    serde_json::from_str(&content).with_context(|| format!("Parsing {}", path.display()))
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ExportMetadata {
    pub profile_name: String,
    pub exported_at: DateTime<Utc>,
    pub total_count: usize,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WatchlistExport {
    pub metadata: ExportMetadata,
    pub items: Vec<WatchlistItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatchlistItem {
    pub content_id: String,
    pub title: String,
    pub slug: String,
    pub content_type: String,
    pub is_favourite: bool,
    pub fully_watched: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WatchHistoryExport {
    pub metadata: ExportMetadata,
    pub items: Vec<WatchHistoryItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatchHistoryItem {
    pub content_id: String,
    pub parent_id: String,
    pub parent_type: String,
    pub title: String,
    pub series_title: String,
    pub date_played: DateTime<Utc>,
    pub playhead: u32,
    pub fully_watched: bool,
    #[serde(default)]
    pub partial: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CrunchylistsExport {
    pub metadata: ExportMetadata,
    pub lists: Vec<CrunchylistData>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrunchylistData {
    pub name: String,
    pub items: Vec<CrunchylistItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrunchylistItem {
    pub content_id: String,
    pub title: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RatingsExport {
    pub metadata: ExportMetadata,
    pub items: Vec<RatingItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RatingItem {
    pub content_id: String,
    pub content_type: String,
    pub title: String,
    pub rating: String,
}
