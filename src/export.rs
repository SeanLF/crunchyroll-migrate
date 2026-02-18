use crate::models::{
    CrunchylistData, CrunchylistItem, CrunchylistsExport, ExportMetadata, RatingItem,
    RatingsExport, WatchHistoryExport, WatchHistoryItem, WatchlistExport, WatchlistItem,
};
use crate::ui::{self, DataType, ProgressReporter, ProgressUpdate};
use anyhow::{Context, Result};
use chrono::Utc;
use crunchyroll_rs::list::WatchlistOptions;
use crunchyroll_rs::{Crunchyroll, MediaCollection};
use futures_util::StreamExt;
use std::collections::HashSet;
use std::path::Path;
use tokio::sync::Semaphore;

pub async fn run(crunchy: &Crunchyroll, output_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(output_dir)?;

    let profile_name = crunchy.profile_id().await;
    let (reporter, dashboard) = ui::start_dashboard("Export", "", &profile_name);

    let watchlist = export_watchlist(crunchy, &profile_name, &reporter).await?;
    write_atomic(output_dir, "watchlist.json", &watchlist)?;
    reporter.log_success(&format!("Watchlist: {} items", watchlist.items.len()));

    let history = export_history(crunchy, &profile_name, &reporter).await?;
    write_atomic(output_dir, "watch_history.json", &history)?;
    reporter.log_success(&format!("Watch history: {} items", history.items.len()));

    let crunchylists = export_crunchylists(crunchy, &profile_name, &reporter).await?;
    write_atomic(output_dir, "crunchylists.json", &crunchylists)?;
    let list_items: usize = crunchylists.lists.iter().map(|l| l.items.len()).sum();
    reporter.log_success(&format!(
        "Crunchylists: {} lists, {} items",
        crunchylists.lists.len(),
        list_items
    ));

    let ratings = export_ratings(
        crunchy,
        &profile_name,
        &watchlist.items,
        &history.items,
        &reporter,
    )
    .await?;
    write_atomic(output_dir, "ratings.json", &ratings)?;
    reporter.log_success(&format!("Ratings: {} rated items", ratings.items.len()));

    reporter.done();
    dashboard.wait();

    if !ui::is_tty() {
        println!("Export complete -> {}", output_dir.display());
    }
    Ok(())
}

async fn export_watchlist(
    crunchy: &Crunchyroll,
    profile_name: &str,
    reporter: &ProgressReporter,
) -> Result<WatchlistExport> {
    let entries = crunchy
        .watchlist(WatchlistOptions::default())
        .await
        .context("Failed to fetch watchlist")?;

    let items: Vec<WatchlistItem> = entries
        .iter()
        .filter_map(|entry| {
            let (content_id, title, slug, content_type) = extract_series_info(&entry.panel)?;
            Some(WatchlistItem {
                content_id,
                title,
                slug,
                content_type,
                is_favourite: entry.is_favorite,
                fully_watched: entry.fully_watched,
            })
        })
        .collect();

    reporter.progress(export_progress(DataType::Watchlist, items.len()));

    Ok(WatchlistExport {
        metadata: ExportMetadata {
            profile_name: profile_name.to_string(),
            exported_at: Utc::now(),
            total_count: items.len(),
        },
        items,
    })
}

