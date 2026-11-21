use crate::models::{
    self, CrunchylistsExport, RatingItem, RatingsExport, WatchHistoryExport, WatchlistExport,
};
use crate::ui::{DataType, Outcome, Session, Stage, show_key};
use anyhow::{Context, Result};
use crunchyroll_rs::error::ErrorKind;
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

/// The four export files.
pub struct Exports {
    pub watchlist: WatchlistExport,
    pub history: WatchHistoryExport,
    pub crunchylists: CrunchylistsExport,
    pub ratings: RatingsExport,
}

impl Exports {
    pub fn load(dir: &Path) -> Result<Self> {
        Ok(Self {
            watchlist: models::read_export(dir, "watchlist.json")?,
            history: models::read_export(dir, "watch_history.json")?,
            crunchylists: models::read_export(dir, "crunchylists.json")?,
            ratings: models::read_export(dir, "ratings.json")?,
        })
    }

    /// Fill the "On source" column from files (import/diff read a previous export).
    pub fn report_source(&self, ui: &Session) {
        ui.stage(Stage::Read);
        for i in &self.watchlist.items {
            ui.source(DataType::Watchlist, &show_key(&i.title, ""));
        }
        for i in &self.history.items {
            ui.source(DataType::History, &show_key(&i.series_title, &i.title));
        }
        for i in self.crunchylists.lists.iter().flat_map(|l| &l.items) {
            ui.source(DataType::Crunchylists, &show_key(&i.title, ""));
        }
        for i in &self.ratings.items {
            ui.source(DataType::Ratings, &show_key(&i.title, ""));
        }
    }
}

/// Check the target profile and plan every source item as "already there" or "to copy".
pub async fn compare(crunchy: &Crunchyroll, ex: &Exports, ui: &Session) -> Result<TargetState> {
    ui.stage(Stage::Compare);
    ui.status("Checking the target profile…");
    let target = fetch_target_state(crunchy).await?;

    for i in &ex.watchlist.items {
        let already = target.watchlist_ids.contains(&i.content_id);
        ui.planned(DataType::Watchlist, &show_key(&i.title, ""), already);
    }
    for i in &ex.history.items {
        let already = target.history_ids.contains(&i.content_id);
        ui.planned(
            DataType::History,
            &show_key(&i.series_title, &i.title),
            already,
        );
    }
    for list in &ex.crunchylists.lists {
        let existing = target.crunchylists.get(&list.name);
        for i in &list.items {
            let already = existing.is_some_and(|ids| ids.contains(&i.content_id));
            ui.planned(DataType::Crunchylists, &show_key(&i.title, ""), already);
        }
    }
    // Ratings are always written; the target's ratings aren't read up front
    for i in &ex.ratings.items {
        ui.planned(DataType::Ratings, &show_key(&i.title, ""), false);
    }
    ui.compared();
    ui.status("");
    Ok(target)
}

/// Write everything the compare step planned.
pub async fn copy(
    crunchy: &Crunchyroll,
    ex: &Exports,
    target: &TargetState,
    ui: &Session,
) -> Result<()> {
    ui.stage(Stage::Copy);
    import_watchlist(crunchy, &ex.watchlist, target, ui).await?;
    import_crunchylists(crunchy, &ex.crunchylists, target, ui).await?;
    import_ratings(crunchy, &ex.ratings, ui).await?;
    import_history(crunchy, &ex.history, target, ui).await?;
    Ok(())
}

fn record(ui: &Session, kind: DataType, show: &str, label: &str, result: &Result<()>) {
    let outcome = match result {
        Ok(()) => Outcome::Copied,
        Err(e) if is_conflict(e) => Outcome::Already,
        Err(e) => {
            ui.warn(&format!("{label}: {e:#}"));
            Outcome::Failed
        }
    };
    ui.copied(kind, show, outcome);
}

pub struct TargetState {
    pub watchlist_ids: HashSet<String>,
    pub history_ids: HashSet<String>,
    /// Map from list name -> set of content_ids already in that list
    pub crunchylists: HashMap<String, HashSet<String>>,
}

