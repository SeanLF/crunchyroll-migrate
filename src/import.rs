use crate::models::{
    self, CrunchylistsExport, RatingItem, RatingsExport, WatchHistoryExport, WatchlistExport,
};
use crate::ui::{self, DataType, ProgressReporter, ProgressUpdate};
use anyhow::{Context, Result};
use crunchyroll_rs::{Crunchyroll, MediaCollection};
use futures_util::{StreamExt, stream};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

const WRITE_DELAY: Duration = Duration::from_millis(500);
const CONCURRENCY: usize = 5;
const INITIAL_BACKOFF: Duration = Duration::from_secs(2);
const MAX_RETRIES: u32 = 5;

struct Counts {
    total: usize,
    added: usize,
    already_present: usize,
    failed: usize,
}

impl Counts {
    fn new(total: usize) -> Self {
        Self {
            total,
            added: 0,
            already_present: 0,
            failed: 0,
        }
    }

    fn processed(&self) -> usize {
        self.added + self.already_present + self.failed
    }

    fn to_update(&self, data_type: DataType) -> ProgressUpdate {
        ProgressUpdate {
            data_type,
            total: self.total,
            processed: self.processed(),
            added: self.added,
            skipped: 0,
            already_present: self.already_present,
            failed: self.failed,
        }
    }
}

pub async fn run(crunchy: &Crunchyroll, input_dir: &Path, dry_run: bool) -> Result<()> {
    if dry_run {
        println!("Dry run -- showing what would be imported:\n");
        return crate::diff::run(crunchy, input_dir).await;
    }

    let watchlist: WatchlistExport = models::read_export(input_dir, "watchlist.json")?;
    let history: WatchHistoryExport = models::read_export(input_dir, "watch_history.json")?;
    let crunchylists: CrunchylistsExport = models::read_export(input_dir, "crunchylists.json")?;
    let ratings: RatingsExport = models::read_export(input_dir, "ratings.json")?;

    println!("Fetching target account state for pre-filtering...");
    let target_state = fetch_target_state(crunchy).await?;

    let profile = crunchy.profile_id().await;
    let (reporter, dashboard) = ui::start_dashboard("Import", "", &profile);

    let wl = import_watchlist(crunchy, &watchlist, &target_state, &reporter).await?;
    let cl = import_crunchylists(crunchy, &crunchylists, &target_state, &reporter).await?;
    let rt = import_ratings(crunchy, &ratings, &reporter).await?;
    let hi = import_history(crunchy, &history, &target_state, &reporter).await?;

    reporter.done();
    dashboard.wait();

    if !ui::is_tty() {
        print_summary(&[
            ("Watchlist", &wl),
            ("Crunchylists", &cl),
            ("Ratings", &rt),
            ("History", &hi),
        ]);
    }
    Ok(())
}

pub struct TargetState {
    pub watchlist_ids: HashSet<String>,
    pub history_ids: HashSet<String>,
    /// Map from list name -> set of content_ids already in that list
    pub crunchylists: HashMap<String, HashSet<String>>,
}

pub async fn fetch_target_state(crunchy: &Crunchyroll) -> Result<TargetState> {
    use crunchyroll_rs::list::WatchlistOptions;

    let watchlist = crunchy.watchlist(WatchlistOptions::default()).await?;
    let watchlist_ids: HashSet<String> = watchlist
        .iter()
        .filter_map(|e| crate::export::extract_series_info(&e.panel).map(|(id, _, _, _)| id))
        .collect();

    let mut history_ids = HashSet::new();
    let mut stream = crunchy.watch_history();
    while let Some(Ok(entry)) = stream.next().await {
        history_ids.insert(entry.id.clone());
    }

    let lists = crunchy.crunchylists().await?;
    let mut crunchylists = HashMap::new();
    for preview in &lists.items {
        let full_list = preview.crunchylist().await?;
        let item_ids: HashSet<String> = full_list
            .items
            .iter()
            .filter_map(|e| crate::export::extract_series_info(&e.panel).map(|(id, _, _, _)| id))
            .collect();
        crunchylists.insert(preview.title.clone(), item_ids);
    }

    Ok(TargetState {
        watchlist_ids,
        history_ids,
        crunchylists,
    })
}

