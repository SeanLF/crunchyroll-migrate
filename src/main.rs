mod auth;
mod diff;
mod export;
mod import;
mod models;
mod ui;

use anyhow::Context;
use clap::{Parser, Subcommand};
use crunchyroll_rs::list::WatchlistOptions;
use futures_util::StreamExt;
use std::path::PathBuf;

/// Watch history page size. The library default (20) makes large histories crawl.
pub const HISTORY_PAGE_SIZE: u32 = 100;

#[derive(Parser)]
#[command(name = "crunchyroll-migrate")]
#[command(about = "Migrate Crunchyroll profile data between accounts")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Show account info, profiles, and data counts
    Status {
        /// `etp_rt` cookie from a logged-in crunchyroll.com browser session (prompted if omitted)
        #[arg(long)]
        cookie: Option<String>,
        #[arg(long)]
        profile: Option<String>,
    },

    /// Export one profile's data to JSON files
    Export {
        /// `etp_rt` cookie from a logged-in crunchyroll.com browser session (prompted if omitted)
        #[arg(long)]
        cookie: Option<String>,
        #[arg(long)]
        profile: Option<String>,
        #[arg(long, default_value = "./export")]
        output_dir: PathBuf,
    },

    /// Import from JSON files into a profile
    Import {
        /// `etp_rt` cookie from a logged-in crunchyroll.com browser session (prompted if omitted)
        #[arg(long)]
        cookie: Option<String>,
        #[arg(long)]
        profile: Option<String>,
        #[arg(long, default_value = "./export")]
        input_dir: PathBuf,
        #[arg(long)]
        dry_run: bool,
    },

    /// Compare exported data against target account
    Diff {
        /// `etp_rt` cookie from a logged-in crunchyroll.com browser session (prompted if omitted)
        #[arg(long)]
        cookie: Option<String>,
        #[arg(long)]
        profile: Option<String>,
        #[arg(long, short = 'i', default_value = "./export")]
        input_dir: PathBuf,
    },

    /// Rename a profile on the account
    RenameProfile {
        /// `etp_rt` cookie from a logged-in crunchyroll.com browser session (prompted if omitted)
        #[arg(long)]
        cookie: Option<String>,
        #[arg(long)]
        profile: Option<String>,
        #[arg(long)]
        new_name: Option<String>,
    },

    /// Full flow: export -> diff -> confirm -> import
    Migrate {
        /// `etp_rt` cookie from a logged-in crunchyroll.com browser session (prompted if omitted)
        #[arg(long)]
        source_cookie: Option<String>,
        #[arg(long)]
        source_profile: Option<String>,
        /// Target account's `etp_rt` cookie (prompted if omitted; leave the prompt empty to copy
        /// between profiles on the source account)
        #[arg(long)]
        target_cookie: Option<String>,
        #[arg(long)]
        target_profile: Option<String>,
        /// Where the intermediate export goes (deleted after a successful import)
        #[arg(long, default_value = "./migration")]
        data_dir: PathBuf,
        /// Keep the intermediate export after migrating, as a backup
        #[arg(long)]
        keep_data: bool,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Ensure terminal state is restored on panic (raw mode + alternate screen)
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = crossterm::execute!(std::io::stdout(), crossterm::terminal::LeaveAlternateScreen);
        original_hook(info);
    }));

    let cli = Cli::parse();

    let command = match cli.command {
        Some(cmd) => cmd,
        None => select_command()?,
    };

    match command {
        Command::Status { cookie, profile } => {
            let session = auth::initial_login(cookie, "").await?;
            let is_premium = session.crunchy.premium().await;

            println!("Account");
            println!("  Premium: {}", if is_premium { "yes" } else { "no" });
            println!("  Profiles: {} found\n", session.profiles.len());

            for p in &session.profiles {
                let flags = [
                    p.is_primary.then_some("primary"),
                    p.is_selected.then_some("selected"),
                ]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>();

                let suffix = if flags.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", flags.join(", "))
                };
                println!("  - {}{}", p.profile_name, suffix);
            }

            let selected = auth::select_profile(&session.profiles, profile, true)?;
            let profile_name = selected.profile_name.clone();
            let crunchy =
                auth::switch_profile(&session.refresh_token, selected, session.device).await?;

            let watchlist = crunchy.watchlist(WatchlistOptions::default()).await?;
            let mut history_count: u32 = 0;
            let mut history = crunchy.watch_history();
            history.page_size(HISTORY_PAGE_SIZE);
            if let Some(total) = history.total().await {
                history_count = total;
            } else {
                // No total from the API: walk every page. Propagate errors, since a
                // failing page is refetched forever rather than skipped.
                while let Some(entry) = history.next().await {
                    entry.context("Failed to read watch history")?;
                    history_count += 1;
                    if history_count.is_multiple_of(HISTORY_PAGE_SIZE) {
                        eprint!("\r  Counting watch history... {history_count}");
                    }
                }
                eprint!("\r\x1b[K");
            }
            let crunchylists = crunchy.crunchylists().await?;

            println!("\nData for '{profile_name}'");
            println!("  Watchlist:     {} items", watchlist.len());
            println!("  Watch history: {history_count} items");
            println!("  Crunchylists:  {} lists", crunchylists.items.len());
        }
        Command::Export {
            cookie,
            profile,
            output_dir,
        } => {
            let crunchy = auth::login(cookie, profile, "", false).await?;
            export::run(&crunchy, &output_dir).await?;
        }
        Command::Import {
            cookie,
            profile,
            input_dir,
            dry_run,
        } => {
            let crunchy = auth::login(cookie, profile, "", true).await?;
            import::run(&crunchy, &input_dir, dry_run).await?;
        }
        Command::Diff {
            cookie,
            profile,
            input_dir,
        } => {
            let crunchy = auth::login(cookie, profile, "", true).await?;
            diff::run(&crunchy, &input_dir).await?;
        }
        Command::RenameProfile {
            cookie,
            profile,
            new_name,
        } => {
            let session = auth::initial_login(cookie, "").await?;
            let mut target = auth::select_profile(&session.profiles, profile, false)?.clone();
            let new_name = new_name.unwrap_or_else(|| {
                dialoguer::Input::new()
                    .with_prompt("New profile name")
                    .interact_text()
                    .expect("Failed to read new name")
            });
            target.change_profile_name(new_name.clone()).await?;
            println!("Renamed to '{new_name}'");
        }
        Command::Migrate {
            source_cookie,
            source_profile,
            target_cookie,
            target_profile,
            data_dir,
            keep_data,
        } => {
            let source_cookie = auth::resolve_cookie(source_cookie, "Source")?;
            let target_cookie = target_cookie.or_else(auth::prompt_target_cookie);
            let same_account = target_cookie.is_none();
            let target_cookie = target_cookie.unwrap_or_else(|| source_cookie.clone());

            println!("=== Step 1: Export from source ===\n");
            let source = auth::login(Some(source_cookie), source_profile, "Source", false).await?;
            let source_profile_id = source.profile_id().await;
            export::run(&source, &data_dir).await?;
            drop(source);

            println!("\n=== Step 2: Login to target ===\n");
            if same_account {
                println!("Same account: pick (or create) the profile to copy into.");
            }
            let target = auth::login(Some(target_cookie), target_profile, "Target", true).await?;
            if same_account && target.profile_id().await == source_profile_id {
                anyhow::bail!("Source and target are the same profile; nothing to migrate");
            }

            println!("=== Step 3: Diff ===");
            diff::run(&target, &data_dir).await?;

            let proceed = dialoguer::Confirm::new()
                .with_prompt("Proceed with import?")
                .default(true)
                .interact()?;

            if !proceed {
                println!("Aborted.");
                return Ok(());
            }

            println!("\n=== Step 4: Import ===\n");
            import::run(&target, &data_dir, false).await?;

            if keep_data {
                println!(
                    "\nMigration complete. Export kept in {}",
                    data_dir.display()
                );
            } else {
                std::fs::remove_dir_all(&data_dir).with_context(|| {
                    format!("Migrated, but failed to delete {}", data_dir.display())
                })?;
                println!("\nMigration complete. Removed {}", data_dir.display());
            }
        }
    }

    Ok(())
}

