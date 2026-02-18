use chrono::{TimeZone, Utc};
use crunchyroll_migrate::models::*;

fn sample_watchlist() -> WatchlistExport {
    WatchlistExport {
        metadata: ExportMetadata {
            profile_name: "Sean".to_string(),
            exported_at: Utc.with_ymd_and_hms(2026, 2, 18, 12, 0, 0).unwrap(),
            total_count: 2,
        },
        items: vec![
            WatchlistItem {
                content_id: "G4PH0WXYZ".to_string(),
                title: "One Piece".to_string(),
                slug: "one-piece".to_string(),
                content_type: "series".to_string(),
                is_favourite: true,
                fully_watched: false,
            },
            WatchlistItem {
                content_id: "GMKUX0ABC".to_string(),
                title: "A Silent Voice".to_string(),
                slug: "a-silent-voice".to_string(),
                content_type: "movie_listing".to_string(),
                is_favourite: false,
                fully_watched: true,
            },
        ],
    }
}

fn sample_history() -> WatchHistoryExport {
    WatchHistoryExport {
        metadata: ExportMetadata {
            profile_name: "Sean".to_string(),
            exported_at: Utc.with_ymd_and_hms(2026, 2, 18, 12, 0, 0).unwrap(),
            total_count: 2,
        },
        items: vec![
            WatchHistoryItem {
                content_id: "GRDQPM1ZY".to_string(),
                parent_id: "G4PH0WXYZ".to_string(),
                parent_type: "series".to_string(),
                title: "Romance Dawn".to_string(),
                series_title: "One Piece".to_string(),
                date_played: Utc.with_ymd_and_hms(2026, 1, 15, 20, 30, 0).unwrap(),
                playhead: 1420,
                fully_watched: true,
                partial: false,
            },
            WatchHistoryItem {
                content_id: "GXYZ00000".to_string(),
                parent_id: "GMKUX0ABC".to_string(),
                parent_type: "movie_listing".to_string(),
                title: String::new(),
                series_title: String::new(),
                date_played: Utc.with_ymd_and_hms(2026, 1, 10, 18, 0, 0).unwrap(),
                playhead: 0,
                fully_watched: false,
                partial: true,
            },
        ],
    }
}

fn sample_crunchylists() -> CrunchylistsExport {
    CrunchylistsExport {
        metadata: ExportMetadata {
            profile_name: "Sean".to_string(),
            exported_at: Utc.with_ymd_and_hms(2026, 2, 18, 12, 0, 0).unwrap(),
            total_count: 1,
        },
        lists: vec![CrunchylistData {
            name: "Favourites".to_string(),
            items: vec![CrunchylistItem {
                content_id: "G4PH0WXYZ".to_string(),
                title: "One Piece".to_string(),
            }],
        }],
    }
}

fn sample_ratings() -> RatingsExport {
    RatingsExport {
        metadata: ExportMetadata {
            profile_name: "Sean".to_string(),
            exported_at: Utc.with_ymd_and_hms(2026, 2, 18, 12, 0, 0).unwrap(),
            total_count: 1,
        },
        items: vec![RatingItem {
            content_id: "G4PH0WXYZ".to_string(),
            content_type: "series".to_string(),
            title: "One Piece".to_string(),
            rating: "FiveStars".to_string(),
        }],
    }
}

#[test]
fn watchlist_round_trip() {
    let original = sample_watchlist();
    let json = serde_json::to_string_pretty(&original).unwrap();
    let parsed: WatchlistExport = serde_json::from_str(&json).unwrap();

    assert_eq!(parsed.metadata.profile_name, "Sean");
    assert_eq!(parsed.metadata.total_count, 2);
    assert_eq!(parsed.items.len(), 2);
    assert_eq!(parsed.items[0].content_id, "G4PH0WXYZ");
    assert_eq!(parsed.items[0].content_type, "series");
    assert!(parsed.items[0].is_favourite);
    assert!(!parsed.items[0].fully_watched);
    assert_eq!(parsed.items[1].content_type, "movie_listing");
}

#[test]
fn history_round_trip() {
    let original = sample_history();
    let json = serde_json::to_string_pretty(&original).unwrap();
    let parsed: WatchHistoryExport = serde_json::from_str(&json).unwrap();

    assert_eq!(parsed.items.len(), 2);
    assert_eq!(parsed.items[0].content_id, "GRDQPM1ZY");
    assert_eq!(parsed.items[0].parent_id, "G4PH0WXYZ");
    assert_eq!(parsed.items[0].playhead, 1420);
    assert!(parsed.items[0].fully_watched);
    assert!(!parsed.items[0].partial);
    assert!(parsed.items[1].partial);
}

#[test]
fn history_partial_defaults_to_false() {
    let json = r#"{
        "content_id": "ABC",
        "parent_id": "DEF",
        "parent_type": "series",
        "title": "Test",
        "series_title": "Test Series",
        "date_played": "2026-01-15T20:30:00Z",
        "playhead": 100,
        "fully_watched": true
    }"#;
    let item: WatchHistoryItem = serde_json::from_str(json).unwrap();
    assert!(!item.partial);
}

#[test]
fn crunchylists_round_trip() {
    let original = sample_crunchylists();
    let json = serde_json::to_string_pretty(&original).unwrap();
    let parsed: CrunchylistsExport = serde_json::from_str(&json).unwrap();

    assert_eq!(parsed.lists.len(), 1);
    assert_eq!(parsed.lists[0].name, "Favourites");
    assert_eq!(parsed.lists[0].items.len(), 1);
    assert_eq!(parsed.lists[0].items[0].content_id, "G4PH0WXYZ");
}

#[test]
fn ratings_round_trip() {
    let original = sample_ratings();
    let json = serde_json::to_string_pretty(&original).unwrap();
    let parsed: RatingsExport = serde_json::from_str(&json).unwrap();

    assert_eq!(parsed.items.len(), 1);
    assert_eq!(parsed.items[0].rating, "FiveStars");
    assert_eq!(parsed.items[0].content_type, "series");
}
