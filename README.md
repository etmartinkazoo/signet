# Signet

A calendar for [Omarchy](https://omarchy.org), the companion to the [Sealwax](https://github.com/PixDevsApps/sealwax) mail client: native GTK 4 and libadwaita, written in Rust, following your live Omarchy theme, and using the accounts you already set up in Sealwax.

A signet is the ring you press into sealing wax.

## What it does

- **Week view:** seven days of hours, all-day and multi-day events in a strip above them, overlapping events side by side, and a line at the current time. A month calendar in the sidebar marks the days that have events. Click a day there to go to its week.
- **Add:** double-click an empty slot, drag down over the hours, double-click the all-day strip, or press **N**.
- **Change:** click an event to edit its title, place, times, all-day status, calendar and notes.
- **Invitees:** add people by email address. Each invitee shows their answer (going, maybe, not going, hasn't answered), and you're added as organizer. Invitations are sent by your calendar server. iCloud, Fastmail, Google and Nextcloud send them. A server without scheduling (or the local calendar) only records who's invited. On an invitation from someone else, the invitee list is the organizer's to change.
- **Video calls:** a link field with a **Join** button. Signet writes the standard `CONFERENCE` property, plus `URL` for Apple Calendar. It reads Google Meet's own property, and Zoom, Meet, Teams, Jitsi, Webex and Whereby links found in an event's location or notes.
- **Remove:** **Delete Event** in the editor.
- **Repeating events** are expanded from their rules, exceptions and moved occurrences. When you change or delete one, Signet asks whether you mean that event alone or the whole series.
- **Offline copy:** events are kept on disk, so the window opens at once and is filled in from the server in the background. It refreshes every 5 minutes, and when you press **R**.
- **Safe writes:** every change goes to the server with the event's ETag, so Signet never overwrites an edit made elsewhere in the meantime. Properties Signet doesn't show, such as attendees, alarms and server extensions, are kept as they were.
- **On This Computer:** a local calendar for events you don't want on any server.

## Accounts shared with Sealwax

Signet reads Sealwax's `~/.config/sealwax/accounts.toml` and never writes it. It takes each account's password from the keyring item Sealwax already keeps (`application=io.github.pixdevsapps.Sealwax`, `account=<key>`, `kind=password`). An account you added in Sealwax shows its calendars here with nothing more to set up, and it keeps Sealwax's label and colour. Remove an account in Sealwax, and Signet forgets its calendars too.

Calendars are read over **CalDAV**. Signet finds the server from your address:

| Provider | Server |
| --- | --- |
| iCloud | `caldav.icloud.com` (the app password Sealwax uses works) |
| Fastmail | `caldav.fastmail.com` |
| Yahoo, AOL, mailbox.org, Posteo, GMX, web.de, Zoho, Yandex | their CalDAV servers |
| Your own domain (Nextcloud, Radicale, Baïkal, Stalwart, …) | `https://<domain>/.well-known/caldav` (RFC 6764) |

**Gmail accounts** (gmail.com, or a Google Workspace domain on Gmail's servers) get their calendars from Google Calendar through **Sign in with Google**. Google's calendar server refuses passwords, app passwords included. The app password Sealwax uses works for mail only. Set up an OAuth client once, as below, and then click **Sign In** in Signet's banner.

Not yet supported: **Microsoft accounts** (`auth = "microsoft"`). These need Microsoft Graph and calendar consent, which Sealwax's mail-only sign-in doesn't grant. Signet says so in a banner. To hide those accounts, set `enabled = false` for them (see below).

### Setting up Sign in with Google

Google requires each app to have its own OAuth client. Making one is free and takes about 10 minutes:

1. Open [Google Cloud Console](https://console.cloud.google.com/) and create a project, for example "Signet".
2. In **APIs & Services → Library**, enable **Google Calendar API** and **CalDAV API**.
3. In **Google Auth Platform**, click **Get started**. Give the app a name (Signet) and your address, and choose **External** as the audience.
4. In **Audience**, click **Publish app**. In *Testing*, Google ends sign-ins after 7 days. Published but unverified is fine for your own use.
5. In **Clients**, click **Create client**. Choose **Desktop app**, then copy the client ID and the client secret. Google shows the secret only once.
6. Add the client to `~/.config/signet/config.toml`:

   ```toml
   [google]
   client_id = "1234…apps.googleusercontent.com"
   client_secret = "GOCSPX-…"
   ```

7. Start Signet and click **Sign In** in the banner. Your browser opens Google's page. Because the client is yours and unverified, Google warns "Google hasn't verified this app": choose **Advanced → Go to Signet**. Then allow calendar access.

Signet asks only for calendar access, plus your address, which it checks against the Sealwax account. The refresh token Google returns is kept in the keyring under Signet's own name (`application=io.brem.Signet`, `kind=google-refresh-token`), never in Sealwax's items. Google says a desktop client's secret can't be kept secret in an installed app, so it lives in `config.toml`. To sign out, revoke Signet's access at [myaccount.google.com/permissions](https://myaccount.google.com/permissions). Signet then offers to sign in again.

## Settings

`~/.config/signet/config.toml` is optional:

```toml
week_start = "monday"     # or "sunday"
day_starts = 7            # the hour the week opens scrolled to

# Sign in with Google (see above):
[google]
client_id = "…apps.googleusercontent.com"
client_secret = "GOCSPX-…"

# Per Sealwax account, by its key in accounts.toml:
[account.work]
caldav = "https://dav.example.com/remote.php/dav"   # when discovery can't find the server
username = "ada"                                    # when the calendar login differs from the mail login
enabled = false                                     # leave this account's calendars out
```

A mistake in the file is shown when Signet starts, and the defaults are used.

## Keyboard

| Key | Action |
| --- | --- |
| **←** / **→** (or **H**/**L**, **K**/**J**, Page Up/Down) | Previous / next week |
| **T** or Home | Today |
| **N**, Ctrl+N | New event |
| **R**, F5, Ctrl+R | Refresh |
| Enter in the title | Save |
| Escape | Close the editor |
| Ctrl+W, Ctrl+Q | Close |

## Build and install

Needs Rust 1.92+, GTK 4.18+, libadwaita 1.9+, SQLite and a Secret Service such as gnome-keyring. All of these are on Omarchy already, except Rust.

```sh
cargo build --release
install -Dm755 target/release/signet ~/.local/bin/signet
install -Dm644 packaging/io.brem.Signet.desktop ~/.local/share/applications/io.brem.Signet.desktop
```

Debug builds use the app-id `io.brem.Signet.Devel`, so they run beside an installed build. Signet's own files are in `~/.local/share/signet/signet.db` (the offline copy, safe to delete) and `~/.config/signet/config.toml`. Set `SIGNET_LOG=debug` to see what it does. Logs never include passwords or event contents.

## Layout and tests

- `core/`: no GTK. `ics` (an iCalendar parser and writer that round-trips), `event` (times in every iCalendar form, recurrence, and edits to a series or one occurrence), `caldav`, `accounts` (Sealwax's file and keyring), `store` (SQLite) and `service` (refreshing and writing).
- `app/`: the window, the week view (drawn with cairo), the editor, and the Omarchy theme.

`cargo test --workspace` runs the unit tests. Two more checks need a server:

```sh
# The CalDAV client against a real server (writes one event to the first calendar it finds, then removes it):
SIGNET_TEST_ROOTS=ca.pem cargo run -p signet-core --example caldav_live -- https://localhost:5232/ user password

# The real window, driven by script: add, change one occurrence, delete, navigate; PNGs and report.txt into DIR.
# Point XDG_CONFIG_HOME at a test setup first: it writes to the calendars it finds.
GDK_BACKEND=broadway SIGNET_SELFTEST=DIR cargo run
```

`SIGNET_TEST_ROOTS` names a PEM file trusted in place of the system's certificates, for a local test server only.

License: MIT.
