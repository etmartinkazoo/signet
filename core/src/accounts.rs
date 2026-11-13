//! The accounts Signet shares with Sealwax. It reads Sealwax's `accounts.toml` (never writes it)
//! and each account's password from the item Sealwax keeps in the keyring, so an account set up
//! once in Sealwax has its calendars here too. Signet's own `config.toml` adds what Sealwax has no
//! reason to know: a CalDAV address when discovery can't find one, or an account to leave out.
//!
//! A password can also be entered in Signet itself: an app password a provider wants for
//! calendars, or any password when Sealwax runs as a flatpak, whose keyring no other app can
//! read. Signet keeps those under its own name and uses them first.

use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use zeroize::Zeroizing;

/// The app-id Sealwax's release build scopes its keyring items with.
pub const SEALWAX_APP_ID: &str = "io.github.pixdevsapps.Sealwax";

fn base_dir(variable: &str, fallback: &str) -> PathBuf {
    match std::env::var_os(variable).map(PathBuf::from) {
        Some(dir) if dir.is_absolute() => dir,
        _ => std::env::home_dir().unwrap_or_default().join(fallback),
    }
}

/// Where Sealwax keeps `accounts.toml`, the first that exists of: the config dir Signet sees
/// (both unsandboxed), the host's `~/.config` (Signet a flatpak, Sealwax not), and the Sealwax
/// flatpak's own config dir. The first is returned when none exists yet.
pub fn sealwax_accounts_file(app_id: &str) -> PathBuf {
    let home = std::env::home_dir().unwrap_or_default();
    let mut candidates = vec![
        base_dir("XDG_CONFIG_HOME", ".config").join("sealwax/accounts.toml"),
        home.join(".config/sealwax/accounts.toml"),
    ];
    // The app-id comes from config.toml: only a plain reverse-DNS name may become a path.
    if !app_id.is_empty() && app_id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        && !app_id.starts_with('.')
    {
        candidates.push(home.join(".var/app").join(app_id).join("config/sealwax/accounts.toml"));
    }
    candidates.iter().find(|path| path.exists()).cloned().unwrap_or_else(|| candidates.swap_remove(0))
}

pub fn signet_config_file() -> PathBuf {
    base_dir("XDG_CONFIG_HOME", ".config").join("signet/config.toml")
}

pub fn signet_data_dir() -> PathBuf {
    base_dir("XDG_DATA_HOME", ".local/share").join("signet")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Auth {
    Password,
    Microsoft,
}

/// One Sealwax account, as much of it as a calendar needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Account {
    pub key: String,
    /// The user's name as Sealwax sends it: the organizer's name on invitations.
    pub name: String,
    pub label: String,
    pub address: String,
    pub username: String,
    pub auth: Auth,
    /// Sealwax's colour name (`"sage"`), for calendars the server gives no colour.
    pub color: Option<String>,
    /// The incoming mail server, a hint for finding the calendar server.
    pub mail_host: Option<String>,
    /// From Signet's `config.toml`: the CalDAV address to use instead of discovery.
    pub caldav: Option<String>,
}

impl Account {
    /// Gmail, whose calendar is Google Calendar and takes Sign in with Google.
    pub fn is_google(&self) -> bool {
        self.caldav.is_none() && crate::google::is_google(&self.address, self.mail_host.as_deref())
    }
}

#[derive(Deserialize)]
struct SealwaxFile {
    #[serde(default)]
    account: Vec<SealwaxAccount>,
}

#[derive(Deserialize)]
struct SealwaxAccount {
    key: String,
    #[serde(default)]
    name: String,
    address: String,
    label: Option<String>,
    username: Option<String>,
    auth: Option<String>,
    color: Option<String>,
    imap: Option<Server>,
    pop3: Option<Server>,
}

#[derive(Deserialize)]
struct Server {
    host: String,
}

/// Whether `text` looks like an email address: one `@` with text on both sides, no spaces.
pub fn is_address(text: &str) -> bool {
    match text.rsplit_once('@') {
        Some((local, domain)) => {
            !local.is_empty() && domain.contains('.') && !domain.starts_with('.') && !domain.ends_with('.')
                && text.len() <= 254 && !text.chars().any(|c| c.is_whitespace() || c.is_control())
        }
        None => false,
    }
}