async fn export_history(
    crunchy: &Crunchyroll,
    profile_name: &str,
    reporter: &ProgressReporter,
) -> Result<WatchHistoryExport> {
    let mut stream = crunchy.watch_history();
    let mut items = Vec::new();
    let mut failed = 0;

    while let Some(result) = stream.next().await {
        match result {
            Ok(entry) => {
                let (title, series_title, partial) = match &entry.panel {
                    Some(panel) => {
                        let title = panel_title(panel);
                        let series_title = panel_series_title(panel);
                        (title, series_title, false)
                    }
                    None => (String::new(), String::new(), true),
                };

                items.push(WatchHistoryItem {
                    content_id: entry.id.clone(),
                    parent_id: entry.parent_id.clone(),
                    parent_type: entry.parent_type.clone(),
                    title,
                    series_title,
                    date_played: entry.date_played,
                    playhead: entry.playhead,
                    fully_watched: entry.fully_watched,
                    partial,
                });

                if items.len() % 50 == 0 {
                    reporter.progress(ProgressUpdate {
                        data_type: DataType::History,
                        total: 0,
                        processed: items.len(),
                        added: items.len(),
                        skipped: 0,
                        already_present: 0,
                        failed,
                    });
                }
            }
            Err(e) => {
                reporter.log_error(&format!("Skipping history entry: {}", e));
                failed += 1;
            }
        }
    }

    // Sort chronologically (oldest first)
    items.sort_by_key(|a| a.date_played);

    reporter.progress(ProgressUpdate {
        data_type: DataType::History,
        total: items.len(),
        processed: items.len(),
        added: items.len(),
        skipped: 0,
        already_present: 0,
        failed,
    });

    Ok(WatchHistoryExport {
        metadata: ExportMetadata {
            profile_name: profile_name.to_string(),
            exported_at: Utc::now(),
            total_count: items.len(),
        },
        items,
    })
}

async fn export_crunchylists(
    crunchy: &Crunchyroll,
    profile_name: &str,
    reporter: &ProgressReporter,
) -> Result<CrunchylistsExport> {
    let lists_meta = crunchy
        .crunchylists()
        .await
        .context("Failed to fetch crunchylists")?;

    let mut lists = Vec::new();
    for preview in &lists_meta.items {
        let full_list = preview
            .crunchylist()
            .await
            .with_context(|| format!("Failed to fetch crunchylist '{}'", preview.title))?;

        let items: Vec<CrunchylistItem> = full_list
            .items
            .iter()
            .filter_map(|entry| {
                let (content_id, title, _, _) = extract_series_info(&entry.panel)?;
                Some(CrunchylistItem { content_id, title })
            })
            .collect();

        lists.push(CrunchylistData {
            name: preview.title.clone(),
            items,
        });
    }

    let total: usize = lists.iter().map(|l| l.items.len()).sum();
    reporter.progress(export_progress(DataType::Crunchylists, total));

    Ok(CrunchylistsExport {
        metadata: ExportMetadata {
            profile_name: profile_name.to_string(),
            exported_at: Utc::now(),
            total_count: total,
        },
        lists,
    })
}

async fn export_ratings(
    crunchy: &Crunchyroll,
    profile_name: &str,
    watchlist: &[WatchlistItem],
    history: &[WatchHistoryItem],
    reporter: &ProgressReporter,
) -> Result<RatingsExport> {
    // Collect unique series/movie_listing IDs and their types
    let mut seen: HashSet<String> = HashSet::new();
    let mut to_check: Vec<(String, String, String)> = Vec::new(); // (id, content_type, title)

    for item in watchlist {
        if seen.insert(item.content_id.clone()) {
            to_check.push((
                item.content_id.clone(),
                item.content_type.clone(),
                item.title.clone(),
            ));
        }
    }

    for item in history {
        if seen.insert(item.parent_id.clone()) {
            to_check.push((
                item.parent_id.clone(),
                item.parent_type.clone(),
                item.series_title.clone(),
            ));
        }
    }

    let total = to_check.len();
    let semaphore = std::sync::Arc::new(Semaphore::new(5));
    let crunchy_clone = crunchy.clone();
    let mut handles = Vec::new();

    for (content_id, content_type, title) in to_check {
        let sem = semaphore.clone();
        let cr = crunchy_clone.clone();

        handles.push(tokio::spawn(async move {
            let Ok(_permit) = sem.acquire().await else {
                return None;
            };
            fetch_rating(&cr, &content_id, &content_type, &title).await
        }));
    }

    let mut items = Vec::new();
    let mut checked = 0;
    for handle in handles {
        checked += 1;
        match handle.await {
            Ok(Some(item)) => items.push(item),
            Ok(None) => {}
            Err(e) => reporter.log_error(&format!("Rating task failed: {}", e)),
        }
        if checked % 5 == 0 || checked == total {
            reporter.progress(ProgressUpdate {
                data_type: DataType::Ratings,
                total,
                processed: checked,
                added: items.len(),
                skipped: 0,
                already_present: 0,
                failed: 0,
            });
        }
    }

    Ok(RatingsExport {
        metadata: ExportMetadata {
            profile_name: profile_name.to_string(),
            exported_at: Utc::now(),
            total_count: items.len(),
        },
        items,
    })
}

