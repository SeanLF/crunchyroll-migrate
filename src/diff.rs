use crate::import::fetch_target_state;
use crate::models::{self, CrunchylistsExport, RatingsExport, WatchHistoryExport, WatchlistExport};
use anyhow::Result;
use crunchyroll_rs::Crunchyroll;
use std::collections::HashSet;
use std::path::Path;

pub struct DiffResult {
    pub watchlist: DiffCounts,
    pub history: DiffCounts,
    pub crunchylists: DiffCounts,
    pub ratings: DiffCounts,
}

pub struct DiffCounts {
    pub in_export: usize,
    pub on_target: usize,
    pub missing: usize,
    pub already_there: usize,
}

pub async fn run(crunchy: &Crunchyroll, input_dir: &Path) -> Result<()> {
    let result = compute_diff(crunchy, input_dir).await?;
    print_diff_table(&result);
    Ok(())
}

pub async fn compute_diff(crunchy: &Crunchyroll, input_dir: &Path) -> Result<DiffResult> {
    let watchlist_export: WatchlistExport = models::read_export(input_dir, "watchlist.json")?;
    let history_export: WatchHistoryExport = models::read_export(input_dir, "watch_history.json")?;
    let crunchylists_export: CrunchylistsExport =
        models::read_export(input_dir, "crunchylists.json")?;
    let ratings_export: RatingsExport = models::read_export(input_dir, "ratings.json")?;

    let target = fetch_target_state(crunchy).await?;

    // Compute diffs
    let export_wl_ids: HashSet<&str> = watchlist_export
        .items
        .iter()
        .map(|i| i.content_id.as_str())
        .collect();
    let wl_already = export_wl_ids
        .iter()
        .filter(|id| target.watchlist_ids.contains(**id))
        .count();

    let export_hist_ids: HashSet<&str> = history_export
        .items
        .iter()
        .map(|i| i.content_id.as_str())
        .collect();
    let hist_already = export_hist_ids
        .iter()
        .filter(|id| target.history_ids.contains(**id))
        .count();

    // Count crunchylist items, checking per-item presence on target
    let export_list_count: usize = crunchylists_export
        .lists
        .iter()
        .map(|l| l.items.len())
        .sum();
    let list_already: usize = crunchylists_export
        .lists
        .iter()
        .flat_map(|l| {
            let target_items = target.crunchylists.get(&l.name);
            l.items
                .iter()
                .filter(move |item| target_items.is_some_and(|ids| ids.contains(&item.content_id)))
        })
        .count();

    let ratings_count = ratings_export.items.len();

    Ok(DiffResult {
        watchlist: DiffCounts {
            in_export: export_wl_ids.len(),
            on_target: target.watchlist_ids.len(),
            missing: export_wl_ids.len() - wl_already,
            already_there: wl_already,
        },
        history: DiffCounts {
            in_export: export_hist_ids.len(),
            on_target: target.history_ids.len(),
            missing: export_hist_ids.len() - hist_already,
            already_there: hist_already,
        },
        crunchylists: DiffCounts {
            in_export: export_list_count,
            on_target: target.crunchylists.values().map(|s| s.len()).sum(),
            missing: export_list_count - list_already,
            already_there: list_already,
        },
        ratings: DiffCounts {
            in_export: ratings_count,
            on_target: 0,
            missing: ratings_count,
            already_there: 0,
        },
    })
}

fn print_diff_table(result: &DiffResult) {
    println!();
    println!(
        "  {:<14} {:>10} {:>10} {:>10} {:>14}",
        "Data Type", "In Export", "On Target", "Missing", "Already There"
    );
    println!("  {}", "─".repeat(62));

    let rows = [
        ("Watchlist", &result.watchlist),
        ("History", &result.history),
        ("Crunchylists", &result.crunchylists),
        ("Ratings", &result.ratings),
    ];

    for (name, counts) in rows {
        println!(
            "  {:<14} {:>10} {:>10} {:>10} {:>14}",
            name, counts.in_export, counts.on_target, counts.missing, counts.already_there
        );
    }
    println!();
}