/// Signet's `config.toml`.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// `"monday"` (the default) or `"sunday"`.
    pub week_start: Option<String>,
    /// The hour the week view opens scrolled to; 7 by default.
    pub day_starts: Option<u32>,
    /// The app-id whose keyring items hold the passwords; Sealwax's release build by default.
    pub sealwax_app_id: Option<String>,
    /// Per Sealwax account key.
    pub account: BTreeMap<String, AccountConfig>,
    /// The OAuth client for Sign in with Google (Google Calendar).
    pub google: Option<crate::google::ClientConfig>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct AccountConfig {
    /// The CalDAV server's address, for a provider discovery doesn't find.
    pub caldav: Option<String>,
    /// The CalDAV login name, when it isn't the mail login.
    pub username: Option<String>,
    /// `false` leaves the account's calendars out.
    pub enabled: Option<bool>,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, String> {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).map_err(|error| format!("{}: {error}", path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(format!("{}: {error}", path.display())),
        }
    }

    pub fn week_starts_sunday(&self) -> bool {
        self.week_start
            .as_deref()
            .is_some_and(|day| day.trim().eq_ignore_ascii_case("sunday"))
    }

    pub fn app_id(&self) -> &str {
        self.sealwax_app_id.as_deref().unwrap_or(SEALWAX_APP_ID)
    }
}

/// Google's CalDAV endpoint that takes a password (an app password), for Google accounts when
/// no OAuth client is set up: the same password Sealwax signs in to Gmail with.
fn google_password_caldav(address: &str) -> String {
    format!("https://www.google.com/calendar/dav/{address}/user/")
}

/// Sealwax's accounts with Signet's settings applied; disabled ones are left out. A missing
/// file is no accounts. A Google account uses Sign in with Google when `[google]` is set up, and
/// its app password over CalDAV otherwise.
pub fn load(path: &Path, config: &Config) -> Result<Vec<Account>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("{}: {error}", path.display())),
    };
    let file: SealwaxFile =
        toml::from_str(&text).map_err(|error| format!("{}: {error}", path.display()))?;
    Ok(file
        .account
        .into_iter()
        .filter_map(|raw| {
            let settings = config.account.get(&raw.key).cloned().unwrap_or_default();
            if settings.enabled == Some(false) {
                return None;
            }
            let auth = match raw.auth.as_deref() {
                Some("microsoft") => Auth::Microsoft,
                _ => Auth::Password,
            };
            let mail_host = raw.imap.or(raw.pop3).map(|server| server.host);
            let caldav = settings.caldav.or_else(|| {
                (config.google.is_none() && crate::google::is_google(&raw.address, mail_host.as_deref()))
                    .then(|| google_password_caldav(&raw.address))
            });
            Some(Account {
                label: raw.label.unwrap_or_else(|| raw.address.clone()),
                username: settings
                    .username
                    .or(raw.username)
                    .unwrap_or_else(|| raw.address.clone()),
                mail_host,
                caldav,
                key: raw.key,
                name: raw.name,
                address: raw.address,
                auth,
                color: raw.color,
            })
        })
        .collect())
}

/// Sealwax's colour names as RGB, for calendars without a colour of their own: each name's hue
/// at a middle lightness that reads on light and dark themes.
pub fn color_rgb(name: &str) -> Option<(f64, f64, f64)> {
    let hue = match name.trim().to_ascii_lowercase().as_str() {
        "rose" => 15.0,
        "clay" => 55.0,
        "olive" => 95.0,
        "sage" => 145.0,
        "teal" => 195.0,
        "slate" => 245.0,
        "iris" => 290.0,
        "mauve" => 335.0,
        _ => return None,
    };
    Some(oklch_to_rgb(0.68, 0.09, hue))
}

