//! What the window asks for: refreshing every account's calendars into the store, and writing
//! an event to its server (or to the local calendar) before the store shows it.

use crate::accounts::{self, Account, Auth, Config};
use crate::caldav::{self, Client, Condition, Credentials, RemoteCalendar};
use crate::google;
use crate::store::{Calendar, Store, StoredResource};
use chrono::{DateTime, Duration, Utc};
use chrono_tz::Tz;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use url::Url;

/// How far back and ahead of the day in view a refresh reads.
const BEHIND: i64 = 120;
const AHEAD: i64 = 365;

/// An account whose calendars couldn't be refreshed, and why, in words for the window.
#[derive(Clone, Debug)]
pub struct Problem {
    pub account: String,
    pub message: String,
    /// The account key when signing in (with Google) would fix it.
    pub sign_in: Option<String>,
}

/// Why an account couldn't be refreshed.
enum Failure {
    Message(String),
    SignIn(String),
}

impl From<String> for Failure {
    fn from(message: String) -> Self {
        Self::Message(message)
    }
}

pub struct Service {
    pub store: Store,
    pub config: Config,
    pub local: Tz,
    accounts: Mutex<Vec<Account>>,
    clients: tokio::sync::Mutex<HashMap<String, Arc<Client>>>,
}

impl Service {
    /// Opens the store in Signet's data directory and reads the settings and Sealwax's accounts.
    /// A settings file with a mistake is reported, and the defaults are used meanwhile.
    pub fn open() -> Result<(Self, Vec<String>), String> {
        let dir = accounts::signet_data_dir();
        std::fs::create_dir_all(&dir).map_err(|error| format!("{}: {error}", dir.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
        }
        let store = Store::open(&dir.join("signet.db")).map_err(|error| error.to_string())?;
        let mut warnings = Vec::new();
        let config = Config::load(&accounts::signet_config_file()).unwrap_or_else(|error| {
            warnings.push(error);
            Config::default()
        });
        Ok((Self::with(store, config), warnings))
    }

    pub fn with(store: Store, config: Config) -> Self {
        Self {
            store,
            config,
            local: crate::event::local_zone(),
            accounts: Mutex::new(Vec::new()),
            clients: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Sealwax's accounts as last read.
    pub fn accounts(&self) -> Vec<Account> {
        self.accounts.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    pub fn account_label(&self, key: &str) -> String {
        if key == crate::store::LOCAL_ACCOUNT {
            return "On This Computer".to_owned();
        }
        self.accounts()
            .into_iter()
            .find(|account| account.key == key)
            .map_or_else(|| key.to_owned(), |account| account.label)
    }

    pub fn account_color(&self, key: &str) -> Option<String> {
        self.accounts()
            .into_iter()
            .find(|account| account.key == key)
            .and_then(|account| account.color)
    }

    fn window(around: DateTime<Utc>) -> (DateTime<Utc>, DateTime<Utc>) {
        (around - Duration::days(BEHIND), around + Duration::days(AHEAD))
    }

    /// Whether every shown server calendar's stored events cover `from..to`.
    pub fn covers(&self, from: DateTime<Utc>, to: DateTime<Utc>) -> bool {
        self.store.calendars().is_ok_and(|calendars| {
            calendars
                .iter()
                .filter(|calendar| !calendar.is_local() && calendar.visible)
                .all(|calendar| calendar.synced.is_some_and(|(start, end)| start <= from && end >= to))
        })
    }

    /// Reads Sealwax's accounts again and refreshes each one's calendars around `around`, all
    /// at once. Calendars whose tag is unchanged and whose stored range covers `around` are not
    /// read again.
    pub async fn refresh(self: &Arc<Self>, around: DateTime<Utc>) -> Vec<Problem> {
        let mut problems = Vec::new();
        let found = accounts::load(&accounts::sealwax_accounts_file(), &self.config);
        let list = match found {
            Ok(list) => list,
            Err(message) => {
                problems.push(Problem { account: "Sealwax accounts".to_owned(), message, sign_in: None });
                self.accounts()
            }
        };
        let keys: Vec<String> = list.iter().map(|account| account.key.clone()).collect();
        if let Err(error) = self.store.retain_accounts(&keys) {
            tracing::warn!("{error}");
        }
        *self.accounts.lock().unwrap_or_else(|p| p.into_inner()) = list.clone();

        let mut tasks = tokio::task::JoinSet::new();
        for account in list {
            let service = Arc::clone(self);
            tasks.spawn(async move {
                let result = service.refresh_account(&account, around).await;
                (account, result)
            });
        }
        while let Some(done) = tasks.join_next().await {
            match done {
                Ok((account, Err(Failure::Message(message)))) => {
                    problems.push(Problem { account: account.label, message, sign_in: None });
                }
                Ok((account, Err(Failure::SignIn(message)))) => {
                    problems.push(Problem { account: account.label, message, sign_in: Some(account.key) });
                }
                _ => {}
            }
        }
        problems
    }

    async fn client(&self, account: &Account) -> Result<Arc<Client>, String> {
        let mut clients = self.clients.lock().await;
        if let Some(client) = clients.get(&account.key) {
            return Ok(Arc::clone(client));
        }
        let credentials = if account.is_google() {
            let config = self.config.google.clone().ok_or_else(|| google::Error::NotConfigured.to_string())?;
            let refresh = accounts::google_token(&account.key)
                .await
                .map_err(|error| error.to_string())?
                .ok_or_else(|| google::Error::SignInNeeded.to_string())?;
            let tokens = google::Tokens::new(config, &account.key, refresh).map_err(|e| e.to_string())?;
            Credentials::Google(Arc::new(tokens))
        } else {
            let password = accounts::sealwax_password(self.config.app_id(), &account.key)
                .await
                .map_err(|error| error.to_string())?;
            Credentials::Password { username: account.username.clone(), password }
        };
        let client = Arc::new(Client::new(credentials).map_err(|e| e.to_string())?);
        clients.insert(account.key.clone(), Arc::clone(&client));
        Ok(client)
    }

    /// The calendar home: the one found before, else found now and kept.
    async fn home(&self, account: &Account, client: &Client) -> Result<Url, String> {
        if let Some(home) = self.store.home(&account.key).ok().flatten().and_then(|h| Url::parse(&h).ok()) {
            return Ok(home);
        }
        let candidates = caldav::candidates(&account.address, account.mail_host.as_deref(), account.caldav.as_deref())
            .map_err(|error| error.to_string())?;
        let mut last = caldav::Error::NoServer;
        for start in candidates {
            match client.discover(&start).await {
                Ok(home) => {
                    let _ = self.store.set_home(&account.key, Some(home.as_str()));
                    return Ok(home);
                }
                Err(caldav::Error::Unauthorized) => return Err(unauthorized()),
                Err(error) => {
                    tracing::debug!("no CalDAV at {start}: {error}");
                    last = error;
                }
            }
        }
        Err(match last {
            caldav::Error::NotFound | caldav::Error::Status(_) | caldav::Error::Network(_) => {
                "no calendar server was found; set its address as caldav = \"https://…\" under \
                 [account.<key>] in ~/.config/signet/config.toml"
                    .to_owned()
            }
            other => other.to_string(),
        })
    }

    async fn refresh_account(&self, account: &Account, around: DateTime<Utc>) -> Result<(), Failure> {
        if account.auth == Auth::Microsoft {
            return Err("Microsoft calendars aren't supported yet".to_owned().into());
        }
        let google_sign_in = |message: String| {
            if account.is_google() && message == google::Error::SignInNeeded.to_string() {
                Failure::SignIn(message)
            } else {
                Failure::Message(message)
            }
        };
        let client = self.client(account).await.map_err(google_sign_in)?;
        let remote = if account.is_google() {
            self.google_calendars(account, &client).await?
        } else {
            let home = self.home(account, &client).await?;
            match client.calendars(&home).await {
                Ok(remote) => remote,
                Err(caldav::Error::Unauthorized) => return Err(unauthorized().into()),
                Err(error) => {
                    // The home may have moved: look for it again next time.
                    let _ = self.store.set_home(&account.key, None);
                    return Err(error.to_string().into());
                }
            }
        };
        let window = Self::window(around);
        let stored: HashMap<String, Calendar> = self
            .store
            .calendars()
            .map_err(|e| e.to_string())?
            .into_iter()
            .filter(|calendar| calendar.account == account.key)
            .map(|calendar| (calendar.url.clone(), calendar))
            .collect();
        let mut keep = Vec::new();
        for calendar in remote {
            let id = self
                .store
                .upsert_calendar(&account.key, calendar.url.as_str(), &calendar.name, calendar.color.as_deref(), calendar.read_only)
                .map_err(|e| e.to_string())?;
            keep.push(id);
            let before = stored.get(calendar.url.as_str());
            let fresh = before.is_some_and(|before| {
                before.ctag.is_some()
                    && before.ctag == calendar.ctag
                    && before.synced.is_some_and(|(from, to)| {
                        from <= around - Duration::days(30) && to >= around + Duration::days(60)
                    })
            });
            if fresh {
                continue;
            }
            let events = match client.events(&calendar.url, window.0, window.1).await {
                Ok(events) => events,
                Err(caldav::Error::SignIn) => return Err(self.signed_out(account).await),
                Err(error) => return Err(format!("{}: {error}", calendar.name).into()),
            };
            let rows: Vec<_> = events
                .into_iter()
                .map(|event| (event.url.to_string(), event.etag, event.ics))
                .collect();
            self.store
                .replace_resources(id, &rows, calendar.ctag.as_deref(), window)
                .map_err(|e| e.to_string())?;
        }
        Ok(self.store.retain_calendars(&account.key, &keep).map_err(|e| e.to_string())?)
    }

    /// A Google account's calendars, from the Calendar API with the account's access token.
    async fn google_calendars(&self, account: &Account, client: &Client) -> Result<Vec<RemoteCalendar>, Failure> {
        let tokens = client.google_tokens().ok_or_else(|| Failure::Message("not a Google client".to_owned()))?;
        let token = match tokens.access().await {
            Ok(token) => token,
            Err(google::Error::SignInNeeded) => return Err(self.signed_out(account).await),
            Err(error) => return Err(error.to_string().into()),
        };
        let http = caldav::http_client()?;
        match google::calendars(&http, &token).await {
            Ok(calendars) => Ok(calendars),
            Err(google::Error::SignInNeeded) => Err(self.signed_out(account).await),
            Err(error) => Err(error.to_string().into()),
        }
    }

    /// Google no longer takes the account's token (revoked, or expired): forget it, so the
    /// window offers to sign in again.
    async fn signed_out(&self, account: &Account) -> Failure {
        self.clients.lock().await.remove(&account.key);
        if let Err(error) = accounts::forget_google_token(&account.key).await {
            tracing::warn!("{error}");
        }
        Failure::SignIn(google::Error::SignInNeeded.to_string())
    }

    /// Signs a Google account in through the browser; `open` opens the sign-in page. The
    /// refresh token goes into the keyring under Signet's name.
    pub async fn google_sign_in(&self, key: &str, open: impl FnOnce(String)) -> Result<(), String> {
        let account = self
            .accounts()
            .into_iter()
            .find(|account| account.key == key)
            .ok_or_else(|| "the account is no longer in Sealwax".to_owned())?;
        let config = self.config.google.clone().ok_or_else(|| google::Error::NotConfigured.to_string())?;
        let grant = google::sign_in(&config, &account.address, open).await.map_err(|e| e.to_string())?;
        accounts::store_google_token(&account.key, &grant.refresh)
            .await
            .map_err(|error| error.to_string())?;
        self.clients.lock().await.remove(&account.key);
        Ok(())
    }

    fn account_for(&self, calendar: &Calendar) -> Result<Account, String> {
        self.accounts()
            .into_iter()
            .find(|account| account.key == calendar.account)
            .ok_or_else(|| "this calendar's account is no longer in Sealwax".to_owned())
    }

    fn calendar(&self, id: i64) -> Result<Calendar, String> {
        self.store
            .calendar(id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "the calendar is gone".to_owned())
    }

    fn resource(&self, id: i64) -> Result<StoredResource, String> {
        self.store
            .resource(id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "the event is gone".to_owned())
    }

    /// Adds a new event (one calendar object) to a calendar.
    pub async fn create(&self, calendar: i64, uid: &str, ics: &str) -> Result<(), String> {
        let calendar = self.calendar(calendar)?;
        if calendar.read_only {
            return Err(format!("{} is read-only", calendar.name));
        }
        if calendar.is_local() {
            let url = format!("{}/{uid}.ics", calendar.url);
            return self.store.put_resource(calendar.id, &url, None, ics).map_err(|e| e.to_string());
        }
        let account = self.account_for(&calendar)?;
        let client = self.client(&account).await?;
        let base = Url::parse(&calendar.url).map_err(|e| e.to_string())?;
        let name: String = uid.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').collect();
        let url = base.join(&format!("{name}.ics")).map_err(|e| e.to_string())?;
        let etag = client.put(&url, ics, Condition::Create).await.map_err(write_error)?;
        self.store
            .put_resource(calendar.id, url.as_str(), etag.as_deref(), ics)
            .map_err(|e| e.to_string())
    }

    /// Replaces an event's calendar object, refusing if it changed on the server meanwhile.
    pub async fn update(&self, resource: i64, ics: &str) -> Result<(), String> {
        let stored = self.resource(resource)?;
        let calendar = self.calendar(stored.calendar)?;
        if calendar.read_only {
            return Err(format!("{} is read-only", calendar.name));
        }
        if !calendar.is_local() {
            let account = self.account_for(&calendar)?;
            let client = self.client(&account).await?;
            let url = Url::parse(&stored.url).map_err(|e| e.to_string())?;
            let condition = stored.etag.as_deref().map_or(Condition::Any, Condition::Match);
            let etag = client.put(&url, ics, condition).await.map_err(write_error)?;
            return self
                .store
                .put_resource(calendar.id, &stored.url, etag.as_deref(), ics)
                .map_err(|e| e.to_string());
        }
        self.store
            .put_resource(calendar.id, &stored.url, None, ics)
            .map_err(|e| e.to_string())
    }

    /// Moves an event to another calendar: written there first, then removed here.
    pub async fn move_to(&self, resource: i64, calendar: i64, uid: &str, ics: &str) -> Result<(), String> {
        self.create(calendar, uid, ics).await?;
        self.delete(resource).await
    }

    pub async fn delete(&self, resource: i64) -> Result<(), String> {
        let stored = self.resource(resource)?;
        let calendar = self.calendar(stored.calendar)?;
        if calendar.read_only {
            return Err(format!("{} is read-only", calendar.name));
        }
        if !calendar.is_local() {
            let account = self.account_for(&calendar)?;
            let client = self.client(&account).await?;
            let url = Url::parse(&stored.url).map_err(|e| e.to_string())?;
            client.delete(&url, stored.etag.as_deref()).await.map_err(write_error)?;
        }
        self.store.delete_resource(resource).map_err(|e| e.to_string())
    }
}

fn unauthorized() -> String {
    "the server didn't accept the password Sealwax has for this account (some providers need \
     an app password for calendars too)"
        .to_owned()
}

fn write_error(error: caldav::Error) -> String {
    match error {
        caldav::Error::Network(_) => format!("{error}; the change wasn't saved"),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Draft, new_calendar};

    fn draft() -> Draft {
        let start = chrono::NaiveDate::from_ymd_opt(2026, 10, 15).unwrap().and_hms_opt(9, 0, 0).unwrap();
        Draft {
            summary: "Dentist".into(),
            location: String::new(),
            description: String::new(),
            attendees: Vec::new(),
            conference: String::new(),
            organizer: None,
            all_day: false,
            start,
            end: start + Duration::hours(1),
        }
    }

    #[tokio::test]
    async fn the_local_calendar_needs_no_server() {
        let service = Service::with(Store::in_memory().unwrap(), Config::default());
        let local = service.store.calendars().unwrap()[0].clone();
        let ics = new_calendar("u1", &draft(), service.local).to_ics();
        service.create(local.id, "u1", &ics).await.unwrap();
        let stored = service.store.resources().unwrap();
        assert_eq!(stored.len(), 1);
        service.update(stored[0].id, &ics.replace("Dentist", "Doctor")).await.unwrap();
        assert!(service.store.resources().unwrap()[0].ics.contains("Doctor"));
        service.delete(stored[0].id).await.unwrap();
        assert!(service.store.resources().unwrap().is_empty());
        assert!(service.covers(Utc::now(), Utc::now()), "no server calendars to cover");
    }
}
