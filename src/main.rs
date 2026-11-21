mod auth;
mod export;
mod import;
mod models;
mod ui;

use anyhow::Context;
use clap::{Parser, Subcommand};
use crunchyroll_rs::list::WatchlistOptions;
use futures_util::StreamExt;
use std::path::{Path, PathBuf};
use ui::{Session, Stage};

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
    // Ensure terminal state is restored on panic (raw mode, bracketed paste, cursor)
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        ui::restore_terminal();
        original_hook(info);
    }));

    let cli = Cli::parse();

    let command = match cli.command {
        Some(cmd) => cmd,
        None => select_command()?,
    };

    match command {
        Command::Status { cookie, profile } => status(cookie, profile).await?,
        Command::Export {
            cookie,
            profile,
            output_dir,
        } => {
            let ui = Session::start("Export", &[Stage::SignIn, Stage::Read]);
            ui.stage(Stage::SignIn);
            let src = auth::login(&ui, cookie, profile, "", false).await?;
            ui.route(&src.profile_name);
            export::run(&src.crunchy, &src.profile_name, &output_dir, &ui).await?;
            ui.finish();
            println!("Saved to {}", display_path(&output_dir));
        }
        Command::Import {
            cookie,
            profile,
            input_dir,
            dry_run,
        } => {
            let exports = import::Exports::load(&input_dir)?;
            let steps: &[Stage] = if dry_run {
                &[Stage::SignIn, Stage::Read, Stage::Compare]
            } else {
                &[Stage::SignIn, Stage::Read, Stage::Compare, Stage::Copy]
            };
            let ui = Session::start(
                if dry_run {
                    "Import (dry run)"
                } else {
                    "Import"
                },
                steps,
            );
            ui.stage(Stage::SignIn);
            let dst = auth::login(&ui, cookie, profile, "", true).await?;
            ui.route(&format!(
                "{} → {}",
                exports.watchlist.metadata.profile_name, dst.profile_name
            ));
            exports.report_source(&ui);
            let target = import::compare(&dst.crunchy, &exports, &ui).await?;
            if !dry_run {
                import::copy(&dst.crunchy, &exports, &target, &ui).await?;
            }
            ui.finish();
        }
        Command::Diff {
            cookie,
            profile,
            input_dir,
        } => {
            let exports = import::Exports::load(&input_dir)?;
            let ui = Session::start("Diff", &[Stage::SignIn, Stage::Read, Stage::Compare]);
            ui.stage(Stage::SignIn);
            let dst = auth::login(&ui, cookie, profile, "", true).await?;
            ui.route(&format!(
                "{} → {}",
                exports.watchlist.metadata.profile_name, dst.profile_name
            ));
            exports.report_source(&ui);
            import::compare(&dst.crunchy, &exports, &ui).await?;
            ui.finish();
        }
        Command::RenameProfile {
            cookie,
            profile,
            new_name,
        } => {
            let ui = Session::prompts_only("Rename profile");
            let session = auth::initial_login(&ui, cookie, "").await?;
            let mut target = auth::select_profile(&ui, &session.profiles, profile, false)
                .await?
                .clone();
            let new_name = match new_name {
                Some(n) => n,
                None => ui.text("New profile name").await?,
            };
            let old_name = target.profile_name.clone();
            target.change_profile_name(new_name.clone()).await?;
            ui.finish();
            println!("Renamed '{old_name}' to '{new_name}'");
        }
        Command::Migrate {
            source_cookie,
            source_profile,
            target_cookie,
            target_profile,
            data_dir,
            keep_data,
        } => {
            migrate(
                source_cookie,
                source_profile,
                target_cookie,
                target_profile,
                &data_dir,
                keep_data,
            )
            .await?;
        }
    }

    Ok(())
}

fn display_path(p: &Path) -> String {
    std::fs::canonicalize(p)
        .unwrap_or_else(|_| p.to_path_buf())
        .display()
        .to_string()
}

