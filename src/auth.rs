use anyhow::{Context, Result};
use crunchyroll_rs::auth::{DeviceIdentifier, SessionToken};
use crunchyroll_rs::profile::Profile;
use crunchyroll_rs::{Crunchyroll, Locale};
use dialoguer::Select;

pub struct InitialSession {
    pub crunchy: Crunchyroll,
    pub refresh_token: String,
    pub device: DeviceIdentifier,
    pub profiles: Vec<Profile>,
}

/// Login with an `etp_rt` browser cookie, fetch profiles, but don't switch to a specific profile yet.
/// Crunchyroll no longer accepts email/password from third-party clients.
/// `context` labels prompts (e.g. "Source" -> "Source etp_rt cookie:"). Empty string for default prompts.
pub async fn initial_login(cookie: Option<String>, context: &str) -> Result<InitialSession> {
    let cookie = resolve_cookie(cookie, context)?;

    let device = DeviceIdentifier::default();

    let prefix = if context.is_empty() {
        String::new()
    } else {
        format!("[{context}] ")
    };
    println!("{prefix}Logging in...");
    let crunchy = Crunchyroll::builder()
        .locale(Locale::en_US)
        .login_with_etp_rt(&cookie, device.clone())
        .await
        .context(
            "Failed to login. The etp_rt cookie may be expired; \
             copy a fresh one from a logged-in crunchyroll.com tab",
        )?;

    // etp_rt login hands back a refresh token, which the profile switch reuses
    let refresh_token = match crunchy.session_token().await {
        SessionToken::RefreshToken(t) | SessionToken::EtpRt(t) => t,
        SessionToken::Anonymous => anyhow::bail!("Login returned an anonymous session"),
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

/// Use the given cookie, or prompt for one. Errors if it ends up empty.
pub fn resolve_cookie(cookie: Option<String>, context: &str) -> Result<String> {
    let cookie = normalize_cookie(&cookie.unwrap_or_else(|| prompt_cookie(context)));
    if cookie.is_empty() {
        anyhow::bail!("No etp_rt cookie given");
    }
    Ok(cookie)
}

/// Prompt for the migration target's cookie. `None` (empty input) means same account as the source.
pub fn prompt_target_cookie() -> Option<String> {
    println!(
        "Target account: paste its etp_rt cookie, \
         or leave empty to copy into another profile on the source account."
    );
    let cookie = normalize_cookie(
        &rpassword::prompt_password("Target etp_rt cookie: ").expect("Failed to read cookie"),
    );
    (!cookie.is_empty()).then_some(cookie)
}

/// Accept the cookie as copied from devtools: bare value, `etp_rt=value`, or with a trailing `;`.
fn normalize_cookie(raw: &str) -> String {
    let s = raw.trim().trim_end_matches(';').trim();
    s.strip_prefix("etp_rt=").unwrap_or(s).trim().to_string()
}

/// Display name of the session's profile, falling back to its id.
pub async fn current_profile_name(crunchy: &Crunchyroll) -> String {
    let id = crunchy.profile_id().await;
    match crunchy.profiles().await {
        Ok(p) => p
            .profiles
            .into_iter()
            .find(|p| p.profile_id == id)
            .map_or(id, |p| p.profile_name),
        Err(_) => id,
    }
}

/// Select a profile by name (or interactively) and return it.
/// `auto_single` skips the picker when there is only one profile; use it for read-only
/// commands only, so writes always land in a profile the user chose.
pub fn select_profile(
    profiles: &[Profile],
    profile_name: Option<String>,
    auto_single: bool,
) -> Result<&Profile> {
    if profiles.is_empty() {
        anyhow::bail!("No profiles found on this account");
    }

    match profile_name {
        Some(name) => profiles
            .iter()
            .find(|p| p.profile_name.eq_ignore_ascii_case(&name))
            .with_context(|| {
                let names: Vec<_> = profiles.iter().map(|p| &p.profile_name).collect();
                format!("Profile '{name}' not found. Available: {names:?}")
            }),
        None if auto_single && profiles.len() == 1 => Ok(&profiles[0]),
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
/// `context` labels prompts (e.g. "Source" -> "Source etp_rt cookie:"). Empty string for default prompts.
pub async fn login(
    cookie: Option<String>,
    profile_name: Option<String>,
    context: &str,
    allow_create: bool,
) -> Result<Crunchyroll> {
    let session = initial_login(cookie, context).await?;
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
                .with_prompt(format!("Profile '{name}' not found. Create it?"))
                .default(false)
                .interact()
                .context("Profile creation cancelled")?;

            if create {
                return create_and_switch(&session, name.clone()).await;
            }
        }

        let names: Vec<_> = session.profiles.iter().map(|p| &p.profile_name).collect();
        anyhow::bail!("Profile '{name}' not found. Available: {names:?}");
    }

    // Interactive mode: show profiles (+ "create new" when premium)
    if session.profiles.is_empty() {
        anyhow::bail!("No profiles found on this account");
    }
    // Only read-only logins skip the picker; a write target is always chosen explicitly
    if session.profiles.len() == 1 && !allow_create {
        return switch_profile(&session.refresh_token, &session.profiles[0], session.device).await;
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
    let profiles = session.crunchy.profiles().await?;
    let base = username_base(&name);

    // Usernames are unique across all of Crunchyroll, not per account: try the plain
    // name first, then add a random suffix.
    const ATTEMPTS: u32 = 4;
    for attempt in 0..ATTEMPTS {
        let username = if attempt == 0 {
            base.clone()
        } else {
            format!("{base}_{}", random_suffix())
        };
        match profiles.new_profile(name.clone(), username).await {
            Ok(new_profile) => {
                println!("Created profile '{}'", new_profile.profile_name);
                return switch_profile(
                    &session.refresh_token,
                    &new_profile,
                    session.device.clone(),
                )
                .await;
            }
            Err(e) => {
                let msg = format!("{e}");
                if msg.contains("not_unique") && attempt + 1 < ATTEMPTS {
                    continue;
                }
                if msg.contains("invalid_auth_token") {
                    anyhow::bail!(
                        "Cannot create profiles on this account (multi-profile requires a premium plan).\n\
                         Use an existing profile instead, or upgrade the account."
                    );
                }
                return Err(e).context("Failed to create profile");
            }
        }
    }
    unreachable!()
}

/// Lowercase ASCII letters, digits and underscores from the profile name, kept short
/// so a suffix still fits.
fn username_base(name: &str) -> String {
    let base: String = name
        .to_lowercase()
        .chars()
        .map(|c| if c == ' ' { '_' } else { c })
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
        .take(12)
        .collect();
    if base.is_empty() {
        "profile".to_string()
    } else {
        base
    }
}

fn random_suffix() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or_default();
    format!("{:04x}", (nanos ^ std::process::id()) & 0xffff)
}

fn prompt_cookie(context: &str) -> String {
    println!(
        "Log in at crunchyroll.com, then copy the etp_rt cookie \
         (devtools > Application/Storage > Cookies)."
    );
    let prompt = if context.is_empty() {
        "etp_rt cookie: ".to_string()
    } else {
        format!("{context} etp_rt cookie: ")
    };
    rpassword::prompt_password(prompt).expect("Failed to read cookie")
}

#[cfg(test)]
mod tests {
    use super::{normalize_cookie, username_base};

    #[test]
    fn username_base_is_short_and_safe() {
        assert_eq!(username_base("Sean Copy"), "sean_copy");
        assert_eq!(username_base("Émilie's list!!"), "milies_list");
        assert_eq!(username_base("a very long profile name"), "a_very_long_");
        assert_eq!(username_base("🍣"), "profile");
    }

    #[test]
    fn normalize_cookie_accepts_devtools_formats() {
        assert_eq!(normalize_cookie("abc-123"), "abc-123");
        assert_eq!(normalize_cookie("  etp_rt=abc-123; \n"), "abc-123");
        assert_eq!(normalize_cookie("etp_rt=abc-123"), "abc-123");
        assert_eq!(normalize_cookie("   "), "");
    }
}