async fn import_watchlist(
    crunchy: &Crunchyroll,
    export: &WatchlistExport,
    target: &TargetState,
    reporter: &ProgressReporter,
) -> Result<Counts> {
    let mut c = Counts::new(export.items.len());

    let to_import: Vec<_> = export
        .items
        .iter()
        .filter(|item| !target.watchlist_ids.contains(&item.content_id))
        .collect();
    c.already_present = export.items.len() - to_import.len();
    reporter.progress(c.to_update(DataType::Watchlist));

    let mut results = stream::iter(to_import)
        .map(|item| {
            let cr = crunchy.clone();
            let content_id = item.content_id.clone();
            let content_type = item.content_type.clone();
            let title = item.title.clone();
            async move {
                let result =
                    retry_with_backoff(|| add_to_watchlist(&cr, &content_id, &content_type)).await;
                tokio::time::sleep(WRITE_DELAY).await;
                (title, result)
            }
        })
        .buffer_unordered(CONCURRENCY);

    while let Some((title, result)) = results.next().await {
        match result {
            Ok(()) => {
                reporter.log_success(&title);
                c.added += 1;
            }
            Err(e) if is_conflict(&e) => {
                reporter.log_skip(&title);
                c.already_present += 1;
            }
            Err(e) => {
                reporter.log_error(&format!("{} -- {}", title, e));
                c.failed += 1;
            }
        }
        reporter.progress(c.to_update(DataType::Watchlist));
    }

    Ok(c)
}

async fn add_to_watchlist(
    crunchy: &Crunchyroll,
    content_id: &str,
    content_type: &str,
) -> Result<()> {
    match content_type {
        "series" => {
            let series: crunchyroll_rs::Series = crunchy.media_from_id(content_id).await?;
            series.add_to_watchlist().await?;
        }
        "movie_listing" => {
            let ml: crunchyroll_rs::MovieListing = crunchy.media_from_id(content_id).await?;
            ml.add_to_watchlist().await?;
        }
        _ => anyhow::bail!("Unknown content type: {}", content_type),
    }
    Ok(())
}

async fn import_crunchylists(
    crunchy: &Crunchyroll,
    export: &CrunchylistsExport,
    target: &TargetState,
    reporter: &ProgressReporter,
) -> Result<Counts> {
    let total_items: usize = export.lists.iter().map(|l| l.items.len()).sum();
    let mut c = Counts::new(total_items);
    reporter.progress(c.to_update(DataType::Crunchylists));

    for list_data in &export.lists {
        let existing_items = target.crunchylists.get(&list_data.name);

        // Get or create the list on the target
        let full_list = if existing_items.is_some() {
            let lists = crunchy.crunchylists().await?;
            let preview = lists
                .items
                .iter()
                .find(|p| p.title == list_data.name)
                .with_context(|| format!("Finding crunchylist '{}'", list_data.name))?;
            reporter.log_skip(&format!(
                "'{}' already exists, checking items",
                list_data.name
            ));
            preview.crunchylist().await?
        } else {
            let lists = crunchy.crunchylists().await?;
            let preview = lists
                .create(&list_data.name)
                .await
                .with_context(|| format!("Creating crunchylist '{}'", list_data.name))?;
            reporter.log_success(&format!("Created list '{}'", list_data.name));
            preview.crunchylist().await?
        };

        for item in &list_data.items {
            if existing_items.is_some_and(|ids| ids.contains(&item.content_id)) {
                c.already_present += 1;
                reporter.progress(c.to_update(DataType::Crunchylists));
                continue;
            }

            match retry_with_backoff(|| add_to_crunchylist(crunchy, &full_list, &item.content_id))
                .await
            {
                Ok(()) => {
                    reporter.log_success(&format!("  {} -> {}", list_data.name, item.title));
                    c.added += 1;
                }
                Err(e) if is_conflict(&e) => {
                    c.already_present += 1;
                }
                Err(e) => {
                    reporter.log_error(&format!("{} -- {}", item.title, e));
                    c.failed += 1;
                }
            }

            reporter.progress(c.to_update(DataType::Crunchylists));
            tokio::time::sleep(WRITE_DELAY).await;
        }
    }

    Ok(c)
}

async fn add_to_crunchylist(
    crunchy: &Crunchyroll,
    list: &crunchyroll_rs::list::Crunchylist,
    content_id: &str,
) -> Result<()> {
    if let Ok(series) = crunchy
        .media_from_id::<crunchyroll_rs::Series>(content_id)
        .await
    {
        list.add(MediaCollection::from(series)).await?;
    } else if let Ok(ml) = crunchy
        .media_from_id::<crunchyroll_rs::MovieListing>(content_id)
        .await
    {
        list.add(MediaCollection::from(ml)).await?;
    } else {
        anyhow::bail!(
            "Content {} not found as series or movie_listing",
            content_id
        );
    }
    Ok(())
}

