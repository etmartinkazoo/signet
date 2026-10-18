//! The offline copy, in `$XDG_DATA_HOME/signet/signet.db`: each account's calendar home, its
//! calendars, and the iCalendar text of their events as the server last sent it. Signet shows
//! this copy at once and refreshes it in the background. The local calendar lives only here.

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

/// The account key of calendars kept only on this computer.
pub const LOCAL_ACCOUNT: &str = "local";
const LOCAL_URL: &str = "local:personal";

const SCHEMA: &str = "
PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;
CREATE TABLE IF NOT EXISTS account (
    key TEXT PRIMARY KEY,
    home TEXT
);
CREATE TABLE IF NOT EXISTS calendar (
    id INTEGER PRIMARY KEY,
    account TEXT NOT NULL,
    url TEXT NOT NULL,
    name TEXT NOT NULL,
    color TEXT,
    ctag TEXT,
    read_only INTEGER NOT NULL DEFAULT 0,
    visible INTEGER NOT NULL DEFAULT 1,
    synced_from TEXT,
    synced_to TEXT,
    UNIQUE (account, url)
);
CREATE TABLE IF NOT EXISTS resource (
    id INTEGER PRIMARY KEY,
    calendar INTEGER NOT NULL REFERENCES calendar (id) ON DELETE CASCADE,
    url TEXT NOT NULL,
    etag TEXT,
    ics TEXT NOT NULL,
    UNIQUE (calendar, url)
);
";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Calendar {
    pub id: i64,
    pub account: String,
    pub url: String,
    pub name: String,
    pub color: Option<String>,
    pub ctag: Option<String>,
    pub read_only: bool,
    pub visible: bool,
    /// The range of days the stored events cover.
    pub synced: Option<(DateTime<Utc>, DateTime<Utc>)>,
}