fn oklch_to_rgb(lightness: f64, chroma: f64, hue: f64) -> (f64, f64, f64) {
    let (a, b) = (chroma * hue.to_radians().cos(), chroma * hue.to_radians().sin());
    let l = (lightness + 0.396_337_78 * a + 0.215_803_76 * b).powi(3);
    let m = (lightness - 0.105_561_35 * a - 0.063_854_17 * b).powi(3);
    let s = (lightness - 0.089_484_18 * a - 1.291_485_5 * b).powi(3);
    let linear = [
        4.076_741_7 * l - 3.307_711_6 * m + 0.230_969_93 * s,
        -1.268_438 * l + 2.609_757_4 * m - 0.341_319_4 * s,
        -0.004_196_086 * l - 0.703_418_6 * m + 1.707_614_7 * s,
    ];
    let gamma = |c: f64| {
        let c = c.clamp(0.0, 1.0);
        if c <= 0.003_130_8 { 12.92 * c } else { 1.055 * c.powf(1.0 / 2.4) - 0.055 }
    };
    (gamma(linear[0]), gamma(linear[1]), gamma(linear[2]))
}

#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    #[error("the keyring is locked")]
    Locked,
    #[error("there's no password for this account in the keyring")]
    Missing,
    #[error("keyring: {0}")]
    Keyring(String),
}

/// The app-id Signet's own keyring items are kept under, in debug and release builds alike.
pub const SIGNET_APP_ID: &str = "io.brem.Signet";
const GOOGLE_TOKEN: &str = "google-refresh-token";
const PASSWORD: &str = "password";

async fn keyring() -> Result<oo7::Keyring, SecretError> {
    let keyring = tokio::time::timeout(std::time::Duration::from_secs(5), oo7::Keyring::new())
        .await
        .map_err(|_| SecretError::Keyring("no keyring answered".to_owned()))?
        .map_err(|error| SecretError::Keyring(error.to_string()))?;
    if keyring.is_locked().await.map_err(|e| SecretError::Keyring(e.to_string()))? {
        keyring.unlock().await.map_err(|_| SecretError::Locked)?;
    }
    Ok(keyring)
}

/// The Google refresh token Signet keeps for a Sealwax account, if it signed in.
pub async fn google_token(account: &str) -> Result<Option<Zeroizing<String>>, SecretError> {
    let attributes = [("application", SIGNET_APP_ID), ("account", account), ("kind", GOOGLE_TOKEN)];
    let keyring = keyring().await?;
    let items = keyring.search_items(&attributes).await.map_err(|e| SecretError::Keyring(e.to_string()))?;
    let Some(item) = items.first() else { return Ok(None) };
    let secret = item.secret().await.map_err(|e| SecretError::Keyring(e.to_string()))?;
    Ok(String::from_utf8(secret.as_bytes().to_vec()).ok().map(Zeroizing::new))
}

pub async fn store_google_token(account: &str, token: &str) -> Result<(), SecretError> {
    let attributes = [("application", SIGNET_APP_ID), ("account", account), ("kind", GOOGLE_TOKEN)];
    keyring()
        .await?
        .create_item(&format!("Signet: Google Calendar ({account})"), &attributes, oo7::Secret::text(token), true)
        .await
        .map_err(|e| SecretError::Keyring(e.to_string()))
}

pub async fn forget_google_token(account: &str) -> Result<(), SecretError> {
    let attributes = [("application", SIGNET_APP_ID), ("account", account), ("kind", GOOGLE_TOKEN)];
    keyring().await?.delete(&attributes).await.map_err(|e| SecretError::Keyring(e.to_string()))
}

/// The password for `account`: the one entered in Signet if there is one, else Sealwax's.
pub async fn account_password(sealwax_app_id: &str, account: &str) -> Result<Zeroizing<String>, SecretError> {
    let keyring = keyring().await?;
    for app_id in [SIGNET_APP_ID, sealwax_app_id] {
        if let Some(password) = find_password(&keyring, app_id, account).await? {
            return Ok(password);
        }
    }
    Err(SecretError::Missing)
}

/// The Secret Service item `app_id` keeps for `account`'s password, if any.
async fn find_password(
    keyring: &oo7::Keyring,
    app_id: &str,
    account: &str,
) -> Result<Option<Zeroizing<String>>, SecretError> {
    let attributes = [("application", app_id), ("account", account), ("kind", PASSWORD)];
    let items = keyring
        .search_items(&attributes)
        .await
        .map_err(|error| SecretError::Keyring(error.to_string()))?;
    let Some(item) = items.first() else { return Ok(None) };
    let secret = item
        .secret()
        .await
        .map_err(|error| SecretError::Keyring(error.to_string()))?;
    let text = String::from_utf8(secret.as_bytes().to_vec())
        .map_err(|_| SecretError::Keyring("the password isn't text".to_owned()))?;
    Ok(Some(Zeroizing::new(text)))
}