async fn import_ratings(
    crunchy: &Crunchyroll,
    export: &RatingsExport,
    reporter: &ProgressReporter,
) -> Result<Counts> {
    let mut c = Counts::new(export.items.len());
    reporter.progress(c.to_update(DataType::Ratings));

    for item in &export.items {
        match retry_with_backoff(|| set_rating(crunchy, item)).await {
            Ok(()) => {
                reporter.log_success(&format!("{} ({})", item.title, item.rating));
                c.added += 1;
            }
            Err(e) => {
                reporter.log_error(&format!("{} -- {}", item.title, e));
                c.failed += 1;
            }
        }

        reporter.progress(c.to_update(DataType::Ratings));
        tokio::time::sleep(WRITE_DELAY).await;
    }

    Ok(c)
}

async fn set_rating(crunchy: &Crunchyroll, item: &RatingItem) -> Result<()> {
    use crunchyroll_rs::media::RatingStar;

    let stars = match item.rating.as_str() {
        "OneStar" => RatingStar::OneStar,
        "TwoStars" => RatingStar::TwoStars,
        "ThreeStars" => RatingStar::ThreeStars,
        "FourStars" => RatingStar::FourStars,
        "FiveStars" => RatingStar::FiveStars,
        other => anyhow::bail!("Unknown rating: {}", other),
    };

    match item.content_type.as_str() {
        "series" => {
            let series: crunchyroll_rs::Series = crunchy.media_from_id(&item.content_id).await?;
            series.rate(stars).await?;
        }
        "movie_listing" => {
            let ml: crunchyroll_rs::MovieListing = crunchy.media_from_id(&item.content_id).await?;
            ml.rate(stars).await?;
        }
        _ => anyhow::bail!("Unknown content type: {}", item.content_type),
    }
    Ok(())
}

async fn import_history(
    crunchy: &Crunchyroll,
    export: &WatchHistoryExport,
    target: &TargetState,
    reporter: &ProgressReporter,
) -> Result<Counts> {
    let mut c = Counts::new(export.items.len());

    let to_import: Vec<_> = export
        .items
        .iter()
        .filter(|item| !target.history_ids.contains(&item.content_id))
        .collect();
    c.already_present = export.items.len() - to_import.len();
    reporter.progress(c.to_update(DataType::History));

    // Pre-fetch account_id once instead of per-request
    let account_id: Arc<str> = crunchy.account().await?.account_id.into();

    let mut results = stream::iter(to_import)
        .map(|item| {
            let cr = crunchy.clone();
            let account_id = account_id.clone();
            let content_id = item.content_id.clone();
            let label = if item.title.is_empty() {
                format!("{} - {}", item.series_title, item.content_id)
            } else {
                format!("{} - {}", item.series_title, item.title)
            };
            async move {
                let result =
                    retry_with_backoff(|| mark_as_watched(&cr, &account_id, &content_id)).await;
                tokio::time::sleep(WRITE_DELAY).await;
                (label, result)
            }
        })
        .buffer_unordered(CONCURRENCY);

    while let Some((label, result)) = results.next().await {
        match result {
            Ok(()) => {
                reporter.log_success(&label);
                c.added += 1;
            }
            Err(e) => {
                reporter.log_error(&format!("{} -- {}", label, e));
                c.failed += 1;
            }
        }
        reporter.progress(c.to_update(DataType::History));
    }

    Ok(c)
}

async fn mark_as_watched(crunchy: &Crunchyroll, account_id: &str, content_id: &str) -> Result<()> {
    let url = format!(
        "https://www.crunchyroll.com/content/v2/discover/{}/mark_as_watched/{}",
        account_id, content_id
    );
    let client = crunchy.client();
    let token = crunchy.access_token().await;

    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", token))
        .json(&serde_json::json!({}))
        .send()
        .await?;

    let status = resp.status();
    if status.is_success() || status.as_u16() == 409 {
        Ok(())
    } else {
        anyhow::bail!("mark_as_watched returned {} for {}", status, content_id)
    }
}

