//! Sign in with Google, for Google Calendar: Google's CalDAV refuses passwords, app passwords
//! included, and takes only OAuth 2.0. The authorization-code flow with PKCE (S256) in the
//! system browser and a loopback redirect, as Google's guide for desktop apps sets out. The
//! refresh token goes into the keyring under Signet's own name, never Sealwax's; access tokens
//! live in memory. The calendars are listed through the Calendar API, whose list has their
//! colours and access, and read and written over CalDAV like any other account's.
//!
//! Nothing here logs a token, a code or a verifier.

use crate::caldav::{self, RemoteCalendar};
use serde::Deserialize;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use url::Url;
use zeroize::Zeroizing;

const AUTHORIZE: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN: &str = "https://oauth2.googleapis.com/token";
const CALENDAR_LIST: &str = "https://www.googleapis.com/calendar/v3/users/me/calendarList?minAccessRole=reader&maxResults=250";
/// Each calendar's CalDAV collection is `<root><calendar id>/events/`.
const CALDAV_ROOT: &str = "https://apidata.googleusercontent.com/caldav/v2/";
/// Calendar access, and the signed-in address so Signet can check it's the right account.
const SCOPES: &str = "https://www.googleapis.com/auth/calendar openid email";
/// How long the browser has to come back.
const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(300);
/// An access token is renewed this long before Google says it ends.
const MARGIN: Duration = Duration::from_secs(60);

/// The OAuth client from Signet's `config.toml`: a "Desktop app" client from Google Cloud
/// Console. Google gives desktop clients a secret, but says it can't be kept secret in an
/// installed app and doesn't treat it as one.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ClientConfig {
    pub client_id: String,
    pub client_secret: String,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Google sign-in isn't set up: add [google] client_id and client_secret to ~/.config/signet/config.toml (see the README)")]
    NotConfigured,
    #[error("sign in with Google to see this account's calendars")]
    SignInNeeded,
    #[error("you signed in to Google as {signed_in}, not {expected}; sign in again with {expected}")]
    WrongAccount { signed_in: String, expected: String },
    #[error("Google sign-in was cancelled or refused ({0})")]
    Refused(String),
    #[error("Google sign-in timed out")]
    Timeout,
    #[error("Google: {0}")]
    Other(String),
}

/// What a finished sign-in gives.
pub struct Grant {
    pub refresh: Zeroizing<String>,
    pub email: Option<String>,
}

#[derive(Deserialize)]
struct TokenAnswer {
    access_token: String,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
}

#[derive(Deserialize)]
struct Refusal {
    error: String,
}

fn random_text() -> String {
    // Two v4 UUIDs: 244 random bits, in characters PKCE allows.
    format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple())
}

fn challenge(verifier: &str) -> String {
    use base64::Engine;
    use sha2::Digest;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(verifier.as_bytes()))
}

/// The `email` claim of an ID token that came straight from Google's token endpoint over TLS.
fn email_in(id_token: &str) -> Option<String> {
    use base64::Engine;
    let payload = id_token.split('.').nth(1)?;
    let json = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .ok()?;
    #[derive(Deserialize)]
    struct Claims {
        email: Option<String>,
    }
    serde_json::from_slice::<Claims>(&json).ok()?.email
}