async fn fetch_rating(
    crunchy: &Crunchyroll,
    content_id: &str,
    content_type: &str,
    title: &str,
) -> Option<RatingItem> {
    let rating_result = match content_type {
        "series" => {
            let series: crunchyroll_rs::Series = match crunchy.media_from_id(content_id).await {
                Ok(s) => s,
                Err(_) => return None,
            };
            series.rating().await
        }
        "movie_listing" => {
            let ml: crunchyroll_rs::MovieListing = match crunchy.media_from_id(content_id).await {
                Ok(m) => m,
                Err(_) => return None,
            };
            ml.rating().await
        }
        _ => return None,
    };

    match rating_result {
        Ok(rating) => rating.rating.map(|stars| RatingItem {
            content_id: content_id.to_string(),
            content_type: content_type.to_string(),
            title: title.to_string(),
            rating: format!("{:?}", stars),
        }),
        Err(_) => None,
    }
}

/// Extract series/movie_listing ID, title, slug, and content_type from a MediaCollection panel.
pub fn extract_series_info(panel: &MediaCollection) -> Option<(String, String, String, String)> {
    match panel {
        MediaCollection::Episode(ep) => Some((
            ep.series_id.clone(),
            ep.series_title.clone(),
            ep.series_slug_title.clone(),
            "series".to_string(),
        )),
        MediaCollection::Movie(mv) => Some((
            mv.movie_listing_id.clone(),
            mv.movie_listing_title.clone(),
            mv.movie_listing_slug_title.clone(),
            "movie_listing".to_string(),
        )),
        MediaCollection::Series(s) => Some((
            s.id.clone(),
            s.title.clone(),
            s.slug_title.clone(),
            "series".to_string(),
        )),
        MediaCollection::MovieListing(ml) => Some((
            ml.id.clone(),
            ml.title.clone(),
            ml.slug_title.clone(),
            "movie_listing".to_string(),
        )),
        _ => None,
    }
}

fn panel_title(panel: &MediaCollection) -> String {
    match panel {
        MediaCollection::Episode(ep) => ep.title.clone(),
        MediaCollection::Movie(mv) => mv.title.clone(),
        MediaCollection::Series(s) => s.title.clone(),
        MediaCollection::MovieListing(ml) => ml.title.clone(),
        _ => String::new(),
    }
}

fn panel_series_title(panel: &MediaCollection) -> String {
    match panel {
        MediaCollection::Episode(ep) => ep.series_title.clone(),
        MediaCollection::Movie(mv) => mv.movie_listing_title.clone(),
        MediaCollection::Series(s) => s.title.clone(),
        MediaCollection::MovieListing(ml) => ml.title.clone(),
        _ => String::new(),
    }
}

/// Build a progress update for an export phase where all items are "added" (fetched).
fn export_progress(data_type: DataType, count: usize) -> ProgressUpdate {
    ProgressUpdate {
        data_type,
        total: count,
        processed: count,
        added: count,
        skipped: 0,
        already_present: 0,
        failed: 0,
    }
}

fn write_atomic<T: serde::Serialize>(dir: &Path, filename: &str, data: &T) -> Result<()> {
    let target = dir.join(filename);
    let tmp = dir.join(format!(".{}.tmp", filename));
    let json = serde_json::to_string_pretty(data)?;
    std::fs::write(&tmp, &json).with_context(|| format!("Failed to write {}", tmp.display()))?;
    std::fs::rename(&tmp, &target)
        .with_context(|| format!("Failed to rename {} -> {}", tmp.display(), target.display()))?;
    Ok(())
}