async fn migrate(
    source_cookie: Option<String>,
    source_profile: Option<String>,
    target_cookie: Option<String>,
    target_profile: Option<String>,
    data_dir: &Path,
    keep_data: bool,
) -> anyhow::Result<()> {
    let ui = Session::start(
        "Migrate",
        &[
            Stage::Source,
            Stage::Read,
            Stage::Target,
            Stage::Compare,
            Stage::Copy,
        ],
    );

    ui.stage(Stage::Source);
    let source_cookie = auth::resolve_cookie(&ui, source_cookie, "Source").await?;
    let src = auth::login(
        &ui,
        Some(source_cookie.clone()),
        source_profile,
        "Source",
        false,
    )
    .await?;
    ui.route(&src.profile_name);
    let source_profile_id = src.crunchy.profile_id().await;
    export::run(&src.crunchy, &src.profile_name, data_dir, &ui).await?;
    drop(src.crunchy);

    ui.stage(Stage::Target);
    let target_cookie = match target_cookie {
        Some(c) => Some(c),
        None => auth::prompt_target_cookie(&ui).await?,
    };
    let same_account = target_cookie.is_none();
    let target_cookie = target_cookie.unwrap_or(source_cookie);
    let dst = auth::login(&ui, Some(target_cookie), target_profile, "Target", true).await?;
    if same_account && dst.crunchy.profile_id().await == source_profile_id {
        anyhow::bail!("Source and target are the same profile; nothing to migrate");
    }
    ui.route(&format!("{} → {}", src.profile_name, dst.profile_name));

    let exports = import::Exports::load(data_dir)?;
    let target = import::compare(&dst.crunchy, &exports, &ui).await?;

    let to_copy = ui.total_to_copy();
    if to_copy == 0 {
        ui.note(&format!("'{}' already has everything", dst.profile_name));
    } else if ui
        .confirm(&format!(
            "Copy {to_copy} items into '{}'?",
            dst.profile_name
        ))
        .await?
    {
        import::copy(&dst.crunchy, &exports, &target, &ui).await?;
    } else {
        ui.finish();
        println!("Nothing copied.");
        return Ok(());
    }
    ui.finish();

    if keep_data {
        println!("Export kept in {}", display_path(data_dir));
    } else {
        std::fs::remove_dir_all(data_dir)
            .with_context(|| format!("Migrated, but failed to delete {}", data_dir.display()))?;
    }
    Ok(())
}

async fn status(cookie: Option<String>, profile: Option<String>) -> anyhow::Result<()> {
    let ui = Session::prompts_only("Status");
    let session = auth::initial_login(&ui, cookie, "").await?;
    let is_premium = session.crunchy.premium().await;
    let selected = auth::select_profile(&ui, &session.profiles, profile, true).await?;
    let src = auth::switch_profile(
        &ui,
        &session.refresh_token,
        selected,
        session.device.clone(),
    )
    .await?;

    ui.status("Reading watchlist…");
    let watchlist = src.crunchy.watchlist(WatchlistOptions::default()).await?;
    let mut history_count: u32 = 0;
    let mut history = src.crunchy.watch_history();
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
                ui.status(&format!("Counting watch history… {history_count}"));
            }
        }
    }
    let crunchylists = src.crunchy.crunchylists().await?;
    ui.finish();

    println!("Account");
    println!("  Premium   {}", if is_premium { "yes" } else { "no" });
    println!("  Profiles  {}", session.profiles.len());
    for p in &session.profiles {
        let marker = if p.profile_id == selected.profile_id {
            "›"
        } else {
            " "
        };
        let primary = if p.is_primary { "  (primary)" } else { "" };
        println!("    {marker} {}{primary}", p.profile_name);
    }
    println!("\n'{}'", src.profile_name);
    println!("  Watchlist      {}", watchlist.len());
    println!("  Watch history  {history_count}");
    println!("  Crunchylists   {}", crunchylists.items.len());
    Ok(())
}

fn select_command() -> anyhow::Result<Command> {
    let items = [
        "Migrate      Copy a profile into another profile or account",
        "Status       Show account info, profiles, and data counts",
        "Export       Save a profile's data to JSON files",
        "Import       Copy JSON files into a profile",
        "Diff         Compare JSON files against a profile",
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