async fn token_request(http: &reqwest::Client, form: &[(&str, &str)]) -> Result<TokenAnswer, Error> {
    let body = Zeroizing::new(url::form_urlencoded::Serializer::new(String::new()).extend_pairs(form).finish());
    let response = http
        .post(TOKEN)
        .header(reqwest::header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(body.as_bytes().to_vec())
        .send()
        .await
        .map_err(|error| Error::Other(caldav::describe(&error)))?;
    let status = response.status();
    let bytes = Zeroizing::new(response.bytes().await.map_err(|e| Error::Other(e.to_string()))?.to_vec());
    if !status.is_success() {
        let refusal = serde_json::from_slice::<Refusal>(&bytes).map(|r| r.error).unwrap_or_else(|_| status.to_string());
        // A refresh token that was revoked, or that expired with a client in Testing mode.
        return Err(if refusal == "invalid_grant" { Error::SignInNeeded } else { Error::Refused(refusal) });
    }
    serde_json::from_slice(&bytes).map_err(|_| Error::Other("an answer that couldn't be read".to_owned()))
}

/// Signs in through the browser: `open` is given the URL to open. `login_hint` preselects the
/// account; the address signed in with must match it.
pub async fn sign_in(client: &ClientConfig, login_hint: &str, open: impl FnOnce(String)) -> Result<Grant, Error> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .map_err(|e| Error::Other(e.to_string()))?;
    let port = listener.local_addr().map_err(|e| Error::Other(e.to_string()))?.port();
    let redirect = format!("http://127.0.0.1:{port}");
    let verifier = Zeroizing::new(random_text());
    let state = random_text();
    let mut url = Url::parse(AUTHORIZE).expect("a valid URL");
    url.query_pairs_mut()
        .append_pair("client_id", &client.client_id)
        .append_pair("redirect_uri", &redirect)
        .append_pair("response_type", "code")
        .append_pair("scope", SCOPES)
        .append_pair("code_challenge", &challenge(&verifier))
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &state)
        .append_pair("access_type", "offline")
        .append_pair("prompt", "consent")
        .append_pair("login_hint", login_hint);
    open(url.to_string());

    let code = tokio::time::timeout(SIGN_IN_TIMEOUT, wait_for_code(&listener, &state))
        .await
        .map_err(|_| Error::Timeout)??;
    let http = caldav::http_client().map_err(Error::Other)?;
    let answer = token_request(
        &http,
        &[
            ("client_id", &client.client_id),
            ("client_secret", &client.client_secret),
            ("code", &code),
            ("code_verifier", &verifier),
            ("redirect_uri", &redirect),
            ("grant_type", "authorization_code"),
        ],
    )
    .await?;
    let email = answer.id_token.as_deref().and_then(email_in);
    if let Some(email) = &email
        && !email.eq_ignore_ascii_case(login_hint)
    {
        return Err(Error::WrongAccount { signed_in: email.clone(), expected: login_hint.to_owned() });
    }
    let refresh = answer
        .refresh_token
        .ok_or_else(|| Error::Other("Google sent no refresh token".to_owned()))?;
    Ok(Grant { refresh: Zeroizing::new(refresh), email })
}