fn select_command() -> anyhow::Result<Command> {
    let items = [
        "Migrate      Full flow: export -> diff -> confirm -> import",
        "Status       Show account info, profiles, and data counts",
        "Export       Export one profile's data to JSON files",
        "Import       Import from JSON files into a profile",
        "Diff         Compare exported data against target account",
        "Rename       Rename a profile on the account",
    ];

    let idx = dialoguer::Select::new()
        .with_prompt("What would you like to do?")
        .items(items)
        .default(0)
        .interact()
        .context("Selection cancelled")?;

    Ok(match idx {
        0 => Command::Migrate {
            source_cookie: None,
            source_profile: None,
            target_cookie: None,
            target_profile: None,
            data_dir: PathBuf::from("./migration"),
            keep_data: false,
        },
        1 => Command::Status {
            cookie: None,
            profile: None,
        },
        2 => Command::Export {
            cookie: None,
            profile: None,
            output_dir: PathBuf::from("./export"),
        },
        3 => Command::Import {
            cookie: None,
            profile: None,
            input_dir: PathBuf::from("./export"),
            dry_run: false,
        },
        4 => Command::Diff {
            cookie: None,
            profile: None,
            input_dir: PathBuf::from("./export"),
        },
        5 => Command::RenameProfile {
            cookie: None,
            profile: None,
            new_name: None,
        },
        _ => unreachable!(),
    })
}
