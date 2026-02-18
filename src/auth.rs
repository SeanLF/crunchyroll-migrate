use anyhow::{Context, Result};
use crunchyroll_rs::crunchyroll::{DeviceIdentifier, SessionToken};
use crunchyroll_rs::profile::Profile;
use crunchyroll_rs::{Crunchyroll, Locale};
use dialoguer::Select;

pub struct InitialSession {
    pub crunchy: Crunchyroll,
    pub refresh_token: String,
    pub device: DeviceIdentifier,
    pub profiles: Vec<Profile>,
}

/// Login with credentials, fetch profiles, but don't switch to a specific profile yet.
/// `context` labels prompts (e.g. "Source" -> "Source email:"). Empty string for default prompts.
pub async fn initial_login(
    email: Option<String>,
    password: Option<String>,
    context: &str,
) -> Result<InitialSession> {
    let email = email.unwrap_or_else(|| prompt_email(context));
    let password = password.unwrap_or_else(|| prompt_password(context));

    let device = DeviceIdentifier::default();

    let prefix = if context.is_empty() {
        String::new()
    } else {
        format!("[{}] ", context)
    };
    println!("{}Logging in as {}...", prefix, email);
    let crunchy = Crunchyroll::builder()
        .locale(Locale::en_US)
        .login_with_credentials(&email, &password, device.clone())
        .await
        .context("Failed to login")?;

    let token = crunchy.session_token().await;
    let refresh_token = match &token {
        SessionToken::RefreshToken(t) => t.clone(),
        _ => anyhow::bail!("Expected refresh token from login"),
    };

    let profiles = crunchy
        .profiles()
        .await
        .context("Failed to fetch profiles")?;

    Ok(InitialSession {
        crunchy,
        refresh_token,
        device,
        profiles: profiles.profiles,
    })
}

/// Select a profile by name (or interactively) and return it.
pub fn select_profile(profiles: &[Profile], profile_name: Option<String>) -> Result<&Profile> {
    if profiles.is_empty() {
        anyhow::bail!("No profiles found on this account");
    }

    match profile_name {
        Some(name) => profiles
            .iter()
            .find(|p| p.profile_name.eq_ignore_ascii_case(&name))
            .with_context(|| {
                let names: Vec<_> = profiles.iter().map(|p| &p.profile_name).collect();
                format!("Profile '{}' not found. Available: {:?}", name, names)
            }),
        None => {
            let items: Vec<String> = profiles
                .iter()
                .map(|p| {
                    let suffix = if p.is_primary { " (primary)" } else { "" };
                    format!("{}{}", p.profile_name, suffix)
                })
                .collect();

            let idx = Select::new()
                .with_prompt("Select profile")
                .items(&items)
                .default(0)
                .interact()
                .context("Profile selection cancelled")?;

            Ok(&profiles[idx])
        }
    }
}

/// Switch to a profile-scoped session.
pub async fn switch_profile(
    refresh_token: &str,
    profile: &Profile,
    device: DeviceIdentifier,
) -> Result<Crunchyroll> {
    println!("Switching to profile '{}'...", profile.profile_name);
    let crunchy = Crunchyroll::builder()
        .locale(Locale::en_US)
        .login_with_refresh_token_profile_id(refresh_token, &profile.profile_id, device)
        .await
        .context("Failed to switch profile")?;

    println!("Authenticated as '{}'\n", profile.profile_name);
    Ok(crunchy)
}

/// Full login flow: credentials -> profile selection -> profile-scoped session.
/// When `allow_create` is true and the profile doesn't exist, offers to create it.
/// `context` labels prompts (e.g. "Source" -> "Source email:"). Empty string for default prompts.
pub async fn login(
    email: Option<String>,
    password: Option<String>,
    profile_name: Option<String>,
    context: &str,
    allow_create: bool,
) -> Result<Crunchyroll> {
    let session = initial_login(email, password, context).await?;
    let can_create = allow_create && session.crunchy.premium().await;

    // When a specific name is given, try to find it or offer to create it
    if let Some(ref name) = profile_name {
        if let Some(profile) = session
            .profiles
            .iter()
            .find(|p| p.profile_name.eq_ignore_ascii_case(name))
        {
            return switch_profile(&session.refresh_token, profile, session.device).await;
        }

        if can_create {
            let create = dialoguer::Confirm::new()
                .with_prompt(format!("Profile '{}' not found. Create it?", name))
                .default(false)
                .interact()
                .context("Profile creation cancelled")?;

            if create {
                return create_and_switch(&session, name.clone()).await;
            }
        }

        let names: Vec<_> = session.profiles.iter().map(|p| &p.profile_name).collect();
        anyhow::bail!("Profile '{}' not found. Available: {:?}", name, names);
    }

    // Interactive mode: show profiles (+ "create new" when premium)
    if session.profiles.is_empty() {
        anyhow::bail!("No profiles found on this account");
    }

    let mut items: Vec<String> = session
        .profiles
        .iter()
        .map(|p| {
            let suffix = if p.is_primary { " (primary)" } else { "" };
            format!("{}{}", p.profile_name, suffix)
        })
        .collect();
    if can_create {
        items.push("+ Create new profile".to_string());
    }

    let idx = Select::new()
        .with_prompt("Select profile")
        .items(&items)
        .default(0)
        .interact()
        .context("Profile selection cancelled")?;

    if idx < session.profiles.len() {
        switch_profile(
            &session.refresh_token,
            &session.profiles[idx],
            session.device,
        )
        .await
    } else {
        let name: String = dialoguer::Input::new()
            .with_prompt("Profile name")
            .interact_text()
            .context("Failed to read profile name")?;
        create_and_switch(&session, name).await
    }
}

async fn create_and_switch(session: &InitialSession, name: String) -> Result<Crunchyroll> {
    let username = name.to_lowercase().replace(' ', "_");

    let profiles = session.crunchy.profiles().await?;
    match profiles.new_profile(name.clone(), username).await {
        Ok(new_profile) => {
            println!("Created profile '{}'", new_profile.profile_name);
            switch_profile(&session.refresh_token, &new_profile, session.device.clone()).await
        }
        Err(e) => {
            let msg = format!("{}", e);
            if msg.contains("invalid_auth_token") {
                anyhow::bail!(
                    "Cannot create profiles on this account (multi-profile requires a premium plan).\n\
                     Use an existing profile instead, or upgrade the account."
                );
            }
            Err(e).context("Failed to create profile")
        }
    }
}

fn prompt_email(context: &str) -> String {
    let prompt = if context.is_empty() {
        "Email".to_string()
    } else {
        format!("{} email", context)
    };
    dialoguer::Input::new()
        .with_prompt(prompt)
        .interact_text()
        .expect("Failed to read email")
}

fn prompt_password(context: &str) -> String {
    let prompt = if context.is_empty() {
        "Password: ".to_string()
    } else {
        format!("{} password: ", context)
    };
    rpassword::prompt_password(prompt).expect("Failed to read password")
}