/// Answers the browser's requests until one carries the code (or a refusal) for `state`.
async fn wait_for_code(listener: &tokio::net::TcpListener, state: &str) -> Result<Zeroizing<String>, Error> {
    loop {
        let (mut stream, _) = listener.accept().await.map_err(|e| Error::Other(e.to_string()))?;
        let mut request = Vec::new();
        let mut buffer = [0u8; 2048];
        while !request.windows(4).any(|w| w == b"\r\n\r\n") && request.len() < 16 * 1024 {
            match stream.read(&mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(read) => request.extend_from_slice(&buffer[..read]),
            }
        }
        let text = String::from_utf8_lossy(&request);
        let path = text.lines().next().and_then(|line| line.split(' ').nth(1)).unwrap_or("/");
        let query = Url::parse(&format!("http://localhost{path}")).ok();
        let pairs: Vec<(String, String)> = query
            .map(|url| url.query_pairs().map(|(k, v)| (k.into_owned(), v.into_owned())).collect())
            .unwrap_or_default();
        let get = |name: &str| pairs.iter().find(|(key, _)| key == name).map(|(_, value)| value.clone());
        let ours = get("state").as_deref() == Some(state);
        let (status, message, result) = match (ours, get("code"), get("error")) {
            (true, Some(code), _) => ("200 OK", "Signed in. You can close this tab and go back to Signet.", Some(Ok(Zeroizing::new(code)))),
            (true, None, Some(error)) => ("200 OK", "Sign-in was cancelled. You can close this tab.", Some(Err(Error::Refused(error)))),
            _ => ("404 Not Found", "Not found.", None),
        };
        let body = format!(
            "<!doctype html><meta charset=utf-8><title>Signet</title>\
             <body style=\"font:16px sans-serif;margin:3em\"><p>{message}</p>"
        );
        let reply = format!(
            "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = stream.write_all(reply.as_bytes()).await;
        let _ = stream.shutdown().await;
        if let Some(result) = result {
            return result;
        }
    }
}

/// An account's tokens: the refresh token from the keyring, and the access token it last gave.
pub struct Tokens {
    client: ClientConfig,
    /// The Sealwax account key the refresh token is kept under.
    account: String,
    http: reqwest::Client,
    refresh: Mutex<Zeroizing<String>>,
    access: tokio::sync::Mutex<Option<(Zeroizing<String>, Instant)>>,
}

impl Tokens {
    pub fn new(client: ClientConfig, account: &str, refresh: Zeroizing<String>) -> Result<Self, Error> {
        Ok(Self {
            client,
            account: account.to_owned(),
            http: caldav::http_client().map_err(Error::Other)?,
            refresh: Mutex::new(refresh),
            access: tokio::sync::Mutex::new(None),
        })
    }

    /// A current access token, renewed when it is about to end. A new refresh token, which
    /// Google sends now and then, replaces the one in the keyring.
    pub async fn access(&self) -> Result<Zeroizing<String>, Error> {
        let mut access = self.access.lock().await;
        if let Some((token, until)) = access.as_ref()
            && Instant::now() + MARGIN < *until
        {
            return Ok(token.clone());
        }
        let refresh = self.refresh.lock().unwrap_or_else(|p| p.into_inner()).clone();
        let answer = token_request(
            &self.http,
            &[
                ("client_id", &self.client.client_id),
                ("client_secret", &self.client.client_secret),
                ("refresh_token", &refresh),
                ("grant_type", "refresh_token"),
            ],
        )
        .await?;
        let token = Zeroizing::new(answer.access_token);
        let lifetime = Duration::from_secs(answer.expires_in.unwrap_or(600));
        *access = Some((token.clone(), Instant::now() + lifetime));
        if let Some(new) = answer.refresh_token.map(Zeroizing::new) {
            if let Err(error) = crate::accounts::store_google_token(&self.account, &new).await {
                tracing::warn!("couldn't keep Google's new refresh token: {error}");
            }
            *self.refresh.lock().unwrap_or_else(|p| p.into_inner()) = new;
        }
        Ok(token)
    }
}

#[derive(Deserialize)]
struct CalendarList {
    #[serde(default)]
    items: Vec<ListedCalendar>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListedCalendar {
    id: String,
    #[serde(default)]
    summary: String,
    #[serde(default)]
    summary_override: Option<String>,
    #[serde(default)]
    background_color: Option<String>,
    #[serde(default)]
    access_role: String,
    #[serde(default)]
    deleted: bool,
}

/// The account's calendars, as Google Calendar's own list has them.
pub async fn calendars(http: &reqwest::Client, access: &str) -> Result<Vec<RemoteCalendar>, Error> {
    let response = http
        .get(CALENDAR_LIST)
        .bearer_auth(access)
        .send()
        .await
        .map_err(|error| Error::Other(caldav::describe(&error)))?;
    match response.status() {
        status if status.is_success() => {}
        reqwest::StatusCode::UNAUTHORIZED => return Err(Error::SignInNeeded),
        reqwest::StatusCode::FORBIDDEN => {
            return Err(Error::Other(
                "the Calendar API refused access; enable \"Google Calendar API\" and \"CalDAV API\" for your client in Google Cloud Console".to_owned(),
            ));
        }
        status => return Err(Error::Other(format!("the calendar list answered {status}"))),
    }
    let bytes = response.bytes().await.map_err(|e| Error::Other(e.to_string()))?;
    let list: CalendarList = serde_json::from_slice(&bytes).map_err(|e| Error::Other(e.to_string()))?;
    Ok(parse_list(list))
}

fn parse_list(list: CalendarList) -> Vec<RemoteCalendar> {
    list.items
        .into_iter()
        .filter(|calendar| !calendar.deleted)
        .filter_map(|calendar| {
            let id: String = url::form_urlencoded::byte_serialize(calendar.id.as_bytes()).collect();
            let url = Url::parse(&format!("{CALDAV_ROOT}{id}/events/")).ok()?;
            Some(RemoteCalendar {
                url,
                name: calendar.summary_override.unwrap_or(calendar.summary),
                color: calendar.background_color.map(|c| c.to_ascii_lowercase()),
                // Google's CalDAV has no tag that changes with the calendar: read it each time.
                ctag: None,
                read_only: !matches!(calendar.access_role.as_str(), "owner" | "writer"),
            })
        })
        .collect()
}

/// Whether an account's mail is Gmail (gmail.com, or a Google Workspace domain on Gmail's
/// servers), and so its calendar is Google Calendar.
pub fn is_google(address: &str, mail_host: Option<&str>) -> bool {
    let domain = address.rsplit_once('@').map(|(_, d)| d.to_ascii_lowercase()).unwrap_or_default();
    matches!(domain.as_str(), "gmail.com" | "googlemail.com")
        || mail_host.is_some_and(|host| host.eq_ignore_ascii_case("imap.gmail.com") || host.eq_ignore_ascii_case("pop.gmail.com"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_is_rfc7636s_example() {
        assert_eq!(
            challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        assert_eq!(random_text().len(), 64);
    }

    #[test]
    fn reads_the_signed_in_address() {
        use base64::Engine;
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"email":"ada@gmail.com","sub":"1"}"#);
        assert_eq!(email_in(&format!("h.{payload}.s")).as_deref(), Some("ada@gmail.com"));
        assert_eq!(email_in("garbage"), None);
    }

    #[test]
    fn lists_calendars_as_caldav_collections() {
        let list: CalendarList = serde_json::from_str(
            r##"{"items":[
              {"id":"ada@gmail.com","summary":"ada@gmail.com","summaryOverride":"Personal","backgroundColor":"#9FC6E7","accessRole":"owner"},
              {"id":"en.usa#holiday@group.v.calendar.google.com","summary":"Holidays","accessRole":"reader"},
              {"id":"x@group.calendar.google.com","summary":"Old","accessRole":"owner","deleted":true}]}"##,
        )
        .unwrap();
        let calendars = parse_list(list);
        assert_eq!(calendars.len(), 2);
        assert_eq!(calendars[0].url.as_str(), "https://apidata.googleusercontent.com/caldav/v2/ada%40gmail.com/events/");
        assert_eq!(calendars[0].name, "Personal");
        assert_eq!(calendars[0].color.as_deref(), Some("#9fc6e7"));
        assert!(!calendars[0].read_only);
        assert!(calendars[1].url.as_str().contains("en.usa%23holiday%40group"));
        assert!(calendars[1].read_only);
    }

    #[test]
    fn knows_gmail_accounts() {
        assert!(is_google("ada@gmail.com", None));
        assert!(is_google("ada@company.com", Some("imap.gmail.com")));
        assert!(!is_google("ada@fastmail.com", Some("imap.fastmail.com")));
    }

    #[tokio::test]
    async fn the_loopback_answers_only_its_own_state() {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let browser = tokio::spawn(async move {
            for path in ["/favicon.ico", "/?state=other&code=bad", "/?state=s1&code=good"] {
                let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
                stream.write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes()).await.unwrap();
                let mut answer = String::new();
                stream.read_to_string(&mut answer).await.unwrap();
            }
        });
        let code = wait_for_code(&listener, "s1").await.unwrap();
        assert_eq!(code.as_str(), "good");
        browser.await.unwrap();
    }
}