async fn fetch_target_state(crunchy: &Crunchyroll) -> Result<TargetState> {
    use crunchyroll_rs::list::WatchlistOptions;

    let watchlist = crunchy.watchlist(WatchlistOptions::default()).await?;
    let watchlist_ids: HashSet<String> = watchlist
        .iter()
        .filter_map(|e| crate::export::extract_series_info(&e.panel).map(|(id, _, _, _)| id))
        .collect();

    let mut history_ids = HashSet::new();
    let mut stream = crunchy.watch_history();
    stream.page_size(crate::HISTORY_PAGE_SIZE);
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
    ui: &Session,
) -> Result<()> {
    let notify = |m: &str| ui.status(m);
    let mut results = stream::iter(
        export
            .items
            .iter()
            .filter(|item| !target.watchlist_ids.contains(&item.content_id)),
    )
    .map(|item| {
        let notify = &notify;
        async move {
            let result = retry_with_backoff(notify, || {
                add_to_watchlist(crunchy, &item.content_id, &item.content_type)
            })
            .await;
            tokio::time::sleep(WRITE_DELAY).await;
            (item, result)
        }
    })
    .buffer_unordered(CONCURRENCY);

    while let Some((item, result)) = results.next().await {
        let show = show_key(&item.title, "");
        record(ui, DataType::Watchlist, &show, &item.title, &result);
    }
    Ok(())
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
        _ => anyhow::bail!("Unknown content type: {content_type}"),
    }
    Ok(())
}

async fn import_crunchylists(
    crunchy: &Crunchyroll,
    export: &CrunchylistsExport,
    target: &TargetState,
    ui: &Session,
) -> Result<()> {
    let notify = |m: &str| ui.status(m);
    for list_data in &export.lists {
        let existing_items = target.crunchylists.get(&list_data.name);

        // Get or create the list on the target
        let lists = crunchy.crunchylists().await?;
        let full_list = if existing_items.is_some() {
            let preview = lists
                .items
                .iter()
                .find(|p| p.title == list_data.name)
                .with_context(|| format!("Finding crunchylist '{}'", list_data.name))?;
            preview.crunchylist().await?
        } else {
            let preview = lists
                .create(&list_data.name)
                .await
                .with_context(|| format!("Creating crunchylist '{}'", list_data.name))?;
            ui.note(&format!("Created list '{}'", list_data.name));
            preview.crunchylist().await?
        };

        for item in &list_data.items {
            if existing_items.is_some_and(|ids| ids.contains(&item.content_id)) {
                continue;
            }
            let result = retry_with_backoff(&notify, || {
                add_to_crunchylist(crunchy, &full_list, &item.content_id)
            })
            .await;
            let show = show_key(&item.title, "");
            let label = format!("{} → {}", item.title, list_data.name);
            record(ui, DataType::Crunchylists, &show, &label, &result);
            tokio::time::sleep(WRITE_DELAY).await;
        }
    }
    Ok(())
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
        anyhow::bail!("Content {content_id} not found as series or movie_listing");
    }
    Ok(())
}

async fn import_ratings(crunchy: &Crunchyroll, export: &RatingsExport, ui: &Session) -> Result<()> {
    let notify = |m: &str| ui.status(m);
    for item in &export.items {
        let result = retry_with_backoff(&notify, || set_rating(crunchy, item)).await;
        let show = show_key(&item.title, "");
        record(ui, DataType::Ratings, &show, &item.title, &result);
        tokio::time::sleep(WRITE_DELAY).await;
    }
    Ok(())
}