impl Calendar {
    pub fn is_local(&self) -> bool {
        self.account == LOCAL_ACCOUNT
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredResource {
    pub id: i64,
    pub calendar: i64,
    pub url: String,
    pub etag: Option<String>,
    pub ics: String,
}

#[derive(Debug, thiserror::Error)]
#[error("the event store: {0}")]
pub struct StoreError(#[from] rusqlite::Error);

pub struct Store {
    connection: Mutex<Connection>,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        Self::with(Connection::open(path)?)
    }

    pub fn in_memory() -> Result<Self, StoreError> {
        Self::with(Connection::open_in_memory()?)
    }

    fn with(connection: Connection) -> Result<Self, StoreError> {
        connection.execute_batch(SCHEMA)?;
        connection.execute(
            "INSERT OR IGNORE INTO calendar (account, url, name, color) VALUES (?1, ?2, 'Personal', NULL)",
            params![LOCAL_ACCOUNT, LOCAL_URL],
        )?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    fn db(&self) -> MutexGuard<'_, Connection> {
        self.connection.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn calendars(&self) -> Result<Vec<Calendar>, StoreError> {
        let db = self.db();
        let mut statement = db.prepare(
            "SELECT id, account, url, name, color, ctag, read_only, visible, synced_from, synced_to
             FROM calendar ORDER BY account = 'local' DESC, account, name COLLATE NOCASE",
        )?;
        let rows = statement.query_map([], |row| {
            let from: Option<String> = row.get(8)?;
            let to: Option<String> = row.get(9)?;
            Ok(Calendar {
                id: row.get(0)?,
                account: row.get(1)?,
                url: row.get(2)?,
                name: row.get(3)?,
                color: row.get(4)?,
                ctag: row.get(5)?,
                read_only: row.get(6)?,
                visible: row.get(7)?,
                synced: from.zip(to).and_then(|(from, to)| {
                    Some((from.parse().ok()?, to.parse().ok()?))
                }),
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn calendar(&self, id: i64) -> Result<Option<Calendar>, StoreError> {
        Ok(self.calendars()?.into_iter().find(|calendar| calendar.id == id))
    }

    pub fn set_visible(&self, id: i64, visible: bool) -> Result<(), StoreError> {
        self.db()
            .execute("UPDATE calendar SET visible = ?2 WHERE id = ?1", params![id, visible])?;
        Ok(())
    }

    pub fn home(&self, account: &str) -> Result<Option<String>, StoreError> {
        Ok(self
            .db()
            .query_row("SELECT home FROM account WHERE key = ?1", [account], |row| row.get(0))
            .optional()?
            .flatten())
    }

    pub fn set_home(&self, account: &str, home: Option<&str>) -> Result<(), StoreError> {
        self.db().execute(
            "INSERT INTO account (key, home) VALUES (?1, ?2)
             ON CONFLICT (key) DO UPDATE SET home = excluded.home",
            params![account, home],
        )?;
        Ok(())
    }

    /// Adds a server's calendar or refreshes its name, colour and access; keeps whether it's shown.
    pub fn upsert_calendar(
        &self,
        account: &str,
        url: &str,
        name: &str,
        color: Option<&str>,
        read_only: bool,
    ) -> Result<i64, StoreError> {
        let db = self.db();
        db.execute(
            "INSERT INTO calendar (account, url, name, color, read_only) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (account, url) DO UPDATE SET
                name = excluded.name, color = excluded.color, read_only = excluded.read_only",
            params![account, url, name, color, read_only],
        )?;
        Ok(db.query_row(
            "SELECT id FROM calendar WHERE account = ?1 AND url = ?2",
            params![account, url],
            |row| row.get(0),
        )?)
    }

    /// Forgets an account's calendars that are no longer on the server.
    pub fn retain_calendars(&self, account: &str, keep: &[i64]) -> Result<(), StoreError> {
        let db = self.db();
        let mut statement = db.prepare("SELECT id FROM calendar WHERE account = ?1")?;
        let ids: Vec<i64> = statement
            .query_map([account], |row| row.get(0))?
            .collect::<Result<_, _>>()?;
        for id in ids.into_iter().filter(|id| !keep.contains(id)) {
            db.execute("DELETE FROM calendar WHERE id = ?1", [id])?;
        }
        Ok(())
    }

    /// Forgets the calendars of accounts that are gone from Sealwax.
    pub fn retain_accounts(&self, keep: &[String]) -> Result<(), StoreError> {
        let db = self.db();
        let mut statement = db.prepare("SELECT DISTINCT account FROM calendar WHERE account != ?1")?;
        let accounts: Vec<String> = statement
            .query_map([LOCAL_ACCOUNT], |row| row.get(0))?
            .collect::<Result<_, _>>()?;
        for account in accounts.into_iter().filter(|account| !keep.contains(account)) {
            db.execute("DELETE FROM calendar WHERE account = ?1", [&account])?;
            db.execute("DELETE FROM account WHERE key = ?1", [&account])?;
        }
        Ok(())
    }

    /// Replaces a calendar's stored events with what the server sent for `from..to`.
    pub fn replace_resources(
        &self,
        calendar: i64,
        resources: &[(String, Option<String>, String)],
        ctag: Option<&str>,
        window: (DateTime<Utc>, DateTime<Utc>),
    ) -> Result<(), StoreError> {
        let mut db = self.db();
        let transaction = db.transaction()?;
        transaction.execute("DELETE FROM resource WHERE calendar = ?1", [calendar])?;
        for (url, etag, ics) in resources {
            transaction.execute(
                "INSERT OR REPLACE INTO resource (calendar, url, etag, ics) VALUES (?1, ?2, ?3, ?4)",
                params![calendar, url, etag, ics],
            )?;
        }
        transaction.execute(
            "UPDATE calendar SET ctag = ?2, synced_from = ?3, synced_to = ?4 WHERE id = ?1",
            params![calendar, ctag, window.0.to_rfc3339(), window.1.to_rfc3339()],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn put_resource(&self, calendar: i64, url: &str, etag: Option<&str>, ics: &str) -> Result<(), StoreError> {
        let db = self.db();
        db.execute(
            "INSERT INTO resource (calendar, url, etag, ics) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (calendar, url) DO UPDATE SET etag = excluded.etag, ics = excluded.ics",
            params![calendar, url, etag, ics],
        )?;
        // The calendar changed: the next refresh reads it again whatever its tag says.
        db.execute("UPDATE calendar SET ctag = NULL WHERE id = ?1 AND account != ?2", params![calendar, LOCAL_ACCOUNT])?;
        Ok(())
    }

    pub fn delete_resource(&self, id: i64) -> Result<(), StoreError> {
        let db = self.db();
        db.execute(
            "UPDATE calendar SET ctag = NULL WHERE account != ?2 AND id = (SELECT calendar FROM resource WHERE id = ?1)",
            params![id, LOCAL_ACCOUNT],
        )?;
        db.execute("DELETE FROM resource WHERE id = ?1", [id])?;
        Ok(())
    }

    pub fn resource(&self, id: i64) -> Result<Option<StoredResource>, StoreError> {
        Ok(self
            .db()
            .query_row(
                "SELECT id, calendar, url, etag, ics FROM resource WHERE id = ?1",
                [id],
                resource_row,
            )
            .optional()?)
    }

    pub fn resources(&self) -> Result<Vec<StoredResource>, StoreError> {
        let db = self.db();
        let mut statement = db.prepare("SELECT id, calendar, url, etag, ics FROM resource")?;
        let rows = statement.query_map([], resource_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }
}

fn resource_row(row: &rusqlite::Row) -> rusqlite::Result<StoredResource> {
    Ok(StoredResource {
        id: row.get(0)?,
        calendar: row.get(1)?,
        url: row.get(2)?,
        etag: row.get(3)?,
        ics: row.get(4)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_calendars_and_their_events() {
        let store = Store::in_memory().unwrap();
        let local = store.calendars().unwrap();
        assert_eq!(local.len(), 1);
        assert!(local[0].is_local());

        let work = store.upsert_calendar("work", "https://dav/w/", "Work", Some("#112233"), false).unwrap();
        store.set_visible(work, false).unwrap();
        let again = store.upsert_calendar("work", "https://dav/w/", "Work!", None, true).unwrap();
        assert_eq!(work, again);
        let stored = store.calendar(work).unwrap().unwrap();
        assert_eq!((stored.name.as_str(), stored.visible, stored.read_only), ("Work!", false, true));

        let window = (Utc::now(), Utc::now());
        store
            .replace_resources(work, &[("https://dav/w/1.ics".into(), Some("\"1\"".into()), "ICS".into())], Some("7"), window)
            .unwrap();
        assert_eq!(store.calendar(work).unwrap().unwrap().ctag.as_deref(), Some("7"));
        store.put_resource(work, "https://dav/w/2.ics", None, "ICS2").unwrap();
        assert_eq!(store.calendar(work).unwrap().unwrap().ctag, None);
        assert_eq!(store.resources().unwrap().len(), 2);

        store.retain_calendars("work", &[]).unwrap();
        assert!(store.resources().unwrap().is_empty(), "events go with their calendar");

        store.set_home("work", Some("https://dav/")).unwrap();
        assert_eq!(store.home("work").unwrap().as_deref(), Some("https://dav/"));
        store.upsert_calendar("gone", "https://x/", "X", None, false).unwrap();
        store.retain_accounts(&["work".to_owned()]).unwrap();
        assert_eq!(store.calendars().unwrap().len(), 1);
    }
}
