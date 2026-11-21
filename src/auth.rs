use crate::ui::Session;
use anyhow::{Context, Result};
use crunchyroll_rs::auth::{DeviceIdentifier, SessionToken};
use crunchyroll_rs::profile::Profile;
use crunchyroll_rs::{Crunchyroll, Locale};

const COOKIE_HINT: &str = "Log in at crunchyroll.com, then copy the etp_rt cookie \
                           (devtools > Application/Storage > Cookies).";

pub struct InitialSession {
    pub crunchy: Crunchyroll,
    pub refresh_token: String,
    pub device: DeviceIdentifier,
    pub profiles: Vec<Profile>,
}

/// A profile-scoped session and the profile's display name.
pub struct LoggedIn {
    pub crunchy: Crunchyroll,
    pub profile_name: String,
}

/// Login with an `etp_rt` browser cookie, fetch profiles, but don't switch to a specific profile yet.
/// Crunchyroll no longer accepts email/password from third-party clients.
/// `context` labels prompts (e.g. "Source" -> "Source etp_rt cookie"). Empty for default prompts.
pub async fn initial_login(
    ui: &Session,
    cookie: Option<String>,
    context: &str,
) -> Result<InitialSession> {
    let cookie = resolve_cookie(ui, cookie, context).await?;
    let device = DeviceIdentifier::default();

    ui.status("Signing in…");
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
    ui.status("");

    Ok(InitialSession {
        crunchy,
        refresh_token,
        device,
        profiles: profiles.profiles,
    })
}

/// Use the given cookie, or prompt for one. Errors if it ends up empty.
pub async fn resolve_cookie(ui: &Session, cookie: Option<String>, context: &str) -> Result<String> {
    let raw = match cookie {
        Some(c) => c,
        None => ui.secret(&cookie_label(context), Some(COOKIE_HINT)).await?,
    };
    let cookie = normalize_cookie(&raw);
    if cookie.is_empty() {
        anyhow::bail!("No etp_rt cookie given");
    }
    Ok(cookie)
}

/// Prompt for the migration target's cookie. `None` (empty input) means same account as the source.
pub async fn prompt_target_cookie(ui: &Session) -> Result<Option<String>> {
    let raw = ui
        .secret(
            "Target etp_rt cookie",
            Some("Paste the target account's cookie, or press enter to copy into another profile on the source account."),
        )
        .await?;
    let cookie = normalize_cookie(&raw);
    Ok((!cookie.is_empty()).then_some(cookie))
}

fn cookie_label(context: &str) -> String {
    if context.is_empty() {
        "etp_rt cookie".to_string()
    } else {
        format!("{context} etp_rt cookie")
    }
}

/// Accept the cookie as copied from devtools: bare value, `etp_rt=value`, or with a trailing `;`.
fn normalize_cookie(raw: &str) -> String {
    let s = raw.trim().trim_end_matches(';').trim();
    s.strip_prefix("etp_rt=").unwrap_or(s).trim().to_string()
}

fn profile_label(p: &Profile) -> String {
    let suffix = if p.is_primary { "  (primary)" } else { "" };
    format!("{}{suffix}", p.profile_name)
}

fn profile_prompt(context: &str) -> String {
    if context.is_empty() {
        "Profile".to_string()
    } else {
        format!("{context} profile")
    }
}

/// Select a profile by name (or interactively) and return it.
/// `auto_single` skips the picker when there is only one profile; use it for read-only
/// commands only, so writes always land in a profile the user chose.
pub async fn select_profile<'a>(
    ui: &Session,
    profiles: &'a [Profile],
    profile_name: Option<String>,
    auto_single: bool,
) -> Result<&'a Profile> {
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
            let items = profiles.iter().map(profile_label).collect();
            let idx = ui.select("Profile", items).await?;
            Ok(&profiles[idx])
        }
    }
}

/// Switch to a profile-scoped session.
pub async fn switch_profile(
    ui: &Session,
    refresh_token: &str,
    profile: &Profile,
    device: DeviceIdentifier,
) -> Result<LoggedIn> {
    ui.status(&format!("Opening '{}'…", profile.profile_name));
    let crunchy = Crunchyroll::builder()
        .locale(Locale::en_US)
        .login_with_refresh_token_profile_id(refresh_token, &profile.profile_id, device)
        .await
        .context("Failed to switch profile")?;
    ui.status("");
    Ok(LoggedIn {
        crunchy,
        profile_name: profile.profile_name.clone(),
    })
}

/// Full login flow: cookie -> profile selection -> profile-scoped session.
/// When `allow_create` is true and the profile doesn't exist, offers to create it.
pub async fn login(
    ui: &Session,
    cookie: Option<String>,
    profile_name: Option<String>,
    context: &str,
    allow_create: bool,
) -> Result<LoggedIn> {
    let session = initial_login(ui, cookie, context).await?;
    let can_create = allow_create && session.crunchy.premium().await;

    // When a specific name is given, try to find it or offer to create it
    if let Some(ref name) = profile_name {
        if let Some(profile) = session
            .profiles
            .iter()
            .find(|p| p.profile_name.eq_ignore_ascii_case(name))
        {
            return switch_profile(ui, &session.refresh_token, profile, session.device).await;
        }

        if can_create
            && ui
                .confirm(&format!("Profile '{name}' not found. Create it?"))
                .await?
        {
            return create_and_switch(ui, &session, name.clone()).await;
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
        return switch_profile(
            ui,
            &session.refresh_token,
            &session.profiles[0],
            session.device,
        )
        .await;
    }

    let mut items: Vec<String> = session.profiles.iter().map(profile_label).collect();
    if can_create {
        items.push("+ New profile".to_string());
    }

    let idx = ui.select(&profile_prompt(context), items).await?;
    if idx < session.profiles.len() {
        switch_profile(
            ui,
            &session.refresh_token,
            &session.profiles[idx],
            session.device,
        )
        .await
    } else {
        let name = ui.text("New profile name").await?;
        create_and_switch(ui, &session, name).await
    }
}

async fn create_and_switch(
    ui: &Session,
    session: &InitialSession,
    name: String,
) -> Result<LoggedIn> {
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
                ui.note(&format!("Created profile '{}'", new_profile.profile_name));
                return switch_profile(
                    ui,
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