async fn set_rating(crunchy: &Crunchyroll, item: &RatingItem) -> Result<()> {
    use crunchyroll_rs::media::RatingStar;

    let stars = match item.rating.as_str() {
        "OneStar" => RatingStar::OneStar,
        "TwoStars" => RatingStar::TwoStars,
        "ThreeStars" => RatingStar::ThreeStars,
        "FourStars" => RatingStar::FourStars,
        "FiveStars" => RatingStar::FiveStars,
        other => anyhow::bail!("Unknown rating: {other}"),
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
    ui: &Session,
) -> Result<()> {
    // Pre-fetch account_id once instead of per-request
    let account_id: Arc<str> = crunchy.account().await?.account_id.into();
    let notify = |m: &str| ui.status(m);

    let mut results = stream::iter(
        export
            .items
            .iter()
            .filter(|item| !target.history_ids.contains(&item.content_id)),
    )
    .map(|item| {
        let account_id = account_id.clone();
        let notify = &notify;
        async move {
            let id = item.content_id.as_str();
            let result = if item.fully_watched {
                retry_with_backoff(notify, || mark_as_watched(crunchy, &account_id, id)).await
            } else if item.playhead > 0 {
                // Restore playhead so the user can resume where they left off
                retry_with_backoff(notify, || {
                    set_playhead(crunchy, &account_id, id, item.playhead)
                })
                .await
            } else {
                Ok(())
            };
            tokio::time::sleep(WRITE_DELAY).await;
            (item, result)
        }
    })
    .buffer_unordered(CONCURRENCY);

    while let Some((item, result)) = results.next().await {
        let show = show_key(&item.series_title, &item.title);
        let label = if item.title.is_empty() {
            format!("{show} ({})", item.content_id)
        } else {
            format!("{show} - {}", item.title)
        };
        record(ui, DataType::History, &show, &label, &result);
    }
    Ok(())
}

async fn set_playhead(
    crunchy: &Crunchyroll,
    account_id: &str,
    content_id: &str,
    playhead: u32,
) -> Result<()> {
    let url = format!("https://www.crunchyroll.com/content/v2/{account_id}/playheads");
    let status = crunchy
        .client()
        .post(&url)
        .header(
            "Authorization",
            format!("Bearer {}", crunchy.access_token().await),
        )
        .json(&serde_json::json!({
            "content_id": content_id,
            "playhead": playhead
        }))
        .send()
        .await?
        .status()
        .as_u16();

    check_status(status, "set_playhead", content_id)
}

async fn mark_as_watched(crunchy: &Crunchyroll, account_id: &str, content_id: &str) -> Result<()> {
    let url = format!(
        "https://www.crunchyroll.com/content/v2/discover/{account_id}/mark_as_watched/{content_id}"
    );
    let status = crunchy
        .client()
        .post(&url)
        .header(
            "Authorization",
            format!("Bearer {}", crunchy.access_token().await),
        )
        .json(&serde_json::json!({}))
        .send()
        .await?
        .status()
        .as_u16();

    check_status(status, "mark_as_watched", content_id)
}

/// Treat 2xx and 409 (already exists) as OK; fail on anything else.
fn check_status(status: u16, operation: &str, content_id: &str) -> Result<()> {
    if (200..300).contains(&status) || status == 409 {
        Ok(())
    } else {
        anyhow::bail!("{operation} returned {status} for {content_id}")
    }
}

/// Retry transient failures with exponential backoff. `notify` reports long waits.
async fn retry_with_backoff<F, Fut>(notify: &dyn Fn(&str), mut f: F) -> Result<()>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    let mut delay = INITIAL_BACKOFF;

    for attempt in 0..=MAX_RETRIES {
        match f().await {
            Ok(()) => return Ok(()),
            Err(e) if attempt < MAX_RETRIES && is_cloudflare_block(&e) => {
                notify("Cloudflare is blocking requests; waiting 60s before retrying");
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

fn error_kind(e: &anyhow::Error) -> Option<&ErrorKind> {
    e.downcast_ref::<crunchyroll_rs::error::Error>()
        .map(crunchyroll_rs::Error::kind)
}

fn is_cloudflare_block(e: &anyhow::Error) -> bool {
    matches!(error_kind(e), Some(ErrorKind::Block { .. }))
}

fn is_transient(e: &anyhow::Error) -> bool {
    // Check structured error first for status code
    if let Some(ErrorKind::Request { status: Some(code) }) = error_kind(e) {
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
    if let Some(ErrorKind::Request { status: Some(code) }) = error_kind(e) {
        return code.as_u16() == 409;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let result = retry_with_backoff(&|_| {}, || async { Ok(()) }).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn retry_succeeds_after_transient_failure() {
        let count = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let count_clone = count.clone();
        let result = retry_with_backoff(&|_| {}, move || {
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
        let result = retry_with_backoff(&|_| {}, || async {
            Err::<(), _>(anyhow::anyhow!("400 bad request"))
        })
        .await;
        assert!(result.is_err());
    }
}