async fn retry_with_backoff<F, Fut>(mut f: F) -> Result<()>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    let mut delay = INITIAL_BACKOFF;

    for attempt in 0..=MAX_RETRIES {
        match f().await {
            Ok(()) => return Ok(()),
            Err(e) if attempt < MAX_RETRIES && is_cloudflare_block(&e) => {
                eprintln!("  Cloudflare block detected, waiting 60s before retry...");
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
            Err(e) if attempt < MAX_RETRIES && is_transient(&e) => {
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(Duration::from_secs(32));
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!()
}

fn is_cloudflare_block(e: &anyhow::Error) -> bool {
    e.downcast_ref::<crunchyroll_rs::error::Error>()
        .is_some_and(|ce| matches!(ce, crunchyroll_rs::error::Error::Block { .. }))
}

fn is_transient(e: &anyhow::Error) -> bool {
    // Check structured error first for status code
    if let Some(crunchyroll_rs::error::Error::Request {
        status: Some(code), ..
    }) = e.downcast_ref::<crunchyroll_rs::error::Error>()
    {
        let n = code.as_u16();
        return n == 429 || (500..=504).contains(&n);
    }
    // Fallback to string matching for non-library errors (e.g. reqwest)
    let msg = e.to_string();
    msg.contains("429")
        || msg.contains("500")
        || msg.contains("502")
        || msg.contains("503")
        || msg.contains("504")
        || msg.contains("timeout")
}

fn is_conflict(e: &anyhow::Error) -> bool {
    if let Some(crunchyroll_rs::error::Error::Request {
        status: Some(code), ..
    }) = e.downcast_ref::<crunchyroll_rs::error::Error>()
    {
        return code.as_u16() == 409;
    }
    false
}

fn print_summary(sections: &[(&str, &Counts)]) {
    println!("\n  Import Summary");
    println!("  {}", "\u{2500}".repeat(50));

    let mut total_added = 0;
    let mut total_already = 0;
    let mut total_failed = 0;

    for (name, c) in sections {
        println!(
            "  {:14} {} added, {} already there, {} failed",
            name, c.added, c.already_present, c.failed
        );
        total_added += c.added;
        total_already += c.already_present;
        total_failed += c.failed;
    }

    println!("  {}", "\u{2500}".repeat(50));
    println!(
        "  {:14} {} added, {} already there, {} failed",
        "Total", total_added, total_already, total_failed
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_new_starts_at_zero() {
        let c = Counts::new(42);
        assert_eq!(c.total, 42);
        assert_eq!(c.added, 0);
        assert_eq!(c.already_present, 0);
        assert_eq!(c.failed, 0);
        assert_eq!(c.processed(), 0);
    }

    #[test]
    fn counts_processed_sums_all() {
        let mut c = Counts::new(10);
        c.added = 3;
        c.already_present = 4;
        c.failed = 2;
        assert_eq!(c.processed(), 9);
    }

    #[test]
    fn counts_to_update_maps_correctly() {
        let mut c = Counts::new(10);
        c.added = 5;
        c.already_present = 3;
        c.failed = 1;
        let u = c.to_update(crate::ui::DataType::Watchlist);
        assert_eq!(u.total, 10);
        assert_eq!(u.processed, 9);
        assert_eq!(u.added, 5);
        assert_eq!(u.already_present, 3);
        assert_eq!(u.failed, 1);
    }

    #[test]
    fn is_transient_matches_status_strings() {
        // The string-matching fallback path for non-crunchyroll-rs errors
        assert!(is_transient(&anyhow::anyhow!("server returned 429")));
        assert!(is_transient(&anyhow::anyhow!("got 500 internal")));
        assert!(is_transient(&anyhow::anyhow!("502 bad gateway")));
        assert!(is_transient(&anyhow::anyhow!("503 unavailable")));
        assert!(is_transient(&anyhow::anyhow!("504 gateway timeout")));
        assert!(is_transient(&anyhow::anyhow!("connection timeout")));
    }

    #[test]
    fn is_transient_rejects_non_transient() {
        assert!(!is_transient(&anyhow::anyhow!("not found")));
        assert!(!is_transient(&anyhow::anyhow!("forbidden")));
        assert!(!is_transient(&anyhow::anyhow!("400 bad request")));
    }

    #[test]
    fn is_conflict_rejects_plain_errors() {
        // Plain anyhow errors don't contain crunchyroll_rs error types
        assert!(!is_conflict(&anyhow::anyhow!("409 conflict")));
        assert!(!is_conflict(&anyhow::anyhow!("something else")));
    }

    #[test]
    fn is_cloudflare_block_rejects_plain_errors() {
        assert!(!is_cloudflare_block(&anyhow::anyhow!("blocked")));
        assert!(!is_cloudflare_block(&anyhow::anyhow!("cloudflare")));
    }

    #[tokio::test]
    async fn retry_succeeds_immediately() {
        let result = retry_with_backoff(|| async { Ok(()) }).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn retry_succeeds_after_transient_failure() {
        let count = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let count_clone = count.clone();
        let result = retry_with_backoff(move || {
            let c = count_clone.clone();
            async move {
                let attempt = c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if attempt < 2 {
                    Err(anyhow::anyhow!("server returned 429"))
                } else {
                    Ok(())
                }
            }
        })
        .await;
        assert!(result.is_ok());
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn retry_gives_up_on_permanent_error() {
        let result =
            retry_with_backoff(|| async { Err::<(), _>(anyhow::anyhow!("400 bad request")) }).await;
        assert!(result.is_err());
    }
}
