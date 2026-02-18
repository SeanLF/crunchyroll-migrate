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
        #[arg(long)]
        email: Option<String>,
        #[arg(long)]
        password: Option<String>,
        #[arg(long)]
        profile: Option<String>,
    },

    /// Export one profile's data to JSON files
    Export {
        #[arg(long)]
        email: Option<String>,
        #[arg(long)]
        password: Option<String>,
        #[arg(long)]
        profile: Option<String>,
        #[arg(long, default_value = "./export")]
        output_dir: PathBuf,
    },

    /// Import from JSON files into a profile
    Import {
        #[arg(long)]
        email: Option<String>,
        #[arg(long)]
        password: Option<String>,
        #[arg(long)]
        profile: Option<String>,
        #[arg(long, default_value = "./export")]
        input_dir: PathBuf,
        #[arg(long)]
        dry_run: bool,
    },

    /// Compare exported data against target account
    Diff {
        #[arg(long)]
        email: Option<String>,
        #[arg(long)]
        password: Option<String>,
        #[arg(long)]
        profile: Option<String>,
        #[arg(long, short = 'i', default_value = "./export")]
        input_dir: PathBuf,
    },

    /// Rename a profile on the account
    RenameProfile {
        #[arg(long)]
        email: Option<String>,
        #[arg(long)]
        password: Option<String>,
        #[arg(long)]
        profile: Option<String>,
        #[arg(long)]
        new_name: Option<String>,
    },

    /// Full flow: export -> diff -> confirm -> import
    Migrate {
        #[arg(long)]
        source_email: Option<String>,
        #[arg(long)]
        source_password: Option<String>,
        #[arg(long)]
        source_profile: Option<String>,
        #[arg(long)]
        target_email: Option<String>,
        #[arg(long)]
        target_password: Option<String>,
        #[arg(long)]
        target_profile: Option<String>,
        #[arg(long, default_value = "./migration")]
        data_dir: PathBuf,
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
        Command::Status {
            email,
            password,
            profile,
        } => {
            let session = auth::initial_login(email, password, "").await?;
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

            let show_data = if profile.is_some() {
                true
            } else {
                dialoguer::Confirm::new()
                    .with_prompt("Show data counts for a profile?")
                    .default(true)
                    .interact()?
            };

            if show_data {
                let selected = auth::select_profile(&session.profiles, profile)?;
                let profile_name = selected.profile_name.clone();
                let crunchy =
                    auth::switch_profile(&session.refresh_token, selected, session.device).await?;

                let watchlist = crunchy.watchlist(WatchlistOptions::default()).await?;
                let mut history_count: u32 = 0;
                let mut history = crunchy.watch_history();
                if let Some(total) = history.total().await {
                    history_count = total;
                } else {
                    while history.next().await.is_some() {
                        history_count += 1;
                    }
                }
                let crunchylists = crunchy.crunchylists().await?;

                println!("\nData for '{}'", profile_name);
                println!("  Watchlist:     {} items", watchlist.len());
                println!("  Watch history: {} items", history_count);
                println!("  Crunchylists:  {} lists", crunchylists.items.len());
            }
        }
        Command::Export {
            email,
            password,
            profile,
            output_dir,
        } => {
            let crunchy = auth::login(email, password, profile, "", false).await?;
            export::run(&crunchy, &output_dir).await?;
        }
        Command::Import {
            email,
            password,
            profile,
            input_dir,
            dry_run,
        } => {
            let crunchy = auth::login(email, password, profile, "", true).await?;
            import::run(&crunchy, &input_dir, dry_run).await?;
        }
        Command::Diff {
            email,
            password,
            profile,
            input_dir,
        } => {
            let crunchy = auth::login(email, password, profile, "", true).await?;
            diff::run(&crunchy, &input_dir).await?;
        }
        Command::RenameProfile {
            email,
            password,
            profile,
            new_name,
        } => {
            let session = auth::initial_login(email, password, "").await?;
            let mut target = auth::select_profile(&session.profiles, profile)?.clone();
            let new_name = new_name.unwrap_or_else(|| {
                dialoguer::Input::new()
                    .with_prompt("New profile name")
                    .interact_text()
                    .expect("Failed to read new name")
            });
            target.change_profile_name(new_name.clone()).await?;
            println!("Renamed to '{}'", new_name);
        }
        Command::Migrate {
            source_email,
            source_password,
            source_profile,
            target_email,
            target_password,
            target_profile,
            data_dir,
        } => {
            println!("=== Step 1: Export from source ===\n");
            let source = auth::login(
                source_email,
                source_password,
                source_profile,
                "Source",
                false,
            )
            .await?;
            export::run(&source, &data_dir).await?;
            drop(source);

            println!("\n=== Step 2: Login to target ===\n");
            let target = auth::login(
                target_email,
                target_password,
                target_profile,
                "Target",
                true,
            )
            .await?;

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

            println!("\nMigration complete.");
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
        .items(&items)
        .default(0)
        .interact()
        .context("Selection cancelled")?;

    Ok(match idx {
        0 => Command::Migrate {
            source_email: None,
            source_password: None,
            source_profile: None,
            target_email: None,
            target_password: None,
            target_profile: None,
            data_dir: PathBuf::from("./migration"),
        },
        1 => Command::Status {
            email: None,
            password: None,
            profile: None,
        },
        2 => Command::Export {
            email: None,
            password: None,
            profile: None,
            output_dir: PathBuf::from("./export"),
        },
        3 => Command::Import {
            email: None,
            password: None,
            profile: None,
            input_dir: PathBuf::from("./export"),
            dry_run: false,
        },
        4 => Command::Diff {
            email: None,
            password: None,
            profile: None,
            input_dir: PathBuf::from("./export"),
        },
        5 => Command::RenameProfile {
            email: None,
            password: None,
            profile: None,
            new_name: None,
        },
        _ => unreachable!(),
    })
}