/// Keeps a password entered in Signet for `account`, replacing one entered before. Sealwax's
/// item is never touched.
pub async fn store_password(account: &str, password: &str) -> Result<(), SecretError> {
    let attributes = [("application", SIGNET_APP_ID), ("account", account), ("kind", PASSWORD)];
    keyring()
        .await?
        .create_item(&format!("Signet: calendar password ({account})"), &attributes, oo7::Secret::text(password), true)
        .await
        .map_err(|e| SecretError::Keyring(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEALWAX: &str = r#"
[[account]]
key = "work"
name = "Ada Lovelace"
address = "ada@fastmail.com"
label = "Work"
color = "sage"

[account.imap]
host = "imap.fastmail.com"

[account.smtp]
host = "smtp.fastmail.com"

[[account.identity]]
address = "board@example.com"

[[account]]
key = "old"
name = "Ada"
address = "ada@example.org"
auth = "microsoft"
[account.pop3]
host = "pop.example.org"
[account.smtp]
host = "smtp.example.org"
"#;

    #[test]
    fn reads_sealwax_accounts_with_signets_settings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("accounts.toml");
        std::fs::write(&path, SEALWAX).unwrap();
        let config: Config = toml::from_str(
            "week_start = \"sunday\"\n[account.work]\ncaldav = \"https://dav.example\"\n",
        )
        .unwrap();
        assert!(config.week_starts_sunday());
        let accounts = load(&path, &config).unwrap();
        assert_eq!(accounts.len(), 2);
        assert_eq!(accounts[0].label, "Work");
        assert_eq!(accounts[0].username, "ada@fastmail.com");
        assert_eq!(accounts[0].mail_host.as_deref(), Some("imap.fastmail.com"));
        assert_eq!(accounts[0].caldav.as_deref(), Some("https://dav.example"));
        assert_eq!(accounts[1].auth, Auth::Microsoft);
        assert_eq!(accounts[1].label, "ada@example.org");

        let off: Config = toml::from_str("[account.old]\nenabled = false\n").unwrap();
        assert_eq!(load(&path, &off).unwrap().len(), 1);
        assert!(load(&dir.path().join("none.toml"), &off).unwrap().is_empty());
    }

    #[test]
    fn google_uses_an_app_password_unless_oauth_is_set_up() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("accounts.toml");
        std::fs::write(&path, "[[account]]\nkey = \"g\"\naddress = \"ada@gmail.com\"\n").unwrap();

        let plain = load(&path, &Config::default()).unwrap();
        assert!(!plain[0].is_google());
        assert_eq!(plain[0].caldav.as_deref(), Some("https://www.google.com/calendar/dav/ada@gmail.com/user/"));

        let oauth: Config = toml::from_str("[google]\nclient_id = \"id\"\nclient_secret = \"secret\"\n").unwrap();
        let signed_in = load(&path, &oauth).unwrap();
        assert!(signed_in[0].is_google());
        assert_eq!(signed_in[0].caldav, None);
    }

    #[test]
    fn only_a_plain_app_id_becomes_a_path() {
        for bad in ["", "../..", "a/b", ".hidden"] {
            assert!(!sealwax_accounts_file(bad).to_string_lossy().contains(".var/app"), "{bad}");
        }
    }

    #[test]
    fn a_misspelt_setting_is_reported() {
        assert!(toml::from_str::<Config>("week_strat = \"sunday\"\n").is_err());
    }

    #[test]
    fn sealwax_colours_are_in_gamut() {
        for name in ["rose", "clay", "olive", "sage", "teal", "slate", "iris", "mauve"] {
            let (r, g, b) = color_rgb(name).unwrap();
            assert!([r, g, b].iter().all(|c| (0.0..=1.0).contains(c)), "{name}");
        }
        assert_eq!(color_rgb("plaid"), None);
    }
}
